// ------------------------------------------------------------------- URL --

const URL_PARTS = ['href', 'protocol', 'username', 'password', 'host', 'hostname', 'port', 'pathname', 'search', 'hash', 'origin'];

function formEncode(s) {
  return encodeURIComponent(s).replace(/%20/g, '+').replace(/[!'()~]/g, c => '%' + c.charCodeAt(0).toString(16).toUpperCase());
}
function formDecode(s) {
  try { return decodeURIComponent(s.replace(/\+/g, ' ')); } catch (e) { return s; }
}
class URLSearchParams {
  constructor(init) {
    defProp(this, '_list', { value: [], writable: true });
    defProp(this, '_url', { value: null, writable: true });
    if (init === undefined || init === null) return;
    if (typeof init === 'object') {
      if (init instanceof URLSearchParams) this._list = init._list.map(p => p.slice());
      else if (typeof init[Symbol.iterator] === 'function') for (const [k, v] of init) this._list.push([String(k), String(v)]);
      else for (const k of Object.keys(init)) this._list.push([k, String(init[k])]);
    } else {
      this._parse(String(init));
    }
  }
  _parse(s) {
    this._list = [];
    if (s.startsWith('?')) s = s.slice(1);
    for (const part of s.split('&')) {
      if (!part) continue;
      const i = part.indexOf('=');
      this._list.push(i >= 0 ? [formDecode(part.slice(0, i)), formDecode(part.slice(i + 1))] : [formDecode(part), '']);
    }
  }
  _update() { if (this._url) { const s = this.toString(); this._url._setPart('search', s ? '?' + s : ''); } }
  get size() { return this._list.length; }
  append(k, v) { this._list.push([String(k), String(v)]); this._update(); }
  delete(k, v) { k = String(k); this._list = this._list.filter(p => p[0] !== k || (v !== undefined && p[1] !== String(v))); this._update(); }
  get(k) { k = String(k); const p = this._list.find(p => p[0] === k); return p ? p[1] : null; }
  getAll(k) { k = String(k); return this._list.filter(p => p[0] === k).map(p => p[1]); }
  has(k, v) { k = String(k); return this._list.some(p => p[0] === k && (v === undefined || p[1] === String(v))); }
  set(k, v) {
    k = String(k); v = String(v);
    const i = this._list.findIndex(p => p[0] === k);
    if (i < 0) this._list.push([k, v]);
    else { this._list[i][1] = v; this._list = this._list.filter((p, j) => j <= i || p[0] !== k); }
    this._update();
  }
  sort() { this._list.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0)); this._update(); }
  forEach(cb, t) { for (const [k, v] of this._list) cb.call(t, v, k, this); }
  keys() { return this._list.map(p => p[0]).values(); }
  values() { return this._list.map(p => p[1]).values(); }
  entries() { return this._list.map(p => p.slice()).values(); }
  [Symbol.iterator]() { return this.entries(); }
  toString() { return this._list.map(p => formEncode(p[0]) + '=' + formEncode(p[1])).join('&'); }
}
expose(URLSearchParams);

// Parsing is done natively with the WHATWG `url` crate.
class URL {
  constructor(url, base) {
    let b = '';
    if (base !== undefined && base !== null) {
      const bp = N.url(String(base), 'about:invalid-base');
      if (!bp) throw new TypeError("Failed to construct 'URL': Invalid base URL");
      b = bp[0];
    }
    const p = N.url(String(url), b || 'about:invalid-base');
    if (!p) throw new TypeError("Failed to construct 'URL': Invalid URL");
    defProp(this, '_p', { value: p, writable: true });
    defProp(this, '_sp', { value: null, writable: true });
  }
  static canParse(url, base) { try { new URL(url, base); return true; } catch (e) { return false; } }
  static parse(url, base) { try { return new URL(url, base); } catch (e) { return null; } }
  static createObjectURL() { return 'blob:' + N.docUrl() + '#' + Math.random().toString(36).slice(2); }
  static revokeObjectURL() {}
  _setPart(k, v) {
    const p = N.urlSet(this._p[0], k, String(v));
    if (p) this._p = p;
  }
  get searchParams() {
    if (!this._sp) { this._sp = new URLSearchParams(this._p[8]); this._sp._url = this; }
    return this._sp;
  }
  toString() { return this._p[0]; }
  toJSON() { return this._p[0]; }
}
URL_PARTS.forEach((k, i) => {
  defProp(URL.prototype, k, {
    get() { return this._p[i]; },
    set(v) {
      if (k === 'origin') return;
      if (k === 'href') {
        const p = N.url(String(v), 'about:invalid-base');
        if (!p) throw new TypeError("Failed to set the 'href' property on 'URL': Invalid URL");
        this._p = p;
      } else {
        this._setPart(k, v);
      }
      if (this._sp && (k === 'href' || k === 'search')) this._sp._parse(this._p[8]);
    },
    configurable: true,
    enumerable: true,
  });
});
expose(URL);
defProp(G, 'webkitURL', { value: URL, writable: true, configurable: true });

