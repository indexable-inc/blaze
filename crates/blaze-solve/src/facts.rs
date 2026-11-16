//! `Facts`, the only way the solver learns anything outside the goal, and the recorder that is
//! the only way to ask it (solve.md, "The seams" and (e), (f)).
//!
//! Every method takes a [`Seal`], which only this crate makes ([`Ask`] does, after the question
//! is noted), so an implementation can write its methods and nothing outside the solver can
//! call them: a read missing from a read list cannot be written (blaze-typeck's `World` and
//! `Reads` pattern). An answer carries the `digest` of its content; a read is the question and
//! that digest, negative answers included (an empty impl set is a read).
//!
//! Types in answers are **portable**: [`Portable`] holds `TyKind` nodes whose `Ty` handles index
//! `nodes` and whose `List`s index `lists`, children before parents, generic parameters as
//! `Param(i)` of the item answered about (for a trait, `Self` is 0; for an impl, its own
//! generics; for an associated item, the parent's generics then its own). Predicates in answers
//! are `Pred`s over the same table.

use crate::error::Stop;
use crate::prefilter::Columns;
use crate::term::{Def, Lang, Pred, Shape, Ty, TyKind, List};

/// The proof that a call to a `Facts` method is made by the recorder.
pub struct Seal {
    _private: (),
}

/// The world has no answer for this question yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Missing;

/// An answer, or `Missing`.
pub type Fetch<T> = Result<T, Missing>;

/// A question to the world: what a read list holds and a `Need` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Question {
    TraitDef(Def),
    Impls(Def, Shape),
    ImplHeader(Def),
    Adt(Def),
    FnSig(Def),
    Assoc(Def),
    Opaque(Def),
    Lang(Lang),
    IsLocal(Def),
    /// A `where const` condition at ground arguments (`Pred::ConstHolds`). The list is a span
    /// of the caller's arena, valid for the solve that stopped on it: the world reads its terms
    /// before the solve runs again.
    ConstHolds(Def, List),
}

/// The value of a `where const` condition at ground arguments (`Facts::const_holds`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConstHolds {
    /// The condition evaluated to `true`: the clause holds.
    True,
    /// The condition evaluated to `false`: the clause fails, and so does the goal it is
    /// nested in (it never sends selection to another impl).
    False,
    /// The evaluation failed (a panic, an error of the evaluator, a condition the evaluator
    /// cannot answer yet): a finding on the clause, which is never taken as true or false.
    Error,
}

/// One read: the question and the digest of its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Read {
    pub question: Question,
    pub digest: u64,
}

/// A portable table of terms (see the module doc).
#[derive(Clone, Copy, Debug)]
pub struct Portable<'w> {
    pub nodes: &'w [TyKind],
    pub lists: &'w [Ty],
}

impl Portable<'_> {
    pub const EMPTY: Portable<'static> = Portable { nodes: &[], lists: &[] };
}

/// A trait's flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct TraitFlags(pub u16);

impl TraitFlags {
    pub const AUTO: u16 = 1 << 0;
    /// `#[marker]`: impls may overlap.
    pub const MARKER: u16 = 1 << 1;
    /// `#[rustc_coinductive]` (`Sized` and its hierarchy carry it).
    pub const COINDUCTIVE: u16 = 1 << 2;
    /// `#[fundamental]`.
    pub const FUNDAMENTAL: u16 = 1 << 3;
    pub const DYN_COMPATIBLE: u16 = 1 << 4;
    /// A trait alias.
    pub const ALIAS: u16 = 1 << 5;
    /// A `const trait`.
    pub const CONST: u16 = 1 << 6;
    pub const UNSAFE: u16 = 1 << 7;
    /// blaze's `#[blaze::sealed]`: no crate but the trait's own implements it.
    pub const SEALED: u16 = 1 << 8;
    /// The trait is `#[rustc_deny_explicit_impl]` (only builtin impls exist).
    pub const DENY_EXPLICIT_IMPL: u16 = 1 << 9;

    pub const fn has(self, bits: u16) -> bool {
        self.0 & bits != 0
    }
}

/// An associated type of a trait: its item, its own generic count, its bounds (a range of
/// `TraitDef::item_bounds`, whose self type is the alias over the trait's and its own params)
/// and its default.
#[derive(Clone, Copy, Debug)]
pub struct AssocTy {
    pub def: Def,
    pub own_generics: u32,
    pub bounds_start: u32,
    pub bounds_len: u32,
    pub default: Option<Ty>,
}

