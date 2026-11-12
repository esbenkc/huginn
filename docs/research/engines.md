# Research: how other small engines are built

Date: 2026-11-12. Part of [docs/research](README.md).

This is a survey of independent browser engines, read for ideas that Huginn can
use. Blitz comes first because it uses almost the same libraries as Huginn.

Claims come from the linked pages, read on the date above. **[unverified]**
marks a claim that was not checked against a fetched source.

## Blitz (same stack as Huginn)

**Structure.** `blitz-dom` holds DOM, style, layout and events. `blitz-html`
(html5ever), `blitz-paint`, `blitz-net` and `blitz-shell` are separate, with
`blitz-traits` as the shared base so the crates do not depend on each other.
<https://github.com/DioxusLabs/blitz>

**Pipeline.** One `resolve()` runs these phases in order
([resolve.rs](https://github.com/DioxusLabs/blitz/blob/main/packages/blitz-dom/src/resolve.rs)):

1. Take in loaded resources. Return early while render-blocking resources are pending.
2. Stylo traversal with the animation clock.
3. Propagate damage flags.
4. Rebuild box children only where construction damage is set.
5. Build deferred inline (Parley) layouts, optionally with rayon.
6. Run Taffy `compute_root_layout`.
7. Transforms and scrollable overflow, then the paint tree and stacking contexts for damaged subtrees.
8. Clear damage.

**Damage model.** Blitz adds its own bits above Stylo's `RestyleDamage`:
`ONLY_RELAYOUT`, `REORDER_CHILDREN`, `CONSTRUCT_BOX`, `CONSTRUCT_FC`,
`CONSTRUCT_DESCENDENT`
([damage.rs](https://github.com/DioxusLabs/blitz/blob/main/packages/blitz-dom/src/layout/damage.rs)).

- `compute_layout_damage(old, new)` returns `ALL_DAMAGE` for changes that affect
  the box tree (display, float, position, font, pseudo `content`), text shaping
  changes and table internals. A changed `order` returns relayout plus reorder.
  Anything else returns relayout.
- Damage is unioned upward in a post-order pass that skips subtrees with no
  damaged descendants.
- A node with relayout or construct damage has its Taffy cache cleared, and so
  do its anonymous layout parents.

**Taffy caching.** Every child layout goes through `taffy::compute_cached_layout`
with a per-node cache (a `CacheTree` impl). Out-of-flow layout runs inside the
cache wrapper, so a cache hit does not run the out-of-flow pass again
([layout/mod.rs](https://github.com/DioxusLabs/blitz/blob/main/packages/blitz-dom/src/layout/mod.rs)).

**Stylo invalidation**
([document.rs](https://github.com/DioxusLabs/blitz/blob/main/packages/blitz-dom/src/document.rs),
[stylo/traversal.rs](https://github.com/DioxusLabs/blitz/blob/main/packages/blitz-dom/src/stylo/traversal.rs)).

- Before an attribute or element state changes, Blitz stores a
  `ServoElementSnapshot` in a `SnapshotMap`.
- `stylist.flush(..).process_style(root, Some(&snapshots))` then lets Stylo's
  invalidator find the affected siblings and descendants.
- Snapshots are cleared after the traversal. Nodes that were never styled are
  not snapshotted.
- `RecalcStyle::process_preorder` calls `recalc_style_at`. When `data.damage` is
  not empty, it marks the ancestor chain for the damage pass.

**Animations and transitions** (traversal.rs).

- Blitz keeps Stylo's animation sets on the document and passes `animations`
  and `current_time_for_animations` in `SharedStyleContext`.
- Each frame it sets `RestyleHint::RESTYLE_SELF` on every node with a set, and
  moves Pending to Running to Finished itself.
- After the traversal it clears cancelled animations, resets `is_new` and drops
  empty sets.
- `has_active_animations` (from `needs_animation_ticks()`) decides whether to
  keep redrawing.

**Problems Blitz hit, and its fixes.**

- Transitions started from unstyled values when a stylesheet arrived after the
  first resolve. The fix: skip style and layout while render-blocking resources
  are pending. <https://github.com/DioxusLabs/blitz/issues/689>
- Animation sets for removed nodes must be purged. Otherwise an infinite
  animation forces a redraw every frame, forever.
- Stylo's parallel traversal uses thread-local caches, and two concurrent
  traversals panic. Blitz serialises them with a global mutex and defaults to
  sequential style
  ([config.rs](https://github.com/DioxusLabs/blitz/blob/main/packages/blitz-dom/src/config.rs),
  <https://github.com/DioxusLabs/blitz/issues/430>).
- Attributes that layout reads directly (`width`, `colspan`, `src`, `value`, all
  SVG attributes) bypass the computed-style diff. They need an explicit
  `read_by_layout` list, "or invalidation silently breaks"
  ([AGENTS.md](https://github.com/DioxusLabs/blitz/blob/main/AGENTS.md),
  [mutator.rs](https://github.com/DioxusLabs/blitz/blob/main/packages/blitz-dom/src/mutator.rs)).
- Pseudo-element styles must be synced during the style traversal, because
  incremental mode no longer rebuilds their boxes.
- `display:none` subtrees are never styled, so `getComputedStyle` needs an
  on-demand `resolve_style`.
- Tests assert that an incrementally updated document matches a fresh one
  (`assert_matches_fresh`). An `incremental_layout` switch marks everything
  damaged.
- Open optimisations: pointer-compare style diffing, limit damage to the
  formatting context, honour repaint-only damage, stop storing `taffy::Style`,
  fix anonymous-node leaks. <https://github.com/DioxusLabs/blitz/issues/255>
- WPT is "the main correctness signal". It runs per pull request and posts a
  diff (AGENTS.md).

No Blitz talk or design post was found. Everything above is from code and issues.

## Servo

**Architecture.** One constellation manages pipelines. The script thread owns
the DOM and the JS engine. Layout starts on the script thread and may use worker
threads. The renderer forwards display lists to WebRender on the embedder's UI
thread and can handle scroll there directly. In single-process mode "all script
threads run in the embedder process".
<https://book.servo.org/design-documentation/architecture.html>

**Pipeline.** Style and damage, box tree, fragment tree, overflow, stacking
context tree, display list. A box can produce several fragments. The design
goals are "rebuild only the damaged part of the tree" and "skip unnecessary
layout phases".
<https://servo.org/files/2025-09-13-a-dive-into-the-servo-layout-system.pdf>

**Graded damage.** `LayoutDamage` has Repaint, RebuildStackingContextTree,
RecalculateOverflow, Relayout, RecomputeInlineContentSizes,
DescendantHasBoxDamage (rebuild this box and its ancestors, not its children)
and BoxDamage. <https://doc.servo.org/src/layout_api/layout_damage.rs.html>

**Per-box caches.** `LayoutBoxBase` caches inline content sizes, keyed on the
containing block's block size. It also caches a layout result with the inputs
it was computed from; a hit needs matching inputs. A dirty flag keeps the stale
entry for a faster relayout.
<https://doc.servo.org/src/layout/layout_box_base.rs.html>

**Incremental wins, in the order they landed.**

- Skip display list construction when nothing changed. This shows most on mouse moves.
- Script queries (`getBoundingClientRect`, `offsetParent`) skip phases that are
  already up to date.
- Transform changes avoid full layout.
  <https://servo.org/blog/2025/07/17/this-month-in-servo/>
- Incremental box construction for flex, grid, table and inline items, and
  cached layout results for replaced boxes.
  <https://servo.org/blog/2025/08/22/this-month-in-servo/>
- "Dirty roots" were replaced by "truly incremental box tree layout". Cached
  fragments are reused in independent formatting contexts.
  <https://servo.org/blog/2026/03/31/february-in-servo/>

**Memory.**

- `about:memory` has a reporter per subsystem.
- Single-process mode uses refcounted byte vectors instead of OS shared memory.
- A canvas leak reached about 5 GB because the GC could not see native memory
  held by small JS wrappers. The fix was to report the native size to the GC.
  <https://taym95.github.io/blog/2026/06/06/fix-servo-canvas-memory-leak/>

## Ladybird

**Processes.** UI, Hub (cookies, storage, fonts), Compositor, per-site
WebContent, RequestServer and ImageDecoder. Helper processes sandbox themselves
with seccomp and Landlock, or Seatbelt on macOS.
[ProcessArchitecture.md](https://github.com/LadybirdBrowser/ladybird/blob/master/Documentation/ProcessArchitecture.md)

**Pipeline.** DOM, style (rules bucketed by the rightmost selector), layout
tree, `LayoutState` with used values, paintable tree, stacking contexts. A
`LayoutState` is committed or thrown away, which allows measure-only layouts.
[LibWebFromLoadingToPainting.md](https://github.com/LadybirdBrowser/ladybird/blob/master/Documentation/LibWebFromLoadingToPainting.md)

**How they choose work** (Andreas Kling).

- Correctness and compatibility come first. Optimise only what makes
  development unpleasant.
- Work in vertical slices around one real site: "as we focus on improving one
  website, thousands of other sites tend to improve as well".
- Mirror the architecture of the specifications, so contributors learn one design.
  <https://awesomekling.substack.com/p/how-were-building-a-browser-when>

**Published performance work.** Each item came from profiling a named site.

- Skipping display list commands under zero-area clips, plus a mask compositing
  rework, took Discord from about 65 to 120 FPS.
- `clientLeft` and `clientTop` no longer force layout.
- Custom properties use structural sharing.
- Animated image frames decode on demand, which saved over 1 GiB on cloudflare.com.
- The JS AST is freed after bytecode generation.
- `document.cookie` is cached behind a version counter.
  <https://ladybird.org/newsletter/2026-02-28/>
- Paint commands are cached per paintable.
- Sibling invalidation sets follow selector combinators.
- A font load invalidates only the elements that use that font.
- Parsed `querySelector` selectors are cached per document.
  <https://ladybird.org/newsletter/2026-03-31>

**Rust.** Swift was dropped over C++ interop and platform support. The LibJS
front end was ported to Rust (about 25k lines) with a requirement of
byte-identical output, checked by running both pipelines side by side.
<https://ladybird.org/posts/adopting-rust/>

## Others, briefly

- **Lightpanda.** No graphical renderer. html5ever, V8 and libcurl. One arena
  per page load, freed at once. It reports 123 MB against 2 GB for headless
  Chrome over 100 pages. <https://github.com/lightpanda-io/browser>
- **NetSurf (LibCSS).** Computed styles are interned, so only unique ones stay
  in memory. <https://www.netsurf-browser.org/projects/libcss/>
- **Flow.** A minute-long Gmail load came from one `innerHTML` string built a
  byte at a time. The blockers were small missing APIs such as `postMessage` and
  `relatedTarget`.
  <https://www.ekioh.com/devblog/full-google-mail-in-a-clean-room-browser/>
- **Ultralight.** A stripped, in-process WebKit. It claims 9-17 MB against
  72-136 MB for Electron, with no method given. <https://ultralig.ht/>
- **litehtml.** Layout only. Drawing, fonts and images go through one small
  callback interface. <https://github.com/litehtml/litehtml>
- **Gosub.** Pluggable backends (null, Cairo, Skia, Vello), Taffy layout,
  opt-in sandboxed child processes. <https://github.com/gosub-io/gosub-engine>
- **WPE/Cog.** No UI toolkit. Frames are handed off as DMABuf or SHM buffers.
  <https://wpewebkit.org/about/architecture.html>
- **Kosmonaut.** Hobby scale. Uses layout tree dumps and pixel reftests.
  <https://github.com/twilco/kosmonaut>
- **Dillo, Sciter.** Nothing to borrow was found.
- **Links, w3m.** Not researched.
- **Web Browser Engineering** (the book).
  - Make layout idempotent before you cache it.
  - Add descendant-dirty bits, and stop marking ancestors at the first one that is already dirty.
  - Notify dependents only when a value really changed.
  - Under-invalidation shows up as glitches that depend on the order of events, so assert in debug builds.
    <https://browser.engineering/invalidation.html>
  - Opacity and transform animations should skip layout and raster.
    <https://browser.engineering/animations.html>

## The ten most useful ideas for Huginn

1. **Use Stylo's own invalidation.** Snapshot attributes and state before a
   mutation, pass the `SnapshotMap` to `process_style`, and rely on restyle
   hints and dirty-descendant bits. Blitz's `snapshot_node` and
   `resolve_stylist` are close to a drop-in reference.
2. **Turn the style diff into graded damage.** Start with repaint, overflow and
   stacking, relayout and box construction, unioned upward with a
   damaged-descendant bit.
3. **Use Taffy's per-node cache.** Wrap child layout in `compute_cached_layout`
   and clear the cache only on damaged nodes and their ancestors. Do the same
   for Huginn's own inline, table and float code.
4. **Skip whole phases.** Repaint-only damage skips layout. No damage skips the
   display list. Script geometry queries on an up-to-date tree do no work.
5. **Drive animations the Blitz way**, with the three fixes Blitz already paid
   for: the render-blocking gate, purging sets of removed nodes, and syncing
   pseudo-element styles.
6. **Keep an explicit list of attributes that layout reads directly**, and force
   full damage for them.
7. **Add differential tests and a switch.** Assert that an incremental result
   equals a fresh layout. Keep a "mark everything damaged" mode to bisect
   invalidation bugs.
8. **Run WPT on each change and show the diff.** Pick work by profiling one real
   site at a time.
9. **Cheap memory techniques.** Report native sizes to V8 as external memory.
   Decode image frames lazily. Share or intern computed styles. Drop JS source
   structures after compilation.
10. **Cache paint commands per node and cull clipped ones.** Keep transform and
    opacity as tree nodes, so those animations avoid layout.

One thing to keep as it is: Stylo's sequential traversal. Blitz defaults to it
after hitting panics with the parallel pool.
