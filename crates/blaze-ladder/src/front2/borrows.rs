//! The move and borrow check of one of the program's own bodies, for `blaze check`'s BORROWS
//! stage (docs/v2/plan/design/flow.md): the body type checked by [`ProjectWorld::check`]'s
//! path, its `Checked` record lowered to the flow form (crates/blaze-flow-lower), then the
//! move check and the two loans tiers (crates/blaze-flow `check`: tier two only for a body
//! tier one cannot accept). One [`BorrowVerdict`] per body: its findings, each with a code and
//! an item-relative range (rule 71: relative to the start of the text the body was lowered
//! from, as [`super::project::BodyDiagnostic`]'s are), or the one reason it was NOT checked
//! (never a clean verdict for a body that was not lowered and checked to its end).
//!
//! The facts the type checker's record does not hold (which ADTs are `Copy`, which implement
//! `Drop`, which declare lifetimes, each callee's signature and its lifetime part) are gathered
//! here, ONE home: [`gather_facts`], which the census (tests/flow_census.rs) calls too.
//!
//! The verdict is TRUSTED NOWHERE YET: on bstd, census 3 (flow.md) found 536 findings in 206
//! of 1,756 lowered bodies, all false. The command files each finding as a NOTE and keeps
//! saying "NOT checked: borrows" until that census reads 0 false findings (blaze-cli
//! body_types.rs `BORROWS_ARE_ERRORS`).

use blaze_flow::loans::{Access, conflicts, for_each_access};
use blaze_flow::{BorrowFinding, BorrowKind, Flow, Loan, MoveFindingKind, MoveOut, Point, StmtKind, Value};
use blaze_flow_lower::{AdtFacts, CalleeFacts, Facts, Refusal, SigBuf, SigLife, SigRefusal, lower, read_signature};
use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;
use core::alloc::Allocator;

use super::body::Body;
use super::check::{Records, answer_impls, eval_len};
use super::heads::{HeadError, Want, member_fn_want};
use super::index::Index;
use super::keys::{Keys, Origin, checker_of};
use super::project::{BodyRef, BodyVerdict, MOST_LENS, ProjectWorld};
use super::project_records::BodySource;
use super::source::NamesSource;
use super::types::Why;

/// One finding of the move or borrow check: its code (rustc's error code where one names the
/// same rule, else a word), what it says, and its item-relative range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BorrowDiagnostic {
    pub start: u32,
    pub end: u32,
    pub code: &'static str,
    pub text: &'static str,
}

/// Why a body's type check gave no record to lower: the borrow check runs over a body whose
/// types were checked clean (the Reference's rules for moves and borrows are stated over typed
/// places), so such a body is NOT checked for borrows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Untyped {
    /// The body has type errors.
    TypeErrors,
    /// Its type check stopped (a refusal, a gap): `BodyVerdict::refused` or `gaps` says why.
    TypesNotChecked,
}

/// Why a body was not checked for moves and borrows: each a refusal, never a pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BorrowRefused {
    /// Its types were not checked clean.
    Untyped(Untyped),
    /// A fact the lowering needs about an ADT is not known: whether it is `Copy` (an unknown
    /// taken as not `Copy` makes false move findings) or implements `Drop` (an unknown taken
    /// as no `Drop` would accept a move out of such a type's field): the ADT's checker key and
    /// which trait.
    FactUnknown { adt: tc::ItemKey, fact: &'static str },
    /// The body's types over the store's table could not be viewed.
    TypeTable(tc::CheckError),
    /// The lowering refused a form, by name, at an item-relative range.
    Lower(Refusal),
    /// A check went past its stated bound (rule 18): a bug of the checker, named.
    Bound(blaze_flow::Bound),
}

impl BorrowRefused {
    /// The refusal's class, for a table or a note.
    pub fn text(&self) -> &'static str {
        match self {
            BorrowRefused::Untyped(Untyped::TypeErrors) => "the body has type errors",
            BorrowRefused::Untyped(Untyped::TypesNotChecked) => "the body's types were not checked",
            BorrowRefused::FactUnknown { .. } => "a trait fact of an ADT is not known",
            BorrowRefused::TypeTable(_) => "the body's type table could not be read",
            BorrowRefused::Lower(_) => "the flow lowering refused a form",
            BorrowRefused::Bound(_) => "the check went past its bound",
        }
    }

    /// The item-relative range the refusal is about: the refused form's, else none.
    pub fn range(&self) -> Option<(u32, u32)> {
        match self {
            BorrowRefused::Lower(r) => Some((r.range.start, r.range.end)),
            _ => None,
        }
    }
}

