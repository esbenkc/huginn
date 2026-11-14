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
#[derive(Debug, PartialEq, Eq)]
pub enum Edit {
    /// Enter: go to the text (modifiers pick the target).
    Go,
    /// Escape: put the URL back and leave the field.
    Cancel,
    /// The text changed. `inline`: it may be completed inline (typing at
    /// the end; not a deletion, a cut or a paste, as in Chrome).
    Text {
        inline: bool,
    },
    /// Only the caret or the selection changed.
    Caret,
    Ignored,
}

#[derive(Default)]
pub struct Toolbar {
    pub focused: bool,
    pub text: String,
    /// Caret position in chars.
    pub caret: usize,
    /// The other end of the selection, in chars; `caret` when nothing is
    /// selected.
    pub anchor: usize,
    /// The selection is an inline completion (`set_completion`).
    autofilled: bool,
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

    /// Focus the field with `text`, all of it selected.
    pub fn focus(&mut self, text: String) {
        self.focused = true;
        self.caret = text.chars().count();
        self.anchor = 0;
        self.text = text;
        self.autofilled = false;
        self.invalidate();
    }

    pub fn blur(&mut self) {
        if self.focused {
            self.focused = false;
            self.anchor = self.caret;
            self.autofilled = false;
            self.invalidate();
        }
    }

    /// The selection as (start, end) in chars, if any.
    pub fn selection(&self) -> Option<(usize, usize)> {
        (self.anchor != self.caret)
            .then(|| (self.anchor.min(self.caret), self.anchor.max(self.caret)))
    }

    /// Replace the text (a suggestion picked with the arrow keys), caret at the end.
    pub fn set_text(&mut self, text: String) {
        self.caret = text.chars().count();
        self.anchor = self.caret;
        self.text = text;
        self.autofilled = false;
        self.invalidate();
    }

    /// Complete the text inline with `rest`, selected, so that typing on
    /// replaces it and Backspace removes it.
    pub fn set_completion(&mut self, rest: &str) {
        self.anchor = self.len();
        self.text.push_str(rest);
        self.caret = self.len();
        self.autofilled = !rest.is_empty();
        self.invalidate();
    }

    /// An inline completion is shown (selected).
    pub fn has_completion(&self) -> bool {
        self.autofilled && self.selection().is_some()
    }

    /// Put the caret at the end and select nothing (a click in the focused field).
    pub fn caret_to_end(&mut self) {
        self.caret = self.len();
        self.anchor = self.caret;
        self.autofilled = false;
        self.invalidate();
    }

    /// Track the pointer over the buttons (their hover box).
    pub fn set_hover(&mut self, hit: Option<ToolbarHit>) {
        let hit = hit.filter(|h| *h != ToolbarHit::Field);
        if hit != self.hover {
            self.hover = hit;
            self.invalidate();
        }
    }

