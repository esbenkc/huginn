//! huginn-net: fetching and resource decoding.
//!
//! - [`Loader`] fetches `http:`, `https:`, `file:` and `data:` URLs. It keeps
//!   an in-memory cache keyed by URL, so a resource used twice is fetched once.
//!   [`Loader::fetch_all`] fetches many URLs in parallel on scoped threads.
//!   Images and fonts go through [`Loader::fetch_uncached`]: their bodies are
//!   decoded on the fetching thread and dropped right away.
//! - [`decode_image`] decodes PNG/JPEG/GIF/WebP/BMP/ICO and SVG into the
//!   rasterizer's pixel type (`vello_common::pixmap::Pixmap`, premultiplied
//!   RGBA8). Big images shown much smaller are downscaled while decoding. The
//!   decoded pixels are stored once and shared by layout (for the natural
//!   size) and paint (for drawing). There is no second copy.
//! - [`decode_font`] turns WOFF/WOFF2 into TrueType/OpenType.
//! - [`decode_html`]/[`decode_text`] pick the character encoding (BOM,
//!   `Content-Type`, `<meta charset>`, `@charset`).
//! - [`media_matches`], [`parse_srcset`], [`eval_sizes`] and
//!   [`choose_candidate`] select responsive images and media-dependent links.

use std::collections::HashMap;
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use url::Url;
pub use vello_common::pixmap::Pixmap;

mod charset;
mod font;
mod media;
mod srcset;
mod svg;

pub use charset::{decode_html, decode_text};
pub use font::{FontFormat, decode_font, font_format, size_adjust_font};
pub use media::{Screen, media_matches, parse_length};
pub use srcset::{Descriptor, SrcsetCandidate, choose_candidate, eval_sizes, parse_srcset};
pub use svg::{decode_svg, is_svg};

/// What a fetch is for. Used for `Accept` headers and, later, priorities.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ResourceKind {
    Document,
    Stylesheet,
    Script,
    Image,
    Font,
    Other,
}

/// A fetched resource.
#[derive(Debug)]
pub struct Resource {
    /// Final URL after redirects. Use it as the base URL of the resource.
    pub url: Url,
    /// HTTP status, or 200 for `file:`/`data:`.
    pub status: u16,
    /// MIME type from the `Content-Type` header or `data:` URL, without parameters.
    pub content_type: Option<String>,
    /// The `charset` parameter of the `Content-Type`, if any (as written).
    pub charset: Option<String>,
    pub body: Vec<u8>,
}

impl Resource {
    /// Body as text: BOM, then the `charset` parameter, then a leading
    /// `@charset "...";` rule, then UTF-8 (invalid bytes replaced). Use for
    /// stylesheets and scripts.
    pub fn text(&self) -> std::borrow::Cow<'_, str> {
        decode_text(&self.body, self.charset.as_deref())
    }

    /// Body as an HTML document: BOM, then the `charset` parameter, then
    /// `<meta charset>` in the first 1024 bytes, then UTF-8 or windows-1252.
    pub fn html_text(&self) -> std::borrow::Cow<'_, str> {
        decode_html(&self.body, self.charset.as_deref())
    }
}

#[derive(Debug, Clone)]
pub enum NetError {
    InvalidUrl(String),
    UnsupportedScheme(String),
    Io(String),
    Http { url: String, status: u16 },
    Transport(String),
    Decode(String),
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::InvalidUrl(u) => write!(f, "invalid URL: {u}"),
            NetError::UnsupportedScheme(s) => write!(f, "unsupported URL scheme: {s}"),
            NetError::Io(e) => write!(f, "I/O error: {e}"),
            NetError::Http { url, status } => write!(f, "HTTP {status} for {url}"),
            NetError::Transport(e) => write!(f, "transport error: {e}"),
            NetError::Decode(e) => write!(f, "decode error: {e}"),
        }
    }
}

