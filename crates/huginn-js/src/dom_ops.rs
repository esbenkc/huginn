//! Engine-independent DOM operations behind the script bindings. The V8
//! layer only converts arguments and return values; the DOM rules live here,
//! where they can be tested without an engine.

use huginn_dom::{Document, LocalName, NodeData, NodeId, local_name, ns};
use huginn_net::Url;

/// DOM `nodeType`.
pub fn node_type(doc: &Document, n: NodeId) -> u32 {
    match doc.node(n).data {
        NodeData::Element(_) => 1,
        NodeData::Text(_) => 3,
        NodeData::ProcessingInstruction { .. } => 7,
        NodeData::Comment(_) => 8,
        NodeData::Document => 9,
        NodeData::Doctype { .. } => 10,
        NodeData::DocumentFragment => 11,
    }
}

/// Why an insertion was refused (the DOM exception to throw).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InsertError {
    /// `HierarchyRequestError`: cycle, or a node that cannot go there.
    Hierarchy,
    /// `NotFoundError`: the reference node is not a child of the parent.
    NotFound,
}

/// DOM `insertBefore` with the pre-insertion checks. A fragment child is
/// emptied into `parent`. Returns the nodes that were inserted, in order.
pub fn insert(
    doc: &mut Document,
    parent: NodeId,
    child: NodeId,
    reference: Option<NodeId>,
) -> Result<Vec<NodeId>, InsertError> {
    let parent_is_doc = match doc.node(parent).data {
        NodeData::Document => true,
        NodeData::DocumentFragment | NodeData::Element(_) => false,
        _ => return Err(InsertError::Hierarchy),
    };
    if child == parent || doc.ancestors(parent).any(|a| a == child) {
        return Err(InsertError::Hierarchy);
    }
    if let Some(r) = reference
        && doc.parent(r) != Some(parent)
    {
        return Err(InsertError::NotFound);
    }
    let nodes: Vec<NodeId> = match doc.node(child).data {
        NodeData::Document => return Err(InsertError::Hierarchy),
        NodeData::DocumentFragment => doc.children(child).collect(),
        _ => vec![child],
    };
    if parent_is_doc {
        for &n in &nodes {
            match doc.node(n).data {
                NodeData::Text(_) => return Err(InsertError::Hierarchy),
                NodeData::Element(_)
                    if doc
                        .children(parent)
                        .any(|c| c != n && doc.element(c).is_some()) =>
                {
                    return Err(InsertError::Hierarchy);
                }
                _ => {}
            }
        }
    }
    let mut reference = reference;
    if reference == Some(child) {
        reference = doc.next_sibling(child);
    }
    for &n in &nodes {
        doc.insert_before(parent, n, reference);
    }
    Ok(nodes)
}

/// True if `n` is connected: its shadow-including root is the document
/// (DOM, "connected"). A node in a shadow tree is connected when its host is.
pub fn is_connected(doc: &Document, mut n: NodeId) -> bool {
    loop {
        let root = doc.root(n);
        if root == NodeId::DOCUMENT {
            return true;
        }
        match doc.shadow_host(root) {
            Some(host) => n = host,
            None => return false,
        }
    }
}

/// Inclusive descendant test (`a.contains(b)`).
pub fn contains(doc: &Document, a: NodeId, b: NodeId) -> bool {
    a == b || doc.ancestors(b).any(|x| x == a)
}

fn is_html_element(doc: &Document, n: NodeId, local: &LocalName) -> bool {
    doc.element(n).is_some_and(|e| e.is_html(local))
}

/// `<script>` elements in the shadow-including subtrees of `roots`
/// (shadow-including tree order), for which `started` returns false.
pub fn scripts_in(
    doc: &Document,
    roots: &[NodeId],
    mut started: impl FnMut(NodeId) -> bool,
) -> Vec<NodeId> {
    let mut out = Vec::new();
    for &r in roots {
        for n in doc.shadow_including_descendants(r) {
            if is_html_element(doc, n, &local_name!("script")) && !started(n) {
                out.push(n);
            }
        }
    }
    out
}

/// HTML elements in the shadow-including subtrees of `roots` whose name could
/// be a custom element (contains a hyphen), or is exactly `name` if given,
/// in shadow-including tree order (DOM "insert" and HTML "define" both walk
/// shadow trees).
pub fn custom_candidates(doc: &Document, roots: &[NodeId], name: Option<&str>) -> Vec<NodeId> {
    let mut out = Vec::new();
    for &r in roots {
        for n in doc.shadow_including_descendants(r) {
            if let Some(e) = doc.element(n)
                && e.name.ns == ns!(html)
                && name.map_or(e.name.local.contains('-'), |name| *e.name.local == *name)
            {
                out.push(n);
            }
        }
    }
    out
}

