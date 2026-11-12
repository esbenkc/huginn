//! Native operations behind the prelude's `N` object. Arguments are node ids
//! (raw `NodeId` numbers, 0 = null), strings and numbers; the DOM rules are
//! in `dom_ops.rs`. Every op catches Rust panics and turns them into a JS
//! exception, because unwinding through V8 frames is not allowed.

use std::panic::{AssertUnwindSafe, catch_unwind};

use huginn_dom::{Document, LocalName, Namespace, NodeData, NodeId, QualName, local_name, ns};
use huginn_net::{ResourceKind, Url};

use super::{SharedState, run_classic};
use crate::activation::{self, EventSink};
use crate::dom_ops::{self, InsertError};
use crate::{HostContext, HostRequest, ScriptKind, forms, inline_source, script_kind};

type Rv<'a> = v8::ReturnValue<'a, v8::Value>;
type Args<'a> = v8::FunctionCallbackArguments<'a>;

pub(super) fn state(scope: &v8::PinScope<'_, '_>) -> SharedState {
    scope
        .get_slot::<SharedState>()
        .expect("huginn-js state slot")
        .clone()
}

/// The shell's context for the running engine call.
///
/// The returned reference is valid until the engine call returns. Callers
/// must not keep it across a call back into JavaScript (which may reach
/// another op).
pub(super) fn host<'a>(scope: &v8::PinScope<'_, '_>) -> Option<&'a mut HostContext<'static>> {
    let ptr = state(scope).borrow().cx;
    // SAFETY: `cx` is set by `Call::enter` from a live `&mut HostContext` and
    // cleared when the call ends; JS (and so this op) only runs in between.
    unsafe { ptr.as_mut() }
}

fn throw(scope: &mut v8::PinScope<'_, '_>, msg: &str) {
    if let Some(m) = v8::String::new(scope, msg) {
        let e = v8::Exception::error(scope, m);
        scope.throw_exception(e);
    }
}

fn arg_u32(scope: &mut v8::PinScope<'_, '_>, args: &Args<'_>, i: i32) -> u32 {
    let v = args.get(i);
    if v.is_uint32() {
        v.uint32_value(scope).unwrap_or(0)
    } else if v.is_number() {
        v.number_value(scope)
            .map_or(0, |f| if f > 0.0 { f as u32 } else { 0 })
    } else {
        0
    }
}

fn arg_node(
    scope: &mut v8::PinScope<'_, '_>,
    args: &Args<'_>,
    i: i32,
    doc: &Document,
) -> Option<NodeId> {
    let id = NodeId::from_raw(arg_u32(scope, args, i))?;
    (id.index() < doc.len()).then_some(id)
}

fn arg_str(scope: &mut v8::PinScope<'_, '_>, args: &Args<'_>, i: i32) -> String {
    args.get(i).to_rust_string_lossy(scope)
}

fn set_str(scope: &mut v8::PinScope<'_, '_>, rv: &mut Rv<'_>, s: &str) {
    match v8::String::new(scope, s) {
        Some(v) => rv.set(v.into()),
        None => rv.set_empty_string(),
    }
}

fn set_ids(scope: &mut v8::PinScope<'_, '_>, rv: &mut Rv<'_>, ids: &[NodeId]) {
    let elems: Vec<v8::Local<v8::Value>> = ids
        .iter()
        .map(|n| v8::Integer::new_from_unsigned(scope, n.to_raw()).into())
        .collect();
    rv.set(v8::Array::new_with_elements(scope, &elems).into());
}

fn set_strs(scope: &mut v8::PinScope<'_, '_>, rv: &mut Rv<'_>, items: &[&str]) {
    let elems: Vec<v8::Local<v8::Value>> = items
        .iter()
        .map(|s| match v8::String::new(scope, s) {
            Some(v) => v.into(),
            None => v8::String::empty(scope).into(),
        })
        .collect();
    rv.set(v8::Array::new_with_elements(scope, &elems).into());
}

fn set_node(rv: &mut Rv<'_>, n: Option<NodeId>) {
    rv.set_uint32(n.map_or(0, NodeId::to_raw));
}

/// Define an op: a native callback with panic protection.
macro_rules! op {
    ($name:ident($scope:ident, $args:ident, $rv:ident) $body:block) => {
        #[allow(unused_variables, unused_mut)]
        fn $name(
            $scope: &mut v8::PinScope,
            $args: v8::FunctionCallbackArguments,
            mut $rv: v8::ReturnValue<v8::Value>,
        ) {
            let r = catch_unwind(AssertUnwindSafe(|| $body));
            if r.is_err() {
                throw(
                    $scope,
                    concat!("huginn internal error in ", stringify!($name)),
                );
            }
        }
    };
}

