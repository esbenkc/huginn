//! The page-load pipeline: fetch -> parse -> scripts -> style -> layout -> paint -> PNG.
//!
//! [`load`] does everything up to layout and returns a [`Page`]. `render`
//! (headless PNG) and the `open` window share it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use huginn_dom::{Document, NodeData, NodeId, local_name};
use huginn_js::{ScriptRef, ScriptTiming};
use huginn_layout::{LayoutEngine, LayoutInput, LayoutTree, Rect, Size};
use huginn_net::{
    Client, CorsSettings, ImageStore, Loader, NetError, ReferrerPolicy, Resource, ResourceKind, Url,
};
use huginn_paint::{CpuRasterizer, Rasterizer};
use huginn_style::{Origin, StyleEngine, StyleMap, StylesheetLoader, Viewport};

use crate::{preload, resources, scripts};

pub struct RenderOptions {
    pub target: String,
    pub out: PathBuf,
    pub width: u32,
    pub height: u32,
    pub full_page: bool,
    /// Run scripts (needs the `v8` feature; otherwise scripts never run).
    pub js: bool,
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1e5).round() / 100.0
}

/// `@import` support on top of the loader.
struct Imports<'a>(&'a Loader);

impl StylesheetLoader for Imports<'_> {
    fn load(&self, url: &str) -> Option<(String, String)> {
        let url = Url::parse(url).ok()?;
        let r = self.0.fetch(&url, ResourceKind::Stylesheet).ok()?;
        Some((r.text().into_owned(), r.url.to_string()))
    }
}

/// A stylesheet source in document order. External ones carry the link's
/// `crossorigin` state: it decides the request (and which preload answers it).
#[derive(Clone, PartialEq)]
enum SheetSource {
    Inline(String),
    External(Url, CorsSettings),
}

/// A fetched, parsed, scripted and styled document, ready for layout.
pub struct Page {
    /// Document URL after redirects.
    pub url: Url,
    /// `document.referrer`: the `Referer` the document was requested with.
    pub referrer: String,
    /// `<base href>` or `url`.
    pub base: Url,
    pub doc: Document,
    pub style_engine: StyleEngine,
    pub styles: StyleMap,
    pub images: ImageStore,
    /// Layout engine that scripts already used for layout queries, if any.
    pub layout_engine: Option<LayoutEngine>,
    /// Decoded web fonts, moved into the layout engine by `register_fonts`.
    fonts: Vec<resources::WebFont>,
    /// Font files fetched so far (`load_added` loads the others text needs).
    font_urls: std::collections::HashSet<Url>,
    /// The `<img>` choices; `loading=lazy` ones load in `load_lazy`.
    img_plan: resources::ImagePlan,
    /// Stylesheets in the cascade, in document order (`restyle_dom` diffs them).
    sheets: Vec<SheetSource>,
    screen: huginn_net::Screen,
    pub js_report: scripts::ScriptReport,
    /// The page's script engine, kept alive by [`load_document`] with
    /// `live` (the `open` window runs scripts after load). `render` drops
    /// it before style, so its heap never adds to peak RSS there.
    pub js: Option<Box<dyn huginn_js::ScriptEngine>>,
    /// The window: the scripts that run after the first paint.
    pub later: Option<LaterScripts>,
    /// The window: images and web fonts still on the network at the first
    /// paint ([`Page::take_pending`]).
    pending: Option<resources::Pending>,
    pub fetch_time: Duration,
    pub parse_time: Duration,
    pub script_time: Duration,
    pub style_time: Duration,
}

impl Page {
    /// Move the page's web fonts into `layout_engine` (no copy; call after
    /// `load` and after `load_added`).
    pub fn register_fonts(&mut self, layout_engine: &mut LayoutEngine) {
        for f in std::mem::take(&mut self.fonts) {
            layout_engine.register_font(f.data, &f.family, f.weight, f.style);
        }
    }

