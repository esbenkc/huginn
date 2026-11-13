//! The look of Huginn's own UI, "Forest Mono": square, dark green, monospace,
//! one mint accent. Colours, the font stack and small label styles live here
//! so the tab strip, toolbar, menus, find bar and internal pages agree.
//!
//! On macOS the chrome is also a clear-acrylic material: the window is see-
//! through to a blurred desktop (`glass.rs`), the bars are translucent slabs
//! with lit edges, and faint "electronics" (circuit traces, chips, ribbon
//! wires) sit inside them. The electronics are a static drawing seeded by the
//! window size, rendered once per size. `HUGINN_OPAQUE_UI=1` turns the
//! material off; headless runs and other platforms never use it.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

use huginn_net::{DecodedImage, Url};

pub const STRIP: &str = "#07110d";
pub const TOOLBAR: &str = "#0b1a14";
pub const ACTIVE: &str = "#0f241b";
pub const HAIRLINE: &str = "#1c3a2c";
pub const HOVER: &str = "#13301f";
pub const TEXT: &str = "#e3f2ea";
pub const MUTED: &str = "#7c9c8c";
/// Disabled controls.
pub const DIM: &str = "#3d5a4c";
pub const MINT: &str = "#2de39a";
/// The accent of private windows.
pub const COOL: &str = "#8fb3ff";
pub const WARN: &str = "#ff6b5a";
/// Local fonts only: the first one installed wins, Menlo is always there.
pub const FONT: &str = "'PP Neue Montreal Mono','ABC Diatype Mono','ABC Diatype Mono Trial',\
                        'JetBrains Mono','SF Mono',Menlo,monospace";
/// Small labels: uppercase, slightly tracked.
pub const LABEL: &str = "text-transform:uppercase;letter-spacing:0.06em";

pub fn accent(private: bool) -> &'static str {
    if private { COOL } else { MINT }
}

/// The accent as 0xRRGGBB, for drawing on the frame.
pub fn accent_rgb(private: bool) -> u32 {
    if private { 0x008F_B3FF } else { 0x002D_E39A }
}

/// Selected text in the address bar and find bar: the accent at 30%.
pub fn selection(private: bool) -> &'static str {
    if private {
        "rgba(143,179,255,0.30)"
    } else {
        "rgba(45,227,154,0.30)"
    }
}

static MATERIAL: AtomicBool = AtomicBool::new(false);

/// Turn the acrylic material on (macOS windows, unless `HUGINN_OPAQUE_UI=1`).
pub fn set_material(on: bool) {
    MATERIAL.store(on, Ordering::Relaxed);
}

/// The chrome is translucent over a blurred desktop.
pub fn material() -> bool {
    MATERIAL.load(Ordering::Relaxed)
}

/// A bar or panel background: `hex` itself, or `hex` at `alpha` over the blur.
pub fn fill(hex: &str, alpha: f32) -> String {
    if !material() {
        return hex.to_owned();
    }
    let v = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
    format!("rgba({},{},{},{alpha})", v >> 16, (v >> 8) & 0xFF, v & 0xFF)
}

/// URL of the chrome backdrop in the strip's and toolbar's image stores.
pub fn backdrop_url() -> Url {
    Url::parse("huginn-backdrop:chrome").expect("valid URL")
}

/// Height of the backdrop: the tab strip and the toolbar are one slab.
pub const BACKDROP_HEIGHT: f32 = 80.0;

thread_local! {
    /// Backdrops by device size; a few windows of different sizes share it.
    static BACKDROPS: RefCell<Vec<((u32, u32), DecodedImage)>> = const { RefCell::new(Vec::new()) };
}

/// The acrylic slab behind the tab strip and toolbar, `width` CSS px wide
/// and [`BACKDROP_HEIGHT`] tall, rendered at `scale`. `None` without the
/// material. Rendered once per window size.
pub fn backdrop(width: f32, scale: f32) -> Option<DecodedImage> {
    if !material() {
        return None;
    }
    let size = (
        (width * scale).round().max(1.0) as u32,
        (BACKDROP_HEIGHT * scale).round().max(1.0) as u32,
    );
    BACKDROPS.with_borrow_mut(|cache| {
        if let Some((_, img)) = cache.iter().find(|(s, _)| *s == size) {
            return Some(img.clone());
        }
        let svg = backdrop_svg(
            width,
            BACKDROP_HEIGHT,
            u64::from(size.0) << 32 | u64::from(size.1),
        );
        let img = huginn_net::decode_image(svg.as_bytes(), Some(size)).ok()?;
        if cache.len() >= 4 {
            cache.remove(0);
        }
        cache.push((size, img.clone()));
        Some(img)
    })
}

