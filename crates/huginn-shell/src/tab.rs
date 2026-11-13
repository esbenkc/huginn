//! One tab: its engine thread, history, the page on screen (a [`LayoutTree`]
//! rasterized in strips), scroll, zoom and input routing to the in-page
//! modules (`interact`, `live`).
//!
//! The page is a layer of full-width raster strips. A strip is rasterized
//! from the current `LayoutTree` the first time it scrolls into view and then
//! copied (blitted) into each frame, so scrolling never re-runs layout or
//! paint. Strips are kept in a small LRU (the visible ones plus one above and
//! one below), so a long page at 2x does not hold hundreds of MB of pixels.
//! A background tab keeps no strips. A discarded tab keeps only its history:
//! its engine thread (and with it the DOM, styles and script isolate) is gone,
//! and it loads again when it becomes active.
//!
//! Coordinates: input events and overlays use window CSS px. The page starts
//! at [`PAGE_TOP`]. Document CSS px are window px minus the page origin,
//! divided by `zoom`, plus `scroll` on y ([`Tab::to_doc`]).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::time::Instant;

use huginn_dom::NodeId;
use huginn_layout::{LayoutTree, Rect};
use huginn_net::{Client, DecodedImage, ReferrerPolicy, Url};
use huginn_paint::{CpuRasterizer, Rasterizer};
use huginn_style::Viewport;

use crate::engine::{Command, EngineEvent, Job, Links, Loaded, PostData};
use crate::input::{CursorKind, Key, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseKind};
use crate::session::Shared;
use crate::ui::{Canvas, Pixels, copy_row};
use crate::{interact, live};

/// Height of the tab strip, then the toolbar, in window CSS px.
pub const TABSTRIP_HEIGHT: f32 = 40.0;
pub const TOOLBAR_HEIGHT: f32 = 40.0;
/// Top of the page area in window CSS px.
pub const PAGE_TOP: f32 = TABSTRIP_HEIGHT + TOOLBAR_HEIGHT;
/// Arrow keys and one wheel "line", in CSS px.
pub const LINE_SCROLL: f32 = 40.0;
/// Height of one raster strip of the page layer, in document CSS px.
const STRIP_HEIGHT: f32 = 256.0;
const WHITE: u32 = 0x00FF_FFFF;

pub type TabId = u64;

/// What in-page code asks the window to do. Drained after each event.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// Load `url` in this tab.
    Navigate(Url),
    /// Open `url` in a new tab next to this one.
    OpenTab {
        url: Url,
        background: bool,
        opener: Box<Opener>,
    },
    OpenWindow {
        url: Url,
        private: bool,
        opener: Box<Opener>,
    },
    Back,
    Forward,
    Reload,
    /// Open `view-source:<url>` of this tab in a new tab.
    ViewSource,
}

/// Where a new tab or window's first load comes from: the page that opened
/// it (`Referer`, `SameSite`, `Sec-Fetch-Site`) and a form body. The
/// default is the user (the address bar, a menu of the browser itself).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Opener {
    pub initiator: Option<Client>,
    pub post: Option<PostData>,
}

/// The page on screen.
pub struct View {
    pub tree: LayoutTree,
    pub links: Links,
}

#[derive(Clone)]
pub(crate) struct HistoryEntry {
    pub url: Url,
    /// Scroll position when we left the entry, restored on back/forward.
    pub scroll: f32,
    pub post: Option<PostData>,
    /// The page that opened this entry (`Referer`, `SameSite`); `None`: the user.
    pub initiator: Option<Client>,
}

/// The page layer: strips of `rows` device rows, rasterized on demand.
struct Strips {
    rows: usize,
    /// Strip index -> pixels and last use.
    strips: Vec<Option<(Pixels, u64)>>,
    clock: u64,
    budget: usize,
}

impl Strips {
    fn new(scale: f32, page_rows: usize) -> Strips {
        let rows = ((STRIP_HEIGHT * scale).round() as usize).max(1);
        Strips {
            rows,
            strips: Vec::new(),
            clock: 0,
            // Visible strips (a viewport can touch one more than it covers)
            // plus one prefetched above and one below.
            budget: page_rows.div_ceil(rows) + 3,
        }
    }

