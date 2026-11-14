//! HTTP/1.1 and HTTP/2 over TCP and TLS, for the loader.
//!
//! hyper speaks both protocols; this module owns the connections. TLS
//! offers `h2` and `http/1.1` (ALPN), like Chrome, and the server picks.
//! Plain `http:` is HTTP/1.1 (browsers do not speak cleartext HTTP/2).
//!
//! - **HTTP/2.** One connection per host carries every request to it at
//!   once. While the first TLS connection to a host is being opened, other
//!   requests to the host wait for it, as in Chrome: if the server picks
//!   `h2`, they share it; if it picks `http/1.1`, they open their own. A
//!   request on a connection that read nothing for 10 s sends a PING with
//!   it; without an answer in 10 s the connection is closed and the
//!   request goes again on a new one (Chrome's check of connections at
//!   risk of loss). Idle connections send nothing.
//! - **HTTP/1.1.** At most 6 connections per host (Chrome's limit), used one
//!   request at a time. A connection whose response was read to the end goes
//!   back to the pool. At most 32 idle connections are kept in all.
//! - Idle connections of both kinds are kept for 5 minutes, with their TLS
//!   sessions for resumption (one rustls config per network); HTTP/1.1
//!   connections that were opened ahead of a request
//!   ([`Transport::preconnect`]) and never used, for 10 s (Chrome's
//!   limits).
//! - Requests from public pages may not reach loopback or private
//!   addresses: they use only connections to public addresses, and open
//!   theirs with the others dropped. So a page's requests share the
//!   connection its navigation opened, as in Chrome.
//! - Name lookups are kept for a minute, and are only made for new
//!   connections. Connecting tries the addresses in the order the resolver
//!   gave; the other address family starts after 300 ms (Happy Eyeballs,
//!   Chrome's delay).
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
use std::task::Poll;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body as _, Incoming};
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
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
/// How long a connection opened ahead of a request waits for one:
/// Chrome's limit for sockets that were never used.
const UNUSED_AGE: Duration = Duration::from_secs(10);
/// How long a resolved address is used: Chrome's default for answers of
/// the system resolver.
const DNS_TTL: Duration = Duration::from_secs(60);
/// Time limit for one exchange: connecting, the response head and the body.
const TIMEOUT: Duration = Duration::from_secs(30);
/// An HTTP/2 connection that read nothing for this long is checked with a
/// PING when it carries a request, and closed when the answer takes as
/// long again (Chrome: connection at risk of loss, hung interval).
const H2_PING_AFTER: Duration = Duration::from_secs(10);
const H2_PING_TIMEOUT: Duration = Duration::from_secs(10);
/// When the other address family starts connecting (Chrome's Happy
/// Eyeballs delay).
const FALLBACK_DELAY: Duration = Duration::from_millis(300);

type ReqBody = Full<Bytes>;

/// The connections of one network, and how to make new ones.
pub(crate) struct Transport {
    /// Made on the first request, so loaders that only read `file:` and
    /// `data:` URLs start no thread.
    runtime: LazyLock<tokio::runtime::Runtime>,
    dial: Dial,
}

/// The pool and what opens connections; cheap to clone (a preconnect
/// thread takes one).
#[derive(Clone)]
struct Dial {
    /// One TLS config, so sessions are resumed across connections.
    tls: Arc<rustls::ClientConfig>,
    pool: Arc<Pool>,
    dns: Arc<Mutex<HashMap<(String, u16), (Vec<SocketAddr>, Instant)>>>,
    loopback_only: bool,
}

/// Where a connection goes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    https: bool,
    host: String,
    port: u16,
}

#[derive(Default)]
struct Pool {
    hosts: Mutex<HashMap<Key, Host>>,
    /// Woken when a connection becomes free, is made, or fails.
    changed: Notify,
}

#[derive(Default)]
struct Host {
    /// The host's HTTP/2 connection.
    h2: Option<H2>,
    /// What the last TLS connection to the host negotiated (`None`: none
    /// was made yet).
    speaks_h2: Option<bool>,
    /// Idle HTTP/1.1 connections.
    idle: Vec<Idle>,
    /// HTTP/1.1 connections in use, and connections being opened.
    busy: usize,
    /// Connections being opened.
    connecting: usize,
}

