//! Stylesheets parsed on other threads ([`huginn_style::parse_stylesheet`]),
//! the process-wide cache of parsed sheets, and repeated sheets in one
//! cascade ([`StyleEngine::add_parsed_stylesheets`]). Generated CSS only.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};

use huginn_dom::{Document, NodeId, parse_html};
use huginn_style::{
    ComputedStyle, NoImports, Origin, ParsedStylesheet, PseudoElement, StyleEngine, StyleMap,
    StylesheetLoader, Viewport, parse_stylesheet,
};

/// Tests that check which parses the cache shares take this, so the
/// eviction test cannot evict a sheet between their two parses.
fn cache_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn engine() -> StyleEngine {
    StyleEngine::new(Viewport {
        width: 1280.0,
        height: 800.0,
        device_pixel_ratio: 1.0,
    })
}

type Styles = Vec<(
    Option<ComputedStyle>,
    Option<ComputedStyle>,
    Option<ComputedStyle>,
)>;

/// Every node's style and `::before`/`::after` styles, by node index.
fn all_styles(doc: &Document, map: &StyleMap) -> Styles {
    (0..doc.len())
        .map(NodeId::from_index)
        .map(|id| {
            (
                map.get(id).map(|s| (**s).clone()),
                map.pseudo(id, PseudoElement::Before).map(|s| (**s).clone()),
                map.pseudo(id, PseudoElement::After).map(|s| (**s).clone()),
            )
        })
        .collect()
}

/// A small deterministic generator (xorshift).
struct Rng(u64);

impl Rng {
    fn next(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// 30 sections of 30 elements with overlapping classes.
fn page() -> String {
    let mut body = String::new();
    for s in 0..30 {
        body.push_str(&format!("<section class='s{s}' id=sec{s}>"));
        for e in 0..30 {
            let tag = ["p", "div", "span", "a", "li"][e % 5];
            body.push_str(&format!(
                "<{tag} class='c{} k{}' data-n={e}>t<em class=c{}>x</em></{tag}>",
                (s * 7 + e) % 60,
                e % 7,
                e % 13
            ));
        }
        body.push_str("</section>");
    }
    format!("<!doctype html><html><body>{body}</body></html>")
}

/// Sheet `i` of a set: rules on the page's classes that overlap the other
/// sheets' (so order decides), with `!important`, custom properties,
/// `@media`, `@supports`, nesting, generated content, and (every fifth
/// sheet) a named layer. About `bytes` long.
fn sheet(i: u64, bytes: usize) -> String {
    let mut r = Rng(0x9e37_79b9_7f4a_7c15 ^ (i + 1));
    let mut css = String::new();
    if i % 5 == 3 {
        css.push_str(&format!("@layer base, l{i};\n"));
    }
    let open = css.len();
    while css.len() - open < bytes {
        let a = r.next(60);
        let sel = match r.next(8) {
            0 => format!(".c{a}"),
            1 => format!(".s{} .c{a}", r.next(30)),
            2 => format!(".k{} > em", r.next(7)),
            3 => format!("p:nth-child({}n+{})", r.next(4) + 1, r.next(3)),
            4 => format!(".c{a}:not(.k{})", r.next(7)),
            5 => format!("#sec{} [data-n=\"{}\"]", r.next(30), r.next(30)),
            6 => format!(".c{a}::before"),
            _ => format!("section:has(.c{a}) .k{}", r.next(7)),
        };
        let mut decls = format!(
            "color: rgb({i}, {}, {}); margin-left: {}px; --v{}: {}px",
            r.next(256),
            r.next(256),
            r.next(40),
            r.next(5),
            r.next(30)
        );
        match r.next(6) {
            0 => decls.push_str(&format!("; font-size: {}px !important", 8 + r.next(20))),
            1 => decls.push_str("; padding-left: calc(var(--v1, 3px) * 2)"),
            2 => decls.push_str(&format!(
                "; display: {}",
                ["block", "flex", "inline-block"][r.next(3) as usize]
            )),
            3 => decls.push_str(&format!("; content: \"{i}\"; width: {}em", r.next(9))),
            4 => decls.push_str(&format!("; & em {{ border-left: {}px solid }}", r.next(5))),
            _ => decls.push_str(&format!("; background-color: #{:06x}", r.next(0xff_ffff))),
        }
        let rule = format!("{sel} {{ {decls} }}\n");
        match r.next(10) {
            0 => css.push_str(&format!(
                "@media (min-width: {}px) {{ {rule} }}\n",
                600 + r.next(1400)
            )),
            1 => css.push_str(&format!("@supports (display: grid) {{ {rule} }}\n")),
            _ => css.push_str(&rule),
        }
    }
    if i % 5 == 3 {
        css.insert_str(open, &format!("@layer l{i} {{\n"));
        css.push_str("}\n");
    }
    css
}

/// Parse `css` as sheet `i` of `set` (a URL of its own per set, so sets do
/// not share parses).
fn parse(set: &str, i: usize, css: &str) -> ParsedStylesheet {
    parse_stylesheet(
        css,
        &format!("https://t.test/{set}/{i}.css"),
        Origin::Author,
        &NoImports,
    )
}

fn styles_with(doc: &Document, sheets: &[ParsedStylesheet]) -> Styles {
    let mut e = engine();
    e.add_parsed_stylesheets(sheets);
    all_styles(doc, &e.compute(doc))
}

#[test]
fn sheets_parsed_on_threads_out_of_order_style_like_serial_parsing() {
    const N: usize = 20;
    let doc = parse_html(&page());
    let sheets: Arc<Vec<String>> = Arc::new((0..N as u64).map(|i| sheet(i, 40_000)).collect());

    // Serial: one engine, sheets parsed one after the other on this thread.
    let mut serial = engine();
    for (i, css) in sheets.iter().enumerate() {
        serial.add_stylesheet(
            css,
            &format!("https://t.test/serial/{i}.css"),
            Origin::Author,
            &NoImports,
        );
    }
    let expected = all_styles(&doc, &serial.compute(&doc));

    // Threads: sheet i is parsed on a thread of its own once sheet i + 1
    // is done, so they come in last first, as when the last link's
    // response is the first to arrive.
    let (done_tx, done_rx) = mpsc::channel();
    let mut release = Vec::new();
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let (tx, rx) = mpsc::channel::<()>();
            release.push(tx);
            let (sheets, done) = (sheets.clone(), done_tx.clone());
            std::thread::spawn(move || {
                rx.recv().unwrap();
                let parsed = parse("threads", i, &sheets[i]);
                done.send(i).unwrap();
                parsed
            })
        })
        .collect();
    release[N - 1].send(()).unwrap();
    let mut order = Vec::new();
    for _ in 0..N {
        let i = done_rx.recv().unwrap();
        order.push(i);
        if i > 0 {
            release[i - 1].send(()).unwrap();
        }
    }
    assert_eq!(order, (0..N).rev().collect::<Vec<_>>());
    let parsed: Vec<ParsedStylesheet> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let got = styles_with(&doc, &parsed);
    assert_eq!(got.len(), expected.len());
    let styled = expected.iter().filter(|s| s.0.is_some()).count();
    assert!(styled > 900, "only {styled} elements styled");
    for (n, (g, e)) in got.iter().zip(&expected).enumerate() {
        assert!(g == e, "node {n} styled differently");
    }
}

