//! Fragment parsing, serialization and cloning for script access
//! (`innerHTML`, `outerHTML`, `insertAdjacentHTML`, `cloneNode`).

use std::io;

use html5ever::ParseOpts;
use html5ever::serialize::{SerializeOpts, Serializer, TraversalScope};
use html5ever::tendril::TendrilSink;

use crate::parse::Sink;
use crate::{Document, NodeData, NodeId, QualName, local_name, ns};

/// Parse `html` as the contents of `context` (the `innerHTML` setter
/// algorithm) and return the new top-level nodes, detached, in order.
///
/// `context` should be an element; for any other node the fragment is parsed
/// as if inside `<body>`. Scripts in the result are inert: the caller decides
/// whether to run them (`innerHTML` never does).
pub fn parse_fragment(doc: &mut Document, context: NodeId, html: &str) -> Vec<NodeId> {
    let name = match doc.element(context) {
        Some(e) => e.name.clone(),
        None => QualName::new(None, ns!(html), local_name!("body")),
    };
    // html5ever builds the fragment under an `<html>` root in a scratch
    // document; copy the root's children over. Tendrils are refcounted, so
    // text is shared, not copied.
    let scratch: Document = html5ever::parse_fragment(
        Sink::default(),
        ParseOpts::default(),
        name,
        Vec::new(),
        true,
    )
    .one(html);
    let Some(root) = scratch.document_element() else {
        return Vec::new();
    };
    scratch
        .children(root)
        .map(|c| import(doc, &scratch, c))
        .collect()
}

/// Deep-copy `node` (and its subtree) from `src` into `dst`. The copy is detached.
fn import(dst: &mut Document, src: &Document, node: NodeId) -> NodeId {
    let top = copy_one(dst, src, node);
    // (source parent, destination parent) pairs still to fill.
    let mut stack = vec![(node, top)];
    while let Some((s, d)) = stack.pop() {
        let src_children = match src.element(s).and_then(|e| e.template_contents) {
            // Template contents live in a fragment; copy them into the copy's fragment.
            Some(frag) => {
                let dst_frag = dst
                    .element(d)
                    .and_then(|e| e.template_contents)
                    .expect("template copy has contents");
                stack.push((frag, dst_frag));
                src.children(s)
            }
            None => src.children(s),
        };
        for c in src_children {
            let copy = copy_one(dst, src, c);
            dst.append_child(d, copy);
            stack.push((c, copy));
        }
    }
    top
}

/// Copy one node without children.
fn copy_one(dst: &mut Document, src: &Document, node: NodeId) -> NodeId {
    match &src.node(node).data {
        NodeData::Element(e) => dst.create_element(e.name.clone(), e.attrs.clone()),
        other => dst.create_node(other.clone()),
    }
}

impl Document {
    /// DOM `cloneNode`: a detached copy of `id`, with its subtree if `deep`.
    /// Template contents are copied with the template.
    pub fn clone_node(&mut self, id: NodeId, deep: bool) -> NodeId {
        let top = self.clone_one(id);
        if !deep {
            return top;
        }
        let mut stack = vec![(id, top)];
        while let Some((s, d)) = stack.pop() {
            if let Some(frag) = self.element(s).and_then(|e| e.template_contents) {
                let dst_frag = self
                    .element(d)
                    .and_then(|e| e.template_contents)
                    .expect("template copy has contents");
                stack.push((frag, dst_frag));
            }
            let children: Vec<NodeId> = self.children(s).collect();
            for c in children {
                let copy = self.clone_one(c);
                self.append_child(d, copy);
                stack.push((c, copy));
            }
        }
        top
    }

    fn clone_one(&mut self, id: NodeId) -> NodeId {
        match &self.node(id).data {
            NodeData::Element(e) => {
                let (name, attrs) = (e.name.clone(), e.attrs.clone());
                self.create_element(name, attrs)
            }
            other => {
                let data = other.clone();
                self.create_node(data)
            }
        }
    }
}

/// Serialize `id` as HTML: the node itself and its subtree if `include_self`
/// (`outerHTML`), else only its children (`innerHTML`). Text and attribute
/// values are escaped; children of `<script>`, `<style>` and friends are not.
pub fn serialize(doc: &Document, id: NodeId, include_self: bool) -> String {
    let scope = if include_self {
        TraversalScope::IncludeNode
    } else {
        TraversalScope::ChildrenOnly(doc.element(id).map(|e| e.name.clone()))
    };
    let opts = SerializeOpts {
        scripting_enabled: true,
        traversal_scope: scope,
        create_missing_parent: true,
    };
    let mut out = Vec::new();
    // Writing to a Vec cannot fail.
    let _ = html5ever::serialize::serialize(&mut out, &NodeRef { doc, id }, opts);
    // The serializer only writes the UTF-8 it was given plus ASCII markup.
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

struct NodeRef<'a> {
    doc: &'a Document,
    id: NodeId,
}