/// An op that maps one node to an optional node.
macro_rules! node_op {
    ($name:ident, |$doc:ident, $n:ident| $e:expr) => {
        op!($name(scope, args, rv) {
            let Some(cx) = host(scope) else { return };
            let $doc: &Document = cx.document;
            let r: Option<NodeId> = match arg_node(scope, &args, 0, $doc) {
                Some($n) => $e,
                None => None,
            };
            set_node(&mut rv, r);
        });
    };
}

node_op!(op_parent, |doc, n| doc.parent(n));
node_op!(op_first, |doc, n| doc.first_child(n));
node_op!(op_last, |doc, n| doc.last_child(n));
node_op!(op_next, |doc, n| doc.next_sibling(n));
node_op!(op_prev, |doc, n| doc.prev_sibling(n));
node_op!(op_tmpl, |doc, n| doc
    .element(n)
    .and_then(|e| e.template_contents));
node_op!(op_shadow, |doc, n| doc.shadow_root(n));
node_op!(op_shadow_host, |doc, n| doc.shadow_host(n));

// `attachShadow`: a new shadow root, or the host's declarative one emptied
// (DOM, "attach a shadow root", step 4). 0 if the element cannot host one.
op!(op_attach_shadow(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let Some(h) = arg_node(scope, &args, 0, cx.document) else { return };
    let root = match cx.document.shadow_root(h) {
        Some(r) => {
            cx.document.remove_children(r);
            Some(r)
        }
        None => cx.document.attach_shadow(h),
    };
    set_node(&mut rv, root);
});

op!(op_type(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let t = arg_node(scope, &args, 0, cx.document).map_or(0, |n| dom_ops::node_type(cx.document, n));
    rv.set_uint32(t);
});

op!(op_kids(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let ids: Vec<NodeId> = match arg_node(scope, &args, 0, cx.document) {
        Some(n) => cx.document.children(n).collect(),
        None => Vec::new(),
    };
    set_ids(scope, &mut rv, &ids);
});

node_op!(op_assigned_slot, |doc, n| doc.assigned_slot(n));

// `slot.assignedNodes()`: the host children assigned to this slot.
op!(op_assigned(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let doc: &Document = cx.document;
    let ids: Vec<NodeId> = match arg_node(scope, &args, 0, doc) {
        Some(slot) => match doc.containing_shadow_root(slot).and_then(|r| doc.shadow_host(r)) {
            Some(h) => doc.children(h).filter(|&c| doc.assigned_slot(c) == Some(slot)).collect(),
            None => Vec::new(),
        },
        None => Vec::new(),
    };
    set_ids(scope, &mut rv, &ids);
});

op!(op_ekids(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let doc: &Document = cx.document;
    let ids: Vec<NodeId> = match arg_node(scope, &args, 0, doc) {
        Some(n) => doc.children(n).filter(|&c| doc.element(c).is_some()).collect(),
        None => Vec::new(),
    };
    set_ids(scope, &mut rv, &ids);
});

op!(op_name(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let Some(n) = arg_node(scope, &args, 0, cx.document) else { return set_str(scope, &mut rv, "") };
    let name: &str = match &cx.document.node(n).data {
        NodeData::Element(e) => &e.name.local,
        NodeData::Doctype { name, .. } => name,
        NodeData::ProcessingInstruction { target, .. } => target,
        _ => "",
    };
    set_str(scope, &mut rv, name);
});

op!(op_ns(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let ns = arg_node(scope, &args, 0, cx.document)
        .and_then(|n| cx.document.element(n))
        .map_or("", |e| &*e.name.ns);
    set_str(scope, &mut rv, ns);
});

op!(op_is_html(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let html = arg_node(scope, &args, 0, cx.document)
        .and_then(|n| cx.document.element(n))
        .is_some_and(|e| e.name.ns == ns!(html));
    rv.set_bool(html);
});

op!(op_data(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let Some(n) = arg_node(scope, &args, 0, cx.document) else { return set_str(scope, &mut rv, "") };
    let s: &str = match &cx.document.node(n).data {
        NodeData::Text(t) | NodeData::Comment(t) => t,
        NodeData::ProcessingInstruction { data, .. } => data,
        _ => "",
    };
    set_str(scope, &mut rv, s);
});

op!(op_set_data(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let text = arg_str(scope, &args, 1);
    if let Some(n) = arg_node(scope, &args, 0, cx.document)
        && matches!(cx.document.node(n).data, NodeData::Text(_) | NodeData::Comment(_))
    {
        cx.document.set_character_data(n, &text);
    }
});