#[test]
fn repeated_sheets_style_like_separate_copies() {
    let _cache = cache_lock();
    let doc = parse_html(&page());
    // 3 is layered (`@layer base, l3` and a layer block), 1 and 2 are not.
    let texts = [sheet(1, 6_000), sheet(3, 6_000), sheet(2, 6_000)];
    let order = [0, 1, 0, 2, 0, 1, 2, 0];
    // The same text at the same URL is one parse.
    let shared: Vec<ParsedStylesheet> = order
        .iter()
        .map(|&t| parse("repeat", t, &texts[t]))
        .collect();
    assert!(shared[0].same_parse(&shared[2]) && shared[1].same_parse(&shared[5]));
    // Each place a parse of its own (a URL of its own).
    let copies: Vec<ParsedStylesheet> = order
        .iter()
        .enumerate()
        .map(|(place, &t)| parse("copies", place, &texts[t]))
        .collect();
    assert!(!copies[0].same_parse(&copies[2]));
    assert!(styles_with(&doc, &shared) == styles_with(&doc, &copies));
}

fn color_of(sheets: &[&str], html: &str) -> huginn_style::Color {
    let doc = parse_html(html);
    let parsed: Vec<ParsedStylesheet> = sheets
        .iter()
        .map(|css| parse_stylesheet(css, "https://t.test/layers.css", Origin::Author, &NoImports))
        .collect();
    let mut e = engine();
    e.add_parsed_stylesheets(&parsed);
    let map = e.compute(&doc);
    map.get(doc.element_by_id("p").unwrap()).unwrap().color
}

