# V8 flags on page load

- date: 2026-11-12
- commit: 9ba8f2c
- binary: `target/bench-release/huginn`, flags set with `HUGINN_V8_FLAGS`
- machine: Linux x86_64, 24 CPUs
- 5 cold runs per page and flag set, live sites, `bench/perf.py --only load`

Each cell is the median CPU time in ms (all work that does not wait for the
network) and the peak RSS in MB.

| page | lite | default | sparkplug | jitless |
|---|---|---|---|---|
| motherfuckingwebsite.com/ | 30 ms, 45 MB | 37 ms, 45 MB | 39 ms, 45 MB | 30 ms, 45 MB |
| news.ycombinator.com/ | 35 ms, 47 MB | 49 ms, 48 MB | 40 ms, 47 MB | 35 ms, 47 MB |
| www.craigslist.org/about/sites | 110 ms, 69 MB | 117 ms, 75 MB | 118 ms, 72 MB | 115 ms, 72 MB |
| en.wikipedia.org/wiki/Web_browser | 171 ms, 71 MB | 169 ms, 81 MB | 164 ms, 78 MB | 174 ms, 77 MB |
| developer.mozilla.org/en-US/docs/Web/HTM | 280 ms, 73 MB | 240 ms, 77 MB | 280 ms, 78 MB | 229 ms, 73 MB |
| github.com/servo/servo | 178 ms, 99 MB | 179 ms, 99 MB | 176 ms, 100 MB | 183 ms, 99 MB |
| docs.rs/serde/latest/serde/ | 52 ms, 57 MB | 55 ms, 57 MB | 52 ms, 57 MB | 53 ms, 57 MB |
| doc.rust-lang.org/book/ch01-00-getting-s | 55 ms, 52 MB | 55 ms, 52 MB | 53 ms, 52 MB | 53 ms, 52 MB |
| blog.kran.ai/ | 92 ms, 62 MB | 91 ms, 62 MB | 92 ms, 62 MB | 94 ms, 62 MB |
| blog.kran.ai/the-ai-bubble | 106 ms, 64 MB | 106 ms, 67 MB | 105 ms, 64 MB | 105 ms, 65 MB |
| juniper.xyz/ | 46 ms, 50 MB | 49 ms, 50 MB | 46 ms, 50 MB | 45 ms, 50 MB |
| www.bbc.com/news/articles/c3x298n87rn2o | 439 ms, 99 MB | 428 ms, 140 MB | 410 ms, 130 MB | 442 ms, 128 MB |
| tailwindcss.com/ | 77 ms, 92 MB | 80 ms, 92 MB | 76 ms, 94 MB | 75 ms, 92 MB |
| todomvc.com/examples/react/dist/ | 46 ms, 49 MB | 43 ms, 49 MB | 43 ms, 49 MB | 41 ms, 49 MB |
| **geometric mean** | **89 ms, 64 MB** | **92 ms, 67 MB** | **90 ms, 66 MB** | **87 ms, 66 MB** |

The flag sets:

- **lite**: `--single-threaded --lite-mode` (today)
- **default**: `--single-threaded`
- **sparkplug**: `--single-threaded --sparkplug --no-maglev --no-turbofan`
- **jitless**: `--single-threaded --jitless`

## Reading

- CPU time is the same for all four within the run-to-run noise of this benchmark
  (about 3 % on the geometric mean).
- Memory is lowest with `--lite-mode`. The largest gap is the BBC article: 99 MB
  against 128 to 140 MB.
- An earlier check on one page (the BBC News front page, 3 runs) had default V8 use a
  third less JavaScript CPU than lite mode. That does not hold across these 14 pages.
- So `--lite-mode` stays the right choice for page load on this corpus. This says
  nothing about long-running scripts after load, which this benchmark does not time.
