//! huginn-style: CSS cascade and computed style.
//!
//! Public contract (stable for huginn-layout and huginn-paint):
//! - [`StyleEngine`]: holds stylesheets (UA + author), computes styles.
//! - [`StyleMap`]: computed style per DOM node, indexed by `NodeId`.
//! - [`ComputedStyle`] and the value types in [`values`].
//!
//! The cascade is Stylo's (Servo's CSS engine), run sequentially on our
//! arena DOM; see ARCHITECTURE.md, "The Stylo decision". `convert.rs` turns
//! Stylo's computed values into [`ComputedStyle`].

mod convert;
mod dom;
mod engine;
mod hints;
mod query;
pub mod values;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

use huginn_dom::{Document, NodeId};
pub use query::{SelectorError, closest, matches_selector, query_selector, query_selector_all};
use style::shared_lock::{SharedRwLock, StylesheetGuards};
use style::stylesheets::{DocumentStyleSheet, UrlExtraData};
use style::stylist::Stylist;
pub use values::*;

/// Viewport used for media queries and viewport units.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
    pub device_pixel_ratio: f32,
}

/// Cascade origin of a stylesheet.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    UserAgent,
    User,
    Author,
}

/// Loads `@import`ed stylesheets. Implemented by the shell on top of huginn-net.
pub trait StylesheetLoader {
    /// Fetch the stylesheet at the absolute `url`. Returns the CSS text and
    /// the final URL (base for nested relative URLs), or `None` on failure.
    fn load(&self, url: &str) -> Option<(String, String)>;
}

/// A loader that never loads anything (for tests and `@import`-free input).
pub struct NoImports;

impl StylesheetLoader for NoImports {
    fn load(&self, _url: &str) -> Option<(String, String)> {
        None
    }
}

/// An `@font-face` rule. The shell fetches `sources` and registers the font
/// with the layout engine under `family`.
#[derive(Clone, Debug, PartialEq)]
pub struct FontFaceRule {
    pub family: Arc<str>,
    pub sources: Vec<FontFaceSource>,
    /// `font-weight` descriptor range (single value gives min == max).
    pub weight: (f32, f32),
    pub style: FontStyle,
    /// `unicode-range` descriptor as inclusive code point ranges. Empty means
    /// every code point. (Added by huginn-net: the shell loads only the subsets
    /// whose range covers text on the page.)
    pub unicode_range: Vec<(u32, u32)>,
    /// `size-adjust` descriptor as a factor (1.0 when absent).
    pub size_adjust: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FontFaceSource {
    /// Absolute URL.
    pub url: String,
    /// The `format()` hint, lowercase, if given (e.g. "woff2").
    pub format: Option<String>,
}

/// Pseudo-elements that generate boxes.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PseudoElement {
    Before,
    After,
}

/// Computed style of one element (or pseudo-element).
///
/// Lengths are px; percentages and `auto` are kept for layout to resolve.
/// Colors are final (`currentcolor` is already resolved).
#[derive(Clone, Debug, PartialEq)]
pub struct ComputedStyle {
    // Box generation and positioning
    pub display: Display,
    /// `display` before blockification (CSS Display 3, section 2.7): an
    /// absolutely positioned box's static position depends on it.
    pub original_display: Display,
    pub position: Position,
    pub float: Float,
    pub clear: Clear,
    pub box_sizing: BoxSizing,
    pub inset: Edges<LengthPercentageAuto>,
    pub z_index: Option<i32>,
    pub overflow_x: Overflow,
    pub overflow_y: Overflow,
    pub visibility: Visibility,
    pub opacity: f32,

    // Sizing
    pub width: LengthPercentageAuto,
    pub height: LengthPercentageAuto,
    pub min_width: LengthPercentageAuto,
    pub min_height: LengthPercentageAuto,
    /// `Auto` means `none`.
    pub max_width: LengthPercentageAuto,
    /// `Auto` means `none`.
    pub max_height: LengthPercentageAuto,
    pub aspect_ratio: Option<f32>,

    // Box model
    pub margin: Edges<LengthPercentageAuto>,
    pub padding: Edges<LengthPercentage>,
    /// Computed border width (0 when the style is `none`/`hidden`).
    pub border_width: Edges<f32>,
    pub border_style: Edges<BorderStyle>,
    pub border_color: Edges<Color>,
    pub border_radius: Corners<LengthPercentage>,

