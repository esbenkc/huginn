//! The tab strip in Forest Mono: flat square tabs with favicon, monospace
//! title and close button, 1 px hairlines between them, the active tab
//! lifted by a 2 px accent rule and joined to the toolbar, a square + button.
//! Tabs shrink as more are added. It is a small HTML document rendered by our
//! own pipeline (over the acrylic backdrop when the material is on); loading
//! spinners are drawn on top each animation frame, so the strip itself is
//! only rendered when its content changes.

use std::time::Instant;

use huginn_layout::{LayoutEngine, Rect};
use huginn_net::{ImageStore, Url};
use huginn_paint::CpuRasterizer;

use crate::engine::escape_html;
use crate::tab::{TABSTRIP_HEIGHT, Tab};
use crate::theme;
use crate::ui::{Pixels, blend, render_glass_snippet, render_snippet};

/// Space at the left of the strip: room for the macOS traffic lights, a small
/// margin where the window has its own title bar.
pub const LEFT_INSET: f32 = if cfg!(target_os = "macos") { 78.0 } else { 8.0 };
const MAX_TAB_WIDTH: f32 = 240.0;
const MIN_TAB_WIDTH: f32 = 36.0;
const PLUS_WIDTH: f32 = 40.0;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum StripHit {
    Tab(usize),
    Close(usize),
    Plus,
}

/// What one tab shows; the strip renders again when any of it changes.
#[derive(Clone, PartialEq)]
struct TabLook {
    title: String,
    loading: bool,
    favicon: Option<usize>,
}

#[derive(Clone, PartialEq)]
struct Look {
    tabs: Vec<TabLook>,
    active: usize,
    hover: Option<StripHit>,
    width: f32,
    private: bool,
}

#[derive(Default)]
pub struct TabStrip {
    pixels: Option<(Pixels, Look)>,
    tabs: Vec<Rect>,
    closes: Vec<Rect>,
    icons: Vec<Rect>,
    plus: Rect,
    pub hover: Option<StripHit>,
}

/// Tab width for `n` tabs in a window `width` CSS px wide.
pub fn tab_width(n: usize, width: f32) -> f32 {
    let avail = width - LEFT_INSET - PLUS_WIDTH - 8.0;
    (avail / n.max(1) as f32).clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH)
}

