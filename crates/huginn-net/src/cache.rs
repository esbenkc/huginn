//! The HTTP cache (RFC 9111) of a profile, shared by all its tabs.
//!
//! It is a private cache, like a browser's. The loader asks it before each
//! `GET` exchange (every redirect hop on its own):
//!
//! - A **fresh** entry answers without a request. Freshness comes from
//!   `Cache-Control: max-age`, else `Expires`, else Chrome's heuristics:
//!   10 % of the time since `Last-Modified` for `200`, `203` and `206`, and
//!   no limit for the permanent `300`, `301`, `308` and `410`.
//!   `no-cache` (and `Pragma: no-cache`) makes an entry stale at once.
//! - A **stale** entry with a validator (`ETag`, `Last-Modified`) turns the
//!   request into a conditional one (`If-None-Match`, `If-Modified-Since`).
//!   A `304` refreshes the stored headers and the stored body answers.
//! - `no-store` (in the request or the response) and `Vary: *` keep a
//!   response out. An entry is used only when every request header its
//!   `Vary` names has the value it had when it was stored; one variant is
//!   kept per URL, like Chrome.
//! - Entries are keyed by the top-level site and the URL (Chrome's cache
//!   partitioning), so a site cannot tell from the cache what other sites
//!   loaded. `Set-Cookie` is never stored.
//!
//! The normal profile stores entries on disk (one file per entry, written
//! through a temp file and a rename, so tabs and processes that share the
//! directory never read half an entry), with a size cap: the least recently
//! used entries go first. Private windows keep theirs in memory. `huginn
//! render` uses no cache, so every benchmark run loads from the network.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use crate::cookies::parse_cookie_date;

/// How a load uses the HTTP cache: Chrome's load types.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum LoadKind {
    /// A new navigation: the cache as the response headers allow.
    #[default]
    Normal,
    /// Reload: the document is revalidated (`Cache-Control: max-age=0`);
    /// subresources come from the cache as usual (Chrome since 2017).
    Reload,
    /// Hard reload (Shift): nothing comes from the cache; requests carry
    /// `Cache-Control: no-cache` and `Pragma: no-cache`, and the answers
    /// replace the entries.
    HardReload,
    /// Back, forward, or a discarded tab coming back: the document comes
    /// from the cache even when stale; subresources as usual.
    History,
}

impl LoadKind {
    pub(crate) fn document(self) -> CacheMode {
        match self {
            LoadKind::Normal => CacheMode::Default,
            LoadKind::Reload => CacheMode::NoCache,
            LoadKind::HardReload => CacheMode::Reload,
            LoadKind::History => CacheMode::ForceCache,
        }
    }

    pub(crate) fn subresources(self) -> CacheMode {
        match self {
            LoadKind::HardReload => CacheMode::Reload,
            _ => CacheMode::Default,
        }
    }
}

/// How one request uses the cache (the Fetch standard's cache mode).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum CacheMode {
    /// Fresh entries answer; stale ones are revalidated.
    #[default]
    Default,
    /// The network only, and the response is not stored.
    NoStore,
    /// The network only; the response is stored.
    Reload,
    /// Every entry is revalidated first, fresh or not.
    NoCache,
    /// Any matching entry answers, stale or not.
    ForceCache,
    /// Only a fresh entry answers; without one nothing is sent and the
    /// request fails.
    OnlyFresh,
}

/// A stored response.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    pub status: u16,
    /// Response headers, names lowercase, without `Set-Cookie`.
    pub headers: Vec<(String, String)>,
    /// Each request header that `Vary` names, with the value the request
    /// had (`None`: it had none).
    pub vary: Vec<(String, Option<String>)>,
    /// When the request was sent and when the response head arrived (Unix ms).
    pub request_time: u64,
    pub response_time: u64,
    pub body: Vec<u8>,
}

