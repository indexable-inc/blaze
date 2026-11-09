//! blaze's `Specialize` pass, which rustc has none of: a call that passes a constant its callee
//! branches on calls a copy of the callee compiled knowing it, with the branches it decides
//! gone. The call itself does not change: its `func` still names the callee, and the optimized
//! body lists the call in [`Inlined::specialized`]. The code generator calls the copy there
//! ([`specialized_mir_body`]); every other reader (the interpreter, the walk of what a body
//! references) calls the callee, with the same result.
//!
//! What a copy knows of a parameter ([`ArgFact`]) is its class among the values the callee
//! compares it with: one of them (`Is`), or none of them (`IsNot`), so `f(5)` and `f(6)` share
//! one copy of an `f` that only asks `n == 0`. A copy keeps the callee's signature and is passed
//! the same arguments; only its body differs. A parameter a copy may know is a `bool`, `char`,
//! integer or enum the callee never writes, borrows or changes in place, and that it branches
//! on: a `SwitchInt` on it, on its discriminant, or on `it == k` or `it != k`, through copies
//! into temporaries each written once, or (as lowering reads every discriminant into one
//! temporary) on what the switch's own block last wrote into one nothing borrows.
//!
//! A call gets a copy when, decided from the callee's runtime body (so that deciding never asks
//! for an optimized body, and no optimized body waits on itself):
//! - The callee is a Rust-ABI function with a body. Not one named by a symbol (`#[no_mangle]`,
//!   `#[export_name]`, `#[linkage]`, linked by name), whose calls another object may take over;
//!   not `#[track_caller]` (itself or in its trait's declaration), `#[cold]`, `#[naked]`,
//!   `#[optimize(none)]`, `#[rustc_no_mir_inline]` (std's UB-check helpers, called only where UB
//!   checks are on) or an intrinsic; not one whose calls the code generator answers itself
//!   ([`crate::mir::answered_by_codegen`]: a copy would be a body it never runs); not one with
//!   inline assembly (which may define a symbol, and a copy would define it twice) or whose
//!   overflow checks are its callers' (`#[rustc_inherit_overflow_checks]`, which one copy for
//!   callers of every setting can't follow); not a closure or a coroutine. Its crate does not
//!   matter: blaze has every crate's MIR, and the code generator optimizes a copy with the
//!   options of the callee's crate.
//! - Each argument the copy knows is a constant: a literal, a variant, a free `const` item, a
//!   temporary written once with one of those, or a parameter of the copy the call is in that
//!   the copy knows.
//! - What the copy knows decides at least one branch, and the work it removes is at least
//!   [`MIN_SAVES`]: [`SWITCH_COST`] per branch decided and one per statement and terminator of
//!   the blocks it no longer reaches, each [`LOOP_WEIGHT`] times where the block is on a cycle.
//! - Budgets (rule 18), each from one function alone, so that no decision depends on another
//!   function or on the order bodies are optimized in: a function has at most [`MAX_COPIES`]
//!   copies whatever its callers pass, as the classes its own comparisons give the parameters
//!   it specializes on bound them; a callee is at most [`MAX_CALLEE_SIZE`]; one body calls at
//!   most [`MAX_COPIES_PER_BODY`] copies, of at most [`GROWTH_PER_BODY`] together, chosen by
//!   the work they remove (most first, then the smaller; copies that weigh the same are taken
//!   together or not at all), so that blocks a body has in one build and not in another (UB
//!   checks) never change the choice. So the
//!   program's copies are at most `MAX_COPIES` times the code of the functions small enough to
//!   copy, and only those some call reaches are compiled. A copy's own calls get copies the
//!   same way: a call of the copy's function knowing what the copy knows is the copy itself,
//!   and any other copy is one of its callee's `MAX_COPIES`.
//! - No profile reaches MIR. With one, the code generator calls a copy only from a caller the
//!   profile says is hot; the body lists every call that may.
//!
//! `#[inline(never)]` callees get copies: the attribute asks for the function not to be built
//! into its callers, and a copy is still a function of its own, called, with the callee's name
//! in backtraces and its source in debug info, as each instance of a generic function is. GCC
//! draws the same line (its manual, "Common Function Attributes"): `noinline` "prevents a
//! function from being considered for inlining", `noclone` from "cloning - a mechanism that
//! produces specialized copies of functions", and `noipa` implies both. A copy is
//! never the address of the callee: only direct calls are listed, and a function pointer to the
//! callee (a reify, a vtable) names the callee itself, so no address comparison sees a copy.
//!
//! [`Inlined::specialized`]: crate::mir::Inlined::specialized

use base_db::Db;
use blaze_mem::{Frame, Tag, frame};
use blaze_query::db::AsId;
use hir_def::{
    AdtId, FunctionId, HasModule, VariantId, attrs::AttrFlags, signatures::FunctionSignature,
    span_names,
};
use rustc_abi::ExternAbi;
use rustc_type_ir::{TypeVisitableExt, inherent::IntoKind};
use smallvec::SmallVec;

use crate::{
    InferBodyId, MemoryMap,
    db::HirDatabase,
    mir::{
        AggregateKind, ArgFact, BasicBlockId, BinOp, InlinedBody, LocalId, MirBody, MirLowerError,
        MirOptLevel, MirOptions, Operand, OperandKind, Rvalue, Specialization, SpecializedCall,
        Statement, StatementKind, SwitchTargets, Terminator, TerminatorKind, UnOp,
        framed::{Bits, bits_in, block_id, block_index, local_index},
        location::track_caller,
        transform::Ctx,
    },
    next_solver::{
        AliasConstKind, Allocation, ConstKind, FreeConstAliasId, Region, RegionExt, Ty, TyKind,
    },
    traits::ParamEnvAndCrate,
    utils::detect_variant_from_bytes,
};

use super::{Location, Mention, cfg::constant_bits, inline, visit_body};

/// Copies of one function, at most, whatever its callers pass: two `bool`s, or an enum of
/// eight variants it compares.
pub const MAX_COPIES: u32 = 8;
/// Values one parameter may be compared with for a copy to know it.
const MAX_COMPARED: usize = 16;
/// Statements and terminators of a callee's runtime body, at most, for it to be copied.
const MAX_CALLEE_SIZE: u32 = 2048;
/// Copies one body calls, at most.
pub const MAX_COPIES_PER_BODY: usize = 8;
/// The sizes of the copies one body calls, summed, at most.
const GROWTH_PER_BODY: u32 = 4096;
/// The least work a copy must remove.
const MIN_SAVES: u32 = 8;
/// How many times a block on a cycle of the control flow counts.
const LOOP_WEIGHT: u32 = 8;
/// What a decided branch saves: the compare and the jump.
const SWITCH_COST: u32 = 2;
/// Rounds of following a parameter's value through temporaries.
const VIEW_ROUNDS: usize = 4;

