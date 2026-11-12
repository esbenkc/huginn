//! Box generation: DOM + styles -> layout tree (`tree.rs`), with anonymous
//! boxes, inline formatting contexts, tables and list markers. Then layout
//! and emission (`emit.rs`).

use std::sync::Arc;

use huginn_dom::{NodeData, NodeId, local_name};
use huginn_style::{
    ComputedStyle, Content, ContentItem, Display, LengthPercentageAuto, ListStylePosition,
    ListStyleType, Position, PseudoElement, TextTransform,
};
use taffy::prelude as tf;
use taffy::{AvailableSpace, Cache, DetailedLayoutInfo, Layout};

use crate::convert::{Decor, fade, optical_sizing, taffy_style, text_style};
use crate::inline::{Brush, Ifc, Item as IItem, Span, span_has_edges, vertical_align};
use crate::table::{Cell, Row, Table};
use crate::tree::{Kind, LNode, LTree, Marker, ix};
use crate::*;

/// An element that generates a box: a DOM element, a `::before`/`::after`,
/// or the anonymous replaced box of an image in a `content` list.
#[derive(Clone)]
enum El {
    Dom(NodeId),
    /// A `::before`/`::after` box (its owning element).
    Pseudo(NodeId),
    /// A `content: url(...)` image: an anonymous inline replaced box inside
    /// the pseudo-element, with the image's natural size (as in Blink).
    Image(DecodedImage),
}

/// A child of a block container, classified.
enum Item {
    Block(El, Arc<ComputedStyle>),
    Inline(Inline),
}

enum Inline {
    Text(NodeId),
    /// Inline element: a style span.
    Element(NodeId, Arc<ComputedStyle>),
    /// Atomic inline: inline-block, inline-flex, inline-table, replaced.
    Atomic(El, Arc<ComputedStyle>),
    /// `::before`/`::after` with `display: inline` (owner element).
    Generated(NodeId, Arc<ComputedStyle>),
    Br(Arc<ComputedStyle>),
    /// `list-style-position: inside` marker text.
    Marker(String, Arc<ComputedStyle>),
    /// Absolutely positioned or fixed box amid inline content.
    Oof(El, Arc<ComputedStyle>),
}

/// Flattened inline content, in document order.
enum Ev {
    Open(Option<NodeId>, Arc<ComputedStyle>),
    Close,
    Text(NodeId),
    Str(String, Arc<ComputedStyle>),
    Br(Arc<ComputedStyle>),
    Atomic(El, Arc<ComputedStyle>),
    Oof(El, Arc<ComputedStyle>),
}

pub(crate) struct Builder<'a, 'e> {
    input: &'a LayoutInput<'a>,
    engine: &'e mut LayoutEngine,
    pub tree: LTree,
    /// Per node: an inline element contains block-level descendants.
    contains_block: Vec<bool>,
}

pub(crate) fn layout(engine: &mut LayoutEngine, input: &LayoutInput<'_>) -> LayoutTree {
    let mut out = LayoutTree {
        viewport: input.viewport,
        document_size: input.viewport,
        text_index: engine.text_index.then(crate::TextIndex::default),
        ..Default::default()
    };
    let doc = input.document;
    let Some(html) = doc.document_element() else {
        return out;
    };
    let Some(html_style) = input.styles.get(html).cloned() else {
        return out;
    };

    let mut b = Builder {
        input,
        engine,
        tree: LTree { nodes: Vec::new() },
        contains_block: compute_contains_block(input),
    };
    let root = b.build_block(El::Dom(html), html_style, Decor::default());
    let vp = input.viewport;
    b.tree.compute_root(
        root,
        tf::Size {
            width: AvailableSpace::Definite(vp.width),
            height: AvailableSpace::Definite(vp.height),
        },
    );
    crate::emit::emit(&mut b.tree, root, input, &mut out);
    if let Some(index) = &mut out.text_index {
        // Kept for the page's lifetime: no growth slack.
        index.text.shrink_to_fit();
        index.runs.shrink_to_fit();
        index.clusters.shrink_to_fit();
    }
    out
}

/// Bottom-up pass: which nodes have block-level descendants reachable through
/// inline (or `display: contents`) elements.
fn compute_contains_block(input: &LayoutInput<'_>) -> Vec<bool> {
    let doc = input.document;
    let mut out = vec![false; doc.len()];
    let order: Vec<NodeId> = doc.shadow_including_descendants(NodeId::DOCUMENT).collect();
    for &n in order.iter().rev() {
        let mut any = false;
        for c in box_children(doc, n) {
            let Some(cs) = input.styles.get(c) else {
                continue;
            };
            any |= match cs.display {
                Display::Inline | Display::Contents => out[c.index()],
                d if d.is_inline_level() => false,
                _ => !is_out_of_flow(cs),
            };
            if any {
                break;
            }
        }
        out[n.index()] = any;
    }
    out
}

/// The children of `node` that can generate boxes: its flat tree children,
/// except that a closed `<details>` shows only its summary (HTML 15.3.4:
/// the details content slot is hidden until the element is open).
fn box_children(doc: &huginn_dom::Document, node: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    let closed = doc.element(node).is_some_and(|e| {
        e.is_html(&local_name!("details")) && e.attr(&local_name!("open")).is_none()
    });
    let summary = if closed {
        doc.children(node).find(|&c| {
            doc.element(c)
                .is_some_and(|e| e.is_html(&local_name!("summary")))
        })
    } else {
        None
    };
    doc.flat_children(node)
        .filter(move |&c| !closed || Some(c) == summary)
}

/// Elements with HTML's button layout: `<button>` and `<input>` in the
/// Submit Button, Reset Button or Button state (HTML 15.5.4, 15.5.15).
fn is_button(doc: &huginn_dom::Document, node: NodeId) -> bool {
    let Some(el) = doc.element(node) else {
        return false;
    };
    if el.is_html(&local_name!("button")) {
        return true;
    }
    el.is_html(&local_name!("input"))
        && el.attr(&local_name!("type")).is_some_and(|t| {
            ["submit", "reset", "button"]
                .iter()
                .any(|b| t.eq_ignore_ascii_case(b))
        })
}

fn is_out_of_flow(s: &ComputedStyle) -> bool {
    matches!(s.position, Position::Absolute | Position::Fixed)
}

