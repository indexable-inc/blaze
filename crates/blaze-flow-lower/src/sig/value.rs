//! The lifetime part of a CALLABLE VALUE's signature: a parameter of the body whose written type
//! is a fn pointer (`fn(&'static T)`), a trait object or `impl` of the `Fn` family
//! (`&dyn Fn(&T) -> &U`, `impl FnMut(&T)`), or a generic parameter bounded by one (`F: Fn(&T)`,
//! in the generics or the where clause). Question T3 of flow.md: the erased types say nothing
//! of a pointer's lifetimes, so `fn(&'static u8)` and `fn(&u8)` are one type there.
//!
//! A STAND-IN like [`super::read_signature`]: it reads the body's own text, lines the written
//! argument types up with the erased types of one call's arguments, refuses by name what it
//! cannot line up and never guesses a region.
//!
//! The rules: the Reference, "Higher-ranked trait bounds": "Trait bounds may be higher ranked
//! over lifetimes. These bounds specify a bound that is true for all lifetimes"; "Lifetime
//! elision": the elision rules hold "in function item, function pointer, and closure trait
//! signatures", so `fn(&u8) -> &u8` is `for<'a> fn(&'a u8) -> &'a u8`. Every region this reader
//! gives but `'static` is therefore bound by the callable's own binder (written `for<'a>` or
//! elided), and each call instantiates it afresh. A lifetime the callable takes from OUTSIDE
//! its binder (the enclosing fn's `'a`, `Self`'s, a hidden one) is fixed across calls: it is
//! refused ([`CallableRefusal::OuterLifetime`]) until the heads carry it.

use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_typeck::{ItemKey, PTy, PTys};

use super::{Ctx, Mode, Pair, Reader, Region, SigBuf, SigRefusal, Tk, lex};

/// Why a callable parameter's signature was not read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallableRefusal {
    /// The reader of signatures refused the text or a written type.
    Sig(SigRefusal),
    /// The fn's text has no parameter of that index.
    NoParam { param: u32 },
    /// The parameter's written type is not a fn pointer or one `Fn`-family bound (an alias, a
    /// `Box<dyn Fn(..)>`, two `Fn` bounds, a generic whose bound is not in this text).
    NotCallable { at: u32 },
    /// The callable's written inputs are not as many as the call's arguments.
    ArgCount { text: u32, call: u32 },
    /// A lifetime not bound by the callable's own binder (the enclosing fn's, `Self`'s, a
    /// hidden one): the same at every call, which a per-call instance cannot say.
    OuterLifetime { at: u32 },
    /// A walk passed its bound.
    Bound,
}

impl From<SigRefusal> for CallableRefusal {
    fn from(r: SigRefusal) -> CallableRefusal {
        CallableRefusal::Sig(r)
    }
}

/// One `Fn`-family term or fn pointer type found in the text: the tokens of its `for<..>`
/// binder (empty when none), of its inputs, and of its output when one is written.
#[derive(Clone, Copy)]
struct Term {
    binder: (u32, u32),
    inputs: (u32, u32),
    output: Option<(u32, u32)>,
}

/// The most peelings, bounds and terms one parameter's type is walked through.
const MOST_STEPS: u32 = 256;

impl<'t, 'w, A: Allocator + Clone> Reader<'t, 'w, A> {
    /// `[for<..>] [unsafe] [extern ["abi"]] fn ( .. ) [-> T]` or `[for<..>] path::Fn* ( .. )
    /// [-> T]` over exactly the tokens `lo..hi`; `None` when they are anything else.
    fn callable_term(&self, lo: u32, hi: u32) -> Option<Term> {
        let mut i = lo;
        let mut binder = (lo, lo);
        if self.is_word(i, hi, "for") && self.is(i + 1, hi, Tk::P(b'<')) {
            let close = self.mate[i as usize + 1];
            if close == u32::MAX || close >= hi {
                return None;
            }
            binder = (i + 2, close);
            i = close + 1;
        }
        while self.is_word(i, hi, "unsafe") {
            i += 1;
        }
        if self.is_word(i, hi, "extern") {
            i += 1;
            if self.is(i, hi, Tk::Lit) {
                i += 1;
            }
        }
        // A path of names; its last is the fn keyword or an `Fn`-family trait.
        let mut last: Option<u32> = None;
        while i < hi && matches!(self.toks[i as usize].k, Tk::Ident | Tk::Sep) {
            if self.toks[i as usize].k == Tk::Ident {
                last = Some(i);
            }
            i += 1;
        }
        let last = last?;
        if last + 1 != i || !matches!(self.word(last), "fn" | "Fn" | "FnMut" | "FnOnce") {
            return None;
        }
        if !self.is(i, hi, Tk::P(b'(')) {
            return None;
        }
        let close = self.mate[i as usize];
        if close == u32::MAX || close >= hi {
            return None;
        }
        let inputs = (i + 1, close);
        if close + 1 == hi {
            return Some(Term { binder, inputs, output: None });
        }
        if self.is(close + 1, hi, Tk::Arrow) && close + 2 < hi {
            return Some(Term { binder, inputs, output: Some((close + 2, hi)) });
        }
        None
    }