struct H2 {
    sender: http2::SendRequest<ReqBody>,
    /// When it was last used (or made).
    used: Instant,
    /// It goes to a loopback or private address: requests from public pages
    /// may not use it.
    local: bool,
}

struct Idle {
    sender: http1::SendRequest<ReqBody>,
    since: Instant,
    local: bool,
    /// It carried a request (else it was opened ahead of one).
    used: bool,
}

impl Idle {
    fn alive(&self) -> bool {
        let age = if self.used { IDLE_AGE } else { UNUSED_AGE };
        !self.sender.is_closed() && self.since.elapsed() < age
    }
}

impl H2 {
    fn alive(&self) -> bool {
        !self.sender.is_closed() && self.used.elapsed() < IDLE_AGE
    }
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
    local: bool,
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
                h.idle.push(Idle {
                    sender: s,
                    since: Instant::now(),
                    local: self.local,
                    used: true,
                });
            }
        }
        trim_idle(&mut hosts);
        drop(hosts);
        self.pool.changed.notify_waiters();
    }
}

/// Keep at most IDLE_TOTAL idle connections: drop the oldest.
fn trim_idle(hosts: &mut HashMap<Key, Host>) {
    let idle: usize = hosts.values().map(|h| h.idle.len()).sum();
    for _ in IDLE_TOTAL..idle {
        let oldest = hosts
            .iter()
            .filter_map(|(k, h)| h.idle.first().map(|i| (i.since, k.clone())))
            .min_by_key(|(at, _)| *at);
        if let Some(h) = oldest.and_then(|(_, k)| hosts.get_mut(&k)) {
            h.idle.remove(0);
        }
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
            dial: Dial {
                tls: tls_config(),
                pool: Arc::default(),
                dns: Arc::default(),
                loopback_only,
            },
        }
    }

    /// A transport that trusts `tls`'s roots (tests).
    #[cfg(test)]
    pub(crate) fn with_tls(tls: Arc<rustls::ClientConfig>) -> Transport {
        let mut t = Transport::new(false);
        t.dial.tls = tls;
        t
    }

    fn handle(&self) -> &tokio::runtime::Handle {
        self.runtime.handle()
    }

    /// Send `head` (with `body`) and wait for the response head. A request
    /// that failed on a kept-alive connection (the server closed it in the
    /// meantime, or an HTTP/2 connection went away) is sent once more, on
    /// a new connection, as in Chrome. `public_only`: the request may not
    /// reach loopback or private addresses.
    pub(crate) fn send(
        &self,
        head: Head,
        body: Option<&[u8]>,
        public_only: bool,
        exchange: &mut Exchange,
    ) -> Result<Response, NetError> {
        let url = head.uri.to_string();
        let key =
            key_for(&head.uri).ok_or_else(|| NetError::InvalidUrl(format!("{url}: no host")))?;
        let body = body.map(Bytes::copy_from_slice).unwrap_or_default();
        let handle = self.handle().clone();
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        let dial = &self.dial;
        let result = handle.block_on(async {
            let send = async {
                match dial
                    .send_once(&key, public_only, &head, &body, false, exchange)
                    .await
                {
                    Err((_, true)) => {
                        *exchange = Exchange::begin();
                        dial.send_once(&key, public_only, &head, &body, true, exchange)
                            .await
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

    /// Open a connection to `uri`'s host ahead of a request, if it has no
    /// usable one and none is being opened: name lookup, TCP, TLS and the
    /// HTTP/2 handshake, nothing sent beyond that (Chrome's preconnect).
    /// Returns at once; the work runs on a thread of its own, and requests
    /// that come meanwhile wait for the connection like for any other one
    /// being opened. With `HUGINN_NET_LOG=1` it prints a `preconnect` line.
    pub(crate) fn preconnect(&self, uri: &http::Uri, public_only: bool) {
        let Some(key) = key_for(uri) else {
            return;
        };
        {
            let mut hosts = self.dial.pool.hosts.lock();
            let h = hosts.entry(key.clone()).or_default();
            h.idle.retain(Idle::alive);
            if h.h2.as_ref().is_some_and(|c| !c.alive()) {
                h.h2 = None;
            }
            let usable = |local: bool| !public_only || !local;
            let has = h.h2.as_ref().is_some_and(|c| usable(c.local))
                || h.idle.iter().any(|i| usable(i.local));
            if has || h.connecting > 0 || h.busy >= MAX_PER_HOST {
                return;
            }
            h.busy += 1;
            h.connecting += 1;
            sweep(&mut hosts);
        }
        let (dial, handle, k) = (self.dial.clone(), self.handle().clone(), key.clone());
        let opened = std::thread::Builder::new()
            .name("huginn-preconnect".into())
            .spawn(move || {
                handle.block_on(async {
                    let _ = tokio::time::timeout(TIMEOUT, dial.open_idle(&k, public_only)).await;
                });
            });
        if opened.is_err() {
            // No thread: undo the count.
            drop(Opening {
                pool: &self.dial.pool,
                key: &key,
                leased: false,
            });
        }
    }
}

impl Dial {
    /// One try. The error says whether the connection was a kept-alive one
    /// (then a retry on a new connection may work).
    async fn send_once(
        &self,
        key: &Key,
        public_only: bool,
        head: &Head,
        body: &Bytes,
        fresh: bool,
        exchange: &mut Exchange,
    ) -> Result<(http::Response<Incoming>, Option<Lease>), (String, bool)> {
        loop {
            let (conn, reused) = self
                .connection(key, public_only, fresh, exchange)
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
    /// HTTP/1.1 one, or a new one. `fresh`: only a new one. `public_only`:
    /// only one to a public address. Returns whether it was kept alive from
    /// before.
    async fn connection(
        &self,
        key: &Key,
        public_only: bool,
        fresh: bool,
        exchange: &mut Exchange,
    ) -> Result<(Conn, bool), String> {
        let usable = |local: bool| !public_only || !local;
        loop {
            let mut notified = pin!(self.pool.changed.notified());
            {
                let mut hosts = self.pool.hosts.lock();
                let h = hosts.entry(key.clone()).or_default();
                h.idle.retain(Idle::alive);
                if let Some(c) = &mut h.h2 {
                    if !c.alive() {
                        h.h2 = None;
                    } else if !fresh && usable(c.local) {
                        c.used = Instant::now();
                        return Ok((Conn::H2(c.sender.clone()), true));
                    }
                }
                // A TLS connection is being opened and may become an
                // HTTP/2 connection for everyone: wait for it.
                let wait_for_alpn =
                    !fresh && key.https && h.speaks_h2 != Some(false) && h.connecting > 0;
                if !wait_for_alpn {
                    if !fresh && let Some(i) = h.idle.iter().rposition(|i| usable(i.local)) {
                        let idle = h.idle.remove(i);
                        h.busy += 1;
                        return Ok((Conn::H1(self.lease(key, idle.sender, idle.local)), true));
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
        let made = self.connect(key, public_only, exchange).await;
        exchange.connect = Some(t.elapsed().saturating_sub(exchange.dns));
        let made = made?;
        let mut hosts = self.pool.hosts.lock();
        let h = hosts.entry(key.clone()).or_default();
        match made {
            Made::H2(sender, local) => {
                h.speaks_h2 = Some(true);
                h.h2 = Some(H2 {
                    sender: sender.clone(),
                    used: Instant::now(),
                    local,
                });
                drop(hosts);
                drop(opening);
                Ok((Conn::H2(sender), false))
            }
            Made::H1(sender, local) => {
                if key.https {
                    h.speaks_h2 = Some(false);
                }
                drop(hosts);
                opening.leased = true;
                drop(opening);
                Ok((Conn::H1(self.lease(key, sender, local)), false))
            }
        }
    }

    /// Open a connection that waits in the pool for a request
    /// ([`Transport::preconnect`], which counted it as being opened).
    async fn open_idle(&self, key: &Key, public_only: bool) {
        let opening = Opening {
            pool: &self.pool,
            key,
            leased: false,
        };
        let mut exchange = Exchange::begin();
        let t = Instant::now();
        let made = self.connect(key, public_only, &mut exchange).await;
        let connect = t.elapsed().saturating_sub(exchange.dns);
        let mut hosts = self.pool.hosts.lock();
        let h = hosts.entry(key.clone()).or_default();
        let protocol = match made {
            Ok(Made::H2(sender, local)) => {
                h.speaks_h2 = Some(true);
                if h.h2.as_ref().is_none_or(|c| !c.alive()) {
                    h.h2 = Some(H2 {
                        sender,
                        used: Instant::now(),
                        local,
                    });
                }
                "h2"
            }
            Ok(Made::H1(sender, local)) => {
                if key.https {
                    h.speaks_h2 = Some(false);
                }
                h.idle.push(Idle {
                    sender,
                    since: Instant::now(),
                    local,
                    used: false,
                });
                trim_idle(&mut hosts);
                "h1"
            }
            Err(_) => "failed",
        };
        drop(hosts);
        drop(opening);
        exchange.preconnected(protocol, connect, key.https, &key.host, key.port);
    }

    fn lease(&self, key: &Key, sender: http1::SendRequest<ReqBody>, local: bool) -> Lease {
        Lease {
            pool: self.pool.clone(),
            key: key.clone(),
            sender: Some(sender),
            local,
            reusable: false,
        }
    }

    /// Open a connection: name lookup, TCP, then TLS for `https:`. Also
    /// whether it goes to a loopback or private address.
    async fn connect(
        &self,
        key: &Key,
        public_only: bool,
        exchange: &mut Exchange,
    ) -> Result<Made, String> {
        let addrs = self.resolve(key, public_only, exchange)?;
        let tcp = connect_tcp(&addrs).await?;
        let local = tcp.peer_addr().map_or(true, |a| crate::is_local_ip(a.ip()));
        let _ = tcp.set_nodelay(true);
        if !key.https {
            return h1(tcp, local).await;
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
                .max_header_list_size(256 << 10)
                // PING a connection that was quiet for a while when it
                // carries a request; never while it is idle.
                .timer(TokioTimer::new())
                .keep_alive_interval(H2_PING_AFTER)
                .keep_alive_timeout(H2_PING_TIMEOUT)
                .keep_alive_while_idle(false);
            let (sender, conn) = b
                .handshake(TokioIo::new(tls))
                .await
                .map_err(|e| e.to_string())?;
            tokio::spawn(conn);
            Ok(Made::H2(sender, local))
        } else {
            h1(tls, local).await
        }
    }

    /// The addresses of `key`'s host. `localhost` and `*.localhost` always
    /// go to loopback (127.0.0.1, then ::1), never to DNS, like Chrome: the
    /// cookie jar treats them as secure. Loopback-only networks find no
    /// other host (no lookup). For public pages, loopback and private
    /// addresses are dropped (Local Network Access); the check is on the
    /// resolved addresses, so DNS rebinding cannot get around it.
    fn resolve(
        &self,
        key: &Key,
        public_only: bool,
        exchange: &mut Exchange,
    ) -> Result<Vec<SocketAddr>, String> {
        let (host, port) = (key.host.as_str(), key.port);
        if host == "localhost" || host.ends_with(".localhost") {
            if public_only {
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
            .filter(|a| !public_only || !crate::is_local_ip(a.ip()))
            .collect();
        if addrs.is_empty() {
            return Err("host not found".into());
        }
        Ok(addrs)
    }
}

/// Connect to the first of `addrs` that answers. The addresses of the
/// first one's family are tried in order; the other family's start
/// `FALLBACK_DELAY` later, or at once when the first family has failed
/// (Happy Eyeballs, RFC 8305, with Chrome's delay).
async fn connect_tcp(addrs: &[SocketAddr]) -> Result<tokio::net::TcpStream, String> {
    let Some(first) = addrs.first() else {
        return Err("no address".into());
    };
    let (primary, secondary): (Vec<SocketAddr>, Vec<SocketAddr>) =
        addrs.iter().partition(|a| a.is_ipv6() == first.is_ipv6());
    let in_order = |list: Vec<SocketAddr>| async move {
        let mut last = String::from("no address");
        for a in list {
            match tokio::net::TcpStream::connect(a).await {
                Ok(s) => return Ok(s),
                Err(e) => last = format!("{a}: {e}"),
            }
        }
        Err(last)
    };
    if secondary.is_empty() {
        return in_order(primary).await;
    }
    let mut first = pin!(in_order(primary));
    let mut other = pin!(in_order(secondary));
    let mut delay = pin!(tokio::time::sleep(FALLBACK_DELAY));
    let (mut first_err, mut other_err, mut other_started) = (None, None, false);
    std::future::poll_fn(|cx| {
        if first_err.is_none()
            && let Poll::Ready(r) = first.as_mut().poll(cx)
        {
            match r {
                Ok(s) => return Poll::Ready(Ok(s)),
                Err(e) => first_err = Some(e),
            }
        }
        if !other_started && (first_err.is_some() || delay.as_mut().poll(cx).is_ready()) {
            other_started = true;
        }
        if other_started
            && other_err.is_none()
            && let Poll::Ready(r) = other.as_mut().poll(cx)
        {
            match r {
                Ok(s) => return Poll::Ready(Ok(s)),
                Err(e) => other_err = Some(e),
            }
        }
        match (&first_err, &other_err) {
            (Some(a), Some(b)) => Poll::Ready(Err(format!("{a}; {b}"))),
            _ => Poll::Pending,
        }
    })
    .await
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
        h.idle.retain(Idle::alive);
        if h.h2.as_ref().is_some_and(|c| !c.alive()) {
            h.h2 = None;
        }
        h.h2.is_some() || !h.idle.is_empty() || h.busy > 0 || h.connecting > 0
    });
}

/// A new connection, and whether it goes to a loopback or private address.
enum Made {
    H1(http1::SendRequest<ReqBody>, bool),
    H2(http2::SendRequest<ReqBody>, bool),
}

/// An HTTP/1.1 connection on `io`; its driver runs on the network thread.
async fn h1<T>(io: T, local: bool) -> Result<Made, String>
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
    Ok(Made::H1(sender, local))
}

/// The pool key of a URI.
fn key_for(uri: &http::Uri) -> Option<Key> {
    let https = uri.scheme_str() == Some("https");
    let host = uri
        .host()?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
    Some(Key { https, host, port })
}

/// The request for HTTP/1.1 or (`h2`) HTTP/2. HTTP/1.1 gets the path as its target and
/// a `Host` header; HTTP/2 keeps the absolute URI (`:scheme`,
/// `:authority`), without user info (RFC 9113, 8.3.1). Both get the user
/// agent and `Accept-Encoding` unless the request has its own.
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
        without_user_info(&head.uri)
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

/// `uri` without `user:password@` in its authority.
fn without_user_info(uri: &http::Uri) -> http::Uri {
    let Some((_, host)) = uri.authority().and_then(|a| a.as_str().rsplit_once('@')) else {
        return uri.clone();
    };
    let mut parts = uri.clone().into_parts();
    parts.authority = host.parse().ok();
    http::Uri::from_parts(parts).unwrap_or_else(|_| uri.clone())
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
    /// after `delay` with its HTTP version and path (and the authority it
    /// got in `x-authority`), closes a connection after
    /// `requests_per_connection` requests, and counts connections and
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
                                // `:authority`, or `Host` on HTTP/1.1.
                                let authority = req
                                    .uri()
                                    .authority()
                                    .map(|a| a.as_str().to_owned())
                                    .or_else(|| {
                                        let host = req.headers().get("host")?;
                                        Some(host.to_str().ok()?.to_owned())
                                    })
                                    .unwrap_or_default();
                                if let Ok(v) = http::HeaderValue::from_str(&authority) {
                                    resp.headers_mut().insert("x-authority", v);
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

    #[test]
    fn user_info_is_never_sent_as_the_authority() {
        for alpn in [&b"h2"[..], &b"http/1.1"[..]] {
            let s = server(&[alpn], Duration::ZERO, usize::MAX);
            let t = client();
            let head = http::Request::get(format!("https://user:secret@127.0.0.1:{}/a", s.port))
                .body(())
                .unwrap()
                .into_parts()
                .0;
            let mut resp = t.send(head, None, false, &mut Exchange::begin()).unwrap();
            let body = String::from_utf8(resp.read(1 << 20).unwrap()).unwrap();
            assert!(body.ends_with(" /a"), "{body}");
            assert_eq!(
                resp.headers.get("x-authority").unwrap(),
                &format!("127.0.0.1:{}", s.port),
                "{}",
                String::from_utf8_lossy(alpn)
            );
        }
    }

    /// Wait until the server has `n` connections (a preconnect runs on a
    /// thread of its own).
    fn wait_for_connections(s: &test_server::Server, n: usize) {
        let t = Instant::now();
        while s.connections.load(Ordering::SeqCst) < n && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        // The handshake may still be finishing on the client.
        std::thread::sleep(Duration::from_millis(100));
    }

    #[test]
    fn a_preconnected_host_needs_no_new_handshake() {
        for alpn in [&b"h2"[..], &b"http/1.1"[..]] {
            let s = server(&[alpn], Duration::ZERO, usize::MAX);
            let t = client();
            let uri: http::Uri = format!("https://127.0.0.1:{}/", s.port).parse().unwrap();
            t.preconnect(&uri, false);
            // A second call while the first connects, and one after it, open nothing.
            t.preconnect(&uri, false);
            wait_for_connections(&s, 1);
            t.preconnect(&uri, false);
            let (body, _, connected) = get(&t, s.port, "/a");
            assert!(body.ends_with(" /a"), "{body}");
            assert!(
                !connected,
                "{}: the request used the preconnected connection",
                String::from_utf8_lossy(alpn)
            );
            assert_eq!(s.connections.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn requests_of_public_pages_do_not_use_connections_to_local_addresses() {
        let s = server(&[b"h2"], Duration::ZERO, usize::MAX);
        let t = client();
        let send = |public_only: bool| {
            let head = http::Request::get(format!("https://127.0.0.1:{}/x", s.port))
                .body(())
                .unwrap()
                .into_parts()
                .0;
            let mut ex = Exchange::begin();
            t.send(head, None, public_only, &mut ex)
                .map(|_| ex.connect.is_some())
        };
        assert_eq!(send(false).unwrap(), true, "the first request connects");
        // 127.0.0.1 is loopback: not reachable from a public page, neither
        // through the open connection nor a new one.
        let err = send(true).unwrap_err().to_string();
        assert!(err.contains("host not found"), "{err}");
        assert_eq!(send(false).unwrap(), false, "the connection is reused");
        assert_eq!(s.connections.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_other_address_family_connects_when_the_first_does_not_answer() {
        // An IPv6 listener; the IPv4 address first goes nowhere (TEST-NET-1:
        // the connect hangs or fails), so the IPv6 one starts after the
        // fallback delay, or at once.
        let Ok(listener) = std::net::TcpListener::bind("[::1]:0") else {
            return; // No IPv6 loopback here.
        };
        let v6 = listener.local_addr().unwrap();
        let addrs = ["192.0.2.1:9".parse().unwrap(), v6];
        let rt = runtime();
        let started = Instant::now();
        let tcp = rt
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), connect_tcp(&addrs)).await
            })
            .expect("not timed out")
            .expect("connected");
        assert_eq!(tcp.peer_addr().unwrap(), v6);
        assert!(started.elapsed() < FALLBACK_DELAY + Duration::from_secs(1));
    }
}