/// Does this text survive white-space processing? Collapsible whitespace
/// alone makes no line box (CSS Text 3, section 4.1.1).
fn text_is_visible(t: &str, ws: huginn_style::WhiteSpace) -> bool {
    use huginn_style::WhiteSpace as W;
    match ws {
        W::Normal | W::Nowrap => t.chars().any(|c| !c.is_ascii_whitespace()),
        W::PreLine => t.chars().any(|c| c == '\n' || !c.is_ascii_whitespace()),
        W::Pre | W::PreWrap | W::BreakSpaces => !t.is_empty(),
    }
}

fn new_node(
    node: Option<NodeId>,
    style: Arc<ComputedStyle>,
    kind: Kind,
    ts: tree::TStyle,
    children: Vec<tf::NodeId>,
) -> LNode {
    LNode {
        node,
        style,
        kind,
        children,
        ts,
        cache: Cache::new(),
        layout: Layout::new(),
        hoisted: Vec::new(),
        detailed: DetailedLayoutInfo::None,
        marker: None,
        content_dy: 0.0,
        border_override: None,
        measured_baselines: None,
    }
}

impl Builder<'_, '_> {
    fn el_node(el: &El) -> Option<NodeId> {
        match el {
            El::Dom(n) => Some(*n),
            El::Pseudo(..) | El::Image(_) => None,
        }
    }

    // ----- classification ---------------------------------------------------

    fn pseudo_item(&self, owner: NodeId, which: PseudoElement, out: &mut Vec<Item>) {
        let Some(ps) = self.input.styles.pseudo(owner, which) else {
            return;
        };
        // Out-of-flow and floating pseudo-elements are blockified (CSS
        // Display 3, section 2.7); the style keeps the declared display.
        let item = if is_out_of_flow(ps) || ps.float != huginn_style::Float::None {
            Item::Block(El::Pseudo(owner), ps.clone())
        } else if ps.display == Display::Inline {
            Item::Inline(Inline::Generated(owner, ps.clone()))
        } else if ps.display.is_inline_level() {
            Item::Inline(Inline::Atomic(El::Pseudo(owner), ps.clone()))
        } else {
            Item::Block(El::Pseudo(owner), ps.clone())
        };
        out.push(item);
    }

    fn collect(&self, node: NodeId, out: &mut Vec<Item>) {
        let doc = self.input.document;
        let styles = self.input.styles;
        self.pseudo_item(node, PseudoElement::Before, out);
        self.input_text(node, out);
        for c in box_children(doc, node) {
            match &doc.node(c).data {
                NodeData::Text(_) => out.push(Item::Inline(Inline::Text(c))),
                NodeData::Element(e) => {
                    let Some(cs) = styles.get(c) else { continue };
                    if cs.display == Display::Contents {
                        self.collect(c, out);
                        continue;
                    }
                    if e.is_html(&local_name!("br")) {
                        out.push(Item::Inline(Inline::Br(cs.clone())));
                        continue;
                    }
                    let item = if !cs.display.is_inline_level() {
                        Item::Block(El::Dom(c), cs.clone())
                    } else if cs.display != Display::Inline || self.replaced(c).is_some() {
                        Item::Inline(Inline::Atomic(El::Dom(c), cs.clone()))
                    } else if self.contains_block[c.index()] {
                        // Block-in-inline: treat the inline as a block.
                        Item::Block(El::Dom(c), cs.clone())
                    } else {
                        Item::Inline(Inline::Element(c, cs.clone()))
                    };
                    out.push(item);
                }
                _ => {}
            }
        }
        self.pseudo_item(node, PseudoElement::After, out);
    }

    /// The text an `<input>` shows: its value, a button label, or its
    /// placeholder (in Chrome's placeholder grey). A drop-down `<select>`
    /// shows its selected option's label (the UA sheet hides the options).
    fn input_text(&self, node: NodeId, out: &mut Vec<Item>) {
        let Some(el) = self.input.document.element(node) else {
            return;
        };
        if el.is_html(&local_name!("select")) {
            return self.select_text(node, out);
        }
        if !el.is_html(&local_name!("input")) {
            return;
        }
        let Some(style) = self.input.styles.get(node) else {
            return;
        };
        let ty = el
            .attr(&local_name!("type"))
            .unwrap_or("text")
            .to_ascii_lowercase();
        let value = el.attr(&local_name!("value")).filter(|v| !v.is_empty());
        let (text, placeholder) = match &*ty {
            "hidden" | "checkbox" | "radio" | "image" | "file" | "range" | "color" => return,
            "submit" => (value.unwrap_or("Submit").to_owned(), false),
            "reset" => (value.unwrap_or("Reset").to_owned(), false),
            "button" => (value.unwrap_or("").to_owned(), false),
            "password" if value.is_some() => {
                ("•".repeat(value.unwrap_or("").chars().count()), false)
            }
            _ => match value {
                Some(v) => (v.to_owned(), false),
                None => (
                    el.attr(&local_name!("placeholder"))
                        .unwrap_or("")
                        .to_owned(),
                    true,
                ),
            },
        };
        if text.is_empty() {
            return;
        }
        let mut st = ComputedStyle::inherit_from(style);
        st.content = Content::Text(text.into());
        st.white_space = huginn_style::WhiteSpace::Pre;
        if placeholder {
            st.color = huginn_style::Color::rgb(0x75, 0x75, 0x75);
        }
        out.push(Item::Inline(Inline::Generated(node, Arc::new(st))));
    }

    /// The label of a single-choice `<select>`'s selected option: the first
    /// option with `selected`, else the first option.
    fn select_text(&self, node: NodeId, out: &mut Vec<Item>) {
        let doc = self.input.document;
        let Some(style) = self.input.styles.get(node) else {
            return;
        };
        if doc.attribute(node, &local_name!("multiple")).is_some() {
            return;
        }
        let options = || {
            doc.descendants(node).filter(|&o| {
                doc.element(o)
                    .is_some_and(|e| e.is_html(&local_name!("option")))
            })
        };
        let Some(option) = options()
            .find(|&o| doc.attribute(o, &local_name!("selected")).is_some())
            .or_else(|| options().next())
        else {
            return;
        };
        let label = doc
            .attribute(option, &local_name!("label"))
            .map(str::to_owned)
            .unwrap_or_else(|| {
                doc.text_content(option)
                    .split_ascii_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            });
        if label.is_empty() {
            return;
        }
        let mut st = ComputedStyle::inherit_from(style);
        st.content = Content::Text(label.into());
        st.white_space = huginn_style::WhiteSpace::Pre;
        out.push(Item::Inline(Inline::Generated(node, Arc::new(st))));
    }

