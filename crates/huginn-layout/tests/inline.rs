//! Inline layout checked on box geometry: each page is parsed, styled and
//! laid out in-process.

use huginn_layout::{LayoutEngine, LayoutInput, LayoutTree, Rect, Size};
use huginn_net::{ImageStore, Url};
use huginn_style::{NoImports, Origin, StyleEngine, Viewport};

fn layout(html: &str) -> (huginn_dom::Document, LayoutTree) {
    let doc = huginn_dom::parse_html(html);
    let mut engine = StyleEngine::new(Viewport {
        width: 400.0,
        height: 300.0,
        device_pixel_ratio: 1.0,
    });
    for n in doc.elements_by_local_name(&huginn_dom::local_name!("style")) {
        engine.add_stylesheet(
            &doc.text_content(n),
            "about:blank",
            Origin::Author,
            &NoImports,
        );
    }
    let styles = engine.compute(&doc);
    let images = ImageStore::new();
    let base = Url::parse("about:blank").unwrap();
    let tree = LayoutEngine::new().layout(&LayoutInput {
        document: &doc,
        styles: &styles,
        images: &images,
        base_url: &base,
        viewport: Size {
            width: 400.0,
            height: 300.0,
        },
    });
    (doc, tree)
}

/// Border box of the first `tag` element.
fn rect(doc: &huginn_dom::Document, tree: &LayoutTree, tag: &str) -> Rect {
    let n = doc
        .elements_by_local_name(&huginn_dom::LocalName::from(tag))
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no <{tag}>"));
    tree.get(
        tree.box_for_node(n)
            .unwrap_or_else(|| panic!("<{tag}> has no box")),
    )
    .rect
}

const PAGE: &str = "<style>
    body { margin: 0; font: 16px/20px sans-serif }
    a { line-height: 50px }
    i { display: inline-block; width: 16px; height: 16px; vertical-align: baseline }
    </style><div><a><b><i></i></b></a></div>";

#[test]
fn text_less_inline_element_line_height_counts_in_standards_mode() {
    // An icon in a link with a tall line-height, the link holding no text.
    let (doc, tree) = layout(&format!("<!DOCTYPE html>{PAGE}"));
    let div = rect(&doc, &tree, "div");
    assert_eq!(div.height, 50.0, "line box from the link's line-height");
    // The icon sits on the baseline: below the line's top by the link's
    // half-leading plus ascent, so it is centred-ish, not at the top.
    let i = rect(&doc, &tree, "i");
    assert!(
        i.y > 10.0 && i.y + i.height < 40.0,
        "icon at y={} in a 50px line",
        i.y
    );
}

#[test]
fn inline_flex_and_inline_grid_sit_on_their_content_baseline() {
    // A 28px button whose text is centred: its baseline is the text's, so the
    // line box is the button's height, as in Chrome. With the bottom edge
    // as the baseline, the strut's descent would add about 5px below it.
    let page = "<!DOCTYPE html><style>
        body { margin: 0; font: 14px/21px sans-serif }
        a { display: inline-flex; height: 28px; align-items: center; font-size: 12px; line-height: 20px }
        s { display: inline-grid; height: 28px; align-items: center; font-size: 12px; line-height: 20px }
        </style><div><a>Notifications</a></div><p><s>Star</s></p>";
    let (doc, tree) = layout(page);
    assert_eq!(rect(&doc, &tree, "div").height, 28.0, "inline-flex line");
    assert_eq!(rect(&doc, &tree, "a").y, 0.0);
    assert_eq!(rect(&doc, &tree, "p").height, 28.0, "inline-grid line");
}

#[test]
fn text_less_inline_element_line_height_ignored_in_quirks_mode() {
    // The line height quirk: inline boxes without text do not size the line.
    let (doc, tree) = layout(PAGE);
    let div = rect(&doc, &tree, "div");
    assert!(div.height < 50.0, "quirks mode line box is {}", div.height);
}

#[test]
fn font_feature_settings_reach_the_shaper() {
    // `"kern" 0` turns off the font's pair kerning: "AV" pairs get wider.
    let width = |features: &str| {
        let (doc, tree) = layout(&format!(
            "<!DOCTYPE html><style>body{{margin:0;font:40px sans-serif}} span{{font-feature-settings:{features}}}</style>
            <p><span>AVAVAVAVAV</span></p>"
        ));
        let n = doc
            .elements_by_local_name(&huginn_dom::local_name!("span"))
            .next()
            .unwrap();
        tree.fragments_for_node(n)
            .map(|f| f.rect.width)
            .sum::<f32>()
    };
    let (kerned, unkerned) = (width("normal"), width("\"kern\" 0"));
    assert!(
        unkerned > kerned + 5.0,
        "kerning off: {unkerned} vs {kerned}"
    );
}

