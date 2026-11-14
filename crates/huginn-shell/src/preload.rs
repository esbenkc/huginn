//! The preload scanner (owned by huginn-net, like `resources.rs`): Chrome's,
//! simplified. While the document arrives, a tokenizer pass over the text
//! so far finds the subresources the parser will ask for and starts them
//! ([`Loader::preload`]), so stylesheets and scripts are on the way before
//! the body ends. The parser's own requests later take those responses
//! instead of fetching again; that needs the same URL, destination and
//! CORS settings, so this file asks for exactly what `pipeline.rs`,
//! `resources.rs` and huginn-js will ask for.
//!
//! - `<link rel=stylesheet>` (not `alternate` or `disabled`): blocking,
//!   low when its `media` does not match the screen (fetched, never
//!   render-blocking, as in Chrome).
//! - `<script src>` (only with scripts on): parser-blocking classic scripts
//!   blocking, `async`, `defer` and module scripts low; `nomodule` and data
//!   blocks skipped (`huginn_js::script_kind`).
//! - `<link rel=preload>`: `as=style` and `as=font` high (after the
//!   blocking ones: without HTTP/2 priorities, ten preloaded fonts would
//!   share the connection with the stylesheets), `as=script` and
//!   `as=image` low; one that cannot match the parser's request (a font
//!   without `crossorigin=anonymous`, an image with `crossorigin`, a
//!   `media` that does not match) is skipped. `<link rel=modulepreload>`:
//!   low.
//! - `<img>` (`src`, `srcset`, `sizes`): low. Not lazy ones, not inside
//!   `<picture>`, not with `crossorigin`.
//! - `<link rel=preconnect>` and `dns-prefetch` warm the connection or the
//!   name lookup; requests that wait, to other origins, preconnect at
//!   once, so their connection is ready when they go.
//! - `<base href>` and `<meta name=referrer>` apply to what follows.
//! - Requests found in the `<head>` that do not block the first paint wait
//!   until the body starts, so the head's stylesheets go first (Chrome
//!   delays such requests while the parser is in the head).
//! - After a `<meta http-equiv=Content-Security-Policy>` nothing more is
//!   started (Chrome waits for the parser to apply such a policy); the
//!   parser fetches the rest itself.
//! - Nothing inside `<template>`, `<svg>` or `<math>`. `<script>`,
//!   `<style>`, `<noscript>` (scripting is on in the parser), `<title>`,
//!   `<textarea>` and the other raw-text elements switch the tokenizer the
//!   way the tree builder does, so their text is not read as markup.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Arc;

use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{
    BufferQueue, Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};
use huginn_dom::{ElementData, QualName, StrTendril, local_name, ns};
use huginn_js::{ScriptKind, script_cors, script_kind};
use huginn_net::{
    Client, CorsSettings, HtmlStreamDecoder, Loader, Priority, ReferrerPolicy, Resource,
    ResourceKind, Screen, Url,
};

/// Scans a document while [`Loader::navigate_streaming`] reads it.
pub struct Scanner {
    sink: Option<Sink>,
    tokenizer: Option<Tokenizer<Sink>>,
    decoder: Option<HtmlStreamDecoder>,
    text: String,
    input: BufferQueue,
}

impl Scanner {
    /// `js`: scripts run (their sources are worth fetching).
    pub fn new(loader: &Arc<Loader>, screen: &Screen, js: bool) -> Scanner {
        Scanner {
            sink: Some(Sink {
                loader: loader.clone(),
                screen: Screen {
                    width: screen.width,
                    height: screen.height,
                    device_pixel_ratio: screen.device_pixel_ratio,
                },
                js,
                state: RefCell::new(State::default()),
            }),
            tokenizer: None,
            decoder: None,
            text: String::new(),
            input: BufferQueue::default(),
        }
    }

