//! Window-level keyboard shortcuts, as in Chrome on macOS. The page never
//! sees these chords. Page-level keys (Cmd+F, Cmd+A, Cmd+Left, Space, ...)
//! go to the tab; see `Tab::key`.

use crate::input::{Key, KeyEvent};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Action {
    NewTab,
    NewWindow,
    NewPrivateWindow,
    CloseTab,
    CloseWindow,
    ReopenClosedTab,
    /// Cmd+1..8: the tab at this index (0-based).
    SelectTab(usize),
    /// Cmd+9.
    LastTab,
    NextTab,
    PrevTab,
    Quit,
    Minimize,
    Fullscreen,
    FocusAddress,
    Back,
    Forward,
    Reload,
    HardReload,
    Home,
    History,
    Bookmark,
    Bookmarks,
    ClearHistory,
    ZoomIn,
    ZoomOut,
    ZoomReset,
}

/// The window action for a key chord, if it is one.
pub fn action(ev: &KeyEvent) -> Option<Action> {
    use Action::*;
    let m = ev.mods;
    let k = &ev.key;
    // Ctrl+Tab and Ctrl+Shift+Tab cycle tabs.
    if m.ctrl && !m.cmd && *k == Key::Tab {
        return Some(if m.shift { PrevTab } else { NextTab });
    }
    if !m.cmd {
        return None;
    }
    if m.ctrl {
        return k.is("f").then_some(Fullscreen);
    }
    if m.alt {
        return match k {
            Key::Right => Some(NextTab),
            Key::Left => Some(PrevTab),
            _ if k.is("b") => Some(Bookmarks),
            _ => None,
        };
    }
    let a = match k {
        Key::Backspace | Key::Delete if m.shift => ClearHistory,
        Key::Char(c) => match (c.to_ascii_lowercase().as_str(), m.shift) {
            ("t", false) => NewTab,
            ("t", true) => ReopenClosedTab,
            ("n", false) => NewWindow,
            ("n", true) => NewPrivateWindow,
            ("w", false) => CloseTab,
            ("w", true) => CloseWindow,
            ("q", _) => Quit,
            ("m", false) => Minimize,
            ("l", false) => FocusAddress,
            ("[", false) => Back,
            ("]", false) => Forward,
            // Cmd+Shift+[ and ] (Cmd+{ and }) also switch tabs.
            ("[" | "{", true) => PrevTab,
            ("]" | "}", true) => NextTab,
            ("r", false) => Reload,
            ("r", true) => HardReload,
            ("h", true) => Home,
            ("y", false) => History,
            ("d", false) => Bookmark,
            ("=" | "+", _) => ZoomIn,
            ("-" | "_", _) => ZoomOut,
            ("0", false) => ZoomReset,
            ("9", false) => LastTab,
            (d, false) if d.len() == 1 && ("1"..="8").contains(&d) => {
                SelectTab(d.parse::<usize>().ok()? - 1)
            }
            _ => return None,
        },
        _ => return None,
    };
    Some(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Modifiers;

    fn chord(spec: &str) -> Option<Action> {
        let mut mods = Modifiers::default();
        let mut parts: Vec<&str> = spec.split('+').collect();
        let name = parts.pop().unwrap();
        for p in parts {
            match p {
                "cmd" => mods.cmd = true,
                "shift" => mods.shift = true,
                "alt" => mods.alt = true,
                "ctrl" => mods.ctrl = true,
                _ => panic!("{p}"),
            }
        }
        let key = match name {
            "tab" => Key::Tab,
            "left" => Key::Left,
            "right" => Key::Right,
            "backspace" => Key::Backspace,
            c => Key::Char(c.into()),
        };
        action(&KeyEvent { key, mods })
    }

    #[test]
    fn chrome_chords_map_to_actions() {
        use Action::*;
        assert_eq!(chord("cmd+t"), Some(NewTab));
        assert_eq!(chord("cmd+shift+t"), Some(ReopenClosedTab));
        assert_eq!(chord("cmd+shift+n"), Some(NewPrivateWindow));
        assert_eq!(chord("cmd+1"), Some(SelectTab(0)));
        assert_eq!(chord("cmd+8"), Some(SelectTab(7)));
        assert_eq!(chord("cmd+9"), Some(LastTab));
        assert_eq!(chord("cmd+0"), Some(ZoomReset));
        assert_eq!(chord("cmd+alt+right"), Some(NextTab));
        assert_eq!(chord("ctrl+shift+tab"), Some(PrevTab));
        assert_eq!(chord("cmd+ctrl+f"), Some(Fullscreen));
        assert_eq!(chord("cmd+alt+b"), Some(Bookmarks));
        assert_eq!(chord("cmd+shift+backspace"), Some(ClearHistory));
        assert_eq!(chord("cmd+shift+="), Some(ZoomIn));
        // Page-level chords are not window actions.
        assert_eq!(chord("cmd+left"), None);
        assert_eq!(chord("cmd+f"), None);
        assert_eq!(chord("cmd+a"), None);
        assert_eq!(chord("t"), None);
    }
}
