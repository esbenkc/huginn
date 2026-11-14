//! A stylesheet parsed before the process has made a `StyleEngine`: the
//! shell parses sheets on fetch threads before the first page's engine
//! exists. Stylo's parser reads feature switches (`:has()`,
//! `text-overflow`, ...) that `huginn_style` turns on once per process.
//!
//! This file holds one test, so its first parse is the first thing the
//! process does with Stylo. Keep it to one test.

use huginn_dom::{Document, NodeId, parse_html};
use huginn_style::{
    Color, NoImports, Origin, ParsedStylesheet, StyleEngine, StyleMap, TextOverflow, Viewport,
    parse_stylesheet,
};

/// Each rule needs a feature that Stylo's parser has behind a switch.
const CSS: &str = "
    section:has(.on) p { color: rgb(0, 128, 0) }
    li:nth-child(odd of .x) { color: rgb(0, 0, 255) }
    p { text-overflow: ellipsis; column-count: 2; line-clamp: 3 }
";

const HTML: &str = "<!doctype html><section><b class=on></b><p id=p>t</p></section>\
                    <ul><li>a<li class=x id=li>b</ul>";

fn style(doc: &Document, sheet: &ParsedStylesheet) -> StyleMap {
    let mut e = StyleEngine::new(Viewport {
        width: 800.0,
        height: 600.0,
        device_pixel_ratio: 1.0,
    });
    e.add_parsed_stylesheets(std::slice::from_ref(sheet));
    e.compute(doc)
}

fn dump(doc: &Document, map: &StyleMap) -> Vec<Option<String>> {
    (0..doc.len())
        .map(|i| map.get(NodeId::from_index(i)).map(|s| format!("{s:#?}")))
        .collect()
}

#[test]
fn a_sheet_parsed_before_the_first_engine_parses_like_later_ones() {
    let doc = parse_html(HTML);
    // No engine has been made in this process yet.
    let first = parse_stylesheet(CSS, "https://t.test/first.css", Origin::Author, &NoImports);
    let first = style(&doc, &first);
    // The same text at another URL (a parse of its own), now that an
    // engine exists.
    let later = parse_stylesheet(CSS, "https://t.test/later.css", Origin::Author, &NoImports);
    let later = style(&doc, &later);

    let (a, b) = (dump(&doc, &first), dump(&doc, &later));
    for (n, (a, b)) in a.iter().zip(&b).enumerate() {
        if a != b {
            let (a, b) = (
                a.as_deref().unwrap_or("none"),
                b.as_deref().unwrap_or("none"),
            );
            let diff: Vec<String> = a
                .lines()
                .zip(b.lines())
                .filter(|(x, y)| x != y)
                .take(8)
                .map(|(x, y)| format!("{} -> {}", x.trim(), y.trim()))
                .collect();
            panic!("node {n}, first parse -> later parse: {diff:#?}");
        }
    }
    let p = first.get(doc.element_by_id("p").unwrap()).unwrap();
    assert_eq!(p.color, Color::rgb(0, 128, 0));
    assert_eq!(p.text_overflow, TextOverflow::Ellipsis);
    assert_eq!(p.line_clamp, Some(3));
    let li = first.get(doc.element_by_id("li").unwrap()).unwrap();
    assert_eq!(li.color, Color::rgb(0, 0, 255));
}
