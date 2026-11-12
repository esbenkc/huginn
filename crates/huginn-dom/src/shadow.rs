//! Shadow DOM: shadow roots, slot assignment and the flat tree.
//!
//! A shadow root is a [`NodeData::DocumentFragment`] that is linked to its
//! host (`ElementData::shadow_root` one way, `Document::shadow_hosts` the
//! other way). It is never a child of the host, so DOM walks (`children`,
//! `descendants`, selectors, `textContent`) do not see shadow trees, as in the
//! DOM spec. Rendering walks the *flat tree* instead: a host's flat children
//! are its shadow root's children, and a `<slot>`'s flat children are the
//! host children assigned to it (or its own children as fallback).
//!
//! Slot assignment is "named" (the default `slotAssignment`): an element goes
//! to the first slot in tree order whose `name` equals its `slot` attribute;
//! text nodes and elements without `slot` go to the first unnamed slot.

use crate::{Children, Document, NodeData, NodeId, local_name, ns};

/// Element names that may host a shadow root (DOM, "attach a shadow root",
/// step 2), besides valid custom element names.
const SHADOW_HOST_NAMES: &[&str] = &[
    "article",
    "aside",
    "blockquote",
    "body",
    "div",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "main",
    "nav",
    "p",
    "section",
    "span",
];

impl Document {
    /// Attach a new, empty shadow root to `host` and return it. Returns
    /// `None` if `host` cannot host one (not an element, or a name that the
    /// DOM spec does not allow) or already has one.
    pub fn attach_shadow(&mut self, host: NodeId) -> Option<NodeId> {
        let el = self.element(host)?;
        if el.shadow_root.is_some() || el.name.ns != ns!(html) {
            return None;
        }
        let name: &str = &el.name.local;
        let custom = name.contains('-') && name.starts_with(|c: char| c.is_ascii_lowercase());
        if !custom && !SHADOW_HOST_NAMES.contains(&name) {
            return None;
        }
        let root = self.push_node(NodeData::DocumentFragment);
        if let NodeData::Element(e) = &mut self.node_mut(host).data {
            e.shadow_root = Some(root);
        }
        self.shadow_hosts.insert(root, host);
        Some(root)
    }

    /// The shadow root of a shadow host.
    #[inline]
    pub fn shadow_root(&self, host: NodeId) -> Option<NodeId> {
        self.element(host)?.shadow_root
    }

    /// The host of a shadow root; `None` for every other node.
    #[inline]
    pub fn shadow_host(&self, root: NodeId) -> Option<NodeId> {
        if self.shadow_hosts.is_empty() {
            return None;
        }
        self.shadow_hosts.get(&root).copied()
    }

    /// True if any shadow root exists. Lets flat-tree code skip all slot
    /// work on pages without shadow DOM.
    #[inline]
    pub fn has_shadow_roots(&self) -> bool {
        !self.shadow_hosts.is_empty()
    }

    /// Set the CSS of a shadow root's `adoptedStyleSheets` (all sheets
    /// joined in order; empty = none). They cascade after the shadow tree's
    /// `<style>` elements.
    pub fn set_adopted_css(&mut self, root: NodeId, css: &str) {
        if css.is_empty() {
            self.adopted_css.remove(&root);
        } else {
            self.adopted_css.insert(root, css.into());
        }
        self.generation += 1;
    }

    /// The CSS of a shadow root's `adoptedStyleSheets`.
    pub fn adopted_css(&self, root: NodeId) -> Option<&str> {
        self.adopted_css.get(&root).map(|s| &**s)
    }

    /// The root of the tree `id` is in: the document node, a shadow root, a
    /// fragment or the topmost node of a detached subtree.
    pub fn root(&self, id: NodeId) -> NodeId {
        self.ancestors(id).last().unwrap_or(id)
    }

    /// The shadow root whose tree contains `id` (not crossing hosts).
    pub fn containing_shadow_root(&self, id: NodeId) -> Option<NodeId> {
        if !self.has_shadow_roots() {
            return None;
        }
        let root = self.root(id);
        self.shadow_hosts.contains_key(&root).then_some(root)
    }

    /// True for an HTML `<slot>` element inside a shadow tree.
    fn is_slot_in_shadow(&self, id: NodeId) -> bool {
        self.element(id)
            .is_some_and(|e| e.is_html(&local_name!("slot")))
            && self.containing_shadow_root(id).is_some()
    }

    /// The slot `node` is assigned to ("find a slot", named assignment):
    /// `node` is an element or text child of a shadow host, and its shadow
    /// tree has a slot with a matching name.
    pub fn assigned_slot(&self, node: NodeId) -> Option<NodeId> {
        if !self.has_shadow_roots() {
            return None;
        }
        let name = match &self.node(node).data {
            NodeData::Element(e) => e.attr(&local_name!("slot")).unwrap_or(""),
            NodeData::Text(_) => "",
            _ => return None,
        };
        let root = self.shadow_root(self.parent(node)?)?;
        self.descendants(root).find(|&n| {
            self.element(n).is_some_and(|e| {
                e.is_html(&local_name!("slot"))
                    && e.attr(&local_name!("name")).unwrap_or("") == name
            })
        })
    }