// ------------------------------------------------------------- encoding --

// UTF-8 and the legacy encodings run natively (`N.utf8Encode`, `N.decode`).
class TextEncoder {
  get encoding() { return 'utf-8'; }
  encode(s = '') { return N.utf8Encode(String(s)); }
  encodeInto(s, dest) {
    s = String(s);
    if (!(dest instanceof Uint8Array)) throw new TypeError("Failed to execute 'encodeInto' on 'TextEncoder': parameter 2 is not of type 'Uint8Array'.");
    const b = N.utf8Encode(s);
    if (b.length <= dest.length) { dest.set(b); return { read: s.length, written: b.length }; }
    // Only whole characters fit.
    let read = 0, written = 0;
    while (read < s.length) {
      const c = s.codePointAt(read);
      const n = c < 0x80 ? 1 : c < 0x800 ? 2 : c < 0x10000 ? 3 : 4;
      if (written + n > dest.length) break;
      read += c > 0xffff ? 2 : 1;
      written += n;
    }
    dest.set(b.subarray(0, written));
    return { read, written };
  }
}
expose(TextEncoder);
const TD = Symbol('huginn.decoder');
class TextDecoder {
  constructor(label = 'utf-8', options = undefined) {
    const enc = N.encLabel(String(label));
    if (enc === null) throw new RangeError(`Failed to construct 'TextDecoder': The encoding label provided ('${label}') is invalid.`);
    const o = options == null ? {} : options;
    defProp(this, TD, { value: { enc, fatal: !!o.fatal, ignoreBOM: !!o.ignoreBOM, pending: null, bomSeen: false } });
  }
  get encoding() { return this[TD].enc; }
  get fatal() { return this[TD].fatal; }
  get ignoreBOM() { return this[TD].ignoreBOM; }
  // With `{stream: true}`, an incomplete character at the end of `input` is
  // kept for the next call.
  decode(input = undefined, options = undefined) {
    const st = this[TD];
    const stream = !!(options && options.stream);
    let bytes;
    if (input === undefined) bytes = new Uint8Array(0);
    else if (input instanceof ArrayBuffer || (typeof SharedArrayBuffer === 'function' && input instanceof SharedArrayBuffer)) bytes = new Uint8Array(input);
    else if (ArrayBuffer.isView(input)) bytes = new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
    else throw new TypeError("Failed to execute 'decode' on 'TextDecoder': The provided value is not of type '(ArrayBuffer or ArrayBufferView)'.");
    if (st.pending !== null) {
      const b = new Uint8Array(st.pending.length + bytes.length);
      b.set(st.pending);
      b.set(bytes, st.pending.length);
      bytes = b;
      st.pending = null;
    }
    let s;
    try {
      s = N.decode(bytes, st.enc, st.fatal, stream);
    } catch (e) {
      st.bomSeen = false;
      throw new TypeError("Failed to execute 'decode' on 'TextDecoder': The encoded data was not valid.");
    }
    if (typeof s !== 'string') { st.pending = bytes.slice(bytes.length - s[1]); s = s[0]; }
    if (!st.bomSeen && s.length) {
      st.bomSeen = true;
      if (!st.ignoreBOM && s.charCodeAt(0) === 0xfeff && /^utf-(8|16[lb]e)$/.test(st.enc)) s = s.slice(1);
    }
    if (!stream) { st.bomSeen = false; st.pending = null; }
    return s;
  }
}
expose(TextDecoder);

