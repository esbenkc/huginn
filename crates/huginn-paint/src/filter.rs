//! CSS filter functions (Filter Effects 1) on premultiplied RGBA8 pixmaps.
//!
//! Like Chrome, the functions work on sRGB values (no linear-light
//! conversion). Color functions work on unpremultiplied colors and clamp
//! after each function. `blur()` is a Gaussian: a true kernel below a
//! standard deviation of 2 device px, else three box blurs (the spec's
//! approximation, also Skia's on the CPU).

use huginn_style::{Color, FilterFunction};
use vello_cpu::Pixmap;
use vello_cpu::peniko::color::PremulRgba8;

/// What a blur reads past the edges of the pixmap.
#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) enum Edge {
    /// Transparent black (`filter`).
    Transparent,
    /// The pixmap reflected at its edges (`backdrop-filter`, Filter Effects 2).
    Mirror,
}

/// How far (CSS px) `filters` move ink: the output reaches this far past
/// the input, and an output pixel reads input up to this far away.
pub(crate) fn reach(filters: &[FilterFunction]) -> f32 {
    filters
        .iter()
        .filter(|f| !f.is_identity())
        .map(|f| match *f {
            FilterFunction::Blur(sigma) => 3.0 * sigma,
            FilterFunction::DropShadow {
                offset_x,
                offset_y,
                blur,
                ..
            } => offset_x.abs().max(offset_y.abs()) + 3.0 * blur,
            _ => 0.0,
        })
        .sum()
}

/// Run `filters` over `pm`, left to right. `lin` is the linear part
/// `[a, b, c, d]` of the CSS px to device px transform: it scales blur
/// radii and turns shadow offsets.
pub(crate) fn apply(pm: &mut Pixmap, filters: &[FilterFunction], lin: [f64; 4], edge: Edge) {
    let [a, b, c, d] = lin;
    let sx = a.hypot(b) as f32;
    let sy = c.hypot(d) as f32;
    let mut i = 0;
    while i < filters.len() {
        let f = filters[i];
        if f.is_identity() {
            i += 1;
            continue;
        }
        match f {
            FilterFunction::Blur(sigma) => blur_rgba(pm, sigma * sx, sigma * sy, edge),
            FilterFunction::DropShadow {
                offset_x,
                offset_y,
                blur,
                color,
            } => {
                let (ox, oy) = (f64::from(offset_x), f64::from(offset_y));
                let dx = (a * ox + c * oy) as f32;
                let dy = (b * ox + d * oy) as f32;
                drop_shadow(pm, (dx, dy), (blur * sx, blur * sy), color);
            }
            _ => {
                // A run of color functions: one pass over the pixels.
                let end = filters[i..]
                    .iter()
                    .position(|f| {
                        matches!(
                            f,
                            FilterFunction::Blur(_) | FilterFunction::DropShadow { .. }
                        )
                    })
                    .map_or(filters.len(), |n| i + n);
                let ops: Vec<ColorOp> = filters[i..end]
                    .iter()
                    .filter(|f| !f.is_identity())
                    .filter_map(|f| ColorOp::new(*f))
                    .collect();
                color_ops(pm, &ops);
                i = end;
                continue;
            }
        }
        i += 1;
    }
}

/// One color function as `rgb' = m * rgb + offset`, `alpha' = alpha * opacity`.
struct ColorOp {
    m: [[f32; 3]; 3],
    offset: f32,
    opacity: f32,
}

