//! Preloads and requests in flight (`Loader::preload`): the parser's request
//! takes a preload of the same request, one in another CORS mode does not,
//! identical requests share one exchange, and low-priority preloads wait.
//! Against a threaded HTTP/1.1 test server on 127.0.0.1.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use huginn_net::{Client, CorsSettings, Loader, Priority, ResourceKind, Url};

/// A request the server got: path, headers, and when (since the server started).
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
    at: Duration,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

struct Server {
    port: u16,
    log: Arc<Mutex<Vec<Seen>>>,
}

impl Server {
    fn url(&self, path: &str) -> Url {
        Url::parse(&format!("http://127.0.0.1:{}{path}", self.port)).unwrap()
    }

    fn requests(&self, path: &str) -> Vec<Seen> {
        self.log
            .lock()
            .iter()
            .filter(|r| r.path == path)
            .cloned()
            .collect()
    }
}

/// Answers every request on a thread of its own after `delay(path)`, with
/// CORS headers that allow the `Origin` it got (with credentials).
fn serve(delay: fn(&str) -> u64) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let start = Instant::now();
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let seen = seen.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if r.read_line(&mut first).is_err() {
                    return;
                }
                let path = first.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let mut headers = Vec::new();
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                    if let Some((n, v)) = h.split_once(':') {
                        headers.push((n.trim().to_ascii_lowercase(), v.trim().to_owned()));
                    }
                }
                let origin = headers
                    .iter()
                    .find(|(n, _)| n == "origin")
                    .map(|(_, v)| v.clone());
                seen.lock().push(Seen {
                    path: path.clone(),
                    headers,
                    at: start.elapsed(),
                });
                std::thread::sleep(Duration::from_millis(delay(&path)));
                let cors = origin.map_or(String::new(), |o| {
                    format!("Access-Control-Allow-Origin: {o}\r\nAccess-Control-Allow-Credentials: true\r\n")
                });
                let mut s = stream;
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n{cors}Content-Length: {}\r\nConnection: close\r\n\r\n{path}",
                    path.len()
                );
            });
        }
    });
    Server { port, log }
}

/// A loader for a page on `page`.
fn loader(page: &str) -> Arc<Loader> {
    let l = Loader::new();
    l.set_client(Some(Client::new(Url::parse(page).unwrap())));
    Arc::new(l)
}

#[test]
fn the_parsers_request_takes_the_preload_instead_of_fetching_again() {
    let s = serve(|_| 200);
    let l = loader(&format!("http://127.0.0.1:{}/page", s.port));
    let (css, img) = (s.url("/a.css"), s.url("/i.png"));
    l.preload(
        &css,
        ResourceKind::Stylesheet,
        CorsSettings::NoCors,
        Priority::Blocking,
    );
    l.preload(
        &img,
        ResourceKind::Image,
        CorsSettings::NoCors,
        Priority::Low,
    );
    // Still in flight: the parser's requests wait for the preloads.
    let r = l.fetch(&css, ResourceKind::Stylesheet).unwrap();
    assert_eq!(r.body, b"/a.css");
    let r = l.fetch_uncached(&img, ResourceKind::Image).unwrap();
    assert_eq!(r.body, b"/i.png");
    // Already in: the stylesheet comes from the loader's memory.
    let r = l.fetch(&css, ResourceKind::Stylesheet).unwrap();
    assert_eq!(r.body, b"/a.css");
    assert_eq!(s.requests("/a.css").len(), 1);
    assert_eq!(s.requests("/i.png").len(), 1);
}

#[test]
fn identical_requests_in_flight_share_one_exchange() {
    let s = serve(|_| 300);
    let l = loader(&format!("http://127.0.0.1:{}/page", s.port));
    let css = s.url("/dup.css");
    // A page that links the same stylesheet six times.
    let links = vec![css.clone(); 6];
    let got = l.map_parallel(&links, |u| l.fetch(u, ResourceKind::Stylesheet).unwrap());
    assert!(got.iter().all(|r| r.body == b"/dup.css"));
    assert_eq!(s.requests("/dup.css").len(), 1);
}

