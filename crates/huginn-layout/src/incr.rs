//! Keeping the box tree between layout passes.
//!
//! A pass used to build every box again. With [`Kept`] the engine keeps the
//! tree of the last pass, and the next pass uses the box of a DOM node again
//! when nothing the box was built from has changed:
//!
//! - the node's subtree in the flat tree: names, attributes, text, and the
//!   images of replaced elements ([`Change::clean`], from a hash per subtree),
//! - the computed style of every node in the subtree, and its `::before` and
//!   `::after` styles,
//! - what the box got from outside its subtree: inherited text decorations
//!   and the list marker text (checked where the box is used again),
//! - the fonts, the text index setting and the number of loaded images
//!   (else nothing is used again in that pass).
//!
//! A kept box brings its whole subtree: Taffy styles, shaped text, list
//! markers. Boxes the new tree does not use are freed after the build
//! (`LTree::sweep`). The layout step then runs on the whole tree as on a new
//! one: the layout caches are not kept (see `LTree::sweep` for why).
//!
//! The engine must lay out only one document while this is on: boxes are
//! found by DOM node id.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use huginn_dom::{Document, NodeData, NodeId, local_name};
use huginn_style::{ComputedStyle, PseudoElement, StyleMap};

use crate::LayoutInput;
use crate::tree::LTree;

/// What the engine keeps from one pass to the next.
#[derive(Default)]
pub(crate) struct Kept {
    pub tree: LTree,
    /// Per DOM node: slot + 1 of the box `Builder::build_block` made for it.
    boxes: Vec<u32>,
    /// Per DOM node: hash of its flat-tree subtree in the last pass.
    subtree: Vec<u64>,
    /// Per DOM node: its style, `::before` style and `::after` style then.
    styles: Vec<[Option<Arc<ComputedStyle>>; 3]>,
    /// The state of the last pass that all of the above depends on.
    world: Option<World>,
}

/// What must be the same in two passes for any box to be used again.
#[derive(PartialEq)]
pub(crate) struct World {
    pub fonts_epoch: u64,
    pub text_index: bool,
    pub images: usize,
    pub quirks: bool,
}

/// The result of comparing the document with the last pass.
pub(crate) struct Change {
    /// Per DOM node: its subtree and all styles in it are as in the last pass.
    pub clean: Vec<bool>,
}

impl Change {
    pub fn is_clean(&self, n: NodeId) -> bool {
        self.clean.get(n.index()).copied().unwrap_or(false)
    }
}

fn same_style(a: &Option<Arc<ComputedStyle>>, b: &Option<Arc<ComputedStyle>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a == b,
        _ => false,
    }
}

struct Walk<'a> {
    doc: &'a Document,
    styles: &'a StyleMap,
    input: &'a LayoutInput<'a>,
    old_subtree: &'a [u64],
    old_styles: &'a [[Option<Arc<ComputedStyle>>; 3]],
    subtree: Vec<u64>,
    new_styles: Vec<[Option<Arc<ComputedStyle>>; 3]>,
    clean: Vec<bool>,
    /// Compare with the last pass (false: everything counts as changed).
    compare: bool,
}

