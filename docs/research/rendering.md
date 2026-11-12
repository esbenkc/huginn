# Research: a fast, small rendering pipeline

Date: 2026-11-12. Part of [docs/research](README.md).

This covers how production engines keep style, layout, paint, text and images
fast and small, and what the smallest useful version of each technique is for
Huginn.

**[V]** marks a claim checked against a source fetched on the date above.
**[U]** marks a claim that was not checked. Sources are numbered at the end.

## Where Huginn is today

- Style and layout run in full after every change. There is no invalidation.
- There are no CSS animations or transitions. Styles are the first frame.
- The window already rasterizes the page in strips of 256 CSS px and keeps
  them (LRU). Scrolling copies strip rows into the frame. A change to the page
  throws the strips away.
- Each frame is presented whole. softbuffer's damage API is not used.
- Raster is vello_cpu on one thread.
- On Linux each start scans the system fonts, about 60 ms. macOS has a lazy
  font index.
- Images are decoded at display size, once, and lazy images only in view.

## 1. Incremental style

**Technique.** Stylo keeps `ElementData` per element: styles, a `RestyleHint`
and `RestyleDamage` [V][S3].

- **Snapshots.** Before an attribute or state changes, the embedder records the
  old value in a snapshot. Stylo matches the dependent selectors against the
  old state and the new one. If the results differ, it derives a hint from the
  selector's combinator [V][S4].
- **Hints.** They separate rematching (`RESTYLE_SELF`, `RESTYLE_DESCENDANTS`)
  from the cheaper recascade (`RECASCADE_SELF`) and from
  `RESTYLE_STYLE_ATTRIBUTE`, which skips selector matching. Animation hints
  are processed in a separate traversal before the normal one [V][S2].
- **Rule tree.** An element whose matched rules cannot have changed skips
  selector matching and sorting [V][S1].
- **Sharing cache.** An LRU of 32 entries. It needs the same parent style, the
  same inline style and presentational hints, and the same results for the
  revalidation selectors [V][S5].

**What the embedder implements** [V][S6].

- Dirty bits: `has_dirty_descendants`, `set_dirty_descendants`, `unset_dirty_descendants`.
- Snapshots: `has_snapshot`, `handled_snapshot`, `set_handled_snapshot`.
- Element data: `ensure_data`, `borrow_data`, `mutate_data`, `clear_data`.
- Animations: `may_have_animations`, `has_animations`, `has_css_animations`,
  `has_css_transitions`, `animation_rule`, `transition_rule`.

**The reference driver is Blitz** [V][S7]. It does all of this in about 1,900
lines across four files.

1. `stylist.flush(...)` with the snapshot map.
2. Tick the `DocumentAnimationSet` (Pending to Running to Finished).
3. Build a `SharedStyleContext` with `snapshot_map`, `animations` and
   `current_time_for_animations`.
4. `RecalcStyle::pre_traverse`, then `style::driver::traverse_dom`.
5. Clear snapshots and `is_new` flags. Recompute `has_active_animations` to
   decide whether to keep ticking frames.

`pre_traverse` exists because invalidation runs from parent to child and cannot
handle the root [V][S8].

**Blink.** Selectors are compiled into invalidation sets keyed by id, class,
tag and attribute. Changes are queued and applied by a depth-first
`StyleInvalidator`. The dirty bits are `NeedsStyleRecalc` and
`ChildNeedsStyleRecalc` [V][S9].

**Smallest version.** A snapshot map, two bits per node, the sequential driver,
and reading `damage` after the traversal.

**Traps.**

- A forgotten snapshot before a mutation under-invalidates, with no error.
- Animation sets of removed nodes must be dropped, or an infinite animation
  keeps the frame loop alive forever [V][S7].
- Stale dirty-descendant bits.

Today Huginn's Stylo data "lives in a side table only during `compute`"
(ARCHITECTURE.md). Incremental restyle needs that data to stay alive between
passes, which costs memory. Measure it.

## 2. Incremental layout

**Technique.** LayoutNG makes layout a function from (node, constraint space)
to an immutable fragment.

- The constraint space is stored with the fragment as the cache key.
- Reuse needs an identical space and no pending layout on the node [V][S11].
- Fragments have no parent pointers, so usually "only the spine of the tree is
  rebuilt" [V][S12].

