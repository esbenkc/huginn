# Huginn architecture

Huginn is a small web browser in Rust. The goal is to beat Ladybird on the
benchmark in [`bench/`](bench/) (rules in `bench/WIN.md`): lower
peak RSS and lower wall time on at least 80% of the 14 corpus pages, and a
screenshot that is about as close to Chrome as Ladybird's on at least 50%.
Speedometer and other JS throughput tests are out of scope. We will not beat
a JIT-heavy engine there, and the bench does not try.

The interfaces between crates are in [CONTRACTS.md](CONTRACTS.md).

## Crates

| crate | what it does | depends on |
|---|---|---|
| `huginn-dom` | Arena DOM (`Document`, `NodeId`) and the html5ever tree sink | html5ever, markup5ever |
| `huginn-style` | CSS parsing, selector matching, cascade, computed style (`StyleEngine`, `StyleMap`, `ComputedStyle`) | huginn-dom, cssparser |
| `huginn-layout` | Box tree, block/flex/grid layout, inline text layout (`LayoutEngine`, `LayoutTree`) | huginn-dom, huginn-style, huginn-net, Taffy, Parley |
| `huginn-paint` | Display list (`DisplayList`) and the CPU rasterizer (`CpuRasterizer`), PNG encoding | huginn-layout, huginn-style, huginn-net, vello_cpu, png |
| `huginn-net` | Fetching (`Loader`: http, https, file, data), in-memory cache, cookies (`CookieJar`), CORS and referrer policy, charset decoding, image/SVG/font decoding (`ImageStore`, `decode_font`), media queries and `srcset` for resource choice, the user agent (`ua`) | ureq, url, data-url, psl, image, avif-parse, rav1d, vello_common, resvg, wuff, encoding_rs |
| `huginn-js` | Script engine: V8 (cargo feature `v8`, on by default), the JavaScript DOM layer (prelude), native DOM operations, modules | huginn-dom, huginn-net, huginn-style, v8 |
| `huginn-shell` | The `huginn` binary. Runs the page-load pipeline and prints the timing JSON | all of the above |

The dependency graph has no cycles. `huginn-dom` and `huginn-net` are leaves. Only
`huginn-shell` knows about every crate, so it is the only place where phases are
ordered.

## Data flow

```
 URL or file path
   |  huginn_net::parse_target
   v
 Loader::navigate                   huginn-net    bytes (cookies, redirects); Resource::html_text picks the charset
   |  huginn_dom::parse_html, then resources::plan_images (srcset, <picture>, lazy)
   v
 Document (arena of Node)           huginn-dom    NodeId per node
   |  collect <style>, <link rel=stylesheet media=...>
   v
 Loader::fetch_all(stylesheets)     huginn-net    up to 6 threads, cached for @import
   |  StyleEngine::add_stylesheet (UA sheet + author sheets, @import); then Loader::clear_cache
   v
 StyleEngine::compute(&Document)    huginn-style  StyleMap: Arc<ComputedStyle> per element
   |  resources::load: one parallel batch of <img> (not display:none, decoded at
   |  display size), CSS backgrounds, inline <svg> (resvg) and the @font-face
   |  files the text uses (WOFF2 -> sfnt); bodies dropped after decoding
   v
 LayoutEngine::layout(LayoutInput)  huginn-layout LayoutTree: flat boxes + text runs + glyphs
   |  resources::load_lazy: loading=lazy images in the painted region (patched
   |  into the tree, or a second layout if one sets its own box size)
   |  huginn_paint::build_display_list(&tree, cull = viewport)
   v
 DisplayList<'tree>                 huginn-paint  borrows glyphs and images, no copies
   |  CpuRasterizer::rasterize(list, region, scale)
   v
 RgbaImage -> huginn_paint::encode_png  huginn-paint  PNG on disk
```

The pipeline lives in `crates/huginn-shell/src/pipeline.rs`. `pipeline::load`
runs phases 1-6 (fetch to style, plus the second fetch wave) and returns a
`Page`; `Page::register_fonts` and `Page::layout` do layout. Both `render` and
the `open` window use them, so new pipeline steps (scripts, font decoding) go
into `load` once and both modes get them. Which subresources a page needs is decided in
`crates/huginn-shell/src/resources.rs` (owned by huginn-net). `pipeline::render` times each phase
and prints one JSON line. The keys are `url`,
`fetch_ms`, `parse_ms` (HTML plus CSS parsing), `style_ms`, `layout_ms`,
`paint_ms` (display list plus raster), `encode_ms`, `total_ms` (process start
to PNG written), `nodes`, `styled`, `boxes`, `images`, `width`, `height`.

With the `v8` feature (the default), the shell runs scripts between CSS parsing and style
(`crates/huginn-shell/src/scripts.rs`): scripts in order, DOMContentLoaded and
load, then timers for up to 500 ms of virtual time. Script sources are
fetched in the first wave with the stylesheets and images. Afterwards the
shell picks up stylesheets and images that scripts added, then styles and
lays out once. Geometry queries during scripts (`getBoundingClientRect`,
`offsetWidth`, `getComputedStyle`) run style and layout on demand, within a
400 ms budget. The JSON line then also has `script_ms`, `scripts` and
`js_errors`, and the script phase in parts: `script_run_ms` (the scripts
the parser found), `script_events_ms` (DOMContentLoaded and load),
`script_timers_ms` (timers while the page settles), `script_layout_ms` and
`script_layouts` (style and layout that scripts asked for), and
`script_wait_ms` (time the thread waited instead of running, mostly for the
network; Linux only, `null` elsewhere). `--no-js` (or `HUGINN_NO_JS=1`) turns scripts off.

## Threading

- `main` parses the arguments and spawns one `render` thread with a 256 MiB
  stack. Style and layout recurse on deep DOMs. Only touched stack pages are
  committed, so the big stack costs no RSS.
- `Loader::map_parallel` runs fetches and image decodes on up to 6 scoped
  threads. Results keep input order.
- Parsing, style, layout and paint run on the render thread. They are
  single-threaded today.
- `Document` is not `Send` or `Sync`. Its strings are `StrTendril`, which has a
  non-atomic refcount. It stays on the render thread.
- Planned: Stylo's parallel traversal (rayon) for style, and vello_cpu's
  `multithreading` feature for raster. Both only pay off on big pages, and
  both add threads and memory, so measure before switching them on.
- V8 runs on the render thread with `--single-threaded` (no background
  compiler or GC threads) and `--lite-mode` (interpreter only: no JIT code,
  lazy feedback vectors; about half the V8 heap on script-heavy pages at the
  same wall time). It gets the document through `HostContext` for each
  call. A small watchdog thread terminates a call that runs past 2 s.