const B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
function btoa(s) {
  s = String(s);
  let out = '';
  for (let i = 0; i < s.length; i += 3) {
    const a = s.charCodeAt(i), b = s.charCodeAt(i + 1), c = s.charCodeAt(i + 2);
    if (a > 255 || b > 255 || c > 255) throw domErr('InvalidCharacterError', 'The string to be encoded contains characters outside of the Latin1 range.');
    const n = (a << 16) | ((b || 0) << 8) | (c || 0);
    out += B64[n >> 18] + B64[(n >> 12) & 63] + (i + 1 < s.length ? B64[(n >> 6) & 63] : '=') + (i + 2 < s.length ? B64[n & 63] : '=');
  }
  return out;
}
function atob(s) {
  s = String(s).replace(/[\t\n\f\r ]/g, '');
  if (s.length % 4 === 0) s = s.replace(/==?$/, '');
  if (s.length % 4 === 1 || /[^A-Za-z0-9+/]/.test(s)) throw domErr('InvalidCharacterError', 'The string to be decoded is not correctly encoded.');
  let out = '';
  let bits = 0, acc = 0;
  for (let i = 0; i < s.length; i++) {
    acc = (acc << 6) | B64.indexOf(s[i]);
    bits += 6;
    if (bits >= 8) { bits -= 8; out += String.fromCharCode((acc >> bits) & 255); }
  }
  return out;
}

// ---------------------------------------------------------------- blobs --

function bytesOf(part) {
  if (part instanceof Blob) return part._bytes;
  if (part instanceof ArrayBuffer) return new Uint8Array(part);
  if (ArrayBuffer.isView(part)) return new Uint8Array(part.buffer, part.byteOffset, part.byteLength);
  return new TextEncoder().encode(String(part));
}
class Blob {
  constructor(parts, options) {
    const chunks = (parts || []).map(bytesOf);
    const total = chunks.reduce((n, c) => n + c.length, 0);
    const bytes = new Uint8Array(total);
    let off = 0;
    for (const c of chunks) { bytes.set(c, off); off += c.length; }
    defProp(this, '_bytes', { value: bytes });
    this.type = options && options.type ? String(options.type).toLowerCase() : '';
  }
  get size() { return this._bytes.length; }
  slice(start, end, type) { return new Blob([this._bytes.subarray(start, end)], { type }); }
  text() { return Promise.resolve(new TextDecoder().decode(this._bytes)); }
  arrayBuffer() { return Promise.resolve(this._bytes.slice().buffer); }
  bytes() { return Promise.resolve(this._bytes.slice()); }
  stream() { return streams().bytesStream(this._bytes.slice()); }
}
expose(Blob);
class File extends Blob {
  constructor(parts, name, options) { super(parts, options); this.name = String(name); this.lastModified = (options && options.lastModified) || Date.now(); }
}
expose(File);
class FileReader extends EventTarget {
  constructor() { super(); this.readyState = 0; this.result = null; this.error = null; }
  _read(blob, conv) {
    this.readyState = 1;
    queueTask(() => {
      this.result = conv(blob);
      this.readyState = 2;
      dispatch(this, new ProgressEvent('load'));
      dispatch(this, new ProgressEvent('loadend'));
    });
  }
  readAsText(b) { this._read(b, x => new TextDecoder().decode(x._bytes)); }
  readAsArrayBuffer(b) { this._read(b, x => x._bytes.slice().buffer); }
  readAsDataURL(b) {
    this._read(b, x => {
      let s = '';
      for (const c of x._bytes) s += String.fromCharCode(c);
      return 'data:' + (x.type || 'application/octet-stream') + ';base64,' + btoa(s);
    });
  }
  readAsBinaryString(b) { this._read(b, x => { let s = ''; for (const c of x._bytes) s += String.fromCharCode(c); return s; }); }
  abort() {}
}
addHandlerProps(FileReader.prototype, ['loadstart', 'loadend', 'progress']);
expose(FileReader);
class FormData {
  constructor(form) {
    defProp(this, '_list', { value: [] });
    if (form && form.elements) {
      for (const el of form.elements) {
        if (!el.name || el.disabled) continue;
        if ((el.type === 'checkbox' || el.type === 'radio') && !el.checked) continue;
        this._list.push([el.name, el.value]);
      }
    }
  }
  append(k, v) { this._list.push([String(k), v instanceof Blob ? v : String(v)]); }
  delete(k) { for (let i = this._list.length - 1; i >= 0; i--) if (this._list[i][0] === k) this._list.splice(i, 1); }
  get(k) { const p = this._list.find(p => p[0] === k); return p ? p[1] : null; }
  getAll(k) { return this._list.filter(p => p[0] === k).map(p => p[1]); }
  has(k) { return this._list.some(p => p[0] === k); }
  set(k, v) { this.delete(k); this.append(k, v); }
  forEach(cb, t) { for (const [k, v] of this._list) cb.call(t, v, k, this); }
  keys() { return this._list.map(p => p[0]).values(); }
  values() { return this._list.map(p => p[1]).values(); }
  entries() { return this._list.map(p => p.slice()).values(); }
  [Symbol.iterator]() { return this.entries(); }
}
expose(FormData);

