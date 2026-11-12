//! Drawing helpers for the window: device pixel buffers, small HTML snippets
//! rendered by our own pipeline (tab strip, toolbar, status bubble, find bar,
//! menus), and a [`Canvas`] over the composed frame for overlays.

use huginn_layout::{LayoutEngine, LayoutInput, LayoutTree, Rect, Size};
use huginn_net::{ImageStore, Url};
use huginn_paint::{CpuRasterizer, Rasterizer, RgbaImage};
use huginn_style::{StyleEngine, Viewport};

/// Pixels in the window's format: 0RGB `u32`, stored little-endian as
/// (B, G, R, 0) bytes, so copying a row into the frame is a plain load.
pub(crate) struct Pixels {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl Pixels {
    /// Converts in place: no second buffer.
    pub fn from_rgba(img: RgbaImage) -> Pixels {
        let mut data = img.data;
        for p in data.chunks_exact_mut(4) {
            p.swap(0, 2);
            p[3] = 0;
        }
        Pixels {
            width: img.width as usize,
            height: img.height as usize,
            data,
        }
    }

    pub fn row(&self, y: usize) -> &[u8] {
        &self.data[y * self.width * 4..(y + 1) * self.width * 4]
    }

    /// Copy into `frame` (row length `stride`) with the top-left corner at (`x`, `y`), clipped.
    pub fn blit(&self, frame: &mut [u32], stride: usize, x: usize, y: usize) {
        let rows = frame.len() / stride;
        for r in 0..self.height.min(rows.saturating_sub(y)) {
            let dst = &mut frame[(y + r) * stride..(y + r + 1) * stride];
            copy_row(&mut dst[x.min(stride)..], self.row(r));
        }
    }
}

/// `src` is (B, G, R, 0) bytes. Pixels past the end of `src` stay as they are.
pub(crate) fn copy_row(dst: &mut [u32], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src.chunks_exact(4)) {
        *d = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
    }
}

/// Lay out a small HTML document. Returns the tree and the border boxes of
/// the elements with the given ids (`Rect::default()` for missing ones).
/// `images` resolves `<img src>` (e.g. favicons under made-up URLs).
pub(crate) fn layout_snippet(
    layout: &mut LayoutEngine,
    html: &str,
    width: f32,
    height: f32,
    ids: &[&str],
    images: &ImageStore,
) -> (LayoutTree, Vec<Rect>) {
    let doc = huginn_dom::parse_html_bytes(html.as_bytes());
    let styles = StyleEngine::new(Viewport {
        width,
        height,
        device_pixel_ratio: 1.0,
    })
    .compute(&doc);
    let base = Url::parse("about:blank").expect("valid URL");
    let tree = layout.layout(&LayoutInput {
        document: &doc,
        styles: &styles,
        images,
        base_url: &base,
        viewport: Size { width, height },
    });
    let rects = ids
        .iter()
        .map(|id| {
            doc.element_by_id(id)
                .and_then(|n| tree.box_for_node(n))
                .map_or(Rect::default(), |b| tree.get(b).rect)
        })
        .collect();
    (tree, rects)
}

/// Lay out and rasterize a snippet into `width` x `height` CSS px at `scale`.
pub(crate) fn render_snippet(
    layout: &mut LayoutEngine,
    raster: &mut CpuRasterizer,
    html: &str,
    width: f32,
    height: f32,
    ids: &[&str],
    images: &ImageStore,
    scale: f32,
) -> (Pixels, Vec<Rect>) {
    let (tree, rects) = layout_snippet(layout, html, width, height, ids, images);
    let region = Rect {
        x: 0.0,
        y: 0.0,
        width,
        height,
    };
    let list = huginn_paint::build_display_list(&tree, Some(region));
    (
        Pixels::from_rgba(raster.rasterize(&list, region, scale)),
        rects,
    )
}

/// The composed frame, for overlays. Coordinates are window CSS px unless a
/// method says otherwise; drawing is clipped to `clip` (device px).
pub(crate) struct Canvas<'a> {
    pub frame: &'a mut [u32],
    /// Frame size in device px.
    pub width: usize,
    pub height: usize,
    /// Device px per CSS px of the window (not including page zoom).
    pub scale: f32,
    /// Device px rectangle drawing is clipped to: (x0, y0, x1, y1).
    pub clip: (usize, usize, usize, usize),
}

