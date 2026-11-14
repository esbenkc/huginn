//! The address bar's suggestions, like Chrome's omnibox: inline autofill of
//! a visited host or URL while the user types, and a drop-down of up to six
//! matches from history (by URL and title) and a search.
//!
//! Inline autofill follows Chrome's HistoryURL provider:
//! - Only rows that count take part (Chrome's `RowQualifiesAsSignificant`):
//!   typed at least once, visited 4 times or more, or visited in the last
//!   3 days. Search result pages of the address bar's own search do not.
//! - A row matches when its URL, after `https://`, `http://` and `www.` (each
//!   left out unless typed), starts with the input. Letter case does not
//!   matter.
//! - Order (`CompareHistoryMatch`): typed rows before never typed ones;
//!   matches right after the scheme and `www.` first; more typed visits; for
//!   rows typed once, a host before a page in it; more visits; the most
//!   recent visit.
//! - Host first: while the input has not gone past the best row's host, the
//!   autofill is that host's root (`kr` -> `kran.ai`), a stored row or a new
//!   one (`PromoteOrCreateShorterSuggestion`). A deeper URL is autofilled
//!   only once the input reaches into its path.
//! - A host is autofilled if it (or the best row) was typed once; a deeper
//!   URL if it was typed twice (`CanPromoteMatchForInlineAutocomplete`).
//!   Rows visited 4 times or more are autofilled without being typed: in
//!   Chrome the HistoryQuick provider scores such frequent rows above the
//!   search suggestion.
//! - As in Chrome, the field shows what the user typed plus the rest of the
//!   URL, without the trailing `/` of a bare host
//!   (`kFormatUrlOmitTrailingSlashOnBareHostname`).
//!
//! The drop-down's other rows are history rows whose URL or title contains
//! every word of the input, ranked by frecency: (visits + 20 x typed visits)
//! times a recency weight, more for rows whose URL starts with the input.

use std::cmp::Ordering;

use huginn_layout::{LayoutEngine, Rect};
use huginn_net::{ImageStore, Url};
use huginn_paint::CpuRasterizer;

use crate::engine::escape_html;
use crate::history::Row;
use crate::internal_pages;
use crate::theme;
use crate::toolbar::display_url;
use crate::ui::{Canvas, Pixels, render_snippet};

