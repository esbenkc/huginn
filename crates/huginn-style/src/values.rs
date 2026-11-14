//! Computed value types. These are the contract between huginn-style and its
//! readers (huginn-layout, huginn-paint). All lengths are CSS px. Percentages stay
//! percentages because only layout knows what they are relative to.

use std::sync::Arc;

/// sRGB color with straight (not premultiplied) alpha.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Color = Color::rgba(0, 0, 0, 0);
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    pub const WHITE: Color = Color::rgb(255, 255, 255);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color { r, g, b, a }
    }

    pub fn is_transparent(self) -> bool {
        self.a == 0
    }

    /// Multiply alpha by `opacity` in 0..=1.
    pub fn with_opacity(self, opacity: f32) -> Color {
        Color {
            a: (f32::from(self.a) * opacity.clamp(0.0, 1.0)).round() as u8,
            ..self
        }
    }
}

/// A linear `calc()` result: `px + percent% of the basis`. Percent is
/// 0..=100. Covers `calc(100% - 20px)` and keyword positions like `right 10px`.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub struct CalcLength {
    pub px: f32,
    pub percent: f32,
}

impl CalcLength {
    /// Resolve against a basis in px (`None` = indefinite, the percent part is 0).
    pub fn resolve(self, basis: Option<f32>) -> f32 {
        self.px + basis.map_or(0.0, |b| b * self.percent / 100.0)
    }
}

/// `<length-percentage>`, computed.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum LengthPercentage {
    Px(f32),
    /// 0..=100, not 0..=1.
    Percent(f32),
    /// Mixed length and percentage (`calc(100% - 20px)`).
    Calc(CalcLength),
}

impl LengthPercentage {
    pub const ZERO: LengthPercentage = LengthPercentage::Px(0.0);

    /// Resolve against a basis in px (`None` = indefinite, percentages become 0).
    pub fn resolve(self, basis: Option<f32>) -> f32 {
        match self {
            LengthPercentage::Px(v) => v,
            LengthPercentage::Percent(p) => basis.map_or(0.0, |b| b * p / 100.0),
            LengthPercentage::Calc(c) => c.resolve(basis),
        }
    }

    /// The value as `px + percent`.
    pub fn as_calc(self) -> CalcLength {
        match self {
            LengthPercentage::Px(px) => CalcLength { px, percent: 0.0 },
            LengthPercentage::Percent(percent) => CalcLength { px: 0.0, percent },
            LengthPercentage::Calc(c) => c,
        }
    }
}

/// `<length-percentage> | auto`, computed. Also used for `none` in
/// `max-width`/`max-height` (as `Auto`).
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub enum LengthPercentageAuto {
    #[default]
    Auto,
    Px(f32),
    /// 0..=100, not 0..=1.
    Percent(f32),
    /// Mixed length and percentage (`calc(100% - 20px)`).
    Calc(CalcLength),
}

impl LengthPercentageAuto {
    pub const ZERO: LengthPercentageAuto = LengthPercentageAuto::Px(0.0);

    pub fn is_auto(self) -> bool {
        matches!(self, LengthPercentageAuto::Auto)
    }

    /// Resolve against a basis in px. `Auto` and indefinite percentages give `None`.
    pub fn resolve(self, basis: Option<f32>) -> Option<f32> {
        match self {
            LengthPercentageAuto::Auto => None,
            LengthPercentageAuto::Px(v) => Some(v),
            LengthPercentageAuto::Percent(p) => basis.map(|b| b * p / 100.0),
            LengthPercentageAuto::Calc(c) if c.percent == 0.0 => Some(c.px),
            LengthPercentageAuto::Calc(c) => basis.map(|b| c.px + b * c.percent / 100.0),
        }
    }
}

impl From<LengthPercentage> for LengthPercentageAuto {
    fn from(v: LengthPercentage) -> Self {
        match v {
            LengthPercentage::Px(p) => LengthPercentageAuto::Px(p),
            LengthPercentage::Percent(p) => LengthPercentageAuto::Percent(p),
            LengthPercentage::Calc(c) => LengthPercentageAuto::Calc(c),
        }
    }
}

/// Four box sides, in CSS order.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub struct Edges<T> {
    pub top: T,
    pub right: T,
    pub bottom: T,
    pub left: T,
}

