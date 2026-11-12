// -------------------------------------------------------------- observers --

// MutationObserver with real records for the mutations scripts make through
// this prelude: attributes (setAttribute and friends), character data, and
// child lists (insert, remove, innerHTML, textContent). Records are
// delivered in a microtask, like in a browser.
const mutationObservers = [];
function notifyMutation(type, targetId, init) {
  if (!mutationObservers.length) return;
  for (const mo of mutationObservers) {
    for (const [nodeId, opts] of mo._targets) {
      if (nodeId !== targetId && !(opts.subtree && N.contains(nodeId, targetId))) continue;
      if (type === 'attributes' && !opts.attributes) continue;
      if (type === 'attributes' && opts.attributeFilter && opts.attributeFilter.indexOf(init.attributeName) < 0) continue;
      if (type === 'characterData' && !opts.characterData) continue;
      if (type === 'childList' && !opts.childList) continue;
      const rec = {
        type, target: wrap(targetId), addedNodes: init.addedNodes || nodeList([]), removedNodes: init.removedNodes || nodeList([]),
        previousSibling: init.previousSibling || null, nextSibling: init.nextSibling || null,
        attributeName: init.attributeName || null, attributeNamespace: null,
        oldValue: (type === 'attributes' && opts.attributeOldValue) || (type === 'characterData' && opts.characterDataOldValue) ? init.oldValue : null,
      };
      mo._records.push(rec);
      if (!mo._scheduled) {
        mo._scheduled = true;
        queueMicrotask(() => {
          mo._scheduled = false;
          const recs = mo.takeRecords();
          if (recs.length) mo._cb.call(mo, recs, mo);
        });
      }
      break;
    }
  }
}
class MutationObserver {
  constructor(cb) {
    if (typeof cb !== 'function') throw new TypeError("parameter 1 is not of type 'Function'");
    defProp(this, '_cb', { value: cb });
    defProp(this, '_targets', { value: new Map() });
    defProp(this, '_records', { value: [], writable: true });
    defProp(this, '_scheduled', { value: false, writable: true });
  }
  observe(target, options) {
    const o = Object.assign({}, options || {});
    if (o.attributeOldValue || o.attributeFilter) o.attributes = true;
    if (o.characterDataOldValue) o.characterData = true;
    this._targets.set(idOf(target), o);
    if (mutationObservers.indexOf(this) < 0) mutationObservers.push(this);
  }
  disconnect() {
    this._targets.clear();
    this._records = [];
    const i = mutationObservers.indexOf(this);
    if (i >= 0) mutationObservers.splice(i, 1);
  }
  takeRecords() { const r = this._records; this._records = []; return r; }
}
expose(MutationObserver);
defProp(G, 'WebKitMutationObserver', { value: MutationObserver, writable: true, configurable: true });

function viewportRect() { const v = N.viewport(); return new DOMRect(0, 0, v[0], v[1]); }

