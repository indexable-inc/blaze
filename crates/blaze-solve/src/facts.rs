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
    /// An opaque type read for its bounds and generics (an alias-bound candidate, coherence).
    Opaque(Def),
    Lang(Lang),
    IsLocal(Def),
    /// A `where const` condition at ground arguments (`Pred::ConstHolds`). The list is a span
    /// of the caller's arena, valid for the solve that stopped on it: the world reads its terms
    /// before the solve runs again.
    ConstHolds(Def, List),
    /// Whether the first impl specializes the second (`Facts::specializes`).
    Specializes(Def, Def),
    /// An opaque type read for its HIDDEN type (an auto trait's leak, structural.rs
    /// `step_auto`; the reveal after type checking, alias.rs): the same fact as
    /// [`Question::Opaque`], a separate kind so a stall is named only by an opaque whose
    /// hidden type was asked ([`Ask::first_hidden_unknown`]), never by one read for its bounds.
    OpaqueHidden(Def),
}

/// The names of [`Question`]'s kinds, in [`Question::kind`]'s order.
pub const QUESTION_KIND_NAMES: [&str; 12] =
    ["trait_def", "impls", "impl_header", "adt", "fn_sig", "assoc", "opaque", "lang", "is_local", "const_holds", "specializes", "opaque_hidden"];

impl Question {
    /// The question's kind, an index into [`QUESTION_KIND_NAMES`] (a measurement's bucket).
    pub fn kind(self) -> usize {
        match self {
            Question::TraitDef(_) => 0,
            Question::Impls(..) => 1,
            Question::ImplHeader(_) => 2,
            Question::Adt(_) => 3,
            Question::FnSig(_) => 4,
            Question::Assoc(_) => 5,
            Question::Opaque(_) => 6,
            Question::Lang(_) => 7,
            Question::IsLocal(_) => 8,
            Question::ConstHolds(..) => 9,
            Question::Specializes(..) => 10,
            Question::OpaqueHidden(_) => 11,
        }
    }
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
    /// The world could not decide the trait's dyn compatibility (a member or supertrait it did
    /// not read: blaze-typeck dyn_compat.rs `Verdict::Gap`). Neither compatible nor not: a goal
    /// whose answer depends on it stops with [`crate::error::Unsupported::DynUndecided`], never
    /// answers No (rule 3). Never set together with [`TraitFlags::DYN_COMPATIBLE`].
    pub const DYN_UNDECIDED: u16 = 1 << 10;

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
    /// The trait's implied clauses, one level: what `elaborate.rs` walks (the reference's
    /// `explicit_implied_clauses_of`, `PredicateFilter::SelfAndAssociatedTypeBounds`): the
    /// supertrait clauses (`Self: Super<..>`, projection bounds on `Self`) and the bounds of
    /// the associated item constraints written with them (`Tr: Super<A: B>` gives
    /// `<Self as Super>::A: B`). A walk of the supertraits proper (dyn upcasting, object
    /// candidates) keeps the clauses whose self type is `Self` (builtin.rs `on_self`). The
    /// trait's other where-clauses are not implied (the Reference, "Traits", "Supertraits").
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
    /// Blaze edition: transparent all-`pub` no-`Drop` struct eligible for automatic `Copy`/`Clone`.
    pub const AUTO_COPY_PUB: u16 = 1 << 9;

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
    /// An ABI other than `Rust` (`extern "C"`, `extern "rust-call"`, ..) or C-variadic: not
    /// `Fn*`-compatible (the pinned nightly's `FnSig::is_fn_trait_compatible`: "!c_variadic &&
    /// safe && abi == ExternAbi::Rust").
    pub const NOT_FN_ABI: u16 = 1 << 3;
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
    /// Whether impl `a` specializes impl `b`, both impls of one trait (rustc's
    /// `impl_specializes`; RFC 1210): every trait reference `a` applies to, `b` applies to,
    /// `b`'s where-clauses holding under `a`'s. A fact of the two impls alone, never of a goal.
    fn specializes(&self, seal: Seal, a: Def, b: Def) -> Fetch<bool>;
}

