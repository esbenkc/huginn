# Huginn benchmark

Page-load benchmark for Huginn against Ladybird, with Chrome as the visual reference.

It answers one question: on a fixed set of real pages, served from local snapshots,
does Huginn load and render a 1280x800 screenshot with less memory and less time
than Ladybird, while looking at least as close to Chrome as Ladybird does?

The win formula is in [WIN.md](WIN.md). Speedometer and other JS throughput tests are
out of scope: we will not beat a JIT engine there soon, and this bench does not try.

The page snapshots are **not** in this repository. They are copies of third-party sites.
`corpus.json` lists the 14 source URLs; `capture.mjs` makes your own snapshot of them
(see "Capture the corpus" below). Live pages change, so your snapshot and your numbers
will differ from ours.

## Files

| file | what it does |
|---|---|
| `corpus.json` | the 14 pages: name, live URL, local port |
| `capture.mjs` | snapshots a live page and every subresource Chrome fetched into `corpus/<name>/` |
| `serve.py` | static replay server on 127.0.0.1, one port per page |
| `check_corpus.py` | screenshots live vs local in Chrome, lists 404s per page, writes `corpus/check.png` (`--reuse-live`: no internet) |
| `procmem.py` | samples memory of a whole process tree via libproc |
| `bench.py` | the benchmark harness |
| `perf.py` | time per phase for one Huginn binary: page loads and scripted interaction (scroll, hover, typing, script changes, resize). No other browser and no corpus needed; runs on Linux and macOS |
| `fidelity.py` | quick developer check: renders corpus pages with one Huginn binary and prints SSIM against the Chrome reference (no timing claims) |
| `WIN.md` | the win formula |
| `ladybird-screenshot-on-load.patch` | the local Ladybird patch (see below; BSD-2-Clause, like Ladybird) |
| `results/` | your runs: JSON, Markdown tables, contact sheets, screenshots. The repository has only the Markdown summaries of our runs. |

## Setup (once)

You need macOS on Apple silicon, Node.js, Python 3 and Google Chrome (for `capture.mjs`).

```sh
cd bench
npm install                                   # puppeteer-core, for capture.mjs
npx @puppeteer/browsers install chrome-headless-shell@157.0.8103.51 --path ./browsers
python3 -m venv .venv && .venv/bin/pip install numpy pillow scikit-image
```

`bench.py` re-runs itself under `.venv/bin/python` when the system `python3` lacks numpy.

## Capture the corpus

```sh
node capture.mjs --all            # or: node capture.mjs <name>, one page from corpus.json
python3 check_corpus.py           # live vs local screenshots and 404s per page
```

`capture.mjs` drives `/Applications/Google Chrome.app` and writes `corpus/<name>/`.
Lazy assets that the capture misses show up as 404s in `check_corpus.py`; add them by
hand or accept them (every browser gets the same 404). Keep `corpus/` to yourself: it holds
copies of other people's pages.

## Run

```sh
python3 bench.py --browsers chrome,ladybird --label baseline     # Chrome + Ladybird baseline
HUGINN=/path/to/huginn python3 bench.py --browsers chrome,ladybird,huginn
HUGINN=/path/to/huginn python3 bench.py --browsers huginn   # compares to the newest Ladybird results
```

Options: `--runs N` (default 5), `--pages a,b`, `--timeout S` (default 60), `--label L`,
`--compare results/x.json`, `--no-warmup`, `--update-ref`.

Binaries: `HUGINN` is required for the `huginn` browser. `CHROME` and `LADYBIRD` override
the default paths (`browsers/chrome-headless-shell/...` from the setup step, and
`~/src/ladybird/Build/release/bin/Ladybird.app/Contents/MacOS/Ladybird`).

For a quick look while you work on Huginn, without timing claims:
`python3 bench.py --browsers chrome --update-ref` once (writes `results/ref/chrome/`), then
`.venv/bin/python fidelity.py --huginn ../target/release/huginn [--pages mdn,github]`.