**Why it matters.** Flex and grid measure and then lay out. Without a cache
this is O(2^n) in nesting depth. Chrome's rule of thumb: if a change to one
property costs 50-100 ms, suspect an exponential bug, not slow code [V][S13].

**Taffy.** Each node has a cache of 9 slots, keyed on known dimensions and
available space. `mark_dirty` marks the node and its ancestors [V][S14][S15].

**Blitz's bridge from style to layout** [V][S16].

- `compute_layout_damage(old, new)` sorts each change into repaint, relayout or
  box construction.
- `propagate_damage_flags` unions damage up the tree, and clears the layout
  cache only on nodes whose subtree needs relayout.
- A `CONSTRUCT_BOX` level rebuilds the box subtree.

**Servo** has the same shape. A background change skips layout. A transform
change rebuilds only the stacking context tree [V][S17].

**Containment.** `content-visibility: auto` skips style, layout and paint of
subtrees that are off screen. The measured case went from 232 ms to 30 ms [V][S19].

**Smallest version.**

1. Sort damage into repaint, relayout or reconstruct.
2. Clear Taffy caches only along the dirty spine.
3. Give Huginn's own inline, float and table code a cache per box, keyed on
   the containing block size.

**Traps.**

- Anonymous boxes and pseudo-elements need their own damage handling. Blitz
  has code and tests just for this.
- A cache key that leaves out an input brings back under-invalidation [V][S13].

## 3. Paint and compositing on the CPU

**Reusing the display list.**

- Blink copies unchanged display items from the previous list, and whole
  unchanged stacking contexts in one step. Items that share a (transform, clip,
  effect, scroll) state form a paint chunk [V][S12].
- Firefox found that building the display list took more than 40% of paint
  time. Retaining it cut paints of 16 ms or more by almost 30%, at the cost of
  a complex merge step [V][S20].

**Tiles and scrolling.**

- Chrome's software raster uses tiles of about 256x256 px on worker threads.
  A scroll moves existing tiles and rasters only the newly exposed ones [V][S21].
- Property trees make a scroll one change to a 2D transform, not a repaint [V][S12].

**Damage.** Chrome's compositor tracks paint invalidation, raster invalidation
and screen damage separately. Screen damage allows a partial swap, which "saves
power" [V][S22].

**Layers.** Firefox before WebRender kept layers so scrolling only recomposited.
Layers "take up a lot of memory" and help with only "a small number of CSS
properties" [V][S23].

**Partial present.** softbuffer has `present_with_damage` (Wayland, X11 with
XShm, Win32) and `age()`. Age 0 means the buffer contents are unknown, so
redraw everything [V][S24].

**vello_cpu.**

- Sparse strips skip empty areas, and solid interiors cost only a setup per
  strip. Path memory is O(n) [V][S25].
- It has a `multithreading` feature.
- Limits: complex filter graphs panic, there are no filters at all in
  multithreaded mode, and glyph caching is experimental [V][S26].

Geometric means computed from the published benchmark data [S28]. These are
this survey's calculations from the data, not published figures, and the
benchmark draws shapes, not web pages.

| comparison | i9-14900K | M1 Pro |
|---|---|---|
| vello_cpu, 1 thread, against tiny-skia | 3.3x faster | 4.6x faster |
| against Cairo | 2.8x faster | 4.3x faster |
| against Skia | not in the data | 2.5x faster |
| against Blend2D, 1 thread | 0.43x | 0.61x |
| vello_cpu, 8 threads against 1 | 4.3-6.1x | 2.7-3.3x |

**GPU.** vello_hybrid keeps geometry on the CPU and paints on the GPU. No
reliable figure was found for the base memory of a wgpu device [U]. Measure it
before adopting it.

**Smallest version.**

1. A damage rectangle, raster clipped to it, and `present_with_damage`.
2. When the page changes, keep the strips that the damage does not touch.
3. Add a second layer later, only for fixed or sticky content and for animated
   transform or opacity.

**Traps.** Fixed and sticky content break a naive scroll by copying. Respect
the buffer age.

## 4. Text

**Technique.**

- Blink matches a typeface lazily, only when layout or paint needs geometry.
- It shapes with the primary font, collects the clusters that font does not
  cover, and tries fallback fonts for those.
