//! Requests in flight, keyed like Chrome matches preloads: URL, what the
//! resource is for, and the CORS mode with its credentials.
//!
//! - **Preloads** ([`Loader::preload`]): the shell's preload scanner starts
//!   the subresources it finds while the document is still arriving. The
//!   response waits here until the parser asks for the same request
//!   ([`Loader::fetch_with`], [`Loader::fetch_uncached`]), which takes it
//!   instead of fetching again. One with another CORS mode or credentials,
//!   or another destination, is a different request, as in Chrome: it does
//!   not take the preload.
//! - **Priorities.** Requests carry no HTTP/2 priority, so lower ones are
//!   held back instead (Chrome orders them the same way, but sends them
//!   all and lets the server's HTTP/2 priorities pick): [`Priority::High`]
//!   preloads wait while a [`Priority::Blocking`] one is queued or in
//!   flight, and [`Priority::Low`] ones while a more urgent one is and
//!   while the loader holds requests ([`Loader::hold_script_requests`]:
//!   the window's first paint). A consumer that asks for a queued preload
//!   sends it at once.
//! - **Identical fetches.** [`Loader::fetch_with`] calls for the same
//!   request at the same time share one exchange (a page that links the
//!   same stylesheet several times fetched it once per link, all at once).
//!
//! Preloads that nothing takes keep their bodies until
//! [`Loader::clear_preloads`] (the end of the load) or the loader drops.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::{Condvar, Mutex};

use crate::{CorsSettings, Loader, MAX_PARALLEL, NetError, Resource, ResourceKind, Url};

/// How soon a preload goes out (most urgent first).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    /// What the first paint waits for (stylesheets for the screen,
    /// parser-blocking scripts): at once.
    Blocking,
    /// Needed soon, but nothing waits for it yet (`<link rel=preload>` of
    /// fonts and stylesheets): when no blocking preload is queued or in
    /// flight.
    High,
    /// The rest (images, async, defer and module scripts, stylesheets for
    /// other media): when no more urgent preload is queued or in flight and
    /// the loader holds no requests.
    Low,
}

impl Priority {
    const ALL: [Priority; 3] = [Priority::Blocking, Priority::High, Priority::Low];
}

/// What a response can answer: Chrome's preload matching (URL, request
/// destination, CORS mode and credentials).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    pub(crate) url: Url,
    pub(crate) kind: ResourceKind,
    pub(crate) cors: CorsSettings,
}

pub(crate) type Fetched = Result<Arc<Resource>, NetError>;

/// One request: a preload (with its priority) or a fetch in flight.
pub(crate) struct Slot {
    state: Mutex<State>,
    changed: Condvar,
    /// False: a [`Loader::fetch_with`] in flight, not a preload.
    preload: bool,
    /// Its [`Priority`] (index into `Priority::ALL`); a queued one becomes
    /// more urgent when the scanner finds a more urgent use of it.
    priority: AtomicU8,
}

enum State {
    Queued,
    Running,
    Done(Fetched),
    /// A consumer took the response ([`Loader::fetch_uncached`]).
    Taken,
}

impl Slot {
    fn new(priority: Option<Priority>, state: State) -> Arc<Slot> {
        Arc::new(Slot {
            state: Mutex::new(state),
            changed: Condvar::new(),
            preload: priority.is_some(),
            priority: AtomicU8::new(priority.unwrap_or(Priority::Blocking) as u8),
        })
    }

    /// A fetch in flight that is not a preload counts as blocking.
    fn priority(&self) -> Priority {
        Priority::ALL[usize::from(self.priority.load(Ordering::Relaxed))]
    }

    /// Mark it running if it is still queued. True: the caller sends it.
    fn start(&self) -> bool {
        let mut s = self.state.lock();
        if matches!(*s, State::Queued) {
            *s = State::Running;
            true
        } else {
            false
        }
    }

    fn finish(&self, r: Fetched) {
        *self.state.lock() = State::Done(r);
        self.changed.notify_all();
    }

    fn is_done(&self) -> bool {
        matches!(*self.state.lock(), State::Done(_))
    }

    /// Wait for the response. `take`: move it out (the slot is left
    /// `Taken`). `None`: another consumer took it.
    fn wait(&self, take: bool) -> Option<Fetched> {
        let mut s = self.state.lock();
        loop {
            match &*s {
                State::Done(r) if !take => return Some(r.clone()),
                State::Done(_) => {
                    let State::Done(r) = std::mem::replace(&mut *s, State::Taken) else {
                        unreachable!()
                    };
                    return Some(r);
                }
                State::Taken => return None,
                State::Queued | State::Running => self.changed.wait(&mut s),
            }
        }
    }
}

