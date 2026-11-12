//! Box geometry, checked in-process: each page is parsed, styled and laid out,
//! then box rectangles are compared.

use huginn_dom::{Document, NodeId, local_name};
use huginn_layout::{BoxKind, DecorationShape, LayoutEngine, LayoutInput, LayoutTree, Rect, Size};
use huginn_net::{ImageStore, Url};
use huginn_style::{NoImports, Origin, StyleEngine, Viewport};

const W: f32 = 400.0;
const H: f32 = 300.0;

fn layout(html: &str) -> (Document, LayoutTree) {
    layout_with(html, &ImageStore::new())
}

fn layout_with(html: &str, images: &ImageStore) -> (Document, LayoutTree) {
    let doc = huginn_dom::parse_html(html);
    let mut engine = StyleEngine::new(Viewport {
        width: W,
        height: H,
        device_pixel_ratio: 1.0,
    });
    for n in doc.elements_by_local_name(&local_name!("style")) {
        engine.add_stylesheet(
            &doc.text_content(n),
            "about:blank",
            Origin::Author,
            &NoImports,
        );
    }
    let styles = engine.compute(&doc);
    let base = Url::parse("about:blank").unwrap();
    let tree = LayoutEngine::new().layout(&LayoutInput {
        document: &doc,
        styles: &styles,
        images,
        base_url: &base,
        viewport: Size {
            width: W,
            height: H,
        },
    });
    (doc, tree)
}

fn by_id(doc: &Document, id: &str) -> NodeId {
    doc.descendants(NodeId::DOCUMENT)
        .find(|&n| {
            doc.element(n)
                .is_some_and(|e| e.attr(&local_name!("id")) == Some(id))
        })
        .unwrap_or_else(|| panic!("no #{id}"))
}

fn rect(doc: &Document, tree: &LayoutTree, id: &str) -> Rect {
    let b = tree
        .box_for_node(by_id(doc, id))
        .unwrap_or_else(|| panic!("#{id} has no box"));
    tree.get(b).rect
}

#[test]
fn whitespace_only_generated_content_makes_no_line_box() {
    // The clearfix idiom: `content: " "` collapses away, so the pseudo-element
    // is empty and takes no height, as a block, table or inline.
    let (doc, tree) = layout(
        "<style>body{margin:0} #flex{display:flex;flex-direction:column}
        #flex:before{content:' ';display:table} #block:before{content:' '}
        #pre:before{content:' ';white-space:pre} .x{height:10px}</style>
        <div id=flex><div id=a class=x></div></div>
        <div id=block><div id=b class=x></div></div>
        <div id=pre><div id=c class=x></div></div>",
    );
    assert_eq!(
        rect(&doc, &tree, "a").y,
        0.0,
        "table pseudo in a flex column"
    );
    assert_eq!(
        rect(&doc, &tree, "b").y,
        10.0,
        "inline pseudo before a block"
    );
    // A preserved space is content: it makes a line.
    assert!(
        rect(&doc, &tree, "c").y > 25.0,
        "preserved space keeps its line"
    );
}

#[test]
fn symbol_list_markers_are_shapes_placed_like_blink() {
    // Helvetica 16px on macOS: Blink's ascent is 12 + 2 (ascent hack) = 14,
    // so the bullet is (14*2/3 + 1)/2 = 5px, starts 14*2/3 + 7 = 16px before
    // the content edge and 3*(14 - 9)/2 = 7px below the text top.
    let (doc, tree) = layout(
        "<style>body{margin:0} ul{margin:0;padding-left:40px;font:16px/20px Helvetica}
        #c{list-style-type:circle} #s{list-style-type:square} #d{list-style-type:decimal}</style>
        <ul><li id=a>one</li><li id=c>two</li><li id=s>three</li><li id=d>four</li></ul>",
    );
    let item = |id: &str| tree.box_for_node(by_id(&doc, id)).unwrap();
    for (id, shape) in [
        ("a", DecorationShape::Ellipse),
        ("c", DecorationShape::EllipseOutline),
        ("s", DecorationShape::Rect),
    ] {
        let baseline = tree.text_runs(item(id))[0].baseline;
        let [d] = tree.decorations(item(id)) else {
            panic!("#{id}: want one marker shape")
        };
        assert_eq!(d.shape, shape, "#{id}");
        assert_eq!(
            (d.rect.x, d.rect.width, d.rect.height),
            (24.0, 5.0, 5.0),
            "#{id}"
        );
        assert_eq!(d.rect.y, (baseline - 7.0).round(), "#{id}");
    }
    // Ordinal markers stay text: the item's run plus the marker's.
    assert!(tree.decorations(item("d")).is_empty());
    assert!(tree.text_runs(item("d")).len() >= 2);
}

