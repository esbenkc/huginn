//! Cookies, CORS and referrers through the real binary (`huginn open
//! --commands`) against local test servers. Two servers on 127.0.0.1 and a
//! `localhost` URL give same-site, cross-origin and cross-site requests.
//! Window layout at 800x600: the page starts 80 px down.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};

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
    let pre = log
        .iter()
        .find(|r| r.method == "OPTIONS" && r.path == "/put")
        .unwrap();
    assert_eq!(pre.header("access-control-request-method"), "PUT");
    assert_eq!(pre.header("access-control-request-headers"), "x-token");
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

fn huginn_net_file_url(path: &std::path::Path) -> String {
    format!("file://{}", path.to_str().expect("UTF-8 path"))
}
