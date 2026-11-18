//! The type-outlives bounds a head declares: `T: 'a`, `T: 'static`, `where T: Trait + 'a`
//! (BR3, a call related by its callee's signature).
//!
//! The Reference, "Trait and lifetime bounds": "`T: 'a` means that all lifetime parameters of
//! `T` outlive `'a`". A call instantiates `T` with the caller's type; every origin position of
//! that type must then outlive the region `'a` stands for at the call. The front end's heads
//! keep a parameter's TRAIT bounds only (front2 types.rs `ParamBound`), so these are read from
//! the fn's own text by the same reader as the rest of its lifetime part ([`super::read_signature`]
//! calls [`Reader::type_outlives`] last, so a lifetime gets the same `Named` number as in the
//! signature's regions). A bound whose left side is not one bare name (`Self: 'a`,
//! `&'b T: 'a`, `Vec<T>: 'a`) is not resolved to a parameter: it is noted as unread
//! (`SigBuf::type_outlives_unread`), and a call whose generic arguments carry a lifetime is
//! then refused by name, never related without it.
//!
//! [`container_declares_type_outlives`] answers the coarser question for an impl's or a trait's
//! header (whose lifetimes the fn's text does not number): whether it declares ANY bound with a
//! lifetime on a type (a trait's `trait Tr: 'static` included). Every walk is bounded; nothing
//! recurses.

use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_typeck::Name;

use super::{MOST, Reader, Region, SigRefusal, Tk, Tok, lex};

/// What one generics list or where clause says, piece by piece.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Said {
    /// `name: .. 'x ..`: the token of the bare name, and the lifetime's token.
    Simple { name: u32, life: u32 },
    /// A lifetime bound on a left side that is not one bare name.
    Other,
}

/// The token after `i`, stepping over a bracketed group as one (`Reader::skip` over bare tokens).
fn step(mate: &[u32], i: u32) -> u32 {
    let m = mate.get(i as usize).copied().unwrap_or(u32::MAX);
    if m != u32::MAX && m > i { m + 1 } else { i + 1 }
}

/// Calls `said` for each type-outlives bound of the comma separated list `lo..hi` (a `<..>`
/// generics list's inside, or a where clause): a piece's left side runs to its first top-level
/// `:`; each top-level lifetime token after it (a `+ 'a`, not one inside a trait's arguments)
/// is a bound. Pieces whose left side is a lifetime (`'a: 'b`: [`Reader::bounds`]) or a `const`
/// parameter are skipped; a leading `for<..>` binder is stepped over.
fn scan(toks: &[Tok], mate: &[u32], text: &str, lo: u32, hi: u32, said: &mut dyn FnMut(Said)) -> Result<(), SigRefusal> {
    let word = |i: u32| -> &str {
        match toks.get(i as usize) {
            Some(t) => text.get(t.lo as usize..t.hi as usize).unwrap_or(""),
            None => "",
        }
    };
    let kind = |i: u32| toks.get(i as usize).map(|t| t.k);
    let mut from = lo;
    let mut steps = 0u32;
    while from < hi {
        // The piece `from..to`: up to the next top-level comma.
        let mut to = from;
        while to < hi && kind(to) != Some(Tk::P(b',')) {
            steps += 1;
            if steps > MOST {
                return Err(SigRefusal::Bound);
            }
            to = step(mate, to);
        }
        let to = to.min(hi);
        let mut start = from;
        if kind(start) == Some(Tk::Ident) && word(start) == "for" && kind(start + 1) == Some(Tk::P(b'<')) {
            start = step(mate, start + 1);
        }
        let skip = matches!(kind(start), Some(Tk::Life)) || (kind(start) == Some(Tk::Ident) && word(start) == "const");
        if !skip && start < to {
            let mut colon = start;
            while colon < to && kind(colon) != Some(Tk::P(b':')) {
                steps += 1;
                if steps > MOST {
                    return Err(SigRefusal::Bound);
                }
                colon = step(mate, colon);
            }
            let simple = colon == start + 1 && kind(start) == Some(Tk::Ident);
            let mut i = colon + 1;
            while i < to {
                steps += 1;
                if steps > MOST {
                    return Err(SigRefusal::Bound);
                }
                match kind(i) {
                    // A default (`T: 'a = U`) ends the bounds.
                    Some(Tk::P(b'=')) => break,
                    Some(Tk::Life) if simple => said(Said::Simple { name: start, life: i }),
                    Some(Tk::Life) => said(Said::Other),
                    _ => {}
                }
                i = step(mate, i);
            }
        }
        from = to + 1;
    }
    Ok(())
}

