//! The shape cache must not change the result: a layout pass that reuses
//! shaped text from the pass before gives the same tree as a pass by a
//! fresh engine. Each step changes the page or the viewport a little, as a
//! live page does.

use huginn_dom::{Document, NodeId, local_name};
use huginn_layout::{LayoutEngine, LayoutInput, Size};
use huginn_net::{ImageStore, Url};
use huginn_style::{NoImports, Origin, StyleEngine, Viewport};

const PAGE: &str = "<!doctype html><style>
    body { margin: 8px; font: 16px sans-serif }
    p { margin: 6px 0 }
    .indent { text-indent: 3em }
    .small { font-size: 11px; color: #a00 }
    .big #first { font-size: 22px }
    .red b { color: #c00 }
    .box { display: inline-block; width: 40px; height: 20px; vertical-align: middle }
    .float { float: left; width: 90px; height: 50px }
    .balance { text-wrap: balance }
    .up { text-transform: uppercase }
    .quote:before { content: 'Quote: ' }
    .abs { position: absolute; top: 0; left: 0 }
</style>
<p id=first>The quick brown fox <b>jumps over</b> the lazy dog, again and again, until the line wraps.</p>
<p id=second class=indent>An indented paragraph with <i>italic <b>and bold</b></i> text that is long enough to take more than one line here.</p>
<p id=third>Before <span class=box></span> an inline block, a break<br>and a second line with <a href=#>a link</a>.</p>
<div class=float></div>
<p id=fourth>Text beside a float. It wraps around the float for a few lines and then goes on below it, at the full width.</p>
<p id=fifth class='balance up quote'>Balanced uppercase text with generated content in front of it.</p>
<p id=sixth>Mixed scripts: 日本語 עברית and an emoji 🎉 with <span class=abs>out of flow</span> text.</p>
<ul><li id=li1>First item</li><li id=li2>Second item</li><li>Second item</li></ul>
<table><tr><td id=td1>Cell one</td><td>Cell two with more text</td></tr></table>";

fn by_id(doc: &Document, id: &str) -> NodeId {
    doc.element_by_id(id)
        .or_else(|| {
            doc.descendants(NodeId::DOCUMENT).find(|&n| {
                doc.element(n)
                    .is_some_and(|e| e.attr(&local_name!("id")) == Some(id))
            })
        })
        .unwrap_or_else(|| panic!("no #{id}"))
}

/// Style `doc` and lay it out with `engine` at `width`; the tree as text.
fn pass(doc: &Document, engine: &mut LayoutEngine, width: f32) -> String {
    let mut style = StyleEngine::new(Viewport {
        width,
        height: 300.0,
        device_pixel_ratio: 1.0,
    });
    for n in doc.elements_by_local_name(&local_name!("style")) {
        style.add_stylesheet(
            &doc.text_content(n),
            "about:blank",
            Origin::Author,
            &NoImports,
        );
    }
    let styles = style.compute(doc);
    let base = Url::parse("about:blank").unwrap();
    let tree = engine.layout(&LayoutInput {
        document: doc,
        styles: &styles,
        images: &ImageStore::new(),
        base_url: &base,
        viewport: Size {
            width,
            height: 300.0,
        },
    });
    // Font blobs are numbered per process, so a fresh engine that loads the
    // same font file again gets another number. Leave the numbers out.
    let mut out = String::new();
    let mut in_blob = false;
    for line in format!("{tree:#?}").lines() {
        if !(in_blob && line.trim_start().starts_with("id: ")) {
            out.push_str(line);
            out.push('\n');
        }
        in_blob = line.trim_end().ends_with("Blob {");
    }
    out
}

fn engine(cache: bool, text_index: bool) -> LayoutEngine {
    let mut e = LayoutEngine::new();
    e.set_shape_cache(cache);
    e.set_text_index(text_index);
    e
}

/// Lay out with the long-lived cached engine and with a fresh engine, and
/// compare. The cached engine is also run twice, so a pass that reuses
/// everything is covered at each step.
fn check(doc: &Document, cached: &mut LayoutEngine, width: f32, text_index: bool, step: &str) {
    let fresh = pass(doc, &mut engine(false, text_index), width);
    for round in 0..2 {
        let got = pass(doc, cached, width);
        assert!(
            got == fresh,
            "step {step:?} (round {round}, text index {text_index}): the cached pass differs from a fresh one"
        );
    }
}

fn run(text_index: bool) {
    let mut doc = huginn_dom::parse_html(PAGE);
    let mut cached = engine(true, text_index);
    let ti = text_index;
    check(&doc, &mut cached, 400.0, ti, "first");
    check(&doc, &mut cached, 250.0, ti, "narrower");
    check(&doc, &mut cached, 700.0, ti, "wider");

    // Text of one paragraph changes; the others are reused.
    let first = by_id(&doc, "first");
    doc.set_text_content(first, "A new and shorter text.");
    check(&doc, &mut cached, 400.0, ti, "text changed");

    // A style change that needs new shaping (font size) on one paragraph.
    let body = doc
        .elements_by_local_name(&local_name!("body"))
        .into_iter()
        .next()
        .expect("body");
    doc.set_attribute(body, local_name!("class"), "big");
    check(&doc, &mut cached, 400.0, ti, "font size by class");

    // A color change inside spans.
    doc.set_attribute(body, local_name!("class"), "red");
    check(&doc, &mut cached, 400.0, ti, "color by class");

    // The indent goes away, then comes back at another width.
    let second = by_id(&doc, "second");
    doc.set_attribute(second, local_name!("class"), "");
    check(&doc, &mut cached, 400.0, ti, "indent removed");
    doc.set_attribute(second, local_name!("class"), "indent small");
    check(&doc, &mut cached, 300.0, ti, "indent back, smaller font");

    // A node is added, and one list item gets the text of another.
    let p = doc.create_html_element("p");
    let t = doc.create_text("An added paragraph.");
    doc.append_child(p, t);
    doc.insert_before(body, p, Some(by_id(&doc, "third")));
    let li1 = by_id(&doc, "li1");
    doc.set_text_content(li1, "Second item");
    check(&doc, &mut cached, 400.0, ti, "node added, same text twice");

    // Paragraphs are removed.
    doc.remove_children(by_id(&doc, "fourth"));
    doc.remove_children(by_id(&doc, "sixth"));
    check(&doc, &mut cached, 400.0, ti, "text removed");
}

#[test]
fn cached_passes_match_fresh_passes() {
    run(false);
}

#[test]
fn cached_passes_match_fresh_passes_with_the_text_index() {
    run(true);
}