/// Where an evaluation gets a fact the moment it lacks one (the pull): the solver asks
/// [`Source::facts`] as before and, when a step meets `Missing`, calls [`Source::fill`] and runs
/// that one step again in the same frame (its partial bindings and rows rolled back first), so
/// no frame is thrown away and the root is not asked again (eval.rs `Solver::pull`).
///
/// Two strategies (rule 78), tested equal in answers and read sets (tests/cost.rs `pull_*`):
/// a source that fills, and [`Stopping`], which fills nothing, so a missing fact stops the
/// whole evaluation with `Stop::Need` and the caller fills it and asks the root again.
pub trait Source<T: crate::tys::Tys> {
    type Facts: Facts;
    /// Whether [`Source::fill`] can fill: `false` keeps the solver from marking each step for
    /// a rollback it never makes.
    const PULLS: bool;
    fn facts(&self) -> &Self::Facts;
    /// Makes the next ask of `q` answer. `false`: not filled now (the source keeps its own
    /// reason, an error of its world, for its caller); the evaluation stops with `Need(q)`.
    /// A source must not fill a question with an answer it would not have given before: the
    /// world's records are read-only for the evaluation (rule 25).
    fn fill(&mut self, tys: &mut T, q: Question) -> bool;
    /// The request's table of shared goals (share.rs), when the caller shares goals between
    /// bodies: the root goals of this evaluation are looked up and put there. DEFAULTED to
    /// none, so every source that does not share is unchanged.
    fn goals(&self) -> Option<&dyn crate::share::GoalShare> {
        None
    }
}

/// The source that fills nothing: a missing fact stops the evaluation (`Stop::Need`).
pub struct Stopping<'f, F: Facts>(pub &'f F);

impl<T: crate::tys::Tys, F: Facts> Source<T> for Stopping<'_, F> {
    type Facts = F;
    const PULLS: bool = false;
    fn facts(&self) -> &F {
        self.0
    }
    fn fill(&mut self, _tys: &mut T, _q: Question) -> bool {
        false
    }
}

/// Where the recorder notes a read: a caller's plain list, or the solver's own log
/// (`readlog::ReadLog`, a set per frame).
pub trait Log {
    fn note(&mut self, r: Read) -> Result<(), crate::error::SolveError>;
}

impl Log for blaze_own::Vec<Read, blaze_own::Alloc<'_>> {
    fn note(&mut self, r: Read) -> Result<(), crate::error::SolveError> {
        self.push(r);
        Ok(())
    }
}

