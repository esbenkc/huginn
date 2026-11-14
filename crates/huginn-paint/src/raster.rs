//! vello_cpu backend for [`crate::CpuRasterizer`].

use std::sync::Arc;

use huginn_layout::Rect;
use huginn_style::{BackgroundRepeat, BlendMode, BorderStyle, Color, Edges, FilterFunction};
use vello_cpu::kurbo::{self, Affine, BezPath, RoundedRect, RoundedRectRadii, Shape};
use vello_cpu::peniko::{self, Extend, Fill, ImageQuality, ImageSampler};
use vello_cpu::{Image, ImageSource, Mask, Pixmap, RenderContext, Resources};

use crate::filter;
use crate::{DisplayItem, DisplayList, MaskSource, ResolvedGradient, RgbaImage};

fn color(c: Color) -> peniko::Color {
    peniko::Color::from_rgba8(c.r, c.g, c.b, c.a)
}

fn krect(r: Rect) -> kurbo::Rect {
    kurbo::Rect::new(
        f64::from(r.x),
        f64::from(r.y),
        f64::from(r.x + r.width),
        f64::from(r.y + r.height),
    )
}

fn rounded(r: Rect, radii: [f32; 4]) -> BezPath {
    RoundedRect::from_rect(
        krect(r),
        RoundedRectRadii::new(
            f64::from(radii[0]),
            f64::from(radii[1]),
            f64::from(radii[2]),
            f64::from(radii[3]),
        ),
    )
    .to_path(0.1)
}

fn has_radius(radii: [f32; 4]) -> bool {
    radii.iter().any(|&r| r > 0.0)
}

/// Box geometry on whole device pixels, as Blink paints it
/// (`ToPixelSnappedRect`: each edge rounded half up), so 1px borders and
/// background edges stay sharp instead of smearing over two pixel rows.
/// Only under an axis-aligned scale and translation (`t`, CSS px to device
/// px); other transforms keep the exact rect.
fn snap(t: &Affine, r: Rect) -> Rect {
    let [sx, ky, kx, sy, tx, ty] = t.as_coeffs();
    if ky != 0.0 || kx != 0.0 || sx <= 0.0 || sy <= 0.0 {
        return r;
    }
    let edge = |v: f32, s: f64, o: f64| (((f64::from(v) * s + o + 0.5).floor() - o) / s) as f32;
    let (x0, x1) = (edge(r.x, sx, tx), edge(r.x + r.width, sx, tx));
    let (y0, y1) = (edge(r.y, sy, ty), edge(r.y + r.height, sy, ty));
    Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    }
}

/// What a Push item put on the render context, so Pop undoes the right thing.
enum Pushed {
    Clip,
    /// A layer (opacity, mask, blend) pushed by the item at `start`, under
    /// `transform`. It is a backdrop root for `backdrop-filter`.
    Layer {
        start: usize,
        transform: Affine,
    },
    /// The transform before the push.
    Transform(Affine),
}

fn pop(ctx: &mut RenderContext, stack: &mut Vec<Pushed>) {
    match stack.pop() {
        Some(Pushed::Clip) => ctx.pop_clip(),
        Some(Pushed::Layer { .. }) => ctx.pop_layer(),
        Some(Pushed::Transform(t)) => ctx.set_transform(t),
        None => {}
    }
}

pub(crate) fn rasterize(
    resources: &mut Resources,
    list: &DisplayList<'_>,
    region: Rect,
    scale: f32,
) -> RgbaImage {
    let width = ((region.width * scale).round() as u32).clamp(1, u32::from(u16::MAX));
    let height = ((region.height * scale).round() as u32).clamp(1, u32::from(u16::MAX));
    let (w16, h16) = (width as u16, height as u16);
    let mut ctx = RenderContext::new(w16, h16);

    // Canvas.
    ctx.set_paint(color(list.canvas));
    ctx.fill_rect(&kurbo::Rect::new(
        0.0,
        0.0,
        f64::from(width),
        f64::from(height),
    ));

    let base = Affine::scale(f64::from(scale))
        * Affine::translate((-f64::from(region.x), -f64::from(region.y)));
    ctx.set_transform(base);

    let mut r = Raster {
        resources,
        mips: Mips::default(),
        scale,
        canvas: list.canvas,
    };
    r.paint(&mut ctx, (w16, h16), &list.items, true);

    let mut pixmap = Pixmap::new(w16, h16);
    ctx.flush();
    ctx.render(&mut pixmap, r.resources);
    RgbaImage {
        width,
        height,
        // The canvas is opaque, so premultiplied and straight alpha are the
        // same bytes; skip the unpremultiply pass.
        data: pixmap.take_rgba8(peniko::ImageAlphaType::AlphaPremultiplied),
    }
}

/// State of one raster pass, shared by the page and its offscreen groups.
struct Raster<'r> {
    resources: &'r mut Resources,
    mips: Mips,
    /// Device px per CSS px.
    scale: f32,
    canvas: Color,
}

