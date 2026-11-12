//! The clipboard. The window uses the macOS pasteboard (`pbcopy`/`pbpaste`)
//! or, on Linux, the X11 or Wayland clipboard (the `arboard` crate).
//! The headless driver calls [`use_memory`] first, so tests never touch the
//! user's clipboard; its content shows in `state` as `clipboard`.

use std::sync::Mutex;

/// `Some` when the clipboard is in memory (headless).
static MEMORY: Mutex<Option<String>> = Mutex::new(None);

/// Keep the clipboard in this process from now on.
pub fn use_memory() {
    *MEMORY.lock().expect("clipboard lock") = Some(String::new());
}

/// The in-memory clipboard, if it is in use.
pub fn memory() -> Option<String> {
    MEMORY.lock().expect("clipboard lock").clone()
}

pub fn get() -> Option<String> {
    if let Some(m) = MEMORY.lock().expect("clipboard lock").as_ref() {
        return Some(m.clone());
    }
    system::get()
}

pub fn set(text: &str) {
    if let Some(m) = MEMORY.lock().expect("clipboard lock").as_mut() {
        text.clone_into(m);
        return;
    }
    system::set(text);
}

#[cfg(target_os = "macos")]
mod system {
    pub fn get() -> Option<String> {
        let out = std::process::Command::new("pbpaste").output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn set(text: &str) {
        use std::io::Write;
        if let Ok(mut child) = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }
}

#[cfg(target_os = "linux")]
mod system {
    use std::cell::RefCell;

    thread_local! {
        /// One connection for the life of the window thread: on X11 and
        /// Wayland the copied text lives in the process that owns the
        /// selection, so the owner must stay alive.
        static CLIPBOARD: RefCell<Option<arboard::Clipboard>> = const { RefCell::new(None) };
    }

    fn with<R>(f: impl FnOnce(&mut arboard::Clipboard) -> Option<R>) -> Option<R> {
        CLIPBOARD.with_borrow_mut(|c| {
            if c.is_none() {
                *c = arboard::Clipboard::new().ok();
            }
            f(c.as_mut()?)
        })
    }

    pub fn get() -> Option<String> {
        with(|c| c.get_text().ok())
    }

    pub fn set(text: &str) {
        with(|c| c.set_text(text).ok());
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod system {
    pub fn get() -> Option<String> {
        None
    }

    pub fn set(_: &str) {}
}
