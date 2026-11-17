//! The lifetime part of a fn's signature: which region stands at each origin position of each
//! input and of the output, and the outlives relations the signature declares (flow.md, "What
//! the signature must say (H1)").
//!
//! [`SigLife`] is the shape the lowering takes, whoever makes it. The front end's heads erase
//! lifetimes today, so [`read_signature`] is a STAND-IN that reads them from the fn's own text
//! and lines them up with the erased types' positions. It refuses by name whatever it cannot line
//! up exactly (an alias to a type with references, an ADT written with some but not all of its
//! lifetime arguments, a default object bound it cannot know); it never guesses a region. An
//! ADT has one origin position per lifetime parameter its head declares (blaze-ladder front2
//! lifetimes.rs `Grain::PerParam`). Its known limits are in flow.md.
//!
//! The rules it applies are the Reference's "Lifetime elision": "Each elided lifetime in the
//! parameters becomes a distinct lifetime parameter"; "If there is exactly one lifetime used in
//! the parameters (elided or not), that lifetime is assigned to all elided output lifetimes"; "If
//! the receiver has type `&Self` or `&mut Self`, then the lifetime of that reference to `Self` is
//! assigned to all elided output lifetime parameters"; and "Default trait object lifetimes": "If
//! the trait object is used as `&'a dyn Trait`, the default is `'a`", else `'static` when the
//! containing type gives no bound.

use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_typeck::{ItemKey, PNode, PTy, PTys};

/// A member of an impl: `Self::Name` and the impl's lifetimes (piece sig-assoc-projection).
mod impl_life;
mod outlives;
mod value;

pub use impl_life::{AssocWhy, ImplText, impl_header_end, read_impl_signature};
pub use outlives::container_declares_type_outlives;

pub use value::{CallableRefusal, read_callable_param};

/// What stands at one origin position of a signature.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Region {
    /// `'static`.
    Static,
    /// A lifetime the signature names (its own or its impl's): numbered by first appearance.
    Named(u32),
    /// An elided input lifetime: each its own parameter.
    Anon(u32),
    /// Position `i` of `Self`: the impl's, the same in every `Self` of one signature.
    SelfTy(u32),
    /// A position whose lifetimes the text does not say (an opaque or associated type): related
    /// to nothing in the fn's own body; a call through such a signature is not instantiated.
    Hidden(u32),
}

/// One signature's lifetime part, borrowed from whoever holds it.
#[derive(Clone, Copy, Debug)]
pub struct SigLife<'s> {
    /// The regions of every input's positions, in order, then the output's.
    pub regions: &'s [Region],
    /// Per input, where its regions end in `regions`; the output's are the rest.
    pub ends: &'s [u32],
    /// `(a, b)`: the signature declares `a: b`.
    pub bounds: &'s [(Region, Region)],
}

/// The owned form [`read_signature`] fills.
pub struct SigBuf<A: Allocator + Clone> {
    pub regions: Vec<Region, A>,
    pub ends: Vec<u32, A>,
    pub bounds: Vec<(Region, Region), A>,
    /// `(name, region)`: the fn declares `name: region` on a type parameter (`T: 'a`,
    /// `where T: Tr + 'static`), the name as written (outlives.rs).
    pub type_outlives: Vec<(blaze_typeck::Name, Region), A>,
    /// A type-outlives bound whose left side is not one bare name (`Self: 'a`, `&'b T: 'a`).
    pub type_outlives_unread: bool,
}

impl<A: Allocator + Clone> SigBuf<A> {
    pub fn new_in(alloc: A) -> SigBuf<A> {
        SigBuf {
            regions: Vec::new_in(alloc.clone()),
            ends: Vec::new_in(alloc.clone()),
            bounds: Vec::new_in(alloc.clone()),
            type_outlives: Vec::new_in(alloc),
            type_outlives_unread: false,
        }
    }

    pub fn life(&self) -> SigLife<'_> {
        SigLife { regions: &self.regions, ends: &self.ends, bounds: &self.bounds }
    }
}

