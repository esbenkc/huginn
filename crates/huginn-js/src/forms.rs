//! HTML form controls and activation behavior on the DOM: what a click on an
//! element does after its `click` event (follow a link, toggle a checkbox,
//! submit a form), and how a form becomes a navigation. Engine-independent:
//! the native ops (`click()`, `form.submit()`) and the browser's input
//! handling use the same functions, with or without a script engine.
//!
//! Form state lives in attributes, like the rest of huginn-js: a control's value
//! is its `value` attribute (a `<textarea>`'s is its text), checkedness is
//! `checked`, and an option is selected when it has `selected`.

use huginn_dom::{Document, ElementData, NodeId, local_name, ns};
use huginn_net::Url;

/// What activating an element does (HTML "activation behavior").
#[derive(Clone, Debug, PartialEq)]
pub enum Activation {
    /// A checkbox or radio button: checkedness changed before the click
    /// event; `input` and `change` follow.
    Checkable,
    /// A submit button (or image button): submit its form.
    Submit { form: NodeId },
    /// A reset button.
    Reset { form: NodeId },
    /// A link with a valid `href`.
    Link { url: Url, new_tab: bool },
    /// A `<label>`: click its labeled control.
    Label { control: NodeId },
    /// The summary of a `<details>`: toggle `open`.
    Summary { details: NodeId },
}

/// A form submission turned into a navigation.
#[derive(Clone, Debug, PartialEq)]
pub struct Submission {
    pub url: Url,
    /// Request body for `method=post` to http(s).
    pub post: Option<PostBody>,
    /// `target=_blank` (or another named target): open in a new tab.
    pub new_tab: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostBody {
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Undo record of a checkbox or radio toggled before its click event, for
/// when a listener cancels the click.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Undo {
    el: NodeId,
    was_checked: bool,
    /// Radio buttons of the group that were checked before.
    unchecked: Vec<NodeId>,
}

fn html<'d>(doc: &'d Document, n: NodeId) -> Option<&'d ElementData> {
    doc.element(n).filter(|e| e.name.ns == ns!(html))
}

/// The `type` of an `<input>`, lowercased; `text` when missing or unknown
/// to us (we treat every unknown type as a text field).
pub fn input_type(el: &ElementData) -> String {
    el.attr(&local_name!("type"))
        .map_or_else(|| "text".to_owned(), |t| t.trim().to_ascii_lowercase())
}

fn is_input_of(doc: &Document, n: NodeId, types: &[&str]) -> bool {
    html(doc, n).is_some_and(|e| {
        e.is_html(&local_name!("input")) && types.contains(&input_type(e).as_str())
    })
}

/// A text field: `<textarea>` or an `<input>` that edits text.
pub fn is_text_control(doc: &Document, n: NodeId) -> bool {
    let Some(e) = html(doc, n) else { return false };
    if e.is_html(&local_name!("textarea")) {
        return true;
    }
    e.is_html(&local_name!("input"))
        && !matches!(
            input_type(e).as_str(),
            "checkbox"
                | "radio"
                | "submit"
                | "reset"
                | "button"
                | "image"
                | "file"
                | "hidden"
                | "color"
                | "range"
        )
}

/// A single-line text field (Enter submits the form).
pub fn is_single_line(doc: &Document, n: NodeId) -> bool {
    is_text_control(doc, n) && !html(doc, n).is_some_and(|e| e.is_html(&local_name!("textarea")))
}

/// The current value of a control: the `value` attribute, or a textarea's text.
pub fn value(doc: &Document, n: NodeId) -> String {
    match html(doc, n) {
        Some(e) if e.is_html(&local_name!("textarea")) => doc.text_content(n),
        Some(e) => e.attr(&local_name!("value")).unwrap_or("").to_owned(),
        None => String::new(),
    }
}

/// Set a control's value (see [`value`]).
pub fn set_value(doc: &mut Document, n: NodeId, v: &str) {
    if html(doc, n).is_some_and(|e| e.is_html(&local_name!("textarea"))) {
        doc.set_text_content(n, v);
    } else if doc.element(n).is_some() {
        doc.set_attribute(n, local_name!("value"), v);
    }
}