    /// Wait for the next of the images and web fonts that were still on
    /// the network at the first paint (or `deadline`; `None`: as long as it
    /// takes) and store what came. New fonts wait for `register_fonts`.
    pub fn take_pending(&mut self, deadline: Option<Instant>) -> resources::Took {
        let Some(p) = &mut self.pending else {
            return resources::Took::default();
        };
        let took = p.take(&mut self.images, &mut self.fonts, deadline);
        if p.is_done() {
            self.pending = None;
            huginn_net::log_phase("images and fonts in");
        }
        took
    }

    /// Whether images or web fonts of the first paint are still on the network.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Tell `layout_engine` which web fonts are in their block period now
    /// (their text is invisible). True if that changed: lay out again.
    pub fn update_invisible_text(&mut self, layout_engine: &mut LayoutEngine) -> bool {
        let blocked = self
            .pending
            .as_mut()
            .map_or_else(Vec::new, |p| p.blocked_families(Instant::now()));
        layout_engine.set_invisible_families(blocked)
    }

    /// When the next web font of the first paint ends a font-display period.
    pub fn next_font_change(&self) -> Option<Instant> {
        self.pending.as_ref()?.next_font_change(Instant::now())
    }

    /// The keys `load_added` would mark as tried for the images still on
    /// the network (so it does not fetch them again).
    pub fn pending_keys(&self) -> impl Iterator<Item = &String> {
        self.pending.iter().flat_map(resources::Pending::tried_keys)
    }

    /// Load the `loading=lazy` images whose boxes intersect `region` and
    /// show them in `tree`. Returns true when the caller must lay out again
    /// (an image sets its own box size).
    pub fn load_lazy(&mut self, loader: &Loader, tree: &mut LayoutTree, region: Rect) -> bool {
        resources::load_lazy(
            loader,
            &self.doc,
            &self.styles,
            &self.img_plan,
            &self.screen,
            &mut self.images,
            tree,
            region,
        )
    }

    /// Load every `loading=lazy` image now (the window scrolls, so all of
    /// them can come into view). Returns true if any was added: lay out
    /// again.
    pub fn load_all_lazy(&mut self, loader: &Loader) -> bool {
        resources::load_all_lazy(
            loader,
            &self.doc,
            &self.styles,
            &self.img_plan,
            &self.screen,
            &mut self.images,
        )
    }

    /// Lay out for `viewport` (the styles must be computed for the same viewport).
    pub fn layout(&self, layout_engine: &mut LayoutEngine, viewport: Viewport) -> LayoutTree {
        let _span = crate::perf::span("layout");
        layout_engine.layout(&LayoutInput {
            document: &self.doc,
            styles: &self.styles,
            images: &self.images,
            base_url: &self.base,
            viewport: Size {
                width: viewport.width,
                height: viewport.height,
            },
        })
    }

    /// Recompute styles for a new viewport (media queries, vw/vh).
    pub fn restyle(&mut self, viewport: Viewport) {
        self.style_engine.set_viewport(viewport);
        let _span = crate::perf::span("style");
        self.styles = self.style_engine.compute(&self.doc);
    }

    /// Recompute styles after scripts changed the DOM, picking up the
    /// stylesheets they added or removed (as `load` does after scripts).
    pub fn restyle_dom(&mut self, loader: &Loader) {
        let sheets = collect_stylesheets(&self.doc, &self.base, &self.screen);
        if sheets != self.sheets {
            let appended = sheets.starts_with(&self.sheets);
            if !appended {
                self.style_engine = StyleEngine::new(self.style_engine.viewport());
                self.style_engine.set_document_url(self.base.as_str());
            }
            let skip = if appended { self.sheets.len() } else { 0 };
            add_sheets(&mut self.style_engine, loader, &sheets[skip..], &self.base);
            self.sheets = sheets;
        }
        let _span = crate::perf::span("style");
        self.styles = self.style_engine.compute(&self.doc);
    }