op!(op_text(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let s = match arg_node(scope, &args, 0, cx.document) {
        Some(n) => cx.document.text_content(n),
        None => String::new(),
    };
    set_str(scope, &mut rv, &s);
});

op!(op_set_text(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let text = arg_str(scope, &args, 1);
    if let Some(n) = arg_node(scope, &args, 0, cx.document) {
        cx.document.set_text_content(n, &text);
    }
});

// shadowRoot.adoptedStyleSheets: the joined CSS of the adopted sheets.
op!(op_set_adopted(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let css = arg_str(scope, &args, 1);
    if let Some(root) = arg_node(scope, &args, 0, cx.document) {
        cx.document.set_adopted_css(root, &css);
    }
});

op!(op_attr(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let name = arg_str(scope, &args, 1);
    match arg_node(scope, &args, 0, cx.document).and_then(|n| dom_ops::get_attr(cx.document, n, &name)) {
        Some(v) => set_str(scope, &mut rv, v),
        None => rv.set_null(),
    }
});

op!(op_set_attr(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let name = arg_str(scope, &args, 1);
    let value = arg_str(scope, &args, 2);
    let Some(n) = arg_node(scope, &args, 0, cx.document) else { return };
    if cx.document.element(n).is_none() {
        return;
    }
    match dom_ops::attr_name(cx.document, n, &name) {
        Some(local) => {
            // Skip no-op writes: they would still invalidate style.
            if dom_ops::get_attr(cx.document, n, &local) != Some(value.as_str()) {
                cx.document.set_attribute(n, local, &value);
            }
            rv.set_uint32(0);
        }
        None => rv.set_uint32(1),
    }
});

op!(op_del_attr(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let name = arg_str(scope, &args, 1);
    let removed = match arg_node(scope, &args, 0, cx.document) {
        Some(n) => match dom_ops::attr_name(cx.document, n, &name) {
            Some(local) => cx.document.remove_attribute(n, &local),
            None => false,
        },
        None => false,
    };
    rv.set_bool(removed);
});

op!(op_attrs(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let pairs = match arg_node(scope, &args, 0, cx.document) {
        Some(n) => dom_ops::attr_pairs(cx.document, n),
        None => Vec::new(),
    };
    let flat: Vec<&str> = pairs.iter().flat_map(|(k, v)| [k.as_str(), v.as_str()]).collect();
    set_strs(scope, &mut rv, &flat);
});

op!(op_create(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let name = arg_str(scope, &args, 0);
    let ns = arg_str(scope, &args, 1);
    let n = cx.document.create_element(
        QualName::new(None, Namespace::from(ns), LocalName::from(name)),
        Vec::new(),
    );
    set_node(&mut rv, Some(n));
});

op!(op_create_text(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let text = arg_str(scope, &args, 0);
    set_node(&mut rv, Some(cx.document.create_text(&text)));
});

op!(op_create_comment(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let text = arg_str(scope, &args, 0);
    set_node(&mut rv, Some(cx.document.create_comment(&text)));
});

op!(op_create_frag(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    set_node(&mut rv, Some(cx.document.create_node(NodeData::DocumentFragment)));
});

/// After nodes were inserted: run scripts that became connected (inline ones
/// now, external ones queued), and return custom-element candidates.
fn after_insert(
    scope: &mut v8::PinScope<'_, '_>,
    rv: &mut Rv<'_>,
    parent: NodeId,
    inserted: &[NodeId],
) {
    let st = state(scope);
    let (scripts, candidates) = {
        let Some(cx) = host(scope) else { return };
        let doc: &Document = cx.document;
        if !dom_ops::is_connected(doc, parent) {
            return;
        }
        let mut s = st.borrow_mut();
        let started = &s.started;
        let scripts = dom_ops::scripts_in(doc, inserted, |n| started.contains(&n));
        for &n in &scripts {
            s.started.insert(n);
        }
        let candidates = if s.custom_elements {
            dom_ops::custom_candidates(doc, inserted, None)
        } else {
            Vec::new()
        };
        (scripts, candidates)
    };
    for n in scripts {
        start_script(scope, &st, n);
    }
    if !candidates.is_empty() {
        set_ids(scope, rv, &candidates);
    }
}