type Result<'db, T> = std::result::Result<T, MirLowerError<'db>>;

/// A specialized copy of a function, by what it knows ([`Specialization`]): the instance the
/// code generator compiles for a call [`Inlined::specialized`](crate::mir::Inlined::specialized)
/// lists. One for every call that knows the same, whichever body it is in.
#[blaze_query::interned(debug)]
pub struct CloneId {
    #[returns(ref)]
    pub spec: Specialization,
}

fn clone_loop<'db>(
    _: &'db Db,
    _: blaze_query::db::Id,
    _: CloneId<'db>,
    _: MirOptions,
) -> Result<'db, MirBody<'db>> {
    Err(MirLowerError::Loop)
}

/// The body of the copy `clone` with `options`: the runtime body of `clone.spec(db).base` with
/// what the copy knows folded in and the branches it decides made gotos, then optimized as
/// `options` say, as the base's own body is. Generic, as every optimized body is:
/// monomorphization instantiates it with the call's [`SpecializedCall::args`].
///
/// A copy that is not one the base can have (a fact of a parameter it does not specialize on,
/// a class its comparisons do not give) is an error naming the base and the facts.
///
/// Its value is MIR as every other body's: charged to `mir` and dropped with them once code is
/// stored (`hir::evict::drop_mir`). coord-108 (B5): it was a plain
/// query, kept for the daemon's life and charged to `query`, so the release that dropped 150,000
/// MIR bodies left every specialized copy.
#[blaze_query::tracked(returns(as_ref), cycle_result = clone_loop, lru = 0, mem_tag = Mir)]
pub fn specialized_mir_body<'db>(
    db: &'db Db,
    clone: CloneId<'db>,
    options: MirOptions,
) -> Result<'db, MirBody<'db>> {
    let spec = clone.spec(db);
    let base = spec.base;
    let span = tracing::info_span!(
        target: "blaze",
        "mir specialized copy",
        krate = tracing::field::Empty,
        function = tracing::field::Empty
    );
    let _span =
        span_names::of_body_then(db, base.into(), span, |text| write_facts(spec, text)).entered();
    let refused = |why: &str| {
        let name = FunctionSignature::of(db, base).name.as_str();
        let krate = base.krate(db).named(db);
        let facts = spec.knowing();
        // alloc: error path
        MirLowerError::ImplementationError(format!(
            "no copy of `{name}` in `{krate}`{facts}: {why}"
        ))
    };
    let plan = plan(db, base).as_ref().map_err(|&why| refused(why))?;
    if spec.facts.is_empty() || !spec.facts.is_sorted_by(|a, b| a.0 < b.0) {
        return Err(refused("its facts are not one for each of some parameters, by index"));
    }
    for (index, fact) in spec.facts.iter() {
        let Some(param) = plan.params.iter().find(|it| it.index == *index) else {
            return Err(refused("it does not specialize on that parameter"));
        };
        if !param.admits(fact) {
            return Err(refused("that is not a class its comparisons give the parameter"));
        }
    }
    let runtime = db.runtime_mir_body(base.into())?;
    // clone: the copy this query keeps, which folding and the passes change in place
    let mut body = runtime.clone();
    let optimizing = options.level != MirOptLevel::Off;
    frame::task(Tag::Mir, |outer| -> Result<'db, ()> {
        let ctx = Ctx::new(db, body.owner, outer);
        frame::task(Tag::Mir, |frame| fold(&ctx, &mut body, &spec.facts, frame));
        let caller = Some(InlinedBody::Fn(base));
        super::passes(&ctx, &mut body, options, caller, optimizing, Some(spec))
    })?;
    Ok(body)
}

/// The parameters of a function a copy of it may know, read from its runtime body.
#[derive(Debug, Clone, PartialEq, Eq, Hash, blaze_query::Value)]
pub(super) struct Plan {
    /// Statements and terminators of the runtime body.
    size: u32,
    /// By index, ascending; the function branches on each.
    params: Box<[Param]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, blaze_query::Value)]
struct Param {
    /// In `param_locals`, and among the call's arguments.
    index: u32,
    kind: Kind,
    /// The size in bits of what the function compares: the parameter, or the local an enum's
    /// discriminant is read into. Values are compared truncated to it.
    bits: u32,
    /// What the function compares it with, truncated, sorted, each once.
    compared: Box<[u128]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, blaze_query::Value)]
enum Kind {
    Bool,
    /// An integer or a `char`.
    Int,
    /// An enum, with its discriminants truncated to `bits`, sorted, each once.
    Enum(Box<[u128]>),
}

impl Param {
    /// How many classes the function's comparisons give the parameter.
    fn classes(&self) -> u32 {
        match &self.kind {
            Kind::Bool => 2,
            Kind::Int => self.compared.len() as u32 + 1,
            Kind::Enum(all) => {
                let compared = all.iter().filter(|it| self.compared.binary_search(it).is_ok());
                let compared = compared.count() as u32;
                compared + u32::from(compared < all.len() as u32)
            }
        }
    }

    /// The class of `value`.
    fn class_of(&self, value: u128) -> ArgFact {
        let value = truncate(value, self.bits);
        if self.compared.binary_search(&value).is_ok() {
            return ArgFact::Is(value);
        }
        // clone: the class's values, kept in the key
        self.canonical(self.compared.clone())
    }

    /// The class of a value known to be in `known`, a class of a parameter of the same kind
    /// and size.
    fn class_given(&self, known: &ArgFact) -> Option<ArgFact> {
        match known {
            ArgFact::Is(value) => Some(self.class_of(*value)),
            // Not any of `outside` is not any of the values this function compares with, where
            // they are all in it.
            ArgFact::IsNot(outside) => {
                let all = self.compared.iter().all(|it| outside.binary_search(it).is_ok());
                // clone: the class's values, kept in the key
                all.then(|| self.canonical(self.compared.clone()))
            }
        }
    }

    /// Not any of `outside`, said as the one value left where the type has only one more.
    fn canonical(&self, outside: Box<[u128]>) -> ArgFact {
        let left = |all: &[u128]| {
            let mut rest = all.iter().filter(|it| outside.binary_search(it).is_err());
            match (rest.next(), rest.next()) {
                (Some(&one), None) => Some(one),
                _ => None,
            }
        };
        let one = match &self.kind {
            Kind::Bool => left(&[0, 1]),
            Kind::Enum(all) => left(all),
            Kind::Int => None,
        };
        one.map_or(ArgFact::IsNot(outside), ArgFact::Is)
    }

