//! All windows of `huginn open` and what they share: the visit log
//! (`huginn://history`), bookmarks, per-site zoom, favicons, recently
//! closed tabs, and engine threads (one per tab). `window.rs` maps winit
//! windows onto [`Session::windows`]; `headless.rs` drives it directly.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use huginn_net::{CookieJar, DecodedImage, Url};

use crate::browser::{Browser, WindowOp};
use crate::engine::{self, Command, EngineEvent};
use crate::internal_pages::{self, Bookmark};
use crate::tab::{HistoryEntry, TabId};

/// Engine events from all tabs, tagged with the tab (0: not tied to a tab).
pub type Notify = Arc<dyn Fn(TabId, EngineEvent) + Send + Sync>;

/// Background tabs that keep their page in memory; older ones are discarded
/// (least recently active first) and load again when activated.
const LIVE_BACKGROUND_TABS: usize = 8;

/// Process memory (`phys_footprint`, what Activity Monitor shows) above
/// which background tabs are discarded one per event, least recently used
/// first, whatever `LIVE_BACKGROUND_TABS` allows. `HUGINN_MEMORY_BUDGET_MB` overrides.
const MEMORY_BUDGET_MB: u64 = 1536;

/// Chrome's zoom steps.
pub const ZOOM_LEVELS: [f32; 15] = [
    0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0,
];

pub struct Visit {
    pub url: Url,
    pub title: String,
}

/// A closed tab, for Cmd+Shift+T.
pub struct ClosedTab {
    pub(crate) history: Vec<HistoryEntry>,
    pub(crate) index: usize,
    pub title: String,
}

enum Favicon {
    Loading,
    Done(Option<DecodedImage>),
}

/// Session state that tabs and windows reach through `Rc<RefCell<Shared>>`.
/// Borrows are short and never held across calls into tabs.
pub struct Shared {
    js: bool,
    notify: Notify,
    next_tab: TabId,
    /// Visits of normal (not private) windows, oldest first.
    pub visits: Vec<Visit>,
    pub bookmarks: Vec<Bookmark>,
    /// `None`: bookmarks are not saved (headless without `HUGINN_PROFILE_DIR`).
    bookmarks_path: Option<PathBuf>,
    /// Zoom per host (or per URL for pages without a host), like Chrome.
    zoom: HashMap<String, f32>,
    favicons: HashMap<Url, Favicon>,
    pub closed: Vec<ClosedTab>,
    /// Activation clock for discarding.
    clock: u64,
    /// Cookies of normal windows, saved in the profile (in memory for
    /// headless runs without `HUGINN_PROFILE_DIR`).
    cookies: Arc<CookieJar>,
    /// Cookies of private windows: in memory only, shared by all private
    /// windows and dropped when the last one closes.
    private_cookies: Option<Arc<CookieJar>>,
}

impl Shared {
    pub fn new_tab_id(&mut self) -> TabId {
        self.next_tab += 1;
        self.next_tab
    }

    /// Start the engine thread of a tab; `private`: it uses the private
    /// windows' cookie jar.
    pub(crate) fn spawn_engine(&mut self, tab: TabId, private: bool) -> Sender<Command> {
        let notify = self.notify.clone();
        let cookies = if private {
            self.private_cookies
                .get_or_insert_with(|| Arc::new(CookieJar::in_memory()))
                .clone()
        } else {
            self.cookies.clone()
        };
        engine::spawn(self.js, cookies, Arc::new(move |ev| notify(tab, ev)))
    }

    pub(crate) fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// The favicon at `url` if loaded; starts loading it otherwise.
    pub fn favicon(&mut self, url: &Url) -> Option<DecodedImage> {
        match self.favicons.get(url) {
            Some(Favicon::Done(img)) => img.clone(),
            Some(Favicon::Loading) => None,
            None => {
                self.favicons.insert(url.clone(), Favicon::Loading);
                let (url, notify) = (url.clone(), self.notify.clone());
                // A plain request like any subresource: no cookies, no extra headers.
                std::thread::Builder::new()
                    .name("favicon".into())
                    .spawn(move || {
                        let image = huginn_net::Loader::new()
                            .fetch_uncached(&url, huginn_net::ResourceKind::Image)
                            .ok()
                            .and_then(|r| huginn_net::decode_image(&r.body, Some((32, 32))).ok());
                        notify(0, EngineEvent::Favicon { url, image });
                    })
                    .ok();
                None
            }
        }
    }

    pub(crate) fn visit(&mut self, url: &Url, title: &str) {
        if url.scheme() == "huginn" || url.scheme() == "view-source" {
            return;
        }
        if let Some(last) = self.visits.last_mut()
            && last.url == *url
        {
            title.clone_into(&mut last.title);
            return;
        }
        self.visits.push(Visit {
            url: url.clone(),
            title: title.to_owned(),
        });
    }

    /// Clear browsing data: history, recently closed tabs and cookies (of
    /// normal and private windows).
    pub fn clear_history(&mut self) {
        self.visits.clear();
        self.closed.clear();
        self.cookies.clear();
        if let Err(e) = self.cookies.save() {
            eprintln!("huginn: cannot save cookies: {e}");
        }
        if let Some(c) = &self.private_cookies {
            c.clear();
        }
    }