    /// Replaced elements: intrinsic size and decoded image, if any.
    fn replaced(&self, node: NodeId) -> Option<(f32, f32, Option<DecodedImage>)> {
        let el = self.input.document.element(node)?;
        if el.name.ns != huginn_dom::ns!(html) && el.name.local != local_name!("svg") {
            return None;
        }
        let attr_px = |name: &huginn_dom::LocalName| {
            el.attr(name)
                .and_then(|v| v.trim().trim_end_matches("px").parse::<f32>().ok())
        };
        let (aw, ah) = (
            attr_px(&local_name!("width")),
            attr_px(&local_name!("height")),
        );
        match &*el.name.local {
            "img" => {
                // The shell stores the srcset/<picture> choice per element;
                // plain `src` images are stored by URL.
                let img = self
                    .input
                    .images
                    .get_for_node(node.to_raw())
                    .cloned()
                    .or_else(|| {
                        el.attr(&local_name!("src"))
                            .and_then(|src| huginn_net::resolve_url(self.input.base_url, src))
                            .and_then(|u| self.input.images.get(u.as_str()).cloned())
                    });
                let (iw, ih) = img
                    .as_ref()
                    .map_or((0.0, 0.0), |i| (i.width() as f32, i.height() as f32));
                let (w, h) = match (aw, ah) {
                    (Some(w), Some(h)) => (w, h),
                    (Some(w), None) if iw > 0.0 => (w, w * ih / iw),
                    (None, Some(h)) if ih > 0.0 => (h * iw / ih, h),
                    _ => (iw, ih),
                };
                Some((w, h, img))
            }
            "svg" => {
                // Inline <svg>, rendered to an image by the shell (huginn-net).
                let img = self.input.images.get_for_node(node.to_raw()).cloned();
                let (iw, ih) = img
                    .as_ref()
                    .map_or((300.0, 150.0), |i| (i.width() as f32, i.height() as f32));
                Some((aw.unwrap_or(iw), ah.unwrap_or(ih), img))
            }
            "video" => {
                // No video decoding: the element shows its poster frame (the
                // shell stores it per element), whose size is the natural
                // size. Width/height attributes win; an attribute alone keeps
                // the poster's aspect ratio.
                let Some(img) = self.input.images.get_for_node(node.to_raw()).cloned() else {
                    return Some((aw.unwrap_or(300.0), ah.unwrap_or(150.0), None));
                };
                let (iw, ih) = (img.width() as f32, img.height() as f32);
                let (w, h) = match (aw, ah) {
                    (Some(w), Some(h)) => (w, h),
                    (Some(w), None) => (w, w * ih / iw),
                    (None, Some(h)) => (h * iw / ih, h),
                    (None, None) => (iw, ih),
                };
                Some((w, h, Some(img)))
            }
            "canvas" | "iframe" | "embed" | "object" => {
                Some((aw.unwrap_or(300.0), ah.unwrap_or(150.0), None))
            }
            _ => None,
        }
    }

    // ----- box building -----------------------------------------------------

    /// A block-level box (or the box of an atomic inline).
    fn build_block(&mut self, el: El, style: Arc<ComputedStyle>, decor: Decor) -> tf::NodeId {
        let node = Self::el_node(&el);
        if let El::Image(img) = el {
            let (w, h) = (img.width() as f32, img.height() as f32);
            return self.replaced_box(None, style, (w, h, Some(img)));
        }
        if let Some(n) = node
            && let Some(r) = self.replaced(n)
        {
            return self.replaced_box(Some(n), style, r);
        }
        if matches!(style.display, Display::Table | Display::InlineTable)
            && let Some(n) = node
        {
            return self.build_table(n, style, decor);
        }
        let decor = decor.extend(&style);
        let mut items = Vec::new();
        match el {
            El::Dom(n) => self.collect(n, &mut items),
            El::Pseudo(owner) => {
                if matches!(&style.content, Content::Text(t) if !t.is_empty())
                    || matches!(style.content, Content::Items(_))
                {
                    // The content is inline content of the pseudo's box: an
                    // anonymous inline that only inherits from it (no second
                    // copy of its margins, borders, background or opacity).
                    let mut anon = ComputedStyle::inherit_from(&style);
                    anon.content = style.content.clone();
                    items.push(Item::Inline(Inline::Generated(owner, Arc::new(anon))));
                }
            }
            El::Image(_) => unreachable!("handled above"),
        }
        // List marker, inside: inline text at the start.
        let marker = self.marker_text(node, &style);
        if let Some(text) = &marker
            && style.list_style_position == ListStylePosition::Inside
        {
            items.insert(0, Item::Inline(Inline::Marker(text.clone(), style.clone())));
        }
        let block_container = !matches!(
            style.display,
            Display::Flex
                | Display::InlineFlex
                | Display::Grid
                | Display::InlineGrid
                | Display::TableRow
        );
        let id = if block_container && items.iter().all(|i| matches!(i, Item::Inline(_))) {
            let inlines: Vec<Inline> = items
                .into_iter()
                .filter_map(|i| match i {
                    Item::Inline(x) => Some(x),
                    Item::Block(..) => None,
                })
                .collect();
            if self.has_content(&inlines) {
                self.text_box(node, style.clone(), inlines, decor)
            } else {
                self.container(node, style.clone(), Vec::new())
            }
        } else {
            let mut children = Vec::new();
            let mut run: Vec<Inline> = Vec::new();
            for item in items {
                match item {
                    Item::Inline(i) => run.push(i),
                    Item::Block(e, s) => {
                        if is_out_of_flow(&s) && !run.is_empty() {
                            // Out-of-flow boxes do not split inline content:
                            // the line layout gives them their static position.
                            run.push(Inline::Oof(e, s));
                            continue;
                        }
                        self.flush_run(&mut run, &style, decor, &mut children);
                        let c = self.build_block(e, s, decor);
                        children.push(c);
                    }
                }
            }
            self.flush_run(&mut run, &style, decor, &mut children);
            self.container(node, style.clone(), children)
        };
        if let Some(text) = marker
            && style.list_style_position == ListStylePosition::Outside
        {
            let m = self.shape_marker(&text, &style);
            self.tree.node_mut(id).marker = m;
        }
        if let Some(n) = node
            && matches!(
                style.display,
                Display::Block | Display::InlineBlock | Display::FlowRoot
            )
            && is_button(self.input.document, n)
        {
            self.wrap_button_content(id);
        }
        id
    }

