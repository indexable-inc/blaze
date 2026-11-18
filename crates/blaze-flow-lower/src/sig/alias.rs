//! A written type PATH is read for what it RESOLVES to, never for the erased type it lines up
//! with (piece sig-paths, lane B9).
//!
//! THE CLASS this closes (a soundness bug, not only false findings): the front end's heads keep
//! a type alias's EXPANSION (`type LaunchAlloc = SharedAlloc<'static>;` erases to
//! `SharedAlloc<_>`), and the reader lined the written `LaunchAlloc` up as if it were
//! `SharedAlloc` itself, written with no lifetime: so its lifetime was read as ELIDED. In an
//! output that took `&self`'s region where the truth is `'static` (every `<impl Grant for
//! X>::grant: from.scope("whole")` in bstd, a false `outlives`); in an INPUT, `x: LaunchAlloc`
//! was a fresh lifetime, so a caller could pass a shorter one where `'static` is demanded, and
//! the check accepted what rustc refuses (E0597/E0521). The same class silently swaps
//! `type Sw<'a, 'b> = Two<'b, 'a>;` and `type P<A, B> = Pair<B, A>;`.
//!
//! THE RULE (the Reference, "Type aliases": "A type alias defines a new name for an existing
//! type"; "Lifetime elision": elision applies to what a signature WRITES): every plain path
//! whose erased type has origin positions is resolved through the world ([`SigWorld`]):
//! - to the erased ADT itself: lined up as before (its lifetimes written or each elided);
//! - to a type alias: its own text `type Name<params> = T;` is lined up against the same erased
//!   type, the alias's lifetime parameters standing for what the path writes for them (elided,
//!   by the signature's rule, when it writes none), its type parameters for the path's type
//!   arguments, which are lined up in the signature's own text and mode. A lifetime the alias
//!   writes itself (`'static`) is the alias's, and is NOT a lifetime the signature uses
//!   (rustc: `fn f(x: L) -> &u8` with `type L = &'static u8;` is E0106): those positions are
//!   `unwritten` for elision;
//! - to anything else, or not at all: refused by name ([`SigRefusal::Path`]), never guessed.
//!
//! An alias of an alias expands again, at most [`MOST_DEPTH`] deep (bounded recursion: one
//! alias text per level). A trait object written bare in an alias's text takes the alias's own
//! context's default (`'static` at its top: "Default trait object lifetimes"), not the use's.

use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_typeck::{FloatTy, IntTy, ItemKey, PNode, PTy, UintTy};

use super::impl_life::ImplLife;
use super::{Ctx, Mode, Pair, Reader, Region, SigRefusal, Tk, lex};

/// The most aliases one written type expands through (an alias of an alias ..).
pub const MOST_DEPTH: u32 = 16;

/// Where an alias's own text resolves its paths: a token the world gave with the alias
/// ([`PathIs::Alias`]) and takes back in [`SigWorld::type_path`]. Its meaning is the world's.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct AliasScope(pub u128);

/// What a written type path resolves to, in the type namespace.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathIs<'w> {
    /// A struct, enum or union: its checker key.
    Adt(ItemKey),
    /// A type alias: where its text resolves its own paths, and its whole item text
    /// (`[attrs] [pub] type Name<params> = T;`).
    Alias { scope: AliasScope, text: &'w str },
    /// The world states that no type alias exists where this path is read: the path names the
    /// erased type itself. ONLY a world whose texts are known to hold no alias says this ([`NoAliases`]).
    NotAlias,
    /// Something else: a type parameter, a primitive, a trait, a module.
    Other,
    /// The path does not resolve.
    Unresolved,
}

/// What the reader asks of the world besides the erased types: how many lifetime parameters an
/// ADT's head declares, and what a written type path resolves to. `from`: `None` for the item
/// being read (a fn, its impl's header and member texts: one module), `Some` inside an alias's
/// text.
pub trait SigWorld {
    fn adt_lifetimes(&self, adt: ItemKey) -> Option<u32>;
    fn type_path(&self, from: Option<AliasScope>, absolute: bool, segments: &[&str]) -> PathIs<'_>;
}