/// What one body's move and borrow check gave.
#[derive(Debug)]
pub struct BorrowVerdict<'a> {
    /// The findings, sorted by range then code, each once.
    pub findings: AVec<'a, BorrowDiagnostic>,
    /// The one reason the body was not checked; `None` only for a body lowered and checked to
    /// its end (its findings then whole).
    pub refused: Option<BorrowRefused>,
}

impl<'a> BorrowVerdict<'a> {
    fn new_in(alloc: Alloc<'a>) -> BorrowVerdict<'a> {
        BorrowVerdict { findings: AVec::new_in(alloc), refused: None }
    }

    /// Checked to its end with no finding.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty() && self.refused.is_none()
    }
}

/// The lang items [`gather_facts`] asks impls of, as checker keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BorrowLangs {
    pub copy: Option<tc::ItemKey>,
    pub drop: Option<tc::ItemKey>,
    pub owned_box: Option<tc::ItemKey>,
}

impl BorrowLangs {
    pub fn of(index: &Index<'_>) -> BorrowLangs {
        let lang = |n: &str| index.lang(n).map(checker_of);
        BorrowLangs { copy: lang("copy"), drop: lang("drop"), owned_box: lang("owned_box") }
    }
}

/// The facts of one body, gathered before it is lowered (blaze-flow-lower's [`Facts`]): every
/// table sorted by checker key, looked up by binary search.
pub struct BorrowFacts<'a> {
    adts: AVec<'a, (u128, AdtFacts)>,
    /// Per (ADT, variant): where its field names start in `fields`, and how many.
    variants: AVec<'a, (u128, u32, u32, u32)>,
    fields: AVec<'a, tc::Name>,
    callees: AVec<'a, (u128, CalleeFacts)>,
    /// The lifetime part of each fn whose text `read_signature` could line up with its head
    /// (the stand-in for question H1: read from the fn's own text, never guessed).
    sigs: AVec<'a, (u128, SigBuf<Alloc<'a>>)>,
    /// Each fn whose signature text was not read, and why.
    sig_refused: AVec<'a, (u128, SigRefusal)>,
    /// The body's owner and its callees, sorted: the fns whose heads were asked.
    items: AVec<'a, u128>,
    /// Each ADT whose `Copy` or `Drop` answer was not known (the census takes it as `false`;
    /// the command refuses the body: [`BorrowRefused::FactUnknown`]).
    unknown: AVec<'a, (u128, &'static str)>,
}