impl std::error::Error for NetError {}

/// Max body size accepted for one resource (64 MiB).
const MAX_BODY: u64 = 64 << 20;
/// Max parallel fetches in [`Loader::fetch_all`]. Same as Chrome's per-host HTTP/1.1 limit.
const MAX_PARALLEL: usize = 6;

/// Fetches resources and caches them in memory for the life of the loader.
///
/// `Loader` is `Sync`: share one per page (or per process) by reference.
pub struct Loader {
    agent: ureq::Agent,
    cache: Mutex<HashMap<Url, Arc<Resource>>>,
}

impl Default for Loader {
    fn default() -> Self {
        Self::new()
    }
}

impl Loader {
    /// A loader. `HUGINN_NET_LOOPBACK_ONLY=1` in the environment makes it
    /// [`Loader::loopback_only`]. The benchmark sets it so Huginn is offline
    /// there like Chrome and Ladybird.
    pub fn new() -> Loader {
        Loader::build(std::env::var_os("HUGINN_NET_LOOPBACK_ONLY").is_some_and(|v| v == "1"))
    }

    /// A loader that reaches only 127.0.0.1, localhost and ::1. Every other
    /// host fails at once as "host not found" (no DNS lookup, no connect,
    /// redirects included).
    pub fn loopback_only() -> Loader {
        Loader::build(true)
    }

    fn build(loopback_only: bool) -> Loader {
        let config = ureq::Agent::config_builder()
            .user_agent(if cfg!(target_os = "macos") {
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Huginn/0.1"
            } else {
                "Mozilla/5.0 (X11; Linux x86_64) Huginn/0.1"
            })
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .build();
        let agent = if loopback_only {
            ureq::Agent::with_parts(
                config,
                ureq::unversioned::transport::DefaultConnector::default(),
                LoopbackOnly::default(),
            )
        } else {
            ureq::Agent::new_with_config(config)
        };
        Loader {
            agent,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Fetch one URL. Cached results are returned without a new request.
    /// Non-2xx HTTP responses are errors.
    pub fn fetch(&self, url: &Url, kind: ResourceKind) -> Result<Arc<Resource>, NetError> {
        if let Some(hit) = self.cache.lock().expect("cache poisoned").get(url) {
            return Ok(hit.clone());
        }
        let res = Arc::new(self.fetch_uncached(url, kind)?);
        self.cache
            .lock()
            .expect("cache poisoned")
            .insert(url.clone(), res.clone());
        Ok(res)
    }

    /// Fetch many URLs in parallel (at most 6 at a time). Results are in input order.
    pub fn fetch_all(
        &self,
        requests: &[(Url, ResourceKind)],
    ) -> Vec<Result<Arc<Resource>, NetError>> {
        self.map_parallel(requests, |(url, kind)| self.fetch(url, *kind))
    }

    /// Drop every cached response. Call when the cached bodies (document,
    /// stylesheets) are parsed and no longer needed; later fetches go to the
    /// network again.
    pub fn clear_cache(&self) {
        let old = std::mem::take(&mut *self.cache.lock().expect("cache poisoned"));
        drop(old);
    }

    /// Run `f` over `items` on up to 6 scoped threads; results keep input order.
    /// Used for fetching and for decoding fetched images off the main thread.
    pub fn map_parallel<T: Sync, R: Send>(
        &self,
        items: &[T],
        f: impl Fn(&T) -> R + Sync,
    ) -> Vec<R> {
        if items.len() <= 1 {
            return items.iter().map(&f).collect();
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
        std::thread::scope(|s| {
            for _ in 0..MAX_PARALLEL.min(items.len()) {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= items.len() {
                            break;
                        }
                        let r = f(&items[i]);
                        *results[i].lock().expect("result slot poisoned") = Some(r);
                    }
                });
            }
        });
        results
            .into_iter()
            .map(|m| {
                m.into_inner()
                    .expect("poisoned")
                    .expect("every slot is filled")
            })
            .collect()
    }

