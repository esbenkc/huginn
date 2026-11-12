//! The V8 engine: one isolate and one context per page, created on the first
//! `execute`. The web platform objects are JavaScript (`src/js/*.js`, the
//! prelude) over native operations on the document (`ops.rs`).
//!
//! Every engine call installs the shell's [`HostContext`] in the isolate's
//! [`State`] for the duration of the call, so native callbacks can reach the
//! document. A watchdog thread terminates calls that run past the budget.

mod modules;
mod ops;

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, Once};
use std::time::{Duration, Instant};

use huginn_dom::{NodeId, local_name};

use crate::{
    EngineOptions, Event, EventTarget, EventValue, HostContext, HostRequest, ScriptEngine,
    ScriptError, ScriptKind,
};

/// The JavaScript DOM layer, one function scope across the files.
const PRELUDE: &str = concat!(
    include_str!("../js/00-base.js"),
    include_str!("../js/10-tasks.js"),
    include_str!("../js/20-node.js"),
    include_str!("../js/30-element.js"),
    include_str!("../js/40-html.js"),
    include_str!("../js/50-document.js"),
    include_str!("../js/60-net.js"),
    include_str!("../js/70-window.js"),
    include_str!("../js/90-hooks.js"),
);

/// ICU data for `Intl`, `toLocaleString` and `localeCompare`. This is the
/// small (1.8 MB) Flutter desktop subset shipped with the `v8` crate; its
/// pages are only touched when a script uses them.
#[repr(C, align(16))]
struct Aligned<T: ?Sized>(T);
static ICU_DATA: &Aligned<[u8]> = &Aligned(*include_bytes!("../../data/icudtl.dat"));

/// V8 heap limit. Pages that need more are stopped, not crashed.
const HEAP_LIMIT: usize = 512 << 20;
/// Timers run per `run_timers` call at most, so a self-rescheduling
/// zero-delay timer cannot spin forever inside one call.
const MAX_TIMERS_PER_ROUND: usize = 2000;

fn init_v8() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = v8::icu::set_common_data_78(&ICU_DATA.0);
        // One thread: no background compiler or GC threads (less memory, and
        // a page load is short). Lite mode: interpreter only, no JIT code and
        // lazy feedback vectors. On the corpus it cuts the V8 heap by up to
        // half (bbc 58 -> 28 MB) at the same wall time, because page scripts
        // mostly run once.
        v8::V8::set_flags_from_string("--single-threaded --lite-mode");
        v8::V8::initialize_platform(v8::new_single_threaded_default_platform(false).make_shared());
        v8::V8::initialize();
    });
}

/// Per-isolate state, shared by the engine and the native callbacks (it lives
/// in an isolate slot).
pub(crate) struct State {
    /// The shell's context during an engine call, null otherwise.
    cx: *mut HostContext<'static>,
    options: EngineOptions,
    /// Script elements that already ran, or must never run (parser-inserted
    /// ones, which the shell runs, and ones created by `innerHTML`).
    started: HashSet<NodeId>,
    /// External scripts inserted by script, waiting to be fetched and run.
    pending_scripts: VecDeque<NodeId>,
    /// `import()` calls waiting to be loaded: (promise resolver, URL).
    pending_imports: VecDeque<(v8::Global<v8::PromiseResolver>, String)>,
    current_script: Option<NodeId>,
    /// Set once a custom element is defined: insertions then report custom
    /// element candidates back to the prelude.
    custom_elements: bool,
    call_base: f64,
    call_start: Instant,
    last_now: f64,
    /// Earliest timer, refreshed at the end of every engine call.
    next_due: Option<f64>,
    hooks: Option<Hooks>,
    modules: modules::ModuleMap,
    /// Uncaught exceptions so far.
    errors: usize,
    /// Requests for the browser (navigations), drained by `take_requests`.
    requests: Vec<HostRequest>,
    /// Elements whose `click()` is running (HTML's click in progress flag).
    clicking: Vec<NodeId>,
}

struct Hooks {
    object: v8::Global<v8::Object>,
    dispatch: v8::Global<v8::Function>,
    run_one: v8::Global<v8::Function>,
    next_due: v8::Global<v8::Function>,
}

