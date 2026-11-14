//! The browsing history of normal windows, for the address bar's
//! suggestions (`omnibox.rs`): one row per URL with its title, visit count,
//! typed count and last visit, like the `urls` table of Chrome's history.
//! Private windows never record anything.
//!
//! The rows live in `history.tsv` in the profile directory, readable by the
//! owner only. Each line adds to a URL's row
//! (`last_visit_ms<TAB>visits<TAB>typed<TAB>url<TAB>title`: the visits and
//! typed visits to add, the time of the visit, the title if not empty), so a
//! visit appends one line without reading the file. The file is read the
//! first time the address bar needs the rows, not at start and not on the
//! way to a first paint; when it holds many more lines than rows it is
//! rewritten with one line per row (a new file, then a rename). Like
//! Chrome, rows not visited for 90 days expire; at most 10,000 rows are
//! kept (the least recently visited go first). Headless runs without
//! `HUGINN_PROFILE_DIR` keep the history in memory only.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use huginn_net::Url;

/// Rows kept at most; the least recently visited go first.
pub const MAX_ROWS: usize = 10_000;
/// Chrome expires history after 90 days.
pub const EXPIRE_MS: u64 = 90 * 24 * 3600 * 1000;
/// Longer titles are cut (in bytes, on a character boundary).
const MAX_TITLE: usize = 1024;
/// Longer URLs (huge `data:`-like query strings) are not recorded.
const MAX_URL: usize = 8 * 1024;
/// The file is rewritten when it has this many more lines than rows.
const COMPACT_SLACK: usize = 1000;

/// How a page load came about; decides what it adds to its row (Chrome's
/// page transitions as `HistoryBackend::AddPageVisit` counts them).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Transition {
    /// Typed or picked in the address bar: a visit and a typed visit.
    Typed,
    /// A link, a form, a bookmark, a new tab tile: a visit.
    Link,
    /// Back or forward: a visit, never a typed one.
    BackForward,
    /// A reload (or a discarded tab loading again): only the title and
    /// the time of the last visit change.
    Reload,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// The URL as `Url` serializes it. Kept as text: the address bar
    /// matches text, and parses only the rows it shows.
    pub url: String,
    pub title: String,
    pub visits: u32,
    pub typed: u32,
    /// Unix time of the last visit, ms.
    pub last_visit: u64,
}

/// What one line adds to a row.
struct Delta<'a> {
    url: &'a str,
    title: &'a str,
    visits: u32,
    typed: u32,
    /// Time of the visit (0: none, a title change).
    at: u64,
}

pub struct History {
    /// `None`: in memory only.
    path: Option<PathBuf>,
    /// `None` until first used.
    rows: Option<Vec<Row>>,
    /// Lines in the file (stale ones included); counted when it is read.
    lines: usize,
}

/// Now as Unix ms.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl History {
    pub fn in_memory() -> History {
        History {
            path: None,
            rows: Some(Vec::new()),
            lines: 0,
        }
    }

    /// History saved in `path`, read when first used.
    pub fn persistent(path: PathBuf) -> History {
        History {
            path: Some(path),
            rows: None,
            lines: 0,
        }
    }

    /// All rows, in no particular order.
    pub fn rows(&mut self) -> &[Row] {
        self.load(now_ms());
        self.rows.as_deref().unwrap_or_default()
    }

    pub fn len(&mut self) -> usize {
        self.rows().len()
    }

    /// The row of `url`, if any.
    #[cfg(test)]
    pub fn get(&mut self, url: &Url) -> Option<&Row> {
        self.rows().iter().find(|r| r.url == url.as_str())
    }

    /// A load of `url` with `title` finished at `now` (Unix ms).
    pub fn record(&mut self, url: &Url, title: &str, how: Transition, now: u64) {
        let (visits, typed) = match how {
            Transition::Typed => (1, 1),
            Transition::Link | Transition::BackForward => (1, 0),
            Transition::Reload => (0, 0),
        };
        self.change(Delta {
            url: url.as_str(),
            title: &clean_title(title),
            visits,
            typed,
            at: now,
        });
    }

    /// The page at `url` changed its title after it loaded.
    pub fn set_title(&mut self, url: &Url, title: &str) {
        let title = clean_title(title);
        if title.is_empty() {
            return;
        }
        let known = self
            .rows
            .as_ref()
            .and_then(|rows| rows.iter().find(|r| r.url == url.as_str()));
        if known.is_some_and(|r| r.title == title) {
            return;
        }
        self.change(Delta {
            url: url.as_str(),
            title: &title,
            visits: 0,
            typed: 0,
            at: 0,
        });
    }

    /// Forget everything (clear browsing data): the rows and the file.
    pub fn clear(&mut self) {
        self.rows = Some(Vec::new());
        self.lines = 0;
        if let Some(p) = &self.path
            && let Err(e) = std::fs::remove_file(p)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("huginn: cannot delete the history file: {e}");
        }
    }

    /// Apply `d` to the rows if they are read, and save it.
    fn change(&mut self, d: Delta) {
        if !recordable(d.url) {
            return;
        }
        if let Some(rows) = &mut self.rows {
            add(rows, &d);
            if rows.len() > MAX_ROWS
                && let Some(old) = (0..rows.len()).min_by_key(|&i| rows[i].last_visit)
            {
                rows.swap_remove(old);
            }
        }
        let Some(path) = &self.path else { return };
        self.lines += 1;
        let rows = self.rows.as_ref().map(Vec::len);
        if rows.is_some_and(|n| self.lines > 2 * n + COMPACT_SLACK) {
            self.compact();
        } else if let Err(e) = append_private(path, &line(&d)) {
            eprintln!("huginn: cannot save history: {e}");
        }
    }

    /// Read the file the first time the rows are needed.
    fn load(&mut self, now: u64) {
        if self.rows.is_some() {
            return;
        }
        let (rows, lines) = self
            .path
            .as_deref()
            .map_or((Vec::new(), 0), |p| read(p, now));
        let compact = lines > rows.len() + COMPACT_SLACK;
        self.rows = Some(rows);
        self.lines = lines;
        if compact {
            self.compact();
        }
    }

    /// Write the file again with one line per row.
    fn compact(&mut self) {
        let (Some(path), Some(rows)) = (&self.path, &self.rows) else {
            return;
        };
        let text: String = rows
            .iter()
            .map(|r| {
                line(&Delta {
                    url: &r.url,
                    title: &r.title,
                    visits: r.visits,
                    typed: r.typed,
                    at: r.last_visit,
                })
            })
            .collect();
        match write_private(path, &text) {
            Ok(()) => self.lines = rows.len(),
            Err(e) => eprintln!("huginn: cannot save history: {e}"),
        }
    }
}

