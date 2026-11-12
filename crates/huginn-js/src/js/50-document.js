// ----------------------------------------------------------- stylesheets --

class CSSRule {
  constructor(text) {
    this.cssText = text;
    this.type = /^\s*@media/i.test(text) ? 4 : /^\s*@font-face/i.test(text) ? 5 : /^\s*@keyframes/i.test(text) ? 7 : 1;
  }
  get selectorText() { const i = this.cssText.indexOf('{'); return i >= 0 ? this.cssText.slice(0, i).trim() : ''; }
  get style() {
    const i = this.cssText.indexOf('{');
    const decls = parseDecls(i >= 0 ? this.cssText.slice(i + 1, this.cssText.lastIndexOf('}')) : '');
    const t = Object.create(CSSStyleDeclaration.prototype);
    t._decls = () => decls.slice();
    t._write = () => {};
    return styleProxy(t);
  }
}
for (const [k, v] of [['STYLE_RULE', 1], ['MEDIA_RULE', 4], ['FONT_FACE_RULE', 5], ['KEYFRAMES_RULE', 7]]) defProp(CSSRule, k, { value: v });
expose(CSSRule);
expose(class CSSStyleRule extends CSSRule {}, 'CSSStyleRule');
expose(class CSSMediaRule extends CSSRule {}, 'CSSMediaRule');

function splitRules(text) {
  const out = [];
  let depth = 0, start = 0, quote = '';
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (quote) { if (c === '\\') i++; else if (c === quote) quote = ''; continue; }
    if (c === '/' && text[i + 1] === '*') { const e = text.indexOf('*/', i + 2); i = e < 0 ? text.length : e + 1; continue; }
    if (c === '"' || c === "'") quote = c;
    else if (c === '{') depth++;
    else if (c === '}') { depth--; if (depth === 0) { out.push(text.slice(start, i + 1).trim()); start = i + 1; } }
    else if (c === ';' && depth === 0) { const s = text.slice(start, i + 1).trim(); if (s) out.push(s); start = i + 1; }
  }
  return out.filter(Boolean);
}

// CSSOM. Rules added by script (`insertRule`, CSS-in-JS libraries) are
// written into a <style> element, which the style engine reads like any
// other: the owner <style> itself, or a <style data-huginn-cssom> next to a
// <link> or in <head> for constructed sheets.
class CSSStyleSheet {
  constructor(options) {
    defProp(this, '_owner', { value: null, writable: true });
    defProp(this, '_rules', { value: [], writable: true });
    this.disabled = false;
    this.media = { mediaText: options && options.media ? String(options.media) : '', length: 0 };
  }
  get ownerNode() { return this._owner; }
  get href() { return this._owner && this._owner.localName === 'link' ? this._owner.href : null; }
  get type() { return 'text/css'; }
  get title() { return this._owner ? this._owner.getAttribute('title') : null; }
  get parentStyleSheet() { return null; }
  get ownerRule() { return null; }
  get cssRules() { return this._rules.slice(); }
  get rules() { return this.cssRules; }
  insertRule(rule, index) {
    index = index === undefined ? 0 : Number(index);
    if (index < 0 || index > this._rules.length) throw domErr('IndexSizeError', 'index out of range');
    this._rules.splice(index, 0, new CSSRule(String(rule)));
    this._sync();
    return index;
  }
  addRule(sel, style, index) { this.insertRule(sel + '{' + style + '}', index === undefined ? this._rules.length : index); return -1; }
  deleteRule(index) { this._rules.splice(Number(index), 1); this._sync(); }
  removeRule(index) { this.deleteRule(index || 0); }
  replaceSync(text) { this._rules = splitRules(String(text)).map(r => new CSSRule(r)); this._sync(); }
  replace(text) { this.replaceSync(text); return Promise.resolve(this); }
  _sync() {
    if (this._roots) for (const r of this._roots) syncAdopted(r);
    let holder = this._holder;
    if (!holder) {
      if (this._owner && this._owner.localName === 'style') {
        holder = this._owner;
      } else if (this._owner || this._adopted) {
        holder = document.createElement('style');
        holder.setAttribute('data-huginn-cssom', '');
        const anchor = this._owner;
        if (anchor && anchor.parentNode) anchor.parentNode.insertBefore(holder, anchor.nextSibling);
        else (document.head || document.documentElement).appendChild(holder);
      } else {
        return;
      }
      defProp(this, '_holder', { value: holder });
    }
    N.setText(holder[ID], this._rules.map(r => r.cssText).join('\n'));
  }
}
expose(CSSStyleSheet);
function styleSheetFor(el) {
  return cached(el, 'sheet', () => {
    const s = new CSSStyleSheet();
    s._owner = el;
    if (el.localName === 'style') s._rules = splitRules(el.textContent).map(r => new CSSRule(r));
    return s;
  });
}
class StyleSheetList {
  item(i) { return this[i] || null; }
}
StyleSheetList.prototype[Symbol.iterator] = Array.prototype.values;
expose(StyleSheetList);
expose(class MediaList {}, 'MediaList');

