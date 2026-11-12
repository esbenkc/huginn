// ------------------------------------------------------ element interfaces --

const P = (C) => C.prototype;
const gs = (get, set) => ({ get, set, configurable: true });
const ro = (get) => ({ get, configurable: true });
const geomOr = (i, def) => function () { const g = N.geom(this[ID]); return g ? Math.round(g[i]) : def; };

// Links.
for (const C of [E.HTMLAnchorElement, E.HTMLAreaElement]) {
  reflectUrl(P(C), ['href']);
  reflect(P(C), ['target', 'download', 'rel', 'hreflang', 'type', 'ping', ['referrerPolicy', 'referrerpolicy']]);
  defProp(P(C), 'relList', ro(function () { return tokenList(this, 'rel'); }));
  defProp(P(C), 'text', gs(function () { return this.textContent; }, function (v) { this.textContent = v; }));
  const parts = [['protocol', 1], ['username', 2], ['password', 3], ['host', 4], ['hostname', 5], ['port', 6],
    ['pathname', 7], ['search', 8], ['hash', 9], ['origin', 10]];
  for (const [k, i] of parts) {
    defProp(P(C), k, gs(
      function () { const v = N.attr(this[ID], 'href'); const u = v === null ? null : N.url(v, ''); return u ? u[i] : ''; },
      function (v) {
        if (k === 'origin') return;
        const href = N.attr(this[ID], 'href');
        const url = new URL(href === null ? '' : href, N.baseUrl());
        url[k] = v;
        this.setAttribute('href', url.href);
      }));
  }
  P(C).toString = function () { return this.href; };
}
reflect(P(E.HTMLAnchorElement), ['name', 'charset', 'coords', 'shape']);
reflect(P(E.HTMLAreaElement), ['alt', 'coords', 'shape']);
reflectUrl(P(E.HTMLBaseElement), ['href']);
reflect(P(E.HTMLBaseElement), ['target']);

// Images.
reflectUrl(P(E.HTMLImageElement), ['src', ['longDesc', 'longdesc']]);
reflect(P(E.HTMLImageElement), ['alt', 'srcset', 'sizes', ['crossOrigin', 'crossorigin'], ['useMap', 'usemap'],
  'decoding', 'loading', ['referrerPolicy', 'referrerpolicy'], ['fetchPriority', 'fetchpriority'], 'name', 'align']);
reflectBool(P(E.HTMLImageElement), [['isMap', 'ismap']]);
reflectInt(P(E.HTMLImageElement), [['width', 'width', geomOr(2, 0)], ['height', 'height', geomOr(3, 0)]]);
Object.defineProperties(P(E.HTMLImageElement), {
  naturalWidth: ro(() => 0),
  naturalHeight: ro(() => 0),
  complete: ro(() => true),
  currentSrc: ro(function () { return this.src; }),
  x: ro(function () { const g = N.geom(this[ID]); return g ? g[0] : 0; }),
  y: ro(function () { const g = N.geom(this[ID]); return g ? g[1] : 0; }),
});
P(E.HTMLImageElement).decode = function () { return Promise.resolve(); };
function Image(w, h) {
  const img = document.createElement('img');
  if (w !== undefined) img.width = w;
  if (h !== undefined) img.height = h;
  return img;
}
Image.prototype = P(E.HTMLImageElement);
defProp(G, 'Image', { value: Image, writable: true, configurable: true });

// Form controls. Values and checkedness live in the attributes, so the
// style engine sees them (`:checked`, `[value]`, rendered input text).
function formOf(el) { return el.closest('form'); }
const VALIDITY = { valid: true, valueMissing: false, typeMismatch: false, patternMismatch: false, tooLong: false,
  tooShort: false, rangeUnderflow: false, rangeOverflow: false, stepMismatch: false, badInput: false, customError: false };
