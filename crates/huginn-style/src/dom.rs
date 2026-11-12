//! Stylo's DOM traits (`TNode`, `TElement`, `selectors::Element`) on top of
//! the huginn-dom arena.
//!
//! A [`HuginnNode`] is a `Copy` handle: a reference to a [`DomCx`] plus a
//! `NodeId`. The `DomCx` holds the document and a side table with everything
//! Stylo wants to own per element during one style pass (its `ElementData`,
//! interned id and classes, the parsed `style` attribute and presentational
//! hints). The side table lives only for one `compute` call.
//!
//! Stylo runs sequentially here (no rayon pool), so the handles do not need
//! to be `Send`. `Document` is not `Sync` anyway (non-atomic `StrTendril`).

use std::cell::{Cell, UnsafeCell};
use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ptr::NonNull;

use huginn_dom::{Document, NodeData, NodeId, local_name, ns};
use selectors::OpaqueElement;
use selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use selectors::bloom::{BLOOM_HASH_MASK, BloomFilter};
use selectors::matching::{ElementSelectorFlags, MatchingContext, VisitedHandlingMode};
use selectors::sink::Push;
use style::applicable_declarations::ApplicableDeclarationBlock;
use style::bloom::each_relevant_element_hash;
use style::context::{QuirksMode, SharedStyleContext};
use style::data::{ElementDataMut, ElementDataRef, ElementDataWrapper};
use style::dom::{LayoutIterator, NodeInfo, OpaqueNode, TDocument, TElement, TNode, TShadowRoot};
use style::properties::{ComputedValues, PropertyDeclarationBlock};
use style::rule_tree::{CascadeLevel, CascadeOrigin, StyleSourceBorrow};
use style::selector_parser::{AttrValue, Lang, NonTSPseudoClass, PseudoElement, SelectorImpl};
use style::servo_arc::{Arc, ArcBorrow};
use style::shared_lock::{Locked, SharedRwLock};
use style::stylesheets::layer_rule::LayerOrder;
use style::stylesheets::scope_rule::ImplicitScopeRoot;
use style::stylist::CascadeData;
use style::values::computed::Display;
use style::values::{AtomIdent, AtomString, GenericAtomIdent};
use style::{Atom, CaseSensitivityExt, LocalName, Namespace};
use style_dom::ElementState;

/// Per-element state for one style pass.
pub(crate) struct ElemSide {
    data: UnsafeCell<Option<ElementDataWrapper>>,
    dirty_descendants: Cell<bool>,
    selector_flags: Cell<ElementSelectorFlags>,
    pub(crate) style_attr: Option<Arc<Locked<PropertyDeclarationBlock>>>,
    pub(crate) hints: Option<Arc<Locked<PropertyDeclarationBlock>>>,
    id: Option<Atom>,
    classes: Box<[Atom]>,
}

impl ElemSide {
    pub(crate) fn new(
        id: Option<Atom>,
        classes: Box<[Atom]>,
        style_attr: Option<Arc<Locked<PropertyDeclarationBlock>>>,
        hints: Option<Arc<Locked<PropertyDeclarationBlock>>>,
    ) -> ElemSide {
        ElemSide {
            data: UnsafeCell::new(None),
            dirty_descendants: Cell::new(true),
            selector_flags: Cell::new(ElementSelectorFlags::empty()),
            style_attr,
            hints,
            id,
            classes,
        }
    }

    /// The element's Stylo data, if it was styled.
    pub(crate) fn data(&self) -> Option<ElementDataRef<'_>> {
        // SAFETY: only called after the traversal, when nothing mutates.
        unsafe { &*self.data.get() }.as_ref().map(|w| w.borrow())
    }
}

/// The document plus the per-pass side table.
pub(crate) struct DomCx<'a> {
    pub(crate) doc: &'a Document,
    pub(crate) lock: SharedRwLock,
    pub(crate) quirks: QuirksMode,
    /// Node index -> index into `elems` (`u32::MAX` = no side entry).
    slot: Vec<u32>,
    elems: Vec<ElemSide>,
    /// One handle per node index; a [`HuginnNode`] points at one of these.
    handles: Box<[NodeHandle<'a>]>,
    /// Rules of each shadow root's `<style>` elements, by shadow root.
    pub(crate) shadow_data: HashMap<NodeId, Arc<CascadeData>>,
    /// `:hover`, `:active` and `:focus-within` elements: the target and its
    /// flat-tree ancestors (short lists, scanned linearly).
    hover: Vec<NodeId>,
    active: Vec<NodeId>,
    focus_within: Vec<NodeId>,
}

