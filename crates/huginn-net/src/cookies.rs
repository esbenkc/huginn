//! The cookie jar (RFC 6265bis, as Chrome implements it).
//!
//! - `Set-Cookie` parsing: `Expires`, `Max-Age` (capped at 400 days),
//!   `Domain` (not a public suffix), `Path`, `Secure`, `HttpOnly`,
//!   `SameSite` (`Lax` when absent), and the `__Secure-`/`__Host-` prefixes.
//!   `Partitioned` is ignored.
//! - Sending: domain and path match, `Secure` only over secure schemes,
//!   `SameSite` against the request's [`CookieContext`].
//! - `document.cookie`: the same store without `HttpOnly` cookies.
//! - Persistence: [`CookieJar::persistent`] loads and [`CookieJar::save`]
//!   writes cookies with an expiry date. Session cookies are never written.
//!
//! "Secure" schemes are `https` and loopback hosts (`localhost`, 127.0.0.1,
//! `::1`), like Chrome.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use url::{Host, Url};

/// Longest name plus value accepted.
const MAX_NAME_VALUE: usize = 4096;
/// Longer attribute values are ignored.
const MAX_ATTRIBUTE: usize = 1024;
/// Expiry dates are capped at 400 days from now.
const MAX_AGE_SECS: i64 = 400 * 24 * 3600;
/// Per registrable domain and in total (Chrome's limits).
const MAX_PER_SITE: usize = 180;
const MAX_TOTAL: usize = 3300;

/// A cookie's `SameSite` attribute.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SameSite {
    /// `SameSite=None`: sent on cross-site requests (requires `Secure`).
    None,
    /// `SameSite=Lax`, or no (or an unknown) attribute.
    Lax,
    Strict,
}

/// How a request relates to the site of the document that made it, for
/// `SameSite` (RFC 6265bis, "same-site and cross-site requests"), and which
/// cookie partition it uses.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CookieContext<'a> {
    /// The strictest `SameSite` value whose cookies the request carries:
    /// `Strict` for same-site requests, `Lax` for a cross-site top-level
    /// navigation with a safe method, `None` for other cross-site requests.
    pub allows: SameSite,
    /// The request navigates the top-level document: its `Set-Cookie`
    /// headers are accepted whatever the site of the page that started it.
    pub top_level: bool,
    /// Third-party context: the top-level site ([`site_key`]) whose
    /// partition the request reads and writes. `None`: first-party cookies.
    /// A third party never sees its first-party cookies, and each
    /// top-level site sees its own copy of the third party's cookies
    /// (like Firefox's Total Cookie Protection).
    pub partition: Option<&'a str>,
}