- It caches shaping per word, keyed on the word, the font description and the
  fallback list, because shaping and fallback are "costly operations" [V][S29].
- Firefox moved its font list into one shared structure, which saved about
  2 MB per content process on Linux [V][S30].

**Why Huginn's text looks lighter than Chrome's.** Skia blends text in the
destination's non-linear space, and applies a "gamma hack" and a contrast hack
to coverage. Linear blending makes black on white look "anemic" at small sizes
[V][S32]. ARCHITECTURE.md measured the same gap (12% to 37% less ink). A
contrast curve on glyph coverage is the general fix.

**Smallest version.**

- A shape cache per word or run.
- A fallback cache per (script, family).
- Grayscale antialiasing with a contrast curve on coverage.
- The lazy font index on Linux, as on macOS.

## 5. Images

**Technique.** Chrome gives each decode its own task, off the main thread, and
keeps decoded images in a cache that can be discarded [V][S22]. Decoding at
the nearest larger size and scaling down is reported from source search only [U].

**Smallest version.** Keep the encoded bytes, decode on first paint on a worker
thread at display size, hold decoded images in an LRU with a byte budget, and
drop the ones that are off screen.

Huginn already decodes at display size and drops the encoded bytes. Dropping
the bytes saves memory but means a discarded image must be fetched again, so
the byte-budget LRU needs the disk cache first.

## 6. Scheduling

**Technique.** The specification's order for "update the rendering" is: resize,
scroll, media queries, animations, `requestAnimationFrame` callbacks, style and
layout, IntersectionObserver, paint. A document with no visible change and no
callbacks is skipped [V][S35].

**Budgets** [V][S36][S37].

- Respond to input within 100 ms.
- About 10 ms of work per 16 ms frame.
- Idle work in pieces of 50 ms or less.

**First paint.** A preload scanner fetches images, scripts and stylesheets
while the parser is blocked [V][S38].

**Smallest version.** One frame callback per vsync, run only when something is
dirty. Input coalesced to one event per frame. A first paint as soon as the
blocking stylesheets have loaded.

**Trap.** With one engine thread per tab, a long script or layout blocks that
tab. Huginn scrolls on the UI thread from kept strips, which already avoids the
worst of this. Keep it that way.

## 7. Memory

- **Where it goes.** In a Firefox example, layout was 54.5% of a window's
  objects, style sheets 24.4% and the DOM 20.9% [V][S40].
- **How to measure.** Use private memory from `/proc/<pid>/smaps`, not RSS
  alone [V][S41].
- **Discarding.** Firefox unloads tabs in least-recently-used order when memory
  is low [V][S42]. Huginn already does this.

**Smallest version.** A memory reporter per subsystem. Byte budgets for the
image, shaping and glyph caches, with one `purge()` hook. Drop layout and paint
data of background tabs.

## 8. Measurement

- **LCP** is good at 2.5 s or less, at the 75th percentile [V][S43].
- **Speedometer 3** times the synchronous work and the frame after it, so that
  layout and paint count [V][S10b].
- **MotionMark** raises complexity until 60 fps breaks [V][S44].

**Honest benchmarking for a small engine.**

- Report time per phase (style, layout, display list, raster, present).
- Report the median and 95th percentile frame time during a scripted scroll.
- Report private memory per page on a fixed corpus.
- Keep cold and warm runs apart.
- Do not compare against Chrome on pages where Huginn skips features.

## The ten changes with the best return

1. **Tracing spans and a memory reporter per subsystem.** Do this first. It
   checks every other item.
2. **Incremental Stylo restyle.** Snapshots, dirty bits and `pre_traverse`,
   following Blitz's driver.
3. **Damage classes.** Repaint, relayout or reconstruct, so a paint-only change
   skips layout.
4. **Keep Taffy's caches.** Clear only the dirty spine. Add caches keyed on
   constraints to the inline, float and table code.
5. **Damage rectangles.** Raster only what changed, keep untouched strips, and
   present with damage.
6. **CSS transitions and animations** through Stylo's `DocumentAnimationSet`.
   This is cheap once item 2 exists. Schedule frames only while an animation is
   active.
7. **Shape and fallback caches**, with a byte limit, and the lazy font index on Linux.
8. **A contrast curve on glyph coverage**, to match Chrome's text weight.
9. **vello_cpu `multithreading` with 2-4 threads.** Measure first: it adds
   threads and memory, and filters do not work in that mode.