/// Rows in the drop-down at most.
pub const MAX_MATCHES: usize = 6;
/// Chrome's `kLowQualityMatchVisitLimit`: rows visited this often count.
const FREQUENT: u32 = 4;
/// Chrome's `kLowQualityMatchAgeLimitInDays`: rows visited this recently count.
const RECENT_MS: u64 = 3 * 24 * 3600 * 1000;
/// URL beginnings that may be left out of the input, longest first.
const PREFIXES: [&str; 5] = ["https://www.", "http://www.", "https://", "http://", ""];
/// Height of a drop-down row and the padding above and below the rows.
pub const ROW_HEIGHT: f32 = 28.0;
const PAD_Y: f32 = 4.0;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A page from history.
    History,
    /// The input itself as a URL.
    Typed,
    /// Search for the input.
    Search,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Match {
    pub kind: Kind,
    /// Where Enter or a click goes.
    pub url: Url,
    /// What the field shows while this row is selected with the arrow keys.
    pub fill: String,
    /// The page title of a history row; empty otherwise.
    pub title: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Suggestions {
    /// Best first; `matches[0]` is the default match (what Enter opens).
    pub matches: Vec<Match>,
    /// The text that completes the input inline to `matches[0]` ("" for none).
    pub inline: String,
}

/// Suggestions for `input` from history `rows` at `now` (Unix ms).
/// `allow_inline`: the input may be completed inline (Chrome prevents it
/// right after a deletion or a paste, and while the caret is not at the end).
pub fn suggest(rows: &[Row], input: &str, allow_inline: bool, now: u64) -> Suggestions {
    if input.trim().is_empty() {
        return Suggestions::default();
    }
    let rows: Vec<&Row> = rows
        .iter()
        .filter(|r| significant(r, now) && !internal_pages::is_search(&r.url))
        .collect();
    let (default, inline) = default_match(&rows, input, allow_inline);
    let looks_like_url = internal_pages::parse_url(input).is_some();
    let search = internal_pages::search_url(input).map(|url| Match {
        kind: Kind::Search,
        url,
        fill: input.to_owned(),
        title: String::new(),
    });
    let mut matches = vec![default];
    // A query: the search comes right after the default (Chrome scores it
    // above history rows); a URL: last.
    if !looks_like_url && let Some(s) = &search {
        push(&mut matches, s.clone());
    }
    let room = MAX_MATCHES - usize::from(search.is_some() && looks_like_url);
    for m in ranked(&rows, input, now) {
        if matches.len() >= room {
            break;
        }
        push(&mut matches, m);
    }
    if let Some(s) = search {
        push(&mut matches, s);
    }
    matches.truncate(MAX_MATCHES);
    Suggestions { matches, inline }
}

fn push(matches: &mut Vec<Match>, m: Match) {
    if !matches.iter().any(|x| x.url == m.url) {
        matches.push(m);
    }
}

/// Chrome's `RowQualifiesAsSignificant`.
fn significant(r: &Row, now: u64) -> bool {
    r.typed >= 1 || r.visits >= FREQUENT || now.saturating_sub(r.last_visit) <= RECENT_MS
}

fn starts_with_ci(s: &str, p: &str) -> bool {
    s.len() >= p.len() && s.as_bytes()[..p.len()].eq_ignore_ascii_case(p.as_bytes())
}

/// Offset in `url` after the scheme and a `www.`.
fn innermost_offset(url: &str) -> usize {
    let after = url.find("://").map_or(0, |i| i + 3);
    if starts_with_ci(&url[after..], "www.") {
        after + 4
    } else {
        after
    }
}

/// Where `input` starts in `url` when it matches a URL beginning, and
/// whether that is right after the scheme and `www.`.
fn prefix_match(url: &str, input: &str) -> Option<(usize, bool)> {
    let inner = innermost_offset(url);
    PREFIXES
        .iter()
        .find(|p| starts_with_ci(url, p) && starts_with_ci(&url[p.len()..], input))
        .map(|p| (p.len(), p.len() == inner))
}

/// A path of `/` and nothing after it (Chrome's `IsHostOnly`), on a URL
/// as `Url` serializes it.
fn host_only(url: &str) -> bool {
    let after = url.find("://").map_or(0, |i| i + 3);
    url[after..]
        .find(['/', '?', '#'])
        .is_none_or(|i| &url[after + i..] == "/")
}

struct Candidate<'a> {
    row: &'a Row,
    offset: usize,
    innermost: bool,
}

/// Chrome's `CompareHistoryMatch`: `Less` puts `a` first.
fn compare(a: &Candidate, b: &Candidate) -> Ordering {
    let (ra, rb) = (a.row, b.row);
    (rb.typed > 0)
        .cmp(&(ra.typed > 0))
        .then(b.innermost.cmp(&a.innermost))
        .then(rb.typed.cmp(&ra.typed))
        .then_with(|| {
            if ra.typed == 1 {
                host_only(&rb.url).cmp(&host_only(&ra.url))
            } else {
                Ordering::Equal
            }
        })
        .then(rb.visits.cmp(&ra.visits))
        .then(rb.last_visit.cmp(&ra.last_visit))
        .then_with(|| rb.url.cmp(&ra.url))
}

/// The default match and its inline completion.
fn default_match(rows: &[&Row], input: &str, allow_inline: bool) -> (Match, String) {
    let mut candidates: Vec<Candidate> = rows
        .iter()
        .filter_map(|r| {
            prefix_match(&r.url, input).map(|(offset, innermost)| Candidate {
                row: r,
                offset,
                innermost,
            })
        })
        .collect();
    candidates.sort_by(compare);
    if let Some(best) = candidates.first()
        && let Some((target, title, eligible)) = autofill_target(rows, best, input)
    {
        let spec = target.as_str();
        let mut rest = &spec[best.offset + input.len()..];
        if host_only(spec) {
            rest = rest.strip_suffix('/').unwrap_or(rest);
        }
        // An exact match needs no completion, so a deletion does not stop it.
        if rest.is_empty() || (allow_inline && eligible) {
            let rest = rest.to_owned();
            return (
                Match {
                    kind: Kind::History,
                    fill: format!("{input}{rest}"),
                    url: target,
                    title,
                },
                rest,
            );
        }
    }
    let m = match internal_pages::parse_url(input) {
        Some(url) => match rows.iter().find(|r| r.url == url.as_str()) {
            Some(r) => Match {
                kind: Kind::History,
                url,
                fill: input.to_owned(),
                title: r.title.clone(),
            },
            None => Match {
                kind: Kind::Typed,
                url,
                fill: input.to_owned(),
                title: String::new(),
            },
        },
        None => Match {
            kind: Kind::Search,
            url: internal_pages::search_url(input).expect("search URL"),
            fill: input.to_owned(),
            title: String::new(),
        },
    };
    (m, String::new())
}

