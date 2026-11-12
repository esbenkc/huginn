//! View source (Cmd+U, a `view-source:` tab) and save page (Cmd+S).
//! Both fetch the document again: the page does not keep its bytes.

use huginn_net::{Loader, ResourceKind, Url};

use crate::engine::escape_html;

/// The URL a `view-source:` URL shows.
pub fn view_source_target(url: &Url) -> Option<Url> {
    (url.scheme() == "view-source")
        .then(|| Url::parse(&url.as_str()["view-source:".len()..]).ok())
        .flatten()
}

/// The view-source page for `target`: its source with line numbers and
/// Chrome's colors (tags purple, attribute names brown, values blue,
/// comments green, doctype gray).
pub fn view_source_html(loader: &Loader, target: &Url) -> Result<String, String> {
    let res = loader
        .fetch_uncached(target, ResourceKind::Document)
        .map_err(|e| format!("{e:?}"))?;
    let text = res.html_text();
    let mut out = String::with_capacity(text.len() * 2 + 1024);
    out.push_str("<!doctype html><html><head><meta charset=utf-8><title>view-source:");
    out.push_str(&escape_html(target.as_str()));
    out.push_str(
        "</title><style>body{margin:0;font:13px/1.35 Menlo,monospace;color:#000;background:#fff}\
         div{display:flex;white-space:pre}\
         b{font-weight:normal;flex:none;width:52px;padding-right:10px;text-align:right;color:#999;\
         border-right:1px solid #ddd;margin-right:10px}\
         .t{color:#881280}.n{color:#994500}.v{color:#1a1aa6}.c{color:#236e25}.d{color:#c0c0c0}</style></head><body>",
    );
    for (i, line) in highlight(&text).split('\n').enumerate() {
        out.push_str(&format!("<div><b>{}</b><span>{}</span></div>", i + 1, line));
    }
    out.push_str("</body></html>");
    Ok(out)
}

/// Escape `src` and wrap its tags, attributes, comments and doctype in
/// spans. A span never crosses a line: it is closed and reopened at each
/// line feed so every line can be its own block.
fn highlight(src: &str) -> String {
    let mut out = String::with_capacity(src.len() * 2);
    let mut rest = src;
    let span = |out: &mut String, class: &str, text: &str| {
        for (k, piece) in text.split('\n').enumerate() {
            if k > 0 {
                out.push('\n');
            }
            if !piece.is_empty() {
                out.push_str(&format!(
                    "<span class={class}>{}</span>",
                    escape_html(piece)
                ));
            }
        }
    };
    while let Some(lt) = rest.find('<') {
        out.push_str(&escape_html(&rest[..lt]));
        rest = &rest[lt..];
        if rest.starts_with("<!--") {
            let end = rest.find("-->").map_or(rest.len(), |e| e + 3);
            span(&mut out, "c", &rest[..end]);
            rest = &rest[end..];
        } else if rest.len() > 1 && (rest.as_bytes()[1] == b'!' || rest.as_bytes()[1] == b'?') {
            let end = rest.find('>').map_or(rest.len(), |e| e + 1);
            span(&mut out, "d", &rest[..end]);
            rest = &rest[end..];
        } else if rest.len() > 1
            && (rest.as_bytes()[1].is_ascii_alphabetic() || rest.as_bytes()[1] == b'/')
        {
            let end = tag_end(rest);
            let name = out_tag_name(&rest[..end]);
            tag(&mut out, &rest[..end], &span);
            rest = &rest[end..];
            // Raw text elements: their content is not markup.
            if let Some(close) = ["script", "style", "textarea", "title"]
                .iter()
                .find(|&&n| n == name)
            {
                let pat = format!("</{close}");
                let stop = find_ci(rest, &pat).unwrap_or(rest.len());
                out.push_str(&escape_html(&rest[..stop]));
                rest = &rest[stop..];
            }
        } else {
            out.push_str("&lt;");
            rest = &rest[1..];
        }
    }
    out.push_str(&escape_html(rest));
    out
}

/// Lowercase name of an opening tag (`<script ...>` -> `script`); empty for end tags.
fn out_tag_name(tag: &str) -> String {
    let t = tag.strip_prefix('<').unwrap_or("");
    if t.starts_with('/') {
        return String::new();
    }
    t.chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    (0..h.len().saturating_sub(n.len() - 1)).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// End of the tag starting at `s[0] == '<'`: after the first `>` outside quotes.
fn tag_end(s: &str) -> usize {
    let mut quote: Option<u8> = None;
    for (i, &b) in s.as_bytes().iter().enumerate().skip(1) {
        match (quote, b) {
            (Some(q), _) if b == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(b),
            (None, b'>') => return i + 1,
            _ => {}
        }
    }
    s.len()
}

/// Color one tag: `<name` and `>` purple, attribute names brown, values blue.
fn tag(out: &mut String, t: &str, span: &dyn Fn(&mut String, &str, &str)) {
    let b = t.as_bytes();
    let mut i = 1;
    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' && !(b[i] == b'/' && i > 1) {
        i += 1;
    }
    span(out, "t", &t[..i]);
    loop {
        let ws = i;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        out.push_str(&t[ws..i]);
        if i >= b.len() {
            return;
        }
        if b[i] == b'>' || b[i] == b'/' {
            span(out, "t", &t[i..]);
            return;
        }
        let name = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'>' {
            i += 1;
        }
        span(out, "n", &t[name..i]);
        if i < b.len() && b[i] == b'=' {
            out.push('=');
            i += 1;
            let v = i;
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                i = (i + 1).min(b.len());
            } else {
                while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                    i += 1;
                }
            }
            span(out, "v", &t[v..i]);
        }
    }
}

/// Fetch `url` again and write its bytes to the downloads folder as
/// `<title>.html`. Returns the path.
pub fn save_page(url: &Url, title: &str) -> Result<std::path::PathBuf, String> {
    let loader = Loader::new();
    let res = loader
        .fetch_uncached(url, ResourceKind::Document)
        .map_err(|e| format!("{e:?}"))?;
    let dir = super::print::downloads_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let host = url.host_str().unwrap_or("page");
    let path = super::print::unique_path(&dir, &super::print::file_stem(title, host), "html");
    std::fs::write(&path, &res.body).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_source_urls_round_trip() {
        let u = Url::parse("https://example.com/a?b#c").unwrap();
        let v = Url::parse(&format!("view-source:{u}")).unwrap();
        assert_eq!(v.as_str(), "view-source:https://example.com/a?b#c");
        assert_eq!(view_source_target(&v), Some(u));
    }

    #[test]
    fn highlight_colors_markup_and_keeps_text() {
        let h = highlight(
            "<!doctype html>\n<a href=\"x>y\" b>t&amp;</a><!-- c\nd --><script>if (a<b) x()</script>",
        );
        assert!(
            h.starts_with("<span class=d>&lt;!doctype html&gt;</span>\n"),
            "{h}"
        );
        assert!(h.contains("<span class=t>&lt;a</span> <span class=n>href</span>=<span class=v>&quot;x&gt;y&quot;</span>"), "{h}");
        assert!(h.contains("t&amp;amp;"), "text is escaped: {h}");
        assert!(
            h.contains("<span class=c>&lt;!-- c</span>\n<span class=c>d --&gt;</span>"),
            "comments split at lines: {h}"
        );
        assert!(
            h.contains("if (a&lt;b) x()"),
            "script content is not markup: {h}"
        );
    }
}
