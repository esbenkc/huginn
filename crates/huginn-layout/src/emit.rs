//! Emission: the laid-out tree (`tree.rs`) -> the flat, absolute
//! [`LayoutTree`] that paint and JS read.

use huginn_style::Display;
use taffy::prelude as tf;

use crate::inline::push_glyph_run;
use crate::tree::{Kind, LTree, ix};
use crate::*;

struct Emitter<'a, 'i> {
    tree: &'a mut LTree,
    input: &'a LayoutInput<'i>,
    /// Absolute border-box origin of absolute/fixed boxes, set by their
    /// containing block (which is emitted before them).
    oof: Vec<Option<(f32, f32)>>,
    /// Padding box of the nearest scroll container around the box being
    /// emitted (the viewport at the top): what sticky boxes stick to.
    scrollport: Rect,
    /// Elements whose `overflow` applies to the viewport, not to their own
    /// box (CSS Overflow 3, section 3.3): the root, and `<body>` when the
    /// root's `overflow` is `visible`.
    viewport_overflow: [Option<huginn_dom::NodeId>; 2],
}

pub(crate) fn emit(
    tree: &mut LTree,
    root: tf::NodeId,
    input: &LayoutInput<'_>,
    out: &mut LayoutTree,
) {
    let n = tree.nodes.len();
    let l = tree.node(root).layout;
    let doc = input.document;
    let html = doc.document_element();
    let html_visible = html.and_then(|h| input.styles.get(h)).is_none_or(|s| {
        s.overflow_x == huginn_style::Overflow::Visible
            && s.overflow_y == huginn_style::Overflow::Visible
    });
    let mut e = Emitter {
        tree,
        input,
        oof: vec![None; n],
        scrollport: Rect {
            x: 0.0,
            y: 0.0,
            width: input.viewport.width,
            height: input.viewport.height,
        },
        viewport_overflow: [html, if html_visible { doc.body() } else { None }],
    };
    let root_id = e.emit(out, root, l.location.x, l.location.y, None);
    out.root = Some(root_id);
    let r = out.get(root_id).rect;
    let overflow_bottom = r.y + l.scrollable_overflow_rect.bottom;
    let overflow_right = r.x + l.scrollable_overflow_rect.right;
    out.document_size = Size {
        width: r.right().max(overflow_right).max(input.viewport.width),
        height: r.bottom().max(overflow_bottom).max(input.viewport.height),
    };
    out.build_node_index(input.document.len());
}

fn edges(r: taffy::Rect<f32>) -> Edges<f32> {
    Edges {
        top: r.top,
        right: r.right,
        bottom: r.bottom,
        left: r.left,
    }
}

