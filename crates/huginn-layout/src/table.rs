//! CSS 2.1 automatic table layout (our code, run from Taffy's dispatch).
//!
//! - Column widths come from the min-content and max-content widths of the
//!   cells (colspan cells spread their excess over the spanned columns),
//!   fixed (`px`) and percentage cell widths, and `<col>` widths.
//! - The table is as wide as its `width`, or shrinks to fit (max-content,
//!   clamped to the available width). Extra width goes to auto columns in
//!   proportion to their max-content width.
//! - Rows are as tall as their tallest cell; rowspan cells grow the last
//!   spanned row. Cells stretch to the row height; their content is moved
//!   down for `vertical-align: middle/bottom`.
//! - `border-spacing` separates cells. `border-collapse: collapse` is
//!   approximated at build time (spacing 0, shared borders drawn once).
//! - Captions go above the rows (`caption-side: top`) or below them
//!   (`bottom`), inside the table's border box.

use huginn_style::{CaptionSide, LengthPercentageAuto, VerticalAlign};
use taffy::prelude as tf;
use taffy::{
    AvailableSpace, Layout, LayoutInput, LayoutOutput, LayoutPartialTree, ResolveOrZero, RunMode,
    SizingMode,
};

use crate::inline::child_border_padding;
use crate::tree::{Kind, LTree, child_input};

pub(crate) struct Cell {
    pub node: tf::NodeId,
    pub row: u32,
    pub col: u32,
    pub rowspan: u32,
    pub colspan: u32,
}

pub(crate) struct Row {
    pub node: tf::NodeId,
    /// Index into `Table::groups`.
    pub group: u32,
}

#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct Col {
    pub min: f32,
    pub max: f32,
    /// Fixed width (border box) from a cell or `<col>` `width` in px.
    pub fixed: Option<f32>,
    /// Percentage width (0..=100).
    pub pct: Option<f32>,
}

pub(crate) struct Table {
    pub cols: usize,
    pub rows: Vec<Row>,
    pub groups: Vec<tf::NodeId>,
    pub captions: Vec<tf::NodeId>,
    pub cells: Vec<Cell>,
    /// Widths from `<col>`/`<colgroup>`, per column.
    pub col_widths: Vec<LengthPercentageAuto>,
    /// (horizontal, vertical) spacing; 0 when borders collapse.
    pub spacing: (f32, f32),
    pub collapse: bool,
    /// Column min/max widths (they do not depend on the table width).
    pub intrinsic: Option<Vec<Col>>,
}

fn content_input(
    run_mode: RunMode,
    known: tf::Size<Option<f32>>,
    avail: tf::Size<AvailableSpace>,
) -> LayoutInput {
    let mut i = child_input(run_mode, known, tf::Size::NONE, avail);
    i.sizing_mode = SizingMode::ContentSize;
    i
}

fn cell_minmax(tree: &mut LTree, cell: tf::NodeId) -> (f32, f32) {
    let mut m = |w: AvailableSpace| {
        tree.compute_child_layout(
            cell,
            content_input(
                RunMode::ComputeSize,
                tf::Size::NONE,
                tf::Size {
                    width: w,
                    height: AvailableSpace::MaxContent,
                },
            ),
        )
        .size
        .width
    };
    let min = m(AvailableSpace::MinContent);
    let max = m(AvailableSpace::MaxContent);
    (min, max.max(min))
}

/// A cell's `width` style: (border-box width if in px, percentage if in %).
fn styled_px(tree: &LTree, cell: tf::NodeId) -> (Option<f32>, Option<f32>) {
    let s = &tree.node(cell).style;
    let pb = s.border_width.left
        + s.border_width.right
        + s.padding.left.resolve(Some(0.0))
        + s.padding.right.resolve(Some(0.0));
    let w = match s.width {
        LengthPercentageAuto::Calc(c) if c.percent == 0.0 => LengthPercentageAuto::Px(c.px),
        w => w,
    };
    match w {
        LengthPercentageAuto::Px(w) => (
            Some(if s.box_sizing == huginn_style::BoxSizing::BorderBox {
                w
            } else {
                w + pb
            }),
            None,
        ),
        LengthPercentageAuto::Percent(p) => (None, Some(p)),
        // Mixed calc() widths count as auto, as in Chrome's table layout.
        LengthPercentageAuto::Auto | LengthPercentageAuto::Calc(_) => (None, None),
    }
}