/// A script element just got connected: inline classic scripts run now,
/// everything else is queued for the next timer round.
fn start_script(scope: &mut v8::PinScope<'_, '_>, st: &SharedState, node: NodeId) {
    let (source, url, kind) = {
        let Some(cx) = host(scope) else { return };
        let doc: &Document = cx.document;
        let Some(kind) = doc.element(node).and_then(script_kind) else {
            return;
        };
        if doc.attribute(node, &local_name!("src")).is_some() || kind == ScriptKind::Module {
            st.borrow_mut().pending_scripts.push_back(node);
            return;
        }
        (inline_source(doc, node), cx.document_url.to_string(), kind)
    };
    debug_assert_eq!(kind, ScriptKind::Classic);
    let prev = st.borrow_mut().current_script.replace(node);
    if let Err(e) = run_classic(scope, &source, &url) {
        st.borrow_mut().log_error(&e.to_string());
    }
    st.borrow_mut().current_script = prev;
}

/// Fetch (same origin only) and run an external or module script inserted
/// by script. Returns false if it could not be loaded or failed to compile.
pub(super) fn run_external_script(
    scope: &mut v8::PinScope<'_, '_>,
    st: &SharedState,
    node: NodeId,
) -> bool {
    let (source, url, kind) = {
        let Some(cx) = host(scope) else { return false };
        let doc: &Document = cx.document;
        let Some(kind) = doc.element(node).and_then(script_kind) else {
            return false;
        };
        match doc.attribute(node, &local_name!("src")) {
            Some(src) => {
                let Some(url) = huginn_net::resolve_url(cx.base_url, src) else {
                    return false;
                };
                if !dom_ops::fetch_allowed(cx.document_url, &url) {
                    return false;
                }
                match cx.loader.fetch(&url, ResourceKind::Script) {
                    Ok(r) => (r.text().into_owned(), r.url.to_string(), kind),
                    Err(_) => return false,
                }
            }
            None => (inline_source(doc, node), cx.document_url.to_string(), kind),
        }
    };
    let prev = st.borrow_mut().current_script.replace(node);
    let r = match kind {
        ScriptKind::Classic => run_classic(scope, &source, &url),
        ScriptKind::Module => super::modules::run_module(scope, st, &source, &url),
    };
    st.borrow_mut().current_script = prev;
    match r {
        Ok(()) => true,
        Err(e) => {
            st.borrow_mut().log_error(&e.to_string());
            !matches!(e, crate::ScriptError::Compile { .. })
        }
    }
}

op!(op_insert(scope, args, rv) {
    let r = {
        let Some(cx) = host(scope) else { return };
        let doc = &mut *cx.document;
        let (Some(parent), Some(child)) = (arg_node(scope, &args, 0, doc), arg_node(scope, &args, 1, doc)) else {
            rv.set_uint32(1);
            return;
        };
        let reference = arg_node(scope, &args, 2, doc);
        if args.get(2).is_number() && arg_u32(scope, &args, 2) != 0 && reference.is_none() {
            rv.set_uint32(2);
            return;
        }
        dom_ops::insert(doc, parent, child, reference).map(|nodes| (parent, nodes))
    };
    match r {
        Ok((parent, nodes)) => after_insert(scope, &mut rv, parent, &nodes),
        Err(InsertError::Hierarchy) => rv.set_uint32(1),
        Err(InsertError::NotFound) => rv.set_uint32(2),
    }
});

op!(op_remove(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    if let Some(n) = arg_node(scope, &args, 0, cx.document) {
        cx.document.detach(n);
    }
});

op!(op_html(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let s = arg_node(scope, &args, 0, cx.document).map_or(String::new(), |n| huginn_dom::serialize(cx.document, n, false));
    set_str(scope, &mut rv, &s);
});

op!(op_outer(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let s = arg_node(scope, &args, 0, cx.document).map_or(String::new(), |n| huginn_dom::serialize(cx.document, n, true));
    set_str(scope, &mut rv, &s);
});

/// Parse a fragment; its scripts are inert (`innerHTML` never runs scripts).
fn parse_inert(st: &SharedState, doc: &mut Document, context: NodeId, html: &str) -> Vec<NodeId> {
    let nodes = huginn_dom::parse_fragment(doc, context, html);
    let scripts = dom_ops::scripts_in(doc, &nodes, |_| false);
    st.borrow_mut().started.extend(scripts);
    nodes
}

