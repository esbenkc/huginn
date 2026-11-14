//! The engine thread of `huginn open`.
//!
//! It loads, styles and lays out pages off the UI thread, so the window stays
//! responsive while a page loads. It owns the [`Page`] (the `Document` is not
//! `Send`). The UI thread gets the finished [`LayoutTree`] and a [`Links`]
//! table, then paints and hit-tests on its own.

use std::any::Any;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use huginn_dom::{Document, NodeId, local_name, ns};
use huginn_layout::{LayoutEngine, LayoutTree};
use huginn_net::{Client, LoadKind, Loader, Network, ReferrerPolicy, Url};
use huginn_style::Viewport;

use crate::input::Modifiers;
use crate::livepage::LivePage;
use crate::pipeline::{self, Page};

/// Work for the tab's engine thread, run on its current page (interact:
/// print, save, focus order). Its answer comes back as an event.
pub type Job =
    Box<dyn FnOnce(&mut Page, &mut LayoutEngine, Viewport) -> Option<EngineEvent> + Send>;

/// A form submission body.
pub type PostData = huginn_js::forms::PostBody;

/// UI thread -> engine thread. `seq` increases with every command.
pub enum Command {
    /// Load `url` and lay it out for `viewport`. `generation` tags the answer.
    /// `html`: the document itself (internal `huginn:` pages), shown at `url`.
    /// `initiator`: the page that started the navigation (`None`: the user).
    /// `kind`: how the load uses the HTTP cache (reload, back and forward).
    Load {
        seq: u64,
        generation: u64,
        url: Url,
        viewport: Viewport,
        post: Option<PostData>,
        html: Option<String>,
        initiator: Option<Client>,
        kind: LoadKind,
    },
    /// Restyle and lay out the current page for a new viewport.
    Resize {
        seq: u64,
        generation: u64,
        viewport: Viewport,
    },
    /// Find the target of `#id` in the current page.
    Fragment {
        seq: u64,
        generation: u64,
        id: String,
    },
    /// The tab became the active one in its window (or stopped being it).
    /// Background tabs throttle timers.
    SetVisible { seq: u64, visible: bool },
    /// Run a job on the current page.
    Run { seq: u64, generation: u64, f: Job },
    /// A DOM input event for the live page (`live`).
    Input {
        seq: u64,
        generation: u64,
        event: crate::live::PageInput,
    },
}

impl Command {
    /// A pointer move for the live page (only the last one of a batch runs).
    fn is_pointer_move(&self) -> bool {
        matches!(self, Command::Input { event, .. } if crate::live::is_pointer_move(event))
    }

    fn seq(&self) -> u64 {
        match self {
            Command::Load { seq, .. }
            | Command::Resize { seq, .. }
            | Command::Fragment { seq, .. }
            | Command::SetVisible { seq, .. }
            | Command::Run { seq, .. }
            | Command::Input { seq, .. } => *seq,
        }
    }
}

/// Engine thread -> UI thread.
pub enum EngineEvent {
    /// The first paint of a page: parsed, its stylesheets and blocking
    /// scripts done.
    Loaded(Box<Loaded>),
    /// New layout of the current page (after a resize, or a DOM change: then
    /// also new links and maybe a new title).
    Relaid {
        generation: u64,
        tree: LayoutTree,
        links: Option<Links>,
        title: Option<String>,
    },
    /// Answer to [`Command::Fragment`]: the target and its ancestors, innermost first.
    Fragment {
        generation: u64,
        targets: Vec<NodeId>,
    },
    /// The page asks to navigate (a link's default action after scripts, a
    /// form submission, `location.href = ...`). `mods` pick the target like a
    /// click with them: same tab, new tab or new window.
    Navigate {
        generation: u64,
        url: Url,
        post: Option<PostData>,
        mods: Modifiers,
    },
    /// Answer of a [`Command::Run`] job for `interact`.
    Interact(Box<dyn Any + Send>),
    /// Events of `live` (focus, caret, clipboard writes, ...).
    Live(crate::live::LiveEvent),
    /// A favicon finished loading (`None`: it failed). Not tied to a page.
    Favicon {
        url: Url,
        image: Option<huginn_net::DecodedImage>,
    },
    /// The load of `generation` is complete: the scripts that ran after the
    /// first paint, DOMContentLoaded, load and the lazy images are done.
    /// `complete_ms` counts from the start of the load, like `load_ms`.
    Complete { generation: u64, complete_ms: f64 },
    /// Every command up to `seq` is done and the queue is empty.
    Idle { seq: u64 },
}

