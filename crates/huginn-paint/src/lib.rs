//! huginn-paint: display list building and rasterization.
//!
//! - [`build_display_list`] walks a [`LayoutTree`] in paint order and emits a
//!   flat [`DisplayList`] (our code). Items borrow glyphs, fonts and images
//!   from the layout tree, so nothing is copied.
//! - [`Rasterizer`] turns a display list into pixels. [`CpuRasterizer`] uses
//!   vello_cpu (library). A GPU rasterizer for `huginn open` can implement
//!   the same trait later.
//! - [`encode_png`] writes the result.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io::Write;

use huginn_layout::{
    BoxId, BoxKind, DecorationShape, LayoutTree, PositionedGlyph, Rect, Size, TextRun,
};
use huginn_net::DecodedImage;
use huginn_style::{
    BackgroundImage, BackgroundRepeat, BlendMode, BorderStyle, ClipPath, Color, Edges,
    FilterFunction, Float, Gradient, Isolation, Overflow, Position, ShapeRadius, Visibility,
};
use vello_cpu::kurbo::{Affine, BezPath, Shape};

mod background;
mod filter;
pub use background::ResolvedGradient;

/// One paint operation. Coordinates are absolute document CSS px.
#[derive(Debug, Clone)]
pub enum DisplayItem<'a> {
    /// Solid rectangle (optionally rounded: radii are top-left, top-right,
    /// bottom-right, bottom-left).
    Rect {
        rect: Rect,
        radii: [f32; 4],
        color: Color,
    },
    /// Box border. Each side has its own width, color and style.
    Border {
        rect: Rect,
        widths: Edges<f32>,
        colors: Edges<Color>,
        styles: Edges<BorderStyle>,
        radii: [f32; 4],
    },
    /// A run of glyphs (one font, size and color).
    Glyphs {
        run: &'a TextRun,
        glyphs: &'a [PositionedGlyph],
        coords: &'a [i16],
    },
    /// An image stretched into `rect`.
    Image {
        rect: Rect,
        image: &'a DecodedImage,
    },
    /// A background image tiled from `origin`, clipped to `clip`. `origin` is
    /// one tile: the image is scaled to its size.
    TiledImage {
        clip: Rect,
        origin: Rect,
        image: &'a DecodedImage,
        repeat: BackgroundRepeat,
    },
    /// A gradient filling `clip` (one background tile or the painting area).
    /// Stops are (offset along the gradient, color), offsets ascending.
    Gradient {
        clip: Rect,
        gradient: ResolvedGradient,
        stops: Box<[(f32, Color)]>,
        /// Repeat the stops beyond 0..=1 instead of padding with the end colors.
        repeating: bool,
    },
    /// Box shadow. Outer: `rect` is the border box, the shadow is drawn only
    /// outside it. Inset: `rect` is the padding box, drawn only inside it.
    BoxShadow {
        rect: Rect,
        radii: [f32; 4],
        offset: (f32, f32),
        blur: f32,
        spread: f32,
        color: Color,
        inset: bool,
    },
    PushClip {
        rect: Rect,
        radii: [f32; 4],
    },
    /// Clip to a path until the matching `PopClip` (a `clip-path` shape).
    PushClipPath {
        path: BezPath,
        even_odd: bool,
    },
    PopClip,
    PushOpacity(f32),
    PopOpacity,
    /// Multiply everything until the matching `PopMask` by the alpha of a
    /// mask image (CSS Masking 1). `clip` is the mask painting area (the
    /// border box); `tile` is one tile of the mask image, repeated per
    /// `repeat`. Nothing outside `clip` shows.
    PushMask {
        clip: Rect,
        tile: Rect,
        repeat: BackgroundRepeat,
        source: MaskSource<'a>,
    },
    PopMask,
    /// Multiply the current transform by the affine `[a, b, c, d, e, f]`
    /// (`x' = a x + c y + e`, `y' = b x + d y + f`, document coordinates).
    PushTransform([f32; 6]),
    PopTransform,
    /// Run everything until the matching `PopFilter` through a `filter`
    /// list (Filter Effects 1), left to right, as one group. The result may
    /// reach past the content (blur, drop shadow).
    PushFilter(&'a [FilterFunction]),
    PopFilter,
    /// Composite everything until the matching `PopBlend` as one group onto
    /// what is below it with this `mix-blend-mode`. `Normal` only isolates
    /// the group (a stacking context with blending children).
    PushBlend(BlendMode),
    PopBlend,
    /// `backdrop-filter` (Filter Effects 2): what is painted below, back to
    /// the backdrop root (the nearest enclosing opacity, mask, blend or
    /// filter group, else the canvas), clipped to the border box `rect`,
    /// run through `filters` with mirrored edges and painted inside `rect`
    /// with `radii`. It follows the element's own layer pushes; the
    /// innermost `own_layers` of the enclosing layers are those.
    BackdropFilter {
        rect: Rect,
        radii: [f32; 4],
        filters: &'a [FilterFunction],
        own_layers: u8,
    },
}

/// The image of a [`DisplayItem::PushMask`].
#[derive(Debug, Clone)]
pub enum MaskSource<'a> {
    Image(&'a DecodedImage),
    /// A gradient resolved for the first tile.
    Gradient {
        gradient: ResolvedGradient,
        stops: Box<[(f32, Color)]>,
        repeating: bool,
    },
}

/// A flat list of paint operations for one document.
#[derive(Debug, Clone)]
pub struct DisplayList<'a> {
    pub items: Vec<DisplayItem<'a>>,
    /// Canvas color (root or body background, white if transparent).
    pub canvas: Color,
    /// Size of the whole document.
    pub page_size: Size,
}

