// Huginn DOM prelude. Runs once per page, before any page script.
//
// The DOM lives in Rust (huginn-dom). These files (src/js/*.js, concatenated in
// name order) build the web platform objects (window, document, Node,
// Element, events, timers, ...) on top of a small set of native operations
// `N` (see `install_ops` in v8engine.rs). Nodes are named by their raw
// NodeId (a number, 0 = null). Each node gets one wrapper object, cached for
// the life of the page, so `===` identity holds.
//
// The completion value is a function (N, global) -> hooks; the engine calls
// the hooks for event dispatch and timers (see 90-hooks.js).
(function (N, G) {
'use strict';

// ---------------------------------------------------------------- basics --

const ID = Symbol('huginn.id');
const LISTENERS = Symbol('huginn.listeners');
const EV = Symbol('huginn.event');
const CE = Symbol('huginn.ce');
const CACHE = Symbol('huginn.cache');
const OWNER = Symbol('huginn.owner');
const HTML_NS = 'http://www.w3.org/1999/xhtml';
const SVG_NS = 'http://www.w3.org/2000/svg';
const MATHML_NS = 'http://www.w3.org/1998/Math/MathML';
const XLINK_NS = 'http://www.w3.org/1999/xlink';
const DOC_ID = 1;
const wrappers = new Map();
const hasOwn = Object.prototype.hasOwnProperty;
const defProp = Object.defineProperty;

// Per-object lazily created values (style declarations, token lists, ...).
function cached(obj, key, make) {
  let c = obj[CACHE];
  if (!c) { c = Object.create(null); defProp(obj, CACHE, { value: c }); }
  return key in c ? c[key] : (c[key] = make());
}

function tag(C, name) {
  defProp(C.prototype, Symbol.toStringTag, { value: name || C.name, configurable: true });
  return C;
}

// Every exposed interface, for `idlEnumerable` at the end of the prelude.
const interfaces = [];

// Make an interface object visible on the global object.
function expose(C, name) {
  name = name || C.name;
  tag(C, name);
  defProp(G, name, { value: C, writable: true, configurable: true });
  interfaces.push(C);
  return C;
}

// WebIDL makes the attributes, operations and constants of an interface
// enumerable, on its prototype and on the interface object. Class syntax
// makes them non-enumerable. Some libraries copy an API with `for...in`
// (Zone.js wraps MutationObserver and IntersectionObserver that way), so
// they would miss every method. Names that start with `_` are Huginn
// internals and stay hidden.
function idlEnumerable(C) {
  for (const [obj, skip] of [[C.prototype, ['constructor']], [C, ['length', 'name', 'prototype']]]) {
    for (const k of Object.getOwnPropertyNames(obj)) {
      if (k[0] === '_' || skip.includes(k)) continue;
      const d = Object.getOwnPropertyDescriptor(obj, k);
      if (!d.enumerable && d.configurable) { d.enumerable = true; defProp(obj, k, d); }
    }
  }
}

function str(v) { return v == null ? '' : String(v); }
function nullableStr(v) { return v === null ? '' : String(v); }

function fmt(v, depth) {
  if (typeof v === 'string') return v;
  if (v instanceof Error) return v.stack || String(v);
  if (typeof v === 'function') return 'function ' + (v.name || '(anonymous)');
  if (v && typeof v === 'object') {
    try {
      if (v[ID] !== undefined) return '<' + String(v.nodeName).toLowerCase() + '>';
      if (depth > 1) return Array.isArray(v) ? '[…]' : '{…}';
      if (Array.isArray(v)) return '[' + v.slice(0, 20).map(x => fmt(x, depth + 1)).join(', ') + ']';
      const keys = Object.keys(v).slice(0, 20);
      return '{' + keys.map(k => k + ': ' + fmt(v[k], depth + 1)).join(', ') + '}';
    } catch (e) {
      return Object.prototype.toString.call(v);
    }
  }
  return String(v);
}

function format(args) {
  const out = [];
  let i = 0;
  if (typeof args[0] === 'string' && args[0].indexOf('%') >= 0 && args.length > 1) {
    i = 1;
    out.push(args[0].replace(/%[sdifoOc%]/g, m => {
      if (m === '%%') return '%';
      if (i >= args.length) return m;
      const a = args[i++];
      if (m === '%c') return '';
      if (m === '%d' || m === '%i') return String(parseInt(a, 10));
      if (m === '%f') return String(Number(a));
      return fmt(a, 0);
    }));
  }
  for (; i < args.length; i++) out.push(fmt(args[i], 0));
  return out.join(' ');
}

let reporting = false;
// An uncaught exception in a callback: log it and fire `error` at window.
function reportError(e) {
  N.log(3, 'Uncaught ' + fmt(e, 0));
  if (reporting) return;
  reporting = true;
  try {
    const ev = new ErrorEvent('error', { message: e && e.message ? String(e.message) : String(e), error: e, cancelable: true });
    dispatch(G, ev);
  } catch (_) {
    // A broken error handler must not take the page down.
  } finally {
    reporting = false;
  }
}

function now() { return N.now(); }

// ------------------------------------------------------------- exceptions --

const DOM_ERR_CODES = {
  IndexSizeError: 1, HierarchyRequestError: 3, WrongDocumentError: 4, InvalidCharacterError: 5,
  NoModificationAllowedError: 7, NotFoundError: 8, NotSupportedError: 9, InvalidStateError: 11,
  SyntaxError: 12, InvalidModificationError: 13, NamespaceError: 14, InvalidAccessError: 15,
  SecurityError: 18, NetworkError: 19, AbortError: 20, QuotaExceededError: 22, TimeoutError: 23,
  DataCloneError: 25,
};
class DOMException extends Error {
  constructor(message, name) {
    super(message === undefined ? '' : String(message));
    defProp(this, 'name', { value: name === undefined ? 'Error' : String(name), configurable: true, writable: true });
  }
  get code() { return DOM_ERR_CODES[this.name] || 0; }
}
expose(DOMException);

function domErr(name, msg) { return new DOMException(msg || name, name); }

// ----------------------------------------------------------------- events --

class Event {
  constructor(type, init) {
    if (arguments.length === 0) throw new TypeError("Failed to construct 'Event': 1 argument required");
    init = init || {};
    defProp(this, EV, { value: {
      type: String(type), bubbles: !!init.bubbles, cancelable: !!init.cancelable, composed: !!init.composed,
      target: null, currentTarget: null, phase: 0, stop: false, stopNow: false, canceled: false,
      passive: false, path: [], time: now(),
    } });
    // Configurable so events fired by the engine can be marked trusted.
    defProp(this, 'isTrusted', { value: false, enumerable: true, configurable: true });
  }
  get type() { return this[EV].type; }
  get bubbles() { return this[EV].bubbles; }
  get cancelable() { return this[EV].cancelable; }
  get composed() { return this[EV].composed; }
  get target() { return this[EV].target; }
  get srcElement() { return this[EV].target; }
  get currentTarget() { return this[EV].currentTarget; }
  get eventPhase() { return this[EV].phase; }
  get defaultPrevented() { return this[EV].canceled; }
  get timeStamp() { return this[EV].time; }
  get returnValue() { return !this[EV].canceled; }
  set returnValue(v) { if (!v) this.preventDefault(); }
  get cancelBubble() { return this[EV].stop; }
  set cancelBubble(v) { if (v) this[EV].stop = true; }
  stopPropagation() { this[EV].stop = true; }
  stopImmediatePropagation() { const s = this[EV]; s.stop = s.stopNow = true; }
  preventDefault() { const s = this[EV]; if (s.cancelable && !s.passive) s.canceled = true; }
  composedPath() { return this[EV].path.slice(); }
  initEvent(type, bubbles, cancelable) {
    const s = this[EV];
    s.type = String(type); s.bubbles = !!bubbles; s.cancelable = !!cancelable;
  }
}
for (const [k, v] of [['NONE', 0], ['CAPTURING_PHASE', 1], ['AT_TARGET', 2], ['BUBBLING_PHASE', 3]]) {
  defProp(Event, k, { value: v, enumerable: true });
  defProp(Event.prototype, k, { value: v, enumerable: true });
}
expose(Event);

// Event subclasses copy the listed init fields (with defaults) onto the event.
function eventClass(name, Parent, fields) {
  const C = class extends Parent {
    constructor(type, init) {
      super(type, init);
      init = init || {};
      for (const k in fields) {
        defProp(this, k, { value: k in init ? init[k] : fields[k], enumerable: true, configurable: true, writable: true });
      }
    }
  };
  defProp(C, 'name', { value: name });
  return expose(C, name);
}
const CustomEvent = eventClass('CustomEvent', Event, { detail: null });
CustomEvent.prototype.initCustomEvent = function (type, bubbles, cancelable, detail) {
  this.initEvent(type, bubbles, cancelable);
  this.detail = detail;
};
const UIEvent = eventClass('UIEvent', Event, { view: null, detail: 0, which: 0 });
const MouseEvent = eventClass('MouseEvent', UIEvent, {
  screenX: 0, screenY: 0, clientX: 0, clientY: 0, pageX: 0, pageY: 0, offsetX: 0, offsetY: 0, x: 0, y: 0,
  movementX: 0, movementY: 0, ctrlKey: false, shiftKey: false, altKey: false, metaKey: false,
  button: 0, buttons: 0, relatedTarget: null,
});
MouseEvent.prototype.getModifierState = function () { return false; };
eventClass('PointerEvent', MouseEvent, {
  pointerId: 1, width: 1, height: 1, pressure: 0, tangentialPressure: 0, tiltX: 0, tiltY: 0, twist: 0,
  pointerType: 'mouse', isPrimary: true,
});
eventClass('WheelEvent', MouseEvent, { deltaX: 0, deltaY: 0, deltaZ: 0, deltaMode: 0 });
eventClass('DragEvent', MouseEvent, { dataTransfer: null });
const KeyboardEvent = eventClass('KeyboardEvent', UIEvent, {
  key: '', code: '', location: 0, ctrlKey: false, shiftKey: false, altKey: false, metaKey: false,
  repeat: false, isComposing: false, charCode: 0, keyCode: 0,
});
KeyboardEvent.prototype.getModifierState = function () { return false; };
eventClass('FocusEvent', UIEvent, { relatedTarget: null });
eventClass('InputEvent', UIEvent, { data: null, inputType: '', isComposing: false, dataTransfer: null });
eventClass('CompositionEvent', UIEvent, { data: '' });
const ErrorEvent = eventClass('ErrorEvent', Event, { message: '', filename: '', lineno: 0, colno: 0, error: null });
eventClass('PromiseRejectionEvent', Event, { promise: null, reason: undefined });
const MessageEvent = eventClass('MessageEvent', Event, { data: null, origin: '', lastEventId: '', source: null, ports: [] });
const HashChangeEvent = eventClass('HashChangeEvent', Event, { oldURL: '', newURL: '' });
const PopStateEvent = eventClass('PopStateEvent', Event, { state: null });
eventClass('StorageEvent', Event, { key: null, oldValue: null, newValue: null, url: '', storageArea: null });
const ProgressEvent = eventClass('ProgressEvent', Event, { lengthComputable: false, loaded: 0, total: 0 });
eventClass('AnimationEvent', Event, { animationName: '', elapsedTime: 0, pseudoElement: '' });
eventClass('TransitionEvent', Event, { propertyName: '', elapsedTime: 0, pseudoElement: '' });
const PageTransitionEvent = eventClass('PageTransitionEvent', Event, { persisted: false });
eventClass('SubmitEvent', Event, { submitter: null });
eventClass('ClipboardEvent', Event, { clipboardData: null });
eventClass('SecurityPolicyViolationEvent', Event, { blockedURI: '', violatedDirective: '' });

class EventTarget {
  addEventListener(type, callback, options) {
    if (callback == null) return;
    let capture = false, once = false, passive = false, signal = null;
    if (typeof options === 'boolean') capture = options;
    else if (options) {
      capture = !!options.capture; once = !!options.once; passive = !!options.passive; signal = options.signal || null;
    }
    if (signal && signal.aborted) return;
    let map = this[LISTENERS];
    if (!map) { map = new Map(); defProp(this, LISTENERS, { value: map }); }
    type = String(type);
    let list = map.get(type);
    if (!list) map.set(type, list = []);
    for (const l of list) if (l.callback === callback && l.capture === capture) return;
    list.push({ callback, capture, once, passive, removed: false });
    if (signal) signal.addEventListener('abort', () => this.removeEventListener(type, callback, capture));
  }
  removeEventListener(type, callback, options) {
    const capture = typeof options === 'boolean' ? options : !!(options && options.capture);
    const map = this[LISTENERS];
    const list = map && map.get(String(type));
    if (!list) return;
    for (let i = 0; i < list.length; i++) {
      if (list[i].callback === callback && list[i].capture === capture) {
        list[i].removed = true;
        list.splice(i, 1);
        return;
      }
    }
  }
  dispatchEvent(event) {
    if (!(event instanceof Event)) throw new TypeError("parameter 1 is not of type 'Event'");
    return dispatch(this, event);
  }
}
expose(EventTarget);

// Target, then its ancestors, then window (except for `load`, which never
// reaches window from a node).
function eventPath(target, type) {
  const path = [target];
  if (target && target[ID] !== undefined) {
    let p = N.parent(target[ID]);
    while (p) {
      path.push(wrap(p));
      p = N.parent(p);
    }
    if (path[path.length - 1] === document && type !== 'load') path.push(G);
  }
  return path;
}

function dispatch(target, event) {
  const s = event[EV];
  s.target = target;
  s.stop = s.stopNow = false;
  const path = s.path = eventPath(target, s.type);
  for (let i = path.length - 1; i > 0 && !s.stop; i--) invoke(path[i], event, s, 1);
  if (!s.stop) invoke(path[0], event, s, 2);
  if (s.bubbles) for (let i = 1; i < path.length && !s.stop; i++) invoke(path[i], event, s, 3);
  s.phase = 0;
  s.currentTarget = null;
  return !s.canceled;
}

// Inline handlers from attributes (`onload="..."`), compiled on first use.
// Window handlers come from <body> attributes.
function attrHandler(obj, type) {
  const id = obj === G ? N.body() : obj[ID];
  if (!id || N.type(id) !== 1) return null;
  const code = N.attr(id, 'on' + type);
  if (code === null) return null;
  return cached(obj === G ? document : obj, 'on:' + type + ':' + code, () => {
    try { return new Function('event', code); } catch (e) { reportError(e); return null; }
  });
}

function invoke(obj, event, s, phase) {
  s.currentTarget = obj;
  s.phase = phase;
  const map = obj[LISTENERS];
  const list = map && map.get(s.type);
  if (list && list.length) {
    for (const l of list.slice()) {
      if (l.removed) continue;
      if ((phase === 1 && !l.capture) || (phase === 3 && l.capture)) continue;
      if (l.once) obj.removeEventListener(s.type, l.callback, l.capture);
      s.passive = l.passive;
      try {
        const cb = l.callback;
        if (typeof cb === 'function') cb.call(obj, event);
        else if (cb && typeof cb.handleEvent === 'function') cb.handleEvent(event);
      } catch (e) {
        reportError(e);
      }
      s.passive = false;
      if (s.stopNow) return;
    }
  }
  if (phase !== 1) {
    let h = obj['on' + s.type];
    if (h === undefined || h === null) h = attrHandler(obj, s.type);
    if (typeof h === 'function') {
      try {
        const r = h.call(obj, event);
        if (r === false) event.preventDefault();
      } catch (e) {
        reportError(e);
      }
    }
  }
}

// `on*` handler properties, so `'onclick' in el` holds like in a browser.
// No touch handlers: Chrome on a desktop has none, and pages use them to
// detect phones.
const HANDLER_NAMES = ('abort blur cancel change click close contextmenu dblclick drag dragend dragenter ' +
  'dragleave dragover dragstart drop error focus focusin focusout input invalid keydown keypress keyup load ' +
  'mousedown mouseenter mouseleave mousemove mouseout mouseover mouseup paste copy cut pointerdown pointerup ' +
  'pointermove pointerover pointerout pointerenter pointerleave pointercancel reset resize scroll select submit ' +
  'toggle wheel animationstart animationend animationiteration transitionend transitionstart ' +
  'transitionrun transitioncancel beforeinput selectionchange').split(' ');
function addHandlerProps(proto, extra) {
  for (const n of HANDLER_NAMES.concat(extra || [])) {
    if (!hasOwn.call(proto, 'on' + n)) defProp(proto, 'on' + n, { value: null, writable: true, configurable: true, enumerable: true });
  }
}

class AbortSignal extends EventTarget {
  constructor() { super(); this.aborted = false; this.reason = undefined; this.onabort = null; }
  throwIfAborted() { if (this.aborted) throw this.reason; }
  static abort(reason) { const c = new AbortController(); c.abort(reason); return c.signal; }
  static timeout(ms) { const c = new AbortController(); setTimeout(() => c.abort(domErr('TimeoutError')), ms); return c.signal; }
  static any(signals) {
    const c = new AbortController();
    for (const s of signals) {
      if (s.aborted) { c.abort(s.reason); break; }
      s.addEventListener('abort', () => c.abort(s.reason));
    }
    return c.signal;
  }
}
expose(AbortSignal);
class AbortController {
  constructor() { this.signal = new AbortSignal(); }
  abort(reason) {
    const s = this.signal;
    if (s.aborted) return;
    s.aborted = true;
    s.reason = reason === undefined ? domErr('AbortError', 'signal is aborted without reason') : reason;
    dispatch(s, new Event('abort'));
  }
}
expose(AbortController);