/// Why a signature's text was not read: the rule, and the byte offset in the fn's text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SigRefusal {
    /// A literal or a bracket the reader cannot close.
    Token { at: u32 },
    /// No `fn name ( .. )` in the text.
    NoFn,
    /// The text's parameters are not as many as the head's inputs.
    ParamCount { text: u32, head: u32 },
    /// The written type and the erased type are not the same form (an alias, a qualified path,
    /// a default argument): positions cannot be lined up.
    Unaligned { at: u32 },
    /// An elided output lifetime that neither rule of the Reference decides.
    ElisionUndecided { at: u32 },
    /// A trait object whose default lifetime depends on a type's declared bounds.
    ObjectDefault { at: u32 },
    /// The world did not say how many lifetimes an ADT has, or a type node is missing.
    Unknown,
    /// A closure or coroutine type in a signature.
    TypeNode,
    /// A walk passed its bound.
    Bound,
    /// `Self::Name` (or `<Self as Tr>::Name`), or the header of the impl a member is read with,
    /// that was not read, and why (impl_life.rs); `at`: the path's offset in the fn's text, 0
    /// for the header.
    Assoc { at: u32, why: AssocWhy },
}

/// The reader's bound on any one walk (tokens, types): a signature past it is refused.
const MOST: u32 = 1 << 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tk {
    Ident,
    Life,
    Arrow,
    Sep,
    Lit,
    P(u8),
}

#[derive(Clone, Copy, Debug)]
struct Tok {
    k: Tk,
    lo: u32,
    hi: u32,
}

fn ident_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic() || c >= 0x80
}

fn ident_continue(c: u8) -> bool {
    ident_start(c) || c.is_ascii_digit()
}

/// The end of the string literal whose opening quote is at `i`.
fn string_end(b: &[u8], i: usize) -> Result<usize, SigRefusal> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Ok(j + 1),
            _ => j += 1,
        }
    }
    Err(SigRefusal::Token { at: i as u32 })
}

/// The end of the raw string whose first `#` or quote is at `i`, or `None` when it is not one
/// (`r#name`, a raw identifier).
fn raw_string_end(b: &[u8], i: usize) -> Result<Option<usize>, SigRefusal> {
    let mut j = i;
    let mut hashes = 0usize;
    while j < b.len() && b[j] == b'#' {
        hashes += 1;
        j += 1;
    }
    if b.get(j) != Some(&b'"') {
        return Ok(None);
    }
    j += 1;
    while j < b.len() {
        if b[j] == b'"' && j + hashes < b.len() && b[j + 1..j + 1 + hashes].iter().all(|c| *c == b'#') {
            return Ok(Some(j + 1 + hashes));
        }
        j += 1;
    }
    Err(SigRefusal::Token { at: i as u32 })
}