// ------------------------------------------------------------- geometry --

class DOMRectReadOnly {
  constructor(x, y, width, height) {
    defProp(this, '_r', { value: [Number(x) || 0, Number(y) || 0, Number(width) || 0, Number(height) || 0] });
  }
  get x() { return this._r[0]; }
  get y() { return this._r[1]; }
  get width() { return this._r[2]; }
  get height() { return this._r[3]; }
  get top() { return Math.min(this._r[1], this._r[1] + this._r[3]); }
  get left() { return Math.min(this._r[0], this._r[0] + this._r[2]); }
  get right() { return Math.max(this._r[0], this._r[0] + this._r[2]); }
  get bottom() { return Math.max(this._r[1], this._r[1] + this._r[3]); }
  toJSON() { return { x: this.x, y: this.y, width: this.width, height: this.height, top: this.top, right: this.right, bottom: this.bottom, left: this.left }; }
  static fromRect(r) { r = r || {}; return new this(r.x, r.y, r.width, r.height); }
}
expose(DOMRectReadOnly);
class DOMRect extends DOMRectReadOnly {
  get x() { return this._r[0]; } set x(v) { this._r[0] = Number(v); }
  get y() { return this._r[1]; } set y(v) { this._r[1] = Number(v); }
  get width() { return this._r[2]; } set width(v) { this._r[2] = Number(v); }
  get height() { return this._r[3]; } set height(v) { this._r[3] = Number(v); }
}
expose(DOMRect);
class DOMPoint {
  constructor(x, y, z, w) { this.x = Number(x) || 0; this.y = Number(y) || 0; this.z = Number(z) || 0; this.w = w === undefined ? 1 : Number(w); }
  matrixTransform(m) { return m ? m.transformPoint(this) : new DOMPoint(this.x, this.y, this.z, this.w); }
  toJSON() { return { x: this.x, y: this.y, z: this.z, w: this.w }; }
  static fromPoint(p) { p = p || {}; return new DOMPoint(p.x, p.y, p.z, p.w); }
}
expose(DOMPoint);
expose(class DOMPointReadOnly extends DOMPoint {}, 'DOMPointReadOnly');
// 2D only.
class DOMMatrix {
  constructor(init) {
    let v = [1, 0, 0, 1, 0, 0];
    if (Array.isArray(init) && init.length >= 6) v = init.slice(0, 6).map(Number);
    else if (typeof init === 'string') {
      const m = /matrix\(([^)]*)\)/.exec(init);
      if (m) v = m[1].split(',').map(Number);
    }
    [this.a, this.b, this.c, this.d, this.e, this.f] = v;
    this.is2D = true;
  }
  get m11() { return this.a; } get m12() { return this.b; } get m21() { return this.c; } get m22() { return this.d; }
  get m41() { return this.e; } get m42() { return this.f; }
  get isIdentity() { return this.a === 1 && this.b === 0 && this.c === 0 && this.d === 1 && this.e === 0 && this.f === 0; }
  multiply(o) {
    return new DOMMatrix([this.a * o.a + this.c * o.b, this.b * o.a + this.d * o.b, this.a * o.c + this.c * o.d,
      this.b * o.c + this.d * o.d, this.a * o.e + this.c * o.f + this.e, this.b * o.e + this.d * o.f + this.f]);
  }
  translate(x, y) { return this.multiply(new DOMMatrix([1, 0, 0, 1, x || 0, y || 0])); }
  scale(s, sy) { s = s === undefined ? 1 : s; return this.multiply(new DOMMatrix([s, 0, 0, sy === undefined ? s : sy, 0, 0])); }
  rotate(deg) {
    const r = (deg || 0) * Math.PI / 180;
    return this.multiply(new DOMMatrix([Math.cos(r), Math.sin(r), -Math.sin(r), Math.cos(r), 0, 0]));
  }
  inverse() {
    const det = this.a * this.d - this.b * this.c;
    if (!det) return new DOMMatrix([NaN, NaN, NaN, NaN, NaN, NaN]);
    return new DOMMatrix([this.d / det, -this.b / det, -this.c / det, this.a / det,
      (this.c * this.f - this.d * this.e) / det, (this.b * this.e - this.a * this.f) / det]);
  }
  transformPoint(p) {
    p = p || {};
    return new DOMPoint(this.a * (p.x || 0) + this.c * (p.y || 0) + this.e, this.b * (p.x || 0) + this.d * (p.y || 0) + this.f);
  }
  toString() { return 'matrix(' + [this.a, this.b, this.c, this.d, this.e, this.f].join(', ') + ')'; }
}
expose(DOMMatrix);
expose(class DOMMatrixReadOnly extends DOMMatrix {}, 'DOMMatrixReadOnly');
defProp(G, 'WebKitCSSMatrix', { value: DOMMatrix, writable: true, configurable: true });

