//! The Stylo side of [`StyleEngine`](crate::StyleEngine): device, stylist,
//! stylesheet parsing (with `@import`), the style traversal and
//! `@font-face` extraction.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc as StdArc, Once};

use huginn_dom::{Document, NodeId, local_name};
use style::author_styles::AuthorStyles;
use style::context::{
    QuirksMode, RegisteredSpeculativePainter, RegisteredSpeculativePainters, SharedStyleContext,
    StyleContext,
};
use style::device::Device;
use style::device::servo::{FontMetricsProvider, MediaData};
use style::dom::{TElement, TNode};
use style::font_metrics::FontMetrics;
use style::global_style_data::GLOBAL_STYLE_DATA;
use style::media_queries::{MediaList, MediaType};
use style::properties::style_structs::Font;
use style::properties::{ComputedValues, PropertyDeclarationBlock, parse_style_attribute};
use style::queries::values::PrefersColorScheme;
use style::selector_parser::SnapshotMap;
use style::servo::animation::DocumentAnimationSet;
use style::servo::media_features::PointerCapabilities;
use style::servo_arc::Arc;
use style::shared_lock::{Locked, SharedRwLock, StylesheetGuards};
use style::stylesheets::DocumentStyleSheet;
use style::stylesheets::import_rule::{ImportLayer, ImportSheet, ImportSupportsCondition};
use style::stylesheets::{
    AllowImportRules, CssRule, CssRuleType, CustomMediaMap, ImportRule, Origin as StyloOrigin,
    Stylesheet, StylesheetInDocument, UrlExtraData,
};
use style::stylist::{CascadeData, Stylist};
use style::thread_state::{self, ThreadState};
use style::traversal::{DomTraversal, recalc_style_at};
use style::traversal_flags::TraversalFlags;
use style::values::CssUrl;
use style::values::computed::font::{GenericFontFamily, QueryFontMetricsFlags};
use style::values::computed::{CSSPixelLength, Length};
use style::{Atom, font_face};

use crate::dom::{DomCx, ElemSide, HuginnNode};
use crate::{FontFaceRule, FontFaceSource, FontStyle, Origin, StylesheetLoader, Viewport, hints};

/// Max `@import` nesting.
const MAX_IMPORT_DEPTH: u32 = 4;

/// Turn on the Stylo features Huginn renders (or at least parses so that
/// the cascade matches Chrome). Must run before the first `Stylist`.
pub(crate) fn init_stylo() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        use style_config::set_pref;
        set_pref!("layout.css.has-selector.enabled", true);
        set_pref!("layout.css.nth-child-of.enabled", true);
        set_pref!("layout.css.text-overflow.enabled", true);
        set_pref!("layout.css.counters.enabled", true);
        set_pref!("layout.css.line-clamp.enabled", true);
        set_pref!("layout.css.columns.enabled", true);
        set_pref!("layout.css.contain.enabled", true);
        set_pref!("layout.css.masking.enabled", true);
        set_pref!("layout.css.backdrop-filter.enabled", true);
        set_pref!("layout.css.at-scope.enabled", true);
        set_pref!("layout.css.font-variations.enabled", true);
        set_pref!("layout.css.writing-mode.enabled", true);
        set_pref!("layout.css.appearance.enabled", true);
        set_pref!("layout.css.custom-media.enabled", true);
        set_pref!("layout.css.webkit-text-fill-color.enabled", true);
        set_pref!("layout.css.text-wrap-style.balance.enabled", true);
        set_pref!("layout.css.text-wrap-style.pretty.enabled", true);
        set_pref!("layout.css.scrollbar-styling.enabled", true);
        set_pref!("layout.css.tree-counting-functions.enabled", true);
        set_pref!("layout.css.basic-shape-shape.enabled", true);
        set_pref!("layout.css.word-break.break-word.enabled", true);
        set_pref!("layout.css.hyphens.enabled", true);
        // `content: url(x) / "alt"` (Chrome 77+); without it the whole
        // declaration is dropped.
        set_pref!("layout.css.content.alt-text.enabled", true);
    });
}