const FORM_CONTROL = {
  form: ro(function () { return formOf(this); }),
  labels: ro(function () { return this.id ? document.querySelectorAll('label[for="' + this.id + '"]') : nodeList([]); }),
  validity: ro(() => VALIDITY),
  validationMessage: ro(() => ''),
  willValidate: ro(() => true),
  checkValidity: { value() { return true; }, configurable: true, writable: true },
  reportValidity: { value() { return true; }, configurable: true, writable: true },
  setCustomValidity: { value() {}, configurable: true, writable: true },
};
for (const C of [E.HTMLInputElement, E.HTMLButtonElement, E.HTMLSelectElement, E.HTMLTextAreaElement,
  E.HTMLOutputElement, E.HTMLFieldSetElement, E.HTMLObjectElement]) {
  Object.defineProperties(P(C), FORM_CONTROL);
  reflect(P(C), ['name']);
  reflectBool(P(C), ['disabled']);
}
reflect(P(E.HTMLInputElement), ['placeholder', 'min', 'max', 'step', 'pattern', 'autocomplete', 'accept', 'alt',
  ['defaultValue', 'value'], ['formAction', 'formaction'], ['formMethod', 'formmethod'], ['formTarget', 'formtarget']]);
reflectUrl(P(E.HTMLInputElement), ['src']);
reflectBool(P(E.HTMLInputElement), [['readOnly', 'readonly'], 'required', 'multiple', ['defaultChecked', 'checked'],
  ['formNoValidate', 'formnovalidate']]);
