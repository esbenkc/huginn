//! The HTTP cache and connection reuse of a profile's `Network`, against a
//! keep-alive test server on 127.0.0.1.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;

use huginn_net::{
    Client, CookieJar, Credentials, HttpCache, LoadKind, Loader, Mode, Network, RedirectMode,
    ResourceKind, ScriptRequest, Url,
};

/// A request the server received.
#[derive(Clone, Debug)]
struct Req {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    /// Which connection it came on (0, 1, ...).
    conn: usize,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// An answer: status line, header lines, body. `chunked`: send the body in
/// two chunks (`Transfer-Encoding: chunked`). `hang_up`: close the
/// connection instead of answering.
#[derive(Default)]
struct Reply {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
    chunked: bool,
    hang_up: bool,
}

fn ok(headers: &[(&'static str, &str)], body: &[u8]) -> Reply {
    Reply {
        status: "200 OK",
        headers: headers.iter().map(|(n, v)| (*n, (*v).to_owned())).collect(),
        body: body.to_vec(),
        ..Reply::default()
    }
}

struct Server {
    port: u16,
    log: Arc<Mutex<Vec<Req>>>,
}

impl Server {
    fn url(&self, path: &str) -> Url {
        Url::parse(&format!("http://127.0.0.1:{}{path}", self.port)).unwrap()
    }

    fn requests(&self, path: &str) -> Vec<Req> {
        self.log
            .lock()
            .iter()
            .filter(|r| r.path == path)
            .cloned()
            .collect()
    }

    fn connections(&self) -> usize {
        self.log
            .lock()
            .iter()
            .map(|r| r.conn + 1)
            .max()
            .unwrap_or(0)
    }
}

/// An HTTP/1.1 server that keeps connections open.
fn serve(handler: impl Fn(&Req) -> Reply + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    let handler = Arc::new(handler);
    let conns = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (seen, handler) = (seen.clone(), handler.clone());
            let conn = conns.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || connection(stream, conn, &seen, &*handler));
        }
    });
    Server { port, log }
}

fn connection(
    stream: TcpStream,
    conn: usize,
    log: &Mutex<Vec<Req>>,
    handler: &(dyn Fn(&Req) -> Reply + Send + Sync),
) {
    let mut out = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);
    loop {
        let mut first = String::new();
        if r.read_line(&mut first).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = first.split_whitespace();
        let mut req = Req {
            method: parts.next().unwrap_or("").to_owned(),
            path: parts.next().unwrap_or("").to_owned(),
            headers: Vec::new(),
            conn,
        };
        let mut len = 0;
        loop {
            let mut h = String::new();
            r.read_line(&mut h).unwrap();
            if h.trim().is_empty() {
                break;
            }
            let (n, v) = h.split_once(':').unwrap();
            let n = n.trim().to_ascii_lowercase();
            if n == "content-length" {
                len = v.trim().parse().unwrap();
            }
            req.headers.push((n, v.trim().to_owned()));
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body).unwrap();
        log.lock().push(req.clone());
        let reply = handler(&req);
        if reply.hang_up {
            return;
        }
        let mut head = format!("HTTP/1.1 {}\r\n", reply.status);
        for (n, v) in &reply.headers {
            head.push_str(&format!("{n}: {v}\r\n"));
        }
        if reply.chunked {
            let (a, b) = reply.body.split_at(reply.body.len() / 2);
            head.push_str("Transfer-Encoding: chunked\r\n\r\n");
            let mut msg = head.into_bytes();
            for chunk in [a, b] {
                msg.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
                msg.extend_from_slice(chunk);
                msg.extend_from_slice(b"\r\n");
            }
            msg.extend_from_slice(b"0\r\n\r\n");
            out.write_all(&msg).unwrap();
        } else {
            head.push_str(&format!("Content-Length: {}\r\n\r\n", reply.body.len()));
            let mut msg = head.into_bytes();
            if req.method != "HEAD" {
                msg.extend_from_slice(&reply.body);
            }
            out.write_all(&msg).unwrap();
        }
    }
}

fn network(cache: HttpCache) -> Arc<Network> {
    Network::new(Arc::new(CookieJar::in_memory()), cache)
}

/// A loader for a page on `page` (its client document).
fn page(net: &Arc<Network>, page: &Url) -> Loader {
    let l = Loader::with_network(net.clone());
    l.set_client(Some(Client::new(page.clone())));
    l
}

fn text(loader: &Loader, url: &Url, kind: ResourceKind) -> String {
    String::from_utf8(loader.fetch_uncached(url, kind).unwrap().body).unwrap()
}