impl ColorOp {
    fn new(f: FilterFunction) -> Option<ColorOp> {
        let diag = |k: f32, offset: f32| ColorOp {
            m: [[k, 0.0, 0.0], [0.0, k, 0.0], [0.0, 0.0, k]],
            offset,
            opacity: 1.0,
        };
        let matrix = |m: [[f32; 3]; 3]| ColorOp {
            m,
            offset: 0.0,
            opacity: 1.0,
        };
        // Matrices: Filter Effects 1, 13.x ("equivalent to" feColorMatrix
        // and feComponentTransfer).
        Some(match f {
            FilterFunction::Brightness(k) => diag(k, 0.0),
            FilterFunction::Contrast(k) => diag(k, 0.5 - 0.5 * k),
            FilterFunction::Invert(a) => diag(1.0 - 2.0 * a, a),
            FilterFunction::Opacity(a) => ColorOp {
                opacity: a,
                ..diag(1.0, 0.0)
            },
            FilterFunction::Grayscale(a) => {
                let k = 1.0 - a;
                matrix([
                    [
                        0.2126 + 0.7874 * k,
                        0.7152 - 0.7152 * k,
                        0.0722 - 0.0722 * k,
                    ],
                    [
                        0.2126 - 0.2126 * k,
                        0.7152 + 0.2848 * k,
                        0.0722 - 0.0722 * k,
                    ],
                    [
                        0.2126 - 0.2126 * k,
                        0.7152 - 0.7152 * k,
                        0.0722 + 0.9278 * k,
                    ],
                ])
            }
            FilterFunction::Sepia(a) => {
                let k = 1.0 - a;
                matrix([
                    [0.393 + 0.607 * k, 0.769 - 0.769 * k, 0.189 - 0.189 * k],
                    [0.349 - 0.349 * k, 0.686 + 0.314 * k, 0.168 - 0.168 * k],
                    [0.272 - 0.272 * k, 0.534 - 0.534 * k, 0.131 + 0.869 * k],
                ])
            }
            FilterFunction::Saturate(s) => matrix([
                [0.213 + 0.787 * s, 0.715 - 0.715 * s, 0.072 - 0.072 * s],
                [0.213 - 0.213 * s, 0.715 + 0.285 * s, 0.072 - 0.072 * s],
                [0.213 - 0.213 * s, 0.715 - 0.715 * s, 0.072 + 0.928 * s],
            ]),
            FilterFunction::HueRotate(deg) => {
                let (sin, cos) = deg.to_radians().sin_cos();
                matrix([
                    [
                        0.213 + cos * 0.787 - sin * 0.213,
                        0.715 - cos * 0.715 - sin * 0.715,
                        0.072 - cos * 0.072 + sin * 0.928,
                    ],
                    [
                        0.213 - cos * 0.213 + sin * 0.143,
                        0.715 + cos * 0.285 + sin * 0.140,
                        0.072 - cos * 0.072 - sin * 0.283,
                    ],
                    [
                        0.213 - cos * 0.213 - sin * 0.787,
                        0.715 - cos * 0.715 + sin * 0.715,
                        0.072 + cos * 0.928 + sin * 0.072,
                    ],
                ])
            }
            FilterFunction::Blur(_) | FilterFunction::DropShadow { .. } => return None,
        })
    }
}

fn color_ops(pm: &mut Pixmap, ops: &[ColorOp]) {
    if ops.is_empty() {
        return;
    }
    for p in pm.data_mut() {
        if p.a == 0 {
            continue;
        }
        let a = f32::from(p.a) / 255.0;
        let mut c = [
            f32::from(p.r) / 255.0 / a,
            f32::from(p.g) / 255.0 / a,
            f32::from(p.b) / 255.0 / a,
        ];
        let mut alpha = a;
        for op in ops {
            let m = &op.m;
            let r = m[0][0] * c[0] + m[0][1] * c[1] + m[0][2] * c[2] + op.offset;
            let g = m[1][0] * c[0] + m[1][1] * c[1] + m[1][2] * c[2] + op.offset;
            let b = m[2][0] * c[0] + m[2][1] * c[1] + m[2][2] * c[2] + op.offset;
            c = [r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0)];
            alpha *= op.opacity;
        }
        let q = |v: f32| (v * alpha * 255.0 + 0.5) as u8;
        *p = PremulRgba8 {
            r: q(c[0]),
            g: q(c[1]),
            b: q(c[2]),
            a: (alpha * 255.0 + 0.5) as u8,
        };
    }
}