    fn clear(&mut self) {
        self.strips = Vec::new();
    }

    fn count(&self) -> usize {
        self.strips.iter().flatten().count()
    }

    fn has(&self, k: usize) -> bool {
        self.strips.get(k).is_some_and(Option::is_some)
    }

    /// Get strip `k`, rasterizing it from `tree` if needed.
    fn get(
        &mut self,
        k: usize,
        tree: &LayoutTree,
        raster: &mut CpuRasterizer,
        width: usize,
        scale: f32,
    ) -> &Pixels {
        self.clock += 1;
        if !self.has(k) {
            let region = Rect {
                x: 0.0,
                y: (k * self.rows) as f32 / scale,
                width: width as f32 / scale,
                height: self.rows as f32 / scale,
            };
            let _span = crate::perf::span("raster");
            let list = huginn_paint::build_display_list(tree, Some(region));
            let px = Pixels::from_rgba(raster.rasterize(&list, region, scale));
            while self.count() >= self.budget {
                let lru = self
                    .strips
                    .iter()
                    .enumerate()
                    .filter_map(|(i, s)| s.as_ref().map(|(_, used)| (i, *used)))
                    .min_by_key(|&(_, used)| used)
                    .map(|(i, _)| i)
                    .expect("budget is at least 1");
                self.strips[lru] = None;
            }
            if self.strips.len() <= k {
                self.strips.resize_with(k + 1, || None);
            }
            self.strips[k] = Some((px, 0));
        }
        let entry = self.strips[k].as_mut().expect("just inserted");
        entry.1 = self.clock;
        &entry.0
    }
}

pub struct Tab {
    pub id: TabId,
    shared: Rc<RefCell<Shared>>,
    /// Private windows record no visits.
    pub private: bool,
    /// `None` when discarded (or never started).
    engine: Option<Sender<Command>>,
    /// Last command sent and last command the engine finished.
    seq: u64,
    done_seq: u64,
    /// Navigation generation; engine answers for older ones are dropped.
    pub generation: u64,
    loading: Option<Instant>,
    pub(crate) history: Vec<HistoryEntry>,
    pub(crate) index: usize,
    pub view: Option<View>,
    pub title: String,
    /// Why the current page failed to load (an error page is shown).
    pub error: Option<String>,
    /// Load time of the current page (fetch to layout), ms.
    pub load_ms: f64,
    /// The current document's referrer policy, for the links it opens.
    referrer_policy: ReferrerPolicy,
    /// The page's icon URL and image, once loaded.
    pub icon: Option<Url>,
    pub favicon: Option<DecodedImage>,
    /// Top of the page area in document CSS px.
    pub scroll: f32,
    /// Scroll position to apply when the loading page arrives.
    restore: Option<f32>,
    /// Page area size in device px and device px per window CSS px.
    width: u32,
    height: u32,
    scale: f32,
    /// Page zoom: 1.0 = 100 %. Layout runs at `css width / zoom`.
    pub zoom: f32,
    strips: Strips,
    /// The page on screen is the active one in its window.
    visible: bool,
    /// The engine's viewport is out of date (resized or zoomed while in the background).
    needs_layout: bool,
    /// A load was stopped: the engine holds a page the view does not show.
    stale: bool,
    /// Discarded to save memory; loads again on activation.
    pub discarded: bool,
    /// Activation clock value, for discarding the least recently used tabs.
    pub(crate) last_active: u64,
    pub hover: Option<Url>,
    /// Pointer shape over the page set by in-page code (an I-beam over
    /// text); `None`: a hand over links, else the arrow.
    pub cursor: Option<CursorKind>,
    /// Link and button of the current press in the page.
    pressed: Option<(Url, MouseButton)>,
    requests: Vec<Request>,
    dirty: bool,
    /// `interact`'s per-tab state (selection, find, focus, menu).
    pub interact: interact::PageState,
    /// `live`'s per-tab state (focused control, caret, popups).
    pub live: live::TabState,
}

