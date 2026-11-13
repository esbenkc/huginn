//! The page-load pipeline: fetch -> parse -> scripts -> style -> layout -> paint -> PNG.
//!
//! [`load`] does everything up to layout and returns a [`Page`]. `render`
//! (headless PNG) and the `open` window share it.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use huginn_dom::{Document, NodeData, NodeId, local_name};
use huginn_layout::{LayoutEngine, LayoutInput, LayoutTree, Rect, Size};
use huginn_net::{ImageStore, Loader, ResourceKind, Url};
use huginn_paint::{CpuRasterizer, Rasterizer};
use huginn_style::{Origin, StyleEngine, StyleMap, StylesheetLoader, Viewport};

use crate::{resources, scripts};

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

/// A stylesheet source in document order.
#[derive(Clone, PartialEq)]
enum SheetSource {
    Inline(String),
    External(Url),
}

/// A fetched, parsed, scripted and styled document, ready for layout.
pub struct Page {
    /// Document URL after redirects.
    pub url: Url,
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
    /// The `<img>` choices; `loading=lazy` ones load in `load_lazy`.
    img_plan: resources::ImagePlan,
    /// Stylesheets in the cascade, in document order (`restyle_dom` diffs them).
    sheets: Vec<SheetSource>,
    screen: huginn_net::Screen,
    pub js_report: scripts::ScriptReport,
    /// The page's script engine, kept alive by [`load_document`] with
    /// `keep_js` (the `open` window runs scripts after load). `render` drops
    /// it before style, so its heap never adds to peak RSS there.
    pub js: Option<Box<dyn huginn_js::ScriptEngine>>,
    pub fetch_time: Duration,
    pub parse_time: Duration,
    pub script_time: Duration,
    pub style_time: Duration,
}

impl Page {
    /// Move the page's web fonts into `layout_engine` (no copy; call once).
    pub fn register_fonts(&mut self, layout_engine: &mut LayoutEngine) {
        for f in std::mem::take(&mut self.fonts) {
            layout_engine.register_font(f.data, &f.family, f.weight, f.style);
        }
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
    /// them can come into view).
    pub fn load_all_lazy(&mut self, loader: &Loader) {
        resources::load_all_lazy(
            loader,
            &self.doc,
            &self.styles,
            &self.img_plan,
            &self.screen,
            &mut self.images,
        );
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

    /// Load the images the current styles and DOM use but the page does not
    /// have yet (added by scripts, or by a `:checked`/`:hover` style).
    /// `tried`: URLs fetched before. True if any image was added.
    pub fn load_added(
        &mut self,
        loader: &Loader,
        tried: &mut std::collections::HashSet<String>,
    ) -> bool {
        resources::load_added(
            loader,
            &self.doc,
            &self.styles,
            &self.base,
            &self.screen,
            &mut self.images,
            tried,
        )
    }
}

pub fn render(opts: &RenderOptions, start: Instant) -> Result<String, String> {
    let loader = Loader::new();
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
    loader: &Loader,
    url: &Url,
    viewport: Viewport,
    js: bool,
    start: Instant,
) -> Result<Page, String> {
    load_document(loader, url, None, viewport, js, false, start)
}

/// [`load`] with a request body (`post`: a form submission) and the option
/// to keep the script engine alive in [`Page::js`].
#[allow(clippy::too_many_arguments)]
pub fn load_document(
    loader: &Loader,
    url: &Url,
    post: Option<&huginn_js::forms::PostBody>,
    viewport: Viewport,
    js: bool,
    keep_js: bool,
    start: Instant,
) -> Result<Page, String> {
    let screen = huginn_net::Screen {
        width: viewport.width,
        height: viewport.height,
        device_pixel_ratio: viewport.device_pixel_ratio,
    };
    // 1. Fetch the document (not cached: the body is dropped after parsing).
    let t = Instant::now();
    let main = match post {
        Some(p) => loader.post_document(url, &p.content_type, &p.body),
        None => loader.fetch_uncached(url, ResourceKind::Document),
    }
    .map_err(|e| format!("cannot load {url}: {e}"))?;
    let mut fetch = t.elapsed();

    // 2. Parse HTML (charset from BOM, Content-Type or <meta charset>).
    let t = Instant::now();
    let mut doc = huginn_dom::parse_html(&main.html_text());
    let doc_url = main.url.clone();
    drop(main);
    let base = document_base(&doc, &doc_url);
    let mut parse = t.elapsed();

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

    // 3. Fetch stylesheets and scripts in parallel. Images wait for style:
    // hidden ones are skipped and the rest are decoded at their displayed size.
    let t = Instant::now();
    let mut sheets = collect_stylesheets(&doc, &base, &screen);
    let mut requests: Vec<(Url, ResourceKind)> = sheets
        .iter()
        .filter_map(|s| match s {
            SheetSource::External(u) => Some((u.clone(), ResourceKind::Stylesheet)),
            SheetSource::Inline(_) => None,
        })
        .collect();
    requests.extend(
        scripts
            .iter()
            .filter_map(|s| s.src.clone())
            .map(|u| (u, ResourceKind::Script)),
    );
    let fetched = loader.fetch_all(&requests);
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
            SheetSource::External(u) => {
                if let Some(Ok(r)) = requests
                    .iter()
                    .position(|(ru, _)| ru == u)
                    .map(|i| &fetched[i])
                {
                    engine.add_stylesheet(&r.text(), r.url.as_str(), Origin::Author, &imports);
                }
            }
        }
    }
    parse += t.elapsed();

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
                    .position(|(ru, k)| ru == u && *k == ResourceKind::Script)?;
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
            &loader,
            &doc_url,
            &base,
            (viewport.width, viewport.height),
            start,
            &scripts,
            &sources,
            &mut host,
            !keep_js,
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
    // Stylesheet and script texts are parsed or run; free them.
    drop(fetched);
    loader.clear_cache();
    // Free the isolate now: its heap must not add to peak RSS during style,
    // layout and paint. A live page keeps it, after a full GC.
    let js_engine = match js_engine {
        Some(mut e) if keep_js => {
            e.trim_memory();
            Some(e)
        }
        _ => None,
    };
    let script_time = t.elapsed();

