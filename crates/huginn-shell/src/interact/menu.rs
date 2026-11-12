//! A drawn menu (context menu, `<select>` dropdown): a small HTML document
//! rendered by our pipeline, light macOS style. Coordinates are window CSS px.

use huginn_layout::Rect;

use crate::engine::escape_html;

#[derive(Clone, Debug, PartialEq)]
pub struct MenuItem {
    pub label: String,
    pub enabled: bool,
    /// A line above this item.
    pub separator: bool,
    pub checked: bool,
}

impl MenuItem {
    pub fn new(label: impl Into<String>) -> MenuItem {
        MenuItem {
            label: label.into(),
            enabled: true,
            separator: false,
            checked: false,
        }
    }

    pub fn enabled(mut self, on: bool) -> MenuItem {
        self.enabled = on;
        self
    }

    pub fn after_separator(mut self) -> MenuItem {
        self.separator = true;
        self
    }
}

/// What a key did to an open menu.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MenuKey {
    None,
    Activate(usize),
    Close,
}

pub const ITEM_HEIGHT: f32 = 22.0;
const SEPARATOR_HEIGHT: f32 = 11.0;
const PAD_Y: f32 = 5.0;
const MIN_WIDTH: f32 = 180.0;

pub struct Menu {
    pub items: Vec<MenuItem>,
    /// Top-left corner, window CSS px (after flipping to fit the window).
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub hovered: Option<usize>,
}

impl Menu {
    /// A menu at (`x`, `y`) that fits in a `win_w` x `win_h` window: it
    /// opens to the left / above the point when it would not fit.
    pub fn new(items: Vec<MenuItem>, x: f32, y: f32, win_w: f32, win_h: f32) -> Menu {
        let longest = items
            .iter()
            .map(|i| i.label.chars().count())
            .max()
            .unwrap_or(0) as f32;
        let width = (longest * 7.2 + 44.0).clamp(MIN_WIDTH, 420.0);
        let mut m = Menu {
            items,
            x,
            y,
            width,
            hovered: None,
        };
        let h = m.height();
        if m.x + width > win_w {
            m.x = (x - width).max(0.0);
        }
        if m.y + h > win_h {
            m.y = (y - h).max(0.0);
        }
        m
    }

    pub fn height(&self) -> f32 {
        2.0 * PAD_Y
            + self
                .items
                .iter()
                .map(|i| ITEM_HEIGHT + if i.separator { SEPARATOR_HEIGHT } else { 0.0 })
                .sum::<f32>()
    }

    pub fn rect(&self) -> Rect {
        Rect {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height(),
        }
    }

    /// The item row at window point (`x`, `y`) (enabled or not).
    pub fn item_at(&self, x: f32, y: f32) -> Option<usize> {
        if x < self.x || x >= self.x + self.width {
            return None;
        }
        let mut top = self.y + PAD_Y;
        for (i, it) in self.items.iter().enumerate() {
            if it.separator {
                top += SEPARATOR_HEIGHT;
            }
            if y >= top && y < top + ITEM_HEIGHT {
                return Some(i);
            }
            top += ITEM_HEIGHT;
        }
        None
    }

    /// Track the pointer. Returns true if the highlight changed.
    pub fn hover(&mut self, x: f32, y: f32) -> bool {
        let h = self.item_at(x, y).filter(|&i| self.items[i].enabled);
        std::mem::replace(&mut self.hovered, h) != h
    }

    pub fn key(&mut self, key: &crate::input::Key) -> MenuKey {
        use crate::input::Key;
        let n = self.items.len();
        let step = |from: Option<usize>, down: bool| -> Option<usize> {
            let mut i = from;
            for _ in 0..n {
                let next = match (i, down) {
                    (None, true) => 0,
                    (None, false) => n - 1,
                    (Some(k), true) => (k + 1) % n,
                    (Some(k), false) => (k + n - 1) % n,
                };
                if self.items[next].enabled {
                    return Some(next);
                }
                i = Some(next);
            }
            None
        };
        match key {
            Key::Escape => MenuKey::Close,
            Key::Down if n > 0 => {
                self.hovered = step(self.hovered, true);
                MenuKey::None
            }
            Key::Up if n > 0 => {
                self.hovered = step(self.hovered, false);
                MenuKey::None
            }
            Key::Enter | Key::Space => self.hovered.map_or(MenuKey::Close, MenuKey::Activate),
            _ => MenuKey::None,
        }
    }

