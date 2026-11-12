//! The toolbar under the tab strip: back, forward, reload (stop while
//! loading) and the address field with the security chip, zoom level and
//! bookmark star. It is a small HTML document rendered by our own pipeline.

use huginn_layout::{LayoutEngine, Rect};
use huginn_net::{ImageStore, Url};
use huginn_paint::CpuRasterizer;

use crate::engine::escape_html;
use crate::input::{Key, KeyEvent};
use crate::tab::TOOLBAR_HEIGHT;
use crate::ui::{Pixels, render_snippet};

/// What the toolbar shows besides the field text.
#[derive(Clone, PartialEq)]
pub struct ToolbarInfo {
    pub url: Option<Url>,
    pub can_back: bool,
    pub can_forward: bool,
    pub loading: bool,
    pub bookmarked: bool,
    pub zoom: f32,
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
        let images = ImageStore::new();
        let html = |end: bool| toolbar_html(self, info, end);
        let (mut px, mut rects) = render_snippet(
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
            (px, rects) = render_snippet(
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

fn button(id: &str, glyph: &str, enabled: bool, size: u32) -> String {
    format!(
        "<div id={id} style=\"width:32px;height:32px;margin-right:2px;border-radius:16px;display:flex;\
         align-items:center;justify-content:center;font-size:{size}px;color:{}\">{glyph}</div>",
        if enabled { "#474747" } else { "#c4c7c5" }
    )
}

/// A padlock drawn with CSS: shackle and body.
const LOCK: &str = "<div style=\"width:12px;height:14px;margin-right:8px;flex-shrink:0;position:relative\">\
    <div style=\"position:absolute;left:2px;top:0;width:4px;height:6px;border:1.5px solid #474747;\
    border-bottom:0;border-radius:4px 4px 0 0\"></div>\
    <div style=\"position:absolute;left:0;top:6px;width:12px;height:8px;background:#474747;border-radius:2px\"></div></div>";

fn chip(text: &str, color: &str) -> String {
    format!(
        "<div style=\"flex-shrink:0;color:{color};margin-right:8px;padding-right:8px;\
         border-right:1px solid #c4c7c5;white-space:pre\">{text}</div>"
    )
}

fn toolbar_html(t: &Toolbar, info: &ToolbarInfo, align_end: bool) -> String {
    let url = info.url.as_ref();
    let (before, after, placeholder) = if t.focused {
        let at = t.byte(t.caret);
        (t.text[..at].to_owned(), t.text[at..].to_owned(), false)
    } else {
        let shown = url.map(display_url).unwrap_or_default();
        let empty = shown.is_empty();
        (shown, String::new(), empty)
    };
    let security = match url.map(Url::scheme) {
        _ if t.focused => String::new(),
        Some("https") => LOCK.to_owned(),
        Some("http") => chip("Not secure", "#5f6368"),
        Some("file") => chip("File", "#5f6368"),
        Some("view-source") => chip("view-source", "#5f6368"),
        Some("huginn") if !placeholder => chip("Huginn", "#5f6368"),
        _ => String::new(),
    };
    let caret = if t.focused && !t.select_all {
        "<div id=caret style=\"width:1px;height:16px;background:#1f1f1f;flex-shrink:0\"></div>"
    } else {
        "<div id=caret style=\"width:0;height:16px;flex-shrink:0\"></div>"
    };
    let text = if placeholder {
        "<div style=\"flex-shrink:0;color:#757575\">Search DuckDuckGo or type a URL</div>"
            .to_owned()
    } else {
        format!(
            "<div style=\"flex-shrink:0;{}\">{}</div>{caret}<div style=\"flex-shrink:0\">{}</div>",
            if t.focused && t.select_all {
                "background:#c2dbff"
            } else {
                ""
            },
            escape_html(&before),
            escape_html(&after),
        )
    };
    let zoom = if (info.zoom - 1.0).abs() > 0.001 {
        format!(
            "<div style=\"flex-shrink:0;margin-left:6px;font-size:12px;color:#474747;background:#e3e3e3;\
             border-radius:9px;padding:1px 7px\">{:.0}%</div>",
            info.zoom * 100.0
        )
    } else {
        String::new()
    };
    let star = format!(
        "<div id=star style=\"flex-shrink:0;width:28px;height:28px;margin-left:4px;margin-right:-8px;\
         display:flex;align-items:center;justify-content:center;font-size:17px;color:{}\">{}</div>",
        if info.bookmarked {
            "#0b57d0"
        } else {
            "#474747"
        },
        if info.bookmarked {
            "\u{2605}"
        } else {
            "\u{2606}"
        }
    );
    format!(
        "<!doctype html><html><body style=\"margin:0;background:#fff;font-family:system-ui,-apple-system,\
         Helvetica,sans-serif;font-size:14px;color:#1f1f1f\"><div style=\"display:flex;align-items:center;\
         height:40px;padding:0 8px;box-sizing:border-box;border-bottom:1px solid #d3d3d3\">{}{}{}\
         <div id=field style=\"flex-grow:1;flex-shrink:1;min-width:0;height:32px;margin-left:6px;\
         box-sizing:border-box;background:{};border:2px solid {};border-radius:16px;padding:0 14px;\
         display:flex;align-items:center;overflow:hidden;white-space:pre\">{security}\
         <div style=\"flex-grow:1;flex-shrink:1;min-width:0;overflow:hidden;display:flex;align-items:center;\
         justify-content:{}\">{text}</div>{zoom}{star}</div></div></body></html>",
        button("back", "\u{2190}", info.can_back, 18),
        button("fwd", "\u{2192}", info.can_forward, 18),
        if info.loading {
            button("reload", "\u{2715}", true, 15)
        } else {
            button("reload", "\u{21bb}", true, 18)
        },
        if t.focused { "#fff" } else { "#f0f4f9" },
        if t.focused { "#0b57d0" } else { "#f0f4f9" },
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
