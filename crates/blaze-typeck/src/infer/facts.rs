//! blaze-solve's `Facts` over typeck's `Reads`: the solver's questions answered from the
//! body's own read seam, so a body's read list holds every fact its goals read.
//!
//! `Facts` answers from `&self` and may only say `Missing`; reading the world needs
//! `&mut Reads`. So the adapter is a cache the context owns: the solver runs against it, and a
//! question it lacks is pulled (infer/mod.rs `Pull`): between two solver steps
//! [`FactCache::fill`] reads that question through `Reads` (entering the read list, or
//! stopping the body with typeck's own `Need`), converts the answer to the solver's portable
//! form and keeps it, and the step that asked runs again. Nothing here depends on the order
//! questions come in: the cache is a function of the answers read, and a fresh run fills it
//! the same way.
//!
//! Associated types: typeck names one by its trait and name; the solver by a def. The def is
//! [`assoc_def`]`(trait, name)`, a 128-bit hash, used for every alias, every trait's associated
//! type and every impl's binding alike, so they agree without asking for the item's key; the
//! cache maps it back for `Facts::assoc` and for `TypeCtx::view`.

use core::cell::{Cell, RefCell};

use blaze_hash::HashMap;
use blaze_own::{Alloc, Vec};
use blaze_solve::facts::{
    AdtDef, AdtFlags, AssocDef, AssocKind, AssocTy, ConstHolds, Facts, Fetch, FnSigDef, ImplAssoc, ImplFlags, ImplHeader,
    ImplPolarity, ImplSet, Missing, OpaqueDef, Portable, Question as SQ, Seal, TraitDef, TraitFlags as STraitFlags,
};
use blaze_solve::prefilter::{self, Columns, Fingerprint};
use blaze_solve::term::{
    AliasKind as SAlias, Def as SDef, Lang, List as SList, Mutbl as SMutbl, Polarity, Pred, Rigid, Shape as SShape,
    Ty as STy, TyKind as K,
};

use crate::error::{CheckError, Malformed, Stop};
use crate::hash::{ContentHash, StableHasher};
use crate::ids::{FloatTy, IntTy, ItemKey, LangItem, Mutbl, Name, UintTy};
use crate::portable::{PNode, PTy, PTys};
use crate::world::{AdtKind, PClause, Question, Reads, Shape, World};

mod item_bounds;

/// The solver's def of `<_ as trait_>::name`.
pub fn assoc_def(trait_: ItemKey, name: Name) -> u128 {
    let mut lo = StableHasher::new("blaze-typeck assoc def lo");
    lo.u128(trait_.0);
    lo.u64(name.0);
    let mut hi = StableHasher::new("blaze-typeck assoc def hi");
    hi.u128(trait_.0);
    hi.u64(name.0);
    u128::from(lo.finish()) | (u128::from(hi.finish()) << 64)
}

#[derive(Clone, Copy, Default)]
struct Span {
    start: u32,
    len: u32,
}

impl Span {
    fn of<T>(v: &[T], start: usize) -> Span {
        Span {
            start: u32::try_from(start).unwrap_or(u32::MAX),
            len: u32::try_from(v.len().saturating_sub(start)).unwrap_or(u32::MAX),
        }
    }

    fn get<T>(self, v: &[T]) -> &[T] {
        let start = usize::try_from(self.start).unwrap_or(usize::MAX);
        let end = usize::try_from(self.start.saturating_add(self.len)).unwrap_or(usize::MAX);
        v.get(start..end).unwrap_or(&[])
    }
}

#[derive(Clone, Copy, Default)]
struct Por {
    nodes: Span,
    lists: Span,
}

/// Whether checker key `d` (first 16 bytes of `blaze_id::ItemKey`, big-endian) carries the
/// `INNER` bit (`0x80` on byte 8), marking an item declared inside a body. Body-local entries
/// are never persisted across bodies in a stretch-reused [`FactCache`].
fn is_inner_def(d: u128) -> bool {
    d.to_be_bytes()[8] & 0x80 != 0
}

fn shape_is_body_local(shape: SShape) -> bool {
    match shape {
        SShape::Closure(_) | SShape::Coroutine(_) => true,
        SShape::Adt(d) | SShape::Foreign(d) | SShape::FnDef(d) | SShape::Dynamic(d) => is_inner_def(d.0),
        _ => false,
    }
}

struct TraitRow {
    flags: STraitFlags,
    generics: u32,
    lang: Option<Lang>,
    supers: Span,
    assoc: Span,
    /// The associated types' item bounds, each type's a range of this span
    /// (`AssocTy::bounds_start`/`bounds_len`): the solver's `TraitDef::item_bounds`.
    item_bounds: Span,
    por: Por,
    digest: u64,
    reads: Span,
    touched_gen: Cell<u32>,
    reusable: bool,
}

struct ImplRow {
    generics: u32,
    polarity: ImplPolarity,
    trait_: SDef,
    args: SList,
    wheres: Span,
    assoc: Span,
    por: Por,
    digest: u64,
    reads: Span,
    touched_gen: Cell<u32>,
    reusable: bool,
}

struct AdtRow {
    flags: AdtFlags,
    generics: u32,
    fields: Span,
    variants: Span,
    sized_tail: Option<STy>,
    discr: blaze_solve::facts::Discr,
    wheres: Span,
    por: Por,
    digest: u64,
    reads: Span,
    touched_gen: Cell<u32>,
}

struct SigRow {
    generics: u32,
    /// `FnSigDef`'s flags: `UNSAFE` and `NOT_FN_ABI` from the fn's header.
    flags: u16,
    io: SList,
    por: Por,
    digest: u64,
    reads: Span,
    touched_gen: Cell<u32>,
}

struct AssocRow {
    def: AssocDef,
    reads: Span,
    touched_gen: Cell<u32>,
}

struct ImplsRow {
    span: Span,
    digest: u64,
    reads: Span,
    touched_gen: Cell<u32>,
}

struct LangRow {
    def: Option<SDef>,
    reads: Span,
    touched_gen: Cell<u32>,
}

/// Key inserted into a persistent [`FactCache`] table during one body round, paired with
/// whether it is sound to retain across bodies on a clean commit.
#[derive(Clone, Copy)]
enum AddedKey {
    Trait(u128),
    Impls(u128, SShape),
    Header(u128),
    Adt(u128),
    Sig(u128),
    Assoc(u128),
    Lang(Lang),
}

#[derive(Clone, Copy, Default)]
struct RoundSnap {
    nodes: usize,
    lists: usize,
    preds: usize,
    assoc_tys: usize,
    impl_assoc: usize,
    fields: usize,
    variants: usize,
    impl_defs: usize,
    impl_columns: usize,
    cached_reads: usize,
    erased: Option<STy>,
}

#[derive(Clone, Copy)]
struct OpaqueRow {
    bounds: Span,
    hidden: Option<STy>,
    por: Por,
    digest: u64,
}

/// What the conversions of one cache did: tables, entries converted, entries the answers'
/// roots reach, entries offered (the tables' lengths).
#[derive(Clone, Copy, Default)]
struct Converted {
    tables: u64,
    converted: u64,
    reached: u64,
    offered: u64,
}

/// A record's table being converted: the question it answered (named by errors), its number
/// in the call's memo, and its terms.
#[derive(Clone, Copy)]
struct Conv<'t> {
    q: Question,
    tab: u32,
    t: PTys<'t>,
}

/// A body's type met inside an opaque's hidden type, whose term is made from its owner's facts
/// (`FactCache::body_terms`), never converted from the record.
#[derive(Clone, Copy)]
enum BodyTy {
    Closure,
    Coroutine,
}

/// A closure or async block of an opaque's hidden type, by identity: its owner, its ordinal
/// among the owner's, and how many owner arguments its term holds (a closure's term holds them
/// before `[kind, sig, upvars]`; an async block's holds them after `[output, interior]`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct HiddenBody {
    pub owner: ItemKey,
    pub ordinal: u32,
    pub args: u32,
}

/// The solver's facts, read through the body's `Reads` (see the module).
pub struct FactCache<'a> {
    alloc: Alloc<'a>,
    nodes: Vec<K, Alloc<'a>>,
    lists: Vec<STy, Alloc<'a>>,
    preds: Vec<Pred, Alloc<'a>>,
    assoc_tys: Vec<AssocTy, Alloc<'a>>,
    impl_assoc: Vec<ImplAssoc, Alloc<'a>>,
    fields: Vec<STy, Alloc<'a>>,
    variants: Vec<(u32, u32), Alloc<'a>>,
    impl_defs: Vec<SDef, Alloc<'a>>,
    traits: HashMap<u128, TraitRow, Alloc<'a>>,
    impls: HashMap<(u128, SShape), ImplsRow, Alloc<'a>>,
    /// Each `impls` entry's fingerprint columns (blaze-solve prefilter.rs): their start in
    /// `impl_columns` and their stride; `WIDTH * stride` bytes.
    impl_cols: HashMap<(u128, SShape), (u32, u32), Alloc<'a>>,
    impl_columns: Vec<u8, Alloc<'a>>,
    /// One bucket's fingerprints and columns while it is filled (reused).
    fp_scratch: Vec<Fingerprint, Alloc<'a>>,
    col_scratch: Vec<u8, Alloc<'a>>,
    headers: HashMap<u128, ImplRow, Alloc<'a>>,
    adts: HashMap<u128, AdtRow, Alloc<'a>>,
    sigs: HashMap<u128, SigRow, Alloc<'a>>,
    assoc: HashMap<u128, AssocRow, Alloc<'a>>,
    opaques: HashMap<u128, OpaqueRow, Alloc<'a>>,
    lang: HashMap<Lang, LangRow, Alloc<'a>>,
    /// Compact side-log of `(Question, digest)` reads captured when each reusable entry was
    /// first populated, replayed into later bodies' `ReadList` on cache hits.
    cached_reads: Vec<(Question, u64), Alloc<'a>>,
    /// `cached_reads` spans touched via `Facts::*` hits during the current reused body round.
    body_replayed: RefCell<Vec<Span, Alloc<'a>>>,
    /// Whether this cache is being reused across consecutive bodies in a stretch.
    reuse_mode: bool,
    /// Monotonic per-body-round generation; an entry with `touched_gen == body_gen` has
    /// already recorded/replayed its reads in the active round.
    body_gen: u32,
    /// Current body's owner key (whose own `FnSig` is excluded from cross-body persistence
    /// because an unresolved inferred output is answered differently inside its own body).
    reuse_owner: ItemKey,
    /// Persistent arena lengths at the start of the active reused round, for rollback on
    /// holed, gapped, or errored rounds.
    round_snap: RoundSnap,
    /// Keys inserted during the active reused round and whether each is eligible to persist.
    round_added: Vec<(AddedKey, bool), Alloc<'a>>,
    /// Body-local `assoc_def` keys inserted into `self.names` during the active round.
    round_inner_names: Vec<u128, Alloc<'a>>,
    /// `assoc_def(trait, name)` back to its trait and name.
    pub names: HashMap<u128, (ItemKey, Name), Alloc<'a>>,
    /// Each closure and async block an opaque's hidden type holds ([`Self::body_terms`]), by
    /// its solver def (coroutine_id.rs `closure_of_def`, `coroutine_def`): the solver
    /// instantiates their terms itself when it reveals the opaque, so the context names them by
    /// this (infer/mod.rs `adopt`).
    pub(crate) hidden_bodies: HashMap<u128, HiddenBody, Alloc<'a>>,
    /// Questions filled after the solver stopped on them: each is one repeated evaluation.
    pub retries: u64,
    /// Every impl's `where const` conditions, in the order the impls were read:
    /// `(condition, the impl's trait)`, for the reason of an unmet bound.
    conditions: Vec<(u128, u128), Alloc<'a>>,
    /// A `where const` condition's value at ground arguments: the arguments (hash-consed terms,
    /// so equal arguments are equal terms) at `span` of `cond_args`, keyed by the condition and
    /// a hash of the terms; a second set of arguments under one key is refused (rule 21).
    holds: HashMap<(u128, u64), (Span, ConstHolds), Alloc<'a>>,
    cond_args: Vec<STy, Alloc<'a>>,
    /// Whether the first impl specializes the second (`Facts::specializes`), as
    /// `InferCtx::fill` decided it.
    specializes: HashMap<(u128, u128), bool, Alloc<'a>>,
    /// What `table` did, for the tally (asks.rs `Tally`): read by `fill`.
    counted: Converted,
    /// The record tables this call has converted from, each to its number in `memo`.
    seen: HashMap<(usize, usize), u32, Alloc<'a>>,
    /// A record table's node (its table's number, its index) to the solver term converted
    /// from it: each node an answer reaches is converted once per call (the cache lives for
    /// one check), and an answer whose roots were converted costs a lookup per root.
    memo: HashMap<(u32, u32), STy, Alloc<'a>>,
    /// The call's erased region, once a node needed it.
    erased: Option<STy>,
    /// `ty`'s work list: a node and whether its children were pushed.
    work: Vec<(u32, bool), Alloc<'a>>,
    /// `ty`'s scratch: one node's children.
    kids: Vec<u32, Alloc<'a>>,
    /// A planted bug (`suite::control_first_impl_taken`): an impl bucket keeps only its first impl.
    #[cfg(test)]
    pub(crate) first_impl_only: bool,
    /// A planted bug (`suite::control_where_const_dropped`): an impl's `where const` clauses
    /// are passed over, so the impl holds for every type.
    #[cfg(test)]
    pub(crate) drop_const_clauses: bool,
}

