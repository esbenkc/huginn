//! The startup snapshot against running the prelude in each isolate.

use std::sync::Arc;

use huginn_dom::local_name;
use huginn_net::{Loader, Url};

use super::{PRELUDE, V8Engine};
use crate::{EngineOptions, HostContext, ScriptEngine, ScriptKind};

/// Everything a page script can see of the prelude: each global and the
/// members of each interface (names, order, kinds), values that come from
/// the page and the build, DOM operations, events, errors and their stack
/// traces, native codecs and the lazily built streams. The result is JSON
/// in `document.title`.
const PROBE: &str = r#"
const kind = (o, k) => {
  const d = Object.getOwnPropertyDescriptor(o, k);
  return (d.get ? 'g' : '') + (d.set ? 's' : '') + ('value' in d ? typeof d.value : '') +
    (d.enumerable ? 'E' : '') + (d.writable ? 'W' : '') + (d.configurable ? 'C' : '');
};
const members = (o) => Reflect.ownKeys(o).map(k => String(k) + ':' + kind(o, k));
// V8 adds these to every context it makes, after the globals of a startup
// snapshot (Chrome 157 has Temporal there too), so only their place differs.
const perContext = ['Temporal', 'SuppressedError', 'DisposableStack', 'AsyncDisposableStack', 'Float16Array',
  'SharedArrayBuffer'];
const names = Reflect.ownKeys(globalThis).filter(k => !perContext.includes(k));
const out = {
  globals: names.map(k => String(k) + ':' + kind(globalThis, k)),
  perContext: perContext.map(k => k in globalThis ? kind(globalThis, k) : 'absent'),
  interfaces: {},
};
for (const k of names) {
  const C = globalThis[k];
  if (typeof C === 'function' && C.prototype) out.interfaces[k] = [members(C), members(C.prototype)];
}
out.navigator = members(Navigator.prototype);
out.windowProto = Object.getPrototypeOf(globalThis) === Window.prototype && globalThis === window &&
  window.self === window && document.defaultView === window;
const ua = navigator.userAgentData;
out.page = [location.href, location.origin, document.URL, innerWidth, innerHeight, screen.width,
  screen.availHeight, matchMedia('(min-width: 1000px)').matches, navigator.userAgent, navigator.appVersion,
  navigator.platform, JSON.stringify(ua), Math.abs(performance.timeOrigin + performance.now() - Date.now()) < 1000];
ua.getHighEntropyValues(['uaFullVersion']).then(v => { out.highEntropy = JSON.stringify(v); });
const p = document.getElementById('a');
const li = document.createElement('li');
li.textContent = 'x';
p.append(li, 'y');
p.classList.add('z');
let fired = 0;
p.addEventListener('ping', e => { fired += e.bubbles ? 2 : 1; });
p.dispatchEvent(new Event('ping'));
out.dom = [p.className, document.querySelectorAll('p, li').length, document.body.innerHTML, fired,
  typeof setTimeout(() => {}, 5)];
const caught = (f) => { try { f(); return 'no error'; } catch (e) { return e.name + ': ' + e.message + ' @ ' + String(e.stack).split('\n')[1]; } };
out.errors = [caught(() => document.createElement('1')), caught(() => document.body.appendChild(null))];
out.codecs = [new TextDecoder().decode(new TextEncoder().encode('h\u00e9llo')), typeof new ReadableStream().getReader];
"#;

/// Run [`PROBE`] on a new page at `url` with `viewport`, in an engine that
/// starts from `snapshot` (`None`: from source). Returns the probe's JSON
/// and `Math.random()`.
fn probe(snapshot: Option<&'static [u8]>, url: &str, viewport: (f32, f32)) -> (String, f64) {
    let mut doc = huginn_dom::parse_html("<title></title><p id=a class=x>hi</p>");
    let loader = Arc::new(Loader::new());
    let url = Url::parse(url).unwrap();
    let mut engine = V8Engine::with_snapshot(EngineOptions::default(), snapshot);
    let mut cx = HostContext {
        document: &mut doc,
        loader: &loader,
        document_url: &url,
        referrer: "",
        base_url: &url,
        viewport,
        now_ms: 0.0,
        layout: None,
        current_script: None,
    };
    engine
        .execute(&mut cx, PROBE, "probe.js", ScriptKind::Classic)
        .unwrap();
    engine.run_microtasks(&mut cx).unwrap();
    let report =
        "document.title = JSON.stringify(out); document.documentElement.dataset.r = Math.random();";
    engine
        .execute(&mut cx, report, "report.js", ScriptKind::Classic)
        .unwrap();
    let title = doc
        .elements_by_local_name(&local_name!("title"))
        .next()
        .unwrap();
    let html = doc.document_element().unwrap();
    let random = doc
        .attribute(html, &"data-r".into())
        .unwrap()
        .parse()
        .unwrap();
    (doc.text_content(title), random)
}

/// A page whose isolate starts from the snapshot sees exactly what it sees
/// when the prelude runs in its own isolate, with its own URL, viewport,
/// time origin and random numbers.
#[cfg(huginn_snapshot)]
#[test]
fn snapshot_starts_like_the_prelude_from_source() {
    let snapshot = Some(&super::SNAPSHOT.0[..]);
    for (url, viewport) in [
        ("https://a.example/dir/page.html?q#h", (1280.0, 800.0)),
        ("http://127.0.0.1:9/other", (800.0, 600.0)),
    ] {
        let (from_snapshot, random) = probe(snapshot, url, viewport);
        let (from_source, _) = probe(None, url, viewport);
        assert_eq!(from_snapshot, from_source);
        assert!(from_snapshot.contains(&format!(r#""page":["{url}""#)));
        assert!(
            from_snapshot.contains(&format!(r#"{},{},{}"#, viewport.0, viewport.1, viewport.0))
        );
        assert!(from_snapshot.contains(huginn_net::ua::USER_AGENT));
        assert!(from_snapshot.contains(r#""highEntropy""#));
        let (_, other) = probe(snapshot, url, viewport);
        assert_ne!(random, other, "Math.random repeats across isolates");
    }
}

/// The snapshot and `PRELUDE` come from one build.rs run. This checks they
/// are the current `src/js`, so an edited prelude never runs from an old
/// snapshot.
#[test]
fn prelude_is_the_current_source() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/js");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "js"))
        .collect();
    files.sort();
    let source: String = files
        .iter()
        .map(|f| std::fs::read_to_string(f).unwrap())
        .collect();
    assert!(PRELUDE == source, "the built prelude is not src/js");
}