    /// Load the images, inline `<svg>`s and web fonts the current styles and
    /// DOM use but the page does not have yet (added by scripts, or by a
    /// `:checked`/`:hover` style). `tried`: URLs fetched before. True if any
    /// was added; new fonts wait for `register_fonts`.
    pub fn load_added(
        &mut self,
        loader: &Loader,
        tried: &mut std::collections::HashSet<String>,
    ) -> bool {
        let images = resources::load_added(
            loader,
            &self.doc,
            &self.styles,
            &self.base,
            &self.screen,
            &mut self.images,
            tried,
        );
        let fonts = resources::load_added_fonts(
            loader,
            &self.doc,
            &self.styles,
            self.style_engine.font_faces(),
            &mut self.font_urls,
        );
        let any_font = !fonts.is_empty();
        self.fonts.extend(fonts);
        images || any_font
    }
}

/// Script sources as the loader returns them.
type Fetched = Vec<Result<Arc<Resource>, NetError>>;

/// The async, defer and module scripts of a page in the window. They do not
/// hold up the first paint: Chrome paints before them when they are not
/// loaded yet. Their sources load on a thread of their own meanwhile.
pub struct LaterScripts {
    pub scripts: Vec<ScriptRef>,
    /// The `src` and CORS mode of the external ones, in order.
    requests: Vec<(Url, CorsSettings)>,
    fetch: Option<std::thread::JoinHandle<Fetched>>,
}

impl LaterScripts {
    /// Start fetching the sources of `scripts`.
    fn start(loader: &Arc<Loader>, scripts: Vec<ScriptRef>) -> LaterScripts {
        let requests: Vec<(Url, CorsSettings)> = scripts
            .iter()
            .filter_map(|s| Some((s.src.clone()?, s.cors)))
            .collect();
        let fetch = if requests.is_empty() {
            None
        } else {
            let (loader, requests) = (loader.clone(), requests.clone());
            std::thread::Builder::new()
                .name("huginn-scripts".into())
                .spawn(move || fetch_scripts(&loader, &requests))
                .ok()
        };
        LaterScripts {
            scripts,
            requests,
            fetch,
        }
    }

    /// Whether the sources are in ([`LaterScripts::sources`] will not wait).
    pub fn ready(&self) -> bool {
        self.fetch
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }

    /// Wait for the sources. Per script: the fetched source, `None` for
    /// inline scripts and failed fetches.
    pub fn sources(&mut self, loader: &Loader) -> Vec<Option<Arc<Resource>>> {
        let fetched = match self.fetch.take().map(std::thread::JoinHandle::join) {
            Some(Ok(f)) => f,
            // No thread could start, or it panicked: fetch them here.
            _ => fetch_scripts(loader, &self.requests),
        };
        let mut fetched = fetched.into_iter();
        self.scripts
            .iter()
            .map(|s| {
                s.src.as_ref()?;
                fetched.next()?.ok()
            })
            .collect()
    }
}

fn fetch_scripts(loader: &Loader, requests: &[(Url, CorsSettings)]) -> Fetched {
    loader.map_parallel(requests, |(u, cors)| {
        loader.fetch_with(u, ResourceKind::Script, *cors)
    })
}