/// A page ready for its first paint (or the error page that replaced it).
pub struct Loaded {
    pub generation: u64,
    /// Final URL after redirects; the requested URL if loading failed.
    pub url: Url,
    pub title: String,
    /// `<link rel=icon>`, else `/favicon.ico` for http(s) pages.
    pub icon: Option<Url>,
    pub tree: LayoutTree,
    pub links: Links,
    /// Target of the URL's fragment and its ancestors, innermost first.
    pub fragment: Vec<NodeId>,
    /// Why loading failed; the tree then shows an error page.
    pub error: Option<String>,
    /// From the start of the load to the first paint, ms.
    pub load_ms: f64,
    /// The document's referrer policy, for the links it starts.
    pub referrer_policy: ReferrerPolicy,
}

/// Which link, if any, each DOM node is inside.
pub struct Links {
    urls: Vec<Url>,
    /// Index into `urls` per `NodeId::index()`, `u32::MAX` for none.
    by_node: Vec<u32>,
}

impl Links {
    /// Resolve every `<a href>` and `<area href>` once, in tree order.
    pub fn build(doc: &Document, base: &Url) -> Links {
        let mut urls = Vec::new();
        let mut by_node = vec![u32::MAX; doc.len()];
        for n in doc.descendants(NodeId::DOCUMENT) {
            let inherited = doc.parent(n).map_or(u32::MAX, |p| by_node[p.index()]);
            let own = doc
                .element(n)
                .filter(|e| {
                    e.name.ns == ns!(html)
                        && matches!(e.name.local, local_name!("a") | local_name!("area"))
                })
                .and_then(|e| e.attr(&local_name!("href")))
                .and_then(|h| huginn_net::resolve_url(base, h.trim()))
                .filter(|u| {
                    matches!(
                        u.scheme(),
                        "http" | "https" | "file" | "data" | "about" | "huginn"
                    )
                });
            by_node[n.index()] = match own {
                Some(u) => {
                    urls.push(u);
                    (urls.len() - 1) as u32
                }
                None => inherited,
            };
        }
        Links { urls, by_node }
    }

    /// The link `node` is inside.
    pub fn get(&self, node: NodeId) -> Option<&Url> {
        let i = *self.by_node.get(node.index())?;
        self.urls.get(i as usize)
    }
}

/// Delivers engine events to the UI thread (tagged with the tab by the caller).
pub type Notify = Arc<dyn Fn(EngineEvent) + Send + Sync>;

/// Start one tab's engine thread. It ends when the returned sender is
/// dropped (the tab closed or was discarded), freeing the page.
/// `js`: run page scripts (when built with the `v8` feature). `net`: the
/// network of the tab's profile (the private one in private windows).
pub fn spawn(js: bool, net: Arc<Network>, notify: Notify) -> Sender<Command> {
    let (tx, rx) = channel();
    // Style and layout recurse on deep DOMs: same big stack as `render`. Only
    // touched stack pages are committed, so idle tabs cost no stack memory.
    std::thread::Builder::new()
        .name("engine".into())
        .stack_size(256 << 20)
        .spawn(move || run(&rx, js, &net, &*notify))
        .expect("spawn engine thread");
    tx
}

/// The tab's page on the engine thread.
pub(crate) struct Current {
    pub page: Page,
    pub layout: LayoutEngine,
    pub viewport: Viewport,
    /// Generation of the load that made this page; commands for other
    /// generations (a stopped load, a newer navigation) are ignored.
    pub generation: u64,
    /// Scripts, timers and input after load (`livepage`).
    pub live: LivePage,
}

