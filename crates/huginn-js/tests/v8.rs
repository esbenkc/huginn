//! Engine behavior through the public `ScriptEngine` API (needs `--features v8`).
#![cfg(feature = "v8")]

use huginn_dom::Document;
use huginn_js::{
    EngineOptions, Event, EventTarget, HostContext, ScriptEngine, ScriptError, ScriptKind,
    create_engine,
};
use huginn_net::{Loader, Url};

struct Page {
    doc: Document,
    loader: Loader,
    url: Url,
    engine: Box<dyn ScriptEngine>,
    now: f64,
}

impl Page {
    fn new(html: &str) -> Page {
        Page::with_options(html, EngineOptions::default())
    }

    fn with_options(html: &str, options: EngineOptions) -> Page {
        Page {
            doc: huginn_dom::parse_html(html),
            loader: Loader::new(),
            url: Url::parse("http://127.0.0.1:9/dir/page.html").unwrap(),
            engine: create_engine(options).expect("built with the v8 feature"),
            now: 0.0,
        }
    }

    fn call<R>(&mut self, f: impl FnOnce(&mut dyn ScriptEngine, &mut HostContext<'_>) -> R) -> R {
        let mut cx = HostContext {
            document: &mut self.doc,
            loader: &self.loader,
            document_url: &self.url,
            base_url: &self.url,
            viewport: (1280.0, 800.0),
            now_ms: self.now,
            layout: None,
            current_script: None,
        };
        f(self.engine.as_mut(), &mut cx)
    }

    fn run(&mut self, js: &str) -> Result<(), ScriptError> {
        let r = self.call(|e, cx| e.execute(cx, js, "test.js", ScriptKind::Classic));
        self.call(|e, cx| e.run_microtasks(cx)).unwrap();
        r
    }

    /// Fire the load events, then run timers for `ms` of virtual time.
    fn load(&mut self, ms: f64) {
        self.call(|e, cx| {
            e.dispatch_event(
                cx,
                EventTarget::Document,
                &Event::new("DOMContentLoaded", true, false),
            )
        })
        .unwrap();
        self.call(|e, cx| {
            e.dispatch_event(cx, EventTarget::Window, &Event::new("load", false, false))
        })
        .unwrap();
        let end = self.now + ms;
        while let Some(due) = self.engine.next_timer_due() {
            if due > end {
                break;
            }
            self.now = self.now.max(due);
            self.call(|e, cx| e.run_timers(cx)).unwrap();
        }
    }

    fn body_html(&self) -> String {
        huginn_dom::serialize(&self.doc, self.doc.body().unwrap(), false)
    }
}

#[test]
fn scripts_build_and_edit_the_dom() {
    let mut p = Page::new("<body><ul id=l><li>a</li></ul></body>");
    p.run(
        r#"
        const ul = document.getElementById('l');
        const li = document.createElement('LI');
        li.className = 'x y';
        li.classList.add('z');
        li.textContent = 'b & c';
        ul.appendChild(li);
        ul.insertAdjacentHTML('beforeend', '<li data-k="v">d</li>');
        ul.lastChild.dataset.extraThing = '1';
        ul.firstElementChild.remove();
        document.body.appendChild(document.createElement('p')).style.marginTop = '4px';
        "#,
    )
    .unwrap();
    assert_eq!(
        p.body_html(),
        r#"<ul id="l"><li class="x y z">b &amp; c</li><li data-k="v" data-extra-thing="1">d</li></ul><p style="margin-top: 4px;"></p>"#
    );
}

#[test]
fn queries_identity_and_dom_errors() {
    let mut p = Page::new("<div id=a class=c><span class=c></span></div>");
    p.run(
        r#"
        const a = document.querySelector('#a');
        if (a !== document.getElementById('a')) throw new Error('identity');
        if (document.querySelectorAll('.c').length !== 2) throw new Error('qsa');
        if (a.firstChild.closest('div') !== a) throw new Error('closest');
        let name = '';
        try { a.firstChild.appendChild(a); } catch (e) { name = e.name; }
        if (name !== 'HierarchyRequestError') throw new Error('cycle allowed: ' + name);
        try { document.querySelector('div >'); } catch (e) { name = e.name; }
        if (name !== 'SyntaxError') throw new Error('bad selector accepted');
        document.body.setAttribute('data-ok', '1');
        "#,
    )
    .unwrap();
    assert_eq!(
        p.doc.attribute(
            p.doc.body().unwrap(),
            &huginn_dom::LocalName::from("data-ok")
        ),
        Some("1")
    );
}

#[test]
fn events_timers_and_microtasks_run_in_order() {
    let mut p = Page::new("<body></body>");
    p.run(
        r#"
        const log = [];
        document.addEventListener('DOMContentLoaded', () => log.push('dcl'));
        window.addEventListener('load', () => log.push('load'));
        document.body.addEventListener('click', e => log.push('bubble:' + e.target.localName));
        Promise.resolve().then(() => log.push('micro'));
        setTimeout(() => { log.push('t10'); }, 10);
        setTimeout(() => { log.push('t0'); Promise.resolve().then(() => log.push('t0-micro')); }, 0);
        requestAnimationFrame(() => log.push('raf'));
        new MessageChannel().port1.postMessage('x');
        const ch = new MessageChannel();
        ch.port1.onmessage = (e) => log.push('msg:' + e.data);
        ch.port2.postMessage('hi');
        setTimeout(() => {
          document.body.appendChild(document.createElement('b')).click();
          document.body.setAttribute('data-log', log.join(','));
        }, 50);
        "#,
    )
    .unwrap();
    p.load(500.0);
    let log = p
        .doc
        .attribute(
            p.doc.body().unwrap(),
            &huginn_dom::LocalName::from("data-log"),
        )
        .unwrap()
        .to_owned();
    assert_eq!(log, "micro,dcl,load,t0,t0-micro,msg:hi,t10,raf,bubble:b");
}

#[test]
fn custom_elements_upgrade_and_connect() {
    let mut p = Page::new("<x-list id=l></x-list>");
    p.run(
        r#"
        class XList extends HTMLElement {
          static get observedAttributes() { return ['n']; }
          connectedCallback() { this.innerHTML = '<i>' + this.getAttribute('n') + '</i>'; }
          attributeChangedCallback(name, old, value) { this.dataset.seen = value; }
        }
        document.getElementById('l').setAttribute('n', '1');
        customElements.define('x-list', XList);
        const made = document.createElement('x-list');
        made.setAttribute('n', '2');
        document.body.appendChild(made);
        if (!(made instanceof XList)) throw new Error('not upgraded');
        "#,
    )
    .unwrap();
    assert_eq!(
        p.body_html(),
        r#"<x-list id="l" n="1" data-seen="1"><i>1</i></x-list><x-list n="2" data-seen="2"><i>2</i></x-list>"#
    );
}

// HTML "define" upgrades candidates in shadow trees too, and insertion into a
// connected shadow root connects (and upgrades) custom elements; nodes in a
// shadow tree are connected when their host is.
#[test]
fn custom_elements_in_shadow_trees_upgrade_and_connect() {
    let mut p = Page::new("<x-host id=h></x-host>");
    p.run(
        r#"
        const log = [];
        const root = document.getElementById('h').attachShadow({ mode: 'open' });
        root.innerHTML = '<x-early></x-early>';
        const early = root.firstChild;
        if (!early.isConnected) throw new Error('shadow child not connected');
        if (early.getRootNode() !== root) throw new Error('root');
        if (early.getRootNode({ composed: true }) !== document) throw new Error('composed root');
        customElements.define('x-early', class extends HTMLElement {
          connectedCallback() { log.push('early'); this.attachShadow({ mode: 'open' }).appendChild(document.createElement('x-late')); }
        });
        customElements.define('x-late', class extends HTMLElement {
          connectedCallback() { log.push('late'); }
          disconnectedCallback() { log.push('late-gone'); }
        });
        const late = early.shadowRoot.firstChild;
        if (customElements.get('x-late') !== late.constructor) throw new Error('nested shadow element not upgraded');
        root.appendChild(document.createElement('x-late'));
        const detached = document.createElement('div').attachShadow({ mode: 'open' });
        detached.appendChild(document.createElement('x-late'));
        early.remove();
        document.body.dataset.log = log.join(',');
        "#,
    )
    .unwrap();
    let body = p.doc.body().unwrap();
    assert_eq!(
        p.doc
            .attribute(body, &huginn_dom::LocalName::from("data-log")),
        Some("early,late,late,late-gone")
    );
}

// importNode (and cloneNode outside template contents) upgrade the copies
// before returning, so on insertion a connectedCallback sees its later
// siblings already upgraded. Clones inside a template's inert contents wait
// for insertion.
#[test]
fn clones_upgrade_custom_elements_before_insertion() {
    let mut p = Page::new("<template id=t><x-tab></x-tab><x-panel></x-panel></template>");
    p.run(
        r#"
        class XPanel extends HTMLElement {}
        customElements.define('x-panel', XPanel);
        customElements.define('x-tab', class extends HTMLElement {
          connectedCallback() { this.dataset.linked = this.nextElementSibling instanceof XPanel; }
        });
        const content = document.getElementById('t').content;
        const inert = content.cloneNode(true);
        if (inert.lastChild instanceof XPanel) throw new Error('template contents clone upgraded');
        document.body.appendChild(document.importNode(content, true));
        document.body.appendChild(inert);
        const plain = document.createElement('div');
        plain.innerHTML = '<x-panel></x-panel>';
        if (plain.firstChild instanceof XPanel) throw new Error('disconnected innerHTML upgraded');
        if (!(plain.cloneNode(true).firstChild instanceof XPanel)) throw new Error('document clone not upgraded');
        "#,
    )
    .unwrap();
    let html = p.body_html();
    assert!(
        html.ends_with(
            r#"<x-tab data-linked="true"></x-tab><x-panel></x-panel><x-tab data-linked="false"></x-tab><x-panel></x-panel>"#
        ),
        "{html}"
    );
}

#[test]
fn inserted_inline_scripts_run_and_innerhtml_scripts_do_not() {
    let mut p = Page::new("<body></body>");
    p.run(
        r#"
        const s = document.createElement('script');
        s.textContent = 'document.body.setAttribute("data-ran", "yes")';
        document.body.appendChild(s);
        document.body.insertAdjacentHTML('beforeend', '<script>document.body.setAttribute("data-bad", "1")</script>');
        "#,
    )
    .unwrap();
    let body = p.doc.body().unwrap();
    assert_eq!(
        p.doc
            .attribute(body, &huginn_dom::LocalName::from("data-ran")),
        Some("yes")
    );
    assert_eq!(
        p.doc
            .attribute(body, &huginn_dom::LocalName::from("data-bad")),
        None
    );
}

#[test]
fn errors_are_reported_and_runaway_scripts_stopped() {
    let mut p = Page::with_options(
        "<body></body>",
        EngineOptions {
            call_budget_ms: 200,
            console: false,
        },
    );
    assert!(matches!(
        p.run("let x = ;"),
        Err(ScriptError::Compile { .. })
    ));
    match p.run("null.f()") {
        Err(ScriptError::Runtime { message, .. }) => {
            assert!(message.contains("TypeError"), "{message}")
        }
        other => panic!("expected a runtime error, got {other:?}"),
    }
    let t = std::time::Instant::now();
    assert!(p.run("for (;;) {}").is_err());
    assert!(t.elapsed().as_millis() < 2000);
    // The engine still works after a terminated script.
    p.run("document.body.id = 'alive'").unwrap();
    assert!(p.doc.element_by_id("alive").is_some());
}

#[test]
fn location_url_and_storage() {
    let mut p = Page::new("<body></body>");
    p.run(
        r#"
        const u = new URL('../x.js?a=1#h', location.href);
        u.searchParams.append('b', 'two words');
        localStorage.setItem('k', 'v');
        history.pushState({}, '', '/other?q');
        const out = [u.href, location.pathname, localStorage.getItem('k'), matchMedia('(min-width: 1000px)').matches,
                     document.querySelector('body').tagName, btoa('hi'), new TextDecoder().decode(new TextEncoder().encode('é'))];
        document.body.setAttribute('data-out', out.join('|'));
        "#,
    )
    .unwrap();
    let out = p
        .doc
        .attribute(
            p.doc.body().unwrap(),
            &huginn_dom::LocalName::from("data-out"),
        )
        .unwrap()
        .to_owned();
    assert_eq!(
        out,
        "http://127.0.0.1:9/x.js?a=1&b=two+words#h|/other|v|true|BODY|aGk=|é"
    );
}