    /// Whether `fact` is a class of this parameter as [`Param::class_of`] says it.
    fn admits(&self, fact: &ArgFact) -> bool {
        match fact {
            ArgFact::Is(value) => match &self.kind {
                Kind::Bool => *value <= 1,
                Kind::Int => self.compared.binary_search(value).is_ok(),
                Kind::Enum(all) => {
                    all.binary_search(value).is_ok() && self.class_of(*value) == *fact
                }
            },
            ArgFact::IsNot(outside) => {
                **outside == *self.compared
                    // clone: two words and a short list, on a query's check
                    && matches!(self.canonical(outside.clone()), ArgFact::IsNot(_))
            }
        }
    }
}

fn truncate(value: u128, bits: u32) -> u128 {
    if bits >= 128 { value } else { value & ((1u128 << bits) - 1) }
}

/// Why `base` has no copies whatever its body, if it has none.
fn ineligible(db: &Db, base: FunctionId) -> Option<&'static str> {
    let flags = AttrFlags::query(db, base.into());
    let named = AttrFlags::NO_MANGLE
        | AttrFlags::HAS_EXPORT_NAME
        | AttrFlags::HAS_LINKAGE
        | AttrFlags::LINKED_BY_NAME;
    if flags.intersects(named) {
        return Some("it is named by a symbol, whose calls another object may take over");
    }
    // A method of a trait impl is `#[track_caller]` also when the trait's declaration is.
    if track_caller(db, base) {
        return Some("it is `#[track_caller]`");
    }
    let refused = [
        (AttrFlags::COLD, "it is `#[cold]`"),
        (AttrFlags::NAKED, "it is `#[naked]`"),
        (AttrFlags::OPTIMIZE_NONE, "it is `#[optimize(none)]`"),
        (AttrFlags::RUSTC_INTRINSIC, "it is an intrinsic"),
        // std's UB-check `precondition_check`s: rustc's MIR inliner leaves them alone, and
        // their calls are in a body only where it has UB checks, so they would make the copies
        // a unit's body calls differ from those of the body the code generator walks.
        (AttrFlags::RUSTC_NO_MIR_INLINE, "it is `#[rustc_no_mir_inline]`"),
    ];
    if let Some(&(_, why)) = refused.iter().find(|(flag, _)| flags.contains(*flag)) {
        return Some(why);
    }
    if crate::mir::answered_by_codegen(db, base) {
        return Some("the code generator answers its calls itself");
    }
    let signature = FunctionSignature::of(db, base);
    if signature.abi != ExternAbi::Rust {
        return Some("its ABI is not Rust's");
    }
    if signature.is_varargs() {
        return Some("it takes variadic arguments");
    }
    if !signature.has_body() {
        return Some("it has no body");
    }
    None
}

/// What a copy of `base` may know: its [`Plan`], or why it has no copies.
#[blaze_query::tracked(returns(ref))]
pub(super) fn plan(db: &Db, base: FunctionId) -> std::result::Result<Plan, &'static str> {
    if let Some(why) = ineligible(db, base) {
        return Err(why);
    }
    let body = db.runtime_mir_body(base.into()).map_err(|_| "its MIR does not lower")?;
    if body.coroutine.is_some() {
        return Err("it is a coroutine's body");
    }
    // Its overflow checks are its callers' (`#[rustc_inherit_overflow_checks]`), and a copy
    // is shared by callers of every setting.
    if body.inherits_overflow_checks {
        return Err("its overflow checks are its callers'");
    }
    // A copy would have the assembly twice, and assembly that defines a symbol then fails to
    // assemble or link where one function would not. Stricter than the inliner, which refuses
    // assembly only into a caller of another `#[instruction_set]`.
    if inline::has_asm(body) {
        return Err("it has inline assembly");
    }
    let size = size_of(body);
    if size > MAX_CALLEE_SIZE {
        return Err("its body is larger than `MAX_CALLEE_SIZE`");
    }
    frame::task(Tag::Mir, |frame| {
        let ctx = Ctx::new(db, base.into(), frame);
        let analysis = Analysis::of(&ctx, body, frame);
        // The parameters in order, while the copies they allow stay within `MAX_COPIES`: each
        // known in one of its classes, or not known.
        let mut copies = 1u32;
        // alloc: the plan, kept by its query
        let mut params = Vec::new();
        for param in analysis.params {
            let with = copies.saturating_mul(param.classes() + 1);
            if with - 1 > MAX_COPIES {
                continue;
            }
            copies = with;
            params.push(param);
        }
        if params.is_empty() {
            return Err("it branches on no parameter a copy could know");
        }
        Ok(Plan { size, params: params.into_boxed_slice() })
    })
}

fn size_of(body: &MirBody<'_>) -> u32 {
    let blocks = body.basic_blocks.iter().map(|(_, it)| it.statements.len() + 1);
    u32::try_from(blocks.sum::<usize>()).unwrap_or(u32::MAX)
}

/// The work [`fold`] removes from the base of `clone`, weighed as [`MIN_SAVES`] says: 0 where
/// it decides no branch.
#[blaze_query::tracked(returns(copy))]
fn saves<'db>(db: &'db Db, clone: CloneId<'db>) -> u32 {
    let spec = clone.spec(db);
    let Ok(body) = db.runtime_mir_body(spec.base.into()) else { return 0 };
    frame::task(Tag::Mir, |frame| {
        let ctx = Ctx::new(db, spec.base.into(), frame);
        let analysis = Analysis::of(&ctx, body, frame);
        let decided = analysis.decided(body, &spec.facts, frame);
        if decided.is_empty() {
            return 0;
        }
        let looped = in_cycles(body, frame);
        let weight = |block: BasicBlockId| match looped.contains(block_index(block)) {
            true => LOOP_WEIGHT,
            false => 1,
        };
        let before = reachable(body, &[], frame);
        let after = reachable(body, &decided, frame);
        let mut saves = 0u32;
        for &(block, _) in decided.iter() {
            if after.contains(block_index(block)) {
                saves = saves.saturating_add(weight(block) * SWITCH_COST);
            }
        }
        for (block, data) in body.basic_blocks.iter() {
            let at = block_index(block);
            if before.contains(at) && !after.contains(at) {
                let work = u32::try_from(data.statements.len() + 1).unwrap_or(u32::MAX);
                saves = saves.saturating_add(weight(block).saturating_mul(work));
            }
        }
        saves
    })
}

/// How a body uses each local, for what a value can be known from.
struct Uses<'f> {
    /// By local: assignments of it as a whole.
    assigned: &'f mut [u32],
    /// By local: where it is assigned, the one place where there is one.
    at: &'f mut [Option<Location>],
    /// Locals written in part, borrowed, returned to by a call or changed in place: what is
    /// read of them is not known from an assignment.
    touched: Bits<'f>,
}

