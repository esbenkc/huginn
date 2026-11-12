// ----------------------------------------------------------- the globals --

const globals = {
  window: G, self: G, frames: G, parent: G, top: G, globalThis: G,
  document, history, navigator, screen, localStorage, sessionStorage, performance, console, crypto, CSS,
  customElements, NodeFilter,
  setTimeout, setInterval, clearTimeout: clearTimer, clearInterval: clearTimer,
  requestAnimationFrame, cancelAnimationFrame: clearTimer, webkitRequestAnimationFrame: requestAnimationFrame,
  webkitCancelAnimationFrame: clearTimer, requestIdleCallback, cancelIdleCallback: clearTimer,
  queueMicrotask, structuredClone, reportError, atob, btoa, fetch, matchMedia, getComputedStyle,
  getSelection: () => selection,
  postMessage(data, origin) {
    queueTask(() => dispatch(G, new MessageEvent('message', { data, origin: urlParts()[10], source: G })));
  },
  alert() {},
  confirm() { return false; },
  prompt() { return null; },
  print() {},
  open() { return null; },
  close() {},
  stop() {},
  focus() {},
  blur() {},
  scroll() {},
  scrollTo() {},
  scrollBy() {},
  moveTo() {},
  moveBy() {},
  resizeTo() {},
  resizeBy() {},
  find() { return false; },
  captureEvents() {},
  releaseEvents() {},
  createImageBitmap() { return Promise.reject(domErr('NotSupportedError', 'createImageBitmap is not supported')); },
  addEventListener: (type, cb, opts) => EventTarget.prototype.addEventListener.call(G, type, cb, opts),
  removeEventListener: (type, cb, opts) => EventTarget.prototype.removeEventListener.call(G, type, cb, opts),
  dispatchEvent: (ev) => EventTarget.prototype.dispatchEvent.call(G, ev),
};
for (const k in globals) defProp(G, k, { value: globals[k], writable: true, configurable: true, enumerable: true });
// `window.location = url` navigates; it must not replace the object.
defProp(G, 'location', { get() { return location; }, set(v) { navigate(v); }, configurable: true, enumerable: true });
// Read-only window state.
const getters = {
  innerWidth: () => N.viewport()[0], innerHeight: () => N.viewport()[1],
  outerWidth: () => N.viewport()[0], outerHeight: () => N.viewport()[1],
  devicePixelRatio: () => 1, scrollX: () => 0, scrollY: () => 0, pageXOffset: () => 0, pageYOffset: () => 0,
  screenX: () => 0, screenY: () => 0, screenLeft: () => 0, screenTop: () => 0,
  closed: () => false, length: () => 0, opener: () => null, frameElement: () => null,
  origin: () => urlParts()[10], isSecureContext: () => /^(https:|http:\/\/(127\.0\.0\.1|localhost|\[::1\])[:/])/.test(curUrl) || curUrl.startsWith('file:'),
  crossOriginIsolated: () => false, event: () => undefined,
  visualViewport: () => cached(document, 'vv', () => Object.assign(new EventTarget(), {
    get width() { return N.viewport()[0]; }, get height() { return N.viewport()[1]; },
    scale: 1, offsetLeft: 0, offsetTop: 0, pageLeft: 0, pageTop: 0, onresize: null, onscroll: null,
  })),
};
for (const k in getters) defProp(G, k, { get: getters[k], set(v) { defProp(G, k, { value: v, writable: true, configurable: true }); }, configurable: true, enumerable: true });
for (const k of ['name', 'status']) defProp(G, k, { value: '', writable: true, configurable: true });
for (const k of ['locationbar', 'menubar', 'personalbar', 'scrollbars', 'statusbar', 'toolbar']) defProp(G, k, { value: { visible: true }, configurable: true });
// Handler properties (`window.onload = ...`).
for (const n of HANDLER_NAMES.concat(['beforeunload', 'hashchange', 'message', 'messageerror', 'offline', 'online',
  'pagehide', 'pageshow', 'popstate', 'storage', 'unload', 'unhandledrejection', 'rejectionhandled', 'languagechange',
  'orientationchange', 'devicemotion', 'deviceorientation'])) {
  if (!(('on' + n) in G)) defProp(G, 'on' + n, { value: null, writable: true, configurable: true, enumerable: true });
}
try { Object.setPrototypeOf(G, Window.prototype); } catch (e) { /* keep Object.prototype */ }
defProp(G, Symbol.toStringTag, { value: 'Window', configurable: true });

// ----------------------------------------------------------- engine hooks --

// Events fired by the engine (DOMContentLoaded, load, script load/error) and
// by the browser (mouse, keyboard, focus, input). `iface` names the event
// class; `init` holds its init members, with element ids for node members.
function hostDispatch(kind, id, type, bubbles, cancelable, iface, init, trusted) {
  const target = kind === 0 ? G : kind === 1 ? document : wrap(id);
  if (!target) return true;
  if (kind === 1 && type === 'DOMContentLoaded') {
    readyState = 'interactive';
    dispatch(document, new Event('readystatechange'));
  } else if (kind === 0 && type === 'load') {
    readyState = 'complete';
    dispatch(document, new Event('readystatechange'));
  }
  for (const k of ['relatedTarget', 'submitter']) if (k in init) init[k] = init[k] ? wrap(init[k]) : null;
  init.bubbles = bubbles;
  init.cancelable = cancelable;
  const C = (iface && hasOwn.call(G, iface) && typeof G[iface] === 'function') ? G[iface] : Event;
  if (C !== Event && C.prototype instanceof UIEvent) init.view = G;
  const ev = new C(type, init);
  if (trusted !== false) defProp(ev, 'isTrusted', { value: true });
  const r = dispatch(target, ev);
  if (kind === 0 && type === 'load') dispatch(G, new PageTransitionEvent('pageshow', { persisted: false }));
  return r;
}

return { dispatch: hostDispatch, runOne, nextDue };
})