impl<'t, 'w, A: Allocator + Clone> Reader<'t, 'w, A> {
    /// The fn's type-outlives bounds from its generics list `generics` (the inside of its
    /// `<..>`) and its where clause `wheres`, into `out` as `(parameter name, region)`; a bound
    /// not on a bare name, or on `'_`, sets `unread`. Called after the signature's regions are
    /// read, so the regions are numbered as they are.
    pub(super) fn type_outlives(
        &mut self,
        generics: Option<(u32, u32)>,
        wheres: Option<(u32, u32)>,
        out: &mut Vec<(Name, Region), A>,
        unread: &mut bool,
    ) -> Result<(), SigRefusal> {
        let mut found: Vec<Said, A> = Vec::new_in(self.alloc.clone());
        for (lo, hi) in [generics, wheres].into_iter().flatten() {
            scan(&self.toks, &self.mate, self.text, lo, hi, &mut |s| found.push(s))?;
        }
        for &s in found.iter() {
            match s {
                Said::Simple { name, life } => match self.named(life) {
                    Some(r) => {
                        let n = Name::of(self.word(name));
                        if !out.iter().any(|&(m, x)| m == n && x == r) {
                            out.push((n, r));
                        }
                    }
                    None => *unread = true,
                },
                Said::Other => *unread = true,
            }
        }
        Ok(())
    }
}

/// One bound with lifetime arguments `name: .. Path<'x, ..> ..` (piece RB8): the token of the
/// bounded name (`simple`: it is one bare name), of the trait's last segment, and the inside
/// of its `<..>`; `binder` the inside of a `for<..>` over the piece or the bound, if any.
#[derive(Clone, Copy)]
struct BoundAt {
    name: u32,
    simple: bool,
    trait_: u32,
    args: (u32, u32),
    binder: Option<(u32, u32)>,
}

/// Calls `said` for each trait bound of the list `lo..hi` (as [`scan`]'s) whose trait path
/// ends in a `<..>`: a piece's bounds after its first top-level `:` split at top-level `+`; a
/// bound in `Fn(..)` form, `?Sized`, a lifetime and a path with no `<..>` are skipped.
fn scan_trait_bounds(toks: &[Tok], mate: &[u32], text: &str, lo: u32, hi: u32, said: &mut dyn FnMut(BoundAt)) -> Result<(), SigRefusal> {
    let word = |i: u32| -> &str {
        match toks.get(i as usize) {
            Some(t) => text.get(t.lo as usize..t.hi as usize).unwrap_or(""),
            None => "",
        }
    };
    let kind = |i: u32| toks.get(i as usize).map(|t| t.k);
    let is_for = |i: u32| kind(i) == Some(Tk::Ident) && word(i) == "for" && kind(i + 1) == Some(Tk::P(b'<'));
    let inside = |i: u32| (i + 2, mate.get(i as usize + 1).copied().unwrap_or(i + 1));
    let mut from = lo;
    let mut steps = 0u32;
    while from < hi {
        let mut to = from;
        while to < hi && kind(to) != Some(Tk::P(b',')) {
            steps += 1;
            if steps > MOST {
                return Err(SigRefusal::Bound);
            }
            to = step(mate, to);
        }
        let to = to.min(hi);
        let mut start = from;
        let mut binder = None;
        if is_for(start) {
            binder = Some(inside(start));
            start = step(mate, start + 1);
        }
        let skip = matches!(kind(start), Some(Tk::Life)) || (kind(start) == Some(Tk::Ident) && word(start) == "const");
        let mut colon = start;
        while !skip && colon < to && kind(colon) != Some(Tk::P(b':')) {
            steps += 1;
            if steps > MOST {
                return Err(SigRefusal::Bound);
            }
            colon = step(mate, colon);
        }
        if !skip && colon < to {
            let simple = colon == start + 1 && kind(start) == Some(Tk::Ident);
            // Each bound `b..e` up to a top-level `+`, a default `=` ending them all.
            let mut b = colon + 1;
            while b < to && kind(b) != Some(Tk::P(b'=')) {
                let mut e = b;
                while e < to && !matches!(kind(e), Some(Tk::P(b'+') | Tk::P(b'='))) {
                    steps += 1;
                    if steps > MOST {
                        return Err(SigRefusal::Bound);
                    }
                    e = step(mate, e);
                }
                let mut at = b;
                let mut own_binder = binder;
                if is_for(at) {
                    own_binder = Some(inside(at));
                    at = step(mate, at + 1);
                }
                // The path's last segment before its `<`; `(` (an `Fn(..)` bound) or `?` ends it.
                let mut last = None;
                while at < e {
                    match kind(at) {
                        Some(Tk::Ident) => last = Some(at),
                        Some(Tk::Sep) => {}
                        Some(Tk::P(b'<')) => {
                            if let Some(t) = last {
                                let close = mate.get(at as usize).copied().unwrap_or(at);
                                said(BoundAt { name: start, simple, trait_: t, args: (at + 1, close), binder: own_binder });
                            }
                            break;
                        }
                        _ => break,
                    }
                    at += 1;
                }
                b = e + 1;
            }
        }
        from = to + 1;
    }
    Ok(())
}

