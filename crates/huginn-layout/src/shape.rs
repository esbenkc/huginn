//! A cache of shaped text between layout passes.
//!
//! A layout pass rebuilds the box tree, and shaping the text of each inline
//! formatting context (font matching, HarfBuzz-style shaping, break analysis)
//! is most of that work. When a page is laid out again after a small change,
//! almost all of its text is the same as before. This cache keeps the shaped
//! [`parley::Layout`] of each inline formatting context from the previous pass
//! and hands it to the next pass when the same text comes back with the same
//! styles and the same fonts.
//!
//! The shaped layout does not depend on the available width: line breaking
//! and the sizes of inline boxes are done again in every pass.
//!
//! The cache holds the contexts of one pass, and layouts are moved, not
//! copied: a pass takes them out and gives them back at its end. It is off
//! by default (`huginn render` lays out
//! once or twice and counts every megabyte); the window turns it on.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use huginn_style::{ComputedStyle, VerticalAlign};

use crate::convert::Decor;
use crate::inline::Brush;

/// One input event of an inline formatting context, as far as shaping sees it.
pub(crate) enum SigEv {
    Open(Arc<ComputedStyle>),
    Close,
    /// Text of a DOM node (the text itself is in the key).
    Text,
    /// Generated text with its own style.
    Str(Arc<ComputedStyle>),
    Br(Arc<ComputedStyle>),
    Atomic(VerticalAlign),
    Oof,
}

/// Everything that decides how an inline formatting context is shaped,
/// except its text (hashed into the key) and the fonts (the epoch).
pub(crate) struct Sig {
    pub root: Arc<ComputedStyle>,
    pub decor: Decor,
    pub evs: Vec<SigEv>,
}

fn same_style(a: &Arc<ComputedStyle>, b: &Arc<ComputedStyle>) -> bool {
    Arc::ptr_eq(a, b) || a == b
}

impl Sig {
    fn matches(&self, other: &Sig) -> bool {
        self.evs.len() == other.evs.len()
            && self.decor == other.decor
            && same_style(&self.root, &other.root)
            && self.evs.iter().zip(&other.evs).all(|p| match p {
                (SigEv::Open(a), SigEv::Open(b))
                | (SigEv::Str(a), SigEv::Str(b))
                | (SigEv::Br(a), SigEv::Br(b)) => same_style(a, b),
                (SigEv::Atomic(a), SigEv::Atomic(b)) => a == b,
                (SigEv::Close, SigEv::Close)
                | (SigEv::Text, SigEv::Text)
                | (SigEv::Oof, SigEv::Oof) => true,
                _ => false,
            })
    }
}

/// Hashes the text and the event order of a context into a 128-bit key. Two
/// contexts with the same key are taken to have the same text: the styles
/// are then compared in full ([`Sig`]).
pub(crate) struct KeyHasher(std::hash::DefaultHasher, std::hash::DefaultHasher);

impl KeyHasher {
    pub fn new() -> KeyHasher {
        let mut second = std::hash::DefaultHasher::new();
        0x9e37_79b9_7f4a_7c15u64.hash(&mut second);
        KeyHasher(std::hash::DefaultHasher::new(), second)
    }

    /// An event without text.
    pub fn mark(&mut self, tag: u8) {
        tag.hash(&mut self.0);
        tag.hash(&mut self.1);
    }

    pub fn text(&mut self, tag: u8, text: &str) {
        self.mark(tag);
        text.hash(&mut self.0);
        text.hash(&mut self.1);
    }

    pub fn finish(self) -> u128 {
        (u128::from(self.0.finish()) << 64) | u128::from(self.1.finish())
    }
}

/// What an inline formatting context needs to give its shaped layout back
/// to the cache at the end of the pass.
pub(crate) struct Ticket {
    key: u128,
    sig: Sig,
    epoch: u64,
}

struct Entry {
    sig: Sig,
    /// `Fonts::epoch` when the text was shaped.
    epoch: u64,
    layout: parley::Layout<Brush>,
    /// The laid-out text, when the engine keeps a text index.
    text: Option<String>,
}

/// The shaped layouts of the last pass. A pass takes them out one by one
/// ([`ShapeCache::take`]) and gives the layouts of all its contexts back at
/// the end ([`ShapeCache::give`], [`ShapeCache::end_pass`]), so nothing is
/// copied and what the pass did not use is dropped.
#[derive(Default)]
pub(crate) struct ShapeCache {
    last: HashMap<u128, Entry>,
    next: HashMap<u128, Entry>,
}

impl ShapeCache {
    /// The shaped layout for `key`, if the last pass shaped it with the same
    /// styles (`sig`), the same fonts (`epoch`) and, when `need_text`, kept
    /// its text. The ticket goes back to [`ShapeCache::give`] either way.
    pub fn take(
        &mut self,
        key: u128,
        sig: Sig,
        epoch: u64,
        need_text: bool,
    ) -> (Option<(parley::Layout<Brush>, Option<String>)>, Ticket) {
        // Out of the cache either way: an entry that does not match is out
        // of date, and dropping it now lets the new shaping reuse its memory.
        let found = self
            .last
            .remove(&key)
            .filter(|e| e.epoch == epoch && (!need_text || e.text.is_some()) && e.sig.matches(&sig))
            .map(|e| {
                let mut layout = e.layout;
                // The last pass broke lines in it and sized its inline boxes;
                // the next one does both again. The indent is set only when it
                // is not zero, and a percentage indent can become zero.
                layout.set_text_indent(0.0, Default::default());
                (layout, e.text.filter(|_| need_text))
            });
        (found, Ticket { key, sig, epoch })
    }

    /// Keep a context's shaped layout for the next pass.
    pub fn give(&mut self, ticket: Ticket, layout: parley::Layout<Brush>, text: Option<String>) {
        self.next.insert(
            ticket.key,
            Entry {
                sig: ticket.sig,
                epoch: ticket.epoch,
                layout,
                text,
            },
        );
    }

    /// The pass is over: what it gave back is the cache now.
    pub fn end_pass(&mut self) {
        self.last = std::mem::take(&mut self.next);
    }
}
