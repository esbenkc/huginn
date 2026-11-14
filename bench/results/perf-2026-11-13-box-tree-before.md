# Huginn performance: box-tree-before

- date: 2026-11-13
- commit: 95633fa (with uncommitted changes)
- binary: `../../../../tmp/claude-1000/-home-sevan-Documents/2ee40f4a-92e2-40c1-8832-c6a394bb0b13/scratchpad/wt-base/target/bench-release/huginn`
- machine: Linux x86_64, 24 CPUs
- runs per page: 5

## Interaction

Synthetic pages, 1200x800 window at scale 1, no window system. Each cell is
median / 95th percentile / maximum in ms for one step. `update` is one whole
page update on the engine thread (restyle, new images, layout); `style` and
`layout` are its parts. `raster` is one 256 px strip; `compose` is one frame.

| scenario | one step is | update | style | layout | raster | compose |
|---|---|---|---|---|---|---|
| scroll | scroll 120 px and draw a frame, 5000 paragraphs | - | - | - | 1.1 / 2.1 / 2.4 (n=47) | 0.4 / 2.0 / 2.6 (n=100) |
| hover | move the pointer to another :hover target, 3000 links | 11.0 / 12.9 / 13.9 (n=60) | 1.0 / 1.1 / 1.2 (n=60) | 9.7 / 11.6 / 12.5 (n=60) | 0.9 / 1.5 / 2.1 (n=180) | 3.0 / 4.2 / 4.6 (n=60) |
| type | type one character in a text field, 3000 list items | 31.2 / 33.3 / 37.6 (n=50) | 0.7 / 0.8 / 1.1 (n=50) | 30.3 / 32.4 / 36.3 (n=50) | 0.5 / 1.0 / 1.2 (n=150) | 1.6 / 3.0 / 3.0 (n=50) |
| class | a script toggles a class on <body>, 3000 list items | 56.3 / 61.5 / 70.9 (n=31) | 1.8 / 2.2 / 2.9 (n=31) | 54.2 / 58.1 / 68.7 (n=31) | 0.5 / 1.1 / 1.1 (n=90) | 1.8 / 2.4 / 2.6 (n=30) |
| append | a script adds one node, 3000 list items | 40.8 / 43.8 / 45.2 (n=31) | 2.6 / 2.8 / 2.8 (n=31) | 37.9 / 40.8 / 42.2 (n=31) | 0.5 / 0.5 / 0.9 (n=90) | 1.5 / 2.0 / 2.0 (n=30) |
| resize | resize the window by 20 px, 5000 paragraphs | - | 4.3 / 5.8 / 6.3 (n=20) | 69.3 / 81.6 / 110.1 (n=20) | 1.1 / 1.6 / 1.7 (n=60) | 4.6 / 5.4 / 6.6 (n=20) |
