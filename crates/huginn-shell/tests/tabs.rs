//! Tabs, windows and Chrome's keyboard and mouse shortcuts, through the real
//! binary (`huginn open --commands`) with real engine threads. Window
//! layout at 800x600: tab strip y 0..40 (tabs from x 78), toolbar y 40..80
//! (back, forward, reload at x 24, 58, 92), page from y 80.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

fn page(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/pages")
        .join(name);
    p.to_str().expect("UTF-8 path").to_owned()
}

fn file_url(name: &str) -> String {
    format!("file://{}", page(name))
}

/// Run `script` on `target` in an 800x600 window and return the JSON lines.
fn run_env(target: &str, script: &str, env: &[(&str, &str)]) -> Vec<Value> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_huginn"));
    cmd.args([
        "open",
        target,
        "--width",
        "800",
        "--height",
        "600",
        "--no-js",
        "--commands",
        script,
    ])
    // Searches and favicons must not leave the machine.
    .env("HUGINN_NET_LOOPBACK_ONLY", "1")
    .env_remove("HUGINN_PROFILE_DIR")
    .env_remove("HUGINN_LIVE_TABS");
    for (k, v) in env {
        cmd.env(k, v);
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
    run_env(target, script, &[])
}

fn url(s: &Value) -> &str {
    s["url"].as_str().expect("url")
}

fn tab_urls(s: &Value) -> Vec<String> {
    s["tabs"]
        .as_array()
        .expect("tabs")
        .iter()
        .map(|t| {
            t["url"]
                .as_str()
                .unwrap_or("")
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_owned()
        })
        .collect()
}

fn active(s: &Value) -> u64 {
    s["active_tab"].as_u64().expect("active_tab")
}

#[test]
fn new_tab_opens_the_new_tab_page_with_the_address_bar_focused() {
    let b = file_url("b.html");
    let s = run(
        &page("a.html"),
        &format!("wait; key cmd+t; wait; state; type {b}; key enter; wait; state"),
    );
    assert_eq!(s[0]["tabs"].as_array().unwrap().len(), 2);
    assert_eq!(active(&s[0]), 1);
    assert_eq!(url(&s[0]), "huginn://newtab/");
    assert_eq!(s[0]["title"], "New Tab");
    assert_eq!(s[0]["focused"], true);
    assert_eq!(s[0]["address"], "");
    assert_eq!(url(&s[1]), b);
    assert_eq!(tab_urls(&s[1]), ["a.html", "b.html"]);
}

#[test]
fn tab_selection_chords() {
    let s = run(
        &page("a.html"),
        "wait; key cmd+t; key cmd+t; key cmd+t; state; key cmd+1; state; key cmd+9; state; \
         key cmd+2; state; key cmd+alt+right; state; key cmd+alt+left; state; key ctrl+tab; state; \
         key ctrl+shift+tab; state; key cmd+shift+]; state; key cmd+shift+[; state; key cmd+1; key cmd+alt+left; state",
    );
    let got: Vec<u64> = s.iter().map(active).collect();
    assert_eq!(got, [3, 0, 3, 1, 2, 1, 2, 1, 2, 1, 3]);
}

#[test]
fn closing_tabs_activates_the_right_neighbour_and_the_last_tab_closes_the_window() {
    let s = run(
        &page("a.html"),
        "wait; key cmd+t; key cmd+t; key cmd+2; key cmd+w; state; key cmd+w; state; key cmd+w; state",
    );
    // Tabs a, new, new; closing the middle one activates its right neighbour.
    assert_eq!(s[0]["tabs"].as_array().unwrap().len(), 2);
    assert_eq!(active(&s[0]), 1);
    assert_eq!(active(&s[1]), 0);
    assert_eq!(url(&s[1]), file_url("a.html"));
    assert_eq!(
        s[2]["quit"], true,
        "closing the last tab closes the window: {:?}",
        s
    );
}

