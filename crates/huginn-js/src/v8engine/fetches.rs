//! Requests of scripts (`fetch()`, async `XMLHttpRequest`) on background
//! threads, as in Chrome: several are in flight at once, and the engine
//! thread keeps running scripts meanwhile. Each one runs the page's own
//! [`Loader::fetch_for_script`], so cookies, CORS, referrers and the caches
//! work as on the engine thread. Before the window's first paint they wait
//! until the requests it needs are done ([`Loader::hold_script_requests`]).
//! Responses queue up in arrival order; the engine delivers them as tasks in
//! `run_timers`.

use std::collections::{HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};

use huginn_net::{Loader, Resource, ResponseType, ScriptRequest};
use parking_lot::{Condvar, Mutex, MutexGuard};

use crate::Waker;

/// Requests of one page that run at once; the rest wait in a queue. Chrome
/// has no such limit per page (only 6 connections per host on HTTP/1.1 and
/// 256 in all), but each request here holds a thread while it waits, and a
/// page that starts hundreds of requests must not make hundreds of threads.
const MAX_IN_FLIGHT: usize = 16;

/// A response as the prelude takes it. Built on the request's thread, so
/// the engine thread only copies it into JavaScript strings.
pub(super) struct Response {
    pub status: String,
    pub url: String,
    /// "name: value\r\n" per header.
    pub head: String,
    pub text: String,
    /// `Response.type`: basic, cors, opaque or opaqueredirect.
    pub kind: &'static str,
    pub redirected: bool,
}

impl Response {
    /// What a script may see of `r` (already filtered by the loader).
    pub fn new(r: &Resource) -> Response {
        let mut head = String::new();
        for (n, v) in &r.headers {
            head.push_str(n);
            head.push_str(": ");
            head.push_str(v);
            head.push_str("\r\n");
        }
        let (kind, url) = match r.response_type {
            ResponseType::Basic => ("basic", r.url.as_str()),
            ResponseType::Cors => ("cors", r.url.as_str()),
            ResponseType::Opaque => ("opaque", ""),
            ResponseType::OpaqueRedirect => ("opaqueredirect", ""),
        };
        Response {
            status: r.status.to_string(),
            url: url.to_owned(),
            head,
            text: r.text().into_owned(),
            kind,
            redirected: r.redirected,
        }
    }
}

/// Send `req` through `loader` on this thread. `None`: a network error
/// (CORS failures included).
pub(super) fn send(loader: &Loader, req: ScriptRequest, console: bool) -> Option<Response> {
    match loader.fetch_for_script(req) {
        Ok(r) => Some(Response::new(&r)),
        Err(e) => {
            if console {
                eprintln!("huginn: fetch: {e}");
            }
            None
        }
    }
}

struct Job {
    id: u32,
    loader: Arc<Loader>,
    req: ScriptRequest,
    console: bool,
}

struct Queue {
    /// Requests no thread has taken yet.
    waiting: VecDeque<Job>,
    /// Threads running; each takes requests until none wait.
    workers: usize,
    /// Responses the engine has not taken, in arrival order.
    arrived: VecDeque<(u32, Option<Response>)>,
    waker: Option<Waker>,
    /// The engine is gone: drop what arrives.
    closed: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    /// Signalled when a response arrives.
    arrival: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock()
    }
}

/// The requests of one page, owned by the engine thread.
pub(super) struct Fetches {
    shared: Arc<Shared>,
    /// Requests the page still waits for: started, and neither delivered
    /// nor aborted.
    pending: HashSet<u32>,
    last_id: u32,
}

impl Fetches {
    pub fn new() -> Fetches {
        Fetches {
            shared: Arc::new(Shared {
                queue: Mutex::new(Queue {
                    waiting: VecDeque::new(),
                    workers: 0,
                    arrived: VecDeque::new(),
                    waker: None,
                    closed: false,
                }),
                arrival: Condvar::new(),
            }),
            pending: HashSet::new(),
            last_id: 0,
        }
    }

