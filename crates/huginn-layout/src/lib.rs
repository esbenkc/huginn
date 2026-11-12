//! huginn-layout: box tree construction and layout.
//!
//! Input: DOM ([`huginn_dom::Document`]), computed styles ([`huginn_style::StyleMap`])
//! and decoded images ([`huginn_net::ImageStore`]).
//! Output: a [`LayoutTree`] of positioned boxes with shaped, positioned text.
//!
//! - Block, flex and grid layout: Taffy (library), run on our own tree
//!   through Taffy's low-level API (`tree.rs`).
//! - Inline formatting contexts (text, inline elements, inline-blocks,
//!   inline images): our code (`inline.rs`) on Parley (library) for shaping
//!   and line breaking.
//! - Tables: our CSS 2.1 automatic table layout (`table.rs`).
//! - Box generation, anonymous boxes, style conversion, list markers, fonts
//!   and the output tree: our code.
//!
//! All output coordinates are absolute document coordinates in CSS px.

mod build;
mod convert;
mod emit;
mod fonts;
mod inline;
mod table;
mod tree;

use std::ops::Range;
use std::sync::Arc;

use huginn_dom::{Document, NodeId};
use huginn_net::{DecodedImage, ImageStore, Url};
use huginn_style::StyleMap;
pub use huginn_style::{Color, ComputedStyle, Edges};
/// Font data shared by text runs and the rasterizer (an `Arc`'d blob plus face index).
pub use parley::FontData;

/// A size in CSS px.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

/// An axis-aligned rectangle in CSS px.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    /// Shrink by `e` on each side.
    pub fn inset(&self, e: Edges<f32>) -> Rect {
        Rect {
            x: self.x + e.left,
            y: self.y + e.top,
            width: (self.width - e.left - e.right).max(0.0),
            height: (self.height - e.top - e.bottom).max(0.0),
        }
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }
}

/// Everything layout needs to read.
pub struct LayoutInput<'a> {
    pub document: &'a Document,
    pub styles: &'a StyleMap,
    pub images: &'a ImageStore,
    /// Document base URL, used to resolve `<img src>`.
    pub base_url: &'a Url,
    pub viewport: Size,
}

/// Index of a box in [`LayoutTree::boxes`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct BoxId(pub u32);

/// What kind of box this is. Paint uses it to decide what to draw besides
/// background and borders.
#[derive(Clone, Debug)]
pub enum BoxKind {
    /// Block container (also list items, table captions and row groups).
    Block,
    Flex,
    Grid,
    /// Holds an inline formatting context: its text runs are in
    /// [`LayoutBox::text`], its inline element fragments in
    /// [`LayoutBox::inline_fragments`], inline images/inline-blocks are its
    /// children.
    Inline,
    /// Replaced image. The content box shows the image, stretched.
    Image(DecodedImage),
    /// Table box (border, background). Children: captions, row groups.
    Table,
    /// Table row. Children: cells.
    TableRow,
    /// Table cell (a block container).
    TableCell,
}

/// A positioned box.
#[derive(Clone, Debug)]
pub struct LayoutBox {
    /// Generating element, `None` for anonymous boxes.
    pub node: Option<NodeId>,
    pub style: Arc<ComputedStyle>,
    pub kind: BoxKind,
    /// Border box, absolute.
    pub rect: Rect,
    /// Used border widths.
    pub border: Edges<f32>,
    /// Used padding.
    pub padding: Edges<f32>,
    /// Scrolling area size from the padding box origin, at least the padding
    /// box (CSSOM View `scrollWidth`/`scrollHeight`).
    pub scroll_size: Size,
    /// Decoded `background-image`, if the style has one and it loaded.
    pub background_image: Option<DecodedImage>,
    /// Decoded `mask-image` URL, if the style has one and it loaded.
    pub mask_image: Option<DecodedImage>,
    pub parent: Option<BoxId>,
    pub first_child: Option<BoxId>,
    pub next_sibling: Option<BoxId>,
    /// Text runs (indices into [`LayoutTree::runs`]) painted with this box,
    /// including list markers.
    pub text: Range<u32>,
    /// Text decoration lines (indices into [`LayoutTree::decorations`]).
    pub decorations: Range<u32>,
    /// Fragments of inline elements in this box's inline formatting context
    /// (indices into [`LayoutTree::fragments`]). Empty for other boxes.
    pub inline_fragments: Range<u32>,
}

