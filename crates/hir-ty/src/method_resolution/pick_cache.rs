//! The pick cache and the autoderef steps cache (notes: docs/v2/plan/lanes/frontend/fast-method.md).
//!
//! A probe's answer is a function of the inputs [`PickKey`] names, its reads aside. For a
//! receiver with no inference variables those are known before the probe, so the answer is
//! stored once per database, crate and revision and shared by every body: a body that takes it
//! replays the reads the probe made (`blaze_query::db::replay_reads`), so it is checked again when any of
//! them changes, as the solver's shared cache does (`next_solver/interner/tls_cache.rs`).
//!
//! The autoderef steps of a canonical receiver are stored the same way, by what
//! `method_autoderef_steps` reads: rustc's `method_autoderef_steps` query.

#[allow(unsafe_code)]
mod table;
#[cfg(test)]
mod tests;

use std::{
    hash::{Hash, Hasher},
    sync::LazyLock,
};

use base_db::Nonce;
use blaze_hash::FixedHasher;
use blaze_query::db::{AsId, Reads, View};
use hir_expand::name::Name;
use rustc_type_ir::{
    RegionKind, TypeFoldable, TypeFolder, TypeSuperFoldable, TypeVisitableExt, inherent::IntoKind,
};

use self::table::{Key, Table};
use super::{
    CandidateSource, MethodError, MethodResolutionContext,
    probe::{MethodAutoderefStepsResult, Mode, Pick, PickKind, PickResult},
    scope_sets::ScopeItems,
};
use crate::next_solver::{
    Canonical, Const, DbInterner, PredefinedOpaques, Region, RegionExt, Ty, TypingMode,
};

/// What a probe found out about itself that decides whether, and under which key, its answer is
/// kept.
#[derive(Default)]
pub(super) struct ProbeFacts {
    /// The steps had no bad type and did not reach the recursion limit.
    pub(super) cacheable: bool,
    /// An inherent candidate's visibility was checked and is `Visibility::Module`, so the answer
    /// depends on the module of the call.
    pub(super) module_dependent: bool,
}

/// Every input of a pick but its reads (the notes' section 3.2 lists where the probe reads each).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct PickKey {
    nonce: Nonce,
    solver_crate: u32,
    resolver_crate: u32,
    /// 0, or the module of the call when the answer depends on it.
    module: u32,
    /// The receiver with its free regions erased.
    self_ty: u32,
    name: u32,
    /// The traits in scope with an item of the name.
    scope: u32,
    param_env: u32,
    typing_defs: u32,
    /// The typing mode's tag (`TYPING_TAG`) and the `FLAG_*` bits.
    bits: u32,
}

const TYPING_TAG: u32 = 0b1111;
const FLAG_PATH: u32 = 1 << 4;
const FLAG_ALLOW_UNSTABLE: u32 = 1 << 5;
const FLAG_REGIONS: u32 = 1 << 6;
const FLAG_EDITION_2021: u32 = 1 << 7;
const FLAG_EDITION_2024: u32 = 1 << 8;
const FLAG_ARBITRARY_SELF_TYPES: u32 = 1 << 9;
const FLAG_ARBITRARY_SELF_TYPES_POINTERS: u32 = 1 << 10;
const FLAG_SUPERTRAIT_ITEM_SHADOWING: u32 = 1 << 11;

impl Key for PickKey {
    #[inline]
    fn hint(&self) -> u64 {
        let mut hasher = FixedHasher::default();
        self.nonce.hash(&mut hasher);
        hasher.write_u64(pair(self.solver_crate, self.resolver_crate));
        hasher.write_u64(pair(self.module, self.self_ty));
        hasher.write_u64(pair(self.name, self.scope));
        hasher.write_u64(pair(self.param_env, self.typing_defs));
        hasher.write_u32(self.bits);
        hasher.finish() | 1
    }
}