#[test]
fn reopen_closed_tab_restores_its_history() {
    let s = run(
        &page("a.html"),
        "wait; click 50 100; wait; key cmd+t; key cmd+1; key cmd+w; state; key cmd+shift+t; wait; state; \
         key cmd+[; wait; state",
    );
    assert_eq!(s[0]["closed"], 1);
    assert_eq!(s[0]["tabs"].as_array().unwrap().len(), 1);
    assert!(url(&s[1]).ends_with("/b.html"), "{}", s[1]);
    assert_eq!(
        (s[1]["history"].as_u64(), s[1]["index"].as_u64()),
        (Some(2), Some(1))
    );
    assert_eq!(s[1]["closed"], 0);
    assert_eq!(tab_urls(&s[1]), ["newtab", "b.html"]);
    assert!(url(&s[2]).ends_with("/a.html"), "{}", s[2]);
}

#[test]
fn link_clicks_with_modifiers_open_tabs_and_windows() {
    // "Go to B" is at window (50, 100).
    let s = run(
        &page("a.html"),
        "wait; click 50 100 cmd; state; click 50 100 middle; state; key cmd+1; click 50 100 cmd shift; state; \
         key cmd+1; click 50 100 shift; wait; state",
    );
    // Cmd+click: background tab right after the opener.
    assert_eq!(tab_urls(&s[0]), ["a.html", "b.html"]);
    assert_eq!(active(&s[0]), 0);
    // Middle click: another background tab, after the first one.
    assert_eq!(tab_urls(&s[1]), ["a.html", "b.html", "b.html"]);
    assert_eq!(active(&s[1]), 0);
    // Cmd+Shift+click: foreground tab next to the opener.
    assert_eq!(active(&s[2]), 1);
    assert!(url(&s[2]).ends_with("/b.html"));
    // Shift+click: a new window.
    assert_eq!(s[3]["windows"], 2);
    assert_eq!(s[3]["window"], 1);
    assert!(url(&s[3]).ends_with("/b.html"));
}

#[test]
fn new_window_private_window_and_window_chords() {
    let s = run(
        &page("a.html"),
        "wait; key cmd+n; state; key cmd+shift+n; state; key cmd+l; type b.html.invalid; key enter; wait; state; \
         key cmd+m; key cmd+ctrl+f; state; key cmd+ctrl+f; key cmd+shift+w; state; key cmd+q; state",
    );
    assert_eq!(
        (s[0]["windows"].as_u64(), s[0]["window"].as_u64()),
        (Some(2), Some(1))
    );
    assert_eq!(url(&s[0]), "huginn://newtab/");
    assert_eq!(s[0]["private"], false);
    assert_eq!(s[1]["windows"], 3);
    assert_eq!(s[1]["private"], true);
    // A private window keeps no history.
    assert_eq!(s[2]["visits"], s[1]["visits"]);
    assert_eq!(
        (s[3]["minimized"].as_bool(), s[3]["fullscreen"].as_bool()),
        (Some(true), Some(true))
    );
    // Cmd+Shift+W closes the window; focus goes to another one.
    assert_eq!(s[4]["windows"], 2);
    assert_eq!(s[5]["quit"], true);
}

#[test]
fn private_new_tab_page_shows_no_visited_sites() {
    let s = run(
        &page("a.html"),
        "wait; key cmd+t; wait; click 400 300; key cmd+a; state; key cmd+shift+n; wait; click 400 300; key cmd+a; state",
    );
    let text = |v: &Value| v["selection"].as_str().unwrap_or("").to_owned();
    assert!(
        text(&s[0]).contains("Page A"),
        "normal new tab page lists the visit: {}",
        s[0]
    );
    assert!(text(&s[1]).contains("Huginn"), "{}", s[1]);
    assert!(
        !text(&s[1]).contains("Page A"),
        "private new tab page lists a visit: {}",
        s[1]
    );
}

