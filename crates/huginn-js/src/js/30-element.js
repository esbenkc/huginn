// ------------------------------------------------------------ attributes --

class Attr extends Node {}
function makeAttr(owner, name, value) {
  const a = Object.create(Attr.prototype);
  const colon = name.indexOf(':');
  defProp(a, 'name', { value: name, enumerable: true });
  defProp(a, 'localName', { value: colon >= 0 ? name.slice(colon + 1) : name, enumerable: true });
  defProp(a, 'nodeName', { value: name });
  defProp(a, 'nodeType', { value: 2 });
  defProp(a, 'namespaceURI', { value: null });
  defProp(a, 'prefix', { value: null });
  defProp(a, 'ownerElement', { value: owner });
  defProp(a, 'specified', { value: true });
  defProp(a, 'value', {
    get() { return owner ? nullableStr(owner.getAttribute(name)) : value; },
    set(v) { if (owner) owner.setAttribute(name, v); else value = String(v); },
    enumerable: true,
  });
  defProp(a, 'nodeValue', { get() { return a.value; }, set(v) { a.value = v; } });
  defProp(a, 'textContent', { get() { return a.value; }, set(v) { a.value = v; } });
  return a;
}
expose(Attr);
class NamedNodeMap {
  item(i) { return this[i] || null; }
  getNamedItem(name) {
    name = String(name);
    for (let i = 0; i < this.length; i++) if (this[i].name === name || this[i].name === name.toLowerCase()) return this[i];
    return null;
  }
  getNamedItemNS(ns, name) { return this.getNamedItem(name); }
  setNamedItem(attr) { this[OWNER].setAttribute(attr.name, attr.value); return null; }
  removeNamedItem(name) { const a = this.getNamedItem(name); this[OWNER].removeAttribute(name); return a; }
}
NamedNodeMap.prototype[Symbol.iterator] = Array.prototype.values;
expose(NamedNodeMap);

class DOMTokenList {
  get value() { return nullableStr(N.attr(this[OWNER][ID], this._attr)); }
  set value(v) { this[OWNER].setAttribute(this._attr, String(v)); }
  _tokens() {
    const v = N.attr(this[OWNER][ID], this._attr);
    if (!v) return [];
    const out = [];
    for (const t of v.split(/[ \t\n\f\r]+/)) if (t && out.indexOf(t) < 0) out.push(t);
    return out;
  }
  _set(tokens) { this[OWNER].setAttribute(this._attr, tokens.join(' ')); }
  get length() { return this._tokens().length; }
  item(i) { const t = this._tokens(); return i < t.length ? t[i] : null; }
  contains(t) { return this._tokens().indexOf(String(t)) >= 0; }
  add(...ts) {
    const cur = this._tokens();
    let changed = false;
    for (let t of ts) {
      t = String(t);
      if (t === '') throw domErr('SyntaxError', 'The token provided must not be empty.');
      if (/[ \t\n\f\r]/.test(t)) throw domErr('InvalidCharacterError', 'The token provided contains HTML space characters.');
      if (cur.indexOf(t) < 0) { cur.push(t); changed = true; }
    }
    if (changed) this._set(cur);
  }
  remove(...ts) {
    const drop = ts.map(String);
    const cur = this._tokens();
    const next = cur.filter(t => drop.indexOf(t) < 0);
    if (next.length !== cur.length) this._set(next);
  }
  toggle(t, force) {
    t = String(t);
    const has = this.contains(t);
    if (has && force !== true) { this.remove(t); return false; }
    if (!has && force !== false) { this.add(t); return true; }
    return has;
  }
  replace(a, b) {
    const cur = this._tokens();
    const i = cur.indexOf(String(a));
    if (i < 0) return false;
    cur[i] = String(b);
    this._set(cur.filter((t, j) => cur.indexOf(t) === j));
    return true;
  }
  supports() { return true; }
  forEach(cb, thisArg) { this._tokens().forEach((t, i) => cb.call(thisArg, t, i, this)); }
  entries() { return this._tokens().entries(); }
  keys() { return this._tokens().keys(); }
  values() { return this._tokens().values(); }
  [Symbol.iterator]() { return this._tokens().values(); }
  toString() { return this.value; }
}
expose(DOMTokenList);
function tokenList(el, attr) {
  return cached(el, 'tokens:' + attr, () => {
    const list = Object.create(DOMTokenList.prototype);
    defProp(list, OWNER, { value: el });
    defProp(list, '_attr', { value: attr });
    // Index access (`classList[0]`) needs a proxy; everything else is plain.
    return new Proxy(list, {
      get(t, p) {
        if (typeof p === 'string' && p.length && p.charCodeAt(0) >= 48 && p.charCodeAt(0) <= 57) {
          const toks = t._tokens();
          const i = Number(p);
          return Number.isInteger(i) && i < toks.length ? toks[i] : undefined;
        }
        return Reflect.get(t, p, t);
      },
      set(t, p, v) { return Reflect.set(t, p, v, t); },
    });
  });
}

