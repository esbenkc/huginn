//! Live JavaScript after load, UI-thread half: routes
//! page input to the tab's engine thread, draws the text-field caret and the
//! `<select>` drop-down, and keeps what the engine reports about focus. The
//! engine-thread half (events, default actions, timers, relayout) is
//! `livepage.rs`. See CONTRACTS.md, "huginn-shell window: tab hooks".

use std::cell::RefCell;

use huginn_dom::NodeId;
use huginn_layout::{LayoutEngine, LayoutTree, Rect};
use huginn_net::ImageStore;
use huginn_paint::CpuRasterizer;

use crate::clipboard;
use crate::engine::{Current, EngineEvent};
use crate::input::{CursorKind, Key, KeyEvent, MouseButton, MouseEvent, MouseKind};
use crate::interact::hit::{contains, node_at};
use crate::interact::menu::{Menu, MenuItem, MenuKey};
pub use crate::livepage::{EditCommand, KeyInput, LiveEvent, MouseInput, PageInput, Status};
use crate::tab::{PAGE_TOP, Tab};
use crate::ui::{Canvas, Pixels, render_snippet};

/// Caret color (Chrome's default `caret-color` on a white field: black).
const CARET: u32 = 0x000000;
/// Selected text in a field (macOS highlight blue).
const FIELD_SELECTION: u32 = 0xB4D7FF;

/// Per-tab UI-side state, at `tab.live`.
#[derive(Default)]
pub struct TabState {
    /// Focus and caret, as last reported by the engine.
    status: Status,
    select: Option<SelectPopup>,
    /// The current press started on a link: its release is ours (the engine
    /// follows the link after the page's click handlers).
    link_press: bool,
}

/// An open `<select>` drop-down.
struct SelectPopup {
    node: NodeId,
    menu: Menu,
    pixels: Option<Pixels>,
    /// The pointer moved since the popup opened (a release then picks).
    moved: bool,
}

/// Engine thread: dispatch `event` on the current page. `visible`: the tab
/// is the active one in its window.
pub(crate) fn input(
    cur: &mut Current,
    event: PageInput,
    _visible: bool,
    notify: &dyn Fn(EngineEvent),
) {
    cur.live.input(
        &mut cur.page,
        &mut cur.layout,
        cur.viewport,
        cur.generation,
        event,
        notify,
    );
}

/// Pointer moves are coalesced: only the last one of a batch runs.
pub(crate) fn is_pointer_move(event: &PageInput) -> bool {
    matches!(event, PageInput::Mouse(m) if m.kind == MouseKind::Move)
}

/// A text field of the page has the focus (Cmd+A/C/X/V/Z edit it).
pub fn text_control_focused(tab: &Tab) -> bool {
    tab.live.status.field.is_some()
}

/// Keys after the find bar and menus, before selection and scrolling.
pub fn key(tab: &mut Tab, ev: &KeyEvent) -> bool {
    if tab.view.is_none() {
        return false;
    }
    if let Some(p) = &mut tab.live.select {
        match p.menu.key(&ev.key) {
            MenuKey::Activate(i) => {
                let node = p.node;
                close_select(tab);
                select_picked(tab, node, i);
            }
            MenuKey::Close => close_select(tab),
            MenuKey::None => {
                p.pixels = None;
                tab.redraw();
            }
        }
        return true;
    }
    let field = text_control_focused(tab);
    let m = ev.mods;
    if m.cmd && !m.ctrl {
        if !field {
            return false;
        }
        let cmd = match &ev.key {
            k if k.is("a") => Some(EditCommand::SelectAll),
            k if k.is("c") => Some(EditCommand::Copy),
            k if k.is("x") => Some(EditCommand::Cut),
            k if k.is("v") => Some(EditCommand::Paste(clipboard::get().unwrap_or_default())),
            k if k.is("z") && m.shift => Some(EditCommand::Redo),
            k if k.is("z") => Some(EditCommand::Undo),
            _ => None,
        };
        if let Some(cmd) = cmd {
            tab.input(PageInput::Edit(cmd));
            return true;
        }
        // Cmd+arrows and Cmd+Backspace move and delete by line in the field.
        if !matches!(
            ev.key,
            Key::Left | Key::Right | Key::Up | Key::Down | Key::Backspace | Key::Delete
        ) {
            return false;
        }
    }
    let Some(k) = key_input(ev) else { return false };
    tab.input(PageInput::Key(k));
    // The field takes every key; Tab moves focus; Space and Enter act on a
    // focused button, link or checkbox instead of scrolling.
    field
        || ev.key == Key::Tab
        || (tab.live.status.activatable && matches!(ev.key, Key::Space | Key::Enter))
}

