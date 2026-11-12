//! `huginn open --commands "<commands>"`: drives the same [`Session`] as
//! the window, without a window. Used by tests and on machines where no
//! window may open. Keys and the pointer go to the focused window (new
//! windows take focus). The clipboard is in memory (`clipboard` in `state`).
//! Commands are separated by `;` or newlines:
//!
//! - `wait`: until every engine has answered everything (loads, relayouts)
//! - `sleep <ms>`: handle engine events for `ms` real milliseconds (timers)
//! - `scroll <dy>`: scroll by CSS px (like the wheel)
//! - `swipe <dx> <dy>`: a finished two-finger trackpad swipe (CSS px)
//! - `key <chord>`: e.g. `space`, `shift+space`, `cmd+t`, `cmd+alt+right`,
//!   `ctrl+shift+tab`, `cmd+shift+backspace`, `alt+enter` (`opt` = `alt`)
//! - `type <text>`: type text (into the address bar when it is focused)
//! - `move <x> <y>`: pointer in window CSS px
//! - `click <x> <y> [cmd] [shift] [alt] [ctrl] [middle|right] [double|triple]`
//! - `down <x> <y> [flags]` / `up <x> <y> [flags]`: one half of a click
//! - `dblclick <x> <y>` / `tripleclick <x> <y>`: the full click sequence
//! - `rclick <x> <y>`: right click
//! - `drag <x0> <y0> <x1> <y1>`: press, move twice, release (left button)
//! - `resize <w> <h>`: window size in CSS px
//! - `window <i>`: focus window `i`
//! - `clipboard <text>`: put text on the clipboard
//! - `prefetch`: do idle work until there is none (prints how many strips)
//! - `shot <png>`: compose a frame of the focused window and save it
//! - `state`: print the session state as one JSON line (the focused
//!   window's active tab, its `tabs`, `windows`, ...)
//!
//! `huginn open <url> --script "<commands>"` runs the same commands in the
//! real window (`window.rs`), plus `capture <png>` (a `screencapture` of the
//! focused window), and quits after the last one.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use huginn_net::Url;
use huginn_paint::RgbaImage;

use crate::browser::Browser;
use crate::engine::EngineEvent;
use crate::input::{Key, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseKind};
use crate::session::Session;
use crate::tab::TabId;

pub fn run(
    url: Option<Url>,
    width: f32,
    height: f32,
    scale: f32,
    js: bool,
    commands: &str,
) -> Result<(), String> {
    crate::clipboard::use_memory();
    let (tx, rx) = channel::<(TabId, EngineEvent)>();
    let notify = Arc::new(move |id, ev| {
        let _ = tx.send((id, ev));
    });
    let mut s = Session::new(js, notify, (width, height), scale, true);
    s.open_window(url, false);
    for cmd in split(commands) {
        pump(&mut s, &rx);
        if s.quit {
            break;
        }
        match apply(&mut s, cmd, scale)? {
            Step::Next => {}
            Step::Wait => {
                while s.busy() {
                    let (id, ev) = rx.recv().map_err(|_| "the engine thread died")?;
                    s.handle(id, ev);
                }
            }
            Step::Sleep(d) => {
                let end = Instant::now() + d;
                while let Some(left) = end.checked_duration_since(Instant::now()) {
                    if let Ok((id, ev)) = rx.recv_timeout(left) {
                        s.handle(id, ev);
                    }
                }
            }
        }
        s.settle();
        for w in &mut s.windows {
            // No window system: fullscreen and minimize only show in `state`.
            w.take_window_ops();
        }
    }
    if s.quit {
        println!("{}", serde_json::json!({ "quit": true }));
    }
    Ok(())
}

/// The commands of a script, trimmed, without empty ones.
pub fn split(commands: &str) -> impl Iterator<Item = &str> {
    commands
        .split([';', '\n'])
        .map(str::trim)
        .filter(|c| !c.is_empty())
}