/// The world of a text that names no type alias: every path is the erased type's own
/// ([`PathIs::NotAlias`]). What the older entry points (`read_signature` and its siblings, taking
/// only an ADT lifetime count) read with: right for fixtures, WRONG for real code, whose callers
/// pass a resolving world to the `_with` entry points.
pub struct NoAliases<'f>(pub &'f dyn Fn(ItemKey) -> Option<u32>);

impl SigWorld for NoAliases<'_> {
    fn adt_lifetimes(&self, adt: ItemKey) -> Option<u32> {
        (self.0)(adt)
    }
    fn type_path(&self, _: Option<AliasScope>, _: bool, _: &[&str]) -> PathIs<'_> {
        PathIs::NotAlias
    }
}

/// Why a written path was not read ([`SigRefusal::Path`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathWhy {
    /// The world did not resolve it.
    Unresolved = 0,
    /// It resolves to something that is neither the erased ADT nor a type alias.
    NotAType = 1,
    /// It resolves to an ADT other than the erased one.
    OtherAdt = 2,
    /// The alias's text is not `type Name<params> = T;` as the reader takes it.
    AliasForm = 3,
    /// The path's arguments do not match the alias's parameters (their count, a default, a
    /// binding `Name = T`).
    AliasArgs = 4,
    /// An elided lifetime in the alias's text (E0106).
    AliasElided = 5,
    /// A lifetime in the alias's text that is not one of its parameters.
    AliasLifetime = 6,
    /// The alias's written type and the erased type are not the same form.
    AliasUnaligned = 7,
    /// Two uses of one alias type parameter with different position counts.
    AliasHole = 8,
}

/// A use of an alias's type parameter met while lining up the alias's text: the erased type
/// there, its first position, its context.
#[derive(Clone, Copy)]
pub(super) struct Hole {
    param: u32,
    ty: PTy,
    base: u32,
    ctx: Ctx,
}

/// An alias's text as the reader takes it.
struct AliasForm<A: Allocator + Clone> {
    /// The tokens of its lifetime parameters' names, in order.
    lifetimes: Vec<u32, A>,
    /// Its type and const parameters, in order: each name's token.
    params: Vec<u32, A>,
    /// Its written type's tokens.
    rhs: (u32, u32),
}

impl<'t, 'w, A: Allocator + Clone> Reader<'t, 'w, A> {
    /// `lo..hi` as a plain type path, `[::] a :: b :: Name [<..>]`: whether it is absolute, where
    /// its names end, and the inside of its last segment's `<..>`. `None`: no plain path.
    fn plain_path(&self, lo: u32, hi: u32) -> Option<(bool, u32, Option<(u32, u32)>)> {
        if lo >= hi {
            return None;
        }
        let (name_end, args) = if self.is(hi - 1, hi, Tk::P(b'>')) {
            let open = self.mate[hi as usize - 1];
            if open == u32::MAX || open <= lo || open >= hi {
                return None;
            }
            (open, Some((open + 1, hi - 1)))
        } else {
            (hi, None)
        };
        let mut i = lo;
        let absolute = self.is(i, name_end, Tk::Sep);
        if absolute {
            i += 1;
        }
        loop {
            if !self.is(i, name_end, Tk::Ident) || matches!(self.word(i), "dyn" | "impl" | "fn" | "Self" | "mut" | "unsafe" | "extern" | "for") {
                return None;
            }
            i += 1;
            if i == name_end {
                break;
            }
            if !self.is(i, name_end, Tk::Sep) {
                return None;
            }
            i += 1;
        }
        Some((absolute, name_end, args))
    }

    /// The use of an alias type parameter `p` is, when this reader reads an alias's text and
    /// `p` is one bare name of its parameters: noted for the use's reader to fill.
    pub(super) fn hole(&mut self, p: Pair) -> bool {
        if self.holes.is_empty() || p.hi != p.lo + 1 || !self.is(p.lo, p.hi, Tk::Ident) {
            return false;
        }
        let w = self.word(p.lo);
        let Some(j) = self.holes.iter().position(|h| *h == w) else { return false };
        self.found.push(Hole { param: j as u32, ty: p.ty, base: p.base, ctx: p.ctx });
        true
    }

