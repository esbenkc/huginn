//! In-page interaction: text selection and copy,
//! find in page, inline-link hit testing, the context menu, view source,
//! save page and print to PDF. The tab calls these hooks; see CONTRACTS.md,
//! "huginn-shell window: tab hooks". Each returns true when it handled the
//! event (the tab's defaults then do not run).
//!
//! Text positions are byte offsets into the page's `TextIndex` (built by
//! huginn-layout for the window only). Overlays (selection, find matches) are
//! multiplied over the page pixels after the strips are blitted, so the
//! strips never need to be rasterized again for them.

pub mod focus;
pub mod hit;
pub mod menu;
pub mod print;
pub mod source;
pub mod text;

use std::any::Any;
use std::cell::RefCell;
use std::ops::Range;

use huginn_layout::{LayoutEngine, Rect};
use huginn_net::{ImageStore, Url};
use huginn_paint::CpuRasterizer;

use crate::engine::{EngineEvent, escape_html};
use crate::input::{CursorKind, Key, KeyEvent, MouseButton, MouseEvent, MouseKind};
use crate::tab::{Opener, PAGE_TOP, Request, Tab};
use crate::ui::{Canvas, Pixels, render_snippet};
use menu::{Menu, MenuItem, MenuKey};

/// Chrome's selection color on macOS (light blue), multiplied under the text.
const SELECTION: u32 = 0x00B4_D5FE;
/// Find matches: yellow; the active match: orange (Chrome's colors).
const MATCH: u32 = 0x00FF_FF00;
const ACTIVE_MATCH: u32 = 0x00FF_9632;
/// Most matches find counts (Chrome stops at 1000 for the count too).
const MAX_MATCHES: usize = 1000;
/// Find bar size, window CSS px.
const FIND_W: f32 = 372.0;
const FIND_H: f32 = 48.0;
/// The pointer must move this far (CSS px) before a press becomes a drag.
const DRAG_SLOP: f32 = 3.0;

/// Per-tab state, at `tab.interact`.
#[derive(Default)]
pub struct PageState {
    /// Selected text range (bytes of the text index); never empty.
    selection: Option<Range<u32>>,
    /// Selection rectangles in document px, cached for drawing.
    selection_rects: Vec<Rect>,
    drag: Option<Drag>,
    find: Find,
    menu: Option<ContextMenu>,
    /// A message bubble (saved file, print result) until the next input.
    notice: Option<Notice>,
    /// Length and hash of the text the selection and matches refer to.
    text_key: (usize, u64),
    /// Per run of the current tree: the rectangle it shows in (overflow clips).
    clips: Vec<Rect>,
}

