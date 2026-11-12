//! huginn-js: the JavaScript engine.
//!
//! This crate defines how the browser drives a script engine and how scripts
//! reach the DOM. With the `v8` cargo feature, [`create_engine`] returns a V8
//! engine (the `v8` crate): a JavaScript DOM layer (`src/prelude.js`) over a
//! small set of native operations on [`huginn_dom::Document`]. Without the
//! feature, [`create_engine`] returns `None` and pages render like a browser
//! with JavaScript turned off ([`ScriptingDisabled`]).
//!
//! DOM access from bindings goes through [`HostContext::document`], using the
//! mutation API on [`huginn_dom::Document`] (`create_element`, `append_child`,
//! `insert_before`, `detach`, `set_attribute`, `remove_attribute`,
//! `set_text_content`, ...). Node identity across the FFI boundary is
//! [`NodeId::to_raw`]/[`NodeId::from_raw`].

pub mod activation;
#[cfg_attr(not(feature = "v8"), allow(dead_code))]
mod dom_ops;
pub mod forms;
#[cfg_attr(not(feature = "v8"), allow(dead_code))]
mod importmap;
#[cfg(feature = "v8")]
mod v8engine;

use std::fmt;

use huginn_dom::{Document, ElementData, NodeData, NodeId, local_name};
use huginn_net::{Loader, Url};

/// Classic script, ES module, or import map.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ScriptKind {
    Classic,
    Module,
    /// `<script type="importmap">`: JSON that maps module specifiers.
    ImportMap,
}

/// When a script runs relative to parsing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ScriptTiming {
    /// Blocks the parser (plain `<script>`).
    Blocking,
    /// `defer` (and module scripts without `async`): after parsing, in order.
    Defer,
    /// `async`: as soon as fetched, in any order.
    Async,
}

/// A `<script>` element found in the document.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptRef {
    pub node: NodeId,
    pub kind: ScriptKind,
    pub timing: ScriptTiming,
    /// Absolute URL for external scripts, `None` for inline ones.
    pub src: Option<Url>,
}

/// Which kind of script a `<script>` element is, or `None` if the HTML spec
/// would not run it (data blocks such as `type="application/json"`, and
/// classic scripts marked `nomodule`).
pub fn script_kind(el: &ElementData) -> Option<ScriptKind> {
    let ty = el
        .attr(&local_name!("type"))
        .or_else(|| el.attr(&local_name!("language")).map(|_| ""))
        .map(|t| t.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let kind = match ty.as_str() {
        ""
        | "text/javascript"
        | "application/javascript"
        | "text/ecmascript"
        | "application/ecmascript"
        | "application/x-javascript"
        | "text/jscript" => ScriptKind::Classic,
        "module" => ScriptKind::Module,
        "importmap" => ScriptKind::ImportMap,
        _ => return None,
    };
    if kind == ScriptKind::Classic && el.attr(&local_name!("nomodule")).is_some() {
        return None;
    }
    Some(kind)
}

/// Find the scripts the HTML spec would run, in document order. Data blocks
/// (`type="application/json"` etc.) and `nomodule` scripts are skipped, and
/// so are external import maps, which browsers do not load.
pub fn collect_scripts(doc: &Document, base: &Url) -> Vec<ScriptRef> {
    let mut out = Vec::new();
    for n in doc.elements_by_local_name(&local_name!("script")) {
        let Some(el) = doc.element(n) else { continue };
        let Some(kind) = script_kind(el) else {
            continue;
        };
        let src = el
            .attr(&local_name!("src"))
            .and_then(|s| huginn_net::resolve_url(base, s));
        if kind == ScriptKind::ImportMap && el.attr(&local_name!("src")).is_some() {
            continue;
        }
        let timing = if el.attr(&local_name!("async")).is_some()
            && (src.is_some() || kind == ScriptKind::Module)
        {
            ScriptTiming::Async
        } else if kind == ScriptKind::Module
            || (src.is_some() && el.attr(&local_name!("defer")).is_some())
        {
            ScriptTiming::Defer
        } else {
            ScriptTiming::Blocking
        };
        out.push(ScriptRef {
            node: n,
            kind,
            timing,
            src,
        });
    }
    out
}

/// Inline source text of a `<script>` element.
pub fn inline_source(doc: &Document, script: NodeId) -> String {
    doc.children(script)
        .filter_map(|c| match &doc.node(c).data {
            NodeData::Text(t) => Some(&**t),
            _ => None,
        })
        .collect()
}

/// Everything a script engine may touch while it runs. Created by the shell
/// for each call, so the engine never owns the document.
pub struct HostContext<'a> {
    pub document: &'a mut Document,
    pub loader: &'a Loader,
    /// `document.URL` and `location`.
    pub document_url: &'a Url,
    /// Base for relative URLs (`<base href>` or the document URL).
    pub base_url: &'a Url,
    /// Viewport size in CSS px (`window.innerWidth/innerHeight`).
    pub viewport: (f32, f32),
    /// Milliseconds since navigation start (`performance.now()`).
    pub now_ms: f64,
    /// Style and layout on demand for geometry queries (`getBoundingClientRect`,
    /// `offsetWidth`, `getComputedStyle`). `None`: geometry reads as zero.
    pub layout: Option<&'a mut dyn LayoutHost>,
    /// The `<script>` element whose code `execute` runs (`document.currentScript`).
    pub current_script: Option<NodeId>,
}