/// Font metrics without a font context: Stylo uses these for `ex`, `ch`,
/// `cap` and `ic`. The ratios are typical of Latin system fonts.
#[derive(Debug)]
struct ApproxMetrics;

impl FontMetricsProvider for ApproxMetrics {
    fn query_font_metrics(
        &self,
        _vertical: bool,
        _font: &Font,
        base_size: CSSPixelLength,
        _flags: QueryFontMetricsFlags,
    ) -> FontMetrics {
        let px = base_size.px();
        FontMetrics {
            x_height: Some(Length::new(px * 0.5)),
            zero_advance_measure: Some(Length::new(px * 0.55)),
            cap_height: Some(Length::new(px * 0.7)),
            ic_width: Some(Length::new(px)),
            ascent: Length::new(px * 0.9),
            script_percent_scale_down: None,
            script_script_percent_scale_down: None,
        }
    }

    /// Chrome's default sizes: 13px for monospace, 16px for the rest.
    fn base_size_for_generic(&self, generic: GenericFontFamily) -> Length {
        match generic {
            GenericFontFamily::Monospace => Length::new(13.0),
            _ => Length::new(16.0),
        }
    }
}

pub(crate) fn make_device(vp: Viewport) -> Device {
    let dpr = if vp.device_pixel_ratio > 0.0 {
        vp.device_pixel_ratio
    } else {
        1.0
    };
    let media_data = MediaData {
        media_type: MediaType::screen(),
        viewport_size: euclid::Size2D::new(vp.width, vp.height),
        device_size: euclid::Size2D::new(vp.width * dpr, vp.height * dpr),
        device_pixel_ratio: euclid::Scale::new(dpr),
        quirks_mode: QuirksMode::NoQuirks,
        prefers_color_scheme: PrefersColorScheme::Light,
        primary_pointer_capabilities: PointerCapabilities::FINE | PointerCapabilities::HOVER,
        all_pointer_capabilities: PointerCapabilities::FINE | PointerCapabilities::HOVER,
    };
    Device::new(
        ComputedValues::initial_values_with_font_override(Font::initial_values()),
        Box::new(ApproxMetrics),
        "Submit".into(),
        media_data,
    )
}

fn stylo_origin(origin: Origin) -> StyloOrigin {
    match origin {
        Origin::UserAgent => StyloOrigin::UserAgent,
        Origin::User => StyloOrigin::User,
        Origin::Author => StyloOrigin::Author,
    }
}

pub(crate) fn url_data(url: &str) -> UrlExtraData {
    let url =
        url::Url::parse(url).unwrap_or_else(|_| url::Url::parse("about:blank").expect("valid URL"));
    UrlExtraData(Arc::new(url))
}

/// Parse a stylesheet. `@import`s are fetched synchronously through `loader`.
pub(crate) fn parse_sheet(
    css: &str,
    base_url: &str,
    origin: Origin,
    lock: &SharedRwLock,
    loader: &dyn StylesheetLoader,
    quirks: QuirksMode,
    depth: u32,
) -> Arc<Stylesheet> {
    parse_sheet_with_media(
        css,
        base_url,
        origin,
        lock,
        loader,
        quirks,
        depth,
        Arc::new(lock.wrap(MediaList::empty())),
    )
}

/// Chrome parses `-webkit-line-clamp` and `-webkit-backdrop-filter` as
/// other names of `line-clamp` and `backdrop-filter`. Stylo's servo build
/// has neither name (Gecko-only), so declarations with them are renamed
/// before parsing. Only a property name is renamed: the name must start a
/// declaration (after `{`, `;` or white space) and be followed by `:`.
pub(crate) fn webkit_aliases(css: &str) -> Cow<'_, str> {
    const ALIASES: [(&str, &str); 2] = [
        ("-webkit-line-clamp", "line-clamp"),
        ("-webkit-backdrop-filter", "backdrop-filter"),
    ];
    if !ALIASES.iter().any(|(from, _)| css.contains(from)) {
        return Cow::Borrowed(css);
    }
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    'scan: while let Some(at) = rest.find("-webkit-") {
        for (from, to) in ALIASES {
            let Some(after) = rest[at..].strip_prefix(from) else {
                continue;
            };
            let starts = rest[..at]
                .chars()
                .next_back()
                .is_none_or(|c| c == '{' || c == ';' || c.is_whitespace());
            if starts && after.trim_start().starts_with(':') {
                out.push_str(&rest[..at]);
                out.push_str(to);
                rest = after;
                continue 'scan;
            }
        }
        let skip = at + "-webkit-".len();
        out.push_str(&rest[..skip]);
        rest = &rest[skip..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}
