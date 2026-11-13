//! The toolbar under the tab strip: back, forward, reload (stop while
//! loading) as thin line icons, and the square address field with the
//! security chip, zoom level and bookmark star. It is a small HTML document
//! rendered by our own pipeline (over the acrylic backdrop when the material
//! is on).

use huginn_layout::{LayoutEngine, Rect};
use huginn_net::{ImageStore, Url};
use huginn_paint::CpuRasterizer;

use crate::engine::escape_html;
use crate::input::{Key, KeyEvent};
use crate::tab::TOOLBAR_HEIGHT;
use crate::theme;
use crate::ui::{Pixels, render_glass_snippet, render_snippet};

/// What the toolbar shows besides the field text.
#[derive(Clone, PartialEq)]
pub struct ToolbarInfo {
    pub url: Option<Url>,
    pub can_back: bool,
    pub can_forward: bool,
    pub loading: bool,
    pub bookmarked: bool,
    pub zoom: f32,
    /// The window is private: the accent is the cool one.
    pub private: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ToolbarHit {
    Back,
    Forward,
    Reload,
    Star,
    Field,
}

/// Result of a key in the focused address field.
pub enum Edit {
    /// Enter: go to the text (modifiers pick the target).
    Go,
    /// Escape: put the URL back and leave the field.
    Cancel,
    Changed,
    Ignored,
}

#[derive(Default)]
pub struct Toolbar {
    pub focused: bool,
    pub text: String,
    /// Caret position in chars.
    pub caret: usize,
    pub select_all: bool,
    /// The button under the pointer.
    hover: Option<ToolbarHit>,
    /// Rendered toolbar and what it showed; `None` when it must be redrawn.
    pixels: Option<(Pixels, ToolbarInfo)>,
    back: Rect,
    forward: Rect,
    reload: Rect,
    star: Rect,
    field: Rect,
}

impl Toolbar {
    pub fn invalidate(&mut self) {
        self.pixels = None;
    }

    pub fn focus(&mut self, text: String) {
        self.focused = true;
        self.caret = text.chars().count();
        self.text = text;
        self.select_all = true;
        self.invalidate();
    }

    pub fn blur(&mut self) {
        if self.focused {
            self.focused = false;
            self.select_all = false;
            self.invalidate();
        }
    }

    /// Track the pointer over the buttons (their hover box).
    pub fn set_hover(&mut self, hit: Option<ToolbarHit>) {
        let hit = hit.filter(|h| *h != ToolbarHit::Field);
        if hit != self.hover {
            self.hover = hit;
            self.invalidate();
        }
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<ToolbarHit> {
        let inside = |r: Rect| x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
        [
            (self.back, ToolbarHit::Back),
            (self.forward, ToolbarHit::Forward),
            (self.reload, ToolbarHit::Reload),
            (self.star, ToolbarHit::Star),
            (self.field, ToolbarHit::Field),
        ]
        .into_iter()
        .find(|&(r, _)| inside(r))
        .map(|(_, h)| h)
    }

    fn byte(&self, char_index: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_index)
            .map_or(self.text.len(), |(i, _)| i)
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    pub fn insert(&mut self, s: &str) {
        if self.select_all {
            self.text.clear();
            self.caret = 0;
            self.select_all = false;
        }
        let at = self.byte(self.caret);
        self.text.insert_str(at, s);
        self.caret += s.chars().count();
        self.invalidate();
    }

    /// Start of the word before the caret (Option+Backspace, Option+Left).
    fn word_left(&self) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.caret.min(chars.len());
        while i > 0 && !chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    }

    fn word_right(&self) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.caret.min(chars.len());
        while i < chars.len() && !chars[i].is_alphanumeric() {
            i += 1;
        }
        while i < chars.len() && chars[i].is_alphanumeric() {
            i += 1;
        }
        i
    }

    fn delete_range(&mut self, from: usize, to: usize) {
        let (a, b) = (self.byte(from.min(to)), self.byte(from.max(to)));
        self.text.replace_range(a..b, "");
        self.caret = from.min(to);
    }

