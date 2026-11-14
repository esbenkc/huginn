//! Stylesheets parsed on any thread, and the process-wide cache of parsed
//! stylesheets.
//!
//! The shell parses each stylesheet on the thread that fetched it, as soon
//! as its bytes are in ([`parse_stylesheet`]); the render thread only adds
//! the parsed sheets to its engine
//! ([`StyleEngine::add_parsed_stylesheets`](crate::StyleEngine::add_parsed_stylesheets)).
//! Parsed CSS is never changed afterwards (scripts' CSSOM writes `<style>`
//! text, see huginn-js), so one parse serves every engine of the process:
//! all of them read stylesheets under one lock ([`lock`]).
//!
//! The cache keeps parsed sheets by origin, URL and text, as Chrome keeps
//! parsed stylesheets in its memory cache: a second `<link>` to the same
//! sheet, a revisit, a reload, another tab and a restyle after scripts
//! use the same parse. Sheets with `@import` are not kept (the imported
//! sheets are not part of the key), as in Blink.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc as StdArc, LazyLock, Mutex, MutexGuard, OnceLock};

use style::context::QuirksMode;
use style::media_queries::MediaList;
use style::servo_arc::Arc;
use style::shared_lock::{SharedRwLock, SharedRwLockReadGuard};
use style::stylesheets::{CssRule, CssRules, Stylesheet, StylesheetContents};
use style::values::AtomIdent;

use crate::{Origin, StylesheetLoader, engine};

/// The lock of every stylesheet, `style` attribute and presentational hint
/// in the process. Nothing takes it for writing: parsed CSS never changes.
pub(crate) fn lock() -> &'static SharedRwLock {
    static LOCK: LazyLock<SharedRwLock> = LazyLock::new(SharedRwLock::new);
    &LOCK
}

/// A parsed stylesheet. Cheap to clone, and it can go to other threads.
/// Parses of the same sheet that the cache answered are the same
/// `ParsedStylesheet`.
#[derive(Clone)]
pub struct ParsedStylesheet(StdArc<Parsed>);

pub(crate) struct Parsed {
    pub(crate) contents: Arc<StylesheetContents>,
    /// No `@import`, `@scope`, `@custom-media` or anonymous layer, and
    /// layers are declared only at the top or inside other layers. Then an
    /// earlier copy of the sheet in a cascade can be left out; see
    /// `StyleEngine::add_parsed_stylesheets`.
    pub(crate) dedupable: bool,
    /// The cascade layers the sheet declares, by full name (`a`, `a.b`),
    /// in the order Stylo registers them (first declaration).
    pub(crate) layers: Vec<LayerPath>,
    has_imports: bool,
}

/// The full name of a cascade layer: `a.b` is `[a, b]`.
pub(crate) type LayerPath = Box<[AtomIdent]>;

impl std::fmt::Debug for ParsedStylesheet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedStylesheet")
            .field("url", &self.0.contents.url_data.0.as_str())
            .field("dedupable", &self.0.dedupable)
            .field("layers", &self.0.layers.len())
            .finish()
    }
}

impl ParsedStylesheet {
    /// Whether `self` and `other` are one parse (the cache's answer for the
    /// same sheet).
    pub fn same_parse(&self, other: &ParsedStylesheet) -> bool {
        StdArc::ptr_eq(&self.0, &other.0)
    }

    pub(crate) fn parsed(&self) -> &Parsed {
        &self.0
    }

    pub(crate) fn key(&self) -> *const Parsed {
        StdArc::as_ptr(&self.0)
    }

    /// A stylesheet object of its own for one place in a cascade, on the
    /// shared parse (Stylo keeps each stylesheet of a cascade once).
    pub(crate) fn instance(&self) -> Stylesheet {
        let lock = lock();
        Stylesheet {
            contents: lock.wrap(self.0.contents.clone()),
            shared_lock: lock.clone(),
            media: Arc::new(lock.wrap(MediaList::empty())),
            disabled: Default::default(),
        }
    }
}