impl<'t, 'w, A: Allocator + Clone> Reader<'t, 'w, A> {
    /// The fn's trait bounds with lifetime arguments (piece RB8), from its generics list and
    /// where clause, into `out` with their arguments' regions in `regions`. A lifetime the
    /// bound's own `for<..>` binds is no region of the fn: such a bound is no row. Called after
    /// every other part is read, so a lifetime written only here is numbered last.
    pub(super) fn trait_bounds(
        &mut self,
        generics: Option<(u32, u32)>,
        wheres: Option<(u32, u32)>,
        out: &mut Vec<super::TextBound, A>,
        regions: &mut Vec<Region, A>,
    ) -> Result<(), SigRefusal> {
        let mut found: Vec<BoundAt, A> = Vec::new_in(self.alloc.clone());
        for (lo, hi) in [generics, wheres].into_iter().flatten() {
            scan_trait_bounds(&self.toks, &self.mate, self.text, lo, hi, &mut |b| found.push(b))?;
        }
        for &b in found.iter() {
            let lo = regions.len() as u32;
            let mut unread = !b.simple;
            let mut bound_by_binder = false;
            let mut any = false;
            let (mut i, end) = b.args;
            let mut steps = 0u32;
            while i < end {
                steps += 1;
                if steps > MOST {
                    return Err(SigRefusal::Bound);
                }
                // One argument `i..j`: a lifetime is one token alone.
                let mut j = i;
                while j < end && self.toks[j as usize].k != Tk::P(b',') {
                    j = step(&self.mate, j);
                }
                if j == i + 1 && self.toks[i as usize].k == Tk::Life {
                    any = true;
                    let w = self.word(i);
                    let bound_here = b.binder.is_some_and(|(blo, bhi)| (blo..bhi).any(|x| self.toks.get(x as usize).is_some_and(|t| t.k == Tk::Life) && self.word(x) == w));
                    if bound_here {
                        bound_by_binder = true;
                    } else {
                        match self.named(i) {
                            Some(r) => regions.push(r),
                            None => unread = true,
                        }
                    }
                }
                i = j + 1;
            }
            if !any {
                continue;
            }
            // A bound its own `for<..>` quantifies (`T: for<'de> Deserialize<'de>`) holds for
            // every region, so it fixes none of the fn's: a row with no regions. One that
            // mixes bound and free lifetimes is not taken apart: unread.
            let binder = bound_by_binder && regions.len() as u32 == lo;
            unread |= bound_by_binder && !binder;
            if bound_by_binder {
                regions.truncate(lo as usize);
            }
            let param = if b.simple { Name::of(self.word(b.name)) } else { Name::of("") };
            out.push(super::TextBound { param, trait_: Name::of(self.word(b.trait_)), lo, hi: regions.len() as u32, unread, binder });
        }
        Ok(())
    }
}

