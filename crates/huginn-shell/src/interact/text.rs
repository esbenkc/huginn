//! Text positions over the layout tree's text index: point to text offset,
//! word and paragraph ranges, highlight rectangles of a range, find.
//!
//! Positions are byte offsets into [`TextIndex::text`] (always on a char
//! boundary). Non-empty runs are in text order, so a range maps to a
//! contiguous slice of runs.

use std::ops::Range;

use huginn_layout::{LayoutTree, Rect, TextIndex};

/// The text offset nearest to document point (`x`, `y`): the run on the
/// nearest line (vertically, then horizontally), then the cluster edge
/// nearest to `x`.
pub fn position_at(tree: &LayoutTree, x: f32, y: f32) -> Option<u32> {
    let index = tree.text_index.as_ref()?;
    let mut best: Option<((f32, f32), usize)> = None;
    for (i, r) in index.runs.iter().enumerate() {
        if r.text.is_empty() {
            continue;
        }
        let (x0, x1) = run_extent(index, i);
        let dy = if y < r.top {
            r.top - y
        } else if y >= r.bottom {
            y - r.bottom + 0.01
        } else {
            0.0
        };
        let dx = if x < x0 {
            x0 - x
        } else if x > x1 {
            x - x1
        } else {
            0.0
        };
        if best.is_none_or(|(k, _)| (dy, dx) < k) {
            best = Some(((dy, dx), i));
        }
    }
    let (_, i) = best?;
    Some(offset_in_run(index, i, x))
}

/// Whether document point (`x`, `y`) is on a line of text (for the I-beam cursor).
pub fn over_text(tree: &LayoutTree, x: f32, y: f32) -> bool {
    let Some(index) = tree.text_index.as_ref() else {
        return false;
    };
    index.runs.iter().enumerate().any(|(i, r)| {
        !r.text.is_empty() && y >= r.top && y < r.bottom && {
            let (x0, x1) = run_extent(index, i);
            x >= x0 && x < x1
        }
    })
}

/// Left and right edge of run `i`.
fn run_extent(index: &TextIndex, i: usize) -> (f32, f32) {
    index
        .clusters(i)
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), c| {
            (a.min(c.x), b.max(c.x + c.width))
        })
}

