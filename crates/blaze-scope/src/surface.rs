//! D189, point 4: the surface pass, between Names and Heads. Per FILE (or any text that holds
//! bodies), which bodies are SURFACE bodies: bodies that may write something seen outside them
//! (an impl, an exported or linked item), and so get a `BodyScope` task before Heads. Every
//! other body's block scopes are solved inside its own lowering, and it costs no task and no
//! record before Bodies (D189, point 7).
//!
//! A body is a surface body when (D189, point 4):
//! - its text shows it ([`blaze_file_items::nested::Nested::surface_by_text`]);
//! - a call in it (or in a local item's own text, read the same way) names a definition whose
//!   [`MacroEffect`] `emits_surface` or `binds_at_top` (blaze-scope `macro_effect`);
//! - a call's input holds an item word, an attribute, or a nested call not proven the same way
//!   ([`blaze_file_items::body_paths::input_mark`]: a transcriber may place the caller's tokens
//!   as items);
//! - a call's path is not read, names nothing, or carries an attribute that is not inert.
//!
//! A call's path is resolved in the body's LEXICAL chain, as Rust reads it (the Reference,
//! "Macros By Example", "Scoping, exporting, and importing"; "Names", "Scopes"): a one-segment
//! name first in the textual scope of the body's own `macro_rules!` (the latest one written
//! before the call in a block around it, through the local items around the call too), then by
//! path: the items and named `use`s / `extern crate`s of each block around the call, innermost
//! first, then that block's globs; then the body's module in the FINISHED module names
//! ([`BodyNames`], which applies the module's textual scope, its items and imports, and the
//! preludes). Where two readings could apply (a block `use` and a module `macro_rules!` of one
//! name), the effect is the join of both: a superset, never a guess of less (rule 3).
//!
//! The names are finished when this runs (D189's order: Names solves module-level items only,
//! with no body text in), so every answer here is exact; each read is noted in the record's
//! read list ([`FileSurface::reads`]), and the record is keyed by the text and its bodies
//! ([`surface_key`]): valid by its key, or by its read list checked against today's names.
//! The [`Effects`] kept while one record is made are that record's own, so every read that
//! decided an answer is in its list (rule 21).

#![blaze::sans_io]

use core::cell::RefCell;

use blaze_file_items::body_paths::{InputMark, UseBinding, WrittenPath, call_path, input_mark, item_name, macro_rules_parts, use_bindings};
use blaze_file_items::nested::{MacroEffect, Nested, NestedCall, NestedKind, scan};
use blaze_id::ItemKey;
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc};
use blaze_surface::{Namespace, PathKind, Range};

use crate::macro_effect::{EffectNames, Effects, builtin_effect};
use crate::task::{MacroBody, MacroSrc};

/// The finished module names the surface pass reads, beyond what [`EffectNames`] reads.
pub trait BodyNames<'a>: EffectNames<'a> {
    /// The macro the path `kind` + `segments` names when written in `module` (not in a block),
    /// read in the finished names: the module's textual scope, its items and imports, the
    /// preludes (the macro namespace for the last segment). `None`: it names nothing.
    fn macro_in(&self, module: ItemKey, kind: PathKind, segments: &[&'a str]) -> Option<ItemKey>;
    /// The module (or enum, or crate root) the path names when written in `module`, in the
    /// type namespace: a block glob's source. `None`: it names no module.
    fn module_in(&self, module: ItemKey, kind: PathKind, segments: &[&'a str]) -> Option<ItemKey>;
}

/// D189's one seam (rule 78): who solves a body's block scopes. `Modules` (the default until
/// D189's three proofs pass) is the old path: blocks written as module text into the crate's
/// module tree (blocks.rs, expanded_blocks.rs). `Bodies` is the new one: the module names are
/// solved without body text, the surface pass ([`file_surface`]) picks the bodies whose blocks
/// write something seen outside them, and `BodyScope(body)` solves each body's blocks with the
/// module names fixed (a task only for surface bodies; inside the body's own lowering for every
/// other one).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BlockScopes {
    #[default]
    Modules,
    Bodies,
}

impl BlockScopes {
    /// The hidden flag's value (`--block-scopes=bodies|modules`); `None` for any other word.
    pub fn parse(word: &str) -> Option<BlockScopes> {
        match word {
            "modules" => Some(BlockScopes::Modules),
            "bodies" => Some(BlockScopes::Bodies),
            _ => None,
        }
    }

    /// Its byte in a key: a record made under one choice is never read under the other.
    pub fn key_byte(self) -> u8 {
        match self {
            BlockScopes::Modules => 0,
            BlockScopes::Bodies => 1,
        }
    }
}

/// One body of the text the pass reads: its range in the text and its module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyAt {
    pub range: Range,
    pub module: ItemKey,
}

/// What in a call's input made its body a surface body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputAt {
    ItemWord,
    Attribute,
    /// A nested call not proven, at its range in the body's text.
    Call(Range),
    /// A form not read (a `$`).
    Unread,
}

/// Why a body is a surface body. A call's range is in the body's own text (item-relative,
/// rule 71), a local item's call's too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceWhy {
    /// Its text alone ([`Nested::surface_by_text`]).
    Text,
    /// A call's definition may write something seen outside it.
    Emits { call: Range },
    /// A call's definition may bind, at its block's top, a name a later call's path starts with.
    Binds { call: Range },
    /// A call's input.
    Input { call: Range, at: InputAt },
    /// A call's path is not read here, carries an attribute that is not inert, or names nothing.
    NotRead { call: Range },
    /// Local items nested deeper than [`MAX_FRAMES`].
    Deep { call: Range },
}