/// A trait's header (`text`: its item text): its name, and its lifetime parameters' names in
/// order into `out` (`trait Conv<'a, 'b>`: `Conv`, `['a, 'b]`; piece RB8). Not a trait: refused.
pub fn trait_header<A: Allocator + Clone>(text: &str, out: &mut Vec<Name, A>, alloc: A) -> Result<Name, SigRefusal> {
    let (head, toks, mate) = header(text, alloc)?;
    let end = toks.len() as u32;
    let word = |i: u32| -> &str {
        match toks.get(i as usize) {
            Some(t) => head.get(t.lo as usize..t.hi as usize).unwrap_or(""),
            None => "",
        }
    };
    let mut i = 0u32;
    while i < end && !(toks[i as usize].k == Tk::Ident && word(i) == "trait") {
        i = step(&mate, i);
    }
    if i + 1 >= end || toks[i as usize + 1].k != Tk::Ident {
        return Err(SigRefusal::NoFn);
    }
    let name = Name::of(word(i + 1));
    let at = i + 2;
    if at < end && toks[at as usize].k == Tk::P(b'<') {
        let close = mate[at as usize];
        let mut first = true;
        let mut j = at + 1;
        while j < close {
            let t = toks[j as usize];
            if first && t.k == Tk::Life {
                out.push(Name::of(word(j)));
            }
            first = t.k == Tk::P(b',');
            j = step(&mate, j);
        }
    }
    Ok(name)
}

/// Whether an impl's or a trait's header (`text`: its item text, the header read up to its
/// first `{`) declares a bound with a lifetime on a type: in its generics list, its where
/// clause, or (a trait) its supertraits (`trait Tr: 'static`, a bound on `Self`). A header the
/// lexer cannot read is refused (the caller then relates no call through it).
pub fn container_declares_type_outlives<A: Allocator + Clone>(text: &str, alloc: A) -> Result<bool, SigRefusal> {
    let (head, toks, mate) = header(text, alloc)?;
    let word = |i: u32| -> &str {
        match toks.get(i as usize) {
            Some(t) => head.get(t.lo as usize..t.hi as usize).unwrap_or(""),
            None => "",
        }
    };
    let end = toks.len() as u32;
    // The `impl` or `trait` keyword: the first outside every bracket (attributes are bracketed).
    let mut i = 0u32;
    while i < end && !(toks[i as usize].k == Tk::Ident && matches!(word(i), "impl" | "trait")) {
        i = step(&mate, i);
    }
    if i >= end {
        return Err(SigRefusal::NoFn);
    }
    let is_trait = word(i) == "trait";
    let mut any = false;
    let mut said = |_: Said| any = true;
    // `impl<..>`, or `trait Name<..>`.
    let mut at = if is_trait { i + 2 } else { i + 1 };
    if at < end && toks[at as usize].k == Tk::P(b'<') {
        let close = mate[at as usize];
        scan(&toks, &mate, head, at + 1, close, &mut said)?;
        at = close + 1;
    }
    // A trait's supertraits: `: A + 'static` before its where clause.
    let mut where_at = at;
    while where_at < end && !(toks[where_at as usize].k == Tk::Ident && word(where_at) == "where") {
        if is_trait && toks[where_at as usize].k == Tk::Life {
            said(Said::Other);
        }
        where_at = step(&mate, where_at);
    }
    if where_at < end {
        scan(&toks, &mate, head, where_at + 1, end, &mut said)?;
    }
    Ok(any)
}