/// Two `u32` fields as one word of a hash.
#[inline]
fn pair(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// What the pick table holds for a key in a revision.
enum Cached {
    /// The answer depends on the module of the call: it is under the key with the module.
    ByModule,
    Answer(Answer, Reads),
}

/// A probe's answer, with the lifetime of the interner it was made through erased (`erase`).
enum Answer {
    Found(Pick<'static>),
    NoMatch,
    Ambiguity(Box<[CandidateSource]>),
    PrivateMatch(Pick<'static>),
}

/// Picks of every database alive, `1 << PICK_BITS` of them: a crate's distinct (receiver, name,
/// scope) triples checked in one revision, across every crate being checked.
const PICK_BITS: u32 = 16;

static PICKS: LazyLock<Table<PickKey, View, Cached>> =
    LazyLock::new(|| Table::new(PICK_BITS));

/// The answer of the probe for `name` on `self_ty` in `ctx`: from the table when it holds it,
/// else `probe`'s, kept when its shape allows (`PickKey::new`, `Answer::of`).
pub(super) fn pick<'db>(
    ctx: &MethodResolutionContext<'_, 'db>,
    mode: Mode,
    name: Name,
    self_ty: Ty<'db>,
    allow_unstable: bool,
    scope: ScopeItems,
    probe: impl FnOnce(&mut ProbeFacts) -> PickResult<'db>,
) -> PickResult<'db> {
    let db = ctx.infcx.interner.db;
    let (nonce, revision) = db.nonce_and_revision();
    let Some(mut key) = PickKey::new(ctx, nonce, mode, name, self_ty, allow_unstable, scope) else {
        // Outside the cache's domain the probe is the one path.
        return probe(&mut ProbeFacts::default());
    };
    {
        let guard = blaze_reclaim::enter();
        let mut found = PICKS.get(&guard, &key, revision);
        if let Some(Cached::ByModule) = found {
            key.module = ctx.resolver.module().as_id().as_u32();
            found = PICKS.get(&guard, &key, revision);
        }
        if let Some(Cached::Answer(answer, reads)) = found {
            blaze_query::db::replay_reads(db, *reads);
            let result = answer.to_result();
            #[cfg(test)]
            tests::on_hit(answer, &result, || probe(&mut ProbeFacts::default()));
            return result;
        }
    }
    let mut facts = ProbeFacts::default();
    let (result, reads) = blaze_query::db::capture_reads(db, || probe(&mut facts));
    if let Some(reads) = reads
        && facts.cacheable
        && let Some(answer) = Answer::of(&result)
    {
        let guard = blaze_reclaim::enter();
        if facts.module_dependent && key.module == 0 {
            PICKS.insert(&guard, key, revision, Cached::ByModule);
            key.module = ctx.resolver.module().as_id().as_u32();
        }
        PICKS.insert(&guard, key, revision, Cached::Answer(answer, reads));
    }
    result
}

impl PickKey {
    /// The key of a probe, or `None` when its answer may depend on the body's inference state:
    /// a receiver with inference variables (after resolving them), placeholders, opaque types or
    /// errors, a param env with variables or placeholders, or opaque types in the body's storage.
    fn new<'db>(
        ctx: &MethodResolutionContext<'_, 'db>,
        nonce: Nonce,
        mode: Mode,
        name: Name,
        self_ty: Ty<'db>,
        allow_unstable: bool,
        scope: ScopeItems,
    ) -> Option<PickKey> {
        let infcx = ctx.infcx;
        let self_ty =
            if self_ty.has_infer() { infcx.deeply_resolve_ignoring_regions(self_ty) } else { self_ty };
        if self_ty.has_non_region_infer()
            || self_ty.has_placeholders()
            || self_ty.has_opaque_types()
            || self_ty.references_error()
        {
            return None;
        }
        let clauses = ctx.param_env.clauses;
        if clauses.has_infer() || clauses.has_placeholders() {
            return None;
        }
        if !infcx.inner.borrow().opaque_type_storage.is_empty() {
            return None;
        }
        let interner = infcx.interner;
        let (typing_tag, typing_defs) = typing_mode_key(infcx.typing_mode_raw());
        let features = ctx.features;
        let bits = typing_tag
            | flag(mode == Mode::Path, FLAG_PATH)
            | flag(allow_unstable, FLAG_ALLOW_UNSTABLE)
            | flag(interner.regions(), FLAG_REGIONS)
            | flag(ctx.edition.at_least_2021(), FLAG_EDITION_2021)
            | flag(ctx.edition.at_least_2024(), FLAG_EDITION_2024)
            | flag(features.arbitrary_self_types, FLAG_ARBITRARY_SELF_TYPES)
            | flag(features.arbitrary_self_types_pointers, FLAG_ARBITRARY_SELF_TYPES_POINTERS)
            | flag(features.supertrait_item_shadowing, FLAG_SUPERTRAIT_ITEM_SHADOWING);
        Some(PickKey {
            nonce,
            solver_crate: interner.expect_crate().as_id().as_u32(),
            resolver_crate: ctx.resolver.krate().as_id().as_u32(),
            module: 0,
            self_ty: erase_free_regions(interner, self_ty).index(),
            name: name.symbol().id(),
            scope: scope.id(),
            param_env: clauses.index(),
            typing_defs,
            bits,
        })
    }
}

