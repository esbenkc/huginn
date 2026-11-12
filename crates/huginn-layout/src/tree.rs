//! The layout tree: our box tree, stored so that Taffy's algorithms can run
//! on it directly (Taffy's low-level API).
//!
//! Taffy lays out block, flex and grid containers. Our own code lays out
//! inline formatting contexts (`inline.rs`, with Parley) and tables
//! (`table.rs`). Because both are dispatched from the same
//! `compute_child_layout`, an inline formatting context can size its
//! inline-blocks with the real available width, and a table can measure its
//! cells, at any depth.

use std::sync::Arc;

use huginn_dom::NodeId;
use huginn_net::DecodedImage;
use huginn_style::ComputedStyle;
use taffy::prelude as tf;
use taffy::{
    BlockContext, Cache, CacheTree, DetailedLayoutInfo, Layout, LayoutBlockContainer,
    LayoutContainingBlock, LayoutFlexboxContainer, LayoutGridContainer, LayoutInput, LayoutOutput,
    LayoutPartialTree, RunMode, TraversePartialTree, TraverseTree,
};

use crate::inline::Ifc;
use crate::table::Table;

pub(crate) type TStyle = taffy::Style<String>;

/// What our code does with a node, beyond Taffy's `display`.
pub(crate) enum Kind {
    /// Block, flow-root, flex or grid container: Taffy, by `ts.display`.
    Container,
    /// A block container whose content is one inline formatting context.
    /// Its box children are the atomic inlines (inline-blocks, images).
    Ifc(Box<Ifc>),
    /// Replaced element with an intrinsic size.
    Replaced {
        width: f32,
        height: f32,
        image: Option<DecodedImage>,
    },
    /// Table: laid out by `table.rs`. Box children: captions, then row groups.
    Table(Box<Table>),
    /// Row group or row: positioned by the table algorithm.
    TablePart,
}

pub(crate) struct LNode {
    /// Generating element; `None` for anonymous boxes and pseudo-elements.
    pub node: Option<NodeId>,
    pub style: Arc<ComputedStyle>,
    pub kind: Kind,
    /// Box children, in tree order. Taffy sees the same list.
    pub children: Vec<tf::NodeId>,
    pub ts: TStyle,
    pub cache: Cache,
    /// Unrounded layout. `location` is relative to the parent's border box,
    /// or to the containing block's border box for absolute/fixed boxes.
    pub layout: Layout,
    /// Absolute/fixed boxes whose containing block is this node.
    pub hoisted: Vec<tf::NodeId>,
    pub detailed: DetailedLayoutInfo<String>,
    /// List marker (outside position), shaped at build time.
    pub marker: Option<Box<Marker>>,
    /// Table cells: vertical offset of the content (vertical-align).
    pub content_dy: f32,
    /// Border-collapse: used border widths differ from the style.
    pub border_override: Option<huginn_style::Edges<f32>>,
    /// Baselines of the last size-only (`ComputeSize`) run, by its output
    /// size. Taffy's measure cache keeps only sizes, so a cache hit would
    /// lose them; see `CacheTree::cache_get`.
    pub measured_baselines: Option<(tf::Size<f32>, taffy::Baselines)>,
}

/// An outside list marker.
pub(crate) struct Marker {
    /// The marker text ("1. ", "• ") shaped in the list item's font.
    pub layout: parley::Layout<crate::inline::Brush>,
    /// `disc`, `circle` and `square`: Blink paints a shape instead of the
    /// text, sized from the first available font's ascent (whole pixels).
    pub symbol: Option<(crate::DecorationShape, f32)>,
}

pub(crate) struct LTree {
    pub nodes: Vec<LNode>,
}

#[inline]
pub(crate) fn ix(id: tf::NodeId) -> usize {
    usize::from(id)
}

pub(crate) struct ChildIter<'a>(std::slice::Iter<'a, tf::NodeId>);

impl Iterator for ChildIter<'_> {
    type Item = tf::NodeId;
    fn next(&mut self) -> Option<tf::NodeId> {
        self.0.next().copied()
    }
}

impl LTree {
    pub fn push(&mut self, n: LNode) -> tf::NodeId {
        self.nodes.push(n);
        tf::NodeId::from(self.nodes.len() - 1)
    }

    pub fn node(&self, id: tf::NodeId) -> &LNode {
        &self.nodes[ix(id)]
    }