impl Emitter<'_, '_> {
    fn emit(
        &mut self,
        out: &mut LayoutTree,
        id: tf::NodeId,
        x: f32,
        y: f32,
        parent: Option<BoxId>,
    ) -> BoxId {
        let n = &self.tree.nodes[ix(id)];
        let l = n.layout;
        let border = n.border_override.unwrap_or_else(|| edges(l.border));
        let padding = edges(l.padding);
        let rect = Rect {
            x,
            y,
            width: l.size.width,
            height: l.size.height,
        };
        let kind = match &n.kind {
            Kind::Ifc(_) => BoxKind::Inline,
            Kind::Replaced { image: Some(i), .. } => BoxKind::Image(i.clone()),
            Kind::Replaced { image: None, .. } => BoxKind::Block,
            Kind::Table(_) => BoxKind::Table,
            Kind::TablePart => match n.style.display {
                Display::TableRow => BoxKind::TableRow,
                _ => BoxKind::Block,
            },
            Kind::Container => match n.style.display {
                Display::Flex | Display::InlineFlex => BoxKind::Flex,
                Display::Grid | Display::InlineGrid => BoxKind::Grid,
                _ => BoxKind::Block,
            },
        };
        let kind = if n.style.display == Display::TableCell && !matches!(kind, BoxKind::Image(_)) {
            BoxKind::TableCell
        } else {
            kind
        };
        let url_image = |layer: Option<&huginn_style::BackgroundImage>| match layer {
            Some(huginn_style::BackgroundImage::Url(url)) => self.input.images.get(url).cloned(),
            Some(_) | None => None,
        };
        let background_image = url_image(n.style.background_image.as_ref());
        let mask_image = url_image(n.style.mask.as_deref().map(|m| &m.image));
        let box_id = BoxId(out.boxes.len() as u32);
        let runs_start = out.runs.len() as u32;
        let deco_start = out.decorations.len() as u32;
        let frag_start = out.fragments.len() as u32;
        // The scrolling area, and the scroll offset a script set, clamped to
        // it (CSSOM View: an element scrolls between 0 and scrollWidth/Height
        // minus its padding box).
        let pad = rect.inset(border);
        let sor = l.scrollable_overflow_rect;
        let scroll_size = Size {
            width: sor.right.max(pad.width),
            height: sor.bottom.max(pad.height),
        };
        let scroller = is_scroll_container(&n.style)
            && (n.node.is_none() || !self.viewport_overflow.contains(&n.node));
        let (sx, sy) = match n.node {
            Some(node) if scroller => {
                let (sx, sy) = self.input.document.scroll_offset(node);
                (
                    sx.clamp(0.0, scroll_size.width - pad.width),
                    sy.clamp(0.0, scroll_size.height - pad.height),
                )
            }
            _ => (0.0, 0.0),
        };
        out.boxes.push(LayoutBox {
            node: n.node,
            style: n.style.clone(),
            kind,
            rect,
            border,
            padding,
            scroll_size,
            background_image,
            mask_image,
            parent,
            first_child: None,
            next_sibling: None,
            text: runs_start..runs_start,
            decorations: deco_start..deco_start,
            inline_fragments: frag_start..frag_start,
        });
        // Everything this box contains moves with its scroll offset,
        // including the absolute boxes it is the containing block of.
        for &h in &n.hoisted {
            let hl = self.tree.nodes[ix(h)].layout.location;
            self.oof[ix(h)] = Some((x + hl.x - sx, y + hl.y - sy));
        }
        let dy = n.content_dy;
        let content = rect.inset(border).inset(padding);
        let dom_node = n.node;
        let has_marker = n.marker.is_some();

        if let Kind::Ifc(ifc) = &mut self.tree.nodes[ix(id)].kind {
            crate::inline::emit(ifc, out, content.x - sx, content.y + dy - sy);
        }
        let own_text_end = out.runs.len() as u32;
        if has_marker && own_text_end > runs_start {
            // The list item holds its own text: the marker follows it so the
            // box's run range stays one contiguous range.
            let baseline = out.runs[runs_start as usize].baseline;
            self.emit_marker(out, id, dom_node, content, Some(baseline));
        }
        {
            let b = &mut out.boxes[box_id.0 as usize];
            b.text = runs_start..out.runs.len() as u32;
            b.decorations = deco_start..out.decorations.len() as u32;
            b.inline_fragments = frag_start..out.fragments.len() as u32;
        }

        let child_runs_start = out.runs.len();
        let children = std::mem::take(&mut self.tree.nodes[ix(id)].children);
        // Containing block of sticky children: the content box, or for a
        // scroll container its whole scrolled content.
        let sticky_cb = if scroller {
            Rect {
                x: content.x - sx,
                y: content.y - sy,
                width: content
                    .width
                    .max(scroll_size.width - padding.left - padding.right),
                height: content
                    .height
                    .max(scroll_size.height - padding.top - padding.bottom),
            }
        } else {
            content
        };
        let outer_scrollport = self.scrollport;
        if scroller {
            self.scrollport = pad;
        }
        let mut prev: Option<BoxId> = None;
        for &c in &children {
            let cn = &self.tree.nodes[ix(c)];
            let cl = cn.layout.location;
            let oof = matches!(
                cn.ts.position,
                taffy::Position::Absolute | taffy::Position::Fixed
            );
            let (mut cx, mut cy) = match self.oof[ix(c)] {
                Some(p) if oof => p,
                _ => (x + cl.x - sx, y + cl.y + dy - sy),
            };
            if cn.ts.position == taffy::Position::Sticky {
                let (ox, oy) = self.sticky_offset(c, cx, cy, sticky_cb);
                cx += ox;
                cy += oy;
            }
            let cid = self.emit(out, c, cx, cy, Some(box_id));
            match prev {
                Some(p) => out.boxes[p.0 as usize].next_sibling = Some(cid),
                None => out.boxes[box_id.0 as usize].first_child = Some(cid),
            }
            prev = Some(cid);
        }
        self.scrollport = outer_scrollport;
        self.tree.nodes[ix(id)].children = children;

        if has_marker && own_text_end == runs_start {
            // The text is in child boxes: align the marker with their first line.
            let baseline = out.runs.get(child_runs_start).map(|r| r.baseline);
            let marker_start = out.runs.len() as u32;
            let deco_start = out.decorations.len() as u32;
            self.emit_marker(out, id, dom_node, content, baseline);
            // The list item has no own text (so no own decorations either):
            // its empty ranges can hold the marker.
            let b = &mut out.boxes[box_id.0 as usize];
            b.text = marker_start..out.runs.len() as u32;
            if b.decorations.is_empty() {
                b.decorations = deco_start..out.decorations.len() as u32;
            }
        }
        box_id
    }

