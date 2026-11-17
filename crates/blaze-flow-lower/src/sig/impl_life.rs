//! A member of an impl: its signature's `Self::Name` read through the impl's own `type Name =
//! ..;` text, and the impl's lifetime parameters mapped to the positions of its self type
//! (piece sig-assoc-projection).
//!
//! THE CLASS this closes: the front end resolves `Self::Name` in an impl's fn to the impl's own
//! associated type (blaze-ladder front2 types.rs, `SelfTy::Ty` with the impl's `self_assoc`:
//! the one resolution the type check's heads are built with), so the head's erased type at that
//! place is the impl's type (`Pending<T>` for `-> Self::IntoFuture`), while the text says
//! `Self::IntoFuture`. The reader lined the written PATH up as if it were the ADT's own: an
//! elided lifetime by the fn's elision rules (`ElisionUndecided` for `fn into_future(self)`, a
//! WRONG region when another input decides elision), `Unaligned` when the impl's type is a
//! reference. And a lifetime the impl declares (`impl<'a> .. for B<'a>`) was a fresh `Named`
//! region at each call, unrelated to the receiver's `'a`. Now:
//!
//! - the impl's header is read once ([`impl_life`]): its self type is lined up with the head's
//!   erased `Self` exactly as a parameter's type is, and each lifetime it holds by name is
//!   `Region::SelfTy(k)` of its first position (a name at two positions makes them equal, a
//!   `'static` there is `SelfTy(k): 'static`; the header's declared `'a: 'b` are kept over
//!   those). The Reference, "Implementations", "Generic implementations": "Generic parameters
//!   constrain an implementation if the parameter appears at least once in [..] the
//!   implemented trait, if it has one, [or] the implementing type": a lifetime in the self type
//!   is the call's own `Self`'s, which `SelfTy(k)` is at every call.
//! - `Self::Name` (and `<Self as Tr>::Name` with `Tr` written as the impl's trait is) whose
//!   erased type is not a projection is lined up against the impl's `type Name = T;` text, the
//!   same erased type: what the front end normalized it to IS that member's type, so no second
//!   normalizer is consulted (rule 30). The Reference, "Associated Items", "Associated types":
//!   an impl's `type Name = T;` "defines" the associated type for that implementation. Its
//!   lifetimes are the impl's (elision is not allowed there: E0106), so each is the mapped
//!   region; a name the self type does not hold (an impl lifetime only its trait names, or a
//!   generic associated type's own) is refused by name ([`AssocWhy::Lifetime`]).
//! - whatever cannot be read is refused BY NAME ([`SigRefusal::Assoc`] with an [`AssocWhy`]),
//!   never guessed: the call then takes the lowering's older path for an unread head.
//!
//! LEFT AS IT WAS, named: a projection the head cannot resolve (`<T as Tr>::X`, `T::X` over
//! the head's own type parameter, or `Self::X` of another trait's impl) stays a
//! `Region::Hidden` position; a call through such a head is counted in `Stats::calls_hidden`
//! and related by elision or one origin, as before. That is a superset for the OUTPUT: by the
//! rule quoted above (E0207), the lifetimes of `<T as Tr<A>>::X` are among those of `T`, `A`
//! and `'static`, all of which are inputs' positions the coarse path relates to the output. It
//! is NOT a superset for an INPUT hidden position whose impl writes `'static` (the coarse path
//! demands no `'static` of the argument): the next piece, normalizing per call.
//!
//! Every read here is part of the head's lifetime part, the `BorrowHead` row's answer; what was
//! taken of the impl's text is its own row (blaze-ladder front2 sig_impl.rs, rule 21).

use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_typeck::{ItemKey, PNode, PTy, PTys};

use super::{Ctx, Mode, Pair, Reader, Region, SigBuf, SigLife, SigRefusal, Tk, Tok, lex, read_signature_in};