impl State {
    fn new(options: EngineOptions) -> State {
        State {
            cx: std::ptr::null_mut(),
            options,
            started: HashSet::new(),
            pending_scripts: VecDeque::new(),
            pending_imports: VecDeque::new(),
            current_script: None,
            custom_elements: false,
            call_base: 0.0,
            call_start: Instant::now(),
            last_now: 0.0,
            next_due: None,
            hooks: None,
            modules: modules::ModuleMap::default(),
            errors: 0,
            requests: Vec::new(),
            clicking: Vec::new(),
        }
    }

    /// `performance.now()`: the shell's virtual time plus real time spent in
    /// this call, never going backwards.
    fn now(&mut self) -> f64 {
        let t = self.call_base + self.call_start.elapsed().as_secs_f64() * 1000.0;
        if t > self.last_now {
            self.last_now = t;
        }
        self.last_now
    }

    fn log_error(&mut self, msg: &str) {
        self.errors += 1;
        let short: String = msg.chars().take(400).collect();
        eprintln!("huginn: js: {short}");
    }
}

pub(crate) type SharedState = Rc<RefCell<State>>;

/// Terminates the running script when an engine call passes its budget.
struct Watchdog {
    shared: Arc<(Mutex<WatchState>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct WatchState {
    deadline: Option<Instant>,
    quit: bool,
}

impl Watchdog {
    fn new(handle: v8::IsolateHandle) -> Watchdog {
        let shared = Arc::new((
            Mutex::new(WatchState {
                deadline: None,
                quit: false,
            }),
            Condvar::new(),
        ));
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("huginn-js-watchdog".into())
            .stack_size(64 * 1024)
            .spawn(move || {
                let (lock, cv) = &*s;
                let Ok(mut st) = lock.lock() else { return };
                loop {
                    if st.quit {
                        return;
                    }
                    st = match st.deadline {
                        None => match cv.wait(st) {
                            Ok(g) => g,
                            Err(_) => return,
                        },
                        Some(d) => {
                            let now = Instant::now();
                            if now >= d {
                                handle.terminate_execution();
                                st.deadline = None;
                                st
                            } else {
                                match cv.wait_timeout(st, d - now) {
                                    Ok((g, _)) => g,
                                    Err(_) => return,
                                }
                            }
                        }
                    };
                }
            })
            .ok();
        Watchdog { shared, thread }
    }

    fn set(&self, deadline: Option<Instant>) {
        let (lock, cv) = &*self.shared;
        if let Ok(mut st) = lock.lock() {
            st.deadline = deadline;
            cv.notify_one();
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        {
            let (lock, cv) = &*self.shared;
            if let Ok(mut st) = lock.lock() {
                st.quit = true;
                cv.notify_one();
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Runtime {
    // Field order is drop order: globals before the isolate.
    context: v8::Global<v8::Context>,
    state: SharedState,
    watchdog: Watchdog,
    handle: v8::IsolateHandle,
    /// Heap-limit callback data; must outlive the isolate's use of it.
    _near_limit: Box<v8::IsolateHandle>,
    isolate: v8::OwnedIsolate,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Release every handle while the isolate still exists.
        let mut st = self.state.borrow_mut();
        st.hooks = None;
        st.pending_imports.clear();
        st.modules.clear();
    }
}

/// The V8 [`ScriptEngine`]. Cheap to create: the isolate is made on the
/// first `execute`, so pages without scripts never pay for it.
pub struct V8Engine {
    options: EngineOptions,
    rt: Option<Runtime>,
}

impl V8Engine {
    pub fn new(options: EngineOptions) -> V8Engine {
        V8Engine { options, rt: None }
    }

    fn runtime(&mut self, cx: &mut HostContext<'_>) -> Result<&mut Runtime, ScriptError> {
        if self.rt.is_none() {
            self.rt = Some(Runtime::new(self.options.clone(), cx)?);
        }
        Ok(self.rt.as_mut().expect("runtime was just created"))
    }
}

/// Near the heap limit: stop the script and grant a little more room so V8
/// does not abort the process while the termination unwinds.
unsafe extern "C" fn near_heap_limit(
    data: *mut std::ffi::c_void,
    current: usize,
    _initial: usize,
) -> usize {
    // SAFETY: `data` is the boxed handle owned by the Runtime, which outlives the isolate.
    let handle = unsafe { &*(data as *const v8::IsolateHandle) };
    handle.terminate_execution();
    current + (64 << 20)
}

/// Installs the host context for one engine call and arms the watchdog.
struct Call<'r> {
    state: &'r SharedState,
    watchdog: &'r Watchdog,
}

impl<'r> Call<'r> {
    fn enter(state: &'r SharedState, watchdog: &'r Watchdog, cx: &mut HostContext<'_>) -> Call<'r> {
        {
            let mut st = state.borrow_mut();
            st.cx = cx as *mut HostContext<'_> as *mut HostContext<'static>;
            st.call_base = cx.now_ms.max(st.last_now);
            st.call_start = Instant::now();
            st.current_script = cx.current_script;
            let budget = Duration::from_millis(st.options.call_budget_ms);
            watchdog.set(Some(Instant::now() + budget));
        }
        Call { state, watchdog }
    }
}

impl Drop for Call<'_> {
    fn drop(&mut self) {
        self.watchdog.set(None);
        let mut st = self.state.borrow_mut();
        st.cx = std::ptr::null_mut();
        st.current_script = None;
    }
}

impl Runtime {
    fn new(options: EngineOptions, cx: &mut HostContext<'_>) -> Result<Runtime, ScriptError> {
        init_v8();
        let params = v8::CreateParams::default().heap_limits(0, HEAP_LIMIT);
        let mut isolate = v8::Isolate::new(params);
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        isolate.set_host_import_module_dynamically_callback(modules::dynamic_import);
        isolate.set_promise_reject_callback(promise_reject);
        let handle = isolate.thread_safe_handle();
        let near_limit = Box::new(isolate.thread_safe_handle());
        isolate.add_near_heap_limit_callback(
            near_heap_limit,
            &*near_limit as *const v8::IsolateHandle as *mut _,
        );

        // Scripts look elements up by id often; walking the tree each time
        // is quadratic on script-heavy pages.
        cx.document.index_ids();
        let mut state = State::new(options);
        for n in cx.document.elements_by_local_name(&local_name!("script")) {
            state.started.insert(n);
        }
        let state: SharedState = Rc::new(RefCell::new(state));
        isolate.set_slot(state.clone());
        let watchdog = Watchdog::new(handle.clone());

        let context = {
            v8::scope!(let scope, &mut isolate);
            let context = v8::Context::new(scope, Default::default());
            v8::Global::new(scope, context)
        };
        let mut rt = Runtime {
            context,
            state,
            watchdog,
            handle,
            _near_limit: near_limit,
            isolate,
        };
        rt.install_prelude(cx)?;
        Ok(rt)
    }

    /// Run the prelude and keep its hooks.
    fn install_prelude(&mut self, cx: &mut HostContext<'_>) -> Result<(), ScriptError> {
        let _call = Call::enter(&self.state, &self.watchdog, cx);
        let state = self.state.clone();
        v8::scope!(let scope, &mut self.isolate);
        let context = v8::Local::new(scope, &self.context);
        let scope = &mut v8::ContextScope::new(scope, context);
        v8::tc_scope!(let tc, scope);
        let fail =
            |tc: &mut v8::PinnedRef<'_, v8::TryCatch<v8::HandleScope>>| ScriptError::Runtime {
                url: "huginn:prelude".into(),
                message: exception_message(tc),
            };
        let native = ops::install_ops(tc);
        let Some(func) = compile_and_run(tc, PRELUDE, "huginn:prelude") else {
            return Err(fail(tc));
        };
        let Ok(func) = v8::Local::<v8::Function>::try_from(func) else {
            return Err(ScriptError::Runtime {
                url: "huginn:prelude".into(),
                message: "prelude is not a function".into(),
            });
        };
        let global = context.global(tc);
        let undefined = v8::undefined(tc).into();
        let Some(hooks) = func.call(tc, undefined, &[native.into(), global.into()]) else {
            return Err(fail(tc));
        };
        let Ok(hooks) = v8::Local::<v8::Object>::try_from(hooks) else {
            return Err(ScriptError::Runtime {
                url: "huginn:prelude".into(),
                message: "prelude returned no hooks".into(),
            });
        };
        let get = |tc: &mut v8::PinnedRef<'_, v8::TryCatch<v8::HandleScope>>, name: &str| {
            let key = v8::String::new(tc, name)?;
            let f = hooks.get(tc, key.into())?;
            let f = v8::Local::<v8::Function>::try_from(f).ok()?;
            Some(v8::Global::new(tc, f))
        };
        let (Some(dispatch), Some(run_one), Some(next_due)) =
            (get(tc, "dispatch"), get(tc, "runOne"), get(tc, "nextDue"))
        else {
            return Err(ScriptError::Runtime {
                url: "huginn:prelude".into(),
                message: "prelude hooks missing".into(),
            });
        };
        let object = v8::Global::new(tc, hooks);
        state.borrow_mut().hooks = Some(Hooks {
            object,
            dispatch,
            run_one,
            next_due,
        });
        Ok(())
    }
}

/// Compile and run a classic script. `None` means an exception is pending in
/// the try-catch scope.
fn compile_and_run<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    source: &str,
    url: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let source = v8::String::new(scope, source)?;
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
        false,
        None,
    );
    let script = v8::Script::compile(scope, source, Some(&origin))?;
    script.run(scope)
}

/// "url:line: message" for the pending exception.
pub(crate) fn exception_message(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<v8::HandleScope>>,
) -> String {
    if tc.has_terminated() {
        return "script terminated (time or memory budget)".into();
    }
    let Some(exception) = tc.exception() else {
        return "unknown error".into();
    };
    let text = exception.to_rust_string_lossy(tc);
    match tc.message() {
        Some(m) => {
            let file = m
                .get_script_resource_name(tc)
                .map(|s| s.to_rust_string_lossy(tc))
                .unwrap_or_default();
            let line = m.get_line_number(tc).unwrap_or(0);
            format!("{file}:{line}: {text}")
        }
        None => text,
    }
}

extern "C" fn promise_reject(msg: v8::PromiseRejectMessage) {
    if msg.get_event() != v8::PromiseRejectEvent::PromiseRejectWithNoHandler {
        return;
    }
    v8::callback_scope!(unsafe scope, &msg);
    let st = ops::state(scope);
    if !st.borrow().options.console {
        return;
    }
    let text = msg
        .get_value()
        .map(|v| v.to_rust_string_lossy(scope))
        .unwrap_or_default();
    eprintln!(
        "huginn: js: unhandled rejection: {}",
        text.chars().take(300).collect::<String>()
    );
}

/// Run `f` in the runtime's context with the host context installed. If a
/// script was terminated by the watchdog, the termination is cancelled
/// afterwards so later calls run normally.
fn with_context<R>(
    rt: &mut Runtime,
    cx: &mut HostContext<'_>,
    f: impl for<'s, 'i> FnOnce(&mut v8::PinScope<'s, 'i>, &SharedState) -> R,
) -> R {
    let state = rt.state.clone();
    let handle = rt.handle.clone();
    let r = {
        let _call = Call::enter(&rt.state, &rt.watchdog, cx);
        v8::scope!(let scope, &mut rt.isolate);
        let context = v8::Local::new(scope, &rt.context);
        let scope = &mut v8::ContextScope::new(scope, context);
        let r = f(scope, &state);
        // Remember the next timer for `next_timer_due` (which cannot call JS).
        let due = next_due_now(scope, &state);
        state.borrow_mut().next_due = due;
        r
    };
    handle.cancel_terminate_execution();
    r
}

fn next_due_now(scope: &mut v8::PinScope<'_, '_>, state: &SharedState) -> Option<f64> {
    let (f, recv) = {
        let st = state.borrow();
        let hooks = st.hooks.as_ref()?;
        (
            v8::Local::new(scope, &hooks.next_due),
            v8::Local::new(scope, &hooks.object),
        )
    };
    let v = f.call(scope, recv.into(), &[])?;
    let due = v.number_value(scope)?;
    (due >= 0.0).then_some(due)
}

impl ScriptEngine for V8Engine {
    fn execute(
        &mut self,
        cx: &mut HostContext<'_>,
        source: &str,
        url: &str,
        kind: ScriptKind,
    ) -> Result<(), ScriptError> {
        let rt = self.runtime(cx)?;
        let result = with_context(rt, cx, |scope, state| match kind {
            ScriptKind::Classic => run_classic(scope, source, url),
            ScriptKind::Module => modules::run_module(scope, state, source, url),
        });
        if let Err(e) = &result {
            rt.state.borrow_mut().errors += 1;
            let _ = e;
        }
        result
    }

