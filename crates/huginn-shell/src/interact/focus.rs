//! Sequential focus navigation order (Tab / Shift+Tab), as in HTML's
//! "sequential focus navigation": elements with a positive `tabindex` first,
//! by tabindex then tree order, then the rest in tree order.
//! The live-JS input code calls [`next_focusable`] as the default action of
//! Tab, after page script had its chance to cancel the keydown.

use huginn_dom::{Document, LocalName, NodeId, local_name, ns};
use huginn_style::{Display, StyleMap, Visibility};

/// The element focus moves to from `from` (`None`: from the start, or from
/// the end when `backward`). `None` when `from` is the last (or first)
/// element: focus leaves the page, as Chrome moves it to the toolbar.
pub fn next_focusable(
    doc: &Document,
    styles: &StyleMap,
    from: Option<NodeId>,
    backward: bool,
) -> Option<NodeId> {
    let order = focus_order(doc, styles);
    let Some(from) = from else {
        return if backward {
            order.last().copied()
        } else {
            order.first().copied()
        };
    };
    match order.iter().position(|&n| n == from) {
        Some(i) if backward => i.checked_sub(1).map(|j| order[j]),
        Some(i) => order.get(i + 1).copied(),
        // `from` is not in the order (clicked text, a tabindex=-1 element):
        // continue from its place in tree order.
        None => {
            let pos = tree_positions(doc);
            let at = pos.get(from.index()).copied().unwrap_or(0);
            let seq = order
                .iter()
                .filter(|&&n| tabindex(doc, n).is_none_or(|t| t == 0));
            if backward {
                seq.filter(|&&n| pos[n.index()] < at).last().copied()
            } else {
                seq.copied().find(|&n| pos[n.index()] > at)
            }
        }
    }
}

/// Elements in sequential focus navigation order.
pub fn focus_order(doc: &Document, styles: &StyleMap) -> Vec<NodeId> {
    let mut positive: Vec<(i32, NodeId)> = Vec::new();
    let mut rest = Vec::new();
    for n in doc.descendants(NodeId::DOCUMENT) {
        if !is_sequentially_focusable(doc, styles, n) {
            continue;
        }
        match tabindex(doc, n) {
            Some(t) if t > 0 => positive.push((t, n)),
            _ => rest.push(n),
        }
    }
    // Stable: equal tabindex keeps tree order.
    positive.sort_by_key(|&(t, _)| t);
    positive.into_iter().map(|(_, n)| n).chain(rest).collect()
}

fn tree_positions(doc: &Document) -> Vec<u32> {
    let mut pos = vec![0u32; doc.len()];
    for (i, n) in doc.descendants(NodeId::DOCUMENT).enumerate() {
        pos[n.index()] = i as u32;
    }
    pos
}

fn tabindex(doc: &Document, n: NodeId) -> Option<i32> {
    doc.element(n)?
        .attr(&local_name!("tabindex"))?
        .trim()
        .parse()
        .ok()
}

/// Focusable and reachable with Tab: rendered, not disabled or inert, and
/// focusable by kind (links, form controls, `summary`, editing hosts) or by
/// a `tabindex` of 0 or more.
pub fn is_sequentially_focusable(doc: &Document, styles: &StyleMap, n: NodeId) -> bool {
    let Some(e) = doc.element(n) else {
        return false;
    };
    if e.name.ns != ns!(html) {
        return false;
    }
    let ti = tabindex(doc, n);
    if ti.is_some_and(|t| t < 0) {
        return false;
    }
    let local = &e.name.local;
    let focusable_kind = match *local {
        local_name!("a") | local_name!("area") => e.attr(&local_name!("href")).is_some(),
        local_name!("button") | local_name!("select") | local_name!("textarea") => true,
        local_name!("input") => !e
            .attr(&local_name!("type"))
            .is_some_and(|t| t.eq_ignore_ascii_case("hidden")),
        local_name!("iframe") => true,
        local_name!("summary") => {
            doc.parent(n)
                .and_then(|p| doc.element(p))
                .is_some_and(|p| p.is_html(&local_name!("details")))
                && doc.parent(n).is_some_and(|p| {
                    doc.children(p).find(|&c| {
                        doc.element(c)
                            .is_some_and(|c| c.is_html(&local_name!("summary")))
                    }) == Some(n)
                })
        }
        local_name!("audio") | local_name!("video") => e.attr(&local_name!("controls")).is_some(),
        _ => e.attr(&local_name!("contenteditable")).is_some_and(|v| {
            v.is_empty()
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("plaintext-only")
        }),
    };
    if !focusable_kind && ti.is_none() {
        return false;
    }
    if is_form_control(local) && is_disabled(doc, n) {
        return false;
    }
    is_rendered(doc, styles, n)
}

