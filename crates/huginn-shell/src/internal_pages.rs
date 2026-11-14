//! Internal `huginn:` pages (new tab, history, bookmarks), the bookmarks
//! file and address bar input. The UI makes the HTML from session data; the
//! engine loads it like any page but shows it at the `huginn:` URL. The pages
//! wear the chrome's Forest Mono look; with the acrylic material their
//! background is see-through and the content sits on clear panels.

use std::path::PathBuf;

use huginn_net::Url;

use crate::engine::escape_html;
use crate::session::Shared;
use crate::theme;

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
        "history" => history(shared, private),
        "bookmarks" => bookmarks(shared, private),
        _ => page(
            "Not found",
            &format!(
                "{}<p>{}</p>",
                heading("Not found", private),
                escape_html(u.as_str())
            ),
            private,
        ),
    })
}

/// Hairlines on the page: solid, or light over the acrylic.
fn line() -> &'static str {
    if theme::material() {
        "rgba(227,242,234,0.13)"
    } else {
        theme::HAIRLINE
    }
}

fn page(title: &str, body: &str, private: bool) -> String {
    let accent = theme::accent(private);
    let line = line();
    // Opaque: the strip colour. Acrylic: no background (the blurred desktop
    // shows) and clear panels with a lit top edge.
    let (bg, panel, tile) = if theme::material() {
        (
            "transparent",
            "background:rgba(11,26,20,0.58);border:1px solid rgba(227,242,234,0.16);\
             border-top-color:rgba(255,255,255,0.5);padding:28px 32px",
            "rgba(11,26,20,0.55)",
        )
    } else {
        (theme::STRIP, "", "transparent")
    };
    format!(
        "<!doctype html><html><head><title>{}</title><style>\
         html{{background:{bg}}}\
         body{{margin:0;font-family:{font};font-size:13px;color:{text}}}\
         svg{{display:block}}\
         main{{max-width:760px;margin:0 auto;padding:48px 24px}}\
         .panel{{{panel}}}\
         h1{{display:flex;align-items:center;font-size:11px;font-weight:400;color:{muted};text-transform:uppercase;\
         letter-spacing:0.14em;margin:0 0 8px;padding-bottom:14px;border-bottom:1px solid {line}}}\
         h1 svg{{margin-right:10px}}\
         a{{color:{accent};text-decoration:none}}\
         .row{{display:flex;align-items:baseline;padding:10px 0;border-bottom:1px solid {line}}}\
         .t{{flex:1;min-width:0;overflow:hidden;white-space:nowrap}}\
         .h{{color:{muted};font-size:10px;margin-left:16px;white-space:nowrap;text-transform:uppercase;letter-spacing:0.06em}}\
         .empty{{color:{muted}}}\
         .hero{{position:relative;padding-top:24px}}\
         .slab{{position:relative;width:440px;height:440px;margin:24px auto 0}}\
         .slab>svg{{position:absolute;left:0;top:0}}\
         .slab .hero{{padding:44px}}\
         .flock{{position:absolute;right:44px;top:56px}}\
         .word{{font-size:56px;line-height:1;letter-spacing:0.32em;text-transform:uppercase;color:{text};margin-top:36px}}\
         .sub{{font-size:11px;letter-spacing:0.14em;text-transform:uppercase;color:{muted};margin-top:16px}}\
         .priv{{display:inline-block;font-size:10px;letter-spacing:0.12em;color:{accent};border:1px solid {accent};\
         padding:3px 8px;margin-top:20px}}\
         .tiles{{display:flex;flex-wrap:wrap;width:680px;margin:40px auto 0;border-top:1px solid {line};border-left:1px solid {line}}}\
         .tile{{display:block;width:170px;height:68px;box-sizing:border-box;padding:14px;background:{tile};\
         border-right:1px solid {line};border-bottom:1px solid {line};overflow:hidden;white-space:nowrap}}\
         .tt{{display:block;font-size:12px;color:{text};overflow:hidden}}\
         .th{{display:block;font-size:10px;color:{muted};margin-top:10px;text-transform:uppercase;letter-spacing:0.06em;overflow:hidden}}\
         </style></head><body><main>{body}</main></body></html>",
        escape_html(title),
        font = theme::FONT,
        text = theme::TEXT,
        muted = theme::MUTED,
    )
}

