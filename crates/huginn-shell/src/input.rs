//! Input events of `huginn open`, independent of the window system.
//! `window.rs` maps winit events to these; `headless.rs` parses them from text.

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    /// Command on macOS.
    pub cmd: bool,
    pub shift: bool,
    /// Option on macOS.
    pub alt: bool,
    pub ctrl: bool,
}

impl Modifiers {
    pub fn none(self) -> bool {
        self == Modifiers::default()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    /// Printable text. With cmd, ctrl or alt held: the key's character without
    /// modifiers (`cmd+opt+b` is `Char("b")`, not `"∫"`).
    Char(String),
    Enter,
    /// Backspace (the Mac "delete" key).
    Backspace,
    /// Forward delete.
    Delete,
    Escape,
    Space,
    Tab,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
}

impl Key {
    /// True for `Char(c)` with `c` equal to `s`, ignoring ASCII case.
    pub fn is(&self, s: &str) -> bool {
        matches!(self, Key::Char(c) if c.eq_ignore_ascii_case(s))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct KeyEvent {
    pub key: Key,
    pub mods: Modifiers,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MouseKind {
    Down,
    Up,
    /// Pointer moved; `button` is the button held, if any (a drag).
    Move,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct MouseEvent {
    pub kind: MouseKind,
    pub button: MouseButton,
    /// Whether a button is held (always true for `Down`/`Up`).
    pub pressed: bool,
    /// 1, 2 or 3 for single, double and triple clicks (on `Down` and `Up`).
    pub clicks: u8,
    /// Window CSS px (points), origin at the top-left of the window.
    pub x: f32,
    pub y: f32,
    pub mods: Modifiers,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CursorKind {
    Default,
    Pointer,
    Text,
}
