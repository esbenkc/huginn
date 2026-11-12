//! The clipboard. The window uses the macOS pasteboard (`pbcopy`/`pbpaste`).
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
    let out = std::process::Command::new("pbpaste").output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn set(text: &str) {
    if let Some(m) = MEMORY.lock().expect("clipboard lock").as_mut() {
        text.clone_into(m);
        return;
    }
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