/// A page heading: the accent asterisk and an uppercase label.
fn heading(label: &str, private: bool) -> String {
    format!(
        "<h1>{}<span>{}</span></h1>",
        theme::asterisk_svg(12, theme::accent(private)),
        escape_html(label)
    )
}

/// Wrap a list page's body in a panel (acrylic) or nothing (opaque).
fn panel(body: &str) -> String {
    format!("<div class=panel>{body}</div>")
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
        let host = u.host_str().unwrap_or(u.scheme());
        let label = if t.is_empty() { host } else { t };
        tiles.push_str(&format!(
            "<a class=tile href=\"{}\"><span class=tt>{}</span><span class=th>{}</span></a>",
            escape_html(u.as_str()),
            escape_html(label),
            escape_html(host)
        ));
    }
    let accent = theme::accent(private);
    let hero = format!(
        "<div class=hero>{}<div class=flock>{}</div><div class=word>Huginn</div>{}<div class=sub>{}</div></div>",
        theme::asterisk_svg(56, accent),
        theme::flock_svg(150),
        if private {
            "<div class=priv>PRIVATE</div>"
        } else {
            ""
        },
        if private {
            "Pages you visit are not kept in history"
        } else {
            "Search DuckDuckGo or type a URL"
        }
    );
    // Acrylic: the mark sits in a clear slab with electronics inside.
    let hero = if theme::material() {
        format!(
            "<div class=slab>{}{hero}</div>",
            theme::slab_svg(440, 440, 0x4E45_5754_4142)
        )
    } else {
        hero
    };
    let tiles = if tiles.is_empty() {
        String::new()
    } else {
        format!("<div class=tiles>{tiles}</div>")
    };
    page("New Tab", &format!("{hero}{tiles}"), private)
}

fn history(shared: &Shared, private: bool) -> String {
    let mut body = heading("History", private);
    if shared.visits.is_empty() {
        body.push_str("<p class=empty>No pages visited in this session.</p>");
    }
    for v in shared.visits.iter().rev() {
        body.push_str(&row(&v.url, &v.title));
    }
    page("History", &panel(&body), private)
}

fn bookmarks(shared: &Shared, private: bool) -> String {
    let mut body = heading("Bookmarks", private);
    if shared.bookmarks.is_empty() {
        body.push_str("<p class=empty>No bookmarks yet. Press \u{2318}D on a page to add one.</p>");
    }
    for b in &shared.bookmarks {
        body.push_str(&row(&b.url, &b.title));
    }
    page("Bookmarks", &panel(&body), private)
}

/// Address bar input to a URL: full URLs as typed, paths as files, bare
/// hosts get `http://` (local) or `https://`, anything else is a search.
pub fn parse_input(input: &str) -> Option<Url> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    parse_url(s).or_else(|| search_url(s))
}

/// The address bar's search for `query`.
pub fn search_url(query: &str) -> Option<Url> {
    let q: String = url::form_urlencoded::byte_serialize(query.trim().as_bytes()).collect();
    Url::parse(&format!("{SEARCH}{q}")).ok()
}

/// A result page of the address bar's search (never suggested from
/// history, like Chrome leaves out its default search engine's pages).
/// DuckDuckGo answers `SEARCH` with a redirect to `html.duckduckgo.com`.
pub fn is_search(url: &str) -> bool {
    url.starts_with(SEARCH) || url.starts_with("https://html.duckduckgo.com/html/?")
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
/// `~/Library/Application Support/Huginn` on macOS and
/// `$XDG_CONFIG_HOME/huginn` (`~/.config/huginn`) elsewhere.
pub fn profile_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("HUGINN_PROFILE_DIR") {
        return Some(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        return home.map(|h| h.join("Library/Application Support/Huginn"));
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
        .or(home.map(|h| h.join(".config")))
        .map(|d| d.join("huginn"))
}

/// Where the HTTP cache lives: `$HUGINN_PROFILE_DIR/Cache`, else the
/// user's cache directory: `~/Library/Caches/Huginn` on macOS and
/// `$XDG_CACHE_HOME/huginn` (`~/.cache/huginn`) elsewhere. The system may
/// delete it at any time, and backups skip it.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("HUGINN_PROFILE_DIR") {
        return Some(PathBuf::from(d).join("Cache"));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        return home.map(|h| h.join("Library/Caches/Huginn"));
    }
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
        .or(home.map(|h| h.join(".cache")))
        .map(|d| d.join("huginn"))
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