impl CookieContext<'_> {
    /// A same-site request (or one the user started from the browser UI).
    pub const SAME_SITE: CookieContext<'static> = CookieContext {
        allows: SameSite::Strict,
        top_level: false,
        partition: None,
    };

    fn admits(self, cookie: SameSite) -> bool {
        matches!(
            (self.allows, cookie),
            (_, SameSite::None) | (SameSite::Strict, _) | (SameSite::Lax, SameSite::Lax)
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Cookie {
    name: String,
    value: String,
    /// Lowercase host or `Domain` attribute, without a leading dot.
    domain: String,
    host_only: bool,
    path: String,
    /// Unix seconds; `None`: a session cookie.
    expires: Option<i64>,
    secure: bool,
    http_only: bool,
    same_site: SameSite,
    /// Unique, increasing: creation order (microseconds since the epoch).
    created: u64,
    last_access: u64,
    /// Top-level site of a third-party cookie ([`CookieContext::partition`]).
    partition: Option<String>,
}

#[derive(Default)]
struct Store {
    /// Cookies by `domain`.
    by_domain: HashMap<String, Vec<Cookie>>,
    count: usize,
    /// A persistent cookie changed since the last save.
    dirty: bool,
    /// Last timestamp handed out (keeps `created` unique).
    clock: u64,
}

/// Cookies of one profile. `Sync`: share it by `Arc` between loaders.
pub struct CookieJar {
    store: Mutex<Store>,
    /// Where [`CookieJar::save`] writes; `None`: in memory only.
    path: Option<PathBuf>,
    /// Held while writing the file; the time of the last write.
    saved: Mutex<Option<Instant>>,
}

impl Default for CookieJar {
    fn default() -> Self {
        CookieJar::in_memory()
    }
}

impl std::fmt::Debug for CookieJar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookieJar")
            .field("cookies", &self.lock().count)
            .field("path", &self.path)
            .finish()
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

impl CookieJar {
    /// An empty jar that is never written to disk (private windows, `huginn render`).
    pub fn in_memory() -> CookieJar {
        CookieJar {
            store: Mutex::new(Store::default()),
            path: None,
            saved: Mutex::new(None),
        }
    }

    /// A jar backed by `path`: loads the cookies saved there (a missing or
    /// unreadable file is an empty jar); [`CookieJar::save`] writes them back.
    pub fn persistent(path: PathBuf) -> CookieJar {
        let mut jar = CookieJar::in_memory();
        if let Ok(text) = std::fs::read_to_string(&path) {
            jar.load(&text);
        }
        jar.path = Some(path);
        jar
    }

    fn lock(&self) -> parking_lot::MutexGuard<'_, Store> {
        self.store.lock()
    }

    /// Number of stored cookies (expired ones included until next use).
    pub fn len(&self) -> usize {
        self.lock().count
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Delete every cookie (clear browsing data). Saved at the next [`CookieJar::save`].
    pub fn clear(&self) {
        let mut s = self.lock();
        s.by_domain.clear();
        s.count = 0;
        s.dirty = true;
    }

    /// Write the persistent cookies if any changed. Owner-only file
    /// permissions; written to a temporary file, then renamed.
    pub fn save(&self) -> std::io::Result<()> {
        self.save_unless_saved_within(Duration::ZERO)
    }

    /// [`CookieJar::save`], unless the file was written less than `interval`
    /// ago (cheap to call after every page load; the next call catches up).
    pub fn save_unless_saved_within(&self, interval: Duration) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut saved = self.saved.lock();
        if saved.is_some_and(|t| t.elapsed() < interval) {
            return Ok(());
        }
        let text = {
            let mut s = self.lock();
            if !s.dirty {
                return Ok(());
            }
            s.dirty = false;
            serialize(&s, now_secs())
        };
        let r = write_private(path, &text);
        if r.is_err() {
            self.lock().dirty = true;
        }
        *saved = Some(Instant::now());
        r
    }

    /// The `Cookie` header for an HTTP request to `url`, or `None`.
    pub fn header_for(&self, url: &Url, cx: CookieContext) -> Option<String> {
        self.get(url, cx, true)
    }

    /// Store a `Set-Cookie` header received from `url`.
    pub fn set_from_header(&self, url: &Url, set_cookie: &str, cx: CookieContext) {
        self.set(url, set_cookie, cx, true);
    }

    /// `document.cookie` of a document at `url` shown in a page whose
    /// top-level document is at `top_level` (the same URL for the page
    /// itself). A cross-site frame sees only its partition.
    pub fn document_cookie(&self, url: &Url, top_level: &Url) -> String {
        let key = site_key(top_level);
        match document_context(url, top_level, key.as_deref()) {
            Some(cx) => self.get(url, cx, false).unwrap_or_default(),
            None => String::new(),
        }
    }

    /// `document.cookie = value` in a document at `url` under `top_level`.
    pub fn set_document_cookie(&self, url: &Url, top_level: &Url, value: &str) {
        let key = site_key(top_level);
        if let Some(cx) = document_context(url, top_level, key.as_deref()) {
            self.set(url, value, cx, false);
        }
    }

    fn get(&self, url: &Url, cx: CookieContext, http: bool) -> Option<String> {
        let host = cookie_host(url)?;
        let secure = is_secure(url);
        let path = url.path();
        let now = now_secs();
        let stamp = now_micros();
        let mut s = self.lock();
        // Every domain the host matches: the host, then each parent domain.
        let mut domains = vec![host.as_str()];
        if !is_ip(&host) {
            let mut d = host.as_str();
            while let Some((_, rest)) = d.split_once('.') {
                d = rest;
                domains.push(d);
            }
        }
        let mut expired = false;
        // (bucket, index, path length, created) of each cookie to send.
        let mut hits: Vec<(&str, usize, usize, u64)> = Vec::new();
        for &d in &domains {
            let Some(list) = s.by_domain.get(d) else {
                continue;
            };
            for (i, c) in list.iter().enumerate() {
                if c.expires.is_some_and(|e| e <= now) {
                    expired = true;
                    continue;
                }
                if c.partition.as_deref() != cx.partition
                    || (c.host_only && c.domain != host)
                    || !path_matches(path, &c.path)
                    || (c.secure && !secure)
                    || (c.http_only && !http)
                    || !cx.admits(c.same_site)
                {
                    continue;
                }
                hits.push((d, i, c.path.len(), c.created));
            }
        }
        // Longer paths first, then older cookies first.
        hits.sort_by(|a, b| b.2.cmp(&a.2).then(a.3.cmp(&b.3)));
        let mut out = String::new();
        for &(d, i, _, _) in &hits {
            let c = &mut s.by_domain.get_mut(d).expect("bucket")[i];
            c.last_access = stamp;
            if !out.is_empty() {
                out.push_str("; ");
            }
            if !c.name.is_empty() {
                out.push_str(&c.name);
                out.push('=');
            }
            out.push_str(&c.value);
        }
        if expired {
            s.purge_expired(now);
        }
        (!hits.is_empty()).then_some(out)
    }

    fn set(&self, url: &Url, set_cookie: &str, cx: CookieContext, http: bool) {
        let Some(host) = cookie_host(url) else {
            return;
        };
        let now = now_secs();
        let Some(parsed) = parse_set_cookie(set_cookie, now) else {
            return;
        };
        let secure_url = is_secure(url);
        // Domain: a public suffix is allowed only as the host itself.
        let (domain, host_only) = match parsed.domain {
            Some(d) if is_public_suffix(&d) => {
                if d == host {
                    (host.clone(), true)
                } else {
                    return;
                }
            }
            Some(d) => {
                if !domain_matches(&host, &d) {
                    return;
                }
                (d, false)
            }
            None => (host.clone(), true),
        };
        let path = parsed.path.unwrap_or_else(|| default_path(url));
        if (parsed.secure && !secure_url) || (parsed.http_only && !http) {
            return;
        }
        if parsed.same_site == SameSite::None && !parsed.secure {
            return;
        }
        if parsed.same_site != SameSite::None && cx.allows != SameSite::Strict && !cx.top_level {
            return;
        }
        let lower = parsed.name.to_ascii_lowercase();
        if lower.starts_with("__secure-") && !parsed.secure {
            return;
        }
        if lower.starts_with("__host-") && !(parsed.secure && host_only && path == "/") {
            return;
        }
        // Nameless cookies must not look like a prefixed one.
        if parsed.name.is_empty() {
            let v = parsed.value.to_ascii_lowercase();
            if v.starts_with("__secure-") || v.starts_with("__host-") {
                return;
            }
        }
        let mut s = self.lock();
        // Leave secure cookies alone: an insecure origin may not shadow them.
        if !parsed.secure && !secure_url {
            let shadows = s.by_domain.values().flatten().any(|c| {
                c.secure
                    && c.name == parsed.name
                    && (domain_matches(&c.domain, &domain) || domain_matches(&domain, &c.domain))
                    && path_matches(&path, &c.path)
            });
            if shadows {
                return;
            }
        }
        let stamp = s.tick();
        let mut created = stamp;
        let list = s.by_domain.entry(domain.clone()).or_default();
        let mut removed_persistent = false;
        if let Some(i) = list.iter().position(|c| {
            c.name == parsed.name
                && c.host_only == host_only
                && c.path == path
                && c.partition.as_deref() == cx.partition
        }) {
            if list[i].http_only && !http {
                return;
            }
            created = list[i].created;
            removed_persistent = list[i].expires.is_some();
            list.remove(i);
            s.count -= 1;
        }
        let expired = parsed.expires.is_some_and(|e| e <= now);
        if expired {
            if removed_persistent {
                s.dirty = true;
            }
            if s.by_domain.get(&domain).is_some_and(Vec::is_empty) {
                s.by_domain.remove(&domain);
            }
            return;
        }
        let cookie = Cookie {
            name: parsed.name,
            value: parsed.value,
            domain: domain.clone(),
            host_only,
            path,
            expires: parsed.expires,
            secure: parsed.secure,
            http_only: parsed.http_only,
            same_site: parsed.same_site,
            created,
            last_access: stamp,
            partition: cx.partition.map(str::to_owned),
        };
        if cookie.expires.is_some() || removed_persistent {
            s.dirty = true;
        }
        s.by_domain
            .get_mut(&domain)
            .expect("bucket exists")
            .push(cookie);
        s.count += 1;
        s.evict(&domain, now);
    }

    fn load(&self, text: &str) {
        let now = now_secs();
        let mut s = self.lock();
        for line in text.lines() {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let Some(c) = deserialize(line) else { continue };
            if c.expires.is_none_or(|e| e <= now) {
                continue;
            }
            s.clock = s.clock.max(c.created).max(c.last_access);
            s.by_domain.entry(c.domain.clone()).or_default().push(c);
            s.count += 1;
        }
    }
}

impl Store {
    fn tick(&mut self) -> u64 {
        self.clock = now_micros().max(self.clock + 1);
        self.clock
    }

    fn purge_expired(&mut self, now: i64) {
        let mut removed = 0;
        let mut persistent = false;
        self.by_domain.retain(|_, list| {
            list.retain(|c| {
                let keep = c.expires.is_none_or(|e| e > now);
                if !keep {
                    removed += 1;
                    persistent = true;
                }
                keep
            });
            !list.is_empty()
        });
        self.count -= removed;
        self.dirty |= persistent;
    }

    /// Enforce the per-site and total limits: expired cookies go first,
    /// then the least recently used.
    fn evict(&mut self, domain: &str, now: i64) {
        let site = registrable_domain(domain).to_owned();
        let in_site = |d: &str| {
            d.ends_with(site.as_str())
                && (d.len() == site.len() || d.as_bytes()[d.len() - site.len() - 1] == b'.')
        };
        let site_count: usize = self
            .by_domain
            .iter()
            .filter(|(d, _)| in_site(d))
            .map(|(_, l)| l.len())
            .sum();
        if site_count <= MAX_PER_SITE && self.count <= MAX_TOTAL {
            return;
        }
        self.purge_expired(now);
        loop {
            let site_count: usize = self
                .by_domain
                .iter()
                .filter(|(d, _)| in_site(d))
                .map(|(_, l)| l.len())
                .sum();
            let over_site = site_count > MAX_PER_SITE;
            if !over_site && self.count <= MAX_TOTAL {
                return;
            }
            // RFC 6265bis 5.7: non-secure cookies go before secure ones,
            // then the least recently used.
            let victim = self
                .by_domain
                .iter()
                .filter(|(d, _)| !over_site || in_site(d))
                .flat_map(|(d, l)| {
                    l.iter()
                        .enumerate()
                        .map(move |(i, c)| (d, i, (c.secure, c.last_access)))
                })
                .min_by_key(|&(_, _, k)| k)
                .map(|(d, i, _)| (d.clone(), i));
            let Some((d, i)) = victim else { return };
            let list = self.by_domain.get_mut(&d).expect("victim bucket");
            let c = list.remove(i);
            if list.is_empty() {
                self.by_domain.remove(&d);
            }
            self.count -= 1;
            self.dirty |= c.expires.is_some();
        }
    }
}

/// Lowercase host of an http(s) URL; other schemes have no cookies.
fn cookie_host(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    Some(match url.host()? {
        Host::Domain(d) => d.trim_end_matches('.').to_ascii_lowercase(),
        Host::Ipv4(a) => a.to_string(),
        Host::Ipv6(a) => format!("[{a}]"),
    })
}

fn is_ip(host: &str) -> bool {
    host.starts_with('[') || host.parse::<std::net::Ipv4Addr>().is_ok()
}

/// `https`, or a loopback host (Chrome treats those as secure contexts).
pub(crate) fn is_secure(url: &Url) -> bool {
    url.scheme() == "https" || is_loopback(url)
}

pub(crate) fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(d)) => {
            let d = d.trim_end_matches('.');
            d.eq_ignore_ascii_case("localhost") || d.to_ascii_lowercase().ends_with(".localhost")
        }
        Some(Host::Ipv4(a)) => a.is_loopback(),
        Some(Host::Ipv6(a)) => a.is_loopback(),
        None => false,
    }
}