// Reports each observed element once, as soon as possible, with its real
// geometry (lazy loaders then load what is in the viewport).
class IntersectionObserver {
  constructor(cb, options) {
    defProp(this, '_cb', { value: cb });
    defProp(this, '_pending', { value: [], writable: true });
    this.root = (options && options.root) || null;
    this.rootMargin = (options && options.rootMargin) || '0px';
    const t = options && options.threshold;
    this.thresholds = Array.isArray(t) ? t : [t || 0];
  }
  observe(el) {
    this._pending.push(el);
    if (this._pending.length > 1) return;
    queueTask(() => {
      const els = this._pending;
      this._pending = [];
      // Browsers never call back with no entries (all were unobserved).
      if (!els.length) return;
      const vp = viewportRect();
      const entries = els.map(target => {
        const r = target.getBoundingClientRect();
        const ix = Math.max(0, Math.min(r.right, vp.right) - Math.max(r.left, 0));
        const iy = Math.max(0, Math.min(r.bottom, vp.bottom) - Math.max(r.top, 0));
        const visible = (r.width > 0 || r.height > 0) && r.bottom >= 0 && r.top <= vp.bottom && r.right >= 0 && r.left <= vp.right;
        const area = r.width * r.height;
        return {
          target, time: now(), rootBounds: vp, boundingClientRect: r,
          intersectionRect: new DOMRect(Math.max(r.left, 0), Math.max(r.top, 0), ix, iy),
          isIntersecting: visible, isVisible: visible, intersectionRatio: visible ? (area ? (ix * iy) / area : 1) : 0,
        };
      });
      try { this._cb.call(this, entries, this); } catch (e) { reportError(e); }
    });
  }
  unobserve(el) { this._pending = this._pending.filter(x => x !== el); }
  disconnect() { this._pending = []; }
  takeRecords() { return []; }
}
expose(IntersectionObserver);
expose(class IntersectionObserverEntry {}, 'IntersectionObserverEntry');
class ResizeObserver {
  constructor(cb) { defProp(this, '_cb', { value: cb }); defProp(this, '_pending', { value: [], writable: true }); }
  observe(el) {
    this._pending.push(el);
    if (this._pending.length > 1) return;
    queueTask(() => {
      const els = this._pending;
      this._pending = [];
      if (!els.length) return;
      const entries = els.map(target => {
        const r = target.getBoundingClientRect();
        const size = [{ inlineSize: r.width, blockSize: r.height }];
        return { target, contentRect: new DOMRect(0, 0, r.width, r.height), borderBoxSize: size, contentBoxSize: size, devicePixelContentBoxSize: size };
      });
      try { this._cb.call(this, entries, this); } catch (e) { reportError(e); }
    });
  }
  unobserve(el) { this._pending = this._pending.filter(x => x !== el); }
  disconnect() { this._pending = []; }
}
expose(ResizeObserver);
expose(class ResizeObserverEntry {}, 'ResizeObserverEntry');
class PerformanceObserver {
  constructor(cb) { this._cb = cb; }
  observe() {}
  disconnect() {}
  takeRecords() { return []; }
  static get supportedEntryTypes() { return []; }
}
expose(PerformanceObserver);
expose(class ReportingObserver { observe() {} disconnect() {} takeRecords() { return []; } }, 'ReportingObserver');

// --------------------------------------------------------------- storage --

class Storage {
  get length() { return this._map.size; }
  key(i) { return Array.from(this._map.keys())[i] ?? null; }
  getItem(k) { const v = this._map.get(String(k)); return v === undefined ? null : v; }
  setItem(k, v) { this._map.set(String(k), String(v)); }
  removeItem(k) { this._map.delete(String(k)); }
  clear() { this._map.clear(); }
}
expose(Storage);
// In memory, empty at load: like a first visit in a fresh profile.
function makeStorage() {
  const s = Object.create(Storage.prototype);
  defProp(s, '_map', { value: new Map() });
  return new Proxy(s, {
    get(t, p) {
      if (typeof p !== 'string' || p in t) { const v = t[p]; return typeof v === 'function' ? v.bind(t) : v; }
      return t.getItem(p) ?? undefined;
    },
    set(t, p, v) { if (typeof p === 'string') t.setItem(p, v); return true; },
    has(t, p) { return p in t || t._map.has(p); },
    deleteProperty(t, p) { t.removeItem(p); return true; },
    ownKeys(t) { return Array.from(t._map.keys()); },
    getOwnPropertyDescriptor(t, p) {
      return t._map.has(p) ? { value: t._map.get(p), writable: true, enumerable: true, configurable: true } : undefined;
    },
  });
}
const localStorage = makeStorage();
const sessionStorage = makeStorage();

// ------------------------------------------------------ location, history --

let curUrl = N.docUrl();
const urlParts = () => N.url(curUrl, '') || URL_PARTS.map(() => '');
let historyState = null;