#[test]
fn fresh_responses_come_from_the_cache_for_later_pages() {
    let s = serve(|_| ok(&[("Cache-Control", "max-age=60")], b"p { color: red }"));
    let net = network(HttpCache::in_memory(1 << 20));
    let home = s.url("/");
    for _ in 0..3 {
        // A new page each time, like a navigation.
        let l = page(&net, &home);
        assert_eq!(
            text(&l, &s.url("/a.css"), ResourceKind::Stylesheet),
            "p { color: red }"
        );
    }
    assert_eq!(s.requests("/a.css").len(), 1);
}

#[test]
fn stale_responses_are_revalidated() {
    let s = serve(|r| {
        match (
            r.path.as_str(),
            r.header("if-none-match"),
            r.header("if-modified-since"),
        ) {
            ("/etag", Some("\"v1\""), _) | ("/lm", _, Some("Tue, 01 Oct 2024 10:00:00 GMT")) => {
                Reply {
                    status: "304 Not Modified",
                    headers: vec![
                        ("Cache-Control", "no-cache".into()),
                        ("X-Seen", "again".into()),
                    ],
                    ..Reply::default()
                }
            }
            ("/etag", ..) => ok(
                &[("Cache-Control", "no-cache"), ("ETag", "\"v1\"")],
                b"first",
            ),
            _ => ok(
                &[
                    ("Cache-Control", "max-age=0"),
                    ("Last-Modified", "Tue, 01 Oct 2024 10:00:00 GMT"),
                ],
                b"modified",
            ),
        }
    });
    let net = network(HttpCache::in_memory(1 << 20));
    let home = s.url("/");
    for _ in 0..2 {
        let l = page(&net, &home);
        let r = l
            .fetch_uncached(&s.url("/etag"), ResourceKind::Script)
            .unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"first"[..]));
        assert_eq!(text(&l, &s.url("/lm"), ResourceKind::Script), "modified");
    }
    let etag = s.requests("/etag");
    assert_eq!(etag.len(), 2);
    assert_eq!(etag[0].header("if-none-match"), None);
    assert_eq!(etag[1].header("if-none-match"), Some("\"v1\""));
    let lm = s.requests("/lm");
    assert_eq!(lm.len(), 2);
    assert_eq!(
        lm[1].header("if-modified-since"),
        Some("Tue, 01 Oct 2024 10:00:00 GMT")
    );
    // The 304's header fields replace the stored ones.
    let r = page(&net, &home)
        .fetch_uncached(&s.url("/etag"), ResourceKind::Script)
        .unwrap();
    assert_eq!(r.header("x-seen"), Some("again"));
    assert_eq!(r.header("etag"), Some("\"v1\""));
}

#[test]
fn no_store_and_unsafe_methods_keep_responses_out() {
    let s = serve(|r| match r.path.as_str() {
        "/secret" => ok(&[("Cache-Control", "no-store, max-age=60")], b"secret"),
        _ => ok(&[("Cache-Control", "max-age=60")], r.method.as_bytes()),
    });
    let net = network(HttpCache::in_memory(1 << 20));
    let home = s.url("/");
    for _ in 0..2 {
        text(&page(&net, &home), &s.url("/secret"), ResourceKind::Other);
    }
    assert_eq!(s.requests("/secret").len(), 2);

    // A POST to a URL drops what the cache had for it (RFC 9111 4.4).
    let l = page(&net, &home);
    assert_eq!(text(&l, &s.url("/doc"), ResourceKind::Other), "GET");
    assert_eq!(text(&l, &s.url("/doc"), ResourceKind::Other), "GET");
    assert_eq!(s.requests("/doc").len(), 1);
    let post = l
        .fetch_for_script(ScriptRequest {
            url: s.url("/doc"),
            method: "POST".into(),
            headers: Vec::new(),
            body: Some(b"x".to_vec()),
            mode: Mode::Cors,
            credentials: Credentials::SameOrigin,
            redirect: RedirectMode::Follow,
        })
        .unwrap();
    assert_eq!(post.body, b"POST");
    assert_eq!(text(&l, &s.url("/doc"), ResourceKind::Other), "GET");
    assert_eq!(s.requests("/doc").len(), 3);
}

