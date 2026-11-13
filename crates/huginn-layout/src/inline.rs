//! Inline formatting contexts: line layout with Parley, atomic inlines
//! (inline-blocks, images) sized through Taffy, inline element fragments.
//!
//! An IFC is one Parley layout. Inline elements are style spans. Atomic
//! inlines are Parley inline boxes whose size is computed at layout time
//! (shrink-to-fit against the real available width). Inline padding, border
//! and margin take space as zero-height inline boxes at the start and end of
//! the span ("spacers"), so text after them moves.

use std::sync::Arc;

use huginn_dom::NodeId;
use huginn_style::{Color, ComputedStyle, LengthPercentage, LengthPercentageAuto, Visibility};
use taffy::prelude as tf;
use taffy::{
    AvailableSpace, BlockContext, Layout, LayoutInput, LayoutOutput, LayoutPartialTree,
    MaybeResolve, ResolveOrZero, RunMode,
};

use crate::tree::{LTree, child_input};
use crate::{
    DecorationLine, DecorationShape, InlineFragment, LayoutTree, PositionedGlyph, Rect, TextRun,
};

/// Parley brush: text color plus the index of the innermost span.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub(crate) struct Brush {
    pub color: Color,
    pub span: u32,
}

/// An inline element (or the root of the IFC at index 0).
pub(crate) struct Span {
    pub node: Option<NodeId>,
    pub style: Arc<ComputedStyle>,
    /// Parent span index (the root is its own parent).
    pub parent: u32,
    /// Glyph baseline shift (px, downwards) from [`crate::fonts::Fonts::line_height`].
    pub baseline_shift: f32,
    /// A span without text in a standards-mode document: its box (over,
    /// under its baseline, px), which the atomic inlines in it carry.
    pub strut: Option<(f32, f32)>,
    /// Product of `opacity` of this span and its ancestor spans (the IFC
    /// root's own opacity is its box's).
    pub opacity: f32,
}

/// What a Parley inline box (by id) stands for.
pub(crate) enum Item {
    /// Atomic inline: box child of the IFC node. `strut`: the (over, under)
    /// extent of the text-less spans around it that share its baseline. The
    /// Parley box grows to cover it, so the line box makes room for them.
    Atomic {
        child: tf::NodeId,
        span: u32,
        strut: Option<(f32, f32)>,
    },
    /// Start edge of a span: margin + border + padding left.
    SpanStart(u32),
    /// End edge of a span: padding + border + margin right.
    SpanEnd(u32),
    /// Absolutely positioned or fixed box child of the IFC node; the Parley
    /// box (out of flow, zero size) marks its static position. `block_level`:
    /// its display was block-level before blockification.
    Oof {
        child: tf::NodeId,
        block_level: bool,
    },
}

pub(crate) struct Ifc {
    pub layout: parley::Layout<Brush>,
    pub spans: Vec<Span>,
    pub items: Vec<Item>,
    /// Content-box width of the last full layout.
    pub final_width: f32,
    /// Cached content widths (valid while `items` is empty).
    pub widths: Option<parley::ContentWidths>,
    /// Line breaking passes so far, and the pass of the final layout.
    pub breaks: u32,
    pub final_breaks: u32,
    /// `text-wrap: balance` in the final layout: the narrower width the
    /// lines were broken at and their x offset (see [`balance`]).
    pub final_balance: Option<(f32, f32)>,
    /// The laid-out text (whitespace collapsed), kept only for the text index.
    pub text: Option<String>,
    /// Set when the shaped layout goes back to the shape cache after the pass.
    pub shape: Option<crate::shape::Ticket>,
}

fn resolve_lpa_zero(v: LengthPercentageAuto, basis: Option<f32>) -> f32 {
    v.resolve(basis.or(Some(0.0))).unwrap_or(0.0)
}

fn resolve_lp(v: LengthPercentage, basis: Option<f32>) -> f32 {
    v.resolve(basis.or(Some(0.0)))
}

/// Inline-start and inline-end space of a span (margin + border + padding).
fn span_edges(s: &ComputedStyle, cb: Option<f32>) -> (f32, f32) {
    let start =
        resolve_lpa_zero(s.margin.left, cb) + s.border_width.left + resolve_lp(s.padding.left, cb);
    let end = resolve_lpa_zero(s.margin.right, cb)
        + s.border_width.right
        + resolve_lp(s.padding.right, cb);
    (start, end)
}

/// Does this span need start/end spacers?
pub(crate) fn span_has_edges(s: &ComputedStyle) -> (bool, bool) {
    let nz = |m: LengthPercentageAuto, b: f32, p: LengthPercentage| {
        !matches!(
            m,
            LengthPercentageAuto::Auto | LengthPercentageAuto::Px(0.0)
        ) || b > 0.0
            || !matches!(p, LengthPercentage::Px(0.0))
    };
    (
        nz(s.margin.left, s.border_width.left, s.padding.left),
        nz(s.margin.right, s.border_width.right, s.padding.right),
    )
}

