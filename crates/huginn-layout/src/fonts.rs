//! Font discovery for layout: a lazy font collection.
//!
//! Parley's default `FontContext` scans every system font file at startup
//! (40-50 ms, even for a page without text). Instead, Huginn keeps a small
//! on-disk index of system fonts (family name -> font files) and registers a
//! font file with the collection only when a style asks for its family.
//!
//! - Index: `~/Library/Caches/Huginn/fonts-v1.idx` (or `$HUGINN_CACHE_DIR`).
//!   It is rebuilt when a font directory's mtime changes. Building it costs one
//!   full system scan; loading it costs well under a millisecond.
//! - CSS generic families map to Chrome's macOS defaults (Times, Helvetica,
//!   Courier, ...), and lists without a generic family end in `serif`.
//! - Fallback fonts for symbols and non-Latin scripts load when the text
//!   contains such characters.
//!
//! On platforms other than macOS, the full system collection is used (on
//! Linux: fontconfig's fonts, generic families and fallbacks), and the
//! macOS-only parts of Blink's metrics (the ascent hack) are off. On Linux a
//! generic family is fontconfig's first few matches, and a character they
//! lack comes from the fallback list of its script (the vendored Fontique,
//! `vendor/fontique/HUGINN.md`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use huginn_style::{ComputedStyle, FontStyle, LineHeight};
use parley::fontique::{self, Collection, CollectionOptions, FamilyId, QueryFamily, Script};
use parley::{FontFamilyName, GenericFamily};

/// A resolved `font-family` list: names that exist, then a generic family.
pub(crate) type FamilyList = Arc<[FontFamilyName<'static>]>;

pub(crate) struct Fonts {
    pub ctx: parley::FontContext,
    /// `None`: the collection holds all system fonts (non-macOS, or no index).
    index: Option<FontIndex>,
    /// Per index file: already registered with the collection.
    loaded: Vec<bool>,
    /// CSS `font-family` source string -> resolved list.
    lists: HashMap<Arc<str>, FamilyList>,
    /// Generic families already pointed at a concrete family.
    generics: u32,
    /// Fallback classes ([`FALLBACKS`] indices) already installed.
    fallbacks: u64,
    /// Goes up whenever the fonts that text can be shaped with change: a
    /// font file or web font is registered, a generic family or a fallback
    /// is set. Text shaped at another epoch may be out of date.
    pub epoch: u64,
    /// (family list pointer, weight, style) -> vertical metrics of the
    /// list's first available font, for `line-height: normal`.
    vmetrics: HashMap<(usize, u32, u8), Option<VMetrics>>,
}

/// Vertical metrics of a font in font units, as Chrome reads them on macOS
/// (CoreText: `hhea` ascender, descender and line gap).
#[derive(Copy, Clone, Debug)]
struct VMetrics {
    upem: f32,
    ascent: f32,
    descent: f32,
    line_gap: f32,
    /// Times, Helvetica or Courier: Blink adds 15% to the ascent on macOS.
    ascent_hack: bool,
}

impl VMetrics {
    /// Ascent and descent at `size` px, each rounded to whole pixels as Blink
    /// does on macOS, and Blink's ascent hack (0 unless `ascent_hack`).
    fn blink_px(&self, size: f32) -> (f32, f32, f32) {
        let k = size / self.upem;
        let (ascent, descent) = ((self.ascent * k).round(), (self.descent * k).round());
        let hack = if self.ascent_hack {
            ((ascent + descent) * 0.15).round()
        } else {
            0.0
        };
        (ascent, descent, hack)
    }
}

struct FontIndex {
    files: Vec<PathBuf>,
    /// Lowercase family name -> file indices.
    families: HashMap<String, Vec<u32>>,
}

/// The CSS generic families as desktop Chrome resolves them on macOS
/// (chrome/app/resources/locale_settings_mac.grd: serif Times, sans-serif
/// Helvetica, monospace Menlo). We follow the browser users run, not
/// chrome-headless-shell, which has no font preferences and falls back to
/// Blink's built-in Times New Roman / Arial. First one present wins.
const GENERICS: &[(GenericFamily, &[&str])] = &[
    (GenericFamily::Serif, &["Times", "Times New Roman"]),
    (GenericFamily::SansSerif, &["Helvetica", "Arial"]),
    (
        GenericFamily::Monospace,
        &["Menlo", "Courier New", "Courier"],
    ),
    (GenericFamily::Cursive, &["Apple Chancery"]),
    (GenericFamily::Fantasy, &["Papyrus"]),
    (
        GenericFamily::SystemUi,
        &["System Font", ".SF NS", "Helvetica Neue"],
    ),
    (GenericFamily::UiSerif, &[".New York", "New York", "Times"]),
    (
        GenericFamily::UiSansSerif,
        &["System Font", ".SF NS", "Helvetica Neue"],
    ),
    (
        GenericFamily::UiMonospace,
        &[".SF NS Mono", "SF Mono", "Menlo"],
    ),
    (
        GenericFamily::UiRounded,
        &[".SF NS Rounded", "SF Pro Rounded", "System Font"],
    ),
    (GenericFamily::Emoji, &["Apple Color Emoji"]),
    (GenericFamily::Math, &["STIX Two Math"]),
    (GenericFamily::FangSong, &["STFangsong"]),
];

/// Common web fonts that systems other than macOS and Windows lack, and the
/// fonts with the same metrics that stand in for them (fontconfig's
/// `30-metric-aliases.conf`). First one installed wins.
const METRIC_ALIASES: &[(&[&str], &[&str])] = &[
    (
        &["Arial", "Helvetica", "Helvetica Neue"],
        &["Liberation Sans", "Arimo", "Nimbus Sans"],
    ),
    (
        &["Times New Roman", "Times"],
        &["Liberation Serif", "Tinos", "Nimbus Roman"],
    ),
    (
        &["Courier New", "Courier"],
        &["Liberation Mono", "Cousine", "Nimbus Mono PS"],
    ),
];

/// Color emoji fonts for the `emoji` generic family where the system
/// collection does not name one.
const SYSTEM_EMOJI: &[&str] = &["Noto Color Emoji", "Twemoji", "Noto Emoji"];

/// A class of characters that needs fallback fonts, the scripts (fallback
/// keys) it applies to, and the families to try, in order.
struct Fallback {
    scripts: &'static [[u8; 4]],
    families: &'static [&'static str],
}

