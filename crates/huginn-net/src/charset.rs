//! Character encodings: which encoding a document or stylesheet uses, and
//! decoding it to UTF-8 (WHATWG Encoding Standard via `encoding_rs`).
//!
//! UTF-8 input without a BOM is borrowed, not copied.

use std::borrow::Cow;

use encoding_rs::{Encoding, UTF_8, WINDOWS_1252};

/// How far the `<meta charset>` prescan looks (HTML spec: 1024 bytes).
const PRESCAN_LIMIT: usize = 1024;

/// Decode an HTML document. Order (HTML spec "determining the character
/// encoding"): byte order mark, the transport charset (`Content-Type`), a
/// `<meta charset>` or `<meta http-equiv=content-type>` in the first 1024
/// bytes. Without any of those: UTF-8 if the bytes are valid UTF-8, else
/// windows-1252 (what Chrome's detector picks for Latin-script pages).
pub fn decode_html<'a>(bytes: &'a [u8], transport: Option<&str>) -> Cow<'a, str> {
    if let Some((enc, bom)) = Encoding::for_bom(bytes) {
        return decode_without_bom(enc, &bytes[bom..]);
    }
    if let Some(enc) = transport.and_then(label) {
        return decode_without_bom(html_override(enc), bytes);
    }
    if let Some(enc) = prescan_meta(&bytes[..bytes.len().min(PRESCAN_LIMIT)]) {
        return decode_without_bom(html_override(enc), bytes);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => Cow::Borrowed(s),
        Err(_) => decode_without_bom(WINDOWS_1252, bytes),
    }
}

/// [`decode_html`] for a document that arrives in pieces (the preload
/// scanner reads it while the rest is on the way). The encoding is picked
/// like `decode_html` does once the first 1024 bytes are in (or at the end
/// of a shorter document); without a declaration it is UTF-8, invalid bytes
/// replaced. (`decode_html` decodes such a document as windows-1252 if the
/// whole of it is not UTF-8.)
pub struct HtmlStreamDecoder {
    transport: Option<&'static Encoding>,
    /// Bytes that came before the encoding was picked.
    pending: Vec<u8>,
    decoder: Option<encoding_rs::Decoder>,
}

impl HtmlStreamDecoder {
    /// `transport`: the `charset` parameter of the `Content-Type`.
    pub fn new(transport: Option<&str>) -> HtmlStreamDecoder {
        HtmlStreamDecoder {
            transport: transport.and_then(label).map(html_override),
            pending: Vec::new(),
            decoder: None,
        }
    }

    /// Decode the next `bytes` of the document and append the text to
    /// `out`. `last`: they are the end of the document.
    pub fn push(&mut self, bytes: &[u8], last: bool, out: &mut String) {
        if let Some(d) = &mut self.decoder {
            decode_into(d, bytes, last, out);
            return;
        }
        self.pending.extend_from_slice(bytes);
        let enough = if self.transport.is_some() {
            3
        } else {
            PRESCAN_LIMIT
        };
        if self.pending.len() < enough && !last {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        let (enc, skip) = match Encoding::for_bom(&pending) {
            Some((enc, bom)) => (enc, bom),
            None => (
                self.transport
                    .or_else(|| {
                        prescan_meta(&pending[..pending.len().min(PRESCAN_LIMIT)])
                            .map(html_override)
                    })
                    .unwrap_or(UTF_8),
                0,
            ),
        };
        let d = self.decoder.insert(enc.new_decoder_without_bom_handling());
        decode_into(d, &pending[skip..], last, out);
    }
}

fn decode_into(d: &mut encoding_rs::Decoder, mut bytes: &[u8], last: bool, out: &mut String) {
    loop {
        if let Some(n) = d.max_utf8_buffer_length(bytes.len()) {
            out.reserve(n);
        }
        let (result, read, _) = d.decode_to_string(bytes, out, last);
        bytes = &bytes[read..];
        if result == encoding_rs::CoderResult::InputEmpty {
            return;
        }
    }
}

/// Decode a stylesheet or any other text resource. Order (CSS Syntax
/// "determine the fallback encoding"): byte order mark, the transport charset,
/// an `@charset "...";` rule at byte 0, then UTF-8.
pub fn decode_text<'a>(bytes: &'a [u8], transport: Option<&str>) -> Cow<'a, str> {
    if let Some((enc, bom)) = Encoding::for_bom(bytes) {
        return decode_without_bom(enc, &bytes[bom..]);
    }
    let enc = transport
        .and_then(label)
        .or_else(|| css_charset_rule(bytes))
        .unwrap_or(UTF_8);
    decode_without_bom(enc, bytes)
}

fn decode_without_bom<'a>(enc: &'static Encoding, bytes: &'a [u8]) -> Cow<'a, str> {
    enc.decode_without_bom_handling(bytes).0
}