// ---------------------------------------------------------- fetch and XHR --
// Requests are served synchronously by the shell's loader with the Fetch
// standard's rules: another origin must allow a `cors` request (a failed
// CORS check is a network error) and answers `no-cors` with an opaque
// response. The result is delivered in a later task, like a fast network.

// A request body as [string or Uint8Array, its default Content-Type].
function requestBody(b) {
  if (b === undefined || b === null) return [null, null];
  if (typeof b === 'string') return [b, 'text/plain;charset=UTF-8'];
  if (b instanceof URLSearchParams) return [String(b), 'application/x-www-form-urlencoded;charset=UTF-8'];
  if (b instanceof FormData) {
    const boundary = '----HuginnFormBoundary' + Math.random().toString(36).slice(2);
    const q = s => String(s).replace(/"/g, '%22').replace(/\r?\n|\r/g, '%0A');
    let s = '';
    for (const [k, v] of b) {
      s += '--' + boundary + '\r\nContent-Disposition: form-data; name="' + q(k) + '"';
      if (v instanceof Blob) {
        s += '; filename="' + q(v.name || 'blob') + '"\r\nContent-Type: ' + (v.type || 'application/octet-stream') + '\r\n\r\n' + new TextDecoder().decode(v._bytes) + '\r\n';
      } else s += '\r\n\r\n' + v + '\r\n';
    }
    return [s + '--' + boundary + '--\r\n', 'multipart/form-data; boundary=' + boundary];
  }
  if (b instanceof Blob) return [b._bytes, b.type || null];
  if (b instanceof ArrayBuffer) return [new Uint8Array(b), null];
  if (ArrayBuffer.isView(b)) return [new Uint8Array(b.buffer, b.byteOffset, b.byteLength), null];
  return [String(b), 'text/plain;charset=UTF-8'];
}

// Send a request; `headers` iterates [name, value] pairs. Returns
// { status, url, headers: [[name, value]...], body, type, redirected }, or
// null for a network error.
function nativeFetch(url, method, headers, body, mode, credentials, redirect) {
  let flat = '';
  if (headers) for (const [k, v] of headers) flat += String(k).replace(/\n/g, '') + '\n' + String(v).replace(/[\r\n]/g, ' ') + '\n';
  const r = N.fetch(String(url), String(method || 'GET'), flat, body === undefined ? null : body,
    mode || 'cors', credentials || 'same-origin', redirect || 'follow');
  if (!r) return null;
  const list = [];
  for (const line of r[2].split('\r\n')) {
    const i = line.indexOf(': ');
    if (i > 0) list.push([line.slice(0, i), line.slice(i + 2)]);
  }
  return { status: +r[0], url: r[1], headers: list, body: r[3], type: r[4], redirected: r[5] === '1' };
}
const STATUS_TEXT = { 200: 'OK', 201: 'Created', 204: 'No Content', 301: 'Moved Permanently', 302: 'Found', 304: 'Not Modified', 400: 'Bad Request', 401: 'Unauthorized', 403: 'Forbidden', 404: 'Not Found', 500: 'Internal Server Error' };

class Headers {
  constructor(init) {
    defProp(this, '_map', { value: new Map() });
    if (!init) return;
    if (init instanceof Headers) init.forEach((v, k) => this.append(k, v));
    else if (typeof init[Symbol.iterator] === 'function') for (const [k, v] of init) this.append(k, v);
    else for (const k of Object.keys(init)) this.append(k, init[k]);
  }
  append(k, v) { k = String(k).toLowerCase(); const o = this._map.get(k); this._map.set(k, o === undefined ? String(v) : o + ', ' + v); }
  set(k, v) { this._map.set(String(k).toLowerCase(), String(v)); }
  get(k) { const v = this._map.get(String(k).toLowerCase()); return v === undefined ? null : v; }
  getSetCookie() { return []; }
  has(k) { return this._map.has(String(k).toLowerCase()); }
  delete(k) { this._map.delete(String(k).toLowerCase()); }
  forEach(cb, t) { for (const [k, v] of this._map) cb.call(t, v, k, this); }
  keys() { return this._map.keys(); }
  values() { return this._map.values(); }
  entries() { return this._map.entries(); }
  [Symbol.iterator]() { return this._map.entries(); }
}
expose(Headers);

// A body's source (`_body`) is null, a string, a Uint8Array or a
// ReadableStream. Scripts that read `body` get a stream, made on first use;
// text() and friends read the source directly when no stream exists.
const BODY = Symbol('huginn.body');
const USED = Symbol('huginn.used');
function isReadableStream(v) { return streamsLib !== null && v instanceof streamsLib.ReadableStream; }
function bodyStream(b) {
  if (b[BODY] !== undefined) return b[BODY];
  return isReadableStream(b._body) ? b._body : null;
}
function extractBody(body) {
  if (body === undefined || body === null) return null;
  if (typeof body === 'string') return body;
  if (isReadableStream(body)) {
    if (streams().isDisturbed(body) || streams().isStreamLocked(body)) throw new TypeError('Response body object should not be disturbed or locked');
    return body;
  }
  if (body instanceof Blob) return body._bytes.slice();
  if (body instanceof ArrayBuffer) return new Uint8Array(body.slice(0));
  if (ArrayBuffer.isView(body)) return new Uint8Array(body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength));
  return String(body);
}
const bodyText = b => b === null ? '' : typeof b === 'string' ? b : new TextDecoder().decode(b);
const bodyBytes = b => b === null ? new Uint8Array(0) : typeof b === 'string' ? new TextEncoder().encode(b) : b;
class Body {
  get body() {
    const b = this._body;
    if (b === null) return null;
    let s = bodyStream(this);
    if (s === null) {
      // A body already read by text() and friends streams as empty.
      const bytes = this[USED] ? new Uint8Array(0) : typeof b === 'string' ? new TextEncoder().encode(b) : b.slice();
      s = streams().bytesStream(bytes);
      defProp(this, BODY, { value: s, writable: true, configurable: true });
    }
    return s;
  }
  get bodyUsed() {
    if (this[USED]) return true;
    const s = bodyStream(this);
    return s !== null && streams().isDisturbed(s);
  }
  // The whole body: null, a string or a Uint8Array.
  _consume() {
    if (this.bodyUsed) return Promise.reject(new TypeError('body stream already read'));
    const s = bodyStream(this);
    if (s !== null) {
      if (streams().isStreamLocked(s)) return Promise.reject(new TypeError('body stream is locked'));
      return streams().readAllBytes(s);
    }
    defProp(this, USED, { value: true });
    return Promise.resolve(this._body);
  }
  text() { return this._consume().then(bodyText); }
  json() { return this._consume().then(b => JSON.parse(bodyText(b))); }
  arrayBuffer() {
    return this._consume().then(b => {
      const u = bodyBytes(b);
      return u.byteOffset === 0 && u.byteLength === u.buffer.byteLength ? u.buffer : u.slice().buffer;
    });
  }
  bytes() { return this._consume().then(bodyBytes); }
  blob() { return this._consume().then(b => new Blob([bodyBytes(b)], { type: this.headers.get('content-type') || '' })); }
  formData() { return this._consume().then(b => { const f = new FormData(); new URLSearchParams(bodyText(b)).forEach((v, k) => f.append(k, v)); return f; }); }
}
class Request extends Body {
  constructor(input, init) {
    super();
    init = init || {};
    const src = input instanceof Request ? input : null;
    this.url = src ? src.url : new URL(String(input), N.baseUrl()).href;
    this.method = String(init.method || (src && src.method) || 'GET');
    // Only these methods are uppercased (`patch` stays `patch`, like Chrome).
    const upper = this.method.toUpperCase();
    if (['DELETE', 'GET', 'HEAD', 'OPTIONS', 'POST', 'PUT'].includes(upper)) this.method = upper;
    if (['CONNECT', 'TRACE', 'TRACK'].includes(upper)) throw new TypeError("'" + this.method + "' HTTP method is unsupported.");
    this.headers = new Headers(init.headers || (src && src.headers));
    this.signal = init.signal || (src && src.signal) || new AbortSignal();
    this.credentials = init.credentials || (src && src.credentials) || 'same-origin';
    this.mode = init.mode || (src && src.mode) || 'cors';
    if (this.mode === 'navigate') throw new TypeError("Cannot construct a Request with a RequestInit whose mode member is set as 'navigate'.");
    if (this.mode === 'no-cors' && !['GET', 'HEAD', 'POST'].includes(this.method)) throw new TypeError("'" + this.method + "' is unsupported in no-cors mode.");
    this.cache = init.cache || 'default';
    this.redirect = init.redirect || (src && src.redirect) || 'follow';
    this.referrer = 'about:client';
    this.referrerPolicy = init.referrerPolicy || '';
    this.integrity = init.integrity || '';
    this.keepalive = !!init.keepalive;
    const raw = init.body !== undefined ? init.body : src ? src._payload : null;
    if (raw !== null && raw !== undefined && (this.method === 'GET' || this.method === 'HEAD')) throw new TypeError('Request with GET/HEAD method cannot have body.');
    const [payload, type] = requestBody(raw);
    if (type && !this.headers.has('content-type')) this.headers.set('content-type', type);
    defProp(this, '_payload', { value: payload });
    defProp(this, '_body', { value: payload });
  }
  clone() { return new Request(this); }
}
expose(Request);
class Response extends Body {
  constructor(body, init) {
    super();
    init = init || {};
    this.status = init.status === undefined ? 200 : Number(init.status);
    this.statusText = init.statusText === undefined ? (STATUS_TEXT[this.status] || '') : String(init.statusText);
    this.headers = new Headers(init.headers);
    this.type = 'default';
    this.url = '';
    this.redirected = false;
    defProp(this, '_body', { value: extractBody(body), writable: true });
  }
  get ok() { return this.status >= 200 && this.status < 300; }
  clone() {
    if (this.bodyUsed) throw new TypeError("Failed to execute 'clone' on 'Response': Response body is already used");
    const r = new Response(null, this);
    r.url = this.url;
    r.type = this.type;
    const s = bodyStream(this);
    if (s !== null) {
      const [a, b] = streams().teeStream(s);
      defProp(this, BODY, { value: a, writable: true, configurable: true });
      r._body = b;
    } else {
      r._body = this._body instanceof Uint8Array ? this._body.slice() : this._body;
    }
    return r;
  }
  static error() { const r = new Response(null, { status: 0, statusText: '' }); r.type = 'error'; return r; }
  static json(data, init) {
    const r = new Response(JSON.stringify(data), init);
    if (!r.headers.has('content-type')) r.headers.set('content-type', 'application/json');
    return r;
  }
  static redirect(url, status) { return new Response(null, { status: status || 302, headers: { location: String(url) } }); }
}
expose(Response);