pub(crate) fn vertical_align(v: huginn_style::VerticalAlign) -> parley::VerticalAlign {
    use huginn_style::VerticalAlign as V;
    match v {
        V::Baseline => parley::VerticalAlign::BASELINE,
        V::Sub => parley::VerticalAlign::SUB,
        V::Super => parley::VerticalAlign::SUPER,
        V::Top => parley::VerticalAlign::TOP,
        V::TextTop => parley::VerticalAlign::TEXT_TOP,
        V::Middle => parley::VerticalAlign::MIDDLE,
        V::Bottom => parley::VerticalAlign::BOTTOM,
        V::TextBottom => parley::VerticalAlign::TEXT_BOTTOM,
    }
}

/// Resolved margins of a child from its Taffy style.
fn child_margins(tree: &LTree, child: tf::NodeId, cb: Option<f32>) -> taffy::Rect<f32> {
    let m = tree.node(child).ts.margin;
    let r = |v: tf::LengthPercentageAuto| {
        v.resolve_to_option(cb.unwrap_or(0.0), crate::convert::resolve_calc)
            .unwrap_or(0.0)
    };
    taffy::Rect {
        left: r(m.left),
        right: r(m.right),
        top: r(m.top),
        bottom: r(m.bottom),
    }
}

/// Border and padding of a child, resolved like Taffy does.
pub(crate) fn child_border_padding(
    tree: &LTree,
    child: tf::NodeId,
    cb: Option<f32>,
) -> (taffy::Rect<f32>, taffy::Rect<f32>) {
    let ts = &tree.node(child).ts;
    let r = |v: tf::LengthPercentage| v.resolve_or_zero(cb, crate::convert::resolve_calc);
    let b = ts.border;
    let p = ts.padding;
    (
        taffy::Rect {
            left: r(b.left),
            right: r(b.right),
            top: r(b.top),
            bottom: r(b.bottom),
        },
        taffy::Rect {
            left: r(p.left),
            right: r(p.right),
            top: r(p.top),
            bottom: r(p.bottom),
        },
    )
}

/// Size of an atomic inline: border box, margins, baseline from the top of
/// the margin box. `mode` is the IFC's available width.
fn atomic_size(
    tree: &mut LTree,
    child: tf::NodeId,
    cb: Option<f32>,
    mode: AvailableSpace,
) -> (tf::Size<f32>, taffy::Rect<f32>, Option<f32>) {
    let margin = child_margins(tree, child, cb);
    let parent = tf::Size {
        width: cb,
        height: None,
    };
    let (auto_width, clips, container) = {
        let n = tree.node(child);
        let replaced = matches!(n.kind, crate::tree::Kind::Replaced { .. });
        (
            n.ts.size.width.is_auto() && !replaced,
            n.ts.overflow.x != taffy::Overflow::Visible
                || n.ts.overflow.y != taffy::Overflow::Visible,
            matches!(n.kind, crate::tree::Kind::Container) && !n.children.is_empty(),
        )
    };
    let avail_w = match mode {
        AvailableSpace::Definite(a) => {
            AvailableSpace::Definite((a - margin.left - margin.right).max(0.0))
        }
        other => other,
    };
    let known_w = if auto_width {
        // Shrink-to-fit: min(max-content, max(min-content, available)).
        let measure = |tree: &mut LTree, w: AvailableSpace| {
            tree.compute_child_layout(
                child,
                child_input(
                    RunMode::ComputeSize,
                    tf::Size::NONE,
                    parent,
                    tf::Size {
                        width: w,
                        height: AvailableSpace::MaxContent,
                    },
                ),
            )
            .size
            .width
        };
        let min = measure(tree, AvailableSpace::MinContent);
        let max = measure(tree, AvailableSpace::MaxContent);
        Some(match avail_w {
            AvailableSpace::Definite(a) => max.min(a).max(min),
            AvailableSpace::MinContent => min,
            AvailableSpace::MaxContent => max,
        })
    } else {
        None
    };
    let out = tree.compute_child_layout(
        child,
        child_input(
            RunMode::ComputeSize,
            tf::Size {
                width: known_w,
                height: None,
            },
            parent,
            tf::Size {
                width: known_w.map_or(avail_w, AvailableSpace::Definite),
                height: AvailableSpace::MaxContent,
            },
        ),
    );
    let mut baselines = out.baselines;
    if !clips && container && baselines.first.is_none() && baselines.last.is_none() {
        // Taffy's flex and grid layouts (and block layout when the style fixes
        // both sizes) stop after sizing in `ComputeSize` and report no
        // baseline. An inline-flex or inline-grid takes its baseline from its
        // first item, an inline-block from its lines (CSS Flexbox 8.5, CSS 2.1
        // 10.8.1), not from its bottom edge, so lay it out at its final size.
        // The final pass of the IFC asks for the same layout: a cache hit.
        let size = out.size;
        baselines = tree
            .compute_child_layout(
                child,
                child_input(
                    RunMode::PerformLayout,
                    tf::Size {
                        width: Some(size.width),
                        height: Some(size.height),
                    },
                    parent,
                    tf::Size {
                        width: AvailableSpace::Definite(size.width),
                        height: AvailableSpace::Definite(size.height),
                    },
                ),
            )
            .baselines;
    }
    let baseline = if clips {
        None
    } else {
        baselines.last.or(baselines.first).map(|b| b + margin.top)
    };
    (out.size, margin, baseline)
}