#[test]
fn vary_keeps_the_variant_for_the_request_headers() {
    // `Accept` differs between an image and a stylesheet request.
    let s = serve(|r| {
        let accept = r.header("accept").unwrap_or("").to_owned();
        ok(
            &[("Cache-Control", "max-age=60"), ("Vary", "Accept")],
            accept.as_bytes(),
        )
    });
    let net = network(HttpCache::in_memory(1 << 20));
    let l = page(&net, &s.url("/"));
    let url = s.url("/v");
    let image = text(&l, &url, ResourceKind::Image);
    let style = text(&l, &url, ResourceKind::Stylesheet);
    assert_ne!(image, style);
    assert_eq!(text(&l, &url, ResourceKind::Stylesheet), style);
    assert_eq!(s.requests("/v").len(), 2);

    // `Vary: *` is never reused.
    let s = serve(|_| ok(&[("Cache-Control", "max-age=60"), ("Vary", "*")], b"x"));
    let l = page(&net, &s.url("/"));
    text(&l, &s.url("/star"), ResourceKind::Other);
    text(&l, &s.url("/star"), ResourceKind::Other);
    assert_eq!(s.requests("/star").len(), 2);
}

#[test]
fn entries_are_partitioned_by_top_level_site() {
    let s = serve(|_| ok(&[("Cache-Control", "max-age=60")], b"shared"));
    let net = network(HttpCache::in_memory(1 << 20));
    let thing = s.url("/thing.js");
    let a = page(&net, &Url::parse("http://localhost:1/").unwrap());
    let b = page(&net, &s.url("/"));
    text(&a, &thing, ResourceKind::Script);
    text(&a, &thing, ResourceKind::Script);
    assert_eq!(s.requests("/thing.js").len(), 1);
    // Another top-level site does not see what the first one loaded.
    text(&b, &thing, ResourceKind::Script);
    assert_eq!(s.requests("/thing.js").len(), 2);
}

#[test]
fn reload_revalidates_the_document_and_hard_reload_skips_the_cache() {
    let s = serve(|r| {
        if r.header("if-none-match") == Some("\"d\"") {
            return Reply {
                status: "304 Not Modified",
                ..Reply::default()
            };
        }
        ok(
            &[("Cache-Control", "max-age=600"), ("ETag", "\"d\"")],
            b"<p>doc",
        )
    });
    let net = network(HttpCache::in_memory(1 << 20));
    let doc = s.url("/doc");
    let style = s.url("/s.css");
    let load = |kind| {
        let l = Loader::with_network(net.clone());
        l.set_load_kind(kind);
        let (r, _) = l.navigate(&doc, None, None).unwrap();
        assert_eq!(r.body, b"<p>doc");
        l.set_client(Some(Client::new(r.url.clone())));
        text(&l, &style, ResourceKind::Stylesheet);
    };
    load(LoadKind::Normal);
    load(LoadKind::Normal);
    assert_eq!(s.requests("/doc").len(), 1);
    // Back and forward: no request even for a stale document.
    load(LoadKind::History);
    assert_eq!(s.requests("/doc").len(), 1);

    load(LoadKind::Reload);
    let docs = s.requests("/doc");
    assert_eq!(docs.len(), 2);
    assert_eq!(docs[1].header("cache-control"), Some("max-age=0"));
    assert_eq!(docs[1].header("if-none-match"), Some("\"d\""));
    // Subresources of a reload come from the cache.
    assert_eq!(s.requests("/s.css").len(), 1);

    load(LoadKind::HardReload);
    for path in ["/doc", "/s.css"] {
        let last = s.requests(path).pop().unwrap();
        assert_eq!(last.header("cache-control"), Some("no-cache"), "{path}");
        assert_eq!(last.header("pragma"), Some("no-cache"), "{path}");
        assert_eq!(last.header("if-none-match"), None, "{path}");
    }
    assert_eq!(s.requests("/doc").len(), 3);
    assert_eq!(s.requests("/s.css").len(), 2);
}

#[test]
fn a_script_with_a_validator_gets_the_server_answer() {
    let s = serve(|r| {
        if r.header("if-none-match").is_some() {
            return Reply {
                status: "304 Not Modified",
                ..Reply::default()
            };
        }
        ok(
            &[("Cache-Control", "max-age=60"), ("ETag", "\"x\"")],
            b"data",
        )
    });
    let net = network(HttpCache::in_memory(1 << 20));
    let l = page(&net, &s.url("/"));
    text(&l, &s.url("/api"), ResourceKind::Other);
    let r = l
        .fetch_for_script(ScriptRequest {
            url: s.url("/api"),
            method: "GET".into(),
            headers: vec![("If-None-Match".into(), "\"x\"".into())],
            body: None,
            mode: Mode::Cors,
            credentials: Credentials::SameOrigin,
            redirect: RedirectMode::Follow,
        })
        .unwrap();
    assert_eq!(r.status, 304);
    assert_eq!(s.requests("/api").len(), 2);
}

