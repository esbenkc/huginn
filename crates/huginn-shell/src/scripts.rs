//! The script phase of a page load: run the parser-found scripts, fire
//! DOMContentLoaded and load, then let timers settle for a short virtual
//! time. Style and layout run on demand when scripts ask for geometry.
//!
//! Order (CONTRACTS.md, huginn-js): blocking scripts in document order, then
//! async ones, then deferred ones; a microtask checkpoint after each; then
//! DOMContentLoaded and load; then timers due within `SETTLE_MS` of virtual
//! time (no real waiting) or until the real-time budget is spent.

use std::borrow::Cow;
use std::time::{Duration, Instant};

use huginn_dom::{Document, NodeId};
use huginn_js::{
    Event, EventTarget, HostContext, LayoutHost, NodeGeometry, ScriptEngine, ScriptKind, ScriptRef,
    ScriptTiming,
};
use huginn_layout::{LayoutEngine, LayoutInput, LayoutTree, Rect, Size};
use huginn_net::{ImageStore, Loader, Url};
use huginn_style::{
    Color, ComputedStyle, LengthPercentage, LengthPercentageAuto, LineHeight, StyleEngine, StyleMap,
};

/// Virtual time timers may advance after `load`.
const SETTLE_MS: f64 = 500.0;
/// Real time the settle loop may take.
const SETTLE_BUDGET: Duration = Duration::from_millis(1500);
/// Real time the parser-found scripts may take together; later ones are
/// skipped (each single call is also cut off by the engine's watchdog).
const SCRIPTS_BUDGET: Duration = Duration::from_millis(4000);
/// Real time all on-demand style and layout passes may take together. A pass
/// that starts runs to the end; after the budget, scripts get stale geometry.
const LAYOUT_BUDGET: Duration = Duration::from_millis(400);

/// What the script phase did, for the JSON report.
#[derive(Default)]
pub struct ScriptReport {
    pub scripts: usize,
    pub errors: usize,
    pub layouts: usize,
}

