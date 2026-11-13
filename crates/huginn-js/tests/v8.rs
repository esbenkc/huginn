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

#[test]
fn navigator_reports_the_user_agent_and_brands_the_network_sends() {
    let mut p = Page::new("<body></body>");
    p.run(
        r#"
        const b = document.body, d = navigator.userAgentData;
        b.setAttribute('data-ua', navigator.userAgent);
        b.setAttribute('data-brands', d.brands.map(x => `"${x.brand}";v="${x.version}"`).join(', '));
        d.getHighEntropyValues(['fullVersionList', 'uaFullVersion']).then(v => {
          b.setAttribute('data-full', v.uaFullVersion + '|' +
            v.fullVersionList.map(x => x.brand + ' ' + x.version).join(', '));
        });
        "#,
    )
    .unwrap();
    let attr = |name: &str| {
        p.doc
            .attribute(p.doc.body().unwrap(), &huginn_dom::LocalName::from(name))
            .map(str::to_owned)
    };
    let ua = huginn_net::ua::USER_AGENT;
    assert_eq!(attr("data-ua").as_deref(), Some(ua));
    assert_eq!(
        attr("data-brands").as_deref(),
        Some(huginn_net::ua::SEC_CH_UA)
    );
    let chrome = huginn_net::ua::CHROME_MAJOR;
    assert_eq!(
        attr("data-full").as_deref(),
        Some(
            format!(
                "{chrome}.0.0.0|Chromium {chrome}.0.0.0, Huginn {}, Not_A Brand 24.0.0.0",
                huginn_net::ua::HUGINN_FULL_VERSION
            )
            .as_str()
        )
    );
    // The Chromium brand version is the Chrome release whose V8 runs the
    // page's scripts (V8 15.2 ships in Chrome 152).
    let mut v8 = v8::V8::get_version().split('.');
    let (major, minor) = (v8.next().unwrap(), v8.next().unwrap());
    assert_eq!(
        format!("{major}{minor}"),
        chrome,
        "bump CHROME_MAJOR with the v8 crate"
    );
}

impl Page {
    fn body_attr(&self, name: &str) -> Option<String> {
        self.doc
            .attribute(self.doc.body().unwrap(), &huginn_dom::LocalName::from(name))
            .map(str::to_owned)
    }
}

/// Runs async checks; each pushes a failure description into `fails`. The
/// result lands in `data-fails` once every check settled.
const ASYNC_CHECKS: &str = r#"
const fails = [];
const checks = [];
const check = (name, f) => checks.push(Promise.resolve().then(f).then(
    r => { if (r !== true) fails.push(name + ': ' + r); },
    e => fails.push(name + ' threw ' + e)));
const done = () => Promise.all(checks).then(() => document.body.setAttribute('data-fails', JSON.stringify(fails)));
"#;