- `huginn open` has two threads of its own; see the next section.

## The window (`huginn open`)

The browser is called **Huginn** in everything a user sees (window titles,
`huginn://` pages), and the binary is `huginn`.

Files: `window.rs` (winit and softbuffer, one native window per browser
window), `session.rs` (all windows and what they share), `browser.rs` (one
window: tabs, tab strip, toolbar, input routing, frame), `tab.rs` (one tab:
engine thread, history, page layer, zoom, discarding), `tabstrip.rs`,
`toolbar.rs`, `shortcuts.rs` (window chords), `internal_pages.rs`
(`huginn://newtab`, `history`, `bookmarks`, address bar input), `engine.rs`
(the per-tab engine thread), `headless.rs` (the command driver), `input.rs`,
`ui.rs`, `clipboard.rs`. In-page modules plug in through fixed hooks:
`interact/` (selection, find, focus, menus, save, print) and `live.rs` (live
JavaScript); see CONTRACTS.md, "huginn-shell window: tab hooks".

- **Threads.** The main thread runs the winit event loop and owns the UI of
  every window. Each tab has its own `engine` thread (256 MiB stack, only
  touched pages committed) that owns its `Page` (the `Document` is not
  `Send`). It runs `pipeline::load` and layout and sends the finished
  `LayoutTree` and a link table to the main thread, tagged with the tab id.
  Windows stay responsive while pages load. Commands carry a navigation
  generation; stale answers are dropped. A burst of resizes is coalesced.
  Closing or discarding a tab drops its command sender; the thread ends and
  frees the page.
- **Tabs and discarding.** A background tab keeps its `LayoutTree` and
  engine page but no raster strips. At most 8 background tabs stay live
  (`HUGINN_LIVE_TABS`); the least recently active ones beyond that are
  discarded like Chrome's tab discarding: engine thread, DOM, styles and
  layout go, history, title and favicon stay, and the tab loads again (at
  its scroll position) when activated. When the process footprint passes
  1.5 GB (`HUGINN_MEMORY_BUDGET_MB`), background tabs are discarded one per
  event until it drops. Background tabs relayout only when activated.
  `Command::SetVisible` tells the engine which tab is in front (timers).
- **Page layer.** The main thread rasterizes the active page in full-width
  strips of 256 CSS px at `scale * zoom`, from the `LayoutTree` it holds.
  Scrolling only copies strip rows into the frame. A strip is rasterized
  the first time it is visible, and one strip above and one below the
  viewport are prefetched when idle. Only those strips are kept (LRU).
- **Zoom.** Per site (host, or URL for hostless pages), shared by all tabs
  and windows, Chrome's steps 25 %..300 %. A zoomed page lays out at
  `window width / zoom` CSS px and rasterizes at `scale * zoom`; the
  toolbar shows the level.
- **Tab strip and toolbar.** Small HTML documents rendered by our own
  pipeline (`ui::render_snippet`) with the window's own `LayoutEngine`,
  rendered again only when what they show changes. The strip is in the
  title bar area (transparent title bar, full-size content view, room for
  the traffic lights): tabs with favicon (`<link rel=icon>` or
  `/favicon.ico`, fetched once per session by a short thread; a CSS globe
  otherwise), title and close button (active and hovered tabs), a + button.
  Tabs shrink from 240 px to 36 px. Loading tabs show a blue spinner drawn
  on top each animation frame. The toolbar has back, forward, reload (stop
  while loading) and the address field with a lock (https), "Not secure"
  (http), "File" or "Huginn" chip, the zoom level and a bookmark star.
  Private windows have a dark strip and a "Private" badge.
- **Display.** softbuffer hands each CPU frame to a `CALayer`. No GPU
  device is created. The event loop sleeps (`ControlFlow::Wait`); it only
  wakes on a timer while a spinner animates.
- **Internal pages.** `huginn://newtab` (wordmark and tiles of bookmarks and
  recent sites), `huginn://history` (this session's visits of normal
  windows), `huginn://bookmarks`. The UI makes their HTML; the engine loads
  it with scripts off and shows it at the `huginn:` URL. Bookmarks live in
  `~/Library/Application Support/Huginn/bookmarks.tsv` (`url<TAB>title`;
  `HUGINN_PROFILE_DIR` overrides). Address bar text that is not a URL searches
  `https://duckduckgo.com/html/?q=` with no other parameters.
- **Tests.** `huginn open <url> --commands "wait; key cmd+t; click 50 100
  cmd; state"` drives the same `Session` without a window and prints JSON
  (`tests/open.rs`, `tests/tabs.rs`). `--script` runs the same commands in
  the real window and quits after the last one; `capture <png>` saves a
  `screencapture` of it.
- **Measured (2026-11-12, fastdev build with V8, 1280x800 window at 2x,
  corpus pages from `serve.py`, tabs cycling through the 14 pages).**
  Physical footprint (RSS) after load: 1 tab 110 MB (191), 5 tabs 133 MB
  (221), 20 tabs 207 MB (302) with 11 tabs discarded, 215 MB (324) with all
  20 live. Idle CPU with 20 live tabs: no CPU time over 10 s (all threads
  blocked, checked with `sample`). Isolates are still dropped after load
  here; live JavaScript adds its per-tab cost.

### Shortcuts and gestures (Chrome on macOS)

On Linux, Ctrl takes the place of Cmd in every chord below, and `window.rs`
(`native_chord`) maps Chrome's Linux chords that differ onto these: F5 and
Ctrl+F5 (reload, hard reload), F11 (full screen), F6 and Alt+D (address bar),
Alt+Left and Alt+Right (back, forward), Alt+Home (home), Ctrl+Tab and
Ctrl+PageDown / Ctrl+PageUp (next and previous tab), Ctrl+H (history),
Ctrl+Shift+O (bookmarks).

