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
//! Borrow checking runs under `OneRule` (`blaze-borrow` + `RegionSlots`) and its findings are
//! errors (`blaze-cli body_types.rs` `BORROWS_ARE_ERRORS = true`); any body whose borrows cannot
//! be lowered whole files a refusal (`BORROWS_NOT_CHECKED`) so `check` reports it honestly.

use blaze_flow::loans::{Access, conflicts, for_each_access};
use blaze_flow::{BorrowFinding, BorrowKind, Flow, Loan, MoveFindingKind, MoveOut, Point, StmtKind, Value};
use blaze_flow_lower::{
    AdtFacts, CallLacks, CalleeFacts, FieldRegion, Facts, FieldLead, NoRegions, Refusal, RegionFacts, Requirement, ShapeRefusal, SigLife,
};
use blaze_own::{AVec, Alloc, Arc};
use blaze_typeck as tc;
use core::alloc::Allocator;

use super::body::Body;
use super::borrow_reads::{AdtAnswer, HeadAnswer, adt_read, assoc_copy_read, clauses_read, copy_supertrait_read, file_borrow_reads, head_read, impl_read};
use super::check::{Records, answer_impls, eval_len};
use super::reads::Read;
use super::borrows_nested::{
    ClosureEnvUse, FlowOf, NestedFacts, Requirements, classify_closure_env_use, closure_flow_of, closure_flow_of_as_fn_once, env_escapes_closure, flow_of,
    nested_inner_first, nested_range,
    next_nested, prepare_closure_env, propagate_nested_requirement,
};
use super::stalled_goals::{AsyncInteriors, Decision, Stalls, decide_stalled_goals};
use super::borrows_unmade::{AdtUnmade, HeadClass};
use super::heads::{HeadError, Want, member_fn_want};
use super::index::Index;
use super::keys::{Origin, checker_of};
use super::project::{BodyRef, BodyVerdict, MOST_LENS, ProjectWorld};
use super::project_records::BodySource;
use super::source::NamesSource;
use super::tables::view::BodyHeadFacts;
use super::types::Why;

/// The facts a callee's generic shape is counted by (D148: the same as its signature column's):
/// each declared count from the names (`HeadsArity`), an opaque's captures from its head record
/// (`RegionFacts::opaque_captures`, head_record.rs `RegionReader`), as the column was laid.
/// Never the in-process maps for an opaque: they hold only heads made in this process, so a
/// callee whose head came from its record was `CountUnknown(OpaqueCaptures)` (tempo's 102).
struct ShapeArity<'a, H: super::types::Heads + ?Sized, S, R: RegionFacts + ?Sized> {
    declared: super::types::HeadsArity<'a, H, S>,
    regions: &'a R,
}

impl<'n, H: super::types::Heads + ?Sized, S: NamesSource<'n>, R: RegionFacts + ?Sized> tc::regions::Arity for ShapeArity<'_, H, S, R> {
    fn adt_lifetimes(&self, adt: tc::ItemKey) -> tc::regions::Count {
        tc::regions::Arity::adt_lifetimes(&self.declared, adt)
    }
    fn opaque_captures(&self, opaque: tc::ItemKey) -> tc::regions::Count {
        self.regions.opaque_captures(opaque).map_or(tc::regions::Count::NotKnown, tc::regions::Count::Known)
    }
    fn alias_lifetimes(&self, trait_: tc::ItemKey, name: tc::Name) -> tc::regions::Count {
        tc::regions::Arity::alias_lifetimes(&self.declared, trait_, name)
    }
    fn trait_lifetimes(&self, trait_: tc::ItemKey) -> tc::regions::Count {
        tc::regions::Arity::trait_lifetimes(&self.declared, trait_)
    }
}

/// Each head's trait bounds with lifetime arguments (piece RB8).
#[path = "borrows_ties.rs"]
mod ties;

/// Each field's regions from its head record's field column (D148), and each `Copy` impl's
/// demands of the ADT's arguments (lane B11).
#[path = "borrow_fields.rs"]
mod fields;
pub(crate) use fields::{NO_ROW, declared_lifetimes, freeze_field_regions};

/// The region projection of an ADT's head. A body holds a share of these immutable
/// columns, so reading a field never copies the other fields' regions into its frame.
pub type AdtFieldRegions = Arc<FieldRegions, Alloc<'static>>;
pub struct FieldRegions {
    pub at: AVec<'static, (u32, u32)>,
    pub list: AVec<'static, FieldRegion>,
}

impl FieldRegions {
    pub(crate) fn new(at: AVec<'static, (u32, u32)>, list: AVec<'static, FieldRegion>, alloc: Alloc<'static>) -> AdtFieldRegions {
        Arc::new_in(FieldRegions { at, list }, alloc)
    }

    fn field(&self, index: usize) -> Option<&[FieldRegion]> {
        let &(start, len) = self.at.get(index)?;
        if (start, len) == NO_ROW {
            return None;
        }
        self.list.get(start as usize..start as usize + len as usize)
    }
}

struct RegionFields {
    first_variant: usize,
    variant_count: usize,
    first_field: usize,
    regions: AdtFieldRegions,
    /// Per-body slice `(start, len)` into `BorrowFacts::suppressed_field_regions` naming field
    /// indices whose `PNode::Alias` may normalize in this body and must return `None` from
    /// `Facts::field_regions`.
    suppressed: Option<(u32, u32)>,
}

/// One finding of the move or borrow check: its code (rustc's error code where one names the
/// same rule, else a word), what it says, and its item-relative range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BorrowDiagnostic {
    pub start: u32,
    pub end: u32,
    pub code: &'static str,
    pub text: &'static str,
    /// An `Invalidated` finding's borrow: the item-relative range of the statement that took
    /// it (blaze-flow `LoanParts::issued`, rule 71); `None` for any other finding.
    pub borrowed: Option<(u32, u32)>,
    /// What keeps that loan live at the conflict (blaze-flow `LoanParts::kept`); `None` for any
    /// other finding.
    pub kept: Option<KeptAt>,
}

/// What keeps a loan live where it is invalidated (blaze-flow `Kept`), its range item-relative.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeptAt {
    /// The later use: rustc's "borrow later used here".
    Use { start: u32, end: u32 },
    /// A lifetime of the signature holds the loan: it must outlive the call.
    Signature,
    /// None was found, or the finding had no parts row: blaze-flow's liveness invariant broke.
    /// Named in the note, never dropped (rule 3).
    Unfound,
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
    /// A finding that reads regions, in a body one of whose flows the lowering related at some
    /// step by a relation stricter than Rust's (blaze-flow-lower `Lowered::coarse`: the first
    /// such step's form and item-relative range, `CallGenericUnaligned`, `AwaitValueUnrelated`
    /// or `ProjectionUnbound`): the finding may be the relation's artifact, so the body's
    /// borrows are not checked, by that step's name. A body clean under it stands checked;
    /// move findings read no region and are kept (`check_borrows`).
    Stricter(Refusal),
    /// A check went past its stated bound (rule 18): a bug of the checker, named.
    Bound(blaze_flow::Bound<tc::ItemKey>),
    /// A closure's or async block's body needs a lifetime relation among its captures,
    /// parameters and result that its owner would have to prove and that is no requirement on
    /// the closure's or future's value's own regions (one that is becomes the owner's to prove:
    /// borrows_nested.rs `Requirements`, closures and async blocks alike; one that names a
    /// closure's parameter or result, an async block's output or a closure's environment
    /// reference is this): at the item-relative range of the first.
    NestedOutlives { from: Option<blaze_flow_lower::External>, into: Option<blaze_flow_lower::External>, start: u32, end: u32 },
    /// The lowering refused an ADT it has no facts for (`Form::UnknownAdt`, at the
    /// item-relative `start..end`) because the store could not make the ADT's row: the first
    /// such ADT of the body by checker key, why its row was not made, and how many others of
    /// the body were not made either (borrows_unmade.rs).
    AdtNotMade { adt: tc::ItemKey, why: AdtUnmade, others: u32, start: u32, end: u32 },
    /// The lowering refused a call of `callee` (or the body of `callee` itself), with an
    /// argument or the result carrying a lifetime, at the item-relative `start..end`, because
    /// its head regions were not read from its head record (`Form::RegionsUnread`, D148): the
    /// callee by checker key and library item, its head's first words, and why (`NoHead`: no
    /// head record holds them; `HeadRefused`: the head was refused, its own error reported
    /// there; `RecordForm`: a record of another form, rebuilt). Never checked under a guess.
    /// `gap`: when this store made the callee's head (the body's own fn, or a callee made
    /// here), the making store's detail of the refusal (heads.rs `RegionGap::say`: for E0107
    /// the path, its declared and its written lifetime counts); empty for a head read from
    /// another store's record, whose answer is its kind alone (`why`) beside `head`.
    /// `unlaid`: the refusal's typed kind as this store filed it (made here, or read from the
    /// callee's frozen head, form 12: the same in every store), whose class is printed first
    /// (`[NotLaid/ProjectionBoundElsewhere/Output]`); `None` when the head was not filed whole.
    RegionsUnread {
        callee: tc::ItemKey,
        item: Option<blaze_id::ItemKey>,
        head: Words,
        why: tc::region_facts::UnreadWhy,
        unlaid: Option<super::head_record::Unlaid>,
        gap: Words,
        start: u32,
        end: u32,
    },
    /// The lowering refused a call of `callee`, whose head writes lifetimes and whose lifetime
    /// part WAS read, at the item-relative `start..end`, because the call could not be related
    /// through that signature (`Form::CallWritesLifetimes`):
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
            BorrowRefused::Stricter(_) => "a borrow finding under a relation stricter than Rust's, which may be the relation's own",
            BorrowRefused::Bound(_) => "the check went past its bound",
            BorrowRefused::NestedOutlives { .. } => {
                "a closure or async block needs a lifetime relation its owner must prove that is no requirement on the closure value's regions"
            }
            BorrowRefused::AdtNotMade { .. } => "an ADT the body moves or drops could not be made from the records",
            BorrowRefused::RegionsUnread { .. } => "a callee's head regions were not read",
            BorrowRefused::CallUnrelated { .. } => "the call could not be related through the callee's signature",
        }
    }

    /// The item-relative range the refusal is about: the refused form's, else none.
    pub fn range(&self) -> Option<(u32, u32)> {
        match self {
            BorrowRefused::Lower(r) | BorrowRefused::Stricter(r) => Some((r.range.start, r.range.end)),
            BorrowRefused::NestedOutlives { start, end, .. } => Some((*start, *end)),
            BorrowRefused::AdtNotMade { start, end, .. } => Some((*start, *end)),
            BorrowRefused::RegionsUnread { start, end, .. } => Some((*start, *end)),
            BorrowRefused::CallUnrelated { start, end, .. } => Some((*start, *end)),
            _ => None,
        }
    }
}

impl core::fmt::Display for BorrowRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RegionsUnread { why, unlaid, gap, .. } => {
                use tc::region_facts::UnreadWhy;
                match (why, unlaid) {
                    (UnreadWhy::NoHead, _) => f.write_str("[NoHead] ")?,
                    (UnreadWhy::HeadRefused, _) => f.write_str("[HeadRefused] ")?,
                    (UnreadWhy::RecordForm { found }, _) => write!(f, "[RecordForm/{found}] ")?,
                    // The typed class first and short (blaze-cli `first_refusal_class` keeps
                    // up to the first `] `), then the cause in Rust's words and its part.
                    (UnreadWhy::Refused { what }, Some(u)) => {
                        f.write_str("[")?;
                        u.class(&mut *f)?;
                        match u {
                            super::head_record::Unlaid::NotLaid { part, .. } => write!(f, "] {what}, in {part}. ")?,
                            _ => write!(f, "] {what}. ")?,
                        }
                    }
                    (UnreadWhy::Refused { what }, None) => write!(f, "[Refused: {what}] ")?,
                }
                // The making store's detail, when this store made the head (rule 19).
                if !gap.as_str().is_empty() {
                    write!(f, "[{}] ", gap.as_str())?;
                }
            }
            Self::CallUnrelated { lacks, shape_why, .. } => write!(f, "[{}] ", call_relation_reason(*lacks, *shape_why))?,
            // The stricter step by form and reason, first: the class the refusals are counted by.
            Self::Stricter(r) => {
                if let Some((form, why)) = stricter_reason(r.form) {
                    write!(f, "[{form}/{why}] ")?;
                }
            }
            Self::NestedOutlives { from, into, .. } => {
                use blaze_flow_lower::External;
                let endpoint = |end| match end {
                    Some(External::Sig) => "External::Sig",
                    Some(External::SigOwner(_)) => "External::SigOwner",
                    Some(External::Late) => "External::Late",
                    Some(External::Env) => "External::Env",
                    Some(External::Capture(_)) => "capture",
                    Some(External::Static) => "'static",
                    Some(External::OwnerParam(_)) => "owner param",
                    Some(External::Param(_)) => "type param",
                    Some(External::DynPlaceholder(_)) => "dyn placeholder",
                    None => "hidden",
                };
                write!(f, "[{} -> {}] ", endpoint(*from), endpoint(*into))?;
            }
            _ => {}
        }
        write!(f, "{}: {self:?}", self.text())
    }
}

fn call_relation_reason(lacks: CallLacks, shape: Option<ShapeRefusal>) -> &'static str {
    use blaze_flow_lower::Misfit;
    match lacks {
        CallLacks::Bounds => "Bounds",
        CallLacks::JoinedSuperset => "JoinedSuperset",
        CallLacks::Shape => match shape {
            None => "Shape",
            Some(ShapeRefusal::Unknown { .. }) => "Shape/Unknown",
            Some(ShapeRefusal::Bound { .. }) => "Shape/Bound",
            Some(ShapeRefusal::AdtUnknown { .. }) => "Shape/AdtUnknown",
            Some(ShapeRefusal::CountUnknown { .. }) => "Shape/CountUnknown",
            Some(ShapeRefusal::Unnameable { .. }) => "Shape/Unnameable",
            Some(ShapeRefusal::NoHead) => "Shape/NoHead",
        },
        CallLacks::Unaligned(why) => match why {
            Misfit::NotTried => "Unaligned/NotTried",
            Misfit::Inputs { .. } => "Unaligned/Inputs",
            Misfit::Malformed { .. } => "Unaligned/Malformed",
            Misfit::RegionCount { .. } => "Unaligned/RegionCount",
            Misfit::InputProjection { .. } => "Unaligned/InputProjection",
            Misfit::Skeleton { .. } => "Unaligned/Skeleton",
            Misfit::ProjectionLoose { .. } => "Unaligned/ProjectionLoose",
            Misfit::Positions { .. } => "Unaligned/Positions",
        },
    }
}

/// The stricter step's class for a [`BorrowRefused::Stricter`] note, its form and the reason
/// it carries (`lacks`, `why`) by variant name: what the "refused by cause" line counts by
/// (blaze-cli engine_bodies.rs `first_refusal_class` keeps the bracket). The callee, trait and
/// range are in the rest of the note, not the class.
fn stricter_reason(form: blaze_flow_lower::Form) -> Option<(&'static str, &'static str)> {
    use blaze_flow_lower::{AwaitUnrelated as A, Form, ProjectionUnbound as P};
    Some(match form {
        Form::CallGenericUnaligned { lacks, .. } => ("CallGenericUnaligned", call_relation_reason(lacks, None)),
        Form::AwaitValueUnrelated { why, .. } => (
            "AwaitValueUnrelated",
            match why {
                A::NotACall => "NotACall",
                A::NoHead => "NoHead",
                A::NotIdentity => "NotIdentity",
                A::HeadUnread => "HeadUnread",
                A::NoWrittenReturn => "NoWrittenReturn",
                A::OpaqueOutputUnread { .. } => "OpaqueOutputUnread",
                A::OpaqueOutputUnaligned { .. } => "OpaqueOutputUnaligned",
                A::NoBounds => "NoBounds",
                A::BoundUnread { .. } => "BoundUnread",
                A::ShapeUnread => "ShapeUnread",
                A::SignatureUnaligned => "SignatureUnaligned",
                A::GenericUnaligned(m) => call_relation_reason(CallLacks::Unaligned(m), None),
            },
        ),
        Form::ProjectionUnbound { why, .. } => (
            "ProjectionUnbound",
            match why {
                P::NoRow => "NoRow",
                P::RowNotImpl => "RowNotImpl",
                P::RowUnanswered => "RowUnanswered",
                P::RowsDisagree => "RowsDisagree",
                P::SlotsUncounted { .. } => "SlotsUncounted",
                P::InsideImplValue => "InsideImplValue",
                P::ImplHeaderUnread => "ImplHeaderUnread",
                P::ImplAssocUnread => "ImplAssocUnread",
                P::ImplParamsUnread => "ImplParamsUnread",
                P::GatLifetimes => "GatLifetimes",
                P::SelfTypeUnread => "SelfTypeUnread",
                P::SelfSlotsDiffer { .. } => "SelfSlotsDiffer",
                P::SelfRegionUnnumbered => "SelfRegionUnnumbered",
                P::BoundTwice => "BoundTwice",
                P::ValueUnaligned => "ValueUnaligned",
            },
        ),
        // A statement-level stricter relation (blaze-flow-lower `Unified`): each by its own
        // name, so the refusals split by it.
        Form::Unified { why } => (
            "Unified",
            match why {
                blaze_flow_lower::Unified::ValueInexact => "ValueInexact",
                blaze_flow_lower::Unified::BorrowInexact => "BorrowInexact",
                blaze_flow_lower::Unified::SlotInexact => "SlotInexact",
                blaze_flow_lower::Unified::BinaryOperator => "BinaryOperator",
                blaze_flow_lower::Unified::DerefTarget => "DerefTarget",
                blaze_flow_lower::Unified::AsyncBlockOutput => "AsyncBlockOutput",
                blaze_flow_lower::Unified::ClosureValueCall => "ClosureValueCall",
                blaze_flow_lower::Unified::ClosureCallSig => "ClosureCallSig",
                blaze_flow_lower::Unified::ClosureSigTies => "ClosureSigTies",
                blaze_flow_lower::Unified::VarianceUnknown { .. } => "VarianceUnknown",
                blaze_flow_lower::Unified::VarianceInexact => "VarianceInexact",
                blaze_flow_lower::Unified::AggregateHub => "AggregateHub",
            },
        ),
        // blaze-flow-lower records no other form as `coarse` (lib.rs `Lowered::coarse`): no
        // bracket, the note's rest names the form by its `Debug`.
        _ => return None,
    })
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
}