function setHash(hash) {
  const oldURL = curUrl;
  const p = N.urlSet(curUrl, 'hash', hash);
  if (!p || p[0] === curUrl) return;
  curUrl = p[0];
  queueTask(() => {
    dispatch(G, new HashChangeEvent('hashchange', { oldURL, newURL: curUrl }));
  });
}
// The browser navigates after the script returns; `huginn render` ignores it.
function navigate(url, replace) {
  const p = N.url(String(url), curUrl);
  if (!p) return;
  const cur = urlParts();
  // Same document apart from the fragment: a hash change, not a navigation.
  if (p[0].split('#')[0] === cur[0].split('#')[0] && p[9] !== cur[9]) { setHash(p[9]); return; }
  N.navigate(p[0], !!replace);
}

class Location {
  get href() { return curUrl; }
  set href(v) { navigate(v); }
  get hash() { return urlParts()[9]; }
  set hash(v) { v = String(v); setHash(v === '' || v[0] === '#' ? v : '#' + v); }
  assign(url) { navigate(url); }
  replace(url) { navigate(url, true); }
  reload() { N.navigate(curUrl, true); }
  toString() { return curUrl; }
  get ancestorOrigins() { return { length: 0, item() { return null; }, contains() { return false; } }; }
}
URL_PARTS.forEach((k, i) => {
  if (k === 'href' || k === 'hash') return;
  defProp(Location.prototype, k, {
    get() { return urlParts()[i]; },
    set(v) {
      if (k === 'origin') return;
      const p = N.urlSet(curUrl, k, String(v));
      if (p) navigate(p[0]);
    },
    configurable: true,
  });
});
expose(Location);
const location = new Location();

class History {
  get length() { return 1; }
  get state() { return historyState; }
  get scrollRestoration() { return 'auto'; }
  set scrollRestoration(v) {}
  pushState(state, title, url) {
    historyState = state === undefined ? null : state;
    if (url !== undefined && url !== null) {
      const p = N.url(String(url), curUrl);
      if (!p) throw domErr('SecurityError', 'invalid URL');
      curUrl = p[0];
    }
  }
  replaceState(state, title, url) { this.pushState(state, title, url); }
  back() {}
  forward() {}
  go() {}
}
expose(History);
const history = new History();

// ------------------------------------------------------------- navigator --

// N.os: the system this build runs on; N.ua, N.secChUa: the User-Agent and
// Sec-CH-UA headers the network sends (`install_ops`), so script and headers agree.
const OS = N.os === 'macos'
  ? { platform: 'MacIntel', name: 'macOS', version: '14.0.0', arch: 'arm' }
  : { platform: 'Linux x86_64', name: 'Linux', version: '6.8.0', arch: 'x86' };
const UA = N.ua;
const BRANDS = Object.freeze(Array.from(N.secChUa.matchAll(/"([^"]*)";v="([^"]*)"/g),
  m => Object.freeze({ brand: m[1], version: m[2] })));
// Full versions: Chrome's reduced `<major>.0.0.0`, and Huginn's own.
const FULL_VERSIONS = Object.freeze(BRANDS.map(b => Object.freeze({
  brand: b.brand, version: b.brand === 'Huginn' ? N.huginnVersion : b.version + '.0.0.0' })));