/// The chrome slab: strip and toolbar tints, a glassy sheen, the
/// electronics, a faint inner hairline, a bright top edge, a dark bottom edge.
fn backdrop_svg(w: f32, h: f32, seed: u64) -> String {
    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w} {h}\" width=\"{w}\" height=\"{h}\">\
         <defs><linearGradient id=\"g\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\">\
         <stop offset=\"0\" stop-color=\"#fff\" stop-opacity=\".07\"/>\
         <stop offset=\".5\" stop-color=\"#fff\" stop-opacity=\".02\"/>\
         <stop offset=\"1\" stop-color=\"#fff\" stop-opacity=\".04\"/></linearGradient>\
         <linearGradient id=\"s\" x1=\"0\" y1=\"0\" x2=\"1\" y2=\"0\">\
         <stop offset=\"0\" stop-color=\"#fff\" stop-opacity=\".25\"/>\
         <stop offset=\".3\" stop-color=\"#fff\" stop-opacity=\".7\"/>\
         <stop offset=\".75\" stop-color=\"#fff\" stop-opacity=\".5\"/>\
         <stop offset=\"1\" stop-color=\"#fff\" stop-opacity=\".2\"/></linearGradient></defs>\
         <rect width=\"{w}\" height=\"{half}\" fill=\"{STRIP}\" fill-opacity=\".58\"/>\
         <rect y=\"{half}\" width=\"{w}\" height=\"{half}\" fill=\"{TOOLBAR}\" fill-opacity=\".52\"/>\
         <rect width=\"{w}\" height=\"{h}\" fill=\"url(#g)\"/>",
        half = h / 2.0
    );
    // Chips stay in the strip's right part, away from the address text.
    circuitry(&mut s, w, h, seed, 1.0, [0.45, 0.97, 0.0, 0.5]);
    let _ = write!(
        s,
        "<rect x=\"1.5\" y=\"1.5\" width=\"{}\" height=\"{}\" fill=\"none\" stroke=\"#fff\" stroke-opacity=\".12\"/>\
         <rect width=\"{w}\" height=\"1\" fill=\"url(#s)\"/>\
         <rect width=\"1\" height=\"{h}\" fill=\"#fff\" fill-opacity=\".22\"/>\
         <rect y=\"{}\" width=\"{w}\" height=\"1\" fill=\"#000\" fill-opacity=\".25\"/>\
         <rect y=\"{}\" width=\"{w}\" height=\"1\" fill=\"#000\" fill-opacity=\".45\"/></svg>",
        w - 3.0,
        h - 3.0,
        h - 2.0,
        h - 1.0
    );
    s
}

/// SplitMix64: a tiny deterministic generator, so a seed always draws the
/// same picture.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `a..b`.
    fn range(&mut self, a: f32, b: f32) -> f32 {
        a + (self.next() >> 40) as f32 / (1u64 << 24) as f32 * (b - a)
    }

    fn coin(&mut self) -> bool {
        self.next() & 1 == 1
    }
}