impl Canvas<'_> {
    /// Window CSS rect to clipped device px (x0, y0, x1, y1).
    fn device(&self, r: Rect) -> (usize, usize, usize, usize) {
        let d = |v: f32| (v * self.scale).round().max(0.0) as usize;
        let (cx0, cy0, cx1, cy1) = self.clip;
        (
            d(r.x).clamp(cx0, cx1),
            d(r.y).clamp(cy0, cy1),
            d(r.right()).clamp(cx0, cx1),
            d(r.bottom()).clamp(cy0, cy1),
        )
    }

    /// Fill `r` with an opaque 0xRRGGBB color.
    pub fn fill(&mut self, r: Rect, rgb: u32) {
        let (x0, y0, x1, y1) = self.device(r);
        for y in y0..y1 {
            self.frame[y * self.width + x0..y * self.width + x1].fill(rgb & 0x00FF_FFFF);
        }
    }

    /// Blend 0xRRGGBB over `r` with `alpha` in 0..=255.
    pub fn blend(&mut self, r: Rect, rgb: u32, alpha: u8) {
        let (x0, y0, x1, y1) = self.device(r);
        let a = u32::from(alpha);
        for y in y0..y1 {
            for p in &mut self.frame[y * self.width + x0..y * self.width + x1] {
                *p = blend(*p, rgb, a);
            }
        }
    }

    /// Multiply 0xRRGGBB into `r`: a highlight under the text (white
    /// becomes the color, dark text stays dark). Selection and find use it.
    pub fn multiply(&mut self, r: Rect, rgb: u32) {
        let (x0, y0, x1, y1) = self.device(r);
        let c = |shift: u32| (rgb >> shift) & 0xFF;
        let (cr, cg, cb) = (c(16), c(8), c(0));
        for y in y0..y1 {
            for p in &mut self.frame[y * self.width + x0..y * self.width + x1] {
                let d = |shift: u32| (*p >> shift) & 0xFF;
                *p = ((d(16) * cr / 255) << 16) | ((d(8) * cg / 255) << 8) | (d(0) * cb / 255);
            }
        }
    }

    /// Stroke the outline of `r`, `w` CSS px wide.
    pub fn outline(&mut self, r: Rect, w: f32, rgb: u32) {
        let edge = |x, y, width, height| Rect {
            x,
            y,
            width,
            height,
        };
        self.fill(edge(r.x, r.y, r.width, w), rgb);
        self.fill(edge(r.x, r.bottom() - w, r.width, w), rgb);
        self.fill(edge(r.x, r.y, w, r.height), rgb);
        self.fill(edge(r.right() - w, r.y, w, r.height), rgb);
    }

    /// Copy `px` with its top-left at window CSS (`x`, `y`), clipped.
    pub fn blit(&mut self, px: &Pixels, x: f32, y: f32) {
        let (x0, y0) = (
            (x * self.scale).round() as isize,
            (y * self.scale).round() as isize,
        );
        let (cx0, cy0, cx1, cy1) = self.clip;
        for r in 0..px.height {
            let fy = y0 + r as isize;
            if fy < cy0 as isize || fy >= cy1 as isize {
                continue;
            }
            let src = px.row(r);
            for c in 0..px.width {
                let fx = x0 + c as isize;
                if fx < cx0 as isize || fx >= cx1 as isize {
                    continue;
                }
                let s = &src[c * 4..c * 4 + 4];
                self.frame[fy as usize * self.width + fx as usize] =
                    u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
            }
        }
    }
}

/// `dst` 0RGB blended with `rgb` at `a`/255.
pub(crate) fn blend(dst: u32, rgb: u32, a: u32) -> u32 {
    let mix = |shift: u32| {
        let d = (dst >> shift) & 0xFF;
        let s = (rgb >> shift) & 0xFF;
        ((s * a + d * (255 - a)) / 255) << shift
    };
    mix(16) | mix(8) | mix(0)
}
