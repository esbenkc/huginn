# Huginn performance: shape-cache

- date: 2026-11-12
- commit: e90cc7b (with uncommitted changes)
- binary: `target/bench-release/huginn`
- machine: Linux x86_64, 24 CPUs
- runs per page: 5

## Load

Median of the runs, in ms. `cpu` is all work that does not wait for the
network (every phase but `fetch`, minus the time scripts waited). `total` is
process start to PNG written, with its lowest and highest run. Pages come
from the live network unless the target is a file, so only `cpu`, the
phases other than `fetch`, and memory are comparable between runs.

| page | cpu | parse | script | of which timers | script waited | script layout (passes) | style | layout | paint | total (min-max) | peak RSS MB | nodes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| motherfuckingwebsite.com/ | 43 | 1 | 14 | 0 | 0 | 0 (0) | 0 | 22 | 3 | 266 (250-412) | 50 | 109 |
| news.ycombinator.com/ | 40 | 2 | 14 | 0 | 0 | 0 (0) | 2 | 18 | 3 | 190 (180-207) | 51 | 1297 |
| www.craigslist.org/about/sites | 119 | 4 | 101 | 65 | 0 | 52 (3) | 3 | 8 | 3 | 407 (394-472) | 74 | 4035 |
| en.wikipedia.org/wiki/Web_browser | 188 | 10 | 238 | 217 | 81 | 48 (2) | 4 | 10 | 4 | 379 (359-445) | 76 | 8072 |
| developer.mozilla.org/en-US/docs/W | 290 | 5 | 2317 | 2259 | 2045 | 61 (5) | 3 | 7 | 3 | 2870 (2516-3208) | 78 | 5387 |
| github.com/servo/servo | 187 | 67 | 26 | 0 | 0 | 0 (0) | 21 | 71 | 2 | 512 (497-851) | 103 | 3468 |
| docs.rs/serde/latest/serde/ | 48 | 5 | 263 | 250 | 247 | 0 (0) | 4 | 17 | 4 | 911 (734-1383) | 60 | 824 |
| doc.rust-lang.org/book/ch01-00-get | 56 | 2 | 47 | 0 | 0 | 24 (3) | 1 | 3 | 2 | 151 (138-163) | 55 | 1073 |
| blog.kran.ai/ | 92 | 4 | 81 | 0 | 0 | 62 (1) | 1 | 3 | 2 | 585 (397-755) | 67 | 605 |
| blog.kran.ai/the-ai-bubble | 104 | 4 | 93 | 0 | 0 | 66 (3) | 0 | 3 | 3 | 522 (470-639) | 69 | 401 |
| juniper.xyz/ | 46 | 1 | 39 | 1 | 0 | 24 (6) | 0 | 2 | 3 | 452 (433-563) | 54 | 270 |
| www.bbc.com/news/articles/c3x298n8 | 453 | 4 | 439 | 328 | 0 | 115 (10) | 1 | 5 | 1 | 800 (709-825) | 104 | 2841 |
| tailwindcss.com/ | 83 | 19 | 24 | 0 | 0 | 0 (0) | 12 | 25 | 3 | 752 (728-846) | 95 | 3237 |
| todomvc.com/examples/react/dist/ | 57 | 1 | 49 | 26 | 17 | 0 (0) | 0 | 19 | 3 | 166 (145-476) | 53 | 238 |

Geometric mean over 14 pages: cpu 96 ms, peak RSS 68 MB.

## Interaction

Synthetic pages, 1200x800 window at scale 1, no window system. Each cell is
median / 95th percentile / maximum in ms for one step. `update` is one whole
page update on the engine thread (restyle, new images, layout); `style` and
`layout` are its parts. `raster` is one 256 px strip; `compose` is one frame.

| scenario | one step is | update | style | layout | raster | compose |
|---|---|---|---|---|---|---|
| scroll | scroll 120 px and draw a frame, 5000 paragraphs | - | - | - | 1.0 / 1.2 / 1.7 (n=47) | 0.1 / 1.2 / 2.0 (n=100) |
| hover | move the pointer to another :hover target, 3000 links | 10.3 / 11.0 / 12.4 (n=60) | 0.9 / 1.0 / 1.1 (n=60) | 9.0 / 9.7 / 11.1 (n=60) | 0.8 / 1.5 / 1.8 (n=180) | 2.8 / 3.3 / 4.6 (n=60) |
| type | type one character in a text field, 3000 list items | 34.5 / 35.9 / 36.7 (n=50) | 0.7 / 0.7 / 0.8 (n=50) | 33.6 / 35.0 / 35.7 (n=50) | 0.5 / 1.2 / 1.4 (n=150) | 2.0 / 2.4 / 2.6 (n=50) |
| class | a script toggles a class on <body>, 3000 list items | 65.8 / 67.5 / 72.7 (n=31) | 1.7 / 1.8 / 1.9 (n=31) | 63.8 / 65.6 / 70.4 (n=31) | 0.5 / 1.1 / 1.3 (n=90) | 2.0 / 2.2 / 2.3 (n=30) |
| append | a script adds one node, 3000 list items | 42.1 / 42.8 / 46.8 (n=31) | 2.6 / 2.6 / 2.7 (n=31) | 39.1 / 39.8 / 44.0 (n=31) | 0.4 / 0.9 / 1.1 (n=90) | 1.4 / 2.4 / 2.6 (n=30) |
| resize | resize the window by 20 px, 5000 paragraphs | - | 4.1 / 4.4 / 5.3 (n=20) | 67.9 / 73.0 / 75.3 (n=20) | 1.0 / 1.4 / 1.7 (n=60) | 4.7 / 5.0 / 5.0 (n=20) |
