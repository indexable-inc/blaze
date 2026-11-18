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
use blaze_flow_lower::{
    AdtFacts, CallLacks, CalleeFacts, FieldRegion, Facts, FieldLead, Refusal, Requirement, ShapeRefusal, SigBuf, SigLife, SigRefusal,
};
use blaze_flow_lower::sig::{read_async_signature_with, read_impl_signature_with, read_signature_with};
use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;
use core::alloc::Allocator;

use super::body::Body;
use super::borrow_reads::{AdtAnswer, AskedAdts, HeadAnswer, adt_read, file_borrow_reads, head_read};
use super::check::{Records, answer_impls, eval_len};
use super::reads::Read;
use super::borrows_nested::{FlowOf, NestedFacts, Requirements, closure_flow_of, nested_inner_first, nested_range, next_nested};
use super::stalled_goals::{AsyncInteriors, Decision, Stalls, decide_stalled_goals};
use super::borrows_unmade::{AdtUnmade, HeadClass};
use super::heads::{HeadError, Want, member_fn_want};
use super::index::Index;
use super::keys::{Keys, Origin, checker_of};
use super::project::{BodyRef, BodyVerdict, MOST_LENS, ProjectWorld};
use super::project_records::BodySource;
use super::source::NamesSource;
use super::types::Why;

/// Each head's trait bounds with lifetime arguments (piece RB8).
#[path = "borrows_ties.rs"]
mod ties;

/// The signature reader's world: written paths resolved, aliases expanded (lane B11).
#[path = "borrow_sig_world.rs"]
mod sig_world;

/// Each field's regions read from its written type, and each `Copy` impl's demands of the
/// ADT's arguments (lane B11).
#[path = "borrow_fields.rs"]
mod fields;

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
    /// which trait, the ADT's library item when its key begins one, and why the selection gave
    /// no answer (the refused candidate impl, by key and crate, for a gap).
    FactUnknown { adt: tc::ItemKey, item: Option<blaze_id::ItemKey>, fact: &'static str, why: ImplsUnknown },
    /// The body's types over the store's table could not be viewed.
    TypeTable(tc::CheckError),
    /// The lowering refused a form, by name, at an item-relative range.
    Lower(Refusal),
    /// A check went past its stated bound (rule 18): a bug of the checker, named.
    Bound(blaze_flow::Bound<tc::ItemKey>),
    /// A closure's or async block's body needs a lifetime relation among its captures,
    /// parameters and result that its owner would have to prove and that is no requirement on
    /// the closure's or future's value's own regions (one that is becomes the owner's to prove:
    /// borrows_nested.rs `Requirements`, closures and async blocks alike; one that names a
    /// closure's parameter or result, an async block's output or a closure's environment
    /// reference is this): at the item-relative range of the first.
    NestedOutlives { start: u32, end: u32 },
    /// The lowering refused an ADT it has no facts for (`Form::UnknownAdt`, at the
    /// item-relative `start..end`) because the store could not make the ADT's row: the first
    /// such ADT of the body by checker key, why its row was not made, and how many others of
    /// the body were not made either (borrows_unmade.rs).
    AdtNotMade { adt: tc::ItemKey, why: AdtUnmade, others: u32, start: u32, end: u32 },
    /// The lowering refused a call of `callee`, whose head writes lifetimes, at the
    /// item-relative `start..end` because the head's lifetime part was not read from its text
    /// (`Form::CallWritesLifetimes` lacking `CallLacks::Sig`): the callee by checker key and
    /// library item, its head's first words, and why its text was not read.
    CallSigUnread { callee: tc::ItemKey, item: Option<blaze_id::ItemKey>, head: Words, why: SigRefusal, start: u32, end: u32 },
    /// The lowering refused a call of `callee`, whose head writes lifetimes and whose lifetime
    /// part WAS read, at the item-relative `start..end`, because the call could not be related
    /// through that signature (`Form::CallWritesLifetimes` lacking anything but `Sig`):
    /// `lacks` says what (`Shape`: the head's generic shape was not read, `shape_why` says at
    /// which input or output and why; `Bounds`: its declared bounds were not filed;
    /// `Unaligned`: the call's types did not line up with the instantiated head, its `Misfit`
    /// says where), with the callee by checker key and library item and its head's first words
    /// (rule 19).
    CallUnrelated {
        callee: tc::ItemKey,
        item: Option<blaze_id::ItemKey>,
        head: Words,
        lacks: CallLacks,
        shape_why: Option<ShapeRefusal>,
        start: u32,
        end: u32,
    },
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
            BorrowRefused::NestedOutlives { .. } => {
                "a closure or async block needs a lifetime relation its owner must prove that is no requirement on the closure value's regions"
            }
            BorrowRefused::AdtNotMade { .. } => "an ADT the body moves or drops could not be made from the records",
            BorrowRefused::CallSigUnread { .. } => "a callee's signature writes lifetimes its text was not read for",
            BorrowRefused::CallUnrelated { .. } => "a call of a fn whose signature writes lifetimes could not be related through that signature",
        }
    }

    /// The item-relative range the refusal is about: the refused form's, else none.
    pub fn range(&self) -> Option<(u32, u32)> {
        match self {
            BorrowRefused::Lower(r) => Some((r.range.start, r.range.end)),
            BorrowRefused::NestedOutlives { start, end } => Some((*start, *end)),
            BorrowRefused::AdtNotMade { start, end, .. } => Some((*start, *end)),
            BorrowRefused::CallSigUnread { start, end, .. } => Some((*start, *end)),
            BorrowRefused::CallUnrelated { start, end, .. } => Some((*start, *end)),
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
    /// The item-relative range of the closure or async block whose own flow gave `refused`;
    /// `None` for the owner's flow (the flow census names the closure a refusal is in).
    pub within: Option<(u32, u32)>,
    /// How many impls' texts the facts' gathering read (sig_impl.rs `ImplTextsMemo`: each once
    /// a body, its floor; a test reads it, nothing prints it).
    pub impl_text_reads: u32,
}

impl<'a> BorrowVerdict<'a> {
    fn new_in(alloc: Alloc<'a>) -> BorrowVerdict<'a> {
        BorrowVerdict { findings: AVec::new_in(alloc), refused: None, within: None, impl_text_reads: 0 }
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
    /// `Clone`: a `Copy` impl's `T: Clone` bound beside `T: Copy` asks nothing more of `T`
    /// (`Copy: Clone`), read by [`Facts::copy_args`]'s provider (borrow_fields.rs).
    pub clone: Option<tc::ItemKey>,
}

impl BorrowLangs {
    pub fn of(index: &Index<'_>) -> BorrowLangs {
        let lang = |n: &str| index.lang(n).map(checker_of);
        BorrowLangs { copy: lang("copy"), drop: lang("drop"), owned_box: lang("owned_box"), clone: lang("clone") }
    }
}

/// The facts of one body, gathered before it is lowered (blaze-flow-lower's [`Facts`]): every
/// table sorted by checker key, looked up by binary search.
pub struct BorrowFacts<'a> {
    adts: AVec<'a, (u128, AdtFacts)>,
    /// Per (ADT, variant): where its field names start in `fields`, and how many.
    variants: AVec<'a, (u128, u32, u32, u32)>,
    fields: AVec<'a, tc::Name>,
    /// Beside `fields`: the type parameter each field is declared as, when its declared type
    /// is a bare one (blaze-flow-lower `Facts::field_param`).
    field_params: AVec<'a, Option<u32>>,
    /// Beside `fields`: how each field's declared type begins ([`field_lead`]), what a pattern
    /// that peels references under the field peels (blaze-flow-lower `Facts::field_lead`).
    field_leads: AVec<'a, FieldLead>,
    /// Beside `fields`: where each field's regions are in `field_region_list` (borrow_fields.rs
    /// `field_regions_of`), or `fields::NO_ROW` (blaze-flow-lower `Facts::field_regions`).
    field_region_at: AVec<'a, (u32, u32)>,
    field_region_list: AVec<'a, FieldRegion>,
    /// Per ADT whose one `Copy` impl was read, sorted by checker key: where its arguments'
    /// needs are in `copy_need` (borrow_fields.rs `copy_args_of`; `Facts::copy_args`).
    copy_args: AVec<'a, (u128, u32, u32)>,
    copy_need: AVec<'a, bool>,
    callees: AVec<'a, (u128, CalleeFacts)>,
    /// The lifetime part of each fn whose text `read_signature` could line up with its head
    /// (the stand-in for question H1: read from the fn's own text, never guessed).
    sigs: AVec<'a, (u128, SigBuf<Alloc<'a>>)>,
    /// Each fn whose signature text was not read, and why.
    sig_refused: AVec<'a, (u128, SigRefusal)>,
    /// Each head's declared type-outlives bounds and their read rows (front2 call_generics.rs):
    /// a call of a head that writes lifetimes, with lifetime-carrying generic arguments, is
    /// related through them (blaze-flow-lower generic_call.rs, pieces BR3 and BR4).
    calls: super::call_generics::CallBoundsTable<'a>,
    /// Each head's trait bounds with lifetime arguments and their read rows (borrows_ties.rs):
    /// a callee's trait lifetimes are the caller's at a call (blaze-flow-lower trait_ties.rs).
    ties: ties::TiesTable<'a>,
    /// The generic shape of each callee head whose signature was read (blaze-flow-lower
    /// `read_shape`): a call whose type arguments carry origins is related through it.
    shapes: AVec<'a, (u128, blaze_flow_lower::ShapeBuf<Alloc<'a>>)>,
    /// Each head whose lifetime part was read but whose generic shape was not, and why, sorted
    /// by checker key: what a `CallWritesLifetimes { lacks: Shape }` refusal is worded with.
    shape_refused: AVec<'a, (u128, ShapeRefusal)>,
    /// Each trait that is the principal of a trait object of the body's types, sorted, and
    /// whether its header declares a lifetime (`None`: its text not read; sig_impl.rs
    /// `trait_lifetimes`, its row `trait_read`).
    object_traits: AVec<'a, (u128, Option<bool>)>,
    /// Each impl the type check selected at one of the body's calls (`Checked::selected`),
    /// sorted, with its item text and its `(name, text)` associated type members, copied into
    /// the body's allocator (blaze-flow-lower `Facts::impl_source`; sig_impl.rs `impl_texts`,
    /// its row `impl_source_read`). An impl whose text the source does not give has no row,
    /// and a call that needs it is refused by name (`ImplUnread`).
    impl_sources: AVec<'a, (u128, &'a str, &'a [(&'a str, &'a str)])>,
    /// How many impls' texts the gathering read from the source (sig_impl.rs `ImplTextsMemo`:
    /// one an impl a body).
    impl_text_reads: u32,
    /// Whether a fn-pointer type's bound lifetimes are read as bound (`Facts::fn_binders`);
    /// `false` only under the control `FactsPlant::FnBindersFree`.
    fn_binders: bool,
    /// The variance of each ADT's lifetime and type parameters, in head order, from its head
    /// record (lifetimes.rs `AdtLife.variances`), for blaze-flow-lower `Facts::adt_variances`:
    /// per ADT where its run is in `variance_list`. An ADT with no row is taken invariant in
    /// every parameter by the lowering (sound).
    variance_at: AVec<'a, (u128, u32, u32)>,
    variance_list: AVec<'a, blaze_flow_lower::Variance>,
    /// The body's owner and its callees, sorted: the fns whose heads were asked.
    items: AVec<'a, u128>,
    /// Each ADT whose `Copy` or `Drop` answer was not known (the census takes it as `false`;
    /// the command refuses the body: [`BorrowRefused::FactUnknown`]).
    unknown: AVec<'a, (u128, &'static str)>,
    /// Why each of `unknown` was not known, in its order.
    unknown_why: AVec<'a, ImplsUnknown>,
    /// Each ADT of the body's types whose row the store could not make, and why, sorted by
    /// checker key (borrows_unmade.rs; the lowering refuses such an ADT as `UnknownAdt`).
    unmade: AVec<'a, (u128, AdtUnmade)>,
    /// One read row per ADT and per head the facts were taken of (front2 borrow_reads.rs),
    /// for the body's read list (rule 21).
    reads: AVec<'a, Read>,
    /// The text of the body's own fn (blaze-flow-lower `Facts::own_text`): a call through one
    /// of the body's parameters reads that parameter's written type from it. Empty: not known.
    /// No read row of its own: the owner's text is the body's own input (its key covers it).
    own_text: AVec<'a, u8>,
}