class DOMStringMap {}
expose(DOMStringMap);
function camelToData(p) { return 'data-' + p.replace(/[A-Z]/g, c => '-' + c.toLowerCase()); }
function dataToCamel(n) { return n.slice(5).replace(/-([a-z])/g, (m, c) => c.toUpperCase()); }
function dataset(el) {
  return cached(el, 'dataset', () => new Proxy(Object.create(DOMStringMap.prototype), {
    get(t, p) {
      if (typeof p !== 'string') return t[p];
      const v = N.attr(el[ID], camelToData(p));
      return v === null ? undefined : v;
    },
    set(t, p, v) { if (typeof p === 'string') el.setAttribute(camelToData(p), String(v)); return true; },
    has(t, p) { return typeof p === 'string' && N.attr(el[ID], camelToData(p)) !== null; },
    deleteProperty(t, p) { if (typeof p === 'string') el.removeAttribute(camelToData(p)); return true; },
    ownKeys() {
      const a = N.attrs(el[ID]);
      const keys = [];
      for (let i = 0; i < a.length; i += 2) if (a[i].startsWith('data-')) keys.push(dataToCamel(a[i]));
      return keys;
    },
    getOwnPropertyDescriptor(t, p) {
      if (typeof p !== 'string') return undefined;
      const v = N.attr(el[ID], camelToData(p));
      return v === null ? undefined : { value: v, writable: true, enumerable: true, configurable: true };
    },
  }));
}

// ----------------------------------------------------------- inline style --
// `el.style` reads and writes the `style` attribute, so the style engine
// re-cascades it like any other attribute change.

function cssName(p) {
  if (p === 'cssFloat') return 'float';
  if (p.startsWith('--')) return p;
  let n = p.replace(/[A-Z]/g, c => '-' + c.toLowerCase());
  if (/^(webkit|moz|ms|o)-/.test(n)) n = '-' + n;
  return n;
}

// Split a declaration block into [name, value, important] triples.
function parseDecls(text) {
  const out = [];
  if (!text) return out;
  let depth = 0, quote = '', start = 0;
  const push = (s) => {
    const i = s.indexOf(':');
    if (i <= 0) return;
    const name = s.slice(0, i).trim();
    let value = s.slice(i + 1).trim();
    let important = false;
    const m = /!\s*important\s*$/i.exec(value);
    if (m) { important = true; value = value.slice(0, m.index).trim(); }
    if (name) out.push([name.startsWith('--') ? name : name.toLowerCase(), value, important]);
  };
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (quote) { if (c === '\\') i++; else if (c === quote) quote = ''; continue; }
    if (c === '"' || c === "'") quote = c;
    else if (c === '(') depth++;
    else if (c === ')') depth = Math.max(0, depth - 1);
    else if (c === ';' && depth === 0) { push(text.slice(start, i)); start = i + 1; }
  }
  push(text.slice(start));
  return out;
}
function serializeDecls(decls) {
  return decls.map(d => d[0] + ': ' + d[1] + (d[2] ? ' !important' : '') + ';').join(' ');
}

class CSSStyleDeclaration {
  get cssText() { return serializeDecls(this._decls()); }
  set cssText(v) { this._write(parseDecls(str(v))); }
  get length() { return this._decls().length; }
  item(i) { const d = this._decls(); return i < d.length ? d[i][0] : ''; }
  getPropertyValue(name) {
    name = String(name);
    if (!name.startsWith('--')) name = name.toLowerCase();
    const d = this._decls();
    for (let i = d.length - 1; i >= 0; i--) if (d[i][0] === name) return d[i][1];
    return '';
  }
  getPropertyPriority(name) {
    const d = this._decls();
    for (let i = d.length - 1; i >= 0; i--) if (d[i][0] === name) return d[i][2] ? 'important' : '';
    return '';
  }
  setProperty(name, value, priority) {
    name = String(name);
    if (!name.startsWith('--')) name = name.toLowerCase();
    value = str(value).trim();
    if (value === '') { this.removeProperty(name); return; }
    const d = this._decls().filter(x => x[0] !== name);
    d.push([name, value, String(priority || '').toLowerCase() === 'important']);
    this._write(d);
  }
  removeProperty(name) {
    name = String(name);
    const d = this._decls();
    const old = this.getPropertyValue(name);
    const next = d.filter(x => x[0] !== name);
    if (next.length !== d.length) this._write(next);
    return old;
  }
  get parentRule() { return null; }
}
expose(CSSStyleDeclaration);