/// Run the page's scripts. `sources[i]` is the fetched text and final URL of
/// `scripts[i]` for external scripts (None if the fetch failed).
#[allow(clippy::too_many_arguments)]
pub fn run(
    engine: &mut dyn ScriptEngine,
    doc: &mut Document,
    loader: &Loader,
    document_url: &Url,
    base: &Url,
    viewport: (f32, f32),
    start: Instant,
    scripts: &[ScriptRef],
    sources: &[Option<(Cow<'_, str>, String)>],
    layout: &mut PageLayout<'_>,
    settle: bool,
) -> ScriptReport {
    let mut report = ScriptReport::default();
    let elapsed_ms = || start.elapsed().as_secs_f64() * 1000.0;
    let mut cx = HostContext {
        document: doc,
        loader,
        document_url,
        base_url: base,
        viewport,
        now_ms: elapsed_ms(),
        layout: Some(layout),
        current_script: None,
    };
    let order = [
        ScriptTiming::Blocking,
        ScriptTiming::Async,
        ScriptTiming::Defer,
    ];
    let scripts_end = Instant::now() + SCRIPTS_BUDGET;
    for timing in order {
        for (s, src) in scripts
            .iter()
            .zip(sources)
            .filter(|(s, _)| s.timing == timing)
        {
            let inline;
            let (source, url): (&str, &str) = match (&s.src, src) {
                (Some(_), Some((text, url))) => (text, url),
                (Some(_), None) => {
                    let _ = engine.dispatch_event(
                        &mut cx,
                        EventTarget::Node(s.node),
                        &Event::new("error", false, false),
                    );
                    continue;
                }
                (None, _) => {
                    inline = huginn_js::inline_source(cx.document, s.node);
                    (&inline, document_url.as_str())
                }
            };
            if Instant::now() > scripts_end {
                eprintln!("huginn: js: script budget spent, skipping {url}");
                continue;
            }
            cx.now_ms = elapsed_ms();
            cx.current_script = Some(s.node);
            report.scripts += 1;
            if let Err(e) = engine.execute(&mut cx, source, url, s.kind) {
                report.errors += 1;
                eprintln!("huginn: js: {}", truncate(&e.to_string(), 400));
            }
            cx.current_script = None;
            let _ = engine.run_microtasks(&mut cx);
            if s.src.is_some() || s.kind == ScriptKind::Module {
                let _ = engine.dispatch_event(
                    &mut cx,
                    EventTarget::Node(s.node),
                    &Event::new("load", false, false),
                );
                let _ = engine.run_microtasks(&mut cx);
            }
        }
    }
    if report.scripts == 0 {
        return report;
    }

    cx.now_ms = elapsed_ms();
    let _ = engine.dispatch_event(
        &mut cx,
        EventTarget::Document,
        &Event::new("DOMContentLoaded", true, false),
    );
    let _ = engine.run_microtasks(&mut cx);
    let _ = engine.dispatch_event(
        &mut cx,
        EventTarget::Window,
        &Event::new("load", false, false),
    );
    let _ = engine.run_microtasks(&mut cx);

    // Settle: jump virtual time to each due timer, without real waiting. The
    // window skips this: its timers run in real time after load.
    let mut now = elapsed_ms();
    let settle_end = if settle {
        now + SETTLE_MS
    } else {
        f64::NEG_INFINITY
    };
    let real_end = Instant::now() + SETTLE_BUDGET;
    while let Some(due) = engine.next_timer_due() {
        if due > settle_end || Instant::now() > real_end {
            break;
        }
        now = now.max(due);
        cx.now_ms = now;
        if let Err(e) = engine.run_timers(&mut cx) {
            report.errors += 1;
            eprintln!("huginn: js: {}", truncate(&e.to_string(), 400));
        }
    }
    drop(cx);
    report.layouts = layout.passes;
    report
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Style and layout on demand for scripts, sharing the page's style engine,
/// images and layout engine (so fonts are set up once per page).
pub struct PageLayout<'a> {
    pub style: &'a StyleEngine,
    pub images: &'a ImageStore,
    pub base: &'a Url,
    pub viewport: Size,
    pub layout_engine: &'a mut Option<LayoutEngine>,
    styles: Option<(u64, StyleMap)>,
    /// Layout of the document at (generation, scroll generation).
    tree: Option<((u64, u64), LayoutTree)>,
    spent: Duration,
    pub passes: usize,
}

impl<'a> PageLayout<'a> {
    pub fn new(
        style: &'a StyleEngine,
        images: &'a ImageStore,
        base: &'a Url,
        viewport: Size,
        layout_engine: &'a mut Option<LayoutEngine>,
    ) -> PageLayout<'a> {
        PageLayout {
            style,
            images,
            base,
            viewport,
            layout_engine,
            styles: None,
            tree: None,
            spent: Duration::ZERO,
            passes: 0,
        }
    }

    fn may_recompute(&self) -> bool {
        self.spent < LAYOUT_BUDGET
    }

    fn ensure_styles(&mut self, doc: &Document) {
        let fresh = self
            .styles
            .as_ref()
            .is_some_and(|(g, _)| *g == doc.generation());
        if fresh || (self.styles.is_some() && !self.may_recompute()) {
            return;
        }
        let t = Instant::now();
        self.styles = Some((doc.generation(), self.style.compute(doc)));
        self.spent += t.elapsed();
    }

    fn ensure_layout(&mut self, doc: &Document) {
        let at = (doc.generation(), doc.scroll_generation());
        let fresh = self.tree.as_ref().is_some_and(|(g, _)| *g == at);
        if fresh
            || (self.tree.is_some() && !self.may_recompute())
            || (self.tree.is_none() && !self.may_recompute() && self.passes > 0)
        {
            return;
        }
        self.ensure_styles(doc);
        let Some((_, styles)) = &self.styles else {
            return;
        };
        let t = Instant::now();
        let engine = self.layout_engine.get_or_insert_with(LayoutEngine::new);
        let tree = engine.layout(&LayoutInput {
            document: doc,
            styles,
            images: self.images,
            base_url: self.base,
            viewport: self.viewport,
        });
        self.tree = Some((at, tree));
        self.passes += 1;
        self.spent += t.elapsed();
    }
}

fn union(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    Rect {
        x,
        y,
        width: a.right().max(b.right()) - x,
        height: a.bottom().max(b.bottom()) - y,
    }
}

/// Border box of everything `node` generated: its boxes, or for an inline
/// element its line fragments (CSSOM View: the union of the client rects).
/// Also the border widths and the scrolling area of its first box or
/// fragment (an inline element has no scrolling area).
fn node_geometry(tree: &LayoutTree, node: NodeId) -> Option<(Rect, [f32; 4], [f32; 2])> {
    let mut out: Option<(Rect, [f32; 4], [f32; 2])> = None;
    let mut add = |r: Rect, border: [f32; 4], scroll: [f32; 2]| match &mut out {
        Some((u, _, _)) => *u = union(*u, r),
        None => out = Some((r, border, scroll)),
    };
    for &b in tree.boxes_for_node(node) {
        let b = tree.get(b);
        add(
            b.rect,
            [b.border.left, b.border.top, b.border.right, b.border.bottom],
            [b.scroll_size.width, b.scroll_size.height],
        );
    }
    for f in tree.fragments_for_node(node) {
        let w = &f.style.border_width;
        add(f.rect, [w.left, w.top, w.right, w.bottom], [0.0, 0.0]);
    }
    out
}

impl LayoutHost for PageLayout<'_> {
    fn geometry(&mut self, doc: &Document, node: NodeId) -> Option<NodeGeometry> {
        self.ensure_layout(doc);
        let (_, tree) = self.tree.as_ref()?;
        let (r, border, scroll_size) = node_geometry(tree, node)?;
        Some(NodeGeometry {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
            border,
            scroll_size,
        })
    }

    fn computed_value(&mut self, doc: &Document, node: NodeId, property: &str) -> String {
        self.ensure_styles(doc);
        let Some((_, styles)) = &self.styles else {
            return String::new();
        };
        match styles.get(node) {
            Some(s) => {
                let used = self
                    .tree
                    .as_ref()
                    .and_then(|(_, tree)| node_geometry(tree, node).map(|(r, ..)| r));
                computed_value(s, property, used)
            }
            // Not rendered: `display: none` here or on an ancestor.
            None if property == "display" => "none".into(),
            None => String::new(),
        }
    }
}