#[test]
fn opacity_on_inline_elements_scales_their_text_alpha() {
    let (doc, tree) = layout(
        "<style>body{margin:0;color:#000}</style>
        <p id=p>a <span style='opacity:.5'>b <em style='opacity:.5'>c</em></span> d</p>",
    );
    let p = tree.box_for_node(by_id(&doc, "p")).unwrap();
    let alphas: Vec<u8> = tree.text_runs(p).iter().map(|r| r.color.a).collect();
    assert_eq!(alphas, [255, 128, 64, 255]);
}

#[test]
fn bottom_captions_go_below_the_rows_and_top_captions_above() {
    // The <figure>/<figcaption> idiom (Wikipedia thumbnails): a table whose
    // caption sits under the image. Top captions still stack first.
    let (doc, tree) = layout(
        "<style>body{margin:0} .t{display:table} .row{display:table-row;height:50px}
        .cap{display:table-caption;height:20px} .b{caption-side:bottom}</style>
        <div class=t id=t><div class='cap b' id=b1></div><div class=row id=row></div>
        <div class=cap id=top></div><div class='cap b' id=b2></div></div>",
    );
    let t = rect(&doc, &tree, "t");
    assert_eq!(
        t.height, 110.0,
        "table holds the rows and all three captions"
    );
    assert_eq!(rect(&doc, &tree, "top").y, t.y, "top caption first");
    assert_eq!(
        rect(&doc, &tree, "row").y,
        t.y + 20.0,
        "row under the top caption"
    );
    assert_eq!(
        rect(&doc, &tree, "b1").y,
        t.y + 70.0,
        "bottom captions under the row, in document order"
    );
    assert_eq!(rect(&doc, &tree, "b2").y, t.y + 90.0);
}

#[test]
fn content_url_images_are_inline_replaced_boxes_inside_the_pseudo() {
    // As in Blink: `content: url()` gives the pseudo-element an anonymous
    // inline image at its natural size, on the baseline of the pseudo's
    // line (which keeps its strut). Strings stay text; the alt text after
    // `/` is not rendered; an image that did not load shows nothing.
    let mut images = ImageStore::new();
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="18" height="12"/>"#;
    images.insert(
        &Url::parse("http://t.test/i.svg").unwrap(),
        huginn_net::decode_svg(svg, None).unwrap(),
    );
    let (doc, tree) = layout_with(
        "<!DOCTYPE html><style>body{margin:0;font:16px/30px sans-serif}
        #b::before{content:url(http://t.test/i.svg) 'x' / 'alt';display:block;margin-left:7px}
        #m::before{content:url(http://t.test/missing.svg)}</style>
        <div id=b></div><div id=m>y</div>",
        &images,
    );
    let b = tree.box_for_node(by_id(&doc, "b")).unwrap();
    let pseudo = tree.children(b).next().expect("::before box");
    let img = tree
        .children(pseudo)
        .map(|c| tree.get(c))
        .find(|lb| matches!(lb.kind, BoxKind::Image(_)))
        .expect("image box");
    assert_eq!(
        (img.rect.x, img.rect.width, img.rect.height),
        (7.0, 18.0, 12.0)
    );
    assert_eq!(
        tree.get(pseudo).rect.height,
        30.0,
        "one line of the pseudo's line-height"
    );
    let runs = tree.text_runs(pseudo);
    assert_eq!(runs.len(), 1, "only 'x', no alt text");
    assert_eq!(runs[0].glyphs.len(), 1);
    assert_eq!(
        img.rect.bottom(),
        runs[0].baseline,
        "image sits on the baseline"
    );
    assert_eq!(
        rect(&doc, &tree, "m").height,
        30.0,
        "a missing image adds nothing"
    );
}

#[test]
fn blockified_pseudo_applies_its_own_box_edges_once() {
    // The text of an absolutely positioned (blockified) ::before is inline
    // content of the pseudo's box: its margin, padding and background belong
    // to that box only, not again to an inline around the text.
    let (doc, tree) = layout(
        "<style>body{margin:0;font:16px/20px sans-serif}
        #h::before{content:'#';position:absolute;margin-left:5px;padding-left:20px;background:yellow}</style>
        <div id=h>t</div>",
    );
    let h = tree.box_for_node(by_id(&doc, "h")).unwrap();
    let pseudo = tree
        .children(h)
        .find(|&c| {
            tree.get(c).node.is_none()
                && tree.get(c).style.position == huginn_style::Position::Absolute
        })
        .expect("abspos ::before box");
    assert_eq!(tree.get(pseudo).rect.x, 5.0);
    let run = &tree.text_runs(pseudo)[0];
    assert_eq!(
        tree.glyphs[run.glyphs.start as usize].x, 25.0,
        "text at the content edge"
    );
    let yellow = huginn_style::Color::rgb(255, 255, 0);
    assert!(
        tree.fragments
            .iter()
            .all(|f| f.style.background_color != yellow),
        "no second background"
    );
}