#[allow(clippy::too_many_arguments)]
fn parse_sheet_with_media(
    css: &str,
    base_url: &str,
    origin: Origin,
    lock: &SharedRwLock,
    loader: &dyn StylesheetLoader,
    quirks: QuirksMode,
    depth: u32,
    media: Arc<Locked<MediaList>>,
) -> Arc<Stylesheet> {
    let imports = ImportLoader {
        loader,
        origin,
        depth,
        quirks,
    };
    Arc::new(Stylesheet::from_str(
        &webkit_aliases(css),
        url_data(base_url),
        stylo_origin(origin),
        media,
        lock.clone(),
        Some(&imports),
        None,
        quirks,
        if depth < MAX_IMPORT_DEPTH {
            AllowImportRules::Yes
        } else {
            AllowImportRules::No
        },
    ))
}

/// Adapts our [`StylesheetLoader`] to Stylo's import loader.
struct ImportLoader<'a> {
    loader: &'a dyn StylesheetLoader,
    origin: Origin,
    depth: u32,
    quirks: QuirksMode,
}

impl style::stylesheets::StylesheetLoader for ImportLoader<'_> {
    fn request_stylesheet(
        &self,
        url: CssUrl,
        location: cssparser::SourceLocation,
        lock: &SharedRwLock,
        media: Arc<Locked<MediaList>>,
        supports: Option<ImportSupportsCondition>,
        layer: ImportLayer,
    ) -> Arc<Locked<ImportRule>> {
        let enabled = supports.as_ref().is_none_or(|s| s.enabled);
        let loaded = (enabled && self.depth + 1 < MAX_IMPORT_DEPTH)
            .then(|| url.url().map(|u| u.as_str().to_owned()))
            .flatten()
            .and_then(|u| self.loader.load(&u));
        let stylesheet = match loaded {
            Some((css, final_url)) => {
                // The @import's media list applies to the imported sheet.
                ImportSheet::new(parse_sheet_with_media(
                    &css,
                    &final_url,
                    self.origin,
                    lock,
                    self.loader,
                    self.quirks,
                    self.depth + 1,
                    media,
                ))
            }
            None => ImportSheet::new_refused(),
        };
        Arc::new(lock.wrap(ImportRule {
            url,
            stylesheet,
            supports,
            layer,
            source_location: location,
        }))
    }
}

