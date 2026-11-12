//! Subresources of a page: which images, inline SVGs and web fonts it needs,
//! fetched and decoded on the loader's worker threads. Bodies are fetched
//! without the cache and dropped as soon as they are decoded.
//!
//! The pipeline calls three functions, in this order:
//! 1. [`plan_images`] after parsing: the `<img>` choice per element
//!    (`srcset`/`sizes`/`<picture>` for the screen), split into eager and
//!    `loading=lazy`, and the `<video poster>` images (always eager).
//! 2. [`load`] after style: one parallel batch with the eager images (skipped
//!    when `display:none`, decoded at their CSS/attribute size), CSS
//!    background images, inline `<svg>` elements (serialized and rasterized at
//!    their CSS size) and the `@font-face` files that CSS font matching picks
//!    for the text on the page (only `unicode-range` subsets that cover it).
//! 3. [`load_lazy`] after layout: lazy images whose box is in the painted
//!    region. They are patched into the layout tree, or the caller lays out
//!    again when the image changes the box size.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use huginn_dom::{Document, ElementData, NodeData, NodeId, local_name, ns};
use huginn_layout::{BoxKind, LayoutTree, Rect};
use huginn_net::{DecodedImage, ImageStore, Loader, NetError, ResourceKind, Screen, Url};
use huginn_style::{
    BackgroundImage, Color, ComputedStyle, Content, ContentItem, FontFaceRule, FontStyle,
    LengthPercentageAuto, PseudoElement, StyleMap,
};

/// The image chosen for one `<img>` element, or the poster of a `<video>`.
pub struct ImgChoice {
    node: NodeId,
    url: Url,
    /// Pixel density of the chosen candidate: natural CSS size = pixels / density.
    density: f32,
    /// Width the `sizes` attribute gives the image slot, in CSS px.
    slot_width: Option<f32>,
    /// Layout finds plain `src` images by URL; other choices are stored per element.
    by_url: bool,
}

/// `<img>` choices, then `<video>` posters, in document order.
pub struct ImagePlan {
    pub eager: Vec<ImgChoice>,
    pub lazy: Vec<ImgChoice>,
}

/// Choose the image for every `<img>` (srcset, sizes, `<picture>` sources)
/// and the poster of every `<video>`.
pub fn plan_images(doc: &Document, base: &Url, screen: &Screen) -> ImagePlan {
    let mut plan = ImagePlan {
        eager: Vec::new(),
        lazy: Vec::new(),
    };
    for node in doc.elements_by_local_name(&local_name!("img")) {
        let Some(el) = doc.element(node) else {
            continue;
        };
        let Some(choice) = choose_img(doc, node, el, base, screen) else {
            continue;
        };
        let lazy = el
            .attr(&local_name!("loading"))
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("lazy"));
        if lazy {
            plan.lazy.push(choice);
        } else {
            plan.eager.push(choice);
        }
    }
    // A video shows its poster frame until it plays; there is no video
    // decoder, so the poster is what the element displays. Stored per element.
    for node in doc.elements_by_local_name(&local_name!("video")) {
        let Some(el) = doc
            .element(node)
            .filter(|e| e.is_html(&local_name!("video")))
        else {
            continue;
        };
        if let Some(url) = el
            .attr(&local_name!("poster"))
            .filter(|s| !s.trim().is_empty())
            .and_then(|s| huginn_net::resolve_url(base, s))
        {
            plan.eager.push(ImgChoice {
                node,
                url,
                density: 1.0,
                slot_width: None,
                by_url: false,
            });
        }
    }
    plan
}

fn choose_img(
    doc: &Document,
    node: NodeId,
    el: &ElementData,
    base: &Url,
    screen: &Screen,
) -> Option<ImgChoice> {
    let src = el
        .attr(&local_name!("src"))
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| huginn_net::resolve_url(base, s));
    let chosen = |srcset: &str, sizes: Option<&str>, src: Option<&str>| {
        choose_from_srcset(
            srcset,
            sizes,
            src,
            attr_px(el, &local_name!("width")),
            base,
            screen,
        )
    };

    // <picture>: the first <source> before the <img> whose type and media match.
    let mut pick = None;
    if let Some(parent) = doc.parent(node)
        && doc
            .element(parent)
            .is_some_and(|p| p.is_html(&local_name!("picture")))
    {
        for s in doc.children(parent) {
            if s == node {
                break;
            }
            let Some(source) = doc.element(s).filter(|e| e.is_html(&local_name!("source"))) else {
                continue;
            };
            if source
                .attr(&local_name!("type"))
                .is_some_and(|t| !supported_image_type(t))
            {
                continue;
            }
            if source
                .attr(&local_name!("media"))
                .is_some_and(|m| !huginn_net::media_matches(m, screen))
            {
                continue;
            }
            if let Some(srcset) = source.attr(&local_name!("srcset"))
                && let Some(c) = chosen(srcset, source.attr(&local_name!("sizes")), None)
            {
                pick = Some(c);
                break;
            }
        }
    }
    if pick.is_none()
        && let Some(srcset) = el.attr(&local_name!("srcset"))
    {
        let raw_src = el
            .attr(&local_name!("src"))
            .filter(|s| !s.trim().is_empty());
        pick = chosen(srcset, el.attr(&local_name!("sizes")), raw_src);
    }
    let (url, density, slot_width) = match pick {
        Some(p) => p,
        None => (src.clone()?, 1.0, None),
    };
    let by_url = density == 1.0 && src.as_ref() == Some(&url);
    Some(ImgChoice {
        node,
        url,
        density,
        slot_width,
        by_url,
    })
}

