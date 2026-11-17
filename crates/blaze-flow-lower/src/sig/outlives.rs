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