impl<'a> BorrowVerdict<'a> {
    fn new_in(alloc: Alloc<'a>) -> BorrowVerdict<'a> {
        BorrowVerdict { findings: AVec::new_in(alloc), refused: None, within: None }
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
    pub manually_drop: Option<tc::ItemKey>,
    pub phantom_data: Option<tc::ItemKey>,
    /// `Clone`: a `Copy` impl's `T: Clone` bound beside `T: Copy` asks nothing more of `T`
    /// (`Copy: Clone`), read by [`Facts::copy_args`]'s provider (borrow_fields.rs).
    pub clone: Option<tc::ItemKey>,
    /// The traits whose every proof relates no region ([`Facts::trait_relates_nothing`]):
    /// `Sized`, `MetaSized`, `PointeeSized`, `Destruct`, `DiscriminantKind`, and `Pointee`.
    /// Each is `#[rustc_deny_explicit_impl]` in core (no impl can be written), and its builtin
    /// rule reads no region.
    pub relates_nothing: [Option<tc::ItemKey>; 4],
    /// The callable traits (`Fn`, `FnMut`, `FnOnce`), for [`Facts::is_fn_trait`].
    pub fn_traits: [Option<tc::ItemKey>; 3],
    /// `Deref`: for [`Facts::deref_impl`].
    pub deref: Option<tc::ItemKey>,
    /// `core::future::Future`, for [`Facts::is_future_trait`].
    pub future: Option<tc::ItemKey>,
    /// Compiler-builtin traits whose associated types always have zero lifetimes:
    /// `DiscriminantKind::Discriminant` and `Pointee::Metadata`.
    pub discriminant_kind: Option<tc::ItemKey>,
    pub pointee: Option<tc::ItemKey>,
    /// `core::marker::Tuple`, used by `inst_needs_drop` when verifying `FnMut` blanket impls.
    pub tuple_trait: Option<tc::ItemKey>,
    /// `core::any::TryAsDynCompatible` (`#[lang = "try_as_dyn"]`). Note: NOT in `relates_nothing`
    /// because `dyn Trait + 'b: TryAsDynCompatible<'a>` requires `'b == 'a` for soundness of
    /// `core::any::try_as_dyn`.
    pub try_as_dyn: Option<tc::ItemKey>,
}

impl BorrowLangs {
    pub fn of(index: &Index<'_>) -> BorrowLangs {
        let lang = |n: &str| index.lang(n).map(checker_of);
        let discriminant_kind = lang("discriminant_kind");
        let pointee = lang("pointee_trait");
        BorrowLangs {
            copy: lang("copy"),
            drop: lang("drop"),
            owned_box: lang("owned_box"),
            manually_drop: lang("manually_drop"),
            phantom_data: lang("phantom_data"),
            clone: lang("clone"),
            relates_nothing: [lang("sized"), lang("meta_sized"), lang("pointee_sized"), lang("destruct")],
            fn_traits: [lang("fn"), lang("fn_mut"), lang("fn_once")],
            deref: lang("deref"),
            future: lang("future_trait"),
            discriminant_kind,
            pointee,
            tuple_trait: lang("tuple_trait"),
            try_as_dyn: lang("try_as_dyn"),
        }
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
    /// Frozen field region columns by ADT, addressed by variant and field in constant time.
    field_regions: blaze_hash::HashMap<tc::ItemKey, RegionFields, Alloc<'a>>,
    /// Per-body suppressed field indices referenced by [`RegionFields::suppressed`].
    suppressed_field_regions: AVec<'a, u32>,
    /// Per ADT whose one `Copy` impl was read, sorted by checker key: where its arguments'
    /// needs are in `copy_need` (borrow_fields.rs `copy_args_of`; `Facts::copy_args`).
    copy_args: AVec<'a, (u128, u32, u32)>,
    copy_need: AVec<'a, bool>,
    /// Per ADT whose structural drop summary was read, sorted by checker key: where its
    /// arguments' drop flags are in `drop_need` (borrow_fields.rs `drop_facts_of`; `Facts::drop_args`).
    drop_args: AVec<'a, (u128, u32, u32)>,
    drop_need: AVec<'a, bool>,
    /// Instantiated body `PTy`s (sorted by `PTy::0`) whose structural drop glue was resolved by
    /// `borrow_fields::InstDropEval` (normalizing `PNode::Alias` fields), for `Facts::inst_needs_drop`.
    inst_drop: AVec<'a, (u32, bool)>,
    /// Per ADT whose strict-drop summary was read (RFC 1327 `#[may_dangle]`), sorted by checker
    /// key: where its arguments' strict flags are in `drop_strict_need` (`Facts::drop_strict`).
    drop_strict: AVec<'a, (u128, u32, u32)>,
    drop_strict_need: AVec<'a, bool>,
    /// Per ADT whose per-lifetime drop-liveness summary was read, sorted by checker key: where
    /// its lifetime flags are in `drop_lt_need` (`Facts::drop_lifetimes`).
    drop_lifetimes: AVec<'a, (u128, u32, u32)>,
    drop_lt_need: AVec<'a, bool>,
    /// The body owner's type parameters bounded by `Copy` in its clauses (own and inherited:
    /// `Preds::clauses`), sorted, each once (`Facts::param_copy`).
    param_copy: AVec<'a, u32>,
    /// `PNode::Alias` types in this body bounded by `Copy` (via their associated type's own
    /// trait bounds or the body owner's `where` clauses), sorted by `PTy::0` (`Facts::alias_copy`).
    alias_copy: AVec<'a, u32>,
    /// Per callee with a made head, sorted. Its signature regions are NOT here: the lowering
    /// reads them from the head records ([`BorrowFacts::read_by`], D148), each in place.
    callees: AVec<'a, (u128, CalleeFacts)>,
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
    /// whether it declares a lifetime: its head record's lifetime count (`None`: unread), its
    /// row filed once with the ties' (borrows_ties.rs `trait_count`).
    object_traits: AVec<'a, (u128, Option<bool>)>,
    /// Each trait an `ExTrait` node of the body's types or of the heads' columns names, sorted,
    /// and how many lifetime parameters it declares, from the one region counter (types.rs
    /// `HeadsArity`, `Arity::trait_lifetimes`; `None`: not known), for blaze-flow-lower
    /// `Facts::trait_lifetimes` (the one call rule). The read: the names source files the
    /// trait's `Item` cell as it answers (tables/view.rs `declared_lifetimes`, D155).
    trait_counts: AVec<'a, (u128, Option<u32>)>,
    /// Sorted checker keys of gathered traits marked `auto trait` (`TraitFlags::AUTO`), for
    /// `blaze_flow_lower::Facts::is_auto_trait`.
    auto_traits: AVec<'a, u128>,
    /// Each `<_ as trait_>::name` of those types, sorted, and its lifetime count (the trait's
    /// plus the associated type's own, `Arity::alias_lifetimes`), for `Facts::alias_lifetimes`;
    /// its reads the trait's `Item` and `Members` cells, filed by the names source likewise.
    alias_counts: AVec<'a, (u128, tc::Name, Option<u32>)>,
    /// Whether a fn-pointer type's bound lifetimes are read as bound (`Facts::fn_binders`);
    /// `false` only under the control `FactsPlant::FnBindersFree`.
    fn_binders: bool,
    /// The variance of each ADT's lifetime, type and const parameters, lifetimes first, from
    /// the ADT facts (adt_facts.rs, [`adt_variances_in`]), for blaze-flow-lower
    /// `Facts::adt_variances`: per ADT where its run is in `variance_list`. An ADT with no row
    /// (its facts refused) is taken invariant by the lowering, which records that stricter
    /// step (`Unified::VarianceUnknown`).
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
    /// The bounds the owner's signature types and its impl's header types imply (RFC 1214;
    /// [`owner_implied`]), in the owner's numbering: `T: 'r` and `'long: 'short`.
    implied: super::adt_outlives::OutlivesBuf<'a>,
    /// Whether `implied` was computed: `None` under `FactsPlant::NoVariances` (no ADT facts),
    /// `Some(Err(why))` where it was refused (then nothing is implied beyond what is declared).
    implied_state: Option<Result<(), super::adt_facts::ImpliedRefusal>>,
    /// Whether the owner is a `const` or `static` item (an associated const too): its body is
    /// a constant initializer (blaze-flow-lower `Facts::constant_initializer`). The owner's
    /// kind is its own text's, so the body's key covers it.
    constant_initializer: bool,
    /// [`BorrowLangs::relates_nothing`], copied by the gather: the core lang items, read from
    /// the toolchain's library (its key covers them).
    relates_nothing: [Option<tc::ItemKey>; 4],
    fn_traits: [Option<tc::ItemKey>; 3],
    future_trait: Option<tc::ItemKey>,
    into_future_trait: Option<tc::ItemKey>,
    discriminant_kind: Option<tc::ItemKey>,
    pointee: Option<tc::ItemKey>,
    try_as_dyn: Option<tc::ItemKey>,
    /// Unique `Deref` impl per ADT, sorted by ADT key (`Facts::deref_impl`).
    deref_impls: AVec<'a, (u128, tc::ItemKey)>,
    /// `(trait_key, assoc_name, assoc_item)` sorted by `(trait_key, assoc_name)` (`Facts::assoc_item`).
    assoc_items: AVec<'a, (u128, tc::Name, tc::ItemKey)>,
    /// RPITIT opaques sorted by opaque key (`Facts::rpitit_opaque`).
    rpitit_opaques: AVec<'a, (u128, blaze_flow_lower::RpititOpaque)>,
    /// Trait-impl methods overriding RPITIT fns, sorted by `(imp, fn_name)` (`Facts::impl_fn`).
    impl_fns: AVec<'a, (u128, tc::Name, tc::ItemKey)>,
    /// Enclosing trait impl of `owner` and its `Self`-supertrait impls (when unique for `SelfTy`).
    owner_trait_impls: AVec<'a, blaze_flow_lower::OwnerTraitImpl>,
}

/// The facts the lowering asks: a body's gathered [`BorrowFacts`] with the head records' region
/// facts (head_record.rs `RegionReader` over `Store::regions`, D148), the one home of every
/// fn's signature regions, read in place. Made by [`BorrowFacts::read_by`] once the gathering
/// has let go of the records.
#[derive(Clone, Copy)]
pub struct ReadBy<'f, 'a> {
    pub facts: &'f BorrowFacts<'a>,
    regions: &'f dyn RegionFacts,
    /// The table `regions`' columns are laid over: the head store's (`Store::tys`, heads.rs
    /// lays each signature there), never the checked record's, whose `PTy`s are a fresh
    /// numbering (blaze-typeck writeback.rs, `PortableTys::new_in`).
    tys: tc::PTys<'f>,
}

impl<'a> BorrowFacts<'a> {
    /// These facts with `regions`, the head records' region facts, and `tys`, the table their
    /// columns are laid over (`r.store.tys.view()`), as the lowering's [`Facts`].
    pub fn read_by<'f>(&'f self, regions: &'f dyn RegionFacts, tys: tc::PTys<'f>) -> ReadBy<'f, 'a> {
        ReadBy { facts: self, regions, tys }
    }

    /// The bounds the owner's signature and impl header imply (RFC 1214: a fn assumes its
    /// inputs and output well-formed, an impl its header types), in the owner's numbering:
    /// `(type parameter, region)` and `(long, short)`, each sorted. `Ok(None)`: not computed
    /// (the control `FactsPlant::NoVariances`); `Err(why)`: refused, nothing implied.
    pub fn implied(&self) -> Result<Option<(&[(u32, tc::regions::Region)], &[(tc::regions::Region, tc::regions::Region)])>, super::adt_facts::ImpliedRefusal> {
        match self.implied_state {
            None => Ok(None),
            Some(Err(why)) => Err(why),
            Some(Ok(())) => Ok(Some((self.implied.params.as_slice(), self.implied.regions.as_slice()))),
        }
    }

    fn new_in(alloc: Alloc<'a>) -> BorrowFacts<'a> {
        BorrowFacts {
            adts: AVec::new_in(alloc),
            variants: AVec::new_in(alloc),
            fields: AVec::new_in(alloc),
            field_params: AVec::new_in(alloc),
            field_leads: AVec::new_in(alloc),
            field_regions: blaze_hash::HashMap::new_in(alloc),
            suppressed_field_regions: AVec::new_in(alloc),
            copy_args: AVec::new_in(alloc),
            copy_need: AVec::new_in(alloc),
            drop_args: AVec::new_in(alloc),
            drop_need: AVec::new_in(alloc),
            inst_drop: AVec::new_in(alloc),
            drop_strict: AVec::new_in(alloc),
            drop_strict_need: AVec::new_in(alloc),
            drop_lifetimes: AVec::new_in(alloc),
            drop_lt_need: AVec::new_in(alloc),
            param_copy: AVec::new_in(alloc),
            alias_copy: AVec::new_in(alloc),
            callees: AVec::new_in(alloc),
            calls: super::call_generics::CallBoundsTable::new_in(alloc),
            ties: ties::TiesTable::new_in(alloc),
            shapes: AVec::new_in(alloc),
            shape_refused: AVec::new_in(alloc),
            object_traits: AVec::new_in(alloc),
            trait_counts: AVec::new_in(alloc),
            auto_traits: AVec::new_in(alloc),
            alias_counts: AVec::new_in(alloc),
            fn_binders: true,
            variance_at: AVec::new_in(alloc),
            variance_list: AVec::new_in(alloc),
            items: AVec::new_in(alloc),
            unknown: AVec::new_in(alloc),
            unknown_why: AVec::new_in(alloc),
            unmade: AVec::new_in(alloc),
            reads: AVec::new_in(alloc),
            implied: super::adt_outlives::OutlivesBuf::new_in(alloc),
            implied_state: None,
            constant_initializer: false,
            relates_nothing: [None; 4],
            fn_traits: [None; 3],
            future_trait: None,
            into_future_trait: None,
            discriminant_kind: None,
            pointee: None,
            try_as_dyn: None,
            deref_impls: AVec::new_in(alloc),
            assoc_items: AVec::new_in(alloc),
            rpitit_opaques: AVec::new_in(alloc),
            impl_fns: AVec::new_in(alloc),
            owner_trait_impls: AVec::new_in(alloc),
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

    /// Whether `trait_` is `core::any::TryAsDynCompatible` (`#[lang = "try_as_dyn"]`).
    pub fn is_try_as_dyn_trait(&self, trait_: tc::ItemKey) -> bool {
        Some(trait_) == self.try_as_dyn
    }

    fn variant(&self, adt: tc::ItemKey, variant: u32) -> Option<(u32, u32)> {
        let at = self.variants.binary_search_by_key(&(adt.0, variant), |&(a, v, _, _)| (a, v)).ok()?;
        self.variants.get(at).map(|&(_, _, first, count)| (first, count))
    }
}

impl Facts for ReadBy<'_, '_> {
    /// A fn's signature regions (`Facts::sig`, provided over this) and every other head region
    /// fact: the head records', never a text read here (D148).
    fn regions(&self) -> &dyn RegionFacts {
        self.regions
    }
    fn region_tys(&self) -> Option<tc::PTys<'_>> {
        Some(self.tys)
    }
    fn fn_binders(&self) -> bool {
        self.facts.fn_binders
    }
    fn call_bounds(&self, item: tc::ItemKey) -> Option<blaze_flow_lower::CallBounds<'_>> {
        self.facts.calls.get(item)
    }
    fn ties(&self, item: tc::ItemKey) -> Option<blaze_flow_lower::Ties<'_>> {
        self.facts.ties.get(item)
    }
    fn adt(&self, adt: tc::ItemKey) -> Option<AdtFacts> {
        let at = self.facts.adts.binary_search_by_key(&adt.0, |&(k, _)| k).ok()?;
        self.facts.adts.get(at).map(|&(_, f)| f)
    }
    fn callee(&self, item: tc::ItemKey) -> Option<CalleeFacts> {
        let at = self.facts.callees.binary_search_by_key(&item.0, |&(k, _)| k).ok()?;
        self.facts.callees.get(at).map(|&(_, f)| f)
    }
    fn field(&self, adt: tc::ItemKey, variant: u32, name: tc::Name) -> Option<u32> {
        let (first, count) = self.facts.variant(adt, variant)?;
        let names = self.facts.fields.get(first as usize..(first + count) as usize)?;
        names.iter().position(|n| *n == name).map(|i| i as u32)
    }
    fn field_count(&self, adt: tc::ItemKey, variant: u32) -> Option<u32> {
        self.facts.variant(adt, variant).map(|(_, count)| count)
    }
    fn field_param(&self, adt: tc::ItemKey, variant: u32, field: u32) -> Option<u32> {
        let (first, count) = self.facts.variant(adt, variant)?;
        if field >= count {
            return None;
        }
        self.facts.field_params.get((first + field) as usize).copied().flatten()
    }
    fn field_lead(&self, adt: tc::ItemKey, variant: u32, field: u32) -> Option<FieldLead> {
        let (first, count) = self.facts.variant(adt, variant)?;
        if field >= count {
            return None;
        }
        self.facts.field_leads.get((first + field) as usize).copied()
    }
    fn field_regions(&self, adt: tc::ItemKey, variant: u32, field: u32) -> Option<&[FieldRegion]> {
        let row = self.facts.field_regions.get(&adt)?;
        if variant as usize >= row.variant_count {
            return None;
        }
        let &(_, _, first, count) = self.facts.variants.get(row.first_variant + variant as usize)?;
        if field >= count {
            return None;
        }
        let field_idx = first as usize - row.first_field + field as usize;
        if let Some((start, len)) = row.suppressed
            && let Ok(fi) = u32::try_from(field_idx)
            && self
                .facts
                .suppressed_field_regions
                .get(start as usize..start as usize + len as usize)
                .is_some_and(|sl| sl.iter().any(|&x| x == fi))
        {
            return None;
        }
        row.regions.field(field_idx)
    }
    fn copy_args(&self, adt: tc::ItemKey) -> Option<&[bool]> {
        let at = self.facts.copy_args.binary_search_by_key(&adt.0, |&(k, _, _)| k).ok()?;
        let &(_, start, len) = self.facts.copy_args.get(at)?;
        let start = usize::try_from(start).ok()?;
        let end = start.checked_add(usize::try_from(len).ok()?)?;
        self.facts.copy_need.get(start..end)
    }
    fn drop_args(&self, adt: tc::ItemKey) -> Option<&[bool]> {
        let at = self.facts.drop_args.binary_search_by_key(&adt.0, |&(k, _, _)| k).ok()?;
        let &(_, start, len) = self.facts.drop_args.get(at)?;
        let start = usize::try_from(start).ok()?;
        let end = start.checked_add(usize::try_from(len).ok()?)?;
        self.facts.drop_need.get(start..end)
    }
    fn inst_needs_drop(&self, ty: tc::PTy) -> Option<bool> {
        let at = self.facts.inst_drop.binary_search_by_key(&ty.0, |&(k, _)| k).ok()?;
        self.facts.inst_drop.get(at).map(|&(_, v)| v)
    }
    fn drop_strict(&self, adt: tc::ItemKey) -> Option<&[bool]> {
        let at = self.facts.drop_strict.binary_search_by_key(&adt.0, |&(k, _, _)| k).ok()?;
        let &(_, start, len) = self.facts.drop_strict.get(at)?;
        let start = usize::try_from(start).ok()?;
        let end = start.checked_add(usize::try_from(len).ok()?)?;
        self.facts.drop_strict_need.get(start..end)
    }
    fn drop_lifetimes(&self, adt: tc::ItemKey) -> Option<&[bool]> {
        let at = self.facts.drop_lifetimes.binary_search_by_key(&adt.0, |&(k, _, _)| k).ok()?;
        let &(_, start, len) = self.facts.drop_lifetimes.get(at)?;
        let start = usize::try_from(start).ok()?;
        let end = start.checked_add(usize::try_from(len).ok()?)?;
        self.facts.drop_lt_need.get(start..end)
    }
    fn param_copy(&self, index: u32) -> bool {
        self.facts.param_copy.binary_search(&index).is_ok()
    }
    fn alias_copy(&self, alias: tc::PTy) -> bool {
        self.facts.alias_copy.binary_search(&alias.0).is_ok()
    }
    fn shape(&self, item: tc::ItemKey) -> Option<blaze_flow_lower::Shape<'_>> {
        let at = self.facts.shapes.binary_search_by_key(&item.0, |(k, _)| *k).ok()?;
        self.facts.shapes.get(at).map(|(_, s)| s.shape())
    }
    fn object_trait_lifetimes(&self, trait_: tc::ItemKey) -> Option<bool> {
        let at = self.facts.object_traits.binary_search_by_key(&trait_.0, |&(k, _)| k).ok()?;
        self.facts.object_traits.get(at).and_then(|&(_, l)| l)
    }
    fn trait_lifetimes(&self, trait_: tc::ItemKey) -> Option<u32> {
        let at = self.facts.trait_counts.binary_search_by_key(&trait_.0, |&(k, _)| k).ok()?;
        self.facts.trait_counts.get(at).and_then(|&(_, n)| n)
    }
    fn alias_lifetimes(&self, trait_: tc::ItemKey, name: tc::Name) -> Option<u32> {
        let at = self.facts.alias_counts.binary_search_by_key(&(trait_.0, name), |&(k, n, _)| (k, n)).ok()?;
        self.facts.alias_counts.get(at).and_then(|&(_, _, n)| n)
    }
    fn trait_relates_nothing(&self, trait_: tc::ItemKey) -> bool {
        self.facts.relates_nothing.contains(&Some(trait_))
            || Some(trait_) == self.facts.discriminant_kind
            || Some(trait_) == self.facts.pointee
    }
    fn is_auto_trait(&self, trait_: tc::ItemKey) -> bool {
        self.facts.auto_traits.binary_search(&trait_.0).is_ok()
    }
    fn is_try_as_dyn_trait(&self, trait_: tc::ItemKey) -> bool {
        self.facts.is_try_as_dyn_trait(trait_)
    }
    fn is_fn_trait(&self, trait_: tc::ItemKey) -> bool {
        self.facts.fn_traits.contains(&Some(trait_))
    }
    fn is_future_trait(&self, trait_: tc::ItemKey) -> bool {
        Some(trait_) == self.facts.future_trait
    }
    fn is_into_future_trait(&self, trait_: tc::ItemKey) -> bool {
        Some(trait_) == self.facts.into_future_trait
    }
    fn assoc_item(&self, trait_: tc::ItemKey, name: tc::Name) -> Option<tc::ItemKey> {
        let at = self.facts.assoc_items.binary_search_by_key(&(trait_.0, name), |&(k, n, _)| (k, n)).ok()?;
        Some(self.facts.assoc_items[at].2)
    }
    fn owner_trait_impl(&self, trait_: tc::ItemKey) -> Option<blaze_flow_lower::OwnerTraitImpl> {
        self.facts.owner_trait_impls.iter().copied().find(|oti| oti.trait_ == trait_)
    }
    fn deref_impl(&self, adt: tc::ItemKey) -> Option<tc::ItemKey> {
        let at = self.facts.deref_impls.binary_search_by_key(&adt.0, |&(k, _)| k).ok()?;
        Some(self.facts.deref_impls[at].1)
    }
    fn is_zero_slot_builtin_assoc(&self, trait_: tc::ItemKey, name: tc::Name) -> bool {
        (Some(trait_) == self.facts.discriminant_kind && name == tc::Name::of("Discriminant"))
            || (Some(trait_) == self.facts.pointee && name == tc::Name::of("Metadata"))
    }
    fn rpitit_opaque(&self, opaque: tc::ItemKey) -> Option<blaze_flow_lower::RpititOpaque> {
        let at = self.facts.rpitit_opaques.binary_search_by_key(&opaque.0, |&(k, _)| k).ok()?;
        self.facts.rpitit_opaques.get(at).map(|&(_, v)| v)
    }
    fn impl_fn(&self, imp: tc::ItemKey, fn_name: tc::Name) -> Option<tc::ItemKey> {
        let at = self.facts.impl_fns.binary_search_by_key(&(imp.0, fn_name), |&(k, n, _)| (k, n)).ok()?;
        self.facts.impl_fns.get(at).map(|&(_, _, v)| v)
    }
    fn adt_variances(&self, adt: tc::ItemKey) -> Option<&[blaze_flow_lower::Variance]> {
        let at = self.facts.variance_at.binary_search_by_key(&adt.0, |&(k, _, _)| k).ok()?;
        let &(_, lo, hi) = self.facts.variance_at.get(at)?;
        self.facts.variance_list.get(lo as usize..hi as usize)
    }
    // A refused answer is no answer: nothing implied beyond what `signature` reads itself, so
    // the lowering's own unsure marks stand (stricter, never a guess).
    fn implied_bounds(&self) -> Option<(&[(u32, tc::regions::Region)], &[(tc::regions::Region, tc::regions::Region)])> {
        self.facts.implied().ok().flatten()
    }
    fn constant_initializer(&self) -> bool {
        self.facts.constant_initializer
    }
    // The nested bodies are the caller's to say (borrows_nested.rs `NestedFacts` answers them
    // for `borrows_of`, which checks each closure and async block first): these facts alone
    // say no closure or async block body is checked, and nothing is required of one.
    fn closure_bodies_checked(&self) -> bool {
        false
    }
    fn closure_requirements(&self, _: tc::ExprId) -> &[blaze_flow_lower::Requirement] {
        &[]
    }
    fn async_bodies_checked(&self) -> bool {
        false
    }
    // Not gathered: whether a type implements `Future` is not answered, so an `.await` whose
    // operand is not this body's own future is refused `Form::AwaitOperand` by name.
    fn is_future(&self, _: tc::PTy) -> Option<bool> {
        None
    }
}


/// How a field's declared type `t` (a type of the store's table) begins, for a pattern that peels
/// references under the field (blaze-flow-lower `FieldLead`): a bare type parameter, one
/// reference to a type parameter or to a type with no reference at its head, a type with no
/// reference at its head, or not read (a projection, an opaque type or a trait object, which may
/// stand for a reference; two references deep).
/// Collects a trait's sorted direct `Self` supertraits and `Self: 'r` outlives bounds (excluding
/// `Region::Late`), keeping both `copy_supertrait_read` sites identical (rule 21).
fn trait_self_supers<'a>(
    def: &super::heads::TraitExtra,
    tys: tc::PTys<'_>,
    direct: &mut AVec<'a, u128>,
    self_outlives: &mut AVec<'a, tc::regions::Region>,
) {
    direct.clear();
    self_outlives.clear();
    for sc in def.supertraits.iter() {
        match *sc {
            tc::PClause::Trait { trait_: sup, args }
                if tys.list(args).ok().and_then(|l| l.first().copied()).is_some_and(|first| matches!(tys.node(first), Ok(tc::PNode::Param(0)))) =>
            {
                direct.push(sup.0);
            }
            tc::PClause::TypeOutlives { ty, region }
                if matches!(tys.node(ty), Ok(tc::PNode::Param(0)))
                    && !matches!(region, tc::regions::Region::Late { .. }) =>
            {
                self_outlives.push(region);
            }
            _ => {}
        }
    }
    direct.sort_unstable();
    direct.dedup();
    self_outlives.sort_unstable();
    self_outlives.dedup();
}