/// What `best` autofills to: its host's root while the input has not gone
/// past the host, else `best` itself; with a title and whether it may be
/// completed inline.
fn autofill_target(rows: &[&Row], best: &Candidate, input: &str) -> Option<(Url, String, bool)> {
    let url = Url::parse(&best.row.url).ok()?;
    let mut root = url.clone();
    if matches!(url.scheme(), "http" | "https") && url.has_host() {
        root.set_path("/");
        root.set_query(None);
        root.set_fragment(None);
    }
    let in_host = root.as_str().len() >= best.offset + input.len()
        && starts_with_ci(&root.as_str()[best.offset..], input);
    let typed_host = |r: &Row| r.typed >= 1 || r.visits >= FREQUENT;
    if in_host && root != url {
        let stored = rows.iter().find(|r| r.url == root.as_str());
        let eligible = typed_host(best.row) || stored.is_some_and(|r| typed_host(r));
        let title = stored.map(|r| r.title.clone()).unwrap_or_default();
        return Some((root, title, eligible));
    }
    let eligible = if host_only(&best.row.url) {
        typed_host(best.row)
    } else {
        best.row.typed >= 2 || best.row.visits >= FREQUENT
    };
    Some((url, best.row.title.clone(), eligible))
}

/// Recency weight of a visit `age_ms` ago.
fn recency(age_ms: u64) -> f64 {
    let days = age_ms / (24 * 3600 * 1000);
    match days {
        0..=4 => 1.0,
        5..=14 => 0.7,
        15..=31 => 0.5,
        32..=90 => 0.3,
        _ => 0.1,
    }
}

