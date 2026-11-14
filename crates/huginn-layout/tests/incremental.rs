//! A kept box tree must not change the result (`incr.rs`): a pass that uses
//! boxes, layout caches and shaped text from the pass before gives the same
//! tree as a pass by a fresh engine.
//!
//! The first test walks a page through changes chosen to hit what a kept box
//! can depend on from outside (floats, list numbers, tables, positioned
//! boxes, inherited decorations). The second applies random changes.

use huginn_dom::{Document, NodeId, local_name};
use huginn_layout::{LayoutEngine, LayoutInput, Size};
use huginn_net::{ImageStore, Url};
use huginn_style::{NoImports, Origin, StyleEngine, Viewport};

const STYLE: &str = "<style>
    body { margin: 8px; font: 16px sans-serif }
    p { margin: 6px 0 }
    .indent { text-indent: 3em }
    .small { font-size: 11px }
    .big { font-size: 22px }
    .red { color: #c00 }
    .u { text-decoration: underline }
    .hide { display: none }
    .ib { display: inline-block; width: 40px; height: 20px; vertical-align: middle }
    .fl { float: left; width: 90px; height: 50px; margin: 4px }
    .fr { float: right; width: 60px; height: 80px }
    .tall { height: 140px }
    .clear { clear: both }
    .flex { display: flex; gap: 6px; flex-wrap: wrap }
    .flex > * { flex: 1 1 80px }
    .grid { display: grid; grid-template-columns: 1fr 2fr; gap: 4px }
    .rel { position: relative }
    .abs { position: absolute; top: 4px; right: 4px; width: 50px }
    .pad { padding: 10px; border: 2px solid #000 }
    .bfc { overflow: hidden }
    .first { order: -1 }
    .q:before { content: '>> ' }
    .q:after { content: ' <<'; display: block }
    table { border-collapse: collapse }
    td { border: 1px solid #888; padding: 2px }
    .wide { width: 180px }
</style>";

const PAGE: &str = "
<div id=a class=rel><p id=p1>The quick brown fox <b id=b1>jumps over</b> the lazy dog, again and again, until the line wraps.</p>
  <span id=abs1 class=abs>abs box</span></div>
<div id=fl1 class=fl></div>
<p id=p2 class=indent>An indented paragraph beside a float, with <i>italic <b>and bold</b></i> text that is long enough to take more than one line here.</p>
<div id=wrap><p id=p3>Inside a plain block, still beside the float if it is tall. More text to wrap around it for a while longer.</p>
  <div id=fr1 class=fr></div><p id=p4>After a right float inside the block. Text text text text text text text text text.</p></div>
<p id=p5 class=clear>Cleared paragraph with <span class=ib></span> an inline block and a break<br>then a second line.</p>
<ol id=ol><li id=li1>First item</li><li id=li2>Second item</li><li id=li3>Third <u>item</u></li></ol>
<ul id=ul><li>Bullet one</li><li id=li5>Bullet two</li></ul>
<div id=flex class=flex><div id=f1 class=pad>flex one</div><div id=f2 class=pad>flex two with more text</div><div id=f3>flex three</div></div>
<div id=grid class=grid><div id=g1>grid one</div><div id=g2>grid two has a longer text in it</div><div id=g3>grid three</div></div>
<table id=t><tr><td id=td1>Cell one</td><td id=td2>Cell two with <b>more</b> text</td></tr><tr><td id=td3>x</td><td id=td4><p id=tp>para in cell</p></td></tr></table>
<p id=p6 class=q>Generated content around this paragraph.</p>
<div id=bfc class=bfc><div id=fl2 class=fl></div><p id=p7>Text in its own formatting context next to its own float, long enough to wrap.</p></div>
<p id=p8><button id=btn>Button</button> <input id=inp value=abc> <select id=sel><option>one<option selected>two</select></p>
<details id=det><summary>Summary</summary><p id=dp>Details content.</p></details>
<p id=p9>Last paragraph, 日本語 and עברית text.</p>";

fn by_id(doc: &Document, id: &str) -> NodeId {
    doc.descendants(NodeId::DOCUMENT)
        .find(|&n| {
            doc.element(n)
                .is_some_and(|e| e.attr(&local_name!("id")) == Some(id))
        })
        .unwrap_or_else(|| panic!("no #{id}"))
}

/// The tree as text, and one line per box (node, rectangle) to read a
/// difference by.
fn pass_both(doc: &Document, engine: &mut LayoutEngine, width: f32) -> (String, String) {
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
    let mut boxes = String::new();
    for (i, b) in tree.boxes.iter().enumerate() {
        let name = b
            .node
            .and_then(|n| doc.element(n))
            .map_or("-".to_owned(), |e| {
                format!(
                    "{}#{}.{}",
                    e.name.local,
                    e.attr(&local_name!("id")).unwrap_or(""),
                    e.attr(&local_name!("class")).unwrap_or("")
                )
            });
        let r = b.rect;
        boxes.push_str(&format!(
            "{i} {name} {:?} x{} y{} w{} h{} text{:?}\n",
            b.kind, r.x, r.y, r.width, r.height, b.text
        ));
    }
    (out, boxes)
}

fn engine(keep: bool, text_index: bool) -> LayoutEngine {
    let mut e = LayoutEngine::new();
    e.set_shape_cache(keep);
    e.set_keep_tree(keep);
    e.set_text_index(text_index);
    e
}

/// Lay out with the long-lived engine and with a fresh one, and compare.
/// The long-lived engine runs twice, so a pass that changes nothing is
/// covered at each step.
fn check(doc: &Document, kept: &mut LayoutEngine, width: f32, text_index: bool, step: &str) {
    let (fresh, fresh_boxes) = pass_both(doc, &mut engine(false, text_index), width);
    for round in 0..2 {
        let (got, got_boxes) = pass_both(doc, kept, width);
        if got != fresh {
            if let Ok(dir) = std::env::var("INCR_DUMP") {
                let body = doc.body().expect("body");
                let _ = std::fs::write(
                    format!("{dir}/doc.html"),
                    huginn_dom::serialize(doc, body, true),
                );
                let _ = std::fs::write(format!("{dir}/kept.txt"), &got_boxes);
                let _ = std::fs::write(format!("{dir}/fresh.txt"), &fresh_boxes);
            }
            let at = got
                .lines()
                .zip(fresh.lines())
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            let show = |s: &str| {
                s.lines()
                    .skip(at.saturating_sub(12))
                    .take(16)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            panic!(
                "step {step:?} (round {round}, width {width}, text index {text_index}): the kept \
                 tree gives another layout than a fresh engine, first at line {at}\n--- kept\n{}\n--- fresh\n{}",
                show(&got),
                show(&fresh)
            );
        }
    }
}

fn class(doc: &mut Document, id: &str, value: &str) {
    let n = by_id(doc, id);
    doc.set_attribute(n, local_name!("class"), value);
}

fn scripted(text_index: bool) {
    let mut doc = huginn_dom::parse_html(&format!("<!doctype html>{STYLE}{PAGE}"));
    let mut kept = engine(true, text_index);
    let ti = text_index;
    let k = &mut kept;
    check(&doc, k, 400.0, ti, "first");
    check(&doc, k, 260.0, ti, "narrower");
    check(&doc, k, 640.0, ti, "wider");
    check(&doc, k, 400.0, ti, "back");

    let p1 = by_id(&doc, "p1");
    doc.set_text_content(p1, "A new and shorter text.");
    check(&doc, k, 400.0, ti, "text changed");

    // Floats: the boxes beside and after a float are unchanged themselves.
    class(&mut doc, "fl1", "fl tall");
    check(&doc, k, 400.0, ti, "float taller");
    class(&mut doc, "fl1", "fl wide");
    check(&doc, k, 400.0, ti, "float wider");
    class(&mut doc, "fl1", "hide");
    check(&doc, k, 400.0, ti, "float gone");
    class(&mut doc, "fl1", "fl");
    check(&doc, k, 400.0, ti, "float back");
    class(&mut doc, "fr1", "fr tall");
    check(&doc, k, 400.0, ti, "inner float taller");
    class(&mut doc, "fr1", "hide");
    check(&doc, k, 320.0, ti, "inner float gone, narrower");
    class(&mut doc, "fl2", "fl tall wide");
    check(&doc, k, 400.0, ti, "float in its own context");
    let fl = doc.create_html_element("div");
    doc.set_attribute(fl, local_name!("class"), "fr");
    let body = doc.body().expect("body");
    doc.insert_before(body, fl, Some(by_id(&doc, "p2")));
    check(&doc, k, 400.0, ti, "float inserted");
    doc.detach(fl);
    check(&doc, k, 400.0, ti, "float removed");

    // List numbers depend on the items before.
    let li = doc.create_html_element("li");
    let t = doc.create_text("Zeroth item");
    doc.append_child(li, t);
    let ol = by_id(&doc, "ol");
    doc.insert_before(ol, li, Some(by_id(&doc, "li1")));
    check(&doc, k, 400.0, ti, "list item inserted first");
    doc.detach(by_id(&doc, "li2"));
    check(&doc, k, 400.0, ti, "list item removed");
    doc.set_attribute(ol, local_name!("start"), "7");
    check(&doc, k, 400.0, ti, "list start");

    // Inherited decoration reaches unchanged boxes.
    class(&mut doc, "wrap", "u");
    check(&doc, k, 400.0, ti, "underline inherited");
    class(&mut doc, "wrap", "");
    check(&doc, k, 400.0, ti, "underline gone");

    // Tables: cells change place and size.
    let td = doc.create_html_element("td");
    let t = doc.create_text("new cell");
    doc.append_child(td, t);
    let row = doc.parent(by_id(&doc, "td1")).expect("row");
    doc.insert_before(row, td, Some(by_id(&doc, "td1")));
    check(&doc, k, 400.0, ti, "cell inserted");
    let tp = by_id(&doc, "tp");
    doc.set_text_content(tp, "a much longer paragraph in the cell than before");
    check(&doc, k, 400.0, ti, "cell content changed");
    class(&mut doc, "td2", "wide");
    check(&doc, k, 300.0, ti, "cell width");

    // Flex and grid: order, sizes, wrapping.
    class(&mut doc, "f3", "first");
    check(&doc, k, 400.0, ti, "flex order");
    class(&mut doc, "f1", "pad big");
    check(&doc, k, 400.0, ti, "flex item font");
    class(&mut doc, "g2", "hide");
    check(&doc, k, 400.0, ti, "grid item hidden");
    class(&mut doc, "grid", "flex");
    check(&doc, k, 400.0, ti, "grid becomes flex");

    // Positioned boxes and their containing block.
    class(&mut doc, "a", "");
    check(&doc, k, 400.0, ti, "containing block moves out");
    class(&mut doc, "a", "rel pad");
    check(&doc, k, 400.0, ti, "containing block back, padded");
    class(&mut doc, "abs1", "");
    check(&doc, k, 400.0, ti, "no longer positioned");

    // Form controls, details, generated content.
    let inp = by_id(&doc, "inp");
    doc.set_attribute(inp, local_name!("value"), "abcdef");
    check(&doc, k, 400.0, ti, "input value");
    let det = by_id(&doc, "det");
    doc.set_attribute(det, local_name!("open"), "");
    check(&doc, k, 400.0, ti, "details open");
    class(&mut doc, "p6", "");
    check(&doc, k, 400.0, ti, "generated content gone");
    class(&mut doc, "p6", "q red");
    check(&doc, k, 400.0, ti, "generated content back, red");

    // Whole-page style changes, then nothing left.
    class(&mut doc, "p9", "big");
    check(&doc, k, 400.0, ti, "font size");
    doc.remove_children(by_id(&doc, "wrap"));
    doc.detach(by_id(&doc, "t"));
    check(&doc, k, 400.0, ti, "subtrees removed");
    doc.remove_children(body);
    check(&doc, k, 400.0, ti, "empty body");
}

#[test]
fn kept_tree_matches_fresh_passes() {
    scripted(false);
}

#[test]
fn kept_tree_matches_fresh_passes_with_the_text_index() {
    scripted(true);
}

/// A small deterministic generator (xorshift), so a failure can be replayed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const CLASSES: &[&str] = &[
    "", "", "indent", "small", "big", "red", "u", "hide", "ib", "fl", "fr", "fl tall", "clear",
    "flex", "grid", "rel", "abs", "pad", "bfc", "first", "q", "wide", "rel pad", "fr wide",
];
const TAGS: &[&str] = &["div", "p", "span", "li", "b", "div", "p"];
const WORDS: &[&str] = &[
    "alpha",
    "beta",
    "gamma",
    "delta",
    "a",
    "to",
    "longer-word",
    "日本",
    "x",
    "wrap wrap wrap",
];

fn elements(doc: &Document) -> Vec<NodeId> {
    let body = doc.body().expect("body");
    doc.descendants(body)
        .filter(|&n| doc.element(n).is_some())
        .collect()
}

fn random_change(doc: &mut Document, rng: &mut Rng) -> String {
    let body = doc.body().expect("body");
    let els = elements(doc);
    let target = if els.is_empty() {
        body
    } else {
        *rng.pick(&els)
    };
    let tag = |doc: &Document, n: NodeId| {
        doc.element(n)
            .map_or(String::new(), |e| e.name.local.to_string())
    };
    match rng.below(10) {
        0..=2 => {
            let c = *rng.pick(CLASSES);
            doc.set_attribute(target, local_name!("class"), c);
            format!("class of <{}> = {c:?}", tag(doc, target))
        }
        3..=4 => {
            // New element with text, before a random child or at the end.
            let t = *rng.pick(TAGS);
            let el = doc.create_html_element(t);
            let c = *rng.pick(CLASSES);
            doc.set_attribute(el, local_name!("class"), c);
            let words: Vec<&str> = (0..1 + rng.below(8)).map(|_| *rng.pick(WORDS)).collect();
            let text = doc.create_text(&words.join(" "));
            doc.append_child(el, text);
            let kids: Vec<NodeId> = doc.children(target).collect();
            let before = if kids.is_empty() || rng.below(3) == 0 {
                None
            } else {
                Some(*rng.pick(&kids))
            };
            // Void and replaced-like elements take no children.
            let parent = match tag(doc, target).as_str() {
                "input" | "br" | "select" | "option" | "button" | "summary" | "table" | "tr" => {
                    body
                }
                _ => target,
            };
            let before = before.filter(|_| parent == target);
            doc.insert_before(parent, el, before);
            format!("insert <{t} class={c:?}> in <{}>", tag(doc, parent))
        }
        5 => {
            if target != body {
                doc.detach(target);
            }
            format!("remove <{}>", tag(doc, target))
        }
        6..=7 => {
            // Change the text of a random text node.
            let texts: Vec<NodeId> = doc
                .descendants(body)
                .filter(|&n| doc.text(n).is_some())
                .collect();
            if texts.is_empty() {
                return "no text".into();
            }
            let n = *rng.pick(&texts);
            let words: Vec<&str> = (0..rng.below(10)).map(|_| *rng.pick(WORDS)).collect();
            let s = words.join(" ");
            doc.set_character_data(n, &s);
            format!("text = {s:?}")
        }
        8 => {
            // Move an element to another parent.
            let dest = if els.is_empty() {
                body
            } else {
                *rng.pick(&els)
            };
            let inside = doc.descendants(target).any(|n| n == dest) || dest == target;
            let leaf = matches!(
                tag(doc, dest).as_str(),
                "input" | "br" | "select" | "option" | "button" | "summary" | "table" | "tr"
            );
            if target != body && !inside && !leaf {
                doc.detach(target);
                doc.append_child(dest, target);
                format!("move <{}> into <{}>", tag(doc, target), tag(doc, dest))
            } else {
                "no move".into()
            }
        }
        _ => {
            let ols: Vec<NodeId> = els
                .iter()
                .copied()
                .filter(|&n| tag(doc, n) == "ol")
                .collect();
            if ols.is_empty() {
                return "no list".into();
            }
            let v = (rng.below(9) + 1).to_string();
            doc.set_attribute(*rng.pick(&ols), local_name!("start"), &v);
            format!("ol start = {v}")
        }
    }
}

fn fuzz(seed: u64, steps: usize) {
    let mut doc = huginn_dom::parse_html(&format!("<!doctype html>{STYLE}{PAGE}"));
    let mut kept = engine(true, true);
    let mut rng = Rng(seed);
    let widths = [400.0, 400.0, 400.0, 280.0, 620.0];
    check(&doc, &mut kept, 400.0, true, "start");
    let mut log: Vec<String> = Vec::new();
    for step in 0..steps {
        // Sometimes several changes land in one pass.
        for _ in 0..1 + rng.below(3) / 2 {
            let what = random_change(&mut doc, &mut rng);
            log.push(format!("{step}: {what}"));
        }
        let width = *rng.pick(&widths);
        let tail = log[log.len().saturating_sub(6)..].join(" | ");
        check(
            &doc,
            &mut kept,
            width,
            true,
            &format!("seed {seed} step {step}; last changes: {tail}"),
        );
    }
}

/// Three fixed seeds by default. `INCR_FUZZ=<first>..<last>` runs that range
/// of seeds instead (a longer soak), `INCR_FUZZ_STEPS` sets the steps per seed.
#[test]
fn random_changes_match_fresh_passes() {
    let steps = std::env::var("INCR_FUZZ_STEPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(80);
    let seeds: Vec<u64> = match std::env::var("INCR_FUZZ") {
        Ok(range) => {
            let (a, b) = range.split_once("..").expect("INCR_FUZZ=<first>..<last>");
            (a.parse().expect("seed")..b.parse().expect("seed")).collect()
        }
        Err(_) => vec![1, 42, 0x9e3779b97f4a7c15],
    };
    for seed in seeds {
        fuzz(seed, steps);
    }
}

/// Debugging aid: `INCR_REPLAY=<file with a body's HTML>` lays that page out
/// at a series of widths with one kept engine.
#[test]
fn replay_widths() {
    let Ok(path) = std::env::var("INCR_REPLAY") else {
        return;
    };
    let body = std::fs::read_to_string(path).expect("read");
    let doc = huginn_dom::parse_html(&format!("<!doctype html>{STYLE}{body}"));
    let mut kept = engine(true, true);
    for (i, w) in [400.0, 280.0, 620.0, 400.0, 620.0, 280.0, 400.0]
        .into_iter()
        .enumerate()
    {
        if let Ok(pat) = std::env::var("INCR_SHOW") {
            let f = pass_both(&doc, &mut engine(false, true), w).1;
            for l in f.lines().filter(|l| pat.split(',').any(|p| l.contains(p))) {
                eprintln!("fresh w{w}: {l}");
            }
        }
        check(&doc, &mut kept, w, true, &format!("replay {i}"));
    }
}