#[inline]
fn flag(on: bool, bit: u32) -> u32 {
    if on { bit } else { 0 }
}

/// The typing mode as its tag (at most `TYPING_TAG`) and its list of def ids' index.
fn typing_mode_key(mode: TypingMode<'_>) -> (u32, u32) {
    match mode {
        TypingMode::Coherence => (0, 0),
        TypingMode::Typeck { defining_opaque_types_and_generators: defs } => (1, defs.index()),
        TypingMode::PostTypeckThroughBorrowck { defining_opaque_types: defs } => {
            (2, defs.index())
        }
        TypingMode::PostBorrowck { defined_opaque_types: defs } => (3, defs.index()),
        TypingMode::Reflection => (4, 0),
        TypingMode::PostAnalysis => (5, 0),
        TypingMode::Codegen => (6, 0),
        TypingMode::ErasedNotCoherence(_) => (7, 0),
    }
}

impl Answer {
    /// `result` as kept, if it may be: a pick or `NoMatch`, `Ambiguity` or `PrivateMatch`, with
    /// no inference variable or placeholder in it.
    fn of(result: &PickResult<'_>) -> Option<Answer> {
        Some(match result {
            Ok(pick) if is_global(pick) => {
                // clone: a miss, once per key and revision; the caller keeps its own.
                Answer::Found(erase::pick(pick.clone()))
            }
            Err(MethodError::NoMatch) => Answer::NoMatch,
            // alloc: a miss that is an error, once per key and revision.
            Err(MethodError::Ambiguity(sources)) => Answer::Ambiguity(sources.as_slice().into()),
            Err(MethodError::PrivateMatch(pick)) if is_global(pick) => {
                // clone: as for `Found`.
                Answer::PrivateMatch(erase::pick(pick.clone()))
            }
            Ok(_)
            | Err(
                MethodError::PrivateMatch(_)
                | MethodError::IllegalSizedBound { .. }
                | MethodError::ErrorReported,
            ) => return None,
        })
    }

    fn to_result<'db>(&self) -> PickResult<'db> {
        match self {
            // A pick's two lists are empty but for shadowed supertrait items and unstable
            // candidates set aside.
            // clone: a copy of the pick that allocates nothing for empty lists.
            Answer::Found(pick) => Ok(erase::unerase_pick(pick).clone()),
            Answer::NoMatch => Err(MethodError::NoMatch),
            // alloc: the error's list, on the error path.
            Answer::Ambiguity(sources) => Err(MethodError::Ambiguity(sources.to_vec())),
            Answer::PrivateMatch(pick) => {
                // clone: as for `Found`, on the error path.
                Err(MethodError::PrivateMatch(erase::unerase_pick(pick).clone()))
            }
        }
    }
}