    /// Fetch without touching the cache. Use for bodies that are decoded once
    /// and then dropped (images, fonts), so they do not stay in memory.
    pub fn fetch_uncached(&self, url: &Url, kind: ResourceKind) -> Result<Resource, NetError> {
        match url.scheme() {
            "file" => {
                let path = url
                    .to_file_path()
                    .map_err(|_| NetError::InvalidUrl(url.to_string()))?;
                let body = std::fs::read(&path)
                    .map_err(|e| NetError::Io(format!("{}: {e}", path.display())))?;
                Ok(Resource {
                    url: url.clone(),
                    status: 200,
                    content_type: mime_from_path(&path).map(str::to_owned),
                    charset: None,
                    body,
                })
            }
            "data" => {
                let data = data_url::DataUrl::process(url.as_str())
                    .map_err(|e| NetError::InvalidUrl(format!("{url}: {e:?}")))?;
                let (body, _) = data
                    .decode_to_vec()
                    .map_err(|e| NetError::Decode(format!("data URL: {e:?}")))?;
                let mime = data.mime_type();
                let charset = mime.get_parameter("charset").map(str::to_owned);
                Ok(Resource {
                    url: url.clone(),
                    status: 200,
                    content_type: Some(format!("{}/{}", mime.type_, mime.subtype)),
                    charset,
                    body,
                })
            }
            "http" | "https" => self.fetch_http(url, kind),
            other => Err(NetError::UnsupportedScheme(other.to_owned())),
        }
    }

    /// POST `body` to an http(s) `url` (a form submission) and return the
    /// document it answers with. Never cached.
    pub fn post_document(
        &self,
        url: &Url,
        content_type: &str,
        body: &[u8],
    ) -> Result<Resource, NetError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(NetError::UnsupportedScheme(url.scheme().to_owned()));
        }
        let resp = self
            .agent
            .post(url.as_str())
            .header("Accept", "text/html,application/xhtml+xml,*/*;q=0.8")
            .header("Content-Type", content_type)
            .send(body)
            .map_err(|e| NetError::Transport(format!("{url}: {e}")))?;
        read_response(url, resp)
    }

    fn fetch_http(&self, url: &Url, kind: ResourceKind) -> Result<Resource, NetError> {
        let accept = match kind {
            ResourceKind::Document => "text/html,application/xhtml+xml,*/*;q=0.8",
            ResourceKind::Stylesheet => "text/css,*/*;q=0.1",
            ResourceKind::Image => "image/avif,image/webp,image/png,image/*,*/*;q=0.8",
            ResourceKind::Font => "font/woff2,font/woff,font/ttf,*/*;q=0.5",
            ResourceKind::Script | ResourceKind::Other => "*/*",
        };
        let resp = self
            .agent
            .get(url.as_str())
            .header("Accept", accept)
            .call()
            .map_err(|e| NetError::Transport(format!("{url}: {e}")))?;
        read_response(url, resp)
    }
}

/// Status check, final URL, content type and body of an HTTP response.
fn read_response(
    url: &Url,
    mut resp: ureq::http::Response<ureq::Body>,
) -> Result<Resource, NetError> {
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(NetError::Http {
            url: url.to_string(),
            status,
        });
    }
    let final_url = {
        use ureq::ResponseExt;
        Url::parse(&resp.get_uri().to_string()).unwrap_or_else(|_| url.clone())
    };
    let header = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok());
    let content_type = header.map(|v| {
        v.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    });
    let charset = header.and_then(charset_param).map(str::to_owned);
    // Size the buffer from Content-Length so read_to_end does not double it.
    let mut body =
        Vec::with_capacity(resp.body().content_length().unwrap_or(0).min(MAX_BODY) as usize);
    resp.body_mut()
        .with_config()
        .limit(MAX_BODY)
        .reader()
        .read_to_end(&mut body)
        .map_err(|e| NetError::Transport(format!("{url}: {e}")))?;
    Ok(Resource {
        url: final_url,
        status,
        content_type,
        charset,
        body,
    })
}