impl Tab {
    /// A tab with no page. `width`/`height`: page area in device px.
    pub(crate) fn new(
        id: TabId,
        shared: Rc<RefCell<Shared>>,
        private: bool,
        width: u32,
        height: u32,
        scale: f32,
    ) -> Tab {
        let (width, height) = (width.max(1), height.max(1));
        Tab {
            id,
            shared,
            private,
            engine: None,
            seq: 0,
            done_seq: 0,
            generation: 0,
            loading: None,
            history: Vec::new(),
            index: 0,
            view: None,
            title: String::new(),
            error: None,
            load_ms: 0.0,
            referrer_policy: ReferrerPolicy::default(),
            icon: None,
            favicon: None,
            scroll: 0.0,
            restore: None,
            width,
            height,
            scale,
            zoom: 1.0,
            strips: Strips::new(scale, height as usize),
            visible: true,
            needs_layout: false,
            stale: false,
            discarded: false,
            last_active: 0,
            hover: None,
            cursor: None,
            pressed: None,
            requests: Vec::new(),
            dirty: true,
            interact: interact::PageState::default(),
            live: live::TabState::default(),
        }
    }

    /// A reopened (closed) tab or a tab restored with its history; loads on activation.
    pub(crate) fn restore_history(
        &mut self,
        history: Vec<HistoryEntry>,
        index: usize,
        title: String,
    ) {
        self.index = index.min(history.len().saturating_sub(1));
        self.history = history;
        self.title = title;
        self.discarded = !self.history.is_empty();
    }

    // ----- state -------------------------------------------------------------

    pub fn url(&self) -> Option<&Url> {
        self.history.get(self.index).map(|e| &e.url)
    }

    pub fn can_back(&self) -> bool {
        self.index > 0
    }

    pub fn can_forward(&self) -> bool {
        self.index + 1 < self.history.len()
    }

    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    pub fn loading_since(&self) -> Option<Instant> {
        self.loading
    }

    /// The engine still has commands to answer.
    pub fn busy(&self) -> bool {
        self.engine.is_some() && self.done_seq < self.seq
    }

    /// Title for the tab strip and window: the page title, else the URL.
    pub fn display_title(&self) -> String {
        if !self.title.is_empty() {
            self.title.clone()
        } else if let Some(u) = self.url().filter(|u| !crate::internal_pages::is_new_tab(u)) {
            u.to_string()
        } else if self.is_loading() {
            "Loading…".into()
        } else {
            "New Tab".into()
        }
    }

    pub(crate) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Ask for a new frame (overlays changed).
    pub fn redraw(&mut self) {
        self.dirty = true;
    }

    /// Something in-page code wants from the window (new tab, back, ...).
    pub fn request(&mut self, r: Request) {
        self.requests.push(r);
    }