    // 5. Style.
    let t = Instant::now();
    let styles = engine.compute(&doc);
    let style_time = t.elapsed();

    // 6. Second fetch wave, one parallel batch: <img> (eager, chosen from
    // srcset/<picture>), CSS background images, inline <svg> and the web
    // fonts the text uses. Bodies are dropped as soon as they are decoded.
    let t = Instant::now();
    let img_plan = resources::plan_images(&doc, &base, &screen);
    let resources::Loaded { images, fonts } = resources::load(
        loader,
        &doc,
        &styles,
        engine.font_faces(),
        &img_plan,
        &screen,
    );
    fetch += t.elapsed();

    Ok(Page {
        url: doc_url,
        base,
        doc,
        style_engine: engine,
        styles,
        images,
        layout_engine,
        fonts,
        img_plan,
        sheets,
        screen,
        js_report,
        js: js_engine,
        fetch_time: fetch,
        parse_time: parse,
        script_time,
        style_time,
    })
}

/// Add stylesheets in document order, fetching external ones (cached by the loader).
fn add_sheets(engine: &mut StyleEngine, loader: &Loader, sheets: &[SheetSource], base: &Url) {
    let imports = Imports(loader);
    let externals: Vec<(Url, ResourceKind)> = sheets
        .iter()
        .filter_map(|s| match s {
            SheetSource::External(u) => Some((u.clone(), ResourceKind::Stylesheet)),
            SheetSource::Inline(_) => None,
        })
        .collect();
    let fetched = loader.fetch_all(&externals);
    let mut next = fetched.iter();
    for s in sheets {
        match s {
            SheetSource::Inline(css) => {
                engine.add_stylesheet(css, base.as_str(), Origin::Author, &imports)
            }
            SheetSource::External(_) => {
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
                out.push(SheetSource::External(u));
            }
        }
    }
    out
}