    /// The next piece of the document (`head`: the response, without its
    /// body). The first call, with no bytes, makes the document the
    /// loader's client, so the requests carry its origin and referrer.
    pub fn feed(&mut self, head: &Resource, bytes: &[u8]) {
        if self.tokenizer.is_none() {
            let Some(sink) = self.sink.take() else {
                return;
            };
            if !head
                .content_type
                .as_deref()
                .is_none_or(|t| t == "text/html")
            {
                return;
            }
            let policy = head
                .header("referrer-policy")
                .and_then(ReferrerPolicy::parse_header)
                .unwrap_or_default();
            sink.loader.set_client(Some(Client {
                url: head.url.clone(),
                referrer_policy: policy,
            }));
            {
                let mut st = sink.state.borrow_mut();
                st.document = Some(head.url.clone());
                st.base = Some(head.url.clone());
            }
            self.decoder = Some(HtmlStreamDecoder::new(head.charset.as_deref()));
            self.tokenizer = Some(Tokenizer::new(sink, TokenizerOpts::default()));
        }
        self.decode(bytes, false);
    }

    /// The document is in: scan what is left and start the requests that
    /// waited for the body.
    pub fn finish(&mut self) {
        if self.decoder.is_none() {
            return;
        }
        self.decode(&[], true);
        self.decoder = None;
        if let Some(t) = &self.tokenizer {
            t.end();
            t.sink.body_started();
        }
    }

    fn decode(&mut self, bytes: &[u8], last: bool) {
        let (Some(decoder), Some(tokenizer)) = (&mut self.decoder, &self.tokenizer) else {
            return;
        };
        if bytes.is_empty() && !last {
            return;
        }
        decoder.push(bytes, last, &mut self.text);
        if self.text.is_empty() || tokenizer.sink.state.borrow().stopped {
            self.text.clear();
            return;
        }
        self.input.push_back(StrTendril::from(self.text.as_str()));
        self.text.clear();
        let _ = tokenizer.feed(&self.input);
    }
}

/// A request found, before it goes to the loader.
struct Found {
    url: Url,
    kind: ResourceKind,
    cors: CorsSettings,
    priority: Priority,
}

struct Sink {
    loader: Arc<Loader>,
    screen: Screen,
    js: bool,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    document: Option<Url>,
    /// `<base href>`, else the document URL.
    base: Option<Url>,
    base_set: bool,
    /// Open `<template>`, `<svg>`/`<math>` and `<picture>` elements.
    template: usize,
    foreign: usize,
    picture: usize,
    /// The parser has left the head: requests go at once.
    in_body: bool,
    /// A Content-Security-Policy `<meta>` was seen: start nothing more.
    stopped: bool,
    /// Requests found in the head that do not block the first paint.
    waiting: Vec<Found>,
    /// Origins warmed up for requests that wait.
    preconnected: HashSet<String>,
    /// What went to the loader, in order (the tests read it).
    #[cfg(test)]
    started: Vec<(String, ResourceKind, CorsSettings, Priority)>,
}

impl Sink {
    fn body_started(&self) {
        let waiting = {
            let mut st = self.state.borrow_mut();
            st.in_body = true;
            std::mem::take(&mut st.waiting)
        };
        for f in waiting {
            self.start(f);
        }
    }

    /// Hand a request to the loader. Ones that do not block the first paint
    /// wait while the head is scanned, so the head's stylesheets go first
    /// (the loader then holds them back while those are in flight).
    fn found(&self, f: Found) {
        {
            let mut st = self.state.borrow_mut();
            if f.priority != Priority::Blocking {
                let origin = f.url.origin();
                let other = st.document.as_ref().is_none_or(|d| d.origin() != origin);
                if other && st.preconnected.insert(origin.ascii_serialization()) {
                    self.loader.preconnect(&f.url);
                }
                if !st.in_body {
                    st.waiting.push(f);
                    return;
                }
            }
        }
        self.start(f);
    }

    fn start(&self, f: Found) {
        #[cfg(test)]
        self.state
            .borrow_mut()
            .started
            .push((f.url.to_string(), f.kind, f.cors, f.priority));
        self.loader.preload(&f.url, f.kind, f.cors, f.priority);
    }

    fn resolve(&self, href: Option<&str>) -> Option<Url> {
        let href = href.filter(|h| !h.trim().is_empty())?;
        let st = self.state.borrow();
        huginn_net::resolve_url(st.base.as_ref()?, href)
            .filter(|u| matches!(u.scheme(), "http" | "https"))
    }