const STYLE_OWN = new Set(['cssText', 'length', 'item', 'getPropertyValue', 'getPropertyPriority',
  'setProperty', 'removeProperty', 'parentRule', '_decls', '_write', 'constructor']);
function styleProxy(target) {
  return new Proxy(target, {
    get(t, p) {
      if (typeof p !== 'string') return t[p];
      if (STYLE_OWN.has(p)) { const v = t[p]; return typeof v === 'function' ? v.bind(t) : v; }
      if (p.length && p.charCodeAt(0) >= 48 && p.charCodeAt(0) <= 57) return t.item(Number(p));
      if (p === 'toJSON' || p === 'then') return undefined;
      return t.getPropertyValue(cssName(p));
    },
    set(t, p, v) {
      if (typeof p !== 'string') { t[p] = v; return true; }
      if (p === 'cssText') { t.cssText = v; return true; }
      t.setProperty(cssName(p), v);
      return true;
    },
    // Feature tests (`'transform' in el.style`) see Chrome's answers:
    // standard and -webkit- names yes, other vendor prefixes no.
    has(t, p) {
      if (typeof p !== 'string') return p in t;
      return !/^(Moz|moz|ms|O[A-Z])/.test(p);
    },
  });
}

function inlineStyle(el) {
  return cached(el, 'style', () => {
    const t = Object.create(CSSStyleDeclaration.prototype);
    let lastText = null, lastDecls = [];
    t._decls = () => {
      const text = N.attr(el[ID], 'style');
      if (text !== lastText) { lastText = text; lastDecls = parseDecls(text || ''); }
      return lastDecls.slice();
    };
    t._write = (decls) => {
      const text = serializeDecls(decls);
      if (text === '' && N.attr(el[ID], 'style') === null) return;
      N.setAttr(el[ID], 'style', text);
    };
    return styleProxy(t);
  });
}

function computedStyle(el, pseudo) {
  const t = Object.create(CSSStyleDeclaration.prototype);
  const id = el && el[ID];
  // Pseudo-element styles are not exposed; report them as empty.
  const skip = !id || N.type(id) !== 1 || (pseudo !== undefined && pseudo !== null && pseudo !== '');
  t.getPropertyValue = (name) => {
    if (skip) return '';
    name = String(name);
    return N.cstyle(id, name.startsWith('--') ? name : name.toLowerCase());
  };
  t._decls = () => [];
  const ro = () => { throw domErr('NoModificationAllowedError', 'computed style is read-only'); };
  t._write = ro;
  t.setProperty = ro;
  t.removeProperty = ro;
  return styleProxy(t);
}

// --------------------------------------------------------------- elements --

