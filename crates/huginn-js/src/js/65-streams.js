// --------------------------------------------------------------- streams --
// WHATWG Streams (https://streams.spec.whatwg.org/): readable streams with
// default and byte controllers and readers, writable streams, transform
// streams, piping, tee, async iteration, queuing strategies, and the text
// encoding and compression streams. The algorithms follow the spec's
// abstract operations, named the same. Each public object keeps its internal
// slots in one record under the `SL` symbol.
//
// Built on first use: most pages never touch a stream, and the classes then
// cost no heap. The globals are accessors until then.

const STREAM_NAMES = ['ReadableStream', 'ReadableStreamDefaultReader', 'ReadableStreamBYOBReader',
  'ReadableStreamDefaultController', 'ReadableByteStreamController', 'ReadableStreamBYOBRequest',
  'WritableStream', 'WritableStreamDefaultWriter', 'WritableStreamDefaultController',
  'TransformStream', 'TransformStreamDefaultController', 'ByteLengthQueuingStrategy',
  'CountQueuingStrategy', 'TextEncoderStream', 'TextDecoderStream', 'CompressionStream',
  'DecompressionStream'];
let streamsLib = null;
const streamGetters = {};
function streams() {
  if (streamsLib) return streamsLib;
  streamsLib = makeStreams();
  for (const k of STREAM_NAMES) {
    const d = Object.getOwnPropertyDescriptor(G, k);
    if (d && d.get === streamGetters[k]) defProp(G, k, { value: streamsLib[k], writable: true, configurable: true });
  }
  return streamsLib;
}
for (const k of STREAM_NAMES) {
  streamGetters[k] = function () { return streams()[k]; };
  defProp(G, k, {
    get: streamGetters[k],
    set(v) { defProp(G, k, { value: v, writable: true, configurable: true }); },
    configurable: true,
  });
}

