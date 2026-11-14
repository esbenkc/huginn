// ------------------------------------------------------------------ nodes --

function wrap(id) {
  if (!id) return null;
  let w = wrappers.get(id);
  if (w) return w;
  let proto;
  switch (N.type(id)) {
    case 1: proto = elementProto(id); break;
    case 3: proto = Text.prototype; break;
    case 7: proto = ProcessingInstruction.prototype; break;
    case 8: proto = Comment.prototype; break;
    case 9: return document;
    case 10: proto = DocumentType.prototype; break;
    case 11: proto = N.shadowHost(id) ? ShadowRoot.prototype : DocumentFragment.prototype; break;
    default: proto = Node.prototype;
  }
  w = Object.create(proto);
  w[ID] = id;
  wrappers.set(id, w);
  return w;
}

function idOf(node, which) {
  if (node === null || node === undefined) return 0;
  const id = node[ID];
  if (id === undefined) throw new TypeError('parameter ' + (which || 1) + " is not of type 'Node'");
  return id;
}

function makeList(C, ids) {
  const list = Object.create(C.prototype);
  for (let i = 0; i < ids.length; i++) list[i] = wrap(ids[i]);
  defProp(list, 'length', { value: ids.length });
  return list;
}

// Lists are snapshots taken when the property is read. Pages that hold a
// live list across mutations see stale contents; that is rare in practice.
class NodeList {
  item(i) { return this[i] || null; }
}
NodeList.prototype.forEach = Array.prototype.forEach;
NodeList.prototype.entries = Array.prototype.entries;
NodeList.prototype.keys = Array.prototype.keys;
NodeList.prototype.values = Array.prototype.values;
NodeList.prototype[Symbol.iterator] = Array.prototype.values;
expose(NodeList);
class HTMLCollection {
  item(i) { return this[i] || null; }
  namedItem(name) {
    for (let i = 0; i < this.length; i++) {
      const e = this[i];
      if (e.id === name || e.getAttribute('name') === name) return e;
    }
    return null;
  }
}
HTMLCollection.prototype[Symbol.iterator] = Array.prototype.values;
expose(HTMLCollection);
const nodeList = ids => makeList(NodeList, ids);
const htmlCollection = ids => makeList(HTMLCollection, ids);

function insertError(code) {
  if (code === 2) return domErr('NotFoundError', 'The node before which the new node is to be inserted is not a child of this node.');
  return domErr('HierarchyRequestError', 'The new child element contains the parent.');
}

// All insertions go through here. The native side moves the node (or the
// children of a fragment), runs newly connected inline scripts, queues
// external ones, and returns newly connected custom-element candidates.
function insertNode(parent, child, ref) {
  const cid = idOf(child, 1);
  if (!cid) throw new TypeError("parameter 1 is not of type 'Node'");
  const added = mutationObservers.length ? (N.type(cid) === 11 ? N.kids(cid) : [cid]) : null;
  const r = N.insert(parent[ID], cid, idOf(ref, 2));
  if (typeof r === 'number') throw insertError(r);
  if (added && added.length) {
    notifyMutation('childList', parent[ID], {
      addedNodes: nodeList(added), previousSibling: wrap(N.prev(added[0])), nextSibling: ref || null,
    });
  }
  if (r) connectCustom(r);
  return child;
}

function removeNode(child) {
  const id = child[ID];
  const parent = N.parent(id);
  const prev = parent && mutationObservers.length ? N.prev(id) : 0;
  const next = parent && mutationObservers.length ? N.next(id) : 0;
  if (ceDefs.size && parent && N.connected(id)) {
    const removed = N.customIn(id);
    N.remove(id);
    disconnectCustom(removed);
  } else {
    N.remove(id);
  }
  if (parent && mutationObservers.length) {
    notifyMutation('childList', parent, { removedNodes: nodeList([id]), previousSibling: wrap(prev), nextSibling: wrap(next) });
  }
}

// Character data writes, with MutationObserver records.
function setCharData(id, v) {
  if (mutationObservers.length) {
    const old = N.data(id);
    N.setData(id, v);
    notifyMutation('characterData', id, { oldValue: old });
  } else {
    N.setData(id, v);
  }
}