/// The key of a `where const` condition's arguments in the cache: an FNV-1a hash of the terms.
fn args_key(args: &[STy]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for t in args {
        for b in t.0.to_le_bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

fn digest(domain: &str, v: &impl ContentHash) -> u64 {
    let mut h = StableHasher::new(domain);
    v.hash_into(&mut h);
    h.finish()
}

/// A discriminant type as one number, for the ADT row's digest.
fn discr_code(d: blaze_solve::facts::Discr) -> u8 {
    match d {
        blaze_solve::facts::Discr::Unread => 0,
        blaze_solve::facts::Discr::Int(i) => 1 + i as u8,
        blaze_solve::facts::Discr::Uint(u) => 7 + u as u8,
    }
}

fn int(i: IntTy) -> blaze_solve::term::IntTy {
    use blaze_solve::term::IntTy as S;
    match i {
        IntTy::Isize => S::Isize,
        IntTy::I8 => S::I8,
        IntTy::I16 => S::I16,
        IntTy::I32 => S::I32,
        IntTy::I64 => S::I64,
        IntTy::I128 => S::I128,
    }
}

fn uint(u: UintTy) -> blaze_solve::term::UintTy {
    use blaze_solve::term::UintTy as S;
    match u {
        UintTy::Usize => S::Usize,
        UintTy::U8 => S::U8,
        UintTy::U16 => S::U16,
        UintTy::U32 => S::U32,
        UintTy::U64 => S::U64,
        UintTy::U128 => S::U128,
    }
}

fn float(f: FloatTy) -> blaze_solve::term::FloatTy {
    use blaze_solve::term::FloatTy as S;
    match f {
        FloatTy::F16 => S::F16,
        FloatTy::F32 => S::F32,
        FloatTy::F64 => S::F64,
        FloatTy::F128 => S::F128,
    }
}

fn mutbl(m: Mutbl) -> SMutbl {
    match m {
        Mutbl::Not => SMutbl::Not,
        Mutbl::Mut => SMutbl::Mut,
    }
}

/// typeck's shape of the solver's (the world's buckets are typeck's).
pub fn shape(s: SShape) -> Shape {
    use blaze_solve::term::{FloatTy as SF, IntTy as SI, UintTy as SU};
    let m = |m: SMutbl| match m {
        SMutbl::Not => Mutbl::Not,
        SMutbl::Mut => Mutbl::Mut,
    };
    match s {
        SShape::Bool => Shape::Bool,
        SShape::Char => Shape::Char,
        SShape::Int(i) => Shape::Int(match i {
            SI::Isize => IntTy::Isize,
            SI::I8 => IntTy::I8,
            SI::I16 => IntTy::I16,
            SI::I32 => IntTy::I32,
            SI::I64 => IntTy::I64,
            SI::I128 => IntTy::I128,
        }),
        SShape::Uint(u) => Shape::Uint(match u {
            SU::Usize => UintTy::Usize,
            SU::U8 => UintTy::U8,
            SU::U16 => UintTy::U16,
            SU::U32 => UintTy::U32,
            SU::U64 => UintTy::U64,
            SU::U128 => UintTy::U128,
        }),
        SShape::Float(f) => Shape::Float(match f {
            SF::F16 => FloatTy::F16,
            SF::F32 => FloatTy::F32,
            SF::F64 => FloatTy::F64,
            SF::F128 => FloatTy::F128,
        }),
        SShape::Str => Shape::Str,
        SShape::Never => Shape::Never,
        SShape::Tuple(n) => Shape::Tuple(n),
        SShape::Record(s, n) => Shape::Record(s, n),
        SShape::Union(n) => Shape::Union(n),
        SShape::Ref(x) => Shape::Ref(m(x)),
        SShape::Ptr(x) => Shape::Ptr(m(x)),
        SShape::Array => Shape::Array,
        SShape::Slice => Shape::Slice,
        SShape::Pat => Shape::Pat,
        SShape::Adt(d) => Shape::Adt(ItemKey(d.0)),
        SShape::Foreign(d) => Shape::Foreign(ItemKey(d.0)),
        SShape::FnDef(d) => Shape::FnDef(ItemKey(d.0)),
        SShape::FnPtr(n) => Shape::FnPtr(n),
        SShape::Closure(_) | SShape::Coroutine(_) => Shape::Closure,
        SShape::Dynamic(d) => Shape::Dynamic(if d.0 == 0 { None } else { Some(ItemKey(d.0)) }),
        SShape::AnyInt => Shape::AnyInt,
        SShape::AnyFloat => Shape::AnyFloat,
        SShape::Blanket => Shape::Param,
    }
}

/// typeck's lang item of the solver's.
pub fn lang_item(l: Lang) -> LangItem {
    match l {
        Lang::Sized => LangItem::Sized,
        Lang::MetaSized => LangItem::MetaSized,
        Lang::PointeeSized => LangItem::PointeeSized,
        Lang::Copy => LangItem::Copy,
        Lang::Clone => LangItem::Clone,
        Lang::Send => LangItem::Send,
        Lang::Sync => LangItem::Sync,
        Lang::Unpin => LangItem::Unpin,
        Lang::Fn => LangItem::Fn,
        Lang::FnMut => LangItem::FnMut,
        Lang::FnOnce => LangItem::FnOnce,
        Lang::FnOnceOutput => LangItem::FnOnceOutput,
        Lang::FnPtrTrait => LangItem::FnPtrTrait,
        Lang::Tuple => LangItem::Tuple,
        Lang::Pointee => LangItem::Pointee,
        Lang::Metadata => LangItem::Metadata,
        Lang::DynMetadata => LangItem::DynMetadata,
        Lang::DiscriminantKind => LangItem::DiscriminantKind,
        Lang::Discriminant => LangItem::Discriminant,
        Lang::Destruct => LangItem::Destruct,
        Lang::Drop => LangItem::Drop,
        Lang::Unsize => LangItem::Unsize,
        Lang::CoerceUnsized => LangItem::CoerceUnsized,
        Lang::DispatchFromDyn => LangItem::DispatchFromDyn,
        Lang::PhantomData => LangItem::PhantomData,
        Lang::OwnedBox => LangItem::OwnedBox,
        Lang::ManuallyDrop => LangItem::ManuallyDrop,
        Lang::Future => LangItem::Future,
        Lang::FutureOutput => LangItem::FutureOutput,
        Lang::AsyncFn => LangItem::AsyncFn,
        Lang::AsyncFnMut => LangItem::AsyncFnMut,
        Lang::AsyncFnOnce => LangItem::AsyncFnOnce,
        Lang::AsyncFnOnceOutput => LangItem::AsyncFnOnceOutput,
        Lang::CallOnceFuture => LangItem::CallOnceFuture,
        Lang::CallRefFuture => LangItem::CallRefFuture,
        Lang::From => LangItem::From,
        Lang::Enum => LangItem::Enum,
        Lang::UnitEnum => LangItem::UnitEnum,
        Lang::TryAsDyn => LangItem::TryAsDyn,
    }
}

/// The solver's lang item of typeck's, for the ones the solver knows.
fn solver_lang(l: LangItem) -> Option<Lang> {
    const ALL: [Lang; 39] = [
        Lang::Sized,
        Lang::MetaSized,
        Lang::PointeeSized,
        Lang::Copy,
        Lang::Clone,
        Lang::Send,
        Lang::Sync,
        Lang::Unpin,
        Lang::Fn,
        Lang::FnMut,
        Lang::FnOnce,
        Lang::FnOnceOutput,
        Lang::FnPtrTrait,
        Lang::Tuple,
        Lang::Pointee,
        Lang::Metadata,
        Lang::DynMetadata,
        Lang::DiscriminantKind,
        Lang::Discriminant,
        Lang::Destruct,
        Lang::Drop,
        Lang::Unsize,
        Lang::CoerceUnsized,
        Lang::DispatchFromDyn,
        Lang::PhantomData,
        Lang::OwnedBox,
        Lang::ManuallyDrop,
        Lang::Future,
        Lang::FutureOutput,
        Lang::AsyncFn,
        Lang::AsyncFnMut,
        Lang::AsyncFnOnce,
        Lang::AsyncFnOnceOutput,
        Lang::CallOnceFuture,
        Lang::CallRefFuture,
        Lang::From,
        Lang::Enum,
        Lang::UnitEnum,
        Lang::TryAsDyn,
    ];
    ALL.into_iter().find(|&s| lang_item(s) == l)
}

impl<'a> FactCache<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> FactCache<'a> {
        FactCache {
            alloc,
            nodes: Vec::new_in(alloc),
            lists: Vec::new_in(alloc),
            preds: Vec::new_in(alloc),
            assoc_tys: Vec::new_in(alloc),
            impl_assoc: Vec::new_in(alloc),
            fields: Vec::new_in(alloc),
            variants: Vec::new_in(alloc),
            impl_defs: Vec::new_in(alloc),
            traits: HashMap::new_in(alloc),
            impls: HashMap::new_in(alloc),
            impl_cols: HashMap::new_in(alloc),
            impl_columns: Vec::new_in(alloc),
            fp_scratch: Vec::new_in(alloc),
            col_scratch: Vec::new_in(alloc),
            headers: HashMap::new_in(alloc),
            adts: HashMap::new_in(alloc),
            sigs: HashMap::new_in(alloc),
            assoc: HashMap::new_in(alloc),
            opaques: HashMap::new_in(alloc),
            lang: HashMap::new_in(alloc),
            cached_reads: Vec::new_in(alloc),
            body_replayed: RefCell::new(Vec::new_in(alloc)),
            reuse_mode: false,
            body_gen: 0,
            reuse_owner: ItemKey(0),
            round_snap: RoundSnap::default(),
            round_added: Vec::new_in(alloc),
            round_inner_names: Vec::new_in(alloc),
            names: HashMap::new_in(alloc),
            hidden_bodies: HashMap::new_in(alloc),
            retries: 0,
            conditions: Vec::new_in(alloc),
            holds: HashMap::new_in(alloc),
            cond_args: Vec::new_in(alloc),
            specializes: HashMap::new_in(alloc),
            counted: Converted::default(),
            seen: HashMap::new_in(alloc),
            memo: HashMap::new_in(alloc),
            erased: None,
            work: Vec::new_in(alloc),
            kids: Vec::new_in(alloc),
            #[cfg(test)]
            first_impl_only: false,
            #[cfg(test)]
            drop_const_clauses: false,
        }
    }

    /// Begins a single-threaded stretch-reused body round for `owner`: clears all per-body
    /// ephemeral state (`opaques`, `hidden_bodies`, `holds`, `specializes`, `conditions`,
    /// `seen`, `memo`, replay log), snapshots persistent arenas for rollback on holed/gapped/
    /// errored rounds, and advances `body_gen` so cached entries touched during this round
    /// replay their exact `(Question, digest)` side-logs into `ReadList`.
    pub fn begin_body_reuse(&mut self, owner: ItemKey) {
        self.clear_ephemeral();
        self.reuse_mode = true;
        self.reuse_owner = owner;
        if self.body_gen == u32::MAX {
            for r in self.traits.values() {
                r.touched_gen.set(0);
            }
            for r in self.impls.values() {
                r.touched_gen.set(0);
            }
            for r in self.headers.values() {
                r.touched_gen.set(0);
            }
            for r in self.adts.values() {
                r.touched_gen.set(0);
            }
            for r in self.sigs.values() {
                r.touched_gen.set(0);
            }
            for r in self.assoc.values() {
                r.touched_gen.set(0);
            }
            for r in self.lang.values() {
                r.touched_gen.set(0);
            }
            self.body_gen = 1;
        } else {
            self.body_gen += 1;
        }
        self.round_snap = RoundSnap {
            nodes: self.nodes.len(),
            lists: self.lists.len(),
            preds: self.preds.len(),
            assoc_tys: self.assoc_tys.len(),
            impl_assoc: self.impl_assoc.len(),
            fields: self.fields.len(),
            variants: self.variants.len(),
            impl_defs: self.impl_defs.len(),
            impl_columns: self.impl_columns.len(),
            cached_reads: self.cached_reads.len(),
            erased: self.erased,
        };
    }

    /// Finishes the active stretch-reused body round:
    /// - When `commit` is `true`, retains all eligible cross-body entries added this round and
    ///   evicts non-reusable entries (body-local `INNER`, `owner`'s own `FnSig`, `where const`
    ///   impls, `dyn_compat::Gap` traits, or any holed entry).
    /// - When `commit` is `false` (holed round, gapped round, error, or unwind), rolls back
    ///   every entry and persistent arena append made during this round.
    /// Always clears all per-body ephemeral state (`opaques`, `holds`, `specializes`, etc.).
    pub fn end_body_reuse(&mut self, commit: bool) {
        if !commit {
            let n = self.round_added.len();
            for i in 0..n {
                if let Some(&(key, _)) = self.round_added.get(i) {
                    self.remove_added(key);
                }
            }
            self.nodes.truncate(self.round_snap.nodes);
            self.lists.truncate(self.round_snap.lists);
            self.preds.truncate(self.round_snap.preds);
            self.assoc_tys.truncate(self.round_snap.assoc_tys);
            self.impl_assoc.truncate(self.round_snap.impl_assoc);
            self.fields.truncate(self.round_snap.fields);
            self.variants.truncate(self.round_snap.variants);
            self.impl_defs.truncate(self.round_snap.impl_defs);
            self.impl_columns.truncate(self.round_snap.impl_columns);
            self.cached_reads.truncate(self.round_snap.cached_reads);
            self.erased = self.round_snap.erased;
        } else {
            let n = self.round_added.len();
            for i in 0..n {
                if let Some(&(key, reusable)) = self.round_added.get(i)
                    && !reusable
                {
                    self.remove_added(key);
                }
            }
        }
        self.clear_ephemeral();
    }

    fn remove_added(&mut self, key: AddedKey) {
        match key {
            AddedKey::Trait(k) => {
                self.traits.remove(&k);
            }
            AddedKey::Impls(d, s) => {
                self.impls.remove(&(d, s));
                self.impl_cols.remove(&(d, s));
            }
            AddedKey::Header(k) => {
                self.headers.remove(&k);
            }
            AddedKey::Adt(k) => {
                self.adts.remove(&k);
            }
            AddedKey::Sig(k) => {
                self.sigs.remove(&k);
            }
            AddedKey::Assoc(d) => {
                self.assoc.remove(&d);
            }
            AddedKey::Lang(l) => {
                self.lang.remove(&l);
            }
        }
    }

    fn clear_ephemeral(&mut self) {
        self.opaques.clear();
        self.hidden_bodies.clear();
        self.holds.clear();
        self.cond_args.clear();
        self.specializes.clear();
        self.conditions.clear();
        self.seen.clear();
        self.memo.clear();
        self.body_replayed.get_mut().clear();
        self.round_added.clear();
        for &d in self.round_inner_names.iter() {
            self.names.remove(&d);
        }
        self.round_inner_names.clear();
        self.retries = 0;
        self.counted = Converted::default();
    }

    /// Whether any cached entry from an earlier body was hit during the active round.
    pub fn has_replayed_reads(&self) -> bool {
        !self.body_replayed.borrow().is_empty()
    }

    /// Visits every `(Question, digest)` read replayed from stretch-cached entries touched
    /// during the active round.
    pub fn for_each_replayed_read(&self, mut f: impl FnMut(Question, u64)) {
        let spans = self.body_replayed.borrow();
        for &span in spans.iter() {
            for &(q, d) in span.get(&self.cached_reads) {
                f(q, d);
            }
        }
    }

    /// Marks `touched_gen` for the active reused round and appends `reads` to the replay log
    /// if this entry came from an earlier body and has not yet been touched in this round.
    #[inline]
    fn touch_row(&self, touched_gen: &Cell<u32>, reads: Span) {
        if self.reuse_mode && touched_gen.get() != self.body_gen {
            touched_gen.set(self.body_gen);
            if reads.len > 0 {
                self.body_replayed.borrow_mut().push(reads);
            }
        }
    }

    fn record_name(&mut self, d: u128, trait_: ItemKey, name: Name) {
        let inserted = self.names.insert(d, (trait_, name)).is_none();
        if inserted && self.reuse_mode && is_inner_def(trait_.0) {
            self.round_inner_names.push(d);
        }
    }

    fn push_read<W: World>(&mut self, reads: &Reads<'_, '_, W>, q: Question, reusable: &mut bool) {
        if let Some(d) = reads.recorded(q) {
            self.cached_reads.push((q, d));
        } else {
            *reusable = false;
        }
    }

    /// Whether the value of `cond` at `args` is kept.
    pub fn const_holds_known(&self, cond: SDef, args: &[STy]) -> bool {
        self.holds.contains_key(&(cond.0, args_key(args)))
    }

    /// Keeps the value of `cond` at `args`; other arguments with the same key are a refusal.
    pub fn keep_const_holds(&mut self, cond: SDef, args: &[STy], v: ConstHolds) -> Result<(), Stop> {
        let key = (cond.0, args_key(args));
        if let Some(&(span, _)) = self.holds.get(&key) {
            if span.get(&self.cond_args) != args {
                return Err(CheckError::Malformed(Malformed::ConditionKey { cond: ItemKey(cond.0) }).into());
            }
            return Ok(());
        }
        let start = self.cond_args.len();
        self.cond_args.extend_from_slice(args);
        self.holds.insert(key, (Span::of(&self.cond_args, start), v));
        Ok(())
    }

    /// Keeps whether impl `a` specializes impl `b`.
    pub fn keep_specializes(&mut self, a: SDef, b: SDef, v: bool) {
        self.specializes.insert((a.0, b.0), v);
    }

    /// The `i`th impl `where const` condition read, with its impl's trait.
    pub fn const_condition(&self, i: usize) -> Option<(SDef, u128)> {
        self.conditions.get(i).map(|&(c, t)| (SDef(c), t))
    }

    /// Why `cond` fails: a value kept as false or failed, else, when the bound is over a
    /// generic parameter, that it is not decided.
    pub fn reason(&self, cond: SDef, generic: bool) -> Option<crate::diag::Because> {
        let k = ItemKey(cond.0);
        // The solver never evaluates a condition over a parameter, so no kept answer is this
        // bound's own.
        if generic {
            return Some(crate::diag::Because::Generic(k));
        }
        let mut kept = self.holds.iter().filter(|((c, _), _)| *c == cond.0).map(|(_, &(_, v))| v);
        let worst = kept.try_fold(None, |acc: Option<ConstHolds>, v| match v {
            ConstHolds::Error => Err(()),
            ConstHolds::False => Ok(Some(ConstHolds::False)),
            ConstHolds::True => Ok(acc),
        });
        match worst {
            Err(()) => Some(crate::diag::Because::Error(k)),
            Ok(Some(_)) => Some(crate::diag::Because::False(k)),
            Ok(None) => None,
        }
    }

    /// Whether the cache answers `q`.
    pub fn has(&self, q: SQ) -> bool {
        match q {
            SQ::TraitDef(d) => self.traits.contains_key(&d.0),
            SQ::Impls(d, s) => self.impls.contains_key(&(d.0, s)),
            SQ::ImplHeader(d) => self.headers.contains_key(&d.0),
            SQ::Adt(d) => self.adts.contains_key(&d.0),
            SQ::FnSig(d) => self.sigs.contains_key(&d.0),
            SQ::Assoc(d) => self.assoc.contains_key(&d.0),
            SQ::Opaque(d) | SQ::OpaqueHidden(d) => self.opaques.contains_key(&d.0),
            SQ::Lang(l) => self.lang.contains_key(&l),
            SQ::IsLocal(_) => false,
            // Its arguments are terms of the context: `InferCtx::fill` reads it.
            SQ::ConstHolds(..) => false,
            SQ::Specializes(a, b) => self.specializes.contains_key(&(a.0, b.0)),
        }
    }

    /// Reads `q` through `reads` and keeps its answer (see the module).
    pub fn fill<W: World>(&mut self, q: SQ, reads: &mut Reads<'_, '_, W>) -> Result<(), Stop> {
        let kept = self.has(q);
        if let Some(t) = reads.tally() {
            t.solve.fills += 1;
            t.solve.fill_misses += u64::from(!kept);
            t.solve.misses_by_kind[q.kind()] += u64::from(!kept);
        }
        if kept {
            return Ok(());
        }
        let at = reads.stamp();
        let before = self.counted;
        let out = self.fill_one(q, reads);
        let end = reads.stamp();
        let c = self.counted;
        if let Some(t) = reads.tally() {
            t.conversions += c.tables - before.tables;
            t.entries_converted += c.converted - before.converted;
            t.entries_reached += c.reached - before.reached;
            t.entries_offered += c.offered - before.offered;
            t.convert_ns += end.saturating_sub(at);
        }
        out
    }

    fn fill_one<W: World>(&mut self, q: SQ, reads: &mut Reads<'_, '_, W>) -> Result<(), Stop> {
        match q {
            SQ::TraitDef(d) => self.fill_trait(ItemKey(d.0), reads),
            SQ::Impls(d, s) => {
                let holes_before = reads.hole_count();
                let reads_start = self.cached_reads.len();
                let got = reads.impls_of(ItemKey(d.0), shape(s))?;
                #[cfg(test)]
                let got = if self.first_impl_only { &got[..got.len().min(1)] } else { got };
                // Every header first: the bucket's fingerprint columns are made from them, so
                // the solver drops an impl whose head cannot match the goal before it imports
                // the header (prefilter.rs, trait_goals.rs `assemble_impls`). The solver read
                // every one of them before this too (each was a candidate), one stop and
                // retry per header; now the bucket's are read in this one fill.
                let mut all_headers_reusable = true;
                for k in got {
                    if let Some(existing) = self.headers.get(&k.0) {
                        self.touch_row(&existing.touched_gen, existing.reads);
                        all_headers_reusable &= existing.reusable && !is_inner_def(k.0);
                    } else {
                        self.fill_impl(*k, reads)?;
                        if let Some(added) = self.headers.get(&k.0) {
                            all_headers_reusable &= added.reusable && !is_inner_def(k.0);
                        } else {
                            all_headers_reusable = false;
                        }
                    }
                }
                let start = self.impl_defs.len();
                self.impl_defs.extend(got.iter().map(|k| SDef(k.0)));
                let span = Span::of(&self.impl_defs, start);
                let mut h = StableHasher::new("blaze-typeck impls");
                self.fp_scratch.clear();
                for k in got {
                    h.u128(k.0);
                    let r = self.headers.get(&k.0).ok_or(CheckError::Malformed(Malformed::NoRecord(Question::ImplHeader(*k))))?;
                    // The columns come from the headers: a header's change changes this answer.
                    h.u64(r.digest);
                    let fp = prefilter::fingerprint_impl(self.portable(r.por), r.args);
                    self.fp_scratch.push(fp);
                }
                let stride = prefilter::columns_of(&self.fp_scratch, &mut self.col_scratch);
                let at = u32::try_from(self.impl_columns.len()).unwrap_or(u32::MAX);
                self.impl_columns.extend_from_slice(&self.col_scratch);
                let mut reusable = self.reuse_mode
                    && all_headers_reusable
                    && !is_inner_def(d.0)
                    && !shape_is_body_local(s)
                    && reads.hole_count() == holes_before;
                let row_reads = if self.reuse_mode {
                    let s_start = self.cached_reads.len();
                    self.push_read(reads, Question::ImplsOf(ItemKey(d.0), shape(s)), &mut reusable);
                    Span::of(&self.cached_reads, s_start)
                } else {
                    let _ = reads_start;
                    Span::default()
                };
                self.impl_cols.insert((d.0, s), (at, stride));
                self.impls.insert(
                    (d.0, s),
                    ImplsRow { span, digest: h.finish(), reads: row_reads, touched_gen: Cell::new(self.body_gen) },
                );
                if self.reuse_mode {
                    self.round_added.push((AddedKey::Impls(d.0, s), reusable));
                }
                Ok(())
            }
            SQ::ImplHeader(d) => self.fill_impl(ItemKey(d.0), reads),
            SQ::Adt(d) => self.fill_adt(ItemKey(d.0), reads),
            SQ::FnSig(d) => self.fill_sig(ItemKey(d.0), reads),
            SQ::Assoc(d) => self.fill_assoc(d.0, reads),
            SQ::Opaque(d) | SQ::OpaqueHidden(d) => self.fill_opaque(ItemKey(d.0), reads),
            SQ::Lang(l) => {
                let holes_before = reads.hole_count();
                let reads_start = self.cached_reads.len();
                let mut reusable = self.reuse_mode;
                let item = lang_item(l);
                let got = match item.assoc_type() {
                    // An associated type: `assoc_def(trait, name)`, the one name the solver
                    // gives it (`LangItem::assoc_type`), once the trait is seen to declare it.
                    Some((trait_lang, name)) => {
                        let found_trait = reads.lang_item(trait_lang)?;
                        if self.reuse_mode {
                            self.push_read(reads, Question::Lang(trait_lang), &mut reusable);
                        }
                        match found_trait {
                            Some(t) => {
                                let name = Name::of(name);
                                let assoc = reads.assoc_item(t, name)?;
                                if self.reuse_mode {
                                    self.push_read(reads, Question::AssocItem(t, name), &mut reusable);
                                }
                                match assoc {
                                    Some(a) if a.kind == crate::world::AssocKind::Type => Some(ItemKey(assoc_def(t, name))),
                                    _ => {
                                        return Err(CheckError::Malformed(Malformed::NoRecord(Question::AssocItem(t, name))).into());
                                    }
                                }
                            }
                            None => None,
                        }
                    }
                    None => {
                        let got = reads.lang_item(item)?;
                        if self.reuse_mode {
                            self.push_read(reads, Question::Lang(item), &mut reusable);
                        }
                        got
                    }
                };
                if reads.hole_count() > holes_before || got.is_some_and(|k| is_inner_def(k.0)) {
                    reusable = false;
                }
                let row_reads = if self.reuse_mode { Span::of(&self.cached_reads, reads_start) } else { Span::default() };
                self.lang.insert(l, LangRow { def: got.map(|k| SDef(k.0)), reads: row_reads, touched_gen: Cell::new(self.body_gen) });
                if self.reuse_mode {
                    self.round_added.push((AddedKey::Lang(l), reusable));
                }
                Ok(())
            }
            // Coherence's question; a body's check runs in analysis mode, which never asks it.
            SQ::IsLocal(d) => Err(CheckError::Malformed(Malformed::NoRecord(Question::Attrs(ItemKey(d.0)))).into()),
            // `InferCtx::fill` answers it: its arguments are terms of the context.
            SQ::ConstHolds(cond, _) => Err(CheckError::Malformed(Malformed::NotGround { cond: ItemKey(cond.0) }).into()),
            // `InferCtx::fill` answers it: the answer is a solve of its own.
            SQ::Specializes(a, _) => Err(CheckError::Malformed(Malformed::NoRecord(Question::ImplHeader(ItemKey(a.0)))).into()),
        }
    }

    /// Opens `t`, the table of the answer to `q`, for conversion: converts nothing; `ty` and
    /// `list` then convert exactly what the answer's handles reach (D62 stage one).
    fn table<'t>(&mut self, q: Question, t: PTys<'t>) -> Conv<'t> {
        self.counted.tables += 1;
        self.counted.offered += t.len() as u64;
        // The table's identity for this call's memo: the world's answers are borrowed for the
        // whole call, so two answers whose tables start at one address are one table (the
        // address is never part of a key, a record or an output; it only finds the memo).
        let id = t.identity();
        let next = self.seen.len() as u32;
        let tab = *self.seen.entry(id).or_insert(next);
        Conv { q, tab, t }
    }

    /// The call's one erased region node.
    fn erased(&mut self) -> STy {
        match self.erased {
            Some(e) => e,
            None => {
                let e = STy(self.nodes.len() as u32);
                self.nodes.push(K::ReErased);
                self.erased = Some(e);
                e
            }
        }
    }

    /// The solver term of `root` of `c`'s table: converted with every node it reaches that
    /// this call has not converted yet, children first, by a work list; a lookup when it has.
    /// A child must come before its parent in the table (as the solver's portable tables
    /// require), so the walk ends.
    fn ty(&mut self, c: Conv<'_>, root: PTy) -> Result<STy, Stop> {
        if let Some(&s) = self.memo.get(&(c.tab, root.0)) {
            self.counted.reached += 1;
            return Ok(s);
        }
        self.work.clear();
        self.work.push((root.0, false));
        while let Some((n, expanded)) = self.work.pop() {
            if self.memo.contains_key(&(c.tab, n)) {
                continue;
            }
            let node = c.t.node(PTy(n)).map_err(Stop::from)?;
            if !expanded {
                self.work.push((n, true));
                self.kids.clear();
                crate::portable::node_children(c.t, node, &mut self.kids)?;
                for &ch in &self.kids {
                    if ch >= n {
                        return Err(CheckError::Malformed(Malformed::TypeOrder { node: n, child: ch }).into());
                    }
                    self.work.push((ch, false));
                }
                continue;
            }
            let k = self.convert(c, node)?;
            let s = STy(self.nodes.len() as u32);
            self.nodes.push(k);
            self.memo.insert((c.tab, n), s);
            self.counted.converted += 1;
            self.counted.reached += 1;
        }
        self.memo.get(&(c.tab, root.0)).copied().ok_or(CheckError::Malformed(Malformed::TypeOrder { node: root.0, child: root.0 }).into())
    }

    /// The list `l` of `c`'s table as a list of solver terms, its terms converted first.
    fn list(&mut self, c: Conv<'_>, l: crate::ids::List) -> Result<SList, Stop> {
        let mut terms: Vec<STy, Alloc<'a>> = Vec::with_capacity_in(l.len as usize, self.alloc);
        for i in l.range() {
            let p = c.t.list_slot(i as usize).ok_or(CheckError::Malformed(Malformed::NoRecord(c.q)))?;
            terms.push(self.ty(c, p)?);
        }
        let start = self.lists.len() as u32;
        self.lists.extend_from_slice(&terms);
        Ok(SList { start, len: l.len })
    }

    /// The solver node of `n`, whose children are converted already.
    fn convert(&mut self, c: Conv<'_>, n: PNode) -> Result<K, Stop> {
        let (q, t, tab) = (c.q, c.t, c.tab);
        let erased = self.erased().0;
        let memo = &self.memo;
        let lists = &mut self.lists;
        let ty = |p: PTy| -> Result<STy, Stop> {
            memo.get(&(tab, p.0)).copied().ok_or(CheckError::Malformed(Malformed::TypeOrder { node: p.0, child: p.0 }).into())
        };
        let mut list = |l: crate::ids::List| -> Result<SList, Stop> {
            let start = lists.len() as u32;
            for i in l.range() {
                let p = t.list_slot(i as usize).ok_or(CheckError::Malformed(Malformed::NoRecord(q)))?;
                lists.push(ty(p)?);
            }
            Ok(SList { start, len: l.len })
        };
        {
            let k = match n {
                PNode::Bool => K::Bool,
                PNode::Char => K::Char,
                PNode::Int(i) => K::Int(int(i)),
                PNode::Uint(u) => K::Uint(uint(u)),
                PNode::Float(f) => K::Float(float(f)),
                PNode::Str => K::Str,
                PNode::Never => K::Never,
                PNode::Tuple(l) => K::Tuple(list(l)?),
                PNode::Record(schema, l) => K::Record(schema.0, list(l)?),
                PNode::Union(l) => K::Union(list(l)?),
                PNode::Ref(m, p) => K::Ref(STy(erased), ty(p)?, mutbl(m)),
                PNode::Ptr(m, p) => K::RawPtr(ty(p)?, mutbl(m)),
                PNode::Array(e, n) => K::Array(ty(e)?, ty(n)?),
                PNode::Slice(e) => K::Slice(ty(e)?),
                PNode::Pat(b, l) => K::Pat(ty(b)?, list(l)?),
                PNode::Adt(k, l) => K::Adt(SDef(k.0), list(l)?),
                PNode::FnDef(k, l) => K::FnDef(SDef(k.0), list(l)?),
                PNode::FnPtr(l, h) => K::FnPtr(list(l)?, h),
                PNode::Param(i) => K::Param(i),
                PNode::ConstParam(i) => K::ConstParam(i),
                PNode::ConstValue { ty: vt, bits } => {
                    let vt = match t.node(vt).map_err(Stop::from)? {
                        PNode::Bool => blaze_solve::term::ValTy::Bool,
                        PNode::Char => blaze_solve::term::ValTy::Char,
                        PNode::Int(i) => blaze_solve::term::ValTy::Int(int(i)),
                        PNode::Uint(u) => blaze_solve::term::ValTy::Uint(uint(u)),
                        // An enum's variant (`PortableTys::variant_value`).
                        PNode::Adt(k, l) if l.len == 0 => blaze_solve::term::ValTy::Enum(SDef(k.0)),
                        _ => return Err(CheckError::Malformed(Malformed::ConstValueType { node: vt.0 }).into()),
                    };
                    K::ConstValue(vt, bits)
                }
                PNode::Alias { trait_, name, args } => {
                    let d = assoc_def(trait_, name);
                    if self.names.insert(d, (trait_, name)).is_none() && self.reuse_mode && is_inner_def(trait_.0) {
                        self.round_inner_names.push(d);
                    }
                    K::Alias(Rigid::No, SAlias::Projection, SDef(d), list(args)?)
                }
                PNode::Opaque(k, l) => K::Alias(Rigid::No, SAlias::Opaque, SDef(k.0), list(l)?),
                PNode::Error => K::Error,
                PNode::Dynamic(l) => K::Dynamic(list(l)?, STy(erased)),
                PNode::ExTrait(k, l) => K::ExTrait(SDef(k.0), list(l)?),
                PNode::ExProj { trait_, name, args, term } => {
                    let d = assoc_def(trait_, name);
                    if self.names.insert(d, (trait_, name)).is_none() && self.reuse_mode && is_inner_def(trait_.0) {
                        self.round_inner_names.push(d);
                    }
                    K::ExProj(SDef(d), list(args)?, ty(term)?)
                }
                PNode::ExAuto(k) => K::ExAuto(SDef(k.0)),
                PNode::Foreign(k) => K::Foreign(SDef(k.0)),
                PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } | PNode::Infer => {
                    return Err(CheckError::Malformed(Malformed::BodyTypeInRecord(q)).into());
                }
            };
            Ok(k)
        }
    }

    /// Converts clauses over `c`'s table into `self.preds`, converting what they reach.
    fn clauses(&mut self, cs: &[PClause], c: Conv<'_>) -> Result<(), Stop> {
        for cl in cs {
            let p = match *cl {
                PClause::Trait { trait_, args } => {
                    Pred::Trait { trait_: SDef(trait_.0), args: self.list(c, args)?, polarity: Polarity::Positive }
                }
                PClause::Projection { trait_, args, name, term } => {
                    let d = assoc_def(trait_, name);
                    self.record_name(d, trait_, name);
                    let args = self.list(c, args)?;
                    let term = self.ty(c, term)?;
                    let alias = self.nodes.len() as u32;
                    self.nodes.push(K::Alias(Rigid::No, SAlias::Projection, SDef(d), args));
                    Pred::Projection { alias: STy(alias), term }
                }
                PClause::Const { cond, args } => {
                    #[cfg(test)]
                    if self.drop_const_clauses {
                        continue;
                    }
                    Pred::ConstHolds { cond: SDef(cond.0), args: self.list(c, args)? }
                }
                PClause::ForFields { container, trait_, args } => Pred::ForFields {
                    container: self.ty(c, container)?,
                    trait_: SDef(trait_.0),
                    args_after_self: self.list(c, args)?,
                },
                // Outlives clauses are the borrow checker's (D148); the checker takes them as true.
                PClause::TypeOutlives { .. } | PClause::RegionOutlives { .. } => continue,
            };
            self.preds.push(p);
        }
        Ok(())
    }

    /// An answer's portable table is the call's whole converted table (its handles are
    /// absolute): every node an answer reaches was converted into it, once per call.
    fn start(&self) -> Por {
        Por::default()
    }

    fn end(&self, mut p: Por) -> Por {
        p.nodes.len = self.nodes.len() as u32;
        p.lists.len = self.lists.len() as u32;
        p
    }

    /// The own type and const parameter count of trait `trait_`'s associated type `name`
    /// (`Question::AssocGenerics`; the Reference, "Associated Types": "Associated types may
    /// include generic parameters and where clauses"), `trait_generics` the count the
    /// trait's definition gave when the caller holds it: an answer that counts the trait
    /// differently is malformed. A world that keeps no count (`None`,
    /// [`World::assoc_generics`]'s default) files none.
    fn own_generics<W: World>(trait_: ItemKey, name: Name, trait_generics: Option<u32>, reads: &mut Reads<'_, '_, W>) -> Result<u32, Stop> {
        match reads.assoc_generics(trait_, name)? {
            Some(g) if trait_generics.is_some_and(|n| n != g.parent) => {
                let trait_generics = trait_generics.unwrap_or(g.parent);
                Err(CheckError::Malformed(Malformed::AssocGenerics { trait_, name, trait_generics, answered: g.parent }).into())
            }
            Some(g) => Ok(g.own),
            None => Ok(0),
        }
    }

    fn fill_trait<W: World>(&mut self, k: ItemKey, reads: &mut Reads<'_, '_, W>) -> Result<(), Stop> {
        let holes_before = reads.hole_count();
        let reads_start = self.cached_reads.len();
        let mut reusable = self.reuse_mode && !is_inner_def(k.0);
        let q = Question::TraitDef(k);
        let Some(t) = reads.trait_def(k)? else { return Err(CheckError::Malformed(Malformed::NoRecord(q)).into()) };
        if self.reuse_mode {
            self.push_read(reads, q, &mut reusable);
        }
        let por = self.start();
        let c = self.table(q, t.types);
        let supers = self.preds.len();
        self.clauses(t.supertraits, c)?;
        let supers = Span::of(&self.preds, supers);
        let assoc = self.assoc_tys.len();
        // An associated type's item bounds (`type A: Bound;` and its implied `Sized`) are the
        // predicates of its own item, each with `Self` the alias `<Self as Trait<..>>::A` over
        // the trait's parameters, as an opaque type's bounds are its key's predicates
        // (`fill_opaque`). The Reference, "Associated Items", "Associated Types": "The optional
        // trait bounds must be fulfilled by the implementations of the type alias. There is an
        // implicit `Sized` bound on associated types that can be relaxed using the special
        // `?Sized` bound.": every implementation meets them, so the solver assembles them for a
        // rigid alias (`<S as Serializer>::SerializeStruct: Sized` in a body generic over `S`).
        let items = self.preds.len();
        let mut bounds_digest = 0u64;
        for a in t.assoc_types {
            let d = assoc_def(k, a.name);
            self.record_name(d, k, a.name);
            let default = match a.default {
                Some(p) => Some(self.ty(c, p)?),
                None => None,
            };
            let bq = Question::Predicates(a.item);
            let bounds = reads.predicates(a.item)?;
            if self.reuse_mode {
                self.push_read(reads, bq, &mut reusable);
            }
            let bc = self.table(bq, bounds.types);
            let start = self.preds.len();
            self.clauses(bounds.clauses, bc)?;
            // A synthetic associated type's item is its trait fn's opaque, whose predicates have
            // `Self` the opaque: made the projection's own (`item_bounds::opaque_self_as_projection`).
            // A written associated type's bounds have `Self` the projection already: kept.
            self.opaque_self_as_projection(start, a.item, SDef(d))?;
            // In the records' order of the associated types, so equal answers of two types
            // do not cancel.
            bounds_digest = bounds_digest.rotate_left(1) ^ digest("blaze-typeck assoc bounds", &reads.digest(bq)?);
            // Each written bound with its trait's supertraits, transitively (`type Error:
            // Error;` under `trait Error: Sized + StdError` is a `StdError` too): the solver
            // reads these as they are given (`item_bounds::elaborate_item_bounds`).
            let supers_read = self.elaborate_item_bounds(start, reads)?;
            if self.reuse_mode {
                let end = self.preds.len();
                for idx in start..end {
                    if let Some(&Pred::Trait { trait_: st, polarity: Polarity::Positive, .. }) = self.preds.get(idx) {
                        self.push_read(reads, Question::TraitDef(ItemKey(st.0)), &mut reusable);
                    }
                }
            }
            bounds_digest = bounds_digest.rotate_left(1) ^ digest("blaze-typeck assoc bound supertraits", &supers_read);
            // The associated type's own parameters (`type Out<T>;`), after the trait's: part
            // of the trait's facts and of their digest.
            let own_generics = Self::own_generics(k, a.name, Some(t.generics), reads)?;
            if self.reuse_mode {
                self.push_read(reads, Question::AssocGenerics(k, a.name), &mut reusable);
            }
            bounds_digest = bounds_digest.rotate_left(1) ^ digest("blaze-typeck assoc generics", &u64::from(own_generics));
            self.assoc_tys.push(AssocTy {
                def: SDef(d),
                own_generics,
                bounds_start: (start - items) as u32,
                bounds_len: (self.preds.len() - start) as u32,
                default,
            });
        }
        let item_bounds = Span::of(&self.preds, items);
        let assoc = Span::of(&self.assoc_tys, assoc);
        let f = t.flags;
        let mut flags = 0u16;
        for (b, s) in [
            (crate::world::TraitFlags::AUTO, STraitFlags::AUTO),
            (crate::world::TraitFlags::MARKER, STraitFlags::MARKER),
            (crate::world::TraitFlags::UNSAFE, STraitFlags::UNSAFE),
            (crate::world::TraitFlags::FUNDAMENTAL, STraitFlags::FUNDAMENTAL),
            (crate::world::TraitFlags::COINDUCTIVE, STraitFlags::COINDUCTIVE),
            (crate::world::TraitFlags::CONST, STraitFlags::CONST),
        ] {
            if f.has(b) {
                flags |= s;
            }
        }
        // The rule's verdict (`crate::dyn_compat`), three ways: compatible, not (a refusal), or
        // a gap the world could not decide. A gap is no "not": it sets `DYN_UNDECIDED`, and a
        // solver goal that needs the verdict stops naming the trait (blaze-solve builtin.rs
        // `dyn_compatible`, rule 3) instead of answering No for every `dyn Tr + Send: Send`.
        let dyn_verdict = crate::dyn_compat::verdict(reads, k)?;
        match dyn_verdict {
            crate::dyn_compat::Verdict::Compatible => flags |= STraitFlags::DYN_COMPATIBLE,
            crate::dyn_compat::Verdict::Gap { .. } => {
                flags |= STraitFlags::DYN_UNDECIDED;
                reusable = false;
            }
            crate::dyn_compat::Verdict::Not { .. } => {}
        }
        if self.reuse_mode && reusable {
            // Replay-capture the exact supertrait `TraitDef` reads that `dyn_compat::verdict`
            // performed in the same BFS order and stopping at the same `by` trait.
            let mut list = [ItemKey(0); crate::dyn_compat::MAX_SUPERTRAITS];
            list[0] = k;
            let mut len = 1usize;
            let mut at = 0usize;
            while at < len {
                let Some(&cur) = list.get(at) else {
                    reusable = false;
                    break;
                };
                at += 1;
                if cur != k {
                    self.push_read(reads, Question::TraitDef(cur), &mut reusable);
                }
                if matches!(dyn_verdict, crate::dyn_compat::Verdict::Not { by, member: None, .. } if by == cur) {
                    break;
                }
                if let Some(def) = reads.trait_def(cur)? {
                    for clause in def.supertraits {
                        let PClause::Trait { trait_: st, args } = *clause else { continue };
                        let Ok(list_args) = def.types.list(args) else {
                            reusable = false;
                            break;
                        };
                        let Some((&first, _)) = list_args.split_first() else { continue };
                        if def.types.node(first).ok() != Some(PNode::Param(0)) {
                            continue;
                        }
                        self.push_read(reads, Question::TraitDef(st), &mut reusable);
                        if reads.trait_def(st)?.is_some_and(|d| d.lang == Some(LangItem::Sized)) {
                            continue;
                        }
                        if list.get(..len).is_some_and(|s| s.contains(&st)) {
                            continue;
                        }
                        if let Some(slot) = list.get_mut(len) {
                            *slot = st;
                            len += 1;
                        } else {
                            reusable = false;
                            break;
                        }
                    }
                } else {
                    reusable = false;
                }
                if matches!(dyn_verdict, crate::dyn_compat::Verdict::Not { by, .. } if by == cur) {
                    break;
                }
            }
        }
        let lang = match t.lang.and_then(solver_lang) {
            Some(l) => Some(l),
            None => {
                let is_try_as_dyn = reads.lang_item(LangItem::TryAsDyn)? == Some(k);
                if self.reuse_mode {
                    self.push_read(reads, Question::Lang(LangItem::TryAsDyn), &mut reusable);
                }
                is_try_as_dyn.then_some(Lang::TryAsDyn)
            }
        };
        if reads.hole_count() > holes_before {
            reusable = false;
        }
        let row_reads = if self.reuse_mode { Span::of(&self.cached_reads, reads_start) } else { Span::default() };
        let row = TraitRow {
            flags: STraitFlags(flags),
            generics: t.generics,
            lang,
            supers,
            assoc,
            item_bounds,
            por: self.end(por),
            digest: digest("blaze-typeck trait", &reads.digest(q)?) ^ digest("blaze-typeck trait item bounds", &bounds_digest),
            reads: row_reads,
            touched_gen: Cell::new(self.body_gen),
            reusable,
        };
        self.traits.insert(k.0, row);
        if self.reuse_mode {
            self.round_added.push((AddedKey::Trait(k.0), reusable));
        }
        Ok(())
    }

    fn fill_impl<W: World>(&mut self, k: ItemKey, reads: &mut Reads<'_, '_, W>) -> Result<(), Stop> {
        let holes_before = reads.hole_count();
        let reads_start = self.cached_reads.len();
        let mut reusable = self.reuse_mode && !is_inner_def(k.0);
        let q = Question::ImplHeader(k);
        let Some(h) = reads.impl_header(k)? else { return Err(CheckError::Malformed(Malformed::NoRecord(q)).into()) };
        let Some(trait_) = h.trait_ else { return Err(CheckError::Malformed(Malformed::NoRecord(q)).into()) };
        if is_inner_def(trait_.0) {
            reusable = false;
        }
        if self.reuse_mode {
            self.push_read(reads, q, &mut reusable);
            // `Reads::impl_header` checks trait arity via `reads.trait_def(trait_)`.
            self.push_read(reads, Question::TraitDef(trait_), &mut reusable);
        }
        let wc = reads.predicates(k)?;
        if self.reuse_mode {
            self.push_read(reads, Question::Predicates(k), &mut reusable);
        }
        let por = self.start();
        let c = self.table(q, h.types);
        // The trait's arguments, self first: a list of its own.
        let self_ty = self.ty(c, h.self_ty)?;
        let mut terms: Vec<STy, Alloc<'a>> = Vec::with_capacity_in(h.trait_args.list().len as usize + 1, self.alloc);
        terms.push(self_ty);
        for i in h.trait_args.list().range() {
            let p = h.types.list_slot(i).ok_or(CheckError::Malformed(Malformed::NoRecord(q)))?;
            terms.push(self.ty(c, p)?);
        }
        let args = SList { start: self.lists.len() as u32, len: terms.len() as u32 };
        self.lists.extend_from_slice(&terms);
        let assoc = self.impl_assoc.len();
        for &(name, v) in h.assoc_types {
            let d = assoc_def(trait_, name);
            self.record_name(d, trait_, name);
            let value = self.ty(c, v)?;
            // An impl's `type Out<X> = ..;` has the parameters its trait declares for `Out`
            // (the value is over the impl's generics, then those): the count is the trait's.
            let own_generics = Self::own_generics(trait_, name, None, reads)?;
            if self.reuse_mode {
                self.push_read(reads, Question::AssocGenerics(trait_, name), &mut reusable);
            }
            self.impl_assoc.push(ImplAssoc { trait_item: SDef(d), value, own_generics, is_default: false });
        }
        let assoc = Span::of(&self.impl_assoc, assoc);
        let wcc = self.table(Question::Predicates(k), wc.types);
        let wheres = self.preds.len();
        self.clauses(wc.clauses, wcc)?;
        let wheres = Span::of(&self.preds, wheres);
        for c in wc.clauses {
            if let PClause::Const { cond, .. } = *c {
                self.conditions.push((cond.0, trait_.0));
                reusable = false;
            }
        }
        if reads.hole_count() > holes_before {
            reusable = false;
        }
        let row_reads = if self.reuse_mode { Span::of(&self.cached_reads, reads_start) } else { Span::default() };
        let row = ImplRow {
            generics: h.generics,
            polarity: h.polarity,
            trait_: SDef(trait_.0),
            args,
            wheres,
            assoc,
            por: self.end(por),
            digest: digest("blaze-typeck impl", &reads.digest(q)?) ^ digest("blaze-typeck impl where", &reads.digest(Question::Predicates(k))?),
            reads: row_reads,
            touched_gen: Cell::new(self.body_gen),
            reusable,
        };
        self.headers.insert(k.0, row);
        if self.reuse_mode {
            self.round_added.push((AddedKey::Header(k.0), reusable));
        }
        Ok(())
    }

    fn fill_adt<W: World>(&mut self, k: ItemKey, reads: &mut Reads<'_, '_, W>) -> Result<(), Stop> {
        let holes_before = reads.hole_count();
        let reads_start = self.cached_reads.len();
        let mut reusable = self.reuse_mode && !is_inner_def(k.0);
        let q = Question::Adt(k);
        let Some(a) = reads.adt(k)? else { return Err(CheckError::Malformed(Malformed::NoRecord(q)).into()) };
        if self.reuse_mode {
            self.push_read(reads, q, &mut reusable);
        }
        let wc = reads.predicates(k)?;
        if self.reuse_mode {
            self.push_read(reads, Question::Predicates(k), &mut reusable);
        }
        let mut flags = match a.kind {
            AdtKind::Struct => AdtFlags::STRUCT,
            AdtKind::Enum => {
                if a.variants.iter().all(|v| v.ctor == crate::world::CtorKind::Unit) {
                    AdtFlags::ENUM | AdtFlags::UNIT_ENUM
                } else {
                    AdtFlags::ENUM
                }
            }
            AdtKind::Union => AdtFlags::UNION,
        };
        for (l, f) in [
            (LangItem::OwnedBox, AdtFlags::BOX),
            (LangItem::PhantomData, AdtFlags::PHANTOM_DATA),
            (LangItem::ManuallyDrop, AdtFlags::MANUALLY_DROP),
        ] {
            let is_lang = reads.lang_item(l)? == Some(k);
            if self.reuse_mode {
                self.push_read(reads, Question::Lang(l), &mut reusable);
            }
            if is_lang {
                flags |= f;
            }
        }
        let drop_lang = reads.lang_item(LangItem::Drop)?;
        if self.reuse_mode {
            self.push_read(reads, Question::Lang(LangItem::Drop), &mut reusable);
        }
        if let Some(drop) = drop_lang {
            let drop_impls = reads.impls_of(drop, Shape::Adt(k))?;
            if self.reuse_mode {
                self.push_read(reads, Question::ImplsOf(drop, Shape::Adt(k)), &mut reusable);
            }
            if !drop_impls.is_empty() {
                flags |= AdtFlags::HAS_DROP;
            }
        }
        let por = self.start();
        let c = self.table(q, a.types);
        let fields = self.fields.len();
        let variants = self.variants.len();
        for v in a.variants {
            let at = (self.fields.len() - fields) as u32;
            for i in v.fields.range() {
                let f = a.fields.get(i).ok_or(CheckError::Malformed(Malformed::NoRecord(q)))?;
                let ty = self.ty(c, f.ty)?;
                self.fields.push(ty);
            }
            self.variants.push((at, v.fields.len));
        }
        let sized_tail = match (a.kind, a.variants) {
            (AdtKind::Struct, [v]) if v.fields.len > 0 => {
                Some(self.fields[fields + v.fields.len as usize - 1])
            }
            _ => None,
        };
        // Only an enum's discriminant type is read (any other type's is `u8`).
        // For structs, read `AUTO_COPY_PUB` (Blaze edition transparent all-`pub` structs).
        let mut auto_copy_pub = false;
        let discr = match a.kind {
            AdtKind::Enum => {
                let attrs = reads.attrs(k)?;
                if self.reuse_mode {
                    self.push_read(reads, Question::Attrs(k), &mut reusable);
                }
                match attrs.discr() {
                    crate::world::DiscrRepr::Unread => blaze_solve::facts::Discr::Unread,
                    crate::world::DiscrRepr::Int(i) => blaze_solve::facts::Discr::Int(int(i)),
                    crate::world::DiscrRepr::Uint(u) => blaze_solve::facts::Discr::Uint(uint(u)),
                }
            }
            AdtKind::Struct => {
                let attrs = reads.attrs(k)?;
                if self.reuse_mode {
                    self.push_read(reads, Question::Attrs(k), &mut reusable);
                }
                if attrs.0 & crate::world::TypeAttrs::AUTO_COPY_PUB != 0 {
                    flags |= AdtFlags::AUTO_COPY_PUB;
                    auto_copy_pub = true;
                }
                blaze_solve::facts::Discr::Unread
            }
            AdtKind::Union => blaze_solve::facts::Discr::Unread,
        };
        let fields = Span::of(&self.fields, fields);
        let variants = Span::of(&self.variants, variants);
        let wcc = self.table(Question::Predicates(k), wc.types);
        let wheres = self.preds.len();
        self.clauses(wc.clauses, wcc)?;
        let wheres = Span::of(&self.preds, wheres);
        if reads.hole_count() > holes_before {
            reusable = false;
        }
        let row_reads = if self.reuse_mode { Span::of(&self.cached_reads, reads_start) } else { Span::default() };
        let row = AdtRow {
            flags: AdtFlags(flags),
            generics: a.generics,
            fields,
            variants,
            sized_tail,
            discr,
            wheres,
            por: self.end(por),
            digest: digest("blaze-typeck adt", &reads.digest(q)?)
                ^ digest("blaze-typeck adt where", &reads.digest(Question::Predicates(k))?)
                ^ digest("blaze-typeck adt discr", &(discr_code(discr) as u32))
                ^ digest("blaze-typeck adt auto_copy_pub", &u32::from(auto_copy_pub)),
            reads: row_reads,
            touched_gen: Cell::new(self.body_gen),
        };
        self.adts.insert(k.0, row);
        if self.reuse_mode {
            self.round_added.push((AddedKey::Adt(k.0), reusable));
        }
        Ok(())
    }

    fn fill_sig<W: World>(&mut self, k: ItemKey, reads: &mut Reads<'_, '_, W>) -> Result<(), Stop> {
        let holes_before = reads.hole_count();
        let reads_start = self.cached_reads.len();
        let mut reusable = self.reuse_mode && !is_inner_def(k.0) && k != self.reuse_owner;
        let q = Question::FnSig(k);
        let Some(s) = reads.fn_sig(k)? else { return Err(CheckError::Malformed(Malformed::NoRecord(q)).into()) };
        if self.reuse_mode {
            self.push_read(reads, q, &mut reusable);
        }
        let por = self.start();
        let c = self.table(q, s.types);
        let mut terms: Vec<STy, Alloc<'a>> = Vec::with_capacity_in(s.inputs.len() + 1, self.alloc);
        for &p in s.inputs {
            terms.push(self.ty(c, p)?);
        }
        terms.push(self.ty(c, s.output)?);
        let at = self.lists.len() as u32;
        self.lists.extend_from_slice(&terms);
        let io = SList { start: at, len: s.inputs.len() as u32 + 1 };
        // `FnSig::is_fn_trait_compatible` reads the safety and the ABI from the header.
        let mut flags = 0;
        if s.header.safety == crate::ids::Safety::Unsafe {
            flags |= FnSigDef::UNSAFE;
        }
        if s.header.abi != crate::ids::Abi::Rust || s.header.c_variadic {
            flags |= FnSigDef::NOT_FN_ABI;
        }
        if reads.hole_count() > holes_before {
            reusable = false;
        }
        let row_reads = if self.reuse_mode { Span::of(&self.cached_reads, reads_start) } else { Span::default() };
        let row = SigRow {
            generics: s.parent_count + s.own_count,
            flags,
            io,
            por: self.end(por),
            digest: digest("blaze-typeck sig", &reads.digest(q)?),
            reads: row_reads,
            touched_gen: Cell::new(self.body_gen),
        };
        self.sigs.insert(k.0, row);
        if self.reuse_mode {
            self.round_added.push((AddedKey::Sig(k.0), reusable));
        }
        Ok(())
    }

    fn fill_assoc<W: World>(&mut self, d: u128, reads: &mut Reads<'_, '_, W>) -> Result<(), Stop> {
        let holes_before = reads.hole_count();
        // An associated type the cache named by `assoc_def`: its trait's generics.
        if let Some(&(trait_, _)) = self.names.get(&d) {
            if let Some(existing) = self.traits.get(&trait_.0) {
                self.touch_row(&existing.touched_gen, existing.reads);
            } else {
                self.fill_trait(trait_, reads)?;
            }
            let (generics, own_generics, row_reads, trait_reusable) = match self.traits.get(&trait_.0) {
                Some(t) => {
                    let own = t.assoc.get(&self.assoc_tys).iter().find(|a| a.def == SDef(d)).map_or(0, |a| a.own_generics);
                    (t.generics, own, t.reads, t.reusable)
                }
                None => (0, 0, Span::default(), false),
            };
            // The count is part of what the associated type is: two traits' types never
            // share `d`, but one type whose count changed is another answer.
            let digest = digest("blaze-typeck assoc", &ItemKey(d)) ^ digest("blaze-typeck assoc own generics", &u64::from(own_generics));
            let reusable = self.reuse_mode && trait_reusable && !is_inner_def(trait_.0) && reads.hole_count() == holes_before;
            self.assoc.insert(
                d,
                AssocRow {
                    def: AssocDef {
                        container: SDef(trait_.0),
                        trait_: Some(SDef(trait_.0)),
                        kind: AssocKind::Type,
                        parent_generics: generics,
                        own_generics,
                        digest,
                    },
                    reads: row_reads,
                    touched_gen: Cell::new(self.body_gen),
                },
            );
            if self.reuse_mode {
                self.round_added.push((AddedKey::Assoc(d), reusable));
            }
            return Ok(());
        }
        let reads_start = self.cached_reads.len();
        let mut reusable = self.reuse_mode && !is_inner_def(d);
        let q = Question::AssocParent(ItemKey(d));
        let Some(p) = reads.assoc_parent(ItemKey(d))? else {
            return Err(CheckError::Malformed(Malformed::NoRecord(q)).into());
        };
        if is_inner_def(p.container.0) {
            reusable = false;
        }
        if self.reuse_mode {
            self.push_read(reads, q, &mut reusable);
        }
        let (trait_, generics) = match reads.trait_def(p.container)? {
            Some(t) => {
                if self.reuse_mode {
                    self.push_read(reads, Question::TraitDef(p.container), &mut reusable);
                }
                (Some(SDef(p.container.0)), t.generics)
            }
            None => {
                if self.reuse_mode {
                    self.push_read(reads, Question::TraitDef(p.container), &mut reusable);
                }
                match reads.impl_header(p.container)? {
                    Some(h) => {
                        if self.reuse_mode {
                            self.push_read(reads, Question::ImplHeader(p.container), &mut reusable);
                            if let Some(t) = h.trait_ {
                                if is_inner_def(t.0) {
                                    reusable = false;
                                }
                                self.push_read(reads, Question::TraitDef(t), &mut reusable);
                            }
                        }
                        (h.trait_.map(|t| SDef(t.0)), h.generics)
                    }
                    None => return Err(CheckError::Malformed(Malformed::NoRecord(q)).into()),
                }
            }
        };
        let kind = match p.kind {
            crate::world::AssocKind::Fn { .. } => AssocKind::Fn,
            crate::world::AssocKind::Const => AssocKind::Const,
            crate::world::AssocKind::Type => AssocKind::Type,
        };
        // A trait's associated type named by its own item: its own parameters as the world
        // counts them. A fn's own generics are its signature's, not filed here.
        let own_generics = match (kind, trait_) {
            (AssocKind::Type, Some(t)) if t == SDef(p.container.0) => {
                let own = Self::own_generics(p.container, p.name, Some(generics), reads)?;
                if self.reuse_mode {
                    self.push_read(reads, Question::AssocGenerics(p.container, p.name), &mut reusable);
                }
                own
            }
            _ => 0,
        };
        if reads.hole_count() > holes_before {
            reusable = false;
        }
        let row_reads = if self.reuse_mode { Span::of(&self.cached_reads, reads_start) } else { Span::default() };
        let digest = digest("blaze-typeck assoc parent", &p) ^ digest("blaze-typeck assoc own generics", &u64::from(own_generics));
        self.assoc.insert(
            d,
            AssocRow {
                def: AssocDef { container: SDef(p.container.0), trait_, kind, parent_generics: generics, own_generics, digest },
                reads: row_reads,
                touched_gen: Cell::new(self.body_gen),
            },
        );
        if self.reuse_mode {
            self.round_added.push((AddedKey::Assoc(d), reusable));
        }
        Ok(())
    }

    fn fill_opaque<W: World>(&mut self, k: ItemKey, reads: &mut Reads<'_, '_, W>) -> Result<(), Stop> {
        let q = Question::Opaque(k);
        let hidden = reads.opaque_hidden(k)?;
        let bounds = reads.predicates(k)?;
        let por = self.start();
        // The hidden type's term. A closure or an async block in it, at its root
        // (`fn f() -> impl Fn(u8) -> u8 { |x| x }`, an `async fn`) or below it
        // (`Map<Filter<Enumerate<..>, {closure#0}>, {closure#1}>`, blaze-mem's `Slab::iter_mut`),
        // is made from its owner's facts first ([`Self::body_terms`]); the rest is converted
        // around them. Each one's question is folded into the digest below (rule 21).
        let mut bodies: Vec<Question, Alloc<'a>> = Vec::new_in(self.alloc);
        let hidden_ty = match hidden {
            Some(h) => {
                let c = self.table(q, h.types);
                self.body_terms(q, h, &mut bodies, reads)?;
                Some(self.ty(c, h.ty)?)
            }
            None => None,
        };
        let bc = self.table(Question::Predicates(k), bounds.types);
        let start = self.preds.len();
        self.clauses(bounds.clauses, bc)?;
        // Each written bound with its trait's supertraits, transitively (`impl FnMut(u8) -> u16`
        // is filed as `Opaque: FnMut<(u8,)>` and `<Opaque as FnOnce<(u8,)>>::Output == u16`,
        // and `FnMut<Args>: FnOnce<Args>`): the solver reads a rigid alias's bounds as they are
        // given (blaze-solve alias.rs `push_alias_bounds`), as for an associated type's item
        // bounds (`item_bounds::elaborate_item_bounds`). The Reference, "Traits",
        // "Supertraits": "anywhere a generic or trait object is bounded by a trait, it has
        // access to the associated items of its supertraits".
        let supers_read = self.elaborate_item_bounds(start, reads)?;
        let mut d = digest("blaze-typeck opaque bounds", &reads.digest(Question::Predicates(k))?);
        d ^= digest("blaze-typeck opaque bound supertraits", &supers_read);
        if hidden.is_some() {
            d ^= digest("blaze-typeck opaque hidden", &reads.digest(q)?);
            // Every closure and async block the hidden type holds, each once (`body_terms`
            // pushes a question once, so no two equal digests cancel in the xor).
            for &bq in bodies.iter() {
                let label = match bq {
                    Question::Coroutine(..) => "blaze-typeck opaque hidden coroutine",
                    _ => "blaze-typeck opaque hidden closure",
                };
                d ^= digest(label, &reads.digest(bq)?);
            }
        }
        let row = OpaqueRow { bounds: Span::of(&self.preds, start), hidden: hidden_ty, por: self.end(por), digest: d };
        self.opaques.insert(k.0, row);
        Ok(())
    }

    /// Puts in the memo the term of every closure and async block that `h` (opaque `q`'s
    /// hidden type) reaches, made from its owner's facts, so `ty` takes it and
    /// never `convert`s it (which refuses a body's type in a record). Each one's question is
    /// pushed to `bodies` once, for the opaque's digest. Children reached through another
    /// record's signature, output or interior precede their parents too.
    fn body_terms<'w, W: World>(
        &mut self,
        q: Question,
        h: crate::world::Hidden<'w>,
        bodies: &mut Vec<Question, Alloc<'a>>,
        reads: &mut Reads<'w, '_, W>,
    ) -> Result<(), Stop> {
        for (q, h) in super::hidden_body_nodes(q, h, reads, self.alloc)? {
            let c = self.table(q, h.types);
            let n = h.ty.0;
            let (which, owner, ordinal, args) = match h.types.node(h.ty)? {
                PNode::Closure { owner, ordinal, args } => (BodyTy::Closure, owner, ordinal, args),
                PNode::Coroutine { owner, ordinal, args } => (BodyTy::Coroutine, owner, ordinal, args),
                _ => return Err(CheckError::Malformed(Malformed::NoRecord(q)).into()),
            };
            let (t, bq, def) = match which {
                BodyTy::Closure => (
                    self.closure_term(c, owner, ordinal, args, reads)?,
                    Question::Closure(owner, ordinal),
                    crate::coroutine_id::closure_of_def(owner, ordinal),
                ),
                BodyTy::Coroutine => (
                    self.coroutine_term(c, owner, ordinal, args, reads)?,
                    Question::Coroutine(owner, ordinal),
                    crate::coroutine_id::coroutine_def(owner, ordinal),
                ),
            };
            self.hidden_bodies.insert(def, HiddenBody { owner, ordinal, args: args.len });
            self.memo.insert((c.tab, n), t);
            if !bodies.contains(&bq) {
                bodies.push(bq);
            }
        }
        Ok(())
    }

    /// Refuses by name `args` (of a closure or async block in the hidden type or its records)
    /// unless they are its owner's type and const parameters in declaration order: writeback writes
    /// them so, the parameters the owner's facts are in, so no substitution is made.
    fn owner_identity(hc: Conv<'_>, args: crate::ids::List, what: &'static str) -> Result<(), Stop> {
        for (index, &p) in hc.t.list(args)?.iter().enumerate() {
            if !matches!(hc.t.node(p)?, PNode::Param(j) | PNode::ConstParam(j) if j as usize == index) {
                return Err(CheckError::Unported(what).into());
            }
        }
        Ok(())
    }

    /// An async block in an opaque's hidden type (an `async fn`'s is its root block's future by
    /// identity, `PNode::Coroutine`, writeback): the solver's `Coroutine(coroutine_def(owner,
    /// ordinal), [output, interior, owner arguments..])` from the owner's facts.
    fn coroutine_term<W: World>(
        &mut self,
        hc: Conv<'_>,
        owner: ItemKey,
        ordinal: u32,
        args: crate::ids::List,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<STy, Stop> {
        Self::owner_identity(hc, args, "an opaque's hidden coroutine at arguments other than its owner's own")?;
        let cq = Question::Coroutine(owner, ordinal);
        let Some(facts) = reads.coroutine(owner, ordinal)? else {
            return Err(CheckError::Malformed(Malformed::BodyTypeInRecord(cq)).into());
        };
        let c = self.table(cq, facts.types);
        let o = self.ty(c, facts.output)?;
        let i = self.ty(c, facts.interior)?;
        let mut owner_args = Vec::with_capacity_in(args.len as usize, self.alloc);
        for &p in hc.t.list(args)? {
            owner_args.push(self.ty(hc, p)?);
        }
        let start = self.lists.len() as u32;
        self.lists.push(o);
        self.lists.push(i);
        self.lists.extend_from_slice(&owner_args);
        let t = STy(self.nodes.len() as u32);
        self.nodes.push(K::Coroutine(SDef(crate::coroutine_id::coroutine_def(owner, ordinal)), SList { start, len: 2 + args.len }));
        Ok(t)
    }

    /// A closure of the defining body in an opaque's hidden type: the term a body makes for it
    /// (infer/mod.rs `TyView::ClosureOf`): def from its identity, args `[owner args.., kind,
    /// rust-call sig, environment]` from the owner's check (`Question::Closure`).
    /// `hc` is the hidden type's table, opened.
    fn closure_term<W: World>(
        &mut self,
        hc: Conv<'_>,
        owner: ItemKey,
        ordinal: u32,
        args: crate::ids::List,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<STy, Stop> {
        Self::owner_identity(hc, args, "an opaque's hidden closure at arguments other than its owner's own")?;
        let cq = Question::Closure(owner, ordinal);
        let Some((facts, upvars)) = reads.closure(owner, ordinal)? else {
            return Err(CheckError::Malformed(Malformed::BodyTypeInRecord(cq)).into());
        };
        let mut all: Vec<STy, Alloc<'a>> = Vec::with_capacity_in(args.len as usize + 3, self.alloc);
        for &p in hc.t.list(args)? {
            all.push(self.ty(hc, p)?);
        }
        // The kind: the solver's `CLOSURE_FN*` value as a `usize` constant.
        let kind = match facts.kind {
            crate::ty::ClosureKind::Fn => blaze_solve::term::CLOSURE_FN,
            crate::ty::ClosureKind::FnMut => blaze_solve::term::CLOSURE_FN_MUT,
            crate::ty::ClosureKind::FnOnce => blaze_solve::term::CLOSURE_FN_ONCE,
        };
        let k = STy(self.nodes.len() as u32);
        self.nodes.push(K::ConstValue(blaze_solve::term::ValTy::Uint(uint(UintTy::Usize)), u128::from(kind)));
        all.push(k);
        // The rust-call signature (`rust_call_sig`): `fn((inputs..,)) -> output`.
        let fc = self.table(cq, facts.types);
        let PNode::FnPtr(io, _) = facts.types.node(facts.sig).map_err(Stop::from)? else {
            return Err(CheckError::Malformed(Malformed::NoRecord(cq)).into());
        };
        let Some(last) = io.len.checked_sub(1) else {
            return Err(CheckError::Malformed(Malformed::NoRecord(cq)).into());
        };
        let mut ins: Vec<STy, Alloc<'a>> = Vec::with_capacity_in(last as usize, self.alloc);
        let mut out = None;
        for (n, i) in io.range().enumerate() {
            let p = facts.types.list_slot(i as usize).ok_or(CheckError::Malformed(Malformed::NoRecord(cq)))?;
            let s = self.ty(fc, p)?;
            if n as u32 == last { out = Some(s) } else { ins.push(s) }
        }
        let out = out.ok_or(CheckError::Malformed(Malformed::NoRecord(cq)))?;
        let start = self.lists.len() as u32;
        self.lists.extend_from_slice(&ins);
        let tupled = STy(self.nodes.len() as u32);
        self.nodes.push(K::Tuple(SList { start, len: last }));
        let start = self.lists.len() as u32;
        self.lists.push(tupled);
        self.lists.push(out);
        let sig = STy(self.nodes.len() as u32);
        self.nodes.push(K::FnPtr(SList { start, len: 2 }, blaze_solve::term::FnPtrHeader::RUST));
        all.push(sig);
        let def = SDef(crate::coroutine_id::closure_of_def(owner, ordinal));
        let env = match upvars {
            Some(p) => self.ty(fc, p)?,
            None => {
                let env = STy(self.nodes.len() as u32);
                self.nodes.push(K::Foreign(def));
                env
            }
        };
        all.push(env);
        let start = self.lists.len() as u32;
        self.lists.extend_from_slice(&all);
        let t = STy(self.nodes.len() as u32);
        self.nodes.push(K::Closure(def, SList { start, len: all.len() as u32 }));
        Ok(t)
    }

    fn portable(&self, p: Por) -> Portable<'_> {
        Portable { nodes: p.nodes.get(&self.nodes), lists: p.lists.get(&self.lists) }
    }

    /// The allocator the cache is in.
    pub fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }
}