    // Background
    pub background_color: Color,
    pub background_image: Option<BackgroundImage>,
    pub background_repeat: BackgroundRepeat,
    /// `None` = `mask-image: none` (or no layer we can draw). Creates a
    /// stacking context.
    pub mask: Option<Arc<Mask>>,

    // Flexbox and box alignment
    pub flex_direction: FlexDirection,
    pub flex_wrap: FlexWrap,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: LengthPercentageAuto,
    pub order: i32,
    pub justify_content: Alignment,
    pub align_items: Alignment,
    pub align_self: Alignment,
    pub align_content: Alignment,
    pub row_gap: LengthPercentage,
    pub column_gap: LengthPercentage,

    // Text and fonts (inherited)
    pub color: Color,
    /// CSS `font-family` list as written, e.g. `"Helvetica Neue", Arial, sans-serif`.
    pub font_family: Arc<str>,
    pub font_size: f32,
    /// 1..=1000; 400 normal, 700 bold.
    pub font_weight: f32,
    pub font_style: FontStyle,
    pub line_height: LineHeight,
    pub text_align: TextAlign,
    pub text_indent: LengthPercentage,
    pub text_transform: TextTransform,
    pub white_space: WhiteSpace,
    /// `text-wrap-style: balance` (`text-wrap: balance`).
    pub text_wrap_balance: bool,
    pub letter_spacing: f32,
    pub word_spacing: f32,
    pub list_style_type: ListStyleType,
    pub list_style_position: ListStylePosition,

    // Text decoration (not inherited; layout propagates it to descendants)
    pub text_decoration_line: TextDecorationLine,
    pub text_decoration_color: Color,
    pub vertical_align: VerticalAlign,

    // Generated content (only meaningful on ::before/::after styles)
    pub content: Content,

    // ---- Added for layout and paint (2026-11-12). Defaults keep the old behavior.
    // Background (first layer)
    pub background_size: BackgroundSize,
    /// (x, y) from the top-left of the background area; keywords resolved
    /// (`left`/`top` = 0%, `center` = 50%, `right 10px` = calc(100% - 10px)).
    pub background_position: (LengthPercentage, LengthPercentage),
    // Effects
    /// `None` = `box-shadow: none`. Listed front (top) shadow first.
    pub box_shadow: Option<Arc<[BoxShadow]>>,
    /// `None` = `transform: none`. Applied left to right, as written.
    pub transform: Option<Arc<[TransformOp]>>,
    pub transform_origin: (LengthPercentage, LengthPercentage),
    pub outline_width: f32,
    pub outline_style: BorderStyle,
    pub outline_color: Color,
    pub outline_offset: f32,
    pub object_fit: ObjectFit,
    pub text_overflow: TextOverflow,
    // Tables
    /// Inherited.
    pub border_collapse: BorderCollapse,
    /// (horizontal, vertical) px. Inherited.
    pub border_spacing: (f32, f32),
    pub table_layout: TableLayout,
    /// Inherited.
    pub caption_side: CaptionSide,
    // Grid
    /// `None` = `none`.
    pub grid_template_columns: Option<Arc<[GridTrack]>>,
    /// `None` = `none`.
    pub grid_template_rows: Option<Arc<[GridTrack]>>,
    /// `None` = `none`.
    pub grid_template_areas: Option<Arc<[GridArea]>>,
    /// `None` = `auto`.
    pub grid_auto_columns: Option<Arc<[TrackSize]>>,
    /// `None` = `auto`.
    pub grid_auto_rows: Option<Arc<[TrackSize]>>,
    pub grid_auto_flow: GridAutoFlow,
    /// (start, end).
    pub grid_column: (GridLine, GridLine),
    /// (start, end).
    pub grid_row: (GridLine, GridLine),
    pub justify_items: Alignment,
    pub justify_self: Alignment,
    // SVG paint (inherited), for inline <svg> content.
    /// `None` = `fill: none`. Initial: black.
    pub fill: Option<Color>,
    /// `None` = `stroke: none` (the initial value).
    pub stroke: Option<Color>,
    /// `None` = `clip-path: none` (also for `url()` and box-only values).
    pub clip_path: Option<Arc<ClipPath>>,
    /// `None` = `normal`. Sorted by tag, one entry per tag. Inherited.
    pub font_feature_settings: Option<Arc<[FontFeature]>>,
}

