//! `Content-Encoding`. Huginn asks for `gzip, deflate, br` and decodes a
//! body after reading all of it. A streaming decoder that reads from the
//! socket can stop at the end of the compressed data and leave the end of a
//! chunked body unread; then the connection is closed instead of used again
//! (ureq did that: on brotli sites every request paid for a new TCP and TLS
//! handshake). [`StreamDecoder`] is pushed every frame instead, so the body
//! is still read to its end; the document uses it, so the preload scanner
//! sees its text while the rest is on the way.

use std::io::{Read, Write};

/// `Accept-Encoding` of every request.
pub(crate) const ACCEPT_ENCODING: &str = "gzip, deflate, br";

/// Undo `encoding` (codings in the order they were applied, `gzip, br`).
/// Unknown codings are left as they are. The result may be
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

/// Undoes one `Content-Encoding` while a body arrives: push each frame,
/// read what is decoded so far.
pub(crate) struct StreamDecoder {
    inner: Inner,
    limit: u64,
}

enum Inner {
    Identity(Vec<u8>),
    Gzip(flate2::write::MultiGzDecoder<Vec<u8>>),
    Brotli(Box<brotli_decompressor::DecompressorWriter<Vec<u8>>>),
}

impl StreamDecoder {
    /// A decoder for `encoding`, or `None` when the body has to be decoded
    /// at the end with [`decode`]: several codings, and `deflate` (zlib or
    /// raw is only known by trying). Unknown codings are left as they are,
    /// like [`decode`] does.
    pub(crate) fn new(encoding: Option<&str>, limit: u64) -> Option<StreamDecoder> {
        let coding = encoding.map(str::trim).unwrap_or("").to_ascii_lowercase();
        let inner = match coding.as_str() {
            _ if coding.contains(',') => return None,
            "gzip" | "x-gzip" => Inner::Gzip(flate2::write::MultiGzDecoder::new(Vec::new())),
            "br" => Inner::Brotli(Box::new(brotli_decompressor::DecompressorWriter::new(
                Vec::new(),
                4096,
            ))),
            "deflate" => return None,
            _ => Inner::Identity(Vec::new()),
        };
        Some(StreamDecoder { inner, limit })
    }

    /// Decode `raw`, the next bytes of the body. Returns everything decoded
    /// so far.
    pub(crate) fn push(&mut self, raw: &[u8]) -> Result<&[u8], String> {
        let out = match &mut self.inner {
            Inner::Identity(v) => {
                v.extend_from_slice(raw);
                &v[..]
            }
            Inner::Gzip(d) => {
                d.write_all(raw)
                    .and_then(|()| d.flush())
                    .map_err(|e| format!("gzip: {e}"))?;
                &d.get_ref()[..]
            }
            Inner::Brotli(d) => {
                d.write_all(raw)
                    .and_then(|()| d.flush())
                    .map_err(|e| format!("br: {e}"))?;
                &d.get_ref()[..]
            }
        };
        if out.len() as u64 > self.limit {
            return Err(format!("body larger than {} bytes", self.limit));
        }
        Ok(out)
    }

    /// The end of the body: the whole decoded body. Truncated compressed
    /// data is an error.
    pub(crate) fn finish(self) -> Result<Vec<u8>, String> {
        let out = match self.inner {
            Inner::Identity(v) => v,
            Inner::Gzip(d) => d.finish().map_err(|e| format!("gzip: {e}"))?,
            Inner::Brotli(mut d) => {
                d.close().map_err(|e| format!("br: {e}"))?;
                d.into_inner().map_err(|_| "br: truncated".to_owned())?
            }
        };
        if out.len() as u64 > self.limit {
            return Err(format!("body larger than {} bytes", self.limit));
        }
        Ok(out)
    }
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

    /// Push `raw` in pieces of `piece` bytes; the decoded text after each.
    fn stream(coding: &str, raw: &[u8], piece: usize, limit: u64) -> Result<Vec<u8>, String> {
        let mut d = StreamDecoder::new(Some(coding), limit).expect("streamable");
        let mut seen = 0;
        for c in raw.chunks(piece) {
            let out = d.push(c)?;
            // What came out so far only grows.
            assert!(out.len() >= seen);
            seen = out.len();
        }
        d.finish()
    }

    #[test]
    fn a_body_pushed_in_pieces_decodes_like_the_whole() {
        let text: Vec<u8> = (0..20_000u32).flat_map(|i| i.to_le_bytes()).collect();
        let mut two = gzip(&text[..1000]);
        two.extend(gzip(&text[1000..]));
        // "hi", compressed with `brotli -q 0`.
        let br = [0x8f, 0x00, 0x80, 0x68, 0x69, 0x03];
        for piece in [1, 7, 4096, usize::MAX] {
            assert_eq!(stream("gzip", &gzip(&text), piece, u64::MAX).unwrap(), text);
            assert_eq!(stream("x-gzip", &two, piece, u64::MAX).unwrap(), text);
            assert_eq!(stream("br", &br, piece, 100).unwrap(), b"hi");
            assert_eq!(stream("identity", b"plain", piece, 100).unwrap(), b"plain");
        }
        // The end of the compressed data is missing, or it is too big.
        let whole = gzip(&text);
        assert!(stream("gzip", &whole[..whole.len() - 10], 512, u64::MAX).is_err());
        assert!(stream("br", &br[..4], 1, 100).is_err());
        assert!(stream("gzip", &whole, 512, 1000).is_err());
        // Several codings, and deflate (zlib or raw), wait for the end.
        assert!(StreamDecoder::new(Some("gzip, br"), 100).is_none());
        assert!(StreamDecoder::new(Some("deflate"), 100).is_none());
    }
}