function fetch(input, init) {
  let req;
  try { req = new Request(input, init); } catch (e) { return Promise.reject(e); }
  return new Promise((resolve, reject) => {
    if (req.signal.aborted) { reject(req.signal.reason); return; }
    queueTask(() => {
      if (req.signal.aborted) { reject(req.signal.reason); return; }
      const r = nativeFetch(req.url, req.method, req.headers, req._payload, req.mode, req.credentials, req.redirect);
      if (!r) { reject(new TypeError('Failed to fetch')); return; }
      const empty = req.method === 'HEAD' || r.type === 'opaque' || r.type === 'opaqueredirect';
      const res = new Response(empty ? null : r.body, { status: r.status, headers: r.headers });
      res.url = r.url;
      res.type = r.type;
      res.redirected = r.redirected;
      resolve(res);
    });
  });
}

class XMLHttpRequest extends EventTarget {
  constructor() {
    super();
    this.readyState = 0;
    this.status = 0;
    this.statusText = '';
    this.responseType = '';
    this.responseURL = '';
    this.timeout = 0;
    this.withCredentials = false;
    this.upload = new EventTarget();
    defProp(this, '_req', { value: null, writable: true });
    defProp(this, '_res', { value: null, writable: true });
  }
  open(method, url, async) {
    let m = String(method);
    const upper = m.toUpperCase();
    if (['CONNECT', 'TRACE', 'TRACK'].includes(upper)) throw domErr('SecurityError', "'" + m + "' HTTP method is unsupported.");
    if (['DELETE', 'GET', 'HEAD', 'OPTIONS', 'POST', 'PUT'].includes(upper)) m = upper;
    this._req = { method: m, url: new URL(String(url), N.baseUrl()).href, async: async !== false, aborted: false, headers: [], body: null };
    this._res = null;
    this.status = 0;
    this._state(1);
  }
  setRequestHeader(name, value) {
    if (!this._req || this.readyState !== 1) throw domErr('InvalidStateError', "The object's state must be OPENED.");
    this._req.headers.push([String(name), String(value)]);
  }
  overrideMimeType() {}
  getResponseHeader(name) {
    if (!this._res) return null;
    const n = String(name).toLowerCase();
    const vals = this._res.headers.filter(h => h[0] === n).map(h => h[1]);
    return vals.length ? vals.join(', ') : null;
  }
  getAllResponseHeaders() {
    if (!this._res) return '';
    const m = new Map();
    for (const [k, v] of this._res.headers) m.set(k, m.has(k) ? m.get(k) + ', ' + v : v);
    return Array.from(m.keys()).sort().map(k => k + ': ' + m.get(k) + '\r\n').join('');
  }
  get responseText() {
    if (this.responseType !== '' && this.responseType !== 'text') throw domErr('InvalidStateError', 'responseText is only available if responseType is "" or "text"');
    return this._res ? this._res.body : '';
  }
  get responseXML() { return null; }
  get response() {
    if (!this._res || this.readyState !== 4) return this.responseType === '' || this.responseType === 'text' ? '' : null;
    const body = this._res.body;
    switch (this.responseType) {
      case 'json': try { return JSON.parse(body); } catch (e) { return null; }
      case 'arraybuffer': return new TextEncoder().encode(body).buffer;
      case 'blob': return new Blob([body], { type: this.getResponseHeader('content-type') || '' });
      case 'document': return detachedDocument(body);
      default: return body;
    }
  }
  _state(s) { this.readyState = s; dispatch(this, new Event('readystatechange')); }
  _finish() {
    const req = this._req;
    if (!req || req.aborted) return;
    const hasBody = req.method !== 'GET' && req.method !== 'HEAD';
    const [payload, type] = hasBody ? requestBody(req.body) : [null, null];
    const headers = req.headers.slice();
    if (type && !headers.some(h => h[0].toLowerCase() === 'content-type')) headers.push(['Content-Type', type]);
    const r = nativeFetch(req.url, req.method, headers, payload, 'cors', this.withCredentials ? 'include' : 'same-origin', 'follow');
    if (!r) {
      this._state(4);
      dispatch(this, new ProgressEvent('error'));
      dispatch(this, new ProgressEvent('loadend'));
      return;
    }
    this._res = r;
    this.status = r.status;
    this.statusText = STATUS_TEXT[r.status] || '';
    this.responseURL = r.url;
    this._state(2);
    this._state(3);
    this._state(4);
    const n = r.body.length;
    dispatch(this, new ProgressEvent('load', { lengthComputable: true, loaded: n, total: n }));
    dispatch(this, new ProgressEvent('loadend', { lengthComputable: true, loaded: n, total: n }));
  }
  send(body) {
    if (!this._req || this.readyState !== 1) throw domErr('InvalidStateError', "The object's state must be OPENED.");
    this._req.body = body === undefined ? null : body;
    dispatch(this, new ProgressEvent('loadstart'));
    if (this._req.async) queueTask(() => this._finish());
    else this._finish();
  }
  abort() {
    if (this._req) this._req.aborted = true;
    if (this.readyState > 0 && this.readyState < 4) {
      this._state(4);
      dispatch(this, new ProgressEvent('abort'));
    }
    this.readyState = 0;
  }
}
for (const [k, v] of [['UNSENT', 0], ['OPENED', 1], ['HEADERS_RECEIVED', 2], ['LOADING', 3], ['DONE', 4]]) {
  defProp(XMLHttpRequest, k, { value: v });
  defProp(XMLHttpRequest.prototype, k, { value: v });
}
addHandlerProps(XMLHttpRequest.prototype, ['readystatechange', 'loadstart', 'loadend', 'progress', 'timeout']);
expose(XMLHttpRequest);
expose(class XMLHttpRequestEventTarget extends EventTarget {}, 'XMLHttpRequestEventTarget');