/// Build the display list. `cull` drops boxes that cannot touch this region
/// (pass the viewport for a screenshot, `None` to keep everything).
///
/// Paint order follows CSS 2.1 Appendix E. The root element and every box
/// that creates a stacking context (positioned with a `z-index`, flex/grid
/// items with a `z-index`, `opacity < 1`, a transform, mask, clip-path,
/// filter, backdrop-filter or blend mode, `isolation: isolate`,
/// `position: fixed/sticky`) paint as:
/// own background and border, child contexts with negative `z-index`,
/// backgrounds of in-flow blocks, floats, inline content (text, replaced
/// images, atomic inlines), then positioned descendants and child contexts
/// with `z-index` >= 0, by z-index then tree order. Floats, atomic inlines and
/// positioned boxes with `z-index: auto` paint the same way as pseudo
/// stacking contexts, but their positioned descendants join the parent
/// context. `overflow` clips follow the containing-block chain: absolutely
/// positioned boxes escape clips of non-positioned ancestors, fixed boxes
/// escape all of them.
pub fn build_display_list<'a>(tree: &'a LayoutTree, cull: Option<Rect>) -> DisplayList<'a> {
    let mut b = ListBuilder {
        tree,
        cull,
        items: Vec::new(),
        canvas_source: None,
        clips: Vec::new(),
        emitted: Vec::new(),
        clips_dirty: false,
        cb_len: 0,
        pending: BinaryHeap::new(),
        floats: Vec::new(),
        canvas: Color::WHITE,
        blend_inside: false,
    };
    let mut canvas = Color::WHITE;
    if let Some(root) = tree.root {
        // CSS Backgrounds 3, 2.11.2: the root background (or body's, if the
        // root has none) paints the whole canvas.
        let root_style = &tree.get(root).style;
        if !root_style.background_color.is_transparent() {
            canvas = blend_over_white(root_style.background_color);
            b.canvas_source = Some(root);
        } else if let Some(body) = tree.children(root).find(|&c| {
            tree.get(c)
                .node
                .is_some_and(|_| tree.get(c).style.display != huginn_style::Display::None)
        }) && !tree.get(body).style.background_color.is_transparent()
        {
            canvas = blend_over_white(tree.get(body).style.background_color);
            b.canvas_source = Some(body);
        }
        b.canvas = canvas;
        if tree.get(root).style.opacity > 0.0 {
            b.paint_stacking_context(root);
        }
    }
    DisplayList {
        items: b.items,
        canvas,
        page_size: tree.document_size,
    }
}

fn blend_over_white(c: Color) -> Color {
    let a = f32::from(c.a) / 255.0;
    let mix = |v: u8| (f32::from(v) * a + 255.0 * (1.0 - a)).round() as u8;
    Color::rgb(mix(c.r), mix(c.g), mix(c.b))
}

/// An `overflow` clip: the padding box of `owner`, with inner radii.
#[derive(Copy, Clone, Debug)]
struct Clip {
    owner: BoxId,
    rect: Rect,
    radii: [f32; 4],
}

/// A box painted later in its stacking context: a positioned descendant, a
/// child stacking context or a float.
struct Deferred {
    z: i32,
    id: BoxId,
    /// Creates a real stacking context (else a pseudo one).
    real: bool,
    /// Clip chain that applies to the box, relative to the stacking context.
    clips: Vec<Clip>,
    /// Prefix of `clips` that applies to its absolutely positioned descendants.
    cb_len: usize,
}

impl Deferred {
    fn key(&self) -> (i32, u32) {
        (self.z, self.id.0)
    }
}

impl PartialEq for Deferred {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}
impl Eq for Deferred {}
impl PartialOrd for Deferred {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Deferred {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(&other.key())
    }
}

/// How a child box takes part in its parent's painting.
enum Layer {
    /// In-flow, non-positioned: painted in phases with its parent.
    Flow,
    Float,
    /// Positioned or stacking context: painted from the context's queue.
    Deferred {
        z: i32,
        real: bool,
    },
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Phase {
    /// Only find deferred boxes and floats.
    Collect,
    /// Backgrounds and borders of in-flow block-level boxes.
    Blocks,
    /// Text, replaced content and atomic inlines.
    Content,
}

struct ListBuilder<'a> {
    tree: &'a LayoutTree,
    cull: Option<Rect>,
    items: Vec<DisplayItem<'a>>,
    canvas_source: Option<BoxId>,
    /// Clip chain of the box being painted, relative to the current stacking context.
    clips: Vec<Clip>,
    /// Clips pushed into `items` right now, relative to the current stacking context.
    emitted: Vec<Clip>,
    /// `clips` changed since `emitted` was synced.
    clips_dirty: bool,
    /// Prefix of `clips` that applies to absolutely positioned descendants.
    cb_len: usize,
    /// Deferred boxes of the current stacking context, smallest (z, tree order) first.
    pending: BinaryHeap<Reverse<Deferred>>,
    /// Floats of the current (pseudo) stacking context, in tree order.
    floats: Vec<Deferred>,
    /// The canvas color (root or body background).
    canvas: Color,
    /// The current stacking context has a child context with a
    /// `mix-blend-mode`: it must composite as an isolated group.
    blend_inside: bool,
}

fn radii(b: &huginn_layout::LayoutBox) -> [f32; 4] {
    let r = &b.style.border_radius;
    let w = b.rect.width;
    let h = b.rect.height;
    let max = w.min(h) / 2.0;
    [r.top_left, r.top_right, r.bottom_right, r.bottom_left]
        .map(|v| v.resolve(Some(w)).min(max).max(0.0))
}

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let r = a.right().min(b.right());
    let bot = a.bottom().min(b.bottom());
    (r > x && bot > y).then_some(Rect {
        x,
        y,
        width: r - x,
        height: bot - y,
    })
}