/// RFC 6265bis domain-match: `host` is `domain` or a subdomain of it (IP
/// addresses match only themselves).
fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || (!is_ip(host)
            && host.len() > domain.len()
            && host.ends_with(domain)
            && host.as_bytes()[host.len() - domain.len() - 1] == b'.')
}

/// RFC 6265bis path-match.
fn path_matches(request: &str, cookie: &str) -> bool {
    request == cookie
        || (request.starts_with(cookie)
            && (cookie.ends_with('/') || request.as_bytes().get(cookie.len()) == Some(&b'/')))
}

/// The directory of the URL's path: the default `Path` of a cookie.
fn default_path(url: &Url) -> String {
    let p = url.path();
    match p.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(i) if p.starts_with('/') => p[..i].to_owned(),
        Some(_) => "/".to_owned(),
    }
}

/// `host` is a public suffix (`com`, `co.uk`, `github.io`, an unknown TLD).
fn is_public_suffix(host: &str) -> bool {
    !is_ip(host) && psl::suffix_str(host) == Some(host)
}

/// The registrable domain (eTLD+1) of a lowercase host, or the host itself
/// for IP addresses, single labels and public suffixes.
pub fn registrable_domain(host: &str) -> &str {
    if is_ip(host) {
        return host;
    }
    psl::domain_str(host).unwrap_or(host)
}