/// Whether a trait's header (`text`: its item text) declares a lifetime parameter
/// (`trait Visitor<'de>`): any lifetime in its generics list. Such a trait's associated types
/// may hold that lifetime, which no type argument of a projection onto it carries (the
/// Reference, "Implementations", "Generic implementations": an impl's lifetime is constrained
/// by appearing in the implemented trait as well as in the self type). Not a trait: refused.
pub fn trait_declares_lifetime<A: Allocator + Clone>(text: &str, alloc: A) -> Result<bool, SigRefusal> {
    let (head, toks, mate) = header(text, alloc)?;
    let end = toks.len() as u32;
    let mut i = 0u32;
    while i < end && !(toks[i as usize].k == Tk::Ident && head.get(toks[i as usize].lo as usize..toks[i as usize].hi as usize) == Some("trait")) {
        i = step(&mate, i);
    }
    let at = i + 2;
    if at > end {
        return Err(SigRefusal::NoFn);
    }
    if at == end || toks[at as usize].k != Tk::P(b'<') {
        return Ok(false);
    }
    let close = mate[at as usize];
    Ok(toks[at as usize..close as usize].iter().any(|t| t.k == Tk::Life))
}

/// Whether an associated type's declaration (`text`: its member text, `type Name<..> ..;`) has
/// generics of its own (a generic associated type): its lifetime parameters are erased from a
/// projection's arguments, so a projection onto it holds lifetimes no argument carries.
pub fn assoc_declares_generics<A: Allocator + Clone>(text: &str, alloc: A) -> Result<bool, SigRefusal> {
    let mut toks: Vec<Tok, A> = Vec::new_in(alloc);
    lex(text, &mut toks)?;
    let word = |t: &Tok| text.get(t.lo as usize..t.hi as usize).unwrap_or("");
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate() {
        match t.k {
            Tk::P(b'(') | Tk::P(b'[') => depth += 1,
            Tk::P(b')') | Tk::P(b']') => depth -= 1,
            Tk::Ident if depth == 0 && word(t) == "type" => {
                return match (toks.get(i + 1), toks.get(i + 2)) {
                    (Some(n), next) if n.k == Tk::Ident => Ok(next.is_some_and(|x| x.k == Tk::P(b'<'))),
                    _ => Err(SigRefusal::NoFn),
                };
            }
            _ => {}
        }
    }
    Err(SigRefusal::NoFn)
}

/// A container's header: the text to its first `{` outside `(..)` and `[..]` as the lexer
/// reads it (a `{` inside a string of an attribute, `#[rustc_on_unimplemented(label =
/// "`{Self}` is not an iterator")]`, is no cut), its tokens, and its brackets paired as
/// `Reader::pair_brackets` pairs them.
fn header<A: Allocator + Clone>(text: &str, alloc: A) -> Result<(&str, Vec<Tok, A>, Vec<u32, A>), SigRefusal> {
    let mut toks: Vec<Tok, A> = Vec::new_in(alloc.clone());
    lex(text, &mut toks)?;
    // The cut as `impl_header_end` makes it: the header's `{`, or (the lexer stopped at a
    // `fn` type's `{`) the end of the last token.
    let at = super::impl_life::header_cut(&toks);
    let cut = match toks.get(at) {
        Some(t) => t.lo as usize,
        None => toks.last().map_or(0, |t| t.hi as usize),
    };
    toks.truncate(at);
    let head = text.get(..cut).unwrap_or(text);
    // Brackets paired as `Reader::pair_brackets` does, over the header alone.
    let mut mate: Vec<u32, A> = Vec::new_in(alloc.clone());
    mate.resize(toks.len(), u32::MAX);
    let mut open: Vec<u32, A> = Vec::new_in(alloc);
    for i in 0..toks.len() {
        let Tk::P(c) = toks[i].k else { continue };
        let want = match c {
            b'(' | b'[' | b'<' => {
                open.push(i as u32);
                continue;
            }
            b')' => b'(',
            b']' => b'[',
            b'>' => b'<',
            _ => continue,
        };
        match open.pop() {
            Some(o) if toks[o as usize].k == Tk::P(want) => {
                mate[o as usize] = i as u32;
                mate[i] = o;
            }
            _ => return Err(SigRefusal::Token { at: toks[i].lo }),
        }
    }
    if let Some(o) = open.pop() {
        return Err(SigRefusal::Token { at: toks[o as usize].lo });
    }
    Ok((head, toks, mate))
}