    /// A key while the field has focus.
    pub fn edit(&mut self, ev: &KeyEvent) -> Edit {
        let m = ev.mods;
        let len = self.len();
        match &ev.key {
            Key::Enter => return Edit::Go,
            Key::Escape => return Edit::Cancel,
            Key::Char(c) if m.cmd => match c.to_ascii_lowercase().as_str() {
                "a" => self.select_all = true,
                "c" => crate::clipboard::set(&self.text),
                "x" => {
                    crate::clipboard::set(&self.text);
                    self.text.clear();
                    self.caret = 0;
                    self.select_all = false;
                }
                "v" => {
                    if let Some(t) = crate::clipboard::get() {
                        self.insert(&t.replace(['\n', '\r'], ""));
                    }
                }
                _ => return Edit::Ignored,
            },
            Key::Char(_) if m.ctrl => return Edit::Ignored,
            Key::Char(s) => self.insert(s),
            Key::Space => self.insert(" "),
            Key::Backspace | Key::Delete if self.select_all => {
                self.text.clear();
                self.caret = 0;
                self.select_all = false;
            }
            Key::Backspace if m.cmd => self.delete_range(0, self.caret),
            Key::Backspace if m.alt => self.delete_range(self.word_left(), self.caret),
            Key::Backspace if self.caret > 0 => self.delete_range(self.caret - 1, self.caret),
            Key::Delete if self.caret < len => self.delete_range(self.caret, self.caret + 1),
            Key::Left | Key::Right | Key::Home | Key::End | Key::Up | Key::Down => {
                let all = std::mem::take(&mut self.select_all);
                self.caret = match &ev.key {
                    Key::Home | Key::Up => 0,
                    Key::End | Key::Down => len,
                    Key::Left if m.cmd => 0,
                    Key::Right if m.cmd => len,
                    Key::Left if all => 0,
                    Key::Right if all => len,
                    Key::Left if m.alt => self.word_left(),
                    Key::Right if m.alt => self.word_right(),
                    Key::Left => self.caret.saturating_sub(1),
                    _ => (self.caret + 1).min(len),
                };
            }
            Key::Backspace | Key::Delete => {}
            _ => return Edit::Ignored,
        }
        self.invalidate();
        Edit::Changed
    }

    /// The rendered toolbar for `info` at `width` window CSS px, rendering it if needed.
    pub fn pixels(
        &mut self,
        layout: &mut LayoutEngine,
        raster: &mut CpuRasterizer,
        info: &ToolbarInfo,
        width: f32,
        scale: f32,
    ) -> &Pixels {
        if self.pixels.as_ref().is_none_or(|(_, shown)| shown != info) {
            self.render(layout, raster, info, width, scale);
        }
        &self.pixels.as_ref().expect("just rendered").0
    }

    fn render(
        &mut self,
        layout: &mut LayoutEngine,
        raster: &mut CpuRasterizer,
        info: &ToolbarInfo,
        width: f32,
        scale: f32,
    ) {
        let ids = ["back", "fwd", "reload", "star", "field", "caret"];
        let mut images = ImageStore::new();
        let glass = theme::backdrop(width, scale).inspect(|img| {
            images.insert(&theme::backdrop_url(), img.clone());
        });
        let glass = glass.is_some();
        let render = if glass {
            render_glass_snippet
        } else {
            render_snippet
        };
        let html = |end: bool| toolbar_html(self, info, end, glass, width);
        let (mut px, mut rects) = render(
            layout,
            raster,
            &html(false),
            width,
            TOOLBAR_HEIGHT,
            &ids,
            &images,
            scale,
        );
        // Keep the caret in view: if it is past the field's end, align the text to the end.
        if self.focused && rects[5].right() > rects[4].right() - 40.0 {
            (px, rects) = render(
                layout,
                raster,
                &html(true),
                width,
                TOOLBAR_HEIGHT,
                &ids,
                &images,
                scale,
            );
        }
        self.back = rects[0];
        self.forward = rects[1];
        self.reload = rects[2];
        self.star = rects[3];
        self.field = rects[4];
        self.pixels = Some((px, info.clone()));
    }
}

/// How Chrome shows a URL when the field is not focused: no `https://`, no
/// trailing `/` on a bare origin. New tab pages show nothing.
pub fn display_url(u: &Url) -> String {
    if crate::internal_pages::is_new_tab(u) {
        return String::new();
    }
    let s = u.as_str();
    let s = s.strip_prefix("https://").unwrap_or(s);
    if u.path() == "/" && u.query().is_none() && u.fragment().is_none() {
        s.strip_suffix('/').unwrap_or(s).to_owned()
    } else {
        s.to_owned()
    }
}

/// A square toolbar button holding a line icon drawn with CSS.
fn button(id: &str, icon: &str, hover: bool) -> String {
    format!(
        "<div id={id} style=\"width:28px;height:28px;margin-right:2px;flex-shrink:0;display:flex;\
         align-items:center;justify-content:center;background:{}\"><div style=\"position:relative;\
         width:16px;height:16px\">{icon}</div></div>",
        if hover {
            theme::fill(theme::HOVER, 0.55)
        } else {
            "transparent".to_owned()
        }
    )
}