/// Why a `Self::Name` of a signature, or the impl's header, was not read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AssocWhy {
    /// No impl's text was given (a free fn, a trait's fn, a caller that has none).
    NoImpl = 0,
    /// The impl's header did not lex, or its self type's text does not line up with its erased
    /// type (a type alias, a macro): its lifetimes cannot be named.
    Header = 1,
    /// The impl has no `type Name` member text.
    NoText = 2,
    /// Two `type Name` member texts (a `cfg` pair this reader does not evaluate).
    TwoTexts = 3,
    /// The member's text is not `type Name = T;` as the reader takes it.
    Form = 4,
    /// A lifetime in the member's text that the impl's self type does not hold: an impl
    /// lifetime only its trait names, or a generic associated type's own.
    Lifetime = 5,
    /// An elided lifetime in the member's text (E0106: never valid in an impl's `type`).
    Elided = 6,
    /// A generic associated type: `Self::Name<..>`, or `type Name<..>`.
    Gat = 7,
    /// `<Self as Tr>::Name` where `Tr` is not written as the impl's trait is.
    OtherTrait = 8,
    /// The member's own text names `Self::Other`: not expanded twice (no recursion).
    Nested = 9,
    /// The member's written type and the head's erased type are not the same form.
    Unaligned = 10,
}

/// The impl a fn is a member of, as its text says it.
#[derive(Clone, Copy, Debug)]
pub struct ImplText<'t> {
    /// The impl's item text; its header is read up to its first `{` outside brackets.
    pub text: &'t str,
    /// The impl's self type as the fn's head erased it (the head's `Self`).
    pub self_ty: PTy,
    /// Each associated type member: its name and its whole text (`type Name = T;`), in any
    /// order (a name is looked up; two texts of one name are refused).
    pub assoc: &'t [(&'t str, &'t str)],
}

/// What the header gave the reader of one member's signature.
pub(super) struct ImplLife<'t, A: Allocator + Clone> {
    /// Each lifetime the self type holds by name, with `SelfTy(k)` of its first position.
    pub(super) names: Vec<(&'t str, Region), A>,
    /// `(a, b)`: `a: b`, from the self type (a name at two positions, a `'static`) and the
    /// header's declared bounds between mapped lifetimes.
    pub(super) bounds: Vec<(Region, Region), A>,
    /// The tokens of the impl's trait path as written; empty for an inherent impl.
    trait_words: Vec<&'t str, A>,
    assoc: &'t [(&'t str, &'t str)],
    /// `Self::Name` is expanded (false inside a member's own text).
    expand: bool,
    /// The header was read.
    header: bool,
}

impl<'t, A: Allocator + Clone> ImplLife<'t, A> {
    pub(super) fn region_of(&self, w: &str) -> Option<Region> {
        self.names.iter().find(|(n, _)| *n == w).map(|&(_, r)| r)
    }

    /// The same names for a member's own text, which expands no `Self::Name` again.
    fn nested(&self, alloc: A) -> ImplLife<'t, A> {
        let mut names = Vec::new_in(alloc.clone());
        names.extend_from_slice(&self.names);
        ImplLife { names, bounds: Vec::new_in(alloc.clone()), trait_words: Vec::new_in(alloc), assoc: &[], expand: false, header: self.header }
    }

    /// A header that was not read: no lifetime is mapped, every `Self::Name` is refused.
    fn unread(alloc: A) -> ImplLife<'t, A> {
        ImplLife { names: Vec::new_in(alloc.clone()), bounds: Vec::new_in(alloc.clone()), trait_words: Vec::new_in(alloc), assoc: &[], expand: false, header: false }
    }
}

impl SigLife<'_> {
    /// A position whose lifetimes the text does not say (`Region::Hidden`): a call through it
    /// is not instantiated, and is counted (`Stats::calls_hidden`).
    pub fn holds_hidden(&self) -> bool {
        self.regions.iter().any(|r| matches!(r, Region::Hidden(_)))
    }
}