/// The tokens of `text` up to the signature's end: the first `{` or `;` outside `(` and `[`
/// after the `fn` keyword. Comments are skipped; attributes stay (the parser steps over them).
fn lex<A: Allocator + Clone>(text: &str, toks: &mut Vec<Tok, A>) -> Result<(), SigRefusal> {
    let b = text.as_bytes();
    let mut i = 0usize;
    let mut seen_fn = false;
    let mut depth = 0i32;
    while i < b.len() {
        if toks.len() as u32 > MOST {
            return Err(SigRefusal::Bound);
        }
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut open = 1u32;
            i += 2;
            while i < b.len() && open > 0 {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    open += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    open -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        let lo = i;
        if c == b'"' {
            i = string_end(b, i)?;
            toks.push(Tok { k: Tk::Lit, lo: lo as u32, hi: i as u32 });
            continue;
        }
        if ident_start(c) {
            while i < b.len() && ident_continue(b[i]) {
                i += 1;
            }
            let word = &b[lo..i];
            let prefix = matches!(word, b"r" | b"b" | b"br" | b"c" | b"cr");
            if prefix && b.get(i) == Some(&b'"') && !matches!(word, b"r" | b"br" | b"cr") {
                i = string_end(b, i)?;
                toks.push(Tok { k: Tk::Lit, lo: lo as u32, hi: i as u32 });
                continue;
            }
            if matches!(word, b"r" | b"br" | b"cr")
                && matches!(b.get(i), Some(&b'"') | Some(&b'#'))
                && let Some(end) = raw_string_end(b, i)?
            {
                i = end;
                toks.push(Tok { k: Tk::Lit, lo: lo as u32, hi: i as u32 });
                continue;
            }
            if word == b"fn" && depth == 0 {
                seen_fn = true;
            }
            toks.push(Tok { k: Tk::Ident, lo: lo as u32, hi: i as u32 });
            continue;
        }
        if c.is_ascii_digit() {
            while i < b.len() && ident_continue(b[i]) {
                i += 1;
            }
            toks.push(Tok { k: Tk::Lit, lo: lo as u32, hi: i as u32 });
            continue;
        }
        if c == b'\'' {
            if b.get(i + 1).is_some_and(|n| ident_start(*n)) {
                let mut j = i + 1;
                while j < b.len() && ident_continue(b[j]) {
                    j += 1;
                }
                if b.get(j) == Some(&b'\'') {
                    i = j + 1;
                    toks.push(Tok { k: Tk::Lit, lo: lo as u32, hi: i as u32 });
                } else {
                    i = j;
                    toks.push(Tok { k: Tk::Life, lo: lo as u32, hi: i as u32 });
                }
                continue;
            }
            let mut j = i + 1;
            if b.get(j) == Some(&b'\\') {
                j += 2;
            }
            while j < b.len() && b[j] != b'\'' {
                j += 1;
            }
            if j >= b.len() {
                return Err(SigRefusal::Token { at: lo as u32 });
            }
            i = j + 1;
            toks.push(Tok { k: Tk::Lit, lo: lo as u32, hi: i as u32 });
            continue;
        }
        if c == b'-' && b.get(i + 1) == Some(&b'>') {
            i += 2;
            toks.push(Tok { k: Tk::Arrow, lo: lo as u32, hi: i as u32 });
            continue;
        }
        if c == b':' && b.get(i + 1) == Some(&b':') {
            i += 2;
            toks.push(Tok { k: Tk::Sep, lo: lo as u32, hi: i as u32 });
            continue;
        }
        match c {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'{' | b';' if seen_fn && depth == 0 => break,
            _ => {}
        }
        i += 1;
        toks.push(Tok { k: Tk::P(c), lo: lo as u32, hi: i as u32 });
    }
    Ok(())
}

/// Where an elided lifetime is being read.
#[derive(Clone, Copy)]
enum Mode {
    Input,
    Output(Option<Region>),
}

/// What encloses a type: a trait object's default lifetime reads it.
#[derive(Clone, Copy)]
enum Ctx {
    Ref(Region),
    /// A type argument of an ADT, which has lifetimes or not.
    Adt(bool),
    Other,
}

/// One pair still to line up: an erased type, the tokens `lo..hi` of its written type, and the
/// index in `regions` of its first position. `whole_self`: the written type is `Self`.
#[derive(Clone, Copy)]
struct Pair {
    ty: PTy,
    lo: u32,
    hi: u32,
    base: u32,
    ctx: Ctx,
    whole_self: bool,
}

struct Reader<'t, 'w, A: Allocator + Clone> {
    text: &'t str,
    tys: PTys<'w>,
    adt_lifetimes: &'w dyn Fn(ItemKey) -> Option<u32>,
    toks: Vec<Tok, A>,
    /// Per token, its closing or opening bracket's index (`u32::MAX` for the rest).
    mate: Vec<u32, A>,
    /// The lifetime names met, as token indices of their first appearance.
    names: Vec<u32, A>,
    anon: u32,
    hidden: u32,
    alloc: A,
    /// The impl the fn is a member of, when its text was given (impl_life.rs): its lifetimes
    /// by name and its associated type members.
    imp: Option<impl_life::ImplLife<'t, A>>,
    /// The positions whose regions the fn's own text did not write (a `Self`'s, a
    /// `Self::Name`'s): elision counts only what is written.
    unwritten: Vec<u32, A>,
}

impl<'t, 'w, A: Allocator + Clone> Reader<'t, 'w, A> {
    fn word(&self, i: u32) -> &'t str {
        let t = self.toks[i as usize];
        self.text.get(t.lo as usize..t.hi as usize).unwrap_or("")
    }

    fn at(&self, i: u32) -> u32 {
        match self.toks.get(i as usize) {
            Some(t) => t.lo,
            None => self.text.len() as u32,
        }
    }

    fn is(&self, i: u32, hi: u32, k: Tk) -> bool {
        i < hi && self.toks[i as usize].k == k
    }

    fn is_word(&self, i: u32, hi: u32, w: &str) -> bool {
        self.is(i, hi, Tk::Ident) && self.word(i) == w
    }

    /// The token after `i`, stepping over a bracketed group as one.
    fn skip(&self, i: u32) -> u32 {
        let m = self.mate[i as usize];
        if m != u32::MAX && m > i { m + 1 } else { i + 1 }
    }

    fn pair_brackets(&mut self) -> Result<(), SigRefusal> {
        let mut stack: Vec<u32, A> = Vec::new_in(self.alloc.clone());
        self.mate.resize(self.toks.len(), u32::MAX);
        for i in 0..self.toks.len() {
            let Tk::P(c) = self.toks[i].k else { continue };
            let open = match c {
                b'(' | b'[' | b'<' | b'{' => {
                    stack.push(i as u32);
                    continue;
                }
                b')' => b'(',
                b']' => b'[',
                b'>' => b'<',
                b'}' => b'{',
                _ => continue,
            };
            match stack.pop() {
                Some(o) if self.toks[o as usize].k == Tk::P(open) => {
                    self.mate[o as usize] = i as u32;
                    self.mate[i] = o;
                }
                _ => return Err(SigRefusal::Token { at: self.toks[i].lo }),
            }
        }
        match stack.pop() {
            Some(o) => Err(SigRefusal::Token { at: self.toks[o as usize].lo }),
            None => Ok(()),
        }
    }

    /// The pieces of `lo..hi` between its top-level commas (an empty last piece is dropped).
    fn pieces(&self, lo: u32, hi: u32, out: &mut Vec<(u32, u32), A>) {
        out.clear();
        let (mut i, mut from) = (lo, lo);
        while i < hi {
            if self.toks[i as usize].k == Tk::P(b',') {
                out.push((from, i));
                from = i + 1;
                i += 1;
            } else {
                i = self.skip(i);
            }
        }
        if from < hi {
            out.push((from, hi));
        }
    }

    /// The region a written lifetime token names; `None` for `'_`.
    fn named(&mut self, i: u32) -> Option<Region> {
        let w = self.word(i);
        if w == "'_" {
            return None;
        }
        // A lifetime of the impl, held by its self type: that position of `Self` (impl_life.rs).
        if let Some(r) = self.imp.as_ref().and_then(|m| m.region_of(w)) {
            return Some(r);
        }
        if w == "'static" {
            return Some(Region::Static);
        }
        for (n, &first) in self.names.iter().enumerate() {
            if self.word(first) == w {
                return Some(Region::Named(n as u32));
            }
        }
        self.names.push(i);
        Some(Region::Named(self.names.len() as u32 - 1))
    }

    fn elided(&mut self, mode: Mode, at: u32) -> Result<Region, SigRefusal> {
        match mode {
            Mode::Input => {
                self.anon += 1;
                Ok(Region::Anon(self.anon - 1))
            }
            Mode::Output(Some(r)) => Ok(r),
            Mode::Output(None) => Err(SigRefusal::ElisionUndecided { at: self.at(at) }),
        }
    }

    fn node(&self, t: PTy) -> Result<PNode, SigRefusal> {
        self.tys.node(t).map_err(|_| SigRefusal::Unknown)
    }

    /// The number of origin positions of `t`, as the lowering counts them: one per reference,
    /// one per lifetime parameter of an ADT, one per opaque, alias or trait object.
    fn count(&self, t: PTy) -> Result<u32, SigRefusal> {
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        stack.push(t);
        let (mut n, mut steps) = (0u32, 0u32);
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST {
                return Err(SigRefusal::Bound);
            }
            match self.node(t)? {
                PNode::Ref(_, i) => {
                    n += 1;
                    stack.push(i);
                }
                PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) => stack.push(i),
                PNode::Array(i, _) => stack.push(i),
                PNode::Tuple(l) => stack.extend_from_slice(self.tys.list(l).map_err(|_| SigRefusal::Unknown)?),
                PNode::Adt(k, l) => {
                    n += (self.adt_lifetimes)(k).ok_or(SigRefusal::Unknown)?;
                    stack.extend_from_slice(self.tys.list(l).map_err(|_| SigRefusal::Unknown)?);
                }
                PNode::Opaque(_, _) | PNode::Alias { .. } | PNode::Dynamic(_) => n += 1,
                PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } => return Err(SigRefusal::TypeNode),
                _ => {}
            }
        }
        Ok(n)
    }

    /// Lines up one input or the output: fills `regions[base..]`.
    fn line_up(&mut self, first: Pair, mode: Mode, regions: &mut Vec<Region, A>) -> Result<(), SigRefusal> {
        let mut work: Vec<Pair, A> = Vec::new_in(self.alloc.clone());
        let mut parts: Vec<(u32, u32), A> = Vec::new_in(self.alloc.clone());
        work.push(first);
        let mut steps = 0u32;
        while let Some(mut p) = work.pop() {
            steps += 1;
            if steps > MOST {
                return Err(SigRefusal::Bound);
            }
            let n = self.count(p.ty)?;
            if n == 0 {
                continue;
            }
            // `( T )` is `T`; `(T,)` is a tuple.
            while self.is(p.lo, p.hi, Tk::P(b'(')) && self.mate[p.lo as usize] == p.hi - 1 {
                self.pieces(p.lo + 1, p.hi - 1, &mut parts);
                let trailing = p.hi >= 2 && self.toks[p.hi as usize - 2].k == Tk::P(b',');
                if parts.len() != 1 || trailing {
                    break;
                }
                p.lo += 1;
                p.hi -= 1;
            }
            if p.whole_self || (p.hi == p.lo + 1 && self.is_word(p.lo, p.hi, "Self")) {
                for k in 0..n {
                    regions[(p.base + k) as usize] = Region::SelfTy(k);
                    self.unwritten.push(p.base + k);
                }
                continue;
            }
            // `Self::Name`: the impl's own `type Name = ..;` text (impl_life.rs), never the path
            // read as an ADT's name.
            if self.assoc_path(p, regions)? {
                continue;
            }
            let unaligned = SigRefusal::Unaligned { at: self.at(p.lo) };
            if p.lo >= p.hi {
                return Err(unaligned);
            }
            match self.node(p.ty)? {
                PNode::Ref(_, inner) => {
                    if !self.is(p.lo, p.hi, Tk::P(b'&')) {
                        return Err(unaligned);
                    }
                    let mut i = p.lo + 1;
                    let written = if self.is(i, p.hi, Tk::Life) {
                        i += 1;
                        self.named(i - 1)
                    } else {
                        None
                    };
                    let r = match written {
                        Some(r) => r,
                        None => self.elided(mode, p.lo)?,
                    };
                    if self.is_word(i, p.hi, "mut") {
                        i += 1;
                    }
                    regions[p.base as usize] = r;
                    work.push(Pair { ty: inner, lo: i, hi: p.hi, base: p.base + 1, ctx: Ctx::Ref(r), whole_self: false });
                }
                PNode::Ptr(_, inner) => {
                    if !self.is(p.lo, p.hi, Tk::P(b'*')) || p.lo + 2 > p.hi {
                        return Err(unaligned);
                    }
                    work.push(Pair { ty: inner, lo: p.lo + 2, hi: p.hi, base: p.base, ctx: Ctx::Other, whole_self: false });
                }
                PNode::Slice(inner) | PNode::Array(inner, _) => {
                    if !self.is(p.lo, p.hi, Tk::P(b'[')) || self.mate[p.lo as usize] != p.hi - 1 {
                        return Err(unaligned);
                    }
                    let (mut i, mut end) = (p.lo + 1, p.hi - 1);
                    while i < p.hi - 1 {
                        if self.toks[i as usize].k == Tk::P(b';') {
                            end = i;
                            break;
                        }
                        i = self.skip(i);
                    }
                    work.push(Pair { ty: inner, lo: p.lo + 1, hi: end, base: p.base, ctx: Ctx::Other, whole_self: false });
                }
                PNode::Tuple(l) => {
                    if !self.is(p.lo, p.hi, Tk::P(b'(')) || self.mate[p.lo as usize] != p.hi - 1 {
                        return Err(unaligned);
                    }
                    let elems = self.tys.list(l).map_err(|_| SigRefusal::Unknown)?;
                    self.pieces(p.lo + 1, p.hi - 1, &mut parts);
                    if parts.len() != elems.len() {
                        return Err(unaligned);
                    }
                    let mut off = p.base;
                    for (j, &t) in elems.iter().enumerate() {
                        let (lo, hi) = parts[j];
                        work.push(Pair { ty: t, lo, hi, base: off, ctx: Ctx::Other, whole_self: false });
                        off += self.count(t)?;
                    }
                }
                PNode::Adt(k, l) => {
                    // A plain path: `a::b::Name` or `a::b::Name<..>`, arguments on the last segment only.
                    let (name_end, args) = if self.is(p.hi - 1, p.hi, Tk::P(b'>')) {
                        let open = self.mate[p.hi as usize - 1];
                        (open, Some((open + 1, p.hi - 1)))
                    } else {
                        (p.hi, None)
                    };
                    if name_end <= p.lo || name_end > p.hi {
                        return Err(unaligned);
                    }
                    for i in p.lo..name_end {
                        if !matches!(self.toks[i as usize].k, Tk::Ident | Tk::Sep) {
                            return Err(unaligned);
                        }
                    }
                    let last = self.word(name_end - 1);
                    if !self.is(name_end - 1, p.hi, Tk::Ident) || matches!(last, "dyn" | "impl" | "fn" | "Self") {
                        return Err(unaligned);
                    }
                    match args {
                        Some((lo, hi)) => self.pieces(lo, hi, &mut parts),
                        None => parts.clear(),
                    }
                    // One position per lifetime parameter the ADT's head declares, in order.
                    let has = (self.adt_lifetimes)(k).ok_or(SigRefusal::Unknown)?;
                    // The written lifetime arguments' tokens: one per parameter, or none (each
                    // elided: the Reference, "Lifetime elision", "Each elided lifetime in the
                    // parameters becomes a distinct lifetime parameter").
                    let mut lifes: Vec<u32, A> = Vec::new_in(self.alloc.clone());
                    let mut types: Vec<(u32, u32), A> = Vec::new_in(self.alloc.clone());
                    for j in 0..parts.len() {
                        let (lo, hi) = parts[j];
                        if hi == lo + 1 && self.is(lo, hi, Tk::Life) {
                            lifes.push(lo);
                        } else {
                            // `Name = Type` is a binding, not an argument: only in bounds.
                            if self.is(lo + 1, hi, Tk::P(b'=')) {
                                return Err(unaligned);
                            }
                            types.push((lo, hi));
                        }
                    }
                    // Written lifetimes are all of the parameters, or none (E0107 otherwise).
                    if !lifes.is_empty() && lifes.len() as u32 != has {
                        return Err(unaligned);
                    }
                    for q in 0..has {
                        let written = match lifes.get(q as usize) {
                            Some(&t) => self.named(t),
                            None => None,
                        };
                        let r = match written {
                            Some(r) => r,
                            None => self.elided(mode, p.lo)?,
                        };
                        regions[(p.base + q) as usize] = r;
                    }
                    let mut off = p.base + has;
                    let elems = self.tys.list(l).map_err(|_| SigRefusal::Unknown)?;
                    if types.len() != elems.len() {
                        // Defaults or const arguments: fine only when no argument has a position.
                        if n != has {
                            return Err(unaligned);
                        }
                        continue;
                    }
                    for (j, &t) in elems.iter().enumerate() {
                        let (lo, hi) = types[j];
                        work.push(Pair { ty: t, lo, hi, base: off, ctx: Ctx::Adt(has > 0), whole_self: false });
                        off += self.count(t)?;
                    }
                }
                PNode::Dynamic(_) => {
                    if !self.is_word(p.lo, p.hi, "dyn") {
                        return Err(unaligned);
                    }
                    let mut written: Option<u32> = None;
                    let mut i = p.lo + 1;
                    while i < p.hi {
                        if self.toks[i as usize].k == Tk::Life {
                            written = Some(i);
                        }
                        i = self.skip(i);
                    }
                    let r = match written.map(|i| (i, self.named(i))) {
                        Some((_, Some(r))) => r,
                        Some((i, None)) => self.elided(mode, i)?,
                        None => match p.ctx {
                            Ctx::Ref(r) => r,
                            Ctx::Adt(false) => Region::Static,
                            _ => return Err(SigRefusal::ObjectDefault { at: self.at(p.lo) }),
                        },
                    };
                    regions[p.base as usize] = r;
                }
                PNode::Opaque(_, _) | PNode::Alias { .. } => {
                    self.hidden += 1;
                    regions[p.base as usize] = Region::Hidden(self.hidden - 1);
                }
                _ => return Err(unaligned),
            }
        }
        Ok(())
    }

    /// The declared bounds of one generics list or where clause: `'a: 'b + 'c`.
    fn bounds(&mut self, lo: u32, hi: u32, out: &mut Vec<(Region, Region), A>) {
        let mut parts: Vec<(u32, u32), A> = Vec::new_in(self.alloc.clone());
        self.pieces(lo, hi, &mut parts);
        for j in 0..parts.len() {
            let (lo, hi) = parts[j];
            if !(self.is(lo, hi, Tk::Life) && self.is(lo + 1, hi, Tk::P(b':'))) {
                continue;
            }
            let Some(a) = self.named(lo) else { continue };
            for i in lo + 2..hi {
                if self.toks[i as usize].k == Tk::Life
                    && let Some(b) = self.named(i)
                {
                    out.push((a, b));
                }
            }
        }
    }
}