#[test]
fn a_repeated_sheet_keeps_the_layer_order_of_its_first_place() {
    use huginn_style::Color;
    let html = "<p id=p>x</p>";
    // Layer `a` is declared first (by the first X), so `b` wins. Without
    // the first X, `b` would come first and `a` win.
    let x = "@layer a { p { color: rgb(255, 0, 0) } }";
    let y = "@layer b { p { color: rgb(0, 0, 255) } }";
    assert_eq!(color_of(&[x, y, x], html), Color::rgb(0, 0, 255));
    // An anonymous layer is a layer of its own at each place: the first
    // X's layer comes before `m`, and for `!important` the earlier layer
    // wins.
    let x = "@layer { p { color: rgb(255, 0, 0) !important } }";
    let y = "@layer m { p { color: rgb(0, 0, 255) !important } }";
    assert_eq!(color_of(&[x, y, x], html), Color::rgb(255, 0, 0));
    // Plain repeats: the last place wins over what came between.
    let x = "p { color: rgb(255, 0, 0) }";
    let y = "p { color: rgb(0, 0, 255) }";
    assert_eq!(color_of(&[x, y, x], html), Color::rgb(255, 0, 0));
    assert_eq!(color_of(&[y, x, y, x, y], html), Color::rgb(0, 0, 255));
}

/// Counts `@import` loads.
struct Counting(AtomicUsize);

impl StylesheetLoader for Counting {
    fn load(&self, url: &str) -> Option<(String, String)> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Some(("p { color: rgb(0, 128, 0) }".into(), url.into()))
    }
}

#[test]
fn parsed_sheets_are_shared_until_the_text_or_url_changes() {
    let _cache = cache_lock();
    let css = sheet(7, 4_000);
    let a = parse("cache", 0, &css);
    // A second load, another tab, a restyle: the same parse.
    assert!(parse("cache", 0, &css).same_parse(&a));
    // The body changed: parsed again.
    let changed = format!("{css}\n.c1 {{ color: red }}");
    assert!(!parse("cache", 0, &changed).same_parse(&a));
    // Another URL (another base for `url()`): parsed again.
    assert!(!parse("cache", 1, &css).same_parse(&a));
    // A sheet with `@import` is not kept: its imports are fetched again.
    let loader = Counting(AtomicUsize::new(0));
    let url = "https://t.test/cache/import.css";
    let with_import = "@import url(b.css); p { margin: 1px }";
    let first = parse_stylesheet(with_import, url, Origin::Author, &loader);
    let second = parse_stylesheet(with_import, url, Origin::Author, &loader);
    assert!(!first.same_parse(&second));
    assert_eq!(loader.0.load(Ordering::Relaxed), 2);
}

#[test]
fn the_cache_drops_the_least_recently_used_sheets_past_its_budget() {
    let _cache = cache_lock();
    let kept = parse("budget", 0, "p { color: red }");
    let dropped = parse("budget", 1, "p { color: blue }");
    // 12 MB of other sheets (comments parse fast), using `kept` between.
    let filler = format!("/*{}*/", "x".repeat(1 << 20));
    for i in 0..12 {
        parse("filler", i, &filler);
        assert!(parse("budget", 0, "p { color: red }").same_parse(&kept));
    }
    assert!(!parse("budget", 1, "p { color: blue }").same_parse(&dropped));
}

#[test]
fn passes_on_several_threads_at_once_give_the_same_styles() {
    // Tabs style on threads of their own; big documents share the style
    // threads, one pass at a time, and the others style on their own thread.
    let html = page();
    let texts: Vec<String> = (0..6).map(|i| sheet(i, 20_000)).collect();
    let sheets = |texts: &[String]| -> Vec<ParsedStylesheet> {
        texts
            .iter()
            .enumerate()
            .map(|(i, t)| parse("concurrent", i, t))
            .collect()
    };
    let expected = styles_with(&parse_html(&html), &sheets(&texts));
    let threads: Vec<_> = (0..6)
        .map(|_| {
            let (html, texts, expected) = (html.clone(), texts.clone(), expected.clone());
            std::thread::spawn(move || {
                let doc = parse_html(&html);
                for round in 0..10 {
                    let got = styles_with(&doc, &sheets(&texts));
                    if let Some(n) = (0..got.len()).find(|&n| got[n] != expected[n]) {
                        let (a, b) = (format!("{:#?}", expected[n]), format!("{:#?}", got[n]));
                        let diff: Vec<String> = a
                            .lines()
                            .zip(b.lines())
                            .filter(|(x, y)| x != y)
                            .take(8)
                            .map(|(x, y)| format!("{} -> {}", x.trim(), y.trim()))
                            .collect();
                        panic!("round {round}: node {n} differs: {diff:#?}");
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}
