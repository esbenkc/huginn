//! In-page interaction through `huginn open --commands`: inline links,
//! selection and copy, find, the context menu, view source, print to PDF
//! and save page. The clipboard is in memory in headless mode.
//!
//! `tests/pages/text.html` (16px/20px text, no body margin) puts its
//! paragraphs at document y 0, 40 and 80; the page area starts at window
//! y 80, so they are at window y 80, 120 and 160.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn page(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/pages")
        .join(name);
    p.to_str().expect("UTF-8 path").to_owned()
}

/// Run `script` on `target` in an 800x600 window and return the `state` lines.
fn run_in(target: &str, script: &str, downloads: Option<&Path>) -> Vec<Value> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_huginn"));
    cmd.args([
        "open",
        target,
        "--width",
        "800",
        "--height",
        "600",
        "--commands",
        script,
    ]);
    if let Some(d) = downloads {
        cmd.env("HUGINN_DOWNLOADS", d);
    }
    let out = cmd.output().expect("run huginn");
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

fn run(target: &str, script: &str) -> Vec<Value> {
    run_in(target, script, None)
}

fn str_of<'a>(s: &'a Value, key: &str) -> &'a str {
    s[key].as_str().unwrap_or_else(|| panic!("{key} in {s}"))
}

#[test]
fn inline_link_inside_a_paragraph_opens() {
    // The link is the first words of the first paragraph: no box of its own.
    let s = run(&page("text.html"), "wait; click 10 90; wait; state");
    assert!(
        str_of(&s[0], "url").ends_with("/pages/b.html?inline"),
        "{}",
        s[0]
    );
}

#[test]
fn double_and_triple_click_select_and_cmd_c_copies() {
    let s = run(
        &page("text.html"),
        "wait; dblclick 5 130; state; key cmd+c; state; tripleclick 5 130; state; key cmd+a; key cmd+c; state",
    );
    assert_eq!(s[0]["selection"], "The", "{}", s[0]);
    assert_eq!(s[1]["clipboard"], "The");
    assert_eq!(
        s[2]["selection"],
        "The quick brown fox jumps over the lazy dog."
    );
    let all = str_of(&s[3], "clipboard");
    assert!(
        all.starts_with("inline link starts this paragraph.\nThe quick"),
        "{all:?}"
    );
    assert!(all.ends_with("The last fox is far below."), "{all:?}");
    assert!(
        all.contains("Second fox, and a bold Fox too."),
        "inline elements keep their spaces: {all:?}"
    );
}

#[test]
fn dragging_selects_text_and_does_not_follow_a_link() {
    let s = run(
        &page("text.html"),
        "wait; drag 2 130 120 130; state; drag 0 90 120 130; state; click 600 300; state",
    );
    let first = str_of(&s[0], "selection");
    assert!(
        first.starts_with("The quick") && first.len() < 30,
        "{first:?}"
    );
    // From the link: a selection, not a navigation.
    assert!(
        str_of(&s[1], "url").ends_with("/pages/text.html"),
        "{}",
        s[1]
    );
    assert!(
        str_of(&s[1], "selection").starts_with("inline link starts this paragraph.\nThe"),
        "{}",
        s[1]
    );
    assert_eq!(
        s[2]["selection"],
        Value::Null,
        "a click collapses the selection"
    );
}

#[test]
fn find_counts_steps_wraps_and_selects_on_close() {
    let s = run(
        &page("text.html"),
        "wait; key cmd+f; type fox; state; key enter; state; key shift+enter; state; \
         key cmd+g; key cmd+g; key cmd+g; state; key cmd+g; state; key escape; state",
    );
    let f = |i: usize| &s[i]["find"];
    assert_eq!(f(0)["open"], true);
    assert_eq!(f(0)["query"], "fox");
    assert_eq!(
        f(0)["count"],
        4,
        "case-insensitive, 'Fox' in <b> included: {}",
        s[0]
    );
    assert_eq!(f(0)["current"], 1);
    let r = f(0)["rect"].as_array().expect("rect of the active match");
    let (y, h) = (r[1].as_f64().unwrap(), r[3].as_f64().unwrap());
    assert!(
        (118.0..=122.0).contains(&y) && h > 10.0,
        "first fox is on the second paragraph line: {r:?}"
    );
    assert_eq!(f(1)["current"], 2);
    assert_eq!(f(2)["current"], 1);
    assert_eq!(f(3)["current"], 4);
    assert!(
        s[3]["scroll"].as_f64().unwrap() > 2000.0,
        "the last match scrolled into view: {}",
        s[3]
    );
    assert_eq!(f(4)["current"], 1, "wraps");
    assert_eq!(s[4]["scroll"].as_f64().unwrap(), 0.0);
    assert_eq!(f(5)["open"], false);
    assert_eq!(
        s[5]["selection"], "fox",
        "the active match is selected when the bar closes"
    );
}