/// `@font-face` rules of `sheet` that apply on `device` (including nested
/// `@media`/`@supports`/`@layer` and imported sheets).
pub(crate) fn font_faces(
    sheet: &Stylesheet,
    device: &Device,
    lock: &SharedRwLock,
) -> Vec<FontFaceRule> {
    let guard = lock.read();
    let custom_media = CustomMediaMap::default();
    let mut out = Vec::new();
    for rule in sheet
        .contents(&guard)
        .effective_rules(device, &custom_media, &guard)
    {
        let CssRule::FontFace(ff) = rule else {
            continue;
        };
        let d = &ff.read_with(&guard).descriptors;
        let (Some(family), Some(src)) = (&d.font_family, &d.src) else {
            continue;
        };
        let sources = src
            .0
            .iter()
            .filter_map(|s| match s {
                font_face::Source::Url(u) => Some(FontFaceSource {
                    url: u.url.url()?.as_str().to_owned(),
                    format: u.format_hint.as_ref().map(|f| match f {
                        font_face::FontFaceSourceFormat::String(s) => s.to_ascii_lowercase(),
                        font_face::FontFaceSourceFormat::Keyword(k) => {
                            style_traits::ToCss::to_css_string(k).to_ascii_lowercase()
                        }
                    }),
                }),
                font_face::Source::Local(_) => None,
            })
            .collect();
        let weight = d
            .font_weight
            .as_ref()
            .and_then(|w| w.compute())
            .map(|w| (w.0.value(), w.1.value()))
            .unwrap_or((400.0, 400.0));
        let style = match &d.font_style {
            Some(font_face::FontStyleRange::Italic) => FontStyle::Italic,
            Some(font_face::FontStyleRange::Oblique(a, _))
                if a.degrees().is_some_and(|d| d != 0.0) =>
            {
                FontStyle::Oblique
            }
            _ => FontStyle::Normal,
        };
        let unicode_range = d
            .unicode_range
            .as_ref()
            .map(|r| r.iter().map(|r| (r.start, r.end)).collect())
            .unwrap_or_default();
        out.push(FontFaceRule {
            family: StdArc::from(&*family.name),
            sources,
            weight,
            style,
            unicode_range,
            size_adjust: d
                .size_adjust
                .as_ref()
                .and_then(|p| p.0.get())
                .unwrap_or(1.0),
        });
    }
    out
}

/// Interned declaration blocks, so identical `style` attributes and hints
/// share one `Arc` (Stylo's sharing cache then shares their styles too).
#[derive(Default)]
struct BlockCache(HashMap<String, Arc<Locked<PropertyDeclarationBlock>>>);

impl BlockCache {
    fn get(
        &mut self,
        css: &str,
        lock: &SharedRwLock,
        url: &UrlExtraData,
        quirks: QuirksMode,
    ) -> Arc<Locked<PropertyDeclarationBlock>> {
        if let Some(b) = self.0.get(css) {
            return b.clone();
        }
        let block = Arc::new(lock.wrap(parse_style_attribute(
            &webkit_aliases(css),
            url,
            None,
            quirks,
            CssRuleType::Style,
        )));
        self.0.insert(css.to_owned(), block.clone());
        block
    }
}

/// Build the per-element side table for a style pass.
pub(crate) fn build_cx<'a>(
    doc: &'a Document,
    lock: &SharedRwLock,
    doc_url: &UrlExtraData,
) -> Box<DomCx<'a>> {
    let quirks = match doc.quirks_mode() {
        huginn_dom::QuirksMode::Quirks => QuirksMode::Quirks,
        huginn_dom::QuirksMode::LimitedQuirks => QuirksMode::LimitedQuirks,
        huginn_dom::QuirksMode::NoQuirks => QuirksMode::NoQuirks,
    };
    let mut styles = BlockCache::default();
    let mut hint_blocks = BlockCache::default();
    DomCx::with_side(doc, lock.clone(), |id| {
        let el = doc.element(id).expect("side entries are elements");
        let id_atom = el.id().filter(|s| !s.is_empty()).map(Atom::from);
        let classes: Box<[Atom]> = match el.attr(&local_name!("class")) {
            Some(c) if !c.is_empty() => c.split_ascii_whitespace().map(Atom::from).collect(),
            _ => Box::default(),
        };
        let style_attr = el
            .attr(&local_name!("style"))
            .filter(|s| !s.trim().is_empty())
            .map(|s| styles.get(s, lock, doc_url, quirks));
        let hints =
            hints::hint_css(doc, id).map(|css| hint_blocks.get(&css, lock, doc_url, quirks));
        ElemSide::new(id_atom, classes, style_attr, hints)
    })
}