    /// `p`'s written type when it is a plain path: resolved (the module doc). `Ok(true)`: an
    /// alias, expanded into `regions[p.base..]`; `Ok(false)`: it names the erased type itself
    /// (or is no plain path, or the erased type is an opaque or a projection, which the caller's
    /// `Hidden` arm takes): the caller lines it up.
    pub(super) fn path_or_alias(&mut self, p: Pair, mode: Mode, regions: &mut Vec<Region, A>) -> Result<bool, SigRefusal> {
        let node = self.node(p.ty)?;
        if matches!(node, PNode::Opaque(_, _) | PNode::Alias { .. }) {
            return Ok(false);
        }
        let Some((absolute, name_end, args)) = self.plain_path(p.lo, p.hi) else { return Ok(false) };
        let at = self.at(p.lo);
        let mut segments: Vec<&'t str, A> = Vec::new_in(self.alloc.clone());
        for i in p.lo..name_end {
            if self.toks[i as usize].k == Tk::Ident {
                segments.push(self.word(i));
            }
        }
        let world: &'w dyn super::SigWorld = self.world;
        let refuse = |why: PathWhy| SigRefusal::Path { at, why };
        // A name no item has is a generic parameter's or a primitive type's: the type namespace
        // tries items first (an item `u8` would shadow the primitive), then these. Only a lone
        // relative segment can be one, and only against the node it names.
        let builtin = !absolute && segments.len() == 1 && args.is_none() && builtin_word(node, segments[0]);
        match (world.type_path(self.from, absolute, &segments), node) {
            (PathIs::Adt(k), PNode::Adt(erased, _)) if k == erased => Ok(false),
            (PathIs::NotAlias, _) => Ok(false),
            (PathIs::Unresolved, _) if builtin => Ok(false),
            (PathIs::Alias { scope, text }, _) => {
                self.expand_alias(p, mode, scope, text, args, regions)?;
                Ok(true)
            }
            (PathIs::Adt(_), _) => Err(refuse(PathWhy::OtherAdt)),
            (PathIs::Other, _) => Err(refuse(PathWhy::NotAType)),
            (PathIs::Unresolved, _) => Err(refuse(PathWhy::Unresolved)),
        }
    }

    /// The alias text's form: `type Name [<params>] = T [where ..] ;`.
    fn alias_form(&self) -> Result<AliasForm<A>, PathWhy> {
        let end = self.toks.len() as u32;
        let mut i = 0u32;
        while i < end && !self.is_word(i, end, "type") {
            i = self.skip(i);
        }
        if !self.is(i + 1, end, Tk::Ident) {
            return Err(PathWhy::AliasForm);
        }
        let mut form = AliasForm { lifetimes: Vec::new_in(self.alloc.clone()), params: Vec::new_in(self.alloc.clone()), rhs: (0, 0) };
        let mut j = i + 2;
        if self.is(j, end, Tk::P(b'<')) {
            let close = self.mate[j as usize];
            if close == u32::MAX || close <= j {
                return Err(PathWhy::AliasForm);
            }
            let mut parts: Vec<(u32, u32), A> = Vec::new_in(self.alloc.clone());
            self.pieces(j + 1, close, &mut parts);
            for &(lo, hi) in parts.iter() {
                if self.is(lo, hi, Tk::Life) {
                    form.lifetimes.push(lo);
                } else if self.is_word(lo, hi, "const") && self.is(lo + 1, hi, Tk::Ident) {
                    form.params.push(lo + 1);
                } else if self.is(lo, hi, Tk::Ident) {
                    // A default (`T = u8`) is a parameter the path may omit: not read.
                    let mut k = lo;
                    while k < hi {
                        if self.toks[k as usize].k == Tk::P(b'=') {
                            return Err(PathWhy::AliasArgs);
                        }
                        k = self.skip(k);
                    }
                    form.params.push(lo);
                } else {
                    return Err(PathWhy::AliasForm);
                }
            }
            j = close + 1;
        }
        if !self.is(j, end, Tk::P(b'=')) {
            return Err(PathWhy::AliasForm);
        }
        let lo = j + 1;
        let mut k = lo;
        while k < end && !self.is(k, end, Tk::P(b';')) && !self.is_word(k, end, "where") {
            k = self.skip(k);
        }
        if k == lo {
            return Err(PathWhy::AliasForm);
        }
        form.rhs = (lo, k);
        Ok(form)
    }