class Element extends Node {
  get tagName() {
    const id = this[ID];
    const n = N.name(id);
    return N.isHtml(id) ? n.toUpperCase() : n;
  }
  get localName() { return N.name(this[ID]); }
  get namespaceURI() { return N.ns(this[ID]) || null; }
  get prefix() { return null; }
  get id() { return nullableStr(N.attr(this[ID], 'id')); }
  set id(v) { this.setAttribute('id', v); }
  get className() { return nullableStr(N.attr(this[ID], 'class')); }
  set className(v) { this.setAttribute('class', v); }
  get classList() { return tokenList(this, 'class'); }
  set classList(v) { this.setAttribute('class', v); }
  get part() { return tokenList(this, 'part'); }
  get slot() { return nullableStr(N.attr(this[ID], 'slot')); }
  set slot(v) { this.setAttribute('slot', v); }
  get attributes() {
    const a = N.attrs(this[ID]);
    const map = Object.create(NamedNodeMap.prototype);
    defProp(map, OWNER, { value: this });
    let n = 0;
    for (let i = 0; i < a.length; i += 2) {
      const attr = makeAttr(this, a[i], a[i + 1]);
      map[n++] = attr;
      if (!(a[i] in map)) defProp(map, a[i], { value: attr, configurable: true });
    }
    defProp(map, 'length', { value: n });
    return map;
  }
  getAttribute(name) { return N.attr(this[ID], String(name)); }
  getAttributeNS(ns, name) { return N.attr(this[ID], String(name)); }
  getAttributeNode(name) {
    const v = this.getAttribute(name);
    return v === null ? null : makeAttr(this, String(name).toLowerCase(), v);
  }
  getAttributeNames() { const a = N.attrs(this[ID]); const out = []; for (let i = 0; i < a.length; i += 2) out.push(a[i]); return out; }
  hasAttribute(name) { return N.attr(this[ID], String(name)) !== null; }
  hasAttributeNS(ns, name) { return this.hasAttribute(name); }
  hasAttributes() { return N.attrs(this[ID]).length > 0; }
  setAttribute(name, value) {
    name = String(name);
    value = String(value);
    const id = this[ID];
    const watch = this[CE] || mutationObservers.length;
    if (watch && N.isHtml(id)) name = name.toLowerCase();
    const old = watch ? N.attr(id, name) : null;
    if (N.setAttr(id, name, value) === 1) throw domErr('InvalidCharacterError', "'" + name + "' is not a valid attribute name.");
    if (watch) attributeWritten(this, name, old, value);
    if ((name.length === 3 || name.length === 6) && this instanceof E.HTMLImageElement && /^(src|srcset)$/i.test(name)) imageLoaded(this);
  }
  setAttributeNS(ns, name, value) {
    name = String(name);
    // Stored with its prefix, so `xlink:href` serializes back the same way.
    if (ns === XLINK_NS && name.indexOf(':') < 0) name = 'xlink:' + name;
    this.setAttribute(name, value);
  }
  setAttributeNode(attr) { this.setAttribute(attr.name, attr.value); return null; }
  removeAttribute(name) {
    name = String(name);
    const id = this[ID];
    const watch = this[CE] || mutationObservers.length;
    if (watch && N.isHtml(id)) name = name.toLowerCase();
    const old = watch ? N.attr(id, name) : null;
    N.delAttr(id, name);
    if (watch && old !== null) attributeWritten(this, name, old, null);
  }
  removeAttributeNS(ns, name) { this.removeAttribute(name); }
  toggleAttribute(name, force) {
    const has = this.hasAttribute(name);
    if (has && force !== true) { this.removeAttribute(name); return false; }
    if (!has && force !== false) { this.setAttribute(name, ''); return true; }
    return has;
  }
  get innerHTML() { return N.html(this[ID]); }
  set innerHTML(v) { setInnerHTML(this, v); }
  get outerHTML() { return N.outer(this[ID]); }
  set outerHTML(v) {
    const p = this.parentNode;
    if (!p) return;
    const frag = parseHTMLFragment(p.nodeType === 1 ? p : document.body, str(v));
    insertNode(p, frag, this);
    removeNode(this);
  }
  insertAdjacentHTML(pos, html) {
    pos = String(pos).toLowerCase();
    const ctx = pos === 'beforebegin' || pos === 'afterend' ? this.parentNode : this;
    if (!ctx) return;
    adjacent(this, pos, parseHTMLFragment(ctx.nodeType === 1 ? ctx : document.body, str(html)));
  }
  insertAdjacentElement(pos, el) { return adjacent(this, String(pos).toLowerCase(), el); }
  insertAdjacentText(pos, text) { adjacent(this, String(pos).toLowerCase(), document.createTextNode(str(text))); }
  matches(sel) { return checkSelector(N.matches(this[ID], String(sel))); }
  webkitMatchesSelector(sel) { return this.matches(sel); }
  msMatchesSelector(sel) { return this.matches(sel); }
  closest(sel) { return wrap(checkSelector(N.closest(this[ID], String(sel)))); }
  getBoundingClientRect() {
    const g = N.geom(this[ID]);
    return g ? new DOMRect(g[0], g[1], g[2], g[3]) : new DOMRect(0, 0, 0, 0);
  }
  getClientRects() {
    const g = N.geom(this[ID]);
    return g ? [new DOMRect(g[0], g[1], g[2], g[3])] : [];
  }
  get clientWidth() {
    const id = this[ID];
    if (id === N.docEl()) return N.viewport()[0];
    const g = N.geom(id);
    return g ? Math.max(0, Math.round(g[2] - g[4] - g[6])) : 0;
  }
  get clientHeight() {
    const id = this[ID];
    if (id === N.docEl()) return N.viewport()[1];
    const g = N.geom(id);
    return g ? Math.max(0, Math.round(g[3] - g[5] - g[7])) : 0;
  }
  get clientTop() { const g = N.geom(this[ID]); return g ? Math.round(g[5]) : 0; }
  get clientLeft() { const g = N.geom(this[ID]); return g ? Math.round(g[4]) : 0; }
  get scrollWidth() {
    const id = this[ID];
    const g = N.geom(id);
    if (id === N.docEl()) return Math.max(N.viewport()[0], g ? Math.round(g[2]) : 0);
    return g ? Math.round(g[8]) : 0;
  }
  get scrollHeight() {
    const id = this[ID];
    const g = N.geom(id);
    if (id === N.docEl()) return Math.max(N.viewport()[1], g ? Math.round(g[3]) : 0);
    return g ? Math.round(g[9]) : 0;
  }
  get scrollTop() { return elementScrolls(this[ID]) ? N.scrollPos(this[ID])[1] : 0; }
  set scrollTop(v) { scrollElement(this, undefined, v); }
  get scrollLeft() { return elementScrolls(this[ID]) ? N.scrollPos(this[ID])[0] : 0; }
  set scrollLeft(v) { scrollElement(this, v, undefined); }
  scroll(a, b) { scrollElementArgs(this, a, b, false); }
  scrollTo(a, b) { scrollElementArgs(this, a, b, false); }
  scrollBy(a, b) { scrollElementArgs(this, a, b, true); }
  scrollIntoView() {}
  scrollIntoViewIfNeeded() {}
  get shadowRoot() {
    const r = wrap(N.shadow(this[ID]));
    return r && r.mode === 'open' ? r : null;
  }
  attachShadow(init) {
    const old = wrap(N.shadow(this[ID]));
    if (old && old[SHADOW_MODE]) throw domErr('NotSupportedError', 'Shadow root cannot be created on a host which already hosts a shadow tree.');
    const root = wrap(N.attachShadow(this[ID]));
    if (!root) throw domErr('NotSupportedError', 'This element does not support attachShadow');
    root[SHADOW_MODE] = init && init.mode === 'closed' ? 'closed' : 'open';
    return root;
  }
  get assignedSlot() { return wrap(N.assignedSlot(this[ID])); }
  animate() { return makeAnimation(); }
  getAnimations() { return []; }
  requestFullscreen() { return Promise.reject(domErr('NotSupportedError')); }
  requestPointerLock() {}
  setPointerCapture() {}
  releasePointerCapture() {}
  hasPointerCapture() { return false; }
  checkVisibility() { return N.geom(this[ID]) !== null; }
}
mixin(Element, ParentNode);
mixin(Element, ChildNode);
expose(Element);