/// A trait's definition, as solving needs it.
#[derive(Clone, Copy, Debug)]
pub struct TraitDef<'w> {
    pub flags: TraitFlags,
    /// Generic parameters, `Self` included as 0.
    pub generics: u32,
    pub lang: Option<Lang>,
    /// The supertrait clauses (`Self: Super<..>`, and projection bounds on `Self`), elaborated
    /// one level: what `elaborate.rs` walks.
    pub supertraits: &'w [Pred],
    /// The trait's other where-clauses.
    pub where_clauses: &'w [Pred],
    pub assoc: &'w [AssocTy],
    pub item_bounds: &'w [Pred],
    pub portable: Portable<'w>,
    pub digest: u64,
}

/// An impl's polarity (`ImplPolarity`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImplPolarity {
    Positive,
    Negative,
    /// `#[rustc_reservation_impl]`: ambiguous in coherence, absent elsewhere.
    Reservation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct ImplFlags(pub u16);

impl ImplFlags {
    /// The impl has `default` items or is `default impl` (specialization).
    pub const DEFAULT: u16 = 1 << 0;
    /// A `const impl`.
    pub const CONST: u16 = 1 << 1;

    pub const fn has(self, bits: u16) -> bool {
        self.0 & bits != 0
    }
}

/// An associated item an impl defines: the trait's item it implements and its value (a type,
/// or for a const a const term), over the impl's generics then the item's own.
#[derive(Clone, Copy, Debug)]
pub struct ImplAssoc {
    pub trait_item: Def,
    pub value: Ty,
    pub own_generics: u32,
    /// `default type A = ..`: not used for normalization outside the specializing graph.
    pub is_default: bool,
}

/// An impl's header.
#[derive(Clone, Copy, Debug)]
pub struct ImplHeader<'w> {
    pub generics: u32,
    pub polarity: ImplPolarity,
    pub flags: ImplFlags,
    pub trait_: Def,
    /// The trait's arguments, the self type first.
    pub args: List,
    pub where_clauses: &'w [Pred],
    pub assoc: &'w [ImplAssoc],
    pub portable: Portable<'w>,
    pub digest: u64,
}

/// The impls of a trait that may apply to a self shape: the shape's bucket, then the blanket
/// bucket (blaze-impls' `candidates`), with the fingerprint columns (solve.md (d)).
#[derive(Clone, Copy, Debug)]
pub struct ImplSet<'w> {
    pub impls: &'w [Def],
    pub columns: Columns<'w>,
    pub digest: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct AdtFlags(pub u16);

impl AdtFlags {
    pub const STRUCT: u16 = 1 << 0;
    pub const ENUM: u16 = 1 << 1;
    pub const UNION: u16 = 1 << 2;
    pub const FUNDAMENTAL: u16 = 1 << 3;
    /// The `owned_box` lang item.
    pub const BOX: u16 = 1 << 4;
    pub const PHANTOM_DATA: u16 = 1 << 5;
    pub const MANUALLY_DROP: u16 = 1 << 6;
    pub const NON_EXHAUSTIVE: u16 = 1 << 7;
    /// The ADT has a `Drop` impl (for `Destruct` and `Copy`).
    pub const HAS_DROP: u16 = 1 << 8;

    pub const fn has(self, bits: u16) -> bool {
        self.0 & bits != 0
    }
}

/// An enum's discriminant type. Reference, "Enumerations", "Discriminants": "Under the
/// default representation, the discriminant is interpreted as an `isize` value", and under a
/// primitive representation (`#[repr(u8)]` etc.) it is that integer ("Type Layout",
/// "Primitive representations").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Discr {
    /// The world did not say (no `repr` read): a projection to it is an error naming the enum,
    /// never a guess.
    Unread,
    Int(crate::term::IntTy),
    Uint(crate::term::UintTy),
}

