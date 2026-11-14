//! HTTP/1.1 and HTTP/2 over TCP and TLS, for the loader.
//!
//! hyper speaks both protocols; this module owns the connections. TLS
//! offers `h2` and `http/1.1` (ALPN), like Chrome, and the server picks.
//! Plain `http:` is HTTP/1.1 (browsers do not speak cleartext HTTP/2).
//!
//! - **HTTP/2.** One connection per host carries every request to it at
//!   once. While the first TLS connection to a host is being opened, other
//!   requests to the host wait for it, as in Chrome: if the server picks
//!   `h2`, they share it; if it picks `http/1.1`, they open their own.
//! - **HTTP/1.1.** At most 6 connections per host (Chrome's limit), used one
//!   request at a time. A connection whose response was read to the end goes
//!   back to the pool. At most 32 idle connections are kept in all.
//! - Idle connections of both kinds are kept for 5 minutes, with their TLS
//!   sessions for resumption (one rustls config per network).
//! - Name lookups are kept for a minute, and are only made for new
//!   connections.
//!
//! The API is blocking: callers are the loader's worker threads and the
//! engine threads. One tokio worker thread (`huginn-net`) drives the
//! connections (reads, writes, TLS records, HTTP/2 frames). DNS, TCP connect
//! and the TLS handshake of a new connection run on the thread that asked
//! for it, so handshakes to different hosts run in parallel.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::pin::pin;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body as _, Incoming};
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::netlog::{Exchange, Protocol};
use crate::{NetError, compression, ua};

/// HTTP/1.1 connections per host, idle or in use (Chrome's limit).
const MAX_PER_HOST: usize = 6;
/// Idle HTTP/1.1 connections kept in all.
const IDLE_TOTAL: usize = 32;
/// How long an idle connection is kept: Chrome's limit for sockets that
/// were used once.
const IDLE_AGE: Duration = Duration::from_secs(300);
/// How long a resolved address is used: Chrome's default for answers of
/// the system resolver.
const DNS_TTL: Duration = Duration::from_secs(60);
/// Time limit for one exchange: connecting, the response head and the body.
const TIMEOUT: Duration = Duration::from_secs(30);

type ReqBody = Full<Bytes>;

/// The connections of one network, and how to make new ones.
pub(crate) struct Transport {
    /// Made on the first request, so loaders that only read `file:` and
    /// `data:` URLs start no thread.
    runtime: LazyLock<tokio::runtime::Runtime>,
    /// One TLS config, so sessions are resumed across connections.
    tls: Arc<rustls::ClientConfig>,
    pool: Arc<Pool>,
    dns: Mutex<HashMap<(String, u16), (Vec<SocketAddr>, Instant)>>,
    loopback_only: bool,
}

/// Where a connection goes. Requests from public pages (`public_only`) get
/// connections of their own, made with the private addresses dropped.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    https: bool,
    host: String,
    port: u16,
    public_only: bool,
}

#[derive(Default)]
struct Pool {
    hosts: Mutex<HashMap<Key, Host>>,
    /// Woken when a connection becomes free, is made, or fails.
    changed: Notify,
}

#[derive(Default)]
struct Host {
    /// The host's HTTP/2 connection and when it was last used.
    h2: Option<(http2::SendRequest<ReqBody>, Instant)>,
    /// What the last TLS connection to the host negotiated (`None`: none
    /// was made yet).
    speaks_h2: Option<bool>,
    /// Idle HTTP/1.1 connections and since when.
    idle: Vec<(http1::SendRequest<ReqBody>, Instant)>,
    /// HTTP/1.1 connections in use, and connections being opened.
    busy: usize,
    /// Connections being opened.
    connecting: usize,
}

/// A request ready to send: method, absolute URI and headers.
pub(crate) type Head = http::request::Parts;

/// A response whose head arrived. Read its body with [`Response::read`] or
/// [`Response::drain`]. An HTTP/1.1 connection goes back to the pool when
/// the body was read to its end; otherwise it is closed.
pub(crate) struct Response {
    pub status: u16,
    pub headers: http::HeaderMap,
    body: Incoming,
    lease: Option<Lease>,
    handle: tokio::runtime::Handle,
    deadline: tokio::time::Instant,
}