/// The site of a top-level document as a partition key:
/// `scheme://registrable-domain`. `None` for opaque origins (`file:`,
/// `data:`): their third-party requests carry no cookies.
pub fn site_key(url: &Url) -> Option<String> {
    let host = cookie_site_host(url)?;
    Some(format!("{}://{}", url.scheme(), registrable_domain(&host)))
}

/// The cookie context of `document.cookie` for a document at `url` under a
/// top-level document at `top_level` (partition key `key`). `None`: no
/// cookies (a cross-site frame under an opaque top-level origin).
fn document_context<'a>(
    url: &Url,
    top_level: &Url,
    key: Option<&'a str>,
) -> Option<CookieContext<'a>> {
    if url == top_level || same_site(url, top_level) {
        return Some(CookieContext::SAME_SITE);
    }
    Some(CookieContext {
        allows: SameSite::None,
        top_level: false,
        partition: Some(key?),
    })
}

/// Schemeful same-site: same scheme and registrable domain. Opaque origins
/// (`file:`, `data:`) are same-site with nothing.
pub fn same_site(a: &Url, b: &Url) -> bool {
    if a.scheme() != b.scheme() {
        return false;
    }
    match (cookie_site_host(a), cookie_site_host(b)) {
        (Some(x), Some(y)) => registrable_domain(&x) == registrable_domain(&y),
        _ => false,
    }
}

fn cookie_site_host(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https" | "ws" | "wss") {
        return None;
    }
    Some(match url.host()? {
        Host::Domain(d) => d.trim_end_matches('.').to_ascii_lowercase(),
        Host::Ipv4(a) => a.to_string(),
        Host::Ipv6(a) => format!("[{a}]"),
    })
}

#[derive(Debug, PartialEq)]
struct Parsed {
    name: String,
    value: String,
    expires: Option<i64>,
    domain: Option<String>,
    path: Option<String>,
    secure: bool,
    http_only: bool,
    same_site: SameSite,
}

fn trim_wsp(s: &str) -> &str {
    s.trim_matches([' ', '\t'])
}

/// RFC 6265bis 5.6: parse a `Set-Cookie` value. `None`: ignore it.
fn parse_set_cookie(s: &str, now: i64) -> Option<Parsed> {
    if s.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f) {
        return None;
    }
    let (pair, attrs) = s.split_once(';').unwrap_or((s, ""));
    let (name, value) = match pair.split_once('=') {
        Some((n, v)) => (trim_wsp(n), trim_wsp(v)),
        None => ("", trim_wsp(pair)),
    };
    if (name.is_empty() && value.is_empty()) || name.len() + value.len() > MAX_NAME_VALUE {
        return None;
    }
    let mut p = Parsed {
        name: name.to_owned(),
        value: value.to_owned(),
        expires: None,
        domain: None,
        path: None,
        secure: false,
        http_only: false,
        same_site: SameSite::Lax,
    };
    let mut max_age: Option<i64> = None;
    let mut expires: Option<i64> = None;
    if attrs.is_empty() {
        return Some(p);
    }
    for av in attrs.split(';') {
        let (k, v) = match av.split_once('=') {
            Some((k, v)) => (trim_wsp(k), trim_wsp(v)),
            None => (trim_wsp(av), ""),
        };
        if v.len() > MAX_ATTRIBUTE {
            continue;
        }
        match k.to_ascii_lowercase().as_str() {
            "expires" => {
                if let Some(t) = parse_cookie_date(v) {
                    expires = Some(t);
                }
            }
            "max-age" => {
                let digits = v.strip_prefix('-').unwrap_or(v);
                if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                    let n: i64 = v.parse().unwrap_or(if v.starts_with('-') {
                        i64::MIN
                    } else {
                        i64::MAX
                    });
                    max_age = Some(if n <= 0 {
                        i64::MIN
                    } else {
                        now.saturating_add(n.min(MAX_AGE_SECS))
                    });
                }
            }
            "domain" => {
                if !v.is_empty() {
                    let d = v.strip_prefix('.').unwrap_or(v).to_ascii_lowercase();
                    // A domain is matched against the punycode host.
                    p.domain = Some(url::Host::parse(&d).map(|h| h.to_string()).unwrap_or(d));
                }
            }
            "path" => {
                p.path = (v.starts_with('/')).then(|| v.to_owned());
            }
            "secure" => p.secure = true,
            "httponly" => p.http_only = true,
            "samesite" => {
                p.same_site = match v.to_ascii_lowercase().as_str() {
                    "none" => SameSite::None,
                    "strict" => SameSite::Strict,
                    _ => SameSite::Lax,
                }
            }
            _ => {}
        }
    }
    p.expires = max_age.or(expires.map(|e| e.min(now.saturating_add(MAX_AGE_SECS))));
    Some(p)
}