/// A loader's requests in flight and queued preloads.
#[derive(Default)]
pub(crate) struct Preloads {
    slots: HashMap<Key, Arc<Slot>>,
    /// Queued preloads, in the order they were found.
    queue: VecDeque<(Key, Arc<Slot>)>,
    /// Threads sending queued preloads.
    workers: usize,
    /// Preloads in flight per priority.
    running: [usize; 3],
    /// The loader, for the threads (set by the first preload).
    loader: Weak<Loader>,
    /// [`Loader::clear_preloads`] was called: preloads still queued or in
    /// flight go out (they hold up nothing), but their responses are not
    /// kept for later.
    cleared: bool,
}

impl Loader {
    /// Start fetching `url` before the parser asks for it (the preload
    /// scanner, `<link rel=preload>`). The parser's request with the same
    /// URL, kind and CORS settings takes the response instead of fetching
    /// again; [`Priority::Low`] ones wait (see the module docs). Does
    /// nothing when the request is already in flight, queued or answered,
    /// and for URLs other than http and https.
    pub fn preload(
        self: &Arc<Self>,
        url: &Url,
        kind: ResourceKind,
        cors: CorsSettings,
        priority: Priority,
    ) {
        if !matches!(url.scheme(), "http" | "https") || self.check_local(url).is_err() {
            return;
        }
        let key = Key {
            url: url.clone(),
            kind,
            cors,
        };
        if self.has_response(&key) {
            return;
        }
        let mut p = self.preloads.lock();
        if let Some(s) = p.slots.get(&key) {
            // A queued preload that turns out to be more urgent.
            if s.preload && priority < s.priority() {
                s.priority.fetch_min(priority as u8, Ordering::Relaxed);
                self.pump(&mut p);
            }
            return;
        }
        p.loader = Arc::downgrade(self);
        let slot = Slot::new(Some(priority), State::Queued);
        p.slots.insert(key.clone(), slot.clone());
        p.queue.push_back((key, slot));
        self.pump(&mut p);
    }

    /// Drop the responses of preloads nothing took. Ones still queued or in
    /// flight go on (Chrome fetches them too), but nothing keeps their
    /// responses, unless the parser asked for them meanwhile. Call when the
    /// page's load is done.
    pub fn clear_preloads(&self) {
        let mut p = self.preloads.lock();
        p.cleared = true;
        p.slots.retain(|_, s| !s.preload || !s.is_done());
    }

    /// The loader's memory already has the response for `key`.
    fn has_response(&self, key: &Key) -> bool {
        self.cache
            .lock()
            .expect("cache poisoned")
            .get(&key.url)
            .is_some_and(|r| {
                key.cors == CorsSettings::NoCors || r.response_type != crate::ResponseType::Opaque
            })
    }

    /// Start threads for the preloads that may go now.
    pub(crate) fn pump(&self, p: &mut Preloads) {
        let ready = self.ready(p);
        while p.workers < ready.min(MAX_PARALLEL) {
            let loader = p.loader.clone();
            let spawned = std::thread::Builder::new()
                .name("huginn-preload".into())
                .spawn(move || preload_worker(&loader));
            if spawned.is_err() {
                break;
            }
            p.workers += 1;
        }
    }

    /// The least urgent priority whose queued preloads may go now.
    fn may_go(&self, p: &mut Preloads) -> Option<Priority> {
        p.queue
            .retain(|(_, s)| matches!(*s.state.lock(), State::Queued));
        let queued = |q: Priority| p.queue.iter().any(|(_, s)| s.priority() == q);
        let busy = |q: Priority| queued(q) || p.running[q as usize] > 0;
        let lowest = if busy(Priority::Blocking) {
            Priority::Blocking
        } else if busy(Priority::High) || *self.script_holds.lock() > 0 {
            Priority::High
        } else {
            Priority::Low
        };
        p.queue
            .iter()
            .any(|(_, s)| s.priority() <= lowest)
            .then_some(lowest)
    }

    /// How many queued preloads may go now.
    fn ready(&self, p: &mut Preloads) -> usize {
        let Some(lowest) = self.may_go(p) else {
            return 0;
        };
        p.queue
            .iter()
            .filter(|(_, s)| s.priority() <= lowest)
            .count()
    }