impl<'f> Uses<'f> {
    fn of(body: &MirBody<'_>, frame: &'f Frame) -> Uses<'f> {
        let count = body.locals.len();
        let assigned = frame.alloc_slice_fill_with(count, |_| 0u32);
        let at = frame.alloc_slice_fill_with(count, |_| None);
        let mut touched: Bits<'f> = bits_in(count, frame);
        visit_body(body, |local, mention, location| {
            let index = local_index(local);
            match mention {
                Mention::Assigned => {
                    assigned[index] += 1;
                    at[index] = Some(location);
                }
                Mention::Read | Mention::Storage => (),
                // A drop of the whole value ends it: what was read before was what it held.
                Mention::Mutated if is_drop_of(body, local, location) => (),
                Mention::AssignedPart
                | Mention::CallReturned
                | Mention::SharedBorrow
                | Mention::Mutated => {
                    touched.insert(index);
                }
            }
        });
        Uses { assigned, at, touched }
    }

    /// Whether `local` holds what its one assignment gives it wherever it is read: it is
    /// written nowhere else, in part or through a borrow.
    fn settled(&self, local: LocalId) -> bool {
        let index = local_index(local);
        self.assigned[index] == 1 && !self.touched.contains(index)
    }

    /// Whether nothing in the body writes or borrows `local`, a parameter.
    fn untouched(&self, local: LocalId) -> bool {
        let index = local_index(local);
        self.assigned[index] == 0 && !self.touched.contains(index)
    }

    /// The one assignment of `local`, where it is [`Uses::settled`].
    fn assignment<'b>(&self, body: &'b MirBody<'_>, local: LocalId) -> Option<&'b Rvalue> {
        if !self.settled(local) {
            return None;
        }
        let at = self.at[local_index(local)]?;
        match &body.basic_blocks[at.block].statements.get(at.index)?.kind {
            StatementKind::Assign(_, rvalue) => Some(rvalue),
            _ => None,
        }
    }
}

fn is_drop_of(body: &MirBody<'_>, local: LocalId, at: Location) -> bool {
    let block = &body.basic_blocks[at.block];
    at.index == block.statements.len()
        && matches!(
            &block.terminator,
            Some(Terminator { kind: TerminatorKind::Drop { place, .. }, .. })
                if place.local == local && place.projection.is_empty()
        )
}

/// What a local holds of a parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    /// The parameter's value (by its index), or of an enum its discriminant.
    Value(u32),
    /// Whether the parameter's value is `value` (or with `ne`, is not).
    Cmp { param: u32, value: u128, ne: bool },
}

/// What the branches of a body read of its parameters.
struct Analysis<'f> {
    /// By local.
    views: &'f mut [Option<View>],
    /// By block: what the `SwitchInt` ending it reads of a parameter.
    switches: &'f mut [Option<View>],
    /// The parameters it branches on that a copy may know, by index, ascending.
    params: Vec<Param, &'f Frame>,
}

/// A parameter while its uses are read.
struct Candidate<'f> {
    index: u32,
    local: LocalId,
    kind: Kind,
    /// 0 for an enum until its discriminant is read.
    bits: u32,
    /// An enum's discriminant is read into locals of more than one size.
    mixed: bool,
    compared: Vec<u128, &'f Frame>,
    branches: u32,
}

impl<'f> Analysis<'f> {
    fn of<'db>(ctx: &Ctx<'_, 'db>, body: &MirBody<'db>, frame: &'f Frame) -> Analysis<'f> {
        let uses = Uses::of(body, frame);
        let views: &'f mut [Option<View>] =
            frame.alloc_slice_fill_with(body.locals.len(), |_| None);
        let mut candidates: Vec<Candidate<'f>, &Frame> = Vec::new_in(frame);
        for (index, &local) in body.param_locals.iter().enumerate() {
            if !uses.untouched(local) {
                continue;
            }
            let ty = ctx.normalize(body.locals[local].ty.as_ref());
            let Some((kind, bits)) = kind_of(ctx, ty) else { continue };
            let index = index as u32;
            if !matches!(kind, Kind::Enum(_)) {
                views[local_index(local)] = Some(View::Value(index));
            }
            let compared = Vec::new_in(frame);
            candidates.push(Candidate {
                index,
                local,
                kind,
                bits,
                mixed: false,
                compared,
                branches: 0,
            });
        }
        // Follow the values into temporaries each written once, in a bounded number of rounds:
        // a longer chain is not followed, and its branches stay.
        for _ in 0..VIEW_ROUNDS {
            let mut changed = false;
            for (_, block) in body.basic_blocks.iter() {
                for statement in &block.statements {
                    let StatementKind::Assign(place, rvalue) = &statement.kind else { continue };
                    let target = place.local;
                    if !place.projection.is_empty()
                        || views[local_index(target)].is_some()
                        || !uses.settled(target)
                    {
                        continue;
                    }
                    let Some(view) =
                        view_of_rvalue(ctx, body, rvalue, target, views, &mut candidates)
                    else {
                        continue;
                    };
                    views[local_index(target)] = Some(view);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        // The branches, and what each compares with.
        let switches: &'f mut [Option<View>] =
            frame.alloc_slice_fill_with(body.basic_blocks.len(), |_| None);
        for (at, block) in body.basic_blocks.iter() {
            let Some(Terminator { kind: TerminatorKind::SwitchInt { discr, targets }, .. }) =
                &block.terminator
            else {
                continue;
            };
            let view = switch_view(views, discr).or_else(|| {
                last_assigned(ctx, body, &uses, &block.statements, discr, views, &mut candidates)
            });
            let Some(view) = view else { continue };
            switches[block_index(at)] = Some(view);
            let param = match view {
                View::Value(param) | View::Cmp { param, .. } => param,
            };
            let Some(candidate) = candidates.iter_mut().find(|it| it.index == param) else {
                continue;
            };
            match view {
                View::Value(_) => {
                    let bits = candidate.bits;
                    candidate.compared.extend(targets.values.iter().map(|&it| truncate(it, bits)));
                }
                View::Cmp { value, .. } => candidate.compared.push(value),
            }
            candidate.branches += 1;
        }
        let mut params = Vec::new_in(frame);
        for mut candidate in candidates {
            if candidate.branches == 0 || candidate.mixed || candidate.bits == 0 {
                continue;
            }
            candidate.compared.sort_unstable();
            candidate.compared.dedup();
            if candidate.compared.len() > MAX_COMPARED {
                continue;
            }
            let kind = match candidate.kind {
                Kind::Enum(_) => match discriminants(ctx, body, candidate.local, candidate.bits) {
                    Some(all) => Kind::Enum(all),
                    None => continue,
                },
                other => other,
            };
            params.push(Param {
                index: candidate.index,
                kind,
                bits: candidate.bits,
                // alloc: the plan's, kept by its query
                compared: candidate.compared.iter().copied().collect(),
            });
        }
        Analysis { views, switches, params }
    }

    /// The branches `facts` decide, each with where it goes, by block.
    fn decided(
        &self,
        body: &MirBody<'_>,
        facts: &[(u32, ArgFact)],
        frame: &'f Frame,
    ) -> Vec<(BasicBlockId, BasicBlockId), &'f Frame> {
        let mut decided = Vec::new_in(frame);
        for (block, data) in body.basic_blocks.iter() {
            let Some(Terminator { kind: TerminatorKind::SwitchInt { targets, .. }, .. }) =
                &data.terminator
            else {
                continue;
            };
            let Some(view) = self.switches[block_index(block)] else { continue };
            if let Some(target) = self.target(view, targets, facts) {
                decided.push((block, target));
            }
        }
        decided
    }

    /// Where a switch that reads `view` goes, where `facts` decide it.
    fn target(
        &self,
        view: View,
        targets: &SwitchTargets,
        facts: &[(u32, ArgFact)],
    ) -> Option<BasicBlockId> {
        let fact_of = |param: u32| facts.iter().find(|it| it.0 == param).map(|it| &it.1);
        match view {
            View::Value(param) => {
                let bits = self.params.iter().find(|it| it.index == param)?.bits;
                match fact_of(param)? {
                    ArgFact::Is(value) => Some(
                        targets
                            .iter()
                            .find(|&(it, _)| truncate(it, bits) == *value)
                            .map_or(targets.otherwise(), |(_, target)| target),
                    ),
                    ArgFact::IsNot(outside) => targets
                        .values
                        .iter()
                        .all(|&it| outside.binary_search(&truncate(it, bits)).is_ok())
                        .then(|| targets.otherwise()),
                }
            }
            View::Cmp { param, value, ne } => {
                let holds = compare(fact_of(param)?, value, ne)?;
                Some(targets.target_for_value(u128::from(holds)))
            }
        }
    }
}

/// What the local `target`, assigned `rvalue` once, holds of a parameter.
fn view_of_rvalue<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    rvalue: &Rvalue,
    target: LocalId,
    views: &[Option<View>],
    candidates: &mut [Candidate<'_>],
) -> Option<View> {
    let view_of = |operand: &Operand| match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) if place.projection.is_empty() => {
            views[local_index(place.local)]
        }
        _ => None,
    };
    match rvalue {
        Rvalue::Discriminant(place) if place.projection.is_empty() => {
            let candidate = candidates.iter_mut().find(|it| it.local == place.local)?;
            if !matches!(candidate.kind, Kind::Enum(_)) {
                return None;
            }
            let ty = ctx.normalize(body.locals[target].ty.as_ref());
            let bits = (ctx.size_of(ty)? * 8) as u32;
            if candidate.bits == 0 {
                candidate.bits = bits;
            } else if candidate.bits != bits {
                candidate.mixed = true;
            }
            Some(View::Value(candidate.index))
        }
        Rvalue::Use(operand) => view_of(operand),
        Rvalue::BinaryOp(op @ (BinOp::Eq | BinOp::Ne), left, right) => {
            let (param, constant) = match (view_of(left), view_of(right)) {
                (Some(View::Value(param)), None) => (param, right),
                (None, Some(View::Value(param))) => (param, left),
                _ => return None,
            };
            let bits = candidates.iter().find(|it| it.index == param && it.bits != 0)?.bits;
            let value = truncate(constant_bits(constant)?, bits);
            Some(View::Cmp { param, value, ne: *op == BinOp::Ne })
        }
        Rvalue::UnaryOp(UnOp::Not, operand) => match view_of(operand)? {
            View::Cmp { param, value, ne } => Some(View::Cmp { param, value, ne: !ne }),
            View::Value(_) => None,
        },
        _ => None,
    }
}