/// Radii of the padding box: outer radii minus the adjacent border widths.
pub(crate) fn inner_radii(r: [f32; 4], w: Edges<f32>) -> [f32; 4] {
    [
        (r[0] - w.top.max(w.left)).max(0.0),
        (r[1] - w.top.max(w.right)).max(0.0),
        (r[2] - w.bottom.max(w.right)).max(0.0),
        (r[3] - w.bottom.max(w.left)).max(0.0),
    ]
}

fn classify(b: &huginn_layout::LayoutBox, parent_is_flex_or_grid: bool) -> Layer {
    let s = &b.style;
    let positioned = s.position != Position::Static;
    // z-index applies to positioned boxes and to flex and grid items.
    let z = if positioned || parent_is_flex_or_grid {
        s.z_index
    } else {
        None
    };
    let real = z.is_some()
        || s.opacity < 1.0
        || s.transform.is_some()
        || s.mask.is_some()
        || s.clip_path.is_some()
        || s.filter.is_some()
        || s.backdrop_filter.is_some()
        || s.mix_blend_mode != BlendMode::Normal
        || s.isolation == Isolation::Isolate
        || matches!(s.position, Position::Fixed | Position::Sticky);
    if real || positioned {
        Layer::Deferred {
            z: z.unwrap_or(0),
            real,
        }
    } else if s.float != Float::None {
        Layer::Float
    } else {
        Layer::Flow
    }
}

/// The clip item of a box with `clip-path` (CSS Masking 1 basic shapes, the
/// border box as reference box), or `None` if the shape has no area: then
/// nothing of the box shows.
fn clip_path_item<'a>(b: &huginn_layout::LayoutBox, cp: &ClipPath) -> Option<DisplayItem<'a>> {
    let r = b.rect;
    let (w, h) = (r.width, r.height);
    let ellipse = |cx: f32, cy: f32, rx: f32, ry: f32| {
        (rx > 0.0 && ry > 0.0).then(|| DisplayItem::PushClipPath {
            path: vello_cpu::kurbo::Ellipse::new(
                (f64::from(r.x + cx), f64::from(r.y + cy)),
                (f64::from(rx), f64::from(ry)),
                0.0,
            )
            .to_path(0.1),
            even_odd: false,
        })
    };
    // closest-side / farthest-side along one axis from a center offset.
    let side = |c: f32, len: f32, far: bool| {
        let (a, b) = (c.abs(), (len - c).abs());
        if far { a.max(b) } else { a.min(b) }
    };
    match cp {
        ClipPath::Inset { inset, radius } => {
            let (mut l, mut rt) = (inset.left.resolve(Some(w)), inset.right.resolve(Some(w)));
            let (mut t, mut bt) = (inset.top.resolve(Some(h)), inset.bottom.resolve(Some(h)));
            // A pair of insets that adds up to more than the box scales down.
            if l + rt > w {
                let f = w / (l + rt);
                (l, rt) = (l * f, rt * f);
            }
            if t + bt > h {
                let f = h / (t + bt);
                (t, bt) = (t * f, bt * f);
            }
            let rect = Rect {
                x: r.x + l,
                y: r.y + t,
                width: w - l - rt,
                height: h - t - bt,
            };
            if rect.width <= 0.0 || rect.height <= 0.0 {
                return None;
            }
            let mut radii = [
                radius.top_left,
                radius.top_right,
                radius.bottom_right,
                radius.bottom_left,
            ]
            .map(|v| v.resolve(Some(w)).max(0.0));
            // Corners that would overlap shrink together (CSS Backgrounds 3, 5.5).
            let f = [
                (radii[0] + radii[1], rect.width),
                (radii[1] + radii[2], rect.height),
                (radii[2] + radii[3], rect.width),
                (radii[3] + radii[0], rect.height),
            ]
            .iter()
            .filter(|(sum, _)| *sum > 0.0)
            .fold(1.0f32, |f, (sum, len)| f.min(len / sum));
            radii = radii.map(|v| v * f);
            Some(DisplayItem::PushClip { rect, radii })
        }
        ClipPath::Circle { radius, center } => {
            let (cx, cy) = (center.0.resolve(Some(w)), center.1.resolve(Some(h)));
            let corner = |far: bool| side(cx, w, far).hypot(side(cy, h, far));
            let rad = match radius {
                ShapeRadius::Length(v) => v.resolve(Some(w.hypot(h) / std::f32::consts::SQRT_2)),
                ShapeRadius::ClosestSide => side(cx, w, false).min(side(cy, h, false)),
                ShapeRadius::FarthestSide => side(cx, w, true).max(side(cy, h, true)),
                ShapeRadius::ClosestCorner => corner(false),
                ShapeRadius::FarthestCorner => corner(true),
            };
            ellipse(cx, cy, rad, rad)
        }
        ClipPath::Ellipse {
            radius_x,
            radius_y,
            center,
        } => {
            let (cx, cy) = (center.0.resolve(Some(w)), center.1.resolve(Some(h)));
            // Corner sizes keep the aspect ratio of the matching side size
            // and pass through the corner, which is sqrt(2) times as far
            // (CSS Images 3, radial-gradient sizes).
            let axis = |rad: &ShapeRadius, c: f32, len: f32| match rad {
                ShapeRadius::Length(v) => v.resolve(Some(len)),
                ShapeRadius::ClosestSide => side(c, len, false),
                ShapeRadius::FarthestSide => side(c, len, true),
                ShapeRadius::ClosestCorner => side(c, len, false) * std::f32::consts::SQRT_2,
                ShapeRadius::FarthestCorner => side(c, len, true) * std::f32::consts::SQRT_2,
            };
            ellipse(cx, cy, axis(radius_x, cx, w), axis(radius_y, cy, h))
        }
        ClipPath::Polygon { even_odd, points } => {
            if points.len() < 3 {
                return None;
            }
            let mut path = BezPath::new();
            for (i, (x, y)) in points.iter().enumerate() {
                let p = (
                    f64::from(r.x + x.resolve(Some(w))),
                    f64::from(r.y + y.resolve(Some(h))),
                );
                if i == 0 {
                    path.move_to(p);
                } else {
                    path.line_to(p);
                }
            }
            path.close_path();
            Some(DisplayItem::PushClipPath {
                path,
                even_odd: *even_odd,
            })
        }
    }
}

