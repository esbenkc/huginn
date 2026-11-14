//! Where the time of each HTTP exchange goes. `HUGINN_NET_LOG=1` prints one
//! line per exchange to stderr:
//!
//! ```text
//! huginn: net: +412.3 GET 200 network new dns=1.1 connect=48.2 head=95.0 body=3.4 bytes=10521 style https://example.com/a.css
//! ```
//!
//! `+412.3`: start, ms after the first exchange of the process. `network`,
//! `hit` (from the HTTP cache, no request) or `revalidated` (the server
//! answered 304). `new` or `reused`: whether the exchange opened a
//! connection. `dns`: name lookup; `connect`: TCP and TLS handshakes (new
//! connections only); `head`: request sent until the response head arrived
//! (includes `dns` and `connect`); `body`: reading and decompressing the
//! body. All times in ms.

use std::cell::Cell;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use ureq::unversioned::transport::{ConnectionDetails, Connector, DefaultConnector, Transport};

thread_local! {
    /// Name lookup time of the current exchange on this thread.
    static DNS: Cell<Duration> = const { Cell::new(Duration::ZERO) };
    /// Connect time of the current exchange, if it opened a connection.
    static CONNECT: Cell<Option<Duration>> = const { Cell::new(None) };
}

/// Whether `HUGINN_NET_LOG=1`.
static ENABLED: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("HUGINN_NET_LOG").is_some_and(|v| v == "1"));

/// Start of the log's clock: the first exchange of the process.
static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);

/// One exchange on this thread, from just before the request is sent.
pub(crate) struct Exchange {
    start: Instant,
    head: Duration,
}

impl Exchange {
    pub(crate) fn begin() -> Exchange {
        DNS.set(Duration::ZERO);
        CONNECT.set(None);
        if *ENABLED {
            LazyLock::force(&ORIGIN);
        }
        Exchange {
            start: Instant::now(),
            head: Duration::ZERO,
        }
    }

    /// The response head arrived.
    pub(crate) fn head(&mut self) {
        self.head = self.start.elapsed();
    }

    /// Whether the exchange opened a new connection (so far).
    pub(crate) fn connected(&self) -> bool {
        CONNECT.get().is_some()
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
            "huginn: net: +{:.1} {method} {status} {how} {} dns={:.1} connect={:.1} head={:.1} body={:.1} bytes={bytes} {dest} {url}",
            ms(self.start.duration_since(*ORIGIN)),
            if self.connected() { "new" } else { "reused" },
            ms(DNS.get()),
            CONNECT.get().map_or(0.0, ms),
            ms(self.head),
            ms(body),
        );
    }
}

/// Add a name lookup's time to the current exchange.
pub(crate) fn dns(d: Duration) {
    DNS.set(DNS.get() + d);
}

/// ureq's connector chain (TCP, then TLS), timed: a new connection records
/// its connect time in the current exchange.
#[derive(Debug, Default)]
pub(crate) struct TimedConnector(DefaultConnector);

impl Connector<()> for TimedConnector {
    type Out = Box<dyn Transport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<()>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        let t = Instant::now();
        let out = self.0.connect(details, chained);
        CONNECT.set(Some(CONNECT.get().unwrap_or_default() + t.elapsed()));
        out
    }
}