const SYMBOLS: usize = 0;
const CJK: usize = 1;
const FALLBACKS: &[Fallback] = &[
    // Symbols, arrows, shapes, dingbats, IPA, combining marks.
    Fallback {
        scripts: &[*b"Latn", *b"Zyyy", *b"Zinh", *b"Zzzz", *b"Grek", *b"Cyrl"],
        families: &[
            "Lucida Grande",
            "Apple Symbols",
            "Menlo",
            "Geneva",
            "Arial Unicode MS",
        ],
    },
    Fallback {
        scripts: &[
            *b"Hani", *b"Hira", *b"Kana", *b"Hans", *b"Hant", *b"Jpan", *b"Bopo", *b"Zyyy",
            *b"Latn",
        ],
        families: &[
            "Hiragino Sans",
            "Hiragino Kaku Gothic ProN",
            "Heiti SC",
            "Arial Unicode MS",
        ],
    },
    Fallback {
        scripts: &[*b"Hang", *b"Kore"],
        families: &["Apple SD Gothic Neo", "Arial Unicode MS"],
    },
    Fallback {
        scripts: &[*b"Arab"],
        families: &["Geeza Pro", "Arial Unicode MS"],
    },
    Fallback {
        scripts: &[*b"Hebr"],
        families: &["Arial Hebrew", "Lucida Grande"],
    },
    Fallback {
        scripts: &[*b"Deva"],
        families: &["Kohinoor Devanagari", "Devanagari Sangam MN"],
    },
    Fallback {
        scripts: &[*b"Beng"],
        families: &["Kohinoor Bangla", "Bangla Sangam MN"],
    },
    Fallback {
        scripts: &[*b"Taml"],
        families: &["Tamil Sangam MN"],
    },
    Fallback {
        scripts: &[*b"Telu"],
        families: &["Kohinoor Telugu", "Telugu Sangam MN"],
    },
    Fallback {
        scripts: &[*b"Knda"],
        families: &["Kannada Sangam MN"],
    },
    Fallback {
        scripts: &[*b"Mlym"],
        families: &["Malayalam Sangam MN"],
    },
    Fallback {
        scripts: &[*b"Gujr"],
        families: &["Kohinoor Gujarati", "Gujarati Sangam MN"],
    },
    Fallback {
        scripts: &[*b"Guru"],
        families: &["Gurmukhi MN"],
    },
    Fallback {
        scripts: &[*b"Sinh"],
        families: &["Sinhala Sangam MN"],
    },
    Fallback {
        scripts: &[*b"Thai"],
        families: &["Thonburi"],
    },
    Fallback {
        scripts: &[*b"Laoo"],
        families: &["Lao Sangam MN"],
    },
    Fallback {
        scripts: &[*b"Khmr"],
        families: &["Khmer Sangam MN"],
    },
    Fallback {
        scripts: &[*b"Mymr"],
        families: &["Myanmar Sangam MN"],
    },
    Fallback {
        scripts: &[*b"Tibt"],
        families: &["Kokonor"],
    },
    Fallback {
        scripts: &[*b"Geor"],
        families: &["Helvetica", "Arial Unicode MS"],
    },
    Fallback {
        scripts: &[*b"Armn"],
        families: &["Mshtakan", "Arial Unicode MS"],
    },
    Fallback {
        scripts: &[*b"Ethi"],
        families: &["Kefa"],
    },
];