Outputs: `results/<label>-<date>.json` (every run), `results/<label>.md` (table and
verdict), `results/<label>-contact.png` (screenshots side by side), and the Chrome
reference screenshots in `results/ref/chrome/<page>.png`.

`bench.py` starts `serve.py` itself if the corpus ports are not already open.
To serve by hand: `python3 serve.py [--log-misses] [page ...]`. Each page is then at the
`local_url` in its `corpus/<name>/manifest.json`, for example `http://127.0.0.1:8104/wiki/Web_browser`.

## Time per phase (`perf.py`)

`bench.py` answers "is Huginn ahead of Ladybird". `perf.py` answers "where
does Huginn spend its time, and did this change help". It needs only Python 3
and a Huginn binary.

```sh
cargo build --profile bench-release -p huginn-shell
python3 bench/perf.py --label before          # writes results/perf-<date>-before.md
# ... change the code, build again ...
python3 bench/perf.py --label after
```

- **Load.** `huginn render` of each URL in `corpus.json` (or the targets you
  pass), 5 cold runs. Pages come from the live network, so `fetch` and `total`
  are noisy. Compare `cpu`: every phase but `fetch`, minus the time scripts
  waited for the network. Peak RSS is the kernel's figure for the process.
- **Interaction.** Synthetic pages driven with `huginn open --commands`, no
  window. One row per scenario with the median, 95th percentile and maximum
  time of one step, per phase (see ARCHITECTURE.md, "Timing").

Run both binaries on the same machine, one after the other, with nothing else
running. `perf.py` has no fidelity check: run the tests and `fidelity.py` too.

## How each browser is run

All three get a fresh temporary profile per run, a 1280x800 viewport at device scale 1,
and the page URL on 127.0.0.1. The harness launches the process, waits for it to exit,
and reads the PNG it wrote.

**Chrome** (`chrome-headless-shell` 157.0.8103.51, the Chrome-for-Testing headless build):

```sh
chrome-headless-shell --headless --user-data-dir=<tmp> --no-first-run --no-default-browser-check \
  --use-mock-keychain --disable-background-networking --disable-component-update --disable-sync \
  --disable-extensions --hide-scrollbars --force-device-scale-factor=1 --window-size=1280,800 \
  --host-resolver-rules="MAP * ~NOTFOUND , EXCLUDE 127.0.0.1" --screenshot=<png> <url>
```

Why not `/Applications/Google Chrome.app --headless`: on this machine, full Chrome with a fresh
profile starts Google Updater, installs default web apps, and takes 20-30 s per launch.
That is setup noise, not page loading. `chrome-headless-shell` has the same Blink and Skia,
so it renders the same pixels, and it starts in well under a second.
`--screenshot` waits for the load event, then captures the viewport.

**Ladybird** (built from source, see "Ladybird build"):

```sh
~/src/ladybird/Build/release/bin/Ladybird.app/Contents/MacOS/Ladybird \
  --headless=screenshot --screenshot-on-load --screenshot-path=<png> \
  --window-width=1280 --window-height=800 --profile-path=<tmp> \
  --disable-http-disk-cache --dns-server=127.0.0.1 --dns-port=8153 <url>
```

