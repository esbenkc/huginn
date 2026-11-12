//! `querySelector(All)` / `matches` / `closest` for huginn-js, on Stylo's
//! selector parser and matcher (same selector support as the cascade).

use huginn_dom::{Document, NodeId};
use selectors::matching::{
    MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, SelectorCaches,
    matches_selector_list,
};
use selectors::parser::SelectorList;
use style::selector_parser::{SelectorImpl, SelectorParser};
use style::shared_lock::SharedRwLock;

use crate::dom::DomCx;
use crate::engine;

/// The selector string is not valid CSS (a `SyntaxError` in the DOM).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SelectorError;

impl std::fmt::Display for SelectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid selector")
    }
}

impl std::error::Error for SelectorError {}

fn parse(selectors: &str) -> Result<SelectorList<SelectorImpl>, SelectorError> {
    engine::init_stylo();
    let url = engine::url_data("about:blank");
    SelectorParser::parse_author_origin_no_namespace(selectors, &url).map_err(|_| SelectorError)
}

fn run<R>(
    doc: &Document,
    scope: Option<NodeId>,
    f: impl FnOnce(&DomCx<'_>, &mut MatchingContext<'_, SelectorImpl>) -> R,
) -> R {
    let cx = DomCx::bare(doc, SharedRwLock::new());
    let mut caches = SelectorCaches::default();
    let mut ctx = MatchingContext::new(
        MatchingMode::Normal,
        None,
        &mut caches,
        cx.quirks,
        NeedsSelectorFlags::No,
        MatchingForInvalidation::No,
    );
    if let Some(scope) = scope.filter(|s| doc.element(*s).is_some()) {
        ctx.scope_element = Some(selectors::Element::opaque(&cx.node(scope)));
    }
    f(&cx, &mut ctx)
}

/// Elements in the subtree of `scope` (not `scope` itself) that match
/// `selectors`, in tree order (`querySelectorAll`). Like the DOM, the
/// selector is matched against the whole document, not only the subtree.
pub fn query_selector_all(
    doc: &Document,
    scope: NodeId,
    selectors: &str,
) -> Result<Vec<NodeId>, SelectorError> {
    let list = parse(selectors)?;
    Ok(run(doc, Some(scope), |cx, ctx| {
        doc.descendants(scope)
            .skip(1)
            .filter(|&id| {
                doc.element(id).is_some() && matches_selector_list(&list, cx.node(id), ctx)
            })
            .collect()
    }))
}

/// First match of [`query_selector_all`], without collecting the rest.
pub fn query_selector(
    doc: &Document,
    scope: NodeId,
    selectors: &str,
) -> Result<Option<NodeId>, SelectorError> {
    let list = parse(selectors)?;
    Ok(run(doc, Some(scope), |cx, ctx| {
        doc.descendants(scope)
            .skip(1)
            .find(|&id| doc.element(id).is_some() && matches_selector_list(&list, cx.node(id), ctx))
    }))
}

/// Whether `element` matches `selectors` (`Element.matches`). False for
/// non-elements.
pub fn matches_selector(
    doc: &Document,
    element: NodeId,
    selectors: &str,
) -> Result<bool, SelectorError> {
    let list = parse(selectors)?;
    if doc.element(element).is_none() {
        return Ok(false);
    }
    Ok(run(doc, Some(element), |cx, ctx| {
        matches_selector_list(&list, cx.node(element), ctx)
    }))
}

/// DOM `element.closest(selectors)`: `element` or its nearest ancestor that
/// matches.
pub fn closest(
    doc: &Document,
    element: NodeId,
    selectors: &str,
) -> Result<Option<NodeId>, SelectorError> {
    let list = parse(selectors)?;
    Ok(run(doc, Some(element), |cx, ctx| {
        std::iter::once(element)
            .chain(doc.ancestors(element))
            .find(|&id| doc.element(id).is_some() && matches_selector_list(&list, cx.node(id), ctx))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_scope_order_and_errors() {
        let doc = huginn_dom::parse_html(
            "<div id=a class=x><p class=x id=b><span class=x id=c></span></p></div><p class=x id=d></p>",
        );
        let a = doc.element_by_id("a").unwrap();
        let ids = |v: Vec<NodeId>| -> Vec<String> {
            v.iter()
                .map(|&n| doc.element(n).unwrap().id().unwrap().to_owned())
                .collect()
        };
        // Scope itself is excluded; matching uses the whole document (div p).
        assert_eq!(ids(query_selector_all(&doc, a, ".x").unwrap()), ["b", "c"]);
        assert_eq!(
            ids(query_selector_all(&doc, NodeId::DOCUMENT, "div p, #d").unwrap()),
            ["b", "d"]
        );
        assert_eq!(
            query_selector(&doc, NodeId::DOCUMENT, "p").unwrap(),
            doc.element_by_id("b")
        );
        let c = doc.element_by_id("c").unwrap();
        assert_eq!(closest(&doc, c, "p").unwrap(), doc.element_by_id("b"));
        assert!(matches_selector(&doc, c, "div span.x").unwrap());
        assert!(!matches_selector(&doc, c, "span::before").unwrap());
        assert!(matches_selector(&doc, a, "div:has(> p span)").unwrap());
        assert_eq!(query_selector_all(&doc, a, "p >"), Err(SelectorError));
    }
}
