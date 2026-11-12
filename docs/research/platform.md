# Research: networking, JavaScript, security and compatibility

Date: 2026-11-12. Part of [docs/research](README.md).

This covers the half of a browser that is not rendering. Each section gives the
standard design, the smallest version that would work for Huginn, and the traps.

Claims come from the linked pages, read on the date above. **[unverified]**
marks a claim that was not checked against a fetched source. **[inference]**
marks reasoning, not a sourced fact.

## Where Huginn is today

- HTTP is ureq with rustls: blocking, HTTP/1.1, memory cache only.
- There is no cookie jar. `document.cookie` is a map inside the page.
- `fetch` and XHR are same origin only, GET and HEAD only, with no request
  headers or bodies. They block the tab's engine thread.
- `document.referrer` is always empty.
- V8 runs with `--single-threaded --lite-mode`. The web platform objects are
  JavaScript on top of about 80 native operations.
- `localStorage` exists but lives in memory. `MutationObserver` and
  `IntersectionObserver` exist in a reduced form.
- There is no sandbox. Pages run in the browser process.
- The HTTP `User-Agent` says `Huginn/0.1`, while `navigator.userAgent` says
  Chrome 131. The two do not agree.

Two failures on real sites led to this research (traced on 2026-11-12):

- **bbc.com loops between bbc.com and bbc.co.uk.** bbc.com asks
  `https://www.bbc.co.uk/userinfo` where the visitor is. Huginn blocks the
  cross-origin request, so the page assumes a UK visitor and redirects.
  bbc.co.uk makes the same request (same origin there), learns the visitor is
  not in the UK and redirects back. The site's loop guard reads
  `document.referrer`, which is empty.
- **Google search returns HTTP 429.** Google serves its JavaScript bot check.
  The script throws (`Lk is not a function`), and the follow-up request is
  refused. Google also serves Huginn its old basic HTML home page.

## 1. Networking

**The standard design (Fetch).**

- The credentials mode defaults to `same-origin`. Request modes are
  `same-origin`, `cors`, `no-cors` and `navigate`. Scripts may not set `Cookie`,
  `Host`, `Origin`, `Referer` or any `Sec-` or `Proxy-` header.
  <https://fetch.spec.whatwg.org/>
- Redirects: at most 20. A 301 or 302 on POST, and a 303 on anything but GET or
  HEAD, becomes a GET with no body. `Authorization` is dropped on a
  cross-origin hop. `Cookie`, `Referer` and `Origin` are added inside the
  network step, so they are computed again for each hop.
- CORS: a GET, HEAD or POST with only safelisted headers needs no preflight.
  Anything else sends an `OPTIONS` preflight without credentials. A response to
  a request with credentials may not use `*`.
  <https://developer.mozilla.org/en-US/docs/Web/HTTP/Guides/CORS>
- Referrer: the default policy is `strict-origin-when-cross-origin`. Same
  origin gets the full URL, cross origin gets the origin only, and HTTPS to
  HTTP gets nothing. <https://w3c.github.io/webappsec-referrer-policy/>

**Cookies (RFC 6265bis, draft 20).**

- Stored fields: name, value, expiry, domain, path, creation and last access
  time, and the flags persistent, host-only, secure-only, http-only and same-site.
- A cookie with no SameSite is enforced like Lax, with an exception for
  cookies about two minutes old or less.
- `SameSite=None` needs `Secure`. The `__Secure-` and `__Host-` prefixes are
  enforced. Name plus value is at most 4096 bytes. Lifetime is at most 400 days.
- A `Domain` that is a public suffix is rejected, so the public suffix list is
  required.
  <https://www.ietf.org/archive/id/draft-ietf-httpbis-rfc6265bis-20.html>
- Partitioned cookies (CHIPS) key the jar by the top-level site.
  <https://developer.mozilla.org/en-US/docs/Web/Privacy/Guides/Privacy_sandbox/Partitioned_cookies>

**HTTP cache (RFC 9111).**

- Required: what may be stored, `no-store`, `no-cache`, `must-revalidate`,
  `Vary` matching, header update on 304, and invalidation after unsafe methods.
- Heuristic freshness is optional. The RFC suggests 10% of the time since
  `Last-Modified`. <https://www.rfc-editor.org/rfc/rfc9111.html>
- Chrome keys its cache by top-level site, frame site and URL. This cost about
  3.6% more cache misses.
  <https://developer.chrome.com/blog/http-cache-partitioning>