    pub(crate) fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }

    /// The tab has an engine thread (it is not discarded).
    pub fn has_engine(&self) -> bool {
        self.engine.is_some()
    }

    /// Device px per document CSS px.
    pub fn page_scale(&self) -> f32 {
        self.scale * self.zoom
    }

    /// Page area size in document CSS px.
    pub fn page_size(&self) -> (f32, f32) {
        (
            self.width as f32 / self.page_scale(),
            self.height as f32 / self.page_scale(),
        )
    }

    /// Window CSS point -> document CSS point.
    pub fn to_doc(&self, x: f32, y: f32) -> (f32, f32) {
        (x / self.zoom, (y - PAGE_TOP) / self.zoom + self.scroll)
    }

    /// Document CSS rect -> window CSS rect.
    pub fn doc_to_window(&self, r: Rect) -> Rect {
        Rect {
            x: r.x * self.zoom,
            y: (r.y - self.scroll) * self.zoom + PAGE_TOP,
            width: r.width * self.zoom,
            height: r.height * self.zoom,
        }
    }

    pub fn max_scroll(&self) -> f32 {
        self.view.as_ref().map_or(0.0, |v| {
            (v.tree.document_size.height - self.page_size().1).max(0.0)
        })
    }

    fn viewport(&self) -> Viewport {
        let (w, h) = self.page_size();
        Viewport {
            width: w,
            height: h.max(1.0),
            device_pixel_ratio: self.page_scale(),
        }
    }

    // ----- engine ------------------------------------------------------------

    fn send(&mut self, make: impl FnOnce(u64) -> Command) {
        if self.engine.is_none() {
            let engine = self.shared.borrow_mut().spawn_engine(self.id, self.private);
            if !self.visible {
                let _ = engine.send(Command::SetVisible {
                    seq: 0,
                    visible: false,
                });
            }
            self.engine = Some(engine);
        }
        self.seq += 1;
        if let Some(e) = &self.engine {
            // Fails only if the engine thread panicked.
            let _ = e.send(make(self.seq));
        }
    }

    /// Run `f` on the engine thread on this tab's page; its event comes back
    /// to `interact::on_engine` (wrap it in `EngineEvent::Interact`).
    pub fn run(&mut self, f: Job) {
        if self.view.is_some() && !self.stale {
            let generation = self.generation;
            self.send(|seq| Command::Run { seq, generation, f });
        }
    }

    /// Send a DOM input event to the live page.
    pub fn input(&mut self, event: live::PageInput) {
        if self.view.is_some() && !self.stale {
            let generation = self.generation;
            self.send(|seq| Command::Input {
                seq,
                generation,
                event,
            });
        }
    }

    pub(crate) fn handle(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::Idle { seq } => self.done_seq = self.done_seq.max(seq),
            EngineEvent::Loaded(l) if l.generation == self.generation => self.on_loaded(*l),
            EngineEvent::Relaid {
                generation,
                tree,
                links,
                title,
            } if generation == self.generation && !self.stale => {
                if let Some(v) = &mut self.view {
                    v.tree = tree;
                    if let Some(l) = links {
                        v.links = l;
                    }
                    if let Some(t) = title {
                        self.title = t;
                    }
                    self.strips.clear();
                    self.hover = None;
                    self.set_scroll(self.scroll);
                    self.dirty = true;
                    interact::on_relayout(self);
                }
            }
            EngineEvent::Fragment {
                generation,
                targets,
            } if generation == self.generation => {
                if let Some(y) = self.target_y(&targets) {
                    self.set_scroll(y);
                }
            }
            EngineEvent::Navigate {
                generation,
                url,
                post,
                mods,
            } if generation == self.generation => {
                self.open_link(url, post, MouseButton::Left, mods);
            }
            EngineEvent::Interact(any) => interact::on_engine(self, any),
            EngineEvent::Live(ev) => live::on_engine(self, ev),
            _ => {}
        }
    }

    fn on_loaded(&mut self, l: Loaded) {
        let entry = &mut self.history[self.index];
        let mut url = l.url;
        if url.fragment().is_none() && entry.url.fragment().is_some() {
            url.set_fragment(entry.url.fragment());
        }
        entry.url = url;
        self.title = l.title;
        self.error = l.error;
        self.load_ms = l.load_ms;
        self.referrer_policy = l.referrer_policy;
        self.loading = None;
        self.stale = false;
        if self.icon != l.icon {
            self.favicon = None;
            self.icon = l.icon;
        }
        if let Some(icon) = &self.icon {
            self.favicon = self.shared.borrow_mut().favicon(icon);
        }
        self.view = Some(View {
            tree: l.tree,
            links: l.links,
        });
        self.strips.clear();
        self.hover = None;
        self.pressed = None;
        let y = self
            .restore
            .take()
            .or_else(|| self.target_y(&l.fragment))
            .unwrap_or(0.0);
        self.scroll = 0.0;
        self.set_scroll(y);
        self.dirty = true;
        if !self.private && self.error.is_none() {
            let url = self.history[self.index].url.clone();
            self.shared.borrow_mut().visit(&url, &self.title);
        }
        interact::on_load(self);
        live::on_load(self);
    }

    /// Document y of the first target that has a box.
    fn target_y(&self, targets: &[NodeId]) -> Option<f32> {
        let tree = &self.view.as_ref()?.tree;
        targets
            .iter()
            .find_map(|&n| tree.box_for_node(n))
            .map(|b| tree.get(b).rect.y)
    }

    // ----- navigation --------------------------------------------------------

    /// Open `url` as a new history entry, as the user (address bar, bookmark).
    pub fn navigate(&mut self, url: Url) {
        self.navigate_with(url, None, None);
    }

    pub(crate) fn navigate_with(
        &mut self,
        url: Url,
        post: Option<PostData>,
        initiator: Option<Client>,
    ) {
        if let Some(cur) = self.history.get_mut(self.index) {
            cur.scroll = self.scroll;
        }
        let same_doc = post.is_none()
            && self.loading.is_none()
            && self.view.is_some()
            && !self.stale
            && url.fragment().is_some()
            && self.url().is_some_and(|c| same_document(c, &url));
        self.history.truncate(self.index + 1);
        self.history.push(HistoryEntry {
            url,
            scroll: 0.0,
            post,
            initiator,
        });
        self.index = self.history.len() - 1;
        if same_doc {
            let (generation, id) = (
                self.generation,
                self.history[self.index]
                    .url
                    .fragment()
                    .unwrap_or("")
                    .to_owned(),
            );
            self.send(|seq| Command::Fragment {
                seq,
                generation,
                id,
            });
            self.dirty = true;
        } else {
            self.start_load(None);
        }
    }

    pub fn back(&mut self) {
        self.go(-1);
    }

    pub fn forward(&mut self) {
        self.go(1);
    }

    pub fn reload(&mut self) {
        if self.history.is_empty() {
            return;
        }
        self.history[self.index].scroll = self.scroll;
        self.start_load(Some(self.scroll));
    }

    /// Stop the load in progress: keep showing the previous page.
    pub fn stop(&mut self) -> bool {
        if self.loading.is_none() {
            return false;
        }
        self.loading = None;
        // Drop the engine's answer; its page no longer matches the view.
        self.generation += 1;
        if self.view.is_some() {
            self.stale = true;
            // The entry that was loading goes away, unless it was a reload or back/forward.
            if self.restore.is_none() && self.index > 0 {
                self.history.truncate(self.index);
                self.index -= 1;
            }
        }
        self.restore = None;
        self.dirty = true;
        true
    }

    fn go(&mut self, delta: isize) {
        let Some(i) = self
            .index
            .checked_add_signed(delta)
            .filter(|&i| i < self.history.len())
        else {
            return;
        };
        self.history[self.index].scroll = self.scroll;
        let same_doc = self.loading.is_none()
            && self.view.is_some()
            && !self.stale
            && same_document(&self.history[self.index].url, &self.history[i].url);
        self.index = i;
        let scroll = self.history[i].scroll;
        if same_doc {
            self.set_scroll(scroll);
            self.dirty = true;
        } else {
            self.start_load(Some(scroll));
        }
    }

    pub(crate) fn start_load(&mut self, restore: Option<f32>) {
        self.generation += 1;
        self.loading = Some(Instant::now());
        self.restore = restore;
        self.discarded = false;
        self.needs_layout = false;
        self.zoom = self.shared.borrow().zoom_for(&self.history[self.index].url);
        let entry = &self.history[self.index];
        let (generation, url, post, initiator, viewport) = (
            self.generation,
            entry.url.clone(),
            entry.post.clone(),
            entry.initiator.clone(),
            self.viewport(),
        );
        let html = crate::internal_pages::html(&url, &self.shared.borrow(), self.private);
        self.send(|seq| Command::Load {
            seq,
            generation,
            url,
            viewport,
            post,
            html,
            initiator,
        });
        self.dirty = true;
    }

    /// Open a link the way a click with `button` and `mods` does in Chrome:
    /// Cmd or middle button: background tab (with Shift: foreground tab);
    /// Shift: new window; else this tab, with this page as the initiator.
    pub(crate) fn open_link(
        &mut self,
        url: Url,
        post: Option<PostData>,
        button: MouseButton,
        mods: Modifiers,
    ) {
        let initiator = self.initiator();
        if mods.cmd || button == MouseButton::Middle {
            self.request(Request::OpenTab {
                url,
                background: !mods.shift,
                opener: Box::new(Opener { initiator, post }),
            });
        } else if mods.shift {
            self.request(Request::OpenWindow {
                url,
                private: self.private,
                opener: Box::new(Opener { initiator, post }),
            });
        } else {
            self.navigate_with(url, post, initiator);
        }
    }

    /// The current page as the initiator of the navigations it starts.
    /// `None` for error pages and the browser's own `huginn:` pages: their
    /// links count as the user's.
    pub(crate) fn initiator(&self) -> Option<Client> {
        self.url()
            .filter(|u| self.error.is_none() && !matches!(u.scheme(), "huginn" | "view-source"))
            .map(|u| Client {
                url: u.clone(),
                referrer_policy: self.referrer_policy,
            })
    }

    // ----- lifecycle ---------------------------------------------------------

    /// Became the active tab of its window, or stopped being it.
    pub(crate) fn set_visible(&mut self, visible: bool) {
        if visible == self.visible {
            return;
        }
        self.visible = visible;
        self.hover = None;
        self.pressed = None;
        if !visible {
            // Background tabs keep their layout but no pixels.
            self.strips.clear();
        }
        if self.engine.is_some() {
            self.send(|seq| Command::SetVisible { seq, visible });
        }
        if visible {
            if self.discarded {
                let scroll = self.history[self.index].scroll;
                self.start_load(Some(scroll));
            } else if self.needs_layout {
                self.relayout();
            }
        }
        self.dirty = true;
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Free the page: engine thread, DOM, styles, layout, isolate. History,
    /// title and icon stay; the page loads again on activation.
    pub(crate) fn discard(&mut self) {
        if self.discarded || self.visible || self.loading.is_some() || self.history.is_empty() {
            return;
        }
        self.history[self.index].scroll = self.scroll;
        self.engine = None;
        self.done_seq = self.seq;
        self.view = None;
        self.strips.clear();
        self.stale = false;
        self.discarded = true;
        self.interact = interact::PageState::default();
        self.live = live::TabState::default();
    }

    /// The page area changed size (device px) or the window its scale.
    pub(crate) fn resize(&mut self, width: u32, height: u32, scale: f32) {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height, scale) == (self.width, self.height, self.scale) {
            return;
        }
        let keep_strips = width == self.width && scale == self.scale;
        self.width = width;
        self.height = height;
        self.scale = scale;
        let mut strips = Strips::new(self.page_scale(), height as usize);
        if keep_strips {
            // Same width and scale: the old strips stay valid until the new layout arrives.
            strips.strips = std::mem::take(&mut self.strips.strips);
        }
        self.strips = strips;
        self.relayout();
        self.set_scroll(self.scroll);
        self.dirty = true;
    }

    /// Set the page zoom (1.0 = 100 %); the page lays out again at the zoomed width.
    pub fn set_zoom(&mut self, zoom: f32) {
        if zoom == self.zoom {
            return;
        }
        self.zoom = zoom;
        self.strips = Strips::new(self.page_scale(), self.height as usize);
        self.relayout();
        self.set_scroll(self.scroll);
        self.dirty = true;
    }

    /// Bring the engine's layout up to date with the viewport (now, or on activation).
    fn relayout(&mut self) {
        if self.history.is_empty() || self.discarded {
            return;
        }
        if !self.visible {
            self.needs_layout = true;
            return;
        }
        self.needs_layout = false;
        if self.stale {
            // The engine holds another page: load this one again.
            self.start_load(Some(self.scroll));
        } else {
            let (generation, viewport) = (self.generation, self.viewport());
            self.send(|seq| Command::Resize {
                seq,
                generation,
                viewport,
            });
        }
    }

    // ----- scrolling and input -----------------------------------------------

    pub fn scroll_by(&mut self, dy: f32) {
        self.set_scroll(self.scroll + dy);
    }

    pub fn set_scroll(&mut self, y: f32) {
        let y = y.clamp(0.0, self.max_scroll());
        if y != self.scroll {
            self.scroll = y;
            self.dirty = true;
        }
    }

    /// A key the window did not take. Order: the find bar or menu
    /// (`interact::key_capture`), the page's scripts and form controls
    /// (`live::key`), selection and focus (`interact::key`), then scrolling.
    pub(crate) fn key(&mut self, ev: &KeyEvent) -> bool {
        interact::key_capture(self, ev)
            || live::key(self, ev)
            || interact::key(self, ev)
            || self.default_key(ev)
    }

    fn default_key(&mut self, ev: &KeyEvent) -> bool {
        let page = self.page_size().1 * 0.875;
        let m = ev.mods;
        match &ev.key {
            Key::Space if m.shift && !m.cmd => self.scroll_by(-page),
            Key::Space | Key::PageDown if !m.cmd => self.scroll_by(page),
            Key::PageUp if !m.cmd => self.scroll_by(-page),
            Key::Down if m.cmd => self.set_scroll(f32::MAX),
            Key::Up if m.cmd => self.set_scroll(0.0),
            Key::Down if m.none() => self.scroll_by(LINE_SCROLL),
            Key::Up if m.none() => self.scroll_by(-LINE_SCROLL),
            Key::Home => self.set_scroll(0.0),
            Key::End => self.set_scroll(f32::MAX),
            Key::Left if m.cmd && !m.alt => self.back(),
            Key::Right if m.cmd && !m.alt => self.forward(),
            Key::Escape => return self.stop(),
            _ => return false,
        }
        true
    }

    /// A pointer event in the page area. Order as for keys; the default
    /// opens links (press and release on the same link) and tracks hover.
    pub(crate) fn mouse(&mut self, ev: &MouseEvent) -> bool {
        let handled =
            interact::mouse_capture(self, ev) || live::mouse(self, ev) || interact::mouse(self, ev);
        if ev.kind == MouseKind::Move {
            let link = self.link_at(ev.x, ev.y).cloned();
            if link != self.hover {
                self.hover = link;
                self.dirty = true;
            }
        }
        if handled {
            if ev.kind == MouseKind::Down {
                self.pressed = None;
            }
            return true;
        }
        match ev.kind {
            MouseKind::Down if ev.button != MouseButton::Right => {
                self.pressed = self.link_at(ev.x, ev.y).cloned().map(|u| (u, ev.button));
            }
            MouseKind::Up => {
                if let Some((url, button)) = self.pressed.take()
                    && button == ev.button
                    && self.link_at(ev.x, ev.y) == Some(&url)
                {
                    self.open_link(url, None, button, ev.mods);
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    /// Pointer left the page area.
    pub(crate) fn mouse_leave(&mut self) {
        if self.hover.take().is_some() {
            self.dirty = true;
        }
        live::mouse_leave(self);
    }

    /// The link under window CSS point (`x`, `y`).
    pub fn link_at(&self, x: f32, y: f32) -> Option<&Url> {
        if y < PAGE_TOP {
            return None;
        }
        let view = self.view.as_ref()?;
        let (dx, dy) = self.to_doc(x, y);
        let node = interact::hit::node_at(&view.tree, dx, dy)?;
        view.links.get(node)
    }

    // ----- painting ----------------------------------------------------------

    /// Rasterize one strip just outside the viewport if it is missing.
    /// Returns true if it did work (call again when idle).
    pub(crate) fn prefetch(&mut self, raster: &mut CpuRasterizer) -> bool {
        let Some(view) = &self.view else { return false };
        if self.dirty || !self.visible {
            return false;
        }
        let scale = self.page_scale();
        let rows = self.strips.rows;
        let top = (self.scroll * scale).round() as usize;
        let bottom = top + self.height as usize;
        let doc_rows = (view.tree.document_size.height * scale).ceil() as usize;
        let below = bottom.saturating_sub(1) / rows + 1;
        let above = (top / rows).checked_sub(1);
        for k in [Some(below), above].into_iter().flatten() {
            if k * rows < doc_rows && !self.strips.has(k) {
                self.strips
                    .get(k, &view.tree, raster, self.width as usize, scale);
                return true;
            }
        }
        false
    }

    /// Draw the page area into `frame` rows `top..top + height` (device px).
    pub(crate) fn compose(
        &mut self,
        frame: &mut [u32],
        stride: usize,
        top: usize,
        raster: &mut CpuRasterizer,
    ) {
        let page = &mut frame[top * stride..];
        let page_rows = (page.len() / stride).min(self.height as usize);
        page[..page_rows * stride].fill(WHITE);
        let Some(view) = &self.view else { return };
        let scale = self.page_scale();
        let first = (self.scroll * scale).round() as usize;
        let rows = self.strips.rows;
        let mut y = 0;
        while y < page_rows {
            let k = (first + y) / rows;
            let off = first + y - k * rows;
            let strip = self.strips.get(k, &view.tree, raster, stride, scale);
            let n = (rows - off)
                .min(page_rows - y)
                .min(strip.height.saturating_sub(off));
            if n == 0 {
                break;
            }
            for i in 0..n {
                copy_row(
                    &mut page[(y + i) * stride..(y + i + 1) * stride],
                    strip.row(off + i),
                );
            }
            y += n;
        }
        self.draw_scrollbar(page, stride, page_rows);
    }

    fn draw_scrollbar(&self, page: &mut [u32], w: usize, page_rows: usize) {
        let max = self.max_scroll();
        if max <= 0.0 || page_rows == 0 {
            return;
        }
        let view_h = self.page_size().1;
        let thumb = ((page_rows as f32 * view_h / (view_h + max)) as usize)
            .max((24.0 * self.scale) as usize)
            .min(page_rows);
        let y0 = ((page_rows - thumb) as f32 * self.scroll / max) as usize;
        let x1 = w.saturating_sub((3.0 * self.scale) as usize);
        let x0 = x1.saturating_sub((6.0 * self.scale) as usize);
        for y in y0..(y0 + thumb).min(page_rows) {
            for p in &mut page[y * w + x0..y * w + x1] {
                // 50% black over the page.
                *p = (*p >> 1) & 0x007F_7F7F;
            }
        }
    }

    /// In-page overlays (selection, find matches, caret), clipped to the page area.
    pub(crate) fn draw_page_overlays(&self, c: &mut Canvas) {
        interact::draw_page(self, c);
        live::draw_page(self, c);
    }

    /// Window overlays (find bar, menus, popups).
    pub(crate) fn draw_window_overlays(&mut self, c: &mut Canvas) {
        live::draw_window(self, c);
        interact::draw_window(self, c);
    }

    /// Per-tab fields of the headless `state`.
    pub(crate) fn state(&self, out: &mut serde_json::Map<String, serde_json::Value>) {
        let mut set = |k: &str, v: serde_json::Value| {
            out.insert(k.into(), v);
        };
        set("url", self.url().map(Url::as_str).into());
        set("title", self.title.clone().into());
        set("error", self.error.clone().into());
        set("load_ms", self.load_ms.into());
        set("loading", self.is_loading().into());
        set("history", self.history.len().into());
        set("index", self.index.into());
        set("scroll", self.scroll.into());
        set("max_scroll", self.max_scroll().into());
        set("zoom", self.zoom.into());
        set("hover", self.hover.as_ref().map(Url::as_str).into());
        set("strips", self.strips.count().into());
        set("discarded", self.discarded.into());
        set("icon", self.icon.as_ref().map(Url::as_str).into());
        set("favicon", self.favicon.is_some().into());
        interact::state(self, out);
        live::state(self, out);
    }
}

/// Same document: equal up to the fragment.
pub(crate) fn same_document(a: &Url, b: &Url) -> bool {
    let strip = |u: &Url| {
        let mut u = u.clone();
        u.set_fragment(None);
        u
    };
    strip(a) == strip(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_document_ignores_only_the_fragment() {
        let u = |s: &str| Url::parse(s).unwrap();
        assert!(same_document(&u("http://a/x#1"), &u("http://a/x#2")));
        assert!(same_document(&u("http://a/x"), &u("http://a/x#2")));
        assert!(!same_document(&u("http://a/x#1"), &u("http://a/y#1")));
        assert!(!same_document(&u("http://a/x?q=1"), &u("http://a/x?q=2")));
    }
}