/// A 1.5 px bar at (`x`, `y`) in a 16 px icon box, `w` long, turned `deg`.
fn bar(x: f32, y: f32, w: f32, deg: i32, color: &str) -> String {
    format!(
        "<div style=\"position:absolute;left:{x}px;top:{y}px;width:{w}px;height:1.5px;background:{color};\
         transform:rotate({deg}deg)\"></div>"
    )
}

/// An arrow: a shaft and a chevron, pointing left or right.
fn arrow(left: bool, color: &str) -> String {
    let chevron = |x: f32, deg: i32| {
        format!(
            "<div style=\"position:absolute;left:{x}px;top:4px;width:8px;height:8px;box-sizing:border-box;\
             border-top:1.5px solid {color};border-left:1.5px solid {color};transform:rotate({deg}deg)\"></div>"
        )
    };
    if left {
        bar(2.5, 7.25, 11.5, 0, color) + &chevron(3.7, -45)
    } else {
        bar(2.0, 7.25, 11.5, 0, color) + &chevron(4.3, 135)
    }
}

/// Reload: the clockwise arrow glyph, thin and in the icon colour.
fn reload_icon(color: &str) -> String {
    format!(
        "<div style=\"position:absolute;left:0;top:-2px;width:16px;height:20px;font-size:16px;line-height:20px;\
         text-align:center;color:{color}\">\u{21bb}</div>"
    )
}

fn stop_icon(color: &str) -> String {
    bar(1.5, 7.25, 13.0, 45, color) + &bar(1.5, 7.25, 13.0, -45, color)
}

/// A small uppercase label in the field (security state, scheme).
fn chip(text: &str, color: &str, border: &str) -> String {
    format!(
        "<div style=\"flex-shrink:0;color:{color};border:1px solid {border};font-size:10px;line-height:12px;\
         padding:1px 5px;margin-right:10px;white-space:pre;{}\">{text}</div>",
        theme::LABEL
    )
}

/// `shown` split around the URL's host, so the host can stand out.
fn split_host<'a>(shown: &'a str, url: Option<&Url>) -> (&'a str, &'a str, &'a str) {
    match url.and_then(Url::host_str).filter(|h| !h.is_empty()) {
        Some(host) if shown.contains(host) => {
            let i = shown.find(host).expect("contains");
            (
                &shown[..i],
                &shown[i..i + host.len()],
                &shown[i + host.len()..],
            )
        }
        _ => ("", shown, ""),
    }
}

