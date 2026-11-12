//! Native byte work for the prelude: text encodings for `TextEncoder` and
//! `TextDecoder` (UTF-8 here, the legacy encodings through encoding_rs), and
//! the deflate and gzip codecs behind `CompressionStream` and
//! `DecompressionStream` (flate2). These loops would be slow in JavaScript:
//! the isolate runs in lite mode, without a JIT.

use std::borrow::Cow;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};

use encoding_rs::Encoding;
use flate2::Compression;
use flate2::write::{
    DeflateDecoder, DeflateEncoder, GzDecoder, GzEncoder, ZlibDecoder, ZlibEncoder,
};

use super::ops::state;

/// A `CompressionStream` or `DecompressionStream` in progress. Encoders and
/// decoders write into a `Vec` that each step drains.
pub(crate) enum Codec {
    Gzip(GzEncoder<Vec<u8>>),
    Deflate(ZlibEncoder<Vec<u8>>),
    DeflateRaw(DeflateEncoder<Vec<u8>>),
    Gunzip(GzDecoder<Vec<u8>>),
    Inflate(ZlibDecoder<Vec<u8>>),
    InflateRaw(DeflateDecoder<Vec<u8>>),
}

macro_rules! each {
    ($c:expr, $w:ident => $e:expr) => {
        match $c {
            Codec::Gzip($w) => $e,
            Codec::Deflate($w) => $e,
            Codec::DeflateRaw($w) => $e,
            Codec::Gunzip($w) => $e,
            Codec::Inflate($w) => $e,
            Codec::InflateRaw($w) => $e,
        }
    };
}

impl Codec {
    /// `format` is a CompressionFormat: "gzip", "deflate" (zlib) or
    /// "deflate-raw".
    pub(crate) fn new(format: &str, decompress: bool) -> Option<Codec> {
        let level = Compression::default();
        Some(match (format, decompress) {
            ("gzip", false) => Codec::Gzip(GzEncoder::new(Vec::new(), level)),
            ("deflate", false) => Codec::Deflate(ZlibEncoder::new(Vec::new(), level)),
            ("deflate-raw", false) => Codec::DeflateRaw(DeflateEncoder::new(Vec::new(), level)),
            ("gzip", true) => Codec::Gunzip(GzDecoder::new(Vec::new())),
            ("deflate", true) => Codec::Inflate(ZlibDecoder::new(Vec::new())),
            ("deflate-raw", true) => Codec::InflateRaw(DeflateDecoder::new(Vec::new())),
            _ => return None,
        })
    }

    fn decompresses(&self) -> bool {
        matches!(
            self,
            Codec::Gunzip(_) | Codec::Inflate(_) | Codec::InflateRaw(_)
        )
    }

    /// Feed `input`; returns the output produced so far. Decoders flush, so
    /// decompressed data is not held back; encoders do not, because a flush
    /// would add sync markers to the compressed stream.
    pub(crate) fn push(&mut self, input: &[u8]) -> std::io::Result<Vec<u8>> {
        let flush = self.decompresses();
        each!(self, w => {
            w.write_all(input)?;
            if flush {
                w.flush()?;
            }
            Ok(std::mem::take(w.get_mut()))
        })
    }

    /// End of input: the rest of the output. Decoders fail on a truncated
    /// stream.
    pub(crate) fn finish(mut self) -> std::io::Result<Vec<u8>> {
        each!(&mut self, w => {
            w.try_finish()?;
            Ok(std::mem::take(w.get_mut()))
        })
    }
}

/// The WHATWG name (lowercase) of the encoding `label` names, or `None` for
/// an unknown label and for "replacement", which `TextDecoder` rejects.
pub(crate) fn encoding_for_label(label: &str) -> Option<String> {
    let e = Encoding::for_label(label.as_bytes())?;
    (e != encoding_rs::REPLACEMENT).then(|| e.name().to_ascii_lowercase())
}

/// Decode `bytes` in `encoding` (a name from [`encoding_for_label`]).
/// Malformed input becomes U+FFFD, or `Err` with `fatal`. With `stream`, an
/// incomplete sequence at the end is left undecoded and its length returned,
/// so the caller can put it in front of the next chunk. No BOM handling.
pub(crate) fn decode<'a>(
    bytes: &'a [u8],
    encoding: &str,
    fatal: bool,
    stream: bool,
) -> Result<(Cow<'a, str>, usize), ()> {
    match encoding {
        "utf-8" => decode_utf8(bytes, fatal, stream),
        "utf-16le" => decode_utf16(bytes, true, fatal, stream),
        "utf-16be" => decode_utf16(bytes, false, fatal, stream),
        _ => {
            let enc = Encoding::for_label(encoding.as_bytes()).ok_or(())?;
            let (text, had_errors) = enc.decode_without_bom_handling(bytes);
            if fatal && had_errors {
                Err(())
            } else {
                Ok((text, 0))
            }
        }
    }
}

