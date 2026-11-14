# Huginn contracts

This file is the interface between crates. Each crate has one owner. An owner
may change anything inside the crate, but not the public items listed here,
unless the same commit updates this file and the callers. Read
[ARCHITECTURE.md](ARCHITECTURE.md) first.

Every item in "Today" sections exists in the code now. Items in "To add"
sections do not exist yet; their signatures are the agreed shape, so two
owners can build both sides at the same time.

## Rules for all crates

- **Units.** All geometry is CSS px as `f32`. Layout and paint use absolute
  document coordinates (origin = top-left of the document, not the viewport).
  The device pixel ratio is applied only in `Rasterizer::rasterize`.
- **Colors.** `huginn_style::Color` is sRGB with straight alpha, `u8` channels.
- **Node identity.** `huginn_dom::NodeId` is the only way to name a DOM node
  across crates. Side tables index by `NodeId::index()`.
- **Changing a contract.** Edit the code, this file and every caller in one
  commit. Tell the owners of the affected crates.
- **Build and test.** Parallel builds can use separate `CARGO_TARGET_DIR`s
  so they do not wait on the cargo lock. The fixture test is
  `cargo test --release -p huginn-shell --test fixtures`. It must
  pass after every commit.
- **CLI.** `huginn render <url-or-file> --out <png> [--width 1280 --height 800] [--full-page] [--no-js]`
  exits 0 on success and prints exactly one JSON line on stdout with at least
  `url`, `parse_ms`, `style_ms`, `layout_ms`, `paint_ms`, `total_ms`, `nodes`.
  Errors go to stderr with exit 1. Do not print anything else on stdout.

## huginn-dom

Arena DOM. Owner: huginn-dom.

### Today

```rust
pub struct NodeId(NonZeroU32);            // Copy, Eq, Ord, Hash; Debug prints "#index"
impl NodeId {
    pub const DOCUMENT: NodeId;           // always arena index 0
    pub fn index(self) -> usize;          // arena index; key for side tables
    pub fn from_index(index: usize) -> NodeId;
    pub fn to_raw(self) -> u32;           // opaque handle for V8 wrappers (never 0)
    pub fn from_raw(raw: u32) -> Option<NodeId>;
}

pub struct Node {
    pub parent: Option<NodeId>, pub first_child: Option<NodeId>, pub last_child: Option<NodeId>,
    pub prev_sibling: Option<NodeId>, pub next_sibling: Option<NodeId>,
    pub data: NodeData,
}
pub enum NodeData {
    Document, DocumentFragment,
    Doctype { name: StrTendril, public_id: StrTendril, system_id: StrTendril },
    Element(ElementData), Text(StrTendril), Comment(StrTendril),
    ProcessingInstruction { target: StrTendril, data: StrTendril },
}
pub struct ElementData {
    pub name: QualName, pub attrs: Vec<Attribute>,
    pub template_contents: Option<NodeId>,   // <template> only
    pub shadow_root: Option<NodeId>,         // shadow hosts only (a DocumentFragment)
}
impl ElementData {
    pub fn attr(&self, name: &LocalName) -> Option<&str>;   // null namespace
    pub fn id(&self) -> Option<&str>;
    pub fn classes(&self) -> impl Iterator<Item = &str>;
    pub fn is_html(&self, local: &LocalName) -> bool;
}

pub fn parse_html(html: &str) -> Document;
pub fn parse_html_bytes(bytes: &[u8]) -> Document;  // UTF-8 only; the shell decodes with huginn_net's Resource::html_text and calls parse_html

impl Document {
    pub fn new() -> Document;
    pub fn len(&self) -> usize;                // arena size, detached nodes included
    pub fn generation(&self) -> u64;           // bumps on every tree, attribute or text change
    // element scroll offsets set by scripts (layout clamps them); changes bump scroll_generation only
    pub fn scroll_offset(&self, id: NodeId) -> (f32, f32); pub fn set_scroll_offset(&mut self, id: NodeId, x: f32, y: f32);
    pub fn scroll_generation(&self) -> u64;
    pub fn quirks_mode(&self) -> QuirksMode;   pub fn set_quirks_mode(&mut self, mode: QuirksMode);
    // read
    pub fn node(&self, id: NodeId) -> &Node;
    pub fn element(&self, id: NodeId) -> Option<&ElementData>;
    pub fn text(&self, id: NodeId) -> Option<&str>;
    pub fn parent / first_child / last_child / next_sibling / prev_sibling(&self, id: NodeId) -> Option<NodeId>;
    pub fn children(&self, id: NodeId) -> Children<'_>;        // Iterator<Item = NodeId>
    pub fn descendants(&self, id: NodeId) -> Descendants<'_>;  // pre-order, includes id
    pub fn ancestors(&self, id: NodeId) -> Ancestors<'_>;      // parent first
    pub fn document_element(&self) -> Option<NodeId>;
    pub fn body(&self) -> Option<NodeId>;
    pub fn head(&self) -> Option<NodeId>;
    pub fn element_by_id(&self, id: &str) -> Option<NodeId>;   // linear scan, or the id index
    /// Keep an id index from now on (the V8 engine calls it); element_by_id
    /// then looks up candidates and checks they are connected.
    pub fn index_ids(&mut self);
    // hover/active/focus for :hover, :active, :focus, :focus-visible, :focus-within
    // (InteractionState { hover, active, focus: Option<NodeId>, focus_visible: bool });
    // not counted in generation(); set by the window and by focus()/blur()
    pub fn interaction(&self) -> &InteractionState; pub fn set_interaction(&mut self, s: InteractionState);
    pub fn elements_by_local_name<'a>(&'a self, name: &'a LocalName) -> impl Iterator<Item = NodeId> + 'a;
    pub fn attribute(&self, id: NodeId, name: &LocalName) -> Option<&str>;
    pub fn text_content(&self, id: NodeId) -> String;
    // create (detached)
    pub fn create_element(&mut self, name: QualName, attrs: Vec<Attribute>) -> NodeId;
    pub fn create_html_element(&mut self, local: &str) -> NodeId;
    pub fn create_text(&mut self, text: &str) -> NodeId;
    pub fn create_text_tendril(&mut self, text: StrTendril) -> NodeId;
    pub fn create_comment(&mut self, text: &str) -> NodeId;
    pub fn create_node(&mut self, data: NodeData) -> NodeId;
    // mutate
    pub fn detach(&mut self, id: NodeId);                                  // removeChild
    pub fn append_child(&mut self, parent: NodeId, child: NodeId);         // panics on cycles
    pub fn insert_before(&mut self, parent: NodeId, child: NodeId, reference: Option<NodeId>);
    pub fn remove_children(&mut self, id: NodeId);
    pub fn set_attribute(&mut self, id: NodeId, name: LocalName, value: &str);  // panics on non-element
    pub fn remove_attribute(&mut self, id: NodeId, name: &LocalName) -> bool;
    pub fn set_character_data(&mut self, id: NodeId, text: &str);         // text or comment
    pub fn set_text_content(&mut self, id: NodeId, text: &str);
    pub fn append_text(&mut self, parent: NodeId, text: &str);            // merges with last text child
    pub fn clone_node(&mut self, id: NodeId, deep: bool) -> NodeId;        // cloneNode, detached copy
    // shadow DOM (src/shadow.rs). A shadow root is a DocumentFragment linked to
    // its host, never a child of it: children/descendants/ancestors stay in one tree.
    pub fn attach_shadow(&mut self, host: NodeId) -> Option<NodeId>;      // None: not a valid host, or has one
    pub fn shadow_root(&self, host: NodeId) -> Option<NodeId>;
    pub fn shadow_host(&self, root: NodeId) -> Option<NodeId>;            // None for non-shadow-roots
    pub fn has_shadow_roots(&self) -> bool;
    pub fn root(&self, id: NodeId) -> NodeId;                             // topmost ancestor
    pub fn containing_shadow_root(&self, id: NodeId) -> Option<NodeId>;
    pub fn assigned_slot(&self, node: NodeId) -> Option<NodeId>;          // named slot assignment
    // Rendering walks the flat tree: host -> shadow root children, slot -> assigned nodes (or fallback).
    pub fn flat_children(&self, id: NodeId) -> FlatChildren<'_>;
    pub fn flat_parent(&self, id: NodeId) -> Option<NodeId>;
    pub fn shadow_including_descendants(&self, id: NodeId) -> ShadowIncludingDescendants<'_>; // host, its shadow tree, then children
}
/// innerHTML / insertAdjacentHTML: parse `html` in the context of `context`
/// and return the new top-level nodes, detached, in order.
pub fn parse_fragment(doc: &mut Document, context: NodeId, html: &str) -> Vec<NodeId>;
/// outerHTML (`include_self`) / innerHTML getter.
pub fn serialize(doc: &Document, id: NodeId, include_self: bool) -> String;
```

Re-exports: `QualName`, `LocalName`, `Namespace`, `Prefix`, `Attribute`,
`StrTendril`, `QuirksMode`, and the `local_name!`, `ns!`, `namespace_url!`
macros from markup5ever.