// Replace all children (textContent, innerHTML), with records and
// custom-element callbacks. `replace` does the native work and returns
// custom-element candidates among the new children, if any.
function replaceChildrenNative(id, replace) {
  const removedIds = mutationObservers.length ? N.kids(id) : null;
  const removed = ceDefs.size && N.connected(id) ? N.customIn(id).filter(x => x !== id) : null;
  const added = replace();
  if (removed) disconnectCustom(removed);
  if (removedIds) notifyMutation('childList', id, { removedNodes: nodeList(removedIds), addedNodes: nodeList(N.kids(id)) });
  if (added) connectCustom(added);
}

// Strings in append()/before()/... become text nodes; several nodes go in a fragment.
function toNode(nodes) {
  if (nodes.length === 1 && nodes[0] && nodes[0][ID] !== undefined) return nodes[0];
  const frag = document.createDocumentFragment();
  for (const n of nodes) frag.appendChild(n && n[ID] !== undefined ? n : document.createTextNode(String(n)));
  return frag;
}

class Node extends EventTarget {
  constructor() { super(); throw new TypeError('Illegal constructor'); }
  get nodeType() { return N.type(this[ID]); }
  get nodeName() {
    switch (N.type(this[ID])) {
      case 1: return this.tagName;
      case 3: return '#text';
      case 7: return N.name(this[ID]);
      case 8: return '#comment';
      case 9: return '#document';
      case 10: return N.name(this[ID]);
      case 11: return '#document-fragment';
    }
    return '';
  }
  get nodeValue() {
    const t = N.type(this[ID]);
    return t === 3 || t === 7 || t === 8 ? N.data(this[ID]) : null;
  }
  set nodeValue(v) {
    const t = N.type(this[ID]);
    if (t === 3 || t === 7 || t === 8) setCharData(this[ID], nullableStr(v));
  }
  get textContent() {
    const t = N.type(this[ID]);
    if (t === 9 || t === 10) return null;
    if (t === 3 || t === 7 || t === 8) return N.data(this[ID]);
    return N.text(this[ID]);
  }
  set textContent(v) {
    const id = this[ID];
    const t = N.type(id);
    if (t === 9 || t === 10) return;
    if (t === 3 || t === 7 || t === 8) { setCharData(id, nullableStr(v)); return; }
    replaceChildrenNative(id, () => { N.setText(id, nullableStr(v)); return null; });
  }
  get parentNode() { return wrap(N.parent(this[ID])); }
  get parentElement() { const p = N.parent(this[ID]); return p && N.type(p) === 1 ? wrap(p) : null; }
  get firstChild() { return wrap(N.first(this[ID])); }
  get lastChild() { return wrap(N.last(this[ID])); }
  get previousSibling() { return wrap(N.prev(this[ID])); }
  get nextSibling() { return wrap(N.next(this[ID])); }
  get childNodes() { return nodeList(N.kids(this[ID])); }
  hasChildNodes() { return N.first(this[ID]) !== 0; }
  get ownerDocument() { return this[ID] === DOC_ID ? null : document; }
  get isConnected() { return N.connected(this[ID]); }
  get baseURI() { return N.baseUrl(); }
  getRootNode(options) {
    const composed = !!(options && options.composed);
    let id = this[ID], p;
    for (;;) {
      while ((p = N.parent(id))) id = p;
      if (!composed || !(p = N.shadowHost(id))) return wrap(id);
      id = p;
    }
  }
  appendChild(child) { return insertNode(this, child, null); }
  insertBefore(child, ref) { return insertNode(this, child, ref === undefined ? null : ref); }
  removeChild(child) {
    const id = idOf(child);
    if (!id || N.parent(id) !== this[ID]) throw domErr('NotFoundError', 'The node to be removed is not a child of this node.');
    removeNode(child);
    return child;
  }
  replaceChild(child, old) {
    const oid = idOf(old, 2);
    if (!oid || N.parent(oid) !== this[ID]) throw domErr('NotFoundError', 'The node to be replaced is not a child of this node.');
    if (child === old) return old;
    let ref = N.next(oid);
    if (ref && ref === idOf(child)) ref = N.next(ref);
    removeNode(old);
    insertNode(this, child, wrap(ref));
    return old;
  }
  cloneNode(deep) {
    const id = N.clone(this[ID], !!deep);
    if (!inTemplateContents(this[ID])) upgradeClones(id);
    return wrap(id);
  }
  contains(other) {
    if (other == null) return false;
    return N.contains(this[ID], idOf(other));
  }
  compareDocumentPosition(other) { return N.cmp(this[ID], idOf(other)); }
  isSameNode(other) { return other === this; }
  isEqualNode(other) {
    if (!other || other.nodeType !== this.nodeType) return false;
    if (this.nodeType === 1) return this.outerHTML === other.outerHTML;
    return this.nodeValue === other.nodeValue && this.textContent === other.textContent;
  }
  normalize() {}
  lookupNamespaceURI(prefix) { return prefix ? null : HTML_NS; }
  lookupPrefix() { return null; }
  isDefaultNamespace(ns) { return ns === HTML_NS; }
}
const NODE_CONSTS = {
  ELEMENT_NODE: 1, ATTRIBUTE_NODE: 2, TEXT_NODE: 3, CDATA_SECTION_NODE: 4, ENTITY_REFERENCE_NODE: 5,
  ENTITY_NODE: 6, PROCESSING_INSTRUCTION_NODE: 7, COMMENT_NODE: 8, DOCUMENT_NODE: 9, DOCUMENT_TYPE_NODE: 10,
  DOCUMENT_FRAGMENT_NODE: 11, NOTATION_NODE: 12, DOCUMENT_POSITION_DISCONNECTED: 1,
  DOCUMENT_POSITION_PRECEDING: 2, DOCUMENT_POSITION_FOLLOWING: 4, DOCUMENT_POSITION_CONTAINS: 8,
  DOCUMENT_POSITION_CONTAINED_BY: 16, DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC: 32,
};
for (const k in NODE_CONSTS) {
  defProp(Node, k, { value: NODE_CONSTS[k], enumerable: true });
  defProp(Node.prototype, k, { value: NODE_CONSTS[k], enumerable: true });
}
expose(Node);