/// Disabled: the control's own `disabled`, or a disabled `<fieldset>`
/// ancestor (unless the control is in its first `<legend>`).
pub fn is_disabled(doc: &Document, n: NodeId) -> bool {
    let Some(e) = html(doc, n) else { return false };
    let control = matches!(
        &*e.name.local,
        "button" | "input" | "select" | "textarea" | "option" | "optgroup" | "fieldset"
    );
    if !control {
        return false;
    }
    if e.attr(&local_name!("disabled")).is_some() {
        return true;
    }
    let mut child = n;
    for a in doc.ancestors(n) {
        if let Some(ae) = html(doc, a)
            && ae.is_html(&local_name!("fieldset"))
            && ae.attr(&local_name!("disabled")).is_some()
        {
            let first_legend = doc
                .children(a)
                .find(|&c| html(doc, c).is_some_and(|ce| ce.is_html(&local_name!("legend"))));
            if first_legend != Some(child) {
                return true;
            }
        }
        child = a;
    }
    false
}

/// Can be focused by a click or `focus()`.
pub fn is_focusable(doc: &Document, n: NodeId) -> bool {
    let Some(e) = html(doc, n) else { return false };
    if is_disabled(doc, n) {
        return false;
    }
    if e.attr(&local_name!("tabindex"))
        .is_some_and(|t| t.trim().parse::<i32>().is_ok())
    {
        return true;
    }
    if e.attr(&local_name!("contenteditable"))
        .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
    {
        return true;
    }
    match &*e.name.local {
        "a" | "area" => e.attr(&local_name!("href")).is_some(),
        "button" | "select" | "textarea" | "iframe" => true,
        "input" => input_type(e) != "hidden",
        "summary" => doc
            .parent(n)
            .is_some_and(|p| html(doc, p).is_some_and(|pe| pe.is_html(&local_name!("details")))),
        _ => false,
    }
}

/// The element a click on `n` focuses: its nearest focusable inclusive
/// ancestor, else the control of the `<label>` it is in (None: focus moves
/// to the document).
pub fn click_focus_target(doc: &Document, n: NodeId) -> Option<NodeId> {
    let chain = || std::iter::once(n).chain(doc.ancestors(n));
    chain().find(|&a| is_focusable(doc, a)).or_else(|| {
        let label =
            chain().find(|&a| html(doc, a).is_some_and(|e| e.is_html(&local_name!("label"))))?;
        labeled_control(doc, label).filter(|&c| is_focusable(doc, c))
    })
}

/// The form a control belongs to: its `form` attribute's target, else the
/// nearest `<form>` ancestor.
pub fn form_owner(doc: &Document, control: NodeId) -> Option<NodeId> {
    let e = html(doc, control)?;
    if let Some(id) = e.attr(&local_name!("form")) {
        return doc
            .element_by_id(id)
            .filter(|&f| html(doc, f).is_some_and(|fe| fe.is_html(&local_name!("form"))));
    }
    doc.ancestors(control)
        .find(|&a| html(doc, a).is_some_and(|ae| ae.is_html(&local_name!("form"))))
}

fn is_submit_button(doc: &Document, n: NodeId) -> bool {
    let Some(e) = html(doc, n) else { return false };
    if e.is_html(&local_name!("button")) {
        return e.attr(&local_name!("type")).is_none_or(|t| {
            let t = t.trim();
            t.eq_ignore_ascii_case("submit")
                || !(t.eq_ignore_ascii_case("button") || t.eq_ignore_ascii_case("reset"))
        });
    }
    is_input_of(doc, n, &["submit", "image"])
}

/// The labeled control of a `<label>`: `for`'s target, else its first
/// labelable descendant.
pub fn labeled_control(doc: &Document, label: NodeId) -> Option<NodeId> {
    let labelable = |n: NodeId| {
        html(doc, n).is_some_and(|e| match &*e.name.local {
            "button" | "meter" | "output" | "progress" | "select" | "textarea" => true,
            "input" => input_type(e) != "hidden",
            _ => false,
        })
    };
    match html(doc, label)?.attr(&local_name!("for")) {
        Some(id) => doc.element_by_id(id).filter(|&c| labelable(c)),
        None => doc.descendants(label).skip(1).find(|&c| labelable(c)),
    }
}