fn label(name: &str) -> Option<&'static Encoding> {
    Encoding::for_label(name.trim().as_bytes())
}

/// HTML never decodes as UTF-16 from a declaration (the BOM case is handled
/// before), and `x-user-defined` means windows-1252.
fn html_override(enc: &'static Encoding) -> &'static Encoding {
    if enc == encoding_rs::UTF_16BE || enc == encoding_rs::UTF_16LE {
        UTF_8
    } else if enc == encoding_rs::X_USER_DEFINED {
        WINDOWS_1252
    } else {
        enc
    }
}

/// `@charset "name";` exactly at the start of a stylesheet.
fn css_charset_rule(bytes: &[u8]) -> Option<&'static Encoding> {
    let rest = bytes.strip_prefix(b"@charset \"")?;
    let end = rest.iter().take(64).position(|&b| b == b'"')?;
    if rest.get(end + 1) != Some(&b';') {
        return None;
    }
    let enc = Encoding::for_label(&rest[..end])?;
    // A stylesheet that says UTF-16 in ASCII bytes is really UTF-8.
    Some(
        if enc == encoding_rs::UTF_16BE || enc == encoding_rs::UTF_16LE {
            UTF_8
        } else {
            enc
        },
    )
}

/// The HTML spec's "prescan a byte stream to determine its encoding",
/// simplified: comments are skipped, `<meta>` attributes are read, any other
/// tag is skipped to its `>` (respecting quotes).
fn prescan_meta(bytes: &[u8]) -> Option<&'static Encoding> {
    let mut pos = 0;
    while pos < bytes.len() {
        let rest = &bytes[pos..];
        if rest.starts_with(b"<!--") {
            pos += find(&rest[4..], b"-->").map_or(bytes.len(), |i| 4 + i + 3);
            continue;
        }
        if rest.len() > 5
            && rest[..5].eq_ignore_ascii_case(b"<meta")
            && matches!(rest[5], b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'/')
        {
            let (end, found) = meta_charset(&rest[5..]);
            if found.is_some() {
                return found;
            }
            pos += 5 + end;
            continue;
        }
        if rest.len() > 1
            && rest[0] == b'<'
            && (rest[1].is_ascii_alphabetic() || matches!(rest[1], b'/' | b'!' | b'?'))
        {
            pos += skip_tag(rest);
            continue;
        }
        pos += 1;
    }
    None
}

/// Read the attributes of one `<meta ...>` tag (starting after `<meta`).
/// Returns how many bytes the tag used and the encoding it declares, if any.
fn meta_charset(bytes: &[u8]) -> (usize, Option<&'static Encoding>) {
    let mut pos = 0;
    let mut charset: Option<&'static Encoding> = None;
    let mut content_charset: Option<&'static Encoding> = None;
    let mut http_equiv_content_type = false;
    loop {
        let Some((name, value, next)) = next_attribute(bytes, pos) else {
            pos = bytes.len();
            break;
        };
        pos = next;
        if name.is_empty() {
            break; // reached '>'
        }
        if name.eq_ignore_ascii_case(b"charset") {
            charset = charset.or_else(|| Encoding::for_label(value.trim_ascii()));
        } else if name.eq_ignore_ascii_case(b"http-equiv") {
            http_equiv_content_type |= value.trim_ascii().eq_ignore_ascii_case(b"content-type");
        } else if name.eq_ignore_ascii_case(b"content") {
            content_charset = content_charset.or_else(|| charset_from_content(value));
        }
    }
    let found = charset.or(if http_equiv_content_type {
        content_charset
    } else {
        None
    });
    (pos, found)
}

/// One attribute starting at `pos`: returns (name, value, position after it).
/// An empty name means the tag ended. `None` means the input ended.
fn next_attribute(bytes: &[u8], mut pos: usize) -> Option<(&[u8], &[u8], usize)> {
    while pos < bytes.len() && (bytes[pos].is_ascii_whitespace() || bytes[pos] == b'/') {
        pos += 1;
    }
    match bytes.get(pos)? {
        b'>' => return Some((&[], &[], pos + 1)),
        _ => {}
    }
    let name_start = pos;
    while pos < bytes.len()
        && !matches!(bytes[pos], b'=' | b'>' | b'/')
        && !bytes[pos].is_ascii_whitespace()
    {
        pos += 1;
    }
    let name = &bytes[name_start..pos];
    while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
        pos += 1;
    }
    if bytes.get(pos) != Some(&b'=') {
        return Some((name, &[], pos));
    }
    pos += 1;
    while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
        pos += 1;
    }
    let value = match *bytes.get(pos)? {
        q @ (b'"' | b'\'') => {
            let start = pos + 1;
            let len = bytes[start..].iter().position(|&b| b == q)?;
            pos = start + len + 1;
            &bytes[start..start + len]
        }
        _ => {
            let start = pos;
            while pos < bytes.len() && bytes[pos] != b'>' && !bytes[pos].is_ascii_whitespace() {
                pos += 1;
            }
            &bytes[start..pos]
        }
    };
    Some((name, value, pos))
}