fn intrinsic(tree: &mut LTree, t: &Table) -> Vec<Col> {
    let mut cols = vec![Col::default(); t.cols];
    for (j, w) in t.col_widths.iter().enumerate().take(t.cols) {
        match *w {
            LengthPercentageAuto::Px(px) => {
                cols[j].fixed = Some(px);
                cols[j].min = cols[j].min.max(px);
                cols[j].max = cols[j].max.max(px);
            }
            LengthPercentageAuto::Percent(p) => cols[j].pct = Some(p),
            LengthPercentageAuto::Auto | LengthPercentageAuto::Calc(_) => {}
        }
    }
    let hs = t.spacing.0;
    let mut spanning: Vec<(usize, f32, f32, Option<f32>)> = Vec::new();
    for (k, c) in t.cells.iter().enumerate() {
        let (min, max) = cell_minmax(tree, c.node);
        let (fixed, pct) = styled_px(tree, c.node);
        let max = fixed.map_or(max, |f| f.max(min));
        if c.colspan == 1 {
            let col = &mut cols[c.col as usize];
            col.min = col.min.max(min);
            col.max = col.max.max(max);
            if let Some(f) = fixed {
                col.fixed = Some(col.fixed.map_or(f, |x| x.max(f)));
            }
            if let Some(p) = pct {
                col.pct = Some(col.pct.map_or(p, |x| x.max(p)));
            }
        } else {
            spanning.push((k, min, max, fixed));
        }
    }
    // Spanning cells: spread what the spanned columns lack.
    spanning.sort_by_key(|&(k, ..)| t.cells[k].colspan);
    for (k, min, max, _) in spanning {
        let c = &t.cells[k];
        let range = c.col as usize..(c.col + c.colspan).min(t.cols as u32) as usize;
        let gaps = hs * (range.len().saturating_sub(1)) as f32;
        let span_min: f32 = cols[range.clone()].iter().map(|c| c.min).sum::<f32>() + gaps;
        let span_max: f32 = cols[range.clone()].iter().map(|c| c.max).sum::<f32>() + gaps;
        let n = range.len() as f32;
        if min > span_min {
            let excess = min - span_min;
            let total_max: f32 = cols[range.clone()].iter().map(|c| c.max).sum();
            for col in &mut cols[range.clone()] {
                let share = if total_max > 0.0 {
                    col.max / total_max
                } else {
                    1.0 / n
                };
                col.min += excess * share;
                col.max = col.max.max(col.min);
            }
        }
        if max > span_max {
            let excess = max - span_max.max(min);
            let total_max: f32 = cols[range.clone()].iter().map(|c| c.max).sum();
            for col in &mut cols[range.clone()] {
                let share = if total_max > 0.0 {
                    col.max / total_max
                } else {
                    1.0 / n
                };
                col.max += excess.max(0.0) * share;
            }
        }
    }
    cols
}

/// Column widths for a table content width `avail` (spacing excluded).
fn distribute(cols: &[Col], avail: f32) -> Vec<f32> {
    let sum_min: f32 = cols.iter().map(|c| c.min).sum();
    if avail <= sum_min {
        return cols.iter().map(|c| c.min).collect();
    }
    let mut w: Vec<f32> = cols.iter().map(|c| c.min).collect();
    let mut left = avail - sum_min;
    // 1. Percentage columns take their share first.
    for (j, c) in cols.iter().enumerate() {
        if let Some(p) = c.pct {
            let want = (avail * p / 100.0).max(c.min);
            let add = (want - w[j]).min(left).max(0.0);
            w[j] += add;
            left -= add;
        }
    }
    // 2. Fixed columns grow to their width.
    for (j, c) in cols.iter().enumerate() {
        if c.pct.is_none()
            && let Some(f) = c.fixed
        {
            let add = (f.max(c.min) - w[j]).min(left).max(0.0);
            w[j] += add;
            left -= add;
        }
    }
    // 3. Auto columns grow toward their max-content width, then beyond in
    //    proportion to it.
    let auto: Vec<usize> = (0..cols.len())
        .filter(|&j| cols[j].pct.is_none() && cols[j].fixed.is_none())
        .collect();
    let grow: f32 = auto
        .iter()
        .map(|&j| cols[j].max - w[j])
        .filter(|g| *g > 0.0)
        .sum();
    if grow > 0.0 {
        let f = (left / grow).min(1.0);
        for &j in &auto {
            let g = (cols[j].max - w[j]).max(0.0);
            w[j] += g * f;
        }
        left -= grow * f;
    }
    if left > 0.01 {
        let targets: Vec<usize> = if !auto.is_empty() {
            auto
        } else {
            (0..cols.len()).filter(|&j| cols[j].pct.is_none()).collect()
        };
        let targets = if targets.is_empty() {
            (0..cols.len()).collect()
        } else {
            targets
        };
        let total: f32 = targets.iter().map(|&j| w[j]).sum();
        for &j in &targets {
            let share = if total > 0.0 {
                w[j] / total
            } else {
                1.0 / targets.len() as f32
            };
            w[j] += left * share;
        }
    }
    w
}