    /// Children of `id` in the flat tree: the shadow root's children for a
    /// shadow host, the assigned nodes (or else its own children) for a slot
    /// in a shadow tree, and the DOM children otherwise.
    pub fn flat_children(&self, id: NodeId) -> FlatChildren<'_> {
        if self.has_shadow_roots() {
            if let Some(root) = self.shadow_root(id) {
                return FlatChildren::Plain(self.children(root));
            }
            if self.is_slot_in_shadow(id)
                && let Some(root) = self.containing_shadow_root(id)
                && let Some(&host) = self.shadow_hosts.get(&root)
                && self
                    .children(host)
                    .any(|c| self.assigned_slot(c) == Some(id))
            {
                return FlatChildren::Assigned {
                    doc: self,
                    slot: id,
                    inner: self.children(host),
                };
            }
        }
        FlatChildren::Plain(self.children(id))
    }

    /// Parent of `id` in the flat tree: the assigned slot for a slotted
    /// node, the host for a child of a shadow root, else the DOM parent.
    /// Host children that no slot takes are not in the flat tree; for them
    /// this returns the DOM parent (the host).
    pub fn flat_parent(&self, id: NodeId) -> Option<NodeId> {
        let parent = self.parent(id)?;
        if !self.has_shadow_roots() {
            return Some(parent);
        }
        if let Some(slot) = self.assigned_slot(id) {
            return Some(slot);
        }
        Some(self.shadow_host(parent).unwrap_or(parent))
    }

    /// `id` and its shadow-including descendants in shadow-including tree
    /// order: a host is followed by its shadow tree, then its children.
    pub fn shadow_including_descendants(&self, id: NodeId) -> ShadowIncludingDescendants<'_> {
        ShadowIncludingDescendants {
            doc: self,
            stack: vec![id],
        }
    }
}

/// Iterator returned by [`Document::flat_children`].
pub enum FlatChildren<'a> {
    Plain(Children<'a>),
    /// The host's children that are assigned to `slot`.
    Assigned {
        doc: &'a Document,
        slot: NodeId,
        inner: Children<'a>,
    },
}

impl Iterator for FlatChildren<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        match self {
            FlatChildren::Plain(c) => c.next(),
            FlatChildren::Assigned { doc, slot, inner } => {
                inner.find(|&c| doc.assigned_slot(c) == Some(*slot))
            }
        }
    }
}

/// Iterator returned by [`Document::shadow_including_descendants`].
pub struct ShadowIncludingDescendants<'a> {
    doc: &'a Document,
    stack: Vec<NodeId>,
}

impl Iterator for ShadowIncludingDescendants<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        let cur = self.stack.pop()?;
        let doc = self.doc;
        let mut child = doc.last_child(cur);
        while let Some(c) = child {
            self.stack.push(c);
            child = doc.prev_sibling(c);
        }
        if let Some(root) = doc.element(cur).and_then(|e| e.shadow_root) {
            self.stack.push(root);
        }
        Some(cur)
    }
}

#[cfg(test)]
mod tests {
    use crate::parse_html;

    #[test]
    fn declarative_shadow_root_slots_light_children() {
        let doc = parse_html(
            "<div id=h><template shadowrootmode=open><b id=s1>[<slot name=a></slot>]</b><slot id=d>fb</slot></template>\
             <i id=x slot=a>x</i>t<u id=y slot=zz>y</u></div>",
        );
        let host = doc.element_by_id("h").unwrap();
        let root = doc
            .shadow_root(host)
            .expect("declarative shadow root attached");
        assert_eq!(doc.shadow_host(root), Some(host));
        // The template is not a DOM child; the light children are.
        let kids: Vec<_> = doc.children(host).collect();
        assert_eq!(kids.len(), 3);
        let (x, t, y) = (kids[0], kids[1], kids[2]);
        let flat: Vec<_> = doc.flat_children(host).collect();
        assert_eq!(flat.len(), 2, "the shadow root's children");
        let slot_a = doc
            .first_child(flat[0])
            .and_then(|n| doc.next_sibling(n))
            .unwrap();
        let d = flat[1];
        assert_eq!(doc.assigned_slot(x), Some(slot_a));
        assert_eq!(doc.assigned_slot(t), Some(d));
        assert_eq!(doc.assigned_slot(y), None, "no slot named zz");
        assert_eq!(doc.flat_children(slot_a).collect::<Vec<_>>(), vec![x]);
        assert_eq!(doc.flat_children(d).collect::<Vec<_>>(), vec![t]);
        assert_eq!(doc.flat_parent(x), Some(slot_a));
        assert_eq!(doc.flat_parent(flat[0]), Some(host));
        // Shadow-including order: host, shadow tree, then light children.
        let order: Vec<_> = doc.shadow_including_descendants(host).collect();
        assert_eq!(order[0], host);
        assert_eq!(order[1], root);
        assert!(order.iter().position(|&n| n == x) > order.iter().position(|&n| n == d));
        // getElementById does not see into shadow trees.
        assert_eq!(doc.element_by_id("s1"), None);
        assert_eq!(doc.root(flat[0]), root);
        assert_ne!(doc.root(x), root);
    }

    #[test]
    fn unassigned_slot_shows_fallback_and_bad_hosts_keep_template() {
        let doc = parse_html(
            "<x-a id=h><template shadowrootmode=open><slot id=d>fb</slot></template></x-a>\
             <a id=a><template shadowrootmode=open>x</template></a>",
        );
        let host = doc.element_by_id("h").unwrap();
        let root = doc.shadow_root(host).unwrap();
        let slot = doc.first_child(root).unwrap();
        let fb: Vec<_> = doc.flat_children(slot).collect();
        assert_eq!(fb.len(), 1);
        assert_eq!(doc.text(fb[0]), Some("fb"));
        // <a> cannot host a shadow root: the template stays a normal element.
        let a = doc.element_by_id("a").unwrap();
        assert!(doc.shadow_root(a).is_none());
        assert!(doc.first_child(a).is_some());
    }
}