/// An ADT's definition, as solving needs it: its field types (every variant's, flattened, each
/// variant a range) and the tail field whose sizedness is the ADT's.
#[derive(Clone, Copy, Debug)]
pub struct AdtDef<'w> {
    pub flags: AdtFlags,
    pub generics: u32,
    pub fields: &'w [Ty],
    /// (start, len) into `fields`, one per variant (a struct has one).
    pub variants: &'w [(u32, u32)],
    /// The last field of a struct, when it has one.
    pub sized_tail: Option<Ty>,
    /// An enum's discriminant type (`DiscriminantKind::Discriminant`); `Unread` for a struct or
    /// union (whose discriminant type is `u8` whatever this says) and for an enum whose world
    /// does not read `repr` yet.
    pub discr: Discr,
    /// The ADT's where-clauses (for well-formedness).
    pub where_clauses: &'w [Pred],
    pub portable: Portable<'w>,
    pub digest: u64,
}

/// A fn item's signature: its inputs then output, over its generics (the parent's first).
#[derive(Clone, Copy, Debug)]
pub struct FnSigDef<'w> {
    pub generics: u32,
    pub inputs_output: List,
    /// `const fn`, `unsafe fn`, `#[target_feature]` (which is not `Fn*`).
    pub flags: u16,
    pub portable: Portable<'w>,
    pub digest: u64,
}

impl FnSigDef<'_> {
    pub const CONST: u16 = 1 << 0;
    pub const UNSAFE: u16 = 1 << 1;
    pub const TARGET_FEATURE: u16 = 1 << 2;
    /// `extern "rust-call"`: not `Fn*`-compatible (`ExternAbi::is_fn_trait_compatible`).
    pub const RUST_CALL: u16 = 1 << 3;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AssocKind {
    Type,
    Const,
    Fn,
}

/// An associated item: its container (a trait or an impl), the trait it belongs to (its own
/// trait, or the trait an impl implements; none for an inherent impl's), its kind and generics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AssocDef {
    pub container: Def,
    pub trait_: Option<Def>,
    pub kind: AssocKind,
    /// The container's generics (the parent's).
    pub parent_generics: u32,
    pub own_generics: u32,
    pub digest: u64,
}

/// An opaque type: its bounds (self is `Alias(Opaque, def, params)`), its generics, and the
/// hidden type when the asking body may see it (`None` outside its defining scope).
#[derive(Clone, Copy, Debug)]
pub struct OpaqueDef<'w> {
    pub generics: u32,
    pub bounds: &'w [Pred],
    pub hidden: Option<Ty>,
    pub portable: Portable<'w>,
    pub digest: u64,
}

/// Everything the solver may learn from outside the goal. Implemented by blaze-ty's adapter
/// over typeck's `Reads` (so a body's read list holds what its goals read) and by the tests'
/// world. Each method answers one [`Question`], or `Missing`; it must be a function of the
/// records: equal arguments give equal answers and digests.
pub trait Facts {
    fn trait_def(&self, seal: Seal, t: Def) -> Fetch<TraitDef<'_>>;
    /// The impls of `t` for a self type of `shape`, with the blanket ones, in the records'
    /// order, and their fingerprint columns.
    fn impls(&self, seal: Seal, t: Def, shape: Shape) -> Fetch<ImplSet<'_>>;
    fn impl_header(&self, seal: Seal, i: Def) -> Fetch<ImplHeader<'_>>;
    fn adt(&self, seal: Seal, d: Def) -> Fetch<AdtDef<'_>>;
    fn fn_sig(&self, seal: Seal, d: Def) -> Fetch<FnSigDef<'_>>;
    fn assoc(&self, seal: Seal, d: Def) -> Fetch<AssocDef>;
    fn opaque(&self, seal: Seal, d: Def) -> Fetch<OpaqueDef<'_>>;
    /// The lang item, if the crates in scope define it.
    fn lang(&self, seal: Seal, l: Lang) -> Fetch<Option<Def>>;
    /// Whether `d` is of the crate being compiled (coherence's orphan check and unknowable
    /// candidates).
    fn is_local(&self, seal: Seal, d: Def) -> Fetch<bool>;
    /// The value of the `where const` condition `cond` at `args`, every one ground and
    /// resolved (hash-consed, so equal arguments are equal terms). `Missing` stops the solve
    /// with `Need(Question::ConstHolds(cond, list))`, the list holding these terms.
    fn const_holds(&self, seal: Seal, cond: Def, args: &[Ty]) -> Fetch<ConstHolds>;
}

/// The digest of a lang item's answer (its def, or none).
fn lang_digest(d: Option<Def>) -> u64 {
    match d {
        // FNV-1a over the key's bytes: a digest only has to tell answers apart.
        Some(Def(k)) => {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in k.to_le_bytes() {
                h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
            }
            h
        }
        None => 0,
    }
}