/// The author rules of every connected shadow root: its `<style>` elements
/// in tree order, then its `adoptedStyleSheets` (CSS Scoping 3: they apply
/// only inside that shadow tree, plus `:host` and `::slotted`). `sheets`
/// caches parsed sheets by text. `<link rel=stylesheet>` in shadow trees is
/// not loaded.
pub(crate) fn shadow_cascade_data(
    stylist: &mut Stylist,
    doc: &Document,
    lock: &SharedRwLock,
    base_url: &str,
    sheets: &mut HashMap<Box<str>, DocumentStyleSheet>,
) -> HashMap<NodeId, Arc<CascadeData>> {
    let guard = lock.read();
    let quirks = stylist.quirks_mode();
    let mut out = HashMap::new();
    for root in doc.shadow_including_descendants(NodeId::DOCUMENT) {
        if doc.shadow_host(root).is_none() {
            continue;
        }
        let texts = doc
            .descendants(root)
            .filter(|&n| {
                doc.element(n)
                    .is_some_and(|e| e.is_html(&local_name!("style")))
            })
            .map(|n| doc.text_content(n))
            .chain(doc.adopted_css(root).map(str::to_owned));
        let mut styles = AuthorStyles::<DocumentStyleSheet>::new();
        let mut any = false;
        for css in texts {
            let sheet = sheets
                .entry(css.into_boxed_str())
                .or_insert_with_key(|css| {
                    DocumentStyleSheet(parse_sheet(
                        css,
                        base_url,
                        Origin::Author,
                        lock,
                        &crate::NoImports,
                        quirks,
                        0,
                    ))
                })
                .clone();
            styles.stylesheets.append_stylesheet(sheet, &guard);
            any = true;
        }
        if any {
            styles.flush(stylist, &guard);
            out.insert(root, styles.data.clone());
        }
    }
    out
}

struct NoPainters;

impl RegisteredSpeculativePainters for NoPainters {
    fn get(&self, _name: &Atom) -> Option<&dyn RegisteredSpeculativePainter> {
        None
    }
}

/// The style traversal: match and cascade every element in pre-order.
struct Recalc<'a> {
    shared: SharedStyleContext<'a>,
}

impl<'a, 'dom> DomTraversal<HuginnNode<'dom>> for Recalc<'a> {
    fn process_preorder<F>(
        &self,
        context: &mut StyleContext<HuginnNode<'dom>>,
        node: HuginnNode<'dom>,
        note_child: F,
    ) where
        F: FnMut(HuginnNode<'dom>),
    {
        let Some(el) = node.as_element() else { return };
        // SAFETY: the sequential traversal has exclusive access to the data.
        let mut data = unsafe { el.ensure_data() };
        recalc_style_at(self, context, el, &mut data, note_child);
        // SAFETY: as above.
        unsafe { el.unset_dirty_descendants() };
    }

    fn needs_postorder_traversal() -> bool {
        false
    }

    fn process_postorder(
        &self,
        _context: &mut StyleContext<HuginnNode<'dom>>,
        _node: HuginnNode<'dom>,
    ) {
        unreachable!("no postorder traversal")
    }

    fn shared_context(&self) -> &SharedStyleContext<'_> {
        &self.shared
    }
}

/// Style every element of `cx.doc` (sequentially).
pub(crate) fn traverse(stylist: &Stylist, cx: &DomCx<'_>, lock: &SharedRwLock) {
    let Some(root) = cx.doc.document_element() else {
        return;
    };
    let guard = lock.read();
    let guards = StylesheetGuards::same(&guard);
    let snapshots = SnapshotMap::new();
    let shared = SharedStyleContext {
        stylist,
        visited_styles_enabled: false,
        options: GLOBAL_STYLE_DATA.options.clone(),
        guards,
        current_time_for_animations: 0.0,
        traversal_flags: TraversalFlags::empty(),
        snapshot_map: &snapshots,
        animations: DocumentAnimationSet::default(),
        registered_speculative_painters: &NoPainters,
    };
    let root = cx.node(root);
    let token = Recalc::pre_traverse(root, &shared);
    if !token.should_traverse() {
        return;
    }
    let traversal = Recalc { shared };
    thread_state::enter(ThreadState::LAYOUT);
    style::driver::traverse_dom(&traversal, token, None);
    thread_state::exit(ThreadState::LAYOUT);
}

/// The Stylo data of an element after `traverse`.
pub(crate) fn data_of<'c>(
    cx: &'c DomCx<'_>,
    id: NodeId,
) -> Option<style::data::ElementDataRef<'c>> {
    cx.side(id)?.data()
}