10. **Image cache with a byte budget and decoding off the engine thread.**

Later: a preload scanner (worth it once loads wait on the network), a retained
display list (complex; Firefox gained about 30% on slow paints), layers, and a
GPU path once its memory cost is measured.

## Sources

- S1 <https://hacks.mozilla.org/2017/08/inside-a-super-fast-css-engine-quantum-css-aka-stylo/>
- S2 <https://raw.githubusercontent.com/servo/stylo/main/style/invalidation/element/restyle_hints.rs>
- S3 <https://raw.githubusercontent.com/servo/stylo/main/style/data.rs>
- S4 <https://raw.githubusercontent.com/servo/stylo/main/style/invalidation/element/element_wrapper.rs>
- S5 <https://raw.githubusercontent.com/servo/stylo/main/style/sharing/mod.rs>
- S6 <https://raw.githubusercontent.com/servo/stylo/main/style/dom.rs>
- S7 <https://raw.githubusercontent.com/DioxusLabs/blitz/main/packages/blitz-dom/src/stylo/traversal.rs>
- S8 <https://raw.githubusercontent.com/servo/stylo/main/style/traversal.rs>
- S9 <https://chromium.googlesource.com/chromium/src/+/HEAD/third_party/blink/renderer/core/css/style-invalidation.md>
- S10b <https://webkit.org/blog/15131/speedometer-3-0-the-best-way-yet-to-measure-browser-performance/>
- S11 <https://chromium.googlesource.com/chromium/src/+/HEAD/third_party/blink/renderer/core/layout/layout_ng.md>
- S12 <https://developer.chrome.com/docs/chromium/renderingng-data-structures>
- S13 <https://developer.chrome.com/docs/chromium/layoutng>
- S14 <https://docs.rs/taffy/latest/taffy/tree/struct.TaffyTree.html>
- S15 <https://raw.githubusercontent.com/DioxusLabs/taffy/main/src/tree/cache.rs>
- S16 <https://raw.githubusercontent.com/DioxusLabs/blitz/main/packages/blitz-dom/src/layout/damage.rs>
- S17 <https://servo.org/files/2025-09-13-a-dive-into-the-servo-layout-system.pdf>
- S19 <https://web.dev/articles/content-visibility>
- S20 <https://mozillagfx.wordpress.com/2018/01/09/retained-display-list/>
- S21, S22 <https://chromium.googlesource.com/chromium/src/+/HEAD/docs/how_cc_works.md>
- S23 <https://hacks.mozilla.org/2017/10/the-whole-web-at-maximum-fps-how-webrender-gets-rid-of-jank/>
- S24 <https://docs.rs/softbuffer/latest/softbuffer/struct.Buffer.html>
- S25 <https://docs.google.com/presentation/d/1f_vKBJMaD68ifBO2j83lBly9Zdk-2bsvj_DIHXxvcuk/htmlpresent>
- S26 <https://docs.rs/crate/vello_cpu/latest>
- S28 <https://laurenzv.github.io/vello_chart/>
- S29 <https://chromium.googlesource.com/chromium/src/+/HEAD/third_party/blink/renderer/platform/fonts/README.md>
- S30 <https://bugzilla.mozilla.org/show_bug.cgi?id=1514869>
- S32 <https://skia.org/docs/dev/design/raster_tragedy/>
- S35 <https://html.spec.whatwg.org/multipage/webappapis.html>
- S36 <https://web.dev/articles/rail>
- S37 <https://w3c.github.io/requestidlecallback/>
- S38 <https://web.dev/articles/preload-scanner>
- S40 <https://firefox-source-docs.mozilla.org/performance/memory/about_colon_memory.html>
- S41 <https://chromium.googlesource.com/chromium/src/+/HEAD/docs/memory/key_concepts.md>
- S42 <https://hacks.mozilla.org/2021/10/tab-unloading-in-firefox-93/>
- S43 <https://web.dev/articles/lcp>
- S44 <https://browserbench.org/MotionMark1.3/about.html>

**Not obtained.** The "Life of a Pixel" slides, the WebRender picture caching
write-up, the MemShrink posts, the sparse strips thesis, and any primary
document on WebKit's style invalidation or layout.