- No primary number was found for what a disk cache saves over a memory cache.

**Protocols and priorities.**

- 85% of requests use HTTP/2 or later. About 21-22% of home pages are still
  HTTP/1.1 and 7-9% are HTTP/3. <https://almanac.httparchive.org/en/2024/http>
- Chrome's default priorities: HTML, CSS in the head and fonts are highest.
  Synchronous scripts are high, async scripts low, images in the viewport high.
  <https://web.dev/articles/fetch-priority>
- The preload scanner is a second, speculative HTML tokenizer. It keeps
  fetching while the parser waits for CSS or a synchronous script.
  <https://web.dev/articles/preload-scanner>

**What others use.**

- Servo's `net` crate: `hyper` (HTTP/1 and HTTP/2), `hyper-rustls`, `tokio`,
  `cookie`, `async-compression` (brotli, gzip, zlib, zstd), `rusqlite`.
  <https://raw.githubusercontent.com/servo/servo/main/components/net/Cargo.toml>
- Ladybird: a separate RequestServer process, now one per top-level site, with
  the HTTP cache and third-party cookies split per site.
  <https://buttondown.com/ladybird/archive/this-month-in-ladybird-october-2026/>
- `reqwest`: pooling built in, HTTP/2 by default, cookies and decompression as
  features, HTTP/3 experimental. <https://docs.rs/reqwest/latest/reqwest/>

**Smallest version for Huginn [inference].**

- One network thread with a small tokio runtime and `hyper` + `hyper-rustls`,
  with HTTP/1.1 and HTTP/2.
- Do redirects, cookies and the referrer in Huginn's own Fetch layer, not in
  the HTTP client. Fetch needs cookie, SameSite, CORS and referrer logic on
  every hop.
- Skip HTTP/3. It is 7-9% of pages and always has an HTTP/2 fallback.

**Trap.** Huginn's TLS handshake does not look like a mainstream browser's.
Cloudflare fingerprints clients from the ClientHello.
<https://developers.cloudflare.com/bots/additional-configurations/ja3-ja4-fingerprint/>

## 2. The JavaScript engine

**Tiers, with numbers.**

- `--jitless`: Speedometer 2.0 is about 40% slower and the Web Tooling
  Benchmark 80% slower, but YouTube in a living-room test is only 6% slower.
  The heap shrinks by a median of 1.7%. WebAssembly is not supported.
  <https://v8.dev/blog/jitless>
- Lite mode: 22% less heap on average. With no feedback vectors, the experiment
  showed 12% worse page load time and 120% more CPU time on interactive pages.
  Most of its memory wins later shipped in default V8 (an 18% smaller heap)
  "without any significant impact". <https://v8.dev/blog/v8-lite>
- Sparkplug compiles bytecode to machine code in one pass: +41% on Speedometer
  over the interpreter alone. Maglev compiles about 10x slower than Sparkplug
  and 10x faster than TurboFan. <https://v8.dev/blog/sparkplug>,
  <https://v8.dev/blog/maglev>
- Pointer compression: up to 43% less V8 heap, with a 4 GB heap limit.
  <https://v8.dev/blog/pointer-compression>

Conclusion **[inference]**: `--lite-mode` is a poor trade today. Default flags
with Sparkplug (and perhaps Maglev, without TurboFan) get most of the speed
back for little memory. Keep `--jitless` as a hardened mode. Measure both on
the corpus before changing the default, because the benchmark counts memory.

**Snapshots and the code cache.**

- A startup snapshot cuts context creation from 40 ms to under 2 ms on desktop.
  Values of `Date.now` and `Math.random` taken while building it are frozen in.
  <https://v8.dev/blog/custom-startup-snapshots>
- Deno builds its JS runtime into a snapshot at build time and embeds it.
  <https://deno.com/blog/roll-your-own-javascript-runtime-pt3>
- Huginn's platform layer is JavaScript, which is the ideal case for a snapshot
  **[inference]**.
- Code cache: bytecode is serialised after the script runs and stored with the
  HTTP cache entry, for scripts of 1 KiB or more.
  <https://v8.dev/blog/code-caching-for-devs>
- Embedded builtins make the builtins a fixed cost, not a cost per isolate,
  which makes one isolate per tab affordable.
  <https://v8.dev/blog/embedded-builtins>

**Other engines.**

- QuickJS: 367 KiB, starts in under 300 µs, nearly all of test262, interpreter
  only. <https://bellard.org/quickjs/>