/// The recorder: the world's borrow, and the read log every question is appended to before its
/// answer is handed on. The only caller of `Facts`.
pub struct Ask<'f, F: Facts> {
    facts: &'f F,
}

impl<'f, F: Facts> Ask<'f, F> {
    pub fn new(facts: &'f F) -> Ask<'f, F> {
        Ask { facts }
    }

    fn seal() -> Seal {
        Seal { _private: () }
    }

    fn note<T>(
        log: &mut blaze_own::Vec<Read, blaze_own::Alloc<'_>>,
        q: Question,
        got: Fetch<T>,
        digest: impl FnOnce(&T) -> u64,
    ) -> Result<T, Stop> {
        match got {
            Ok(v) => {
                log.push(Read { question: q, digest: digest(&v) });
                Ok(v)
            }
            Err(Missing) => Err(Stop::Need(q)),
        }
    }

    pub fn trait_def(&self, log: &mut blaze_own::Vec<Read, blaze_own::Alloc<'_>>, t: Def) -> Result<TraitDef<'f>, Stop> {
        let got = self.facts.trait_def(Self::seal(), t);
        Self::note(log, Question::TraitDef(t), got, |a| a.digest)
    }

    pub fn impls(
        &self,
        log: &mut blaze_own::Vec<Read, blaze_own::Alloc<'_>>,
        t: Def,
        shape: Shape,
    ) -> Result<ImplSet<'f>, Stop> {
        let got = self.facts.impls(Self::seal(), t, shape);
        Self::note(log, Question::Impls(t, shape), got, |a| a.digest)
    }

    pub fn impl_header(&self, log: &mut blaze_own::Vec<Read, blaze_own::Alloc<'_>>, i: Def) -> Result<ImplHeader<'f>, Stop> {
        let got = self.facts.impl_header(Self::seal(), i);
        Self::note(log, Question::ImplHeader(i), got, |a| a.digest)
    }

    pub fn adt(&self, log: &mut blaze_own::Vec<Read, blaze_own::Alloc<'_>>, d: Def) -> Result<AdtDef<'f>, Stop> {
        let got = self.facts.adt(Self::seal(), d);
        Self::note(log, Question::Adt(d), got, |a| a.digest)
    }

    pub fn fn_sig(&self, log: &mut blaze_own::Vec<Read, blaze_own::Alloc<'_>>, d: Def) -> Result<FnSigDef<'f>, Stop> {
        let got = self.facts.fn_sig(Self::seal(), d);
        Self::note(log, Question::FnSig(d), got, |a| a.digest)
    }

    pub fn assoc(&self, log: &mut blaze_own::Vec<Read, blaze_own::Alloc<'_>>, d: Def) -> Result<AssocDef, Stop> {
        let got = self.facts.assoc(Self::seal(), d);
        Self::note(log, Question::Assoc(d), got, |a| a.digest)
    }

    pub fn opaque(&self, log: &mut blaze_own::Vec<Read, blaze_own::Alloc<'_>>, d: Def) -> Result<OpaqueDef<'f>, Stop> {
        let got = self.facts.opaque(Self::seal(), d);
        Self::note(log, Question::Opaque(d), got, |a| a.digest)
    }

    pub fn lang(&self, log: &mut blaze_own::Vec<Read, blaze_own::Alloc<'_>>, l: Lang) -> Result<Option<Def>, Stop> {
        let got = self.facts.lang(Self::seal(), l);
        Self::note(log, Question::Lang(l), got, |a| lang_digest(*a))
    }

    pub fn is_local(&self, log: &mut blaze_own::Vec<Read, blaze_own::Alloc<'_>>, d: Def) -> Result<bool, Stop> {
        let got = self.facts.is_local(Self::seal(), d);
        Self::note(log, Question::IsLocal(d), got, |a| u64::from(*a))
    }

    /// `cond` at the ground `args`, which `list` holds (the question a `Need` names).
    pub fn const_holds(
        &self,
        log: &mut blaze_own::Vec<Read, blaze_own::Alloc<'_>>,
        cond: Def,
        list: List,
        args: &[Ty],
    ) -> Result<ConstHolds, Stop> {
        let got = self.facts.const_holds(Self::seal(), cond, args);
        Self::note(log, Question::ConstHolds(cond, list), got, |a| *a as u64)
    }
}