/// Per line (x0, x1) of the first `tag` element's inline fragments.
fn line_extents(doc: &huginn_dom::Document, tree: &LayoutTree, tag: &str) -> Vec<(f32, f32)> {
    let n = doc
        .elements_by_local_name(&huginn_dom::LocalName::from(tag))
        .next()
        .unwrap();
    tree.fragments
        .iter()
        .filter(|f| f.node == n)
        .map(|f| (f.rect.x, f.rect.x + f.rect.width))
        .collect()
}

#[test]
fn text_wrap_balance_evens_the_lines_and_keeps_the_count() {
    // Ten equal words in 250px: greedy breaks 7 + 3, balance gives 5 + 5,
    // and centered lines center in the full width, not the narrower one.
    let words = "aa aa aa aa aa aa aa aa aa aa";
    let page = |extra: &str| {
        format!(
            "<!DOCTYPE html><style>body{{margin:0}} p{{margin:0;width:250px;font:20px/20px monospace;{extra}}}</style>
            <p><span>{words}</span></p>"
        )
    };
    let (doc, tree) = layout(&page(""));
    let greedy = line_extents(&doc, &tree, "span");
    assert_eq!(greedy.len(), 2);
    assert!(
        greedy[0].1 - greedy[0].0 > 2.0 * (greedy[1].1 - greedy[1].0),
        "greedy: long first line {greedy:?}"
    );

    let (doc, tree) = layout(&page("text-wrap:balance"));
    let bal = line_extents(&doc, &tree, "span");
    assert_eq!(bal.len(), 2, "same line count");
    // The first line's fragment also holds its trailing space (one 12px
    // advance); a 6 + 4 split would differ by 96px.
    let (a, b) = (bal[0].1 - bal[0].0, bal[1].1 - bal[1].0);
    assert!(a > b && a - b < 13.0, "balanced lines {bal:?}");
    assert_eq!(bal[0].0, 0.0, "start aligned");

    let (doc, tree) = layout(&page("text-wrap:balance;text-align:center"));
    let c = line_extents(&doc, &tree, "span");
    assert!(
        (c[1].0 - (250.0 - c[1].1)).abs() < 1.0,
        "centered in 250px, not in the balanced width: {c:?}"
    );
    assert!(
        (c[0].0 - c[1].0).abs() < 1.0,
        "both lines hold 5 words: {c:?}"
    );
}

#[test]
fn inline_block_in_grid_item_sits_on_the_baseline() {
    // A grid item is measured more than once; the cached measure must keep
    // the inline-block's baseline, or it sits on the baseline with its
    // bottom edge and the line grows by the descent.
    let (doc, tree) = layout(
        "<!DOCTYPE html><style>body { margin: 0; font: 16px/24px sans-serif }
        nav { display: grid } a { display: inline-block; padding: 4px 0 }</style>
        <nav><p style='margin: 0'><a>Try it</a></p></nav>",
    );
    assert_eq!(rect(&doc, &tree, "a").height, 32.0);
    assert_eq!(
        rect(&doc, &tree, "p").height,
        32.0,
        "line box = the inline-block, no extra descent"
    );
}

#[test]
fn absolute_box_in_inline_content_takes_no_line_space() {
    // `position: absolute` inside a link: out of flow at its static
    // position, so the text after it does not move.
    let (doc, tree) = layout(
        "<!DOCTYPE html><style>body { margin: 0; font: 16px/20px sans-serif }
        u { position: absolute; width: 30px; height: 30px; margin-left: -10px }</style>
        <p><a>ab<u></u>cd</a></p><p><a>abcd</a></p>",
    );
    let ps: Vec<_> = doc
        .elements_by_local_name(&huginn_dom::local_name!("p"))
        .collect();
    let p0 = tree.get(tree.box_for_node(ps[0]).unwrap()).rect;
    let p1 = tree.get(tree.box_for_node(ps[1]).unwrap()).rect;
    assert_eq!(
        p0.height, p1.height,
        "the absolute box does not grow the line"
    );
    let a_width = |i: usize| {
        let a = doc
            .elements_by_local_name(&huginn_dom::local_name!("a"))
            .nth(i)
            .unwrap();
        tree.fragments_for_node(a)
            .map(|f| f.rect.width)
            .sum::<f32>()
    };
    assert!(
        (a_width(0) - a_width(1)).abs() < 0.5,
        "same link width with and without the box"
    );
    // Static position: right after "ab", at the top of the line, then the margin.
    let u = rect(&doc, &tree, "u");
    assert!(
        u.x > 0.0 && u.x < 16.0,
        "u at x={} (after 'ab' minus 10px)",
        u.x
    );
    assert_eq!(u.y, p0.y);
}