op!(op_set_html(scope, args, rv) {
    let st = state(scope);
    let candidates = {
        let Some(cx) = host(scope) else { return };
        let html = arg_str(scope, &args, 1);
        let doc = &mut *cx.document;
        let Some(n) = arg_node(scope, &args, 0, doc) else { return };
        // <template>.innerHTML fills the template contents.
        let target = doc.element(n).and_then(|e| e.template_contents).unwrap_or(n);
        doc.remove_children(target);
        let nodes = parse_inert(&st, doc, n, &html);
        for &c in &nodes {
            doc.append_child(target, c);
        }
        if st.borrow().custom_elements && dom_ops::is_connected(doc, target) {
            dom_ops::custom_candidates(doc, &nodes, None)
        } else {
            Vec::new()
        }
    };
    if !candidates.is_empty() {
        set_ids(scope, &mut rv, &candidates);
    }
});

op!(op_frag(scope, args, rv) {
    let st = state(scope);
    let Some(cx) = host(scope) else { return };
    let html = arg_str(scope, &args, 1);
    let doc = &mut *cx.document;
    let context = arg_node(scope, &args, 0, doc).or_else(|| doc.body()).unwrap_or(NodeId::DOCUMENT);
    let nodes = parse_inert(&st, doc, context, &html);
    set_ids(scope, &mut rv, &nodes);
});

op!(op_clone(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let deep = args.get(1).boolean_value(scope);
    let r = arg_node(scope, &args, 0, cx.document).map(|n| cx.document.clone_node(n, deep));
    set_node(&mut rv, r);
});

op!(op_qs(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let sel = arg_str(scope, &args, 1);
    let Some(root) = arg_node(scope, &args, 0, cx.document) else { return rv.set_uint32(0) };
    match huginn_style::query_selector(cx.document, root, &sel) {
        Ok(n) => set_node(&mut rv, n),
        Err(_) => rv.set_int32(-1),
    }
});

op!(op_qsa(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let sel = arg_str(scope, &args, 1);
    let Some(root) = arg_node(scope, &args, 0, cx.document) else { return set_ids(scope, &mut rv, &[]) };
    match huginn_style::query_selector_all(cx.document, root, &sel) {
        Ok(ids) => set_ids(scope, &mut rv, &ids),
        Err(_) => rv.set_null(),
    }
});

op!(op_matches(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let sel = arg_str(scope, &args, 1);
    let Some(n) = arg_node(scope, &args, 0, cx.document) else { return rv.set_bool(false) };
    match huginn_style::matches_selector(cx.document, n, &sel) {
        Ok(b) => rv.set_bool(b),
        Err(_) => rv.set_null(),
    }
});

op!(op_closest(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let sel = arg_str(scope, &args, 1);
    let Some(n) = arg_node(scope, &args, 0, cx.document) else { return rv.set_uint32(0) };
    match huginn_style::closest(cx.document, n, &sel) {
        Ok(r) => set_node(&mut rv, r),
        Err(_) => rv.set_int32(-1),
    }
});

op!(op_by_id(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let id = arg_str(scope, &args, 0);
    set_node(&mut rv, if id.is_empty() { None } else { cx.document.element_by_id(&id) });
});

op!(op_by_tag(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let name = arg_str(scope, &args, 1);
    let ids = arg_node(scope, &args, 0, cx.document).map_or(Vec::new(), |r| dom_ops::by_tag(cx.document, r, &name));
    set_ids(scope, &mut rv, &ids);
});

op!(op_by_class(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let names = arg_str(scope, &args, 1);
    let ids = arg_node(scope, &args, 0, cx.document).map_or(Vec::new(), |r| dom_ops::by_class(cx.document, r, &names));
    set_ids(scope, &mut rv, &ids);
});

op!(op_doc_el(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    set_node(&mut rv, cx.document.document_element());
});

op!(op_head(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    set_node(&mut rv, cx.document.head());
});

op!(op_body(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    set_node(&mut rv, cx.document.body());
});

op!(op_contains(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let doc: &Document = cx.document;
    let r = match (arg_node(scope, &args, 0, doc), arg_node(scope, &args, 1, doc)) {
        (Some(a), Some(b)) => dom_ops::contains(doc, a, b),
        _ => false,
    };
    rv.set_bool(r);
});

op!(op_cmp(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let doc: &Document = cx.document;
    let r = match (arg_node(scope, &args, 0, doc), arg_node(scope, &args, 1, doc)) {
        (Some(a), Some(b)) => dom_ops::compare_position(doc, a, b),
        _ => 1,
    };
    rv.set_uint32(r);
});

op!(op_connected(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let r = arg_node(scope, &args, 0, cx.document).is_some_and(|n| dom_ops::is_connected(cx.document, n));
    rv.set_bool(r);
});

op!(op_quirks(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    rv.set_bool(cx.document.quirks_mode() == huginn_dom::QuirksMode::Quirks);
});