/// One connection from the pool, for one exchange.
enum Conn {
    H1(Lease),
    H2(http2::SendRequest<ReqBody>),
}

/// An HTTP/1.1 connection taken from the pool (or newly made). Dropping it
/// returns the connection if `reusable`, else closes it.
struct Lease {
    pool: Arc<Pool>,
    key: Key,
    sender: Option<http1::SendRequest<ReqBody>>,
    reusable: bool,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut hosts = self.pool.hosts.lock();
        if let Some(h) = hosts.get_mut(&self.key) {
            h.busy = h.busy.saturating_sub(1);
            if self.reusable
                && let Some(s) = self.sender.take()
                && !s.is_closed()
            {
                h.idle.push((s, Instant::now()));
            }
        }
        // Keep at most IDLE_TOTAL idle connections: drop the oldest.
        let idle: usize = hosts.values().map(|h| h.idle.len()).sum();
        for _ in IDLE_TOTAL..idle {
            let oldest = hosts
                .iter()
                .filter_map(|(k, h)| h.idle.first().map(|(_, at)| (*at, k.clone())))
                .min_by_key(|(at, _)| *at);
            if let Some(h) = oldest.and_then(|(_, k)| hosts.get_mut(&k)) {
                h.idle.remove(0);
            }
        }
        drop(hosts);
        self.pool.changed.notify_waiters();
    }
}

/// Counts a connection being opened; undone when it is dropped (failure,
/// or the timeout dropped the future).
struct Opening<'a> {
    pool: &'a Pool,
    key: &'a Key,
    /// Became an HTTP/1.1 lease: `busy` stays counted.
    leased: bool,
}

impl Drop for Opening<'_> {
    fn drop(&mut self) {
        if let Some(h) = self.pool.hosts.lock().get_mut(self.key) {
            h.connecting = h.connecting.saturating_sub(1);
            if !self.leased {
                h.busy = h.busy.saturating_sub(1);
            }
        }
        self.pool.changed.notify_waiters();
    }
}

impl Transport {
    pub(crate) fn new(loopback_only: bool) -> Transport {
        Transport {
            runtime: LazyLock::new(runtime),
            tls: tls_config(),
            pool: Arc::default(),
            dns: Mutex::default(),
            loopback_only,
        }
    }

    /// A transport that trusts `tls`'s roots (tests).
    #[cfg(test)]
    pub(crate) fn with_tls(tls: Arc<rustls::ClientConfig>) -> Transport {
        Transport {
            tls,
            ..Transport::new(false)
        }
    }

    fn handle(&self) -> &tokio::runtime::Handle {
        self.runtime.handle()
    }

    /// Send `head` (with `body`) and wait for the response head. A request
    /// that failed on a kept-alive connection (the server closed it in the
    /// meantime, or an HTTP/2 connection went away) is sent once more, on
    /// a new connection, as in Chrome.
    pub(crate) fn send(
        &self,
        head: Head,
        body: Option<&[u8]>,
        public_only: bool,
        exchange: &mut Exchange,
    ) -> Result<Response, NetError> {
        let url = head.uri.to_string();
        let key = key_for(&head.uri, public_only)
            .ok_or_else(|| NetError::InvalidUrl(format!("{url}: no host")))?;
        let body = body.map(Bytes::copy_from_slice).unwrap_or_default();
        let handle = self.handle().clone();
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        let result = handle.block_on(async {
            let send = async {
                match self.send_once(&key, &head, &body, false, exchange).await {
                    Err((_, true)) => {
                        *exchange = Exchange::begin();
                        self.send_once(&key, &head, &body, true, exchange).await
                    }
                    r => r,
                }
            };
            tokio::time::timeout_at(deadline, send).await
        });
        let (resp, lease) = match result {
            Ok(Ok(r)) => r,
            Ok(Err((e, _))) => return Err(NetError::Transport(format!("{url}: {e}"))),
            Err(_) => return Err(NetError::Transport(format!("{url}: timed out"))),
        };
        let (parts, body) = resp.into_parts();
        Ok(Response {
            status: parts.status.as_u16(),
            headers: parts.headers,
            body,
            lease,
            handle,
            deadline,
        })
    }