/// `needle` in `hay` (ASCII case-insensitive): the byte offsets it starts at.
fn find_ci<'a>(hay: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    (0..=h.len().saturating_sub(n.len()))
        .filter(move |&i| n.len() <= h.len() && h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// History rows whose URL (after the scheme) or title contains every word
/// of the input, best first.
fn ranked(rows: &[&Row], input: &str, now: u64) -> Vec<Match> {
    let terms: Vec<&str> = input.split_whitespace().collect();
    let word_start = |s: &str, i: usize| i == 0 || !s.as_bytes()[i - 1].is_ascii_alphanumeric();
    let mut scored: Vec<(f64, &Row)> = rows
        .iter()
        .filter_map(|r| {
            let spec = r.url.as_str();
            let url = &spec[spec.find("://").map_or(0, |i| i + 3)..];
            let mut at_word = true;
            for t in &terms {
                let mut hits = find_ci(url, t)
                    .map(|i| word_start(url, i))
                    .chain(find_ci(&r.title, t).map(|i| word_start(&r.title, i)))
                    .peekable();
                hits.peek()?;
                at_word &= hits.any(|w| w);
            }
            let topical = if prefix_match(spec, input).is_some() {
                4.0
            } else if at_word {
                2.0
            } else {
                1.0
            };
            let frecency = f64::from(r.visits.saturating_add(r.typed.saturating_mul(20)))
                * recency(now.saturating_sub(r.last_visit));
            Some((topical * frecency, *r))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(b.1.last_visit.cmp(&a.1.last_visit))
    });
    // One more than fits: the default match may be one of them.
    scored
        .into_iter()
        .filter_map(|(_, r)| {
            let url = Url::parse(&r.url).ok()?;
            Some(Match {
                kind: Kind::History,
                fill: display_url(&url),
                url,
                title: r.title.clone(),
            })
        })
        .take(MAX_MATCHES + 1)
        .collect()
}

/// The drop-down under the address field while the user types.
#[derive(Default)]
pub struct Popup {
    /// What the user typed (the field without the inline completion).
    pub user_text: String,
    pub result: Suggestions,
    /// The row the arrow keys picked; 0 is the default match.
    pub selected: usize,
    /// The row under the pointer.
    pub hovered: Option<usize>,
    /// Window CSS px.
    rect: Rect,
    /// Rendered rows and the scale they were rendered at.
    pixels: Option<(Pixels, f32)>,
    /// Origin of the last default match the network warmed up.
    pub preconnected: Option<String>,
}

impl Popup {
    pub fn is_open(&self) -> bool {
        !self.result.matches.is_empty()
    }

    pub fn close(&mut self) {
        self.user_text.clear();
        self.result = Suggestions::default();
        self.selected = 0;
        self.hovered = None;
        self.pixels = None;
        self.preconnected = None;
    }

    /// New suggestions for `user_text`; the default row is selected.
    pub fn set(&mut self, user_text: String, result: Suggestions) {
        self.user_text = user_text;
        self.result = result;
        self.selected = 0;
        self.hovered = None;
        self.pixels = None;
    }

    /// The field text with the default match: input plus inline completion.
    pub fn default_text(&self) -> String {
        format!("{}{}", self.user_text, self.result.inline)
    }

    pub fn select(&mut self, i: usize) {
        if i != self.selected {
            self.selected = i;
            self.pixels = None;
        }
    }

    pub fn hover(&mut self, i: Option<usize>) {
        if i != self.hovered {
            self.hovered = i;
            self.pixels = None;
        }
    }

    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// Put the drop-down under the address field `field` (window CSS px).
    pub fn place(&mut self, field: Rect) {
        let rect = Rect {
            x: field.x,
            y: field.bottom() - 1.0,
            width: field.width,
            height: 2.0 * PAD_Y + ROW_HEIGHT * self.result.matches.len() as f32 + 1.0,
        };
        if rect != self.rect {
            self.rect = rect;
            self.pixels = None;
        }
    }

    /// The row at window point (`x`, `y`).
    pub fn row_at(&self, x: f32, y: f32) -> Option<usize> {
        let r = self.rect;
        if !self.is_open() || x < r.x || x >= r.right() || y < r.y || y >= r.bottom() {
            return None;
        }
        let i = ((y - r.y - PAD_Y) / ROW_HEIGHT).floor();
        (i >= 0.0 && (i as usize) < self.result.matches.len()).then_some(i as usize)
    }

    pub fn draw(
        &mut self,
        c: &mut Canvas,
        layout: &mut LayoutEngine,
        raster: &mut CpuRasterizer,
        private: bool,
    ) {
        if !self.is_open() {
            return;
        }
        let r = self.rect;
        if self.pixels.as_ref().is_none_or(|(_, s)| *s != c.scale) {
            let html = self.html(private);
            let (px, _) = render_snippet(
                layout,
                raster,
                &html,
                r.width,
                r.height,
                &[],
                &ImageStore::new(),
                c.scale,
            );
            self.pixels = Some((px, c.scale));
        }
        if let Some((px, _)) = &self.pixels {
            c.blit(px, r.x, r.y);
        }
    }

    fn html(&self, private: bool) -> String {
        let accent = theme::accent(private);
        let mut rows = String::new();
        for (i, m) in self.result.matches.iter().enumerate() {
            let selected = i == self.selected;
            let bg = if selected {
                theme::HOVER
            } else if self.hovered == Some(i) {
                theme::ACTIVE
            } else {
                "transparent"
            };
            let rule = if selected {
                format!(
                    "<div style=\"position:absolute;left:0;top:0;width:2px;height:{ROW_HEIGHT}px;background:{accent}\"></div>"
                )
            } else {
                String::new()
            };
            // A filled square for pages, a hollow one for a search.
            let marker = match m.kind {
                Kind::Search => format!(
                    "<div style=\"width:6px;height:6px;flex-shrink:0;margin-right:12px;box-sizing:border-box;\
                     border:1px solid {}\"></div>",
                    theme::MUTED
                ),
                _ => format!(
                    "<div style=\"width:6px;height:6px;flex-shrink:0;margin-right:12px;background:{}\"></div>",
                    if selected { accent } else { theme::MUTED }
                ),
            };
            let (text, note) = match m.kind {
                Kind::Search => (escape_html(&m.fill), "Search DuckDuckGo".to_owned()),
                Kind::Typed => (escape_html(&m.fill), String::new()),
                Kind::History => {
                    let shown = if i == 0 {
                        m.fill.clone()
                    } else {
                        display_url(&m.url)
                    };
                    let host = m.url.host_str().unwrap_or("");
                    let text = match shown.find(host).filter(|_| !host.is_empty()) {
                        Some(at) => format!(
                            "<span style=\"color:{muted}\">{}</span>{}<span style=\"color:{muted}\">{}</span>",
                            escape_html(&shown[..at]),
                            escape_html(host),
                            escape_html(&shown[at + host.len()..]),
                            muted = theme::MUTED
                        ),
                        None => escape_html(&shown),
                    };
                    (text, m.title.clone())
                }
            };
            let note = if note.is_empty() {
                String::new()
            } else {
                format!(
                    "<div style=\"flex-shrink:1;min-width:0;overflow:hidden;margin-left:16px;color:{}\">{}</div>",
                    theme::MUTED,
                    escape_html(&note)
                )
            };
            rows.push_str(&format!(
                "<div style=\"position:relative;height:{ROW_HEIGHT}px;padding:0 12px;display:flex;align-items:center;\
                 background:{bg};white-space:pre;overflow:hidden\">{rule}{marker}\
                 <div style=\"flex-shrink:0;max-width:70%;overflow:hidden\">{text}</div>{note}</div>"
            ));
        }
        format!(
            "<!doctype html><html><body style=\"margin:0;font-family:{};font-size:12px;color:{}\">\
             <div style=\"box-sizing:border-box;width:{w}px;height:{h}px;padding:{PAD_Y}px 0;background:{};\
             border:1px solid {accent};border-top:0;overflow:hidden\">{rows}</div></body></html>",
            theme::FONT,
            theme::TEXT,
            theme::TOOLBAR,
            w = self.rect.width,
            h = self.rect.height,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 3600 * 1000;
    const NOW: u64 = 1_000 * DAY;

    fn row(url: &str, title: &str, visits: u32, typed: u32, days_ago: u64) -> Row {
        Row {
            url: Url::parse(url).unwrap().into(),
            title: title.into(),
            visits,
            typed,
            last_visit: NOW - days_ago * DAY,
        }
    }

    /// The field after typing `input`: input plus the selected completion,
    /// and where the default match goes.
    fn field(rows: &[Row], input: &str) -> (String, String, String) {
        let s = suggest(rows, input, true, NOW);
        (
            input.to_owned(),
            s.inline.clone(),
            s.matches[0].url.to_string(),
        )
    }

    fn fill(rows: &[Row], input: &str) -> String {
        let (typed, inline, _) = field(rows, input);
        typed + &inline
    }

    #[test]
    fn a_typed_host_autofills_with_or_without_www_and_scheme() {
        let rows = [
            row("https://kran.ai/", "Kran", 1, 1, 20),
            row("https://www.example.com/", "Example", 1, 1, 20),
        ];
        assert_eq!(
            field(&rows, "kr"),
            ("kr".into(), "an.ai".into(), "https://kran.ai/".into())
        );
        assert_eq!(fill(&rows, "KR"), "KRan.ai");
        assert_eq!(fill(&rows, "exa"), "example.com");
        assert_eq!(fill(&rows, "www.exa"), "www.example.com");
        assert_eq!(fill(&rows, "https://kr"), "https://kran.ai");
        assert_eq!(fill(&rows, "https://www.ex"), "https://www.example.com");
        // Only a URL beginning matches, not the middle of a host.
        assert_eq!(fill(&rows, "ran"), "ran");
        // The trailing slash shows once the user types it.
        assert_eq!(fill(&rows, "kran.ai/"), "kran.ai/");
        // Typed in full: no completion, and Enter goes to the stored URL.
        assert_eq!(
            field(&rows, "kran.ai"),
            ("kran.ai".into(), String::new(), "https://kran.ai/".into())
        );
    }

    #[test]
    fn the_host_comes_first_and_a_deeper_url_only_once_the_path_is_typed() {
        let rows = [
            row("https://kran.ai/", "Kran", 2, 1, 1),
            row("https://kran.ai/blog/one", "One", 40, 5, 0),
            row("https://kran.ai/blog/two", "Two", 3, 2, 0),
        ];
        assert_eq!(fill(&rows, "kr"), "kran.ai");
        assert_eq!(fill(&rows, "kran.ai/"), "kran.ai/");
        assert_eq!(fill(&rows, "kran.ai/b"), "kran.ai/blog/one");
        assert_eq!(fill(&rows, "kran.ai/blog/t"), "kran.ai/blog/two");
        // A host never visited itself is made from a deeper typed page.
        let deep = [row("https://docs.example/guide/start", "Guide", 1, 1, 0)];
        assert_eq!(
            field(&deep, "do"),
            (
                "do".into(),
                "cs.example".into(),
                "https://docs.example/".into()
            )
        );
        // A deeper page typed once is not autofilled; twice it is.
        assert_eq!(fill(&deep, "docs.example/g"), "docs.example/g");
        let twice = [row("https://docs.example/guide/start", "Guide", 2, 2, 0)];
        assert_eq!(fill(&twice, "docs.example/g"), "docs.example/guide/start");
    }

    #[test]
    fn typed_hosts_win_then_typed_count_visits_and_recency() {
        let rows = [
            row("https://krill.example/", "", 50, 0, 0),
            row("https://kran.ai/", "", 1, 1, 30),
        ];
        assert_eq!(fill(&rows, "kr"), "kran.ai");
        let rows = [
            row("https://kran.ai/", "", 1, 1, 0),
            row("https://krone.example/", "", 1, 3, 30),
        ];
        assert_eq!(fill(&rows, "kr"), "krone.example");
        let rows = [
            row("https://kran.ai/", "", 2, 1, 0),
            row("https://krone.example/", "", 9, 1, 30),
        ];
        assert_eq!(fill(&rows, "kr"), "krone.example");
        let rows = [
            row("https://kran.ai/", "", 2, 1, 0),
            row("https://krone.example/", "", 2, 1, 30),
        ];
        assert_eq!(fill(&rows, "kr"), "kran.ai");
    }

    #[test]
    fn rows_that_do_not_count_are_not_autofilled() {
        // Visited once by a link, today: in the drop-down, not autofilled.
        let rows = [row("https://kran.ai/", "Kran", 1, 0, 0)];
        let s = suggest(&rows, "kr", true, NOW);
        assert_eq!(s.inline, "");
        assert_eq!(s.matches[0].kind, Kind::Search);
        assert!(
            s.matches
                .iter()
                .any(|m| m.url.as_str() == "https://kran.ai/")
        );
        // Visited 4 times by links: autofilled.
        let rows = [row("https://kran.ai/", "Kran", 4, 0, 10)];
        assert_eq!(fill(&rows, "kr"), "kran.ai");
        // Visited once 10 days ago, never typed: not even suggested.
        let rows = [row("https://kran.ai/", "Kran", 1, 0, 10)];
        let s = suggest(&rows, "kr", true, NOW);
        assert_eq!(s.matches.len(), 1);
        // The address bar's own search results are never suggested.
        let rows = [
            row("https://duckduckgo.com/html/?q=kran", "kran", 9, 9, 0),
            row("https://html.duckduckgo.com/html/?q=kran", "kran", 9, 9, 0),
        ];
        assert_eq!(fill(&rows, "duck"), "duck");
        assert_eq!(fill(&rows, "html"), "html");
        assert_eq!(suggest(&rows, "kran", true, NOW).matches.len(), 1);
    }

    #[test]
    fn a_deletion_stops_the_completion_but_not_an_exact_match() {
        let rows = [row("http://127.0.0.1:8080/", "Local", 1, 1, 0)];
        let s = suggest(&rows, "127", false, NOW);
        assert_eq!(s.inline, "");
        assert_eq!(s.matches[0].kind, Kind::Search);
        let s = suggest(&rows, "127.0.0.1:8080", false, NOW);
        assert_eq!(s.inline, "");
        assert_eq!(s.matches[0].url.as_str(), "http://127.0.0.1:8080/");
        let s = suggest(&rows, "127", true, NOW);
        assert_eq!(s.inline, ".0.0.1:8080");
    }

    #[test]
    fn the_drop_down_matches_titles_and_urls_and_ends_with_a_search() {
        let rows = [
            row("https://kran.ai/", "Kran home", 3, 1, 0),
            row("https://news.example/a", "Rust weekly", 9, 0, 1),
            row("https://blog.example/rust", "Notes", 2, 0, 2),
            row("https://other.example/", "Nothing here", 50, 5, 0),
        ];
        let s = suggest(&rows, "rust", true, NOW);
        let kinds: Vec<_> = s.matches.iter().map(|m| (m.kind, m.url.as_str())).collect();
        assert_eq!(
            kinds,
            [
                (Kind::Search, "https://duckduckgo.com/html/?q=rust"),
                (Kind::History, "https://news.example/a"),
                (Kind::History, "https://blog.example/rust"),
            ]
        );
        // Several words must all match; a URL-like input searches last.
        let s = suggest(&rows, "kran home", true, NOW);
        assert_eq!(s.matches[1].url.as_str(), "https://kran.ai/");
        let s = suggest(&rows, "kran.ai/x", true, NOW);
        assert_eq!(s.matches[0].kind, Kind::Typed);
        assert_eq!(s.matches.last().unwrap().kind, Kind::Search);
        // At most six rows.
        let many: Vec<Row> = (0..20)
            .map(|i| row(&format!("https://k{i}.example/"), "", 5, 1, 0))
            .collect();
        assert_eq!(suggest(&many, "k", true, NOW).matches.len(), MAX_MATCHES);
    }
}