    /// The alias `text` (resolving in `scope`) written at `p` with the path arguments `args`,
    /// expanded into `regions[p.base..]` (the module doc).
    fn expand_alias(&mut self, p: Pair, mode: Mode, scope: AliasScope, text: &'w str, args: Option<(u32, u32)>, regions: &mut Vec<Region, A>) -> Result<(), SigRefusal> {
        let at = self.at(p.lo);
        let refuse = |why: PathWhy| SigRefusal::Path { at, why };
        if self.depth >= MOST_DEPTH {
            return Err(SigRefusal::Bound);
        }
        let mut sub: Reader<'w, 'w, A> = Reader {
            text,
            tys: self.tys,
            world: self.world,
            toks: Vec::new_in(self.alloc.clone()),
            mate: Vec::new_in(self.alloc.clone()),
            names: Vec::new_in(self.alloc.clone()),
            anon: 0,
            hidden: 0,
            alloc: self.alloc.clone(),
            imp: None,
            unwritten: Vec::new_in(self.alloc.clone()),
            from: Some(scope),
            holes: Vec::new_in(self.alloc.clone()),
            found: Vec::new_in(self.alloc.clone()),
            depth: self.depth + 1,
        };
        lex(text, &mut sub.toks).map_err(|_| refuse(PathWhy::AliasForm))?;
        sub.pair_brackets().map_err(|_| refuse(PathWhy::AliasForm))?;
        let form = sub.alias_form().map_err(refuse)?;
        // The path's arguments: its lifetimes (all of the alias's, or none: each elided) and
        // its type and const arguments, one per parameter.
        let mut parts: Vec<(u32, u32), A> = Vec::new_in(self.alloc.clone());
        match args {
            Some((lo, hi)) => self.pieces(lo, hi, &mut parts),
            None => parts.clear(),
        }
        let mut lifes: Vec<u32, A> = Vec::new_in(self.alloc.clone());
        let mut types: Vec<(u32, u32), A> = Vec::new_in(self.alloc.clone());
        for &(lo, hi) in parts.iter() {
            if hi == lo + 1 && self.is(lo, hi, Tk::Life) {
                lifes.push(lo);
            } else if self.is(lo + 1, hi, Tk::P(b'=')) {
                return Err(refuse(PathWhy::AliasArgs));
            } else {
                types.push((lo, hi));
            }
        }
        if (!lifes.is_empty() && lifes.len() != form.lifetimes.len()) || types.len() != form.params.len() {
            return Err(refuse(PathWhy::AliasArgs));
        }
        // Each alias lifetime parameter: what the path writes for it, or elided by the
        // signature's rule. These are lifetimes the signature uses.
        let mut names: Vec<(&'w str, Region), A> = Vec::new_in(self.alloc.clone());
        let mut used: Vec<Region, A> = Vec::new_in(self.alloc.clone());
        for (q, &tok) in form.lifetimes.iter().enumerate() {
            let written = match lifes.get(q) {
                Some(&t) => self.named(t),
                None => None,
            };
            let r = match written {
                Some(r) => r,
                None => self.elided(mode, p.lo)?,
            };
            names.push((sub.word(tok), r));
            used.push(r);
        }
        for &tok in form.params.iter() {
            let w = sub.word(tok);
            sub.holes.push(w);
        }
        sub.imp = Some(ImplLife::of_alias(names, self.alloc.clone()));
        let n = self.count(p.ty)?;
        let mut got: Vec<Region, A> = Vec::new_in(self.alloc.clone());
        got.resize(n as usize, Region::Hidden(u32::MAX));
        // No elision in an alias's text (E0106); a bare trait object there is `'static`.
        sub.line_up(Pair { ty: p.ty, lo: form.rhs.0, hi: form.rhs.1, base: 0, ctx: Ctx::Adt(false), whole_self: false }, Mode::Output(None), &mut got)
            .map_err(|e| match e {
                SigRefusal::ElisionUndecided { .. } => refuse(PathWhy::AliasElided),
                SigRefusal::Bound => SigRefusal::Bound,
                SigRefusal::Path { why, .. } => refuse(why),
                _ => refuse(PathWhy::AliasUnaligned),
            })?;
        if !sub.names.is_empty() {
            return Err(refuse(PathWhy::AliasLifetime));
        }
        // The alias's own positions: its regions, offset hidden ones; `unwritten` unless a
        // lifetime the path wrote or elided. The holes stay to fill.
        let mut in_hole: Vec<bool, A> = Vec::new_in(self.alloc.clone());
        in_hole.resize(n as usize, false);
        for h in sub.found.iter() {
            let len = self.count(h.ty)?;
            for k in h.base..h.base + len {
                if let Some(x) = in_hole.get_mut(k as usize) {
                    *x = true;
                }
            }
        }
        for k in 0..n {
            if in_hole[k as usize] {
                continue;
            }
            let x = match got[k as usize] {
                Region::Hidden(u32::MAX) => return Err(refuse(PathWhy::AliasUnaligned)),
                Region::Hidden(h) => Region::Hidden(self.hidden + h),
                x => x,
            };
            regions[(p.base + k) as usize] = x;
            if !used.contains(&x) {
                self.unwritten.push(p.base + k);
            }
        }
        self.hidden += sub.hidden;
        // Each type parameter's first use is the path's argument lined up in this text and
        // mode; a later use of the same parameter is the same type: its regions copied.
        let mut first: Vec<(u32, u32, u32), A> = Vec::new_in(self.alloc.clone());
        for j in 0..sub.found.len() {
            let h = sub.found[j];
            let len = self.count(h.ty)?;
            let to = p.base + h.base;
            match first.iter().find(|f| f.0 == h.param).copied() {
                Some((_, from, l)) => {
                    if l != len {
                        return Err(refuse(PathWhy::AliasHole));
                    }
                    for k in 0..len {
                        regions[(to + k) as usize] = regions[(from + k) as usize];
                        if self.unwritten.contains(&(from + k)) {
                            self.unwritten.push(to + k);
                        }
                    }
                }
                None => {
                    let (lo, hi) = types[h.param as usize];
                    self.line_up(Pair { ty: h.ty, lo, hi, base: to, ctx: h.ctx, whole_self: false }, mode, regions)?;
                    first.push((h.param, to, len));
                }
            }
        }
        Ok(())
    }
}

/// Whether `word` names `node` with no item behind it: a generic parameter (its name is not
/// kept in the erased node, and a parameter's name is never an item's) or the primitive type it
/// spells.
fn builtin_word(node: PNode, word: &str) -> bool {
    let spelled = match node {
        PNode::Param(_) | PNode::ConstParam(_) => return true,
        PNode::Bool => "bool",
        PNode::Char => "char",
        PNode::Str => "str",
        PNode::Int(i) => match i {
            IntTy::Isize => "isize",
            IntTy::I8 => "i8",
            IntTy::I16 => "i16",
            IntTy::I32 => "i32",
            IntTy::I64 => "i64",
            IntTy::I128 => "i128",
        },
        PNode::Uint(u) => match u {
            UintTy::Usize => "usize",
            UintTy::U8 => "u8",
            UintTy::U16 => "u16",
            UintTy::U32 => "u32",
            UintTy::U64 => "u64",
            UintTy::U128 => "u128",
        },
        PNode::Float(f) => match f {
            FloatTy::F16 => "f16",
            FloatTy::F32 => "f32",
            FloatTy::F64 => "f64",
            FloatTy::F128 => "f128",
        },
        _ => return false,
    };
    spelled == word
}