/// The `PushMask` item of a box with `mask-image`, or `None` if the mask
/// image is missing or empty (then nothing of the box may show). The mask is
/// positioned and clipped against the border box.
fn mask_item<'a>(
    b: &'a huginn_layout::LayoutBox,
    m: &huginn_style::Mask,
) -> Option<DisplayItem<'a>> {
    let area = b.rect;
    let intrinsic = match &m.image {
        BackgroundImage::Url(_) => {
            let img = b.mask_image.as_ref()?;
            Some((img.width() as f32, img.height() as f32))
        }
        _ => None,
    };
    let (tw, th) = background::tile_size(m.size, area, intrinsic);
    if tw <= 0.0 || th <= 0.0 {
        return None;
    }
    let (x, y) = background::tile_origin(m.position, area, (tw, th));
    let tile = Rect {
        x,
        y,
        width: tw,
        height: th,
    };
    let source = match &m.image {
        BackgroundImage::Url(_) => MaskSource::Image(b.mask_image.as_ref()?),
        BackgroundImage::LinearGradient(g) | BackgroundImage::RadialGradient(g) => {
            let (gradient, stops, repeating) = background::resolve_gradient(g, tile)?;
            MaskSource::Gradient {
                gradient,
                stops: stops.into_boxed_slice(),
                repeating,
            }
        }
    };
    Some(DisplayItem::PushMask {
        clip: area,
        tile,
        repeat: m.repeat,
        source,
    })
}

impl<'a> ListBuilder<'a> {
    fn push(&mut self, item: DisplayItem<'a>) {
        if self.clips_dirty {
            self.sync_clips();
        }
        self.items.push(item);
    }

    /// Make the pushed clips in `items` match `clips` (lazily, so empty
    /// clips never reach the list).
    fn sync_clips(&mut self) {
        let keep = self
            .emitted
            .iter()
            .zip(&self.clips)
            .take_while(|(a, b)| a.owner == b.owner)
            .count();
        while self.emitted.len() > keep {
            self.emitted.pop();
            self.items.push(DisplayItem::PopClip);
        }
        for c in &self.clips[keep..] {
            self.items.push(DisplayItem::PushClip {
                rect: c.rect,
                radii: c.radii,
            });
            self.emitted.push(*c);
        }
        self.clips_dirty = false;
    }

    fn set_clips(&mut self, clips: Vec<Clip>) -> Vec<Clip> {
        self.clips_dirty = true;
        std::mem::replace(&mut self.clips, clips)
    }

    /// Push the `overflow` clip of `id` onto the clip chain, if it clips.
    fn enter_clip(&mut self, id: BoxId) -> bool {
        let b = self.tree.get(id);
        let s = &b.style;
        if s.overflow_x == Overflow::Visible && s.overflow_y == Overflow::Visible {
            return false;
        }
        self.clips.push(Clip {
            owner: id,
            rect: b.padding_rect(),
            radii: inner_radii(radii(b), b.border),
        });
        self.clips_dirty = true;
        true
    }

    fn leave_clip(&mut self, entered: bool) {
        if entered {
            self.clips.pop();
            self.clips_dirty = true;
        }
    }

    fn in_view(&self, r: &Rect) -> bool {
        self.cull.is_none_or(|c| c.intersects(r) || r.width == 0.0)
    }

