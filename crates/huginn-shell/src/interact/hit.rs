//! Hit testing in the layout tree: which DOM node is at a document point.

use huginn_dom::NodeId;
use huginn_layout::{BoxId, BoxKind, LayoutTree, Rect};
use huginn_style::{PointerEvents, Position, Visibility};

/// The innermost DOM node at document point (`x`, `y`), in CSS px: the box
/// painted on top there ([`topmost_box`]), then the innermost inline element
/// fragment of that box's inline formatting context (an `<a>` inside a
/// paragraph has no box, only fragments), else the nearest ancestor box
/// with a node.
pub fn node_at(tree: &LayoutTree, x: f32, y: f32) -> Option<NodeId> {
    let hit = topmost_box(tree, x, y)?;
    if let Some(n) = fragment_at(tree, hit, x, y) {
        return Some(n);
    }
    let mut cur = Some(hit);
    while let Some(id) = cur {
        let b = tree.get(id);
        if b.node.is_some() {
            return b.node;
        }
        // An anonymous block inside a paragraph's IFC: the inline element
        // around it may still hold the point.
        if let Some(p) = b.parent
            && let Some(n) = fragment_at(tree, p, x, y)
        {
            return Some(n);
        }
        cur = b.parent;
    }
    None
}

/// The visible box painted on top at the point that takes pointer events
/// (`pointer-events: none` boxes let them through), in an approximation of CSS
/// paint order: a descendant over its ancestor; between two branches,
/// below their common ancestor, a higher `z-index` (of the outermost
/// positioned box on the branch that sets one) wins, then a branch with a
/// positioned box over an in-flow one, then the later branch in tree order.
pub fn topmost_box(tree: &LayoutTree, x: f32, y: f32) -> Option<BoxId> {
    let chain = |id: BoxId| {
        let mut out = vec![id];
        while let Some(p) = tree.get(*out.last().expect("not empty")).parent {
            out.push(p);
        }
        out
    };
    // (z, positioned, index of the branch root) of `path` (innermost first).
    let key = |path: &[BoxId]| {
        let (mut z, mut positioned) = (0, false);
        for &c in path {
            let s = &tree.get(c).style;
            if s.position != Position::Static {
                positioned = true;
                if let Some(zi) = s.z_index {
                    z = zi;
                }
            }
        }
        (z, positioned, path.last().map_or(0, |b| b.0))
    };
    let above = |a: BoxId, b: BoxId| {
        let (ca, cb) = (chain(a), chain(b));
        let Some(ia) = ca.iter().position(|n| cb.contains(n)) else {
            return a.0 > b.0;
        };
        let ib = cb
            .iter()
            .position(|n| *n == ca[ia])
            .expect("common ancestor is in both chains");
        match (ia, ib) {
            (0, _) => false,
            (_, 0) => true,
            _ => key(&ca[..ia]) > key(&cb[..ib]),
        }
    };
    tree.boxes
        .iter()
        .enumerate()
        .filter(|(_, b)| {
            contains(b.rect, x, y)
                && b.style.visibility == Visibility::Visible
                && b.style.pointer_events == PointerEvents::Auto
        })
        .map(|(i, _)| BoxId(i as u32))
        .reduce(|best, c| if above(c, best) { c } else { best })
}

/// The innermost inline fragment of `id`'s IFC that contains the point.
/// Fragments are in tree order of their elements, so the last hit is the
/// innermost (inline siblings do not overlap).
fn fragment_at(tree: &LayoutTree, id: BoxId, x: f32, y: f32) -> Option<NodeId> {
    let b = tree.get(id);
    if !matches!(b.kind, BoxKind::Inline) {
        return None;
    }
    let r = b.inline_fragments.start as usize..b.inline_fragments.end as usize;
    tree.fragments[r]
        .iter()
        .rev()
        .find(|f| contains(f.rect, x, y))
        .map(|f| f.node)
}

pub fn contains(r: Rect, x: f32, y: f32) -> bool {
    x >= r.x && x < r.right() && y >= r.y && y < r.bottom()
}