impl<'a> BorrowFacts<'a> {
    fn new_in(alloc: Alloc<'a>) -> BorrowFacts<'a> {
        BorrowFacts {
            own_text: AVec::new_in(alloc),
            adts: AVec::new_in(alloc),
            variants: AVec::new_in(alloc),
            fields: AVec::new_in(alloc),
            field_params: AVec::new_in(alloc),
            field_leads: AVec::new_in(alloc),
            field_region_at: AVec::new_in(alloc),
            field_region_list: AVec::new_in(alloc),
            copy_args: AVec::new_in(alloc),
            copy_need: AVec::new_in(alloc),
            callees: AVec::new_in(alloc),
            sigs: AVec::new_in(alloc),
            sig_refused: AVec::new_in(alloc),
            calls: super::call_generics::CallBoundsTable::new_in(alloc),
            ties: ties::TiesTable::new_in(alloc),
            shapes: AVec::new_in(alloc),
            shape_refused: AVec::new_in(alloc),
            object_traits: AVec::new_in(alloc),
            impl_sources: AVec::new_in(alloc),
            impl_text_reads: 0,
            fn_binders: true,
            variance_at: AVec::new_in(alloc),
            variance_list: AVec::new_in(alloc),
            items: AVec::new_in(alloc),
            unknown: AVec::new_in(alloc),
            unknown_why: AVec::new_in(alloc),
            unmade: AVec::new_in(alloc),
            reads: AVec::new_in(alloc),
        }
    }

    /// What the facts were read from, as rows of the body's read list (front2
    /// borrow_reads.rs): one `BorrowAdt` per ADT, one `BorrowHead` per head, unsorted.
    pub fn reads(&self) -> &[Read] {
        &self.reads
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

    /// Why each of [`BorrowFacts::unknown`] was not known, in its order.
    pub fn unknown_why(&self) -> &[ImplsUnknown] {
        &self.unknown_why
    }

    /// Each ADT whose row was not made, and why, by checker key.
    pub fn unmade(&self) -> &[(u128, AdtUnmade)] {
        &self.unmade
    }

    /// How many impls' texts were read from the source while gathering (each once a body:
    /// sig_impl.rs `ImplTextsMemo`), for a test of that floor.
    pub fn impl_text_reads(&self) -> u32 {
        self.impl_text_reads
    }

    fn variant(&self, adt: tc::ItemKey, variant: u32) -> Option<(u32, u32)> {
        let at = self.variants.binary_search_by_key(&(adt.0, variant), |&(a, v, _, _)| (a, v)).ok()?;
        self.variants.get(at).map(|&(_, _, first, count)| (first, count))
    }
}

impl Facts for BorrowFacts<'_> {
    fn own_text(&self) -> Option<&str> {
        core::str::from_utf8(&self.own_text).ok().filter(|t| !t.is_empty())
    }
    fn fn_binders(&self) -> bool {
        self.fn_binders
    }
    fn call_bounds(&self, item: tc::ItemKey) -> Option<blaze_flow_lower::CallBounds<'_>> {
        self.calls.get(item)
    }
    fn ties(&self, item: tc::ItemKey) -> Option<blaze_flow_lower::Ties<'_>> {
        self.ties.get(item)
    }
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
    fn field_param(&self, adt: tc::ItemKey, variant: u32, field: u32) -> Option<u32> {
        let (first, count) = self.variant(adt, variant)?;
        if field >= count {
            return None;
        }
        self.field_params.get((first + field) as usize).copied().flatten()
    }
    fn field_lead(&self, adt: tc::ItemKey, variant: u32, field: u32) -> Option<FieldLead> {
        let (first, count) = self.variant(adt, variant)?;
        if field >= count {
            return None;
        }
        self.field_leads.get((first + field) as usize).copied()
    }
    fn field_regions(&self, adt: tc::ItemKey, variant: u32, field: u32) -> Option<&[FieldRegion]> {
        let (first, count) = self.variant(adt, variant)?;
        if field >= count {
            return None;
        }
        let &(start, len) = self.field_region_at.get((first + field) as usize)?;
        if (start, len) == fields::NO_ROW {
            return None;
        }
        self.field_region_list.get(start as usize..(start + len) as usize)
    }
    fn copy_args(&self, adt: tc::ItemKey) -> Option<&[bool]> {
        let at = self.copy_args.binary_search_by_key(&adt.0, |&(k, _, _)| k).ok()?;
        let &(_, start, len) = self.copy_args.get(at)?;
        self.copy_need.get(start as usize..(start + len) as usize)
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
    fn shape(&self, item: tc::ItemKey) -> Option<blaze_flow_lower::Shape<'_>> {
        let at = self.shapes.binary_search_by_key(&item.0, |(k, _)| *k).ok()?;
        self.shapes.get(at).map(|(_, s)| s.shape())
    }
    fn object_trait_lifetimes(&self, trait_: tc::ItemKey) -> Option<bool> {
        let at = self.object_traits.binary_search_by_key(&trait_.0, |&(k, _)| k).ok()?;
        self.object_traits.get(at).and_then(|&(_, l)| l)
    }
    fn impl_source(&self, imp: tc::ItemKey) -> Option<blaze_flow_lower::ImplSource<'_>> {
        let at = self.impl_sources.binary_search_by_key(&imp.0, |&(k, _, _)| k).ok()?;
        self.impl_sources.get(at).map(|&(_, text, assoc)| blaze_flow_lower::ImplSource { text, assoc })
    }
    fn adt_variances(&self, adt: tc::ItemKey) -> Option<&[blaze_flow_lower::Variance]> {
        let at = self.variance_at.binary_search_by_key(&adt.0, |&(k, _, _)| k).ok()?;
        let &(_, lo, hi) = self.variance_at.get(at)?;
        self.variance_list.get(lo as usize..hi as usize)
    }
}