impl Default for ComputedStyle {
    /// CSS initial values, with the UA default font (16px serif).
    fn default() -> Self {
        ComputedStyle {
            display: Display::Inline,
            original_display: Display::Inline,
            position: Position::Static,
            float: Float::None,
            clear: Clear::None,
            box_sizing: BoxSizing::ContentBox,
            inset: Edges::all(LengthPercentageAuto::Auto),
            z_index: None,
            overflow_x: Overflow::Visible,
            overflow_y: Overflow::Visible,
            visibility: Visibility::Visible,
            opacity: 1.0,
            width: LengthPercentageAuto::Auto,
            height: LengthPercentageAuto::Auto,
            min_width: LengthPercentageAuto::Auto,
            min_height: LengthPercentageAuto::Auto,
            max_width: LengthPercentageAuto::Auto,
            max_height: LengthPercentageAuto::Auto,
            aspect_ratio: None,
            margin: Edges::all(LengthPercentageAuto::ZERO),
            padding: Edges::all(LengthPercentage::ZERO),
            border_width: Edges::all(0.0),
            border_style: Edges::all(BorderStyle::None),
            border_color: Edges::all(Color::BLACK),
            border_radius: Corners::all(LengthPercentage::ZERO),
            background_color: Color::TRANSPARENT,
            background_image: None,
            background_repeat: BackgroundRepeat::Repeat,
            mask: None,
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::NoWrap,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: LengthPercentageAuto::Auto,
            order: 0,
            justify_content: Alignment::Normal,
            align_items: Alignment::Normal,
            align_self: Alignment::Normal,
            align_content: Alignment::Normal,
            row_gap: LengthPercentage::ZERO,
            column_gap: LengthPercentage::ZERO,
            color: Color::BLACK,
            font_family: Arc::from("serif"),
            font_size: 16.0,
            font_weight: 400.0,
            font_style: FontStyle::Normal,
            line_height: LineHeight::Normal,
            text_align: TextAlign::Start,
            text_indent: LengthPercentage::ZERO,
            text_transform: TextTransform::None,
            white_space: WhiteSpace::Normal,
            text_wrap_balance: false,
            letter_spacing: 0.0,
            word_spacing: 0.0,
            list_style_type: ListStyleType::Disc,
            list_style_position: ListStylePosition::Outside,
            text_decoration_line: TextDecorationLine::default(),
            text_decoration_color: Color::BLACK,
            vertical_align: VerticalAlign::Baseline,
            content: Content::None,
            background_size: BackgroundSize::Auto,
            background_position: (
                LengthPercentage::Percent(0.0),
                LengthPercentage::Percent(0.0),
            ),
            box_shadow: None,
            transform: None,
            transform_origin: (
                LengthPercentage::Percent(50.0),
                LengthPercentage::Percent(50.0),
            ),
            outline_width: 0.0,
            outline_style: BorderStyle::None,
            outline_color: Color::BLACK,
            outline_offset: 0.0,
            object_fit: ObjectFit::Fill,
            text_overflow: TextOverflow::Clip,
            border_collapse: BorderCollapse::Separate,
            border_spacing: (0.0, 0.0),
            table_layout: TableLayout::Auto,
            caption_side: CaptionSide::Top,
            grid_template_columns: None,
            grid_template_rows: None,
            grid_template_areas: None,
            grid_auto_columns: None,
            grid_auto_rows: None,
            grid_auto_flow: GridAutoFlow::Row,
            grid_column: (GridLine::Auto, GridLine::Auto),
            grid_row: (GridLine::Auto, GridLine::Auto),
            justify_items: Alignment::Normal,
            justify_self: Alignment::Normal,
            fill: Some(Color::BLACK),
            stroke: None,
            clip_path: None,
            font_feature_settings: None,
        }
    }
}

impl ComputedStyle {
    /// Initial values for non-inherited properties, inherited values copied
    /// from `parent`.
    pub fn inherit_from(parent: &ComputedStyle) -> ComputedStyle {
        ComputedStyle {
            visibility: parent.visibility,
            color: parent.color,
            font_family: parent.font_family.clone(),
            font_size: parent.font_size,
            font_weight: parent.font_weight,
            font_style: parent.font_style,
            line_height: parent.line_height,
            text_align: parent.text_align,
            text_indent: parent.text_indent,
            text_transform: parent.text_transform,
            white_space: parent.white_space,
            text_wrap_balance: parent.text_wrap_balance,
            letter_spacing: parent.letter_spacing,
            word_spacing: parent.word_spacing,
            list_style_type: parent.list_style_type,
            list_style_position: parent.list_style_position,
            border_collapse: parent.border_collapse,
            border_spacing: parent.border_spacing,
            caption_side: parent.caption_side,
            fill: parent.fill,
            stroke: parent.stroke,
            font_feature_settings: parent.font_feature_settings.clone(),
            ..ComputedStyle::default()
        }
    }