/// One surface body: its index among the bodies given and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceRow {
    pub body: u32,
    pub why: SurfaceWhy,
}

/// One read of the finished names: the question's digest and the answer's (zero: none).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SurfaceRead {
    pub question: [u8; 32],
    pub answer: [u8; 32],
}

/// The surface record of one text: its surface bodies, and every read of the names that
/// decided it, sorted and without repeats.
#[derive(Debug)]
pub struct FileSurface<'o> {
    pub rows: AVec<'o, SurfaceRow>,
    pub reads: AVec<'o, SurfaceRead>,
}

impl FileSurface<'_> {
    /// Whether body `body` (its index among the bodies given) is a surface body.
    pub fn is_surface(&self, body: u32) -> bool {
        self.rows.iter().any(|r| r.body == body)
    }

    /// The digest of the read list: with [`surface_key`], what the record is checked by.
    pub fn reads_digest(&self) -> ContentId {
        let mut h = Hasher::new("blaze-scope surface reads 1");
        for r in self.reads.iter() {
            h.update(&r.question).update(&r.answer);
        }
        h.finish()
    }
}

/// The key of a text's surface record (rule 65, from text, before the work): the crate's root,
/// the text, and each body's range and module.
pub fn surface_key(root: ItemKey, text: &str, bodies: &[BodyAt]) -> ContentId {
    let mut h = Hasher::new("blaze-scope surface record 1");
    h.update(&root.0.0).update(&u64::try_from(text.len()).unwrap_or(u64::MAX).to_le_bytes()).update(text.as_bytes());
    for b in bodies {
        h.update(&b.range.start.to_le_bytes()).update(&b.range.end.to_le_bytes()).update(&b.module.0.0);
    }
    h.finish()
}

/// A planted bug for the tests that must catch it (rule 23): `SkipEffect` judges a call by its
/// name alone (a builtin's effect by that name, NONE otherwise), never asking the definition the
/// path resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Plant {
    #[default]
    None,
    SkipEffect,
}

/// How many local items deep the pass reads a body (a local fn's body, a local `mod`'s items):
/// a call deeper is [`SurfaceWhy::Deep`] (rule 18: a bound, never a "no").
pub const MAX_FRAMES: usize = 8;

/// How many block `use`s deep one path is followed (`use a as b; use b as c; c!()`).
pub const MAX_USE_DEPTH: u32 = 16;

/// The surface record of `text` holding `bodies`, its names read through `names`; `root` the
/// root module of the text's crate (what a body `macro_rules!`'s `$crate` names). Scratch and
/// the record are in `alloc`.
pub fn file_surface<'a, 'o, N: BodyNames<'a>>(names: &N, root: ItemKey, text: &'a str, bodies: &[BodyAt], plant: Plant, alloc: Alloc<'o>) -> FileSurface<'o> {
    let logged = Logged { names, reads: RefCell::new(AVec::new_in(alloc)) };
    let mut rows: AVec<'o, SurfaceRow> = AVec::new_in(alloc);
    {
        let mut pass = Pass { names: &logged, root, effects: Effects::new_in(alloc), plant, alloc, module: root };
        for (i, b) in bodies.iter().enumerate() {
            let body = u32::try_from(i).unwrap_or(u32::MAX);
            let Some(own) = text.get(b.range.start as usize..b.range.end as usize) else {
                rows.push(SurfaceRow { body, why: SurfaceWhy::NotRead { call: b.range } });
                continue;
            };
            pass.module = b.module;
            if let Some(why) = pass.body(own) {
                rows.push(SurfaceRow { body, why });
            }
        }
    }
    let mut reads = logged.reads.into_inner();
    reads.sort_unstable();
    reads.dedup();
    FileSurface { rows, reads }
}

/// The names read, each read noted.
struct Logged<'r, 'o, N> {
    names: &'r N,
    reads: RefCell<AVec<'o, SurfaceRead>>,
}