/// Source bytes the cache keeps at most; the least recently used sheets go
/// first. Pages hold the sheets they use whether or not the cache keeps
/// them, so this bounds only what the cache keeps alive on its own.
const CACHE_BYTES: usize = 8 << 20;

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    origin: u8,
    url: Box<str>,
    len: usize,
    hash: u64,
}

struct Entry {
    /// Filled by the first thread that asks; the others wait for it.
    sheet: StdArc<OnceLock<ParsedStylesheet>>,
    len: usize,
    used: u64,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<Key, Entry>,
    bytes: usize,
    clock: u64,
}

fn cache() -> MutexGuard<'static, Cache> {
    static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Mutex::default);
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

impl Cache {
    /// The slot for `key`, made if new; evicts the least recently used
    /// entries when over [`CACHE_BYTES`].
    fn slot(&mut self, key: &Key) -> StdArc<OnceLock<ParsedStylesheet>> {
        self.clock += 1;
        let now = self.clock;
        if let Some(e) = self.entries.get_mut(key) {
            e.used = now;
            return e.sheet.clone();
        }
        let sheet = StdArc::new(OnceLock::new());
        self.entries.insert(
            key.clone(),
            Entry {
                sheet: sheet.clone(),
                len: key.len,
                used: now,
            },
        );
        self.bytes += key.len;
        while self.bytes > CACHE_BYTES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.remove(&oldest);
        }
        sheet
    }

    fn remove(&mut self, key: &Key) {
        if let Some(e) = self.entries.remove(key) {
            self.bytes -= e.len;
        }
    }
}

/// Parse a stylesheet, on any thread. `base_url` resolves relative `url()`s
/// and `@import`s; `@import`s are fetched synchronously through `loader`.
/// The same origin, URL and text give the same parse while the cache keeps
/// it (several threads asking at once wait for one parse).
pub fn parse_stylesheet(
    css: &str,
    base_url: &str,
    origin: Origin,
    loader: &dyn StylesheetLoader,
) -> ParsedStylesheet {
    let mut h = DefaultHasher::new();
    css.hash(&mut h);
    let key = Key {
        origin: origin as u8,
        url: base_url.into(),
        len: css.len(),
        hash: h.finish(),
    };
    let slot = cache().slot(&key);
    let sheet = slot
        .get_or_init(|| parse_uncached(css, base_url, origin, loader))
        .clone();
    if sheet.0.has_imports {
        let mut c = cache();
        if c.entries
            .get(&key)
            .is_some_and(|e| StdArc::ptr_eq(&e.sheet, &slot))
        {
            c.remove(&key);
        }
    }
    sheet
}

fn parse_uncached(
    css: &str,
    base_url: &str,
    origin: Origin,
    loader: &dyn StylesheetLoader,
) -> ParsedStylesheet {
    let lock = lock();
    let sheet = engine::parse_sheet(css, base_url, origin, lock, loader, QuirksMode::NoQuirks, 0);
    let guard = lock.read();
    let contents = sheet.contents.read_with(&guard).clone();
    let has_imports = contents
        .rules(&guard)
        .iter()
        .any(|r| matches!(r, CssRule::Import(_)));
    let mut layers = Vec::new();
    let dedupable = scan(
        contents.rules.read_with(&guard),
        &guard,
        Some(&[]),
        &mut layers,
    );
    drop(guard);
    ParsedStylesheet(StdArc::new(Parsed {
        contents,
        dedupable,
        layers,
        has_imports,
    }))
}

