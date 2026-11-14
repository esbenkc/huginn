//! The V8 engine: one isolate and one context per page, created on the first
//! `execute`. The web platform objects are JavaScript (`src/js/*.js`, the
//! prelude) over native operations on the document (`ops.rs`). The build
//! runs the prelude into a V8 startup snapshot (build.rs, `startup.rs`), and
//! each isolate starts from it.
//!
//! Every engine call installs the shell's [`HostContext`] in the isolate's
//! [`State`] for the duration of the call, so native callbacks can reach the
//! document. A watchdog thread terminates calls that run past the budget.

mod codec;
mod fetches;
mod modules;
mod ops;
mod startup;
#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use huginn_dom::{NodeId, local_name};

use crate::{
    EngineOptions, Event, EventTarget, EventValue, HostContext, HostRequest, ScriptEngine,
    ScriptError, ScriptKind, Waker,
};

use startup::exception_message;

/// The JavaScript DOM layer, one function scope across the files (joined by
/// build.rs).
const PRELUDE: &str = include_str!(concat!(env!("OUT_DIR"), "/prelude.js"));

/// An isolate with the prelude already run: the V8 startup snapshot made by
/// build.rs (not in cross builds).
#[cfg(huginn_snapshot)]
static SNAPSHOT: &startup::Aligned<[u8]> =
    &startup::Aligned(*include_bytes!(concat!(env!("OUT_DIR"), "/prelude.snap")));

/// V8 heap limit. Pages that need more are stopped, not crashed.
const HEAP_LIMIT: usize = 512 << 20;
/// Timers run per `run_timers` call at most, so a self-rescheduling
/// zero-delay timer cannot spin forever inside one call.
const MAX_TIMERS_PER_ROUND: usize = 2000;

/// `HUGINN_V8_FLAGS` replaces V8's flags ([`startup::V8_FLAGS`]), to measure
/// other settings (for example `--single-threaded` alone, or with
/// `--jitless`).
fn init_v8() {
    let flags = std::env::var("HUGINN_V8_FLAGS");
    startup::init_v8(flags.as_deref().unwrap_or(startup::V8_FLAGS));
}

/// The startup snapshot for new isolates. None without one, or with
/// `HUGINN_V8_FLAGS`: a snapshot works only with the flags it was made with.
/// Then each isolate runs the prelude itself.
fn startup_snapshot() -> Option<&'static [u8]> {
    #[cfg(huginn_snapshot)]
    if std::env::var_os("HUGINN_V8_FLAGS").is_none() {
        return Some(&SNAPSHOT.0);
    }
    None
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
    /// Compression streams in progress, by the id the prelude holds.
    codecs: HashMap<u32, codec::Codec>,
    next_codec: u32,
    /// The watchdog's clock; native operations stop it while they wait for
    /// the network.
    clock: WatchClock,
    /// Requests of `fetch()` and async XHR on other threads.
    fetches: fetches::Fetches,
}

struct Hooks {
    object: v8::Global<v8::Object>,
    dispatch: v8::Global<v8::Function>,
    run_one: v8::Global<v8::Function>,
    next_due: v8::Global<v8::Function>,
    net_done: v8::Global<v8::Function>,
}