const CHROME_FULL = FULL_VERSIONS.find(b => b.brand === 'Chromium').version;
class Navigator {}
const navigatorProps = {
  userAgent: UA, appVersion: UA.slice(8), appName: 'Netscape', appCodeName: 'Mozilla', product: 'Gecko',
  productSub: '20030107', vendor: 'Google Inc.', vendorSub: '', platform: OS.platform, language: 'en-US',
  languages: Object.freeze(['en-US', 'en']), cookieEnabled: true, onLine: true, hardwareConcurrency: 8,
  deviceMemory: 8, maxTouchPoints: 0, webdriver: false, doNotTrack: null, pdfViewerEnabled: true,
  plugins: Object.freeze([]), mimeTypes: Object.freeze([]),
  userAgentData: {
    brands: BRANDS, mobile: false, platform: OS.name,
    getHighEntropyValues() {
      return Promise.resolve({ brands: BRANDS, mobile: false, platform: OS.name, platformVersion: OS.version,
        architecture: OS.arch, bitness: '64', model: '', wow64: false, formFactors: ['Desktop'],
        uaFullVersion: CHROME_FULL, fullVersionList: FULL_VERSIONS });
    },
    toJSON() { return { brands: BRANDS, mobile: false, platform: OS.name }; },
  },
  connection: Object.assign(new EventTarget(), { effectiveType: '4g', downlink: 10, rtt: 50, saveData: false, type: 'wifi' }),
  permissions: { query() { return Promise.resolve({ state: 'prompt', onchange: null, addEventListener() {}, removeEventListener() {} }); } },
  clipboard: { writeText() { return Promise.resolve(); }, readText() { return Promise.resolve(''); }, write() { return Promise.resolve(); }, read() { return Promise.resolve([]); } },
  geolocation: { getCurrentPosition(ok, err) { if (err) queueTask(() => err({ code: 1, message: 'denied' })); }, watchPosition() { return 0; }, clearWatch() {} },
  storage: { estimate() { return Promise.resolve({ quota: 1 << 30, usage: 0 }); }, persist() { return Promise.resolve(false); }, persisted() { return Promise.resolve(false); } },
  locks: { request(name, opts, cb) { const f = typeof opts === 'function' ? opts : cb; return Promise.resolve().then(() => f({ name })); }, query() { return Promise.resolve({ held: [], pending: [] }); } },
  mediaSession: { metadata: null, playbackState: 'none', setActionHandler() {} },
};
for (const k in navigatorProps) defProp(Navigator.prototype, k, { value: navigatorProps[k], configurable: true, enumerable: true });
Navigator.prototype.javaEnabled = function () { return false; };
Navigator.prototype.sendBeacon = function () { return true; };
Navigator.prototype.vibrate = function () { return false; };
Navigator.prototype.registerProtocolHandler = function () {};
Navigator.prototype.getGamepads = function () { return []; };
expose(Navigator);
const navigator = new Navigator();

const screen = (() => {
  const v = N.viewport();
  const orientation = Object.assign(new EventTarget(), { type: 'landscape-primary', angle: 0, onchange: null, lock() { return Promise.reject(domErr('NotSupportedError')); }, unlock() {} });
  return { width: v[0], height: v[1], availWidth: v[0], availHeight: v[1], availLeft: 0, availTop: 0, colorDepth: 24, pixelDepth: 24, orientation };
})();

// ----------------------------------------------------------- matchMedia --