function adjacent(el, pos, node) {
  switch (pos) {
    case 'beforebegin': { const p = el.parentNode; if (!p) return null; insertNode(p, node, el); return node; }
    case 'afterbegin': insertNode(el, node, el.firstChild); return node;
    case 'beforeend': insertNode(el, node, null); return node;
    case 'afterend': { const p = el.parentNode; if (!p) return null; insertNode(p, node, el.nextSibling); return node; }
  }
  throw domErr('SyntaxError', "The value provided ('" + pos + "') is not one of 'beforeBegin', 'afterBegin', 'beforeEnd', or 'afterEnd'.");
}

function parseHTMLFragment(ctx, html) {
  const frag = document.createDocumentFragment();
  for (const id of N.frag(ctx[ID], html)) N.insert(frag[ID], id, 0);
  return frag;
}

// Images are decoded by the shell after scripts run. Scripts that wait for
// `load` (lazy loaders, carousels) get it in the next task, as if the
// image were cached.
function imageLoaded(img) {
  if (img[CACHE] && img[CACHE].loadQueued) return;
  cached(img, 'loadQueued', () => true);
  queueTask(() => {
    img[CACHE].loadQueued = false;
    dispatch(img, new Event('load'));
  });
}

// Custom element callback and MutationObserver record for an attribute write.
function attributeWritten(el, name, old, value) {
  if (el[CE]) attributeChanged(el, name, old, value);
  if (mutationObservers.length) notifyMutation('attributes', el[ID], { attributeName: name, oldValue: old });
}

function setInnerHTML(el, v) {
  const id = el[ID];
  replaceChildrenNative(id, () => N.setHtml(id, str(v)));
}

function makeAnimation() {
  const a = new EventTarget();
  a.finished = Promise.resolve(a);
  a.ready = Promise.resolve(a);
  a.playState = 'finished';
  a.onfinish = null;
  a.oncancel = null;
  a.currentTime = 0;
  a.effect = null;
  for (const m of ['cancel', 'finish', 'play', 'pause', 'reverse', 'updatePlaybackRate', 'commitStyles', 'persist']) a[m] = function () {};
  return a;
}