    pub fn node_mut(&mut self, id: tf::NodeId) -> &mut LNode {
        &mut self.nodes[ix(id)]
    }

    /// Lay out the whole tree against the viewport.
    pub fn compute_root(&mut self, root: tf::NodeId, avail: tf::Size<tf::AvailableSpace>) {
        taffy::compute_root_layout(self, root, avail);
    }

    fn dispatch(
        &mut self,
        id: tf::NodeId,
        inputs: LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> LayoutOutput {
        if inputs.run_mode == RunMode::PerformHiddenLayout {
            return taffy::compute_hidden_layout(self, id);
        }
        taffy::compute_cached_layout(self, id, inputs, |tree, id, inputs| {
            enum Algo {
                Ifc,
                Table,
                Replaced(f32, f32),
                Empty,
                Hidden,
                Block,
                FlowRoot,
                Flex,
                Grid,
            }
            let algo = {
                let n = &tree.nodes[ix(id)];
                match &n.kind {
                    Kind::Ifc(_) => Algo::Ifc,
                    Kind::Table(_) => Algo::Table,
                    Kind::Replaced { width, height, .. } => Algo::Replaced(*width, *height),
                    // Rows and row groups are positioned by their table.
                    Kind::TablePart => Algo::Empty,
                    Kind::Container => match (n.ts.display, !n.children.is_empty()) {
                        (tf::Display::None, _) => Algo::Hidden,
                        (tf::Display::Block, true) => Algo::Block,
                        (tf::Display::FlowRoot, true) => Algo::FlowRoot,
                        (tf::Display::Flex, true) => Algo::Flex,
                        (tf::Display::Grid, true) => Algo::Grid,
                        _ => Algo::Empty,
                    },
                }
            };
            let mut out = match algo {
                Algo::Ifc => crate::inline::compute(tree, id, inputs, block_ctx),
                Algo::Table => crate::table::compute(tree, id, inputs),
                Algo::Replaced(w, h) => {
                    // CSS Sizing 3 §5.2.1c: for the min-content contribution of
                    // a replaced box, cyclic percentages (the containing block
                    // width depends on this contribution) resolve against zero.
                    // So `max-width: 100%` images do not widen auto tracks and
                    // shrink-to-fit boxes (Blink does the same).
                    let mut inputs = inputs;
                    if inputs.available_space.width == tf::AvailableSpace::MinContent
                        && inputs.known_dimensions.width.is_none()
                        && inputs.parent_size.width.is_none()
                    {
                        inputs.parent_size.width = Some(0.0);
                    }
                    let limits = replaced_limits(&tree.nodes[ix(id)].ts, inputs.parent_size);
                    taffy::compute_leaf_layout(
                        inputs,
                        &tree.nodes[ix(id)].ts,
                        crate::convert::resolve_calc,
                        |known, _| replaced_size(w, h, known, limits),
                    )
                }
                Algo::Empty => taffy::compute_leaf_layout(
                    inputs,
                    &tree.nodes[ix(id)].ts,
                    crate::convert::resolve_calc,
                    |_, _| tf::Size::ZERO,
                ),
                Algo::Hidden => taffy::compute_hidden_layout(tree, id),
                Algo::Block => taffy::compute_block_layout(tree, id, inputs, block_ctx),
                Algo::FlowRoot => taffy::compute_block_layout(tree, id, inputs, None),
                Algo::Flex => taffy::compute_flexbox_layout(tree, id, inputs),
                Algo::Grid => taffy::compute_grid_layout(tree, id, inputs),
            };
            if inputs.run_mode == RunMode::PerformLayout {
                taffy::compute_oof_layout(tree, id, &mut out);
            }
            out
        })
    }
}

/// Content-box `min-width`, `max-width`, `min-height`, `max-height` of a
/// replaced box (`None`: no constraint, or a percentage of an indefinite size).
#[derive(Copy, Clone, Default)]
pub(crate) struct Limits {
    min: tf::Size<Option<f32>>,
    max: tf::Size<Option<f32>>,
}

fn replaced_limits(ts: &TStyle, parent: tf::Size<Option<f32>>) -> Limits {
    use taffy::{MaybeResolve, ResolveOrZero};
    let calc = crate::convert::resolve_calc;
    let to_content = |s: tf::Size<Option<f32>>| {
        if ts.box_sizing != taffy::BoxSizing::BorderBox {
            return s;
        }
        let pb = ts.padding.resolve_or_zero(parent.width, calc).sum_axes()
            + ts.border.resolve_or_zero(parent.width, calc).sum_axes();
        tf::Size {
            width: s.width.map(|v| (v - pb.width).max(0.0)),
            height: s.height.map(|v| (v - pb.height).max(0.0)),
        }
    };
    Limits {
        min: to_content(ts.min_size.maybe_resolve(parent, calc)),
        max: to_content(ts.max_size.maybe_resolve(parent, calc)),
    }
}

/// Size of a replaced element from its intrinsic size and the known dimensions
/// (aspect ratio preserved). With neither dimension known, the min/max
/// `limits` apply as in CSS 2.1, section 10.4 (the table for replaced
/// elements with an intrinsic ratio): `max-width: 100%; height: auto`
/// scales the height down with the width.
pub(crate) fn replaced_size(
    w: f32,
    h: f32,
    known: tf::Size<Option<f32>>,
    limits: Limits,
) -> tf::Size<f32> {
    match (known.width, known.height) {
        (Some(kw), Some(kh)) => tf::Size {
            width: kw,
            height: kh,
        },
        (Some(kw), None) => tf::Size {
            width: kw,
            height: if w > 0.0 { kw * h / w } else { h },
        },
        (None, Some(kh)) => tf::Size {
            width: if h > 0.0 { kh * w / h } else { w },
            height: kh,
        },
        (None, None) if w > 0.0 && h > 0.0 => {
            let (width, height) = constrain_ratio(w, h, limits);
            tf::Size { width, height }
        }
        (None, None) => tf::Size {
            width: w,
            height: h,
        },
    }
}

/// CSS 2.1, section 10.4: used size of a replaced box with intrinsic size
/// `w` x `h` (both > 0) and auto width and height under min/max limits.
fn constrain_ratio(w: f32, h: f32, l: Limits) -> (f32, f32) {
    let min_w = l.min.width.unwrap_or(0.0).max(0.0);
    let min_h = l.min.height.unwrap_or(0.0).max(0.0);
    let max_w = l.max.width.unwrap_or(f32::INFINITY).max(min_w);
    let max_h = l.max.height.unwrap_or(f32::INFINITY).max(min_h);
    match (w > max_w, w < min_w, h > max_h, h < min_h) {
        (true, _, true, _) if max_w / w <= max_h / h => (max_w, min_h.max(max_w * h / w)),
        (true, _, true, _) => (min_w.max(max_h * w / h), max_h),
        (_, true, _, true) if min_w / w <= min_h / h => (max_w.min(min_h * w / h), min_h),
        (_, true, _, true) => (min_w, max_h.min(min_w * h / w)),
        (_, true, true, _) => (min_w, max_h),
        (true, _, _, true) => (max_w, min_h),
        (true, _, _, _) => (max_w, min_h.max(max_w * h / w)),
        (_, true, _, _) => (min_w, max_h.min(min_w * h / w)),
        (_, _, true, _) => (min_w.max(max_h * w / h), max_h),
        (_, _, _, true) => (max_w.min(min_h * w / h), min_h),
        _ => (w, h),
    }
}

impl TraversePartialTree for LTree {
    type ChildIter<'a> = ChildIter<'a>;