/// Whether "the parameter is `value`" (or with `ne`, is not) holds, where `fact` decides it.
fn compare(fact: &ArgFact, value: u128, ne: bool) -> Option<bool> {
    let equal = match fact {
        ArgFact::Is(it) => *it == value,
        ArgFact::IsNot(outside) if outside.binary_search(&value).is_ok() => false,
        ArgFact::IsNot(_) => return None,
    };
    Some(equal != ne)
}

/// What a switch on a local that is assigned more than once reads of a parameter: the value of
/// its last assignment in the switch's own block, where nothing writes it in part or borrows
/// it, so that assignment is what the switch reads. Lowering reads every enum's discriminant
/// into one temporary of the body, just before the switch on it.
fn last_assigned<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    uses: &Uses<'_>,
    statements: &[Statement],
    discr: &Operand,
    views: &[Option<View>],
    candidates: &mut [Candidate<'_>],
) -> Option<View> {
    let (OperandKind::Copy(place) | OperandKind::Move(place)) = &discr.kind else { return None };
    if !place.projection.is_empty() || uses.touched.contains(local_index(place.local)) {
        return None;
    }
    let (target, rvalue) = statements.iter().rev().find_map(|statement| match &statement.kind {
        StatementKind::Assign(target, rvalue) if target.local == place.local => {
            Some((target, rvalue))
        }
        _ => None,
    })?;
    // A write of a part of it leaves the rest as it was.
    if !target.projection.is_empty() {
        return None;
    }
    view_of_rvalue(ctx, body, rvalue, place.local, views, candidates)
}

fn switch_view(views: &[Option<View>], discr: &Operand) -> Option<View> {
    match &discr.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) if place.projection.is_empty() => {
            views[local_index(place.local)]
        }
        _ => None,
    }
}

/// What a copy may know of a parameter of type `ty`, with the size in bits of what is
/// compared: of an enum, that of the local its discriminant is read into, found later (0).
fn kind_of<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>) -> Option<(Kind, u32)> {
    let scalar = |kind| Some((kind, (ctx.size_of(ty)? * 8) as u32));
    match ty.kind() {
        TyKind::Bool => scalar(Kind::Bool),
        TyKind::Char | TyKind::Int(_) | TyKind::Uint(_) => scalar(Kind::Int),
        TyKind::Adt(adt, _) => match adt.def_id() {
            AdtId::EnumId(it) if it.enum_variants(ctx.db).variants.len() >= 2 => {
                // alloc: an empty slice, which allocates nothing
                Some((Kind::Enum(Box::default()), 0))
            }
            _ => None,
        },
        _ => None,
    }
}

/// The discriminants of the enum `local` is of, truncated to `bits`, sorted, each once.
fn discriminants<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    local: LocalId,
    bits: u32,
) -> Option<Box<[u128]>> {
    let ty = ctx.normalize(body.locals[local].ty.as_ref());
    let TyKind::Adt(adt, _) = ty.kind() else { return None };
    let AdtId::EnumId(it) = adt.def_id() else { return None };
    // alloc: past 8 variants, while the plan is made
    let mut all: SmallVec<[u128; 8]> = SmallVec::new();
    for &(variant, _) in it.enum_variants(ctx.db).variants.values() {
        let value = ctx.db.const_eval_discriminant(variant).ok()?;
        all.push(truncate(value as u128, bits));
    }
    all.sort_unstable();
    all.dedup();
    // alloc: the plan's, kept by its query
    Some(all.into_iter().collect())
}