#[test]
fn a_request_in_another_cors_mode_or_destination_does_not_take_the_preload() {
    let s = serve(|_| 0);
    // A page on another origin: CORS requests send `Origin`, no-cors ones do not.
    let l = loader("http://localhost:1/page");
    let (css, js, font) = (s.url("/a.css"), s.url("/b.js"), s.url("/f.woff2"));
    l.preload(
        &css,
        ResourceKind::Stylesheet,
        CorsSettings::Anonymous,
        Priority::Blocking,
    );
    l.preload(
        &js,
        ResourceKind::Script,
        CorsSettings::UseCredentials,
        Priority::Blocking,
    );
    l.preload(
        &font,
        ResourceKind::Stylesheet,
        CorsSettings::NoCors,
        Priority::Blocking,
    );
    // Same URLs, other modes, credentials or destination.
    l.fetch_with(&css, ResourceKind::Stylesheet, CorsSettings::NoCors)
        .unwrap();
    l.fetch_with(&js, ResourceKind::Script, CorsSettings::Anonymous)
        .unwrap();
    l.fetch_uncached(&font, ResourceKind::Font).unwrap();
    let modes = |p: &str| -> Vec<(Option<String>, Option<String>)> {
        s.requests(p)
            .iter()
            .map(|r| {
                (
                    r.header("origin").map(str::to_owned),
                    r.header("sec-fetch-dest").map(str::to_owned),
                )
            })
            .collect()
    };
    let origin = || Some("http://localhost:1".to_owned());
    let mut css_modes = modes("/a.css");
    css_modes.sort();
    assert_eq!(
        css_modes,
        [
            (None, Some("style".into())),
            (origin(), Some("style".into()))
        ]
    );
    assert_eq!(s.requests("/b.js").len(), 2);
    let mut font_modes = modes("/f.woff2");
    font_modes.sort();
    assert_eq!(
        font_modes,
        [
            (None, Some("style".into())),
            (origin(), Some("font".into()))
        ]
    );
}

#[test]
fn less_urgent_preloads_wait_for_more_urgent_ones_and_low_ones_for_a_hold() {
    let s = serve(|p| match p {
        "/slow.css" => 400,
        "/f.woff2" => 200,
        _ => 0,
    });
    let l = loader(&format!("http://127.0.0.1:{}/page", s.port));
    let first = |p: &str| -> Duration {
        let t = Instant::now();
        loop {
            if let Some(r) = s.requests(p).first() {
                return r.at;
            }
            assert!(t.elapsed() < Duration::from_secs(5), "{p} never went");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let (css, font, img) = (s.url("/slow.css"), s.url("/f.woff2"), s.url("/a.png"));
    l.preload(
        &img,
        ResourceKind::Image,
        CorsSettings::NoCors,
        Priority::Low,
    );
    l.preload(
        &font,
        ResourceKind::Font,
        CorsSettings::Anonymous,
        Priority::High,
    );
    l.preload(
        &css,
        ResourceKind::Stylesheet,
        CorsSettings::NoCors,
        Priority::Blocking,
    );
    // The stylesheet first, the font after its response, the image after
    // the font's, although they were asked for in the other order.
    let (c, f, i) = (first("/slow.css"), first("/f.woff2"), first("/a.png"));
    assert!(f >= c + Duration::from_millis(350), "{c:?} {f:?}");
    assert!(i >= f + Duration::from_millis(150), "{f:?} {i:?}");

    // While the window holds requests for its first paint, low-priority
    // preloads wait (high ones do not); the parser's own request for one
    // sends it at once.
    let hold = l.hold_script_requests();
    l.preload(
        &s.url("/b.png"),
        ResourceKind::Image,
        CorsSettings::NoCors,
        Priority::Low,
    );
    l.preload(
        &s.url("/c.png"),
        ResourceKind::Image,
        CorsSettings::NoCors,
        Priority::Low,
    );
    l.preload(
        &s.url("/g.woff2"),
        ResourceKind::Font,
        CorsSettings::Anonymous,
        Priority::High,
    );
    first("/g.woff2");
    std::thread::sleep(Duration::from_millis(300));
    assert!(s.requests("/b.png").is_empty() && s.requests("/c.png").is_empty());
    l.fetch_uncached(&s.url("/b.png"), ResourceKind::Image)
        .unwrap();
    assert_eq!(s.requests("/b.png").len(), 1);
    assert!(s.requests("/c.png").is_empty());
    drop(hold);
    first("/c.png");
    // A low-priority preload that turns out to be render-blocking goes at once.
    let _hold = l.hold_script_requests();
    l.preload(
        &s.url("/d.js"),
        ResourceKind::Script,
        CorsSettings::NoCors,
        Priority::Low,
    );
    std::thread::sleep(Duration::from_millis(100));
    assert!(s.requests("/d.js").is_empty());
    l.preload(
        &s.url("/d.js"),
        ResourceKind::Script,
        CorsSettings::NoCors,
        Priority::Blocking,
    );
    first("/d.js");
}