Invariants:
- `NodeId::DOCUMENT` is the root. Ids are stable and never reused. Detached
  nodes stay in the arena.
- All mutation goes through `Document` methods, and each one bumps
  `generation()`. Style and layout results are valid only for the generation
  they were computed at.
- The document parser attaches `<template shadowrootmode>` as a declarative
  shadow root (`parse_fragment` does not). Style, layout and paint render the
  flat tree; DOM queries do not cross shadow boundaries.
- The parser runs with scripting enabled (html5ever's default, same as
  Chrome). So `<noscript>` contains one raw text node, and the UA stylesheet
  hides it.
- `Document` is not `Send`/`Sync` (non-atomic `StrTendril`).

### To add (huginn-dom owner)

```rust
```

If Stylo needs `Send + Sync` access to the DOM for parallel traversal, switch
text and attribute storage to atomic tendrils or `Box<str>`, keeping the
read API above (`text()`, `attr()` return `&str`).

## huginn-style

CSS parsing, selector matching, cascade (Stylo 0.24 inside; see
ARCHITECTURE.md, "The Stylo decision"). Owner: huginn-style.

### Today

```rust
pub struct Viewport { pub width: f32, pub height: f32, pub device_pixel_ratio: f32 }
pub enum Origin { UserAgent, User, Author }
pub trait StylesheetLoader {
    /// Fetch an @import. Returns (css text, final URL), or None.
    fn load(&self, url: &str) -> Option<(String, String)>;
}
pub struct NoImports;                          // StylesheetLoader that loads nothing

pub struct StyleEngine { .. }
impl StyleEngine {
    pub fn new(viewport: Viewport) -> StyleEngine;          // UA sheet already loaded
    pub fn viewport(&self) -> Viewport;
    pub fn set_viewport(&mut self, viewport: Viewport);
    /// Base for url() in `style` attributes and presentational hints.
    pub fn set_document_url(&mut self, url: &str);
    /// Call in document order. `base_url` resolves url() and @import.
    pub fn add_stylesheet(&mut self, css: &str, base_url: &str, origin: Origin, loader: &dyn StylesheetLoader);
    /// Some document rule uses :hover / :active / the focus pseudo-classes (true with shadow roots).
    pub fn uses_state(&self, doc: &Document, hover: bool, active: bool, focus: bool) -> bool;
    pub fn font_faces(&self) -> &[FontFaceRule];
    pub fn compute(&self, doc: &Document) -> StyleMap;
}
pub const UA_STYLESHEET: &str;                 // src/ua.css

pub struct StyleMap { .. }
impl StyleMap {
    /// None for non-elements, text nodes and display:none subtrees.
    pub fn get(&self, id: NodeId) -> Option<&Arc<ComputedStyle>>;
    /// ::before / ::after, only if the pseudo generates a box.
    pub fn pseudo(&self, id: NodeId, which: PseudoElement) -> Option<&Arc<ComputedStyle>>;
    pub fn styled_count(&self) -> usize;
}
pub enum PseudoElement { Before, After }

pub struct FontFaceRule { pub family: Arc<str> /* as written */, pub sources: Vec<FontFaceSource>,
                          pub weight: (f32, f32), pub style: FontStyle,
                          pub unicode_range: Vec<(u32, u32)> /* inclusive; empty = all */,
                          pub size_adjust: f32 /* size-adjust factor; the shell rescales the font */,
                          pub display: FontDisplay /* font-display; Auto when absent */ }
pub struct FontFaceSource { pub url: String /* absolute */, pub format: Option<String> /* lowercase */ }
pub enum FontDisplay { Auto /* default */, Block, Swap, Fallback, Optional }
```

`ComputedStyle` (all fields `pub`, `Clone + Debug + PartialEq`, `Default` =
CSS initial values with 16px serif, `ComputedStyle::inherit_from(&parent)`,
`line_height_px()`):

| group | fields |
|---|---|
| box generation | `display: Display`, `position: Position`, `float: Float`, `clear: Clear`, `box_sizing: BoxSizing`, `inset: Edges<LengthPercentageAuto>`, `z_index: Option<i32>`, `overflow_x/overflow_y: Overflow`, `visibility: Visibility`, `pointer_events: PointerEvents` (`Auto`, `None`; inherited), `opacity: f32` |
| sizing | `width`, `height`, `min_width`, `min_height`, `max_width`, `max_height` (all `LengthPercentageAuto`; `Auto` = `none` for max-*), `aspect_ratio: Option<f32>` |
| box model | `margin: Edges<LengthPercentageAuto>`, `padding: Edges<LengthPercentage>`, `border_width: Edges<f32>`, `border_style: Edges<BorderStyle>`, `border_color: Edges<Color>`, `border_radius: Corners<LengthPercentage>` |
| background | `background_color: Color`, `background_image: Option<BackgroundImage>` (`Url(Arc<str>)` absolute, `LinearGradient(Arc<Gradient>)`, `RadialGradient(Arc<Gradient>)`; the topmost drawable layer), `background_repeat: BackgroundRepeat`, `background_size: BackgroundSize`, `background_position: (LengthPercentage, LengthPercentage)` (keywords resolved: `right 10px` = `Calc{px:-10, percent:100}`) |
| masking | `mask: Option<Arc<Mask>>` (`None` = `mask-image: none`; `Mask { image: BackgroundImage, size, position, repeat }`, the first drawable layer, against the border box; creates a stacking context) |
| flex/align | `flex_direction`, `flex_wrap`, `flex_grow`, `flex_shrink`, `flex_basis`, `order`, `justify_content`, `align_items`, `align_self`, `align_content`, `justify_items`, `justify_self` (`Alignment`), `row_gap`, `column_gap` |
| grid | `grid_template_columns`, `grid_template_rows: Option<Arc<[GridTrack]>>` (`None` = `none`), `grid_template_areas: Option<Arc<[GridArea]>>`, `grid_auto_columns`, `grid_auto_rows: Option<Arc<[TrackSize]>>` (`None` = `auto`), `grid_auto_flow: GridAutoFlow`, `grid_column`, `grid_row: (GridLine, GridLine)` (start, end) |
| tables | `border_collapse: BorderCollapse` (inherited), `border_spacing: (f32, f32)` (h, v px, inherited), `table_layout: TableLayout`, `caption_side: CaptionSide` (inherited) |
| effects | `box_shadow: Option<Arc<[BoxShadow]>>`, `transform: Option<Arc<[TransformOp]>>` (2D, left to right), `transform_origin: (LengthPercentage, LengthPercentage)`, `outline_width`, `outline_style: BorderStyle`, `outline_color`, `outline_offset`, `object_fit: ObjectFit`, `text_overflow: TextOverflow`, `clip_path: Option<Arc<ClipPath>>` (border-box relative; `None` also for `url()`; an absolutely positioned box without one gets its legacy `clip: rect()` as an `Inset`), `filter`, `backdrop_filter: Option<Arc<[FilterFunction]>>` (`None` = `none`; left to right; identity lists such as `blur(0)` are kept; `url()` does not parse), `mix_blend_mode: BlendMode`, `isolation: Isolation` |
| text (inherited) | `color`, `font_family: Arc<str>` (the list as written), `font_size`, `font_weight` (1..=1000), `font_style`, `line_height: LineHeight`, `text_align` (incl. `WebkitCenter/Left/Right`: also aligns block children), `text_indent`, `text_transform`, `white_space`, `text_wrap_balance` (`text-wrap-style: balance`), `letter_spacing`, `word_spacing`, `list_style_type`, `list_style_position`, `font_feature_settings: Option<Arc<[FontFeature]>>` (`None` = `normal`) |
| SVG paint (inherited) | `fill: Option<Color>` (`None` = `none`, initial black), `stroke: Option<Color>` (initial `None`) |
| decoration | `text_decoration_line: TextDecorationLine`, `text_decoration_color`, `vertical_align: VerticalAlign` |
| generated | `content: Content` (only on pseudo styles): `None`, `Text(Arc<str>)`, or `Items([ContentItem::Text | ContentItem::Image(absolute url)])` when the list has an image; alt text after `/` is dropped |