    /// One try. The error says whether the connection was a kept-alive one
    /// (then a retry on a new connection may work).
    async fn send_once(
        &self,
        key: &Key,
        head: &Head,
        body: &Bytes,
        fresh: bool,
        exchange: &mut Exchange,
    ) -> Result<(http::Response<Incoming>, Option<Lease>), (String, bool)> {
        loop {
            let (conn, reused) = self
                .connection(key, fresh, exchange)
                .await
                .map_err(|e| (e, false))?;
            match conn {
                Conn::H2(mut sender) => {
                    exchange.protocol = Protocol::H2;
                    if sender.ready().await.is_err() {
                        if reused {
                            continue;
                        }
                        return Err(("connection closed".into(), false));
                    }
                    let req = request(head, body, true);
                    return match sender.send_request(req).await {
                        Ok(r) => Ok((r, None)),
                        Err(e) => Err((e.to_string(), reused)),
                    };
                }
                Conn::H1(mut lease) => {
                    exchange.protocol = Protocol::H1;
                    let sender = lease.sender.as_mut().expect("a new lease has its sender");
                    if sender.ready().await.is_err() {
                        // Closed while idle: take another.
                        if reused {
                            continue;
                        }
                        return Err(("connection closed".into(), false));
                    }
                    let req = request(head, body, false);
                    return match sender.send_request(req).await {
                        Ok(r) => Ok((r, Some(lease))),
                        Err(e) => Err((e.to_string(), reused)),
                    };
                }
            }
        }
    }

    /// A connection for `key`: the host's HTTP/2 connection, an idle
    /// HTTP/1.1 one, or a new one. `fresh`: only a new one. Returns whether
    /// it was kept alive from before.
    async fn connection(
        &self,
        key: &Key,
        fresh: bool,
        exchange: &mut Exchange,
    ) -> Result<(Conn, bool), String> {
        loop {
            let mut notified = pin!(self.pool.changed.notified());
            {
                let mut hosts = self.pool.hosts.lock();
                let h = hosts.entry(key.clone()).or_default();
                h.idle
                    .retain(|(s, at)| !s.is_closed() && at.elapsed() < IDLE_AGE);
                if let Some((s, used)) = &mut h.h2 {
                    if s.is_closed() || used.elapsed() >= IDLE_AGE {
                        h.h2 = None;
                    } else if !fresh {
                        *used = Instant::now();
                        return Ok((Conn::H2(s.clone()), true));
                    }
                }
                // A TLS connection is being opened and may become an
                // HTTP/2 connection for everyone: wait for it.
                let wait_for_alpn =
                    !fresh && key.https && h.speaks_h2 != Some(false) && h.connecting > 0;
                if !wait_for_alpn {
                    if !fresh && let Some((sender, _)) = h.idle.pop() {
                        h.busy += 1;
                        return Ok((Conn::H1(self.lease(key, sender)), true));
                    }
                    if h.busy < MAX_PER_HOST {
                        h.busy += 1;
                        h.connecting += 1;
                        sweep(&mut hosts);
                        break;
                    }
                }
                notified.as_mut().enable();
            }
            notified.await;
        }
        let mut opening = Opening {
            pool: &self.pool,
            key,
            leased: false,
        };
        let t = Instant::now();
        let made = self.connect(key, exchange).await;
        exchange.connect = Some(t.elapsed().saturating_sub(exchange.dns));
        let made = made?;
        let mut hosts = self.pool.hosts.lock();
        let h = hosts.entry(key.clone()).or_default();
        match made {
            Made::H2(sender) => {
                h.speaks_h2 = Some(true);
                h.h2 = Some((sender.clone(), Instant::now()));
                drop(hosts);
                drop(opening);
                Ok((Conn::H2(sender), false))
            }
            Made::H1(sender) => {
                if key.https {
                    h.speaks_h2 = Some(false);
                }
                drop(hosts);
                opening.leased = true;
                drop(opening);
                Ok((Conn::H1(self.lease(key, sender)), false))
            }
        }
    }

    fn lease(&self, key: &Key, sender: http1::SendRequest<ReqBody>) -> Lease {
        Lease {
            pool: self.pool.clone(),
            key: key.clone(),
            sender: Some(sender),
            reusable: false,
        }
    }