/// The activation behavior a click on `target` runs: that of the nearest
/// inclusive ancestor that has one, with the element.
pub fn activation(doc: &Document, target: NodeId, base: &Url) -> Option<(NodeId, Activation)> {
    for n in std::iter::once(target).chain(doc.ancestors(target)) {
        let Some(e) = html(doc, n) else { continue };
        let act = match &*e.name.local {
            "a" | "area" => match e
                .attr(&local_name!("href"))
                .and_then(|h| huginn_net::resolve_url(base, h))
            {
                Some(url) => {
                    let new_tab = e.attr(&local_name!("target")).is_some_and(opens_new_tab);
                    Some(Activation::Link { url, new_tab })
                }
                None => None,
            },
            "button" | "input" if is_disabled(doc, n) => return None,
            "button" => {
                let form = form_owner(doc, n);
                match e
                    .attr(&local_name!("type"))
                    .map(|t| t.trim().to_ascii_lowercase())
                {
                    Some(t) if t == "button" => return None,
                    Some(t) if t == "reset" => form.map(|form| Activation::Reset { form }),
                    _ => form.map(|form| Activation::Submit { form }),
                }
            }
            "input" => match input_type(e).as_str() {
                "checkbox" | "radio" => Some(Activation::Checkable),
                "submit" | "image" => form_owner(doc, n).map(|form| Activation::Submit { form }),
                "reset" => form_owner(doc, n).map(|form| Activation::Reset { form }),
                _ => return None,
            },
            "label" => match labeled_control(doc, n) {
                // A click on the control itself (inside the label) is not forwarded.
                Some(c) if c != target && !doc.ancestors(target).any(|a| a == c) => {
                    Some(Activation::Label { control: c })
                }
                _ => return None,
            },
            "summary" => match doc.parent(n) {
                Some(p)
                    if html(doc, p).is_some_and(|pe| pe.is_html(&local_name!("details")))
                        && doc.children(p).find(|&c| {
                            html(doc, c).is_some_and(|ce| ce.is_html(&local_name!("summary")))
                        }) == Some(n) =>
                {
                    Some(Activation::Summary { details: p })
                }
                _ => None,
            },
            // Controls without activation behavior stop the search: a click
            // in a text field inside a link does not follow the link.
            "select" | "textarea" => return None,
            _ => None,
        };
        if let Some(a) = act {
            return Some((n, a));
        }
    }
    None
}

fn opens_new_tab(target: &str) -> bool {
    let t = target.trim();
    !t.is_empty()
        && !["_self", "_parent", "_top"]
            .iter()
            .any(|k| t.eq_ignore_ascii_case(k))
}

fn is_checked(doc: &Document, n: NodeId) -> bool {
    doc.attribute(n, &local_name!("checked")).is_some()
}

/// Radio buttons in the same group as `radio` (same name, same form owner,
/// same tree), `radio` included.
fn radio_group(doc: &Document, radio: NodeId) -> Vec<NodeId> {
    let Some(name) = doc
        .attribute(radio, &local_name!("name"))
        .filter(|n| !n.is_empty())
    else {
        return vec![radio];
    };
    let owner = form_owner(doc, radio);
    let root = doc.root(radio);
    doc.descendants(root)
        .filter(|&r| {
            r == radio
                || (is_input_of(doc, r, &["radio"])
                    && doc.attribute(r, &local_name!("name")) == Some(name)
                    && form_owner(doc, r) == owner)
        })
        .collect()
}

/// Legacy-pre-activation behavior: a checkbox flips, a radio button gets
/// checked (and its group unchecked) before the click event is dispatched.
pub fn pre_activate(doc: &mut Document, el: NodeId) -> Option<Undo> {
    if is_disabled(doc, el) {
        return None;
    }
    if is_input_of(doc, el, &["checkbox"]) {
        let was_checked = is_checked(doc, el);
        set_checked(doc, el, !was_checked);
        return Some(Undo {
            el,
            was_checked,
            unchecked: Vec::new(),
        });
    }
    if is_input_of(doc, el, &["radio"]) {
        let was_checked = is_checked(doc, el);
        let unchecked: Vec<NodeId> = radio_group(doc, el)
            .into_iter()
            .filter(|&r| r != el && is_checked(doc, r))
            .collect();
        for &r in &unchecked {
            set_checked(doc, r, false);
        }
        set_checked(doc, el, true);
        return Some(Undo {
            el,
            was_checked,
            unchecked,
        });
    }
    None
}

/// Legacy-canceled-activation behavior: restore what [`pre_activate`] changed.
pub fn undo(doc: &mut Document, undo: Undo) {
    set_checked(doc, undo.el, undo.was_checked);
    for r in undo.unchecked {
        set_checked(doc, r, true);
    }
}