/// Fallback class of a non-ASCII character, if it needs one.
fn fallback_class(c: char) -> Option<usize> {
    let u = c as u32;
    Some(match u {
        // Latin-1, Latin Extended, Greek, Cyrillic, general punctuation and
        // currency: covered by the usual Latin fonts.
        0x80..=0x24F | 0x370..=0x52F | 0x1E00..=0x1FFF | 0x2000..=0x20CF => return None,
        0x590..=0x5FF => 4,
        0x600..=0x6FF | 0x750..=0x77F | 0x8A0..=0x8FF | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF => 3,
        0x900..=0x97F => 5,
        0x980..=0x9FF => 6,
        0xA00..=0xA7F => 12,
        0xA80..=0xAFF => 11,
        0xB80..=0xBFF => 7,
        0xC00..=0xC7F => 8,
        0xC80..=0xCFF => 9,
        0xD00..=0xD7F => 10,
        0xD80..=0xDFF => 13,
        0xE00..=0xE7F => 14,
        0xE80..=0xEFF => 15,
        0xF00..=0xFFF => 18,
        0x1000..=0x109F => 17,
        0x10A0..=0x10FF => 19,
        0x530..=0x58F => 20,
        0x1200..=0x139F => 21,
        0x1780..=0x17FF => 16,
        0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7AF => 2,
        0x2E80..=0x2FFF | 0x3000..=0x312F | 0x3190..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF => {
            CJK
        }
        0x20000..=0x3FFFF => CJK,
        _ => SYMBOLS,
    })
}

impl Fonts {
    pub fn new() -> Fonts {
        let index = if cfg!(target_os = "macos") {
            FontIndex::load_or_build()
        } else {
            None
        };
        let collection = Collection::new(CollectionOptions {
            shared: false,
            system_fonts: index.is_none(),
        });
        let loaded = vec![false; index.as_ref().map_or(0, |i| i.files.len())];
        let mut collection = collection;
        if index.is_none()
            && collection
                .generic_families(GenericFamily::Emoji)
                .next()
                .is_none()
            && let Some(id) = SYSTEM_EMOJI.iter().find_map(|n| collection.family_id(n))
        {
            collection.set_generic_families(GenericFamily::Emoji, std::iter::once(id));
        }
        Fonts {
            ctx: parley::FontContext {
                collection,
                source_cache: Default::default(),
            },
            index,
            loaded,
            lists: HashMap::new(),
            generics: 0,
            fallbacks: 0,
            epoch: 0,
            vmetrics: HashMap::new(),
        }
    }