/// Faint electronics inside a `w` x `h` slab, as SVG elements appended to
/// `out`: circuit traces with 45 degree bends ending in via rings, a few chip
/// packages with pin rows, grey ribbon wires, one chartreuse part and one red
/// LED. `k` scales the parts (1 in the chrome, larger on the new tab page);
/// chips sit in `area` (x0, x1, y0, y1 as fractions of the slab).
pub fn circuitry(out: &mut String, w: f32, h: f32, seed: u64, k: f32, area: [f32; 4]) {
    let mut r = Rng(seed ^ 0x4855_4749_4E4E);
    let (lo, hi) = (3.0 * k, h - 3.0 * k);
    // Traces: horizontal runs and 45 degree diagonals, clamped to the slab.
    let traces = ((w * h) / (2400.0 * k * k)).clamp(6.0, 70.0) as usize;
    for _ in 0..traces {
        let (mut x, mut y) = (r.range(0.0, w), r.range(lo, hi));
        let _ = write!(out, "<polyline points=\"{x:.1},{y:.1}");
        for seg in 0..3 {
            if seg % 2 == 0 {
                x += r.range(8.0, 70.0) * k * if r.coin() { 1.0 } else { -1.0 };
            } else {
                let d = r.range(4.0, 16.0) * k;
                let dy = if (r.coin() && y + d <= hi) || y - d < lo {
                    d
                } else {
                    -d
                };
                x += d * if r.coin() { 1.0 } else { -1.0 };
                y += dy;
            }
            let _ = write!(out, " {x:.1},{y:.1}");
        }
        let _ = write!(
            out,
            "\" fill=\"none\" stroke=\"#9fb3a9\" stroke-opacity=\".2\" stroke-width=\"{:.2}\"/>\
             <circle cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"{:.2}\" fill=\"none\" stroke=\"#c9d8d0\" \
             stroke-opacity=\".3\" stroke-width=\"{:.2}\"/>",
            0.8 * k,
            1.6 * k,
            0.6 * k
        );
    }
    // Ribbon wires: four parallel strands on a gentle S curve.
    for _ in 0..2 {
        let x0 = r.range(-0.1, 0.5) * w;
        let x1 = x0 + r.range(0.3, 0.6) * w;
        let (y0, y1) = (r.range(lo, hi - 6.0 * k), r.range(lo, hi - 6.0 * k));
        let bend = r.range(10.0, 24.0) * k;
        for i in 0..4 {
            let o = i as f32 * 1.6 * k;
            let _ = write!(
                out,
                "<path d=\"M{x0:.1} {:.1} C {:.1} {:.1}, {:.1} {:.1}, {x1:.1} {:.1}\" fill=\"none\" \
                 stroke=\"#aab4ae\" stroke-opacity=\".22\" stroke-width=\"{:.2}\"/>",
                y0 + o,
                x0 + (x1 - x0) * 0.35,
                y0 + o - bend,
                x0 + (x1 - x0) * 0.65,
                y1 + o + bend,
                y1 + o,
                0.7 * k
            );
        }
    }
    // Chips with pin rows; the accents sit on the first two.
    let chips = 3;
    for c in 0..chips {
        let cw = r.range(18.0, 34.0) * k;
        let ch = r.range(8.0, 12.0) * k;
        let band = (area[1] - area[0]) * w / chips as f32;
        let x = area[0] * w + band * c as f32 + r.range(0.0, (band - cw).max(0.0));
        let top = (area[2] * h).max(lo) + 2.0 * k;
        let y = r.range(top, (area[3] * h).min(hi) - ch - 2.0 * k).max(top);
        let _ = write!(
            out,
            "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{cw:.1}\" height=\"{ch:.1}\" fill=\"#0e1512\" \
             fill-opacity=\".5\" stroke=\"#3a4a43\" stroke-opacity=\".8\" stroke-width=\"{:.2}\"/>",
            0.6 * k
        );
        let mut px = x + 1.5 * k;
        while px + k <= x + cw - k {
            for py in [y - 1.6 * k, y + ch] {
                let _ = write!(
                    out,
                    "<rect x=\"{px:.1}\" y=\"{py:.1}\" width=\"{:.2}\" height=\"{:.2}\" fill=\"#8a9a92\" fill-opacity=\".45\"/>",
                    0.9 * k,
                    1.6 * k
                );
            }
            px += 3.0 * k;
        }
        match c {
            0 => {
                let _ = write!(
                    out,
                    "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" fill=\"#b8e94a\" fill-opacity=\".85\"/>",
                    x + cw + 4.0 * k,
                    y + ch / 2.0 - 1.1 * k,
                    5.0 * k,
                    2.2 * k
                );
            }
            1 => {
                let (lx, ly) = (x + cw - 2.5 * k, y + 2.5 * k);
                let _ = write!(
                    out,
                    "<circle cx=\"{lx:.1}\" cy=\"{ly:.1}\" r=\"{:.2}\" fill=\"#ff3b3b\" fill-opacity=\".14\"/>\
                     <circle cx=\"{lx:.1}\" cy=\"{ly:.1}\" r=\"{:.2}\" fill=\"#ff3b3b\" fill-opacity=\".9\"/>",
                    3.2 * k,
                    1.2 * k
                );
            }
            _ => {}
        }
    }
}

/// A mint asterisk (three bars), `size` px, as inline SVG for pages.
pub fn asterisk_svg(size: u32, color: &str) -> String {
    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\" width=\"{size}\" height=\"{size}\">"
    );
    for deg in [0, 60, 120] {
        let _ = write!(
            s,
            "<rect x=\"42\" y=\"4\" width=\"16\" height=\"92\" fill=\"{color}\" transform=\"rotate({deg} 50 50)\"/>"
        );
    }
    s.push_str("</svg>");
    s
}