/// [`super::read_signature`] of a fn that is a member of the impl `imp`: `Self::Name` is read
/// through the impl's own `type Name` text, and the impl's lifetimes through its self type (the
/// module doc). A header that cannot be read refuses the signature by name only when the fn's
/// text writes a lifetime it does not declare (one of the impl's): otherwise nothing of the
/// header is needed, and the fn is read as before (its `Self::Name`, if any, refused by name).
#[expect(clippy::too_many_arguments, reason = "read_signature's inputs and the impl's text")]
pub fn read_impl_signature<'t, A: Allocator + Clone>(
    text: &'t str,
    imp: ImplText<'t>,
    tys: PTys<'_>,
    inputs: &[PTy],
    output: PTy,
    adt_lifetimes: &dyn Fn(ItemKey) -> Option<u32>,
    out: &mut SigBuf<A>,
    alloc: A,
) -> Result<(), SigRefusal> {
    let life = match impl_life(imp, tys, adt_lifetimes, alloc.clone()) {
        Ok(life) => life,
        Err(why) => {
            if writes_outer_lifetime(text, alloc.clone())? {
                return Err(SigRefusal::Assoc { at: 0, why });
            }
            ImplLife::unread(alloc.clone())
        }
    };
    read_signature_in(text, Some(life), tys, inputs, output, adt_lifetimes, out, alloc)
}

/// The byte offset where the impl's header ends (its first `{` outside brackets), as
/// [`read_impl_signature`] reads it: the extent of the text a read row of the header digests.
/// `None`: the text does not lex.
pub fn impl_header_end<A: Allocator + Clone>(text: &str, alloc: A) -> Option<usize> {
    let mut toks: Vec<Tok, A> = Vec::new_in(alloc);
    lex(text, &mut toks).ok()?;
    let cut = header_cut(&toks);
    match toks.get(cut) {
        Some(t) => Some(t.lo as usize),
        None => Some(toks.last().map_or(0, |t| t.hi as usize)),
    }
}

/// The index of the header's `{`: the first outside `(..)` and `[..]` (attributes are
/// bracketed). The lexer stops AT that `{` when the header holds a `fn` type (`impl Tr for
/// fn(u8) {`): then every token is the header's, and the cut is their count.
fn header_cut(toks: &[Tok]) -> usize {
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate() {
        match t.k {
            Tk::P(b'(') | Tk::P(b'[') => depth += 1,
            Tk::P(b')') | Tk::P(b']') => depth -= 1,
            Tk::P(b'{') if depth == 0 => return i,
            _ => {}
        }
    }
    toks.len()
}

