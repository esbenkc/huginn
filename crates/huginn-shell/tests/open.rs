//! `huginn open --commands`: the window's browser state machine, driven
//! without a window. Link clicks, history, scrolling, fragments, reload, the
//! address bar and error pages, through the real binary and engine thread.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

fn page(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/pages")
        .join(name);
    p.to_str().expect("UTF-8 path").to_owned()
}

/// Run `script` on `target` in an 800x600 window and return the `state` lines.
fn run(target: &str, script: &str) -> Vec<Value> {
    let out = Command::new(env!("CARGO_BIN_EXE_huginn"))
        .args([
            "open",
            target,
            "--width",
            "800",
            "--height",
            "600",
            "--commands",
            script,
        ])
        .output()
        .expect("run huginn");
    assert!(
        out.status.success(),
        "huginn open failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("UTF-8 stdout")
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad JSON line {l:?}: {e}")))
        .collect()
}

fn url(s: &Value) -> &str {
    s["url"].as_str().expect("url")
}

fn scroll(s: &Value) -> f64 {
    s["scroll"].as_f64().expect("scroll")
}

#[test]
fn link_click_then_back_and_forward() {
    // The "Go to B" block link is at document (0, 0, 200, 40); the page area
    // starts below the 40 px tab strip and the 40 px toolbar.
    let s = run(
        &page("a.html"),
        "wait; state; click 50 100; wait; state; key cmd+[; wait; state; key cmd+]; wait; state",
    );
    assert!(url(&s[0]).ends_with("/pages/a.html"), "{}", s[0]);
    assert_eq!(s[0]["title"], "Page A");
    assert!(url(&s[1]).ends_with("/pages/b.html"), "{}", s[1]);
    assert_eq!(s[1]["title"], "Page B");
    assert_eq!(
        (s[1]["history"].as_u64(), s[1]["index"].as_u64()),
        (Some(2), Some(1))
    );
    assert!(url(&s[2]).ends_with("/pages/a.html"), "{}", s[2]);
    assert_eq!(
        (s[2]["history"].as_u64(), s[2]["index"].as_u64()),
        (Some(2), Some(0))
    );
    assert_eq!(s[2]["title"], "Page A");
    assert!(url(&s[3]).ends_with("/pages/b.html"), "{}", s[3]);
}

#[test]
fn keys_scroll_within_the_document() {
    let s = run(
        &page("a.html"),
        "wait; key space; state; key end; state; key up; state; key home; state",
    );
    let max = s[0]["max_scroll"].as_f64().expect("max_scroll");
    assert!(max > 2000.0, "page is taller than the window: {}", s[0]);
    // Space scrolls most of the 520 px page area.
    assert!(scroll(&s[0]) > 400.0 && scroll(&s[0]) < 520.0, "{}", s[0]);
    assert_eq!(scroll(&s[1]), max);
    assert_eq!(scroll(&s[2]), max - 40.0);
    assert_eq!(scroll(&s[3]), 0.0);
}

#[test]
fn fragment_link_scrolls_and_back_restores() {
    // "Jump to bottom" is at document y 40..80. #bottom is the last element,
    // so the scroll position clamps at the end of the document.
    let s = run(
        &page("a.html"),
        "wait; click 50 140; wait; state; key cmd+[; wait; state",
    );
    assert!(url(&s[0]).ends_with("/pages/a.html#bottom"), "{}", s[0]);
    assert_eq!(scroll(&s[0]), s[0]["max_scroll"].as_f64().unwrap());
    assert_eq!(s[0]["history"], 2);
    assert!(url(&s[1]).ends_with("/pages/a.html"), "{}", s[1]);
    assert_eq!(scroll(&s[1]), 0.0);
}

#[test]
fn reload_keeps_the_scroll_position() {
    let s = run(
        &page("a.html"),
        "wait; scroll 700; state; key cmd+r; wait; state",
    );
    assert_eq!(scroll(&s[0]), 700.0);
    assert_eq!(scroll(&s[1]), 700.0);
    assert_eq!(s[1]["history"], 1);
}

#[test]
fn address_bar_navigates() {
    let b = format!("file://{}", page("b.html"));
    let s = run(
        &page("a.html"),
        &format!("wait; key cmd+l; type {b}; state; key enter; wait; state"),
    );
    assert_eq!(s[0]["focused"], true);
    assert_eq!(s[0]["address"], b.as_str());
    assert_eq!(s[1]["focused"], false);
    assert_eq!(url(&s[1]), b);
    assert_eq!(s[1]["title"], "Page B");
    assert_eq!(s[1]["history"], 2);
}

#[test]
fn missing_page_shows_an_error_page_at_its_url() {
    let missing = page("does-not-exist.html");
    let s = run(&missing, "wait; state");
    assert!(url(&s[0]).ends_with("/does-not-exist.html"), "{}", s[0]);
    assert_eq!(s[0]["title"], "Cannot load page");
    assert!(
        s[0]["error"].as_str().is_some_and(|e| !e.is_empty()),
        "{}",
        s[0]
    );
    assert_eq!(s[0]["loading"], false);
}
