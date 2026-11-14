//! SVG images: usvg parses and simplifies, resvg rasterizes (on tiny-skia).
//!
//! Used for `<img src=*.svg>`, CSS `url(*.svg)` and inline `<svg>` elements
//! (the shell serializes the subtree to markup first). The result is a normal
//! [`DecodedImage`]: its `width()`/`height()` are the SVG's own size in CSS px,
//! and the pixmap is rendered at the size the caller asks for, so icons stay
//! sharp when they are displayed bigger or smaller than their natural size.

use resvg::{tiny_skia, usvg};

use crate::{DecodedImage, NetError};

/// Largest pixmap side for an SVG. Bigger requests are scaled down uniformly.
const MAX_SIDE: f32 = 4096.0;
/// Largest pixmap area for an SVG (16 Mpx = 64 MiB). Bigger requests are scaled down.
const MAX_AREA: f32 = 16.0 * 1024.0 * 1024.0;

/// True if `bytes` look like an SVG document: optional BOM and whitespace,
/// then `<svg`, or an XML declaration, comment or doctype followed by `<svg`
/// in the first KiB.
pub fn is_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    let head = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let start = head
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(head.len());
    let head = &head[start..];
    if head.starts_with(b"<svg") {
        return true;
    }
    (head.starts_with(b"<?xml") || head.starts_with(b"<!")) && head.windows(4).any(|w| w == b"<svg")
}

/// Parse an SVG document and rasterize it.
///
/// `size_hint` is the pixmap size in device pixels (the displayed size times
/// the device pixel ratio); a 0 side follows the SVG's aspect ratio. `None`
/// renders at the SVG's natural size. The
/// drawing keeps its aspect ratio and is centered in the pixmap
/// (`preserveAspectRatio="xMidYMid meet"`). Text elements are not drawn (no
/// font database).
pub fn decode_svg(bytes: &[u8], size_hint: Option<(u32, u32)>) -> Result<DecodedImage, NetError> {
    let _permit = crate::DecodePermit::take();
    let tree = usvg::Tree::from_data(bytes, &usvg::Options::default())
        .map_err(|e| NetError::Decode(format!("SVG: {e}")))?;
    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    let natural = (w.round().max(1.0) as u32, h.round().max(1.0) as u32);

    let (mut rw, mut rh) = match size_hint {
        Some((hw, hh)) if hw > 0 && hh > 0 => (hw as f32, hh as f32),
        Some((hw, 0)) if hw > 0 => (hw as f32, hw as f32 * h / w),
        Some((0, hh)) if hh > 0 => (hh as f32 * w / h, hh as f32),
        _ => (w, h),
    };
    let shrink = (MAX_SIDE / rw.max(rh))
        .min((MAX_AREA / (rw * rh)).sqrt())
        .min(1.0);
    rw = (rw * shrink).round().max(1.0);
    rh = (rh * shrink).round().max(1.0);

    let mut pixmap = tiny_skia::Pixmap::new(rw as u32, rh as u32)
        .ok_or_else(|| NetError::Decode(format!("SVG pixmap {rw}x{rh}")))?;
    let scale = (rw / w).min(rh / h);
    let transform = tiny_skia::Transform::from_row(
        scale,
        0.0,
        0.0,
        scale,
        (rw - w * scale) / 2.0,
        (rh - h * scale) / 2.0,
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    drop(tree);

    // tiny-skia pixels are premultiplied RGBA8, the same layout vello stores,
    // so the buffer moves over without a conversion pass.
    let (pw, ph) = (pixmap.width() as u16, pixmap.height() as u16);
    let pixmap = crate::Pixmap::from_parts(
        pixmap.take(),
        pw,
        ph,
        vello_common::pixmap::PixelMetadata {
            may_have_transparency: true,
            alpha_type: vello_common::peniko::ImageAlphaType::AlphaPremultiplied,
        },
    );
    Ok(DecodedImage::new(pixmap, natural))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ICON: &[u8] =
        br##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="8" viewBox="0 0 2 1">
        <rect width="1" height="1" fill="#f00"/></svg>"##;

    fn rgba(img: &DecodedImage, x: u16, y: u16) -> [u8; 4] {
        let p = img.pixmap().sample(x, y);
        [p.r, p.g, p.b, p.a]
    }

    #[test]
    fn natural_size_is_css_size_and_pixmap_follows_the_hint() {
        let img = decode_svg(ICON, None).unwrap();
        assert_eq!((img.width(), img.height()), (16, 8));
        assert_eq!((img.pixmap().width(), img.pixmap().height()), (16, 8));
        assert_eq!(rgba(&img, 2, 4), [255, 0, 0, 255]);
        assert_eq!(rgba(&img, 12, 4)[3], 0, "right half is empty");

        let big = decode_svg(ICON, Some((64, 32))).unwrap();
        assert_eq!(
            (big.width(), big.height()),
            (16, 8),
            "natural size does not change"
        );
        assert_eq!((big.pixmap().width(), big.pixmap().height()), (64, 32));
        assert_eq!(rgba(&big, 31, 16), [255, 0, 0, 255]);
        assert_eq!(rgba(&big, 33, 16)[3], 0);
    }

    #[test]
    fn huge_hints_are_capped() {
        let img = decode_svg(ICON, Some((100_000, 50_000))).unwrap();
        assert!(img.pixmap().width() <= 4096 && img.pixmap().height() <= 2048);
    }

    #[test]
    fn sniffing() {
        assert!(is_svg(b"  <svg xmlns='http://www.w3.org/2000/svg'/>"));
        assert!(is_svg(b"<?xml version='1.0'?>\n<svg/>"));
        assert!(is_svg(b"<!-- c --><svg/>"));
        assert!(!is_svg(b"\x89PNG\r\n"));
        assert!(!is_svg(b"<!doctype html><html>"));
    }
}