impl Walk<'_> {
    /// Hash the subtree of `n` and compare it with the last pass. Returns
    /// (hash, the subtree and its styles are unchanged).
    fn visit(&mut self, n: NodeId) -> (u64, bool) {
        let i = n.index();
        let mut h = std::hash::DefaultHasher::new();
        let mut styles_same = true;
        match &self.doc.node(n).data {
            NodeData::Text(_) => {
                1u8.hash(&mut h);
                self.doc.text(n).unwrap_or("").hash(&mut h);
            }
            NodeData::Element(e) => {
                2u8.hash(&mut h);
                e.name.ns.hash(&mut h);
                e.name.local.hash(&mut h);
                for a in &e.attrs {
                    a.name.local.hash(&mut h);
                    a.value[..].hash(&mut h);
                }
                e.attrs.len().hash(&mut h);
                // Replaced content: the image the element shows now.
                let img = self.input.images.get_for_node(n.to_raw()).or_else(|| {
                    if !e.is_html(&local_name!("img")) {
                        return None;
                    }
                    let src = e.attr(&local_name!("src"))?;
                    let url = huginn_net::resolve_url(self.input.base_url, src)?;
                    self.input.images.get(url.as_str())
                });
                if let Some(img) = img {
                    (
                        Arc::as_ptr(img.pixmap()) as usize,
                        img.width(),
                        img.height(),
                    )
                        .hash(&mut h);
                }
                let now = [
                    self.styles.get(n).cloned(),
                    self.styles.pseudo(n, PseudoElement::Before).cloned(),
                    self.styles.pseudo(n, PseudoElement::After).cloned(),
                ];
                styles_same = self.compare
                    && self
                        .old_styles
                        .get(i)
                        .is_some_and(|old| old.iter().zip(&now).all(|(a, b)| same_style(a, b)));
                self.new_styles[i] = now;
            }
            _ => 0u8.hash(&mut h),
        }
        let children: Vec<NodeId> = self.doc.flat_children(n).collect();
        for c in children {
            let (ch, same) = self.visit(c);
            ch.hash(&mut h);
            styles_same &= same;
        }
        let hash = h.finish();
        self.subtree[i] = hash;
        let clean = styles_same && self.compare && self.old_subtree.get(i) == Some(&hash);
        self.clean[i] = clean;
        (hash, styles_same)
    }
}

impl Kept {
    /// Start a pass: find what changed since the last one, and remember this
    /// pass's document for the next. `world` is this pass's state.
    pub fn begin(&mut self, input: &LayoutInput<'_>, world: World) -> Change {
        let doc = input.document;
        let n = doc.len();
        let compare = self.world.as_ref() == Some(&world);
        let mut walk = Walk {
            doc,
            styles: input.styles,
            input,
            old_subtree: &self.subtree,
            old_styles: &self.styles,
            subtree: vec![0; n],
            new_styles: vec![[None, None, None]; n],
            clean: vec![false; n],
            compare,
        };
        walk.visit(NodeId::DOCUMENT);
        let Walk {
            subtree,
            new_styles,
            clean,
            ..
        } = walk;
        self.subtree = subtree;
        self.styles = new_styles;
        self.boxes.resize(n, 0);
        if !compare {
            // No box can be used again; the sweep frees them all.
            self.boxes.iter_mut().for_each(|b| *b = 0);
        }
        self.world = None;
        Change { clean }
    }

    /// The pass is over and its tree is good for the next one, unless the
    /// fonts changed while it ran (`world` is the state at its end).
    pub fn end(&mut self, started: World, ended: World) {
        self.world = (started == ended).then_some(ended);
    }

    /// The kept box of `n`, if there is one.
    pub fn box_of(&self, n: NodeId) -> Option<taffy::NodeId> {
        match self.boxes.get(n.index()) {
            Some(&b) if b != 0 => Some(taffy::NodeId::from(b as usize - 1)),
            _ => None,
        }
    }

    pub fn set_box(&mut self, n: NodeId, id: taffy::NodeId) {
        if let Some(b) = self.boxes.get_mut(n.index()) {
            *b = usize::from(id) as u32 + 1;
        }
    }

    pub fn clear_box(&mut self, n: NodeId) {
        if let Some(b) = self.boxes.get_mut(n.index()) {
            *b = 0;
        }
    }

    /// A tree slot was freed: forget the box if it was `n`'s.
    pub fn forget(&mut self, n: Option<NodeId>, slot: usize) {
        if let Some(b) = n.and_then(|n| self.boxes.get_mut(n.index()))
            && *b == slot as u32 + 1
        {
            *b = 0;
        }
    }
}