pub(crate) fn compute(tree: &mut LTree, id: tf::NodeId, inputs: LayoutInput) -> LayoutOutput {
    let Kind::Table(mut t) = std::mem::replace(&mut tree.node_mut(id).kind, Kind::Container) else {
        unreachable!("table::compute on a non-table node")
    };
    let out = compute_inner(tree, id, &mut t, inputs);
    tree.node_mut(id).kind = Kind::Table(t);
    out
}

fn compute_inner(
    tree: &mut LTree,
    id: tf::NodeId,
    t: &mut Table,
    inputs: LayoutInput,
) -> LayoutOutput {
    if t.intrinsic.is_none() {
        t.intrinsic = Some(intrinsic(tree, t));
    }
    let cols = t.intrinsic.clone().unwrap_or_default();
    let ts = tree.node(id).ts.clone();
    let style = tree.node(id).style.clone();
    let parent_w = inputs.parent_size.width;
    let r = |v: tf::LengthPercentage| v.resolve_or_zero(parent_w, crate::convert::resolve_calc);
    let border = taffy::Rect {
        left: r(ts.border.left),
        right: r(ts.border.right),
        top: r(ts.border.top),
        bottom: r(ts.border.bottom),
    };
    let padding = if t.collapse {
        taffy::Rect::zero()
    } else {
        taffy::Rect {
            left: r(ts.padding.left),
            right: r(ts.padding.right),
            top: r(ts.padding.top),
            bottom: r(ts.padding.bottom),
        }
    };
    let (hs, vs) = t.spacing;
    let n = t.cols;
    let inset_x = border.left + border.right + padding.left + padding.right;
    let inset_y = border.top + border.bottom + padding.top + padding.bottom;
    let extra_x = inset_x + if n > 0 { hs * (n + 1) as f32 } else { 0.0 };
    let sum_min: f32 = cols.iter().map(|c| c.min).sum();
    let sum_max: f32 = cols.iter().map(|c| c.max).sum();
    let min_t = sum_min + extra_x;
    let max_t = sum_max.max(sum_min) + extra_x;

    // Table width (border box).
    let inherent = inputs.sizing_mode == SizingMode::InherentSize;
    let styled = if inherent {
        style.width.resolve(parent_w)
    } else {
        None
    };
    let mut width = match (inputs.known_dimensions.width, styled) {
        (Some(w), _) => w,
        (None, Some(w)) => w.max(min_t),
        (None, None) => match inputs.available_space.width {
            AvailableSpace::Definite(a) => max_t.min(a.max(min_t)),
            AvailableSpace::MinContent => min_t,
            AvailableSpace::MaxContent => max_t,
        },
    };
    if inherent && inputs.known_dimensions.width.is_none() {
        if let Some(mx) = style.max_width.resolve(parent_w) {
            width = width.min(mx.max(min_t));
        }
        if let Some(mn) = style.min_width.resolve(parent_w) {
            width = width.max(mn);
        }
    }
    let content_w = (width - extra_x).max(0.0);
    let col_w = distribute(&cols, content_w);
    let mut col_x = Vec::with_capacity(n);
    let mut x = border.left + padding.left + hs;
    for w in &col_w {
        col_x.push(x);
        x += w + hs;
    }

    // Captions: `caption-side: top` ones stack above the rows, `bottom` ones
    // below, each side in document order.
    let inner_w = (width - inset_x).max(0.0);
    let (mut top_h, mut bottom_h) = (0.0f32, 0.0f32);
    let mut caption_layouts = Vec::new();
    for &c in &t.captions {
        let o = tree.compute_child_layout(
            c,
            child_input(
                RunMode::ComputeSize,
                tf::Size {
                    width: Some(inner_w),
                    height: None,
                },
                tf::Size {
                    width: Some(inner_w),
                    height: None,
                },
                tf::Size {
                    width: AvailableSpace::Definite(inner_w),
                    height: AvailableSpace::MaxContent,
                },
            ),
        );
        let bottom = tree.node(c).style.caption_side == CaptionSide::Bottom;
        let side = if bottom { &mut bottom_h } else { &mut top_h };
        caption_layouts.push((c, bottom, *side, o.size.height));
        *side += o.size.height;
    }
    let caption_h = top_h + bottom_h;

    // Row heights.
    let nrows = t.rows.len();
    let mut row_h = vec![0.0f32; nrows];
    for (i, row) in t.rows.iter().enumerate() {
        if let Some(h) = tree.node(row.node).style.height.resolve(None) {
            row_h[i] = h;
        }
    }
    let cell_w = |c: &Cell| -> f32 {
        let a = c.col as usize;
        let b = ((c.col + c.colspan) as usize).min(n);
        col_w[a..b].iter().sum::<f32>() + hs * (b - a).saturating_sub(1) as f32
    };
    let mut content_h = vec![0.0f32; t.cells.len()];
    for (k, c) in t.cells.iter().enumerate() {
        let w = cell_w(c);
        let o = tree.compute_child_layout(
            c.node,
            content_input(
                RunMode::ComputeSize,
                tf::Size {
                    width: Some(w),
                    height: None,
                },
                tf::Size {
                    width: AvailableSpace::Definite(w),
                    height: AvailableSpace::MaxContent,
                },
            ),
        );
        let styled_h = tree.node(c.node).style.height.resolve(None).unwrap_or(0.0);
        content_h[k] = o.size.height;
        if c.rowspan == 1 && (c.row as usize) < nrows {
            row_h[c.row as usize] = row_h[c.row as usize].max(o.size.height).max(styled_h);
        }
    }
    for (k, c) in t.cells.iter().enumerate() {
        if c.rowspan > 1 {
            let a = c.row as usize;
            let b = ((c.row + c.rowspan) as usize).min(nrows);
            if a >= b {
                continue;
            }
            let total: f32 = row_h[a..b].iter().sum::<f32>() + vs * (b - a - 1) as f32;
            if content_h[k] > total {
                row_h[b - 1] += content_h[k] - total;
            }
        }
    }
    let rows_h: f32 = row_h.iter().sum::<f32>()
        + if nrows > 0 {
            vs * (nrows + 1) as f32
        } else {
            0.0
        };
    let mut height = inset_y + caption_h + rows_h;
    let mut extra_y = 0.0;
    let styled_h = match inputs.known_dimensions.height {
        Some(h) => Some(h),
        None if inherent => style.height.resolve(inputs.parent_size.height),
        None => None,
    };
    if let Some(h) = styled_h
        && h > height
    {
        extra_y = h - height;
        height = h;
    }
    if extra_y > 0.0 && nrows > 0 {
        let per = extra_y / nrows as f32;
        for h in &mut row_h {
            *h += per;
        }
    }

    let size = tf::Size { width, height };
    let mut out = LayoutOutput::from_outer_size(size);
    if inputs.run_mode != RunMode::PerformLayout {
        return out;
    }

    // Final layout: captions, groups, rows, cells.
    let set = |tree: &mut LTree,
               node: tf::NodeId,
               location: taffy::Point<f32>,
               size: tf::Size<f32>,
               cb: Option<f32>| {
        let (b, p) = child_border_padding(tree, node, cb);
        let used_border = tree.node(node).border_override.map(|e| taffy::Rect {
            left: e.left,
            right: e.right,
            top: e.top,
            bottom: e.bottom,
        });
        tree.set_unrounded_layout(
            node,
            &Layout {
                order: 0,
                location,
                size,
                scrollable_overflow_rect: taffy::Rect::zero(),
                scrollbar_size: tf::Size::ZERO,
                border: used_border.unwrap_or(b),
                padding: p,
                margin: taffy::Rect::zero(),
            },
        );
    };
    for (c, bottom, off, h) in caption_layouts {
        tree.compute_child_layout(
            c,
            child_input(
                RunMode::PerformLayout,
                tf::Size {
                    width: Some(inner_w),
                    height: Some(h),
                },
                tf::Size {
                    width: Some(inner_w),
                    height: None,
                },
                tf::Size {
                    width: AvailableSpace::Definite(inner_w),
                    height: AvailableSpace::Definite(h),
                },
            ),
        );
        let y = if bottom {
            height - border.bottom - padding.bottom - bottom_h + off
        } else {
            border.top + padding.top + off
        };
        set(
            tree,
            c,
            taffy::Point {
                x: border.left + padding.left,
                y,
            },
            tf::Size {
                width: inner_w,
                height: h,
            },
            Some(inner_w),
        );
    }
    let top0 = border.top + padding.top + top_h + vs;
    let mut row_y = Vec::with_capacity(nrows);
    let mut y = top0;
    for h in &row_h {
        row_y.push(y);
        y += h + vs;
    }
    let row_left = border.left + padding.left + hs;
    let row_w = (content_w + hs * (n.saturating_sub(1)) as f32).max(0.0);
    // Groups span their rows.
    let mut group_rect: Vec<Option<(f32, f32)>> = vec![None; t.groups.len()];
    for (i, row) in t.rows.iter().enumerate() {
        let g = &mut group_rect[row.group as usize];
        let (top, bottom) = (row_y[i], row_y[i] + row_h[i]);
        *g = Some(match *g {
            Some((a, b)) => (a.min(top), b.max(bottom)),
            None => (top, bottom),
        });
    }
    for (gi, &g) in t.groups.iter().enumerate() {
        let (top, bottom) = group_rect[gi].unwrap_or((top0, top0));
        set(
            tree,
            g,
            taffy::Point {
                x: row_left,
                y: top,
            },
            tf::Size {
                width: row_w,
                height: bottom - top,
            },
            Some(content_w),
        );
    }
    for (i, row) in t.rows.iter().enumerate() {
        let gy = group_rect[row.group as usize].map_or(top0, |r| r.0);
        set(
            tree,
            row.node,
            taffy::Point {
                x: 0.0,
                y: row_y[i] - gy,
            },
            tf::Size {
                width: row_w,
                height: row_h[i],
            },
            Some(content_w),
        );
    }
    for (k, c) in t.cells.iter().enumerate() {
        let r = c.row as usize;
        if r >= nrows {
            continue;
        }
        let rb = ((c.row + c.rowspan) as usize).min(nrows);
        let w = cell_w(c);
        let h = row_h[r..rb].iter().sum::<f32>() + vs * (rb - r - 1) as f32;
        let co = tree.compute_child_layout(
            c.node,
            content_input(
                RunMode::PerformLayout,
                tf::Size {
                    width: Some(w),
                    height: Some(h),
                },
                tf::Size {
                    width: AvailableSpace::Definite(w),
                    height: AvailableSpace::Definite(h),
                },
            ),
        );
        let cx = col_x.get(c.col as usize).copied().unwrap_or(row_left);
        set(
            tree,
            c.node,
            taffy::Point {
                x: cx - row_left,
                y: 0.0,
            },
            tf::Size {
                width: w,
                height: h,
            },
            Some(content_w),
        );
        let free = (h - content_h[k]).max(0.0);
        tree.node_mut(c.node).content_dy = match tree.node(c.node).style.vertical_align {
            VerticalAlign::Middle => free / 2.0,
            VerticalAlign::Bottom => free,
            _ => 0.0,
        };
        let mut oof = co.oof_candidates;
        if !oof.is_empty() {
            oof.translate(taffy::Point { x: cx, y: row_y[r] });
            out.oof_candidates.append(&mut oof);
        }
    }
    if let Some(&first) = row_y.first() {
        // Baseline: the first row's top plus a typical first-line offset is
        // not known here; use the first cell's content baseline if any.
        let _ = first;
    }
    out.oof_positioning_area = Some(taffy::OofPositioningArea {
        size: tf::Size {
            width: width - border.left - border.right,
            height: height - border.top - border.bottom,
        },
        offset: taffy::Point {
            x: border.left,
            y: border.top,
        },
    });
    out
}