- Boa: over 90% of test262. <https://boajs.dev/>
- LibJS now has a JIT and is mostly Rust (Ladybird newsletter above).
- Verdict **[inference]**: stay on V8. Jitless V8 already beats the small
  interpreters, and bot checks assume the behaviour of a mainstream engine.

**Bindings.**

- Servo generates its Rust bindings from WebIDL with a Python generator.
- jsdom uses `webidl2js`: generated JS wrapper classes do type conversion and
  brand checks, and call hand-written implementation classes. This is the
  closest model to Huginn's. <https://github.com/jsdom/webidl2js>
- Deno's `op2`: operations marked `fast` use the V8 Fast API. Integers, floats,
  booleans and typed buffers qualify. Strings are fast only when Latin-1.
  <https://docs.rs/deno_core/latest/deno_core/attr.op2.html>
- Wrapper lifetime: Blink traces from JS into the DOM and back as one heap. The
  older approach caused leaks, and wrappers that were collected too early lost
  their properties. <https://v8.dev/blog/tracing-js-dom>

**Smallest version for Huginn [inference].**

- Keep one wrapper per node in a map by node id. Hold it strongly while the
  node is in a document or has listeners or script-added properties, and weakly
  otherwise.
- Generate argument conversion and brand checks from WebIDL into the JS layer.

**Trap.** A wrapper that is collected and loses its script-added properties
breaks frameworks without any error.

## 3. The event loop

**The standard design.**

- One task per turn, then a microtask checkpoint.
- Rendering order: resize and scroll, media queries, animations,
  `requestAnimationFrame` callbacks, style and layout, ResizeObserver,
  IntersectionObserver, paint.
- Rendering happens at most once per frame. Idle deadlines are at most 50 ms.
  <https://html.spec.whatwg.org/multipage/webappapis.html>
- Timers: after five levels of nesting, a timeout under 4 ms becomes 4 ms.
  <https://html.spec.whatwg.org/multipage/timers-and-user-prompts.html>

**Smallest version.** Implement that loop as written. Completed fetches and
XHRs are tasks posted from the network thread.

**Traps [inference].**

- A synchronous fetch resolves promises in an order that no site was tested
  against, and freezes input and timers.
- Running microtasks anywhere but after each task or callback breaks the
  schedulers of frameworks.
- `requestAnimationFrame` that is not tied to frames breaks animations and
  lazy loading.
- A missing `requestIdleCallback` means deferred hydration never runs.

## 4. Security

**What the big browsers do, and what it costs.**

- Site Isolation costs 9-13% of total memory. The number of renderer processes
  rises about 43.5% at the median. <https://www.usenix.org/system/files/sec19-reis.pdf>
- Chrome calls single-process mode "generally not a safe or robust process model".
  <https://chromium.googlesource.com/chromium/src/+/main/docs/process_model_and_site_isolation.md>
- Mozilla says separate OS processes are the only defence that is strong enough
  against Spectre-like attacks.
  <https://hacks.mozilla.org/2021/05/introducing-firefox-new-site-isolation-security-architecture/>
- The "rule of 2": never combine untrusted input, an unsafe language and high
  privilege.
  <https://chromium.googlesource.com/chromium/src/+/main/docs/security/rule-of-2.md>
- About 70% of Chrome's high-severity bugs are memory safety bugs.
  <https://www.chromium.org/Home/chromium-security/memory-safety/>

Rust removes the "unsafe language" leg for Huginn's DOM and network code. V8 is
C++ and runs the attacker's code with the user's full privileges. `--jitless`
removes executable memory at run time, which is a real hardening.

**Cheap mitigations.**

- Landlock lets an unprivileged process restrict itself: files since Linux
  5.13, TCP bind and connect from ABI 4.
  <https://docs.kernel.org/userspace-api/landlock.html>
- Chromium on Linux layers namespaces and seccomp-bpf.
  <https://chromium.googlesource.com/chromium/src/+/main/sandbox/linux/README.md>
- Ladybird's helper processes sandbox themselves with seccomp and Landlock, or
  Seatbelt on macOS.

**A responsible minimum for Huginn [inference].**

1. Landlock for the whole process: fonts and configuration read-only, write
   access to the profile directory only.
2. A seccomp filter that denies `execve` and `ptrace`.
3. Same-origin checks in Rust, in the native operations (fetch, cookies,
   storage, frames), not in JavaScript that the page can reach.