/// `node` and its flat-tree ancestors (elements only).
fn flat_chain(doc: &Document, node: Option<NodeId>) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut cur = node;
    while let Some(n) = cur {
        if doc.element(n).is_some() {
            out.push(n);
        }
        cur = doc.flat_parent(n);
    }
    out
}

impl<'a> DomCx<'a> {
    /// A context for selector matching only (querySelector): no side table.
    pub(crate) fn bare(doc: &'a Document, lock: SharedRwLock) -> Box<DomCx<'a>> {
        DomCx::boxed(doc, lock, Vec::new(), Vec::new())
    }

    /// A context for a style pass. `make` builds the side entry of each element.
    pub(crate) fn with_side(
        doc: &'a Document,
        lock: SharedRwLock,
        mut make: impl FnMut(NodeId) -> ElemSide,
    ) -> Box<DomCx<'a>> {
        let mut slot = vec![u32::MAX; doc.len()];
        let mut elems = Vec::new();
        for id in doc.shadow_including_descendants(NodeId::DOCUMENT) {
            if doc.element(id).is_some() {
                slot[id.index()] = elems.len() as u32;
                elems.push(make(id));
            }
        }
        DomCx::boxed(doc, lock, slot, elems)
    }

    /// Box the context first, so the handles can point back at it.
    fn boxed(
        doc: &'a Document,
        lock: SharedRwLock,
        slot: Vec<u32>,
        elems: Vec<ElemSide>,
    ) -> Box<DomCx<'a>> {
        let state = doc.interaction();
        let mut cx = Box::new(DomCx {
            doc,
            lock,
            quirks: quirks_of(doc),
            slot,
            elems,
            handles: Box::default(),
            shadow_data: HashMap::new(),
            hover: flat_chain(doc, state.hover),
            active: flat_chain(doc, state.active),
            focus_within: flat_chain(doc, state.focus),
        });
        let at: *const DomCx<'a> = &*cx;
        cx.handles = (0..doc.len())
            .map(|i| NodeHandle {
                cx: at,
                id: NodeId::from_index(i),
            })
            .collect();
        cx
    }

    pub(crate) fn side(&self, id: NodeId) -> Option<&ElemSide> {
        let i = *self.slot.get(id.index())?;
        self.elems.get(i as usize)
    }

    pub(crate) fn node(&'a self, id: NodeId) -> HuginnNode<'a> {
        HuginnNode(&self.handles[id.index()])
    }
}

fn quirks_of(doc: &Document) -> QuirksMode {
    match doc.quirks_mode() {
        huginn_dom::QuirksMode::Quirks => QuirksMode::Quirks,
        huginn_dom::QuirksMode::LimitedQuirks => QuirksMode::LimitedQuirks,
        huginn_dom::QuirksMode::NoQuirks => QuirksMode::NoQuirks,
    }
}

/// A node handle for Stylo: one pointer into [`DomCx`]'s handle table.
///
/// Stylo's style sharing cache stores elements in thread-local storage as
/// `usize`, so a `TElement` must be exactly pointer-sized.
#[derive(Copy, Clone)]
pub(crate) struct HuginnNode<'a>(&'a NodeHandle<'a>);

/// The target of a [`HuginnNode`]: its context and node id.
pub(crate) struct NodeHandle<'a> {
    cx: *const DomCx<'a>,
    id: NodeId,
}

impl PartialEq for HuginnNode<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.nid() == other.nid()
    }
}
impl Eq for HuginnNode<'_> {}

impl Hash for HuginnNode<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.nid().hash(state)
    }
}

impl fmt::Debug for HuginnNode<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.cx().doc.element(self.nid()) {
            Some(el) => write!(f, "<{}>{:?}", &*el.name.local, self.nid()),
            None => write!(f, "{:?}", self.nid()),
        }
    }
}

impl<'a> HuginnNode<'a> {
    fn cx(&self) -> &'a DomCx<'a> {
        // SAFETY: handles live in their `DomCx` (boxed, so it never moves)
        // and the `'a` borrow of that `DomCx` outlives every `HuginnNode`.
        unsafe { &*self.0.cx }
    }

    pub(crate) fn nid(&self) -> NodeId {
        self.0.id
    }

