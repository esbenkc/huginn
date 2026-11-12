//! Responsive images: `srcset` parsing, `sizes` evaluation and candidate choice.

use crate::media::{Screen, media_matches, parse_length, split_top_level};

/// One `srcset` entry. `density` is resolved later for `w` descriptors.
#[derive(Clone, Debug, PartialEq)]
pub struct SrcsetCandidate<'a> {
    pub url: &'a str,
    pub descriptor: Descriptor,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Descriptor {
    /// `640w`: the image's width in px.
    Width(f32),
    /// `2x`, or no descriptor (`1x`).
    Density(f32),
}

/// Parse a `srcset` attribute (HTML spec "parse a srcset attribute").
/// Candidates with invalid descriptors are dropped.
pub fn parse_srcset(s: &str) -> Vec<SrcsetCandidate<'_>> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        while pos < bytes.len() && (bytes[pos].is_ascii_whitespace() || bytes[pos] == b',') {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        let start = pos;
        while pos < bytes.len() && !bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        let mut url = &s[start..pos];
        let mut descriptors = "";
        if url.ends_with(',') {
            url = url.trim_end_matches(',');
        } else {
            // Descriptors run to the next comma outside parentheses.
            let dstart = pos;
            let mut depth = 0;
            while pos < bytes.len() {
                match bytes[pos] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    b',' if depth <= 0 => break,
                    _ => {}
                }
                pos += 1;
            }
            descriptors = &s[dstart..pos];
        }
        if url.is_empty() {
            continue;
        }
        if let Some(descriptor) = parse_descriptors(descriptors) {
            out.push(SrcsetCandidate { url, descriptor });
        }
    }
    out
}

fn parse_descriptors(d: &str) -> Option<Descriptor> {
    let mut result = None;
    for tok in d.split_ascii_whitespace() {
        let (last, _) = tok.char_indices().last()?;
        let (num, unit) = tok.split_at(last);
        let v: f32 = num
            .parse()
            .ok()
            .filter(|v: &f32| v.is_finite() && *v > 0.0)?;
        let next = match unit {
            "w" | "W" => Descriptor::Width(v),
            "x" | "X" => Descriptor::Density(v),
            "h" | "H" => continue, // future-compat height descriptor; ignored
            _ => return None,
        };
        if result.is_some() {
            return None; // two width or density descriptors
        }
        result = Some(next);
    }
    Some(result.unwrap_or(Descriptor::Density(1.0)))
}

/// Evaluate a `sizes` attribute to the slot width in CSS px. `None` for
/// `auto` or when nothing matches and there is no default: callers then use
/// the layout width they know (or the viewport width).
pub fn eval_sizes(sizes: &str, screen: &Screen) -> Option<f32> {
    for entry in split_top_level(sizes, b',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if entry.eq_ignore_ascii_case("auto") {
            return None;
        }
        // The length is the last component; everything before it is the condition.
        let (cond, len) = match entry.ends_with(')') {
            // `calc(...)`/`min(...)` length: find its function name start.
            true => {
                let open = matching_open_paren(entry)?;
                let fstart = entry[..open]
                    .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                    .map_or(0, |i| i + 1);
                if fstart == 0 && entry.starts_with('(') {
                    // A bare media condition without a length: invalid entry.
                    continue;
                }
                (&entry[..fstart], &entry[fstart..])
            }
            false => match entry.rfind(|c: char| c.is_ascii_whitespace()) {
                Some(i) => (&entry[..i], &entry[i + 1..]),
                None => ("", entry),
            },
        };
        let cond = cond.trim();
        if cond.is_empty() || media_matches(cond, screen) {
            if let Some(px) = parse_length(len, screen) {
                return Some(px.max(0.0));
            }
        }
    }
    None
}