pub fn render(opts: &RenderOptions, start: Instant) -> Result<String, String> {
    let loader = Arc::new(Loader::new());
    let url = huginn_net::parse_target(&opts.target).map_err(|e| e.to_string())?;
    let viewport = Viewport {
        width: opts.width as f32,
        height: opts.height as f32,
        device_pixel_ratio: 1.0,
    };

    let mut page = load(&loader, &url, viewport, opts.js, start)?;

    // 7. Layout.
    let t = Instant::now();
    let mut layout_engine = page.layout_engine.take().unwrap_or_default();
    page.register_fonts(&mut layout_engine);
    let mut tree = page.layout(&mut layout_engine, viewport);
    let mut layout_time = t.elapsed();

    let region = Rect {
        x: 0.0,
        y: 0.0,
        width: viewport.width,
        height: if opts.full_page {
            tree.document_size.height.min(f32::from(u16::MAX))
        } else {
            viewport.height
        },
    };
    // 7b. loading=lazy images in the painted region; lay out again only if
    // one of them decides its own box size.
    let t = Instant::now();
    let relayout = page.load_lazy(&loader, &mut tree, region);
    page.fetch_time += t.elapsed();
    if relayout {
        let t = Instant::now();
        tree = page.layout(&mut layout_engine, viewport);
        layout_time += t.elapsed();
    }

    // 8. Paint: display list + rasterize.
    let t = Instant::now();
    let list = huginn_paint::build_display_list(&tree, Some(region));
    let image = CpuRasterizer::new().rasterize(&list, region, viewport.device_pixel_ratio);
    let paint_time = t.elapsed();

    // 9. Encode.
    let t = Instant::now();
    let file = std::fs::File::create(&opts.out)
        .map_err(|e| format!("cannot create {}: {e}", opts.out.display()))?;
    huginn_paint::encode_png(&image, std::io::BufWriter::new(file))
        .map_err(|e| format!("PNG encoding failed: {e}"))?;
    let encode_time = t.elapsed();

    let report = serde_json::json!({
        "url": url.as_str(),
        "fetch_ms": ms(page.fetch_time),
        "parse_ms": ms(page.parse_time),
        "style_ms": ms(page.style_time),
        "layout_ms": ms(layout_time),
        "paint_ms": ms(paint_time),
        "encode_ms": ms(encode_time),
        "script_ms": ms(page.script_time),
        "scripts": page.js_report.scripts,
        "js_errors": page.js_report.errors,
        // The script phase in parts: by stage (parser-found scripts, the
        // DOMContentLoaded and load events, timers), then the style and
        // layout that scripts asked for, and the time the thread waited
        // instead of running (null where the system does not say).
        "script_run_ms": ms(page.js_report.run_time),
        "script_events_ms": ms(page.js_report.events_time),
        "script_timers_ms": ms(page.js_report.timers_time),
        "script_layout_ms": ms(page.js_report.layout_time),
        "script_layouts": page.js_report.layouts,
        "script_wait_ms": page.js_report.cpu_time.map(|cpu| {
            let r = &page.js_report;
            ms((r.run_time + r.events_time + r.timers_time).saturating_sub(cpu))
        }),
        "total_ms": ms(start.elapsed()),
        "nodes": page.doc.len(),
        "styled": page.styles.styled_count(),
        "boxes": tree.boxes.len(),
        "images": page.images.len(),
        "width": image.width,
        "height": image.height,
    });
    Ok(report.to_string())
}

/// Fetch, parse, run scripts (if `js`) and style `url`: phases 1-6, everything
/// before layout. `start` is the time origin for scripts (`performance.now()`).
pub fn load(
    loader: &Arc<Loader>,
    url: &Url,
    viewport: Viewport,
    js: bool,
    start: Instant,
) -> Result<Page, String> {
    load_document(loader, url, None, None, viewport, js, false, start)
}