    /// The address field in toolbar CSS px (as last rendered).
    pub fn field(&self) -> Rect {
        self.field
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

    /// Delete the selection. Returns whether there was one.
    fn delete_selection(&mut self) -> bool {
        let Some((a, b)) = self.selection() else {
            return false;
        };
        self.delete_range(a, b);
        true
    }

    pub fn insert(&mut self, s: &str) {
        self.delete_selection();
        let at = self.byte(self.caret);
        self.text.insert_str(at, s);
        self.caret += s.chars().count();
        self.anchor = self.caret;
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
        self.anchor = self.caret;
    }

    /// The selected text, else all of it (Cmd+C, Cmd+X).
    fn selected_text(&self) -> String {
        match self.selection() {
            Some((a, b)) => self.text[self.byte(a)..self.byte(b)].to_owned(),
            None => self.text.clone(),
        }
    }

    /// A key while the field has focus.
    pub fn edit(&mut self, ev: &KeyEvent) -> Edit {
        let m = ev.mods;
        let len = self.len();
        let before = self.text.clone();
        let r = match &ev.key {
            Key::Enter => return Edit::Go,
            Key::Escape => return Edit::Cancel,
            Key::Char(c) if m.cmd => match c.to_ascii_lowercase().as_str() {
                "a" => {
                    self.anchor = 0;
                    self.caret = len;
                    Edit::Caret
                }
                "c" => {
                    crate::clipboard::set(&self.selected_text());
                    return Edit::Caret;
                }
                "x" => {
                    crate::clipboard::set(&self.selected_text());
                    if !self.delete_selection() {
                        self.text.clear();
                        self.caret = 0;
                        self.anchor = 0;
                    }
                    Edit::Text { inline: false }
                }
                "v" => match crate::clipboard::get() {
                    Some(t) => {
                        self.insert(&t.replace(['\n', '\r'], ""));
                        Edit::Text { inline: false }
                    }
                    None => return Edit::Caret,
                },
                _ => return Edit::Ignored,
            },
            Key::Char(_) if m.ctrl => return Edit::Ignored,
            Key::Char(_) | Key::Space => {
                self.insert(match &ev.key {
                    Key::Char(s) => s,
                    _ => " ",
                });
                Edit::Text {
                    inline: self.caret == self.len(),
                }
            }
            Key::Backspace | Key::Delete => {
                if !self.delete_selection() {
                    match &ev.key {
                        Key::Backspace if m.cmd => self.delete_range(0, self.caret),
                        Key::Backspace if m.alt => self.delete_range(self.word_left(), self.caret),
                        Key::Backspace if self.caret > 0 => {
                            self.delete_range(self.caret - 1, self.caret)
                        }
                        Key::Delete if self.caret < len => {
                            self.delete_range(self.caret, self.caret + 1)
                        }
                        _ => {}
                    }
                }
                Edit::Text { inline: false }
            }
            // Tab takes an inline completion, like Right at the end.
            Key::Tab if self.has_completion() => {
                self.caret = len;
                self.anchor = len;
                Edit::Caret
            }
            Key::Left | Key::Right | Key::Home | Key::End | Key::Up | Key::Down => {
                let sel = self.selection();
                self.caret = match (&ev.key, sel) {
                    (Key::Home | Key::Up, _) => 0,
                    (Key::End | Key::Down, _) => len,
                    (Key::Left, _) if m.cmd => 0,
                    (Key::Right, _) if m.cmd => len,
                    (Key::Left, Some((a, _))) => a,
                    (Key::Right, Some((_, b))) => b,
                    (Key::Left, _) if m.alt => self.word_left(),
                    (Key::Right, _) if m.alt => self.word_right(),
                    (Key::Left, _) => self.caret.saturating_sub(1),
                    _ => (self.caret + 1).min(len),
                };
                self.anchor = self.caret;
                Edit::Caret
            }
            _ => return Edit::Ignored,
        };
        self.autofilled = false;
        self.invalidate();
        match r {
            Edit::Text { .. } if self.text == before => Edit::Caret,
            r => r,
        }
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
    let (before, placeholder) = if t.focused {
        (String::new(), false)
    } else {
        let shown = url.map(display_url).unwrap_or_default();
        let empty = shown.is_empty();
        (shown, empty)
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
    let caret = if t.focused && t.selection().is_none() {
        format!(
            "<div id=caret style=\"width:1.5px;height:16px;background:{accent};flex-shrink:0\"></div>"
        )
    } else {
        "<div id=caret style=\"width:0;height:16px;flex-shrink:0\"></div>".to_owned()
    };
    let text = if placeholder {
        format!("<div style=\"flex-shrink:0;color:{muted}\">Search DuckDuckGo or type a URL</div>")
    } else if t.focused {
        // Text before, in and after the selection; the caret at its end
        // (where an inline completion keeps it) or its start.
        let (a, b) = t.selection().unwrap_or((t.caret, t.caret));
        let (a, b) = (t.byte(a), t.byte(b));
        let part = |s: &str, style: &str| {
            if s.is_empty() {
                String::new()
            } else {
                format!(
                    "<div style=\"flex-shrink:0;{style}\">{}</div>",
                    escape_html(s)
                )
            }
        };
        let selected = part(
            &t.text[a..b],
            &format!("background:{}", theme::selection(info.private)),
        );
        let (first, second) = if t.byte(t.caret) == a && a != b {
            (caret, selected)
        } else {
            (selected, caret)
        };
        format!(
            "{}{first}{second}{}",
            part(&t.text[..a], ""),
            part(&t.text[b..], "")
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

    fn key(t: &mut Toolbar, key: Key, mods: Modifiers) -> Edit {
        t.edit(&KeyEvent { key, mods })
    }

    #[test]
    fn caret_editing() {
        let mut t = Toolbar::default();
        t.focus("example.com".into());
        key(&mut t, Key::Left, Modifiers::default());
        assert_eq!((t.caret, t.selection()), (0, None));
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
    fn an_inline_completion_is_selected_typed_over_deleted_or_taken() {
        let none = Modifiers::default;
        let mut t = Toolbar::default();
        t.focus(String::new());
        let typed = key(&mut t, Key::Char("k".into()), none());
        assert_eq!(typed, Edit::Text { inline: true });
        t.set_completion("ran.ai");
        assert_eq!((t.text.as_str(), t.selection()), ("kran.ai", Some((1, 7))));
        assert!(t.has_completion());
        // Typing replaces the completion.
        let typed = key(&mut t, Key::Char("r".into()), none());
        assert_eq!(
            (typed, t.text.as_str()),
            (Edit::Text { inline: true }, "kr")
        );
        // Backspace and Delete remove only the completion, and ask for no new one.
        t.set_completion("an.ai");
        let deleted = key(&mut t, Key::Backspace, none());
        assert_eq!(
            (deleted, t.text.as_str()),
            (Edit::Text { inline: false }, "kr")
        );
        t.set_completion("an.ai");
        let deleted = key(&mut t, Key::Delete, none());
        assert_eq!(
            (deleted, t.text.as_str()),
            (Edit::Text { inline: false }, "kr")
        );
        // Tab and Right take it: the caret goes to the end.
        for k in [Key::Tab, Key::Right] {
            t.set_text("kr".into());
            t.set_completion("an.ai");
            assert_eq!(key(&mut t, k, none()), Edit::Caret);
            assert_eq!(
                (t.text.as_str(), t.selection(), t.caret),
                ("kran.ai", None, 7)
            );
            assert!(!t.has_completion());
        }
        // Without a completion Tab is not the field's.
        assert_eq!(key(&mut t, Key::Tab, none()), Edit::Ignored);
        // Typing away from the end does not ask for a completion.
        key(&mut t, Key::Left, none());
        let typed = key(&mut t, Key::Char("x".into()), none());
        assert_eq!(
            (typed, t.text.as_str()),
            (Edit::Text { inline: false }, "kran.axi")
        );
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