/// Whether the click changed the element's checkedness (then `input` and
/// `change` fire).
pub fn changed(undo: &Undo, doc: &Document) -> bool {
    is_checked(doc, undo.el) != undo.was_checked
}

fn set_checked(doc: &mut Document, n: NodeId, on: bool) {
    if on {
        if !is_checked(doc, n) {
            doc.set_attribute(n, local_name!("checked"), "");
        }
    } else {
        doc.remove_attribute(n, &local_name!("checked"));
    }
}

/// Options of a `<select>` in tree order (including those in `<optgroup>`).
pub fn options(doc: &Document, select: NodeId) -> Vec<NodeId> {
    doc.descendants(select)
        .skip(1)
        .filter(|&o| html(doc, o).is_some_and(|e| e.is_html(&local_name!("option"))))
        .collect()
}

/// Index of the selected option: the first with `selected`, else 0 for a
/// single-choice select with options.
pub fn selected_index(doc: &Document, select: NodeId) -> Option<usize> {
    let opts = options(doc, select);
    let multiple = doc.attribute(select, &local_name!("multiple")).is_some();
    opts.iter()
        .position(|&o| doc.attribute(o, &local_name!("selected")).is_some())
        .or((!opts.is_empty() && !multiple).then_some(0))
}

/// The value of an option: its `value` attribute, else its text with
/// whitespace collapsed.
pub fn option_value(doc: &Document, option: NodeId) -> String {
    match doc.attribute(option, &local_name!("value")) {
        Some(v) => v.to_owned(),
        None => option_label(doc, option),
    }
}

