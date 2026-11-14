//! huginn-net: fetching and resource decoding.
//!
//! - [`Loader`] fetches `http:`, `https:`, `file:` and `data:` URLs. It keeps
//!   an in-memory cache keyed by URL, so a resource used twice is fetched once.
//!   [`Loader::fetch_all`] fetches many URLs in parallel on scoped threads.
//!   Images and fonts go through [`Loader::fetch_uncached`]: their bodies are
//!   decoded on the fetching thread and dropped right away. It follows
//!   redirects itself, hop by hop: cookies ([`CookieJar`], RFC 6265bis with
//!   `SameSite`), `Referer` ([`ReferrerPolicy`]), and for scripts' requests
//!   the Fetch standard's modes and CORS ([`Loader::fetch_for_script`]).
//! - [`decode_image`] decodes PNG/JPEG/GIF/WebP/BMP/ICO, AVIF (the `avif`
//!   feature) and SVG into the rasterizer's pixel type
//!   (`vello_common::pixmap::Pixmap`, premultiplied RGBA8). Big images shown
//!   much smaller are downscaled while decoding. The decoded pixels are stored
//!   once and shared by layout (for the natural size) and paint (for
//!   drawing). There is no second copy.
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

use parking_lot::RwLock;

pub use url::Url;
pub use vello_common::pixmap::Pixmap;

#[cfg(feature = "avif")]
mod avif;
mod charset;
mod cookies;
mod cors;
mod font;
mod media;
mod referrer;
mod srcset;
mod svg;
pub mod ua;

pub use charset::{decode_html, decode_text};
pub use cookies::{CookieContext, CookieJar, SameSite, registrable_domain, same_site, site_key};
pub use font::{FontFormat, decode_font, font_format, size_adjust_font};
pub use media::{Screen, media_matches, parse_length};
pub use referrer::{ReferrerPolicy, referrer_for};
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

impl ResourceKind {
    /// `Sec-Fetch-Dest`.
    fn destination(self) -> &'static str {
        match self {
            ResourceKind::Document => "document",
            ResourceKind::Stylesheet => "style",
            ResourceKind::Script => "script",
            ResourceKind::Image => "image",
            ResourceKind::Font => "font",
            ResourceKind::Other => "empty",
        }
    }
}

/// A request's mode (Fetch standard).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Loading a top-level document.
    Navigate,
    /// Other origins fail.
    SameOrigin,
    /// Other origins answer with an opaque response (`<img>`, classic `<script>`).
    NoCors,
    /// Other origins must allow the request (`fetch()`, module scripts).
    Cors,
}

/// When a request carries cookies (Fetch "credentials mode").
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Credentials {
    Omit,
    /// Only to the requesting document's own origin.
    SameOrigin,
    Include,
}

/// What a script may see of a response (Fetch "response type").
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ResponseType {
    /// Same origin: everything but `Set-Cookie`.
    #[default]
    Basic,
    /// Another origin that allowed the request: safelisted and exposed headers.
    Cors,
    /// Another origin in `no-cors` mode: nothing.
    Opaque,
    /// A redirect with `redirect: "manual"`: nothing.
    OpaqueRedirect,
}

/// What happens on a redirect (`fetch()`'s `redirect` option).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum RedirectMode {
    #[default]
    Follow,
    Error,
    Manual,
}

/// An element's `crossorigin` attribute (HTML "CORS settings attribute").
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum CorsSettings {
    /// No attribute: a `no-cors` request with cookies.
    #[default]
    NoCors,
    /// `crossorigin` or `crossorigin=anonymous`; also every module script.
    Anonymous,
    /// `crossorigin=use-credentials`.
    UseCredentials,
}

impl CorsSettings {
    /// The state of a `crossorigin` attribute (`None`: the attribute is absent).
    pub fn from_attr(value: Option<&str>) -> CorsSettings {
        match value {
            None => CorsSettings::NoCors,
            Some(v) if v.trim().eq_ignore_ascii_case("use-credentials") => {
                CorsSettings::UseCredentials
            }
            Some(_) => CorsSettings::Anonymous,
        }
    }

    fn mode(self) -> (Mode, Credentials) {
        match self {
            CorsSettings::NoCors => (Mode::NoCors, Credentials::Include),
            CorsSettings::Anonymous => (Mode::Cors, Credentials::SameOrigin),
            CorsSettings::UseCredentials => (Mode::Cors, Credentials::Include),
        }
    }
}

/// The document a loader fetches for (its origin, site for cookies and
/// referrer), or the document that started a navigation.
#[derive(Clone, Debug, PartialEq)]
pub struct Client {
    pub url: Url,
    pub referrer_policy: ReferrerPolicy,
}

impl Client {
    pub fn new(url: Url) -> Client {
        Client {
            url,
            referrer_policy: ReferrerPolicy::default(),
        }
    }
}

/// A request made by a script (`fetch()`, `XMLHttpRequest`).
#[derive(Clone, Debug)]
pub struct ScriptRequest {
    pub url: Url,
    pub method: String,
    /// Author headers; forbidden ones are dropped.
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub mode: Mode,
    pub credentials: Credentials,
    pub redirect: RedirectMode,
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
    /// Response headers, names lowercase, without `Set-Cookie`.
    pub headers: Vec<(String, String)>,
    /// What a script may see of it.
    pub response_type: ResponseType,
    /// A redirect was followed.
    pub redirected: bool,
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

    /// The first value of response header `name` (lowercase).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn local(
        url: &Url,
        content_type: Option<String>,
        charset: Option<String>,
        body: Vec<u8>,
    ) -> Resource {
        Resource {
            url: url.clone(),
            status: 200,
            headers: content_type
                .iter()
                .map(|t| ("content-type".to_owned(), t.clone()))
                .collect(),
            content_type,
            charset,
            response_type: ResponseType::Basic,
            redirected: false,
            body,
        }
    }
}

#[derive(Debug, Clone)]
pub enum NetError {
    InvalidUrl(String),
    UnsupportedScheme(String),
    Io(String),
    Http {
        url: String,
        status: u16,
    },
    Transport(String),
    Decode(String),
    /// Refused by the same-origin policy or CORS.
    Blocked(String),
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
            NetError::Blocked(e) => write!(f, "blocked: {e}"),
        }
    }
}

impl std::error::Error for NetError {}

/// Max body size accepted for one resource (64 MiB).
const MAX_BODY: u64 = 64 << 20;
/// Max parallel fetches in [`Loader::fetch_all`]. Same as Chrome's per-host HTTP/1.1 limit.
const MAX_PARALLEL: usize = 6;
/// Redirects followed per request (Fetch's limit).
const MAX_REDIRECTS: usize = 20;

/// Fetches resources and caches them in memory for the life of the loader.
///
/// `Loader` is `Sync`: share one per page (or per process) by reference.
/// Requests carry the cookies of its [`CookieJar`] and the `Origin` and
/// `Referer` of its [`Client`], the document it fetches for.
pub struct Loader {
    agent: ureq::Agent,
    /// Same agent, but its resolver drops loopback and private addresses:
    /// subresources and script requests of a public page (Local Network
    /// Access). Made on first use.
    public_agent: std::sync::OnceLock<ureq::Agent>,
    config: ureq::config::Config,
    loopback_only: bool,
    cache: Mutex<HashMap<Url, Arc<Resource>>>,
    cookies: Arc<CookieJar>,
    client: RwLock<Option<Client>>,
    /// Whether the client document is on a public address (cached).
    client_public: parking_lot::Mutex<Option<bool>>,
}

