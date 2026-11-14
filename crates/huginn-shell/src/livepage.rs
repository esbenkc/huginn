//! The engine-thread half of live JavaScript in the window (the UI half is
//! `live.rs`). After load the page keeps its script engine: timers run in
//! real time (background tabs at most once a second), user input becomes
//! DOM events with their default actions (links, form controls, text
//! editing, focus), and DOM changes are styled and laid out again.
//!
//! Pages without scripts never get an isolate, unless they have inline
//! `on*` handler attributes (then the first input event creates it).
//! Default actions run without one, so plain HTML forms work too.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use huginn_dom::{Document, InteractionState, NodeData, NodeId, local_name};
use huginn_js::activation::{self, EngineSink, EventSink};
use huginn_js::{Event, EventTarget, EventValue, HostContext, HostRequest, ScriptKind, forms};
use huginn_layout::{LayoutEngine, Size};
use huginn_net::{Loader, Url};
use huginn_style::{StyleMap, Viewport};

use crate::engine::{EngineEvent, Links, PostData};
use crate::input::{Modifiers, MouseButton, MouseKind};
use crate::pipeline::Page;
use crate::scripts::PageLayout;

/// Background tabs run timers at most this often (Chrome: once a second).
const BACKGROUND_PERIOD: Duration = Duration::from_secs(1);

/// A DOM input event, sent by the UI (`Tab::input`) to the engine thread.
#[derive(Clone, Debug)]
pub enum PageInput {
    Mouse(MouseInput),
    Key(KeyInput),
    /// Editing commands for the focused text field (Cmd+A/C/X/V/Z).
    Edit(EditCommand),
    /// An option picked in a `<select>` popup.
    SelectOption {
        node: NodeId,
        index: usize,
    },
    /// The pointer left the page area.
    Leave,
    /// Scroll a scroll container by `dy` CSS px, to at most `max`, and fire
    /// `scroll` at it (keys; `Tab::scroll_key` picked it).
    Scroll {
        node: NodeId,
        dy: f32,
        max: f32,
    },
    /// A wheel turn of `dy` at document point (`x`, `y`): a `wheel` event at
    /// `target`, then, unless a listener cancels it, `scroll` as for
    /// [`PageInput::Scroll`] (`Tab::wheel` picked the container; `None`:
    /// the UI scrolled the page).
    Wheel {
        x: f32,
        y: f32,
        /// Viewport CSS px (`clientX`/`clientY`).
        client: (f32, f32),
        target: Option<NodeId>,
        dy: f32,
        scroll: Option<(NodeId, f32)>,
    },
}

#[derive(Clone, Debug)]
pub struct MouseInput {
    pub kind: MouseKind,
    pub button: MouseButton,
    /// A button is held (for moves: a drag).
    pub pressed: bool,
    pub clicks: u8,
    /// Document CSS px.
    pub x: f32,
    pub y: f32,
    /// Viewport CSS px (`clientX`/`clientY`).
    pub client_x: f32,
    pub client_y: f32,
    pub mods: Modifiers,
    /// Innermost node under the pointer.
    pub target: Option<NodeId>,
    /// Character index in the text field under the pointer, if any.
    pub text_pos: Option<usize>,
}

/// A key press, in DOM terms (`KeyboardEvent.key` / `.code`).
#[derive(Clone, Debug)]
pub struct KeyInput {
    pub key: String,
    pub code: String,
    /// Legacy `keyCode`.
    pub key_code: u32,
    pub mods: Modifiers,
}

impl KeyInput {
    /// Text the key types: a single printable character (or a space).
    fn text(&self) -> Option<&str> {
        let printable = self.key.chars().count() == 1 && !self.key.chars().any(char::is_control);
        (printable && !self.mods.cmd && !self.mods.ctrl).then_some(self.key.as_str())
    }
}

#[derive(Clone, Debug)]
pub enum EditCommand {
    SelectAll,
    Copy,
    Cut,
    Paste(String),
    Undo,
    Redo,
}

/// What the UI needs to know about the page's focus.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub generation: u64,
    pub focus: Option<NodeId>,
    /// The focused text field and its selection.
    pub field: Option<FieldStatus>,
    /// Space and Enter act on the focused element (a link, button, checkbox).
    pub activatable: bool,
    /// The page has a V8 isolate.
    pub isolate: bool,
    /// The page had scripts (or inline handlers) at load: link clicks go
    /// through the engine so listeners can cancel them. Without, the UI
    /// follows links itself, at once.
    pub scripted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldStatus {
    pub node: NodeId,
    /// Caret and selection anchor, as char indices into the value.
    pub caret: usize,
    pub anchor: usize,
    pub multiline: bool,
}