function mediaFeature(f) {
  const v = N.viewport();
  const len = (s) => {
    const m = /^(-?[\d.]+)(px|em|rem)?$/.exec(s.trim());
    if (!m) return NaN;
    return parseFloat(m[1]) * (m[2] === 'em' || m[2] === 'rem' ? 16 : 1);
  };
  let m = /^\(\s*([a-z-]+)\s*(?::\s*([^)]*?))?\s*\)$/.exec(f);
  if (m) {
    const name = m[1], val = m[2] === undefined ? null : m[2].trim().toLowerCase();
    switch (name) {
      case 'min-width': return v[0] >= len(val);
      case 'max-width': return v[0] <= len(val);
      case 'min-height': return v[1] >= len(val);
      case 'max-height': return v[1] <= len(val);
      case 'width': return val === null ? true : v[0] === len(val);
      case 'height': return val === null ? true : v[1] === len(val);
      case 'orientation': return val === (v[0] >= v[1] ? 'landscape' : 'portrait');
      case 'prefers-color-scheme': return val === 'light';
      case 'prefers-reduced-motion': case 'prefers-reduced-transparency': case 'prefers-reduced-data':
        return val === 'no-preference' || val === null ? val === 'no-preference' : false;
      case 'prefers-contrast': return val === 'no-preference';
      case 'forced-colors': return val === 'none';
      case 'inverted-colors': return val === 'none';
      case 'hover': case 'any-hover': return val === null || val === 'hover';
      case 'pointer': case 'any-pointer': return val === null || val === 'fine';
      case 'color': return true;
      case 'monochrome': return val === '0';
      case 'display-mode': return val === 'browser';
      case 'scripting': return val === 'enabled';
      case 'update': return val === 'fast';
      case 'min-resolution': return 96 >= parseFloat(val) * (/dppx|x$/.test(val) ? 96 : 1);
      case 'max-resolution': return 96 <= parseFloat(val) * (/dppx|x$/.test(val) ? 96 : 1);
      case '-webkit-min-device-pixel-ratio': case 'min-device-pixel-ratio': return 1 >= parseFloat(val);
      case '-webkit-max-device-pixel-ratio': case 'max-device-pixel-ratio': return 1 <= parseFloat(val);
      case 'min-aspect-ratio': { const [a, b] = val.split('/').map(Number); return v[0] / v[1] >= a / (b || 1); }
      case 'max-aspect-ratio': { const [a, b] = val.split('/').map(Number); return v[0] / v[1] <= a / (b || 1); }
    }
    return false;
  }
  // Range syntax: (width >= 600px), (400px <= width <= 700px).
  m = /^\(\s*(?:([\d.]+[a-z]*)\s*(<=|<|>=|>)\s*)?(width|height)\s*(?:(<=|<|>=|>|=)\s*([\d.]+[a-z]*))?\s*\)$/.exec(f);
  if (m) {
    const x = m[3] === 'width' ? v[0] : v[1];
    const cmp = (a, op, b) => op === '<' ? a < b : op === '<=' ? a <= b : op === '>' ? a > b : op === '>=' ? a >= b : a === b;
    if (m[1] && !cmp(len(m[1]), m[2], x)) return false;
    if (m[5] && !cmp(x, m[4], len(m[5]))) return false;
    return true;
  }
  return false;
}
function evalMedia(q) {
  q = String(q).trim().toLowerCase();
  if (q === '' || q === 'all') return true;
  return q.split(',').some(part => {
    part = part.trim();
    let neg = false;
    if (part.startsWith('not ')) { neg = true; part = part.slice(4).trim(); }
    if (part.startsWith('only ')) part = part.slice(5).trim();
    const terms = part.split(/\s+and\s+/);
    const ok = terms.every(t => {
      t = t.trim();
      if (t === 'all' || t === 'screen') return true;
      if (t === 'print' || t === 'speech' || t === 'tv' || t === 'handheld') return false;
      if (t.startsWith('not ')) return !mediaFeature(t.slice(4).trim());
      return mediaFeature(t);
    });
    return neg ? !ok : ok;
  });
}
class MediaQueryList extends EventTarget {
  constructor(q) { super(); this.media = String(q); this.matches = evalMedia(q); this.onchange = null; }
  addListener(cb) { this.addEventListener('change', cb); }
  removeListener(cb) { this.removeEventListener('change', cb); }
}
expose(MediaQueryList);
function matchMedia(q) { return new MediaQueryList(q); }

// ----------------------------------------------------------- performance --