fn matching_open_paren(s: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, b) in s.bytes().enumerate().rev() {
        match b {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Pick the candidate for `device_pixel_ratio` when the image slot is
/// `slot_width` CSS px wide: the smallest one whose density is at least the
/// ratio, or the densest one if none is dense enough.
pub fn choose_candidate<'a>(
    candidates: &[SrcsetCandidate<'a>],
    slot_width: f32,
    device_pixel_ratio: f32,
) -> Option<&'a str> {
    let density = |c: &SrcsetCandidate<'_>| match c.descriptor {
        Descriptor::Density(x) => x,
        Descriptor::Width(w) => w / slot_width.max(1.0),
    };
    let mut best: Option<(&'a str, f32)> = None;
    for c in candidates {
        let d = density(c);
        best = match best {
            None => Some((c.url, d)),
            Some((_, bd)) if bd < device_pixel_ratio && d > bd => Some((c.url, d)),
            Some((_, bd)) if d >= device_pixel_ratio && d < bd => Some((c.url, d)),
            keep => keep,
        };
    }
    best.map(|(u, _)| u)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESKTOP: Screen = Screen {
        width: 1280.0,
        height: 800.0,
        device_pixel_ratio: 1.0,
    };

    #[test]
    fn parses_candidates_and_descriptors() {
        let c = parse_srcset(" a.jpg 320w, b.jpg 640w,c.jpg 1280w ,d.jpg");
        let urls: Vec<_> = c.iter().map(|c| c.url).collect();
        assert_eq!(urls, ["a.jpg", "b.jpg", "c.jpg", "d.jpg"]);
        assert_eq!(c[1].descriptor, Descriptor::Width(640.0));
        assert_eq!(c[3].descriptor, Descriptor::Density(1.0));
        let c = parse_srcset("x.png 1.5x, y.png 2x, bad.png 2q, two.png 1x 2x, z.png,");
        let urls: Vec<_> = c.iter().map(|c| c.url).collect();
        assert_eq!(urls, ["x.png", "y.png", "z.png"]);
        // Commas inside URLs (data: or CDN parameters) do not split.
        let c = parse_srcset(
            "https://i.example/w_200,h_100/p.jpg 200w, https://i.example/w_400,h_200/p.jpg 400w",
        );
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].url, "https://i.example/w_400,h_200/p.jpg");
    }

    #[test]
    fn sizes_pick_the_first_matching_condition() {
        assert_eq!(
            eval_sizes("(max-width: 600px) 100vw, 50vw", &DESKTOP),
            Some(640.0)
        );
        assert_eq!(
            eval_sizes("(min-width: 1024px) 300px, 100vw", &DESKTOP),
            Some(300.0)
        );
        assert_eq!(
            eval_sizes("(min-width: 1024px) calc(50vw - 40px), 100vw", &DESKTOP),
            Some(600.0)
        );
        assert_eq!(eval_sizes("calc(100vw - 2rem)", &DESKTOP), Some(1248.0));
        assert_eq!(eval_sizes("auto, 100vw", &DESKTOP), None);
        assert_eq!(eval_sizes("(max-width: 10px) 5px", &DESKTOP), None);
    }

    #[test]
    fn choice_is_the_smallest_dense_enough_candidate() {
        let c = parse_srcset("s.jpg 320w, m.jpg 640w, l.jpg 1280w");
        assert_eq!(choose_candidate(&c, 300.0, 1.0), Some("s.jpg"));
        assert_eq!(choose_candidate(&c, 500.0, 1.0), Some("m.jpg"));
        assert_eq!(choose_candidate(&c, 500.0, 2.0), Some("l.jpg"));
        assert_eq!(
            choose_candidate(&c, 2000.0, 1.0),
            Some("l.jpg"),
            "densest when none is enough"
        );
        let x = parse_srcset("a.png, b.png 2x");
        assert_eq!(choose_candidate(&x, 100.0, 1.0), Some("a.png"));
        assert_eq!(choose_candidate(&x, 100.0, 2.0), Some("b.png"));
        assert_eq!(choose_candidate(&[], 100.0, 1.0), None);
    }
}