    /// Start `req` on a background thread. Returns its id (never 0).
    pub fn start(&mut self, loader: Arc<Loader>, req: ScriptRequest, console: bool) -> u32 {
        self.last_id = self.last_id.wrapping_add(1).max(1);
        let id = self.last_id;
        self.pending.insert(id);
        let spawn = {
            let mut q = self.shared.lock();
            q.waiting.push_back(Job {
                id,
                loader,
                req,
                console,
            });
            let spawn = q.workers < MAX_IN_FLIGHT;
            if spawn {
                q.workers += 1;
            }
            spawn
        };
        if spawn {
            let shared = self.shared.clone();
            let started = std::thread::Builder::new()
                .name("huginn-fetch".into())
                .spawn(move || work(&shared))
                .is_ok();
            if !started {
                // No thread: the requests no other thread will take fail.
                let mut q = self.shared.lock();
                q.workers -= 1;
                if q.workers == 0 {
                    let failed: Vec<u32> = q.waiting.drain(..).map(|j| j.id).collect();
                    q.arrived.extend(failed.into_iter().map(|id| (id, None)));
                }
            }
        }
        id
    }

    /// Abort request `id`: a request still waiting never goes out, and the
    /// response of one in flight is dropped when it arrives.
    pub fn cancel(&mut self, id: u32) {
        if self.pending.remove(&id) {
            self.shared.lock().waiting.retain(|j| j.id != id);
        }
    }

    /// Responses that arrived for requests the page still waits for, in
    /// arrival order.
    pub fn take_arrived(&mut self) -> Vec<(u32, Option<Response>)> {
        let arrived: Vec<_> = self.shared.lock().arrived.drain(..).collect();
        arrived
            .into_iter()
            .filter(|(id, _)| self.pending.remove(id))
            .collect()
    }

    /// Whether a response is ready (`take_arrived` may still drop it).
    pub fn has_arrived(&self) -> bool {
        !self.shared.lock().arrived.is_empty()
    }

    /// Requests whose response the page has not got yet.
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// Sleep until a response arrives, at most `timeout`. True if one is ready.
    pub fn wait(&self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        let mut q = self.shared.lock();
        while q.arrived.is_empty() {
            if self.shared.arrival.wait_until(&mut q, end).timed_out() {
                return !q.arrived.is_empty();
            }
        }
        true
    }

    /// Called from the request's thread when a response arrives and none
    /// was waiting to be taken.
    pub fn set_waker(&self, waker: Option<Waker>) {
        self.shared.lock().waker = waker;
    }
}

impl Drop for Fetches {
    fn drop(&mut self) {
        let mut q = self.shared.lock();
        q.closed = true;
        q.waiting.clear();
        q.arrived.clear();
        q.waker = None;
    }
}

/// A request thread: send waiting requests until none are left.
fn work(shared: &Shared) {
    loop {
        // Before the first paint, the page's own requests go first
        // (`Loader::hold_script_requests`). The request waits in the queue,
        // so one aborted meanwhile never goes out.
        let next = shared.lock().waiting.front().map(|j| j.loader.clone());
        if let Some(loader) = next {
            loader.wait_for_script_turn();
        }
        let job = {
            let mut q = shared.lock();
            match q.waiting.pop_front() {
                Some(j) => j,
                None => {
                    q.workers -= 1;
                    return;
                }
            }
        };
        let Job {
            id,
            loader,
            req,
            console,
        } = job;
        // A panic in the loader is this request's network error, not a
        // request that never ends.
        let response = catch_unwind(AssertUnwindSafe(|| send(&loader, req, console)))
            .ok()
            .flatten();
        drop(loader);
        let waker = {
            let mut q = shared.lock();
            if q.closed {
                continue;
            }
            let first = q.arrived.is_empty();
            q.arrived.push_back((id, response));
            shared.arrival.notify_all();
            // The engine takes all arrived responses at once: wake it for
            // the first one only.
            first.then(|| q.waker.clone()).flatten()
        };
        if let Some(wake) = waker {
            wake();
        }
    }
}