op!(op_geom(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let Some(n) = arg_node(scope, &args, 0, cx.document) else { return rv.set_null() };
    let doc: &Document = cx.document;
    let g = match cx.layout.as_deref_mut() {
        Some(l) if dom_ops::is_connected(doc, n) && doc.element(n).is_some() => l.geometry(doc, n),
        _ => None,
    };
    match g {
        Some(g) => {
            let vals = [
                g.x,
                g.y,
                g.width,
                g.height,
                g.border[0],
                g.border[1],
                g.border[2],
                g.border[3],
                g.scroll_size[0],
                g.scroll_size[1],
            ];
            let elems: Vec<v8::Local<v8::Value>> = vals.iter().map(|&v| v8::Number::new(scope, f64::from(v)).into()).collect();
            rv.set(v8::Array::new_with_elements(scope, &elems).into());
        }
        None => rv.set_null(),
    }
});

// Element scroll offset: [left, top].
op!(op_scroll_pos(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let Some(n) = arg_node(scope, &args, 0, cx.document) else { return rv.set_null() };
    let (x, y) = cx.document.scroll_offset(n);
    let elems = [v8::Number::new(scope, f64::from(x)).into(), v8::Number::new(scope, f64::from(y)).into()];
    rv.set(v8::Array::new_with_elements(scope, &elems).into());
});

// Set an element's scroll offset (already clamped by the caller).
op!(op_set_scroll_pos(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let Some(n) = arg_node(scope, &args, 0, cx.document) else { return };
    let x = args.get(1).number_value(scope).filter(|v| v.is_finite()).unwrap_or(0.0) as f32;
    let y = args.get(2).number_value(scope).filter(|v| v.is_finite()).unwrap_or(0.0) as f32;
    cx.document.set_scroll_offset(n, x, y);
});

op!(op_cstyle(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let prop = arg_str(scope, &args, 1);
    let Some(n) = arg_node(scope, &args, 0, cx.document) else { return set_str(scope, &mut rv, "") };
    let doc: &Document = cx.document;
    let v = match cx.layout.as_deref_mut() {
        Some(l) if doc.element(n).is_some() => l.computed_value(doc, n, &prop),
        _ => String::new(),
    };
    set_str(scope, &mut rv, &v);
});

op!(op_now(scope, args, rv) {
    let t = state(scope).borrow_mut().now();
    rv.set_double(t);
});

// The time the current engine call started at: timers are scheduled from it,
// so their order does not depend on how fast the script ran.
op!(op_task_now(scope, args, rv) {
    let t = state(scope).borrow().call_base;
    rv.set_double(t);
});

op!(op_log(scope, args, rv) {
    let level = arg_u32(scope, &args, 0);
    let msg = arg_str(scope, &args, 1);
    let st = state(scope);
    let console = st.borrow().options.console;
    if msg.starts_with("Uncaught ") {
        st.borrow_mut().log_error(&msg);
    } else if console {
        let tag = ["debug", "log", "warn", "error"].get(level as usize).copied().unwrap_or("log");
        eprintln!("huginn: console.{tag}: {}", msg.chars().take(2000).collect::<String>());
    }
});

op!(op_fetch(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let href = arg_str(scope, &args, 0);
    let method = arg_str(scope, &args, 1);
    let Some(url) = huginn_net::resolve_url(cx.base_url, &href) else { return rv.set_null() };
    if !(method == "GET" || method == "HEAD") || !dom_ops::fetch_allowed(cx.document_url, &url) {
        return rv.set_null();
    }
    match cx.loader.fetch(&url, ResourceKind::Other) {
        Ok(r) => {
            let status = r.status.to_string();
            let body = if method == "HEAD" { String::new() } else { r.text().into_owned() };
            set_strs(scope, &mut rv, &[&status, r.url.as_str(), r.content_type.as_deref().unwrap_or(""), &body]);
        }
        Err(huginn_net::NetError::Http { status, .. }) => {
            let status = status.to_string();
            set_strs(scope, &mut rv, &[&status, url.as_str(), "", ""]);
        }
        Err(_) => rv.set_null(),
    }
});

// Parse `href` against `base` ("" = the document base; "about:invalid-base"
// = no base) and return the 11 URL parts, or null.
op!(op_url(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let href = arg_str(scope, &args, 0);
    let base = arg_str(scope, &args, 1);
    let parsed = match base.as_str() {
        "" => cx.base_url.join(&href).ok(),
        "about:invalid-base" => Url::parse(&href).ok(),
        b => Url::parse(b).ok().and_then(|b| b.join(&href).ok()),
    };
    match parsed {
        Some(u) => {
            let parts = dom_ops::url_parts(&u);
            let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
            set_strs(scope, &mut rv, &refs);
        }
        None => rv.set_null(),
    }
});