Value types in `huginn_style::values` (re-exported at the crate root): `Color`,
`LengthPercentage { Px, Percent, Calc(CalcLength) }` (percent is 0..=100;
`CalcLength { px, percent }` is `px + percent% of the basis`, use
`resolve(basis)`), `LengthPercentageAuto { Auto, Px, Percent, Calc }`,
`Edges<T>`, `Corners<T>`, `Display` (flattened:
`None, Contents, Block, Inline, InlineBlock, FlowRoot, ListItem, Flex,
InlineFlex, Grid, InlineGrid, Table, InlineTable, TableRowGroup,
TableHeaderGroup, TableFooterGroup, TableRow, TableCell, TableColumnGroup,
TableColumn, TableCaption`), `Position`, `Float`, `Clear`, `BoxSizing`,
`Overflow`, `Visibility`, `PointerEvents`, `BorderStyle`, `FontStyle`, `LineHeight`,
`TextAlign`, `TextDecorationLine`, `TextTransform`, `WhiteSpace`,
`VerticalAlign`, `ListStyleType`, `ListStylePosition`, `FlexDirection`,
`FlexWrap`, `Alignment`, `BackgroundImage`, `Gradient { repeating, shape:
GradientShape, stops: Vec<ColorStop> }`, `GradientShape { Linear { angle_deg,
corner }, Radial { circle, extent: RadialExtent, center } }`, `ColorStop {
color, position: Option<LengthPercentage> }`, `BackgroundRepeat`,
`BackgroundSize { Auto, Cover, Contain, Explicit(w, h) }`, `BoxShadow {
offset_x, offset_y, blur, spread, color, inset }`, `TransformOp { Translate,
Scale, Rotate(deg), Skew(deg, deg), Matrix([f32; 6]) }`, `ObjectFit`,
`TextOverflow`, `BorderCollapse`, `TableLayout`, `CaptionSide { Top, Bottom }`, `GridTrack { Size(TrackSize),
Repeat(GridRepeat, Arc<[TrackSize]>), LineNames(Arc<[Arc<str>]>) }`,
`TrackSize { Breadth, MinMax, FitContent }`, `TrackBreadth { Length, Fr,
Auto, MinContent, MaxContent }`, `GridRepeat { Count(u16), AutoFill, AutoFit
}`, `GridArea { name, row_start, row_end, column_start, column_end }` (1-based
lines, end exclusive), `GridLine { Auto, Line(i16), Span(u16), Named(Arc<str>)
}`, `GridAutoFlow`, `Content`, `ClipPath { Inset { inset: Edges<LP>, radius:
Corners<LP> }, Circle { radius: ShapeRadius, center }, Ellipse { radius_x,
radius_y, center }, Polygon { even_odd, points: Arc<[(LP, LP)]> } }`,
`ShapeRadius { Length(LP), ClosestSide, FarthestSide, ClosestCorner,
FarthestCorner }`, `FontFeature { tag: [u8; 4], value: u32 }`,
`FilterFunction { Blur(std dev px), Brightness, Contrast, Saturate (factor),
Grayscale, Invert, Sepia, Opacity (0..=1), HueRotate(deg), DropShadow {
offset_x, offset_y, blur (std dev px), color } }` (`is_identity()`),
`BlendMode { Normal, Multiply, .., Luminosity, PlusLighter }` (the
`mix-blend-mode` keywords), `Isolation { Auto, Isolate }`.

Selector queries for huginn-js (`src/query.rs`, on the same matcher as the cascade).
The selector is matched against the whole document; results are in tree order;
`scope` itself is never returned; selectors with a pseudo-element never match:

```rust
pub fn query_selector_all(doc: &Document, scope: NodeId, selectors: &str) -> Result<Vec<NodeId>, SelectorError>;
pub fn query_selector(doc: &Document, scope: NodeId, selectors: &str) -> Result<Option<NodeId>, SelectorError>;
pub fn matches_selector(doc: &Document, element: NodeId, selectors: &str) -> Result<bool, SelectorError>;
pub fn closest(doc: &Document, element: NodeId, selectors: &str) -> Result<Option<NodeId>, SelectorError>;
pub struct SelectorError;                     // invalid selector list (DOM SyntaxError)
```

Invariants:
- Lengths are computed to px (em, rem, vw, vh resolved). Percentages stay
  percentages. Colors are final (`currentcolor` resolved).
- Blockification is done: children of flex/grid containers, floats,
  absolutely positioned boxes and the root have block-level `display`.
- Text nodes have no style. Use the parent element's style.


## huginn-layout

Box tree and layout. Owner: huginn-layout.

### Today

```rust
pub struct Size { pub width: f32, pub height: f32 }
pub struct Rect { pub x: f32, pub y: f32, pub width: f32, pub height: f32 }
impl Rect { pub fn right(&self) -> f32; pub fn bottom(&self) -> f32;
            pub fn inset(&self, e: Edges<f32>) -> Rect; pub fn intersects(&self, other: &Rect) -> bool; }

pub struct LayoutInput<'a> {
    pub document: &'a Document, pub styles: &'a StyleMap, pub images: &'a ImageStore,
    pub base_url: &'a Url, pub viewport: Size,
}

pub struct LayoutEngine { .. }               // owns the lazy font collection + parley LayoutContext
impl LayoutEngine {
    /// Cheap: system fonts load lazily, by family, from an on-disk index
    /// (~/Library/Caches/Huginn/fonts-v1.idx, or $HUGINN_CACHE_DIR).
    pub fn new() -> LayoutEngine;
    /// TrueType/OpenType bytes only. Returns the number of faces added.
    /// Call before layout(); the family name wins over system fonts.
    pub fn register_font(&mut self, data: Vec<u8>, family: &str, weight: f32, style: huginn_style::FontStyle) -> usize;
    pub fn layout(&mut self, input: &LayoutInput<'_>) -> LayoutTree;
    /// Web font families still loading in their font-display block period:
    /// text whose first available family is one of them keeps the fallback's
    /// geometry and gets alpha 0 (runs and decorations). True if the set
    /// changed (the next layout shapes all text again).
    pub fn set_invisible_families(&mut self, families: Vec<String>) -> bool;
    /// Also build `LayoutTree::text_index` (off by default; the window turns it on).
    pub fn set_text_index(&mut self, on: bool);
    /// Keep the shaped text of each pass for the next one (off by default; the
    /// window turns it on). The result of `layout` is the same with it on or off.
    pub fn set_shape_cache(&mut self, on: bool);
    /// Keep the box tree of each pass for the next one (off by default; the
    /// window turns it on). The result of `layout` is the same with it on or
    /// off. An engine with this on must lay out one document only.
    pub fn set_keep_tree(&mut self, on: bool);
}

pub struct BoxId(pub u32);                   // index into LayoutTree::boxes
pub enum BoxKind {
    Block, Flex, Grid,
    Inline,                                  // holds an inline formatting context
    Image(DecodedImage),
    Table, TableRow, TableCell,              // row groups and captions are Block
}
pub struct LayoutBox {
    pub node: Option<NodeId>,                // None for anonymous boxes and block-level pseudo-elements
    pub style: Arc<ComputedStyle>,
    pub kind: BoxKind,
    pub rect: Rect,                          // border box, absolute
    pub border: Edges<f32>, pub padding: Edges<f32>,   // used values (border-collapse may drop sides)
    pub scroll_size: Size,                   // scrolling area from the padding box origin (scrollWidth/Height)
    pub scroll_offset: Option<(f32, f32)>,   // (left, top) of a scroll container, clamped; None otherwise (and for viewport overflow)
    pub background_image: Option<DecodedImage>,
    pub mask_image: Option<DecodedImage>,    // decoded mask-image URL, if it loaded
    pub parent: Option<BoxId>, pub first_child: Option<BoxId>, pub next_sibling: Option<BoxId>,
    pub text: Range<u32>,                    // into LayoutTree::runs (own text + list marker)
    pub decorations: Range<u32>,             // into LayoutTree::decorations
    pub inline_fragments: Range<u32>,        // into LayoutTree::fragments (IFC boxes only)
}
impl LayoutBox { pub fn padding_rect(&self) -> Rect; pub fn content_rect(&self) -> Rect; }

pub struct TextRun {
    pub font: FontData,                      // parley::FontData: Arc'd blob + face index
    pub font_size: f32, pub color: Color,
    pub glyphs: Range<u32>, pub coords: Range<u32>,
    pub skew: Option<f32>, pub embolden: bool,
    pub baseline: f32,                       // absolute y
    pub node: Option<NodeId>,                // innermost inline element, else the IFC owner
}
/// One line's piece of an inline element (or of an inline ::before/::after,
/// with the owning element as node).
pub struct InlineFragment {
    pub node: NodeId, pub style: Arc<ComputedStyle>,
    pub rect: Rect,                          // border box of this piece, absolute
    pub first: bool, pub last: bool,         // holds the element's start / end (box-decoration-break: slice)
}
pub struct PositionedGlyph { pub id: u32, pub x: f32, pub y: f32 }   // absolute
pub struct DecorationLine { pub rect: Rect, pub color: Color }

pub struct LayoutTree {
    pub boxes: Vec<LayoutBox>, pub runs: Vec<TextRun>, pub glyphs: Vec<PositionedGlyph>,
    pub coords: Vec<i16>, pub decorations: Vec<DecorationLine>,
    pub fragments: Vec<InlineFragment>,      // tree order of the elements, then line order
    pub root: Option<BoxId>,                 // the <html> box
    pub viewport: Size,
    pub document_size: Size,                 // scrollable size, at least the viewport
    pub text_index: Option<TextIndex>,       // only with set_text_index(true)
}
/// Rendered text of the runs, for selection, find and copy (window only).
pub struct TextIndex {
    pub text: String,                        // runs' text in run order; '\n' between IFCs and for <br>;
                                             // ' ' for a collapsed space no run shows
    pub runs: Vec<RunText>,                  // parallel to LayoutTree::runs
    pub clusters: Vec<TextCluster>,
}
impl TextIndex { pub fn clusters(&self, run: usize) -> &[TextCluster]; pub fn cluster_end(&self, run: usize, k: usize) -> u32; }
pub struct RunText { pub text: Range<u32>, pub clusters: Range<u32>, pub top: f32, pub bottom: f32 }
                                             // text empty for list markers; top/bottom: line box, absolute
pub struct TextCluster { pub text: u32, pub x: f32, pub width: f32 }   // grapheme: start byte, left edge, advance
impl LayoutTree {
    pub fn get(&self, id: BoxId) -> &LayoutBox;
    pub fn children(&self, id: BoxId) -> impl Iterator<Item = BoxId> + '_;   // tree order
    pub fn text_runs(&self, id: BoxId) -> &[TextRun];
    pub fn glyphs(&self, run: &TextRun) -> &[PositionedGlyph];
    pub fn coords(&self, run: &TextRun) -> &[i16];
    pub fn decorations(&self, id: BoxId) -> &[DecorationLine];
    pub fn box_for_node(&self, node: NodeId) -> Option<BoxId>;              // first box, O(1)
    /// O(1) node -> boxes (tree order), for getBoundingClientRect and offset* in huginn-js.
    pub fn boxes_for_node(&self, node: NodeId) -> &[BoxId];
    /// O(1) node -> inline fragments (line order); inline elements have no box.
    pub fn fragments_for_node(&self, node: NodeId) -> impl Iterator<Item = &InlineFragment> + '_;
}
```