/// UTF-8 with the Encoding Standard's error handling: each maximal invalid
/// subpart becomes one U+FFFD (as in `String::from_utf8_lossy`).
fn decode_utf8(bytes: &[u8], fatal: bool, stream: bool) -> Result<(Cow<'_, str>, usize), ()> {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return Ok((Cow::Borrowed(s), 0));
    }
    let mut out = String::with_capacity(bytes.len() + 3);
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                return Ok((Cow::Owned(out), 0));
            }
            Err(e) => {
                let (good, bad) = rest.split_at(e.valid_up_to());
                out.push_str(std::str::from_utf8(good).unwrap_or_default());
                match e.error_len() {
                    Some(n) => {
                        if fatal {
                            return Err(());
                        }
                        out.push('\u{FFFD}');
                        rest = &bad[n..];
                    }
                    // The input ends inside a sequence.
                    None if stream => return Ok((Cow::Owned(out), bad.len())),
                    None if fatal => return Err(()),
                    None => {
                        out.push('\u{FFFD}');
                        return Ok((Cow::Owned(out), 0));
                    }
                }
            }
        }
    }
}

fn decode_utf16(
    bytes: &[u8],
    le: bool,
    fatal: bool,
    stream: bool,
) -> Result<(Cow<'_, str>, usize), ()> {
    let unit = |i: usize| {
        let b = [bytes[2 * i], bytes[2 * i + 1]];
        if le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        }
    };
    let mut n = bytes.len() / 2;
    let mut held = bytes.len() % 2;
    if stream && n > 0 && (0xD800..0xDC00).contains(&unit(n - 1)) {
        n -= 1;
        held += 2;
    }
    let mut out = String::with_capacity(n);
    for r in char::decode_utf16((0..n).map(unit)) {
        match r {
            Ok(c) => out.push(c),
            Err(_) if fatal => return Err(()),
            Err(_) => out.push('\u{FFFD}'),
        }
    }
    if !stream && held > 0 {
        if fatal {
            return Err(());
        }
        out.push('\u{FFFD}');
        held = 0;
    }
    Ok((Cow::Owned(out), held))
}

// ------------------------------------------------------------------ ops --

type Rv<'a> = v8::ReturnValue<'a, v8::Value>;

fn throw_type_error(scope: &mut v8::PinScope<'_, '_>, msg: &str) {
    if let Some(m) = v8::String::new(scope, msg) {
        let e = v8::Exception::type_error(scope, m);
        scope.throw_exception(e);
    }
}

/// A native callback with panic protection (see `op!` in ops.rs).
macro_rules! op {
    ($name:ident($scope:ident, $args:ident, $rv:ident) $body:block) => {
        #[allow(unused_variables, unused_mut)]
        fn $name(
            $scope: &mut v8::PinScope,
            $args: v8::FunctionCallbackArguments,
            mut $rv: v8::ReturnValue<v8::Value>,
        ) {
            let r = catch_unwind(AssertUnwindSafe(|| $body));
            if r.is_err() {
                throw_type_error(
                    $scope,
                    concat!("huginn internal error in ", stringify!($name)),
                );
            }
        }
    };
}

/// A new `Uint8Array` holding a copy of `bytes`.
fn set_bytes(scope: &mut v8::PinScope<'_, '_>, rv: &mut Rv<'_>, bytes: &[u8]) {
    let ab = v8::ArrayBuffer::new(scope, bytes.len());
    if !bytes.is_empty() {
        let store = ab.get_backing_store();
        let Some(ptr) = store.data() else { return };
        // SAFETY: the buffer was just allocated with `bytes.len()` bytes and
        // nothing else references it yet.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.as_ptr().cast::<u8>(), bytes.len())
        };
    }
    if let Some(a) = v8::Uint8Array::new(scope, ab, 0, bytes.len()) {
        rv.set(a.into());
    }
}

