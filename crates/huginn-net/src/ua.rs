//! What Huginn says it is. One source for the `User-Agent` header,
//! `navigator.userAgent`, `navigator.userAgentData` and the User-Agent
//! client hints, so they always agree.
//!
//! The string has the tokens of Chrome's reduced user agent that sites
//! sniff for a modern engine (`Mozilla/5.0`, the platform, `AppleWebKit`,
//! `Safari`) and names Huginn, but does not claim `Chrome/`: over Huginn's
//! TLS stack a Chrome claim gets hard 403s from bot filters
//! (ARCHITECTURE.md, "User agent").

use url::{Host, Url};

/// The Chrome release whose V8 huginn-js embeds (the `v8` crate 152.x is
/// V8 15.2, Chrome 152). Bump it with the `v8` crate.
macro_rules! chrome_major {
    () => {
        "152"
    };
}

/// Version of the `Chromium` brand in the client hints and
/// `navigator.userAgentData`.
pub const CHROME_MAJOR: &str = chrome_major!();

/// Huginn's full version (`navigator.userAgentData` high-entropy values).
pub const HUGINN_FULL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The `User-Agent` header and `navigator.userAgent`.
#[cfg(target_os = "macos")]
pub const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Huginn/",
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR"),
    " Safari/537.36"
);

/// The `User-Agent` header and `navigator.userAgent`.
#[cfg(not(target_os = "macos"))]
pub const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Huginn/",
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR"),
    " Safari/537.36"
);

/// `Sec-CH-UA`: the brands of `navigator.userAgentData.brands`, with
/// Chrome's GREASE brand.
pub const SEC_CH_UA: &str = concat!(
    "\"Chromium\";v=\"",
    chrome_major!(),
    "\", \"Huginn\";v=\"",
    env!("CARGO_PKG_VERSION_MAJOR"),
    "\", \"Not_A Brand\";v=\"24\""
);

const CLIENT_HINTS: [(&str, &str); 3] = [
    ("Sec-CH-UA", SEC_CH_UA),
    ("Sec-CH-UA-Mobile", "?0"),
    (
        "Sec-CH-UA-Platform",
        if cfg!(target_os = "macos") {
            "\"macOS\""
        } else {
            "\"Linux\""
        },
    ),
];

/// The client hints Chrome sends on every request by default (the
/// low-entropy ones). Like Chrome, only to potentially trustworthy URLs:
/// `https:`, `wss:` and loopback hosts.
pub fn client_hint_headers(url: &Url) -> &'static [(&'static str, &'static str)] {
    if potentially_trustworthy(url) {
        &CLIENT_HINTS
    } else {
        &[]
    }
}

fn potentially_trustworthy(url: &Url) -> bool {
    match url.scheme() {
        "https" | "wss" => true,
        "http" | "ws" => match url.host() {
            Some(Host::Ipv4(a)) => a.is_loopback(),
            Some(Host::Ipv6(a)) => a.is_loopback(),
            Some(Host::Domain(d)) => d == "localhost" || d.ends_with(".localhost"),
            None => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_hints_only_go_to_trustworthy_urls() {
        let hints = |u: &str| client_hint_headers(&Url::parse(u).unwrap()).len();
        assert_eq!(hints("https://example.com/"), 3);
        assert_eq!(hints("http://127.0.0.1:8000/"), 3);
        assert_eq!(hints("http://[::1]/"), 3);
        assert_eq!(hints("http://localhost/"), 3);
        assert_eq!(hints("http://example.com/"), 0);
        assert_eq!(hints("http://localhost.example.com/"), 0);
    }
}
