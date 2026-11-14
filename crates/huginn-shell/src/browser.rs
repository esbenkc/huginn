//! One browser window, independent of the window system: its tabs, the tab
//! strip, the toolbar, window-level shortcuts, input routing and frame
//! composition. `window.rs` drives it from winit events; `headless.rs`
//! drives it without a window. Windows live in a [`Session`](crate::session::Session).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use huginn_layout::{LayoutEngine, Rect};
use huginn_net::{DecodedImage, Url};
use huginn_paint::CpuRasterizer;

use crate::engine::EngineEvent;
use crate::input::{CursorKind, Key, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseKind};
use crate::internal_pages::{self, parse_input, parse_url};
use crate::omnibox::{self, Kind, Popup};
use crate::session::{ClosedTab, Shared, ZOOM_LEVELS};
use crate::shortcuts::{self, Action};
use crate::tab::{Opener, PAGE_TOP, Request, TABSTRIP_HEIGHT, Tab, TabId};
use crate::tabstrip::{StripHit, TabStrip};
use crate::toolbar::{Edit, Toolbar, ToolbarHit, ToolbarInfo};
use crate::ui::{Canvas, Pixels};

/// Frame interval of the loading spinners.
const ANIMATION_FRAME: Duration = Duration::from_millis(33);
/// Two presses within this time and distance make a double click.
const MULTI_CLICK: Duration = Duration::from_millis(500);
const MULTI_CLICK_DISTANCE: f32 = 4.0;
/// A tab moves with the pointer once it is dragged this far.
const DRAG_START: f32 = 5.0;

/// Requests for the session or the window system.
#[derive(Debug, PartialEq)]
pub enum WindowOp {
    NewWindow {
        url: Option<Url>,
        private: bool,
        opener: Box<Opener>,
    },
    /// Close this window (its tabs are gone).
    Close,
    Quit,
    /// Per-site zoom changed: other windows apply it too.
    ZoomChanged,
    Minimize,
    ToggleFullscreen,
    /// Start moving the window with the pointer (press on the empty strip).
    DragWindow,
}

/// Where the current press started; moves and the release go there.
#[derive(Copy, Clone, Debug, PartialEq)]
enum Press {
    Strip(Option<StripHit>),
    Toolbar(Option<ToolbarHit>),
    /// The address bar's drop-down (the row).
    Suggestions(Option<usize>),
    Page,
}

struct TabDrag {
    index: usize,
    /// Pointer x minus the tab's left edge when the drag started.
    grab: f32,
    start_x: f32,
    moving: bool,
}

pub struct Browser {
    /// Stable key of this window in the session.
    pub id: u64,
    shared: Rc<RefCell<Shared>>,
    /// No history is kept; the strip is dark.
    pub private: bool,
    tabs: Vec<Tab>,
    active: usize,
    /// Window size in device px and device px per CSS px.
    width: u32,
    height: u32,
    scale: f32,
    toolbar: Toolbar,
    /// The address bar's suggestions while the user types.
    omnibox: Popup,
    strip: TabStrip,
    /// For the strip, toolbar and status bubble (the page's lives on the engine thread).
    layout: LayoutEngine,
    raster: CpuRasterizer,
    status: Option<(Url, Pixels)>,
    pointer: (f32, f32),
    press: Option<(Press, MouseButton)>,
    last_click: Option<(Instant, f32, f32, MouseButton, u8)>,
    drag: Option<TabDrag>,
    /// Where the next tab opened from a link of tab `.0` goes.
    child_insert: Option<(TabId, usize)>,
    ops: Vec<WindowOp>,
    pub fullscreen: bool,
    pub minimized: bool,
    dirty: bool,
}

impl Browser {
    pub fn new(
        id: u64,
        shared: Rc<RefCell<Shared>>,
        private: bool,
        width: u32,
        height: u32,
        scale: f32,
    ) -> Browser {
        Browser {
            id,
            shared,
            private,
            tabs: Vec::new(),
            active: 0,
            width: width.max(1),
            height: height.max(1),
            scale,
            toolbar: Toolbar::default(),
            omnibox: Popup::default(),
            strip: TabStrip::default(),
            layout: LayoutEngine::new(),
            raster: CpuRasterizer::new(),
            status: None,
            pointer: (0.0, 0.0),
            press: None,
            last_click: None,
            drag: None,
            child_insert: None,
            ops: Vec::new(),
            fullscreen: false,
            minimized: false,
            dirty: true,
        }
    }