    fn media_matches(&self, el: &ElementData) -> bool {
        el.attr(&local_name!("media"))
            .is_none_or(|m| huginn_net::media_matches(m, &self.screen))
    }

    fn start_tag(&self, el: &ElementData) {
        let name = &el.name.local;
        if *name == local_name!("base") {
            let href = el.attr(&local_name!("href"));
            let url = {
                let st = self.state.borrow();
                if st.base_set {
                    return;
                }
                href.and_then(|h| huginn_net::resolve_url(st.document.as_ref()?, h))
            };
            if let Some(url) = url {
                let mut st = self.state.borrow_mut();
                st.base = Some(url);
                st.base_set = true;
            }
            return;
        }
        if *name == local_name!("meta") {
            self.meta(el);
            return;
        }
        if self.state.borrow().stopped {
            return;
        }
        if *name == local_name!("link") {
            self.link(el);
        } else if *name == local_name!("script") {
            self.script(el);
        } else if *name == local_name!("img") {
            self.img(el);
        }
    }

    fn meta(&self, el: &ElementData) {
        let content = el.attr(&local_name!("content"));
        if el
            .attr(&local_name!("http-equiv"))
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("content-security-policy"))
        {
            self.state.borrow_mut().stopped = true;
            return;
        }
        let referrer = el
            .attr(&local_name!("name"))
            .is_some_and(|n| n.trim().eq_ignore_ascii_case("referrer"));
        if let Some(policy) = content
            .filter(|_| referrer)
            .and_then(ReferrerPolicy::parse_token)
        {
            let url = self.state.borrow().document.clone();
            if let Some(url) = url {
                self.loader.set_client(Some(Client {
                    url,
                    referrer_policy: policy,
                }));
            }
        }
    }

    fn link(&self, el: &ElementData) {
        let rel = el
            .attr(&local_name!("rel"))
            .unwrap_or("")
            .to_ascii_lowercase();
        let rels: Vec<&str> = rel.split_ascii_whitespace().collect();
        let has = |r: &str| rels.contains(&r);
        let href = el.attr(&local_name!("href"));
        let cors = CorsSettings::from_attr(el.attr(&local_name!("crossorigin")));
        if has("stylesheet") {
            if has("alternate") || el.attr(&local_name!("disabled")).is_some() {
                return;
            }
            let Some(url) = self.resolve(href) else {
                return;
            };
            let priority = if self.media_matches(el) {
                Priority::Blocking
            } else {
                Priority::Low
            };
            self.found(Found {
                url,
                kind: ResourceKind::Stylesheet,
                cors,
                priority,
            });
        } else if has("modulepreload") {
            let as_script = el
                .attr(&local_name!("as"))
                .is_none_or(|a| a.trim().eq_ignore_ascii_case("script"));
            if !self.js || !as_script {
                return;
            }
            let Some(url) = self.resolve(href) else {
                return;
            };
            self.found(Found {
                url,
                kind: ResourceKind::Script,
                // Modules are CORS requests, `anonymous` by default.
                cors: match cors {
                    CorsSettings::NoCors => CorsSettings::Anonymous,
                    c => c,
                },
                priority: Priority::Low,
            });
        } else if has("preload") {
            self.preload_link(el, href, cors);
        } else if has("preconnect") {
            if let Some(url) = self.resolve(href) {
                self.loader.preconnect(&url);
            }
        } else if has("dns-prefetch")
            && let Some(url) = self.resolve(href)
        {
            self.loader.dns_prefetch(&url);
        }
    }

    /// `<link rel=preload as=...>`, when the parser's request can take it.
    fn preload_link(&self, el: &ElementData, href: Option<&str>, cors: CorsSettings) {
        if !self.media_matches(el) {
            return;
        }
        let ty = el
            .attr(&local_name!("type"))
            .map(|t| t.trim().to_ascii_lowercase())
            .filter(|t| !t.is_empty());
        let as_ = el
            .attr(&local_name!("as"))
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let (kind, priority) = match as_.as_str() {
            "style" if ty.as_deref().is_none_or(|t| t == "text/css") => {
                (ResourceKind::Stylesheet, Priority::High)
            }
            "script" if self.js => (ResourceKind::Script, Priority::Low),
            // Web fonts are CORS requests without credentials.
            "font"
                if cors == CorsSettings::Anonymous
                    && ty.as_deref().is_none_or(|t| {
                        matches!(
                            t,
                            "font/woff2" | "font/woff" | "font/ttf" | "font/otf" | "font/sfnt"
                        )
                    }) =>
            {
                (ResourceKind::Font, Priority::High)
            }
            // Images are `no-cors` requests.
            "image"
                if cors == CorsSettings::NoCors
                    && ty
                        .as_deref()
                        .is_none_or(crate::resources::supported_image_type) =>
            {
                (ResourceKind::Image, Priority::Low)
            }
            _ => return,
        };
        let url = if kind == ResourceKind::Image
            && let Some(srcset) = el.attr(&local_name!("imagesrcset"))
        {
            let base = self.state.borrow().base.clone();
            let Some(base) = base else {
                return;
            };
            crate::resources::choose_from_srcset(
                srcset,
                el.attr(&local_name!("imagesizes")),
                href.filter(|h| !h.trim().is_empty()),
                None,
                &base,
                &self.screen,
            )
            .map(|(u, _, _)| u)
        } else {
            self.resolve(href)
        };
        if let Some(url) = url.filter(|u| matches!(u.scheme(), "http" | "https")) {
            self.found(Found {
                url,
                kind,
                cors,
                priority,
            });
        }
    }

    fn script(&self, el: &ElementData) {
        if !self.js {
            return;
        }
        let Some(kind) = script_kind(el) else {
            return;
        };
        if kind == ScriptKind::ImportMap {
            return;
        }
        let Some(url) = self.resolve(el.attr(&local_name!("src"))) else {
            return;
        };
        // Parser-blocking: a classic script without `async` or `defer`
        // (`huginn_js::collect_scripts`).
        let blocking = kind == ScriptKind::Classic
            && el.attr(&local_name!("async")).is_none()
            && el.attr(&local_name!("defer")).is_none();
        self.found(Found {
            url,
            kind: ResourceKind::Script,
            cors: script_cors(el, kind),
            priority: if blocking {
                Priority::Blocking
            } else {
                Priority::Low
            },
        });
    }

    fn img(&self, el: &ElementData) {
        let lazy = el
            .attr(&local_name!("loading"))
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("lazy"));
        if lazy || self.state.borrow().picture > 0 || el.attr(&local_name!("crossorigin")).is_some()
        {
            return;
        }
        let src = el
            .attr(&local_name!("src"))
            .filter(|s| !s.trim().is_empty());
        let url = match el.attr(&local_name!("srcset")) {
            Some(srcset) => {
                let base = self.state.borrow().base.clone();
                let Some(base) = base else {
                    return;
                };
                let width = el
                    .attr(&local_name!("width"))
                    .and_then(|v| v.trim().trim_end_matches("px").parse::<f32>().ok())
                    .filter(|v| v.is_finite() && *v >= 0.0);
                crate::resources::choose_from_srcset(
                    srcset,
                    el.attr(&local_name!("sizes")),
                    src,
                    width,
                    &base,
                    &self.screen,
                )
                .map(|(u, _, _)| u)
                .or_else(|| self.resolve(src))
            }
            None => self.resolve(src),
        };
        if let Some(url) = url.filter(|u| matches!(u.scheme(), "http" | "https")) {
            self.found(Found {
                url,
                kind: ResourceKind::Image,
                cors: CorsSettings::NoCors,
                priority: Priority::Low,
            });
        }
    }
}