/// Whether a head's text writes a lifetime before its body: a `'` followed by a letter or `_`
/// before the first `{` (a stand-in for the heads' lifetime part; `'_` counts: it is written).
fn writes_lifetime(text: &str) -> bool {
    let head = text.split('{').next().unwrap_or(text);
    let b = head.as_bytes();
    (0..b.len()).any(|i| b[i] == b'\'' && b.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_') && b.get(i + 2) != Some(&b'\''))
}

/// How a field's declared type `t` (a type of the store's table) begins, for a pattern that peels
/// references under the field (blaze-flow-lower `FieldLead`): a bare type parameter, one
/// reference to a type parameter or to a type with no reference at its head, a type with no
/// reference at its head, or not read (a projection, an opaque type or a trait object, which may
/// stand for a reference; two references deep).
fn field_lead(tys: tc::PTys<'_>, t: tc::PTy) -> FieldLead {
    let plain = |n: tc::PNode| {
        matches!(
            n,
            tc::PNode::Bool
                | tc::PNode::Char
                | tc::PNode::Int(_)
                | tc::PNode::Uint(_)
                | tc::PNode::Float(_)
                | tc::PNode::Str
                | tc::PNode::Never
                | tc::PNode::Tuple(_)
                | tc::PNode::Ptr(..)
                | tc::PNode::Array(..)
                | tc::PNode::Slice(_)
                | tc::PNode::Adt(..)
                | tc::PNode::FnDef(..)
                | tc::PNode::FnPtr(..)
        )
    };
    match tys.node(t) {
        Ok(tc::PNode::Param(p)) => FieldLead::Param(p),
        Ok(tc::PNode::Ref(mutbl, inner)) => match tys.node(inner) {
            Ok(tc::PNode::Param(p)) => FieldLead::Ref { mutbl, param: Some(p) },
            Ok(n) if plain(n) => FieldLead::Ref { mutbl, param: None },
            _ => FieldLead::Unread,
        },
        Ok(n) if plain(n) => FieldLead::Plain,
        _ => FieldLead::Unread,
    }
}

/// How many lifetime parameters an ADT's head declares ([`declared_lifetimes`] of its text):
/// one origin position each (blaze-flow-lower `AdtFacts::lifetimes`).
fn adt_head_lifetimes<'n, S: NamesSource<'n>>(keys: &Keys, names: S, adt: tc::ItemKey) -> u32 {
    // The head after its docs and attributes (head_text.rs): `Rc`'s docs say "'Rc' stands for ..".
    keys.library_in(names, adt)
        .and_then(|lib| names.text_of(lib))
        .map_or(0, |t| declared_lifetimes(super::head_text::after_outer_trivia(t.text)))
}

/// The lifetime parameters of a `struct`, `enum` or `union` head's generics list: the entries
/// of `<..>` right after the name that begin with a lifetime (`struct Checking<'c, 'g, M>`: 2).
/// No generics list: 0. A `>` of a `->` (a fn pointer in a default) closes nothing.
fn declared_lifetimes(head: &str) -> u32 {
    let mut n = 0u32;
    generic_entries(head, &mut |word| n += u32::from(word.starts_with('\'')));
    n
}

/// Each entry of a `struct`, `enum` or `union` head's generics list, in order, as its first
/// word: `'a` for a lifetime, `T` for a type parameter, `const` for a const parameter, `#` for
/// one with an attribute (`#[may_dangle] T`), `` (empty) for anything else. The one scanner
/// [`declared_lifetimes`] counts and borrow_fields.rs names the parameters by.
fn generic_entries<'h>(head: &'h str, each: &mut dyn FnMut(&'h str)) {
    let b = head.as_bytes();
    let word_at = |i: usize, w: &str| {
        head.get(i..).is_some_and(|s| s.starts_with(w))
            && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_'))
            && b.get(i + w.len()).is_some_and(|c| c.is_ascii_whitespace())
    };
    let Some(kw) = (0..b.len()).find(|&i| ["struct", "enum", "union"].iter().any(|w| word_at(i, w))) else { return };
    let mut i = kw;
    while i < b.len() && !b[i].is_ascii_whitespace() {
        i += 1;
    }
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] >= 0x80) {
        i += 1;
    }
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if b.get(i) != Some(&b'<') {
        return;
    }
    let word_byte = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'\'' || c >= 0x80;
    let (mut depth, mut entry) = (0u32, true);
    while i < b.len() {
        let c = b[i];
        match c {
            b'<' => depth += 1,
            b'>' if i > 0 && b[i - 1] == b'-' => {}
            b'>' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            b',' if depth == 1 => entry = true,
            _ if c.is_ascii_whitespace() => {}
            _ => {
                if depth == 1 && entry {
                    // The entry's first word, read in place (the scan itself goes on byte by
                    // byte: a word holds no `<`, `>` or `,`).
                    let mut end = i;
                    while end < b.len() && word_byte(b[end]) {
                        end += 1;
                    }
                    each(match c {
                        b'#' => "#",
                        _ => head.get(i..end).unwrap_or(""),
                    });
                }
                if depth == 1 {
                    entry = false;
                }
            }
        }
        i += 1;
    }
}

/// Asks the store for one head, evaluating the array lengths it names, at most [`MOST_LENS`];
/// `false` when it cannot be made (the fact then stays unknown, and the lowering says so by
/// name where it matters). A caller that files why asks [`ensure_why`].
fn ensure_one<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, w: Want) -> bool {
    ensure_why(r, w).is_ok()
}

/// [`ensure_one`] with the store's refusal kept, by its class: the last `HeadError` met (an
/// array length that did not evaluate is the `NeedConst` that asked for it).
fn ensure_why<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, w: Want) -> Result<(), HeadClass> {
    let mut lens = 0;
    loop {
        match r.store.ensure(r.names, r.keys, w) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let class = HeadClass::of(&e);
                match e {
                    HeadError::Item(_, Why::NeedConst(c, _)) if lens < MOST_LENS => {
                        lens += 1;
                        if eval_len(r, c).is_err() {
                            return Err(class);
                        }
                    }
                    _ => return Err(class),
                }
            }
        }
    }
}

/// A container's head (an impl's, or a trait's with its members) made: `false` when it cannot
/// be.
fn container_head<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, lib: blaze_id::ItemKey, is_trait: bool) -> bool {
    let wants: &[Want] = if is_trait { &[Want::Head(lib), Want::Members(lib)] } else { &[Want::Impl(lib)] };
    wants.iter().all(|&w| ensure_one(r, w))
}

/// Whether `trait_` has an impl for the ADT `adt`: refused when the question could not be
/// answered (no such lang item, or the selection could not be made), with why.
fn has_impl<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, trait_: Option<tc::ItemKey>, adt: tc::ItemKey) -> Result<bool, ImplsUnknown> {
    let trait_ = trait_.ok_or(ImplsUnknown::NoLangItem)?;
    let shape = tc::Shape::Adt(adt);
    if let Some(v) = answer_impls(r, trait_, shape) {
        return Ok(!v.is_empty());
    }
    // Why: the question's gap (a refused candidate) when it was answered, else its fill.
    let b = r.buckets.as_ref().ok_or(ImplsUnknown::NotAnswered)?;
    // An impl READ to be of `trait_` at this shape answers YES whatever the refused candidates
    // are: a refused one can add an impl, never take the read one away (an existing impl is
    // not undone by another; a negative impl of the trait overlapping it is a coherence error,
    // the Reference, "Trait implementation coherence"). Only NO needs every candidate read.
    if b.impls_of(trait_, shape).is_some_and(|got| !got.is_empty()) {
        return Ok(true);
    }
    match b.trait_gap(trait_, shape) {
        Some((candidate, why)) => {
            use core::fmt::Write;
            let mut said = Words::EMPTY;
            let _ = write!(said, "candidate {candidate:x}, header `");
            match r.names.text_of(candidate) {
                Some(t) => {
                    for (i, word) in t.impl_header.unwrap_or(t.text).split_whitespace().take(24).enumerate() {
                        let _ = write!(said, "{}{word}", if i == 0 { "" } else { " " });
                    }
                }
                None => {
                    let _ = write!(said, "(no text)");
                }
            }
            let _ = write!(said, "`: {why}");
            Err(ImplsUnknown::RefusedCandidate { candidate, krate: b.krate(candidate), why: said })
        }
        None => Err(ImplsUnknown::NotAnswered),
    }
}

