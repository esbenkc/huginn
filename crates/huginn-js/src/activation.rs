//! Event sequences with default actions: a click and its activation
//! behavior, form submission, focus changes. The same code runs for user
//! input (the browser drives it through [`EngineSink`], with or without a
//! script engine) and for scripts (`el.click()`, `form.requestSubmit()`,
//! `el.focus()`, through the V8 layer's own sink).

use huginn_dom::{InteractionState, NodeId};
use huginn_net::Url;

use crate::forms::{self, Activation};
use crate::{Event, EventTarget, EventValue, HostContext, HostRequest, ScriptEngine};

/// Where the algorithms read the document, fire events and leave requests.
pub trait EventSink {
    fn document(&mut self) -> &mut huginn_dom::Document;
    /// Dispatch `event` at `target`; false if a listener canceled it.
    fn fire(&mut self, target: EventTarget, event: &Event) -> bool;
    fn request(&mut self, request: HostRequest);
    fn base_url(&self) -> Url;
    fn document_url(&self) -> Url;
}

/// A `click` MouseEvent with the given init members.
pub fn click_event(init: Vec<(&'static str, EventValue)>, trusted: bool) -> Event {
    let mut e = Event::ui("click", "PointerEvent", init);
    e.trusted = trusted;
    e
}

/// Fire a click at `target` and run the activation behavior it reaches
/// (HTML: legacy-pre-activation, dispatch, then activation or
/// legacy-canceled-activation behavior).
pub fn click(sink: &mut dyn EventSink, target: NodeId, event: &Event) {
    let base = sink.base_url();
    let doc = sink.document();
    if forms::is_disabled(doc, target) {
        return;
    }
    let act = forms::activation(doc, target, &base);
    let undo = match &act {
        Some((el, Activation::Checkable)) => forms::pre_activate(doc, *el),
        _ => None,
    };
    if !sink.fire(EventTarget::Node(target), event) {
        if let Some(u) = undo {
            forms::undo(sink.document(), u);
        }
        return;
    }
    let Some((el, act)) = act else { return };
    match act {
        Activation::Checkable => {
            if undo.is_some_and(|u| forms::changed(&u, sink.document())) {
                fire_input_change(sink, el);
            }
        }
        Activation::Submit { form } => submit(sink, form, Some(el), false),
        Activation::Reset { form } => {
            sink.fire(EventTarget::Node(form), &Event::new("reset", true, true));
        }
        Activation::Link { url, new_tab } => sink.request(HostRequest::Navigate {
            url,
            post: None,
            new_tab,
            replace: false,
        }),
        Activation::Label { control } => {
            let mut e = event.clone();
            e.trusted = false;
            click(sink, control, &e);
        }
        Activation::Summary { details } => {
            let doc = sink.document();
            let open = huginn_dom::local_name!("open");
            if doc.attribute(details, &open).is_some() {
                doc.remove_attribute(details, &open);
            } else {
                doc.set_attribute(details, open, "");
            }
            sink.fire(
                EventTarget::Node(details),
                &Event::new("toggle", false, false),
            );
        }
    }
}

/// `input` then `change`, as after a user changes a control.
pub fn fire_input_change(sink: &mut dyn EventSink, el: NodeId) {
    let mut input = Event::new("input", true, false);
    input.init.push(("composed", EventValue::Bool(true)));
    sink.fire(EventTarget::Node(el), &input);
    sink.fire(EventTarget::Node(el), &Event::new("change", true, false));
}

/// Submit `form` (HTML form submission algorithm). `from_method`: called by
/// `form.submit()`, which fires no submit event.
pub fn submit(
    sink: &mut dyn EventSink,
    form: NodeId,
    submitter: Option<NodeId>,
    from_method: bool,
) {
    if !from_method {
        let mut ev = Event::new("submit", true, true);
        ev.interface = "SubmitEvent";
        ev.init.push(("submitter", EventValue::Node(submitter)));
        if !sink.fire(EventTarget::Node(form), &ev) {
            return;
        }
    }
    let (base, doc_url) = (sink.base_url(), sink.document_url());
    if let Some(s) = forms::submission(sink.document(), form, submitter, &base, &doc_url) {
        sink.request(HostRequest::Navigate {
            url: s.url,
            post: s.post,
            new_tab: s.new_tab,
            replace: false,
        });
    }
}

/// Enter in a single-line text field: click the form's default button, or
/// submit the form if it has none and only this field.
pub fn implicit_submit(sink: &mut dyn EventSink, field: NodeId) {
    let doc = sink.document();
    let Some(form) = forms::form_owner(doc, field) else {
        return;
    };
    match forms::implicit_submission(doc, form) {
        Ok(Some(button)) => click(sink, button, &click_event(Vec::new(), false)),
        Ok(None) => submit(sink, form, None, false),
        Err(()) => {}
    }
}

/// Move focus to `to` (None: the document), firing blur/focusout on the old
/// element and focus/focusin on the new one. `visible`: `:focus-visible`.
pub fn set_focus(sink: &mut dyn EventSink, to: Option<NodeId>, visible: bool) {
    let doc = sink.document();
    let old = doc.interaction().focus;
    if old == to {
        if doc.interaction().focus_visible != visible && to.is_some() {
            let state = InteractionState {
                focus_visible: visible,
                ..doc.interaction().clone()
            };
            doc.set_interaction(state);
        }
        return;
    }
    let state = InteractionState {
        focus: to,
        focus_visible: visible && to.is_some(),
        ..doc.interaction().clone()
    };
    doc.set_interaction(state);
    let focus_event = |kind: &str, bubbles: bool, related: Option<NodeId>| {
        Event::ui(
            kind,
            "FocusEvent",
            vec![("relatedTarget", EventValue::Node(related))],
        )
        .with_bubbles(bubbles, false)
    };
    if let Some(o) = old {
        sink.fire(EventTarget::Node(o), &focus_event("blur", false, to));
        sink.fire(EventTarget::Node(o), &focus_event("focusout", true, to));
    }
    if let Some(n) = to {
        sink.fire(EventTarget::Node(n), &focus_event("focus", false, old));
        sink.fire(EventTarget::Node(n), &focus_event("focusin", true, old));
    }
}

/// The browser's sink: events go to the script engine (if any), each
/// followed by a microtask checkpoint; requests are collected.
pub struct EngineSink<'e, 'c> {
    pub engine: Option<&'e mut (dyn ScriptEngine + 'static)>,
    pub cx: &'e mut HostContext<'c>,
    pub requests: Vec<HostRequest>,
    /// Uncaught exceptions in listeners.
    pub errors: usize,
}

impl<'e, 'c> EngineSink<'e, 'c> {
    pub fn new(
        engine: Option<&'e mut (dyn ScriptEngine + 'static)>,
        cx: &'e mut HostContext<'c>,
    ) -> Self {
        EngineSink {
            engine,
            cx,
            requests: Vec::new(),
            errors: 0,
        }
    }
}

impl EventSink for EngineSink<'_, '_> {
    fn document(&mut self) -> &mut huginn_dom::Document {
        self.cx.document
    }

    fn fire(&mut self, target: EventTarget, event: &Event) -> bool {
        let Some(engine) = self.engine.as_deref_mut() else {
            return true;
        };
        let r = engine.dispatch_event(self.cx, target, event);
        if engine.run_microtasks(self.cx).is_err() {
            self.errors += 1;
        }
        self.requests.extend(engine.take_requests());
        match r {
            Ok(ok) => ok,
            Err(_) => {
                self.errors += 1;
                true
            }
        }
    }

    fn request(&mut self, request: HostRequest) {
        self.requests.push(request);
    }

    fn base_url(&self) -> Url {
        self.cx.base_url.clone()
    }

    fn document_url(&self) -> Url {
        self.cx.document_url.clone()
    }
}