/// Whether the fn's text writes a lifetime that is neither `'_`, `'static` nor one its own
/// generics declare: then it names one of the impl's.
fn writes_outer_lifetime<A: Allocator + Clone>(text: &str, alloc: A) -> Result<bool, SigRefusal> {
    let mut toks: Vec<Tok, A> = Vec::new_in(alloc);
    lex(text, &mut toks)?;
    let word = |t: &Tok| text.get(t.lo as usize..t.hi as usize).unwrap_or("");
    let Some(f) = toks.iter().position(|t| t.k == Tk::Ident && word(t) == "fn") else {
        return Err(SigRefusal::NoFn);
    };
    // The generics `<..>` right after the name, by depth (`->` is its own token).
    let (mut glo, mut ghi) = (0usize, 0usize);
    if toks.get(f + 2).is_some_and(|t| t.k == Tk::P(b'<')) {
        let mut depth = 0i32;
        for (i, t) in toks.iter().enumerate().skip(f + 2) {
            match t.k {
                Tk::P(b'<') => depth += 1,
                Tk::P(b'>') => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                (glo, ghi) = (f + 3, i);
                break;
            }
        }
    }
    // A declared lifetime: one that opens a generics entry (`<'a`, `, 'b`).
    let declared = |w: &str| (glo..ghi).any(|i| toks[i].k == Tk::Life && word(&toks[i]) == w && matches!(toks[i - 1].k, Tk::P(b'<') | Tk::P(b',')));
    for t in toks.iter() {
        if t.k != Tk::Life {
            continue;
        }
        let w = word(t);
        if w != "'_" && w != "'static" && !declared(w) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Reads the impl's header: its self type's lifetimes by name, its declared bounds between
/// them, its trait path's tokens (the module doc).
fn impl_life<'t, A: Allocator + Clone>(
    imp: ImplText<'t>,
    tys: PTys<'_>,
    adt_lifetimes: &dyn Fn(ItemKey) -> Option<u32>,
    alloc: A,
) -> Result<ImplLife<'t, A>, AssocWhy> {
    let mut r = Reader {
        text: imp.text,
        tys,
        adt_lifetimes,
        toks: Vec::new_in(alloc.clone()),
        mate: Vec::new_in(alloc.clone()),
        names: Vec::new_in(alloc.clone()),
        anon: 0,
        hidden: 0,
        alloc: alloc.clone(),
        imp: None,
        unwritten: Vec::new_in(alloc.clone()),
    };
    lex(imp.text, &mut r.toks).map_err(|_| AssocWhy::Header)?;
    let cut = header_cut(&r.toks);
    r.toks.truncate(cut);
    r.pair_brackets().map_err(|_| AssocWhy::Header)?;
    let end = r.toks.len() as u32;
    let mut i = 0u32;
    while i < end && !r.is_word(i, end, "impl") {
        i = r.skip(i);
    }
    if i >= end {
        return Err(AssocWhy::Header);
    }
    i += 1;
    let generics = if r.is(i, end, Tk::P(b'<')) {
        let close = r.mate[i as usize];
        if close == u32::MAX || close <= i {
            return Err(AssocWhy::Header);
        }
        let g = (i + 1, close);
        i = close + 1;
        Some(g)
    } else {
        None
    };
    // `impl const Tr for X`, `impl !Tr for X`.
    if r.is_word(i, end, "const") {
        i += 1;
    }
    if r.is(i, end, Tk::P(b'!')) {
        i += 1;
    }
    // The trait and the self type: split at the first `for` that opens no `for<..>` binder.
    let (mut for_at, mut where_at) = (None, end);
    let mut j = i;
    while j < end {
        if r.is_word(j, end, "where") {
            where_at = j;
            break;
        }
        if for_at.is_none() && r.is_word(j, end, "for") && !r.is(j + 1, end, Tk::P(b'<')) {
            for_at = Some(j);
        }
        j = r.skip(j);
    }
    let (trait_range, (lo, hi)) = match for_at {
        Some(f) => (Some((i, f)), (f + 1, where_at)),
        None => (None, (i, where_at)),
    };
    if lo >= hi {
        return Err(AssocWhy::Header);
    }
    let n = r.count(imp.self_ty).map_err(|_| AssocWhy::Header)?;
    let mut regions: Vec<Region, A> = Vec::new_in(alloc.clone());
    regions.resize(n as usize, Region::Hidden(u32::MAX));
    // An impl's `dyn Tr` written bare is `dyn Tr + 'static` (the Reference, "Default trait
    // object lifetimes": "If neither of those rules apply, then [..] `'static`"): the context
    // of a type argument of an ADT without lifetimes gives exactly that.
    r.line_up(Pair { ty: imp.self_ty, lo, hi, base: 0, ctx: Ctx::Adt(false), whole_self: false }, Mode::Input, &mut regions)
        .map_err(|_| AssocWhy::Header)?;
    let mut life = ImplLife {
        names: Vec::new_in(alloc.clone()),
        bounds: Vec::new_in(alloc.clone()),
        trait_words: Vec::new_in(alloc.clone()),
        assoc: imp.assoc,
        expand: true,
        header: true,
    };
    for k in 0..n {
        let here = Region::SelfTy(k);
        match regions[k as usize] {
            Region::Named(m) => {
                let w = r.word(r.names[m as usize]);
                match life.region_of(w) {
                    Some(first) => {
                        life.bounds.push((first, here));
                        life.bounds.push((here, first));
                    }
                    None => life.names.push((w, here)),
                }
            }
            Region::Static => life.bounds.push((here, Region::Static)),
            Region::Hidden(u32::MAX) => return Err(AssocWhy::Header),
            _ => {}
        }
    }
    // The header's declared `'a: 'b` between lifetimes the self type holds.
    let mut declared: Vec<(Region, Region), A> = Vec::new_in(alloc);
    if let Some((glo, ghi)) = generics {
        r.bounds(glo, ghi, &mut declared);
    }
    if where_at < end {
        r.bounds(where_at + 1, end, &mut declared);
    }
    for &(a, b) in declared.iter() {
        let mapped = |x: Region| match x {
            Region::Named(m) => life.region_of(r.word(r.names[m as usize])),
            Region::Static => Some(Region::Static),
            _ => None,
        };
        if let (Some(a), Some(b)) = (mapped(a), mapped(b)) {
            life.bounds.push((a, b));
        }
    }
    if let Some((tlo, thi)) = trait_range {
        for t in tlo..thi {
            life.trait_words.push(r.word(t));
        }
    }
    Ok(life)
}