Re-exports: `Color`, `ComputedStyle`, `Edges` (from huginn-style), `FontData`
(from parley).

Invariants:
- Every coordinate is absolute in the document. `position: fixed` boxes are
  placed for scroll position 0 (document = viewport coordinates). The
  content of a scroll container (`overflow` hidden/auto/scroll, not the
  root or a `<body>` whose overflow goes to the viewport) is shifted by
  `Document::scroll_offset`, clamped to the scrolling area. `sticky` boxes
  are offset to stay in their nearest scrollport (the viewport at scroll 0
  at the top), within their containing block.
- A box's `text` range is one contiguous range. For list items, the marker
  runs are in the same range as the item's own text.
- Boxes are in tree order. A child's index is greater than its parent's.
  Absolutely positioned boxes stay under their DOM parent in the box tree,
  but are placed against their containing block.
- Table cells: the cell box fills its row(s); its content is shifted for
  `vertical-align: middle/bottom`.

### To add (huginn-layout owner)

Nothing promised. Planned: line-by-line float avoidance in inline content,
`grid-template-*` once huginn-style computes it.

## huginn-paint

Display list and rasterizer. Owner: huginn-paint.

### Today

```rust
pub enum DisplayItem<'a> {
    Rect { rect: Rect, radii: [f32; 4], color: Color },     // radii: tl, tr, br, bl
    Border { rect: Rect, widths: Edges<f32>, colors: Edges<Color>, styles: Edges<BorderStyle>, radii: [f32; 4] },
                                                            // also used for `outline`
    Glyphs { run: &'a TextRun, glyphs: &'a [PositionedGlyph], coords: &'a [i16] },
    Image { rect: Rect, image: &'a DecodedImage },          // pixmap scaled into rect
    TiledImage { clip: Rect, origin: Rect, image: &'a DecodedImage, repeat: BackgroundRepeat },
                                                            // origin = one tile (position and size)
    Gradient { clip: Rect, gradient: ResolvedGradient, stops: Box<[(f32, Color)]>, repeating: bool },
    BoxShadow { rect: Rect, radii: [f32; 4], offset: (f32, f32), blur: f32, spread: f32, color: Color, inset: bool },
                                                            // outer: rect = border box; inset: rect = padding box
    PushClip { rect: Rect, radii: [f32; 4] }, PopClip,
    PushClipPath { path: BezPath, even_odd: bool },         // clip-path circle/ellipse/polygon; inset() is a
                                                            // PushClip; both end with PopClip
    PushOpacity(f32), PopOpacity,
    PushMask { clip: Rect, tile: Rect, repeat: BackgroundRepeat, source: MaskSource<'a> }, PopMask,
                                                            // alpha of an image or gradient tile, clipped to clip
    PushTransform([f32; 6]), PopTransform,                  // affine [a b c d e f], document coordinates
    PushFilter(&'a [FilterFunction]), PopFilter,            // `filter`: the group, filtered left to right
    PushBlend(BlendMode), PopBlend,                         // `mix-blend-mode`; Normal = an isolated group
    BackdropFilter { rect: Rect, radii: [f32; 4], filters: &'a [FilterFunction], own_layers: u8 },
                                                            // `backdrop-filter`: what is below, back to the
                                                            // nearest enclosing layer that is not one of the
                                                            // element's `own_layers`, clipped to rect
}
pub enum ResolvedGradient {
    Linear { start: (f32, f32), end: (f32, f32) },
    Radial { center: (f32, f32), start_radius: f32, end_radius: f32, y_scale: f32 },
}
pub struct DisplayList<'a> {
    pub items: Vec<DisplayItem<'a>>,
    pub canvas: Color,                       // root or body background, white if transparent
    pub page_size: Size,
}
/// Paint order: CSS 2.1 appendix E. A stacking context (the root; positioned
/// or flex/grid items with a z-index; opacity < 1; transform; mask;
/// clip-path; filter; backdrop-filter; mix-blend-mode; isolation: isolate;
/// fixed/sticky)
/// paints its background and border, child contexts with negative z-index,
/// in-flow block backgrounds, floats, inline content, then positioned
/// descendants and child contexts with z-index >= 0 (by z-index, then tree
/// order). Floats, atomic inlines and z-index:auto positioned boxes paint as
/// pseudo stacking contexts. Overflow clips follow the containing-block
/// chain (abspos escapes clips of non-positioned ancestors, fixed escapes all).
/// `cull` drops boxes outside the region (pass the viewport for screenshots).
/// Around a stacking context's content, outside in: transform, clip-path,
/// blend (or an isolation group, if a child context blends), opacity, mask,
/// the backdrop filter, the filter. A filter list of identity functions
/// (`blur(0)`) adds no item.
pub fn build_display_list<'a>(tree: &'a LayoutTree, cull: Option<Rect>) -> DisplayList<'a>;

pub struct RgbaImage { pub width: u32, pub height: u32, pub data: Vec<u8> }  // straight alpha, row-major
pub trait Rasterizer {
    /// Output size = region size x scale.
    fn rasterize(&mut self, list: &DisplayList<'_>, region: Rect, scale: f32) -> RgbaImage;
}
pub struct CpuRasterizer { .. }              // vello_cpu
impl CpuRasterizer { pub fn new() -> CpuRasterizer; }
pub fn encode_png(img: &RgbaImage, out: impl Write) -> Result<(), png::EncodingError>;
```

Invariants:
- Display items borrow from the `LayoutTree`; the list never outlives it.
  Gradient stops are the only owned data (a small boxed slice per item).
- Push/Pop items are balanced and nested.
- A GPU rasterizer for `huginn open` implements the same `Rasterizer` trait.

### To add (huginn-paint owner)

- Inline fragments (`LayoutTree::fragments`, once on main) paint as `Rect`
  and `Border` items in the inline-content phase, sliced by `first`/`last`.