/// `getElementsByTagName`: descendants of `root` (not `root`) by qualified
/// name. HTML elements match case-insensitively; `*` matches all elements.
pub fn by_tag(doc: &Document, root: NodeId, name: &str) -> Vec<NodeId> {
    let lower = name.to_ascii_lowercase();
    doc.descendants(root)
        .skip(1)
        .filter(|&n| {
            doc.element(n).is_some_and(|e| {
                name == "*"
                    || if e.name.ns == ns!(html) {
                        *e.name.local == *lower
                    } else {
                        *e.name.local == *name
                    }
            })
        })
        .collect()
}

/// `getElementsByClassName`: descendants of `root` that have every class.
pub fn by_class(doc: &Document, root: NodeId, names: &str) -> Vec<NodeId> {
    let wanted: Vec<&str> = names.split_ascii_whitespace().collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    doc.descendants(root)
        .skip(1)
        .filter(|&n| {
            doc.element(n)
                .is_some_and(|e| wanted.iter().all(|w| e.classes().any(|c| c == *w)))
        })
        .collect()
}

/// Lowercase attribute names on HTML elements, like `setAttribute` does in an
/// HTML document. `None` if the name is not a valid attribute name.
pub fn attr_name(doc: &Document, el: NodeId, name: &str) -> Option<LocalName> {
    if name.is_empty()
        || name.chars().any(|c| {
            c.is_ascii_whitespace()
                || c.is_control()
                || matches!(c, '"' | '\'' | '>' | '/' | '=' | '<')
        })
    {
        return None;
    }
    let html = doc.element(el).is_some_and(|e| e.name.ns == ns!(html));
    Some(if html && name.bytes().any(|b| b.is_ascii_uppercase()) {
        LocalName::from(name.to_ascii_lowercase())
    } else {
        LocalName::from(name)
    })
}

/// Attribute value by qualified name (`xlink:href` finds the parser's
/// namespaced attribute as well as one set by script).
pub fn get_attr<'d>(doc: &'d Document, el: NodeId, name: &str) -> Option<&'d str> {
    let e = doc.element(el)?;
    let lower;
    let name = if e.name.ns == ns!(html) && name.bytes().any(|b| b.is_ascii_uppercase()) {
        lower = name.to_ascii_lowercase();
        &lower
    } else {
        name
    };
    e.attrs
        .iter()
        .find(|a| match &a.name.prefix {
            None => *a.name.local == *name,
            Some(p) => {
                name.len() == p.len() + 1 + a.name.local.len()
                    && name.starts_with(&**p)
                    && name.as_bytes()[p.len()] == b':'
                    && name.ends_with(&*a.name.local)
            }
        })
        .map(|a| &*a.value)
}

/// Attributes as `(qualified name, value)` pairs, in order.
pub fn attr_pairs(doc: &Document, el: NodeId) -> Vec<(String, String)> {
    let Some(e) = doc.element(el) else {
        return Vec::new();
    };
    e.attrs
        .iter()
        .map(|a| {
            let name = match &a.name.prefix {
                Some(p) => format!("{}:{}", &**p, &*a.name.local),
                None => a.name.local.to_string(),
            };
            (name, a.value.to_string())
        })
        .collect()
}

/// DOM `compareDocumentPosition(other)` bits, as seen from `a`.
pub fn compare_position(doc: &Document, a: NodeId, b: NodeId) -> u32 {
    const DISCONNECTED: u32 = 1;
    const PRECEDING: u32 = 2;
    const FOLLOWING: u32 = 4;
    const CONTAINS: u32 = 8;
    const CONTAINED_BY: u32 = 16;
    const IMPLEMENTATION_SPECIFIC: u32 = 32;
    if a == b {
        return 0;
    }
    let path = |n: NodeId| {
        let mut p: Vec<NodeId> = doc.ancestors(n).collect();
        p.reverse();
        p.push(n);
        p
    };
    let (pa, pb) = (path(a), path(b));
    if pa[0] != pb[0] {
        let order = if a < b { FOLLOWING } else { PRECEDING };
        return DISCONNECTED | IMPLEMENTATION_SPECIFIC | order;
    }
    if pb.contains(&a) {
        return CONTAINED_BY | FOLLOWING;
    }
    if pa.contains(&b) {
        return CONTAINS | PRECEDING;
    }
    let i = pa.iter().zip(&pb).take_while(|(x, y)| x == y).count();
    // pa[i] and pb[i] are siblings: whichever comes first precedes.
    let (sa, sb) = (pa[i], pb[i]);
    let mut s = doc.next_sibling(sa);
    while let Some(n) = s {
        if n == sb {
            return FOLLOWING;
        }
        s = doc.next_sibling(n);
    }
    PRECEDING
}

/// The 11 parts the prelude's URL objects expose: href, protocol, username,
/// password, host, hostname, port, pathname, search, hash, origin.
pub fn url_parts(u: &Url) -> [String; 11] {
    use url::quirks as q;
    [
        q::href(u).to_owned(),
        q::protocol(u).to_owned(),
        q::username(u).to_owned(),
        q::password(u).to_owned(),
        q::host(u).to_owned(),
        q::hostname(u).to_owned(),
        q::port(u).to_owned(),
        q::pathname(u).to_owned(),
        q::search(u).to_owned(),
        q::hash(u).to_owned(),
        q::origin(u),
    ]
}