/// An ArrayBufferView argument, or return with a TypeError.
macro_rules! view_arg {
    ($scope:ident, $args:ident, $i:expr) => {
        match v8::Local::<v8::ArrayBufferView>::try_from($args.get($i)) {
            Ok(v) => v,
            Err(_) => {
                return throw_type_error(
                    $scope,
                    "The provided value is not of type 'ArrayBufferView'.",
                )
            }
        }
    };
}

// `TextEncoder.encode`: UTF-8, lone surrogates as U+FFFD.
op!(op_utf8_encode(scope, args, rv) {
    let s = args.get(0).to_rust_string_lossy(scope);
    set_bytes(scope, &mut rv, s.as_bytes());
});

op!(op_enc_label(scope, args, rv) {
    let label = args.get(0).to_rust_string_lossy(scope);
    match encoding_for_label(&label).and_then(|n| v8::String::new(scope, &n)) {
        Some(s) => rv.set(s.into()),
        None => rv.set_null(),
    }
});

// `N.decode(view, encoding, fatal, stream)`: the text, or [text, n] when the
// last n bytes are an incomplete sequence held back in stream mode.
op!(op_decode(scope, args, rv) {
    let view = view_arg!(scope, args, 0);
    let mut storage = [0u8; v8::TYPED_ARRAY_MAX_SIZE_IN_HEAP];
    let bytes = view.get_contents(&mut storage);
    let encoding = args.get(1).to_rust_string_lossy(scope);
    let fatal = args.get(2).is_true();
    let stream = args.get(3).is_true();
    let Ok((text, held)) = decode(bytes, &encoding, fatal, stream) else {
        return throw_type_error(scope, "The encoded data was not valid.");
    };
    let Some(s) = v8::String::new(scope, &text) else {
        return throw_type_error(scope, "The decoded string is too long.");
    };
    if held == 0 {
        rv.set(s.into());
    } else {
        let n = v8::Integer::new_from_unsigned(scope, held as u32).into();
        rv.set(v8::Array::new_with_elements(scope, &[s.into(), n]).into());
    }
});

op!(op_codec_new(scope, args, rv) {
    let format = args.get(0).to_rust_string_lossy(scope);
    let Some(codec) = Codec::new(&format, args.get(1).is_true()) else {
        return throw_type_error(scope, "Unsupported compression format");
    };
    let st = state(scope);
    let mut st = st.borrow_mut();
    st.next_codec += 1;
    let id = st.next_codec;
    st.codecs.insert(id, codec);
    rv.set_uint32(id);
});

op!(op_codec_push(scope, args, rv) {
    let id = args.get(0).uint32_value(scope).unwrap_or(0);
    let view = view_arg!(scope, args, 1);
    let mut storage = [0u8; v8::TYPED_ARRAY_MAX_SIZE_IN_HEAP];
    let bytes = view.get_contents(&mut storage);
    let st = state(scope);
    let r = match st.borrow_mut().codecs.get_mut(&id) {
        Some(c) => c.push(bytes),
        None => return throw_type_error(scope, "The stream is closed"),
    };
    match r {
        Ok(out) => set_bytes(scope, &mut rv, &out),
        Err(e) => {
            st.borrow_mut().codecs.remove(&id);
            throw_type_error(scope, &format!("Compression error: {e}"));
        }
    }
});

op!(op_codec_finish(scope, args, rv) {
    let id = args.get(0).uint32_value(scope).unwrap_or(0);
    let codec = state(scope).borrow_mut().codecs.remove(&id);
    let Some(codec) = codec else {
        return throw_type_error(scope, "The stream is closed");
    };
    match codec.finish() {
        Ok(out) => set_bytes(scope, &mut rv, &out),
        Err(e) => throw_type_error(scope, &format!("Compression error: {e}")),
    }
});

op!(op_codec_drop(scope, args, rv) {
    let id = args.get(0).uint32_value(scope).unwrap_or(0);
    state(scope).borrow_mut().codecs.remove(&id);
});