impl<'a> BorrowFacts<'a> {
    fn new_in(alloc: Alloc<'a>) -> BorrowFacts<'a> {
        BorrowFacts {
            adts: AVec::new_in(alloc),
            variants: AVec::new_in(alloc),
            fields: AVec::new_in(alloc),
            callees: AVec::new_in(alloc),
            sigs: AVec::new_in(alloc),
            sig_refused: AVec::new_in(alloc),
            items: AVec::new_in(alloc),
            unknown: AVec::new_in(alloc),
        }
    }

    /// The fns whose heads were asked: the body's owner and its callees, by checker key.
    pub fn items(&self) -> &[u128] {
        &self.items
    }

    /// Each fn whose signature text was not read, and why.
    pub fn sig_refused(&self) -> &[(u128, SigRefusal)] {
        &self.sig_refused
    }

    /// Each ADT whose `Copy` or `Drop` answer was not known, and which.
    pub fn unknown(&self) -> &[(u128, &'static str)] {
        &self.unknown
    }

    fn variant(&self, adt: tc::ItemKey, variant: u32) -> Option<(u32, u32)> {
        let at = self.variants.binary_search_by_key(&(adt.0, variant), |&(a, v, _, _)| (a, v)).ok()?;
        self.variants.get(at).map(|&(_, _, first, count)| (first, count))
    }
}

impl Facts for BorrowFacts<'_> {
    fn sig(&self, item: tc::ItemKey) -> Option<SigLife<'_>> {
        let at = self.sigs.binary_search_by_key(&item.0, |(k, _)| *k).ok()?;
        self.sigs.get(at).map(|(_, s)| s.life())
    }
    fn adt(&self, adt: tc::ItemKey) -> Option<AdtFacts> {
        let at = self.adts.binary_search_by_key(&adt.0, |&(k, _)| k).ok()?;
        self.adts.get(at).map(|&(_, f)| f)
    }
    fn callee(&self, item: tc::ItemKey) -> Option<CalleeFacts> {
        let at = self.callees.binary_search_by_key(&item.0, |&(k, _)| k).ok()?;
        self.callees.get(at).map(|&(_, f)| f)
    }
    fn field(&self, adt: tc::ItemKey, variant: u32, name: tc::Name) -> Option<u32> {
        let (first, count) = self.variant(adt, variant)?;
        let names = self.fields.get(first as usize..(first + count) as usize)?;
        names.iter().position(|n| *n == name).map(|i| i as u32)
    }
    fn field_count(&self, adt: tc::ItemKey, variant: u32) -> Option<u32> {
        self.variant(adt, variant).map(|(_, count)| count)
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

/// Whether a head's text writes a lifetime before its body: a `'` followed by a letter or `_`
/// before the first `{` (a stand-in for the heads' lifetime part; `'_` counts: it is written).
fn writes_lifetime(text: &str) -> bool {
    let head = text.split('{').next().unwrap_or(text);
    let b = head.as_bytes();
    (0..b.len()).any(|i| b[i] == b'\'' && b.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_') && b.get(i + 2) != Some(&b'\''))
}

/// Whether an ADT's head declares lifetimes: a `'` in its head's text before its body.
fn adt_head_lifetimes<'n, S: NamesSource<'n>>(keys: &Keys, names: S, adt: tc::ItemKey) -> bool {
    keys.library_in(names, adt).and_then(|lib| names.text_of(lib)).is_some_and(|t| t.text.split(['{', '(', ';']).next().unwrap_or("").contains('\''))
}

/// Asks the store for one head, evaluating the array lengths it names, at most [`MOST_LENS`];
/// `false` when it cannot be made (the fact then stays unknown, and the lowering says so by
/// name where it matters).
fn ensure_one<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, w: Want) -> bool {
    let mut lens = 0;
    loop {
        match r.store.ensure(r.names, r.keys, w) {
            Ok(()) => return true,
            Err(HeadError::Item(_, Why::NeedConst(c, _))) if lens < MOST_LENS => {
                lens += 1;
                if eval_len(r, c).is_err() {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
}

/// A container's head (an impl's, or a trait's with its members) made: `false` when it cannot
/// be.
fn container_head<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, lib: blaze_id::ItemKey, is_trait: bool) -> bool {
    let wants: &[Want] = if is_trait { &[Want::Head(lib), Want::Members(lib)] } else { &[Want::Impl(lib)] };
    wants.iter().all(|&w| ensure_one(r, w))
}

/// Whether `trait_` has an impl for the ADT `adt`: `None` when the question could not be
/// answered (no such lang item, or the selection could not be made).
fn has_impl<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, trait_: Option<tc::ItemKey>, adt: tc::ItemKey) -> Option<bool> {
    answer_impls(r, trait_?, tc::Shape::Adt(adt)).map(|v| !v.is_empty())
}

/// The facts of the body of `owner` checked as `c`, gathered before it is lowered: per ADT its
/// record type names (asked of the store when not made yet), its fields, whether it is `Copy`
/// and implements `Drop` (asked of the selection), whether it declares lifetimes; per callee
/// (and the owner) its head made, whether it writes lifetimes and takes `&self`, and its
/// signature's lifetime part read from its text. THE ONE HOME of this gathering: the command
/// ([`ProjectWorld::check_borrows`]) and the census (tests/flow_census.rs) both call it.
pub fn gather_facts<'n, 'a, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, owner: tc::ItemKey, c: &tc::Checked<'_>, langs: BorrowLangs, alloc: Alloc<'a>) -> BorrowFacts<'a> {
    let mut g = BorrowFacts::new_in(alloc);
    let mut adts: AVec<'a, u128> = AVec::new_in(alloc);
    for n in c.types.view().nodes() {
        if let tc::PNode::Adt(k, _) = n {
            adts.push(k.0);
        }
    }
    adts.sort_unstable();
    adts.dedup();
    for &k in adts.iter() {
        let key = tc::ItemKey(k);
        // The store holds only the ADTs something asked for: ask. One that cannot be made stays
        // unknown here, and the lowering refuses its body by name (`UnknownAdt`).
        if !r.store.adts.contains_key(&key)
            && let Some(lib) = r.keys.library_in(r.names, key)
        {
            ensure_one(r, Want::Adt(lib));
        }
        let Some(row) = r.store.adts.get(&key) else { continue };
        let kind = row.kind;
        for (vi, v) in row.variants.iter().enumerate() {
            let first = g.fields.len() as u32;
            for f in row.fields.get(v.fields.range()).unwrap_or(&[]) {
                g.fields.push(f.name);
            }
            g.variants.push((k, vi as u32, first, g.fields.len() as u32 - first));
        }
        let copy = has_impl(r, langs.copy, key);
        let drop_impl = has_impl(r, langs.drop, key);
        if copy.is_none() {
            g.unknown.push((k, "Copy"));
        }
        if drop_impl.is_none() {
            g.unknown.push((k, "Drop"));
        }
        let copy = copy.unwrap_or(false);
        g.adts.push((k, AdtFacts {
            copy,
            drop_impl: drop_impl.unwrap_or(false),
            needs_drop: !copy,
            lifetimes: adt_head_lifetimes(&*r.keys, r.names, key),
            union: kind == tc::AdtKind::Union,
            is_enum: kind == tc::AdtKind::Enum,
            is_box: Some(key) == langs.owned_box,
        }));
    }
    let mut items: AVec<'a, u128> = AVec::new_in(alloc);
    for k in c.callees.column::<1>().iter() {
        items.push(k.0);
    }
    items.push(owner.0);
    items.sort_unstable();
    items.dedup();
    for &k in items.iter() {
        // A callee's head is made when something asks: ask, so its signature is read and not
        // taken as unknown (a call of unknown signature is related coarsely, and cannot say
        // that the callee demands `'static`). A member is made with its impl or its trait.
        let key = tc::ItemKey(k);
        if !r.store.fns.contains_key(&key) {
            match r.keys.origin(key).cloned() {
                Some(Origin::Item(lib)) => {
                    ensure_one(r, Want::Fn(lib));
                }
                Some(Origin::Member { container, .. }) => {
                    if let Some(lib) = r.keys.library_in(r.names, container)
                        && !container_head(r, lib, false)
                    {
                        container_head(r, lib, true);
                    }
                    // An impl's fn's signature is its own want (heads.rs `Want::MemberFn`).
                    if let Some(w) = member_fn_want(r.names, &*r.keys, key) {
                        ensure_one(r, w);
                    }
                }
                None => {}
            }
        }
        let Some(f) = r.store.fns.get(&key) else { continue };
        let tys = r.store.tys.view();
        let self_ref = f.self_param && f.inputs.first().is_some_and(|&t| matches!(tys.node(t), Ok(tc::PNode::Ref(..))));
        g.callees.push((k, CalleeFacts { writes_lifetimes: writes_lifetime(f.text), self_ref }));
        let (keys, names) = (&*r.keys, r.names);
        let adt_lifetimes = |a: tc::ItemKey| Some(adt_head_lifetimes(keys, names, a));
        let mut buf = SigBuf::new_in(alloc);
        match read_signature(f.text, tys, &f.inputs[..], f.output, &adt_lifetimes, &mut buf, alloc) {
            Ok(()) => g.sigs.push((k, buf)),
            Err(why) => g.sig_refused.push((k, why)),
        }
    }
    g.items = items;
    g
}

