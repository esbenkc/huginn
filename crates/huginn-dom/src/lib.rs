//! huginn-dom: the Huginn document tree.
//!
//! The DOM is an arena: every node lives in one `Vec<Node>` owned by a
//! [`Document`] and is addressed by a [`NodeId`] (a 32-bit index). Links
//! between nodes are `Option<NodeId>` (4 bytes each, thanks to the
//! `NonZeroU32` niche). There is no `Rc`, `RefCell` or per-node heap
//! allocation for the tree structure itself.
//!
//! Names are interned atoms from `markup5ever`/`web_atoms`. These are the
//! same atom types that Stylo 0.24 matches selectors against.
//!
//! Text and attribute values are `StrTendril`, which html5ever produces
//! without copying. `StrTendril` uses a non-atomic refcount, so a `Document`
//! is neither `Send` nor `Sync` today: it lives on the render thread. Parallel
//! styling (Stylo) needs atomic or owned strings first (see CONTRACTS.md).

mod fragment;
mod parse;
mod shadow;

use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroU32;

pub use markup5ever::interface::QuirksMode;
pub use markup5ever::tendril::StrTendril;
pub use markup5ever::{
    Attribute, LocalName, Namespace, Prefix, QualName, local_name, namespace_url, ns,
};

pub use fragment::{parse_fragment, serialize};
pub use parse::{parse_html, parse_html_bytes};
pub use shadow::{FlatChildren, ShadowIncludingDescendants};

/// Index of a node in its [`Document`] arena.
///
/// Ids are stable for the life of the document. Detached nodes keep their id;
/// ids are never reused (a free list with generations is a later step, see
/// CONTRACTS.md).
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(NonZeroU32);

impl NodeId {
    /// The document node. It is always the first node of the arena.
    pub const DOCUMENT: NodeId = NodeId(NonZeroU32::MIN);

    /// Arena index of this node. Side tables in other crates (styles, layout
    /// boxes) are indexed by this value.
    #[inline]
    pub fn index(self) -> usize {
        (self.0.get() - 1) as usize
    }

    /// Build an id from an arena index.
    #[inline]
    pub fn from_index(index: usize) -> NodeId {
        let raw = u32::try_from(index + 1).expect("DOM arena exceeds u32::MAX nodes");
        NodeId(NonZeroU32::new(raw).expect("index + 1 is never zero"))
    }

    /// Raw 32-bit value. Useful as an opaque handle across FFI (V8 wrappers).
    #[inline]
    pub fn to_raw(self) -> u32 {
        self.0.get()
    }

    /// Inverse of [`NodeId::to_raw`]. Returns `None` for 0.
    #[inline]
    pub fn from_raw(raw: u32) -> Option<NodeId> {
        NonZeroU32::new(raw).map(NodeId)
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.index())
    }
}

/// One node in the arena.
#[derive(Debug, Clone)]
pub struct Node {
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub data: NodeData,
}

/// What a node is.
#[derive(Debug, Clone)]
pub enum NodeData {
    Document,
    /// Created for `<template>` contents and for shadow roots. Never attached
    /// to the main tree; a shadow root is linked to its host instead (see
    /// [`Document::shadow_host`]).
    DocumentFragment,
    Doctype {
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    },
    Element(ElementData),
    Text(StrTendril),
    Comment(StrTendril),
    ProcessingInstruction {
        target: StrTendril,
        data: StrTendril,
    },
}

/// Element name, attributes and element-specific extras.
#[derive(Debug, Clone)]
pub struct ElementData {
    pub name: QualName,
    pub attrs: Vec<Attribute>,
    /// For `<template>`: the fragment holding its contents.
    pub template_contents: Option<NodeId>,
    /// For a shadow host: its shadow root (a [`NodeData::DocumentFragment`]).
    pub shadow_root: Option<NodeId>,
}