const NONE: [u8; 32] = [0; 32];

fn kind_bytes(kind: PathKind) -> [u8; 33] {
    let mut out = [0u8; 33];
    match kind {
        PathKind::Plain => out[0] = 1,
        PathKind::Crate => out[0] = 2,
        PathKind::SelfMod => out[0] = 3,
        PathKind::Super(n) => {
            out[0] = 4;
            out[1] = n;
        }
        PathKind::Abs => out[0] = 5,
        PathKind::DollarCrate(k) => {
            out[0] = 6;
            out[1..].copy_from_slice(&k.0.0);
        }
    }
    out
}

fn ns_byte(ns: Namespace) -> u8 {
    match ns {
        Namespace::Types => 0,
        Namespace::Values => 1,
        Namespace::Macros => 2,
    }
}

fn key_bytes(k: Option<ItemKey>) -> [u8; 32] {
    k.map_or(NONE, |k| k.0.0)
}

/// A source's digest: what of it the effect walk reads.
fn source_bytes(src: Option<MacroSrc<'_>>) -> [u8; 32] {
    let Some(src) = src else { return NONE };
    let mut h = Hasher::new("blaze-scope surface source 1");
    match src.body {
        MacroBody::Rules(r) => h.update(b"r").update(r.as_bytes()),
        MacroBody::Macro2 { params, body } => h.update(b"m").update(params.unwrap_or("").as_bytes()).update(b"\0").update(body.as_bytes()),
        MacroBody::BuiltinFn(name) => h.update(b"b").update(name.as_bytes()),
        MacroBody::BuiltinDerive { .. } => h.update(b"d"),
        MacroBody::BuiltinOther(_) => h.update(b"o"),
        MacroBody::Proc { .. } => h.update(b"p"),
    };
    h.finish().0
}

const SURFACE_READ_DOMAIN: &str = "blaze-scope surface read 1";

impl<'o, N> Logged<'_, 'o, N> {
    fn note(&self, h: &Hasher, answer: [u8; 32]) {
        self.reads.borrow_mut().push(SurfaceRead { question: h.finish().0, answer });
    }

    fn path_question(tag: &[u8], module: ItemKey, kind: PathKind, segments: &[&str]) -> Hasher {
        let mut h = Hasher::new(SURFACE_READ_DOMAIN);
        h.update(tag).update(&module.0.0).update(&kind_bytes(kind));
        for s in segments {
            h.update(s.as_bytes()).update(b"\0");
        }
        h
    }
}

impl<'a, N: BodyNames<'a>> EffectNames<'a> for Logged<'_, '_, N> {
    fn root_of(&self, def: ItemKey) -> Option<ItemKey> {
        let answer = self.names.root_of(def);
        let mut h = Hasher::new(SURFACE_READ_DOMAIN);
        h.update(b"root").update(&def.0.0);
        self.note(&h, key_bytes(answer));
        answer
    }

    fn lookup(&self, module: ItemKey, ns: Namespace, name: &'a str) -> Option<ItemKey> {
        let answer = self.names.lookup(module, ns, name);
        let mut h = Hasher::new(SURFACE_READ_DOMAIN);
        h.update(b"lookup").update(&module.0.0).update(&[ns_byte(ns)]).update(name.as_bytes());
        self.note(&h, key_bytes(answer));
        answer
    }

    fn source(&self, def: ItemKey) -> Option<MacroSrc<'a>> {
        let answer = self.names.source(def);
        let mut h = Hasher::new(SURFACE_READ_DOMAIN);
        h.update(b"source").update(&def.0.0);
        self.note(&h, source_bytes(answer));
        answer
    }
}

impl<'a, N: BodyNames<'a>> BodyNames<'a> for Logged<'_, '_, N> {
    fn macro_in(&self, module: ItemKey, kind: PathKind, segments: &[&'a str]) -> Option<ItemKey> {
        let answer = self.names.macro_in(module, kind, segments);
        self.note(&Self::path_question(b"macro", module, kind, segments), key_bytes(answer));
        answer
    }

    fn module_in(&self, module: ItemKey, kind: PathKind, segments: &[&'a str]) -> Option<ItemKey> {
        let answer = self.names.module_in(module, kind, segments);
        self.note(&Self::path_question(b"module", module, kind, segments), key_bytes(answer));
        answer
    }
}

/// One text of the lexical chain: a body's, or a local item's inside it. `outer`: where the
/// item whose text this is stands in the frame before it (its block and its start).
#[derive(Clone, Copy)]
struct Frame<'t, 'n, 's> {
    text: &'t str,
    nested: &'n Nested<'s>,
    /// The offset of `text` in the body's text.
    offset: u32,
    outer: (u32, u32),
}