/// How a written type names an associated type of `Self`.
#[derive(Clone, Copy)]
enum AssocForm {
    /// `Self::Name`: the token of `Name`.
    Plain(u32),
    /// `<Self as Tr>::Name`: the token of `Name` and the tokens of `Tr`.
    Qualified(u32, (u32, u32)),
    /// Either, with arguments on `Name` (a generic associated type).
    WithArgs,
}

impl<'t, 'w, A: Allocator + Clone> Reader<'t, 'w, A> {
    fn assoc_form(&self, lo: u32, hi: u32) -> Option<AssocForm> {
        if self.is_word(lo, hi, "Self") && self.is(lo + 1, hi, Tk::Sep) && self.is(lo + 2, hi, Tk::Ident) {
            if hi == lo + 3 {
                return Some(AssocForm::Plain(lo + 2));
            }
            if self.is(lo + 3, hi, Tk::P(b'<')) && self.mate[(lo + 3) as usize] == hi - 1 {
                return Some(AssocForm::WithArgs);
            }
            return None;
        }
        if self.is(lo, hi, Tk::P(b'<')) && self.is_word(lo + 1, hi, "Self") && self.is_word(lo + 2, hi, "as") {
            let m = self.mate[lo as usize];
            if m == u32::MAX || m <= lo + 2 || !self.is(m + 1, hi, Tk::Sep) || !self.is(m + 2, hi, Tk::Ident) {
                return None;
            }
            if hi == m + 3 {
                return Some(AssocForm::Qualified(m + 2, (lo + 3, m)));
            }
            if self.is(m + 3, hi, Tk::P(b'<')) && self.mate[(m + 3) as usize] == hi - 1 {
                return Some(AssocForm::WithArgs);
            }
        }
        None
    }

