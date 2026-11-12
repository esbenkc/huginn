//! CORS (Fetch standard): which requests need a preflight, the CORS check
//! on responses, preflight validation, and which response headers a script
//! may read. Header names are lowercase throughout.

/// `GET`, `HEAD` and `POST` need no preflight.
pub fn is_safelisted_method(method: &str) -> bool {
    matches!(method, "GET" | "HEAD" | "POST")
}

/// `CONNECT`, `TRACE` and `TRACK` may not be used by scripts.
pub fn is_forbidden_method(method: &str) -> bool {
    ["CONNECT", "TRACE", "TRACK"]
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method))
}

/// Uppercase the standard methods (`post` -> `POST`); others stay as written.
pub fn normalize_method(method: &str) -> String {
    for m in ["DELETE", "GET", "HEAD", "OPTIONS", "POST", "PUT"] {
        if m.eq_ignore_ascii_case(method) {
            return m.to_owned();
        }
    }
    method.to_owned()
}

/// Headers a script may not set: the browser controls them. Chrome also
/// keeps `User-Agent`.
pub fn is_forbidden_request_header(name: &str, value: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if n.starts_with("proxy-") || n.starts_with("sec-") {
        return true;
    }
    if matches!(
        n.as_str(),
        "x-http-method" | "x-http-method-override" | "x-method-override"
    ) {
        return value.split(',').any(|m| is_forbidden_method(m.trim()));
    }
    matches!(
        n.as_str(),
        "accept-charset"
            | "accept-encoding"
            | "access-control-request-headers"
            | "access-control-request-method"
            | "connection"
            | "content-length"
            | "cookie"
            | "cookie2"
            | "date"
            | "dnt"
            | "expect"
            | "host"
            | "keep-alive"
            | "origin"
            | "referer"
            | "set-cookie"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "user-agent"
            | "via"
    )
}

fn has_unsafe_byte(v: &str) -> bool {
    v.bytes().any(|b| {
        (b < 0x20 && b != b'\t')
            || b == 0x7f
            || matches!(
                b,
                b'"' | b'('
                    | b')'
                    | b':'
                    | b'<'
                    | b'>'
                    | b'?'
                    | b'@'
                    | b'['
                    | b'\\'
                    | b']'
                    | b'{'
                    | b'}'
            )
    })
}

/// Fetch's "CORS-safelisted request-header".
pub fn is_safelisted_request_header(name: &str, value: &str) -> bool {
    if value.len() > 128 {
        return false;
    }
    match name.to_ascii_lowercase().as_str() {
        "accept" => !has_unsafe_byte(value),
        "accept-language" | "content-language" => value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b" *,-.;=".contains(&b)),
        "content-type" => {
            if has_unsafe_byte(value) {
                return false;
            }
            let essence = value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            matches!(
                essence.as_str(),
                "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain"
            )
        }
        "range" => {
            let Some(r) = value.strip_prefix("bytes=") else {
                return false;
            };
            let Some((a, b)) = r.split_once('-') else {
                return false;
            };
            let digits = |s: &str| s.bytes().all(|c| c.is_ascii_digit());
            !a.is_empty()
                && digits(a)
                && digits(b)
                && (b.is_empty() || a.parse::<u64>().ok() <= b.parse::<u64>().ok())
        }
        _ => false,
    }
}

/// Fetch's "CORS-unsafe request-header names": sorted, lowercase, unique.
pub fn unsafe_request_header_names(headers: &[(String, String)]) -> Vec<String> {
    let mut unsafe_names = Vec::new();
    let mut potentially = Vec::new();
    let mut size = 0;
    for (n, v) in headers {
        let n = n.to_ascii_lowercase();
        if is_safelisted_request_header(&n, v) {
            size += v.len();
            potentially.push(n);
        } else {
            unsafe_names.push(n);
        }
    }
    if size > 1024 {
        unsafe_names.extend(potentially);
    }
    unsafe_names.sort();
    unsafe_names.dedup();
    unsafe_names
}

/// All values of `name`, joined by `", "` like a header list's "get".
pub fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    let mut out: Option<String> = None;
    for (n, v) in headers {
        if n == name {
            match &mut out {
                Some(o) => {
                    o.push_str(", ");
                    o.push_str(v);
                }
                None => out = Some(v.clone()),
            }
        }
    }
    out
}