fn run(rx: &Receiver<Command>, js: bool, net: &Arc<Network>, notify: &dyn Fn(EngineEvent)) {
    let mut current: Option<Current> = None;
    // Whether the tab is the active one in its window.
    let mut visible = true;
    loop {
        // Sleep until a command arrives or the page's next timer is due.
        let wake = current.as_ref().and_then(|c| c.live.next_wakeup(&c.page));
        let first = match wake {
            None => match rx.recv() {
                Ok(c) => c,
                Err(_) => break,
            },
            Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(c) => c,
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(c) = &mut current {
                        c.live.run_timers(
                            &mut c.page,
                            &mut c.layout,
                            c.viewport,
                            c.generation,
                            notify,
                        );
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };
        // Coalesce a burst (live resize sends many): only the last load and
        // the last viewport matter, and only the last pointer move.
        let mut batch = vec![first];
        batch.extend(rx.try_iter());
        let last_seq = batch.last().map_or(0, Command::seq);
        let start = batch
            .iter()
            .rposition(|c| matches!(c, Command::Load { .. }))
            .unwrap_or(0);
        let batch = batch.split_off(start);
        let last_viewport = batch.iter().rev().find_map(|c| match c {
            Command::Resize { viewport, .. } => Some(*viewport),
            _ => None,
        });
        let last_move = batch.iter().rposition(Command::is_pointer_move);
        for (i, cmd) in batch.into_iter().enumerate() {
            match cmd {
                Command::Load {
                    generation,
                    url,
                    viewport,
                    post,
                    html,
                    initiator,
                    kind,
                    ..
                } => {
                    // Drop the old page first: lower peak memory.
                    drop(current.take());
                    let (mut cur, loaded) = load(
                        generation,
                        &url,
                        last_viewport.unwrap_or(viewport),
                        js,
                        post,
                        html,
                        initiator.as_ref(),
                        kind,
                        net,
                    );
                    notify(EngineEvent::Loaded(Box::new(loaded)));
                    cur.live.set_visible(&mut cur.page, visible);
                    cur.live.after_load(
                        &mut cur.page,
                        &mut cur.layout,
                        cur.viewport,
                        cur.generation,
                        notify,
                    );
                    // The page is on screen: now the async, defer and module
                    // scripts, and the load events that waited for them.
                    cur.live.finish_load(
                        &mut cur.page,
                        &mut cur.layout,
                        cur.viewport,
                        cur.generation,
                        notify,
                    );
                    // `loading=lazy` images after the page is on screen, as
                    // in Chrome (on bbc.com they held up the first paint by
                    // seconds); then the page is laid out again.
                    if cur.page.load_all_lazy(cur.live.loader()) {
                        let tree = cur.page.layout(&mut cur.layout, cur.viewport);
                        notify(EngineEvent::Relaid {
                            generation: cur.generation,
                            tree,
                            links: None,
                            title: None,
                        });
                    }
                    notify(EngineEvent::Complete {
                        generation: cur.generation,
                        complete_ms: cur.live.load_start().elapsed().as_secs_f64() * 1e3,
                    });
                    // The load is done: from now on the page's requests use
                    // the cache as usual, also after a hard reload.
                    cur.live.loader().set_load_kind(LoadKind::Normal);
                    current = Some(cur);
                    // Off the UI thread, at most every 30 s; the session
                    // saves the rest when it ends.
                    if let Err(e) = net
                        .cookies()
                        .save_unless_saved_within(Duration::from_secs(30))
                    {
                        eprintln!("huginn: cannot save cookies: {e}");
                    }
                }
                Command::Resize { .. } => {
                    let viewport = last_viewport.expect("batch has a resize");
                    if let Some(cur) = &mut current
                        && cur.viewport != viewport
                    {
                        cur.viewport = viewport;
                        cur.page.restyle(viewport);
                        let tree = cur.page.layout(&mut cur.layout, viewport);
                        notify(EngineEvent::Relaid {
                            generation: cur.generation,
                            tree,
                            links: None,
                            title: None,
                        });
                    }
                }
                Command::Fragment { generation, id, .. } => {
                    let cur = current.as_mut().filter(|c| c.generation == generation);
                    let targets = cur
                        .as_ref()
                        .map_or_else(Vec::new, |c| fragment_targets(&c.page.doc, &id));
                    notify(EngineEvent::Fragment {
                        generation,
                        targets,
                    });
                    if let Some(c) = cur {
                        c.live.fragment(
                            &mut c.page,
                            &mut c.layout,
                            c.viewport,
                            c.generation,
                            &id,
                            notify,
                        );
                    }
                }
                Command::SetVisible { visible: v, .. } => {
                    visible = v;
                    if let Some(c) = &mut current {
                        c.live.set_visible(&mut c.page, v);
                    }
                }
                Command::Run { generation, f, .. } => {
                    if let Some(cur) = current.as_mut().filter(|c| c.generation == generation)
                        && let Some(ev) = f(&mut cur.page, &mut cur.layout, cur.viewport)
                    {
                        notify(ev);
                    }
                }
                Command::Input {
                    generation, event, ..
                } => {
                    let superseded =
                        last_move.is_some_and(|l| l > i) && crate::live::is_pointer_move(&event);
                    if let Some(cur) = current
                        .as_mut()
                        .filter(|c| c.generation == generation && !superseded)
                    {
                        crate::live::input(cur, event, visible, notify);
                    }
                }
            }
        }
        notify(EngineEvent::Idle { seq: last_seq });
    }
}

#[allow(clippy::too_many_arguments)]
fn load(
    generation: u64,
    url: &Url,
    viewport: Viewport,
    js: bool,
    post: Option<PostData>,
    html: Option<String>,
    initiator: Option<&Client>,
    kind: LoadKind,
    net: &Arc<Network>,
) -> (Current, Loaded) {
    let t = Instant::now();
    // One loader per page, on the profile's network (idle connections and
    // the HTTP cache outlive the page); scripts keep fetching through it
    // (XMLHttpRequest, fetch) while the page lives.
    let loader = Arc::new(Loader::with_network(net.clone()));
    loader.set_load_kind(kind);
    let internal;
    let (mut page, error) = {
        // Internal schemes: `huginn:` pages come with their HTML;
        // `view-source:` fetches the source here, off the UI thread.
        let mut failed = None;
        let html = html.or_else(|| {
            let source = crate::interact::source::view_source_target(url)?;
            Some(
                crate::interact::source::view_source_html(&loader, &source).unwrap_or_else(|e| {
                    let page = error_page(url, &e);
                    failed = Some(e);
                    page
                }),
            )
        });
        internal = html.is_some();
        let target = match &html {
            Some(h) => data_url(h),
            None if url.scheme() == "about" => blank_url(),
            None => url.clone(),
        };
        let post = post.as_ref().filter(|_| !internal);
        match pipeline::load_document(
            &loader,
            &target,
            post,
            initiator.filter(|_| !internal),
            viewport,
            js && !internal,
            true,
            t,
        ) {
            Ok(page) => (page, failed),
            Err(e) => {
                let page =
                    pipeline::load(&loader, &data_url(&error_page(url, &e)), viewport, false, t)
                        .expect("the built-in error page always loads");
                (page, Some(e))
            }
        }
    };
    // A fresh layout engine per page (or the one scripts already used), so web
    // fonts do not leak across pages.
    let mut layout = page.layout_engine.take().unwrap_or_default();
    // Selection, find and copy read the rendered text (render never builds it).
    layout.set_text_index(true);
    // The page is laid out again after every change: keep its shaped text.
    // `HUGINN_SHAPE_CACHE=0` turns that off, to tell whether a layout bug
    // comes from the cache.
    layout.set_shape_cache(std::env::var_os("HUGINN_SHAPE_CACHE").is_none_or(|v| v != "0"));
    // And its box tree: a pass builds boxes only for what changed. The
    // engine lays out this one page. `HUGINN_KEEP_TREE=0` turns it off.
    layout.set_keep_tree(std::env::var_os("HUGINN_KEEP_TREE").is_none_or(|v| v != "0"));
    page.register_fonts(&mut layout);
    let tree = page.layout(&mut layout, viewport);
    let shown_at_url = error.is_some() || internal || url.scheme() == "about";
    let final_url = if shown_at_url {
        url.clone()
    } else {
        page.url.clone()
    };
    let loaded = Loaded {
        generation,
        icon: if error.is_some() {
            None
        } else {
            icon_url(&page.doc, &page.base, &final_url)
        },
        url: final_url,
        title: title(&page.doc),
        tree,
        links: Links::build(&page.doc, &page.base),
        fragment: url
            .fragment()
            .map_or_else(Vec::new, |f| fragment_targets(&page.doc, f)),
        error,
        load_ms: t.elapsed().as_secs_f64() * 1e3,
        referrer_policy: loader
            .client()
            .map(|c| c.referrer_policy)
            .unwrap_or_default(),
    };
    (
        Current {
            live: LivePage::new(loader, t, &mut page),
            page,
            layout,
            viewport,
            generation,
        },
        loaded,
    )
}

/// The page's icon: the last `<link rel~=icon href>`, else `/favicon.ico` on
/// http(s) origins.
fn icon_url(doc: &Document, base: &Url, url: &Url) -> Option<Url> {
    let declared = doc
        .elements_by_local_name(&local_name!("link"))
        .filter(|&n| {
            doc.attribute(n, &local_name!("rel")).is_some_and(|r| {
                r.split_ascii_whitespace()
                    .any(|t| t.eq_ignore_ascii_case("icon"))
            })
        })
        .filter_map(|n| doc.attribute(n, &local_name!("href")))
        .filter_map(|h| huginn_net::resolve_url(base, h))
        .last();
    declared.or_else(|| {
        matches!(url.scheme(), "http" | "https")
            .then(|| url.join("/favicon.ico").ok())
            .flatten()
    })
}

/// `<title>` text with whitespace collapsed.
pub(crate) fn title(doc: &Document) -> String {
    doc.elements_by_local_name(&local_name!("title"))
        .find(|&n| doc.element(n).is_some_and(|e| e.name.ns == ns!(html)))
        .map(|n| {
            doc.text_content(n)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

/// The element `#id` points at (by id, else `<a name>`), then its ancestors.
fn fragment_targets(doc: &Document, fragment: &str) -> Vec<NodeId> {
    let id = percent_decode(fragment);
    let target = doc.element_by_id(&id).or_else(|| {
        doc.elements_by_local_name(&local_name!("a"))
            .find(|&n| doc.attribute(n, &local_name!("name")) == Some(id.as_str()))
    });
    let Some(target) = target else {
        return Vec::new();
    };
    std::iter::once(target)
        .chain(doc.ancestors(target))
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn blank_url() -> Url {
    Url::parse("data:text/html,").expect("valid data URL")
}

/// `html` as a `data:` URL.
fn data_url(html: &str) -> Url {
    let mut s = String::from("data:text/html;charset=utf-8,");
    for &b in html.as_bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~ ".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    Url::parse(&s).expect("valid data URL")
}

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

fn error_page(url: &Url, error: &str) -> String {
    use crate::theme::{FONT, HAIRLINE, MUTED, STRIP, TEXT, WARN};
    format!(
        "<!doctype html><title>Cannot load page</title>\
         <body style=\"margin:64px;background:{STRIP};font-family:{FONT};color:{TEXT}\">\
         <div style=\"font-size:10px;letter-spacing:0.14em;color:{WARN};border:1px solid {WARN};display:inline-block;\
         padding:3px 8px\">ERROR</div>\
         <h1 style=\"font-size:20px;font-weight:400;margin:24px 0 16px;padding-bottom:16px;border-bottom:1px solid {HAIRLINE}\">\
         This page could not be loaded</h1>\
         <p style=\"color:{TEXT}\">{}</p><p style=\"color:{MUTED};font-size:12px\">{}</p></body>",
        escape_html(url.as_str()),
        escape_html(error)
    )
}