/// At most [`Words::MOST`] bytes of a refusal's words, kept inline so the refusal stays a
/// `Copy` value; words past the bound are cut at a character's boundary and marked `...`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Words {
    len: u16,
    cut: bool,
    bytes: [u8; Words::MOST],
}

impl Words {
    pub const MOST: usize = 400;
    pub const EMPTY: Words = Words { len: 0, cut: false, bytes: [0u8; Words::MOST] };

    pub fn as_str(&self) -> &str {
        // Only whole characters are ever written ([`Words::write_str`]): always UTF-8.
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("")
    }
}

impl core::fmt::Write for Words {
    /// Appends as much of `s` as fits before the last three bytes, cut at a character's
    /// boundary; a cut writes `...` there and keeps the rest out.
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        if self.cut {
            return Ok(());
        }
        let at = self.len as usize;
        let room = (Words::MOST - 3).saturating_sub(at);
        let mut end = s.len().min(room);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.bytes[at..at + end].copy_from_slice(&s.as_bytes()[..end]);
        let mut len = at + end;
        if end < s.len() {
            self.bytes[len..len + 3].copy_from_slice(b"...");
            len += 3;
            self.cut = true;
        }
        self.len = len as u16;
        Ok(())
    }
}

impl core::fmt::Debug for Words {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

/// Why the selection gave no answer to "does this ADT implement this trait": each a named
/// refusal, never an empty answer taken as no.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImplsUnknown {
    /// The program has no lang item for the trait asked about (`copy`, `drop`).
    NoLangItem,
    /// The question was not answered: the trait's head or the question's fill (the
    /// candidates' headers lowered, then the answer) refused.
    NotAnswered,
    /// A candidate impl of the trait at the asked shape was refused, so the answer may miss
    /// it: the least such candidate by key, the crate it is written in when known, and the
    /// candidate's key, header and why it was refused, in words.
    RefusedCandidate { candidate: blaze_id::ItemKey, krate: Option<usize>, why: Words },
}

/// The facts of the body of `owner` checked as `c`, gathered before it is lowered: per ADT its
/// record type names (asked of the store when not made yet), its fields, whether it is `Copy`
/// and implements `Drop` (asked of the selection), whether it declares lifetimes; per callee
/// (and the owner) its head made, whether it writes lifetimes and takes `&self`, and its
/// signature's lifetime part read from its text. THE ONE HOME of this gathering: the command
/// ([`ProjectWorld::check_borrows`]) and the census (tests/flow_census.rs) both call it.
pub fn gather_facts<'n, 'a, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, owner: tc::ItemKey, c: &tc::Checked<'_>, langs: BorrowLangs, alloc: Alloc<'a>) -> BorrowFacts<'a> {
    gather_facts_planted(r, owner, c, langs, FactsPlant::None, alloc)
}

/// A provider of [`gather_facts`] turned off, for a test's control (rule 23: the cause removed,
/// the effect gone). Never used by the command.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactsPlant {
    /// Every provider on: what the command runs.
    None,
    /// No field's regions (`Facts::field_regions` answers nothing).
    NoFieldRegions,
    /// No `Copy` impl read (`Facts::copy_args` answers nothing).
    NoCopyArgs,
    /// Heads read with no world (blaze-flow-lower `NoAliases`: every path the erased type's
    /// own, no alias expanded).
    NoAliases,
    /// No closure requirement filed (borrows_nested.rs `Requirements` left empty): each
    /// closure's and nested async block's outlives result is then the `NestedOutlives`
    /// refusal, as before the wiring (lanes B11, B14).
    NoClosureRequirements,
    /// No ADT's variances filed (`Facts::adt_variances` answers nothing): every parameter of
    /// every ADT is related both ways (invariant), as before piece B13.
    NoVariances,
    /// No impl's texts given at a selected call (`Facts::impl_source` answers nothing): a
    /// bound over a concrete type is refused `ImplUnread`, as before piece B13.
    NoImplSources,
    /// Every lifetime a fn-pointer type's binder binds read as FREE (`Facts::fn_binders`
    /// false): the body's own a placeholder like a named lifetime, a call through a fn-pointer
    /// parameter relating it to the parameter's own position, as before lane B15 a reading that
    /// knew no binder would.
    FnBindersFree,
}