4. One isolate per tab.
5. HttpOnly cookies never reach JavaScript.
6. An honest warning ("not for sensitive logins") until the engine runs in its
   own sandboxed process.

**What a single process gives up.** A bug in V8 or in a native operation
compromises the user's account. Sites that share the process have no Spectre
defence against each other.

## 5. Web compatibility

- **Running WPT.** `./wpt run`, hosts file entries, a trusted test CA, JSON
  reports with `--log-wptreport`.
  <https://web-platform-tests.org/running-tests/from-local-system.html>
- **Ladybird** wraps this in `Meta/WPT.sh run|compare|import`, and copies newly
  passing tests into its tree as regression tests.
- **Servo** tracks pass rates per focus area on its own dashboard.
  <https://servo.org/blog/2023/07/20/servo-web-platform-tests>
- **User-Agent.** Ladybird's old string got "degraded UIs, 'your browser is not
  supported' pages, network throttling, and outright HTTP 403 errors". Adding
  `Chrome/140.0.0.0 AppleWebKit/537.36 Safari/537.36`, with "Ladybird" kept,
  "immediately unlocked modern versions of Google Search, Gmail, Instagram".
  <https://ladybird.org/newsletter/2026-01-31>
- **Bot checks.** Google Search has required JavaScript since January 2025.
  <https://techcrunch.com/2025/01/17/google-begins-requiring-javascript-for-google-search/>
  Cloudflare warns that "custom or heavily modified browser engines" may have
  trouble with its challenges.
  <https://developers.cloudflare.com/cloudflare-challenges/reference/supported-browsers/>
- A challenge script usually sets a cookie, posts a result and reloads. Huginn
  can do none of the three today **[inference]**.
- No measured ranking of which missing features break the most sites was
  found. The order in the list below is judgement.

## 6. "No bloat" in practice

- Most of Chrome's weight is deliberate isolation: a process per site, spare
  renderers and utility processes. Its average renderer shrank from 87.2 MB to
  42.3 MB under Site Isolation while the number of them rose (USENIX paper above).
- Firefox in 2021 ran eight content processes, up to two semi-privileged ones
  and four utility processes (Mozilla post above).
- Ladybird has moved toward more processes: compositor, network per site,
  audio, image decoder.
- So "one process" is Huginn's largest memory advantage and also its largest
  security gap. Decide this trade on purpose.
- Dependencies **[inference]**: a hyper, tokio and rustls stack is far smaller
  than V8. V8's flags and snapshot are where discipline about memory and binary
  size pays off.

## The ten changes that would help most

Ordered by compatibility and safety gained for the work (judgement).

1. **A cookie jar.** The RFC 6265bis storage model, SameSite Lax by default,
   prefixes, the public suffix list, `document.cookie`, and HttpOnly hidden
   from JavaScript.
2. **A Fetch layer shaped like the specification.** All methods, request
   headers and bodies, CORS with preflight, credentials modes, the redirect
   rules, `Origin` and `Referer`. This fixes the bbc.com loop.
3. **The referrer.** Implement `strict-origin-when-cross-origin` and fill in
   `document.referrer`.
4. **Asynchronous networking.** A network thread (hyper, rustls, tokio, HTTP/2,
   pooling, brotli, gzip and zstd) that posts tasks when requests finish.
   Synchronous XHR stays the only blocking path.
5. **The HTML event loop as written.** A microtask checkpoint per task, the
   4 ms timer clamp, `requestAnimationFrame` and observers tied to frames,
   `requestIdleCallback`.
6. **One User-Agent story.** A Chrome-compatible string with a Huginn token,
   the same in HTTP and in `navigator`.
7. **V8 settings.** Measure default V8 with Sparkplug against `--lite-mode`,
   keep `--jitless` as a hardened mode, and move the JS platform layer into a
   startup snapshot.
8. **A sandbox for the whole process.** Landlock and seccomp, origin checks in
   Rust, one isolate per tab. A separate sandboxed process for V8 comes later.
9. **Storage and observers.** `localStorage` and `sessionStorage` kept per
   origin on disk, the History API, complete Mutation, Intersection and Resize
   observers. IndexedDB, import maps and CSS animations come after.
10. **A disk cache and a WPT runner.** An RFC 9111 disk cache keyed by
    top-level site, with the V8 code cache attached to entries. A script that
    runs WPT and tracks `/fetch`, `/cookies`, `/html/webappapis` and `/dom`.