/// Structural equality of two erased `PTy`s residing in different `PTys` tables (bounded,
/// zero-allocation, used to match owner `where` clauses against body `PNode::Alias` types).
fn pty_eq_across(ta: tc::PTys<'_>, a: tc::PTy, tb: tc::PTys<'_>, b: tc::PTy) -> bool {
    let mut stack = [(tc::PTy(0), tc::PTy(0)); 64];
    let mut sp = 1usize;
    stack[0] = (a, b);
    let mut steps = 0u32;
    while sp > 0 {
        steps += 1;
        if steps > 256 {
            return false;
        }
        sp -= 1;
        let Some(&(x, y)) = stack.get(sp) else { return false };
        let (Ok(nx), Ok(ny)) = (ta.node(x), tb.node(y)) else { return false };
        let push_pair = |stack: &mut [(tc::PTy, tc::PTy); 64], sp: &mut usize, px: tc::PTy, py: tc::PTy| -> bool {
            let Some(slot) = stack.get_mut(*sp) else { return false };
            *slot = (px, py);
            *sp += 1;
            true
        };
        let push_lists = |stack: &mut [(tc::PTy, tc::PTy); 64], sp: &mut usize, lx: tc::List, ly: tc::List| -> bool {
            let (Ok(sx), Ok(sy)) = (ta.list(lx), tb.list(ly)) else { return false };
            if sx.len() != sy.len() {
                return false;
            }
            for (&ex, &ey) in sx.iter().zip(sy.iter()) {
                if !push_pair(stack, sp, ex, ey) {
                    return false;
                }
            }
            true
        };
        match (nx, ny) {
            (tc::PNode::Bool, tc::PNode::Bool)
            | (tc::PNode::Char, tc::PNode::Char)
            | (tc::PNode::Str, tc::PNode::Str)
            | (tc::PNode::Never, tc::PNode::Never) => {}
            (tc::PNode::Int(i), tc::PNode::Int(j)) if i == j => {}
            (tc::PNode::Uint(i), tc::PNode::Uint(j)) if i == j => {}
            (tc::PNode::Float(i), tc::PNode::Float(j)) if i == j => {}
            (tc::PNode::Param(i), tc::PNode::Param(j)) if i == j => {}
            (tc::PNode::ConstParam(i), tc::PNode::ConstParam(j)) if i == j => {}
            (tc::PNode::ConstValue { ty: tx, bits: vx }, tc::PNode::ConstValue { ty: ty, bits: vy }) if vx == vy => {
                if !push_pair(&mut stack, &mut sp, tx, ty) {
                    return false;
                }
            }
            (tc::PNode::Ref(mx, ix), tc::PNode::Ref(my, iy)) | (tc::PNode::Ptr(mx, ix), tc::PNode::Ptr(my, iy)) if mx == my => {
                if !push_pair(&mut stack, &mut sp, ix, iy) {
                    return false;
                }
            }
            (tc::PNode::Slice(ix), tc::PNode::Slice(iy)) => {
                if !push_pair(&mut stack, &mut sp, ix, iy) {
                    return false;
                }
            }
            (tc::PNode::Array(ix, lx), tc::PNode::Array(iy, ly)) if lx == ly => {
                if !push_pair(&mut stack, &mut sp, ix, iy) {
                    return false;
                }
            }
            (tc::PNode::Tuple(lx), tc::PNode::Tuple(ly))
            | (tc::PNode::Union(lx), tc::PNode::Union(ly))
            | (tc::PNode::Dynamic(lx), tc::PNode::Dynamic(ly)) => {
                if !push_lists(&mut stack, &mut sp, lx, ly) {
                    return false;
                }
            }
            (tc::PNode::Adt(kx, lx), tc::PNode::Adt(ky, ly))
            | (tc::PNode::Opaque(kx, lx), tc::PNode::Opaque(ky, ly))
            | (tc::PNode::FnDef(kx, lx), tc::PNode::FnDef(ky, ly))
            | (tc::PNode::ExTrait(kx, lx), tc::PNode::ExTrait(ky, ly))
                if kx == ky =>
            {
                if !push_lists(&mut stack, &mut sp, lx, ly) {
                    return false;
                }
            }
            (tc::PNode::FnPtr(lx, sx), tc::PNode::FnPtr(ly, sy)) if sx == sy => {
                if !push_lists(&mut stack, &mut sp, lx, ly) {
                    return false;
                }
            }
            (tc::PNode::Alias { trait_: tx, name: nx, args: lx }, tc::PNode::Alias { trait_: ty, name: ny, args: ly }) if tx == ty && nx == ny => {
                if !push_lists(&mut stack, &mut sp, lx, ly) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

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
                | tc::PNode::Record(..)
                | tc::PNode::Union(_)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyImpl {
    None,
    Positive,
    Negative,
}

/// Whether `trait_` has a positive, negative, or no explicit impl for `adt`.
fn copy_impl_state<'n, 'a, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    trait_: Option<tc::ItemKey>,
    adt: tc::ItemKey,
    alloc: Alloc<'a>,
) -> Result<CopyImpl, ImplsUnknown> {
    let trait_ = trait_.ok_or(ImplsUnknown::NoLangItem)?;
    let shape = tc::Shape::Adt(adt);
    let mut list: AVec<'a, tc::ItemKey> = AVec::new_in(alloc);
    if let Some(v) = answer_impls(r, trait_, shape) {
        list.extend_from_slice(&v);
    } else {
        let b = r.buckets.as_ref().ok_or(ImplsUnknown::NotAnswered)?;
        if let Some(got) = b.impls_of(trait_, shape)
            && !got.is_empty()
        {
            list.extend_from_slice(got);
        } else {
            return match b.trait_gap(trait_, shape) {
                Some((candidate, why)) => {
                    use core::fmt::Write;
                    let mut said = Words::EMPTY;
                    let _ = write!(said, "candidate {candidate:x}, header `");
                    match r.names.signature_text_of(candidate) {
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
            };
        }
    }
    if list.is_empty() {
        return Ok(CopyImpl::None);
    }
    let mut saw_neg = false;
    for &imp in list.iter() {
        if !r.store.impls.contains_key(&imp)
            && let Some(lib) = r.keys.library_in(r.names, imp)
        {
            let _ = ensure_one(r, Want::Impl(lib));
        }
        match r.store.impls.get(&imp).map(|h| h.polarity) {
            Some(tc::ImplPolarity::Positive) | None => return Ok(CopyImpl::Positive),
            Some(tc::ImplPolarity::Negative) => saw_neg = true,
            Some(tc::ImplPolarity::Reservation) => {}
        }
    }
    Ok(if saw_neg { CopyImpl::Negative } else { CopyImpl::None })
}

/// Whether an `AUTO_COPY_PUB` struct `root` is `Copy` (for instantiations where its required
/// generic parameters are `Copy`), pushing its required-parameter mask onto `need` when
/// `generics > 0`. Iterative worklist (no recursion), bounded to 256 steps.
fn auto_copy_of<'n, 'a, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    langs: BorrowLangs,
    root: tc::ItemKey,
    need: &mut AVec<'a, bool>,
    alloc: Alloc<'a>,
) -> Option<(bool, Option<(u32, u32)>)> {
    let generics = r.store.adts.get(&root)?.generics;
    let g_usize = usize::try_from(generics).ok()?;
    let start = need.len();
    for _ in 0..g_usize {
        need.push(false);
    }
    let mut stack: AVec<'a, (tc::PTy, Option<usize>)> = AVec::new_in(alloc);
    let mut envs: AVec<'a, (Option<usize>, tc::List)> = AVec::new_in(alloc);
    let mut nested_need: AVec<'a, bool> = AVec::new_in(alloc);
    if let Some(row) = r.store.adts.get(&root) {
        for f in row.fields.iter() {
            stack.push((f.ty, None));
        }
    }
    let mut steps = 0u32;
    while let Some((ty, env)) = stack.pop() {
        steps += 1;
        if steps > 256 {
            need.truncate(start);
            return Some((false, None));
        }
        let Ok(node) = r.store.tys.view().node(ty) else {
            need.truncate(start);
            return Some((false, None));
        };
        match node {
            tc::PNode::Bool
            | tc::PNode::Char
            | tc::PNode::Int(_)
            | tc::PNode::Uint(_)
            | tc::PNode::Float(_)
            | tc::PNode::Never
            | tc::PNode::Ptr(..)
            | tc::PNode::FnDef(..)
            | tc::PNode::FnPtr(..)
            | tc::PNode::ConstValue { .. }
            | tc::PNode::ConstParam(_)
            | tc::PNode::Ref(tc::Mutbl::Not, _) => {}
            tc::PNode::Array(inner, _) | tc::PNode::Pat(inner, _) => {
                stack.push((inner, env));
            }
            tc::PNode::Tuple(l) | tc::PNode::Record(_, l) | tc::PNode::Union(l) => {
                let Ok(items) = r.store.tys.view().list(l) else {
                    need.truncate(start);
                    return Some((false, None));
                };
                for &item in items {
                    stack.push((item, env));
                }
            }
            tc::PNode::Param(p) => match env {
                None => {
                    let Ok(pi) = usize::try_from(p) else {
                        need.truncate(start);
                        return Some((false, None));
                    };
                    if let Some(slot) = need.get_mut(start + pi) {
                        *slot = true;
                    } else {
                        need.truncate(start);
                        return Some((false, None));
                    }
                }
                Some(e) => {
                    let &(parent, args_list) = envs.get(e)?;
                    let Ok(items) = r.store.tys.view().list(args_list) else {
                        need.truncate(start);
                        return Some((false, None));
                    };
                    let Ok(pi) = usize::try_from(p) else {
                        need.truncate(start);
                        return Some((false, None));
                    };
                    let Some(&arg) = items.get(pi) else {
                        need.truncate(start);
                        return Some((false, None));
                    };
                    stack.push((arg, parent));
                }
            },
            tc::PNode::Adt(nested_k, nested_args) => {
                if !r.store.adts.contains_key(&nested_k)
                    && let Some(lib) = r.keys.library_in(r.names, nested_k)
                {
                    let _ = ensure_one(r, Want::Adt(lib));
                }
                match copy_impl_state(r, langs.copy, nested_k, alloc) {
                    Ok(CopyImpl::Positive) => {
                        nested_need.clear();
                        let mask = fields::copy_args_of(r, langs, nested_k, Some(true), &mut nested_need, alloc);
                        let Ok(args) = r.store.tys.view().list(nested_args) else {
                            need.truncate(start);
                            return Some((false, None));
                        };
                        match mask {
                            Some((ms, ml))
                                if usize::try_from(ml).ok() == Some(args.len())
                                    && let Ok(ms_u) = usize::try_from(ms)
                                    && let Some(flags) = nested_need.get(ms_u..ms_u + args.len()) =>
                            {
                                for (&a, &req) in args.iter().zip(flags.iter()) {
                                    if req {
                                        stack.push((a, env));
                                    }
                                }
                            }
                            _ => {
                                for &a in args {
                                    stack.push((a, env));
                                }
                            }
                        }
                    }
                    Ok(CopyImpl::None)
                        if r.store.is_auto_copy_pub(r.names, r.keys, nested_k)
                            && matches!(has_impl(r, langs.drop, nested_k), Ok(false) | Err(ImplsUnknown::NoLangItem))
                            && !matches!(copy_impl_state(r, langs.clone, nested_k, alloc), Ok(CopyImpl::Negative)) =>
                    {
                        let env_id = envs.len();
                        envs.push((env, nested_args));
                        if let Some(nrow) = r.store.adts.get(&nested_k) {
                            for f in nrow.fields.iter() {
                                stack.push((f.ty, Some(env_id)));
                            }
                        } else {
                            need.truncate(start);
                            return Some((false, None));
                        }
                    }
                    _ => {
                        need.truncate(start);
                        return Some((false, None));
                    }
                }
            }
            _ => {
                need.truncate(start);
                return Some((false, None));
            }
        }
    }
    if g_usize == 0 {
        need.truncate(start);
        return Some((true, None));
    }
    let start_u32 = u32::try_from(start).ok()?;
    let len_u32 = u32::try_from(g_usize).ok()?;
    Some((true, Some((start_u32, len_u32))))
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
            match r.names.signature_text_of(candidate) {
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
/// (and the owner) its head made, whether it writes lifetimes and takes `&self`, its generic
/// shape and its clauses' outlives and ties, each over its head record's regions (D148; no
/// fn's text is read: the lowering reads the signature column itself, [`BorrowFacts::read_by`]).
/// THE ONE HOME of this gathering: the command
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
    /// The lowering given no head records' regions (blaze-flow-lower `NoRegions`): every call
    /// and body that carries a lifetime refused `RegionsUnread`. Its name is the old control's
    /// (heads read with no alias world): the producer now expands an alias where it lays a
    /// head's column (D148, piece L1a), so removing the column is what removes that cause.
    NoAliases,
    /// No closure requirement filed (borrows_nested.rs `Requirements` left empty): each
    /// closure's and nested async block's outlives result is then the `NestedOutlives`
    /// refusal, as before the wiring (lanes B11, B14).
    NoClosureRequirements,
    /// No ADT's variances filed (`Facts::adt_variances` answers nothing): every parameter of
    /// every ADT is related both ways (invariant), as before piece B13.
    NoVariances,
    /// Every lifetime a fn-pointer type's binder binds read as FREE (`Facts::fn_binders`
    /// false): the body's own a placeholder like a named lifetime, a call through a fn-pointer
    /// parameter relating it to the parameter's own position, as before lane B15 a reading that
    /// knew no binder would.
    FnBindersFree,
    /// No trait's and no alias's lifetime count filed (`Facts::trait_lifetimes` and
    /// `Facts::alias_lifetimes` answer nothing): the one call rule refuses each call whose
    /// contract holds an alias or a trait reference by `Count`, and each selected impl read
    /// for an associated value by `HeadUnread`, as before the counts reached the flows
    /// (borrows_nested.rs `NestedFacts`).
    NoTraitCounts,
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
    g.relates_nothing = langs.relates_nothing;
    g.fn_traits = langs.fn_traits;
    g.future_trait = langs.future;
    g.into_future_trait = super::langs::find(r.index, crate::resolve::Paths { names: r.names }, tc::LangItem::IntoFuture).ok().flatten().map(checker_of);
    g.discriminant_kind = langs.discriminant_kind;
    g.pointee = langs.pointee;
    g.try_as_dyn = langs.try_as_dyn;
    // A `const` or `static` item's body (module-level, local in a fn/macro, associated, or
    // anonymous) is a constant initializer: its extending borrows' temporaries live for `'static`
    // (the Reference, "Temporary lifetime extension").
    g.constant_initializer = matches!(
        super::by_origin::kind_of_key(r.names, &*r.keys, owner),
        Ok(
            super::by_origin::KeyKind::Item(super::by_origin::ItemKind::Const(_) | super::by_origin::ItemKind::Static(_))
                | super::by_origin::KeyKind::MemberConst { .. }
                | super::by_origin::KeyKind::Anon { .. }
        )
    );
    // The owner's `T: Copy` and transitive supertrait `T: 'r` clauses (own and inherited),
    // including transitive supertraits (`T: Tr` where `trait Tr: Copy` or `trait Tr: 'static`,
    // e.g. `Planted<P>: Copy`, `Relation: 'static`, `Prefab: 'static`, or parameterized
    // `L: LoadItems<'a>` where `trait LoadItems<'a>: 'a` implies `L: 'a`). Each inspected
    // trait's direct `Self` supertraits and `Self: 'r` outlives bounds are filed as a
    // `BorrowHead` read row (rule 21).
    let mut param_outlives: AVec<'a, (u32, tc::regions::Region)> = AVec::new_in(alloc);
    {
        let mut lt_pool: AVec<'a, Option<tc::regions::Region>> = AVec::new_in(alloc);
        let mut param_traits: AVec<'a, (u32, tc::ItemKey, usize, usize)> = AVec::new_in(alloc);
        {
            let owner_regions = r.store.regions.reader(r.store.work_alloc());
            let owner_views = owner_regions.clauses(owner).ok();
            let mut read_owner_clauses = false;
            if let Some(head) = r.store.fns.get(&owner)
                && let super::types::SelfTy::Trait(lib) = head.self_
            {
                let tk = checker_of(lib);
                let lt_start = lt_pool.len();
                if let Ok(params) = owner_regions.lifetime_params(tk) {
                    for i in 0..params.count() {
                        lt_pool.push(Some(tc::regions::Region::Param(i)));
                    }
                }
                param_traits.push((0, tk, lt_start, lt_pool.len() - lt_start));
            }
            if let Some(p) = r.store.preds.get(&owner) {
                let tys = r.store.tys.view();
                for (nth, c) in p.clauses.iter().enumerate() {
                    if let tc::PClause::Trait { trait_, args } = *c
                        && let Ok(list) = tys.list(args)
                        && let Some(&first) = list.first()
                        && let Ok(tc::PNode::Param(i)) = tys.node(first)
                    {
                        if Some(trait_) == langs.copy {
                            g.param_copy.push(i);
                        } else {
                            let lt_start = lt_pool.len();
                            if let Some(views) = owner_views
                                && let Ok(nth_u32) = u32::try_from(nth)
                                && let Some(cv) = views.iter().find(|v| v.nth == nth_u32)
                            {
                                read_owner_clauses = true;
                                for &reg in cv.col.own() {
                                    lt_pool.push(match reg {
                                        tc::regions::Region::Static | tc::regions::Region::Param(_) => Some(reg),
                                        tc::regions::Region::Late { .. } => None,
                                    });
                                }
                            }
                            param_traits.push((i, trait_, lt_start, lt_pool.len() - lt_start));
                        }
                    }
                }
            }
            if read_owner_clauses {
                g.reads.push(clauses_read(owner.0, &owner_regions));
            }
        }
        if !param_traits.is_empty() {
            let mut visited: AVec<'a, (tc::ItemKey, usize, usize)> = AVec::new_in(alloc);
            let mut stack: AVec<'a, (tc::ItemKey, usize, usize)> = AVec::new_in(alloc);
            let mut direct: AVec<'a, u128> = AVec::new_in(alloc);
            let mut self_outlives: AVec<'a, tc::regions::Region> = AVec::new_in(alloc);
            let mut recorded: AVec<'a, tc::ItemKey> = AVec::new_in(alloc);
            let mut clauses_recorded: AVec<'a, tc::ItemKey> = AVec::new_in(alloc);
            for &(param, start_trait, start_lo, start_len) in param_traits.iter() {
                let mut found_copy = g.param_copy.scan_for(&param).is_some();
                let mut found_static = param_outlives.iter().any(|&(p, reg)| p == param && reg == tc::regions::Region::Static);
                if found_copy && found_static {
                    continue;
                }
                visited.clear();
                stack.clear();
                stack.push((start_trait, start_lo, start_len));
                while let Some((t, t_lo, t_len)) = stack.pop() {
                    if Some(t) == langs.copy {
                        found_copy = true;
                        if found_static {
                            break;
                        }
                        continue;
                    }
                    let t_lts_match = |lo: usize, len: usize| lt_pool.get(lo..lo + len) == lt_pool.get(t_lo..t_lo + t_len);
                    if visited.iter().any(|&(vk, vlo, vlen)| vk == t && t_lts_match(vlo, vlen)) || visited.len() >= 64 {
                        continue;
                    }
                    visited.push((t, t_lo, t_len));
                    if !r.store.trait_defs.contains_key(&t)
                        && let Some(lib) = r.keys.library_in(r.names, t)
                    {
                        ensure_one(r, Want::Members(lib));
                    }
                    if let Some(def) = r.store.trait_defs.get(&t) {
                        let tys = r.store.tys.view();
                        trait_self_supers(def, tys, &mut direct, &mut self_outlives);
                        if recorded.scan_for(&t).is_none() {
                            recorded.push(t);
                            g.reads.push(copy_supertrait_read(t.0, Some((&direct, &self_outlives))));
                        }
                        for &reg in self_outlives.iter() {
                            match reg {
                                tc::regions::Region::Static => {
                                    found_static = true;
                                    param_outlives.push((param, tc::regions::Region::Static));
                                }
                                tc::regions::Region::Param(j) => {
                                    if let Ok(ji) = usize::try_from(j)
                                        && let Some(Some(mapped)) = lt_pool.get(t_lo..t_lo + t_len).and_then(|s| s.get(ji)).copied()
                                    {
                                        if mapped == tc::regions::Region::Static {
                                            found_static = true;
                                        }
                                        param_outlives.push((param, mapped));
                                    }
                                }
                                tc::regions::Region::Late { .. } => {}
                            }
                        }
                        if found_copy && found_static {
                            break;
                        }
                        let t_regions = r.store.regions.reader(r.store.work_alloc());
                        let t_views = if !direct.is_empty() && t_len > 0 {
                            if clauses_recorded.scan_for(&t).is_none() {
                                clauses_recorded.push(t);
                                g.reads.push(clauses_read(t.0, &t_regions));
                            }
                            t_regions.clauses(t).ok()
                        } else {
                            None
                        };
                        for &sup in direct.iter() {
                            let sup_key = tc::ItemKey(sup);
                            let mut pushed_any = false;
                            if let Some(views) = t_views {
                                for cv in views {
                                    if let Ok(tc::PNode::ExTrait(ct, args)) = tys.node(cv.col.ty())
                                        && ct == sup_key
                                        && tys
                                            .list(args)
                                            .ok()
                                            .and_then(|l| l.first().copied())
                                            .is_some_and(|first| matches!(tys.node(first), Ok(tc::PNode::Param(0))))
                                    {
                                        let next_lo = lt_pool.len();
                                        let own_slice = cv.col.own();
                                        for &reg in own_slice {
                                            let mapped = match reg {
                                                tc::regions::Region::Static => Some(tc::regions::Region::Static),
                                                tc::regions::Region::Param(k) => usize::try_from(k)
                                                    .ok()
                                                    .and_then(|ki| lt_pool.get(t_lo..t_lo + t_len)?.get(ki).copied().flatten()),
                                                tc::regions::Region::Late { .. } => None,
                                            };
                                            lt_pool.push(mapped);
                                        }
                                        stack.push((sup_key, next_lo, lt_pool.len() - next_lo));
                                        pushed_any = true;
                                    }
                                }
                            }
                            if !pushed_any {
                                stack.push((sup_key, 0, 0));
                            }
                        }
                    } else if recorded.scan_for(&t).is_none() {
                        recorded.push(t);
                        g.reads.push(copy_supertrait_read(t.0, None));
                    }
                }
                if found_copy {
                    g.param_copy.push(param);
                }
            }
        }
        g.param_copy.sort_unstable();
        g.param_copy.dedup();
        param_outlives.sort_unstable();
        param_outlives.dedup();
    }
    // Ensure callees, FnDef value items, selected impls, and supertrait heads BEFORE collecting
    // ADTs so every ADT named inside a signature, selected impl header/assoc value, or supertrait
    // clause gets its `AdtFacts` and variances (preventing `Count/Contract/AdtLifetimes`).
    let mut items: AVec<'a, u128> = AVec::new_in(alloc);
    for k in c.callees.column::<1>().iter() {
        items.push(k.0);
    }
    let has_value_callable = c.types.view().nodes().any(|n| matches!(n, tc::PNode::FnPtr(..) | tc::PNode::Dynamic(_)));
    if has_value_callable {
        for d in c.expr_defs.column::<1>().iter() {
            if let tc::Def::Static(k, _) | tc::Def::Const(k) = *d {
                items.push(k.0);
            }
        }
    }
    let mut opaques: AVec<'a, u128> = AVec::new_in(alloc);
    let mut traits: AVec<'a, u128> = AVec::new_in(alloc);
    for n in c.types.view().nodes() {
        match n {
            tc::PNode::FnDef(k, _) => items.push(k.0),
            tc::PNode::Opaque(k, _) => opaques.push(k.0),
            tc::PNode::ExTrait(k, _) | tc::PNode::ExProj { trait_: k, .. } | tc::PNode::Alias { trait_: k, .. } => traits.push(k.0),
            _ => {}
        }
    }
    items.push(owner.0);
    items.sort_unstable();
    items.dedup();
    opaques.sort_unstable();
    opaques.dedup();
    for &k in items.iter() {
        let key = tc::ItemKey(k);
        if !r.store.fns.contains_key(&key) && !r.store.ctors.contains_key(&key) {
            match r.keys.origin(key).cloned() {
                Some(Origin::Item(lib)) => {
                    match r.names.detail_of(lib) {
                        Some(blaze_surface::ItemDetail::Variant { .. }) => {
                            if let Some((parent, _)) = r.names.variant_of(lib) {
                                ensure_one(r, Want::Adt(parent));
                            }
                        }
                        Some(blaze_surface::ItemDetail::Struct | blaze_surface::ItemDetail::Enum | blaze_surface::ItemDetail::Union) => {
                            ensure_one(r, Want::Adt(lib));
                        }
                        _ => {
                            ensure_one(r, Want::Fn(lib));
                        }
                    }
                }
                Some(Origin::Member { container, kind, .. }) => {
                    if let Some(lib) = r.keys.library_in(r.names, container) {
                        match r.names.detail_of(lib) {
                            Some(blaze_surface::ItemDetail::Impl) => {
                                container_head(r, lib, false);
                            }
                            Some(blaze_surface::ItemDetail::Trait) => {
                                container_head(r, lib, true);
                            }
                            Some(blaze_surface::ItemDetail::Struct | blaze_surface::ItemDetail::Enum | blaze_surface::ItemDetail::Union)
                                if kind == super::keys::MemberKind::Ctor =>
                            {
                                ensure_one(r, Want::Adt(lib));
                            }
                            _ => {}
                        }
                    }
                    if let Some(w) = member_fn_want(r.names, &*r.keys, key) {
                        ensure_one(r, w);
                    }
                }
                None => {}
            }
        }
        if let Some(f) = r.store.fns.get(&key) {
            let tys = r.store.tys.view();
            let self_ref = f.self_param && f.inputs.first().is_some_and(|&t| matches!(tys.node(t), Ok(tc::PNode::Ref(..))));
            let callee = CalleeFacts { writes_lifetimes: f.qualifiers.writes_lifetimes(), self_ref, c_variadic: f.c_variadic };
            g.callees.push((k, callee));
        } else if r.store.ctors.contains_key(&key) {
            g.callees.push((k, CalleeFacts { writes_lifetimes: true, self_ref: false, c_variadic: false }));
        } else {
            g.reads.push(head_read(k, HeadAnswer::NotMade));
        }
    }
    let mut selected: AVec<'a, (u128, Option<tc::Name>)> = AVec::new_in(alloc);
    if let Some(Origin::Member { container, .. }) = r.keys.origin(owner).cloned()
        && let Some(ih) = r.store.impls.get(&container)
        && let Some(tr) = ih.trait_
    {
        let tys = r.store.tys.view();
        let (self_shape, tc_shape) = match tys.node(ih.self_ty) {
            Ok(tc::PNode::Adt(k, _)) => (Some(blaze_flow_lower::SelfShape::Adt(k)), Some(tc::Shape::Adt(k))),
            Ok(tc::PNode::Ref(m, _)) => (Some(blaze_flow_lower::SelfShape::Ref(m)), Some(tc::Shape::Ref(m))),
            Ok(tc::PNode::Tuple(l)) => {
                let n = tys.list(l).ok().and_then(|ks| u32::try_from(ks.len()).ok());
                (n.map(blaze_flow_lower::SelfShape::Tuple), n.map(tc::Shape::Tuple))
            }
            _ => (None, None),
        };
        if let (Some(self_shape), Some(tc_shape)) = (self_shape, tc_shape) {
            g.owner_trait_impls.push(blaze_flow_lower::OwnerTraitImpl { imp: container, trait_: tr, generics: ih.generics, self_shape });
            traits.push(tr.0);
            selected.push((container.0, None));
            // Also record unique impls of `tr`'s `Self`-supertraits for `SelfTy` (e.g.
            // `Cloned<I>: Iterator` inside `impl DoubleEndedIterator for Cloned<I>`).
            let mut work: AVec<'a, tc::ItemKey> = AVec::new_in(alloc);
            let mut seen: AVec<'a, tc::ItemKey> = AVec::new_in(alloc);
            work.push(tr);
            seen.push(tr);
            while let Some(cur) = work.pop() {
                if seen.len() >= 32 {
                    break;
                }
                if !r.store.trait_defs.contains_key(&cur)
                    && let Some(lib) = r.keys.library_in(r.names, cur)
                {
                    ensure_one(r, Want::Members(lib));
                }
                let mut sups: AVec<'a, tc::ItemKey> = AVec::new_in(alloc);
                if let Some(def) = r.store.trait_defs.get(&cur) {
                    let tys = r.store.tys.view();
                    for sc in def.supertraits.iter() {
                        if let tc::PClause::Trait { trait_: sup, args } = *sc
                            && let Ok(list) = tys.list(args)
                            && list.first().is_some_and(|&a| matches!(tys.node(a), Ok(tc::PNode::Param(0))))
                            && seen.scan_for(&sup).is_none()
                        {
                            seen.push(sup);
                            sups.push(sup);
                        }
                    }
                }
                for &sup in sups.iter() {
                    work.push(sup);
                    if let Some(impls) = answer_impls(r, sup, tc_shape)
                        && let &[imp] = &impls[..]
                        && let Some(lib) = r.keys.library_in(r.names, imp)
                        && container_head(r, lib, false)
                        && let Some(sup_ih) = r.store.impls.get(&imp)
                    {
                        g.owner_trait_impls.push(blaze_flow_lower::OwnerTraitImpl { imp, trait_: sup, generics: sup_ih.generics, self_shape });
                        traits.push(sup.0);
                        selected.push((imp.0, None));
                    }
                }
            }
        }
    }
    for p in c.selected.iter() {
        traits.push(p.trait_.0);
        if let tc::Selected::Impl { imp, .. } = p.selected {
            selected.push((imp.0, p.name));
        }
    }
    if let Some(p) = r.store.preds.get(&owner) {
        for cl in p.clauses.iter() {
            if let tc::PClause::Trait { trait_, .. } | tc::PClause::Projection { trait_, .. } = *cl {
                traits.push(trait_.0);
            }
        }
    }
    selected.sort_unstable_by_key(|&(k, n)| (k, n.is_some(), n.map_or(0, |n| n.0)));
    selected.dedup();
    let mut last_impl = None;
    for &(k, _) in selected.iter() {
        if last_impl == Some(k) {
            continue;
        }
        last_impl = Some(k);
        if let Some(lib) = r.keys.library_in(r.names, tc::ItemKey(k))
            && matches!(r.names.detail_of(lib), Some(blaze_surface::ItemDetail::Impl))
        {
            container_head(r, lib, false);
        }
    }
    for &k in items.iter() {
        if let Some(p) = r.store.preds.get(&tc::ItemKey(k)) {
            for cl in p.clauses.iter() {
                if let tc::PClause::Trait { trait_, .. } | tc::PClause::Projection { trait_, .. } = *cl {
                    traits.push(trait_.0);
                }
            }
        }
    }
    for &(k, _) in selected.iter() {
        if let Some(p) = r.store.preds.get(&tc::ItemKey(k)) {
            for cl in p.clauses.iter() {
                if let tc::PClause::Trait { trait_, .. } | tc::PClause::Projection { trait_, .. } = *cl {
                    traits.push(trait_.0);
                }
            }
        }
    }
    // Ensure trait heads transitively through supertraits, trait where-clauses, associated-type
    // item bounds, and nested alias traits so `regions.clauses` and `assoc_items` are loaded for
    // supertrait elaboration in `blaze-borrow`.
    {
        fn push_clause_traits(stys: tc::PTys<'_>, cl: &tc::PClause, traits: &mut AVec<'_, u128>) {
            let (args, extra_ty) = match *cl {
                tc::PClause::Trait { trait_: sup, args } => {
                    if traits.scan_for(&sup.0).is_none() {
                        traits.push(sup.0);
                    }
                    (Some(args), None)
                }
                tc::PClause::Projection { trait_: sup, args, term, .. } => {
                    if traits.scan_for(&sup.0).is_none() {
                        traits.push(sup.0);
                    }
                    (Some(args), Some(term))
                }
                _ => (None, None),
            };
            let mut st = [tc::PTy(0); 32];
            let mut len = 0usize;
            if let Some(args) = args
                && let Ok(list) = stys.list(args)
            {
                for &t in list {
                    if len < st.len() {
                        st[len] = t;
                        len += 1;
                    }
                }
            }
            if let Some(t) = extra_ty
                && len < st.len()
            {
                st[len] = t;
                len += 1;
            }
            let mut steps = 0u32;
            while len > 0 && steps < 64 {
                len -= 1;
                steps += 1;
                let Ok(node) = stys.node(st[len]) else { continue };
                match node {
                    tc::PNode::Alias { trait_: at, args: al, .. }
                    | tc::PNode::ExProj { trait_: at, args: al, .. }
                    | tc::PNode::ExTrait(at, al) => {
                        if traits.scan_for(&at.0).is_none() {
                            traits.push(at.0);
                        }
                        if let Ok(xs) = stys.list(al) {
                            for &x in xs {
                                if len < st.len() {
                                    st[len] = x;
                                    len += 1;
                                }
                            }
                        }
                    }
                    tc::PNode::Adt(_, al)
                    | tc::PNode::Tuple(al)
                    | tc::PNode::Dynamic(al)
                    | tc::PNode::Opaque(_, al) => {
                        if let Ok(xs) = stys.list(al) {
                            for &x in xs {
                                if len < st.len() {
                                    st[len] = x;
                                    len += 1;
                                }
                            }
                        }
                    }
                    tc::PNode::Ref(_, x)
                    | tc::PNode::Ptr(_, x)
                    | tc::PNode::Slice(x)
                    | tc::PNode::Array(x, _) => {
                        if len < st.len() {
                            st[len] = x;
                            len += 1;
                        }
                    }
                    _ => {}
                }
            }
        }

        let mut ti = 0usize;
        traits.sort_unstable();
        traits.dedup();
        while ti < traits.len() && traits.len() < 1024 {
            let tk = tc::ItemKey(traits[ti]);
            ti += 1;
            if let Some(lib) = r.keys.library_in(r.names, tk)
                && matches!(r.names.detail_of(lib), Some(blaze_surface::ItemDetail::Trait))
            {
                container_head(r, lib, true);
                r.store.ensure_assoc_regions(r.names, r.keys, lib);
            }
            let stys = r.store.tys.view();
            if let Some(p) = r.store.preds.get(&tk) {
                for cl in p.clauses.iter() {
                    push_clause_traits(stys, cl, &mut traits);
                }
            }
            if let Some(def) = r.store.trait_defs.get(&tk) {
                for sc in def.supertraits.iter() {
                    push_clause_traits(stys, sc, &mut traits);
                }
                for a in def.assoc.iter() {
                    g.assoc_items.push((tk.0, a.name, a.item));
                    if let Some(ap) = r.store.preds.get(&a.item) {
                        for ac in ap.clauses.iter() {
                            push_clause_traits(stys, ac, &mut traits);
                        }
                    }
                }
            }
        }
        traits.sort_unstable();
        traits.dedup();
        g.assoc_items.sort_unstable_by_key(|&(k, n, _)| (k, n));
        g.assoc_items.dedup_by_key(|&mut (k, n, _)| (k, n));
    }
    // Check which `PNode::Alias` types in this body are bounded by `Copy` (either via their
    // associated type item's own trait bounds `type At<'a>: Copy` / supertrait of `Copy`, or
    // via the body owner's `where` clauses `M::At<'_>: Copy`).
    if let Some(copy_tr) = langs.copy {
        let mut visited: AVec<'a, tc::ItemKey> = AVec::new_in(alloc);
        let mut stack: AVec<'a, tc::ItemKey> = AVec::new_in(alloc);
        let mut direct: AVec<'a, u128> = AVec::new_in(alloc);
        let mut self_outlives: AVec<'a, tc::regions::Region> = AVec::new_in(alloc);
        let mut recorded: AVec<'a, tc::ItemKey> = AVec::new_in(alloc);
        let mut assoc_copy_cache: AVec<'a, (u128, bool)> = AVec::new_in(alloc);
        let mut reaches_copy = |r: &mut Records<'_, 'n, S>, start: tc::ItemKey, reads: &mut AVec<'a, Read>| -> bool {
            visited.clear();
            stack.clear();
            stack.push(start);
            while let Some(t) = stack.pop() {
                if t == copy_tr {
                    return true;
                }
                if visited.scan_for(&t).is_some() || visited.len() >= 64 {
                    continue;
                }
                visited.push(t);
                if !r.store.trait_defs.contains_key(&t)
                    && let Some(lib) = r.keys.library_in(r.names, t)
                {
                    ensure_one(r, Want::Members(lib));
                }
                if let Some(def) = r.store.trait_defs.get(&t) {
                    let tys = r.store.tys.view();
                    trait_self_supers(def, tys, &mut direct, &mut self_outlives);
                    if recorded.scan_for(&t).is_none() {
                        recorded.push(t);
                        reads.push(copy_supertrait_read(t.0, Some((&direct, &self_outlives))));
                    }
                    for &sup in direct.iter() {
                        stack.push(tc::ItemKey(sup));
                    }
                } else if recorded.scan_for(&t).is_none() {
                    recorded.push(t);
                    reads.push(copy_supertrait_read(t.0, None));
                }
            }
            false
        };
        let body_tys = c.types.view();
        for (idx, n) in body_tys.nodes().enumerate() {
            let tc::PNode::Alias { trait_, name, .. } = *n else { continue };
            let mut is_copy = false;
            if let Ok(pos) = g.assoc_items.binary_search_by_key(&(trait_.0, name), |&(k, nm, _)| (k, nm))
                && let Some(&(_, _, a_item)) = g.assoc_items.get(pos)
            {
                if let Some(&(_, cached)) = assoc_copy_cache.iter().find(|&&(k, _)| k == a_item.0) {
                    is_copy = cached;
                } else {
                    let mut sups: AVec<'a, tc::ItemKey> = AVec::new_in(alloc);
                    if let Some(ap) = r.store.preds.get(&a_item) {
                        let stys = r.store.tys.view();
                        for ac in ap.clauses.iter() {
                            if let tc::PClause::Trait { trait_: sup, args } = *ac
                                && let Ok(list) = stys.list(args)
                                && let Some(&first) = list.first()
                                && let Ok(tc::PNode::Alias { trait_: at, name: an, .. }) = stys.node(first)
                                && at == trait_
                                && an == name
                            {
                                sups.push(sup);
                            }
                        }
                    }
                    let mut found = false;
                    for &sup in sups.iter() {
                        if reaches_copy(r, sup, &mut g.reads) {
                            found = true;
                            break;
                        }
                    }
                    assoc_copy_cache.push((a_item.0, found));
                    g.reads.push(assoc_copy_read(a_item.0, found));
                    is_copy = found;
                }
            }
            if !is_copy
                && let Ok(pty_id) = u32::try_from(idx)
            {
                let mut owner_sups: AVec<'a, tc::ItemKey> = AVec::new_in(alloc);
                if let Some(op) = r.store.preds.get(&owner) {
                    let stys = r.store.tys.view();
                    for oc in op.clauses.iter() {
                        if let tc::PClause::Trait { trait_: sup, args } = *oc
                            && let Ok(list) = stys.list(args)
                            && let Some(&first) = list.first()
                            && pty_eq_across(stys, first, body_tys, tc::PTy(pty_id))
                        {
                            owner_sups.push(sup);
                        }
                    }
                }
                for &sup in owner_sups.iter() {
                    if reaches_copy(r, sup, &mut g.reads) {
                        is_copy = true;
                        break;
                    }
                }
            }
            if is_copy && let Ok(pty_id) = u32::try_from(idx) {
                g.alias_copy.push(pty_id);
            }
        }
        g.alias_copy.sort_unstable();
        g.alias_copy.dedup();
    }
    // Gather RPITIT synthetic metadata (`rpitit_opaques`, overriding `impl_fns`, and `Twin`
    // default opaque regions) before ADT/read collection so every overriding impl fn and Twin
    // opaque participates in ADT variance/outlives, read tracking (Rule 21), and signature facts.
    {
        let mut extra_opaques: AVec<'a, u128> = AVec::new_in(alloc);
        for &ok_u128 in opaques.iter() {
            if let Some(twin) = r.store.rpitit_defaults.get(&tc::ItemKey(ok_u128)) {
                extra_opaques.push(twin.source.0);
            }
        }
        for &tk_u128 in traits.iter() {
            let tk = tc::ItemKey(tk_u128);
            ensure_rpitit_trait_rows(r, tk);
            let Some(tr_rows) = r.store.rpitit_traits.get(&tk) else { continue };
            let Some(trait_types) = r.keys.library_in(r.names, tk).and_then(|lib| r.store.maps.traits.get(&lib)).map(|th| th.generics) else {
                continue;
            };
            for s in tr_rows.rows.iter() {
                let default_twin = s.default.and_then(|d| match r.store.tys.view().node(d) {
                    Ok(tc::PNode::Opaque(k, _)) => Some(k),
                    _ => None,
                });
                g.rpitit_opaques.push((
                    s.opaque.0,
                    blaze_flow_lower::RpititOpaque {
                        trait_: tk,
                        name: s.name,
                        fn_name: s.fn_name,
                        trait_fn: s.trait_fn,
                        trait_types,
                        default_twin,
                    },
                ));
                extra_opaques.push(s.opaque.0);
                if let Some(tw) = default_twin {
                    extra_opaques.push(tw.0);
                }
            }
        }
        let mut last_imp = None;
        let mut new_impl_fns: AVec<'a, tc::ItemKey> = AVec::new_in(alloc);
        for &(imp_u128, _) in selected.iter() {
            if last_imp == Some(imp_u128) {
                continue;
            }
            last_imp = Some(imp_u128);
            let imp_k = tc::ItemKey(imp_u128);
            let Some(tr) = r.store.impls.get(&imp_k).and_then(|ih| ih.trait_) else { continue };
            ensure_rpitit_trait_rows(r, tr);
            let Some(ih) = r.store.impls.get(&imp_k) else { continue };
            let Some(tr_rows) = r.store.rpitit_traits.get(&tr) else { continue };
            for s in tr_rows.rows.iter() {
                if let Some(&(_, impl_fn)) = ih.fns.iter().find(|&&(n, _)| n == s.fn_name) {
                    g.impl_fns.push((imp_u128, s.fn_name, impl_fn));
                    if items.scan_for(&impl_fn.0).is_none() && new_impl_fns.scan_for(&impl_fn).is_none() {
                        new_impl_fns.push(impl_fn);
                    }
                }
            }
        }
        for &impl_fn in new_impl_fns.iter() {
            if !r.store.fns.contains_key(&impl_fn)
                && let Some(w) = member_fn_want(r.names, &*r.keys, impl_fn)
            {
                ensure_one(r, w);
            }
            items.push(impl_fn.0);
            if let Some(f) = r.store.fns.get(&impl_fn) {
                let tys = r.store.tys.view();
                let self_ref = f.self_param && f.inputs.first().is_some_and(|&t| matches!(tys.node(t), Ok(tc::PNode::Ref(..))));
                g.callees.push((impl_fn.0, CalleeFacts { writes_lifetimes: f.qualifiers.writes_lifetimes(), self_ref, c_variadic: f.c_variadic }));
                let out = r.store.fn_output(impl_fn).unwrap_or(f.output);
                let mut st: AVec<'a, tc::PTy> = AVec::new_in(alloc);
                st.push(out);
                let mut steps = 0u32;
                while let Some(t) = st.pop() {
                    steps += 1;
                    if steps > 1024 {
                        break;
                    }
                    let Ok(node) = tys.node(t) else { continue };
                    if let tc::PNode::Opaque(ok, _) = node {
                        extra_opaques.push(ok.0);
                    }
                    if let Ok((list, last)) = super::adt_variance::children(tys, t, node) {
                        st.extend_from_slice(list);
                        if let Some(ch) = last {
                            st.push(ch);
                        }
                    }
                }
            } else {
                g.reads.push(head_read(impl_fn.0, HeadAnswer::NotMade));
            }
        }
        opaques.extend_from_slice(&extra_opaques);
        opaques.sort_unstable();
        opaques.dedup();
        items.sort_unstable();
        items.dedup();
        g.callees.sort_unstable_by_key(|&(k, _)| k);
        g.callees.dedup_by_key(|&mut (k, _)| k);
        g.rpitit_opaques.sort_unstable_by_key(|&(k, _)| k);
        g.rpitit_opaques.dedup_by_key(|&mut (k, _)| k);
        g.impl_fns.sort_unstable_by_key(|&(imp, n, _)| (imp, n));
        g.impl_fns.dedup_by_key(|&mut (imp, n, _)| (imp, n));
        for &ok_u128 in opaques.iter() {
            let ok = tc::ItemKey(ok_u128);
            if r.store.rpitit_defaults.contains_key(&ok) && r.store.regions.made(ok).is_none() {
                ensure_twin_opaque_regions(r, ok);
            }
        }
    }
    let mut adts: AVec<'a, u128> = AVec::new_in(alloc);
    for n in c.types.view().nodes() {
        if let tc::PNode::Adt(k, _) = n {
            adts.push(k.0);
        }
    }
    // Also collect every ADT mentioned in selected impl headers/assoc values/where-clauses,
    // callee signatures/clauses, opaque clauses, and trait supertrait clauses.
    {
        let reader = r.store.regions.reader(alloc);
        let tys = r.store.tys.view();
        let mut walk_ty = |root: tc::PTy| {
            let mut st: AVec<'a, tc::PTy> = AVec::new_in(alloc);
            st.push(root);
            let mut steps = 0u32;
            while let Some(t) = st.pop() {
                steps += 1;
                if steps > 4096 {
                    break;
                }
                let Ok(node) = tys.node(t) else { continue };
                if let tc::PNode::Adt(a, _) = node {
                    adts.push(a.0);
                }
                if let Ok((list, last)) = super::adt_variance::children(tys, t, node) {
                    st.extend_from_slice(list);
                    if let Some(c) = last {
                        st.push(c);
                    }
                }
            }
        };
        for &(k, name) in selected.iter() {
            let ik = tc::ItemKey(k);
            if let Ok((self_col, trait_col)) = reader.impl_header(ik) {
                walk_ty(self_col.ty());
                if let Some(tc) = trait_col {
                    walk_ty(tc.ty());
                }
            }
            if let Some(n) = name
                && let Ok(v) = reader.impl_assoc(ik, n)
            {
                walk_ty(v.ty());
            }
            if let Some(ih) = r.store.impls.get(&ik) {
                for &(an, _) in ih.assoc.iter() {
                    if let Ok(v) = reader.impl_assoc(ik, an) {
                        walk_ty(v.ty());
                    }
                }
            }
            if let Ok(cls) = reader.clauses(ik) {
                for cv in cls {
                    walk_ty(cv.col.ty());
                }
            }
        }
        for &k in items.iter() {
            let ik = tc::ItemKey(k);
            if let Ok(sig) = reader.sig(ik) {
                walk_ty(sig.col.ty());
            }
            if let Ok(cls) = reader.clauses(ik) {
                for cv in cls {
                    walk_ty(cv.col.ty());
                }
            }
        }
        for &k in opaques.iter() {
            if let Ok(cls) = reader.clauses(tc::ItemKey(k)) {
                for cv in cls {
                    walk_ty(cv.col.ty());
                }
            }
        }
        for &k in traits.iter() {
            if let Ok(cls) = reader.clauses(tc::ItemKey(k)) {
                for cv in cls {
                    walk_ty(cv.col.ty());
                }
            }
        }
        for &(_, _, a_item) in g.assoc_items.iter() {
            if let Ok(cls) = reader.clauses(a_item) {
                for cv in cls {
                    walk_ty(cv.col.ty());
                }
            }
        }
    }
    adts.sort_unstable();
    adts.dedup();
    // The types the owner assumes well-formed (RFC 1214): its signature's and its impl's
    // header's, laid now; the ADTs they name join the roots of the ADT facts.
    let implied_in = owner_implied_input(r, owner, alloc);
    let mut roots: AVec<'a, u128> = AVec::with_capacity_in(adts.len(), alloc);
    roots.extend_from_slice(&adts);
    roots.extend(implied_in.adts());
    roots.sort_unstable();
    roots.dedup();
    // The variance of every parameter of every ADT of the body's types, and of the ADTs their
    // fields name, and their outlives requirements (adt_facts.rs, D190 point 2): one fixed
    // point per group, before the rows below take each answer. Each ADT it read is a row of
    // the body's read list (rule 21).
    let adt_facts = match plant {
        FactsPlant::NoVariances => None,
        _ => Some(adt_facts_of(r, &roots, alloc)),
    };
    if let Some(t) = &adt_facts {
        for (k, input) in t.reads() {
            g.reads.push(Read {
                fact: super::reads::Fact { kind: super::reads::FactKind::BorrowAdt, key: k, detail: super::adt_facts::ADT_FACTS_DETAIL },
                answer: Some(input),
            });
        }
        // The owner's implied bounds, from the same table; what it read of the owner and its
        // impl is a row each.
        g.implied_state = Some(implied_in.solve(t, &mut g.implied));
        for (k, digest) in implied_in.reads() {
            g.reads.push(Read {
                fact: super::reads::Fact { kind: super::reads::FactKind::BorrowHead, key: k, detail: super::adt_facts::IMPLIED_DETAIL },
                answer: Some(digest),
            });
        }
    }
    let mut alias_guard: Option<fields::FieldAliasGuard<'a>> = None;
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
        let Some(row) = BodyHeadFacts::new(&*r.store, r.names).adt(key) else {
            g.unmade.push((k, unmade));
            g.reads.push(adt_read(k, AdtAnswer::NotMade));
            continue;
        };
        let kind = row.kind;
        let lifetimes = row.lifetimes;
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
        let regions = row.field_regions.clone();
        let has_alias_field = plant != FactsPlant::NoFieldRegions
            && fields::adt_has_alias_field(r.store.tys.view(), &row.fields);
        let field_count = row.fields.len();
        let variant_count = row.variants.len();
        let copy_state = copy_impl_state(r, langs.copy, key, alloc);
        // A `Copy` type has no `Drop` impl: the Reference, "Special types and traits", `Copy`:
        // "Copy cannot be implemented for types that implement Drop" (rustc E0184), so YES to
        // `Copy` is NO to `Drop` without asking (an unread candidate cannot be its `Drop`).
        // Likewise, in a `#![no_core]` crate graph with no `#[lang = "drop"]` trait at all, no ADT
        // can implement `Drop`.
        let drop_impl = match copy_state {
            Ok(CopyImpl::Positive) => Ok(false),
            _ if langs.drop.is_none() => Ok(false),
            _ => has_impl(r, langs.drop, key),
        };
        if let Err(why) = copy_state {
            g.unknown.push((k, "Copy"));
            g.unknown_why.push(why);
        }
        if let Err(why) = drop_impl {
            g.unknown.push((k, "Drop"));
            g.unknown_why.push(why);
        }
        let no_drop = matches!(drop_impl, Ok(false) | Err(ImplsUnknown::NoLangItem));
        let (copy, copy_need) = match copy_state {
            Ok(CopyImpl::Positive) => {
                let copy = Some(true);
                let copy_need = match plant {
                    FactsPlant::NoCopyArgs => None,
                    _ => fields::copy_args_of(r, langs, key, copy, &mut g.copy_need, alloc),
                };
                (copy, copy_need)
            }
            Ok(CopyImpl::None)
                if no_drop
                    && r.store.is_auto_copy_pub(r.names, r.keys, key)
                    && !matches!(copy_impl_state(r, langs.clone, key, alloc), Ok(CopyImpl::Negative)) =>
            {
                match auto_copy_of(r, langs, key, &mut g.copy_need, alloc) {
                    Some((true, mask)) => {
                        let copy_need = match plant {
                            FactsPlant::NoCopyArgs => None,
                            _ => mask,
                        };
                        (Some(true), copy_need)
                    }
                    _ => (Some(false), None),
                }
            }
            Ok(CopyImpl::None | CopyImpl::Negative) => (Some(false), None),
            Err(_) => (None, None),
        };
        let drop_impl = drop_impl.ok();
        if let Some((start, len)) = copy_need {
            g.copy_args.push((k, start, len));
        }
        // The variance of each lifetime parameter (blaze-flow-lower `Facts::adt_variances`);
        // ADTs are taken in key order, so `variance_at` stays sorted. What was taken is this
        // ADT's row too (rule 21).
        let variances = match &adt_facts {
            None => None,
            Some(t) => adt_variances_in(t, k, &mut g.variance_list),
        };
        if let Some((lo, hi)) = variances {
            g.variance_at.push((k, lo, hi));
        }
        let copy_taken = copy.unwrap_or(false);
        let adt_uncond_copy = copy_need.is_some_and(|(start, len)| {
            let s = start as usize;
            let e = s.saturating_add(len as usize);
            g.copy_need.get(s..e).is_some_and(|need| need.iter().all(|&b| !b))
        });
        let drop_summary = fields::drop_facts_of(
            r,
            langs,
            key,
            drop_impl,
            adt_uncond_copy,
            &mut g.drop_need,
            &mut g.drop_strict_need,
            &mut g.drop_lt_need,
            &mut g.reads,
            alloc,
        );
        let needs_drop = drop_summary.needs_drop;
        let drop_need = drop_summary.drop_args;
        let drop_strict_span = drop_summary.drop_strict;
        let drop_lt_span = drop_summary.drop_lifetimes;
        if let Some((start, len)) = drop_need {
            g.drop_args.push((k, start, len));
        }
        if let Some((start, len)) = drop_strict_span {
            g.drop_strict.push((k, start, len));
        }
        if let Some((start, len)) = drop_lt_span {
            g.drop_lifetimes.push((k, start, len));
        }
        let deref_impl = if let Some(deref_tr) = langs.deref
            && let Some(impls) = answer_impls(r, deref_tr, tc::Shape::Adt(key))
            && let &[imp] = &impls[..]
        {
            g.deref_impls.push((k, imp));
            Some(imp.0)
        } else {
            None
        };
        let facts = AdtFacts {
            copy: copy_taken,
            drop_impl: drop_impl.unwrap_or(false),
            needs_drop,
            lifetimes,
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
        let copy_args = copy_need.and_then(|(start, len)| {
            let s = usize::try_from(start).ok()?;
            let e = s.checked_add(usize::try_from(len).ok()?)?;
            g.copy_need.get(s..e)
        });
        let drop_args = drop_need.and_then(|(start, len)| {
            let s = usize::try_from(start).ok()?;
            let e = s.checked_add(usize::try_from(len).ok()?)?;
            g.drop_need.get(s..e)
        });
        let drop_strict = drop_strict_span.and_then(|(start, len)| {
            let s = usize::try_from(start).ok()?;
            let e = s.checked_add(usize::try_from(len).ok()?)?;
            g.drop_strict_need.get(s..e)
        });
        let drop_lifetimes = drop_lt_span.and_then(|(start, len)| {
            let s = usize::try_from(start).ok()?;
            let e = s.checked_add(usize::try_from(len).ok()?)?;
            g.drop_lt_need.get(s..e)
        });
        let variances = variances.and_then(|(lo, hi)| {
            let s = usize::try_from(lo).ok()?;
            let e = usize::try_from(hi).ok()?;
            g.variance_list.get(s..e)
        });
        let suppressed = if has_alias_field {
            let guard = alias_guard.get_or_insert_with(|| fields::FieldAliasGuard::new_in(alloc));
            guard.check_adt(r, owner, c.types.view(), key, &mut g.suppressed_field_regions)
        } else {
            None
        };
        let mut absent = AVec::new_in(alloc);
        let spans = if plant == FactsPlant::NoFieldRegions {
            absent.resize(field_count, NO_ROW);
            &absent[..]
        } else if let Some((s, len)) = suppressed {
            absent.extend_from_slice(&regions.at[..]);
            if let Some(sl) = g.suppressed_field_regions.get(s as usize..s as usize + len as usize) {
                for &fi in sl {
                    if let Some(slot) = absent.get_mut(fi as usize) {
                        *slot = NO_ROW;
                    }
                }
            }
            &absent[..]
        } else {
            &regions.at[..]
        };
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
                field_regions: spans,
                region_list: &regions.list,
                copy_args,
                drop_args,
                drop_strict,
                drop_lifetimes,
                deref_impl,
                variances,
            },
        );
        g.reads.push(read);
        if plant != FactsPlant::NoFieldRegions {
            g.field_regions.insert(
                key,
                RegionFields { first_variant: variants_from, variant_count, first_field: fields_from, regions, suppressed },
            );
        }
    }
    if let Some(guard) = alias_guard {
        guard.push_reads(&mut g.reads);
    }
    // Evaluate instantiated structural drop glue for concrete body `PNode::Adt` types whose
    // uninstantiated ADT summary had `needs_drop && !drop_impl` (e.g. `Flatten<...>`,
    // `Split<'_, &str>`, `SplitWhitespace<'_>` whose fields hold `PNode::Alias`).
    {
        let body_tys = c.types.view();
        let mut eval: Option<fields::InstDropEval<'a>> = None;
        for (idx, node) in body_tys.nodes().enumerate() {
            let tc::PNode::Adt(k, _) = *node else { continue };
            let Ok(at) = g.adts.binary_search_by_key(&k.0, |&(ak, _)| ak) else { continue };
            let af = g.adts[at].1;
            if !af.needs_drop || af.drop_impl {
                continue;
            }
            let Ok(pty_id) = u32::try_from(idx) else { continue };
            let ev = eval.get_or_insert_with(|| fields::InstDropEval::new_in(alloc));
            if let Some(needs) = ev.eval_body_ty(r, langs, body_tys, tc::PTy(pty_id)) {
                g.inst_drop.push((pty_id, needs));
            }
        }
        if let Some(ev) = eval {
            ev.push_reads(&mut g.reads);
        }
        g.inst_drop.sort_unstable_by_key(|&(k, _)| k);
    }
    let target_name = tc::Name::of("Target");
    let prev_selected_len = selected.len();
    for &(_, imp) in g.deref_impls.iter() {
        selected.push((imp.0, Some(target_name)));
    }
    if selected.len() != prev_selected_len {
        selected.sort_unstable_by_key(|&(k, n)| (k, n.is_some(), n.map_or(0, |n| n.0)));
        selected.dedup();
        let mut last_impl = None;
        for &(k, _) in selected.iter() {
            if last_impl == Some(k) {
                continue;
            }
            last_impl = Some(k);
            if let Some(lib) = r.keys.library_in(r.names, tc::ItemKey(k))
                && matches!(r.names.detail_of(lib), Some(blaze_surface::ItemDetail::Impl))
            {
                container_head(r, lib, false);
            }
        }
    }
    // From here the records are only read. Each head's regions (D148) are its head record's,
    // read in place through ONE reader (head_record.rs `RegionReader` over `Store::regions`):
    // no fn's text is read here, and a head whose regions were not made (no record, the head
    // refused, an old form) is named by the lowering at the call that needs them
    // (`Form::RegionsUnread`), never related under a guess.
    {
        let regions = r.store.regions.reader(alloc);
        // Each head's row (rule 21): its callee facts and its signature column as read, or
        // why it was not, so a lifetime-only edit of a callee's signature moves the body's
        // read list (the regions digest moves with it: head_record.rs `regions_digest`).
        for &(k, callee) in g.callees.iter() {
            let sig = regions.sig(tc::ItemKey(k)).map(SigLife::new);
            g.reads.push(head_read(k, HeadAnswer::Made { callee, sig }));
        }
        // Each selected impl's row (rule 21): its regions as read (header, lifetimes and the
        // value of each associated type read), so `type A = Inner<'a>` edited to
        // `Inner<'static>` rechecks the body; the type check's reads hold only its types.
        let mut names: AVec<'a, tc::Name> = AVec::new_in(alloc);
        let mut i = 0;
        while i < selected.len() {
            let imp = selected[i].0;
            names.clear();
            while i < selected.len() && selected[i].0 == imp {
                if let Some(n) = selected[i].1 {
                    names.push(n);
                }
                i += 1;
            }
            if let Some(ih) = r.store.impls.get(&tc::ItemKey(imp)) {
                for &(an, _) in ih.assoc.iter() {
                    if names.scan_for(&an).is_none() {
                        names.push(an);
                    }
                }
                names.sort_unstable_by_key(|n| n.0);
            }
            g.reads.push(impl_read(imp, &names, &regions));
        }
        for &k in traits.iter() {
            g.reads.push(clauses_read(k, &regions));
        }
        for &k in opaques.iter() {
            g.reads.push(clauses_read(k, &regions));
        }
        for &(_, _, a_item) in g.assoc_items.iter() {
            g.reads.push(clauses_read(a_item.0, &regions));
        }
        // The generic shape of each head whose signature regions are filed: where its type
        // parameters stand among its positions (blaze-flow-lower generic_call.rs), counted by
        // the one region counter. A head whose shape cannot be lined up has none, kept with why
        // (rule 3): the note of a call refused for it says why.
        for &k in items.iter() {
            let key = tc::ItemKey(k);
            if regions.sig(key).is_err() {
                continue;
            }
            let Some(f) = r.store.fns.get(&key) else {
                // Kept with why (rule 19): a call refused for it names it.
                g.shape_refused.push((k, blaze_flow_lower::ShapeRefusal::NoHead));
                continue;
            };
            // The one region counter's facts (D148) for its generic shape: each declared count
            // the names' (D155: no ADT head is imported to count it), an opaque's captures as
            // the head record filed them (the signature column's own count, head_record.rs
            // `RegionReader::opaque_captures`), never the in-process maps ([`ShapeArity`]).
            let arity = ShapeArity { declared: super::types::HeadsArity { heads: &r.store.maps, opaques: &[], names: r.names }, regions: &regions };
            let mut shape = blaze_flow_lower::ShapeBuf::new_in(alloc);
            let out = r.store.fn_output(key).unwrap_or(f.output);
            match blaze_flow_lower::read_shape(r.store.tys.view(), &f.inputs[..], out, &arity, &mut shape, alloc) {
                Ok(()) => g.shapes.push((k, shape)),
                Err(why) => g.shape_refused.push((k, why)),
            }
        }
        // Each head's declared outlives and trait-lifetime ties, from its clauses
        // (`Preds::clauses`, own and inherited) and its head record's regions (front2
        // call_generics.rs, borrows_ties.rs; piece L4), and their read rows (rule 21).
        let future = r.index.lang("future_trait");
        for &k in items.iter() {
            let key = tc::ItemKey(k);
            let head = r.store.fns.get(&key);
            let clauses = r.store.preds.get(&key).map(|p| &p.clauses[..]);
            let extra_outlives: &[(u32, tc::regions::Region)] = if k == owner.0 { &param_outlives } else { &[] };
            g.calls.gather(k, head, clauses, r.store.tys.view(), &regions, future, extra_outlives, alloc);
            g.ties.gather(k, head, clauses, r.store.tys.view(), &regions);
        }
        // Each trait object's principal: whether it declares a lifetime decides how an unsized
        // coercion to it is related (blaze-flow-lower `Lower::unsize_to_object`). Its count is
        // the trait's head record's (`RegionFacts::lifetime_params`), its row the ties' one
        // trait row (borrows_ties.rs `trait_count`, rule 21); unread is `None`, never a guess.
        for n in c.types.view().nodes() {
            if let tc::PNode::ExTrait(t, _) = n {
                g.object_traits.push((t.0, None));
            }
        }
        g.object_traits.sort_unstable_by_key(|&(k, _)| k);
        g.object_traits.dedup_by_key(|&mut (k, _)| k);
        for o in g.object_traits.iter_mut() {
            o.1 = g.ties.trait_count(tc::ItemKey(o.0), &regions).map(|n| n > 0);
        }
        // Each trait reference and alias of the body's types and the heads' columns, counted
        // by the one region counter (D148, D155) for the one call rule (blaze-flow-lower
        // one_call.rs): unknown is `None`, which that rule refuses by `Count`, never 0.
        // An alias's trait is asked too (one_call.rs `impl_at` reads `trait_lifetimes` beside
        // `alias_lifetimes`), and so is every row the type check selected: a trait need not
        // stand as a trait reference in any table (`<I as IntoIterator>::IntoIter` and
        // `<I as Iterator>::Item` of a `for` loop name `IntoIterator` and `Iterator` only as
        // an alias's trait), and an uncounted trait made every such call `HeadUnread`.
        // A projection bound (`ExProj`: `F: Future<Output = T>`, `I: IntoIterator<Item = X>`,
        // in a clause column or a `dyn`) is counted as its alias (blaze-typeck regions.rs
        // `region_arity`: `alias_lifetimes(trait_, name)`), so it is filed as one; and every
        // projection and trait clause of the owner's and each gathered head's where-clauses
        // (`Preds::clauses`: the contract the one rule lays as `ExTrait`/`ExProj` nodes,
        // blaze-flow-lower one_call.rs `clause_of`), whether or not such a node was interned
        // yet. The census (wave-1119): 54,907 `Count` refusals named an alias's lifetimes "not
        // filed". The count itself is the one read below (rule 21).
        for n in c.types.view().nodes() {
            match n {
                tc::PNode::ExTrait(t, _) => g.trait_counts.push((t.0, None)),
                tc::PNode::Alias { trait_, name, .. } | tc::PNode::ExProj { trait_, name, .. } => {
                    g.alias_counts.push((trait_.0, *name, None));
                    g.trait_counts.push((trait_.0, None));
                }
                _ => {}
            }
        }
        // Walk only the `PTy`s in `r.store.tys` reachable from this body's gathered heads
        // (`items`, `owner`'s container impl, `selected`, `opaques`, `traits`, `assoc_items`,
        // and `roots` ADTs), never scanning unrelated heads in `r.store.tys`.
        {
            struct TyWalk<'a, 't> {
                stys: tc::PTys<'t>,
                seen: blaze_hash::Map<u32, (), blaze_hash::FixedState, Alloc<'a>>,
                st: AVec<'a, tc::PTy>,
            }
            impl<'a> TyWalk<'a, '_> {
                fn walk_ty(&mut self, root: tc::PTy, trait_counts: &mut AVec<'a, (u128, Option<u32>)>, alias_counts: &mut AVec<'a, (u128, tc::Name, Option<u32>)>) {
                    if self.seen.insert(root.0, ()).is_some() {
                        return;
                    }
                    self.st.push(root);
                    while let Some(t) = self.st.pop() {
                        let Ok(node) = self.stys.node(t) else { continue };
                        match node {
                            tc::PNode::ExTrait(tr, _) => trait_counts.push((tr.0, None)),
                            tc::PNode::Alias { trait_, name, .. } | tc::PNode::ExProj { trait_, name, .. } => {
                                alias_counts.push((trait_.0, name, None));
                                trait_counts.push((trait_.0, None));
                            }
                            _ => {}
                        }
                        if let Ok((list, last)) = super::adt_variance::children(self.stys, t, node) {
                            for &ch in list {
                                if self.seen.insert(ch.0, ()).is_none() {
                                    self.st.push(ch);
                                }
                            }
                            if let Some(ch) = last
                                && self.seen.insert(ch.0, ()).is_none()
                            {
                                self.st.push(ch);
                            }
                        }
                    }
                }
                fn walk_list(&mut self, list: tc::List, trait_counts: &mut AVec<'a, (u128, Option<u32>)>, alias_counts: &mut AVec<'a, (u128, tc::Name, Option<u32>)>) {
                    if let Ok(items) = self.stys.list(list) {
                        for &t in items {
                            self.walk_ty(t, trait_counts, alias_counts);
                        }
                    }
                }
                fn walk_clause(&mut self, cl: &tc::PClause, trait_counts: &mut AVec<'a, (u128, Option<u32>)>, alias_counts: &mut AVec<'a, (u128, tc::Name, Option<u32>)>) {
                    match *cl {
                        tc::PClause::Trait { trait_, args } => {
                            trait_counts.push((trait_.0, None));
                            self.walk_list(args, trait_counts, alias_counts);
                        }
                        tc::PClause::ForFields { container, trait_, args } => {
                            trait_counts.push((trait_.0, None));
                            self.walk_ty(container, trait_counts, alias_counts);
                            self.walk_list(args, trait_counts, alias_counts);
                        }
                        tc::PClause::Projection { trait_, args, name, term } => {
                            alias_counts.push((trait_.0, name, None));
                            trait_counts.push((trait_.0, None));
                            self.walk_list(args, trait_counts, alias_counts);
                            self.walk_ty(term, trait_counts, alias_counts);
                        }
                        tc::PClause::Const { args, .. } => self.walk_list(args, trait_counts, alias_counts),
                        tc::PClause::TypeOutlives { ty, .. } => self.walk_ty(ty, trait_counts, alias_counts),
                        tc::PClause::RegionOutlives { .. } => {}
                    }
                }
            }
            let mut walker = TyWalk {
                stys: r.store.tys.view(),
                seen: blaze_hash::Map::new(blaze_hash::MapOpts { hasher: blaze_hash::FixedState, alloc, capacity: None }),
                st: AVec::new_in(alloc),
            };
            for &k in items.iter() {
                let ik = tc::ItemKey(k);
                if let Ok(sig) = regions.sig(ik) {
                    walker.walk_ty(sig.col.ty(), &mut g.trait_counts, &mut g.alias_counts);
                }
                if let Ok(cls) = regions.clauses(ik) {
                    for cv in cls {
                        walker.walk_ty(cv.col.ty(), &mut g.trait_counts, &mut g.alias_counts);
                    }
                }
                if let Some(f) = r.store.fns.get(&ik) {
                    for &t in f.inputs.iter() {
                        walker.walk_ty(t, &mut g.trait_counts, &mut g.alias_counts);
                    }
                    walker.walk_ty(r.store.fn_output(ik).unwrap_or(f.output), &mut g.trait_counts, &mut g.alias_counts);
                }
                if let Some(p) = r.store.preds.get(&ik) {
                    for cl in p.clauses.iter() {
                        walker.walk_clause(cl, &mut g.trait_counts, &mut g.alias_counts);
                    }
                }
            }
            if let Some(Origin::Member { container, .. }) = r.keys.origin(owner) {
                let ik = *container;
                if let Ok((self_col, trait_col)) = regions.impl_header(ik) {
                    walker.walk_ty(self_col.ty(), &mut g.trait_counts, &mut g.alias_counts);
                    if let Some(tc_col) = trait_col {
                        walker.walk_ty(tc_col.ty(), &mut g.trait_counts, &mut g.alias_counts);
                    }
                }
                if let Ok(cls) = regions.clauses(ik) {
                    for cv in cls {
                        walker.walk_ty(cv.col.ty(), &mut g.trait_counts, &mut g.alias_counts);
                    }
                }
                if let Some(ih) = r.store.impls.get(&ik) {
                    walker.walk_ty(ih.self_ty, &mut g.trait_counts, &mut g.alias_counts);
                    walker.walk_list(ih.trait_args.list(), &mut g.trait_counts, &mut g.alias_counts);
                }
                if let Some(p) = r.store.preds.get(&ik) {
                    for cl in p.clauses.iter() {
                        walker.walk_clause(cl, &mut g.trait_counts, &mut g.alias_counts);
                    }
                }
            }
            let mut last_imp = None;
            for &(k, name) in selected.iter() {
                let ik = tc::ItemKey(k);
                if let Some(n) = name
                    && let Ok(v) = regions.impl_assoc(ik, n)
                {
                    walker.walk_ty(v.ty(), &mut g.trait_counts, &mut g.alias_counts);
                }
                if last_imp == Some(k) {
                    continue;
                }
                last_imp = Some(k);
                if let Ok((self_col, trait_col)) = regions.impl_header(ik) {
                    walker.walk_ty(self_col.ty(), &mut g.trait_counts, &mut g.alias_counts);
                    if let Some(tc_col) = trait_col {
                        walker.walk_ty(tc_col.ty(), &mut g.trait_counts, &mut g.alias_counts);
                    }
                }
                if let Some(ih) = r.store.impls.get(&ik) {
                    walker.walk_ty(ih.self_ty, &mut g.trait_counts, &mut g.alias_counts);
                    walker.walk_list(ih.trait_args.list(), &mut g.trait_counts, &mut g.alias_counts);
                    for &(an, v) in ih.assoc.iter() {
                        walker.walk_ty(v, &mut g.trait_counts, &mut g.alias_counts);
                        if let Ok(rv) = regions.impl_assoc(ik, an) {
                            walker.walk_ty(rv.ty(), &mut g.trait_counts, &mut g.alias_counts);
                        }
                    }
                }
                if let Ok(cls) = regions.clauses(ik) {
                    for cv in cls {
                        walker.walk_ty(cv.col.ty(), &mut g.trait_counts, &mut g.alias_counts);
                    }
                }
                if let Some(p) = r.store.preds.get(&ik) {
                    for cl in p.clauses.iter() {
                        walker.walk_clause(cl, &mut g.trait_counts, &mut g.alias_counts);
                    }
                }
            }
            for &k in opaques.iter() {
                let ik = tc::ItemKey(k);
                if let Ok(cls) = regions.clauses(ik) {
                    for cv in cls {
                        walker.walk_ty(cv.col.ty(), &mut g.trait_counts, &mut g.alias_counts);
                    }
                }
                if let Some(p) = r.store.preds.get(&ik) {
                    for cl in p.clauses.iter() {
                        walker.walk_clause(cl, &mut g.trait_counts, &mut g.alias_counts);
                    }
                }
            }
            for &k in traits.iter() {
                let ik = tc::ItemKey(k);
                if let Ok(cls) = regions.clauses(ik) {
                    for cv in cls {
                        walker.walk_ty(cv.col.ty(), &mut g.trait_counts, &mut g.alias_counts);
                    }
                }
                if let Some(def) = r.store.trait_defs.get(&ik) {
                    for cl in def.supertraits.iter() {
                        walker.walk_clause(cl, &mut g.trait_counts, &mut g.alias_counts);
                    }
                }
                if let Some(p) = r.store.preds.get(&ik) {
                    for cl in p.clauses.iter() {
                        walker.walk_clause(cl, &mut g.trait_counts, &mut g.alias_counts);
                    }
                }
            }
            for &(_, _, a_item) in g.assoc_items.iter() {
                if let Ok(cls) = regions.clauses(a_item) {
                    for cv in cls {
                        walker.walk_ty(cv.col.ty(), &mut g.trait_counts, &mut g.alias_counts);
                    }
                }
                if let Some(p) = r.store.preds.get(&a_item) {
                    for cl in p.clauses.iter() {
                        walker.walk_clause(cl, &mut g.trait_counts, &mut g.alias_counts);
                    }
                }
            }
            for &k in roots.iter() {
                if let Some(row) = r.store.adts.get(&tc::ItemKey(k)) {
                    for f in row.fields.iter() {
                        walker.walk_ty(f.ty, &mut g.trait_counts, &mut g.alias_counts);
                    }
                }
            }
        }
        for p in c.selected.iter() {
            g.trait_counts.push((p.trait_.0, None));
            if let Some(name) = p.name {
                g.alias_counts.push((p.trait_.0, name, None));
            }
        }
        // The control: nothing filed, so every count is unknown (rule 23).
        if plant == FactsPlant::NoTraitCounts {
            g.trait_counts.clear();
            g.alias_counts.clear();
        }
        g.trait_counts.sort_unstable_by_key(|&(k, _)| k);
        g.trait_counts.dedup_by_key(|&mut (k, _)| k);
        g.alias_counts.sort_unstable_by_key(|&(k, n, _)| (k, n));
        g.alias_counts.dedup_by_key(|&mut (k, n, _)| (k, n));
        {
            let arity = super::types::HeadsArity { heads: &r.store.maps, opaques: &[], names: r.names };
            let count = |got: tc::regions::Count| match got {
                tc::regions::Count::Known(n) => Some(n),
                tc::regions::Count::NotKnown => None,
            };
            for t in g.trait_counts.iter_mut() {
                t.1 = count(tc::regions::Arity::trait_lifetimes(&arity, tc::ItemKey(t.0)));
                if r.keys
                    .library_in(r.names, tc::ItemKey(t.0))
                    .and_then(|lib| r.store.maps.traits.get(&lib))
                    .is_some_and(|td| td.flags.has(tc::TraitFlags::AUTO))
                {
                    g.auto_traits.push(t.0);
                }
            }
            g.auto_traits.sort_unstable();
            g.auto_traits.dedup();
            for a in g.alias_counts.iter_mut() {
                a.2 = count(tc::regions::Arity::alias_lifetimes(&arity, tc::ItemKey(a.0), a.1));
            }
        }
        // Synthetic RPITIT associated types (`async fn` / `-> impl Trait` in traits) are not
        // written as `type ...;` members in the trait text, so `NamesSource::assoc_lifetimes`
        // answers `None`; their lifetime count is the corresponding RPITIT opaque's captures.
        drop(regions);
        for a in g.alias_counts.iter_mut() {
            if a.2.is_some() {
                continue;
            }
            let tk = tc::ItemKey(a.0);
            ensure_rpitit_trait_rows(r, tk);
            if let Some(opaque) = r.store.rpitit_traits.get(&tk).and_then(|tr| tr.rows.iter().find(|s| s.name == a.1)).map(|s| s.opaque) {
                a.2 = r.store.regions.made(opaque).and_then(|f| f.regions().opaque_captures).or_else(|| r.store.maps.opaque_lifetimes.get(&opaque).copied());
            }
        }
    }
    g.items = items;
    g
}

/// Files `FiledRegions` for a provided trait fn's RPITIT `Twin` opaque (`rpitit::default_key`)
/// by cloning its source opaque's regions and rewriting `Opaque(source, args)` to
/// `Opaque(twin, args)` in each clause column root.
fn ensure_twin_opaque_regions<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, twin_k: tc::ItemKey) {
    let Some((source, owner)) = r.store.rpitit_defaults.get(&twin_k).map(|t| (t.source, t.owner)) else {
        return;
    };
    if r.store.regions.made(twin_k).is_some() {
        return;
    }
    if r.store.regions.made(source).is_none()
        && let Some(w) = member_fn_want(r.names, &*r.keys, owner)
    {
        ensure_one(r, w);
    }
    let Some(src_filed) = r.store.regions.made(source) else {
        return;
    };
    let alloc = r.store.tys.alloc();
    let src = src_filed.regions();
    let mut lifetimes = tc::regions::LifetimeParams::new_in(src.params.count(), alloc);
    for i in 0..src.params.count() {
        if src.params.is_late(i) == Ok(true) {
            let _ = lifetimes.set_late(i);
        }
    }
    for &(longer, shorter) in src.params.outlives() {
        let _ = lifetimes.add_outlives(longer, shorter);
    }
    let mut hr = tc::region_facts::HeadRegions::new_in(lifetimes, alloc);
    hr.opaque_captures = src.opaque_captures;
    hr.outlives.extend_from_slice(&src.outlives);

    struct TwinArity<'a> {
        inner: &'a dyn tc::regions::Arity,
        twin: tc::ItemKey,
        source: tc::ItemKey,
        captures: Option<u32>,
    }
    impl tc::regions::Arity for TwinArity<'_> {
        fn adt_lifetimes(&self, adt: tc::ItemKey) -> tc::regions::Count {
            self.inner.adt_lifetimes(adt)
        }
        fn opaque_captures(&self, opaque: tc::ItemKey) -> tc::regions::Count {
            if opaque == self.twin {
                match self.captures {
                    Some(n) => tc::regions::Count::Known(n),
                    None => self.inner.opaque_captures(self.source),
                }
            } else {
                self.inner.opaque_captures(opaque)
            }
        }
        fn alias_lifetimes(&self, trait_: tc::ItemKey, name: tc::Name) -> tc::regions::Count {
            self.inner.alias_lifetimes(trait_, name)
        }
        fn trait_lifetimes(&self, trait_: tc::ItemKey) -> tc::regions::Count {
            self.inner.trait_lifetimes(trait_)
        }
    }

    let swap_one = |tys: &tc::PortableTys<'_>, t: tc::PTy| -> Option<tc::PTy> {
        match tys.view().node(t).ok()? {
            tc::PNode::Opaque(k, args) if k == source => tys.intern_shared(tc::PNode::Opaque(twin_k, args)).ok(),
            _ => Some(t),
        }
    };
    let swap_list = |tys: &tc::PortableTys<'_>, l: tc::List| -> Option<tc::List> {
        let items = tys.view().list(l).ok()?;
        let mut out: AVec<'static, tc::PTy> = AVec::with_capacity_in(items.len(), alloc);
        let mut changed = false;
        for &it in items {
            let sw = swap_one(tys, it)?;
            if sw != it {
                changed = true;
            }
            out.push(sw);
        }
        if !changed {
            return Some(l);
        }
        tys.intern_list_shared(&out).ok()
    };

    let arity = TwinArity { inner: src_filed.arity(), twin: twin_k, source, captures: src.opaque_captures };
    for cl in src.clauses.iter() {
        let root_ty = cl.col.ty();
        let Ok(node) = r.store.tys.view().node(root_ty) else { continue };
        let new_ty = match node {
            tc::PNode::Opaque(k, args) if k == source => r.store.tys.intern_shared(tc::PNode::Opaque(twin_k, args)).ok(),
            tc::PNode::ExTrait(tr, args) => swap_list(&r.store.tys, args).and_then(|a| r.store.tys.intern_shared(tc::PNode::ExTrait(tr, a)).ok()),
            tc::PNode::ExProj { trait_, name, args, term } => swap_list(&r.store.tys, args).and_then(|a| {
                let t = swap_one(&r.store.tys, term)?;
                r.store.tys.intern_shared(tc::PNode::ExProj { trait_, name, args: a, term: t }).ok()
            }),
            _ => Some(root_ty),
        };
        let Some(new_ty) = new_ty else { continue };
        if let Ok(col) = tc::regions::RegionCol::lay(r.store.tys.view(), new_ty, &arity, cl.col.as_slice(), alloc) {
            hr.clauses.push(tc::region_facts::ClauseRegions { nth: cl.nth, binders: cl.binders, col });
        }
    }
    if let Ok(filed) = super::head_record::FiledRegions::new(hr, &arity, r.store.tys.view(), alloc) {
        r.store.regions.file(twin_k, filed);
        if let Some(n) = src.opaque_captures {
            r.store.maps.opaque_lifetimes.insert_absent(twin_k, n);
        }
    }
}