    /// Parley line height for style `s` whose family list is `fam`, and the
    /// baseline shift (px, downwards) that Parley's placement needs to match
    /// Chrome's.
    ///
    /// `line-height: normal` follows Blink on macOS: the first available
    /// font's ascent, descent and line gap, each rounded to whole pixels.
    /// For Times, Helvetica and Courier Blink also adds 15% to the ascent;
    /// Parley places the baseline from the unmodified ascent, so text in
    /// those fonts gets shifted down by the difference.
    pub fn line_height(
        &mut self,
        s: &ComputedStyle,
        fam: &FamilyList,
    ) -> (parley::LineHeight, f32) {
        let m = self.vmetrics(s, fam).map(|m| {
            let (ascent, descent, hack) = m.blink_px(s.font_size);
            (
                ascent + descent,
                hack,
                (m.line_gap * s.font_size / m.upem).round(),
            )
        });
        let value = match (s.line_height, m) {
            (LineHeight::Number(n), _) => parley::LineHeight::FontSizeRelative(n),
            (LineHeight::Px(px), _) => parley::LineHeight::Absolute(px),
            (LineHeight::Normal, Some((content, hack, gap))) => {
                parley::LineHeight::Absolute(content + hack + gap)
            }
            (LineHeight::Normal, None) => parley::LineHeight::MetricsRelative(1.0),
        };
        // Blink puts half the leading above the hacked ascent, Parley above
        // the real one, so Chrome's baseline sits about hack/2 lower. Fitted
        // against Chrome on the corpus (Times 16/24/32px: 0/1/2 px).
        let shift = match m {
            Some((_, hack, _)) if hack > 0.0 => ((hack - 1.0) / 2.0).floor(),
            _ => 0.0,
        };
        (value, shift)
    }

    /// The span box of an inline element with style `s` (family list `fam`,
    /// Parley line height `lh` from [`Fonts::line_height`]): its extent
    /// (over, under) above and below its baseline, in px. This is how Parley
    /// (quantized) builds the box of a span from its first available font
    /// when the span has text on the line (CSS 2 §10.8.1).
    pub fn span_box(
        &mut self,
        s: &ComputedStyle,
        fam: &FamilyList,
        lh: parley::LineHeight,
    ) -> Option<(f32, f32)> {
        let m = self.vmetrics(s, fam)?;
        let (ascent, descent, _) = m.blink_px(s.font_size);
        let line_height = match lh {
            parley::LineHeight::Absolute(px) => px,
            parley::LineHeight::FontSizeRelative(n) => n * s.font_size,
            parley::LineHeight::MetricsRelative(n) => {
                (ascent + descent + (m.line_gap * s.font_size / m.upem).round()) * n
            }
        };
        let over = ascent + ((line_height - ascent - descent) / 2.0).floor();
        Some((over, line_height - over))
    }

    /// Blink's `FontMetrics::Ascent()` of the first available font: whole
    /// pixels, with the macOS ascent hack. List marker symbols use it.
    pub fn ascent(&mut self, s: &ComputedStyle, fam: &FamilyList) -> Option<f32> {
        let (ascent, _, hack) = self.vmetrics(s, fam)?.blink_px(s.font_size);
        Some(ascent + hack)
    }

