# Third-party code and licenses

Huginn's own code is licensed under MIT OR Apache-2.0 (see `LICENSE-MIT` and
`LICENSE-APACHE`). It uses the libraries below. Their licenses apply to their code.

## Third-party files in this repository

| path | what | license |
|---|---|---|
| `vendor/taffy/` | Taffy 0.15.1 with small layout fixes. Each change is marked `huginn:`; list them with `grep -rn huginn: vendor/taffy/src`. | MIT, `vendor/taffy/LICENSE` |
| `crates/huginn-js/data/icudtl.dat` | ICU 78 data, the `flutter_desktop` subset that ships in the `v8` crate 152.2.0 (`third_party/icu/flutter_desktop/icudtl.dat`), unchanged. | Unicode License V3, `crates/huginn-js/data/LICENSE-ICU` |
| `bench/ladybird-screenshot-on-load.patch` | A patch to Ladybird that adds `--screenshot-on-load`. | BSD-2-Clause, the license of Ladybird |

The benchmark page snapshots are not in this repository (see `bench/README.md`).
The browser uses system fonts only; no font files are included.

## Major dependencies

| crate | version | license | used for | source |
|---|---|---|---|---|
| stylo, stylo_traits, stylo_dom, stylo_static_prefs, selectors | 0.24.0 / 0.43.0 | MPL-2.0 | CSS cascade, selector matching, computed values (Servo) | https://github.com/servo/stylo |
| cssparser | 0.38.0 | MPL-2.0 | CSS tokenizer (Servo) | https://github.com/servo/rust-cssparser |
| app_units | 0.7.8 | MPL-2.0 | Stylo's fixed-point lengths | https://github.com/servo/app_units |
| html5ever, markup5ever | 0.40.1 / 0.40.0 | MIT OR Apache-2.0 | HTML parser (Servo) | https://github.com/servo/html5ever |
| taffy (vendored, patched) | 0.15.1 | MIT | block, flex, grid and float layout | https://github.com/DioxusLabs/taffy |
| parley (fontique, harfrust, skrifa) | 0.12.0 | Apache-2.0 OR MIT | text shaping, line breaking, font fallback | https://github.com/linebender/parley |
| vello_cpu, vello_common, glifo | 0.4.0 / 0.5.0 | Apache-2.0 OR MIT | CPU rasterizer | https://github.com/linebender/vello |
| resvg (usvg, tiny-skia) | 0.50.0 | Apache-2.0 OR MIT (tiny-skia: BSD-3-Clause) | SVG images | https://github.com/linebender/resvg |
| v8 (rusty_v8) | 152.2.0 | MIT (bindings); V8 itself: BSD-3-Clause | JavaScript engine | https://github.com/denoland/rusty_v8 |
| image | 0.25.10 | MIT OR Apache-2.0 | PNG, JPEG, GIF, WebP, BMP, ICO decoding | https://github.com/image-rs/image |
| rav1d | 1.1.0 | BSD-2-Clause | AV1 decoding for AVIF images (Rust port of dav1d) | https://github.com/memorysafety/rav1d |
| avif-parse | 2.1.0 | MPL-2.0 | AVIF (HEIF) container parsing | https://github.com/kornelski/avif-parse |
| png | 0.18.1 | MIT OR Apache-2.0 | PNG encoding | https://github.com/image-rs/image-png |
| wuff | 0.2.10 | MIT | WOFF and WOFF2 decoding | https://github.com/nicoburns/wuff |
| ureq (rustls, ring, webpki-roots) | 3.5.1 | MIT OR Apache-2.0 (ring: Apache-2.0 AND ISC; webpki-roots: CDLA-Permissive-2.0) | HTTP client and TLS | https://github.com/algesten/ureq |
| url, data-url | 2.5.8 / 0.3.2 | MIT OR Apache-2.0 | URL parsing | https://github.com/servo/rust-url |
| encoding_rs | 0.8.44 | (Apache-2.0 OR MIT) AND BSD-3-Clause | character encodings | https://github.com/hsivonen/encoding_rs |
| icu_* (ICU4X, via Stylo and Parley) | 2.x | Unicode-3.0 | Unicode segmentation and properties | https://github.com/unicode-org/icu4x |
| winit | 0.30.13 | Apache-2.0 | windows and the event loop | https://github.com/rust-windowing/winit |
| softbuffer | 0.4.8 | MIT OR Apache-2.0 | shows CPU pixels in the window | https://github.com/rust-windowing/softbuffer |
| arboard | 3.7.0 | MIT OR Apache-2.0 | the system clipboard on Linux (X11 and Wayland) | https://github.com/1Password/arboard |
| miniz_oxide | 0.8.9 | MIT OR Zlib OR Apache-2.0 | deflate for PDF output | https://github.com/Frommi/miniz_oxide |
| serde_json | 1.0.154 | MIT OR Apache-2.0 | the timing JSON line | https://github.com/serde-rs/json |