    /// HTML 15.5.4 (button layout): the content of a button goes in an
    /// anonymous button content box, centered vertically in the button when
    /// it fits, else at the top. Blink makes the button a column flexbox
    /// whose one anonymous item has auto block margins: auto margins take
    /// only positive free space, so overflowing content stays at the top.
    /// The button's baseline is then its content's first baseline.
    fn wrap_button_content(&mut self, id: tf::NodeId) {
        let outer = self.tree.node_mut(id);
        if outer.children.is_empty() && !matches!(outer.kind, Kind::Ifc(_)) {
            // No content: nothing to center, keep the inline-block baseline.
            return;
        }
        let kind = std::mem::replace(&mut outer.kind, Kind::Container);
        let children = std::mem::take(&mut outer.children);
        let mut anon = ComputedStyle::inherit_from(&outer.style);
        anon.display = Display::Block;
        let mut ts = taffy_style(&anon, matches!(kind, Kind::Ifc(_)));
        ts.margin.top = tf::LengthPercentageAuto::auto();
        ts.margin.bottom = tf::LengthPercentageAuto::auto();
        let inner = self
            .tree
            .push(new_node(None, Arc::new(anon), kind, ts, children));
        let outer = self.tree.node_mut(id);
        outer.children = vec![inner];
        outer.ts.display = tf::Display::Flex;
        outer.ts.flex_direction = taffy::FlexDirection::Column;
        outer.ts.flex_wrap = taffy::FlexWrap::NoWrap;
        outer.ts.align_items = taffy::AlignItems::STRETCH;
        outer.ts.justify_content = taffy::AlignContent::NORMAL;
    }

    fn flush_run(
        &mut self,
        run: &mut Vec<Inline>,
        parent: &ComputedStyle,
        decor: Decor,
        children: &mut Vec<tf::NodeId>,
    ) {
        if !run.is_empty() && self.has_content(run) {
            let mut anon = ComputedStyle::inherit_from(parent);
            anon.display = Display::Block;
            let inlines = std::mem::take(run);
            let b = self.text_box(None, Arc::new(anon), inlines, decor);
            children.push(b);
        }
        // No line box (only collapsible white space): the out-of-flow boxes
        // in the run stay in the block flow.
        for i in run.drain(..) {
            if let Inline::Oof(e, s) = i {
                let c = self.build_block(e, s, decor);
                children.push(c);
            }
        }
    }

    fn container(
        &mut self,
        node: Option<NodeId>,
        style: Arc<ComputedStyle>,
        children: Vec<tf::NodeId>,
    ) -> tf::NodeId {
        let ts = taffy_style(&style, false);
        self.tree
            .push(new_node(node, style, Kind::Container, ts, children))
    }

    fn replaced_box(
        &mut self,
        node: Option<NodeId>,
        style: Arc<ComputedStyle>,
        r: (f32, f32, Option<DecodedImage>),
    ) -> tf::NodeId {
        let (w, h, img) = r;
        let mut ts = taffy_style(&style, true);
        ts.item_is_replaced = true;
        if ts.aspect_ratio.is_none() && w > 0.0 && h > 0.0 {
            ts.aspect_ratio = Some(w / h);
        }
        self.tree.push(new_node(
            node,
            style,
            Kind::Replaced {
                width: w,
                height: h,
                image: img,
            },
            ts,
            Vec::new(),
        ))
    }

    /// Does this inline content produce anything visible?
    fn has_content(&self, items: &[Inline]) -> bool {
        let doc = self.input.document;
        items.iter().any(|i| match i {
            Inline::Text(n) => doc.text(*n).is_some_and(|t| {
                let ws = doc
                    .flat_parent(*n)
                    .and_then(|p| self.input.styles.get(p))
                    .map(|s| s.white_space);
                text_is_visible(t, ws.unwrap_or_default())
            }),
            Inline::Element(n, s) => {
                let (a, b) = span_has_edges(s);
                if a || b {
                    return true;
                }
                let mut sub = Vec::new();
                self.collect(*n, &mut sub);
                let sub: Vec<Inline> = sub
                    .into_iter()
                    .filter_map(|x| match x {
                        Item::Inline(i) => Some(i),
                        Item::Block(..) => None,
                    })
                    .collect();
                self.has_content(&sub)
            }
            Inline::Atomic(..) | Inline::Br(_) | Inline::Marker(..) => true,
            // Out of flow: no line box of its own.
            Inline::Oof(..) => false,
            // `content: " "` (clearfix pseudo-elements) collapses away like
            // whitespace-only text: no line box (CSS 2.1, section 9.4.2).
            Inline::Generated(_, s) => match &s.content {
                Content::Text(t) if text_is_visible(t, s.white_space) => true,
                Content::Items(_) => true,
                _ => {
                    let (a, b) = span_has_edges(s);
                    a || b
                }
            },
        })
    }