    fn dispatch_event(
        &mut self,
        cx: &mut HostContext<'_>,
        target: EventTarget,
        event: &Event,
    ) -> Result<bool, ScriptError> {
        // No isolate means no listeners.
        let Some(rt) = self.rt.as_mut() else {
            return Ok(true);
        };
        with_context(rt, cx, |scope, state| {
            dispatch_js(scope, state, target, event)
        })
    }

    fn run_microtasks(&mut self, cx: &mut HostContext<'_>) -> Result<(), ScriptError> {
        let Some(rt) = self.rt.as_mut() else {
            return Ok(());
        };
        with_context(rt, cx, |scope, _| {
            scope.perform_microtask_checkpoint();
            Ok(())
        })
    }

    fn run_timers(&mut self, cx: &mut HostContext<'_>) -> Result<usize, ScriptError> {
        let Some(rt) = self.rt.as_mut() else {
            return Ok(0);
        };
        let at = cx.now_ms;
        with_context(rt, cx, |scope, state| {
            let mut ran = run_pending(scope, state);
            let (f, recv) = {
                let st = state.borrow();
                let Some(hooks) = st.hooks.as_ref() else {
                    return Ok(ran);
                };
                (
                    v8::Local::new(scope, &hooks.run_one),
                    v8::Local::new(scope, &hooks.object),
                )
            };
            let at = v8::Number::new(scope, at).into();
            while ran < MAX_TIMERS_PER_ROUND {
                v8::tc_scope!(let tc, scope);
                let Some(r) = f.call(tc, recv.into(), &[at]) else {
                    let msg = exception_message(tc);
                    state.borrow_mut().log_error(&msg);
                    return Err(ScriptError::Runtime {
                        url: "timer".into(),
                        message: msg,
                    });
                };
                if !r.is_true() {
                    break;
                }
                ran += 1;
                tc.perform_microtask_checkpoint();
            }
            Ok(ran)
        })
    }