/// Where a path is read: a frame, a block of it, a position in its text.
#[derive(Clone, Copy)]
struct At {
    frame: usize,
    block: u32,
    pos: u32,
}

/// The blocks from `block` out to the frame's outermost, `block` first.
fn chain<'n, 't>(nested: &'n Nested<'t>, block: u32) -> impl Iterator<Item = u32> + use<'n, 't> {
    let mut next = Some(block);
    core::iter::from_fn(move || {
        let b = next?;
        // A block's parent is written before it (rule 18: at most one step per block).
        next = nested.blocks.get(b as usize).and_then(|blk| blk.parent).filter(|&p| p < b);
        Some(b)
    })
}

fn item_text<'t>(text: &'t str, range: Range) -> Option<&'t str> {
    text.get(range.start as usize..range.end as usize)
}

struct Pass<'p, 'r, 'o, N> {
    names: &'p Logged<'r, 'o, N>,
    root: ItemKey,
    effects: Effects<'o>,
    plant: Plant,
    alloc: Alloc<'o>,
    /// The module of the body being read.
    module: ItemKey,
}

impl<'a, 'p, 'r, 'o, N: BodyNames<'a>> Pass<'p, 'r, 'o, N> {
    /// Why the body `text` is a surface body, or `None`.
    fn body(&mut self, text: &'a str) -> Option<SurfaceWhy> {
        let nested = scan(text, self.alloc);
        if nested.surface_by_text {
            return Some(SurfaceWhy::Text);
        }
        self.frame(text, nested)
    }

    /// Why the innermost of the frames (its calls, then its local items' texts) makes the body a
    /// surface body.
    fn frame(&mut self, text: &'a str, nested: Nested<'o>) -> Option<SurfaceWhy> {
        struct OwnedFrame<'a, 'o> {
            text: &'a str,
            nested: Nested<'o>,
            offset: u32,
            outer: (u32, u32),
            next_item: usize,
            calls_done: bool,
        }
        let mut owned: AVec<'o, OwnedFrame<'a, 'o>> = AVec::new_in(self.alloc);
        owned.push(OwnedFrame { text, nested, offset: 0, outer: (0, 0), next_item: 0, calls_done: false });
        loop {
            let Some(here) = owned.len().checked_sub(1) else {
                return None;
            };
            if let Some(top) = owned.last_mut()
                && !top.calls_done
            {
                top.calls_done = true;
                let mut views: blaze_own::FixedVec<Frame<'a, '_, 'o>, MAX_FRAMES> = blaze_own::FixedVec::new();
                for f in owned.iter() {
                    let _ = views.push(Frame { text: f.text, nested: &f.nested, offset: f.offset, outer: f.outer });
                }
                let f = views[here];
                for call in f.nested.calls.iter() {
                    if let Some(why) = self.call(views.as_slice(), here, call) {
                        return Some(why);
                    }
                }
            }
            let mut next_frame: Option<OwnedFrame<'a, 'o>> = None;
            let owned_len = owned.len();
            if let Some(top) = owned.last_mut() {
                while let Some(item) = top.nested.items.get(top.next_item).copied() {
                    top.next_item += 1;
                    // Kinds whose own text is read when lowered and may hold calls: everything but an
                    // impl and an extern block (already surface by text), a `use`, an `extern crate`,
                    // and a macro definition (its calls are its own callers').
                    if matches!(item.kind, NestedKind::Impl | NestedKind::ExternBlock | NestedKind::Use | NestedKind::ExternCrate | NestedKind::MacroRules | NestedKind::Macro) {
                        continue;
                    }
                    let Some(own) = item_text(top.text, item.range) else { return Some(SurfaceWhy::NotRead { call: item.range }) };
                    if !own.contains('!') {
                        continue;
                    }
                    let at = Range::new(top.offset + item.range.start, top.offset + item.range.end);
                    if owned_len >= MAX_FRAMES {
                        return Some(SurfaceWhy::Deep { call: at });
                    }
                    let inner = scan(own, self.alloc);
                    if inner.surface_by_text {
                        return Some(SurfaceWhy::Text);
                    }
                    next_frame = Some(OwnedFrame {
                        text: own,
                        nested: inner,
                        offset: top.offset + item.range.start,
                        outer: (item.block, item.range.start),
                        next_item: 0,
                        calls_done: false,
                    });
                    break;
                }
            }
            if let Some(frame) = next_frame {
                owned.push(frame);
            } else {
                owned.pop();
            }
        }
    }