impl LayoutBox {
    /// Padding box (border box minus borders).
    pub fn padding_rect(&self) -> Rect {
        self.rect.inset(self.border)
    }

    /// Content box.
    pub fn content_rect(&self) -> Rect {
        self.padding_rect().inset(self.padding)
    }
}

/// A run of glyphs with one font, size and color.
#[derive(Clone, Debug)]
pub struct TextRun {
    pub font: FontData,
    pub font_size: f32,
    pub color: Color,
    /// Glyphs, indices into [`LayoutTree::glyphs`].
    pub glyphs: Range<u32>,
    /// Variable-font coordinates, indices into [`LayoutTree::coords`].
    pub coords: Range<u32>,
    /// Synthetic italic: skew angle in degrees.
    pub skew: Option<f32>,
    /// Synthetic bold requested by font matching.
    pub embolden: bool,
    /// Absolute y of the baseline.
    pub baseline: f32,
    /// Innermost element of the run: the inline element (span, link) that
    /// holds it, else the block that owns the inline formatting context.
    /// `None` for text of anonymous boxes outside any inline element.
    pub node: Option<NodeId>,
}

/// The rendered text of the runs, for selection, find and copy in the
/// window. Built only when [`LayoutEngine::set_text_index`] is on; the
/// `render` path never builds it.
#[derive(Debug, Default)]
pub struct TextIndex {
    /// Text of the runs in run order, as laid out (collapsed whitespace,
    /// `<br>` as `'\n'`). Inline formatting contexts are separated by one
    /// `'\n'`; text between two runs of one context that no run shows (a
    /// collapsed space at a line break) is one `' '`.
    pub text: String,
    /// One entry per run, parallel to [`LayoutTree::runs`].
    pub runs: Vec<RunText>,
    /// Grapheme clusters of all runs; each run's are in logical order.
    pub clusters: Vec<TextCluster>,
}

/// Text of one run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunText {
    /// Bytes of [`TextIndex::text`]. Empty for list markers and generated text
    /// that is not part of the selectable content.
    pub text: Range<u32>,
    /// Indices into [`TextIndex::clusters`].
    pub clusters: Range<u32>,
    /// Top and bottom of the run's line box, absolute.
    pub top: f32,
    pub bottom: f32,
}

/// One grapheme cluster: its text starts at `text` (a byte offset into
/// [`TextIndex::text`]) and ends where the next cluster of the run starts
/// (or at the end of the run's text).
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct TextCluster {
    pub text: u32,
    /// Left edge and advance, absolute.
    pub x: f32,
    pub width: f32,
}

impl TextIndex {
    /// Clusters of run `i`.
    pub fn clusters(&self, i: usize) -> &[TextCluster] {
        let r = &self.runs[i].clusters;
        &self.clusters[r.start as usize..r.end as usize]
    }

    /// End byte of cluster `k` of run `i`.
    pub fn cluster_end(&self, i: usize, k: usize) -> u32 {
        self.clusters(i)
            .get(k + 1)
            .map_or(self.runs[i].text.end, |c| c.text)
    }
}