#[test]
fn ascii_soft_wraps_follow_chrome_not_after_slash() {
    // Ten characters fit per line. UAX #14 allows a break after the '/'
    // ("aaaa-bbbb/" + "cccc"); Chrome only breaks after the '-' between
    // printable ASCII characters here ("aaaa-" + "bbbb/cccc").
    let (doc, tree) = layout(
        "<!DOCTYPE html><style>body{margin:0} p{margin:0;width:130px;font:20px/20px monospace}</style>
        <p><span>aaaa-bbbb/cccc</span></p>",
    );
    let lines = line_extents(&doc, &tree, "span");
    assert_eq!(lines.len(), 2, "{lines:?}");
    let first = lines[0].1 - lines[0].0;
    assert!(
        first < 70.0,
        "first line is 'aaaa-' (5 of 14 characters): {lines:?}"
    );
}

fn layout_indexed(html: &str) -> LayoutTree {
    let doc = huginn_dom::parse_html(html);
    let mut engine = StyleEngine::new(Viewport {
        width: 400.0,
        height: 300.0,
        device_pixel_ratio: 1.0,
    });
    for n in doc.elements_by_local_name(&huginn_dom::local_name!("style")) {
        engine.add_stylesheet(
            &doc.text_content(n),
            "about:blank",
            Origin::Author,
            &NoImports,
        );
    }
    let styles = engine.compute(&doc);
    let images = ImageStore::new();
    let base = Url::parse("about:blank").unwrap();
    let mut layout = LayoutEngine::new();
    layout.set_text_index(true);
    layout.layout(&LayoutInput {
        document: &doc,
        styles: &styles,
        images: &images,
        base_url: &base,
        viewport: Size {
            width: 400.0,
            height: 300.0,
        },
    })
}

#[test]
fn text_index_holds_rendered_text_with_block_breaks() {
    let tree = layout_indexed(
        "<style>body { margin: 0; font: 16px/20px sans-serif } p { margin: 0 }</style>\
         <p>Hello   <a href=x>world</a>!</p><p style=\"visibility:hidden\">secret</p><ul><li>item</li></ul>\
         <p>one<br>two</p>",
    );
    let index = tree.text_index.as_ref().expect("index built");
    assert_eq!(index.text, "Hello world!\nitem\none\ntwo");
    assert_eq!(index.runs.len(), tree.runs.len(), "one entry per run");
    // Each run's clusters cover its text, and sit on its line.
    for (i, r) in index.runs.iter().enumerate() {
        let cs = index.clusters(i);
        if r.text.is_empty() {
            assert!(cs.is_empty());
            continue;
        }
        assert_eq!(cs[0].text, r.text.start);
        assert!(
            r.top < tree.runs[i].baseline && tree.runs[i].baseline < r.bottom,
            "{r:?}"
        );
        let glyphs = tree.glyphs(&tree.runs[i]);
        assert!(
            (cs[0].x - glyphs[0].x).abs() < 0.01,
            "first cluster starts at the first glyph"
        );
    }
    // "world" is a run of its own (in the link), right after "Hello ".
    let w = index.text.find("world").unwrap() as u32;
    let i = index
        .runs
        .iter()
        .position(|r| r.text.start == w)
        .expect("run for the link text");
    assert_eq!(index.cluster_end(i, 0), w + 1);
}

#[test]
fn text_index_is_off_by_default() {
    let (_, tree) = layout("<p>text</p>");
    assert!(tree.text_index.is_none());
}

#[test]
fn text_index_keeps_order_on_a_very_long_line() {
    // ~120k px of one line with a style change every two characters: cluster
    // positions that far out are not exact in f32, so runs must not be
    // matched to clusters by x.
    let body: String = (0..6000).map(|_| "<span>ab</span><b>cd</b>").collect();
    let tree = layout_indexed(&format!(
        "<div style=\"white-space:pre;font:16px monospace\">{body}</div>"
    ));
    let index = tree.text_index.as_ref().unwrap();
    assert_eq!(index.text.len(), 24000);
    assert!(
        index
            .text
            .chars()
            .collect::<Vec<_>>()
            .chunks(4)
            .all(|c| c == ['a', 'b', 'c', 'd'])
    );
}