impl Raster<'_> {
    /// Paint `items` (balanced) into `ctx`, which is `size` device px.
    /// `on_canvas`: `ctx` starts with the canvas color, so it is part of
    /// any backdrop read here.
    fn paint(
        &mut self,
        ctx: &mut RenderContext,
        size: (u16, u16),
        items: &[DisplayItem<'_>],
        on_canvas: bool,
    ) {
        let first_transform = *ctx.transform();
        let scale = self.scale;
        let mut stack: Vec<Pushed> = Vec::new();
        let mut i = 0;
        while i < items.len() {
            match &items[i] {
                DisplayItem::Rect {
                    rect,
                    radii,
                    color: c,
                } => {
                    let rect = snap(ctx.transform(), *rect);
                    ctx.set_paint(color(*c));
                    if has_radius(*radii) {
                        ctx.set_fill_rule(Fill::NonZero);
                        ctx.fill_path(&rounded(rect, *radii));
                    } else {
                        ctx.fill_rect(&krect(rect));
                    }
                }
                DisplayItem::Border {
                    rect,
                    widths,
                    colors,
                    styles,
                    radii,
                } => {
                    let rect = snap(ctx.transform(), *rect);
                    draw_border(ctx, rect, *widths, *colors, *styles, *radii)
                }
                DisplayItem::Glyphs {
                    run,
                    glyphs,
                    coords,
                } => {
                    // Like Skia (Chrome) for horizontal text: the baseline sits on a
                    // whole device pixel, x keeps its subpixel position. A baseline
                    // between pixels blurs every horizontal stem.
                    let [_, ky, kx, sy, _, ty] = ctx.transform().as_coeffs();
                    let snap_y = ky == 0.0 && kx == 0.0 && sy > 0.0;
                    ctx.set_paint(color(run.color));
                    let mut builder = ctx
                        .glyph_run(self.resources, &run.font)
                        .font_size(run.font_size)
                        .normalized_coords(coords)
                        .hint(false);
                    if let Some(deg) = run.skew {
                        builder = builder
                            .glyph_transform(Affine::skew(f64::from(deg).to_radians().tan(), 0.0));
                    }
                    if run.embolden {
                        let a = f64::from(fake_bold_outset(run.font_size));
                        builder = builder
                            .font_embolden(glifo::FontEmbolden::new(kurbo::Diagonal2::new(a, a)));
                    }
                    // Glyph rendering errors (e.g. unsupported bitmap formats) skip the run.
                    let _ = builder.fill_glyphs(glyphs.iter().map(|g| vello_cpu::Glyph {
                        id: g.id,
                        x: g.x,
                        y: if snap_y {
                            (((f64::from(g.y) * sy + ty + 0.5).floor() - ty) / sy) as f32
                        } else {
                            g.y
                        },
                    }));
                }
                DisplayItem::Image { rect, image } => {
                    let rect = &snap(ctx.transform(), *rect);
                    if rect.width > 0.0 && rect.height > 0.0 {
                        let (pixmap, quality) =
                            self.mips
                                .get(image.pixmap(), rect.width * scale, rect.height * scale);
                        let sx = f64::from(rect.width) / f64::from(pixmap.width());
                        let sy = f64::from(rect.height) / f64::from(pixmap.height());
                        let mut paint = image_paint(&pixmap, Extend::Pad);
                        if let peniko::Brush::Image(img) = &mut paint {
                            img.sampler.quality = quality;
                        }
                        ctx.set_paint(paint);
                        ctx.set_paint_transform(
                            Affine::translate((f64::from(rect.x), f64::from(rect.y)))
                                * Affine::scale_non_uniform(sx, sy),
                        );
                        ctx.fill_rect(&krect(*rect));
                        ctx.reset_paint_transform();
                    }
                }
                DisplayItem::TiledImage {
                    clip,
                    origin,
                    image,
                    repeat,
                } => self.tiled_image(ctx, *clip, *origin, image, *repeat),
                DisplayItem::BoxShadow {
                    rect,
                    radii,
                    offset,
                    blur,
                    spread,
                    color: c,
                    inset,
                } => draw_box_shadow(ctx, *rect, *radii, *offset, *blur, *spread, *c, *inset),
                DisplayItem::Gradient {
                    clip,
                    gradient,
                    stops,
                    repeating,
                } => draw_gradient(ctx, *clip, gradient, stops, *repeating),
                DisplayItem::PushTransform(m) => {
                    let t = *ctx.transform();
                    ctx.set_transform(t * Affine::new(m.map(f64::from)));
                    stack.push(Pushed::Transform(t));
                }
                DisplayItem::PushClip { rect, radii } => {
                    // Non-isolated clip: no extra layer to composite.
                    if has_radius(*radii) {
                        ctx.set_fill_rule(Fill::NonZero);
                        ctx.push_clip_path(&rounded(*rect, *radii));
                    } else {
                        ctx.push_clip_rect(&krect(*rect));
                    }
                    stack.push(Pushed::Clip);
                }
                DisplayItem::PushClipPath { path, even_odd } => {
                    ctx.set_fill_rule(if *even_odd {
                        Fill::EvenOdd
                    } else {
                        Fill::NonZero
                    });
                    ctx.push_clip_path(path);
                    ctx.set_fill_rule(Fill::NonZero);
                    stack.push(Pushed::Clip);
                }
                DisplayItem::PushOpacity(o) => {
                    ctx.push_opacity_layer(*o);
                    stack.push(Pushed::Layer {
                        start: i,
                        transform: *ctx.transform(),
                    });
                }
                DisplayItem::PushBlend(mode) => {
                    ctx.push_layer(None, Some(blend_mode(*mode)), None, None, None);
                    stack.push(Pushed::Layer {
                        start: i,
                        transform: *ctx.transform(),
                    });
                }
                DisplayItem::PushMask {
                    clip,
                    tile,
                    repeat,
                    source,
                } => {
                    let t = *ctx.transform();
                    let mask = match source {
                        MaskSource::Image(image) => {
                            let (pixmap, _) = self.mips.get(
                                image.pixmap(),
                                tile.width * scale,
                                tile.height * scale,
                            );
                            build_mask(t, size, *clip, *tile, *repeat, |u, v| {
                                image_alpha(&pixmap, u, v)
                            })
                        }
                        MaskSource::Gradient {
                            gradient,
                            stops,
                            repeating,
                        } => build_mask(t, size, *clip, *tile, *repeat, |u, v| {
                            let p = (tile.x + u * tile.width, tile.y + v * tile.height);
                            gradient_alpha(gradient, stops, *repeating, p)
                        }),
                    };
                    ctx.push_layer(
                        Some(&krect(*clip).to_path(0.1)),
                        None,
                        None,
                        Some(mask),
                        None,
                    );
                    stack.push(Pushed::Layer {
                        start: i,
                        transform: t,
                    });
                }
                DisplayItem::PushFilter(filters) => {
                    let end = matching_pop_filter(items, i);
                    self.filter_group(ctx, size, &items[i + 1..end], filters);
                    i = end + 1;
                    continue;
                }
                DisplayItem::BackdropFilter {
                    rect,
                    radii,
                    filters,
                    own_layers,
                } => {
                    // The backdrop root: the innermost layer that is not
                    // the element's own, else the start of `items`.
                    let root = stack
                        .iter()
                        .rev()
                        .filter_map(|p| match p {
                            Pushed::Layer { start, transform } => Some((*start, *transform)),
                            _ => None,
                        })
                        .nth(usize::from(*own_layers));
                    let (from, t0, canvas) = match root {
                        Some((start, t)) => (start + 1, t, false),
                        None => (0, first_transform, on_canvas),
                    };
                    let backdrop = &items[from..i];
                    self.backdrop_filter(ctx, size, backdrop, t0, canvas, *rect, *radii, filters);
                }
                DisplayItem::PopClip
                | DisplayItem::PopOpacity
                | DisplayItem::PopMask
                | DisplayItem::PopBlend
                | DisplayItem::PopTransform
                | DisplayItem::PopFilter => pop(ctx, &mut stack),
            }
            i += 1;
        }
        while !stack.is_empty() {
            pop(ctx, &mut stack);
        }
    }

    fn tiled_image(
        &mut self,
        ctx: &mut RenderContext,
        clip: Rect,
        origin: Rect,
        image: &huginn_net::DecodedImage,
        repeat: BackgroundRepeat,
    ) {
        let (rx, ry) = match repeat {
            BackgroundRepeat::Repeat => (true, true),
            BackgroundRepeat::RepeatX => (true, false),
            BackgroundRepeat::RepeatY => (false, true),
            BackgroundRepeat::NoRepeat => (false, false),
        };
        let mut area = clip;
        if !rx {
            area.x = area.x.max(origin.x);
            area.width = (clip.right().min(origin.right()) - area.x).max(0.0);
        }
        if !ry {
            area.y = area.y.max(origin.y);
            area.height = (clip.bottom().min(origin.bottom()) - area.y).max(0.0);
        }
        if area.width <= 0.0 || area.height <= 0.0 {
            return;
        }
        if origin.width <= 0.0 || origin.height <= 0.0 {
            return;
        }
        // `origin` is one tile: its size scales the image (background-size).
        let (pixmap, quality) = self.mips.get(
            image.pixmap(),
            origin.width * self.scale,
            origin.height * self.scale,
        );
        let mut paint = image_paint(&pixmap, Extend::Repeat);
        if let peniko::Brush::Image(img) = &mut paint {
            img.sampler.x_extend = if rx { Extend::Repeat } else { Extend::Pad };
            img.sampler.y_extend = if ry { Extend::Repeat } else { Extend::Pad };
            img.sampler.quality = quality;
        }
        ctx.set_paint(paint);
        ctx.set_paint_transform(
            Affine::translate((f64::from(origin.x), f64::from(origin.y)))
                * Affine::scale_non_uniform(
                    f64::from(origin.width) / f64::from(pixmap.width()),
                    f64::from(origin.height) / f64::from(pixmap.height()),
                ),
        );
        ctx.fill_rect(&krect(area));
        ctx.reset_paint_transform();
    }

    /// Paint `items` into an offscreen pixmap, run `filters` over it and
    /// draw the result. The pixmap covers what the items paint, grown by
    /// the filter's reach, within reach of what `ctx` shows.
    fn filter_group(
        &mut self,
        ctx: &mut RenderContext,
        size: (u16, u16),
        items: &[DisplayItem<'_>],
        filters: &[FilterFunction],
    ) {
        let t = *ctx.transform();
        let Some(content) = content_bounds(items, t) else {
            return;
        };
        let (rx, ry) = device_reach(&t, filter::reach(filters));
        let target = device_rect(size);
        let Some(area) = pixel_area(content.inflate(rx, ry).intersect(target.inflate(rx, ry)))
        else {
            return;
        };
        let mut sub = RenderContext::new(area.2, area.3);
        sub.set_transform(Affine::translate((-f64::from(area.0), -f64::from(area.1))) * t);
        self.paint(&mut sub, (area.2, area.3), items, false);
        let mut pm = Pixmap::new(area.2, area.3);
        sub.flush();
        sub.render(&mut pm, self.resources);
        drop(sub);
        let [a, b, c, d, _, _] = t.as_coeffs();
        filter::apply(&mut pm, filters, [a, b, c, d], filter::Edge::Transparent);
        draw_device_pixmap(ctx, pm, area, target);
    }

    /// `backdrop-filter` (Filter Effects 2): paint `backdrop` (the items
    /// since the backdrop root, under `t0`) again offscreen, clipped to the
    /// element's border box, run `filters` over it with mirrored edges and
    /// draw it inside the border box (with its radii).
    #[allow(clippy::too_many_arguments)]
    fn backdrop_filter(
        &mut self,
        ctx: &mut RenderContext,
        size: (u16, u16),
        backdrop: &[DisplayItem<'_>],
        t0: Affine,
        canvas: bool,
        rect: Rect,
        radii: [f32; 4],
        filters: &[FilterFunction],
    ) {
        let t = *ctx.transform();
        let target = device_rect(size);
        let Some(area) = pixel_area(t.transform_rect_bbox(krect(rect)).intersect(target)) else {
            return;
        };
        let mut sub = RenderContext::new(area.2, area.3);
        if canvas {
            sub.set_paint(color(self.canvas));
            sub.fill_rect(&device_rect((area.2, area.3)));
        }
        sub.set_transform(Affine::translate((-f64::from(area.0), -f64::from(area.1))) * t0);
        self.paint(&mut sub, (area.2, area.3), backdrop, canvas);
        let mut pm = Pixmap::new(area.2, area.3);
        sub.flush();
        sub.render(&mut pm, self.resources);
        drop(sub);
        let [a, b, c, d, _, _] = t.as_coeffs();
        filter::apply(&mut pm, filters, [a, b, c, d], filter::Edge::Mirror);
        ctx.set_fill_rule(Fill::NonZero);
        if has_radius(radii) {
            ctx.push_clip_path(&rounded(rect, radii));
        } else {
            ctx.push_clip_path(&krect(rect).to_path(0.1));
        }
        draw_device_pixmap(ctx, pm, area, target);
        ctx.pop_clip();
    }
}

/// The index of the `PopFilter` that closes the `PushFilter` at `open`.
fn matching_pop_filter(items: &[DisplayItem<'_>], open: usize) -> usize {
    let mut depth = 0usize;
    for (i, item) in items.iter().enumerate().skip(open) {
        match item {
            DisplayItem::PushFilter(_) => depth += 1,
            DisplayItem::PopFilter => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    items.len()
}

fn device_rect((w, h): (u16, u16)) -> kurbo::Rect {
    kurbo::Rect::new(0.0, 0.0, f64::from(w), f64::from(h))
}

/// `reach` CSS px in device px along x and y under `t`.
fn device_reach(t: &Affine, reach: f32) -> (f64, f64) {
    let [a, b, c, d, _, _] = t.as_coeffs();
    let r = f64::from(reach);
    ((r * a.hypot(b)).ceil(), (r * c.hypot(d)).ceil())
}

/// Whole device pixels covering `r`: (x, y, width, height), or `None` if
/// empty. Offscreen groups are capped at 16384 px a side.
fn pixel_area(r: kurbo::Rect) -> Option<(i32, i32, u16, u16)> {
    if !(r.width() > 0.0 && r.height() > 0.0) {
        return None;
    }
    let (x0, y0) = (r.x0.floor(), r.y0.floor());
    let w = (r.x1.ceil() - x0).min(16384.0) as u16;
    let h = (r.y1.ceil() - y0).min(16384.0) as u16;
    (w > 0 && h > 0).then_some((x0 as i32, y0 as i32, w, h))
}

/// Draw `pm`, whose top-left pixel is at device `(area.0, area.1)`, into
/// `ctx` within `visible`, pixel for pixel.
fn draw_device_pixmap(
    ctx: &mut RenderContext,
    pm: Pixmap,
    area: (i32, i32, u16, u16),
    visible: kurbo::Rect,
) {
    let (x, y) = (f64::from(area.0), f64::from(area.1));
    let shown =
        kurbo::Rect::new(x, y, x + f64::from(area.2), y + f64::from(area.3)).intersect(visible);
    if !(shown.width() > 0.0 && shown.height() > 0.0) {
        return;
    }
    let saved = *ctx.transform();
    ctx.set_transform(Affine::IDENTITY);
    let mut paint = image_paint(&Arc::new(pm), Extend::Pad);
    if let peniko::Brush::Image(img) = &mut paint {
        img.sampler.quality = ImageQuality::Low;
    }
    ctx.set_paint(paint);
    ctx.set_paint_transform(Affine::translate((x, y)));
    ctx.fill_rect(&shown);
    ctx.reset_paint_transform();
    ctx.set_transform(saved);
}

/// Device-space bounds of what `items` paint, drawn under `t`, before any
/// filter of the group itself; `None` if they paint nothing.
fn content_bounds(items: &[DisplayItem<'_>], t: Affine) -> Option<kurbo::Rect> {
    enum Frame {
        Transform(Affine),
        Clip(Option<kurbo::Rect>),
        Filter,
        Layer,
    }
    /// Clips inside one nested filter group, and how far that filter grows ink.
    struct Level {
        clip: Option<kurbo::Rect>,
        grow: (f64, f64),
    }
    let mut t = t;
    let mut levels = vec![Level {
        clip: None,
        grow: (0.0, 0.0),
    }];
    let mut stack: Vec<Frame> = Vec::new();
    let mut out: Option<kurbo::Rect> = None;
    let narrow = |levels: &mut Vec<Level>, c: kurbo::Rect| {
        if let Some(l) = levels.last_mut() {
            l.clip = Some(l.clip.map_or(c, |old| old.intersect(c)));
        }
    };
    for item in items {
        let r = match item {
            DisplayItem::Rect { rect, .. }
            | DisplayItem::Border { rect, .. }
            | DisplayItem::Image { rect, .. }
            | DisplayItem::BackdropFilter { rect, .. } => Some(krect(*rect)),
            DisplayItem::TiledImage { clip, .. } | DisplayItem::Gradient { clip, .. } => {
                Some(krect(*clip))
            }
            DisplayItem::BoxShadow {
                rect,
                offset,
                blur,
                spread,
                inset,
                ..
            } => {
                let reach = if *inset {
                    0.0
                } else {
                    blur * 1.5 + spread.max(0.0) + offset.0.abs().max(offset.1.abs())
                };
                Some(krect(*rect).inflate(f64::from(reach), f64::from(reach)))
            }
            DisplayItem::Glyphs { run, glyphs, .. } => {
                let (mut x0, mut x1, mut y0, mut y1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
                for g in glyphs.iter() {
                    x0 = x0.min(g.x);
                    x1 = x1.max(g.x);
                    y0 = y0.min(g.y);
                    y1 = y1.max(g.y);
                }
                // Glyph boxes are not known here: allow an em before the
                // origin, two after, 1.5 above the baseline and one below.
                let em = run.font_size;
                (x0 <= x1).then(|| {
                    kurbo::Rect::new(
                        f64::from(x0 - em),
                        f64::from(y0 - 1.5 * em),
                        f64::from(x1 + 2.0 * em),
                        f64::from(y1 + em),
                    )
                })
            }
            DisplayItem::PushTransform(m) => {
                stack.push(Frame::Transform(t));
                t *= Affine::new(m.map(f64::from));
                None
            }
            DisplayItem::PushClip { rect, .. } | DisplayItem::PushMask { clip: rect, .. } => {
                stack.push(Frame::Clip(levels.last().and_then(|l| l.clip)));
                narrow(&mut levels, t.transform_rect_bbox(krect(*rect)));
                None
            }
            DisplayItem::PushClipPath { path, .. } => {
                stack.push(Frame::Clip(levels.last().and_then(|l| l.clip)));
                narrow(&mut levels, t.transform_rect_bbox(path.bounding_box()));
                None
            }
            DisplayItem::PushFilter(filters) => {
                stack.push(Frame::Filter);
                levels.push(Level {
                    clip: None,
                    grow: device_reach(&t, filter::reach(filters)),
                });
                None
            }
            DisplayItem::PushOpacity(_) | DisplayItem::PushBlend(_) => {
                stack.push(Frame::Layer);
                None
            }
            DisplayItem::PopTransform
            | DisplayItem::PopClip
            | DisplayItem::PopMask
            | DisplayItem::PopFilter
            | DisplayItem::PopOpacity
            | DisplayItem::PopBlend => {
                match stack.pop() {
                    Some(Frame::Transform(old)) => t = old,
                    Some(Frame::Clip(old)) => {
                        if let Some(l) = levels.last_mut() {
                            l.clip = old;
                        }
                    }
                    Some(Frame::Filter) => {
                        levels.pop();
                    }
                    Some(Frame::Layer) | None => {}
                }
                None
            }
        };
        let Some(r) = r else { continue };
        // Innermost group first: its clips, then its filter's growth.
        let mut d = Some(t.transform_rect_bbox(r));
        for l in levels.iter().rev() {
            d = d
                .map(|d| l.clip.map_or(d, |c| d.intersect(c)))
                .filter(|d| d.width() > 0.0 && d.height() > 0.0)
                .map(|d| d.inflate(l.grow.0, l.grow.1));
        }
        if let Some(d) = d {
            out = Some(out.map_or(d, |o| o.union(d)));
        }
    }
    out
}

fn blend_mode(m: BlendMode) -> peniko::BlendMode {
    use peniko::{Compose, Mix};
    let mix = match m {
        BlendMode::Normal | BlendMode::PlusLighter => Mix::Normal,
        BlendMode::Multiply => Mix::Multiply,
        BlendMode::Screen => Mix::Screen,
        BlendMode::Overlay => Mix::Overlay,
        BlendMode::Darken => Mix::Darken,
        BlendMode::Lighten => Mix::Lighten,
        BlendMode::ColorDodge => Mix::ColorDodge,
        BlendMode::ColorBurn => Mix::ColorBurn,
        BlendMode::HardLight => Mix::HardLight,
        BlendMode::SoftLight => Mix::SoftLight,
        BlendMode::Difference => Mix::Difference,
        BlendMode::Exclusion => Mix::Exclusion,
        BlendMode::Hue => Mix::Hue,
        BlendMode::Saturation => Mix::Saturation,
        BlendMode::Color => Mix::Color,
        BlendMode::Luminosity => Mix::Luminosity,
    };
    // plus-lighter is the Porter-Duff "plus" operator, not a mix.
    let compose = if m == BlendMode::PlusLighter {
        Compose::Plus
    } else {
        Compose::SrcOver
    };
    peniko::BlendMode::new(mix, compose)
}

fn draw_gradient(
    ctx: &mut RenderContext,
    clip: Rect,
    gradient: &ResolvedGradient,
    stops: &[(f32, Color)],
    repeating: bool,
) {
    if clip.width <= 0.0 || clip.height <= 0.0 {
        return;
    }
    let extend = if repeating {
        Extend::Repeat
    } else {
        Extend::Pad
    };
    let stops: Vec<peniko::ColorStop> = stops.iter().map(|&(o, c)| (o, color(c)).into()).collect();
    let g = match *gradient {
        ResolvedGradient::Linear { start, end } => peniko::Gradient::new_linear(
            (f64::from(start.0), f64::from(start.1)),
            (f64::from(end.0), f64::from(end.1)),
        ),
        // A circle around the origin; the paint transform moves it to
        // the center and squashes it into an ellipse.
        ResolvedGradient::Radial {
            start_radius,
            end_radius,
            ..
        } => {
            peniko::Gradient::new_two_point_radial((0.0, 0.0), start_radius, (0.0, 0.0), end_radius)
        }
    }
    .with_extend(extend)
    .with_stops(stops.as_slice());
    ctx.set_paint(g);
    if let ResolvedGradient::Radial {
        center, y_scale, ..
    } = *gradient
    {
        ctx.set_paint_transform(
            Affine::translate((f64::from(center.0), f64::from(center.1)))
                * Affine::scale_non_uniform(1.0, f64::from(y_scale)),
        );
    }
    ctx.fill_rect(&krect(clip));
    ctx.reset_paint_transform();
}

/// The alpha mask of a `PushMask` item. vello masks cover the whole render
/// context in device pixels and ignore the transform, so map each device
/// pixel in `clip` back through `t` to the mask tile. `sample(u, v)` gives
/// the mask alpha (0..=1) at tile coordinates `u, v` in `0..1`.
fn build_mask(
    t: Affine,
    (w, h): (u16, u16),
    clip: Rect,
    tile: Rect,
    repeat: BackgroundRepeat,
    sample: impl Fn(f32, f32) -> f32,
) -> Mask {
    let (w, h) = (usize::from(w), usize::from(h));
    // Zeroed pages are not committed until written: only the masked area
    // costs memory.
    let mut data = vec![0u8; w * h];
    let bb = t.transform_rect_bbox(krect(clip));
    let x0 = bb.x0.floor().max(0.0) as usize;
    let y0 = bb.y0.floor().max(0.0) as usize;
    let x1 = (bb.x1.ceil().max(0.0) as usize).min(w);
    let y1 = (bb.y1.ceil().max(0.0) as usize).min(h);
    let (rx, ry) = match repeat {
        BackgroundRepeat::Repeat => (true, true),
        BackgroundRepeat::RepeatX => (true, false),
        BackgroundRepeat::RepeatY => (false, true),
        BackgroundRepeat::NoRepeat => (false, false),
    };
    let inv = t.inverse();
    let axis = |d: f32, size: f32, repeats: bool| -> Option<f32> {
        let u = d / size;
        if repeats {
            Some(u - u.floor())
        } else {
            (0.0..1.0).contains(&u).then_some(u)
        }
    };
    for py in y0..y1 {
        for px in x0..x1 {
            let p = inv * kurbo::Point::new(px as f64 + 0.5, py as f64 + 0.5);
            let (x, y) = (p.x as f32, p.y as f32);
            if x < clip.x || y < clip.y || x >= clip.right() || y >= clip.bottom() {
                continue;
            }
            let (Some(u), Some(v)) = (
                axis(x - tile.x, tile.width, rx),
                axis(y - tile.y, tile.height, ry),
            ) else {
                continue;
            };
            data[py * w + px] = (sample(u, v).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
    }
    Mask::from_parts(data, w as u16, h as u16)
}

/// Bilinear alpha of `pixmap` at `u, v` (0..1 across the image).
fn image_alpha(pixmap: &Pixmap, u: f32, v: f32) -> f32 {
    let (pw, ph) = (usize::from(pixmap.width()), usize::from(pixmap.height()));
    if pw == 0 || ph == 0 {
        return 0.0;
    }
    let fx = (u * pw as f32 - 0.5).clamp(0.0, (pw - 1) as f32);
    let fy = (v * ph as f32 - 0.5).clamp(0.0, (ph - 1) as f32);
    let (x0, y0) = (fx as usize, fy as usize);
    let (x1, y1) = ((x0 + 1).min(pw - 1), (y0 + 1).min(ph - 1));
    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
    let d = pixmap.data();
    let a = |x: usize, y: usize| f32::from(d[y * pw + x].a);
    let top = a(x0, y0) * (1.0 - tx) + a(x1, y0) * tx;
    let bottom = a(x0, y1) * (1.0 - tx) + a(x1, y1) * tx;
    (top * (1.0 - ty) + bottom * ty) / 255.0
}

/// Alpha of a resolved gradient at document point `p`.
fn gradient_alpha(
    g: &ResolvedGradient,
    stops: &[(f32, Color)],
    repeating: bool,
    p: (f32, f32),
) -> f32 {
    let t = match *g {
        ResolvedGradient::Linear { start, end } => {
            let (dx, dy) = (end.0 - start.0, end.1 - start.1);
            let len2 = dx * dx + dy * dy;
            if len2 <= 0.0 {
                0.0
            } else {
                ((p.0 - start.0) * dx + (p.1 - start.1) * dy) / len2
            }
        }
        ResolvedGradient::Radial {
            center,
            start_radius,
            end_radius,
            y_scale,
        } => {
            let dx = p.0 - center.0;
            let dy = if y_scale != 0.0 {
                (p.1 - center.1) / y_scale
            } else {
                0.0
            };
            let span = end_radius - start_radius;
            if span == 0.0 {
                1.0
            } else {
                ((dx * dx + dy * dy).sqrt() - start_radius) / span
            }
        }
    };
    let t = if repeating {
        t - t.floor()
    } else {
        t.clamp(0.0, 1.0)
    };
    let alpha = |c: Color| f32::from(c.a) / 255.0;
    let Some(&(first_at, first)) = stops.first() else {
        return 0.0;
    };
    if t <= first_at {
        return alpha(first);
    }
    for pair in stops.windows(2) {
        let ((a_at, a), (b_at, b)) = (pair[0], pair[1]);
        if t <= b_at {
            let f = if b_at > a_at {
                (t - a_at) / (b_at - a_at)
            } else {
                1.0
            };
            return alpha(a) + (alpha(b) - alpha(a)) * f;
        }
    }
    stops.last().map_or(0.0, |&(_, c)| alpha(c))
}

/// Downscaled copies of images drawn much smaller than their size, for one
/// raster pass. Bilinear sampling aliases below half size; Chrome uses
/// mipmaps there, so we pre-shrink by 2x box filtering until the draw scale
/// is at least 1/2. Upscaled images use bicubic sampling, like Chrome's
/// high-quality filter.
#[derive(Default)]
struct Mips {
    cache: Vec<(*const Pixmap, u32, Arc<Pixmap>)>,
}

impl Mips {
    /// The pixmap to sample for drawing `src` at `w` x `h` device pixels.
    fn get(&mut self, src: &Arc<Pixmap>, w: f32, h: f32) -> (Arc<Pixmap>, ImageQuality) {
        let s = (w / f32::from(src.width())).max(h / f32::from(src.height()));
        if s > 1.0 {
            return (src.clone(), ImageQuality::High);
        }
        let mut levels = 0u32;
        let mut t = s;
        while t < 0.5 && (src.width() >> (levels + 1)) > 0 && (src.height() >> (levels + 1)) > 0 {
            t *= 2.0;
            levels += 1;
        }
        if levels == 0 {
            return (src.clone(), ImageQuality::Medium);
        }
        let key = Arc::as_ptr(src);
        if let Some((_, _, p)) = self
            .cache
            .iter()
            .find(|(k, l, _)| *k == key && *l == levels)
        {
            return (p.clone(), ImageQuality::Medium);
        }
        let mut cur = halve(src);
        for _ in 1..levels {
            cur = halve(&cur);
        }
        let cur = Arc::new(cur);
        self.cache.push((key, levels, cur.clone()));
        (cur, ImageQuality::Medium)
    }
}

/// Half-size copy by 2x2 box filtering (premultiplied, so edges stay clean).
fn halve(src: &Pixmap) -> Pixmap {
    let (w, h) = (usize::from(src.width()), usize::from(src.height()));
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let mut dst = Pixmap::new(nw as u16, nh as u16);
    let s = src.data();
    let d = dst.data_mut();
    for y in 0..nh {
        let r0 = (2 * y).min(h - 1) * w;
        let r1 = (2 * y + 1).min(h - 1) * w;
        for x in 0..nw {
            let (c0, c1) = ((2 * x).min(w - 1), (2 * x + 1).min(w - 1));
            let (a, b, c, e) = (s[r0 + c0], s[r0 + c1], s[r1 + c0], s[r1 + c1]);
            let avg = |p: u8, q: u8, r: u8, t: u8| {
                ((u16::from(p) + u16::from(q) + u16::from(r) + u16::from(t) + 2) / 4) as u8
            };
            d[y * nw + x] = vello_cpu::peniko::color::PremulRgba8 {
                r: avg(a.r, b.r, c.r, e.r),
                g: avg(a.g, b.g, c.g, e.g),
                b: avg(a.b, b.b, c.b, e.b),
                a: avg(a.a, b.a, c.a, e.a),
            };
        }
    }
    dst.set_may_have_transparency(src.may_have_transparency());
    dst
}

/// Box shadow (CSS Backgrounds 3, 7.1). Outer shadows are drawn only outside
/// the border box `rect`; inset shadows only inside the padding box `rect`.
/// Blur radius B is a gaussian with standard deviation B / 2.
#[allow(clippy::too_many_arguments)]
fn draw_box_shadow(
    ctx: &mut RenderContext,
    rect: Rect,
    radii: [f32; 4],
    offset: (f32, f32),
    blur: f32,
    spread: f32,
    c: Color,
    inset: bool,
) {
    if c.is_transparent() || rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }
    let sigma = blur / 2.0;
    let grow = if inset { -spread } else { spread };
    let shape = Rect {
        x: rect.x + offset.0 - grow,
        y: rect.y + offset.1 - grow,
        width: rect.width + 2.0 * grow,
        height: rect.height + 2.0 * grow,
    };
    // Radii follow the spread; square corners stay square.
    let shape_radii = radii.map(|r| if r > 0.0 { (r + grow).max(0.0) } else { 0.0 });
    let corner = shape_radii.iter().copied().fold(0.0f32, f32::max);
    ctx.set_paint(color(c));
    if inset {
        ctx.set_fill_rule(Fill::NonZero);
        let area = rounded(rect, radii);
        ctx.push_clip_path(&area);
        if shape.width <= 0.0 || shape.height <= 0.0 {
            // The spread swallowed the hole: the whole padding box is shadow.
            ctx.fill_path(&area);
        } else if sigma > 0.0 {
            ctx.fill_blurred_rounded_rect_in(&area, &krect(shape), corner, sigma, true);
        } else {
            let mut ring = area.clone();
            ring.extend(rounded(shape, shape_radii));
            ctx.set_fill_rule(Fill::EvenOdd);
            ctx.fill_path(&ring);
            ctx.set_fill_rule(Fill::NonZero);
        }
        ctx.pop_clip();
        return;
    }
    if shape.width <= 0.0 || shape.height <= 0.0 {
        return;
    }
    // Clip out the border box: everything within `reach` of the shadow except the box.
    let reach = f64::from(3.0 * sigma + 1.0);
    let bounds = krect(shape).union(krect(rect)).inflate(reach, reach);
    let mut outside = bounds.to_path(0.1);
    outside.extend(rounded(rect, radii));
    ctx.set_fill_rule(Fill::EvenOdd);
    ctx.push_clip_path(&outside);
    ctx.set_fill_rule(Fill::NonZero);
    if sigma > 0.0 {
        ctx.fill_blurred_rounded_rect(&krect(shape), corner, sigma);
    } else if has_radius(shape_radii) {
        ctx.fill_path(&rounded(shape, shape_radii));
    } else {
        ctx.fill_rect(&krect(shape));
    }
    ctx.pop_clip();
}

fn image_paint(pixmap: &Arc<Pixmap>, extend: Extend) -> vello_cpu::PaintType {
    vello_cpu::PaintType::Image(Image {
        image: ImageSource::Pixmap(pixmap.clone()),
        sampler: ImageSampler {
            x_extend: extend,
            y_extend: extend,
            quality: ImageQuality::Medium,
            alpha: 1.0,
        },
    })
}

fn polygon(pts: &[(f32, f32)]) -> BezPath {
    let mut p = BezPath::new();
    p.move_to((f64::from(pts[0].0), f64::from(pts[0].1)));
    for &(x, y) in &pts[1..] {
        p.line_to((f64::from(x), f64::from(y)));
    }
    p.close_path();
    p
}

fn draw_border(
    ctx: &mut RenderContext,
    rect: Rect,
    w: Edges<f32>,
    colors: Edges<Color>,
    styles: Edges<BorderStyle>,
    radii: [f32; 4],
) {
    let visible =
        |width: f32, c: Color, s: BorderStyle| width > 0.0 && !c.is_transparent() && s.is_visible();
    let uniform = colors.top == colors.right
        && colors.top == colors.bottom
        && colors.top == colors.left
        && w.top == w.right
        && w.top == w.bottom
        && w.top == w.left
        && styles.top == styles.right
        && styles.top == styles.bottom
        && styles.top == styles.left;
    if uniform {
        if !visible(w.top, colors.top, styles.top) {
            return;
        }
        match styles.top {
            BorderStyle::Solid => {
                fill_ring(ctx, rect, w, radii, colors.top);
                return;
            }
            BorderStyle::Double if w.top >= 3.0 => {
                let third = w.map(|v| v / 3.0);
                fill_ring(ctx, rect, third, radii, colors.top);
                let inner = rect.inset(w.map(|v| v * 2.0 / 3.0));
                fill_ring(
                    ctx,
                    inner,
                    third,
                    crate::inner_radii(radii, w.map(|v| v * 2.0 / 3.0)),
                    colors.top,
                );
                return;
            }
            BorderStyle::Dashed | BorderStyle::Dotted if has_radius(radii) => {
                // Stroke along the middle of the border.
                let half = w.top / 2.0;
                let mid = rect.inset(Edges::all(half));
                let path = rounded(mid, radii.map(|r| (r - half).max(0.0)));
                ctx.set_paint(color(colors.top));
                ctx.set_stroke(dash_stroke(styles.top, w.top));
                ctx.stroke_path(&path);
                return;
            }
            _ => {}
        }
    }
    // Per side: trapezoids, so corners join diagonally like browsers draw
    // them. With radii, triangles to the center clipped to the border ring.
    let (x0, y0, x1, y1) = (rect.x, rect.y, rect.right(), rect.bottom());
    let (ix0, iy0, ix1, iy1) = (x0 + w.left, y0 + w.top, x1 - w.right, y1 - w.bottom);
    let round = has_radius(radii);
    if round {
        ctx.set_fill_rule(Fill::EvenOdd);
        ctx.push_clip_path(&ring(rect, w, radii));
        ctx.set_fill_rule(Fill::NonZero);
    }
    let (cx, cy) = (x0 + rect.width / 2.0, y0 + rect.height / 2.0);
    // (width, color, style, outer edge, inner edge, outward normal is left/top)
    let sides = [
        (
            w.top,
            colors.top,
            styles.top,
            [(x0, y0), (x1, y0)],
            [(ix1, iy0), (ix0, iy0)],
            true,
        ),
        (
            w.right,
            colors.right,
            styles.right,
            [(x1, y0), (x1, y1)],
            [(ix1, iy1), (ix1, iy0)],
            false,
        ),
        (
            w.bottom,
            colors.bottom,
            styles.bottom,
            [(x1, y1), (x0, y1)],
            [(ix0, iy1), (ix1, iy1)],
            false,
        ),
        (
            w.left,
            colors.left,
            styles.left,
            [(x0, y1), (x0, y0)],
            [(ix0, iy0), (ix0, iy1)],
            true,
        ),
    ];
    for (width, c, style, outer, inner, top_left) in sides {
        if !visible(width, c, style) {
            continue;
        }
        let shade = match style {
            // 3D styles: darken the top/left or bottom/right like Chrome.
            BorderStyle::Inset | BorderStyle::Groove if top_left => darken(c),
            BorderStyle::Outset | BorderStyle::Ridge if !top_left => darken(c),
            _ => c,
        };
        let area = if round {
            polygon(&[outer[0], outer[1], (cx, cy)])
        } else {
            polygon(&[outer[0], outer[1], inner[0], inner[1]])
        };
        ctx.set_paint(color(shade));
        match style {
            BorderStyle::Dashed | BorderStyle::Dotted => {
                // Stroke the middle line of the side, clipped to its area.
                let mid = |a: (f32, f32), b: (f32, f32)| {
                    kurbo::Point::new(f64::from((a.0 + b.0) / 2.0), f64::from((a.1 + b.1) / 2.0))
                };
                let mut line = BezPath::new();
                line.move_to(mid(outer[0], inner[1]));
                line.line_to(mid(outer[1], inner[0]));
                ctx.push_clip_path(&area);
                ctx.set_stroke(dash_stroke(style, width));
                ctx.stroke_path(&line);
                ctx.pop_clip();
            }
            BorderStyle::Double if width >= 3.0 => {
                // Outer and inner thirds of the side.
                let lerp = |a: (f32, f32), b: (f32, f32), t: f32| {
                    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
                };
                let (o0, o1, i0, i1) = (outer[0], outer[1], inner[1], inner[0]);
                let band = |t0: f32, t1: f32| {
                    polygon(&[
                        lerp(o0, i0, t0),
                        lerp(o1, i1, t0),
                        lerp(o1, i1, t1),
                        lerp(o0, i0, t1),
                    ])
                };
                ctx.push_clip_path(&area);
                ctx.fill_path(&band(0.0, 1.0 / 3.0));
                ctx.fill_path(&band(2.0 / 3.0, 1.0));
                ctx.pop_clip();
            }
            _ => ctx.fill_path(&area),
        }
    }
    if round {
        ctx.pop_clip();
    }
}

/// The area between the border box and the padding box.
fn ring(rect: Rect, w: Edges<f32>, radii: [f32; 4]) -> BezPath {
    let mut path = rounded(rect, radii);
    path.extend(rounded(rect.inset(w), crate::inner_radii(radii, w)));
    path
}

fn fill_ring(ctx: &mut RenderContext, rect: Rect, w: Edges<f32>, radii: [f32; 4], c: Color) {
    ctx.set_paint(color(c));
    if has_radius(radii) {
        ctx.set_fill_rule(Fill::EvenOdd);
        ctx.fill_path(&ring(rect, w, radii));
        ctx.set_fill_rule(Fill::NonZero);
    } else {
        // Four rectangles: cheaper than an even-odd path.
        let (x0, y0, x1, y1) = (rect.x, rect.y, rect.right(), rect.bottom());
        let mut fill = |x: f32, y: f32, r: f32, b: f32| {
            if r > x && b > y {
                ctx.fill_rect(&kurbo::Rect::new(
                    f64::from(x),
                    f64::from(y),
                    f64::from(r),
                    f64::from(b),
                ));
            }
        };
        fill(x0, y0, x1, y0 + w.top);
        fill(x0, y1 - w.bottom, x1, y1);
        fill(x0, y0 + w.top, x0 + w.left, y1 - w.bottom);
        fill(x1 - w.right, y0 + w.top, x1, y1 - w.bottom);
    }
}

/// Chrome's dash pattern: thin dashed borders are 3 on, 2 off (in border
/// widths), thick ones 2 on, 1 off; dotted borders are dots one width apart
/// (round from 3 px up).
fn dash_stroke(style: BorderStyle, width: f32) -> kurbo::Stroke {
    let w = f64::from(width);
    match style {
        BorderStyle::Dotted if width >= 3.0 => kurbo::Stroke::new(w)
            .with_caps(kurbo::Cap::Round)
            .with_dashes(0.0, [0.0, 2.0 * w]),
        BorderStyle::Dotted => kurbo::Stroke::new(w).with_dashes(0.0, [w, w]),
        _ if width >= 3.0 => kurbo::Stroke::new(w).with_dashes(0.0, [2.0 * w, w]),
        _ => kurbo::Stroke::new(w).with_dashes(0.0, [3.0 * w, 2.0 * w]),
    }
}

/// Synthetic bold outset per side, like Skia's fake bold (which Chrome uses):
/// the outline grows by `size / 24` at 9 px down to `size / 32` at 36 px, in total.
fn fake_bold_outset(size: f32) -> f32 {
    let t = ((size - 9.0) / (36.0 - 9.0)).clamp(0.0, 1.0);
    let ratio = 1.0 / 24.0 + (1.0 / 32.0 - 1.0 / 24.0) * t;
    size * ratio / 2.0
}

fn darken(c: Color) -> Color {
    Color::rgba(
        (f32::from(c.r) * 0.6) as u8,
        (f32::from(c.g) * 0.6) as u8,
        (f32::from(c.b) * 0.6) as u8,
        c.a,
    )
}