    fn child_ids(&self, parent: tf::NodeId) -> ChildIter<'_> {
        ChildIter(self.nodes[ix(parent)].children.iter())
    }

    fn child_count(&self, parent: tf::NodeId) -> usize {
        self.nodes[ix(parent)].children.len()
    }

    fn get_child_id(&self, parent: tf::NodeId, index: usize) -> tf::NodeId {
        self.nodes[ix(parent)].children[index]
    }
}

impl TraverseTree for LTree {}

impl LayoutPartialTree for LTree {
    type CoreContainerStyle<'a> = &'a TStyle;
    type CustomIdent = String;

    fn get_core_container_style(&self, id: tf::NodeId) -> &TStyle {
        &self.nodes[ix(id)].ts
    }

    fn set_unrounded_layout(&mut self, id: tf::NodeId, layout: &Layout) {
        self.nodes[ix(id)].layout = *layout;
    }

    fn compute_child_layout(&mut self, id: tf::NodeId, inputs: LayoutInput) -> LayoutOutput {
        self.dispatch(id, inputs, None)
    }

    fn resolve_calc_value(&self, val: *const (), basis: f32) -> f32 {
        crate::convert::resolve_calc(val, basis)
    }
}

impl LayoutContainingBlock for LTree {
    type OofItemStyle<'a> = &'a TStyle;

