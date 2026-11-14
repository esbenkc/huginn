//! Stylo computed values -> our flat [`ComputedStyle`].
//!
//! The conversion is a pure function of one `ComputedValues`, so elements
//! that share a Stylo style share one converted `Arc<ComputedStyle>` (see
//! `StyleEngine::compute`).

use std::sync::Arc;

use style::color::AbsoluteColor;
use style::computed_values;
use style::properties::ComputedValues;
use style::values::computed::length_percentage::Unpacked;
use style::values::computed::{self as c, Length};
use style::values::generics;
use style::values::specified::align::AlignFlags;

use crate::values::*;
use crate::{ComputedStyle, Content};

/// Basis used to flatten non-linear `calc()` (min/max/clamp mixing
/// percentages and lengths): the viewport width.
pub(crate) struct Cx {
    pub viewport_width: f32,
}

pub(crate) fn color(c: &AbsoluteColor) -> Color {
    let c = c.into_srgb_legacy();
    let [r, g, b, a] = *c.raw_components();
    let to8 = |x: f32| (x * 255.0).round().clamp(0.0, 255.0) as u8;
    Color::rgba(to8(r), to8(g), to8(b), to8(a))
}

fn px(l: Length) -> f32 {
    l.px()
}

impl Cx {
    fn lp(&self, v: &c::LengthPercentage) -> LengthPercentage {
        match v.unpack() {
            Unpacked::Length(l) => LengthPercentage::Px(l.px()),
            Unpacked::Percentage(p) => LengthPercentage::Percent(p.0 * 100.0),
            Unpacked::Calc(calc) => {
                // Linear calc is f(b) = px + k*b. Probe four bases to tell it
                // apart from min()/max()/clamp(). The bases are typical box
                // widths and big enough that `calc(100% - 20px)` is not
                // clamped at 0 (non-negative properties clamp the result).
                let f = |b: f32| calc.resolve(Length::new(b)).px();
                let (f0, f1, f2, f3) = (f(1000.0), f(1500.0), f(2000.0), f(2500.0));
                let (d1, d2, d3) = (f1 - f0, f2 - f1, f3 - f2);
                if (d2 - d1).abs() < 0.01 && (d3 - d2).abs() < 0.01 {
                    // k*500 = d1, so the percentage is d1 / 5.
                    let percent = d1 / 5.0;
                    let px = f0 - d1 * 2.0;
                    if percent.abs() < 1e-4 {
                        LengthPercentage::Px(px)
                    } else {
                        LengthPercentage::Calc(CalcLength { px, percent })
                    }
                } else {
                    LengthPercentage::Px(f(self.viewport_width))
                }
            }
        }
    }

    fn lpa(&self, v: &c::LengthPercentage) -> LengthPercentageAuto {
        self.lp(v).into()
    }

    fn size(&self, v: &c::Size) -> LengthPercentageAuto {
        match v {
            generics::length::GenericSize::LengthPercentage(lp) => self.lpa(&lp.0),
            generics::length::GenericSize::FitContentFunction(lp) => self.lpa(&lp.0),
            _ => LengthPercentageAuto::Auto,
        }
    }

    fn max_size(&self, v: &c::MaxSize) -> LengthPercentageAuto {
        match v {
            generics::length::GenericMaxSize::LengthPercentage(lp) => self.lpa(&lp.0),
            _ => LengthPercentageAuto::Auto,
        }
    }

    fn margin(&self, v: &c::Margin) -> LengthPercentageAuto {
        match v {
            generics::length::GenericMargin::LengthPercentage(lp) => self.lpa(lp),
            generics::length::GenericMargin::Auto => LengthPercentageAuto::Auto,
            _ => LengthPercentageAuto::ZERO,
        }
    }

    fn inset(&self, v: &c::Inset) -> LengthPercentageAuto {
        match v {
            generics::position::GenericInset::LengthPercentage(lp) => self.lpa(lp),
            _ => LengthPercentageAuto::Auto,
        }
    }
}

fn display(d: c::Display) -> Display {
    use style::values::specified::box_::{DisplayInside as I, DisplayOutside as O};
    let inline = d.outside() == O::Inline;
    match d.inside() {
        I::None => Display::None,
        I::Contents => Display::Contents,
        I::Flow if d.is_list_item() => Display::ListItem,
        I::Flow if inline => Display::Inline,
        I::Flow if d.outside() == O::TableCaption => Display::TableCaption,
        I::Flow => Display::Block,
        I::FlowRoot if inline => Display::InlineBlock,
        I::FlowRoot if d.outside() == O::TableCaption => Display::TableCaption,
        I::FlowRoot if d.is_list_item() => Display::ListItem,
        I::FlowRoot => Display::FlowRoot,
        I::Flex if inline => Display::InlineFlex,
        I::Flex => Display::Flex,
        I::Grid | I::GridLanes if inline => Display::InlineGrid,
        I::Grid | I::GridLanes => Display::Grid,
        I::Table if inline => Display::InlineTable,
        I::Table => Display::Table,
        I::TableRowGroup => Display::TableRowGroup,
        I::TableHeaderGroup => Display::TableHeaderGroup,
        I::TableFooterGroup => Display::TableFooterGroup,
        I::TableRow => Display::TableRow,
        I::TableCell => Display::TableCell,
        I::TableColumn => Display::TableColumn,
        I::TableColumnGroup => Display::TableColumnGroup,
    }
}

fn border_style(s: style::values::specified::BorderStyle) -> BorderStyle {
    use style::values::specified::BorderStyle as B;
    match s {
        B::None => BorderStyle::None,
        B::Hidden => BorderStyle::Hidden,
        B::Solid => BorderStyle::Solid,
        B::Dashed => BorderStyle::Dashed,
        B::Dotted => BorderStyle::Dotted,
        B::Double => BorderStyle::Double,
        B::Groove => BorderStyle::Groove,
        B::Ridge => BorderStyle::Ridge,
        B::Inset => BorderStyle::Inset,
        B::Outset => BorderStyle::Outset,
    }
}

