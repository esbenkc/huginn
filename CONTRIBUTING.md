# Contributing to Huginn

Thank you for helping. Read [ARCHITECTURE.md](ARCHITECTURE.md) first, then the
section of [CONTRACTS.md](CONTRACTS.md) for the crate you change.

## Build and test

```sh
cargo build --release -p huginn-shell        # target/release/huginn
cargo test --workspace                        # every test
cargo test --release -p huginn-shell --test fixtures   # renders fixtures/ with the real binary
```

`--profile fastdev` builds much faster (no LTO, incremental). Use it while you
work, for example `cargo test --profile fastdev --workspace`. Never quote time
or memory numbers from it: those come from `--profile bench-release` only.

All tests must pass before you open a pull request.

## The integrity rule: fix the browser, not the page

Huginn is measured on real pages (see [bench/](bench/)). It is tempting to make
a page look right with a special case. Do not.

- No code may check for a host, a URL, a page title, a class name or any other
  sign of a specific site to change what Huginn does.
- A fix must be general web platform behavior: what the HTML and CSS
  specifications say, or what Chrome does where they leave room.
- Each fix needs a small test or fixture of your own that shows the problem:
  a synthetic HTML page in `fixtures/` or `crates/*/tests/`. Never add a copy
  of a third-party page, image or font to the repository.
- Report benchmark results as they are, including pages that got worse.
  Do not change the corpus or the win formula in `bench/WIN.md` to make a
  result look better; any change there needs a dated line with the reason.

## Changing interfaces

The public items of each crate are listed in CONTRACTS.md. If you change one,
update the code, CONTRACTS.md and every caller in the same commit.

## Style

- Run `cargo fmt` before you commit.
- Comments and docs: short sentences, plain English, one idea per sentence.
- Keep memory and time in mind: Huginn is one process, and the benchmark
  measures both.

## License

Unless you state otherwise, any contribution you submit for inclusion in Huginn,
as defined in the Apache-2.0 license, is dual licensed under MIT OR Apache-2.0,
without any additional terms or conditions.