| chord or gesture | action | status |
|---|---|---|
| Cmd+T | new tab: `huginn://newtab`, address bar focused | done |
| Cmd+N / Cmd+Shift+N | new window / new private window (no history) | done |
| Cmd+W / Cmd+Shift+W | close tab (window with its last tab) / close window | done |
| Cmd+Shift+T | reopen last closed tab with its history | done |
| Cmd+1..8 / Cmd+9 | tab N / last tab | done |
| Cmd+Opt+Right, Ctrl+Tab, Cmd+Shift+] | next tab | done |
| Cmd+Opt+Left, Ctrl+Shift+Tab, Cmd+Shift+[ | previous tab | done |
| Cmd+Q / Cmd+M / Cmd+Ctrl+F | quit / minimize / fullscreen | done |
| Cmd+L | focus the address bar (select all) | done |
| Enter / Opt+Enter / Cmd+Enter / Shift+Enter in the address bar | go / new tab / new background tab / new window; non-URLs search DuckDuckGo | done |
| Left/Right, Opt+Left/Right, Cmd+Left/Right, Home/End, Backspace, Opt+Backspace, Cmd+Backspace, Delete, Cmd+A/C/X/V, Esc in the address bar | caret editing; Esc puts the URL back | done |
| Cmd+[ / Cmd+Left, Cmd+] / Cmd+Right, mouse buttons 4/5 | back / forward | done |
| Cmd+R / Cmd+Shift+R | reload / hard reload (pages are never cached, so the same) | done |
| Esc | stop loading (the previous page stays) | done |
| Cmd+Shift+H | home: `huginn://newtab` | done |
| Cmd+Y | `huginn://history` in a new tab | done |
| Cmd+D / Cmd+Opt+B | bookmark toggle (star in the address bar) / `huginn://bookmarks` | done |
| Cmd+Shift+Delete | clear history (session visits and closed tabs), shows the history page | done |
| Cmd+= / Cmd+- / Cmd+0 | zoom in / out / reset, per site | done |
| Space, Shift+Space, PageUp/Down, arrows, Home/End, Cmd+Up/Down | scroll | done |
| click / Cmd+click / Cmd+Shift+click / Shift+click / middle click on a link | open / background tab / foreground tab / new window / background tab | done |
| hover a link | URL bottom left | done |
| tab strip: click, middle click, x, +, drag | switch, close, close, new tab, reorder | done |
| empty tab strip drag | moves the window | done |
| toolbar back / forward / reload (stop) / star | as the keys | done |
| Cmd+F/G/Shift+G/E, Cmd+A/C, Cmd+U/S/P, right-click (Ctrl+click) menu, drag/double/triple-click selection, inline links | in-page interaction | done (`interact/`, next section) |
| Tab/Shift+Tab focus, Enter/Space on a focused link or button, typing in forms, Cmd+A/C/X/V/Z in fields, JS events | live page | done (`live.rs`, `livepage.rs`, next sections) |
| two-finger swipe back/forward | | not done (optional) |
| drag a tab out into its own window | | not done (optional) |
| Cmd+Shift+C, Cmd+Opt+I/J/U | developer tools | out of scope: Huginn has no devtools |
| Cmd+Shift+B, Cmd+, | bookmarks bar, settings | out of scope: no bookmarks bar, no settings UI |

### In-page interaction (`interact/`)

Selection, find, the context menu, view source, save and print. All of it
runs on the UI thread from the `LayoutTree` the tab already holds, except
print and save, which run as jobs on the tab's engine thread.

- **Text index.** huginn-layout keeps, for the window only, the rendered text of
  every run with its grapheme clusters (x, advance) and line box
  (`TextIndex`). Selection, find and copy are byte ranges in that text.
  Cost: 0.1 MB on hackernews, mdn, github and bbc, 0.46 MB on the
  wikipedia article (RSS within run-to-run noise). `render` does not build it.
- **Overlays.** Selection (Chrome's light blue) and find matches (yellow,
  active orange) are multiplied over the composed page pixels, so text
  stays dark and the strips are never rasterized again. Rectangles are
  computed once per search, selection change or relayout and cut to the
  overflow clips of their boxes; text clipped away (screen-reader-only
  labels) gets no match. The find bar, the context menu and the notice
  bubble are small HTML documents rendered by our own pipeline with one
  shared `LayoutEngine` per UI thread.
- **Inline links.** Hit testing takes the deepest box, then the innermost
  inline fragment of its inline formatting context, so an `<a>` inside a
  paragraph is clickable.
- **Print (Cmd+P).** The engine thread restyles and lays the page out at
  the printable width of US Letter with Chrome's default 0.4 in margins
  (739 x 979 CSS px), breaks pages above any line of text a break would
  cut, rasterizes each page at 2x and writes a PDF of Flate images to
  `~/Downloads` (`$HUGINN_DOWNLOADS` in tests); then it restores the
  screen styles. The wikipedia article prints as 9 pages (4.2 MB).
- **Save (Cmd+S)** fetches the document again and writes its bytes as
  `<title>.html`. **View source (Cmd+U)** opens `view-source:<url>`; the
  engine fetches the source and shows it with line numbers and Chrome's
  syntax colors.

| input | does | status |
|---|---|---|
| drag | select text (autoscrolls past the page edge; a drag from a link selects, it does not open the link) | done |
| double / triple click | select word / paragraph; Shift+click extends | done |
| Cmd+A, Cmd+C | select all, copy (macOS pasteboard) | done |
| Cmd+F; Enter / Shift+Enter, Cmd+G / Cmd+Shift+G; Esc | find bar with live highlights and "3/17"; next, previous (wraps); close (the active match becomes the selection) | done |
| Cmd+E | use the selection for find | done |
| right click, Ctrl+click | context menu (a right click on a word selects it first, as on macOS): link (open in new tab / window / private window, copy address), selection (copy, search DuckDuckGo), page (back, forward, reload, save, print, select all, view source); arrows, Enter, Esc | done |
| Cmd+U, Cmd+S, Cmd+P | view source tab, save HTML, print to PDF | done |
| Tab / Shift+Tab, Enter on a focused link | focus order in `focus::next_focusable`; the live-JS input code runs it as the keydown default action and paints the `:focus-visible` ring | done (live page) |
| Cmd+Shift+C, Cmd+Opt+I/J/U | devtools | out of scope: Huginn has no devtools |

Not yet: Shift+arrow keys to extend a selection, the selection drag
image, `@media print` styles (print uses the screen styles; huginn-style
matches media as `screen`), selectable text in the PDF (pages are images).


### Live JavaScript and form controls (`live.rs`, `livepage.rs`)

The window keeps each page's script engine after load (`pipeline::load_document`
with `keep_js`; `huginn render` still drops the isolate before style).
`livepage.rs` runs on the tab's engine thread, `live.rs` on the UI thread.

- **Timers.** The engine thread sleeps until a command or the next timer
  (`recv_timeout`). Timers run in real time from load (the window skips the
  500 ms virtual settle of `render`). Background tabs run them at most once
  a second and give unused V8 heap back (`trim_memory`).
- **Input.** Every page pointer and key event goes to the engine
  (`PageInput`); moves are coalesced per batch. The engine fires pointer,
  mouse, keyboard, focus, input, change and submit events through the
  prelude (trusted, with `clientX`, `key`, modifiers ...), then runs the
  default action from `huginn_js::activation`: links (after click handlers, so
  `preventDefault` works; pages without scripts follow links in the UI at
  once), checkbox and radio (toggled before the click, restored if
  canceled), `<label>`, `<summary>`, `<select>` (drawn drop-down with the
  interact `Menu`), GET and POST form submission (implicit submission on
  Enter), Tab focus navigation, Enter/Space on buttons and links.