/// The current time in Unix ms.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The `Cache-Control` directives the cache acts on (and `Pragma: no-cache`).
#[derive(Default, Debug)]
pub(crate) struct Directives {
    pub no_store: bool,
    pub no_cache: bool,
    pub must_revalidate: bool,
    pub max_age: Option<u64>,
}

impl Directives {
    pub(crate) fn parse<'a>(headers: impl Iterator<Item = (&'a str, &'a str)>) -> Directives {
        let mut d = Directives::default();
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("pragma") {
                if value
                    .split(',')
                    .any(|t| t.trim().eq_ignore_ascii_case("no-cache"))
                {
                    d.no_cache = true;
                }
                continue;
            }
            if !name.eq_ignore_ascii_case("cache-control") {
                continue;
            }
            for dir in value.split(',') {
                let (k, v) = match dir.split_once('=') {
                    Some((k, v)) => (k.trim(), Some(v.trim().trim_matches('"'))),
                    None => (dir.trim(), None),
                };
                match k.to_ascii_lowercase().as_str() {
                    "no-store" => d.no_store = true,
                    // `no-cache="field"` only limits what may be reused; treat
                    // it like a plain `no-cache` (always revalidate).
                    "no-cache" => d.no_cache = true,
                    "must-revalidate" => d.must_revalidate = true,
                    // The first valid `max-age` wins (RFC 9111 4.2.1); a bad
                    // value makes the response stale.
                    "max-age" if d.max_age.is_none() => {
                        d.max_age = Some(v.and_then(|v| v.parse().ok()).unwrap_or(0));
                    }
                    _ => {}
                }
            }
        }
        d
    }
}

/// HTTP-date to Unix ms.
fn parse_date(s: &str) -> Option<u64> {
    parse_cookie_date(s).and_then(|t| u64::try_from(t).ok()?.checked_mul(1000))
}

impl Entry {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn directives(&self) -> Directives {
        Directives::parse(self.headers.iter().map(|(n, v)| (n.as_str(), v.as_str())))
    }

    /// `Date`, else when the response arrived.
    fn date(&self) -> u64 {
        self.header("date")
            .and_then(parse_date)
            .unwrap_or(self.response_time)
    }

    /// How long the response is fresh, in ms (Chrome's
    /// `GetFreshnessLifetimes`, with RFC 9111's rule that an invalid
    /// `Expires` means "already expired").
    fn freshness_lifetime(&self) -> u64 {
        let d = self.directives();
        if d.no_cache || d.no_store {
            return 0;
        }
        if let Some(age) = d.max_age {
            return age.saturating_mul(1000);
        }
        let date = self.date();
        if let Some(expires) = self.header("expires") {
            return parse_date(expires).map_or(0, |e| e.saturating_sub(date));
        }
        match self.status {
            200 | 203 | 206 if !d.must_revalidate => self
                .header("last-modified")
                .and_then(parse_date)
                .filter(|&lm| lm < date)
                .map_or(0, |lm| (date - lm) / 10),
            300 | 301 | 308 | 410 => u64::MAX,
            _ => 0,
        }
    }

    /// The response's age now, in ms (RFC 9111 4.2.3).
    fn current_age(&self, now: u64) -> u64 {
        let age_value = self
            .header("age")
            .and_then(|a| a.trim().parse::<u64>().ok())
            .map_or(0, |a| a.saturating_mul(1000));
        let apparent_age = self.response_time.saturating_sub(self.date());
        let response_delay = self.response_time.saturating_sub(self.request_time);
        let corrected_initial_age = apparent_age.max(age_value.saturating_add(response_delay));
        corrected_initial_age.saturating_add(now.saturating_sub(self.response_time))
    }

    /// Whether the entry may answer without asking the server.
    pub(crate) fn is_fresh(&self, now: u64) -> bool {
        self.freshness_lifetime() > self.current_age(now)
    }

    /// `ETag` and `Last-Modified`, for a conditional request.
    pub(crate) fn validators(&self) -> (Option<&str>, Option<&str>) {
        (self.header("etag"), self.header("last-modified"))
    }