const timeOrigin = Date.now() - now();
// PerformanceNavigationTiming for this load (all phases at time 0).
function navigationEntry() {
  return { name: curUrl, entryType: 'navigation', initiatorType: 'navigation', type: 'navigate', startTime: 0,
    duration: now(), redirectCount: 0, nextHopProtocol: 'http/1.1', transferSize: 0, encodedBodySize: 0,
    decodedBodySize: 0, responseStatus: 200, domInteractive: 0, domContentLoadedEventStart: 0,
    domContentLoadedEventEnd: 0, domComplete: 0, loadEventStart: 0, loadEventEnd: 0, toJSON() { return this; } };
}
const perfMarks = [];
class Performance extends EventTarget {
  now() { return now(); }
  get timeOrigin() { return timeOrigin; }
  get timing() {
    const t = timeOrigin;
    return { navigationStart: t, fetchStart: t, domainLookupStart: t, domainLookupEnd: t, connectStart: t, connectEnd: t,
      requestStart: t, responseStart: t, responseEnd: t, domLoading: t, domInteractive: t, domContentLoadedEventStart: t,
      domContentLoadedEventEnd: t, domComplete: t, loadEventStart: t, loadEventEnd: t, unloadEventStart: 0, unloadEventEnd: 0,
      redirectStart: 0, redirectEnd: 0, secureConnectionStart: 0 };
  }
  get navigation() { return { type: 0, redirectCount: 0 }; }
  get memory() { return { jsHeapSizeLimit: 2 ** 31, totalJSHeapSize: 2 ** 24, usedJSHeapSize: 2 ** 23 }; }
  get eventCounts() { return new Map(); }
  mark(name, opts) {
    const e = { name: String(name), entryType: 'mark', startTime: opts && opts.startTime !== undefined ? opts.startTime : now(), duration: 0, detail: opts ? opts.detail : null };
    perfMarks.push(e);
    return e;
  }
  measure(name, start, end) {
    const find = (n) => { const m = perfMarks.filter(e => e.name === n).pop(); return m ? m.startTime : 0; };
    const s = typeof start === 'string' ? find(start) : 0;
    const e = typeof end === 'string' ? find(end) : now();
    const entry = { name: String(name), entryType: 'measure', startTime: s, duration: e - s, detail: null };
    perfMarks.push(entry);
    return entry;
  }
  clearMarks(name) { for (let i = perfMarks.length - 1; i >= 0; i--) if (perfMarks[i].entryType === 'mark' && (name === undefined || perfMarks[i].name === name)) perfMarks.splice(i, 1); }
  clearMeasures(name) { for (let i = perfMarks.length - 1; i >= 0; i--) if (perfMarks[i].entryType === 'measure' && (name === undefined || perfMarks[i].name === name)) perfMarks.splice(i, 1); }
  clearResourceTimings() {}
  setResourceTimingBufferSize() {}
  getEntries() { return [navigationEntry()].concat(perfMarks); }
  getEntriesByType(t) { return t === 'navigation' ? [navigationEntry()] : perfMarks.filter(e => e.entryType === t); }
  getEntriesByName(n, t) { return perfMarks.filter(e => e.name === n && (t === undefined || e.entryType === t)); }
  toJSON() { return { timeOrigin }; }
}
expose(Performance);
const performance = new Performance();

// --------------------------------------------------------------- console --

const counters = new Map();
const timersByLabel = new Map();
const console = {};
for (const [name, level] of [['log', 1], ['info', 1], ['debug', 0], ['trace', 0], ['warn', 2], ['error', 3],
  ['dir', 1], ['dirxml', 1], ['table', 1], ['group', 1], ['groupCollapsed', 1]]) {
  console[name] = function (...args) { N.log(level, format(args)); };
}
Object.assign(console, {
  groupEnd() {},
  clear() {},
  assert(cond, ...args) { if (!cond) N.log(3, 'Assertion failed: ' + format(args)); },
  count(label = 'default') { const n = (counters.get(label) || 0) + 1; counters.set(label, n); N.log(1, label + ': ' + n); },
  countReset(label = 'default') { counters.delete(label); },
  time(label = 'default') { timersByLabel.set(label, now()); },
  timeLog(label = 'default') { N.log(1, label + ': ' + (now() - (timersByLabel.get(label) || 0)) + ' ms'); },
  timeEnd(label = 'default') { this.timeLog(label); timersByLabel.delete(label); },
  timeStamp() {},
  profile() {},
  profileEnd() {},
});