/// Taffy entry point for an IFC node.
pub(crate) fn compute(
    tree: &mut LTree,
    id: tf::NodeId,
    inputs: LayoutInput,
    block_ctx: Option<&mut BlockContext<'_>>,
) -> LayoutOutput {
    let crate::tree::Kind::Ifc(ifc) =
        std::mem::replace(&mut tree.node_mut(id).kind, crate::tree::Kind::Container)
    else {
        unreachable!("inline::compute on a non-IFC node")
    };
    let mut ifc = ifc;
    let ts = tree.node(id).ts.clone();
    let style = tree.node(id).style.clone();
    let parent_w = inputs.parent_size.width;
    let (border, padding) = {
        let r = |v: tf::LengthPercentage| v.resolve_or_zero(parent_w, crate::convert::resolve_calc);
        (
            taffy::Rect {
                left: r(ts.border.left),
                right: r(ts.border.right),
                top: r(ts.border.top),
                bottom: r(ts.border.bottom),
            },
            taffy::Rect {
                left: r(ts.padding.left),
                right: r(ts.padding.right),
                top: r(ts.padding.top),
                bottom: r(ts.padding.bottom),
            },
        )
    };
    let inset_left = border.left + padding.left;
    let inset_top = border.top + padding.top;
    let run_mode = inputs.run_mode;
    // The final pass breaks lines at the available width only when that width
    // is the box's own: the parent fixed it (block flow, stretched grid and
    // flex items, atomic inlines) or the style sets it. A float, an absolutely
    // positioned box or a grid item with auto margins is laid out without a
    // width and shrinks to fit, as in a measuring pass.
    let fixed_width = inputs.known_dimensions.width.is_some()
        || ts
            .size
            .width
            .maybe_resolve(parent_w, crate::convert::resolve_calc)
            .is_some();
    let mut child_outputs: Vec<(tf::NodeId, taffy::OofCandidates)> = Vec::new();
    // Floats of the enclosing block formatting context: lines next to a
    // float are shortened and moved beside it.
    let mut block_ctx = block_ctx;
    let inset_right = border.right + padding.right;
    if let Some(ctx) = block_ctx.as_deref_mut() {
        ctx.apply_content_box_inset([inset_left, inset_right]);
    }
    let floats = block_ctx
        .as_deref()
        .is_some_and(|ctx| ctx.has_active_floats(inset_top));

    let mut out =
        taffy::compute_leaf_layout(inputs, &ts, crate::convert::resolve_calc, |known, avail| {
            // `avail` is the content box. A definite width is final when the
            // parent fixed our width (or in the final layout pass); otherwise we
            // shrink to fit into it.
            let exact = match avail.width {
                AvailableSpace::Definite(a)
                    if known.width.is_some()
                        || (run_mode == RunMode::PerformLayout && fixed_width) =>
                {
                    Some(a)
                }
                _ => None,
            };
            let cb = match avail.width {
                AvailableSpace::Definite(a) => Some(a),
                _ => None,
            };
            let mode = avail.width;
            // 1. Size atomic inlines and spacers for this width.
            // (border box size, margins, offset of the margin box in the Parley box)
            let mut sizes: Vec<(tf::Size<f32>, taffy::Rect<f32>, f32)> =
                Vec::with_capacity(ifc.items.len());
            let mut boxes: Vec<(f32, f32, Option<f32>)> = Vec::with_capacity(ifc.items.len());
            for item in &ifc.items {
                let (w, h, base, size, margin, dy) = match item {
                    Item::Atomic { child, strut, .. } => {
                        let (size, margin, base) = atomic_size(tree, *child, cb, mode);
                        let w = size.width + margin.left + margin.right;
                        let h = size.height + margin.top + margin.bottom;
                        match strut {
                            Some((over, under)) => {
                                let ascent = base.unwrap_or(h);
                                let top = ascent.max(*over);
                                let bottom = (h - ascent).max(*under);
                                (w, top + bottom, Some(top), size, margin, top - ascent)
                            }
                            None => (w, h, base, size, margin, 0.0),
                        }
                    }
                    Item::SpanStart(s) => {
                        let w = span_edges(&ifc.spans[*s as usize].style, cb).0;
                        (w, 0.0, None, tf::Size::ZERO, taffy::Rect::zero(), 0.0)
                    }
                    Item::SpanEnd(s) => {
                        let w = span_edges(&ifc.spans[*s as usize].style, cb).1;
                        (w, 0.0, None, tf::Size::ZERO, taffy::Rect::zero(), 0.0)
                    }
                    Item::Oof { .. } => (0.0, 0.0, None, tf::Size::ZERO, taffy::Rect::zero(), 0.0),
                };
                boxes.push((w, h, base));
                sizes.push((size, margin, dy));
            }
            for b in ifc.layout.inline_boxes_mut() {
                if let Some(&(w, h, base)) = boxes.get(b.id as usize) {
                    b.width = w;
                    b.height = h;
                    b.baseline = base;
                }
            }
            // 2. Width to break lines at.
            let width = match exact {
                Some(w) => w,
                None => {
                    let cw = if ifc.items.is_empty() {
                        *ifc.widths
                            .get_or_insert_with(|| ifc.layout.calculate_content_widths())
                    } else {
                        ifc.layout.calculate_content_widths()
                    };
                    match avail.width {
                        AvailableSpace::MinContent => cw.min,
                        AvailableSpace::MaxContent => cw.max,
                        AvailableSpace::Definite(a) => a.min(cw.max).max(cw.min),
                    }
                }
            };
            let indent = resolve_lp(style.text_indent, Some(width));
            if indent != 0.0 {
                ifc.layout.set_text_indent(indent, Default::default());
            }
            if floats && exact.is_some() {
                let ctx = block_ctx.as_deref().expect("floats imply a block context");
                break_around_floats(&mut ifc.layout, ctx, width, inset_left, inset_top);
            } else {
                ifc.layout.break_all_lines(Some(width + 0.01));
            }
            ifc.breaks += 1;
            if run_mode == RunMode::PerformLayout {
                let align = crate::convert::text_align(style.text_align);
                // Balancing keeps the line count, so the height measured above
                // stays; only the final layout needs it.
                ifc.final_balance = None;
                if style.text_wrap_balance && !floats && exact.is_some() && wraps(style.white_space)
                {
                    ifc.final_balance = balance(&mut ifc.layout, width, align);
                    ifc.breaks += 1;
                }
                ifc.layout.align(align, parley::AlignmentOptions::default());
                ifc.final_width = width;
                ifc.final_breaks = ifc.breaks;
                // 3. Lay out and place atomic inlines.
                for line in ifc.layout.lines() {
                    let m = line.metrics();
                    // Anything in flow before the current item on this line.
                    let mut in_flow_before = false;
                    for it in line.items() {
                        let pb = match it {
                            parley::PositionedLayoutItem::InlineBox(pb) => pb,
                            parley::PositionedLayoutItem::GlyphRun(_) => {
                                in_flow_before = true;
                                continue;
                            }
                        };
                        let i = pb.id as usize;
                        let child = match ifc.items[i] {
                            Item::Atomic { child, .. } => child,
                            Item::Oof { child, block_level } => {
                                // Static position (CSS 2.1, section 10.3.7; Blink's
                                // NGInlineLayoutAlgorithm): an originally inline box
                                // sits where it is in the line, a block-level one at
                                // the start of the next line (of this one while it
                                // is still empty).
                                let (sx, sy) = if !block_level {
                                    (pb.x, m.block_min_coord)
                                } else if in_flow_before {
                                    (0.0, m.block_max_coord)
                                } else {
                                    (0.0, m.block_min_coord)
                                };
                                let position = match tree.node(child).ts.position {
                                    taffy::Position::Fixed => taffy::Position::Fixed,
                                    _ => taffy::Position::Absolute,
                                };
                                let order =
                                    tree.node(id)
                                        .children
                                        .iter()
                                        .position(|&c| c == child)
                                        .unwrap_or(0) as u32;
                                let mut oof = taffy::OofCandidates::new();
                                oof.push(taffy::OofCandidate {
                                    node: child,
                                    order,
                                    position,
                                    static_position: taffy::Point {
                                        x: taffy::AxisStaticPosition::from_edge(
                                            inset_left + sx,
                                            taffy::AxisStaticEdge::Start,
                                        ),
                                        y: taffy::AxisStaticPosition::from_edge(
                                            inset_top + sy,
                                            taffy::AxisStaticEdge::Start,
                                        ),
                                    },
                                });
                                child_outputs.push((child, oof));
                                continue;
                            }
                            _ => {
                                in_flow_before = true;
                                continue;
                            }
                        };
                        in_flow_before = true;
                        let (size, margin, dy) = sizes[i];
                        let co = tree.compute_child_layout(
                            child,
                            child_input(
                                RunMode::PerformLayout,
                                tf::Size {
                                    width: Some(size.width),
                                    height: Some(size.height),
                                },
                                tf::Size {
                                    width: cb,
                                    height: None,
                                },
                                tf::Size {
                                    width: AvailableSpace::Definite(size.width),
                                    height: AvailableSpace::Definite(size.height),
                                },
                            ),
                        );
                        let (b, p) = child_border_padding(tree, child, cb);
                        let mut location = taffy::Point {
                            x: inset_left + pb.x + margin.left,
                            y: inset_top + pb.y + dy + margin.top,
                        };
                        let cs = &tree.node(child).style;
                        if cs.position == huginn_style::Position::Relative {
                            location.x += cs
                                .inset
                                .left
                                .resolve(cb)
                                .or(cs.inset.right.resolve(cb).map(|r| -r))
                                .unwrap_or(0.0);
                            location.y += cs
                                .inset
                                .top
                                .resolve(None)
                                .or(cs.inset.bottom.resolve(None).map(|b| -b))
                                .unwrap_or(0.0);
                        }
                        tree.set_unrounded_layout(
                            child,
                            &Layout {
                                order: i as u32,
                                location,
                                size,
                                scrollable_overflow_rect: co.scrollable_overflow_rect,
                                scrollbar_size: tf::Size::ZERO,
                                border: b,
                                padding: p,
                                margin,
                            },
                        );
                        let mut oof = co.oof_candidates;
                        if !oof.is_empty() {
                            oof.translate(location);
                            child_outputs.push((child, oof));
                        }
                    }
                }
            }
            tf::Size {
                width,
                height: ifc.layout.height(),
            }
        });
    let first = ifc.layout.lines().next().map(|l| l.metrics().baseline);
    let last = ifc.layout.lines().last().map(|l| l.metrics().baseline);
    out.baselines = taffy::Baselines {
        first: first.map(|b| b + inset_top),
        last: last.map(|b| b + inset_top),
    };
    if run_mode == RunMode::PerformLayout {
        for (_, mut c) in child_outputs {
            out.oof_candidates.append(&mut c);
        }
        out.oof_positioning_area = Some(taffy::OofPositioningArea {
            size: tf::Size {
                width: out.size.width - border.left - border.right,
                height: out.size.height - border.top - border.bottom,
            },
            offset: taffy::Point {
                x: border.left,
                y: border.top,
            },
        });
    }
    tree.node_mut(id).kind = crate::tree::Kind::Ifc(ifc);
    out
}