/// Collect the layers `rules` declare into `out` (first declarations, each
/// after its parents). `layer`: the containing layer's name, or `None`
/// inside a rule where a layer would depend on more than its place (a
/// condition, a style rule). Returns whether the rules are dedupable (see
/// [`Parsed::dedupable`]): an `@import`, `@scope`, `@custom-media`,
/// anonymous layer or conditional layer makes the cascade depend on more
/// than the last place of a sheet (layer order is that of first
/// declaration, a prelude-less `@scope` is scoped to its owner node, a
/// custom media query is defined for the rules after it).
fn scan(
    rules: &CssRules,
    guard: &SharedRwLockReadGuard,
    layer: Option<&[AtomIdent]>,
    out: &mut Vec<LayerPath>,
) -> bool {
    rules.0.iter().all(|rule| match rule {
        CssRule::Import(_) | CssRule::Scope(_) | CssRule::CustomMedia(_) => false,
        CssRule::LayerStatement(r) => layer.is_some_and(|parent| {
            for name in &r.names {
                register(out, parent, name.layer_names());
            }
            true
        }),
        CssRule::LayerBlock(r) => match (layer, &r.name) {
            (Some(parent), Some(name)) => {
                let path = register(out, parent, name.layer_names());
                scan(r.rules.read_with(guard), guard, Some(&path), out)
            }
            _ => false,
        },
        CssRule::Style(r) => r
            .read_with(guard)
            .rules
            .as_ref()
            .is_none_or(|r| scan(r.read_with(guard), guard, None, out)),
        CssRule::Media(r) => scan(r.rules.read_with(guard), guard, None, out),
        CssRule::Supports(r) => scan(r.rules.read_with(guard), guard, None, out),
        CssRule::Container(r) => scan(r.rules.read_with(guard), guard, None, out),
        CssRule::Document(r) => scan(r.rules.read_with(guard), guard, None, out),
        CssRule::StartingStyle(r) => scan(r.rules.read_with(guard), guard, None, out),
        CssRule::AppearanceBase(r) => scan(r.rules.read_with(guard), guard, None, out),
        CssRule::Page(r) => scan(r.read_with(guard).rules.read_with(guard), guard, None, out),
        _ => true,
    })
}

/// Register layer `parent.name` and the layers between (Stylo registers
/// `a` before `a.b`); returns its full name.
fn register(out: &mut Vec<LayerPath>, parent: &[AtomIdent], name: &[AtomIdent]) -> Vec<AtomIdent> {
    let mut path = parent.to_vec();
    for part in name {
        path.push(part.clone());
        if !out.iter().any(|p| **p == *path) {
            out.push(path.clone().into_boxed_slice());
        }
    }
    path
}

/// Which places of `sheets` (one cascade, in order) can be left out: the
/// earlier copies of a dedupable sheet that comes again later. Every rule
/// of such a copy matches again in the last copy, with the same
/// specificity, in the same named layer and later, so it never wins. That
/// holds while the layer order stays the same; layer order is that of
/// first declaration, so copies that declare layers are left out only if
/// the first declarations in `sheets` stay in the same order without them.
pub(crate) fn left_out(sheets: &[ParsedStylesheet]) -> Vec<bool> {
    let last: HashMap<*const Parsed, usize> = sheets
        .iter()
        .enumerate()
        .map(|(i, s)| (s.key(), i))
        .collect();
    let mut out: Vec<bool> = sheets
        .iter()
        .enumerate()
        .map(|(i, s)| s.0.dedupable && last[&s.key()] > i)
        .collect();
    let order = |keep: &dyn Fn(usize) -> bool| {
        let mut seen: Vec<&LayerPath> = Vec::new();
        for (i, s) in sheets.iter().enumerate() {
            if keep(i) {
                for l in &s.0.layers {
                    if !seen.contains(&l) {
                        seen.push(l);
                    }
                }
            }
        }
        seen
    };
    let layered = |i: usize| out[i] && !sheets[i].0.layers.is_empty();
    if (0..sheets.len()).any(layered) && order(&|_| true) != order(&|i| !out[i]) {
        for (i, s) in sheets.iter().enumerate() {
            if !s.0.layers.is_empty() {
                out[i] = false;
            }
        }
    }
    out
}