impl TokenSink for Sink {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let Token::TagToken(Tag {
            kind,
            name,
            self_closing,
            attrs,
            ..
        }) = token
        else {
            return TokenSinkResult::Continue;
        };
        let foreign = self.state.borrow().foreign > 0;
        if kind == TagKind::EndTag {
            let mut st = self.state.borrow_mut();
            let depth = if name == local_name!("svg") || name == local_name!("math") {
                &mut st.foreign
            } else if foreign {
                return TokenSinkResult::Continue;
            } else if name == local_name!("template") {
                &mut st.template
            } else if name == local_name!("picture") {
                &mut st.picture
            } else {
                return TokenSinkResult::Continue;
            };
            *depth = depth.saturating_sub(1);
            return TokenSinkResult::Continue;
        }
        if name == local_name!("svg") || name == local_name!("math") {
            if !self_closing {
                self.state.borrow_mut().foreign += 1;
            }
            return TokenSinkResult::Continue;
        }
        if foreign {
            return TokenSinkResult::Continue;
        }
        let in_head = matches!(
            name,
            local_name!("html")
                | local_name!("head")
                | local_name!("title")
                | local_name!("meta")
                | local_name!("link")
                | local_name!("style")
                | local_name!("script")
                | local_name!("noscript")
                | local_name!("base")
                | local_name!("template")
        );
        if !in_head && !self.state.borrow().in_body {
            self.body_started();
        }
        let raw = match name {
            local_name!("script") => TokenSinkResult::RawData(RawKind::ScriptData),
            local_name!("style")
            | local_name!("noscript")
            | local_name!("xmp")
            | local_name!("iframe")
            | local_name!("noembed")
            | local_name!("noframes") => TokenSinkResult::RawData(RawKind::Rawtext),
            local_name!("title") | local_name!("textarea") => {
                TokenSinkResult::RawData(RawKind::Rcdata)
            }
            local_name!("plaintext") => TokenSinkResult::Plaintext,
            _ => TokenSinkResult::Continue,
        };
        if name == local_name!("template") {
            self.state.borrow_mut().template += 1;
            return raw;
        }
        if name == local_name!("picture") {
            self.state.borrow_mut().picture += 1;
        }
        if self.state.borrow().template > 0 {
            return raw;
        }
        let el = ElementData {
            name: QualName::new(None, ns!(html), name),
            attrs,
            template_contents: None,
            shadow_root: None,
        };
        self.start_tag(&el);
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use CorsSettings::{Anonymous, NoCors, UseCredentials};
    use Priority::{Blocking, High, Low};
    use ResourceKind::{Font, Image, Script, Stylesheet};