- `Svg` (vector paths from huginn-net's SVG decoder), `text-shadow`.

## huginn-net

Fetching and resource decoding. Owner: huginn-net.

### Today

```rust
pub use url::Url;
pub use vello_common::pixmap::Pixmap;
pub enum ResourceKind { Document, Stylesheet, Script, Image, Font, Other }
pub struct Resource {
    pub url: Url,                            // final URL after redirects; use as base
    pub status: u16,                         // 200 for file: and data:
    pub content_type: Option<String>,        // lowercase, no parameters
    pub charset: Option<String>,             // Content-Type charset parameter, as written
    pub headers: Vec<(String, String)>,      // lowercase names, no Set-Cookie
    pub response_type: ResponseType,         // Basic (same origin), Cors, Opaque, OpaqueRedirect
    pub redirected: bool,
    pub body: Vec<u8>,
}
impl Resource {
    pub fn text(&self) -> Cow<'_, str>;       // CSS/JS: BOM, charset param, @charset, else UTF-8 (lossy)
    pub fn html_text(&self) -> Cow<'_, str>;  // HTML: BOM, charset param, <meta charset> prescan, else UTF-8 or windows-1252
    pub fn header(&self, name: &str) -> Option<&str>;  // first value, lowercase name
}
pub fn decode_html(bytes: &[u8], transport_charset: Option<&str>) -> Cow<'_, str>;  // UTF-8 is borrowed
pub fn decode_text(bytes: &[u8], transport_charset: Option<&str>) -> Cow<'_, str>;
pub enum NetError { InvalidUrl(String), UnsupportedScheme(String), Io(String),
                    Http { url: String, status: u16 }, Transport(String), Decode(String),
                    Blocked(String) }      // same-origin policy or CORS refused it

// Fetch standard request options.
pub enum Mode { Navigate, SameOrigin, NoCors, Cors }
pub enum Credentials { Omit, SameOrigin, Include }
pub enum RedirectMode { Follow, Error, Manual }
pub enum CorsSettings { NoCors, Anonymous, UseCredentials }   // `crossorigin`; from_attr(Option<&str>)
/// The document a loader fetches for (origin, site for cookies, referrer),
/// or the page that started a navigation.
pub struct Client { pub url: Url, pub referrer_policy: ReferrerPolicy }
pub struct ScriptRequest { pub url: Url, pub method: String, pub headers: Vec<(String, String)>,
                           pub body: Option<Vec<u8>>, pub mode: Mode, pub credentials: Credentials,
                           pub redirect: RedirectMode }

/// What a profile's tabs share (Chrome's network context): connections (HTTP/2 by
/// ALPN, one per host; else HTTP/1.1, 6 per host; idle 5 min; TLS sessions), resolved
/// hosts (1 min), HTTP cache, cookie jar. Its thread (`huginn-net`) starts on the first
/// HTTP request.
pub struct Network { .. }                    // Sync; one per profile, in an Arc
impl Network {
    pub fn new(cookies: Arc<CookieJar>, cache: HttpCache) -> Arc<Network>;  // HUGINN_NET_LOOPBACK_ONLY=1 -> loopback only
    pub fn cookies(&self) -> &Arc<CookieJar>;
    pub fn cache(&self) -> &HttpCache;
    /// Warm DNS + TCP + TLS (+ HTTP/2 handshake) to url's origin, nothing sent (Chrome's
    /// omnibox preconnect). Returns at once (short thread); no-op if the origin has a
    /// connection or one is opening, and for non-http(s) URLs. Unused HTTP/1.1 ones live 10 s.
    pub fn preconnect(&self, url: &Url);
}
/// RFC 9111 private cache: fresh entries answer, stale ones are revalidated (ETag,
/// Last-Modified, 304), no-store and Vary honoured, keyed by top-level site + URL.
pub struct HttpCache { .. }
impl HttpCache {
    pub fn off() -> HttpCache;                               // huginn render: always the network
    pub fn in_memory(max_bytes: u64) -> HttpCache;           // private windows
    pub fn on_disk(dir: impl Into<PathBuf>, max_bytes: u64) -> HttpCache;  // one file per entry, LRU, safe across tabs and processes
    pub fn clear(&self);
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
/// How a load uses the HTTP cache (Chrome's load types): Normal; Reload (document
/// revalidated, subresources as usual); HardReload (nothing from the cache);
/// History (back/forward, discarded tabs: the document even when stale).
pub enum LoadKind { Normal, Reload, HardReload, History }

pub struct Loader { .. }                     // Sync; one per page
impl Loader {
    pub fn new() -> Loader;                  // own Network: in-memory cookie jar, no HTTP cache; 30 s global timeout, 64 MiB body limit; HUGINN_NET_LOOPBACK_ONLY=1 -> loopback_only()
    pub fn with_network(net: Arc<Network>) -> Loader;  // the profile's network, shared by its tabs
    pub fn loopback_only() -> Loader;        // hosts other than 127.0.0.1/localhost/::1 fail as host-not-found (bench: offline like Chrome/Ladybird)
    pub fn network(&self) -> &Arc<Network>;
    pub fn cookies(&self) -> &Arc<CookieJar>;
    pub fn set_load_kind(&self, kind: LoadKind);       // for the next navigation and its subresources; Normal again when done
    pub fn set_client(&self, client: Option<Client>);   // the document, once its final URL is known
    pub fn client(&self) -> Option<Client>;
    /// fetch()/async XHR (sent from other threads) wait while a guard lives: the window holds
    /// them while it loads the first paint's resources. Sync XHR is not held.
    pub fn hold_script_requests(&self) -> ScriptRequestsHeld<'_>;
    pub fn wait_for_script_turn(&self);      // blocks while held; the threads of fetch()/async XHR call it
    pub fn preconnect(&self, url: &Url);     // Network::preconnect for the client page (public pages: public addresses only)
    pub fn fetch(&self, url: &Url, kind: ResourceKind) -> Result<Arc<Resource>, NetError>;  // no-cors subresource, kept in the loader by URL
    pub fn fetch_with(&self, url: &Url, kind: ResourceKind, cors: CorsSettings) -> Result<Arc<Resource>, NetError>;  // modules, <script crossorigin>
    /// Not kept in the loader (the HTTP cache still answers): for bodies decoded once and
    /// dropped (images, fonts). Document = a browser-started navigation.
    pub fn fetch_uncached(&self, url: &Url, kind: ResourceKind) -> Result<Resource, NetError>;
    /// fetch_uncached only if nothing has to be sent: data:/file: URLs and fresh HTTP cache
    /// entries (a hard reload has none); None otherwise. The window loads these before its
    /// first paint.
    pub fn fetch_cached(&self, url: &Url, kind: ResourceKind) -> Option<Resource>;
    /// Top-level GET or form POST (content type, body); initiator = the page that started it.
    /// Returns the document and its referrer (document.referrer).
    pub fn navigate(&self, url: &Url, post: Option<(&str, &[u8])>, initiator: Option<&Client>)
        -> Result<(Resource, Option<String>), NetError>;
    /// fetch()/XHR: any method, CORS (preflight, Access-Control-* checks) for other origins,
    /// any status; opaque responses come back empty, CORS ones with exposed headers only.
    /// A conditional header (If-None-Match, ...) bypasses the HTTP cache (Fetch).
    pub fn fetch_for_script(&self, req: ScriptRequest) -> Result<Resource, NetError>;
    pub fn fetch_all(&self, requests: &[(Url, ResourceKind)]) -> Vec<Result<Arc<Resource>, NetError>>;  // <= 32 parallel, input order
    pub fn map_parallel<T: Sync, R: Send>(&self, items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R>;  // <= 32 threads
    pub fn clear_cache(&self);               // drop the bodies kept in the loader (after CSS parsing); the HTTP cache stays
}
pub struct ScriptRequestsHeld<'a> { .. }    // Loader::hold_script_requests; the requests go when it drops
/// HUGINN_NET_LOG=1: print `huginn: load: +<ms> <what>` on the net log's clock (the shell
/// marks the phases of a page load with it).
pub fn log_phase(what: &str);

/// RFC 6265bis cookies: Domain (no public suffixes), Path, Expires/Max-Age
/// (<= 400 days), Secure (https and loopback), HttpOnly, SameSite (Lax when
/// absent; None needs Secure), __Secure-/__Host- prefixes.
pub struct CookieJar { .. }                  // Sync
impl CookieJar {
    pub fn in_memory() -> CookieJar;         // private windows, `huginn render`
    pub fn persistent(path: PathBuf) -> CookieJar;   // loads; save() writes cookies with an expiry (0600)
    pub fn save(&self) -> io::Result<()>;    // if changed
    pub fn save_unless_saved_within(&self, interval: Duration) -> io::Result<()>;
    pub fn clear(&self);
    pub fn len(&self) -> usize; pub fn is_empty(&self) -> bool;
    pub fn header_for(&self, url: &Url, cx: CookieContext) -> Option<String>;
    pub fn set_from_header(&self, url: &Url, set_cookie: &str, cx: CookieContext);
    pub fn document_cookie(&self, url: &Url, top_level: &Url) -> String;   // no HttpOnly; a cross-site frame sees its partition
    pub fn set_document_cookie(&self, url: &Url, top_level: &Url, value: &str);   // cannot set or replace HttpOnly
}
pub struct CookieContext<'a> { pub allows: SameSite, pub top_level: bool,
                               pub partition: Option<&'a str> }   // third party: top-level site_key; SAME_SITE const
pub enum SameSite { None, Lax, Strict }
pub fn registrable_domain(host: &str) -> &str;   // eTLD+1 (Public Suffix List)
pub fn same_site(a: &Url, b: &Url) -> bool;      // schemeful
pub fn site_key(url: &Url) -> Option<String>;    // "scheme://registrable-domain"; None for opaque origins
pub enum ReferrerPolicy { .., StrictOriginWhenCrossOrigin /* default */, .. }   // parse_token, parse_header
pub fn referrer_for(source: &Url, policy: ReferrerPolicy, target: &Url) -> Option<String>;
pub fn parse_target(target: &str) -> Result<Url, NetError>;    // URL or file path
pub fn resolve_url(base: &Url, href: &str) -> Option<Url>;

pub struct DecodedImage { .. }               // Clone = Arc clone
impl DecodedImage {
    pub fn new(pixmap: Pixmap, natural: (u32, u32)) -> DecodedImage;
    pub fn with_natural_size(&self, natural: (u32, u32)) -> DecodedImage;   // same pixels (srcset density)
    pub fn width(&self) -> u32; pub fn height(&self) -> u32;               // natural size, CSS px
    pub fn pixmap(&self) -> &Arc<Pixmap>;                                   // may be smaller or bigger: scale by its size
}
/// PNG JPEG GIF WebP BMP ICO, AVIF (feature `avif`, default on), and SVG (sniffed). `display` = drawn size in
/// device px if known (0 = unknown side): rasters >= 2x that are downscaled,
/// SVGs are rendered at it. At most 6 decodes run at once in the process (others wait).
pub fn decode_image(bytes: &[u8], display: Option<(u32, u32)>) -> Result<DecodedImage, NetError>;
pub fn decode_svg(bytes: &[u8], size_hint: Option<(u32, u32)>) -> Result<DecodedImage, NetError>;  // resvg, no text
pub fn is_svg(bytes: &[u8]) -> bool;
/// WOFF and WOFF2 to TrueType/OpenType bytes; sfnt passes through without a copy.
pub fn decode_font(bytes: Vec<u8>) -> Result<Vec<u8>, NetError>;
pub fn font_format(bytes: &[u8]) -> Option<FontFormat>;               // Sfnt, Woff, Woff2
pub struct ImageStore { .. }                 // absolute URL -> image, and element (NodeId::to_raw) -> image
impl ImageStore { pub fn new() -> ImageStore; pub fn insert(&mut self, url: &Url, image: DecodedImage);
                  pub fn get(&self, absolute_url: &str) -> Option<&DecodedImage>;
                  pub fn insert_for_node(&mut self, node: u32, image: DecodedImage);   // srcset choice, inline <svg>, <video poster>
                  pub fn get_for_node(&self, node: u32) -> Option<&DecodedImage>;
                  pub fn len(&self) -> usize; pub fn is_empty(&self) -> bool; }

// Responsive images and media-dependent resources (not @media rules: huginn-style).
pub struct Screen { pub width: f32, pub height: f32, pub device_pixel_ratio: f32 }
pub fn media_matches(list: &str, screen: &Screen) -> bool;
pub fn parse_length(s: &str, screen: &Screen) -> Option<f32>;      // px, em, rem, vw, vh, calc/min/max/clamp
pub fn parse_srcset(s: &str) -> Vec<SrcsetCandidate<'_>>;         // { url, descriptor: Width(f32) | Density(f32) }
pub fn eval_sizes(sizes: &str, screen: &Screen) -> Option<f32>;   // slot width, CSS px
pub fn choose_candidate<'a>(c: &[SrcsetCandidate<'a>], slot_width: f32, dpr: f32) -> Option<&'a str>;

// What Huginn says it is: one source for headers and `navigator` (huginn-js).
pub mod ua {
    pub const USER_AGENT: &str;              // "Mozilla/5.0 (<Chrome's platform>) AppleWebKit/537.36 (KHTML, like Gecko) Huginn/<major.minor> Safari/537.36"; no Chrome/ token
    pub const CHROME_MAJOR: &str;            // Chromium brand version: Chrome release of the embedded V8
    pub const SEC_CH_UA: &str;               // brands: Chromium, Huginn, GREASE
    pub const HUGINN_FULL_VERSION: &str;
    pub fn client_hint_headers(url: &Url) -> &'static [(&'static str, &'static str)];  // Sec-CH-UA*; https and loopback only
}
```

Invariants:
- Non-2xx HTTP responses are `NetError::Http`, except from `fetch_for_script`.
- Every http(s) request sends `ua::USER_AGENT` and `ua::client_hint_headers`
  (on every redirect hop and preflight). The image `Accept` header lists only
  formats `decode_image` decodes.
- Redirects are followed by the loader, one hop at a time (at most 20):
  each hop gets its own `Cookie`, `Referer`, `Origin` and `Sec-Fetch-*`
  headers and stores its `Set-Cookie` headers; for CORS each hop must pass
  the CORS check. Requests to other origins in `cors` mode send `Origin`;
  non-simple ones are preflighted (`OPTIONS`, no cookies) first.
- Cookies go out only with credentials (`Include`, or `SameOrigin` to the
  client's origin), only if `SameSite` allows: same-site requests carry
  all, cross-site top-level `GET` navigations `Lax` and `None`, other
  cross-site requests `None` only. Scripts never see `Set-Cookie`.
- Third-party requests (not navigations, to another site than the client)
  read and write the client's cookie partition only, never the third
  party's first-party cookies. Navigations started by a page (same tab,
  new tab, new window, forms) carry the page as initiator; only the
  browser UI navigates without one, and a cross-site redirect makes the
  rest of such a chain cross-site.
- Requests from a page on a public address, other than top-level
  navigations, never connect to loopback or private addresses (checked on
  the resolved addresses). `localhost` and `*.localhost` resolve to
  loopback only.
- Pixmaps are premultiplied RGBA8 (vello's storage), at most 65535 px per
  side. `DecodedImage::width()/height()` are the natural size in CSS px; the
  pixmap size can differ, so layout uses `width()` and paint scales by
  `pixmap().width()`.
- Layout looks up an `<img>` by element first (`get_for_node`), then by the
  resolved `src` URL. Inline `<svg>` and the `<video>` poster are looked up
  by element only; a video's natural size is its poster's (300x150 without one).

The shell's `huginn-shell/src/resources.rs` (owned by huginn-net) decides which
subresources a page needs: the `<img>` choice (`srcset`, `sizes`,
`<picture>`), `<video poster>` images, `loading=lazy` images only when layout puts them in view,
`display:none` images skipped, inline `<svg>` serialized and rasterized at
its CSS size, and the `@font-face` faces CSS font matching picks for the
page's text (style, weight, `unicode-range`).

## huginn-js

Script engine. Owner: huginn-js. V8 (the `v8` crate) is behind the cargo feature
`v8` (`huginn-js/v8`, passed through as `huginn-shell/v8`, which is a default feature
of huginn-shell).

### Today

```rust
pub enum ScriptKind { Classic, Module, ImportMap }  // ImportMap: `<script type="importmap">`, JSON that maps module specifiers
pub enum ScriptTiming { Blocking, Defer, Async }
pub struct ScriptRef { pub node: NodeId, pub kind: ScriptKind, pub timing: ScriptTiming, pub src: Option<Url>,
                       pub cors: CorsSettings }   // crossorigin; modules default to Anonymous
pub fn script_cors(el: &ElementData, kind: ScriptKind) -> CorsSettings;
/// Scripts the HTML spec would run, in document order (skips data blocks and nomodule).
pub fn collect_scripts(doc: &Document, base: &Url) -> Vec<ScriptRef>;
pub fn script_kind(el: &ElementData) -> Option<ScriptKind>;   // None: not a runnable script
pub fn inline_source(doc: &Document, script: NodeId) -> String;

/// Built by the shell for each call. The engine never owns the document.
pub struct HostContext<'a> {
    pub document: &'a mut Document,
    pub loader: &'a Arc<Loader>,             // shared: fetch() and async XHR run on other threads
    pub document_url: &'a Url,               // document.URL, location
    pub referrer: &'a str,                   // document.referrer
    pub base_url: &'a Url,                   // <base href> or the document URL
    pub viewport: (f32, f32),                // innerWidth, innerHeight
    pub now_ms: f64,                         // virtual ms since navigation start
    pub layout: Option<&'a mut dyn LayoutHost>,  // geometry on demand; None = zeros
    pub current_script: Option<NodeId>,      // document.currentScript during execute
}
/// Style and layout for scripts, implemented by the shell. It may return
/// stale results to bound the cost of layout thrashing.
pub trait LayoutHost {
    fn geometry(&mut self, doc: &Document, node: NodeId) -> Option<NodeGeometry>;
    fn computed_value(&mut self, doc: &Document, node: NodeId, property: &str) -> String;
}
pub struct NodeGeometry {
    pub x: f32, pub y: f32, pub width: f32, pub height: f32, pub border: [f32; 4], /* l t r b */
    pub scroll_size: [f32; 2],               // scrollWidth, scrollHeight (unrounded)
}
// Element scrolling: scrollTop/scrollLeft/scroll()/scrollTo()/scrollBy() clamp
// against the geometry and store the offset in the Document
// (`set_scroll_offset`, which bumps `scroll_generation`, not `generation`).
pub struct Event { pub kind: String, pub bubbles: bool, pub cancelable: bool }
impl Event { pub fn new(kind: &str, bubbles: bool, cancelable: bool) -> Event; }
pub enum EventTarget { Window, Document, Node(NodeId) }
pub enum ScriptError { Disabled, Compile { url: String, message: String }, Runtime { url: String, message: String } }

pub trait ScriptEngine {
    fn execute(&mut self, cx: &mut HostContext<'_>, source: &str, url: &str, kind: ScriptKind) -> Result<(), ScriptError>;
    /// Capture and bubble. Returns false if a listener called preventDefault().
    fn dispatch_event(&mut self, cx: &mut HostContext<'_>, target: EventTarget, event: &Event) -> Result<bool, ScriptError>;
    fn run_microtasks(&mut self, cx: &mut HostContext<'_>) -> Result<(), ScriptError>;
    /// Run timers (setTimeout, setInterval, requestAnimationFrame, message
    /// tasks) and script-inserted external scripts due at or before cx.now_ms,
    /// and deliver arrived responses to fetch()/async XHR (one task each, in
    /// arrival order, microtasks after each).
    fn run_timers(&mut self, cx: &mut HostContext<'_>) -> Result<usize, ScriptError>;
    fn next_timer_due(&self) -> Option<f64>;  // an arrived response is due now
}
pub struct ScriptingDisabled;                // runs nothing
pub struct EngineOptions { pub call_budget_ms: u64, pub console: bool }   // Default: 2000 ms, HUGINN_JS_CONSOLE
/// V8 with the `v8` feature, else None. Cheap: the isolate is created on the first `execute`.
pub fn create_engine(options: EngineOptions) -> Option<Box<dyn ScriptEngine>>;
// Event also has `interface: &'static str` ("MouseEvent", "KeyboardEvent", ...),
// `init: Vec<(&'static str, EventValue)>` (Bool, Num, Str, Node) and `trusted`;
// Event::ui(kind, interface, init) builds a bubbling, cancelable, composed one.
pub enum HostRequest { Navigate { url: Url, post: Option<forms::PostBody>, new_tab: bool, replace: bool } }
// ScriptEngine, provided methods: take_requests() -> Vec<HostRequest> (location, form.submit(),
// a.click()), heap_bytes() -> usize (0 = no isolate), trim_memory() (full GC),
// requests_in_flight() -> usize (fetch()/async XHR started, response not delivered, aborted
// ones excluded), wait_for_response(&self, timeout: Duration) -> bool (sleep until a response
// arrives; true: run_timers delivers it), set_waker(Waker) (called on the request's thread when
// a response arrives).
pub type Waker = Arc<dyn Fn() + Send + Sync>;
// huginn_js::forms: activation behavior (Activation), form owner, entry list, submission
// (GET query / POST urlencoded, multipart, text/plain), checkedness, select options.
// huginn_js::activation: click / submit / implicit_submit / set_focus over an EventSink;
// EngineSink drives them from the browser, the V8 ops (click(), requestSubmit(), focus()) from scripts.
```

How the bindings reach the DOM: the web platform objects are JavaScript
(`src/js/*.js`, the prelude) over ~60 native operations (`src/v8engine/ops.rs`)
that take raw `NodeId`s and call `huginn_dom::Document` through `cx.document`.
DOM rules (pre-insertion checks, queries, URL parts) are in `src/dom_ops.rs`.

| JS | native side |
|---|---|
| `createElement(NS)`, `createTextNode`, `createComment` | `create_element`, `create_text`, `create_comment` |
| `appendChild`, `insertBefore`, `append`, `before`, ... | `dom_ops::insert` (DOM checks, fragments), then newly connected `<script>`s run |
| `removeChild`, `remove` | `detach` |
| attributes, `classList`, `dataset`, `style` (the `style` attribute) | `set_attribute` / `remove_attribute`; HTML names lowercased |
| `textContent`, `data` | `set_text_content`, `set_character_data` |
| `innerHTML`, `outerHTML`, `insertAdjacentHTML`, `cloneNode` | `huginn_dom::parse_fragment`, `serialize`, `Document::clone_node` |
| `querySelector(All)`, `matches`, `closest` | `huginn_style::query_selector(_all)`, `matches_selector`, `closest` |
| `getBoundingClientRect`, `offset*`, `client*`, `getComputedStyle` | `cx.layout` (`LayoutHost`) |
| `fetch`, async `XMLHttpRequest` | `cx.loader.fetch_for_script` on a thread of the page (`v8engine/fetches.rs`, at most 16 in flight per page, the rest queued; none go out while a `Loader::hold_script_requests` guard lives): Fetch modes and CORS; other origins must allow `cors` requests |
| sync `XMLHttpRequest` | `cx.loader.fetch_for_script` on the engine thread |
| inserted `<script src>`, module imports | `cx.loader.fetch_with`: classic scripts `no-cors` from any origin; modules and `crossorigin` scripts CORS |
| `document.cookie`, `document.referrer` | `cx.loader.cookies()` (no HttpOnly), `cx.referrer` |
| wrapper identity | one JS wrapper per `NodeId::to_raw`, cached |

The prelude runs at build time: `build.rs` runs it once and saves the isolate
as a V8 startup snapshot (`src/v8engine/startup.rs`), and each page's isolate
starts from it. A cross build has no snapshot, and `HUGINN_V8_FLAGS` turns it
off (a snapshot needs the flags it was made with); then each isolate runs the
prelude itself. So the prelude must not call `N` while it runs. The engine
then adds the native operations and constants to `N` and calls the prelude's
`start` hook, which reads the page's own values (`onStart` in `00-base.js`:
the URL, the viewport, the time origin, the user agent).

Event loop, owned by the shell (`huginn-shell/src/scripts.rs`):
1. Parse. Run `Blocking` scripts in order, then `Async`, then `Defer`
   (all fetched up front with the stylesheets). `run_microtasks` after each.
2. `dispatch_event(Document, "DOMContentLoaded")`, then
   `dispatch_event(Window, "load")`, each followed by `run_microtasks`.
3. Settle: jump virtual time to each due timer and `run_timers`, up to
   500 ms of virtual time or 1.5 s of real time. While `requests_in_flight`
   is not zero, virtual time runs at real speed instead: `wait_for_response`
   until the next timer is due, then `run_timers`. After the 500 ms, time
   stops and the loop waits for responses until the 1.5 s are spent.
4. If `document.generation()` changed, pick up new stylesheets and images,
   then style, layout and paint once.
The window (`huginn open`) runs only the `Blocking` scripts in step 1 and
paints; then the `Async` and `Defer` ones (fetched meanwhile), as Chrome
does when they are not loaded at the end of parsing. DOMContentLoaded fires
before the paint unless a `Defer` script is to come, `load` unless any
script is to come. It skips step 3 (its timers run in real time).
No V8 isolate is created for pages without scripts: an isolate costs RSS.
Timer due times (setTimeout, setInterval, rAF one frame later, message and
response tasks) count from the time the engine call started (`cx.now_ms`), not
from `performance.now()`, so their order does not depend on CPU speed.

## huginn-shell

The binary. Owner: integration. It is the only crate that
orders phases. Public surface is the CLI and JSON line under "Rules for all
crates". Internal to the binary: `pipeline::load(&Arc<Loader>, &Url, Viewport, js, Instant)
-> Result<Page, String>` (phases 1-6, scripts included), `Page::register_fonts`, `Page::layout`,
`Page::restyle`, and `pipeline::render(&RenderOptions, Instant)` with
`RenderOptions { target, out, width, height, full_page, js }`. `render` and
the `open` window both call `load`, so pipeline steps belong there.

`huginn open [<url>] [--width W --height H] [--no-js]` opens a window (see
ARCHITECTURE.md). `--commands "<cmds>" [--scale S]` runs it headless; the
command language is documented at the top of `headless.rs` (`wait`, `sleep`,
`scroll`, `swipe <dx> <dy>`, `key <chord>`, `type`, `move`, `click x y [cmd shift alt ctrl
middle right double triple]`, `down`, `up`, `dblclick`, `tripleclick`,
`rclick`, `drag`, `resize`, `window <i>`, `clipboard <text>`, `prefetch`,
`shot <png>`, `state`). `--script "<cmds>"` runs the same commands in the
real window (plus `capture <png>`, a `screencapture` of it) and quits after
the last one. `state` prints one JSON line: the focused window's
active tab (`url`, `title`, `error`, `load_ms` (to the first paint),
`complete_ms` (to the end of the scripts after it, the images and fonts
still on the network at the first paint, the load events and the lazy
images; `null` before), `contentful_ms` (to the first tree that paints
content: visible text, an image, a background image; `null` before),
`loading`, `history`,
`index`, `scroll`, `max_scroll`, `zoom`, `hover`, `strips`, `discarded`,
`icon`, `favicon`, plus the fields `interact::state` and `live::state` add),
the window (`tabs` [{`url`, `title`, `loading`, `discarded`, `active`,
`zoom`}], `active_tab`, `private`, `address`, `focused` (address bar),
`address_selection` ([start, end] in chars, or null), `suggestions`
[{`kind` (`history`, `typed`, `search`), `url`, `title`}], `suggestion` (the
picked row, null when the drop-down is closed), `suggestions_rect` ([x, y,
width, height] in window CSS px, or null), `bookmarked`, `fullscreen`,
`minimized`, `page_top`, `size`, `scale`) and the session (`windows`,
`window`, `visits`, `history_rows` (rows of the saved history), `closed`,
`bookmarks`, `clipboard`, `engines`). Env: `HUGINN_PROFILE_DIR` (bookmarks
and history files; headless saves none without it), `HUGINN_LIVE_TABS`
(background tabs kept in memory, default 8).

### huginn-shell window: tab hooks

Modules: `session.rs` (all windows, shared state), `browser.rs` (one
window: tabs, strip, toolbar, routing), `tab.rs` (one tab: engine thread,
history, page layer, zoom), `tabstrip.rs`, `toolbar.rs`, `omnibox.rs`
(address bar suggestions), `history.rs` (saved browsing history),
`shortcuts.rs`, `internal_pages.rs`, `input.rs`, `ui.rs`, `clipboard.rs`. In-page modules
plug in through fixed functions: `interact/` (in-page interaction) and `live.rs`
(live JavaScript). Their owners change bodies freely; signatures change only
with this file.

```rust
// input.rs
pub struct Modifiers { pub cmd: bool, pub shift: bool, pub alt: bool, pub ctrl: bool }
pub enum Key { Char(String), Enter, Backspace, Delete, Escape, Space, Tab, Up, Down, Left, Right, PageUp, PageDown, Home, End }
pub struct KeyEvent { pub key: Key, pub mods: Modifiers }
pub enum MouseButton { Left, Middle, Right }
pub enum MouseKind { Down, Up, Move }
pub struct MouseEvent { pub kind: MouseKind, pub button: MouseButton, pub pressed: bool,
                        pub clicks: u8 /* 1..=3 */, pub x: f32, pub y: f32 /* window CSS px */, pub mods: Modifiers }
pub enum CursorKind { Default, Pointer, Text }

// tab.rs
pub const PAGE_TOP: f32 = 80.0;          // tab strip 40 + toolbar 40, window CSS px
pub enum Request { Navigate(Url), OpenTab { url: Url, background: bool, opener: Box<Opener> },
                   OpenWindow { url: Url, private: bool, opener: Box<Opener> }, Back, Forward, Reload, ViewSource }
pub struct Opener { pub initiator: Option<Client>, pub post: Option<PostData>,
                    pub typed: bool /* address bar: history counts a typed visit */ }
pub struct View { pub tree: LayoutTree, pub links: Links }
impl Tab {
    pub view: Option<View>; pub scroll: f32; pub zoom: f32; pub title: String;
    pub hover: Option<Url>; pub cursor: Option<CursorKind>;
    pub interact: interact::PageState; pub live: live::TabState;
    pub fn url(&self) -> Option<&Url>;  pub fn can_back(&self) -> bool;  pub fn can_forward(&self) -> bool;
    pub fn to_doc(&self, x: f32, y: f32) -> (f32, f32);      // window CSS -> document CSS
    pub fn doc_to_window(&self, r: Rect) -> Rect;
    pub fn page_size(&self) -> (f32, f32);                   // viewport in document CSS px
    pub fn set_scroll(&mut self, y: f32);  pub fn scroll_by(&mut self, dy: f32);
    pub fn wheel(&mut self, x: f32, y: f32, dy: f32);       // innermost scrollable container under the pointer, else the page
    pub fn set_zoom(&mut self, zoom: f32);
    pub fn redraw(&mut self);                                // overlays changed
    pub fn request(&mut self, r: Request);                   // run by the window after the event
    pub fn run(&mut self, f: engine::Job);                   // job on the tab's engine thread
    pub fn input(&mut self, e: live::PageInput);             // DOM event to the live page
    pub fn link_at(&self, x: f32, y: f32) -> Option<&Url>;
}

// engine.rs (one thread per tab; it ends when the tab drops its EngineTx: closed or discarded)
pub type Job = Box<dyn FnOnce(&mut Page, &mut LayoutEngine, Viewport) -> Option<EngineEvent> + Send>;
pub struct PostData { pub content_type: String, pub body: Vec<u8> }
pub type EngineTx = Arc<Sender<Command>>;   // engine::spawn returns it; responses to scripts' requests hold only a Weak
pub enum Command { Load { seq, generation, url, viewport, post: Option<PostData>, html: Option<String> },
                   Resize { seq, generation, viewport }, Fragment { seq, generation, id },
                   SetVisible { seq, visible }, Run { seq, generation, f: Job },
                   Input { seq, generation, event: live::PageInput },
                   Wake }   // a response to fetch()/XHR arrived (sent by its thread, not by the UI)
pub enum EngineEvent { Loaded(Box<Loaded>), Relaid { generation, tree, links: Option<Links>, title: Option<String> },
                       Fragment { generation, targets }, Navigate { generation, url, post: Option<PostData>, mods: Modifiers },
                       Interact(Box<dyn Any + Send>), Live(live::LiveEvent),
                       Favicon { url, image: Option<DecodedImage> }, Idle { seq } }

// ui.rs
pub(crate) struct Pixels;  fn layout_snippet(..);  fn render_snippet(..) -> (Pixels, Vec<Rect>);
pub(crate) struct Canvas { frame, width, height, scale, clip }   // fill, blend, outline, blit (window CSS px)

// clipboard.rs: get() -> Option<String>, set(&str). Headless keeps it in memory.
```

Order of input in the page area: the window's shortcut table first
(`shortcuts::action`; the page never sees those chords), then the address
bar if focused, then `interact::key_capture` (find bar, menu) ->
`live::key` (scripts, form controls) -> `interact::key` (selection, find,
focus, Cmd+U/S/P) -> tab defaults (scrolling, Cmd+Left/Right, Cmd+Up/Down,
Esc stops loading). Pointer: `interact::mouse_capture` -> `live::mouse`
(every page event; true = it takes the default action) -> `interact::mouse`
-> tab default (press and release on the same link opens it: Cmd or middle
button = background tab, with Shift = foreground tab, Shift = new window).
Drawing: page strips, `interact::draw_page`, `live::draw_page` (clipped to
the page), status bubble, tab strip, toolbar, `live::draw_window`,
`interact::draw_window`. Lifecycle: `on_load` after a page arrives,
`interact::on_relayout` after `Relaid`, `on_engine` for their events. A
discarded tab gets fresh `PageState`/`TabState`.

Engine-side hook points (the live JavaScript code in `engine.rs`): `Current`
(the page on the engine thread), `load()` (build the isolate), the wait in
`run()` (timers and responses to scripts' requests), `live::input(cur, event, visible, notify)`. `live.rs` also exports
`text_control_focused(&Tab)` (interact skips Cmd+A/C then), `focused(&Tab) -> Option<NodeId>` (keys scroll its container), `select_picked(&mut Tab, node,
index)` and `mouse_leave(&mut Tab)`; the engine half is `livepage.rs` (`LivePage` in `Current`).

### huginn-shell window: in-page interaction

`interact/` implements the tab hooks above and exports these for the other
in-page modules. Text positions are byte offsets into the page's
`TextIndex` (huginn-layout builds it because `engine::load` calls
`set_text_index(true)`; `render` never does).

```rust
// interact/hit.rs: the innermost node at a document point: last box that
// contains it, then the innermost inline fragment of its IFC (inline links).
pub fn node_at(tree: &LayoutTree, x: f32, y: f32) -> Option<NodeId>;
// The box painted on top at a point; `pointer-events: none` boxes let it through.
pub fn topmost_box(tree: &LayoutTree, x: f32, y: f32) -> Option<BoxId>;
// interact/focus.rs: sequential focus order (tabindex > 0, then tree order;
// rendered, not disabled/inert). None after the last: focus leaves the page.
pub fn next_focusable(doc: &Document, styles: &StyleMap, from: Option<NodeId>, backward: bool) -> Option<NodeId>;
// interact/text.rs (document CSS px)
pub fn position_at(tree, x, y) -> Option<u32>;  pub fn over_text(tree, x, y) -> bool;
pub fn word_at(text, pos) -> Range<u32>;        pub fn paragraph_at(text, pos) -> Range<u32>;
pub fn range_text(index, range) -> String;      pub fn range_rects(tree, clips: &[Rect], range) -> Vec<Rect>;
pub fn run_clips(tree) -> Vec<Rect>;            pub fn find_all(text, query, limit) -> Vec<Range<u32>>;
// interact/menu.rs: drawn menu (context menu; reusable for <select>)
pub struct MenuItem { pub label: String, pub enabled: bool, pub separator: bool, pub checked: bool }
impl Menu { pub fn new(items, x, y, win_w, win_h) -> Menu; pub fn item_at(&self, x, y) -> Option<usize>;
            pub fn hover(&mut self, x, y) -> bool; pub fn key(&mut self, &Key) -> MenuKey; pub fn html(&self) -> String; pub fn rect(&self) -> Rect; }
// interact/print.rs: Letter, 0.4 in margins, pages broken above cut lines, 2x raster, Flate images
pub fn print_pdf(page: &mut Page, layout: &mut LayoutEngine, screen: Viewport, title: &str) -> (Vec<u8>, usize);
pub fn downloads_dir() -> PathBuf;               // $HUGINN_DOWNLOADS, else ~/Downloads
// interact/source.rs: view-source page (engine thread), save page (fetches the bytes again)
pub fn view_source_target(url: &Url) -> Option<Url>;
pub fn view_source_html(loader: &Loader, target: &Url) -> Result<String, String>;
pub fn save_page(url: &Url, title: &str) -> Result<PathBuf, String>;
```

Headless `state` fields added per tab: `selection` (text or null), `find`
(`open`, `focused`, `query`, `count`, `current` 1-based, `rect` of the
active match in window CSS px), `menu` (`items`, disabled ones in
parentheses, `rect`, `hovered`), `notice` (the bubble text: saved file,
print result).
