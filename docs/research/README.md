# Research notes

Date: 2026-11-12.

These notes survey how browsers are built, to guide work on Huginn. The goal
is the one in the main README: small, fast, one process, and no bloat, while
real sites still work.

| file | what it covers |
|---|---|
| [engines.md](engines.md) | How Blitz, Servo, Ladybird and smaller engines are built, and what to borrow |
| [rendering.md](rendering.md) | Incremental style and layout, CPU paint, text, images, scheduling, memory, measurement |
| [platform.md](platform.md) | Networking, cookies, the JavaScript engine, the event loop, security, web compatibility |

## How to read them

- The notes were gathered by AI research agents from public sources in one
  session, and then edited. They are a starting point, not a specification.
- Each claim links to its source. Claims marked **[unverified]**, **[U]** or
  **[inference]** were not checked against a fetched source. Check a number at
  its source before you quote it elsewhere.
- Numbers from other browsers describe those browsers. Measure Huginn before
  and after each change (`bench/`, and the timing JSON of `huginn render`).
- The "Where Huginn is today" sections describe commit `8cca1e8`, the Linux
  port (pull request #1).

## What the research says, in one page

**1. Huginn's biggest performance gap is that nothing is incremental.** Style
and layout run in full after every change. Every engine surveyed puts most of
its performance work here. Blitz shows the way on the same libraries: Stylo
snapshots and restyle hints, a damage class per change (repaint, relayout,
rebuild boxes), and Taffy's per-node cache cleared only along the dirty path.
CSS transitions and animations come almost free once this exists.

**2. Huginn's biggest compatibility gap is the network layer, not rendering.**
There are no cookies, script requests are same origin and GET only, and
`document.referrer` is empty. Both real-site failures traced so far (the
bbc.com redirect loop and Google's bot check) come from this. A cookie jar and
a Fetch layer shaped like the specification are the two changes that unblock
the most sites.

**3. "One process" is both the advantage and the risk.** It is why Huginn uses
a fraction of the memory of Chrome or Ladybird, whose weight is mostly process
isolation. It also means a bug in V8 or a native operation has the user's full
privileges. The cheap middle ground is to sandbox the whole process (Landlock
and seccomp on Linux, Seatbelt on macOS) and to enforce origin checks in Rust.

**4. Some current choices deserve a second measurement.**

- `--lite-mode`: V8's own numbers show worse page load and much more CPU on
  interactive pages, and most of its memory savings later shipped in default
  V8. A startup snapshot of the JS platform layer would cut context creation
  from tens of milliseconds to under two.
- The two User-Agent strings (HTTP and `navigator`) disagree. Ladybird found
  that a Chrome-compatible string with its own token unlocked the modern
  versions of large sites.

**5. Things to keep.** The arena DOM, the single process, sequential Stylo,
CPU raster with kept strips, tab discarding, and the rule "fix the browser,
not the page". The research supports each of them.

## A suggested order of work

Each step makes the next one cheaper or safer to do.

1. **Measurement.** Tracing spans per phase, a memory reporter per subsystem,
   and a WPT runner that prints a diff. Without these, later steps cannot be
   judged.
2. **Network correctness.** Cookie jar, Fetch with CORS, referrer, one
   User-Agent story. See [platform.md](platform.md), items 1-3 and 6.
3. **Incremental style and layout.** Stylo invalidation, damage classes, kept
   Taffy caches, differential tests against a fresh layout. See
   [rendering.md](rendering.md), items 2-4, and [engines.md](engines.md).
4. **Asynchronous networking and the event loop as specified.** A network
   thread, completed requests as tasks, frames tied to
   `requestAnimationFrame`.
5. **Animations and partial repaint.** Stylo's animation sets, damage
   rectangles, present with damage.
6. **A sandbox for the whole process**, before recommending Huginn for
   everyday browsing.
7. **V8 startup and settings, disk cache, storage on disk.**

Steps 2 and 3 do not depend on each other and can run in parallel.

## Open questions

- How much memory does keeping Stylo's element data between passes cost per
  page? ARCHITECTURE.md counts on it being freed after `compute`.
- Does default V8 with Sparkplug stay inside the memory goal on the corpus?
- Is a separate sandboxed process for V8 worth its memory, or is a
  whole-process sandbox enough for Huginn's aims?
- No measured ranking was found of which missing web features break the most
  sites. The orders in these notes are judgement.