    /// Outside list marker: its end touches the content box's start edge.
    /// `disc`, `circle` and `square` are shapes, sized and placed as Blink
    /// does (`ListMarker::InlineMarginsForOutside`, `RelativeSymbolMarkerRect`).
    fn emit_marker(
        &mut self,
        out: &mut LayoutTree,
        id: tf::NodeId,
        node: Option<huginn_dom::NodeId>,
        content: Rect,
        baseline: Option<f32>,
    ) {
        let n = &self.tree.nodes[ix(id)];
        let Some(m) = n.marker.as_deref() else { return };
        let Some(first) = m.layout.lines().next() else {
            return;
        };
        let line_baseline = first.metrics().baseline;
        let baseline = baseline.unwrap_or(content.y + line_baseline);
        if let Some((shape, ascent)) = m.symbol {
            // The marker box starts `offset + 8` px before the content; the
            // shape is 1px into it, below the top of its text box.
            let offset = (ascent * 2.0 / 3.0).floor();
            let size = ((offset + 1.0) / 2.0).floor();
            let x = content.x - offset - 7.0;
            let y = baseline - ascent + (3.0 * (ascent - offset) / 2.0).floor();
            out.decorations.push(DecorationLine {
                rect: Rect {
                    x: x.round(),
                    y: y.round(),
                    width: size,
                    height: size,
                },
                color: n.style.color,
                shape,
            });
            return;
        }
        let origin_x = content.x - m.layout.full_width();
        let origin_y = baseline - line_baseline;
        for line in m.layout.lines() {
            for item in line.items() {
                if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                    push_glyph_run(out, &gr, origin_x, origin_y, node);
                }
            }
        }
    }

    /// Offset of a sticky box from its in-flow position (CSS Positioned
    /// Layout 3, section 3.4): it moves just enough to keep its border box
    /// inside the scrollport minus its non-auto insets, but its margin box
    /// never leaves its containing block `cb`. Rect at (`x`, `y`).
    fn sticky_offset(&self, id: tf::NodeId, x: f32, y: f32, cb: Rect) -> (f32, f32) {
        let n = &self.tree.nodes[ix(id)];
        let (size, m, inset) = (n.layout.size, n.layout.margin, &n.style.inset);
        let sp = self.scrollport;
        let axis = |start: f32,
                    len: f32,
                    sp0: f32,
                    sp_len: f32,
                    cb0: f32,
                    cb_len: f32,
                    m0: f32,
                    m1: f32,
                    i0: Option<f32>,
                    i1: Option<f32>| {
            let mut d = 0.0f32;
            if let Some(i) = i0 {
                // Push forward to the start edge, at most to the end of the containing block.
                let want = sp0 + i - start;
                let room = cb0 + cb_len - (start + len + m1);
                if want > 0.0 {
                    d = want.min(room.max(0.0));
                }
            }
            if let Some(i) = i1 {
                let want = start + d + len - (sp0 + sp_len - i);
                let room = start + d - m0 - cb0;
                if want > 0.0 {
                    d -= want.min(room.max(0.0));
                }
            }
            d
        };
        let dx = axis(
            x,
            size.width,
            sp.x,
            sp.width,
            cb.x,
            cb.width,
            m.left,
            m.right,
            inset.left.resolve(Some(sp.width)),
            inset.right.resolve(Some(sp.width)),
        );
        let dy = axis(
            y,
            size.height,
            sp.y,
            sp.height,
            cb.y,
            cb.height,
            m.top,
            m.bottom,
            inset.top.resolve(Some(sp.height)),
            inset.bottom.resolve(Some(sp.height)),
        );
        (dx, dy)
    }
}

/// A scroll container (CSS Overflow 3): `overflow` other than `visible` and
/// `clip`. Scripts can scroll it, even when `hidden`.
fn is_scroll_container(s: &huginn_style::ComputedStyle) -> bool {
    use huginn_style::Overflow::{Auto, Hidden, Scroll};
    matches!(s.overflow_x, Hidden | Scroll | Auto) || matches!(s.overflow_y, Hidden | Scroll | Auto)
}