/// Add `d` to its row in `rows` (a new row if there is none).
fn add(rows: &mut Vec<Row>, d: &Delta) {
    let i = match rows.iter().position(|r| r.url == d.url) {
        Some(i) => i,
        None => {
            rows.push(Row {
                url: d.url.to_owned(),
                title: String::new(),
                visits: 0,
                typed: 0,
                last_visit: 0,
            });
            rows.len() - 1
        }
    };
    let row = &mut rows[i];
    row.visits = row.visits.saturating_add(d.visits);
    row.typed = row.typed.saturating_add(d.typed);
    row.last_visit = row.last_visit.max(d.at);
    if !d.title.is_empty() {
        d.title.clone_into(&mut row.title);
    }
}

/// Web pages and local files go into history; the browser's own pages,
/// `data:`, `about:` and `view-source:` do not (Chrome's `CanAddURL`).
fn recordable(url: &str) -> bool {
    ["http://", "https://", "file://"]
        .iter()
        .any(|s| url.starts_with(s))
        && url.len() <= MAX_URL
}

fn clean_title(title: &str) -> String {
    let t = title.trim();
    let mut end = t.len().min(MAX_TITLE);
    while !t.is_char_boundary(end) {
        end -= 1;
    }
    t[..end].replace(['\t', '\n', '\r'], " ")
}

fn line(d: &Delta) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\n",
        d.at, d.visits, d.typed, d.url, d.title
    )
}

/// The rows in `path` (the sum of its lines per URL), without expired ones
/// and capped, and the number of lines read.
fn read(path: &Path, now: u64) -> (Vec<Row>, usize) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (Vec::new(), 0);
    };
    let mut rows: Vec<Row> = Vec::new();
    let mut at: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut lines = 0;
    for l in text.lines() {
        lines += 1;
        let mut f = l.splitn(5, '\t');
        let (Some(time), Some(visits), Some(typed), Some(url)) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        let (Ok(time), Ok(visits), Ok(typed)) = (
            time.parse::<u64>(),
            visits.parse::<u32>(),
            typed.parse::<u32>(),
        ) else {
            continue;
        };
        let title = f.next().unwrap_or("");
        let i = *at.entry(url).or_insert_with(|| {
            rows.push(Row {
                url: url.to_owned(),
                title: String::new(),
                visits: 0,
                typed: 0,
                last_visit: 0,
            });
            rows.len() - 1
        });
        let row = &mut rows[i];
        row.visits = row.visits.saturating_add(visits);
        row.typed = row.typed.saturating_add(typed);
        row.last_visit = row.last_visit.max(time);
        if !title.is_empty() {
            title.clone_into(&mut row.title);
        }
    }
    rows.retain(|r| now.saturating_sub(r.last_visit) <= EXPIRE_MS && recordable(&r.url));
    if rows.len() > MAX_ROWS {
        rows.sort_unstable_by(|a, b| b.last_visit.cmp(&a.last_visit));
        rows.truncate(MAX_ROWS);
    }
    (rows, lines)
}