impl<T: Copy> Edges<T> {
    pub const fn all(v: T) -> Edges<T> {
        Edges {
            top: v,
            right: v,
            bottom: v,
            left: v,
        }
    }

    pub fn map<U>(self, mut f: impl FnMut(T) -> U) -> Edges<U> {
        Edges {
            top: f(self.top),
            right: f(self.right),
            bottom: f(self.bottom),
            left: f(self.left),
        }
    }
}

/// Four corners: top-left, top-right, bottom-right, bottom-left.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub struct Corners<T> {
    pub top_left: T,
    pub top_right: T,
    pub bottom_right: T,
    pub bottom_left: T,
}

impl<T: Copy> Corners<T> {
    pub const fn all(v: T) -> Corners<T> {
        Corners {
            top_left: v,
            top_right: v,
            bottom_right: v,
            bottom_left: v,
        }
    }
}

/// `display`, flattened into the combinations Huginn handles.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum Display {
    None,
    Contents,
    Block,
    #[default]
    Inline,
    InlineBlock,
    FlowRoot,
    ListItem,
    Flex,
    InlineFlex,
    Grid,
    InlineGrid,
    Table,
    InlineTable,
    TableRowGroup,
    TableHeaderGroup,
    TableFooterGroup,
    TableRow,
    TableCell,
    TableColumnGroup,
    TableColumn,
    TableCaption,
}

impl Display {
    /// Inline-level: participates in an inline formatting context.
    pub fn is_inline_level(self) -> bool {
        matches!(
            self,
            Display::Inline
                | Display::InlineBlock
                | Display::InlineFlex
                | Display::InlineGrid
                | Display::InlineTable
        )
    }