/// Emit the text runs, decorations and inline fragments of an IFC whose
/// content box starts at (`ox`, `oy`).
pub(crate) fn emit(ifc: &mut Ifc, out: &mut LayoutTree, ox: f32, oy: f32) {
    if ifc.breaks != ifc.final_breaks {
        // A later size query re-broke the lines; restore the final layout.
        match ifc.final_balance {
            Some((w, x)) => break_balanced(&mut ifc.layout, ifc.final_width, w, x),
            None => ifc.layout.break_all_lines(Some(ifc.final_width + 0.01)),
        }
        ifc.layout.align(
            crate::convert::text_align(ifc.spans[0].style.text_align),
            parley::AlignmentOptions::default(),
        );
    }
    let nspans = ifc.spans.len();
    // Per span: per line extents (line index, x0, x1, top, bottom, holds
    // content: text or an atomic inline, not only edge spacers).
    type Ext = (u32, f32, f32, f32, f32, bool);
    let mut ext: Vec<Vec<Ext>> = (0..nspans).map(|_| Vec::new()).collect();
    fn touch(ext: &mut [Vec<Ext>], s: usize, line: u32, x0: f32, x1: f32, top: f32, bottom: f32) {
        let v = &mut ext[s];
        match v.last_mut() {
            Some(e) if e.0 == line => {
                e.1 = e.1.min(x0);
                e.2 = e.2.max(x1);
                e.3 = e.3.min(top);
                e.4 = e.4.max(bottom);
                e.5 = true;
            }
            _ => v.push((line, x0, x1, top, bottom, true)),
        }
    }
    // Text index: where the last indexed run of this IFC ended in its text.
    let mut indexed: Option<usize> = None;
    for (li, line) in ifc.layout.lines().enumerate() {
        let li = li as u32;
        let lm = line.metrics();
        let (line_top, line_bottom) = (oy + lm.block_min_coord, oy + lm.block_max_coord);
        // (shaped run index, glyph runs of it seen so far on this line)
        let mut shaped: Option<(usize, u32)> = None;
        for item in line.items() {
            match item {
                parley::PositionedLayoutItem::GlyphRun(gr) => {
                    let span = gr.style().brush.span as usize;
                    let hidden = ifc.spans[span].style.visibility != Visibility::Visible;
                    let piece = match shaped {
                        Some((i, k)) if i == gr.run().index() => k + 1,
                        _ => 0,
                    };
                    shaped = Some((gr.run().index(), piece));
                    if !hidden {
                        push_glyph_run(
                            out,
                            &gr,
                            ox,
                            oy + ifc.spans[span].baseline_shift,
                            ifc.spans[span].node,
                        );
                        if let Some(text) = &ifc.text {
                            index_run(
                                out,
                                &gr,
                                ox,
                                piece,
                                text,
                                &mut indexed,
                                line_top,
                                line_bottom,
                            );
                        }
                    }
                    let m = gr.run().font_metrics();
                    let base = gr.baseline();
                    let x0 = gr.offset();
                    let (top, bottom) = (base - m.ascent, base + m.descent);
                    // The run belongs to its span and every ancestor span.
                    let mut s = span;
                    loop {
                        touch(&mut ext, s, li, x0, x0 + gr.advance(), top, bottom);
                        let p = ifc.spans[s].parent as usize;
                        if p == s {
                            break;
                        }
                        s = p;
                    }
                }
                parley::PositionedLayoutItem::InlineBox(pb) => {
                    let (span, x0, x1, own) = match ifc.items[pb.id as usize] {
                        Item::SpanStart(s) => {
                            let m = resolve_lpa_zero(ifc.spans[s as usize].style.margin.left, None);
                            (s as usize, pb.x + m, pb.x + pb.width, true)
                        }
                        Item::SpanEnd(s) => {
                            let m =
                                resolve_lpa_zero(ifc.spans[s as usize].style.margin.right, None);
                            (s as usize, pb.x, pb.x + pb.width - m, true)
                        }
                        // An atomic inline widens the spans around it.
                        Item::Atomic { span, .. } => (span as usize, pb.x, pb.x + pb.width, false),
                        // Out of flow: not part of any fragment.
                        Item::Oof { .. } => continue,
                    };
                    let content = !own;
                    let mut s = span;
                    let mut first = own;
                    loop {
                        let (a, b) = if first {
                            (x0, x1)
                        } else {
                            (pb.x, pb.x + pb.width)
                        };
                        // Spacers carry no vertical extent of their own.
                        let v = &mut ext[s];
                        match v.last_mut() {
                            Some(e) if e.0 == li => {
                                e.1 = e.1.min(a);
                                e.2 = e.2.max(b);
                                e.5 |= content;
                            }
                            _ => v.push((li, a, b, f32::INFINITY, f32::NEG_INFINITY, content)),
                        }
                        first = false;
                        let p = ifc.spans[s].parent as usize;
                        if p == s {
                            break;
                        }
                        s = p;
                    }
                }
            }
        }
    }
    // Fragments, in tree order of spans, then line order. Span 0 is the IFC root.
    let lines: Vec<parley::LineMetrics> = ifc.layout.lines().map(|l| *l.metrics()).collect();
    for (si, e) in ext.iter_mut().enumerate().skip(1) {
        let span = &ifc.spans[si];
        let Some(node) = span.node else { continue };
        // Parley may break a line between a span's start spacer and its
        // text, or before its end spacer. A piece holding only an edge then
        // sits on its own line; Chrome keeps edges with the content, so
        // drop such pieces (unless the span has no content at all).
        if e.iter().any(|p| p.5) {
            e.retain(|p| p.5);
        }
        let s = &span.style;
        let n = e.len();
        for (k, &(li, x0, x1, top, bottom, _)) in e.iter().enumerate() {
            let lm = &lines[li as usize];
            let (top, bottom) = if top.is_finite() {
                (top, bottom)
            } else {
                (
                    lm.baseline - lm.line_height * 0.4,
                    lm.baseline + lm.line_height * 0.1,
                )
            };
            let pt = resolve_lp(s.padding.top, None) + s.border_width.top;
            let pb = resolve_lp(s.padding.bottom, None) + s.border_width.bottom;
            out.fragments.push(InlineFragment {
                node,
                style: s.clone(),
                rect: Rect {
                    x: ox + x0,
                    y: oy + top - pt,
                    width: (x1 - x0).max(0.0),
                    height: (bottom - top) + pt + pb,
                },
                first: k == 0,
                last: k + 1 == n,
                opacity: span.opacity,
            });
        }
    }
    // Text decorations of the root span are already pushed with the runs.
}