/// Apply a WHATWG URL setter (`url.pathname = ...`). `None` if `part` is
/// unknown or the new href does not parse.
pub fn url_set(u: &Url, part: &str, value: &str) -> Option<Url> {
    use url::quirks as q;
    let mut u = u.clone();
    match part {
        "href" => {
            q::set_href(&mut u, value).ok()?;
        }
        "protocol" => {
            let _ = q::set_protocol(&mut u, value);
        }
        "username" => {
            let _ = q::set_username(&mut u, value);
        }
        "password" => {
            let _ = q::set_password(&mut u, value);
        }
        "host" => {
            let _ = q::set_host(&mut u, value);
        }
        "hostname" => {
            let _ = q::set_hostname(&mut u, value);
        }
        "port" => {
            let _ = q::set_port(&mut u, value);
        }
        "pathname" => q::set_pathname(&mut u, value),
        "search" => q::set_search(&mut u, value),
        "hash" => q::set_hash(&mut u, value),
        _ => return None,
    }
    Some(u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_moves_fragment_children_and_rejects_bad_trees() {
        let mut doc = huginn_dom::parse_html("<div id=a><p id=p></p></div>");
        let a = doc.element_by_id("a").unwrap();
        let p = doc.element_by_id("p").unwrap();
        let frag = doc.create_node(NodeData::DocumentFragment);
        let x = doc.create_html_element("i");
        let y = doc.create_text("y");
        doc.append_child(frag, x);
        doc.append_child(frag, y);
        assert_eq!(insert(&mut doc, a, frag, Some(p)), Ok(vec![x, y]));
        assert_eq!(doc.children(a).collect::<Vec<_>>(), vec![x, y, p]);
        assert_eq!(doc.first_child(frag), None);
        // Cycles, missing references and text under the document are refused.
        assert_eq!(insert(&mut doc, p, a, None), Err(InsertError::Hierarchy));
        let stray = doc.create_html_element("b");
        assert_eq!(
            insert(&mut doc, a, stray, Some(stray)),
            Err(InsertError::NotFound)
        );
        let t = doc.create_text("t");
        assert_eq!(
            insert(&mut doc, NodeId::DOCUMENT, t, None),
            Err(InsertError::Hierarchy)
        );
        // Inserting a node before itself leaves it in place.
        assert_eq!(insert(&mut doc, a, p, Some(p)), Ok(vec![p]));
        assert_eq!(doc.last_child(a), Some(p));
    }

    #[test]
    fn queries_follow_dom_rules() {
        let doc = huginn_dom::parse_html(
            "<div id=r class='a b'><P class='b a'>x</P><svg><foreignObject/></svg><span class=a></span></div>",
        );
        let r = doc.element_by_id("r").unwrap();
        assert_eq!(by_tag(&doc, r, "P").len(), 1);
        assert_eq!(by_tag(&doc, r, "foreignObject").len(), 1);
        assert_eq!(by_tag(&doc, r, "foreignobject").len(), 0);
        assert_eq!(by_tag(&doc, r, "*").len(), 4);
        // Root excluded, all classes required, order irrelevant.
        assert_eq!(by_class(&doc, r, "a b").len(), 1);
        assert_eq!(by_class(&doc, NodeId::DOCUMENT, " a ").len(), 3);
        assert_eq!(get_attr(&doc, r, "CLASS"), Some("a b"));
        assert!(attr_name(&doc, r, "data-X").is_some_and(|n| &*n == "data-x"));
        assert!(attr_name(&doc, r, "a b").is_none());
    }

    #[test]
    fn document_position_bits() {
        let doc = huginn_dom::parse_html("<div id=a><i id=b></i></div><p id=c></p>");
        let (a, b, c) = (
            doc.element_by_id("a").unwrap(),
            doc.element_by_id("b").unwrap(),
            doc.element_by_id("c").unwrap(),
        );
        assert_eq!(compare_position(&doc, a, b), 16 | 4);
        assert_eq!(compare_position(&doc, b, a), 8 | 2);
        assert_eq!(compare_position(&doc, b, c), 4);
        assert_eq!(compare_position(&doc, c, b), 2);
    }

    #[test]
    fn url_parts_and_setters() {
        let u = Url::parse("http://127.0.0.1:8114/examples/react/dist/?q=1#/active").unwrap();
        let p = url_parts(&u);
        assert_eq!(
            &p[1..],
            [
                "http:",
                "",
                "",
                "127.0.0.1:8114",
                "127.0.0.1",
                "8114",
                "/examples/react/dist/",
                "?q=1",
                "#/active",
                "http://127.0.0.1:8114"
            ]
        );
        let v = url_set(&u, "hash", "x").unwrap();
        assert_eq!(
            v.as_str(),
            "http://127.0.0.1:8114/examples/react/dist/?q=1#x"
        );
    }
}
