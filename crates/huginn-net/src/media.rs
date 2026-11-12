//! Media queries and CSS lengths for resource selection: `<source media>`,
//! `<link media>` and `<img sizes>`. Evaluated against a fixed screen (the
//! viewport, light color scheme, fine pointer, no reduced motion).
//!
//! Stylesheet `@media` rules are evaluated by huginn-style, not here.

/// The screen a page is rendered for.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Screen {
    /// Viewport size in CSS px.
    pub width: f32,
    pub height: f32,
    pub device_pixel_ratio: f32,
}

/// Font size used for `em` and `rem` in media queries and `sizes` (the
/// initial value; media queries never see author font sizes).
const FONT_PX: f32 = 16.0;

/// Evaluate a media query list (`screen and (min-width: 40em), print`).
/// Empty means `all`. Unknown features do not match.
pub fn media_matches(list: &str, screen: &Screen) -> bool {
    let list = list.trim();
    list.is_empty() || split_top_level(list, b',').any(|q| query_matches(q.trim(), screen))
}

fn query_matches(q: &str, screen: &Screen) -> bool {
    let lower = q.to_ascii_lowercase();
    let mut rest = lower.as_str();
    let negate = if let Some(r) = strip_word(rest, "not") {
        rest = r;
        true
    } else {
        if let Some(r) = strip_word(rest, "only") {
            rest = r;
        }
        false
    };
    let mut ok = true;
    if !rest.starts_with('(') {
        // A media type, optionally followed by `and (...)`.
        let end = rest
            .find(|c: char| c.is_ascii_whitespace())
            .unwrap_or(rest.len());
        ok = matches!(&rest[..end], "all" | "screen");
        rest = rest[end..].trim_start();
        match strip_word(rest, "and") {
            Some(r) => rest = r,
            None if rest.is_empty() => return ok != negate,
            None => return false,
        }
    }
    ok &= condition_matches(rest, screen);
    ok != negate
}

/// `(a) and (b)`, `(a) or (b)`, `not (a)`, nested parentheses.
fn condition_matches(cond: &str, screen: &Screen) -> bool {
    let cond = cond.trim();
    if let Some(r) = strip_word(cond, "not") {
        return !condition_matches(r, screen);
    }
    let mut result: Option<bool> = None;
    let mut op_or = false;
    let mut rest = cond;
    while !rest.is_empty() {
        let Some(inner_end) = matching_paren(rest) else {
            return false;
        };
        let inner = &rest[1..inner_end];
        let v = if inner.trim_start().starts_with('(') || inner.trim_start().starts_with("not ") {
            condition_matches(inner, screen)
        } else {
            feature_matches(inner, screen)
        };
        result = Some(match result {
            None => v,
            Some(prev) if op_or => prev || v,
            Some(prev) => prev && v,
        });
        rest = rest[inner_end + 1..].trim_start();
        if let Some(r) = strip_word(rest, "and") {
            op_or = false;
            rest = r;
        } else if let Some(r) = strip_word(rest, "or") {
            op_or = true;
            rest = r;
        } else if !rest.is_empty() {
            return false;
        }
    }
    result.unwrap_or(false)
}

/// One `feature: value`, `feature` or range (`width >= 600px`, `400px < width < 800px`).
fn feature_matches(f: &str, screen: &Screen) -> bool {
    let f = f.trim();
    if let Some((name, value)) = f.split_once(':') {
        let (name, value) = (name.trim(), value.trim());
        let len = || parse_length(value, screen);
        return match name {
            "min-width" => len().is_some_and(|v| screen.width >= v),
            "max-width" => len().is_some_and(|v| screen.width <= v),
            "width" => len().is_some_and(|v| screen.width == v),
            "min-height" => len().is_some_and(|v| screen.height >= v),
            "max-height" => len().is_some_and(|v| screen.height <= v),
            "height" => len().is_some_and(|v| screen.height == v),
            "orientation" => (value == "landscape") == (screen.width >= screen.height),
            "prefers-color-scheme" => value == "light",
            "prefers-reduced-motion" | "prefers-reduced-transparency" | "prefers-reduced-data" => {
                value == "no-preference"
            }
            "prefers-contrast" | "forced-colors" | "inverted-colors" => {
                matches!(value, "no-preference" | "none")
            }
            "hover" | "any-hover" => value == "hover",
            "pointer" | "any-pointer" => value == "fine",
            "min-resolution" => resolution(value).is_some_and(|v| screen.device_pixel_ratio >= v),
            "max-resolution" => resolution(value).is_some_and(|v| screen.device_pixel_ratio <= v),
            "-webkit-min-device-pixel-ratio" | "min--moz-device-pixel-ratio" => value
                .parse::<f32>()
                .is_ok_and(|v| screen.device_pixel_ratio >= v),
            "-webkit-max-device-pixel-ratio" => value
                .parse::<f32>()
                .is_ok_and(|v| screen.device_pixel_ratio <= v),
            "color" | "min-color" => true,
            "display-mode" => value == "browser",
            "scripting" => value == "none",
            _ => false,
        };
    }
    if matches!(
        f,
        "color" | "hover" | "pointer" | "any-hover" | "any-pointer" | "width" | "height"
    ) {
        return true;
    }
    range_matches(f, screen).unwrap_or(false)
}

