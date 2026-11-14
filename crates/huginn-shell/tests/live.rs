//! Live JavaScript and form controls in the window, driven headless: click
//! handlers, preventDefault, :hover, checkboxes, radios, labels, the select
//! drop-down, typing, GET and POST form submission, timers.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

fn page(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/pages")
        .join(name);
    p.to_str().expect("UTF-8 path").to_owned()
}

/// Run `script` on `target` in an 800x600 window; return the `state` lines.
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

fn title(s: &Value) -> &str {
    s["title"].as_str().expect("title")
}

// The page area starts 80 px down (tab strip and toolbar); live.html and
// form.html place their controls absolutely (document px in the comments).

#[test]
fn click_handler_changes_the_dom_and_prevent_default_keeps_the_page() {
    // #btn at (10, 10), #pd link at (10, 50).
    let s = run(
        &page("live.html"),
        "wait; click 50 105; wait; state; click 50 135; wait; state",
    );
    assert_eq!(
        title(&s[0]),
        "clicked 1 true 50,25",
        "trusted click with client coordinates"
    );
    assert!(s[0]["isolate"].as_bool().unwrap());
    assert_eq!(title(&s[1]), "prevented");
    assert!(
        s[1]["url"].as_str().unwrap().ends_with("/live.html"),
        "the link was not followed: {}",
        s[1]
    );
}

#[test]
fn hover_fires_mouseenter() {
    // #hov at (10, 80): its :hover background is checked by the pixel test below.
    let s = run(&page("live.html"), "wait; move 40 175; wait; state");
    assert_eq!(title(&s[0]), "entered");
}

#[test]
fn hover_style_paints() {
    let dir = std::env::temp_dir().join(format!("huginn-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b) = (dir.join("a.png"), dir.join("b.png"));
    run(
        &page("live.html"),
        &format!(
            "wait; move 40 175; wait; shot {}; move 400 500; wait; shot {}",
            a.display(),
            b.display()
        ),
    );
    let pixel = |p: &PathBuf| {
        let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(p).unwrap()));
        let mut r = dec.read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        let info = r.next_frame(&mut buf).unwrap();
        let ch = info.color_type.samples();
        let i = (175 * info.width as usize + 40) * ch;
        (buf[i], buf[i + 1], buf[i + 2])
    };
    assert_eq!(pixel(&a), (0, 128, 0), ":hover background");
    assert_eq!(
        pixel(&b),
        (255, 0, 0),
        "plain background after the pointer left"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn checkbox_radio_label_select_typing_and_get_submit() {
    // #cb (10,10), #r2 (50,40), #sel (10,70) whose drop-down opens below it,
    // #q (10,110), #go (10,150), #lab (10,190) labels #cb.
    let s = run(
        &page("form.html"),
        "wait; click 20 100; wait; state; click 60 130; wait; state; click 40 280; wait; state; \
         click 50 200; type hello world; key backspace; wait; state; \
         click 50 160; wait; state; click 60 234; wait; state; click 50 245; wait; state",
    );
    assert_eq!(title(&s[0]), "change cb=true r=one s=a q=");
    assert_eq!(title(&s[1]), "change cb=true r=two s=a q=");
    assert_eq!(
        title(&s[2]),
        "change cb=false r=two s=a q=",
        "a label click toggles its checkbox"
    );
    assert_eq!(title(&s[3]), "input cb=false r=two s=a q=hello worl");
    assert_eq!(s[3]["field"]["caret"], 10);
    assert_eq!(s[4]["select_open"], true);
    assert_eq!(title(&s[5]), "change cb=false r=two s=c q=hello worl");
    assert!(
        s[6]["url"]
            .as_str()
            .unwrap()
            .ends_with("/b.html?r=two&s=c&q=hello+worl"),
        "{}",
        s[6]["url"]
    );
}

#[test]
fn editing_keys_and_clipboard_in_a_field() {
    let s = run(
        &page("form.html"),
        "wait; click 50 200; wait; type abc def; key alt+backspace; key backspace; key shift+left; key cmd+x; wait; state; \
         key cmd+v; key cmd+v; wait; state; key cmd+z; wait; state",
    );
    assert_eq!(
        title(&s[0]),
        "input cb=false r=one s=a q=ab",
        "word delete, then cut of 'c'"
    );
    assert_eq!(s[0]["clipboard"], "c");
    assert_eq!(title(&s[1]), "input cb=false r=one s=a q=abcc");
    assert_eq!(title(&s[2]), "input cb=false r=one s=a q=abc");
}

#[test]
fn set_interval_runs_in_real_time() {
    // A 100 ms interval should tick about 15 times in 1.5 s. Ask for 4 so a slow,
    // shared CI runner still passes; the point is that timers keep running in real time.
    let s = run(&page("timer.html"), "wait; state; sleep 1500; state");
    let tick = |v: &Value| title(v).trim_start_matches("tick ").parse::<u32>().unwrap();
    let (a, b) = (tick(&s[0]), tick(&s[1]));
    assert!(b >= a + 4, "at least 4 ticks in 1500 ms: {a} -> {b}");
}

#[test]
fn wheel_scrolls_the_container_under_the_pointer_then_the_page() {
    // #box: a 300x200 `overflow: auto` box at (0, 0) with 1000 px of content,
    // under a full-window `pointer-events: none` layer. The page is 2000 px.
    let s = run(
        &page("scroll.html"),
        "wait; move 100 180; scroll 300; wait; state; scroll 5000; wait; state; \
         scroll 100; wait; state",
    );
    assert_eq!(
        title(&s[0]),
        "scrolled 300",
        "the box scrolls, not the page"
    );
    assert_eq!(s[0]["scroll"].as_f64(), Some(0.0));
    assert_eq!(
        title(&s[1]),
        "scrolled 800",
        "clamped to scrollHeight - clientHeight"
    );
    // At its end the box cannot move: the page scrolls (scroll chaining).
    assert_eq!(title(&s[2]), "scrolled 800");
    assert_eq!(s[2]["scroll"].as_f64(), Some(100.0));
}

#[test]
fn post_form_sends_its_body() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut r = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            r.read_line(&mut first).unwrap();
            let mut len = 0;
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let html = if first.starts_with("POST") {
                format!("<title>posted {}</title>", String::from_utf8_lossy(&body))
            } else {
                "<form method=post action=/save><input name=a value=1 style='position:absolute;top:0;left:0;width:100px;height:20px'>\
                 <button style='position:absolute;top:40px;left:0;width:100px;height:30px'>Save</button></form>"
                    .to_owned()
            };
            let mut s = stream;
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                html.len()
            );
        }
    });
    let s = run(
        &format!("http://127.0.0.1:{port}/"),
        "wait; click 50 135; wait; state",
    );
    assert_eq!(title(&s[0]), "posted a=1");
    assert!(s[0]["url"].as_str().unwrap().ends_with("/save"));
}

#[test]
fn positioned_control_before_its_label_gets_the_click() {
    let s = run(&page("hit.html"), "wait; click 20 100; wait; state");
    assert_eq!(title(&s[0]), "toggled true");
}