// --------------------------------------------------------------- document --

let readyState = 'loading';
const cookies = new Map();

class Document extends Node {
  constructor() { super(); }
  get URL() { return curUrl; }
  get documentURI() { return curUrl; }
  get domain() { return urlParts()[5]; }
  set domain(v) {}
  get referrer() { return ''; }
  get cookie() { return Array.from(cookies, ([k, v]) => (k ? k + '=' : '') + v).join('; '); }
  set cookie(v) {
    const parts = String(v).split(';');
    const eq = parts[0].indexOf('=');
    const name = (eq >= 0 ? parts[0].slice(0, eq) : '').trim();
    const value = (eq >= 0 ? parts[0].slice(eq + 1) : parts[0]).trim();
    let expired = false;
    for (const p of parts.slice(1)) {
      const i = p.indexOf('=');
      const k = (i >= 0 ? p.slice(0, i) : p).trim().toLowerCase();
      const val = i >= 0 ? p.slice(i + 1).trim() : '';
      if (k === 'max-age' && Number(val) <= 0) expired = true;
      if (k === 'expires' && Date.parse(val) < Date.now()) expired = true;
    }
    if (expired) cookies.delete(name); else cookies.set(name, value);
  }
  get readyState() { return readyState; }
  get characterSet() { return 'UTF-8'; }
  get charset() { return 'UTF-8'; }
  get inputEncoding() { return 'UTF-8'; }
  get contentType() { return 'text/html'; }
  get compatMode() { return N.quirks() ? 'BackCompat' : 'CSS1Compat'; }
  get doctype() { for (const id of N.kids(DOC_ID)) if (N.type(id) === 10) return wrap(id); return null; }
  get documentElement() { return wrap(N.docEl()); }
  get head() { return wrap(N.head()); }
  get body() { return wrap(N.body()); }
  set body(v) {
    const html = this.documentElement;
    const old = this.body;
    if (old) html.replaceChild(v, old); else html.appendChild(v);
  }
  get scrollingElement() { return this.documentElement; }
  get activeElement() { const f = N.focused(); return f ? wrap(f) : this.body; }
  get defaultView() { return G; }
  get visibilityState() { return 'visible'; }
  get hidden() { return false; }
  get webkitVisibilityState() { return 'visible'; }
  get webkitHidden() { return false; }
  get fullscreenElement() { return null; }
  get fullscreenEnabled() { return false; }
  get pointerLockElement() { return null; }
  get pictureInPictureElement() { return null; }
  get designMode() { return 'off'; }
  set designMode(v) {}
  get dir() { const h = this.documentElement; return h ? h.dir : ''; }
  set dir(v) { const h = this.documentElement; if (h) h.dir = v; }
  get currentScript() { return wrap(N.curScript()); }
  get title() {
    const t = N.qs(DOC_ID, 'title');
    return t > 0 ? N.text(t).replace(/[ \t\n\f\r]+/g, ' ').trim() : '';
  }
  set title(v) {
    let t = N.qs(DOC_ID, 'title');
    if (t <= 0) {
      const head = this.head;
      if (!head) return;
      t = head.appendChild(this.createElement('title'))[ID];
    }
    N.setText(t, str(v));
  }
  get lastModified() { return new Date().toLocaleString('en-US'); }
  get forms() { return htmlCollection(N.byTag(DOC_ID, 'form')); }
  get images() { return htmlCollection(N.byTag(DOC_ID, 'img')); }
  get links() { return htmlCollection(N.qsa(DOC_ID, 'a[href],area[href]')); }
  get anchors() { return htmlCollection(N.qsa(DOC_ID, 'a[name]')); }
  get scripts() { return htmlCollection(N.byTag(DOC_ID, 'script')); }
  get embeds() { return htmlCollection(N.byTag(DOC_ID, 'embed')); }
  get plugins() { return this.embeds; }
  get styleSheets() {
    const list = Object.create(StyleSheetList.prototype);
    let n = 0;
    for (const id of N.qsa(DOC_ID, 'style:not([data-huginn-cssom]),link')) {
      const el = wrap(id);
      if (el.localName === 'link' && !/(^|\s)stylesheet(\s|$)/i.test(el.rel)) continue;
      list[n++] = styleSheetFor(el);
    }
    defProp(list, 'length', { value: n });
    return list;
  }
  get adoptedStyleSheets() { return cached(this, 'adopted', () => []); }
  set adoptedStyleSheets(v) {
    const list = Array.from(v || []);
    for (const s of list) {
      if (!s._adopted) { defProp(s, '_adopted', { value: true }); s._sync(); }
    }
    this[CACHE].adopted = list;
  }
  get fonts() { return fontFaceSet; }
  get implementation() { return domImplementation; }
  get timeline() { return { currentTime: now() }; }
  get location() { return location; }
  set location(v) { location.href = v; }
  getElementById(id) { return wrap(N.byId(String(id))); }
  getElementsByName(name) { return nodeList(N.qsa(DOC_ID, '[name="' + String(name).replace(/["\\]/g, '\\$&') + '"]')); }
  createElement(name, options) {
    name = String(name);
    if (!/^[A-Za-z_:][^\s"'>/=\x00-\x1f]*$/.test(name)) throw domErr('InvalidCharacterError', "The tag name provided ('" + name + "') is not a valid name.");
    const lower = name.toLowerCase();
    const def = lower.indexOf('-') > 0 ? ceDefs.get(lower) : null;
    if (def) {
      try { return new def.ctor(); } catch (e) { reportError(e); }
    }
    const w = wrap(N.create(lower, HTML_NS));
    if (lower === 'script') cached(w, 'dynamic', () => true);
    return w;
  }
  createElementNS(ns, qname, options) {
    ns = ns == null ? '' : String(ns);
    qname = String(qname);
    const local = qname.indexOf(':') >= 0 ? qname.slice(qname.indexOf(':') + 1) : qname;
    if (ns === HTML_NS) return this.createElement(local, options);
    return wrap(N.create(local, ns));
  }
  createTextNode(data) { return wrap(N.createText(str(data))); }
  createComment(data) { return wrap(N.createComment(str(data))); }
  createCDATASection(data) { return wrap(N.createText(str(data))); }
  createProcessingInstruction(target, data) { return wrap(N.createComment('?' + target + ' ' + data + '?')); }
  createDocumentFragment() { return wrap(N.createFrag()); }
  createAttribute(name) { return makeAttr(null, String(name).toLowerCase(), ''); }
  createEvent(kind) {
    const map = { customevent: CustomEvent, mouseevent: MouseEvent, mouseevents: MouseEvent, uievent: UIEvent,
      uievents: UIEvent, keyboardevent: KeyboardEvent, messageevent: MessageEvent, hashchangeevent: HashChangeEvent,
      errorevent: ErrorEvent, popstateevent: PopStateEvent };
    const C = map[String(kind).toLowerCase()] || Event;
    return new C('');
  }
  createRange() { return new Range(); }
  createTreeWalker(root, whatToShow, filter) { return new TreeWalker(root, whatToShow, filter); }
  createNodeIterator(root, whatToShow, filter) { return new NodeIterator(root, whatToShow, filter); }
  importNode(node, deep) {
    const id = N.clone(idOf(node, 1), !!deep);
    if (!id) throw new TypeError("parameter 1 is not of type 'Node'");
    upgradeClones(id);
    return wrap(id);
  }
  adoptNode(node) { if (node.parentNode) node.parentNode.removeChild(node); return node; }
  elementFromPoint() { return this.body; }
  elementsFromPoint() { const b = this.body; return b ? [b, this.documentElement] : []; }
  caretRangeFromPoint() { return null; }
  caretPositionFromPoint() { return null; }
  getSelection() { return selection; }
  hasFocus() { return true; }
  open() { return this; }
  close() {}
  // document.write during a parser-blocking script inserts after the script;
  // after loading it is ignored (a real browser would wipe the page).
  write(...parts) {
    if (readyState === 'complete') return;
    const r = N.write(parts.join(''));
    if (r) connectCustom(r);
  }
  writeln(...parts) { this.write(parts.join('') + '\n'); }
  execCommand() { return false; }
  queryCommandSupported() { return false; }
  queryCommandState() { return false; }
  queryCommandEnabled() { return false; }
  exitFullscreen() { return Promise.resolve(); }
  exitPointerLock() {}
  exitPictureInPicture() { return Promise.resolve(); }
  startViewTransition(cb) {
    const p = Promise.resolve().then(() => cb && cb());
    return { finished: p, ready: p, updateCallbackDone: p, skipTransition() {} };
  }
  hasStorageAccess() { return Promise.resolve(true); }
  requestStorageAccess() { return Promise.resolve(); }
}
mixin(Document, ParentNode);
addHandlerProps(Document.prototype, ['readystatechange', 'visibilitychange', 'fullscreenchange', 'pointerlockchange',
  'securitypolicyviolation', 'DOMContentLoaded']);
expose(Document);
class HTMLDocument extends Document {}
expose(HTMLDocument);
expose(class XMLDocument extends Document {}, 'XMLDocument');

const document = Object.create(HTMLDocument.prototype);
document[ID] = DOC_ID;
wrappers.set(DOC_ID, document);

// A second document (DOMParser, createHTMLDocument): a detached <html>
// element in the same arena behind a small document-like facade.
function detachedDocument(html) {
  const root = wrap(N.create('html', HTML_NS));
  const m = /<html[^>]*>([\s\S]*)<\/html>/i.exec(html);
  const inner = m ? m[1] : html.replace(/<!doctype[^>]*>/i, '');
  for (const id of N.frag(root[ID], inner)) N.insert(root[ID], id, 0);
  let head = root.querySelector('head');
  let body = root.querySelector('body');
  if (!head) head = root.insertBefore(document.createElement('head'), root.firstChild);
  if (!body) body = root.appendChild(document.createElement('body'));
  const doc = Object.create(HTMLDocument.prototype);
  doc[ID] = root[ID];
  const defs = {
    documentElement: root, head, body, defaultView: null, readyState: 'complete', URL: 'about:blank',
    title: { get() { const t = root.querySelector('title'); return t ? t.textContent.trim() : ''; } },
    getElementById: function (id) { return root.querySelector('[id="' + String(id).replace(/["\\]/g, '\\$&') + '"]'); },
    querySelector: function (s) { return root.matches(s) ? root : root.querySelector(s); },
    querySelectorAll: function (s) { return root.querySelectorAll(s); },
    getElementsByTagName: function (n) { return root.getElementsByTagName(n); },
    getElementsByClassName: function (n) { return root.getElementsByClassName(n); },
    children: { get() { return htmlCollection([root[ID]]); } },
    firstElementChild: root, childNodes: { get() { return nodeList([root[ID]]); } },
  };
  for (const k in defs) {
    const v = defs[k];
    defProp(doc, k, v && typeof v === 'object' && 'get' in v && !(v[ID]) ? v : { value: v });
  }
  return doc;
}
const domImplementation = {
  hasFeature() { return true; },
  createHTMLDocument(title) { return detachedDocument('<head><title>' + str(title) + '</title></head><body></body>'); },
  createDocument() { return detachedDocument(''); },
  createDocumentType() { return null; },
};
class DOMParser {
  parseFromString(text, type) { return detachedDocument(String(text)); }
}
expose(DOMParser);
class XMLSerializer {
  serializeToString(node) { return node.nodeType === 1 ? node.outerHTML : node.nodeType === 9 ? node.documentElement.outerHTML : node.textContent || ''; }
}
expose(XMLSerializer);

// Ranges and selection: enough for feature tests and fragment creation.
class Range {
  constructor() {
    this.startContainer = document; this.startOffset = 0;
    this.endContainer = document; this.endOffset = 0;
  }
  get collapsed() { return this.startContainer === this.endContainer && this.startOffset === this.endOffset; }
  get commonAncestorContainer() { return this.startContainer; }
  setStart(n, o) { this.startContainer = n; this.startOffset = o; }
  setEnd(n, o) { this.endContainer = n; this.endOffset = o; }
  setStartBefore(n) { this.setStart(n.parentNode, 0); }
  setStartAfter(n) { this.setStart(n.parentNode, 0); }
  setEndBefore(n) { this.setEnd(n.parentNode, 0); }
  setEndAfter(n) { this.setEnd(n.parentNode, 0); }
  selectNode(n) { this.setStart(n, 0); this.setEnd(n, 0); }
  selectNodeContents(n) { this.setStart(n, 0); this.setEnd(n, n.childNodes.length); }
  collapse(toStart) { if (toStart) this.setEnd(this.startContainer, this.startOffset); else this.setStart(this.endContainer, this.endOffset); }
  cloneRange() { const r = new Range(); r.setStart(this.startContainer, this.startOffset); r.setEnd(this.endContainer, this.endOffset); return r; }
  createContextualFragment(html) {
    const ctx = this.startContainer && this.startContainer.nodeType === 1 ? this.startContainer : document.body || document.documentElement;
    return parseHTMLFragment(ctx, str(html));
  }
  getBoundingClientRect() { return this.startContainer && this.startContainer.getBoundingClientRect ? this.startContainer.getBoundingClientRect() : new DOMRect(); }
  getClientRects() { return []; }
  deleteContents() {}
  extractContents() { return document.createDocumentFragment(); }
  cloneContents() { return document.createDocumentFragment(); }
  insertNode(n) { const c = this.startContainer; if (c && c.nodeType === 1) c.insertBefore(n, c.childNodes[this.startOffset] || null); }
  surroundContents(n) {}
  detach() {}
  toString() { return ''; }
}
expose(Range);
expose(class StaticRange extends Range {}, 'StaticRange');
class Selection {
  get rangeCount() { return 0; }
  get isCollapsed() { return true; }
  get type() { return 'None'; }
  get anchorNode() { return null; }
  get focusNode() { return null; }
  getRangeAt() { throw domErr('IndexSizeError', 'no range'); }
  addRange() {}
  removeAllRanges() {}
  removeRange() {}
  empty() {}
  collapse() {}
  collapseToStart() {}
  collapseToEnd() {}
  selectAllChildren() {}
  setBaseAndExtent() {}
  extend() {}
  containsNode() { return false; }
  toString() { return ''; }
}
expose(Selection);
const selection = new Selection();

const NodeFilter = {
  FILTER_ACCEPT: 1, FILTER_REJECT: 2, FILTER_SKIP: 3, SHOW_ALL: 0xFFFFFFFF, SHOW_ELEMENT: 0x1, SHOW_ATTRIBUTE: 0x2,
  SHOW_TEXT: 0x4, SHOW_CDATA_SECTION: 0x8, SHOW_PROCESSING_INSTRUCTION: 0x40, SHOW_COMMENT: 0x80,
  SHOW_DOCUMENT: 0x100, SHOW_DOCUMENT_TYPE: 0x200, SHOW_DOCUMENT_FRAGMENT: 0x400,
};
defProp(G, 'NodeFilter', { value: NodeFilter, writable: true, configurable: true });
function acceptNode(walker, node) {
  const bit = 1 << (node.nodeType - 1);
  if (!(walker.whatToShow & bit)) return 3;
  const f = walker.filter;
  if (!f) return 1;
  return typeof f === 'function' ? f(node) : f.acceptNode(node);
}
// Pre-order walk inside `root`. FILTER_REJECT skips the subtree.
function nextInTree(root, node, skipChildren) {
  if (!skipChildren && node.firstChild) return node.firstChild;
  while (node && node !== root) {
    if (node.nextSibling) return node.nextSibling;
    node = node.parentNode;
  }
  return null;
}
class TreeWalker {
  constructor(root, whatToShow, filter) {
    this.root = root;
    this.whatToShow = whatToShow === undefined ? 0xFFFFFFFF : whatToShow >>> 0;
    this.filter = filter || null;
    this.currentNode = root;
  }
  nextNode() {
    let n = this.currentNode, skip = false;
    while ((n = nextInTree(this.root, n, skip))) {
      const r = acceptNode(this, n);
      if (r === 1) { this.currentNode = n; return n; }
      skip = r === 2;
    }
    return null;
  }
  previousNode() {
    let n = this.currentNode;
    while (n && n !== this.root) {
      let p = n.previousSibling;
      if (p) { while (p.lastChild) p = p.lastChild; n = p; } else n = n.parentNode;
      if (n && acceptNode(this, n) === 1) { this.currentNode = n; return n; }
    }
    return null;
  }
  parentNode() {
    let n = this.currentNode;
    while (n && n !== this.root) {
      n = n.parentNode;
      if (n && acceptNode(this, n) === 1) { this.currentNode = n; return n; }
    }
    return null;
  }
  firstChild() { const c = this.currentNode.firstChild; if (c) { this.currentNode = c; if (acceptNode(this, c) === 1) return c; return this.nextSibling(); } return null; }
  lastChild() { const c = this.currentNode.lastChild; if (c) { this.currentNode = c; if (acceptNode(this, c) === 1) return c; return this.previousSibling(); } return null; }
  nextSibling() { let n = this.currentNode; while ((n = n.nextSibling)) if (acceptNode(this, n) === 1) { this.currentNode = n; return n; } return null; }
  previousSibling() { let n = this.currentNode; while ((n = n.previousSibling)) if (acceptNode(this, n) === 1) { this.currentNode = n; return n; } return null; }
}
expose(TreeWalker);
class NodeIterator {
  constructor(root, whatToShow, filter) {
    this.root = root;
    this.whatToShow = whatToShow === undefined ? 0xFFFFFFFF : whatToShow >>> 0;
    this.filter = filter || null;
    this.referenceNode = root;
    this.started = false;
  }
  nextNode() {
    let n = this.referenceNode;
    if (!this.started) { this.started = true; if (acceptNode(this, n) === 1) return n; }
    while ((n = nextInTree(this.root, n, false))) {
      this.referenceNode = n;
      if (acceptNode(this, n) === 1) return n;
    }
    return null;
  }
  previousNode() { return null; }
  detach() {}
}
expose(NodeIterator);

// Web fonts load in the shell after scripts run; scripts see them as ready.
class FontFace {
  constructor(family, source, descriptors) {
    this.family = String(family);
    this.status = 'loaded';
    Object.assign(this, { style: 'normal', weight: 'normal', stretch: 'normal', display: 'auto' }, descriptors || {});
    this.loaded = Promise.resolve(this);
  }
  load() { return Promise.resolve(this); }
}
expose(FontFace);
const fontFaceSet = new EventTarget();
const fontList = new Set();
Object.assign(fontFaceSet, {
  status: 'loaded',
  ready: Promise.resolve(fontFaceSet),
  onloading: null, onloadingdone: null, onloadingerror: null,
  add(f) { fontList.add(f); return this; },
  delete(f) { return fontList.delete(f); },
  clear() { fontList.clear(); },
  has(f) { return fontList.has(f); },
  check() { return true; },
  load() { return Promise.resolve([]); },
  forEach(cb, t) { fontList.forEach(f => cb.call(t, f, f, this)); },
  values() { return fontList.values(); },
  entries() { return fontList.entries(); },
  keys() { return fontList.keys(); },
  [Symbol.iterator]() { return fontList.values(); },
});
defProp(fontFaceSet, 'size', { get() { return fontList.size; } });

// -------------------------------------------------------- custom elements --
// Upgrades happen when an element with a defined name is connected (or at
// define time for connected ones, shadow trees included).

const ceDefs = new Map();
const ceByCtor = new Map();
const ceWaiters = new Map();
const upgrading = [];

function upgrade(el, def) {
  defProp(el, CE, { value: def, writable: true, configurable: true });
  Object.setPrototypeOf(el, def.ctor.prototype);
  upgrading.push(el);
  try {
    new def.ctor();
  } catch (e) {
    reportError(e);
  } finally {
    upgrading.pop();
  }
  for (const name of def.observed) {
    const v = N.attr(el[ID], name);
    if (v !== null) attributeChanged(el, name, null, v);
  }
}
function connectCustom(ids) {
  for (const id of ids) {
    const def = ceDefs.get(N.name(id));
    if (!def || !N.isHtml(id)) continue;
    const el = wrap(id);
    if (!el[CE]) upgrade(el, def);
    if (typeof el.connectedCallback === 'function' && N.connected(id)) {
      try { el.connectedCallback(); } catch (e) { reportError(e); }
    }
  }
}
function disconnectCustom(ids) {
  for (const id of ids) {
    const el = wrappers.get(id);
    if (el && el[CE] && typeof el.disconnectedCallback === 'function') {
      try { el.disconnectedCallback(); } catch (e) { reportError(e); }
    }
  }
}
function attributeChanged(el, name, old, value) {
  const def = el[CE];
  if (!def || def.observed.indexOf(name) < 0 || typeof el.attributeChangedCallback !== 'function') return;
  try { el.attributeChangedCallback(name, old, value); } catch (e) { reportError(e); }
}
// Template contents fragments that scripts have seen (`template.content`).
// Their nodes belong to the template's inert document, which has no custom
// element registry.
const templateContents = new Set();
function inTemplateContents(id) {
  if (!templateContents.size) return false;
  let p;
  while ((p = N.parent(id))) id = p;
  return templateContents.has(id);
}
// A clone made in (or imported into) the document upgrades its custom
// elements before cloneNode/importNode returns: "create an element" with the
// synchronous custom elements flag unset enqueues an upgrade reaction, and
// both methods are [CEReactions]. Not connected yet, so no connectedCallback.
function upgradeClones(id) {
  if (!ceDefs.size) return;
  for (const c of N.customIn(id)) {
    const def = ceDefs.get(N.name(c));
    if (!def || !N.isHtml(c)) continue;
    const el = wrap(c);
    if (!el[CE]) upgrade(el, def);
  }
}

class CustomElementRegistry {
  define(name, ctor, options) {
    name = String(name);
    if (typeof ctor !== 'function') throw new TypeError("parameter 2 is not of type 'CustomElementConstructor'");
    if (!/^[a-z][a-z0-9._\-\u00b7\u00c0-\uffff]*-[a-z0-9._\-\u00b7\u00c0-\uffff]*$/.test(name)) {
      throw domErr('SyntaxError', "'" + name + "' is not a valid custom element name");
    }
    if (ceDefs.has(name)) throw domErr('NotSupportedError', "the name '" + name + "' has already been used with this registry");
    if (options && options.extends) return;  // customized built-ins are not supported
    let observed = [];
    try { observed = Array.from(ctor.observedAttributes || [], String); } catch (e) { reportError(e); }
    const def = { name, ctor, observed };
    ceDefs.set(name, def);
    ceByCtor.set(ctor, def);
    N.defineCE();
    connectCustom(N.customIn(DOC_ID, name));
    const w = ceWaiters.get(name);
    if (w) { ceWaiters.delete(name); w.resolve(ctor); }
  }
  get(name) { const d = ceDefs.get(String(name)); return d ? d.ctor : undefined; }
  getName(ctor) { const d = ceByCtor.get(ctor); return d ? d.name : null; }
  whenDefined(name) {
    name = String(name);
    const d = ceDefs.get(name);
    if (d) return Promise.resolve(d.ctor);
    let w = ceWaiters.get(name);
    if (!w) {
      let resolve;
      const promise = new Promise(r => { resolve = r; });
      ceWaiters.set(name, w = { promise, resolve });
    }
    return w.promise;
  }
  upgrade(root) {
    if (!root) return;
    connectCustom(N.customIn(root[ID]));
  }
}
expose(CustomElementRegistry);
const customElements = new CustomElementRegistry();