/// The signature flock: six small birds as white strokes.
pub fn flock_svg(width: u32) -> String {
    const BIRDS: [(f32, f32, f32); 6] = [
        (8.0, 30.0, 1.0),
        (34.0, 14.0, 0.8),
        (58.0, 34.0, 1.15),
        (86.0, 8.0, 0.7),
        (104.0, 26.0, 0.9),
        (132.0, 16.0, 0.6),
    ];
    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 150 50\" width=\"{width}\" height=\"{}\">",
        width / 3
    );
    for (x, y, k) in BIRDS {
        let _ = write!(
            s,
            "<path d=\"M{x} {y} q{a:.1} {b:.1} {c:.1} {d:.1} q{a:.1} {b:.1} {c:.1} {e:.1}\" fill=\"none\" \
             stroke=\"#fff\" stroke-opacity=\".85\" stroke-width=\"1.3\" stroke-linecap=\"square\"/>",
            a = 4.0 * k,
            b = -4.0 * k,
            c = 8.0 * k,
            d = 1.0 * k,
            e = -1.0 * k,
        );
    }
    s.push_str("</svg>");
    s
}

/// A clear-acrylic slab `w` x `h` with electronics inside, as inline SVG for
/// the new tab page (modelled on the app icon).
pub fn slab_svg(w: u32, h: u32, seed: u64) -> String {
    let (wf, hf) = (w as f32, h as f32);
    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w} {h}\" width=\"{w}\" height=\"{h}\">\
         <defs><linearGradient id=\"glass\" x1=\"0\" y1=\"0\" x2=\"1\" y2=\"1\">\
         <stop offset=\"0\" stop-color=\"#fff\" stop-opacity=\".14\"/>\
         <stop offset=\".45\" stop-color=\"#fff\" stop-opacity=\".04\"/>\
         <stop offset=\"1\" stop-color=\"#fff\" stop-opacity=\".09\"/></linearGradient>\
         <linearGradient id=\"spec\" x1=\"0\" y1=\"0\" x2=\"1\" y2=\"0\">\
         <stop offset=\"0\" stop-color=\"#fff\" stop-opacity=\"0\"/>\
         <stop offset=\".3\" stop-color=\"#fff\" stop-opacity=\".9\"/>\
         <stop offset=\".7\" stop-color=\"#fff\" stop-opacity=\".6\"/>\
         <stop offset=\"1\" stop-color=\"#fff\" stop-opacity=\"0\"/></linearGradient></defs>\
         <rect width=\"{w}\" height=\"{h}\" fill=\"{STRIP}\" fill-opacity=\".5\"/>"
    );
    // Chips below the wordmark, so they never sit behind text.
    circuitry(&mut s, wf, hf, seed, 2.2, [0.06, 0.94, 0.56, 0.97]);
    let _ = write!(
        s,
        "<rect width=\"{w}\" height=\"{h}\" fill=\"url(#glass)\" stroke=\"#e8f2ee\" stroke-opacity=\".45\" stroke-width=\"1.5\"/>\
         <rect x=\"5\" y=\"5\" width=\"{}\" height=\"{}\" fill=\"none\" stroke=\"#fff\" stroke-opacity=\".12\" stroke-width=\"1.2\"/>\
         <rect x=\"6\" y=\"1\" width=\"{}\" height=\"2\" fill=\"url(#spec)\"/>\
         <rect x=\"1\" y=\"12\" width=\"2\" height=\"{}\" fill=\"#fff\" fill-opacity=\".3\"/>\
         <path d=\"M0 {} H{} V0\" fill=\"none\" stroke=\"#000\" stroke-opacity=\".5\" stroke-width=\"2\"/></svg>",
        wf - 10.0,
        hf - 10.0,
        wf - 12.0,
        hf * 0.6,
        hf - 1.0,
        wf - 1.0
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn electronics_are_the_same_for_a_size_and_differ_between_sizes() {
        let draw = |seed| {
            let mut s = String::new();
            circuitry(&mut s, 1280.0, 80.0, seed, 1.0, [0.1, 0.9, 0.0, 1.0]);
            s
        };
        assert_eq!(draw(7), draw(7));
        assert_ne!(draw(7), draw(8));
    }

    #[test]
    fn traces_stay_inside_the_slab() {
        let mut s = String::new();
        circuitry(&mut s, 900.0, 80.0, 42, 1.0, [0.1, 0.9, 0.0, 1.0]);
        for poly in s.split("points=\"").skip(1) {
            let pts = poly.split('"').next().unwrap();
            for p in pts.split(' ') {
                let y: f32 = p.split(',').nth(1).unwrap().parse().unwrap();
                assert!((2.9..=77.1).contains(&y), "{y} in {pts}");
            }
        }
    }
}