    /// Paint a box that creates a stacking context, with all its descendants.
    ///
    /// Items around the content, outside in: transform, clip-path, blend
    /// (or an isolation group), opacity, mask, the filtered backdrop, then
    /// the filter (Filter Effects 1: filter first, then clipping, masking
    /// and opacity; blending composites the result).
    fn paint_stacking_context(&mut self, id: BoxId) {
        let b = self.tree.get(id);
        let s = &b.style;
        let root = Some(id) == self.tree.root;
        let opacity = s.opacity;
        let positioned = s.position != Position::Static;
        // Filters that change pixels. An identity list (`blur(0)`) only
        // makes a stacking context and a containing block.
        let active = |f: &'a [FilterFunction]| f.iter().any(|f| !f.is_identity());
        let filter = s.filter.as_deref().filter(|f| active(f));
        let backdrop = s.backdrop_filter.as_deref().filter(|f| active(f));
        // The root blends with the canvas only: nothing to do.
        let blend = if root {
            BlendMode::Normal
        } else {
            s.mix_blend_mode
        };
        let transform = s
            .transform
            .as_deref()
            .map(|ops| background::transform_affine(ops, s.transform_origin, b.rect))
            .filter(|m| *m != Affine::IDENTITY);
        // CSS Masking 1: a mask layer whose image is missing (failed to
        // load, or empty) still masks, so the element and its descendants
        // are invisible.
        let mask = match s.mask.as_deref() {
            Some(m) => match mask_item(b, m) {
                Some(item) => Some(item),
                None => return,
            },
            None => None,
        };
        // A shape without area clips the element and its descendants away.
        let clip_path = match s.clip_path.as_deref() {
            Some(cp) => match clip_path_item(b, cp) {
                Some(item) => Some(item),
                None => return,
            },
            None => None,
        };
        // The context starts with no clips of its own; the caller's clips
        // stay pushed around it.
        let saved_clips = self.set_clips(Vec::new());
        let saved_emitted = std::mem::take(&mut self.emitted);
        let saved_cb = std::mem::replace(&mut self.cb_len, 0);
        let saved_pending = std::mem::take(&mut self.pending);
        let saved_floats = std::mem::take(&mut self.floats);
        self.clips_dirty = false;

        // Culling works in untransformed coordinates only.
        let saved_cull = self.cull;
        if let Some(m) = transform {
            if m.determinant().abs() < 1e-9 {
                // Flattened to nothing (e.g. scale(0)).
                self.restore_context(
                    saved_clips,
                    saved_emitted,
                    saved_cb,
                    saved_pending,
                    saved_floats,
                );
                return;
            }
            self.items
                .push(DisplayItem::PushTransform(m.as_coeffs().map(|v| v as f32)));
            self.cull = None;
        }
        let saved_blend_inside = std::mem::replace(&mut self.blend_inside, false);
        // clip-path applies in the element's own (transformed) coordinates.
        let clip_path_layer = clip_path.is_some();
        if let Some(item) = clip_path {
            self.items.push(item);
        }
        let layers_start = self.items.len();
        let opacity_layer = opacity < 1.0;
        let mask_layer = mask.is_some();
        // An element with backdrop-filter is a backdrop root for its
        // descendants, so it gets a group of its own.
        let blend_layer =
            blend != BlendMode::Normal || (backdrop.is_some() && !opacity_layer && !mask_layer);
        if blend_layer {
            self.items.push(DisplayItem::PushBlend(blend));
        }
        if opacity_layer {
            self.items.push(DisplayItem::PushOpacity(opacity));
        }
        if let Some(item) = mask {
            self.items.push(item);
        }
        if let Some(filters) = backdrop {
            self.items.push(DisplayItem::BackdropFilter {
                rect: b.rect,
                radii: radii(b),
                filters,
                own_layers: u8::from(blend_layer) + u8::from(opacity_layer) + u8::from(mask_layer),
            });
        }
        if let Some(filters) = filter {
            self.items.push(DisplayItem::PushFilter(filters));
            // Content within the filter's reach of the region shows in it.
            let reach = filter::reach(filters);
            self.cull = self.cull.map(|c| Rect {
                x: c.x - reach,
                y: c.y - reach,
                width: c.width + 2.0 * reach,
                height: c.height + 2.0 * reach,
            });
        }
        let start = self.items.len();
        if filter.is_some() && root && self.canvas_source.is_some() {
            // The root's filter also applies to the root (or body)
            // background on the canvas, not to the default white (Chrome).
            self.items.push(DisplayItem::Rect {
                rect: Rect {
                    x: -1.0e6,
                    y: -1.0e6,
                    width: 2.0e6 + self.tree.document_size.width,
                    height: 2.0e6 + self.tree.document_size.height,
                },
                radii: [0.0; 4],
                color: self.canvas,
            });
        }

        self.box_background(id);
        self.outline(id);
        let clipped = self.enter_clip(id);
        // Transforms and filters make a containing block for absolutely
        // positioned and fixed descendants.
        if positioned || transform.is_some() || s.filter.is_some() || s.backdrop_filter.is_some() {
            self.cb_len = self.clips.len();
        }
        self.walk(id, Phase::Collect);
        // Child contexts with negative z-index.
        while self.pending.peek().is_some_and(|d| d.0.z < 0) {
            let Some(Reverse(d)) = self.pending.pop() else {
                break;
            };
            self.paint_deferred(d);
        }
        self.walk(id, Phase::Blocks);
        self.paint_floats();
        self.box_content(id);
        self.walk(id, Phase::Content);
        // Positioned descendants and child contexts with z-index >= 0. Painting
        // one can queue more (positioned descendants of z-index:auto boxes).
        while let Some(Reverse(d)) = self.pending.pop() {
            self.paint_deferred(d);
        }
        self.leave_clip(clipped);
        self.clips.clear();
        self.sync_clips();

        let empty = self.items.len() == start;
        // Stacking contexts are isolated groups (Compositing 1, 3.2): a
        // blending child must not see what is below this context.
        if self.blend_inside
            && !empty
            && !root
            && !blend_layer
            && !opacity_layer
            && !mask_layer
            && filter.is_none()
        {
            self.items
                .insert(start, DisplayItem::PushBlend(BlendMode::Normal));
            self.items.push(DisplayItem::PopBlend);
        }
        if filter.is_some() {
            if empty {
                self.items.pop();
            } else {
                self.items.push(DisplayItem::PopFilter);
            }
        }
        if empty && backdrop.is_none() {
            self.items.truncate(layers_start);
        } else {
            if mask_layer {
                self.items.push(DisplayItem::PopMask);
            }
            if opacity_layer {
                self.items.push(DisplayItem::PopOpacity);
            }
            if blend_layer {
                self.items.push(DisplayItem::PopBlend);
            }
        }
        if clip_path_layer {
            if matches!(
                self.items.last(),
                Some(DisplayItem::PushClip { .. } | DisplayItem::PushClipPath { .. })
            ) {
                self.items.pop();
            } else {
                self.items.push(DisplayItem::PopClip);
            }
        }
        if transform.is_some() && matches!(self.items.last(), Some(DisplayItem::PushTransform(_))) {
            self.items.pop();
        } else if transform.is_some() {
            self.items.push(DisplayItem::PopTransform);
        }
        self.cull = saved_cull;
        // A blending context makes its parent context an isolated group.
        self.blend_inside = saved_blend_inside || blend != BlendMode::Normal;
        self.restore_context(
            saved_clips,
            saved_emitted,
            saved_cb,
            saved_pending,
            saved_floats,
        );
    }

    fn restore_context(
        &mut self,
        clips: Vec<Clip>,
        emitted: Vec<Clip>,
        cb_len: usize,
        pending: BinaryHeap<Reverse<Deferred>>,
        floats: Vec<Deferred>,
    ) {
        self.floats = floats;
        self.pending = pending;
        self.cb_len = cb_len;
        self.emitted = emitted;
        self.set_clips(clips);
    }

