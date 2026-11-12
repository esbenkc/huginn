//! ES modules: `<script type="module">` with static imports, and `import()`.
//! The import graph is fetched up front through the shell's loader (same
//! origin only), then instantiated and evaluated. Bare specifiers and import
//! maps are not supported: such imports fail like a missing file.

use std::collections::HashMap;

use huginn_net::{ResourceKind, Url};

use super::{SharedState, exception_message, ops};
use crate::ScriptError;
use crate::dom_ops;

#[derive(Default)]
pub(crate) struct ModuleMap {
    /// Fetched modules by URL.
    by_url: HashMap<String, v8::Global<v8::Module>>,
    /// Module identity hash -> URL that relative imports resolve against.
    bases: HashMap<i32, String>,
}

impl ModuleMap {
    pub(crate) fn clear(&mut self) {
        self.by_url.clear();
        self.bases.clear();
    }
}

/// Relative and absolute specifiers only (`./a.js`, `/a.js`, `https://...`).
fn resolve_specifier(base: &str, spec: &str) -> Option<Url> {
    if let Ok(u) = Url::parse(spec) {
        return Some(u);
    }
    if !(spec.starts_with('/') || spec.starts_with("./") || spec.starts_with("../")) {
        return None;
    }
    Url::parse(base).ok()?.join(spec).ok()
}

fn compile<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    source: &str,
    url: &str,
) -> Option<v8::Local<'s, v8::Module>> {
    let src = v8::String::new(scope, source)?;
    let name = v8::String::new(scope, url)?;
    let origin = v8::ScriptOrigin::new(
        scope,
        name.into(),
        0,
        0,
        false,
        0,
        None,
        false,
        false,
        true,
        None,
    );
    let mut source = v8::script_compiler::Source::new(src, Some(&origin));
    v8::script_compiler::compile_module(scope, &mut source)
}

/// Fetch a module's source through the loader, same origin only.
fn fetch_source(scope: &v8::PinScope<'_, '_>, url: &Url) -> Result<(String, String), String> {
    let cx = ops::host(scope).ok_or("no host context")?;
    if !dom_ops::fetch_allowed(cx.document_url, url) {
        return Err(format!("cross-origin module {url} not loaded"));
    }
    let r = cx
        .loader
        .fetch(url, ResourceKind::Script)
        .map_err(|e| format!("cannot load module {url}: {e}"))?;
    Ok((r.text().into_owned(), r.url.to_string()))
}

/// Compile `source` (whose imports resolve against `base`) and fetch and
/// compile everything it imports, transitively. `key` registers the root
/// by URL (None for inline modules).
fn load_graph<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: &SharedState,
    source: &str,
    base: &str,
    key: Option<&str>,
) -> Result<v8::Local<'s, v8::Module>, String> {
    let root =
        compile(scope, source, base).ok_or_else(|| format!("{base}: module failed to compile"))?;
    {
        let mut st = state.borrow_mut();
        st.modules
            .bases
            .insert(root.get_identity_hash().get(), base.to_owned());
        if let Some(k) = key {
            st.modules
                .by_url
                .insert(k.to_owned(), v8::Global::new(scope, root));
        }
    }
    let mut queue = vec![(root, base.to_owned())];
    while let Some((m, base)) = queue.pop() {
        let requests = m.get_module_requests();
        for i in 0..requests.length() {
            let Some(req) = requests
                .get(scope, i)
                .and_then(|d| v8::Local::<v8::ModuleRequest>::try_from(d).ok())
            else {
                continue;
            };
            let spec = req.get_specifier().to_rust_string_lossy(scope);
            let url = resolve_specifier(&base, &spec)
                .ok_or_else(|| format!("{base}: cannot resolve import '{spec}'"))?;
            if state.borrow().modules.by_url.contains_key(url.as_str()) {
                continue;
            }
            let (text, final_url) = fetch_source(scope, &url)?;
            let dep = compile(scope, &text, &final_url)
                .ok_or_else(|| format!("{final_url}: module failed to compile"))?;
            {
                let mut st = state.borrow_mut();
                st.modules
                    .bases
                    .insert(dep.get_identity_hash().get(), final_url.clone());
                st.modules
                    .by_url
                    .insert(url.to_string(), v8::Global::new(scope, dep));
                if final_url != url.as_str() {
                    st.modules
                        .by_url
                        .insert(final_url.clone(), v8::Global::new(scope, dep));
                }
            }
            queue.push((dep, final_url));
        }
    }
    Ok(root)
}