    fn with(&self, id: NodeId) -> HuginnNode<'a> {
        self.cx().node(id)
    }

    fn el(&self) -> &'a huginn_dom::ElementData {
        self.cx()
            .doc
            .element(self.nid())
            .expect("HuginnNode used as element is not an element")
    }

    fn side(&self) -> Option<&'a ElemSide> {
        self.cx().side(self.nid())
    }

    fn has_attr(&self, name: &huginn_dom::LocalName) -> bool {
        self.el().attr(name).is_some()
    }

    /// The names in the `part` attribute (CSS Shadow Parts).
    fn part_names(&self) -> std::str::SplitAsciiWhitespace<'a> {
        self.el()
            .attr(&local_name!("part"))
            .unwrap_or("")
            .split_ascii_whitespace()
    }

    /// The `exportparts` mappings as `(inner, outer)` names: a comma-separated
    /// list of `name` or `inner: outer` (CSS Shadow Parts, "parse a part
    /// mapping list"; invalid entries are skipped).
    fn part_mappings(&self) -> impl Iterator<Item = (&'a str, &'a str)> {
        let ident = |s: &'a str| {
            let s = s.trim_matches(|c: char| c.is_ascii_whitespace());
            (!s.is_empty() && !s.contains(|c: char| c.is_ascii_whitespace() || c == ':'))
                .then_some(s)
        };
        let list = self.el().attr(&local_name!("exportparts")).unwrap_or("");
        list.split(',')
            .filter_map(move |m| match m.split_once(':') {
                Some((inner, outer)) => Some((ident(inner)?, ident(outer)?)),
                None => ident(m).map(|n| (n, n)),
            })
    }

    fn is_html(&self, name: &huginn_dom::LocalName) -> bool {
        self.el().is_html(name)
    }

    fn is_form_control(&self) -> bool {
        let el = self.el();
        el.name.ns == ns!(html)
            && matches!(
                &*el.name.local,
                "input" | "button" | "select" | "textarea" | "option" | "optgroup" | "fieldset"
            )
    }

    fn input_type(&self) -> Option<&'a str> {
        self.is_html(&local_name!("input"))
            .then(|| self.el().attr(&local_name!("type")).unwrap_or("text"))
    }

    fn is_checkable_checked(&self) -> bool {
        match self.input_type() {
            Some(t) if t.eq_ignore_ascii_case("checkbox") || t.eq_ignore_ascii_case("radio") => {
                self.has_attr(&local_name!("checked"))
            }
            _ => self.is_html(&local_name!("option")) && self.has_attr(&local_name!("selected")),
        }
    }

    fn is_text_field(&self) -> bool {
        if self.is_html(&local_name!("textarea")) {
            return true;
        }
        match self.input_type() {
            Some(t) => !matches!(
                t.to_ascii_lowercase().as_str(),
                "checkbox"
                    | "radio"
                    | "submit"
                    | "reset"
                    | "button"
                    | "image"
                    | "file"
                    | "hidden"
                    | "color"
                    | "range"
            ),
            None => false,
        }
    }

    fn element_state(&self) -> ElementState {
        let mut s = ElementState::DEFINED;
        if selectors::Element::is_link(self) {
            s |= ElementState::UNVISITED;
        }
        if self.is_form_control() {
            if self.has_attr(&local_name!("disabled")) {
                s |= ElementState::DISABLED;
            } else {
                s |= ElementState::ENABLED;
            }
            if self.is_checkable_checked() {
                s |= ElementState::CHECKED;
            }
            if self.has_attr(&local_name!("required")) {
                s |= ElementState::REQUIRED;
            } else {
                s |= ElementState::OPTIONAL_;
            }
            if self.is_text_field() {
                if self.has_attr(&local_name!("readonly")) {
                    s |= ElementState::READONLY;
                } else {
                    s |= ElementState::READWRITE;
                }
                let empty = if self.is_html(&local_name!("textarea")) {
                    self.cx().doc.text_content(self.nid()).is_empty()
                } else {
                    self.el()
                        .attr(&local_name!("value"))
                        .is_none_or(str::is_empty)
                };
                if empty && self.has_attr(&local_name!("placeholder")) {
                    s |= ElementState::PLACEHOLDER_SHOWN;
                }
                s |= ElementState::VALID;
            } else {
                s |= ElementState::READONLY;
            }
        } else {
            s |= ElementState::READONLY;
        }
        let cx = self.cx();
        let id = self.nid();
        if cx.hover.contains(&id) {
            s |= ElementState::HOVER;
        }
        if cx.active.contains(&id) {
            s |= ElementState::ACTIVE;
        }
        if cx.focus_within.contains(&id) {
            s |= ElementState::FOCUS_WITHIN;
            if cx.focus_within[0] == id {
                s |= ElementState::FOCUS;
                if cx.doc.interaction().focus_visible {
                    s |= ElementState::FOCUSRING;
                }
            }
        }
        s
    }

    fn data_cell(&self) -> &'a UnsafeCell<Option<ElementDataWrapper>> {
        &self.side().expect("style pass without a side table").data
    }

    fn lang_matches(&self, value: &Lang) -> bool {
        let mut cur = Some(self.nid());
        while let Some(id) = cur {
            if let Some(el) = self.cx().doc.element(id)
                && let Some(lang) = el.attr(&local_name!("lang"))
            {
                return lang_prefix_match(lang, value);
            }
            cur = self.cx().doc.parent(id);
        }
        false
    }
}