/// A move finding's code and words: the Reference, "Moved and copied types".
fn move_words(kind: MoveFindingKind) -> (&'static str, &'static str) {
    match kind {
        MoveFindingKind::UseOfMoved => ("E0382", "a use of a value after it was moved"),
        MoveFindingKind::AssignToMovedPart => ("E0382", "an assignment to a part of a value after it was moved"),
        MoveFindingKind::MoveOut(MoveOut::Deref) => ("E0507", "a move out of a place behind a reference or raw pointer"),
        MoveFindingKind::MoveOut(MoveOut::Index) => ("E0508", "a move out of an indexed place"),
        MoveFindingKind::MoveOut(MoveOut::DropField) => ("E0509", "a move out of a field of a type that implements `Drop`"),
    }
}

/// An invalidated loan's code and words, from the statement at `at` and the access of it that
/// conflicts with `loan` (blaze-flow's `conflicts`, the rule tier two applied): the
/// Reference, "References and borrowing" (a `&mut` borrow is unique; a shared one forbids
/// mutation while it is live).
fn invalidated_words<A: Allocator>(flow: &Flow<A>, at: Point, loan: Loan) -> (&'static str, &'static str) {
    let s = at.stmt();
    let held_mut = matches!(flow.loan(loan).kind, BorrowKind::Mut | BorrowKind::TwoPhase);
    let mut found: Option<Access> = None;
    for_each_access(flow, s, |a| {
        if found.is_none() && conflicts(flow, loan, a, true) {
            found = Some(a);
        }
    });
    let Some(a) = found else {
        return ("E0502", "a live borrow is invalidated here");
    };
    match flow.stmt(s).kind {
        StmtKind::StorageDead(_) => ("E0597", "a borrowed value goes out of scope while the borrow is live"),
        StmtKind::Drop(_) => ("E0505", "a borrowed value is dropped while the borrow is live"),
        StmtKind::Activate(_) => ("E0502", "a two-phase mutable borrow is used while another borrow is live"),
        StmtKind::Assign { value: Value::Borrow { kind, place, .. }, .. } if place == a.place && a.deep && a.write == (kind == BorrowKind::Mut) => {
            match (kind, held_mut) {
                (BorrowKind::Mut, true) => ("E0499", "a second mutable borrow while a mutable borrow is live"),
                (BorrowKind::Mut, false) => ("E0502", "a mutable borrow while a shared borrow is live"),
                _ => ("E0502", "a shared borrow while a mutable borrow is live"),
            }
        }
        _ if a.write && a.deep => ("E0505", "a move out of a value while it is borrowed"),
        _ if a.write => ("E0506", "an assignment to a place while it is borrowed"),
        _ => ("E0503", "a use of a value while it is mutably borrowed"),
    }
}