fn range_matches(f: &str, screen: &Screen) -> Option<bool> {
    // Tokens: values and operators, e.g. ["400px", "<", "width", "<=", "800px"].
    let mut parts: Vec<&str> = Vec::new();
    let mut rest = f;
    while !rest.is_empty() {
        rest = rest.trim_start();
        let op_len = if rest.starts_with("<=") || rest.starts_with(">=") {
            2
        } else if rest.starts_with(['<', '>', '=']) {
            1
        } else {
            0
        };
        if op_len > 0 {
            parts.push(&rest[..op_len]);
            rest = &rest[op_len..];
        } else {
            let end = rest.find(['<', '>', '=']).unwrap_or(rest.len());
            parts.push(rest[..end].trim());
            rest = &rest[end..];
        }
    }
    let value_of = |s: &str| match s {
        "width" => Some(screen.width),
        "height" => Some(screen.height),
        _ => parse_length(s, screen),
    };
    let cmp = |a: f32, op: &str, b: f32| match op {
        "<" => a < b,
        "<=" => a <= b,
        ">" => a > b,
        ">=" => a >= b,
        _ => a == b,
    };
    match parts.as_slice() {
        [a, op, b] => Some(cmp(value_of(a)?, op, value_of(b)?)),
        [a, op1, b, op2, c] => {
            let (a, b, c) = (value_of(a)?, value_of(b)?, value_of(c)?);
            Some(cmp(a, op1, b) && cmp(b, op2, c))
        }
        _ => None,
    }
}

fn resolution(v: &str) -> Option<f32> {
    if let Some(n) = v.strip_suffix("dppx").or_else(|| v.strip_suffix('x')) {
        n.trim().parse().ok()
    } else if let Some(n) = v.strip_suffix("dpi") {
        n.trim().parse::<f32>().ok().map(|d| d / 96.0)
    } else {
        None
    }
}

/// A CSS length in px: `px`, `em`, `rem`, `vw`, `vh`, `vmin`, `vmax`, `0`, and
/// `calc()`/`min()`/`max()`/`clamp()` over those. Percentages are not lengths here.
pub fn parse_length(s: &str, screen: &Screen) -> Option<f32> {
    let mut p = LengthParser {
        s: s.trim().as_bytes(),
        pos: 0,
        screen,
    };
    let v = p.sum()?;
    p.skip_ws();
    (p.pos == p.s.len()).then_some(v)
}

struct LengthParser<'a> {
    s: &'a [u8],
    pos: usize,
    screen: &'a Screen,
}