    fn vmetrics(&mut self, s: &ComputedStyle, fam: &FamilyList) -> Option<VMetrics> {
        let style = match s.font_style {
            FontStyle::Normal => 0,
            FontStyle::Italic => 1,
            FontStyle::Oblique => 2,
        };
        let key = (
            Arc::as_ptr(fam) as *const () as usize,
            s.font_weight.to_bits(),
            style,
        );
        if let Some(m) = self.vmetrics.get(&key) {
            return *m;
        }
        let parley::FontContext {
            collection,
            source_cache,
        } = &mut self.ctx;
        let font = {
            let mut q = collection.query(source_cache);
            q.set_families(fam.iter().map(|f| match f {
                FontFamilyName::Named(n) => QueryFamily::Named(n),
                FontFamilyName::Generic(g) => QueryFamily::Generic(*g),
            }));
            q.set_attributes(fontique::Attributes {
                width: fontique::FontWidth::NORMAL,
                weight: fontique::FontWeight::new(s.font_weight),
                style: match s.font_style {
                    FontStyle::Normal => fontique::FontStyle::Normal,
                    FontStyle::Italic => fontique::FontStyle::Italic,
                    FontStyle::Oblique => fontique::FontStyle::Oblique(None),
                },
            });
            q.first_available_font()
        };
        let m = font.and_then(|f| {
            let name = collection.family_name(f.family.0).unwrap_or("");
            // Blink has this hack in its macOS font code only.
            let ascent_hack =
                cfg!(target_os = "macos") && matches!(name, "Times" | "Helvetica" | "Courier");
            read_vmetrics(f.blob.as_ref(), f.index, ascent_hack)
        });
        self.vmetrics.insert(key, m);
        m
    }

    /// Resolve a CSS `font-family` list, loading the fonts it names.
    pub fn family_list(&mut self, css: &Arc<str>) -> FamilyList {
        if let Some(l) = self.lists.get(css) {
            return l.clone();
        }
        let mut out: Vec<FontFamilyName<'static>> = Vec::new();
        let mut has_generic = false;
        for name in FontFamilyName::parse_css_list(css).filter_map(Result::ok) {
            let name = match name {
                FontFamilyName::Named(n)
                    if n.eq_ignore_ascii_case("-apple-system")
                        || n.eq_ignore_ascii_case("BlinkMacSystemFont") =>
                {
                    FontFamilyName::Generic(GenericFamily::SystemUi)
                }
                other => other.into_owned(),
            };
            match &name {
                FontFamilyName::Generic(g) => {
                    self.ensure_generic(*g);
                    has_generic = true;
                }
                FontFamilyName::Named(n) => {
                    if !self.ensure_family(n) {
                        // No index (not macOS): the usual metric-compatible
                        // stand-in for a common web font, as fontconfig does.
                        if self.index.is_none()
                            && let Some(alias) = self.metric_alias(n)
                        {
                            let alias = FontFamilyName::Named(alias.into());
                            if !out.contains(&alias) {
                                out.push(alias);
                            }
                        }
                        continue;
                    }
                }
            }
            if !out.contains(&name) {
                out.push(name);
            }
        }
        if !has_generic {
            // CSS: when no listed family is available, use the default font.
            self.ensure_generic(GenericFamily::Serif);
            out.push(FontFamilyName::Generic(GenericFamily::Serif));
        }
        let list: FamilyList = out.into();
        self.lists.insert(css.clone(), list.clone());
        list
    }

    /// Drop resolved lists (a web font was registered: names may resolve now).
    pub fn forget_lists(&mut self) {
        self.epoch += 1;
        self.lists.clear();
        self.vmetrics.clear();
    }

    /// Install fallback fonts for the characters in `text`.
    pub fn cover(&mut self, text: &str) {
        if text.is_ascii() || self.index.is_none() {
            return;
        }
        for c in text.chars() {
            if (c as u32) < 0x80 {
                continue;
            }
            if let Some(class) = fallback_class(c)
                && self.fallbacks & (1 << class) == 0
            {
                self.fallbacks |= 1 << class;
                self.epoch += 1;
                let fb = &FALLBACKS[class];
                let ids: Vec<FamilyId> = fb.families.iter().filter_map(|f| self.load(f)).collect();
                for s in fb.scripts {
                    self.ctx
                        .collection
                        .append_fallbacks(Script::from_bytes(*s), ids.iter().copied());
                }
                if class == SYMBOLS {
                    // Emoji clusters query the generic emoji family.
                    self.ensure_generic(GenericFamily::Emoji);
                }
            }
        }
    }

    /// Register a family's files with the collection. Returns false if the
    /// family exists nowhere.
    fn ensure_family(&mut self, name: &str) -> bool {
        self.load(name).is_some()
    }