    /// The next preload a thread may send, if any (else the thread ends).
    fn next_preload(&self) -> Option<(Key, Arc<Slot>)> {
        let mut p = self.preloads.lock();
        // The most urgent that may go, in the order they were found.
        let next = self.may_go(&mut p).and_then(|lowest| {
            let first = Priority::ALL
                .iter()
                .filter(|q| **q <= lowest)
                .find_map(|q| p.queue.iter().position(|(_, s)| s.priority() == *q))?;
            p.queue.remove(first)
        });
        if next.is_none() {
            p.workers -= 1;
        }
        next
    }

    /// Send a preload this thread started.
    fn send_preload(&self, key: &Key, slot: &Slot) {
        let q = slot.priority() as usize;
        self.preloads.lock().running[q] += 1;
        let r = self
            .fetch_uncached_with(&key.url, key.kind, key.cors)
            .map(Arc::new);
        slot.finish(r);
        let mut p = self.preloads.lock();
        p.running[q] -= 1;
        if p.cleared && p.slots.get(key).is_some_and(|s| std::ptr::eq(&**s, slot)) {
            p.slots.remove(key);
        }
        self.pump(&mut p);
    }

    /// The response of a preload or of a fetch in flight for `key`, waited
    /// for (a queued preload is sent on this thread). A preload's response
    /// is taken: nobody else gets it. `None`: there is none, or another
    /// consumer took it.
    pub(crate) fn join(&self, key: &Key) -> Option<Fetched> {
        let slot = self.preloads.lock().slots.get(key).cloned()?;
        if slot.start() {
            self.send_preload(key, &slot);
        }
        let r = slot.wait(slot.preload);
        self.forget(key, &slot);
        r
    }

    /// A finished preload for `key`, taken; `None` if there is none or it
    /// is still on the network.
    pub(crate) fn take_finished(&self, key: &Key) -> Option<Fetched> {
        let slot = self.preloads.lock().slots.get(key).cloned()?;
        if !slot.is_done() {
            return None;
        }
        let r = slot.wait(true);
        self.forget(key, &slot);
        r
    }

    /// Fetch `key` unless an identical fetch is in flight (then wait for
    /// it) or a preload has it (then take it).
    pub(crate) fn fetch_shared(&self, key: &Key) -> Fetched {
        let slot = {
            let mut p = self.preloads.lock();
            match p.slots.get(key) {
                Some(s) => Err(s.clone()),
                // Another request took a response for it since this one
                // looked (it is in memory before its slot goes).
                None if self.has_response(key) => {
                    let hit = self
                        .cache
                        .lock()
                        .expect("cache poisoned")
                        .get(&key.url)
                        .cloned();
                    return hit.ok_or_else(|| NetError::Transport(format!("{}: gone", key.url)));
                }
                None => {
                    let s = Slot::new(None, State::Running);
                    p.slots.insert(key.clone(), s.clone());
                    Ok(s)
                }
            }
        };
        match slot {
            Ok(mine) => {
                let r = self
                    .fetch_uncached_with(&key.url, key.kind, key.cors)
                    .map(Arc::new);
                if let Ok(res) = &r {
                    self.cache
                        .lock()
                        .expect("cache poisoned")
                        .insert(key.url.clone(), res.clone());
                }
                mine.finish(r.clone());
                self.forget(key, &mine);
                r
            }
            Err(theirs) => {
                if theirs.start() {
                    self.send_preload(key, &theirs);
                }
                let r = theirs.wait(false);
                // Into the loader's memory before the slot goes, so a
                // request that misses the slot finds the response there.
                if let Some(Ok(res)) = &r {
                    self.cache
                        .lock()
                        .expect("cache poisoned")
                        .insert(key.url.clone(), res.clone());
                }
                self.forget(key, &theirs);
                match r {
                    Some(r) => r,
                    // Taken by another consumer: fetch it here.
                    None => self.fetch_shared(key),
                }
            }
        }
    }

    /// Remove `slot` from the table (if it is still the one for `key`).
    fn forget(&self, key: &Key, slot: &Arc<Slot>) {
        let mut p = self.preloads.lock();
        if p.slots.get(key).is_some_and(|s| Arc::ptr_eq(s, slot)) {
            p.slots.remove(key);
        }
    }
}

/// A preload thread: sends queued preloads until none may go. It holds the
/// loader only while it sends one, so a closed page's queue is not sent.
fn preload_worker(loader: &Weak<Loader>) {
    while let Some(me) = loader.upgrade() {
        let Some((key, slot)) = me.next_preload() else {
            return;
        };
        if slot.start() {
            me.send_preload(&key, &slot);
        }
    }
}