reflectInt(P(E.HTMLInputElement), [['maxLength', 'maxlength', -1], ['minLength', 'minlength', -1], ['size', 'size', 20]]);
Object.defineProperties(P(E.HTMLInputElement), {
  type: gs(function () { return (N.attr(this[ID], 'type') || '').toLowerCase() || 'text'; },
    function (v) { this.setAttribute('type', v); }),
  value: gs(function () { return nullableStr(N.attr(this[ID], 'value')); },
    function (v) { N.setAttr(this[ID], 'value', str(v)); }),
  checked: gs(function () { return N.attr(this[ID], 'checked') !== null; }, function (v) {
    if (!v) { N.delAttr(this[ID], 'checked'); return; }
    N.setAttr(this[ID], 'checked', '');
    const name = N.attr(this[ID], 'name');
    if (name && this.type === 'radio') {
      for (const r of N.qsa(DOC_ID, 'input[type=radio]')) {
        if (r !== this[ID] && N.attr(r, 'name') === name) N.delAttr(r, 'checked');
      }
    }
  }),
  indeterminate: { value: false, writable: true, configurable: true },
  files: ro(() => null),
  list: ro(function () { const l = N.attr(this[ID], 'list'); return l ? document.getElementById(l) : null; }),
  valueAsNumber: gs(function () { return parseFloat(this.value); }, function (v) { this.value = String(v); }),
  valueAsDate: gs(() => null, () => {}),
  selectionStart: gs(function () { return this.value.length; }, () => {}),
  selectionEnd: gs(function () { return this.value.length; }, () => {}),
  selectionDirection: gs(() => 'none', () => {}),
});
for (const C of [E.HTMLInputElement, E.HTMLTextAreaElement]) {
  for (const m of ['select', 'setSelectionRange', 'setRangeText']) P(C)[m] = function () {};
}
for (const m of ['showPicker', 'stepUp', 'stepDown']) P(E.HTMLInputElement)[m] = function () {};
defProp(P(E.HTMLButtonElement), 'type', gs(function () {
  const t = (N.attr(this[ID], 'type') || '').toLowerCase();
  return t === 'button' || t === 'reset' ? t : 'submit';
}, function (v) { this.setAttribute('type', v); }));
reflect(P(E.HTMLButtonElement), ['value', ['formAction', 'formaction'], ['formMethod', 'formmethod']]);
reflect(P(E.HTMLTextAreaElement), ['placeholder', 'wrap', 'autocomplete']);
reflectBool(P(E.HTMLTextAreaElement), [['readOnly', 'readonly'], 'required']);
reflectInt(P(E.HTMLTextAreaElement), [['rows', 'rows', 2], ['cols', 'cols', 20], ['maxLength', 'maxlength', -1], ['minLength', 'minlength', -1]]);
Object.defineProperties(P(E.HTMLTextAreaElement), {
  type: ro(() => 'textarea'),
  value: gs(function () { return this.textContent; }, function (v) { this.textContent = str(v); }),
  defaultValue: gs(function () { return this.textContent; }, function (v) { this.textContent = str(v); }),
  textLength: ro(function () { return this.textContent.length; }),
  selectionStart: gs(() => 0, () => {}),
  selectionEnd: gs(() => 0, () => {}),
});
const optionsOf = (sel) => N.qsa(sel[ID], 'option');
reflectBool(P(E.HTMLSelectElement), ['multiple', 'required']);
reflectInt(P(E.HTMLSelectElement), [['size', 'size', 0]]);
Object.defineProperties(P(E.HTMLSelectElement), {
  type: ro(function () { return this.multiple ? 'select-multiple' : 'select-one'; }),
  options: ro(function () { return htmlCollection(optionsOf(this)); }),
  length: ro(function () { return optionsOf(this).length; }),
  selectedIndex: gs(function () {
    const o = optionsOf(this);
    for (let i = 0; i < o.length; i++) if (N.attr(o[i], 'selected') !== null) return i;
    return o.length && !this.multiple ? 0 : -1;
  }, function (i) {
    const o = optionsOf(this);
    for (let k = 0; k < o.length; k++) wrap(o[k]).selected = k === Number(i);
  }),
  selectedOptions: ro(function () {
    const i = this.selectedIndex;
    return htmlCollection(this.multiple ? optionsOf(this).filter(o => N.attr(o, 'selected') !== null) : i >= 0 ? [optionsOf(this)[i]] : []);
  }),
  value: gs(function () { const i = this.selectedIndex; return i >= 0 ? wrap(optionsOf(this)[i]).value : ''; },
    function (v) { for (const o of optionsOf(this)) wrap(o).selected = wrap(o).value === String(v); }),
});
P(E.HTMLSelectElement).item = function (i) { return wrap(optionsOf(this)[i] || 0); };
P(E.HTMLSelectElement).namedItem = function (n) { return this.options.namedItem(n); };
P(E.HTMLSelectElement).add = function (opt, before) {
  this.insertBefore(opt, typeof before === 'number' ? this.options[before] || null : before || null);
};
P(E.HTMLSelectElement).remove = function (i) {
  if (i === undefined) { ChildNode.prototype.remove.call(this); return; }
  const o = this.options[i];
  if (o) o.remove();
};
reflectBool(P(E.HTMLOptionElement), ['selected', ['defaultSelected', 'selected'], 'disabled']);
reflect(P(E.HTMLOptionElement), ['label']);
Object.defineProperties(P(E.HTMLOptionElement), {
  value: gs(function () { const v = N.attr(this[ID], 'value'); return v === null ? this.text : v; },
    function (v) { this.setAttribute('value', v); }),
  text: gs(function () { return N.text(this[ID]).replace(/[ \t\n\f\r]+/g, ' ').trim(); },
    function (v) { this.textContent = v; }),
  index: ro(function () { const s = this.closest('select'); return s ? optionsOf(s).indexOf(this[ID]) : 0; }),
  form: ro(function () { return formOf(this); }),
});
function Option(text, value, defaultSelected, selected) {
  const o = document.createElement('option');
  if (text !== undefined) o.text = text;
  if (value !== undefined) o.value = value;
  if (defaultSelected || selected) o.selected = true;
  return o;
}
Option.prototype = P(E.HTMLOptionElement);
defProp(G, 'Option', { value: Option, writable: true, configurable: true });
reflectUrl(P(E.HTMLFormElement), ['action']);
reflect(P(E.HTMLFormElement), ['method', 'enctype', 'target', 'name', ['acceptCharset', 'accept-charset'], 'autocomplete', 'rel']);
reflectBool(P(E.HTMLFormElement), [['noValidate', 'novalidate']]);
Object.defineProperties(P(E.HTMLFormElement), {
  elements: ro(function () { return htmlCollection(N.qsa(this[ID], 'input,select,textarea,button,fieldset,output,object')); }),
  length: ro(function () { return this.elements.length; }),
});
// The browser navigates after the script returns (HostRequest::Navigate).
P(E.HTMLFormElement).submit = function () { N.submit(this[ID], 0, true); };
P(E.HTMLFormElement).requestSubmit = function (submitter) { N.submit(this[ID], submitter ? submitter[ID] : 0, false); };
P(E.HTMLFormElement).reset = function () { dispatch(this, new Event('reset', { bubbles: true, cancelable: true })); };
P(E.HTMLFormElement).checkValidity = function () { return true; };
P(E.HTMLFormElement).reportValidity = function () { return true; };
reflect(P(E.HTMLLabelElement), [['htmlFor', 'for']]);
defProp(P(E.HTMLLabelElement), 'control', ro(function () {
  const f = this.htmlFor;
  return f ? document.getElementById(f) : this.querySelector('input,select,textarea,button');
}));
defProp(P(E.HTMLLabelElement), 'form', ro(function () { return formOf(this); }));
defProp(P(E.HTMLLegendElement), 'form', ro(function () { return formOf(this); }));
reflect(P(E.HTMLOutputElement), [['htmlFor', 'for']]);