/// A 1D Gaussian blur of one standard deviation.
enum Kernel {
    /// Weights for offsets `-r..=r`.
    Gauss(Vec<f32>),
    /// Box blurs run one after the other: (size, samples left of the output pixel).
    Boxes([(usize, usize); 3]),
}

impl Kernel {
    fn new(sigma: f32) -> Option<Kernel> {
        if sigma <= 0.0 {
            return None;
        }
        if sigma < 2.0 {
            let r = (3.0 * sigma).ceil() as i32;
            let w: Vec<f32> = (-r..=r)
                .map(|i| (-((i * i) as f32) / (2.0 * sigma * sigma)).exp())
                .collect();
            let sum: f32 = w.iter().sum();
            return Some(Kernel::Gauss(w.into_iter().map(|v| v / sum).collect()));
        }
        // Filter Effects 1, feGaussianBlur: d = floor(s * 3 * sqrt(2 * pi) / 4 + 0.5).
        let d = (sigma * 3.0 * (2.0 * std::f32::consts::PI).sqrt() / 4.0 + 0.5).floor() as usize;
        let d = d.max(1);
        Some(Kernel::Boxes(if d % 2 == 1 {
            [(d, d / 2); 3]
        } else {
            // Two boxes of size d centered on the pixel's left and right
            // edges, then one of size d + 1 centered on the pixel.
            [(d, d / 2), (d, d / 2 - 1), (d + 1, d / 2)]
        }))
    }

    /// Samples one output reads on each side.
    fn extent(&self) -> usize {
        match self {
            Kernel::Gauss(w) => w.len() / 2,
            Kernel::Boxes(b) => b.iter().map(|&(d, _)| d).sum::<usize>() / 2 + 2,
        }
    }

    /// Blur `line` (`ch` interleaved channels) in place; `tmp` is scratch
    /// space of the same length.
    fn run(&self, line: &mut [f32], tmp: &mut [f32], ch: usize) {
        let n = line.len() / ch;
        match self {
            Kernel::Gauss(w) => {
                let r = w.len() / 2;
                for x in 0..n {
                    for c in 0..ch {
                        let mut acc = 0.0;
                        for (k, &wk) in w.iter().enumerate() {
                            let s = x + k;
                            if s >= r && s - r < n {
                                acc += wk * line[(s - r) * ch + c];
                            }
                        }
                        tmp[x * ch + c] = acc;
                    }
                }
                line.copy_from_slice(tmp);
            }
            Kernel::Boxes(boxes) => {
                for &(d, left) in boxes {
                    let inv = 1.0 / d as f32;
                    for c in 0..ch {
                        // Window of output x: [x - left, x - left + d - 1].
                        let mut sum = 0.0;
                        for k in 0..(d - left).min(n) {
                            sum += line[k * ch + c];
                        }
                        for x in 0..n {
                            tmp[x * ch + c] = sum * inv;
                            let add = x + d - left;
                            if add < n {
                                sum += line[add * ch + c];
                            }
                            if x >= left {
                                sum -= line[(x - left) * ch + c];
                            }
                        }
                    }
                    line.copy_from_slice(tmp);
                }
            }
        }
    }
}

/// Index into `0..n` for a sample `i` places before (negative) or after the
/// line, reflected at the edges.
fn mirror(i: isize, n: usize) -> usize {
    let n = n as isize;
    let p = i.rem_euclid(2 * n);
    (if p < n { p } else { 2 * n - 1 - p }) as usize
}

/// Blur `n` samples of `ch` channels: `get(i)` reads sample `i`, `put(i, v)`
/// writes the result.
struct LineBuf {
    line: Vec<f32>,
    tmp: Vec<f32>,
}