    /// Used line height in px for this style's own font size.
    /// `normal` uses 1.2, which matches common fonts closely enough for layout
    /// of boxes; text layout uses the real font metrics.
    pub fn line_height_px(&self) -> f32 {
        match self.line_height {
            LineHeight::Normal => self.font_size * 1.2,
            LineHeight::Number(n) => self.font_size * n,
            LineHeight::Px(px) => px,
        }
    }
}

/// Computed styles for a document, indexed by `NodeId`.
///
/// Elements that are not rendered (`display: none` subtrees) have no style.
/// Text nodes have no style; use the parent element's style.
#[derive(Debug, Default)]
pub struct StyleMap {
    styles: Vec<Option<Arc<ComputedStyle>>>,
    /// Sorted by (node index, pseudo) for binary search.
    pseudos: Vec<(u32, PseudoElement, Arc<ComputedStyle>)>,
}

impl StyleMap {
    /// Style of an element, `None` for non-elements and `display: none` subtrees.
    pub fn get(&self, id: NodeId) -> Option<&Arc<ComputedStyle>> {
        self.styles.get(id.index())?.as_ref()
    }

    /// Style of a `::before`/`::after` box of `id`, only if it generates a box
    /// (its `content` is not `none`).
    pub fn pseudo(&self, id: NodeId, which: PseudoElement) -> Option<&Arc<ComputedStyle>> {
        let key = (id.index() as u32, which);
        self.pseudos
            .binary_search_by(|(n, p, _)| (*n, *p).cmp(&key))
            .ok()
            .map(|i| &self.pseudos[i].2)
    }

    /// Number of styled elements.
    pub fn styled_count(&self) -> usize {
        self.styles.iter().filter(|s| s.is_some()).count()
    }
}

/// Holds stylesheets and computes styles for documents.
///
/// The cascade is Stylo's (see ARCHITECTURE.md, "The Stylo decision"); this
/// type keeps the stylist and converts Stylo's computed values into
/// [`ComputedStyle`].
pub struct StyleEngine {
    viewport: Viewport,
    lock: SharedRwLock,
    stylist: RefCell<Stylist>,
    font_faces: Vec<FontFaceRule>,
    document_url: UrlExtraData,
    quirks_sheet_added: Cell<bool>,
    /// Parsed shadow-root `<style>` sheets, by text (Lit and friends repeat
    /// the same sheet in every instance of a component).
    shadow_sheets: RefCell<HashMap<Box<str>, style::stylesheets::DocumentStyleSheet>>,
}