// Scripts, styles and document metadata.
reflectUrl(P(E.HTMLScriptElement), ['src']);
reflect(P(E.HTMLScriptElement), ['type', 'charset', ['crossOrigin', 'crossorigin'], 'integrity',
  ['referrerPolicy', 'referrerpolicy'], 'event', ['htmlFor', 'for'], ['fetchPriority', 'fetchpriority']]);
reflectBool(P(E.HTMLScriptElement), ['defer', ['noModule', 'nomodule']]);
Object.defineProperties(P(E.HTMLScriptElement), {
  // Script-created scripts are async unless set otherwise.
  async: gs(function () { return N.attr(this[ID], 'async') !== null || (this[CACHE] !== undefined && this[CACHE].dynamic === true); },
    function (v) {
      cached(this, 'dynamic', () => false);
      this[CACHE].dynamic = false;
      if (v) this.setAttribute('async', ''); else this.removeAttribute('async');
    }),
  text: gs(function () { return N.text(this[ID]); }, function (v) { this.textContent = v; }),
});
E.HTMLScriptElement.supports = function (t) { return t === 'classic' || t === 'module'; };
reflectUrl(P(E.HTMLLinkElement), ['href']);
reflect(P(E.HTMLLinkElement), ['rel', 'media', 'type', 'as', ['crossOrigin', 'crossorigin'], 'integrity', 'hreflang',
  ['referrerPolicy', 'referrerpolicy'], 'imagesrcset', 'imagesizes', ['fetchPriority', 'fetchpriority'], 'blocking']);
reflectBool(P(E.HTMLLinkElement), ['disabled']);
Object.defineProperties(P(E.HTMLLinkElement), {
  relList: ro(function () { return tokenList(this, 'rel'); }),
  sizes: ro(function () { return tokenList(this, 'sizes'); }),
  sheet: ro(function () { return /(^|\s)stylesheet(\s|$)/i.test(this.rel) && this.isConnected ? styleSheetFor(this) : null; }),
});
reflect(P(E.HTMLStyleElement), ['media', 'type', 'blocking']);
reflectBool(P(E.HTMLStyleElement), ['disabled']);
defProp(P(E.HTMLStyleElement), 'sheet', ro(function () { return this.isConnected ? styleSheetFor(this) : null; }));
reflect(P(E.HTMLMetaElement), ['name', 'content', ['httpEquiv', 'http-equiv'], 'charset', 'media', 'scheme']);
defProp(P(E.HTMLTitleElement), 'text', gs(function () { return this.textContent; }, function (v) { this.textContent = v; }));

// Embedded content. Frames are never loaded.
reflectUrl(P(E.HTMLIFrameElement), ['src']);
reflect(P(E.HTMLIFrameElement), ['name', 'width', 'height', 'allow', 'loading', 'srcdoc', ['referrerPolicy', 'referrerpolicy'],
  'align', 'scrolling', ['frameBorder', 'frameborder']]);
reflectBool(P(E.HTMLIFrameElement), [['allowFullscreen', 'allowfullscreen']]);
Object.defineProperties(P(E.HTMLIFrameElement), {
  sandbox: ro(function () { return tokenList(this, 'sandbox'); }),
  contentWindow: ro(() => null),
  contentDocument: ro(() => null),
});
reflectUrl(P(E.HTMLEmbedElement), ['src']);
reflect(P(E.HTMLEmbedElement), ['type', 'width', 'height']);
reflectUrl(P(E.HTMLObjectElement), ['data']);
reflect(P(E.HTMLObjectElement), ['type', 'width', 'height']);
defProp(P(E.HTMLObjectElement), 'contentDocument', ro(() => null));
reflectUrl(P(E.HTMLSourceElement), ['src']);
reflect(P(E.HTMLSourceElement), ['type', 'srcset', 'sizes', 'media', 'width', 'height']);
reflectUrl(P(E.HTMLTrackElement), ['src']);
reflect(P(E.HTMLTrackElement), ['kind', 'srclang', 'label']);