impl TabStrip {
    pub fn invalidate(&mut self) {
        self.pixels = None;
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<StripHit> {
        let inside = |r: &Rect| x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
        if y >= TABSTRIP_HEIGHT {
            return None;
        }
        if let Some(i) = self.closes.iter().position(inside) {
            return Some(StripHit::Close(i));
        }
        if let Some(i) = self.tabs.iter().position(inside) {
            return Some(StripHit::Tab(i));
        }
        inside(&self.plus).then_some(StripHit::Plus)
    }

    /// Border box of tab `i` in window CSS px.
    pub fn tab_rect(&self, i: usize) -> Option<Rect> {
        self.tabs.get(i).copied()
    }

    /// Render the strip if what it shows changed (also before hit testing).
    pub fn prepare(
        &mut self,
        layout: &mut LayoutEngine,
        raster: &mut CpuRasterizer,
        tabs: &[Tab],
        active: usize,
        private: bool,
        width: f32,
        scale: f32,
    ) {
        let look = Look {
            tabs: tabs
                .iter()
                .map(|t| TabLook {
                    title: t.display_title(),
                    loading: t.is_loading(),
                    favicon: t
                        .favicon
                        .as_ref()
                        .map(|f| std::sync::Arc::as_ptr(f.pixmap()) as *const () as usize),
                })
                .collect(),
            active,
            hover: self.hover,
            width,
            private,
        };
        if self.pixels.as_ref().is_none_or(|(_, shown)| *shown != look) {
            self.render(layout, raster, tabs, &look, scale);
        }
    }

    /// Draw the prepared strip into the top rows of `frame`, then the
    /// spinners of loading tabs.
    pub fn draw(&self, tabs: &[Tab], scale: f32, frame: &mut [u32], stride: usize) {
        let Some((px, look)) = &self.pixels else {
            return;
        };
        px.blit(frame, stride, 0, 0);
        for (i, t) in tabs.iter().enumerate() {
            if let (Some(since), Some(r)) = (t.loading_since(), self.icons.get(i)) {
                spinner(
                    frame,
                    stride,
                    *r,
                    scale,
                    since,
                    theme::accent_rgb(look.private),
                );
            }
        }
    }

    fn render(
        &mut self,
        layout: &mut LayoutEngine,
        raster: &mut CpuRasterizer,
        tabs: &[Tab],
        look: &Look,
        scale: f32,
    ) {
        let mut images = ImageStore::new();
        for (i, t) in tabs.iter().enumerate() {
            if let Some(img) = &t.favicon {
                images.insert(&favicon_url(i), img.clone());
            }
        }
        let glass = theme::backdrop(look.width, scale);
        if let Some(img) = &glass {
            images.insert(&theme::backdrop_url(), img.clone());
        }
        let n = tabs.len();
        let w = tab_width(n, look.width);
        let accent = theme::accent(look.private);
        let line = theme::HAIRLINE;
        let mut html = String::new();
        for (i, tab) in look.tabs.iter().enumerate() {
            let active = i == look.active;
            let hovered =
                matches!(look.hover, Some(StripHit::Tab(h) | StripHit::Close(h)) if h == i);
            // Narrow inactive tabs hide the close button, like Chrome.
            let show_close = active || (hovered && w >= 60.0);
            let show_title = w >= 60.0 || (!show_close && w >= 48.0);
            let bg = if active {
                theme::fill(theme::ACTIVE, 0.6)
            } else if hovered {
                theme::fill(theme::HOVER, 0.5)
            } else {
                "transparent".to_owned()
            };
            let color = if active || hovered {
                theme::TEXT
            } else {
                theme::MUTED
            };
            let icon = if tab.loading {
                String::new()
            } else if tab.favicon.is_some() {
                format!(
                    "<img src=\"{}\" width=16 height=16 style=\"display:block\">",
                    favicon_url(i)
                )
            } else {
                asterisk(accent)
            };
            let close = if show_close {
                let over = matches!(look.hover, Some(StripHit::Close(h)) if h == i);
                format!(
                    "<div id=x{i} style=\"width:18px;height:18px;flex-shrink:0;margin-left:4px;display:flex;\
                     align-items:center;justify-content:center;font-size:15px;line-height:18px;color:{};background:{}\">\
                     \u{d7}</div>",
                    if over { theme::TEXT } else { theme::MUTED },
                    if over { line } else { "transparent" },
                )
            } else {
                String::new()
            };
            let title = if show_title {
                format!(
                    "<div style=\"flex-grow:1;flex-shrink:1;min-width:0;overflow:hidden;white-space:nowrap;\
                     margin-left:8px;font-size:12px;color:{color}\">{}</div>",
                    escape_html(&tab.title)
                )
            } else {
                "<div style=\"flex-grow:1\"></div>".to_owned()
            };
            let rule = if active {
                format!(
                    "<div style=\"position:absolute;left:0;top:0;width:{w:.2}px;height:2px;background:{accent}\"></div>"
                )
            } else {
                String::new()
            };
            let first = if i == 0 {
                format!(
                    "<div style=\"position:absolute;left:0;top:0;width:1px;height:40px;background:{line}\"></div>"
                )
            } else {
                String::new()
            };
            html.push_str(&format!(
                "<div id=t{i} style=\"position:relative;width:{w:.2}px;height:40px;flex-shrink:0;box-sizing:border-box;\
                 display:flex;align-items:center;padding:0 {pad}px;background:{bg}\">\
                 <div id=i{i} style=\"width:16px;height:16px;flex-shrink:0\">{icon}</div>{title}{close}{first}{rule}\
                 <div style=\"position:absolute;right:0;top:0;width:1px;height:40px;background:{line}\"></div></div>",
                pad = if w < 60.0 { 6.0 } else { 10.0 },
            ));
        }
        let plus_over = look.hover == Some(StripHit::Plus);
        let plus_color = if plus_over { theme::TEXT } else { theme::MUTED };
        let badge = if look.private {
            format!(
                "<div style=\"flex-grow:1\"></div><div style=\"align-self:center;margin-right:12px;font-size:10px;\
                 line-height:12px;{};color:{accent};border:1px solid {accent};padding:3px 8px\">Private</div>",
                theme::LABEL
            )
        } else {
            String::new()
        };
        // The hairline under the strip, except under the active tab: it
        // joins the toolbar.
        let a_left = LEFT_INSET + look.active as f32 * w;
        let backdrop = if glass.is_some() {
            format!(
                "<img src=\"{}\" style=\"position:absolute;left:0;top:0;width:{}px;height:{}px\">",
                theme::backdrop_url(),
                look.width,
                theme::BACKDROP_HEIGHT
            )
        } else {
            String::new()
        };
        let doc = format!(
            "<!doctype html><html><body style=\"margin:0;background:{};font-family:{}\">{backdrop}\
             <div style=\"position:relative;display:flex;height:40px;padding-left:{LEFT_INSET}px;box-sizing:border-box\">\
             {html}<div id=plus style=\"position:relative;width:{PLUS_WIDTH}px;height:40px;flex-shrink:0;display:flex;\
             align-items:center;justify-content:center;background:{}\"><div style=\"position:relative;width:12px;height:12px\">\
             <div style=\"position:absolute;left:0;top:5.25px;width:12px;height:1.5px;background:{plus_color}\"></div>\
             <div style=\"position:absolute;left:5.25px;top:0;width:1.5px;height:12px;background:{plus_color}\"></div></div>\
             <div style=\"position:absolute;right:0;top:0;width:1px;height:40px;background:{line}\"></div></div>{badge}</div>\
             <div style=\"position:absolute;left:0;top:39px;width:{a_left:.2}px;height:1px;background:{line}\"></div>\
             <div style=\"position:absolute;left:{:.2}px;top:39px;width:{:.2}px;height:1px;background:{line}\"></div>\
             </body></html>",
            if glass.is_some() {
                "transparent"
            } else {
                theme::STRIP
            },
            theme::FONT,
            if plus_over {
                theme::fill(theme::HOVER, 0.5)
            } else {
                "transparent".to_owned()
            },
            a_left + w,
            (look.width - a_left - w).max(0.0),
        );
        let mut ids: Vec<String> = Vec::with_capacity(n * 3 + 1);
        for i in 0..n {
            ids.push(format!("t{i}"));
        }
        for i in 0..n {
            ids.push(format!("x{i}"));
        }
        for i in 0..n {
            ids.push(format!("i{i}"));
        }
        ids.push("plus".into());
        let idr: Vec<&str> = ids.iter().map(String::as_str).collect();
        let render = if glass.is_some() {
            render_glass_snippet
        } else {
            render_snippet
        };
        let (px, rects) = render(
            layout,
            raster,
            &doc,
            look.width,
            TABSTRIP_HEIGHT,
            &idr,
            &images,
            scale,
        );
        self.tabs = rects[..n].to_vec();
        // Missing close buttons have empty rects: they never hit.
        self.closes = rects[n..2 * n].to_vec();
        self.icons = rects[2 * n..3 * n].to_vec();
        self.plus = rects[3 * n];
        self.pixels = Some((px, look.clone()));
    }
}

fn favicon_url(i: usize) -> Url {
    Url::parse(&format!("huginn-favicon:{i}")).expect("valid URL")
}

/// The mark for pages without a favicon: an accent asterisk of three bars.
fn asterisk(color: &str) -> String {
    let mut s = String::from("<div style=\"position:relative;width:16px;height:16px\">");
    for deg in [0, 60, 120] {
        s.push_str(&format!(
            "<div style=\"position:absolute;left:7px;top:2px;width:2px;height:12px;background:{color};\
             transform:rotate({deg}deg)\"></div>"
        ));
    }
    s.push_str("</div>");
    s
}

/// The loading indicator in the icon box `r` (window CSS px): a 2 x 2 grid
/// of small squares; the lit one steps clockwise, 90 degrees every 120 ms.
fn spinner(frame: &mut [u32], stride: usize, r: Rect, scale: f32, since: Instant, rgb: u32) {
    const CELLS: [(f32, f32); 4] = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
    let lit = (since.elapsed().as_millis() / 120 % 4) as usize;
    let rows = frame.len() / stride;
    let d = |v: f32| (v * scale).round().max(0.0) as usize;
    for (i, (cx, cy)) in CELLS.into_iter().enumerate() {
        let (x, y) = (r.x + 3.0 + cx * 6.0, r.y + 3.0 + cy * 6.0);
        let a = if i == lit { 255 } else { 60 };
        for row in d(y)..d(y + 4.0).min(rows) {
            for p in
                &mut frame[row * stride + d(x).min(stride)..row * stride + d(x + 4.0).min(stride)]
            {
                *p = blend(*p, rgb, a);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_shrink_as_more_are_added() {
        assert_eq!(tab_width(1, 1280.0), MAX_TAB_WIDTH);
        let w5 = tab_width(5, 1280.0);
        let w20 = tab_width(20, 1280.0);
        assert!(w20 < w5 && w5 <= MAX_TAB_WIDTH, "{w5} {w20}");
        assert_eq!(tab_width(200, 1280.0), MIN_TAB_WIDTH);
    }
}