/// RFC 6265bis 5.1.1 cookie-date: Unix seconds, or `None` if invalid.
fn parse_cookie_date(s: &str) -> Option<i64> {
    let delim = |c: char| matches!(c, '\t' | ' '..='/' | ';'..='@' | '['..='`' | '{'..='~');
    let (mut time, mut day, mut month, mut year) = (None, None, None, None);
    for tok in s.split(delim).filter(|t| !t.is_empty()) {
        if time.is_none()
            && let Some(t) = parse_time(tok)
        {
            time = Some(t);
            continue;
        }
        if day.is_none()
            && let Some(d) = leading_digits(tok, 1, 2)
        {
            day = Some(d);
            continue;
        }
        // `get`, not a slice: byte 3 may be inside a non-ASCII character
        // ("éé", or a localized month such as "août").
        if month.is_none()
            && let Some(m) = tok.get(..3)
        {
            let m = m.to_ascii_lowercase();
            if let Some(i) = [
                "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
            ]
            .iter()
            .position(|&n| n == m)
            {
                month = Some(i as u32 + 1);
                continue;
            }
        }
        if year.is_none()
            && let Some(y) = leading_digits(tok, 2, 4)
        {
            year = Some(y);
        }
    }
    let (h, mi, sec) = time?;
    let (day, month, mut year) = (day?, month?, year?);
    if (70..=99).contains(&year) {
        year += 1900;
    } else if year <= 69 {
        year += 2000;
    }
    if !(1..=31).contains(&day) || year < 1601 || h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days_in_month = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day > days_in_month[month as usize - 1] {
        return None;
    }
    let days = days_from_civil(year as i64, month, day);
    Some(days * 86400 + i64::from(h) * 3600 + i64::from(mi) * 60 + i64::from(sec))
}

/// `min..=max` digits at the start of `tok`, followed by a non-digit or the end.
fn leading_digits(tok: &str, min: usize, max: usize) -> Option<u32> {
    let n = tok.bytes().take_while(u8::is_ascii_digit).count();
    (min..=max).contains(&n).then(|| tok[..n].parse().ok())?
}

/// `hms-time` at the start of `tok`: 1-2 digits, `:`, 1-2 digits, `:`, 1-2 digits.
fn parse_time(tok: &str) -> Option<(u32, u32, u32)> {
    let mut parts = tok.splitn(3, ':');
    let h = parts.next()?;
    let m = parts.next()?;
    let s = parts.next()?;
    let digits = |x: &str| -> Option<u32> {
        (!x.is_empty() && x.len() <= 2 && x.bytes().all(|b| b.is_ascii_digit()))
            .then(|| x.parse().ok())?
    };
    Some((digits(h)?, digits(m)?, leading_digits(s, 1, 2)?))
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ----- persistence ------------------------------------------------------------

const FILE_HEADER: &str = "# Huginn cookies v1";

fn escape(s: &str) -> String {
    s.replace('%', "%25").replace('\t', "%09")
}

fn unescape(s: &str) -> String {
    s.replace("%09", "\t").replace("%25", "%")
}

fn serialize(s: &Store, now: i64) -> String {
    let mut out = String::from(FILE_HEADER);
    out.push('\n');
    let mut all: Vec<&Cookie> = s
        .by_domain
        .values()
        .flatten()
        .filter(|c| c.expires.is_some_and(|e| e > now))
        .collect();
    all.sort_by_key(|c| c.created);
    for c in all {
        let same_site = match c.same_site {
            SameSite::None => "none",
            SameSite::Lax => "lax",
            SameSite::Strict => "strict",
        };
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            c.domain,
            u8::from(c.host_only),
            escape(&c.path),
            c.expires.unwrap_or(0),
            u8::from(c.secure),
            u8::from(c.http_only),
            same_site,
            c.created,
            c.last_access,
            escape(c.partition.as_deref().unwrap_or("")),
            escape(&c.name),
            escape(&c.value),
        ));
    }
    out
}