// Attribute reflection: [property, attribute] pairs, or names whose
// attribute is the lowercase property name.
function pairs(props) { return props.map(p => Array.isArray(p) ? p : [p, p.toLowerCase()]); }
function reflect(proto, props) {
  for (const [prop, attr] of pairs(props)) {
    defProp(proto, prop, {
      get() { return nullableStr(N.attr(this[ID], attr)); },
      set(v) { this.setAttribute(attr, v); },
      configurable: true,
    });
  }
}
function reflectBool(proto, props) {
  for (const [prop, attr] of pairs(props)) {
    defProp(proto, prop, {
      get() { return N.attr(this[ID], attr) !== null; },
      set(v) { if (v) this.setAttribute(attr, ''); else this.removeAttribute(attr); },
      configurable: true,
    });
  }
}
function reflectUrl(proto, props) {
  for (const [prop, attr] of pairs(props)) {
    defProp(proto, prop, {
      get() {
        const v = N.attr(this[ID], attr);
        if (v === null) return '';
        const u = N.url(v, '');
        return u ? u[0] : v;
      },
      set(v) { this.setAttribute(attr, v); },
      configurable: true,
    });
  }
}
// [property, attribute, default (value or function of the element)]
function reflectInt(proto, props) {
  for (const [prop, attr, def] of props) {
    defProp(proto, prop, {
      get() {
        const v = parseInt(N.attr(this[ID], attr), 10);
        return Number.isNaN(v) ? (typeof def === 'function' ? def.call(this) : def) : v;
      },
      set(v) { this.setAttribute(attr, String(Math.trunc(Number(v)) || 0)); },
      configurable: true,
    });
  }
}

class HTMLElement extends Element {
  // Only custom element constructors may be called (`super()` from a class
  // passed to customElements.define). During an upgrade the element exists
  // already and is returned as `this`.
  constructor() {
    if (upgrading.length) return upgrading[upgrading.length - 1];
    const nt = new.target;
    const def = nt && ceByCtor.get(nt);
    if (!def) throw new TypeError('Illegal constructor');
    const id = N.create(def.name, HTML_NS);
    const w = Object.create(nt.prototype);
    w[ID] = id;
    wrappers.set(id, w);
    defProp(w, CE, { value: def, writable: true });
    return w;
  }
  get style() { return inlineStyle(this); }
  set style(v) { inlineStyle(this).cssText = v; }
  get dataset() { return dataset(this); }
  get innerText() { return N.text(this[ID]); }
  set innerText(v) { this.textContent = v; }
  get outerText() { return N.text(this[ID]); }
  get isContentEditable() { return false; }
  get contentEditable() { return N.attr(this[ID], 'contenteditable') || 'inherit'; }
  set contentEditable(v) { this.setAttribute('contenteditable', v); }
  get offsetParent() { return offsetParentOf(this); }
  get offsetTop() { return offsetPos(this, 1); }
  get offsetLeft() { return offsetPos(this, 0); }
  get offsetWidth() { const g = N.geom(this[ID]); return g ? Math.round(g[2]) : 0; }
  get offsetHeight() { const g = N.geom(this[ID]); return g ? Math.round(g[3]) : 0; }
  focus() { N.focus(this[ID]); }
  blur() { if (N.focused() === this[ID]) N.focus(0); }
  // Untrusted click plus activation behavior (follow a link, toggle a
  // checkbox, submit a form); see huginn-js activation.rs.
  click() { N.click(this[ID]); }
  attachInternals() {
    return { setFormValue() {}, setValidity() {}, checkValidity: () => true, reportValidity: () => true,
      states: new Set(), form: null, labels: [], shadowRoot: null };
  }
  showPopover() {}
  hidePopover() {}
  togglePopover() { return false; }
}
reflect(HTMLElement.prototype, ['title', 'lang', 'dir', ['accessKey', 'accesskey'], 'translate', 'nonce',
  ['enterKeyHint', 'enterkeyhint'], ['inputMode', 'inputmode'], 'autocapitalize', 'popover']);
reflectBool(HTMLElement.prototype, ['hidden', 'inert', 'autofocus', 'draggable', 'spellcheck']);
reflectInt(HTMLElement.prototype, [['tabIndex', 'tabindex', function () {
  return /^(a|area|button|input|select|textarea|iframe|summary)$/.test(N.name(this[ID])) ? 0 : -1;
}]]);
addHandlerProps(HTMLElement.prototype);
expose(HTMLElement);

function offsetParentOf(el) {
  const body = N.body();
  if (el[ID] === body || !N.geom(el[ID])) return null;
  let p = N.parent(el[ID]);
  while (p && N.type(p) === 1) {
    if (p === body) return wrap(p);
    const pos = N.cstyle(p, 'position');
    if (pos && pos !== 'static') return wrap(p);
    const n = N.name(p);
    if (n === 'td' || n === 'th' || n === 'table') return wrap(p);
    p = N.parent(p);
  }
  return body ? wrap(body) : null;
}
function offsetPos(el, axis) {
  const g = N.geom(el[ID]);
  if (!g) return 0;
  const p = offsetParentOf(el);
  const pg = p && p[ID] !== N.body() ? N.geom(p[ID]) : null;
  const base = pg ? pg[axis] + pg[4 + axis] : 0;
  // Layout positions: scrolling the ancestors up to the offset parent does
  // not change them.
  let scrolled = 0;
  const stop = p ? N.parent(p[ID]) : null;
  for (let a = N.parent(el[ID]); a && a !== stop && N.type(a) === 1; a = N.parent(a)) {
    if (elementScrolls(a)) scrolled += N.scrollPos(a)[axis];
  }
  return Math.round(g[axis] - base + scrolled);
}

