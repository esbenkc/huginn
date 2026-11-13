# Huginn performance: baseline-e28cdc3

- date: 2026-11-12
- commit: e28cdc3 (with uncommitted changes)
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
| motherfuckingwebsite.com/ | 36 | 1 | 14 | 0 | 0 | 0 (0) | 0 | 16 | 3 | 259 (245-350) | 49 | 109 |
| news.ycombinator.com/ | 35 | 2 | 10 | 0 | 0 | 0 (0) | 2 | 18 | 3 | 186 (179-219) | 50 | 1297 |
| www.craigslist.org/about/sites | 111 | 3 | 93 | 63 | 0 | 52 (3) | 2 | 8 | 3 | 427 (391-481) | 72 | 4035 |
| en.wikipedia.org/wiki/Web_browser | 163 | 8 | 234 | 209 | 93 | 45 (2) | 4 | 10 | 4 | 380 (338-438) | 75 | 8072 |
| developer.mozilla.org/en-US/docs/W | 280 | 5 | 2068 | 2009 | 1838 | 61 (5) | 3 | 7 | 3 | 2599 (2314-2920) | 76 | 4567 |
| github.com/servo/servo | 174 | 54 | 25 | 0 | 0 | 0 (0) | 21 | 70 | 2 | 501 (478-809) | 102 | 3468 |
| docs.rs/serde/latest/serde/ | 60 | 7 | 250 | 238 | 234 | 0 (0) | 3 | 26 | 4 | 1013 (636-1040) | 60 | 824 |
| doc.rust-lang.org/book/ch01-00-get | 56 | 2 | 47 | 0 | 0 | 24 (3) | 1 | 2 | 2 | 166 (150-175) | 54 | 1073 |
| blog.kran.ai/ | 90 | 4 | 80 | 0 | 0 | 62 (1) | 1 | 3 | 3 | 562 (439-1577) | 66 | 605 |
| blog.kran.ai/the-ai-bubble | 107 | 4 | 96 | 0 | 0 | 67 (3) | 0 | 3 | 3 | 649 (453-3255) | 68 | 401 |
| juniper.xyz/ | 45 | 2 | 38 | 1 | 0 | 24 (6) | 0 | 2 | 3 | 469 (417-596) | 54 | 270 |
| www.bbc.com/news/articles/c3x298n8 | 433 | 4 | 423 | 318 | 0 | 111 (10) | 1 | 4 | 1 | 732 (692-1079) | 102 | 2841 |
| tailwindcss.com/ | 75 | 15 | 22 | 0 | 0 | 0 (0) | 12 | 24 | 2 | 791 (689-802) | 94 | 3235 |
| todomvc.com/examples/react/dist/ | 50 | 1 | 46 | 27 | 17 | 0 (0) | 0 | 17 | 3 | 247 (150-498) | 51 | 238 |

Geometric mean over 14 pages: cpu 91 ms, peak RSS 67 MB.

## Interaction

Synthetic pages, 1200x800 window at scale 1, no window system. Each cell is
median / 95th percentile / maximum in ms for one step. `update` is one whole
page update on the engine thread (restyle, new images, layout); `style` and
`layout` are its parts. `raster` is one 256 px strip; `compose` is one frame.

| scenario | one step is | update | style | layout | raster | compose |
|---|---|---|---|---|---|---|
| scroll | scroll 120 px and draw a frame, 5000 paragraphs | - | - | - | 1.0 / 1.2 / 1.4 (n=47) | 0.2 / 1.2 / 1.6 (n=100) |
| hover | move the pointer to another :hover target, 3000 links | 23.1 / 24.7 / 34.7 (n=60) | 0.9 / 1.0 / 1.6 (n=60) | 21.9 / 23.5 / 33.4 (n=60) | 0.8 / 1.4 / 2.1 (n=180) | 2.9 / 3.2 / 4.4 (n=60) |
| type | type one character in a text field, 3000 list items | 52.2 / 53.1 / 55.1 (n=50) | 0.7 / 0.7 / 0.8 (n=50) | 51.4 / 52.3 / 54.2 (n=50) | 0.5 / 0.5 / 1.0 (n=150) | 1.5 / 1.8 / 2.1 (n=50) |
| class | a script toggles a class on <body>, 3000 list items | 60.2 / 61.5 / 68.5 (n=31) | 1.6 / 1.7 / 1.7 (n=31) | 58.2 / 59.7 / 66.5 (n=31) | 0.5 / 1.1 / 1.2 (n=90) | 1.9 / 2.2 / 2.4 (n=30) |
| append | a script adds one node, 3000 list items | 61.5 / 63.4 / 66.4 (n=31) | 2.5 / 2.7 / 3.7 (n=31) | 58.6 / 60.5 / 62.3 (n=31) | 0.4 / 1.0 / 1.1 (n=90) | 1.3 / 2.1 / 2.1 (n=30) |
| resize | resize the window by 20 px, 5000 paragraphs | - | 4.1 / 4.4 / 4.4 (n=20) | 163.3 / 166.4 / 168.4 (n=20) | 1.0 / 1.3 / 2.2 (n=60) | 4.6 / 4.8 / 5.9 (n=20) |
