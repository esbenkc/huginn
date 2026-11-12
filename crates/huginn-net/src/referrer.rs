//! Referrer Policy (W3C): what `Referer` a request carries and what
//! `document.referrer` shows. The default is `strict-origin-when-cross-origin`,
//! like every current browser.

use url::Url;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ReferrerPolicy {
    NoReferrer,
    NoReferrerWhenDowngrade,
    SameOrigin,
    Origin,
    StrictOrigin,
    OriginWhenCrossOrigin,
    #[default]
    StrictOriginWhenCrossOrigin,
    UnsafeUrl,
}

impl ReferrerPolicy {
    /// One policy token (`Referrer-Policy` value, `referrerpolicy` attribute,
    /// `<meta name=referrer>` content including its legacy keywords).
    pub fn parse_token(token: &str) -> Option<ReferrerPolicy> {
        use ReferrerPolicy::*;
        Some(match token.trim().to_ascii_lowercase().as_str() {
            "no-referrer" | "never" => NoReferrer,
            "no-referrer-when-downgrade" => NoReferrerWhenDowngrade,
            "same-origin" => SameOrigin,
            "origin" => Origin,
            "strict-origin" => StrictOrigin,
            "origin-when-cross-origin" | "origin-when-crossorigin" => OriginWhenCrossOrigin,
            "strict-origin-when-cross-origin" | "default" => StrictOriginWhenCrossOrigin,
            "unsafe-url" | "always" => UnsafeUrl,
            _ => return None,
        })
    }

    /// A `Referrer-Policy` header: the last token this browser knows wins.
    pub fn parse_header(value: &str) -> Option<ReferrerPolicy> {
        value.split(',').filter_map(Self::parse_token).next_back()
    }
}

/// A URL is "potentially trustworthy" for downgrade checks: https, wss, or a loopback host.
fn trustworthy(url: &Url) -> bool {
    matches!(url.scheme(), "https" | "wss") || crate::cookies::is_loopback(url)
}

/// The `Referer` value a request from a document at `source` to `target`
/// carries under `policy`, or `None`. Local documents (`file:`, `data:`,
/// `about:`, `blob:`) never send one.
pub fn referrer_for(source: &Url, policy: ReferrerPolicy, target: &Url) -> Option<String> {
    use ReferrerPolicy::*;
    if !matches!(source.scheme(), "http" | "https") {
        return None;
    }
    let full = || {
        let mut u = source.clone();
        let _ = u.set_username("");
        let _ = u.set_password(None);
        u.set_fragment(None);
        u.to_string()
    };
    let origin = || format!("{}/", source.origin().ascii_serialization());
    let same_origin = source.origin() == target.origin();
    let downgrade = trustworthy(source) && !trustworthy(target);
    let r = match policy {
        NoReferrer => return None,
        UnsafeUrl => full(),
        Origin => origin(),
        SameOrigin => {
            if !same_origin {
                return None;
            }
            full()
        }
        StrictOrigin => {
            if downgrade {
                return None;
            }
            origin()
        }
        NoReferrerWhenDowngrade => {
            if downgrade {
                return None;
            }
            full()
        }
        OriginWhenCrossOrigin => {
            if same_origin {
                full()
            } else {
                origin()
            }
        }
        StrictOriginWhenCrossOrigin => {
            if same_origin {
                full()
            } else if downgrade {
                return None;
            } else {
                origin()
            }
        }
    };
    // Very long referrers are cut to the origin (Chrome: 4 KiB).
    Some(if r.len() > 4096 { origin() } else { r })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn default_policy_trims_cross_origin_and_drops_downgrades() {
        let p = ReferrerPolicy::default();
        let src = u("https://user:pw@www.bbc.com/news/a?q=1#frag");
        assert_eq!(
            referrer_for(&src, p, &u("https://www.bbc.com/x")).as_deref(),
            Some("https://www.bbc.com/news/a?q=1")
        );
        assert_eq!(
            referrer_for(&src, p, &u("https://www.bbc.co.uk/")).as_deref(),
            Some("https://www.bbc.com/")
        );
        assert_eq!(referrer_for(&src, p, &u("http://example.com/")), None);
        assert_eq!(
            referrer_for(
                &u("file:///Users/me/a.html"),
                ReferrerPolicy::UnsafeUrl,
                &u("https://a.com/")
            ),
            None
        );
        assert_eq!(
            referrer_for(&src, ReferrerPolicy::NoReferrer, &u("https://www.bbc.com/")),
            None
        );
        assert_eq!(
            referrer_for(&src, ReferrerPolicy::SameOrigin, &u("https://other.com/")),
            None
        );
    }

    #[test]
    fn parses_header_and_legacy_tokens() {
        assert_eq!(
            ReferrerPolicy::parse_header("no-referrer, bogus, unsafe-url, nonsense"),
            Some(ReferrerPolicy::UnsafeUrl)
        );
        assert_eq!(
            ReferrerPolicy::parse_token("never"),
            Some(ReferrerPolicy::NoReferrer)
        );
        assert_eq!(ReferrerPolicy::parse_header("x"), None);
    }
}