    fn load(&mut self, name: &str) -> Option<FamilyId> {
        if let Some(id) = self.ctx.collection.family_id(name) {
            return Some(id);
        }
        let index = self.index.as_ref()?;
        let files = index.families.get(&name.to_lowercase())?;
        let paths: Vec<&Path> = files
            .iter()
            .filter(|&&f| !self.loaded[f as usize])
            .map(|&f| index.files[f as usize].as_path())
            .collect();
        for &f in files {
            self.loaded[f as usize] = true;
        }
        if !paths.is_empty() {
            self.ctx.collection.load_fonts_from_paths(paths);
            self.epoch += 1;
        }
        self.ctx.collection.family_id(name)
    }

    /// The first installed stand-in for the missing family `name`.
    fn metric_alias(&mut self, name: &str) -> Option<&'static str> {
        let (_, aliases) = METRIC_ALIASES
            .iter()
            .find(|(names, _)| names.iter().any(|n| n.eq_ignore_ascii_case(name)))?;
        aliases
            .iter()
            .copied()
            .find(|a| self.ctx.collection.family_id(a).is_some())
    }

    fn ensure_generic(&mut self, g: GenericFamily) {
        let bit = 1u32 << (g as u32);
        if self.index.is_none() || self.generics & bit != 0 {
            return;
        }
        self.generics |= bit;
        let Some((_, names)) = GENERICS.iter().find(|(k, _)| *k == g) else {
            return;
        };
        if let Some(id) = names.iter().find_map(|n| self.load(n)) {
            self.ctx
                .collection
                .set_generic_families(g, std::iter::once(id));
            self.epoch += 1;
        }
    }
}