// Copy accessors and methods of a mixin class onto an interface.
function mixin(target, source) {
  for (const k of Object.getOwnPropertyNames(source.prototype)) {
    if (k === 'constructor') continue;
    defProp(target.prototype, k, Object.getOwnPropertyDescriptor(source.prototype, k));
  }
}

function checkSelector(r, sel) {
  if (r === null || r === -1) throw domErr('SyntaxError', `'${sel}' is not a valid selector.`);
  return r;
}

class ParentNode {
  get children() { return htmlCollection(N.ekids(this[ID])); }
  get childElementCount() { return N.ekids(this[ID]).length; }
  get firstElementChild() { const k = N.ekids(this[ID]); return k.length ? wrap(k[0]) : null; }
  get lastElementChild() { const k = N.ekids(this[ID]); return k.length ? wrap(k[k.length - 1]) : null; }
  append(...nodes) { if (nodes.length) insertNode(this, toNode(nodes), null); }
  prepend(...nodes) { if (nodes.length) insertNode(this, toNode(nodes), this.firstChild); }
  replaceChildren(...nodes) {
    const n = nodes.length ? toNode(nodes) : null;
    let c;
    while ((c = this.firstChild)) removeNode(c);
    if (n) insertNode(this, n, null);
  }
  querySelector(sel) { sel = String(sel); return wrap(checkSelector(N.qs(this[ID], sel), sel)); }
  querySelectorAll(sel) { sel = String(sel); return nodeList(checkSelector(N.qsa(this[ID], sel), sel)); }
  getElementsByTagName(name) { return htmlCollection(N.byTag(this[ID], String(name))); }
  getElementsByTagNameNS(ns, name) { return htmlCollection(N.byTag(this[ID], String(name))); }
  getElementsByClassName(names) { return htmlCollection(N.byClass(this[ID], String(names))); }
}