    /// Whether the request headers `value_of` gives match those this entry
    /// was stored for (`Vary`).
    pub(crate) fn matches(&self, value_of: impl Fn(&str) -> Option<String>) -> bool {
        self.vary.iter().all(|(n, v)| value_of(n) == *v)
    }

    /// Take the header fields of a `304` (RFC 9111 4.3.4): each one
    /// replaces the stored field of the same name, except fields that
    /// describe the stored body or the connection.
    pub(crate) fn refresh(
        &mut self,
        headers: &[(String, String)],
        request_time: u64,
        response_time: u64,
    ) {
        const KEEP: [&str; 6] = [
            "content-length",
            "content-encoding",
            "transfer-encoding",
            "connection",
            "keep-alive",
            "content-range",
        ];
        let fresh: Vec<&(String, String)> = headers
            .iter()
            .filter(|(n, _)| !KEEP.contains(&n.as_str()))
            .collect();
        self.headers
            .retain(|(n, _)| !fresh.iter().any(|(f, _)| f == n));
        self.headers.extend(fresh.into_iter().cloned());
        self.request_time = request_time;
        self.response_time = response_time;
    }

    fn size(&self) -> u64 {
        let h: usize = self.headers.iter().map(|(n, v)| n.len() + v.len()).sum();
        (self.body.len() + h + 64) as u64
    }
}