    type Started = Vec<(String, ResourceKind, CorsSettings, Priority)>;

    /// Scan `html` from http://127.0.0.1:1/dir/page, fed `piece` bytes at
    /// a time: what went to the loader, in order, and how many of those
    /// went before the end of the document. Port 1 refuses the requests.
    fn scan(html: &str, piece: usize, js: bool) -> (Started, usize) {
        let loader = Arc::new(Loader::loopback_only());
        let screen = Screen {
            width: 1280.0,
            height: 800.0,
            device_pixel_ratio: 1.0,
        };
        let mut s = Scanner::new(&loader, &screen, js);
        let head = Resource {
            url: Url::parse("http://127.0.0.1:1/dir/page").unwrap(),
            status: 200,
            content_type: Some("text/html".into()),
            charset: Some("utf-8".into()),
            headers: Vec::new(),
            response_type: huginn_net::ResponseType::Basic,
            redirected: false,
            body: Vec::new(),
        };
        s.feed(&head, &[]);
        for c in html.as_bytes().chunks(piece) {
            s.feed(&head, c);
        }
        let started = |s: &Scanner| {
            s.tokenizer
                .as_ref()
                .expect("scanning")
                .sink
                .state
                .borrow()
                .started
                .clone()
        };
        let before_end = started(&s).len();
        s.finish();
        let all = started(&s);
        loader.clear_preloads();
        (all, before_end)
    }

    fn url(path: &str) -> String {
        format!("http://127.0.0.1:1{path}")
    }