fn is_form_control(local: &LocalName) -> bool {
    matches!(
        *local,
        local_name!("button")
            | local_name!("input")
            | local_name!("select")
            | local_name!("textarea")
    )
}

/// `disabled` on the control, or inside a disabled `<fieldset>` (but not in
/// its first `<legend>`).
fn is_disabled(doc: &Document, n: NodeId) -> bool {
    let disabled = local_name!("disabled");
    if doc.attribute(n, &disabled).is_some() {
        return true;
    }
    let mut child = n;
    for a in doc.ancestors(n) {
        if let Some(e) = doc.element(a)
            && e.is_html(&local_name!("fieldset"))
            && e.attr(&disabled).is_some()
        {
            let first_legend = doc.children(a).find(|&c| {
                doc.element(c)
                    .is_some_and(|c| c.is_html(&local_name!("legend")))
            });
            if first_legend != Some(child) {
                return true;
            }
        }
        child = a;
    }
    false
}

/// Has a style, is not inside `display: none` or an `inert` subtree, and is visible.
fn is_rendered(doc: &Document, styles: &StyleMap, n: NodeId) -> bool {
    let Some(s) = styles.get(n) else { return false };
    if s.display == Display::None || s.visibility != Visibility::Visible {
        return false;
    }
    let inert = LocalName::from("inert");
    std::iter::once(n).chain(doc.ancestors(n)).all(|a| {
        doc.element(a).is_none_or(|e| e.attr(&inert).is_none())
            && styles.get(a).is_none_or(|s| s.display != Display::None)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use huginn_style::{NoImports, Origin, StyleEngine, Viewport};

    fn order(html: &str) -> (Document, StyleMap) {
        let doc = huginn_dom::parse_html(html);
        let mut engine = StyleEngine::new(Viewport {
            width: 400.0,
            height: 300.0,
            device_pixel_ratio: 1.0,
        });
        for n in doc.elements_by_local_name(&local_name!("style")) {
            engine.add_stylesheet(
                &doc.text_content(n),
                "about:blank",
                Origin::Author,
                &NoImports,
            );
        }
        let styles = engine.compute(&doc);
        (doc, styles)
    }

    fn ids(doc: &Document, nodes: &[NodeId]) -> Vec<String> {
        nodes
            .iter()
            .map(|&n| {
                doc.element(n)
                    .and_then(|e| e.id())
                    .unwrap_or("?")
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn tab_order_follows_tabindex_then_tree_order() {
        let (doc, styles) = order(
            "<a id=a href=x>a</a><a id=nohref>x</a><input id=i><input id=h type=hidden>\
             <button id=b disabled>b</button><span id=s tabindex=0>s</span><span id=neg tabindex=-1>n</span>\
             <div style=\"display:none\"><a id=hidden href=y>h</a></div><a id=vis style=\"visibility:hidden\" href=z>v</a>\
             <fieldset disabled><legend><input id=inlegend></legend><input id=infs></fieldset>\
             <div inert><a id=inert href=w>i</a></div>\
             <textarea id=t tabindex=2></textarea><select id=sel tabindex=1></select>\
             <details><summary id=sum>more</summary></details>",
        );
        let o = focus_order(&doc, &styles);
        assert_eq!(
            ids(&doc, &o),
            ["sel", "t", "a", "i", "s", "inlegend", "sum"]
        );
        let id = |s: &str| doc.element_by_id(s).unwrap();
        let next = |from: Option<&str>, back: bool| {
            next_focusable(&doc, &styles, from.map(id), back).map(|n| ids(&doc, &[n])[0].clone())
        };
        assert_eq!(next(None, false).as_deref(), Some("sel"));
        assert_eq!(next(None, true).as_deref(), Some("sum"));
        assert_eq!(next(Some("a"), false).as_deref(), Some("i"));
        assert_eq!(next(Some("a"), true).as_deref(), Some("t"));
        assert_eq!(
            next(Some("sum"), false),
            None,
            "focus leaves the page after the last"
        );
        // From an element outside the order: the next one after it in tree order.
        assert_eq!(next(Some("neg"), false).as_deref(), Some("inlegend"));
        assert_eq!(next(Some("neg"), true).as_deref(), Some("s"));
    }
}