/// `text/html; charset="utf-8"` -> `utf-8`.
fn charset_param(content_type: &str) -> Option<&str> {
    content_type.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| v.trim().trim_matches(['"', '\'']))
    })
}

/// Resolves only loopback hosts; everything else is "host not found".
#[derive(Debug, Default)]
struct LoopbackOnly(ureq::unversioned::resolver::DefaultResolver);

impl ureq::unversioned::resolver::Resolver for LoopbackOnly {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        match uri.host() {
            Some("127.0.0.1" | "localhost" | "[::1]" | "::1") => {
                self.0.resolve(uri, config, timeout)
            }
            _ => Err(ureq::Error::HostNotFound),
        }
    }
}

fn mime_from_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => return None,
    })
}

/// Parse a command-line target: an absolute URL, or a file path (relative or absolute).
pub fn parse_target(target: &str) -> Result<Url, NetError> {
    if let Ok(u) = Url::parse(target)
        && u.scheme().len() > 1
    {
        return Ok(u);
    }
    let path = std::path::absolute(target).map_err(|e| NetError::Io(e.to_string()))?;
    Url::from_file_path(&path).map_err(|_| NetError::InvalidUrl(target.to_owned()))
}

/// Resolve `href` against `base`. Returns `None` for unparsable input.
pub fn resolve_url(base: &Url, href: &str) -> Option<Url> {
    base.join(href.trim()).ok()
}

/// A decoded image, shared between layout and paint.
///
/// `width()`/`height()` are the natural size in CSS px (what layout uses for
/// `<img>` and backgrounds). The pixmap can be smaller (a big photo decoded
/// for a small slot) or bigger (an SVG icon rendered at its displayed size),
/// so paint must scale by the pixmap's own size, not by `width()`.
#[derive(Clone)]
pub struct DecodedImage {
    pixmap: Arc<Pixmap>,
    width: u32,
    height: u32,
}

impl fmt::Debug for DecodedImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DecodedImage({}x{}, pixmap {}x{})",
            self.width,
            self.height,
            self.pixmap.width(),
            self.pixmap.height()
        )
    }
}

impl DecodedImage {
    /// Wrap a pixmap whose natural (CSS px) size is `natural`.
    pub fn new(pixmap: Pixmap, natural: (u32, u32)) -> DecodedImage {
        DecodedImage {
            pixmap: Arc::new(pixmap),
            width: natural.0.max(1),
            height: natural.1.max(1),
        }
    }

    /// The same pixels with another natural size (an Arc clone). Used for a
    /// `srcset` candidate: a `2x` image is half its pixel size in CSS px.
    pub fn with_natural_size(&self, natural: (u32, u32)) -> DecodedImage {
        DecodedImage {
            pixmap: self.pixmap.clone(),
            width: natural.0.max(1),
            height: natural.1.max(1),
        }
    }

    /// Natural width in CSS px.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Natural height in CSS px.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The pixels (premultiplied RGBA8, vello's format). Its size may differ
    /// from `width()` x `height()`; scale by `pixmap().width()`.
    pub fn pixmap(&self) -> &Arc<Pixmap> {
        &self.pixmap
    }
}

