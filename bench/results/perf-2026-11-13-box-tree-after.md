# Huginn performance: box-tree-after

- date: 2026-11-13
- commit: 95633fa (with uncommitted changes)
- binary: `target/bench-release/huginn`
- machine: Linux x86_64, 24 CPUs
- runs per page: 5

## Interaction

Synthetic pages, 1200x800 window at scale 1, no window system. Each cell is
median / 95th percentile / maximum in ms for one step. `update` is one whole
page update on the engine thread (restyle, new images, layout); `style` and
`layout` are its parts. `raster` is one 256 px strip; `compose` is one frame.

| scenario | one step is | update | style | layout | raster | compose |
|---|---|---|---|---|---|---|
| scroll | scroll 120 px and draw a frame, 5000 paragraphs | - | - | - | 1.1 / 1.4 / 1.5 (n=47) | 0.2 / 1.4 / 2.0 (n=100) |
| hover | move the pointer to another :hover target, 3000 links | 8.7 / 12.3 / 17.3 (n=60) | 0.9 / 1.1 / 1.5 (n=60) | 7.5 / 10.8 / 15.5 (n=60) | 1.1 / 1.6 / 2.1 (n=180) | 4.3 / 5.9 / 7.2 (n=60) |
| type | type one character in a text field, 3000 list items | 8.4 / 12.3 / 16.5 (n=50) | 0.7 / 0.9 / 1.1 (n=50) | 7.6 / 11.3 / 14.9 (n=50) | 0.5 / 1.1 / 1.4 (n=150) | 1.7 / 2.7 / 3.5 (n=50) |
| class | a script toggles a class on <body>, 3000 list items | 50.2 / 52.6 / 53.5 (n=31) | 1.7 / 1.9 / 1.9 (n=31) | 48.2 / 50.6 / 51.4 (n=31) | 0.5 / 1.0 / 1.4 (n=90) | 1.6 / 2.5 / 2.6 (n=30) |
| append | a script adds one node, 3000 list items | 16.2 / 19.6 / 20.9 (n=31) | 2.6 / 3.3 / 4.2 (n=31) | 13.3 / 16.6 / 17.9 (n=31) | 0.6 / 1.2 / 1.4 (n=90) | 2.0 / 3.0 / 3.2 (n=30) |
| resize | resize the window by 20 px, 5000 paragraphs | - | 4.2 / 5.3 / 5.3 (n=20) | 59.3 / 68.7 / 69.8 (n=20) | 1.1 / 1.5 / 2.3 (n=60) | 4.7 / 6.3 / 7.0 (n=20) |
