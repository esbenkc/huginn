//! The tab strip, drawn like Chrome's light theme: tabs with favicon, title
//! and close button, the active tab joined to the toolbar, a + button. Tabs
//! shrink as more are added. It is a small HTML document rendered by our
//! own pipeline; loading spinners are drawn on top each animation frame, so
//! the strip itself is only rendered when its content changes.

use std::time::Instant;

use huginn_layout::{LayoutEngine, Rect};
use huginn_net::{ImageStore, Url};
use huginn_paint::CpuRasterizer;

use crate::engine::escape_html;
use crate::tab::{TABSTRIP_HEIGHT, Tab};
use crate::ui::{Pixels, blend, render_snippet};

/// Room for the macOS traffic lights at the left of the strip.
pub const LEFT_INSET: f32 = 78.0;
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
        let Some((px, _)) = &self.pixels else { return };
        px.blit(frame, stride, 0, 0);
        for (i, t) in tabs.iter().enumerate() {
            if let (Some(since), Some(r)) = (t.loading_since(), self.icons.get(i)) {
                spinner(frame, stride, *r, scale, since);
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
        let n = tabs.len();
        let w = tab_width(n, look.width);
        let (strip_bg, plus_color) = if look.private {
            ("#3c4043", "#e8eaed")
        } else {
            ("#dde3ea", "#1f1f1f")
        };
        let mut html = String::new();
        for (i, tab) in look.tabs.iter().enumerate() {
            let active = i == look.active;
            let hovered =
                matches!(look.hover, Some(StripHit::Tab(h) | StripHit::Close(h)) if h == i);
            // Chrome hides the close button of narrow inactive tabs.
            let show_close = active || (hovered && w >= 60.0);
            let show_title = w >= 60.0 || (!show_close && w >= 48.0);
            let bg = if active {
                "#fff"
            } else if hovered {
                if look.private { "#4a4c50" } else { "#eef0f3" }
            } else {
                "transparent"
            };
            let color = if active || !look.private {
                "#1f1f1f"
            } else {
                "#e8eaed"
            };
            // A separator between two inactive, not hovered tabs.
            let next_plain = i + 1 < n
                && i + 1 != look.active
                && !matches!(look.hover, Some(StripHit::Tab(h) | StripHit::Close(h)) if h == i + 1);
            let sep = if !active && !hovered && next_plain {
                "#a8abaf"
            } else {
                "transparent"
            };
            let icon = if tab.loading {
                String::new()
            } else if tab.favicon.is_some() {
                format!(
                    "<img src=\"{}\" width=16 height=16 style=\"display:block\">",
                    favicon_url(i)
                )
            } else {
                globe(if active || !look.private {
                    "#5f6368"
                } else {
                    "#bdc1c6"
                })
            };
            let close = if show_close {
                format!(
                    "<div id=x{i} style=\"width:18px;height:18px;flex-shrink:0;margin-left:4px;border-radius:9px;\
                     display:flex;align-items:center;justify-content:center;font-size:11px;color:{color};{}\">\u{2715}</div>",
                    if matches!(look.hover, Some(StripHit::Close(h)) if h == i) {
                        "background:#d5d8dc"
                    } else {
                        ""
                    }
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
            html.push_str(&format!(
                "<div id=t{i} style=\"position:relative;width:{w:.2}px;height:34px;flex-shrink:0;box-sizing:border-box;\
                 display:flex;align-items:center;padding:0 {pad}px;background:{bg};border-radius:8px 8px 0 0\">\
                 <div id=i{i} style=\"width:16px;height:16px;flex-shrink:0\">{icon}</div>{title}{close}\
                 <div style=\"position:absolute;right:0;top:9px;width:1px;height:16px;background:{sep}\"></div></div>",
                pad = if w < 60.0 { 6.0 } else { 10.0 },
            ));
        }
        let plus_bg = if look.hover == Some(StripHit::Plus) {
            if look.private { "#5f6368" } else { "#c9cfd6" }
        } else {
            "transparent"
        };
        let badge = if look.private {
            "<div style=\"flex-grow:1\"></div><div style=\"align-self:center;margin-right:12px;font-size:12px;\
             color:#e8eaed;background:#5f6368;border-radius:10px;padding:2px 10px\">Private</div>"
        } else {
            ""
        };
        let doc = format!(
            "<!doctype html><html><body style=\"margin:0;background:{strip_bg};font-family:system-ui,-apple-system,\
             Helvetica,sans-serif\"><div style=\"display:flex;align-items:flex-end;height:40px;\
             padding-left:{LEFT_INSET}px;box-sizing:border-box\">{html}<div id=plus style=\"width:28px;height:28px;\
             flex-shrink:0;margin:0 0 3px 6px;border-radius:14px;display:flex;align-items:center;justify-content:center;\
             font-size:20px;font-weight:300;color:{plus_color};background:{plus_bg}\">+</div>{badge}</div></body></html>"
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
        let (px, rects) = render_snippet(
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

/// A globe drawn with CSS: a circle, a meridian ellipse and the equator.
fn globe(color: &str) -> String {
    format!(
        "<div style=\"position:relative;width:16px;height:16px\">\
         <div style=\"position:absolute;left:1px;top:1px;width:11px;height:11px;border:1.5px solid {color};border-radius:8px\"></div>\
         <div style=\"position:absolute;left:5px;top:1px;width:3px;height:11px;border:1.5px solid {color};border-radius:4px\"></div>\
         <div style=\"position:absolute;left:1px;top:7px;width:14px;height:1.5px;background:{color}\"></div></div>"
    )
}

/// Chrome's loading throbber: a blue arc turning once a second, in `r` (window CSS px).
fn spinner(frame: &mut [u32], stride: usize, r: Rect, scale: f32, since: Instant) {
    let size = 16.0 * scale;
    let (cx, cy) = ((r.x + 8.0) * scale, (r.y + 8.0) * scale);
    let radius = 6.5 * scale;
    let half = 1.0 * scale;
    let t = since.elapsed().as_secs_f32();
    let start = (t * std::f32::consts::TAU) % std::f32::consts::TAU;
    let sweep = std::f32::consts::PI * (1.0 + 0.5 * (t * 2.0).sin());
    let rows = frame.len() / stride;
    let (x0, y0) = (
        (cx - size / 2.0).max(0.0) as usize,
        (cy - size / 2.0).max(0.0) as usize,
    );
    for y in y0..((cy + size / 2.0) as usize).min(rows) {
        for x in x0..((cx + size / 2.0) as usize).min(stride) {
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            let d = (dx * dx + dy * dy).sqrt();
            let cover = (half + 0.5 - (d - radius).abs()).clamp(0.0, 1.0);
            if cover <= 0.0 {
                continue;
            }
            let a = (dy.atan2(dx) - start).rem_euclid(std::f32::consts::TAU);
            if a > sweep {
                continue;
            }
            let p = &mut frame[y * stride + x];
            *p = blend(*p, 0x000B_57D0, (cover * 255.0) as u32);
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