fn is_global(pick: &Pick<'_>) -> bool {
    let kind_is_global = match &pick.kind {
        PickKind::WhereClausePick(trait_ref) => {
            !trait_ref.has_infer() && !trait_ref.has_placeholders()
        }
        PickKind::InherentImplPick(_) | PickKind::ObjectPick(_) | PickKind::TraitPick(_) => true,
    };
    kind_is_global && !pick.self_ty.has_infer() && !pick.self_ty.has_placeholders()
}

/// `ty` with every free region `'erased`: what a probe's answer is the same for.
pub(super) fn erase_free_regions<'db>(interner: DbInterner<'db>, ty: Ty<'db>) -> Ty<'db> {
    if !ty.has_free_regions() {
        return ty;
    }
    ty.fold_with(&mut EraseFreeRegions { interner, erased: Region::new_erased(interner) })
}

/// `pick` with its self type's free regions erased, as every pick a probe for a name gives.
pub(super) fn erase_regions_of_pick<'db>(
    interner: DbInterner<'db>,
    mut pick: Pick<'db>,
) -> Pick<'db> {
    pick.self_ty = erase_free_regions(interner, pick.self_ty);
    pick
}

struct EraseFreeRegions<'db> {
    interner: DbInterner<'db>,
    erased: Region<'db>,
}

impl<'db> TypeFolder<DbInterner<'db>> for EraseFreeRegions<'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.interner
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        if ty.has_free_regions() { ty.super_fold_with(self) } else { ty }
    }

    fn fold_const(&mut self, ct: Const<'db>) -> Const<'db> {
        if ct.has_free_regions() { ct.super_fold_with(self) } else { ct }
    }

    fn fold_region(&mut self, region: Region<'db>) -> Region<'db> {
        match region.kind() {
            RegionKind::ReBound(..) => region,
            _ => self.erased,
        }
    }
}

/// What `method_autoderef_steps` reads but its reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct StepsKey {
    nonce: Nonce,
    solver_crate: u32,
    self_ty: u32,
    opaques: u32,
    var_kinds: u32,
    max_universe: u32,
    param_env: u32,
    typing_defs: u32,
    /// The typing mode's tag (`TYPING_TAG`), `FLAG_REGIONS` and the two arbitrary self types
    /// flags.
    bits: u32,
}

impl Key for StepsKey {
    #[inline]
    fn hint(&self) -> u64 {
        let mut hasher = FixedHasher::default();
        self.nonce.hash(&mut hasher);
        hasher.write_u32(self.solver_crate);
        hasher.write_u64(pair(self.self_ty, self.opaques));
        hasher.write_u64(pair(self.var_kinds, self.max_universe));
        hasher.write_u64(pair(self.param_env, self.typing_defs));
        hasher.write_u32(self.bits);
        hasher.finish() | 1
    }
}

struct CachedSteps {
    steps: MethodAutoderefStepsResult<'static>,
    reads: Reads,
}

/// Steps of every database alive, `1 << STEPS_BITS` of them: the distinct canonical receivers
/// probed in a revision.
const STEPS_BITS: u32 = 14;

static STEPS: LazyLock<Table<StepsKey, View, CachedSteps>> =
    LazyLock::new(|| Table::new(STEPS_BITS));