/// One line's piece of an inline element (or an inline `::before`/`::after`,
/// with the owning element as `node`). Paint draws its background and
/// border; JS reads its geometry.
#[derive(Clone, Debug)]
pub struct InlineFragment {
    pub node: NodeId,
    pub style: Arc<ComputedStyle>,
    /// Border box of this fragment, absolute.
    pub rect: Rect,
    /// The fragment holds the element's start (draws the left border and padding).
    pub first: bool,
    /// The fragment holds the element's end (draws the right border and padding).
    pub last: bool,
    /// Product of the `opacity` of the element and its inline ancestors in
    /// the same line box: paint scales the fragment's alpha by it.
    pub opacity: f32,
}

/// A glyph at an absolute position (x along the baseline, y = baseline).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct PositionedGlyph {
    pub id: u32,
    pub x: f32,
    pub y: f32,
}

/// Underline / overline / line-through (a filled rectangle), or the shape of
/// a `disc`, `circle` or `square` list marker.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct DecorationLine {
    pub rect: Rect,
    pub color: Color,
    pub shape: DecorationShape,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DecorationShape {
    /// Fill `rect`.
    Rect,
    /// Fill the ellipse inscribed in `rect`.
    Ellipse,
    /// Stroke the ellipse inscribed in `rect`, 1px wide.
    EllipseOutline,
}

/// Layout result: a flat box arena plus shared text buffers.
#[derive(Debug, Default)]
pub struct LayoutTree {
    pub boxes: Vec<LayoutBox>,
    pub runs: Vec<TextRun>,
    pub glyphs: Vec<PositionedGlyph>,
    pub coords: Vec<i16>,
    pub decorations: Vec<DecorationLine>,
    /// Inline element fragments, in tree order of the elements, then line order.
    pub fragments: Vec<InlineFragment>,
    /// Box of the root element (`<html>`), if it is rendered.
    pub root: Option<BoxId>,
    pub viewport: Size,
    /// Scrollable size of the document.
    pub document_size: Size,
    /// Rendered text of the runs, if the engine was asked to build it.
    pub text_index: Option<TextIndex>,
    /// Node index: boxes of node `i` are `node_boxes[node_box_start[i]..node_box_start[i + 1]]`.
    node_box_start: Vec<u32>,
    node_boxes: Vec<BoxId>,
    /// Same for fragments (indices into `fragments`).
    node_frag_start: Vec<u32>,
    node_frags: Vec<u32>,
}

impl LayoutTree {
    pub fn get(&self, id: BoxId) -> &LayoutBox {
        &self.boxes[id.0 as usize]
    }