/// `text/html; charset=windows-1251` -> windows-1251.
fn charset_from_content(content: &[u8]) -> Option<&'static Encoding> {
    let lower = content.to_ascii_lowercase();
    let at = find(&lower, b"charset")?;
    let rest = content[at + 7..].trim_ascii_start();
    let rest = rest.strip_prefix(b"=")?.trim_ascii_start();
    let value = match rest.first()? {
        q @ (b'"' | b'\'') => {
            let end = rest[1..].iter().position(|b| b == q)?;
            &rest[1..1 + end]
        }
        _ => {
            let end = rest
                .iter()
                .position(|&b| b == b';' || b.is_ascii_whitespace())
                .unwrap_or(rest.len());
            &rest[..end]
        }
    };
    Encoding::for_label(value)
}

/// Skip one tag (or `<!...>`, `<?...>`) to just after its `>`, honoring quotes.
fn skip_tag(bytes: &[u8]) -> usize {
    let mut quote = None;
    for (i, &b) in bytes.iter().enumerate().skip(1) {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'>' => return i + 1,
            None => {}
        }
    }
    bytes.len()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_precedence_bom_transport_meta_then_detection() {
        // é in windows-1252 is 0xE9; in UTF-8 it is C3 A9.
        let meta_1252 = b"<html><head><!-- <meta charset=utf-8> --><meta charset=\"windows-1252\"></head>caf\xE9";
        assert!(decode_html(meta_1252, None).ends_with("café"));
        // The transport charset wins over <meta>.
        let utf8_body = "<meta charset=windows-1252>café".as_bytes();
        assert!(decode_html(utf8_body, Some("utf-8")).ends_with("café"));
        // The BOM wins over everything.
        let bom = b"\xEF\xBB\xBF<meta charset=windows-1252>caf\xC3\xA9";
        assert!(decode_html(bom, Some("iso-8859-2")).ends_with("café"));
        // http-equiv form.
        let equiv = b"<meta http-equiv='Content-Type' content='text/html; charset=koi8-r'>\xC1";
        assert!(decode_html(equiv, None).ends_with('а'));
        // content= without http-equiv does not count.
        let no_equiv = b"<meta content='text/html; charset=koi8-r'>\xC1";
        assert!(decode_html(no_equiv, None).ends_with('Á'));
        // No declaration: valid UTF-8 stays UTF-8, anything else is windows-1252.
        assert!(decode_html("<p>café".as_bytes(), None).ends_with("café"));
        assert!(decode_html(b"<p>caf\xE9", None).ends_with("café"));
    }

    #[test]
    fn meta_after_1024_bytes_is_ignored_and_utf16_declarations_mean_utf8() {
        let mut late = vec![b' '; 1100];
        late.extend_from_slice(b"<meta charset=koi8-r>\xC3\xA9");
        assert!(decode_html(&late, None).ends_with("é"));
        assert!(decode_html("<meta charset=utf-16le>é".as_bytes(), None).ends_with("é"));
    }

    #[test]
    fn a_document_in_pieces_decodes_like_the_whole() {
        // A <meta charset> in the first 1024 bytes, a split multi-byte
        // sequence, and a BOM split over two pieces.
        let mut meta = b"<meta charset=koi8-r>".to_vec();
        meta.extend(std::iter::repeat_n(b' ', 1100));
        meta.extend_from_slice(b"\xC1");
        let utf8 = "<p>café ok</p>".as_bytes();
        let bom = b"\xEF\xBB\xBF<p>caf\xC3\xA9";
        for (bytes, transport) in [
            (&meta[..], None),
            (utf8, None),
            (utf8, Some("utf-8")),
            (&bom[..], Some("koi8-r")),
        ] {
            for piece in [1, 2, 7, 1024, 4096] {
                let mut d = HtmlStreamDecoder::new(transport);
                let mut out = String::new();
                let chunks: Vec<&[u8]> = bytes.chunks(piece).collect();
                for (i, c) in chunks.iter().enumerate() {
                    d.push(c, i + 1 == chunks.len(), &mut out);
                }
                assert_eq!(out, decode_html(bytes, transport), "piece {piece}");
            }
        }
    }

    #[test]
    fn css_charset_rule_and_transport() {
        assert_eq!(
            decode_text(b"@charset \"iso-8859-1\"; a{content:'\xE9'}", None),
            "@charset \"iso-8859-1\"; a{content:'\u{e9}'}"
        );
        assert_eq!(
            decode_text(b"a{content:'\xE9'}", Some("latin1")),
            "a{content:'\u{e9}'}"
        );
        assert_eq!(
            decode_text(b"a{content:'\xE9'}", None),
            "a{content:'\u{FFFD}'}"
        );
    }
}