/// Populate `Store::rpitit_traits` for `trait_` if not already computed (e.g. when borrow
/// checking runs against a warm cached `Checked` record without re-running typeck in this process).
fn ensure_rpitit_trait_rows<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, trait_: tc::ItemKey) {
    if r.store.rpitit_traits.contains_key(&trait_) {
        return;
    }
    let Some(lib) = r.keys.library_in(r.names, trait_) else { return };
    if !matches!(r.names.detail_of(lib), Some(blaze_surface::ItemDetail::Trait)) {
        return;
    }
    container_head(r, lib, true);
    if !r.store.maps.traits.get(&lib).is_some_and(super::rpitit::may_have_rows) {
        return;
    }
    for _ in 0..256 {
        let step = {
            let Some(t) = r.store.maps.traits.get(&lib) else { return };
            let Some(extra) = r.store.trait_defs.get(&trait_) else { return };
            match super::rpitit::trait_rows(r.store, trait_, t, &extra.assoc) {
                Ok(_) => None,
                Err(super::rpitit::Need::Fn(fk)) => Some(fk),
                Err(super::rpitit::Need::Refused(..)) => None,
            }
        };
        let Some(fk) = step else { break };
        if let Some(w) = member_fn_want(r.names, &*r.keys, fk) {
            ensure_one(r, w);
        } else {
            break;
        }
    }
}