// Connections that never open: scripts get their error/close events.
class WebSocket extends EventTarget {
  constructor(url) {
    super();
    this.url = String(url);
    this.readyState = 0;
    this.protocol = '';
    this.extensions = '';
    this.bufferedAmount = 0;
    this.binaryType = 'blob';
    queueTask(() => {
      this.readyState = 3;
      dispatch(this, new Event('error'));
      dispatch(this, new CloseEvent('close', { code: 1006, wasClean: false }));
    });
  }
  send() {}
  close() { this.readyState = 3; }
}
for (const [k, v] of [['CONNECTING', 0], ['OPEN', 1], ['CLOSING', 2], ['CLOSED', 3]]) defProp(WebSocket, k, { value: v });
addHandlerProps(WebSocket.prototype, ['open', 'message', 'close']);
expose(WebSocket);
const CloseEvent = eventClass('CloseEvent', Event, { code: 0, reason: '', wasClean: false });
class EventSource extends EventTarget {
  constructor(url) {
    super();
    this.url = String(url);
    this.readyState = 0;
    this.withCredentials = false;
    queueTask(() => { this.readyState = 2; dispatch(this, new Event('error')); });
  }
  close() { this.readyState = 2; }
}
addHandlerProps(EventSource.prototype, ['open', 'message']);
expose(EventSource);

// ------------------------------------------------------------- messaging --

class MessagePort extends EventTarget {
  constructor() { super(); defProp(this, '_other', { value: null, writable: true }); defProp(this, '_closed', { value: false, writable: true }); }
  postMessage(data) {
    const other = this._other;
    if (!other || this._closed) return;
    queueTask(() => { if (!other._closed) dispatch(other, new MessageEvent('message', { data })); });
  }
  start() {}
  close() { this._closed = true; }
}
addHandlerProps(MessagePort.prototype, ['message', 'messageerror']);
expose(MessagePort);
class MessageChannel {
  constructor() {
    this.port1 = new MessagePort();
    this.port2 = new MessagePort();
    this.port1._other = this.port2;
    this.port2._other = this.port1;
  }
}
expose(MessageChannel);
class BroadcastChannel extends EventTarget {
  constructor(name) { super(); this.name = String(name); }
  postMessage() {}
  close() {}
}
addHandlerProps(BroadcastChannel.prototype, ['message', 'messageerror']);
expose(BroadcastChannel);