/// Children of `id` as serialized: template contents for `<template>`.
fn serialized_children(doc: &Document, id: NodeId) -> crate::Children<'_> {
    match doc.element(id).and_then(|e| e.template_contents) {
        Some(frag) => doc.children(frag),
        None => doc.children(id),
    }
}

impl html5ever::serialize::Serialize for NodeRef<'_> {
    fn serialize<S: Serializer>(&self, s: &mut S, scope: TraversalScope) -> io::Result<()> {
        enum Step {
            Open(NodeId),
            Close(NodeId),
        }
        let doc = self.doc;
        // Explicit stack: deep DOMs must not overflow the native stack.
        let mut stack: Vec<Step> = Vec::new();
        match scope {
            TraversalScope::IncludeNode => stack.push(Step::Open(self.id)),
            TraversalScope::ChildrenOnly(_) => {
                let kids: Vec<NodeId> = serialized_children(doc, self.id).collect();
                stack.extend(kids.into_iter().rev().map(Step::Open));
            }
        }
        while let Some(step) = stack.pop() {
            match step {
                Step::Close(n) => {
                    if let Some(e) = doc.element(n) {
                        s.end_elem(e.name.clone())?;
                    }
                }
                Step::Open(n) => match &doc.node(n).data {
                    NodeData::Element(e) => {
                        s.start_elem(
                            e.name.clone(),
                            e.attrs.iter().map(|a| (&a.name, &a.value[..])),
                        )?;
                        stack.push(Step::Close(n));
                        let kids: Vec<NodeId> = serialized_children(doc, n).collect();
                        stack.extend(kids.into_iter().rev().map(Step::Open));
                    }
                    NodeData::Text(t) => s.write_text(t)?,
                    NodeData::Comment(t) => s.write_comment(t)?,
                    NodeData::Doctype { name, .. } => s.write_doctype(name)?,
                    NodeData::ProcessingInstruction { target, data } => {
                        s.write_processing_instruction(target, data)?
                    }
                    NodeData::Document | NodeData::DocumentFragment => {
                        let kids: Vec<NodeId> = doc.children(n).collect();
                        stack.extend(kids.into_iter().rev().map(Step::Open));
                    }
                },
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_html;

    #[test]
    fn fragment_round_trips_through_serialize() {
        let mut doc = parse_html("<ul id=l></ul>");
        let ul = doc.element_by_id("l").unwrap();
        // <li> end tags are implied; the parser must use the <ul> context.
        let nodes = parse_fragment(&mut doc, ul, "<li class=a>one<li>two &amp; <b>three</b>");
        assert_eq!(nodes.len(), 2);
        for n in nodes {
            doc.append_child(ul, n);
        }
        assert_eq!(
            serialize(&doc, ul, false),
            r#"<li class="a">one</li><li>two &amp; <b>three</b></li>"#
        );
        assert_eq!(
            serialize(&doc, ul, true),
            r#"<ul id="l"><li class="a">one</li><li>two &amp; <b>three</b></li></ul>"#
        );
    }

    #[test]
    fn table_context_and_raw_text() {
        let mut doc = parse_html("<table><tbody id=t></tbody></table><div id=d></div>");
        let tbody = doc.element_by_id("t").unwrap();
        let rows = parse_fragment(&mut doc, tbody, "<tr><td>x</td></tr>");
        assert_eq!(doc.element(rows[0]).unwrap().name.local, local_name!("tr"));
        let div = doc.element_by_id("d").unwrap();
        let n = parse_fragment(&mut doc, div, "<script>if (a < b) {}</script>");
        doc.append_child(div, n[0]);
        assert_eq!(
            serialize(&doc, div, false),
            "<script>if (a < b) {}</script>"
        );
    }

    #[test]
    fn deep_clone_copies_subtree_and_is_detached() {
        let mut doc = parse_html("<div id=a title=t><p>x<i>y</i></p></div>");
        let a = doc.element_by_id("a").unwrap();
        let c = doc.clone_node(a, true);
        assert_eq!(doc.parent(c), None);
        assert_eq!(serialize(&doc, c, true), serialize(&doc, a, true));
        let shallow = doc.clone_node(a, false);
        assert_eq!(doc.first_child(shallow), None);
        assert_eq!(doc.attribute(shallow, &local_name!("title")), Some("t"));
    }
}