    /// The block-level equivalent (CSS "blockification").
    pub fn blockified(self) -> Display {
        match self {
            Display::Inline | Display::InlineBlock => Display::Block,
            Display::InlineFlex => Display::Flex,
            Display::InlineGrid => Display::Grid,
            Display::InlineTable => Display::Table,
            other => other,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum Position {
    #[default]
    Static,
    Relative,
    Absolute,
    Fixed,
    Sticky,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum Float {
    #[default]
    None,
    Left,
    Right,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum Clear {
    #[default]
    None,
    Left,
    Right,
    Both,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum BoxSizing {
    #[default]
    ContentBox,
    BorderBox,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum Overflow {
    #[default]
    Visible,
    Hidden,
    Clip,
    Scroll,
    Auto,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum Visibility {
    #[default]
    Visible,
    Hidden,
    Collapse,
}

/// `pointer-events` (the HTML values; the SVG ones are Gecko only in Stylo).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum PointerEvents {
    #[default]
    Auto,
    /// The box is never the target of pointer events; they go to what is
    /// under it.
    None,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum BorderStyle {
    #[default]
    None,
    Hidden,
    Solid,
    Dashed,
    Dotted,
    Double,
    Groove,
    Ridge,
    Inset,
    Outset,
}

impl BorderStyle {
    /// `none` and `hidden` force the used border width to 0.
    pub fn is_visible(self) -> bool {
        !matches!(self, BorderStyle::None | BorderStyle::Hidden)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum FontStyle {
    #[default]
    Normal,
    Italic,
    Oblique,
}

#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub enum LineHeight {
    #[default]
    Normal,
    /// Unitless multiplier of the font size.
    Number(f32),
    Px(f32),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum TextAlign {
    #[default]
    Start,
    End,
    Left,
    Right,
    Center,
    Justify,
    /// `-webkit-center` (`<center>`, `align=center`): centers inline content
    /// and block children.
    WebkitCenter,
    /// `-webkit-left`: like `left`, and also aligns block children left.
    WebkitLeft,
    /// `-webkit-right`: like `right`, and also aligns block children right.
    WebkitRight,
}

/// `text-decoration-line` flags.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct TextDecorationLine {
    pub underline: bool,
    pub overline: bool,
    pub line_through: bool,
}

impl TextDecorationLine {
    pub fn any(self) -> bool {
        self.underline || self.overline || self.line_through
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum TextTransform {
    #[default]
    None,
    Uppercase,
    Lowercase,
    Capitalize,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum WhiteSpace {
    #[default]
    Normal,
    Pre,
    Nowrap,
    PreWrap,
    PreLine,
    BreakSpaces,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum VerticalAlign {
    #[default]
    Baseline,
    Sub,
    Super,
    Top,
    TextTop,
    Middle,
    Bottom,
    TextBottom,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum ListStyleType {
    None,
    #[default]
    Disc,
    Circle,
    Square,
    Decimal,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum ListStylePosition {
    #[default]
    Outside,
    Inside,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum FlexDirection {
    #[default]
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum FlexWrap {
    #[default]
    NoWrap,
    Wrap,
    WrapReverse,
}

/// Values shared by `align-items`, `align-self`, `align-content`,
/// `justify-content`, `justify-items` and `justify-self`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum Alignment {
    /// `normal` (or `auto` for `*-self`).
    #[default]
    Normal,
    Start,
    End,
    FlexStart,
    FlexEnd,
    Center,
    Baseline,
    Stretch,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
    Left,
    Right,
}

/// One `background-image` layer (the topmost layer that Huginn can draw).
#[derive(Clone, Debug, PartialEq)]
pub enum BackgroundImage {
    /// Absolute URL, resolved against the stylesheet that declared it.
    Url(Arc<str>),
    /// `linear-gradient()` / `repeating-linear-gradient()`; `shape` is `Linear`.
    LinearGradient(Arc<Gradient>),
    /// `radial-gradient()` / `repeating-radial-gradient()`; `shape` is `Radial`.
    RadialGradient(Arc<Gradient>),
}

/// A computed CSS gradient.
#[derive(Clone, Debug, PartialEq)]
pub struct Gradient {
    pub repeating: bool,
    pub shape: GradientShape,
    /// At least two stops, in order. Positions are as written; a stop without
    /// a position is spread evenly between its neighbours (paint does that).
    pub stops: Vec<ColorStop>,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum GradientShape {
    Linear {
        /// CSS angle in degrees: 0 = to top, 90 = to right, 180 = to bottom.
        angle_deg: f32,
        /// `to <corner>` as (right, bottom): `to right bottom` = (true, true),
        /// `to left top` = (false, false). Paint derives the angle from the
        /// box size; `angle_deg` is then only a fallback.
        corner: Option<(bool, bool)>,
    },
    Radial {
        /// `circle` (true) or `ellipse` (false).
        circle: bool,
        extent: RadialExtent,
        /// Center, from the top-left of the box.
        center: (LengthPercentage, LengthPercentage),
    },
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum RadialExtent {
    ClosestSide,
    ClosestCorner,
    FarthestSide,
    FarthestCorner,
    /// Explicit radii (x, y). A circle has x == y.
    Explicit(LengthPercentage, LengthPercentage),
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ColorStop {
    pub color: Color,
    pub position: Option<LengthPercentage>,
}

/// `background-size` of the first layer.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub enum BackgroundSize {
    #[default]
    Auto,
    Cover,
    Contain,
    /// Width and height; `Auto` keeps the image's aspect ratio.
    Explicit(LengthPercentageAuto, LengthPercentageAuto),
}

/// The first drawable `mask-image` layer with its geometry (CSS Masking 1).
/// Positioned and sized against the border box (`mask-origin` and
/// `mask-clip` initial values); the image's alpha is the mask.
#[derive(Clone, Debug, PartialEq)]
pub struct Mask {
    pub image: BackgroundImage,
    pub size: BackgroundSize,
    pub position: (LengthPercentage, LengthPercentage),
    pub repeat: BackgroundRepeat,
}

/// One `box-shadow` (also used for `text-shadow` later).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct BoxShadow {
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Color,
    pub inset: bool,
}

/// One 2D transform function, computed. Angles in degrees.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum TransformOp {
    Translate(LengthPercentage, LengthPercentage),
    Scale(f32, f32),
    Rotate(f32),
    Skew(f32, f32),
    /// `matrix(a, b, c, d, e, f)`; e and f in px.
    Matrix([f32; 6]),
}

/// A computed `clip-path` basic shape, relative to the border box.
/// (`url()` references and box-only values are not kept.)
#[derive(Clone, Debug, PartialEq)]
pub enum ClipPath {
    /// `inset()` (also `rect()` and `xywh()`, which compute to it): the
    /// distance of each edge from the border box edge, plus corner radii
    /// (horizontal radius only).
    Inset {
        inset: Edges<LengthPercentage>,
        radius: Corners<LengthPercentage>,
    },
    /// `circle()`. Percent radius is of `sqrt(w² + h²) / sqrt(2)`.
    Circle {
        radius: ShapeRadius,
        center: (LengthPercentage, LengthPercentage),
    },
    /// `ellipse()`. Percent radii are of the width and height.
    Ellipse {
        radius_x: ShapeRadius,
        radius_y: ShapeRadius,
        center: (LengthPercentage, LengthPercentage),
    },
    /// `polygon()`: points from the top-left of the border box.
    Polygon {
        even_odd: bool,
        points: Arc<[(LengthPercentage, LengthPercentage)]>,
    },
}

/// Radius of a `circle()`/`ellipse()` clip shape.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ShapeRadius {
    Length(LengthPercentage),
    ClosestSide,
    FarthestSide,
    ClosestCorner,
    FarthestCorner,
}

/// One `font-feature-settings` entry, e.g. `"tnum" 1`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FontFeature {
    /// The OpenType tag as written, e.g. `*b"liga"`.
    pub tag: [u8; 4],
    pub value: u32,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum ObjectFit {
    #[default]
    Fill,
    Contain,
    Cover,
    None,
    ScaleDown,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum TextOverflow {
    #[default]
    Clip,
    Ellipsis,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum BorderCollapse {
    #[default]
    Separate,
    Collapse,
}

/// `caption-side`: the table caption sits above (`top`) or below the rows.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum CaptionSide {
    #[default]
    Top,
    Bottom,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum TableLayout {
    #[default]
    Auto,
    Fixed,
}

/// One breadth inside a grid track size.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum TrackBreadth {
    Length(LengthPercentage),
    /// `<flex>`, e.g. `1fr`.
    Fr(f32),
    Auto,
    MinContent,
    MaxContent,
}

/// One grid track size.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum TrackSize {
    Breadth(TrackBreadth),
    MinMax(TrackBreadth, TrackBreadth),
    FitContent(LengthPercentage),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GridRepeat {
    Count(u16),
    AutoFill,
    AutoFit,
}

/// One entry of `grid-template-columns`/`-rows`, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum GridTrack {
    Size(TrackSize),
    /// `repeat(count, tracks)`. Line names inside the repeat are dropped.
    Repeat(GridRepeat, Arc<[TrackSize]>),
    /// `[a b]`: names of the grid line before the next track.
    LineNames(Arc<[Arc<str>]>),
}

/// A named area from `grid-template-areas`. Lines are 1-based; `*_end` is
/// the line after the area (exclusive), as in `grid-row: 1 / 3`.
#[derive(Clone, Debug, PartialEq)]
pub struct GridArea {
    pub name: Arc<str>,
    pub row_start: u16,
    pub row_end: u16,
    pub column_start: u16,
    pub column_end: u16,
}

/// One side of `grid-row`/`grid-column` placement.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum GridLine {
    #[default]
    Auto,
    /// Line number (negative counts from the end). Never 0.
    Line(i16),
    /// `span N`.
    Span(u16),
    /// A named line or area (`grid-area: header` gives `Named("header")`
    /// on all four sides; layout looks up the area or `header-start`/`-end`).
    Named(Arc<str>),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum GridAutoFlow {
    #[default]
    Row,
    Column,
    RowDense,
    ColumnDense,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum BackgroundRepeat {
    #[default]
    Repeat,
    RepeatX,
    RepeatY,
    NoRepeat,
}

/// Generated content of `::before`/`::after`.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Content {
    /// `normal`/`none`: no box is generated.
    #[default]
    None,
    Text(Arc<str>),
    /// A content list with at least one image, in order (CSS Content 3,
    /// section 1.1). Adjacent strings are merged.
    Items(Arc<[ContentItem]>),
}

/// One piece of a `content` list that has images.
#[derive(Clone, Debug, PartialEq)]
pub enum ContentItem {
    Text(Arc<str>),
    /// `url(...)`: absolute URL. Shown as an anonymous inline replaced box.
    Image(Arc<str>),
}
