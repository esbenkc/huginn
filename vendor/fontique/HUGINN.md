# Fontique in Huginn

This is [Fontique](https://github.com/linebender/parley/tree/main/fontique)
0.12.0 as published on crates.io, with one change. The workspace uses it
through `[patch.crates-io]` in the top `Cargo.toml`. List the changed places
with `grep -rn huginn: vendor/fontique/src`.

## The change (fontconfig backend, Linux and FreeBSD only)

Upstream fills each generic family (`serif`, `sans-serif`, `monospace`, ...)
with every family that `FcFontSort` returns: every font that adds Unicode
coverage. On a desktop with the Noto fonts that is about 170 families. A
query for a character that the first family lacks (an emoji, an arrow, a CJK
character) then walks that list and opens every font file on the way.
Measured on Ubuntu 26.04 with 651 font files: one emoji in a paragraph opened
480 font files and cost 70 ms.

The change:

- `SystemFonts::new`: a generic family keeps its first 3 families.
- `SystemFonts::fallback_list` (new): the fallback for a script is a list,
  not one family. It holds every family, sorted by fontconfig for a sample of
  the script, so the first entry is the same family as upstream's single
  match and the search can still end at any installed font. For scripts whose
  text often has symbols in it (Latin, Greek, Cyrillic, common), the 8 fonts
  with the most of a symbol sample come right after the first entry.
- `Collection::fallback_families` calls `fallback_list` on these systems.

This is close to what Chrome does on Linux: the generic family is fontconfig's best match,
and other characters are looked up apart from it.

What changes for pages:

- Emoji come from the color emoji font. Before, a text font that happened
  to have the glyph could win.
- A character that the page's font lacks comes from fontconfig's best font
  for its script, whatever generic family the text asked for. Before, serif
  text got a serif fallback when one came first in the long list.
- No character lost its glyph in a comparison of 1972 sampled characters in
  serif, sans-serif and monospace text.

## Updating

Copy the new release over `Cargo.toml`, `README.md`, the licenses and `src`,
drop the `[[example]]` entries from `Cargo.toml` (the examples are not
vendored), and apply the `huginn:` changes again. The other platform
backends are unchanged.
