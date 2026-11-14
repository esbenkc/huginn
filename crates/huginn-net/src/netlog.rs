//! Where the time of each HTTP exchange goes. `HUGINN_NET_LOG=1` prints one
//! line per exchange to stderr:
//!
//! ```text
//! huginn: net: +412.3 GET 200 network new h2 dns=1.1 connect=48.2 head=95.0 body=3.4 bytes=10521 style https://example.com/a.css
//! ```
//!
//! `+412.3`: start, ms after the first exchange (or phase mark) of the
//! process. `network`, `hit` (from the HTTP cache, no request) or
//! `revalidated` (the server answered 304). `new` or `reused`: whether the
//! exchange opened a connection. `h1` or `h2`: HTTP/1.1 or HTTP/2 (`-`: no
//! request, a cache hit). `dns`: name lookup; `connect`: TCP and TLS
//! handshakes (new connections only); `head`: request sent until the
//! response head arrived (includes `dns` and `connect`); `body`: reading
//! and decompressing the body. All times in ms.
//!
//! The shell marks the phases of a page load on the same clock
//! ([`log_phase`]):
//!
//! ```text
//! huginn: load: +530.1 first paint
//! ```

use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Whether `HUGINN_NET_LOG=1`.
static ENABLED: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("HUGINN_NET_LOG").is_some_and(|v| v == "1"));

/// Start of the log's clock: the first exchange of the process.
static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Print `huginn: load: +12.3 <what>` on the log's clock (only with
/// `HUGINN_NET_LOG=1`): a phase of a page load, to read the exchanges
/// against.
pub fn log_phase(what: &str) {
    if *ENABLED {
        eprintln!(
            "huginn: load: +{:.1} {what}",
            ORIGIN.elapsed().as_secs_f64() * 1e3
        );
    }
}

/// The HTTP version of an exchange.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Protocol {
    /// No request was sent (a cache hit).
    #[default]
    None,
    H1,
    H2,
}

/// One exchange, from just before the request is sent.
pub(crate) struct Exchange {
    start: Instant,
    head: Duration,
    /// Name lookup time.
    pub(crate) dns: Duration,
    /// Connect time (TCP and TLS), if the exchange opened a connection.
    pub(crate) connect: Option<Duration>,
    pub(crate) protocol: Protocol,
}

impl Exchange {
    pub(crate) fn begin() -> Exchange {
        if *ENABLED {
            LazyLock::force(&ORIGIN);
        }
        Exchange {
            start: Instant::now(),
            head: Duration::ZERO,
            dns: Duration::ZERO,
            connect: None,
            protocol: Protocol::None,
        }
    }

    /// The response head arrived.
    pub(crate) fn head(&mut self) {
        self.head = self.start.elapsed();
    }

    /// Print the line (only with `HUGINN_NET_LOG=1`). `dest`: the
    /// request's destination (`document`, `style`, `image`, ...).
    pub(crate) fn end(
        &self,
        method: &str,
        status: u16,
        how: &str,
        bytes: usize,
        dest: &str,
        url: &str,
    ) {
        if !*ENABLED {
            return;
        }
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        let body = if self.head.is_zero() {
            Duration::ZERO
        } else {
            self.start.elapsed().saturating_sub(self.head)
        };
        eprintln!(
            "huginn: net: +{:.1} {method} {status} {how} {} {} dns={:.1} connect={:.1} head={:.1} body={:.1} bytes={bytes} {dest} {url}",
            ms(self.start.duration_since(*ORIGIN)),
            if self.connect.is_some() {
                "new"
            } else {
                "reused"
            },
            match self.protocol {
                Protocol::None => "-",
                Protocol::H1 => "h1",
                Protocol::H2 => "h2",
            },
            ms(self.dns),
            self.connect.map_or(0.0, ms),
            ms(self.head),
            ms(body),
        );
    }

    /// Print the line of a connection opened ahead of a request (only with
    /// `HUGINN_NET_LOG=1`): `huginn: preconnect: +12.3 h2 dns=1.0
    /// connect=30.2 https://example.com:443`. `protocol` is `h1`, `h2` or
    /// `failed`.
    pub(crate) fn preconnected(
        &self,
        protocol: &str,
        connect: Duration,
        https: bool,
        host: &str,
        port: u16,
    ) {
        if !*ENABLED {
            return;
        }
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        eprintln!(
            "huginn: preconnect: +{:.1} {protocol} dns={:.1} connect={:.1} {}://{host}:{port}",
            ms(self.start.duration_since(*ORIGIN)),
            ms(self.dns),
            ms(connect),
            if https { "https" } else { "http" },
        );
    }
}