struct Drag {
    /// The unit pressed on: a caret, a word (double click) or a paragraph (triple click).
    anchor: Range<u32>,
    granularity: Granularity,
    start: (f32, f32),
    moved: bool,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Granularity {
    Char,
    Word,
    Paragraph,
}

#[derive(Default)]
struct Find {
    open: bool,
    /// Keys go to the find field.
    focused: bool,
    query: String,
    /// The field's text is selected (typing replaces it).
    select_all: bool,
    matches: Vec<Range<u32>>,
    /// Highlight rectangles per match (document px), same order as `matches`.
    rects: Vec<Vec<Rect>>,
    current: Option<usize>,
    /// Rendered bar and its buttons (window px, relative to the bar).
    pixels: Option<Pixels>,
    buttons: [Rect; 3],
}

struct ContextMenu {
    menu: Menu,
    actions: Vec<Action>,
    pixels: Option<Pixels>,
    /// The pointer moved since the menu opened (a release then activates).
    moved: bool,
}

struct Notice {
    text: String,
    pixels: Option<Pixels>,
}

#[derive(Clone, Debug)]
enum Action {
    Request(Request),
    Copy,
    CopyText(String),
    SelectAll,
    Print,
    Save,
}

/// Results of engine jobs, back through `on_engine`.
enum JobResult {
    Printed { path: String, pages: usize },
    Saved { path: String },
    Failed(String),
}

// ----- UI rendering ----------------------------------------------------------

thread_local! {
    /// One layout engine and rasterizer for every overlay of this thread (the
    /// UI thread): fonts load once, not per tab.
    static UI: RefCell<Option<(LayoutEngine, CpuRasterizer, ImageStore)>> = const { RefCell::new(None) };
}

fn render(html: &str, width: f32, height: f32, ids: &[&str], scale: f32) -> (Pixels, Vec<Rect>) {
    UI.with(|ui| {
        let mut ui = ui.borrow_mut();
        let (layout, raster, images) = ui
            .get_or_insert_with(|| (LayoutEngine::new(), CpuRasterizer::new(), ImageStore::new()));
        render_snippet(layout, raster, html, width, height, ids, images, scale)
    })
}

// ----- hooks -----------------------------------------------------------------

/// Keys while the find bar or the context menu has focus, before the page's scripts.
pub fn key_capture(tab: &mut Tab, ev: &KeyEvent) -> bool {
    dismiss_notice(tab);
    if let Some(m) = &mut tab.interact.menu {
        match m.menu.key(&ev.key) {
            MenuKey::Activate(i) => {
                let action = m.actions[i].clone();
                close_menu(tab);
                run_action(tab, action);
            }
            MenuKey::Close => close_menu(tab),
            MenuKey::None => {
                m.pixels = None;
                tab.redraw();
            }
        }
        return true;
    }
    if !(tab.interact.find.open && tab.interact.find.focused) {
        return false;
    }
    let m = ev.mods;
    let find = &mut tab.interact.find;
    match &ev.key {
        Key::Char(c) if m.cmd && c.eq_ignore_ascii_case("v") => {
            if let Some(t) = crate::clipboard::get() {
                let t = t.lines().next().unwrap_or("").to_owned();
                find_insert(tab, &t);
            }
        }
        Key::Char(c) if m.cmd && c.eq_ignore_ascii_case("a") => {
            find.select_all = true;
            find.pixels = None;
            tab.redraw();
        }
        Key::Char(c) if m.cmd && (c.eq_ignore_ascii_case("c") || c.eq_ignore_ascii_case("x")) => {
            crate::clipboard::set(&find.query.clone());
            if c.eq_ignore_ascii_case("x") && find.select_all {
                find_insert(tab, "");
            }
        }
        // Other shortcuts (Cmd+G, Cmd+W, ...) are not the field's.
        _ if m.cmd || m.ctrl => return false,
        Key::Char(c) => {
            let c = c.clone();
            find_insert(tab, &c);
        }
        Key::Space => find_insert(tab, " "),
        Key::Backspace | Key::Delete => {
            let find = &mut tab.interact.find;
            if find.select_all {
                find.query.clear();
            } else {
                find.query.pop();
            }
            find.select_all = false;
            search(tab, true);
        }
        Key::Enter => step(tab, !m.shift),
        Key::Escape => close_find(tab),
        Key::Left | Key::Right | Key::Home | Key::End => {
            find.select_all = false;
            find.pixels = None;
            tab.redraw();
        }
        // The field keeps the other keys (no page scrolling while typing).
        _ => {}
    }
    true
}

/// Keys after the page's scripts and form controls.
pub fn key(tab: &mut Tab, ev: &KeyEvent) -> bool {
    let m = ev.mods;
    if !m.cmd || m.ctrl || m.alt {
        return false;
    }
    let Key::Char(c) = &ev.key else { return false };
    match (c.to_ascii_lowercase().as_str(), m.shift) {
        ("f", false) => open_find(tab),
        ("g", back) => {
            if tab.interact.find.query.is_empty() {
                open_find(tab);
            } else {
                if !tab.interact.find.open {
                    tab.interact.find.open = true;
                    search(tab, false);
                }
                step(tab, !back);
            }
        }
        ("e", false) => {
            let Some(sel) = selection_text(tab) else {
                return true;
            };
            let q = sel.lines().next().unwrap_or("").to_owned();
            let find = &mut tab.interact.find;
            find.query = q;
            find.select_all = true;
            if find.open {
                search(tab, true);
            }
        }
        ("a", false) => select_all(tab),
        ("c", false) => {
            let Some(t) = selection_text(tab) else {
                return false;
            };
            crate::clipboard::set(&t);
        }
        ("p", false) => print(tab),
        ("s", false) => save(tab),
        ("u", false) => tab.request(Request::ViewSource),
        _ => return false,
    }
    true
}

/// Pointer events while the menu or find bar is open, before the page's scripts.
pub fn mouse_capture(tab: &mut Tab, ev: &MouseEvent) -> bool {
    if ev.kind != MouseKind::Move {
        dismiss_notice(tab);
    }
    if let Some(m) = &mut tab.interact.menu {
        match ev.kind {
            MouseKind::Move => {
                m.moved = true;
                if m.menu.hover(ev.x, ev.y) {
                    m.pixels = None;
                    tab.redraw();
                }
            }
            MouseKind::Down => {
                if !hit::contains(m.menu.rect(), ev.x, ev.y) {
                    close_menu(tab);
                }
            }
            MouseKind::Up => {
                let item = m
                    .menu
                    .item_at(ev.x, ev.y)
                    .filter(|&i| m.menu.items[i].enabled);
                if let Some(i) = item
                    && (m.moved || ev.kind == MouseKind::Up && ev.clicks > 0)
                {
                    let action = m.actions[i].clone();
                    close_menu(tab);
                    run_action(tab, action);
                }
            }
        }
        return true;
    }
    let find = &tab.interact.find;
    if find.open {
        let bar = find_bar_rect(tab);
        if hit::contains(bar, ev.x, ev.y) {
            if ev.kind == MouseKind::Down && ev.button == MouseButton::Left {
                let (x, y) = (ev.x - bar.x, ev.y - bar.y);
                match find.buttons.iter().position(|r| hit::contains(*r, x, y)) {
                    Some(0) => step(tab, false),
                    Some(1) => step(tab, true),
                    Some(2) => close_find(tab),
                    _ => {
                        let find = &mut tab.interact.find;
                        find.focused = true;
                        find.select_all = ev.clicks >= 2;
                        find.pixels = None;
                        tab.redraw();
                    }
                }
            }
            if ev.kind == MouseKind::Move {
                tab.cursor = Some(CursorKind::Default);
            }
            return true;
        }
        if ev.kind == MouseKind::Down && find.focused {
            tab.interact.find.focused = false;
            tab.interact.find.pixels = None;
            tab.redraw();
        }
    }
    false
}

/// Pointer events after the page's scripts: selection by drag and
/// multi-click, the context menu.
pub fn mouse(tab: &mut Tab, ev: &MouseEvent) -> bool {
    let right = ev.button == MouseButton::Right || (ev.button == MouseButton::Left && ev.mods.ctrl);
    match ev.kind {
        MouseKind::Down if right => {
            select_word_for_menu(tab, ev.x, ev.y);
            open_context_menu(tab, ev.x, ev.y);
            true
        }
        MouseKind::Down if ev.button == MouseButton::Left => {
            let Some(pos) = pos_at(tab, ev.x, ev.y) else {
                set_selection(tab, None);
                return false;
            };
            let text = text_of(tab);
            let (anchor, granularity) = match ev.clicks {
                2 => (text::word_at(text, pos), Granularity::Word),
                n if n >= 3 => (text::paragraph_at(text, pos), Granularity::Paragraph),
                _ => (pos..pos, Granularity::Char),
            };
            let extend = ev.mods.shift && ev.clicks == 1;
            let anchor = match (&tab.interact.selection, extend) {
                (Some(s), true) => s.start..s.start,
                _ => anchor,
            };
            tab.interact.drag = Some(Drag {
                anchor: anchor.clone(),
                granularity,
                start: (ev.x, ev.y),
                moved: false,
            });
            if extend {
                drag_to(tab, ev.x, ev.y);
            } else {
                set_selection(tab, Some(anchor));
            }
            // A link still opens on release unless the press becomes a drag.
            false
        }
        MouseKind::Move => {
            let Some(d) = &mut tab.interact.drag else {
                update_cursor(tab, ev.x, ev.y);
                return false;
            };
            if !ev.pressed {
                tab.interact.drag = None;
                return false;
            }
            if !d.moved && (ev.x - d.start.0).abs().max((ev.y - d.start.1).abs()) < DRAG_SLOP {
                return true;
            }
            d.moved = true;
            autoscroll(tab, ev.y);
            drag_to(tab, ev.x, ev.y);
            true
        }
        MouseKind::Up => {
            let moved = tab.interact.drag.take().is_some_and(|d| d.moved);
            // A drag that started on a link selects; it does not open the link.
            moved
        }
        MouseKind::Down => false,
    }
}

/// Draw over the page: find matches, then the selection.
pub fn draw_page(tab: &Tab, c: &mut Canvas) {
    let (_, page_h) = tab.page_size();
    let viewport = Rect {
        x: 0.0,
        y: tab.scroll,
        width: f32::MAX,
        height: page_h,
    };
    let find = &tab.interact.find;
    if find.open {
        for (i, rects) in find.rects.iter().enumerate() {
            let color = if find.current == Some(i) {
                ACTIVE_MATCH
            } else {
                MATCH
            };
            for r in rects.iter().filter(|r| r.intersects(&viewport)) {
                c.multiply(tab.doc_to_window(*r), color);
            }
        }
    }
    for r in tab
        .interact
        .selection_rects
        .iter()
        .filter(|r| r.intersects(&viewport))
    {
        c.multiply(tab.doc_to_window(*r), SELECTION);
    }
}

/// Draw over the window: the find bar, the context menu and the notice bubble.
pub fn draw_window(tab: &mut Tab, c: &mut Canvas) {
    let scale = c.scale;
    let win_w = c.width as f32 / scale;
    let win_h = c.height as f32 / scale;
    if tab.interact.find.open {
        if tab.interact.find.pixels.is_none() {
            render_find_bar(tab, scale);
        }
        let bar = find_bar_rect(tab);
        if let Some(px) = &tab.interact.find.pixels {
            c.blit(px, bar.x, bar.y);
        }
    }
    if let Some(n) = &mut tab.interact.notice {
        let px = n
            .pixels
            .get_or_insert_with(|| render_notice(&n.text, win_w, scale));
        let (w, h) = (px.width as f32 / scale, px.height as f32 / scale);
        c.blit(px, win_w - w - 12.0, win_h - h - 12.0);
    }
    let private = tab.private;
    if let Some(m) = &mut tab.interact.menu {
        let r = m.menu.rect();
        let px = m
            .pixels
            .get_or_insert_with(|| render(&m.menu.html(private), r.width, r.height, &[], scale).0);
        c.blit(px, r.x, r.y);
    }
}

/// A new page is on screen: drop the selection and the menu; an open find
/// bar searches the new page.
pub fn on_load(tab: &mut Tab) {
    let key = text_key(tab);
    let clips = tab
        .view
        .as_ref()
        .map_or_else(Vec::new, |v| text::run_clips(&v.tree));
    let s = &mut tab.interact;
    s.clips = clips;
    s.selection = None;
    s.selection_rects.clear();
    s.drag = None;
    s.menu = None;
    s.text_key = key;
    if tab.interact.find.open {
        search(tab, false);
    }
}

/// The page was laid out again (resize, zoom, DOM change): recompute
/// rectangles; offsets stay valid only if the text is the same.
pub fn on_relayout(tab: &mut Tab) {
    let key = text_key(tab);
    tab.interact.clips = tab
        .view
        .as_ref()
        .map_or_else(Vec::new, |v| text::run_clips(&v.tree));
    if key != tab.interact.text_key {
        tab.interact.text_key = key;
        tab.interact.selection = None;
        tab.interact.drag = None;
    }
    let sel = tab.interact.selection.clone();
    set_selection(tab, sel);
    if tab.interact.find.open {
        let current = tab.interact.find.current;
        search(tab, false);
        if let Some(c) = current
            && c < tab.interact.find.matches.len()
        {
            tab.interact.find.current = Some(c);
        }
    }
}

/// The answer of a job sent with `Tab::run`.
pub fn on_engine(tab: &mut Tab, ev: Box<dyn Any + Send>) {
    let Ok(r) = ev.downcast::<JobResult>() else {
        return;
    };
    let text = match *r {
        JobResult::Printed { path, pages } => {
            format!(
                "Saved {} ({pages} page{}) to {}",
                file_name(&path),
                if pages == 1 { "" } else { "s" },
                dir_name(&path)
            )
        }
        JobResult::Saved { path } => format!("Saved {} to {}", file_name(&path), dir_name(&path)),
        JobResult::Failed(e) => format!("Failed: {e}"),
    };
    tab.interact.notice = Some(Notice { text, pixels: None });
    tab.redraw();
}

/// Fields for the headless `state` line.
pub fn state(tab: &Tab, out: &mut serde_json::Map<String, serde_json::Value>) {
    use serde_json::{Value, json};
    let s = &tab.interact;
    out.insert(
        "selection".into(),
        selection_text(tab).map_or(Value::Null, Value::String),
    );
    let f = &s.find;
    let current_rect = f
        .current
        .and_then(|i| f.rects.get(i))
        .and_then(|r| r.first())
        .map(|r| tab.doc_to_window(*r))
        .map(|r| json!([r.x, r.y, r.width, r.height]));
    out.insert(
        "find".into(),
        json!({
            "open": f.open,
            "focused": f.focused,
            "query": f.query,
            "count": f.matches.len(),
            "current": f.current.map(|c| c + 1),
            "rect": current_rect,
        }),
    );
    out.insert(
        "menu".into(),
        s.menu.as_ref().map_or(Value::Null, |m| {
            let r = m.menu.rect();
            json!({
                "items": m.menu.items.iter().map(|i| if i.enabled { i.label.clone() } else { format!("({})", i.label) }).collect::<Vec<_>>(),
                "rect": [r.x, r.y, r.width, r.height],
                "hovered": m.menu.hovered,
            })
        }),
    );
    out.insert(
        "notice".into(),
        s.notice
            .as_ref()
            .map_or(Value::Null, |n| Value::String(n.text.clone())),
    );
}

// ----- selection -------------------------------------------------------------

fn text_of(tab: &Tab) -> &str {
    tab.view
        .as_ref()
        .and_then(|v| v.tree.text_index.as_ref())
        .map_or("", |i| i.text.as_str())
}

fn text_key(tab: &Tab) -> (usize, u64) {
    use std::hash::{Hash, Hasher};
    let t = text_of(tab);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    t.hash(&mut h);
    (t.len(), h.finish())
}

/// Text offset at window point (`x`, `y`).
fn pos_at(tab: &Tab, x: f32, y: f32) -> Option<u32> {
    let view = tab.view.as_ref()?;
    let (dx, dy) = tab.to_doc(x, y);
    text::position_at(&view.tree, dx, dy)
}

fn set_selection(tab: &mut Tab, sel: Option<Range<u32>>) {
    let sel = sel.filter(|r| r.start < r.end);
    let rects = match (&sel, &tab.view) {
        (Some(r), Some(v)) => text::range_rects(&v.tree, &tab.interact.clips, r.clone()),
        _ => Vec::new(),
    };
    if sel != tab.interact.selection || rects != tab.interact.selection_rects {
        tab.interact.selection = sel;
        tab.interact.selection_rects = rects;
        tab.redraw();
    }
}

fn drag_to(tab: &mut Tab, x: f32, y: f32) {
    let Some(pos) = pos_at(tab, x, y) else { return };
    let Some(d) = &tab.interact.drag else { return };
    let (anchor, granularity) = (d.anchor.clone(), d.granularity);
    let text = text_of(tab);
    let unit = match granularity {
        Granularity::Char => pos..pos,
        Granularity::Word => text::word_at(text, pos),
        Granularity::Paragraph => text::paragraph_at(text, pos),
    };
    let sel = anchor.start.min(unit.start)..anchor.end.max(unit.end);
    set_selection(tab, Some(sel));
}

/// While dragging above or below the page area, scroll toward the pointer.
fn autoscroll(tab: &mut Tab, y: f32) {
    let bottom = PAGE_TOP + tab.page_size().1 * tab.zoom;
    if y < PAGE_TOP {
        tab.scroll_by((y - PAGE_TOP) / tab.zoom);
    } else if y > bottom {
        tab.scroll_by((y - bottom) / tab.zoom);
    }
}

/// A right click on a word (not a link, not inside the selection) selects
/// it first, as on macOS, so the menu offers Copy and Search.
fn select_word_for_menu(tab: &mut Tab, x: f32, y: f32) {
    if tab.link_at(x, y).is_some() {
        return;
    }
    let on_text = tab.view.as_ref().is_some_and(|v| {
        let (dx, dy) = tab.to_doc(x, y);
        text::over_text(&v.tree, dx, dy)
    });
    let Some(pos) = pos_at(tab, x, y).filter(|_| on_text) else {
        return;
    };
    if tab
        .interact
        .selection
        .as_ref()
        .is_some_and(|s| s.start <= pos && pos < s.end)
    {
        return;
    }
    let word = text::word_at(text_of(tab), pos);
    if text_of(tab)[word.start as usize..word.end as usize]
        .chars()
        .any(char::is_alphanumeric)
    {
        set_selection(tab, Some(word));
    }
}

fn select_all(tab: &mut Tab) {
    let len = text_of(tab).len() as u32;
    set_selection(tab, Some(0..len));
}

fn selection_text(tab: &Tab) -> Option<String> {
    let r = tab.interact.selection.clone()?;
    let index = tab.view.as_ref()?.tree.text_index.as_ref()?;
    Some(text::range_text(index, r))
}

/// I-beam over text, like Chrome; links and the rest keep the tab's cursor.
fn update_cursor(tab: &mut Tab, x: f32, y: f32) {
    let over_text = tab.link_at(x, y).is_none()
        && tab.view.as_ref().is_some_and(|v| {
            let (dx, dy) = tab.to_doc(x, y);
            text::over_text(&v.tree, dx, dy)
        });
    let want = over_text.then_some(CursorKind::Text);
    if tab.cursor != want {
        tab.cursor = want;
    }
}

// ----- find ------------------------------------------------------------------

fn open_find(tab: &mut Tab) {
    let find = &mut tab.interact.find;
    find.open = true;
    find.focused = true;
    find.select_all = true;
    find.pixels = None;
    if !find.query.is_empty() {
        search(tab, false);
    }
    tab.redraw();
}

fn close_find(tab: &mut Tab) {
    let find = &mut tab.interact.find;
    // Like Chrome: the active match becomes the selection.
    let active = find.current.and_then(|i| find.matches.get(i).cloned());
    find.open = false;
    find.focused = false;
    find.matches.clear();
    find.rects.clear();
    find.current = None;
    find.pixels = None;
    if active.is_some() {
        set_selection(tab, active);
    }
    tab.redraw();
}

fn find_insert(tab: &mut Tab, s: &str) {
    let find = &mut tab.interact.find;
    if find.select_all {
        find.query.clear();
        find.select_all = false;
    }
    find.query.push_str(s);
    search(tab, true);
}

/// Search the page for the query. The active match is the first one at or
/// below the top of the viewport; `reveal` scrolls it into view.
fn search(tab: &mut Tab, reveal: bool) {
    let (matches, rects) = match &tab.view {
        Some(v) if !tab.interact.find.query.is_empty() => {
            let text = v.tree.text_index.as_ref().map_or("", |i| i.text.as_str());
            let clips = &tab.interact.clips;
            // Matches in clipped-away text (screen-reader-only labels) do not count.
            text::find_all(text, &tab.interact.find.query, MAX_MATCHES)
                .into_iter()
                .map(|m| {
                    let r = text::range_rects(&v.tree, clips, m.clone());
                    (m, r)
                })
                .filter(|(_, r)| !r.is_empty())
                .unzip()
        }
        _ => (Vec::new(), Vec::new()),
    };
    let top = tab.scroll;
    let current = rects
        .iter()
        .position(|r| r.first().is_some_and(|r| r.y >= top))
        .or((!rects.is_empty()).then_some(0));
    let find = &mut tab.interact.find;
    find.matches = matches;
    find.rects = rects;
    find.current = current;
    find.pixels = None;
    if reveal {
        reveal_current(tab);
    }
    tab.redraw();
}

/// Next (or previous) match, wrapping.
fn step(tab: &mut Tab, forward: bool) {
    let find = &mut tab.interact.find;
    let n = find.matches.len();
    if n == 0 {
        return;
    }
    find.current = Some(match (find.current, forward) {
        (None, true) => 0,
        (None, false) => n - 1,
        (Some(i), true) => (i + 1) % n,
        (Some(i), false) => (i + n - 1) % n,
    });
    find.pixels = None;
    reveal_current(tab);
    tab.redraw();
}

/// Scroll the active match to the middle of the page area if it is not visible.
fn reveal_current(tab: &mut Tab) {
    let find = &tab.interact.find;
    let Some(r) = find
        .current
        .and_then(|i| find.rects.get(i))
        .and_then(|r| r.first())
        .copied()
    else {
        return;
    };
    let (_, page_h) = tab.page_size();
    // The find bar covers the top of the page.
    let top = tab.scroll + FIND_H / tab.zoom;
    if r.y < top || r.bottom() > tab.scroll + page_h {
        tab.set_scroll(r.y - (page_h - r.height) / 2.0);
    }
}

/// The find bar, window CSS px: under the toolbar, at the right.
fn find_bar_rect(tab: &Tab) -> Rect {
    let win_w = tab.page_size().0 * tab.zoom;
    Rect {
        x: (win_w - FIND_W - 16.0).max(0.0),
        y: PAGE_TOP,
        width: FIND_W.min(win_w),
        height: FIND_H,
    }
}

fn render_find_bar(tab: &mut Tab, scale: f32) {
    let accent = crate::theme::accent(tab.private);
    let selection = crate::theme::selection(tab.private);
    let find = &tab.interact.find;
    let count = if find.query.is_empty() {
        String::new()
    } else {
        format!(
            "{}/{}",
            find.current.map_or(0, |c| c + 1),
            find.matches.len()
        )
    };
    let none = !find.query.is_empty() && find.matches.is_empty();
    let caret = if find.focused && !find.select_all {
        format!("<div style=\"width:1.5px;height:16px;background:{accent};flex:none\"></div>")
    } else {
        String::new()
    };
    // Chevrons and the cross are drawn with CSS: no font has all three alike.
    let button = |id: &str, inner: &str| {
        format!(
            "<div id={id} style=\"width:28px;height:28px;flex:none;display:flex;align-items:center;justify-content:center\">\
             {inner}</div>"
        )
    };
    let (muted, line) = (crate::theme::MUTED, crate::theme::HAIRLINE);
    let has = !find.matches.is_empty();
    let color = if has { muted } else { crate::theme::DIM };
    let chevron = |deg: i32, dy: i32| {
        format!(
            "<div style=\"width:7px;height:7px;border-top:1.5px solid {color};border-left:1.5px solid {color};\
             transform:translateY({dy}px) rotate({deg}deg)\"></div>"
        )
    };
    let bar = |deg: i32| {
        format!(
            "<div style=\"position:absolute;left:0;top:6px;width:14px;height:1.5px;background:{muted};transform:rotate({deg}deg)\"></div>"
        )
    };
    let cross = format!(
        "<div style=\"position:relative;width:14px;height:14px\">{}{}</div>",
        bar(45),
        bar(-45)
    );
    let html = format!(
        "<!doctype html><html><body style=\"margin:0;font:13px {};color:{};background:{}\">\
         <div style=\"box-sizing:border-box;width:{FIND_W}px;height:{FIND_H}px;background:{};\
         border:1px solid {line};border-top:0;display:flex;align-items:center;padding:0 6px 0 12px\">\
         <div id=field style=\"flex:1;min-width:0;height:28px;display:flex;align-items:center;overflow:hidden;white-space:pre;\
         box-sizing:border-box;padding:0 8px;border:1px solid {};background:{}\"><div style=\"flex:none;{}\">{}</div>{caret}</div>\
         <div style=\"flex:none;color:{};padding:0 10px;font-size:10px;{}\">{}</div>\
         <div style=\"flex:none;width:1px;height:24px;background:{line};margin-right:4px\"></div>{}{}{}</div></body></html>",
        crate::theme::FONT,
        crate::theme::TEXT,
        crate::theme::TOOLBAR,
        crate::theme::TOOLBAR,
        if find.focused { accent } else { line },
        if none { "#2a1412" } else { crate::theme::STRIP },
        if find.focused && find.select_all {
            format!("background:{selection}")
        } else {
            String::new()
        },
        escape_html(&find.query),
        if none { crate::theme::WARN } else { muted },
        crate::theme::LABEL,
        count,
        button("prev", &chevron(45, 2)),
        button("next", &chevron(225, -2)),
        button("close", &cross),
    );
    let (px, rects) = render(&html, FIND_W, FIND_H, &["prev", "next", "close"], scale);
    let find = &mut tab.interact.find;
    find.pixels = Some(px);
    find.buttons = [rects[0], rects[1], rects[2]];
}

// ----- context menu ----------------------------------------------------------

fn open_context_menu(tab: &mut Tab, x: f32, y: f32) {
    let link = tab.link_at(x, y).cloned();
    let selected = selection_text(tab).filter(|s| !s.trim().is_empty());
    let mut items = Vec::new();
    let mut actions = Vec::new();
    let mut add = |item: MenuItem, action: Action| {
        items.push(item);
        actions.push(action);
    };
    if let Some(url) = &link {
        let page = Opener {
            initiator: tab.initiator(),
            ..Opener::default()
        };
        add(
            MenuItem::new("Open Link in New Tab"),
            Action::Request(Request::OpenTab {
                url: url.clone(),
                background: true,
                opener: Box::new(page.clone()),
            }),
        );
        // Another profile in a private window: nothing of this page goes along.
        let (normal, private) = if tab.private {
            (Opener::default(), page)
        } else {
            (page, Opener::default())
        };
        add(
            MenuItem::new("Open Link in New Window"),
            Action::Request(Request::OpenWindow {
                url: url.clone(),
                private: false,
                opener: Box::new(normal),
            }),
        );
        add(
            MenuItem::new("Open Link in Private Window"),
            Action::Request(Request::OpenWindow {
                url: url.clone(),
                private: true,
                opener: Box::new(private),
            }),
        );
        add(
            MenuItem::new("Copy Link Address").after_separator(),
            Action::CopyText(url.to_string()),
        );
    }
    if let Some(s) = &selected {
        let short: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
        let short = if short.chars().count() > 30 {
            format!("{}\u{2026}", short.chars().take(30).collect::<String>())
        } else {
            short
        };
        add(
            MenuItem::new("Copy")
                .after_separator()
                .shortcut("\u{2318}C"),
            Action::Copy,
        );
        if let Some(url) = search_url(s) {
            add(
                MenuItem::new(format!("Search DuckDuckGo for \u{201c}{short}\u{201d}")),
                Action::Request(Request::OpenTab {
                    url,
                    background: false,
                    opener: Box::default(),
                }),
            );
        }
    }
    // Like Chrome: a link or a selection gets its own items; the page items
    // (history, save, print, source) are for a click on the page itself.
    if link.is_none() && selected.is_none() {
        add(
            MenuItem::new("Back")
                .enabled(tab.can_back())
                .shortcut("\u{2318}["),
            Action::Request(Request::Back),
        );
        add(
            MenuItem::new("Forward")
                .enabled(tab.can_forward())
                .shortcut("\u{2318}]"),
            Action::Request(Request::Forward),
        );
        add(
            MenuItem::new("Reload").shortcut("\u{2318}R"),
            Action::Request(Request::Reload),
        );
        add(
            MenuItem::new("Save As\u{2026}")
                .after_separator()
                .shortcut("\u{2318}S"),
            Action::Save,
        );
        add(
            MenuItem::new("Print\u{2026}").shortcut("\u{2318}P"),
            Action::Print,
        );
        add(
            MenuItem::new("Select All")
                .after_separator()
                .shortcut("\u{2318}A"),
            Action::SelectAll,
        );
        add(
            MenuItem::new("View Page Source")
                .after_separator()
                .shortcut("\u{2318}U"),
            Action::Request(Request::ViewSource),
        );
    } else if link.is_none() {
        add(
            MenuItem::new("Select All")
                .after_separator()
                .shortcut("\u{2318}A"),
            Action::SelectAll,
        );
        add(
            MenuItem::new("Print\u{2026}").shortcut("\u{2318}P"),
            Action::Print,
        );
    }
    let (w, h) = tab.page_size();
    let win = (w * tab.zoom, PAGE_TOP + h * tab.zoom);
    tab.interact.menu = Some(ContextMenu {
        menu: Menu::new(items, x, y, win.0, win.1),
        actions,
        pixels: None,
        moved: false,
    });
    tab.interact.drag = None;
    tab.redraw();
}

fn close_menu(tab: &mut Tab) {
    if tab.interact.menu.take().is_some() {
        tab.redraw();
    }
}

fn run_action(tab: &mut Tab, action: Action) {
    match action {
        Action::Request(r) => tab.request(r),
        Action::Copy => {
            if let Some(t) = selection_text(tab) {
                crate::clipboard::set(&t);
            }
        }
        Action::CopyText(t) => crate::clipboard::set(&t),
        Action::SelectAll => select_all(tab),
        Action::Print => print(tab),
        Action::Save => save(tab),
    }
}

/// DuckDuckGo's HTML search for `q`, with no other parameters.
pub fn search_url(q: &str) -> Option<Url> {
    let mut u = Url::parse("https://duckduckgo.com/html/").ok()?;
    u.query_pairs_mut().append_pair("q", q.trim());
    Some(u)
}

// ----- print and save --------------------------------------------------------

fn print(tab: &mut Tab) {
    let title = tab.display_title();
    tab.interact.notice = Some(Notice {
        text: "Printing\u{2026}".into(),
        pixels: None,
    });
    tab.redraw();
    tab.run(Box::new(move |page, layout, viewport| {
        let (pdf, pages) = print::print_pdf(page, layout, viewport, &title);
        let dir = print::downloads_dir();
        let host = page.url.host_str().unwrap_or("page").to_owned();
        let path = print::unique_path(&dir, &print::file_stem(&title, &host), "pdf");
        let result = std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(&path, pdf))
            .map_or_else(
                |e| JobResult::Failed(format!("{}: {e}", path.display())),
                |()| JobResult::Printed {
                    path: path.display().to_string(),
                    pages,
                },
            );
        Some(EngineEvent::Interact(Box::new(result)))
    }));
}