/// Break lines one at a time, each in the space the floats leave free at
/// its height. `width` is the content width; coordinates are relative to
/// the IFC's content box, whose top-left is at (`inset_left`, `inset_top`)
/// in the border box.
fn break_around_floats(
    layout: &mut parley::Layout<Brush>,
    ctx: &BlockContext<'_>,
    width: f32,
    inset_left: f32,
    inset_top: f32,
) {
    let mut br = layout.break_lines();
    br.state_mut().set_layout_max_advance(width + 0.01);
    loop {
        let y = br.committed_y() as f32;
        let slot = ctx.find_content_slot(inset_top + y, taffy::Clear::None, None);
        let x = (slot.x - inset_left).clamp(0.0, width);
        let w = slot.width.min(width - x).max(0.0);
        br.state_mut().set_line_x(x);
        br.state_mut().set_line_max_advance(w + 0.01);
        if br.break_next().is_none() {
            break;
        }
    }
    br.finish();
}

/// Does this `white-space` value allow soft wraps?
fn wraps(ws: huginn_style::WhiteSpace) -> bool {
    !matches!(
        ws,
        huginn_style::WhiteSpace::Nowrap | huginn_style::WhiteSpace::Pre
    )
}

/// Blink balances paragraphs of at most this many lines
/// (`kMaxLinesForBalance` in score_line_break_context.h).
const MAX_LINES_FOR_BALANCE: usize = 6;