- **Text fields.** Value in the `value` attribute (a textarea's text).
  Caret, selection, word/line motion (Alt/Cmd+arrows), Backspace/Delete,
  Cmd+A/C/X/V/Z with undo, `beforeinput`/`input`, `change` on blur,
  `maxlength`, `readonly`. The caret and the selection are drawn from the
  text index clusters of the field's runs.
- **Restyle.** After an engine call, a changed DOM, or a hover/active/focus
  change used by some rule (`StyleEngine::uses_state`), restyles (picking up
  added stylesheets), loads images that became needed
  (`resources::load_added`) and lays out again (`Relaid` with links and
  title). Style and layout are full passes, not incremental, with one
  exception: the tab's layout engine keeps the shaped text of each inline
  formatting context from one pass to the next (`huginn-layout/src/shape.rs`),
  so a pass shapes only text whose content, styles or fonts changed.
  Shaping was 45 to 90 % of a pass. `HUGINN_SHAPE_CACHE=0` turns it off.
- **Requests from scripts.** `location.href/assign/replace/reload`,
  `form.submit()`, `requestSubmit()`, `a.click()` become
  `HostRequest::Navigate`; `javascript:` URLs run in the page.
- **Memory** (headless, 1280x800, max RSS): TodoMVC 37 MB live vs 19 MB
  with `--no-js`, so the kept isolate costs about 18 MB per scripted tab.
  A page without scripts gets no isolate (a.html 16.9 vs 16.8 MB); pages
  with only inline `on*` handlers get one on their first input event.
- **Not yet.** TodoMVC: typing and Enter add items, but its toggle circle
  and the filter links do not update the list yet (to debug: the toggle's
  `change` in React 18's event system, `hashchange` routing). Form named
  access (`form.q`), `form.reset()` restoring defaults, IME, multi-line
  caret by clicks in wrapped textareas, `<select multiple>`, drag and drop,
  `contenteditable`. XHR/fetch after load block the tab's engine thread.

## Timing (`perf.rs`)

`crates/huginn-shell/src/perf.rs` records how long the phases take while the
window runs: `style` and `layout` (engine threads), `update` (one whole page
update after input or a script), `raster` (one strip), `compose` (one frame)
and `present`. It keeps the count, the total, the maximum and the last 512
durations per phase. The `perf` command of the command driver prints them as
one JSON line (median and 95th percentile); `perf reset` forgets them. The
`frame` command composes a frame without a window. `bench/perf.py` uses these
to time scrolling, hover, typing, script changes and resizing on synthetic
pages.

`HUGINN_V8_FLAGS` replaces the V8 flags, to measure other settings.

## Memory strategy

Peak RSS is half of the win condition. Ladybird runs several processes (UI,
WebContent, RequestServer, ImageDecoder). Huginn is one process. That is
our biggest memory lever, so keep it that way: no helper processes.

- **Arena DOM.** All nodes are in one `Vec<Node>`. Links are `Option<NodeId>`,
  4 bytes each thanks to the `NonZeroU32` niche. No `Rc`, no `RefCell`, no
  per-node allocation for tree structure. Side tables in other crates
  (`StyleMap`) are plain `Vec`s indexed by `NodeId::index()`.
- **Interned atoms.** Element and attribute names are `LocalName`,
  `Namespace` and `Prefix` atoms from `markup5ever`/`web_atoms`. Known HTML
  names are static atoms (no allocation). These are the same atom types that
  Stylo 0.24 uses.
- **Zero-copy text.** html5ever hands text to the tree sink as `StrTendril`.
  The DOM keeps the tendril; small strings are inline, big ones share buffers.
- **Shared computed style.** `StyleMap` stores `Arc<ComputedStyle>`. Layout
  boxes hold a clone of the same `Arc`, not a copy.
- **Flat layout output.** `LayoutTree` is a few flat `Vec`s (boxes, runs,
  glyphs, variable-font coords, decorations). Boxes and runs point into them
  with `Range<u32>`.
- **Borrowed display list.** `DisplayItem<'a>` borrows glyph slices, runs and
  images from the `LayoutTree`. Nothing is copied to paint.
- **Decode once.** `decode_image` produces one `Arc<Pixmap>`. Layout reads its
  size, paint draws it. There is no second copy.
- **Paint only the viewport.** The display list culls boxes outside the
  viewport, and the rasterizer allocates only `width x height` pixels.
- **Bodies are not kept.** The document, images and fonts are fetched without
  the cache and dropped right after parsing or decoding. Stylesheets are
  cached only until CSS parsing ends (`Loader::clear_cache`). Images are
  decoded only when they generate a box, at most at about twice their
  displayed size, and `loading=lazy` images only when layout puts them in
  the painted region. Web fonts load only for faces the page text uses.
- **Known waste** (owner tasks in CONTRACTS.md): identical computed styles
  are not shared. Detached DOM nodes are never freed.

## User agent

Huginn sends the tokens of Chrome's reduced user agent that sites sniff for
a modern engine, names itself, and does not claim to be Chrome:

```
Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Huginn/0.1 Safari/537.36
Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Huginn/0.1 Safari/537.36
```

Why:

- Sites sniff the user agent. With the old `Mozilla/5.0 (Macintosh; Intel
  Mac OS X 10_15_7) Huginn/0.1`, google.com served its old basic-HTML page,
  then a script bot check that failed, then HTTP 429. With the
  `AppleWebKit/537.36 (KHTML, like Gecko)` and `Safari/537.36` tokens it
  serves the current page. Other new engines also borrow tokens: Servo
  sends a Firefox string with `Servo/1.0` in it.
- No `Chrome/` token. Bot filters compare a Chrome claim with the TLS and
  HTTP fingerprint of the connection. Huginn's (rustls, HTTP/1.1) is not
  Chrome's, so a Chrome claim reads as a bot and gets a hard 403. Huginn
  does not imitate Chrome's fingerprint; that would be bot-detection
  evasion. Each cell below is one or more live requests (2026-11-12, no
  cookies, same request; only the user agent differs). "Chrome" is this
  string with `Chrome/152.0.0.0` before `Safari/537.36` and the Huginn
  token at the end.

  | site | old (`Huginn/0.1` only) | Chrome + Huginn | this string |
  |---|---|---|---|
  | google.com | old basic page | current page | current page |
  | pexels.com | 200 | 403 | 200 |
  | canva.com | 200 | 403 | 200 |
  | coinbase.com | 200 | 403 | 200 |
  | crunchyroll.com | 200 | 403 | 200 |
  | medium.com | 403 | 200 | 200 |
  | openai.com | 403 (200 on a retry) | 200 | 200 |
  | quora.com | 403 | 403 | 403 (200 with the client hints) |
  | zillow.com | 200 | 200 | 403 |
  | instagram.com | 200, 417 KB | 200, 518 KB | 200, 417 KB |

  The same for every string: 200 from dr.dk, bbc.com, reddit.com, x.com,
  youtube.com, discord.com, gitlab.com, notion.so, figma.com and
  cloudflare.com; 403 from stackoverflow.com, npmjs.com, udemy.com, etsy.com
  and nytimes.com (cookie or script challenges); 401 from wsj.com; 202 from
  amazon.com.
- `Huginn/<major.minor>` names the browser. Sites and server logs can tell
  Huginn apart, and a site can work around a Huginn bug. The platform part
  is Chrome's frozen one (`10_15_7` on every macOS version), so the string
  says nothing beyond the OS family and the Huginn version.
- One source, `huginn_net::ua`, feeds the `User-Agent` header,
  `navigator.userAgent`, `navigator.userAgentData` and the `Sec-CH-UA`,
  `Sec-CH-UA-Mobile` and `Sec-CH-UA-Platform` headers. The brands are
  `Chromium` 152, `Huginn` and a GREASE brand: Huginn runs Chrome 152's
  JavaScript engine (V8 15.2), so scripts that check the Chromium version
  get the JavaScript features they expect. A test in
  `crates/huginn-js/tests/v8.rs` fails when the `v8` crate moves and
  `ua::CHROME_MAJOR` does not. As in Chrome, the client hints go only to
  `https:` and loopback URLs, and only these low-entropy ones; Huginn
  ignores `Accept-CH`.
- HTTP/2 would be a separate improvement (for speed), not a way to look
  like Chrome.

The image `Accept` header lists only formats `decode_image` decodes:
`image/avif,image/webp,image/svg+xml,image/*,*/*;q=0.8`. Image servers and
CDNs pick the format from it. Listing a format Huginn could not decode
made every negotiated image vanish (dr.dk sent AVIF). Without the `avif`
cargo feature, `image/avif` is not listed.

AVIF images are decoded by rav1d (a Rust port of dav1d, the decoder Chrome
uses) after avif-parse reads the HEIF container (`huginn-net/src/avif.rs`).
Huginn converts YUV to RGB itself, with bilinear chroma upsampling as in
libavif; the output matches libavif within 1 per channel. rav1d is built
without its assembly: the crates.io release does not build its ARM
assembly, and its x86 assembly needs `nasm`. A 720x405 photo decodes in
about 6 ms (JPEG: about 1 ms), 1920x1080 in about 40 ms. Each decode uses
one thread (`n_threads = 1`); images still decode in parallel on the six
fetch threads. Not supported yet: grid (tiled) AVIF, `irot`/`imir`/`clap`
transforms, animation (the first frame is shown).

## Cookies and cross-origin requests

`huginn-net` owns the cookie jar (`cookies.rs`, RFC 6265bis), referrer
policy (`referrer.rs`) and CORS (`cors.rs`). `Loader::send` follows
redirects itself, one hop at a time, so every hop gets its own `Cookie`,
`Referer`, `Origin` and `Sec-Fetch-*` headers.

- Profiles: normal windows share a jar saved to `cookies.tsv` in the
  profile directory (0600 file, 0700 directory, written through a new temp
  file and a rename). All private windows share one in-memory jar, dropped
  when the last one closes. `huginn render` uses an in-memory jar.
- Third-party cookies are partitioned by top-level site, like Firefox's
  Total Cookie Protection. A request from a page to another site (an image,
  a script, `fetch()`) reads and writes only the cookies stored under the
  page's site (`site_key`: scheme and registrable domain). The third
  party's own first-party cookies are never sent there, and two top-level
  sites never share a third party's cookies, so a tracker cannot follow
  the user from site to site. First-party cookies (top-level navigations
  and same-site requests) are unchanged. `document.cookie` of a cross-site
  frame would see only its partition (`CookieJar::document_cookie` takes
  the top-level URL); Huginn does not run frames yet. Cookies under an
  opaque top-level origin (`file:`, `data:`) are never sent to third
  parties.
- SameSite: navigations a page starts (links, forms, `location`, in the
  same tab, a new tab or a new window) carry the page as initiator, so a
  cross-site page gets Lax cookies on `GET` only and never Strict ones.
  Only the browser UI (address bar, bookmarks, menus) navigates without an
  initiator (`Sec-Fetch-Site: none`, `Sec-Fetch-User: ?1`); if such a chain
  redirects to another site, the rest of it is cross-site.
- Local Network Access: requests from a page on a public address, other
  than top-level navigations, may not reach loopback or private addresses.
  The check runs in the resolver on the resolved addresses (a second ureq
  agent, made on first use), so DNS rebinding cannot get around it.
  `localhost` and `*.localhost` always resolve to loopback, never through
  DNS, because the jar treats them as secure.
- Eviction (180 cookies per site, 3300 in total) removes expired cookies,
  then non-secure ones, then the least recently used, so an insecure origin
  cannot flood out a Secure cookie and shadow it.

## External crates

Versions are the ones in `Cargo.lock`.

| crate | version | used in | why |
|---|---|---|---|
| html5ever | 0.40.1 | huginn-dom | Spec-compliant HTML parser from Servo. Fast, streaming, handles every kind of broken HTML. Writing our own parser would cost weeks and lose. |
| markup5ever / web_atoms | 0.40.0 / 0.3.0 | huginn-dom, huginn-style | Interned names and the `TreeSink` interface. Same atoms as Stylo 0.24. |
| cssparser | 0.38.0 | huginn-style | Servo's CSS tokenizer and parser toolkit. Same major version as Stylo 0.24. |
| taffy | 0.15.1 | huginn-layout | Block, flex, grid and float layout. Also used by Blitz and Bevy. Features: `block_layout`, `flexbox`, `grid`, `float_layout`, `calc`, `content_size`. |
| parley (fontique, harfrust, skrifa) | 0.12.0 | huginn-layout | Text shaping, bidi, line breaking, font fallback and system font discovery (CoreText on macOS, fontconfig on Linux). Fontique is vendored with a change to its fontconfig backend: `vendor/fontique/HUGINN.md`. |
| vello_cpu / vello_common | 0.4.0 | huginn-paint, huginn-net | CPU vector rasterizer from the Linebender project. Paths, rounded rects, clips, glyphs, images. No GPU needed for headless screenshots. |
| png | 0.18.1 | huginn-paint | PNG encoding, fast compression. |
| image | 0.25.10 | huginn-net | PNG, JPEG, GIF, WebP, BMP and ICO decoding, and the area-average downscale for big images shown small. |
| rav1d | 1.1.0 | huginn-net (feature `avif`, default on) | AV1 decoding for AVIF images. Memory-safe Rust port of dav1d, the decoder Chrome uses. Pure Rust, no system library. BSD-2-Clause. |
| avif-parse | 2.1.0 | huginn-net (feature `avif`) | Reads the AVIF (HEIF) container: the color and alpha AV1 items. MPL-2.0. |
| resvg (usvg, tiny-skia) | 0.50.0 | huginn-net | SVG images, CSS SVG backgrounds and inline `<svg>`. Features off: no text (it would load a font database on every page), no raster images inside SVG. tiny-skia's premultiplied RGBA moves into vello's pixmap without a conversion. |
| wuff | 0.2.10 | huginn-net | Pure-Rust WOFF and WOFF2 to sfnt (the WOFF2 glyf/loca transform included), as in Blitz. Every web font in the corpus is WOFF2. |
| encoding_rs | 0.8.44 | huginn-net | WHATWG Encoding Standard decoders for non-UTF-8 documents and stylesheets. UTF-8 input is borrowed, not copied. |
| ureq (rustls) | 3.5.1 | huginn-net | Small blocking HTTP/1.1 client with gzip and brotli. The corpus is HTTP/1.1 on 127.0.0.1; an async stack (tokio, hyper) would add threads and memory for nothing. Six scoped worker threads fetch and decode in parallel. |
| url / data-url | 2.5.8 / 0.3.2 | huginn-net, all | WHATWG URL parsing and `data:` URLs. |
| psl | 2.1 | huginn-net | The Public Suffix List compiled into lookup code: cookie `Domain` checks and site boundaries (`SameSite`, `Sec-Fetch-Site`). No list is parsed at run time, so it costs no heap. |
| parking_lot | 0.12 | huginn-net | Locks without poisoning for the cookie jar and the loader's client; already in the tree through Stylo. |
| serde_json | 1.0.154 | huginn-shell | The timing JSON line. |
| v8 (rusty_v8) | 152.2.0 | huginn-js (feature `v8`, default on in huginn-shell) | The JavaScript engine. The only engine that runs modern framework code (React on todomvc) fast enough. Its build downloads a prebuilt static library (~40 MB). ICU data: the 1.8 MB Flutter subset shipped with the crate (`crates/huginn-js/data/icudtl.dat`, Unicode License V3). |
| stylo (+ selectors, stylo_traits, stylo_dom) | 0.24.0 | huginn-style | Servo's CSS engine: custom properties, `calc()`, every shorthand, `@layer`, `:has()`, media queries level 4. See "The Stylo decision". MPL-2.0. |

Licenses of all dependencies: [THIRD-PARTY.md](THIRD-PARTY.md).

## Our code vs libraries

| part | ours | library |
|---|---|---|
| Fetching, cache, redirects, MIME | `huginn-net` (`Loader`) | ureq does HTTP; url does URLs |
| HTML tokenizing and tree building | tree sink (`huginn-dom/src/parse.rs`) | html5ever |
| DOM storage and mutation API | all of `huginn-dom` | atoms from markup5ever |
| CSS parsing, selectors, cascade | glue in `huginn-style` (`dom.rs`, `engine.rs`, `hints.rs`, `convert.rs`, `query.rs`), `ua.css` | Stylo 0.24 |
| Box generation, anonymous boxes, inline flattening, list markers, style conversion | `huginn-layout/src/build.rs`, `convert.rs` | |
| Layout tree that Taffy runs on (low-level API), dispatch, absolute/fixed placement | `huginn-layout/src/tree.rs`, `emit.rs` | Taffy's algorithms and its out-of-flow pass |
| Block, flex, grid, float layout | | Taffy |
| Inline formatting contexts: inline-block sizing, inline fragments, vertical-align, floats beside lines | `huginn-layout/src/inline.rs` | Parley shapes and breaks lines |
| Table layout (CSS 2.1 automatic) | `huginn-layout/src/table.rs` | |
| Font discovery (lazy, on-disk family index) | `huginn-layout/src/fonts.rs` | fontique scans the files |
| Display list and paint order (stacking, backgrounds, borders, clips) | `huginn-paint/src/lib.rs` | |
| Rasterization | glue in `huginn-paint/src/raster.rs` | vello_cpu |
| Image, SVG and font decoding | glue in `huginn-net`; AVIF YUV to RGB in `huginn-net/src/avif.rs` | image, rav1d, avif-parse, resvg, wuff |
| Which images and fonts a page needs (`srcset`, `<picture>`, lazy, `@font-face` matching, `unicode-range`) | `huginn-shell/src/resources.rs`, `huginn-net` media queries | |
| Charset sniffing | `huginn-net/src/charset.rs` (BOM, `Content-Type`, `<meta>` prescan, `@charset`) | encoding_rs |
| Script loading order, event loop, DOM bindings (JS prelude + native ops), modules | `huginn-js`, `huginn-shell/src/scripts.rs` | V8 runs the JavaScript |
| Pipeline, CLI, timing JSON | `huginn-shell` | |

## The Blitz decision

[Blitz](https://github.com/DioxusLabs/blitz) is a Rust HTML/CSS renderer from
DioxusLabs. It combines html5ever, Stylo, Taffy, Parley and Vello, the same
family of libraries we picked. We do **not** depend on Blitz crates. We use
Blitz as a reference.

Why:

1. **Scope.** Our plan is to own the DOM, resource loading, JS bindings,
   integration and the pipeline. `blitz-dom` is exactly that layer: it owns
   the document, the Stylo integration, layout and event handling as one unit.
   Taking it would mean adopting its DOM and its mutation API as our V8
   binding surface.
2. **Stability and version lock.** `blitz-dom` is `0.3.0-beta.4`. It pins its
   sibling crates with exact versions (`blitz-traits =0.3.0-beta.4`,
   `stylo_taffy =0.3.0-beta.4`) and depends on `stylo 0.22` and
   `selectors 0.41`, while the current releases are `stylo 0.24` and
   `selectors 0.43`. Depending on it would tie our upgrades to its release
   cadence.
3. **Control over memory and time.** Both are the win condition. We want to
   decide the data layout ourselves: arena DOM with 4-byte ids, flat layout
   tree, a display list that borrows instead of copying, a single process.

What we take from Blitz as a reference, file by file, when an owner needs it:
the Stylo `TElement`/`TNode` implementation for a custom DOM, the
`stylo_taffy` conversion of Stylo computed values to Taffy styles, inline
layout with Parley inline boxes inside Taffy leaves, WOFF2 decoding with
`wuff`, and SVG with `usvg`.

## The Stylo decision

Landed 2026-11-12: **`huginn-style` runs Stylo 0.24 behind its own
API.** `StyleEngine`, `StyleMap` and `ComputedStyle` did not change shape;
layout and paint still read only `ComputedStyle`. The phase-1 engine
(`cascade.rs`, `css.rs`, `selector.rs`) is deleted.

Why Stylo and not completing the phase-1 engine:

1. **The corpus needs most of modern CSS at once.** Counted in the
   snapshots: tailwind has 941 `oklch()`, 1,189 `color-mix()`, 123
   `@property`, 23 `@layer`, 106 `:has()` and 10,718 `var(--`; github has
   22,122 `var(--`, 1,972 logical properties, 792 range media queries, 106
   `:has()` and 21 `@container`. Writing all of that, plus every shorthand,
   `revert`, layer order and registered custom properties, is thousands of
   lines, and each gap is a broken page.
2. **Speed is built in.** Stylo has the rule hash, the ancestor bloom
   filter, the style sharing cache and the rule tree. The phase-1 engine
   matched every rule against every element.
3. **It fits our DOM.** Stylo's `TElement`/`TNode` traits do not need
   `Send`/`Sync`. We run the *sequential* traversal (`traverse_dom(.., None)`),
   so no rayon pool and no extra threads; `Document` keeps `StrTendril`.
4. **Memory.** Stylo's per-element data lives in a side table only during
   `compute`. Elements that share a Stylo style share one converted
   `Arc<ComputedStyle>`.

Measured at landing (fastdev profile, background QoS, same machine and
conditions, `style_ms` = `StyleEngine::compute`): github 23,092 ms (phase-1)
vs 145-290 ms (Stylo), tailwind 7,764-8,277 ms vs 97-226 ms, wikipedia
1,051-1,466 ms vs 76-111 ms. That is 50-100x faster.

Cost: a bigger dependency tree (stylo, selectors, servo_arc, icu_segmenter)
and a slow first build (Stylo generates its property code with Python at
build time; `python3` must be on `PATH`).

How it fits together (`crates/huginn-style/src`):

- `engine.rs`: Stylo prefs (`:has`, `text-overflow`, ...), the `Device`
  (screen, light scheme, fine pointer with hover), stylesheet parsing with
  synchronous `@import` through our `StylesheetLoader`, `@font-face`
  extraction (incl. `unicode-range`), the traversal.
- `dom.rs`: `HuginnNode` implements `TNode`, `TElement`, `TDocument` and
  `selectors::Element`. Stylo's style sharing cache stores elements in TLS
  as `usize`, so `HuginnNode` must be one pointer: it points at a per-node
  handle (context pointer + `NodeId`) in a boxed `DomCx`. The side table
  holds interned id and classes, the parsed `style` attribute and the
  presentational hints, per element.
- `hints.rs`: legacy HTML attributes as presentational hints (`bgcolor`,
  `width`/`height`, `align`/`valign`, `cellpadding`/`cellspacing`, table
  `border`, `<font>`, `nowrap`, list `type`, body margins). Identical hint
  and `style` blocks are interned, so Stylo can share their styles.
- `convert.rs`: Stylo `ComputedValues` to `ComputedStyle`. Linear `calc()`
  becomes `LengthPercentage::Calc { px, percent }` (probed at four bases,
  so non-negative clamping does not hide it); non-linear
  `min()`/`max()`/`clamp()` with percentages is flattened at the viewport
  width.
- `query.rs`: `query_selector(_all)`, `matches_selector` and `closest` on
  Stylo's selector parser and matcher. Each call builds a `DomCx` with one
  handle per node (16 bytes per node).
- `ua.css` (Chrome-like defaults) and `quirks.css` (added for quirks-mode
  documents only).

Not supported yet: `@container` (needs layout sizes during style),
`:hover`/`:focus` states (no input), animations and transitions (styles
are the first frame), counters in `content`, `mask-image` (paint draws the
masked box as a solid square: MDN icons).

## Build profiles

- `cargo build --release`: thin LTO, 16 codegen units. Use for daily work.
- `cargo build --profile bench-release`: fat LTO, 1 codegen unit, stripped.
  Use for benchmark runs.
- `cargo build --profile fastdev`: opt-level 1, no LTO, incremental. Quick
  fidelity checks only; never quote its times or memory.
- Parallel builds can use separate `CARGO_TARGET_DIR`s (`target-<name>`), so
  they do not block each other on the cargo lock.

## Phases

1. Headless `huginn render` with the phase-1 engines. Done: all fixtures and
   all 14 corpus pages render and exit 0.
2. Fidelity and speed on the corpus: the per-crate task lists below, ordered
   by impact.
3. V8 (`huginn-js`). The window (`huginn open`) exists already; script events
   (click handlers) will go through the engine thread.

## Corpus status (2026-11-12)

One smoke render per page against `serve.py`, compared by eye with Chrome's
`corpus/<page>/local.png`. All 14 pages exit 0 and write a PNG. Times were
taken at background QoS (efficiency cores), so they are slow in absolute
terms; the ratios between phases still hold.

| page | total ms | worst phase | what looks wrong |
|---|---|---|---|
| plain | 232 | layout 202 (font setup) | Close. List markers touch the text. |
| hackernews | 207 | layout 171 | Table layout wrong (cells spread and centered). No `bgcolor` (orange bar, beige body). |
| craigslist | 674 | layout 483 | Close. Map is grey (JS). |
| wikipedia | 1621 | style 814, layout 693 | Page grid broken: TOC above the article, not beside it. No SVG icons, infobox missing. |
| mdn | 314 | layout 218 | Mostly unstyled: nav stacked as buttons, big blank areas, "Skip to main content" visible. |
| github | 22127 | style 21518 | Mostly unstyled. No header, icons or layout. Style time is the problem. |
| docsrs | 1081 | layout 742 | Structure right. Web fonts (WOFF2) missing, sidebar background missing. |
| rustbook | 327 | layout 287 | Sidebar missing (built by JS). Fonts and colors (`var()`) missing. |
| kranblog | 241 | layout 199 | Post list not in rows, fonts and borders missing. |
| kranpost | 233 | layout 193 | Close. Web font missing, TOC sidebar missing, content column too wide. |
| juniper | 317 | layout 273 | Serif instead of web font, container not centered, nav and logo wrong. |
| bbc | 827 | layout 697 | Header logo and layout wrong, headline width wrong, web fonts missing. |
| tailwind | 6004 | style 5552 | Almost blank. Tailwind v4 CSS (`var()`, `@layer`, `oklch()`) not understood. |
| todomvc | 206 | layout 196 | App missing: it is rendered by React (needs JS). |

## Task lists per crate, by impact on the corpus

**huginn-style**
1. `style_ms` on github is 145-290 ms at background QoS; profile `flush`
   (rule hash build) vs traversal vs `convert`.
2. Reuse one `DomCx` across `query_selector` calls while the DOM does not
   change (huginn-js calls them often).
3. `StyleMap::pseudo` scans every pseudo style; index it by node.
4. `@container` queries (needs layout sizes), `mask-image`.

**huginn-layout**
1. Font setup: `LayoutEngine::new()` builds the full system font collection.
   That costs about 40-50 ms per page on performance cores, even for a page
   with no text. It is most of the time on small pages. Load fonts lazily or
   from a cache.
2. Real table layout (CSS 2.1 automatic layout) instead of flex rows of equal
   cells (hackernews, wikipedia, craigslist).
3. Grid templates and areas once huginn-style provides them (wikipedia, mdn,
   github, tailwind).
4. Inline fragments: backgrounds, borders and padding of inline elements.
5. `position: fixed` against the viewport, `sticky`, visually-hidden patterns.
6. Replaced elements: SVG images, inline `<svg>` sizing, form controls, iframes.
7. List marker gap and `::marker`.
8. Node-to-box index for JS geometry queries.

**huginn-paint**
Done: CSS 2.1 Appendix E paint order with stacking contexts, negative
z-index and the float layer; containing-block clip chains (non-isolated
clips, no layers); opacity groups; 2D transforms; `clip-path` basic shapes
(inset, circle, ellipse, polygon against the border box; a stacking
context; github's skeleton bars 0.972 -> 0.987); background-size,
background-position, repeat and gradient tiles; linear and radial
gradients (also repeating); box-shadow (outer and inset, blurred);
outline; object-fit; border-radius on backgrounds, borders and replaced
content; dashed, dotted and double borders; mipmapped image downscaling and
bicubic upscaling; synthetic bold; text baselines on whole device pixels
like Skia (x stays subpixel; hackernews 0.974 -> 0.990); PNG with
`Compression::Fastest` (encode on plain: 65 ms -> 38 ms at background QoS,
single run).
Backgrounds, gradients, shadows, transforms, outline and object-fit take
effect as huginn-style fills those `ComputedStyle` fields (defaults today).
Pixel tests: `crates/huginn-paint/tests/paint_order.rs`.
1. Paint inline fragments once huginn-layout produces them (one `Rect` +
   `Border` per fragment, sliced by `first`/`last`).
2. Text: Chrome on macOS draws text with more ink than our vello glyphs
   (hackernews: 12% more on 13 px black, 37% more on 9 px grey). Chrome
   dilates glyphs only when `-webkit-font-smoothing` is `auto` (CoreText
   smoothing); 9 of 14 corpus pages set `antialiased` somewhere. Measured
   (2026-11-12, with baseline snapping, one global outset of 0.1 px per
   side): hackernews 0.990 -> 0.993, plain and craigslist +0.001; docsrs and
   juniper -0.003, wikipedia -0.002, mdn -0.0015, tailwind -0.001. So it
   needs the property first: Stylo has it only for Gecko
   (`-moz-osx-font-smoothing`, alias `-webkit-font-smoothing`), and the
   servo build drops it.
3. SVG content (github, mdn, wikipedia, juniper, bbc).
4. `text-shadow`, multiple background layers.

**huginn-net**
Done: WOFF/WOFF2 web fonts (only faces and `unicode-range` subsets the text
uses), SVG (`<img>`, CSS backgrounds, inline `<svg>`), no bodies kept after
decoding, images decoded at display size, `srcset`/`sizes`/`<picture>`,
`loading=lazy` only in view, charset sniffing. Open:
1. Web fonts on kranpost, kranblog, juniper and mdn wait for `var()` in
   huginn-style: their `font-family` is a custom property.
2. SVG `fill`/`stroke` from CSS (tailwind and rustbook color icons with CSS
   classes); huginn-style adds the fields, then pass them to the rasterizer.
3. `mask-image: url(*.svg)` icons (mdn, wikipedia): needs huginn-style and huginn-paint.
4. The metric overrides in `@font-face` (`ascent-override`, `descent-override`, `line-gap-override`; tailwind), and `size-adjust` for CFF fonts (done for TrueType outlines: `size_adjust_font` rescales `head.unitsPerEm`).
5. Lazy loading checks the painted region only; Chrome also preloads about
   1250 px below it. Matters only for full-page screenshots.

**huginn-dom**
1. Thread-safe strings, if Stylo needs them.
2. A node free list for script-heavy pages.
Done: `parse_fragment`, `serialize`, `clone_node` (innerHTML, outerHTML,
cloneNode); an opt-in id index for `element_by_id` (`index_ids`, switched on
by the V8 engine). Charset sniffing is in huginn-net (`Resource::html_text`).

**huginn-js**
Done: V8, on by default (`--lite-mode`); the DOM layer in JavaScript (`src/js/`):
nodes, elements, attributes, `classList`, `dataset`, inline `style`, events
(capture and bubble, `on*` handlers), timers, rAF, `MessageChannel`,
`fetch`/XHR (Fetch modes and CORS), `document.cookie` (the cookie jar),
`URL`, `TextEncoder`/`TextDecoder` (every WHATWG
encoding, streaming; native, through encoding_rs), WHATWG streams (readable
with byte streams and BYOB readers, writable, transform, pipes, tee, text and
compression streams; `65-streams.js`, built on first use), `Response.body`
as a stream, `localStorage` (in memory),
`matchMedia`, `location`/`history`, custom elements (upgrade and callbacks,
shadow trees included: a node in a shadow tree is connected when its host
is; `importNode`/`cloneNode` upgrade copies), shadow roots (rendered through
the flat tree; `::part`, `::slotted`, `:host`),
`MutationObserver` (records for script mutations), `IntersectionObserver`
(reports real geometry once), `getComputedStyle`, `getBoundingClientRect`;
classic, defer, async and module scripts (static imports, `import()`,
`import.meta`, import maps with scopes and merging: `src/importmap.rs`),
script-inserted scripts, `document.write`. Timers are
ordered from the start time of the engine call, so the order does not depend
on CPU speed.
1. A V8 startup snapshot of the prelude (isolate plus prelude cost about
   5.5 ms and 15 MB RSS per script page).
2. Image `load` events and `naturalWidth` (needs decoded sizes from huginn-net).
3. `about:blank` iframes with their own realm (`contentWindow` is null today;
   Google's search page script reads builtins from one and fails).