/// Engine -> UI events of the live page (inside `EngineEvent::Live`).
#[derive(Clone, Debug)]
pub enum LiveEvent {
    Status(Status),
    /// Write this to the clipboard (copy or cut in a text field).
    Copy(String),
    /// Open the drop-down of a `<select>`.
    OpenSelect {
        generation: u64,
        node: NodeId,
        items: Vec<SelectItem>,
        selected: Option<usize>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelectItem {
    pub label: String,
    pub disabled: bool,
}

/// The focused text field being edited.
struct Field {
    node: NodeId,
    caret: usize,
    anchor: usize,
    /// The value we last saw: a different value means a script set it, and
    /// the caret moves to the end (like Chrome).
    known: String,
    /// The value at focus time: on blur, a different value fires `change`.
    at_focus: String,
    undo: Vec<(String, usize)>,
    redo: Vec<(String, usize)>,
    /// The last edit was typing: further typing joins its undo step.
    typing: bool,
}

/// Live state of one page on its engine thread.
pub struct LivePage {
    loader: Arc<Loader>,
    /// Time origin of `performance.now()` and timers.
    start: Instant,
    /// The page has inline `on*` handlers but maybe no isolate yet.
    inline_handlers: bool,
    /// See [`Status::scripted`].
    scripted: bool,
    visible: bool,
    last_timers: Instant,
    hover: Option<NodeId>,
    pressed: Option<NodeId>,
    field: Option<Field>,
    /// Document generation, scroll generation and interaction state of the
    /// tree on screen.
    shown_generation: u64,
    shown_scroll: u64,
    shown_state: InteractionState,
    status: Status,
    /// Image URLs fetched after load (failures are not retried).
    tried: HashSet<String>,
}

/// What an engine call asks the browser to do.
struct Outcome {
    requests: Vec<HostRequest>,
    /// Modifiers of the user action, for link dispositions.
    mods: Modifiers,
    copy: Option<String>,
    open_select: Option<NodeId>,
}

impl LivePage {
    /// The page's loader (scripts' requests, images added later).
    pub fn loader(&self) -> &Loader {
        &self.loader
    }

    pub fn new(loader: Loader, start: Instant, page: &mut Page) -> LivePage {
        let inline_handlers = has_inline_handlers(&page.doc);
        let scripted =
            inline_handlers || page.js.as_deref_mut().is_some_and(|js| js.heap_bytes() > 0);
        LivePage {
            loader: Arc::new(loader),
            start,
            inline_handlers,
            scripted,
            visible: true,
            last_timers: Instant::now(),
            hover: None,
            pressed: None,
            field: None,
            shown_generation: page.doc.generation(),
            shown_scroll: page.doc.scroll_generation(),
            shown_state: page.doc.interaction().clone(),
            status: Status::default(),
            tried: HashSet::new(),
        }
    }

    /// When the engine thread must wake up for timers.
    pub fn next_wakeup(&self, page: &Page) -> Option<Instant> {
        let due = page.js.as_ref()?.next_timer_due()?;
        let at = self.start + Duration::from_secs_f64(due.max(0.0) / 1000.0);
        Some(if self.visible {
            at
        } else {
            at.max(self.last_timers + BACKGROUND_PERIOD)
        })
    }

    /// The tab became the visible one, or a background tab.
    pub fn set_visible(&mut self, page: &mut Page, visible: bool) {
        self.visible = visible;
        if !visible && let Some(js) = page.js.as_deref_mut() {
            js.trim_memory();
        }
    }

    /// Run due timers, then show their DOM changes.
    pub fn run_timers(
        &mut self,
        page: &mut Page,
        layout: &mut LayoutEngine,
        viewport: Viewport,
        generation: u64,
        notify: &dyn Fn(EngineEvent),
    ) {
        if page.js.is_none() {
            return;
        }
        self.last_timers = Instant::now();
        let out = self.call(page, layout, viewport, |_, sink, _| {
            if let Some(js) = sink.engine.as_deref_mut() {
                let _ = js.run_timers(sink.cx);
                sink.requests.extend(js.take_requests());
            }
            Reply::default()
        });
        self.finish(page, layout, viewport, generation, out, notify);
    }

    /// Requests left by the load (`location.href = ...` in a script), then
    /// the first status.
    pub fn after_load(
        &mut self,
        page: &mut Page,
        layout: &mut LayoutEngine,
        viewport: Viewport,
        generation: u64,
        notify: &dyn Fn(EngineEvent),
    ) {
        let requests = page
            .js
            .as_deref_mut()
            .map_or_else(Vec::new, |js| js.take_requests());
        let out = Outcome {
            requests,
            mods: Modifiers::default(),
            copy: None,
            open_select: None,
        };
        self.finish(page, layout, viewport, generation, out, notify);
    }

    /// The UI navigated to `#id` in this document: update `location` and fire
    /// `hashchange`, like a fragment link would.
    pub fn fragment(
        &mut self,
        page: &mut Page,
        layout: &mut LayoutEngine,
        viewport: Viewport,
        generation: u64,
        id: &str,
        notify: &dyn Fn(EngineEvent),
    ) {
        if page.js.as_deref_mut().is_none_or(|js| js.heap_bytes() == 0) {
            return;
        }
        let code = format!("location.hash = {};", js_string(&format!("#{id}")));
        let out = self.call(page, layout, viewport, |_, sink, _| {
            if let Some(js) = sink.engine.as_deref_mut() {
                let _ = js.execute(sink.cx, &code, "huginn:fragment", ScriptKind::Classic);
                let _ = js.run_microtasks(sink.cx);
                sink.requests.extend(js.take_requests());
            }
            Reply::default()
        });
        self.finish(page, layout, viewport, generation, out, notify);
    }

    /// Dispatch one input event and run its default action.
    pub fn input(
        &mut self,
        page: &mut Page,
        layout: &mut LayoutEngine,
        viewport: Viewport,
        generation: u64,
        ev: PageInput,
        notify: &dyn Fn(EngineEvent),
    ) {
        let out = self.call(page, layout, viewport, |me, sink, styles| match ev {
            PageInput::Mouse(m) => me.mouse(sink, &m),
            PageInput::Key(k) => me.key(sink, &k, styles),
            PageInput::Edit(cmd) => me.edit_command(sink, cmd),
            PageInput::SelectOption { node, index } => {
                if forms::select_option(sink.document(), node, index) {
                    activation::fire_input_change(sink, node);
                }
                Reply::default()
            }
            PageInput::Leave => {
                me.set_hover(sink, None, None);
                Reply::default()
            }
            PageInput::Scroll { node, dy, max } => {
                scroll_element(sink, node, dy, max);
                Reply::default()
            }
            PageInput::Wheel {
                x,
                y,
                client,
                target,
                dy,
                scroll,
            } => {
                let target = target.and_then(|t| element_of(sink.document(), t));
                let go = target.is_none_or(|t| {
                    let init = vec![
                        ("deltaY", EventValue::Num(f64::from(dy))),
                        ("clientX", EventValue::Num(f64::from(client.0))),
                        ("clientY", EventValue::Num(f64::from(client.1))),
                        ("x", EventValue::Num(f64::from(client.0))),
                        ("y", EventValue::Num(f64::from(client.1))),
                        ("pageX", EventValue::Num(f64::from(x))),
                        ("pageY", EventValue::Num(f64::from(y))),
                    ];
                    let ev = Event::ui("wheel", "WheelEvent", init).with_bubbles(true, true);
                    sink.fire(EventTarget::Node(t), &ev)
                });
                if go && let Some((node, max)) = scroll {
                    scroll_element(sink, node, dy, max);
                }
                Reply::default()
            }
        });
        self.finish(page, layout, viewport, generation, out, notify);
    }

    /// Run `f` with an event sink over the page. Creates the isolate first
    /// if the page has inline handlers.
    fn call(
        &mut self,
        page: &mut Page,
        layout: &mut LayoutEngine,
        viewport: Viewport,
        f: impl FnOnce(&mut LivePage, &mut EngineSink<'_, '_>, &StyleMap) -> Reply,
    ) -> Outcome {
        let loader = self.loader.clone();
        let now_ms = self.start.elapsed().as_secs_f64() * 1000.0;
        let mut engine_slot = Some(std::mem::take(layout));
        let size = Size {
            width: viewport.width,
            height: viewport.height,
        };
        let Page {
            doc,
            style_engine,
            styles,
            images,
            base,
            url,
            referrer,
            js,
            ..
        } = page;
        let mut host = PageLayout::new(style_engine, images, base, size, &mut engine_slot);
        let mut cx = HostContext {
            document: doc,
            loader: &loader,
            document_url: url,
            referrer,
            base_url: base,
            viewport: (viewport.width, viewport.height),
            now_ms,
            layout: Some(&mut host),
            current_script: None,
        };
        if self.inline_handlers
            && let Some(js) = js.as_deref_mut()
            && js.heap_bytes() == 0
        {
            let _ = js.execute(&mut cx, "", "huginn:handlers", ScriptKind::Classic);
        }
        let mut sink = EngineSink::new(js.as_deref_mut(), &mut cx);
        let reply = f(self, &mut sink, styles);
        let requests = std::mem::take(&mut sink.requests);
        drop(sink);
        drop(cx);
        drop(host);
        *layout = engine_slot.take().unwrap_or_default();
        Outcome {
            requests,
            mods: reply.mods,
            copy: reply.copy,
            open_select: reply.open_select,
        }
    }

    /// Navigate, restyle and lay out after DOM changes, report the status.
    fn finish(
        &mut self,
        page: &mut Page,
        layout: &mut LayoutEngine,
        viewport: Viewport,
        generation: u64,
        out: Outcome,
        notify: &dyn Fn(EngineEvent),
    ) {
        for r in out.requests {
            let HostRequest::Navigate {
                url, post, new_tab, ..
            } = r;
            if url.scheme() == "javascript" {
                self.run_javascript_url(page, layout, viewport, &url);
                continue;
            }
            let mut mods = out.mods;
            if new_tab && !mods.cmd && !mods.shift {
                // `target=_blank`: a new foreground tab.
                mods.cmd = true;
                mods.shift = true;
            }
            let post = post.map(|p| PostData {
                content_type: p.content_type,
                body: p.body,
            });
            notify(EngineEvent::Navigate {
                generation,
                url,
                post,
                mods,
            });
        }
        if let Some(text) = out.copy {
            notify(EngineEvent::Live(LiveEvent::Copy(text)));
        }
        self.sync_field(&page.doc);
        let state = page.doc.interaction().clone();
        let old = &self.shown_state;
        let state_dirty = state != *old
            && page.style_engine.uses_state(
                &page.doc,
                state.hover != old.hover,
                state.active != old.active,
                state.focus != old.focus || state.focus_visible != old.focus_visible,
            );
        if page.doc.generation() != self.shown_generation || state_dirty {
            let _span = crate::perf::span("update");
            page.restyle_dom(&self.loader);
            page.load_added(&self.loader, &mut self.tried);
            page.register_fonts(layout);
            let tree = page.layout(layout, viewport);
            notify(EngineEvent::Relaid {
                generation,
                tree,
                links: Some(Links::build(&page.doc, &page.base)),
                title: Some(crate::engine::title(&page.doc)),
            });
            self.shown_generation = page.doc.generation();
        } else if page.doc.scroll_generation() != self.shown_scroll {
            // Only scroll offsets changed (the wheel, or `scrollTop` from a
            // script): the styles hold, lay out again.
            let _span = crate::perf::span("update");
            let tree = page.layout(layout, viewport);
            notify(EngineEvent::Relaid {
                generation,
                tree,
                links: None,
                title: None,
            });
        }
        self.shown_scroll = page.doc.scroll_generation();
        self.shown_state = state;
        if let Some(node) = out.open_select {
            let opts = forms::options(&page.doc, node);
            let items = opts
                .iter()
                .map(|&o| SelectItem {
                    label: forms::option_label(&page.doc, o),
                    disabled: forms::is_disabled(&page.doc, o),
                })
                .collect();
            let selected = forms::selected_index(&page.doc, node);
            notify(EngineEvent::Live(LiveEvent::OpenSelect {
                generation,
                node,
                items,
                selected,
            }));
        }
        let status = self.make_status(page, generation);
        if status != self.status {
            self.status = status.clone();
            notify(EngineEvent::Live(LiveEvent::Status(status)));
        }
    }

    fn make_status(&self, page: &mut Page, generation: u64) -> Status {
        let doc = &page.doc;
        let focus = doc.interaction().focus;
        let activatable = focus.is_some_and(|f| {
            !forms::is_text_control(doc, f)
                && doc.element(f).is_some_and(|e| {
                    matches!(
                        &*e.name.local,
                        "a" | "area" | "button" | "input" | "summary" | "select"
                    )
                })
        });
        Status {
            generation,
            focus,
            field: self
                .field
                .as_ref()
                .filter(|f| Some(f.node) == focus)
                .map(|f| FieldStatus {
                    node: f.node,
                    caret: f.caret,
                    anchor: f.anchor,
                    multiline: !forms::is_single_line(doc, f.node),
                }),
            activatable,
            isolate: page.js.as_deref_mut().is_some_and(|js| js.heap_bytes() > 0),
            scripted: self.scripted,
        }
    }

    /// Follow focus changes made by scripts: start or stop editing, and
    /// move the caret to the end when a script replaced the value.
    fn sync_field(&mut self, doc: &Document) {
        let focus = doc
            .interaction()
            .focus
            .filter(|&f| forms::is_text_control(doc, f));
        match (focus, &mut self.field) {
            (None, _) => self.field = None,
            (Some(f), Some(field)) if field.node == f => {
                let v = forms::value(doc, f);
                if v != field.known {
                    let n = v.chars().count();
                    field.caret = n;
                    field.anchor = n;
                    field.known = v;
                }
            }
            (Some(f), _) => self.field = Some(Field::new(doc, f, true)),
        }
    }

    /// `javascript:` URL: run it in the page instead of navigating.
    fn run_javascript_url(
        &mut self,
        page: &mut Page,
        layout: &mut LayoutEngine,
        viewport: Viewport,
        url: &Url,
    ) {
        let code = percent_decode(&url.as_str()["javascript:".len()..]);
        if code.trim().is_empty() || code.trim() == "void(0)" || code.trim() == "void 0" {
            return;
        }
        let _ = self.call(page, layout, viewport, |_, sink, _| {
            if let Some(js) = sink.engine.as_deref_mut() {
                let _ = js.execute(sink.cx, &code, "javascript:", ScriptKind::Classic);
                let _ = js.run_microtasks(sink.cx);
            }
            Reply::default()
        });
    }

    // ----- mouse ------------------------------------------------------------

    fn mouse(&mut self, sink: &mut EngineSink<'_, '_>, m: &MouseInput) -> Reply {
        let doc = sink.document();
        let target = m.target.and_then(|t| element_of(doc, t));
        let mut reply = Reply {
            mods: m.mods,
            ..Reply::default()
        };
        match m.kind {
            MouseKind::Move => {
                self.set_hover(sink, target, Some(m));
                if let Some(t) = target {
                    let buttons = if m.pressed { button_bit(m.button) } else { 0 };
                    sink.fire(
                        EventTarget::Node(t),
                        &mouse_event("pointermove", "PointerEvent", m, buttons, false),
                    );
                    sink.fire(
                        EventTarget::Node(t),
                        &mouse_event("mousemove", "MouseEvent", m, buttons, true),
                    );
                }
                // Drag-select inside the focused field.
                if m.pressed
                    && let (Some(field), Some(pos)) = (&mut self.field, m.text_pos)
                    && target == Some(field.node)
                    && self.pressed == Some(field.node)
                {
                    field.caret = pos;
                }
            }
            MouseKind::Down => {
                self.set_hover(sink, target, Some(m));
                let Some(t) = target else { return reply };
                let bit = button_bit(m.button);
                sink.fire(
                    EventTarget::Node(t),
                    &mouse_event("pointerdown", "PointerEvent", m, bit, true),
                );
                let ok = sink.fire(
                    EventTarget::Node(t),
                    &mouse_event("mousedown", "MouseEvent", m, bit, true),
                );
                match m.button {
                    MouseButton::Left => {
                        self.pressed = Some(t);
                        let state = InteractionState {
                            active: Some(t),
                            ..sink.document().interaction().clone()
                        };
                        sink.document().set_interaction(state);
                        if ok {
                            let doc = sink.document();
                            let focus = forms::click_focus_target(doc, t);
                            let visible = focus.is_some_and(|f| forms::is_text_control(doc, f));
                            self.focus(sink, focus, visible, false);
                            if let (Some(field), Some(f)) = (&mut self.field, focus)
                                && field.node == f
                            {
                                field.click(sink.document(), m.text_pos, m.clicks, m.mods.shift);
                            }
                            let doc = sink.document();
                            if let Some(f) = focus
                                && doc
                                    .element(f)
                                    .is_some_and(|e| e.is_html(&local_name!("select")))
                                && doc.attribute(f, &local_name!("multiple")).is_none()
                            {
                                reply.open_select = Some(f);
                            }
                        }
                    }
                    MouseButton::Right => {
                        // macOS fires contextmenu on mouse down.
                        sink.fire(
                            EventTarget::Node(t),
                            &mouse_event("contextmenu", "PointerEvent", m, bit, true),
                        );
                    }
                    MouseButton::Middle => {}
                }
            }
            MouseKind::Up => {
                let bit = button_bit(m.button);
                if let Some(t) = target {
                    sink.fire(
                        EventTarget::Node(t),
                        &mouse_event("pointerup", "PointerEvent", m, 0, true),
                    );
                    sink.fire(
                        EventTarget::Node(t),
                        &mouse_event("mouseup", "MouseEvent", m, 0, true),
                    );
                }
                if m.button == MouseButton::Left {
                    let state = InteractionState {
                        active: None,
                        ..sink.document().interaction().clone()
                    };
                    sink.document().set_interaction(state);
                    let pressed = self.pressed.take();
                    let doc = sink.document();
                    let clicked = match (pressed, target) {
                        (Some(p), Some(t)) => common_ancestor(doc, p, t),
                        _ => None,
                    };
                    if let Some(c) = clicked {
                        let before = sink.requests.len();
                        activation::click(
                            sink,
                            c,
                            &mouse_event("click", "PointerEvent", m, 0, true),
                        );
                        // Without scripts the UI already followed the link.
                        let base = sink.base_url();
                        if !self.scripted
                            && matches!(
                                forms::activation(sink.document(), c, &base),
                                Some((_, forms::Activation::Link { .. }))
                            )
                        {
                            sink.requests.truncate(before);
                        }
                        if m.clicks == 2 {
                            sink.fire(
                                EventTarget::Node(c),
                                &mouse_event("dblclick", "MouseEvent", m, 0, true),
                            );
                        }
                    }
                } else if let Some(t) = target
                    && m.button == MouseButton::Middle
                    && self.scripted
                {
                    let ok = sink.fire(
                        EventTarget::Node(t),
                        &mouse_event("auxclick", "PointerEvent", m, bit, true),
                    );
                    let base = sink.base_url();
                    if ok
                        && let Some((_, forms::Activation::Link { url, .. })) =
                            forms::activation(sink.document(), t, &base)
                    {
                        // A middle click opens the link in a background tab.
                        reply.mods.cmd = true;
                        sink.request(HostRequest::Navigate {
                            url,
                            post: None,
                            new_tab: false,
                            replace: false,
                        });
                    }
                }
            }
        }
        reply
    }

    /// Move `:hover` to `to`, firing over/out and enter/leave events.
    fn set_hover(
        &mut self,
        sink: &mut EngineSink<'_, '_>,
        to: Option<NodeId>,
        m: Option<&MouseInput>,
    ) {
        let old = self.hover;
        if old == to {
            return;
        }
        self.hover = to;
        let state = InteractionState {
            hover: to,
            ..sink.document().interaction().clone()
        };
        sink.document().set_interaction(state);
        let Some(m) = m else { return };
        let doc = sink.document();
        let chain = |n: Option<NodeId>| -> Vec<NodeId> {
            n.map_or_else(Vec::new, |n| {
                std::iter::once(n)
                    .chain(doc.ancestors(n))
                    .filter(|&a| doc.element(a).is_some())
                    .collect()
            })
        };
        let (old_chain, new_chain) = (chain(old.filter(|&o| is_connected(doc, o))), chain(to));
        let with_related =
            |kind: &str, iface: &'static str, related: Option<NodeId>, bubbles: bool| {
                let mut e = mouse_event(kind, iface, m, 0, bubbles);
                e.bubbles = bubbles;
                e.cancelable = bubbles;
                e.init.push(("relatedTarget", EventValue::Node(related)));
                e
            };
        if let Some(&o) = old_chain.first() {
            sink.fire(
                EventTarget::Node(o),
                &with_related("pointerout", "PointerEvent", to, true),
            );
            sink.fire(
                EventTarget::Node(o),
                &with_related("mouseout", "MouseEvent", to, true),
            );
        }
        for &o in old_chain.iter().filter(|o| !new_chain.contains(o)) {
            sink.fire(
                EventTarget::Node(o),
                &with_related("pointerleave", "PointerEvent", to, false),
            );
            sink.fire(
                EventTarget::Node(o),
                &with_related("mouseleave", "MouseEvent", to, false),
            );
        }
        if let Some(&n) = new_chain.first() {
            sink.fire(
                EventTarget::Node(n),
                &with_related("pointerover", "PointerEvent", old, true),
            );
            sink.fire(
                EventTarget::Node(n),
                &with_related("mouseover", "MouseEvent", old, true),
            );
        }
        for &n in new_chain.iter().rev().filter(|n| !old_chain.contains(n)) {
            sink.fire(
                EventTarget::Node(n),
                &with_related("pointerenter", "PointerEvent", old, false),
            );
            sink.fire(
                EventTarget::Node(n),
                &with_related("mouseenter", "MouseEvent", old, false),
            );
        }
    }

    /// Move focus, firing `change` on a text field the user changed.
    fn focus(
        &mut self,
        sink: &mut EngineSink<'_, '_>,
        to: Option<NodeId>,
        visible: bool,
        select_all: bool,
    ) {
        let doc = sink.document();
        if doc.interaction().focus == to {
            return;
        }
        if let Some(field) = self.field.take()
            && forms::value(doc, field.node) != field.at_focus
        {
            sink.fire(
                EventTarget::Node(field.node),
                &Event::new("change", true, false),
            );
        }
        activation::set_focus(sink, to, visible);
        let doc = sink.document();
        self.field = to
            .filter(|&f| doc.interaction().focus == Some(f) && forms::is_text_control(doc, f))
            .map(|f| Field::new(doc, f, select_all));
    }

    // ----- keyboard ---------------------------------------------------------

    fn key(&mut self, sink: &mut EngineSink<'_, '_>, k: &KeyInput, styles: &StyleMap) -> Reply {
        let doc = sink.document();
        let focus = doc.interaction().focus.filter(|&f| is_connected(doc, f));
        let target = focus.map_or_else(
            || doc.body().map_or(EventTarget::Document, EventTarget::Node),
            EventTarget::Node,
        );
        let reply = Reply {
            mods: k.mods,
            ..Reply::default()
        };
        let mut ok = sink.fire(target, &key_event("keydown", k));
        if ok && let Some(text) = k.text().or((k.key == "Enter").then_some("\r")) {
            let mut press = key_event("keypress", k);
            let code = text.chars().next().map_or(0, |c| c as u32);
            press.init.extend([
                ("charCode", EventValue::Num(f64::from(code))),
                ("keyCode", EventValue::Num(f64::from(code))),
                ("which", EventValue::Num(f64::from(code))),
            ]);
            ok = sink.fire(target, &press);
        }
        if ok {
            self.key_default(sink, k, focus, styles);
        }
        sink.fire(target, &key_event("keyup", k));
        reply
    }

    /// Default actions of a key press that no listener canceled.
    fn key_default(
        &mut self,
        sink: &mut EngineSink<'_, '_>,
        k: &KeyInput,
        focus: Option<NodeId>,
        styles: &StyleMap,
    ) {
        if k.key == "Tab" && !k.mods.cmd && !k.mods.ctrl && !k.mods.alt {
            let doc = sink.document();
            let next = crate::interact::focus::next_focusable(doc, styles, focus, k.mods.shift);
            let select_all = next.is_some_and(|n| forms::is_single_line(doc, n));
            self.focus(sink, next, true, select_all);
            return;
        }
        if self.field.as_ref().is_some_and(|f| Some(f.node) == focus) {
            self.edit_key(sink, k);
            return;
        }
        let Some(f) = focus else { return };
        let doc = sink.document();
        let el = doc.element(f);
        let is_input = |types: &[&str]| {
            el.is_some_and(|e| {
                e.is_html(&local_name!("input")) && types.contains(&forms::input_type(e).as_str())
            })
        };
        let activates = match k.key.as_str() {
            "Enter" => {
                el.is_some_and(|e| matches!(&*e.name.local, "a" | "area" | "button" | "summary"))
                    || is_input(&["submit", "reset", "button", "image"])
            }
            " " => {
                el.is_some_and(|e| matches!(&*e.name.local, "button" | "summary"))
                    || is_input(&["submit", "reset", "button", "image", "checkbox", "radio"])
            }
            _ => false,
        };
        if activates {
            activation::click(
                sink,
                f,
                &activation::click_event(vec![("detail", EventValue::Num(0.0))], true),
            );
        } else if k.key == "Enter" && is_input(&["checkbox", "radio"]) {
            activation::implicit_submit(sink, f);
        }
    }

    /// Editing keys in the focused text field.
    fn edit_key(&mut self, sink: &mut EngineSink<'_, '_>, k: &KeyInput) {
        let Some(field) = self.field.as_mut() else {
            return;
        };
        let doc = sink.document();
        let value = forms::value(doc, field.node);
        let multiline = !forms::is_single_line(doc, field.node);
        let chars: Vec<char> = value.chars().collect();
        let n = chars.len();
        let (shift, word, line) = (k.mods.shift, k.mods.alt, k.mods.cmd);
        let move_to = |field: &mut Field, pos: usize| {
            field.caret = pos.min(n);
            if !shift {
                field.anchor = field.caret;
            }
            field.typing = false;
        };
        match k.key.as_str() {
            "ArrowLeft" | "ArrowRight"
                if !shift && field.caret != field.anchor && !word && !line =>
            {
                let (a, b) = field.range();
                let pos = if k.key == "ArrowLeft" { a } else { b };
                move_to(field, pos);
            }
            "ArrowLeft" => {
                let pos = if line {
                    line_start(&chars, field.caret)
                } else if word {
                    word_left(&chars, field.caret)
                } else {
                    field.caret.saturating_sub(1)
                };
                move_to(field, pos);
            }
            "ArrowRight" => {
                let pos = if line {
                    line_end(&chars, field.caret)
                } else if word {
                    word_right(&chars, field.caret)
                } else {
                    field.caret + 1
                };
                move_to(field, pos);
            }
            "Home" => move_to(
                field,
                if multiline && !line {
                    line_start(&chars, field.caret)
                } else {
                    0
                },
            ),
            "End" => move_to(
                field,
                if multiline && !line {
                    line_end(&chars, field.caret)
                } else {
                    n
                },
            ),
            "ArrowUp" | "ArrowDown" if !multiline || line => {
                move_to(field, if k.key == "ArrowUp" { 0 } else { n })
            }
            "ArrowUp" | "ArrowDown" => {
                let pos = vertical(&chars, field.caret, k.key == "ArrowUp");
                move_to(field, pos);
            }
            "Backspace" | "Delete" => {
                let (a, b) = field.range();
                let (from, to) = if a != b {
                    (a, b)
                } else if k.key == "Backspace" {
                    let from = if line {
                        line_start(&chars, a)
                    } else if word {
                        word_left(&chars, a)
                    } else {
                        a.saturating_sub(1)
                    };
                    (from, a)
                } else {
                    let to = if word {
                        word_right(&chars, a)
                    } else {
                        (a + 1).min(n)
                    };
                    (a, to)
                };
                if from < to {
                    let kind = if k.key == "Backspace" {
                        "deleteContentBackward"
                    } else {
                        "deleteContentForward"
                    };
                    self.replace(sink, from, to, "", kind);
                }
            }
            "Enter" if multiline => {
                let (a, b) = field.range();
                self.replace(sink, a, b, "\n", "insertLineBreak");
            }
            "Enter" => {
                let node = field.node;
                activation::implicit_submit(sink, node);
            }
            _ => {
                if let Some(text) = k.text().filter(|_| !k.mods.alt) {
                    let (a, b) = field.range();
                    self.replace(sink, a, b, text, "insertText");
                }
            }
        }
    }

    /// Replace chars `from..to` of the field's value with `text`:
    /// `beforeinput`, the change, `input`.
    fn replace(
        &mut self,
        sink: &mut EngineSink<'_, '_>,
        from: usize,
        to: usize,
        text: &str,
        input_type: &str,
    ) {
        let Some(node) = self.field.as_ref().map(|f| f.node) else {
            return;
        };
        let doc = sink.document();
        if doc.attribute(node, &local_name!("readonly")).is_some() || forms::is_disabled(doc, node)
        {
            return;
        }
        let value = forms::value(doc, node);
        let single = forms::is_single_line(doc, node);
        let mut text: String = if single {
            text.replace(['\r', '\n'], "")
        } else {
            text.replace("\r\n", "\n")
        };
        if let Some(max) = doc
            .attribute(node, &local_name!("maxlength"))
            .and_then(|m| m.trim().parse::<usize>().ok())
        {
            let room = max.saturating_sub(value.chars().count() - (to - from));
            text = text.chars().take(room).collect();
            if text.is_empty() && from == to {
                return;
            }
        }
        let data = if text.is_empty() {
            EventValue::Node(None)
        } else {
            EventValue::Str(text.clone())
        };
        let input_init = |e: &mut Event| {
            e.init
                .push(("inputType", EventValue::Str(input_type.to_owned())));
            e.init.push(("data", data.clone()));
        };
        let mut before = Event::ui("beforeinput", "InputEvent", Vec::new());
        input_init(&mut before);
        if !sink.fire(EventTarget::Node(node), &before) {
            return;
        }
        let doc = sink.document();
        let value = forms::value(doc, node);
        let chars: Vec<char> = value.chars().collect();
        let (from, to) = (from.min(chars.len()), to.min(chars.len()));
        let new: String = chars[..from]
            .iter()
            .chain(text.chars().collect::<Vec<_>>().iter())
            .chain(&chars[to..])
            .collect();
        let Some(field) = self.field.as_mut() else {
            return;
        };
        let typing = input_type == "insertText";
        if !(typing && field.typing) {
            field.undo.push((value.clone(), field.caret));
        }
        field.redo.clear();
        field.typing = typing;
        field.caret = from + text.chars().count();
        field.anchor = field.caret;
        field.known = new.clone();
        forms::set_value(doc, node, &new);
        let mut input = Event::ui("input", "InputEvent", Vec::new()).with_bubbles(true, false);
        input_init(&mut input);
        sink.fire(EventTarget::Node(node), &input);
    }

    fn edit_command(&mut self, sink: &mut EngineSink<'_, '_>, cmd: EditCommand) -> Reply {
        let mut reply = Reply::default();
        let Some(field) = self.field.as_mut() else {
            return reply;
        };
        let node = field.node;
        let doc = sink.document();
        let value = forms::value(doc, node);
        let (a, b) = field.range();
        let selected: String = value.chars().skip(a).take(b - a).collect();
        let password = doc.element(node).is_some_and(|e| {
            e.is_html(&local_name!("input")) && forms::input_type(e) == "password"
        });
        match cmd {
            EditCommand::SelectAll => {
                field.anchor = 0;
                field.caret = value.chars().count();
            }
            EditCommand::Copy | EditCommand::Cut => {
                let kind = if matches!(cmd, EditCommand::Copy) {
                    "copy"
                } else {
                    "cut"
                };
                if a == b
                    || password
                    || !sink.fire(
                        EventTarget::Node(node),
                        &Event::ui(kind, "ClipboardEvent", Vec::new()),
                    )
                {
                    return reply;
                }
                reply.copy = Some(selected);
                if kind == "cut" {
                    self.replace(sink, a, b, "", "deleteByCut");
                }
            }
            EditCommand::Paste(text) => {
                if sink.fire(
                    EventTarget::Node(node),
                    &Event::ui("paste", "ClipboardEvent", Vec::new()),
                ) {
                    self.replace(sink, a, b, &text, "insertFromPaste");
                }
            }
            EditCommand::Undo | EditCommand::Redo => {
                let undo = matches!(cmd, EditCommand::Undo);
                let (from, to) = if undo {
                    (&mut field.undo, &mut field.redo)
                } else {
                    (&mut field.redo, &mut field.undo)
                };
                let Some((v, caret)) = from.pop() else {
                    return reply;
                };
                to.push((value, field.caret));
                field.caret = caret.min(v.chars().count());
                field.anchor = field.caret;
                field.known = v.clone();
                field.typing = false;
                forms::set_value(doc, node, &v);
                let mut input =
                    Event::ui("input", "InputEvent", Vec::new()).with_bubbles(true, false);
                input.init.push((
                    "inputType",
                    EventValue::Str(if undo { "historyUndo" } else { "historyRedo" }.to_owned()),
                ));
                sink.fire(EventTarget::Node(node), &input);
            }
        }
        reply
    }
}

/// What one input event asks of the browser besides its requests.
#[derive(Default)]
struct Reply {
    mods: Modifiers,
    copy: Option<String>,
    open_select: Option<NodeId>,
}

impl Field {
    fn new(doc: &Document, node: NodeId, select_all: bool) -> Field {
        let value = forms::value(doc, node);
        let n = value.chars().count();
        Field {
            node,
            caret: n,
            anchor: if select_all { 0 } else { n },
            known: value.clone(),
            at_focus: value,
            undo: Vec::new(),
            redo: Vec::new(),
            typing: false,
        }
    }