    #[test]
    fn finds_what_the_parser_will_fetch_with_its_mode_and_priority() {
        let html = "<!doctype html><html><head>
            <link rel=modulepreload href=/m.js>
            <link rel=stylesheet href=a.css>
            <link rel=stylesheet href=/print.css media=print>
            <link rel='alternate stylesheet' href=/alt.css>
            <link rel=stylesheet href=/off.css disabled>
            <link rel=stylesheet href=//127.0.0.1:1/x.css crossorigin>
            <link rel=preload as=font href=/f.woff2 crossorigin>
            <link rel=preload as=font href=/nocors.woff2>
            <link rel=preload as=image href=/hero.png>
            <script src=/block.js></script>
            <script src=/async.js async></script>
            <script type=module src=/mod.js></script>
            <script src=/old.js nomodule></script>
            <script type=application/json src=/data.json></script>
            <script src=/cred.js crossorigin=use-credentials></script>
            </head><body>
            <img src=/eager.png><img src=/lazy.png loading=lazy>
            <picture><source srcset=/p.webp><img src=/p.jpg></picture>
            <img srcset='/s1.png 1x, /s2.png 2x' src=/s.png>
            </body></html>";
        let (got, _) = scan(html, 4096, true);
        let want: Started = vec![
            (url("/dir/a.css"), Stylesheet, NoCors, Blocking),
            (url("/x.css"), Stylesheet, Anonymous, Blocking),
            (url("/block.js"), Script, NoCors, Blocking),
            (url("/cred.js"), Script, UseCredentials, Blocking),
            // The others found in the head wait for the body, in order.
            (url("/m.js"), Script, Anonymous, Low),
            (url("/print.css"), Stylesheet, NoCors, Low),
            (url("/f.woff2"), Font, Anonymous, High),
            (url("/hero.png"), Image, NoCors, Low),
            (url("/async.js"), Script, NoCors, Low),
            (url("/mod.js"), Script, Anonymous, Low),
            (url("/eager.png"), Image, NoCors, Low),
            (url("/s1.png"), Image, NoCors, Low),
        ];
        assert_eq!(got, want);
        // Without scripts, no script is worth fetching.
        let (got, _) = scan(html, 4096, false);
        assert!(got.iter().all(|(_, k, ..)| *k != Script), "{got:?}");
    }

    #[test]
    fn text_that_is_not_markup_starts_nothing() {
        let html = "<head><title><link rel=stylesheet href=/t.css></title>
            <script>document.write('<link rel=stylesheet href=/s.css>')</script>
            <style>/* <link rel=stylesheet href=/st.css> */</style>
            <noscript><link rel=stylesheet href=/ns.css></noscript>
            <!-- <link rel=stylesheet href=/c.css> -->
            <template><link rel=stylesheet href=/tpl.css></template>
            </head><body><textarea><img src=/ta.png></textarea>
            <svg><image href=/i.svg /><script href=/svg.js></script></svg>
            <link rel=stylesheet href=/real.css></body>";
        let (got, _) = scan(html, 4096, true);
        assert_eq!(got, vec![(url("/real.css"), Stylesheet, NoCors, Blocking)]);
    }

    #[test]
    fn base_href_applies_after_it_and_a_csp_meta_stops_the_scan() {
        let html = "<head><link rel=stylesheet href=one.css><base href=/b/>
            <link rel=stylesheet href=two.css><base href=/ignored/>
            <link rel=stylesheet href=three.css>
            <meta http-equiv=Content-Security-Policy content=\"style-src 'self'\">
            <link rel=stylesheet href=four.css></head>";
        let (got, _) = scan(html, 4096, true);
        let urls: Vec<&str> = got.iter().map(|(u, ..)| u.as_str()).collect();
        assert_eq!(
            urls,
            [url("/dir/one.css"), url("/b/two.css"), url("/b/three.css")]
        );
    }

    #[test]
    fn pieces_of_any_size_find_the_same_and_start_before_the_end() {
        let html = format!(
            "<head><link rel=stylesheet href=/a.css><script src=/b.js></script></head><body>{}\
             <link rel=stylesheet href=/late.css></body>",
            "<p>text</p>".repeat(500)
        );
        let (whole, _) = scan(&html, html.len(), true);
        for piece in [1, 3, 64, 1500] {
            let (got, before_end) = scan(&html, piece, true);
            assert_eq!(got, whole, "piece {piece}");
            // Everything was started while the document was still coming.
            assert_eq!(before_end, got.len(), "piece {piece}");
        }
    }
}