// Canvas: a 2D context that accepts every call and draws nothing.
reflectInt(P(E.HTMLCanvasElement), [['width', 'width', 300], ['height', 'height', 150]]);
P(E.HTMLCanvasElement).getContext = function (type) { return String(type) === '2d' ? canvasContext(this) : null; };
P(E.HTMLCanvasElement).toDataURL = function () { return 'data:,'; };
P(E.HTMLCanvasElement).toBlob = function (cb) { setTimeout(() => cb(null), 0); };
P(E.HTMLCanvasElement).captureStream = function () { return null; };
P(E.HTMLCanvasElement).transferControlToOffscreen = function () { throw domErr('NotSupportedError'); };
function canvasContext(canvas) {
  return cached(canvas, 'ctx2d', () => {
    const imageData = (w, h) => ({ width: w, height: h, data: new Uint8ClampedArray(Math.max(0, (w | 0) * (h | 0) * 4)) });
    const ctx = {
      canvas, fillStyle: '#000000', strokeStyle: '#000000', lineWidth: 1, font: '10px sans-serif',
      textAlign: 'start', textBaseline: 'alphabetic', globalAlpha: 1, globalCompositeOperation: 'source-over',
      lineCap: 'butt', lineJoin: 'miter', miterLimit: 10, shadowBlur: 0, shadowColor: 'rgba(0, 0, 0, 0)',
      shadowOffsetX: 0, shadowOffsetY: 0, imageSmoothingEnabled: true, direction: 'ltr', filter: 'none',
      measureText(t) {
        const m = /(\d+(?:\.\d+)?)px/.exec(this.font);
        const size = m ? parseFloat(m[1]) : 10;
        const w = String(t).length * size * 0.5;
        return { width: w, actualBoundingBoxLeft: 0, actualBoundingBoxRight: w, actualBoundingBoxAscent: size * 0.8,
          actualBoundingBoxDescent: size * 0.2, fontBoundingBoxAscent: size * 0.8, fontBoundingBoxDescent: size * 0.2 };
      },
      getImageData(x, y, w, h) { return imageData(w, h); },
      createImageData(w, h) { return imageData(w, h); },
      createLinearGradient() { return { addColorStop() {} }; },
      createRadialGradient() { return { addColorStop() {} }; },
      createConicGradient() { return { addColorStop() {} }; },
      createPattern() { return {}; },
      getTransform() { return new DOMMatrix(); },
      getLineDash() { return []; },
      isPointInPath() { return false; },
      isPointInStroke() { return false; },
      getContextAttributes() { return { alpha: true }; },
    };
    for (const m of ['save', 'restore', 'scale', 'rotate', 'translate', 'transform', 'setTransform', 'resetTransform',
      'clearRect', 'fillRect', 'strokeRect', 'beginPath', 'closePath', 'moveTo', 'lineTo', 'bezierCurveTo',
      'quadraticCurveTo', 'arc', 'arcTo', 'ellipse', 'rect', 'roundRect', 'fill', 'stroke', 'clip', 'fillText',
      'strokeText', 'drawImage', 'putImageData', 'setLineDash', 'drawFocusIfNeeded', 'reset']) ctx[m] = function () {};
    return ctx;
  });
}