/// The digest of a lang item's answer (its def, or none).
pub(crate) fn lang_digest(d: Option<Def>) -> u64 {
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

    fn note<T, L: Log + ?Sized>(log: &mut L, q: Question, got: Fetch<T>, digest: impl FnOnce(&T) -> u64) -> Result<T, Stop> {
        match got {
            Ok(v) => {
                log.note(Read { question: q, digest: digest(&v) })?;
                Ok(v)
            }
            Err(Missing) => Err(Stop::Need(q)),
        }
    }

    pub fn trait_def<L: Log + ?Sized>(&self, log: &mut L, t: Def) -> Result<TraitDef<'f>, Stop> {
        let got = self.facts.trait_def(Self::seal(), t);
        Self::note(log, Question::TraitDef(t), got, |a| a.digest)
    }

    pub fn impls<L: Log + ?Sized>(&self, log: &mut L, t: Def, shape: Shape) -> Result<ImplSet<'f>, Stop> {
        let got = self.facts.impls(Self::seal(), t, shape);
        Self::note(log, Question::Impls(t, shape), got, |a| a.digest)
    }

    pub fn impl_header<L: Log + ?Sized>(&self, log: &mut L, i: Def) -> Result<ImplHeader<'f>, Stop> {
        let got = self.facts.impl_header(Self::seal(), i);
        Self::note(log, Question::ImplHeader(i), got, |a| a.digest)
    }

    pub fn adt<L: Log + ?Sized>(&self, log: &mut L, d: Def) -> Result<AdtDef<'f>, Stop> {
        let got = self.facts.adt(Self::seal(), d);
        Self::note(log, Question::Adt(d), got, |a| a.digest)
    }

    pub fn fn_sig<L: Log + ?Sized>(&self, log: &mut L, d: Def) -> Result<FnSigDef<'f>, Stop> {
        let got = self.facts.fn_sig(Self::seal(), d);
        Self::note(log, Question::FnSig(d), got, |a| a.digest)
    }

    pub fn assoc<L: Log + ?Sized>(&self, log: &mut L, d: Def) -> Result<AssocDef, Stop> {
        let got = self.facts.assoc(Self::seal(), d);
        Self::note(log, Question::Assoc(d), got, |a| a.digest)
    }

    pub fn opaque<L: Log + ?Sized>(&self, log: &mut L, d: Def) -> Result<OpaqueDef<'f>, Stop> {
        let got = self.facts.opaque(Self::seal(), d);
        Self::note(log, Question::Opaque(d), got, |a| a.digest)
    }

    /// [`Ask::opaque`] read for its hidden type ([`Question::OpaqueHidden`]).
    pub fn opaque_hidden<L: Log + ?Sized>(&self, log: &mut L, d: Def) -> Result<OpaqueDef<'f>, Stop> {
        let got = self.facts.opaque(Self::seal(), d);
        Self::note(log, Question::OpaqueHidden(d), got, |a| a.digest)
    }

    pub fn lang<L: Log + ?Sized>(&self, log: &mut L, l: Lang) -> Result<Option<Def>, Stop> {
        let got = self.facts.lang(Self::seal(), l);
        Self::note(log, Question::Lang(l), got, |a| lang_digest(*a))
    }

    pub fn is_local<L: Log + ?Sized>(&self, log: &mut L, d: Def) -> Result<bool, Stop> {
        let got = self.facts.is_local(Self::seal(), d);
        Self::note(log, Question::IsLocal(d), got, |a| u64::from(*a))
    }

    pub fn specializes<L: Log + ?Sized>(&self, log: &mut L, a: Def, b: Def) -> Result<bool, Stop> {
        let got = self.facts.specializes(Self::seal(), a, b);
        Self::note(log, Question::Specializes(a, b), got, |s| u64::from(*s))
    }

    /// The typed cause of an undecided answer that stalled on an opaque type: the first opaque
    /// of `reads` (a root's read list as [`crate::Solver::evaluate_from`] appends it, in the
    /// order of first reads: rule 25) whose hidden type these facts do not give, `skip`'s aside.
    /// The auto-trait rule reads that opaque's fact the moment it stalls on it
    /// (structural.rs `step_auto`, "ambiguous until it does"), and a cached, shared or
    /// provisional answer carries its read list whole, so the cause survives every reuse of
    /// the answer at no cost on the `Yes` and `No` paths: the caller asks only after a
    /// `Maybe`. Only a read for the hidden type ([`Question::OpaqueHidden`]) is a cause: an
    /// opaque read for its bounds alone (`+ Send` written on it answers `Send` by the
    /// alias-bound candidate) was the superset's misattribution (tempo c0255wave87: seven
    /// bodies named a required trait fn's `+ Send` future, hiding their real cause).
    /// Noted nowhere: each read is in the list already.
    pub fn first_hidden_unknown(&self, reads: &[Read], mut skip: impl FnMut(Def) -> bool) -> Option<Def> {
        reads.iter().find_map(|r| match r.question {
            Question::OpaqueHidden(d) if !skip(d) => match self.facts.opaque(Self::seal(), d) {
                Ok(o) if o.hidden.is_none() => Some(d),
                _ => None,
            },
            _ => None,
        })
    }

    /// The digest `q` answers today, asked again (a shared goal's read list checked against a
    /// body's facts: eval.rs `share_replay`). Noted nowhere: the caller notes the list whole
    /// once every digest is equal. `Stop::Need(q)` when the facts lack it.
    pub fn replay(&self, q: Question) -> Result<u64, Stop> {
        let s = Self::seal;
        let got = match q {
            Question::TraitDef(t) => self.facts.trait_def(s(), t).map(|a| a.digest),
            Question::Impls(t, shape) => self.facts.impls(s(), t, shape).map(|a| a.digest),
            Question::ImplHeader(i) => self.facts.impl_header(s(), i).map(|a| a.digest),
            Question::Adt(d) => self.facts.adt(s(), d).map(|a| a.digest),
            Question::FnSig(d) => self.facts.fn_sig(s(), d).map(|a| a.digest),
            Question::Assoc(d) => self.facts.assoc(s(), d).map(|a| a.digest),
            Question::Opaque(d) | Question::OpaqueHidden(d) => self.facts.opaque(s(), d).map(|a| a.digest),
            Question::Lang(l) => self.facts.lang(s(), l).map(lang_digest),
            Question::IsLocal(d) => self.facts.is_local(s(), d).map(u64::from),
            Question::Specializes(a, b) => self.facts.specializes(s(), a, b).map(u64::from),
            // Its question names the asker's arena terms: a goal that read one is never put.
            Question::ConstHolds(..) => return Err(Stop::Error(crate::error::SolveError::Share(crate::share::ShareError::Unreplayable))),
        };
        got.map_err(|Missing| Stop::Need(q))
    }

    /// `cond` at the ground `args`, which `list` holds (the question a `Need` names).
    pub fn const_holds<L: Log + ?Sized>(
        &self,
        log: &mut L,
        cond: Def,
        list: List,
        args: &[Ty],
    ) -> Result<ConstHolds, Stop> {
        let got = self.facts.const_holds(Self::seal(), cond, args);
        Self::note(log, Question::ConstHolds(cond, list), got, |a| *a as u64)
    }
}