/// [`gather_facts`] with one provider turned off ([`FactsPlant`]).
#[doc(hidden)]
pub fn gather_facts_planted<'n, 'a, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    owner: tc::ItemKey,
    c: &tc::Checked<'_>,
    langs: BorrowLangs,
    plant: FactsPlant,
    alloc: Alloc<'a>,
) -> BorrowFacts<'a> {
    let mut g = BorrowFacts::new_in(alloc);
    g.fn_binders = plant != FactsPlant::FnBindersFree;
    if let Some(f) = r.store.fns.get(&owner) {
        g.own_text.extend_from_slice(f.text.as_bytes());
    }
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
        // Why a row is missing is kept (rule 3): the key begins no library item, or the
        // store's refusal by class, or (no error and no row) a want that made nothing.
        let mut unmade = AdtUnmade::NoRow;
        if !r.store.adts.contains_key(&key) {
            match r.keys.library_in(r.names, key) {
                Some(lib) => {
                    if let Err(class) = ensure_why(r, Want::Adt(lib)) {
                        unmade = AdtUnmade::Head(class);
                    }
                }
                None => unmade = AdtUnmade::NoLibraryKey,
            }
        }
        let Some(row) = r.store.adts.get(&key) else {
            g.unmade.push((k, unmade));
            g.reads.push(adt_read(k, AdtAnswer::NotMade));
            continue;
        };
        let kind = row.kind;
        let (variants_from, fields_from) = (g.variants.len(), g.fields.len());
        for (vi, v) in row.variants.iter().enumerate() {
            let first = g.fields.len() as u32;
            for f in row.fields.get(v.fields.range()).unwrap_or(&[]) {
                g.fields.push(f.name);
                g.field_params.push(match r.store.tys.view().node(f.ty) {
                    Ok(tc::PNode::Param(p)) => Some(p),
                    _ => None,
                });
                g.field_leads.push(field_lead(r.store.tys.view(), f.ty));
            }
            g.variants.push((k, vi as u32, first, g.fields.len() as u32 - first));
        }
        // Each field's regions, read from its written type (borrow_fields.rs), beside `fields`;
        // what the reading asked is this ADT's row too (rule 21).
        let (asked_adts, asked_paths) = (AskedAdts::new_in(alloc), sig_world::AskedPaths::new_in(alloc));
        let regions_from = g.field_region_at.len();
        {
            let mut field_tys: AVec<'a, tc::PTy> = AVec::new_in(alloc);
            for v in row.variants.iter() {
                for f in row.fields.get(v.fields.range()).unwrap_or(&[]) {
                    field_tys.push(f.ty);
                }
            }
            if plant == FactsPlant::NoFieldRegions {
                for _ in field_tys.iter() {
                    g.field_region_at.push(fields::NO_ROW);
                }
            } else {
                let is_enum = kind == tc::AdtKind::Enum;
                let (at, list) = (&mut g.field_region_at, &mut g.field_region_list);
                fields::field_regions_of(r.names, &*r.keys, &*r.store, key, is_enum, &field_tys, at, list, &asked_adts, &asked_paths, alloc);
            }
        }
        let copy = has_impl(r, langs.copy, key);
        // A `Copy` type has no `Drop` impl: the Reference, "Special types and traits", `Copy`:
        // "Copy cannot be implemented for types that implement Drop" (rustc E0184), so YES to
        // `Copy` is NO to `Drop` without asking (an unread candidate cannot be its `Drop`).
        let drop_impl = match copy {
            Ok(true) => Ok(false),
            _ => has_impl(r, langs.drop, key),
        };
        if let Err(why) = copy {
            g.unknown.push((k, "Copy"));
            g.unknown_why.push(why);
        }
        if let Err(why) = drop_impl {
            g.unknown.push((k, "Drop"));
            g.unknown_why.push(why);
        }
        let (copy, drop_impl) = (copy.ok(), drop_impl.ok());
        // Which arguments the one `Copy` impl needs `Copy` (borrow_fields.rs); ADTs are taken
        // in key order, so `copy_args` stays sorted.
        let copy_need = match plant {
            FactsPlant::NoCopyArgs => None,
            _ => fields::copy_args_of(r, langs, key, copy, &mut g.copy_need, alloc),
        };
        if let Some((start, len)) = copy_need {
            g.copy_args.push((k, start, len));
        }
        // The variance of each lifetime parameter (blaze-flow-lower `Facts::adt_variances`);
        // ADTs are taken in key order, so `variance_at` stays sorted. What was taken is this
        // ADT's row too (rule 21).
        let variances = match plant {
            FactsPlant::NoVariances => None,
            _ => adt_variances_of(r, key, &mut g.variance_list),
        };
        if let Some((lo, hi)) = variances {
            g.variance_at.push((k, lo, hi));
        }
        let copy_taken = copy.unwrap_or(false);
        let facts = AdtFacts {
            copy: copy_taken,
            drop_impl: drop_impl.unwrap_or(false),
            needs_drop: !copy_taken,
            lifetimes: adt_head_lifetimes(&*r.keys, r.names, key),
            union: kind == tc::AdtKind::Union,
            is_enum: kind == tc::AdtKind::Enum,
            is_box: Some(key) == langs.owned_box,
        };
        g.adts.push((k, facts));
        // The row of what was taken (rule 21): the answers as asked, unknown included.
        let mut variant_lens: AVec<'a, u32> = AVec::new_in(alloc);
        for &(_, _, _, count) in g.variants[variants_from..].iter() {
            variant_lens.push(count);
        }
        let (asked_adts, asked_paths) = (asked_adts.take_sorted(), asked_paths.take_sorted());
        let copy_args = copy_need.and_then(|(start, len)| g.copy_need.get(start as usize..(start + len) as usize));
        let variances = variances.and_then(|(lo, hi)| g.variance_list.get(lo as usize..hi as usize));
        let read = adt_read(
            k,
            AdtAnswer::Made {
                copy,
                drop_impl,
                facts,
                variant_lens: &variant_lens,
                fields: &g.fields[fields_from..],
                field_params: &g.field_params[fields_from..],
                field_leads: &g.field_leads[fields_from..],
                field_regions: &g.field_region_at[regions_from..],
                region_list: &g.field_region_list,
                copy_args,
                variances,
                asked_adts: &asked_adts,
                asked_paths: &asked_paths,
            },
        );
        g.reads.push(read);
    }
    let mut items: AVec<'a, u128> = AVec::new_in(alloc);
    for k in c.callees.column::<1>().iter() {
        items.push(k.0);
    }
    items.push(owner.0);
    items.sort_unstable();
    items.dedup();
    // Each impl's texts, read once in this body for every member fn's signature and every call
    // the type check selected the impl at (sig_impl.rs `ImplTextsMemo`).
    let mut impl_memo = super::sig_impl::ImplTextsMemo::new_in(alloc);
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
                    // The container asked by what it is, never tried as one kind and then the
                    // other: a struct's ctor (made with the struct's head) and an anonymous
                    // constant (a member of its module) have no container head to make, and a
                    // refused ask of one is a seed error that refuses a seeded body (lane FM).
                    if let Some(lib) = r.keys.library_in(r.names, container) {
                        match r.names.detail_of(lib) {
                            Some(blaze_surface::ItemDetail::Impl) => {
                                container_head(r, lib, false);
                            }
                            Some(blaze_surface::ItemDetail::Trait) => {
                                container_head(r, lib, true);
                            }
                            _ => {}
                        }
                    }
                    // An impl's fn's signature is its own want (heads.rs `Want::MemberFn`).
                    if let Some(w) = member_fn_want(r.names, &*r.keys, key) {
                        ensure_one(r, w);
                    }
                }
                None => {}
            }
        }
        let Some(f) = r.store.fns.get(&key) else {
            g.reads.push(head_read(k, HeadAnswer::NotMade));
            continue;
        };
        let tys = r.store.tys.view();
        let self_ref = f.self_param && f.inputs.first().is_some_and(|&t| matches!(tys.node(t), Ok(tc::PNode::Ref(..))));
        let callee = CalleeFacts { writes_lifetimes: writes_lifetime(super::head_text::after_outer_trivia(f.text)), self_ref };
        g.callees.push((k, callee));
        let (keys, names) = (&*r.keys, r.names);
        // Each ADT the reading asks about is part of what this head's row says (rule 21).
        let asked = AskedAdts::new_in(alloc);
        // Each written path the reading resolves (a type alias expanded, an ADT named) is part
        // of it too (borrow_sig_world.rs): read from the head's module, its parameters
        // shadowing items of their names.
        let asked_paths = sig_world::AskedPaths::new_in(alloc);
        let mut params: AVec<'a, tc::Name> = AVec::new_in(alloc);
        for &(name, _) in f.params.iter() {
            params.push(name);
        }
        let front = sig_world::FrontWorld::new(names, keys, &r.store.locals, f.module, &params, &asked, &asked_paths, alloc);
        // The control's world (FactsPlant::NoAliases): the reader's own, no path resolved.
        let counted = |a: tc::ItemKey| {
            let lifetimes = adt_head_lifetimes(keys, names, a);
            asked.note(a, lifetimes);
            Some(lifetimes)
        };
        let unresolved = blaze_flow_lower::sig::NoAliases(&counted);
        let world: &dyn blaze_flow_lower::sig::SigWorld = if plant == FactsPlant::NoAliases { &unresolved } else { &front };
        let mut buf = SigBuf::new_in(alloc);
        // An impl's member reads `Self::Name` through the impl's own `type Name` text and the
        // impl's lifetimes through its self type (blaze-flow-lower sig/impl_life.rs); what it
        // took of the impl's text is a row of its own (sig_impl.rs, rule 21).
        let imp = super::sig_impl::impl_of(names, keys, key, f, &mut impl_memo, alloc);
        g.reads.push(super::sig_impl::impl_read(k, &imp, alloc));
        // An `async fn`'s body's value is its future's `Output` as written (blaze-flow-lower
        // `BodyOut`): read with that type (async_output.rs, the head's one home of it).
        let read = match (super::async_output::of(&*r.store, key), imp.text()) {
            (Some(written), it) => read_async_signature_with(f.text, it, tys, &f.inputs[..], f.output, written, world, &mut buf, alloc),
            (None, Some(it)) => read_impl_signature_with(f.text, it, tys, &f.inputs[..], f.output, world, &mut buf, alloc),
            (None, None) => read_signature_with(f.text, tys, &f.inputs[..], f.output, world, &mut buf, alloc),
        };
        let adts = asked.take_sorted();
        let paths = asked_paths.take_sorted();
        match read {
            Ok(()) => {
                g.reads.push(head_read(k, HeadAnswer::Made { callee, sig: Ok(buf.life()), adts: &adts, paths: &paths }));
                g.sigs.push((k, buf));
            }
            Err(why) => {
                g.reads.push(head_read(k, HeadAnswer::Made { callee, sig: Err(why), adts: &adts, paths: &paths }));
                g.sig_refused.push((k, why));
            }
        }
    }
    // The generic shape of each head whose lifetime part was read: where its type parameters
    // stand among its positions (blaze-flow-lower generic_call.rs). A head whose shape cannot
    // be lined up has none, and a call of it with type arguments that carry origins keeps the
    // one origin.
    // A projection in a head's output stands for its normalized type's positions when its
    // trait's text says it is closed (blaze-flow-lower generic_call.rs, "PROJECTIONS"); each
    // trait asked is a row of the body's read list (sig_impl.rs `trait_read`, rule 21).
    let asked_traits: core::cell::RefCell<AVec<'a, u128>> = core::cell::RefCell::new(AVec::new_in(alloc));
    for &k in items.iter() {
        if g.sigs.binary_search_by_key(&k, |(x, _)| *x).is_err() {
            continue;
        }
        let Some(f) = r.store.fns.get(&tc::ItemKey(k)) else { continue };
        let tys = r.store.tys.view();
        let (keys, names) = (&*r.keys, r.names);
        let adt_lifetimes = |a: tc::ItemKey| Some(adt_head_lifetimes(keys, names, a));
        let assoc = |t: tc::ItemKey, name: tc::Name| {
            asked_traits.borrow_mut().push(t.0);
            ties::assoc_read(names, keys, t, name, alloc)
        };
        let mut shape = blaze_flow_lower::ShapeBuf::new_in(alloc);
        match blaze_flow_lower::read_shape(tys, &f.inputs[..], f.output, &adt_lifetimes, &assoc, &mut shape, alloc) {
            Ok(()) => g.shapes.push((k, shape)),
            // Kept, never dropped (rule 3): the note of a call refused for it says why.
            Err(why) => g.shape_refused.push((k, why)),
        }
    }
    let mut asked_traits = asked_traits.into_inner();
    // Each trait object's principal: whether it declares a lifetime decides how an unsized
    // coercion to it is related (blaze-flow-lower `Lower::unsize_to_object`); its row is the
    // same trait row.
    for n in c.types.view().nodes() {
        if let tc::PNode::ExTrait(t, _) = n {
            g.object_traits.push((t.0, None));
        }
    }
    g.object_traits.sort_unstable_by_key(|&(k, _)| k);
    g.object_traits.dedup_by_key(|&mut (k, _)| k);
    for o in g.object_traits.iter_mut() {
        o.1 = super::sig_impl::trait_lifetimes(r.names, &*r.keys, tc::ItemKey(o.0), alloc);
        asked_traits.push(o.0);
    }
    asked_traits.sort_unstable();
    asked_traits.dedup();
    for &t in asked_traits.iter() {
        g.reads.push(super::sig_impl::trait_read(r.names, &*r.keys, tc::ItemKey(t), alloc));
    }
    // Each head's declared type-outlives bounds, from the lifetime part just read and its
    // container's header (front2 call_generics.rs), and their read rows (rule 21).
    for &k in items.iter() {
        let key = tc::ItemKey(k);
        let head = r.store.fns.get(&key);
        let sig = g.sigs.binary_search_by_key(&k, |(x, _)| *x).ok().and_then(|i| g.sigs.get(i)).map(|(_, b)| b);
        let container = match r.keys.origin(key) {
            Some(Origin::Member { container, .. }) => Some(r.keys.library_in(r.names, *container).and_then(|lib| r.names.text_of(lib)).map(|t| t.text)),
            _ => None,
        };
        g.calls.gather(k, head, sig, container, alloc);
        let trait_text = |lib: blaze_id::ItemKey| r.names.text_of(lib).map(|t| t.text);
        g.ties.gather(k, head, sig, &trait_text, alloc);
    }
    if plant == FactsPlant::NoImplSources {
        g.impl_text_reads = impl_memo.reads();
    } else {
        gather_impl_sources(&mut g, r.names, &*r.keys, c, &mut impl_memo, alloc);
    }
    g.items = items;
    g
}