// Media: never plays.
reflectUrl(P(E.HTMLMediaElement), ['src']);
reflect(P(E.HTMLMediaElement), ['preload', ['crossOrigin', 'crossorigin']]);
reflectBool(P(E.HTMLMediaElement), ['autoplay', 'loop', 'controls', ['defaultMuted', 'muted']]);
Object.defineProperties(P(E.HTMLMediaElement), {
  currentSrc: ro(function () { return this.src; }),
  paused: ro(() => true),
  ended: ro(() => false),
  seeking: ro(() => false),
  readyState: ro(() => 0),
  networkState: ro(() => 0),
  duration: ro(() => NaN),
  currentTime: { value: 0, writable: true, configurable: true },
  volume: { value: 1, writable: true, configurable: true },
  muted: { value: false, writable: true, configurable: true },
  playbackRate: { value: 1, writable: true, configurable: true },
  buffered: ro(() => ({ length: 0, start() { return 0; }, end() { return 0; } })),
  textTracks: ro(() => []),
  error: ro(() => null),
});
P(E.HTMLMediaElement).play = function () { return Promise.resolve(); };
for (const m of ['pause', 'load', 'fastSeek']) P(E.HTMLMediaElement)[m] = function () {};
P(E.HTMLMediaElement).canPlayType = function () { return ''; };
P(E.HTMLMediaElement).addTextTrack = function () { return {}; };
reflectUrl(P(E.HTMLVideoElement), ['poster']);
reflectInt(P(E.HTMLVideoElement), [['width', 'width', 0], ['height', 'height', 0]]);
reflectBool(P(E.HTMLVideoElement), [['playsInline', 'playsinline']]);
Object.defineProperties(P(E.HTMLVideoElement), { videoWidth: ro(() => 0), videoHeight: ro(() => 0) });
function Audio(src) { const a = document.createElement('audio'); if (src !== undefined) a.src = src; return a; }
Audio.prototype = P(E.HTMLAudioElement);
defProp(G, 'Audio', { value: Audio, writable: true, configurable: true });

// Other elements.
defProp(P(E.HTMLTemplateElement), 'content', ro(function () {
  const id = N.tmpl(this[ID]);
  if (id) templateContents.add(id);
  return wrap(id);
}));
reflectBool(P(E.HTMLDetailsElement), ['open']);
reflect(P(E.HTMLDetailsElement), ['name']);
reflectBool(P(E.HTMLDialogElement), ['open']);
P(E.HTMLDialogElement).show = function () { this.open = true; };
P(E.HTMLDialogElement).showModal = function () { this.open = true; };
P(E.HTMLDialogElement).close = function (v) {
  this.open = false;
  if (v !== undefined) this.returnValue = v;
  dispatch(this, new Event('close'));
};
P(E.HTMLDialogElement).returnValue = '';
P(E.HTMLSlotElement).assignedNodes = function (opts) {
  const ids = N.assigned(this[ID]);
  if (ids.length || !(opts && opts.flatten)) return ids.map(wrap);
  return N.kids(this[ID]).map(wrap);
};
P(E.HTMLSlotElement).assignedElements = function (opts) {
  return this.assignedNodes(opts).filter(n => n.nodeType === 1);
};
P(E.HTMLSlotElement).assign = function () {};
reflect(P(E.HTMLSlotElement), ['name']);
reflectInt(P(E.HTMLOListElement), [['start', 'start', 1]]);
reflectBool(P(E.HTMLOListElement), ['reversed']);
reflect(P(E.HTMLOListElement), ['type']);
reflectInt(P(E.HTMLLIElement), [['value', 'value', 0]]);
reflect(P(E.HTMLTimeElement), [['dateTime', 'datetime']]);
reflect(P(E.HTMLDataElement), ['value']);
reflect(P(E.HTMLQuoteElement), ['cite']);
for (const C of [E.HTMLProgressElement, E.HTMLMeterElement]) {
  defProp(P(C), 'value', gs(function () { return parseFloat(N.attr(this[ID], 'value')) || 0; }, function (v) { this.setAttribute('value', v); }));
  defProp(P(C), 'max', gs(function () { return parseFloat(N.attr(this[ID], 'max')) || 1; }, function (v) { this.setAttribute('max', v); }));
}