/// `body` with what `facts` say folded in: each branch they decide a goto, each comparison they
/// decide its `bool`, an enum parameter's discriminant its value, and a scalar parameter whose
/// value they give that value wherever it is read as an operand. The parameters stay.
fn fold<'db>(ctx: &Ctx<'_, 'db>, body: &mut MirBody<'db>, facts: &[(u32, ArgFact)], frame: &Frame) {
    let analysis = Analysis::of(ctx, body, frame);
    let decided = analysis.decided(body, facts, frame);
    let fact_of = |param: u32| facts.iter().find(|it| it.0 == param).map(|it| &it.1);
    let enum_param = |local: LocalId| {
        let is = |it: &&Param| body.param_locals[it.index as usize] == local;
        let param =
            analysis.params.iter().filter(|it| matches!(it.kind, Kind::Enum(_))).find(is)?;
        Some((param.index, param.bits))
    };
    // The assignments that become constants, with the value of each.
    let mut constants: Vec<(BasicBlockId, usize, u128), &Frame> = Vec::new_in(frame);
    for (block, data) in body.basic_blocks.iter() {
        for (at, statement) in data.statements.iter().enumerate() {
            let StatementKind::Assign(place, rvalue) = &statement.kind else { continue };
            if !place.projection.is_empty() {
                continue;
            }
            let value = match (analysis.views[local_index(place.local)], rvalue) {
                // Into whichever local it is read, as the parameter is never written, where that
                // local is of the size the plan read it at: the value is truncated to it.
                (_, Rvalue::Discriminant(read)) if read.projection.is_empty() => {
                    let Some((param, bits)) = enum_param(read.local) else { continue };
                    let ty = ctx.normalize(body.locals[place.local].ty.as_ref());
                    if ctx.size_of(ty).map(|it| (it * 8) as u32) != Some(bits) {
                        continue;
                    }
                    match fact_of(param) {
                        Some(ArgFact::Is(value)) => *value,
                        _ => continue,
                    }
                }
                (Some(View::Cmp { param, value, ne }), _) => {
                    match fact_of(param).and_then(|it| compare(it, value, ne)) {
                        Some(holds) => u128::from(holds),
                        None => continue,
                    }
                }
                _ => continue,
            };
            constants.push((block, at, value));
        }
    }
    for &(block, at, value) in constants.iter() {
        let statement = &mut body.basic_blocks[block].statements[at];
        let StatementKind::Assign(place, rvalue) = &mut statement.kind else { continue };
        let ty = body.locals[place.local].ty.as_ref();
        if let Some(constant) = scalar_constant(ctx, ty, value) {
            *rvalue = Rvalue::Use(constant);
        }
    }
    for &(block, target) in decided.iter() {
        if let Some(terminator) = &mut body.basic_blocks[block].terminator {
            terminator.kind = TerminatorKind::Goto { target };
        }
    }
    for param in analysis.params.iter() {
        if matches!(param.kind, Kind::Enum(_)) {
            continue;
        }
        let Some(ArgFact::Is(value)) = fact_of(param.index) else { continue };
        let local = body.param_locals[param.index as usize];
        let ty = body.locals[local].ty.as_ref();
        let Some(constant) = scalar_constant(ctx, ty, *value) else { continue };
        let mut replace = |operand: &mut Operand| {
            if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind
                && place.local == local
                && place.projection.is_empty()
            {
                // clone: a constant of at most 16 bytes
                *operand = constant.clone();
            }
        };
        for (_, block) in body.basic_blocks.iter_mut() {
            for statement in &mut block.statements {
                statement.kind.for_each_operand_mut(&mut replace);
            }
            if let Some(terminator) = &mut block.terminator {
                terminator.kind.for_each_operand_mut(&mut replace);
            }
        }
    }
}

/// `value` as a constant of `ty`, a `bool`, `char` or integer, where its size is known.
fn scalar_constant<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>, value: u128) -> Option<Operand> {
    let size = ctx.size_of(ctx.normalize(ty))? as usize;
    let bytes = value.to_le_bytes();
    // alloc: the constant's bytes, kept in the body
    Some(Operand::from_bytes(Box::from(bytes.get(..size)?), ty))
}

/// The blocks reachable from the start, each block of `decided` going only where it says.
fn reachable<'f>(
    body: &MirBody<'_>,
    decided: &[(BasicBlockId, BasicBlockId)],
    frame: &'f Frame,
) -> Bits<'f> {
    let mut seen: Bits<'f> = bits_in(body.basic_blocks.len(), frame);
    let mut stack: Vec<BasicBlockId, &Frame> = Vec::new_in(frame);
    // Where each decided block goes, by block: the first decision of it, as `decided` lists.
    let mut goto: Vec<Option<BasicBlockId>, &Frame> = Vec::new_in(frame);
    goto.resize(body.basic_blocks.len(), None);
    for &(block, target) in decided {
        goto[block_index(block)].get_or_insert(target);
    }
    seen.insert(block_index(body.start_block));
    stack.push(body.start_block);
    while let Some(block) = stack.pop() {
        let mut visit = |target: BasicBlockId| {
            if seen.insert(block_index(target)) {
                stack.push(target);
            }
        };
        if let Some(target) = goto[block_index(block)] {
            visit(target);
            continue;
        }
        if let Some(terminator) = &body.basic_blocks[block].terminator {
            for target in terminator.kind.all_successors() {
                visit(target);
            }
        }
    }
    seen
}

/// The blocks on a cycle of the control flow: of a strongly connected component (Tarjan's,
/// without recursion) of more than one block, or with an edge to itself.
fn in_cycles<'f>(body: &MirBody<'_>, frame: &'f Frame) -> Bits<'f> {
    const UNSEEN: u32 = u32::MAX;
    let count = body.basic_blocks.len();
    let index = frame.alloc_slice_fill_with(count, |_| UNSEEN);
    let low = frame.alloc_slice_fill_with(count, |_| 0u32);
    let mut on_stack: Bits<'_> = bits_in(count, frame);
    let mut looped: Bits<'f> = bits_in(count, frame);
    let mut stack: Vec<usize, &Frame> = Vec::new_in(frame);
    // The blocks being visited, each with how many of its successors it has gone to.
    let mut visiting: Vec<(usize, usize), &Frame> = Vec::new_in(frame);
    let successor = |block: usize, nth: usize| {
        let terminator = body.basic_blocks[block_id(block)].terminator.as_ref()?;
        terminator.kind.all_successors().nth(nth).map(block_index)
    };
    let mut next = 0u32;
    let start = block_index(body.start_block);
    index[start] = next;
    low[start] = next;
    next += 1;
    stack.push(start);
    on_stack.insert(start);
    visiting.push((start, 0));
    while let Some(&(block, nth)) = visiting.last() {
        if let Some(target) = successor(block, nth) {
            if let Some(last) = visiting.last_mut() {
                last.1 += 1;
            }
            if target == block {
                looped.insert(block);
            }
            if index[target] == UNSEEN {
                index[target] = next;
                low[target] = next;
                next += 1;
                stack.push(target);
                on_stack.insert(target);
                visiting.push((target, 0));
            } else if on_stack.contains(target) {
                low[block] = low[block].min(index[target]);
            }
            continue;
        }
        visiting.pop();
        if let Some(&(parent, _)) = visiting.last() {
            low[parent] = low[parent].min(low[block]);
        }
        if low[block] == index[block] {
            let Some(from) = stack.iter().rposition(|&it| it == block) else { break };
            if stack.len() - from > 1 {
                for &member in &stack[from..] {
                    looped.insert(member);
                }
            }
            for &member in &stack[from..] {
                on_stack.remove(member);
            }
            stack.truncate(from);
        }
    }
    looped
}