impl ElementData {
    /// Value of an attribute in the null namespace.
    pub fn attr(&self, name: &LocalName) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.name.ns == ns!() && a.name.local == *name)
            .map(|a| &*a.value)
    }

    /// Value of the `id` attribute.
    pub fn id(&self) -> Option<&str> {
        self.attr(&local_name!("id"))
    }

    /// Whitespace-separated tokens of the `class` attribute.
    pub fn classes(&self) -> impl Iterator<Item = &str> {
        self.attr(&local_name!("class"))
            .unwrap_or("")
            .split_ascii_whitespace()
    }

    /// True if this is an HTML element with the given local name.
    pub fn is_html(&self, local: &LocalName) -> bool {
        self.name.ns == ns!(html) && self.name.local == *local
    }
}

/// What the user points at and types into: the elements behind `:hover`,
/// `:active`, `:focus`, `:focus-visible` and `:focus-within`. Set by the
/// browser (and by `focus()`/`blur()` from scripts); the style engine reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InteractionState {
    /// Innermost element under the pointer.
    pub hover: Option<NodeId>,
    /// Element the primary button was pressed on, while it is held.
    pub active: Option<NodeId>,
    /// Focused element.
    pub focus: Option<NodeId>,
    /// The focus came from the keyboard, or is in a text field (`:focus-visible`).
    pub focus_visible: bool,
}

/// The document: node arena plus document-level state.
///
/// All tree mutation goes through methods on this type. Every mutation bumps
/// [`Document::generation`], so style and layout can tell when their cached
/// results are stale.
#[derive(Debug, Clone)]
pub struct Document {
    nodes: Vec<Node>,
    quirks_mode: QuirksMode,
    generation: u64,
    /// `id` value -> elements carrying it, connected or not, in creation
    /// order. Only kept after [`Document::index_ids`] (script engines); a
    /// page without scripts never pays for it.
    ids: Option<HashMap<Box<str>, Vec<NodeId>>>,
    /// Shadow root -> its host. Empty on pages without shadow DOM.
    shadow_hosts: HashMap<NodeId, NodeId>,
    /// Shadow root -> the CSS of its `adoptedStyleSheets`, in order.
    adopted_css: HashMap<NodeId, Box<str>>,
    /// Element scroll offsets (`scrollLeft`, `scrollTop`) set by scripts.
    /// Empty on pages that never scroll an element.
    scroll: HashMap<NodeId, (f32, f32)>,
    /// Bumped when a scroll offset changes: layout must re-place boxes, but
    /// style is unaffected, so this is not part of `generation`.
    scroll_generation: u64,
    interaction: InteractionState,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    /// An empty document holding only the document node.
    pub fn new() -> Document {
        Document {
            nodes: vec![Node {
                parent: None,
                first_child: None,
                last_child: None,
                prev_sibling: None,
                next_sibling: None,
                data: NodeData::Document,
            }],
            quirks_mode: QuirksMode::NoQuirks,
            generation: 0,
            ids: None,
            shadow_hosts: HashMap::new(),
            adopted_css: HashMap::new(),
            scroll: HashMap::new(),
            scroll_generation: 0,
            interaction: InteractionState::default(),
        }
    }

