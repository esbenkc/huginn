//! AVIF still images.
//!
//! The HEIF container is parsed by `avif-parse`. The AV1 frames inside it
//! (the color image and an optional alpha image) are decoded by `rav1d`, a
//! Rust port of dav1d, the AV1 decoder Chrome uses. YUV is converted to RGB
//! here with the matrix and range from the AV1 sequence header; subsampled
//! chroma is upsampled bilinearly, like libavif.
//!
//! Not supported (the decode fails or ignores them): grid (tiled) images,
//! the `irot`/`imir`/`clap` transforms, and animation beyond the primary
//! frame.

use std::mem::MaybeUninit;
use std::ptr::{self, NonNull};

use rav1d::Dav1dResult;
use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::*;
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_get_picture,
    dav1d_open, dav1d_picture_unref, dav1d_send_data,
};

/// Largest frame accepted, in pixels: 512 MiB as RGBA8, the same cap as the
/// `image` crate's default allocation limit for the other formats.
const MAX_PIXELS: u32 = 1 << 27;

/// An AVIF file: an ISO-BMFF `ftyp` box with the `avif` or `avis` brand
/// (major or compatible).
pub fn is_avif(bytes: &[u8]) -> bool {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    let size = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let brand = |b: &[u8]| b == b"avif" || b == b"avis";
    // Compatible brands follow the 4-byte minor version at offset 12.
    let compatible = bytes.get(16..size.clamp(16, bytes.len())).unwrap_or(&[]);
    brand(&bytes[8..12]) || compatible.chunks_exact(4).any(brand)
}

/// Decoded AVIF pixels: straight (not premultiplied) RGBA8, row-major.
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    /// An alpha image was present (the pixels may be transparent).
    pub has_alpha: bool,
}

/// Decode the primary image of an AVIF file (and its alpha image, if any).
pub fn decode(bytes: &[u8]) -> Result<Rgba, String> {
    let data = avif_parse::read_avif(&mut &bytes[..]).map_err(|e| format!("AVIF: {e}"))?;
    let color = Decoder::new()?.decode(&data.primary_item)?;
    let (w, h) = color.size();
    let mut pixels = vec![0u8; w * h * 4];
    color.write_rgb(&mut pixels)?;
    drop(color);
    let mut has_alpha = false;
    if let Some(item) = &data.alpha_item {
        let alpha = Decoder::new()?.decode(item)?;
        // The alpha image must match the color image (HEIF); ignore it otherwise.
        if alpha.size() == (w, h) {
            alpha.write_alpha(&mut pixels)?;
            has_alpha = true;
            if data.premultiplied_alpha {
                unpremultiply(&mut pixels);
            }
        }
    }
    Ok(Rgba {
        width: w as u32,
        height: h as u32,
        pixels,
        has_alpha,
    })
}

/// A rav1d decoder for one still image.
struct Decoder(Option<Dav1dContext>);

impl Decoder {
    fn new() -> Result<Decoder, String> {
        // SAFETY: dav1d_default_settings fills every field of the settings;
        // dav1d_open gets valid pointers to the settings and the context slot.
        unsafe {
            let mut settings = MaybeUninit::<Dav1dSettings>::uninit();
            dav1d_default_settings(NonNull::from(&mut settings).cast());
            let mut settings = settings.assume_init();
            // Decode on the calling thread: images are already decoded in
            // parallel, one per fetch thread.
            settings.n_threads = 1;
            settings.max_frame_delay = 1;
            // Only the highest spatial layer (what libavif asks for).
            settings.all_layers = 0;
            settings.frame_size_limit = MAX_PIXELS;
            let mut ctx = None;
            check(dav1d_open(
                Some(NonNull::from(&mut ctx)),
                Some(NonNull::from(&mut settings)),
            ))?;
            Ok(Decoder(ctx))
        }
    }