impl StyleEngine {
    /// A new engine with the built-in user-agent stylesheet loaded.
    pub fn new(viewport: Viewport) -> StyleEngine {
        engine::init_stylo();
        let lock = SharedRwLock::new();
        let stylist = Stylist::new(
            engine::make_device(viewport),
            style::context::QuirksMode::NoQuirks,
        );
        let mut engine = StyleEngine {
            viewport,
            lock,
            stylist: RefCell::new(stylist),
            font_faces: Vec::new(),
            document_url: engine::url_data("about:blank"),
            quirks_sheet_added: Cell::new(false),
            shadow_sheets: RefCell::default(),
        };
        engine.add_stylesheet(UA_STYLESHEET, "about:ua", Origin::UserAgent, &NoImports);
        engine
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    /// Change the viewport. Media queries are re-evaluated on the next `compute`.
    pub fn set_viewport(&mut self, viewport: Viewport) {
        self.viewport = viewport;
        let guard = self.lock.read();
        let guards = StylesheetGuards::same(&guard);
        self.stylist
            .get_mut()
            .set_device(engine::make_device(viewport), &guards);
    }

    /// The document's URL: the base for `url()` in `style` attributes and
    /// presentational hints (e.g. `<body background>`).
    pub fn set_document_url(&mut self, url: &str) {
        self.document_url = engine::url_data(url);
    }

    /// Add a stylesheet. Call in document order: later sheets win ties.
    /// `base_url` resolves relative `url()`s and `@import`s.
    pub fn add_stylesheet(
        &mut self,
        css: &str,
        base_url: &str,
        origin: Origin,
        loader: &dyn StylesheetLoader,
    ) {
        let stylist = self.stylist.get_mut();
        let sheet = engine::parse_sheet(
            css,
            base_url,
            origin,
            &self.lock,
            loader,
            stylist.device().quirks_mode(),
            0,
        );
        self.font_faces
            .extend(engine::font_faces(&sheet, stylist.device(), &self.lock));
        stylist.append_stylesheet(DocumentStyleSheet(sheet), &self.lock.read());
    }

    /// `@font-face` rules seen so far, in order.
    pub fn font_faces(&self) -> &[FontFaceRule] {
        &self.font_faces
    }

    /// Whether a document-level rule (as of the last `compute`) uses the
    /// pseudo-classes behind the given parts of the interaction state:
    /// `:hover`, `:active`, and the focus ones. When the changed parts are
    /// unused, a hover or focus change needs no restyle. Shadow-tree rules
    /// are not tracked, so pages with shadow roots always answer true.
    pub fn uses_state(&self, doc: &Document, hover: bool, active: bool, focus: bool) -> bool {
        use style_dom::ElementState as S;
        if doc.has_shadow_roots() {
            return true;
        }
        let mut want = S::empty();
        if hover {
            want |= S::HOVER;
        }
        if active {
            want |= S::ACTIVE;
        }
        if focus {
            want |= S::FOCUS | S::FOCUS_WITHIN | S::FOCUSRING;
        }
        self.stylist
            .borrow()
            .iter_origins()
            .any(|(data, _)| data.has_state_dependency(want))
    }

    /// Compute styles for every element of `doc`.
    pub fn compute(&self, doc: &Document) -> StyleMap {
        let mut stylist = self.stylist.borrow_mut();
        let quirks = match doc.quirks_mode() {
            huginn_dom::QuirksMode::Quirks => style::context::QuirksMode::Quirks,
            huginn_dom::QuirksMode::LimitedQuirks => style::context::QuirksMode::LimitedQuirks,
            huginn_dom::QuirksMode::NoQuirks => style::context::QuirksMode::NoQuirks,
        };
        stylist.set_quirks_mode(quirks);
        if quirks == style::context::QuirksMode::Quirks && !self.quirks_sheet_added.get() {
            let sheet = engine::parse_sheet(
                QUIRKS_STYLESHEET,
                "about:ua",
                Origin::UserAgent,
                &self.lock,
                &NoImports,
                quirks,
                0,
            );
            stylist.append_stylesheet(DocumentStyleSheet(sheet), &self.lock.read());
            self.quirks_sheet_added.set(true);
        }
        {
            let guard = self.lock.read();
            stylist.flush(&StylesheetGuards::same(&guard));
        }
        let mut cx = engine::build_cx(doc, &self.lock, &self.document_url);
        if doc.has_shadow_roots() {
            cx.shadow_data = engine::shadow_cascade_data(
                &mut stylist,
                doc,
                &self.lock,
                self.document_url.0.as_str(),
                &mut self.shadow_sheets.borrow_mut(),
            );
        }
        engine::traverse(&stylist, &cx, &self.lock);
        drop(stylist);

        let conv = convert::Cx {
            viewport_width: self.viewport.width,
        };
        let mut map = StyleMap {
            styles: vec![None; doc.len()],
            pseudos: Vec::new(),
        };
        // Elements that share a Stylo style share one converted style.
        let mut shared: HashMap<*const style::properties::ComputedValues, Arc<ComputedStyle>> =
            HashMap::new();
        for id in doc.shadow_including_descendants(NodeId::DOCUMENT) {
            let Some(data) = engine::data_of(&cx, id) else {
                continue;
            };
            let Some(primary) = data.styles.get_primary() else {
                continue;
            };
            if primary.get_box().display.is_none() {
                continue;
            }
            let key = &**primary as *const _;
            let style = shared
                .entry(key)
                .or_insert_with(|| Arc::new(convert::convert(&conv, primary)))
                .clone();
            for (which, pseudo) in [
                (
                    PseudoElement::Before,
                    style::selector_parser::PseudoElement::Before,
                ),
                (
                    PseudoElement::After,
                    style::selector_parser::PseudoElement::After,
                ),
            ] {
                if let Some(ps) = data.styles.pseudos.get(&pseudo) {
                    let ps = convert::convert(&conv, ps);
                    if ps.content != Content::None && ps.display != Display::None {
                        map.pseudos.push((id.index() as u32, which, Arc::new(ps)));
                    }
                }
            }
            map.styles[id.index()] = Some(style);
        }
        map.pseudos.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        map
    }
}

/// The built-in user-agent stylesheet (HTML rendering defaults).
pub const UA_STYLESHEET: &str = include_str!("ua.css");

/// Extra user-agent rules for quirks-mode documents (Chrome's quirks.css).
const QUIRKS_STYLESHEET: &str = include_str!("quirks.css");

#[cfg(test)]
mod tests {
    use super::*;
    use huginn_dom::parse_html;