/// Every pointer event in the page. True: the live page takes the default
/// action (a click on a link is followed by the engine after the page's
/// click handlers ran, so they can cancel it).
pub fn mouse(tab: &mut Tab, ev: &MouseEvent) -> bool {
    if tab.live.select.is_some() {
        return select_mouse(tab, ev);
    }
    let Some(view) = &tab.view else { return false };
    let (x, y) = tab.to_doc(ev.x, ev.y);
    let target = node_at(&view.tree, x, y);
    let field = tab.live.status.field.as_ref().map(|f| f.node);
    // Caret position for a press in a field (or a drag in the focused one).
    let text_pos = match (ev.kind, target) {
        (MouseKind::Down, Some(t)) => text_index_at(&view.tree, t, x, y),
        (MouseKind::Move, Some(t)) if ev.pressed && Some(t) == field => {
            text_index_at(&view.tree, t, x, y)
        }
        _ => None,
    };
    // Pages without scripts: the tab follows links itself, at once.
    let on_link = tab.live.status.scripted && tab.link_at(ev.x, ev.y).is_some();
    tab.input(PageInput::Mouse(MouseInput {
        kind: ev.kind,
        button: ev.button,
        pressed: ev.pressed,
        clicks: ev.clicks,
        x,
        y,
        client_x: x,
        client_y: y - tab.scroll,
        mods: ev.mods,
        target,
        text_pos,
    }));
    if ev.kind == MouseKind::Move && field.is_some() && target == field {
        tab.cursor = Some(CursorKind::Text);
    }
    match ev.kind {
        MouseKind::Down => {
            tab.live.link_press = on_link && ev.button != MouseButton::Right;
            // A press in the focused field moves its caret, not the page selection.
            text_pos.is_some() && target == field
        }
        MouseKind::Up => {
            std::mem::take(&mut tab.live.link_press) || on_link && ev.button != MouseButton::Right
        }
        MouseKind::Move => false,
    }
}

/// The pointer left the page area.
pub fn mouse_leave(tab: &mut Tab) {
    tab.input(PageInput::Leave);
}

/// Draw over the page: the caret and the selection of the focused field.
pub fn draw_page(tab: &Tab, c: &mut Canvas) {
    let (Some(f), Some(view)) = (&tab.live.status.field, &tab.view) else {
        return;
    };
    let tree = &view.tree;
    let (a, b) = (f.caret.min(f.anchor), f.caret.max(f.anchor));
    if a != b {
        for r in field_range_rects(tree, f.node, a, b) {
            c.multiply(tab.doc_to_window(r), FIELD_SELECTION);
        }
    } else if let Some(r) = caret_rect(tree, f.node, f.caret) {
        let mut w = tab.doc_to_window(r);
        w.width = 1.0;
        c.fill(w, CARET);
    }
}

/// Draw over the window: the `<select>` drop-down.
pub fn draw_window(tab: &mut Tab, c: &mut Canvas) {
    let scale = c.scale;
    let private = tab.private;
    if let Some(p) = &mut tab.live.select {
        let r = p.menu.rect();
        let px = p
            .pixels
            .get_or_insert_with(|| render(&p.menu.html(private), r.width, r.height, scale));
        c.blit(px, r.x, r.y);
    }
}

/// A new page is on screen.
pub fn on_load(tab: &mut Tab) {
    tab.live = TabState::default();
}

pub fn on_engine(tab: &mut Tab, ev: LiveEvent) {
    match ev {
        LiveEvent::Status(s) => {
            if s.generation == tab.generation && s != tab.live.status {
                tab.live.status = s;
                tab.redraw();
            }
        }
        LiveEvent::Copy(text) => clipboard::set(&text),
        LiveEvent::OpenSelect {
            generation,
            node,
            items,
            selected,
        } => {
            if generation != tab.generation {
                return;
            }
            let Some(view) = &tab.view else { return };
            let Some(b) = view.tree.box_for_node(node) else {
                return;
            };
            let r = tab.doc_to_window(view.tree.get(b).rect);
            let (pw, ph) = tab.page_size();
            let (win_w, win_h) = (pw * tab.zoom, ph * tab.zoom + PAGE_TOP);
            let items = items
                .into_iter()
                .enumerate()
                .map(|(i, it)| {
                    let mut m = MenuItem::new(it.label).enabled(!it.disabled);
                    m.checked = Some(i) == selected;
                    m
                })
                .collect();
            let mut menu = Menu::new(items, r.x, r.bottom(), win_w, win_h);
            menu.width = menu.width.max(r.width);
            menu.hovered = selected;
            tab.live.select = Some(SelectPopup {
                node,
                menu,
                pixels: None,
                moved: false,
            });
            tab.redraw();
        }
    }
}