    /// Decode one AV1 temporal unit (an AVIF item) to its last picture.
    fn decode(&mut self, obus: &[u8]) -> Result<Picture, String> {
        let ctx = self.0.ok_or("AVIF: decoder not open")?;
        let mut last = None;
        // SAFETY: `ctx` is open; `data` is created by dav1d_data_create with
        // room for `obus`, and released by dav1d_data_unref.
        unsafe {
            let mut data = Dav1dData::default();
            let buf = dav1d_data_create(Some(NonNull::from(&mut data)), obus.len());
            if buf.is_null() {
                return Err("AVIF: out of memory".into());
            }
            ptr::copy_nonoverlapping(obus.as_ptr(), buf, obus.len());
            let mut sent = Ok(());
            while data.sz > 0 {
                let r = dav1d_send_data(Some(ctx), Some(NonNull::from(&mut data)));
                if !would_block(r) {
                    sent = check(r);
                    break;
                }
                // The decoder holds a finished picture: take it, then send the rest.
                match Picture::get(ctx) {
                    Ok(p) => last = Some(p),
                    Err(e) => {
                        sent = Err(e);
                        break;
                    }
                }
            }
            dav1d_data_unref(Some(NonNull::from(&mut data)));
            sent?;
        }
        while let Ok(p) = Picture::get(ctx) {
            last = Some(p);
        }
        last.ok_or_else(|| "AVIF: no frame decoded".into())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the context came from dav1d_open; dav1d_close clears it.
        unsafe { dav1d_close(Some(NonNull::from(&mut self.0))) };
    }
}

/// `dav1d_*` return 0 or a negated errno.
fn check(r: Dav1dResult) -> Result<(), String> {
    if r.0 == 0 {
        Ok(())
    } else {
        Err(format!(
            "AVIF: {}",
            std::io::Error::from_raw_os_error(r.0.saturating_neg())
        ))
    }
}

/// `EAGAIN`: the decoder wants a picture taken before it reads more data.
fn would_block(r: Dav1dResult) -> bool {
    r.0 < 0
        && std::io::Error::from_raw_os_error(r.0.saturating_neg()).kind()
            == std::io::ErrorKind::WouldBlock
}

/// A decoded frame, released on drop.
struct Picture(Dav1dPicture);

impl Drop for Picture {
    fn drop(&mut self) {
        // SAFETY: the picture came from dav1d_get_picture.
        unsafe { dav1d_picture_unref(Some(NonNull::from(&mut self.0))) };
    }
}

/// One plane of samples: `u8` for 8-bit pictures, `u16` for 10 and 12 bit.
struct Plane<'a, T> {
    samples: &'a [T],
    stride: usize,
}

impl<T: Copy> Plane<'_, T> {
    fn row(&self, y: usize) -> &[T] {
        &self.samples[y * self.stride..]
    }
}

/// Chroma at full resolution, one row at a time. Subsampled chroma is
/// interpolated bilinearly: each output pixel mixes its two nearest chroma
/// samples 3:1 per axis (samples sit at the center of their luma block), as
/// libavif and libyuv do.
struct Upsampler {
    shift: (usize, usize),
    /// Chroma plane size.
    size: (usize, usize),
    /// The current row after the vertical pass, times 4.
    cols: Vec<u32>,
}

impl Upsampler {
    fn new((w, h): (usize, usize), (sx, sy): (usize, usize)) -> Upsampler {
        let size = ((w + sx) >> sx, (h + sy) >> sy);
        Upsampler {
            shift: (sx, sy),
            size,
            cols: vec![0; size.0],
        }
    }

    fn row<T: Copy + Into<u32>>(&mut self, p: &Plane<'_, T>, y: usize, out: &mut [u16]) {
        let ((sx, sy), (cw, ch)) = (self.shift, self.size);
        let cy = y >> sy;
        let near = &p.row(cy)[..cw];
        if sy == 1 {
            let ny = if y & 1 == 1 {
                (cy + 1).min(ch - 1)
            } else {
                cy.saturating_sub(1)
            };
            for ((c, &a), &b) in self.cols.iter_mut().zip(near).zip(&p.row(ny)[..cw]) {
                *c = 3 * a.into() + b.into();
            }
        } else {
            for (c, &a) in self.cols.iter_mut().zip(near) {
                *c = 4 * a.into();
            }
        }
        if sx == 1 {
            for (x, o) in out.iter_mut().enumerate() {
                let cx = x >> 1;
                let nx = if x & 1 == 1 {
                    (cx + 1).min(cw - 1)
                } else {
                    cx.saturating_sub(1)
                };
                *o = ((3 * self.cols[cx] + self.cols[nx] + 8) >> 4) as u16;
            }
        } else {
            for (o, &c) in out.iter_mut().zip(&self.cols) {
                *o = ((c + 2) >> 2) as u16;
            }
        }
    }
}

impl Picture {
    fn get(ctx: Dav1dContext) -> Result<Picture, String> {
        let mut pic = Picture(Dav1dPicture::default());
        // SAFETY: `ctx` is open and `pic.0` is a valid (empty) picture; on
        // error it stays empty and its drop is a no-op unref.
        check(unsafe { dav1d_get_picture(Some(ctx), Some(NonNull::from(&mut pic.0))) })?;
        Ok(pic)
    }