/// Border box of a node in CSS px, in document coordinates (the viewport is
/// never scrolled, so these are also viewport coordinates), plus its border
/// widths (for `clientWidth` and friends).
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct NodeGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Left, top, right, bottom border widths.
    pub border: [f32; 4],
    /// Scrolling area from the padding box origin (`scrollWidth`,
    /// `scrollHeight`), at least the padding box.
    pub scroll_size: [f32; 2],
}

/// Style and layout queries from scripts, implemented by the shell. The
/// shell decides how often it really re-runs style and layout; it may return
/// stale results to bound the cost of layout thrashing.
pub trait LayoutHost {
    /// Geometry of the boxes generated by `node` (their union), or `None` if
    /// it generates no box (`display: none`, detached, or text inside a line).
    fn geometry(&mut self, doc: &Document, node: NodeId) -> Option<NodeGeometry>;
    /// Computed value of a CSS property (lowercase, hyphenated name), as
    /// `getComputedStyle(el).getPropertyValue(name)` would serialize it.
    /// Empty if unknown or the element is not rendered.
    fn computed_value(&mut self, doc: &Document, node: NodeId, property: &str) -> String;
}

/// A DOM event to dispatch.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// Event type, e.g. "load", "DOMContentLoaded", "click".
    pub kind: String,
    pub bubbles: bool,
    pub cancelable: bool,
    /// DOM interface to construct: "Event", "MouseEvent", "PointerEvent",
    /// "KeyboardEvent", "FocusEvent", "InputEvent", "SubmitEvent", ...
    pub interface: &'static str,
    /// Members of the event's init dictionary (`clientX`, `key`, ...).
    pub init: Vec<(&'static str, EventValue)>,
    /// `isTrusted`: fired by the browser, not by a script's `click()`.
    pub trusted: bool,
}

/// A value in an event's init dictionary.
#[derive(Clone, Debug, PartialEq)]
pub enum EventValue {
    Bool(bool),
    Num(f64),
    Str(String),
    /// An element (`relatedTarget`, `submitter`), or null.
    Node(Option<NodeId>),
}

impl Event {
    /// A trusted plain `Event`.
    pub fn new(kind: &str, bubbles: bool, cancelable: bool) -> Event {
        Event {
            kind: kind.to_owned(),
            bubbles,
            cancelable,
            interface: "Event",
            init: Vec::new(),
            trusted: true,
        }
    }

    /// A trusted UI event of DOM interface `interface`: bubbles, cancelable
    /// and composed, like mouse and keyboard events.
    pub fn ui(
        kind: &str,
        interface: &'static str,
        mut init: Vec<(&'static str, EventValue)>,
    ) -> Event {
        init.push(("composed", EventValue::Bool(true)));
        Event {
            interface,
            init,
            ..Event::new(kind, true, true)
        }
    }

    pub fn with_bubbles(mut self, bubbles: bool, cancelable: bool) -> Event {
        self.bubbles = bubbles;
        self.cancelable = cancelable;
        self
    }
}

/// Something scripts or default actions ask the browser to do.
#[derive(Clone, Debug, PartialEq)]
pub enum HostRequest {
    /// Load `url`: a link, a form submission, `location.href = ...`.
    /// `replace`: `location.replace()` (no new history entry).
    Navigate {
        url: Url,
        post: Option<forms::PostBody>,
        new_tab: bool,
        replace: bool,
    },
}

/// Where an event is dispatched.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EventTarget {
    Window,
    Document,
    Node(NodeId),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScriptError {
    /// Scripting is disabled; nothing ran.
    Disabled,
    Compile {
        url: String,
        message: String,
    },
    /// Uncaught exception.
    Runtime {
        url: String,
        message: String,
    },
}

impl fmt::Display for ScriptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScriptError::Disabled => write!(f, "scripting is disabled"),
            ScriptError::Compile { url, message } => write!(f, "{url}: syntax error: {message}"),
            ScriptError::Runtime { url, message } => write!(f, "{url}: uncaught: {message}"),
        }
    }
}

impl std::error::Error for ScriptError {}

/// A JavaScript engine bound to one document.
///
/// The shell calls these in this order for a page load:
/// 1. `execute` for each script (blocking ones during parse, then deferred),
/// 2. `dispatch_event(Document, "DOMContentLoaded")`, `dispatch_event(Window, "load")`,
/// 3. `run_microtasks` after every task,
/// 4. `run_timers` while `next_timer_due` is within the load budget.
///
/// After any call that changed `document.generation()`, the shell re-runs
/// style and layout before painting.
pub trait ScriptEngine {
    /// Run a classic script or module. `source` is the script text; `url` is
    /// used for error messages and as the module base.
    fn execute(
        &mut self,
        cx: &mut HostContext<'_>,
        source: &str,
        url: &str,
        kind: ScriptKind,
    ) -> Result<(), ScriptError>;