#[test]
fn navigation_chords_and_toolbar_buttons() {
    let s = run(
        &page("a.html"),
        "wait; click 50 100; wait; key cmd+[; wait; state; key cmd+]; wait; state; key cmd+left; wait; state; \
         key cmd+right; wait; state; click 24 60; wait; state; click 58 60; wait; state; scroll 30; \
         key cmd+shift+r; wait; state; key cmd+shift+h; state",
    );
    let names: Vec<&str> = s[..6]
        .iter()
        .map(|v| url(v).rsplit('/').next().unwrap())
        .collect();
    assert_eq!(
        names,
        ["a.html", "b.html", "a.html", "b.html", "a.html", "b.html"]
    );
    assert_eq!(s[6]["history"], 2, "hard reload adds no entry");
    assert!(url(&s[6]).ends_with("/b.html"));
    assert_eq!(url(&s[7]), "huginn://newtab/");
    assert_eq!(s[7]["focused"], true);
}

#[test]
fn cmd_up_and_down_go_to_the_top_and_bottom() {
    let s = run(
        &page("a.html"),
        "wait; key cmd+down; state; key cmd+up; state",
    );
    assert_eq!(s[0]["scroll"], s[0]["max_scroll"]);
    assert_eq!(s[1]["scroll"], 0.0);
}

#[test]
fn two_finger_swipes_go_back_and_forward() {
    let s = run(
        &page("a.html"),
        "wait; click 50 100; wait; swipe 200 10; wait; state; swipe 100 0; swipe 200 120; state; swipe -200 0; wait; state",
    );
    assert!(url(&s[0]).ends_with("/a.html"), "{}", s[0]);
    // Too short, or too vertical: no navigation.
    assert!(url(&s[1]).ends_with("/a.html"));
    assert!(url(&s[2]).ends_with("/b.html"));
}

#[test]
fn escape_stops_a_load_and_keeps_the_page() {
    let s = run(
        &page("a.html"),
        "wait; click 50 100; key escape; state; wait; state; resize 700 500; wait; state",
    );
    assert_eq!(s[0]["loading"], false);
    assert!(url(&s[0]).ends_with("/a.html"), "{}", s[0]);
    assert_eq!(s[0]["history"], 1);
    // The stopped load's answer is dropped.
    assert!(url(&s[1]).ends_with("/a.html"));
    assert_eq!(s[1]["title"], "Page A");
    // The engine reloads the page on screen when it needs it again.
    assert_eq!(s[2]["title"], "Page A");
    assert!(s[2]["max_scroll"].as_f64().unwrap() > 2000.0);
}

#[test]
fn page_chords_work_from_the_address_bar() {
    // Cmd+F is not an address bar chord: it leaves the field and opens the
    // find bar, which then gets the typing.
    let s = run(
        &page("a.html"),
        "wait; key cmd+l; key cmd+f; type link; state",
    );
    assert_eq!(s[0]["focused"], false);
    assert_eq!(s[0]["find"]["open"], true);
    assert_eq!(s[0]["address"], file_url("a.html"));
}

#[test]
fn address_bar_searches_and_opens_tabs_and_windows() {
    let b = file_url("b.html");
    let s = run(
        &page("a.html"),
        &format!(
            "wait; key cmd+l; type hello world; key enter; state; key cmd+1; key cmd+l; type {b}; key alt+enter; wait; state; \
             key cmd+1; key cmd+l; type {b}; key cmd+enter; state; key cmd+l; type {b}; key shift+enter; state"
        ),
    );
    assert_eq!(url(&s[0]), "https://duckduckgo.com/html/?q=hello+world");
    assert_eq!(s[0]["focused"], false);
    // Option+Enter: new foreground tab.
    assert_eq!(s[1]["tabs"].as_array().unwrap().len(), 2);
    assert_eq!(active(&s[1]), 1);
    assert_eq!(url(&s[1]), b);
    // Cmd+Enter: new background tab.
    assert_eq!(s[2]["tabs"].as_array().unwrap().len(), 3);
    assert_eq!(active(&s[2]), 0);
    // Shift+Enter: new window.
    assert_eq!(s[3]["windows"], 2);
}