    fn engine() -> StyleEngine {
        StyleEngine::new(Viewport {
            width: 1280.0,
            height: 800.0,
            device_pixel_ratio: 1.0,
        })
    }

    fn style_of(css: &str, html: &str, id: &str) -> Arc<ComputedStyle> {
        let doc = parse_html(html);
        let mut e = engine();
        e.add_stylesheet(css, "file:///t/a.css", Origin::Author, &NoImports);
        let map = e.compute(&doc);
        map.get(doc.element_by_id(id).unwrap()).unwrap().clone()
    }

    #[test]
    fn specificity_beats_order_and_important_beats_specificity() {
        let s = style_of(
            "#a { color: red } p { color: blue } .c { background: green !important } #a { background: yellow }",
            "<p id=a class=c>x</p>",
            "a",
        );
        assert_eq!(s.color, Color::rgb(255, 0, 0));
        assert_eq!(s.background_color, Color::rgb(0, 128, 0));
    }

    #[test]
    fn inline_style_wins_and_em_uses_parent_font_size() {
        let s = style_of(
            "div { font-size: 20px } span { font-size: 2em; margin-left: 1em }",
            "<div><span id=s style='color: #0f0'>x</span></div>",
            "s",
        );
        assert_eq!(s.color, Color::rgb(0, 255, 0));
        assert_eq!(s.font_size, 40.0);
        assert_eq!(s.margin.left, LengthPercentageAuto::Px(40.0));
    }

    #[test]
    fn inheritance_and_ua_defaults() {
        let s = style_of(
            "body { color: rgb(1, 2, 3) }",
            "<body><h1 id=h>x</h1></body>",
            "h",
        );
        assert_eq!(s.color, Color::rgb(1, 2, 3));
        assert_eq!(s.display, Display::Block);
        assert_eq!(s.font_size, 32.0);
        assert_eq!(s.font_weight, 700.0);
    }

    #[test]
    fn combinators_and_media_queries() {
        let css = "ul > li.x + li { color: red } div p { color: blue } \
                   @media (max-width: 600px) { #c { color: green } } \
                   @media screen and (min-width: 1000px) { #c { background: navy } }";
        let html =
            "<ul><li class=x>1</li><li id=b>2</li></ul><div><section><p id=c>3</p></section></div>";
        assert_eq!(style_of(css, html, "b").color, Color::rgb(255, 0, 0));
        let c = style_of(css, html, "c");
        assert_eq!(c.color, Color::rgb(0, 0, 255));
        assert_eq!(c.background_color, Color::rgb(0, 0, 128));
    }

    #[test]
    fn flex_items_are_blockified_and_border_uses_currentcolor() {
        let s = style_of(
            ".f { display: flex } span { color: #123456; border: 2px solid }",
            "<div class=f><span id=i>x</span></div>",
            "i",
        );
        assert_eq!(s.display, Display::Block);
        assert_eq!(s.border_width.top, 2.0);
        assert_eq!(s.border_color.left, Color::rgb(0x12, 0x34, 0x56));
    }

    #[test]
    fn before_pseudo_gets_content() {
        let doc = parse_html("<p id=p>x</p>");
        let mut e = engine();
        e.add_stylesheet(
            "p::before { content: \"> \"; color: red }",
            "file:///a.css",
            Origin::Author,
            &NoImports,
        );
        let map = e.compute(&doc);
        let p = doc.element_by_id("p").unwrap();
        let before = map.pseudo(p, PseudoElement::Before).unwrap();
        assert_eq!(before.content, Content::Text(Arc::from("> ")));
        assert_eq!(before.color, Color::rgb(255, 0, 0));
        assert!(map.pseudo(p, PseudoElement::After).is_none());
    }