### V8

The `v8` crate downloads a prebuilt static V8 library when it builds. V8 is under the
BSD-3-Clause license. The library also contains third-party code that V8 bundles (for
example ICU, zlib, abseil-cpp and LLVM's libc++, which is Apache-2.0 WITH LLVM-exception).
See https://github.com/denoland/rusty_v8 and https://v8.dev for the full notices.
If you ship a `huginn` binary, ship those notices with it.

### MPL-2.0 (Stylo, cssparser and avif-parse)

The MPL-2.0 is a file-level copyleft license. Huginn uses these crates unchanged from
crates.io and does not copy their files into this repository, so Huginn's own files
stay MIT OR Apache-2.0. If you distribute a binary, tell recipients where the source of
the MPL-2.0 crates is (crates.io, https://github.com/servo/stylo,
https://github.com/kornelski/avif-parse).

## License summary

321 third-party crates are in the dependency graph for macOS arm64 (281 linked or used at
compile time by the crates, 40 used only by build scripts). None is GPL, LGPL or AGPL.

| license expression | crates |
|---|---|
| MIT OR Apache-2.0 | 137 |
| MIT | 57 |
| Apache-2.0 OR MIT | 32 |
| Unicode-3.0 | 27 |
| MPL-2.0 | 15 |
| Zlib OR Apache-2.0 OR MIT | 8 |
| BSD-3-Clause | 7 |
| Unlicense OR MIT | 6 |
| MIT OR Apache-2.0 OR Zlib | 4 |
| Apache-2.0 | 3 |
| Zlib | 3 |
| ISC | 3 |
| BSD-2-Clause | 2 |
| BSD-3-Clause OR MIT | 2 |
| MIT OR Zlib OR Apache-2.0 | 2 |
| BSD-3-Clause OR Apache-2.0 | 2 |
| BSD-2-Clause OR Apache-2.0 OR MIT | 2 |
| 0BSD OR MIT OR Apache-2.0 | 1 |
| Apache-2.0 AND MIT | 1 |
| (Apache-2.0 OR MIT) AND BSD-3-Clause | 1 |
| Apache-2.0 AND ISC | 1 |
| Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | 1 |
| Apache-2.0 OR ISC OR MIT | 1 |
| CC0-1.0 | 1 |
| (MIT OR Apache-2.0) AND Unicode-3.0 | 1 |
| CDLA-Permissive-2.0 | 1 |

## All crates

Generated from `cargo metadata --filter-platform aarch64-apple-darwin` on 2026-11-12.
The Linux build adds crates that are not in this list yet (the X11 and Wayland
backends of winit and softbuffer, fontconfig bindings, arboard and their
dependencies); list them with `cargo metadata --filter-platform x86_64-unknown-linux-gnu`.
"build" marks crates that only build scripts use.

| crate | version | license | |
|---|---|---|---|
| adler2 | 2.0.1 | 0BSD OR MIT OR Apache-2.0 |  |
| aho-corasick | 1.1.5 | Unlicense OR MIT | build |
| alloc-no-stdlib | 2.0.4 | BSD-3-Clause |  |
| alloc-no-stdlib | 3.0.0 | BSD-3-Clause |  |
| alloc-stdlib | 0.2.4 | BSD-3-Clause |  |
| allocator-api2 | 0.2.21 | MIT OR Apache-2.0 |  |
| app_units | 0.7.8 | MPL-2.0 |  |
| arrayref | 0.3.9 | BSD-2-Clause |  |
| arrayvec | 0.7.8 | MIT OR Apache-2.0 |  |
| assert_matches | 1.5.0 | MIT/Apache-2.0 |  |
| atomic_refcell | 0.1.14 | Apache-2.0 OR MIT |  |
| atomig | 0.4.3 | MIT/Apache-2.0 |  |
| atomig-macro | 0.4.0 | MIT/Apache-2.0 |  |
| autocfg | 1.5.1 | Apache-2.0 OR MIT | build |
| avif-parse | 2.1.0 | MPL-2.0 |  |
| base64 | 0.23.1 | MIT OR Apache-2.0 |  |
| bindgen | 0.72.1 | BSD-3-Clause | build |
| bitflags | 1.3.2 | MIT/Apache-2.0 |  |
| bitflags | 2.13.2 | MIT OR Apache-2.0 |  |
| bitreader | 0.3.11 | MIT OR Apache-2.0 |  |
| block2 | 0.5.1 | MIT |  |
| brotli-decompressor | 5.0.3 | BSD-3-Clause/MIT |  |
| brotli-decompressor | 6.3.1 | BSD-3-Clause/MIT |  |
| bytemuck | 1.25.2 | Zlib OR Apache-2.0 OR MIT |  |
| bytemuck_derive | 1.12.1 | Zlib OR Apache-2.0 OR MIT |  |
| byteorder | 1.5.0 | Unlicense OR MIT |  |
| byteorder-lite | 0.1.0 | Unlicense OR MIT |  |
| bytes | 1.12.1 | MIT |  |
| calendrical_calculations | 0.2.4 | Apache-2.0 |  |
| cc | 1.11.1 | MIT OR Apache-2.0 | build |
| cexpr | 0.6.0 | Apache-2.0/MIT | build |
| cfg-if | 1.0.5 | MIT OR Apache-2.0 |  |
| cfg_aliases | 0.2.2 | MIT | build |
| clang-sys | 1.9.1 | Apache-2.0 | build |
| color | 0.3.3 | Apache-2.0 OR MIT |  |
| color_quant | 1.1.0 | MIT |  |
| core-foundation | 0.9.4 | MIT OR Apache-2.0 |  |
| core-foundation-sys | 0.8.7 | MIT OR Apache-2.0 |  |
| core-graphics | 0.23.2 | MIT OR Apache-2.0 |  |
| core-graphics-types | 0.1.3 | MIT OR Apache-2.0 |  |
| core_maths | 0.1.1 | MIT |  |
| crc32fast | 1.5.5 | MIT OR Apache-2.0 |  |
| crossbeam-deque | 0.8.8 | MIT OR Apache-2.0 |  |
| crossbeam-epoch | 0.9.21 | MIT OR Apache-2.0 |  |
| crossbeam-utils | 0.8.23 | MIT OR Apache-2.0 |  |
| cssparser | 0.38.0 | MPL-2.0 |  |
| cssparser-macros | 0.7.1 | MPL-2.0 |  |
| cursor-icon | 1.2.0 | MIT OR Apache-2.0 OR Zlib |  |
| darling | 0.20.11 | MIT |  |
| darling_core | 0.20.11 | MIT |  |
| darling_macro | 0.20.11 | MIT |  |
| data-url | 0.3.2 | MIT OR Apache-2.0 |  |
| derive_more | 2.1.1 | MIT |  |
| derive_more-impl | 2.1.1 | MIT |  |
| diplomat | 0.16.1 | MIT OR Apache-2.0 |  |
| diplomat-runtime | 0.16.1 | MIT OR Apache-2.0 |  |
| diplomat_core | 0.16.1 | MIT OR Apache-2.0 |  |
| dispatch | 0.2.0 | MIT |  |
| dispatch2 | 0.3.1 | Zlib OR Apache-2.0 OR MIT |  |
| displaydoc | 0.2.7 | MIT OR Apache-2.0 |  |
| dpi | 0.1.2 | Apache-2.0 AND MIT |  |
| dtoa | 1.0.11 | MIT OR Apache-2.0 |  |
| dtoa-short | 0.3.5 | MPL-2.0 |  |
| either | 1.19.0 | MIT OR Apache-2.0 |  |
| encoding_rs | 0.8.44 | (Apache-2.0 OR MIT) AND BSD-3-Clause |  |
| equivalent | 1.0.3 | Apache-2.0 OR MIT |  |
| errno | 0.3.15 | MIT OR Apache-2.0 | build |
| euclid | 0.22.14 | MIT OR Apache-2.0 |  |
| fallible_collections | 0.5.2 | MIT OR Apache-2.0 |  |
| fastrand | 2.5.0 | Apache-2.0 OR MIT |  |
| fdeflate | 0.3.7 | MIT OR Apache-2.0 |  |
| fearless_simd | 1.2.0 | Apache-2.0 OR MIT |  |
| fearless_simd_macros | 0.1.0 | Apache-2.0 OR MIT |  |
| find-msvc-tools | 0.1.15 | MIT OR Apache-2.0 | build |
| flate2 | 1.1.11 | MIT OR Apache-2.0 |  |
| float-cmp | 0.9.0 | MIT |  |
| fnv | 1.0.7 | Apache-2.0 / MIT |  |
| foldhash | 0.2.0 | Zlib |  |
| font-types | 0.12.7 | MIT OR Apache-2.0 |  |
| fontique | 0.12.0 | Apache-2.0 OR MIT |  |
| foreign-types | 0.5.0 | MIT/Apache-2.0 |  |
| foreign-types-macros | 0.2.4 | MIT/Apache-2.0 |  |
| foreign-types-shared | 0.3.1 | MIT/Apache-2.0 |  |
| form_urlencoded | 1.2.2 | MIT OR Apache-2.0 |  |
| fslock | 0.2.1 | MIT | build |
| getrandom | 0.2.17 | MIT OR Apache-2.0 |  |
| gif | 0.14.2 | MIT OR Apache-2.0 |  |
| glifo | 0.5.0 | Apache-2.0 OR MIT |  |
| glob | 0.3.4 | MIT OR Apache-2.0 | build |
| guillotiere | 0.7.0 | MIT/Apache-2.0 |  |
| gzip-header | 1.1.0 | MIT/Apache-2.0 | build |
| harfrust | 0.12.0 | MIT |  |
| hashbrown | 0.17.1 | MIT OR Apache-2.0 |  |
| heck | 0.5.0 | MIT OR Apache-2.0 |  |
| home | 0.5.12 | MIT OR Apache-2.0 | build |
| html5ever | 0.40.1 | MIT OR Apache-2.0 |  |
| http | 1.5.0 | MIT OR Apache-2.0 |  |
| httparse | 1.10.1 | MIT OR Apache-2.0 |  |
| icu_calendar | 2.4.0 | Unicode-3.0 |  |
| icu_calendar_data | 2.4.0 | Unicode-3.0 |  |
| icu_collections | 2.4.0 | Unicode-3.0 |  |
| icu_locale_core | 2.4.0 | Unicode-3.0 |  |
| icu_locale_fallback | 2.4.0 | Unicode-3.0 |  |
| icu_locale_fallback_data | 2.4.0 | Unicode-3.0 |  |
| icu_normalizer | 2.4.0 | Unicode-3.0 |  |
| icu_normalizer_data | 2.4.0 | Unicode-3.0 |  |
| icu_properties | 2.4.0 | Unicode-3.0 |  |
| icu_properties_data | 2.4.0 | Unicode-3.0 |  |
| icu_provider | 2.4.0 | Unicode-3.0 |  |
| icu_segmenter | 2.4.0 | Unicode-3.0 |  |
| icu_segmenter_data | 2.4.0 | Unicode-3.0 |  |
| ident_case | 1.0.1 | MIT/Apache-2.0 |  |
| idna | 1.1.0 | MIT OR Apache-2.0 |  |
| idna_adapter | 1.2.2 | Apache-2.0 OR MIT |  |
| image | 0.25.10 | MIT OR Apache-2.0 |  |
| image-webp | 0.2.4 | MIT OR Apache-2.0 |  |
| imagesize | 0.15.0 | MIT |  |
| indexmap | 2.14.2 | Apache-2.0 OR MIT |  |
| itertools | 0.13.0 | MIT OR Apache-2.0 | build |
| itertools | 0.14.0 | MIT OR Apache-2.0 |  |
| itoa | 1.0.19 | MIT OR Apache-2.0 |  |
| ixdtf | 0.6.7 | Unicode-3.0 |  |
| jobserver | 0.1.35 | MIT OR Apache-2.0 | build |
| kurbo | 0.13.1 | Apache-2.0 OR MIT |  |
| leb128 | 0.2.7 | MIT OR Apache-2.0 |  |
| libc | 0.2.191 | MIT OR Apache-2.0 |  |
| libloading | 0.8.9 | ISC | build |
| libm | 0.2.16 | MIT |  |
| linebender_resource_handle | 0.1.1 | Apache-2.0 OR MIT |  |
| litemap | 0.8.3 | Unicode-3.0 |  |
| lock_api | 0.4.14 | MIT OR Apache-2.0 |  |
| log | 0.4.35 | MIT OR Apache-2.0 |  |
| malloc_size_of_derive | 0.1.3 | MIT OR Apache-2.0 |  |
| markup5ever | 0.40.0 | MIT OR Apache-2.0 |  |
| memchr | 2.8.3 | Unlicense OR MIT |  |
| memmap2 | 0.9.12 | MIT OR Apache-2.0 |  |
| mime | 0.3.18 | MIT OR Apache-2.0 |  |
| minimal-lexical | 0.2.1 | MIT/Apache-2.0 | build |
| miniz_oxide | 0.8.9 | MIT OR Zlib OR Apache-2.0 |  |
| miniz_oxide | 0.9.1 | MIT OR Zlib OR Apache-2.0 |  |
| moxcms | 0.8.1 | BSD-3-Clause OR Apache-2.0 |  |
| multiversion_no_op | 1.0.0 | Apache-2.0 OR MIT |  |
| nasm-rs | 0.3.2 | MIT OR Apache-2.0 | build |
| new_debug_unreachable | 1.0.6 | MIT |  |
| nom | 7.1.3 | MIT | build |
| num-derive | 0.4.2 | MIT OR Apache-2.0 |  |
| num-integer | 0.1.47 | MIT OR Apache-2.0 |  |
| num-traits | 0.2.19 | MIT OR Apache-2.0 |  |
| num_cpus | 1.17.0 | MIT OR Apache-2.0 |  |
| objc-sys | 0.3.5 | MIT |  |
| objc2 | 0.5.3 | MIT |  |
| objc2 | 0.6.5 | MIT |  |
| objc2-app-kit | 0.2.2 | MIT |  |
| objc2-core-data | 0.2.2 | MIT |  |
| objc2-core-foundation | 0.3.2 | Zlib OR Apache-2.0 OR MIT |  |
| objc2-core-graphics | 0.3.2 | Zlib OR Apache-2.0 OR MIT |  |
| objc2-core-image | 0.2.2 | MIT |  |
| objc2-core-text | 0.3.2 | Zlib OR Apache-2.0 OR MIT |  |
| objc2-encode | 4.1.0 | MIT |  |
| objc2-foundation | 0.2.2 | MIT |  |
| objc2-foundation | 0.3.2 | MIT |  |
| objc2-io-surface | 0.3.2 | Zlib OR Apache-2.0 OR MIT |  |
| objc2-metal | 0.2.2 | MIT |  |
| objc2-quartz-core | 0.2.2 | MIT |  |
| objc2-quartz-core | 0.3.2 | Zlib OR Apache-2.0 OR MIT |  |
| once_cell | 1.21.4 | MIT OR Apache-2.0 |  |
| parking_lot | 0.12.5 | MIT OR Apache-2.0 |  |
| parking_lot_core | 0.9.12 | MIT OR Apache-2.0 |  |
| parlance | 0.1.1 | Apache-2.0 OR MIT |  |
| parley | 0.12.0 | Apache-2.0 OR MIT |  |
| parley_data | 0.12.0 | Apache-2.0 OR MIT |  |
| parley_emoji | 0.12.0 | MIT |  |
| parley_engine | 0.12.0 | Apache-2.0 OR MIT |  |
| paste | 1.0.15 | MIT OR Apache-2.0 |  |
| peniko | 0.6.1 | Apache-2.0 OR MIT |  |
| percent-encoding | 2.3.2 | MIT OR Apache-2.0 |  |
| phf | 0.14.0 | MIT |  |
| phf_codegen | 0.14.0 | MIT | build |
| phf_generator | 0.14.0 | MIT |  |
| phf_macros | 0.14.0 | MIT |  |
| phf_shared | 0.14.0 | MIT |  |
| pico-args | 0.5.0 | MIT |  |
| pin-project-lite | 0.2.17 | Apache-2.0 OR MIT |  |
| png | 0.18.1 | MIT OR Apache-2.0 |  |
| polycool | 0.4.0 | MIT OR Apache-2.0 |  |
| potential_utf | 0.1.6 | Unicode-3.0 |  |
| precomputed-hash | 0.1.1 | MIT |  |
| prettyplease | 0.2.37 | MIT OR Apache-2.0 | build |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 |  |
| pxfm | 0.1.30 | BSD-3-Clause OR Apache-2.0 |  |
| quick-error | 2.0.1 | MIT/Apache-2.0 |  |
| quote | 1.0.47 | MIT OR Apache-2.0 |  |
| rand | 0.8.8 | MIT OR Apache-2.0 |  |
| rand_core | 0.6.4 | MIT OR Apache-2.0 |  |
| rav1d | 1.1.0 | BSD-2-Clause |  |
| raw-cpuid | 11.6.0 | MIT |  |
| raw-window-handle | 0.6.2 | MIT OR Apache-2.0 OR Zlib |  |
| rayon | 1.12.0 | MIT OR Apache-2.0 |  |
| rayon-core | 1.13.0 | MIT OR Apache-2.0 |  |
| read-fonts | 0.41.0 | MIT OR Apache-2.0 |  |
| read-fonts | 0.46.0 | MIT OR Apache-2.0 |  |
| regex | 1.13.1 | MIT OR Apache-2.0 | build |
| regex-automata | 0.4.18 | MIT OR Apache-2.0 | build |
| regex-syntax | 0.8.11 | MIT OR Apache-2.0 | build |
| resb | 0.1.2 | Unicode-3.0 |  |
| resvg | 0.50.0 | Apache-2.0 OR MIT |  |
| rgb | 0.8.53 | MIT |  |
| ring | 0.17.14 | Apache-2.0 AND ISC |  |
| roxmltree | 0.21.2 | MIT OR Apache-2.0 |  |
| rustc-hash | 2.1.3 | Apache-2.0 OR MIT |  |
| rustc_version | 0.4.1 | MIT OR Apache-2.0 | build |
| rustix | 0.38.44 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | build |
| rustls | 0.23.46 | Apache-2.0 OR ISC OR MIT |  |
| rustls-pki-types | 1.16.0 | MIT OR Apache-2.0 |  |
| rustls-webpki | 0.103.16 | ISC |  |
| rustversion | 1.0.23 | MIT OR Apache-2.0 |  |
| same-file | 1.0.6 | Unlicense/MIT | build |
| scopeguard | 1.2.0 | MIT OR Apache-2.0 |  |
| selectors | 0.43.0 | MPL-2.0 |  |
| semver | 1.0.28 | MIT OR Apache-2.0 | build |
| serde | 1.0.229 | MIT OR Apache-2.0 |  |
| serde_core | 1.0.229 | MIT OR Apache-2.0 |  |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 |  |
| serde_json | 1.0.154 | MIT OR Apache-2.0 |  |
| serde_spanned | 1.1.3 | MIT OR Apache-2.0 | build |
| servo_arc | 0.6.0 | MIT OR Apache-2.0 |  |
| shlex | 1.3.0 | MIT OR Apache-2.0 | build |
| shlex | 2.0.1 | MIT OR Apache-2.0 | build |
| simd-adler32 | 0.3.10 | MIT |  |
| simdutf8 | 0.1.5 | MIT OR Apache-2.0 |  |
| simplecss | 0.2.2 | Apache-2.0 OR MIT |  |
| siphasher | 1.0.4 | MIT OR Apache-2.0 |  |
| skrifa | 0.44.0 | MIT OR Apache-2.0 |  |
| skrifa | 0.49.0 | MIT OR Apache-2.0 |  |
| slotmap | 1.1.1 | Zlib |  |
| smallbitvec | 2.6.1 | MIT OR Apache-2.0 |  |
| smallvec | 1.16.5 | MIT OR Apache-2.0 |  |
| smol_str | 0.2.2 | MIT OR Apache-2.0 |  |
| softbuffer | 0.4.8 | MIT OR Apache-2.0 |  |
| stable_deref_trait | 1.2.1 | MIT OR Apache-2.0 |  |
| static_assertions | 1.1.0 | MIT OR Apache-2.0 |  |
| strck | 1.0.0 | MIT |  |
| strict-num | 0.1.1 | MIT |  |
| string_cache | 0.11.0 | MIT OR Apache-2.0 |  |
| string_cache_codegen | 0.11.2 | MIT OR Apache-2.0 | build |
| strum | 0.26.3 | MIT |  |
| strum | 0.28.0 | MIT |  |
| strum_macros | 0.26.4 | MIT |  |
| strum_macros | 0.28.0 | MIT |  |
| stylo | 0.24.0 | MPL-2.0 |  |
| stylo_atoms | 0.24.0 | MPL-2.0 |  |
| stylo_derive | 0.24.0 | MPL-2.0 |  |
| stylo_dom | 0.24.0 | MPL-2.0 |  |
| stylo_malloc_size_of | 0.24.0 | MIT OR Apache-2.0 |  |
| stylo_static_prefs | 0.24.0 | MPL-2.0 |  |
| stylo_traits | 0.24.0 | MPL-2.0 |  |
| subtle | 2.6.1 | BSD-3-Clause |  |
| svgtypes | 0.16.2 | Apache-2.0 OR MIT |  |
| syn | 2.0.119 | MIT OR Apache-2.0 |  |
| syn | 3.0.10 | MIT OR Apache-2.0 |  |
| synstructure | 0.13.2 | MIT |  |
| synstructure | 0.14.0 | MIT |  |
| taffy | 0.15.1 | MIT |  |
| temporal_capi | 0.2.6 | MIT OR Apache-2.0 |  |
| temporal_rs | 0.2.6 | MIT OR Apache-2.0 |  |
| tendril | 0.5.1 | MIT OR Apache-2.0 |  |
| thin-vec | 0.2.21 | MIT OR Apache-2.0 |  |
| thiserror | 2.0.21 | MIT OR Apache-2.0 |  |
| thiserror-impl | 2.0.21 | MIT OR Apache-2.0 |  |
| timezone_provider | 0.2.6 | MIT OR Apache-2.0 |  |
| tiny-skia | 0.12.0 | BSD-3-Clause |  |
| tiny-skia-path | 0.12.0 | BSD-3-Clause |  |
| tinystr | 0.8.5 | Unicode-3.0 |  |
| to_method | 1.1.0 | CC0-1.0 |  |
| to_shmem | 0.8.0 | MPL-2.0 |  |
| to_shmem_derive | 0.3.0 | MPL-2.0 |  |
| toml | 1.2.1+spec-1.1.0 | MIT OR Apache-2.0 | build |
| toml_datetime | 1.1.3+spec-1.1.0 | MIT OR Apache-2.0 | build |
| toml_parser | 1.1.5+spec-1.1.0 | MIT OR Apache-2.0 | build |
| toml_writer | 1.2.0+spec-1.1.0 | MIT OR Apache-2.0 | build |
| tracing | 0.1.44 | MIT |  |
| tracing-core | 0.1.36 | MIT |  |
| uluru | 3.1.0 | MPL-2.0 |  |
| unicode-ident | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 |  |
| untrusted | 0.9.0 | ISC |  |
| ureq | 3.5.1 | MIT OR Apache-2.0 |  |
| ureq-proto | 0.7.1 | MIT OR Apache-2.0 |  |
| url | 2.5.8 | MIT OR Apache-2.0 |  |
| usvg | 0.50.0 | Apache-2.0 OR MIT |  |
| utf8-zero | 0.8.1 | MIT OR Apache-2.0 |  |
| utf8_iter | 1.1.0 | Apache-2.0 OR MIT |  |
| utf_types | 1.0.0 | Apache-2.0 OR MIT |  |
| v8 | 152.2.0 | MIT |  |
| vello_common | 0.4.0 | Apache-2.0 OR MIT |  |
| vello_cpu | 0.4.0 | Apache-2.0 OR MIT |  |
| version_check | 0.9.5 | MIT/Apache-2.0 | build |
| void | 1.0.2 | MIT |  |
| walkdir | 2.5.0 | Unlicense/MIT | build |
| web_atoms | 0.3.0 | MIT OR Apache-2.0 |  |
| webpki-roots | 1.0.12 | CDLA-Permissive-2.0 |  |
| weezl | 0.1.12 | MIT OR Apache-2.0 |  |
| which | 6.0.3 | MIT | build |
| winit | 0.30.13 | Apache-2.0 |  |
| winnow | 1.0.5 | MIT | build |
| writeable | 0.6.4 | Unicode-3.0 |  |
| wuff | 0.2.10 | MIT |  |
| yoke | 0.8.4 | Unicode-3.0 |  |
| yoke-derive | 0.8.4 | Unicode-3.0 |  |
| zerocopy | 0.7.35 | BSD-2-Clause OR Apache-2.0 OR MIT |  |
| zerocopy-derive | 0.7.35 | BSD-2-Clause OR Apache-2.0 OR MIT |  |
| zerofrom | 0.1.8 | Unicode-3.0 |  |
| zerofrom-derive | 0.1.8 | Unicode-3.0 |  |
| zeroize | 1.9.1 | Apache-2.0 OR MIT |  |
| zerotrie | 0.2.5 | Unicode-3.0 |  |
| zerovec | 0.11.9 | Unicode-3.0 |  |
| zerovec-derive | 0.11.7 | Unicode-3.0 |  |
| zlib-rs | 0.6.8 | Zlib |  |
| zmij | 1.0.23 | MIT |  |
| zoneinfo64 | 0.3.0 | Unicode-3.0 |  |
| zune-core | 0.5.3 | MIT OR Apache-2.0 OR Zlib |  |
| zune-jpeg | 0.5.15 | MIT OR Apache-2.0 OR Zlib |  |