/// The `srcset` candidate for the screen: (URL, density, slot width if `w`
/// descriptors were used). `src` joins as the `1x` candidate when the set has
/// neither `w` descriptors nor a `1x` entry (HTML spec).
fn choose_from_srcset<'a>(
    srcset: &'a str,
    sizes: Option<&str>,
    src: Option<&'a str>,
    width_attr: Option<f32>,
    base: &Url,
    screen: &Screen,
) -> Option<(Url, f32, Option<f32>)> {
    use huginn_net::Descriptor;
    let mut cands = huginn_net::parse_srcset(srcset);
    if let Some(src) = src
        && !cands.iter().any(|c| {
            matches!(c.descriptor, Descriptor::Width(_)) || c.descriptor == Descriptor::Density(1.0)
        })
    {
        cands.push(huginn_net::SrcsetCandidate {
            url: src,
            descriptor: Descriptor::Density(1.0),
        });
    }
    if cands.is_empty() {
        return None;
    }
    let uses_w = cands
        .iter()
        .any(|c| matches!(c.descriptor, Descriptor::Width(_)));
    let slot = sizes
        .and_then(|s| huginn_net::eval_sizes(s, screen))
        .or(width_attr)
        .unwrap_or(screen.width);
    let url = huginn_net::choose_candidate(&cands, slot, screen.device_pixel_ratio)?;
    let density = cands
        .iter()
        .find(|c| c.url == url)
        .map_or(1.0, |c| match c.descriptor {
            Descriptor::Width(w) => w / slot.max(1.0),
            Descriptor::Density(x) => x,
        });
    Some((
        huginn_net::resolve_url(base, url)?,
        density,
        uses_w.then_some(slot),
    ))
}

fn supported_image_type(t: &str) -> bool {
    let t = t
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        t.as_str(),
        "image/png"
            | "image/jpeg"
            | "image/jpg"
            | "image/pjpeg"
            | "image/gif"
            | "image/webp"
            | "image/bmp"
            | "image/x-icon"
            | "image/vnd.microsoft.icon"
            | "image/svg+xml"
    )
}