/// `:lang(want)`: `lang` equals `want` or starts with `want-` (ASCII
/// case-insensitive).
fn lang_prefix_match(lang: &str, want: &str) -> bool {
    lang.len() >= want.len()
        && lang[..want.len()].eq_ignore_ascii_case(want)
        && (lang.len() == want.len() || lang.as_bytes()[want.len()] == b'-')
}

impl NodeInfo for HuginnNode<'_> {
    fn is_element(&self) -> bool {
        self.cx().doc.element(self.nid()).is_some()
    }

    fn is_text_node(&self) -> bool {
        matches!(self.cx().doc.node(self.nid()).data, NodeData::Text(_))
    }
}

impl<'a> TDocument for HuginnNode<'a> {
    type ConcreteNode = HuginnNode<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn is_html_document(&self) -> bool {
        true
    }

    fn quirks_mode(&self) -> QuirksMode {
        self.cx().quirks
    }

    fn shared_lock(&self) -> &SharedRwLock {
        &self.cx().lock
    }
}
impl<'a> TShadowRoot for HuginnNode<'a> {
    type ConcreteNode = HuginnNode<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn host(&self) -> <Self::ConcreteNode as TNode>::ConcreteElement {
        self.with(
            self.cx()
                .doc
                .shadow_host(self.nid())
                .expect("TShadowRoot on a node that is not a shadow root"),
        )
    }

    fn style_data<'b>(&self) -> Option<&'b style::stylist::CascadeData>
    where
        Self: 'b,
    {
        self.cx().shadow_data.get(&self.nid()).map(|d| &**d)
    }
}

impl<'a> TNode for HuginnNode<'a> {
    type ConcreteElement = HuginnNode<'a>;
    type ConcreteDocument = HuginnNode<'a>;
    type ConcreteShadowRoot = HuginnNode<'a>;

    fn parent_node(&self) -> Option<Self> {
        self.cx().doc.parent(self.nid()).map(|p| self.with(p))
    }

    fn first_child(&self) -> Option<Self> {
        self.cx().doc.first_child(self.nid()).map(|c| self.with(c))
    }

    fn last_child(&self) -> Option<Self> {
        self.cx().doc.last_child(self.nid()).map(|c| self.with(c))
    }

    fn prev_sibling(&self) -> Option<Self> {
        self.cx().doc.prev_sibling(self.nid()).map(|c| self.with(c))
    }

    fn next_sibling(&self) -> Option<Self> {
        self.cx().doc.next_sibling(self.nid()).map(|c| self.with(c))
    }

    fn owner_doc(&self) -> Self::ConcreteDocument {
        self.with(NodeId::DOCUMENT)
    }

    fn is_in_document(&self) -> bool {
        true
    }

    /// The parent in the flat tree: a slotted node's slot, a shadow root
    /// child's host. Inheritance follows it (CSS Scoping 3, 3.3).
    fn traversal_parent(&self) -> Option<Self::ConcreteElement> {
        let doc = self.cx().doc;
        doc.flat_parent(self.nid())
            .filter(|&p| doc.element(p).is_some())
            .map(|p| self.with(p))
    }

    fn opaque(&self) -> OpaqueNode {
        OpaqueNode(self.nid().index())
    }

    fn debug_id(self) -> usize {
        self.nid().index()
    }

    fn as_element(&self) -> Option<Self::ConcreteElement> {
        self.is_element().then_some(*self)
    }

    fn as_document(&self) -> Option<Self::ConcreteDocument> {
        (self.nid() == NodeId::DOCUMENT).then_some(*self)
    }