// Tables.
const childrenNamed = (el, names) => N.ekids(el[ID]).filter(k => names.indexOf(N.name(k)) >= 0);
reflectInt(P(E.HTMLTableCellElement), [['colSpan', 'colspan', 1], ['rowSpan', 'rowspan', 1]]);
reflect(P(E.HTMLTableCellElement), ['headers', 'scope', 'abbr', 'align', ['vAlign', 'valign'], 'width', 'height', ['bgColor', 'bgcolor']]);
defProp(P(E.HTMLTableCellElement), 'cellIndex', ro(function () {
  const p = N.parent(this[ID]);
  return p ? childrenNamed(wrap(p), ['td', 'th']).indexOf(this[ID]) : -1;
}));
Object.defineProperties(P(E.HTMLTableElement), {
  rows: ro(function () {
    const rows = [];
    for (const k of childrenNamed(this, ['thead', 'tbody', 'tfoot', 'tr'])) {
      if (N.name(k) === 'tr') rows.push(k); else rows.push(...childrenNamed(wrap(k), ['tr']));
    }
    return htmlCollection(rows);
  }),
  tBodies: ro(function () { return htmlCollection(childrenNamed(this, ['tbody'])); }),
  tHead: ro(function () { return wrap(childrenNamed(this, ['thead'])[0] || 0); }),
  tFoot: ro(function () { return wrap(childrenNamed(this, ['tfoot'])[0] || 0); }),
  caption: ro(function () { return wrap(childrenNamed(this, ['caption'])[0] || 0); }),
});
reflect(P(E.HTMLTableElement), ['border', 'width', 'align', 'summary', ['cellPadding', 'cellpadding'], ['cellSpacing', 'cellspacing'], ['bgColor', 'bgcolor']]);
P(E.HTMLTableElement).createTBody = function () { return this.appendChild(document.createElement('tbody')); };
P(E.HTMLTableElement).createTHead = function () { return this.tHead || this.insertBefore(document.createElement('thead'), this.firstChild); };
P(E.HTMLTableElement).createTFoot = function () { return this.tFoot || this.appendChild(document.createElement('tfoot')); };
P(E.HTMLTableElement).createCaption = function () { return this.caption || this.insertBefore(document.createElement('caption'), this.firstChild); };
P(E.HTMLTableElement).insertRow = function (i) { return (this.tBodies[0] || this.createTBody()).insertRow(i); };
P(E.HTMLTableElement).deleteRow = function (i) { const r = this.rows[i]; if (r) r.remove(); };
defProp(P(E.HTMLTableSectionElement), 'rows', ro(function () { return htmlCollection(childrenNamed(this, ['tr'])); }));
P(E.HTMLTableSectionElement).insertRow = function (i) {
  const tr = document.createElement('tr');
  const rows = this.rows;
  return this.insertBefore(tr, i === undefined || i < 0 || i >= rows.length ? null : rows[i]);
};
P(E.HTMLTableSectionElement).deleteRow = function (i) { const r = this.rows[i]; if (r) r.remove(); };
Object.defineProperties(P(E.HTMLTableRowElement), {
  cells: ro(function () { return htmlCollection(childrenNamed(this, ['td', 'th'])); }),
  rowIndex: ro(function () { const t = this.closest('table'); return t ? Array.prototype.indexOf.call(t.rows, this) : -1; }),
  sectionRowIndex: ro(function () { const p = this.parentNode; return p && p.rows ? Array.prototype.indexOf.call(p.rows, this) : -1; }),
});
P(E.HTMLTableRowElement).insertCell = function (i) {
  const td = document.createElement('td');
  const cells = this.cells;
  return this.insertBefore(td, i === undefined || i < 0 || i >= cells.length ? null : cells[i]);
};
P(E.HTMLTableRowElement).deleteCell = function (i) { const c = this.cells[i]; if (c) c.remove(); };
reflect(P(E.HTMLTableRowElement), ['align', ['vAlign', 'valign'], ['bgColor', 'bgcolor']]);
reflect(P(E.HTMLBodyElement), [['bgColor', 'bgcolor'], 'background', 'text', 'link', ['vLink', 'vlink'], ['aLink', 'alink']]);
addHandlerProps(P(E.HTMLBodyElement), ['beforeunload', 'hashchange', 'message', 'offline', 'online', 'pagehide',
  'pageshow', 'popstate', 'storage', 'unload']);
reflect(P(E.HTMLHtmlElement), ['version', 'manifest']);
for (const C of [E.HTMLDivElement, E.HTMLParagraphElement, E.HTMLHeadingElement]) reflect(P(C), ['align']);
reflect(P(E.HTMLFontElement), ['color', 'face', 'size']);