/// The variances of ADT `key`'s parameters, lifetimes first then its type and const
/// parameters in order, from the ADT facts (adt_facts.rs, every parameter computed by the
/// fixed point over its group, std's ADTs from their definitions), pushed onto `list`.
/// `None`: its facts were refused (its head unmade, a field's column unread, an ADT it names
/// refused): the ADT gets no row, and the lowering records each relation through it as
/// `Unified::VarianceUnknown` (blaze-flow-lower variance.rs), never a silent `In`.
fn adt_variances_in(table: &super::adt_facts::AdtFactsTable<'_>, key: u128, list: &mut AVec<'_, blaze_flow_lower::Variance>) -> Option<(u32, u32)> {
    let given = table.variances(key)?;
    let lo = list.len() as u32;
    for v in given.iter() {
        list.push(v.answer());
    }
    Some((lo, list.len() as u32))
}

/// The types body owner `owner` assumes well-formed (RFC 1214, "Implied bounds"): its
/// signature's, inputs and output, laid over its head record's column (`RegionFacts::sig`, over
/// `FnPtr(inputs.., output)`), and, for a member of an impl, the impl header's self type and
/// trait reference (`RegionFacts::impl_header`; a member numbers its impl's lifetimes and type
/// parameters first, as the impl does: heads_regions.rs `impl_header_regions`). A trait's member
/// assumes no header. Each laid by the one region counter; one not read is the answer's
/// refusal, by name ([`super::adt_facts::ImpliedRefusal`]).
fn owner_implied_input<'n, 'a, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, owner: tc::ItemKey, alloc: Alloc<'a>) -> super::adt_facts::ImpliedInput<'a> {
    let mut input = super::adt_facts::ImpliedInput::new_in(alloc);
    let arity = super::types::HeadsArity { heads: &r.store.maps, opaques: &[], names: r.names };
    let reader = r.store.regions.reader(alloc);
    let tys = r.store.tys.view();
    match reader.sig(owner) {
        Ok(sig) => input.piece(owner.0, tys, &arity, sig.col.ty(), sig.col.as_slice()),
        Err(u) => input.unread(owner.0, u),
    }
    let imp = match r.keys.origin(owner) {
        Some(Origin::Member { container, .. }) => match r.keys.library_in(r.names, *container) {
            Some(lib) if matches!(r.names.detail_of(lib), Some(blaze_surface::ItemDetail::Impl)) => Some(*container),
            _ => None,
        },
        _ => None,
    };
    if let Some(imp) = imp {
        match reader.impl_header(imp) {
            Ok((self_col, trait_col)) => {
                input.piece(imp.0, tys, &arity, self_col.ty(), self_col.as_slice());
                if let Some(t) = trait_col {
                    input.piece(imp.0, tys, &arity, t.ty(), t.as_slice());
                }
            }
            Err(u) => input.unread(imp.0, u),
        }
    }
    input
}