/// Each impl the type check selected at one of the body's calls (`Checked::selected`'s
/// `Selected::Impl` rows): its texts for blaze-flow-lower `Facts::impl_source` (a bound over a
/// concrete type answered by its impl, trait_ties.rs `tie_by_impl`; a hidden input read
/// through the impl, impl_call.rs), and one read row keyed by the impl (sig_impl.rs
/// `impl_source_read`, the same row a member's signature files: rule 21). The texts are
/// copied once into the body's allocator, all impls in one block, so the facts own them.
fn gather_impl_sources<'n, 'a, S: NamesSource<'n>>(
    g: &mut BorrowFacts<'a>,
    names: S,
    keys: &Keys,
    c: &tc::Checked<'_>,
    memo: &mut super::sig_impl::ImplTextsMemo<'n, 'a>,
    alloc: Alloc<'a>,
) {
    let mut imps: AVec<'a, u128> = AVec::new_in(alloc);
    for row in c.selected.iter() {
        if let tc::Selected::Impl { imp, .. } = row.selected {
            imps.push(imp.0);
        }
    }
    imps.sort_unstable();
    imps.dedup();
    // Pass one: each impl's row, and its texts laid end to end in one block (byte ranges).
    let mut bytes: AVec<'a, u8> = AVec::new_in(alloc);
    // (impl, text range, where its members start in `members`, how many)
    let mut spans: AVec<'a, (u128, (u32, u32), u32, u32)> = AVec::new_in(alloc);
    let mut members: AVec<'a, ((u32, u32), (u32, u32))> = AVec::new_in(alloc);
    let put = |bytes: &mut AVec<'a, u8>, s: &str| {
        let lo = bytes.len() as u32;
        bytes.extend_from_slice(s.as_bytes());
        (lo, bytes.len() as u32)
    };
    for &k in imps.iter() {
        let imp = tc::ItemKey(k);
        let texts = memo.get(names, keys, imp, alloc);
        g.reads.push(super::sig_impl::impl_source_read(imp, texts, alloc));
        let Some((text, assoc)) = texts else { continue };
        let at = put(&mut bytes, text);
        let first = members.len() as u32;
        for &(name, body) in assoc {
            let n = put(&mut bytes, name);
            let b = put(&mut bytes, body);
            members.push((n, b));
        }
        spans.push((k, at, first, assoc.len() as u32));
    }
    g.impl_text_reads = memo.reads();
    if spans.is_empty() {
        return;
    }
    // The block lives as long as the body's allocator (the facts are of one body): given up,
    // never freed on its own. Texts laid end to end at char boundaries are UTF-8 whole, so
    // `from_utf8` holds; were it ever not to, no impl has a row and each call that needs one
    // is refused by name (`ImplUnread`), never read from a guess.
    let Ok(block) = core::str::from_utf8(bytes.leak()) else { return };
    let cut = |(lo, hi): (u32, u32)| block.get(lo as usize..hi as usize);
    let mut pairs: AVec<'a, (&'a str, &'a str)> = AVec::new_in(alloc);
    for &(n, b) in members.iter() {
        let (Some(n), Some(b)) = (cut(n), cut(b)) else { return };
        pairs.push((n, b));
    }
    let pairs: &'a [(&'a str, &'a str)] = pairs.leak();
    for &(k, at, first, count) in spans.iter() {
        let (Some(text), Some(assoc)) = (cut(at), pairs.get(first as usize..(first + count) as usize)) else { return };
        g.impl_sources.push((k, text, assoc));
    }
}

/// The variances of ADT `key`'s lifetime parameters, in head order, from its lifetime part
/// (lifetimes.rs `AdtLife.variances`, computed from its fields' written types; one not computed
/// is `In` already, so what is given is sound), pushed onto `list`. `None`: the part was not
/// made (no library key, or refused): the ADT gets no row and the lowering takes every one of
/// its parameters invariant. Type parameters are not computed by the part and get no entry
/// (invariant too).
fn adt_variances_of<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, key: tc::ItemKey, list: &mut AVec<'_, blaze_flow_lower::Variance>) -> Option<(u32, u32)> {
    let lib = r.keys.library_in(r.names, key)?;
    let life = r.store.adt_life(r.names, &*r.keys, lib).ok()?;
    let lo = list.len() as u32;
    for v in life.variances.iter() {
        list.push(match v.variance {
            super::lifetimes::Variance::Co => blaze_flow_lower::Variance::Co,
            super::lifetimes::Variance::Contra => blaze_flow_lower::Variance::Contra,
            super::lifetimes::Variance::In => blaze_flow_lower::Variance::In,
        });
    }
    Some((lo, list.len() as u32))
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
/// mutation while it is live). `returned`: the return place holds the loan there (blaze-flow
/// `BorrowFinding::Invalidated`), so a local's or temporary's end is the returned value
/// outliving what it borrows: E0515, never E0597 or E0505.
fn invalidated_words<A: Allocator>(flow: &Flow<A>, at: Point, loan: Loan, returned: bool) -> (&'static str, &'static str) {
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
        StmtKind::StorageDead(_) | StmtKind::Drop(_) if returned => {
            ("E0515", "a reference to a local or temporary value is returned: the value ends with the fn")
        }
        StmtKind::StorageDead(_) => ("E0597", "a borrowed value goes out of scope while the borrow is live"),
        StmtKind::Drop(_) => ("E0505", "a borrowed value is dropped while the borrow is live"),
        // A two-phase borrow is a mutable one (RFC 2025): under a live mutable loan its
        // reservation or its activation is a second mutable borrow (E0499), under a shared one
        // its activation is E0502.
        StmtKind::Activate(_) if held_mut => ("E0499", "a two-phase mutable borrow is used while a mutable borrow is live"),
        StmtKind::Activate(_) => ("E0502", "a two-phase mutable borrow is used while another borrow is live"),
        StmtKind::Assign { value: Value::Borrow { kind, place, .. }, .. } if place == a.place && a.deep && a.write == (kind == BorrowKind::Mut) => {
            match (kind, held_mut) {
                (BorrowKind::Mut | BorrowKind::TwoPhase, true) => ("E0499", "a second mutable borrow while a mutable borrow is live"),
                (BorrowKind::Mut, false) => ("E0502", "a mutable borrow while a shared borrow is live"),
                _ => ("E0502", "a shared borrow while a mutable borrow is live"),
            }
        }
        _ if a.write && a.deep => ("E0505", "a move out of a value while it is borrowed"),
        _ if a.write => ("E0506", "an assignment to a place while it is borrowed"),
        _ => ("E0503", "a use of a value while it is mutably borrowed"),
    }
}