impl LineBuf {
    fn blur(
        &mut self,
        k: &Kernel,
        n: usize,
        ch: usize,
        edge: Edge,
        get: impl Fn(usize, &mut [f32]),
        mut put: impl FnMut(usize, &[f32]),
    ) {
        let pad = k.extent();
        let len = (n + 2 * pad) * ch;
        self.line.clear();
        self.line.resize(len, 0.0);
        self.tmp.resize(len, 0.0);
        for i in 0..n + 2 * pad {
            let src = i as isize - pad as isize;
            let s = match edge {
                Edge::Transparent if src < 0 || src >= n as isize => continue,
                Edge::Transparent => src as usize,
                Edge::Mirror => mirror(src, n),
            };
            get(s, &mut self.line[i * ch..(i + 1) * ch]);
        }
        k.run(&mut self.line, &mut self.tmp[..len], ch);
        for i in 0..n {
            put(i, &self.line[(i + pad) * ch..(i + pad + 1) * ch]);
        }
    }
}

fn to_f(p: PremulRgba8, out: &mut [f32]) {
    out[0] = f32::from(p.r);
    out[1] = f32::from(p.g);
    out[2] = f32::from(p.b);
    out[3] = f32::from(p.a);
}

fn to_px(v: &[f32]) -> PremulRgba8 {
    let q = |x: f32| (x + 0.5).clamp(0.0, 255.0) as u8;
    let a = q(v[3]);
    // Rounding may leave a color channel above alpha; keep it premultiplied.
    PremulRgba8 {
        r: q(v[0]).min(a),
        g: q(v[1]).min(a),
        b: q(v[2]).min(a),
        a,
    }
}

/// Gaussian blur of all four channels with standard deviations `sx`, `sy`
/// (device px).
fn blur_rgba(pm: &mut Pixmap, sx: f32, sy: f32, edge: Edge) {
    let (w, h) = (usize::from(pm.width()), usize::from(pm.height()));
    if w == 0 || h == 0 {
        return;
    }
    let mut buf = LineBuf {
        line: Vec::new(),
        tmp: Vec::new(),
    };
    if let Some(k) = Kernel::new(sx) {
        let data = pm.data_mut();
        let mut row = vec![PremulRgba8::from_u32(0); w];
        for y in 0..h {
            row.copy_from_slice(&data[y * w..(y + 1) * w]);
            let out = &mut data[y * w..(y + 1) * w];
            buf.blur(
                &k,
                w,
                4,
                edge,
                |i, o| to_f(row[i], o),
                |i, v| out[i] = to_px(v),
            );
        }
    }
    if let Some(k) = Kernel::new(sy) {
        let data = pm.data_mut();
        let mut col = vec![PremulRgba8::from_u32(0); h];
        for x in 0..w {
            for (y, c) in col.iter_mut().enumerate() {
                *c = data[y * w + x];
            }
            buf.blur(
                &k,
                h,
                4,
                edge,
                |i, o| to_f(col[i], o),
                |i, v| data[i * w + x] = to_px(v),
            );
        }
    }
    pm.set_may_have_transparency(true);
}