fn toolbar_html(
    t: &Toolbar,
    info: &ToolbarInfo,
    align_end: bool,
    glass: bool,
    width: f32,
) -> String {
    let url = info.url.as_ref();
    let accent = theme::accent(info.private);
    let (before, after, placeholder) = if t.focused {
        let at = t.byte(t.caret);
        (t.text[..at].to_owned(), t.text[at..].to_owned(), false)
    } else {
        let shown = url.map(display_url).unwrap_or_default();
        let empty = shown.is_empty();
        (shown, String::new(), empty)
    };
    let (muted, line) = (theme::MUTED, theme::HAIRLINE);
    let security = match url.map(Url::scheme) {
        _ if t.focused => String::new(),
        Some("https") => format!(
            "<div style=\"width:6px;height:6px;flex-shrink:0;margin-right:10px;background:{muted}\"></div>"
        ),
        Some("http") => chip("Not secure", theme::WARN, theme::WARN),
        Some("file") => chip("File", muted, line),
        Some("view-source") => chip("view-source", muted, line),
        Some("huginn") if !placeholder => chip("Huginn", muted, line),
        _ => String::new(),
    };
    let caret = if t.focused && !t.select_all {
        format!(
            "<div id=caret style=\"width:1.5px;height:16px;background:{accent};flex-shrink:0\"></div>"
        )
    } else {
        "<div id=caret style=\"width:0;height:16px;flex-shrink:0\"></div>".to_owned()
    };
    let text = if placeholder {
        format!("<div style=\"flex-shrink:0;color:{muted}\">Search DuckDuckGo or type a URL</div>")
    } else if t.focused {
        format!(
            "<div style=\"flex-shrink:0;{}\">{}</div>{caret}<div style=\"flex-shrink:0\">{}</div>",
            if t.select_all {
                format!("background:{}", theme::selection(info.private))
            } else {
                String::new()
            },
            escape_html(&before),
            escape_html(&after),
        )
    } else {
        // Like Chrome: the host stands out, scheme and path are quieter.
        let (pre, host, post) = split_host(&before, url);
        format!(
            "<div style=\"flex-shrink:0;color:{muted}\">{}</div><div style=\"flex-shrink:0\">{}</div>\
             <div style=\"flex-shrink:0;color:{muted}\">{}</div>{caret}",
            escape_html(pre),
            escape_html(host),
            escape_html(post),
        )
    };
    let zoom = if (info.zoom - 1.0).abs() > 0.001 {
        format!(
            "<div style=\"flex-shrink:0;margin-left:6px;font-size:10px;line-height:12px;color:{muted};\
             border:1px solid {line};padding:1px 5px;{}\">{:.0}%</div>",
            theme::LABEL,
            info.zoom * 100.0
        )
    } else {
        String::new()
    };
    let star = format!(
        "<div id=star style=\"flex-shrink:0;width:28px;height:26px;margin-left:4px;margin-right:-9px;\
         display:flex;align-items:center;justify-content:center;font-size:15px;color:{};background:{}\">{}</div>",
        if info.bookmarked { accent } else { muted },
        if t.hover == Some(ToolbarHit::Star) {
            theme::fill(theme::HOVER, 0.55)
        } else {
            "transparent".to_owned()
        },
        if info.bookmarked {
            "\u{2605}"
        } else {
            "\u{2606}"
        }
    );
    let color = |enabled: bool| if enabled { muted } else { theme::DIM };
    let hover = |h: ToolbarHit, enabled: bool| enabled && t.hover == Some(h);
    let backdrop = if glass {
        format!(
            "<img src=\"{}\" style=\"position:absolute;left:0;top:-40px;width:{width}px;height:{}px\">",
            theme::backdrop_url(),
            theme::BACKDROP_HEIGHT
        )
    } else {
        String::new()
    };
    format!(
        "<!doctype html><html><body style=\"margin:0;background:{};font-family:{};font-size:13px;\
         color:{}\">{backdrop}<div style=\"position:relative;display:flex;align-items:center;\
         height:40px;padding:0 8px;box-sizing:border-box;border-bottom:1px solid {line}\">{}{}{}\
         <div id=field style=\"flex-grow:1;flex-shrink:1;min-width:0;height:28px;margin-left:6px;\
         box-sizing:border-box;background:{};border:1px solid {};padding:0 10px;\
         display:flex;align-items:center;overflow:hidden;white-space:pre\">{security}\
         <div style=\"flex-grow:1;flex-shrink:1;min-width:0;overflow:hidden;display:flex;align-items:center;\
         justify-content:{}\">{text}</div>{zoom}{star}</div></div></body></html>",
        if glass { "transparent" } else { theme::TOOLBAR },
        theme::FONT,
        theme::TEXT,
        button(
            "back",
            &arrow(true, color(info.can_back)),
            hover(ToolbarHit::Back, info.can_back)
        ),
        button(
            "fwd",
            &arrow(false, color(info.can_forward)),
            hover(ToolbarHit::Forward, info.can_forward)
        ),
        if info.loading {
            button("reload", &stop_icon(muted), hover(ToolbarHit::Reload, true))
        } else {
            button(
                "reload",
                &reload_icon(muted),
                hover(ToolbarHit::Reload, true),
            )
        },
        theme::fill(theme::STRIP, 0.62),
        if t.focused { accent } else { line },
        if align_end { "flex-end" } else { "flex-start" },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Modifiers;

    fn key(t: &mut Toolbar, key: Key, mods: Modifiers) {
        t.edit(&KeyEvent { key, mods });
    }

    #[test]
    fn caret_editing() {
        let mut t = Toolbar::default();
        t.focus("example.com".into());
        key(&mut t, Key::Left, Modifiers::default());
        assert_eq!((t.caret, t.select_all), (0, false));
        key(&mut t, Key::Char("x".into()), Modifiers::default());
        assert_eq!(t.text, "xexample.com");
        key(
            &mut t,
            Key::Right,
            Modifiers {
                cmd: true,
                ..Modifiers::default()
            },
        );
        key(
            &mut t,
            Key::Backspace,
            Modifiers {
                alt: true,
                ..Modifiers::default()
            },
        );
        assert_eq!(t.text, "xexample.");
        key(&mut t, Key::Backspace, Modifiers::default());
        assert_eq!(t.text, "xexample");
        key(&mut t, Key::Home, Modifiers::default());
        key(&mut t, Key::Delete, Modifiers::default());
        assert_eq!(t.text, "example");
    }

    #[test]
    fn url_display_hides_https_and_root_slash() {
        let u = |s: &str| Url::parse(s).unwrap();
        assert_eq!(display_url(&u("https://example.com/")), "example.com");
        assert_eq!(display_url(&u("https://example.com/a/")), "example.com/a/");
        assert_eq!(display_url(&u("http://example.com/")), "http://example.com");
        assert_eq!(display_url(&u("huginn://newtab/")), "");
    }
}