#[test]
fn button_content_is_centered_vertically_unless_it_overflows() {
    // HTML button layout, checked against Chrome: the content box is centered
    // in a tall button and top-aligned in one too short for it; the button's
    // baseline is its content's.
    let (doc, tree) = layout(
        "<!DOCTYPE html><style>body{margin:0;font:16px/20px sans-serif}
        button{display:inline-block;padding:0;border:0;font:inherit;width:40px;height:60px} #o{height:10px}</style>
        <div id=line>a<button id=b>Go</button><button id=o>Up</button></div>",
    );
    let content = |id: &str| {
        let b = tree.box_for_node(by_id(&doc, id)).unwrap();
        let c = tree
            .children(b)
            .next()
            .expect("anonymous button content box");
        (
            tree.get(b).rect,
            tree.get(c).rect,
            tree.text_runs(c)[0].baseline,
        )
    };
    let (b, bc, b_base) = content("b");
    assert_eq!((b.y, bc.y, bc.height), (0.0, 20.0, 20.0), "centered");
    let (o, oc, o_base) = content("o");
    assert_eq!(oc.y, o.y, "overflowing content starts at the top");
    let line = tree.box_for_node(by_id(&doc, "line")).unwrap();
    let a_base = tree.text_runs(line)[0].baseline;
    assert_eq!(
        (b_base, o_base),
        (a_base, a_base),
        "buttons sit on the line's baseline"
    );
}

#[test]
fn percentage_max_width_images_do_not_widen_auto_grid_tracks() {
    // CSS Sizing 3 §5.2.1c: in a min-content contribution, a replaced box's
    // cyclic `max-width: 100%` resolves against zero. A wide responsive image
    // deep inside a grid item (block child, `width` attribute as a length, and
    // an auto-width one in a line) must not push the auto column past the grid.
    let (doc, tree) = layout(
        "<style>body{margin:0} img{max-width:100%} .b{display:block}</style>
        <div style='display:grid;width:200px'><div id=item>
          <div><a><img class=b width=300 height=150></a></div>
          <div><img style='width:auto' width=350 height=100></div>
        </div></div>",
    );
    assert_eq!(
        rect(&doc, &tree, "item").width,
        200.0,
        "the auto track fills the grid, no wider"
    );
}

#[test]
fn percentage_width_images_compress_the_min_content_of_inline_blocks() {
    // The same rule through the leaf measure: an inline-block shrinks to fit,
    // max(min-content, min(available, max-content)), and the image with
    // `width: 50%` adds nothing to its min-content.
    let (doc, tree) = layout(
        "<style>body{margin:0}</style>
        <div style='width:120px'><div id=ib style='display:inline-block'>
          <img style='display:block;width:50%' width=300 height=150></div></div>",
    );
    assert_eq!(
        rect(&doc, &tree, "ib").width,
        120.0,
        "min-content 0, so it takes the available width"
    );
}

#[test]
fn generic_families_resolve_like_desktop_chrome_on_macos() {
    // Desktop Chrome on macOS (locale_settings_mac.grd): serif is Times,
    // sans-serif Helvetica and monospace Menlo. Same text, same width as
    // the named family.
    let (doc, tree) = layout(
        "<style>body{margin:0;font-size:20px} span{display:inline-block}</style>
        <div><span id=serif style='font-family:serif'>Wasp quiz 0123</span></div>
        <div><span id=times style='font-family:Times'>Wasp quiz 0123</span></div>
        <div><span id=sans style='font-family:sans-serif'>Wasp quiz 0123</span></div>
        <div><span id=helv style='font-family:Helvetica'>Wasp quiz 0123</span></div>
        <div><span id=mono style='font-family:monospace'>Wasp quiz 0123</span></div>
        <div><span id=menlo style='font-family:Menlo'>Wasp quiz 0123</span></div>
        <div><span id=arial style='font-family:Arial'>Wasp quiz 0123</span></div>",
    );
    let w = |id| rect(&doc, &tree, id).width;
    assert_eq!(w("serif"), w("times"));
    assert_eq!(w("sans"), w("helv"));
    assert_eq!(w("mono"), w("menlo"));
    assert_ne!(
        w("arial"),
        w("helv"),
        "the fonts differ, so the test can tell"
    );
}