/// `drop-shadow()`: the alpha of the input, offset by `(dx, dy)` and
/// blurred by `(sx, sy)` (device px), filled with `color`, under the input.
fn drop_shadow(pm: &mut Pixmap, (dx, dy): (f32, f32), (sx, sy): (f32, f32), color: Color) {
    let (w, h) = (usize::from(pm.width()), usize::from(pm.height()));
    if w == 0 || h == 0 {
        return;
    }
    let (ox, oy) = (dx.round() as isize, dy.round() as isize);
    let mut alpha = vec![0f32; w * h];
    {
        let data = pm.data();
        for y in 0..h {
            let sy_ = y as isize - oy;
            if sy_ < 0 || sy_ >= h as isize {
                continue;
            }
            for x in 0..w {
                let sx_ = x as isize - ox;
                if sx_ < 0 || sx_ >= w as isize {
                    continue;
                }
                alpha[y * w + x] = f32::from(data[sy_ as usize * w + sx_ as usize].a);
            }
        }
    }
    let mut buf = LineBuf {
        line: Vec::new(),
        tmp: Vec::new(),
    };
    if let Some(k) = Kernel::new(sx) {
        let mut row = vec![0f32; w];
        for y in 0..h {
            row.copy_from_slice(&alpha[y * w..(y + 1) * w]);
            let out = &mut alpha[y * w..(y + 1) * w];
            buf.blur(
                &k,
                w,
                1,
                Edge::Transparent,
                |i, o| o[0] = row[i],
                |i, v| out[i] = v[0],
            );
        }
    }
    if let Some(k) = Kernel::new(sy) {
        let mut col = vec![0f32; h];
        for x in 0..w {
            for (y, c) in col.iter_mut().enumerate() {
                *c = alpha[y * w + x];
            }
            buf.blur(
                &k,
                h,
                1,
                Edge::Transparent,
                |i, o| o[0] = col[i],
                |i, v| alpha[i * w + x] = v[0],
            );
        }
    }
    let ca = f32::from(color.a) / 255.0;
    let (cr, cg, cb) = (
        f32::from(color.r) / 255.0,
        f32::from(color.g) / 255.0,
        f32::from(color.b) / 255.0,
    );
    for (p, &sa) in pm.data_mut().iter_mut().zip(&alpha) {
        // Shadow alpha in 0..=1, then the input over it.
        let s = (sa / 255.0 * ca).clamp(0.0, 1.0);
        if s <= 0.0 {
            continue;
        }
        let keep = 1.0 - f32::from(p.a) / 255.0;
        let mix = |src: u8, c: f32| (f32::from(src) + c * s * 255.0 * keep + 0.5).min(255.0) as u8;
        *p = PremulRgba8 {
            r: mix(p.r, cr),
            g: mix(p.g, cg),
            b: mix(p.b, cb),
            a: mix(p.a, 1.0),
        };
    }
    pm.set_may_have_transparency(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixmap(w: u16, h: u16, f: impl Fn(usize, usize) -> PremulRgba8) -> Pixmap {
        let mut pm = Pixmap::new(w, h);
        let wu = usize::from(w);
        for (i, p) in pm.data_mut().iter_mut().enumerate() {
            *p = f(i % wu, i / wu);
        }
        pm
    }

    const ID: [f64; 4] = [1.0, 0.0, 0.0, 1.0];
    const CLEAR: PremulRgba8 = PremulRgba8 {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    #[test]
    fn blur_keeps_total_ink_and_spreads_it() {
        // One opaque white line, full height: the blur spreads it into a
        // symmetric bump across the middle row, and the ink stays the same.
        let white = PremulRgba8 {
            r: 255,
            g: 255,
            b: 255,
            a: 255,
        };
        for sigma in [1.0f32, 4.0] {
            let mut pm = pixmap(41, 41, |x, _| if x == 20 { white } else { CLEAR });
            apply(
                &mut pm,
                &[FilterFunction::Blur(sigma)],
                ID,
                Edge::Transparent,
            );
            let a: Vec<u32> = pm.data()[20 * 41..21 * 41]
                .iter()
                .map(|p| u32::from(p.a))
                .collect();
            let total: u32 = a.iter().sum();
            assert!((245..=265).contains(&total), "sigma {sigma}: total {total}");
            assert_eq!(a[19], a[21], "sigma {sigma}: {a:?}");
            assert!(a[20] > a[22] && a[22] > a[25], "sigma {sigma}: {a:?}");
            assert_eq!(a[0], 0, "sigma {sigma}: reach is at most 3 sigma");
        }
    }

    #[test]
    fn mirror_edges_keep_a_flat_backdrop_flat() {
        // Transparent edges would darken the border of a flat image.
        let px = PremulRgba8 {
            r: 0x80,
            g: 0x40,
            b: 0x20,
            a: 0xff,
        };
        let mut pm = pixmap(8, 8, |_, _| px);
        apply(&mut pm, &[FilterFunction::Blur(5.0)], ID, Edge::Mirror);
        assert!(pm.data().iter().all(|p| *p == px));
    }
}