/// A call of a head that writes lifetimes refused by the lowering (`CallWritesLifetimes`), by
/// name (rule 19): lacking `Sig`, the head's text was not read, and why, from the facts'
/// `sig_refused` ([`BorrowRefused::CallSigUnread`]); lacking anything else, what could not be
/// related, with the shape's refusal from `shape_refused` when that is what it lacked
/// ([`BorrowRefused::CallUnrelated`]). `None` for any other refusal.
fn call_refused<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, facts: &BorrowFacts<'_>, refusal: Refusal) -> Option<BorrowRefused> {
    let blaze_flow_lower::Form::CallWritesLifetimes { callee, lacks } = refusal.form else {
        return None;
    };
    let head = head_words(r, callee);
    let item = r.keys.library_in(r.names, callee);
    let (start, end) = (refusal.range.start, refusal.range.end);
    if lacks != CallLacks::Sig {
        let shape_why = match lacks {
            CallLacks::Shape => facts.shape_refused.binary_search_by_key(&callee.0, |&(k, _)| k).ok().map(|i| facts.shape_refused[i].1),
            _ => None,
        };
        return Some(BorrowRefused::CallUnrelated { callee, item, head, lacks, shape_why, start, end });
    }
    let &(_, why) = facts.sig_refused().iter().find(|(k, _)| *k == callee.0)?;
    Some(BorrowRefused::CallSigUnread { callee, item, head, why, start, end })
}

/// The first words of `callee`'s head, up to its body (rule 19: a person reads which fn).
fn head_words<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, callee: tc::ItemKey) -> Words {
    let mut head = Words::EMPTY;
    if let Some(f) = r.store.fns.get(&callee) {
        use core::fmt::Write;
        let text = super::head_text::after_outer_trivia(f.text);
        for (i, word) in text.split('{').next().unwrap_or(text).split_whitespace().take(24).enumerate() {
            let _ = write!(head, "{}{word}", if i == 0 { "" } else { " " });
        }
    }
    head
}

/// The move and borrow check of `body`, whose type check gave `done`: the facts gathered, the
/// body lowered to its flow, then the move check and the loans tiers.
#[expect(clippy::too_many_arguments, reason = "the records, the body and its check, the two outputs, the test view, the control's plant and the allocator")]
fn borrows_of<'n, 'a, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'n>,
    done: &tc::Checked<'_>,
    reads: &mut AVec<'a, Read>,
    interiors: &mut AsyncInteriors<'a>,
    mut show: Option<&mut dyn core::fmt::Write>,
    plant: FactsPlant,
    alloc: Alloc<'a>,
) -> BorrowVerdict<'a> {
    let mut verdict = BorrowVerdict::new_in(alloc);
    if !done.diags.is_empty() {
        verdict.refused = Some(BorrowRefused::Untyped(Untyped::TypeErrors));
        return verdict;
    }
    let langs = BorrowLangs::of(r.index);
    let facts = gather_facts_planted(r, body.owner, done, langs, plant, alloc);
    verdict.impl_text_reads = facts.impl_text_reads();
    if let Some(out) = show.as_deref_mut() {
        use core::fmt::Write as _;
        let _ = show_facts(&facts, out);
        let (es, ks) = (done.callees.column::<0>(), done.callees.column::<1>());
        for (e, k) in es.iter().zip(ks.iter()) {
            let _ = writeln!(out, "call at {e:?}: {:x}", k.0);
        }
    }
    // Everything below reads these facts, a refusal included: all of them are the body's reads.
    reads.extend_from_slice(facts.reads());
    reads.extend_from_slice(facts.calls.reads());
    reads.extend_from_slice(facts.ties.reads());
    if let (Some(&(adt, fact)), Some(&why)) = (facts.unknown().first(), facts.unknown_why().first()) {
        let item = r.keys.library_in(r.names, tc::ItemKey(adt));
        verdict.refused = Some(BorrowRefused::FactUnknown { adt: tc::ItemKey(adt), item, fact, why });
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
    // Each closure's and nested async block's body first, innermost first (borrows_nested.rs
    // `nested_inner_first`): an outlives result of one between two positions of its value (or
    // `'static`) is a requirement its maker proves at the expression (borrows_nested.rs
    // `Requirements`, blaze-flow-lower `requirement`: one mechanism for both, lane B14); then
    // the body's own flow (and an `async fn`'s root block), asked with those requirements. The
    // verdict is all of them, and a refusal of any is the body's (findings already made are
    // kept; never a pass).
    let mut requirements = Requirements::new_in(alloc);
    let mut needs: AVec<'a, Requirement> = AVec::new_in(alloc);
    for &of in nested_inner_first(&view, alloc).iter() {
        let lowered = {
            let nested = NestedFacts::with_requirements(&facts, &requirements);
            match of {
                FlowOf::Closure(closure) => {
                    closure_flow_of(&view, done, closure, &nested, alloc).map(|cf| (cf.lowered.flow, cf.lowered.params, cf.externals))
                }
                // An async block's lowering also gives its exact interior when a goal stalled
                // (stalled_goals.rs): lowered once, for both.
                FlowOf::AsyncBlock(block) => blaze_flow_lower::lower_async(&view, done, block, &nested, alloc).map(|la| {
                    interiors.keep(&la, done, block, alloc);
                    (la.flow, AVec::new_in(alloc), la.externals)
                }),
                FlowOf::Owner => continue,
            }
        };
        let (flow, params, externals) = match lowered {
            Ok(x) => x,
            Err(refusal) => {
                verdict.refused = Some(lower_refused(r, &facts, refusal));
                verdict.within = nested_range(&view, of);
                break;
            }
        };
        if let Some(out) = show.as_deref_mut() {
            let _ = show_flow(&flow, of, out);
        }
        needs.clear();
        // The control (FactsPlant::NoClosureRequirements): no externals, so no requirement is
        // mapped and each outlives result stays the refusal.
        let externals = if plant == FactsPlant::NoClosureRequirements { None } else { Some(&externals[..]) };
        if file_flow(&flow, &params, of, &view, externals, &mut needs, &mut verdict, alloc) {
            break;
        }
        let made = match of {
            FlowOf::Closure(e) | FlowOf::AsyncBlock(e) => e,
            FlowOf::Owner => continue,
        };
        for &n in needs.iter() {
            requirements.add(made, n);
        }
    }
    let nested = NestedFacts::with_requirements(&facts, &requirements);
    let mut next = if verdict.refused.is_some() { None } else { Some(FlowOf::Owner) };
    while let Some(of) = next {
        // Checked above, innermost first: closures and every async block but an `async fn`'s
        // root (whose placeholders are the signature's: its results are the fn's own).
        if of.is_nested(&view) {
            next = next_nested(&view, of);
            continue;
        }
        // An async block's lowering also gives its exact interior when a goal stalled
        // (stalled_goals.rs): lowered once, for both.
        let (flow, params) = match interiors.flow_of(of, &view, done, &nested, alloc) {
            Ok(f) => f,
            Err(refusal) => {
                verdict.refused = Some(lower_refused(r, &facts, refusal));
                verdict.within = nested_range(&view, of);
                break;
            }
        };
        if let Some(out) = show.as_deref_mut() {
            let _ = show_flow(&flow, of, out);
        }
        if file_flow(&flow, &params, of, &view, None, &mut needs, &mut verdict, alloc) {
            break;
        }
        next = next_nested(&view, of);
    }
    verdict.findings.sort_unstable();
    verdict.findings.dedup();
    verdict
}

/// A refusal of the lowering, by name: the lowering asks `Facts::adt` only of the body's ADTs,
/// and every ADT made is in the facts, so an `UnknownAdt` is one of `unmade`, named with why; a
/// call refused for its head's lifetimes says why ([`call_refused`]).
fn lower_refused<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, facts: &BorrowFacts<'_>, refusal: Refusal) -> BorrowRefused {
    match facts.unmade().first() {
        Some(&(adt, why)) if refusal.form == blaze_flow_lower::Form::UnknownAdt => BorrowRefused::AdtNotMade {
            adt: tc::ItemKey(adt),
            why,
            others: facts.unmade().len() as u32 - 1,
            start: refusal.range.start,
            end: refusal.range.end,
        },
        _ => call_refused(r, facts, refusal).unwrap_or(BorrowRefused::Lower(refusal)),
    }
}