/// Decode PNG, JPEG, GIF (first frame), WebP, BMP, ICO or SVG bytes.
///
/// `display` is the size the image will be drawn at, in device pixels, when
/// the caller knows it (`<img width height>`, `srcset` slot, CSS px sizes); a
/// 0 side means "unknown". A raster image at least twice that size is
/// downscaled while decoding, which saves memory and paint time. An SVG is
/// rendered at `display` (or at its natural size). Either way
/// `width()`/`height()` stay the natural size.
///
/// Images larger than 65535 px on a side are rejected (rasterizer limit).
pub fn decode_image(bytes: &[u8], display: Option<(u32, u32)>) -> Result<DecodedImage, NetError> {
    if svg::is_svg(bytes) {
        return decode_svg(bytes, display);
    }
    let img = image::load_from_memory(bytes).map_err(|e| NetError::Decode(e.to_string()))?;
    let (w, h) = (img.width(), img.height());
    let has_alpha = img.color().has_alpha();
    let img = match display {
        Some((dw, dh)) if w > 0 && h > 0 => {
            // A 0 side is unknown; the known side decides.
            let f = (f64::from(dw) / f64::from(w)).max(f64::from(dh) / f64::from(h));
            if f > 0.0 && f <= 0.5 {
                let nw = (f64::from(w) * f).ceil().max(1.0) as u32;
                let nh = (f64::from(h) * f).ceil().max(1.0) as u32;
                img.thumbnail_exact(nw, nh)
            } else {
                img
            }
        }
        _ => img,
    };
    let rgba = img.into_rgba8();
    let (pw, ph) = rgba.dimensions();
    let (Ok(w16), Ok(h16)) = (u16::try_from(pw), u16::try_from(ph)) else {
        return Err(NetError::Decode(format!("image too large: {w}x{h}")));
    };
    let pixmap = Pixmap::from_parts(
        rgba.into_raw(),
        w16,
        h16,
        vello_common::pixmap::PixelMetadata {
            // Opaque formats (JPEG, RGB PNG) skip the premultiply pass.
            may_have_transparency: has_alpha,
            alpha_type: vello_common::peniko::ImageAlphaType::Alpha,
        },
    );
    Ok(DecodedImage::new(pixmap, (w, h)))
}

/// Decoded images, filled by the shell and read by layout (`<img>` sizes,
/// CSS backgrounds) and paint.
///
/// Two keys: the absolute URL (CSS backgrounds, plain `<img src>`), and the
/// element (`NodeId::to_raw()`) for images chosen per element: the
/// `srcset`/`<picture>` choice of an `<img>`, the poster of a `<video>`, and
/// inline `<svg>` rendered as an image. Layout asks for the element first,
/// then the `src` URL.
#[derive(Default, Debug, Clone)]
pub struct ImageStore {
    images: HashMap<String, DecodedImage>,
    by_node: HashMap<u32, DecodedImage>,
}

impl ImageStore {
    pub fn new() -> ImageStore {
        ImageStore::default()
    }

    pub fn insert(&mut self, url: &Url, image: DecodedImage) {
        self.images.insert(url.as_str().to_owned(), image);
    }

    pub fn get(&self, absolute_url: &str) -> Option<&DecodedImage> {
        self.images.get(absolute_url)
    }

    /// Image for one element; `node` is `huginn_dom::NodeId::to_raw()`.
    pub fn insert_for_node(&mut self, node: u32, image: DecodedImage) {
        self.by_node.insert(node, image);
    }

    /// Image for one element; `node` is `huginn_dom::NodeId::to_raw()`.
    pub fn get_for_node(&self, node: u32) -> Option<&DecodedImage> {
        self.by_node.get(&node)
    }