/// Add the ops above to the prelude's `N` object.
pub(super) fn install(scope: &mut v8::PinScope<'_, '_>, obj: v8::Local<'_, v8::Object>) {
    macro_rules! install {
        ($($name:literal => $f:ident),* $(,)?) => {
            $(
                if let (Some(key), Some(func)) = (v8::String::new(scope, $name), v8::Function::new(scope, $f)) {
                    obj.set(scope, key.into(), func.into());
                }
            )*
        };
    }
    install! {
        "utf8Encode" => op_utf8_encode, "encLabel" => op_enc_label, "decode" => op_decode,
        "codecNew" => op_codec_new, "codecPush" => op_codec_push, "codecFinish" => op_codec_finish,
        "codecDrop" => op_codec_drop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(bytes: &[u8], enc: &str, fatal: bool, stream: bool) -> Result<(String, usize), ()> {
        decode(bytes, enc, fatal, stream).map(|(s, n)| (s.into_owned(), n))
    }

    #[test]
    fn utf8_replaces_maximal_subparts() {
        // F0 80: 80 is not a valid second byte after F0, so two U+FFFD.
        assert_eq!(
            dec(b"a\xF0\x80b", "utf-8", false, false),
            Ok(("a\u{FFFD}\u{FFFD}b".into(), 0))
        );
        // A truncated sequence at the end is one U+FFFD.
        assert_eq!(
            dec(b"a\xE2\x82", "utf-8", false, false),
            Ok(("a\u{FFFD}".into(), 0))
        );
        assert_eq!(dec(b"a\xE2\x82", "utf-8", true, false), Err(()));
        assert_eq!(dec(b"\xC0\x80", "utf-8", true, false), Err(()));
    }

    #[test]
    fn utf8_stream_holds_incomplete_tail() {
        // "€" is E2 82 AC; split after two bytes.
        assert_eq!(dec(b"x\xE2\x82", "utf-8", false, true), Ok(("x".into(), 2)));
        assert_eq!(
            dec(b"\xE2\x82\xAC", "utf-8", true, true),
            Ok(("€".into(), 0))
        );
        // An invalid byte is not held back, even at the end.
        assert_eq!(
            dec(b"x\xFF", "utf-8", false, true),
            Ok(("x\u{FFFD}".into(), 0))
        );
    }

    #[test]
    fn utf16_stream_holds_half_units_and_lead_surrogates() {
        // U+1F600 in UTF-16LE: 3D D8 00 DE.
        assert_eq!(
            dec(b"a\x00\x3D", "utf-16le", false, true),
            Ok(("a".into(), 1))
        );
        assert_eq!(
            dec(b"a\x00\x3D\xD8", "utf-16le", false, true),
            Ok(("a".into(), 2))
        );
        assert_eq!(
            dec(b"\x3D\xD8\x00\xDE", "utf-16le", false, false),
            Ok(("\u{1F600}".into(), 0))
        );
        assert_eq!(
            dec(b"\x00a\x00", "utf-16be", false, false),
            Ok(("a\u{FFFD}".into(), 0))
        );
        assert_eq!(dec(b"\x3D\xD8", "utf-16le", true, false), Err(()));
    }

    #[test]
    fn legacy_encodings_and_labels() {
        assert_eq!(
            encoding_for_label(" Latin1 ").as_deref(),
            Some("windows-1252")
        );
        assert_eq!(encoding_for_label("utf8").as_deref(), Some("utf-8"));
        assert_eq!(encoding_for_label("sjis").as_deref(), Some("shift_jis"));
        assert_eq!(encoding_for_label("iso-2022-kr"), None);
        assert_eq!(encoding_for_label("nonsense"), None);
        // 0x80 is the euro sign in windows-1252, not U+0080.
        assert_eq!(
            dec(b"\x80", "windows-1252", false, false),
            Ok(("€".into(), 0))
        );
        assert_eq!(
            dec(b"\x82\xA0", "shift_jis", false, false),
            Ok(("あ".into(), 0))
        );
    }

    #[test]
    fn codecs_round_trip_and_reject_truncation() {
        let text = b"hello hello hello hello hello".repeat(50);
        for format in ["gzip", "deflate", "deflate-raw"] {
            let mut enc = Codec::new(format, false).unwrap();
            let mut packed = enc.push(&text[..100]).unwrap();
            packed.extend(enc.push(&text[100..]).unwrap());
            packed.extend(enc.finish().unwrap());
            assert!(packed.len() < text.len(), "{format} compresses");

            let mut dec = Codec::new(format, true).unwrap();
            let mut out = Vec::new();
            for chunk in packed.chunks(7) {
                out.extend(dec.push(chunk).unwrap());
            }
            out.extend(dec.finish().unwrap());
            assert_eq!(out, text, "{format} round trip");

            let mut cut = Codec::new(format, true).unwrap();
            let _ = cut.push(&packed[..packed.len() / 2]);
            assert!(
                cut.finish().is_err(),
                "{format} truncated input is an error"
            );
        }
        assert!(Codec::new("brotli", false).is_none());
    }
}