    fn flatten(&self, items: Vec<Inline>, out: &mut Vec<Ev>) {
        for i in items {
            match i {
                Inline::Text(n) => out.push(Ev::Text(n)),
                Inline::Generated(owner, s) => {
                    out.push(Ev::Open(Some(owner), s.clone()));
                    match &s.content {
                        Content::Text(t) if !t.is_empty() => {
                            out.push(Ev::Str(t.to_string(), s.clone()))
                        }
                        Content::Items(list) => {
                            for item in list.iter() {
                                match item {
                                    ContentItem::Text(t) => {
                                        out.push(Ev::Str(t.to_string(), s.clone()))
                                    }
                                    // An image that did not load shows nothing.
                                    ContentItem::Image(url) => {
                                        if let Some(img) = self.input.images.get(url) {
                                            let anon = Arc::new(ComputedStyle::inherit_from(&s));
                                            out.push(Ev::Atomic(El::Image(img.clone()), anon));
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    out.push(Ev::Close);
                }
                Inline::Br(s) => out.push(Ev::Br(s)),
                Inline::Marker(t, s) => out.push(Ev::Str(t, s)),
                Inline::Atomic(e, s) => out.push(Ev::Atomic(e, s)),
                Inline::Oof(e, s) => out.push(Ev::Oof(e, s)),
                Inline::Element(n, s) => {
                    out.push(Ev::Open(Some(n), s));
                    let mut sub = Vec::new();
                    self.collect(n, &mut sub);
                    let sub: Vec<Inline> = sub
                        .into_iter()
                        .map(|x| match x {
                            Item::Inline(i) => i,
                            Item::Block(e, s) if is_out_of_flow(&s) => Inline::Oof(e, s),
                            // contains_block said no blocks; a stray one becomes atomic.
                            Item::Block(e, s) => Inline::Atomic(e, s),
                        })
                        .collect();
                    self.flatten(sub, out);
                    out.push(Ev::Close);
                }
            }
        }
    }

    /// A box holding an inline formatting context.
    fn text_box(
        &mut self,
        node: Option<NodeId>,
        style: Arc<ComputedStyle>,
        items: Vec<Inline>,
        decor: Decor,
    ) -> tf::NodeId {
        let mut evs = Vec::new();
        self.flatten(items, &mut evs);

        // Atomic inlines and out-of-flow boxes are box children of the IFC.
        let mut children = Vec::new();
        let mut atomic_ids = Vec::new();
        for ev in &evs {
            if let Ev::Atomic(e, s) | Ev::Oof(e, s) = ev {
                let c = self.build_block(e.clone(), s.clone(), Decor::default());
                children.push(c);
                atomic_ids.push(c);
            }
        }

        // Spans without text. Parley sizes a line from the span boxes of the
        // text on it, so these would not count. In standards mode they do
        // (CSS 2 §10.8): their boxes go into the atomic inlines they hold.
        let doc = self.input.document;
        let mut textless = vec![false; evs.len()];
        if doc.quirks_mode() != huginn_dom::QuirksMode::Quirks {
            // (event index of the open span, it has text)
            let mut open: Vec<(usize, bool)> = Vec::new();
            for (i, ev) in evs.iter().enumerate() {
                let has_text = match ev {
                    Ev::Open(..) => {
                        open.push((i, false));
                        continue;
                    }
                    Ev::Close => {
                        if let Some((o, has)) = open.pop() {
                            textless[o] = !has;
                            if has && let Some(p) = open.last_mut() {
                                p.1 = true;
                            }
                        }
                        continue;
                    }
                    Ev::Text(n) => doc
                        .text(*n)
                        .is_some_and(|t| t.chars().any(|c| !c.is_ascii_whitespace())),
                    Ev::Str(t, _) => !t.is_empty(),
                    Ev::Br(_) => true,
                    Ev::Atomic(..) | Ev::Oof(..) => false,
                };
                if has_text && let Some(p) = open.last_mut() {
                    p.1 = true;
                }
            }
        }

        // Resolve font families first (loads font files lazily) and install
        // fallbacks for the text.
        let fonts = &mut self.engine.fonts;
        let root_fam = fonts.family_list(&style.font_family);
        let root_lh = fonts.line_height(&style, &root_fam);
        let sel = |fonts: &mut crate::fonts::Fonts, s: &ComputedStyle| {
            let fam = fonts.family_list(&s.font_family);
            let lh = fonts.line_height(s, &fam);
            Some((fam, lh))
        };
        let mut struts: Vec<Option<(f32, f32)>> = vec![None; evs.len()];
        let ev_fams: Vec<Option<(crate::fonts::FamilyList, (parley::LineHeight, f32))>> = evs
            .iter()
            .enumerate()
            .map(|(i, ev)| match ev {
                Ev::Open(_, s) => {
                    let r = sel(fonts, s);
                    if textless[i]
                        && let Some((fam, lh)) = &r
                    {
                        struts[i] = fonts.span_box(s, fam, lh.0);
                    }
                    r
                }
                Ev::Br(s) => sel(fonts, s),
                Ev::Str(t, s) => {
                    fonts.cover(t);
                    sel(fonts, s)
                }
                Ev::Text(n) => {
                    fonts.cover(doc.text(*n).unwrap_or(""));
                    None
                }
                Ev::Close | Ev::Atomic(..) | Ev::Oof(..) => None,
            })
            .collect();

        let mut spans = vec![Span {
            node,
            style: style.clone(),
            parent: 0,
            baseline_shift: root_lh.1,
            strut: None,
            opacity: 1.0,
        }];
        let mut items: Vec<IItem> = Vec::new();
        let root_opsz = optical_sizing(&style);
        let root_ts = text_style(&style, &decor, &root_fam, root_lh.0, 0, true, &root_opsz);
        let LayoutEngine {
            fonts,
            text: lcx,
            text_index,
            ..
        } = &mut *self.engine;
        let keep_text = *text_index;
        let mut tb = lcx.tree_builder(&mut fonts.ctx, 1.0, true, &root_ts);
        // Soft wrap opportunities as in Chrome: Blink decides pairs of
        // printable ASCII characters with its own table (no break after '/'
        // in a URL or path, but after '-' and '?') and breaks after every
        // space run; UAX #14 decides the rest.
        tb.set_line_break_override(Some(parley::CHROMIUM_LINE_BREAK_OVERRIDE));
        // (span index, decorations in effect)
        let mut stack: Vec<(u32, Decor)> = vec![(0, decor)];
        let mut next_atomic = 0usize;
        for (i, (ev, sel)) in evs.iter().zip(&ev_fams).enumerate() {
            let (cur, cur_decor) = *stack.last().expect("root span");
            let (fam, lh): (&[parley::FontFamilyName<'static>], _) = sel
                .as_ref()
                .map_or((&root_fam, root_lh), |(f, lh)| (f, *lh));
            match ev {
                Ev::Open(n, s) => {
                    let si = spans.len() as u32;
                    let opacity = spans[cur as usize].opacity * s.opacity;
                    spans.push(Span {
                        node: *n,
                        style: s.clone(),
                        parent: cur,
                        baseline_shift: lh.1,
                        strut: struts[i],
                        opacity,
                    });
                    let d = cur_decor.extend(s);
                    let opsz = optical_sizing(s);
                    let mut ts = text_style(s, &d, fam, lh.0, si, false, &opsz);
                    if opacity < 1.0 {
                        fade(&mut ts, opacity);
                    }
                    tb.push_style_span(ts);
                    if span_has_edges(s).0 {
                        tb.push_inline_box(spacer(items.len()));
                        items.push(IItem::SpanStart(si));
                    }
                    stack.push((si, d));
                }
                Ev::Close => {
                    let s = &spans[cur as usize].style;
                    if span_has_edges(s).1 {
                        tb.push_inline_box(spacer(items.len()));
                        items.push(IItem::SpanEnd(cur));
                    }
                    tb.pop_style_span();
                    stack.pop();
                }
                Ev::Text(n) => {
                    let t = doc.text(*n).unwrap_or("");
                    push_transformed(&mut tb, t, spans[cur as usize].style.text_transform);
                }
                Ev::Str(t, s) => push_transformed(&mut tb, t, s.text_transform),
                Ev::Br(s) => {
                    let opsz = optical_sizing(s);
                    let mut ts = text_style(s, &cur_decor, fam, lh.0, cur, false, &opsz);
                    ts.white_space_collapse = parley::WhiteSpaceCollapse::Preserve;
                    tb.push_style_span(ts);
                    tb.push_text("\n");
                    tb.pop_style_span();
                }
                Ev::Atomic(_, s) => {
                    let child = atomic_ids[next_atomic];
                    next_atomic += 1;
                    // Struts of the text-less spans whose baseline is the atomic's.
                    let mut strut: Option<(f32, f32)> = None;
                    if s.vertical_align == huginn_style::VerticalAlign::Baseline {
                        let mut k = cur;
                        while k != 0 {
                            let sp = &spans[k as usize];
                            if let Some((o, u)) = sp.strut {
                                strut = Some(strut.map_or((o, u), |(a, b)| (a.max(o), b.max(u))));
                            }
                            if sp.style.vertical_align != huginn_style::VerticalAlign::Baseline {
                                break;
                            }
                            k = sp.parent;
                        }
                    }
                    tb.push_inline_box(parley::InlineBox {
                        id: items.len() as u64,
                        kind: parley::InlineBoxKind::InFlow,
                        index: 0,
                        width: 0.0,
                        height: 0.0,
                        baseline: None,
                        vertical_align: vertical_align(s.vertical_align),
                    });
                    items.push(IItem::Atomic {
                        child,
                        span: cur,
                        strut,
                    });
                }
                Ev::Oof(_, s) => {
                    let child = atomic_ids[next_atomic];
                    next_atomic += 1;
                    // A zero-size marker: where the box would have been in the
                    // line is its static position (CSS 2.1, section 10.3.7).
                    tb.push_inline_box(parley::InlineBox {
                        kind: parley::InlineBoxKind::OutOfFlow,
                        ..spacer(items.len())
                    });
                    items.push(IItem::Oof {
                        child,
                        block_level: !s.original_display.is_inline_level(),
                    });
                }
            }
        }
        let (layout, text) = tb.build();
        let ifc = Ifc {
            layout,
            spans,
            items,
            final_width: 0.0,
            widths: None,
            breaks: 0,
            final_breaks: 0,
            final_balance: None,
            text: keep_text.then_some(text),
        };
        let ts = taffy_style(&style, true);
        self.tree.push(new_node(
            node,
            style,
            Kind::Ifc(Box::new(ifc)),
            ts,
            children,
        ))
    }

    // ----- list markers -------------------------------------------------------

    fn marker_text(&self, node: Option<NodeId>, style: &ComputedStyle) -> Option<String> {
        if style.display != Display::ListItem || style.list_style_type == ListStyleType::None {
            return None;
        }
        let doc = self.input.document;
        let node = node?;
        let ordinal = {
            let start = doc
                .parent(node)
                .and_then(|p| doc.attribute(p, &local_name!("start")))
                .and_then(|s| s.trim().parse::<i64>().ok())
                .unwrap_or(1);
            let before = std::iter::successors(doc.prev_sibling(node), |&n| doc.prev_sibling(n))
                .filter(|&n| {
                    self.input
                        .styles
                        .get(n)
                        .is_some_and(|s| s.display == Display::ListItem)
                })
                .count() as i64;
            start + before
        };
        Some(match style.list_style_type {
            ListStyleType::None => return None,
            ListStyleType::Disc => "• ".to_owned(),
            ListStyleType::Circle => "◦ ".to_owned(),
            ListStyleType::Square => "▪ ".to_owned(),
            ListStyleType::Decimal => format!("{ordinal}. "),
            ListStyleType::LowerAlpha => format!("{}. ", alpha(ordinal)),
            ListStyleType::UpperAlpha => format!("{}. ", alpha(ordinal).to_ascii_uppercase()),
            ListStyleType::LowerRoman => format!("{}. ", roman(ordinal)),
            ListStyleType::UpperRoman => format!("{}. ", roman(ordinal).to_ascii_uppercase()),
        })
    }

    fn shape_marker(&mut self, text: &str, style: &ComputedStyle) -> Option<Box<Marker>> {
        let fam = self.engine.fonts.family_list(&style.font_family);
        let lh = self.engine.fonts.line_height(style, &fam);
        let shape = match style.list_style_type {
            ListStyleType::Disc => Some(DecorationShape::Ellipse),
            ListStyleType::Circle => Some(DecorationShape::EllipseOutline),
            ListStyleType::Square => Some(DecorationShape::Rect),
            _ => None,
        };
        let symbol = shape.and_then(|s| Some((s, self.engine.fonts.ascent(style, &fam)?)));
        self.engine.fonts.cover(text);
        let opsz = optical_sizing(style);
        let mut ts = text_style(style, &Decor::default(), &fam, lh.0, 0, true, &opsz);
        // The marker's trailing space separates it from the content.
        ts.white_space_collapse = parley::WhiteSpaceCollapse::Preserve;
        ts.text_wrap_mode = parley::TextWrapMode::NoWrap;
        let LayoutEngine {
            fonts, text: lcx, ..
        } = &mut *self.engine;
        let mut tb = lcx.tree_builder(&mut fonts.ctx, 1.0, true, &ts);
        tb.push_text(text);
        let (mut layout, _) = tb.build();
        layout.break_all_lines(None);
        Some(Box::new(Marker { layout, symbol }))
    }

    // ----- tables ---------------------------------------------------------------

    fn build_table(&mut self, node: NodeId, style: Arc<ComputedStyle>, decor: Decor) -> tf::NodeId {
        let doc = self.input.document;
        let styles = self.input.styles;
        let decor = decor.extend(&style);
        let mut captions = Vec::new();
        let mut col_widths: Vec<LengthPercentageAuto> = Vec::new();
        // Row groups in display order: header groups, body, footer groups.
        let mut head: Vec<(Option<NodeId>, Arc<ComputedStyle>, Vec<NodeId>)> = Vec::new();
        let mut body: Vec<(Option<NodeId>, Arc<ComputedStyle>, Vec<NodeId>)> = Vec::new();
        let mut foot: Vec<(Option<NodeId>, Arc<ComputedStyle>, Vec<NodeId>)> = Vec::new();
        let mut loose: Vec<NodeId> = Vec::new();
        let anon_group = |table: &ComputedStyle| {
            let mut s = ComputedStyle::inherit_from(table);
            s.display = Display::TableRowGroup;
            Arc::new(s)
        };
        let col_span = |n: NodeId| {
            doc.attribute(n, &local_name!("span"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(1)
                .clamp(1, 1000)
        };
        let table_children: Vec<NodeId> = flatten_contents(self.input, node);
        for c in table_children {
            let Some(cs) = styles.get(c) else { continue };
            match cs.display {
                Display::TableCaption => {
                    let id = self.build_block(El::Dom(c), cs.clone(), decor);
                    captions.push(id);
                }
                Display::TableColumn => {
                    for _ in 0..col_span(c) {
                        col_widths.push(cs.width);
                    }
                }
                Display::TableColumnGroup => {
                    let cols: Vec<NodeId> = doc
                        .children(c)
                        .filter(|&k| {
                            styles
                                .get(k)
                                .is_some_and(|s| s.display == Display::TableColumn)
                        })
                        .collect();
                    if cols.is_empty() {
                        for _ in 0..col_span(c) {
                            col_widths.push(cs.width);
                        }
                    }
                    for k in cols {
                        let ks = styles
                            .get(k)
                            .map_or(LengthPercentageAuto::Auto, |s| s.width);
                        for _ in 0..col_span(k) {
                            col_widths.push(if ks.is_auto() { cs.width } else { ks });
                        }
                    }
                }
                Display::TableHeaderGroup | Display::TableRowGroup | Display::TableFooterGroup => {
                    if !loose.is_empty() {
                        body.push((None, anon_group(&style), std::mem::take(&mut loose)));
                    }
                    let rows = flatten_contents(self.input, c);
                    let g = (Some(c), cs.clone(), rows);
                    match cs.display {
                        Display::TableHeaderGroup if head.is_empty() => head.push(g),
                        Display::TableFooterGroup if foot.is_empty() => foot.push(g),
                        _ => body.push(g),
                    }
                }
                Display::None => {}
                _ => loose.push(c),
            }
        }
        if !loose.is_empty() {
            body.push((None, anon_group(&style), loose));
        }

        // Rows and cells, with the slot grid for rowspan/colspan.
        let mut groups = Vec::new();
        let mut rows: Vec<Row> = Vec::new();
        let mut cells: Vec<Cell> = Vec::new();
        let mut occupied: Vec<Vec<bool>> = Vec::new();
        let mut ncols = col_widths.len();
        let mut group_children: Vec<Vec<tf::NodeId>> = Vec::new();
        let mut group_meta = Vec::new();
        for (gnode, gstyle, members) in head.into_iter().chain(body).chain(foot) {
            let gi = group_meta.len() as u32;
            let gdecor = decor.extend(&gstyle);
            let mut row_ids = Vec::new();
            // Members are rows; anything else is wrapped in an anonymous row.
            let mut pending: Vec<NodeId> = Vec::new();
            let mut row_list: Vec<(Option<NodeId>, Arc<ComputedStyle>, Vec<NodeId>)> = Vec::new();
            for m in members {
                let Some(ms) = styles.get(m) else {
                    if doc.text(m).is_some_and(|t| t.trim().is_empty()) {
                        continue;
                    }
                    pending.push(m);
                    continue;
                };
                if ms.display == Display::TableRow {
                    if !pending.is_empty() {
                        row_list.push((None, anon_row(&gstyle), std::mem::take(&mut pending)));
                    }
                    row_list.push((Some(m), ms.clone(), flatten_contents(self.input, m)));
                } else if ms.display != Display::None {
                    pending.push(m);
                }
            }
            if !pending.is_empty() {
                row_list.push((None, anon_row(&gstyle), pending));
            }
            for (rnode, rstyle, rcells) in row_list {
                let r = rows.len();
                if occupied.len() <= r {
                    occupied.resize(r + 1, Vec::new());
                }
                let rdecor = gdecor.extend(&rstyle);
                let mut cell_ids = Vec::new();
                let mut col = 0usize;
                for cnode in rcells {
                    let cstyle = match styles.get(cnode) {
                        Some(s) if s.display == Display::None => continue,
                        Some(s) => s.clone(),
                        None => {
                            // Text directly in a row: only non-whitespace makes a cell.
                            if doc.text(cnode).is_none_or(|t| t.trim().is_empty()) {
                                continue;
                            }
                            continue;
                        }
                    };
                    while occupied[r].get(col).copied().unwrap_or(false) {
                        col += 1;
                    }
                    let attr_n = |name: &huginn_dom::LocalName| {
                        doc.attribute(cnode, name)
                            .and_then(|v| v.trim().parse::<u32>().ok())
                    };
                    let colspan = attr_n(&local_name!("colspan")).unwrap_or(1).clamp(1, 1000);
                    let rowspan = attr_n(&local_name!("rowspan"))
                        .unwrap_or(1)
                        .clamp(1, 65534)
                        .max(1);
                    let id = self.build_cell(cnode, cstyle, rdecor);
                    for rr in r..r + rowspan as usize {
                        if occupied.len() <= rr {
                            occupied.resize(rr + 1, Vec::new());
                        }
                        let row = &mut occupied[rr];
                        if row.len() < col + colspan as usize {
                            row.resize(col + colspan as usize, false);
                        }
                        for k in col..col + colspan as usize {
                            row[k] = true;
                        }
                    }
                    cells.push(Cell {
                        node: id,
                        row: r as u32,
                        col: col as u32,
                        rowspan,
                        colspan,
                    });
                    cell_ids.push(id);
                    col += colspan as usize;
                    ncols = ncols.max(col);
                }
                let rts = part_style(&rstyle);
                let rid = self
                    .tree
                    .push(new_node(rnode, rstyle, Kind::TablePart, rts, cell_ids));
                rows.push(Row {
                    node: rid,
                    group: gi,
                });
                row_ids.push(rid);
            }
            // Rowspans may not reach past the group's last row.
            for c in cells
                .iter_mut()
                .filter(|c| rows[c.row as usize].group == gi)
            {
                let last = rows.len() as u32;
                c.rowspan = c.rowspan.min(last - c.row);
            }
            occupied.truncate(rows.len());
            group_children.push(row_ids);
            group_meta.push((gnode, gstyle));
        }
        for ((gnode, gstyle), kids) in group_meta.into_iter().zip(group_children) {
            let gts = part_style(&gstyle);
            groups.push(
                self.tree
                    .push(new_node(gnode, gstyle, Kind::TablePart, gts, kids)),
            );
        }

        let collapse = table_collapse(&style);
        let spacing = if collapse {
            (0.0, 0.0)
        } else {
            table_spacing(&style)
        };
        if collapse {
            self.collapse_borders(&style, &cells, rows.len(), ncols);
        }
        let mut children = captions.clone();
        children.extend(groups.iter().copied());
        let mut ts = taffy_style(&style, false);
        ts.display = tf::Display::Block;
        ts.item_is_table = true;
        let table = Table {
            cols: ncols,
            rows,
            groups,
            captions,
            cells,
            col_widths,
            spacing,
            collapse,
            intrinsic: None,
        };
        let id = self.tree.push(new_node(
            Some(node),
            style.clone(),
            Kind::Table(Box::new(table)),
            ts,
            children,
        ));
        if collapse && let Some(o) = collapsed_table_border(&style) {
            self.tree.node_mut(id).border_override = Some(o);
        }
        id
    }

    fn build_cell(&mut self, node: NodeId, style: Arc<ComputedStyle>, decor: Decor) -> tf::NodeId {
        let id = self.build_block(El::Dom(node), style, decor);
        let ts = &mut self.tree.node_mut(id).ts;
        // The table algorithm sizes and places cells.
        if ts.display == tf::Display::Block {
            ts.display = tf::Display::FlowRoot;
        }
        ts.position = taffy::Position::Relative;
        ts.margin = taffy::Rect {
            left: tf::LengthPercentageAuto::length(0.0),
            right: tf::LengthPercentageAuto::length(0.0),
            top: tf::LengthPercentageAuto::length(0.0),
            bottom: tf::LengthPercentageAuto::length(0.0),
        };
        ts.float = taffy::Float::None;
        id
    }

    /// `border-collapse: collapse`: a cell keeps its right and bottom borders
    /// and drops the left/top ones shared with a neighbor (drawn by it).
    fn collapse_borders(
        &mut self,
        table: &ComputedStyle,
        cells: &[Cell],
        nrows: usize,
        ncols: usize,
    ) {
        let table_has_border = table.border_width.left > 0.0 || table.border_width.top > 0.0;
        for c in cells {
            let n = &mut self.tree.nodes[ix(c.node)];
            let mut b = n.style.border_width;
            if c.col > 0 {
                b.left = 0.0;
            }
            if c.row > 0 {
                b.top = 0.0;
            }
            if table_has_border {
                if c.col == 0 {
                    b.left = 0.0;
                }
                if c.row == 0 {
                    b.top = 0.0;
                }
                if (c.col + c.colspan) as usize >= ncols {
                    b.right = 0.0;
                }
                if (c.row + c.rowspan) as usize >= nrows {
                    b.bottom = 0.0;
                }
            }
            if b != n.style.border_width {
                n.ts.border = taffy::Rect {
                    left: tf::LengthPercentage::length(b.left),
                    right: tf::LengthPercentage::length(b.right),
                    top: tf::LengthPercentage::length(b.top),
                    bottom: tf::LengthPercentage::length(b.bottom),
                };
                n.border_override = Some(b);
            }
        }
    }
}

fn spacer(id: usize) -> parley::InlineBox {
    parley::InlineBox {
        id: id as u64,
        kind: parley::InlineBoxKind::InFlow,
        index: 0,
        width: 0.0,
        height: 0.0,
        baseline: None,
        vertical_align: parley::VerticalAlign::BASELINE,
    }
}

fn anon_row(group: &ComputedStyle) -> Arc<ComputedStyle> {
    let mut s = ComputedStyle::inherit_from(group);
    s.display = Display::TableRow;
    Arc::new(s)
}

/// Taffy style for rows and row groups (sized by the table).
fn part_style(s: &ComputedStyle) -> tree::TStyle {
    let mut ts = taffy_style(s, true);
    ts.position = taffy::Position::Relative;
    ts
}

/// Children of `node`, looking through `display: contents`.
fn flatten_contents(input: &LayoutInput<'_>, node: NodeId) -> Vec<NodeId> {
    let mut out = Vec::new();
    for c in box_children(input.document, node) {
        if input
            .styles
            .get(c)
            .is_some_and(|s| s.display == Display::Contents)
        {
            out.extend(flatten_contents(input, c));
        } else {
            out.push(c);
        }
    }
    out
}

fn table_collapse(s: &ComputedStyle) -> bool {
    s.border_collapse == huginn_style::BorderCollapse::Collapse
}

fn table_spacing(s: &ComputedStyle) -> (f32, f32) {
    s.border_spacing
}

/// Collapsed table border: half of it lies outside the table box in CSS;
/// we keep the full width inside (the cells drop their outer borders).
fn collapsed_table_border(_s: &ComputedStyle) -> Option<Edges<f32>> {
    None
}

fn push_transformed(tb: &mut parley::TreeBuilder<'_, Brush>, t: &str, tt: TextTransform) {
    match tt {
        TextTransform::None => tb.push_text(t),
        TextTransform::Uppercase => tb.push_text(&t.to_uppercase()),
        TextTransform::Lowercase => tb.push_text(&t.to_lowercase()),
        TextTransform::Capitalize => {
            let mut s = String::with_capacity(t.len());
            let mut at_start = true;
            for c in t.chars() {
                if at_start && c.is_alphabetic() {
                    s.extend(c.to_uppercase());
                } else {
                    s.push(c);
                }
                at_start = c.is_whitespace();
            }
            tb.push_text(&s);
        }
    }
}

fn alpha(mut n: i64) -> String {
    if n <= 0 {
        return n.to_string();
    }
    let mut s = Vec::new();
    while n > 0 {
        n -= 1;
        s.push(b'a' + (n % 26) as u8);
        n /= 26;
    }
    s.reverse();
    String::from_utf8(s).expect("ascii")
}

fn roman(mut n: i64) -> String {
    if !(1..4000).contains(&n) {
        return n.to_string();
    }
    const T: [(i64, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut s = String::new();
    for (v, r) in T {
        while n >= v {
            s.push_str(r);
            n -= v;
        }
    }
    s
}