/// An option picked in a `<select>` drop-down.
pub fn select_picked(tab: &mut Tab, node: NodeId, index: usize) {
    tab.input(PageInput::SelectOption { node, index });
}

/// Fields for the headless `state` line: `focus` (node id), `field`
/// (caret and anchor), `isolate`, `select_open`.
pub fn state(tab: &Tab, out: &mut serde_json::Map<String, serde_json::Value>) {
    let s = &tab.live.status;
    out.insert("focus".into(), s.focus.map(NodeId::to_raw).into());
    out.insert(
        "field".into(),
        s.field.as_ref().map_or(
            serde_json::Value::Null,
            |f| serde_json::json!({ "caret": f.caret, "anchor": f.anchor }),
        ),
    );
    out.insert("isolate".into(), s.isolate.into());
    out.insert("select_open".into(), tab.live.select.is_some().into());
}

fn close_select(tab: &mut Tab) {
    tab.live.select = None;
    tab.redraw();
}

/// Pointer events while the drop-down is open: it takes them all.
fn select_mouse(tab: &mut Tab, ev: &MouseEvent) -> bool {
    let Some(p) = &mut tab.live.select else {
        return false;
    };
    match ev.kind {
        MouseKind::Move => {
            p.moved = true;
            if p.menu.hover(ev.x, ev.y) {
                p.pixels = None;
                tab.redraw();
            }
        }
        MouseKind::Down => {
            if !contains(p.menu.rect(), ev.x, ev.y) {
                close_select(tab);
            }
        }
        MouseKind::Up => {
            let item = p
                .menu
                .item_at(ev.x, ev.y)
                .filter(|&i| p.menu.items[i].enabled);
            if let Some(i) = item
                && p.moved
            {
                let node = p.node;
                close_select(tab);
                select_picked(tab, node, i);
            }
        }
    }
    true
}

thread_local! {
    /// Layout engine and rasterizer for the drop-down (fonts load once).
    static UI: RefCell<Option<(LayoutEngine, CpuRasterizer, ImageStore)>> = const { RefCell::new(None) };
}

fn render(html: &str, width: f32, height: f32, scale: f32) -> Pixels {
    UI.with(|ui| {
        let mut ui = ui.borrow_mut();
        let (layout, raster, images) = ui
            .get_or_insert_with(|| (LayoutEngine::new(), CpuRasterizer::new(), ImageStore::new()));
        render_snippet(layout, raster, html, width, height, &[], images, scale).0
    })
}

/// A key press as a DOM `KeyboardEvent` (`key`, `code`, legacy `keyCode`).
fn key_input(ev: &KeyEvent) -> Option<KeyInput> {
    let (key, code, key_code): (String, String, u32) = match &ev.key {
        Key::Char(s) => {
            let c = s.chars().next()?;
            let upper = c.to_ascii_uppercase();
            let (code, key_code) = match c {
                'a'..='z' | 'A'..='Z' => (format!("Key{upper}"), upper as u32),
                '0'..='9' => (format!("Digit{c}"), c as u32),
                ';' | ':' => ("Semicolon".into(), 186),
                '=' | '+' => ("Equal".into(), 187),
                ',' | '<' => ("Comma".into(), 188),
                '-' | '_' => ("Minus".into(), 189),
                '.' | '>' => ("Period".into(), 190),
                '/' | '?' => ("Slash".into(), 191),
                '`' | '~' => ("Backquote".into(), 192),
                '[' | '{' => ("BracketLeft".into(), 219),
                '\\' | '|' => ("Backslash".into(), 220),
                ']' | '}' => ("BracketRight".into(), 221),
                '\'' | '"' => ("Quote".into(), 222),
                _ => (String::new(), 0),
            };
            (s.clone(), code, key_code)
        }
        Key::Enter => ("Enter".into(), "Enter".into(), 13),
        Key::Backspace => ("Backspace".into(), "Backspace".into(), 8),
        Key::Delete => ("Delete".into(), "Delete".into(), 46),
        Key::Escape => ("Escape".into(), "Escape".into(), 27),
        Key::Space => (" ".into(), "Space".into(), 32),
        Key::Tab => ("Tab".into(), "Tab".into(), 9),
        Key::Up => ("ArrowUp".into(), "ArrowUp".into(), 38),
        Key::Down => ("ArrowDown".into(), "ArrowDown".into(), 40),
        Key::Left => ("ArrowLeft".into(), "ArrowLeft".into(), 37),
        Key::Right => ("ArrowRight".into(), "ArrowRight".into(), 39),
        Key::PageUp => ("PageUp".into(), "PageUp".into(), 33),
        Key::PageDown => ("PageDown".into(), "PageDown".into(), 34),
        Key::Home => ("Home".into(), "Home".into(), 36),
        Key::End => ("End".into(), "End".into(), 35),
    };
    Some(KeyInput {
        key,
        code,
        key_code,
        mods: ev.mods,
    })
}