impl Default for Loader {
    fn default() -> Self {
        Self::new()
    }
}

/// One request on its way through redirects.
struct Request<'a> {
    url: Url,
    method: String,
    headers: Vec<(String, String)>,
    body: Option<&'a [u8]>,
    kind: ResourceKind,
    mode: Mode,
    credentials: Credentials,
    redirect: RedirectMode,
    /// The document making the request (or starting the navigation);
    /// `None`: the browser itself (the address bar, a bookmark).
    client: Option<Client>,
    /// Return non-2xx responses (with their body) instead of an error.
    any_status: bool,
}

/// What a request's chain of URLs has been relative to its client, for `Sec-Fetch-Site`.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SiteRelation {
    SameOrigin,
    SameSite,
    CrossSite,
}

impl Loader {
    /// A loader with its own in-memory cookie jar. `HUGINN_NET_LOOPBACK_ONLY=1`
    /// in the environment makes it [`Loader::loopback_only`]. The benchmark
    /// sets it so Huginn is offline there like Chrome and Ladybird.
    pub fn new() -> Loader {
        Loader::with_cookies(Arc::new(CookieJar::in_memory()))
    }

    /// A loader that stores and sends cookies in `jar` (shared by the tabs
    /// of a profile).
    pub fn with_cookies(jar: Arc<CookieJar>) -> Loader {
        Loader::build(
            std::env::var_os("HUGINN_NET_LOOPBACK_ONLY").is_some_and(|v| v == "1"),
            jar,
        )
    }

    /// A loader that reaches only 127.0.0.1, localhost and ::1. Every other
    /// host fails at once as "host not found" (no DNS lookup, no connect,
    /// redirects included).
    pub fn loopback_only() -> Loader {
        Loader::build(true, Arc::new(CookieJar::in_memory()))
    }

    fn build(loopback_only: bool, cookies: Arc<CookieJar>) -> Loader {
        let config = ureq::Agent::config_builder()
            .user_agent(ua::USER_AGENT)
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            // Redirects are followed here, hop by hop, for cookies and CORS.
            .max_redirects(0)
            .allow_non_standard_methods(true)
            .build();
        let agent = ureq::Agent::with_parts(
            config.clone(),
            ureq::unversioned::transport::DefaultConnector::default(),
            HostResolver {
                loopback_only,
                ..HostResolver::default()
            },
        );
        Loader {
            agent,
            public_agent: std::sync::OnceLock::new(),
            config,
            loopback_only,
            cache: Mutex::new(HashMap::new()),
            cookies,
            client: RwLock::new(None),
            client_public: parking_lot::Mutex::new(None),
        }
    }

    /// The agent for a request: requests from a public page that are not
    /// top-level navigations may not reach loopback or private addresses
    /// (Local Network Access). The check is on the resolved addresses, so
    /// DNS rebinding cannot get around it.
    fn agent_for(&self, mode: Mode, client: Option<&Client>) -> &ureq::Agent {
        if mode == Mode::Navigate || self.loopback_only {
            return &self.agent;
        }
        let Some(c) = client else { return &self.agent };
        let public = *self
            .client_public
            .lock()
            .get_or_insert_with(|| is_public_url(&c.url));
        if !public {
            return &self.agent;
        }
        self.public_agent.get_or_init(|| {
            ureq::Agent::with_parts(
                self.config.clone(),
                ureq::unversioned::transport::DefaultConnector::default(),
                HostResolver {
                    public_only: true,
                    ..HostResolver::default()
                },
            )
        })
    }

    /// The cookie jar requests use (and `document.cookie` reads).
    pub fn cookies(&self) -> &Arc<CookieJar> {
        &self.cookies
    }

    /// Set the document this loader fetches for. Call once the document's
    /// final URL (and `Referrer-Policy`) is known.
    pub fn set_client(&self, client: Option<Client>) {
        *self.client.write() = client;
        *self.client_public.lock() = None;
    }

    /// The document this loader fetches for.
    pub fn client(&self) -> Option<Client> {
        self.client.read().clone()
    }

    /// Fetch one URL as a `no-cors` subresource of the client document.
    /// Cached results are returned without a new request. Non-2xx HTTP
    /// responses are errors.
    pub fn fetch(&self, url: &Url, kind: ResourceKind) -> Result<Arc<Resource>, NetError> {
        self.fetch_with(url, kind, CorsSettings::NoCors)
    }

