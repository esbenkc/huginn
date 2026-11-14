//! `Content-Encoding`. Huginn asks for `gzip, deflate, br` and decodes a
//! body after reading all of it. ureq's own streaming decoders stopped at
//! the end of the compressed data and left the end of a chunked body
//! unread, so the connection was closed instead of used again: on brotli
//! sites every request paid for a new TCP and TLS handshake.

use std::io::Read;

/// `Accept-Encoding` of every request.
pub(crate) const ACCEPT_ENCODING: &str = "gzip, deflate, br";

/// Undo `encoding` (codings in the order they were applied, `gzip, br`).
/// Unknown codings are left as they are, like ureq did. The result may be
/// at most `limit` bytes.
pub(crate) fn decode(encoding: Option<&str>, raw: Vec<u8>, limit: u64) -> Result<Vec<u8>, String> {
    let Some(encoding) = encoding else {
        return Ok(raw);
    };
    let mut body = raw;
    for coding in encoding.rsplit(',').map(str::trim) {
        let mut out = Vec::with_capacity(body.len().saturating_mul(4));
        let read = match coding.to_ascii_lowercase().as_str() {
            "gzip" | "x-gzip" => flate2::read::MultiGzDecoder::new(&body[..])
                .take(limit + 1)
                .read_to_end(&mut out),
            // `deflate` is zlib-wrapped, but some servers send raw deflate
            // (Chrome takes both).
            "deflate" => flate2::read::ZlibDecoder::new(&body[..])
                .take(limit + 1)
                .read_to_end(&mut out)
                .or_else(|_| {
                    out.clear();
                    flate2::read::DeflateDecoder::new(&body[..])
                        .take(limit + 1)
                        .read_to_end(&mut out)
                }),
            "br" => brotli_decompressor::Decompressor::new(&body[..], 4096)
                .take(limit + 1)
                .read_to_end(&mut out),
            _ => continue,
        };
        read.map_err(|e| format!("{coding}: {e}"))?;
        if out.len() as u64 > limit {
            return Err(format!("{coding}: body larger than {limit} bytes"));
        }
        body = out;
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn decodes_each_coding_in_reverse_order() {
        assert_eq!(decode(None, b"plain".to_vec(), 100).unwrap(), b"plain");
        assert_eq!(
            decode(Some("identity"), b"plain".to_vec(), 100).unwrap(),
            b"plain"
        );
        assert_eq!(decode(Some("gzip"), gzip(b"hello"), 100).unwrap(), b"hello");
        assert_eq!(
            decode(Some("gzip, gzip"), gzip(&gzip(b"twice")), 100).unwrap(),
            b"twice"
        );
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        z.write_all(b"zlib").unwrap();
        assert_eq!(
            decode(Some("deflate"), z.finish().unwrap(), 100).unwrap(),
            b"zlib"
        );
        let mut d = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        d.write_all(b"raw").unwrap();
        assert_eq!(
            decode(Some("deflate"), d.finish().unwrap(), 100).unwrap(),
            b"raw"
        );
        // "hi", compressed with `brotli -q 0`.
        let br = [0x8f, 0x00, 0x80, 0x68, 0x69, 0x03];
        assert_eq!(decode(Some("br"), br.to_vec(), 100).unwrap(), b"hi");
    }

    #[test]
    fn rejects_bombs_and_garbage() {
        let big = gzip(&vec![0; 10_000]);
        assert!(decode(Some("gzip"), big.clone(), 9_999).is_err());
        assert_eq!(decode(Some("gzip"), big, 10_000).unwrap().len(), 10_000);
        assert!(decode(Some("gzip"), b"not gzip".to_vec(), 100).is_err());
    }
}