impl LengthParser<'_> {
    fn skip_ws(&mut self) {
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.skip_ws();
        if self.s.get(self.pos) == Some(&c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn sum(&mut self) -> Option<f32> {
        let mut v = self.product()?;
        loop {
            if self.eat(b'+') {
                v += self.product()?;
            } else if self.eat(b'-') {
                v -= self.product()?;
            } else {
                return Some(v);
            }
        }
    }

    fn product(&mut self) -> Option<f32> {
        let mut v = self.atom()?;
        loop {
            if self.eat(b'*') {
                v *= self.atom()?;
            } else if self.eat(b'/') {
                let d = self.atom()?;
                if d == 0.0 {
                    return None;
                }
                v /= d;
            } else {
                return Some(v);
            }
        }
    }

    fn atom(&mut self) -> Option<f32> {
        self.skip_ws();
        if self.eat(b'(') {
            let v = self.sum()?;
            return self.eat(b')').then_some(v);
        }
        let start = self.pos;
        while self.pos < self.s.len()
            && (self.s[self.pos].is_ascii_alphabetic() || self.s[self.pos] == b'-')
        {
            self.pos += 1;
        }
        let func = std::str::from_utf8(&self.s[start..self.pos])
            .ok()?
            .to_ascii_lowercase();
        if !func.is_empty() {
            if !self.eat(b'(') {
                return None;
            }
            let mut args = vec![self.sum()?];
            while self.eat(b',') {
                args.push(self.sum()?);
            }
            if !self.eat(b')') {
                return None;
            }
            return match (func.as_str(), args.as_slice()) {
                ("calc", [v]) => Some(*v),
                ("min", a) => a.iter().copied().reduce(f32::min),
                ("max", a) => a.iter().copied().reduce(f32::max),
                ("clamp", [lo, v, hi]) => Some(v.min(*hi).max(*lo)),
                _ => None,
            };
        }
        // Number with an optional unit.
        let start = self.pos;
        if matches!(self.s.get(self.pos), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        while self.pos < self.s.len()
            && (self.s[self.pos].is_ascii_digit() || self.s[self.pos] == b'.')
        {
            self.pos += 1;
        }
        let n: f32 = std::str::from_utf8(&self.s[start..self.pos])
            .ok()?
            .parse()
            .ok()?;
        let ustart = self.pos;
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_alphabetic() {
            self.pos += 1;
        }
        let unit = std::str::from_utf8(&self.s[ustart..self.pos])
            .ok()?
            .to_ascii_lowercase();
        let sc = self.screen;
        Some(match unit.as_str() {
            "" => n, // unitless: 0, or a factor inside calc()
            "px" => n,
            "em" | "rem" => n * FONT_PX,
            "vw" => n * sc.width / 100.0,
            "vh" => n * sc.height / 100.0,
            "vmin" => n * sc.width.min(sc.height) / 100.0,
            "vmax" => n * sc.width.max(sc.height) / 100.0,
            "pt" => n * 96.0 / 72.0,
            "cm" => n * 96.0 / 2.54,
            "mm" => n * 96.0 / 25.4,
            "in" => n * 96.0,
            "ch" => n * FONT_PX / 2.0,
            "ex" => n * FONT_PX / 2.0,
            _ => return None,
        })
    }
}

fn strip_word<'a>(s: &'a str, word: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(word)?;
    match rest.chars().next() {
        Some(c) if c.is_ascii_whitespace() || c == '(' => Some(rest.trim_start()),
        _ => None,
    }
}

/// Index of the `)` matching the `(` at the start of `s`.
fn matching_paren(s: &str) -> Option<usize> {
    if !s.starts_with('(') {
        return None;
    }
    let mut depth = 0;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split on `sep` outside parentheses.
pub(crate) fn split_top_level(s: &str, sep: u8) -> impl Iterator<Item = &str> {
    let mut depth = 0i32;
    let mut start = 0;
    let bytes = s.as_bytes();
    let mut parts = Vec::new();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if b == sep && depth <= 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts.into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESKTOP: Screen = Screen {
        width: 1280.0,
        height: 800.0,
        device_pixel_ratio: 1.0,
    };

    #[test]
    fn media_queries() {
        let m = |q| media_matches(q, &DESKTOP);
        assert!(m(""));
        assert!(m("all"));
        assert!(m("screen and (min-width: 1024px)"));
        assert!(!m("screen and (max-width: 1024px)"));
        assert!(!m("print"));
        assert!(m("print, (min-width: 40em)"));
        assert!(!m("not screen"));
        assert!(m("not print"));
        assert!(m("(min-width: 600px) and (orientation: landscape)"));
        assert!(!m("(prefers-color-scheme: dark)"));
        assert!(m("(prefers-color-scheme: light)"));
        assert!(m("(width >= 1000px)"));
        assert!(!m("(400px <= width <= 700px)"));
        assert!(m("(max-width: 600px) or (min-width: 1200px)"));
        assert!(!m("(min-width: 600px) and (max-width: 1000px)"));
        assert!(!m("(unknown-feature: 1)"));
        assert!(!m("screen and (-webkit-min-device-pixel-ratio: 2)"));
    }

    #[test]
    fn lengths() {
        let l = |s| parse_length(s, &DESKTOP);
        assert_eq!(l("300px"), Some(300.0));
        assert_eq!(l("50vw"), Some(640.0));
        assert_eq!(l("2em"), Some(32.0));
        assert_eq!(l("calc(100vw - 2rem)"), Some(1248.0));
        assert_eq!(l("min(100vw, 800px)"), Some(800.0));
        assert!((l("calc((100vw - 48px) / 3)").unwrap() - 410.667).abs() < 0.01);
        assert_eq!(l("50%"), None);
        assert_eq!(l("auto"), None);
    }
}