/// The request headers named by a response's `Vary` (lowercase), or `None`
/// for `Vary: *` (never reusable).
pub(crate) fn vary_names(headers: &[(String, String)]) -> Option<Vec<String>> {
    let mut names = Vec::new();
    for (_, v) in headers.iter().filter(|(n, _)| n == "vary") {
        for name in v.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            if name == "*" {
                return None;
            }
            let name = name.to_ascii_lowercase();
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    Some(names)
}

/// Whether a response to a `GET` may be stored (RFC 9111 section 3) and is
/// worth storing: fresh for a while, or with a validator. Redirects other
/// than the permanent ones need explicit freshness.
pub(crate) fn storable(entry: &Entry) -> bool {
    let d = entry.directives();
    if d.no_store || vary_names(&entry.headers).is_none() {
        return false;
    }
    let lifetime = entry.freshness_lifetime();
    match entry.status {
        200 | 203 | 204 | 300 | 404 | 405 | 410 | 414 | 501 => {
            lifetime > 0 || entry.validators() != (None, None)
        }
        301 | 302 | 303 | 307 | 308 => lifetime > 0,
        _ => false,
    }
}

/// The HTTP cache of a profile. Cheap to share: put it in one
/// [`Network`](crate::Network).
pub struct HttpCache {
    store: Store,
}

enum Store {
    Off,
    Memory(Mutex<Memory>),
    Disk(Disk),
}

impl HttpCache {
    /// No cache: every request goes to the network (`huginn render`).
    pub fn off() -> HttpCache {
        HttpCache { store: Store::Off }
    }

    /// Entries in memory, at most `max_bytes` (private windows).
    pub fn in_memory(max_bytes: u64) -> HttpCache {
        HttpCache {
            store: Store::Memory(Mutex::new(Memory {
                max: max_bytes,
                size: 0,
                clock: 0,
                entries: HashMap::new(),
            })),
        }
    }

    /// Entries as files in `dir` (made on the first write, mode 0700), at
    /// most `max_bytes` together. Other processes may share the directory.
    /// A thread reads the sizes of the entries now (some 25 ms for 8000
    /// files), so the first page load does not wait for it.
    pub fn on_disk(dir: impl Into<PathBuf>, max_bytes: u64) -> HttpCache {
        let dir = dir.into();
        let index = Arc::new(Mutex::new(None));
        let (scan_dir, scan_index) = (dir.clone(), index.clone());
        let _ = std::thread::Builder::new()
            .name("cache-index".into())
            .spawn(move || {
                let found = Disk::scan(&scan_dir);
                scan_index.lock().get_or_insert(found);
            });
        HttpCache {
            store: Store::Disk(Disk {
                dir,
                max: max_bytes,
                index,
            }),
        }
    }

    pub(crate) fn is_off(&self) -> bool {
        matches!(self.store, Store::Off)
    }

    pub(crate) fn get(&self, key: &str) -> Option<Entry> {
        match &self.store {
            Store::Off => None,
            Store::Memory(m) => {
                let mut m = m.lock();
                m.clock += 1;
                let clock = m.clock;
                let (e, used) = m.entries.get_mut(key)?;
                *used = clock;
                Some(e.clone())
            }
            Store::Disk(d) => d.get(key),
        }
    }

    pub(crate) fn put(&self, key: &str, entry: &Entry) {
        match &self.store {
            Store::Off => {}
            Store::Memory(m) => m.lock().put(key, entry),
            Store::Disk(d) => d.put(key, entry),
        }
    }

    pub(crate) fn remove(&self, key: &str) {
        match &self.store {
            Store::Off => {}
            Store::Memory(m) => {
                let mut m = m.lock();
                if let Some((e, _)) = m.entries.remove(key) {
                    m.size -= e.size();
                }
            }
            Store::Disk(d) => d.remove(key),
        }
    }

    /// Remove every entry (clearing browsing data).
    pub fn clear(&self) {
        match &self.store {
            Store::Off => {}
            Store::Memory(m) => {
                let mut m = m.lock();
                m.entries.clear();
                m.size = 0;
            }
            Store::Disk(d) => d.clear(),
        }
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        match &self.store {
            Store::Off => 0,
            Store::Memory(m) => m.lock().entries.len(),
            Store::Disk(d) => d.with_index(|i| i.files.len()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct Memory {
    max: u64,
    size: u64,
    clock: u64,
    entries: HashMap<String, (Entry, u64)>,
}

impl Memory {
    fn put(&mut self, key: &str, entry: &Entry) {
        let size = entry.size() + key.len() as u64;
        if size > self.max / 8 {
            return;
        }
        self.clock += 1;
        if let Some((old, _)) = self
            .entries
            .insert(key.to_owned(), (entry.clone(), self.clock))
        {
            self.size -= old.size() + key.len() as u64;
        }
        self.size += size;
        while self.size > self.max {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some((e, _)) = self.entries.remove(&oldest) {
                self.size -= e.size() + oldest.len() as u64;
            }
        }
    }
}

/// Entry files in one directory: `<16 hex digits>` (FNV-1a of the key).
/// The file holds the key, so a hash collision is a miss.
struct Disk {
    dir: PathBuf,
    max: u64,
    /// Size and last use (Unix ms) per file, read from the directory by a
    /// thread (or by the first write, if that comes first). Other
    /// processes' writes show up when read.
    index: Arc<Mutex<Option<Index>>>,
}

/// Numbers temp file names: unique in the process (several caches may
/// share a directory), and the process id makes them unique between
/// processes.
static TMP: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct Index {
    files: HashMap<u64, (u64, u64)>,
    total: u64,
}

/// File format version: the first 8 bytes of every entry file.
const MAGIC: &[u8; 8] = b"HUGNC001";

fn fnv1a(key: &str) -> u64 {
    key.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl Disk {
    fn path(&self, hash: u64) -> PathBuf {
        self.dir.join(format!("{hash:016x}"))
    }

    /// Run `f` on the index, reading the directory first if needed.
    fn with_index<R>(&self, f: impl FnOnce(&mut Index) -> R) -> R {
        let mut guard = self.index.lock();
        let index = guard.get_or_insert_with(|| Disk::scan(&self.dir));
        f(index)
    }

    /// Read the directory: entry sizes and modification times. Leftover
    /// temp files older than an hour (a crash mid-write) are removed.
    fn scan(dir: &std::path::Path) -> Index {
        let mut index = Index::default();
        let Ok(dir) = std::fs::read_dir(dir) else {
            return index;
        };
        let now = SystemTime::now();
        for e in dir.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            let Ok(meta) = e.metadata() else { continue };
            if name.starts_with("tmp-") {
                let old = meta
                    .modified()
                    .ok()
                    .and_then(|m| now.duration_since(m).ok())
                    .is_some_and(|age| age.as_secs() > 3600);
                if old {
                    let _ = std::fs::remove_file(e.path());
                }
                continue;
            }
            let Ok(hash) = u64::from_str_radix(name, 16) else {
                continue;
            };
            let used = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as u64);
            index.files.insert(hash, (meta.len(), used));
            index.total += meta.len();
        }
        index
    }

    fn get(&self, key: &str) -> Option<Entry> {
        let hash = fnv1a(key);
        let bytes = std::fs::read(self.path(hash)).ok()?;
        let (stored_key, entry) = decode(&bytes)?;
        if stored_key != key {
            return None;
        }
        // A hit counts as a use for eviction (before the index is read, the
        // file's age stands in).
        let len = bytes.len() as u64;
        if let Some(i) = self.index.lock().as_mut() {
            let old = i.files.insert(hash, (len, now_ms()));
            i.total = i.total + len - old.map_or(0, |(s, _)| s);
        }
        Some(entry)
    }

    fn put(&self, key: &str, entry: &Entry) {
        let bytes = encode(key, entry);
        let len = bytes.len() as u64;
        if len > self.max / 8 {
            return;
        }
        let hash = fnv1a(key);
        if let Err(e) = self.write(hash, &bytes) {
            eprintln!("huginn: cache: cannot write {}: {e}", self.dir.display());
            return;
        }
        let evict = self.with_index(|i| {
            let old = i.files.insert(hash, (len, now_ms()));
            i.total = i.total + len - old.map_or(0, |(s, _)| s);
            let mut evict = Vec::new();
            if i.total > self.max {
                // Down to 90 %, so the next writes do not evict again at once.
                let mut by_age: Vec<(u64, u64, u64)> =
                    i.files.iter().map(|(&h, &(s, u))| (u, h, s)).collect();
                by_age.sort_unstable();
                for (_, h, s) in by_age {
                    if i.total <= self.max / 10 * 9 {
                        break;
                    }
                    i.files.remove(&h);
                    i.total -= s;
                    evict.push(h);
                }
            }
            evict
        });
        for h in evict {
            let _ = std::fs::remove_file(self.path(h));
        }
    }

    /// Write the whole entry to a new temp file, then rename it over the
    /// entry: readers see the old entry or the new one, never a mix.
    fn write(&self, hash: u64, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&self.dir)?;
        let tmp = self.dir.join(format!(
            "tmp-{}-{}",
            std::process::id(),
            TMP.fetch_add(1, Ordering::Relaxed)
        ));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let written = opts.open(&tmp).and_then(|mut f| f.write_all(bytes));
        match written.and_then(|()| std::fs::rename(&tmp, self.path(hash))) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    fn remove(&self, key: &str) {
        let hash = fnv1a(key);
        if std::fs::remove_file(self.path(hash)).is_ok() {
            self.with_index(|i| {
                if let Some((s, _)) = i.files.remove(&hash) {
                    i.total -= s;
                }
            });
        }
    }

    fn clear(&self) {
        let files: Vec<u64> = self.with_index(|i| {
            i.total = 0;
            i.files.drain().map(|(h, _)| h).collect()
        });
        for h in files {
            let _ = std::fs::remove_file(self.path(h));
        }
    }
}

/// `MAGIC`, then the key, the times, the status, the `Vary` values, the
/// headers and the body. Lengths are little-endian u32 (u64 for the body).
fn encode(key: &str, e: &Entry) -> Vec<u8> {
    fn str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut out = Vec::with_capacity(e.size() as usize + key.len() + 64);
    out.extend_from_slice(MAGIC);
    str(&mut out, key);
    out.extend_from_slice(&e.request_time.to_le_bytes());
    out.extend_from_slice(&e.response_time.to_le_bytes());
    out.extend_from_slice(&e.status.to_le_bytes());
    out.extend_from_slice(&(e.vary.len() as u32).to_le_bytes());
    for (n, v) in &e.vary {
        str(&mut out, n);
        out.push(u8::from(v.is_some()));
        str(&mut out, v.as_deref().unwrap_or(""));
    }
    out.extend_from_slice(&(e.headers.len() as u32).to_le_bytes());
    for (n, v) in &e.headers {
        str(&mut out, n);
        str(&mut out, v);
    }
    out.extend_from_slice(&(e.body.len() as u64).to_le_bytes());
    out.extend_from_slice(&e.body);
    out
}

/// The key and entry of a file; `None` if it is cut short or not ours.
fn decode(bytes: &[u8]) -> Option<(&str, Entry)> {
    struct Reader<'a>(&'a [u8]);
    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            if self.0.len() < n {
                return None;
            }
            let (a, b) = self.0.split_at(n);
            self.0 = b;
            Some(a)
        }
        fn u32(&mut self) -> Option<u32> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
        }
        fn u64(&mut self) -> Option<u64> {
            Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
        }
        fn str(&mut self) -> Option<&'a str> {
            let n = self.u32()? as usize;
            std::str::from_utf8(self.take(n)?).ok()
        }
    }
    let mut r = Reader(bytes);
    if r.take(8)? != MAGIC {
        return None;
    }
    let key = r.str()?;
    let request_time = r.u64()?;
    let response_time = r.u64()?;
    let status = u16::from_le_bytes(r.take(2)?.try_into().ok()?);
    let n = r.u32()? as usize;
    let mut vary = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        let name = r.str()?.to_owned();
        let present = r.take(1)?[0] == 1;
        let value = r.str()?;
        vary.push((name, present.then(|| value.to_owned())));
    }
    let n = r.u32()? as usize;
    let mut headers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        headers.push((r.str()?.to_owned(), r.str()?.to_owned()));
    }
    let len = usize::try_from(r.u64()?).ok()?;
    let body = r.take(len)?.to_vec();
    if !r.0.is_empty() {
        return None;
    }
    Some((
        key,
        Entry {
            status,
            headers,
            vary,
            request_time,
            response_time,
            body,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: u64 = 1_700_000_000_000;

    fn entry(status: u16, headers: &[(&str, &str)]) -> Entry {
        Entry {
            status,
            headers: headers
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect(),
            vary: Vec::new(),
            request_time: T - 100,
            response_time: T,
            body: b"body".to_vec(),
        }
    }

    fn http_date(ms: u64) -> String {
        // 2023-11-14T22:13:20Z is T; dates in the tests are T plus whole days.
        let days = (ms as i64 - T as i64) / 86_400_000;
        format!("{} Nov 2023 22:13:20 GMT", 14 + days)
    }

    #[test]
    fn max_age_beats_expires_and_counts_age() {
        let e = entry(200, &[("cache-control", "max-age=60"), ("expires", "0")]);
        assert!(e.is_fresh(T + 59_000));
        assert!(!e.is_fresh(T + 60_000));
        // A response that was already 50 s old when it arrived (`Age`).
        let e = entry(200, &[("cache-control", "max-age=60"), ("age", "50")]);
        assert!(e.is_fresh(T + 9_000));
        assert!(!e.is_fresh(T + 10_000));
        // The first max-age counts; a bad one means stale.
        let e = entry(200, &[("cache-control", "max-age=x, max-age=60")]);
        assert!(!e.is_fresh(T));
    }

    #[test]
    fn expires_is_relative_to_date() {
        let date = http_date(T);
        let tomorrow = http_date(T + 86_400_000);
        let e = entry(200, &[("date", &date), ("expires", &tomorrow)]);
        assert!(e.is_fresh(T + 86_000_000));
        assert!(!e.is_fresh(T + 86_400_000));
        // An invalid Expires means already expired.
        let e = entry(200, &[("expires", "-1")]);
        assert!(!e.is_fresh(T));
    }

    #[test]
    fn heuristic_and_permanent_freshness() {
        let date = http_date(T);
        let ten_days_ago = http_date(T - 10 * 86_400_000);
        let e = entry(200, &[("date", &date), ("last-modified", &ten_days_ago)]);
        // 10 % of 10 days.
        assert!(e.is_fresh(T + 86_000_000));
        assert!(!e.is_fresh(T + 86_400_000));
        let mut e2 = e.clone();
        e2.headers
            .push(("cache-control".into(), "must-revalidate".into()));
        assert!(!e2.is_fresh(T));
        // Only 200, 203 and 206 get the heuristic.
        let mut e3 = e.clone();
        e3.status = 404;
        assert!(!e3.is_fresh(T));
        // Permanent redirects are fresh with no headers at all.
        assert!(entry(301, &[]).is_fresh(T + 365 * 86_400_000));
        assert!(!entry(302, &[]).is_fresh(T));
    }

    #[test]
    fn no_cache_and_pragma_mean_stale() {
        assert!(!entry(200, &[("cache-control", "no-cache, max-age=600")]).is_fresh(T));
        assert!(
            !entry(
                200,
                &[("pragma", "no-cache"), ("cache-control", "max-age=600")]
            )
            .is_fresh(T)
        );
    }

    #[test]
    fn what_is_stored() {
        assert!(storable(&entry(200, &[("cache-control", "max-age=60")])));
        assert!(storable(&entry(200, &[("etag", "\"a\"")])));
        // Neither fresh nor revalidatable: no use storing it.
        assert!(!storable(&entry(200, &[])));
        assert!(!storable(&entry(
            200,
            &[("cache-control", "max-age=60, no-store")]
        )));
        assert!(!storable(&entry(
            200,
            &[("cache-control", "max-age=60"), ("vary", "Accept, *")]
        )));
        assert!(storable(&entry(301, &[])));
        assert!(!storable(&entry(302, &[("etag", "\"a\"")])));
        assert!(storable(&entry(302, &[("cache-control", "max-age=5")])));
        assert!(!storable(&entry(206, &[("cache-control", "max-age=5")])));
    }

    #[test]
    fn refresh_keeps_body_fields() {
        let mut e = entry(
            200,
            &[("etag", "\"1\""), ("content-length", "4"), ("x-a", "old")],
        );
        e.refresh(
            &[
                ("etag".into(), "\"2\"".into()),
                ("content-length".into(), "0".into()),
                ("cache-control".into(), "max-age=9".into()),
            ],
            T + 5,
            T + 10,
        );
        assert_eq!(e.validators().0, Some("\"2\""));
        assert_eq!(e.header("content-length"), Some("4"));
        assert_eq!(e.header("x-a"), Some("old"));
        assert!(e.is_fresh(T + 10 + 8_000));
        assert_eq!(e.body, b"body");
    }

    #[test]
    fn file_format_round_trips_and_rejects_damage() {
        let mut e = entry(200, &[("etag", "\"1\"")]);
        e.vary = vec![
            ("accept".into(), Some("text/css".into())),
            ("origin".into(), None),
        ];
        let bytes = encode("https://a.test https://a.test/x", &e);
        let (key, back) = decode(&bytes).unwrap();
        assert_eq!(key, "https://a.test https://a.test/x");
        assert_eq!(back, e);
        assert!(decode(&bytes[..bytes.len() - 1]).is_none());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(decode(&longer).is_none());
    }

    #[test]
    fn memory_store_evicts_least_recently_used() {
        let cache = HttpCache::in_memory(8 * 300);
        let mut e = entry(200, &[("cache-control", "max-age=60")]);
        e.body = vec![0; 200];
        for k in ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"] {
            cache.put(k, &e);
            // Keep `a` in use.
            assert!(cache.get("a").is_some());
        }
        assert!(cache.get("a").is_some());
        assert!(cache.get("b").is_none());
        assert!(cache.get("j").is_some());
    }
}