/// Runs `f` on the autoderef steps of `input`: the table's, borrowed in place, when it holds
/// them, else `compute`'s, kept when their reads were captured.
pub(super) fn with_steps<'db, R>(
    ctx: &MethodResolutionContext<'_, 'db>,
    input: &Canonical<'db, (Ty<'db>, PredefinedOpaques<'db>)>,
    compute: impl FnOnce() -> MethodAutoderefStepsResult<'db>,
    f: impl FnOnce(&MethodAutoderefStepsResult<'db>) -> R,
) -> R {
    let interner = ctx.infcx.interner;
    let db = interner.db;
    let clauses = ctx.param_env.clauses;
    if clauses.has_infer() || clauses.has_placeholders() {
        // Outside the cache's domain the steps are made for this probe alone.
        return f(&compute());
    }
    let (nonce, revision) = db.nonce_and_revision();
    let (typing_tag, typing_defs) = typing_mode_key(ctx.infcx.typing_mode_raw());
    let (self_ty, opaques) = input.value;
    let key = StepsKey {
        nonce,
        solver_crate: interner.expect_crate().as_id().as_u32(),
        self_ty: self_ty.index(),
        opaques: opaques.index(),
        var_kinds: input.var_kinds.index(),
        max_universe: input.max_universe.as_u32(),
        param_env: clauses.index(),
        typing_defs,
        bits: typing_tag
            | flag(interner.regions(), FLAG_REGIONS)
            | flag(ctx.features.arbitrary_self_types, FLAG_ARBITRARY_SELF_TYPES)
            | flag(
                ctx.features.arbitrary_self_types_pointers,
                FLAG_ARBITRARY_SELF_TYPES_POINTERS,
            ),
    };
    let guard = blaze_reclaim::enter();
    if let Some(cached) = STEPS.get(&guard, &key, revision) {
        blaze_query::db::replay_reads(db, cached.reads);
        return f(erase::unerase_steps(&cached.steps));
    }
    let (steps, reads) = blaze_query::db::capture_reads(db, compute);
    match reads {
        Some(reads) => {
            let cached =
                STEPS.insert(&guard, key, revision, CachedSteps { steps: erase::steps(steps), reads });
            f(erase::unerase_steps(&cached.steps))
        }
        None => f(&steps),
    }
}

/// A pick's and the steps' lifetime, erased for the tables and given back to their readers.
///
/// The `'db` of these types marks interned handles (types, generic argument lists, bound
/// variable lists: an index into a process-wide table that is never freed, and a
/// `PhantomData<&'db ()>`) and `blaze_query::db::Id`s; none is a reference into the database. A table entry
/// is handed out only to a probe of the database its key's nonce names, in the revision its
/// state names, so every handle in it was made by that database as it still is.
///
/// The safe version is a lift: a fold over every handle of the pick or of each step's canonical
/// type, rebuilding the value on every insert and every hit, where this is a move or a borrow.
#[allow(unsafe_code)]
mod erase {
    use std::mem;

    use super::{MethodAutoderefStepsResult, Pick};

    pub(super) fn pick(pick: Pick<'_>) -> Pick<'static> {
        // SAFETY: `Pick<'static>` is `Pick<'_>` with its one lifetime `'static`, which bounds no
        // reference in it (see the module).
        unsafe { mem::transmute::<Pick<'_>, Pick<'static>>(pick) }
    }

    pub(super) fn unerase_pick<'a, 'db>(pick: &'a Pick<'static>) -> &'a Pick<'db> {
        // SAFETY: as in `pick`, the other way; the caller's database is the entry's (see the
        // module).
        unsafe { mem::transmute::<&'a Pick<'static>, &'a Pick<'db>>(pick) }
    }

    pub(super) fn steps(steps: MethodAutoderefStepsResult<'_>) -> MethodAutoderefStepsResult<'static> {
        // SAFETY: as in `pick`.
        unsafe {
            mem::transmute::<MethodAutoderefStepsResult<'_>, MethodAutoderefStepsResult<'static>>(
                steps,
            )
        }
    }

    pub(super) fn unerase_steps<'a, 'db>(
        steps: &'a MethodAutoderefStepsResult<'static>,
    ) -> &'a MethodAutoderefStepsResult<'db> {
        // SAFETY: as in `unerase_pick`.
        unsafe {
            mem::transmute::<&'a MethodAutoderefStepsResult<'static>, &'a MethodAutoderefStepsResult<'db>>(
                steps,
            )
        }
    }
}