    /// Open a connection: name lookup, TCP, then TLS for `https:`.
    async fn connect(&self, key: &Key, exchange: &mut Exchange) -> Result<Made, String> {
        let addrs = self.resolve(key, exchange)?;
        let mut last = String::from("no address");
        let mut tcp = None;
        for a in addrs {
            match tokio::net::TcpStream::connect(a).await {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last = format!("{a}: {e}"),
            }
        }
        let tcp = tcp.ok_or(last)?;
        let _ = tcp.set_nodelay(true);
        if !key.https {
            return h1(tcp).await;
        }
        let name = rustls::pki_types::ServerName::try_from(key.host.clone())
            .map_err(|e| format!("{}: {e}", key.host))?;
        let tls = tokio_rustls::TlsConnector::from(Arc::clone(&self.tls))
            .connect(name, tcp)
            .await
            .map_err(|e| e.to_string())?;
        if tls.get_ref().1.alpn_protocol() == Some(b"h2") {
            let mut b = http2::Builder::new(TokioExecutor::new());
            // Chrome's flow-control windows.
            b.initial_stream_window_size(6 << 20)
                .initial_connection_window_size(15 << 20)
                .max_header_list_size(256 << 10);
            let (sender, conn) = b
                .handshake(TokioIo::new(tls))
                .await
                .map_err(|e| e.to_string())?;
            tokio::spawn(conn);
            Ok(Made::H2(sender))
        } else {
            h1(tls).await
        }
    }

    /// The addresses of `key`'s host. `localhost` and `*.localhost` always
    /// go to loopback (127.0.0.1, then ::1), never to DNS, like Chrome: the
    /// cookie jar treats them as secure. Loopback-only networks find no
    /// other host (no lookup). For public pages, loopback and private
    /// addresses are dropped (Local Network Access); the check is on the
    /// resolved addresses, so DNS rebinding cannot get around it.
    fn resolve(&self, key: &Key, exchange: &mut Exchange) -> Result<Vec<SocketAddr>, String> {
        let (host, port) = (key.host.as_str(), key.port);
        if host == "localhost" || host.ends_with(".localhost") {
            if key.public_only {
                return Err("host not found".into());
            }
            return Ok(vec![
                SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
            ]);
        }
        if self.loopback_only && !matches!(host, "127.0.0.1" | "::1") {
            return Err("host not found".into());
        }
        let addrs = if let Ok(ip) = host.parse::<IpAddr>() {
            vec![SocketAddr::new(ip, port)]
        } else {
            let k = (host.to_owned(), port);
            let cached = self
                .dns
                .lock()
                .get(&k)
                .filter(|(_, at)| at.elapsed() < DNS_TTL)
                .map(|(a, _)| a.clone());
            match cached {
                Some(a) => a,
                None => {
                    let t = Instant::now();
                    let found = (host, port).to_socket_addrs();
                    exchange.dns += t.elapsed();
                    let found: Vec<SocketAddr> =
                        found.map_err(|e| format!("host not found: {e}"))?.collect();
                    self.dns.lock().insert(k, (found.clone(), Instant::now()));
                    found
                }
            }
        };
        let addrs: Vec<SocketAddr> = addrs
            .into_iter()
            .filter(|a| !key.public_only || !crate::is_local_ip(a.ip()))
            .collect();
        if addrs.is_empty() {
            return Err("host not found".into());
        }
        Ok(addrs)
    }
}

/// The network thread: one tokio worker.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .thread_name("huginn-net")
        .enable_io()
        .enable_time()
        .build()
        .expect("the network thread starts")
}

/// rustls with the ring provider and the Mozilla roots (webpki-roots),
/// offering HTTP/2 and HTTP/1.1.
fn tls_config() -> Arc<rustls::ClientConfig> {
    tls_config_with(rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    })
}

fn tls_config_with(roots: rustls::RootCertStore) -> Arc<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Arc::new(config)
}

/// Close idle connections past their age and forget hosts with no
/// connections (run when a connection is opened, so it costs nothing on
/// requests that reuse one).
fn sweep(hosts: &mut HashMap<Key, Host>) {
    hosts.retain(|_, h| {
        h.idle
            .retain(|(s, at)| !s.is_closed() && at.elapsed() < IDLE_AGE);
        if h.h2
            .as_ref()
            .is_some_and(|(s, used)| s.is_closed() || used.elapsed() >= IDLE_AGE)
        {
            h.h2 = None;
        }
        h.h2.is_some() || !h.idle.is_empty() || h.busy > 0 || h.connecting > 0
    });
}