/// The ADT facts (adt_facts.rs) of `roots` and of every ADT their fields name, transitively:
/// each head made first (the store holds only what something asked for), then laid with its
/// fields' region columns (head_record.rs `RegionReader::field`, the one home of a field's
/// regions) and solved group by group. An ADT whose head cannot be made is a typed refusal,
/// and so is every ADT whose answer needs it.
fn adt_facts_of<'n, 'a, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, roots: &[u128], alloc: Alloc<'a>) -> super::adt_facts::AdtFactsTable<'a> {
    use super::adt_facts::{AdtFactsInput, AdtFactsRefusal, MOST_ADTS};
    use super::adt_variance::{AdtLang, MOST_LAID_NODES, children};
    let cell = r.index.lang("unsafe_cell").map(checker_of);
    let phantom = r.index.lang("phantom_data").map(checker_of);
    let lang_of = |key: tc::ItemKey| {
        if Some(key) == cell {
            AdtLang::UnsafeCell
        } else if Some(key) == phantom {
            AdtLang::PhantomData
        } else {
            AdtLang::Plain
        }
    };
    // Every ADT reachable, sorted by key, with why its head was not made.
    let mut seen: AVec<'a, (u128, Option<AdtUnmade>)> = AVec::new_in(alloc);
    let mut todo: AVec<'a, u128> = AVec::new_in(alloc);
    for &k in roots.iter() {
        if let Err(at) = seen.binary_search_by_key(&k, |&(s, _)| s)
            && seen.len() < MOST_ADTS
        {
            seen.insert(at, (k, None));
            todo.push(k);
        }
    }
    let mut walk: AVec<'a, tc::PTy> = AVec::new_in(alloc);
    let mut found: AVec<'a, u128> = AVec::new_in(alloc);
    while let Some(k) = todo.pop() {
        let key = tc::ItemKey(k);
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
        found.clear();
        {
            let Some(row) = BodyHeadFacts::new(&*r.store, r.names).adt(key) else {
                if let Ok(at) = seen.binary_search_by_key(&k, |&(s, _)| s) {
                    seen[at].1 = Some(unmade);
                }
                continue;
            };
            // A lang ADT's variances are fixed: its fields are not read.
            if lang_of(key) != AdtLang::Plain {
                continue;
            }
            let tys = r.store.tys.view();
            for f in row.fields.iter() {
                walk.clear();
                walk.push(f.ty);
                let mut budget = MOST_LAID_NODES;
                while let Some(t) = walk.pop() {
                    if budget == 0 {
                        break;
                    }
                    budget -= 1;
                    let Ok(node) = tys.node(t) else { continue };
                    if let tc::PNode::Adt(a, _) = node {
                        found.push(a.0);
                    }
                    // A type the walk cannot read is the lay's refusal, by name, below.
                    if let Ok((list, last)) = children(tys, t, node) {
                        walk.extend_from_slice(list);
                        if let Some(c) = last {
                            walk.push(c);
                        }
                    }
                }
            }
        }
        for &a in found.iter() {
            if let Err(at) = seen.binary_search_by_key(&a, |&(s, _)| s)
                && seen.len() < MOST_ADTS
            {
                seen.insert(at, (a, None));
                todo.push(a);
            }
        }
    }
    // Each laid with its fields' columns, as the head record filed them.
    let arity = super::types::HeadsArity { heads: &r.store.maps, opaques: &[], names: r.names };
    let reader = r.store.regions.reader(alloc);
    let tys = r.store.tys.view();
    let mut input = AdtFactsInput::new_in(alloc);
    for &(k, unmade) in seen.iter() {
        if let Some(u) = unmade {
            input.refused(k, AdtFactsRefusal::Unmade(u));
            continue;
        }
        let key = tc::ItemKey(k);
        let Some(row) = BodyHeadFacts::new(&*r.store, r.names).adt(key) else {
            input.refused(k, AdtFactsRefusal::Unmade(AdtUnmade::NoRow));
            continue;
        };
        let lang = lang_of(key);
        input.adt(k, lang, row.lifetimes, row.generics);
        if lang != AdtLang::Plain {
            continue;
        }
        for (vi, v) in row.variants.iter().enumerate() {
            for (j, f) in row.fields.get(v.fields.range()).unwrap_or(&[]).iter().enumerate() {
                let col = match reader.field(key, vi as u32, j as u32) {
                    Ok(c) if c.ty() == f.ty => Ok(c.as_slice()),
                    Ok(_) => Err(tc::region_facts::RegionsUnread { item: key, why: tc::region_facts::UnreadWhy::Refused { what: "a field's region column is laid over another type than the field's" } }),
                    Err(u) => Err(u),
                };
                input.field(tys, &arity, f.ty, col);
            }
        }
        // Its declared outlives bounds (where-clauses and parameters' bounds), the seeds of its
        // requirements (RFC 2093: explicit predicates are part of what a use requires). A bound
        // under its own `for<..>` binds its region and is no requirement.
        let refused = |what: &'static str| AdtFactsRefusal::Regions(tc::region_facts::RegionsUnread { item: key, why: tc::region_facts::UnreadWhy::Refused { what } });
        match (reader.outlives(key), reader.clauses(key)) {
            (Ok(outlives), Ok(clauses)) => {
                for o in outlives.iter() {
                    match *o {
                        tc::region_facts::Outlives::Region { long, short } => input.declared_region(long, short),
                        tc::region_facts::Outlives::Type { clause, region } => match clauses.iter().find(|c| c.nth == clause) {
                            Some(c) if c.binders == 0 => input.declared_type(tys, &arity, c.col.ty(), Ok(c.col.as_slice()), region),
                            Some(_) => {}
                            None => input.outlives_refused(refused("a declared `T: 'r` of an ADT has no clause column")),
                        },
                    }
                }
            }
            (Err(u), _) | (_, Err(u)) => input.outlives_refused(AdtFactsRefusal::Regions(u)),
        }
    }
    input.solve()
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

/// Whether finding `d` was decided by regions (a loan's liveness, an outlives result): every
/// finding but a move's ([`move_words`]' codes), which reads only which places were moved.
/// What a stricter relation can make falsely (`check_borrows`, blaze-flow-lower
/// `Lowered::coarse`).
pub(super) fn reads_regions(d: &BorrowDiagnostic) -> bool {
    !matches!(d.code, "E0382" | "E0507" | "E0508" | "E0509")
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

/// A call refused by the lowering for its head's lifetimes, by name (rule 19): its head regions
/// not read (`Form::RegionsUnread`: the callee, or the body's own fn, and why, from the head
/// records: [`BorrowRefused::RegionsUnread`]), or read and the call not related through them
/// (`CallWritesLifetimes`: what could not be related, with the shape's refusal from
/// `shape_refused` when that is what it lacked, [`BorrowRefused::CallUnrelated`]). `None` for
/// any other refusal.
fn call_refused<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, facts: &BorrowFacts<'_>, refusal: Refusal) -> Option<BorrowRefused> {
    let (start, end) = (refusal.range.start, refusal.range.end);
    match refusal.form {
        blaze_flow_lower::Form::RegionsUnread { item: callee, why } => {
            let (head, item) = (head_words(r, callee), r.keys.library_in(r.names, callee));
            let unlaid = r.store.regions.unlaid_of(callee);
            Some(BorrowRefused::RegionsUnread { callee, item, head, why, unlaid, gap: gap_words(r, callee), start, end })
        }
        blaze_flow_lower::Form::CallWritesLifetimes { callee, lacks } => {
            let (head, item) = (head_words(r, callee), r.keys.library_in(r.names, callee));
            let shape_why = match lacks {
                CallLacks::Shape => facts.shape_refused.binary_search_by_key(&callee.0, |&(k, _)| k).ok().map(|i| facts.shape_refused[i].1),
                _ => None,
            };
            Some(BorrowRefused::CallUnrelated { callee, item, head, lacks, shape_why, start, end })
        }
        _ => None,
    }
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

/// The making store's detail of `callee`'s regions refusal ([`BorrowRefused::RegionsUnread`]'s
/// `gap`): what this store's walk compared (heads.rs `RegionGap::say`), over the head's text
/// its ranges are relative to; empty when this store did not make the head (its record's kind
/// is the whole answer there).
fn gap_words<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, callee: tc::ItemKey) -> Words {
    let mut gap = Words::EMPTY;
    if let (Some(g), Some(f)) = (r.store.region_gaps.get(&callee), r.store.fns.get(&callee)) {
        // `Words` cuts what does not fit and writes `...`: it never fails.
        let _ = g.say(f.text, &mut gap);
    }
    gap
}

/// The type check's own path for a stalled auto-trait goal of an async block's future, with no
/// borrow check after it ([`ProjectWorld::check`]): each async block's WITNESS lowered
/// (stalled_goals.rs `AsyncInteriors::witness_all`, over the facts `gather_facts` reads, their
/// one home) and every stalled row decided on the exact interiors (`decide_stalled_goals`),
/// exactly as `check_borrows` does before its borrow lowerings. What it read goes into `reads`
/// (rule 21). A body whose table cannot be viewed lowers no block: each stalled row is then
/// `FutureWitness` by `decide_stalled_goals`, never a pass (rule 3).
pub(super) fn witness_and_decide<'n, 'a, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'_>,
    done: &tc::Checked<'_>,
    reads: &mut AVec<'a, Read>,
    alloc: Alloc<'a>,
) -> Decision<'a> {
    let mut interiors = AsyncInteriors::new_in(done, alloc);
    if !done.stalled.is_empty() {
        let langs = BorrowLangs::of(r.index);
        let facts = gather_facts_planted(r, body.owner, done, langs, FactsPlant::None, alloc);
        reads.extend_from_slice(facts.reads());
        reads.extend_from_slice(facts.calls.reads());
        reads.extend_from_slice(facts.ties.reads());
        let reader = r.store.regions.reader(alloc);
        let lowering = facts.read_by(&reader, r.store.tys.view());
        if let Ok(tys) = body.tys.view(&r.store.tys) {
            let view = body.typeck.view(tys);
            let requirements = Requirements::new_in(alloc);
            interiors.witness_all(&view, done, &NestedFacts::with_requirements(lowering, &requirements), alloc);
        }
    }
    decide_stalled_goals(r, body, done, &interiors, reads, alloc)
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
    // Every fn's signature regions are its head record's, read in place (D148): the lowering's
    // facts are the gathered ones with the head records' reader. The control
    // (`FactsPlant::NoAliases`) gives none: every call carrying a lifetime is then refused.
    let reader = r.store.regions.reader(alloc);
    let regions: &dyn RegionFacts = if plant == FactsPlant::NoAliases { &NoRegions } else { &reader };
    let lowering = facts.read_by(regions, r.store.tys.view());
    if let Some(out) = show.as_deref_mut() {
        use core::fmt::Write as _;
        let _ = show_facts(lowering, out);
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
    // The first stricter relation any of the body's flows used, and which flow
    // (blaze-flow-lower `Lowered::coarse`; applied after the last flow, below).
    let mut coarse: Option<(Refusal, FlowOf)> = None;
    // Each async block's exact interior, when a goal stalled, from its witness lowering, before
    // and apart from the borrow lowerings below (stalled_goals.rs `witness_all`).
    interiors.witness_all(&view, done, &NestedFacts::with_requirements(lowering, &requirements), alloc);
    for &of in nested_inner_first(&view, alloc).iter() {
        let closure_usage = if plant != FactsPlant::NoClosureRequirements
            && let FlowOf::Closure(closure) = of
        {
            Some(classify_closure_env_use(closure, &view, done, r.store, &langs, &lowering, alloc))
        } else {
            None
        };
        let lowered = {
            let nested = NestedFacts::with_requirements(lowering, &requirements);
            match of {
                FlowOf::Closure(closure) => {
                    let cf = if closure_usage == Some(ClosureEnvUse::FnOnceOnly) {
                        closure_flow_of_as_fn_once(&view, done, closure, &nested, alloc)
                    } else {
                        closure_flow_of(&view, done, closure, &nested, alloc)
                    };
                    cf.map(|cf| (cf.lowered.flow, cf.lowered.params, cf.externals, cf.lowered.coarse))
                }
                FlowOf::AsyncBlock(block) => {
                    blaze_flow_lower::lower_async(&view, done, block, &nested, alloc).map(|la| (la.flow, AVec::new_in(alloc), la.externals, la.coarse))
                }
                FlowOf::Owner => continue,
            }
        };
        let (flow, params, mut externals, flow_coarse) = match lowered {
            Ok(x) => x,
            Err(refusal) => {
                verdict.refused = Some(lower_refused(r, &facts, refusal));
                verdict.within = nested_range(&view, of);
                break;
            }
        };
        if coarse.is_none() {
            coarse = flow_coarse.map(|c| (c, of));
        }
        if let Some(out) = show.as_deref_mut() {
            let _ = show_flow(&flow, of, out);
        }
        needs.clear();
        let mut forbid_env_escape: AVec<'a, blaze_flow::Origin> = AVec::new_in(alloc);
        if let Some(usage) = closure_usage
            && usage != ClosureEnvUse::FnOnceOnly
            && let FlowOf::Closure(closure) = of
        {
            prepare_closure_env(closure, done, usage, &mut externals, &mut forbid_env_escape, alloc);
        }
        // The control (FactsPlant::NoClosureRequirements): no externals, so no requirement is
        // mapped and each outlives result stays the refusal.
        let externals = if plant == FactsPlant::NoClosureRequirements { None } else { Some(&externals[..]) };
        if file_flow(&flow, &params, of, &view, externals, &forbid_env_escape, &mut needs, &mut verdict, alloc) {
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
    let nested = NestedFacts::with_requirements(lowering, &requirements);
    let mut next = if verdict.refused.is_some() { None } else { Some(FlowOf::Owner) };
    while let Some(of) = next {
        // Checked above, innermost first: closures and every async block but an `async fn`'s
        // root (whose placeholders are the signature's: its results are the fn's own).
        if of.is_nested(&view) {
            next = next_nested(&view, of);
            continue;
        }
        let (flow, params, flow_coarse) = match flow_of(of, &view, done, &nested, alloc) {
            Ok(f) => f,
            Err(refusal) => {
                verdict.refused = Some(lower_refused(r, &facts, refusal));
                verdict.within = nested_range(&view, of);
                break;
            }
        };
        if coarse.is_none() {
            coarse = flow_coarse.map(|c| (c, of));
        }
        if let Some(out) = show.as_deref_mut() {
            let _ = show_flow(&flow, of, out);
        }
        if file_flow(&flow, &params, of, &view, None, &[], &mut needs, &mut verdict, alloc) {
            break;
        }
        next = next_nested(&view, of);
    }
    // STRICT RELATIONS REFUSE ONLY ON A FINDING (blaze-flow-lower `Lowered::coarse`): a flow
    // related by a stricter relation than Rust's (it only adds constraints) is clean under
    // Rust's rules when clean under it, so the body stands checked; a finding of one that reads
    // regions may be the relation's artifact, so the body is refused by the stricter step's
    // name and those findings dropped (rule 3: never a false finding, never a pass). Move
    // findings read no region: kept. A refusal already made stands (its findings already made
    // are kept, as before, except these).
    if let Some((c, of)) = coarse
        && verdict.findings.iter().any(reads_regions)
    {
        verdict.findings.retain(|d| !reads_regions(d));
        if verdict.refused.is_none() {
            verdict.refused = Some(BorrowRefused::Stricter(c));
            verdict.within = nested_range(&view, of);
        }
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
#[expect(clippy::too_many_arguments, reason = "the flow, its parameters, which flow, the body's view, the closure's externals and env-escape list, the two outputs and the allocator")]
pub(super) fn file_flow<'a>(
    flow: &Flow<Alloc<'a>>,
    params: &[blaze_flow_lower::ParamOrigin],
    of: FlowOf,
    view: &tc::BodyView<'_>,
    externals: Option<&[(blaze_flow::Origin, blaze_flow_lower::External)]>,
    forbid_env_escape: &[blaze_flow::Origin],
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
            verdict.findings.push(BorrowDiagnostic { start: m.span.lo, end: m.span.hi, code, text, borrowed: None, kept: None });
        }
        for b in record.borrows.iter() {
            // An `Invalidated` finding's parts: its row of `loan_parts`, sorted by `(at, loan)`
            // as the findings are; a finding with no row says so (`KeptAt::Unfound`).
            let mut parts: Option<Option<blaze_flow::LoanParts>> = None;
            let (span, (code, text)) = match *b {
                BorrowFinding::Invalidated { at, loan, span, returned } => {
                    parts = Some(record.loan_parts.binary_search_by_key(&(at, loan), |p| (p.at, p.loan)).ok().map(|i| record.loan_parts[i]));
                    (span, invalidated_words(flow, at, loan, returned))
                }
                // A nested body's outlives result is its owner's to prove: a closure's, between
                // two positions of its value (or propagated through a late-bound region's
                // non-local upper/lower bound in `flow.known`), is a requirement its maker's flow
                // emits; a region the call chooses escaping into one the closure keeps is E0521;
                // any other is refused (never dropped).
                BorrowFinding::UnprovenOutlives { span, from, into, .. } if of.is_nested(view) => {
                    if let Some(req) = externals.and_then(|ext| propagate_nested_requirement(ext, &flow.known, from, into, alloc)) {
                        needs.push(req);
                        continue;
                    }
                    // A region the expecting clause's `for<..>` binds, known to outlive no region
                    // the owner names, that must outlive one the owner names: rustc's E0521 in
                    // the closure (region_infer `try_propagate_universal_region_error`), never a
                    // requirement on the owner (blaze-flow-lower closure.rs `escapes_closure`).
                    if externals.is_some_and(|ext| blaze_flow_lower::escapes_closure(ext, &flow.known, from, into, alloc)) {
                        (span, ("E0521", "borrowed data escapes outside of closure: a region each call chooses must outlive one the closure keeps"))
                    } else if externals.is_some_and(|ext| env_escapes_closure(ext, &flow.known, forbid_env_escape, from, into, alloc)) {
                        (span, ("E0521", "captured variable cannot escape closure body: a per-call environment reference must outlive a region the closure's owner names"))
                    } else {
                        if verdict.refused.is_none() {
                            let endpoint = |origin| externals?.iter().find(|(o, _)| *o == origin).map(|&(_, end)| end);
                            verdict.refused = Some(BorrowRefused::NestedOutlives { from: endpoint(from), into: endpoint(into), start: span.lo, end: span.hi });
                        }
                        continue;
                    }
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
            let borrowed = parts.flatten().map(|p| (p.issued.lo, p.issued.hi));
            let kept = parts.map(|row| match row.map(|p| p.kept) {
                Some(blaze_flow::Kept::Use { span, .. }) => KeptAt::Use { start: span.lo, end: span.hi },
                Some(blaze_flow::Kept::Signature { .. }) => KeptAt::Signature,
                Some(blaze_flow::Kept::Unfound) | None => KeptAt::Unfound,
            });
            verdict.findings.push(BorrowDiagnostic { start: span.lo, end: span.hi, code, text, borrowed, kept });
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
fn show_facts(facts: ReadBy<'_, '_>, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
    use core::fmt::Write as _;
    for &k in facts.facts.items() {
        let key = tc::ItemKey(k);
        writeln!(out, "head {k:x}: {:?}", Facts::callee(&facts, key))?;
        match Facts::sig(&facts, key) {
            Ok(life) => {
                writeln!(out, "  sig {:?} inputs {} bounds {:?} body {:?}", life.slots(), life.inputs(), life.bounds(), life.body())?
            }
            Err(unread) => writeln!(out, "  sig unread {:?}", unread.why)?,
        }
        match Facts::shape(&facts, key) {
            Some(s) => writeln!(out, "  shape {:?} ends {:?}", s.marks, s.ends)?,
            None => writeln!(out, "  no shape")?,
        }
        match Facts::call_bounds(&facts, key) {
            Some(b) => writeln!(out, "  bounds {b:?}")?,
            None => writeln!(out, "  no call bounds")?,
        }
    }
    for &(k, l) in facts.facts.object_traits.iter() {
        writeln!(out, "object trait {k:x}: declares a lifetime {l:?}")?;
    }
    // The owner's implied bounds (RFC 1214; `BorrowFacts::implied`), one line.
    writeln!(out, "implied {:?}", facts.facts.implied())?;
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
        let mut typed = None;
        // The check files an auto-trait goal that fails only on an async block's superset
        // interior as a stalled row; the borrow check lowers each async body once, and the
        // rows are decided on the exact interiors of those lowerings (stalled_goals.rs).
        let mut verdict = self.check_then(body, Stalls::Decide, alloc, |r, lowered, done| {
            let decided = {
                let _cost = super::tables::cost::Cost::enter(r.store.cost, super::tables::cost::Part::Borrows);
                let mut interiors = AsyncInteriors::new_in(done, alloc);
                borrows = Some(borrows_of(r, lowered, done, &mut reads, &mut interiors, show, plant, alloc));
                decide_stalled_goals(r, lowered, done, &interiors, &mut reads, alloc)
            };
            // The checked record (D163 R1), encoded once here where the columns are, AFTER the
            // stalled goals are decided (F18): a check whose stalled goals all held on their exact
            // interiors (`Proved`) keeps the record a clean check keeps; any other decision files
            // a diagnostic, a gap or a refusal, and the record refuses its stalled rows by name
            // (`NotClean { what: "stalled" }`, counted by tables/task.rs).
            if done.diags.is_empty() && done.undecided.is_empty() {
                typed = Some(if matches!(decided, Decision::Proved) {
                    blaze_typeck::checked_record::encode_decided(done, alloc)
                } else {
                    blaze_typeck::checked_record::encode(done, alloc)
                });
            }
            decision = Some(decided);
        });
        // What the borrow verdict and the decision read, in the body's read list and its
        // digest (rule 21: borrow_reads.rs, stalled_goals.rs).
        file_borrow_reads(&mut verdict, &reads, alloc);
        verdict.typed = typed;
        if let Some(decision) = decision {
            decision.file(&mut verdict);
        }
        let borrows = match borrows {
            Some(b) => b,
            None => {
                let why = if verdict.diagnostics.is_empty() { Untyped::TypesNotChecked } else { Untyped::TypeErrors };
                BorrowVerdict { findings: AVec::new_in(alloc), refused: Some(BorrowRefused::Untyped(why)), within: None }
            }
        };
        (verdict, borrows)
    }
}

/// A fn's signature regions reach the borrow check from its head record (D148, piece L3), the
/// whole path: the program's text, its heads made and their regions filed (L1a, L2), the body's
/// facts gathered with no signature text read, the lowering reading the column through
/// [`ReadBy`]. The fixture is the held test front2_w1_callee_fn_life.rs's.
#[cfg(test)]
mod region_record_tests {
    use super::{BorrowRefused, FactsPlant};
    use crate::crates::{DirShelf, Inputs, Toolchain, UserCrate};
    use crate::front2::project::{BodyRef, ProjectWorld};
    use crate::refusal::SourceFile;

    const SOURCE: &str = r#"
fn f<'a, 'b>(y: &'b u8, x: &'a u8) -> &'a u8 {
    let _ = y;
    x
}

fn h(x: &u8, y: &u8) -> &u8 {
    let _ = y;
    x
}

fn order_clean() {
    let a = 1u8;
    let result;
    {
        let b = 2u8;
        result = f(&b, &a);
    }
    let _used = *result;
}

fn order_control() {
    let b = 1u8;
    let result;
    {
        let a = 2u8;
        result = f(&b, &a);
    }
    let _used = *result;
}

fn mention_control() {
    let b = 1u8;
    let result;
    {
        let a = 2u8;
        result = f(&b, &a);
    }
    let _ = *result;
}

fn mention_no_access() {
    let mut x = 0u8;
    let m = &mut x;
    let _ = x;
    *m = 1;
}

fn mention_of_the_reference() {
    let r;
    {
        let a = 0u8;
        r = &a;
    }
    let _ = r;
}

fn assign_mention_control() {
    let r;
    {
        let a = 0u8;
        r = &a;
    }
    _ = *r;
}

fn statement_read_control() {
    let r;
    {
        let a = 0u8;
        r = &a;
    }
    *r;
}

fn calls_refused() {
    let a = 1u8;
    let r = h(&a, &a);
    let _ = *r;
}

macro_rules! byte {
    () => {
        u8
    };
}

fn m_macro(x: &u8, _y: byte!()) -> &u8 {
    x
}

fn calls_macro_type() {
    let a = 1u8;
    let r = m_macro(&a, 2);
    let _ = *r;
}

fn stub<'a>(a: &'a u8, _b: &'a u8) -> &'a u8 {
    a
}

fn m(x: &u8, _y: fn(&u8, &u8) -> &u8) -> &u8 {
    x
}

fn calls_soft_refused() {
    let a = 1u8;
    let r = m(&a, stub);
    let _ = *r;
}

fn main() {}
"#;

    /// THE FIRST CONTROL of D148 through the record path: `f<'a, 'b>(y: &'b u8, x: &'a u8) ->
    /// &'a u8` ties its result to its SECOND argument. order_clean (`a`, the second, outlives
    /// `result`) is clean; order_control (`a` dies in the inner block) is E0597. PLANTED
    /// (`FactsPlant::NoAliases`: the lowering given no head records' regions): order_control is
    /// no longer a finding but refused `RegionsUnread` naming `f`, so the E0597 came from the
    /// record's column and from nothing else (rule 23).
    ///
    /// A callee whose head is a program error (`h`: two elided lifetimes in, one out, no rule
    /// picks the output's: E0106, reported by `h`'s own check) leaves its caller untyped
    /// (`Untyped(TypesNotChecked)`): no head, no type check, no borrow check, never a pass.
    /// A callee whose erased head is whole but whose REGIONS the producer refused softly (`m`,
    /// a type macro in a parameter: types.rs `regions_made`, "a type macro's expansion", a
    /// `RegionGap` filed refused) is typed, and its call refused by name, `RegionsUnread {
    /// item: m, why: Refused { .. } }`, never checked under a guess. Its control is order_clean:
    /// the same call shape of a head whose regions were made is checked.
    ///
    /// Prints one line per body: its verdict.
    #[test]
    fn signature_regions_come_from_the_head_record() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let home = std::env::var("HOME").expect("HOME is set");
        let tc = Toolchain::open(&mut DirShelf::new(std::format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
        let files = [("/l3/src/main.rs".to_string(), SOURCE.to_string())];
        let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
        let user = UserCrate { name: "l3", dir: "/l3/", root: "/l3/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, erased.bump()).expect("fixture names");
        let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
        let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front2");
        let bodies = world.bodies(alloc);
        let body = |name: &str| {
            bodies
                .iter()
                .find_map(|&(body, _)| {
                    let BodyRef::Free(key) = body else { return None };
                    (names.item_text(key).and_then(|item| item.name) == Some(name)).then_some(body)
                })
                .unwrap_or_else(|| panic!("missing fixture body {name}"))
        };

        let (types, borrows) = world.check_borrows(body("order_clean"), alloc);
        std::println!("order_clean: {borrows:?}");
        assert!(types.is_clean(), "order_clean: {types:?}");
        assert!(borrows.is_clean(), "order_clean is clean: {borrows:?}");

        let (types, borrows) = world.check_borrows(body("order_control"), alloc);
        std::println!("order_control: {borrows:?}");
        assert!(types.is_clean(), "order_control: {types:?}");
        assert!(borrows.refused.is_none(), "order_control: {borrows:?}");
        assert!(borrows.findings.iter().any(|d| d.code == "E0597"), "order_control is E0597: {borrows:?}");

        let (_, planted) = world.check_borrows_planted(body("order_control"), FactsPlant::NoAliases, alloc);
        std::println!("order_control with no head regions: {planted:?}");
        assert!(planted.findings.is_empty(), "CONTROL: a finding with no head regions: {planted:?}");
        assert!(matches!(planted.refused, Some(BorrowRefused::RegionsUnread { .. })), "CONTROL: not refused by name: {planted:?}");

        // `let _ = *result;` uses `result` only by MENTIONING `*result` (blaze-flow
        // `StmtKind::Mention`): evaluating the place dereferences `result`, so `result` is live
        // there and `a`'s loan with it: E0597. Lowered as nothing (before the statement) it
        // was accepted with a dangling borrow; that control is blaze-flow-lower's
        // `a_mention_through_a_dangling_reference_is_e0597` (`Plant::MentionNothing`).
        // `_ = *r;` is the same mention, `*r;` a read (the Reference, "Expression statements").
        for name in ["mention_control", "assign_mention_control", "statement_read_control"] {
            let (types, mention) = world.check_borrows(body(name), alloc);
            std::println!("{name}: {mention:?}");
            assert!(types.is_clean(), "{name}: {types:?}");
            assert!(mention.refused.is_none(), "{name}: {mention:?}");
            assert!(mention.findings.iter().any(|d| d.code == "E0597"), "{name} is E0597: {mention:?}");
        }
        // A mention is no access (the Reference, "Wildcard pattern": it "does not copy, move or
        // borrow the value it matches"): `let _ = x;` under a live `&mut x` invalidates
        // nothing. And naming a reference reads nothing through it: only a `Deref` on the
        // way makes the mention a use, so `let _ = r;` with `r` dangling is clean.
        for name in ["mention_no_access", "mention_of_the_reference"] {
            let (types, clean) = world.check_borrows(body(name), alloc);
            std::println!("{name}: {clean:?}");
            assert!(types.is_clean(), "{name}: {types:?}");
            assert!(clean.is_clean(), "{name} is clean: {clean:?}");
        }

        // A head that is a program error: the caller is untyped, the error is the callee's.
        let (types, refused) = world.check_borrows(body("calls_refused"), alloc);
        std::println!("calls_refused: {types:?} {refused:?}");
        assert!(refused.findings.is_empty(), "calls_refused is checked under a guess: {refused:?}");
        assert!(
            matches!(refused.refused, Some(BorrowRefused::Untyped(super::Untyped::TypesNotChecked))),
            "calls_refused: the caller of a head in error is untyped: {refused:?}"
        );
        // `h`'s E0106 is its own check's (a diagnostic, or the refusal that stopped it: the
        // verdict's text names the code either way).
        let (h_types, _) = world.check_borrows(body("h"), alloc);
        let h_text = std::format!("{h_types:?}");
        std::println!("h: {h_text}");
        assert!(h_text.contains("E0106"), "h's own check reports its E0106: {h_text}");

        // A callee whose signature contains a type macro (`m_macro` takes `byte!()`) has its
        // expanded type's regions laid and its calls borrow-checked cleanly.
        let (types, mac) = world.check_borrows(body("calls_macro_type"), alloc);
        assert!(types.is_clean(), "calls_macro_type is typed: {types:?}");
        assert!(mac.is_clean(), "calls_macro_type is clean: {mac:?}");

        // A head whose regions alone were refused: typed, and the call refused by its name.
        let (types, soft) = world.check_borrows(body("calls_soft_refused"), alloc);
        std::println!("calls_soft_refused: {types:?} {soft:?}");
        assert!(types.is_clean(), "calls_soft_refused is typed: {types:?}");
        assert!(soft.findings.is_empty(), "calls_soft_refused is checked under a guess: {soft:?}");
        match soft.refused {
            Some(refused @ BorrowRefused::RegionsUnread { why, item, unlaid, .. }) => {
                // Its head is filed whole: its regions are refused by their kind's name, never
                // said as a refused head (heads.rs `file_regions`).
                assert!(matches!(why, blaze_typeck::region_facts::UnreadWhy::Refused { .. }), "calls_soft_refused: why: {why:?}");
                let named = item.and_then(|lib| names.item_text(lib)).and_then(|t| t.name);
                assert_eq!(named, Some("m"), "calls_soft_refused names the callee");
                // By its typed cause and part: `m`'s input 1 is `fn(&u8, &u8) -> &u8`, whose
                // elided return lifetime has two input lifetimes (`FnPtrElision`); the class is
                // printed first and short (blaze-cli `first_refusal_class` keeps up to the first
                // `] `).
                use crate::front2::head_record::Unlaid;
                use crate::front2::types::{NotLaidWhy, SigPart};
                assert_eq!(unlaid, Some(Unlaid::NotLaid { why: NotLaidWhy::FnPtrElision, part: SigPart::Input(1) }), "calls_soft_refused: the typed kind");
                let mut said = super::Words::EMPTY;
                core::fmt::Write::write_fmt(&mut said, format_args!("{refused}")).unwrap();
                let said = said.as_str();
                assert!(said.starts_with("[NotLaid/FnPtrElision/Input] "), "calls_soft_refused: the class is not first: {said}");
                assert!(said.contains("input 1"), "calls_soft_refused: the input is not named: {said}");
                // CONTROL (rule 23): the same refusal with no typed kind prints the old class,
                // so the bracket above is the kind's, not the text's.
                let BorrowRefused::RegionsUnread { callee, item, head, why, gap, start, end, .. } = refused else { unreachable!() };
                let untyped = BorrowRefused::RegionsUnread { callee, item, head, why, unlaid: None, gap, start, end };
                let mut said = super::Words::EMPTY;
                core::fmt::Write::write_fmt(&mut said, format_args!("{untyped}")).unwrap();
                assert!(said.as_str().starts_with("[Refused: "), "CONTROL: an untyped refusal printed a typed class: {}", said.as_str());
            }
            other => panic!("calls_soft_refused is not refused by the callee's name: {other:?}"),
        }
    }
}

/// F18 (D197 root 1): a body whose stalled goals all held on their exact interiors keeps its
/// typed record ([`ProjectWorld::check_borrows`] encodes after `decide_stalled_goals`). tempo's
/// `main` hands async blocks to spawners: each block's `Send` stalls at the check on its
/// superset interior and is proved on its witness, so without this the body has no record and
/// the run checks it again.
#[cfg(test)]
mod stalled_record_tests {
    use super::Stalls;
    use crate::crates::{DirShelf, Inputs, Toolchain, UserCrate};
    use crate::front2::project::{BodyRef, ProjectWorld};
    use crate::refusal::SourceFile;
    use blaze_typeck::checked_record::{self, CheckedRecordError};

    const SOURCE: &str = r#"
use std::rc::Rc;
fn require_send<T: Send>(_: T) {}
fn local_send() {
    require_send(async {
        let b = 1u8;
        async {}.await;
        drop(b);
    });
}
fn local_send_rc() {
    require_send(async {
        let r = Rc::new(1u8);
        async {}.await;
        drop(r);
    });
}
fn main() {}
"#;

    /// `local_send` (rustc accepts it: a `u8` and the awaited `async {}` are `Send`) keeps a
    /// record, and the record round-trips (rule 26). CONTROL, the cause removed (rule 23): the
    /// same check's columns encoded BEFORE the decision, as `check_borrows_in` did, are refused
    /// `NotClean { what: "stalled" }`, so the record comes from deciding first. TWIN:
    /// `local_send_rc` (E0277, an `Rc` held across the `.await`) keeps none (its stalled rows
    /// were not proved), and its verdict is not clean, so tables/task.rs `keeps_typed` drops it.
    ///
    /// Prints: `F18: local_send keeps <n> bytes; control: <refusal>; twin: <typed>`
    #[test]
    fn a_body_whose_stalled_goals_were_proved_keeps_its_typed_record() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let home = std::env::var("HOME").expect("HOME is set");
        let tc = Toolchain::open(&mut DirShelf::new(std::format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
        let files = [("/f18/src/main.rs".to_string(), SOURCE.to_string())];
        let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
        let user = UserCrate { name: "f18", dir: "/f18/", root: "/f18/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, erased.bump()).expect("fixture names");
        let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
        let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front2");
        let bodies = world.bodies(alloc);
        let body = |name: &str| {
            bodies
                .iter()
                .find_map(|&(body, _)| {
                    let BodyRef::Free(key) = body else { return None };
                    (names.item_text(key).and_then(|item| item.name) == Some(name)).then_some(body)
                })
                .unwrap_or_else(|| panic!("missing fixture body {name}"))
        };

        let (types, _) = world.check_borrows(body("local_send"), alloc);
        assert!(types.is_clean(), "local_send: {types:?}");
        let bytes = match &types.typed {
            Some(Ok(bytes)) => bytes,
            other => panic!("local_send keeps no typed record: {other:?}"),
        };
        let read = checked_record::decode(bytes, alloc).unwrap_or_else(|e| panic!("local_send's record: {e}"));
        let again = checked_record::encode(&read, alloc).unwrap_or_else(|e| panic!("local_send's record re-encoded: {e}"));
        assert_eq!(again.as_slice(), bytes.as_slice(), "local_send's record round-trips");

        // CONTROL: the columns as the check left them, before the decision.
        let mut stalled = 0;
        let mut before = None;
        let _ = world.check_then(body("local_send"), Stalls::Decide, alloc, |_, _, done| {
            stalled = done.stalled.len();
            before = Some(checked_record::encode(done, alloc).map(|_| ()));
        });
        assert!(stalled > 0, "CONTROL: local_send stalls its `Send` at the check");
        let control = before.expect("the check finished");
        assert!(matches!(control, Err(CheckedRecordError::NotClean { what: "stalled", .. })), "CONTROL: encoded before the decision: {control:?}");

        // TWIN: a stalled goal that fails.
        let (twin, _) = world.check_borrows(body("local_send_rc"), alloc);
        assert!(!twin.is_clean() && twin.diagnostics.iter().any(|d| d.code == "E0277"), "local_send_rc: {twin:?}");
        assert!(!matches!(twin.typed, Some(Ok(_))), "local_send_rc keeps a record: {:?}", twin.typed);
        std::println!("F18: local_send keeps {} bytes; control: {control:?}; twin: {:?}", bytes.len(), twin.typed.as_ref().map(|t| t.as_ref().map(|b| b.len())));
    }
}

/// `[External::Env -> External::SigOwner]`: a closure passed directly to an `FnOnce`-only callee
/// (e.g. `Option::and_then(|p| self.ends.get_mut(p))`) consumes its `ByMutRef` capture once, so
/// its `&mut` capture position becomes an owner-enforced `Need::Capture` -> `Need::Sig`
/// requirement; `FnMut` closures and `ByValue` captures still reject escaping env references
/// with `E0521`, and owner-side aliasing/dangling borrows are still caught.
#[cfg(test)]
mod closure_env_tests {
    use super::{BorrowRefused, FactsPlant};
    use crate::crates::{DirShelf, Inputs, Toolchain, UserCrate};
    use crate::front2::project::{BodyRef, ProjectWorld};
    use crate::refusal::SourceFile;

    const SOURCE: &str = r#"
struct Slab {
    items: [u8; 4],
}

impl Slab {
    fn get_mut(&mut self, i: usize) -> Option<&mut u8> {
        self.items.get_mut(i)
    }
}

struct Sim {
    ends: Slab,
}

fn fnonce_and_then_mut_capture(slab: &mut Slab, peer: Option<usize>) {
    if let Some(it) = peer.and_then(|p| slab.get_mut(p)) {
        *it = 1;
    }
}

fn fnonce_projected_field_mut_capture(sim: &mut Sim, peer: Option<usize>) {
    if let Some(it) = peer.and_then(|p| sim.ends.get_mut(p)) {
        *it = 2;
    }
}

fn fnonce_aliasing_conflict_rejected(slab: &mut Slab, peer: Option<usize>) {
    let it = peer.and_then(|p| slab.get_mut(p));
    let other = slab.items[0];
    if let Some(m) = it {
        *m = other;
    }
}

fn fnonce_dangling_return_rejected(peer: Option<usize>) -> Option<&'static mut u8> {
    let mut slab = Slab { items: [0; 4] };
    peer.and_then(|p| slab.get_mut(p))
}

fn call_mut_twice<'a, F: FnMut() -> &'a mut u8>(mut f: F) -> (&'a mut u8, &'a mut u8) {
    let a = f();
    let b = f();
    (a, b)
}

fn fn_mut_escape_rejected() {
    let mut x = 0u8;
    let (_a, _b) = call_mut_twice(|| &mut x);
}

fn fnonce_by_value_escape_rejected() {
    let x = 0u8;
    let r = Some(()).map(move |()| &x);
    if let Some(p) = r {
        let _ = *p;
    }
}

fn main() {}
"#;

    #[test]
    fn closure_fnonce_env_and_soundness() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let home = std::env::var("HOME").expect("HOME is set");
        let tc = Toolchain::open(&mut DirShelf::new(std::format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
        let files = [("/cenv/src/main.rs".to_string(), SOURCE.to_string())];
        let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
        let user = UserCrate { name: "cenv", dir: "/cenv/", root: "/cenv/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, erased.bump()).expect("fixture names");
        let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
        let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front2");
        let bodies = world.bodies(alloc);
        let body = |name: &str| {
            bodies
                .iter()
                .find_map(|&(body, _)| {
                    let BodyRef::Free(key) = body else { return None };
                    (names.item_text(key).and_then(|item| item.name) == Some(name)).then_some(body)
                })
                .unwrap_or_else(|| panic!("missing fixture body {name}"))
        };

        for name in ["fnonce_and_then_mut_capture", "fnonce_projected_field_mut_capture"] {
            let (types, borrows) = world.check_borrows(body(name), alloc);
            assert!(types.is_clean(), "{name} types: {types:?}");
            assert!(borrows.is_clean(), "{name} borrows: {borrows:?}");
        }

        // CONTROL (rule 23): without closure requirements, `fnonce_and_then_mut_capture` falls
        // back to `NestedOutlives`.
        let (_, planted) = world.check_borrows_planted(body("fnonce_and_then_mut_capture"), FactsPlant::NoClosureRequirements, alloc);
        assert!(matches!(planted.refused, Some(BorrowRefused::NestedOutlives { .. })), "CONTROL: {planted:?}");

        // NEGATIVE 1: owner still enforces mutable borrow lifetime of the captured place.
        let (types, aliasing) = world.check_borrows(body("fnonce_aliasing_conflict_rejected"), alloc);
        assert!(types.is_clean(), "aliasing types: {types:?}");
        assert!(aliasing.refused.is_none(), "aliasing refused: {aliasing:?}");
        assert!(
            aliasing.findings.iter().any(|d| d.code == "E0503" || d.code == "E0502"),
            "aliasing must be rejected as E0503/E0502: {aliasing:?}"
        );

        // NEGATIVE 2: owner rejects returning a borrow of a local slab through `Option::and_then`.
        let (types, dangling) = world.check_borrows(body("fnonce_dangling_return_rejected"), alloc);
        assert!(types.is_clean(), "dangling types: {types:?}");
        assert!(dangling.refused.is_none(), "dangling refused: {dangling:?}");
        assert!(
            dangling.findings.iter().any(|d| d.code == "E0515" || d.code == "E0597"),
            "dangling must be rejected as E0515/E0597: {dangling:?}"
        );

        // NEGATIVE 3: `FnMut` closure cannot return `&mut` reborrowed from `&mut self`.
        let (types, fn_mut) = world.check_borrows(body("fn_mut_escape_rejected"), alloc);
        assert!(types.is_clean(), "fn_mut types: {types:?}");
        assert!(fn_mut.refused.is_none(), "fn_mut refused: {fn_mut:?}");
        assert!(
            fn_mut.findings.iter().any(|d| d.code == "E0521"),
            "fn_mut escape must be rejected as E0521: {fn_mut:?}"
        );

        // NEGATIVE 4: `ByValue` capture in a `move` `FnOnce` closure cannot return a reference
        // to the closure-owned variable (lowered as `FnOnce::call_once(self)`, so returning `&x`
        // is rustc's exact E0515 dangling local reference error).
        let (types, by_val) = world.check_borrows(body("fnonce_by_value_escape_rejected"), alloc);
        assert!(types.is_clean(), "by_val types: {types:?}");
        assert!(by_val.refused.is_none(), "by_val refused: {by_val:?}");
        assert!(
            by_val.findings.iter().any(|d| d.code == "E0515"),
            "by_val escape must be rejected as E0515: {by_val:?}"
        );
    }
}

/// `[NotLaid/DynRegion/Input]` & `[NotLaid/DynRegion/Output]`: trait objects whose principal
/// trait carries lifetime parameters (`&mut dyn Store<'_>`, `&dyn Store<'m>`,
/// `Box<dyn Store<'m>>`, `&mut dyn StoreAssoc<'m, Out = &'a u8>`) lay both the object's
/// default lifetime bound (RFC 599/1156) and the trait's own lifetime arguments soundly.
#[cfg(test)]
mod dyn_region_tests {
    use crate::crates::{DirShelf, Inputs, Toolchain, UserCrate};
    use crate::front2::project::{BodyRef, ProjectWorld};
    use crate::refusal::SourceFile;

    const SOURCE: &str = r#"
trait Store<'m> {
    fn get(&self) -> &'m u8;
}

struct MemStore<'m>(&'m u8);

impl<'m> Store<'m> for MemStore<'m> {
    fn get(&self) -> &'m u8 {
        self.0
    }
}

trait StoreAssoc<'m> {
    type Out;
    fn take(&mut self) -> Self::Out;
}

fn tell_public(store: &mut dyn Store<'_>) -> u8 {
    *store.get()
}

fn run_snapshot(store: &dyn Store<'_>) -> u8 {
    *store.get()
}

fn read_store<'m>(store: &dyn Store<'m>) -> &'m u8 {
    store.get()
}

fn take_box<'m>(store: Box<dyn Store<'m>>) -> &'m u8 {
    store.get()
}

fn keep_store<'a, 'm>(store: &'a dyn Store<'m>) -> &'a dyn Store<'m> {
    store
}

fn use_assoc<'m, 'a>(store: &mut dyn StoreAssoc<'m, Out = &'a u8>) -> &'a u8 {
    store.take()
}

fn dyn_store_named_lifetime_ok<'a, 'm>(store: &'a dyn Store<'m>) -> &'m u8 {
    let r = read_store(store);
    r
}

fn dyn_store_dangling_trait_lifetime_rejected<'a, 'm, 'b>(store: &'a dyn Store<'m>) -> &'b u8 {
    read_store(store)
}

fn dyn_store_object_default_dangling_rejected<'a, 'm>(store: &'a dyn Store<'m>) -> &'m dyn Store<'m> {
    keep_store(store)
}

fn main() {}
"#;

    #[test]
    fn dyn_trait_lifetimes_and_defaults_soundness() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let home = std::env::var("HOME").expect("HOME is set");
        let tc = Toolchain::open(&mut DirShelf::new(std::format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
        let files = [("/dynr/src/main.rs".to_string(), SOURCE.to_string())];
        let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
        let user = UserCrate { name: "dynr", dir: "/dynr/", root: "/dynr/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, erased.bump()).expect("fixture names");
        let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
        let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front2");
        let bodies = world.bodies(alloc);
        let body = |name: &str| {
            bodies
                .iter()
                .find_map(|&(body, _)| {
                    let BodyRef::Free(key) = body else { return None };
                    (names.item_text(key).and_then(|item| item.name) == Some(name)).then_some(body)
                })
                .unwrap_or_else(|| panic!("missing fixture body {name}"))
        };

        for name in [
            "tell_public",
            "run_snapshot",
            "read_store",
            "take_box",
            "keep_store",
            "use_assoc",
            "dyn_store_named_lifetime_ok",
        ] {
            let (types, borrows) = world.check_borrows(body(name), alloc);
            assert!(types.is_clean(), "{name} types: {types:?}");
            assert!(borrows.is_clean(), "{name} borrows: {borrows:?}");
        }

        // NEGATIVE 1: trait lifetime `'m` of `&'a dyn Store<'m>` cannot satisfy unrelated `'a`.
        let (types, bad_trait_lt) = world.check_borrows(body("dyn_store_dangling_trait_lifetime_rejected"), alloc);
        assert!(types.is_clean(), "bad_trait_lt types: {types:?}");
        assert!(bad_trait_lt.refused.is_none(), "bad_trait_lt refused: {bad_trait_lt:?}");
        assert!(
            !bad_trait_lt.findings.is_empty(),
            "unrelated trait lifetime `'m` -> `'a` must be rejected: {bad_trait_lt:?}"
        );

        // NEGATIVE 2: default object lifetime `'a` of `&'a dyn Store<'m>` cannot satisfy unrelated `'m`.
        let (types, bad_obj_lt) = world.check_borrows(body("dyn_store_object_default_dangling_rejected"), alloc);
        assert!(types.is_clean(), "bad_obj_lt types: {types:?}");
        assert!(bad_obj_lt.refused.is_none(), "bad_obj_lt refused: {bad_obj_lt:?}");
        assert!(
            !bad_obj_lt.findings.is_empty(),
            "unrelated object lifetime `'a` -> `'m` must be rejected: {bad_obj_lt:?}"
        );
    }
}

/// `[NotLaid/DefaultArg]`: omitted defaulted type arguments whose defaults mention lifetimes
/// (`Records<'r, 'n, S = &'n Names<'n>>`, `Def<T = &'static u8>`, chained defaults, type aliases
/// with lifetime defaults, fn-pointer defaults) lay their exact region slots in `PNode::Adt`
/// order as `Slot::quiet` while preserving Rust soundness and elision rules.
#[cfg(test)]
mod default_arg_region_tests {
    use crate::crates::{DirShelf, Inputs, Toolchain, UserCrate};
    use crate::front2::project::{BodyRef, ProjectWorld};
    use crate::refusal::SourceFile;

    const SOURCE: &str = r#"
struct Names<'n>(&'n u8);

struct Records<'r, 'n, S = &'n Names<'n>> {
    r: &'r u8,
    s: S,
}

struct Def<T = &'static u8>(T);

struct Chain<'a, A = &'a u8, B = (A, &'a u8)>(A, B);

struct FnDef<F = for<'x> fn(&'x u8) -> &'x u8>(F);

type AliasRec<'r, 'n, S = &'n Names<'n>> = Records<'r, 'n, S>;

fn id<'x>(x: &'x u8) -> &'x u8 {
    x
}