/// Writes what `spec` knows after a path ([`Specialization::knowing`]).
fn write_facts(spec: &Specialization, out: &mut String) {
    use std::fmt::Write;
    _ = write!(out, "{}", spec.knowing());
}

/// The span of the pass over `body`, opened at its first call of a callee that has copies
/// (so a body with none pays for no name): a copy named by its base and what it knows, any
/// other body by its owner's path (a closure's is the body it is written in).
fn pass_span<'db>(
    db: &'db Db,
    body: &MirBody<'db>,
    within: Option<&Specialization>,
) -> tracing::Span {
    let span = tracing::info_span!(
        target: "blaze",
        "mir specialize",
        krate = tracing::field::Empty,
        function = tracing::field::Empty
    );
    match (within, body.owner) {
        (Some(spec), _) => {
            span_names::of_body_then(db, spec.base.into(), span, |text| write_facts(spec, text))
        }
        (None, InferBodyId::DefWithBodyId(def)) => span_names::of_body(db, def, span),
        (None, InferBodyId::AnonConstId(_)) => span,
    }
}

/// The `Specialize` pass over `body`: each call that gets a copy of its callee listed in
/// `body.inlined.specialized`. `within` is the copy `body` is the body of, if it is one: what
/// it knows of its parameters passes on to the calls they are given to.
pub(super) fn specialize<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    within: Option<&Specialization>,
    frame: &Frame,
) {
    if body.coroutine.is_some() {
        return;
    }
    let db = ctx.db;
    let env = ParamEnvAndCrate { param_env: ctx.env, krate: ctx.krate() };
    let uses = Uses::of(body, frame);
    let within = within.and_then(|it| Some((it, plan(db, it.base).as_ref().ok()?)));
    let mut span = None;
    // Every call that may get a copy, in block order; then the copies the body calls, chosen
    // by the work they remove, so that which a body calls does not depend on the order of its
    // blocks, which the UB checks a unit has and the walked body lacks may change.
    let mut found: Vec<Found<'db>, &Frame> = Vec::new_in(frame);
    for (block, data) in body.basic_blocks.iter() {
        if data.is_cleanup {
            continue;
        }
        let Some(Terminator { kind: TerminatorKind::Call { func, args, .. }, .. }) =
            &data.terminator
        else {
            continue;
        };
        let Some((func, fn_args)) = inline::fn_def(body, func) else { continue };
        let Some(fn_args) = inline::fold(ctx, fn_args, None) else { continue };
        let Some((InlinedBody::Fn(base), base_args)) = inline::resolve(ctx, env, func, fn_args)
        else {
            continue;
        };
        if span.is_none() {
            span = Some(pass_span(db, body, within.map(|(it, _)| it)).entered());
        }
        let Ok(callee) = plan(db, base) else { continue };
        // The body agrees with the ABI of the trait method the call names.
        if FunctionSignature::of(db, func).abi != ExternAbi::Rust {
            refuse(block, base, "the call is not of the Rust ABI");
            continue;
        }
        // alloc: past 2 facts, once per call that may get a copy
        let mut facts: SmallVec<[(u32, ArgFact); 2]> = SmallVec::new();
        for param in callee.params.iter() {
            let Some(arg) = args.get(param.index as usize) else { break };
            if let Some(fact) = arg_fact(ctx, body, &uses, arg, param, within) {
                facts.push((param.index, fact));
            }
        }
        if facts.is_empty() {
            continue;
        }
        // alloc: the copy's key, kept in the body and by the interner
        let spec = Specialization { base, facts: facts.into_iter().collect() };
        let own = within.is_some_and(|(it, _)| *it == spec);
        let interner = ctx.infcx.interner;
        let args =
            rustc_type_ir::fold_regions(interner, base_args, |_, _| Region::new_erased(interner));
        let id = CloneId::new(db, spec);
        found.push(Found { block, id, own, size: callee.size, args: args.store(), copy: None });
    }
    // Each copy once, weighed at its first call: the calls grouped by the copy's interned id,
    // which only groups them, then by block. Nothing below reads the order of the groups.
    let mut by_copy: Vec<(blaze_query::db::Id, usize), &Frame> = Vec::new_in(frame);
    let others = found.iter().enumerate().filter(|(_, it)| !it.own);
    by_copy.extend(others.map(|(at, it)| (it.id.as_id(), at)));
    by_copy.sort_unstable();
    let mut weighed: Vec<Weighed, &Frame> = Vec::new_in(frame);
    for calls in by_copy.chunk_by(|a, b| a.0 == b.0) {
        let first = &found[calls[0].1];
        let saves = saves(db, first.id);
        if saves < MIN_SAVES {
            continue;
        }
        let copy = weighed.len();
        let (base, block, size) = (first.id.spec(db).base, first.block, first.size);
        weighed.push(Weighed { base, saves, size, block, taken: false });
        calls.iter().for_each(|&(_, at)| found[at].copy = Some(copy));
    }
    // Most work removed first, then the smaller copy. Copies that weigh the same are taken
    // together or not at all: which of them a body would take first is block order. The first
    // call's block, which no two copies share, orders only the refusals of a tie.
    let mut ranked: Vec<(std::cmp::Reverse<u32>, u32, BasicBlockId, usize), &Frame> =
        Vec::new_in(frame);
    let weights = weighed.iter().zip(0..);
    ranked.extend(weights.map(|(it, at)| (std::cmp::Reverse(it.saves), it.size, it.block, at)));
    ranked.sort_unstable_by_key(|&(saves, size, block, _)| (saves, size, block_index(block)));
    let (mut taken, mut growth) = (0usize, 0u32);
    for tied in ranked.chunk_by(|a, b| (a.0, a.1) == (b.0, b.1)) {
        let size = tied.iter().fold(0u32, |sum, it| sum.saturating_add(it.1));
        let why = if taken + tied.len() > MAX_COPIES_PER_BODY {
            "the body would call more than `MAX_COPIES_PER_BODY` copies"
        } else if growth.saturating_add(size) > GROWTH_PER_BODY {
            "the copies the body calls would pass `GROWTH_PER_BODY`"
        } else {
            (taken, growth) = (taken + tied.len(), growth + size);
            tied.iter().for_each(|it| weighed[it.3].taken = true);
            continue;
        };
        for &(.., at) in tied {
            let copy = &weighed[at];
            refuse(copy.block, copy.base, why);
        }
    }
    for call in found.iter().filter(|it| !it.own && it.copy.is_none()) {
        refuse(call.block, call.id.spec(db).base, "it would remove less work than `MIN_SAVES`");
    }
    let listed =
        found.into_iter().filter(|call| call.own || call.copy.is_some_and(|it| weighed[it].taken));
    // alloc: the body's list, kept with it; clone: the interner keeps its own key
    let calls = listed.map(|it| SpecializedCall {
        block: it.block,
        clone: it.id.spec(db).clone(),
        args: it.args,
    });
    body.inlined.specialized = calls.collect();
    drop(span);
}