    // ----- state -------------------------------------------------------------

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn tab_mut(&mut self, i: usize) -> &mut Tab {
        &mut self.tabs[i]
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn has_tab(&self, id: TabId) -> bool {
        self.tabs.iter().any(|t| t.id == id)
    }

    fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn tab_mut_active(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// "Page title - Huginn" ("- Huginn Private" in private windows).
    pub fn window_title(&self) -> String {
        let app = if self.private {
            "Huginn Private"
        } else {
            "Huginn"
        };
        match self.tabs.get(self.active) {
            Some(t) => format!("{} - {app}", t.display_title()),
            None => app.into(),
        }
    }

    /// Window size in device px.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Device px per CSS px.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Some tab's engine still has commands to answer.
    pub fn busy(&self) -> bool {
        self.tabs.iter().any(Tab::busy)
    }

    /// True once after anything visible changed.
    pub fn take_dirty(&mut self) -> bool {
        let mut d = std::mem::take(&mut self.dirty);
        for t in &mut self.tabs {
            d |= t.take_dirty();
        }
        d
    }

    /// When the window must wake for the next animation frame, if animating.
    pub fn next_frame(&self) -> Option<Instant> {
        self.tabs
            .iter()
            .any(Tab::is_loading)
            .then(|| Instant::now() + ANIMATION_FRAME)
    }

    /// Advance animations (the loading spinners).
    pub fn tick(&mut self) {
        if self.tabs.iter().any(Tab::is_loading) {
            self.dirty = true;
        }
    }

    pub fn take_session_ops(&mut self) -> Vec<WindowOp> {
        let (session, window): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.ops).into_iter().partition(|op| {
                matches!(
                    op,
                    WindowOp::NewWindow { .. }
                        | WindowOp::Close
                        | WindowOp::Quit
                        | WindowOp::ZoomChanged
                )
            });
        self.ops = window;
        session
    }

    /// Ops for the window system (minimize, fullscreen, drag).
    pub fn take_window_ops(&mut self) -> Vec<WindowOp> {
        std::mem::take(&mut self.ops)
    }

    fn page_size_device(&self) -> (u32, u32) {
        let top = (PAGE_TOP * self.scale).round() as u32;
        (self.width, self.height.saturating_sub(top).max(1))
    }

    fn css_width(&self) -> f32 {
        self.width as f32 / self.scale
    }

    fn address_text(&self) -> String {
        if self.toolbar.focused {
            self.toolbar.text.clone()
        } else {
            self.tabs
                .get(self.active)
                .and_then(Tab::url)
                .map(Url::to_string)
                .unwrap_or_default()
        }
    }

    /// Window fields of the headless `state`, plus the active tab's.
    pub fn state(&self, out: &mut serde_json::Map<String, serde_json::Value>) {
        if let Some(t) = self.tabs.get(self.active) {
            t.state(out);
        }
        let bookmarked = self
            .tabs
            .get(self.active)
            .and_then(Tab::url)
            .is_some_and(|u| self.shared.borrow().is_bookmarked(u));
        let tabs: Vec<serde_json::Value> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                serde_json::json!({
                    "url": t.url().map(Url::as_str),
                    "title": t.display_title(),
                    "loading": t.is_loading(),
                    "discarded": t.discarded,
                    "active": i == self.active,
                    "zoom": t.zoom,
                })
            })
            .collect();
        let mut set = |k: &str, v: serde_json::Value| {
            out.insert(k.into(), v);
        };
        set("tabs", tabs.into());
        set("active_tab", self.active.into());
        set("private", self.private.into());
        set("address", self.address_text().into());
        set("focused", self.toolbar.focused.into());
        set(
            "address_selection",
            match self.toolbar.selection().filter(|_| self.toolbar.focused) {
                Some((a, b)) => serde_json::json!([a, b]),
                None => serde_json::Value::Null,
            },
        );
        let suggestions: Vec<serde_json::Value> = self
            .omnibox
            .result
            .matches
            .iter()
            .map(|m| {
                serde_json::json!({
                    "kind": match m.kind {
                        Kind::History => "history",
                        Kind::Typed => "typed",
                        Kind::Search => "search",
                    },
                    "url": m.url.as_str(),
                    "title": m.title,
                })
            })
            .collect();
        set("suggestions", suggestions.into());
        let (selected, rect) = if self.omnibox.is_open() {
            let r = self.omnibox.rect();
            (
                self.omnibox.selected.into(),
                serde_json::json!([r.x, r.y, r.width, r.height]),
            )
        } else {
            (serde_json::Value::Null, serde_json::Value::Null)
        };
        set("suggestion", selected);
        set("suggestions_rect", rect);
        set("bookmarked", bookmarked.into());
        set("fullscreen", self.fullscreen.into());
        set("minimized", self.minimized.into());
        set("page_top", PAGE_TOP.into());
        set("size", serde_json::json!([self.width, self.height]));
        set("scale", self.scale.into());
    }

    // ----- tabs --------------------------------------------------------------

    /// Open a tab on `url` (or the new tab page, with the address bar
    /// focused) at `at` (default: the end). Returns its index.
    pub fn open_tab(&mut self, url: Option<Url>, background: bool, at: Option<usize>) -> usize {
        self.open_tab_from(url, Opener::default(), background, at)
    }

    /// [`Browser::open_tab`] for a page's link or form (`from`).
    pub fn open_tab_from(
        &mut self,
        url: Option<Url>,
        from: Opener,
        background: bool,
        at: Option<usize>,
    ) -> usize {
        let id = self.shared.borrow_mut().new_tab_id();
        let (w, h) = self.page_size_device();
        let mut tab = Tab::new(id, self.shared.clone(), self.private, w, h, self.scale);
        let new_tab_page = url.is_none();
        tab.navigate_with(
            url.unwrap_or_else(|| internal_pages::url(internal_pages::NEW_TAB)),
            from,
        );
        let at = at.unwrap_or(self.tabs.len()).min(self.tabs.len());
        self.tabs.insert(at, tab);
        if at <= self.active && self.tabs.len() > 1 {
            self.active += 1;
        }
        if background && self.tabs.len() > 1 {
            self.tabs[at].set_visible(false);
        } else {
            self.activate(at);
            if new_tab_page {
                self.focus_field(String::new());
            }
        }
        self.strip.invalidate();
        self.dirty = true;
        at
    }

    /// Make tab `i` the active one.
    pub fn activate(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        if i != self.active && self.active < self.tabs.len() {
            self.tabs[self.active].set_visible(false);
            self.child_insert = None;
        }
        self.active = i;
        let clock = self.shared.borrow_mut().tick();
        let tab = &mut self.tabs[i];
        tab.last_active = clock;
        tab.set_visible(true);
        self.blur_address();
        self.status = None;
        self.dirty = true;
    }

    /// Close tab `i`; the window closes with its last tab.
    pub fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(i);
        if !self.private && !tab.history.is_empty() {
            self.shared.borrow_mut().closed.push(ClosedTab {
                history: tab.history.clone(),
                index: tab.index,
                title: tab.title.clone(),
            });
        }
        drop(tab);
        self.drag = None;
        self.press = None;
        if self.tabs.is_empty() {
            self.ops.push(WindowOp::Close);
            return;
        }
        // Like Chrome: the tab to the right becomes active, else the one to the left.
        if i < self.active {
            self.active -= 1;
        } else if i == self.active {
            self.active = i.min(self.tabs.len() - 1);
            let a = self.active;
            self.active = usize::MAX;
            self.activate(a);
        }
        self.strip.invalidate();
        self.dirty = true;
    }

    fn reopen_closed_tab(&mut self) {
        let Some(closed) = self.shared.borrow_mut().closed.pop() else {
            return;
        };
        let id = self.shared.borrow_mut().new_tab_id();
        let (w, h) = self.page_size_device();
        let mut tab = Tab::new(id, self.shared.clone(), self.private, w, h, self.scale);
        tab.restore_history(closed.history, closed.index, closed.title);
        tab.set_visible(false);
        self.tabs.push(tab);
        self.activate(self.tabs.len() - 1);
        self.strip.invalidate();
    }

    /// Open `url` from tab `opener` in a new tab, like Chrome: a foreground
    /// tab goes right after the opener, a background tab after the tabs the
    /// opener already opened.
    fn open_child_tab(&mut self, opener: usize, url: Url, background: bool, from: Opener) {
        let opener_id = self.tabs[opener].id;
        let at = match self.child_insert {
            Some((id, at)) if id == opener_id && background => at,
            _ => opener + 1,
        };
        let at = self.open_tab_from(Some(url), from, background, Some(at));
        if background {
            self.child_insert = Some((opener_id, at + 1));
        }
    }

    /// Carry out what tab `i` asked for.
    fn run_requests(&mut self, i: usize) {
        let requests = self.tabs[i].take_requests();
        for r in requests {
            match r {
                Request::Navigate(u) => self.tabs[i].navigate(u),
                Request::OpenTab {
                    url,
                    background,
                    opener,
                } => self.open_child_tab(i, url, background, *opener),
                Request::OpenWindow {
                    url,
                    private,
                    opener,
                } => self.ops.push(WindowOp::NewWindow {
                    url: Some(url),
                    private,
                    opener,
                }),
                Request::Back => self.tabs[i].back(),
                Request::Forward => self.tabs[i].forward(),
                Request::Reload => self.tabs[i].reload(false),
                Request::ViewSource => {
                    if let Some(u) = self.tabs[i]
                        .url()
                        .and_then(|u| Url::parse(&format!("view-source:{u}")).ok())
                    {
                        self.open_child_tab(i, u, false, Opener::default());
                    }
                }
            }
        }
    }

    pub fn handle(&mut self, id: TabId, ev: EngineEvent) {
        let Some(i) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        self.tabs[i].handle(ev);
        self.run_requests(i);
    }

    /// A favicon arrived: tabs showing that icon pick it up.
    pub fn set_favicon(&mut self, url: &Url, image: Option<&DecodedImage>) {
        for t in &mut self.tabs {
            if t.icon.as_ref() == Some(url) {
                t.favicon = image.cloned();
                self.dirty = true;
            }
        }
    }

    /// Apply the session's per-site zoom to every tab.
    pub fn apply_zoom(&mut self) {
        for t in &mut self.tabs {
            if let Some(u) = t.url() {
                let z = self.shared.borrow().zoom_for(u);
                t.set_zoom(z);
            }
        }
    }

    // ----- actions -----------------------------------------------------------

    fn act(&mut self, a: Action) {
        match a {
            Action::NewTab => {
                self.open_tab(None, false, None);
            }
            Action::NewWindow => self.ops.push(WindowOp::NewWindow {
                url: None,
                private: false,
                opener: Box::default(),
            }),
            Action::NewPrivateWindow => self.ops.push(WindowOp::NewWindow {
                url: None,
                private: true,
                opener: Box::default(),
            }),
            Action::CloseTab => self.close_tab(self.active),
            Action::CloseWindow => {
                while !self.tabs.is_empty() {
                    self.close_tab(self.tabs.len() - 1);
                }
            }
            Action::ReopenClosedTab => self.reopen_closed_tab(),
            Action::SelectTab(i) => self.activate(i.min(self.tabs.len() - 1)),
            Action::LastTab => self.activate(self.tabs.len() - 1),
            Action::NextTab => self.activate((self.active + 1) % self.tabs.len()),
            Action::PrevTab => self.activate((self.active + self.tabs.len() - 1) % self.tabs.len()),
            Action::Quit => self.ops.push(WindowOp::Quit),
            Action::Minimize => {
                self.minimized = true;
                self.ops.push(WindowOp::Minimize);
            }
            Action::Fullscreen => {
                self.fullscreen = !self.fullscreen;
                self.ops.push(WindowOp::ToggleFullscreen);
            }
            Action::FocusAddress => self.focus_address_bar(),
            Action::Back => self.tab_mut_active().back(),
            Action::Forward => self.tab_mut_active().forward(),
            Action::Reload | Action::HardReload => {
                self.blur_address();
                self.tab_mut_active()
                    .reload(matches!(a, Action::HardReload));
            }
            Action::Home => {
                self.tab_mut_active()
                    .navigate(internal_pages::url(internal_pages::NEW_TAB));
                self.focus_field(String::new());
            }
            Action::History => {
                self.open_tab(
                    Some(internal_pages::url(internal_pages::HISTORY)),
                    false,
                    None,
                );
            }
            Action::Bookmarks => {
                self.open_tab(
                    Some(internal_pages::url(internal_pages::BOOKMARKS)),
                    false,
                    None,
                );
            }
            Action::Bookmark => {
                let t = self.tab();
                if let Some(u) = t.url().cloned() {
                    let title = t.title.clone();
                    self.shared.borrow_mut().toggle_bookmark(&u, &title);
                    self.dirty = true;
                }
            }
            Action::ClearHistory => {
                self.shared.borrow_mut().clear_history();
                self.open_tab(
                    Some(internal_pages::url(internal_pages::HISTORY)),
                    false,
                    None,
                );
            }
            Action::ZoomIn | Action::ZoomOut | Action::ZoomReset => {
                let cur = self.tab().zoom;
                let z = match a {
                    Action::ZoomIn => ZOOM_LEVELS.iter().copied().find(|&z| z > cur + 0.001),
                    Action::ZoomOut => ZOOM_LEVELS.iter().rev().copied().find(|&z| z < cur - 0.001),
                    _ => Some(1.0),
                };
                if let (Some(z), Some(u)) = (z, self.tab().url().cloned()) {
                    self.shared.borrow_mut().set_zoom(&u, z);
                    self.ops.push(WindowOp::ZoomChanged);
                }
            }
        }
        self.dirty = true;
    }

    pub fn focus_address_bar(&mut self) {
        let text = self
            .tab()
            .url()
            .filter(|u| !internal_pages::is_new_tab(u))
            .map(Url::to_string)
            .unwrap_or_default();
        self.focus_field(text);
        self.dirty = true;
    }

    /// Focus the address field with `text` selected; no suggestions yet.
    fn focus_field(&mut self, text: String) {
        self.toolbar.focus(text);
        self.omnibox.close();
    }

    /// Leave the address field and close its suggestions.
    fn blur_address(&mut self) {
        self.toolbar.blur();
        self.omnibox.close();
    }

    // ----- input -------------------------------------------------------------

    pub fn resize(&mut self, width: u32, height: u32, scale: f32) {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height, scale) == (self.width, self.height, self.scale) {
            return;
        }
        self.width = width;
        self.height = height;
        self.scale = scale;
        let (w, h) = self.page_size_device();
        for t in &mut self.tabs {
            t.resize(w, h, scale);
        }
        self.status = None;
        self.strip.invalidate();
        self.toolbar.invalidate();
        self.dirty = true;
    }

    pub fn key(&mut self, ev: KeyEvent) {
        if let Some(a) = shortcuts::action(&ev) {
            self.act(a);
            return;
        }
        if self.toolbar.focused {
            if self.suggestions_key(&ev) {
                self.dirty = true;
                return;
            }
            match self.toolbar.edit(&ev) {
                Edit::Go => self.go(&ev),
                Edit::Cancel => self.blur_address(),
                Edit::Text { inline } => self.autocomplete(inline),
                Edit::Caret => {}
                // Page chords the field does not use (Cmd+F/G/E/P/S/U) work
                // from the address bar too, like Chrome's omnibox.
                Edit::Ignored if ev.mods.cmd => {
                    self.blur_address();
                    let a = self.active;
                    self.tabs[a].key(&ev);
                    self.run_requests(a);
                }
                Edit::Ignored => {
                    if ev.key == Key::Tab {
                        self.blur_address();
                    }
                }
            }
            self.dirty = true;
            return;
        }
        let a = self.active;
        self.tabs[a].key(&ev);
        self.run_requests(a);
    }

    /// Keys the drop-down takes: Up and Down pick a row (the field shows
    /// it), Esc on a picked row goes back to what the user typed.
    fn suggestions_key(&mut self, ev: &KeyEvent) -> bool {
        let m = ev.mods;
        if !self.omnibox.is_open() || m.cmd || m.alt || m.ctrl {
            return false;
        }
        let n = self.omnibox.result.matches.len();
        let to = match ev.key {
            Key::Up => self.omnibox.selected.saturating_sub(1),
            Key::Down => (self.omnibox.selected + 1).min(n - 1),
            Key::Escape if self.omnibox.selected > 0 => 0,
            _ => return false,
        };
        self.pick(to);
        true
    }

    /// Select row `i` of the drop-down; the field shows its text (row 0:
    /// what the user typed and the inline completion).
    fn pick(&mut self, i: usize) {
        if i == self.omnibox.selected {
            return;
        }
        self.omnibox.select(i);
        if i == 0 {
            self.toolbar.set_text(self.omnibox.user_text.clone());
            let inline = self.omnibox.result.inline.clone();
            self.toolbar.set_completion(&inline);
        } else {
            let fill = self.omnibox.result.matches[i].fill.clone();
            self.toolbar.set_text(fill);
        }
    }

    /// The field's text changed: new suggestions, and the inline
    /// completion if `inline` allows one.
    fn autocomplete(&mut self, inline: bool) {
        let text = self.toolbar.text.clone();
        let result = {
            let _span = crate::perf::span("suggest");
            let mut shared = self.shared.borrow_mut();
            omnibox::suggest(
                shared.history.rows(),
                &text,
                inline,
                crate::history::now_ms(),
            )
        };
        if result.matches.is_empty() {
            self.omnibox.close();
            return;
        }
        self.toolbar.set_completion(&result.inline);
        self.omnibox.set(text, result);
        self.place_suggestions();
        // Like Chrome, warm up a connection to a default match from history
        // while the user is still typing.
        if let Some(m) = self.omnibox.result.matches.first()
            && m.kind == Kind::History
            && matches!(m.url.scheme(), "http" | "https")
        {
            let origin = m.url.origin().ascii_serialization();
            if self.omnibox.preconnected.as_deref() != Some(origin.as_str()) {
                self.shared.borrow().preconnect(self.private, &m.url);
                self.omnibox.preconnected = Some(origin);
            }
        }
    }

    /// Put the drop-down under the address field as it is on screen.
    fn place_suggestions(&mut self) {
        if !self.omnibox.is_open() {
            return;
        }
        self.prepare_chrome();
        let f = self.toolbar.field();
        self.omnibox.place(Rect {
            y: f.y + TABSTRIP_HEIGHT,
            ..f
        });
    }

    /// Where Enter (or a click on row `row`) goes, and whether history
    /// counts it as typed: the picked row, else the default match while the
    /// field shows it, else the text as a URL or a search.
    fn destination(&self, row: Option<usize>) -> Option<(Url, bool)> {
        let p = &self.omnibox;
        let picked = match row {
            Some(i) => p.result.matches.get(i),
            None if p.selected > 0 => p.result.matches.get(p.selected),
            None => p
                .result
                .matches
                .first()
                .filter(|_| self.toolbar.text == p.default_text()),
        };
        if let Some(m) = picked {
            return Some((m.url.clone(), m.kind != Kind::Search));
        }
        let text = &self.toolbar.text;
        // Enter on the URL the field was focused with is not typed.
        let typed = p.is_open();
        match parse_url(text) {
            Some(u) => Some((u, typed)),
            None => parse_input(text).map(|u| (u, false)),
        }
    }

    /// Enter in the address field. Option: new tab; Cmd: new background
    /// tab; Shift: new window.
    fn go(&mut self, ev: &KeyEvent) {
        if let Some((url, typed)) = self.destination(None) {
            self.open_from_address(url, typed, ev.mods, false);
        }
    }

    /// Open `url` from the address bar; modifiers as for Enter, `middle`
    /// (a middle click on a suggestion): a new background tab.
    fn open_from_address(&mut self, url: Url, typed: bool, m: Modifiers, middle: bool) {
        self.blur_address();
        let from = Opener {
            typed,
            ..Opener::default()
        };
        if m.alt || m.cmd || middle {
            let background = (m.cmd || middle) && !m.alt;
            self.open_tab_from(Some(url), from, background, Some(self.active + 1));
        } else if m.shift {
            self.ops.push(WindowOp::NewWindow {
                url: Some(url),
                private: self.private,
                opener: Box::new(from),
            });
        } else {
            self.tab_mut_active().navigate_with(url, from);
        }
    }

    /// A wheel turn of `dy` CSS px at the pointer ([`Tab::wheel`]).
    pub fn wheel(&mut self, dy: f32) {
        let (x, y) = self.pointer;
        self.tab_mut_active().wheel(x, y, dy);
    }

    /// A finished two-finger trackpad swipe with total movement (`dx`, `dy`)
    /// in CSS px: a mostly horizontal swipe of 160 px or more goes back
    /// (fingers to the right) or forward, like Chrome. Pages do not scroll
    /// sideways, so every horizontal swipe is a gesture.
    pub fn swipe(&mut self, dx: f32, dy: f32) -> bool {
        if dx.abs() < 160.0 || dx.abs() < 2.5 * dy.abs() {
            return false;
        }
        let t = self.tab_mut_active();
        if dx > 0.0 {
            t.back()
        } else {
            t.forward()
        }
        self.dirty = true;
        true
    }

    /// A pointer event. Returns the cursor to show.
    pub fn mouse(&mut self, mut ev: MouseEvent) -> CursorKind {
        self.pointer = (ev.x, ev.y);
        if ev.y < PAGE_TOP {
            self.prepare_chrome();
        }
        self.place_suggestions();
        if ev.kind == MouseKind::Down {
            ev.clicks = self.count_clicks(&ev);
        }
        let in_suggestions = {
            let r = self.omnibox.rect();
            self.omnibox.is_open()
                && ev.x >= r.x
                && ev.x < r.right()
                && ev.y >= r.y
                && ev.y < r.bottom()
        };
        let area = match (ev.kind, self.press) {
            (MouseKind::Down, _) | (_, None) => {
                if in_suggestions {
                    Press::Suggestions(self.omnibox.row_at(ev.x, ev.y))
                } else if ev.y < TABSTRIP_HEIGHT {
                    Press::Strip(self.strip.hit(ev.x, ev.y))
                } else if ev.y < PAGE_TOP {
                    Press::Toolbar(self.toolbar.hit(ev.x, ev.y - TABSTRIP_HEIGHT))
                } else {
                    Press::Page
                }
            }
            (_, Some((p, _))) => p,
        };
        if ev.kind == MouseKind::Down {
            self.press = Some((area, ev.button));
        }
        let release = ev.kind == MouseKind::Up;
        let cursor = match area {
            Press::Strip(hit) => {
                self.toolbar.set_hover(None);
                self.omnibox.hover(None);
                self.strip_mouse(&ev, hit);
                CursorKind::Default
            }
            Press::Suggestions(pressed) => {
                self.toolbar.set_hover(None);
                self.set_strip_hover(None);
                let here = self.omnibox.row_at(ev.x, ev.y);
                self.omnibox.hover(here);
                // A click is a press and release on the same row.
                if release
                    && ev.button != MouseButton::Right
                    && let Some(i) = here.filter(|&i| Some(i) == pressed)
                    && let Some((url, typed)) = self.destination(Some(i))
                {
                    self.open_from_address(url, typed, ev.mods, ev.button == MouseButton::Middle);
                }
                CursorKind::Default
            }
            Press::Toolbar(hit) => {
                self.omnibox.hover(None);
                if ev.kind == MouseKind::Down && ev.button == MouseButton::Left {
                    self.toolbar_click(hit);
                }
                self.toolbar.set_hover(hit);
                self.set_strip_hover(None);
                if hit == Some(ToolbarHit::Field) {
                    CursorKind::Text
                } else {
                    CursorKind::Default
                }
            }
            Press::Page => {
                self.toolbar.set_hover(None);
                self.set_strip_hover(None);
                self.omnibox.hover(None);
                if ev.kind == MouseKind::Down {
                    self.blur_address();
                }
                let a = self.active;
                self.tabs[a].mouse(&ev);
                self.run_requests(a);
                let t = &self.tabs[self.active];
                if let Some(c) = t.cursor {
                    c
                } else if t.hover.is_some() {
                    CursorKind::Pointer
                } else {
                    CursorKind::Default
                }
            }
        };
        if release {
            self.press = None;
            self.drag = None;
        }
        if area != Press::Page {
            self.tab_mut_active().mouse_leave();
        }
        self.dirty = true;
        cursor
    }

    /// Click count of a press: 2 and 3 for quick presses at the same spot.
    /// A count already in the event (headless) wins; the window sends 0.
    fn count_clicks(&mut self, ev: &MouseEvent) -> u8 {
        let now = Instant::now();
        let n = match self.last_click {
            _ if ev.clicks > 0 => ev.clicks,
            Some((t, x, y, b, n))
                if b == ev.button
                    && now - t < MULTI_CLICK
                    && (x - ev.x).abs() <= MULTI_CLICK_DISTANCE
                    && (y - ev.y).abs() <= MULTI_CLICK_DISTANCE =>
            {
                n % 3 + 1
            }
            _ => 1,
        };
        self.last_click = Some((now, ev.x, ev.y, ev.button, n));
        n
    }

    fn set_strip_hover(&mut self, hit: Option<StripHit>) {
        if self.strip.hover != hit {
            self.strip.hover = hit;
            self.dirty = true;
        }
    }

    fn strip_mouse(&mut self, ev: &MouseEvent, pressed: Option<StripHit>) {
        let here = self.strip.hit(ev.x, ev.y);
        match ev.kind {
            MouseKind::Move => {
                if let Some(d) = &mut self.drag {
                    if !d.moving && (ev.x - d.start_x).abs() >= DRAG_START {
                        d.moving = true;
                    }
                    if d.moving {
                        self.drag_tab(ev.x);
                    }
                }
                if self.drag.is_none() {
                    self.set_strip_hover(here);
                }
            }
            MouseKind::Down => match (here, ev.button) {
                (Some(StripHit::Tab(i)), MouseButton::Left) => {
                    self.activate(i);
                    let left = self.strip.tab_rect(i).map_or(ev.x, |r| r.x);
                    self.drag = Some(TabDrag {
                        index: i,
                        grab: ev.x - left,
                        start_x: ev.x,
                        moving: false,
                    });
                }
                (None, MouseButton::Left) => self.ops.push(WindowOp::DragWindow),
                _ => {}
            },
            MouseKind::Up => {
                // A click is a press and release on the same target.
                if here != pressed {
                    return;
                }
                match (here, ev.button) {
                    (Some(StripHit::Close(i)), MouseButton::Left)
                    | (Some(StripHit::Tab(i) | StripHit::Close(i)), MouseButton::Middle) => {
                        self.close_tab(i);
                    }
                    (Some(StripHit::Plus), MouseButton::Left) => {
                        self.open_tab(None, false, None);
                    }
                    _ => {}
                }
                self.set_strip_hover(self.strip.hit(ev.x, ev.y));
            }
        }
    }

    /// Move the dragged tab to the slot under the pointer.
    fn drag_tab(&mut self, x: f32) {
        let Some(d) = &self.drag else { return };
        let w = crate::tabstrip::tab_width(self.tabs.len(), self.css_width());
        let left = x - d.grab - crate::tabstrip::LEFT_INSET;
        let slot = ((left + w / 2.0) / w)
            .floor()
            .clamp(0.0, (self.tabs.len() - 1) as f32) as usize;
        let from = d.index;
        if slot == from {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(slot, tab);
        if self.active == from {
            self.active = slot;
        } else if from < self.active && slot >= self.active {
            self.active -= 1;
        } else if from > self.active && slot <= self.active {
            self.active += 1;
        }
        if let Some(d) = &mut self.drag {
            d.index = slot;
        }
        self.child_insert = None;
        self.strip.invalidate();
        self.dirty = true;
    }

    fn toolbar_click(&mut self, hit: Option<ToolbarHit>) {
        match hit {
            Some(ToolbarHit::Back) => self.tab_mut_active().back(),
            Some(ToolbarHit::Forward) => self.tab_mut_active().forward(),
            Some(ToolbarHit::Reload) => {
                let t = self.tab_mut_active();
                if !t.stop() {
                    t.reload(false);
                }
            }
            Some(ToolbarHit::Star) => self.act(Action::Bookmark),
            Some(ToolbarHit::Field) => {
                if self.toolbar.focused {
                    self.toolbar.caret_to_end();
                } else {
                    self.focus_address_bar();
                }
            }
            None => {}
        }
        self.dirty = true;
    }

    /// The pointer left the window.
    pub fn mouse_leave(&mut self) {
        self.set_strip_hover(None);
        self.omnibox.hover(None);
        self.tab_mut_active().mouse_leave();
    }

    // ----- painting ----------------------------------------------------------

    /// Rasterize one strip of the active page just outside the viewport.
    /// Returns true if it did work (call again when idle).
    pub fn prefetch(&mut self) -> bool {
        if self.dirty {
            return false;
        }
        let a = self.active;
        self.tabs[a].prefetch(&mut self.raster)
    }

    /// Draw the whole window into `frame` (`width * height` 0RGB pixels).
    pub fn compose(&mut self, frame: &mut [u32]) {
        let _span = crate::perf::span("compose");
        let (w, h) = (self.width as usize, self.height as usize);
        assert_eq!(frame.len(), w * h, "frame size");
        let width = self.css_width();
        let strip_rows = ((TABSTRIP_HEIGHT * self.scale).round() as usize).min(h);
        let page_top = ((PAGE_TOP * self.scale).round() as usize).min(h);

        // Page layer and its overlays.
        let a = self.active;
        self.tabs[a].compose(frame, w, page_top, &mut self.raster);
        let mut canvas = Canvas {
            frame: &mut *frame,
            width: w,
            height: h,
            scale: self.scale,
            clip: (0, page_top, w, h),
        };
        self.tabs[a].draw_page_overlays(&mut canvas);

        // Status bubble with the hovered link.
        let hover = self.tabs[a].hover.clone();
        if self.status.as_ref().map(|(u, _)| u) != hover.as_ref() {
            self.status = hover.map(|u| {
                let px = self.render_status(u.as_str());
                (u, px)
            });
        }
        if let Some((_, s)) = &self.status {
            s.blit(frame, w, 0, h.saturating_sub(s.height).max(page_top));
        }

        // Tab strip and toolbar.
        self.prepare_chrome();
        self.strip
            .draw(&self.tabs, self.scale, &mut frame[..strip_rows * w], w);
        let info = self.toolbar_info();
        let px = self
            .toolbar
            .pixels(&mut self.layout, &mut self.raster, &info, width, self.scale);
        px.blit(&mut frame[..page_top * w], w, 0, strip_rows);
        // Loading: a thin accent bar sweeps along the bottom of the toolbar.
        if let Some(since) = self.tabs[a].loading_since() {
            let t = since.elapsed().as_secs_f32() % 1.4 / 1.4;
            let seg = width * 0.25;
            let mut canvas = Canvas {
                frame: &mut frame[..page_top * w],
                width: w,
                height: page_top,
                scale: self.scale,
                clip: (0, strip_rows, w, page_top),
            };
            canvas.fill(
                Rect {
                    x: t * (width + seg) - seg,
                    y: PAGE_TOP - 2.0,
                    width: seg,
                    height: 2.0,
                },
                crate::theme::accent_rgb(self.private),
            );
        }

        // Window overlays: find bar, menus, popups, then the address bar's
        // suggestions over all of them.
        let mut canvas = Canvas {
            frame,
            width: w,
            height: h,
            scale: self.scale,
            clip: (0, 0, w, h),
        };
        self.tabs[a].draw_window_overlays(&mut canvas);
        self.place_suggestions();
        self.omnibox.draw(
            &mut canvas,
            &mut self.layout,
            &mut self.raster,
            self.private,
        );
    }

    fn toolbar_info(&self) -> ToolbarInfo {
        let tab = &self.tabs[self.active];
        ToolbarInfo {
            url: tab.url().cloned(),
            can_back: tab.can_back(),
            can_forward: tab.can_forward(),
            loading: tab.is_loading(),
            bookmarked: tab
                .url()
                .is_some_and(|u| self.shared.borrow().is_bookmarked(u)),
            zoom: tab.zoom,
            private: self.private,
        }
    }

    /// Render the strip and toolbar if their content changed, so their hit
    /// regions match what is (or will be) on screen.
    fn prepare_chrome(&mut self) {
        let width = self.css_width();
        self.strip.prepare(
            &mut self.layout,
            &mut self.raster,
            &self.tabs,
            self.active,
            self.private,
            width,
            self.scale,
        );
        let info = self.toolbar_info();
        self.toolbar
            .pixels(&mut self.layout, &mut self.raster, &info, width, self.scale);
    }

    fn render_status(&mut self, url: &str) -> Pixels {
        let width = self.css_width();
        let html = format!(
            "<body style=\"margin:0\"><div style=\"display:flex\"><div id=s style=\"max-width:{:.0}px;\
             overflow:hidden;white-space:pre;background:{};border:1px solid {};border-left:0;\
             border-bottom:0;padding:4px 8px;font-family:{};font-size:11px;color:{}\">{}</div></div></body>",
            width * 0.6,
            crate::theme::TOOLBAR,
            crate::theme::HAIRLINE,
            crate::theme::FONT,
            crate::theme::MUTED,
            crate::engine::escape_html(url)
        );
        let images = huginn_net::ImageStore::new();
        let (tree, rects) =
            crate::ui::layout_snippet(&mut self.layout, &html, width, 40.0, &["s"], &images);
        let region = Rect {
            x: 0.0,
            y: 0.0,
            width: rects[0].right().max(1.0),
            height: rects[0].bottom().max(1.0),
        };
        let list = huginn_paint::build_display_list(&tree, Some(region));
        use huginn_paint::Rasterizer;
        Pixels::from_rgba(self.raster.rasterize(&list, region, self.scale))
    }
}
