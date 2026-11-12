//! Web font decoding: WOFF and WOFF2 to plain TrueType/OpenType (sfnt).

use crate::NetError;

/// The container format of a font file, from its first four bytes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FontFormat {
    /// TrueType, OpenType/CFF or a TrueType collection. Usable as is.
    Sfnt,
    Woff,
    Woff2,
}

/// Sniff the font container from the magic number. `None` for anything else
/// (HTML error pages, SVG fonts, EOT).
pub fn font_format(bytes: &[u8]) -> Option<FontFormat> {
    match bytes.get(..4)? {
        b"wOFF" => Some(FontFormat::Woff),
        b"wOF2" => Some(FontFormat::Woff2),
        [0, 1, 0, 0] | b"OTTO" | b"true" | b"ttcf" => Some(FontFormat::Sfnt),
        _ => None,
    }
}

/// WOFF and WOFF2 to TrueType/OpenType bytes; sfnt passes through without a copy.
///
/// Takes the body by value so plain TTF/OTF files are not copied, and so the
/// compressed WOFF body is freed as soon as the sfnt is built.
pub fn decode_font(bytes: Vec<u8>) -> Result<Vec<u8>, NetError> {
    match font_format(&bytes) {
        Some(FontFormat::Sfnt) => Ok(bytes),
        Some(FontFormat::Woff) => {
            wuff::decompress_woff1(&bytes).map_err(|e| NetError::Decode(format!("WOFF: {e}")))
        }
        Some(FontFormat::Woff2) => {
            wuff::decompress_woff2(&bytes).map_err(|e| NetError::Decode(format!("WOFF2: {e}")))
        }
        None => Err(NetError::Decode(
            "not a TrueType, OpenType, WOFF or WOFF2 font".into(),
        )),
    }
}

/// `@font-face { size-adjust }`: scale the glyph outlines and metrics of a
/// TrueType-outline font by `factor` (CSS Fonts 5). Outlines, advances,
/// kerning and the vertical metrics are all in font units, so dividing
/// `head.unitsPerEm` by `factor` scales them together; the font size the
/// page asks for is unchanged. Returns false and leaves the font as is for
/// a collection, a CFF font (its FontMatrix can override the em), a broken
/// table directory, or a units-per-em outside 16..=16384.
pub fn size_adjust_font(sfnt: &mut [u8], factor: f32) -> bool {
    fn be16(b: &[u8], at: usize) -> Option<u16> {
        Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
    }
    fn be32(b: &[u8], at: usize) -> Option<u32> {
        Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
    }
    fn units_per_em_at(sfnt: &[u8]) -> Option<usize> {
        if !matches!(sfnt.get(..4)?, [0, 1, 0, 0] | b"true") {
            return None;
        }
        let (mut head, mut glyf) = (None, false);
        for i in 0..usize::from(be16(sfnt, 4)?) {
            let rec = 12 + 16 * i;
            match sfnt.get(rec..rec + 4)? {
                b"head" => head = Some(be32(sfnt, rec + 8)? as usize + 18),
                b"glyf" => glyf = true,
                _ => {}
            }
        }
        head.filter(|&at| glyf && at + 2 <= sfnt.len())
    }
    if !(factor.is_finite() && factor > 0.0) {
        return false;
    }
    let Some(at) = units_per_em_at(sfnt) else {
        return false;
    };
    let Some(upem) = be16(sfnt, at) else {
        return false;
    };
    let scaled = (f32::from(upem) / factor).round();
    if !(16.0..=16384.0).contains(&scaled) {
        return false;
    }
    sfnt[at..at + 2].copy_from_slice(&(scaled as u16).to_be_bytes());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_containers_and_rejects_garbage() {
        assert_eq!(font_format(b"wOF2...."), Some(FontFormat::Woff2));
        assert_eq!(font_format(b"wOFF...."), Some(FontFormat::Woff));
        assert_eq!(font_format(b"OTTO...."), Some(FontFormat::Sfnt));
        assert_eq!(font_format(&[0, 1, 0, 0, 9]), Some(FontFormat::Sfnt));
        assert!(decode_font(b"<!doctype html>".to_vec()).is_err());
        assert!(decode_font(b"wOF2 truncated".to_vec()).is_err());
    }

    /// A table directory with `head` (units-per-em 1000) and an empty
    /// outline table `outlines` (b"glyf" or b"CFF ").
    fn sfnt(version: &[u8; 4], outlines: &[u8; 4]) -> Vec<u8> {
        let mut f = version.to_vec();
        f.extend_from_slice(&[0, 2, 0, 32, 0, 1, 0, 0]);
        for (tag, offset, len) in [(b"head", 44u32, 54u32), (outlines, 98, 0)] {
            f.extend_from_slice(tag);
            f.extend_from_slice(&[0; 4]);
            f.extend_from_slice(&offset.to_be_bytes());
            f.extend_from_slice(&len.to_be_bytes());
        }
        let mut head = [0u8; 54];
        head[18..20].copy_from_slice(&1000u16.to_be_bytes());
        f.extend_from_slice(&head);
        f
    }

    #[test]
    fn size_adjust_rescales_units_per_em_of_truetype_fonts_only() {
        let upem = |f: &[u8]| u16::from_be_bytes([f[44 + 18], f[44 + 19]]);
        let mut f = sfnt(&[0, 1, 0, 0], b"glyf");
        assert!(size_adjust_font(&mut f, 0.95));
        assert_eq!(upem(&f), 1053, "outlines and metrics shrink to 95%");
        let mut f = sfnt(&[0, 1, 0, 0], b"glyf");
        assert!(size_adjust_font(&mut f, 2.0));
        assert_eq!(upem(&f), 500);
        let mut f = sfnt(&[0, 1, 0, 0], b"glyf");
        assert!(!size_adjust_font(&mut f, 0.01), "units-per-em above 16384");
        let mut cff = sfnt(b"OTTO", b"CFF ");
        assert!(!size_adjust_font(&mut cff, 0.95));
        assert_eq!(upem(&cff), 1000);
        assert!(!size_adjust_font(&mut f[..30], 0.95), "truncated directory");
    }
}