function makeStreams() {
const SL = Symbol('huginn.stream');
const noop = () => {};
const resolved = v => Promise.resolve(v);
const rejected = e => Promise.reject(e);
function setHandled(p) { p.then(undefined, noop); return p; }
// A promise with its settle functions and state (the spec inspects it).
function deferred() {
  const d = { state: 'pending', promise: null, resolve: null, reject: null };
  d.promise = new Promise((res, rej) => {
    d.resolve = v => { if (d.state === 'pending') { d.state = 'fulfilled'; res(v); } };
    d.reject = e => { if (d.state === 'pending') { d.state = 'rejected'; rej(e); } };
  });
  return d;
}
function rejectHandled(d, e) { setHandled(d.promise); d.reject(e); }
function rejectedDeferred(e) { const d = deferred(); rejectHandled(d, e); return d; }
function resolvedDeferred(v) { const d = deferred(); d.resolve(v); return d; }
function promiseCall(f, self, ...args) {
  try { return Promise.resolve(f.apply(self, args)); } catch (e) { return Promise.reject(e); }
}
function slot(o, kind) {
  const s = o !== null && (typeof o === 'object' || typeof o === 'function') ? o[SL] : undefined;
  if (!s || s.kind !== kind) throw new TypeError('Illegal invocation');
  return s;
}
// `slot` for methods that return a promise: brand errors reject.
function pslot(o, kind) { try { return slot(o, kind); } catch (e) { return null; } }
const illegal = () => rejected(new TypeError('Illegal invocation'));
function hide(o, rec) { rec.pub = o; defProp(o, SL, { value: rec }); return rec; }
function make(C, rec) { return hide(Object.create(C.prototype), rec); }

function isObj(v) { return v !== null && (typeof v === 'object' || typeof v === 'function'); }
function dict(v, what) {
  if (v === undefined || v === null) return {};
  if (!isObj(v)) throw new TypeError(`${what} is not an object`);
  return v;
}
function fnMember(o, name) {
  const v = o[name];
  if (v === undefined) return undefined;
  if (typeof v !== 'function') throw new TypeError(`${name} is not a function`);
  return v;
}
function extractHWM(strategy, dflt) {
  const v = strategy.highWaterMark;
  if (v === undefined) return dflt;
  const h = Number(v);
  if (Number.isNaN(h) || h < 0) throw new RangeError('Invalid highWaterMark');
  return h;
}
function extractSize(strategy) {
  const size = fnMember(strategy, 'size');
  return size === undefined ? () => 1 : chunk => size.call(undefined, chunk);
}
function strategyDict(s) {
  s = dict(s, 'strategy');
  return { highWaterMark: s.highWaterMark, size: fnMember(s, 'size') };
}

// Queue-with-sizes.
function resetQueue(c) { c.queue = []; c.queueTotalSize = 0; }
function enqueueValueWithSize(c, value, size) {
  size = Number(size);
  if (!(size >= 0) || size === Infinity) throw new RangeError('The chunk size must be a finite, non-negative number');
  c.queue.push({ value, size });
  c.queueTotalSize += size;
}
function dequeueValue(c) {
  const pair = c.queue.shift();
  c.queueTotalSize -= pair.size;
  if (c.queueTotalSize < 0) c.queueTotalSize = 0;
  return pair.value;
}

// ArrayBuffer transfer: the old buffer becomes detached, like in a browser.
const isDetached = b => b.detached === true;
function transferBuffer(b) {
  if (isDetached(b)) throw new TypeError('The ArrayBuffer is detached');
  return typeof b.transfer === 'function' ? b.transfer() : b.slice(0);
}
function isView(v) { return ArrayBuffer.isView(v); }

// ------------------------------------------------------ readable streams --

function initRS(o) {
  return hide(o, { kind: 'RS', state: 'readable', reader: undefined, storedError: undefined, disturbed: false, controller: undefined });
}
function isLocked(s) { return s.reader !== undefined; }

class ReadableStream {
  constructor(underlyingSource = undefined, strategy = undefined) {
    const src = dict(underlyingSource, 'underlyingSource');
    const st = strategyDict(strategy);
    const s = initRS(this);
    const type = src.type === undefined ? undefined : String(src.type);
    if (type === 'bytes') {
      if (st.size !== undefined) throw new RangeError('The strategy for a byte stream cannot have a size function');
      setUpByteControllerFromSource(s, underlyingSource, src, extractHWM(st, 0));
    } else {
      if (type !== undefined) throw new TypeError(`The provided value '${type}' is not a valid enum value of type ReadableStreamType.`);
      setUpDefaultControllerFromSource(s, underlyingSource, src, extractHWM(st, 1), extractSize(st));
    }
  }
  get locked() { return isLocked(slot(this, 'RS')); }
  cancel(reason) {
    const s = pslot(this, 'RS');
    if (!s) return illegal();
    if (isLocked(s)) return rejected(new TypeError('Cannot cancel a stream that already has a reader'));
    return readableStreamCancel(s, reason);
  }
  getReader(options = undefined) {
    const s = slot(this, 'RS');
    const mode = dict(options, 'options').mode;
    if (mode === undefined) return acquireDefaultReader(s).pub;
    if (String(mode) === 'byob') return acquireBYOBReader(s).pub;
    throw new TypeError(`The provided value '${mode}' is not a valid enum value of type ReadableStreamReaderMode.`);
  }
  pipeThrough(transform, options = undefined) {
    const s = slot(this, 'RS');
    if (!isObj(transform)) throw new TypeError('transform is not an object');
    const readable = transform.readable;
    const ws = slot(transform.writable, 'WS');
    slot(readable, 'RS');
    const o = pipeOptions(options);
    if (isLocked(s)) throw new TypeError('ReadableStream is locked');
    if (isWSLocked(ws)) throw new TypeError('WritableStream is locked');
    setHandled(readableStreamPipeTo(s, ws, o.preventClose, o.preventAbort, o.preventCancel, o.signal));
    return readable;
  }
  pipeTo(destination, options = undefined) {
    const s = pslot(this, 'RS');
    if (!s) return illegal();
    const ws = pslot(destination, 'WS');
    if (!ws) return rejected(new TypeError('The destination is not a WritableStream'));
    let o;
    try { o = pipeOptions(options); } catch (e) { return rejected(e); }
    if (isLocked(s)) return rejected(new TypeError('ReadableStream is locked'));
    if (isWSLocked(ws)) return rejected(new TypeError('WritableStream is locked'));
    return readableStreamPipeTo(s, ws, o.preventClose, o.preventAbort, o.preventCancel, o.signal);
  }
  tee() { return readableStreamTee(slot(this, 'RS')).map(b => b.pub); }
  values(options = undefined) {
    const s = slot(this, 'RS');
    const preventCancel = !!dict(options, 'options').preventCancel;
    return makeAsyncIterator(acquireDefaultReader(s), preventCancel);
  }
  static from(asyncIterable) { return readableStreamFromIterable(asyncIterable).pub; }
}
defProp(ReadableStream.prototype, Symbol.asyncIterator, { value: ReadableStream.prototype.values, writable: true, configurable: true });

function pipeOptions(options) {
  const o = dict(options, 'options');
  const signal = o.signal;
  if (signal !== undefined && !(signal instanceof AbortSignal)) throw new TypeError("Failed to read the 'signal' property: Failed to convert value to 'AbortSignal'.");
  return { preventAbort: !!o.preventAbort, preventCancel: !!o.preventCancel, preventClose: !!o.preventClose, signal };
}

function createReadableStream(start, pull, cancel, hwm = 1, size = () => 1) {
  const s = make(ReadableStream, { kind: 'RS', state: 'readable', reader: undefined, storedError: undefined, disturbed: false, controller: undefined });
  const c = make(ReadableStreamDefaultController, { kind: 'RSDC' });
  setUpDefaultController(s, c, start, pull, cancel, hwm, size);
  return s;
}
function createReadableByteStream(start, pull, cancel) {
  const s = make(ReadableStream, { kind: 'RS', state: 'readable', reader: undefined, storedError: undefined, disturbed: false, controller: undefined });
  const c = make(ReadableByteStreamController, { kind: 'RBSC' });
  setUpByteController(s, c, start, pull, cancel, 0, undefined);
  return s;
}

function readableStreamCancel(s, reason) {
  s.disturbed = true;
  if (s.state === 'closed') return resolved(undefined);
  if (s.state === 'errored') return rejected(s.storedError);
  readableStreamClose(s);
  const reader = s.reader;
  if (reader !== undefined && reader.kind === 'RSBR') {
    const reqs = reader.readIntoRequests;
    reader.readIntoRequests = [];
    for (const r of reqs) r.close(undefined);
  }
  return s.controller.cancelSteps(reason).then(noop);
}
function readableStreamClose(s) {
  s.state = 'closed';
  const reader = s.reader;
  if (reader === undefined) return;
  reader.closed.resolve(undefined);
  if (reader.kind === 'RSDR') {
    const reqs = reader.readRequests;
    reader.readRequests = [];
    for (const r of reqs) r.close();
  }
}
function readableStreamError(s, e) {
  s.state = 'errored';
  s.storedError = e;
  const reader = s.reader;
  if (reader === undefined) return;
  rejectHandled(reader.closed, e);
  if (reader.kind === 'RSDR') defaultReaderErrorReadRequests(reader, e);
  else byobReaderErrorReadIntoRequests(reader, e);
}
function fulfillReadRequest(s, chunk, done) {
  const r = s.reader.readRequests.shift();
  if (done) r.close(); else r.chunk(chunk);
}
function fulfillReadIntoRequest(s, chunk, done) {
  const r = s.reader.readIntoRequests.shift();
  if (done) r.close(chunk); else r.chunk(chunk);
}
const numReadRequests = s => s.reader.readRequests.length;
const numReadIntoRequests = s => s.reader.readIntoRequests.length;
const hasDefaultReader = s => s.reader !== undefined && s.reader.kind === 'RSDR';
const hasBYOBReader = s => s.reader !== undefined && s.reader.kind === 'RSBR';

// ------------------------------------------------------------- readers --

function readerGenericInitialize(r, s) {
  r.stream = s;
  s.reader = r;
  if (s.state === 'readable') r.closed = deferred();
  else if (s.state === 'closed') r.closed = resolvedDeferred(undefined);
  else r.closed = rejectedDeferred(s.storedError);
}
function readerGenericRelease(r) {
  const s = r.stream;
  const e = new TypeError('Reader was released and can no longer be used to monitor the stream\'s closedness');
  if (s.state === 'readable') rejectHandled(r.closed, e);
  else r.closed = rejectedDeferred(e);
  s.controller.releaseSteps();
  s.reader = undefined;
  r.stream = undefined;
}
function readerGenericCancel(r, reason) { return readableStreamCancel(r.stream, reason); }

function setUpDefaultReader(r, s) {
  if (isLocked(s)) throw new TypeError('ReadableStream is locked to a reader');
  readerGenericInitialize(r, s);
  r.readRequests = [];
}
function acquireDefaultReader(s) {
  const r = make(ReadableStreamDefaultReader, { kind: 'RSDR' });
  setUpDefaultReader(r, s);
  return r;
}
function defaultReaderRead(r, req) {
  const s = r.stream;
  s.disturbed = true;
  if (s.state === 'closed') req.close();
  else if (s.state === 'errored') req.error(s.storedError);
  else s.controller.pullSteps(req);
}
function defaultReaderRelease(r) {
  readerGenericRelease(r);
  defaultReaderErrorReadRequests(r, new TypeError('Reader was released'));
}
function defaultReaderErrorReadRequests(r, e) {
  const reqs = r.readRequests;
  r.readRequests = [];
  for (const q of reqs) q.error(e);
}
function readRequestFor(d) {
  return {
    chunk: value => d.resolve({ value, done: false }),
    close: () => d.resolve({ value: undefined, done: true }),
    error: e => d.reject(e),
  };
}

class ReadableStreamDefaultReader {
  constructor(stream) { setUpDefaultReader(hide(this, { kind: 'RSDR' }), slot(stream, 'RS')); }
  get closed() { const r = pslot(this, 'RSDR'); return r ? r.closed.promise : illegal(); }
  cancel(reason) {
    const r = pslot(this, 'RSDR');
    if (!r) return illegal();
    if (r.stream === undefined) return rejected(new TypeError('This readable stream reader has been released and cannot be used to cancel its previous owner stream'));
    return readerGenericCancel(r, reason);
  }
  read() {
    const r = pslot(this, 'RSDR');
    if (!r) return illegal();
    if (r.stream === undefined) return rejected(new TypeError('This readable stream reader has been released and cannot be used to read from its previous owner stream'));
    const d = deferred();
    defaultReaderRead(r, readRequestFor(d));
    return d.promise;
  }
  releaseLock() {
    const r = slot(this, 'RSDR');
    if (r.stream !== undefined) defaultReaderRelease(r);
  }
}

function setUpBYOBReader(r, s) {
  if (isLocked(s)) throw new TypeError('ReadableStream is locked to a reader');
  if (s.controller.kind !== 'RBSC') throw new TypeError('Cannot get a BYOB reader for a stream that is not a byte stream');
  readerGenericInitialize(r, s);
  r.readIntoRequests = [];
}
function acquireBYOBReader(s) {
  const r = make(ReadableStreamBYOBReader, { kind: 'RSBR' });
  setUpBYOBReader(r, s);
  return r;
}
function byobReaderRead(r, view, min, req) {
  const s = r.stream;
  s.disturbed = true;
  if (s.state === 'errored') req.error(s.storedError);
  else byteControllerPullInto(s.controller, view, min, req);
}
function byobReaderRelease(r) {
  readerGenericRelease(r);
  byobReaderErrorReadIntoRequests(r, new TypeError('Reader was released'));
}
function byobReaderErrorReadIntoRequests(r, e) {
  const reqs = r.readIntoRequests;
  r.readIntoRequests = [];
  for (const q of reqs) q.error(e);
}

class ReadableStreamBYOBReader {
  constructor(stream) { setUpBYOBReader(hide(this, { kind: 'RSBR' }), slot(stream, 'RS')); }
  get closed() { const r = pslot(this, 'RSBR'); return r ? r.closed.promise : illegal(); }
  cancel(reason) {
    const r = pslot(this, 'RSBR');
    if (!r) return illegal();
    if (r.stream === undefined) return rejected(new TypeError('This readable stream reader has been released and cannot be used to cancel its previous owner stream'));
    return readerGenericCancel(r, reason);
  }
  read(view, options = undefined) {
    const r = pslot(this, 'RSBR');
    if (!r) return illegal();
    if (!isView(view)) return rejected(new TypeError("Failed to execute 'read': parameter 1 is not of type 'ArrayBufferView'."));
    let min;
    try {
      const o = dict(options, 'options');
      min = o.min === undefined ? 1 : Math.trunc(Number(o.min));
      if (!(min >= 0) || min > Number.MAX_SAFE_INTEGER) throw new TypeError('min is out of range');
    } catch (e) { return rejected(e); }
    if (view.byteLength === 0) return rejected(new TypeError('view must have non-zero byteLength'));
    if (view.buffer.byteLength === 0) return rejected(new TypeError("view's buffer must have non-zero byteLength"));
    if (isDetached(view.buffer)) return rejected(new TypeError("view's buffer has been detached"));
    if (min === 0) return rejected(new TypeError('options.min must be greater than 0'));
    if (view instanceof DataView ? min > view.byteLength : min > view.length) return rejected(new RangeError('options.min must be less than or equal to the view\'s length'));
    if (r.stream === undefined) return rejected(new TypeError('This readable stream reader has been released and cannot be used to read from its previous owner stream'));
    const d = deferred();
    byobReaderRead(r, view, min, {
      chunk: value => d.resolve({ value, done: false }),
      close: value => d.resolve({ value, done: true }),
      error: e => d.reject(e),
    });
    return d.promise;
  }
  releaseLock() {
    const r = slot(this, 'RSBR');
    if (r.stream !== undefined) byobReaderRelease(r);
  }
}

// ----------------------------------------------- default controller --

class ReadableStreamDefaultController {
  constructor() { throw new TypeError('Illegal constructor'); }
  get desiredSize() { return defaultControllerGetDesiredSize(slot(this, 'RSDC')); }
  close() {
    const c = slot(this, 'RSDC');
    if (!defaultControllerCanCloseOrEnqueue(c)) throw new TypeError('The stream is not in a state that permits close');
    defaultControllerClose(c);
  }
  enqueue(chunk) {
    const c = slot(this, 'RSDC');
    if (!defaultControllerCanCloseOrEnqueue(c)) throw new TypeError('The stream is not in a state that permits enqueue');
    defaultControllerEnqueue(c, chunk);
  }
  error(e) { defaultControllerError(slot(this, 'RSDC'), e); }
}

function setUpDefaultController(s, c, start, pull, cancel, hwm, size) {
  c.stream = s;
  resetQueue(c);
  c.started = c.closeRequested = c.pullAgain = c.pulling = false;
  c.strategySizeAlgorithm = size;
  c.strategyHWM = hwm;
  c.pullAlgorithm = pull;
  c.cancelAlgorithm = cancel;
  c.cancelSteps = reason => {
    resetQueue(c);
    const r = c.cancelAlgorithm(reason);
    defaultControllerClearAlgorithms(c);
    return r;
  };
  c.pullSteps = req => {
    if (c.queue.length > 0) {
      const chunk = dequeueValue(c);
      if (c.closeRequested && c.queue.length === 0) {
        defaultControllerClearAlgorithms(c);
        readableStreamClose(s);
      } else {
        defaultControllerCallPullIfNeeded(c);
      }
      req.chunk(chunk);
    } else {
      s.reader.readRequests.push(req);
      defaultControllerCallPullIfNeeded(c);
    }
  };
  c.releaseSteps = noop;
  s.controller = c;
  const startResult = start();
  resolved(startResult).then(() => {
    c.started = true;
    defaultControllerCallPullIfNeeded(c);
  }, r => defaultControllerError(c, r));
}
function setUpDefaultControllerFromSource(s, source, src, hwm, size) {
  const c = make(ReadableStreamDefaultController, { kind: 'RSDC' });
  const start = fnMember(src, 'start'), pull = fnMember(src, 'pull'), cancel = fnMember(src, 'cancel');
  setUpDefaultController(s, c,
    () => start === undefined ? undefined : start.call(source, c.pub),
    () => pull === undefined ? resolved(undefined) : promiseCall(pull, source, c.pub),
    reason => cancel === undefined ? resolved(undefined) : promiseCall(cancel, source, reason),
    hwm, size);
}
function defaultControllerCallPullIfNeeded(c) {
  if (!defaultControllerShouldCallPull(c)) return;
  if (c.pulling) { c.pullAgain = true; return; }
  c.pulling = true;
  c.pullAlgorithm().then(() => {
    c.pulling = false;
    if (c.pullAgain) { c.pullAgain = false; defaultControllerCallPullIfNeeded(c); }
  }, e => defaultControllerError(c, e));
}
function defaultControllerShouldCallPull(c) {
  const s = c.stream;
  if (!defaultControllerCanCloseOrEnqueue(c) || !c.started) return false;
  if (isLocked(s) && hasDefaultReader(s) && numReadRequests(s) > 0) return true;
  return defaultControllerGetDesiredSize(c) > 0;
}
function defaultControllerClearAlgorithms(c) {
  c.pullAlgorithm = undefined;
  c.cancelAlgorithm = undefined;
  c.strategySizeAlgorithm = undefined;
}
function defaultControllerClose(c) {
  if (!defaultControllerCanCloseOrEnqueue(c)) return;
  c.closeRequested = true;
  if (c.queue.length === 0) {
    defaultControllerClearAlgorithms(c);
    readableStreamClose(c.stream);
  }
}
function defaultControllerEnqueue(c, chunk) {
  if (!defaultControllerCanCloseOrEnqueue(c)) return;
  const s = c.stream;
  if (isLocked(s) && numReadRequests(s) > 0) {
    fulfillReadRequest(s, chunk, false);
  } else {
    let size;
    try { size = c.strategySizeAlgorithm(chunk); } catch (e) { defaultControllerError(c, e); throw e; }
    try { enqueueValueWithSize(c, chunk, size); } catch (e) { defaultControllerError(c, e); throw e; }
  }
  defaultControllerCallPullIfNeeded(c);
}
function defaultControllerError(c, e) {
  const s = c.stream;
  if (s.state !== 'readable') return;
  resetQueue(c);
  defaultControllerClearAlgorithms(c);
  readableStreamError(s, e);
}
function defaultControllerGetDesiredSize(c) {
  const st = c.stream.state;
  if (st === 'errored') return null;
  if (st === 'closed') return 0;
  return c.strategyHWM - c.queueTotalSize;
}
function defaultControllerHasBackpressure(c) { return !defaultControllerShouldCallPull(c); }
function defaultControllerCanCloseOrEnqueue(c) { return !c.closeRequested && c.stream.state === 'readable'; }

// -------------------------------------------------- byte controller --

class ReadableByteStreamController {
  constructor() { throw new TypeError('Illegal constructor'); }
  get byobRequest() { const r = byteControllerGetBYOBRequest(slot(this, 'RBSC')); return r === null ? null : r.pub; }
  get desiredSize() { return byteControllerGetDesiredSize(slot(this, 'RBSC')); }
  close() {
    const c = slot(this, 'RBSC');
    if (c.closeRequested) throw new TypeError('The stream has already been closed; do not close it again!');
    if (c.stream.state !== 'readable') throw new TypeError('The stream is not in the readable state and cannot be closed');
    byteControllerClose(c);
  }
  enqueue(chunk) {
    const c = slot(this, 'RBSC');
    if (!isView(chunk)) throw new TypeError("Failed to execute 'enqueue': parameter 1 is not of type 'ArrayBufferView'.");
    if (chunk.byteLength === 0) throw new TypeError('chunk must have non-zero byteLength');
    if (chunk.buffer.byteLength === 0) throw new TypeError("chunk's buffer must have non-zero byteLength");
    if (c.closeRequested) throw new TypeError('stream is closed or draining');
    if (c.stream.state !== 'readable') throw new TypeError('The stream is not in the readable state and cannot be enqueued to');
    byteControllerEnqueue(c, chunk);
  }
  error(e) { byteControllerError(slot(this, 'RBSC'), e); }
}

class ReadableStreamBYOBRequest {
  constructor() { throw new TypeError('Illegal constructor'); }
  get view() { return slot(this, 'BYOBREQ').view; }
  respond(bytesWritten) {
    const r = slot(this, 'BYOBREQ');
    bytesWritten = Math.trunc(Number(bytesWritten));
    if (!(bytesWritten >= 0)) throw new TypeError('bytesWritten is out of range');
    if (r.controller === undefined) throw new TypeError('This BYOB request has been invalidated');
    if (isDetached(r.view.buffer)) throw new TypeError('The BYOB request\'s buffer has been detached and so cannot be used as a response');
    byteControllerRespond(r.controller, bytesWritten);
  }
  respondWithNewView(view) {
    const r = slot(this, 'BYOBREQ');
    if (!isView(view)) throw new TypeError("parameter 1 is not of type 'ArrayBufferView'.");
    if (r.controller === undefined) throw new TypeError('This BYOB request has been invalidated');
    if (isDetached(view.buffer)) throw new TypeError('The given view\'s buffer has been detached and so cannot be used as a response');
    byteControllerRespondWithNewView(r.controller, view);
  }
}

function setUpByteController(s, c, start, pull, cancel, hwm, autoAllocateChunkSize) {
  c.stream = s;
  c.pullAgain = c.pulling = false;
  c.byobRequest = null;
  resetQueue(c);
  c.closeRequested = c.started = false;
  c.strategyHWM = hwm;
  c.pullAlgorithm = pull;
  c.cancelAlgorithm = cancel;
  c.autoAllocateChunkSize = autoAllocateChunkSize;
  c.pendingPullIntos = [];
  c.cancelSteps = reason => {
    byteControllerClearPendingPullIntos(c);
    resetQueue(c);
    const r = c.cancelAlgorithm(reason);
    byteControllerClearAlgorithms(c);
    return r;
  };
  c.pullSteps = req => {
    if (c.queueTotalSize > 0) { byteControllerFillReadRequestFromQueue(c, req); return; }
    const size = c.autoAllocateChunkSize;
    if (size !== undefined) {
      let buffer;
      try { buffer = new ArrayBuffer(size); } catch (e) { req.error(e); return; }
      c.pendingPullIntos.push({ buffer, bufferByteLength: size, byteOffset: 0, byteLength: size, bytesFilled: 0,
        minimumFill: 1, elementSize: 1, viewConstructor: Uint8Array, readerType: 'default' });
    }
    s.reader.readRequests.push(req);
    byteControllerCallPullIfNeeded(c);
  };
  c.releaseSteps = () => {
    if (c.pendingPullIntos.length > 0) {
      const first = c.pendingPullIntos[0];
      first.readerType = 'none';
      c.pendingPullIntos = [first];
    }
  };
  s.controller = c;
  const startResult = start();
  resolved(startResult).then(() => {
    c.started = true;
    byteControllerCallPullIfNeeded(c);
  }, r => byteControllerError(c, r));
}
function setUpByteControllerFromSource(s, source, src, hwm) {
  const c = make(ReadableByteStreamController, { kind: 'RBSC' });
  const start = fnMember(src, 'start'), pull = fnMember(src, 'pull'), cancel = fnMember(src, 'cancel');
  let auto = src.autoAllocateChunkSize;
  if (auto !== undefined) {
    auto = Math.trunc(Number(auto));
    if (!(auto >= 0) || auto > Number.MAX_SAFE_INTEGER) throw new TypeError('autoAllocateChunkSize is out of range');
    if (auto === 0) throw new TypeError('autoAllocateChunkSize must be greater than 0');
  }
  setUpByteController(s, c,
    () => start === undefined ? undefined : start.call(source, c.pub),
    () => pull === undefined ? resolved(undefined) : promiseCall(pull, source, c.pub),
    reason => cancel === undefined ? resolved(undefined) : promiseCall(cancel, source, reason),
    hwm, auto);
}
function byteControllerGetBYOBRequest(c) {
  if (c.byobRequest === null && c.pendingPullIntos.length > 0) {
    const first = c.pendingPullIntos[0];
    const view = new Uint8Array(first.buffer, first.byteOffset + first.bytesFilled, first.byteLength - first.bytesFilled);
    c.byobRequest = make(ReadableStreamBYOBRequest, { kind: 'BYOBREQ', controller: c, view });
  }
  return c.byobRequest;
}
function byteControllerGetDesiredSize(c) {
  const st = c.stream.state;
  if (st === 'errored') return null;
  if (st === 'closed') return 0;
  return c.strategyHWM - c.queueTotalSize;
}
function byteControllerCallPullIfNeeded(c) {
  if (!byteControllerShouldCallPull(c)) return;
  if (c.pulling) { c.pullAgain = true; return; }
  c.pulling = true;
  c.pullAlgorithm().then(() => {
    c.pulling = false;
    if (c.pullAgain) { c.pullAgain = false; byteControllerCallPullIfNeeded(c); }
  }, e => byteControllerError(c, e));
}
function byteControllerShouldCallPull(c) {
  const s = c.stream;
  if (s.state !== 'readable' || c.closeRequested || !c.started) return false;
  if (hasDefaultReader(s) && numReadRequests(s) > 0) return true;
  if (hasBYOBReader(s) && numReadIntoRequests(s) > 0) return true;
  return byteControllerGetDesiredSize(c) > 0;
}
function byteControllerClearAlgorithms(c) { c.pullAlgorithm = undefined; c.cancelAlgorithm = undefined; }
function byteControllerClearPendingPullIntos(c) { byteControllerInvalidateBYOBRequest(c); c.pendingPullIntos = []; }
function byteControllerInvalidateBYOBRequest(c) {
  if (c.byobRequest === null) return;
  c.byobRequest.controller = undefined;
  c.byobRequest.view = null;
  c.byobRequest = null;
}
function byteControllerClose(c) {
  const s = c.stream;
  if (c.closeRequested || s.state !== 'readable') return;
  if (c.queueTotalSize > 0) { c.closeRequested = true; return; }
  if (c.pendingPullIntos.length > 0) {
    const first = c.pendingPullIntos[0];
    if (first.bytesFilled % first.elementSize !== 0) {
      const e = new TypeError('Insufficient bytes to fill elements in the given buffer');
      byteControllerError(c, e);
      throw e;
    }
  }
  byteControllerClearAlgorithms(c);
  readableStreamClose(s);
}
function byteControllerEnqueue(c, chunk) {
  const s = c.stream;
  if (c.closeRequested || s.state !== 'readable') return;
  const { buffer, byteOffset, byteLength } = chunk;
  if (isDetached(buffer)) throw new TypeError('chunk\'s buffer is detached and so cannot be enqueued');
  const transferred = transferBuffer(buffer);
  if (c.pendingPullIntos.length > 0) {
    const first = c.pendingPullIntos[0];
    if (isDetached(first.buffer)) throw new TypeError('The BYOB request\'s buffer has been detached and so cannot be filled with an enqueued chunk');
    byteControllerInvalidateBYOBRequest(c);
    first.buffer = transferBuffer(first.buffer);
    if (first.readerType === 'none') byteControllerEnqueueDetachedPullIntoToQueue(c, first);
  }
  if (hasDefaultReader(s)) {
    byteControllerProcessReadRequestsUsingQueue(c);
    if (numReadRequests(s) === 0) {
      byteControllerEnqueueChunkToQueue(c, transferred, byteOffset, byteLength);
    } else {
      if (c.pendingPullIntos.length > 0) byteControllerShiftPendingPullInto(c);
      fulfillReadRequest(s, new Uint8Array(transferred, byteOffset, byteLength), false);
    }
  } else if (hasBYOBReader(s)) {
    byteControllerEnqueueChunkToQueue(c, transferred, byteOffset, byteLength);
    for (const p of byteControllerProcessPullIntoDescriptorsUsingQueue(c)) byteControllerCommitPullIntoDescriptor(s, p);
  } else {
    byteControllerEnqueueChunkToQueue(c, transferred, byteOffset, byteLength);
  }
  byteControllerCallPullIfNeeded(c);
}
function byteControllerEnqueueChunkToQueue(c, buffer, byteOffset, byteLength) {
  c.queue.push({ buffer, byteOffset, byteLength });
  c.queueTotalSize += byteLength;
}
function byteControllerEnqueueClonedChunkToQueue(c, buffer, byteOffset, byteLength) {
  let clone;
  try { clone = buffer.slice(byteOffset, byteOffset + byteLength); } catch (e) { byteControllerError(c, e); throw e; }
  byteControllerEnqueueChunkToQueue(c, clone, 0, byteLength);
}
function byteControllerEnqueueDetachedPullIntoToQueue(c, p) {
  if (p.bytesFilled > 0) byteControllerEnqueueClonedChunkToQueue(c, p.buffer, p.byteOffset, p.bytesFilled);
  byteControllerShiftPendingPullInto(c);
}
function byteControllerError(c, e) {
  const s = c.stream;
  if (s.state !== 'readable') return;
  byteControllerClearPendingPullIntos(c);
  resetQueue(c);
  byteControllerClearAlgorithms(c);
  readableStreamError(s, e);
}
function byteControllerFillReadRequestFromQueue(c, req) {
  const entry = c.queue.shift();
  c.queueTotalSize -= entry.byteLength;
  byteControllerHandleQueueDrain(c);
  req.chunk(new Uint8Array(entry.buffer, entry.byteOffset, entry.byteLength));
}
function byteControllerHandleQueueDrain(c) {
  if (c.queueTotalSize === 0 && c.closeRequested) {
    byteControllerClearAlgorithms(c);
    readableStreamClose(c.stream);
  } else {
    byteControllerCallPullIfNeeded(c);
  }
}
function byteControllerProcessReadRequestsUsingQueue(c) {
  const reader = c.stream.reader;
  while (reader.readRequests.length > 0) {
    if (c.queueTotalSize === 0) return;
    byteControllerFillReadRequestFromQueue(c, reader.readRequests.shift());
  }
}
function byteControllerProcessPullIntoDescriptorsUsingQueue(c) {
  const filled = [];
  while (c.pendingPullIntos.length > 0) {
    if (c.queueTotalSize === 0) break;
    const p = c.pendingPullIntos[0];
    if (byteControllerFillPullIntoDescriptorFromQueue(c, p)) {
      byteControllerShiftPendingPullInto(c);
      filled.push(p);
    }
  }
  return filled;
}
function byteControllerFillPullIntoDescriptorFromQueue(c, p) {
  const maxBytesToCopy = Math.min(c.queueTotalSize, p.byteLength - p.bytesFilled);
  const maxBytesFilled = p.bytesFilled + maxBytesToCopy;
  let remaining = maxBytesToCopy;
  let ready = false;
  const maxAlignedBytes = maxBytesFilled - (maxBytesFilled % p.elementSize);
  if (maxAlignedBytes >= p.minimumFill) {
    remaining = maxAlignedBytes - p.bytesFilled;
    ready = true;
  }
  while (remaining > 0) {
    const head = c.queue[0];
    const n = Math.min(remaining, head.byteLength);
    const destStart = p.byteOffset + p.bytesFilled;
    new Uint8Array(p.buffer, destStart, n).set(new Uint8Array(head.buffer, head.byteOffset, n));
    if (head.byteLength === n) c.queue.shift();
    else { head.byteOffset += n; head.byteLength -= n; }
    c.queueTotalSize -= n;
    p.bytesFilled += n;
    remaining -= n;
  }
  return ready;
}
function byteControllerShiftPendingPullInto(c) { return c.pendingPullIntos.shift(); }
function byteControllerConvertPullIntoDescriptor(p) {
  const buffer = transferBuffer(p.buffer);
  return new p.viewConstructor(buffer, p.byteOffset, p.bytesFilled / p.elementSize);
}
function byteControllerCommitPullIntoDescriptor(s, p) {
  const done = s.state === 'closed';
  const view = byteControllerConvertPullIntoDescriptor(p);
  if (p.readerType === 'default') fulfillReadRequest(s, view, done);
  else fulfillReadIntoRequest(s, view, done);
}
function byteControllerPullInto(c, view, min, req) {
  const s = c.stream;
  let size = 1, ctor = DataView;
  if (!(view instanceof DataView)) { size = view.BYTES_PER_ELEMENT; ctor = view.constructor; }
  const minimumFill = min * size;
  const { byteOffset, byteLength } = view;
  let buffer;
  try { buffer = transferBuffer(view.buffer); } catch (e) { req.error(e); return; }
  const p = { buffer, bufferByteLength: buffer.byteLength, byteOffset, byteLength, bytesFilled: 0, minimumFill,
    elementSize: size, viewConstructor: ctor, readerType: 'byob' };
  if (c.pendingPullIntos.length > 0) {
    c.pendingPullIntos.push(p);
    s.reader.readIntoRequests.push(req);
    return;
  }
  if (s.state === 'closed') { req.close(new ctor(p.buffer, p.byteOffset, 0)); return; }
  if (c.queueTotalSize > 0) {
    if (byteControllerFillPullIntoDescriptorFromQueue(c, p)) {
      const filled = byteControllerConvertPullIntoDescriptor(p);
      byteControllerHandleQueueDrain(c);
      req.chunk(filled);
      return;
    }
    if (c.closeRequested) {
      const e = new TypeError('Insufficient bytes to fill elements in the given buffer');
      byteControllerError(c, e);
      req.error(e);
      return;
    }
  }
  c.pendingPullIntos.push(p);
  s.reader.readIntoRequests.push(req);
  byteControllerCallPullIfNeeded(c);
}
function byteControllerRespond(c, bytesWritten) {
  const first = c.pendingPullIntos[0];
  if (c.stream.state === 'closed') {
    if (bytesWritten !== 0) throw new TypeError('bytesWritten must be 0 when calling respond() on a closed stream');
  } else {
    if (bytesWritten === 0) throw new TypeError('bytesWritten must be greater than 0 when calling respond() on a readable stream');
    if (first.bytesFilled + bytesWritten > first.byteLength) throw new RangeError('bytesWritten out of range');
  }
  first.buffer = transferBuffer(first.buffer);
  byteControllerRespondInternal(c, bytesWritten);
}
function byteControllerRespondInternal(c, bytesWritten) {
  const first = c.pendingPullIntos[0];
  byteControllerInvalidateBYOBRequest(c);
  if (c.stream.state === 'closed') byteControllerRespondInClosedState(c, first);
  else byteControllerRespondInReadableState(c, bytesWritten, first);
  byteControllerCallPullIfNeeded(c);
}
function byteControllerRespondInClosedState(c, first) {
  if (first.readerType === 'none') byteControllerShiftPendingPullInto(c);
  const s = c.stream;
  if (hasBYOBReader(s)) {
    const filled = [];
    while (filled.length < numReadIntoRequests(s)) filled.push(byteControllerShiftPendingPullInto(c));
    for (const p of filled) byteControllerCommitPullIntoDescriptor(s, p);
  }
}
function byteControllerRespondInReadableState(c, bytesWritten, p) {
  p.bytesFilled += bytesWritten;
  const s = c.stream;
  if (p.readerType === 'none') {
    byteControllerEnqueueDetachedPullIntoToQueue(c, p);
    for (const f of byteControllerProcessPullIntoDescriptorsUsingQueue(c)) byteControllerCommitPullIntoDescriptor(s, f);
    return;
  }
  if (p.bytesFilled < p.minimumFill) return;
  byteControllerShiftPendingPullInto(c);
  const remainder = p.bytesFilled % p.elementSize;
  if (remainder > 0) {
    const end = p.byteOffset + p.bytesFilled;
    byteControllerEnqueueClonedChunkToQueue(c, p.buffer, end - remainder, remainder);
  }
  p.bytesFilled -= remainder;
  const filled = byteControllerProcessPullIntoDescriptorsUsingQueue(c);
  byteControllerCommitPullIntoDescriptor(s, p);
  for (const f of filled) byteControllerCommitPullIntoDescriptor(s, f);
}
function byteControllerRespondWithNewView(c, view) {
  const first = c.pendingPullIntos[0];
  if (c.stream.state === 'closed') {
    if (view.byteLength !== 0) throw new TypeError('The view\'s length must be 0 when calling respondWithNewView() on a closed stream');
  } else if (view.byteLength === 0) {
    throw new TypeError('The view\'s length must be greater than 0 when calling respondWithNewView() on a readable stream');
  }
  if (first.byteOffset + first.bytesFilled !== view.byteOffset) throw new RangeError('The region specified by view does not match byobRequest');
  if (first.bufferByteLength !== view.buffer.byteLength) throw new RangeError('The buffer of view has different capacity than byobRequest');
  if (first.bytesFilled + view.byteLength > first.byteLength) throw new RangeError('The region specified by view is larger than byobRequest');
  const n = view.byteLength;
  first.buffer = transferBuffer(view.buffer);
  byteControllerRespondInternal(c, n);
}

// ------------------------------------------------------ tee, from, iterate --

function readableStreamTee(s) {
  return s.controller.kind === 'RBSC' ? byteStreamTee(s) : defaultTee(s);
}
function defaultTee(s) {
  const reader = acquireDefaultReader(s);
  let reading = false, readAgain = false, canceled1 = false, canceled2 = false, reason1, reason2, branch1, branch2;
  const cancelPromise = deferred();
  const pull = () => {
    if (reading) { readAgain = true; return resolved(undefined); }
    reading = true;
    defaultReaderRead(reader, {
      chunk: chunk => queueMicrotask(() => {
        readAgain = false;
        if (!canceled1) defaultControllerEnqueue(branch1.controller, chunk);
        if (!canceled2) defaultControllerEnqueue(branch2.controller, chunk);
        reading = false;
        if (readAgain) pull();
      }),
      close: () => {
        reading = false;
        if (!canceled1) defaultControllerClose(branch1.controller);
        if (!canceled2) defaultControllerClose(branch2.controller);
        if (!canceled1 || !canceled2) cancelPromise.resolve(undefined);
      },
      error: () => { reading = false; },
    });
    return resolved(undefined);
  };
  const cancel1 = reason => {
    canceled1 = true; reason1 = reason;
    if (canceled2) cancelPromise.resolve(readableStreamCancel(s, [reason1, reason2]));
    return cancelPromise.promise;
  };
  const cancel2 = reason => {
    canceled2 = true; reason2 = reason;
    if (canceled1) cancelPromise.resolve(readableStreamCancel(s, [reason1, reason2]));
    return cancelPromise.promise;
  };
  branch1 = createReadableStream(noop, pull, cancel1);
  branch2 = createReadableStream(noop, pull, cancel2);
  reader.closed.promise.then(undefined, r => {
    defaultControllerError(branch1.controller, r);
    defaultControllerError(branch2.controller, r);
    if (!canceled1 || !canceled2) cancelPromise.resolve(undefined);
  });
  return [branch1, branch2];
}
// Byte stream tee: both branches are byte streams; the second gets copies.
function byteStreamTee(s) {
  const reader = acquireDefaultReader(s);
  let reading = false, readAgain = false, canceled1 = false, canceled2 = false, reason1, reason2, branch1, branch2;
  const cancelPromise = deferred();
  const enqueue = (b, chunk) => {
    const c = b.controller;
    if (!c.closeRequested && b.state === 'readable') byteControllerEnqueue(c, chunk);
  };
  const close = b => {
    const c = b.controller;
    if (c.closeRequested || b.state !== 'readable') return;
    byteControllerClose(c);
    if (c.pendingPullIntos.length > 0) byteControllerRespond(c, 0);
  };
  const pull = () => {
    if (reading) { readAgain = true; return resolved(undefined); }
    reading = true;
    defaultReaderRead(reader, {
      chunk: chunk => queueMicrotask(() => {
        readAgain = false;
        const copy = canceled2 ? null : chunk.slice();
        try {
          if (!canceled1) enqueue(branch1, chunk);
          if (!canceled2) enqueue(branch2, copy);
        } catch (e) {
          byteControllerError(branch1.controller, e);
          byteControllerError(branch2.controller, e);
          cancelPromise.resolve(readableStreamCancel(s, e));
          return;
        }
        reading = false;
        if (readAgain) pull();
      }),
      close: () => {
        reading = false;
        if (!canceled1) close(branch1);
        if (!canceled2) close(branch2);
        if (!canceled1 || !canceled2) cancelPromise.resolve(undefined);
      },
      error: () => { reading = false; },
    });
    return resolved(undefined);
  };
  const cancel1 = reason => {
    canceled1 = true; reason1 = reason;
    if (canceled2) cancelPromise.resolve(readableStreamCancel(s, [reason1, reason2]));
    return cancelPromise.promise;
  };
  const cancel2 = reason => {
    canceled2 = true; reason2 = reason;
    if (canceled1) cancelPromise.resolve(readableStreamCancel(s, [reason1, reason2]));
    return cancelPromise.promise;
  };
  branch1 = createReadableByteStream(noop, pull, cancel1);
  branch2 = createReadableByteStream(noop, pull, cancel2);
  reader.closed.promise.then(undefined, r => {
    byteControllerError(branch1.controller, r);
    byteControllerError(branch2.controller, r);
    if (!canceled1 || !canceled2) cancelPromise.resolve(undefined);
  });
  return [branch1, branch2];
}

function readableStreamFromIterable(iterable) {
  let iterator, next;
  const am = iterable == null ? undefined : iterable[Symbol.asyncIterator];
  if (am != null) {
    iterator = am.call(iterable);
  } else {
    const sm = iterable == null ? undefined : iterable[Symbol.iterator];
    if (typeof sm !== 'function') throw new TypeError('ReadableStream.from: argument is not iterable');
    const syncIt = sm.call(iterable);
    iterator = (async function* () { yield* { [Symbol.iterator]: () => syncIt }; })();
  }
  if (!isObj(iterator)) throw new TypeError('The iterator is not an object');
  next = iterator.next;
  let s;
  const pull = () => {
    let r;
    try { r = next.call(iterator); } catch (e) { return rejected(e); }
    return resolved(r).then(res => {
      if (!isObj(res)) throw new TypeError('The promise returned by the iterator.next() method must fulfill with an object');
      if (res.done) defaultControllerClose(s.controller);
      else defaultControllerEnqueue(s.controller, res.value);
    });
  };
  const cancel = reason => {
    let ret;
    try {
      ret = iterator.return;
      if (ret == null) return resolved(undefined);
      ret = ret.call(iterator, reason);
    } catch (e) { return rejected(e); }
    return resolved(ret).then(res => {
      if (!isObj(res)) throw new TypeError('The promise returned by the iterator.return() method must fulfill with an object');
    });
  };
  s = createReadableStream(noop, pull, cancel, 0);
  return s;
}

const AsyncIteratorPrototype = Object.getPrototypeOf(Object.getPrototypeOf(async function* () {}).prototype);
const ITER = Symbol('huginn.iter');
const ReadableStreamAsyncIteratorPrototype = Object.create(AsyncIteratorPrototype, {
  next: { value: function next() {
    const it = this && this[ITER];
    if (!it) return rejected(new TypeError('Illegal invocation'));
    const step = () => {
      if (it.finished) return resolved({ value: undefined, done: true });
      const r = it.reader;
      if (r.stream === undefined) return rejected(new TypeError('Cannot get the next iteration result once the reader has been released'));
      const d = deferred();
      defaultReaderRead(r, {
        chunk: value => d.resolve({ value, done: false }),
        close: () => { defaultReaderRelease(r); d.resolve({ value: undefined, done: true }); },
        error: e => { defaultReaderRelease(r); d.reject(e); },
      });
      return d.promise.then(res => { if (res.done) it.finished = true; return res; }, e => { it.finished = true; throw e; });
    };
    it.ongoing = it.ongoing ? it.ongoing.then(step, step) : step();
    return it.ongoing;
  }, writable: true, configurable: true },
  return: { value: function (value) {
    const it = this && this[ITER];
    if (!it) return rejected(new TypeError('Illegal invocation'));
    const step = () => {
      if (it.finished) return resolved({ value, done: true });
      it.finished = true;
      const r = it.reader;
      if (r.stream === undefined) return resolved({ value, done: true });
      if (!it.preventCancel) {
        const p = readerGenericCancel(r, value);
        defaultReaderRelease(r);
        return p.then(() => ({ value, done: true }));
      }
      defaultReaderRelease(r);
      return resolved({ value, done: true });
    };
    it.ongoing = it.ongoing ? it.ongoing.then(step, step) : step();
    return it.ongoing;
  }, writable: true, configurable: true },
  [Symbol.toStringTag]: { value: 'ReadableStream AsyncIterator', configurable: true },
});
function makeAsyncIterator(reader, preventCancel) {
  const o = Object.create(ReadableStreamAsyncIteratorPrototype);
  defProp(o, ITER, { value: { reader, preventCancel, ongoing: null, finished: false } });
  return o;
}

// ---------------------------------------------------------------- piping --

function readableStreamPipeTo(source, dest, preventClose, preventAbort, preventCancel, signal) {
  const reader = acquireDefaultReader(source);
  const writer = acquireWriter(dest);
  source.disturbed = true;
  let shuttingDown = false;
  let currentWrite = resolved(undefined);
  const promise = deferred();
  let abortAlgorithm;

  const finalize = (isError, error) => {
    writerRelease(writer);
    defaultReaderRelease(reader);
    if (signal !== undefined) signal.removeEventListener('abort', abortAlgorithm);
    if (isError) promise.reject(error); else promise.resolve(undefined);
  };
  const waitForWritesToFinish = () => {
    const old = currentWrite;
    return currentWrite.then(() => old !== currentWrite ? waitForWritesToFinish() : undefined);
  };
  const destWritableAndOpen = () => dest.state === 'writable' && !closeQueuedOrInFlight(dest);
  const shutdownWithAction = (action, originalIsError, originalError) => {
    if (shuttingDown) return;
    shuttingDown = true;
    const doTheRest = () => {
      let p;
      try { p = resolved(action()); } catch (e) { p = rejected(e); }
      p.then(() => finalize(originalIsError, originalError), e => finalize(true, e));
    };
    if (destWritableAndOpen()) waitForWritesToFinish().then(doTheRest);
    else doTheRest();
  };
  const shutdown = (isError, error) => {
    if (shuttingDown) return;
    shuttingDown = true;
    if (destWritableAndOpen()) waitForWritesToFinish().then(() => finalize(isError, error));
    else finalize(isError, error);
  };

  if (signal !== undefined) {
    abortAlgorithm = () => {
      const error = signal.reason;
      const actions = [];
      if (!preventAbort) actions.push(() => dest.state === 'writable' ? writableStreamAbort(dest, error) : resolved(undefined));
      if (!preventCancel) actions.push(() => source.state === 'readable' ? readableStreamCancel(source, error) : resolved(undefined));
      shutdownWithAction(() => Promise.all(actions.map(a => a())), true, error);
    };
    if (signal.aborted) { abortAlgorithm(); return promise.promise; }
    signal.addEventListener('abort', abortAlgorithm);
  }

  const pipeStep = () => {
    if (shuttingDown) return resolved(true);
    return writer.ready.promise.then(() => {
      if (shuttingDown) return true;
      const d = deferred();
      defaultReaderRead(reader, {
        chunk: chunk => {
          currentWrite = setHandled(writerWrite(writer, chunk).then(noop, noop));
          d.resolve(false);
        },
        close: () => d.resolve(true),
        error: e => d.reject(e),
      });
      return d.promise;
    });
  };
  const pipeLoop = () => new Promise((resolveLoop, rejectLoop) => {
    const next = done => { if (done) resolveLoop(); else pipeStep().then(next, rejectLoop); };
    next(false);
  });

  // Errors must be propagated forward.
  const sourceErrored = e => {
    if (!preventAbort) shutdownWithAction(() => writableStreamAbort(dest, e), true, e);
    else shutdown(true, e);
  };
  if (source.state === 'errored') sourceErrored(source.storedError);
  else reader.closed.promise.then(undefined, sourceErrored);
  // Errors must be propagated backward.
  const destErrored = e => {
    if (!preventCancel) shutdownWithAction(() => readableStreamCancel(source, e), true, e);
    else shutdown(true, e);
  };
  if (dest.state === 'errored') destErrored(dest.storedError);
  else writer.closed.promise.then(undefined, destErrored);
  // Closing must be propagated forward.
  const sourceClosed = () => {
    if (!preventClose) shutdownWithAction(() => writerCloseWithErrorPropagation(writer));
    else shutdown();
  };
  if (source.state === 'closed') sourceClosed();
  else reader.closed.promise.then(sourceClosed, noop);
  // Closing must be propagated backward.
  if (closeQueuedOrInFlight(dest) || dest.state === 'closed') {
    const destClosed = new TypeError('the destination writable stream closed before all data could be piped to it');
    if (!preventCancel) shutdownWithAction(() => readableStreamCancel(source, destClosed), true, destClosed);
    else shutdown(true, destClosed);
  }
  setHandled(pipeLoop());
  return promise.promise;
}

// ------------------------------------------------------ writable streams --

const CLOSE_SENTINEL = {};

function initWS(o) {
  return hide(o, { kind: 'WS', state: 'writable', storedError: undefined, writer: undefined, controller: undefined,
    inFlightWriteRequest: undefined, closeRequest: undefined, inFlightCloseRequest: undefined,
    pendingAbortRequest: undefined, writeRequests: [], backpressure: false });
}
const isWSLocked = s => s.writer !== undefined;

class WritableStream {
  constructor(underlyingSink = undefined, strategy = undefined) {
    const sink = dict(underlyingSink, 'underlyingSink');
    const st = strategyDict(strategy);
    if (sink.type !== undefined) throw new RangeError('Invalid type is specified');
    const s = initWS(this);
    const size = extractSize(st);
    const hwm = extractHWM(st, 1);
    const c = make(WritableStreamDefaultController, { kind: 'WSDC' });
    const start = fnMember(sink, 'start'), write = fnMember(sink, 'write'), close = fnMember(sink, 'close'), abort = fnMember(sink, 'abort');
    setUpWSController(s, c,
      () => start === undefined ? undefined : start.call(underlyingSink, c.pub),
      chunk => write === undefined ? resolved(undefined) : promiseCall(write, underlyingSink, chunk, c.pub),
      () => close === undefined ? resolved(undefined) : promiseCall(close, underlyingSink),
      reason => abort === undefined ? resolved(undefined) : promiseCall(abort, underlyingSink, reason),
      hwm, size);
  }
  get locked() { return isWSLocked(slot(this, 'WS')); }
  abort(reason) {
    const s = pslot(this, 'WS');
    if (!s) return illegal();
    if (isWSLocked(s)) return rejected(new TypeError('Cannot abort a stream that already has a writer'));
    return writableStreamAbort(s, reason);
  }
  close() {
    const s = pslot(this, 'WS');
    if (!s) return illegal();
    if (isWSLocked(s)) return rejected(new TypeError('Cannot close a stream that already has a writer'));
    if (closeQueuedOrInFlight(s)) return rejected(new TypeError('Cannot close an already-closing stream'));
    return writableStreamClose(s);
  }
  getWriter() { return acquireWriter(slot(this, 'WS')).pub; }
}

function createWritableStream(start, write, close, abort, hwm, size) {
  const rec = initWS(Object.create(WritableStream.prototype));
  setUpWSController(rec, make(WritableStreamDefaultController, { kind: 'WSDC' }), start, write, close, abort, hwm, size);
  return rec;
}

function writableStreamAbort(s, reason) {
  if (s.state === 'closed' || s.state === 'errored') return resolved(undefined);
  s.controller.abortController.abort(reason);
  if (s.state === 'closed' || s.state === 'errored') return resolved(undefined);
  if (s.pendingAbortRequest !== undefined) return s.pendingAbortRequest.d.promise;
  let wasAlreadyErroring = false;
  if (s.state === 'erroring') { wasAlreadyErroring = true; reason = undefined; }
  const d = deferred();
  s.pendingAbortRequest = { d, reason, wasAlreadyErroring };
  if (!wasAlreadyErroring) writableStreamStartErroring(s, reason);
  return d.promise;
}
function writableStreamClose(s) {
  if (s.state === 'closed' || s.state === 'errored') return rejected(new TypeError('The stream is not in the writable state and cannot be closed'));
  const d = deferred();
  s.closeRequest = d;
  const w = s.writer;
  if (w !== undefined && s.backpressure && s.state === 'writable') w.ready.resolve(undefined);
  wsControllerClose(s.controller);
  return d.promise;
}
function writableStreamDealWithRejection(s, e) {
  if (s.state === 'writable') { writableStreamStartErroring(s, e); return; }
  writableStreamFinishErroring(s);
}
function writableStreamStartErroring(s, reason) {
  const c = s.controller;
  s.state = 'erroring';
  s.storedError = reason;
  if (s.writer !== undefined) writerEnsureReadyPromiseRejected(s.writer, reason);
  if (!hasOperationMarkedInFlight(s) && c.started) writableStreamFinishErroring(s);
}
function writableStreamFinishErroring(s) {
  s.state = 'errored';
  resetQueue(s.controller);
  const e = s.storedError;
  for (const d of s.writeRequests) d.reject(e);
  s.writeRequests = [];
  const abortRequest = s.pendingAbortRequest;
  if (abortRequest === undefined) { rejectCloseAndClosedPromiseIfNeeded(s); return; }
  s.pendingAbortRequest = undefined;
  if (abortRequest.wasAlreadyErroring) {
    abortRequest.d.reject(e);
    rejectCloseAndClosedPromiseIfNeeded(s);
    return;
  }
  const c = s.controller;
  const p = c.abortAlgorithm(abortRequest.reason);
  wsControllerClearAlgorithms(c);
  p.then(() => {
    abortRequest.d.resolve(undefined);
    rejectCloseAndClosedPromiseIfNeeded(s);
  }, r => {
    abortRequest.d.reject(r);
    rejectCloseAndClosedPromiseIfNeeded(s);
  });
}
function writableStreamFinishInFlightWrite(s) { s.inFlightWriteRequest.resolve(undefined); s.inFlightWriteRequest = undefined; }
function writableStreamFinishInFlightWriteWithError(s, e) {
  s.inFlightWriteRequest.reject(e);
  s.inFlightWriteRequest = undefined;
  writableStreamDealWithRejection(s, e);
}
function writableStreamFinishInFlightClose(s) {
  s.inFlightCloseRequest.resolve(undefined);
  s.inFlightCloseRequest = undefined;
  if (s.state === 'erroring') {
    s.storedError = undefined;
    if (s.pendingAbortRequest !== undefined) {
      s.pendingAbortRequest.d.resolve(undefined);
      s.pendingAbortRequest = undefined;
    }
  }
  s.state = 'closed';
  if (s.writer !== undefined) s.writer.closed.resolve(undefined);
}
function writableStreamFinishInFlightCloseWithError(s, e) {
  s.inFlightCloseRequest.reject(e);
  s.inFlightCloseRequest = undefined;
  if (s.pendingAbortRequest !== undefined) {
    s.pendingAbortRequest.d.reject(e);
    s.pendingAbortRequest = undefined;
  }
  writableStreamDealWithRejection(s, e);
}
function closeQueuedOrInFlight(s) { return s.closeRequest !== undefined || s.inFlightCloseRequest !== undefined; }
function hasOperationMarkedInFlight(s) { return s.inFlightWriteRequest !== undefined || s.inFlightCloseRequest !== undefined; }
function rejectCloseAndClosedPromiseIfNeeded(s) {
  if (s.closeRequest !== undefined) {
    s.closeRequest.reject(s.storedError);
    s.closeRequest = undefined;
  }
  if (s.writer !== undefined) rejectHandled(s.writer.closed, s.storedError);
}
function writableStreamUpdateBackpressure(s, backpressure) {
  const w = s.writer;
  if (w !== undefined && backpressure !== s.backpressure) {
    if (backpressure) w.ready = deferred();
    else w.ready.resolve(undefined);
  }
  s.backpressure = backpressure;
}

// --------------------------------------------------------------- writers --

function setUpWriter(w, s) {
  if (isWSLocked(s)) throw new TypeError('WritableStream is locked to a writer');
  w.stream = s;
  s.writer = w;
  const st = s.state;
  if (st === 'writable') {
    w.ready = !closeQueuedOrInFlight(s) && s.backpressure ? deferred() : resolvedDeferred(undefined);
    w.closed = deferred();
  } else if (st === 'erroring') {
    w.ready = rejectedDeferred(s.storedError);
    w.closed = deferred();
  } else if (st === 'closed') {
    w.ready = resolvedDeferred(undefined);
    w.closed = resolvedDeferred(undefined);
  } else {
    w.ready = rejectedDeferred(s.storedError);
    w.closed = rejectedDeferred(s.storedError);
  }
}
function acquireWriter(s) {
  const w = make(WritableStreamDefaultWriter, { kind: 'WSDW' });
  setUpWriter(w, s);
  return w;
}
function writerEnsureClosedPromiseRejected(w, e) {
  if (w.closed.state === 'pending') rejectHandled(w.closed, e);
  else w.closed = rejectedDeferred(e);
}
function writerEnsureReadyPromiseRejected(w, e) {
  if (w.ready.state === 'pending') rejectHandled(w.ready, e);
  else w.ready = rejectedDeferred(e);
}
function writerGetDesiredSize(w) {
  const st = w.stream.state;
  if (st === 'errored' || st === 'erroring') return null;
  if (st === 'closed') return 0;
  return wsControllerGetDesiredSize(w.stream.controller);
}
function writerRelease(w) {
  const s = w.stream;
  const e = new TypeError('Writer was released and can no longer be used to monitor the stream\'s closedness');
  writerEnsureReadyPromiseRejected(w, e);
  writerEnsureClosedPromiseRejected(w, e);
  s.writer = undefined;
  w.stream = undefined;
}
function writerWrite(w, chunk) {
  const s = w.stream;
  const c = s.controller;
  const size = wsControllerGetChunkSize(c, chunk);
  if (s !== w.stream) return rejected(new TypeError('Cannot write to a stream using a released writer'));
  const st = s.state;
  if (st === 'errored') return rejected(s.storedError);
  if (closeQueuedOrInFlight(s) || st === 'closed') return rejected(new TypeError('The stream is closing or closed and cannot be written to'));
  if (st === 'erroring') return rejected(s.storedError);
  const d = deferred();
  s.writeRequests.push(d);
  wsControllerWrite(c, chunk, size);
  return d.promise;
}
function writerCloseWithErrorPropagation(w) {
  const s = w.stream;
  if (closeQueuedOrInFlight(s) || s.state === 'closed') return resolved(undefined);
  if (s.state === 'errored') return rejected(s.storedError);
  return writableStreamClose(s);
}

class WritableStreamDefaultWriter {
  constructor(stream) { setUpWriter(hide(this, { kind: 'WSDW' }), slot(stream, 'WS')); }
  get closed() { const w = pslot(this, 'WSDW'); return w ? w.closed.promise : illegal(); }
  get ready() { const w = pslot(this, 'WSDW'); return w ? w.ready.promise : illegal(); }
  get desiredSize() {
    const w = slot(this, 'WSDW');
    if (w.stream === undefined) throw new TypeError('This writer has been released');
    return writerGetDesiredSize(w);
  }
  abort(reason) {
    const w = pslot(this, 'WSDW');
    if (!w) return illegal();
    if (w.stream === undefined) return rejected(new TypeError('This writer has been released'));
    return writableStreamAbort(w.stream, reason);
  }
  close() {
    const w = pslot(this, 'WSDW');
    if (!w) return illegal();
    const s = w.stream;
    if (s === undefined) return rejected(new TypeError('This writer has been released'));
    if (closeQueuedOrInFlight(s)) return rejected(new TypeError('Cannot close an already-closing stream'));
    return writableStreamClose(s);
  }
  releaseLock() {
    const w = slot(this, 'WSDW');
    if (w.stream !== undefined) writerRelease(w);
  }
  write(chunk = undefined) {
    const w = pslot(this, 'WSDW');
    if (!w) return illegal();
    if (w.stream === undefined) return rejected(new TypeError('This writer has been released'));
    return writerWrite(w, chunk);
  }
}

// ----------------------------------------------------- writable controller --

class WritableStreamDefaultController {
  constructor() { throw new TypeError('Illegal constructor'); }
  get signal() { return slot(this, 'WSDC').abortController.signal; }
  error(e = undefined) {
    const c = slot(this, 'WSDC');
    if (c.stream.state !== 'writable') return;
    wsControllerError(c, e);
  }
}

function setUpWSController(s, c, start, write, close, abort, hwm, size) {
  c.stream = s;
  s.controller = c;
  resetQueue(c);
  c.abortController = new AbortController();
  c.started = false;
  c.strategySizeAlgorithm = size;
  c.strategyHWM = hwm;
  c.writeAlgorithm = write;
  c.closeAlgorithm = close;
  c.abortAlgorithm = abort;
  writableStreamUpdateBackpressure(s, wsControllerGetBackpressure(c));
  const startResult = start();
  resolved(startResult).then(() => {
    c.started = true;
    wsControllerAdvanceQueueIfNeeded(c);
  }, r => {
    c.started = true;
    writableStreamDealWithRejection(s, r);
  });
}
function wsControllerClearAlgorithms(c) {
  c.writeAlgorithm = c.closeAlgorithm = c.abortAlgorithm = c.strategySizeAlgorithm = undefined;
}
function wsControllerClose(c) {
  enqueueValueWithSize(c, CLOSE_SENTINEL, 0);
  wsControllerAdvanceQueueIfNeeded(c);
}
function wsControllerGetChunkSize(c, chunk) {
  if (c.strategySizeAlgorithm === undefined) return 1;
  try { return c.strategySizeAlgorithm(chunk); } catch (e) { wsControllerErrorIfNeeded(c, e); return 1; }
}
function wsControllerGetDesiredSize(c) { return c.strategyHWM - c.queueTotalSize; }
function wsControllerWrite(c, chunk, size) {
  try { enqueueValueWithSize(c, chunk, size); } catch (e) { wsControllerErrorIfNeeded(c, e); return; }
  const s = c.stream;
  if (!closeQueuedOrInFlight(s) && s.state === 'writable') writableStreamUpdateBackpressure(s, wsControllerGetBackpressure(c));
  wsControllerAdvanceQueueIfNeeded(c);
}
function wsControllerAdvanceQueueIfNeeded(c) {
  const s = c.stream;
  if (!c.started || s.inFlightWriteRequest !== undefined) return;
  if (s.state === 'erroring') { writableStreamFinishErroring(s); return; }
  if (c.queue.length === 0) return;
  const value = c.queue[0].value;
  if (value === CLOSE_SENTINEL) wsControllerProcessClose(c);
  else wsControllerProcessWrite(c, value);
}
function wsControllerErrorIfNeeded(c, e) { if (c.stream.state === 'writable') wsControllerError(c, e); }
function wsControllerProcessClose(c) {
  const s = c.stream;
  s.inFlightCloseRequest = s.closeRequest;
  s.closeRequest = undefined;
  dequeueValue(c);
  const p = c.closeAlgorithm();
  wsControllerClearAlgorithms(c);
  p.then(() => writableStreamFinishInFlightClose(s), r => writableStreamFinishInFlightCloseWithError(s, r));
}
function wsControllerProcessWrite(c, chunk) {
  const s = c.stream;
  s.inFlightWriteRequest = s.writeRequests.shift();
  c.writeAlgorithm(chunk).then(() => {
    writableStreamFinishInFlightWrite(s);
    dequeueValue(c);
    if (!closeQueuedOrInFlight(s) && s.state === 'writable') writableStreamUpdateBackpressure(s, wsControllerGetBackpressure(c));
    wsControllerAdvanceQueueIfNeeded(c);
  }, r => {
    if (s.state === 'writable') wsControllerClearAlgorithms(c);
    writableStreamFinishInFlightWriteWithError(s, r);
  });
}
function wsControllerGetBackpressure(c) { return wsControllerGetDesiredSize(c) <= 0; }
function wsControllerError(c, e) {
  wsControllerClearAlgorithms(c);
  writableStreamStartErroring(c.stream, e);
}

// ----------------------------------------------------- transform streams --

class TransformStream {
  constructor(transformer = undefined, writableStrategy = undefined, readableStrategy = undefined) {
    const t = dict(transformer, 'transformer');
    const ws = strategyDict(writableStrategy);
    const rs = strategyDict(readableStrategy);
    const flush = fnMember(t, 'flush'), readableType = t.readableType, start = fnMember(t, 'start');
    const transform = fnMember(t, 'transform'), writableType = t.writableType, cancel = fnMember(t, 'cancel');
    if (readableType !== undefined) throw new RangeError('Invalid readableType specified');
    if (writableType !== undefined) throw new RangeError('Invalid writableType specified');
    const readableHWM = extractHWM(rs, 0), readableSize = extractSize(rs);
    const writableHWM = extractHWM(ws, 1), writableSize = extractSize(ws);
    const startPromise = deferred();
    const s = hide(this, { kind: 'TS' });
    initTransformStream(s, startPromise, writableHWM, writableSize, readableHWM, readableSize);
    const c = make(TransformStreamDefaultController, { kind: 'TSDC' });
    setUpTSController(s, c,
      transform === undefined
        ? chunk => { try { tsControllerEnqueue(c, chunk); return resolved(undefined); } catch (e) { return rejected(e); } }
        : chunk => promiseCall(transform, transformer, chunk, c.pub),
      () => flush === undefined ? resolved(undefined) : promiseCall(flush, transformer, c.pub),
      reason => cancel === undefined ? resolved(undefined) : promiseCall(cancel, transformer, reason));
    startPromise.resolve(start === undefined ? undefined : start.call(transformer, c.pub));
  }
  get readable() { return slot(this, 'TS').readable.pub; }
  get writable() { return slot(this, 'TS').writable.pub; }
}

function initTransformStream(s, startPromise, writableHWM, writableSize, readableHWM, readableSize) {
  const start = () => startPromise.promise;
  s.writable = createWritableStream(start, chunk => tsSinkWrite(s, chunk), () => tsSinkClose(s), reason => tsSinkAbort(s, reason), writableHWM, writableSize);
  s.readable = createReadableStream(start, () => tsSourcePull(s), reason => tsSourceCancel(s, reason), readableHWM, readableSize);
  s.backpressure = undefined;
  s.backpressureChangePromise = undefined;
  tsSetBackpressure(s, true);
  s.controller = undefined;
}
function tsError(s, e) {
  defaultControllerError(s.readable.controller, e);
  tsErrorWritableAndUnblockWrite(s, e);
}
function tsErrorWritableAndUnblockWrite(s, e) {
  tsControllerClearAlgorithms(s.controller);
  wsControllerErrorIfNeeded(s.writable.controller, e);
  tsUnblockWrite(s);
}
function tsUnblockWrite(s) { if (s.backpressure) tsSetBackpressure(s, false); }
function tsSetBackpressure(s, backpressure) {
  if (s.backpressureChangePromise !== undefined) s.backpressureChangePromise.resolve(undefined);
  s.backpressureChangePromise = deferred();
  s.backpressure = backpressure;
}

class TransformStreamDefaultController {
  constructor() { throw new TypeError('Illegal constructor'); }
  get desiredSize() { return defaultControllerGetDesiredSize(slot(this, 'TSDC').stream.readable.controller); }
  enqueue(chunk = undefined) { tsControllerEnqueue(slot(this, 'TSDC'), chunk); }
  error(reason = undefined) { tsError(slot(this, 'TSDC').stream, reason); }
  terminate() { tsControllerTerminate(slot(this, 'TSDC')); }
}

function setUpTSController(s, c, transform, flush, cancel) {
  c.stream = s;
  s.controller = c;
  c.transformAlgorithm = transform;
  c.flushAlgorithm = flush;
  c.cancelAlgorithm = cancel;
  c.finishPromise = undefined;
}
function tsControllerClearAlgorithms(c) { c.transformAlgorithm = c.flushAlgorithm = c.cancelAlgorithm = undefined; }
function tsControllerEnqueue(c, chunk) {
  const s = c.stream;
  const rc = s.readable.controller;
  if (!defaultControllerCanCloseOrEnqueue(rc)) throw new TypeError('Readable side is not in a state that permits enqueue');
  try { defaultControllerEnqueue(rc, chunk); } catch (e) {
    tsErrorWritableAndUnblockWrite(s, e);
    throw s.readable.storedError;
  }
  const backpressure = defaultControllerHasBackpressure(rc);
  if (backpressure !== s.backpressure) tsSetBackpressure(s, true);
}
function tsControllerPerformTransform(c, chunk) {
  return c.transformAlgorithm(chunk).then(undefined, r => { tsError(c.stream, r); throw r; });
}
function tsControllerTerminate(c) {
  const s = c.stream;
  defaultControllerClose(s.readable.controller);
  tsErrorWritableAndUnblockWrite(s, new TypeError('TransformStream terminated'));
}
function tsSinkWrite(s, chunk) {
  const c = s.controller;
  if (s.backpressure) {
    return s.backpressureChangePromise.promise.then(() => {
      const w = s.writable;
      if (w.state === 'erroring') throw w.storedError;
      return tsControllerPerformTransform(c, chunk);
    });
  }
  return tsControllerPerformTransform(c, chunk);
}
function tsSinkAbort(s, reason) {
  const c = s.controller;
  if (c.finishPromise !== undefined) return c.finishPromise.promise;
  const readable = s.readable;
  const d = c.finishPromise = deferred();
  const p = c.cancelAlgorithm(reason);
  tsControllerClearAlgorithms(c);
  p.then(() => {
    if (readable.state === 'errored') d.reject(readable.storedError);
    else { defaultControllerError(readable.controller, reason); d.resolve(undefined); }
  }, r => {
    defaultControllerError(readable.controller, r);
    d.reject(r);
  });
  return d.promise;
}
function tsSinkClose(s) {
  const c = s.controller;
  if (c.finishPromise !== undefined) return c.finishPromise.promise;
  const readable = s.readable;
  const d = c.finishPromise = deferred();
  const p = c.flushAlgorithm();
  tsControllerClearAlgorithms(c);
  p.then(() => {
    if (readable.state === 'errored') d.reject(readable.storedError);
    else { defaultControllerClose(readable.controller); d.resolve(undefined); }
  }, r => {
    defaultControllerError(readable.controller, r);
    d.reject(r);
  });
  return d.promise;
}
function tsSourcePull(s) {
  tsSetBackpressure(s, false);
  return s.backpressureChangePromise.promise;
}
function tsSourceCancel(s, reason) {
  const c = s.controller;
  if (c.finishPromise !== undefined) return c.finishPromise.promise;
  const writable = s.writable;
  const d = c.finishPromise = deferred();
  const p = c.cancelAlgorithm(reason);
  tsControllerClearAlgorithms(c);
  p.then(() => {
    if (writable.state === 'errored') d.reject(writable.storedError);
    else { wsControllerErrorIfNeeded(writable.controller, reason); tsUnblockWrite(s); d.resolve(undefined); }
  }, r => {
    wsControllerErrorIfNeeded(writable.controller, r);
    tsUnblockWrite(s);
    d.reject(r);
  });
  return d.promise;
}

// ------------------------------------------------------- queuing strategies --

const byteLengthSize = { size(chunk) { return chunk.byteLength; } }.size;
const countSize = { size() { return 1; } }.size;
class ByteLengthQueuingStrategy {
  constructor(init) {
    if (!isObj(init) || init.highWaterMark === undefined) throw new TypeError("Failed to construct 'ByteLengthQueuingStrategy': required member highWaterMark is undefined.");
    hide(this, { kind: 'BLQS', hwm: Number(init.highWaterMark) });
  }
  get highWaterMark() { return slot(this, 'BLQS').hwm; }
  get size() { slot(this, 'BLQS'); return byteLengthSize; }
}
class CountQueuingStrategy {
  constructor(init) {
    if (!isObj(init) || init.highWaterMark === undefined) throw new TypeError("Failed to construct 'CountQueuingStrategy': required member highWaterMark is undefined.");
    hide(this, { kind: 'CQS', hwm: Number(init.highWaterMark) });
  }
  get highWaterMark() { return slot(this, 'CQS').hwm; }
  get size() { slot(this, 'CQS'); return countSize; }
}

// -------------------------------------------- encoding and compression --

// A transform stream whose public object is `o` (TextEncoderStream etc.).
function innerTransform(o, kind, transformer) {
  const ts = new TransformStream(transformer);
  hide(o, { kind, ts: ts[SL] });
}

class TextEncoderStream {
  constructor() {
    let pending = null;
    innerTransform(this, 'TES', {
      transform(chunk, c) {
        let s = String(chunk);
        if (pending !== null) { s = pending + s; pending = null; }
        const last = s.charCodeAt(s.length - 1);
        if (last >= 0xd800 && last < 0xdc00) { pending = s[s.length - 1]; s = s.slice(0, -1); }
        if (s.length) c.enqueue(new TextEncoder().encode(s));
      },
      flush(c) { if (pending !== null) c.enqueue(new Uint8Array([0xef, 0xbf, 0xbd])); },
    });
  }
  get encoding() { slot(this, 'TES'); return 'utf-8'; }
  get readable() { return slot(this, 'TES').ts.readable.pub; }
  get writable() { return slot(this, 'TES').ts.writable.pub; }
}

class TextDecoderStream {
  constructor(label = 'utf-8', options = undefined) {
    const dec = new TextDecoder(label, options);
    innerTransform(this, 'TDS', {
      transform(chunk, c) {
        if (!(chunk instanceof ArrayBuffer) && !isView(chunk)) throw new TypeError("Failed to execute 'transform': The provided value is not of type '(ArrayBuffer or ArrayBufferView)'.");
        const s = dec.decode(chunk, { stream: true });
        if (s) c.enqueue(s);
      },
      flush(c) { const s = dec.decode(); if (s) c.enqueue(s); },
    });
    this[SL].dec = dec;
  }
  get encoding() { return slot(this, 'TDS').dec.encoding; }
  get fatal() { return slot(this, 'TDS').dec.fatal; }
  get ignoreBOM() { return slot(this, 'TDS').dec.ignoreBOM; }
  get readable() { return slot(this, 'TDS').ts.readable.pub; }
  get writable() { return slot(this, 'TDS').ts.writable.pub; }
}

// Compression runs natively (flate2); `N.codec*` keep the codec state.
const FORMATS = ['deflate', 'deflate-raw', 'gzip'];
function codecTransform(o, kind, format, decompress) {
  format = String(format);
  if (FORMATS.indexOf(format) < 0) throw new TypeError(`The provided value '${format}' is not a valid enum value of type CompressionFormat.`);
  let id = N.codecNew(format, decompress);
  const bytes = chunk => {
    if (chunk instanceof ArrayBuffer) return new Uint8Array(chunk);
    if (isView(chunk)) return new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
    throw new TypeError("The provided value is not of type '(ArrayBuffer or ArrayBufferView)'.");
  };
  const drop = () => { if (id) { N.codecDrop(id); id = 0; } };
  innerTransform(o, kind, {
    transform(chunk, c) {
      let out;
      try { out = N.codecPush(id, bytes(chunk)); } catch (e) { drop(); throw new TypeError(String(e.message)); }
      if (out.byteLength) c.enqueue(out);
    },
    flush(c) {
      let out;
      try { out = N.codecFinish(id); } catch (e) { throw new TypeError(String(e.message)); } finally { id = 0; }
      if (out.byteLength) c.enqueue(out);
    },
    cancel: drop,
  });
}
class CompressionStream {
  constructor(format) { codecTransform(this, 'CS', format, false); }
  get readable() { return slot(this, 'CS').ts.readable.pub; }
  get writable() { return slot(this, 'CS').ts.writable.pub; }
}
class DecompressionStream {
  constructor(format) { codecTransform(this, 'DS', format, true); }
  get readable() { return slot(this, 'DS').ts.readable.pub; }
  get writable() { return slot(this, 'DS').ts.writable.pub; }
}

// ----------------------------------------------------------- for the prelude --

// A byte stream over `bytes` (Response and Blob bodies), enqueued in one chunk.
function bytesStream(bytes) {
  const s = createReadableByteStream(noop, () => resolved(undefined), () => resolved(undefined));
  if (bytes.byteLength) byteControllerEnqueue(s.controller, bytes);
  byteControllerClose(s.controller);
  return s.pub;
}
// Read every chunk of `stream` (a ReadableStream of Uint8Array) into one Uint8Array.
function readAllBytes(stream) {
  const reader = acquireDefaultReader(slot(stream, 'RS'));
  const chunks = [];
  let total = 0;
  return new Promise((resolve, reject) => {
    const step = () => defaultReaderRead(reader, {
      chunk: chunk => {
        if (!(chunk instanceof Uint8Array)) { reject(new TypeError('Received non-Uint8Array chunk')); return; }
        chunks.push(chunk);
        total += chunk.byteLength;
        step();
      },
      close: () => {
        const out = new Uint8Array(total);
        let off = 0;
        for (const c of chunks) { out.set(c, off); off += c.byteLength; }
        resolve(out);
      },
      error: reject,
    });
    step();
  });
}
function isDisturbed(stream) { return slot(stream, 'RS').disturbed; }
function isStreamLocked(stream) { return isLocked(slot(stream, 'RS')); }
function teeStream(stream) { return readableStreamTee(slot(stream, 'RS')).map(b => b.pub); }

const lib = {
  ReadableStream, ReadableStreamDefaultReader, ReadableStreamBYOBReader, ReadableStreamDefaultController,
  ReadableByteStreamController, ReadableStreamBYOBRequest, WritableStream, WritableStreamDefaultWriter,
  WritableStreamDefaultController, TransformStream, TransformStreamDefaultController, ByteLengthQueuingStrategy,
  CountQueuingStrategy, TextEncoderStream, TextDecoderStream, CompressionStream, DecompressionStream,
  bytesStream, readAllBytes, isDisturbed, isStreamLocked, teeStream,
};
for (const k of STREAM_NAMES) {
  const C = lib[k];
  tag(C, k);
  idlEnumerable(C);
}
return lib;
}