op!(op_url_set(scope, args, rv) {
    let href = arg_str(scope, &args, 0);
    let part = arg_str(scope, &args, 1);
    let value = arg_str(scope, &args, 2);
    match Url::parse(&href).ok().and_then(|u| dom_ops::url_set(&u, &part, &value)) {
        Some(u) => {
            let parts = dom_ops::url_parts(&u);
            let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
            set_strs(scope, &mut rv, &refs);
        }
        None => rv.set_null(),
    }
});

op!(op_doc_url(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let u = cx.document_url.to_string();
    set_str(scope, &mut rv, &u);
});

op!(op_base_url(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let u = cx.base_url.to_string();
    set_str(scope, &mut rv, &u);
});

op!(op_cur_script(scope, args, rv) {
    let n = state(scope).borrow().current_script;
    set_node(&mut rv, n);
});

// `document.write`: parse the markup and insert it after the running
// script (or at the end of <body>). Scripts in it run, like in a browser.
op!(op_write(scope, args, rv) {
    let st = state(scope);
    let (parent, nodes) = {
        let Some(cx) = host(scope) else { return };
        let html = arg_str(scope, &args, 0);
        let doc = &mut *cx.document;
        let cur = st.borrow().current_script.filter(|&s| dom_ops::is_connected(doc, s));
        let in_body = |doc: &Document, p: NodeId| doc.element(p).is_some_and(|e| !e.is_html(&local_name!("head")));
        let (parent, before) = match cur.and_then(|s| doc.parent(s).map(|p| (p, s))) {
            Some((p, s)) if in_body(doc, p) => (p, doc.next_sibling(s)),
            _ => match doc.body() {
                Some(b) => (b, None),
                None => return,
            },
        };
        let nodes = huginn_dom::parse_fragment(doc, parent, &html);
        for &n in &nodes {
            doc.insert_before(parent, n, before);
        }
        (parent, nodes)
    };
    after_insert(scope, &mut rv, parent, &nodes);
});

// Custom-element candidates in a node's shadow-including subtree; with a
// second (string) argument, only the elements with that local name.
op!(op_custom_in(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let name = args.get(1).is_string().then(|| arg_str(scope, &args, 1));
    let ids = arg_node(scope, &args, 0, cx.document)
        .map_or(Vec::new(), |n| dom_ops::custom_candidates(cx.document, &[n], name.as_deref()));
    set_ids(scope, &mut rv, &ids);
});

op!(op_define_ce(scope, args, rv) {
    state(scope).borrow_mut().custom_elements = true;
});

op!(op_viewport(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let (w, h) = cx.viewport;
    let elems = [v8::Number::new(scope, f64::from(w)).into(), v8::Number::new(scope, f64::from(h)).into()];
    rv.set(v8::Array::new_with_elements(scope, &elems).into());
});

/// Activation algorithms run from scripts: events go straight into the
/// running context, requests into the engine state.
struct ScopeSink<'a, 's, 'i> {
    scope: &'a mut v8::PinScope<'s, 'i>,
    state: SharedState,
}

impl EventSink for ScopeSink<'_, '_, '_> {
    fn document(&mut self) -> &mut Document {
        host(self.scope)
            .expect("ops run inside an engine call")
            .document
    }

    fn fire(&mut self, target: crate::EventTarget, event: &crate::Event) -> bool {
        match super::dispatch_js(self.scope, &self.state, target, event) {
            Ok(ok) => ok,
            Err(e) => {
                self.state.borrow_mut().log_error(&e.to_string());
                true
            }
        }
    }

    fn request(&mut self, request: HostRequest) {
        self.state.borrow_mut().requests.push(request);
    }

    fn base_url(&self) -> Url {
        host(self.scope)
            .expect("ops run inside an engine call")
            .base_url
            .clone()
    }

    fn document_url(&self) -> Url {
        host(self.scope)
            .expect("ops run inside an engine call")
            .document_url
            .clone()
    }
}

// `el.click()`: an untrusted click and the activation behavior it reaches.
op!(op_click(scope, args, rv) {
    let st = state(scope);
    let Some(n) = host(scope).and_then(|cx| arg_node(scope, &args, 0, cx.document)) else { return };
    if st.borrow().clicking.contains(&n) {
        return;
    }
    st.borrow_mut().clicking.push(n);
    let ev = activation::click_event(Vec::new(), false);
    activation::click(&mut ScopeSink { scope, state: st.clone() }, n, &ev);
    st.borrow_mut().clicking.retain(|&c| c != n);
});