    /// Number of nodes in the arena, including detached ones.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Always false: the document node exists from construction.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Mutation counter. Increases on every change to the tree, attributes or text.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn interaction(&self) -> &InteractionState {
        &self.interaction
    }

    /// Replace the interaction state. Not counted in `generation`: the
    /// browser compares states and restyles only when a rule uses the
    /// changed pseudo-class (`StyleEngine::uses_state`).
    pub fn set_interaction(&mut self, state: InteractionState) {
        self.interaction = state;
    }

    /// Counts changes to element scroll offsets ([`Document::set_scroll_offset`]).
    pub fn scroll_generation(&self) -> u64 {
        self.scroll_generation
    }

    /// Scroll offset of an element's scroll container, (left, top) in CSS
    /// px; (0, 0) if never scrolled. Layout clamps it to the scrollable
    /// overflow.
    pub fn scroll_offset(&self, node: NodeId) -> (f32, f32) {
        if self.scroll.is_empty() {
            return (0.0, 0.0);
        }
        self.scroll.get(&node).copied().unwrap_or((0.0, 0.0))
    }

    pub fn set_scroll_offset(&mut self, node: NodeId, x: f32, y: f32) {
        if self.scroll_offset(node) == (x, y) {
            return;
        }
        if x == 0.0 && y == 0.0 {
            self.scroll.remove(&node);
        } else {
            self.scroll.insert(node, (x, y));
        }
        self.scroll_generation += 1;
    }

    pub fn quirks_mode(&self) -> QuirksMode {
        self.quirks_mode
    }

    pub fn set_quirks_mode(&mut self, mode: QuirksMode) {
        self.quirks_mode = mode;
    }

    #[inline]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.index()]
    }

    #[inline]
    fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.index()]
    }

    /// Element data, or `None` for non-element nodes.
    #[inline]
    pub fn element(&self, id: NodeId) -> Option<&ElementData> {
        match &self.node(id).data {
            NodeData::Element(e) => Some(e),
            _ => None,
        }
    }

    /// Text of a text node, or `None` for other nodes.
    #[inline]
    pub fn text(&self, id: NodeId) -> Option<&str> {
        match &self.node(id).data {
            NodeData::Text(t) => Some(t),
            _ => None,
        }
    }

    #[inline]
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).parent
    }

    #[inline]
    pub fn first_child(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).first_child
    }

    #[inline]
    pub fn last_child(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).last_child
    }

    #[inline]
    pub fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).next_sibling
    }

    #[inline]
    pub fn prev_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).prev_sibling
    }

    /// Direct children in order.
    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            doc: self,
            next: self.first_child(id),
        }
    }

    /// `id` and all its descendants in tree order (pre-order).
    pub fn descendants(&self, id: NodeId) -> Descendants<'_> {
        Descendants {
            doc: self,
            root: id,
            next: Some(id),
        }
    }

    /// Parent, grandparent, ... up to the document node.
    pub fn ancestors(&self, id: NodeId) -> Ancestors<'_> {
        Ancestors {
            doc: self,
            next: self.parent(id),
        }
    }

    /// The root element (`<html>`).
    pub fn document_element(&self) -> Option<NodeId> {
        self.children(NodeId::DOCUMENT)
            .find(|&c| self.element(c).is_some())
    }

    /// The `<body>` element, if any.
    pub fn body(&self) -> Option<NodeId> {
        let html = self.document_element()?;
        self.children(html).find(|&c| {
            self.element(c)
                .is_some_and(|e| e.is_html(&local_name!("body")))
        })
    }

    /// The `<head>` element, if any.
    pub fn head(&self) -> Option<NodeId> {
        let html = self.document_element()?;
        self.children(html).find(|&c| {
            self.element(c)
                .is_some_and(|e| e.is_html(&local_name!("head")))
        })
    }

    /// First element in tree order whose `id` attribute equals `id`.
    pub fn element_by_id(&self, id: &str) -> Option<NodeId> {
        if let Some(ids) = &self.ids {
            let mut connected = ids
                .get(id)?
                .iter()
                .copied()
                .filter(|&n| self.ancestors(n).last() == Some(NodeId::DOCUMENT));
            let first = connected.next()?;
            if connected.next().is_none() {
                return Some(first);
            }
            // Duplicate ids: tree order decides.
        }
        self.descendants(NodeId::DOCUMENT)
            .find(|&n| self.element(n).and_then(|e| e.id()) == Some(id))
    }

    /// Keep an index of `id` attributes from now on, so
    /// [`Document::element_by_id`] does not walk the whole tree.
    pub fn index_ids(&mut self) {
        if self.ids.is_some() {
            return;
        }
        let mut ids: HashMap<Box<str>, Vec<NodeId>> = HashMap::new();
        for (i, node) in self.nodes.iter().enumerate() {
            if let NodeData::Element(e) = &node.data
                && let Some(id) = e.id()
            {
                ids.entry(id.into())
                    .or_default()
                    .push(NodeId::from_index(i));
            }
        }
        self.ids = Some(ids);
    }

    fn index_id(&mut self, node: NodeId, old: Option<&str>, new: Option<&str>) {
        let Some(ids) = &mut self.ids else { return };
        if let Some(old) = old
            && let Some(list) = ids.get_mut(old)
        {
            list.retain(|&n| n != node);
            if list.is_empty() {
                ids.remove(old);
            }
        }
        if let Some(new) = new {
            ids.entry(new.into()).or_default().push(node);
        }
    }

    /// HTML elements with the given local name, in tree order.
    pub fn elements_by_local_name<'a>(
        &'a self,
        name: &'a LocalName,
    ) -> impl Iterator<Item = NodeId> + 'a {
        self.descendants(NodeId::DOCUMENT)
            .filter(move |&n| self.element(n).is_some_and(|e| e.is_html(name)))
    }

    /// Attribute value of an element (null namespace).
    pub fn attribute(&self, id: NodeId, name: &LocalName) -> Option<&str> {
        self.element(id)?.attr(name)
    }

    /// Concatenated text of all descendant text nodes (DOM `textContent`).
    pub fn text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        for n in self.descendants(id) {
            if let NodeData::Text(t) = &self.node(n).data {
                out.push_str(t);
            }
        }
        out
    }

    // ----- creation -------------------------------------------------------

    fn push_node(&mut self, data: NodeData) -> NodeId {
        let id = NodeId::from_index(self.nodes.len());
        self.nodes.push(Node {
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
            data,
        });
        self.generation += 1;
        id
    }

    /// Create a detached element.
    pub fn create_element(&mut self, name: QualName, attrs: Vec<Attribute>) -> NodeId {
        let is_template = name.ns == ns!(html) && name.local == local_name!("template");
        let template_contents = is_template.then(|| self.push_node(NodeData::DocumentFragment));
        let el = ElementData {
            name,
            attrs,
            template_contents,
            shadow_root: None,
        };
        let id = if self.ids.is_some() {
            el.id().map(Box::<str>::from)
        } else {
            None
        };
        let n = self.push_node(NodeData::Element(el));
        self.index_id(n, None, id.as_deref());
        n
    }

    /// Create a detached HTML element by local name.
    pub fn create_html_element(&mut self, local: &str) -> NodeId {
        self.create_element(
            QualName::new(None, ns!(html), LocalName::from(local)),
            Vec::new(),
        )
    }

    /// Create a detached text node.
    pub fn create_text(&mut self, text: &str) -> NodeId {
        self.push_node(NodeData::Text(StrTendril::from_slice(text)))
    }

    /// Create a detached text node from a tendril without copying.
    pub fn create_text_tendril(&mut self, text: StrTendril) -> NodeId {
        self.push_node(NodeData::Text(text))
    }

    /// Create a detached comment node.
    pub fn create_comment(&mut self, text: &str) -> NodeId {
        self.push_node(NodeData::Comment(StrTendril::from_slice(text)))
    }

    /// Create a node from raw data. Used by the parser for doctype and PI nodes.
    pub fn create_node(&mut self, data: NodeData) -> NodeId {
        self.push_node(data)
    }

    // ----- tree mutation ----------------------------------------------------

    /// Remove `id` from its parent. No-op if already detached.
    pub fn detach(&mut self, id: NodeId) {
        let Node {
            parent,
            prev_sibling,
            next_sibling,
            ..
        } = *self.node(id);
        let Some(parent) = parent else { return };
        match prev_sibling {
            Some(p) => self.node_mut(p).next_sibling = next_sibling,
            None => self.node_mut(parent).first_child = next_sibling,
        }
        match next_sibling {
            Some(n) => self.node_mut(n).prev_sibling = prev_sibling,
            None => self.node_mut(parent).last_child = prev_sibling,
        }
        let node = self.node_mut(id);
        node.parent = None;
        node.prev_sibling = None;
        node.next_sibling = None;
        self.generation += 1;
    }

    /// Append `child` as the last child of `parent`, detaching it first.
    ///
    /// # Panics
    /// If `child` is `parent` or one of its ancestors (would create a cycle).
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) {
        self.insert_before(parent, child, None);
    }

    /// Insert `child` into `parent` before `reference` (or at the end if `None`).
    /// `child` is detached from its old position first.
    ///
    /// # Panics
    /// If `reference` is not a child of `parent`, or if the insertion would
    /// create a cycle.
    pub fn insert_before(&mut self, parent: NodeId, child: NodeId, reference: Option<NodeId>) {
        assert!(
            child != parent && !self.ancestors(parent).any(|a| a == child),
            "insert_before would create a cycle"
        );
        if let Some(r) = reference {
            assert_eq!(
                self.parent(r),
                Some(parent),
                "reference is not a child of parent"
            );
            if r == child {
                return;
            }
        }
        self.detach(child);
        let prev = match reference {
            Some(r) => self.node(r).prev_sibling,
            None => self.node(parent).last_child,
        };
        {
            let c = self.node_mut(child);
            c.parent = Some(parent);
            c.prev_sibling = prev;
            c.next_sibling = reference;
        }
        match prev {
            Some(p) => self.node_mut(p).next_sibling = Some(child),
            None => self.node_mut(parent).first_child = Some(child),
        }
        match reference {
            Some(r) => self.node_mut(r).prev_sibling = Some(child),
            None => self.node_mut(parent).last_child = Some(child),
        }
        self.generation += 1;
    }

    /// Remove all children of `id` (they stay in the arena, detached).
    pub fn remove_children(&mut self, id: NodeId) {
        while let Some(c) = self.first_child(id) {
            self.detach(c);
        }
    }

    /// Set (or add) an attribute in the null namespace.
    ///
    /// # Panics
    /// If `id` is not an element.
    pub fn set_attribute(&mut self, id: NodeId, name: LocalName, value: &str) {
        let indexed = self.ids.is_some() && name == local_name!("id");
        let NodeData::Element(el) = &mut self.node_mut(id).data else {
            panic!("set_attribute on a non-element node");
        };
        let mut old = None;
        match el
            .attrs
            .iter_mut()
            .find(|a| a.name.ns == ns!() && a.name.local == name)
        {
            Some(a) => {
                if indexed {
                    old = Some(Box::<str>::from(&*a.value));
                }
                a.value = StrTendril::from_slice(value);
            }
            None => el.attrs.push(Attribute {
                name: QualName::new(None, ns!(), name),
                value: StrTendril::from_slice(value),
            }),
        }
        if indexed {
            self.index_id(id, old.as_deref(), Some(value));
        }
        self.generation += 1;
    }

    /// Remove an attribute in the null namespace. Returns true if it existed.
    pub fn remove_attribute(&mut self, id: NodeId, name: &LocalName) -> bool {
        let old = if self.ids.is_some() && *name == local_name!("id") {
            self.attribute(id, name).map(Box::<str>::from)
        } else {
            None
        };
        let NodeData::Element(el) = &mut self.node_mut(id).data else {
            return false;
        };
        let before = el.attrs.len();
        el.attrs
            .retain(|a| !(a.name.ns == ns!() && a.name.local == *name));
        let removed = el.attrs.len() != before;
        if removed {
            self.index_id(id, old.as_deref(), None);
            self.generation += 1;
        }
        removed
    }

    /// Replace the data of a text or comment node.
    ///
    /// # Panics
    /// If `id` is neither a text nor a comment node.
    pub fn set_character_data(&mut self, id: NodeId, text: &str) {
        match &mut self.node_mut(id).data {
            NodeData::Text(t) | NodeData::Comment(t) => *t = StrTendril::from_slice(text),
            _ => panic!("set_character_data on a node without character data"),
        }
        self.generation += 1;
    }

    /// DOM `textContent` setter: for elements and fragments, replace all
    /// children with one text node (none if `text` is empty); for text and
    /// comment nodes, replace the data.
    pub fn set_text_content(&mut self, id: NodeId, text: &str) {
        match self.node(id).data {
            NodeData::Text(_) | NodeData::Comment(_) => self.set_character_data(id, text),
            NodeData::Element(_) | NodeData::DocumentFragment => {
                self.remove_children(id);
                if !text.is_empty() {
                    let t = self.create_text(text);
                    self.append_child(id, t);
                }
            }
            _ => {}
        }
    }

    /// Append text to `parent`, merging with a trailing text node if there is one.
    /// This is what the HTML parser does.
    pub fn append_text(&mut self, parent: NodeId, text: &str) {
        if let Some(last) = self.last_child(parent)
            && let NodeData::Text(t) = &mut self.node_mut(last).data
        {
            t.push_slice(text);
            self.generation += 1;
            return;
        }
        let t = self.create_text(text);
        self.append_child(parent, t);
    }
}