fn save(tab: &mut Tab) {
    let Some(url) = tab.url().cloned() else {
        return;
    };
    let title = tab.display_title();
    tab.run(Box::new(move |_, _, _| {
        let result = match source::save_page(&url, &title) {
            Ok(p) => JobResult::Saved {
                path: p.display().to_string(),
            },
            Err(e) => JobResult::Failed(e),
        };
        Some(EngineEvent::Interact(Box::new(result)))
    }));
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |f| f.to_string_lossy().into_owned())
}

fn dir_name(path: &str) -> String {
    std::path::Path::new(path)
        .parent()
        .and_then(|p| p.file_name())
        .map_or_else(String::new, |f| f.to_string_lossy().into_owned())
}

fn dismiss_notice(tab: &mut Tab) {
    // "Printing..." stays until the job answers.
    if tab
        .interact
        .notice
        .as_ref()
        .is_some_and(|n| !n.text.ends_with('\u{2026}'))
    {
        tab.interact.notice = None;
        tab.redraw();
    }
}

fn render_notice(text: &str, win_w: f32, scale: f32) -> Pixels {
    let w = (text.chars().count() as f32 * 7.0 + 32.0)
        .min(win_w - 24.0)
        .max(60.0);
    let html = format!(
        "<!doctype html><html><body style=\"margin:0;font:12px {}\">\
         <div style=\"box-sizing:border-box;width:{w}px;height:36px;padding:0 14px;display:flex;align-items:center;\
         background:{};color:{};border:1px solid {};white-space:pre;overflow:hidden\">{}</div></body></html>",
        crate::theme::FONT,
        crate::theme::TOOLBAR,
        crate::theme::TEXT,
        crate::theme::HAIRLINE,
        escape_html(text)
    );
    render(&html, w, 36.0, &[], scale).0
}
