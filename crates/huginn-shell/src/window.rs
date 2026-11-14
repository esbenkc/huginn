//! `huginn open`: native windows. winit runs the event loop, softbuffer
//! shows the frames that each [`Browser`] composes on the CPU. No GPU device
//! is created. The tab strip is drawn in the title bar area (transparent
//! title bar, full-size content view), with room for the traffic lights.
//! The loop sleeps (`ControlFlow::Wait`) unless a spinner is animating or a
//! strip is being prefetched, so idle windows cost no CPU.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use huginn_net::Url;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::{
    ElementState, KeyEvent as WKeyEvent, MouseButton as WButton, MouseScrollDelta, StartCause,
    TouchPhase, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key as WKey, NamedKey};
#[cfg(target_os = "macos")]
use winit::platform::macos::WindowAttributesExtMacOS;
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{CursorIcon, Fullscreen, Window, WindowId};

use crate::browser::WindowOp;
use crate::engine::EngineEvent;
use crate::input::{CursorKind, Key, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseKind};
use crate::session::Session;
use crate::tab::{LINE_SCROLL, TabId};

type UserEvent = (TabId, EngineEvent);

/// Where frames go: softbuffer, or on macOS with the acrylic material our
/// own layer that keeps alpha (`glass.rs`).
enum Surface {
    // Field order is drop order: the surface before its context.
    Soft {
        surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
        _context: softbuffer::Context<Rc<Window>>,
    },
    #[cfg(target_os = "macos")]
    Glass(crate::glass::GlassSurface),
}

struct Win {
    // Field order is drop order: the surface before the window it draws into.
    surface: Surface,
    window: Rc<Window>,
    /// The session's key for this window's `Browser`.
    key: u64,
    title: String,
    cursor: CursorKind,
    pointer: (f32, f32),
    /// Trackpad movement of the current two-finger gesture, CSS px.
    swipe: (f32, f32),
}

/// `--script`: commands run inside the real window, between frames.
struct Script {
    commands: std::collections::VecDeque<String>,
    wait: Option<ScriptWait>,
}

enum ScriptWait {
    Engines,
    Until(Instant),
}

struct App {
    session: Session,
    script: Option<Script>,
    /// The first window's URL, until the event loop starts.
    start: Option<Option<Url>>,
    wins: HashMap<WindowId, Win>,
    mods: Modifiers,
    error: Option<String>,
}

/// Open a window on `url` (or a new tab page) and run until the last window
/// closes or Cmd+Q. `width` and `height` are in CSS px (points).
/// `script`: commands as for `--commands` (see `headless.rs`), run in the
/// window, plus `capture <png>` (a `screencapture` of the focused window
/// with its shadow left out). The app quits after the last command.
pub fn run(
    url: Option<Url>,
    width: f64,
    height: f64,
    js: bool,
    script: Option<&str>,
) -> Result<(), String> {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .map_err(|e| format!("cannot start the event loop: {e}"))?;
    let proxy = Mutex::new(event_loop.create_proxy());
    let notify = Arc::new(move |id, ev| {
        // Fails only after the event loop exited.
        let _ = proxy.lock().expect("proxy lock").send_event((id, ev));
    });
    // The acrylic material: macOS windows, unless HUGINN_OPAQUE_UI=1.
    crate::theme::set_material(
        cfg!(target_os = "macos") && std::env::var_os("HUGINN_OPAQUE_UI").is_none_or(|v| v != "1"),
    );
    let session = Session::new(js, notify, (width as f32, height as f32), 1.0, false);
    let mut app = App {
        session,
        script: script.map(|s| Script {
            commands: crate::headless::split(s).map(str::to_owned).collect(),
            wait: None,
        }),
        start: Some(url),
        wins: HashMap::new(),
        mods: Modifiers::default(),
        error: None,
    };
    event_loop
        .run_app(&mut app)
        .map_err(|e| format!("event loop failed: {e}"))?;
    app.error.map_or(Ok(()), Err)
}