    fn as_shadow_root(&self) -> Option<Self::ConcreteShadowRoot> {
        self.cx()
            .doc
            .shadow_host(self.nid())
            .is_some()
            .then_some(*self)
    }
}

/// Flat tree children of a node, for `TElement::traversal_children`.
pub(crate) struct ChildIter<'a> {
    cx: &'a DomCx<'a>,
    inner: huginn_dom::FlatChildren<'a>,
}

impl<'a> Iterator for ChildIter<'a> {
    type Item = HuginnNode<'a>;

    fn next(&mut self) -> Option<HuginnNode<'a>> {
        self.inner.next().map(|id| self.cx.node(id))
    }
}

impl selectors::Element for HuginnNode<'_> {
    type Impl = SelectorImpl;

    fn opaque(&self) -> OpaqueElement {
        // Node indices are never 0 for elements (0 is the document), so the
        // pointer is never null. Stylo only compares these.
        OpaqueElement::from_ptr(
            NonNull::new(self.nid().index() as *mut ()).expect("element at index 0"),
        )
    }

    /// The DOM parent element (selectors do not cross shadow boundaries;
    /// `:host` matching uses `containing_shadow_host`).
    fn parent_element(&self) -> Option<Self> {
        self.parent_node().filter(|p| p.is_element())
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        self.parent_node()
            .is_some_and(|p| p.as_shadow_root().is_some())
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        TElement::containing_shadow(self).map(|root| root.host())
    }

    fn is_pseudo_element(&self) -> bool {
        false
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        let mut cur = self.cx().doc.prev_sibling(self.nid());
        while let Some(id) = cur {
            if self.cx().doc.element(id).is_some() {
                return Some(self.with(id));
            }
            cur = self.cx().doc.prev_sibling(id);
        }
        None
    }

    fn next_sibling_element(&self) -> Option<Self> {
        let mut cur = self.cx().doc.next_sibling(self.nid());
        while let Some(id) = cur {
            if self.cx().doc.element(id).is_some() {
                return Some(self.with(id));
            }
            cur = self.cx().doc.next_sibling(id);
        }
        None
    }

    fn first_element_child(&self) -> Option<Self> {
        self.cx()
            .doc
            .children(self.nid())
            .find(|&c| self.cx().doc.element(c).is_some())
            .map(|c| self.with(c))
    }

    fn is_html_element_in_html_document(&self) -> bool {
        self.el().name.ns == ns!(html)
    }

    fn has_local_name(&self, local_name: &huginn_dom::LocalName) -> bool {
        self.el().name.local == *local_name
    }

    fn has_namespace(&self, ns: &huginn_dom::Namespace) -> bool {
        self.el().name.ns == *ns
    }

    fn is_same_type(&self, other: &Self) -> bool {
        let (a, b) = (self.el(), other.el());
        a.name.local == b.name.local && a.name.ns == b.name.ns
    }

    fn attr_matches(
        &self,
        ns: &NamespaceConstraint<&Namespace>,
        local_name: &LocalName,
        operation: &AttrSelectorOperation<&AtomString>,
    ) -> bool {
        self.el().attrs.iter().any(|attr| {
            attr.name.local == local_name.0
                && match ns {
                    NamespaceConstraint::Any => true,
                    NamespaceConstraint::Specific(ns) => attr.name.ns == ns.0,
                }
                && operation.eval_str(&attr.value)
        })
    }

    fn match_non_ts_pseudo_class(
        &self,
        pc: &NonTSPseudoClass,
        _context: &mut MatchingContext<SelectorImpl>,
    ) -> bool {
        use NonTSPseudoClass as P;
        let state = |f: ElementState| self.element_state().intersects(f);
        match pc {
            P::Link | P::AnyLink => self.is_link(),
            P::Defined => true,
            P::Checked => state(ElementState::CHECKED),
            P::Disabled => state(ElementState::DISABLED),
            P::Enabled => state(ElementState::ENABLED),
            P::Required => state(ElementState::REQUIRED),
            P::Optional => state(ElementState::OPTIONAL_),
            P::ReadOnly => state(ElementState::READONLY),
            P::ReadWrite => state(ElementState::READWRITE),
            P::PlaceholderShown => state(ElementState::PLACEHOLDER_SHOWN),
            P::Valid => state(ElementState::VALID),
            P::Open => {
                (self.is_html(&local_name!("details")) || self.is_html(&local_name!("dialog")))
                    && self.has_attr(&local_name!("open"))
            }
            P::Lang(lang) => self.lang_matches(lang),
            P::ServoNonZeroBorder => {
                self.is_html(&local_name!("table"))
                    && self
                        .el()
                        .attr(&local_name!("border"))
                        .is_some_and(|b| b.trim().parse::<u32>().map_or(true, |n| n != 0))
            }
            P::Default => {
                self.is_checkable_checked()
                    || (self.is_html(&local_name!("button"))
                        && self
                            .el()
                            .attr(&local_name!("type"))
                            .is_none_or(|t| t.eq_ignore_ascii_case("submit")))
            }
            P::Hover => self.cx().hover.contains(&self.nid()),
            P::Active => self.cx().active.contains(&self.nid()),
            P::FocusWithin => self.cx().focus_within.contains(&self.nid()),
            P::Focus => self.cx().focus_within.first() == Some(&self.nid()),
            P::FocusVisible => {
                self.cx().focus_within.first() == Some(&self.nid())
                    && self.cx().doc.interaction().focus_visible
            }
            P::Autofill
            | P::CustomState(_)
            | P::Fullscreen
            | P::InRange
            | P::Indeterminate
            | P::Invalid
            | P::Modal
            | P::MozInert
            | P::MozMeterOptimum
            | P::MozMeterSubOptimum
            | P::MozMeterSubSubOptimum
            | P::MozTopmostModal
            | P::OutOfRange
            | P::PopoverOpen
            | P::Target
            | P::UserInvalid
            | P::UserValid
            | P::Visited => false,
        }
    }

    fn match_pseudo_element(
        &self,
        _pe: &PseudoElement,
        _context: &mut MatchingContext<SelectorImpl>,
    ) -> bool {
        false
    }

    fn apply_selector_flags(&self, flags: ElementSelectorFlags) {
        let self_flags = flags.for_self();
        if !self_flags.is_empty()
            && let Some(side) = self.side()
        {
            side.selector_flags
                .set(side.selector_flags.get() | self_flags);
        }
        let parent_flags = flags.for_parent();
        if !parent_flags.is_empty()
            && let Some(parent) = selectors::Element::parent_element(self)
            && let Some(side) = parent.side()
        {
            side.selector_flags
                .set(side.selector_flags.get() | parent_flags);
        }
    }

    fn is_link(&self) -> bool {
        let el = self.el();
        el.name.ns == ns!(html)
            && matches!(&*el.name.local, "a" | "area" | "link")
            && el.attr(&local_name!("href")).is_some()
    }

    fn is_html_slot_element(&self) -> bool {
        self.is_html(&local_name!("slot"))
    }

    fn assigned_slot(&self) -> Option<Self> {
        self.cx()
            .doc
            .assigned_slot(self.nid())
            .map(|s| self.with(s))
    }

    fn has_id(&self, id: &AtomIdent, case_sensitivity: CaseSensitivity) -> bool {
        match self.side() {
            Some(side) => side
                .id
                .as_ref()
                .is_some_and(|mine| case_sensitivity.eq_atom(mine, id)),
            None => self
                .el()
                .id()
                .is_some_and(|mine| case_sensitivity.eq(mine.as_bytes(), id.0.as_bytes())),
        }
    }

    fn has_class(&self, name: &AtomIdent, case_sensitivity: CaseSensitivity) -> bool {
        match self.side() {
            Some(side) => side
                .classes
                .iter()
                .any(|c| case_sensitivity.eq_atom(c, name)),
            None => self
                .el()
                .classes()
                .any(|c| case_sensitivity.eq(c.as_bytes(), name.0.as_bytes())),
        }
    }

    fn has_custom_state(&self, _name: &AtomIdent) -> bool {
        false
    }

    // `exportparts` read outer -> inner: the inner part name this host
    // exports as `name`.
    fn imported_part(&self, name: &AtomIdent) -> Option<AtomIdent> {
        self.part_mappings()
            .find(|&(_, outer)| *outer == *name.0)
            .map(|(inner, _)| GenericAtomIdent(Atom::from(inner)))
    }

    fn is_part(&self, name: &AtomIdent) -> bool {
        self.part_names().any(|p| *p == *name.0)
    }

    fn is_empty(&self) -> bool {
        !self
            .cx()
            .doc
            .children(self.nid())
            .any(|c| match &self.cx().doc.node(c).data {
                NodeData::Element(_) => true,
                NodeData::Text(t) => !t.is_empty(),
                _ => false,
            })
    }

    fn is_root(&self) -> bool {
        self.cx().doc.parent(self.nid()) == Some(NodeId::DOCUMENT)
    }

    fn add_element_unique_hashes(&self, filter: &mut BloomFilter) -> bool {
        each_relevant_element_hash(*self, |hash| filter.insert_hash(hash & BLOOM_HASH_MASK));
        true
    }
}