// ----------------------------------------------------------------- misc --

const crypto = {
  getRandomValues(a) {
    if (!ArrayBuffer.isView(a)) throw new TypeError("parameter 1 is not of type 'ArrayBufferView'");
    const b = new Uint8Array(a.buffer, a.byteOffset, a.byteLength);
    for (let i = 0; i < b.length; i++) b[i] = (Math.random() * 256) | 0;
    return a;
  },
  randomUUID() {
    const b = crypto.getRandomValues(new Uint8Array(16));
    b[6] = (b[6] & 15) | 64;
    b[8] = (b[8] & 63) | 128;
    const h = Array.from(b, x => x.toString(16).padStart(2, '0')).join('');
    return h.slice(0, 8) + '-' + h.slice(8, 12) + '-' + h.slice(12, 16) + '-' + h.slice(16, 20) + '-' + h.slice(20);
  },
  subtle: {},
};
for (const m of ['digest', 'encrypt', 'decrypt', 'sign', 'verify', 'generateKey', 'importKey', 'exportKey', 'deriveKey', 'deriveBits', 'wrapKey', 'unwrapKey']) {
  crypto.subtle[m] = () => Promise.reject(domErr('NotSupportedError', 'crypto.subtle is not supported'));
}

function structuredClone(v, seen) {
  if (v === null || typeof v !== 'object') {
    if (typeof v === 'function' || typeof v === 'symbol') throw domErr('DataCloneError', 'value could not be cloned');
    return v;
  }
  seen = seen || new Map();
  if (seen.has(v)) return seen.get(v);
  let out;
  if (Array.isArray(v)) { out = []; seen.set(v, out); for (const x of v) out.push(structuredClone(x, seen)); return out; }
  if (v instanceof Date) return new Date(v.getTime());
  if (v instanceof RegExp) return new RegExp(v.source, v.flags);
  if (v instanceof Map) { out = new Map(); seen.set(v, out); for (const [k, x] of v) out.set(structuredClone(k, seen), structuredClone(x, seen)); return out; }
  if (v instanceof Set) { out = new Set(); seen.set(v, out); for (const x of v) out.add(structuredClone(x, seen)); return out; }
  if (ArrayBuffer.isView(v)) return v.slice();
  if (v instanceof ArrayBuffer) return v.slice(0);
  if (v instanceof Error) { const e = new Error(v.message); e.name = v.name; return e; }
  if (v[ID] !== undefined) throw domErr('DataCloneError', 'nodes could not be cloned');
  out = {};
  seen.set(v, out);
  for (const k of Object.keys(v)) out[k] = structuredClone(v[k], seen);
  return out;
}

const CSS = {
  supports(a, b) { return true; },
  escape(s) {
    s = String(s);
    let out = '';
    for (let i = 0; i < s.length; i++) {
      const c = s.charCodeAt(i);
      if (c === 0) out += '\uFFFD';
      else if ((c >= 1 && c <= 31) || c === 127 || (i === 0 && c >= 48 && c <= 57) || (i === 1 && c >= 48 && c <= 57 && s.charCodeAt(0) === 45)) out += '\\' + c.toString(16) + ' ';
      else if (i === 0 && c === 45 && s.length === 1) out += '\\' + s[i];
      else if (c >= 128 || c === 45 || c === 95 || (c >= 48 && c <= 57) || (c >= 65 && c <= 90) || (c >= 97 && c <= 122)) out += s[i];
      else out += '\\' + s[i];
    }
    return out;
  },
  registerProperty() {},
  px: (n) => n + 'px',
};

function getComputedStyle(el, pseudo) {
  if (!el || el[ID] === undefined) throw new TypeError("parameter 1 is not of type 'Element'");
  return computedStyle(el, pseudo);
}

class Window extends EventTarget {}
expose(Window);