    /// Why the call `call` of frame `here` makes the body a surface body.
    fn call(&mut self, frames: &[Frame<'a, '_, '_>], here: usize, call: &NestedCall) -> Option<SurfaceWhy> {
        let f = frames[here];
        let range = Range::new(f.offset + call.range.start, f.offset + call.range.end);
        let Some((path, inert)) = call_path(f.text, call, self.alloc) else { return Some(SurfaceWhy::NotRead { call: range }) };
        if !inert {
            return Some(SurfaceWhy::NotRead { call: range });
        }
        let at = At { frame: here, block: call.block, pos: call.range.start };
        let effect = self.effect_at(frames, at, path);
        if effect.emits_surface {
            return Some(SurfaceWhy::Emits { call: range });
        }
        if effect.binds_at_top {
            return Some(SurfaceWhy::Binds { call: range });
        }
        let offset = f.offset;
        let alloc = self.alloc;
        let mark = input_mark(
            f.text,
            call,
            &mut |p: WrittenPath<'a>, _: Range| {
                let e = self.effect_at(frames, at, p);
                !e.emits_surface && !e.binds_at_top
            },
            alloc,
        )?;
        let at = match mark {
            InputMark::ItemWord(_) => InputAt::ItemWord,
            InputMark::Attribute => InputAt::Attribute,
            InputMark::Call(r) => InputAt::Call(Range::new(offset + r.start, offset + r.end)),
            InputMark::Unread => InputAt::Unread,
        };
        Some(SurfaceWhy::Input { call: range, at })
    }