impl App {
    /// Make the winit windows match the session: create new ones, drop
    /// closed ones, carry out window ops, redraw what changed.
    fn sync(&mut self, el: &ActiveEventLoop) {
        self.session.settle();
        if self.session.quit {
            el.exit();
            return;
        }
        // Windows the session closed.
        let keys: Vec<u64> = self.session.windows.iter().map(|b| b.id).collect();
        self.wins.retain(|_, w| keys.contains(&w.key));
        // Windows the session opened.
        let missing: Vec<u64> = keys
            .iter()
            .copied()
            .filter(|k| !self.wins.values().any(|w| w.key == *k))
            .collect();
        for key in missing {
            if let Err(e) = self.create(el, key) {
                self.error = Some(e);
                el.exit();
                return;
            }
        }
        for win in self.wins.values_mut() {
            let Some(b) = self.session.window_mut(win.key) else {
                continue;
            };
            for op in b.take_window_ops() {
                match op {
                    WindowOp::Minimize => win.window.set_minimized(true),
                    WindowOp::ToggleFullscreen => {
                        // The real state wins: the green button may have changed it.
                        b.fullscreen = win.window.fullscreen().is_none();
                        win.window
                            .set_fullscreen(b.fullscreen.then_some(Fullscreen::Borderless(None)));
                    }
                    WindowOp::DragWindow => {
                        let _ = win.window.drag_window();
                    }
                    _ => {}
                }
            }
            if b.take_dirty() {
                win.window.request_redraw();
            }
            let title = b.window_title();
            if title != win.title {
                win.window.set_title(&title);
                win.title = title;
            }
        }
    }

    fn create(&mut self, el: &ActiveEventLoop, key: u64) -> Result<(), String> {
        let (w, h, scale) = self
            .session
            .windows
            .iter()
            .find(|b| b.id == key)
            .map_or((1280, 800, 1.0), |b| (b.size().0, b.size().1, b.scale()));
        let n = self.wins.len() as f64;
        let attrs = Window::default_attributes()
            .with_title("Huginn")
            .with_inner_size(LogicalSize::new(
                f64::from(w) / f64::from(scale),
                f64::from(h) / f64::from(scale),
            ))
            .with_position(LogicalPosition::new(80.0 + 28.0 * n, 60.0 + 28.0 * n));
        // macOS: the tab strip is drawn in the title bar area. Elsewhere the
        // window keeps its normal title bar and the strip sits under it.
        #[cfg(target_os = "macos")]
        let attrs = attrs
            .with_titlebar_transparent(true)
            .with_title_hidden(true)
            .with_fullsize_content_view(true)
            .with_transparent(crate::theme::material());
        let window = Rc::new(
            el.create_window(attrs)
                .map_err(|e| format!("cannot create window: {e}"))?,
        );
        let soft = |window: &Rc<Window>| -> Result<Surface, String> {
            let context =
                softbuffer::Context::new(window.clone()).map_err(|e| format!("softbuffer: {e}"))?;
            let surface = softbuffer::Surface::new(&context, window.clone())
                .map_err(|e| format!("softbuffer: {e}"))?;
            Ok(Surface::Soft {
                surface,
                _context: context,
            })
        };
        #[cfg(target_os = "macos")]
        let surface = if crate::theme::material() {
            Surface::Glass(crate::glass::GlassSurface::new(&window)?)
        } else {
            soft(&window)?
        };
        #[cfg(not(target_os = "macos"))]
        let surface = soft(&window)?;
        let mut win = Win {
            surface,
            window,
            key,
            title: String::new(),
            cursor: CursorKind::Default,
            pointer: (0.0, 0.0),
            swipe: (0.0, 0.0),
        };
        win.resize_surface(&mut self.session);
        self.wins.insert(win.window.id(), win);
        Ok(())
    }
}