    /// Children in paint (tree) order.
    pub fn children(&self, id: BoxId) -> impl Iterator<Item = BoxId> + '_ {
        let mut next = self.get(id).first_child;
        std::iter::from_fn(move || {
            let cur = next?;
            next = self.get(cur).next_sibling;
            Some(cur)
        })
    }

    pub fn text_runs(&self, id: BoxId) -> &[TextRun] {
        let r = &self.get(id).text;
        &self.runs[r.start as usize..r.end as usize]
    }

    pub fn glyphs(&self, run: &TextRun) -> &[PositionedGlyph] {
        &self.glyphs[run.glyphs.start as usize..run.glyphs.end as usize]
    }

    pub fn coords(&self, run: &TextRun) -> &[i16] {
        &self.coords[run.coords.start as usize..run.coords.end as usize]
    }

    pub fn decorations(&self, id: BoxId) -> &[DecorationLine] {
        let r = &self.get(id).decorations;
        &self.decorations[r.start as usize..r.end as usize]
    }

    /// The first box generated by `node`, if any.
    pub fn box_for_node(&self, node: NodeId) -> Option<BoxId> {
        self.boxes_for_node(node).first().copied()
    }

    /// O(1) node -> boxes (tree order), for getBoundingClientRect and offset*.
    /// Inline elements have no box; see [`LayoutTree::fragments_for_node`].
    pub fn boxes_for_node(&self, node: NodeId) -> &[BoxId] {
        let i = node.index();
        match (self.node_box_start.get(i), self.node_box_start.get(i + 1)) {
            (Some(&a), Some(&b)) => &self.node_boxes[a as usize..b as usize],
            _ => &[],
        }
    }

    /// O(1) node -> inline fragments (line order).
    pub fn fragments_for_node(&self, node: NodeId) -> impl Iterator<Item = &InlineFragment> + '_ {
        let i = node.index();
        let r = match (self.node_frag_start.get(i), self.node_frag_start.get(i + 1)) {
            (Some(&a), Some(&b)) => a as usize..b as usize,
            _ => 0..0,
        };
        self.node_frags[r]
            .iter()
            .map(|&f| &self.fragments[f as usize])
    }

    /// Build the node index (counting sort by node).
    fn build_node_index(&mut self, nodes: usize) {
        fn csr(
            nodes: usize,
            keys: impl Iterator<Item = Option<usize>> + Clone,
        ) -> (Vec<u32>, Vec<u32>) {
            let mut start = vec![0u32; nodes + 1];
            for k in keys.clone().flatten() {
                if k < nodes {
                    start[k + 1] += 1;
                }
            }
            for i in 0..nodes {
                start[i + 1] += start[i];
            }
            let mut fill = start.clone();
            let mut items = vec![0u32; start[nodes] as usize];
            for (j, k) in keys.enumerate() {
                if let Some(k) = k
                    && k < nodes
                {
                    items[fill[k] as usize] = j as u32;
                    fill[k] += 1;
                }
            }
            (start, items)
        }
        let (s, b) = csr(nodes, self.boxes.iter().map(|b| b.node.map(|n| n.index())));
        self.node_box_start = s;
        self.node_boxes = b.into_iter().map(BoxId).collect();
        let (s, f) = csr(nodes, self.fragments.iter().map(|f| Some(f.node.index())));
        self.node_frag_start = s;
        self.node_frags = f;
    }
}

/// Owns font and text-layout caches. Reuse one engine across layouts.
pub struct LayoutEngine {
    fonts: fonts::Fonts,
    text: parley::LayoutContext<inline::Brush>,
    /// Build [`LayoutTree::text_index`].
    text_index: bool,
}

impl Default for LayoutEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutEngine {
    /// System fonts load lazily: only the families that styles ask for are
    /// registered (see `fonts.rs`). Cheap to create.
    pub fn new() -> LayoutEngine {
        LayoutEngine {
            fonts: fonts::Fonts::new(),
            text: parley::LayoutContext::new(),
            text_index: false,
        }
    }

    /// Also build [`LayoutTree::text_index`] (off by default). The window
    /// turns it on for selection, find and copy.
    pub fn set_text_index(&mut self, on: bool) {
        self.text_index = on;
    }

    /// Register a web font (from `@font-face`). `data` must be TrueType/OpenType
    /// (WOFF/WOFF2 must be decompressed first). Returns the number of faces added.
    pub fn register_font(
        &mut self,
        data: Vec<u8>,
        family: &str,
        weight: f32,
        style: huginn_style::FontStyle,
    ) -> usize {
        let blob = parley::fontique::Blob::new(Arc::new(data));
        let info = parley::fontique::FontInfoOverride {
            family_name: Some(family),
            weight: Some(parley::FontWeight::new(weight)),
            style: Some(match style {
                huginn_style::FontStyle::Normal => parley::FontStyle::Normal,
                huginn_style::FontStyle::Italic => parley::FontStyle::Italic,
                huginn_style::FontStyle::Oblique => parley::FontStyle::Oblique(None),
            }),
            ..Default::default()
        };
        let added = self
            .fonts
            .ctx
            .collection
            .register_fonts(blob, Some(info))
            .iter()
            .map(|(_, faces)| faces.len())
            .sum();
        self.fonts.forget_lists();
        added
    }

    /// Lay out a document.
    pub fn layout(&mut self, input: &LayoutInput<'_>) -> LayoutTree {
        build::layout(self, input)
    }
}