enum Made {
    H1(http1::SendRequest<ReqBody>),
    H2(http2::SendRequest<ReqBody>),
}

/// An HTTP/1.1 connection on `io`; its driver runs on the network thread.
async fn h1<T>(io: T) -> Result<Made, String>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut b = http1::Builder::new();
    // As lenient as Chrome with old servers.
    b.allow_spaces_after_header_name_in_responses(true)
        .allow_obsolete_multiline_headers_in_responses(true);
    let (sender, conn) = b
        .handshake(TokioIo::new(io))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(conn);
    Ok(Made::H1(sender))
}

/// The pool key of a URI.
fn key_for(uri: &http::Uri, public_only: bool) -> Option<Key> {
    let https = uri.scheme_str() == Some("https");
    let host = uri
        .host()?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
    Some(Key {
        https,
        host,
        port,
        public_only,
    })
}

/// The request for HTTP/1.1 or (`h2`) HTTP/2. HTTP/1.1 gets the path as its target and
/// a `Host` header; HTTP/2 keeps the absolute URI (`:scheme`,
/// `:authority`). Both get the user agent and `Accept-Encoding` unless the
/// request has its own.
fn request(head: &Head, body: &Bytes, h2: bool) -> http::Request<ReqBody> {
    use http::header::{ACCEPT_ENCODING, HOST, USER_AGENT};
    let mut headers = head.headers.clone();
    if !headers.contains_key(USER_AGENT) {
        headers.insert(USER_AGENT, http::HeaderValue::from_static(ua::USER_AGENT));
    }
    if !headers.contains_key(ACCEPT_ENCODING) {
        headers.insert(
            ACCEPT_ENCODING,
            http::HeaderValue::from_static(compression::ACCEPT_ENCODING),
        );
    }
    let uri = if h2 {
        headers.remove(HOST);
        head.uri.clone()
    } else {
        if let Some(a) = head.uri.authority()
            && !headers.contains_key(HOST)
        {
            // The authority without user info, and without a default port.
            let host = a.host();
            let value = match a.port_u16() {
                Some(p) if Some(p) != default_port(&head.uri) => format!("{host}:{p}"),
                _ => host.to_owned(),
            };
            if let Ok(v) = http::HeaderValue::from_str(&value) {
                headers.insert(HOST, v);
            }
        }
        head.uri
            .path_and_query()
            .map_or_else(|| http::Uri::from_static("/"), |p| p.clone().into())
    };
    let mut req = http::Request::new(Full::new(body.clone()));
    *req.method_mut() = head.method.clone();
    *req.uri_mut() = uri;
    *req.version_mut() = if h2 {
        http::Version::HTTP_2
    } else {
        http::Version::HTTP_11
    };
    *req.headers_mut() = headers;
    req
}

fn default_port(uri: &http::Uri) -> Option<u16> {
    match uri.scheme_str() {
        Some("https") => Some(443),
        Some("http") => Some(80),
        _ => None,
    }
}

impl Response {
    /// Read the body to its end; more than `limit` bytes is an error.
    pub(crate) fn read(&mut self, limit: u64) -> Result<Vec<u8>, String> {
        let hint = self.body.size_hint().exact().unwrap_or(0).min(limit);
        let mut out = Vec::with_capacity(hint as usize);
        self.read_into(&mut out, limit)?;
        Ok(out)
    }

    /// Read a small body (a redirect, an error, a `304`) to its end, so the
    /// connection can carry the next request; one over `limit` bytes is left
    /// (and its connection closed).
    pub(crate) fn drain(&mut self, limit: u64) {
        let _ = self.read_into(&mut Vec::new(), limit);
    }