#[test]
fn history_page_and_clear_history() {
    let s = run(
        &page("a.html"),
        "wait; click 50 100; wait; key cmd+y; wait; state; key cmd+shift+backspace; wait; state",
    );
    assert_eq!(url(&s[0]), "huginn://history/");
    assert_eq!(s[0]["title"], "History");
    assert_eq!(s[0]["visits"], 2);
    assert_eq!(s[0]["tabs"].as_array().unwrap().len(), 2);
    assert_eq!(s[1]["visits"], 0);
    assert_eq!(url(&s[1]), "huginn://history/");
    assert_eq!(s[1]["tabs"].as_array().unwrap().len(), 3);
}

#[test]
fn bookmarks_are_saved_and_listed() {
    let dir = std::env::temp_dir().join(format!("huginn-tabs-test-{}", std::process::id()));
    let d = dir.to_str().unwrap();
    let s = run_env(
        &page("a.html"),
        "wait; key cmd+d; state; key cmd+alt+b; wait; state",
        &[("HUGINN_PROFILE_DIR", d)],
    );
    assert_eq!(
        (s[0]["bookmarked"].as_bool(), s[0]["bookmarks"].as_u64()),
        (Some(true), Some(1))
    );
    let file = std::fs::read_to_string(dir.join("bookmarks.tsv")).expect("bookmarks file");
    assert_eq!(file, format!("{}\tPage A\n", file_url("a.html")));
    assert_eq!(url(&s[1]), "huginn://bookmarks/");
    assert_eq!(s[1]["title"], "Bookmarks");
    // A second run reads the file; Cmd+D again removes the bookmark.
    let s = run_env(
        &page("a.html"),
        "wait; state; key cmd+d; state",
        &[("HUGINN_PROFILE_DIR", d)],
    );
    assert_eq!(s[0]["bookmarked"], true);
    assert_eq!(s[1]["bookmarked"], false);
    assert_eq!(
        std::fs::read_to_string(dir.join("bookmarks.tsv")).unwrap(),
        ""
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn zoom_is_per_site_and_lays_out_at_the_zoomed_width() {
    let s = run(
        &page("a.html"),
        "wait; state; key cmd+=; wait; state; key cmd+=; key cmd+-; key cmd+-; key cmd+-; wait; state; \
         click 50 100 cmd shift; wait; state; key cmd+1; key cmd+0; wait; state",
    );
    let zoom = |v: &Value| v["zoom"].as_f64().unwrap();
    let max = |v: &Value| v["max_scroll"].as_f64().unwrap();
    assert_eq!(zoom(&s[0]), 1.0);
    assert!((zoom(&s[1]) - 1.1).abs() < 1e-6);
    // The viewport is smaller in document px, so there is more to scroll.
    assert!(max(&s[1]) > max(&s[0]) + 30.0, "{} vs {}", s[1], s[0]);
    assert!((zoom(&s[2]) - 0.9).abs() < 1e-6);
    // Another site keeps its own zoom; at 90 % "Go to B" (0..200 x 0..40) is still at (50, 100).
    assert_eq!(zoom(&s[3]), 1.0);
    assert!(url(&s[3]).ends_with("/b.html"));
    assert_eq!(zoom(&s[4]), 1.0);
    assert_eq!(max(&s[4]), max(&s[0]));
}

#[test]
fn background_tabs_beyond_the_budget_are_discarded_and_reload_on_activation() {
    let s = run_env(
        &page("a.html"),
        "wait; click 50 100 cmd; click 50 100 cmd; click 50 100 cmd; wait; scroll 300; key cmd+2; wait; key cmd+1; \
         wait; state; key cmd+4; wait; state; key cmd+1; wait; state",
        &[("HUGINN_LIVE_TABS", "1")],
    );
    let discarded: Vec<bool> = s[0]["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["discarded"].as_bool().unwrap())
        .collect();
    // Tab 2 was active most recently, so it stays; tabs 3 and 4 are discarded.
    assert_eq!(discarded, [false, false, true, true]);
    assert_eq!(s[0]["engines"], 2);
    // A discarded tab loads again when activated.
    assert!(url(&s[1]).ends_with("/b.html"));
    assert_eq!(s[1]["discarded"], false);
    assert_eq!(s[1]["title"], "Page B");
    // Tab 1 was discarded meanwhile (the budget is one background tab) and keeps its scroll position.
    assert_eq!(s[2]["title"], "Page A");
    assert_eq!(s[2]["scroll"], 300.0);
}

#[test]
fn background_tabs_are_discarded_under_memory_pressure() {
    // A 1 MB budget is always exceeded: background tabs go one per event.
    let s = run_env(
        &page("a.html"),
        "wait; click 50 100 cmd; click 50 100 cmd; wait; state; state; state",
        &[("HUGINN_MEMORY_BUDGET_MB", "1")],
    );
    let discarded: Vec<bool> = s[2]["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["discarded"].as_bool().unwrap())
        .collect();
    assert_eq!(discarded, [false, true, true]);
    assert_eq!(s[2]["engines"], 1);
}

#[test]
fn tab_strip_mouse_new_switch_close_and_reorder() {
    // The strip starts after the traffic lights on macOS, after a small
    // margin elsewhere (`tabstrip::LEFT_INSET`).
    let inset: f32 = if cfg!(target_os = "macos") { 78.0 } else { 8.0 };
    // At 800 px on macOS, three tabs are (800 - 78 - 48) / 3 = 224.67 px wide
    // from x 78, elsewhere at their 240 px maximum; a close button's centre
    // is 19 px from its tab's right edge.
    let w = ((800.0 - inset - 48.0) / 3.0).min(240.0);
    let tab_x = |i: f32| inset + i * w + w / 2.0;
    // With two tabs left, they are 240 px wide again.
    let close_x = |i: f32| inset + (i + 1.0) * 240.0 - 19.0;
    // Two tabs are at their 240 px maximum; the + button follows them.
    let plus_x = inset + 2.0 * 240.0 + 20.0;
    let s = run(
        &page("a.html"),
        &format!(
            "wait; click 50 100 cmd; wait; click {plus_x} 23; state; click {} 23; state; drag {} 23 {} 23; state; \
             click {} 23 middle; state; click {} 23; state",
            tab_x(1.0),
            tab_x(0.0),
            tab_x(2.0) + 10.0,
            tab_x(0.0),
            close_x(1.0),
        ),
    );
    assert_eq!(tab_urls(&s[0]), ["a.html", "b.html", "newtab"]);
    assert_eq!(active(&s[0]), 2);
    assert_eq!(active(&s[1]), 1);
    // Dragging tab 0 to the right end moves it there and keeps it active.
    assert_eq!(tab_urls(&s[2]), ["b.html", "newtab", "a.html"]);
    assert_eq!(active(&s[2]), 2);
    // Middle click closes a tab.
    assert_eq!(tab_urls(&s[3]), ["newtab", "a.html"]);
    // The close button of the active tab closes it.
    assert_eq!(tab_urls(&s[4]), ["newtab"]);
}

#[test]
fn favicon_comes_from_link_rel_icon() {
    let s = run(&page("icon.html"), "wait; sleep 300; state");
    assert!(
        s[0]["icon"].as_str().unwrap().ends_with("/pages/icon.png"),
        "{}",
        s[0]
    );
    assert_eq!(s[0]["favicon"], true);
}
