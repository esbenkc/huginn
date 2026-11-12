//! Internal `huginn:` pages (new tab, history, bookmarks), the bookmarks
//! file and address bar input. The UI makes the HTML from session data; the
//! engine loads it like any page but shows it at the `huginn:` URL.

use std::path::PathBuf;

use huginn_net::Url;

use crate::engine::escape_html;
use crate::session::Shared;

pub const NEW_TAB: &str = "huginn://newtab/";
pub const HISTORY: &str = "huginn://history/";
pub const BOOKMARKS: &str = "huginn://bookmarks/";
/// Address bar searches: no identifying parameters, the HTML (no-script) version.
const SEARCH: &str = "https://duckduckgo.com/html/?q=";

pub fn url(s: &str) -> Url {
    Url::parse(s).expect("valid internal URL")
}

pub fn is_new_tab(u: &Url) -> bool {
    u.scheme() == "huginn" && u.host_str() == Some("newtab")
}

/// The HTML for a `huginn:` URL; `None` for other URLs (they load normally).
/// `private`: the page is in a private window (the new tab page then shows
/// no sites).
pub fn html(u: &Url, shared: &Shared, private: bool) -> Option<String> {
    if u.scheme() != "huginn" {
        return None;
    }
    Some(match u.host_str().unwrap_or("") {
        "newtab" => new_tab(shared, private),
        "history" => history(shared),
        "bookmarks" => bookmarks(shared),
        _ => page(
            "Not found",
            &format!("<h1>Not found</h1><p>{}</p>", escape_html(u.as_str())),
        ),
    })
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><title>{}</title><style>\
         body{{margin:0;font-family:system-ui,-apple-system,Helvetica,sans-serif;color:#202124;background:#fff}}\
         main{{max-width:760px;margin:0 auto;padding:40px 24px}}\
         h1{{font-size:24px;font-weight:500;margin:0 0 24px}}\
         a{{color:#1a0dab;text-decoration:none}}\
         .row{{display:flex;align-items:baseline;padding:8px 12px;border-bottom:1px solid #eceef1}}\
         .t{{flex:1;min-width:0;overflow:hidden;white-space:nowrap;font-size:14px}}\
         .h{{color:#5f6368;font-size:12px;margin-left:16px;white-space:nowrap}}\
         .empty{{color:#5f6368;font-size:14px}}\
         .tiles{{display:flex;flex-wrap:wrap;justify-content:center;margin-top:32px}}\
         .tile{{display:block;width:112px;margin:8px;padding:16px 8px;border-radius:12px;background:#f1f3f4;\
         text-align:center;font-size:13px;color:#202124;overflow:hidden;white-space:nowrap}}\
         </style></head><body><main>{body}</main></body></html>",
        escape_html(title)
    )
}

fn row(url: &Url, title: &str) -> String {
    let title = if title.is_empty() {
        url.as_str()
    } else {
        title
    };
    format!(
        "<div class=row><a class=t href=\"{}\">{}</a><span class=h>{}</span></div>",
        escape_html(url.as_str()),
        escape_html(title),
        escape_html(url.host_str().unwrap_or(url.scheme()))
    )
}

fn new_tab(shared: &Shared, private: bool) -> String {
    let mut tiles = String::new();
    let mut seen = Vec::new();
    let picks = shared
        .bookmarks
        .iter()
        .map(|b| (&b.url, &b.title))
        .chain(shared.visits.iter().rev().map(|v| (&v.url, &v.title)));
    for (u, t) in picks.filter(|_| !private) {
        if seen.contains(&u) || u.scheme() == "huginn" || seen.len() >= 8 {
            continue;
        }
        seen.push(u);
        let label = if t.is_empty() {
            u.host_str().unwrap_or(u.as_str())
        } else {
            t
        };
        tiles.push_str(&format!(
            "<a class=tile href=\"{}\">{}</a>",
            escape_html(u.as_str()),
            escape_html(label)
        ));
    }
    page(
        "New Tab",
        &format!(
            "<div style=\"text-align:center;margin-top:120px\"><div style=\"font-size:56px;font-weight:600;\
             letter-spacing:-1px;color:#3c4043\">Huginn</div><div style=\"color:#5f6368;font-size:14px;margin-top:8px\">\
             {}</div></div><div class=tiles>{tiles}</div>",
            if private {
                "Private window: pages you visit are not kept in history"
            } else {
                "Search DuckDuckGo or type a URL"
            }
        ),
    )
}

fn history(shared: &Shared) -> String {
    let mut body = String::from("<h1>History</h1>");
    if shared.visits.is_empty() {
        body.push_str("<p class=empty>No pages visited in this session.</p>");
    }
    for v in shared.visits.iter().rev() {
        body.push_str(&row(&v.url, &v.title));
    }
    page("History", &body)
}

fn bookmarks(shared: &Shared) -> String {
    let mut body = String::from("<h1>Bookmarks</h1>");
    if shared.bookmarks.is_empty() {
        body.push_str("<p class=empty>No bookmarks yet. Press \u{2318}D on a page to add one.</p>");
    }
    for b in &shared.bookmarks {
        body.push_str(&row(&b.url, &b.title));
    }
    page("Bookmarks", &body)
}

/// Address bar input to a URL: full URLs as typed, paths as files, bare
/// hosts get `http://` (local) or `https://`, anything else is a search.
pub fn parse_input(input: &str) -> Option<Url> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    parse_url(s).or_else(|| {
        let q: String = url::form_urlencoded::byte_serialize(s.as_bytes()).collect();
        Url::parse(&format!("{SEARCH}{q}")).ok()
    })
}

/// Like [`parse_input`] without the search fallback.
pub fn parse_url(input: &str) -> Option<Url> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    if s.contains("://")
        || ["about:", "data:", "file:", "view-source:"]
            .iter()
            .any(|p| s.starts_with(p))
    {
        return Url::parse(s).ok();
    }
    if let Some(rest) = s.strip_prefix('~') {
        let home = std::env::var("HOME").ok()?;
        return huginn_net::parse_target(&format!("{home}{rest}")).ok();
    }
    if s.starts_with('/') || s.starts_with('.') {
        return huginn_net::parse_target(s).ok();
    }
    let host = s.split(['/', ':', '?', '#']).next().unwrap_or("");
    if s.contains(char::is_whitespace) || (!host.contains('.') && host != "localhost") {
        return None;
    }
    let local = host == "localhost" || host.parse::<std::net::Ipv4Addr>().is_ok();
    Url::parse(&format!("{}://{s}", if local { "http" } else { "https" })).ok()
}

#[derive(Clone, Debug)]
pub struct Bookmark {
    pub url: Url,
    pub title: String,
}

/// Where bookmarks live: `$HUGINN_PROFILE_DIR`, else
/// `~/Library/Application Support/Huginn`.
pub fn profile_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("HUGINN_PROFILE_DIR") {
        return Some(PathBuf::from(d));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/Huginn"))
}

/// The bookmarks file: one bookmark per line, `url<TAB>title`.
pub fn load_bookmarks(path: &std::path::Path) -> Vec<Bookmark> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| {
            let (u, t) = l.split_once('\t').unwrap_or((l, ""));
            Some(Bookmark {
                url: Url::parse(u.trim()).ok()?,
                title: t.trim().to_owned(),
            })
        })
        .collect()
}

pub fn save_bookmarks(path: &std::path::Path, bookmarks: &[Bookmark]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text: String = bookmarks
        .iter()
        .map(|b| format!("{}\t{}\n", b.url, b.title.replace(['\t', '\n', '\r'], " ")))
        .collect();
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_input_becomes_url_or_search() {
        let p = |s: &str| parse_input(s).map(|u| u.to_string());
        assert_eq!(
            p("127.0.0.1:8101/").as_deref(),
            Some("http://127.0.0.1:8101/")
        );
        assert_eq!(
            p("localhost:3000").as_deref(),
            Some("http://localhost:3000/")
        );
        assert_eq!(p("example.com/a").as_deref(), Some("https://example.com/a"));
        assert_eq!(p("http://x.org/#f").as_deref(), Some("http://x.org/#f"));
        assert_eq!(p("  about:blank ").as_deref(), Some("about:blank"));
        assert_eq!(p("huginn://history").as_deref(), Some("huginn://history"));
        assert_eq!(
            p("not a url").as_deref(),
            Some("https://duckduckgo.com/html/?q=not+a+url")
        );
        assert_eq!(
            p("word").as_deref(),
            Some("https://duckduckgo.com/html/?q=word")
        );
        assert_eq!(
            p("a&b=c").as_deref(),
            Some("https://duckduckgo.com/html/?q=a%26b%3Dc")
        );
        assert_eq!(p(""), None);
    }

    #[test]
    fn bookmarks_round_trip_through_the_file() {
        let dir = std::env::temp_dir().join(format!("huginn-bookmarks-{}", std::process::id()));
        let path = dir.join("bookmarks.tsv");
        let marks = vec![
            Bookmark {
                url: url("https://a.example/"),
                title: "A\ttab".into(),
            },
            Bookmark {
                url: url("file:///tmp/b.html"),
                title: String::new(),
            },
        ];
        save_bookmarks(&path, &marks).unwrap();
        let back = load_bookmarks(&path);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].title, "A tab");
        assert_eq!(back[1].url.as_str(), "file:///tmp/b.html");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