    /// The effect of the macro `path` names, read at `at`.
    fn effect_at(&mut self, frames: &[Frame<'a, '_, '_>], at: At, path: WrittenPath<'a>) -> MacroEffect {
        if self.plant == Plant::SkipEffect {
            return path.segments().last().map_or(MacroEffect::ANY, |&n| builtin_effect(n));
        }
        self.effect_of(frames, at, path, 0)
    }

    fn effect_of(&mut self, frames: &[Frame<'a, '_, '_>], at: At, path: WrittenPath<'a>, depth: u32) -> MacroEffect {
        if depth > MAX_USE_DEPTH {
            return MacroEffect::ANY;
        }
        let segments = path.segments();
        let Some(&first) = segments.first() else { return MacroEffect::ANY };
        if path.kind != PathKind::Plain {
            // `crate::`, `self::`, `super::`, `::`: a block is no module, so these are read at
            // the body's module (the Reference, "Paths", "Path qualifiers").
            return self.def_effect(self.names.macro_in(self.module, path.kind, segments));
        }
        let single = segments.len() == 1;
        if single && let Some((key, rules)) = self.textual(frames, at, first) {
            let Some(rules) = rules else { return MacroEffect::ANY };
            let root = self.root;
            let scratch = self.alloc;
            return self.effects.of_local(self.names, key, root, rules, scratch);
        }
        // Path-based: each block around the call, innermost first, through the frames.
        let mut found: Option<MacroEffect> = None;
        let mut point = Some(at);
        while let Some(p) = point {
            let f = frames[p.frame];
            for b in chain(f.nested, p.block) {
                match self.block_binding(frames, p.frame, b, path, single, depth) {
                    Binding::None => {}
                    Binding::Some(effect) => {
                        found = Some(effect);
                        break;
                    }
                }
            }
            if found.is_some() {
                break;
            }
            point = (p.frame > 0).then(|| At { frame: p.frame - 1, block: f.outer.0, pos: f.outer.1 });
        }
        match found {
            // A one-segment name bound in a block may still be read in the module's textual
            // scope first (a `macro_rules!` before the fn): both readings, joined.
            Some(effect) if single => {
                let module = self.macro_in_module(path);
                effect.or(if module.is_some() { self.def_effect(module) } else { MacroEffect::NONE })
            }
            Some(effect) => effect,
            None => {
                let module = self.macro_in_module(path);
                self.def_effect(module)
            }
        }
    }

    fn macro_in_module(&self, path: WrittenPath<'a>) -> Option<ItemKey> {
        self.names.macro_in(self.module, path.kind, path.segments())
    }

    /// A definition's effect; nothing named, or no source, is [`MacroEffect::ANY`].
    fn def_effect(&mut self, def: Option<ItemKey>) -> MacroEffect {
        let Some(def) = def else { return MacroEffect::ANY };
        let Some(src) = self.names.source(def) else { return MacroEffect::ANY };
        let scratch = self.alloc;
        self.effects.of(self.names, def, src, scratch)
    }

    /// The body's own `macro_rules!` named `name` in textual scope at `at`: the one written
    /// last before it in a block around it, through the frames (a local item's text sees the
    /// definitions before the item). Its key (for the walk's cycle cut only) and its rules.
    fn textual(&self, frames: &[Frame<'a, '_, '_>], at: At, name: &str) -> Option<(ItemKey, Option<&'a str>)> {
        let mut point = Some(at);
        while let Some(p) = point {
            let f = frames[p.frame];
            let mut best: Option<(u32, Option<&'a str>)> = None;
            for b in chain(f.nested, p.block) {
                for item in f.nested.items.iter().filter(|i| i.block == b && i.kind == NestedKind::MacroRules && i.range.end <= p.pos) {
                    // A `macro_rules!` whose name is not read may be this one: its rules are
                    // `None`, "anything", never skipped (rule 3).
                    let (n, rules) = match item_text(f.text, item.range).and_then(|own| macro_rules_parts(own, self.alloc)) {
                        Some((n, rules)) => (Some(n), Some(rules)),
                        None => (None, None),
                    };
                    if n.is_none_or(|n| n == name) && best.is_none_or(|(s, _)| s < item.range.start) {
                        best = Some((item.range.start, rules));
                    }
                }
            }
            if let Some((start, rules)) = best {
                let mut h = Hasher::new("blaze-scope surface local macro 1");
                h.update(&self.module.0.0).update(&(f.offset + start).to_le_bytes()).update(rules.unwrap_or("").as_bytes());
                return Some((ItemKey(h.finish()), rules));
            }
            point = (p.frame > 0).then(|| At { frame: p.frame - 1, block: f.outer.0, pos: f.outer.1 });
        }
        None
    }

    /// What block `b` of frame `fi` binds `path`'s first segment to: its items and named
    /// imports, then its globs.
    fn block_binding(&mut self, frames: &[Frame<'a, '_, '_>], fi: usize, b: u32, path: WrittenPath<'a>, single: bool, depth: u32) -> Binding {
        let f = frames[fi];
        let first = path.segments()[0];
        let mut globs: AVec<'_, (WrittenPath<'a>, u32)> = AVec::new_in(self.alloc);
        let mut named: Option<(WrittenPath<'a>, u32)> = None;
        for item in f.nested.items.iter().filter(|i| i.block == b) {
            let Some(own) = item_text(f.text, item.range) else { return Binding::Some(MacroEffect::ANY) };
            match item.kind {
                NestedKind::Use | NestedKind::ExternCrate => {
                    let start = item.range.start;
                    let whole = use_bindings(own, item.kind, self.alloc, &mut |u: UseBinding<'a>| match u.binds {
                        Some(n) if n == first => named = Some((u.path, start)),
                        Some(_) => {}
                        None => globs.push((u.path, start)),
                    });
                    // A `use` not read may bind any name.
                    if !whole {
                        return Binding::Some(MacroEffect::ANY);
                    }
                }
                // A `macro` (path-based) of the name, or a type or module the path goes
                // through: a block item, which only the body's own scope solves.
                NestedKind::Macro if single => {
                    if item_name(own, item.kind, self.alloc).is_none_or(|n| n == first) {
                        return Binding::Some(MacroEffect::ANY);
                    }
                }
                NestedKind::Mod | NestedKind::Struct | NestedKind::Enum | NestedKind::Union | NestedKind::Trait | NestedKind::TypeAlias if !single => {
                    if item_name(own, item.kind, self.alloc).is_none_or(|n| n == first) {
                        return Binding::Some(MacroEffect::ANY);
                    }
                }
                _ => {}
            }
        }
        if let Some((target, start)) = named {
            let Some(through) = path.through(&target) else { return Binding::Some(MacroEffect::ANY) };
            // The `use`'s own path is read from its block (2018: a block's names first).
            let at = At { frame: fi, block: b, pos: start };
            return Binding::Some(self.effect_of(frames, at, through, depth + 1));
        }
        for &(glob, _) in globs.iter() {
            // A glob whose path starts at a block's name: only the body's own scope solves it.
            let module = if glob.kind == PathKind::Plain && self.bound_in_blocks(frames, fi, b, glob.segments().first().copied()) {
                None
            } else {
                self.names.module_in(self.module, glob.kind, glob.segments())
            };
            let Some(module) = module else { return Binding::Some(MacroEffect::ANY) };
            let ns = if single { Namespace::Macros } else { Namespace::Types };
            if self.names.lookup(module, ns, first).is_some() {
                let def = self.names.macro_in(module, PathKind::SelfMod, path.segments());
                return Binding::Some(self.def_effect(def));
            }
        }
        Binding::None
    }

    /// Whether any block from `b` outward in frame `fi`, or around the frame, holds a `use`,
    /// `extern crate` or named item that binds `name`.
    fn bound_in_blocks(&self, frames: &[Frame<'a, '_, '_>], fi: usize, b: u32, name: Option<&str>) -> bool {
        let Some(name) = name else { return true };
        let mut point = Some((fi, b));
        while let Some((i, blk)) = point {
            let f = frames[i];
            for c in chain(f.nested, blk) {
                for item in f.nested.items.iter().filter(|it| it.block == c) {
                    let Some(own) = item_text(f.text, item.range) else { return true };
                    let hit = match item.kind {
                        NestedKind::Use | NestedKind::ExternCrate => {
                            let mut hit = false;
                            let whole = use_bindings(own, item.kind, self.alloc, &mut |u: UseBinding<'a>| hit |= u.binds.is_none_or(|n| n == name));
                            hit || !whole
                        }
                        kind => item_name(own, kind, self.alloc) == Some(name),
                    };
                    if hit {
                        return true;
                    }
                }
            }
            point = (i > 0).then(|| (i - 1, f.outer.0));
        }
        false
    }
}

/// What one block binds a path's first segment to.
enum Binding {
    None,
    Some(MacroEffect),
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use blaze_id::{ContentId, ItemKey};
    use blaze_surface::{Namespace, PathKind, Range};

    use super::{BodyAt, BodyNames, FileSurface, Plant, SurfaceWhy, file_surface};
    use crate::macro_effect::EffectNames;
    use crate::task::{MacroBody, MacroSrc};

    fn key(name: &str) -> ItemKey {
        ItemKey(ContentId::of("a surface test key", name.as_bytes()))
    }

    /// Two crates' finished names: `dep` (`format!`, an expression; `emits_impl!`, whose
    /// `$crate` callee writes an impl; `binds!`, a `macro_rules!` at the top) and `user`, whose
    /// root solved `use dep::emits_impl as format;` (so `format` there IS `emits_impl`) and
    /// `use dep::format as fmt_ok;`, and binds the crate name `dep`.
    struct World {
        names: Vec<(ItemKey, Namespace, &'static str, ItemKey)>,
        defs: Vec<(ItemKey, ItemKey, MacroSrc<'static>)>,
    }

    fn rules(name: &'static str, text: &'static str) -> MacroSrc<'static> {
        MacroSrc { name, body: MacroBody::Rules(text), module: key("dep"), export: true, local_inner: false }
    }

    fn world() -> World {
        let dep = key("dep");
        let user = key("user");
        let export = key("dep::__export");
        let mut names = Vec::new();
        let mut defs = Vec::new();
        let mut def = |name: &'static str, src: MacroSrc<'static>, at: ItemKey| {
            names.push((at, Namespace::Macros, name, key(name)));
            defs.push((key(name), dep, src));
        };
        def("format", rules("format", "($($a:tt)*) => { $crate::__export::inner!($($a)*) }"), dep);
        def("inner", rules("inner", "($($a:tt)*) => { 0 }"), export);
        def("emits_impl", rules("emits_impl", "() => { $crate::__export::writes!() }"), dep);
        def("writes", rules("writes", "() => { { impl $crate::Marker for u8 {} 0 } }"), export);
        def("binds", rules("binds", "() => { macro_rules! local { () => {} } }"), dep);
        names.push((dep, Namespace::Types, "__export", export));
        // The user crate's solved root: the rename, the plain name, the crate.
        names.push((user, Namespace::Macros, "format", key("emits_impl")));
        names.push((user, Namespace::Macros, "fmt_ok", key("format")));
        names.push((user, Namespace::Macros, "binds", key("binds")));
        names.push((user, Namespace::Macros, "emits_impl", key("emits_impl")));
        names.push((user, Namespace::Types, "dep", dep));
        World { names, defs }
    }

    impl World {
        fn find(&self, module: ItemKey, ns: Namespace, name: &str) -> Option<ItemKey> {
            self.names.iter().find(|n| n.0 == module && n.1 == ns && n.2 == name).map(|n| n.3)
        }

        fn walk(&self, module: ItemKey, kind: PathKind, segments: &[&str], last: Namespace) -> Option<ItemKey> {
            let mut m = match kind {
                PathKind::Plain | PathKind::SelfMod => module,
                PathKind::Crate => key("user"),
                _ => return None,
            };
            let (&name, through) = segments.split_last()?;
            for s in through {
                m = self.find(m, Namespace::Types, s)?;
            }
            self.find(m, last, name)
        }
    }

    impl EffectNames<'static> for World {
        fn root_of(&self, def: ItemKey) -> Option<ItemKey> {
            self.defs.iter().find(|d| d.0 == def).map(|d| d.1)
        }
        fn lookup(&self, module: ItemKey, ns: Namespace, name: &'static str) -> Option<ItemKey> {
            self.find(module, ns, name)
        }
        fn source(&self, def: ItemKey) -> Option<MacroSrc<'static>> {
            self.defs.iter().find(|d| d.0 == def).map(|d| d.2)
        }
    }

    impl BodyNames<'static> for World {
        fn macro_in(&self, module: ItemKey, kind: PathKind, segments: &[&'static str]) -> Option<ItemKey> {
            self.walk(module, kind, segments, Namespace::Macros)
        }
        fn module_in(&self, module: ItemKey, kind: PathKind, segments: &[&'static str]) -> Option<ItemKey> {
            self.walk(module, kind, segments, Namespace::Types)
        }
    }

    fn surface(text: &'static str, plant: Plant) -> FileSurface<'static> {
        let heap = blaze_own::testing::heap();
        let body = BodyAt { range: Range::new(0, u32::try_from(text.len()).unwrap_or(u32::MAX)), module: key("user") };
        file_surface(&world(), key("user"), text, &[body], plant, heap.alloc())
    }

    fn why(text: &'static str) -> Option<SurfaceWhy> {
        surface(text, Plant::None).rows.first().map(|r| r.why)
    }

    /// D189, point 4, each case with its twin: a body is a surface body by its text (a block
    /// impl; the twin a plain block struct), by a call whose definition emits (the twin
    /// `fmt_ok!`, `format!`-shaped), by a call that binds at top, by an input holding an item
    /// word (the twin an expression), by a nested call not proven (the twin a proven one), by a
    /// call through a block `use` (the twin through one to a safe macro), by a call of the
    /// body's own `macro_rules!` writing an impl (the twin one writing an expression), and by
    /// a call in a local fn's own body (the twin a safe one there). Prints nothing.
    #[test]
    fn each_surface_case_and_its_twin() {
        assert_eq!(why("fn f() { impl T for X {} }"), Some(SurfaceWhy::Text));
        assert_eq!(why("fn f() { struct S; let s = S; }"), None, "twin: a block struct alone");
        assert!(matches!(why("fn f() { emits_impl!(); }"), Some(SurfaceWhy::Emits { .. })));
        assert_eq!(why("fn f() { fmt_ok!(); }"), None, "twin: a format!-shaped definition");
        assert!(matches!(why("fn f() { binds!(); }"), Some(SurfaceWhy::Binds { .. })));
        assert!(matches!(why("fn f() { fmt_ok!(impl X for Y {}); }"), Some(SurfaceWhy::Input { .. })));
        assert_eq!(why("fn f() { fmt_ok!(1 + 2); }"), None, "twin: an expression input");
        assert!(matches!(why("fn f() { fmt_ok!(emits_impl!()); }"), Some(SurfaceWhy::Input { .. })));
        assert_eq!(why("fn f() { fmt_ok!(fmt_ok!(1)); }"), None, "twin: a nested call proven");
        assert!(matches!(why("fn f() { use dep::emits_impl as e; e!(); }"), Some(SurfaceWhy::Emits { .. })));
        assert_eq!(why("fn f() { use dep::format as e; e!(); }"), None, "twin: a block use of a safe macro");
        assert!(matches!(why("fn f() { macro_rules! m { () => { impl X for Y {} } } m!(); }"), Some(SurfaceWhy::Emits { .. })));
        assert_eq!(why("fn f() { macro_rules! m { () => { 1 } } m!(); }"), None, "twin: a local macro writing an expression");
        assert!(matches!(why("fn f() { fn inner() { emits_impl!(); } inner(); }"), Some(SurfaceWhy::Emits { .. })));
        assert_eq!(why("fn f() { fn inner() { fmt_ok!(); } inner(); }"), None, "twin: a safe call in a local fn");
        assert!(matches!(why("fn f() { nothing_named!(); }"), Some(SurfaceWhy::Emits { .. })), "a path naming nothing may do anything");
    }

    /// D189's proof (2): `use dep::emits_impl as format; fn g() { format!(); }` is a surface
    /// body: the module's solved names bind `format` to `emits_impl`. THE PLANTED BUG: the
    /// pass judging the call by its name (`Plant::SkipEffect`: `format` is a builtin's name
    /// whose effect is NONE) misses it, so this test fails under it. The record is the same
    /// for the same input, its reads sorted and not empty. Prints nothing.
    #[test]
    fn a_renamed_macro_is_read_by_its_definition() {
        let text = "fn g() { format!(); }";
        let real = surface(text, Plant::None);
        assert!(real.is_surface(0), "format! is emits_impl!: {:?}", real.rows);
        assert!(matches!(real.rows[0].why, SurfaceWhy::Emits { .. }));
        let planted = surface(text, Plant::SkipEffect);
        assert!(!planted.is_surface(0), "the planted bug must miss it, or this test proves nothing");
        let again = surface(text, Plant::None);
        assert_eq!(again.rows.as_slice(), real.rows.as_slice());
        assert_eq!(again.reads_digest(), real.reads_digest(), "the read list is deterministic");
        assert!(!real.reads.is_empty() && real.reads.windows(2).all(|w| w[0] < w[1]), "reads sorted, without repeats");
    }
}
