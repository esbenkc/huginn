# Win formula: Huginn vs Ladybird

`bench.py` applies this formula and prints the verdict when both `huginn` and
`ladybird` are in `--browsers`. The code is `evaluate()` in `bench.py`.

## Inputs per page

For each corpus page and each browser, `bench.py` runs N cold loads (default N = 5).
Each load ends in a 1280x800 screenshot. Per browser and page it keeps:

- **wall**: median over the N runs of the time from process launch to process exit.
- **peak RSS**: median over the N runs of the peak of the summed resident size of the
  browser's whole process tree (sampled every 20 ms; see README.md).
- **SSIM**: median over the N runs of SSIM between the browser's screenshot and the
  Chrome reference screenshot of the same page.

A failed run (non-zero exit, timeout, or no screenshot) still counts. Its wall time is
the time until it exited or was killed. Its SSIM is computed on whatever it saved,
or 0 if it saved nothing.

## Per page

Huginn wins a page on:

- **memory** if its peak RSS < Ladybird's peak RSS,
- **time** if its median wall < Ladybird's median wall,
- **fidelity** if its SSIM >= Ladybird's SSIM - 0.02.

Huginn cannot win memory or time on a page where all its runs failed. A browser that
crashes early is cheap, but it did not do the work.

## Overall

Huginn wins the benchmark if, over all corpus pages:

- it wins **memory on at least 80%** of pages, and
- it wins **time on at least 80%** of pages, and
- it wins **fidelity on at least 50%** of pages.

With 14 pages this means at least 12 memory wins, 12 time wins and 7 fidelity wins.

## Why these numbers

- The 0.02 SSIM slack: two Chrome runs of the same snapshot do not always give the
  same pixels (animations, carousels, clocks, async font loading). The baseline
  records Chrome-vs-Chrome SSIM per page as a noise floor. Where it is below 1.0,
  differences of about 0.02 are noise.
- Memory and time need a large majority, because they are the claim ("lighter and
  faster than Ladybird"). Fidelity needs only parity on half the pages, because
  matching Chrome pixel-for-pixel is not the goal; not being visibly worse than
  Ladybird is.

## Changes

None yet. Any change to the thresholds must add a dated line here with the reason.