fn resolve_callback<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    _attributes: v8::Local<'s, v8::FixedArray>,
    referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    v8::callback_scope!(unsafe scope, context);
    let spec = specifier.to_rust_string_lossy(scope);
    let found = {
        let st = ops::state(scope);
        let st = st.borrow();
        st.modules
            .bases
            .get(&referrer.get_identity_hash().get())
            .and_then(|base| resolve_specifier(base, &spec))
            .and_then(|u| {
                st.modules
                    .by_url
                    .get(u.as_str())
                    .map(|g| v8::Local::new(scope, g))
            })
    };
    if found.is_none() {
        // V8 requires an exception when resolution fails.
        let msg = v8::String::new(scope, &format!("cannot resolve module '{spec}'"))?;
        let e = v8::Exception::type_error(scope, msg);
        scope.throw_exception(e);
    }
    found
}

/// Instantiate and evaluate; `Err` carries the message.
fn link_and_run(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<v8::HandleScope>>,
    module: v8::Local<'_, v8::Module>,
) -> Result<(), String> {
    if module.get_status() == v8::ModuleStatus::Uninstantiated
        && module.instantiate_module(tc, resolve_callback).is_none()
    {
        return Err(exception_message(tc));
    }
    if matches!(module.get_status(), v8::ModuleStatus::Instantiated)
        && module.evaluate(tc).is_none()
    {
        return Err(exception_message(tc));
    }
    if module.get_status() == v8::ModuleStatus::Errored {
        let e = module.get_exception();
        return Err(e.to_rust_string_lossy(tc));
    }
    Ok(())
}

/// Run a module script. `url` is its URL (the document URL for inline ones).
pub(super) fn run_module(
    scope: &mut v8::PinScope<'_, '_>,
    state: &SharedState,
    source: &str,
    url: &str,
) -> Result<(), ScriptError> {
    v8::tc_scope!(let tc, scope);
    let module = match load_graph(tc, state, source, url, None) {
        Ok(m) => m,
        Err(message) => {
            let message = if tc.has_caught() {
                exception_message(tc)
            } else {
                message
            };
            return Err(ScriptError::Compile {
                url: url.into(),
                message,
            });
        }
    };
    link_and_run(tc, module).map_err(|message| ScriptError::Runtime {
        url: url.into(),
        message,
    })
}

/// `import()`: queue the load; the promise settles in the next timer round.
pub(super) fn dynamic_import<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    _options: v8::Local<'s, v8::Data>,
    resource: v8::Local<'s, v8::Value>,
    specifier: v8::Local<'s, v8::String>,
    _attributes: v8::Local<'s, v8::FixedArray>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let resolver = v8::PromiseResolver::new(scope)?;
    let promise = resolver.get_promise(scope);
    let spec = specifier.to_rust_string_lossy(scope);
    let mut base = resource.to_rust_string_lossy(scope);
    if Url::parse(&base).is_err()
        && let Some(cx) = ops::host(scope)
    {
        base = cx.base_url.to_string();
    }
    match resolve_specifier(&base, &spec) {
        Some(u) => {
            let st = ops::state(scope);
            st.borrow_mut()
                .pending_imports
                .push_back((v8::Global::new(scope, resolver), u.to_string()));
        }
        None => {
            let msg = v8::String::new(scope, &format!("cannot resolve module '{spec}'"))?;
            let e = v8::Exception::type_error(scope, msg);
            resolver.reject(scope, e);
        }
    }
    Some(promise)
}

/// Load, run and settle queued `import()` calls. Returns how many settled.
pub(super) fn run_pending_imports(scope: &mut v8::PinScope<'_, '_>, state: &SharedState) -> usize {
    let mut n = 0;
    loop {
        let next = state.borrow_mut().pending_imports.pop_front();
        let Some((resolver, url)) = next else { break };
        n += 1;
        v8::tc_scope!(let tc, scope);
        let resolver = v8::Local::new(tc, &resolver);
        let existing = state
            .borrow()
            .modules
            .by_url
            .get(&url)
            .map(|g| v8::Local::new(tc, g));
        let module = match existing {
            Some(m) => Ok(m),
            None => match Url::parse(&url)
                .map_err(|e| e.to_string())
                .and_then(|u| fetch_source(tc, &u))
            {
                Ok((text, final_url)) => load_graph(tc, state, &text, &final_url, Some(&url)),
                Err(e) => Err(e),
            },
        };
        let result = module.and_then(|m| link_and_run(tc, m).map(|_| m));
        match result {
            Ok(m) => {
                let ns = m.get_module_namespace();
                resolver.resolve(tc, ns);
            }
            Err(msg) => {
                state.borrow_mut().log_error(&msg);
                tc.reset();
                if let Some(s) = v8::String::new(tc, &msg) {
                    let e = v8::Exception::type_error(tc, s);
                    resolver.reject(tc, e);
                }
            }
        }
        tc.perform_microtask_checkpoint();
    }
    n
}