/// `Debug` names of the keyword enums are their CSS keywords in CamelCase.
fn keyword(v: &impl std::fmt::Debug) -> String {
    let name = format!("{v:?}");
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn px(v: f32) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}px")
    } else {
        format!("{v}px")
    }
}

fn lp(v: LengthPercentage) -> String {
    match v {
        LengthPercentage::Px(p) => px(p),
        LengthPercentage::Percent(p) => format!("{p}%"),
        // calc(): the px part, as far as it resolves without a basis.
        other => px(other.resolve(None)),
    }
}

fn lpa(v: LengthPercentageAuto, auto: &str) -> String {
    match v {
        LengthPercentageAuto::Auto => auto.into(),
        LengthPercentageAuto::Px(p) => px(p),
        LengthPercentageAuto::Percent(p) => format!("{p}%"),
        other => other.resolve(None).map_or_else(String::new, px),
    }
}

fn color(c: Color) -> String {
    if c.a == 255 {
        format!("rgb({}, {}, {})", c.r, c.g, c.b)
    } else {
        let a = (f32::from(c.a) / 255.0 * 1000.0).round() / 1000.0;
        format!("rgba({}, {}, {}, {a})", c.r, c.g, c.b)
    }
}

/// `getComputedStyle` serialization for the properties scripts commonly read.
/// `used` is the border box from layout, for `width`/`height`.
fn computed_value(s: &ComputedStyle, prop: &str, used: Option<Rect>) -> String {
    let edge = |e: &huginn_style::Edges<f32>, side: &str| match side {
        "top" => e.top,
        "right" => e.right,
        "bottom" => e.bottom,
        _ => e.left,
    };
    match prop {
        "display" => keyword(&s.display),
        "position" => keyword(&s.position),
        "float" => keyword(&s.float),
        "clear" => keyword(&s.clear),
        "box-sizing" => keyword(&s.box_sizing),
        "visibility" => keyword(&s.visibility),
        "overflow" if s.overflow_x == s.overflow_y => keyword(&s.overflow_x),
        "overflow" => format!("{} {}", keyword(&s.overflow_x), keyword(&s.overflow_y)),
        "overflow-x" => keyword(&s.overflow_x),
        "overflow-y" => keyword(&s.overflow_y),
        "opacity" => format!("{}", s.opacity),
        "z-index" => s.z_index.map_or("auto".into(), |z| z.to_string()),
        "color" => color(s.color),
        "background-color" => color(s.background_color),
        "font-size" => px(s.font_size),
        "font-family" => s.font_family.to_string(),
        "font-weight" => format!("{}", s.font_weight),
        "font-style" => keyword(&s.font_style),
        "line-height" => match s.line_height {
            LineHeight::Normal => "normal".into(),
            _ => px(s.line_height_px()),
        },
        "text-align" => keyword(&s.text_align),
        "text-transform" => keyword(&s.text_transform),
        "white-space" => keyword(&s.white_space),
        "letter-spacing" => {
            if s.letter_spacing == 0.0 {
                "normal".into()
            } else {
                px(s.letter_spacing)
            }
        }
        "flex-direction" => keyword(&s.flex_direction),
        "flex-wrap" => keyword(&s.flex_wrap),
        "flex-grow" => format!("{}", s.flex_grow),
        "flex-shrink" => format!("{}", s.flex_shrink),
        "width" => match (used, s.box_sizing) {
            (Some(r), huginn_style::BoxSizing::BorderBox) => px(r.width),
            (Some(r), _) => px((r.width
                - s.border_width.left
                - s.border_width.right
                - s.padding.left.resolve(None)
                - s.padding.right.resolve(None))
            .max(0.0)),
            (None, _) => lpa(s.width, "auto"),
        },
        "height" => match (used, s.box_sizing) {
            (Some(r), huginn_style::BoxSizing::BorderBox) => px(r.height),
            (Some(r), _) => px((r.height
                - s.border_width.top
                - s.border_width.bottom
                - s.padding.top.resolve(None)
                - s.padding.bottom.resolve(None))
            .max(0.0)),
            (None, _) => lpa(s.height, "auto"),
        },
        "min-width" => lpa(s.min_width, "auto"),
        "min-height" => lpa(s.min_height, "auto"),
        "max-width" => lpa(s.max_width, "none"),
        "max-height" => lpa(s.max_height, "none"),
        "top" => lpa(s.inset.top, "auto"),
        "right" => lpa(s.inset.right, "auto"),
        "bottom" => lpa(s.inset.bottom, "auto"),
        "left" => lpa(s.inset.left, "auto"),
        _ => {
            if let Some(side) = prop.strip_prefix("margin-") {
                let m = &s.margin;
                let v = match side {
                    "top" => m.top,
                    "right" => m.right,
                    "bottom" => m.bottom,
                    "left" => m.left,
                    _ => return String::new(),
                };
                lpa(v, "auto")
            } else if let Some(side) = prop.strip_prefix("padding-") {
                let p = &s.padding;
                let v = match side {
                    "top" => p.top,
                    "right" => p.right,
                    "bottom" => p.bottom,
                    "left" => p.left,
                    _ => return String::new(),
                };
                lp(v)
            } else if let Some(side) = prop
                .strip_prefix("border-")
                .and_then(|r| r.strip_suffix("-width"))
            {
                px(edge(&s.border_width, side))
            } else if let Some(side) = prop
                .strip_prefix("border-")
                .and_then(|r| r.strip_suffix("-color"))
            {
                let c = &s.border_color;
                color(match side {
                    "top" => c.top,
                    "right" => c.right,
                    "bottom" => c.bottom,
                    _ => c.left,
                })
            } else {
                String::new()
            }
        }
    }
}