fn alignment(f: AlignFlags) -> Alignment {
    match f.value() {
        AlignFlags::START | AlignFlags::SELF_START => Alignment::Start,
        AlignFlags::END | AlignFlags::SELF_END => Alignment::End,
        AlignFlags::FLEX_START => Alignment::FlexStart,
        AlignFlags::FLEX_END => Alignment::FlexEnd,
        AlignFlags::CENTER | AlignFlags::ANCHOR_CENTER => Alignment::Center,
        AlignFlags::BASELINE | AlignFlags::LAST_BASELINE => Alignment::Baseline,
        AlignFlags::STRETCH => Alignment::Stretch,
        AlignFlags::SPACE_BETWEEN => Alignment::SpaceBetween,
        AlignFlags::SPACE_AROUND => Alignment::SpaceAround,
        AlignFlags::SPACE_EVENLY => Alignment::SpaceEvenly,
        AlignFlags::LEFT => Alignment::Left,
        AlignFlags::RIGHT => Alignment::Right,
        _ => Alignment::Normal,
    }
}

fn list_style_type(t: &c::ListStyleType) -> ListStyleType {
    use style::counter_style::CounterStyle;
    match &t.0 {
        CounterStyle::None => ListStyleType::None,
        CounterStyle::Name(name) => match &*name.0 {
            "disc" => ListStyleType::Disc,
            "circle" => ListStyleType::Circle,
            "square" | "disclosure-open" | "disclosure-closed" => ListStyleType::Square,
            "decimal" | "decimal-leading-zero" => ListStyleType::Decimal,
            "lower-alpha" | "lower-latin" => ListStyleType::LowerAlpha,
            "upper-alpha" | "upper-latin" => ListStyleType::UpperAlpha,
            "lower-roman" => ListStyleType::LowerRoman,
            "upper-roman" => ListStyleType::UpperRoman,
            "none" => ListStyleType::None,
            _ => ListStyleType::Decimal,
        },
        _ => ListStyleType::None,
    }
}

fn track_breadth(
    cx: &Cx,
    b: &generics::grid::GenericTrackBreadth<c::LengthPercentage>,
) -> TrackBreadth {
    use generics::grid::GenericTrackBreadth as B;
    match b {
        B::Breadth(lp) => TrackBreadth::Length(cx.lp(lp)),
        B::Flex(f) => TrackBreadth::Fr(f.0),
        B::Auto => TrackBreadth::Auto,
        B::MinContent => TrackBreadth::MinContent,
        B::MaxContent => TrackBreadth::MaxContent,
    }
}

fn track_size(cx: &Cx, s: &generics::grid::GenericTrackSize<c::LengthPercentage>) -> TrackSize {
    use generics::grid::{GenericTrackBreadth as B, GenericTrackSize as S};
    match s {
        S::Breadth(b) => TrackSize::Breadth(track_breadth(cx, b)),
        S::Minmax(a, b) => TrackSize::MinMax(track_breadth(cx, a), track_breadth(cx, b)),
        S::FitContent(B::Breadth(lp)) => TrackSize::FitContent(cx.lp(lp)),
        S::FitContent(_) => TrackSize::Breadth(TrackBreadth::Auto),
    }
}

fn names(list: &[style::values::CustomIdent]) -> Arc<[Arc<str>]> {
    list.iter().map(|n| Arc::from(&*n.0)).collect()
}

fn grid_template(cx: &Cx, t: &c::GridTemplateComponent) -> Option<Arc<[GridTrack]>> {
    use generics::grid::{
        GenericGridTemplateComponent as G, GenericTrackListValue as V, RepeatCount,
    };
    let G::TrackList(list) = t else { return None };
    let mut out = Vec::with_capacity(list.values.len() * 2 + 1);
    for (i, value) in list.values.iter().enumerate() {
        if let Some(n) = list.line_names.get(i).filter(|n| !n.is_empty()) {
            out.push(GridTrack::LineNames(names(n)));
        }
        match value {
            V::TrackSize(s) => out.push(GridTrack::Size(track_size(cx, s))),
            V::TrackRepeat(r) => {
                let count = match r.count {
                    RepeatCount::Number(n) => GridRepeat::Count(n.clamp(1, u16::MAX as i32) as u16),
                    RepeatCount::AutoFill => GridRepeat::AutoFill,
                    RepeatCount::AutoFit => GridRepeat::AutoFit,
                };
                let sizes: Arc<[TrackSize]> =
                    r.track_sizes.iter().map(|s| track_size(cx, s)).collect();
                out.push(GridTrack::Repeat(count, sizes));
            }
        }
    }
    if let Some(n) = list
        .line_names
        .get(list.values.len())
        .filter(|n| !n.is_empty())
    {
        out.push(GridTrack::LineNames(names(n)));
    }
    Some(out.into())
}

