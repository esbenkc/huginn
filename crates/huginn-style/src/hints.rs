//! Presentational hints: legacy HTML attributes that map to CSS
//! (HTML Standard, "Rendering", and Chrome's behavior where it differs).
//!
//! The hints for one element are written as CSS declarations and parsed by
//! Stylo like a `style` attribute. They cascade at the presentational-hint
//! level: above the UA sheet, below every author rule.

use std::fmt::Write;

use huginn_dom::{Document, ElementData, NodeId, local_name, ns};
use style::color::AbsoluteColor;
use style::servo::attr::{
    LengthOrPercentageOrAuto, parse_legacy_color, parse_length, parse_nonzero_length,
};

/// The hint declarations of `id`, or `None` if it has none.
pub(crate) fn hint_css(doc: &Document, id: NodeId) -> Option<String> {
    let el = doc.element(id)?;
    if el.name.ns == ns!(svg) {
        return svg_hint_css(doc, id, el);
    }
    if el.name.ns != ns!(html) {
        return None;
    }
    let mut css = String::new();
    let tag = &*el.name.local;
    let attr = |name| el.attr(&name);

    // Colors and backgrounds.
    if matches!(
        tag,
        "body" | "table" | "thead" | "tbody" | "tfoot" | "tr" | "td" | "th"
    ) && let Some(c) = attr(local_name!("bgcolor")).and_then(legacy_color)
    {
        let _ = write!(css, "background-color:{c};");
    }
    if matches!(
        tag,
        "body" | "table" | "thead" | "tbody" | "tfoot" | "tr" | "td" | "th"
    ) && let Some(url) = attr(local_name!("background")).filter(|u| !u.trim().is_empty())
    {
        let _ = write!(css, "background-image:url(\"{}\");", css_escape(url.trim()));
    }
    if tag == "body"
        && let Some(c) = attr(local_name!("text")).and_then(legacy_color)
    {
        let _ = write!(css, "color:{c};");
    }
    if tag == "font" {
        if let Some(c) = attr(local_name!("color")).and_then(legacy_color) {
            let _ = write!(css, "color:{c};");
        }
        if let Some(face) = attr(local_name!("face")).filter(|f| !f.trim().is_empty()) {
            let _ = write!(css, "font-family:{};", font_face_list(face));
        }
        if let Some(size) = attr(local_name!("size")).and_then(font_size_keyword) {
            let _ = write!(css, "font-size:{size};");
        }
    }
    // <a> inherits the body's link color (body link="...").
    if tag == "a"
        && el.attr(&local_name!("href")).is_some()
        && let Some(body) = doc.body()
        && let Some(c) = doc
            .attribute(body, &local_name!("link"))
            .and_then(legacy_color)
    {
        let _ = write!(css, "color:{c};");
    }

    // Body margins.
    if tag == "body" {
        for (name, sides) in [
            ("marginwidth", &["left", "right"][..]),
            ("marginheight", &["top", "bottom"][..]),
            ("leftmargin", &["left"][..]),
            ("topmargin", &["top"][..]),
        ] {
            if let Some(v) = el.attrs.iter().find(|a| &*a.name.local == name)
                && let Ok(px) = v.value.trim().parse::<u32>()
            {
                for side in sides {
                    let _ = write!(css, "margin-{side}:{px}px;");
                }
            }
        }
    }

    // Dimensions.
    let width_tags = matches!(
        tag,
        "table"
            | "col"
            | "colgroup"
            | "tr"
            | "td"
            | "th"
            | "hr"
            | "img"
            | "iframe"
            | "embed"
            | "object"
            | "video"
            | "marquee"
            | "canvas"
    ) || is_image_input(el);
    let height_tags = matches!(
        tag,
        "table"
            | "thead"
            | "tbody"
            | "tfoot"
            | "tr"
            | "td"
            | "th"
            | "img"
            | "iframe"
            | "embed"
            | "object"
            | "video"
            | "marquee"
            | "canvas"
    ) || is_image_input(el);
    let ignore_zero = matches!(tag, "td" | "th");
    if width_tags
        && let Some(v) =
            attr(local_name!("width")).and_then(|v| dimension(v, ignore_zero || tag == "table"))
    {
        let _ = write!(css, "width:{v};");
    }
    if height_tags
        && let Some(v) = attr(local_name!("height")).and_then(|v| dimension(v, ignore_zero))
    {
        let _ = write!(css, "height:{v};");
    }
    if matches!(tag, "img" | "object" | "embed") || is_image_input(el) {
        if let Some(v) = attr(local_name!("hspace")).and_then(|v| dimension(v, false)) {
            let _ = write!(css, "margin-left:{v};margin-right:{v};");
        }
        if let Some(v) = attr(local_name!("vspace")).and_then(|v| dimension(v, false)) {
            let _ = write!(css, "margin-top:{v};margin-bottom:{v};");
        }
        if let Some(n) = attr(local_name!("border")).and_then(|v| v.trim().parse::<u32>().ok()) {
            let _ = write!(css, "border:{n}px solid;");
        }
    }

    // Alignment.
    if let Some(align) = attr(local_name!("align")).map(|a| a.trim().to_ascii_lowercase()) {
        match tag {
            "table" => match align.as_str() {
                "center" | "middle" => css.push_str("margin-left:auto;margin-right:auto;"),
                "left" => css.push_str("float:left;"),
                "right" => css.push_str("float:right;"),
                _ => {}
            },
            "img" | "object" | "embed" | "iframe" | "input" => match align.as_str() {
                "left" => css.push_str("float:left;"),
                "right" => css.push_str("float:right;"),
                "top" => css.push_str("vertical-align:top;"),
                "middle" | "center" | "absmiddle" => css.push_str("vertical-align:middle;"),
                "bottom" | "baseline" => css.push_str("vertical-align:baseline;"),
                "texttop" => css.push_str("vertical-align:text-top;"),
                _ => {}
            },
            "hr" => match align.as_str() {
                "left" => css.push_str("margin-left:0;margin-right:auto;"),
                "right" => css.push_str("margin-left:auto;margin-right:0;"),
                "center" | "middle" => css.push_str("margin-left:auto;margin-right:auto;"),
                _ => {}
            },
            "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "legend" => match align.as_str() {
                "left" | "right" | "center" | "justify" => {
                    let _ = write!(css, "text-align:{align};");
                }
                "middle" => css.push_str("text-align:center;"),
                _ => {}
            },
            "div" | "caption" | "thead" | "tbody" | "tfoot" | "tr" | "td" | "th" => {
                match align.as_str() {
                    "left" => css.push_str("text-align:-webkit-left;"),
                    "right" => css.push_str("text-align:-webkit-right;"),
                    "center" | "middle" => css.push_str("text-align:-webkit-center;"),
                    "justify" => css.push_str("text-align:justify;"),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if matches!(
        tag,
        "td" | "th" | "tr" | "thead" | "tbody" | "tfoot" | "col" | "colgroup"
    ) && let Some(valign) = attr(local_name!("valign")).map(|a| a.trim().to_ascii_lowercase())
        && matches!(valign.as_str(), "top" | "middle" | "bottom" | "baseline")
    {
        let _ = write!(css, "vertical-align:{valign};");
    }
    if matches!(tag, "td" | "th") && el.attr(&local_name!("nowrap")).is_some() {
        css.push_str("white-space:nowrap;");
    }

    // Tables: cellspacing, cellpadding, border.
    if tag == "table" {
        if let Some(n) = attr(local_name!("cellspacing")).and_then(|v| pixels(v)) {
            let _ = write!(css, "border-spacing:{n}px;");
        }
        if let Some(n) = table_border(el) {
            if n > 0 {
                let _ = write!(css, "border:{n}px outset;");
            }
        }
    }
    if matches!(tag, "td" | "th")
        && let Some(table) = owning_table(doc, id)
        && let Some(t) = doc.element(table)
    {
        if let Some(n) = t.attr(&local_name!("cellpadding")).and_then(pixels) {
            let _ = write!(css, "padding:{n}px;");
        }
        if table_border(t).is_some_and(|n| n > 0) {
            css.push_str("border:1px inset;");
        }
    }

    // Lists.
    if matches!(tag, "ol" | "ul" | "li")
        && let Some(t) = attr(local_name!("type"))
    {
        let kind = match t.trim() {
            "1" => Some("decimal"),
            "a" => Some("lower-alpha"),
            "A" => Some("upper-alpha"),
            "i" => Some("lower-roman"),
            "I" => Some("upper-roman"),
            other => match other.to_ascii_lowercase().as_str() {
                "disc" => Some("disc"),
                "circle" => Some("circle"),
                "square" => Some("square"),
                "none" => Some("none"),
                _ => None,
            },
        };
        if let Some(kind) = kind {
            let _ = write!(css, "list-style-type:{kind};");
        }
    }

    // <hr size=N color=X noshade>.
    if tag == "hr" {
        if let Some(c) = attr(local_name!("color")).and_then(legacy_color) {
            let _ = write!(css, "border-color:{c};background-color:{c};");
        }
        if let Some(n) = attr(local_name!("size"))
            .and_then(pixels)
            .filter(|n| *n > 0)
        {
            let _ = write!(css, "height:{}px;", n.saturating_sub(2));
        }
        if el.attr(&local_name!("noshade")).is_some() {
            css.push_str("border-style:solid;background-color:gray;");
        }
    }

    (!css.is_empty()).then_some(css)
}

fn is_image_input(el: &ElementData) -> bool {
    el.is_html(&local_name!("input"))
        && el
            .attr(&local_name!("type"))
            .is_some_and(|t| t.eq_ignore_ascii_case("image"))
}

/// The nearest ancestor `<table>` of a cell.
fn owning_table(doc: &Document, cell: NodeId) -> Option<NodeId> {
    doc.ancestors(cell).find(|&a| {
        doc.element(a)
            .is_some_and(|e| e.is_html(&local_name!("table")))
    })
}

/// `<table border>`: an empty or invalid value means 1.
fn table_border(el: &ElementData) -> Option<u32> {
    let v = el.attr(&local_name!("border"))?;
    Some(pixels(v).unwrap_or(1))
}

/// HTML "rules for parsing non-negative integers", leading digits only.
fn pixels(v: &str) -> Option<u32> {
    style::servo::attr::parse_unsigned_integer(v.trim().chars()).ok()
}

/// SVG 2 geometry properties: `width` and `height` of an outermost `<svg>`
/// (one not inside another SVG element) are presentation attributes for the
/// CSS properties, as in Chrome. Inner SVG content is drawn by the SVG
/// renderer, not laid out as boxes.
fn svg_hint_css(doc: &Document, id: NodeId, el: &ElementData) -> Option<String> {
    if &*el.name.local != "svg"
        || doc
            .parent(id)
            .and_then(|p| doc.element(p))
            .is_some_and(|p| p.name.ns == ns!(svg))
    {
        return None;
    }
    let mut css = String::new();
    for (name, prop) in [
        (local_name!("width"), "width"),
        (local_name!("height"), "height"),
    ] {
        if let Some(v) = el.attr(&name).and_then(svg_length) {
            let _ = write!(css, "{prop}:{v};");
        }
    }
    (!css.is_empty()).then_some(css)
}

/// An SVG length attribute as a CSS value: a bare number is in px. Anything
/// but a single token of a length, percentage or keyword is dropped, so the
/// value cannot add declarations; Stylo rejects tokens that are not valid.
fn svg_length(v: &str) -> Option<String> {
    let v = v.trim();
    if v.parse::<f32>().is_ok_and(f32::is_finite) {
        return Some(format!("{v}px"));
    }
    let token = !v.is_empty()
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'%' | b'-' | b'+'));
    token.then(|| v.to_owned())
}

/// HTML "rules for parsing dimension values" as a CSS value.
fn dimension(v: &str, ignore_zero: bool) -> Option<String> {
    let parsed = if ignore_zero {
        parse_nonzero_length(v)
    } else {
        parse_length(v)
    };
    match parsed {
        LengthOrPercentageOrAuto::Length(au) => Some(format!("{}px", au.to_f32_px())),
        LengthOrPercentageOrAuto::Percentage(f) => Some(format!("{}%", f * 100.0)),
        LengthOrPercentageOrAuto::Auto => None,
    }
}

/// HTML "rules for parsing a legacy colour value", as CSS.
fn legacy_color(v: &str) -> Option<String> {
    let c: AbsoluteColor = parse_legacy_color(v).ok()?.into_srgb_legacy();
    let [r, g, b, _] = *c.raw_components();
    let to8 = |x: f32| (x * 255.0).round().clamp(0.0, 255.0) as u8;
    Some(format!("rgb({},{},{})", to8(r), to8(g), to8(b)))
}

/// `<font size>`: 1..7, or +N/-N relative to 3.
fn font_size_keyword(v: &str) -> Option<&'static str> {
    let v = v.trim();
    let n: i32 = if let Some(rest) = v.strip_prefix('+') {
        3 + rest.parse::<i32>().ok()?
    } else if let Some(rest) = v.strip_prefix('-') {
        3 - rest.parse::<i32>().ok()?
    } else {
        v.parse().ok()?
    };
    Some(match n.clamp(1, 7) {
        1 => "x-small",
        2 => "small",
        3 => "medium",
        4 => "large",
        5 => "x-large",
        6 => "xx-large",
        _ => "xxx-large",
    })
}

/// `<font face="Verdana, Geneva">` as a CSS family list (each name quoted).
fn font_face_list(face: &str) -> String {
    face.split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(|f| match f.to_ascii_lowercase().as_str() {
            "serif" | "sans-serif" | "monospace" | "cursive" | "fantasy" | "system-ui" => {
                f.to_string()
            }
            _ => format!("\"{}\"", css_escape(f)),
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn css_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
}