/// Text of `node`'s runs in the text index, with the runs (index, byte
/// offset of the run's text from the field's first byte).
fn field_text(tree: &LayoutTree, node: NodeId) -> Option<(&str, Vec<usize>)> {
    let index = tree.text_index.as_ref()?;
    let runs: Vec<usize> = (0..tree.runs.len())
        .filter(|&i| tree.runs[i].node == Some(node) && !index.clusters(i).is_empty())
        .collect();
    let first = index.runs[*runs.first()?].text.start as usize;
    let last = index.runs[*runs.last()?].text.end as usize;
    Some((index.text.get(first..last)?, runs))
}

/// (x, top, bottom, byte) of every cluster start of the field, plus its end.
fn field_stops(tree: &LayoutTree, node: NodeId) -> Option<(String, Vec<(f32, f32, f32, usize)>)> {
    let (text, runs) = field_text(tree, node)?;
    let index = tree.text_index.as_ref()?;
    let base = index.runs[runs[0]].text.start as usize;
    let mut stops = Vec::new();
    for &i in &runs {
        let rt = &index.runs[i];
        for (k, c) in index.clusters(i).iter().enumerate() {
            stops.push((c.x, rt.top, rt.bottom, c.text as usize - base));
            if k + 1 == index.clusters(i).len() {
                stops.push((
                    c.x + c.width,
                    rt.top,
                    rt.bottom,
                    index.cluster_end(i, k) as usize - base,
                ));
            }
        }
    }
    Some((text.to_owned(), stops))
}

fn char_to_byte(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(b, _)| b)
}

/// The caret before char `pos` of the field's value (document px, zero width).
fn caret_rect(tree: &LayoutTree, node: NodeId, pos: usize) -> Option<Rect> {
    let Some((text, stops)) = field_stops(tree, node) else {
        // Empty field: at the start of its content box, a line high.
        let b = tree.get(tree.box_for_node(node)?);
        let content = b.content_rect();
        let h = b.style.line_height_px().min(content.height.max(1.0));
        return Some(Rect {
            x: content.x,
            y: content.y + (content.height - h) / 2.0,
            width: 0.0,
            height: h,
        });
    };
    let byte = char_to_byte(&text, pos);
    // The last stop at or before the caret's byte.
    let &(x, top, bottom, _) = stops.iter().rev().find(|s| s.3 <= byte).or(stops.first())?;
    Some(Rect {
        x,
        y: top,
        width: 0.0,
        height: bottom - top,
    })
}

/// Rectangles of chars `a..b` of the field's value.
fn field_range_rects(tree: &LayoutTree, node: NodeId, a: usize, b: usize) -> Vec<Rect> {
    let Some((text, stops)) = field_stops(tree, node) else {
        return Vec::new();
    };
    let (ba, bb) = (char_to_byte(&text, a), char_to_byte(&text, b));
    let mut out: Vec<Rect> = Vec::new();
    for w in stops.windows(2) {
        let (s, e) = (w[0], w[1]);
        if s.3 >= ba && s.3 < bb && e.1 == s.1 && e.0 > s.0 {
            match out.last_mut() {
                Some(r) if r.y == s.1 && (r.right() - s.0).abs() < 0.5 => r.width = e.0 - r.x,
                _ => out.push(Rect {
                    x: s.0,
                    y: s.1,
                    width: e.0 - s.0,
                    height: s.2 - s.1,
                }),
            }
        }
    }
    out
}

/// Char index in `node`'s text nearest to document point (`x`, `y`), if
/// `node` shows text (an input's value or a textarea's text).
fn text_index_at(tree: &LayoutTree, node: NodeId, x: f32, y: f32) -> Option<usize> {
    let Some((text, stops)) = field_stops(tree, node) else {
        // An empty text field still takes the caret (at 0).
        return tree
            .box_for_node(node)
            .filter(|_| tree.runs.iter().all(|r| r.node != Some(node)))
            .map(|_| 0);
    };
    let line = stops
        .iter()
        .filter(|s| y >= s.1 && y < s.2)
        .map(|s| s.1)
        .next()
        .or_else(|| {
            stops
                .iter()
                .min_by(|a, b| (a.1 - y).abs().total_cmp(&(b.1 - y).abs()))
                .map(|s| s.1)
        })?;
    let best = stops
        .iter()
        .filter(|s| s.1 == line)
        .min_by(|a, b| (a.0 - x).abs().total_cmp(&(b.0 - x).abs()))?;
    Some(text[..best.3.min(text.len())].chars().count())
}
