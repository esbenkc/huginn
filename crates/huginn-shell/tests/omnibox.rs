//! The address bar's history, inline autofill and drop-down through the real
//! binary (`huginn open --commands`) against a local test server. Each test
//! has its own profile directory, so history is saved like in a window.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

/// Serve two pages on 127.0.0.1: `/` ("Local home") and `/docs/guide`
/// ("Guide"); the rest is 404. Returns the port.
fn serve() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut r = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if r.read_line(&mut first).is_err() {
                continue;
            }
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
            }
            let path = first.split_whitespace().nth(1).unwrap_or("/");
            let (status, body) = match path {
                "/" => ("200 OK", "<title>Local home</title><p>home"),
                "/docs/guide" => ("200 OK", "<title>Guide</title><p>guide"),
                _ => ("404 Not Found", "<title>404</title>"),
            };
            let mut s = stream;
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    port
}

/// A fresh profile directory for one test.
fn profile(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("huginn-omnibox-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Run `script` in an 800x600 window (on the new tab page without
/// `target`) with history in `profile`; return the `state` lines.
fn run(target: Option<&str>, script: &str, profile: &PathBuf) -> Vec<Value> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_huginn"));
    cmd.arg("open");
    if let Some(t) = target {
        cmd.arg(t);
    }
    cmd.args([
        "--width",
        "800",
        "--height",
        "600",
        "--no-js",
        "--commands",
        script,
    ])
    .env("HUGINN_NET_LOOPBACK_ONLY", "1")
    .env("HUGINN_PROFILE_DIR", profile);
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

fn address(s: &Value) -> &str {
    s["address"].as_str().expect("address")
}

fn selection(s: &Value) -> Option<(u64, u64)> {
    let a = s["address_selection"].as_array()?;
    Some((a[0].as_u64()?, a[1].as_u64()?))
}

fn first_suggestion(s: &Value) -> (&str, &str) {
    let m = &s["suggestions"][0];
    (
        m["kind"].as_str().expect("kind"),
        m["url"].as_str().expect("url"),
    )
}

#[test]
fn typing_autofills_a_typed_host_backspace_removes_it_and_enter_goes_there() {
    let port = serve();
    let dir = profile("autofill");
    let host = format!("127.0.0.1:{port}");
    let root = format!("http://{host}/");
    let s = run(
        None,
        &format!(
            "type {host}; key enter; wait; key cmd+l; type 12; state; type 7; state; \
             key backspace; state; key backspace; state; type 7; state; key tab; state; \
             key enter; wait; state"
        ),
        &dir,
    );
    let n = host.chars().count() as u64;
    // `12` completes to the host, the completed part selected; Enter would go to its root.
    assert_eq!(address(&s[0]), host);
    assert_eq!(selection(&s[0]), Some((2, n)));
    assert_eq!(first_suggestion(&s[0]), ("history", root.as_str()));
    // Typing on keeps the completion.
    assert_eq!(
        (address(&s[1]), selection(&s[1])),
        (host.as_str(), Some((3, n)))
    );
    // Backspace removes the completion only, and a second one does not bring it back.
    assert_eq!((address(&s[2]), selection(&s[2])), ("127", None));
    assert_eq!(first_suggestion(&s[2]).0, "search");
    assert_eq!((address(&s[3]), selection(&s[3])), ("12", None));
    // The next character completes again.
    assert_eq!(
        (address(&s[4]), selection(&s[4])),
        (host.as_str(), Some((3, n)))
    );
    // Tab takes the completion.
    assert_eq!((address(&s[5]), selection(&s[5])), (host.as_str(), None));
    // Enter goes to the completed URL.
    assert_eq!(s[6]["url"], root.as_str());
    assert_eq!(s[6]["focused"], false);
    assert_eq!(s[6]["title"], "Local home");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn history_survives_a_restart_and_the_drop_down_is_keyboard_and_mouse_driven() {
    let port = serve();
    let dir = profile("dropdown");
    let host = format!("127.0.0.1:{port}");
    let root = format!("http://{host}/");
    let guide = format!("http://{host}/docs/guide");
    run(
        None,
        &format!(
            "type {host}; key enter; wait; key cmd+l; type {host}/docs/guide; key enter; wait"
        ),
        &dir,
    );
    let s = run(
        None,
        "type 12; state; key down; state; key escape; state; key escape; state; \
         key cmd+l; type guide; state",
        &dir,
    );
    // The first run's history is back: `12` autofills the host, not the deeper page.
    assert_eq!(s[0]["history_rows"], 2);
    assert_eq!(address(&s[0]), host);
    let urls: Vec<&str> = s[0]["suggestions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["url"].as_str().unwrap())
        .collect();
    assert_eq!(urls[0], root);
    assert!(urls.contains(&guide.as_str()), "{urls:?}");
    // Down picks the next row and shows it in the field; Esc goes back to
    // what was typed with its completion, a second Esc leaves the field.
    assert_eq!(s[1]["suggestion"], 1);
    assert_ne!(address(&s[1]), host);
    assert_eq!(selection(&s[1]), None);
    assert_eq!(
        (s[2]["suggestion"].as_u64(), address(&s[2])),
        (Some(0), host.as_str())
    );
    assert_eq!(selection(&s[2]), Some((2, host.chars().count() as u64)));
    assert_eq!(
        (
            s[3]["focused"].as_bool(),
            s[3]["suggestions"].as_array().map(Vec::len)
        ),
        (Some(false), Some(0))
    );
    // Titles match too; a click on a row opens it.
    let rows = s[4]["suggestions"].as_array().unwrap();
    let i = rows
        .iter()
        .position(|m| m["url"] == guide.as_str())
        .expect("the guide by its title");
    assert_eq!(rows[i]["title"], "Guide");
    let r = s[4]["suggestions_rect"].as_array().unwrap();
    let (x, y) = (
        r[0].as_f64().unwrap() + 40.0,
        r[1].as_f64().unwrap() + 4.0 + 28.0 * i as f64 + 14.0,
    );
    let s = run(
        None,
        &format!("type guide; click {x} {y}; wait; state"),
        &dir,
    );
    assert_eq!(s[0]["url"], guide.as_str());
    assert_eq!(s[0]["title"], "Guide");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_page_reached_by_a_link_is_suggested_but_not_autofilled() {
    let port = serve();
    let dir = profile("link");
    let s = run(
        Some(&format!("http://localhost:{port}/")),
        "wait; key cmd+l; type loc; state",
        &dir,
    );
    assert_eq!((address(&s[0]), selection(&s[0])), ("loc", None));
    assert_eq!(first_suggestion(&s[0]).0, "search");
    let urls: Vec<&str> = s[0]["suggestions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["url"].as_str().unwrap())
        .collect();
    assert!(
        urls.contains(&format!("http://localhost:{port}/").as_str()),
        "{urls:?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn private_windows_record_no_history_and_clearing_browsing_data_empties_it() {
    let port = serve();
    let dir = profile("private");
    let host = format!("127.0.0.1:{port}");
    let s = run(
        None,
        &format!("key cmd+shift+n; type {host}; key enter; wait; state; key cmd+l; type 12; state"),
        &dir,
    );
    assert_eq!(s[0]["private"], true);
    assert_eq!(s[0]["url"], format!("http://{host}/").as_str());
    assert_eq!(s[0]["history_rows"], 0);
    assert_eq!((address(&s[1]), selection(&s[1])), ("12", None));
    assert!(!dir.join("history.tsv").exists());

    let s = run(
        None,
        &format!("type {host}; key enter; wait; state; key cmd+shift+backspace; wait; state"),
        &dir,
    );
    assert_eq!(s[0]["history_rows"], 1);
    assert_eq!(s[1]["history_rows"], 0);
    assert!(!dir.join("history.tsv").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}