// CSSOM View: an element scrolls if its box is a scroll container. The root
// and <body> stand for the viewport, which does not scroll here.
function elementScrolls(id) {
  if (id === N.docEl() || id === N.body()) return false;
  const scrolls = v => v === 'hidden' || v === 'scroll' || v === 'auto';
  return scrolls(N.cstyle(id, 'overflow-x')) || scrolls(N.cstyle(id, 'overflow-y'));
}
// "Scroll an element": the new offset is clamped to the scrolling area.
// `undefined` keeps an axis.
function scrollElement(el, x, y) {
  const id = el[ID];
  if (!elementScrolls(id)) return;
  const g = N.geom(id);
  if (!g) return;
  const cur = N.scrollPos(id);
  const clamp = (v, c, area, client) => {
    if (v === undefined) return c;
    v = +v;
    if (!Number.isFinite(v)) v = 0;
    return Math.max(0, Math.min(v, area - client));
  };
  N.setScrollPos(id, clamp(x, cur[0], g[8], g[2] - g[4] - g[6]), clamp(y, cur[1], g[9], g[3] - g[5] - g[7]));
}
// scroll()/scrollTo()/scrollBy() with (x, y) or ({ left, top }).
function scrollElementArgs(el, a, b, relative) {
  let x, y;
  if (a !== null && typeof a === 'object') {
    x = a.left;
    y = a.top;
  } else if (a !== undefined) {
    x = a;
    y = b;
  }
  if (relative) {
    const cur = N.scrollPos(el[ID]);
    x = x === undefined ? undefined : cur[0] + (+x || 0);
    y = y === undefined ? undefined : cur[1] + (+y || 0);
  }
  scrollElement(el, x, y);
}

// Element interfaces by tag name.
const ELEMENT_CLASSES = Object.create(null);
function elementClass(name, tags, Parent) {
  const C = class extends (Parent || HTMLElement) {};
  defProp(C, 'name', { value: name });
  expose(C, name);
  for (const t of tags) ELEMENT_CLASSES[t] = C;
  return C;
}
const E = Object.create(null);
for (const [name, tags] of [
  ['HTMLAnchorElement', ['a']], ['HTMLAreaElement', ['area']], ['HTMLBaseElement', ['base']],
  ['HTMLBodyElement', ['body']], ['HTMLBRElement', ['br']], ['HTMLButtonElement', ['button']],
  ['HTMLCanvasElement', ['canvas']], ['HTMLDataElement', ['data']], ['HTMLDataListElement', ['datalist']],
  ['HTMLDetailsElement', ['details']], ['HTMLDialogElement', ['dialog']], ['HTMLDivElement', ['div']],
  ['HTMLDListElement', ['dl']], ['HTMLEmbedElement', ['embed']], ['HTMLFieldSetElement', ['fieldset']],
  ['HTMLFormElement', ['form']], ['HTMLHeadingElement', ['h1', 'h2', 'h3', 'h4', 'h5', 'h6']],
  ['HTMLHeadElement', ['head']], ['HTMLHRElement', ['hr']], ['HTMLHtmlElement', ['html']],
  ['HTMLIFrameElement', ['iframe']], ['HTMLImageElement', ['img']], ['HTMLInputElement', ['input']],
  ['HTMLLabelElement', ['label']], ['HTMLLegendElement', ['legend']], ['HTMLLIElement', ['li']],
  ['HTMLLinkElement', ['link']], ['HTMLMapElement', ['map']], ['HTMLMetaElement', ['meta']],
  ['HTMLMeterElement', ['meter']], ['HTMLModElement', ['ins', 'del']], ['HTMLObjectElement', ['object']],
  ['HTMLOListElement', ['ol']], ['HTMLOptGroupElement', ['optgroup']], ['HTMLOptionElement', ['option']],
  ['HTMLOutputElement', ['output']], ['HTMLParagraphElement', ['p']], ['HTMLPictureElement', ['picture']],
  ['HTMLPreElement', ['pre', 'listing', 'xmp']], ['HTMLProgressElement', ['progress']],
  ['HTMLQuoteElement', ['q', 'blockquote']], ['HTMLScriptElement', ['script']],
  ['HTMLSelectElement', ['select']], ['HTMLSlotElement', ['slot']], ['HTMLSourceElement', ['source']],
  ['HTMLSpanElement', ['span']], ['HTMLStyleElement', ['style']], ['HTMLTableElement', ['table']],
  ['HTMLTableCaptionElement', ['caption']], ['HTMLTableCellElement', ['td', 'th']],
  ['HTMLTableColElement', ['col', 'colgroup']], ['HTMLTableSectionElement', ['tbody', 'thead', 'tfoot']],
  ['HTMLTableRowElement', ['tr']], ['HTMLTemplateElement', ['template']], ['HTMLTextAreaElement', ['textarea']],
  ['HTMLTimeElement', ['time']], ['HTMLTitleElement', ['title']], ['HTMLTrackElement', ['track']],
  ['HTMLUListElement', ['ul']], ['HTMLMenuElement', ['menu']], ['HTMLParamElement', ['param']],
  ['HTMLFrameSetElement', ['frameset']], ['HTMLFontElement', ['font']], ['HTMLDirectoryElement', ['dir']],
]) E[name] = elementClass(name, tags);
E.HTMLUnknownElement = elementClass('HTMLUnknownElement', []);
E.HTMLMediaElement = elementClass('HTMLMediaElement', []);
E.HTMLVideoElement = elementClass('HTMLVideoElement', ['video'], E.HTMLMediaElement);
E.HTMLAudioElement = elementClass('HTMLAudioElement', ['audio'], E.HTMLMediaElement);