    fn get_oof_item_style(&self, id: tf::NodeId) -> &TStyle {
        &self.nodes[ix(id)].ts
    }

    fn clear_hoisted_children(&mut self, id: tf::NodeId) {
        self.nodes[ix(id)].hoisted.clear();
    }

    fn add_hoisted_children(&mut self, id: tf::NodeId, hoisted: &[tf::NodeId]) {
        self.nodes[ix(id)].hoisted.extend_from_slice(hoisted);
    }

    fn get_detailed_layout_info(&self, id: tf::NodeId) -> &DetailedLayoutInfo<String> {
        &self.nodes[ix(id)].detailed
    }
}

impl CacheTree for LTree {
    fn cache_get(&mut self, id: tf::NodeId, input: &LayoutInput) -> Option<LayoutOutput> {
        let n = &mut self.nodes[ix(id)];
        let mut out = n.cache.get(input)?;
        // A size-only hit comes back without baselines; an inline-block
        // measured twice would then sit on the baseline with its bottom edge.
        if input.run_mode == RunMode::ComputeSize
            && let Some((size, b)) = n.measured_baselines
            && size == out.size
        {
            out.baselines = b;
        }
        Some(out)
    }

    fn cache_store(&mut self, id: tf::NodeId, input: &LayoutInput, out: LayoutOutput) {
        let n = &mut self.nodes[ix(id)];
        if input.run_mode == RunMode::ComputeSize {
            n.measured_baselines = Some((out.size, out.baselines));
        }
        n.cache.store(input, out)
    }

    fn cache_clear(&mut self, id: tf::NodeId) {
        self.nodes[ix(id)].cache.clear();
    }
}

impl LayoutBlockContainer for LTree {
    type BlockContainerStyle<'a> = &'a TStyle;
    type BlockItemStyle<'a> = &'a TStyle;

    fn get_block_container_style(&self, id: tf::NodeId) -> &TStyle {
        &self.nodes[ix(id)].ts
    }

    fn get_block_child_style(&self, id: tf::NodeId) -> &TStyle {
        &self.nodes[ix(id)].ts
    }

    fn compute_block_child_layout(
        &mut self,
        id: tf::NodeId,
        inputs: LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> LayoutOutput {
        self.dispatch(id, inputs, block_ctx)
    }
}

impl LayoutFlexboxContainer for LTree {
    type FlexboxContainerStyle<'a> = &'a TStyle;
    type FlexboxItemStyle<'a> = &'a TStyle;

    fn get_flexbox_container_style(&self, id: tf::NodeId) -> &TStyle {
        &self.nodes[ix(id)].ts
    }

    fn get_flexbox_child_style(&self, id: tf::NodeId) -> &TStyle {
        &self.nodes[ix(id)].ts
    }
}

impl LayoutGridContainer for LTree {
    type GridContainerStyle<'a> = &'a TStyle;
    type GridItemStyle<'a> = &'a TStyle;

    fn get_grid_container_style(&self, id: tf::NodeId) -> &TStyle {
        &self.nodes[ix(id)].ts
    }

    fn get_grid_child_style(&self, id: tf::NodeId) -> &TStyle {
        &self.nodes[ix(id)].ts
    }

    fn set_detailed_grid_info(&mut self, id: tf::NodeId, info: taffy::DetailedGridInfo<String>) {
        self.nodes[ix(id)].detailed = DetailedLayoutInfo::Grid(Box::new(info));
    }
}

/// `LayoutInput` for measuring or laying out a child from our own algorithms.
pub(crate) fn child_input(
    run_mode: RunMode,
    known: tf::Size<Option<f32>>,
    parent_size: tf::Size<Option<f32>>,
    avail: tf::Size<tf::AvailableSpace>,
) -> LayoutInput {
    LayoutInput {
        run_mode,
        sizing_mode: taffy::SizingMode::InherentSize,
        axis: taffy::RequestedAxis::Both,
        known_dimensions: known,
        known_dimensions_are_definite: tf::Size {
            width: true,
            height: true,
        },
        parent_size,
        available_space: avail,
        vertical_margins_are_collapsible: taffy::Line::FALSE,
    }
}