    /// Number of stored images (URL entries plus element entries).
    pub fn len(&self) -> usize {
        self.images.len() + self.by_node.len()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty() && self.by_node.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_url_and_cache() {
        let loader = Loader::new();
        let url = Url::parse("data:text/css,p%7Bcolor:red%7D").unwrap();
        let a = loader.fetch(&url, ResourceKind::Stylesheet).unwrap();
        assert_eq!(a.text(), "p{color:red}");
        assert_eq!(a.content_type.as_deref(), Some("text/css"));
        let b = loader.fetch(&url, ResourceKind::Stylesheet).unwrap();
        assert!(Arc::ptr_eq(&a, &b), "second fetch must hit the cache");
    }

    #[test]
    fn loopback_only_blocks_other_hosts_and_redirects_to_them() {
        use std::io::{BufRead, BufReader, Write};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in server.incoming().take(2) {
                let mut stream = stream.unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let reply = if line.contains("/redirect") {
                    "HTTP/1.1 302 Found\r\nLocation: http://example.com/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                };
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        let loader = Loader::loopback_only();
        let local = Url::parse(&format!("http://127.0.0.1:{port}/a")).unwrap();
        assert_eq!(
            loader.fetch(&local, ResourceKind::Other).unwrap().text(),
            "ok"
        );
        let host_not_found = |r: Result<Arc<Resource>, NetError>| matches!(r, Err(NetError::Transport(e)) if e.contains("host not found"));
        let external = Url::parse("http://example.com/").unwrap();
        assert!(host_not_found(loader.fetch(&external, ResourceKind::Other)));
        let redirect = Url::parse(&format!("http://127.0.0.1:{port}/redirect")).unwrap();
        assert!(host_not_found(loader.fetch(&redirect, ResourceKind::Other)));
    }

    #[test]
    fn missing_file_is_an_error() {
        let loader = Loader::new();
        let url = Url::parse("file:///definitely/not/here.css").unwrap();
        assert!(matches!(
            loader.fetch(&url, ResourceKind::Stylesheet),
            Err(NetError::Io(_))
        ));
    }

    #[test]
    fn target_accepts_paths_and_urls() {
        assert_eq!(
            parse_target("http://127.0.0.1:8000/a.html")
                .unwrap()
                .scheme(),
            "http"
        );
        assert_eq!(
            parse_target("fixtures/simple.html").unwrap().scheme(),
            "file"
        );
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, _| {
            image::Rgba([if x < w / 2 { 255 } else { 0 }, 0, 0, 255])
        });
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn big_images_shown_small_are_downscaled_but_keep_their_natural_size() {
        let bytes = png(400, 200);
        let full = decode_image(&bytes, None).unwrap();
        assert_eq!((full.width(), full.height()), (400, 200));
        assert_eq!(full.pixmap().width(), 400);

        let small = decode_image(&bytes, Some((100, 50))).unwrap();
        assert_eq!(
            (small.width(), small.height()),
            (400, 200),
            "layout still sees the natural size"
        );
        assert_eq!((small.pixmap().width(), small.pixmap().height()), (100, 50));
        assert_eq!(small.pixmap().sample(10, 25).r, 255);
        assert_eq!(small.pixmap().sample(90, 25).r, 0);

        // Less than 2x bigger than the slot: decoded at full size.
        let near = decode_image(&bytes, Some((250, 125))).unwrap();
        assert_eq!(near.pixmap().width(), 400);
        // A slot with another aspect ratio keeps the image's own ratio.
        let odd = decode_image(&bytes, Some((50, 100))).unwrap();
        assert_eq!(
            (odd.pixmap().width(), odd.pixmap().height()),
            (200, 100),
            "covers the 100 px tall slot"
        );
        let tiny = decode_image(&bytes, Some((40, 10))).unwrap();
        assert_eq!((tiny.pixmap().width(), tiny.pixmap().height()), (40, 20));
    }

    #[test]
    fn decode_image_dispatches_svg() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="12"/>"#;
        let img = decode_image(svg, Some((48, 24))).unwrap();
        assert_eq!((img.width(), img.height()), (24, 12));
        assert_eq!(img.pixmap().width(), 48);
    }

    #[test]
    fn charset_parameter_is_kept() {
        assert_eq!(
            charset_param("text/html; charset=\"Shift_JIS\""),
            Some("Shift_JIS")
        );
        assert_eq!(charset_param("text/css;CHARSET=utf-8 "), Some("utf-8"));
        assert_eq!(charset_param("text/html"), None);
    }
}