fn deserialize(line: &str) -> Option<Cookie> {
    let f: Vec<&str> = line.split('\t').collect();
    let [
        domain,
        host_only,
        path,
        expires,
        secure,
        http_only,
        same_site,
        created,
        last,
        partition,
        name,
        value,
    ] = f.as_slice()
    else {
        return None;
    };
    Some(Cookie {
        name: unescape(name),
        value: unescape(value),
        domain: (*domain).to_owned(),
        host_only: *host_only == "1",
        path: unescape(path),
        expires: Some(expires.parse().ok()?),
        secure: *secure == "1",
        http_only: *http_only == "1",
        same_site: match *same_site {
            "none" => SameSite::None,
            "strict" => SameSite::Strict,
            _ => SameSite::Lax,
        },
        created: created.parse().ok()?,
        last_access: last.parse().ok()?,
        partition: (!partition.is_empty()).then(|| unescape(partition)),
    })
}

/// Write `text` to `path` readable by the owner only: a new, uniquely named
/// temporary file (never an existing file or a symlink), then a rename. A
/// missing profile directory is created owner-only.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
        && !dir.exists()
    {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(dir)?;
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc_o_nofollow());
    }
    let mut f = opts.open(&tmp)?;
    let r = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// `O_NOFOLLOW` without a libc dependency.