fn list(headers: &[(String, String)], name: &str) -> Vec<String> {
    header_value(headers, name)
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The CORS check: `Access-Control-Allow-Origin` is `origin` (or `*`
/// without credentials), and with credentials
/// `Access-Control-Allow-Credentials: true`.
pub fn cors_check(headers: &[(String, String)], origin: &str, credentials: bool) -> bool {
    let Some(allow) = header_value(headers, "access-control-allow-origin") else {
        return false;
    };
    if !credentials && allow == "*" {
        return true;
    }
    if allow != origin {
        return false;
    }
    !credentials
        || header_value(headers, "access-control-allow-credentials").as_deref() == Some("true")
}

/// A preflight response allows `method` and the request's unsafe header
/// names (Fetch, "CORS-preflight fetch" steps 7.3-7.8).
pub fn preflight_allows(
    headers: &[(String, String)],
    method: &str,
    unsafe_names: &[String],
    credentials: bool,
) -> bool {
    let methods = list(headers, "access-control-allow-methods");
    let names: Vec<String> = list(headers, "access-control-allow-headers")
        .into_iter()
        .map(|n| n.to_ascii_lowercase())
        .collect();
    let wildcard = |l: &[String]| !credentials && l.iter().any(|x| x == "*");
    if !is_safelisted_method(method) && !methods.iter().any(|m| m == method) && !wildcard(&methods)
    {
        return false;
    }
    unsafe_names
        .iter()
        .all(|n| names.contains(n) || (n != "authorization" && wildcard(&names)))
}

/// Response headers a script may read. Same-origin (`basic`) responses
/// show all but `Set-Cookie`; `cors` responses show the safelisted ones
/// plus `Access-Control-Expose-Headers`.
pub fn exposed_headers(
    headers: &[(String, String)],
    cors: bool,
    credentials: bool,
) -> Vec<(String, String)> {
    let visible = |n: &str| !matches!(n, "set-cookie" | "set-cookie2");
    if !cors {
        return headers
            .iter()
            .filter(|(n, _)| visible(n))
            .cloned()
            .collect();
    }
    let exposed: Vec<String> = list(headers, "access-control-expose-headers")
        .into_iter()
        .map(|n| n.to_ascii_lowercase())
        .collect();
    let all = !credentials && exposed.iter().any(|n| n == "*");
    headers
        .iter()
        .filter(|(n, _)| {
            visible(n)
                && (all
                    || exposed.contains(n)
                    || matches!(
                        n.as_str(),
                        "cache-control"
                            | "content-language"
                            | "content-length"
                            | "content-type"
                            | "expires"
                            | "last-modified"
                            | "pragma"
                    ))
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn simple_requests_need_no_preflight() {
        assert!(
            unsafe_request_header_names(&h(&[
                ("Accept", "application/json"),
                ("Content-Type", "text/plain;charset=UTF-8"),
            ]))
            .is_empty()
        );
        assert_eq!(
            unsafe_request_header_names(&h(&[
                ("Content-Type", "application/json"),
                ("X-Token", "1"),
                ("x-token", "2"),
            ])),
            ["content-type", "x-token"]
        );
        assert!(!is_safelisted_request_header("accept", "a\"b"));
        assert!(is_safelisted_request_header("range", "bytes=0-10"));
        assert!(!is_safelisted_request_header("range", "bytes=10-0"));
        assert!(is_forbidden_request_header("Sec-Fetch-Site", "x"));
        assert!(is_forbidden_request_header(
            "X-HTTP-Method-Override",
            "TRACE"
        ));
        assert!(!is_forbidden_request_header(
            "X-HTTP-Method-Override",
            "PUT"
        ));
        assert_eq!(normalize_method("patch"), "patch");
        assert_eq!(normalize_method("delete"), "DELETE");
    }

    #[test]
    fn cors_check_rules() {
        let o = "https://a.com";
        assert!(cors_check(
            &h(&[("access-control-allow-origin", "*")]),
            o,
            false
        ));
        assert!(!cors_check(
            &h(&[("access-control-allow-origin", "*")]),
            o,
            true
        ));
        assert!(!cors_check(&h(&[]), o, false));
        assert!(!cors_check(
            &h(&[("access-control-allow-origin", "https://b.com")]),
            o,
            false
        ));
        let creds = h(&[
            ("access-control-allow-origin", "https://a.com"),
            ("access-control-allow-credentials", "true"),
        ]);
        assert!(cors_check(&creds, o, true));
        assert!(!cors_check(&creds[..1], o, true));
        // Two values never match one origin.
        assert!(!cors_check(
            &h(&[
                ("access-control-allow-origin", o),
                ("access-control-allow-origin", o)
            ]),
            o,
            false
        ));
    }

    #[test]
    fn preflight_and_exposure() {
        let pre = h(&[
            ("access-control-allow-methods", "PUT, DELETE"),
            ("access-control-allow-headers", "X-Token"),
        ]);
        assert!(preflight_allows(&pre, "PUT", &["x-token".into()], true));
        assert!(!preflight_allows(&pre, "PATCH", &[], false));
        assert!(!preflight_allows(&pre, "PUT", &["x-other".into()], false));
        let star = h(&[
            ("access-control-allow-methods", "*"),
            ("access-control-allow-headers", "*"),
        ]);
        assert!(preflight_allows(&star, "PATCH", &["x-other".into()], false));
        assert!(!preflight_allows(&star, "PATCH", &[], true));
        assert!(!preflight_allows(
            &star,
            "GET",
            &["authorization".into()],
            false
        ));

        let resp = h(&[
            ("content-type", "text/plain"),
            ("x-secret", "1"),
            ("x-public", "2"),
            ("set-cookie", "a=1"),
            ("access-control-expose-headers", "X-Public"),
        ]);
        let names = |v: Vec<(String, String)>| v.into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        assert_eq!(
            names(exposed_headers(&resp, true, false)),
            ["content-type", "x-public"]
        );
        assert_eq!(names(exposed_headers(&resp, false, false)).len(), 4);
    }
}
