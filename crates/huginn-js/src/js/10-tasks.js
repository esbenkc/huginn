// ------------------------------------------------------------- task queue --
// setTimeout, setInterval, requestAnimationFrame, MessageChannel messages,
// XHR and fetch completion are all timers ordered by (due time, sequence).
// The engine advances (virtual) time and calls `runOne` for each due timer,
// with a microtask checkpoint after each one. Due times count from the start
// of the current engine call, not from `performance.now()`, so the order of
// timers does not depend on how fast the script ran.

let timerSeq = 0;
const timers = new Map();
const FRAME = 1000 / 60;

function addTimer(due, fn, interval) {
  const id = ++timerSeq;
  timers.set(id, { id, due, fn, interval });
  return id;
}
const taskNow = () => N.taskNow();
function queueTask(fn) { return addTimer(taskNow(), fn, 0); }

function timerFn(handler, args) {
  if (typeof handler === 'function') return args.length ? () => handler.apply(G, args) : handler;
  const code = String(handler);
  return () => (0, eval)(code);
}
function setTimeout(handler, ms, ...args) {
  ms = Number(ms) || 0;
  return addTimer(taskNow() + Math.max(0, ms), timerFn(handler, args), 0);
}
function setInterval(handler, ms, ...args) {
  // Browsers clamp nested timers to 4 ms; a floor also stops a 0 ms
  // interval from spinning the settle loop.
  ms = Math.max(4, Number(ms) || 0);
  return addTimer(taskNow() + ms, timerFn(handler, args), ms);
}
function clearTimer(id) { timers.delete(Number(id)); }
function requestAnimationFrame(cb) {
  if (typeof cb !== 'function') throw new TypeError("parameter 1 is not of type 'Function'");
  // The next frame is one frame after this task, so callbacks requested in
  // one task share a timestamp and the order against timers is fixed.
  const due = taskNow() + FRAME;
  return addTimer(due, () => cb(due), 0);
}
function requestIdleCallback(cb) {
  const deadline = { didTimeout: false, timeRemaining: () => 10 };
  return addTimer(taskNow() + 1, () => cb(deadline), 0);
}
function queueMicrotask(cb) {
  if (typeof cb !== 'function') throw new TypeError("parameter 1 is not of type 'Function'");
  Promise.resolve().then(() => { try { cb(); } catch (e) { reportError(e); } });
}

function nextTimer() {
  let best = null;
  for (const t of timers.values()) {
    if (!best || t.due < best.due || (t.due === best.due && t.id < best.id)) best = t;
  }
  return best;
}

// Run the earliest timer if it is due at `at`. Returns true if one ran.
function runOne(at) {
  const t = nextTimer();
  if (!t || t.due > at) return false;
  if (t.interval) {
    // Skip missed ticks instead of firing them back to back.
    t.due += t.interval;
    if (t.due <= at) t.due = at + t.interval;
  } else {
    timers.delete(t.id);
  }
  try { t.fn(); } catch (e) { reportError(e); }
  return true;
}
function nextDue() {
  const t = nextTimer();
  return t ? t.due : -1;
}