#[test]
fn connections_are_reused_across_pages_and_encodings() {
    let br = vec![0x8f, 0x00, 0x80, 0x68, 0x69, 0x03]; // "hi", `brotli -q 0`
    let mut gz = Vec::new();
    {
        let mut e = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::fast());
        e.write_all(b"hi").unwrap();
        e.finish().unwrap();
    }
    let s = serve(move |r| {
        let (encoding, body) = match r.path.as_str() {
            "/br" => ("br", br.clone()),
            "/gz" => ("gzip", gz.clone()),
            _ => ("identity", b"hi".to_vec()),
        };
        Reply {
            status: "200 OK",
            headers: vec![("Content-Encoding", encoding.into())],
            body,
            // The end of a chunked body comes after the end of the
            // compressed data.
            chunked: true,
            hang_up: false,
        }
    });
    // No cache: every fetch is an exchange.
    let net = network(HttpCache::off());
    for _ in 0..3 {
        let l = page(&net, &s.url("/"));
        for path in ["/br", "/gz", "/plain"] {
            assert_eq!(text(&l, &s.url(path), ResourceKind::Script), "hi");
        }
    }
    assert_eq!(s.log.lock().len(), 9);
    assert_eq!(s.connections(), 1);
}

#[test]
fn a_request_on_a_connection_the_server_dropped_is_sent_again() {
    // The second request on a connection gets no answer: the server
    // closed it, as servers do with idle connections.
    let answered = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let seen = answered.clone();
    let s = serve(move |r| {
        if !seen.lock().insert(r.conn) {
            return Reply {
                hang_up: true,
                ..Reply::default()
            };
        }
        ok(&[], b"fine")
    });
    let net = network(HttpCache::off());
    let l = page(&net, &s.url("/"));
    for _ in 0..3 {
        assert_eq!(text(&l, &s.url("/x"), ResourceKind::Other), "fine");
    }
    // Each later request went out on the old connection, then on a new one.
    assert_eq!(s.requests("/x").len(), 5);
    assert_eq!(s.connections(), 3);
}

#[test]
fn disk_cache_survives_a_restart_and_concurrent_writers() {
    let dir = std::env::temp_dir().join(format!("huginn-cache-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // Bodies big enough that a torn write would show.
    let body = |path: &str| match path.strip_prefix("/c") {
        Some(_) => vec![b'c'; 15_000],
        None => path.repeat(20_000).into_bytes(),
    };
    let s = serve(move |r| ok(&[("Cache-Control", "max-age=600")], &body(&r.path)));
    let paths: Vec<String> = (0..12).map(|i| format!("/r{i}")).collect();
    // Two "processes" (networks on the same directory), four threads each,
    // all storing the same entries at once.
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let net = network(HttpCache::on_disk(&dir, 64 << 20));
            for _ in 0..4 {
                let (net, s, paths) = (net.clone(), &s, &paths);
                scope.spawn(move || {
                    let l = page(&net, &s.url("/"));
                    for p in paths {
                        let r = l.fetch_uncached(&s.url(p), ResourceKind::Image).unwrap();
                        assert_eq!(r.body, p.repeat(20_000).into_bytes());
                    }
                });
            }
        }
    });
    let before = s.log.lock().len();
    assert!(before >= paths.len());
    // A new process reads every entry back, whole, without a request.
    let net = network(HttpCache::on_disk(&dir, 64 << 20));
    let l = page(&net, &s.url("/"));
    for p in &paths {
        let r = l.fetch_uncached(&s.url(p), ResourceKind::Image).unwrap();
        assert_eq!(r.body, p.repeat(20_000).into_bytes());
    }
    assert_eq!(s.log.lock().len(), before);
    assert_eq!(net.cache().len(), paths.len());
    // No temp files are left.
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.iter().all(|n| !n.starts_with("tmp-")), "{names:?}");
    net.cache().clear();
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

    // A small cap: the least recently used entries go.
    let net = network(HttpCache::on_disk(&dir, 150_000));
    let l = page(&net, &s.url("/"));
    for i in 0..12 {
        l.fetch_uncached(&s.url(&format!("/c{i}")), ResourceKind::Image)
            .unwrap();
        // `/c0` stays in use.
        l.fetch_uncached(&s.url("/c0"), ResourceKind::Image)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let total: u64 = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    assert!(total <= 150_000, "{total}");
    let before = s.requests("/c0").len() + s.requests("/c1").len();
    l.fetch_uncached(&s.url("/c0"), ResourceKind::Image)
        .unwrap();
    l.fetch_uncached(&s.url("/c1"), ResourceKind::Image)
        .unwrap();
    // `/c0` was kept, `/c1` was dropped.
    assert_eq!(s.requests("/c0").len(), 1);
    assert_eq!(
        s.requests("/c0").len() + s.requests("/c1").len(),
        before + 1
    );
    let _ = std::fs::remove_dir_all(&dir);
}