impl Facts for FactCache<'_> {
    fn trait_def(&self, _: Seal, t: SDef) -> Fetch<TraitDef<'_>> {
        let r = self.traits.get(&t.0).ok_or(Missing)?;
        self.touch_row(&r.touched_gen, r.reads);
        Ok(TraitDef {
            flags: r.flags,
            generics: r.generics,
            lang: r.lang,
            supertraits: r.supers.get(&self.preds),
            where_clauses: &[],
            assoc: r.assoc.get(&self.assoc_tys),
            item_bounds: r.item_bounds.get(&self.preds),
            portable: self.portable(r.por),
            digest: r.digest,
        })
    }

    fn impls(&self, _: Seal, t: SDef, shape: SShape) -> Fetch<ImplSet<'_>> {
        let r = self.impls.get(&(t.0, shape)).ok_or(Missing)?;
        let &(at, stride) = self.impl_cols.get(&(t.0, shape)).ok_or(Missing)?;
        let impls = r.span.get(&self.impl_defs);
        if self.reuse_mode && r.touched_gen.get() != self.body_gen {
            self.touch_row(&r.touched_gen, r.reads);
            for &imp in impls {
                if let Some(h) = self.headers.get(&imp.0) {
                    self.touch_row(&h.touched_gen, h.reads);
                }
            }
        }
        // `fill_one` writes both entries together, the columns `WIDTH * stride` bytes at `at`.
        let at_usize = usize::try_from(at).unwrap_or(usize::MAX);
        let col_len = prefilter::WIDTH.saturating_mul(usize::try_from(stride).unwrap_or(usize::MAX));
        let bytes = self.impl_columns.get(at_usize..at_usize.saturating_add(col_len)).ok_or(Missing)?;
        let len = u32::try_from(impls.len()).unwrap_or(u32::MAX);
        Ok(ImplSet { impls, columns: Columns { bytes, stride, len }, digest: r.digest })
    }

    fn impl_header(&self, _: Seal, i: SDef) -> Fetch<ImplHeader<'_>> {
        let r = self.headers.get(&i.0).ok_or(Missing)?;
        self.touch_row(&r.touched_gen, r.reads);
        Ok(ImplHeader {
            generics: r.generics,
            polarity: r.polarity,
            flags: ImplFlags(0),
            trait_: r.trait_,
            args: r.args,
            where_clauses: r.wheres.get(&self.preds),
            assoc: r.assoc.get(&self.impl_assoc),
            portable: self.portable(r.por),
            digest: r.digest,
        })
    }

    fn adt(&self, _: Seal, d: SDef) -> Fetch<AdtDef<'_>> {
        let r = self.adts.get(&d.0).ok_or(Missing)?;
        self.touch_row(&r.touched_gen, r.reads);
        Ok(AdtDef {
            flags: r.flags,
            generics: r.generics,
            fields: r.fields.get(&self.fields),
            variants: r.variants.get(&self.variants),
            sized_tail: r.sized_tail,
            discr: r.discr,
            where_clauses: r.wheres.get(&self.preds),
            portable: self.portable(r.por),
            digest: r.digest,
        })
    }

    fn fn_sig(&self, _: Seal, d: SDef) -> Fetch<FnSigDef<'_>> {
        let r = self.sigs.get(&d.0).ok_or(Missing)?;
        self.touch_row(&r.touched_gen, r.reads);
        Ok(FnSigDef { generics: r.generics, inputs_output: r.io, flags: r.flags, portable: self.portable(r.por), digest: r.digest })
    }

    fn assoc(&self, _: Seal, d: SDef) -> Fetch<AssocDef> {
        let r = self.assoc.get(&d.0).ok_or(Missing)?;
        self.touch_row(&r.touched_gen, r.reads);
        Ok(r.def)
    }

    fn opaque(&self, _: Seal, d: SDef) -> Fetch<OpaqueDef<'_>> {
        let r = self.opaques.get(&d.0).ok_or(Missing)?;
        // The world's opaque answer does not carry its generics yet: 0 until it does.
        Ok(OpaqueDef {
            generics: 0,
            bounds: r.bounds.get(&self.preds),
            hidden: r.hidden,
            portable: self.portable(r.por),
            digest: r.digest,
        })
    }

    fn lang(&self, _: Seal, l: Lang) -> Fetch<Option<SDef>> {
        let r = self.lang.get(&l).ok_or(Missing)?;
        self.touch_row(&r.touched_gen, r.reads);
        Ok(r.def)
    }

    fn is_local(&self, _: Seal, _: SDef) -> Fetch<bool> {
        Err(Missing)
    }

    fn const_holds(&self, _: Seal, cond: SDef, args: &[STy]) -> Fetch<ConstHolds> {
        match self.holds.get(&(cond.0, args_key(args))) {
            Some(&(span, v)) if span.get(&self.cond_args) == args => Ok(v),
            // Absent, or other arguments under the key: the fill refuses the second.
            _ => Err(Missing),
        }
    }

    fn specializes(&self, _: Seal, a: SDef, b: SDef) -> Fetch<bool> {
        self.specializes.get(&(a.0, b.0)).copied().ok_or(Missing)
    }
}