/// Checks one lowered flow `of` and files what it found into `verdict`: its findings, a
/// nested flow's outlives result as a closure requirement pushed onto `needs` (when `externals`
/// is a closure's and blaze-flow-lower `requirement` maps it) or else a `NestedOutlives`
/// refusal. `true`: the body's check stops here (`verdict.refused` is set, `within` with it).
#[expect(clippy::too_many_arguments, reason = "the flow, its parameters, which flow, the body's view, the closure's externals, the two outputs and the allocator")]
fn file_flow<'a>(
    flow: &Flow<Alloc<'a>>,
    params: &[blaze_flow_lower::ParamOrigin],
    of: FlowOf,
    view: &tc::BodyView<'_>,
    externals: Option<&[(blaze_flow::Origin, blaze_flow_lower::External)]>,
    needs: &mut AVec<'a, Requirement>,
    verdict: &mut BorrowVerdict<'a>,
    alloc: Alloc<'a>,
) -> bool {
    let record = match blaze_flow::check_named(view.owner, flow, alloc) {
        Ok(record) => record,
        Err(bound) => {
            verdict.refused = Some(BorrowRefused::Bound(bound));
            verdict.within = nested_range(view, of);
            return true;
        }
    };
    {
        for m in record.moves.iter() {
            let (code, text) = move_words(m.kind);
            verdict.findings.push(BorrowDiagnostic { start: m.span.lo, end: m.span.hi, code, text });
        }
        for b in record.borrows.iter() {
            let (span, (code, text)) = match *b {
                BorrowFinding::Invalidated { at, loan, span, returned } => (span, invalidated_words(flow, at, loan, returned)),
                // A nested body's outlives result is its owner's to prove: a closure's, between
                // two positions of its value, is a requirement its maker's flow emits; any
                // other is refused, never a finding (nor dropped).
                BorrowFinding::UnprovenOutlives { span, from, into, .. } if of.is_nested(view) => {
                    if let Some(req) = externals.and_then(|ext| blaze_flow_lower::requirement(ext, from, into)) {
                        needs.push(req);
                    } else if verdict.refused.is_none() {
                        verdict.refused = Some(BorrowRefused::NestedOutlives { start: span.lo, end: span.hi });
                    }
                    continue;
                }
                // From a placeholder of the body's type parameter (blaze-flow-lower
                // param_outlives.rs): E0310 when every bound on it was read; else not checked,
                // by name (as the nested arm above: findings already made are kept).
                BorrowFinding::UnprovenOutlives { span, from, .. } => match params.iter().find(|p| p.origin == from) {
                    Some(p) if p.certain => {
                        (span, ("E0310", "the parameter type may not live long enough: the callee needs a bound the caller does not declare"))
                    }
                    Some(p) => {
                        if verdict.refused.is_none() {
                            verdict.refused = Some(BorrowRefused::Lower(Refusal {
                                form: blaze_flow_lower::Form::ParamOutlivesUnread { param: p.param },
                                range: tc::Range { start: span.lo, end: span.hi },
                            }));
                        }
                        continue;
                    }
                    None => (span, ("outlives", "a lifetime must outlive another and the signature does not say so")),
                },
            };
            verdict.findings.push(BorrowDiagnostic { start: span.lo, end: span.hi, code, text });
        }
    }
    if verdict.refused.is_some() {
        verdict.within = nested_range(view, of);
        return true;
    }
    false
}

/// One lowered flow in the flow's text form, then what that form leaves out: each statement's
/// item-relative range, the placeholders, which locals mention which origins, the known
/// relations and each loan's statement. For a test that must show why a finding was made.
fn show_flow<A: Allocator>(flow: &Flow<A>, of: FlowOf, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
    struct W<'w>(&'w mut dyn core::fmt::Write);
    impl core::fmt::Write for W<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            self.0.write_str(s)
        }
    }
    let mut w = W(out);
    use core::fmt::Write as _;
    writeln!(w, "flow of {of:?}: {} locals, {} args, {} origins", flow.locals, flow.args, flow.origins)?;
    flow.print(&mut w)?;
    w.write_str("spans:")?;
    for (s, row) in flow.stmts.iter().enumerate() {
        write!(w, " s{s}={}..{}", row.span.lo, row.span.hi)?;
    }
    w.write_str("\nplaceholders:")?;
    for (o, p) in flow.placeholder.iter().enumerate() {
        if *p {
            write!(w, " 'o{o}")?;
        }
    }
    w.write_str("\nmentions:")?;
    for (l, o) in flow.local_origins.iter() {
        write!(w, " {l}:{o}")?;
    }
    w.write_str("\nknown:")?;
    for (from, into) in flow.known.iter() {
        write!(w, " {from}:{into}")?;
    }
    w.write_str("\nloans:")?;
    for (l, row) in flow.loans.iter().enumerate() {
        write!(w, " L{l}@{}", row.at)?;
    }
    w.write_str("\n")
}

/// What the facts say of each head the body calls: its callee facts, lifetime part, generic
/// shape and declared bounds, or why its lifetime part was not read.
fn show_facts(facts: &BorrowFacts<'_>, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
    use core::fmt::Write as _;
    for &k in facts.items() {
        let key = tc::ItemKey(k);
        writeln!(out, "head {k:x}: {:?}", Facts::callee(facts, key))?;
        if let Some(life) = Facts::sig(facts, key) {
            writeln!(out, "  sig {:?} ends {:?} bounds {:?} body {:?}", life.regions, life.ends, life.bounds, life.body)?;
        }
        if let Some(&(_, why)) = facts.sig_refused().iter().find(|(x, _)| *x == k) {
            writeln!(out, "  sig refused {why:?}")?;
        }
        match Facts::shape(facts, key) {
            Some(s) => writeln!(out, "  shape {:?} ends {:?}", s.marks, s.ends)?,
            None => writeln!(out, "  no shape")?,
        }
        match Facts::call_bounds(facts, key) {
            Some(b) => writeln!(out, "  bounds {b:?}")?,
            None => writeln!(out, "  no call bounds")?,
        }
    }
    for &(k, l) in facts.object_traits.iter() {
        writeln!(out, "object trait {k:x}: declares a lifetime {l:?}")?;
    }
    Ok(())
}

impl<'n, S: BodySource<'n>> ProjectWorld<'n, S> {
    /// As [`ProjectWorld::check_borrows`], writing each flow the borrow check ran over to `show`
    /// ([`show_flow`]'s form) before its check: a test's view of why a finding was made.
    pub fn check_borrows_shown<'a>(&mut self, body: BodyRef<'n>, show: &mut dyn core::fmt::Write, alloc: Alloc<'a>) -> (BodyVerdict<'a>, BorrowVerdict<'a>) {
        self.check_borrows_in(body, Some(show), FactsPlant::None, alloc)
    }

    /// As [`ProjectWorld::check_borrows`] with one fact provider turned off ([`FactsPlant`]):
    /// a test's control only.
    #[doc(hidden)]
    pub fn check_borrows_planted<'a>(&mut self, body: BodyRef<'n>, plant: FactsPlant, alloc: Alloc<'a>) -> (BodyVerdict<'a>, BorrowVerdict<'a>) {
        self.check_borrows_in(body, None, plant, alloc)
    }

    /// Checks one body's types ([`ProjectWorld::check`]: its verdict unchanged) and then, when
    /// they were checked clean, its moves and borrows (as [`ProjectWorld::check_borrows_in`]).
    pub fn check_borrows<'a>(&mut self, body: BodyRef<'n>, alloc: Alloc<'a>) -> (BodyVerdict<'a>, BorrowVerdict<'a>) {
        self.check_borrows_in(body, None, FactsPlant::None, alloc)
    }

    /// Checks one body's types ([`ProjectWorld::check`]: its verdict unchanged) and then, when
    /// they were checked clean, its moves and borrows over the same lowered body and `Checked`
    /// record (nothing lowered or checked twice). A body whose types were not checked clean is
    /// [`BorrowRefused::Untyped`]: not checked, never clean.
    fn check_borrows_in<'a>(&mut self, body: BodyRef<'n>, show: Option<&mut dyn core::fmt::Write>, plant: FactsPlant, alloc: Alloc<'a>) -> (BodyVerdict<'a>, BorrowVerdict<'a>) {
        let mut borrows: Option<BorrowVerdict<'a>> = None;
        let mut reads: AVec<'a, Read> = AVec::new_in(alloc);
        let mut decision: Option<Decision<'a>> = None;
        // The check files an auto-trait goal that fails only on an async block's superset
        // interior as a stalled row; the borrow check lowers each async body once, and the
        // rows are decided on the exact interiors of those lowerings (stalled_goals.rs).
        let mut verdict = self.check_then(body, Stalls::Decide, alloc, |r, lowered, done| {
            let _cost = super::tables::cost::Cost::enter(r.store.cost, super::tables::cost::Part::Borrows);
            let mut interiors = AsyncInteriors::new_in(done, alloc);
            borrows = Some(borrows_of(r, lowered, done, &mut reads, &mut interiors, show, plant, alloc));
            decision = Some(decide_stalled_goals(r, lowered, done, &interiors, &mut reads, alloc));
        });
        // What the borrow verdict and the decision read, in the body's read list and its
        // digest (rule 21: borrow_reads.rs, stalled_goals.rs).
        file_borrow_reads(&mut verdict, &reads, alloc);
        if let Some(decision) = decision {
            decision.file(&mut verdict);
        }
        let borrows = match borrows {
            Some(b) => b,
            None => {
                let why = if verdict.diagnostics.is_empty() { Untyped::TypesNotChecked } else { Untyped::TypeErrors };
                BorrowVerdict { findings: AVec::new_in(alloc), refused: Some(BorrowRefused::Untyped(why)), within: None, impl_text_reads: 0 }
            }
        };
        (verdict, borrows)
    }
}