    /// Selection as (start, end).
    fn range(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    /// A click in the field: place the caret; double-click selects a word,
    /// triple-click everything; shift extends the selection.
    fn click(&mut self, doc: &Document, pos: Option<usize>, clicks: u8, shift: bool) {
        let value = forms::value(doc, self.node);
        let chars: Vec<char> = value.chars().collect();
        let pos = pos.unwrap_or(chars.len()).min(chars.len());
        self.typing = false;
        match clicks {
            2 => {
                self.anchor = word_left(&chars, (pos + 1).min(chars.len()));
                self.caret = word_right(&chars, pos);
            }
            n if n >= 3 => {
                self.anchor = 0;
                self.caret = chars.len();
            }
            _ => {
                self.caret = pos;
                if !shift {
                    self.anchor = pos;
                }
            }
        }
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn word_left(chars: &[char], pos: usize) -> usize {
    let mut i = pos.min(chars.len());
    while i > 0 && !is_word(chars[i - 1]) {
        i -= 1;
    }
    while i > 0 && is_word(chars[i - 1]) {
        i -= 1;
    }
    i
}

fn word_right(chars: &[char], pos: usize) -> usize {
    let mut i = pos.min(chars.len());
    while i < chars.len() && !is_word(chars[i]) {
        i += 1;
    }
    while i < chars.len() && is_word(chars[i]) {
        i += 1;
    }
    i
}

fn line_start(chars: &[char], pos: usize) -> usize {
    chars[..pos.min(chars.len())]
        .iter()
        .rposition(|&c| c == '\n')
        .map_or(0, |i| i + 1)
}

fn line_end(chars: &[char], pos: usize) -> usize {
    let pos = pos.min(chars.len());
    chars[pos..]
        .iter()
        .position(|&c| c == '\n')
        .map_or(chars.len(), |i| pos + i)
}

/// The same column on the previous or next line (by characters).
fn vertical(chars: &[char], pos: usize, up: bool) -> usize {
    let start = line_start(chars, pos);
    let col = pos - start;
    if up {
        if start == 0 {
            return 0;
        }
        let prev = line_start(chars, start - 1);
        prev + col.min(start - 1 - prev)
    } else {
        let end = line_end(chars, pos);
        if end == chars.len() {
            return end;
        }
        let next_end = line_end(chars, end + 1);
        (end + 1 + col).min(next_end)
    }
}

/// The element for a hit node: itself, or a text node's parent element.
fn element_of(doc: &Document, n: NodeId) -> Option<NodeId> {
    match doc.node(n).data {
        NodeData::Element(_) => Some(n),
        _ => doc.parent(n).filter(|&p| doc.element(p).is_some()),
    }
}

fn is_connected(doc: &Document, n: NodeId) -> bool {
    doc.root(n) == NodeId::DOCUMENT
}

/// Nearest common inclusive ancestor (the click target of a press on `a`
/// released over `b`).
fn common_ancestor(doc: &Document, a: NodeId, b: NodeId) -> Option<NodeId> {
    let chain: Vec<NodeId> = std::iter::once(a).chain(doc.ancestors(a)).collect();
    std::iter::once(b)
        .chain(doc.ancestors(b))
        .find(|n| chain.contains(n))
        .filter(|&n| doc.element(n).is_some())
}

fn button_bit(b: MouseButton) -> u16 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 4,
    }
}

fn mods_init(m: Modifiers) -> [(&'static str, EventValue); 4] {
    [
        ("ctrlKey", EventValue::Bool(m.ctrl)),
        ("shiftKey", EventValue::Bool(m.shift)),
        ("altKey", EventValue::Bool(m.alt)),
        ("metaKey", EventValue::Bool(m.cmd)),
    ]
}

/// Scroll `node` by `dy`, to at most `max`, and fire `scroll` at it (CSSOM
/// View: at the element, not bubbling).
fn scroll_element(sink: &mut EngineSink<'_, '_>, node: NodeId, dy: f32, max: f32) {
    let doc = sink.document();
    let (x, y) = doc.scroll_offset(node);
    let to = (y + dy).clamp(0.0, max.max(0.0));
    if to != y {
        doc.set_scroll_offset(node, x, to);
        sink.fire(EventTarget::Node(node), &Event::new("scroll", false, false));
    }
}

fn mouse_event(
    kind: &str,
    iface: &'static str,
    m: &MouseInput,
    buttons: u16,
    cancelable: bool,
) -> Event {
    let button = match m.button {
        MouseButton::Left => 0.0,
        MouseButton::Middle => 1.0,
        MouseButton::Right => 2.0,
    };
    let mut init = vec![
        ("clientX", EventValue::Num(f64::from(m.client_x))),
        ("clientY", EventValue::Num(f64::from(m.client_y))),
        ("x", EventValue::Num(f64::from(m.client_x))),
        ("y", EventValue::Num(f64::from(m.client_y))),
        ("pageX", EventValue::Num(f64::from(m.x))),
        ("pageY", EventValue::Num(f64::from(m.y))),
        ("screenX", EventValue::Num(f64::from(m.client_x))),
        ("screenY", EventValue::Num(f64::from(m.client_y))),
        ("button", EventValue::Num(button)),
        ("buttons", EventValue::Num(f64::from(buttons))),
        ("detail", EventValue::Num(f64::from(m.clicks))),
        ("which", EventValue::Num(button + 1.0)),
    ];
    init.extend(mods_init(m.mods));
    Event::ui(kind, iface, init).with_bubbles(true, cancelable)
}

fn key_event(kind: &str, k: &KeyInput) -> Event {
    let mut init = vec![
        ("key", EventValue::Str(k.key.clone())),
        ("code", EventValue::Str(k.code.clone())),
        ("keyCode", EventValue::Num(f64::from(k.key_code))),
        ("which", EventValue::Num(f64::from(k.key_code))),
    ];
    init.extend(mods_init(k.mods));
    Event::ui(kind, "KeyboardEvent", init)
}

/// The page has `on*` event handler attributes (they need an isolate even
/// without `<script>`s).
fn has_inline_handlers(doc: &Document) -> bool {
    doc.descendants(NodeId::DOCUMENT).any(|n| {
        doc.element(n).is_some_and(|e| {
            e.attrs
                .iter()
                .any(|a| a.name.local.starts_with("on") && a.name.local.len() > 2)
        })
    })
}

/// `s` as a JavaScript string literal.
fn js_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn percent_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={}", s.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_and_line_motion() {
        let c: Vec<char> = "ab cd\nefg h".chars().collect();
        assert_eq!(word_left(&c, 5), 3);
        assert_eq!(word_right(&c, 0), 2);
        assert_eq!(line_start(&c, 8), 6);
        assert_eq!(line_end(&c, 1), 5);
        assert_eq!(vertical(&c, 8, true), 2);
        assert_eq!(vertical(&c, 1, false), 7);
    }

    #[test]
    fn javascript_urls_decode() {
        assert_eq!(percent_decode("alert(%22a+b%22)"), "alert(\"a+b\")");
    }
}