    /// Paint a float, an atomic inline or a positioned box with `z-index:
    /// auto`: like a stacking context, but positioned descendants and child
    /// contexts go to the enclosing context's queue.
    fn paint_pseudo_context(&mut self, id: BoxId) {
        let positioned = self.tree.get(id).style.position != Position::Static;
        let saved_floats = std::mem::take(&mut self.floats);
        let saved_cb = self.cb_len;
        self.box_background(id);
        self.outline(id);
        let clipped = self.enter_clip(id);
        if positioned {
            self.cb_len = self.clips.len();
        }
        self.walk(id, Phase::Collect);
        self.walk(id, Phase::Blocks);
        self.paint_floats();
        self.box_content(id);
        self.walk(id, Phase::Content);
        self.leave_clip(clipped);
        self.cb_len = saved_cb;
        self.floats = saved_floats;
    }

    fn paint_deferred(&mut self, d: Deferred) {
        let saved_clips = self.set_clips(d.clips);
        let saved_cb = std::mem::replace(&mut self.cb_len, d.cb_len);
        if d.real {
            // Push the clips now: the context's opacity layer goes inside them.
            self.sync_clips();
            self.paint_stacking_context(d.id);
        } else {
            self.paint_pseudo_context(d.id);
        }
        self.cb_len = saved_cb;
        self.set_clips(saved_clips);
    }

    fn paint_floats(&mut self) {
        let floats = std::mem::take(&mut self.floats);
        for f in floats {
            self.paint_deferred(f);
        }
    }

    /// Visit the in-flow descendants of `parent` for one paint phase. Stops at
    /// floats, positioned boxes, stacking contexts and atomic inlines, which
    /// paint as (pseudo) stacking contexts.
    fn walk(&mut self, parent: BoxId, phase: Phase) {
        let tree = self.tree;
        let pb = tree.get(parent);
        let parent_is_ifc = matches!(pb.kind, BoxKind::Inline);
        let parent_is_flex_or_grid = matches!(pb.kind, BoxKind::Flex | BoxKind::Grid);
        for c in tree.children(parent) {
            let cb = tree.get(c);
            if cb.style.opacity <= 0.0 {
                continue;
            }
            match classify(cb, parent_is_flex_or_grid) {
                Layer::Deferred { z, real } => {
                    if phase == Phase::Collect {
                        let (clips, cb_len) = self.chain_for(cb.style.position);
                        self.pending.push(Reverse(Deferred {
                            z,
                            id: c,
                            real,
                            clips,
                            cb_len,
                        }));
                    }
                }
                Layer::Float => {
                    if phase == Phase::Collect {
                        self.floats.push(Deferred {
                            z: 0,
                            id: c,
                            real: false,
                            clips: self.clips.clone(),
                            cb_len: self.cb_len,
                        });
                    }
                }
                Layer::Flow if parent_is_ifc => {
                    // Atomic inline (inline-block, inline image): painted
                    // whole, in line order with the text.
                    if phase == Phase::Content {
                        self.paint_pseudo_context(c);
                    }
                }
                Layer::Flow => {
                    match phase {
                        Phase::Blocks => self.box_background(c),
                        Phase::Content => self.outline(c),
                        Phase::Collect => {}
                    }
                    let clipped = self.enter_clip(c);
                    if phase == Phase::Content {
                        self.box_content(c);
                    }
                    self.walk(c, phase);
                    self.leave_clip(clipped);
                }
            }
        }
    }

    /// The clip chain for a deferred box with this `position`.
    fn chain_for(&self, position: Position) -> (Vec<Clip>, usize) {
        match position {
            // The containing block is the nearest positioned ancestor: clips
            // of non-positioned boxes in between do not apply.
            Position::Absolute => (self.clips[..self.cb_len].to_vec(), self.cb_len),
            Position::Fixed => (Vec::new(), 0),
            _ => (self.clips.clone(), self.cb_len),
        }
    }

    /// Background and border of one box: outer shadows, background color,
    /// background image or gradient, inset shadows, border.
    fn box_background(&mut self, id: BoxId) {
        let b = self.tree.get(id);
        let s = &b.style;
        if s.visibility != Visibility::Visible {
            return;
        }
        let shadows = s.box_shadow.as_deref().unwrap_or(&[]);
        let reach = shadows
            .iter()
            .filter(|sh| !sh.inset)
            .map(|sh| sh.blur * 1.5 + sh.spread.max(0.0) + sh.offset_x.abs().max(sh.offset_y.abs()))
            .fold(0.0f32, f32::max);
        let ink = Rect {
            x: b.rect.x - reach,
            y: b.rect.y - reach,
            width: b.rect.width + 2.0 * reach,
            height: b.rect.height + 2.0 * reach,
        };
        if !self.in_view(&ink) {
            return;
        }
        let r = radii(b);
        // Front shadow is listed first: paint back to front.
        for sh in shadows
            .iter()
            .rev()
            .filter(|sh| !sh.inset && !sh.color.is_transparent())
        {
            self.push(DisplayItem::BoxShadow {
                rect: b.rect,
                radii: r,
                offset: (sh.offset_x, sh.offset_y),
                blur: sh.blur,
                spread: sh.spread,
                color: sh.color,
                inset: false,
            });
        }
        if Some(id) != self.canvas_source && !s.background_color.is_transparent() {
            self.push(DisplayItem::Rect {
                rect: b.rect,
                radii: r,
                color: s.background_color,
            });
        }
        self.background_layer(b, r);
        let inner = inner_radii(r, b.border);
        for sh in shadows
            .iter()
            .rev()
            .filter(|sh| sh.inset && !sh.color.is_transparent())
        {
            self.push(DisplayItem::BoxShadow {
                rect: b.padding_rect(),
                radii: inner,
                offset: (sh.offset_x, sh.offset_y),
                blur: sh.blur,
                spread: sh.spread,
                color: sh.color,
                inset: true,
            });
        }
        let w = b.border;
        if w.top + w.right + w.bottom + w.left > 0.0 {
            self.push(DisplayItem::Border {
                rect: b.rect,
                widths: w,
                colors: s.border_color,
                styles: s.border_style,
                radii: r,
            });
        }
    }