fn pump(s: &mut Session, rx: &Receiver<(TabId, EngineEvent)>) {
    while let Ok((id, ev)) = rx.try_recv() {
        s.handle(id, ev);
    }
}

/// What the driver must do after a command.
pub enum Step {
    Next,
    /// Until no engine is busy (`wait`).
    Wait,
    /// Handle events for this long (`sleep`).
    Sleep(Duration),
}

/// Run one command on the focused window. The real window's `--script`
/// mode uses the same commands (see `window.rs`).
pub fn apply(s: &mut Session, cmd: &str, scale: f32) -> Result<Step, String> {
    let (verb, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
    let rest = rest.trim();
    let bad = || format!("bad command: {cmd}");
    let nums = |n: usize| -> Result<Vec<f32>, String> {
        let v: Vec<f32> = rest
            .split_whitespace()
            .take(n)
            .map_while(|t| t.parse().ok())
            .collect();
        if v.len() == n { Ok(v) } else { Err(bad()) }
    };
    let flags = || rest.split_whitespace().skip(2).collect::<Vec<_>>();
    match verb {
        "wait" => return Ok(Step::Wait),
        "sleep" => {
            return Ok(Step::Sleep(Duration::from_millis(
                rest.parse().map_err(|_| bad())?,
            )));
        }
        "state" => {
            println!("{}", s.state());
            return Ok(Step::Next);
        }
        "window" => {
            let i: usize = rest.parse().map_err(|_| bad())?;
            if i >= s.windows.len() {
                return Err(format!("no window {i}"));
            }
            s.focused = i;
            return Ok(Step::Next);
        }
        "clipboard" => {
            crate::clipboard::set(rest);
            return Ok(Step::Next);
        }
        _ => {}
    }
    let Some(w) = s.windows.get_mut(s.focused) else {
        return Err("no window".into());
    };
    match verb {
        "scroll" => w.scroll_by(rest.parse().map_err(|_| bad())?),
        "swipe" => {
            let v = nums(2)?;
            w.swipe(v[0], v[1]);
        }
        "key" => {
            let ev = parse_key(rest).ok_or_else(bad)?;
            w.key(ev);
        }
        "type" => {
            for c in rest.chars() {
                let key = if c == ' ' {
                    Key::Space
                } else {
                    Key::Char(c.to_string())
                };
                w.key(KeyEvent {
                    key,
                    mods: Modifiers::default(),
                });
            }
        }
        "move" => {
            let v = nums(2)?;
            w.mouse(mouse(MouseKind::Move, v[0], v[1], &[]));
        }
        "click" | "down" | "up" => {
            let v = nums(2)?;
            let f = flags();
            if verb != "up" {
                w.mouse(mouse(MouseKind::Move, v[0], v[1], &[]));
                w.mouse(mouse(MouseKind::Down, v[0], v[1], &f));
            }
            if verb != "down" {
                w.mouse(mouse(MouseKind::Up, v[0], v[1], &f));
            }
        }
        "dblclick" | "tripleclick" | "rclick" => {
            let v = nums(2)?;
            w.mouse(mouse(MouseKind::Move, v[0], v[1], &[]));
            let counts: &[&str] = match verb {
                "dblclick" => &["single", "double"],
                "tripleclick" => &["single", "double", "triple"],
                _ => &["right"],
            };
            for c in counts {
                w.mouse(mouse(MouseKind::Down, v[0], v[1], &[c]));
                w.mouse(mouse(MouseKind::Up, v[0], v[1], &[c]));
            }
        }
        "drag" => {
            let v = nums(4)?;
            let (x0, y0, x1, y1) = (v[0], v[1], v[2], v[3]);
            w.mouse(mouse(MouseKind::Move, x0, y0, &[]));
            w.mouse(mouse(MouseKind::Down, x0, y0, &[]));
            let mut mv = mouse(MouseKind::Move, (x0 + x1) / 2.0, (y0 + y1) / 2.0, &[]);
            mv.pressed = true;
            w.mouse(mv);
            mv.x = x1;
            mv.y = y1;
            w.mouse(mv);
            w.mouse(mouse(MouseKind::Up, x1, y1, &[]));
        }
        "resize" => {
            let v = nums(2)?;
            let (dw, dh) = ((v[0] * scale).round() as u32, (v[1] * scale).round() as u32);
            w.resize(dw, dh, scale);
        }
        "prefetch" => {
            let mut frame = frame_for(w);
            w.compose(&mut frame);
            w.take_dirty();
            let mut n = 0;
            while w.prefetch() {
                n += 1;
                if n > 1000 {
                    return Err("prefetch does not settle".into());
                }
            }
            println!("{}", serde_json::json!({ "prefetched": n }));
        }
        "shot" => {
            let mut frame = frame_for(w);
            w.compose(&mut frame);
            w.take_dirty();
            save_png(&frame, w, rest)?;
        }
        _ => return Err(bad()),
    }
    Ok(Step::Next)
}

/// A mouse event with flags: `cmd`, `shift`, `alt`/`opt`, `ctrl`,
/// `middle`/`right`, `double`/`triple`.
fn mouse(kind: MouseKind, x: f32, y: f32, flags: &[&str]) -> MouseEvent {
    let mut ev = MouseEvent {
        kind,
        button: MouseButton::Left,
        pressed: kind != MouseKind::Move,
        clicks: 1,
        x,
        y,
        mods: Modifiers::default(),
    };
    for f in flags {
        match *f {
            "cmd" => ev.mods.cmd = true,
            "shift" => ev.mods.shift = true,
            "alt" | "opt" => ev.mods.alt = true,
            "ctrl" => ev.mods.ctrl = true,
            "middle" => ev.button = MouseButton::Middle,
            "right" => ev.button = MouseButton::Right,
            "double" => ev.clicks = 2,
            "triple" => ev.clicks = 3,
            _ => {}
        }
    }
    ev
}

fn frame_for(b: &Browser) -> Vec<u32> {
    let (w, h) = b.size();
    vec![0; w as usize * h as usize]
}

pub(crate) fn save_png(frame: &[u32], b: &Browser, path: &str) -> Result<(), String> {
    let (width, height) = b.size();
    let mut data = Vec::with_capacity(frame.len() * 4);
    for &p in frame {
        let [b0, g, r, _] = p.to_le_bytes();
        data.extend_from_slice(&[r, g, b0, 255]);
    }
    let img = RgbaImage {
        width,
        height,
        data,
    };
    let file = std::fs::File::create(path).map_err(|e| format!("cannot create {path}: {e}"))?;
    huginn_paint::encode_png(&img, std::io::BufWriter::new(file))
        .map_err(|e| format!("PNG encoding failed: {e}"))
}

/// `cmd+shift+space` -> Space with cmd and shift.
fn parse_key(spec: &str) -> Option<KeyEvent> {
    let mut mods = Modifiers::default();
    // A trailing `+` is the key itself: `cmd++`.
    let (spec, plus) = match spec.strip_suffix("++") {
        Some(s) => (s, true),
        None => (spec, false),
    };
    let mut parts: Vec<&str> = spec.split('+').collect();
    let name = if plus { "+" } else { parts.pop()? };
    for m in parts {
        match m {
            "cmd" => mods.cmd = true,
            "shift" => mods.shift = true,
            "alt" | "opt" => mods.alt = true,
            "ctrl" => mods.ctrl = true,
            _ => return None,
        }
    }
    let key = match name {
        "enter" => Key::Enter,
        "backspace" => Key::Backspace,
        "delete" => Key::Delete,
        "escape" | "esc" => Key::Escape,
        "space" => Key::Space,
        "tab" => Key::Tab,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "home" => Key::Home,
        "end" => Key::End,
        c if c.chars().count() == 1 => Key::Char(c.to_owned()),
        _ => return None,
    };
    Some(KeyEvent { key, mods })
}