    fn read_into(&mut self, out: &mut Vec<u8>, limit: u64) -> Result<(), String> {
        let (body, deadline) = (&mut self.body, self.deadline);
        let read = self.handle.block_on(async {
            let read = async {
                while let Some(frame) = body.frame().await {
                    let frame = frame.map_err(|e| e.to_string())?;
                    if let Ok(data) = frame.into_data() {
                        if (out.len() + data.len()) as u64 > limit {
                            return Err(format!("body is larger than {limit} bytes"));
                        }
                        out.extend_from_slice(&data);
                    }
                }
                Ok(())
            };
            tokio::time::timeout_at(deadline, read).await
        });
        match read {
            Ok(Ok(())) => {
                if let Some(l) = &mut self.lease {
                    l.reusable = true;
                }
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err("timed out".into()),
        }
    }
}

impl Drop for Response {
    fn drop(&mut self) {
        // An empty body (HEAD, 204, 304) needs no reading.
        if self.body.is_end_stream()
            && let Some(l) = &mut self.lease
        {
            l.reusable = true;
        }
    }
}

/// A TLS test server and a transport that trusts it.
#[cfg(test)]
pub(crate) mod test_server {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // A test CA and a certificate for 127.0.0.1 and localhost signed by it
    // (ECDSA P-256, valid until 2126), made with openssl for these tests.
    const CA: &[u8] = include_bytes!("testdata/ca.der");
    const LEAF: &[u8] = include_bytes!("testdata/leaf.der");
    const LEAF_KEY: &[u8] = include_bytes!("testdata/leaf.key.der");

    /// A TLS server on 127.0.0.1 that offers `alpn`, answers every request
    /// after `delay` with its HTTP version and path, closes a connection
    /// after `requests_per_connection` requests, and counts connections and
    /// requests in flight.
    pub(crate) struct Server {
        pub(crate) port: u16,
        pub(crate) connections: Arc<AtomicUsize>,
        pub(crate) max_in_flight: Arc<AtomicUsize>,
        _rt: tokio::runtime::Runtime,
    }