class SVGElement extends Element {
  get style() { return inlineStyle(this); }
  set style(v) { inlineStyle(this).cssText = v; }
  get dataset() { return dataset(this); }
  get className() {
    const el = this;
    return {
      get baseVal() { return nullableStr(N.attr(el[ID], 'class')); },
      set baseVal(v) { el.setAttribute('class', v); },
      get animVal() { return this.baseVal; },
    };
  }
  set className(v) { this.setAttribute('class', v); }
  get ownerSVGElement() { return this.parentElement ? this.parentElement.closest('svg') : null; }
  get viewportElement() { return this.ownerSVGElement; }
  focus() {}
  blur() {}
  getBBox() { const g = N.geom(this[ID]); return g ? new DOMRect(0, 0, g[2], g[3]) : new DOMRect(0, 0, 0, 0); }
  getCTM() { return new DOMMatrix(); }
  getScreenCTM() { return new DOMMatrix(); }
  getTotalLength() { return 0; }
  getPointAtLength() { return new DOMPoint(0, 0); }
}
reflectInt(SVGElement.prototype, [['tabIndex', 'tabindex', -1]]);
addHandlerProps(SVGElement.prototype);
expose(SVGElement);
class SVGGraphicsElement extends SVGElement {}
expose(SVGGraphicsElement);
class SVGSVGElement extends SVGGraphicsElement {
  createSVGPoint() { return new DOMPoint(0, 0); }
  createSVGMatrix() { return new DOMMatrix(); }
  createSVGRect() { return new DOMRect(); }
  suspendRedraw() { return 0; }
  unsuspendRedraw() {}
  forceRedraw() {}
}
expose(SVGSVGElement);
const SVG_CLASSES = Object.create(null);
for (const [n, t] of [['SVGPathElement', 'path'], ['SVGGElement', 'g'], ['SVGUseElement', 'use'],
  ['SVGRectElement', 'rect'], ['SVGCircleElement', 'circle'], ['SVGTextElement', 'text'],
  ['SVGImageElement', 'image'], ['SVGPolygonElement', 'polygon'], ['SVGLineElement', 'line'],
  ['SVGEllipseElement', 'ellipse'], ['SVGPolylineElement', 'polyline'], ['SVGSymbolElement', 'symbol']]) {
  const C = class extends SVGGraphicsElement {};
  defProp(C, 'name', { value: n });
  expose(C, n);
  SVG_CLASSES[t] = C;
}
class MathMLElement extends Element {
  get style() { return inlineStyle(this); }
}
expose(MathMLElement);

function elementProto(id) {
  const name = N.name(id);
  if (N.isHtml(id)) {
    const C = ELEMENT_CLASSES[name];
    return C ? C.prototype : HTMLElement.prototype;
  }
  const ns = N.ns(id);
  if (ns === SVG_NS) {
    if (name === 'svg') return SVGSVGElement.prototype;
    const C = SVG_CLASSES[name];
    return C ? C.prototype : SVGElement.prototype;
  }
  if (ns === MATHML_NS) return MathMLElement.prototype;
  return Element.prototype;
}