/// A numeric `width`/`height` attribute (`"24"`, `"24px"`), in CSS px.
fn attr_px(el: &ElementData, name: &huginn_dom::LocalName) -> Option<f32> {
    el.attr(name)
        .and_then(|v| v.trim().trim_end_matches("px").parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// Displayed size in device px from CSS px sizes, then the attributes, then
/// the srcset slot width. A 0 side is unknown.
fn display_size(
    style: &ComputedStyle,
    el: &ElementData,
    slot_width: Option<f32>,
    dpr: f32,
) -> Option<(u32, u32)> {
    let css = |l: LengthPercentageAuto| match l {
        LengthPercentageAuto::Px(v) => Some(v),
        _ => None,
    };
    let (cw, ch) = (css(style.width), css(style.height));
    // One CSS side set and the other auto: the auto side follows the image's
    // aspect ratio, not the attribute (`height: 46px; width: auto` on
    // `<img width=382 height=159>`), so leave it unknown.
    let (w, h) = match (cw, ch) {
        (Some(w), Some(h)) => (Some(w), Some(h)),
        (Some(w), None) => (Some(w), None),
        (None, Some(h)) => (None, Some(h)),
        (None, None) => (
            attr_px(el, &local_name!("width")).or(slot_width),
            attr_px(el, &local_name!("height")),
        ),
    };
    let dev = |v: Option<f32>| v.map_or(0, |v| (v * dpr).ceil().max(1.0) as u32);
    (w.is_some() || h.is_some()).then(|| (dev(w), dev(h)))
}

/// A web font ready for `LayoutEngine::register_font`.
pub struct WebFont {
    pub family: Arc<str>,
    pub weight: f32,
    pub style: FontStyle,
    /// TrueType/OpenType bytes.
    pub data: Vec<u8>,
}

/// What [`load`] produced.
pub struct Loaded {
    pub images: ImageStore,
    /// In registration order: within a family, the subset covering the most
    /// text comes first.
    pub fonts: Vec<WebFont>,
}

enum Job {
    /// Fetch and decode one image URL; `display` is the largest known display size.
    Image {
        url: Url,
        display: Option<(u32, u32)>,
    },
    Font {
        url: Url,
    },
    Svg {
        markup: String,
        hint: Option<(u32, u32)>,
    },
}

enum Done {
    Image(Result<DecodedImage, NetError>),
    Font(Result<Vec<u8>, NetError>),
}

impl Job {
    fn run(&self, loader: &Loader) -> Done {
        match self {
            Job::Image { url, display } => Done::Image(
                loader
                    .fetch_uncached(url, ResourceKind::Image)
                    .and_then(|r| huginn_net::decode_image(&r.body, *display)),
            ),
            Job::Font { url } => Done::Font(
                loader
                    .fetch_uncached(url, ResourceKind::Font)
                    .and_then(|r| huginn_net::decode_font(r.body)),
            ),
            Job::Svg { markup, hint } => {
                Done::Image(huginn_net::decode_svg(markup.as_bytes(), *hint))
            }
        }
    }
}

/// Collects image jobs, one per URL, merging display sizes.
#[derive(Default)]
struct ImageJobs {
    jobs: Vec<Job>,
    by_url: HashMap<Url, usize>,
}

impl ImageJobs {
    fn add(&mut self, url: &Url, display: Option<(u32, u32)>) -> usize {
        if let Some(&i) = self.by_url.get(url) {
            if let Job::Image { display: d, .. } = &mut self.jobs[i] {
                // Decode big enough for every user; unknown means natural size.
                *d = match (*d, display) {
                    (Some(a), Some(b)) if a.0 > 0 && b.0 > 0 || a.1 > 0 && b.1 > 0 => {
                        Some((a.0.max(b.0), a.1.max(b.1)))
                    }
                    _ => None,
                };
            }
            return i;
        }
        self.jobs.push(Job::Image {
            url: url.clone(),
            display,
        });
        self.by_url.insert(url.clone(), self.jobs.len() - 1);
        self.jobs.len() - 1
    }
}

/// Fetch and decode the eager images, CSS backgrounds, inline SVGs and web
/// fonts in one parallel batch.
pub fn load(
    loader: &Loader,
    doc: &Document,
    styles: &StyleMap,
    font_faces: &[FontFaceRule],
    plan: &ImagePlan,
    screen: &Screen,
) -> Loaded {
    let mut jobs = ImageJobs::default();
    // <img> elements that generate a box.
    let mut img_users: Vec<(usize, &ImgChoice)> = Vec::new();
    for c in &plan.eager {
        let (Some(style), Some(el)) = (styles.get(c.node), doc.element(c.node)) else {
            continue;
        };
        let display = display_size(style, el, c.slot_width, screen.device_pixel_ratio);
        img_users.push((jobs.add(&c.url, display), c));
    }
    // CSS background and mask images, and `content: url(...)` images, at
    // natural size: of elements in shadow trees and of ::before/::after too.
    for n in doc.shadow_including_descendants(NodeId::DOCUMENT) {
        let pseudos = [PseudoElement::Before, PseudoElement::After].map(|p| styles.pseudo(n, p));
        for s in std::iter::once(styles.get(n)).chain(pseudos).flatten() {
            let mask = s.mask.as_deref().map(|m| &m.image);
            for layer in [s.background_image.as_ref(), mask].into_iter().flatten() {
                if let BackgroundImage::Url(u) = layer
                    && let Ok(u) = Url::parse(u)
                {
                    jobs.add(&u, None);
                }
            }
            if let Content::Items(list) = &s.content {
                for item in list.iter() {
                    if let ContentItem::Image(u) = item
                        && let Ok(u) = Url::parse(u)
                    {
                        jobs.add(&u, None);
                    }
                }
            }
        }
    }
    let mut jobs_list = jobs.jobs;
    let url_of: Vec<Option<Url>> = jobs_list
        .iter()
        .map(|j| match j {
            Job::Image { url, .. } => Some(url.clone()),
            _ => None,
        })
        .collect();

    // Inline <svg>.
    let svg_start = jobs_list.len();
    let svg_nodes = collect_inline_svgs(doc, styles, screen, &mut jobs_list);

    // Web fonts.
    let font_start = jobs_list.len();
    let selected = select_font_faces(doc, styles, font_faces);
    let mut font_meta = Vec::new();
    for (face, url) in selected {
        font_meta.push(face);
        jobs_list.push(Job::Font { url });
    }

    let mut done = loader.map_parallel(&jobs_list, |j| j.run(loader));
    drop(jobs_list);

    let mut images = ImageStore::new();
    let decoded: Vec<Option<DecodedImage>> = done[..svg_start]
        .iter()
        .map(|d| match d {
            Done::Image(Ok(img)) => Some(img.clone()),
            _ => None,
        })
        .collect();
    for (i, url) in url_of.iter().enumerate() {
        if let (Some(img), Some(url)) = (&decoded[i], url) {
            images.insert(url, img.clone());
        }
    }
    for (job, c) in img_users {
        if let Some(img) = &decoded[job]
            && !c.by_url
        {
            images.insert_for_node(c.node.to_raw(), with_density(img, c.density));
        }
    }
    for (k, node) in svg_nodes.into_iter().enumerate() {
        if let Done::Image(Ok(img)) = &done[svg_start + k] {
            images.insert_for_node(node.to_raw(), img.clone());
        }
    }
    let mut fonts = Vec::new();
    for (k, face) in font_meta.into_iter().enumerate() {
        if let Done::Font(Ok(mut data)) =
            std::mem::replace(&mut done[font_start + k], Done::Font(Ok(Vec::new())))
        {
            let ff = &font_faces[face];
            if ff.size_adjust != 1.0 {
                huginn_net::size_adjust_font(&mut data, ff.size_adjust);
            }
            fonts.push(WebFont {
                family: ff.family.clone(),
                weight: representative_weight(ff.weight),
                style: ff.style,
                data,
            });
        }
    }
    Loaded { images, fonts }
}

fn with_density(img: &DecodedImage, density: f32) -> DecodedImage {
    if density == 1.0 || density <= 0.0 {
        return img.clone();
    }
    let w = (img.width() as f32 / density).round() as u32;
    let h = (img.height() as f32 / density).round() as u32;
    img.with_natural_size((w, h))
}

/// Load the `loading=lazy` images whose box intersects `region`, store them,
/// and show them in `tree`. Returns true if one of them had no box size of
/// its own (so the caller must lay out again for its natural size).
#[allow(clippy::too_many_arguments)]
pub fn load_lazy(
    loader: &Loader,
    doc: &Document,
    styles: &StyleMap,
    plan: &ImagePlan,
    screen: &Screen,
    images: &mut ImageStore,
    tree: &mut LayoutTree,
    region: Rect,
) -> bool {
    if plan.lazy.is_empty() {
        return false;
    }
    let wanted: HashMap<NodeId, usize> = plan
        .lazy
        .iter()
        .enumerate()
        .map(|(i, c)| (c.node, i))
        .collect();
    // Lazy <img> boxes in view: (plan index, box index).
    let in_view: Vec<(usize, usize)> = tree
        .boxes
        .iter()
        .enumerate()
        .filter_map(|(b, lb)| {
            Some((*wanted.get(&lb.node?)?, b)).filter(|_| lb.rect.intersects(&region))
        })
        .collect();
    let choices: Vec<&ImgChoice> = in_view.iter().map(|&(i, _)| &plan.lazy[i]).collect();
    let loaded = load_choices(loader, doc, styles, &choices, screen, images);
    let mut relayout = false;
    for ((_, b), img) in in_view.into_iter().zip(loaded) {
        let Some(img) = img else { continue };
        let lb = &mut tree.boxes[b];
        if lb.rect.width > 0.0 && lb.rect.height > 0.0 {
            lb.kind = BoxKind::Image(img);
        } else {
            relayout = true;
        }
    }
    relayout
}

/// Load every `loading=lazy` image now (the window, which scrolls).
pub fn load_all_lazy(
    loader: &Loader,
    doc: &Document,
    styles: &StyleMap,
    plan: &ImagePlan,
    screen: &Screen,
    images: &mut ImageStore,
) {
    let choices: Vec<&ImgChoice> = plan.lazy.iter().collect();
    load_choices(loader, doc, styles, &choices, screen, images);
}

/// After scripts changed the DOM or a state changed styles (the live window):
/// load the `<img>` choices and CSS background, mask and `content` images
/// that are not in `images` yet. Returns true if any was added. Inline
/// `<svg>` and web fonts are not reloaded. `tried`: URLs fetched before
/// (failures are not retried).
pub fn load_added(
    loader: &Loader,
    doc: &Document,
    styles: &StyleMap,
    base: &Url,
    screen: &Screen,
    images: &mut ImageStore,
    tried: &mut std::collections::HashSet<String>,
) -> bool {
    let plan = plan_images(doc, base, screen);
    let missing: Vec<&ImgChoice> = plan
        .eager
        .iter()
        .chain(&plan.lazy)
        .filter(|c| styles.get(c.node).is_some())
        .filter(|c| {
            if c.by_url {
                images.get(c.url.as_str()).is_none()
            } else {
                images.get_for_node(c.node.to_raw()).is_none()
            }
        })
        .filter(|c| {
            tried.insert(if c.by_url {
                c.url.to_string()
            } else {
                format!("{} {}", c.node.to_raw(), c.url)
            })
        })
        .collect();
    let mut added = load_choices(loader, doc, styles, &missing, screen, images)
        .iter()
        .any(Option::is_some);
    let mut jobs = ImageJobs::default();
    for n in doc.shadow_including_descendants(NodeId::DOCUMENT) {
        let pseudos = [PseudoElement::Before, PseudoElement::After].map(|p| styles.pseudo(n, p));
        for s in std::iter::once(styles.get(n)).chain(pseudos).flatten() {
            let mask = s.mask.as_deref().map(|m| &m.image);
            let content = match &s.content {
                Content::Items(list) => list
                    .iter()
                    .filter_map(|i| match i {
                        ContentItem::Image(u) => Some(&**u),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let layers = [s.background_image.as_ref(), mask]
                .into_iter()
                .flatten()
                .filter_map(|l| match l {
                    BackgroundImage::Url(u) => Some(&**u),
                    _ => None,
                });
            for u in layers.chain(content) {
                if images.get(u).is_none()
                    && tried.insert(u.to_owned())
                    && let Ok(u) = Url::parse(u)
                {
                    jobs.add(&u, None);
                }
            }
        }
    }
    if !jobs.jobs.is_empty() {
        let done = loader.map_parallel(&jobs.jobs, |j| j.run(loader));
        for (job, d) in jobs.jobs.iter().zip(done) {
            if let (Job::Image { url, .. }, Done::Image(Ok(img))) = (job, d) {
                images.insert(url, img);
                added = true;
            }
        }
    }
    added
}

/// Fetch and decode `choices` in parallel and store them. Returns the image
/// per choice (`None` when hidden or failed).
fn load_choices(
    loader: &Loader,
    doc: &Document,
    styles: &StyleMap,
    choices: &[&ImgChoice],
    screen: &Screen,
    images: &mut ImageStore,
) -> Vec<Option<DecodedImage>> {
    let mut jobs = ImageJobs::default();
    let users: Vec<Option<usize>> = choices
        .iter()
        .map(|c| {
            let (style, el) = (styles.get(c.node)?, doc.element(c.node)?);
            let display = display_size(style, el, c.slot_width, screen.device_pixel_ratio);
            Some(jobs.add(&c.url, display))
        })
        .collect();
    if jobs.jobs.is_empty() {
        return vec![None; choices.len()];
    }
    let done = loader.map_parallel(&jobs.jobs, |j| j.run(loader));
    choices
        .iter()
        .zip(users)
        .map(|(c, job)| {
            let Done::Image(Ok(img)) = &done[job?] else {
                return None;
            };
            let img = with_density(img, c.density);
            if c.by_url {
                images.insert(&c.url, img.clone());
            } else {
                images.insert_for_node(c.node.to_raw(), img.clone());
            }
            Some(img)
        })
        .collect()
}

// ----- inline <svg> ---------------------------------------------------------

/// Serialize every rendered top-level inline `<svg>` into a job. Returns the
/// nodes in job order.
fn collect_inline_svgs(
    doc: &Document,
    styles: &StyleMap,
    screen: &Screen,
    jobs: &mut Vec<Job>,
) -> Vec<NodeId> {
    let mut nodes = Vec::new();
    for n in doc.shadow_including_descendants(NodeId::DOCUMENT) {
        let Some(el) = doc.element(n) else { continue };
        if el.name.ns != ns!(svg) || el.name.local != local_name!("svg") {
            continue;
        }
        // Nested <svg> is part of its parent's markup.
        if doc
            .parent(n)
            .and_then(|p| doc.element(p))
            .is_some_and(|p| p.name.ns == ns!(svg))
        {
            continue;
        }
        let Some(style) = styles.get(n) else { continue };
        let hint = display_size(style, el, None, screen.device_pixel_ratio);
        if is_zero_sized(style, el) {
            continue;
        }
        let mut markup = String::with_capacity(512);
        write_svg_element(doc, n, Some(style.color), &mut markup);
        jobs.push(Job::Svg { markup, hint });
        nodes.push(n);
    }
    nodes
}

/// `width="0"`, `height: 0` and similar sprite containers draw nothing.
fn is_zero_sized(style: &ComputedStyle, el: &ElementData) -> bool {
    let zero = |css: LengthPercentageAuto, attr: Option<f32>| match css {
        LengthPercentageAuto::Px(v) => v <= 0.0,
        LengthPercentageAuto::Auto => attr.is_some_and(|v| v <= 0.0),
        _ => false, // percent, calc(): resolved by layout
    };
    zero(style.width, attr_px(el, &local_name!("width")))
        || zero(style.height, attr_px(el, &local_name!("height")))
}

/// Write an SVG element and its SVG descendants as XML. The root gets the
/// namespace declarations and, for `currentColor`, the computed `color`.
fn write_svg_element(doc: &Document, node: NodeId, root_color: Option<Color>, out: &mut String) {
    let Some(el) = doc.element(node) else { return };
    out.push('<');
    out.push_str(&el.name.local);
    if root_color.is_some() {
        out.push_str(
            r#" xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink""#,
        );
    }
    let mut has_color = false;
    for a in &el.attrs {
        let prefix = if a.name.ns == ns!(xmlns) {
            continue; // declared above
        } else if a.name.ns == ns!(xlink) {
            "xlink:"
        } else if a.name.ns == ns!(xml) {
            "xml:"
        } else {
            ""
        };
        if !is_xml_name(&a.name.local) {
            continue; // e.g. Vue's `@click` or `:class`: invalid XML, usvg would reject the whole SVG
        }
        has_color |= prefix.is_empty() && &*a.name.local == "color";
        out.push(' ');
        out.push_str(prefix);
        out.push_str(&a.name.local);
        out.push_str("=\"");
        escape(&a.value, true, out);
        out.push('"');
    }
    if let Some(c) = root_color
        && !has_color
    {
        use std::fmt::Write;
        let _ = if c.a == 255 {
            write!(out, r##" color="#{:02x}{:02x}{:02x}""##, c.r, c.g, c.b)
        } else {
            write!(
                out,
                r#" color="rgba({},{},{},{:.3})""#,
                c.r,
                c.g,
                c.b,
                f32::from(c.a) / 255.0
            )
        };
    }
    out.push('>');
    for c in doc.children(node) {
        match &doc.node(c).data {
            NodeData::Element(ce) if ce.name.ns == ns!(svg) => write_svg_element(doc, c, None, out),
            NodeData::Text(t) => escape(t, false, out),
            _ => {}
        }
    }
    out.push_str("</");
    out.push_str(&el.name.local);
    out.push('>');
}

fn is_xml_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn escape(s: &str, attr: bool, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

// ----- web fonts ------------------------------------------------------------

/// Code points used by the page's text: a bitmap for the BMP plus a set for
/// the rest.
struct CharSet {
    bmp: Vec<u64>,
    astral: HashSet<u32>,
}

impl CharSet {
    fn new() -> CharSet {
        CharSet {
            bmp: vec![0; 1024],
            astral: HashSet::new(),
        }
    }

    fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            let c = c as u32;
            if c < 0x10000 {
                self.bmp[(c >> 6) as usize] |= 1 << (c & 63);
            } else {
                self.astral.insert(c);
            }
        }
    }

    /// How many used code points fall in `lo..=hi`.
    fn count(&self, lo: u32, hi: u32) -> u32 {
        let mut n = 0;
        if lo < 0x10000 {
            let hi_bmp = hi.min(0xFFFF);
            for word in (lo >> 6)..=(hi_bmp >> 6) {
                let mut bits = self.bmp[word as usize];
                let base = word << 6;
                if base < lo {
                    bits &= !0u64 << (lo - base);
                }
                if base + 63 > hi_bmp {
                    bits &= !0u64 >> (63 - (hi_bmp - base));
                }
                n += bits.count_ones();
            }
        }
        if hi >= 0x10000 {
            n += self.astral.iter().filter(|&&c| c >= lo && c <= hi).count() as u32;
        }
        n
    }

    fn coverage(&self, ranges: &[(u32, u32)]) -> u32 {
        if ranges.is_empty() {
            return u32::MAX;
        }
        ranges
            .iter()
            .map(|&(lo, hi)| self.count(lo, hi.max(lo)))
            .sum()
    }
}

fn style_key(s: FontStyle) -> u8 {
    match s {
        FontStyle::Normal => 0,
        FontStyle::Italic => 1,
        FontStyle::Oblique => 2,
    }
}

/// The faces to load: for every (font-family, weight, style) used by text,
/// the first family in the list that has `@font-face` rules, the best face
/// group by CSS font matching (style, then weight), and the faces of that
/// group whose `unicode-range` covers text on the page. Returns face indices
/// with the URL of their first usable source, most text-coverage first.
fn select_font_faces(
    doc: &Document,
    styles: &StyleMap,
    faces: &[FontFaceRule],
) -> Vec<(usize, Url)> {
    if faces.is_empty() {
        return Vec::new();
    }
    let mut by_family: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, f) in faces.iter().enumerate() {
        if source_url(f).is_some() {
            by_family
                .entry(f.family.to_lowercase())
                .or_default()
                .push(i);
        }
    }
    if by_family.is_empty() {
        return Vec::new();
    }
    let mut chars = CharSet::new();
    let mut used: HashSet<(Arc<str>, u32, u8)> = HashSet::new();
    for n in doc.shadow_including_descendants(NodeId::DOCUMENT) {
        match &doc.node(n).data {
            NodeData::Text(t) => {
                if t.trim().is_empty() {
                    continue;
                }
                let Some(style) = doc.parent(n).and_then(|p| styles.get(p)) else {
                    continue;
                };
                chars.insert_str(t);
                used.insert((
                    style.font_family.clone(),
                    style.font_weight.to_bits(),
                    style_key(style.font_style),
                ));
            }
            // Text of ::before/::after (icon fonts put their glyphs there).
            NodeData::Element(_) => {
                for ps in [PseudoElement::Before, PseudoElement::After]
                    .into_iter()
                    .filter_map(|p| styles.pseudo(n, p))
                {
                    let mut any = false;
                    let mut add = |t: &str| {
                        if !t.trim().is_empty() {
                            chars.insert_str(t);
                            any = true;
                        }
                    };
                    match &ps.content {
                        Content::Text(t) => add(t),
                        Content::Items(list) => list.iter().for_each(|i| {
                            if let ContentItem::Text(t) = i {
                                add(t);
                            }
                        }),
                        Content::None => {}
                    }
                    if any {
                        used.insert((
                            ps.font_family.clone(),
                            ps.font_weight.to_bits(),
                            style_key(ps.font_style),
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    let mut picked: Vec<usize> = Vec::new();
    let mut lists: HashMap<Arc<str>, Option<&Vec<usize>>> = HashMap::new();
    for (family_list, weight_bits, style) in &used {
        let family_faces = *lists
            .entry(family_list.clone())
            .or_insert_with(|| family_names(family_list).find_map(|name| by_family.get(&name)));
        let Some(family_faces) = family_faces else {
            continue;
        };
        let weight = f32::from_bits(*weight_bits);
        let rank = |i: usize| {
            (
                style_rank(faces[i].style, *style),
                weight_rank(faces[i].weight, weight),
            )
        };
        let Some(best) = family_faces
            .iter()
            .min_by(|a, b| rank(**a).partial_cmp(&rank(**b)).unwrap())
        else {
            continue;
        };
        let (best_style, best_weight) = (faces[*best].style, faces[*best].weight);
        for &i in family_faces {
            let f = &faces[i];
            if f.style == best_style
                && f.weight == best_weight
                && chars.coverage(&f.unicode_range) > 0
                && !picked.contains(&i)
            {
                picked.push(i);
            }
        }
    }
    // Most coverage first, so a family's main subset is registered first.
    picked.sort_by_key(|&i| {
        (
            std::cmp::Reverse(chars.coverage(&faces[i].unicode_range)),
            i,
        )
    });
    picked
        .into_iter()
        .filter_map(|i| Some((i, source_url(&faces[i])?)))
        .collect()
}

/// The first `src` we can decode: WOFF2, WOFF, TrueType or OpenType.
fn source_url(face: &FontFaceRule) -> Option<Url> {
    face.sources.iter().find_map(|s| {
        let ok = match s.format.as_deref() {
            Some(f) => {
                let f = f.split_ascii_whitespace().next().unwrap_or("");
                let f = f.strip_suffix("-variations").unwrap_or(f);
                matches!(
                    f,
                    "woff2" | "woff" | "truetype" | "opentype" | "ttf" | "otf" | "collection"
                )
            }
            None => {
                let path = s
                    .url
                    .split(['?', '#'])
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                !(path.ends_with(".eot") || path.ends_with(".svg"))
            }
        };
        if ok { Url::parse(&s.url).ok() } else { None }
    })
}

/// `"Fira Sans", Arial, sans-serif` -> fira sans, arial, sans-serif.
fn family_names(list: &str) -> impl Iterator<Item = String> + '_ {
    list.split(',')
        .map(|f| f.trim().trim_matches(['"', '\'']).trim().to_lowercase())
        .filter(|f| !f.is_empty())
}

/// CSS Fonts 4 font-style matching order.
fn style_rank(face: FontStyle, want: u8) -> u8 {
    let face = style_key(face);
    match want {
        // Indexed by face style: [normal, italic, oblique].
        1 => [2, 0, 1][face as usize], // italic: italic, oblique, normal
        2 => [2, 1, 0][face as usize], // oblique: oblique, italic, normal
        _ => [0, 2, 1][face as usize], // normal: normal, oblique, italic
    }
}

/// CSS Fonts 4 font-weight matching order: (tier, distance), lower is better.
fn weight_rank((lo, hi): (f32, f32), w: f32) -> (u8, f32) {
    if lo <= w && w <= hi {
        return (0, 0.0);
    }
    if (400.0..=500.0).contains(&w) {
        if lo > w && lo <= 500.0 {
            (1, lo - w)
        } else if hi < w {
            (2, w - hi)
        } else {
            (3, lo - w)
        }
    } else if w < 400.0 {
        if hi < w { (1, w - hi) } else { (2, lo - w) }
    } else if lo > w {
        (1, lo - w)
    } else {
        (2, w - hi)
    }
}

/// One weight to register a face under. Variable faces (a range) get the
/// value nearest to 400; Parley then sets the `wght` axis per request.
fn representative_weight((lo, hi): (f32, f32)) -> f32 {
    400.0_f32.clamp(lo.min(hi), hi.max(lo))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(
        family: &str,
        weight: (f32, f32),
        style: FontStyle,
        range: &[(u32, u32)],
    ) -> FontFaceRule {
        FontFaceRule {
            family: Arc::from(family),
            sources: vec![huginn_style::FontFaceSource {
                url: format!("http://h/{family}-{}.woff2", weight.0),
                format: Some("woff2".into()),
            }],
            weight,
            style,
            unicode_range: range.to_vec(),
            size_adjust: 1.0,
        }
    }

    fn styled(html: &str, css: &str) -> (Document, StyleMap) {
        let doc = huginn_dom::parse_html(html);
        let mut e = huginn_style::StyleEngine::new(huginn_style::Viewport {
            width: 1280.0,
            height: 800.0,
            device_pixel_ratio: 1.0,
        });
        e.add_stylesheet(
            css,
            "http://h/",
            huginn_style::Origin::Author,
            &huginn_style::NoImports,
        );
        let s = e.compute(&doc);
        (doc, s)
    }

    #[test]
    fn font_matching_picks_style_weight_and_covering_subsets() {
        let faces = vec![
            face("Body", (400.0, 400.0), FontStyle::Normal, &[]), // 0
            face("Body", (700.0, 700.0), FontStyle::Normal, &[]), // 1
            face("Body", (400.0, 400.0), FontStyle::Italic, &[]), // 2
            face("Body", (300.0, 300.0), FontStyle::Normal, &[]), // 3 never used
            face("Sub", (100.0, 900.0), FontStyle::Normal, &[(0x400, 0x4FF)]), // 4 cyrillic: no text
            face("Sub", (100.0, 900.0), FontStyle::Normal, &[(0, 0xFF)]),      // 5 latin
            face("Unused", (400.0, 400.0), FontStyle::Normal, &[]),            // 6
        ];
        let (doc, styles) = styled(
            "<p>plain <b>bold</b> <i>it</i></p><h2>sub</h2><div hidden>Unused</div>",
            "p { font-family: 'Body', serif } h2 { font-family: sans-serif, Sub; font-weight: 600 } \
             div { font-family: Unused }",
        );
        let mut got: Vec<usize> = select_font_faces(&doc, &styles, &faces)
            .into_iter()
            .map(|(i, _)| i)
            .collect();
        got.sort();
        assert_eq!(got, [0, 1, 2, 5]);
    }

    #[test]
    fn generated_content_text_selects_its_font_faces() {
        // Icon fonts: the glyph is ::before content in a private-use code
        // point, the element itself has no text.
        let faces = vec![
            face("Icons", (900.0, 900.0), FontStyle::Normal, &[]), // 0
            face("Icons", (400.0, 400.0), FontStyle::Normal, &[]), // 1 wrong weight
            face(
                "Late",
                (400.0, 400.0),
                FontStyle::Normal,
                &[(0xE000, 0xF8FF)],
            ), // 2
            face("Gone", (400.0, 400.0), FontStyle::Normal, &[]),  // 3 empty content
        ];
        let (doc, styles) = styled(
            "<span class=i></span><span class=j></span><span class=k></span>",
            ".i::before { content: '\\f1b3'; font-family: Icons; font-weight: 900 } \
             .j::after { content: url(http://h/x.svg) '\\e001'; font-family: Late } \
             .k::before { content: ''; font-family: Gone }",
        );
        let mut got: Vec<usize> = select_font_faces(&doc, &styles, &faces)
            .into_iter()
            .map(|(i, _)| i)
            .collect();
        got.sort();
        assert_eq!(got, [0, 2]);
    }

    #[test]
    fn weight_order_follows_css_fonts_4() {
        // Desired 500 with faces 300 and 700: below wins (400-500 band looks down first after up to 500).
        assert!(weight_rank((300.0, 300.0), 500.0) < weight_rank((700.0, 700.0), 500.0));
        // Desired 600: heavier first.
        assert!(weight_rank((700.0, 700.0), 600.0) < weight_rank((500.0, 500.0), 600.0));
        // Desired 300: lighter first.
        assert!(weight_rank((200.0, 200.0), 300.0) < weight_rank((400.0, 400.0), 300.0));
        // Inside a variable range: exact.
        assert_eq!(weight_rank((100.0, 900.0), 650.0), (0, 0.0));
        assert_eq!(representative_weight((100.0, 900.0)), 400.0);
        assert_eq!(representative_weight((600.0, 800.0)), 600.0);
    }

    #[test]
    fn img_choice_uses_srcset_sizes_and_picture() {
        let base = Url::parse("http://h/p/").unwrap();
        let screen = Screen {
            width: 1280.0,
            height: 800.0,
            device_pixel_ratio: 1.0,
        };
        let doc = huginn_dom::parse_html(
            "<img id=a src=a.png>\
             <img id=b src=b.png srcset='b-320.png 320w, b-640.png 640w' sizes='(min-width: 1000px) 300px, 100vw'>\
             <img id=c src=c.png srcset='c2.png 2x'>\
             <picture><source type=image/avif srcset=d.avif><source media='(max-width: 600px)' srcset=d-small.webp>\
             <source srcset=d.webp><img id=d src=d.jpg loading=lazy></picture>",
        );
        let plan = plan_images(&doc, &base, &screen);
        let get = |id: &str| {
            let n = doc.element_by_id(id).unwrap();
            plan.eager
                .iter()
                .chain(&plan.lazy)
                .find(|c| c.node == n)
                .unwrap()
        };
        assert_eq!(get("a").url.as_str(), "http://h/p/a.png");
        assert!(get("a").by_url);
        let b = get("b");
        assert_eq!(b.url.as_str(), "http://h/p/b-320.png");
        assert!(!b.by_url);
        assert!((b.density - 320.0 / 300.0).abs() < 1e-4);
        assert_eq!(
            get("c").url.as_str(),
            "http://h/p/c.png",
            "src is the 1x candidate"
        );
        assert_eq!(get("d").url.as_str(), "http://h/p/d.webp");
        assert_eq!(plan.lazy.len(), 1);
    }

    #[test]
    fn inline_svg_markup_parses_and_uses_current_color() {
        let (doc, styles) = styled(
            "<p style='color: #00ff00'><svg viewBox='0 0 2 2' width=4 height=4><path d='M0 0H2V2H0z' fill=currentColor /></svg></p>",
            "",
        );
        let screen = Screen {
            width: 1280.0,
            height: 800.0,
            device_pixel_ratio: 1.0,
        };
        let mut jobs = Vec::new();
        let nodes = collect_inline_svgs(&doc, &styles, &screen, &mut jobs);
        assert_eq!(nodes.len(), 1);
        let Job::Svg { markup, hint } = &jobs[0] else {
            panic!()
        };
        assert!(markup.contains("viewBox=\"0 0 2 2\""), "{markup}");
        let img = huginn_net::decode_svg(markup.as_bytes(), *hint).unwrap();
        assert_eq!((img.width(), img.height()), (4, 4));
        let p = img.pixmap().sample(2, 2);
        assert_eq!((p.r, p.g, p.b, p.a), (0, 255, 0, 255));
    }

    #[test]
    fn charset_counts_ranges() {
        let mut c = CharSet::new();
        c.insert_str("Aé€😀");
        assert_eq!(c.count(0, 0x7F), 1);
        assert_eq!(c.count(0x80, 0xFF), 1);
        assert_eq!(c.count(0x20AC, 0x20AC), 1);
        assert_eq!(c.count(0x1F600, 0x1F64F), 1);
        assert_eq!(c.count(0x400, 0x4FF), 0);
        assert_eq!(c.coverage(&[]), u32::MAX);
    }
}