    pub(crate) fn server(
        alpn: &[&[u8]],
        delay: Duration,
        requests_per_connection: usize,
    ) -> Server {
        use hyper::server::conn::{http1 as s1, http2 as s2};
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![LEAF.to_vec().into()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(LEAF_KEY.to_vec().into()),
        )
        .unwrap();
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let (c, i, m) = (connections.clone(), in_flight, max_in_flight.clone());
        rt.spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                c.fetch_add(1, Ordering::SeqCst);
                let (acceptor, i, m) = (acceptor.clone(), i.clone(), m.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let h2 = tls.get_ref().1.alpn_protocol() == Some(b"h2");
                    let served = Arc::new(AtomicUsize::new(0));
                    let count = served.clone();
                    let service =
                        hyper::service::service_fn(move |req: http::Request<Incoming>| {
                            let (i, m, served) = (i.clone(), m.clone(), count.clone());
                            async move {
                                let now = i.fetch_add(1, Ordering::SeqCst) + 1;
                                m.fetch_max(now, Ordering::SeqCst);
                                tokio::time::sleep(delay).await;
                                i.fetch_sub(1, Ordering::SeqCst);
                                let last = served.fetch_add(1, Ordering::SeqCst) + 1
                                    >= requests_per_connection;
                                let mut resp = http::Response::new(Full::new(Bytes::from(
                                    format!("{:?} {}", req.version(), req.uri().path()),
                                )));
                                if last && !h2 {
                                    resp.headers_mut().insert(
                                        "connection",
                                        http::HeaderValue::from_static("close"),
                                    );
                                }
                                Ok::<_, std::convert::Infallible>(resp)
                            }
                        });
                    let io = TokioIo::new(tls);
                    if h2 {
                        let conn =
                            s2::Builder::new(TokioExecutor::new()).serve_connection(io, service);
                        let mut conn = std::pin::pin!(conn);
                        // Close the connection (GOAWAY) after its requests.
                        let mut closing = false;
                        loop {
                            let tick = tokio::time::sleep(Duration::from_millis(2));
                            tokio::select! {
                                _ = conn.as_mut() => return,
                                _ = tick => {}
                            }
                            if !closing && served.load(Ordering::SeqCst) >= requests_per_connection
                            {
                                closing = true;
                                conn.as_mut().graceful_shutdown();
                            }
                        }
                    } else {
                        let _ = s1::Builder::new().serve_connection(io, service).await;
                    }
                });
            }
        });
        Server {
            port,
            connections,
            max_in_flight,
            _rt: rt,
        }
    }

    /// A transport that trusts the test CA.
    pub(crate) fn client() -> Transport {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CA.to_vec().into()).unwrap();
        Transport::with_tls(tls_config_with(roots))
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::{client, server};
    use super::*;
    use std::sync::atomic::Ordering;

    /// GET `path`: the body and the protocol used.
    fn get(t: &Transport, port: u16, path: &str) -> (String, Protocol, bool) {
        let head = http::Request::get(format!("https://127.0.0.1:{port}{path}"))
            .body(())
            .unwrap()
            .into_parts()
            .0;
        let mut ex = Exchange::begin();
        let mut resp = t.send(head, None, false, &mut ex).unwrap();
        let body = String::from_utf8(resp.read(1 << 20).unwrap()).unwrap();
        (body, ex.protocol, ex.connect.is_some())
    }

    /// `n` requests from `n` threads at once.
    fn burst(t: &Transport, port: u16, n: usize) -> Vec<(String, Protocol, bool)> {
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..n)
                .map(|k| s.spawn(move || get(t, port, &format!("/{k}"))))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        })
    }

    #[test]
    fn http2_carries_a_burst_of_requests_on_one_connection() {
        let s = server(
            &[b"h2", b"http/1.1"],
            Duration::from_millis(300),
            usize::MAX,
        );
        let t = client();
        let started = Instant::now();
        let got = burst(&t, s.port, 20);
        let took = started.elapsed();
        for (k, (body, protocol, _)) in got.iter().enumerate() {
            assert_eq!(body, &format!("HTTP/2.0 /{k}"));
            assert_eq!(*protocol, Protocol::H2);
        }
        // The requests waited for the first TLS handshake and then shared
        // its connection, all at the same time.
        assert_eq!(s.connections.load(Ordering::SeqCst), 1);
        assert_eq!(
            got.iter().filter(|g| g.2).count(),
            1,
            "one exchange connected"
        );
        assert!(s.max_in_flight.load(Ordering::SeqCst) > MAX_PER_HOST);
        // 20 x 300 ms: 6 at a time would take 1.2 s or more.
        assert!(took < Duration::from_millis(1100), "{took:?}");
        // Later requests use it too.
        let (body, protocol, connected) = get(&t, s.port, "/again");
        assert_eq!(
            (body.as_str(), protocol, connected),
            ("HTTP/2.0 /again", Protocol::H2, false)
        );
        assert_eq!(s.connections.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn http1_servers_get_at_most_six_connections() {
        let s = server(&[b"http/1.1"], Duration::from_millis(100), usize::MAX);
        let t = client();
        let got = burst(&t, s.port, 20);
        for (k, (body, protocol, _)) in got.iter().enumerate() {
            assert_eq!(body, &format!("HTTP/1.1 /{k}"));
            assert_eq!(*protocol, Protocol::H1);
        }
        let opened = s.connections.load(Ordering::SeqCst);
        assert!((2..=MAX_PER_HOST).contains(&opened), "{opened} connections");
        assert!(s.max_in_flight.load(Ordering::SeqCst) <= MAX_PER_HOST);
        // Idle connections are used again.
        burst(&t, s.port, MAX_PER_HOST);
        assert_eq!(s.connections.load(Ordering::SeqCst), opened);
    }

    #[test]
    fn a_closed_http2_connection_is_replaced() {
        // The server closes each connection (GOAWAY) after one request.
        let s = server(&[b"h2"], Duration::ZERO, 1);
        let t = client();
        for k in 0..3 {
            let (body, protocol, connected) = get(&t, s.port, &format!("/{k}"));
            assert_eq!(body, format!("HTTP/2.0 /{k}"));
            assert_eq!(protocol, Protocol::H2);
            assert!(connected, "request {k} opened a new connection");
            // Let the GOAWAY and the close arrive.
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(s.connections.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_closed_http1_connection_is_replaced() {
        let s = server(&[b"http/1.1"], Duration::ZERO, 1);
        let t = client();
        for k in 0..3 {
            let (body, _, connected) = get(&t, s.port, &format!("/{k}"));
            assert_eq!(body, format!("HTTP/1.1 /{k}"));
            assert!(connected, "Connection: close was honoured");
        }
        assert_eq!(s.connections.load(Ordering::SeqCst), 3);
    }
}