/// The text of an option with whitespace collapsed.
pub fn option_label(doc: &Document, option: NodeId) -> String {
    doc.text_content(option)
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Select option `index` of a single-choice `<select>`. Returns true if the
/// selection changed.
pub fn select_option(doc: &mut Document, select: NodeId, index: usize) -> bool {
    let opts = options(doc, select);
    let Some(&pick) = opts.get(index) else {
        return false;
    };
    if selected_index(doc, select) == Some(index)
        && doc.attribute(pick, &local_name!("selected")).is_some()
    {
        return false;
    }
    let before = selected_index(doc, select);
    for &o in &opts {
        if o != pick {
            doc.remove_attribute(o, &local_name!("selected"));
        }
    }
    doc.set_attribute(pick, local_name!("selected"), "");
    before != Some(index)
}

/// The form's default button: its first submit button in tree order.
pub fn default_button(doc: &Document, form: NodeId) -> Option<NodeId> {
    let root = doc.root(form);
    doc.descendants(root)
        .find(|&n| is_submit_button(doc, n) && form_owner(doc, n) == Some(form))
}

/// Implicit submission (Enter in a text field): the default button to
/// click, `Ok(None)` to submit without a button (no button and only one
/// field that blocks implicit submission), or `Err(())` to do nothing.
pub fn implicit_submission(doc: &Document, form: NodeId) -> Result<Option<NodeId>, ()> {
    if let Some(b) = default_button(doc, form) {
        return if is_disabled(doc, b) {
            Err(())
        } else {
            Ok(Some(b))
        };
    }
    let root = doc.root(form);
    let blocking = doc
        .descendants(root)
        .filter(|&n| {
            is_input_of(
                doc,
                n,
                &[
                    "text",
                    "search",
                    "url",
                    "tel",
                    "email",
                    "password",
                    "date",
                    "month",
                    "week",
                    "time",
                    "datetime-local",
                    "number",
                ],
            ) && form_owner(doc, n) == Some(form)
        })
        .count();
    if blocking <= 1 { Ok(None) } else { Err(()) }
}

/// The form data set ("constructing the entry list"), in tree order.
pub fn entry_list(
    doc: &Document,
    form: NodeId,
    submitter: Option<NodeId>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let root = doc.root(form);
    for n in doc.descendants(root) {
        let Some(e) = html(doc, n) else { continue };
        let local = &*e.name.local;
        if !matches!(local, "button" | "input" | "select" | "textarea")
            || form_owner(doc, n) != Some(form)
        {
            continue;
        }
        if is_disabled(doc, n)
            || doc
                .ancestors(n)
                .any(|a| html(doc, a).is_some_and(|ae| ae.is_html(&local_name!("datalist"))))
        {
            continue;
        }
        let ty = if local == "input" {
            input_type(e)
        } else {
            String::new()
        };
        let is_button =
            local == "button" || matches!(ty.as_str(), "submit" | "reset" | "button" | "image");
        if is_button && Some(n) != submitter {
            continue;
        }
        if matches!(ty.as_str(), "checkbox" | "radio") && !is_checked(doc, n) {
            continue;
        }
        let name = e.attr(&local_name!("name")).unwrap_or("");
        if ty == "image" {
            let prefix = if name.is_empty() {
                String::new()
            } else {
                format!("{name}.")
            };
            out.push((format!("{prefix}x"), "0".to_owned()));
            out.push((format!("{prefix}y"), "0".to_owned()));
            continue;
        }
        if name.is_empty() {
            continue;
        }
        match local {
            "select" => {
                let opts = options(doc, n);
                let multiple = e.attr(&local_name!("multiple")).is_some();
                let picked: Vec<NodeId> = if multiple {
                    opts.iter()
                        .copied()
                        .filter(|&o| doc.attribute(o, &local_name!("selected")).is_some())
                        .collect()
                } else {
                    selected_index(doc, n)
                        .and_then(|i| opts.get(i).copied())
                        .into_iter()
                        .collect()
                };
                for o in picked {
                    if !is_disabled(doc, o) {
                        out.push((name.to_owned(), option_value(doc, o)));
                    }
                }
            }
            "textarea" => out.push((
                name.to_owned(),
                doc.text_content(n)
                    .replace("\r\n", "\n")
                    .replace('\n', "\r\n"),
            )),
            _ => {
                let v = match ty.as_str() {
                    "checkbox" | "radio" => {
                        e.attr(&local_name!("value")).unwrap_or("on").to_owned()
                    }
                    "file" => String::new(),
                    "hidden" if name.eq_ignore_ascii_case("_charset_") => "UTF-8".to_owned(),
                    _ => e.attr(&local_name!("value")).unwrap_or("").to_owned(),
                };
                out.push((name.to_owned(), v));
            }
        }
    }
    out
}

/// Turn a form submission into a navigation (HTML "form submission
/// algorithm" after the submit event). `None` for `method=dialog`, unknown
/// schemes and invalid actions.
pub fn submission(
    doc: &Document,
    form: NodeId,
    submitter: Option<NodeId>,
    base: &Url,
    document_url: &Url,
) -> Option<Submission> {
    let attr = |name: &huginn_dom::LocalName, over: &huginn_dom::LocalName| {
        submitter
            .filter(|&s| s != form)
            .and_then(|s| doc.attribute(s, over))
            .or_else(|| doc.attribute(form, name))
            .map(str::trim)
    };
    let method = attr(&local_name!("method"), &local_name!("formmethod"))
        .unwrap_or("get")
        .to_ascii_lowercase();
    if method == "dialog" {
        return None;
    }
    let mut url =
        match attr(&local_name!("action"), &local_name!("formaction")).filter(|a| !a.is_empty()) {
            Some(a) => huginn_net::resolve_url(base, a)?,
            None => document_url.clone(),
        };
    let enctype = attr(&local_name!("enctype"), &local_name!("formenctype"))
        .unwrap_or("")
        .to_ascii_lowercase();
    let new_tab =
        attr(&local_name!("target"), &local_name!("formtarget")).is_some_and(opens_new_tab);
    let entries = entry_list(doc, form, submitter);
    let post = match (url.scheme(), method.as_str()) {
        ("http" | "https", "post") => Some(encode_body(&entries, &enctype)),
        ("http" | "https" | "file", _) => {
            url.set_fragment(None);
            let q = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(&entries)
                .finish();
            url.set_query(Some(&q));
            None
        }
        ("data", _) => None,
        _ => return None,
    };
    Some(Submission { url, post, new_tab })
}

fn encode_body(entries: &[(String, String)], enctype: &str) -> PostBody {
    match enctype {
        "multipart/form-data" => {
            let boundary = "----HuginnFormBoundary7MA4YWxkTrZu0gW";
            let mut body = Vec::new();
            for (name, value) in entries {
                let esc = |s: &str| {
                    s.replace('"', "%22")
                        .replace('\r', "%0D")
                        .replace('\n', "%0A")
                };
                body.extend_from_slice(
                    format!(
                        "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n",
                        esc(name)
                    )
                    .as_bytes(),
                );
                body.extend_from_slice(value.as_bytes());
                body.extend_from_slice(b"\r\n");
            }
            body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
            PostBody {
                content_type: format!("multipart/form-data; boundary={boundary}"),
                body,
            }
        }
        "text/plain" => {
            let body: String = entries
                .iter()
                .map(|(n, v)| format!("{n}={v}\r\n"))
                .collect();
            PostBody {
                content_type: "text/plain".to_owned(),
                body: body.into_bytes(),
            }
        }
        _ => {
            let body = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(entries)
                .finish();
            PostBody {
                content_type: "application/x-www-form-urlencoded".to_owned(),
                body: body.into_bytes(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(html: &str) -> Document {
        huginn_dom::parse_html(html)
    }

    fn by_id(d: &Document, id: &str) -> NodeId {
        d.element_by_id(id).expect("element")
    }

    #[test]
    fn get_form_puts_entries_in_the_query() {
        let d = doc(
            r#"<form id=f action="/search?old=1#x"><input name=q value="a b&c"><input type=checkbox name=c><input type=checkbox name=d checked><select name=s><option>one<option selected value=2>two</select><textarea name=t>l1
l2</textarea><button id=b name=go value=1>Go</button><button name=other>x</button></form>"#,
        );
        let base = Url::parse("http://h/p").unwrap();
        let s = submission(&d, by_id(&d, "f"), Some(by_id(&d, "b")), &base, &base).unwrap();
        assert_eq!(
            s.url.as_str(),
            "http://h/search?q=a+b%26c&d=on&s=2&t=l1%0D%0Al2&go=1"
        );
        assert!(s.post.is_none());
    }

    #[test]
    fn post_form_has_an_urlencoded_body_and_formaction_wins() {
        let d = doc(
            r#"<form id=f method=post action=/a><input name=x value=1><input id=b type=submit formaction=/b name=s value=S></form>"#,
        );
        let base = Url::parse("http://h/").unwrap();
        let s = submission(&d, by_id(&d, "f"), Some(by_id(&d, "b")), &base, &base).unwrap();
        assert_eq!(s.url.as_str(), "http://h/b");
        let post = s.post.unwrap();
        assert_eq!(post.content_type, "application/x-www-form-urlencoded");
        assert_eq!(post.body, b"x=1&s=S");
    }

    #[test]
    fn radio_click_unchecks_the_group_and_cancel_restores_it() {
        let mut d = doc(
            r#"<form><input type=radio name=r id=a checked><input type=radio name=r id=b></form><input type=radio name=r id=c checked>"#,
        );
        let (a, b, c) = (by_id(&d, "a"), by_id(&d, "b"), by_id(&d, "c"));
        let u = pre_activate(&mut d, b).unwrap();
        assert!(!is_checked(&d, a) && is_checked(&d, b));
        assert!(
            is_checked(&d, c),
            "a radio in another form owner is another group"
        );
        undo(&mut d, u);
        assert!(is_checked(&d, a) && !is_checked(&d, b));
    }

    #[test]
    fn activation_finds_links_labels_and_stops_at_text_fields() {
        let d = doc(
            r#"<a id=a href="/x" target=_blank><span id=s>t</span><input id=i></a><label id=l for=cb>L</label><input type=checkbox id=cb>"#,
        );
        let base = Url::parse("http://h/").unwrap();
        let (n, act) = activation(&d, by_id(&d, "s"), &base).unwrap();
        assert_eq!(n, by_id(&d, "a"));
        assert_eq!(
            act,
            Activation::Link {
                url: Url::parse("http://h/x").unwrap(),
                new_tab: true
            }
        );
        assert!(activation(&d, by_id(&d, "i"), &base).is_none());
        assert_eq!(
            activation(&d, by_id(&d, "l"), &base).unwrap().1,
            Activation::Label {
                control: by_id(&d, "cb")
            }
        );
    }

    #[test]
    fn implicit_submission_needs_a_button_or_a_single_field() {
        let d = doc(
            r#"<form id=one><input name=a></form><form id=two><input name=a><input name=b></form><form id=three><input name=a><input name=b><button id=b>go</button></form>"#,
        );
        assert_eq!(implicit_submission(&d, by_id(&d, "one")), Ok(None));
        assert_eq!(implicit_submission(&d, by_id(&d, "two")), Err(()));
        assert_eq!(
            implicit_submission(&d, by_id(&d, "three")),
            Ok(Some(by_id(&d, "b")))
        );
    }
}