A new `--profile-path` also starts a new browser process. (`--force-new-process` is
deprecated upstream and is an error together with `--profile-path`.)
UDP port 8153 is a DNS server inside `serve.py` that answers NXDOMAIN to every name at once.
It is Ladybird's equivalent of Chrome's `--host-resolver-rules`. With a closed DNS port,
Ladybird waits 5 s for a DNS timeout on any page with an uncaptured third-party request
(for example the kran.ai blog's analytics beacon, and craigslist). That cost was the harness,
not the browser, so the bench removes it.

**Huginn**:

```sh
$HUGINN render <url> --out <png> --width 1280 --height 800
```

It must print one JSON line with its phase timings; `bench.py` stores it per run.

## Ladybird build

```sh
brew install autoconf autoconf-archive automake ccache cmake libtool make nasm ninja pkg-config
git clone https://github.com/LadybirdBrowser/ladybird.git ~/src/ladybird
cd ~/src/ladybird
git checkout b1476b89            # the commit the baseline used
git apply /path/to/huginn/bench/ladybird-screenshot-on-load.patch
./Meta/ladybird.py build -j 10 ladybird   # Release preset; vcpkg builds the third-party deps first
```

Build record (2026-11-12): Ladybird `b1476b89` ("LibWeb: Split SRI metadata on ASCII
whitespace"), Release preset, Apple clang 17, macOS 26.3 arm64.

### The patch, and why

Stock Ladybird has a headless screenshot mode (`--headless=screenshot`), but it waits a
fixed `--screenshot-delay` (default 1 s) after starting the load, and then paints the
**whole document** (every pixel of a long page, e.g. 1280x12000 for Wikipedia).
Neither works for a load-time benchmark: wall time would be the delay, and Ladybird
would pay for painting and PNG-encoding pages of content nobody asked for.

`ladybird-screenshot-on-load.patch` (62 added lines, 8 changed, in 11 files) adds `--screenshot-on-load`.
With it, the UI process asks for a screenshot when WebContent reports the document's
readiness as "complete", which is the step just before the load event fires. The request
goes over IPC and WebContent serves it at its next rendering opportunity, so load event
handlers have run by then (the same moment Chrome's `--screenshot` uses). WebContent paints
only the viewport: a new `take_viewport_screenshot` IPC message queues the same screenshot
task as the full-document screenshot, with the viewport rect instead of the document's
scrollable overflow rect. Same paint path, same display list; only the size of the target
bitmap changes (1280x800, 4 MB, instead of the whole page). Without the flag, Ladybird
behaves exactly as upstream. Checked at `b1476b89`: upstream has no way to screenshot at
load (only `--screenshot-delay`, whole seconds, then a full-document screenshot).

## Measurement method

**Wall time**: from `Popen` to process exit, measured in the harness. It includes process
startup, loading, layout, paint, PNG encoding and shutdown, for every browser alike.

**Peak memory of the process tree.** Ladybird runs a UI process plus WebContent,
RequestServer, ImageDecoder and other helpers. Chrome runs a browser process plus GPU,
network and renderer helpers. `/usr/bin/time -l` only reports the root process (and the
largest single child), so it misses most of a multi-process browser. Instead, `procmem.py`
runs a thread that every 20 ms:

1. lists the tree: children of the root by parent pid, recursively (`proc_listchildpids`),
   plus every process in the root's process group (the harness starts each browser in a
   new session, so the group is the browser's);
2. reads each process's `ri_resident_size` and `ri_phys_footprint` (`proc_pid_rusage`);
3. sums each over the tree and keeps the peak of each sum.

It uses libproc through ctypes, so a sample costs microseconds, not a `ps` fork.

- **peak RSS** (used by the win formula): peak of the summed resident size. This is what
  `ps` and `time -l` report per process. Summing it counts shared pages (system frameworks,
  shared-memory bitmaps) once per process, so it overstates multi-process browsers.
- **peak footprint** (reported, not scored): peak of the summed `phys_footprint`, which is
  Activity Monitor's "Memory" column. It counts private and GPU memory and does not
  double count shared framework pages.

**Visual fidelity**: SSIM (scikit-image `structural_similarity`, 7x7 window, 8-bit luma,
data range 255) between a browser's 1280x800 screenshot and the Chrome reference screenshot
of the same page. Screenshots that are not 1280x800 are cropped from the top-left or padded
with white. The Chrome reference is the Chrome run (of N) most similar to the other Chrome
runs. Chrome's own SSIM column (other runs vs the reference) is the noise floor of the page.

**Order**: one untimed warm-up run per browser first (the first launch of a binary pays for
macOS code-signature checks). Then, for each page, N rounds; each round runs every browser
once, and the order of browsers rotates each round, so slow drift does not favour one browser.

## Corpus

14 pages, captured on 2026-11-12. Each one is served on its own port so that root-relative
URLs (`/static/app.js`) and JS `fetch('/api/...')` resolve inside the snapshot.

`capture.mjs` loads the live page in Chrome (puppeteer-core), scrolls to the bottom to
trigger lazy loading, waits for the network to go idle, and stores every response body.
Then it rewrites absolute URLs in HTML, CSS, JS and JSON: the page's own host becomes
`http://127.0.0.1:<port>`, other hosts become `http://127.0.0.1:<port>/__h/<host>`
(plain and JSON-escaped forms, with and without scheme). It strips `integrity=` attributes
(the bytes changed) and CSP `<meta>` tags (they would block the local origin).
`serve.py` answers from the manifest with the recorded Content-Type; anything else is 404.

Requests that the snapshot cannot answer get a 404 from the local server. Chrome is run
with all other hosts unresolvable. Ladybird is pointed at `serve.py`'s DNS server on
udp 127.0.0.1:8153, which answers NXDOMAIN to every name, so it cannot reach the internet
either and fails such lookups at once. Huginn runs with `HUGINN_NET_LOOPBACK_ONLY=1`, which
makes it fail every non-loopback host at once.

Corpus check (2026-11-12, `python3 check_corpus.py --reuse-live`): in Chrome, every page
loads from the local server with zero 404s, except `bbc`. Lazy assets that the capture
missed were added by hand: 3 MDN badge SVGs, the kran.ai blog video and poster, and 15
Tailwind `_next/image` logos. `bbc` still 404s on its two consent bundles
(`gdpr-tcf`, `usnat-gpp`). This is on purpose: without them the page shows the article
instead of a cookie dialog, and every browser gets the same 404. SSIM(local vs live) is
0.96 to 1.00 for all pages except `hackernews` (0.91: the front page changed) and `bbc`
(0.59: the live page showed the consent dialog). `mdn` varies run to run: its "Try it"
widget renders around the load event, so it is sometimes in the screenshot and sometimes not.

## Caveats

- Wall time includes browser startup and shutdown. For tiny pages that is most of the time.
- Peak memory is sampled every 20 ms; a spike shorter than that can be missed.
- Summed RSS double counts shared pages, which favours single-process browsers. The
  footprint column shows the same comparison without that effect.
- SSIM punishes small shifts. A one-pixel line-height difference over a page of text drops
  SSIM a lot, even when the page looks the same to a person. Compare SSIM between browsers
  on the same page, not across pages.
- Live pages change: the snapshot is frozen, but some pages run clocks, carousels or random
  content in JS, so even two Chrome runs differ. See Chrome's SSIM column.
- The load event is a proxy for "rendered". Pages that render content after load (lazy JS,
  late web fonts) can look different at screenshot time in different browsers.
- The machine is a laptop: thermal state and background processes add noise. Medians of 5
  help but do not remove it.
- Ladybird's summed footprint is often larger than its summed RSS. Footprint counts
  memory that is not resident (compressed pages, IOSurfaces and other GPU memory).
- Peak RSS also takes the root process's own maximum from `wait4` (`ru_maxrss`), so a run
  that ends before the first 20 ms sample still has an RSS. Footprint has no such fallback:
  a footprint of 0 (Huginn on `plain` and `hackernews`) means no sample was taken.
- Baseline conditions (2026-11-12, 07:54): other builds were running at background
  priority on the efficiency cores; load average 3.8 at the start. Treat differences
  under about 10% as noise.

## Published results

`results/2026-11-12-baseline.md` is Chrome and Ladybird alone.
`results/2026-11-12-huginn-92cc567.md` is all three browsers, 5 runs per page, with
Huginn at commit 92cc567 built with `--profile bench-release`. That run was recorded
before the project was renamed, when the binary was called `kbrowser`; only the label
in the table changed.