fn records_elided(r: &Records<'_, '_>) -> u8 {
    *r.r + *r.s.0
}

fn records_named<'r, 'n>(r: &'r Records<'r, 'n>) -> &'n u8 {
    r.s.0
}

fn def_static_quiet(x: &u8, _y: Def) -> &u8 {
    x
}

fn def_in_option(x: &u8, _y: Option<Def>) -> &u8 {
    x
}

fn chain_ok<'a>(c: Chain<'a>) -> &'a u8 {
    c.1.1
}

fn alias_rec_ok<'r, 'n>(r: &'r AliasRec<'r, 'n>) -> &'n u8 {
    r.s.0
}

fn fn_def_ok<'a>(f: FnDef, x: &'a u8) -> &'a u8 {
    (f.0)(x)
}

fn calls_all_defaults() {
    let a = 1u8;
    let b = 2u8;
    let names = Names(&b);
    let rec = Records { r: &a, s: &names };
    let _ = records_elided(&rec);
    let n = records_named(&rec);
    let _ = *n;
    let d = def_static_quiet(&a, Def(&0));
    let _ = *d;
    let o = def_in_option(&a, Some(Def(&0)));
    let _ = *o;
    let c = chain_ok(Chain(&a, (&a, &a)));
    let _ = *c;
    let ar = alias_rec_ok(&rec);
    let _ = *ar;
    let fr = fn_def_ok(FnDef(id), &a);
    let _ = *fr;
}

fn records_default_static_rejected<'r, 'n>(r: &'r Records<'r, 'n>) -> &'static u8 {
    r.s.0
}

fn records_default_wrong_lt_rejected<'r, 'n>(r: Records<'r, 'n>) -> &'r u8 {
    r.s.0
}

fn records_local_dangling_rejected<'a>() -> &'a u8 {
    let a = 1u8;
    let b = 2u8;
    let names = Names(&b);
    let rec = Records { r: &a, s: &names };
    records_named(&rec)
}

fn chain_wrong_lt_rejected<'a, 'b>(c: Chain<'a>) -> &'b u8 {
    c.1.1
}

fn def_elision_rejected(d: Def) -> &u8 {
    d.0
}

fn main() {}
"#;

    #[test]
    fn defaulted_type_arg_lifetimes_soundness() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let home = std::env::var("HOME").expect("HOME is set");
        let tc = Toolchain::open(&mut DirShelf::new(std::format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
        let files = [("/defarg/src/main.rs".to_string(), SOURCE.to_string())];
        let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
        let user = UserCrate { name: "defarg", dir: "/defarg/", root: "/defarg/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, erased.bump()).expect("fixture names");
        let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
        let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front2");
        let bodies = world.bodies(alloc);
        let body = |name: &str| {
            bodies
                .iter()
                .find_map(|&(body, _)| {
                    let BodyRef::Free(key) = body else { return None };
                    (names.item_text(key).and_then(|item| item.name) == Some(name)).then_some(body)
                })
                .unwrap_or_else(|| panic!("missing fixture body {name}"))
        };

        for name in [
            "records_elided",
            "records_named",
            "def_static_quiet",
            "def_in_option",
            "chain_ok",
            "alias_rec_ok",
            "fn_def_ok",
            "calls_all_defaults",
        ] {
            let (types, borrows) = world.check_borrows(body(name), alloc);
            assert!(types.is_clean(), "{name} types: {types:?}");
            assert!(borrows.is_clean(), "{name} borrows: {borrows:?}");
        }

        // NEGATIVE 1-4: defaulted lifetime arguments enforce exact outlives and dangling checks.
        for name in [
            "records_default_static_rejected",
            "records_default_wrong_lt_rejected",
            "records_local_dangling_rejected",
            "chain_wrong_lt_rejected",
        ] {
            let (types, bad) = world.check_borrows(body(name), alloc);
            assert!(types.is_clean(), "{name} types: {types:?}");
            assert!(bad.refused.is_none(), "{name} must not refuse: {bad:?}");
            assert!(!bad.findings.is_empty(), "{name} must be rejected: {bad:?}");
        }

        // NEGATIVE 5: defaulted `'static` in `Def` is `quiet` and does NOT satisfy output elision (E0106).
        let (elision_types, _) = world.check_borrows(body("def_elision_rejected"), alloc);
        let elision_text = std::format!("{elision_types:?}");
        assert!(!elision_types.is_clean(), "def_elision_rejected must fail E0106: {elision_text}");
        assert!(elision_text.contains("E0106"), "def_elision_rejected must report E0106: {elision_text}");
    }

    #[test]
    fn closure_default_arg_regions_soundness() {
        const SRC: &str = r#"
struct Names<'n>(&'n u8);
trait NamesSource<'n> {}
impl<'n> NamesSource<'n> for &'n Names<'n> {}

struct Records<'r, 'n, S = &'n Names<'n>> {
    r: &'r u8,
    idx: &'r mut Names<'n>,
    s: S,
}

fn refused<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>) -> u8 {
    *r.r
}

fn equate_inv<'a>(_a: &mut &'a u8, _b: &mut &'a u8) {}
fn require_static(_x: &'static u8) {}

// POSITIVE: `|r: &Records<'_, '_>|` ties `'n` and defaulted `S = &'n Names<'n>` via shared
// `WrittenRegion::Elided`, so calling `refused<'n, S: NamesSource<'n>>(r)` checks cleanly.
fn closure_records_refused_ok() {
    let refuse = |r: &Records<'_, '_>| refused(r);
    let a = 1u8;
    let b = 2u8;
    let n = Names(&b);
    let mut n2 = Names(&b);
    let rec = Records { r: &a, idx: &mut n2, s: &n };
    let _ = refuse(&rec);
}

// POSITIVE: body never names `Names` directly — only via `Records`'s defaulted `S`.
fn closure_records_no_names_mentioned_ok(r: &Records<'_, '_>) -> u8 {
    let refuse = |x: &Records<'_, '_>| refused(x);
    refuse(r)
}

// NEGATIVE 1: explicitly writing distinct `'_` lifetimes for `'n` and `S = &'_ Names<'_>`
// leaves them independent late regions, so calling `refused` must still be refused `NestedOutlives`.
fn closure_records_distinct_elided_rejected() {
    let refuse = |r: &Records<'_, '_, &'_ Names<'_>>| refused(r);
    let a = 1u8;
    let b = 2u8;
    let n = Names(&b);
    let mut n2 = Names(&b);
    let rec = Records { r: &a, idx: &mut n2, s: &n };
    let _ = refuse(&rec);
}

// NEGATIVE 2: equating two genuinely independent late-bound closure parameters must not pass.
fn closure_independent_late_equated_rejected() {
    let f = |a: &mut &u8, b: &mut &u8| equate_inv(a, b);
    let x = 1u8;
    let y = 2u8;
    let mut rx = &x;
    let mut ry = &y;
    f(&mut rx, &mut ry);
}

// NEGATIVE 3: requiring a closure's defaulted `'n` reference to outlive `'static` must be rejected.
fn closure_records_return_static_rejected() {
    let _f = |r: &Records<'_, '_>| require_static(r.s.0);
}

fn main() {}
"#;
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let home = std::env::var("HOME").expect("HOME is set");
        let tc = Toolchain::open(&mut DirShelf::new(std::format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
        let files = [("/clrec/src/main.rs".to_string(), SRC.to_string())];
        let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
        let user = UserCrate { name: "clrec", dir: "/clrec/", root: "/clrec/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, erased.bump()).expect("fixture names");
        let sources = [SourceFile { path: "src/main.rs", text: SRC }];
        let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front2");
        let bodies = world.bodies(alloc);
        let body = |name: &str| {
            bodies
                .iter()
                .find_map(|&(body, _)| {
                    let BodyRef::Free(key) = body else { return None };
                    (names.item_text(key).and_then(|item| item.name) == Some(name)).then_some(body)
                })
                .unwrap_or_else(|| panic!("missing fixture body {name}"))
        };

        // POSITIVE tests must type-check and borrow-check cleanly.
        for name in ["closure_records_refused_ok", "closure_records_no_names_mentioned_ok"] {
            let (types, borrows) = world.check_borrows(body(name), alloc);
            assert!(types.is_clean(), "{name} types: {types:?}");
            assert!(borrows.is_clean(), "{name} borrows: {borrows:?}");
        }

        // NEGATIVE 1-2: genuinely independent late lifetimes must still fail `NestedOutlives(Late -> Late)`.
        for name in ["closure_records_distinct_elided_rejected", "closure_independent_late_equated_rejected"] {
            let (types, bad) = world.check_borrows(body(name), alloc);
            assert!(types.is_clean(), "{name} types: {types:?}");
            assert!(
                matches!(
                    bad.refused,
                    Some(super::BorrowRefused::NestedOutlives {
                        from: Some(blaze_flow_lower::External::Late),
                        into: Some(blaze_flow_lower::External::Late),
                        ..
                    })
                ),
                "{name} must refuse Late -> Late: {bad:?}"
            );
        }

        // NEGATIVE 3: returning a late-bound defaulted lifetime as `'static` must produce a borrow finding.
        let (types, bad_ret) = world.check_borrows(body("closure_records_return_static_rejected"), alloc);
        assert!(types.is_clean(), "closure_records_return_static_rejected types: {types:?}");
        assert!(bad_ret.refused.is_none(), "closure_records_return_static_rejected must not refuse: {bad_ret:?}");
        assert!(
            !bad_ret.findings.is_empty(),
            "closure_records_return_static_rejected must reject returning `'n` as `'static`: {bad_ret:?}"
        );
    }
}

/// `NestedOutlives { from: Capture | Param, into: Late }`: NLL closure region propagation through
/// late-bound regions (`std::thread::scope`'s `'env: 'scope`, and unbounded `for<'a>` propagating
/// to `'static`), preserving Rust soundness against local-borrow escapes, unbounded type params,
/// and unprovable `Late -> Late` constraints.
#[cfg(test)]
mod nested_late_outlives_tests {
    use super::{BorrowRefused, FactsPlant};
    use crate::crates::{DirShelf, Inputs, Toolchain, UserCrate};
    use crate::front2::project::{BodyRef, ProjectWorld};
    use crate::refusal::SourceFile;

    const SOURCE: &str = r#"
fn scope_mut_capture_ok(xs: &mut [u8], tag: &u8) {
    std::thread::scope(|scope| {
        for chunk in xs.chunks_mut(2) {
            scope.spawn(move || {
                for slot in chunk.iter_mut() {
                    *slot = *tag;
                }
            });
        }
    });
}

fn scope_generic_param_ok<A: Send>(first: impl FnOnce() -> A + Send) -> A {
    std::thread::scope(|scope| {
        let handle = scope.spawn(first);
        handle.join().unwrap()
    })
}

fn call_with_any<F>(f: F) -> u8
where
    F: for<'a> FnOnce(&'a u8) -> &'a u8,
{
    static X: u8 = 42;
    *f(&X)
}

static GLOBAL_BYTE: u8 = 7;

fn unbounded_late_static_capture_ok() -> u8 {
    let r: &'static u8 = &GLOBAL_BYTE;
    call_with_any(|_x| r)
}

fn unbounded_late_local_capture_rejected() -> u8 {
    let local = 1u8;
    let r = &local;
    call_with_any(|_x| r)
}

fn require_for_all<F>(f: F)
where
    F: for<'a> FnOnce(&mut &'a u8),
{
    static X: u8 = 0;
    let mut r: &u8 = &X;
    f(&mut r);
}

fn need_outlives<'a, T: 'a>(_: &mut &'a u8, _: T) {}

fn unbounded_late_param_rejected<T>(x: T) {
    require_for_all(move |a| {
        need_outlives(a, x);
    });
}

fn call_two_lates<F>(f: F) -> u8
where
    F: for<'a, 'b> FnOnce(&'a u8, &'b u8) -> &'a u8,
{
    static X: u8 = 1;
    let y = 2u8;
    *f(&X, &y)
}

fn late_to_late_rejected() -> u8 {
    call_two_lates(|_x, y| y)
}

struct FakeScope<'scope, 'env: 'scope> {
    _marker: core::marker::PhantomData<(&'scope mut &'scope (), &'env mut &'env ())>,
}

impl<'scope, 'env> FakeScope<'scope, 'env> {
    fn spawn<F: FnOnce() + 'scope>(&'scope self, _f: F) {}
}

fn with_fake_scope<'env, F>(_anchor: &mut &'env u8, f: F)
where
    F: for<'scope> FnOnce(&'scope FakeScope<'scope, 'env>),
{
    let s = FakeScope { _marker: core::marker::PhantomData };
    f(&s);
}

fn fake_scope_short_capture_rejected<'env>(anchor: &mut &'env u8) {
    let local = 1u8;
    let r = &local;
    with_fake_scope(anchor, |scope| {
        scope.spawn(move || {
            let _ = *r;
        });
    });
}

fn main() {}
"#;

    #[test]
    fn nested_outlives_into_late_propagation_soundness() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let home = std::env::var("HOME").expect("HOME is set");
        let tc = Toolchain::open(&mut DirShelf::new(std::format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
        let files = [("/nlate/src/main.rs".to_string(), SOURCE.to_string())];
        let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
        let user = UserCrate { name: "nlate", dir: "/nlate/", root: "/nlate/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, erased.bump()).expect("fixture names");
        let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
        let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front2");
        let bodies = world.bodies(alloc);
        let body = |name: &str| {
            bodies
                .iter()
                .find_map(|&(body, _)| {
                    let BodyRef::Free(key) = body else { return None };
                    (names.item_text(key).and_then(|item| item.name) == Some(name)).then_some(body)
                })
                .unwrap_or_else(|| panic!("missing fixture body {name}"))
        };

        for name in ["scope_mut_capture_ok", "scope_generic_param_ok", "unbounded_late_static_capture_ok"] {
            let (types, borrows) = world.check_borrows(body(name), alloc);
            assert!(types.is_clean(), "{name} types: {types:?}");
            assert!(borrows.is_clean(), "{name} borrows: {borrows:?}");
        }

        // CONTROL (rule 23): without closure requirements, `scope_mut_capture_ok` falls back to `NestedOutlives`.
        let (_, planted) = world.check_borrows_planted(body("scope_mut_capture_ok"), FactsPlant::NoClosureRequirements, alloc);
        assert!(matches!(planted.refused, Some(BorrowRefused::NestedOutlives { .. })), "CONTROL: {planted:?}");

        // NEGATIVE 1: unbounded `for<'a>` with local borrow capture propagates to `'static` and rejects (E0597).
        let (types, bad_local) = world.check_borrows(body("unbounded_late_local_capture_rejected"), alloc);
        assert!(types.is_clean(), "bad_local types: {types:?}");
        assert!(bad_local.refused.is_none(), "bad_local refused: {bad_local:?}");
        assert!(bad_local.findings.iter().any(|d| d.code == "E0597"), "bad_local must be E0597: {bad_local:?}");

        // NEGATIVE 2: unbounded `for<'a>` with generic param `T` propagates to `T: 'static` and rejects (E0310).
        let (types, bad_param) = world.check_borrows(body("unbounded_late_param_rejected"), alloc);
        assert!(types.is_clean(), "bad_param types: {types:?}");
        assert!(bad_param.refused.is_none(), "bad_param refused: {bad_param:?}");
        assert!(bad_param.findings.iter().any(|d| d.code == "E0310"), "bad_param must be E0310: {bad_param:?}");

        // NEGATIVE 3: `Late -> Late` (`'b: 'a`) with no lower bound must never be accepted.
        let (types, bad_late) = world.check_borrows(body("late_to_late_rejected"), alloc);
        assert!(types.is_clean(), "bad_late types: {types:?}");
        assert!(!bad_late.is_clean(), "late_to_late must never be accepted: {bad_late:?}");

        // NEGATIVE 4: explicit outer `'env` longer than local borrow rejects (E0597) after propagation.
        let (types, bad_scope) = world.check_borrows(body("fake_scope_short_capture_rejected"), alloc);
        assert!(types.is_clean(), "bad_scope types: {types:?}");
        assert!(bad_scope.refused.is_none(), "bad_scope refused: {bad_scope:?}");
        assert!(bad_scope.findings.iter().any(|d| d.code == "E0597"), "bad_scope must be E0597: {bad_scope:?}");
    }
}

/// Regression and soundness tests for closures taking `me: &Self` inside `impl<'a> T<'a>` and
/// `impl T<'_>` (`blaze-consteval/src/machine.rs:1996` and `blaze-native/src/layout.rs:1204`
/// shapes): `Self`'s container lifetimes are owner regions (`WrittenRegion::Named`), while the
/// outer `&` on `&Self` remains a per-call late region so returning `&me.tag` is still rejected
/// with E0521 and returning `'a` as `'static` is still rejected.
#[cfg(test)]
mod closure_self_param_region_tests {
    use crate::crates::{DirShelf, Inputs, Toolchain, UserCrate};
    use crate::front2::project::{BodyRef, ProjectWorld};
    use crate::refusal::SourceFile;

    const SOURCE: &str = r#"
struct Writer<'a> {
    bytes: &'a [u8],
    tag: u8,
}

impl<'a> Writer<'a> {
    fn slice(&self) -> &'a [u8] {
        self.bytes
    }

    fn far_ok(&mut self, opt: Option<&'a [u8]>) -> Result<&'a [u8], &'a [u8]> {
        let far = |me: &Self| -> &'a [u8] { me.slice() };
        opt.ok_or_else(|| far(self))
    }

    fn info_ok(&mut self) -> Result<u8, &'a [u8]> {
        let info = |m: &Self| -> Result<u8, &'a [u8]> {
            if m.tag == 0 { Err(m.slice()) } else { Ok(m.tag) }
        };
        info(self)
    }

    fn bad_escape_self_ref(&mut self) -> &'a u8 {
        let bad = |me: &Self| -> &'a u8 { &me.tag };
        bad(self)
    }

    fn bad_static_self(&mut self) -> &'static [u8] {
        let bad = |me: &Self| -> &'static [u8] { me.slice() };
        bad(self)
    }
}

impl Writer<'_> {
    fn elided_impl_ok(&mut self) -> u8 {
        let far = |me: &Self| me.slice();
        far(self).len() as u8
    }
}

fn main() {}
"#;

    #[test]
    fn closure_self_param_regions_and_soundness() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let home = std::env::var("HOME").expect("HOME is set");
        let tc = Toolchain::open(&mut DirShelf::new(std::format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
        let files = [("/cself/src/main.rs".to_string(), SOURCE.to_string())];
        let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
        let user = UserCrate { name: "cself", dir: "/cself/", root: "/cself/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, erased.bump()).expect("fixture names");
        let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
        let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front2");
        let bodies = world.bodies(alloc);
        let member = |method: &str| {
            bodies
                .iter()
                .find_map(|&(body, _)| {
                    let BodyRef::Member { name, .. } = body else { return None };
                    (name == method).then_some(body)
                })
                .unwrap_or_else(|| panic!("missing fixture method {method}"))
        };

        for name in ["far_ok", "info_ok", "elided_impl_ok"] {
            let (types, borrows) = world.check_borrows(member(name), alloc);
            assert!(types.is_clean(), "{name} types: {types:?}");
            assert!(borrows.is_clean(), "{name} borrows: {borrows:?}");
        }

        // NEGATIVE 1: outer `&` on `me: &Self` is still a per-call late region (E0521).
        let (types, bad_ref) = world.check_borrows(member("bad_escape_self_ref"), alloc);
        assert!(types.is_clean(), "bad_escape_self_ref types: {types:?}");
        assert!(bad_ref.refused.is_none(), "bad_escape_self_ref refused: {bad_ref:?}");
        assert!(
            bad_ref.findings.iter().any(|d| d.code == "E0521"),
            "bad_escape_self_ref must be E0521: {bad_ref:?}",
        );

        // NEGATIVE 2: `'a` inside `Self` must remain an owner lifetime, never `'static`.
        let (types, bad_static) = world.check_borrows(member("bad_static_self"), alloc);
        assert!(types.is_clean(), "bad_static_self types: {types:?}");
        assert!(!bad_static.is_clean(), "bad_static_self must be rejected: {bad_static:?}");
    }
}

/// Regression and soundness tests for:
/// 1. Blanket impls whose associated type projects through an impl parameter's lifetime-carrying
///    bound (`impl<'de, A: ?Sized + SeqAccess<'de>> SeqAccess<'de> for &mut A { type Error =
///    A::Error; }`, `crates/blaze-protocol/src/cbor/de.rs::deserialize_any` shape): `heads.rs`
///    lays `impl_assoc` with `impl_given` (`param_bounds`) so the impl's `HeadRegions` are filed
///    instead of refusing with `HeadUnread`.
/// 2. `FnOnce`-only consumers of `move` closures with `ByValue` `&mut` captures
///    (`crates/blaze-cli/src/store/take.rs:826` `scope.may_overlap` shape): `borrows.rs` classifies
///    closure env usage BEFORE lowering and lowers `FnOnceOnly` closures via
///    `lower_closure_as_fn_once`, while rejecting returning a reference to the owned capture local
///    and rejecting `FnMut` escapes (E0521).
#[cfg(test)]
mod wave4_front2_soundness_tests {
    use crate::crates::{DirShelf, Inputs, Toolchain, UserCrate};
    use crate::front2::project::{BodyRef, ProjectWorld};
    use crate::refusal::SourceFile;

    const SOURCE: &str = r#"
// --- 1. Blanket impl assoc type projecting through lifetime-carrying param bound (cbor/de.rs) ---
pub trait SeqAccess<'de> {
    type Error;
    fn next_element(&mut self) -> Result<Option<&'de [u8]>, Self::Error>;
}

impl<'de, A> SeqAccess<'de> for &mut A
where
    A: ?Sized + SeqAccess<'de>,
{
    type Error = A::Error;
    fn next_element(&mut self) -> Result<Option<&'de [u8]>, Self::Error> {
        (**self).next_element()
    }
}

pub struct Items<'a, 'de> {
    pub bytes: &'a &'de [u8],
}

impl<'de> SeqAccess<'de> for Items<'_, 'de> {
    type Error = u32;
    fn next_element(&mut self) -> Result<Option<&'de [u8]>, u32> {
        Ok(Some(*self.bytes))
    }
}

pub trait Visitor<'de> {
    type Value;
    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error>;
}

pub fn cbor_visit_seq_ok<'de, V: Visitor<'de>>(bytes: &'de [u8], visitor: V) -> Result<V::Value, u32> {
    let mut items = Items { bytes: &bytes };
    visitor.visit_seq(&mut items)
}

pub struct BorrowVisitor;
impl<'de> Visitor<'de> for BorrowVisitor {
    type Value = Option<&'de [u8]>;
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        seq.next_element()
    }
}

// NEGATIVE: `'de` flows through `&mut Items<'_, 'de>`'s `SeqAccess<'de>` impl and must NOT unify with `'static`.
pub fn cbor_visit_seq_bad_static<'de>(bytes: &'de [u8]) -> Result<Option<&'static [u8]>, u32> {
    let mut items = Items { bytes: &bytes };
    BorrowVisitor.visit_seq(&mut items)
}

// --- 2. FnOnceOnly + ByValue `&mut` capture (`store/take.rs:826` `may_overlap` shape) ---
pub struct Cx;
pub struct Scope<'a>(core::marker::PhantomData<&'a ()>);
impl<'a> Scope<'a> {
    pub fn may_overlap<T: Send, E>(
        &self,
        _name: &'static str,
        task: impl FnOnce(&Cx) -> Result<T, E> + Send,
    ) -> Result<T, E> {
        let cx = Cx;
        task(&cx)
    }

    pub fn repeat_mut<T, E>(
        &self,
        mut task: impl FnMut(&Cx) -> Result<T, E>,
    ) -> Result<T, E> {
        let cx = Cx;
        task(&cx)
    }
}

fn fetch_chunk<'b>(_cx: &Cx, bufs_chunk: &'b mut [u8]) -> &'b mut [u8] {
    bufs_chunk
}

pub fn may_overlap_by_value_mut_ok<'b>(scope: &Scope<'_>, bufs_chunk: &'b mut [u8]) -> &'b mut [u8] {
    let res = scope.may_overlap("take-lan-segz", move |worker_cx| -> Result<_, core::convert::Infallible> {
        Ok(fetch_chunk(worker_cx, bufs_chunk))
    });
    match res {
        Ok(s) => s,
        Err(e) => match e {},
    }
}

// NEGATIVE 1: returning a reference to the owned `bufs_chunk` capture local itself (not reborrowing
// through `&'b mut [u8]`) MUST still be rejected under `lower_closure_as_fn_once`.
pub fn may_overlap_by_value_local_ref_bad<'b>(scope: &Scope<'_>, bufs_chunk: &'b mut [u8]) -> &'b [u8] {
    let mut owned = [1u8; 4];
    let _ = bufs_chunk;
    let res = scope.may_overlap("bad-local", move |_worker_cx| -> Result<&'b [u8], core::convert::Infallible> {
        owned[0] = 2;
        Ok(&owned[..])
    });
    match res {
        Ok(s) => s,
        Err(e) => match e {},
    }
}

// NEGATIVE 2: under an `FnMut` consumer, escaping a reborrow of a `ByValue` `&mut` capture MUST
// still be rejected with E0521 (cannot reborrow `bufs_chunk` across multiple `FnMut` calls).
pub fn fn_mut_by_value_escape_bad<'b>(scope: &Scope<'_>, bufs_chunk: &'b mut [u8]) -> &'b mut [u8] {
    let res = scope.repeat_mut(move |worker_cx| -> Result<_, core::convert::Infallible> {
        Ok(fetch_chunk(worker_cx, bufs_chunk))
    });
    match res {
        Ok(s) => s,
        Err(e) => match e {},
    }
}