    fn size(&self) -> (usize, usize) {
        (self.0.p.w.max(0) as usize, self.0.p.h.max(0) as usize)
    }

    /// Chroma subsampling (x, y shifts), or `None` for monochrome.
    fn chroma_shift(&self) -> Result<Option<(usize, usize)>, String> {
        Ok(match self.0.p.layout {
            DAV1D_PIXEL_LAYOUT_I400 => None,
            DAV1D_PIXEL_LAYOUT_I420 => Some((1, 1)),
            DAV1D_PIXEL_LAYOUT_I422 => Some((1, 0)),
            DAV1D_PIXEL_LAYOUT_I444 => Some((0, 0)),
            _ => return Err("AVIF: unknown pixel layout".into()),
        })
    }

    /// Plane `n` (0 luma, 1 U, 2 V).
    fn plane<T>(&self, n: usize) -> Result<Plane<'_, T>, String> {
        let (w, h) = self.size();
        let (w, h) = match (n, self.chroma_shift()?) {
            (0, _) => (w, h),
            (_, Some((sx, sy))) => ((w + sx) >> sx, (h + sy) >> sy),
            (_, None) => return Err("AVIF: no chroma plane".into()),
        };
        let bytes = std::mem::size_of::<T>();
        let data = self.0.data[n].ok_or("AVIF: missing plane")?;
        // Luma has its own stride; both chroma planes share one.
        let stride = self.0.stride[usize::from(n != 0)];
        if stride <= 0 || stride as usize % bytes != 0 || (stride as usize / bytes) < w {
            return Err("AVIF: bad stride".into());
        }
        let stride = stride as usize / bytes;
        // SAFETY: rav1d allocates whole rows, so the plane holds `stride * h`
        // samples of the picture's bit depth; the picture outlives the slice.
        let samples = unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<T>(), stride * h) };
        Ok(Plane { samples, stride })
    }

    fn color(&self) -> (Dav1dMatrixCoefficients, bool) {
        // SAFETY: a decoded picture's sequence header lives as long as it does.
        match self.0.seq_hdr.map(|h| unsafe { h.as_ref() }) {
            Some(h) => (h.mtrx, h.color_range != 0),
            None => (DAV1D_MC_UNKNOWN, false),
        }
    }

    /// Fill the RGB bytes of `out` (RGBA8) and set alpha to opaque.
    fn write_rgb(&self, out: &mut [u8]) -> Result<(), String> {
        match self.0.p.bpc {
            8 => self.write_rgb_as::<u8>(out),
            10 | 12 => self.write_rgb_as::<u16>(out),
            d => Err(format!("AVIF: unsupported bit depth {d}")),
        }
    }

    fn write_rgb_as<T: Copy + Into<u32>>(&self, out: &mut [u8]) -> Result<(), String> {
        let (w, h) = self.size();
        let (mtrx, full) = self.color();
        let lut = Luts::new(self.0.p.bpc as u32, mtrx, full);
        let luma = self.plane::<T>(0)?;
        let Some((sx, sy)) = self.chroma_shift()? else {
            for (y, row) in out.chunks_exact_mut(w * 4).take(h).enumerate() {
                for (px, &l) in row.chunks_exact_mut(4).zip(luma.row(y)) {
                    let v = lut.clamp(lut.y[l.into() as usize]);
                    px.copy_from_slice(&[v, v, v, 255]);
                }
            }
            return Ok(());
        };
        let (u, v) = (self.plane::<T>(1)?, self.plane::<T>(2)?);
        let mut up = Upsampler::new((w, h), (sx, sy));
        let (mut cb_row, mut cr_row) = (vec![0u16; w], vec![0u16; w]);
        for (y, row) in out.chunks_exact_mut(w * 4).take(h).enumerate() {
            up.row(&u, y, &mut cb_row);
            up.row(&v, y, &mut cr_row);
            let pixels = row
                .chunks_exact_mut(4)
                .zip(luma.row(y))
                .zip(cb_row.iter().zip(&cr_row));
            for ((px, &l), (&cb, &cr)) in pixels {
                let (l, cb, cr) = (lut.y[l.into() as usize], usize::from(cb), usize::from(cr));
                let (r, g, b) = match lut.kind {
                    Kind::Ycbcr => (
                        l + lut.r_cr[cr],
                        l + lut.g_cr[cr] + lut.g_cb[cb],
                        l + lut.b_cb[cb],
                    ),
                    // Y = G, U = B, V = R.
                    Kind::Identity => (lut.y[cr], l, lut.y[cb]),
                    // Y, U = Cg, V = Co.
                    Kind::YCgCo => {
                        let (cg, co) = (lut.c[cb], lut.c[cr]);
                        let t = l - cg;
                        (t + co, l + cg, t - co)
                    }
                };
                px.copy_from_slice(&[lut.clamp(r), lut.clamp(g), lut.clamp(b), 255]);
            }
        }
        Ok(())
    }

    /// Set the alpha bytes of `out` (RGBA8) from this monochrome picture.
    fn write_alpha(&self, out: &mut [u8]) -> Result<(), String> {
        match self.0.p.bpc {
            8 => self.write_alpha_as::<u8>(out),
            10 | 12 => self.write_alpha_as::<u16>(out),
            d => Err(format!("AVIF: unsupported alpha bit depth {d}")),
        }
    }

    fn write_alpha_as<T: Copy + Into<u32>>(&self, out: &mut [u8]) -> Result<(), String> {
        let (w, h) = self.size();
        let (_, full) = self.color();
        let lut = Luts::new(self.0.p.bpc as u32, DAV1D_MC_IDENTITY, full);
        let plane = self.plane::<T>(0)?;
        for (y, row) in out.chunks_exact_mut(w * 4).take(h).enumerate() {
            for (px, &a) in row.chunks_exact_mut(4).zip(plane.row(y)) {
                px[3] = lut.clamp(lut.y[a.into() as usize]);
            }
        }
        Ok(())
    }
}