    pub fn is_bookmarked(&self, url: &Url) -> bool {
        self.bookmarks.iter().any(|b| b.url == *url)
    }

    /// Add a bookmark, or remove it if the page is bookmarked. Returns whether it is now bookmarked.
    pub fn toggle_bookmark(&mut self, url: &Url, title: &str) -> bool {
        let on = if let Some(i) = self.bookmarks.iter().position(|b| b.url == *url) {
            self.bookmarks.remove(i);
            false
        } else {
            self.bookmarks.push(Bookmark {
                url: url.clone(),
                title: title.to_owned(),
            });
            true
        };
        if let Some(p) = &self.bookmarks_path
            && let Err(e) = internal_pages::save_bookmarks(p, &self.bookmarks)
        {
            eprintln!("huginn: cannot save bookmarks to {}: {e}", p.display());
        }
        on
    }

    fn zoom_key(url: &Url) -> String {
        url.host_str()
            .map_or_else(|| url.as_str().to_owned(), str::to_owned)
    }

    pub fn zoom_for(&self, url: &Url) -> f32 {
        self.zoom.get(&Self::zoom_key(url)).copied().unwrap_or(1.0)
    }

    pub fn set_zoom(&mut self, url: &Url, zoom: f32) {
        let key = Self::zoom_key(url);
        if zoom == 1.0 {
            self.zoom.remove(&key);
        } else {
            self.zoom.insert(key, zoom);
        }
    }
}

pub struct Session {
    pub windows: Vec<Browser>,
    /// The window that gets keys (the frontmost).
    pub focused: usize,
    pub shared: Rc<RefCell<Shared>>,
    next_window: u64,
    /// Size of new windows: CSS px, and device px per CSS px.
    size: (f32, f32),
    scale: f32,
    pub quit: bool,
    live_background: usize,
    memory_budget: u64,
}