/// A call [`specialize`] may list.
struct Found<'db> {
    block: BasicBlockId,
    id: CloneId<'db>,
    /// A call of the copy the body is of, knowing what it knows: the copy itself, in no budget.
    own: bool,
    /// The callee's size, what its copy adds to `GROWTH_PER_BODY`.
    size: u32,
    args: crate::next_solver::StoredGenericArgs,
    /// The copy in `weighed` it calls, if it removes enough work to be weighed.
    copy: Option<usize>,
}

/// A copy some call of the body may call, with what it weighs in the body's budgets.
struct Weighed {
    base: FunctionId,
    saves: u32,
    size: u32,
    /// The first call of it.
    block: BasicBlockId,
    taken: bool,
}

/// Says why the call ending `block` gets no copy of `base`, under the pass's span.
fn refuse(block: BasicBlockId, base: FunctionId, why: &str) {
    let block = block_index(block);
    tracing::debug!(target: "blaze", block, ?base, why, "no specialized copy");
}

/// What a copy of the callee would know of `param` from `arg`: a constant's class, or the
/// class a copy (`within`) knows its own parameter `arg` reads to be in.
fn arg_fact<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    uses: &Uses<'_>,
    arg: &Operand,
    param: &Param,
    within: Option<(&Specialization, &Plan)>,
) -> Option<ArgFact> {
    if let Some(value) = value_of(ctx, body, uses, arg, param) {
        return Some(param.class_of(value));
    }
    let (OperandKind::Copy(place) | OperandKind::Move(place)) = &arg.kind else { return None };
    if !place.projection.is_empty() || !uses.untouched(place.local) {
        return None;
    }
    let (spec, plan) = within?;
    let index = body.param_locals.iter().position(|it| *it == place.local)? as u32;
    let (_, known) = spec.facts.iter().find(|it| it.0 == index)?;
    let own = plan.params.iter().find(|it| it.index == index)?;
    let same = std::mem::discriminant(&own.kind) == std::mem::discriminant(&param.kind);
    if !same || own.bits != param.bits {
        return None;
    }
    if let (Kind::Enum(own), Kind::Enum(theirs)) = (&own.kind, &param.kind)
        && own != theirs
    {
        return None;
    }
    param.class_given(known)
}

/// The value `arg` gives `param`, truncated: a constant's, a variant's discriminant, or that
/// of the one assignment of a temporary nothing else writes or borrows.
fn value_of<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    uses: &Uses<'_>,
    arg: &Operand,
    param: &Param,
) -> Option<u128> {
    let constant = match &arg.kind {
        OperandKind::Constant { .. } | OperandKind::Allocation { .. } => arg,
        OperandKind::Copy(place) | OperandKind::Move(place) if place.projection.is_empty() => {
            if body.param_locals.contains(&place.local) {
                return None;
            }
            match uses.assignment(body, place.local)? {
                Rvalue::Use(it) => it,
                Rvalue::Aggregate(AggregateKind::Adt(VariantId::EnumVariantId(variant), _), _)
                    if matches!(param.kind, Kind::Enum(_)) =>
                {
                    let value = ctx.db.const_eval_discriminant(*variant).ok()?;
                    return Some(truncate(value as u128, param.bits));
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    let value = match param.kind {
        Kind::Bool | Kind::Int => constant_bits(constant).or_else(|| {
            let allocation = evaluated(ctx, constant)?;
            bytes_value(&allocation.memory, &allocation.memory_map)
        })?,
        Kind::Enum(_) => enum_constant(ctx, constant)?,
    };
    Some(truncate(value, param.bits))
}

/// The value of a free `const` item the operand names, evaluated, where its arguments name no
/// parameter of the body.
fn evaluated<'db>(ctx: &Ctx<'_, 'db>, operand: &Operand) -> Option<Allocation<'db>> {
    let OperandKind::Constant { konst, .. } = &operand.kind else { return None };
    let ConstKind::Alias(_, alias) = konst.as_ref().kind() else { return None };
    let AliasConstKind::Free { def_id: FreeConstAliasId(id) } = alias.kind else { return None };
    if alias.args.has_non_region_param() {
        return None;
    }
    let env = ParamEnvAndCrate { param_env: ctx.env, krate: ctx.krate() };
    ctx.db.const_eval(id, alias.args, Some(env)).ok()
}

fn bytes_value(bytes: &[u8], memory_map: &MemoryMap<'_>) -> Option<u128> {
    if bytes.len() > 16 || !matches!(memory_map, MemoryMap::Empty) {
        return None;
    }
    let mut padded = [0u8; 16];
    padded[..bytes.len()].copy_from_slice(bytes);
    Some(u128::from_le_bytes(padded))
}

/// The discriminant of an enum constant: bytes, or a free `const` item, read through its type's
/// layout.
fn enum_constant<'db>(ctx: &Ctx<'_, 'db>, operand: &Operand) -> Option<u128> {
    let allocation = match &operand.kind {
        OperandKind::Allocation { allocation } => allocation.as_ref(),
        OperandKind::Constant { .. } => evaluated(ctx, operand)?,
        _ => return None,
    };
    if !matches!(allocation.memory_map, MemoryMap::Empty) {
        return None;
    }
    let ty = ctx.normalize(allocation.ty);
    let TyKind::Adt(adt, _) = ty.kind() else { return None };
    let AdtId::EnumId(it) = adt.def_id() else { return None };
    let layout = ctx.layout_of(ty)?;
    let data_layout = ctx.db.target_data_layout(ctx.krate()).ok()?;
    let (variant, _) =
        detect_variant_from_bytes(&layout, ctx.db, data_layout, &allocation.memory, it)?;
    Some(ctx.db.const_eval_discriminant(variant).ok()? as u128)
}
