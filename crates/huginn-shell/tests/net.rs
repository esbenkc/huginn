//! Cookies, CORS, referrers and scripts' requests through the real binary
//! (`huginn open --commands`, `huginn render`) against local test servers.
//! Two servers on 127.0.0.1 and a `localhost` URL give same-site,
//! cross-origin and cross-site requests. Window layout at 800x600: the page
//! starts 80 px down.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

/// A request a test server received.
#[derive(Clone, Debug, Default)]
struct Req {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Req {
    fn header(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map_or("", |(_, v)| v.as_str())
    }
}

/// Serve HTTP on 127.0.0.1: `handler` returns the status line and extra
/// header lines (joined by `\n`) and the body. Returns the port and a log.
fn serve(
    handler: impl Fn(&Req) -> (String, String) + Send + 'static,
) -> (u16, Arc<Mutex<Vec<Req>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut r = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if r.read_line(&mut first).is_err() {
                continue;
            }
            let mut parts = first.split_whitespace();
            let mut req = Req {
                method: parts.next().unwrap_or("").to_owned(),
                path: parts.next().unwrap_or("").to_owned(),
                headers: Vec::new(),
                body: String::new(),
            };
            let mut len = 0;
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
                let (n, v) = h.split_once(':').unwrap();
                let (n, v) = (n.trim().to_ascii_lowercase(), v.trim().to_owned());
                if n == "content-length" {
                    len = v.parse().unwrap();
                }
                req.headers.push((n, v));
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            req.body = String::from_utf8_lossy(&body).into_owned();
            let (head, body) = handler(&req);
            seen.lock().unwrap().push(req);
            let head = head.replace('\n', "\r\n");
            let mut s = stream;
            let _ = write!(
                s,
                "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (port, log)
}

fn html(head: &str, body: &str) -> (String, String) {
    (
        format!(
            "200 OK\nContent-Type: text/html{}{head}",
            if head.is_empty() { "" } else { "\n" }
        ),
        body.to_owned(),
    )
}

/// Run `script` on `target` in an 800x600 window; return the `state` lines.
fn run(target: &str, script: &str, env: &[(&str, &str)]) -> Vec<Value> {
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
    ])
    .env("HUGINN_NET_LOOPBACK_ONLY", "1")
    .env_remove("HUGINN_PROFILE_DIR");
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

fn title(s: &Value) -> &str {
    s["title"].as_str().expect("title")
}

/// A full-page link: a click anywhere below the toolbar follows it.
fn link(href: &str) -> String {
    format!(
        "<a href='{href}' style='position:absolute;top:0;left:0;width:800px;height:500px'>go</a>"
    )
}

#[test]
fn cookies_reach_the_server_and_http_only_ones_stay_hidden_from_scripts() {
    let (port, log) = serve(|r| match r.path.as_str() {
        "/" => html(
            "Set-Cookie: sid=s1; HttpOnly; Path=/\nSet-Cookie: theme=dark; Path=/",
            &format!(
                "<script>document.cookie = 'js=1; path=/'; document.title = document.cookie;</script>{}",
                link("/echo")
            ),
        ),
        _ => html("", &format!("<title>{}</title>", r.header("cookie"))),
    });
    let s = run(
        &format!("http://127.0.0.1:{port}/"),
        "wait; state; click 400 300; wait; state",
        &[],
    );
    assert_eq!(title(&s[0]), "theme=dark; js=1");
    assert_eq!(title(&s[1]), "sid=s1; theme=dark; js=1");
    assert_eq!(s[1]["cookies"], 3);
    // The link carried the full referrer (same origin).
    let log = log.lock().unwrap();
    let echo = log.iter().find(|r| r.path == "/echo").unwrap();
    assert_eq!(echo.header("referer"), format!("http://127.0.0.1:{port}/"));
}

#[test]
fn cross_origin_requests_follow_cors() {
    // The API on `localhost`: another origin and another site.
    let (api, api_log) = serve(|r| {
        let origin = r.header("origin").to_owned();
        let allow = format!(
            "Access-Control-Allow-Origin: {origin}\nAccess-Control-Allow-Credentials: true"
        );
        match (r.method.as_str(), r.path.as_str()) {
            ("OPTIONS", "/put") => (
                format!(
                    "204 No Content\n{allow}\nAccess-Control-Allow-Methods: PUT\nAccess-Control-Allow-Headers: x-token"
                ),
                String::new(),
            ),
            ("OPTIONS", _) => ("204 No Content".into(), String::new()),
            (_, "/allowed" | "/put") => (
                format!("200 OK\nContent-Type: text/plain\n{allow}"),
                format!("{} ok", r.method),
            ),
            (_, "/lib.js") => (
                "200 OK\nContent-Type: text/javascript".into(),
                "window.libRan = 'lib';".into(),
            ),
            (_, "/mod.js") => (
                "200 OK\nContent-Type: text/javascript".into(),
                "export const m = 'mod';".into(),
            ),
            _ => ("200 OK\nContent-Type: text/plain".into(), "secret".into()),
        }
    });
    let script = format!(
        r#"<script>
        const api = 'http://localhost:{api}';
        const out = [];
        const step = (name, p) => p.then(v => out.push(name + '=' + v), e => out.push(name + '=' + e.name));
        const s = document.createElement('script');
        s.src = api + '/lib.js';
        document.head.appendChild(s);
        Promise.all([
          step('allowed', fetch(api + '/allowed').then(r => r.text())),
          step('denied', fetch(api + '/private').then(r => r.text())),
          step('opaque', fetch(api + '/private', {{ mode: 'no-cors' }}).then(r => r.type + r.status)),
          step('put', fetch(api + '/put', {{ method: 'PUT', headers: {{ 'X-Token': 't' }}, body: 'b' }}).then(r => r.text())),
          step('del', fetch(api + '/put', {{ method: 'DELETE' }}).then(r => r.text())),
          step('mod', import(api + '/mod.js').then(m => m.m)),
          step('xhr', new Promise((ok, no) => {{
            const x = new XMLHttpRequest();
            x.open('GET', api + '/private');
            x.onload = () => ok(x.responseText);
            x.onerror = () => no(new Error('x'));
            x.send();
          }})),
        ]).then(() => {{ document.title = out.sort().join(' ') + ' lib=' + window.libRan; }});
        </script>"#
    );
    let (port, _) = serve(move |_| html("", &script));
    let s = run(
        &format!("http://127.0.0.1:{port}/"),
        "wait; sleep 500; state",
        &[],
    );
    assert_eq!(
        title(&s[0]),
        "allowed=GET ok del=TypeError denied=TypeError mod=TypeError opaque=opaque0 put=PUT ok xhr=Error lib=lib"
    );
    let log = api_log.lock().unwrap();
    // The requests run in parallel: the PUT and DELETE preflights come in
    // either order.
    let preflight = |method: &str| {
        log.iter()
            .find(|r| {
                r.method == "OPTIONS"
                    && r.path == "/put"
                    && r.header("access-control-request-method") == method
            })
            .unwrap_or_else(|| panic!("no {method} preflight in {log:?}"))
    };
    assert_eq!(
        preflight("PUT").header("access-control-request-headers"),
        "x-token"
    );
    assert_eq!(
        preflight("DELETE").header("access-control-request-headers"),
        ""
    );
    assert!(
        !log.iter().any(|r| r.method == "DELETE"),
        "a refused preflight stops the request"
    );
    let put = log.iter().find(|r| r.method == "PUT").unwrap();
    assert_eq!(put.header("origin"), format!("http://127.0.0.1:{port}"));
}

#[test]
fn third_party_cookies_are_partitioned_and_referrer_crosses_sites() {
    // Site B (`localhost`) sets its cookies on a visit, then links to site A
    // (127.0.0.1), whose page calls B with credentials. B's first-party
    // cookies, `SameSite=None` included, stay in B's own partition.
    let (a, _) = serve(|_| {
        html(
            "",
            r#"<script>
            fetch(location.search.slice(1) + '/echo', { credentials: 'include' })
              .then(r => r.text())
              .then(t => { document.title = 'B saw [' + t + '] referrer ' + document.referrer; });
            </script>"#,
        )
    });
    let (b, _) = serve(move |r| match r.path.as_str() {
        "/" => html(
            "Set-Cookie: lax=1; Path=/\nSet-Cookie: strict=1; SameSite=Strict; Path=/\nSet-Cookie: none=1; SameSite=None; Secure; Path=/",
            &link(&format!(
                "http://127.0.0.1:{a}/?http://localhost:{}",
                r.header("host").rsplit(':').next().unwrap()
            )),
        ),
        _ => (
            format!(
                "200 OK\nContent-Type: text/plain\nAccess-Control-Allow-Origin: {}\nAccess-Control-Allow-Credentials: true",
                r.header("origin")
            ),
            r.header("cookie").to_owned(),
        ),
    });
    let s = run(
        &format!("http://localhost:{b}/"),
        "wait; click 400 300; wait; sleep 300; state",
        &[],
    );
    assert_eq!(
        title(&s[0]),
        format!("B saw [] referrer http://localhost:{b}/")
    );
}

#[test]
fn pages_opening_new_tabs_get_no_strict_cookies_and_keep_form_bodies() {
    // The victim (127.0.0.1) sets a Strict and a Lax cookie on a visit; its
    // /echo shows what arrived. The attacker page (localhost) opens it in a
    // new tab by script: a `target=_blank` link click, or a form submit.
    let (victim, log) = serve(|r| match r.path.as_str() {
        "/" => html(
            "Set-Cookie: s=1; SameSite=Strict; Path=/\nSet-Cookie: l=1; Path=/",
            "<title>victim</title>",
        ),
        _ => html(
            "",
            &format!(
                "<title>[{}] {} {}</title>",
                r.header("cookie"),
                r.method,
                r.body
            ),
        ),
    });
    let (attacker, _) = serve(move |r| {
        let echo = format!("http://127.0.0.1:{victim}/echo");
        let body = if r.path.contains("form") {
            format!(
                "<form id=f method=post action='{echo}' target=_blank><input name=a value=1></form>\
                 <script>document.getElementById('f').submit()</script>"
            )
        } else {
            format!(
                "<a id=x target=_blank href='{echo}'>x</a><script>document.getElementById('x').click()</script>"
            )
        };
        html("", &body)
    });
    for (path, want) in [("/link", "[l=1] GET"), ("/form", "[] POST a=1")] {
        let s = run(
            &format!("http://127.0.0.1:{victim}/"),
            &format!(
                "wait; key cmd+l; type http://localhost:{attacker}{path}; key enter; wait; sleep 300; state"
            ),
            &[],
        );
        assert_eq!(s[0]["tabs"].as_array().map(Vec::len), Some(2), "{path}");
        assert_eq!(title(&s[0]), want, "{path}");
    }
    let log = log.lock().unwrap();
    let echoes: Vec<&Req> = log.iter().filter(|r| r.path == "/echo").collect();
    assert_eq!(echoes.len(), 2);
    for r in echoes {
        assert_eq!(r.header("sec-fetch-site"), "cross-site");
        assert_eq!(r.header("sec-fetch-user"), "");
        assert!(r.header("referer").starts_with("http://localhost:"));
    }
}

#[test]
fn persistent_cookies_survive_a_restart_but_private_and_session_ones_do_not() {
    let (port, _) = serve(|r| match r.path.as_str() {
        "/set" => html(
            "Set-Cookie: keep=1; Max-Age=3600; Path=/\nSet-Cookie: session=1; Path=/",
            "<title>set</title>",
        ),
        "/private" => html(
            "Set-Cookie: incognito=1; Max-Age=3600; Path=/",
            "<title>private</title>",
        ),
        _ => html("", &format!("<title>[{}]</title>", r.header("cookie"))),
    });
    let profile = std::env::temp_dir().join(format!("huginn-net-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&profile);
    let env = [("HUGINN_PROFILE_DIR", profile.to_str().unwrap())];
    let base = format!("http://127.0.0.1:{port}");
    let s = run(
        &format!("{base}/set"),
        &format!(
            "wait; state; key cmd+shift+n; wait; key cmd+l; type {base}/private; key enter; wait; state; \
             key cmd+shift+w; wait; state"
        ),
        &env,
    );
    assert_eq!((title(&s[0]), &s[0]["cookies"]), ("set", &Value::from(2)));
    // The private window has its own jar.
    assert_eq!(title(&s[1]), "private");
    assert_eq!(
        (&s[1]["cookies"], &s[1]["private_cookies"]),
        (&Value::from(2), &Value::from(1))
    );
    // Closing the last private window drops its jar.
    assert!(s[2]["private_cookies"].is_null());

    let s = run(&format!("{base}/echo"), "wait; state", &env);
    assert_eq!(title(&s[0]), "[keep=1]");
    let _ = std::fs::remove_dir_all(&profile);
}

#[test]
fn a_web_page_cannot_load_local_files_but_a_local_page_can() {
    let dir = std::env::temp_dir().join(format!("huginn-net-local-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let js = dir.join("local.js");
    std::fs::write(&js, "document.title += ' ran';").unwrap();
    let js_url = huginn_net_file_url(&js);
    // A script the parser finds and one a script inserts. The local script
    // adds " ran" to the title each time it runs.
    let page = format!(
        "<title>t</title><script src='{js_url}'></script>
        <script>const s = document.createElement('script'); s.src = '{js_url}';
        s.onerror = () => document.title += ' blocked'; document.head.appendChild(s);</script>"
    );
    let body = page.clone();
    let (port, _) = serve(move |_| html("", &body));
    let s = run(
        &format!("http://127.0.0.1:{port}/"),
        "wait; sleep 200; state",
        &[],
    );
    assert_eq!(title(&s[0]), "t blocked");

    let local = dir.join("page.html");
    std::fs::write(&local, &page).unwrap();
    let s = run(local.to_str().unwrap(), "wait; sleep 200; state", &[]);
    assert_eq!(title(&s[0]), "t ran ran");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn revisits_use_the_http_cache_and_a_hard_reload_skips_it() {
    let (port, log) = serve(|r| match r.path.as_str() {
        "/" if r.header("if-none-match") == "\"p1\"" => ("304 Not Modified".into(), String::new()),
        "/" => html(
            "Cache-Control: no-cache\nETag: \"p1\"",
            "<link rel=stylesheet href=/s.css><title>Cached</title><p>hi",
        ),
        "/s.css" => (
            "200 OK\nContent-Type: text/css\nCache-Control: max-age=600".into(),
            "p { color: red }".into(),
        ),
        _ => ("404 Not Found".into(), String::new()),
    });
    let url = format!("http://127.0.0.1:{port}/");
    let blank = "key cmd+l; type about:blank; key enter; wait";
    let s = run(
        &url,
        &format!(
            "wait; {blank}; key cmd+l; type {url}; key enter; wait; state; \
             key cmd+r; wait; state; key cmd+shift+r; wait; state"
        ),
        &[],
    );
    assert!(s.iter().all(|s| title(s) == "Cached"), "{s:?}");
    let log = log.lock().unwrap();
    let get = |path: &str| log.iter().filter(|r| r.path == path).collect::<Vec<_>>();
    // The stylesheet: once, then from the cache on the revisit and the
    // reload; the hard reload fetches it again.
    let css = get("/s.css");
    assert_eq!(css.len(), 2);
    assert_eq!(css[1].header("cache-control"), "no-cache");
    assert_eq!(css[1].header("pragma"), "no-cache");
    // The document (`no-cache`): revalidated on the revisit and the reload,
    // fetched whole on the hard reload.
    let doc = get("/");
    assert_eq!(doc.len(), 4);
    assert_eq!(doc[0].header("if-none-match"), "");
    assert_eq!(doc[1].header("if-none-match"), "\"p1\"");
    assert_eq!(doc[2].header("if-none-match"), "\"p1\"");
    assert_eq!(doc[2].header("cache-control"), "max-age=0");
    assert_eq!(doc[3].header("if-none-match"), "");
}

#[test]
fn lazy_images_load_after_the_first_paint_and_keep_the_restored_scroll() {
    let (port, log) = serve(|r| match r.path.as_str() {
        "/" => html(
            "",
            "<title>Lazy</title><p>top<p><img loading=lazy src=/tall.svg>",
        ),
        "/tall.svg" => (
            "200 OK\nContent-Type: image/svg+xml\nCache-Control: max-age=600".into(),
            "<svg xmlns='http://www.w3.org/2000/svg' width='10' height='3000'/>".into(),
        ),
        _ => ("404 Not Found".into(), String::new()),
    });
    let s = run(
        &format!("http://127.0.0.1:{port}/"),
        "wait; state; scroll 2500; state; key cmd+r; wait; state",
        &[],
    );
    // The window still loads every lazy image, so the page is tall.
    assert!(s[0]["max_scroll"].as_f64().unwrap() > 2500.0, "{}", s[0]);
    assert_eq!(s[1]["scroll"], 2500.0);
    // The reload shows the page before the image makes it tall; the
    // scroll position comes back once it is.
    assert_eq!(s[2]["scroll"], 2500.0, "{}", s[2]);
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == "/tall.svg")
            .count(),
        1,
        "the reload takes the image from the cache"
    );
}

#[test]
fn a_fetch_that_answers_after_load_updates_the_live_page() {
    // No timers: only the response can wake the tab's engine thread.
    let (port, _) = serve(|r| match r.path.as_str() {
        "/slow" => {
            std::thread::sleep(Duration::from_millis(600));
            ("200 OK\nContent-Type: text/plain".into(), "late".into())
        }
        "/" => html(
            "",
            "<title>start</title><p id=out>waiting</p><script>
            fetch('/slow').then(r => r.text()).then(t => {
              document.getElementById('out').textContent = t;
              document.title = 'got ' + t;
            });
            </script>",
        ),
        _ => ("404 Not Found".into(), String::new()),
    });
    let s = run(
        &format!("http://127.0.0.1:{port}/"),
        "wait; state; sleep 1500; state",
        &[],
    );
    // The load does not wait for the request.
    assert_eq!(title(&s[0]), "start");
    assert_eq!(title(&s[1]), "got late");
}

/// Serve HTTP on 127.0.0.1, one thread per connection: `handler` maps a path
/// to a delay in ms and an HTML body.
fn serve_parallel(handler: fn(&str) -> (u64, String)) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                let _ = r.read_line(&mut first);
                let mut line = String::new();
                while r.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let path = first.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let (ms, body) = handler(&path);
                std::thread::sleep(Duration::from_millis(ms));
                let ty = if path.ends_with(".css") {
                    "text/css"
                } else {
                    "text/html"
                };
                let mut s = stream;
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: {ty}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    port
}

/// A page with an inline script that logs DOMContentLoaded (`D`) and load
/// (`L`) in the title, and `script`, which loads `/slow.js` (`S`). The
/// server answers `/slow.js` after `SLOW_MS`.
fn slow_script_page(script: &str) -> String {
    format!(
        "<title></title><script>document.addEventListener('DOMContentLoaded', () => \
         document.title += 'D'); addEventListener('load', () => document.title += 'L')\
         </script><p>Before</p>{script}<p>After</p>"
    )
}

const SLOW_MS: u64 = 2000;

fn serve_slow_script_pages() -> u16 {
    let (port, _) = serve(|r| match r.path.as_str() {
        "/async" => html(
            "",
            &slow_script_page("<script async src=/slow.js></script>"),
        ),
        "/defer" => html(
            "",
            &slow_script_page("<script defer src=/slow.js></script>"),
        ),
        "/module" => html(
            "",
            &slow_script_page("<script type=module src=/slow.js></script>"),
        ),
        "/blocking" => html("", &slow_script_page("<script src=/slow.js></script>")),
        "/slow.js" => {
            std::thread::sleep(std::time::Duration::from_millis(SLOW_MS));
            (
                "200 OK\nContent-Type: text/javascript".into(),
                "document.title += 'S'".into(),
            )
        }
        _ => ("404 Not Found".into(), String::new()),
    });
    port
}

/// `huginn render <url>`: the JSON line.
fn render(url: &str) -> Value {
    let out_png =
        std::env::temp_dir().join(format!("huginn-net-render-{}.png", std::process::id()));
    let out = Command::new(env!("CARGO_BIN_EXE_huginn"))
        .args(["render", url, "--out", out_png.to_str().unwrap()])
        .env("HUGINN_NET_LOOPBACK_ONLY", "1")
        .output()
        .expect("run huginn");
    let _ = std::fs::remove_file(&out_png);
    assert!(
        out.status.success(),
        "huginn render failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("one JSON line")
}

#[test]
fn render_waits_for_script_requests_within_the_settle_budget() {
    // `/` adds 40 paragraphs in two chained requests and 1000 more after a
    // request that takes 10 s; `/sync` adds the 40 at once.
    let port = serve_parallel(|path| {
        let script = match path {
            "/data" => return (300, "x".into()),
            "/hang" => return (10_000, "y".into()),
            "/sync" => "add(40);",
            _ => {
                "fetch('/data').then(r => r.text()).then(() => { add(20); return fetch('/data'); })
                   .then(r => r.text()).then(() => add(20));
                 fetch('/hang').then(() => add(1000));"
            }
        };
        (
            0,
            format!(
                "<body><script>
                 const add = n => {{ for (let i = 0; i < n; i++) document.body.appendChild(document.createElement('p')); }};
                 {script}
                 </script></body>"
            ),
        )
    });
    let sync = render(&format!("http://127.0.0.1:{port}/sync"));
    let fetched = render(&format!("http://127.0.0.1:{port}/"));
    assert_eq!(fetched["nodes"], sync["nodes"], "{fetched}");
    // The hanging request is given up after the 1.5 s settle budget.
    assert!(fetched["total_ms"].as_f64().unwrap() < 5000.0, "{fetched}");
}

const FETCH_MS: u64 = 1500;

#[test]
fn requests_that_scripts_start_before_the_first_paint_wait_for_it() {
    // The inline script starts 6 slow requests, as many as there are
    // HTTP/1.1 connections to a host, then adds a stylesheet. A sync XHR
    // before them is not held.
    let port = serve_parallel(|path| match path {
        p if p.starts_with("/slow") => (FETCH_MS, "x".into()),
        "/late.css" => (300, "p { color: red }".into()),
        "/sync" => (0, "sync".into()),
        _ => (
            0,
            "<title>start</title><p>text</p><script>
             const x = new XMLHttpRequest();
             x.open('GET', '/sync', false);
             x.send();
             document.title = x.responseText;
             let done = 0;
             for (let i = 0; i < 6; i++) {
               fetch('/slow?' + i).then(r => r.text()).then(() => {
                 if (++done === 6) document.title = 'fetched ' + Math.round(performance.now());
               });
             }
             const l = document.createElement('link');
             l.rel = 'stylesheet';
             l.href = '/late.css';
             document.head.appendChild(l);
             </script>"
                .into(),
        ),
    });
    let s = run(
        &format!("http://127.0.0.1:{port}/"),
        "wait; state; sleep 4000; state",
        &[],
    );
    // The first paint waited for the stylesheet, not for the requests.
    let first = s[0]["load_ms"].as_f64().unwrap();
    assert!((300.0..FETCH_MS as f64).contains(&first), "{}", s[0]);
    assert_eq!(title(&s[0]), "sync", "{}", s[0]);
    // Then the requests went out, all at once: one after another, two of
    // them would already take twice as long.
    let done: f64 = title(&s[1])
        .strip_prefix("fetched ")
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("{}", s[1]));
    assert!(done < first + 1.8 * FETCH_MS as f64, "{}", s[1]);
}

#[test]
fn async_defer_and_module_scripts_do_not_hold_up_the_first_paint() {
    let port = serve_slow_script_pages();
    let base = format!("http://127.0.0.1:{port}");
    let s = run(
        &format!("{base}/async"),
        &format!(
            "wait; state; key cmd+l; type {base}/defer; key enter; wait; state; \
             key cmd+l; type {base}/module; key enter; wait; state"
        ),
        &[],
    );
    for (s, events) in s.iter().zip(["DSL", "SDL", "SDL"]) {
        // The page was on screen long before the script came; the load was
        // complete after it ran.
        let (first, complete) = (s["load_ms"].as_f64().unwrap(), s["complete_ms"].as_f64());
        assert!(first < SLOW_MS as f64 * 0.75, "{s}");
        assert!(complete.is_some_and(|c| c >= SLOW_MS as f64), "{s}");
        // DOMContentLoaded waits for defer and module scripts, load for
        // all of them.
        assert_eq!(title(s), events, "{s}");
    }
}

#[test]
fn a_parser_blocking_script_holds_up_the_content_after_it() {
    let port = serve_slow_script_pages();
    let s = run(
        &format!("http://127.0.0.1:{port}/blocking"),
        "wait; state",
        &[],
    );
    assert!(
        s[0]["load_ms"].as_f64().unwrap() >= SLOW_MS as f64,
        "{}",
        s[0]
    );
    assert_eq!(title(&s[0]), "SDL");
}

/// Serve HTTP on 127.0.0.1, one thread per connection: `handler` maps a path
/// to a delay in ms, extra header lines and a body.
fn serve_delayed(handler: fn(&str) -> (u64, &'static str, String)) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                let _ = r.read_line(&mut first);
                let mut line = String::new();
                while r.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let path = first.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let (ms, headers, body) = handler(&path);
                std::thread::sleep(Duration::from_millis(ms));
                let mut s = stream;
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    port
}

/// How long the test servers take for images and fonts.
const RESOURCE_MS: u64 = 1500;

/// A 100 x 3000 image: the page scrolls once it is in.
const TALL_SVG: &str = "<svg xmlns='http://www.w3.org/2000/svg' width='100' height='3000'>\
    <rect width='100' height='3000' fill='green'/></svg>";

#[test]
fn the_first_paint_does_not_wait_for_images_on_the_network() {
    let port = serve_delayed(|path| match path {
        "/tall.svg" => (
            RESOURCE_MS,
            "Content-Type: image/svg+xml\r\n",
            TALL_SVG.into(),
        ),
        _ => (
            0,
            "Content-Type: text/html\r\n",
            "<title>t</title><p>Text</p><img src=/tall.svg>".into(),
        ),
    });
    // `wait` waits for the whole load; the first state is from before the
    // image came.
    let s = run(
        &format!("http://127.0.0.1:{port}/"),
        &format!("sleep {}; state; wait; state", RESOURCE_MS / 2),
        &[],
    );
    // Painted with its text and without the image.
    let first = s[0]["load_ms"].as_f64().unwrap();
    assert!(first > 0.0 && first < RESOURCE_MS as f64 / 2.0, "{}", s[0]);
    assert_eq!(s[0]["contentful_ms"].as_f64(), Some(first), "{}", s[0]);
    assert_eq!(s[0]["max_scroll"].as_f64(), Some(0.0), "{}", s[0]);
    // Then the image came and the page grew.
    assert!(s[1]["max_scroll"].as_f64().unwrap() > 2000.0, "{}", s[1]);
    assert!(
        s[1]["complete_ms"].as_f64().unwrap() >= RESOURCE_MS as f64,
        "{}",
        s[1]
    );
}

#[test]
fn text_in_a_loading_web_font_follows_font_display() {
    // The font comes late and is no font at all (it fails to decode), so
    // the text shows in the fallback when it is in.
    let port = serve_delayed(|path| {
        let display = match path {
            "/font.woff2" => return (RESOURCE_MS, "Content-Type: font/woff2\r\n", "x".into()),
            "/block" => "block",
            "/swap" => "swap",
            "/fallback" => "fallback",
            _ => "auto",
        };
        (
            0,
            "Content-Type: text/html\r\n",
            format!(
                "<title>t</title><style>@font-face {{ font-family: Late; src: url(/font.woff2); \
                 font-display: {display} }} p {{ font-family: Late, sans-serif }}</style><p>Text</p>"
            ),
        )
    });
    let base = format!("http://127.0.0.1:{port}");
    let page = |name: &str| {
        let s = run(&format!("{base}/{name}"), "wait; state", &[]);
        let first = s[0]["load_ms"].as_f64().unwrap();
        assert!(first < RESOURCE_MS as f64 / 2.0, "{name}: {}", s[0]);
        (first, s[0]["contentful_ms"].as_f64().unwrap())
    };
    // Block (and auto, Chrome's block): the first paint has invisible text;
    // the text shows when the font is in (here before the 3 s period ends).
    for name in ["block", "auto"] {
        let (_, text) = page(name);
        assert!(text >= RESOURCE_MS as f64, "{name}: text at {text}");
    }
    // Swap: the fallback at once.
    let (first, text) = page("swap");
    assert_eq!(text, first);
    // Fallback: invisible for 100 ms, then the fallback.
    let (first, text) = page("fallback");
    assert!(
        text >= first.max(100.0) && text < RESOURCE_MS as f64 / 2.0,
        "fallback: text at {text}, first paint at {first}"
    );
}

#[test]
fn stylesheets_apply_in_document_order_whatever_order_they_arrive_in() {
    // Each sheet is parsed on the thread that fetched it, so `b.css` (slow)
    // is parsed last; `a.css` is linked twice. Document order: a, the
    // inline sheet, b, a: the last `a.css` sets the height.
    let port = serve_delayed(|path| match path {
        "/a.css" => (
            0,
            "Content-Type: text/css\r\n",
            "#box { height: 4000px } @layer x { #box { width: 10px } }".into(),
        ),
        "/b.css" => (
            300,
            "Content-Type: text/css\r\n",
            "#box { height: 3000px } @layer y { #box { width: 20px } }".into(),
        ),
        _ => (
            0,
            "Content-Type: text/html\r\n",
            "<!doctype html><title>t</title>
             <link rel=stylesheet href=/a.css>
             <style>#box { height: 2000px }</style>
             <link rel=stylesheet href=/b.css>
             <link rel=stylesheet href=/a.css>
             <body style='margin: 0'><div id=box></div>"
                .into(),
        ),
    });
    let s = run(&format!("http://127.0.0.1:{port}/"), "wait; state", &[]);
    // 4000 px of page in a 520 px tall viewport.
    let max = s[0]["max_scroll"].as_f64().unwrap();
    assert!((3400.0..3600.0).contains(&max), "{}", s[0]);
}

fn huginn_net_file_url(path: &std::path::Path) -> String {
    format!("file://{}", path.to_str().expect("UTF-8 path"))
}