impl Session {
    /// `headless`: bookmarks and cookies are saved only with
    /// `HUGINN_PROFILE_DIR` set.
    pub fn new(js: bool, notify: Notify, size: (f32, f32), scale: f32, headless: bool) -> Session {
        let profile = if headless && std::env::var_os("HUGINN_PROFILE_DIR").is_none() {
            None
        } else {
            internal_pages::profile_dir()
        };
        let bookmarks_path = profile.as_ref().map(|d| d.join("bookmarks.tsv"));
        let bookmarks = bookmarks_path
            .as_deref()
            .map(internal_pages::load_bookmarks)
            .unwrap_or_default();
        let cookies = Arc::new(match &profile {
            Some(d) => CookieJar::persistent(d.join("cookies.tsv")),
            None => CookieJar::in_memory(),
        });
        let live_background = std::env::var("HUGINN_LIVE_TABS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(LIVE_BACKGROUND_TABS);
        let memory_budget = std::env::var("HUGINN_MEMORY_BUDGET_MB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(MEMORY_BUDGET_MB)
            << 20;
        Session {
            windows: Vec::new(),
            focused: 0,
            shared: Rc::new(RefCell::new(Shared {
                js,
                notify,
                next_tab: 0,
                visits: Vec::new(),
                bookmarks,
                bookmarks_path,
                zoom: HashMap::new(),
                favicons: HashMap::new(),
                closed: Vec::new(),
                clock: 0,
                cookies,
                private_cookies: None,
            })),
            next_window: 0,
            size,
            scale,
            quit: false,
            live_background,
            memory_budget,
        }
    }

    /// Device px per CSS px for new windows.
    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
    }

    /// Open a window with one tab on `url` (or a new tab page). Returns its index.
    pub fn open_window(&mut self, url: Option<Url>, private: bool) -> usize {
        self.next_window += 1;
        let (w, h) = (
            (self.size.0 * self.scale).round() as u32,
            (self.size.1 * self.scale).round() as u32,
        );
        let mut b = Browser::new(
            self.next_window,
            self.shared.clone(),
            private,
            w,
            h,
            self.scale,
        );
        b.open_tab(url, false, None);
        self.windows.push(b);
        self.focused = self.windows.len() - 1;
        self.focused
    }

    pub fn window_mut(&mut self, key: u64) -> Option<&mut Browser> {
        self.windows.iter_mut().find(|w| w.id == key)
    }

    pub fn focus_window(&mut self, key: u64) {
        if let Some(i) = self.windows.iter().position(|w| w.id == key) {
            self.focused = i;
        }
    }

    /// Route an engine event to its tab (or to every tab, for favicons).
    pub fn handle(&mut self, tab: TabId, ev: EngineEvent) {
        if let EngineEvent::Favicon { url, image } = ev {
            self.shared
                .borrow_mut()
                .favicons
                .insert(url.clone(), Favicon::Done(image.clone()));
            for w in &mut self.windows {
                w.set_favicon(&url, image.as_ref());
            }
            return;
        }
        for w in &mut self.windows {
            if w.has_tab(tab) {
                w.handle(tab, ev);
                break;
            }
        }
        self.settle();
    }

    /// After any input: carry out window requests (new window, close, quit)
    /// and discard background tabs beyond the budget.
    pub fn settle(&mut self) {
        let mut zoom = false;
        let mut i = 0;
        while i < self.windows.len() {
            let ops = self.windows[i].take_session_ops();
            let mut closed = false;
            for op in ops {
                match op {
                    WindowOp::NewWindow { url, private } => {
                        self.open_window(url, private);
                    }
                    WindowOp::Close => closed = true,
                    WindowOp::Quit => self.quit = true,
                    WindowOp::ZoomChanged => zoom = true,
                    _ => {}
                }
            }
            if closed || self.windows[i].tab_count() == 0 {
                self.windows.remove(i);
                if self.focused >= i && self.focused > 0 {
                    self.focused -= 1;
                }
                continue;
            }
            i += 1;
        }
        self.focused = self.focused.min(self.windows.len().saturating_sub(1));
        if self.windows.is_empty() {
            self.quit = true;
        }
        // The last private window closed: its cookies go (engine threads of
        // its tabs drop their reference as they end).
        if !self.windows.iter().any(|w| w.private) {
            self.shared.borrow_mut().private_cookies = None;
        }
        if zoom {
            for w in &mut self.windows {
                w.apply_zoom();
            }
        }
        self.discard_old_tabs();
    }

    fn discard_old_tabs(&mut self) {
        let mut live: Vec<(u64, usize, usize)> = Vec::new();
        for (wi, w) in self.windows.iter().enumerate() {
            for (ti, t) in w.tabs().iter().enumerate() {
                if !t.is_visible() && !t.discarded && !t.is_loading() && t.url().is_some() {
                    live.push((t.last_active, wi, ti));
                }
            }
        }
        if live.is_empty() {
            return;
        }
        let over = live.len().saturating_sub(self.live_background);
        let n = if over == 0 && footprint().is_some_and(|f| f > self.memory_budget) {
            1
        } else {
            over
        };
        if n == 0 {
            return;
        }
        live.sort_unstable();
        for &(_, wi, ti) in &live[..n] {
            self.windows[wi].tab_mut(ti).discard();
        }
    }

    /// Some engine still has commands to answer.
    pub fn busy(&self) -> bool {
        self.windows.iter().any(Browser::busy)
    }

    pub fn state(&self) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        if let Some(w) = self.windows.get(self.focused) {
            w.state(&mut out);
        }
        let shared = self.shared.borrow();
        out.insert("windows".into(), self.windows.len().into());
        out.insert("window".into(), self.focused.into());
        out.insert("visits".into(), shared.visits.len().into());
        out.insert("closed".into(), shared.closed.len().into());
        out.insert("bookmarks".into(), shared.bookmarks.len().into());
        out.insert("clipboard".into(), crate::clipboard::memory().into());
        let engines: usize = self
            .windows
            .iter()
            .map(|w| w.tabs().iter().filter(|t| t.has_engine()).count())
            .sum();
        out.insert("engines".into(), engines.into());
        out.insert("cookies".into(), shared.cookies.len().into());
        out.insert(
            "private_cookies".into(),
            shared
                .private_cookies
                .as_ref()
                .map_or(serde_json::Value::Null, |c| c.len().into()),
        );
        serde_json::Value::Object(out)
    }
}

impl Drop for Session {
    /// Write the cookies changed since the last periodic save.
    fn drop(&mut self) {
        if let Err(e) = self.shared.borrow().cookies.save() {
            eprintln!("huginn: cannot save cookies: {e}");
        }
    }
}

/// This process's `phys_footprint` in bytes (`proc_pid_rusage`, macOS).
#[cfg(target_os = "macos")]
fn footprint() -> Option<u64> {
    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
    }
    const RUSAGE_INFO_V4: i32 = 4;
    // rusage_info_v4: a 16-byte UUID, then u64 fields; phys_footprint is the
    // 8th. The buffer is larger than the struct.
    let mut buf = [0u64; 64];
    // SAFETY: the buffer outlives the call and is bigger than rusage_info_v4.
    let r = unsafe { proc_pid_rusage(std::process::id() as i32, RUSAGE_INFO_V4, buf.as_mut_ptr()) };
    (r == 0).then_some(buf[2 + 7])
}

/// This process's private memory in bytes: anonymous resident pages plus
/// swap (`RssAnon` and `VmSwap` in `/proc/self/status`), the closest Linux
/// has to macOS's `phys_footprint`.
#[cfg(target_os = "linux")]
fn footprint() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb = |field: &str| -> Option<u64> {
        let line = status.lines().find(|l| l.starts_with(field))?;
        line[field.len()..]
            .trim()
            .strip_suffix("kB")?
            .trim()
            .parse()
            .ok()
    };
    Some((kb("RssAnon:")? + kb("VmSwap:").unwrap_or(0)) * 1024)
}

/// Unknown on this platform: tabs are discarded by count only.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn footprint() -> Option<u64> {
    None
}
