//! html5ever tree sink that builds a [`Document`] arena.

use std::borrow::Cow;
use std::cell::{Ref, RefCell};

use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{ParseOpts, parse_document};
use markup5ever::{Attribute, QualName};

use crate::{Document, NodeData, NodeId, StrTendril};

/// Parse an HTML document from a string.
pub fn parse_html(html: &str) -> Document {
    parse_document(Sink::document(), ParseOpts::default()).one(html)
}

/// Parse an HTML document from bytes. Invalid UTF-8 is replaced (U+FFFD).
///
/// Charset sniffing (BOM, `<meta charset>`, HTTP header) is not done yet;
/// the bytes are treated as UTF-8.
pub fn parse_html_bytes(bytes: &[u8]) -> Document {
    parse_document(Sink::document(), ParseOpts::default())
        .from_utf8()
        .one(bytes)
}

/// The tree sink. html5ever calls it through `&self`, so the document sits in
/// a `RefCell` during parsing only. The finished `Document` has no `RefCell`.
#[derive(Default)]
pub(crate) struct Sink {
    doc: RefCell<Document>,
    /// `<template shadowrootmode>` attaches a shadow root. True when parsing
    /// a document; false for `innerHTML` (HTML spec, fragment parsing).
    declarative_shadow: bool,
}

impl Sink {
    fn document() -> Sink {
        Sink {
            doc: RefCell::default(),
            declarative_shadow: true,
        }
    }
}

impl TreeSink for Sink {
    type Handle = NodeId;
    type Output = Document;
    type ElemName<'a> = Ref<'a, QualName>;

    fn finish(self) -> Document {
        self.doc.into_inner()
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn get_document(&self) -> NodeId {
        NodeId::DOCUMENT
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> Ref<'a, QualName> {
        Ref::map(self.doc.borrow(), |d| match &d.node(*target).data {
            NodeData::Element(e) => &e.name,
            _ => panic!("elem_name called on a non-element"),
        })
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<Attribute>,
        _flags: ElementFlags,
    ) -> NodeId {
        self.doc.borrow_mut().create_element(name, attrs)
    }

    fn create_comment(&self, text: StrTendril) -> NodeId {
        self.doc.borrow_mut().create_node(NodeData::Comment(text))
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> NodeId {
        self.doc
            .borrow_mut()
            .create_node(NodeData::ProcessingInstruction { target, data })
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        let mut doc = self.doc.borrow_mut();
        match child {
            NodeOrText::AppendNode(n) => doc.append_child(*parent, n),
            NodeOrText::AppendText(t) => append_text_tendril(&mut doc, *parent, None, t),
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        let has_parent = self.doc.borrow().parent(*element).is_some();
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        let mut doc = self.doc.borrow_mut();
        let n = doc.create_node(NodeData::Doctype {
            name,
            public_id,
            system_id,
        });
        doc.append_child(NodeId::DOCUMENT, n);
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        self.doc
            .borrow()
            .element(*target)
            .and_then(|e| e.template_contents)
            .expect("get_template_contents on a non-template")
    }

    fn allow_declarative_shadow_roots(&self, _intended_parent: &NodeId) -> bool {
        self.declarative_shadow
    }

    /// HTML spec, "in body" insertion mode, `<template>` start tag with
    /// `shadowrootmode`: attach a shadow root to the host and parse the
    /// template's contents into it. The template element itself is not
    /// inserted. Returns false (the template stays a normal element) if the
    /// host cannot get a shadow root.
    fn attach_declarative_shadow(
        &self,
        host: &NodeId,
        template: &NodeId,
        _attrs: &[Attribute],
    ) -> bool {
        let mut doc = self.doc.borrow_mut();
        let Some(root) = doc.attach_shadow(*host) else {
            return false;
        };
        if let NodeData::Element(e) = doc.node_data_mut(*template) {
            e.template_contents = Some(root);
        }
        true
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.doc.borrow_mut().set_quirks_mode(mode);
    }

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        let mut doc = self.doc.borrow_mut();
        let parent = doc
            .parent(*sibling)
            .expect("append_before_sibling: sibling has no parent");
        match new_node {
            NodeOrText::AppendNode(n) => doc.insert_before(parent, n, Some(*sibling)),
            NodeOrText::AppendText(t) => append_text_tendril(&mut doc, parent, Some(*sibling), t),
        }
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
        let mut doc = self.doc.borrow_mut();
        let Some(existing) = doc.element(*target).map(|e| e.attrs.clone()) else {
            return;
        };
        for a in attrs {
            if !existing.iter().any(|e| e.name == a.name) {
                // Attributes added by the parser are always in the null namespace
                // for HTML content; keep the qualified name as given.
                if let NodeData::Element(e) = &mut doc.node_data_mut(*target) {
                    e.attrs.push(a);
                }
            }
        }
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.doc.borrow_mut().detach(*target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        let mut doc = self.doc.borrow_mut();
        while let Some(c) = doc.first_child(*node) {
            doc.append_child(*new_parent, c);
        }
    }
}

/// Insert text before `before` (or at the end), merging with an adjacent text
/// node like the HTML spec requires.
fn append_text_tendril(
    doc: &mut Document,
    parent: NodeId,
    before: Option<NodeId>,
    text: StrTendril,
) {
    let prev = match before {
        Some(b) => doc.prev_sibling(b),
        None => doc.last_child(parent),
    };
    if let Some(prev) = prev
        && let NodeData::Text(t) = doc.node_data_mut(prev)
    {
        t.push_tendril(&text);
        return;
    }
    let n = doc.create_text_tendril(text);
    doc.insert_before(parent, n, before);
}

impl Document {
    /// Mutable access to node data for the parser. Does not bump the generation
    /// counter; only the parser uses it, before anyone reads the document.
    fn node_data_mut(&mut self, id: NodeId) -> &mut NodeData {
        &mut self.nodes[id.index()].data
    }
}