/// The move and borrow check of `body`, whose type check gave `done`: the facts gathered, the
/// body lowered to its flow, then the move check and the loans tiers.
fn borrows_of<'n, 'a, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, body: &Body<'n>, done: &tc::Checked<'_>, alloc: Alloc<'a>) -> BorrowVerdict<'a> {
    let mut verdict = BorrowVerdict::new_in(alloc);
    if !done.diags.is_empty() {
        verdict.refused = Some(BorrowRefused::Untyped(Untyped::TypeErrors));
        return verdict;
    }
    let langs = BorrowLangs::of(r.index);
    let facts = gather_facts(r, body.owner, done, langs, alloc);
    if let Some(&(adt, fact)) = facts.unknown().first() {
        verdict.refused = Some(BorrowRefused::FactUnknown { adt: tc::ItemKey(adt), fact });
        return verdict;
    }
    let tys = match body.tys.view(&r.store.tys) {
        Ok(t) => t,
        Err(e) => {
            verdict.refused = Some(BorrowRefused::TypeTable(e));
            return verdict;
        }
    };
    let view = body.typeck.view(tys);
    let lowered = match lower(&view, done, &facts, alloc) {
        Ok(l) => l,
        Err(refusal) => {
            verdict.refused = Some(BorrowRefused::Lower(refusal));
            return verdict;
        }
    };
    let record = match blaze_flow::check(&lowered.flow, alloc) {
        Ok(record) => record,
        Err(bound) => {
            verdict.refused = Some(BorrowRefused::Bound(bound));
            return verdict;
        }
    };
    for m in record.moves.iter() {
        let (code, text) = move_words(m.kind);
        verdict.findings.push(BorrowDiagnostic { start: m.span.lo, end: m.span.hi, code, text });
    }
    for b in record.borrows.iter() {
        let (span, (code, text)) = match *b {
            BorrowFinding::Invalidated { at, loan, span } => (span, invalidated_words(&lowered.flow, at, loan)),
            BorrowFinding::UnprovenOutlives { span, .. } => {
                (span, ("outlives", "a lifetime must outlive another and the signature does not say so"))
            }
        };
        verdict.findings.push(BorrowDiagnostic { start: span.lo, end: span.hi, code, text });
    }
    verdict.findings.sort_unstable();
    verdict.findings.dedup();
    verdict
}

impl<'n, S: BodySource<'n>> ProjectWorld<'n, S> {
    /// Checks one body's types ([`ProjectWorld::check`]: its verdict unchanged) and then, when
    /// they were checked clean, its moves and borrows over the same lowered body and `Checked`
    /// record (nothing lowered or checked twice). A body whose types were not checked clean is
    /// [`BorrowRefused::Untyped`]: not checked, never clean.
    pub fn check_borrows<'a>(&mut self, body: BodyRef<'n>, alloc: Alloc<'a>) -> (BodyVerdict<'a>, BorrowVerdict<'a>) {
        let mut borrows: Option<BorrowVerdict<'a>> = None;
        let verdict = self.check_then(body, alloc, |r, lowered, done| {
            borrows = Some(borrows_of(r, lowered, done, alloc));
        });
        let borrows = match borrows {
            Some(b) => b,
            None => {
                let why = if verdict.diagnostics.is_empty() { Untyped::TypesNotChecked } else { Untyped::TypeErrors };
                BorrowVerdict { findings: AVec::new_in(alloc), refused: Some(BorrowRefused::Untyped(why)) }
            }
        };
        (verdict, borrows)
    }
}