fn grid_line(l: &c::GridLine) -> GridLine {
    if l.is_auto() {
        return GridLine::Auto;
    }
    let has_name = !l.ident.0.is_empty();
    if l.is_span {
        if has_name {
            // `span name`: rare; keep the name.
            return GridLine::Named(Arc::from(&*l.ident.0));
        }
        return GridLine::Span(l.line_num.clamp(1, u16::MAX as i32) as u16);
    }
    if has_name {
        return GridLine::Named(Arc::from(&*l.ident.0));
    }
    GridLine::Line(l.line_num.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
}

fn implicit_tracks(cx: &Cx, t: &c::ImplicitGridTracks) -> Option<Arc<[TrackSize]>> {
    if t.0.is_empty() {
        return None;
    }
    Some(t.0.iter().map(|s| track_size(cx, s)).collect())
}

/// One `background-image` / `mask-image` layer we can draw.
fn image_layer(cx: &Cx, img: &c::Image, s: &ComputedValues) -> Option<BackgroundImage> {
    match img {
        generics::image::GenericImage::Url(u) => {
            u.url().map(|u| BackgroundImage::Url(Arc::from(u.as_str())))
        }
        generics::image::GenericImage::Gradient(g) => gradient(cx, g, s),
        _ => None,
    }
}

/// A gradient whose color stops are all fully transparent paints nothing.
fn is_transparent(l: &BackgroundImage) -> bool {
    match l {
        BackgroundImage::LinearGradient(g) | BackgroundImage::RadialGradient(g) => {
            g.stops.iter().all(|s| s.color.a == 0)
        }
        BackgroundImage::Url(_) => false,
    }
}

/// `background-repeat` / `mask-repeat` (first layer).
fn repeat(r: Option<&c::BackgroundRepeat>) -> BackgroundRepeat {
    use style::values::specified::background::BackgroundRepeatKeyword as K;
    match r.map(|r| (r.0, r.1)) {
        Some((K::NoRepeat, K::NoRepeat)) => BackgroundRepeat::NoRepeat,
        Some((K::NoRepeat, _)) => BackgroundRepeat::RepeatY,
        Some((_, K::NoRepeat)) => BackgroundRepeat::RepeatX,
        _ => BackgroundRepeat::Repeat,
    }
}

/// `background-size` / `mask-size` (first layer).
fn size(cx: &Cx, v: Option<&c::BackgroundSize>) -> BackgroundSize {
    match v {
        Some(generics::background::GenericBackgroundSize::Cover) => BackgroundSize::Cover,
        Some(generics::background::GenericBackgroundSize::Contain) => BackgroundSize::Contain,
        Some(generics::background::GenericBackgroundSize::ExplicitSize { width, height }) => {
            let side = |v: &generics::length::GenericLengthPercentageOrAuto<
                c::NonNegativeLengthPercentage,
            >| match v {
                generics::length::GenericLengthPercentageOrAuto::LengthPercentage(lp) => {
                    cx.lpa(&lp.0)
                }
                generics::length::GenericLengthPercentageOrAuto::Auto => LengthPercentageAuto::Auto,
            };
            match (side(width), side(height)) {
                (LengthPercentageAuto::Auto, LengthPercentageAuto::Auto) => BackgroundSize::Auto,
                (w, h) => BackgroundSize::Explicit(w, h),
            }
        }
        None => BackgroundSize::Auto,
    }
}

fn gradient(cx: &Cx, g: &c::Gradient, style: &ComputedValues) -> Option<BackgroundImage> {
    use generics::image::{
        GenericEndingShape as E, GenericGradient as G, GenericGradientItem as Item,
    };
    use style::values::specified::position::{
        HorizontalPositionKeyword as H, VerticalPositionKeyword as V,
    };
    let stops = |items: &[generics::image::GenericGradientItem<
        c::Color,
        c::LengthPercentage,
    >]|
     -> Vec<ColorStop> {
        items
            .iter()
            .filter_map(|i| match i {
                Item::SimpleColorStop(col) => Some(ColorStop {
                    color: color(&style.resolve_color(col)),
                    position: None,
                }),
                Item::ComplexColorStop {
                    color: col,
                    position,
                } => Some(ColorStop {
                    color: color(&style.resolve_color(col)),
                    position: Some(cx.lp(position)),
                }),
                Item::InterpolationHint(_) => None,
            })
            .collect()
    };
    match g {
        G::Linear {
            direction,
            items,
            flags,
            ..
        } => {
            use c::image::LineDirection as D;
            let (angle_deg, corner) = match direction {
                D::Angle(a) => (a.degrees(), None),
                D::Horizontal(H::Left) => (270.0, None),
                D::Horizontal(H::Right) => (90.0, None),
                D::Vertical(V::Top) => (0.0, None),
                D::Vertical(V::Bottom) => (180.0, None),
                D::Corner(h, v) => {
                    let right = *h == H::Right;
                    let bottom = *v == V::Bottom;
                    let angle = match (right, bottom) {
                        (true, true) => 135.0,
                        (true, false) => 45.0,
                        (false, true) => 225.0,
                        (false, false) => 315.0,
                    };
                    (angle, Some((right, bottom)))
                }
            };
            let stops = stops(items);
            (stops.len() >= 2).then(|| {
                BackgroundImage::LinearGradient(Arc::new(Gradient {
                    repeating: flags.contains(generics::image::GradientFlags::REPEATING),
                    shape: GradientShape::Linear { angle_deg, corner },
                    stops,
                }))
            })
        }
        G::Radial {
            shape,
            position,
            items,
            flags,
            ..
        } => {
            use generics::image::{GenericCircle as Ci, GenericEllipse as El, ShapeExtent as X};
            let ext = |x: &X| match x {
                X::ClosestSide | X::Contain => RadialExtent::ClosestSide,
                X::ClosestCorner => RadialExtent::ClosestCorner,
                X::FarthestSide => RadialExtent::FarthestSide,
                X::FarthestCorner | X::Cover => RadialExtent::FarthestCorner,
            };
            let (circle, extent) = match shape {
                E::Circle(Ci::Radius(r)) => (
                    true,
                    RadialExtent::Explicit(
                        LengthPercentage::Px(r.0.px()),
                        LengthPercentage::Px(r.0.px()),
                    ),
                ),
                E::Circle(Ci::Extent(x)) => (true, ext(x)),
                E::Ellipse(El::Radii(a, b)) => {
                    (false, RadialExtent::Explicit(cx.lp(&a.0), cx.lp(&b.0)))
                }
                E::Ellipse(El::Extent(x)) => (false, ext(x)),
            };
            let stops = stops(items);
            (stops.len() >= 2).then(|| {
                BackgroundImage::RadialGradient(Arc::new(Gradient {
                    repeating: flags.contains(generics::image::GradientFlags::REPEATING),
                    shape: GradientShape::Radial {
                        circle,
                        extent,
                        center: (cx.lp(&position.horizontal), cx.lp(&position.vertical)),
                    },
                    stops,
                }))
            })
        }
        _ => None,
    }
}

fn transform_ops(cx: &Cx, s: &ComputedValues) -> Option<Arc<[TransformOp]>> {
    use generics::transform::{
        GenericRotate as R, GenericScale as Sc, GenericTransformOperation as T,
        GenericTranslate as Tr,
    };
    let b = s.get_box();
    let mut ops = Vec::new();
    if let Tr::Translate(x, y, _) = &b.translate {
        ops.push(TransformOp::Translate(cx.lp(x), cx.lp(y)));
    }
    match &b.rotate {
        R::Rotate(a) => ops.push(TransformOp::Rotate(a.degrees())),
        R::Rotate3D(_, _, z, a) if *z != 0.0 => {
            ops.push(TransformOp::Rotate(a.degrees() * z.signum()))
        }
        _ => {}
    }
    if let Sc::Scale(x, y, _) = &b.scale {
        ops.push(TransformOp::Scale(*x, *y));
    }
    for op in b.transform.0.iter() {
        ops.push(match op {
            T::Matrix(m) => TransformOp::Matrix([m.a, m.b, m.c, m.d, m.e, m.f]),
            T::Matrix3D(m) => TransformOp::Matrix([m.m11, m.m12, m.m21, m.m22, m.m41, m.m42]),
            T::Skew(x, y) => TransformOp::Skew(x.degrees(), y.degrees()),
            T::SkewX(x) => TransformOp::Skew(x.degrees(), 0.0),
            T::SkewY(y) => TransformOp::Skew(0.0, y.degrees()),
            T::Translate(x, y) => TransformOp::Translate(cx.lp(x), cx.lp(y)),
            T::TranslateX(x) => TransformOp::Translate(cx.lp(x), LengthPercentage::ZERO),
            T::TranslateY(y) => TransformOp::Translate(LengthPercentage::ZERO, cx.lp(y)),
            T::Translate3D(x, y, _) => TransformOp::Translate(cx.lp(x), cx.lp(y)),
            T::Scale(x, y) => TransformOp::Scale(*x, *y),
            T::ScaleX(x) => TransformOp::Scale(*x, 1.0),
            T::ScaleY(y) => TransformOp::Scale(1.0, *y),
            T::Scale3D(x, y, _) => TransformOp::Scale(*x, *y),
            T::Rotate(a) | T::RotateZ(a) => TransformOp::Rotate(a.degrees()),
            T::Rotate3D(_, _, z, a) if *z != 0.0 => TransformOp::Rotate(a.degrees() * z.signum()),
            _ => continue,
        });
    }
    (!ops.is_empty()).then(|| ops.into())
}

/// `content` on a pseudo-element (`attr()` is already substituted by Stylo
/// through `TElement::get_attr`). Alternative text after `/` is for
/// accessibility only and is not rendered (CSS Content 3, section 1.3).
fn content(s: &ComputedValues) -> Content {
    use generics::counters::{GenericContent as G, GenericContentItem as I};
    let G::Items(items) = &s.get_counters().content else {
        return Content::None;
    };
    let mut out: Vec<ContentItem> = Vec::new();
    let mut text = String::new();
    for item in items.items.iter().take(items.alt_start) {
        match item {
            I::String(st) => text.push_str(st),
            I::OpenQuote => text.push('\u{201C}'),
            I::CloseQuote => text.push('\u{201D}'),
            I::Image(generics::image::GenericImage::Url(u)) => {
                if let Some(u) = u.url() {
                    if !text.is_empty() {
                        out.push(ContentItem::Text(Arc::from(std::mem::take(&mut text))));
                    }
                    out.push(ContentItem::Image(Arc::from(u.as_str())));
                }
            }
            // Counters, gradients and the rest: nothing (layout still gets a box).
            _ => {}
        }
    }
    if out.is_empty() {
        return Content::Text(Arc::from(text));
    }
    if !text.is_empty() {
        out.push(ContentItem::Text(Arc::from(text)));
    }
    Content::Items(out.into())
}

/// Convert one Stylo style.
pub(crate) fn convert(cx: &Cx, s: &ComputedValues) -> ComputedStyle {
    let bx = s.get_box();
    let pos = s.get_position();
    let border = s.get_border();
    let margin = s.get_margin();
    let padding = s.get_padding();
    let bg = s.get_background();
    let font = s.get_font();
    let itext = s.get_inherited_text();
    let text = s.get_text();
    let effects = s.get_effects();
    let outline = s.get_outline();
    let list = s.get_list();

    let resolve = |col: &c::Color| color(&s.resolve_color(col));
    // The topmost background layer that paints something. A gradient whose
    // stops are all transparent draws nothing (the common
    // `linear-gradient(transparent, transparent), url(sprite.svg)` fallback),
    // so the layer under it shows. Its repeat, size and position are the
    // entries for that layer; shorter lists repeat (CSS Backgrounds 3, 2.2).
    let (bg_layer, bg_image) = bg
        .background_image
        .0
        .iter()
        .enumerate()
        .find_map(|(i, img)| {
            image_layer(cx, img, s)
                .filter(|l| !is_transparent(l))
                .map(|l| (i, Some(l)))
        })
        .unwrap_or((0, None));
    let nth = |len: usize| (len > 0).then(|| bg_layer % len);

    let mut out = ComputedStyle {
        display: display(bx.display),
        original_display: display(bx.original_display),
        position: match bx.position {
            computed_values::position::T::Static => Position::Static,
            computed_values::position::T::Relative => Position::Relative,
            computed_values::position::T::Absolute => Position::Absolute,
            computed_values::position::T::Fixed => Position::Fixed,
            computed_values::position::T::Sticky => Position::Sticky,
        },
        float: match bx.float {
            c::Float::Left => Float::Left,
            c::Float::Right => Float::Right,
            c::Float::None => Float::None,
            c::Float::InlineStart => Float::Left,
            c::Float::InlineEnd => Float::Right,
        },
        clear: match bx.clear {
            c::Clear::None => Clear::None,
            c::Clear::Left | c::Clear::InlineStart => Clear::Left,
            c::Clear::Right | c::Clear::InlineEnd => Clear::Right,
            c::Clear::Both => Clear::Both,
        },
        box_sizing: match pos.box_sizing {
            computed_values::box_sizing::T::BorderBox => BoxSizing::BorderBox,
            computed_values::box_sizing::T::ContentBox => BoxSizing::ContentBox,
        },
        inset: Edges {
            top: cx.inset(&pos.top),
            right: cx.inset(&pos.right),
            bottom: cx.inset(&pos.bottom),
            left: cx.inset(&pos.left),
        },
        z_index: match pos.z_index {
            generics::position::GenericZIndex::Integer(i) => Some(i),
            generics::position::GenericZIndex::Auto => None,
        },
        overflow_x: overflow(bx.overflow_x),
        overflow_y: overflow(bx.overflow_y),
        visibility: match s.get_inherited_box().visibility {
            c::Visibility::Visible => Visibility::Visible,
            c::Visibility::Hidden => Visibility::Hidden,
            c::Visibility::Collapse => Visibility::Collapse,
        },
        pointer_events: match s.get_inherited_ui().pointer_events {
            c::PointerEvents::None => PointerEvents::None,
            _ => PointerEvents::Auto,
        },
        opacity: effects.opacity,
        width: cx.size(&pos.width),
        height: cx.size(&pos.height),
        min_width: match cx.size(&pos.min_width) {
            LengthPercentageAuto::Auto => LengthPercentageAuto::Auto,
            v => v,
        },
        min_height: cx.size(&pos.min_height),
        max_width: cx.max_size(&pos.max_width),
        max_height: cx.max_size(&pos.max_height),
        aspect_ratio: match &pos.aspect_ratio.ratio {
            generics::position::PreferredRatio::Ratio(r) if r.1.0 > 0.0 => Some(r.0.0 / r.1.0),
            _ => None,
        },
        margin: Edges {
            top: cx.margin(&margin.margin_top),
            right: cx.margin(&margin.margin_right),
            bottom: cx.margin(&margin.margin_bottom),
            left: cx.margin(&margin.margin_left),
        },
        padding: Edges {
            top: cx.lp(&padding.padding_top.0),
            right: cx.lp(&padding.padding_right.0),
            bottom: cx.lp(&padding.padding_bottom.0),
            left: cx.lp(&padding.padding_left.0),
        },
        border_style: Edges {
            top: border_style(border.border_top_style),
            right: border_style(border.border_right_style),
            bottom: border_style(border.border_bottom_style),
            left: border_style(border.border_left_style),
        },
        border_width: Edges {
            top: border.border_top_width.0.to_f32_px(),
            right: border.border_right_width.0.to_f32_px(),
            bottom: border.border_bottom_width.0.to_f32_px(),
            left: border.border_left_width.0.to_f32_px(),
        },
        border_color: Edges {
            top: resolve(&border.border_top_color),
            right: resolve(&border.border_right_color),
            bottom: resolve(&border.border_bottom_color),
            left: resolve(&border.border_left_color),
        },
        border_radius: Corners {
            top_left: cx.lp(&border.border_top_left_radius.0.width.0),
            top_right: cx.lp(&border.border_top_right_radius.0.width.0),
            bottom_right: cx.lp(&border.border_bottom_right_radius.0.width.0),
            bottom_left: cx.lp(&border.border_bottom_left_radius.0.width.0),
        },
        background_color: resolve(&bg.background_color),
        background_image: bg_image,
        background_repeat: repeat(
            nth(bg.background_repeat.0.len()).map(|i| &bg.background_repeat.0[i]),
        ),
        mask: {
            let svg = s.get_svg();
            svg.mask_image
                .0
                .iter()
                .find_map(|img| image_layer(cx, img, s))
                .map(|image| {
                    Arc::new(Mask {
                        image,
                        size: size(cx, svg.mask_size.0.first()),
                        position: (
                            svg.mask_position_x
                                .0
                                .first()
                                .map_or(LengthPercentage::Percent(0.0), |x| cx.lp(x)),
                            svg.mask_position_y
                                .0
                                .first()
                                .map_or(LengthPercentage::Percent(0.0), |y| cx.lp(y)),
                        ),
                        repeat: repeat(svg.mask_repeat.0.first()),
                    })
                })
        },
        flex_direction: match pos.flex_direction {
            computed_values::flex_direction::T::Row => FlexDirection::Row,
            computed_values::flex_direction::T::RowReverse => FlexDirection::RowReverse,
            computed_values::flex_direction::T::Column => FlexDirection::Column,
            computed_values::flex_direction::T::ColumnReverse => FlexDirection::ColumnReverse,
        },
        flex_wrap: {
            use style::values::computed::FlexWrap as W;
            if pos.flex_wrap.contains(W::WRAP_REVERSE) {
                FlexWrap::WrapReverse
            } else if pos.flex_wrap.contains(W::WRAP) {
                FlexWrap::Wrap
            } else {
                FlexWrap::NoWrap
            }
        },
        flex_grow: pos.flex_grow.0,
        flex_shrink: pos.flex_shrink.0,
        flex_basis: match &pos.flex_basis {
            generics::flex::GenericFlexBasis::Content => LengthPercentageAuto::Auto,
            generics::flex::GenericFlexBasis::Size(size) => cx.size(size),
        },
        order: pos.order,
        justify_content: alignment(pos.justify_content.primary()),
        align_items: alignment(pos.align_items.0),
        align_self: alignment(pos.align_self.0),
        align_content: alignment(pos.align_content.primary()),
        row_gap: gap(cx, &pos.row_gap),
        column_gap: gap(cx, &pos.column_gap),
        color: color(&itext.color),
        font_family: Arc::from(style_traits::ToCss::to_css_string(&font.font_family)),
        font_size: font.font_size.computed_size().px(),
        font_weight: font.font_weight.value(),
        font_style: if font.font_style == c::FontStyle::NORMAL {
            FontStyle::Normal
        } else if font.font_style == c::FontStyle::ITALIC {
            FontStyle::Italic
        } else {
            FontStyle::Oblique
        },
        line_height: match &font.line_height {
            generics::font::GenericLineHeight::Normal => LineHeight::Normal,
            generics::font::GenericLineHeight::Number(n) => LineHeight::Number(n.0),
            generics::font::GenericLineHeight::Length(l) => LineHeight::Px(l.0.px()),
        },
        text_align: {
            use c::TextAlign as T;
            match itext.text_align {
                T::Start => TextAlign::Start,
                T::End => TextAlign::End,
                T::Left => TextAlign::Left,
                T::Right => TextAlign::Right,
                T::Center => TextAlign::Center,
                T::Justify => TextAlign::Justify,
                T::MozCenter => TextAlign::WebkitCenter,
                T::MozLeft => TextAlign::WebkitLeft,
                T::MozRight => TextAlign::WebkitRight,
            }
        },
        text_indent: cx.lp(&itext.text_indent.length),
        text_transform: {
            use style::values::specified::text::TextTransform as T;
            let t = itext.text_transform;
            if t.contains(T::UPPERCASE) {
                TextTransform::Uppercase
            } else if t.contains(T::LOWERCASE) {
                TextTransform::Lowercase
            } else if t.contains(T::CAPITALIZE) {
                TextTransform::Capitalize
            } else {
                TextTransform::None
            }
        },
        white_space: {
            use style::values::specified::text::{TextWrapMode as W, WhiteSpaceCollapse as C};
            let wrap = itext.text_wrap_mode == W::Wrap;
            match (itext.white_space_collapse, wrap) {
                (C::Collapse, true) => WhiteSpace::Normal,
                (C::Collapse, false) => WhiteSpace::Nowrap,
                (C::PreserveBreaks, _) => WhiteSpace::PreLine,
                (C::BreakSpaces, _) => WhiteSpace::BreakSpaces,
                (_, true) => WhiteSpace::PreWrap,
                (_, false) => WhiteSpace::Pre,
            }
        },
        text_wrap_balance: itext.text_wrap_style == style::values::computed::TextWrapStyle::Balance,
        letter_spacing: itext
            .letter_spacing
            .0
            .resolve(font.font_size.computed_size())
            .px(),
        word_spacing: itext
            .word_spacing
            .resolve(font.font_size.computed_size())
            .px(),
        list_style_type: list_style_type(&list.list_style_type),
        list_style_position: match list.list_style_position {
            computed_values::list_style_position::T::Inside => ListStylePosition::Inside,
            computed_values::list_style_position::T::Outside => ListStylePosition::Outside,
        },
        text_decoration_line: {
            use style::values::specified::text::TextDecorationLine as L;
            let l = text.text_decoration_line;
            TextDecorationLine {
                underline: l.contains(L::UNDERLINE),
                overline: l.contains(L::OVERLINE),
                line_through: l.contains(L::LINE_THROUGH),
            }
        },
        text_decoration_color: resolve(&text.text_decoration_color),
        vertical_align: vertical_align(s),
        content: Content::None,
        background_size: size(
            cx,
            nth(bg.background_size.0.len()).map(|i| &bg.background_size.0[i]),
        ),
        background_position: (
            nth(bg.background_position_x.0.len()).map_or(LengthPercentage::Percent(0.0), |i| {
                cx.lp(&bg.background_position_x.0[i])
            }),
            nth(bg.background_position_y.0.len()).map_or(LengthPercentage::Percent(0.0), |i| {
                cx.lp(&bg.background_position_y.0[i])
            }),
        ),
        box_shadow: {
            let list = &effects.box_shadow.0;
            (!list.is_empty()).then(|| {
                list.iter()
                    .map(|sh| BoxShadow {
                        offset_x: px(sh.base.horizontal),
                        offset_y: px(sh.base.vertical),
                        blur: sh.base.blur.0.px(),
                        spread: px(sh.spread),
                        color: resolve(&sh.base.color),
                        inset: sh.inset,
                    })
                    .collect()
            })
        },
        transform: transform_ops(cx, s),
        transform_origin: (
            cx.lp(&bx.transform_origin.horizontal),
            cx.lp(&bx.transform_origin.vertical),
        ),
        outline_width: outline.outline_width.0.to_f32_px(),
        outline_style: match outline.outline_style {
            style::values::specified::OutlineStyle::Auto => BorderStyle::Solid,
            style::values::specified::OutlineStyle::BorderStyle(b) => border_style(b),
        },
        outline_color: resolve(&outline.outline_color),
        outline_offset: outline.outline_offset.to_f32_px(),
        object_fit: {
            use computed_values::object_fit::T as O;
            match pos.object_fit {
                O::Fill => ObjectFit::Fill,
                O::Contain => ObjectFit::Contain,
                O::Cover => ObjectFit::Cover,
                O::None => ObjectFit::None,
                O::ScaleDown => ObjectFit::ScaleDown,
            }
        },
        text_overflow: match text.text_overflow.second {
            style::values::specified::text::TextOverflowSide::Ellipsis => TextOverflow::Ellipsis,
            _ => TextOverflow::Clip,
        },
        border_collapse: match s.get_inherited_table().border_collapse {
            computed_values::border_collapse::T::Collapse => BorderCollapse::Collapse,
            computed_values::border_collapse::T::Separate => BorderCollapse::Separate,
        },
        border_spacing: {
            let sp = &s.get_inherited_table().border_spacing.0;
            (sp.width.0.px(), sp.height.0.px())
        },
        table_layout: match s.get_table().table_layout {
            computed_values::table_layout::T::Fixed => TableLayout::Fixed,
            computed_values::table_layout::T::Auto => TableLayout::Auto,
        },
        caption_side: match s.get_inherited_table().caption_side {
            computed_values::caption_side::T::Bottom => CaptionSide::Bottom,
            computed_values::caption_side::T::Top => CaptionSide::Top,
        },
        grid_template_columns: grid_template(cx, &pos.grid_template_columns),
        grid_template_rows: grid_template(cx, &pos.grid_template_rows),
        grid_template_areas: match &pos.grid_template_areas {
            c::position::GridTemplateAreas::Areas(areas) => Some(
                areas
                    .0
                    .areas
                    .iter()
                    .map(|a| GridArea {
                        name: Arc::from(&*a.name),
                        row_start: a.rows.start as u16,
                        row_end: a.rows.end as u16,
                        column_start: a.columns.start as u16,
                        column_end: a.columns.end as u16,
                    })
                    .collect(),
            ),
            c::position::GridTemplateAreas::None => None,
        },
        grid_auto_columns: implicit_tracks(cx, &pos.grid_auto_columns),
        grid_auto_rows: implicit_tracks(cx, &pos.grid_auto_rows),
        grid_auto_flow: {
            use style::values::specified::position::GridAutoFlow as F;
            let f = pos.grid_auto_flow;
            match (f.contains(F::COLUMN), f.contains(F::DENSE)) {
                (false, false) => GridAutoFlow::Row,
                (true, false) => GridAutoFlow::Column,
                (false, true) => GridAutoFlow::RowDense,
                (true, true) => GridAutoFlow::ColumnDense,
            }
        },
        grid_column: (
            grid_line(&pos.grid_column_start),
            grid_line(&pos.grid_column_end),
        ),
        grid_row: (grid_line(&pos.grid_row_start), grid_line(&pos.grid_row_end)),
        justify_items: alignment(pos.justify_items.computed.0.0),
        justify_self: alignment(pos.justify_self.0),
        fill: svg_paint(s, &s.get_inherited_svg().fill),
        stroke: svg_paint(s, &s.get_inherited_svg().stroke),
        clip_path: clip_path(cx, &s.get_svg().clip_path).or_else(|| {
            matches!(
                bx.position,
                computed_values::position::T::Absolute | computed_values::position::T::Fixed
            )
            .then(|| clip_rect(&effects.clip))
            .flatten()
        }),
        filter: filters(s, &effects.filter.0),
        backdrop_filter: filters(s, &effects.backdrop_filter.0),
        mix_blend_mode: blend_mode(effects.mix_blend_mode),
        isolation: match bx.isolation {
            c::Isolation::Auto => Isolation::Auto,
            c::Isolation::Isolate => Isolation::Isolate,
        },
        font_feature_settings: {
            let list = &font.font_feature_settings.0;
            (!list.is_empty()).then(|| {
                list.iter()
                    .map(|f| FontFeature {
                        tag: f.tag.0.to_be_bytes(),
                        value: f.value.max(0) as u32,
                    })
                    .collect()
            })
        },
    };
    // A border with style none/hidden has zero width.
    for (w, st) in [
        (&mut out.border_width.top, out.border_style.top),
        (&mut out.border_width.right, out.border_style.right),
        (&mut out.border_width.bottom, out.border_style.bottom),
        (&mut out.border_width.left, out.border_style.left),
    ] {
        if !st.is_visible() {
            *w = 0.0;
        }
    }
    if !out.outline_style.is_visible() {
        out.outline_width = 0.0;
    }
    if s.is_pseudo_style() {
        out.content = content(s);
    }
    out
}

fn svg_paint(s: &ComputedValues, p: &c::SVGPaint) -> Option<Color> {
    use generics::svg::{GenericSVGPaintFallback as F, GenericSVGPaintKind as K};
    match &p.kind {
        K::None => None,
        K::Color(col) => Some(color(&s.resolve_color(col))),
        _ => match &p.fallback {
            F::Color(col) => Some(color(&s.resolve_color(col))),
            _ => None,
        },
    }
}

/// A `filter` / `backdrop-filter` list; `None` for `none`.
fn filters(s: &ComputedValues, list: &[c::Filter]) -> Option<Arc<[FilterFunction]>> {
    use generics::effects::GenericFilter as F;
    if list.is_empty() {
        return None;
    }
    Some(
        list.iter()
            .map(|f| match f {
                F::Blur(l) => FilterFunction::Blur(l.0.px()),
                F::Brightness(v) => FilterFunction::Brightness(v.0),
                F::Contrast(v) => FilterFunction::Contrast(v.0),
                F::Saturate(v) => FilterFunction::Saturate(v.0),
                F::Grayscale(v) => FilterFunction::Grayscale(v.0.min(1.0)),
                F::Invert(v) => FilterFunction::Invert(v.0.min(1.0)),
                F::Sepia(v) => FilterFunction::Sepia(v.0.min(1.0)),
                F::Opacity(v) => FilterFunction::Opacity(v.0.min(1.0)),
                F::HueRotate(a) => FilterFunction::HueRotate(a.degrees()),
                F::DropShadow(sh) => FilterFunction::DropShadow {
                    offset_x: px(sh.horizontal),
                    offset_y: px(sh.vertical),
                    blur: sh.blur.0.px(),
                    color: color(&s.resolve_color(&sh.color)),
                },
                F::Url(u) => match *u {},
            })
            .collect(),
    )
}

fn blend_mode(b: c::effects::Blend) -> BlendMode {
    use c::effects::Blend as B;
    match b {
        B::Normal => BlendMode::Normal,
        B::Multiply => BlendMode::Multiply,
        B::Screen => BlendMode::Screen,
        B::Overlay => BlendMode::Overlay,
        B::Darken => BlendMode::Darken,
        B::Lighten => BlendMode::Lighten,
        B::ColorDodge => BlendMode::ColorDodge,
        B::ColorBurn => BlendMode::ColorBurn,
        B::HardLight => BlendMode::HardLight,
        B::SoftLight => BlendMode::SoftLight,
        B::Difference => BlendMode::Difference,
        B::Exclusion => BlendMode::Exclusion,
        B::Hue => BlendMode::Hue,
        B::Saturation => BlendMode::Saturation,
        B::Color => BlendMode::Color,
        B::Luminosity => BlendMode::Luminosity,
        B::PlusLighter => BlendMode::PlusLighter,
    }
}

/// The legacy `clip: rect(top, right, bottom, left)` of an absolutely
/// positioned box (CSS Masking 1, 6.1) as an `inset()` clip: top and bottom
/// are offsets from the top border edge, left and right from the left one;
/// `auto` is the border box edge. `rect(1px, 1px, 1px, 1px)` (visually
/// hidden text) has no area, so nothing shows. A box with `clip-path` keeps
/// that instead.
fn clip_rect(clip: &c::ClipRectOrAuto) -> Option<Arc<ClipPath>> {
    use generics::GenericClipRectOrAuto as R;
    use generics::length::GenericLengthPercentageOrAuto as L;
    let R::Rect(r) = clip else { return None };
    let px = |v: &c::LengthOrAuto| match v {
        L::LengthPercentage(l) => Some(l.px()),
        L::Auto => None,
    };
    // From the far edge: `calc(100% - offset)`, or 0 for `auto`.
    let far = |v: &c::LengthOrAuto| match px(v) {
        Some(p) => LengthPercentage::Calc(CalcLength {
            px: -p,
            percent: 100.0,
        }),
        None => LengthPercentage::ZERO,
    };
    let near = |v: &c::LengthOrAuto| LengthPercentage::Px(px(v).unwrap_or(0.0));
    Some(Arc::new(ClipPath::Inset {
        inset: Edges {
            top: near(&r.top),
            right: far(&r.right),
            bottom: far(&r.bottom),
            left: near(&r.left),
        },
        radius: Corners {
            top_left: LengthPercentage::ZERO,
            top_right: LengthPercentage::ZERO,
            bottom_right: LengthPercentage::ZERO,
            bottom_left: LengthPercentage::ZERO,
        },
    }))
}

fn clip_path(cx: &Cx, p: &c::basic_shape::ClipPath) -> Option<Arc<ClipPath>> {
    use generics::basic_shape::{
        GenericBasicShape as B, GenericClipPath as P, GenericShapeRadius as R,
    };
    let P::Shape(shape, _) = p else { return None };
    let radius = |r: &c::basic_shape::ShapeRadius| match r {
        R::Length(l) => ShapeRadius::Length(cx.lp(&l.0)),
        R::ClosestSide => ShapeRadius::ClosestSide,
        R::FarthestSide => ShapeRadius::FarthestSide,
        R::ClosestCorner => ShapeRadius::ClosestCorner,
        R::FarthestCorner => ShapeRadius::FarthestCorner,
    };
    let center = |pos: &generics::position::GenericPositionOrAuto<c::Position>| match pos {
        generics::position::GenericPositionOrAuto::Position(p) => {
            (cx.lp(&p.horizontal), cx.lp(&p.vertical))
        }
        generics::position::GenericPositionOrAuto::Auto => (
            LengthPercentage::Percent(50.0),
            LengthPercentage::Percent(50.0),
        ),
    };
    let shape = match &**shape {
        B::Rect(r) => ClipPath::Inset {
            inset: Edges {
                top: cx.lp(&r.rect.0),
                right: cx.lp(&r.rect.1),
                bottom: cx.lp(&r.rect.2),
                left: cx.lp(&r.rect.3),
            },
            radius: Corners {
                top_left: cx.lp(&r.round.top_left.0.width.0),
                top_right: cx.lp(&r.round.top_right.0.width.0),
                bottom_right: cx.lp(&r.round.bottom_right.0.width.0),
                bottom_left: cx.lp(&r.round.bottom_left.0.width.0),
            },
        },
        B::Circle(c) => ClipPath::Circle {
            radius: radius(&c.radius),
            center: center(&c.position),
        },
        B::Ellipse(e) => ClipPath::Ellipse {
            radius_x: radius(&e.semiaxis_x),
            radius_y: radius(&e.semiaxis_y),
            center: center(&e.position),
        },
        B::Polygon(poly) => ClipPath::Polygon {
            even_odd: poly.fill == generics::basic_shape::FillRule::Evenodd,
            points: poly
                .coordinates
                .iter()
                .map(|pt| (cx.lp(&pt.0), cx.lp(&pt.1)))
                .collect(),
        },
        B::PathOrShape(_) => return None,
    };
    Some(Arc::new(shape))
}

fn overflow(o: c::Overflow) -> Overflow {
    match o {
        c::Overflow::Visible => Overflow::Visible,
        c::Overflow::Hidden => Overflow::Hidden,
        c::Overflow::Scroll => Overflow::Scroll,
        c::Overflow::Auto => Overflow::Auto,
        c::Overflow::Clip => Overflow::Clip,
    }
}

fn gap(cx: &Cx, g: &c::length::NonNegativeLengthPercentageOrNormal) -> LengthPercentage {
    match g {
        generics::length::GenericLengthPercentageOrNormal::LengthPercentage(lp) => cx.lp(&lp.0),
        generics::length::GenericLengthPercentageOrNormal::Normal => LengthPercentage::ZERO,
    }
}

fn vertical_align(s: &ComputedValues) -> VerticalAlign {
    use generics::box_::{BaselineShiftKeyword as K, GenericBaselineShift as B};
    use style::values::specified::box_::AlignmentBaseline as A;
    let bx = s.get_box();
    match &bx.baseline_shift {
        B::Keyword(K::Sub) => return VerticalAlign::Sub,
        B::Keyword(K::Super) => return VerticalAlign::Super,
        B::Keyword(K::Top) => return VerticalAlign::Top,
        B::Keyword(K::Bottom) => return VerticalAlign::Bottom,
        B::Keyword(K::Center) => return VerticalAlign::Middle,
        B::Length(_) => {}
    }
    match bx.alignment_baseline {
        A::Middle => VerticalAlign::Middle,
        A::TextTop => VerticalAlign::TextTop,
        A::TextBottom => VerticalAlign::TextBottom,
        _ => VerticalAlign::Baseline,
    }
}