impl<'a> TElement for HuginnNode<'a> {
    type ConcreteNode = HuginnNode<'a>;
    type TraversalChildrenIterator = ChildIter<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn traversal_children(&self) -> LayoutIterator<Self::TraversalChildrenIterator> {
        LayoutIterator(ChildIter {
            cx: self.cx(),
            inner: self.cx().doc.flat_children(self.nid()),
        })
    }

    fn inheritance_parent(&self) -> Option<Self> {
        TNode::traversal_parent(self)
    }

    fn is_html_element(&self) -> bool {
        self.el().name.ns == ns!(html)
    }

    fn is_mathml_element(&self) -> bool {
        self.el().name.ns == ns!(mathml)
    }

    fn is_svg_element(&self) -> bool {
        self.el().name.ns == ns!(svg)
    }

    fn style_attribute(&self) -> Option<ArcBorrow<'_, Locked<PropertyDeclarationBlock>>> {
        self.side()?.style_attr.as_ref().map(|a| a.borrow_arc())
    }

    fn animation_rule(
        &self,
        _: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        None
    }

    fn transition_rule(
        &self,
        _: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        None
    }

    fn state(&self) -> ElementState {
        self.element_state()
    }

    fn has_part_attr(&self) -> bool {
        self.has_attr(&local_name!("part"))
    }

    fn exports_any_part(&self) -> bool {
        self.has_attr(&local_name!("exportparts"))
    }

    fn id(&self) -> Option<&Atom> {
        self.side()?.id.as_ref()
    }

    fn each_class<F>(&self, mut callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        match self.side() {
            Some(side) => side
                .classes
                .iter()
                .for_each(|c| callback(AtomIdent::cast(c))),
            None => self
                .el()
                .classes()
                .for_each(|c| callback(AtomIdent::cast(&Atom::from(c)))),
        }
    }

    fn each_custom_state<F>(&self, _callback: F)
    where
        F: FnMut(&AtomIdent),
    {
    }

    fn each_part<F>(&self, mut callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        for p in self.part_names() {
            callback(AtomIdent::cast(&Atom::from(p)));
        }
    }

    fn each_exported_part<F>(&self, name: &AtomIdent, mut callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        for (inner, outer) in self.part_mappings() {
            if *inner == *name.0 {
                callback(AtomIdent::cast(&Atom::from(outer)));
            }
        }
    }

    fn each_attr_name<F>(&self, mut callback: F)
    where
        F: FnMut(&LocalName),
    {
        for attr in &self.el().attrs {
            callback(GenericAtomIdent::cast(&attr.name.local));
        }
    }

    fn has_dirty_descendants(&self) -> bool {
        self.side().is_some_and(|s| s.dirty_descendants.get())
    }

    fn has_snapshot(&self) -> bool {
        false
    }

    fn handled_snapshot(&self) -> bool {
        true
    }

    unsafe fn set_handled_snapshot(&self) {}

    unsafe fn set_dirty_descendants(&self) {
        if let Some(s) = self.side() {
            s.dirty_descendants.set(true);
        }
    }

    unsafe fn unset_dirty_descendants(&self) {
        if let Some(s) = self.side() {
            s.dirty_descendants.set(false);
        }
    }

    fn store_children_to_process(&self, _n: isize) {
        unreachable!("no parallel traversal")
    }

    fn did_process_child(&self) -> isize {
        unreachable!("no parallel traversal")
    }

    unsafe fn ensure_data(&self) -> ElementDataMut<'_> {
        // SAFETY: the sequential traversal has exclusive access to the side
        // table, and borrows of one element's data never overlap.
        let cell = unsafe { &mut *self.data_cell().get() };
        cell.get_or_insert_with(ElementDataWrapper::default)
            .borrow_mut()
    }

    unsafe fn clear_data(&self) {
        // SAFETY: as in `ensure_data`.
        unsafe { *self.data_cell().get() = None };
    }

    fn has_data(&self) -> bool {
        self.side()
            .is_some_and(|s| unsafe { &*s.data.get() }.is_some())
    }

    fn borrow_data(&self) -> Option<ElementDataRef<'_>> {
        let side = self.side()?;
        // SAFETY: as in `ensure_data`.
        unsafe { &*side.data.get() }.as_ref().map(|w| w.borrow())
    }

    fn mutate_data(&self) -> Option<ElementDataMut<'_>> {
        let side = self.side()?;
        // SAFETY: as in `ensure_data`.
        unsafe { &*side.data.get() }
            .as_ref()
            .map(|w| w.borrow_mut())
    }

    /// Only `::before`/`::after`, and not on replaced elements (as in Chrome).
    fn may_generate_pseudo(&self, pseudo: &PseudoElement, _primary_style: &ComputedValues) -> bool {
        matches!(pseudo, PseudoElement::Before | PseudoElement::After)
            && !(self.el().name.ns == ns!(html)
                && matches!(
                    &*self.el().name.local,
                    "img"
                        | "input"
                        | "select"
                        | "textarea"
                        | "video"
                        | "audio"
                        | "canvas"
                        | "iframe"
                        | "embed"
                        | "object"
                        | "br"
                ))
    }

    fn skip_item_display_fixup(&self) -> bool {
        false
    }

    fn may_have_animations(&self) -> bool {
        false
    }

    fn set_may_have_animations(&self) {}

    fn has_animations(&self, _context: &SharedStyleContext) -> bool {
        false
    }

    fn has_css_animations(
        &self,
        _context: &SharedStyleContext,
        _pseudo: Option<PseudoElement>,
    ) -> bool {
        false
    }

    fn has_css_transitions(
        &self,
        _context: &SharedStyleContext,
        _pseudo: Option<PseudoElement>,
    ) -> bool {
        false
    }

    fn shadow_root(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        self.el().shadow_root.map(|r| self.with(r))
    }

    fn containing_shadow(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        self.cx()
            .doc
            .containing_shadow_root(self.nid())
            .map(|r| self.with(r))
    }

    fn lang_attr(&self) -> Option<AttrValue> {
        self.el().attr(&local_name!("lang")).map(AtomString::from)
    }

    fn match_element_lang(&self, override_lang: Option<Option<AttrValue>>, value: &Lang) -> bool {
        match override_lang {
            Some(Some(lang)) => lang_prefix_match(lang.as_ref(), value),
            Some(None) => false,
            None => self.lang_matches(value),
        }
    }

    fn is_html_document_body_element(&self) -> bool {
        self.is_html(&local_name!("body"))
            && self
                .cx()
                .doc
                .parent(self.nid())
                .is_some_and(|p| self.cx().doc.parent(p) == Some(NodeId::DOCUMENT))
    }

    fn synthesize_presentational_hints_for_legacy_attributes<'b, V>(
        &self,
        _visited_handling: VisitedHandlingMode,
        hints: &mut V,
    ) where
        V: Push<ApplicableDeclarationBlock<'b>>,
        Self: 'b,
    {
        if let Some(block) = self.side().and_then(|s| s.hints.as_ref()) {
            hints.push(ApplicableDeclarationBlock::from_declarations(
                StyleSourceBorrow::from_declarations(block.borrow_arc()),
                CascadeLevel::new(CascadeOrigin::PresHints),
                LayerOrder::root(),
            ));
        }
    }

    fn local_name(&self) -> &huginn_dom::LocalName {
        &self.el().name.local
    }

    fn namespace(&self) -> &huginn_dom::Namespace {
        &self.el().name.ns
    }

    fn query_container_size(
        &self,
        _display: &Display,
    ) -> euclid::default::Size2D<Option<app_units::Au>> {
        // Container queries need layout sizes; style runs before layout.
        Default::default()
    }

    fn has_selector_flags(&self, flags: ElementSelectorFlags) -> bool {
        self.side()
            .is_some_and(|s| s.selector_flags.get().contains(flags))
    }

    fn relative_selector_search_direction(&self) -> ElementSelectorFlags {
        let flags = self
            .side()
            .map_or(ElementSelectorFlags::empty(), |s| s.selector_flags.get());
        flags
            .intersection(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING)
    }

    fn get_attr(&self, attr: &LocalName, namespace: &Namespace) -> Option<String> {
        self.el()
            .attrs
            .iter()
            .find(|a| a.name.local == attr.0 && a.name.ns == namespace.0)
            .map(|a| a.value.to_string())
    }

    fn implicit_scope_for_sheet_in_shadow_root(
        _host: OpaqueElement,
        _sheet_index: usize,
    ) -> Option<ImplicitScopeRoot> {
        None
    }
}