    /// The menu as an HTML document `width` x `height` CSS px.
    pub fn html(&self) -> String {
        let mut rows = String::new();
        for (i, it) in self.items.iter().enumerate() {
            if it.separator {
                rows.push_str("<div style=\"height:1px;margin:5px 0;background:#e3e3e3\"></div>");
            }
            let (bg, fg) = match (self.hovered == Some(i), it.enabled) {
                (true, _) => ("#0a63d8", "#fff"),
                (false, true) => ("transparent", "#1d1d1f"),
                (false, false) => ("transparent", "#a7a7a7"),
            };
            rows.push_str(&format!(
                "<div style=\"height:{ITEM_HEIGHT}px;line-height:{ITEM_HEIGHT}px;margin:0 5px;padding:0 10px 0 21px;\
                 border-radius:4px;background:{bg};color:{fg};white-space:pre;overflow:hidden;position:relative\">\
                 {}{}</div>",
                if it.checked { "<span style=\"position:absolute;left:7px\">\u{2713}</span>" } else { "" },
                escape_html(&it.label)
            ));
        }
        format!(
            "<!doctype html><html><body style=\"margin:0;font:13px system-ui,-apple-system,Helvetica,sans-serif\">\
             <div style=\"box-sizing:border-box;width:{w}px;height:{h}px;padding:{PAD_Y}px 0;background:#f6f6f6;\
             border:1px solid #c8c8c8;border-radius:7px;overflow:hidden\">{rows}</div></body></html>",
            w = self.width,
            h = self.height(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Key;

    fn menu() -> Menu {
        Menu::new(
            vec![
                MenuItem::new("Back").enabled(false),
                MenuItem::new("Reload"),
                MenuItem::new("Copy").after_separator(),
            ],
            100.0,
            50.0,
            800.0,
            600.0,
        )
    }

    #[test]
    fn rows_are_hit_below_separators() {
        let m = menu();
        assert_eq!(m.item_at(110.0, 50.0 + PAD_Y + 1.0), Some(0));
        assert_eq!(m.item_at(110.0, 50.0 + PAD_Y + ITEM_HEIGHT + 1.0), Some(1));
        // The separator gap belongs to no item.
        assert_eq!(
            m.item_at(110.0, 50.0 + PAD_Y + 2.0 * ITEM_HEIGHT + 2.0),
            None
        );
        assert_eq!(
            m.item_at(
                110.0,
                50.0 + PAD_Y + 2.0 * ITEM_HEIGHT + SEPARATOR_HEIGHT + 1.0
            ),
            Some(2)
        );
        assert_eq!(m.item_at(99.0, 60.0), None);
    }

    #[test]
    fn menus_flip_to_fit_the_window() {
        let m = Menu::new(vec![MenuItem::new("A")], 790.0, 590.0, 800.0, 600.0);
        assert!(m.x + m.width <= 800.0 && m.y + m.height() <= 600.0);
    }

    #[test]
    fn keys_skip_disabled_items() {
        let mut m = menu();
        assert_eq!(m.key(&Key::Down), MenuKey::None);
        assert_eq!(m.hovered, Some(1), "Back is disabled");
        m.key(&Key::Down);
        m.key(&Key::Down);
        assert_eq!(m.hovered, Some(1), "wraps past the disabled first item");
        assert_eq!(m.key(&Key::Enter), MenuKey::Activate(1));
        assert_eq!(m.key(&Key::Escape), MenuKey::Close);
        assert!(
            !m.hover(110.0, 50.0 + PAD_Y + 1.0) || m.hovered.is_none(),
            "disabled rows do not highlight"
        );
    }
}