/// `hhea` (or, when that is empty, OS/2 typo) vertical metrics of font
/// `index` in the font file `data` (TTF, OTF or TTC).
fn read_vmetrics(data: &[u8], index: u32, ascent_hack: bool) -> Option<VMetrics> {
    let u16_at = |o: usize| data.get(o..o + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    let i16_at = |o: usize| u16_at(o).map(|v| v as i16 as f32);
    let u32_at = |o: usize| {
        data.get(o..o + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let base = if data.get(0..4)? == b"ttcf" {
        u32_at(12 + 4 * index as usize)? as usize
    } else {
        0
    };
    let tables = u16_at(base + 4)? as usize;
    let table = |tag: &[u8; 4]| {
        (0..tables).find_map(|i| {
            let rec = base + 12 + 16 * i;
            (data.get(rec..rec + 4)? == tag)
                .then(|| u32_at(rec + 8))
                .flatten()
        })
    };
    let upem = u16_at(table(b"head")? as usize + 18)? as f32;
    let hhea = table(b"hhea")? as usize;
    let (mut ascent, mut descent, mut line_gap) =
        (i16_at(hhea + 4)?, -i16_at(hhea + 6)?, i16_at(hhea + 8)?);
    if ascent == 0.0
        && descent == 0.0
        && let Some(os2) = table(b"OS/2").map(|o| o as usize)
    {
        ascent = i16_at(os2 + 68)?;
        descent = -i16_at(os2 + 70)?;
        line_gap = i16_at(os2 + 72)?;
    }
    (upem > 0.0).then_some(VMetrics {
        upem,
        ascent,
        descent: descent.abs(),
        line_gap: line_gap.max(0.0),
        ascent_hack,
    })
}

// ----- the on-disk index ------------------------------------------------------

const MAGIC: &str = "huginn-fonts 1";

fn cache_path() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("HUGINN_CACHE_DIR") {
        return Some(PathBuf::from(d).join("fonts-v1.idx"));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    if cfg!(target_os = "macos") {
        return Some(home.join("Library/Caches/Huginn/fonts-v1.idx"));
    }
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
        .unwrap_or(home.join(".cache"));
    Some(cache.join("huginn/fonts-v1.idx"))
}

/// Directories whose mtime invalidates the index (fonts added or removed).
fn watched_dirs(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = [
        "/System/Library/Fonts",
        "/System/Library/Fonts/Supplemental",
        "/Library/Fonts",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("Library/Fonts"));
    }
    for f in files {
        if let Some(p) = f.parent()
            && !dirs.iter().any(|d| d == p)
        {
            dirs.push(p.to_path_buf());
        }
    }
    dirs
}

fn mtime(p: &Path) -> String {
    match std::fs::metadata(p).and_then(|m| m.modified()) {
        Ok(t) => match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => format!("{}.{:09}", d.as_secs(), d.subsec_nanos()),
            Err(_) => "0".into(),
        },
        Err(_) => "-".into(),
    }
}

impl FontIndex {
    fn load_or_build() -> Option<FontIndex> {
        let path = cache_path()?;
        if let Some(i) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| FontIndex::parse(&s))
        {
            return Some(i);
        }
        let index = FontIndex::scan();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
            let tmp = path.with_extension(format!("tmp{}", std::process::id()));
            if std::fs::write(&tmp, index.serialize()).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
        Some(index)
    }

    /// Parse the cache file; `None` if it is stale or malformed.
    fn parse(s: &str) -> Option<FontIndex> {
        let mut lines = s.lines();
        if lines.next()? != MAGIC {
            return None;
        }
        let mut files = Vec::new();
        let mut families: HashMap<String, Vec<u32>> = HashMap::new();
        for line in lines {
            let (tag, rest) = line.split_once(' ')?;
            match tag {
                "D" => {
                    let (stamp, dir) = rest.split_once('\t')?;
                    if mtime(Path::new(dir)) != stamp {
                        return None;
                    }
                }
                "F" => files.push(PathBuf::from(rest)),
                "N" => {
                    let (name, ids) = rest.split_once('\t')?;
                    let ids: Vec<u32> = ids.split(' ').filter_map(|i| i.parse().ok()).collect();
                    if ids.iter().any(|&i| i as usize >= files.len()) {
                        return None;
                    }
                    families.insert(name.to_lowercase(), ids);
                }
                _ => return None,
            }
        }
        Some(FontIndex { files, families })
    }

    fn serialize(&self) -> String {
        let mut s = String::new();
        s.push_str(MAGIC);
        s.push('\n');
        for d in watched_dirs(&self.files) {
            s.push_str(&format!("D {}\t{}\n", mtime(&d), d.display()));
        }
        for f in &self.files {
            s.push_str(&format!("F {}\n", f.display()));
        }
        let mut names: Vec<(&String, &Vec<u32>)> = self.families.iter().collect();
        names.sort();
        for (name, ids) in names {
            let ids: Vec<String> = ids.iter().map(u32::to_string).collect();
            s.push_str(&format!("N {name}\t{}\n", ids.join(" ")));
        }
        s
    }

    /// Full scan: the system collection lists the font files; a second,
    /// path-based collection names them exactly the way lazy loading will.
    fn scan() -> FontIndex {
        let mut paths: Vec<PathBuf> = Vec::new();
        {
            let mut sys = Collection::new(CollectionOptions {
                shared: false,
                system_fonts: true,
            });
            let ids: Vec<FamilyId> = sys.family_ids().collect();
            for id in ids {
                let Some(fam) = sys.family(id) else { continue };
                for font in fam.fonts() {
                    if let fontique::SourceKind::Path(p) = font.source().kind() {
                        paths.push(p.to_path_buf());
                    }
                }
            }
        }
        paths.sort();
        paths.dedup();
        let mut named = Collection::new(CollectionOptions {
            shared: false,
            system_fonts: false,
        });
        named.load_fonts_from_paths(&paths);
        let mut families: HashMap<String, Vec<u32>> = HashMap::new();
        let ids: Vec<FamilyId> = named.family_ids().collect();
        for id in ids {
            let Some(name) = named.family_name(id).map(str::to_lowercase) else {
                continue;
            };
            let Some(fam) = named.family(id) else {
                continue;
            };
            let entry = families.entry(name).or_default();
            for font in fam.fonts() {
                if let fontique::SourceKind::Path(p) = font.source().kind()
                    && let Ok(i) = paths.binary_search_by(|x| x.as_path().cmp(p))
                    && !entry.contains(&(i as u32))
                {
                    entry.push(i as u32);
                }
            }
        }
        FontIndex {
            files: paths,
            families,
        }
    }
}