    fn next_timer_due(&self) -> Option<f64> {
        let rt = self.rt.as_ref()?;
        let st = rt.state.borrow();
        if !st.pending_scripts.is_empty() || !st.pending_imports.is_empty() {
            // Due now: the shell runs them in the next `run_timers`.
            return Some(st.last_now);
        }
        st.next_due
    }

    fn take_requests(&mut self) -> Vec<HostRequest> {
        match &self.rt {
            Some(rt) => std::mem::take(&mut rt.state.borrow_mut().requests),
            None => Vec::new(),
        }
    }

    fn heap_bytes(&mut self) -> usize {
        let Some(rt) = self.rt.as_mut() else { return 0 };
        let stats = rt.isolate.get_heap_statistics();
        stats.total_heap_size() + stats.external_memory() + stats.malloced_memory()
    }

    fn trim_memory(&mut self) {
        if let Some(rt) = self.rt.as_mut() {
            rt.isolate.low_memory_notification();
        }
    }
}

impl Drop for V8Engine {
    fn drop(&mut self) {
        if let Some(rt) = &self.rt {
            let n = rt.state.borrow().errors;
            if n > 0 && rt.state.borrow().options.console {
                eprintln!("huginn: js: {n} uncaught errors");
            }
        }
    }
}

/// Run a classic script; errors are returned, not logged.
fn run_classic(
    scope: &mut v8::PinScope<'_, '_>,
    source: &str,
    url: &str,
) -> Result<(), ScriptError> {
    v8::tc_scope!(let tc, scope);
    let Some(src) = v8::String::new(tc, source) else {
        return Err(ScriptError::Compile {
            url: url.into(),
            message: "script too large".into(),
        });
    };
    let name = v8::String::new(tc, url).unwrap_or_else(|| v8::String::empty(tc));
    let origin = v8::ScriptOrigin::new(
        tc,
        name.into(),
        0,
        0,
        false,
        0,
        None,
        false,
        false,
        false,
        None,
    );
    let Some(script) = v8::Script::compile(tc, src, Some(&origin)) else {
        return Err(ScriptError::Compile {
            url: url.into(),
            message: exception_message(tc),
        });
    };
    match script.run(tc) {
        Some(_) => Ok(()),
        None => Err(ScriptError::Runtime {
            url: url.into(),
            message: exception_message(tc),
        }),
    }
}

fn dispatch_js(
    scope: &mut v8::PinScope<'_, '_>,
    state: &SharedState,
    target: EventTarget,
    event: &Event,
) -> Result<bool, ScriptError> {
    let (f, recv) = {
        let st = state.borrow();
        let Some(hooks) = st.hooks.as_ref() else {
            return Ok(true);
        };
        (
            v8::Local::new(scope, &hooks.dispatch),
            v8::Local::new(scope, &hooks.object),
        )
    };
    let (kind, id) = match target {
        EventTarget::Window => (0, 0),
        EventTarget::Document => (1, 0),
        EventTarget::Node(n) => (2, n.to_raw()),
    };
    v8::tc_scope!(let tc, scope);
    let Some(ty) = v8::String::new(tc, &event.kind) else {
        return Ok(true);
    };
    let iface = v8::String::new(tc, event.interface).unwrap_or_else(|| v8::String::empty(tc));
    let init = v8::Object::new(tc);
    for (k, v) in &event.init {
        let Some(key) = v8::String::new(tc, k) else {
            continue;
        };
        let val: v8::Local<v8::Value> = match v {
            EventValue::Bool(b) => v8::Boolean::new(tc, *b).into(),
            EventValue::Num(n) => v8::Number::new(tc, *n).into(),
            EventValue::Str(s) => {
                v8::String::new(tc, s).map_or_else(|| v8::String::empty(tc).into(), Into::into)
            }
            EventValue::Node(n) => {
                v8::Integer::new_from_unsigned(tc, n.map_or(0, NodeId::to_raw)).into()
            }
        };
        init.set(tc, key.into(), val);
    }
    let args = [
        v8::Integer::new(tc, kind).into(),
        v8::Integer::new_from_unsigned(tc, id).into(),
        ty.into(),
        v8::Boolean::new(tc, event.bubbles).into(),
        v8::Boolean::new(tc, event.cancelable).into(),
        iface.into(),
        init.into(),
        v8::Boolean::new(tc, event.trusted).into(),
    ];
    match f.call(tc, recv.into(), &args) {
        Some(v) => Ok(v.boolean_value(tc)),
        None => Err(ScriptError::Runtime {
            url: format!("event:{}", event.kind),
            message: exception_message(tc),
        }),
    }
}

/// Fetch and run external scripts inserted by script, then fire their
/// `load` (or `error`) event. Also settles queued `import()` calls.
/// Returns how many ran.
fn run_pending(scope: &mut v8::PinScope<'_, '_>, state: &SharedState) -> usize {
    let mut ran = 0;
    loop {
        let next = state.borrow_mut().pending_scripts.pop_front();
        let Some(node) = next else { break };
        let ok = ops::run_external_script(scope, state, node);
        let ev = Event::new(if ok { "load" } else { "error" }, false, false);
        if let Err(e) = dispatch_js(scope, state, EventTarget::Node(node), &ev) {
            state.borrow_mut().log_error(&e.to_string());
        }
        scope.perform_microtask_checkpoint();
        ran += 1;
    }
    ran + modules::run_pending_imports(scope, state)
}