fn main() {}
"#;

    #[test]
    fn impl_assoc_param_bounds_and_fn_once_by_value_soundness() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let home = std::env::var("HOME").expect("HOME is set");
        let tc = Toolchain::open(&mut DirShelf::new(std::format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
        let files = [("/w4sound/src/main.rs".to_string(), SOURCE.to_string())];
        let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
        let user = UserCrate { name: "w4sound", dir: "/w4sound/", root: "/w4sound/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, erased.bump()).expect("fixture names");
        let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
        let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front2");
        let bodies = world.bodies(alloc);
        let body = |name: &str| {
            bodies
                .iter()
                .find_map(|&(body, _)| {
                    let BodyRef::Free(key) = body else { return None };
                    (names.item_text(key).and_then(|item| item.name) == Some(name)).then_some(body)
                })
                .unwrap_or_else(|| panic!("missing fixture body {name}"))
        };

        // POSITIVE 1: `visitor.visit_seq(&mut items)` selects blanket `&mut A: SeqAccess<'de>` cleanly.
        let (types, borrows) = world.check_borrows(body("cbor_visit_seq_ok"), alloc);
        assert!(types.is_clean(), "cbor_visit_seq_ok types: {types:?}");
        assert!(borrows.is_clean(), "cbor_visit_seq_ok borrows: {borrows:?}");

        // NEGATIVE 1: `'de` through `&mut A: SeqAccess<'de>` must not outlive `'static`.
        let (types, bad_static) = world.check_borrows(body("cbor_visit_seq_bad_static"), alloc);
        assert!(types.is_clean(), "cbor_visit_seq_bad_static types: {types:?}");
        assert!(bad_static.refused.is_none(), "cbor_visit_seq_bad_static refused: {bad_static:?}");
        assert!(!bad_static.findings.is_empty(), "cbor_visit_seq_bad_static must be rejected: {bad_static:?}");

        // POSITIVE 2: `scope.may_overlap(..., move |cx| Ok(fetch_chunk(cx, bufs_chunk)))` passes clean.
        let (types, borrows) = world.check_borrows(body("may_overlap_by_value_mut_ok"), alloc);
        assert!(types.is_clean(), "may_overlap_by_value_mut_ok types: {types:?}");
        assert!(borrows.is_clean(), "may_overlap_by_value_mut_ok borrows: {borrows:?}");

        // NEGATIVE 2: returning a borrow of an owned `move`-captured local MUST still be rejected.
        let (types, bad_local) = world.check_borrows(body("may_overlap_by_value_local_ref_bad"), alloc);
        assert!(types.is_clean(), "may_overlap_by_value_local_ref_bad types: {types:?}");
        assert!(bad_local.refused.is_none(), "may_overlap_by_value_local_ref_bad refused: {bad_local:?}");
        assert!(!bad_local.findings.is_empty(), "may_overlap_by_value_local_ref_bad must be rejected: {bad_local:?}");

        // NEGATIVE 3: under `FnMut`, escaping a reborrow of a `ByValue` `&mut` capture stays E0521.
        let (types, bad_fn_mut) = world.check_borrows(body("fn_mut_by_value_escape_bad"), alloc);
        assert!(types.is_clean(), "fn_mut_by_value_escape_bad types: {types:?}");
        assert!(bad_fn_mut.refused.is_none(), "fn_mut_by_value_escape_bad refused: {bad_fn_mut:?}");
        assert!(
            bad_fn_mut.findings.iter().any(|d| d.code == "E0521"),
            "fn_mut_by_value_escape_bad must be E0521: {bad_fn_mut:?}",
        );
    }
}

/// Regression and soundness tests for Wave 5 `front2` fixes:
/// 1. Instantiated `PNode::Alias` structural drop-glue evaluator (`inst_needs_drop`):
///    - `Split<'_, &str>`, `SplitWhitespace<'_>`, and `Flatten<Take<slice::Iter<'_, Option<&T>>>>`
///      prove `needs_drop == false`.
///    - Negative soundness controls: `Flatten<vec::IntoIter<Vec<DropGuard>>>`, `Flatten<option::IntoIter<DropGuard<'_>>>`,
///      custom `Drop` searcher, and generic `P` MUST still require drop and reject conflicting mutations.
/// 2. `FieldRegionsUnaligned` on ADT fields holding `PNode::Alias` (`run_parent.rs::lowering_counts` shape).
/// 3. Parameterized supertrait outlives propagation (`L: LoadItems<'a>` where `trait LoadItems<'a>: 'a`,
///    `items_record.rs::{read, record}` shape), plus negative controls without `: 'a` or requiring `'static`.
/// 4. Default trait object lifetimes in zero-lifetime type aliases (`type ArcAlias<T> = Arc<T>;
///    ArcAlias<dyn Store>`, `h3/config.rs::client` shape), plus negative controls proving `'static`
///    and refusing lifetime-parameterized aliases.
#[cfg(test)]
mod wave5_front2_soundness_tests {
    use crate::crates::{DirShelf, Inputs, Toolchain, UserCrate};
    use crate::front2::project::{BodyRef, ProjectWorld};
    use crate::refusal::SourceFile;

    const SOURCE: &str = r#"
#![feature(try_as_dyn, type_info)]
extern crate alloc;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::TypeId;
use core::marker::PhantomData;

// --- 1. Instantiated PNode::Alias structural drop-glue (`inst_needs_drop`) ---

pub fn split_str_no_drop_ok(inbox: &mut Vec<u8>) -> usize {
    let head = core::str::from_utf8(&inbox[..]).unwrap_or("");
    let mut lines = head.split("\r\n");
    let first_len = lines.next().unwrap_or("").len();
    // `lines` is still in scope until end of function; mutating `inbox` is valid ONLY because
    // `Split<'_, &str>` has no drop glue (`<&str as Pattern>::Searcher<'_> = StrSearcher`).
    inbox.push(0);
    first_len
}

pub fn split_whitespace_no_drop_ok(inbox: &mut Vec<u8>) -> usize {
    let head = core::str::from_utf8(&inbox[..]).unwrap_or("");
    let mut parts = head.split_whitespace();
    let first_len = parts.next().unwrap_or("").len();
    // `parts` (`SplitWhitespace<'_>`) has no drop glue.
    inbox.push(0);
    first_len
}

pub fn flatten_take_option_ref_no_drop_ok(items: &mut [Option<&u32>; 4], count: usize) {
    let target = 7u32;
    if !items.iter().take(count).flatten().any(|&&x| x == target) {
        if let Some(slot) = items.get_mut(count) {
            *slot = None;
        }
    }
}

pub struct DropGuard<'a> {
    slot: &'a mut u32,
}

impl Drop for DropGuard<'_> {
    fn drop(&mut self) {
        *self.slot += 1;
    }
}

// NEGATIVE 1: `Flatten<option::IntoIter<DropGuard<'_>>>` drops `DropGuard<'_>` at scope exit,
// so mutating `*x` while `flat` is still in scope MUST be rejected.
pub fn flatten_dropping_inner_bad(x: &mut u32) {
    let flat = Some(Some(DropGuard { slot: x })).into_iter().flatten();
    *x = 99;
    let _ = &flat;
}

// NEGATIVE 2: `Flatten<vec::IntoIter<Vec<DropGuard<'_>>>>` drops `Vec` and `DropGuard<'_>` at
// scope exit, so mutating `*x` before `flat` drops MUST be rejected.
pub fn flatten_vec_drop_bad(x: &mut u32) {
    let mut inner = Vec::new();
    inner.push(DropGuard { slot: x });
    let mut outer = Vec::new();
    outer.push(inner);
    let _flat = outer.into_iter().flatten();
    *x = 42;
}

pub trait MyPat<'a> {
    type Searcher;
    fn make(self, x: &'a mut u32) -> Self::Searcher;
}

pub struct MySplit<'a, P: MyPat<'a>> {
    searcher: P::Searcher,
}

fn make_my_split<'a, P: MyPat<'a>>(searcher: P::Searcher) -> MySplit<'a, P> {
    MySplit { searcher }
}

pub struct NoDropPat;
impl<'a> MyPat<'a> for NoDropPat {
    type Searcher = &'a u32;
    fn make(self, x: &'a mut u32) -> &'a u32 {
        &*x
    }
}

pub struct DropPat;
impl<'a> MyPat<'a> for DropPat {
    type Searcher = DropGuard<'a>;
    fn make(self, x: &'a mut u32) -> DropGuard<'a> {
        DropGuard { slot: x }
    }
}

pub fn custom_pat_no_drop_ok(x: &mut u32) {
    let _s = make_my_split::<'_, NoDropPat>(&*x);
    *x = 1;
}

// NEGATIVE 3: `MySplit<DropPat>` normalizes `Searcher` to `DropGuard<'a>` which implements `Drop`,
// so mutating `*x` while `_s` is in scope MUST be rejected.
pub fn custom_pat_drop_bad(x: &mut u32) {
    let _s = make_my_split::<'_, DropPat>(DropGuard { slot: x });
    *x = 1;
}

// NEGATIVE 4: generic `P: MyPat<'a>` cannot prove `needs_drop == false`, so borrowing `x` into
// `MySplit<P>` and mutating `*x` before `_s` drops MUST be rejected.
pub fn custom_pat_generic_bad<P: for<'a> MyPat<'a>>(x: &mut u32, p: P) {
    let _s = make_my_split::<'_, P>(p.make(x));
    *x = 1;
}

// --- 2. FieldRegionsUnaligned for PNode::Alias fields (`run_parent.rs::lowering_counts`) ---

pub trait Front {
    type Maker;
}

pub struct RunMaker<'h> {
    pub data: &'h u8,
}

impl<'h> RunMaker<'h> {
    pub fn count(&self) -> u8 {
        *self.data
    }
}

pub struct Heavy<M> {
    pub maker: M,
}

pub struct Started<'a, M> {
    pub heavy: Option<Heavy<M>>,
    pub _marker: PhantomData<&'a ()>,
}

pub struct Lazy<'a, F: Front> {
    pub started: Option<Started<'a, F::Maker>>,
}

impl<'a, F: Front> Lazy<'a, F> {
    pub fn lowering_counts_ok<'h>(&self) -> u8
    where
        F: Front<Maker = RunMaker<'h>>,
    {
        self.started
            .as_ref()
            .and_then(|s| s.heavy.as_ref())
            .map_or(0, |heavy| heavy.maker.count())
    }

    // NEGATIVE: returning `'h` as `'static` through `heavy.maker.data` MUST be rejected.
    pub fn lowering_counts_bad_static<'h>(&self) -> Option<&'static u8>
    where
        F: Front<Maker = RunMaker<'h>>,
    {
        self.started
            .as_ref()
            .and_then(|s| s.heavy.as_ref())
            .map(|heavy| heavy.maker.data)
    }
}

// --- 3. Parameterized supertrait outlives (`items_record.rs::{read, record}`) ---

pub trait LoadItems<'a>: 'a {
    fn file(&self) -> Option<&'a str>;
}

pub trait SubLoadItems<'a>: LoadItems<'a> {}

pub trait UnboundedLoad<'a> {
    fn file(&self) -> Option<&'a str>;
}

fn take_a<'a, F: Fn() -> Option<&'a str> + 'a>(f: F) -> Option<&'a str> {
    f()
}

fn take_static<F: Fn() -> usize + 'static>(f: F) -> usize {
    f()
}

pub struct RecordItems<'a, L> {
    load: Arc<L>,
    _marker: PhantomData<&'a ()>,
}

impl<'a, L: LoadItems<'a>> RecordItems<'a, L> {
    pub fn record_ok(&self) -> Option<&'a str> {
        let load = Arc::clone(&self.load);
        take_a(move || load.file())
    }

    // NEGATIVE: `L: LoadItems<'a>` implies `L: 'a`, NOT `L: 'static`.
    pub fn record_bad_static(&self) -> usize {
        let load = Arc::clone(&self.load);
        take_static(move || load.file().map_or(0, |s| s.len()))
    }
}

impl<'a, L: SubLoadItems<'a>> RecordItems<'a, L> {
    pub fn record_transitive_ok(&self) -> Option<&'a str> {
        let load = Arc::clone(&self.load);
        take_a(move || load.file())
    }
}

impl<'a, L: UnboundedLoad<'a>> RecordItems<'a, L> {
    // NEGATIVE: `UnboundedLoad<'a>` has NO `: 'a` supertrait, so `L: 'a` MUST NOT be accepted.
    pub fn record_unbounded_bad(&self) -> Option<&'a str> {
        let load = Arc::clone(&self.load);
        take_a(move || load.file())
    }
}

// --- 4. Default trait object lifetimes in zero-lifetime type aliases (`h3/config.rs::client`) ---

pub trait SessionStore: Send + Sync {}
pub struct OwnedStore;
impl SessionStore for OwnedStore {}
pub struct BorrowedStore<'a>(pub &'a u8);
impl SessionStore for BorrowedStore<'_> {}

pub type ArcAlias<T> = Arc<T>;
pub type RefAlias<'a, T> = &'a T;

pub fn store_via_alias(_store: ArcAlias<dyn SessionStore>) {}

pub fn alias_object_default_ok() {
    let s: Arc<OwnedStore> = Arc::new(OwnedStore);
    store_via_alias(s as Arc<dyn SessionStore>);
}

// NEGATIVE 1: `ArcAlias<dyn SessionStore>` defaults to `dyn SessionStore + 'static`, so passing
// a short-lived `Arc<dyn SessionStore + 'a>` into it MUST be rejected with an outlives error.
pub fn alias_object_default_short_bad<'a>(s: Arc<dyn SessionStore + 'a>) {
    store_via_alias(s);
}

// NEGATIVE 2: a type alias declaring a lifetime parameter (`RefAlias<'a, T>`) does NOT blindly
// default `dyn SessionStore` to `'static`.
pub fn alias_with_lifetime_refused<'a>(_r: RefAlias<'a, dyn SessionStore>) {}

// --- 5. `dyn T: TryAsDynCompatible<'a>` (`blaze-text/src/debug.rs::{form, reflect_of}`) ---

pub trait DynForm {
    fn tag(&self) -> u8;
}

pub fn trait_info_of_dyn_ok(id: TypeId) -> bool {
    id.trait_info_of::<dyn DynForm>().is_some()
}

pub fn try_as_dyn_same_lt_ok<'a, T: 'a>(x: &'a T) -> Option<&'a (dyn DynForm + 'a)> {
    core::any::try_as_dyn::<T, dyn DynForm + 'a>(x)
}

// NEGATIVE: `dyn DynForm + 'static: TryAsDynCompatible<'a>` must require `'a == 'static`, so
// turning `&'a u8` into `&'a (dyn DynForm + 'static)` MUST be rejected.
pub fn try_as_dyn_short_as_static_bad<'a>(x: &'a &'a u8) -> Option<&'a (dyn DynForm + 'static)> {
    core::any::try_as_dyn::<&'a u8, dyn DynForm + 'static>(x)
}

fn main() {}
"#;

    #[test]
    fn wave5_front2_blockers_and_soundness() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let home = std::env::var("HOME").expect("HOME is set");
        let tc = Toolchain::open(&mut DirShelf::new(std::format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
        let files = [("/w5sound/src/main.rs".to_string(), SOURCE.to_string())];
        let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
        let user = UserCrate { name: "w5sound", dir: "/w5sound/", root: "/w5sound/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, erased.bump()).expect("fixture names");
        let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
        let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front2");
        let bodies = world.bodies(alloc);
        let free = |name: &str| {
            bodies
                .iter()
                .find_map(|&(body, _)| {
                    let BodyRef::Free(key) = body else { return None };
                    (names.item_text(key).and_then(|item| item.name) == Some(name)).then_some(body)
                })
                .unwrap_or_else(|| panic!("missing fixture body {name}"))
        };
        let member = |method: &str| {
            bodies
                .iter()
                .find_map(|&(body, _)| {
                    let BodyRef::Member { name, .. } = body else { return None };
                    (name == method).then_some(body)
                })
                .unwrap_or_else(|| panic!("missing fixture method {method}"))
        };

        // 1. inst_needs_drop positives
        for name in [
            "split_str_no_drop_ok",
            "split_whitespace_no_drop_ok",
            "flatten_take_option_ref_no_drop_ok",
            "custom_pat_no_drop_ok",
        ] {
            let (types, borrows) = world.check_borrows(free(name), alloc);
            assert!(types.is_clean(), "{name} types: {types:?}");
            assert!(borrows.is_clean(), "{name} borrows: {borrows:?}");
        }

        // 1. inst_needs_drop negatives (MUST reject!)
        for name in [
            "flatten_dropping_inner_bad",
            "flatten_vec_drop_bad",
            "custom_pat_drop_bad",
            "custom_pat_generic_bad",
        ] {
            let (types, borrows) = world.check_borrows(free(name), alloc);
            assert!(types.is_clean(), "{name} types: {types:?}");
            assert!(borrows.refused.is_none(), "{name} refused: {borrows:?}");
            assert!(!borrows.findings.is_empty(), "{name} must report a borrow error: {borrows:?}");
        }

        // 2. FieldRegionsUnaligned on PNode::Alias field
        {
            let (types, borrows) = world.check_borrows(member("lowering_counts_ok"), alloc);
            assert!(types.is_clean(), "lowering_counts_ok types: {types:?}");
            assert!(borrows.is_clean(), "lowering_counts_ok borrows: {borrows:?}");

            let (types, bad) = world.check_borrows(member("lowering_counts_bad_static"), alloc);
            assert!(types.is_clean(), "lowering_counts_bad_static types: {types:?}");
            assert!(!bad.is_clean(), "lowering_counts_bad_static must be rejected: {bad:?}");
        }

        // 3. Parameterized supertrait outlives (`LoadItems<'a>: 'a`)
        for name in ["record_ok", "record_transitive_ok"] {
            let (types, borrows) = world.check_borrows(member(name), alloc);
            assert!(types.is_clean(), "{name} types: {types:?}");
            assert!(borrows.is_clean(), "{name} borrows: {borrows:?}");
        }
        for name in ["record_bad_static", "record_unbounded_bad"] {
            let (types, bad) = world.check_borrows(member(name), alloc);
            assert!(types.is_clean(), "{name} types: {types:?}");
            assert!(!bad.is_clean(), "{name} must NOT be accepted: {bad:?}");
        }

        // 4. Default trait object lifetimes in zero-lifetime type aliases (`ArcAlias<dyn SessionStore>`)
        {
            let (types, borrows) = world.check_borrows(free("alias_object_default_ok"), alloc);
            assert!(types.is_clean(), "alias_object_default_ok types: {types:?}");
            assert!(borrows.is_clean(), "alias_object_default_ok borrows: {borrows:?}");

            let (types, bad) = world.check_borrows(free("alias_object_default_short_bad"), alloc);
            assert!(types.is_clean(), "alias_object_default_short_bad types: {types:?}");
            assert!(bad.refused.is_none(), "alias_object_default_short_bad refused: {bad:?}");
            assert!(!bad.findings.is_empty(), "alias_object_default_short_bad must reject short borrow: {bad:?}");

            let (types, refused) = world.check_borrows(free("alias_with_lifetime_refused"), alloc);
            assert!(types.is_clean(), "alias_with_lifetime_refused types: {types:?}");
            assert!(!refused.is_clean(), "alias_with_lifetime_refused must NOT silently default to 'static: {refused:?}");
        }

        // 5. `dyn T: TryAsDynCompatible<'a>` (`blaze-text/src/debug.rs`)
        {
            for name in ["trait_info_of_dyn_ok", "try_as_dyn_same_lt_ok"] {
                let (types, borrows) = world.check_borrows(free(name), alloc);
                assert!(types.is_clean(), "{name} types: {types:?}");
                assert!(borrows.is_clean(), "{name} borrows: {borrows:?}");
            }

            let (types, bad) = world.check_borrows(free("try_as_dyn_short_as_static_bad"), alloc);
            assert!(types.is_clean(), "try_as_dyn_short_as_static_bad types: {types:?}");
            assert!(bad.refused.is_none(), "try_as_dyn_short_as_static_bad refused: {bad:?}");
            assert!(!bad.findings.is_empty(), "try_as_dyn_short_as_static_bad must be rejected: {bad:?}");
        }
    }
}

/// Regression and soundness tests for Wave 6 `front2` fixes:
/// 1. `InstDropEval::select_impl` scaling past 64 candidates in large `Shape::Ref(Not)` buckets
///    (`crates/blaze-cli/src/engine_bodies.rs:4005` `to_open.iter().take(count).flatten().any(...)`
///    inside an `if ... && let Some(slot) = to_open.get_mut(count)` chain with 70+ `IntoIterator`
///    impls on `&T`), plus negative soundness controls proving dropping inners still reject.
/// 2. `PNode::Alias` in `field_row` + per-body `FieldAliasGuard`:
///    - Clears `Lazy<'r, 'a, F, D>` methods (`run_parent.rs`) and `Hit<'s, 'b, K>` (`proof.rs`)
///      whose `PNode::Alias` fields stay unnormalized.
///    - Negative soundness controls: swapping lifetimes across `&'r` / `&'a`, returning `&'b K::Root`
///      as `'s` / `'static`, concrete normalized alias swapping lifetimes (`Pair<'a, 'b>` ->
///      `(&'b u8, &'a u8)`), blanket-impl normalized alias, and `where F: Front<Maker = RunMaker<'h>>`
///      returning `'h` as `'static` MUST ALL be rejected!
#[cfg(test)]
mod wave6_front2_soundness_tests {
    use crate::crates::{DirShelf, Inputs, Toolchain, UserCrate};
    use crate::front2::project::{BodyRef, ProjectWorld};
    use crate::refusal::SourceFile;

    const SOURCE: &str = r#"
extern crate alloc;
use alloc::vec::Vec;
use core::marker::PhantomData;

// --- 1. Exact `engine_bodies.rs:4005` if-let chain with >64 `IntoIterator for &` impls ---
pub struct Unopened {
    pub world_at: u32,
}

macro_rules! dummy_into_iters {
    ($($name:ident),*) => {
        $(
            pub struct $name;
            impl<'a> IntoIterator for &'a $name {
                type Item = &'a u8;
                type IntoIter = core::slice::Iter<'a, u8>;
                fn into_iter(self) -> Self::IntoIter {
                    [].iter()
                }
            }
        )*
    };
}

dummy_into_iters!(
    D00, D01, D02, D03, D04, D05, D06, D07, D08, D09,
    D10, D11, D12, D13, D14, D15, D16, D17, D18, D19,
    D20, D21, D22, D23, D24, D25, D26, D27, D28, D29,
    D30, D31, D32, D33, D34, D35, D36, D37, D38, D39,
    D40, D41, D42, D43, D44, D45, D46, D47, D48, D49,
    D50, D51, D52, D53, D54, D55, D56, D57, D58, D59,
    D60, D61, D62, D63, D64, D65, D66, D67, D68, D69
);

pub fn pre_open_chain_ok(rest: &[Unopened], places: &[usize]) {
    let mut to_open: [Option<&Unopened>; 8] = [None; 8];
    let mut count = 0usize;
    for &place in places {
        if place > 0
            && let Some(unopened) = rest.get(place - 1)
            && !to_open.iter().take(count).flatten().any(|prev| prev.world_at == unopened.world_at)
            && let Some(slot) = to_open.get_mut(count)
        {
            *slot = Some(unopened);
            count += 1;
        }
    }
}

pub struct DropGuard<'a> {
    slot: &'a mut u32,
}

impl Drop for DropGuard<'_> {
    fn drop(&mut self) {
        *self.slot += 1;
    }
}

// NEGATIVE: `Flatten<Take<vec::IntoIter<Option<DropGuard<'_>>>>>` MUST still require drop even
// with 70+ `IntoIterator` impls in scope, rejecting conflicting mutation before scope exit.
pub fn flatten_take_vec_drop_bad(x: &mut u32, count: usize) {
    let mut v = Vec::new();
    v.push(Some(DropGuard { slot: x }));
    let _flat = v.into_iter().take(count).flatten();
    *x = 77;
}

// --- 2. Exact `run_parent.rs` `Lazy<'r, 'a, F, D>` (2 lifetimes + PNode::Alias field) ---
pub trait Front {
    type Start;
    type Ids;
    type Maker;
}

pub struct RunMaker<'h> {
    pub data: &'h u8,
}

impl<'h> RunMaker<'h> {
    pub fn count(&self) -> u8 {
        *self.data
    }
}

pub struct Heavy<I, M> {
    pub ids: I,
    pub maker: M,
}

pub struct Started<'a, S, I, M> {
    pub start: S,
    pub heavy: Option<Heavy<I, M>>,
    pub _marker: PhantomData<&'a ()>,
}

pub struct Lazy<'r, 'a, F: Front, D> {
    pub cx: &'r u8,
    pub front: F,
    pub folder: D,
    pub started: Option<Started<'a, F::Start, F::Ids, F::Maker>>,
}

impl<'r, 'a, F: Front, D> Lazy<'r, 'a, F, D> {
    pub fn is_cold_ok(&self) -> bool {
        self.started.is_none()
    }

    pub fn start_ok(&self) -> Option<&F::Start> {
        self.started.as_ref().map(|s| &s.start)
    }

    pub fn is_heavy_ok(&self) -> bool {
        self.started.as_ref().is_some_and(|s| s.heavy.is_some())
    }

    pub fn cx_ok(&self) -> &'r u8 {
        self.cx
    }

    // NEGATIVE: returning `'r` as `'a` across sibling fields of `Lazy` MUST be rejected.
    pub fn lazy_bad_r_as_a(&self) -> &'a u8 {
        self.cx
    }

    // NEGATIVE: returning `'h` as `'static` through normalized `F::Maker = RunMaker<'h>` MUST
    // be rejected (suppressed by `FieldAliasGuard`).
    pub fn lazy_lowering_bad_static<'h>(&self) -> Option<&'static u8>
    where
        F: Front<Maker = RunMaker<'h>>,
    {
        self.started
            .as_ref()
            .and_then(|s| s.heavy.as_ref())
            .map(|heavy| heavy.maker.data)
    }
}

// --- 3. Exact `proof.rs` `Hit<'s, 'b, K: Kind>` (2 lifetimes + `&'b K::Root`) ---
pub trait Kind {
    type Root;
}

pub trait HasPayload {
    fn payload(&self) -> usize;
}

#[derive(Clone, Copy)]
pub struct Brand<'s>(PhantomData<fn(&'s ()) -> &'s ()>);

#[derive(Clone, Copy)]
pub struct Holds<'s, K> {
    pub brand: Brand<'s>,
    pub _k: PhantomData<K>,
}

pub struct Hit<'s, 'b, K: Kind> {
    holds: Holds<'s, K>,
    root: &'b K::Root,
    bytes: &'b [u8],
}

pub fn make_hit<'s, 'b, K: Kind>(brand: Brand<'s>, root: &'b K::Root, bytes: &'b [u8]) -> Hit<'s, 'b, K> {
    Hit { holds: Holds { brand, _k: PhantomData }, root, bytes }
}

impl<'s, 'b, K: Kind> Hit<'s, 'b, K> {
    pub fn root_ok(&self) -> &'b K::Root {
        self.root
    }

    // NEGATIVE: returning `&'b K::Root` as `&'s K::Root` MUST be rejected.
    pub fn root_bad_s(&self) -> &'s K::Root {
        self.root
    }

    // NEGATIVE: returning `&'b K::Root` as `&'static K::Root` MUST be rejected.
    pub fn root_bad_static(&self) -> &'static K::Root {
        self.root
    }
}

impl<'s, 'b, K: Kind> Hit<'s, 'b, K>
where
    K::Root: HasPayload,
{
    pub fn payload_ok(&self) -> (&'b [u8], usize) {
        (self.bytes, self.root.payload())
    }
}

// --- 4. Soundness controls for normalized `PNode::Alias` fields ---
pub trait SwapTr {
    type Assoc;
}

pub struct Pair<'a, 'b>(pub &'a u8, pub &'b u8);

impl<'a, 'b> SwapTr for Pair<'a, 'b> {
    // Same slot count (2) as `Pair<'a, 'b>`, but SWAPPED order!
    type Assoc = (&'b u8, &'a u8);
}

pub struct AliasBox<T: SwapTr> {
    pub x: T::Assoc,
}

// NEGATIVE: `f.x.0` has type `&'b u8`, NOT `&'a u8`; unnormalized `[Arg(0)]` would map it to `'a`
// if `FieldAliasGuard` did not suppress concrete instantiations!
pub fn alias_swap_bad<'a, 'b>(f: AliasBox<Pair<'a, 'b>>) -> &'a u8 {
    f.x.0
}

pub trait BlanketTr {
    type Assoc;
}

impl<T> BlanketTr for T {
    type Assoc = &'static u8;
}

pub struct BlanketBox<'a, T: BlanketTr> {
    pub r: &'a u8,
    pub x: T::Assoc,
}

// NEGATIVE: `BlanketTr` has a blanket impl, so `FieldAliasGuard` suppresses `x` while keeping
// `r` sound; returning `r` (`&'a u8`) as `&'static u8` MUST be rejected.
pub fn alias_blanket_bad<'a, T: BlanketTr>(b: BlanketBox<'a, T>) -> &'static u8 {
    b.r
}

fn main() {}
"#;

    #[test]
    fn wave6_front2_blockers_and_soundness() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let home = std::env::var("HOME").expect("HOME is set");
        let tc = Toolchain::open(&mut DirShelf::new(std::format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
        let files = [("/w6sound/src/main.rs".to_string(), SOURCE.to_string())];
        let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
        let user = UserCrate { name: "w6sound", dir: "/w6sound/", root: "/w6sound/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, erased.bump()).expect("fixture names");
        let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
        let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front2");
        let bodies = world.bodies(alloc);
        let free = |name: &str| {
            bodies
                .iter()
                .find_map(|&(body, _)| {
                    let BodyRef::Free(key) = body else { return None };
                    (names.item_text(key).and_then(|item| item.name) == Some(name)).then_some(body)
                })
                .unwrap_or_else(|| panic!("missing fixture body {name}"))
        };
        let member = |method: &str| {
            bodies
                .iter()
                .find_map(|&(body, _)| {
                    let BodyRef::Member { name, .. } = body else { return None };
                    (name == method).then_some(body)
                })
                .unwrap_or_else(|| panic!("missing fixture method {method}"))
        };

        // 1. Positive: `engine_bodies.rs:4005` chain with 70+ `IntoIterator for &` impls + `proof.rs::hit`
        for name in ["pre_open_chain_ok", "make_hit"] {
            let (types, borrows) = world.check_borrows(free(name), alloc);
            assert!(types.is_clean(), "{name} types: {types:?}");
            assert!(borrows.is_clean(), "{name} borrows: {borrows:?}");
        }

        // 2. Positive: `Lazy<'r, 'a, F, D>` and `Hit<'s, 'b, K>` methods
        for name in ["is_cold_ok", "start_ok", "is_heavy_ok", "cx_ok", "root_ok", "payload_ok"] {
            let (types, borrows) = world.check_borrows(member(name), alloc);
            assert!(types.is_clean(), "{name} types: {types:?}");
            assert!(borrows.is_clean(), "{name} borrows: {borrows:?}");
        }

        // 3. Negative soundness controls (free functions): MUST be rejected!
        for name in ["flatten_take_vec_drop_bad", "alias_swap_bad", "alias_blanket_bad"] {
            let (types, bad) = world.check_borrows(free(name), alloc);
            assert!(types.is_clean(), "{name} types: {types:?}");
            assert!(!bad.is_clean(), "{name} must NOT be accepted: {bad:?}");
        }
        // Specifically verify `flatten_take_vec_drop_bad` produces a real borrow finding (not refusal).
        {
            let (_, bad) = world.check_borrows(free("flatten_take_vec_drop_bad"), alloc);
            assert!(bad.refused.is_none(), "flatten_take_vec_drop_bad refused: {bad:?}");
            assert!(!bad.findings.is_empty(), "flatten_take_vec_drop_bad must report borrow error: {bad:?}");
        }

        // 4. Negative soundness controls (methods): MUST be rejected!
        for name in ["lazy_bad_r_as_a", "lazy_lowering_bad_static", "root_bad_s", "root_bad_static"] {
            let (types, bad) = world.check_borrows(member(name), alloc);
            assert!(types.is_clean(), "{name} types: {types:?}");
            assert!(!bad.is_clean(), "{name} must NOT be accepted: {bad:?}");
        }
        // Specifically verify `lazy_bad_r_as_a`, `root_bad_s`, `root_bad_static` report exact findings.
        for name in ["lazy_bad_r_as_a", "root_bad_s", "root_bad_static"] {
            let (_, bad) = world.check_borrows(member(name), alloc);
            assert!(bad.refused.is_none(), "{name} refused: {bad:?}");
            assert!(!bad.findings.is_empty(), "{name} must report borrow error: {bad:?}");
        }
    }
}