impl Win {
    fn resize_surface(&mut self, session: &mut Session) {
        let size = self.window.inner_size();
        if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
            match &mut self.surface {
                // Only fails on platforms without resize support.
                Surface::Soft { surface, .. } => {
                    let _ = surface.resize(w, h);
                }
                #[cfg(target_os = "macos")]
                Surface::Glass(g) => g.resize(w.get(), h.get(), self.window.scale_factor()),
            }
            if let Some(b) = session.window_mut(self.key) {
                b.resize(w.get(), h.get(), self.window.scale_factor() as f32);
            }
        }
    }

    fn redraw(&mut self, session: &mut Session) -> Result<(), String> {
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        let Some(b) = session.window_mut(self.key) else {
            return Ok(());
        };
        if b.size() != (size.width, size.height) {
            return Ok(());
        }
        let pixels = size.width as usize * size.height as usize;
        match &mut self.surface {
            Surface::Soft { surface, .. } => {
                let mut buffer = surface
                    .buffer_mut()
                    .map_err(|e| format!("softbuffer: {e}"))?;
                if buffer.len() != pixels {
                    return Ok(());
                }
                b.compose(&mut buffer);
                let _span = crate::perf::span("present");
                buffer.present().map_err(|e| format!("softbuffer: {e}"))
            }
            #[cfg(target_os = "macos")]
            Surface::Glass(g) => {
                let buffer = g.buffer();
                if buffer.len() != pixels {
                    return Ok(());
                }
                b.compose(buffer);
                let _span = crate::perf::span("present");
                g.present()
            }
        }
    }

    fn set_cursor(&mut self, cursor: CursorKind) {
        if cursor != self.cursor {
            self.cursor = cursor;
            self.window.set_cursor(match cursor {
                CursorKind::Default => CursorIcon::Default,
                CursorKind::Pointer => CursorIcon::Pointer,
                CursorKind::Text => CursorIcon::Text,
            });
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if let Some(url) = self.start.take() {
            // Load the first page at the screen's scale, not at 1x and then again.
            if let Some(m) = el.primary_monitor() {
                self.session.set_scale(m.scale_factor() as f32);
            }
            self.session.open_window(url, false);
            self.sync(el);
        }
    }

    fn new_events(&mut self, el: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause {
            for b in &mut self.session.windows {
                b.tick();
            }
            self.sync(el);
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, (tab, ev): UserEvent) {
        self.session.handle(tab, ev);
        self.sync(el);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let mods = self.mods;
        let Some(win) = self.wins.get_mut(&id) else {
            return;
        };
        let key = win.key;
        match event {
            WindowEvent::CloseRequested => {
                if let Some(i) = self.session.windows.iter().position(|b| b.id == key) {
                    self.session.windows.remove(i);
                    self.session.focused = self
                        .session
                        .focused
                        .min(self.session.windows.len().saturating_sub(1));
                }
            }
            WindowEvent::Focused(true) => {
                self.session.focus_window(key);
                if let Some(b) = self.session.window_mut(key) {
                    b.minimized = false;
                }
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                win.resize_surface(&mut self.session)
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = win.redraw(&mut self.session) {
                    self.error = Some(e);
                    el.exit();
                    return;
                }
            }
            WindowEvent::ModifiersChanged(m) => {
                let s = m.state();
                // `cmd` is the primary shortcut modifier: Command on macOS,
                // Control elsewhere (where `ctrl` is then the Super key).
                let (cmd, ctrl) = if cfg!(target_os = "macos") {
                    (s.super_key(), s.control_key())
                } else {
                    (s.control_key(), s.super_key())
                };
                self.mods = Modifiers {
                    cmd,
                    shift: s.shift_key(),
                    alt: s.alt_key(),
                    ctrl,
                };
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let ev = native_chord(&event, mods)
                    .or_else(|| map_key(&event, mods).map(|key| KeyEvent { key, mods }));
                if let (Some(ev), Some(b)) = (ev, self.session.window_mut(key)) {
                    b.key(ev);
                }
            }
            WindowEvent::MouseWheel { delta, phase, .. } => {
                let s = win.window.scale_factor() as f32;
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -y * LINE_SCROLL,
                    MouseScrollDelta::PixelDelta(p) => {
                        // Trackpad: also sum the swipe for back/forward.
                        match phase {
                            TouchPhase::Started => win.swipe = (0.0, 0.0),
                            TouchPhase::Moved => {
                                win.swipe.0 += p.x as f32 / s;
                                win.swipe.1 += p.y as f32 / s;
                            }
                            TouchPhase::Ended | TouchPhase::Cancelled => {
                                let (dx, dy) = std::mem::take(&mut win.swipe);
                                if let Some(b) = self.session.window_mut(key) {
                                    b.swipe(dx, dy);
                                }
                            }
                        }
                        -(p.y as f32) / s
                    }
                };
                if let Some(b) = self.session.window_mut(key) {
                    b.wheel(dy);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let s = win.window.scale_factor();
                win.pointer = ((position.x / s) as f32, (position.y / s) as f32);
                let (x, y) = win.pointer;
                if let Some(b) = self.session.window_mut(key) {
                    let cursor = b.mouse(MouseEvent {
                        kind: MouseKind::Move,
                        button: MouseButton::Left,
                        pressed: false,
                        clicks: 0,
                        x,
                        y,
                        mods,
                    });
                    win.set_cursor(cursor);
                }
            }
            WindowEvent::CursorLeft { .. } => {
                if let Some(b) = self.session.window_mut(key) {
                    b.mouse_leave();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let (x, y) = win.pointer;
                let pressed = state == ElementState::Pressed;
                let button = match button {
                    WButton::Left => Some(MouseButton::Left),
                    WButton::Middle => Some(MouseButton::Middle),
                    WButton::Right => Some(MouseButton::Right),
                    WButton::Back | WButton::Forward => {
                        if pressed && let Some(b) = self.session.window_mut(key) {
                            b.key(KeyEvent {
                                key: Key::Char(
                                    if button == WButton::Back { "[" } else { "]" }.into(),
                                ),
                                mods: Modifiers {
                                    cmd: true,
                                    ..Modifiers::default()
                                },
                            });
                        }
                        None
                    }
                    WButton::Other(_) => None,
                };
                if let (Some(button), Some(b)) = (button, self.session.window_mut(key)) {
                    let cursor = b.mouse(MouseEvent {
                        kind: if pressed {
                            MouseKind::Down
                        } else {
                            MouseKind::Up
                        },
                        button,
                        pressed: true,
                        clicks: 0,
                        x,
                        y,
                        mods,
                    });
                    win.set_cursor(cursor);
                }
            }
            _ => {}
        }
        self.sync(el);
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.script.is_some() {
            if let Err(e) = self.step_script(el) {
                self.error = Some(e);
                el.exit();
                return;
            }
        }
        // Idle work: one strip just outside the viewport, then sleep.
        let mut busy = false;
        let mut next = None;
        for b in &mut self.session.windows {
            busy |= b.prefetch();
            if let Some(t) = b.next_frame() {
                next = Some(next.map_or(t, |n: std::time::Instant| n.min(t)));
            }
        }
        if let Some(Script {
            wait: Some(ScriptWait::Until(t)),
            ..
        }) = &self.script
        {
            next = Some(next.map_or(*t, |n: Instant| n.min(*t)));
        }
        el.set_control_flow(match (busy, next) {
            (true, _) => ControlFlow::Poll,
            (false, Some(t)) => ControlFlow::WaitUntil(t),
            (false, None) => ControlFlow::Wait,
        });
    }
}

impl App {
    /// Run script commands until one has to wait for engines or time.
    fn step_script(&mut self, el: &ActiveEventLoop) -> Result<(), String> {
        let Some(script) = &mut self.script else {
            return Ok(());
        };
        match script.wait {
            Some(ScriptWait::Engines) if self.session.busy() => return Ok(()),
            Some(ScriptWait::Until(t)) if Instant::now() < t => return Ok(()),
            _ => script.wait = None,
        }
        // Redraw before the next command, so `capture` sees the current frame.
        for win in self.wins.values_mut() {
            win.redraw(&mut self.session)?;
        }
        let Some(cmd) = script.commands.pop_front() else {
            el.exit();
            return Ok(());
        };
        let (verb, rest) = cmd.split_once(' ').unwrap_or((&cmd, ""));
        let focused = self.session.windows.get(self.session.focused).map(|b| b.id);
        let win = self.wins.values_mut().find(|w| Some(w.key) == focused);
        match (verb, win) {
            ("capture", Some(win)) => {
                capture(&win.window, &mut self.session, win.key, rest.trim())?
            }
            ("resize", Some(win)) => {
                let v: Vec<f64> = rest
                    .split_whitespace()
                    .filter_map(|t| t.parse().ok())
                    .collect();
                if let [w, h] = v[..] {
                    let _ = win.window.request_inner_size(LogicalSize::new(w, h));
                }
            }
            _ => match crate::headless::apply(&mut self.session, &cmd, 1.0)? {
                crate::headless::Step::Next => {}
                crate::headless::Step::Wait => script.wait = Some(ScriptWait::Engines),
                crate::headless::Step::Sleep(d) => {
                    script.wait = Some(ScriptWait::Until(Instant::now() + d))
                }
            },
        }
        self.sync(el);
        Ok(())
    }
}

/// Save a screenshot of `window` (without its shadow) with `screencapture`.
#[cfg(target_os = "macos")]
fn capture(window: &Window, _: &mut Session, _: u64, path: &str) -> Result<(), String> {
    let id = window_number(window).ok_or("no window number")?;
    let status = std::process::Command::new("screencapture")
        .args(["-x", "-o", "-l", &id.to_string(), path])
        .status()
        .map_err(|e| format!("screencapture: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("screencapture failed for {path}"))
    }
}

#[cfg(target_os = "macos")]
/// The window server's number for `window` (`-[NSWindow windowNumber]`).
fn window_number(window: &Window) -> Option<isize> {
    use std::ffi::{c_char, c_void};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    #[link(name = "objc")]
    unsafe extern "C" {
        fn sel_registerName(name: *const c_char) -> *const c_void;
        fn objc_msgSend();
    }
    let RawWindowHandle::AppKit(h) = window.window_handle().ok()?.as_raw() else {
        return None;
    };
    // SAFETY: `ns_view` is the window's live NSView; `window` and
    // `windowNumber` take no arguments and return an object and an NSInteger.
    unsafe {
        let send_obj: unsafe extern "C" fn(*const c_void, *const c_void) -> *const c_void =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let send_int: unsafe extern "C" fn(*const c_void, *const c_void) -> isize =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let ns_window = send_obj(h.ns_view.as_ptr(), sel_registerName(c"window".as_ptr()));
        (!ns_window.is_null())
            .then(|| send_int(ns_window, sel_registerName(c"windowNumber".as_ptr())))
    }
}

/// Save a screenshot of the window's content: the frame the browser composes
/// (the same pixels the window shows, without the system title bar).
#[cfg(not(target_os = "macos"))]
fn capture(_: &Window, session: &mut Session, key: u64, path: &str) -> Result<(), String> {
    let b = session.window_mut(key).ok_or("no window")?;
    let (w, h) = b.size();
    let mut frame = vec![0u32; w as usize * h as usize];
    b.compose(&mut frame);
    crate::headless::save_png(&frame, b, path)
}

/// Chrome's Linux and Windows chords that differ from its macOS ones, as the
/// macOS chord the shell handles (`shortcuts.rs`, `Tab::key`). `mods` has
/// Control in `cmd` here.
#[cfg(not(target_os = "macos"))]
fn native_chord(event: &WKeyEvent, mods: Modifiers) -> Option<KeyEvent> {
    let cmd = Modifiers {
        cmd: true,
        ..Modifiers::default()
    };
    let ctrl = Modifiers {
        ctrl: true,
        shift: mods.shift,
        ..Modifiers::default()
    };
    let ch = |c: &str| Key::Char(c.into());
    let plain = !mods.cmd && !mods.alt && !mods.ctrl;
    let only_cmd = mods.cmd && !mods.alt && !mods.ctrl;
    let only_alt = mods.alt && !mods.cmd && !mods.ctrl && !mods.shift;
    let (key, mods) = match &event.logical_key {
        // Reload and hard reload.
        WKey::Named(NamedKey::F5) if !mods.alt && !mods.ctrl => (
            ch("r"),
            Modifiers {
                shift: mods.shift || mods.cmd,
                ..cmd
            },
        ),
        WKey::Named(NamedKey::F11) if plain => (ch("f"), Modifiers { ctrl: true, ..cmd }),
        WKey::Named(NamedKey::F6) if plain => (ch("l"), cmd),
        // Back and forward.
        WKey::Named(NamedKey::ArrowLeft) if only_alt => (ch("["), cmd),
        WKey::Named(NamedKey::ArrowRight) if only_alt => (ch("]"), cmd),
        WKey::Named(NamedKey::Home) if only_alt => (ch("h"), Modifiers { shift: true, ..cmd }),
        // Tab cycling.
        WKey::Named(NamedKey::Tab) if only_cmd => (Key::Tab, ctrl),
        WKey::Named(NamedKey::PageDown) if only_cmd && !mods.shift => (Key::Tab, ctrl),
        WKey::Named(NamedKey::PageUp) if only_cmd && !mods.shift => (
            Key::Tab,
            Modifiers {
                shift: true,
                ..ctrl
            },
        ),
        WKey::Character(_) if only_cmd || only_alt => {
            let WKey::Character(c) = event.key_without_modifiers() else {
                return None;
            };
            match (c.to_ascii_lowercase().as_str(), mods.shift) {
                // History, bookmark manager, focus the address bar.
                ("h", false) if only_cmd => (ch("y"), cmd),
                ("o", true) if only_cmd => (ch("b"), Modifiers { alt: true, ..cmd }),
                ("d", false) if only_alt => (ch("l"), cmd),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(KeyEvent { key, mods })
}

#[cfg(target_os = "macos")]
fn native_chord(_: &WKeyEvent, _: Modifiers) -> Option<KeyEvent> {
    None
}

fn map_key(event: &WKeyEvent, mods: Modifiers) -> Option<Key> {
    if let WKey::Named(n) = &event.logical_key {
        let key = match n {
            NamedKey::Enter => Key::Enter,
            NamedKey::Backspace => Key::Backspace,
            NamedKey::Delete => Key::Delete,
            NamedKey::Escape => Key::Escape,
            NamedKey::Space => Key::Space,
            NamedKey::Tab => Key::Tab,
            NamedKey::ArrowUp => Key::Up,
            NamedKey::ArrowDown => Key::Down,
            NamedKey::ArrowLeft => Key::Left,
            NamedKey::ArrowRight => Key::Right,
            NamedKey::PageUp => Key::PageUp,
            NamedKey::PageDown => Key::PageDown,
            NamedKey::Home => Key::Home,
            NamedKey::End => Key::End,
            _ => return None,
        };
        return Some(key);
    }
    if mods.cmd || mods.ctrl {
        // Shortcuts: the key's character, not the text it would type
        // (Cmd+Opt+B is "b", not "∫").
        return match event.key_without_modifiers() {
            WKey::Character(c) => Some(Key::Char(c.to_string())),
            _ => None,
        };
    }
    event
        .text
        .as_ref()
        .filter(|t| !t.chars().any(char::is_control))
        .map(|t| Key::Char(t.to_string()))
}