/// Iterator over the children of a node.
pub struct Children<'a> {
    doc: &'a Document,
    next: Option<NodeId>,
}

impl Iterator for Children<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        let cur = self.next?;
        self.next = self.doc.next_sibling(cur);
        Some(cur)
    }
}

/// Pre-order iterator over a subtree.
pub struct Descendants<'a> {
    doc: &'a Document,
    root: NodeId,
    next: Option<NodeId>,
}

impl Iterator for Descendants<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        let cur = self.next?;
        self.next = if let Some(c) = self.doc.first_child(cur) {
            Some(c)
        } else {
            let mut n = cur;
            loop {
                if n == self.root {
                    break None;
                }
                if let Some(s) = self.doc.next_sibling(n) {
                    break Some(s);
                }
                match self.doc.parent(n) {
                    Some(p) => n = p,
                    None => break None,
                }
            }
        };
        Some(cur)
    }
}

/// Iterator over the ancestors of a node.
pub struct Ancestors<'a> {
    doc: &'a Document,
    next: Option<NodeId>,
}

impl Iterator for Ancestors<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        let cur = self.next?;
        self.next = self.doc.parent(cur);
        Some(cur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_detach_keep_links_consistent() {
        let mut d = Document::new();
        let a = d.create_html_element("div");
        let b = d.create_text("b");
        let c = d.create_text("c");
        d.append_child(NodeId::DOCUMENT, a);
        d.append_child(a, c);
        d.insert_before(a, b, Some(c));
        assert_eq!(d.children(a).collect::<Vec<_>>(), vec![b, c]);
        assert_eq!(d.text_content(a), "bc");
        d.detach(b);
        assert_eq!(d.children(a).collect::<Vec<_>>(), vec![c]);
        assert_eq!(d.prev_sibling(c), None);
        // Moving a node re-parents it.
        d.append_child(NodeId::DOCUMENT, c);
        assert_eq!(d.first_child(a), None);
        assert_eq!(d.last_child(a), None);
        assert_eq!(d.parent(c), Some(NodeId::DOCUMENT));
    }

    #[test]
    #[should_panic(expected = "cycle")]
    fn insert_rejects_cycles() {
        let mut d = Document::new();
        let a = d.create_html_element("div");
        let b = d.create_html_element("div");
        d.append_child(a, b);
        d.append_child(b, a);
    }

    #[test]
    fn descendants_stay_inside_subtree() {
        let doc = parse_html("<p id=x><b>1</b>2</p><p>3</p>");
        let p = doc.element_by_id("x").unwrap();
        assert_eq!(doc.text_content(p), "12");
        assert_eq!(doc.descendants(p).count(), 4);
    }

    #[test]
    fn id_index_tracks_attributes_and_connection() {
        let mut doc = parse_html("<div id=a></div><p id=b></p>");
        doc.index_ids();
        let a = doc.element_by_id("a").unwrap();
        let b = doc.element_by_id("b").unwrap();
        // Renaming moves the entry; removing drops it.
        doc.set_attribute(a, local_name!("id"), "c");
        assert_eq!(doc.element_by_id("a"), None);
        assert_eq!(doc.element_by_id("c"), Some(a));
        doc.remove_attribute(a, &local_name!("id"));
        assert_eq!(doc.element_by_id("c"), None);
        // Detached elements are not found until inserted.
        let x = doc.create_html_element("i");
        doc.set_attribute(x, local_name!("id"), "b");
        assert_eq!(doc.element_by_id("b"), Some(b));
        doc.detach(b);
        assert_eq!(doc.element_by_id("b"), None);
        let body = doc.body().unwrap();
        doc.append_child(body, x);
        assert_eq!(doc.element_by_id("b"), Some(x));
        // Duplicates resolve in tree order, not creation order.
        doc.insert_before(body, b, Some(x));
        assert_eq!(doc.element_by_id("b"), Some(b));
    }
}