/// How the receiver is written.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Receiver {
    None,
    /// `&self`, `&mut self`, `self: &Self`, `self: &mut Self`: its lifetime decides elision.
    Ref,
    /// `self`, or `self: T` for another `T`.
    Other,
}

/// Reads the lifetime part of the fn whose text is `text` and whose erased inputs and output are
/// `inputs` and `output` in `tys`. `adt_lifetimes` says how many lifetime parameters an ADT's
/// head declares. On `Ok`, `out` holds one region per origin position, in the lowering's order.
pub fn read_signature<A: Allocator + Clone>(
    text: &str,
    tys: PTys<'_>,
    inputs: &[PTy],
    output: PTy,
    adt_lifetimes: &dyn Fn(ItemKey) -> Option<u32>,
    out: &mut SigBuf<A>,
    alloc: A,
) -> Result<(), SigRefusal> {
    read_signature_in(text, None, tys, inputs, output, adt_lifetimes, out, alloc)
}

/// [`read_signature`], of a member of the impl `imp` when given (impl_life.rs
/// [`read_impl_signature`]): its lifetimes by name, its `Self::Name`, its header's bounds.
#[expect(clippy::too_many_arguments, reason = "read_signature's inputs and the impl's reading")]
fn read_signature_in<'t, A: Allocator + Clone>(
    text: &'t str,
    imp: Option<impl_life::ImplLife<'t, A>>,
    tys: PTys<'_>,
    inputs: &[PTy],
    output: PTy,
    adt_lifetimes: &dyn Fn(ItemKey) -> Option<u32>,
    out: &mut SigBuf<A>,
    alloc: A,
) -> Result<(), SigRefusal> {
    out.regions.clear();
    out.ends.clear();
    out.bounds.clear();
    out.type_outlives.clear();
    out.type_outlives_unread = false;
    let mut r = Reader {
        text,
        tys,
        adt_lifetimes,
        toks: Vec::new_in(alloc.clone()),
        mate: Vec::new_in(alloc.clone()),
        names: Vec::new_in(alloc.clone()),
        anon: 0,
        hidden: 0,
        alloc: alloc.clone(),
        imp,
        unwritten: Vec::new_in(alloc.clone()),
    };
    lex(text, &mut r.toks)?;
    r.pair_brackets()?;
    let end = r.toks.len() as u32;
    // The `fn` keyword: the first outside every bracket (attributes are bracketed).
    let mut i = 0u32;
    while i < end && !r.is_word(i, end, "fn") {
        i = r.skip(i);
    }
    let mut i = i + 2;
    if i > end {
        return Err(SigRefusal::NoFn);
    }
    let generics = if r.is(i, end, Tk::P(b'<')) {
        let close = r.mate[i as usize];
        let g = (i + 1, close);
        i = close + 1;
        Some(g)
    } else {
        None
    };
    if !r.is(i, end, Tk::P(b'(')) {
        return Err(SigRefusal::NoFn);
    }
    let params = (i + 1, r.mate[i as usize]);
    i = params.1 + 1;
    // The output's tokens run to `where` or the end.
    let mut where_at = i;
    while where_at < end && !r.is_word(where_at, end, "where") {
        where_at = r.skip(where_at);
    }
    let out_ty = if r.is(i, end, Tk::Arrow) { Some((i + 1, where_at)) } else { None };
    if let Some((lo, hi)) = generics {
        r.bounds(lo, hi, &mut out.bounds);
    }
    if where_at < end {
        r.bounds(where_at + 1, end, &mut out.bounds);
    }
    let mut parts: Vec<(u32, u32), A> = Vec::new_in(alloc.clone());
    r.pieces(params.0, params.1, &mut parts);
    if parts.len() != inputs.len() {
        return Err(SigRefusal::ParamCount { text: parts.len() as u32, head: inputs.len() as u32 });
    }
    let mut receiver = Receiver::None;
    for (j, &ty) in inputs.iter().enumerate() {
        let (lo, hi) = parts[j];
        let base = out.regions.len() as u32;
        let n = r.count(ty)?;
        out.regions.resize((base + n) as usize, Region::Hidden(u32::MAX));
        // The receiver's short forms: `[&['a] [mut]] self`, `mut self`.
        let self_at = if r.is_word(hi.saturating_sub(1), hi, "self") && hi > lo { Some(hi - 1) } else { None };
        let short = self_at.is_some_and(|s| (lo..s).all(|i| matches!(r.toks[i as usize].k, Tk::P(b'&') | Tk::Life) || r.is_word(i, hi, "mut")));
        if j == 0 && short {
            if r.is(lo, hi, Tk::P(b'&')) {
                receiver = Receiver::Ref;
                let PNode::Ref(_, inner) = r.node(ty)? else { return Err(SigRefusal::Unaligned { at: r.at(lo) }) };
                let written = if r.is(lo + 1, hi, Tk::Life) { r.named(lo + 1) } else { None };
                let region = match written {
                    Some(x) => x,
                    None => r.elided(Mode::Input, lo)?,
                };
                out.regions[base as usize] = region;
                r.line_up(Pair { ty: inner, lo, hi, base: base + 1, ctx: Ctx::Ref(region), whole_self: true }, Mode::Input, &mut out.regions)?;
            } else {
                receiver = Receiver::Other;
                r.line_up(Pair { ty, lo, hi, base, ctx: Ctx::Other, whole_self: true }, Mode::Input, &mut out.regions)?;
            }
        } else {
            // `pattern : Type`: the first top-level `:`.
            let mut colon = lo;
            while colon < hi && r.toks[colon as usize].k != Tk::P(b':') {
                colon = r.skip(colon);
            }
            if colon >= hi {
                return Err(SigRefusal::Unaligned { at: r.at(lo) });
            }
            if j == 0 && r.is_word(colon.saturating_sub(1), colon, "self") {
                let t = colon + 1;
                let by_ref = r.is(t, hi, Tk::P(b'&')) && r.is_word(hi - 1, hi, "Self") && (t + 1..hi - 1).all(|i| r.toks[i as usize].k == Tk::Life || r.is_word(i, hi, "mut"));
                receiver = if by_ref { Receiver::Ref } else { Receiver::Other };
            }
            r.line_up(Pair { ty, lo: colon + 1, hi, base, ctx: Ctx::Other, whole_self: false }, Mode::Input, &mut out.regions)?;
        }
        out.ends.push(out.regions.len() as u32);
    }
    // The Reference, "Lifetime elision": the receiver's reference first, else the one lifetime
    // used in the parameters. `Self`'s, a `Self::Name`'s and hidden positions are not lifetimes
    // the text uses; an impl's lifetime written by name (`&'a u8`, a `SelfTy`) is.
    let decided = if receiver == Receiver::Ref {
        out.regions.first().copied()
    } else {
        let mut one: Option<Region> = None;
        let mut many = false;
        for (j, &x) in out.regions.iter().enumerate() {
            if matches!(x, Region::Hidden(_)) || r.unwritten.contains(&(j as u32)) {
                continue;
            }
            match one {
                None => one = Some(x),
                Some(y) if y != x => many = true,
                Some(_) => {}
            }
        }
        if many { None } else { one }
    };
    let base = out.regions.len() as u32;
    let n = r.count(output)?;
    out.regions.resize((base + n) as usize, Region::Hidden(u32::MAX));
    match out_ty {
        Some((lo, hi)) => r.line_up(Pair { ty: output, lo, hi, base, ctx: Ctx::Other, whole_self: false }, Mode::Output(decided), &mut out.regions)?,
        None if n == 0 => {}
        None => return Err(SigRefusal::Unaligned { at: r.at(i) }),
    }
    if out.regions.iter().any(|x| *x == Region::Hidden(u32::MAX)) {
        return Err(SigRefusal::Unaligned { at: r.at(params.0) });
    }
    let wheres = if where_at < end { Some((where_at + 1, end)) } else { None };
    r.type_outlives(generics, wheres, &mut out.type_outlives, &mut out.type_outlives_unread)?;
    // The impl's: its self type's equal and `'static` positions, its declared `'a: 'b`.
    if let Some(imp) = r.imp.as_ref() {
        out.bounds.extend_from_slice(&imp.bounds);
    }
    Ok(())
}