// `form.requestSubmit(submitter)` (submit event first) and `form.submit()`
// (third argument true: no event).
op!(op_submit(scope, args, rv) {
    let st = state(scope);
    let Some(cx) = host(scope) else { return };
    let Some(form) = arg_node(scope, &args, 0, cx.document) else { return };
    let submitter = arg_node(scope, &args, 1, cx.document);
    let from_method = args.get(2).is_true();
    activation::submit(&mut ScopeSink { scope, state: st }, form, submitter, from_method);
});

// `location.href = url` and friends: ask the browser to navigate.
op!(op_navigate(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let href = arg_str(scope, &args, 0);
    let replace = args.get(1).is_true();
    if let Some(url) = huginn_net::resolve_url(cx.base_url, &href) {
        state(scope).borrow_mut().requests.push(HostRequest::Navigate { url, post: None, new_tab: false, replace });
    }
});

// `el.focus()` (node) and `el.blur()` (0): move focus, firing focus events.
op!(op_focus(scope, args, rv) {
    let st = state(scope);
    let Some(cx) = host(scope) else { return };
    let to = arg_node(scope, &args, 0, cx.document);
    if let Some(n) = to
        && !(forms::is_focusable(cx.document, n) && dom_ops::is_connected(cx.document, n))
    {
        return;
    }
    let visible = to.is_some_and(|n| forms::is_text_control(cx.document, n));
    activation::set_focus(&mut ScopeSink { scope, state: st }, to, visible);
});

op!(op_focused(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    set_node(&mut rv, cx.document.interaction().focus);
});

op!(op_disabled(scope, args, rv) {
    let Some(cx) = host(scope) else { return };
    let r = arg_node(scope, &args, 0, cx.document).is_some_and(|n| forms::is_disabled(cx.document, n));
    rv.set_bool(r);
});

/// Build the `N` object with every op.
pub(super) fn install_ops<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Object> {
    let obj = v8::Object::new(scope);
    macro_rules! install {
        ($($name:literal => $f:ident),* $(,)?) => {
            $(
                if let (Some(key), Some(func)) = (v8::String::new(scope, $name), v8::Function::new(scope, $f)) {
                    obj.set(scope, key.into(), func.into());
                }
            )*
        };
    }
    install! {
        "type" => op_type, "parent" => op_parent, "first" => op_first, "last" => op_last,
        "next" => op_next, "prev" => op_prev, "kids" => op_kids, "ekids" => op_ekids,
        "name" => op_name, "ns" => op_ns, "isHtml" => op_is_html, "data" => op_data,
        "setData" => op_set_data, "text" => op_text, "setText" => op_set_text, "attr" => op_attr,
        "setAttr" => op_set_attr, "delAttr" => op_del_attr, "attrs" => op_attrs, "create" => op_create,
        "createText" => op_create_text, "createComment" => op_create_comment, "createFrag" => op_create_frag,
        "insert" => op_insert, "remove" => op_remove, "html" => op_html, "outer" => op_outer,
        "setHtml" => op_set_html, "frag" => op_frag, "clone" => op_clone, "qs" => op_qs, "qsa" => op_qsa,
        "matches" => op_matches, "closest" => op_closest, "byId" => op_by_id, "byTag" => op_by_tag,
        "byClass" => op_by_class, "docEl" => op_doc_el, "head" => op_head, "body" => op_body,
        "contains" => op_contains, "cmp" => op_cmp, "connected" => op_connected, "quirks" => op_quirks,
        "geom" => op_geom, "scrollPos" => op_scroll_pos, "setScrollPos" => op_set_scroll_pos, "cstyle" => op_cstyle, "now" => op_now, "taskNow" => op_task_now, "log" => op_log,
        "fetch" => op_fetch,
        "url" => op_url, "urlSet" => op_url_set, "docUrl" => op_doc_url, "baseUrl" => op_base_url,
        "curScript" => op_cur_script, "write" => op_write, "customIn" => op_custom_in,
        "defineCE" => op_define_ce, "tmpl" => op_tmpl, "viewport" => op_viewport,
        "shadow" => op_shadow, "shadowHost" => op_shadow_host, "attachShadow" => op_attach_shadow,
        "assignedSlot" => op_assigned_slot, "assigned" => op_assigned, "setAdopted" => op_set_adopted,
        "click" => op_click, "submit" => op_submit, "navigate" => op_navigate, "focus" => op_focus,
        "focused" => op_focused, "disabled" => op_disabled,
    }
    obj
}