#[cfg(unix)]
fn libc_o_nofollow() -> i32 {
    if cfg!(target_os = "linux") {
        0o400000
    } else {
        0x0100
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_ascii_cookie_date_is_invalid_and_does_not_panic() {
        // The month check cut tokens at byte 3, inside a character.
        assert_eq!(parse_cookie_date("éé"), None);
        assert_eq!(parse_cookie_date("12 août 2030 10:00:00"), None);
        // A day may have anything after its digits (RFC 6265bis 5.1.1).
        assert_eq!(
            parse_cookie_date("1é Jan 2030 10:00:00"),
            parse_cookie_date("1 Jan 2030 10:00:00")
        );
        assert!(parse_cookie_date("12 Aug 2030 10:00:00").is_some());
    }

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    const SAME: CookieContext = CookieContext::SAME_SITE;
    const CROSS: CookieContext = CookieContext {
        allows: SameSite::None,
        top_level: false,
        partition: None,
    };
    const CROSS_NAV: CookieContext = CookieContext {
        allows: SameSite::Lax,
        top_level: true,
        partition: None,
    };

    #[test]
    fn cookie_dates() {
        assert_eq!(
            parse_cookie_date("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(1_445_412_480)
        );
        assert_eq!(
            parse_cookie_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        // RFC 850 and asctime forms, two-digit years.
        assert_eq!(
            parse_cookie_date("Sunday, 06-Nov-94 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(
            parse_cookie_date("Sun Nov  6 08:49:37 1994"),
            Some(784_111_777)
        );
        assert_eq!(parse_cookie_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse_cookie_date("Fri, 31 Feb 2020 00:00:00 GMT"), None);
        assert_eq!(parse_cookie_date("Fri, 01 Jan 2020 24:00:00 GMT"), None);
        assert_eq!(parse_cookie_date("tomorrow"), None);
    }

    #[test]
    fn parses_attributes_last_one_wins() {
        let p = parse_set_cookie(
            " id = a b ; Path=/x; path=/y; Secure; HttpOnly; SameSite=Strict; Domain=.Example.COM; Partitioned",
            0,
        )
        .unwrap();
        assert_eq!((p.name.as_str(), p.value.as_str()), ("id", "a b"));
        assert_eq!(p.path.as_deref(), Some("/y"));
        assert_eq!(p.domain.as_deref(), Some("example.com"));
        assert!(p.secure && p.http_only);
        assert_eq!(p.same_site, SameSite::Strict);
        // Max-Age beats Expires whatever the order, and is capped at 400 days.
        let p = parse_set_cookie(
            "a=1; Max-Age=60; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            1000,
        )
        .unwrap();
        assert_eq!(p.expires, Some(1060));
        let p = parse_set_cookie("a=1; Max-Age=999999999999", 0).unwrap();
        assert_eq!(p.expires, Some(MAX_AGE_SECS));
        // Unknown SameSite values mean Lax; control characters reject the cookie.
        assert_eq!(
            parse_set_cookie("a=1; SameSite=bogus", 0)
                .unwrap()
                .same_site,
            SameSite::Lax
        );
        assert_eq!(parse_set_cookie("a=1\u{1}", 0), None);
        assert_eq!(parse_set_cookie("=", 0), None);
        assert_eq!(parse_set_cookie("bare", 0).unwrap().value, "bare");
    }

    #[test]
    fn domain_path_and_public_suffix_rules() {
        let jar = CookieJar::in_memory();
        let page = u("https://www.example.co.uk/a/b/page");
        jar.set_from_header(&page, "site=1; Domain=example.co.uk; Path=/", SAME);
        jar.set_from_header(&page, "suffix=1; Domain=co.uk", SAME);
        jar.set_from_header(&page, "other=1; Domain=evil.co.uk", SAME);
        jar.set_from_header(&page, "host=1", SAME);
        // Default path is the directory of the request path.
        assert_eq!(
            jar.header_for(&u("https://www.example.co.uk/a/b/other"), SAME)
                .as_deref(),
            Some("host=1; site=1"),
            "longer paths first"
        );
        assert_eq!(
            jar.header_for(&u("https://www.example.co.uk/a/"), SAME)
                .as_deref(),
            Some("site=1")
        );
        // Domain cookies reach subdomains; host-only ones do not.
        assert_eq!(
            jar.header_for(&u("https://img.example.co.uk/"), SAME)
                .as_deref(),
            Some("site=1")
        );
        assert_eq!(jar.header_for(&u("https://other.co.uk/"), SAME), None);
        // Path matching respects segment boundaries.
        jar.set_from_header(&page, "p=1; Path=/docs", SAME);
        assert!(
            jar.header_for(&u("https://www.example.co.uk/docs/x"), SAME)
                .unwrap()
                .contains("p=1")
        );
        assert!(
            !jar.header_for(&u("https://www.example.co.uk/docsx"), SAME)
                .unwrap_or_default()
                .contains("p=1")
        );
    }

    #[test]
    fn secure_http_only_and_prefixes() {
        let jar = CookieJar::in_memory();
        let http = u("http://example.com/");
        let https = u("https://example.com/");
        jar.set_from_header(&http, "s=1; Secure", SAME);
        assert_eq!(jar.len(), 0, "Secure from an insecure origin is ignored");
        jar.set_from_header(&https, "s=1; Secure", SAME);
        jar.set_from_header(&http, "s=2", SAME);
        assert_eq!(
            jar.header_for(&https, SAME).as_deref(),
            Some("s=1"),
            "insecure may not shadow secure"
        );
        assert_eq!(jar.header_for(&http, SAME), None);
        jar.set_from_header(&https, "h=1; HttpOnly", SAME);
        assert_eq!(jar.document_cookie(&https, &https), "s=1");
        jar.set_document_cookie(&https, &https, "h=2");
        assert!(
            jar.header_for(&https, SAME).unwrap().contains("h=1"),
            "script cannot overwrite HttpOnly"
        );
        jar.set_document_cookie(&https, &https, "j=1; HttpOnly");
        assert!(!jar.header_for(&https, SAME).unwrap().contains("j="));
        jar.set_from_header(
            &https,
            "__Host-a=1; Secure; Path=/; Domain=example.com",
            SAME,
        );
        jar.set_from_header(&https, "__Secure-b=1", SAME);
        jar.set_from_header(&https, "__Host-c=1; Secure; Path=/", SAME);
        let all = jar.header_for(&https, SAME).unwrap();
        assert!(
            !all.contains("__Host-a") && !all.contains("__Secure-b") && all.contains("__Host-c=1")
        );
        // Loopback counts as secure, like Chrome.
        let local = u("http://127.0.0.1:8000/");
        jar.set_from_header(&local, "l=1; Secure; SameSite=None", SAME);
        assert_eq!(jar.header_for(&local, CROSS).as_deref(), Some("l=1"));
    }

    #[test]
    fn same_site_rules() {
        let jar = CookieJar::in_memory();
        let url = u("https://example.com/");
        jar.set_from_header(&url, "lax=1", SAME);
        jar.set_from_header(&url, "strict=1; SameSite=Strict", SAME);
        jar.set_from_header(&url, "none=1; SameSite=None; Secure", SAME);
        jar.set_from_header(&url, "insecure_none=1; SameSite=None", SAME);
        assert_eq!(
            jar.header_for(&url, SAME).as_deref(),
            Some("lax=1; strict=1; none=1")
        );
        assert_eq!(
            jar.header_for(&url, CROSS_NAV).as_deref(),
            Some("lax=1; none=1")
        );
        assert_eq!(jar.header_for(&url, CROSS).as_deref(), Some("none=1"));
        // Cross-site subresponses may set only SameSite=None cookies;
        // top-level navigations may set any.
        jar.set_from_header(&url, "x=1", CROSS);
        jar.set_from_header(&url, "y=1; SameSite=None; Secure", CROSS);
        jar.set_from_header(&url, "z=1; SameSite=Strict", CROSS_NAV);
        let all = jar.header_for(&url, SAME).unwrap();
        let names: Vec<&str> = all.split("; ").collect();
        assert!(!names.contains(&"x=1") && names.contains(&"y=1") && names.contains(&"z=1"));
        assert!(same_site(
            &u("https://www.bbc.co.uk/"),
            &u("https://bbc.co.uk/x")
        ));
        assert!(!same_site(
            &u("https://www.bbc.co.uk/"),
            &u("https://www.bbc.com/")
        ));
        assert!(!same_site(
            &u("http://example.com/"),
            &u("https://example.com/")
        ));
        assert!(!same_site(
            &u("https://a.github.io/"),
            &u("https://b.github.io/")
        ));
    }

    #[test]
    fn expiry_replace_and_delete() {
        let jar = CookieJar::in_memory();
        let url = u("https://example.com/");
        jar.set_from_header(&url, "a=1; Max-Age=100", SAME);
        jar.set_from_header(&url, "a=2", SAME);
        assert_eq!(jar.header_for(&url, SAME).as_deref(), Some("a=2"));
        assert_eq!(jar.len(), 1);
        jar.set_from_header(&url, "a=; Max-Age=0", SAME);
        assert_eq!(jar.header_for(&url, SAME), None);
        jar.set_document_cookie(&url, &url, "b=1; expires=Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(jar.len(), 0);
        // No cookies outside http(s).
        jar.set_document_cookie(&u("file:///tmp/x.html"), &u("file:///tmp/x.html"), "c=1");
        assert_eq!(
            jar.document_cookie(&u("file:///tmp/x.html"), &u("file:///tmp/x.html")),
            ""
        );
        assert_eq!(jar.len(), 0);
    }

    #[test]
    fn per_site_limit_evicts_least_recently_used() {
        let jar = CookieJar::in_memory();
        let url = u("https://example.com/");
        for i in 0..MAX_PER_SITE + 5 {
            jar.set_from_header(&url, &format!("c{i}=1"), SAME);
        }
        assert_eq!(jar.len(), MAX_PER_SITE);
        let all = jar.header_for(&url, SAME).unwrap();
        assert!(!all.contains("c0=") && all.contains(&format!("c{}=", MAX_PER_SITE + 4)));
    }

    #[test]
    fn insecure_flood_cannot_evict_a_secure_cookie() {
        let jar = CookieJar::in_memory();
        let https = u("https://example.com/");
        let http = u("http://example.com/");
        jar.set_from_header(&https, "session=good; Secure", SAME);
        for i in 0..MAX_PER_SITE + 5 {
            jar.set_from_header(&http, &format!("junk{i}=1"), SAME);
        }
        jar.set_from_header(&http, "session=evil", SAME);
        assert_eq!(jar.len(), MAX_PER_SITE);
        let sent = jar.header_for(&https, SAME).unwrap();
        assert!(sent.contains("session=good") && !sent.contains("session=evil"));
    }

    #[test]
    fn third_party_cookies_are_partitioned_by_top_level_site() {
        let jar = CookieJar::in_memory();
        let tracker = u("https://tracker.example/pixel");
        let a = site_key(&u("https://news.a.com/story")).unwrap();
        let b = site_key(&u("https://shop.b.com/")).unwrap();
        assert_eq!(a, "https://a.com");
        fn under(k: &str) -> CookieContext<'_> {
            CookieContext {
                allows: SameSite::None,
                top_level: false,
                partition: Some(k),
            }
        }
        // First-party visit sets the tracker's own cookie.
        jar.set_from_header(&tracker, "id=first; SameSite=None; Secure", SAME);
        // Embedded under a.com: the first-party cookie is not visible.
        assert_eq!(jar.header_for(&tracker, under(&a)), None);
        jar.set_from_header(&tracker, "id=in-a; SameSite=None; Secure", under(&a));
        assert_eq!(
            jar.header_for(&tracker, under(&a)).as_deref(),
            Some("id=in-a")
        );
        // Under b.com it is another, empty partition.
        assert_eq!(jar.header_for(&tracker, under(&b)), None);
        // The first-party cookie is untouched.
        assert_eq!(jar.header_for(&tracker, SAME).as_deref(), Some("id=first"));
        // document.cookie in a cross-site frame sees only its partition.
        let top_a = u("https://news.a.com/story");
        assert_eq!(jar.document_cookie(&tracker, &top_a), "id=in-a");
        jar.set_document_cookie(&tracker, &top_a, "js=1; SameSite=None; Secure");
        assert_eq!(jar.document_cookie(&tracker, &top_a), "id=in-a; js=1");
        assert_eq!(jar.document_cookie(&tracker, &tracker), "id=first");
        // Under an opaque top-level origin a third party gets no cookies.
        assert_eq!(jar.document_cookie(&tracker, &u("file:///x.html")), "");
    }

    #[test]
    fn partitions_survive_save_and_load() {
        let dir = std::env::temp_dir().join(format!("huginn-cookies-part-{}", std::process::id()));
        let path = dir.join("cookies.tsv");
        let tracker = u("https://tracker.example/");
        let cx = CookieContext {
            allows: SameSite::None,
            top_level: false,
            partition: Some("https://a.com"),
        };
        let jar = CookieJar::persistent(path.clone());
        jar.set_from_header(&tracker, "p=1; Max-Age=60; SameSite=None; Secure", cx);
        jar.save().unwrap();
        let jar = CookieJar::persistent(path);
        assert_eq!(jar.header_for(&tracker, cx).as_deref(), Some("p=1"));
        assert_eq!(jar.header_for(&tracker, SAME), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn save_makes_an_owner_only_dir_and_file_and_follows_no_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("huginn-cookies-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("profile");
        let path = dir.join("cookies.tsv");
        let jar = CookieJar::persistent(path.clone());
        jar.set_from_header(&u("https://example.com/"), "a=1; Max-Age=60", SAME);
        jar.save().unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        // A world-readable file planted at the old fixed temp name, a
        // symlink to it, and a world-readable cookies.tsv change nothing.
        let planted = root.join("planted");
        std::fs::write(&planted, "x").unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o666)).unwrap();
        std::os::unix::fs::symlink(&planted, dir.join("cookies.tmp")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        jar.set_from_header(&u("https://example.com/"), "b=1; Max-Age=60", SAME);
        jar.save().unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read_to_string(&planted).unwrap(), "x");
        assert!(
            !std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn persistent_cookies_survive_save_and_load() {
        let dir = std::env::temp_dir().join(format!("huginn-cookies-{}", std::process::id()));
        let path = dir.join("cookies.tsv");
        let _ = std::fs::remove_file(&path);
        let url = u("https://example.com/");
        {
            let jar = CookieJar::persistent(path.clone());
            jar.set_from_header(
                &url,
                "keep=a%b\tc; Max-Age=3600; HttpOnly; SameSite=Strict",
                SAME,
            );
            jar.set_from_header(&url, "session=1", SAME);
            jar.save().unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "cookie file must be private");
        }
        let jar = CookieJar::persistent(path.clone());
        assert_eq!(jar.header_for(&url, SAME).as_deref(), Some("keep=a%b\tc"));
        assert_eq!(
            jar.document_cookie(&url, &url),
            "",
            "HttpOnly survives the round trip"
        );
        jar.clear();
        jar.save().unwrap();
        assert!(CookieJar::persistent(path.clone()).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