/// [`load`] with a request body (`post`: a form submission), the page that
/// started the navigation (`initiator`: its URL and referrer policy decide
/// `Referer` and `SameSite` cookies). `live`: the window. It keeps the
/// script engine alive in [`Page::js`] and runs only the parser-blocking
/// scripts before style, so the page paints without waiting for async,
/// defer and module scripts ([`Page::later`]). Requests that the scripts
/// start meanwhile (`fetch()`, async XHR) wait until it returns. The
/// loader's client becomes the new document.
#[allow(clippy::too_many_arguments)]
pub fn load_document(
    loader: &Arc<Loader>,
    url: &Url,
    post: Option<&huginn_js::forms::PostBody>,
    initiator: Option<&Client>,
    viewport: Viewport,
    js: bool,
    live: bool,
    start: Instant,
) -> Result<Page, String> {
    let screen = huginn_net::Screen {
        width: viewport.width,
        height: viewport.height,
        device_pixel_ratio: viewport.device_pixel_ratio,
    };
    // The window's first paint goes first: requests that scripts send from
    // threads of their own (`fetch()`, async XHR), and low-priority preloads
    // (images, async and module scripts, other media), wait until it has its
    // stylesheets, images and fonts (the guard drops when this returns).
    // `render` waits for scripts' requests before style (scripts.rs), so it
    // holds none.
    let _first_paint = live.then(|| loader.hold_script_requests());

    // 1. Fetch the document (not cached: the body is dropped after parsing).
    // The preload scanner reads it while it arrives and starts the
    // stylesheets, scripts and images it finds; the requests below take
    // their responses.
    huginn_net::log_phase("load");
    let t = Instant::now();
    let mut scanner = preload::Scanner::new(loader, &screen, js);
    let (main, referrer) = loader
        .navigate_streaming(
            url,
            post.map(|p| (p.content_type.as_str(), p.body.as_slice())),
            initiator,
            &mut |head, bytes| scanner.feed(head, bytes),
        )
        .map_err(|e| format!("cannot load {url}: {e}"))?;
    scanner.finish();
    let mut fetch = t.elapsed();
    huginn_net::log_phase("document in");

    // 2. Parse HTML (charset from BOM, Content-Type or <meta charset>).
    let t = Instant::now();
    let mut doc = huginn_dom::parse_html(&main.html_text());
    let doc_url = main.url.clone();
    // The document's referrer policy: `<meta name=referrer>`, else the
    // `Referrer-Policy` header, else the default.
    let referrer_policy = meta_referrer_policy(&doc)
        .or_else(|| {
            main.header("referrer-policy")
                .and_then(ReferrerPolicy::parse_header)
        })
        .unwrap_or_default();
    drop(main);
    loader.set_client(Some(Client {
        url: doc_url.clone(),
        referrer_policy,
    }));
    let base = document_base(&doc, &doc_url);
    let mut parse = t.elapsed();
    huginn_net::log_phase("html parsed");

    // Scripts run only with a script engine (the `v8` feature). Without one,
    // or without scripts, no isolate is ever created.
    let mut js_engine = if js {
        huginn_js::create_engine(huginn_js::EngineOptions::default())
    } else {
        None
    };
    let scripts = match js_engine {
        Some(_) => huginn_js::collect_scripts(&doc, &base),
        None => Vec::new(),
    };
    // The window paints before async, defer and module scripts run, as
    // Chrome does when they are not loaded yet: only parser-blocking
    // scripts hold up the first paint. DOMContentLoaded waits for defer and
    // module scripts, `load` for all of them.
    let (scripts, later): (Vec<ScriptRef>, Vec<ScriptRef>) = if live {
        scripts
            .into_iter()
            .partition(|s| s.timing == ScriptTiming::Blocking)
    } else {
        (scripts, Vec::new())
    };
    let until = if !live {
        scripts::Until::Settled
    } else if later.iter().any(|s| s.timing == ScriptTiming::Defer) {
        scripts::Until::Scripts
    } else if !later.is_empty() {
        scripts::Until::DomContentLoaded
    } else {
        scripts::Until::Load
    };

    // 3. Fetch stylesheets and scripts in parallel. Images wait for style:
    // hidden ones are skipped and the rest are decoded at their displayed size.
    let t = Instant::now();
    let mut sheets = collect_stylesheets(&doc, &base, &screen);
    let mut requests: Vec<(Url, ResourceKind, CorsSettings)> = sheets
        .iter()
        .filter_map(|s| match s {
            SheetSource::External(u, cors) => Some((u.clone(), ResourceKind::Stylesheet, *cors)),
            SheetSource::Inline(_) => None,
        })
        .collect();
    requests.extend(
        scripts
            .iter()
            .filter_map(|s| Some((s.src.clone()?, ResourceKind::Script, s.cors))),
    );
    let fetched = loader.map_parallel(&requests, |(u, kind, cors)| {
        loader.fetch_with(u, *kind, *cors)
    });
    huginn_net::log_phase("stylesheets and blocking scripts in");
    // The later scripts' sources load while the page is styled, laid out and
    // painted: after the stylesheets and blocking scripts, which Chrome
    // gives a higher priority.
    let later = (!later.is_empty()).then(|| LaterScripts::start(loader, later));
    // Scripts lay out without images (their sizes are not known yet).
    let images = ImageStore::new();
    fetch += t.elapsed();

    // 4. Parse CSS (counted as parse time).
    let t = Instant::now();
    let imports = Imports(&loader);
    let mut engine = StyleEngine::new(viewport);
    engine.set_document_url(base.as_str());
    for s in &sheets {
        match s {
            SheetSource::Inline(css) => {
                engine.add_stylesheet(css, base.as_str(), Origin::Author, &imports)
            }
            SheetSource::External(u, cors) => {
                if let Some(Ok(r)) = requests
                    .iter()
                    .position(|(ru, _, rc)| ru == u && rc == cors)
                    .map(|i| &fetched[i])
                {
                    engine.add_stylesheet(&r.text(), r.url.as_str(), Origin::Author, &imports);
                }
            }
        }
    }
    parse += t.elapsed();
    huginn_net::log_phase("css parsed");

    // 4b. Scripts, then pick up the stylesheets and images they added.
    let t = Instant::now();
    let mut layout_engine: Option<LayoutEngine> = None;
    let mut js_report = scripts::ScriptReport::default();
    if let Some(js) = js_engine.as_deref_mut()
        && !scripts.is_empty()
    {
        // Borrowed from the fetched bodies: script text is not copied here.
        let sources: Vec<Option<(std::borrow::Cow<'_, str>, String)>> = scripts
            .iter()
            .map(|s| {
                let u = s.src.as_ref()?;
                let i = requests
                    .iter()
                    .position(|(ru, k, _)| ru == u && *k == ResourceKind::Script)?;
                fetched[i]
                    .as_ref()
                    .ok()
                    .map(|r| (r.text(), r.url.to_string()))
            })
            .collect();
        let generation = doc.generation();
        let size = Size {
            width: viewport.width,
            height: viewport.height,
        };
        let mut host = scripts::PageLayout::new(&engine, &images, &base, size, &mut layout_engine);
        js_report = scripts::run(
            js,
            &mut doc,
            loader,
            &doc_url,
            referrer.as_deref().unwrap_or(""),
            &base,
            (viewport.width, viewport.height),
            start,
            &scripts,
            &sources,
            &mut host,
            until,
        );
        if doc.generation() != generation {
            let new_sheets = collect_stylesheets(&doc, &base, &screen);
            if new_sheets != sheets {
                // Appended sheets go on top of the parsed ones; anything else
                // (removed or reordered sheets) rebuilds the cascade.
                let appended = new_sheets.starts_with(&sheets);
                if !appended {
                    engine = StyleEngine::new(viewport);
                }
                let skip = if appended { sheets.len() } else { 0 };
                add_sheets(&mut engine, &loader, &new_sheets[skip..], &base);
                sheets = new_sheets;
            }
        }
    }
    if !scripts.is_empty() {
        huginn_net::log_phase("blocking scripts run");
    }
    // Stylesheet and script texts are parsed or run; free them.
    drop(fetched);
    loader.clear_cache();
    // Free the isolate now: its heap must not add to peak RSS during style,
    // layout and paint. A live page keeps it, after a full GC.
    let js_engine = match js_engine {
        Some(mut e) if live => {
            e.trim_memory();
            Some(e)
        }
        _ => None,
    };
    // Time the settle loop waited for scripts' requests is network time.
    let script_time = t.elapsed().saturating_sub(js_report.net_wait);
    fetch += js_report.net_wait;

    // 5. Style.
    let t = Instant::now();
    let styles = engine.compute(&doc);
    let style_time = t.elapsed();
    huginn_net::log_phase("styled");

    // 6. Second fetch wave, one parallel batch: <img> (eager, chosen from
    // srcset/<picture>), CSS background images, inline <svg> and the web
    // fonts the text uses. Bodies are dropped as soon as they are decoded.
    // The window paints without the ones still on the network (fonts in
    // their block period aside); they come with `pending`.
    let t = Instant::now();
    let img_plan = resources::plan_images(&doc, &base, &screen);
    let (loaded, pending) = if live {
        resources::load_for_first_paint(
            loader,
            &doc,
            &styles,
            engine.font_faces(),
            &img_plan,
            &screen,
        )
    } else {
        let all = resources::load(
            loader,
            &doc,
            &styles,
            engine.font_faces(),
            &img_plan,
            &screen,
        );
        (all, None)
    };
    let resources::Loaded {
        images,
        fonts,
        font_urls,
    } = loaded;
    fetch += t.elapsed();
    huginn_net::log_phase("images and fonts for the first paint in");

    Ok(Page {
        url: doc_url,
        referrer: referrer.unwrap_or_default(),
        base,
        doc,
        style_engine: engine,
        styles,
        images,
        layout_engine,
        fonts,
        font_urls,
        img_plan,
        sheets,
        screen,
        js_report,
        js: js_engine,
        later,
        pending,
        fetch_time: fetch,
        parse_time: parse,
        script_time,
        style_time,
    })
}

/// Add stylesheets in document order, fetching external ones (cached by the loader).
fn add_sheets(engine: &mut StyleEngine, loader: &Loader, sheets: &[SheetSource], base: &Url) {
    let imports = Imports(loader);
    let externals: Vec<(&Url, CorsSettings)> = sheets
        .iter()
        .filter_map(|s| match s {
            SheetSource::External(u, cors) => Some((u, *cors)),
            SheetSource::Inline(_) => None,
        })
        .collect();
    let fetched = loader.map_parallel(&externals, |(u, cors)| {
        loader.fetch_with(u, ResourceKind::Stylesheet, *cors)
    });
    let mut next = fetched.iter();
    for s in sheets {
        match s {
            SheetSource::Inline(css) => {
                engine.add_stylesheet(css, base.as_str(), Origin::Author, &imports)
            }
            SheetSource::External(..) => {
                if let Some(Ok(r)) = next.next() {
                    engine.add_stylesheet(&r.text(), r.url.as_str(), Origin::Author, &imports);
                }
            }
        }
    }
}

/// `<base href>` or the document URL.
fn document_base(doc: &Document, url: &Url) -> Url {
    doc.elements_by_local_name(&local_name!("base"))
        .find_map(|b| doc.attribute(b, &local_name!("href")))
        .and_then(|h| huginn_net::resolve_url(url, h))
        .unwrap_or_else(|| url.clone())
}

/// The policy of the last `<meta name=referrer content=...>` that names one.
fn meta_referrer_policy(doc: &Document) -> Option<ReferrerPolicy> {
    doc.elements_by_local_name(&local_name!("meta"))
        .filter(|&m| {
            doc.attribute(m, &local_name!("name"))
                .is_some_and(|n| n.trim().eq_ignore_ascii_case("referrer"))
        })
        .filter_map(|m| doc.attribute(m, &local_name!("content")))
        .filter_map(ReferrerPolicy::parse_token)
        .last()
}

fn collect_stylesheets(
    doc: &Document,
    base: &Url,
    screen: &huginn_net::Screen,
) -> Vec<SheetSource> {
    let mut out = Vec::new();
    for n in doc.descendants(NodeId::DOCUMENT) {
        let Some(el) = doc.element(n) else { continue };
        if el.name.ns != huginn_dom::ns!(html) {
            continue;
        }
        let media_ok = el
            .attr(&local_name!("media"))
            .is_none_or(|m| huginn_net::media_matches(m, screen));
        if el.name.local == local_name!("style") && media_ok {
            let css: String = doc
                .children(n)
                .filter_map(|c| match &doc.node(c).data {
                    NodeData::Text(t) => Some(&**t),
                    _ => None,
                })
                .collect();
            out.push(SheetSource::Inline(css));
        } else if el.name.local == local_name!("link") {
            let rel = el
                .attr(&local_name!("rel"))
                .unwrap_or("")
                .to_ascii_lowercase();
            let mut rels = rel.split_ascii_whitespace();
            let is_sheet =
                rels.clone().any(|r| r == "stylesheet") && !rels.any(|r| r == "alternate");
            // A `disabled` link (the attribute, or `link.disabled = true`
            // from script) does not apply its sheet.
            if is_sheet
                && media_ok
                && el.attr(&local_name!("disabled")).is_none()
                && let Some(u) = el
                    .attr(&local_name!("href"))
                    .and_then(|h| huginn_net::resolve_url(base, h))
            {
                let cors = CorsSettings::from_attr(el.attr(&local_name!("crossorigin")));
                out.push(SheetSource::External(u, cors));
            }
        }
    }
    out
}