class ChildNode {
  before(...nodes) { const p = this.parentNode; if (p && nodes.length) insertNode(p, toNode(nodes), this); }
  after(...nodes) { const p = this.parentNode; if (p && nodes.length) insertNode(p, toNode(nodes), this.nextSibling); }
  replaceWith(...nodes) {
    const p = this.parentNode;
    if (!p) return;
    let next = this.nextSibling;
    while (next && nodes.indexOf(next) >= 0) next = next.nextSibling;
    removeNode(this);
    if (nodes.length) insertNode(p, toNode(nodes), next);
  }
  remove() { if (N.parent(this[ID])) removeNode(this); }
  get previousElementSibling() {
    let s = N.prev(this[ID]);
    while (s && N.type(s) !== 1) s = N.prev(s);
    return wrap(s);
  }
  get nextElementSibling() {
    let s = N.next(this[ID]);
    while (s && N.type(s) !== 1) s = N.next(s);
    return wrap(s);
  }
}

class CharacterData extends Node {
  get data() { return N.data(this[ID]); }
  set data(v) { setCharData(this[ID], nullableStr(v)); }
  get length() { return N.data(this[ID]).length; }
  appendData(s) { this.data += String(s); }
  insertData(off, s) { const d = this.data; this.data = d.slice(0, off) + s + d.slice(off); }
  deleteData(off, count) { const d = this.data; this.data = d.slice(0, off) + d.slice(off + count); }
  replaceData(off, count, s) { const d = this.data; this.data = d.slice(0, off) + s + d.slice(off + count); }
  substringData(off, count) { return this.data.substr(off, count); }
}
mixin(CharacterData, ChildNode);
expose(CharacterData);
class Text extends CharacterData {
  constructor(data) { return wrap(N.createText(str(data))); }
  splitText(offset) {
    const d = this.data;
    const rest = document.createTextNode(d.slice(offset));
    this.data = d.slice(0, offset);
    const p = this.parentNode;
    if (p) p.insertBefore(rest, this.nextSibling);
    return rest;
  }
  get wholeText() { return this.data; }
  get assignedSlot() { return null; }
}
expose(Text);
expose(class CDATASection extends Text {}, 'CDATASection');
class Comment extends CharacterData {
  constructor(data) { return wrap(N.createComment(str(data))); }
}
expose(Comment);
class ProcessingInstruction extends CharacterData {
  get target() { return N.name(this[ID]); }
}
expose(ProcessingInstruction);
class DocumentType extends Node {
  get name() { return N.name(this[ID]); }
  get publicId() { return ''; }
  get systemId() { return ''; }
}
mixin(DocumentType, ChildNode);
expose(DocumentType);
class DocumentFragment extends Node {
  constructor() { return wrap(N.createFrag()); }
  getElementById(id) {
    const r = N.qs(this[ID], '[id="' + String(id).replace(/["\\]/g, '\\$&') + '"]');
    return r > 0 ? wrap(r) : null;
  }
}
mixin(DocumentFragment, ParentNode);
expose(DocumentFragment);
// Shadow roots live in huginn-dom (declarative ones from the parser, others from
// attachShadow) and render through the flat tree.
const SHADOW_MODE = Symbol('huginn.shadowMode');
class ShadowRoot extends DocumentFragment {
  get host() { return wrap(N.shadowHost(this[ID])); }
  // Declarative roots: `mode` is not kept; they are treated as open.
  get mode() { return this[SHADOW_MODE] || 'open'; }
  get innerHTML() { return N.html(this[ID]); }
  set innerHTML(v) { setInnerHTML(this, v); }
  get activeElement() { return null; }
  get delegatesFocus() { return false; }
  get adoptedStyleSheets() { return this[ADOPTED] || []; }
  // The style engine reads the joined text of the adopted sheets; a sheet
  // remembers the roots that adopted it so later edits reach them.
  set adoptedStyleSheets(v) {
    const list = Array.from(v || []);
    for (const s of this[ADOPTED] || []) if (s._roots) s._roots.delete(this);
    for (const s of list) {
      if (!s._roots) defProp(s, '_roots', { value: new Set() });
      s._roots.add(this);
    }
    this[ADOPTED] = list;
    syncAdopted(this);
  }
  getSelection() { return null; }
}
expose(ShadowRoot);
const ADOPTED = Symbol('huginn.adopted');
function syncAdopted(root) {
  N.setAdopted(root[ID], root[ADOPTED].map(s => s._rules.map(r => r.cssText).join('\n')).join('\n'));
}