    /// Lines up `p` when its text names an associated type of `Self` whose erased type the
    /// front end resolved (not a projection or an opaque, which the `Hidden` arm takes): the
    /// impl's own `type Name` text, against the same erased type, into `regions[p.base..]`.
    /// `false`: `p` is no such text.
    pub(super) fn assoc_path(&mut self, p: Pair, regions: &mut Vec<Region, A>) -> Result<bool, SigRefusal> {
        let Some(form) = self.assoc_form(p.lo, p.hi) else { return Ok(false) };
        if matches!(self.node(p.ty)?, PNode::Opaque(_, _) | PNode::Alias { .. }) {
            return Ok(false);
        }
        let at = self.at(p.lo);
        let refuse = |why: AssocWhy| SigRefusal::Assoc { at, why };
        let (name_at, tr) = match form {
            AssocForm::Plain(n) => (n, None),
            AssocForm::Qualified(n, tr) => (n, Some(tr)),
            AssocForm::WithArgs => return Err(refuse(AssocWhy::Gat)),
        };
        let name = self.word(name_at);
        let Some(imp) = self.imp.as_ref() else { return Err(refuse(AssocWhy::NoImpl)) };
        if !imp.header {
            return Err(refuse(AssocWhy::Header));
        }
        if !imp.expand {
            return Err(refuse(AssocWhy::Nested));
        }
        if let Some((tlo, thi)) = tr {
            let same = imp.trait_words.len() == (thi - tlo) as usize && (tlo..thi).all(|t| self.word(t) == imp.trait_words[(t - tlo) as usize]);
            if !same {
                return Err(refuse(AssocWhy::OtherTrait));
            }
        }
        let mut found: Option<&'t str> = None;
        for &(n, text) in imp.assoc.iter() {
            if n == name {
                if found.is_some() {
                    return Err(refuse(AssocWhy::TwoTexts));
                }
                found = Some(text);
            }
        }
        let Some(text) = found else { return Err(refuse(AssocWhy::NoText)) };
        let nested = imp.nested(self.alloc.clone());
        let n = self.count(p.ty)?;
        let mut sub = Reader {
            text,
            tys: self.tys,
            adt_lifetimes: self.adt_lifetimes,
            toks: Vec::new_in(self.alloc.clone()),
            mate: Vec::new_in(self.alloc.clone()),
            names: Vec::new_in(self.alloc.clone()),
            anon: 0,
            hidden: 0,
            alloc: self.alloc.clone(),
            imp: Some(nested),
            unwritten: Vec::new_in(self.alloc.clone()),
        };
        lex(text, &mut sub.toks).map_err(|_| refuse(AssocWhy::Form))?;
        sub.pair_brackets().map_err(|_| refuse(AssocWhy::Form))?;
        let (lo, hi) = sub.assoc_value(name).map_err(refuse)?;
        let mut got: Vec<Region, A> = Vec::new_in(self.alloc.clone());
        got.resize(n as usize, Region::Hidden(u32::MAX));
        // No elision in an impl's `type` (E0106): an elided lifetime there is refused.
        sub.line_up(Pair { ty: p.ty, lo, hi, base: 0, ctx: p.ctx, whole_self: false }, Mode::Output(None), &mut got).map_err(|e| match e {
            SigRefusal::ElisionUndecided { .. } => refuse(AssocWhy::Elided),
            SigRefusal::Assoc { why, .. } => refuse(why),
            _ => refuse(AssocWhy::Unaligned),
        })?;
        for k in 0..n {
            let x = match got[k as usize] {
                Region::Named(_) | Region::Anon(_) => return Err(refuse(AssocWhy::Lifetime)),
                Region::Hidden(u32::MAX) => return Err(refuse(AssocWhy::Unaligned)),
                Region::Hidden(h) => Region::Hidden(self.hidden + h),
                x => x,
            };
            regions[(p.base + k) as usize] = x;
            self.unwritten.push(p.base + k);
        }
        self.hidden += sub.hidden;
        Ok(true)
    }

    /// The tokens of `T` in this member text `type name = T;` (or `.. = T where ..;`).
    fn assoc_value(&self, name: &str) -> Result<(u32, u32), AssocWhy> {
        let end = self.toks.len() as u32;
        let mut i = 0u32;
        while i < end && !self.is_word(i, end, "type") {
            i = self.skip(i);
        }
        if !self.is_word(i + 1, end, name) {
            return Err(AssocWhy::Form);
        }
        let mut j = i + 2;
        if self.is(j, end, Tk::P(b'<')) {
            return Err(AssocWhy::Gat);
        }
        if !self.is(j, end, Tk::P(b'=')) {
            return Err(AssocWhy::Form);
        }
        j += 1;
        let lo = j;
        while j < end && !self.is(j, end, Tk::P(b';')) && !self.is_word(j, end, "where") {
            j = self.skip(j);
        }
        if j == lo {
            return Err(AssocWhy::Form);
        }
        Ok((lo, j))
    }
}