    /// The `background-image` layer: tiles positioned in the padding box,
    /// painted in the border box (CSS Backgrounds 3 defaults for
    /// `background-origin` and `background-clip`).
    fn background_layer(&mut self, b: &'a huginn_layout::LayoutBox, r: [f32; 4]) {
        let s = &b.style;
        let Some(layer) = &s.background_image else {
            return;
        };
        let area = b.padding_rect();
        let paint = b.rect;
        let intrinsic = match layer {
            BackgroundImage::Url(_) => match &b.background_image {
                Some(img) => Some((img.width() as f32, img.height() as f32)),
                None => return,
            },
            _ => None,
        };
        let (tw, th) = background::tile_size(s.background_size, area, intrinsic);
        if tw <= 0.0 || th <= 0.0 {
            return;
        }
        let (ox, oy) = background::tile_origin(s.background_position, area, (tw, th));
        let tile = Rect {
            x: ox,
            y: oy,
            width: tw,
            height: th,
        };
        let rounded_clip = r.iter().any(|&v| v > 0.0);
        if rounded_clip {
            self.push(DisplayItem::PushClip {
                rect: paint,
                radii: r,
            });
        }
        match layer {
            BackgroundImage::Url(_) => {
                if let Some(img) = &b.background_image {
                    self.push(DisplayItem::TiledImage {
                        clip: paint,
                        origin: tile,
                        image: img,
                        repeat: s.background_repeat,
                    });
                }
            }
            BackgroundImage::LinearGradient(g) | BackgroundImage::RadialGradient(g) => {
                self.gradient_tiles(g, tile, paint, s.background_repeat);
            }
        }
        if rounded_clip {
            self.items.push(DisplayItem::PopClip);
        }
    }

    /// Gradient tiles covering `paint`, repeated like an image of size `tile`.
    fn gradient_tiles(&mut self, g: &Gradient, tile: Rect, paint: Rect, repeat: BackgroundRepeat) {
        let (rx, ry) = match repeat {
            BackgroundRepeat::Repeat => (true, true),
            BackgroundRepeat::RepeatX => (true, false),
            BackgroundRepeat::RepeatY => (false, true),
            BackgroundRepeat::NoRepeat => (false, false),
        };
        let span = |repeats: bool, start: f32, size: f32, lo: f32, hi: f32| -> (i32, i32) {
            if repeats {
                (
                    ((lo - start) / size).floor() as i32,
                    ((hi - start) / size).ceil() as i32,
                )
            } else {
                (0, 1)
            }
        };
        let (i0, i1) = span(rx, tile.x, tile.width, paint.x, paint.right());
        let (j0, j1) = span(ry, tile.y, tile.height, paint.y, paint.bottom());
        // Tiny tiles over a big area: one gradient over the whole area is a
        // better approximation than thousands of items.
        let too_many = i64::from(i1 - i0) * i64::from(j1 - j0) > 256;
        let Some((geom, stops, repeating)) =
            background::resolve_gradient(g, if too_many { paint } else { tile })
        else {
            return;
        };
        let stops: Box<[(f32, Color)]> = stops.into_boxed_slice();
        if too_many {
            self.push(DisplayItem::Gradient {
                clip: paint,
                gradient: geom,
                stops,
                repeating,
            });
            return;
        }
        for j in j0..j1 {
            for i in i0..i1 {
                let (dx, dy) = (i as f32 * tile.width, j as f32 * tile.height);
                let t = Rect {
                    x: tile.x + dx,
                    y: tile.y + dy,
                    width: tile.width,
                    height: tile.height,
                };
                let Some(clip) = intersect(t, paint) else {
                    continue;
                };
                if !self.in_view(&clip) {
                    continue;
                }
                self.push(DisplayItem::Gradient {
                    clip,
                    gradient: geom.translated(dx, dy),
                    stops: stops.clone(),
                    repeating,
                });
            }
        }
    }

    /// `outline`, outside the border box (follows the border radius, like Chrome).
    fn outline(&mut self, id: BoxId) {
        let b = self.tree.get(id);
        let s = &b.style;
        let w = s.outline_width;
        if w <= 0.0
            || !s.outline_style.is_visible()
            || s.outline_color.is_transparent()
            || s.visibility != Visibility::Visible
        {
            return;
        }
        let grow = s.outline_offset + w;
        let rect = Rect {
            x: b.rect.x - grow,
            y: b.rect.y - grow,
            width: b.rect.width + 2.0 * grow,
            height: b.rect.height + 2.0 * grow,
        };
        if rect.width <= 0.0 || rect.height <= 0.0 || !self.in_view(&rect) {
            return;
        }
        let r = radii(b).map(|v| if v > 0.0 { (v + grow).max(0.0) } else { 0.0 });
        self.push(DisplayItem::Border {
            rect,
            widths: Edges::all(w),
            colors: Edges::all(s.outline_color),
            styles: Edges::all(s.outline_style),
            radii: r,
        });
    }