    /// The `Fn`-family terms among the `+`-separated bounds in `lo..hi`, added to `out`.
    fn callable_terms(&self, lo: u32, hi: u32, out: &mut Vec<Term, A>) {
        let (mut i, mut from) = (lo, lo);
        while i <= hi {
            let at_end = i >= hi;
            if at_end || self.toks[i as usize].k == Tk::P(b'+') {
                if from < i
                    && let Some(t) = self.callable_term(from, i)
                {
                    out.push(t);
                }
                from = i + 1;
                if at_end {
                    break;
                }
                i += 1;
            } else {
                i = self.skip(i);
            }
        }
    }

    /// The bounds written for the generic parameter `name` in the comma-separated pieces of
    /// `lo..hi` (a generics list or a where clause): `name : bounds`.
    fn bounds_of(&self, name: &str, lo: u32, hi: u32, out: &mut Vec<Term, A>) {
        let mut parts: Vec<(u32, u32), A> = Vec::new_in(self.alloc.clone());
        self.pieces(lo, hi, &mut parts);
        for &(plo, phi) in parts.iter() {
            if self.is_word(plo, phi, name) && self.is(plo + 1, phi, Tk::P(b':')) {
                self.callable_terms(plo + 2, phi, out);
            }
        }
    }
}

/// Reads the lifetime part of the callable type written for parameter `param` of the fn whose
/// text is `text`, for one call through it whose arguments have the erased types `inputs` and
/// whose result has `output` (all in `tys`). On `Ok`, `out` holds one region per origin position
/// of each argument and then of the result, in the lowering's order: `Static`, or a region of
/// the callable's own binder (`Named` for a written `for<'a>` lifetime, `Anon` for an elided
/// one), which the caller instantiates afresh at each call. `out.bounds` is empty: a fn pointer
/// or an `Fn` bound declares none, and the implied ones the lowering derives from the types.
pub fn read_callable_param<A: Allocator + Clone>(
    text: &str,
    tys: PTys<'_>,
    param: u32,
    inputs: &[PTy],
    output: PTy,
    adt_lifetimes: &dyn Fn(ItemKey) -> Option<bool>,
    out: &mut SigBuf<A>,
    alloc: A,
) -> Result<(), CallableRefusal> {
    out.regions.clear();
    out.ends.clear();
    out.bounds.clear();
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
    };
    lex(text, &mut r.toks)?;
    r.pair_brackets()?;
    let end = r.toks.len() as u32;
    // The head, as `read_signature` walks it: `fn name [<generics>] ( params ) [-> T] [where ..]`.
    let mut i = 0u32;
    while i < end && !r.is_word(i, end, "fn") {
        i = r.skip(i);
    }
    let mut i = i + 2;
    if i > end {
        return Err(SigRefusal::NoFn.into());
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
        return Err(SigRefusal::NoFn.into());
    }
    let params = (i + 1, r.mate[i as usize]);
    i = params.1 + 1;
    // The where clause: from `where` to the body's brace or the `;`.
    let mut where_at = i;
    while where_at < end && !r.is_word(where_at, end, "where") {
        where_at = r.skip(where_at);
    }
    let mut where_end = where_at;
    while where_end < end && !matches!(r.toks[where_end as usize].k, Tk::P(b'{') | Tk::P(b';')) {
        where_end = r.skip(where_end);
    }
    let mut parts: Vec<(u32, u32), A> = Vec::new_in(alloc.clone());
    r.pieces(params.0, params.1, &mut parts);
    let Some(&(plo, phi)) = parts.get(param as usize) else {
        return Err(CallableRefusal::NoParam { param });
    };
    // `pattern : Type`: the first top-level `:`.
    let mut colon = plo;
    while colon < phi && r.toks[colon as usize].k != Tk::P(b':') {
        colon = r.skip(colon);
    }
    if colon >= phi {
        return Err(CallableRefusal::NotCallable { at: r.at(plo) });
    }
    let (mut lo, mut hi) = (colon + 1, phi);
    let not_callable = CallableRefusal::NotCallable { at: r.at(lo) };
    // Peel what stands around the callable: references, `dyn`, `impl`, a whole paren group.
    let mut steps = 0u32;
    loop {
        steps += 1;
        if steps > MOST_STEPS {
            return Err(CallableRefusal::Bound);
        }
        if lo >= hi {
            return Err(not_callable);
        }
        if r.is(lo, hi, Tk::P(b'&')) {
            lo += 1;
            if r.is(lo, hi, Tk::Life) {
                lo += 1;
            }
            if r.is_word(lo, hi, "mut") {
                lo += 1;
            }
        } else if r.is(lo, hi, Tk::P(b'(')) && r.mate[lo as usize] == hi - 1 {
            lo += 1;
            hi -= 1;
        } else if r.is_word(lo, hi, "dyn") || r.is_word(lo, hi, "impl") {
            lo += 1;
        } else {
            break;
        }
    }
    let mut terms: Vec<Term, A> = Vec::new_in(alloc.clone());
    if hi == lo + 1 && r.is(lo, hi, Tk::Ident) {
        // A generic parameter's name: its bounds, in the generics and in the where clause.
        let name = r.word(lo);
        if let Some((glo, ghi)) = generics {
            r.bounds_of(name, glo, ghi, &mut terms);
        }
        if where_at < end {
            r.bounds_of(name, where_at + 1, where_end, &mut terms);
        }
    } else {
        r.callable_terms(lo, hi, &mut terms);
    }
    // Exactly one: two `Fn` bounds would be two signatures for one call.
    if terms.len() != 1 {
        return Err(not_callable);
    }
    let term = terms[0];
    r.pieces(term.inputs.0, term.inputs.1, &mut parts);
    if parts.len() != inputs.len() {
        return Err(CallableRefusal::ArgCount { text: parts.len() as u32, call: inputs.len() as u32 });
    }
    for (j, &ty) in inputs.iter().enumerate() {
        let (mut alo, ahi) = parts[j];
        // A fn pointer may name its parameters: `fn(x: &T)`.
        if r.is(alo, ahi, Tk::Ident) && r.is(alo + 1, ahi, Tk::P(b':')) {
            alo += 2;
        }
        let base = out.regions.len() as u32;
        let n = r.count(ty)?;
        out.regions.resize((base + n) as usize, Region::Hidden(u32::MAX));
        r.line_up(Pair { ty, lo: alo, hi: ahi, base, ctx: Ctx::Other, whole_self: false }, Mode::Input, &mut out.regions)?;
        out.ends.push(out.regions.len() as u32);
    }
    // The Reference, "Lifetime elision": "If there is exactly one lifetime used in the
    // parameters (elided or not), that lifetime is assigned to all elided output lifetimes."
    let mut one: Option<Region> = None;
    let mut many = false;
    for &x in out.regions.iter() {
        match one {
            None => one = Some(x),
            Some(y) if y != x => many = true,
            Some(_) => {}
        }
    }
    let decided = if many { None } else { one };
    let base = out.regions.len() as u32;
    let n = r.count(output)?;
    out.regions.resize((base + n) as usize, Region::Hidden(u32::MAX));
    match term.output {
        Some((olo, ohi)) => r.line_up(Pair { ty: output, lo: olo, hi: ohi, base, ctx: Ctx::Other, whole_self: false }, Mode::Output(decided), &mut out.regions)?,
        None if n == 0 => {}
        None => return Err(SigRefusal::Unaligned { at: r.at(term.inputs.0) }.into()),
    }
    // Only `'static` and the callable's own binder: a written name must be in its `for<..>`.
    for &x in out.regions.iter() {
        match x {
            Region::Static | Region::Anon(_) => {}
            Region::Named(k) => {
                let first = r.names[k as usize];
                let w = r.word(first);
                let mut bound = false;
                for b in term.binder.0..term.binder.1 {
                    bound |= r.toks[b as usize].k == Tk::Life && r.word(b) == w;
                }
                if !bound {
                    return Err(CallableRefusal::OuterLifetime { at: r.at(first) });
                }
            }
            Region::SelfTy(_) | Region::Hidden(_) => {
                return Err(CallableRefusal::OuterLifetime { at: r.at(term.inputs.0) });
            }
        }
    }
    Ok(())
}