    #[test]
    fn custom_properties_layers_and_modern_colors() {
        let css = "@layer base, utilities; \
                   @layer utilities { .u { color: var(--c, red) } } \
                   @layer base { .u { color: blue; --c: oklch(0.628 0.2577 29.23) } } \
                   :root { --gap: 12px } \
                   .u { margin-left: calc(var(--gap) * 2); background: color-mix(in srgb, #ff0000 50%, #0000ff) }";
        let s = style_of(css, "<p id=a class=u>x</p>", "a");
        // The later layer wins; var() takes --c from the other layer; oklch()
        // (pure red here) becomes sRGB.
        let close = |a: u8, b: u8| a.abs_diff(b) <= 2;
        assert!(
            close(s.color.r, 255) && close(s.color.g, 0) && close(s.color.b, 0),
            "{:?}",
            s.color
        );
        assert_eq!(s.margin.left, LengthPercentageAuto::Px(24.0));
        assert_eq!(s.background_color, Color::rgb(128, 0, 128));
    }

    #[test]
    fn has_selector_and_mixed_calc() {
        let s = style_of(
            "div:has(> .x) { width: calc(100% - 20px) } div:not(:has(.y)) { padding-left: 3px }",
            "<div id=d><span class=x></span></div>",
            "d",
        );
        assert_eq!(
            s.width,
            LengthPercentageAuto::Calc(CalcLength {
                px: -20.0,
                percent: 100.0
            })
        );
        assert_eq!(s.padding.left, LengthPercentage::Px(3.0));
    }

    #[test]
    fn legacy_presentational_hints() {
        let doc = parse_html(
            "<table id=t cellpadding=4 cellspacing=0 width=85% bgcolor=#ff6600 align=center>\
             <tr><td id=c valign=top align=right nowrap><font id=f color=red size=1>x</font></td></tr></table>\
             <center id=m>c</center>",
        );
        let map = engine().compute(&doc);
        let get = |id| map.get(doc.element_by_id(id).unwrap()).unwrap().clone();
        let t = get("t");
        assert_eq!(t.background_color, Color::rgb(255, 102, 0));
        assert_eq!(t.width, LengthPercentageAuto::Percent(85.0));
        assert_eq!(t.border_spacing, (0.0, 0.0));
        assert_eq!(t.margin.left, LengthPercentageAuto::Auto);
        let c = get("c");
        assert_eq!(c.padding.top, LengthPercentage::Px(4.0));
        assert_eq!(c.vertical_align, VerticalAlign::Top);
        assert_eq!(c.text_align, TextAlign::WebkitRight);
        assert_eq!(c.white_space, WhiteSpace::Nowrap);
        let f = get("f");
        assert_eq!(f.color, Color::rgb(255, 0, 0));
        assert_eq!(f.font_size, 10.0);
        assert_eq!(get("m").text_align, TextAlign::WebkitCenter);
    }

    #[test]
    fn outermost_svg_width_and_height_are_presentation_attributes() {
        let css = ".big { width: 24px }";
        let html = "<svg id=a width=16 height=50%><svg id=b width=8 height=8></svg></svg>\
                    <svg id=c class=big width=16 height='1;color:red'></svg>";
        let a = style_of(css, html, "a");
        assert_eq!(a.width, LengthPercentageAuto::Px(16.0));
        assert_eq!(a.height, LengthPercentageAuto::Percent(50.0));
        // Inner <svg> geometry belongs to the SVG renderer.
        assert_eq!(style_of(css, html, "b").width, LengthPercentageAuto::Auto);
        // Author rules win; a value that is not one token is dropped.
        let c = style_of(css, html, "c");
        assert_eq!(c.width, LengthPercentageAuto::Px(24.0));
        assert_eq!(c.height, LengthPercentageAuto::Auto);
        assert_eq!(c.color, style_of(css, html, "a").color);
    }