    /// Dispatch an event through capture and bubble phases. Returns `false`
    /// if a listener called `preventDefault()`.
    fn dispatch_event(
        &mut self,
        cx: &mut HostContext<'_>,
        target: EventTarget,
        event: &Event,
    ) -> Result<bool, ScriptError>;

    /// Drain the microtask queue (promise jobs, mutation observers).
    fn run_microtasks(&mut self, cx: &mut HostContext<'_>) -> Result<(), ScriptError>;

    /// Run every timer (`setTimeout`, `setInterval`, `requestAnimationFrame`)
    /// due at or before `cx.now_ms`. Returns how many callbacks ran.
    fn run_timers(&mut self, cx: &mut HostContext<'_>) -> Result<usize, ScriptError>;

    /// Time (ms since navigation start) of the earliest pending timer.
    fn next_timer_due(&self) -> Option<f64>;

    /// Requests scripts made since the last call (navigations from
    /// `location`, `form.submit()`, `a.click()`).
    fn take_requests(&mut self) -> Vec<HostRequest> {
        Vec::new()
    }

    /// Heap committed by the engine (0 while no isolate exists).
    fn heap_bytes(&mut self) -> usize {
        0
    }

    /// Collect garbage and give unused heap back (after load, and when a
    /// tab goes to the background).
    fn trim_memory(&mut self) {}
}

/// Engine used while V8 is not linked: runs no script, like a browser with
/// JavaScript disabled. Events and timers are no-ops because no listener or
/// timer can ever be registered.
#[derive(Default, Debug)]
pub struct ScriptingDisabled;

impl ScriptEngine for ScriptingDisabled {
    fn execute(
        &mut self,
        _cx: &mut HostContext<'_>,
        _source: &str,
        _url: &str,
        _kind: ScriptKind,
    ) -> Result<(), ScriptError> {
        Err(ScriptError::Disabled)
    }

    fn dispatch_event(
        &mut self,
        _cx: &mut HostContext<'_>,
        _target: EventTarget,
        _event: &Event,
    ) -> Result<bool, ScriptError> {
        Ok(true)
    }

    fn run_microtasks(&mut self, _cx: &mut HostContext<'_>) -> Result<(), ScriptError> {
        Ok(())
    }

    fn run_timers(&mut self, _cx: &mut HostContext<'_>) -> Result<usize, ScriptError> {
        Ok(0)
    }

    fn next_timer_due(&self) -> Option<f64> {
        None
    }
}

/// Options for [`create_engine`].
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// Wall-clock limit for one engine call (one script, event dispatch or
    /// timer round). Longer runs are terminated and reported as errors.
    pub call_budget_ms: u64,
    /// Print `console.*` output to stderr.
    pub console: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            call_budget_ms: 2000,
            console: std::env::var_os("HUGINN_JS_CONSOLE").is_some(),
        }
    }
}

/// A real script engine, or `None` when Huginn is built without one (the
/// `v8` feature). Creating the engine is cheap: the V8 isolate is created on
/// the first `execute`, so a page with no scripts never pays for it.
pub fn create_engine(options: EngineOptions) -> Option<Box<dyn ScriptEngine>> {
    #[cfg(feature = "v8")]
    {
        Some(Box::new(v8engine::V8Engine::new(options)))
    }
    #[cfg(not(feature = "v8"))]
    {
        let _ = options;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_classification_follows_html_rules() {
        let doc = huginn_dom::parse_html(
            r#"<script>a()</script>
               <script src="x.js" defer></script>
               <script src="y.js" async></script>
               <script type="module" src="m.js"></script>
               <script type="application/json">{}</script>
               <script nomodule src="old.js"></script>
               <script type="importmap" async>{}</script>
               <script type="importmap" src="map.json"></script>"#,
        );
        let base = Url::parse("http://h/dir/page.html").unwrap();
        let s = collect_scripts(&doc, &base);
        let summary: Vec<(ScriptKind, ScriptTiming, Option<&str>)> = s
            .iter()
            .map(|r| (r.kind, r.timing, r.src.as_ref().map(|u| u.as_str())))
            .collect();
        assert_eq!(
            summary,
            vec![
                (ScriptKind::Classic, ScriptTiming::Blocking, None),
                (
                    ScriptKind::Classic,
                    ScriptTiming::Defer,
                    Some("http://h/dir/x.js")
                ),
                (
                    ScriptKind::Classic,
                    ScriptTiming::Async,
                    Some("http://h/dir/y.js")
                ),
                (
                    ScriptKind::Module,
                    ScriptTiming::Defer,
                    Some("http://h/dir/m.js")
                ),
                (ScriptKind::ImportMap, ScriptTiming::Blocking, None),
            ]
        );
        assert_eq!(inline_source(&doc, s[0].node), "a()");
    }
}