#[test]
fn streams_pipe_tee_bytes_and_bodies() {
    let mut p = Page::new("<body></body>");
    let js = format!(
        "{ASYNC_CHECKS}{}",
        r#"
        const collect = async rs => { const out = []; for await (const x of rs) out.push(x); return out; };
        check('pipeThrough with backpressure', async () => {
            let pulls = 0;
            const rs = new ReadableStream({ pull(c) { pulls++; if (pulls <= 3) c.enqueue('ab' + pulls); else c.close(); } }, { highWaterMark: 1 });
            const up = new TransformStream({ transform(ch, c) { c.enqueue(ch.toUpperCase()); }, flush(c) { c.enqueue('!'); } });
            const out = (await collect(rs.pipeThrough(up))).join(' ');
            return out === 'AB1 AB2 AB3 !' || out;
        });
        check('pipeTo waits for slow writes and closes', async () => {
            const got = [];
            let closed = false;
            const ws = new WritableStream({ write(ch) { got.push(ch); return new Promise(r => setTimeout(r, 2)); }, close() { closed = true; } });
            const rs = new ReadableStream({ start(c) { [1, 2, 3].forEach(x => c.enqueue(x)); c.close(); } });
            await rs.pipeTo(ws);
            return (got.join() === '1,2,3' && closed) || got.join() + ' ' + closed;
        });
        check('errors propagate through pipes', async () => {
            let aborted;
            const rs = new ReadableStream({ start(c) { c.error(new Error('boom')); } });
            const r = await rs.pipeTo(new WritableStream({ abort(e) { aborted = e.message; } })).then(() => 'resolved', e => e.message);
            return (r === 'boom' && aborted === 'boom') || r + ' ' + aborted;
        });
        check('tee gives both branches every chunk', async () => {
            const [a, b] = new ReadableStream({ start(c) { c.enqueue(1); c.enqueue(2); c.close(); } }).tee();
            const [x, y] = await Promise.all([collect(a), collect(b)]);
            return (x.join() === '1,2' && y.join() === '1,2') || x + '|' + y;
        });
        check('byob reads fill the given view', async () => {
            const rs = new ReadableStream({ type: 'bytes', start(c) { c.enqueue(new Uint8Array([1, 2])); c.enqueue(new Uint8Array([3, 4, 5])); c.close(); } });
            const reader = rs.getReader({ mode: 'byob' });
            const { value } = await reader.read(new Uint16Array(4), { min: 2 });
            const end = await reader.read(new Uint8Array(1));
            return (value instanceof Uint16Array && value.length === 2 && end.value.length === 1 && end.value[0] === 5)
                || [value.length, end.value && end.value.length].join();
        });
        check('a locked stream cannot get a second reader', () => {
            const rs = new ReadableStream();
            rs.getReader();
            try { rs.getReader(); return 'second reader'; } catch (e) { return e instanceof TypeError; }
        });
        check('Response bodies are byte streams', async () => {
            const r = new Response('h\u00e9');
            const { value } = await r.body.getReader().read();
            const used = await r.text().then(() => 'read twice', e => e instanceof TypeError);
            return (value instanceof Uint8Array && value.length === 3 && r.bodyUsed && used === true) || [value, used].join();
        });
        check('Response from a stream, cloned', async () => {
            const enc = new TextEncoder();
            const r = new Response(new ReadableStream({ start(c) { c.enqueue(enc.encode('{"a":')); c.enqueue(enc.encode('1}')); c.close(); } }));
            const copy = r.clone();
            const [j, t] = await Promise.all([r.json(), copy.text()]);
            return (j.a === 1 && t === '{"a":1}') || t;
        });
        check('binary bodies stay binary', async () => {
            const b = new Uint8Array(await new Response(new Uint8Array([0xff, 0, 0x80])).arrayBuffer());
            const s = await new Response(new Blob(['ab', new Uint8Array([0xe2, 0x82, 0xac])]).stream()).text();
            return (b.join() === '255,0,128' && s === 'ab\u20ac') || b.join() + ' ' + s;
        });
        check('text streams split characters across chunks', async () => {
            const parts = ['h\u00e9 ', '\ud83d', '\ude00!'];
            const rs = new ReadableStream({ start(c) { parts.forEach(x => c.enqueue(x)); c.close(); } });
            const bytes = rs.pipeThrough(new TextEncoderStream());
            // Re-chunk byte by byte so every multi-byte character is split.
            const single = bytes.pipeThrough(new TransformStream({ transform(ch, c) { for (const b of ch) c.enqueue(new Uint8Array([b])); } }));
            const out = (await collect(single.pipeThrough(new TextDecoderStream()))).join('');
            return out === parts.join('') || JSON.stringify(out);
        });
        check('compression round trip and truncation', async () => {
            const text = 'stream '.repeat(200);
            const pack = (f, b) => new Response(new Blob([b]).stream().pipeThrough(new CompressionStream(f))).arrayBuffer();
            const unpack = (f, b) => new Response(new Blob([b]).stream().pipeThrough(new DecompressionStream(f))).text();
            for (const f of ['gzip', 'deflate', 'deflate-raw']) {
                const packed = await pack(f, text);
                if (packed.byteLength >= text.length || await unpack(f, packed) !== text) return f + ' round trip';
            }
            const cut = new Uint8Array(await pack('gzip', text)).subarray(0, 12);
            return await unpack('gzip', cut).then(() => 'truncated input accepted', e => e instanceof TypeError);
        });
        done();
        "#
    );
    p.run(&js).unwrap();
    p.load(200.0);
    assert_eq!(p.body_attr("data-fails").as_deref(), Some("[]"));
}