/// The cluster boundary of run `i` nearest to `x`.
fn offset_in_run(index: &TextIndex, i: usize, x: f32) -> u32 {
    let cs = index.clusters(i);
    for (k, c) in cs.iter().enumerate() {
        if x >= c.x && x < c.x + c.width {
            let rtl = cs.len() > 1 && cs[cs.len() - 1].x < cs[0].x;
            let before = (x < c.x + c.width / 2.0) != rtl;
            return if before {
                c.text
            } else {
                index.cluster_end(i, k)
            };
        }
    }
    // Outside the run: its start or end, by side and direction.
    let (x0, _) = run_extent(index, i);
    let rtl = cs.len() > 1 && cs[cs.len() - 1].x < cs[0].x;
    let r = &index.runs[i].text;
    if (x < x0) != rtl { r.start } else { r.end }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The word around `pos` (double click): a run of letters and digits, a run
/// of spaces, or a single other character. Empty at the end of the text.
pub fn word_at(text: &str, pos: u32) -> Range<u32> {
    let pos = floor_char(text, pos as usize);
    let Some(c) = text[pos..].chars().next() else {
        return pos as u32..pos as u32;
    };
    let class = |ch: char| {
        if is_word_char(ch) {
            0
        } else if ch == ' ' || ch == '\u{a0}' || ch == '\t' {
            1
        } else {
            2
        }
    };
    let k = class(c);
    if k == 2 {
        return pos as u32..(pos + c.len_utf8()) as u32;
    }
    let start = text[..pos]
        .char_indices()
        .rev()
        .take_while(|&(_, ch)| class(ch) == k)
        .last()
        .map_or(pos, |(i, _)| i);
    let end = text[pos..]
        .char_indices()
        .find(|&(_, ch)| class(ch) != k)
        .map_or(text.len(), |(i, _)| pos + i);
    start as u32..end as u32
}

/// The paragraph (block line) around `pos` (triple click), without its line feed.
pub fn paragraph_at(text: &str, pos: u32) -> Range<u32> {
    let pos = floor_char(text, pos as usize);
    let start = text[..pos].rfind('\n').map_or(0, |i| i + 1);
    let end = text[pos..].find('\n').map_or(text.len(), |i| pos + i);
    start as u32..end as u32
}

fn floor_char(text: &str, mut pos: usize) -> usize {
    pos = pos.min(text.len());
    while !text.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// Text to copy for `range`: no-break spaces become spaces.
pub fn range_text(index: &TextIndex, range: Range<u32>) -> String {
    let (a, b) = (
        floor_char(&index.text, range.start as usize),
        floor_char(&index.text, range.end as usize),
    );
    index.text[a..b.max(a)].replace('\u{a0}', " ")
}

/// Highlight rectangles for `range`: one per line piece of each run it
/// touches, cut to the run's clip (see [`run_clips`]; pieces clipped away
/// are dropped), merged where pieces on one line touch.
pub fn range_rects(tree: &LayoutTree, clips: &[Rect], range: Range<u32>) -> Vec<Rect> {
    let mut out: Vec<Rect> = Vec::new();
    let Some(index) = tree.text_index.as_ref() else {
        return out;
    };
    if range.start >= range.end {
        return out;
    }
    // Run ends never decrease (the text only grows, empty runs sit at the
    // current end), so the first run that ends after the start is a binary search.
    let first = index.runs.partition_point(|r| r.text.end <= range.start);
    for i in first..index.runs.len() {
        let r = &index.runs[i];
        if r.text.is_empty() {
            continue;
        }
        if r.text.start >= range.end {
            break;
        }
        let (mut x0, mut x1) = (f32::INFINITY, f32::NEG_INFINITY);
        for (k, c) in index.clusters(i).iter().enumerate() {
            let end = index.cluster_end(i, k);
            if c.text < range.end && end > range.start {
                x0 = x0.min(c.x);
                x1 = x1.max(c.x + c.width);
            }
        }
        if x0 >= x1 {
            continue;
        }
        let piece = Rect {
            x: x0,
            y: r.top,
            width: x1 - x0,
            height: r.bottom - r.top,
        };
        let Some(rect) = clips.get(i).map_or(Some(piece), |c| intersect(piece, *c)) else {
            continue;
        };
        match out.last_mut() {
            Some(last)
                if last.y == rect.y
                    && last.height == rect.height
                    && (rect.x - last.right()).abs() < 0.5 =>
            {
                last.width = rect.right() - last.x;
            }
            _ => out.push(rect),
        }
    }
    out
}

/// The part of `a` inside `b`, if any.
pub fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let (x0, y0) = (a.x.max(b.x), a.y.max(b.y));
    let (x1, y1) = (a.right().min(b.right()), a.bottom().min(b.bottom()));
    (x1 > x0 && y1 > y0).then_some(Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

/// Per run: the rectangle it shows in, from the overflow clips of its box
/// and the boxes around it. The root and `<body>` do not clip (their
/// overflow goes to the viewport); fixed boxes escape every clip. Text
/// hidden by a clip (screen-reader-only text, scrolled-away content) gets
/// no highlight and no find match.
pub fn run_clips(tree: &LayoutTree) -> Vec<Rect> {
    use huginn_style::{Overflow, Position};
    let all = Rect {
        x: -1e9,
        y: -1e9,
        width: 2e9,
        height: 2e9,
    };
    let mut clip = vec![all; tree.boxes.len()];
    let mut out = vec![all; tree.runs.len()];
    for (i, b) in tree.boxes.iter().enumerate() {
        let mut c = match b.parent {
            _ if b.style.position == Position::Fixed => all,
            Some(p) => clip[p.0 as usize],
            None => all,
        };
        let propagates = b.parent.is_none_or(|p| tree.get(p).parent.is_none());
        let (cx, cy) = (
            b.style.overflow_x != Overflow::Visible,
            b.style.overflow_y != Overflow::Visible,
        );
        if !propagates && (cx || cy) {
            let p = b.padding_rect();
            let own = Rect {
                x: if cx { p.x } else { all.x },
                y: if cy { p.y } else { all.y },
                width: if cx { p.width } else { all.width },
                height: if cy { p.height } else { all.height },
            };
            c = intersect(c, own).unwrap_or(Rect {
                width: 0.0,
                height: 0.0,
                ..own
            });
        }
        clip[i] = c;
        for r in b.text.start..b.text.end {
            out[r as usize] = c;
        }
    }
    out
}

/// Matches of `query` in `text`, case-insensitive (Unicode simple case
/// folding by lowercasing), at most `limit`.
pub fn find_all(text: &str, query: &str, limit: usize) -> Vec<Range<u32>> {
    let mut out = Vec::new();
    if query.is_empty() {
        return out;
    }
    if text.is_ascii() && query.is_ascii() {
        let (h, n) = (text.as_bytes(), query.as_bytes());
        if n.len() > h.len() {
            return out;
        }
        let mut i = 0;
        while i + n.len() <= h.len() && out.len() < limit {
            if h[i..i + n.len()].eq_ignore_ascii_case(n) {
                out.push(i as u32..(i + n.len()) as u32);
                i += n.len();
            } else {
                i += 1;
            }
        }
        return out;
    }
    // Lowercase both; keep the original offset of every folded byte.
    let mut folded = String::with_capacity(text.len());
    let mut map: Vec<u32> = Vec::with_capacity(text.len() + 1);
    for (i, c) in text.char_indices() {
        for l in c.to_lowercase() {
            let before = folded.len();
            folded.push(l);
            map.extend(std::iter::repeat_n(i as u32, folded.len() - before));
        }
    }
    map.push(text.len() as u32);
    let q: String = query.chars().flat_map(char::to_lowercase).collect();
    let mut from = 0;
    while out.len() < limit
        && let Some(k) = folded[from..].find(&q)
    {
        let (a, b) = (from + k, from + k + q.len());
        // The match ends where the original char of its last byte ends.
        let end = map[b..]
            .iter()
            .copied()
            .find(|&o| o > map[b - 1])
            .unwrap_or(text.len() as u32);
        out.push(map[a]..end);
        from = b;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_spaces_and_punctuation() {
        let t = "Hello, wide  world";
        let w = |p: u32| {
            let r = word_at(t, p);
            &t[r.start as usize..r.end as usize]
        };
        assert_eq!(w(0), "Hello");
        assert_eq!(w(4), "Hello");
        assert_eq!(w(5), ",");
        assert_eq!(w(6), " ");
        assert_eq!(w(12), "  ");
        assert_eq!(w(14), "world");
        assert_eq!(w(18), "");
    }

    #[test]
    fn paragraphs_end_at_line_feeds() {
        let t = "one\ntwo three\nfour";
        let p = |pos: u32| {
            let r = paragraph_at(t, pos);
            &t[r.start as usize..r.end as usize]
        };
        assert_eq!(p(0), "one");
        assert_eq!(p(6), "two three");
        assert_eq!(p(4), "two three");
        assert_eq!(p(17), "four");
    }

    #[test]
    fn find_is_case_insensitive_and_maps_back() {
        let t = "Straße STRASSE straße";
        let m = find_all(t, "straße", 10);
        let got: Vec<&str> = m
            .iter()
            .map(|r| &t[r.start as usize..r.end as usize])
            .collect();
        assert_eq!(got, ["Straße", "straße"]);
        let m = find_all("aAa", "a", 2);
        assert_eq!(m, [0..1, 1..2], "limit");
        assert_eq!(find_all("Foo foo", "FOO", 10), [0..3, 4..7]);
        assert!(find_all("x", "", 10).is_empty());
        // Non-ASCII text, ASCII query: offsets stay byte offsets of the original.
        let t = "é cat CAT";
        let m = find_all(t, "cat", 10);
        assert_eq!(
            m.iter()
                .map(|r| &t[r.start as usize..r.end as usize])
                .collect::<Vec<_>>(),
            ["cat", "CAT"]
        );
    }
}