/// Straight alpha from premultiplied (`premultiplied_alpha` in the file).
fn unpremultiply(pixels: &mut [u8]) {
    for px in pixels.chunks_exact_mut(4) {
        let a = u32::from(px[3]);
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

#[derive(Copy, Clone)]
enum Kind {
    Ycbcr,
    Identity,
    YCgCo,
}

/// Per-sample lookup tables in 16.16 fixed point, scaled to 0..=255.
struct Luts {
    kind: Kind,
    /// Luma (or any full-swing component) to output.
    y: Vec<i32>,
    /// Chroma, centered: Cr to R, Cr and Cb to G, Cb to B.
    r_cr: Vec<i32>,
    g_cr: Vec<i32>,
    g_cb: Vec<i32>,
    b_cb: Vec<i32>,
    /// Centered chroma without a matrix (YCgCo).
    c: Vec<i32>,
}

impl Luts {
    fn new(depth: u32, mtrx: Dav1dMatrixCoefficients, full: bool) -> Luts {
        let n = 1usize << depth;
        let max = (n - 1) as f64;
        let s = f64::from(1u32 << (depth - 8));
        // Luma: full range 0..max, limited 16..235 (scaled to the depth).
        let (y_off, y_div) = if full {
            (0.0, max)
        } else {
            (16.0 * s, 219.0 * s)
        };
        let (c_mid, c_div) = (
            f64::from(1u32 << (depth - 1)),
            if full { max } else { 224.0 * s },
        );
        let (kind, kr, kb) = match mtrx {
            DAV1D_MC_IDENTITY => (Kind::Identity, 0.0, 0.0),
            DAV1D_MC_SMPTE_YCGCO => (Kind::YCgCo, 0.0, 0.0),
            DAV1D_MC_BT709 => (Kind::Ycbcr, 0.2126, 0.0722),
            DAV1D_MC_FCC => (Kind::Ycbcr, 0.30, 0.11),
            DAV1D_MC_SMPTE240 => (Kind::Ycbcr, 0.212, 0.087),
            DAV1D_MC_BT2020_NCL | DAV1D_MC_BT2020_CL => (Kind::Ycbcr, 0.2627, 0.0593),
            // BT.601 (also for unspecified, as libavif and Chrome do).
            _ => (Kind::Ycbcr, 0.299, 0.114),
        };
        let kg = 1.0 - kr - kb;
        let fixed = |f: f64| (f * 255.0 * 65536.0).round() as i32;
        let table = |f: &dyn Fn(f64) -> f64| (0..n).map(|v| fixed(f(v as f64))).collect();
        let chroma = |v: f64| (v - c_mid) / c_div;
        let (r_cr, g_cr, g_cb, b_cb) = match kind {
            Kind::Ycbcr => (
                table(&|v| 2.0 * (1.0 - kr) * chroma(v)),
                table(&|v| -2.0 * kr * (1.0 - kr) / kg * chroma(v)),
                table(&|v| -2.0 * kb * (1.0 - kb) / kg * chroma(v)),
                table(&|v| 2.0 * (1.0 - kb) * chroma(v)),
            ),
            _ => Default::default(),
        };
        Luts {
            kind,
            y: table(&|v| (v - y_off) / y_div),
            r_cr,
            g_cr,
            g_cb,
            b_cb,
            c: match kind {
                Kind::YCgCo => table(&chroma),
                _ => Vec::new(),
            },
        }
    }

    fn clamp(&self, v: i32) -> u8 {
        ((v + 32768) >> 16).clamp(0, 255) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_avif_brands() {
        let mut f = Vec::new();
        f.extend_from_slice(&24u32.to_be_bytes());
        f.extend_from_slice(b"ftypmif1\0\0\0\0avifmiaf");
        assert!(is_avif(&f), "compatible brand");
        f[8..12].copy_from_slice(b"avis");
        f[16..20].copy_from_slice(b"mif1");
        assert!(is_avif(&f), "major brand");
        f[8..12].copy_from_slice(b"heic");
        assert!(!is_avif(&f), "HEIC is not AVIF");
        assert!(!is_avif(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"));
    }

    fn fixture(name: &str) -> Rgba {
        let path = format!(
            "{}/../../fixtures/assets/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert!(is_avif(&bytes), "{name} is sniffed as AVIF");
        decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn px(img: &Rgba, x: usize, y: usize) -> [u8; 4] {
        let i = (y * img.width as usize + x) * 4;
        img.pixels[i..i + 4].try_into().unwrap()
    }

    /// Each channel within `tol` of `want` (the fixtures are lossy).
    fn near(got: [u8; 4], want: [u8; 4], tol: u8) -> bool {
        got.iter().zip(want).all(|(&g, w)| g.abs_diff(w) <= tol)
    }

    #[test]
    fn decodes_every_color_format() {
        // 32x16, left half red, right half blue, encoded by libavif's avifenc:
        // 8-bit 4:2:0 BT.601 full range, 8-bit 4:2:0 BT.709 limited range,
        // 10-bit 4:4:4, and lossless (identity matrix, GBR 4:4:4).
        for name in [
            "redblue-420.avif",
            "redblue-709-limited.avif",
            "redblue-10bit.avif",
            "redblue-lossless.avif",
        ] {
            let img = fixture(name);
            assert_eq!((img.width, img.height), (32, 16), "{name}");
            assert!(!img.has_alpha, "{name}");
            for y in [0, 8, 15] {
                let (left, right) = (px(&img, 4, y), px(&img, 27, y));
                assert!(near(left, [255, 0, 0, 255], 6), "{name} red: {left:?}");
                assert!(near(right, [0, 0, 255, 255], 6), "{name} blue: {right:?}");
            }
        }
        let lossless = fixture("redblue-lossless.avif");
        assert_eq!(px(&lossless, 15, 3), [255, 0, 0, 255], "lossless is exact");
        assert_eq!(px(&lossless, 16, 3), [0, 0, 255, 255], "lossless is exact");
    }

    #[test]
    fn alpha_image_sets_transparency_straight_or_premultiplied() {
        // Left half opaque green; right half red at alpha 128 (top) and fully
        // transparent (bottom). The second file stores premultiplied color.
        for name in ["alpha.avif", "alpha-premul.avif"] {
            let img = fixture(name);
            assert!(img.has_alpha, "{name}");
            let (green, half, clear) = (px(&img, 4, 4), px(&img, 27, 3), px(&img, 27, 12));
            assert!(near(green, [0, 255, 0, 255], 6), "{name}: {green:?}");
            assert!(near(half, [255, 0, 0, 128], 8), "{name}: {half:?}");
            assert_eq!(clear[3], 0, "{name}: {clear:?}");
        }
    }

    #[test]
    fn rejects_truncated_files() {
        let path = format!(
            "{}/../../fixtures/assets/redblue-420.avif",
            env!("CARGO_MANIFEST_DIR")
        );
        let bytes = std::fs::read(path).unwrap();
        for len in [40, bytes.len() / 2, bytes.len() - 8] {
            assert!(decode(&bytes[..len]).is_err(), "{len} bytes");
        }
    }
}