#[test]
fn text_decoder_streams_and_legacy_encodings() {
    let mut p = Page::new("<body></body>");
    p.run(
        r#"
        const out = [];
        const euro = new TextEncoder().encode('a\u20acb');
        const d = new TextDecoder();
        out.push(d.decode(euro.subarray(0, 2), { stream: true }) + '|' + d.decode(euro.subarray(2, 3), { stream: true }) + '|' + d.decode(euro.subarray(3)));
        out.push(new TextDecoder().decode(new Uint8Array([0xef, 0xbb, 0xbf, 0x41, 0xff])));
        try { new TextDecoder('utf-8', { fatal: true }).decode(new Uint8Array([0xc0, 0x80])); out.push('no error'); } catch (e) { out.push(e.name); }
        const w = new TextDecoder(' Latin1 ');
        out.push(w.encoding + ':' + w.decode(new Uint8Array([0x80, 0xe6])));
        out.push(new TextDecoder('utf-16le').decode(new Uint8Array([0x3d, 0xd8, 0x00, 0xde])));
        out.push(new TextDecoder('shift_jis').decode(new Uint8Array([0x82, 0xa0])));
        try { new TextDecoder('no-such-encoding'); out.push('accepted'); } catch (e) { out.push(e.name); }
        const into = new Uint8Array(4);
        const r = new TextEncoder().encodeInto('a\u20acb', into);
        out.push(r.read + ',' + r.written);
        document.body.setAttribute('data-out', out.join(' / '));
        "#,
    )
    .unwrap();
    assert_eq!(
        p.body_attr("data-out").as_deref(),
        Some(
            "a||\u{20ac}b / A\u{FFFD} / TypeError / windows-1252:\u{20ac}\u{e6} / \u{1F600} / \u{3042} / RangeError / 2,4"
        )
    );
}

#[test]
fn import_maps_remap_static_and_dynamic_imports() {
    let mut p = Page::new("<body></body>");
    let module = |text: &str| format!("data:text/javascript,{}", text.replace(' ', "%20"));
    let first = format!(
        r#"{{"imports": {{"lib": "{}", "dup": "{}"}}}}"#,
        module("export default 'mapped'"),
        module("export default 1")
    );
    let second = format!(
        r#"{{"imports": {{"dup": "{}", "late": "{}"}}}}"#,
        module("export default 2"),
        module("export default 'late'")
    );
    for map in [&first, &second] {
        p.call(|e, cx| {
            e.execute(
                cx,
                map,
                "http://127.0.0.1:9/dir/page.html",
                ScriptKind::ImportMap,
            )
        })
        .unwrap();
    }
    // An invalid map is an error and changes nothing.
    assert!(
        p.call(|e, cx| e.execute(
            cx,
            "[1]",
            "http://127.0.0.1:9/dir/page.html",
            ScriptKind::ImportMap
        ))
        .is_err()
    );
    p.call(|e, cx| {
        e.execute(
            cx,
            "import v from 'lib'; import d from 'dup'; document.body.setAttribute('data-static', v + d); document.body.setAttribute('data-meta', import.meta.url + ' ' + import.meta.resolve('lib').slice(0, 20));",
            "http://127.0.0.1:9/dir/page.html",
            ScriptKind::Module,
        )
    })
    .unwrap();
    p.run(
        r#"
        import('late').then(m => document.body.setAttribute('data-late', m.default));
        import('unmapped').catch(e => document.body.setAttribute('data-unmapped', e.name));
        "#,
    )
    .unwrap();
    // `import()` settles in a timer round; due times follow real time spent.
    p.load(1000.0);
    // The first map's "dup" wins: later maps cannot redefine a specifier.
    assert_eq!(p.body_attr("data-static").as_deref(), Some("mapped1"));
    assert_eq!(
        p.body_attr("data-meta").as_deref(),
        Some("http://127.0.0.1:9/dir/page.html data:text/javascript")
    );
    assert_eq!(p.body_attr("data-late").as_deref(), Some("late"));
    assert_eq!(p.body_attr("data-unmapped").as_deref(), Some("TypeError"));
}

#[test]
fn custom_element_upgrade_only_upgrades_new_elements() {
    let mut p = Page::new("<body><x-a></x-a></body>");
    p.run(
        r#"
        let connected = 0;
        customElements.define('x-a', class extends HTMLElement {
            // Libraries call upgrade() from connectedCallback; it must not
            // connect the element again.
            connectedCallback() { connected++; customElements.upgrade(this); }
        });
        const loose = document.createElement('div');
        loose.innerHTML = '<x-a></x-a>';
        customElements.upgrade(loose);
        document.body.setAttribute('data-out', [connected, loose.firstChild.connectedCallback !== undefined].join());
        "#,
    )
    .unwrap();
    assert_eq!(p.body_attr("data-out").as_deref(), Some("1,true"));
}