#[test]
fn find_with_no_match_and_cmd_e() {
    let s = run(
        &page("text.html"),
        "wait; key cmd+f; type zebra; state; key escape; dblclick 60 130; key cmd+e; key cmd+g; state",
    );
    assert_eq!(s[0]["find"]["count"], 0);
    assert_eq!(s[0]["find"]["current"], Value::Null);
    assert_eq!(s[1]["find"]["query"], "quick", "{}", s[1]);
    assert_eq!(s[1]["find"]["count"], 1);
}

#[test]
fn context_menu_on_a_link() {
    let s = run(
        &page("text.html"),
        "wait; rclick 10 90; state; key down; key down; key down; key down; key enter; state; \
         rclick 10 90; key down; key enter; wait; state; rclick 600 300; state; key escape; state",
    );
    let items: Vec<&str> = s[0]["menu"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        &items[..4],
        [
            "Open Link in New Tab",
            "Open Link in New Window",
            "Open Link in Private Window",
            "Copy Link Address"
        ]
    );
    assert_eq!(items.len(), 4, "a link menu has only link items: {items:?}");
    assert!(
        str_of(&s[1], "clipboard").ends_with("/pages/b.html?inline"),
        "{}",
        s[1]
    );
    assert_eq!(s[1]["menu"], Value::Null);
    // "Open Link in New Tab" opens a background tab: this tab stays active.
    assert_eq!(s[2]["tabs"].as_array().map(Vec::len), Some(2), "{}", s[2]);
    assert!(
        str_of(&s[2], "url").ends_with("/pages/text.html"),
        "{}",
        s[2]
    );
    let page_items: Vec<&str> = s[3]["menu"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        page_items,
        [
            "(Back)",
            "(Forward)",
            "Reload",
            "Save As\u{2026}",
            "Print\u{2026}",
            "Select All",
            "View Page Source"
        ]
    );
    assert_eq!(s[4]["menu"], Value::Null);
}

#[test]
fn view_source_opens_a_new_tab() {
    let s = run(&page("text.html"), "wait; key cmd+u; wait; state");
    assert!(
        str_of(&s[0], "url").starts_with("view-source:file://"),
        "{}",
        s[0]
    );
    assert_eq!(s[0]["tabs"].as_array().map(Vec::len), Some(2));
    let s = run(
        &page("text.html"),
        "wait; key cmd+u; wait; key cmd+f; type <title>Text page; state",
    );
    assert_eq!(
        s[0]["find"]["count"], 1,
        "the source is shown as text, not markup: {}",
        s[0]
    );
}

#[test]
fn print_writes_a_multi_page_pdf_and_save_writes_the_html() {
    let dir = std::env::temp_dir().join(format!("huginn-interact-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let s = run_in(
        &page("text.html"),
        "wait; key cmd+p; wait; state; key cmd+s; wait; state",
        Some(&dir),
    );
    let pdf = std::fs::read(dir.join("Text page.pdf")).expect("PDF written");
    assert!(pdf.starts_with(b"%PDF-1.4") && pdf.ends_with(b"%%EOF\n"));
    let pages = pdf.windows(11).filter(|w| w == b"/Type /Page").count() - 1; // minus "/Type /Pages"
    // 3140 px of content, 979 px per Letter page.
    assert_eq!(pages, 4, "{}", s[0]);
    assert_eq!(
        str_of(&s[0], "notice"),
        format!(
            "Saved Text page.pdf (4 pages) to {}",
            dir.file_name().unwrap().to_str().unwrap()
        )
    );
    let html = std::fs::read(dir.join("Text page.html")).expect("HTML written");
    assert_eq!(html, std::fs::read(page("text.html")).unwrap());
    assert!(
        str_of(&s[1], "notice").starts_with("Saved Text page.html"),
        "{}",
        s[1]
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn right_click_on_a_word_selects_it_and_offers_copy() {
    let s = run(
        &page("text.html"),
        "wait; rclick 60 130; state; key down; key enter; state",
    );
    assert_eq!(s[0]["selection"], "quick", "{}", s[0]);
    let items: Vec<&str> = s[0]["menu"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        &items[..2],
        ["Copy", "Search DuckDuckGo for \u{201c}quick\u{201d}"]
    );
    assert_eq!(s[1]["clipboard"], "quick");
}