/// `text-wrap: balance`, as Blink's bisecting balancer
/// (`ParagraphLineBreaker::AttemptParagraphBalancing`): break the lines at
/// the narrowest width, to a CSS pixel, that keeps the line count of the
/// greedy layout at `width`. Lines still align in the full `width`: they
/// start at an x offset that puts the narrower box where the alignment would.
/// Returns that (width, offset), or `None` when the greedy layout stays.
/// `layout` must hold the greedy lines at `width` on entry; on return it
/// holds the final lines.
fn balance(
    layout: &mut parley::Layout<Brush>,
    width: f32,
    align: parley::Alignment,
) -> Option<(f32, f32)> {
    let n = layout.len();
    if !(2..=MAX_LINES_FOR_BALANCE).contains(&n) {
        return None;
    }
    // No narrower width fits the content of the lines in `n` lines.
    let total: f32 = layout
        .lines()
        .map(|l| l.metrics().advance - l.metrics().hanging_advance)
        .sum();
    let (mut lo, mut hi) = ((total / n as f32).floor() - 1.0, width);
    while hi - lo > 1.0 {
        let mid = ((lo + hi) / 2.0).floor();
        layout.break_all_lines(Some(mid + 0.01));
        if layout.len() <= n {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    if hi >= width {
        layout.break_all_lines(Some(width + 0.01));
        return None;
    }
    let x = match align {
        parley::Alignment::Center => (width - hi) / 2.0,
        parley::Alignment::Right | parley::Alignment::End => width - hi,
        _ => 0.0,
    };
    break_balanced(layout, width, hi, x);
    Some((hi, x))
}

/// Break every line at `w`, starting at `x`, in a layout `full` wide.
fn break_balanced(layout: &mut parley::Layout<Brush>, full: f32, w: f32, x: f32) {
    let mut br = layout.break_lines();
    br.state_mut().set_layout_max_advance(full + 0.01);
    loop {
        br.state_mut().set_line_x(x);
        br.state_mut().set_line_max_advance(w + 0.01);
        if br.break_next().is_none() {
            break;
        }
    }
    br.finish();
}

pub(crate) fn push_glyph_run(
    out: &mut LayoutTree,
    gr: &parley::GlyphRun<'_, Brush>,
    ox: f32,
    oy: f32,
    node: Option<NodeId>,
) {
    let run = gr.run();
    let style = gr.style();
    let g0 = out.glyphs.len() as u32;
    out.glyphs
        .extend(gr.positioned_glyphs().map(|g| PositionedGlyph {
            id: g.id,
            x: ox + g.x,
            y: oy + g.y,
        }));
    let c0 = out.coords.len() as u32;
    out.coords
        .extend(run.normalized_coords().iter().map(|c| c.to_bits()));
    let synth = run.synthesis();
    let baseline = oy + gr.baseline();
    out.runs.push(TextRun {
        font: run.font().clone(),
        font_size: run.font_size(),
        color: style.brush.color,
        glyphs: g0..out.glyphs.len() as u32,
        coords: c0..out.coords.len() as u32,
        skew: synth.skew(),
        embolden: synth.embolden(),
        baseline,
        node,
    });
    if let Some(index) = out.text_index.as_mut() {
        // No text yet: `index_run` fills it for IFC text; list markers stay empty.
        let (t, k) = (index.text.len() as u32, index.clusters.len() as u32);
        index.runs.push(crate::RunText {
            text: t..t,
            clusters: k..k,
            top: baseline,
            bottom: baseline,
        });
    }
    let m = run.font_metrics();
    let x = ox + gr.offset();
    let w = gr.advance();
    if let Some(u) = &style.underline {
        let offset = u.offset.unwrap_or(m.underline_offset);
        let size = u.size.unwrap_or(m.underline_size).max(1.0);
        out.decorations.push(DecorationLine {
            rect: Rect {
                x,
                y: baseline - offset,
                width: w,
                height: size,
            },
            color: u.brush.color,
            shape: DecorationShape::Rect,
        });
    }
    if let Some(s) = &style.strikethrough {
        let offset = s.offset.unwrap_or(m.strikethrough_offset);
        let size = s.size.unwrap_or(m.strikethrough_size).max(1.0);
        out.decorations.push(DecorationLine {
            rect: Rect {
                x,
                y: baseline - offset - size / 2.0,
                width: w,
                height: size,
            },
            color: s.brush.color,
            shape: DecorationShape::Rect,
        });
    }
}

/// Append the text and clusters of `gr` (a run of an IFC whose text is
/// `text`) to the text index. `indexed`: end of the IFC text indexed so far.
/// `piece`: which glyph run of its shaped run on this line `gr` is (a shaped
/// run splits into glyph runs, in visual order, where the style changes).
/// Clusters are matched by that order, not by x: x positions far along a
/// very long line are not exact in f32.
#[allow(clippy::too_many_arguments)]
fn index_run(
    out: &mut LayoutTree,
    gr: &parley::GlyphRun<'_, Brush>,
    ox: f32,
    piece: u32,
    text: &str,
    indexed: &mut Option<usize>,
    top: f32,
    bottom: f32,
) {
    let Some(index) = out.text_index.as_mut() else {
        return;
    };
    let run = gr.run();
    let mut x = ox + gr.offset();
    let k0 = index.clusters.len();
    let (mut lo, mut hi) = (usize::MAX, 0);
    let (mut group, mut prev_style) = (0u32, None);
    for c in run.visual_clusters() {
        // As parley splits glyph runs: at a style change between clusters
        // that have glyphs (glyph-less clusters do not split).
        if c.glyphs().next().is_some() {
            let style = c.style_index();
            if prev_style.is_some_and(|p| p != style) {
                group += 1;
            }
            prev_style = Some(style);
        }
        if group != piece {
            continue;
        }
        let (cx, w) = (x, c.advance());
        x += w;
        let r = c.text_range();
        lo = lo.min(r.start);
        hi = hi.max(r.end);
        index.clusters.push(crate::TextCluster {
            text: r.start as u32,
            x: cx,
            width: w,
        });
    }
    // `push_glyph_run` pushed this run's entry without text.
    let Some(entry) = index.runs.last_mut() else {
        return;
    };
    entry.top = top;
    entry.bottom = bottom;
    if lo >= hi || hi > text.len() {
        index.clusters.truncate(k0);
        return;
    }
    // Separator: a new block starts on a new line. A gap inside the IFC (text
    // no run shows: `<br>`, a collapsed space at a line break) keeps its line
    // feeds, else becomes one ' ' if it holds whitespace.
    let ends_nl = index.text.is_empty() || index.text.ends_with('\n');
    match *indexed {
        None if !ends_nl => index.text.push('\n'),
        Some(end) if lo > end => {
            let g = text.get(end..lo).unwrap_or("");
            let feeds = g.matches('\n').count();
            if feeds > 0 {
                index.text.extend(std::iter::repeat_n('\n', feeds));
            } else if !ends_nl && !index.text.ends_with(' ') && g.contains(char::is_whitespace) {
                index.text.push(' ');
            }
        }
        _ => {}
    }
    *indexed = Some(hi);
    let base = index.text.len() as i64 - lo as i64;
    index.text.push_str(&text[lo..hi]);
    let end = index.text.len() as u32;
    let clusters = &mut index.clusters[k0..];
    clusters.sort_by_key(|c| c.text);
    for c in clusters.iter_mut() {
        c.text = (c.text as i64 + base) as u32;
    }
    let k1 = index.clusters.len() as u32;
    let entry = index.runs.last_mut().expect("entry exists");
    entry.text = (lo as i64 + base) as u32..end;
    entry.clusters = k0 as u32..k1;
}