    /// Foreground of one box: replaced content, text and text decorations.
    fn box_content(&mut self, id: BoxId) {
        let tree = self.tree;
        let b = tree.get(id);
        if b.style.visibility != Visibility::Visible {
            return;
        }
        if let BoxKind::Image(img) = &b.kind
            && self.in_view(&b.rect)
        {
            let content = b.content_rect();
            let fitted = background::object_fit_rect(
                content,
                img.width() as f32,
                img.height() as f32,
                b.style.object_fit,
            );
            // Replaced content is clipped to the content box, with the
            // corner curves of the border radius (CSS Backgrounds 3, 5.3).
            let r = inner_radii(
                radii(b),
                Edges {
                    top: b.border.top + b.padding.top,
                    right: b.border.right + b.padding.right,
                    bottom: b.border.bottom + b.padding.bottom,
                    left: b.border.left + b.padding.left,
                },
            );
            let overflows = fitted.x < content.x - 0.01
                || fitted.y < content.y - 0.01
                || fitted.right() > content.right() + 0.01
                || fitted.bottom() > content.bottom() + 0.01;
            let clip = overflows || r.iter().any(|&v| v > 0.0);
            if clip {
                self.push(DisplayItem::PushClip {
                    rect: content,
                    radii: r,
                });
            }
            self.push(DisplayItem::Image {
                rect: fitted,
                image: img,
            });
            if clip {
                self.items.push(DisplayItem::PopClip);
            }
        }
        // Inline element boxes of this inline formatting context, one piece
        // per line, under the text (CSS 2.1 Appendix E, 7.2.1).
        let frags =
            &tree.fragments[b.inline_fragments.start as usize..b.inline_fragments.end as usize];
        for f in frags {
            self.fragment_background(f);
        }
        for run in tree.text_runs(id) {
            if let Some(c) = self.cull {
                // Glyphs reach at most about one em above and below the baseline.
                let em = run.font_size * 1.5;
                if run.baseline + em < c.y || run.baseline - em > c.bottom() {
                    continue;
                }
            }
            self.push(DisplayItem::Glyphs {
                run,
                glyphs: tree.glyphs(run),
                coords: tree.coords(run),
            });
        }
        for d in tree.decorations(id) {
            if !self.in_view(&d.rect) {
                continue;
            }
            let round = [d.rect.width.min(d.rect.height) / 2.0; 4];
            self.push(match d.shape {
                DecorationShape::Rect => DisplayItem::Rect {
                    rect: d.rect,
                    radii: [0.0; 4],
                    color: d.color,
                },
                DecorationShape::Ellipse => DisplayItem::Rect {
                    rect: d.rect,
                    radii: round,
                    color: d.color,
                },
                // A 1px stroke centred on the ellipse's edge.
                DecorationShape::EllipseOutline => DisplayItem::Border {
                    rect: Rect {
                        x: d.rect.x - 0.5,
                        y: d.rect.y - 0.5,
                        width: d.rect.width + 1.0,
                        height: d.rect.height + 1.0,
                    },
                    widths: Edges::all(1.0),
                    colors: Edges::all(d.color),
                    styles: Edges::all(BorderStyle::Solid),
                    radii: [d.rect.width.min(d.rect.height) / 2.0 + 0.5; 4],
                },
            });
        }
    }

    /// Background color and border of one line's piece of an inline element.
    /// A box split over lines has its start edge only on the first piece and
    /// its end edge only on the last (`box-decoration-break: slice`).
    fn fragment_background(&mut self, f: &huginn_layout::InlineFragment) {
        let s = &f.style;
        if s.visibility != Visibility::Visible || f.rect.width <= 0.0 || !self.in_view(&f.rect) {
            return;
        }
        let mut widths = s.border_width;
        if !f.first {
            widths.left = 0.0;
        }
        if !f.last {
            widths.right = 0.0;
        }
        let has_border = widths.top + widths.right + widths.bottom + widths.left > 0.0;
        if s.background_color.is_transparent() && !has_border {
            return;
        }
        let r = &s.border_radius;
        let max = f.rect.width.min(f.rect.height) / 2.0;
        let mut radii = [r.top_left, r.top_right, r.bottom_right, r.bottom_left]
            .map(|v| v.resolve(Some(f.rect.width)).min(max).max(0.0));
        if !f.first {
            radii[0] = 0.0;
            radii[3] = 0.0;
        }
        if !f.last {
            radii[1] = 0.0;
            radii[2] = 0.0;
        }
        // `opacity` of the element and its inline ancestors, as alpha.
        let fade = |c: Color| {
            if f.opacity < 1.0 {
                c.with_opacity(f.opacity)
            } else {
                c
            }
        };
        if !s.background_color.is_transparent() {
            self.push(DisplayItem::Rect {
                rect: f.rect,
                radii,
                color: fade(s.background_color),
            });
        }
        if has_border {
            self.push(DisplayItem::Border {
                rect: f.rect,
                widths,
                colors: s.border_color.map(fade),
                styles: s.border_style,
                radii,
            });
        }
    }
}

/// Straight-alpha RGBA8 pixels, row-major.
#[derive(Debug, Clone)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Turns display lists into pixels.
pub trait Rasterizer {
    /// Render `region` (document coordinates, CSS px) of `list` at `scale`
    /// device pixels per CSS px. Output size is `region` size times `scale`.
    fn rasterize(&mut self, list: &DisplayList<'_>, region: Rect, scale: f32) -> RgbaImage;
}

/// CPU rasterizer on vello_cpu.
pub struct CpuRasterizer {
    resources: vello_cpu::Resources,
}

impl Default for CpuRasterizer {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuRasterizer {
    pub fn new() -> CpuRasterizer {
        CpuRasterizer {
            resources: vello_cpu::Resources::new(),
        }
    }
}

mod raster;

impl Rasterizer for CpuRasterizer {
    fn rasterize(&mut self, list: &DisplayList<'_>, region: Rect, scale: f32) -> RgbaImage {
        raster::rasterize(&mut self.resources, list, region, scale)
    }
}

/// Encode as PNG. Speed matters more than size here: `Fastest` is the `Up`
/// filter plus fdeflate's ultra-fast mode (`Fast` tries every filter on every row).
pub fn encode_png(img: &RgbaImage, out: impl Write) -> Result<(), png::EncodingError> {
    let mut enc = png::Encoder::new(out, img.width, img.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fastest);
    let mut w = enc.write_header()?;
    w.write_image_data(&img.data)?;
    w.finish()
}