impl State {
    fn new(options: EngineOptions, clock: WatchClock) -> State {
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
            codecs: HashMap::new(),
            next_codec: 0,
            clock,
            fetches: fetches::Fetches::new(),
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

/// Run `wait`, a blocking wait for the network, with the watchdog stopped
/// (see [`WatchClock::paused`]).
fn network_wait<R>(state: &SharedState, wait: impl FnOnce() -> R) -> R {
    let clock = state.borrow().clock.clone();
    clock.paused(wait)
}

/// Terminates the running script when an engine call passes its budget.
struct Watchdog {
    clock: WatchClock,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct WatchState {
    deadline: Option<Instant>,
    quit: bool,
}

/// The watchdog's deadline, shared by the engine thread and the watchdog
/// thread.
#[derive(Clone)]
struct WatchClock(Arc<(Mutex<WatchState>, Condvar)>);

impl WatchClock {
    fn set(&self, deadline: Option<Instant>) {
        let (lock, cv) = &*self.0;
        if let Ok(mut st) = lock.lock() {
            st.deadline = deadline;
            cv.notify_one();
        }
    }

    /// Run `wait` with the clock stopped. Sync XHR, inserted scripts and
    /// modules wait for the network inside a call, and that time is not
    /// script time: a task that makes several requests must not be
    /// terminated in the middle of its script.
    fn paused<R>(&self, wait: impl FnOnce() -> R) -> R {
        let left = {
            let (lock, cv) = &*self.0;
            let Ok(mut st) = lock.lock() else {
                return wait();
            };
            let left = st
                .deadline
                .take()
                .map(|d| d.saturating_duration_since(Instant::now()));
            cv.notify_one();
            left
        };
        let r = wait();
        if let Some(left) = left {
            self.set(Some(Instant::now() + left));
        }
        r
    }
}

impl Watchdog {
    fn new(handle: v8::IsolateHandle) -> Watchdog {
        let clock = WatchClock(Arc::new((
            Mutex::new(WatchState {
                deadline: None,
                quit: false,
            }),
            Condvar::new(),
        )));
        let s = clock.0.clone();
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
        Watchdog { clock, thread }
    }

    fn set(&self, deadline: Option<Instant>) {
        self.clock.set(deadline);
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        {
            let (lock, cv) = &*self.clock.0;
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
    /// The isolate's startup snapshot; `None` runs the prelude in the new
    /// isolate instead.
    snapshot: Option<&'static [u8]>,
    rt: Option<Runtime>,
    /// See [`ScriptEngine::set_waker`]; kept for an isolate made later.
    waker: Option<Waker>,
}

impl V8Engine {
    pub fn new(options: EngineOptions) -> V8Engine {
        V8Engine::with_snapshot(options, startup_snapshot())
    }

    fn with_snapshot(options: EngineOptions, snapshot: Option<&'static [u8]>) -> V8Engine {
        V8Engine {
            options,
            snapshot,
            rt: None,
            waker: None,
        }
    }

    fn runtime(&mut self, cx: &mut HostContext<'_>) -> Result<&mut Runtime, ScriptError> {
        if self.rt.is_none() {
            let rt = Runtime::new(self.options.clone(), self.snapshot, cx)?;
            rt.state.borrow().fetches.set_waker(self.waker.clone());
            self.rt = Some(rt);
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
    fn new(
        options: EngineOptions,
        snapshot: Option<&'static [u8]>,
        cx: &mut HostContext<'_>,
    ) -> Result<Runtime, ScriptError> {
        init_v8();
        let mut params = v8::CreateParams::default().heap_limits(0, HEAP_LIMIT);
        if let Some(blob) = snapshot {
            params = params.snapshot_blob(v8::StartupData::from(blob));
        }
        let mut isolate = v8::Isolate::new(params);
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        isolate.set_host_import_module_dynamically_callback(modules::dynamic_import);
        isolate.set_host_initialize_import_meta_object_callback(modules::import_meta);
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
        let watchdog = Watchdog::new(handle.clone());
        let mut state = State::new(options, watchdog.clock.clone());
        for n in cx.document.elements_by_local_name(&local_name!("script")) {
            state.started.insert(n);
        }
        let state: SharedState = Rc::new(RefCell::new(state));
        isolate.set_slot(state.clone());

        // From a snapshot, this is the context the prelude ran in.
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
        rt.start_prelude(cx, snapshot.is_some())?;
        Ok(rt)
    }

    /// Get the prelude's `N` object and hooks from the snapshot, or run the
    /// prelude. Then add the native operations to `N`, call the `start`
    /// hook for this page and keep the other hooks.
    fn start_prelude(
        &mut self,
        cx: &mut HostContext<'_>,
        from_snapshot: bool,
    ) -> Result<(), ScriptError> {
        let _call = Call::enter(&self.state, &self.watchdog, cx);
        let state = self.state.clone();
        v8::scope!(let scope, &mut self.isolate);
        let context = v8::Local::new(scope, &self.context);
        let scope = &mut v8::ContextScope::new(scope, context);
        v8::tc_scope!(let tc, scope);
        let error = |message: String| ScriptError::Runtime {
            url: startup::PRELUDE_URL.into(),
            message,
        };
        let (native, hooks) = if from_snapshot {
            let native = tc.get_context_data_from_snapshot_once::<v8::Object>(startup::N_INDEX);
            let hooks = tc.get_context_data_from_snapshot_once::<v8::Object>(startup::HOOKS_INDEX);
            let (Ok(native), Ok(hooks)) = (native, hooks) else {
                return Err(error("the startup snapshot has no prelude".into()));
            };
            (native, hooks)
        } else {
            let Some(prelude) = startup::run_prelude(tc, PRELUDE) else {
                return Err(error(exception_message(tc)));
            };
            prelude
        };
        ops::install_ops(tc, native);
        let get = |tc: &mut v8::PinnedRef<'_, v8::TryCatch<v8::HandleScope>>, name: &str| {
            let key = v8::String::new(tc, name)?;
            let f = hooks.get(tc, key.into())?;
            let f = v8::Local::<v8::Function>::try_from(f).ok()?;
            Some(v8::Global::new(tc, f))
        };
        let (Some(start), Some(dispatch), Some(run_one), Some(next_due), Some(net_done)) = (
            get(tc, "start"),
            get(tc, "dispatch"),
            get(tc, "runOne"),
            get(tc, "nextDue"),
            get(tc, "netDone"),
        ) else {
            return Err(error("prelude hooks missing".into()));
        };
        let start = v8::Local::new(tc, start);
        if start.call(tc, hooks.into(), &[]).is_none() {
            return Err(error(exception_message(tc)));
        }
        let object = v8::Global::new(tc, hooks);
        state.borrow_mut().hooks = Some(Hooks {
            object,
            dispatch,
            run_one,
            next_due,
            net_done,
        });
        Ok(())
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
            ScriptKind::ImportMap => modules::register_import_map(scope, state, source, url),
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
        with_context(rt, cx, |scope, state| {
            let mut ran = run_pending(scope, state);
            deliver_responses(scope, state);
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
            // The call's task time (`cx.now_ms`, or later if a script
            // already saw a later `performance.now()`): tasks queued for
            // "now", like the responses above, run in this round.
            let at = state.borrow().call_base;
            let at = v8::Number::new(scope, at).into();
            // Each task gets the whole budget, so a task is not terminated
            // because earlier ones in the round were slow. A round starts no
            // new task once the budget is spent (it runs at least one); the
            // rest stay due for the next call, and the shell can lay out in
            // between.
            let (budget, round_end) = {
                let st = state.borrow();
                let budget = Duration::from_millis(st.options.call_budget_ms);
                (budget, st.call_start + budget)
            };
            let mut timers = 0;
            while ran < MAX_TIMERS_PER_ROUND && (timers == 0 || Instant::now() < round_end) {
                state.borrow().clock.set(Some(Instant::now() + budget));
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
                timers += 1;
                tc.perform_microtask_checkpoint();
            }
            Ok(ran)
        })
    }

    fn next_timer_due(&self) -> Option<f64> {
        let rt = self.rt.as_ref()?;
        let st = rt.state.borrow();
        if !st.pending_scripts.is_empty()
            || !st.pending_imports.is_empty()
            || st.fetches.has_arrived()
        {
            // Due now: the shell runs them in the next `run_timers`.
            return Some(st.last_now);
        }
        st.next_due
    }

    fn requests_in_flight(&self) -> usize {
        self.rt
            .as_ref()
            .map_or(0, |rt| rt.state.borrow().fetches.in_flight())
    }

    fn wait_for_response(&self, timeout: Duration) -> bool {
        let Some(rt) = self.rt.as_ref() else {
            return false;
        };
        let st = rt.state.borrow();
        st.fetches.in_flight() > 0 && st.fetches.wait(timeout)
    }

    fn set_waker(&mut self, wake: Waker) {
        if let Some(rt) = &self.rt {
            rt.state.borrow().fetches.set_waker(Some(wake.clone()));
        }
        self.waker = Some(wake);
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
///
/// Fetches are synchronous and the watchdog cannot stop them, so this stops
/// starting new loads once the call's budget is spent; the rest stay queued
/// for the next `run_timers` (`next_timer_due` reports them due now).
fn run_pending(scope: &mut v8::PinScope<'_, '_>, state: &SharedState) -> usize {
    let deadline = {
        let st = state.borrow();
        st.call_start + Duration::from_millis(st.options.call_budget_ms)
    };
    ops::prefetch_pending_scripts(scope, state);
    let mut ran = 0;
    while Instant::now() < deadline {
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
    ran + modules::run_pending_imports(scope, state, deadline)
}

/// Hand the responses that arrived to the prelude (`netDone`), which queues
/// one task per response for now, in arrival order: after the tasks that
/// are already due, before later ones.
fn deliver_responses(scope: &mut v8::PinScope<'_, '_>, state: &SharedState) {
    let arrived = state.borrow_mut().fetches.take_arrived();
    if arrived.is_empty() {
        return;
    }
    let (f, recv) = {
        let st = state.borrow();
        let Some(hooks) = st.hooks.as_ref() else {
            return;
        };
        (
            v8::Local::new(scope, &hooks.net_done),
            v8::Local::new(scope, &hooks.object),
        )
    };
    for (id, response) in arrived {
        let id = v8::Integer::new_from_unsigned(scope, id).into();
        let response = ops::response_value(scope, response.as_ref());
        v8::tc_scope!(let tc, scope);
        if f.call(tc, recv.into(), &[id, response]).is_none() {
            let msg = exception_message(tc);
            state.borrow_mut().log_error(&msg);
        }
    }
}