    #[test]
    fn font_face_descriptors_include_unicode_range() {
        let mut e = engine();
        e.add_stylesheet(
            "@font-face { font-family: Inter; font-weight: 400 800; font-style: italic; \
             src: url(i.woff2) format('woff2'); unicode-range: U+0000-00FF, U+0131, u+04??; size-adjust: 95%; }",
            "http://h/css/a.css",
            Origin::Author,
            &NoImports,
        );
        let ff = &e.font_faces()[0];
        assert_eq!(&*ff.family, "Inter");
        assert_eq!(ff.weight, (400.0, 800.0));
        assert_eq!(ff.style, FontStyle::Italic);
        assert_eq!(ff.sources[0].url, "http://h/css/i.woff2");
        assert_eq!(
            ff.unicode_range,
            [(0, 0xFF), (0x131, 0x131), (0x400, 0x4FF)]
        );
        assert_eq!(ff.size_adjust, 0.95);
    }

    #[test]
    fn clip_path_shapes_and_inherited_font_features() {
        let css = "#a { clip-path: inset(1px 2px 3px 10% round 4px); font-feature-settings: \"tnum\", \"liga\" 0 } \
                   #c { clip-path: circle(closest-side at 0 100%) } \
                   #e { clip-path: polygon(evenodd, 0 0, 100% 0, 50% 100%) }";
        let html = "<div id=a><span id=b>x</span></div><div id=c></div><div id=e></div>";
        let a = style_of(css, html, "a");
        let ClipPath::Inset { inset, radius } = a.clip_path.as_deref().unwrap() else {
            panic!("{:?}", a.clip_path)
        };
        assert_eq!(
            (inset.top, inset.right, inset.bottom, inset.left),
            (
                LengthPercentage::Px(1.0),
                LengthPercentage::Px(2.0),
                LengthPercentage::Px(3.0),
                LengthPercentage::Percent(10.0)
            )
        );
        assert_eq!(radius.bottom_left, LengthPercentage::Px(4.0));
        // Sorted by tag, one entry per tag (Stylo's computed value).
        let features = [
            FontFeature {
                tag: *b"liga",
                value: 0,
            },
            FontFeature {
                tag: *b"tnum",
                value: 1,
            },
        ];
        // font-feature-settings inherits; clip-path does not.
        let b = style_of(css, html, "b");
        assert_eq!(b.font_feature_settings.as_deref(), Some(&features[..]));
        assert_eq!(b.clip_path, None);
        assert_eq!(
            style_of(css, html, "c").clip_path.as_deref(),
            Some(&ClipPath::Circle {
                radius: ShapeRadius::ClosestSide,
                center: (LengthPercentage::Px(0.0), LengthPercentage::Percent(100.0)),
            })
        );
        let e = style_of(css, html, "e");
        let Some(ClipPath::Polygon {
            even_odd: true,
            points,
        }) = e.clip_path.as_deref()
        else {
            panic!("{:?}", e.clip_path)
        };
        assert_eq!(
            points[2],
            (
                LengthPercentage::Percent(50.0),
                LengthPercentage::Percent(100.0)
            )
        );
    }

    // CSS Shadow Parts: `::part()` rules in the host's tree style elements
    // with that `part` name inside the shadow tree; `exportparts` forwards
    // (and renames) an inner part to the next tree out.
    #[test]
    fn part_rules_style_shadow_parts_and_exportparts_renames() {
        let html = "<div id=h class=outer><template shadowrootmode=open>\
                    <style>.inner::part(x) { color: rgb(0, 128, 0) }</style>\
                    <span part='y x'>a</span>\
                    <div class=inner exportparts='x: z, bad name'><template shadowrootmode=open><b part=x>b</b></template></div>\
                    </template></div>";
        let css = ".outer::part(x) { color: rgb(255, 0, 0) } .outer::part(z) { background: blue } \
                   .outer::part(bad) { outline-color: red }";
        let doc = parse_html(html);
        let mut e = engine();
        e.add_stylesheet(css, "file:///t/a.css", Origin::Author, &NoImports);
        let map = e.compute(&doc);
        let find = |name: &str| {
            doc.shadow_including_descendants(NodeId::DOCUMENT)
                .find(|&n| doc.element(n).is_some_and(|el| &*el.name.local == name))
                .unwrap()
        };
        let span = map.get(find("span")).unwrap();
        assert_eq!(span.color, Color::rgb(255, 0, 0));
        let b = map.get(find("b")).unwrap();
        assert_eq!(
            b.color,
            Color::rgb(0, 128, 0),
            "inner part not exported as x"
        );
        assert_eq!(b.background_color, Color::rgb(0, 0, 255), "exported as z");
        assert_eq!(
            b.outline_color,
            Color::rgb(0, 128, 0),
            "'bad name' is not a valid mapping"
        );
    }
}