    /// [`Loader::fetch`] with a `crossorigin` state: `Anonymous` and
    /// `UseCredentials` make a CORS request, so another origin must allow it
    /// (module scripts, `<script crossorigin>`).
    pub fn fetch_with(
        &self,
        url: &Url,
        kind: ResourceKind,
        cors: CorsSettings,
    ) -> Result<Arc<Resource>, NetError> {
        self.check_local(url)?;
        if let Some(hit) = self.cache.lock().expect("cache poisoned").get(url)
            && (cors == CorsSettings::NoCors || hit.response_type != ResponseType::Opaque)
        {
            return Ok(hit.clone());
        }
        let res = Arc::new(self.fetch_uncached_with(url, kind, cors)?);
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
    /// `ResourceKind::Document` is a navigation started by the browser
    /// itself (view source, save page).
    pub fn fetch_uncached(&self, url: &Url, kind: ResourceKind) -> Result<Resource, NetError> {
        if kind == ResourceKind::Document {
            return self.navigate(url, None, None).map(|(r, _)| r);
        }
        self.fetch_uncached_with(url, kind, CorsSettings::NoCors)
    }

    /// A document from the web may not load `file:` subresources (scripts,
    /// styles, images): a page could run or probe the user's local files.
    /// Chrome refuses them too ("Not allowed to load local resource").
    /// Documents that are local files themselves, and the browser's own
    /// pages, may.
    fn check_local(&self, url: &Url) -> Result<(), NetError> {
        let from_web = self
            .client()
            .is_some_and(|c| matches!(c.url.scheme(), "http" | "https"));
        if url.scheme() == "file" && from_web {
            return Err(NetError::Blocked(format!(
                "{url}: a web page may not load a local file"
            )));
        }
        Ok(())
    }

    fn fetch_uncached_with(
        &self,
        url: &Url,
        kind: ResourceKind,
        cors: CorsSettings,
    ) -> Result<Resource, NetError> {
        self.check_local(url)?;
        let (mode, credentials) = cors.mode();
        match url.scheme() {
            // Modules from `file:` are refused like in Chrome (opaque origin).
            "file" if mode == Mode::Cors => Err(NetError::Blocked(format!(
                "{url}: not a CORS request target"
            ))),
            "http" | "https" => self.send(Request {
                url: url.clone(),
                method: "GET".into(),
                headers: Vec::new(),
                body: None,
                kind,
                mode,
                credentials,
                redirect: RedirectMode::Follow,
                client: self.client(),
                any_status: false,
            }),
            _ => fetch_local(url),
        }
    }

    /// Load a top-level document: a `GET`, or a form `POST` (`post`:
    /// content type and body). `initiator`: the page that started the
    /// navigation (a link, a form, `location`); `None` for the browser UI.
    /// Returns the response and the `Referer` it was requested with
    /// (`document.referrer`).
    pub fn navigate(
        &self,
        url: &Url,
        post: Option<(&str, &[u8])>,
        initiator: Option<&Client>,
    ) -> Result<(Resource, Option<String>), NetError> {
        if !matches!(url.scheme(), "http" | "https") {
            if post.is_some() {
                return Err(NetError::UnsupportedScheme(url.scheme().to_owned()));
            }
            return fetch_local(url).map(|r| (r, None));
        }
        let (method, headers, body) = match post {
            Some((ty, body)) => (
                "POST",
                vec![("content-type".to_owned(), ty.to_owned())],
                Some(body),
            ),
            None => ("GET", Vec::new(), None),
        };
        let (res, referrer) = self.send_with_referrer(Request {
            url: url.clone(),
            method: method.into(),
            headers,
            body,
            kind: ResourceKind::Document,
            mode: Mode::Navigate,
            credentials: Credentials::Include,
            redirect: RedirectMode::Follow,
            client: initiator.cloned(),
            any_status: false,
        })?;
        Ok((res, referrer))
    }

    /// A request from a script of the client document (`fetch()`,
    /// `XMLHttpRequest`): any method, headers and body, CORS for other
    /// origins. Any HTTP status is a response. What the script may see is
    /// already filtered: opaque responses have status 0, no headers and no
    /// body; CORS responses only the headers the server exposes.
    pub fn fetch_for_script(&self, req: ScriptRequest) -> Result<Resource, NetError> {
        let method = cors::normalize_method(&req.method);
        if cors::is_forbidden_method(&method) || req.mode == Mode::Navigate {
            return Err(NetError::Blocked(format!("{method} {}", req.url)));
        }
        let mut headers: Vec<(String, String)> = req
            .headers
            .into_iter()
            .filter(|(n, v)| !cors::is_forbidden_request_header(n, v))
            .map(|(n, v)| (n.to_ascii_lowercase(), v))
            .collect();
        if req.mode == Mode::NoCors {
            if !cors::is_safelisted_method(&method) {
                return Err(NetError::Blocked(format!(
                    "{method} is not allowed in no-cors mode"
                )));
            }
            headers.retain(|(n, v)| cors::is_safelisted_request_header(n, v));
        }
        let client = self.client();
        match req.url.scheme() {
            "data" => return fetch_local(&req.url),
            "http" | "https" => {}
            s => {
                return Err(NetError::Blocked(format!(
                    "{s}: URLs cannot be fetched by scripts"
                )));
            }
        }
        // A plain same-origin GET may reuse what the page already loaded.
        let same_origin = client
            .as_ref()
            .is_some_and(|c| c.url.origin() == req.url.origin());
        let cacheable = same_origin
            && method == "GET"
            && headers.is_empty()
            && req.credentials != Credentials::Omit
            && req.redirect == RedirectMode::Follow;
        if cacheable
            && let Some(hit) = self.cache.lock().expect("cache poisoned").get(&req.url)
            && hit.response_type == ResponseType::Basic
        {
            return Ok(Resource {
                url: hit.url.clone(),
                status: hit.status,
                content_type: hit.content_type.clone(),
                charset: hit.charset.clone(),
                headers: hit.headers.clone(),
                response_type: ResponseType::Basic,
                redirected: hit.redirected,
                body: hit.body.clone(),
            });
        }
        let mut res = self.send(Request {
            url: req.url,
            method,
            headers,
            body: req.body.as_deref(),
            kind: ResourceKind::Other,
            mode: req.mode,
            credentials: req.credentials,
            redirect: req.redirect,
            client,
            any_status: true,
        })?;
        match res.response_type {
            ResponseType::Basic => {}
            ResponseType::Cors => {
                res.headers = cors::exposed_headers(
                    &res.headers,
                    true,
                    req.credentials == Credentials::Include,
                );
            }
            ResponseType::Opaque | ResponseType::OpaqueRedirect => {
                // Nothing about the other origin: not even whether it redirected.
                res.redirected = false;
                res.status = 0;
                res.headers = Vec::new();
                res.content_type = None;
                res.charset = None;
                res.body = Vec::new();
            }
        }
        Ok(res)
    }

    /// Fetch's main fetch for http(s), simplified: mode checks, CORS
    /// preflight, cookies, `Origin`/`Referer`/`Sec-Fetch-*`, and redirects
    /// followed one hop at a time.
    fn send(&self, req: Request<'_>) -> Result<Resource, NetError> {
        self.send_with_referrer(req).map(|(r, _)| r)
    }

    /// [`Loader::send`], plus the `Referer` sent on the final hop.
    fn send_with_referrer(
        &self,
        mut req: Request<'_>,
    ) -> Result<(Resource, Option<String>), NetError> {
        let origin = req.client.as_ref().map(|c| c.url.origin());
        let agent = self.agent_for(req.mode, req.client.as_ref());
        // Browser-UI navigations: the site the user asked for. A redirect
        // to another site makes the rest of the chain cross-site.
        let first_url = req.url.clone();
        // Third-party requests use the client's (the top-level page's)
        // cookie partition.
        let partition_key = req.client.as_ref().and_then(|c| cookies::site_key(&c.url));
        let mut sent_referrer: Option<String> = None;
        let mut policy = req
            .client
            .as_ref()
            .map_or(ReferrerPolicy::default(), |c| c.referrer_policy);
        // The referrer for the next hop; dropped for good once a hop's
        // policy yields none.
        let mut referrer_source = req.client.as_ref().map(|c| c.url.clone());
        let unsafe_names = cors::unsafe_request_header_names(&req.headers);
        let mut tainted_origin = false;
        let mut tainting = ResponseType::Basic;
        let mut redirected = false;
        let mut relation = SiteRelation::SameOrigin;
        for _hop in 0..=MAX_REDIRECTS {
            let url = req.url.clone();
            if !matches!(url.scheme(), "http" | "https") {
                return Err(NetError::Blocked(format!("redirect to {url}")));
            }
            let same_origin =
                !tainted_origin && origin.as_ref().is_some_and(|o| *o == url.origin());
            let serialized_origin = match &origin {
                Some(o) if o.is_tuple() && !tainted_origin => o.ascii_serialization(),
                _ => "null".to_owned(),
            };
            let r = match &req.client {
                Some(_) if same_origin => SiteRelation::SameOrigin,
                Some(c) if same_site(&c.url, &url) => SiteRelation::SameSite,
                Some(_) => SiteRelation::CrossSite,
                None if same_site(&first_url, &url) => SiteRelation::SameOrigin,
                None => SiteRelation::CrossSite,
            };
            relation = relation.max(r);
            match req.mode {
                Mode::SameOrigin if !same_origin => {
                    return Err(NetError::Blocked(format!("{url} is not same-origin")));
                }
                Mode::NoCors if !same_origin => {
                    if tainting == ResponseType::Basic {
                        tainting = ResponseType::Opaque;
                    }
                }
                Mode::Cors if !same_origin => {
                    tainting = ResponseType::Cors;
                    if !cors::is_safelisted_method(&req.method) || !unsafe_names.is_empty() {
                        let referer = referrer_source
                            .as_ref()
                            .and_then(|s| referrer_for(s, policy, &url));
                        self.preflight(
                            &url,
                            &req.method,
                            &unsafe_names,
                            &serialized_origin,
                            referer.as_deref(),
                            req.credentials == Credentials::Include,
                            relation,
                            agent,
                        )?;
                    }
                }
                _ => {}
            }
            let third_party = req.mode != Mode::Navigate
                && req
                    .client
                    .as_ref()
                    .is_some_and(|c| !same_site(&c.url, &url));
            let include_credentials = match req.credentials {
                Credentials::Include => true,
                Credentials::SameOrigin => tainting == ResponseType::Basic && same_origin,
                Credentials::Omit => false,
            } && (!third_party || partition_key.is_some());
            let safe_method = matches!(req.method.as_str(), "GET" | "HEAD");
            let cookie_cx = match (&req.mode, &req.client) {
                (Mode::Navigate, _) => CookieContext {
                    allows: if relation < SiteRelation::CrossSite {
                        SameSite::Strict
                    } else if safe_method {
                        SameSite::Lax
                    } else {
                        SameSite::None
                    },
                    top_level: true,
                    partition: None,
                },
                (_, Some(_)) if third_party => CookieContext {
                    allows: SameSite::None,
                    top_level: false,
                    partition: partition_key.as_deref(),
                },
                (_, Some(_)) => CookieContext::SAME_SITE,
                (_, None) => CookieContext::SAME_SITE,
            };

            // Request headers.
            let accept = match req.kind {
                ResourceKind::Document => "text/html,application/xhtml+xml,*/*;q=0.8",
                ResourceKind::Stylesheet => "text/css,*/*;q=0.1",
                ResourceKind::Image => IMAGE_ACCEPT,
                ResourceKind::Font => "font/woff2,font/woff,font/ttf,*/*;q=0.5",
                ResourceKind::Script | ResourceKind::Other => "*/*",
            };
            let mut b = ureq::http::Request::builder()
                .method(req.method.as_str())
                .uri(url.as_str());
            if !req.headers.iter().any(|(n, _)| n == "accept") {
                b = b.header("Accept", accept);
            }
            for (k, v) in ua::client_hint_headers(&url) {
                b = b.header(*k, *v);
            }
            for (n, v) in &req.headers {
                b = b.header(n.as_str(), v.as_str());
            }
            let referer = referrer_source
                .as_ref()
                .and_then(|s| referrer_for(s, policy, &url));
            if referer.is_none() {
                referrer_source = None;
            }
            sent_referrer.clone_from(&referer);
            if let Some(r) = &referer {
                b = b.header("Referer", r.as_str());
            }
            if req.client.is_some() {
                if tainting == ResponseType::Cors {
                    b = b.header("Origin", serialized_origin.as_str());
                } else if !safe_method {
                    let hide = match policy {
                        ReferrerPolicy::NoReferrer => true,
                        ReferrerPolicy::NoReferrerWhenDowngrade
                        | ReferrerPolicy::StrictOrigin
                        | ReferrerPolicy::StrictOriginWhenCrossOrigin => {
                            origin.as_ref().is_some_and(|o| match o {
                                url::Origin::Tuple(s, ..) => s == "https",
                                url::Origin::Opaque(_) => false,
                            }) && !cookies::is_secure(&url)
                        }
                        ReferrerPolicy::SameOrigin => !same_origin,
                        _ => false,
                    };
                    b = b.header(
                        "Origin",
                        if hide {
                            "null"
                        } else {
                            serialized_origin.as_str()
                        },
                    );
                }
            }
            if include_credentials && let Some(c) = self.cookies.header_for(&url, cookie_cx) {
                b = b.header("Cookie", c);
            }
            if cookies::is_secure(&url) {
                b = b
                    .header(
                        "Sec-Fetch-Site",
                        match (&req.client, relation) {
                            (None, SiteRelation::CrossSite) => "cross-site",
                            (None, _) => "none",
                            (_, SiteRelation::SameOrigin) => "same-origin",
                            (_, SiteRelation::SameSite) => "same-site",
                            (_, SiteRelation::CrossSite) => "cross-site",
                        },
                    )
                    .header(
                        "Sec-Fetch-Mode",
                        match req.mode {
                            Mode::Navigate => "navigate",
                            Mode::SameOrigin => "same-origin",
                            Mode::NoCors => "no-cors",
                            Mode::Cors => "cors",
                        },
                    )
                    .header("Sec-Fetch-Dest", req.kind.destination());
                if req.mode == Mode::Navigate && req.client.is_none() {
                    b = b.header("Sec-Fetch-User", "?1");
                }
            }
            if req.mode == Mode::Navigate {
                b = b.header("Upgrade-Insecure-Requests", "1");
            }
            let built = b
                .body(req.body.unwrap_or(&[]))
                .map_err(|e| NetError::InvalidUrl(format!("{url}: {e}")))?;
            let mut resp = if req.body.is_some() {
                agent.run(built)
            } else {
                let (parts, _) = built.into_parts();
                agent.run(ureq::http::Request::from_parts(parts, ()))
            }
            .map_err(|e| NetError::Transport(format!("{url}: {e}")))?;

            // Response headers; cookies go to the jar.
            let status = resp.status().as_u16();
            let mut headers = Vec::with_capacity(resp.headers().len());
            for (n, v) in resp.headers() {
                let v = String::from_utf8_lossy(v.as_bytes());
                if n == "set-cookie" || n == "set-cookie2" {
                    if include_credentials && n == "set-cookie" {
                        self.cookies.set_from_header(&url, &v, cookie_cx);
                    }
                    continue;
                }
                headers.push((n.as_str().to_owned(), v.into_owned()));
            }
            if tainting == ResponseType::Cors
                && !cors::cors_check(
                    &headers,
                    &serialized_origin,
                    req.credentials == Credentials::Include,
                )
            {
                return Err(NetError::Blocked(format!(
                    "{url}: CORS check failed for origin {serialized_origin}"
                )));
            }
            if let Some(p) = cors::header_value(&headers, "referrer-policy")
                .and_then(|v| ReferrerPolicy::parse_header(&v))
            {
                policy = p;
            }

            let location = cors::header_value(&headers, "location")
                .filter(|_| matches!(status, 301 | 302 | 303 | 307 | 308));
            if let Some(location) = location {
                match req.redirect {
                    RedirectMode::Error => {
                        return Err(NetError::Blocked(format!("{url}: unexpected redirect")));
                    }
                    RedirectMode::Manual => {
                        return Ok((
                            Resource {
                                url,
                                status: 0,
                                content_type: None,
                                charset: None,
                                headers: Vec::new(),
                                response_type: ResponseType::OpaqueRedirect,
                                redirected,
                                body: Vec::new(),
                            },
                            sent_referrer,
                        ));
                    }
                    RedirectMode::Follow => {}
                }
                let next = url
                    .join(&location)
                    .map_err(|_| NetError::InvalidUrl(format!("{url}: bad Location {location}")))?;
                let has_credentials = !next.username().is_empty() || next.password().is_some();
                if has_credentials
                    && ((req.mode == Mode::Cors && origin.as_ref() != Some(&next.origin()))
                        || tainting == ResponseType::Cors)
                {
                    return Err(NetError::Blocked(format!(
                        "{next}: redirect with credentials"
                    )));
                }
                if url.origin() != next.origin() {
                    if origin.as_ref().is_some_and(|o| *o != url.origin()) {
                        tainted_origin = true;
                    }
                    req.headers.retain(|(n, _)| n != "authorization");
                }
                if (matches!(status, 301 | 302) && req.method == "POST")
                    || (status == 303 && !matches!(req.method.as_str(), "GET" | "HEAD"))
                {
                    req.method = "GET".into();
                    req.body = None;
                    req.headers.retain(|(n, _)| {
                        !matches!(
                            n.as_str(),
                            "content-type"
                                | "content-encoding"
                                | "content-language"
                                | "content-location"
                        )
                    });
                }
                req.url = next;
                redirected = true;
                continue;
            }

            if !req.any_status && !(200..300).contains(&status) {
                return Err(NetError::Http {
                    url: url.to_string(),
                    status,
                });
            }
            let header = cors::header_value(&headers, "content-type");
            let content_type = header.as_deref().map(|v| {
                v.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
            });
            let charset = header.as_deref().and_then(charset_param).map(str::to_owned);
            // Size the buffer from Content-Length so read_to_end does not double it.
            let mut body = Vec::with_capacity(
                resp.body().content_length().unwrap_or(0).min(MAX_BODY) as usize,
            );
            if req.method != "HEAD" {
                resp.body_mut()
                    .with_config()
                    .limit(MAX_BODY)
                    .reader()
                    .read_to_end(&mut body)
                    .map_err(|e| NetError::Transport(format!("{url}: {e}")))?;
            }
            return Ok((
                Resource {
                    url,
                    status,
                    content_type,
                    charset,
                    headers,
                    response_type: tainting,
                    redirected,
                    body,
                },
                sent_referrer,
            ));
        }
        Err(NetError::Transport(format!(
            "{}: too many redirects",
            req.url
        )))
    }

    /// A CORS preflight (`OPTIONS`) for a request to `url`: no cookies; the
    /// answer must allow the origin, the method and the unsafe headers.
    #[allow(clippy::too_many_arguments)]
    fn preflight(
        &self,
        url: &Url,
        method: &str,
        unsafe_names: &[String],
        origin: &str,
        referer: Option<&str>,
        credentials: bool,
        relation: SiteRelation,
        agent: &ureq::Agent,
    ) -> Result<(), NetError> {
        let mut b = ureq::http::Request::builder()
            .method("OPTIONS")
            .uri(url.as_str())
            .header("Accept", "*/*")
            .header("Access-Control-Request-Method", method)
            .header("Origin", origin);
        if !unsafe_names.is_empty() {
            b = b.header("Access-Control-Request-Headers", unsafe_names.join(","));
        }
        if let Some(r) = referer {
            b = b.header("Referer", r);
        }
        if cookies::is_secure(url) {
            b = b
                .header(
                    "Sec-Fetch-Site",
                    if relation == SiteRelation::CrossSite {
                        "cross-site"
                    } else {
                        "same-site"
                    },
                )
                .header("Sec-Fetch-Mode", "cors")
                .header("Sec-Fetch-Dest", "empty");
        }
        for (k, v) in ua::client_hint_headers(url) {
            b = b.header(*k, *v);
        }
        let built = b
            .body(())
            .map_err(|e| NetError::InvalidUrl(format!("{url}: {e}")))?;
        let resp = agent
            .run(built)
            .map_err(|e| NetError::Transport(format!("{url}: {e}")))?;
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(n, v)| {
                (
                    n.as_str().to_owned(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let ok = (200..300).contains(&resp.status().as_u16())
            && cors::cors_check(&headers, origin, credentials)
            && cors::preflight_allows(&headers, method, unsafe_names, credentials);
        if ok {
            Ok(())
        } else {
            Err(NetError::Blocked(format!(
                "{url}: CORS preflight failed for origin {origin}"
            )))
        }
    }
}

/// `Accept` for images: only formats [`decode_image`] decodes, in Chrome's
/// order. Servers that negotiate send AVIF or WebP when they are listed.
const IMAGE_ACCEPT: &str = if cfg!(feature = "avif") {
    "image/avif,image/webp,image/svg+xml,image/*,*/*;q=0.8"
} else {
    "image/webp,image/svg+xml,image/*,*/*;q=0.8"
};

/// `file:` and `data:` URLs.
fn fetch_local(url: &Url) -> Result<Resource, NetError> {
    match url.scheme() {
        "file" => {
            let path = url
                .to_file_path()
                .map_err(|_| NetError::InvalidUrl(url.to_string()))?;
            let body = std::fs::read(&path)
                .map_err(|e| NetError::Io(format!("{}: {e}", path.display())))?;
            Ok(Resource::local(
                url,
                mime_from_path(&path).map(str::to_owned),
                None,
                body,
            ))
        }
        "data" => {
            let data = data_url::DataUrl::process(url.as_str())
                .map_err(|e| NetError::InvalidUrl(format!("{url}: {e:?}")))?;
            let (body, _) = data
                .decode_to_vec()
                .map_err(|e| NetError::Decode(format!("data URL: {e:?}")))?;
            let mime = data.mime_type();
            let charset = mime.get_parameter("charset").map(str::to_owned);
            Ok(Resource::local(
                url,
                Some(format!("{}/{}", mime.type_, mime.subtype)),
                charset,
                body,
            ))
        }
        other => Err(NetError::UnsupportedScheme(other.to_owned())),
    }
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

/// Resolves hosts for the loader. `localhost` and `*.localhost` always go
/// to loopback (127.0.0.1, then ::1), never to DNS, like Chrome: the cookie
/// jar treats them as secure. `loopback_only`: other hosts are "host not
/// found" (no DNS lookup). `public_only`: loopback and private addresses
/// are dropped from the answer (Local Network Access).
#[derive(Debug, Default)]
struct HostResolver {
    inner: ureq::unversioned::resolver::DefaultResolver,
    loopback_only: bool,
    public_only: bool,
}

impl ureq::unversioned::resolver::Resolver for HostResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
        let host = uri.host().unwrap_or("");
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if host == "localhost" || host.ends_with(".localhost") {
            if self.public_only {
                return Err(ureq::Error::HostNotFound);
            }
            let port = uri
                .port_u16()
                .unwrap_or(if uri.scheme_str() == Some("https") {
                    443
                } else {
                    80
                });
            let mut out = self.empty();
            out.push(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
            out.push(SocketAddr::from((Ipv6Addr::LOCALHOST, port)));
            return Ok(out);
        }
        if self.loopback_only && !matches!(host.as_str(), "127.0.0.1" | "::1") {
            return Err(ureq::Error::HostNotFound);
        }
        let addrs = self.inner.resolve(uri, config, timeout)?;
        if !self.public_only {
            return Ok(addrs);
        }
        let mut out = self.empty();
        for a in addrs.iter().filter(|a| !is_local_ip(a.ip())) {
            out.push(*a);
        }
        if out.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        Ok(out)
    }
}

/// A loopback, private, link-local or otherwise non-public address
/// (Local Network Access's "local" and "private" address spaces).
fn is_local_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(a) => {
            let o = a.octets();
            a.is_loopback()
                || a.is_private()
                || a.is_link_local()
                || a.is_unspecified()
                || a.is_broadcast()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        IpAddr::V6(a) => {
            if let Some(v4) = a.to_ipv4_mapped() {
                return is_local_ip(IpAddr::V4(v4));
            }
            let s = a.segments();
            a.is_loopback()
                || a.is_unspecified()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Whether a document URL is on a public address. Loopback and private IP
/// literals and `localhost` are not; other host names are resolved (the
/// document was just fetched, so this hits the system cache). `file:` and
/// other non-network documents count as local.
fn is_public_url(url: &Url) -> bool {
    use std::net::ToSocketAddrs;
    match url.host() {
        Some(url::Host::Ipv4(a)) => !is_local_ip(a.into()),
        Some(url::Host::Ipv6(a)) => !is_local_ip(a.into()),
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            if d == "localhost" || d.ends_with(".localhost") {
                return false;
            }
            let port = url.port_or_known_default().unwrap_or(80);
            match (d.as_str(), port).to_socket_addrs() {
                Ok(addrs) => {
                    let addrs: Vec<_> = addrs.collect();
                    addrs.is_empty() || addrs.iter().any(|a| !is_local_ip(a.ip()))
                }
                Err(_) => true,
            }
        }
        None => false,
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
        "avif" => "image/avif",
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

/// Decode PNG, JPEG, GIF (first frame), WebP, BMP, ICO, AVIF (the `avif`
/// feature; still images) or SVG bytes.
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
    let (img, has_alpha) = decode_raster(bytes)?;
    let (w, h) = (img.width(), img.height());
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

/// Decode a raster image; also says whether it may have transparency.
fn decode_raster(bytes: &[u8]) -> Result<(image::DynamicImage, bool), NetError> {
    #[cfg(feature = "avif")]
    if avif::is_avif(bytes) {
        let a = avif::decode(bytes).map_err(NetError::Decode)?;
        let img = image::RgbaImage::from_raw(a.width, a.height, a.pixels)
            .ok_or_else(|| NetError::Decode("AVIF: bad buffer size".into()))?;
        return Ok((image::DynamicImage::ImageRgba8(img), a.has_alpha));
    }
    let img = image::load_from_memory(bytes).map_err(|e| NetError::Decode(e.to_string()))?;
    let has_alpha = img.color().has_alpha();
    Ok((img, has_alpha))
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

    /// A request a test server saw.
    #[derive(Clone, Debug)]
    struct Seen {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
    }

    impl Seen {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
        }
    }

    /// An HTTP/1.1 server on 127.0.0.1 answering with `handler`'s status
    /// line plus headers (body after a blank line). Returns its port and the
    /// requests it saw.
    fn serve(handler: impl Fn(&Seen) -> String + Send + 'static) -> (u16, Arc<Mutex<Vec<Seen>>>) {
        use std::io::{BufRead, BufReader, Write};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in server.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or("").to_owned();
                let path = parts.next().unwrap_or("").to_owned();
                let mut headers = Vec::new();
                let mut len = 0;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    let (n, v) = h.split_once(':').unwrap();
                    let (n, v) = (n.trim().to_ascii_lowercase(), v.trim().to_owned());
                    if n == "content-length" {
                        len = v.parse().unwrap();
                    }
                    headers.push((n, v));
                }
                let mut body = vec![0; len];
                std::io::Read::read_exact(&mut reader, &mut body).unwrap();
                let req = Seen {
                    method,
                    path,
                    headers,
                };
                let reply = handler(&req);
                log.lock().unwrap().push(req);
                let (head, body) = reply.split_once("\n\n").unwrap_or((&reply, ""));
                let head = head.replace('\n', "\r\n");
                let out = format!(
                    "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(out.as_bytes()).unwrap();
            }
        });
        (port, seen)
    }

    fn script(url: &str, method: &str, credentials: Credentials) -> ScriptRequest {
        ScriptRequest {
            url: Url::parse(url).unwrap(),
            method: method.into(),
            headers: Vec::new(),
            body: None,
            mode: Mode::Cors,
            credentials,
            redirect: RedirectMode::Follow,
        }
    }

    #[test]
    fn cookies_are_stored_and_sent_across_redirect_hops() {
        let (port, seen) = serve(|r| {
            match r.path.as_str() {
            "/login" => "302 Found\nLocation: /home\nSet-Cookie: sid=1; Path=/\nSet-Cookie: tp=1; SameSite=None; Secure; Path=/".into(),
            _ => "200 OK\nContent-Type: text/plain\n\nhome".into(),
        }
        });
        let loader = Loader::new();
        let login = Url::parse(&format!("http://127.0.0.1:{port}/login")).unwrap();
        let (doc, referrer) = loader.navigate(&login, None, None).unwrap();
        assert_eq!(doc.url.path(), "/home");
        assert!(doc.redirected && referrer.is_none());
        let log = seen.lock().unwrap().clone();
        assert_eq!(log[0].header("cookie"), None);
        assert_eq!(log[1].header("cookie"), Some("sid=1; tp=1"));
        assert_eq!(log[1].header("sec-fetch-site"), Some("none"));
        assert_eq!(log[1].header("sec-fetch-mode"), Some("navigate"));

        // A page on another site (localhost vs 127.0.0.1) loads an image from
        // this server: only the SameSite=None cookie goes along, and the
        // Referer is trimmed to the page's origin.
        loader.set_client(Some(Client::new(
            Url::parse("http://localhost:1/news/story?id=7").unwrap(),
        )));
        let img = Url::parse(&format!("http://127.0.0.1:{port}/pixel")).unwrap();
        let r = loader.fetch_uncached(&img, ResourceKind::Image).unwrap();
        assert_eq!(r.response_type, ResponseType::Opaque);
        let log = seen.lock().unwrap().clone();
        let last = log.last().unwrap();
        // The first-party SameSite=None cookie stays out of third-party
        // requests (cookies are partitioned by top-level site).
        assert_eq!(last.header("cookie"), None);
        assert_eq!(last.header("referer"), Some("http://localhost:1/"));
        assert_eq!(last.header("sec-fetch-site"), Some("cross-site"));
        assert_eq!(last.header("origin"), None);

        // A link from that other site: Lax cookies go on a GET navigation,
        // and the new document's referrer is the linking origin.
        let initiator = loader.client().unwrap();
        let (_, referrer) = loader.navigate(&img, None, Some(&initiator)).unwrap();
        assert_eq!(referrer.as_deref(), Some("http://localhost:1/"));
        let log = seen.lock().unwrap().clone();
        assert_eq!(log.last().unwrap().header("cookie"), Some("sid=1; tp=1"));
        // ... but not on a cross-site POST.
        loader
            .navigate(&img, Some(("text/plain", b"x")), Some(&initiator))
            .unwrap();
        let log = seen.lock().unwrap().clone();
        let post = log.last().unwrap();
        assert_eq!(post.method, "POST");
        assert_eq!(post.header("cookie"), Some("tp=1"));
        assert_eq!(post.header("origin"), Some("http://localhost:1"));
    }

    #[test]
    fn cors_allows_denies_and_hides_what_scripts_may_not_read() {
        let (port, seen) = serve(|r| {
            let origin = r.header("origin").unwrap_or("").to_owned();
            match r.path.as_str() {
                "/open" => format!(
                    "200 OK\nContent-Type: application/json\nX-Secret: s\nX-Public: p\nAccess-Control-Allow-Origin: {origin}\nAccess-Control-Expose-Headers: X-Public\nSet-Cookie: c=1; SameSite=None; Secure\n\n{{\"ok\":1}}"
                ),
                "/creds" => format!(
                    "200 OK\nAccess-Control-Allow-Origin: {origin}\nAccess-Control-Allow-Credentials: true\n\nyes"
                ),
                "/star" => "200 OK\nAccess-Control-Allow-Origin: *\n\nstar".into(),
                "/hop" => "302 Found\nLocation: /open\nAccess-Control-Allow-Origin: *".into(),
                _ => "200 OK\nContent-Type: text/plain\n\nprivate data".into(),
            }
        });
        let loader = Loader::new();
        loader.set_client(Some(Client::new(
            Url::parse("http://localhost:9/app").unwrap(),
        )));
        let at = |p: &str| format!("http://127.0.0.1:{port}{p}");

        let r = loader
            .fetch_for_script(script(&at("/open"), "GET", Credentials::SameOrigin))
            .unwrap();
        assert_eq!((r.status, r.response_type), (200, ResponseType::Cors));
        assert_eq!(r.body, b"{\"ok\":1}");
        let names: Vec<&str> = r.headers.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"content-type") && names.contains(&"x-public"));
        assert!(!names.contains(&"x-secret") && !names.contains(&"set-cookie"));
        let req = seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(req.header("origin"), Some("http://localhost:9"));
        assert_eq!(
            req.header("cookie"),
            None,
            "same-origin credentials stay home"
        );
        // Cookies of a credentials-less CORS response are not stored.
        assert!(loader.cookies().is_empty());

        // No Access-Control-Allow-Origin: the script gets a network error.
        assert!(matches!(
            loader.fetch_for_script(script(&at("/private"), "GET", Credentials::SameOrigin)),
            Err(NetError::Blocked(_))
        ));
        // `*` does not allow credentials; an echoed origin with
        // Allow-Credentials does.
        assert!(
            loader
                .fetch_for_script(script(&at("/star"), "GET", Credentials::Include))
                .is_err()
        );
        assert_eq!(
            loader
                .fetch_for_script(script(&at("/creds"), "GET", Credentials::Include))
                .unwrap()
                .body,
            b"yes"
        );
        // no-cors: the request goes out, the response is opaque.
        let mut req = script(&at("/private"), "GET", Credentials::Include);
        req.mode = Mode::NoCors;
        let r = loader.fetch_for_script(req).unwrap();
        assert_eq!((r.status, r.response_type), (0, ResponseType::Opaque));
        assert!(r.body.is_empty() && r.headers.is_empty());
        // ... and does not tell whether it redirected.
        let mut req = script(&at("/hop"), "GET", Credentials::Include);
        req.mode = Mode::NoCors;
        let r = loader.fetch_for_script(req).unwrap();
        assert!(!r.redirected && r.response_type == ResponseType::Opaque);
        // same-origin mode refuses other origins outright.
        let mut req = script(&at("/open"), "GET", Credentials::Include);
        req.mode = Mode::SameOrigin;
        let before = seen.lock().unwrap().len();
        assert!(loader.fetch_for_script(req).is_err());
        assert_eq!(seen.lock().unwrap().len(), before, "nothing was sent");
        // A CORS redirect within the server keeps working (each hop is checked).
        let r = loader
            .fetch_for_script(script(&at("/hop"), "GET", Credentials::SameOrigin))
            .unwrap();
        assert!(r.redirected && r.url.path() == "/open");
    }

    #[test]
    fn preflight_guards_non_simple_requests() {
        let (port, seen) = serve(|r| {
            let origin = r.header("origin").unwrap_or("").to_owned();
            match (r.method.as_str(), r.path.as_str()) {
                ("OPTIONS", "/api") => format!(
                    "204 No Content\nAccess-Control-Allow-Origin: {origin}\nAccess-Control-Allow-Methods: PUT\nAccess-Control-Allow-Headers: x-token\nAccess-Control-Allow-Credentials: true"
                ),
                ("OPTIONS", _) => "204 No Content".into(),
                (_, "/api") => format!(
                    "200 OK\nAccess-Control-Allow-Origin: {origin}\nAccess-Control-Allow-Credentials: true\n\nput ok"
                ),
                _ => "200 OK\nAccess-Control-Allow-Origin: *\n\nnever".into(),
            }
        });
        let loader = Loader::new();
        // A cookie the API set while embedded in this page (its partition),
        // which a credentialed request carries.
        let api = Url::parse(&format!("http://127.0.0.1:{port}/api")).unwrap();
        loader.cookies().set_from_header(
            &api,
            "k=1; SameSite=None; Secure",
            CookieContext {
                allows: SameSite::None,
                top_level: false,
                partition: Some("http://localhost"),
            },
        );
        loader.set_client(Some(Client::new(
            Url::parse("http://localhost:9/").unwrap(),
        )));

        let mut req = script(api.as_str(), "put", Credentials::Include);
        req.headers = vec![
            ("X-Token".into(), "t".into()),
            ("Content-Type".into(), "text/plain".into()),
        ];
        req.body = Some(b"data".to_vec());
        let r = loader.fetch_for_script(req).unwrap();
        assert_eq!(r.body, b"put ok");
        let log = seen.lock().unwrap().clone();
        let (pre, put) = (&log[log.len() - 2], &log[log.len() - 1]);
        assert_eq!(pre.method, "OPTIONS");
        assert_eq!(pre.header("access-control-request-method"), Some("PUT"));
        assert_eq!(
            pre.header("access-control-request-headers"),
            Some("x-token")
        );
        assert_eq!(pre.header("cookie"), None, "preflights never carry cookies");
        assert_eq!(
            (put.method.as_str(), put.header("cookie")),
            ("PUT", Some("k=1"))
        );
        assert_eq!(put.header("x-token"), Some("t"));

        // A preflight that does not allow the method: the request is never sent.
        let mut req = script(
            &format!("http://127.0.0.1:{port}/other"),
            "DELETE",
            Credentials::SameOrigin,
        );
        req.headers = vec![("Cookie".into(), "forged=1".into())];
        let before = seen.lock().unwrap().len();
        assert!(matches!(
            loader.fetch_for_script(req),
            Err(NetError::Blocked(_))
        ));
        let log = seen.lock().unwrap().clone();
        assert_eq!(log.len(), before + 1);
        assert_eq!(log[before].method, "OPTIONS");
        assert_eq!(
            log[before].header("cookie"),
            None,
            "forbidden headers are dropped"
        );
    }

    /// A server for the security tests: `/hop` redirects to `/echo` on
    /// 127.0.0.1, `/strip` redirects with `Referrer-Policy: no-referrer`,
    /// `/set` sets a first-party cookie, `/cors-set` and `/cors-echo` answer
    /// credentialed CORS; everything else echoes the `Cookie` header.
    fn security_server() -> (u16, Arc<Mutex<Vec<Seen>>>) {
        let port = Arc::new(Mutex::new(0u16));
        let p = port.clone();
        let (n, log) = serve(move |r| {
            let port = *p.lock().unwrap();
            let allow = format!(
                "Access-Control-Allow-Origin: {}\nAccess-Control-Allow-Credentials: true",
                r.header("origin").unwrap_or("")
            );
            let cookie = r.header("cookie").unwrap_or("").to_owned();
            match r.path.as_str() {
                "/hop" => format!("302 Found\nLocation: http://127.0.0.1:{port}/echo"),
                "/strip" => "302 Found\nLocation: /echo\nReferrer-Policy: no-referrer".into(),
                "/set" => {
                    "200 OK\nSet-Cookie: first=1; SameSite=None; Secure; Path=/\n\nset".into()
                }
                "/cors-set" => format!(
                    "200 OK\n{allow}\nSet-Cookie: part=1; SameSite=None; Secure; Path=/\n\nset"
                ),
                "/cors-echo" => format!("200 OK\n{allow}\n\n{cookie}"),
                _ => format!("200 OK\n\n{cookie}"),
            }
        });
        *port.lock().unwrap() = n;
        (n, log)
    }

    #[test]
    fn browser_navigations_lose_strict_cookies_after_a_cross_site_redirect() {
        let (port, seen) = security_server();
        let loader = Loader::new();
        let echo = Url::parse(&format!("http://127.0.0.1:{port}/echo")).unwrap();
        for c in ["s=1; SameSite=Strict; Path=/", "l=1; Path=/"] {
            loader
                .cookies()
                .set_from_header(&echo, c, CookieContext::SAME_SITE);
        }
        // Typed into the address bar: same site, all cookies, `none`.
        let (r, _) = loader.navigate(&echo, None, None).unwrap();
        assert_eq!(r.body, b"s=1; l=1");
        let last = seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(last.header("sec-fetch-site"), Some("none"));
        assert_eq!(last.header("sec-fetch-user"), Some("?1"));
        // Typed, but it redirects from another site (localhost): the chain
        // is cross-site from then on, so Strict cookies stay home.
        let hop = Url::parse(&format!("http://localhost:{port}/hop")).unwrap();
        let (r, _) = loader.navigate(&hop, None, None).unwrap();
        assert_eq!(r.body, b"l=1");
        let last = seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(last.header("sec-fetch-site"), Some("cross-site"));
        // Started by a page of another site: Lax only, and never `none`/`?1`.
        let page = Client::new(Url::parse("http://localhost:1/").unwrap());
        let (r, _) = loader.navigate(&echo, None, Some(&page)).unwrap();
        assert_eq!(r.body, b"l=1");
        let last = seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(last.header("sec-fetch-site"), Some("cross-site"));
        assert_eq!(last.header("sec-fetch-user"), None);
    }

    #[test]
    fn document_referrer_is_the_referer_actually_sent() {
        let (port, seen) = security_server();
        let loader = Loader::new();
        let mut page = Client::new(Url::parse("http://localhost:1/mail?id=secret").unwrap());
        page.referrer_policy = ReferrerPolicy::UnsafeUrl;
        let strip = Url::parse(&format!("http://127.0.0.1:{port}/strip")).unwrap();
        let (r, referrer) = loader.navigate(&strip, None, Some(&page)).unwrap();
        assert_eq!(r.url.path(), "/echo");
        assert_eq!(referrer, None);
        let log = seen.lock().unwrap().clone();
        assert_eq!(
            log[0].header("referer"),
            Some("http://localhost:1/mail?id=secret")
        );
        assert_eq!(log[1].header("referer"), None);
    }

    #[test]
    fn localhost_subdomains_resolve_to_loopback_without_dns() {
        let (port, seen) = security_server();
        let loader = Loader::build(false, Arc::new(CookieJar::in_memory()));
        let url = Url::parse(&format!("http://app.localhost:{port}/x")).unwrap();
        loader.fetch(&url, ResourceKind::Other).unwrap();
        let log = seen.lock().unwrap();
        assert_eq!(
            log[0].header("host"),
            Some(format!("app.localhost:{port}").as_str())
        );
    }

    #[test]
    fn public_pages_cannot_reach_local_addresses() {
        let (port, seen) = security_server();
        let loader = Loader::build(false, Arc::new(CookieJar::in_memory()));
        // A page on a public address (an IP literal: no DNS in the test).
        let page = Client::new(Url::parse("http://93.184.215.14/").unwrap());
        loader.set_client(Some(page.clone()));
        let local = format!("http://127.0.0.1:{port}");
        let mut req = script(&format!("{local}/cors-echo"), "POST", Credentials::Include);
        req.mode = Mode::NoCors;
        assert!(loader.fetch_for_script(req).is_err());
        let img = Url::parse(&format!("{local}/pixel.png")).unwrap();
        assert!(loader.fetch_uncached(&img, ResourceKind::Image).is_err());
        let named = Url::parse(&format!("http://app.localhost:{port}/x")).unwrap();
        assert!(loader.fetch(&named, ResourceKind::Script).is_err());
        assert!(
            seen.lock().unwrap().is_empty(),
            "nothing reached the local server"
        );
        // A top-level navigation to a local address is still allowed.
        assert!(loader.navigate(&img, None, Some(&page)).is_ok());
        // Local pages may use local servers.
        loader.set_client(Some(Client::new(
            Url::parse("http://localhost:9/").unwrap(),
        )));
        assert!(loader.fetch_uncached(&img, ResourceKind::Image).is_ok());
    }

    #[test]
    fn third_party_requests_use_the_top_level_sites_cookie_partition() {
        let (port, seen) = security_server();
        let loader = Loader::new();
        let at = |p: &str| format!("http://127.0.0.1:{port}{p}");
        // A first-party visit sets the third party's own cookie.
        loader
            .navigate(&Url::parse(&at("/set")).unwrap(), None, None)
            .unwrap();
        let fetch = |top: &str, path: &str| {
            loader.set_client(Some(Client::new(Url::parse(top).unwrap())));
            let r = loader.fetch_for_script(script(&at(path), "GET", Credentials::Include));
            String::from_utf8(r.unwrap().body).unwrap()
        };
        // Embedded in a.localhost: the first-party cookie is not sent.
        assert_eq!(fetch("http://a.localhost:9/", "/cors-echo"), "");
        fetch("http://a.localhost:9/", "/cors-set");
        assert_eq!(fetch("http://a.localhost:9/", "/cors-echo"), "part=1");
        // Embedded in b.localhost: another, empty partition.
        assert_eq!(fetch("http://b.localhost:9/", "/cors-echo"), "");
        // First party again: only its own cookie.
        let (r, _) = loader
            .navigate(&Url::parse(&at("/echo")).unwrap(), None, None)
            .unwrap();
        assert_eq!(r.body, b"first=1");
        assert!(seen.lock().unwrap().len() >= 6);
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