/// A missing profile directory is created owner-only.
fn make_dir(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
        && !dir.exists()
    {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(dir)?;
    }
    Ok(())
}

/// Append `text` to `path`, created readable by the owner only (an existing
/// file is made owner-only too; a symlink is refused).
fn append_private(path: &Path, text: &str) -> std::io::Result<()> {
    make_dir(path)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(o_nofollow());
    }
    let mut f = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if f.metadata()?.permissions().mode() & 0o077 != 0 {
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
    f.write_all(text.as_bytes())
}

/// Write `text` to `path` readable by the owner only: a new, uniquely named
/// temporary file (never an existing file or a symlink), then a rename.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    make_dir(path)?;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(o_nofollow());
    }
    let mut f = opts.open(&tmp)?;
    let r = (|| {
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// `O_NOFOLLOW` without a libc dependency.
#[cfg(unix)]
fn o_nofollow() -> i32 {
    if cfg!(target_os = "linux") {
        0o400000
    } else {
        0x0100
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("huginn-history-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("profile").join("history.tsv")
    }

    const DAY: u64 = 24 * 3600 * 1000;

    #[test]
    fn visits_count_by_transition_and_survive_a_restart() {
        let path = temp("round");
        let now = now_ms();
        let mut h = History::persistent(path.clone());
        let a = u("https://a.example/");
        h.record(&a, "A", Transition::Typed, now);
        h.record(&a, "", Transition::Link, now + 1);
        h.record(&a, "A2", Transition::Reload, now + 2);
        h.record(&a, "A3", Transition::BackForward, now + 3);
        h.record(&u("huginn://newtab/"), "New Tab", Transition::Typed, now);
        h.record(
            &u("view-source:https://a.example/"),
            "",
            Transition::Link,
            now,
        );
        h.record(
            &u("https://b.example/x\ty"),
            "B\ttab\nline",
            Transition::Link,
            now,
        );
        let want_a = Row {
            url: a.to_string(),
            title: "A3".into(),
            visits: 3,
            typed: 1,
            last_visit: now + 3,
        };
        assert_eq!(h.get(&a), Some(&want_a));
        assert_eq!(h.len(), 2);

        let mut back = History::persistent(path.clone());
        assert_eq!(back.get(&a), Some(&want_a));
        let b = back.get(&u("https://b.example/xy")).expect("b").clone();
        assert_eq!((b.title.as_str(), b.visits, b.typed), ("B tab line", 1, 0));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
        back.clear();
        assert!(!path.exists());
        assert_eq!(History::persistent(path.clone()).len(), 0);
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn old_rows_expire_and_the_row_count_is_capped() {
        let path = temp("caps");
        let now = now_ms();
        let mut h = History::persistent(path.clone());
        h.record(
            &u("https://old.example/"),
            "",
            Transition::Typed,
            now - 91 * DAY,
        );
        h.record(
            &u("https://recent.example/"),
            "",
            Transition::Link,
            now - 89 * DAY,
        );
        let mut back = History::persistent(path.clone());
        back.load(now);
        assert_eq!(back.get(&u("https://old.example/")), None);
        assert!(back.get(&u("https://recent.example/")).is_some());

        let mut h = History::in_memory();
        for i in 0..MAX_ROWS + 5 {
            h.record(
                &u(&format!("https://h{i}.example/")),
                "",
                Transition::Link,
                now + i as u64,
            );
        }
        assert_eq!(h.len(), MAX_ROWS);
        // The least recently visited went first.
        assert_eq!(h.get(&u("https://h4.example/")), None);
        assert!(h.get(&u("https://h5.example/")).is_some());
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn a_visit_appends_without_reading_and_stale_lines_are_rewritten() {
        let path = temp("compact");
        let lines = || std::fs::read_to_string(&path).unwrap().lines().count();
        let a = u("https://a.example/");
        let now = now_ms();
        let n = 3 * COMPACT_SLACK;
        let mut h = History::persistent(path.clone());
        for i in 0..n {
            h.record(&a, "A", Transition::Link, now + i as u64);
        }
        assert!(h.rows.is_none(), "recording read the file");
        assert_eq!(lines(), n);
        // Reading sums the lines and writes one line per row.
        let mut back = History::persistent(path.clone());
        assert_eq!(back.get(&a).map(|r| r.visits), Some(n as u32));
        assert_eq!(lines(), 1);
        // Once read, the file is rewritten as stale lines pile up.
        for i in 0..n {
            back.record(&a, "A", Transition::Typed, now + i as u64);
        }
        assert!(lines() <= COMPACT_SLACK + 3, "{} lines", lines());
        let mut again = History::persistent(path.clone());
        let r = again.get(&a).unwrap();
        assert_eq!((r.visits, r.typed), (2 * n as u32, n as u32));
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }
}
