//! This module provides a MIR interpreter, which is used in const eval.

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::BTreeMap,
    iter, mem,
    ops::Range,
};

use base_db::{ConstEvalBudget, Crate, Db};
use blaze_hash::{HashMap, HashSet};
use blaze_hashcons::Interned;
use either::Either;
use hir_def::{
    AdtId, DefWithBodyId, EnumVariantId, FunctionId, HasModule, ItemContainerId, Lookup, StaticId,
    VariantId,
    expr_store::HygieneId,
    item_tree::FieldsShape,
    lang_item::LangItems,
    layout::{TagEncoding, Variants},
    resolver::{HasResolver, ValueNs},
    signatures::{StaticFlags, StaticSignature, StructFlags, StructSignature},
};
use hir_expand::mod_path::path;
use hir_lint_levels::LintId;
use la_arena::ArenaMap;
use rustc_abi::{Size, TargetDataLayout};
use rustc_apfloat::{
    Float, FloatConvert,
    ieee::{Double, Half as f16, Quad as f128, Single},
};
use rustc_ast_ir::Mutability;
use rustc_type_ir::{
    AliasTyKind,
    inherent::{GenericArgs as _, IntoKind, SliceLike, Ty as _},
};
use smallvec::{SmallVec, smallvec};
use triomphe::Arc;

use crate::{
    CallableDefId, ComplexMemoryMap, InferBodyId, MemoryMap, ParamEnvAndCrate, PointerAt,
    consteval::{self, ConstEvalError, try_const_usize},
    db::{AnonConstId, GeneralConstId, HirDatabase, InternedClosureId},
    drop::{DropGlue, has_drop_glue, is_freeze},
    infer::PointerCast,
    intrinsic::Intrinsic,
    layout::{InternedLayout, Layout, RustcEnumVariantIdx, RustcFieldIdx},
    method_resolution::{is_dyn_method, lookup_impl_const},
    next_solver::{
        AliasConstKind, AliasTy, Allocation, AllocationData, AnonConstIdWrapper, Const, ConstKind,
        DbInterner, ErrorGuaranteed, FreeConstAliasId, GenericArgs, InherentAssocConstId, Region,
        TraitAssocConstId, Ty, TyKind, TypingMode, ValTree,
        infer::{DbInternerInferExt, InferCtxt, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    },
    traits::FnTrait,
    utils::detect_variant_from_bytes,
};

use super::{
    AggregateKind, AssertKind, BasicBlockId, BinOp, CastKind, LocalId, MirBody, MirPhase, MirSpan,
    Operand, OperandKind, PlaceElem, PlaceTy, ProjectionElem, Rvalue, Statement, StatementKind,
    StoredPlace, Terminator, TerminatorKind, UnOp, return_slot,
};
use super::{
    AddressRead, Budget, ComptimeError, ComptimeErrorKind, Dispatch, IsSigned, MirEvalError,
    VTableMap, pad16,
};

pub mod foreign;
mod hooks;
pub mod machine;
mod narrow;
mod provenance;
mod shim;
mod threads;
mod unwind;

use hooks::Hooks;
use machine::{Access, EvalThread, Machine, MemKind, Plain, Ptr, RetagMode, Shadow, Site, TypedCx};
use narrow::Narrow;
use provenance::Provenance;

macro_rules! from_bytes {
    // A `usize` of a target of `$word`-byte addresses: the evaluator's `cached_ptr_size`.
    (usize, $word:expr, $value:expr) => {
        match $crate::mir::eval::usize_of(AsRef::<[u8]>::as_ref(&$value), $word) {
            Some(it) => it,
            None => return Err(MirEvalError::InternalError("mismatched size in constructing usize".into())),
        }
    };
    // Without the width the `$ty` arm below would read it at the host's: refused instead.
    (usize, $value:expr) => {
        compile_error!("`from_bytes!(usize, ..)` takes the evaluator's address width: `from_bytes!(usize, self.cached_ptr_size, ..)`")
    };
    ($ty:tt, $value:expr) => {
        ($ty::from_le_bytes(match ($value).try_into() {
            Ok(it) => it,
            Err(_) => return Err(MirEvalError::InternalError(stringify!(mismatched size in constructing $ty).into())),
        }))
    };
    ($apfloat:tt, $bits:tt, $value:expr) => {
        // FIXME(#17451): Switch to builtin `f16` and `f128` once they are stable.
        $apfloat::from_bits($bits::from_le_bytes(match ($value).try_into() {
            Ok(it) => it,
            Err(_) => return Err(MirEvalError::InternalError(stringify!(mismatched size in constructing $apfloat).into())),
        }).into())
    };
}
use from_bytes;

/// A refusal of blaze's `#![feature(blaze_comptime)]` (docs/v2/plan/design/comptime.md, "Phase
/// 2b"), at `at` when known; one met in an rvalue gets its statement's.
fn comptime_error<'db>(
    kind: ComptimeErrorKind<'db>,
    at: Option<(InferBodyId<'db>, MirSpan)>,
) -> MirEvalError<'db> {
    // alloc: error path
    MirEvalError::Comptime(Box::new(ComptimeError { kind, at }))
}

pub(super) mod macro_call;

/// A float as an integer of `bits` bits, as `as` makes it and rustc's evaluation computes it
/// with rustc_apfloat: rounded toward zero, saturating, and NaN as zero.
fn apfloat_to_int<F: Float>(value: F, bits: usize, signed: bool) -> u128 {
    if signed { value.to_i128(bits).value as u128 } else { value.to_u128(bits).value }
}

/// The bytes of a float converted to the float type `to` as rustc's evaluation does: rounded
/// to nearest, ties to even, and a NaN made the positive quiet NaN without a payload.
fn convert_apfloat<F>(value: F, to: rustc_type_ir::FloatTy) -> OwnedBytes
where
    F: FloatConvert<f16> + FloatConvert<Single> + FloatConvert<Double> + FloatConvert<f128>,
{
    fn to_float<F: FloatConvert<T>, T: Float>(value: F, bytes: usize) -> OwnedBytes {
        let converted = if value.is_nan() { T::NAN } else { value.convert(&mut false).value };
        OwnedBytes::from_slice(&converted.to_bits().to_le_bytes()[0..bytes])
    }
    match to {
        rustc_type_ir::FloatTy::F16 => to_float::<F, f16>(value, 2),
        rustc_type_ir::FloatTy::F32 => to_float::<F, Single>(value, 4),
        rustc_type_ir::FloatTy::F64 => to_float::<F, Double>(value, 8),
        rustc_type_ir::FloatTy::F128 => to_float::<F, f128>(value, 16),
    }
}

macro_rules! not_supported {
    ($($it:tt)+) => {
        // alloc: error path
        return Err($crate::mir::MirEvalError::NotSupported(format!($($it)+)))
    };
}
use not_supported;

/// The type a vtable's id in memory names.
trait VTableOfBytes<'db> {
    fn ty_of_bytes(&self, bytes: &[u8]) -> Result<'db, Ty<'db>>;
}

impl<'db> VTableOfBytes<'db> for VTableMap<'db> {
    fn ty_of_bytes(&self, bytes: &[u8]) -> Result<'db, Ty<'db>> {
        // Every caller hands the one address-wide slot the id was written in.
        let id = from_bytes!(usize, bytes.len(), bytes);
        self.ty(id)
    }
}

/// Where a call goes on once its callee returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Then {
    /// At this block of the caller.
    Block(BasicBlockId),
    /// Nowhere: the callee returns `!`, and its returning is undefined behavior.
    Diverges,
    /// Out of the run of [`Evaluator::interpret_mir`] it is the first frame of, back to the
    /// evaluator's own code: a constant's evaluation, a shim's callback. Nothing in such a
    /// run can switch threads.
    Nested,
    /// The thread ends (or runs its next exit destructor): the callee is its start function.
    Thread,
    /// Unwinding goes on in the caller: the callee is a `Drop` impl the drop glue called while
    /// a panic unwound.
    Resume,
}

struct StackFrame<'db> {
    locals: Locals<'db>,
    then: Then,
    /// What of a drop the caller still has to do once this frame returns, before it goes on
    /// at `then`: the rest of the drop a `Drop` impl's call is a step of, or the closure
    /// `FnOnce::call_once` consumed.
    drops: Vec<DropStep<'db>>,
    /// The block the frame is at while it doesn't run: the one whose terminator called the
    /// frame above it (where an unwinding goes on, at its cleanup), or where it goes on once
    /// its thread runs again.
    at: BasicBlockId,
    /// A `catch_unwind`'s call of its `try` function: a panic unwinding to it is caught.
    catch: Option<unwind::Catch>,
    prev_stack_ptr: usize,
    span: (MirSpan, InferBodyId<'db>),
    /// The function called, when it is not a closure or what an evaluation starts from.
    callee: Option<FunctionId>,
    /// The caller's place the frame's own return slot is copied to once the frame is popped and
    /// the machine released its protectors: Miri's `return_place`, which the callee's `_0`
    /// never aliases. `None` where `_0` is that place itself (no machine checks), or the frame
    /// is the bottom one of [`Evaluator::interpret_mir`].
    return_to: Option<Interval>,
}

impl<'db> StackFrame<'db> {
    fn new(
        locals: Locals<'db>,
        then: Then,
        prev_stack_ptr: usize,
        span: (MirSpan, InferBodyId<'db>),
        callee: Option<FunctionId>,
    ) -> Self {
        let at = locals.body.start_block;
        let drops = Vec::new(); // alloc: empty; a drop fills it from `spare_drops`
        StackFrame {
            locals,
            then,
            drops,
            at,
            catch: None,
            prev_stack_ptr,
            span,
            callee,
            return_to: None,
        }
    }
}

/// The function a frame runs and the span of its call, for errors.
type Running<'db> = (Option<FunctionId>, (MirSpan, InferBodyId<'db>));

/// How a frame stopped running.
enum Ran<'db> {
    /// It called this frame, which runs next.
    Called(StackFrame<'db>),
    /// It tail-called this frame (`become`), which runs next in its place: it is gone, its
    /// stack freed and the machine told it returned.
    TailCalled(StackFrame<'db>),
    Returned,
    /// It tail-called what ran at once (a shim), and so returned, the machine already told.
    TailReturned,
    /// Its cleanup ended in `UnwindResume`: the panic unwinds out of it.
    Resume,
    /// Its thread gives way: to this one, or (`None`) to the one the machine picks, as the
    /// running one blocked.
    Switch(Option<EvalThread>),
}

/// An error out of a frame's statement (its index in the block) or (`None`) terminator.
struct Raised<'db> {
    error: MirEvalError<'db>,
    statement: Option<usize>,
}

#[derive(Clone, Copy)]
enum MirOrDynIndex<'db> {
    Mir(&'db MirBody<'db>),
    Dyn(usize),
}

/// A stack or heap allocation: where it starts, its size, and what its start is a multiple of.
/// A local of the stack is `tracked` while the machine has it allocated: from the frame's
/// start or its `StorageLive` to its `StorageDead` or the frame's end.
#[derive(Debug, Clone, Copy)]
struct Allocated {
    start: usize,
    size: usize,
    align: usize,
    tracked: bool,
}

/// What made a block of the evaluation's heap, which says who may free it and whether the
/// value of a constant may point into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// The evaluator itself: a constant it read, a literal, a static, a promoted temporary.
    /// Nothing frees it, and a constant's value may point to it.
    Evaluator,
    /// The program's allocator (`__rust_alloc`, `Box::new`), freed by `__rust_dealloc`. A
    /// constant's value may not point to it: the program would free it at run time.
    Allocator,
    /// `const_allocate`, freed by `const_deallocate`. A constant's value may point to it once
    /// `const_make_global` has made it global.
    ConstHeap { global: bool },
    /// The thread shims' `errno` cells and dispatch semaphores, freed only by their shim
    /// (`dispatch_release`), and the arguments and return place of an embedder's call, freed
    /// when it returns.
    Machine,
    /// An anonymous `mmap`, freed only by `munmap`.
    Mmap,
    /// C's heap (`malloc`, `calloc`, `posix_memalign`, `realloc`), freed by `free`: what std's
    /// `System` allocator asks for.
    C,
}

impl Origin {
    fn kind(self) -> MemKind {
        match self {
            Origin::Evaluator => MemKind::Global,
            Origin::Allocator => MemKind::Rust,
            Origin::ConstHeap { .. } => MemKind::ConstHeap,
            Origin::Machine => MemKind::Machine,
            Origin::Mmap => MemKind::Mmap,
            Origin::C => MemKind::C,
        }
    }
}

/// What is left of a drop ([`Evaluator::run_drop_glue`]).
enum DropStep<'db> {
    /// The drop glue of a value: its type, where it is, and its metadata if unsized.
    Glue(Ty<'db>, Address, OwnedBytes),
    /// The parts of an ADT, once its `Drop` impl ran: read then, as the impl may change
    /// them (an enum's variant).
    Fields(Ty<'db>, Address, OwnedBytes),
    /// The `Drop` impl of a value: an ADT's before its parts, a `Box`'s once what it points
    /// to is dropped.
    DropImpl(Ty<'db>, Address),
}

/// A block of the heap: the size and alignment it was asked for, the bytes it takes, of a
/// size class ([`size_class`]) a freed block of the same class and alignment is reused for,
/// and what made it.
#[derive(Debug, Clone, Copy)]
struct HeapBlock {
    size: usize,
    align: usize,
    class: usize,
    origin: Origin,
}

/// The bytes a heap block of `size` takes: a power of two, so a block freed by a `Vec` that
/// grew is the one the next `Vec` of about that size gets.
fn size_class(size: usize) -> usize {
    size.max(8).checked_next_power_of_two().unwrap_or(usize::MAX)
}

/// What one copy of a constant into an evaluation has relocated
/// ([`Evaluator::patch_addresses`]), by where things are now: each place walked at a type, and
/// each pointer word moved. A piece two pointers of the constant share (a `&'static` recipe
/// two fields name) is reached from each, and after the first walk its words hold this
/// evaluation's addresses: read again as the constant's, one that equals another piece's
/// address in the constant's evaluation (both heaps start at the same offset) would be moved
/// a second time, onto that piece, or onto no memory at all when that piece is empty
/// (`read invalid memory address 4`). rustc relocates each pointer of an allocation once, by
/// its provenance.
struct Relocation<'db> {
    walked: HashSet<(usize, Ty<'db>), std::alloc::Global>,
    words: HashMap<usize, Moved, std::alloc::Global>,
}

/// Where a pointer word [`Relocation`] moved points now.
#[derive(Clone, Copy)]
enum Moved {
    /// Into a copy of a piece of the constant (or nowhere it kept, left as it was).
    Piece,
    /// Into this evaluator's copy of a static, which is not the constant's to walk.
    Static,
}

impl Relocation<'_> {
    fn new() -> Self {
        // alloc: one relocation of a constant's copy, dropped when it is done
        Relocation {
            walked: HashSet::new_in(std::alloc::Global),
            words: HashMap::new_in(std::alloc::Global),
        }
    }
}

/// The layout of one variant of an ADT ([`Evaluator::layout_of_variant`]): the type's own,
/// or one made for a variant of an enum with several.
enum VariantLayout {
    Shared(Interned<'static, InternedLayout>),
    Made(Layout),
}

impl std::ops::Deref for VariantLayout {
    type Target = Layout;

    fn deref(&self) -> &Layout {
        match self {
            VariantLayout::Shared(it) => &it.0,
            VariantLayout::Made(it) => it,
        }
    }
}

/// The provenances the machine gave the evaluation's pointers, each once, by [`ProvId`].
struct Provs<P> {
    list: Vec<P>,
    index: HashMap<P, u32, std::alloc::Global>,
}

impl<P> Default for Provs<P> {
    fn default() -> Self {
        Provs { list: Vec::new(), index: HashMap::new_in(std::alloc::Global) } // alloc: empty, allocates nothing
    }
}

pub struct Evaluator<'db, M: Machine<'db> = Plain> {
    /// The checker of undefined behavior the run reports to; [`Plain`] for none.
    machine: Hooks<M>,
    provs: RefCell<Provs<M::Prov>>,
    db: &'db Db,
    /// The body the evaluation was made for (a constant, a static, a function), which its
    /// errors name ([`Self::evaluated`]).
    owner: InferBodyId<'db>,
    param_env: ParamEnvAndCrate<'db>,
    target_data_layout: &'db TargetDataLayout,
    stack: Vec<u8>,
    heap: Vec<u8>,
    /// Every stack allocation, in address order. A raw pointer in a constant doesn't say how
    /// much memory it can reach, its allocation does.
    stack_allocations: Vec<Allocated>,
    /// Every live block of the heap, by its offset in [`Self::heap`].
    // BTreeMap: range queries by offset (the block an address is in).
    heap_blocks: BTreeMap<usize, HeapBlock>,
    /// The offsets of freed heap blocks, by size class and alignment, for the next block of
    /// that class and alignment.
    free_blocks: HashMap<(usize, usize), Vec<usize>, std::alloc::Global>,
    /// Where a promoted temporary of a body (`&[1, 2]`) is: in the heap, as its borrow
    /// outlives the call, once for every call of the body.
    promoted: HashMap<(usize, LocalId), Address, std::alloc::Global>,
    /// Whether dropping a value of a type does anything ([`Self::has_drop_glue`]).
    drop_glue: HashMap<Ty<'db>, bool, std::alloc::Global>,
    /// Where the value of a constant operand is, once read: the program never writes it, so
    /// every use of the constant reads the same copy.
    constants: HashMap<Either<Const<'db>, Allocation<'db>>, Interval, std::alloc::Global>,
    code_stack: Vec<StackFrame<'db>>,
    /// Emptied `code_stack`s, for the nested runs of [`Self::interpret_mir`].
    spare_code_stacks: Vec<Vec<StackFrame<'db>>>,
    /// Emptied buffers of a call's evaluated arguments, one per call being made at once.
    spare_args: Vec<Vec<IntervalAndTy<'db>>>,
    /// Emptied stacks of drop steps ([`DropStep`]), one per drop under way at once.
    spare_drops: Vec<Vec<DropStep<'db>>>,
    /// The `callee` and `span` of the frame being run, which is not on `code_stack`.
    running: Option<Running<'db>>,
    /// The body of the frame being run.
    body_running: Option<&'db MirBody<'db>>,
    /// Every thread made, by [`EvalThread`]; empty while only the main thread is. The running
    /// one's state is in the fields of the evaluator (`stack`, `code_stack`, ...).
    threads: Vec<threads::Thread<'db>>,
    thread: EvalThread,
    /// The runs of [`Self::interpret_mir`] in each other (1 in the outermost one); threads
    /// switch only in the outermost.
    nesting: u32,
    /// What the shim of the call being made blocks the running thread on.
    waiting: Option<threads::Wait>,
    /// The running thread yielded at the call being made.
    yielded: bool,
    /// The running thread's destructors to call when its start function returns.
    at_exit: Vec<(usize, Address)>,
    /// The running thread's `#[thread_local]` statics, as [`Self::static_locations`].
    thread_statics: HashMap<StaticId, Address, std::alloc::Global>,
    /// What the thread shims keep for the whole process: pthread keys, signal actions.
    process: shim::Process,
    /// The virtual clock, in nanoseconds since the run started.
    clock: u64,
    /// Waits made so far, which orders a futex's waiters.
    wait_counter: u64,
    /// The running thread's panic, while it unwinds.
    unwinding: Option<unwind::Unwinding<'db>>,
    /// Where the call being made goes on, for a shim that makes the callee's frame itself.
    call_then: Then,
    /// The frame a shim made of the call being made (`drop_in_place`, `const_eval_select`).
    pending: Option<StackFrame<'db>>,
    /// The places of the call being made's `Operand::Move` arguments, which the callee's
    /// frame protects ([`Machine::protect`]); kept only with checks.
    moved_args: Vec<Interval>,
    /// A panic left the run of [`Self::interpret_mir`] being ended, with its frames on it.
    escaped: bool,
    /// Stores the global location of the statics. We const evaluate every static first time we need it
    /// and see it's missing, then we add it to this to reuse.
    static_locations: HashMap<StaticId, Address, std::alloc::Global>,
    /// The memory of each evaluated static, as start and size.
    static_memory: Vec<(usize, usize, StaticId)>,
    /// The statics whose value is read, when the others are left unevaluated: a static that
    /// is only pointed to needs no value, and may itself point back at the one being
    /// evaluated. `None` evaluates every static that is named.
    read_statics: Option<HashSet<StaticId, std::alloc::Global>>,
    /// Heap memory standing in for the statics left unevaluated, as start and size.
    unevaluated_statics: Vec<(usize, usize, StaticId)>,
    /// The unevaluated static whose memory was read, which fails the evaluation.
    missing_static: Cell<Option<StaticId>>,
    /// Evaluating at compile time (the body of a constant or static, or a const macro) rather
    /// than running a function: no extern static has a value then.
    compile_time: bool,
    /// Evaluating a `const static { .. }` block (`#![feature(blaze_comptime)]`,
    /// docs/v2/plan/design/comptime.md, "Phase 2b"): any function may run, and what cannot
    /// happen at compile time is refused as it is reached (a foreign call, a pointer's address
    /// read as an integer); the value may keep the heap it reaches through shared references.
    /// A property of the whole evaluation: such a block is always a constant of its own.
    comptime: bool,
    /// Heap memory standing in for the extern statics named at compile time, as start and
    /// size: their address is known, their value is not.
    extern_statics: Vec<(usize, usize, StaticId)>,
    /// Running a static's initializer, where the memory of every static is read-only, as in
    /// rustc.
    in_static: bool,
    /// We don't really have function pointers, i.e. pointers to some assembly instructions that we can run. Instead, we
    /// store the type as an interned id in place of function and vtable pointers, and we recover back the type at the
    /// time of use.
    vtable_map: VTableMap<'db>,
    thread_local_storage: shim::TlsData,
    random_state: blaze_rng::SplitMix64,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    layout_cache: RefCell<HashMap<Ty<'db>, Interned<'static, InternedLayout>, std::alloc::Global>>,
    projected_ty_cache:
        RefCell<HashMap<(PlaceTy<'db>, PlaceElem), PlaceTy<'db>, std::alloc::Global>>,
    not_special_fn_cache: RefCell<HashSet<FunctionId, std::alloc::Global>>,
    mir_or_dyn_index_cache:
        RefCell<HashMap<(FunctionId, GenericArgs<'db>), MirOrDynIndex<'db>, std::alloc::Global>>,
    /// Constantly dropping and creating `Locals` is very costly. We store
    /// old locals that we normally want to drop here, to reuse their allocations
    /// later.
    unused_locals_store: RefCell<HashMap<InferBodyId<'db>, Vec<Locals<'db>>, std::alloc::Global>>,
    /// [`Self::storage_marked`] of each body run under a machine, by the body's address.
    storage_marked:
        RefCell<HashMap<usize, Arc<HashSet<LocalId, std::alloc::Global>>, std::alloc::Global>>,
    cached_ptr_size: usize,
    cached_fn_trait_func: Option<FunctionId>,
    cached_fn_mut_trait_func: Option<FunctionId>,
    cached_fn_once_trait_func: Option<FunctionId>,
    crate_id: Crate,
    /// The types of the `TypeId`s made so far. An id here is its place in this list, which
    /// says whether two are of one type and is not the hash compiled code has. A field's id is
    /// its field representing type's (`FieldRepresentingType<Base, VARIANT, FIELD>`), as rustc
    /// makes it.
    type_ids: Vec<Ty<'db>>,
    /// What the evaluation may spend, of the workspace of the crate it is for.
    budget: ConstEvalBudget,
    /// The steps of [`Self::budget`] not spent yet.
    steps_left: u64,
    /// The most bytes of stack and heap together the evaluation has held.
    memory_peak: u64,
    /// The steps of `long_running_const_eval` spent, counted only in the evaluation of a
    /// constant's or static's initializer ([`Self::limits_frames`]), which the lint is of; it
    /// fires at [`LINT_TERMINATOR_LIMIT`]. A step is a run of a block rustc counts
    /// ([`ctfe_limit`](super::ctfe_limit)): a call or a loop's back edge, not every block
    /// [`Self::steps_left`] counts. The count is of one constant: a `const { .. }` block run
    /// by this evaluation ([`Self::inline_const_in_heap`]) has its own, from zero, as rustc
    /// evaluates the block by an evaluation of its own.
    lint_steps: u64,
    /// The steps the `const { .. }` blocks this evaluation ran spent of their own counts, which
    /// [`Self::lint_steps`] leaves out: what a profile adds to say what the evaluation cost.
    lint_steps_inner: u64,
    /// [`ctfe_limit::counted_blocks`](super::ctfe_limit::counted_blocks) of each body run in
    /// an evaluation the lint is of, by the body's address.
    counted_blocks: HashMap<usize, Arc<[u64]>, std::alloc::Global>,
    /// The `-C overflow-checks` of the crate a run's code is compiled as, which a body that
    /// inherits its checks takes ([`Self::set_inherited_overflow_checks`]). Unset, such a body
    /// checks, as a constant's evaluation does.
    inherited_overflow_checks: Option<bool>,
    /// An additional limit on stack depth, to prevent stack overflow
    stack_depth_limit: usize,
    /// Every call takes a frame of [`Self::stack_depth_limit`], which is the crate's recursion
    /// limit, as in rustc: the evaluation is of a constant's or static's initializer. Running a
    /// function, only the nested runs of `interpret_mir` do.
    limits_frames: bool,
    infcx: InferCtxt<'db>,
    /// What an embedder set ([`foreign`]).
    embedder: foreign::Embedder<'db, M>,
}

/// Which memory of the evaluation an address is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Space {
    Stack,
    Heap,
    Invalid,
}

/// The index of a pointer's provenance in [`Evaluator::provs`], one past it; 0 for none
/// (an integer, or any pointer when the machine checks nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
struct ProvId(u32);

/// An address: the memory it is in and the offset there, the provenance of the pointer it
/// was read from, and the alignment (log2) an access through it needs, which a place has
/// from its type and an untyped access doesn't (0). The last two fill what would be padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Address {
    offset: usize,
    prov: ProvId,
    space: Space,
    align: u8,
}

// The offset and 8 bytes of provenance, space and alignment: 16 bytes on a 64-bit target.
const _: () = assert!(size_of::<Address>() == size_of::<usize>() + 8);

/// An address, to match on.
enum At {
    Stack(usize),
    Heap(usize),
    Invalid(usize),
}

use At::*;

#[derive(Debug, Clone, Copy)]
struct Interval {
    addr: Address,
    size: usize,
}

#[derive(Debug, Clone, Copy)]
struct IntervalAndTy<'db> {
    interval: Interval,
    ty: Ty<'db>,
}

impl Interval {
    fn new(addr: Address, size: usize) -> Self {
        Self { addr, size }
    }

    fn get<'b, 'db, M: Machine<'db>>(
        &self,
        memory: &'b Evaluator<'db, M>,
    ) -> Result<'db, &'b [u8]> {
        memory.read_memory(self.addr, self.size)
    }

    fn write_from_bytes<'db, M: Machine<'db>>(
        &self,
        memory: &mut Evaluator<'db, M>,
        bytes: &[u8],
    ) -> Result<'db, ()> {
        memory.write_memory(self.addr, bytes)
    }

    fn write_from_interval<'db, M: Machine<'db>>(
        &self,
        memory: &mut Evaluator<'db, M>,
        interval: Interval,
    ) -> Result<'db, ()> {
        memory.copy_from_interval(self.addr, interval)
    }

    fn slice(self, range: Range<usize>) -> Interval {
        Interval { addr: self.addr.offset(range.start), size: range.len() }
    }
}

impl<'db> IntervalAndTy<'db> {
    fn get<'b, M: Machine<'db>>(&self, memory: &'b Evaluator<'db, M>) -> Result<'db, &'b [u8]> {
        memory.read_memory(self.interval.addr, self.interval.size)
    }

    fn new<M: Machine<'db>>(
        addr: Address,
        ty: Ty<'db>,
        evaluator: &Evaluator<'db, M>,
        locals: &Locals<'db>,
    ) -> Result<'db, IntervalAndTy<'db>> {
        let size = evaluator.size_of_sized(ty, locals, "type of interval")?;
        Ok(IntervalAndTy { interval: Interval { addr, size }, ty })
    }
}

/// The bytes of a computed value, inline up to 16: every scalar up to `u128` and a wide
/// pointer. Only SIMD vectors and aggregates spill.
type OwnedBytes = SmallVec<[u8; 16]>;

/// A value: in memory, or computed. `Owned` bytes are all initialized and carry no
/// provenance; `Shadowed` ones say which are and what they carry.
enum IntervalOrOwned<S = ()> {
    Owned(OwnedBytes),
    Shadowed(OwnedBytes, S),
    Borrowed(Interval),
}

impl<S> From<Interval> for IntervalOrOwned<S> {
    fn from(it: Interval) -> IntervalOrOwned<S> {
        IntervalOrOwned::Borrowed(it)
    }
}

impl<S> IntervalOrOwned<S> {
    fn get<'b, 'db, M: Machine<'db>>(
        &'b self,
        memory: &'b Evaluator<'db, M>,
    ) -> Result<'db, &'b [u8]> {
        Ok(match self {
            IntervalOrOwned::Owned(o) | IntervalOrOwned::Shadowed(o, _) => o,
            IntervalOrOwned::Borrowed(b) => b.get(memory)?,
        })
    }
}

/// Where the addresses of the stack and of the heap start on a target of `word`-byte
/// addresses, which they have to fit. `word` is the evaluator's (`cached_ptr_size`, from the
/// crate's data layout), never the process's: one process evaluates for wasm32 and the host.
fn stack_offset(word: usize) -> usize {
    1 << (word * 8 - 4)
}

fn heap_offset(word: usize) -> usize {
    1 << (word * 8 - 5)
}

/// The `usize` of a target of `word`-byte addresses that `bytes` are.
fn usize_of(bytes: &[u8], word: usize) -> Option<usize> {
    let mut value = [0; size_of::<usize>()];
    (bytes.len() == word).then_some(())?;
    value.get_mut(..word)?.copy_from_slice(bytes);
    Some(usize::from_le_bytes(value))
}

/// `value` as a `usize` of a target of `word`-byte addresses.
fn word_bytes(value: usize, word: usize) -> smallvec::SmallVec<[u8; 8]> {
    smallvec::SmallVec::from_slice(&value.to_le_bytes()[..word])
}

impl Address {
    fn of(space: Space, offset: usize) -> Address {
        Address { offset, prov: ProvId(0), space, align: 0 }
    }

    fn stack(offset: usize) -> Address {
        Address::of(Space::Stack, offset)
    }

    fn heap(offset: usize) -> Address {
        Address::of(Space::Heap, offset)
    }

    fn invalid(addr: usize) -> Address {
        Address::of(Space::Invalid, addr)
    }

    fn at(&self) -> At {
        match self.space {
            Space::Stack => Stack(self.offset),
            Space::Heap => Heap(self.offset),
            Space::Invalid => Invalid(self.offset),
        }
    }

    fn from_bytes<'db>(it: &[u8], word: usize) -> Result<'db, Self> {
        Ok(Address::from_usize(from_bytes!(usize, word, it), word))
    }

    fn from_usize(it: usize, word: usize) -> Self {
        if it > stack_offset(word) {
            Address::stack(it - stack_offset(word))
        } else if it > heap_offset(word) {
            Address::heap(it - heap_offset(word))
        } else {
            Address::invalid(it)
        }
    }

    /// Whether this is a number made into a pointer to `size` bytes, none: no memory is
    /// behind it, and the number is the whole of its value.
    fn is_of_nothing(&self, size: usize) -> bool {
        size == 0 && self.space == Space::Invalid
    }

    fn to_bytes(&self, word: usize) -> smallvec::SmallVec<[u8; 8]> {
        word_bytes(self.to_usize(word), word)
    }

    fn to_usize(&self, word: usize) -> usize {
        match self.space {
            Space::Stack => self.offset + stack_offset(word),
            Space::Heap => self.offset + heap_offset(word),
            Space::Invalid => self.offset,
        }
    }

    /// The address `f` moves this to, of the same provenance; what an access there needs is
    /// for its own type to say.
    fn map(&self, f: impl FnOnce(usize) -> usize) -> Address {
        Address { offset: f(self.offset), align: 0, ..*self }
    }

    fn offset(&self, offset: usize) -> Address {
        self.map(|it| it + offset)
    }

    /// This address, accessed as a value of alignment `align`.
    fn aligned(self, align: usize) -> Address {
        Address { align: align.trailing_zeros() as u8, ..self }
    }

    fn align(&self) -> u64 {
        1 << self.align
    }
}

type Result<'db, T> = std::result::Result<T, MirEvalError<'db>>;

/// The steps at which rustc's const evaluation asks for `long_running_const_eval`
/// (`rustc_const_eval`'s `LINT_TERMINATOR_LIMIT`).
pub const LINT_TERMINATOR_LIMIT: u64 = 2_000_000;

#[derive(Debug)]
struct Locals<'db> {
    ptr: ArenaMap<LocalId, Interval>,
    body: &'db MirBody<'db>,
}

pub struct MirOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl MirOutput {
    pub fn stdout(&self) -> Cow<'_, str> {
        // alloc: the program's output, at the edge, and only when it is not UTF-8
        String::from_utf8_lossy(&self.stdout)
    }
    pub fn stderr(&self) -> Cow<'_, str> {
        // alloc: the program's output, at the edge, and only when it is not UTF-8
        String::from_utf8_lossy(&self.stderr)
    }
}

pub fn interpret_mir<'db>(
    db: &'db Db,
    body: &'db MirBody<'db>,
    trait_env: Option<ParamEnvAndCrate<'db>>,
) -> Result<'db, (Result<'db, Allocation<'db>>, MirOutput)> {
    // What an evaluation spent, over its runs: `blocks` of the budget (every block run) and
    // `steps` of `long_running_const_eval` (the calls and loop turns rustc counts, 2,000,000 at
    // most without an `#[allow]` in the constant and in each `const { .. }` block it runs, whose
    // steps are added here), so a profile says which constant took the time and in which unit
    // it would hit a limit.
    let span = tracing::info_span!(
        target: "blaze",
        "const eval",
        body = %evaluated(db, body.owner),
        blocks = tracing::field::Empty,
        steps = tracing::field::Empty,
        runs = tracing::field::Empty,
    )
    .entered();
    let (mut blocks, mut steps, mut runs) = (0u64, 0u64, 0u32);
    let ty = body.locals[return_slot()].ty.as_ref();
    let mut read_statics = HashSet::new_in(std::alloc::Global);
    loop {
        let mut evaluator =
            Evaluator::with_machine(db, body.owner, trait_env, Provenance::default())
                .map_err(|(e, _)| e)?;
        // The evaluator only asks of it, so it comes back as it went in.
        evaluator.read_statics = Some(mem::take(&mut read_statics));
        let it = evaluator.evaluate(body, ty);
        blocks += evaluator.budget.steps - evaluator.steps_left;
        steps += evaluator.lint_steps + evaluator.lint_steps_inner;
        runs += 1;
        read_statics = evaluator.read_statics.take().unwrap_or_default();
        if let Some(read) = evaluator.missing_static.take()
            && read_statics.insert(read)
        {
            continue;
        }
        span.record("blocks", blocks).record("steps", steps).record("runs", runs);
        return Ok((it, MirOutput { stdout: evaluator.stdout, stderr: evaluator.stderr }));
    }
}

/// Runs `func`, a function of no arguments and no generics (a `#[test]`), with `machine`
/// watching, and asks it to `finish` once the main thread returns.
pub fn run_function<'db, M: Machine<'db>>(
    db: &'db Db,
    func: FunctionId,
    machine: M,
) -> (Result<'db, ()>, MirOutput, M) {
    let env = ParamEnvAndCrate {
        param_env: db.trait_environment(func.into()),
        krate: func.module(db).krate(db),
    };
    let body = match db.monomorphized_runtime_mir_body(
        func.into(),
        GenericArgs::empty().store(),
        env.store(),
    ) {
        Ok(body) => body,
        Err(e) => {
            // alloc: error path
            let output = MirOutput { stdout: Vec::new(), stderr: Vec::new() };
            return (Err(MirEvalError::MirLowerError(func, e)), output, machine);
        }
    };
    let mut evaluator = match Evaluator::with_machine(db, body.owner, Some(env), machine) {
        Ok(it) => it,
        Err((e, machine)) => {
            // alloc: error path
            let output = MirOutput { stdout: Vec::new(), stderr: Vec::new() };
            return (Err(e), output, machine);
        }
    };
    let called = Some((func, MirSpan::Unknown, body.owner));
    let base = evaluator.stack.len();
    let result = evaluator
        .interpret_mir(body, iter::empty(), called)
        // Miri pops `main`'s frame, freeing its locals (those without storage statements
        // too), before the exit calls and the leak check.
        .and_then(|_| evaluator.free_stack(base))
        .and_then(|_| evaluator.main_returned(body))
        .and_then(|_| evaluator.machine.finish());
    let output = MirOutput {
        stdout: mem::take(&mut evaluator.stdout),
        stderr: mem::take(&mut evaluator.stderr),
    };
    (result, output, evaluator.machine.into_inner())
}

impl<'db, M: Machine<'db>> Evaluator<'db, M> {
    /// Runs a body without arguments and takes its result out of the evaluator's memory.
    fn evaluate(&mut self, body: &'db MirBody<'db>, ty: Ty<'db>) -> Result<'db, Allocation<'db>> {
        let interval = self.interpret_mir(body, None.into_iter(), None)?;
        let bytes = interval.get(self)?;
        let mut memory_map =
            self.create_memory_map(bytes, ty, &Locals { ptr: ArenaMap::new(), body })?;
        // The ids made here are not rustc's hashes and mean nothing outside this evaluation,
        // so a constant that may hold one says what each is of. A `TypeId` is pointers to
        // const eval, which cannot read them as integers, so no other value is computed from
        // an id's bytes: comparing two (`IS_UNIT` in core) and reflection's counts, sizes and
        // names are answered.
        if !self.type_ids.is_empty()
            && !self.holds_no_type_id(ty, &mut HashSet::new_in(std::alloc::Global))
        {
            // alloc: the constant's map, kept with it
            memory_map.type_ids = self.type_ids.iter().map(|&ty| Some(ty)).collect();
        }
        let pointers = self.note_pointers(interval, &mut memory_map)?;
        let bytes = Box::from(bytes);
        let memory_map = if memory_map.memory.is_empty()
            && memory_map.statics.is_empty()
            && memory_map.type_ids.is_empty()
            && self.vtable_map.is_empty()
            && !pointers
        {
            MemoryMap::Empty
        } else {
            memory_map.vtable = mem::take(&mut self.vtable_map);
            memory_map.vtable.shrink_to_fit();
            memory_map.memory.shrink_to_fit();
            // alloc: the constant's map, kept with it
            MemoryMap::Complex(Box::new(memory_map))
        };
        Ok(Allocation::new(AllocationData { ty, memory: bytes, memory_map }))
    }

    /// Notes in `map` each word of the value in `value` and of the memory `map` kept that is
    /// a pointer with provenance, as the machine knows them; whether there was one. Without a
    /// machine that tracks provenance there is none.
    fn note_pointers(&self, value: Interval, map: &mut ComplexMemoryMap<'db>) -> Result<'db, bool> {
        if !M::CHECKS {
            return Ok(false);
        }
        let size = self.ptr_size();
        let mut noted = false;
        let (_, shadow) = self.read_shadowed(value.addr, value.size)?;
        shadow.pointers(value.size, size, |at| {
            map.note_pointer(PointerAt::Value(at));
            noted = true;
        });
        let pieces: SmallVec<[(usize, usize); 8]> =
            // alloc: the kept pieces' places, once per constant, as `note_pointer` takes the map
            map.memory.iter().map(|(&addr, bytes)| (addr, bytes.len())).collect();
        for (addr, len) in pieces {
            let (_, shadow) =
                self.read_shadowed(Address::from_usize(addr, self.cached_ptr_size), len)?;
            shadow.pointers(len, size, |at| {
                map.note_pointer(PointerAt::Memory(addr + at));
                noted = true;
            });
        }
        Ok(noted)
    }
}

impl<'db> Evaluator<'db> {
    pub fn new(
        db: &'db Db,
        owner: InferBodyId<'db>,
        trait_env: Option<ParamEnvAndCrate<'db>>,
    ) -> Result<'db, Evaluator<'db>> {
        Evaluator::with_machine(db, owner, trait_env, Plain).map_err(|(e, Plain)| e)
    }
}

impl<'db, M: Machine<'db>> Evaluator<'db, M> {
    /// An evaluator that reports to `machine`, or gives it back with the error.
    pub fn with_machine(
        db: &'db Db,
        owner: InferBodyId<'db>,
        trait_env: Option<ParamEnvAndCrate<'db>>,
        machine: M,
    ) -> std::result::Result<Evaluator<'db, M>, (MirEvalError<'db>, M)> {
        let module = owner.module(db);
        let crate_id = module.krate(db);
        let target_data_layout = match db.target_data_layout(crate_id) {
            Ok(target_data_layout) => target_data_layout,
            Err(e) => return Err((MirEvalError::TargetDataLayoutNotAvailable(e), machine)),
        };
        let cached_ptr_size = target_data_layout.pointer_size().bytes_usize();
        // Impls are looked up from the crate the body is evaluated for: the ones its generic
        // arguments select can be in crates the body's own doesn't depend on.
        let interner = DbInterner::new_with(db, trait_env.map_or(crate_id, |env| env.krate));
        let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
        // A caller's environment (a pass over a `for const` body) can assume things of a
        // binder; the owner's own names none, and a body the evaluator runs is expanded.
        if let Some(env) = trait_env {
            hir_ty::comptime_env::enter_universes(&infcx, env.param_env);
        }
        let lang_items = interner.lang_items();
        let budget = crate_id.workspace_data(db).const_eval;
        let limits_frames =
            !matches!(owner, InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(_)));
        let stack_depth_limit = if limits_frames {
            hir_def::nameres::crate_def_map(db, crate_id).recursion_limit() as usize
        } else {
            100
        };
        Ok(Evaluator {
            machine: Hooks::new(machine),
            provs: RefCell::default(),
            target_data_layout,
            // alloc: one evaluation's stack and heap, reused across its calls
            stack: vec![0],
            // alloc: one evaluation's stack and heap, reused across its calls
            heap: vec![0],
            stack_allocations: Vec::new(), // alloc: empty, allocates nothing; one per evaluation
            heap_blocks: BTreeMap::new(),
            free_blocks: HashMap::new_in(std::alloc::Global),
            promoted: HashMap::new_in(std::alloc::Global),
            drop_glue: HashMap::new_in(std::alloc::Global),
            constants: HashMap::new_in(std::alloc::Global),
            code_stack: vec![], // alloc: empty, allocates nothing; one per evaluation
            spare_code_stacks: Vec::new(), // alloc: empty, allocates nothing; one per evaluation
            spare_args: Vec::new(), // alloc: empty, allocates nothing; one per evaluation
            spare_drops: Vec::new(), // alloc: empty, allocates nothing; one per evaluation
            running: None,
            body_running: None,
            threads: Vec::new(), // alloc: empty, allocates nothing; one per evaluation
            thread: 0,
            nesting: 0,
            waiting: None,
            yielded: false,
            at_exit: Vec::new(), // alloc: empty, allocates nothing; one per evaluation
            thread_statics: HashMap::new_in(std::alloc::Global),
            process: shim::Process::default(),
            clock: 0,
            wait_counter: 0,
            unwinding: None,
            call_then: Then::Nested,
            pending: None,
            moved_args: Vec::new(), // alloc: empty, allocates nothing; one per evaluation
            escaped: false,
            vtable_map: VTableMap::default(),
            thread_local_storage: shim::TlsData::default(),
            static_locations: Default::default(),
            static_memory: Vec::new(), // alloc: empty, allocates nothing; one per evaluation
            read_statics: None,
            unevaluated_statics: Vec::new(), // alloc: empty, allocates nothing; one per evaluation
            missing_static: Cell::new(None),
            compile_time: !matches!(
                owner,
                InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(_))
            ),
            comptime: matches!(
                owner,
                InferBodyId::AnonConstId(konst) if hir_ty::effects::anon_const_is_comptime(db, konst)
            ),
            extern_statics: Vec::new(), // alloc: empty, allocates nothing; one per evaluation
            in_static: matches!(owner, InferBodyId::DefWithBodyId(DefWithBodyId::StaticId(_))),
            db,
            owner,
            random_state: blaze_rng::SplitMix64::new(0),
            param_env: trait_env.unwrap_or_else(|| ParamEnvAndCrate {
                param_env: db.trait_environment(owner.generic_def(db)),
                krate: crate_id,
            }),
            crate_id,
            stdout: vec![], // alloc: empty, allocates nothing; one per evaluation
            stderr: vec![], // alloc: empty, allocates nothing; one per evaluation
            type_ids: Vec::new(), // alloc: empty, allocates nothing; one per evaluation
            stack_depth_limit,
            limits_frames,
            budget,
            steps_left: budget.steps,
            memory_peak: 0,
            lint_steps: 0,
            lint_steps_inner: 0,
            counted_blocks: HashMap::new_in(std::alloc::Global),
            inherited_overflow_checks: None,
            layout_cache: RefCell::new(Default::default()),
            projected_ty_cache: RefCell::new(Default::default()),
            not_special_fn_cache: RefCell::new(Default::default()),
            mir_or_dyn_index_cache: RefCell::new(Default::default()),
            unused_locals_store: RefCell::new(Default::default()),
            storage_marked: RefCell::default(),
            cached_ptr_size,
            cached_fn_trait_func: lang_items.Fn_call,
            cached_fn_mut_trait_func: lang_items.FnMut_call_mut,
            cached_fn_once_trait_func: lang_items.FnOnce_call_once,
            infcx,
            embedder: Default::default(),
        })
    }

    #[inline]
    /// The body this evaluation was made for, as an error names it: "the constant `GOAL`".
    fn evaluated(&self) -> String {
        evaluated(self.db, self.owner)
    }

    fn interner(&self) -> DbInterner<'db> {
        self.infcx.interner
    }

    #[inline]
    fn lang_items(&self) -> &'db LangItems {
        self.infcx.interner.lang_items()
    }

    fn place_addr(&self, p: &StoredPlace, locals: &Locals<'db>) -> Result<'db, Address> {
        Ok(self.place_addr_and_ty_and_metadata(p, locals)?.0)
    }

    fn place_interval(&self, p: &StoredPlace, locals: &Locals<'db>) -> Result<'db, Interval> {
        let (addr, ty, _) = self.place_addr_and_ty_and_metadata(p, locals)?;
        Ok(Interval {
            addr,
            size: self.size_of_sized(ty, locals, "Type of place that we need its interval")?,
        })
    }

    fn ptr_size(&self) -> usize {
        self.cached_ptr_size
    }

    fn projected_ty(&self, ty: PlaceTy<'db>, proj: PlaceElem) -> PlaceTy<'db> {
        let pair = (ty, proj);
        if let Some(r) = self.projected_ty_cache.borrow().get(&pair) {
            return *r;
        }
        let (ty, proj) = pair;
        let r = ty.projection_ty(&self.infcx, &proj, self.param_env.param_env);
        self.projected_ty_cache.borrow_mut().insert((ty, proj), r);
        r
    }

    fn place_addr_and_ty_and_metadata<'b>(
        &'b self,
        p: &StoredPlace,
        locals: &'b Locals<'db>,
    ) -> Result<'db, (Address, Ty<'db>, Option<IntervalOrOwned<M::Shadow>>)> {
        let mut addr = locals.ptr[p.local].addr;
        let mut ty = PlaceTy::from_ty(locals.body.locals[p.local].ty.as_ref());
        let mut metadata: Option<IntervalOrOwned<M::Shadow>> = None; // locals are always sized
        // Behind a dereference every projection is an inbounds offset of the pointer.
        let mut behind_deref = false;
        for proj in p.projection.as_slice() {
            let prev_ty = ty;
            ty = self.projected_ty(ty, *proj);
            match proj {
                ProjectionElem::Deref => {
                    let size = self.ptr_size();
                    metadata = if self.size_align_of(ty.ty, locals)?.is_none() {
                        Some(Interval { addr: addr.offset(size).aligned(size), size }.into())
                    } else {
                        None
                    };
                    addr = self.read_pointer(addr)?;
                    behind_deref = true;
                }
                ProjectionElem::Index(op) => {
                    let offset = from_bytes!(
                        usize,
                        self.cached_ptr_size,
                        self.read_scalar(locals.ptr[*op])?
                    );
                    metadata = None; // Result of index is always sized
                    let ty_size = self.size_of_sized(ty.ty, locals, "array inner type")?;
                    addr = self.project(addr, ty_size * offset, behind_deref)?;
                }
                &ProjectionElem::ConstantIndex { from_end, offset, .. } => {
                    let offset = if from_end {
                        let len = match prev_ty.ty.kind() {
                            TyKind::Array(_, c) => match try_const_usize(self.db, c) {
                                Some(it) => it as u64,
                                None => {
                                    not_supported!("indexing array with unknown const from end")
                                }
                            },
                            TyKind::Slice(_) => match &metadata {
                                Some(it) => {
                                    from_bytes!(usize, self.cached_ptr_size, self.scalar(it)?)
                                        as u64
                                }
                                None => not_supported!("slice place without metadata"),
                            },
                            _ => not_supported!("bad type for const index"),
                        };
                        (len - offset) as usize
                    } else {
                        offset as usize
                    };
                    metadata = None; // Result of index is always sized
                    let ty_size = self.size_of_sized(ty.ty, locals, "array inner type")?;
                    addr = self.project(addr, ty_size * offset, behind_deref)?;
                }
                &ProjectionElem::Subslice { from, to } => {
                    let inner_ty = match ty.ty.kind() {
                        TyKind::Array(inner, _) | TyKind::Slice(inner) => inner,
                        _ => Ty::new_error(self.interner(), ErrorGuaranteed),
                    };
                    metadata = match metadata {
                        Some(it) => {
                            let prev_len =
                                from_bytes!(usize, self.cached_ptr_size, self.scalar(&it)?) as u64;
                            Some(IntervalOrOwned::Owned(OwnedBytes::from_slice(&word_bytes(
                                (prev_len - from - to) as usize,
                                self.cached_ptr_size,
                            ))))
                        }
                        None => None,
                    };
                    let ty_size = self.size_of_sized(inner_ty, locals, "array inner type")?;
                    addr = self.project(addr, ty_size * (from as usize), behind_deref)?;
                }
                ProjectionElem::Field(f) => {
                    let layout = self.layout(prev_ty.ty)?;
                    let offset = match &layout.variants {
                        Variants::Empty => {
                            return Err(MirEvalError::UndefinedBehavior(
                                // alloc: error path
                                "field of an uninhabited type".to_owned(),
                            ));
                        }
                        Variants::Single { .. } => layout.fields.offset(f.0 as usize),
                        Variants::Multiple { variants, .. } => {
                            let Some(hir_def::VariantId::EnumVariantId(it)) = prev_ty.variant_id
                            else {
                                return Err(MirEvalError::InternalError(
                                    "mismatched layout".into(),
                                ));
                            };
                            variants[RustcEnumVariantIdx(it.index(self.db))].field_offsets
                                [RustcFieldIdx::new(f.0 as usize)]
                        }
                    };
                    let offset = offset.bytes_usize();
                    addr = self.project(addr, offset, behind_deref)?;
                    // Unsized field metadata is equal to the metadata of the struct
                    if self.size_align_of(ty.ty, locals)?.is_some() {
                        metadata = None;
                    }
                }
                ProjectionElem::Downcast(_) => {
                    // no runtime effect
                }
                ProjectionElem::ReflectedField(_) => {
                    return Err(MirEvalError::ComptimeUnexpanded(
                        hir_ty::mir::ComptimeUnexpanded::new(
                            hir_ty::mir::MirComptimeForm::ReflectedField,
                            "the MIR evaluator",
                        ),
                    ));
                }
            }
        }
        // An unsized place is never accessed whole (its fields and elements are places of
        // their own), so only a sized one says its alignment.
        if M::CHECKS
            && let Some((_, align)) = self.size_align_of(ty.ty, locals)?
        {
            addr = addr.aligned(align);
        }
        Ok((addr, ty.ty, metadata))
    }

    /// `addr` moved `by` bytes into what it points to: behind a dereference an inbounds
    /// offset, which the machine checks; within a local otherwise.
    #[inline(always)]
    fn project(&self, addr: Address, by: usize, behind_deref: bool) -> Result<'db, Address> {
        if M::CHECKS && behind_deref && by != 0 {
            self.machine.offset(self.ptr(addr), by as i64)?;
        }
        Ok(addr.offset(by))
    }

    /// The bytes of a value used as a scalar (a length, an index), which must all be
    /// initialized.
    fn scalar<'b>(&'b self, value: &'b IntervalOrOwned<M::Shadow>) -> Result<'db, &'b [u8]> {
        match value {
            IntervalOrOwned::Owned(bytes) => Ok(bytes),
            IntervalOrOwned::Shadowed(bytes, shadow) => {
                if M::CHECKS {
                    self.machine.require_init(shadow, 0..bytes.len())?;
                }
                Ok(bytes)
            }
            IntervalOrOwned::Borrowed(it) => self.read_scalar(*it),
        }
    }

    fn layout(&self, ty: Ty<'db>) -> Result<'db, Interned<'static, InternedLayout>> {
        if let Some(x) = self.layout_cache.borrow().get(&ty) {
            return Ok(*x);
        }
        let r = self
            .db
            .layout_of_ty(ty.store(), self.param_env.store())
            .map_err(|e| MirEvalError::LayoutError(e, ty.store()))?;
        self.layout_cache.borrow_mut().insert(ty, r);
        Ok(r)
    }

    fn layout_adt(
        &self,
        adt: AdtId,
        subst: GenericArgs<'db>,
    ) -> Result<'db, Interned<'static, InternedLayout>> {
        self.layout(Ty::new_adt(self.interner(), adt, subst))
    }

    /// The type of the place `p`, which reads no memory: the machine sees each place's
    /// pointers read once, where it is evaluated.
    fn place_ty<'b>(&'b self, p: &StoredPlace, locals: &'b Locals<'db>) -> Result<'db, Ty<'db>> {
        let mut ty = PlaceTy::from_ty(locals.body.locals[p.local].ty.as_ref());
        for proj in p.projection.as_slice() {
            ty = self.projected_ty(ty, *proj);
        }
        Ok(ty.ty)
    }

    /// Whether arithmetic of the body being evaluated panics when it overflows. A function's
    /// does what the code compiled from it does. The initializer of a constant, a static or a
    /// discriminant always checks, as rustc builds it, whatever `-C overflow-checks` says. A
    /// function that inherits its checks takes those of the crate the run is compiled as.
    fn checks_overflow(&self, locals: &Locals<'db>) -> bool {
        !matches!(locals.body.owner, InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(_)))
            || crate::mir::overflow::body_checks_overflow(self.db, locals.body)
                .unwrap_or(self.inherited_overflow_checks.unwrap_or(true))
    }

    fn operand_ty(&self, o: &Operand, locals: &Locals<'db>) -> Result<'db, Ty<'db>> {
        Ok(match &o.kind {
            OperandKind::Copy(p) | OperandKind::Move(p) => self.place_ty(p, locals)?,
            OperandKind::Constant { konst: _, ty } => ty.as_ref(),
            OperandKind::Allocation { allocation } => allocation.as_ref().ty,
            &OperandKind::Static(s) => {
                let ty = self.db.type_for_static(s).instantiate_identity().skip_norm_wip();
                Ty::new_ref(
                    self.interner(),
                    Region::new_static(self.interner()),
                    ty,
                    Mutability::Not,
                )
            }
        })
    }

    fn operand_ty_and_eval(
        &mut self,
        o: &Operand,
        locals: &mut Locals<'db>,
    ) -> Result<'db, IntervalAndTy<'db>> {
        Ok(IntervalAndTy {
            interval: self.eval_operand(o, locals)?,
            ty: self.operand_ty(o, locals)?,
        })
    }

    fn interpret_mir(
        &mut self,
        body: &'db MirBody<'db>,
        args: impl Iterator<Item = IntervalOrOwned<M::Shadow>>,
        called: Option<(FunctionId, MirSpan, InferBodyId<'db>)>,
    ) -> Result<'db, Interval> {
        let depth_limit = self.stack_depth_limit;
        if let Some(it) = self.stack_depth_limit.checked_sub(1) {
            self.stack_depth_limit = it;
        } else {
            return Err(MirEvalError::StackOverflow);
        }
        let (mut locals, prev_stack_ptr) = self.create_locals_for_body(body, None)?;
        self.enter_frame(body, &mut locals, args, None)?;
        let spare = self.spare_code_stacks.pop().unwrap_or_default();
        let prev_code_stack = mem::replace(&mut self.code_stack, spare);
        let prev_running = self.running;
        let prev_body = self.body_running;
        let (callee, span) = match called {
            Some((callee, span, caller)) => (Some(callee), (span, caller)),
            None => (None, (MirSpan::Unknown, body.owner)),
        };
        self.code_stack.push(StackFrame::new(locals, Then::Nested, prev_stack_ptr, span, callee));
        self.nesting += 1;
        let result = self.run_stack(body.start_block);
        self.nesting -= 1;
        let mut my_code_stack = mem::replace(&mut self.code_stack, prev_code_stack);
        self.running = prev_running;
        self.body_running = prev_body;
        self.stack_depth_limit = depth_limit;
        if result.is_ok() {
            my_code_stack.clear();
            self.spare_code_stacks.push(my_code_stack);
            return result;
        }
        result.map_err(|e| {
            if mem::take(&mut self.escaped) {
                return e;
            }
            let error_stack = my_code_stack
                .into_iter()
                .rev()
                .filter_map(|frame| {
                    let f = frame.locals.body.owner.as_function()?;
                    Some((Either::Left(f), frame.span.0, frame.span.1))
                })
                // alloc: error path
                .collect();
            // alloc: error path
            MirEvalError::InFunction(Box::new(e), error_stack)
        })
    }

    /// Runs the frames of [`Self::code_stack`], the top one from `block`, until the bottom
    /// one, which [`Self::interpret_mir`] made, returns: its return place is the result.
    fn run_stack(&mut self, mut block: BasicBlockId) -> Result<'db, Interval> {
        loop {
            let Some(mut frame) = self.code_stack.pop() else {
                not_supported!("missing stack frame");
            };
            self.running = Some((frame.callee, frame.span));
            self.body_running = Some(frame.locals.body);
            match self.run_frame(&mut frame, &mut block) {
                Ok(Ran::Called(callee)) => {
                    self.count_frame()?;
                    frame.at = block;
                    block = callee.locals.body.start_block;
                    self.code_stack.push(frame);
                    self.code_stack.push(callee);
                }
                Ok(Ran::Switch(next)) => {
                    self.code_stack.push(frame);
                    block = self.switch_thread(next, block)?;
                }
                // Takes no frame of the depth limit: the callee is in the caller's place.
                Ok(Ran::TailCalled(callee)) => {
                    block = callee.locals.body.start_block;
                    let StackFrame { locals, .. } = frame;
                    let owner = locals.body.owner;
                    self.unused_locals_store.borrow_mut().entry(owner).or_default().push(locals);
                    self.code_stack.push(callee);
                }
                Ok(Ran::Returned) => {
                    self.machine.ret()?;
                    if let Some(result) = self.returned(frame, &mut block, false)? {
                        return Ok(result);
                    }
                }
                Ok(Ran::TailReturned) => {
                    if let Some(result) = self.returned(frame, &mut block, true)? {
                        return Ok(result);
                    }
                }
                Ok(Ran::Resume) => {
                    frame.at = block;
                    self.unwind_out(frame, &mut block)?;
                }
                Err(Raised { mut error, statement }) => {
                    if let MirEvalError::OverBudget { at: at @ None, .. }
                    | MirEvalError::LongRunning { at: at @ None } = &mut error
                    {
                        let body = frame.locals.body;
                        let terminator = body.basic_blocks[block].terminator.as_ref();
                        *at = Some((body.owner, terminator.map_or(MirSpan::Unknown, |it| it.span)));
                    }
                    frame.at = block;
                    self.raise(frame, &mut block, error, statement)?;
                }
            }
        }
    }

    /// A step of `long_running_const_eval` in an evaluation the lint is of, in `current`, the
    /// body running: [`Self::long_running`] once it is the [`LINT_TERMINATOR_LIMIT`]th.
    fn lint_step(&mut self, current: InferBodyId<'db>) -> Result<'db, ()> {
        self.lint_steps += 1;
        if self.lint_steps == LINT_TERMINATOR_LIMIT { self.long_running(current) } else { Ok(()) }
    }

    /// rustc's `long_running_const_eval` once [`LINT_TERMINATOR_LIMIT`] steps are spent (its
    /// `increment_const_eval_counter` at each `ConstEvalCounter`, [`ctfe_limit`](super::ctfe_limit)),
    /// at the level in force where the outermost frame of a body of the evaluation's crate is
    /// (`best_lint_scope`), or at the crate's root when no frame is of it. An error level stops the evaluation; any other goes on.
    #[cold]
    fn long_running(&self, current: InferBodyId<'db>) -> Result<'db, ()> {
        let db = self.db;
        let lint = LintId::LONG_RUNNING_CONST_EVAL;
        let local = self
            .code_stack
            .iter()
            .map(|frame| frame.locals.body.owner)
            .chain(std::iter::once(current))
            .find(|owner| owner.module(db).krate(db) == self.crate_id);
        let level = match local {
            Some(InferBodyId::DefWithBodyId(owner)) => hir_lint_levels::lint_level(db, owner, lint),
            Some(InferBodyId::AnonConstId(konst)) => {
                let loc = konst.loc(db);
                hir_lint_levels::lint_level_at(db, loc.owner, Some(loc.expr), lint)
                    // alloc: the error's message
                    .map_err(|error| MirEvalError::InternalError(error.to_string().into()))?
            }
            None => hir_lint_levels::crate_lint_level(db, self.crate_id, lint),
        };
        if level.is_error() { Err(MirEvalError::LongRunning { at: None }) } else { Ok(()) }
    }

    /// Runs `frame` from `block` until it calls, returns, unwinds or its thread gives way;
    /// `block` is then where it goes on.
    fn run_frame(
        &mut self,
        frame: &mut StackFrame<'db>,
        block: &mut BasicBlockId,
    ) -> std::result::Result<Ran<'db>, Raised<'db>> {
        let body = frame.locals.body;
        // Looked up once per run of the frame (a call or a return ends it), and only in an
        // evaluation the lint is of.
        let counted = self.limits_frames.then(|| self.counted_blocks(body));
        loop {
            let current_block = &body.basic_blocks[*block];
            if let Some(it) = self.steps_left.checked_sub(1) {
                self.steps_left = it;
            } else {
                let error =
                    MirEvalError::OverBudget { spent: Budget::Steps(self.budget.steps), at: None };
                return Err(Raised { error, statement: None });
            }
            if let Some(counted) = &counted
                && super::ctfe_limit::counts(counted, *block)
            {
                self.lint_step(body.owner).map_err(|error| Raised { error, statement: None })?;
            }
            for (i, statement) in current_block.statements.iter().enumerate() {
                self.exec_statement(statement, &mut frame.locals)
                    .map_err(|error| Raised { error, statement: Some(i) })?;
            }
            let raised = |error| Raised { error, statement: None };
            let Some(terminator) = current_block.terminator.as_ref() else {
                return Err(raised(MirEvalError::NotSupported("block without terminator".into())));
            };
            if M::CHECKS {
                self.machine.step(Site { owner: body.owner, span: terminator.span });
            }
            if let Some(ran) = self.exec_terminator(terminator, frame, block).map_err(raised)? {
                return Ok(ran);
            }
            if let Some(next) = self.pick_at_terminator().map_err(raised)? {
                return Ok(Ran::Switch(Some(next)));
            }
        }
    }

    /// Runs `terminator` of the block `block` names, which it sets to the block that runs
    /// next when the frame goes on.
    fn exec_terminator(
        &mut self,
        terminator: &'db Terminator,
        frame: &mut StackFrame<'db>,
        block: &mut BasicBlockId,
    ) -> Result<'db, Option<Ran<'db>>> {
        let locals = &mut frame.locals;
        match &terminator.kind {
            TerminatorKind::Assert { cond, expected, msg, target, .. } => {
                let cond = self.eval_operand(cond, locals)?;
                let holds = self.read_scalar(cond)?[0] != 0;
                if holds != *expected
                    && !(msg.is_optional_overflow_check() && !self.checks_overflow(locals))
                {
                    return Err(MirEvalError::Panic(self.assert_message(msg, locals)?));
                }
                *block = *target;
            }
            TerminatorKind::Goto { target } => *block = *target,
            TerminatorKind::Call {
                func,
                args,
                destination,
                target,
                cleanup: _,
                from_hir_call: _,
                intrinsic,
            } => {
                return self.exec_call(
                    func,
                    args,
                    destination,
                    *target,
                    *intrinsic,
                    terminator.span,
                    locals,
                    block,
                );
            }
            TerminatorKind::SwitchInt { discr, targets } => {
                let discr = self.eval_operand(discr, locals)?;
                let val = u128::from_le_bytes(pad16(self.read_scalar(discr)?, IsSigned::No));
                *block = targets.target_for_value(val);
            }
            TerminatorKind::Return => return Ok(Some(Ran::Returned)),
            TerminatorKind::Unreachable => {
                // alloc: error path
                return Err(MirEvalError::UndefinedBehavior("unreachable executed".to_owned()));
            }
            TerminatorKind::Drop { place, target, unwind: _ } => {
                if let Some(callee) =
                    self.drop_place(place, locals, terminator.span, Then::Block(*target))?
                {
                    return Ok(Some(Ran::Called(callee)));
                }
                *block = *target;
            }
            TerminatorKind::UnwindResume => return Ok(Some(Ran::Resume)),
            TerminatorKind::Abort => return Err(unwind::abort(self.unwind_reason(locals.body))),
            TerminatorKind::TailCall { func, args, fn_span: _ } => {
                return self.exec_tail_call(func, args, terminator.span, frame).map(Some);
            }
            _ => not_supported!("unknown terminator"),
        }
        Ok(None)
    }

    /// A `Call` terminator: what it calls runs as a frame of its own, as a shim, or blocks
    /// the running thread (its shim set [`Self::waiting`]).
    fn exec_call(
        &mut self,
        func: &'db Operand,
        args: &'db [Operand],
        destination: &'db StoredPlace,
        target: Option<BasicBlockId>,
        intrinsic: Option<Intrinsic>,
        span: MirSpan,
        locals: &mut Locals<'db>,
        block: &mut BasicBlockId,
    ) -> Result<'db, Option<Ran<'db>>> {
        let destination_interval = self.place_interval(destination, locals)?;
        let fn_ty = self.operand_ty(func, locals)?;
        // From a pool, not one field: a shim may run a nested `interpret_mir` that calls again.
        let mut args_evaluated = self.spare_args.pop().unwrap_or_default();
        for it in args.iter() {
            match self.operand_ty_and_eval(it, locals) {
                Ok(it) => args_evaluated.push(it),
                Err(error) => {
                    args_evaluated.clear();
                    self.spare_args.push(args_evaluated);
                    return Err(error);
                }
            }
        }
        let ran = (|| -> Result<'db, Option<Ran<'db>>> {
            if M::CHECKS {
                self.moved_args.clear();
                for (operand, arg) in args.iter().zip(&args_evaluated) {
                    if matches!(operand.kind, OperandKind::Move(_)) && arg.interval.size != 0 {
                        self.moved_args.push(arg.interval);
                    }
                }
            }
            let then = target.map_or(Then::Diverges, Then::Block);
            self.call_then = then;
            let callee = match (fn_ty.kind(), intrinsic) {
                (TyKind::FnPtr(..), _) => {
                    let bytes = self.eval_operand(func, locals)?;
                    let args = &args_evaluated;
                    self.exec_fn_pointer(bytes, destination_interval, args, locals, then, span)?
                }
                (TyKind::FnDef(def, generic_args), Some(intrinsic)) => {
                    let CallableDefId::FunctionId(def) = def.0 else {
                        return Err(MirEvalError::InternalError(
                            "an intrinsic call of a constructor".into(),
                        ));
                    };
                    self.exec_intrinsic_fn(
                        def,
                        intrinsic,
                        &args_evaluated,
                        generic_args.skip_binder(),
                        locals,
                        destination_interval,
                        then,
                        span,
                    )?
                }
                (TyKind::FnDef(def, generic_args), None) => self.exec_fn_def(
                    def.0,
                    generic_args.skip_binder(),
                    destination_interval,
                    &args_evaluated,
                    locals,
                    then,
                    span,
                )?,
                (it, _) => not_supported!("unknown function type {it:?}"),
            };
            self.moved_args.clear();
            self.call_then = Then::Nested;
            if let Some(callee) = callee.or_else(|| self.pending.take()) {
                return Ok(Some(Ran::Called(callee)));
            }
            let Some(target) = target else {
                // The shim wrote a value of an uninhabited type, of any size: as Miri's copy of
                // an intrinsic's result, its validity is what stops it.
                if M::CHECKS && self.waiting.is_none() {
                    let ty = self.place_ty(destination, locals)?;
                    let Interval { addr, size } = destination_interval;
                    self.typed_copy(addr, ty, size, RetagMode::Default)?;
                }
                return Err(MirEvalError::UndefinedBehavior(
                    // alloc: error path
                    "Diverging function returned".to_owned(),
                ));
            };
            *block = target;
            if let Some(wait) = self.waiting.take()
                && self.block_call(wait, destination_interval)?
            {
                return Ok(Some(Ran::Switch(None)));
            }
            // A shim or intrinsic wrote the destination, and the call goes on at its target.
            if M::CHECKS && destination_interval.size != 0 {
                let ty = self.place_ty(destination, locals)?;
                let Interval { addr, size } = destination_interval;
                self.typed_copy(addr, ty, size, RetagMode::Default)?;
            }
            Ok(None)
        })();
        args_evaluated.clear();
        self.spare_args.push(args_evaluated);
        ran
    }

    /// A `TailCall` terminator (`become`), rustc's `InterpCx::init_fn_tail_call`: the callee
    /// and its arguments are read in `frame`, which is then popped (its drops came before the
    /// terminator), and what it calls runs in its place, into its return place and on at its
    /// `then`. The arguments go through a block of the evaluator's own, as `frame`'s stack,
    /// where they were, is where the callee's frame is made. A chain of tail calls so runs in
    /// constant stack, frames and depth.
    fn exec_tail_call(
        &mut self,
        func: &'db Operand,
        args: &'db [Operand],
        span: MirSpan,
        frame: &mut StackFrame<'db>,
    ) -> Result<'db, Ran<'db>> {
        let locals = &mut frame.locals;
        let destination = frame.return_to.unwrap_or(locals.ptr[return_slot()]);
        let fn_ty = self.operand_ty(func, locals)?;
        // A function pointer callee is the block's first word, each argument after it at its
        // alignment.
        let callee_bytes = match fn_ty.kind() {
            TyKind::FnPtr(..) => Some(self.eval_operand(func, locals)?),
            _ => None,
        };
        let (mut size, mut align) = match callee_bytes {
            Some(bytes) => (bytes.size, self.ptr_size()),
            None => (0, 1),
        };
        let mut callee_args: SmallVec<[IntervalAndTy<'db>; 4]> = SmallVec::new();
        let mut offsets: SmallVec<[usize; 4]> = SmallVec::new();
        for it in args.iter() {
            let arg = self.operand_ty_and_eval(it, locals)?;
            let arg_align = self.layout(arg.ty)?.align.abi.bytes() as usize;
            let at = size.next_multiple_of(arg_align);
            size = at + arg.interval.size;
            align = align.max(arg_align);
            callee_args.push(arg);
            offsets.push(at);
        }
        let block = self.allocate(size, align, Origin::Machine, false)?;
        let callee_bytes = match callee_bytes {
            Some(bytes) => {
                self.copy_from_interval(block, bytes)?;
                Some(Interval::new(block, bytes.size))
            }
            None => None,
        };
        for (arg, &at) in callee_args.iter_mut().zip(&offsets) {
            let to = block.offset(at);
            self.copy_from_interval(to, arg.interval)?;
            arg.interval = Interval::new(to, arg.interval.size);
        }

        // The caller's frame is gone: its stack is freed but for its return place, when that
        // is in it (the bottom frame of `interpret_mir`'s), which the callee returns into.
        self.machine.ret()?;
        let mut keep = frame.prev_stack_ptr;
        if destination.size != 0
            && destination.addr.space == Space::Stack
            && let Some(at) = destination.addr.offset.checked_sub(self.stack_base())
            && at >= frame.prev_stack_ptr
        {
            keep = at + destination.size;
        }
        self.truncate_stack(keep)?;

        // `Then::Diverges` only stands in for the caller's `then` while the callee's frame is
        // made: with `Then::Nested` (the caller is a bottom frame) the call would run nested,
        // and a chain of tail calls would grow the evaluator's own stack.
        let stand_in = Then::Diverges;
        self.moved_args.clear();
        self.call_then = stand_in;
        let called = match (fn_ty.kind(), callee_bytes) {
            (TyKind::FnPtr(..), Some(bytes)) => {
                let locals = &frame.locals;
                self.exec_fn_pointer(bytes, destination, &callee_args, locals, stand_in, span)
            }
            (TyKind::FnDef(def, generic_args), _) => self.exec_fn_def(
                def.0,
                generic_args.skip_binder(),
                destination,
                &callee_args,
                &frame.locals,
                stand_in,
                span,
            ),
            // alloc: error path
            (it, _) => Err(MirEvalError::NotSupported(format!("tail call of a {it:?}"))),
        };
        self.call_then = Then::Nested;
        let called = called.map(|it| it.or_else(|| self.pending.take()));
        if size != 0 {
            self.deallocate(block, Origin::Machine, "become")?;
        }
        let Some(mut callee) = called? else {
            if self.waiting.is_some() {
                not_supported!("a tail call of a function that blocks its thread");
            }
            return Ok(Ran::TailReturned);
        };
        if !callee.drops.is_empty() || callee.catch.is_some() {
            not_supported!("a tail call of a drop or of `catch_unwind`");
        }
        callee.then = frame.then;
        callee.prev_stack_ptr = frame.prev_stack_ptr;
        callee.drops = mem::take(&mut frame.drops);
        callee.catch = frame.catch.take();
        Ok(Ran::TailCalled(callee))
    }

    /// `frame` returned, the machine told; `block` is set to where its caller goes on. The
    /// result of the run when `frame` is the bottom one of [`Self::interpret_mir`]. `filled`:
    /// a shim the frame tail-called wrote the result into the frame's destination, so its own
    /// return slot holds nothing.
    fn returned(
        &mut self,
        frame: StackFrame<'db>,
        block: &mut BasicBlockId,
        filled: bool,
    ) -> Result<'db, Option<Interval>> {
        let mut return_interval = frame.locals.ptr[return_slot()];
        let returned_ty = frame.locals.body.locals[return_slot()].ty.as_ref();
        if let Some(to) = frame.return_to {
            // After `machine.ret`, as Miri's `return_from_current_stack_frame`: the place was
            // protected for the call, and the copy into it is the caller's own write.
            if !filled && to.size != 0 {
                self.copy_from_interval(to.addr, return_interval)?;
            }
            return_interval = to;
        }
        let StackFrame { locals, then, drops, prev_stack_ptr, .. } = frame;
        self.unused_locals_store.borrow_mut().entry(locals.body.owner).or_default().push(locals);
        if then == Then::Nested {
            return Ok(Some(return_interval));
        }
        if self.limits_frames {
            self.stack_depth_limit += 1;
        }
        self.free_stack(prev_stack_ptr)?;
        match then {
            Then::Block(target) => {
                // The call's destination, now complete.
                if M::CHECKS && return_interval.size != 0 {
                    let Interval { addr, size } = return_interval;
                    self.typed_copy(addr, returned_ty, size, RetagMode::Default)?;
                }
                *block = target;
                self.finish_drops(drops, then, block)?;
            }
            Then::Diverges => {
                return Err(MirEvalError::UndefinedBehavior(
                    // alloc: error path
                    "Diverging function returned".to_owned(),
                ));
            }
            Then::Thread => *block = self.thread_returned(return_interval)?,
            Then::Resume => {
                if !self.finish_drops(drops, then, block)? {
                    let Some(caller) = self.code_stack.pop() else {
                        not_supported!("unwinding with no frame to resume");
                    };
                    self.unwind_from(caller, block)?;
                }
            }
            Then::Nested => unreachable!("handled above"),
        }
        Ok(None)
    }

    /// A frame is pushed above the running one: in a constant's evaluation it counts toward
    /// the recursion limit, given back as it returns or unwinds.
    fn count_frame(&mut self) -> Result<'db, ()> {
        if self.limits_frames {
            let Some(it) = self.stack_depth_limit.checked_sub(1) else {
                return Err(MirEvalError::StackOverflow);
            };
            self.stack_depth_limit = it;
        }
        Ok(())
    }

    /// The rest of a drop a frame that returned was a step of, in its caller (the top of
    /// [`Self::code_stack`]): the next `Drop` impl it calls becomes a frame going on at
    /// `then`, with `block` set to where it starts. Whether such a frame was made.
    fn finish_drops(
        &mut self,
        drops: Vec<DropStep<'db>>,
        then: Then,
        block: &mut BasicBlockId,
    ) -> Result<'db, bool> {
        if drops.is_empty() {
            self.keep_drops(drops);
            return Ok(false);
        }
        let Some(mut caller) = self.code_stack.pop() else {
            not_supported!("a drop with no frame to finish it in");
        };
        let span = caller.span.0;
        let callee = self.drop_steps(drops, &caller.locals, span, then)?;
        let made = callee.is_some();
        if let Some(callee) = callee {
            self.count_frame()?;
            caller.at = *block;
            *block = callee.locals.body.start_block;
            self.code_stack.push(caller);
            self.code_stack.push(callee);
        } else {
            self.code_stack.push(caller);
        }
        Ok(made)
    }

    fn exec_statement(
        &mut self,
        statement: &'db Statement,
        locals: &mut Locals<'db>,
    ) -> Result<'db, ()> {
        if M::CHECKS {
            self.machine.step(Site { owner: locals.body.owner, span: statement.span });
        }
        match &statement.kind {
            StatementKind::Assign(l, r) => {
                let addr = self.place_addr(l, locals)?;
                let result = match self.eval_rvalue(r, locals) {
                    Ok(it) => it,
                    // A refusal of `#![feature(blaze_comptime)]` in an rvalue is at its statement.
                    Err(MirEvalError::Comptime(mut error)) if error.at.is_none() => {
                        error.at = Some((locals.body.owner, statement.span));
                        return Err(MirEvalError::Comptime(error));
                    }
                    Err(error) => return Err(error),
                };
                self.copy_from_interval_or_owned(addr, result)?;
                if M::CHECKS {
                    self.typed_assign(l, addr, r, locals)?;
                }
            }
            StatementKind::Deinit(_) => not_supported!("de-init statement"),
            StatementKind::InlineAsm(_) => not_supported!("inline assembly"),
            &StatementKind::StorageLive(local) => self.set_storage(local, locals, true)?,
            &StatementKind::StorageDead(local) => self.set_storage(local, locals, false)?,
            StatementKind::FakeRead(_) | StatementKind::Nop => (),
        }
        Ok(())
    }

    /// The typed copy an `Assign` of `r` to `place` (at `addr`) is: the machine checks the
    /// value at the place's type and retags what it holds, by the mode of the notes' table
    /// (`docs/v2/plan/design/unsafe.md`, "Retags without Retag statements").
    fn typed_assign(
        &self,
        place: &StoredPlace,
        addr: Address,
        r: &Rvalue,
        locals: &Locals<'db>,
    ) -> Result<'db, ()> {
        let ty = self.place_ty(place, locals)?;
        let mode = match r {
            // `&raw` is lowered as a `Ref` to a raw pointer.
            Rvalue::Ref(..) if matches!(ty.kind(), TyKind::RawPtr(..)) => RetagMode::Raw,
            Rvalue::Ref(
                super::BorrowKind::Mut { kind: super::MutBorrowKind::TwoPhasedBorrow },
                _,
            ) => RetagMode::TwoPhase,
            // A static's operand is typed `&'static T` here, but rustc's (`static_ptr_ty`) is a
            // raw pointer for a `static mut` or a foreign static, which `AddRetag` leaves alone:
            // a shared retag would freeze what the program writes through it.
            Rvalue::Use(Operand { kind: OperandKind::Static(s), .. })
                if StaticSignature::of(self.db, *s)
                    .flags
                    .intersects(StaticFlags::MUTABLE | StaticFlags::EXTERN) =>
            {
                RetagMode::None
            }
            _ => RetagMode::Default,
        };
        let size = self.size_of_sized(ty, locals, "the place of an assignment")?;
        self.typed_copy(addr, ty, size, mode)
    }

    /// The machine's check of the value of `ty` (of `size` bytes) just copied to `addr`, and
    /// the retag of what it holds by `mode`.
    fn typed_copy(
        &self,
        addr: Address,
        ty: Ty<'db>,
        size: usize,
        mode: RetagMode,
    ) -> Result<'db, ()> {
        let bytes = self.peek_memory(addr, size)?;
        let cx = TypedCx {
            db: self.db,
            env: self.param_env,
            target: self.target_data_layout,
            vtables: &self.vtable_map,
        };
        self.machine.typed(&cx, self.ptr(addr), ty, bytes, mode)
    }

    /// Gives back the stack above `len`, of the frames that returned: nothing points there
    /// any more but what a dangling pointer would, as the temporaries whose borrows outlive
    /// a call are in the heap ([`Self::promoted`]).
    fn free_stack(&mut self, len: usize) -> Result<'db, ()> {
        self.truncate_stack(len)
    }

    fn truncate_stack(&mut self, len: usize) -> Result<'db, ()> {
        let end = Address::stack(self.stack_base() + len).to_usize(self.cached_ptr_size);
        let kept = self.stack_allocations.partition_point(|it| it.start < end);
        if M::CHECKS {
            for allocated in self.stack_allocations[kept..].iter().filter(|it| it.tracked) {
                let addr = allocated.start as u64;
                let ptr = Ptr { addr, prov: self.machine.root(addr) };
                let (size, align) = (Some(allocated.size as u64), Some(allocated.align as u64));
                self.machine.free(ptr, size, align, MemKind::Stack)?;
            }
        }
        self.stack.truncate(len);
        self.stack_allocations.truncate(kept);
        Ok(())
    }

    /// `StorageLive` (`live`) or `StorageDead` of `local`: the machine allocates it anew, or
    /// frees it.
    fn set_storage(
        &mut self,
        local: LocalId,
        locals: &mut Locals<'db>,
        live: bool,
    ) -> Result<'db, ()> {
        let interval = locals.ptr[local];
        if !M::CHECKS || interval.size == 0 || interval.addr.space != Space::Stack {
            return Ok(());
        }
        let start = interval.addr.to_usize(self.cached_ptr_size);
        let i = self.stack_allocations.partition_point(|it| it.start < start);
        let Some(allocated) = self.stack_allocations.get(i).filter(|it| it.start == start).copied()
        else {
            return Ok(());
        };
        let (size, align) = (allocated.size as u64, allocated.align as u64);
        if allocated.tracked {
            let ptr = self.ptr(interval.addr);
            self.machine.free(ptr, Some(size), Some(align), MemKind::Stack)?;
        }
        let prov = match live {
            true => Some(self.machine.alloc(start as u64, size, align, MemKind::Stack, false)?),
            false => None,
        };
        self.stack_allocations[i].tracked = live;
        locals
            .ptr
            .insert(local, Interval { addr: self.with_prov(interval.addr, prov), ..interval });
        Ok(())
    }

    /// The blocks of `body` that are steps of `long_running_const_eval`, made once per body.
    fn counted_blocks(&mut self, body: &'db MirBody<'db>) -> Arc<[u64]> {
        let key = std::ptr::from_ref(body) as usize;
        // clone: an `Arc`
        self.counted_blocks
            .entry(key)
            // alloc: once per body an evaluation runs, reused for each of its calls
            .or_insert_with(|| super::ctfe_limit::counted_blocks(body).into())
            .clone()
    }

    /// The locals of `body` with storage statements, which are live only from a `StorageLive`.
    fn storage_marked(&self, body: &'db MirBody<'db>) -> Arc<HashSet<LocalId, std::alloc::Global>> {
        let key = std::ptr::from_ref(body) as usize;
        if let Some(it) = self.storage_marked.borrow().get(&key) {
            // clone: an `Arc`
            return it.clone();
        }
        let marked: HashSet<LocalId, std::alloc::Global> = body
            .basic_blocks
            .iter()
            .flat_map(|(_, block)| &block.statements)
            .filter_map(|it| match it.kind {
                StatementKind::StorageLive(local) => Some(local),
                _ => None,
            })
            // alloc: once per body an evaluation runs, reused for each of its calls
            .collect();
        // alloc: once per body an evaluation runs, reused for each of its calls
        let marked = Arc::new(marked);
        // clone: an `Arc`
        self.storage_marked.borrow_mut().insert(key, marked.clone());
        marked
    }

    /// A frame of `body` with `locals` starts: the machine sees the call, the protected
    /// places (the `Operand::Move` arguments, and `return_place`, the caller's destination the
    /// frame's `_0` is copied to), and the arguments written to its parameters.
    // Miri: interpret/call.rs, `init_fn_call` protects the return place before the frame, and
    // `pass_argument` an in-place argument's place only after copying it to the parameter:
    // protecting writes it uninitialized through a new tag, which the copy's read through the
    // caller's tag would hit.
    fn enter_frame(
        &mut self,
        body: &'db MirBody<'db>,
        locals: &mut Locals<'db>,
        args: impl Iterator<Item = IntervalOrOwned<M::Shadow>>,
        return_place: Option<Interval>,
    ) -> Result<'db, ()> {
        if M::CHECKS {
            self.machine.call()?;
            if let Some(ret) = return_place
                && ret.size != 0
            {
                self.machine.protect(self.ptr(ret.addr), ret.size as u64)?;
            }
        }
        let moved = mem::take(&mut self.moved_args);
        self.fill_locals_for_body(body, locals, args)?;
        for moved in &moved {
            self.machine.protect(self.ptr(moved.addr), moved.size as u64)?;
        }
        self.moved_args = moved;
        self.moved_args.clear();
        Ok(())
    }

    fn fill_locals_for_body(
        &mut self,
        body: &'db MirBody<'db>,
        locals: &mut Locals<'db>,
        args: impl Iterator<Item = IntervalOrOwned<M::Shadow>>,
    ) -> Result<'db, ()> {
        let mut remain_args = body.param_locals.len();
        for ((local, interval), value) in locals.ptr.iter().skip(1).zip(args) {
            let interval = *interval;
            self.copy_from_interval_or_owned(interval.addr, value)?;
            if M::CHECKS && interval.size != 0 {
                let cx = TypedCx {
                    db: self.db,
                    env: self.param_env,
                    target: self.target_data_layout,
                    vtables: &self.vtable_map,
                };
                let ty = body.locals[local].ty.as_ref();
                let bytes = self.peek_memory(interval.addr, interval.size)?;
                self.machine.typed(&cx, self.ptr(interval.addr), ty, bytes, RetagMode::FnEntry)?;
            }
            if remain_args == 0 {
                return Err(MirEvalError::InternalError("too many arguments".into()));
            }
            remain_args -= 1;
        }
        if remain_args > 0 {
            return Err(MirEvalError::InternalError("too few arguments".into()));
        }
        Ok(())
    }

    /// The locals of a frame called with `destination`. Without machine checks `_0` is the
    /// destination itself; with them it is the frame's own, as in Miri, since the machine
    /// protects the destination for the call and the callee's writes to `_0` are not the
    /// caller's.
    fn callee_locals(
        &mut self,
        body: &'db MirBody<'db>,
        destination: Interval,
    ) -> Result<'db, (Locals<'db>, usize)> {
        self.create_locals_for_body(body, (!M::CHECKS).then_some(destination))
    }

    fn create_locals_for_body(
        &mut self,
        body: &'db MirBody<'db>,
        destination: Option<Interval>,
    ) -> Result<'db, (Locals<'db>, usize)> {
        // A `Drop` drops: what was moved out is already decided by drop elaboration.
        if body.phase != MirPhase::Runtime {
            not_supported!("a body whose drops are not elaborated");
        }
        let mut locals =
            match self.unused_locals_store.borrow_mut().entry(body.owner).or_default().pop() {
                None => Locals { ptr: ArenaMap::new(), body },
                Some(mut l) => {
                    l.body = body;
                    l
                }
            };
        let stack_size = {
            let mut stack_ptr = self.stack.len();
            for (id, it) in body.locals.iter() {
                if id == return_slot()
                    && let Some(destination) = destination
                {
                    locals.ptr.insert(id, destination);
                    continue;
                }
                let (size, align) = self.size_align_of_sized(
                    it.ty.as_ref(),
                    &locals,
                    "no unsized local in extending stack",
                )?;
                if size == 0 {
                    // Nothing is stored, so the address only has to be aligned. It is the
                    // alignment itself, which is what rustc gives a constant of no size.
                    locals.ptr.insert(id, Interval { addr: Address::invalid(align), size });
                    continue;
                }
                if body.promoted_locals.iter().any(|&(it, _)| it == id) {
                    let key = (std::ptr::from_ref(body) as usize, id);
                    let addr = match self.promoted.get(&key) {
                        Some(&addr) => addr,
                        None => {
                            let addr = self.heap_allocate(size, align)?;
                            self.promoted.insert(key, addr);
                            addr
                        }
                    };
                    locals.ptr.insert(id, Interval { addr, size });
                    continue;
                }
                while !stack_ptr.is_multiple_of(align) {
                    stack_ptr += 1;
                }
                let my_ptr = stack_ptr;
                stack_ptr += size;
                let addr = Address::stack(self.stack_base() + my_ptr).aligned(align);
                locals.ptr.insert(id, Interval { addr, size });
                self.stack_allocations.push(Allocated {
                    start: addr.to_usize(self.cached_ptr_size),
                    size,
                    align,
                    tracked: false,
                });
            }
            stack_ptr - self.stack.len()
        };
        let prev_stack_pointer = self.stack.len();
        self.check_memory(stack_size)?;
        self.stack.extend(std::iter::repeat_n(0, stack_size));
        if M::CHECKS {
            // Arguments, the return place and locals without storage statements are live for
            // the whole frame.
            let marked = self.storage_marked(body);
            let frame_start = Address::stack(self.stack_base() + prev_stack_pointer);
            // In index order, as `locals.ptr` is, without borrowing it across `set_storage`.
            for (id, _) in body.locals.iter() {
                let Some(at) = locals.ptr.get(id) else { continue };
                let in_frame = at.addr.to_usize(self.cached_ptr_size)
                    >= frame_start.to_usize(self.cached_ptr_size);
                if !marked.contains(&id) && in_frame {
                    self.set_storage(id, &mut locals, true)?;
                }
            }
        }
        Ok((locals, prev_stack_pointer))
    }

    fn eval_rvalue(
        &mut self,
        r: &Rvalue,
        locals: &mut Locals<'db>,
    ) -> Result<'db, IntervalOrOwned<M::Shadow>> {
        use IntervalOrOwned::*;
        Ok(match r {
            Rvalue::Use(it) => Borrowed(self.eval_operand(it, locals)?),
            Rvalue::Ref(_, p) => {
                let (addr, _, metadata) = self.place_addr_and_ty_and_metadata(p, locals)?;
                self.pointer_value(addr, metadata.as_ref())?
            }
            Rvalue::Len(p) => {
                let (_, _, metadata) = self.place_addr_and_ty_and_metadata(p, locals)?;
                match metadata {
                    Some(m) => {
                        if M::CHECKS {
                            self.scalar(&m)?;
                        }
                        m
                    }
                    None => {
                        return Err(MirEvalError::InternalError(
                            "type without metadata is used for Rvalue::Len".into(),
                        ));
                    }
                }
            }
            Rvalue::UnaryOp(op, val) => {
                let mut value = self.eval_operand(val, locals)?;
                let mut ty = self.operand_ty(val, locals)?;
                while let TyKind::Ref(_, z, _) = ty.kind() {
                    ty = z;
                    let (size, align) =
                        self.size_align_of_sized(ty, locals, "operand of unary op")?;
                    value = Interval::new(self.read_pointer(value.addr)?.aligned(align), size);
                }
                let c = self.read_scalar(value)?;
                if let TyKind::Float(f) = ty.kind() {
                    match f {
                        rustc_type_ir::FloatTy::F16 => {
                            let c = -from_bytes!(f16, u16, c);
                            let bits = u16::try_from(c.to_bits()).unwrap();
                            Owned(OwnedBytes::from_slice(&bits.to_le_bytes()))
                        }
                        rustc_type_ir::FloatTy::F32 => {
                            let c = -from_bytes!(f32, c);
                            Owned(OwnedBytes::from_slice(&c.to_le_bytes()))
                        }
                        rustc_type_ir::FloatTy::F64 => {
                            let c = -from_bytes!(f64, c);
                            Owned(OwnedBytes::from_slice(&c.to_le_bytes()))
                        }
                        rustc_type_ir::FloatTy::F128 => {
                            let c = -from_bytes!(f128, u128, c);
                            Owned(OwnedBytes::from_slice(&c.to_bits().to_le_bytes()))
                        }
                    }
                } else if let Some(n) = Narrow::of(ty) {
                    // `-iN::MIN` wraps here; a check before it is an `Assert` (`OverflowNeg`).
                    let value = n.read(c)?;
                    let value = match op {
                        UnOp::Not => n.not(value),
                        UnOp::Neg => n.neg(value).0,
                    };
                    Owned(Narrow::bytes(value, c.len()))
                } else {
                    let mut c = OwnedBytes::from_slice(c);
                    if matches!(ty.kind(), TyKind::Bool) {
                        c[0] = 1 - c[0];
                    } else {
                        match op {
                            UnOp::Not => c.iter_mut().for_each(|it| *it = !*it),
                            UnOp::Neg => {
                                c.iter_mut().for_each(|it| *it = !*it);
                                for k in c.iter_mut() {
                                    let o;
                                    (*k, o) = k.overflowing_add(1);
                                    if !o {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Owned(c)
                }
            }
            Rvalue::CheckedBinaryOp(op, lhs, rhs) | Rvalue::BinaryOp(op, lhs, rhs) => 'binary_op: {
                // In runtime MIR the checks are `Assert`s before the operation, which wraps.
                let checked = matches!(r, Rvalue::CheckedBinaryOp(..));
                let mut lv = self.eval_operand(lhs, locals)?;
                let mut rv = self.eval_operand(rhs, locals)?;
                let mut ty = self.operand_ty(lhs, locals)?;
                while let TyKind::Ref(_, z, _) = ty.kind() {
                    ty = z;
                    // A string or byte string pattern: the lengths, then the bytes. Elements
                    // without padding are equal exactly when their bytes are.
                    let element = match ty.kind() {
                        TyKind::Str => Some((1, 1)),
                        TyKind::Slice(element)
                            if matches!(
                                element.kind(),
                                TyKind::Int(_) | TyKind::Uint(_) | TyKind::Bool | TyKind::Char
                            ) =>
                        {
                            Some(self.size_align_of_sized(element, locals, "element of a slice")?)
                        }
                        _ => None,
                    };
                    let size = if let Some((element, element_align)) = element {
                        if *op != BinOp::Eq {
                            return Err(MirEvalError::NotSupported(
                                // alloc: error path
                                "Only eq is builtin for `str` and slices".to_owned(),
                            ));
                        }
                        let ps = self.ptr_size();
                        let ls = from_bytes!(
                            usize,
                            self.cached_ptr_size,
                            self.read_scalar(lv.slice(ps..ps * 2))?
                        );
                        let rs = from_bytes!(
                            usize,
                            self.cached_ptr_size,
                            self.read_scalar(rv.slice(ps..ps * 2))?
                        );
                        if ls != rs {
                            break 'binary_op Owned(smallvec![0]); // alloc: inline `OwnedBytes`, no heap
                        }
                        let la = self.read_pointer(lv.addr)?.aligned(element_align);
                        let ra = self.read_pointer(rv.addr)?.aligned(element_align);
                        let lc = self.read_scalar(Interval::new(la, ls * element))?;
                        let rc = self.read_scalar(Interval::new(ra, ls * element))?;
                        break 'binary_op Owned(smallvec![u8::from(lc == rc)]); // alloc: inline `OwnedBytes`, no heap
                    } else {
                        self.size_align_of_sized(ty, locals, "operand of binary op")?
                    };
                    let (size, align) = size;
                    lv = Interval::new(self.read_pointer(lv.addr)?.aligned(align), size);
                    rv = Interval::new(self.read_pointer(rv.addr)?.aligned(align), size);
                }
                let lc = self.read_scalar(lv)?;
                let rc = self.read_scalar(rv)?;
                if let TyKind::Float(f) = ty.kind() {
                    match f {
                        rustc_type_ir::FloatTy::F16 => {
                            let l = from_bytes!(f16, u16, lc);
                            let r = from_bytes!(f16, u16, rc);
                            match op {
                                BinOp::Ge
                                | BinOp::Gt
                                | BinOp::Le
                                | BinOp::Lt
                                | BinOp::Eq
                                | BinOp::Ne => {
                                    let r = op.run_compare(l, r) as u8;
                                    Owned(smallvec![r]) // alloc: inline `OwnedBytes`, no heap
                                }
                                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                                    let r = match op {
                                        BinOp::Add => l + r,
                                        BinOp::Sub => l - r,
                                        BinOp::Mul => l * r,
                                        BinOp::Div => l / r,
                                        BinOp::Rem => l % r,
                                        _ => unreachable!(),
                                    };
                                    Owned(OwnedBytes::from_slice(
                                        &u16::try_from(r.value.to_bits()).unwrap().to_le_bytes(),
                                    ))
                                }
                                it => not_supported!(
                                    "invalid binop {it:?} on floating point operators"
                                ),
                            }
                        }
                        rustc_type_ir::FloatTy::F32 => {
                            let l = from_bytes!(f32, lc);
                            let r = from_bytes!(f32, rc);
                            match op {
                                BinOp::Ge
                                | BinOp::Gt
                                | BinOp::Le
                                | BinOp::Lt
                                | BinOp::Eq
                                | BinOp::Ne => {
                                    let r = op.run_compare(l, r) as u8;
                                    Owned(smallvec![r]) // alloc: inline `OwnedBytes`, no heap
                                }
                                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                                    let r = match op {
                                        BinOp::Add => l + r,
                                        BinOp::Sub => l - r,
                                        BinOp::Mul => l * r,
                                        BinOp::Div => l / r,
                                        BinOp::Rem => l % r,
                                        _ => unreachable!(),
                                    };
                                    // rustc's const eval makes every NaN it computes the
                                    // positive quiet one, where the host's can be negative.
                                    let r = if r.is_nan() { f32::NAN } else { r };
                                    Owned(OwnedBytes::from_slice(&r.to_le_bytes()))
                                }
                                it => not_supported!(
                                    "invalid binop {it:?} on floating point operators"
                                ),
                            }
                        }
                        rustc_type_ir::FloatTy::F64 => {
                            let l = from_bytes!(f64, lc);
                            let r = from_bytes!(f64, rc);
                            match op {
                                BinOp::Ge
                                | BinOp::Gt
                                | BinOp::Le
                                | BinOp::Lt
                                | BinOp::Eq
                                | BinOp::Ne => {
                                    let r = op.run_compare(l, r) as u8;
                                    Owned(smallvec![r]) // alloc: inline `OwnedBytes`, no heap
                                }
                                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                                    let r = match op {
                                        BinOp::Add => l + r,
                                        BinOp::Sub => l - r,
                                        BinOp::Mul => l * r,
                                        BinOp::Div => l / r,
                                        BinOp::Rem => l % r,
                                        _ => unreachable!(),
                                    };
                                    // rustc's const eval makes every NaN it computes the
                                    // positive quiet one, where the host's can be negative.
                                    let r = if r.is_nan() { f64::NAN } else { r };
                                    Owned(OwnedBytes::from_slice(&r.to_le_bytes()))
                                }
                                it => not_supported!(
                                    "invalid binop {it:?} on floating point operators"
                                ),
                            }
                        }
                        rustc_type_ir::FloatTy::F128 => {
                            let l = from_bytes!(f128, u128, lc);
                            let r = from_bytes!(f128, u128, rc);
                            match op {
                                BinOp::Ge
                                | BinOp::Gt
                                | BinOp::Le
                                | BinOp::Lt
                                | BinOp::Eq
                                | BinOp::Ne => {
                                    let r = op.run_compare(l, r) as u8;
                                    Owned(smallvec![r]) // alloc: inline `OwnedBytes`, no heap
                                }
                                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                                    let r = match op {
                                        BinOp::Add => l + r,
                                        BinOp::Sub => l - r,
                                        BinOp::Mul => l * r,
                                        BinOp::Div => l / r,
                                        BinOp::Rem => l % r,
                                        _ => unreachable!(),
                                    };
                                    Owned(OwnedBytes::from_slice(&r.value.to_bits().to_le_bytes()))
                                }
                                it => not_supported!(
                                    "invalid binop {it:?} on floating point operators"
                                ),
                            }
                        }
                    }
                } else if let Some(n) = Narrow::of(ty)
                    && !matches!(
                        op,
                        BinOp::Ge | BinOp::Gt | BinOp::Le | BinOp::Lt | BinOp::Eq | BinOp::Ne
                    )
                {
                    let mut rhs_ty = self.operand_ty(rhs, locals)?;
                    while let TyKind::Ref(_, inner, _) = rhs_ty.kind() {
                        rhs_ty = inner;
                    }
                    Owned(self.narrow_binary_op(n, op, checked, (lc, ty), (rc, rhs_ty), locals)?)
                } else {
                    let is_signed = matches!(ty.kind(), TyKind::Int(_));
                    let l128 = IntValue::from_bytes(lc, is_signed)?;
                    let r128 = IntValue::from_bytes(rc, is_signed)?;
                    match op {
                        BinOp::Ge | BinOp::Gt | BinOp::Le | BinOp::Lt | BinOp::Eq | BinOp::Ne => {
                            let r = op.run_compare(l128, r128) as u8;
                            Owned(smallvec![r]) // alloc: inline `OwnedBytes`, no heap
                        }
                        BinOp::BitAnd
                        | BinOp::BitOr
                        | BinOp::BitXor
                        | BinOp::Add
                        | BinOp::Mul
                        | BinOp::Div
                        | BinOp::Rem
                        | BinOp::Sub => {
                            let wraps = !checked || !self.checks_overflow(locals);
                            let r = match op {
                                BinOp::Add if wraps => l128.wrapping_add(r128),
                                BinOp::Mul if wraps => l128.wrapping_mul(r128),
                                BinOp::Sub if wraps => l128.wrapping_sub(r128),
                                BinOp::Add => l128.checked_add(r128).ok_or_else(|| {
                                    // alloc: error path
                                    MirEvalError::Panic(format!("Overflow in {op:?}"))
                                })?,
                                BinOp::Mul => l128.checked_mul(r128).ok_or_else(|| {
                                    // alloc: error path
                                    MirEvalError::Panic(format!("Overflow in {op:?}"))
                                })?,
                                BinOp::Div => l128.checked_div(r128).ok_or_else(|| {
                                    // alloc: error path
                                    MirEvalError::Panic(format!("Overflow in {op:?}"))
                                })?,
                                BinOp::Rem => l128.checked_rem(r128).ok_or_else(|| {
                                    // alloc: error path
                                    MirEvalError::Panic(format!("Overflow in {op:?}"))
                                })?,
                                BinOp::Sub => l128.checked_sub(r128).ok_or_else(|| {
                                    // alloc: error path
                                    MirEvalError::Panic(format!("Overflow in {op:?}"))
                                })?,
                                BinOp::BitAnd => l128 & r128,
                                BinOp::BitOr => l128 | r128,
                                BinOp::BitXor => l128 ^ r128,
                                _ => unreachable!(),
                            };
                            Owned(r.to_bytes())
                        }
                        BinOp::Shl | BinOp::Shr => {
                            let r = 'b: {
                                let bits = lc.len() as u32 * 8;
                                // An arbitrary-width amount is read at its own width
                                // (`narrow::shift_amount`): `-1i3` is 7.
                                let rhs_ty = self.operand_ty(rhs, locals)?;
                                let narrow_amount =
                                    Narrow::of(rhs_ty).map(|_| narrow::shift_amount(rc, rhs_ty));
                                let amount = if checked && self.checks_overflow(locals) {
                                    match narrow_amount {
                                        Some(amount) => u32::try_from(amount).ok(),
                                        None => r128.as_u32(),
                                    }
                                } else {
                                    // An unchecked shift takes the low bits of its amount.
                                    Some(match narrow_amount {
                                        Some(amount) => (amount % u128::from(bits)) as u32,
                                        None => u32::from(r128.to_bytes()[0]) % bits,
                                    })
                                };
                                if let Some(shift_amount) = amount {
                                    let r = match op {
                                        BinOp::Shl => l128.checked_shl(shift_amount),
                                        BinOp::Shr => l128.checked_shr(shift_amount),
                                        _ => unreachable!(),
                                    };
                                    if shift_amount as usize >= lc.len() * 8 {
                                        // alloc: error path
                                        return Err(MirEvalError::Panic(format!(
                                            "Overflow in {op:?}"
                                        )));
                                    }
                                    if let Some(r) = r {
                                        break 'b r;
                                    }
                                };
                                // alloc: error path
                                return Err(MirEvalError::Panic(format!("Overflow in {op:?}")));
                            };
                            Owned(r.to_bytes())
                        }
                        BinOp::AddWithOverflow
                        | BinOp::SubWithOverflow
                        | BinOp::MulWithOverflow => {
                            let (value, overflowed) = match op {
                                BinOp::AddWithOverflow => {
                                    (l128.wrapping_add(r128), l128.checked_add(r128).is_none())
                                }
                                BinOp::SubWithOverflow => {
                                    (l128.wrapping_sub(r128), l128.checked_sub(r128).is_none())
                                }
                                _ => (l128.wrapping_mul(r128), l128.checked_mul(r128).is_none()),
                            };
                            // `(T, bool)`: the value, the flag right after it, and padding to
                            // `T`'s alignment, which the target sets (`u128`'s is 8 or 16).
                            let align = self.layout(ty)?.align.abi.bytes() as usize;
                            let mut bytes = value.to_bytes();
                            bytes.push(u8::from(overflowed));
                            bytes.resize(bytes.len().next_multiple_of(align), 0);
                            Owned(bytes)
                        }
                        BinOp::Offset => not_supported!("offset binop"),
                    }
                }
            }
            Rvalue::Discriminant(p) => {
                let ty = self.place_ty(p, locals)?;
                let place = self.eval_place(p, locals)?;
                let result = self.read_discriminant(ty, place)?;
                Owned(OwnedBytes::from_slice(&result.to_le_bytes()))
            }
            Rvalue::Repeat(it, len) => {
                let len = match try_const_usize(self.db, len.as_ref()) {
                    Some(it) => it as usize,
                    None => not_supported!("non evaluatable array len in repeat Rvalue"),
                };
                let value = self.eval_operand(it, locals)?;
                let (val, shadow) = self.read_shadowed(value.addr, value.size)?;
                // `[u8]::repeat` copies by doubling, not byte by byte: a `[0u8; 1 << 20]` is
                // twenty copies, not a million steps of an iterator.
                let bytes = OwnedBytes::from_vec(val.repeat(len)); // alloc: the repeated value, sized once
                if !M::CHECKS {
                    return Ok(Owned(bytes));
                }
                Shadowed(bytes, shadow.repeat(value.size, len))
            }
            Rvalue::CopyForDeref(_) => not_supported!("copy for deref"),
            Rvalue::Aggregate(kind, values) => {
                let values = values
                    .iter()
                    .map(|it| self.eval_operand(it, locals))
                    .collect::<Result<'db, Vec<_>>>()?;
                match kind {
                    AggregateKind::Array(_) => {
                        let size = values.iter().map(|it| it.size).sum();
                        let mut offset = 0;
                        let fields = values.iter().map(|&it| {
                            let at = offset;
                            offset += it.size;
                            (at, it)
                        });
                        self.construct_shadowed(size, None, fields)?
                    }
                    AggregateKind::Tuple(ty) => {
                        let layout = self.layout(ty.as_ref())?;
                        let fields = Self::fields_at(&layout, &values);
                        self.construct_shadowed(layout.size.bytes_usize(), None, fields)?
                    }
                    AggregateKind::Union(it, f) => {
                        let layout = self.layout_adt((*it).into(), GenericArgs::empty())?;
                        let offset = layout
                            .fields
                            .offset(u32::from(f.local_id.into_raw()) as usize)
                            .bytes_usize();
                        let fields = iter::once((offset, values[0]));
                        self.construct_shadowed(layout.size.bytes_usize(), None, fields)?
                    }
                    AggregateKind::Adt(it, subst) => {
                        let (size, variant_layout, tag) =
                            self.layout_of_variant(*it, subst.as_ref(), locals)?;
                        let fields = Self::fields_at(&variant_layout, &values);
                        self.construct_shadowed(size, tag, fields)?
                    }
                    AggregateKind::Closure(ty) => {
                        let layout = self.layout(ty.as_ref())?;
                        let fields = Self::fields_at(&layout, &values);
                        self.construct_shadowed(layout.size.bytes_usize(), None, fields)?
                    }
                }
            }
            Rvalue::Cast(kind, operand, target_ty) => match kind {
                CastKind::PointerCoercion(cast) => match cast {
                    PointerCast::ReifyFnPointer | PointerCast::ClosureFnPointer(_) => {
                        let current_ty = self.operand_ty(operand, locals)?;
                        if let TyKind::FnDef(_, _) | TyKind::Closure(_, _) = current_ty.kind() {
                            let id = self.vtable_map.id(current_ty);
                            let ptr_size = self.ptr_size();
                            let bytes = OwnedBytes::from_slice(&id.to_le_bytes()[0..ptr_size]);
                            match M::CHECKS {
                                true => {
                                    let prov = self.machine.function(id as u64);
                                    Shadowed(bytes, M::Shadow::pointer(ptr_size, prov))
                                }
                                false => Owned(bytes),
                            }
                        } else {
                            not_supported!(
                                "creating a fn pointer from a non FnDef or Closure type"
                            );
                        }
                    }
                    PointerCast::Unsize => {
                        let current_ty = self.operand_ty(operand, locals)?;
                        let addr = self.eval_operand(operand, locals)?;
                        self.coerce_unsized(addr, current_ty, target_ty.as_ref())?
                    }
                    PointerCast::MutToConstPointer | PointerCast::UnsafeFnPointer => {
                        // This is no-op
                        Borrowed(self.eval_operand(operand, locals)?)
                    }
                    PointerCast::ArrayToPointer => {
                        // We should remove the metadata part if the current type is slice
                        Borrowed(self.eval_operand(operand, locals)?.slice(0..self.ptr_size()))
                    }
                },
                CastKind::DynStar => not_supported!("dyn star cast"),
                // A thin pointer of a wide one: its address and provenance, without the metadata.
                CastKind::PtrToPtr => {
                    let value = self.eval_operand(operand, locals)?;
                    let dest_size =
                        self.size_of_sized(target_ty.as_ref(), locals, "destination of ptr cast")?;
                    Borrowed(value.slice(0..dest_size))
                }
                CastKind::IntToInt
                | CastKind::PointerExposeAddress
                | CastKind::PointerFromExposedAddress => {
                    let current_ty = self.operand_ty(operand, locals)?;
                    let is_signed = matches!(current_ty.kind(), TyKind::Int(_)).into();
                    let value = self.eval_operand(operand, locals)?;
                    let dest_size = self.size_of_sized(
                        target_ty.as_ref(),
                        locals,
                        "destination of int to int cast",
                    )?;
                    let (bytes, shadow) = self.read_shadowed(value.addr, value.size)?;
                    // A `const static { .. }` block may not read an address: the evaluator's are
                    // not the program's. An integer cast to a pointer and back has none.
                    if self.comptime
                        && *kind == CastKind::PointerExposeAddress
                        && (!M::CHECKS || shadow.prov_at(0, value.size).is_some())
                    {
                        return Err(comptime_error(
                            ComptimeErrorKind::PointerAddress { how: AddressRead::Cast },
                            None,
                        ));
                    }
                    if M::CHECKS {
                        self.machine.require_init(&shadow, 0..value.size)?;
                        if *kind == CastKind::PointerExposeAddress {
                            let prov = shadow.prov_at(0, value.size);
                            let addr = u128::from_le_bytes(pad16(bytes, IsSigned::No)) as u64;
                            self.machine.expose(Ptr { addr, prov })?;
                        }
                    }
                    let mut bytes = OwnedBytes::from_slice(&pad16(bytes, is_signed)[0..dest_size]);
                    // `as uN` truncates to N bits and extends from them.
                    if let Some(n) = Narrow::of(target_ty.as_ref()) {
                        let value = i128::from_le_bytes(pad16(&bytes, IsSigned::Yes));
                        bytes = Narrow::bytes(n.normalize(value), dest_size);
                    }
                    if M::CHECKS && *kind == CastKind::PointerFromExposedAddress {
                        let addr = u128::from_le_bytes(pad16(&bytes, IsSigned::No)) as u64;
                        let prov = self.machine.from_exposed(addr);
                        return Ok(Shadowed(bytes, M::Shadow::pointer(dest_size, prov)));
                    }
                    Owned(bytes)
                }
                CastKind::FloatToInt => {
                    let ty = self.operand_ty(operand, locals)?;
                    let TyKind::Float(ty) = ty.kind() else {
                        not_supported!("invalid float to int cast");
                    };
                    let value = self.eval_operand(operand, locals)?;
                    let value = self.read_scalar(value)?;
                    let is_signed = matches!(target_ty.as_ref().kind(), TyKind::Int(_));
                    let dest_size = self.size_of_sized(
                        target_ty.as_ref(),
                        locals,
                        "destination of float to int cast",
                    )?;
                    // `as uN` saturates at N bits.
                    let narrow = Narrow::of(target_ty.as_ref());
                    let width = narrow.map_or(dest_size * 8, |it| it.bits() as usize);
                    let soft = match ty {
                        rustc_type_ir::FloatTy::F16 => {
                            Some(apfloat_to_int(from_bytes!(f16, u16, value), width, is_signed))
                        }
                        rustc_type_ir::FloatTy::F128 => {
                            Some(apfloat_to_int(from_bytes!(f128, u128, value), width, is_signed))
                        }
                        _ => None,
                    };
                    if let Some(bits) = soft {
                        return Ok(Owned(OwnedBytes::from_slice(
                            &bits.to_le_bytes()[0..dest_size],
                        )));
                    }
                    let value = match ty {
                        rustc_type_ir::FloatTy::F32 => {
                            let value = value.try_into().unwrap();
                            f32::from_le_bytes(value) as f64
                        }
                        rustc_type_ir::FloatTy::F64 => {
                            let value = value.try_into().unwrap();
                            f64::from_le_bytes(value)
                        }
                        rustc_type_ir::FloatTy::F16 | rustc_type_ir::FloatTy::F128 => {
                            unreachable!("converted above")
                        }
                    };
                    if let Some(n) = narrow {
                        return Ok(Owned(Narrow::bytes(n.saturate(value), dest_size)));
                    }
                    let dest_bits = dest_size * 8;
                    let (max, min) = if dest_bits == 128 {
                        (i128::MAX, i128::MIN)
                    } else if is_signed {
                        let max = 1i128 << (dest_bits - 1);
                        (max - 1, -max)
                    } else {
                        ((1i128 << dest_bits) - 1, 0)
                    };
                    let result = if dest_bits == 128 && !is_signed {
                        (value as u128).to_le_bytes()
                    } else {
                        (value as i128).min(max).max(min).to_le_bytes()
                    };
                    Owned(OwnedBytes::from_slice(&result[0..dest_size]))
                }
                CastKind::FloatToFloat => {
                    let ty = self.operand_ty(operand, locals)?;
                    let TyKind::Float(ty) = ty.kind() else {
                        not_supported!("invalid float to int cast");
                    };
                    let value = self.eval_operand(operand, locals)?;
                    let value = self.read_scalar(value)?;
                    let TyKind::Float(target_ty) = target_ty.as_ref().kind() else {
                        not_supported!("invalid float to float cast");
                    };
                    use rustc_type_ir::FloatTy::{F16, F32, F64, F128};
                    if matches!(ty, F16 | F128) || matches!(target_ty, F16 | F128) {
                        return Ok(Owned(match ty {
                            F16 => convert_apfloat(from_bytes!(f16, u16, value), target_ty),
                            F32 => convert_apfloat(from_bytes!(Single, u32, value), target_ty),
                            F64 => convert_apfloat(from_bytes!(Double, u64, value), target_ty),
                            F128 => convert_apfloat(from_bytes!(f128, u128, value), target_ty),
                        }));
                    }
                    let value = match ty {
                        rustc_type_ir::FloatTy::F32 => {
                            let value = value.try_into().unwrap();
                            f32::from_le_bytes(value) as f64
                        }
                        rustc_type_ir::FloatTy::F64 => {
                            let value = value.try_into().unwrap();
                            f64::from_le_bytes(value)
                        }
                        rustc_type_ir::FloatTy::F16 | rustc_type_ir::FloatTy::F128 => {
                            unreachable!("converted above")
                        }
                    };
                    match target_ty {
                        rustc_type_ir::FloatTy::F32 => {
                            Owned(OwnedBytes::from_slice(&(value as f32).to_le_bytes()))
                        }
                        rustc_type_ir::FloatTy::F64 => {
                            Owned(OwnedBytes::from_slice(&value.to_le_bytes()))
                        }
                        rustc_type_ir::FloatTy::F16 | rustc_type_ir::FloatTy::F128 => {
                            unreachable!("converted above")
                        }
                    }
                }
                CastKind::IntToFloat => {
                    let current_ty = self.operand_ty(operand, locals)?;
                    let is_signed = matches!(current_ty.kind(), TyKind::Int(_)).into();
                    let value = self.eval_operand(operand, locals)?;
                    let value = pad16(self.read_scalar(value)?, is_signed);
                    let value = i128::from_le_bytes(value);
                    let TyKind::Float(target_ty) = target_ty.as_ref().kind() else {
                        not_supported!("invalid int to float cast");
                    };
                    // The padded bytes of a `u128` above `i128::MAX` read as negative.
                    let (single, double) = match current_ty.kind() {
                        TyKind::Int(_) => (value as f32, value as f64),
                        _ => (value as u128 as f32, value as u128 as f64),
                    };
                    // rustc_apfloat rounds to nearest, ties to even, as rustc's evaluation does.
                    fn of<F: Float>(value: i128, signed: bool) -> u128 {
                        let float = match signed {
                            true => F::from_i128(value),
                            false => F::from_u128(value as u128),
                        };
                        float.value.to_bits()
                    }
                    let signed = matches!(current_ty.kind(), TyKind::Int(_));
                    match target_ty {
                        rustc_type_ir::FloatTy::F32 => {
                            Owned(OwnedBytes::from_slice(&single.to_le_bytes()))
                        }
                        rustc_type_ir::FloatTy::F64 => {
                            Owned(OwnedBytes::from_slice(&double.to_le_bytes()))
                        }
                        rustc_type_ir::FloatTy::F16 => {
                            let bits = of::<f16>(value, signed) as u16;
                            Owned(OwnedBytes::from_slice(&bits.to_le_bytes()))
                        }
                        rustc_type_ir::FloatTy::F128 => {
                            Owned(OwnedBytes::from_slice(&of::<f128>(value, signed).to_le_bytes()))
                        }
                    }
                }
                // The pointer keeps the function's id, like the fn pointer it came from.
                CastKind::FnPtrToPtr => Borrowed(self.eval_operand(operand, locals)?),
                // The same bytes as the other type, of the same size, each with its shadow.
                CastKind::Transmute => Borrowed(self.eval_operand(operand, locals)?),
            },
            Rvalue::ThreadLocalRef(n) | Rvalue::AddressOf(n) | Rvalue::NullaryOp(n) => match *n {},
        })
    }

    /// What a failed `Assert` panics with: the words the interpreter has always used for an
    /// overflow, and rustc's for an index out of bounds.
    fn assert_message(
        &mut self,
        msg: &AssertKind,
        locals: &mut Locals<'db>,
    ) -> Result<'db, String> {
        Ok(match msg {
            AssertKind::BoundsCheck { len, index } => {
                let len = {
                    let len = self.eval_operand(len, locals)?;
                    from_bytes!(usize, self.cached_ptr_size, self.read_scalar(len)?)
                };
                let index = {
                    let index = self.eval_operand(index, locals)?;
                    from_bytes!(usize, self.cached_ptr_size, self.read_scalar(index)?)
                };
                // alloc: the panic's message, only when an assert fails
                format!("index out of bounds: the len is {len} but the index is {index}")
            }
            // An arbitrary-width integer's panic names the operation and its type.
            AssertKind::Overflow(op, lhs, _) => match Narrow::of(self.operand_ty(lhs, locals)?) {
                Some(n) => n.overflow(op),
                // alloc: the panic's message, only when an assert fails
                None => format!("Overflow in {op:?}"),
            },
            AssertKind::OverflowNeg(operand) => match Narrow::of(self.operand_ty(operand, locals)?)
            {
                Some(n) => n.neg_overflow(),
                // alloc: the panic's message, only when an assert fails
                None => "Overflow in Neg".to_owned(),
            },
            AssertKind::DivisionByZero(operand) => {
                match Narrow::of(self.operand_ty(operand, locals)?) {
                    Some(n) => n.by_zero(&BinOp::Div),
                    // alloc: the panic's message, only when an assert fails
                    None => "Overflow in Div".to_owned(),
                }
            }
            AssertKind::RemainderByZero(operand) => {
                match Narrow::of(self.operand_ty(operand, locals)?) {
                    Some(n) => n.by_zero(&BinOp::Rem),
                    // alloc: the panic's message, only when an assert fails
                    None => "Overflow in Rem".to_owned(),
                }
            }
            // core's words for the panics of the UB checks (`core::panicking`).
            AssertKind::MisalignedPointerDereference { required, found } => {
                let required = {
                    let required = self.eval_operand(required, locals)?;
                    from_bytes!(usize, self.cached_ptr_size, self.read_scalar(required)?)
                };
                let found = {
                    let found = self.eval_operand(found, locals)?;
                    from_bytes!(usize, self.cached_ptr_size, self.read_scalar(found)?)
                };
                // alloc: the panic's message, only when an assert fails
                format!(
                    "misaligned pointer dereference: address must be a multiple of {required:#x} but is {found:#x}"
                )
            }
            // alloc: the panic's message, only when an assert fails
            AssertKind::NullPointerDereference => "null pointer dereference occurred".to_owned(),
            // alloc: the panic's message, only when an assert fails
            AssertKind::NullReferenceConstructed => "null reference produced".to_owned(),
            AssertKind::InvalidEnumConstruction(source) => {
                let source = {
                    let source = self.eval_operand(source, locals)?;
                    u128::from_le_bytes(pad16(self.read_scalar(source)?, IsSigned::No))
                };
                // alloc: the panic's message, only when an assert fails
                format!("trying to construct an enum from an invalid value {source:#x}")
            }
        })
    }

    fn compute_discriminant(&self, ty: Ty<'db>, bytes: &[u8]) -> Result<'db, i128> {
        let layout = self.layout(ty)?;
        let TyKind::Adt(adt_def, _) = ty.kind() else {
            return Ok(0);
        };
        let AdtId::EnumId(e) = adt_def.def_id() else {
            return Ok(0);
        };
        match &layout.variants {
            Variants::Empty => Err(MirEvalError::UndefinedBehavior(
                // alloc: error path
                "reading the discriminant of an uninhabited enum".to_owned(),
            )),
            Variants::Single { index } => {
                let r =
                    self.const_eval_discriminant(e.enum_variants(self.db).variants[index.0].0)?;
                Ok(r)
            }
            Variants::Multiple { tag, tag_encoding, .. } => {
                let size = tag.size(self.target_data_layout).bytes_usize();
                let offset = layout.fields.offset(0).bytes_usize(); // The only field on enum variants is the tag field
                let is_signed = tag.is_signed().into();
                match tag_encoding {
                    TagEncoding::Direct => {
                        let tag = &bytes[offset..offset + size];
                        Ok(i128::from_le_bytes(pad16(tag, is_signed)))
                    }
                    TagEncoding::Niche { untagged_variant, niche_variants, niche_start } => {
                        let tag = &bytes[offset..offset + size];
                        // A niche value stands for the variant that many after the first of
                        // `niche_variants`, and the untagged one may lie among them.
                        let relative = (u128::from_le_bytes(pad16(tag, IsSigned::No))
                            .wrapping_sub(*niche_start)
                            & (u128::MAX >> (128 - 8 * size)))
                            as usize;
                        let first = niche_variants.start.0;
                        let idx = if relative <= niche_variants.last.0 - first {
                            first + relative
                        } else {
                            untagged_variant.0
                        };
                        let result =
                            self.const_eval_discriminant(e.enum_variants(self.db).variants[idx].0)?;
                        Ok(result)
                    }
                }
            }
        }
    }

    /// The pointees of the pointer a `CoerceUnsized` cast from `current` to `target` widens, as
    /// rustc's `coerce_unsized_info` finds it: through a struct, the one field whose type the
    /// cast changes (`Box<T, A>`'s pointer, not its allocator, which is its last field).
    fn coerce_unsized_pointees(
        &self,
        current: Ty<'db>,
        target: Ty<'db>,
    ) -> Result<'db, (Ty<'db>, Ty<'db>)> {
        match (current.kind(), target.kind()) {
            (
                TyKind::RawPtr(from, _) | TyKind::Ref(_, from, _),
                TyKind::RawPtr(to, _) | TyKind::Ref(_, to, _),
            ) => Ok((from, to)),
            (TyKind::Pat(from, _), TyKind::Pat(to, _)) => self.coerce_unsized_pointees(from, to),
            (TyKind::Adt(current_adt, current_subst), TyKind::Adt(target_adt, target_subst))
                if let AdtId::StructId(id) = current_adt.def_id()
                    && target_adt.def_id() == current_adt.def_id() =>
            {
                // The field with bytes, changed or not: a cast from `Box<dyn A>` to `Box<dyn A>`
                // changes none.
                let fields = id.fields(self.db).fields();
                let mut pointer = None;
                for (field_id, field) in self.db.field_types(id.into()).iter() {
                    let from =
                        field.ty().instantiate(self.interner(), current_subst).skip_norm_wip();
                    let to = field.ty().instantiate(self.interner(), target_subst).skip_norm_wip();
                    // A zero-sized field (`Unique<T>`'s `PhantomData<T>`, which rustc's rule
                    // skips) has no bytes for the cast to change.
                    if self.layout(from)?.size.bytes_usize() == 0 {
                        continue;
                    }
                    let name = fields[field_id].name.as_str();
                    if let Some((first, first_from, first_to)) = pointer.replace((name, from, to)) {
                        // The cast widens the value's bytes as one pointer.
                        not_supported!(
                            "unsizing `{current:?}` to `{target:?}`: two of its fields have bytes, \
                             `{first}` (`{first_from:?}` to `{first_to:?}`) and `{name}` \
                             (`{from:?}` to `{to:?}`), and the cast widens one pointer"
                        );
                    }
                }
                match pointer {
                    Some((_, from, to)) => self.coerce_unsized_pointees(from, to),
                    None => not_supported!(
                        "unsizing `{current:?}` to `{target:?}`: none of its fields has bytes"
                    ),
                }
            }
            _ => Err(MirEvalError::CoerceUnsizedError(current.store())),
        }
    }

    fn coerce_unsized(
        &mut self,
        addr: Interval,
        current_ty: Ty<'db>,
        target_ty: Ty<'db>,
    ) -> Result<'db, IntervalOrOwned<M::Shadow>> {
        let (current_ty, target_ty) = self.coerce_unsized_pointees(current_ty, target_ty)?;
        self.unsizing_ptr_from_addr(target_ty, current_ty, addr)
    }

    /// Adds metadata to the address and create the fat pointer result of the unsizing operation.
    fn unsizing_ptr_from_addr(
        &mut self,
        target_ty: Ty<'db>,
        current_ty: Ty<'db>,
        addr: Interval,
    ) -> Result<'db, IntervalOrOwned<M::Shadow>> {
        Ok(match &target_ty.kind() {
            TyKind::Slice(_) => match &current_ty.kind() {
                TyKind::Array(_, size) => {
                    let len = match try_const_usize(self.db, *size) {
                        None => {
                            not_supported!("unevaluatble len of array in coerce unsized")
                        }
                        Some(it) => it as usize,
                    };
                    self.with_metadata(addr, &word_bytes(len, self.cached_ptr_size))?
                }
                t => {
                    not_supported!("slice unsizing from non array type {t:?}")
                }
            },
            // rustc's `unsize_into_ptr`: between two `dyn` types that are one (regions aside;
            // MIR building makes such casts) the pointer keeps its vtable, unchecked, so a
            // vtable of another trait transmuted in (`&dyn Tr` read as `&dyn Display`) stays
            // as it is for the value's check to refuse. Any other cast (an upcast, dropping
            // an auto trait) first checks that the vtable is one of the source type's trait,
            // generic arguments and projections included (`get_ptr_vtable_ty` with
            // `Some(data_a)`), and refuses it as undefined behavior where it is not, naming
            // the constant; only then does it get the vtable of its concrete type for the
            // target. Never a vtable relabeled unchecked: a `Tr` vtable transmuted to
            // `&dyn Sub` and upcast to `&dyn Display` would come out a valid `Display` one.
            // An id with no `dyn` type is a vtable only where the evaluator made it for a
            // panic's payload (unwind's `&'static str` message, a `dyn Any + Send` by
            // construction); a function's id there is refused as rustc refuses a pointer that
            // is no vtable (`InvalidVTablePointer`).
            TyKind::Dynamic(..) if let TyKind::Dynamic(..) = current_ty.kind() => {
                let size = self.ptr_size();
                if addr.size != 2 * size {
                    not_supported!("upcasting `{current_ty:?}` from a pointer of no metadata")
                }
                let id = from_bytes!(
                    usize,
                    self.cached_ptr_size,
                    self.read_memory(addr.addr.offset(size), size)?
                );
                let Some((concrete, from)) = self.vtable_map.entry(id) else {
                    return Err(MirEvalError::InvalidVTableId(id));
                };
                let interner = self.interner();
                if hir_ty::mir::same_dyn_type(interner, current_ty, target_ty) {
                    IntervalOrOwned::Borrowed(addr)
                } else {
                    match from {
                        Some(from)
                            if hir_ty::mir::same_vtable_trait(interner, from, current_ty) => {}
                        None if !matches!(
                            concrete.kind(),
                            TyKind::FnDef(..) | TyKind::Closure(..)
                        ) => {}
                        _ => {
                            let held = match from {
                                // alloc: error path
                                Some(from) => {
                                    format!("the vtable of `{concrete:?}` for `{from:?}`")
                                }
                                // alloc: error path
                                None => format!("the id of the function `{concrete:?}`, no vtable"),
                            };
                            // alloc: error path
                            return Err(MirEvalError::UndefinedBehavior(format!(
                                "{}: a pointer cast from `{current_ty:?}` to `{target_ty:?}` holds \
                                 {held}, where its type says a vtable of `{current_ty:?}`: rustc refuses \
                                 a vtable of another trait than the pointer's (`InvalidVTableTrait`) as \
                                 undefined behavior",
                                self.evaluated()
                            )));
                        }
                    }
                    let vtable = self.vtable_map.vtable_id(concrete, target_ty);
                    self.with_vtable(Interval::new(addr.addr, size), vtable)?
                }
            }
            TyKind::Dynamic(..) => {
                let vtable = self.vtable_map.vtable_id(current_ty, target_ty);
                self.with_vtable(addr, vtable)?
            }
            TyKind::Adt(adt_def, target_subst) => match &current_ty.kind() {
                TyKind::Adt(current_adt_def, current_subst) => {
                    let id = adt_def.def_id();
                    let current_id = current_adt_def.def_id();
                    if id != current_id {
                        not_supported!("unsizing struct with different type");
                    }
                    let id = match id {
                        AdtId::StructId(s) => s,
                        AdtId::UnionId(_) => not_supported!("unsizing unions"),
                        AdtId::EnumId(_) => not_supported!("unsizing enums"),
                    };
                    let Some((last_field, _)) = id.fields(self.db).fields().iter().next_back()
                    else {
                        not_supported!("unsizing struct without field");
                    };
                    let target_last_field = self.db.field_types(id.into())[last_field]
                        .ty()
                        .instantiate(self.interner(), target_subst)
                        .skip_norm_wip();
                    let current_last_field = self.db.field_types(id.into())[last_field]
                        .ty()
                        .instantiate(self.interner(), current_subst)
                        .skip_norm_wip();
                    return self.unsizing_ptr_from_addr(
                        target_last_field,
                        current_last_field,
                        addr,
                    );
                }
                _ => not_supported!("unsizing struct with non adt type"),
            },
            _ => not_supported!("unknown unsized cast"),
        })
    }

    /// The pointer at `ptr` made wide by `metadata`, keeping what the machine knows of it.
    fn with_metadata(
        &self,
        ptr: Interval,
        metadata: &[u8],
    ) -> Result<'db, IntervalOrOwned<M::Shadow>> {
        let (bytes, mut shadow) = self.read_shadowed(ptr.addr, ptr.size)?;
        let mut r = OwnedBytes::with_capacity(bytes.len() + metadata.len());
        r.extend_from_slice(bytes);
        r.extend_from_slice(metadata);
        if !M::CHECKS {
            return Ok(IntervalOrOwned::Owned(r));
        }
        shadow.put(ptr.size, &M::Shadow::init(metadata.len()));
        Ok(IntervalOrOwned::Shadowed(r, shadow))
    }

    /// The pointer at `ptr` made a `dyn` one by the vtable `id`, which carries the provenance
    /// the machine gives vtables, as rustc's `GlobalAlloc::VTable` pointer does.
    fn with_vtable(&self, ptr: Interval, id: usize) -> Result<'db, IntervalOrOwned<M::Shadow>> {
        let metadata = word_bytes(id, self.cached_ptr_size);
        let (bytes, mut shadow) = self.read_shadowed(ptr.addr, ptr.size)?;
        let mut r = OwnedBytes::with_capacity(bytes.len() + metadata.len());
        r.extend_from_slice(bytes);
        r.extend_from_slice(&metadata);
        if !M::CHECKS {
            return Ok(IntervalOrOwned::Owned(r));
        }
        let prov = self.machine.vtable(id as u64);
        shadow.put(ptr.size, &M::Shadow::pointer(metadata.len(), prov));
        Ok(IntervalOrOwned::Shadowed(r, shadow))
    }

    fn layout_of_variant(
        &mut self,
        it: VariantId,
        subst: GenericArgs<'db>,
        _locals: &Locals<'db>,
    ) -> Result<'db, (usize, VariantLayout, Option<(usize, usize, i128)>)> {
        let adt = it.adt_id(self.db);
        let layout = self.layout_adt(adt, subst)?;
        Ok(match &layout.variants {
            Variants::Single { .. } | Variants::Empty => {
                (layout.size.bytes_usize(), VariantLayout::Shared(layout), None)
            }
            Variants::Multiple { tag, tag_encoding, .. } => {
                let enum_variant_id = match it {
                    VariantId::EnumVariantId(it) => it,
                    _ => not_supported!("multi variant layout for non-enums"),
                };
                let mut discriminant = self.const_eval_discriminant(enum_variant_id)?;
                let rustc_enum_variant_idx = RustcEnumVariantIdx(enum_variant_id.index(self.db));
                let variant_layout = Layout::for_variant(&layout, rustc_enum_variant_idx);
                let have_tag = match tag_encoding {
                    TagEncoding::Direct => true,
                    TagEncoding::Niche { untagged_variant, niche_variants, niche_start } => {
                        if *untagged_variant == rustc_enum_variant_idx {
                            false
                        } else {
                            discriminant = ((rustc_enum_variant_idx.0 - niche_variants.start.0)
                                as i128)
                                .wrapping_add(*niche_start as i128);
                            true
                        }
                    }
                };
                (
                    layout.size.bytes_usize(),
                    VariantLayout::Made(variant_layout),
                    if have_tag {
                        Some((
                            layout.fields.offset(0).bytes_usize(),
                            tag.size(self.target_data_layout).bytes_usize(),
                            discriminant,
                        ))
                    } else {
                        None
                    },
                )
            }
        })
    }

    fn construct_with_layout(
        &mut self,
        size: usize, // Not necessarily equal to variant_layout.size
        variant_layout: &Layout,
        tag: Option<(usize, usize, i128)>,
        values: impl Iterator<Item = IntervalOrOwned>,
    ) -> Result<'db, OwnedBytes> {
        let mut result: OwnedBytes = smallvec![0; size]; // alloc: inline for a value up to 16 bytes
        if let Some((offset, size, value)) = tag {
            match result.get_mut(offset..offset + size) {
                Some(it) => it.copy_from_slice(&value.to_le_bytes()[0..size]),
                None => {
                    return Err(MirEvalError::InternalError(
                        // alloc: error path
                        format!(
                            "encoded tag ({offset}, {size}, {value}) is out of bounds 0..{size}"
                        )
                        .into(),
                    ));
                }
            }
        }
        for (i, op) in values.enumerate() {
            let offset = variant_layout.fields.offset(i).bytes_usize();
            let op = op.get(self)?;
            match result.get_mut(offset..offset + op.len()) {
                Some(it) => it.copy_from_slice(op),
                None => {
                    return Err(MirEvalError::InternalError(
                        // alloc: error path
                        format!("field offset ({offset}) is out of bounds 0..{size}").into(),
                    ));
                }
            }
        }
        Ok(result)
    }

    /// The fields `values` of a value of `layout`, each with its offset.
    fn fields_at<'a>(
        layout: &'a Layout,
        values: &'a [Interval],
    ) -> impl Iterator<Item = (usize, Interval)> + 'a {
        values.iter().enumerate().map(|(i, &it)| (layout.fields.offset(i).bytes_usize(), it))
    }

    /// [`Self::construct_with_layout`] of `fields` given with their offsets, with what the
    /// machine knows of each field's bytes: what no field or tag covers (padding, the rest of
    /// a union) is uninitialized to it.
    fn construct_shadowed(
        &self,
        size: usize,
        tag: Option<(usize, usize, i128)>,
        fields: impl Iterator<Item = (usize, Interval)>,
    ) -> Result<'db, IntervalOrOwned<M::Shadow>> {
        let mut result: OwnedBytes = smallvec![0; size]; // alloc: inline for a value up to 16 bytes
        let mut shadow = M::Shadow::default();
        if let Some((offset, tag_size, value)) = tag {
            match result.get_mut(offset..offset + tag_size) {
                Some(it) => it.copy_from_slice(&value.to_le_bytes()[0..tag_size]),
                None => {
                    return Err(MirEvalError::InternalError(
                        // alloc: error path
                        format!(
                            "encoded tag ({offset}, {tag_size}, {value}) is out of bounds 0..{size}"
                        )
                        .into(),
                    ));
                }
            }
            if M::CHECKS {
                shadow.put(offset, &M::Shadow::init(tag_size));
            }
        }
        for (offset, field) in fields {
            let (bytes, field_shadow) = self.read_shadowed(field.addr, field.size)?;
            match result.get_mut(offset..offset + bytes.len()) {
                Some(it) => it.copy_from_slice(bytes),
                None => {
                    return Err(MirEvalError::InternalError(
                        // alloc: error path
                        format!("field offset ({offset}) is out of bounds 0..{size}").into(),
                    ));
                }
            }
            if M::CHECKS {
                shadow.put(offset, &field_shadow);
            }
        }
        if !M::CHECKS {
            return Ok(IntervalOrOwned::Owned(result));
        }
        // A `put` past the end leaves the bytes it skips uninitialized.
        shadow.put(size, &M::Shadow::init(0));
        Ok(IntervalOrOwned::Shadowed(result, shadow))
    }

    /// The discriminant of the value of `ty` at `place`, whose tag must be initialized.
    fn read_discriminant(&self, ty: Ty<'db>, place: Interval) -> Result<'db, i128> {
        let (bytes, shadow) = self.read_shadowed(place.addr, place.size)?;
        if M::CHECKS {
            let layout = self.layout(ty)?;
            if let Variants::Multiple { tag, .. } = &layout.variants {
                let at = layout.fields.offset(0).bytes_usize();
                let size = tag.size(self.target_data_layout).bytes_usize();
                self.machine.require_init(&shadow, at..at + size)?;
            }
        }
        self.compute_discriminant(ty, bytes)
    }

    fn eval_operand(&mut self, it: &Operand, locals: &mut Locals<'db>) -> Result<'db, Interval> {
        Ok(match &it.kind {
            OperandKind::Copy(p) | OperandKind::Move(p) => self.eval_place(p, locals)?,
            OperandKind::Static(st) => {
                let addr = self.eval_static(*st, locals)?;
                Interval::new(addr, self.ptr_size())
            }
            OperandKind::Constant { konst, ty } => match self.placed(it, ty.as_ref(), locals)? {
                Some(placed) => placed,
                None => {
                    let konst = konst.as_ref();
                    if let Some(&interval) = self.constants.get(&Either::Left(konst)) {
                        return Ok(interval);
                    }
                    let interval = self.allocate_const_in_heap(locals, konst)?;
                    if self.shares_one_copy(konst) {
                        self.constants.insert(Either::Left(konst), interval);
                    }
                    interval
                }
            },
            OperandKind::Allocation { allocation } => {
                let allocation = allocation.as_ref();
                match self.placed(it, allocation.ty, locals)? {
                    Some(placed) => placed,
                    None => match self.constants.get(&Either::Right(allocation)) {
                        Some(&interval) => interval,
                        None => {
                            let (db, owner) = (self.db, locals.body.owner);
                            // alloc: error path
                            let whose = || format!("a constant value in {}", evaluated(db, owner));
                            let interval = self.allocate_allocation_in_heap(
                                locals,
                                allocation,
                                &whose,
                                Mutability::Not,
                            )?;
                            self.constants.insert(Either::Right(allocation), interval);
                            interval
                        }
                    },
                }
            }
        })
    }

    /// The value of a literal that `file!()`, or a `concat!` with it, expanded to: the
    /// expansion has a placeholder for the path, which is the build's rule's.
    fn placed(
        &mut self,
        it: &Operand,
        ty: Ty<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, Option<Interval>> {
        let (Some(MirSpan::ExprId(expr)), InferBodyId::DefWithBodyId(owner)) =
            (it.span, locals.body.owner)
        else {
            return Ok(None);
        };
        let placed = match crate::mir::location::placed(self.db, owner, expr) {
            None => None,
            Some(Ok(text)) => Some(text),
            Some(Err(why)) => not_supported!("{why}"),
        };
        Ok(match (placed, ty.kind()) {
            (Some(text), TyKind::Ref(_, to, _)) if to.is_str() => {
                let at = self.heap_allocate(text.len(), 1)?;
                self.write_memory(at, text.as_bytes())?;
                self.freeze(Interval::new(at, text.len()));
                let size = self.ptr_size();
                let addr = self.heap_allocate(2 * size, size)?;
                self.write_pointer(addr, at)?;
                self.write_memory(
                    addr.offset(size),
                    &word_bytes(text.len(), self.cached_ptr_size),
                )?;
                self.freeze(Interval::new(addr, 2 * size));
                Some(Interval::new(addr, 2 * size))
            }
            _ => None,
        })
    }

    /// The values of `consts`, each in memory of its own.
    fn allocate_valtrees(
        &mut self,
        consts: &[Const<'db>],
        locals: &Locals<'db>,
    ) -> Result<'db, Vec<IntervalOrOwned>> {
        let mut values = Vec::with_capacity(consts.len());
        for konst in consts {
            let ConstKind::Value(value) = konst.kind() else {
                not_supported!("an unevaluated constant in a constant");
            };
            let value = self.allocate_valtree_in_heap(value.ty, value.value, locals)?;
            values.push(IntervalOrOwned::Borrowed(value));
        }
        Ok(values)
    }

    /// A constant's value, read-only as all it points to.
    fn allocate_valtree_in_heap(
        &mut self,
        ty: Ty<'db>,
        valtree: ValTree<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, Interval> {
        let value = self.valtree_in_heap(ty, valtree, locals)?;
        self.freeze(value);
        Ok(value)
    }

    fn valtree_in_heap(
        &mut self,
        ty: Ty<'db>,
        valtree: ValTree<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, Interval> {
        match ty.kind() {
            TyKind::Bool => {
                let value = valtree.inner().to_leaf().try_to_bool().unwrap();
                let addr = self.heap_allocate(1, 1)?;
                self.write_memory(addr, &[u8::from(value)])?;
                Ok(Interval::new(addr, 1))
            }
            TyKind::Char => {
                let value = valtree.inner().to_leaf().to_u32();
                let addr = self.heap_allocate(4, 4)?;
                self.write_memory(addr, &value.to_le_bytes())?;
                Ok(Interval::new(addr, 4))
            }
            TyKind::Int(int_ty) => {
                let size = int_ty
                    .bit_width()
                    .map(Size::from_bits)
                    .unwrap_or_else(|| Size::from_bytes(self.ptr_size() as u64));
                let bytes = size.bytes_usize();

                let value = valtree.inner().to_leaf().to_int(size);
                let addr = self.heap_allocate(bytes, bytes)?;
                self.write_memory(addr, &value.to_le_bytes()[..bytes])?;
                Ok(Interval::new(addr, bytes))
            }
            TyKind::Uint(uint_ty) => {
                let size = uint_ty
                    .bit_width()
                    .map(Size::from_bits)
                    .unwrap_or_else(|| Size::from_bytes(self.ptr_size() as u64));
                let bytes = size.bytes_usize();

                let value = valtree.inner().to_leaf().to_uint(size);
                let addr = self.heap_allocate(bytes, bytes)?;
                self.write_memory(addr, &value.to_le_bytes()[..bytes])?;
                Ok(Interval::new(addr, bytes))
            }
            TyKind::Float(float_ty) => {
                let size = Size::from_bits(float_ty.bit_width());
                let bytes = size.bytes_usize();

                let value = valtree.inner().to_leaf().to_uint(size);
                let addr = self.heap_allocate(bytes, bytes)?;
                self.write_memory(addr, &value.to_le_bytes()[..bytes])?;
                Ok(Interval::new(addr, bytes))
            }
            TyKind::RawPtr(..) => {
                let size = self.ptr_size();
                let value = valtree.inner().to_leaf().to_uint(Size::from_bytes(size));
                let addr = self.heap_allocate(size, size)?;
                self.write_memory(addr, &value.to_le_bytes()[..size])?;
                Ok(Interval::new(addr, size))
            }
            TyKind::Ref(_, inner_ty, _) => match inner_ty.kind() {
                TyKind::Str => {
                    let bytes = valtree
                        .inner()
                        .to_branch()
                        .iter()
                        .map(|konst| match konst.kind() {
                            ConstKind::Value(value) => Ok(value.value.inner().to_leaf().to_u8()),
                            _ => not_supported!("unsupported const"),
                        })
                        .collect::<Result<'_, Vec<_>>>()?;
                    let bytes_addr = self.heap_allocate(bytes.len(), 1)?;
                    self.write_memory(bytes_addr, &bytes)?;
                    self.freeze(Interval::new(bytes_addr, bytes.len()));
                    let ref_addr = self.heap_allocate(self.ptr_size() * 2, self.ptr_size())?;
                    self.write_pointer(ref_addr, bytes_addr)?;
                    let mut len = [0; 16];
                    len[..size_of::<usize>()].copy_from_slice(&bytes.len().to_le_bytes());
                    self.write_memory(ref_addr.offset(self.ptr_size()), &len[..self.ptr_size()])?;
                    Ok(Interval::new(ref_addr, self.ptr_size() * 2))
                }
                TyKind::Slice(inner_ty) => {
                    let item_layout = self.layout(inner_ty)?;
                    let items = valtree
                        .inner()
                        .to_branch()
                        .iter()
                        .map(|konst| match konst.kind() {
                            ConstKind::Value(value) => {
                                self.allocate_valtree_in_heap(value.ty, value.value, locals)
                            }
                            _ => not_supported!("unsupported const"),
                        })
                        .collect::<Result<'_, Vec<_>>>()?;
                    let item_size = item_layout.size.bytes_usize();
                    let items_addr = self.heap_allocate(
                        items.len() * item_size,
                        item_layout.align.bytes() as usize,
                    )?;
                    for (i, item) in items.iter().enumerate() {
                        self.copy_from_interval(items_addr.offset(i * item_size), *item)?;
                    }
                    self.freeze(Interval::new(items_addr, items.len() * item_size));
                    let ref_addr = self.heap_allocate(self.ptr_size() * 2, self.ptr_size())?;
                    self.write_pointer(ref_addr, items_addr)?;
                    let mut len = [0; 16];
                    len[..size_of::<usize>()].copy_from_slice(&items.len().to_le_bytes());
                    self.write_memory(ref_addr.offset(self.ptr_size()), &len[..self.ptr_size()])?;
                    Ok(Interval::new(ref_addr, self.ptr_size() * 2))
                }
                // dyn: error path
                TyKind::Dynamic(..) => not_supported!("`dyn Trait` consts not supported yet"),
                _ => {
                    let inner_addr = self.allocate_valtree_in_heap(inner_ty, valtree, locals)?;
                    let ref_addr = self.heap_allocate(self.ptr_size(), self.ptr_size())?;
                    self.write_pointer(ref_addr, inner_addr.addr)?;
                    Ok(Interval::new(ref_addr, self.ptr_size()))
                }
            },
            TyKind::Adt(def, args) => {
                // The fields in order, after the index of the variant of an enum.
                let items = valtree.inner().to_branch();
                let (variant, fields): (VariantId, _) = match def.def_id() {
                    AdtId::StructId(id) => (id.into(), &items[..]),
                    AdtId::UnionId(_) => not_supported!("a union constant"),
                    AdtId::EnumId(id) => {
                        let index = match items.first().map(|it| it.kind()) {
                            Some(ConstKind::Value(it)) => it.value.inner().to_leaf().to_u32(),
                            _ => not_supported!("an enum constant without its variant"),
                        };
                        let variants = &id.enum_variants(self.db).variants;
                        let Some((_, &(variant, ..))) = variants.get_index(index as usize) else {
                            not_supported!("an enum constant of a variant it does not have");
                        };
                        (variant.into(), &items[1..])
                    }
                };
                let (size, layout, tag) = self.layout_of_variant(variant, args, locals)?;
                let fields = self.allocate_valtrees(fields, locals)?;
                let bytes = self.construct_with_layout(size, &layout, tag, fields.into_iter())?;
                let addr = self.heap_allocate(size, layout.align.abi.bytes() as usize)?;
                self.write_memory(addr, &bytes)?;
                Ok(Interval::new(addr, size))
            }
            TyKind::Array(..) | TyKind::Tuple(_) => {
                let layout = self.layout(ty)?;
                let fields = self.allocate_valtrees(&valtree.inner().to_branch()[..], locals)?;
                let size = layout.size.bytes_usize();
                let bytes = self.construct_with_layout(size, &layout, None, fields.into_iter())?;
                let addr = self.heap_allocate(size, layout.align.abi.bytes() as usize)?;
                self.write_memory(addr, &bytes)?;
                Ok(Interval::new(addr, size))
            }
            TyKind::Pat(_, _)
            | TyKind::Slice(_)
            | TyKind::FnDef(_, _)
            | TyKind::Foreign(_)
            | TyKind::Dynamic(_, _)
            | TyKind::UnsafeBinder(..)
            | TyKind::FnPtr(..)
            | TyKind::Closure(_, _)
            | TyKind::CoroutineClosure(_, _)
            | TyKind::Coroutine(_, _)
            | TyKind::CoroutineWitness(_, _)
            | TyKind::Never
            | TyKind::Alias(..)
            | TyKind::Param(_)
            | TyKind::Bound(..)
            | TyKind::Placeholder(_)
            | TyKind::Infer(_)
            | TyKind::Str
            | TyKind::Error(_) => not_supported!("unsupported const"),
        }
    }

    fn allocate_const_in_heap(
        &mut self,
        locals: &Locals<'db>,
        konst: Const<'db>,
    ) -> Result<'db, Interval> {
        match konst.kind() {
            ConstKind::Value(value) => self.allocate_valtree_in_heap(value.ty, value.value, locals),
            ConstKind::Alias(_, alias) => {
                let subst = alias.args;
                let (allocation, id): (_, GeneralConstId<'db>) = match alias.kind {
                    AliasConstKind::Projection { def_id: TraitAssocConstId(const_id) }
                    | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(const_id) }
                    | AliasConstKind::Free { def_id: FreeConstAliasId(const_id) } => {
                        let (const_id, subst) = lookup_impl_const(
                            &self.infcx,
                            self.param_env.param_env,
                            const_id,
                            subst,
                        );
                        let allocation = self
                            .db
                            .const_eval(const_id, subst, Some(self.param_env))
                            .map_err(|e| {
                                let name = GeneralConstId::ConstId(const_id).name(self.db);
                                // alloc: error path
                                MirEvalError::ConstEvalError(name, Box::new(e))
                            })?;
                        (allocation, const_id.into())
                    }
                    AliasConstKind::Anon { def_id: AnonConstIdWrapper(konst) } => {
                        // A `const static { .. }` block is evaluated by an evaluation of its own,
                        // in comptime mode, as any anonymous constant: never in this one.
                        if konst.is_inline_const(self.db)
                            && !hir_ty::effects::anon_const_is_comptime(self.db, konst)
                        {
                            return self.inline_const_in_heap(konst, subst, locals);
                        }
                        let allocation = self
                            .db
                            .anon_const_eval(konst, subst, Some(self.param_env))
                            .map_err(|e| {
                                let name = GeneralConstId::AnonConstId(konst).name(self.db);
                                // alloc: error path
                                MirEvalError::ConstEvalError(name, Box::new(e))
                            })?;
                        (allocation, konst.into())
                    }
                    AliasConstKind::InherentSelf { .. } => {
                        not_supported!(
                            "the inherent constant {alias:?} in Self form after type check"
                        )
                    }
                };
                let db = self.db;
                let whose = || match id {
                    // alloc: error path
                    GeneralConstId::ConstId(_) => format!("the constant `{}`", id.name(db)),
                    // alloc: error path
                    GeneralConstId::StaticId(_) => format!("the static `{}`", id.name(db)),
                    // alloc: error path
                    GeneralConstId::AnonConstId(_) => "an anonymous constant".to_owned(),
                };
                self.allocate_allocation_in_heap(locals, allocation, &whose, Mutability::Not)
            }
            _ => not_supported!("evaluating unknown const"),
        }
    }

    /// Whether every use of `konst` reads the same copy ([`Self::constants`]): not of a
    /// static, which may be written. An inline `const { .. }` is run by the evaluation
    /// ([`Self::inline_const_in_heap`]) once, as the `TypeId`s it makes are the evaluation's.
    fn shares_one_copy(&self, konst: Const<'db>) -> bool {
        match konst.kind() {
            ConstKind::Value(_) => true,
            ConstKind::Alias(..) => true,
            _ => false,
        }
    }

    /// A `const { .. }` block met while evaluating is run by this evaluation, as one lowered in
    /// place was: the `TypeId`s it makes and compares are this evaluation's (`type_info::of`).
    fn inline_const_in_heap(
        &mut self,
        konst: AnonConstId<'db>,
        subst: GenericArgs<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, Interval> {
        let body = super::optimize::interpreted_body(
            self.db,
            super::MirOwner::Body(konst.into()),
            subst.store(),
            self.param_env.store(),
        )
        .map_err(|e| {
            let name = GeneralConstId::AnonConstId(konst).name(self.db);
            // alloc: error path
            MirEvalError::ConstEvalError(name, Box::new(ConstEvalError::MirLowerError(e)))
        })?;
        let mark = self.stack.len();
        // The block is a constant of its own to `long_running_const_eval`: rustc evaluates an
        // inline const by an evaluation of its own, whose count of steps starts at zero, and
        // what it spends is not spent of the constant that uses it. The budget
        // ([`Self::steps_left`]) is this whole evaluation's and goes on.
        let outer = mem::replace(&mut self.lint_steps, 0);
        let result = self.interpret_mir(body, iter::empty(), None);
        self.lint_steps_inner += mem::replace(&mut self.lint_steps, outer);
        let result = result?;
        let ty = body.locals[return_slot()].ty.as_ref();
        let align = self.size_align_of_sized(ty, locals, "the value of a constant")?.1;
        let addr = self.heap_allocate(result.size, align)?;
        let interval = Interval::new(addr, result.size);
        interval.write_from_interval(self, result)?;
        self.freeze(interval);
        self.free_stack(mark)?;
        Ok(interval)
    }

    /// A copy of a constant's value, read-only with what it points to unless `mutability` is
    /// `Mut` (the value of a `static mut`, whose pointees may be written). `whose` names the
    /// constant for an error.
    fn allocate_allocation_in_heap(
        &mut self,
        locals: &Locals<'db>,
        allocation: Allocation<'db>,
        whose: &dyn Fn() -> String,
        mutability: Mutability,
    ) -> Result<'db, Interval> {
        let (size, align) =
            self.size_align_of_sized(allocation.ty, locals, "the value of a constant")?;
        let addr = self.heap_allocate(size, align)?;
        self.write_allocation(locals, allocation, whose, addr, size, mutability)?;
        let value = Interval::new(addr, size);
        if mutability == Mutability::Not {
            self.freeze(value);
        }
        Ok(value)
    }

    /// Copies a constant's value to `addr`, along with the memory it points to, which is
    /// read-only once written unless `nested` is `Mut` (rustc's interning: only a
    /// `static mut`'s pointees may be written). `whose` names the constant ("the constant
    /// `T`") for an error.
    fn write_allocation(
        &mut self,
        locals: &Locals<'db>,
        allocation: Allocation<'db>,
        whose: &dyn Fn() -> String,
        addr: Address,
        size: usize,
        nested: Mutability,
    ) -> Result<'db, ()> {
        let AllocationData { ty, memory: ref v, ref memory_map } = *allocation;
        // Its ids are of the evaluation that made it, and mean another type here.
        if memory_map.has_type_ids() {
            not_supported!("a constant computed from a `TypeId`, read by another evaluation");
        }
        let mut frozen: SmallVec<[Interval; 4]> = SmallVec::new();
        let freezes = M::CHECKS && nested == Mutability::Not;
        let (db, owner) = (self.db, self.owner);
        let what = || format!("{} (of `{ty:?}`), read by {}", whose(), evaluated(db, owner)); // alloc: error path
        let patch_map = memory_map.transform_addresses(what, |b, align| {
            let addr = self.heap_allocate(b.len(), align)?;
            self.write_memory(addr, b)?;
            if freezes {
                frozen.push(Interval::new(addr, b.len()));
            }
            Ok(addr.to_usize(self.cached_ptr_size))
        })?;
        let padded;
        let v: &[u8] = if size != v.len() {
            // Handle self enum
            if size == 16 && v.len() < 16 {
                padded = pad16(v, IsSigned::No);
                &padded
            } else if size < 16 && v.len() == 16 {
                &v[0..size]
            } else {
                return Err(MirEvalError::InvalidConst);
            }
        } else {
            v
        };
        self.write_memory(addr, v)?;
        let ids = self.ids_by_provenance(memory_map, &patch_map, addr, v, size)?;
        let ptr_size = self.cached_ptr_size;
        self.patch_addresses(
            &mut Relocation::new(),
            &patch_map,
            memory_map,
            |bytes| match memory_map {
                MemoryMap::Empty | MemoryMap::Simple(_) => {
                    Err(MirEvalError::InvalidVTableId(from_bytes!(usize, ptr_size, bytes)))
                }
                MemoryMap::Complex(cm) => cm.vtable.ty_of_bytes(bytes),
            },
            addr,
            ty,
            locals,
        )?;
        self.own_ids(memory_map, ids)?;
        for block in frozen {
            self.freeze(block);
        }
        Ok(())
    }

    fn eval_place(&mut self, p: &StoredPlace, locals: &Locals<'db>) -> Result<'db, Interval> {
        let addr = self.place_addr(p, locals)?;
        Ok(Interval::new(
            addr,
            self.size_of_sized(self.place_ty(p, locals)?, locals, "type of this place")?,
        ))
    }

    /// `addr` as the machine sees it: its address and its provenance.
    #[inline(always)]
    fn ptr(&self, addr: Address) -> Ptr<M::Prov> {
        let prov = match (M::CHECKS, addr.prov.0) {
            (false, _) | (_, 0) => None,
            (true, id) => self.provs.borrow().list.get(id as usize - 1).copied(),
        };
        Ptr { addr: addr.to_usize(self.cached_ptr_size) as u64, prov }
    }

    /// `addr` with the provenance `prov`.
    #[inline(always)]
    fn with_prov(&self, addr: Address, prov: Option<M::Prov>) -> Address {
        let Some(prov) = prov.filter(|_| M::CHECKS) else {
            return Address { prov: ProvId(0), ..addr };
        };
        let mut provs = self.provs.borrow_mut();
        let Provs { list, index } = &mut *provs;
        let id = *index.entry(prov).or_insert_with(|| {
            list.push(prov);
            list.len() as u32
        });
        Address { prov: ProvId(id), ..addr }
    }

    /// The bytes of program memory at `addr` as the program reads them, which the machine
    /// sees: an untyped read unless `addr` says the alignment of its type.
    fn read_memory(&self, addr: Address, size: usize) -> Result<'db, &[u8]> {
        if M::CHECKS && size != 0 {
            self.machine.read(self.ptr(addr), size as u64, addr.align(), Access::Plain)?;
        }
        self.peek_memory(addr, size)
    }

    /// [`Self::read_memory`], with which of the bytes are initialized and what they carry.
    fn read_shadowed(&self, addr: Address, size: usize) -> Result<'db, (&[u8], M::Shadow)> {
        let shadow = match M::CHECKS && size != 0 {
            true => self.machine.read(self.ptr(addr), size as u64, addr.align(), Access::Plain)?,
            false => M::Shadow::default(),
        };
        Ok((self.peek_memory(addr, size)?, shadow))
    }

    /// The bytes of a scalar the program uses (a number, a condition, an address), which must
    /// all be initialized.
    fn read_scalar(&self, interval: Interval) -> Result<'db, &[u8]> {
        if !M::CHECKS {
            return interval.get(self);
        }
        let (bytes, shadow) = self.read_shadowed(interval.addr, interval.size)?;
        self.machine.require_init(&shadow, 0..interval.size)?;
        Ok(bytes)
    }

    /// The pointer stored at `addr`, with the provenance the machine says it carries.
    fn read_pointer(&self, addr: Address) -> Result<'db, Address> {
        let size = self.ptr_size();
        let addr = addr.aligned(size);
        if !M::CHECKS {
            return Address::from_bytes(self.peek_memory(addr, size)?, self.cached_ptr_size);
        }
        let (bytes, shadow) = self.read_shadowed(addr, size)?;
        self.machine.require_init(&shadow, 0..size)?;
        let pointer = Address::from_bytes(bytes, self.cached_ptr_size)?;
        Ok(self.with_prov(pointer, shadow.prov_at(0, size)))
    }

    /// The bytes of memory at `addr`, read by the evaluator for itself (a constant's value
    /// once evaluated, a vtable): the machine doesn't see these.
    fn peek_memory(&self, addr: Address, size: usize) -> Result<'db, &[u8]> {
        if size == 0 {
            return Ok(&[]);
        }
        let (mem, pos): (&[u8], usize) = match addr.at() {
            Stack(it) => {
                let (t, pos) = threads::stack_position(it, self.cached_ptr_size);
                let Some(region) = threads::region(&self.stack, &self.threads, self.thread, t)
                else {
                    // alloc: error path
                    return Err(MirEvalError::UndefinedBehavior(format!(
                        "read of the stack of thread {t}, which was never made"
                    )));
                };
                (region, pos)
            }
            Heap(it) => {
                self.check_static_is_evaluated(it, size)?;
                (&self.heap, it)
            }
            Invalid(it) => {
                // alloc: error path
                return Err(MirEvalError::UndefinedBehavior(format!(
                    "read invalid memory address {it} with size {size}"
                )));
            }
        };
        mem.get(pos..pos + size)
            // alloc: error path
            .ok_or_else(|| MirEvalError::UndefinedBehavior("out of bound memory read".to_owned()))
    }

    /// Fails a read of heap memory that stands in for a static left unevaluated.
    fn check_static_is_evaluated(&self, pos: usize, size: usize) -> Result<'db, ()> {
        let read = self
            .unevaluated_statics
            .iter()
            .find(|&&(start, len, _)| pos < start + len && start < pos + size);
        if let Some(&(.., id)) = read {
            self.missing_static.set(Some(id));
            not_supported!("reading a static that was left unevaluated");
        }
        let read = self
            .extern_statics
            .iter()
            .find(|&&(start, len, _)| pos < start + len && start < pos + size);
        let Some(&(.., id)) = read else { return Ok(()) };
        // alloc: error path
        Err(MirEvalError::NotSupported(format!(
            "cannot access extern static `{}` at compile time",
            StaticSignature::of(self.db, id).name.as_str()
        )))
    }

    /// Fails a write to the memory of a static from a static's initializer.
    fn check_static_write(&self, pos: usize, size: usize) -> Result<'db, ()> {
        if !self.in_static {
            return Ok(());
        }
        let written = self
            .static_memory
            .iter()
            .chain(&self.unevaluated_statics)
            .find(|&&(start, len, _)| pos < start + len && start < pos + size);
        let Some(&(.., id)) = written else { return Ok(()) };
        // alloc: error path
        Err(MirEvalError::UndefinedBehavior(format!(
            "modifying the initial value of static `{}` from a static's initializer",
            StaticSignature::of(self.db, id).name.as_str()
        )))
    }

    /// Program memory at `addr` that the program writes computed bytes to, which the machine
    /// sees as initialized and carrying no provenance.
    fn write_memory_using_ref(&mut self, addr: Address, size: usize) -> Result<'db, &mut [u8]> {
        if size == 0 {
            return Ok(&mut []);
        }
        if M::CHECKS {
            let shadow = M::Shadow::init(size);
            let ptr = self.ptr(addr);
            self.machine.write(ptr, size as u64, addr.align(), Access::Plain, &shadow)?;
        }
        self.poke_memory(addr, size)
    }

    /// The bytes of memory at `addr` for the evaluator to write itself, unseen by the machine.
    fn poke_memory(&mut self, addr: Address, size: usize) -> Result<'db, &mut [u8]> {
        if size == 0 {
            return Ok(&mut []);
        }
        if let Heap(pos) = addr.at() {
            self.check_static_write(pos, size)?;
        }
        let (mem, pos) = match addr.at() {
            Stack(it) => {
                let (t, pos) = threads::stack_position(it, self.cached_ptr_size);
                let running = self.thread;
                let Some(region) =
                    threads::region_mut(&mut self.stack, &mut self.threads, running, t)
                else {
                    // alloc: error path
                    return Err(MirEvalError::UndefinedBehavior(format!(
                        "write of the stack of thread {t}, which was never made"
                    )));
                };
                (region, pos)
            }
            Heap(it) => (&mut self.heap, it),
            Invalid(it) => {
                // alloc: error path
                return Err(MirEvalError::UndefinedBehavior(format!(
                    "write invalid memory address {it} with size {size}"
                )));
            }
        };
        mem.get_mut(pos..pos + size)
            // alloc: error path
            .ok_or_else(|| MirEvalError::UndefinedBehavior("out of bound memory write".to_owned()))
    }

    fn write_memory(&mut self, addr: Address, r: &[u8]) -> Result<'db, ()> {
        if r.is_empty() {
            return Ok(());
        }
        self.write_memory_using_ref(addr, r.len())?.copy_from_slice(r);
        Ok(())
    }

    /// Writes `r`, of which `shadow` says what is initialized and what carries provenance.
    fn write_shadowed(&mut self, addr: Address, r: &[u8], shadow: &M::Shadow) -> Result<'db, ()> {
        if r.is_empty() {
            return Ok(());
        }
        if M::CHECKS {
            let ptr = self.ptr(addr);
            self.machine.write(ptr, r.len() as u64, addr.align(), Access::Plain, shadow)?;
        }
        self.poke_memory(addr, r.len())?.copy_from_slice(r);
        Ok(())
    }

    /// Writes at `at` a pointer to `to`, which carries `to`'s provenance.
    fn write_pointer(&mut self, at: Address, to: Address) -> Result<'db, ()> {
        let bytes = to.to_bytes(self.cached_ptr_size);
        if !M::CHECKS {
            return self.write_memory(at, &bytes);
        }
        let shadow = M::Shadow::pointer(bytes.len(), self.ptr(to).prov);
        self.write_shadowed(at.aligned(bytes.len()), &bytes, &shadow)
    }

    /// Writes at `at` a pointer to `to`, an address in memory the evaluator laid out itself (a
    /// constant's), which carries the root provenance of the allocation `to` is in.
    fn write_root_pointer(&mut self, at: Address, to: usize) -> Result<'db, ()> {
        let to = Address::from_usize(to, self.cached_ptr_size);
        if !M::CHECKS {
            return self.write_memory(at, &to.to_bytes(self.cached_ptr_size));
        }
        let to = self.with_prov(to, self.machine.root(to.to_usize(self.cached_ptr_size) as u64));
        self.write_pointer(at, to)
    }

    /// Writes at `at` a pointer to the function `id` (a [`VTableMap`] id), which carries the
    /// provenance the machine gives functions.
    fn write_function_pointer(&mut self, at: Address, id: usize) -> Result<'db, ()> {
        let bytes = word_bytes(id, self.cached_ptr_size);
        if !M::CHECKS {
            self.poke_memory(at, bytes.len())?.copy_from_slice(&bytes);
            return Ok(());
        }
        let shadow = M::Shadow::pointer(bytes.len(), self.machine.function(id as u64));
        self.write_shadowed(at, &bytes, &shadow)
    }

    /// Writes at `at` the vtable `id` (a [`VTableMap`] id of a type and a `dyn` type), which
    /// carries the provenance the machine gives vtables.
    fn write_vtable_pointer(&mut self, at: Address, id: usize) -> Result<'db, ()> {
        let bytes = word_bytes(id, self.cached_ptr_size);
        if !M::CHECKS {
            self.poke_memory(at, bytes.len())?.copy_from_slice(&bytes);
            return Ok(());
        }
        let shadow = M::Shadow::pointer(bytes.len(), self.machine.vtable(id as u64));
        self.write_shadowed(at, &bytes, &shadow)
    }

    /// A computed pointer value to `to` with `metadata` after it, as a `Ref` makes one.
    fn pointer_value(
        &self,
        to: Address,
        metadata: Option<&IntervalOrOwned<M::Shadow>>,
    ) -> Result<'db, IntervalOrOwned<M::Shadow>> {
        let mut bytes = OwnedBytes::from_slice(&to.to_bytes(self.cached_ptr_size));
        let Some(metadata) = metadata else {
            return Ok(match M::CHECKS {
                true => {
                    let shadow = M::Shadow::pointer(bytes.len(), self.ptr(to).prov);
                    IntervalOrOwned::Shadowed(bytes, shadow)
                }
                false => IntervalOrOwned::Owned(bytes),
            });
        };
        if !M::CHECKS {
            bytes.extend(metadata.get(self)?.iter().copied());
            return Ok(IntervalOrOwned::Owned(bytes));
        }
        let mut shadow = M::Shadow::pointer(bytes.len(), self.ptr(to).prov);
        let at = bytes.len();
        let (meta, meta_shadow) = self.value_shadowed(metadata)?;
        bytes.extend(meta.iter().copied());
        shadow.put(at, &meta_shadow);
        Ok(IntervalOrOwned::Shadowed(bytes, shadow))
    }

    /// The bytes of a value and what the machine knows of them.
    fn value_shadowed<'b>(
        &'b self,
        value: &'b IntervalOrOwned<M::Shadow>,
    ) -> Result<'db, (&'b [u8], M::Shadow)> {
        Ok(match value {
            IntervalOrOwned::Owned(bytes) => (bytes, M::Shadow::init(bytes.len())),
            IntervalOrOwned::Shadowed(bytes, shadow) => (bytes, shadow.clone()), // clone: `()` without checks, an inline `SmallVec` of pointer pieces with them
            IntervalOrOwned::Borrowed(it) => self.read_shadowed(it.addr, it.size)?,
        })
    }

    fn copy_from_interval_or_owned(
        &mut self,
        addr: Address,
        r: IntervalOrOwned<M::Shadow>,
    ) -> Result<'db, ()> {
        match r {
            IntervalOrOwned::Borrowed(r) => self.copy_from_interval(addr, r),
            IntervalOrOwned::Owned(r) => self.write_memory(addr, &r),
            IntervalOrOwned::Shadowed(r, shadow) => self.write_shadowed(addr, &r, &shadow),
        }
    }

    /// Copies the bytes of `r` to `addr`, which the machine sees as one copy of what they are.
    fn copy_from_interval(&mut self, addr: Address, r: Interval) -> Result<'db, ()> {
        if r.size == 0 {
            return Ok(());
        }
        if M::CHECKS {
            let align = addr.align().max(r.addr.align());
            self.machine.copy(self.ptr(addr), self.ptr(r.addr), r.size as u64, align, true)?;
        }
        self.move_memory(addr, r)
    }

    /// The bytes of `r` moved to `addr`, which may overlap them, unseen by the machine: what
    /// a copy does once the machine saw it.
    fn move_memory(&mut self, addr: Address, r: Interval) -> Result<'db, ()> {
        if r.size == 0 {
            return Ok(());
        }
        // alloc: error path
        let oob = || MirEvalError::UndefinedBehavior("out of bounds memory write".to_owned());

        if let Heap(src) = r.addr.at() {
            self.check_static_is_evaluated(src, r.size)?;
        }
        if let Heap(dst) = addr.at() {
            self.check_static_write(dst, r.size)?;
        }
        let running = self.thread;
        match (addr.at(), r.addr.at()) {
            (Stack(dst), Stack(src)) => {
                let ((dst_thread, dst), (src_thread, src)) = (
                    threads::stack_position(dst, self.cached_ptr_size),
                    threads::stack_position(src, self.cached_ptr_size),
                );
                if dst_thread == src_thread {
                    let stack = threads::region_mut(
                        &mut self.stack,
                        &mut self.threads,
                        running,
                        dst_thread,
                    )
                    .ok_or_else(oob)?;
                    if stack.len() < src + r.size || stack.len() < dst + r.size {
                        return Err(oob());
                    }
                    stack.copy_within(src..src + r.size, dst)
                } else {
                    let bytes = threads::region(&self.stack, &self.threads, running, src_thread)
                        .and_then(|it| it.get(src..src + r.size))
                        .ok_or_else(oob)?
                        .to_vec(); // alloc: a move between two threads' stacks, through a pointer into another thread's stack
                    threads::region_mut(&mut self.stack, &mut self.threads, running, dst_thread)
                        .and_then(|it| it.get_mut(dst..dst + r.size))
                        .ok_or_else(oob)?
                        .copy_from_slice(&bytes);
                }
            }
            (Heap(dst), Heap(src)) => {
                if self.heap.len() < src + r.size || self.heap.len() < dst + r.size {
                    return Err(oob());
                }
                self.heap.copy_within(src..src + r.size, dst)
            }
            (Stack(dst), Heap(src)) => {
                let (dst_thread, dst) = threads::stack_position(dst, self.cached_ptr_size);
                threads::region_mut(&mut self.stack, &mut self.threads, running, dst_thread)
                    .and_then(|it| it.get_mut(dst..dst + r.size))
                    .ok_or_else(oob)?
                    .copy_from_slice(self.heap.get(src..src + r.size).ok_or_else(oob)?);
            }
            (Heap(dst), Stack(src)) => {
                let (src_thread, src) = threads::stack_position(src, self.cached_ptr_size);
                let bytes = threads::region(&self.stack, &self.threads, running, src_thread)
                    .and_then(|it| it.get(src..src + r.size))
                    .ok_or_else(oob)?;
                self.heap.get_mut(dst..dst + r.size).ok_or_else(oob)?.copy_from_slice(bytes);
            }
            _ => {
                // alloc: error path
                return Err(MirEvalError::UndefinedBehavior(format!(
                    "invalid memory write at address {:#x}",
                    addr.to_usize(self.cached_ptr_size)
                )));
            }
        }

        Ok(())
    }

    fn size_align_of(
        &self,
        ty: Ty<'db>,
        _locals: &Locals<'db>,
    ) -> Result<'db, Option<(usize, usize)>> {
        if let Some(layout) = self.layout_cache.borrow().get(&ty) {
            return Ok(layout
                .is_sized()
                .then(|| (layout.size.bytes_usize(), layout.align.bytes() as usize)));
        }
        let layout = self.layout(ty)?;
        Ok(layout.is_sized().then(|| (layout.size.bytes_usize(), layout.align.bytes() as usize)))
    }

    /// The size of a value of a struct whose last field is a `str`, a slice or a `dyn`, or of
    /// one of those, given the pointer metadata.
    fn size_of_unsized_struct(&self, ty: Ty<'db>, metadata: &[u8]) -> Result<'db, Option<usize>> {
        Ok(self.size_align_of_tail(ty, metadata)?.map(|(size, _)| size))
    }

    /// The size and alignment of a value of the unsized `ty` whose pointer's metadata is
    /// `metadata`: a struct's or tuple's tail starts where its sized fields end, at the tail's
    /// alignment (a `dyn`'s is its concrete type's) capped by the struct's `packed`, as rustc
    /// lays it out ([`Evaluator::tail_offset`], `hir_ty::layout::dyn_tail_place`).
    fn size_align_of_tail(
        &self,
        ty: Ty<'db>,
        metadata: &[u8],
    ) -> Result<'db, Option<(usize, usize)>> {
        let len = from_bytes!(usize, self.cached_ptr_size, metadata);
        Ok(match ty.kind() {
            TyKind::Str => Some((len, 1)),
            TyKind::Slice(inner) => {
                let layout = self.layout(inner)?;
                Some((layout.size.bytes_usize() * len, layout.align.bytes() as usize))
            }
            TyKind::Dynamic(..) => {
                let layout = self.layout(self.vtable_map.ty_of_bytes(metadata)?)?;
                Some((layout.size.bytes_usize(), layout.align.bytes() as usize))
            }
            TyKind::Adt(..) | TyKind::Tuple(..) => {
                let Some(last) = self.last_field(ty)? else { return Ok(None) };
                let Some((tail, tail_align)) = self.size_align_of_tail(last.ty, metadata)? else {
                    return Ok(None);
                };
                let (start, align) = self.tail_offset(ty, &last, tail_align)?;
                Some(((start + tail).next_multiple_of(align), align))
            }
            _ => None,
        })
    }

    /// The last field of the struct or tuple `ty`, normalized, with its index and its
    /// struct's `packed`; `None` for another type, a struct with no fields or an enum.
    fn last_field(&self, ty: Ty<'db>) -> Result<'db, Option<LastField<'db>>> {
        Ok(match ty.kind() {
            TyKind::Adt(adt, subst) => {
                let AdtId::StructId(id) = adt.def_id() else { return Ok(None) };
                let field_types = self.db.field_types(id.into());
                let Some((_, last)) = field_types.iter().next_back() else { return Ok(None) };
                let last = last.ty().instantiate(self.interner(), subst).skip_norm_wip();
                let pack = hir_def::attrs::AttrFlags::repr(self.db, id.into())
                    .and_then(|it| it.pack)
                    .map(|it| it.bytes_usize());
                Some(LastField {
                    ty: self.normalized(last, "the tail of a struct")?,
                    index: field_types.iter().count() - 1,
                    pack,
                })
            }
            TyKind::Tuple(tys) => {
                let Some(last) = tys.iter().next_back() else { return Ok(None) };
                let ty = self.normalized(last, "the tail of a tuple")?;
                Some(LastField { ty, index: tys.len() - 1, pack: None })
            }
            _ => None,
        })
    }

    /// Where the unsized `last` field of `ty` starts when the tail is aligned to `tail_align`,
    /// and the alignment of a value of `ty`: after the sized fields, at the tail's alignment
    /// capped by `packed`, as rustc's `project_field` and `size_and_align_of_dst` place it and
    /// as `hir_ty::layout::dyn_tail_place` does for a `dyn`.
    fn tail_offset(
        &self,
        ty: Ty<'db>,
        last: &LastField<'db>,
        tail_align: usize,
    ) -> Result<'db, (usize, usize)> {
        let layout = self.layout(ty)?;
        let field_align = last.pack.map_or(tail_align, |pack| tail_align.min(pack));
        let start = layout.fields.offset(last.index).bytes_usize().next_multiple_of(field_align);
        Ok((start, (layout.align.bytes() as usize).max(field_align)))
    }

    /// The parts of a value of `ty`, a struct or tuple with an unsized tail whose pointer's
    /// metadata is `meta`, each as `(offset, type, count)`: the sized fields of `ty` and of
    /// each struct in its chain of last fields, a slice tail's elements (`count` of them,
    /// one after another; none when they hold no pointer), and a `dyn` tail's concrete value,
    /// placed by `hir_ty::layout::dyn_tail_place`, the one rule codegen and code identity use.
    /// Building a constant's memory map and patching its addresses walk these.
    fn unsized_parts(
        &self,
        ty: Ty<'db>,
        meta: &[u8],
    ) -> Result<'db, SmallVec<[(usize, Ty<'db>, usize); 4]>> {
        // A chain of last fields deeper than this is a type no program lays out.
        const FUEL: usize = 256;
        let mut parts = SmallVec::new();
        let (mut base, mut at) = (0, ty);
        for _ in 0..FUEL {
            let Some(last) = self.last_field(at)? else { return Ok(parts) };
            let layout = self.layout(at)?;
            let sized: SmallVec<[Ty<'db>; 4]> = match at.kind() {
                TyKind::Adt(adt, subst) => {
                    let AdtId::StructId(id) = adt.def_id() else { return Ok(parts) };
                    let field_types = self.db.field_types(id.into());
                    (field_types.iter().take(last.index))
                        .map(|(_, it)| it.ty().instantiate(self.interner(), subst).skip_norm_wip())
                        .collect()
                }
                TyKind::Tuple(tys) => tys.iter().take(last.index).collect(),
                _ => return Ok(parts),
            };
            for (i, field) in sized.into_iter().enumerate() {
                let field = self.normalized(field, "a field of a constant's struct")?;
                parts.push((base + layout.fields.offset(i).bytes_usize(), field, 1));
            }
            let Some((_, align)) = self.size_align_of_tail(last.ty, meta)? else {
                return Ok(parts);
            };
            let (offset, _) = self.tail_offset(at, &last, align)?;
            match last.ty.kind() {
                TyKind::Slice(elem) => {
                    if !self.holds_no_pointer(elem) {
                        let len = from_bytes!(usize, self.cached_ptr_size, meta);
                        parts.push((base + offset, elem, len));
                    }
                    return Ok(parts);
                }
                TyKind::Dynamic(..) => {
                    let concrete = self.vtable_map.ty_of_bytes(meta)?;
                    let align = self.layout(concrete)?.align.abi.bytes_usize();
                    let place = crate::layout::dyn_tail_place(
                        self.db,
                        ty,
                        align,
                        |it| self.normalized(it, "a field of a constant's struct"),
                        |it| self.layout(it).map(|it| &it.get().0),
                    )?;
                    if let Some((offset, _)) = place {
                        parts.push((offset, concrete, 1));
                    }
                    return Ok(parts);
                }
                TyKind::Adt(..) | TyKind::Tuple(..) => {
                    base += offset;
                    at = last.ty;
                }
                _ => return Ok(parts),
            }
        }
        Err(MirEvalError::StackOverflow)
    }

    /// `ty` normalized when it is an alias (a field's `<T as Tr>::Obj`), as its size, layout or
    /// kind is read; one that does not normalize is an error naming `what` it is.
    fn normalized(&self, ty: Ty<'db>, what: &str) -> Result<'db, Ty<'db>> {
        if !matches!(ty.kind(), TyKind::Alias(..)) {
            return Ok(ty);
        }
        let normalized = ObligationCtxt::new(&self.infcx)
            .structurally_normalize_ty(&ObligationCause::dummy(), self.param_env.param_env, ty)
            .ok();
        match normalized {
            Some(it) if !matches!(it.kind(), TyKind::Alias(..) | TyKind::Infer(..)) => Ok(it),
            _ => not_supported!("{what}, `{ty:?}`, does not normalize"),
        }
    }

    /// The type at the end of `ty`'s chain of last fields, normalized, which says what a pointer
    /// to `ty` has for metadata; one that does not normalize is an error.
    fn struct_tail(&self, ty: Ty<'db>) -> Result<'db, Ty<'db>> {
        let mut failed = None;
        let tail = crate::layout::struct_tail_erasing_lifetimes(self.db, ty, |it| {
            self.normalized(it, "a struct's tail").unwrap_or_else(|e| {
                failed = Some(e);
                it
            })
        });
        match failed {
            Some(e) => Err(e),
            None => self.normalized(tail, "a pointee's tail"),
        }
    }

    /// A version of `self.size_of` which returns error if the type is unsized. `what` argument should
    /// be something that complete this: `error: type {ty} was unsized. {what} should be sized`
    fn size_of_sized(
        &self,
        ty: Ty<'db>,
        locals: &Locals<'db>,
        what: &'static str,
    ) -> Result<'db, usize> {
        match self.size_align_of(ty, locals)? {
            Some(it) => Ok(it.0),
            None => Err(MirEvalError::TypeIsUnsized(ty.store(), what)),
        }
    }

    /// A version of `self.size_align_of` which returns error if the type is unsized. `what` argument should
    /// be something that complete this: `error: type {ty} was unsized. {what} should be sized`
    fn size_align_of_sized(
        &self,
        ty: Ty<'db>,
        locals: &Locals<'db>,
        what: &'static str,
    ) -> Result<'db, (usize, usize)> {
        match self.size_align_of(ty, locals)? {
            Some(it) => Ok(it),
            None => Err(MirEvalError::TypeIsUnsized(ty.store(), what)),
        }
    }

    /// The allocation `addr` is in.
    fn allocation_at(&self, addr: usize) -> Option<Allocated> {
        match Address::from_usize(addr, self.cached_ptr_size).at() {
            Stack(offset) => {
                let (t, _) = threads::stack_position(offset, self.cached_ptr_size);
                let allocations = threads::region_allocations(
                    &self.stack_allocations,
                    &self.threads,
                    self.thread,
                    t,
                );
                let i = allocations.partition_point(|it| it.start <= addr);
                let &allocation = allocations.get(i.checked_sub(1)?)?;
                (addr - allocation.start < allocation.size).then_some(allocation)
            }
            Heap(offset) => {
                let (start, block) = self.heap_block_at(offset)?;
                let start = Address::heap(start).to_usize(self.cached_ptr_size);
                Some(Allocated { start, size: block.size, align: block.align, tracked: false })
            }
            Invalid(_) => None,
        }
    }

    /// The live heap block the heap offset `offset` is in, with where it starts.
    fn heap_block_at(&self, offset: usize) -> Option<(usize, HeapBlock)> {
        let (&start, &block) = self.heap_blocks.range(..=offset).next_back()?;
        (offset - start < block.size).then_some((start, block))
    }

    /// Records in `map` what `addr` is known to be a multiple of: what its allocation was
    /// aligned to, or less when it is at an offset that is a multiple of less. An address one
    /// past an allocation's end is of that allocation, as `behind_raw` takes it (the heap and
    /// the stack leave no allocation starting there): the empty piece a slice's empty tail
    /// keeps (`split_at(len).1`), or a zero-size last field's, is aligned as that offset of
    /// it, never left without an alignment for its reader to refuse. A value of no size
    /// elsewhere is at its alignment, of no allocation (`Address::invalid`), and keeps no piece.
    fn note_align(&self, map: &mut ComplexMemoryMap<'db>, addr: usize) {
        let allocation = self.allocation_at(addr).or_else(|| {
            let it = self.allocation_at(addr.checked_sub(1)?)?;
            (addr - it.start == it.size).then_some(it)
        });
        let Some(allocation) = allocation else { return };
        let align = match addr - allocation.start {
            0 => allocation.align,
            offset => allocation.align.min(1 << offset.trailing_zeros()),
        };
        map.note_align(addr, align);
    }

    /// Whether `more` bytes of stack or heap fit the evaluation's budget; when they do, the
    /// total they make counts toward [`Self::memory_peak`].
    fn check_memory(&mut self, more: usize) -> Result<'db, ()> {
        let used = self.stack.len() as u64 + self.heap.len() as u64;
        match (more as u64).checked_add(used) {
            Some(total) if total <= self.budget.memory => {
                self.memory_peak = self.memory_peak.max(total);
                Ok(())
            }
            _ => Err(MirEvalError::OverBudget {
                spent: Budget::Memory(self.budget.memory),
                at: None,
            }),
        }
    }

    /// Memory of the evaluator's own ([`Origin::Evaluator`]), zeroed.
    fn heap_allocate(&mut self, size: usize, align: usize) -> Result<'db, Address> {
        self.allocate(size, align, Origin::Evaluator, true)
    }

    /// The block of [`Self::heap_allocate`] that `block` is, its bytes written, is read-only
    /// from now on ([`Machine::freeze`]). A zero-sized block holds nothing to write.
    #[inline(always)]
    fn freeze(&self, block: Interval) {
        if M::CHECKS && block.size != 0 {
            self.machine.freeze(block.addr.to_usize(self.cached_ptr_size) as u64);
        }
    }

    /// The block of [`Self::heap_allocate`] that `block` is has its initial bytes written
    /// ([`Machine::initialized`]).
    #[inline(always)]
    fn initialized(&self, block: Interval) {
        if M::CHECKS && block.size != 0 {
            self.machine.initialized(block.addr.to_usize(self.cached_ptr_size) as u64);
        }
    }

    /// A zeroed block of the heap: a freed one of its size class and alignment when there is
    /// one, else one at the end.
    /// The machine is told of it, as `zeroed` (initialized) or not.
    fn allocate(
        &mut self,
        size: usize,
        align: usize,
        origin: Origin,
        zeroed: bool,
    ) -> Result<'db, Address> {
        if !align.is_power_of_two() || align > 10000 {
            // alloc: error path
            return Err(MirEvalError::UndefinedBehavior(format!("Alignment {align} is invalid")));
        }
        if size == 0 {
            // As a local of no size: aligned, of no block, so not where the next block starts.
            return Ok(Address::invalid(align));
        }
        let class = size_class(size);
        let pos = match self.free_blocks.get_mut(&(class, align)).and_then(Vec::pop) {
            Some(pos) => {
                self.heap[pos..pos + class].fill(0);
                pos
            }
            // A byte at least between each block and the next, so a pointer one past a block's
            // end is not also the start of another: which allocation it is of stays known.
            None => {
                let pos = (self.heap.len() + 1).next_multiple_of(align);
                self.check_memory((pos - self.heap.len()).saturating_add(class))?;
                self.heap.resize(pos + class, 0);
                pos
            }
        };
        self.heap_blocks.insert(pos, HeapBlock { size, align, class, origin });
        let addr = Address::heap(pos);
        if !M::CHECKS {
            return Ok(addr);
        }
        let start = addr.to_usize(self.cached_ptr_size) as u64;
        let prov = self.machine.alloc(start, size as u64, align as u64, origin.kind(), zeroed)?;
        Ok(self.with_prov(addr, Some(prov)))
    }

    /// Frees the block `addr` starts, which `origin` made; `what` names the call, for its
    /// error. As rustc's evaluation, freeing anything else is undefined behavior.
    fn deallocate(&mut self, addr: Address, origin: Origin, what: &str) -> Result<'db, ()> {
        let block = match addr.at() {
            Heap(pos) => self.heap_blocks.get(&pos).copied().map(|it| (pos, it)),
            _ => None,
        };
        let Some((pos, block)) = block.filter(|(_, it)| it.origin == origin) else {
            if M::CHECKS {
                // The machine names what the pointer is (freed, of another kind, not at the
                // start), as Miri's `deallocate_ptr` does.
                self.machine.free(self.ptr(addr), None, None, origin.kind())?;
            }
            // alloc: error path
            return Err(MirEvalError::UndefinedBehavior(format!(
                "`{what}` of a pointer that is not the start of memory allocated by it"
            )));
        };
        if M::CHECKS {
            let (size, align) = (Some(block.size as u64), Some(block.align as u64));
            self.machine.free(self.ptr(addr), size, align, origin.kind())?;
        }
        self.heap_blocks.remove(&pos);
        self.free_blocks.entry((block.class, block.align)).or_default().push(pos);
        Ok(())
    }

    /// `__rust_realloc`, or C's `realloc` for [`Origin::C`] (`what`): the block grows or
    /// shrinks in place while it fits its size class, else its bytes move to a new block and
    /// it is freed.
    fn reallocate(
        &mut self,
        addr: Address,
        new_size: usize,
        origin: Origin,
        what: &str,
    ) -> Result<'db, Address> {
        let block = match addr.at() {
            Heap(pos) => self.heap_blocks.get(&pos).copied().map(|it| (pos, it)),
            _ => None,
        };
        let Some((pos, block)) = block.filter(|(_, it)| it.origin == origin) else {
            // alloc: error path
            return Err(MirEvalError::UndefinedBehavior(format!(
                "`{what}` of a pointer that is not the start of memory allocated by it"
            )));
        };
        // The machine sees a reallocation as rustc's evaluation does: a new block, the bytes
        // that fit copied to it, the old one freed.
        if !M::CHECKS && new_size != 0 && new_size <= block.class {
            if new_size > block.size {
                self.heap[pos + block.size..pos + new_size].fill(0);
            }
            self.heap_blocks.insert(pos, HeapBlock { size: new_size, ..block });
            return Ok(addr);
        }
        let new = self.allocate(new_size, block.align, origin, false)?;
        let kept = block.size.min(new_size);
        Interval { addr: new, size: kept }
            .write_from_interval(self, Interval { addr, size: kept })?;
        self.deallocate(addr, origin, what)?;
        Ok(new)
    }

    /// `const_make_global`: the `const_allocate` block `addr` starts may be in the value of
    /// a constant from now on, and is immutable.
    fn make_global(&mut self, addr: Address) -> Result<'db, ()> {
        let block = match addr.at() {
            Heap(pos) => self.heap_blocks.get_mut(&pos),
            _ => None,
        };
        match block {
            Some(HeapBlock { origin: origin @ Origin::ConstHeap { global: false }, .. }) => {
                *origin = Origin::ConstHeap { global: true };
                Ok(())
            }
            _ => Err(MirEvalError::UndefinedBehavior(
                "`const_make_global` of a pointer that is not the start of memory \
                 `const_allocate` allocated and nothing made global yet"
                    // alloc: error path
                    .to_owned(),
            )),
        }
    }

    /// Refuses `addr` in the value of the constant: memory the program allocated, which it
    /// would free at run time, or `const_allocate` memory not made global. rustc refuses the
    /// latter alike ("encountered `const_allocate` pointer in final value that was not made
    /// global"), and has no `Box` or `Vec` that allocates in a constant.
    ///
    /// `shared`: the value reaches `addr` only as immutable, unowned memory, through a shared
    /// reference to `Freeze` data or a raw pointer in such memory. In a `const static { .. }`
    /// block (`#![feature(blaze_comptime)]`) the program's allocations so reached are made
    /// global, as `const_make_global` makes a `const_allocate` block: nothing frees or writes
    /// them (`format!(..).leak()` as a `&'static str`). One the value owns is still refused.
    fn may_be_in_value(&self, addr: usize, shared: bool) -> Result<'db, ()> {
        let Heap(offset) = Address::from_usize(addr, self.cached_ptr_size).at() else {
            return Ok(());
        };
        match self.heap_block_at(offset) {
            Some((_, HeapBlock { origin: Origin::Allocator, .. })) if self.comptime && shared => {
                Ok(())
            }
            Some((_, HeapBlock { origin: Origin::Allocator, .. })) => {
                Err(MirEvalError::HeapInValue { of_const_allocate: false })
            }
            Some((_, HeapBlock { origin: Origin::ConstHeap { global: false }, .. })) => {
                Err(MirEvalError::HeapInValue { of_const_allocate: true })
            }
            _ => Ok(()),
        }
    }

    fn detect_fn_trait(&self, def: FunctionId) -> Option<FnTrait> {
        let def = Some(def);
        if def == self.cached_fn_trait_func {
            Some(FnTrait::Fn)
        } else if def == self.cached_fn_mut_trait_func {
            Some(FnTrait::FnMut)
        } else if def == self.cached_fn_once_trait_func {
            Some(FnTrait::FnOnce)
        } else {
            None
        }
    }

    /// Whether no value of the type has a pointer in it, so that its bytes mean the same
    /// wherever they are read. A type this cannot see through is taken to hold one.
    fn holds_no_pointer(&self, ty: Ty<'db>) -> bool {
        match ty.kind() {
            TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Uint(_) | TyKind::Float(_) => {
                true
            }
            TyKind::Array(inner, _) | TyKind::Pat(inner, _) => self.holds_no_pointer(inner),
            TyKind::Tuple(fields) => fields.iter().all(|it| self.holds_no_pointer(it)),
            TyKind::Adt(adt, subst) => {
                let mut variants = match adt.def_id() {
                    AdtId::StructId(it) => Either::Left(iter::once(VariantId::from(it))),
                    AdtId::UnionId(it) => Either::Left(iter::once(VariantId::from(it))),
                    AdtId::EnumId(it) => Either::Right(
                        it.enum_variants(self.db)
                            .variants
                            .values()
                            .map(|&(v, _)| VariantId::from(v)),
                    ),
                };
                variants.all(|variant| {
                    self.db.field_types(variant).iter().all(|(_, field)| {
                        let field = field.ty().instantiate(self.interner(), subst).skip_norm_wip();
                        self.holds_no_pointer(field)
                    })
                })
            }
            _ => false,
        }
    }

    /// Whether no value of the type has a `TypeId` in it or behind its references. A type
    /// this cannot see through, as a raw pointer or a `dyn`, is taken to have one; one met
    /// again in `seen`, through a reference to itself, adds nothing.
    fn holds_no_type_id(
        &self,
        ty: Ty<'db>,
        seen: &mut HashSet<Ty<'db>, std::alloc::Global>,
    ) -> bool {
        if !seen.insert(ty) {
            return true;
        }
        match ty.kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Str
            | TyKind::Never => true,
            TyKind::Array(inner, _)
            | TyKind::Pat(inner, _)
            | TyKind::Slice(inner)
            | TyKind::Ref(_, inner, _) => self.holds_no_type_id(inner, seen),
            TyKind::Tuple(fields) => fields.iter().all(|it| self.holds_no_type_id(it, seen)),
            TyKind::Adt(adt, subst) => {
                if self.lang_items().TypeId.is_some_and(|it| adt.def_id() == AdtId::StructId(it)) {
                    return false;
                }
                let mut variants = match adt.def_id() {
                    AdtId::StructId(it) => Either::Left(iter::once(VariantId::from(it))),
                    AdtId::UnionId(it) => Either::Left(iter::once(VariantId::from(it))),
                    AdtId::EnumId(it) => Either::Right(
                        it.enum_variants(self.db)
                            .variants
                            .values()
                            .map(|&(v, _)| VariantId::from(v)),
                    ),
                };
                variants.all(|variant| {
                    self.db.field_types(variant).iter().all(|(_, field)| {
                        let field = field.ty().instantiate(self.interner(), subst).skip_norm_wip();
                        self.holds_no_type_id(field, seen)
                    })
                })
            }
            _ => false,
        }
    }

    fn create_memory_map(
        &self,
        bytes: &[u8],
        ty: Ty<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, ComplexMemoryMap<'db>> {
        /// `shared`: the memory `bytes` are in is immutable and unowned, reached through a shared
        /// reference to `Freeze` data ([`Evaluator::may_be_in_value`]); never so at the top.
        fn rec<'db, M: Machine<'db>>(
            this: &Evaluator<'db, M>,
            bytes: &[u8],
            ty: Ty<'db>,
            locals: &Locals<'db>,
            mm: &mut ComplexMemoryMap<'db>,
            stack_depth_limit: usize,
            shared: bool,
        ) -> Result<'db, ()> {
            if stack_depth_limit.checked_sub(1).is_none() {
                return Err(MirEvalError::StackOverflow);
            }
            // Whether what a pointer of `ty` points to is so: through `&T` of a `Freeze` `T`,
            // or through a raw pointer in memory that is. Asked only in a `const static` block,
            // the one evaluation whose value may keep the program's allocations.
            let pointee_shared = match ty.kind() {
                TyKind::Ref(_, pointee, Mutability::Not) => {
                    this.comptime && is_freeze(&this.infcx, pointee, this.param_env.param_env)
                }
                TyKind::Ref(..) => false,
                _ => shared,
            };
            if let TyKind::RawPtr(..) | TyKind::Ref(..) = ty.kind()
                && let Some(addr) = bytes.get(..this.ptr_size())
            {
                let addr = from_bytes!(usize, this.cached_ptr_size, addr);
                // One past a static's end is in it too, as rustc's provenance says.
                let pointee = this.static_memory.iter().find(|&&(start, size, _)| {
                    addr.checked_sub(start).is_some_and(|offset| offset <= size)
                });
                if let Some(&(start, size, id)) = pointee {
                    let pointee = crate::StaticMemory { start, size, id };
                    if !mm.statics.contains(&pointee) {
                        mm.statics.push(pointee);
                    }
                    return Ok(());
                }
                this.may_be_in_value(addr, pointee_shared)?;
            }
            match ty.kind() {
                // Nothing to find in these, and a large array of them is slow to walk.
                TyKind::Array(inner, _) | TyKind::Slice(inner)
                    if matches!(
                        inner.kind(),
                        TyKind::Bool
                            | TyKind::Char
                            | TyKind::Int(_)
                            | TyKind::Uint(_)
                            | TyKind::Float(_)
                    ) => {}
                // Its pointers are an id, of nothing in memory.
                TyKind::Adt(adt, _)
                    if this
                        .lang_items()
                        .TypeId
                        .is_some_and(|it| adt.def_id() == AdtId::StructId(it)) => {}
                TyKind::Pat(base, _) => {
                    rec(this, bytes, base, locals, mm, stack_depth_limit - 1, shared)?
                }
                TyKind::RawPtr(..) => {
                    let Some(addr) = bytes.get(..this.ptr_size()) else { return Ok(()) };
                    let addr = from_bytes!(usize, this.cached_ptr_size, addr);
                    behind_raw(this, addr, mm, stack_depth_limit - 1, pointee_shared)?;
                }
                TyKind::Ref(_, t, _) => {
                    // A field's pointee can be an alias (`&'static <T as Tr>::Obj`), whose size
                    // and metadata are of the type it normalizes to.
                    let t = this.normalized(t, "the pointee of a reference in a constant")?;
                    let size = this.size_align_of(t, locals)?;
                    match size {
                        Some((size, _)) => {
                            let addr_usize = from_bytes!(usize, this.cached_ptr_size, bytes);
                            let addr = Address::from_usize(addr_usize, this.cached_ptr_size);
                            if addr.is_of_nothing(size) {
                                return Ok(());
                            }
                            let bytes = this.read_memory(addr, size)?;
                            mm.memory.insert(addr_usize, bytes);
                            this.note_align(mm, addr_usize);
                            let depth = stack_depth_limit - 1;
                            rec(this, bytes, t, locals, mm, depth, pointee_shared)?;
                        }
                        None => {
                            let mut check_inner = None;
                            let (addr, meta) = bytes.split_at(bytes.len() / 2);
                            if let TyKind::Adt(..) | TyKind::Tuple(..) = t.kind() {
                                // A struct or tuple with an unsized tail, whose fields are
                                // followed as a sized one's are, the tail's by its metadata.
                                if let Some(size) = this.size_of_unsized_struct(t, meta)? {
                                    let addr = Address::from_bytes(addr, this.cached_ptr_size)?;
                                    // Of no size and at no allocation (a zero-size local's
                                    // address is its alignment): nothing to keep, as for
                                    // a sized pointee or a slice.
                                    if addr.is_of_nothing(size) {
                                        return Ok(());
                                    }
                                    let b = this.read_memory(addr, size)?;
                                    mm.memory.insert(addr.to_usize(this.cached_ptr_size), b);
                                    this.note_align(mm, addr.to_usize(this.cached_ptr_size));
                                    let depth = stack_depth_limit - 1;
                                    for (offset, part, count) in this.unsized_parts(t, meta)? {
                                        let size = this.size_of_sized(
                                            part,
                                            locals,
                                            "a part of an unsized value",
                                        )?;
                                        // Elements that take no room may be `usize::MAX` in number.
                                        if size == 0 {
                                            continue;
                                        }
                                        for i in 0..count {
                                            let at = offset + i * size;
                                            let Some(bytes) = b.get(at..at + size) else { break };
                                            let shared = pointee_shared;
                                            rec(this, bytes, part, locals, mm, depth, shared)?;
                                        }
                                    }
                                }
                                return Ok(());
                            }
                            let element_size = match t.kind() {
                                TyKind::Str => 1,
                                TyKind::Slice(t) => {
                                    check_inner = Some(t);
                                    this.size_of_sized(t, locals, "slice inner type")?
                                }
                                TyKind::Dynamic(..) => {
                                    let t = this.vtable_map.ty_of_bytes(meta)?;
                                    check_inner = Some(t);
                                    this.size_of_sized(t, locals, "dyn concrete type")?
                                }
                                _ => return Ok(()),
                            };
                            let count = match t.kind() {
                                TyKind::Dynamic(..) => 1,
                                _ => from_bytes!(usize, this.cached_ptr_size, meta),
                            };
                            let size = element_size * count;
                            let addr = Address::from_bytes(addr, this.cached_ptr_size)?;
                            if addr.is_of_nothing(size) {
                                return Ok(());
                            }
                            let b = this.read_memory(addr, size)?;
                            mm.memory.insert(addr.to_usize(this.cached_ptr_size), b);
                            this.note_align(mm, addr.to_usize(this.cached_ptr_size));
                            // Elements that take no room may be `usize::MAX` in number.
                            let plain = element_size == 0
                                || check_inner.is_some_and(|ty| this.holds_no_pointer(ty));
                            if let Some(ty) = check_inner
                                && !plain
                            {
                                for i in 0..count {
                                    let offset = element_size * i;
                                    rec(
                                        this,
                                        &b[offset..offset + element_size],
                                        ty,
                                        locals,
                                        mm,
                                        stack_depth_limit - 1,
                                        pointee_shared,
                                    )?;
                                }
                            }
                        }
                    }
                }
                TyKind::Array(inner, _) if this.holds_no_pointer(inner) => {}
                TyKind::Array(inner, len) => {
                    let len = match try_const_usize(this.db, len) {
                        Some(it) => it as usize,
                        None => not_supported!("non evaluatable array len in patching addresses"),
                    };
                    let size = this.size_of_sized(inner, locals, "inner of array")?;
                    // There may be `usize::MAX` elements when they take no room.
                    let len = if size == 0 { 0 } else { len };
                    for i in 0..len {
                        let offset = i * size;
                        rec(
                            this,
                            &bytes[offset..offset + size],
                            inner,
                            locals,
                            mm,
                            stack_depth_limit - 1,
                            shared,
                        )?;
                    }
                }
                TyKind::Tuple(subst) => {
                    let layout = this.layout(ty)?;
                    for (id, ty) in subst.iter().enumerate() {
                        let offset = layout.fields.offset(id).bytes_usize();
                        let size = this.layout(ty)?.size.bytes_usize();
                        rec(
                            this,
                            &bytes[offset..offset + size],
                            ty,
                            locals,
                            mm,
                            stack_depth_limit - 1,
                            shared,
                        )?;
                    }
                }
                TyKind::Adt(adt, subst) => match adt.def_id() {
                    // A `#[blaze::bits]` struct is an integer: no field holds an address.
                    AdtId::StructId(s) if crate::layout::is_bits_struct(this.db, s) => {}
                    AdtId::StructId(s) => {
                        let data = s.fields(this.db);
                        let layout = this.layout(ty)?;
                        let field_types = this.db.field_types(s.into());
                        for (f, _) in data.fields().iter() {
                            let offset = layout
                                .fields
                                .offset(u32::from(f.into_raw()) as usize)
                                .bytes_usize();
                            let ty = field_types[f]
                                .ty()
                                .instantiate(this.interner(), subst)
                                .skip_norm_wip();
                            let size = this.layout(ty)?.size.bytes_usize();
                            rec(
                                this,
                                &bytes[offset..offset + size],
                                ty,
                                locals,
                                mm,
                                stack_depth_limit - 1,
                                shared,
                            )?;
                        }
                    }
                    AdtId::EnumId(e) => {
                        let layout = this.layout(ty)?;
                        if let Some((v, l)) = detect_variant_from_bytes(
                            &layout,
                            this.db,
                            this.target_data_layout,
                            bytes,
                            e,
                        ) {
                            let data = v.fields(this.db);
                            let field_types = this.db.field_types(v.into());
                            for (f, _) in data.fields().iter() {
                                let offset =
                                    l.fields.offset(u32::from(f.into_raw()) as usize).bytes_usize();
                                let ty = field_types[f]
                                    .ty()
                                    .instantiate(this.interner(), subst)
                                    .skip_norm_wip();
                                let size = this.layout(ty)?.size.bytes_usize();
                                rec(
                                    this,
                                    &bytes[offset..offset + size],
                                    ty,
                                    locals,
                                    mm,
                                    stack_depth_limit - 1,
                                    shared,
                                )?;
                            }
                        }
                    }
                    // Nothing says which bytes of a union are a pointer, so every word that is
                    // the address of something is taken to be one.
                    AdtId::UnionId(_) => {
                        let layout = this.layout(ty)?;
                        if layout.align.abi.bytes_usize().is_multiple_of(this.ptr_size()) {
                            let pointer =
                                Ty::new_imm_ptr(this.interner(), Ty::new_unit(this.interner()));
                            for word in bytes.chunks_exact(this.ptr_size()) {
                                let depth = stack_depth_limit - 1;
                                rec(this, word, pointer, locals, mm, depth, shared)?;
                            }
                        }
                    }
                },
                TyKind::Alias(_, AliasTy { kind: AliasTyKind::Projection { .. }, .. }) => {
                    let mut ocx = ObligationCtxt::new(&this.infcx);
                    let ty = ocx
                        .structurally_normalize_ty(
                            &ObligationCause::dummy(),
                            this.param_env.param_env,
                            ty,
                        )
                        // alloc: error path
                        .map_err(|_| MirEvalError::NotSupported("couldn't normalize".to_owned()))?;

                    rec(this, bytes, ty, locals, mm, stack_depth_limit - 1, shared)?;
                }
                // A closure's captures, as a struct's fields.
                TyKind::Closure(_, args) => {
                    let layout = this.layout(ty)?;
                    let upvars = args.as_closure().tupled_upvars_ty();
                    for (i, ty) in upvars.tuple_fields().iter().enumerate() {
                        let offset = layout.fields.offset(i).bytes_usize();
                        let size = this.layout(ty)?.size.bytes_usize();
                        rec(
                            this,
                            &bytes[offset..offset + size],
                            ty,
                            locals,
                            mm,
                            stack_depth_limit - 1,
                            shared,
                        )?;
                    }
                }
                _ => (),
            }
            Ok(())
        }
        /// Keeps the whole allocation `addr` is in, the type of which a raw pointer does not
        /// say, and what every word of it that is in another allocation or a static points
        /// to: the reader of the map takes each such word to be a pointer, as it is in a
        /// `NonNull<()>` to a value with a `&str` in it (std's `io::Error` of a message).
        fn behind_raw<'db, M: Machine<'db>>(
            this: &Evaluator<'db, M>,
            addr: usize,
            mm: &mut ComplexMemoryMap<'db>,
            stack_depth_limit: usize,
            shared: bool,
        ) -> Result<'db, ()> {
            if stack_depth_limit.checked_sub(1).is_none() {
                return Err(MirEvalError::StackOverflow);
            }
            let pointee = this.static_memory.iter().find(|&&(start, size, _)| {
                addr.checked_sub(start).is_some_and(|offset| offset <= size)
            });
            if let Some(&(start, size, id)) = pointee {
                let pointee = crate::StaticMemory { start, size, id };
                if !mm.statics.contains(&pointee) {
                    mm.statics.push(pointee);
                }
                return Ok(());
            }
            this.may_be_in_value(addr, shared)?;
            // A pointer one past an allocation's end is to that allocation: the heap leaves a
            // gap after each block, so it is never also the start of the next.
            let allocation = this.allocation_at(addr).or_else(|| {
                let it = this.allocation_at(addr.checked_sub(1)?)?;
                (addr - it.start == it.size).then_some(it)
            });
            let Some(Allocated { start, size, align, .. }) = allocation else {
                return Ok(());
            };
            if mm.memory.get(&start).is_some_and(|it| it.len() >= size) {
                return Ok(());
            }
            let Ok(memory) =
                this.read_memory(Address::from_usize(start, this.cached_ptr_size), size)
            else {
                return Ok(());
            };
            mm.memory.insert(start, memory);
            mm.note_align(start, align);
            for word in memory.chunks_exact(this.ptr_size()) {
                let inner = from_bytes!(usize, this.cached_ptr_size, word);
                if inner != 0 {
                    behind_raw(this, inner, mm, stack_depth_limit - 1, shared)?;
                }
            }
            Ok(())
        }
        let mut mm = ComplexMemoryMap::default();
        rec(self, bytes, ty, locals, &mut mm, self.stack_depth_limit - 1, false)?;
        Ok(mm)
    }

    /// The id this evaluation has for what `id`, an id of the evaluation that made `source`,
    /// names: the same type, and for a vtable the same `dyn` type; and whether it is a
    /// vtable's.
    fn own_id(&mut self, source: &MemoryMap<'db>, id: usize) -> Result<'db, (usize, bool)> {
        let MemoryMap::Complex(map) = source else {
            return Err(MirEvalError::InvalidVTableId(id));
        };
        let Some((ty, dyn_ty)) = map.vtable.entry(id) else {
            return Err(MirEvalError::InvalidVTableId(id));
        };
        Ok((self.vtable_map.intern(ty, dyn_ty), dyn_ty.is_some()))
    }

    /// The words of the constant `source`, copied to `value` (`size` bytes of it) and to the
    /// memory `patch_map` moved, that are a function or a vtable by their provenance, each
    /// with where it is now and the id it holds, of `source`'s evaluation. The walk by type
    /// of [`Self::patch_addresses`] does not see those its types do not name (behind a
    /// pointer cast to another type, in a union); [`Self::own_ids`] gives each this
    /// evaluation's id, as rustc keeps a pointer's provenance whatever type it is copied as.
    fn ids_by_provenance(
        &self,
        source: &MemoryMap<'db>,
        patch_map: &HashMap<usize, usize, std::alloc::Global>,
        value: Address,
        value_bytes: &[u8],
        size: usize,
    ) -> Result<'db, SmallVec<[(Address, usize); 4]>> {
        let mut ids = SmallVec::new();
        let MemoryMap::Complex(map) = source else { return Ok(ids) };
        if map.vtable.is_empty() {
            return Ok(ids);
        }
        let word = self.ptr_size();
        let id_at = |bytes: &[u8], at: usize| {
            let bytes = bytes.get(at..at + word)?;
            let id = usize_of(bytes, word)?;
            map.vtable.entry(id).map(|_| id)
        };
        for &at in source.value_pointers() {
            if at + word <= size
                && let Some(id) = id_at(value_bytes, at)
            {
                ids.push((value.offset(at), id));
            }
        }
        for (&start, bytes) in map.memory.iter() {
            let Some(&moved) = patch_map.get(&start) else { continue };
            for &pointer in source.pointers_in(start..start + bytes.len()) {
                let at = pointer - start;
                if let Some(id) = id_at(bytes, at) {
                    ids.push((Address::from_usize(moved + at, self.cached_ptr_size), id));
                }
            }
        }
        Ok(ids)
    }

    /// Writes this evaluation's id over each of `ids`, with the provenance of a vtable or a
    /// function as its entry says.
    fn own_ids(
        &mut self,
        source: &MemoryMap<'db>,
        ids: SmallVec<[(Address, usize); 4]>,
    ) -> Result<'db, ()> {
        for (at, id) in ids {
            match self.own_id(source, id)? {
                (id, true) => self.write_vtable_pointer(at, id)?,
                (id, false) => self.write_function_pointer(at, id)?,
            }
        }
        Ok(())
    }

    fn patch_addresses(
        &mut self,
        moved: &mut Relocation<'db>,
        patch_map: &HashMap<usize, usize, std::alloc::Global>,
        source: &MemoryMap<'db>,
        ty_of_bytes: impl Fn(&[u8]) -> Result<'db, Ty<'db>> + Copy,
        addr: Address,
        ty: Ty<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, ()> {
        let layout = self.layout(ty)?;
        let my_size = self.size_of_sized(ty, locals, "value to patch address")?;
        // Nothing is an address in a value without bytes, however many elements it has.
        if my_size == 0 {
            return Ok(());
        }
        // A piece two pointers of the constant share is reached from each: walked once at a
        // type, as what it holds is this evaluation's after the first walk ([`Relocation`]).
        let word = self.cached_ptr_size;
        if !moved.walked.insert((addr.to_usize(word), ty)) {
            return Ok(());
        }
        use rustc_type_ir::TyKind;
        // The vtable of a `dyn` pointer is the id the value's own evaluation gave the type it
        // points at and the trait, and this one has another for them: of a pointee that
        // normalizes to a `dyn` (`&<T as Tr>::Obj`) or ends in one (`&Wrap<dyn Tr>`) too.
        if let TyKind::RawPtr(t, _) | TyKind::Ref(_, t, _) = ty.kind()
            && let TyKind::Dynamic(..) = self.struct_tail(t)?.kind()
        {
            let at = addr.offset(self.ptr_size());
            if moved.words.insert(at.to_usize(word), Moved::Piece).is_none() {
                let id = from_bytes!(usize, word, self.peek_memory(at, self.ptr_size())?);
                let (new_id, _) = self.own_id(source, id)?;
                self.write_vtable_pointer(at, new_id)?;
            }
        }
        // Whether the pointer at `addr` is already this evaluation's: moved when another type
        // reached the same word (a union's word as `*const ()`), so it is not moved again.
        let mut already = false;
        if let TyKind::RawPtr(..) | TyKind::Ref(..) = ty.kind() {
            match moved.words.get(&addr.to_usize(word)).copied() {
                // It points into this evaluator's copy of a static, which is not the constant's.
                Some(Moved::Static) => return Ok(()),
                Some(Moved::Piece) => already = true,
                None => {
                    // A pointer into a static points into this evaluator's copy of it.
                    let current =
                        from_bytes!(usize, word, self.peek_memory(addr, self.ptr_size())?);
                    if let Some((id, offset)) = source.static_at(current) {
                        moved.words.insert(addr.to_usize(word), Moved::Static);
                        let location = self.eval_static(id, locals)?;
                        let start =
                            from_bytes!(usize, word, self.peek_memory(location, self.ptr_size())?);
                        return self.write_root_pointer(addr, start + offset);
                    }
                    moved.words.insert(addr.to_usize(word), Moved::Piece);
                }
            }
        }
        match ty.kind() {
            TyKind::Ref(_, t, _) => {
                let t = self.normalized(t, "the pointee of a reference in a constant")?;
                let size = self.size_align_of(t, locals)?;
                match size {
                    Some(_) => {
                        let current = from_bytes!(
                            usize,
                            self.cached_ptr_size,
                            self.peek_memory(addr, my_size)?
                        );
                        let patched = match patch_map.get(&current).filter(|_| !already) {
                            Some(it) => {
                                self.write_root_pointer(addr, *it)?;
                                *it
                            }
                            None if already => current,
                            None => {
                                if M::CHECKS {
                                    self.write_root_pointer(addr, current)?;
                                }
                                current
                            }
                        };
                        self.patch_addresses(
                            moved,
                            patch_map,
                            source,
                            ty_of_bytes,
                            Address::from_usize(patched, self.cached_ptr_size),
                            t,
                            locals,
                        )?;
                    }
                    None => {
                        let bytes = self.peek_memory(addr, my_size)?;
                        let (current, metadata) = bytes.split_at(my_size / 2);
                        let metadata = OwnedBytes::from_slice(metadata);
                        let current = from_bytes!(usize, self.cached_ptr_size, current);
                        let patched = match patch_map.get(&current).filter(|_| !already) {
                            Some(it) => {
                                self.write_root_pointer(addr, *it)?;
                                *it
                            }
                            None if already => current,
                            None => {
                                if M::CHECKS {
                                    self.write_root_pointer(addr, current)?;
                                }
                                current
                            }
                        };
                        let patched = Address::from_usize(patched, self.cached_ptr_size);
                        if let TyKind::Dynamic(..) = t.kind() {
                            let concrete = self.vtable_map.ty_of_bytes(&metadata)?;
                            self.patch_addresses(
                                moved,
                                patch_map,
                                source,
                                ty_of_bytes,
                                patched,
                                concrete,
                                locals,
                            )?;
                        } else if let TyKind::Adt(..) | TyKind::Tuple(..) = t.kind() {
                            // A struct or tuple with an unsized tail: its sized fields and its
                            // tail's (a slice's elements, a `dyn`'s concrete value), where
                            // building the memory map found them.
                            for (offset, part, count) in self.unsized_parts(t, &metadata)? {
                                let size = self.size_of_sized(
                                    part,
                                    locals,
                                    "a part of an unsized value to patch",
                                )?;
                                if size == 0 {
                                    continue;
                                }
                                for i in 0..count {
                                    self.patch_addresses(
                                        moved,
                                        patch_map,
                                        source,
                                        ty_of_bytes,
                                        patched.offset(offset + i * size),
                                        part,
                                        locals,
                                    )?;
                                }
                            }
                        } else if let TyKind::Slice(inner) = t.kind() {
                            let len = from_bytes!(usize, self.cached_ptr_size, metadata);
                            let size = self.size_of_sized(inner, locals, "slice item to patch")?;
                            let len = if size == 0 { 0 } else { len };
                            for i in 0..len {
                                self.patch_addresses(
                                    moved,
                                    patch_map,
                                    source,
                                    ty_of_bytes,
                                    patched.offset(i * size),
                                    inner,
                                    locals,
                                )?;
                            }
                        }
                    }
                }
            }
            TyKind::Pat(base, _) => {
                self.patch_addresses(moved, patch_map, source, ty_of_bytes, addr, base, locals)?
            }
            // A raw pointer can point anywhere into the allocation that was kept for it.
            TyKind::RawPtr(..) if already => {}
            TyKind::RawPtr(..) => {
                let current = from_bytes!(
                    usize,
                    self.cached_ptr_size,
                    self.peek_memory(addr, self.ptr_size())?
                );
                if let Some((start, _)) = source.memory_at(current)
                    && let Some(new_start) = patch_map.get(&start)
                {
                    self.write_root_pointer(addr, new_start + (current - start))?;
                } else if let Ok(fn_ty) = ty_of_bytes(&word_bytes(current, self.cached_ptr_size))
                    && matches!(fn_ty.kind(), TyKind::FnDef(..) | TyKind::Closure(..))
                {
                    // A function cast to a raw pointer is still the id the value's own
                    // evaluation gave the function, and this one has another for it.
                    let new_id = self.vtable_map.id(fn_ty);
                    self.write_function_pointer(addr, new_id)?;
                }
            }
            // Of the constant's evaluation once: read again, the id is this one's.
            TyKind::FnPtr(_, _) => {
                if moved.words.insert(addr.to_usize(word), Moved::Piece).is_none() {
                    let ty = ty_of_bytes(self.peek_memory(addr, my_size)?)?;
                    let new_id = self.vtable_map.id(ty);
                    self.write_function_pointer(addr, new_id)?;
                }
            }
            TyKind::Adt(id, args) => match id.def_id() {
                // A `#[blaze::bits]` struct is an integer: no field holds an address.
                AdtId::StructId(s) if crate::layout::is_bits_struct(self.db, s) => {}
                AdtId::StructId(s) => {
                    for (i, (_, field)) in self.db.field_types(s.into()).iter().enumerate() {
                        let offset = layout.fields.offset(i).bytes_usize();
                        let ty = field.ty().instantiate(self.interner(), args).skip_norm_wip();
                        self.patch_addresses(
                            moved,
                            patch_map,
                            source,
                            ty_of_bytes,
                            addr.offset(offset),
                            ty,
                            locals,
                        )?;
                    }
                }
                // As when the memory was kept: every word that is the address of something
                // is taken to be a pointer.
                AdtId::UnionId(_) => {
                    if layout.align.abi.bytes_usize().is_multiple_of(self.ptr_size()) {
                        let pointer =
                            Ty::new_imm_ptr(self.interner(), Ty::new_unit(self.interner()));
                        for word in 0..my_size / self.ptr_size() {
                            self.patch_addresses(
                                moved,
                                patch_map,
                                source,
                                ty_of_bytes,
                                addr.offset(word * self.ptr_size()),
                                pointer,
                                locals,
                            )?;
                        }
                    }
                }
                AdtId::EnumId(e) => {
                    if let Some((ev, layout)) = detect_variant_from_bytes(
                        &layout,
                        self.db,
                        self.target_data_layout,
                        self.read_memory(addr, layout.size.bytes_usize())?,
                        e,
                    ) {
                        for (i, (_, field)) in self.db.field_types(ev.into()).iter().enumerate() {
                            let offset = layout.fields.offset(i).bytes_usize();
                            let ty = field.ty().instantiate(self.interner(), args).skip_norm_wip();
                            self.patch_addresses(
                                moved,
                                patch_map,
                                source,
                                ty_of_bytes,
                                addr.offset(offset),
                                ty,
                                locals,
                            )?;
                        }
                    }
                }
            },
            TyKind::Tuple(tys) => {
                for (id, ty) in tys.iter().enumerate() {
                    let offset = layout.fields.offset(id).bytes_usize();
                    self.patch_addresses(
                        moved,
                        patch_map,
                        source,
                        ty_of_bytes,
                        addr.offset(offset),
                        ty,
                        locals,
                    )?;
                }
            }
            TyKind::Array(inner, len) => {
                let len = match consteval::try_const_usize(self.db, len) {
                    Some(it) => it as usize,
                    None => not_supported!("non evaluatable array len in patching addresses"),
                };
                let size = self.size_of_sized(inner, locals, "inner of array")?;
                for i in 0..len {
                    self.patch_addresses(
                        moved,
                        patch_map,
                        source,
                        ty_of_bytes,
                        addr.offset(i * size),
                        inner,
                        locals,
                    )?;
                }
            }
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Slice(_)
            | TyKind::FnDef(_, _)
            | TyKind::Str
            | TyKind::Never
            | TyKind::Closure(_, _)
            | TyKind::Coroutine(_, _)
            | TyKind::CoroutineWitness(_, _)
            | TyKind::Foreign(_)
            | TyKind::Error(_)
            | TyKind::Placeholder(_)
            | TyKind::Dynamic(_, _)
            | TyKind::Bound(_, _)
            | TyKind::Infer(_)
            | TyKind::Param(_)
            | TyKind::UnsafeBinder(_)
            | TyKind::CoroutineClosure(_, _) => (),
            TyKind::Alias(..) => {
                let normalized =
                    super::normalize_or_keep(&self.infcx, self.param_env.param_env, ty);
                if !matches!(normalized.kind(), TyKind::Alias(..)) {
                    self.patch_addresses(moved, patch_map, source, ty_of_bytes, addr, normalized, locals)?;
                }
            }
        }
        Ok(())
    }

    /// Whether a call through a function pointer or a vtable in `locals`' body has its target
    /// checked for constness (`#![feature(blaze_comptime)]`, docs/v2/plan/design/comptime.md,
    /// "Phase 2b"): at compile time, outside a `const static` block, in a const context of a
    /// crate with the feature, where the static check let the call through.
    fn checks_dispatch(&self, locals: &Locals<'db>) -> bool {
        self.compile_time
            && !self.comptime
            && hir_ty::effects::dispatch_checked_at_eval(self.db, locals.body.owner)
    }

    /// Refuses the call `through` a function pointer or vtable of `target` (a function item, a
    /// closure, or a type whose `Fn*` impl of `fn_trait`'s method, with its arguments, runs)
    /// when [`Self::checks_dispatch`] and the target is not const.
    fn check_dispatched(
        &self,
        target: Ty<'db>,
        fn_trait: Option<(FunctionId, GenericArgs<'db>)>,
        through: Dispatch,
        locals: &Locals<'db>,
        span: MirSpan,
    ) -> Result<'db, ()> {
        if !self.checks_dispatch(locals) {
            return Ok(());
        }
        match target.kind() {
            TyKind::FnDef(def, args) => match def.0 {
                CallableDefId::FunctionId(func) => {
                    self.check_const_target(func, args.skip_binder(), through, locals, span)
                }
                // A constructor is const.
                CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => Ok(()),
            },
            TyKind::Closure(closure, args) => {
                if hir_ty::effects::closure_is_const(self.db, closure.0) {
                    return Ok(());
                }
                let kind = ComptimeErrorKind::NonConstTarget {
                    callee: Either::Right(closure.0),
                    args: args.store(),
                    through,
                    bounds: false,
                };
                Err(comptime_error(kind, Some((locals.body.owner, span))))
            }
            // A function pointer behind `dyn Fn*` is checked where it is called.
            TyKind::FnPtr(..) => Ok(()),
            // A type with an `Fn*` impl of its own: that impl's method runs.
            _ => match fn_trait {
                Some((func, args)) => {
                    let args = GenericArgs::new_from_iter(
                        iter::once(target.into()).chain(args.iter().skip(1)),
                    );
                    self.check_const_target(func, args, through, locals, span)
                }
                None => Ok(()),
            },
        }
    }

    /// Refuses the call of `func` with `args` reached `through` a function pointer or vtable
    /// unless it is const: `effects::const_call_error`, the rule the static check applies to a
    /// direct call. The error names the function that would run (a trait's method by its
    /// impl's).
    fn check_const_target(
        &self,
        func: FunctionId,
        args: GenericArgs<'db>,
        through: Dispatch,
        locals: &Locals<'db>,
        span: MirSpan,
    ) -> Result<'db, ()> {
        let constness = rustc_type_ir::BoundConstness::Const;
        let Some(error) =
            hir_ty::effects::const_call_error(self.db, self.param_env, constness, func, args)
        else {
            return Ok(());
        };
        let env = ParamEnvAndCrate { param_env: self.param_env.param_env, krate: self.crate_id };
        let (callee, args) = match self.db.lookup_impl_method(env, func, args) {
            (Either::Left(impl_func), impl_args) => (impl_func, impl_args),
            (Either::Right(_), _) => (func, args),
        };
        // The function that runs is const, and its `[const]` bounds are what fails; not a
        // trait's method whose impl is not const.
        let bounds = matches!(error, hir_ty::effects::NonConst::Unsatisfied(..))
            && !matches!(callee.lookup(self.db).container, ItemContainerId::TraitId(_))
            && hir_ty::effects::fn_is_const(self.db, callee);
        let kind = ComptimeErrorKind::NonConstTarget {
            callee: Either::Left(callee),
            args: args.store(),
            through,
            bounds,
        };
        Err(comptime_error(kind, Some((locals.body.owner, span))))
    }

    fn exec_fn_pointer(
        &mut self,
        bytes: Interval,
        destination: Interval,
        args: &[IntervalAndTy<'db>],
        locals: &Locals<'db>,
        then: Then,
        span: MirSpan,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        let id = from_bytes!(usize, self.cached_ptr_size, bytes.get(self)?);
        let next_ty = self.vtable_map.ty(id)?;
        self.check_dispatched(next_ty, None, Dispatch::FnPointer, locals, span)?;
        use rustc_type_ir::TyKind;
        match next_ty.kind() {
            TyKind::FnDef(def, generic_args) => self.exec_fn_def(
                def.0,
                generic_args.skip_binder(),
                destination,
                args,
                locals,
                then,
                span,
            ),
            TyKind::Closure(id, generic_args) => self.exec_closure(
                id.0,
                bytes.slice(0..0),
                generic_args,
                destination,
                args,
                locals,
                span,
                None,
                then,
            ),
            _ => Err(MirEvalError::InternalError("function pointer to non function".into())),
        }
    }

    fn exec_closure(
        &mut self,
        closure: InternedClosureId<'db>,
        closure_data: Interval,
        generic_args: GenericArgs<'db>,
        destination: Interval,
        args: &[IntervalAndTy<'db>],
        locals: &Locals<'db>,
        span: MirSpan,
        consumed: Option<Ty<'db>>,
        then: Then,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        let mir_body = super::optimize::interpreted_body(
            self.db,
            super::MirOwner::Closure(closure),
            generic_args.store(),
            self.param_env.store(),
        )
        .map_err(|it| MirEvalError::MirLowerErrorForClosure(closure, it))?;
        let by_reference =
            mir_body.locals[mir_body.param_locals[0]].ty.as_ref().as_reference().is_some();
        let closure_addr = closure_data.addr;
        let first = match by_reference {
            true => self.pointer_value(closure_addr, None)?,
            false => IntervalOrOwned::Borrowed(closure_data),
        };
        let arg_values =
            iter::once(first).chain(args.iter().map(|it| IntervalOrOwned::Borrowed(it.interval)));
        // A `Fn` or `FnMut` closure called by value through `FnOnce::call_once` is dropped
        // after the call, as rustc's `ClosureOnceShim` does: nothing else owns it then. One
        // whose body takes it by value dropped it itself.
        let dropped = consumed.filter(|_| by_reference);
        if then == Then::Nested {
            let mark = self.stack.len();
            let interval = self.interpret_mir(mir_body, arg_values, None).map_err(|e| {
                MirEvalError::InFunction(
                    // alloc: error path
                    Box::new(e),
                    // alloc: error path
                    vec![(Either::Right(closure), span, locals.body.owner)],
                )
            })?;
            destination.write_from_interval(self, interval)?;
            if let Some(closure_ty) = dropped {
                self.run_drop_glue(closure_ty, locals, closure_addr, &[], span, Then::Nested)?;
            }
            self.free_stack(mark)?;
            return Ok(None);
        }
        let (mut callee_locals, prev_stack_ptr) = self.callee_locals(mir_body, destination)?;
        let return_to = M::CHECKS.then_some(destination);
        self.enter_frame(mir_body, &mut callee_locals, arg_values, return_to)?;
        let mut frame =
            StackFrame::new(callee_locals, then, prev_stack_ptr, (span, locals.body.owner), None);
        frame.return_to = return_to;
        if let Some(closure_ty) = dropped {
            frame.drops = self.spare_drops.pop().unwrap_or_default();
            frame.drops.push(DropStep::Glue(closure_ty, closure_addr, OwnedBytes::new()));
        }
        Ok(Some(frame))
    }

    fn exec_fn_def(
        &mut self,
        def: CallableDefId,
        generic_args: GenericArgs<'db>,
        destination: Interval,
        args: &[IntervalAndTy<'db>],
        locals: &Locals<'db>,
        then: Then,
        span: MirSpan,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        match def {
            CallableDefId::FunctionId(def) => {
                if self.detect_fn_trait(def).is_some() {
                    return self.exec_fn_trait(
                        def,
                        args,
                        generic_args,
                        locals,
                        destination,
                        then,
                        span,
                    );
                }
                self.exec_fn_with_args(def, args, generic_args, locals, destination, then, span)
            }
            // A `#[blaze::bits]` tuple struct's constructor as a function value: each
            // argument's bits at its field's bit offset in the backing integer, as the MIR pass
            // packs a literal.
            CallableDefId::StructId(id) if crate::layout::is_bits_struct(self.db, id) => {
                let ty = Ty::new_adt(self.interner(), id.into(), generic_args);
                let size = self.layout(ty)?.size.bytes_usize();
                let bits = crate::layout::bits_layout(self.db, id).map_err(|err| {
                    let err = crate::layout::LayoutError::Bits(Box::new(err.clone())); // alloc: error path
                    MirEvalError::LayoutError(err, ty.store())
                })?;
                let mut backing = 0u128;
                for (field, arg) in bits.fields.iter().zip(args.iter()) {
                    let bytes = arg.interval.get(self)?;
                    let mut le = [0u8; 16];
                    let len = bytes.len().min(16);
                    le[..len].copy_from_slice(&bytes[..len]);
                    let value = u128::from_le_bytes(le);
                    let width = field.width.bits;
                    let value = if width >= 128 { value } else { value & ((1u128 << width) - 1) };
                    backing |= value.checked_shl(field.offset).unwrap_or(0);
                }
                destination.write_from_bytes(self, &backing.to_le_bytes()[..size])?;
                Ok(None)
            }
            CallableDefId::StructId(id) => {
                let (size, variant_layout, tag) =
                    self.layout_of_variant(id.into(), generic_args, locals)?;
                let result = self.construct_with_layout(
                    size,
                    &variant_layout,
                    tag,
                    args.iter().map(|it| it.interval.into()),
                )?;
                destination.write_from_bytes(self, &result)?;
                Ok(None)
            }
            CallableDefId::EnumVariantId(id) => {
                let (size, variant_layout, tag) =
                    self.layout_of_variant(id.into(), generic_args, locals)?;
                let result = self.construct_with_layout(
                    size,
                    &variant_layout,
                    tag,
                    args.iter().map(|it| it.interval.into()),
                )?;
                destination.write_from_bytes(self, &result)?;
                Ok(None)
            }
        }
    }

    fn get_mir_or_dyn_index(
        &self,
        def: FunctionId,
        generic_args: GenericArgs<'db>,
        locals: &Locals<'db>,
        span: MirSpan,
    ) -> Result<'db, MirOrDynIndex<'db>> {
        let pair = (def, generic_args);
        if let Some(r) = self.mir_or_dyn_index_cache.borrow().get(&pair) {
            return Ok(*r);
        }
        let (def, generic_args) = pair;
        let r = if let Some(self_ty_idx) =
            is_dyn_method(self.interner(), self.param_env.param_env, def, generic_args)
        {
            MirOrDynIndex::Dyn(self_ty_idx)
        } else {
            let (imp, generic_args) = self.db.lookup_impl_method(
                ParamEnvAndCrate { param_env: self.param_env.param_env, krate: self.crate_id },
                def,
                generic_args,
            );
            let imp = match imp {
                Either::Left(imp) => imp,
                // A database that records builtin derives instead of expanding them has no
                // body for the method (`hir_def::nameres::set_expand_builtin_derives`).
                Either::Right((impl_, method)) => {
                    let derive = impl_.loc(self.db).trait_.name();
                    let self_ty = generic_args.as_slice().first().copied();
                    let shown = blaze_query::db::attach(self.db, || format!("{self_ty:?}")); // alloc: error path
                    not_supported!(
                        "`{}::{}` of `{shown}`, which `#[derive({})]` implements: builtin derives \
                         are recorded unexpanded in this database, so the method has no body",
                        derive.as_str(),
                        method.name().as_str(),
                        derive.as_str()
                    )
                }
            };

            let mir_body = super::optimize::interpreted_body(
                self.db,
                super::MirOwner::Body(imp.into()),
                generic_args.store(),
                self.param_env.store(),
            )
            .map_err(|e| {
                MirEvalError::InFunction(
                    // alloc: error path
                    Box::new(MirEvalError::MirLowerError(imp, e)),
                    vec![(Either::Left(imp), span, locals.body.owner)], // alloc: error path
                )
            })?;
            MirOrDynIndex::Mir(mir_body)
        };
        self.mir_or_dyn_index_cache.borrow_mut().insert((def, generic_args), r);
        Ok(r)
    }

    /// A call of `def`, the intrinsic `intrinsic`, as MIR lowering recognized it: the
    /// evaluator's own implementation, or the fallback body where it has none.
    fn exec_intrinsic_fn(
        &mut self,
        def: FunctionId,
        intrinsic: Intrinsic,
        args: &[IntervalAndTy<'db>],
        generic_args: GenericArgs<'db>,
        locals: &Locals<'db>,
        destination: Interval,
        then: Then,
        span: MirSpan,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        if intrinsic == Intrinsic::CatchUnwind {
            return self.exec_catch_unwind(args, locals, destination, then, span);
        }
        self.call_then = then;
        let moved = mem::take(&mut self.moved_args);
        if self.exec_intrinsic_call(
            def,
            intrinsic,
            args,
            generic_args,
            locals,
            destination,
            span,
        )? {
            return Ok(None);
        }
        self.moved_args = moved;
        self.exec_fn_body(def, args, generic_args, locals, destination, then, span)
    }

    fn exec_fn_with_args(
        &mut self,
        def: FunctionId,
        args: &[IntervalAndTy<'db>],
        generic_args: GenericArgs<'db>,
        locals: &Locals<'db>,
        destination: Interval,
        then: Then,
        span: MirSpan,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        self.call_then = then;
        let moved = mem::take(&mut self.moved_args);
        if self.detect_and_exec_special_function(
            def,
            args,
            generic_args,
            locals,
            destination,
            span,
        )? {
            return Ok(None);
        }
        self.moved_args = moved;
        self.exec_fn_body(def, args, generic_args, locals, destination, then, span)
    }

    /// A call of `def` that runs its body: its own, or the method a `dyn` receiver dispatches
    /// it to.
    fn exec_fn_body(
        &mut self,
        mut def: FunctionId,
        args: &[IntervalAndTy<'db>],
        generic_args: GenericArgs<'db>,
        locals: &Locals<'db>,
        destination: Interval,
        then: Then,
        span: MirSpan,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        if let Some(redirect_def) = self.detect_and_redirect_special_function(def)? {
            def = redirect_def;
        }
        let arg_bytes = args.iter().map(|it| IntervalOrOwned::Borrowed(it.interval));
        match self.get_mir_or_dyn_index(def, generic_args, locals, span)? {
            MirOrDynIndex::Dyn(self_ty_idx) => {
                // In the layout of current possible receiver, which at the moment of writing this code is one of
                // `&T`, `&mut T`, `Box<T>`, `Rc<T>`, `Arc<T>`, and `Pin<P>` where `P` is one of possible receivers,
                // the vtable is exactly in the `[ptr_size..2*ptr_size]` bytes. So we can use it without branching on
                // the type.
                let first_arg = arg_bytes.clone().next().unwrap(); // clone: a slice iterator
                let first_arg = first_arg.get(self)?;
                let ty = self
                    .vtable_map
                    .ty_of_bytes(&first_arg[self.ptr_size()..self.ptr_size() * 2])?;
                let mut args_for_target: SmallVec<[IntervalAndTy<'db>; 4]> =
                    SmallVec::from_slice(args);
                args_for_target[0] = IntervalAndTy {
                    interval: args_for_target[0].interval.slice(0..self.ptr_size()),
                    ty,
                };
                let generics_for_target = GenericArgs::new_from_iter(
                    generic_args
                        .iter()
                        .enumerate()
                        .map(|(i, it)| if i == self_ty_idx { ty.into() } else { it }),
                );
                if self.checks_dispatch(locals) {
                    let dyn_ty = generic_args.as_slice()[self_ty_idx].expect_ty().store();
                    let through = Dispatch::Vtable { dyn_ty };
                    self.check_const_target(def, generics_for_target, through, locals, span)?;
                }
                self.exec_fn_with_args(
                    def,
                    &args_for_target,
                    generics_for_target,
                    locals,
                    destination,
                    then,
                    span,
                )
            }
            MirOrDynIndex::Mir(body) => {
                self.exec_looked_up_function(body, locals, def, arg_bytes, span, destination, then)
            }
        }
    }

    fn exec_looked_up_function(
        &mut self,
        mir_body: &'db MirBody<'db>,
        locals: &Locals<'db>,
        def: FunctionId,
        arg_bytes: impl Iterator<Item = IntervalOrOwned<M::Shadow>>,
        span: MirSpan,
        destination: Interval,
        then: Then,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        if then == Then::Nested {
            let mark = self.stack.len();
            let called = Some((def, span, locals.body.owner));
            let result = self.interpret_mir(mir_body, arg_bytes, called).map_err(|e| {
                MirEvalError::InFunction(
                    // alloc: error path
                    Box::new(e),
                    // alloc: error path
                    vec![(Either::Left(def), span, locals.body.owner)],
                )
            })?;
            destination.write_from_interval(self, result)?;
            self.free_stack(mark)?;
            return Ok(None);
        }
        let span = (span, locals.body.owner);
        let (mut locals, prev_stack_ptr) = self.callee_locals(mir_body, destination)?;
        let return_to = M::CHECKS.then_some(destination);
        self.enter_frame(mir_body, &mut locals, arg_bytes, return_to)?;
        let mut frame = StackFrame::new(locals, then, prev_stack_ptr, span, Some(def));
        frame.return_to = return_to;
        Ok(Some(frame))
    }

    fn exec_fn_trait(
        &mut self,
        def: FunctionId,
        args: &[IntervalAndTy<'db>],
        generic_args: GenericArgs<'db>,
        locals: &Locals<'db>,
        destination: Interval,
        then: Then,
        span: MirSpan,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        let func = args
            .first()
            .ok_or_else(|| MirEvalError::InternalError("fn trait with no arg".into()))?;
        let mut func_ty = func.ty;
        let mut func_data = func.interval;
        // The `dyn Fn*` the callee was reached through, if it was.
        let mut through_dyn = None;
        while let TyKind::Ref(_, z, _) = func_ty.kind() {
            func_ty = z;
            if matches!(func_ty.kind(), TyKind::Dynamic(..)) {
                let id = from_bytes!(
                    usize,
                    self.cached_ptr_size,
                    &func_data.get(self)?[self.ptr_size()..self.ptr_size() * 2]
                );
                func_data = func_data.slice(0..self.ptr_size());
                through_dyn = Some(func_ty);
                func_ty = self.vtable_map.ty(id)?;
            }
            let size = self.size_of_sized(func_ty, locals, "self type of fn trait")?;
            func_data = Interval {
                addr: Address::from_bytes(func_data.get(self)?, self.cached_ptr_size)?,
                size,
            };
        }
        if let Some(dyn_ty) = through_dyn {
            let through = Dispatch::Vtable { dyn_ty: dyn_ty.store() };
            let fn_trait = Some((def, generic_args));
            self.check_dispatched(func_ty, fn_trait, through, locals, span)?;
        }
        match func_ty.kind() {
            TyKind::FnDef(def, subst) => self.exec_fn_def(
                def.0,
                subst.skip_binder(),
                destination,
                &args[1..],
                locals,
                then,
                span,
            ),
            TyKind::FnPtr(..) => {
                self.exec_fn_pointer(func_data, destination, &args[1..], locals, then, span)
            }
            TyKind::Closure(closure, subst) => {
                let consumed = (Some(def) == self.cached_fn_once_trait_func && func_ty == func.ty)
                    .then_some(func_ty);
                self.exec_closure(
                    closure.0,
                    func_data,
                    GenericArgs::new_from_slice(subst.as_closure().parent_args()),
                    destination,
                    &args[1..],
                    locals,
                    span,
                    consumed,
                    then,
                )
            }
            _ => {
                // try to execute the manual impl of `FnTrait` for structs (nightly feature used in std)
                let arg0 = func;
                let args = &args[1..];
                let arg1 = {
                    let ty = Ty::new_tup_from_iter(self.interner(), args.iter().map(|it| it.ty));
                    let layout = self.layout(ty)?;
                    let result = self.construct_with_layout(
                        layout.size.bytes_usize(),
                        &layout,
                        None,
                        args.iter().map(|it| IntervalOrOwned::Borrowed(it.interval)),
                    )?;
                    // FIXME: there is some leak here
                    let size = layout.size.bytes_usize();
                    let addr = self.heap_allocate(size, layout.align.bytes() as usize)?;
                    self.write_memory(addr, &result)?;
                    IntervalAndTy { interval: Interval { addr, size }, ty }
                };
                self.exec_fn_with_args(
                    def,
                    &[*arg0, arg1],
                    generic_args,
                    locals,
                    destination,
                    then,
                    span,
                )
            }
        }
    }

    fn eval_static(&mut self, st: StaticId, locals: &Locals<'db>) -> Result<'db, Address> {
        if let Some(o) = self.static_locations.get(&st) {
            return Ok(*o);
        };
        if let Some(o) = self.thread_statics.get(&st) {
            return Ok(*o);
        }
        if let Some(location) = self.preset_static_location(st)? {
            return Ok(location);
        }
        let static_data = StaticSignature::of(self.db, st);
        if hir_def::attrs::AttrFlags::query(self.db, st.into())
            .contains(hir_def::attrs::AttrFlags::THREAD_LOCAL)
        {
            // Each thread's copy starts from the static's value when it first uses it.
            let allocation = self.db.const_eval_static(st).map_err(|e| {
                // alloc: error path
                MirEvalError::ConstEvalError(static_data.name.as_str().to_owned(), Box::new(e))
            })?;
            let (size, align) =
                self.size_align_of_sized(allocation.ty, locals, "the value of a static")?;
            let memory = self.heap_allocate(size, align)?;
            let location = self.heap_allocate(self.ptr_size(), self.ptr_size())?;
            self.write_pointer(location, memory)?;
            self.thread_statics.insert(st, location);
            let nested = self.static_nested_mutability(st);
            let db = self.db;
            // alloc: error path
            let whose = || format!("the static `{}`", GeneralConstId::StaticId(st).name(db));
            self.write_allocation(locals, allocation, &whose, memory, size, nested)?;
            self.freeze_static(st, allocation.ty, Interval::new(memory, size));
            self.record_static(memory, size, st)?;
            return Ok(location);
        }
        let is_read = self.read_statics.as_ref().is_none_or(|read| read.contains(&st));
        let unevaluated = if is_read || static_data.flags.contains(StaticFlags::EXTERN) {
            None
        } else {
            let ty = self.db.type_for_static(st).instantiate_identity().skip_norm_wip();
            self.size_align_of(ty, locals)?
        };
        let result = if let Some((size, align)) = unevaluated {
            let addr = self.heap_allocate(size, align)?;
            if let Heap(start) = addr.at() {
                self.unevaluated_statics.push((start, size, st));
            }
            Interval::new(addr, size)
        } else if !static_data.flags.contains(StaticFlags::EXTERN) {
            let allocation = self.db.const_eval_static(st).map_err(|e| {
                // alloc: error path
                MirEvalError::ConstEvalError(static_data.name.as_str().to_owned(), Box::new(e))
            })?;
            let (size, align) =
                self.size_align_of_sized(allocation.ty, locals, "the value of a static")?;
            let result = Interval::new(self.heap_allocate(size, align)?, size);
            // Known before its value is written: what it points to may point back at it.
            let location = self.locate_static(st, result.addr)?;
            let nested = self.static_nested_mutability(st);
            let db = self.db;
            // alloc: error path
            let whose = || format!("the static `{}`", GeneralConstId::StaticId(st).name(db));
            self.write_allocation(locals, allocation, &whose, result.addr, size, nested)?;
            self.initialized(result);
            self.freeze_static(st, allocation.ty, result);
            self.record_static(result.addr, size, st)?;
            return Ok(location);
        } else {
            let ty = self.db.type_for_static(st).instantiate_identity().skip_norm_wip();
            let Some((size, align)) = self.size_align_of(ty, locals)? else {
                not_supported!("unsized extern static");
            };
            let weak = hir_def::attrs::AttrFlags::linkage(self.db, st.into())
                .is_some_and(|it| it.as_str() == "extern_weak");
            if !self.compile_time && !weak {
                // alloc: error path
                return Err(MirEvalError::NotSupported(format!(
                    "extern static `{}`, which only the linked program has",
                    static_data.name.as_str()
                )));
            }
            let addr = self.heap_allocate(size, align)?;
            let result = Interval::new(addr, size);
            if self.compile_time {
                // Its address is a value at compile time; reading it is not (rustc's
                // `ExternStatic`), which `check_static_is_evaluated` refuses.
                if let Heap(start) = addr.at() {
                    self.extern_statics.push((start, size, st));
                }
            } else {
                // Null: an `extern_weak` one is unresolved, so std's `Option<fn>` for
                // `__cxa_thread_atexit_impl` is `None` and it registers thread-local destructors
                // through a pthread key, which the thread shims run.
                self.initialized(result);
            }
            result
        };
        self.record_static(result.addr, result.size, st)?;
        self.locate_static(st, result.addr)
    }

    /// Whether what the value of `st` points to may be written: only for a `static mut`
    /// (rustc's `intern_const_alloc_recursive`, which accepts `static mut X: &mut [i32] =
    /// &mut [42]`).
    fn static_nested_mutability(&self, st: StaticId) -> Mutability {
        match StaticSignature::of(self.db, st).flags.contains(StaticFlags::MUTABLE) {
            true => Mutability::Mut,
            false => Mutability::Not,
        }
    }

    /// The memory of `st`, of type `ty`, its initial value written: read-only unless it is a
    /// `static mut` or its type has interior mutability (rustc's interning).
    fn freeze_static(&self, st: StaticId, ty: Ty<'db>, memory: Interval) {
        if !M::CHECKS || self.static_nested_mutability(st) == Mutability::Mut {
            return;
        }
        if is_freeze(&self.infcx, ty, self.param_env.param_env) {
            self.freeze(memory);
        }
    }

    /// Records where the memory of a static is, and gives the address of a pointer to it.
    fn locate_static(&mut self, st: StaticId, memory: Address) -> Result<'db, Address> {
        let addr = self.heap_allocate(self.ptr_size(), self.ptr_size())?;
        self.write_pointer(addr, memory)?;
        self.initialized(Interval::new(addr, self.ptr_size()));
        self.static_locations.insert(st, addr);
        Ok(addr)
    }

    fn const_eval_discriminant(&self, variant: EnumVariantId) -> Result<'db, i128> {
        self.db.const_eval_discriminant(variant).map_err(|e| {
            let edition = self.crate_id.data(self.db).edition;
            let name = super::lower::variant_path(self.db, variant, edition);
            // alloc: error path
            MirEvalError::ConstEvalError(name, Box::new(e))
        })
    }

    fn drop_place(
        &mut self,
        place: &StoredPlace,
        locals: &mut Locals<'db>,
        span: MirSpan,
        then: Then,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        let (addr, ty, metadata) = self.place_addr_and_ty_and_metadata(place, locals)?;
        let metadata = match metadata {
            Some(it) => OwnedBytes::from_slice(it.get(self)?),
            None => OwnedBytes::new(),
        };
        self.run_drop_glue(ty, locals, addr, &metadata, span, then)
    }

    /// Drops the value of type `ty` at `addr`, as rustc's `drop_in_place::<ty>`: its `Drop`
    /// impl, then its parts in order (a struct's or tuple's fields, a closure's captures, the
    /// fields of the variant an enum holds, an array's or slice's elements). A `Box` drops what
    /// it points to before its `Drop` frees it.
    ///
    /// The parts wait on a stack of steps rather than the native one: a list of a thousand
    /// boxes is a thousand levels deep. A `Drop` impl's call is a frame going on at `then`
    /// that finishes the drop once it returns; with [`Then::Nested`] the drop runs to its end.
    fn run_drop_glue(
        &mut self,
        ty: Ty<'db>,
        locals: &Locals<'db>,
        addr: Address,
        metadata: &[u8],
        span: MirSpan,
        then: Then,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        let mut steps = self.spare_drops.pop().unwrap_or_default();
        steps.push(DropStep::Glue(ty, addr, OwnedBytes::from_slice(metadata)));
        self.drop_steps(steps, locals, span, then)
    }

    /// `steps`, done with, back to [`Self::spare_drops`] if it holds memory.
    fn keep_drops(&mut self, mut steps: Vec<DropStep<'db>>) {
        if steps.capacity() != 0 {
            steps.clear();
            self.spare_drops.push(steps);
        }
    }

    /// Runs `steps`, last first, until one calls a `Drop` impl: the frame of that call, with
    /// the steps left to run once it returns.
    fn drop_steps(
        &mut self,
        mut steps: Vec<DropStep<'db>>,
        locals: &Locals<'db>,
        span: MirSpan,
        then: Then,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        if self.lang_items().Drop_drop.is_none() {
            self.keep_drops(steps);
            return Ok(None);
        }
        while let Some(step) = steps.pop() {
            match step {
                DropStep::Glue(ty, addr, metadata) => {
                    self.drop_glue_step(ty, locals, addr, &metadata, false, &mut steps)?
                }
                DropStep::Fields(ty, addr, metadata) => {
                    self.drop_glue_step(ty, locals, addr, &metadata, true, &mut steps)?
                }
                DropStep::DropImpl(ty, addr) => {
                    if let Some(mut frame) = self.call_drop_impl(ty, locals, addr, span, then)? {
                        frame.drops = steps;
                        return Ok(Some(frame));
                    }
                }
            }
        }
        self.keep_drops(steps);
        Ok(None)
    }

    /// One step of [`Self::drop_steps`]: pushes what dropping the value does onto `steps`,
    /// last first, in place: they go on in order, then that run is reversed. With
    /// `fields_only`, an ADT's `Drop` impl ran already.
    fn drop_glue_step(
        &mut self,
        ty: Ty<'db>,
        locals: &Locals<'db>,
        addr: Address,
        metadata: &[u8],
        fields_only: bool,
        steps: &mut Vec<DropStep<'db>>,
    ) -> Result<'db, ()> {
        if !self.has_drop_glue(ty) {
            return Ok(());
        }
        // A field's declared type can be a projection (`Cow::Owned`'s `<B as ToOwned>::Owned`,
        // `FlatMap`'s `Option<U::IntoIter>`), which `has_drop_glue` saw through.
        let ty = match ty.kind() {
            TyKind::Alias(..) => {
                let normalized = ObligationCtxt::new(&self.infcx)
                    .structurally_normalize_ty(
                        &ObligationCause::dummy(),
                        self.param_env.param_env,
                        ty,
                    )
                    .map_err(|_| {
                        // alloc: error path
                        MirEvalError::NotSupported(format!(
                            "dropping {ty:?}, which does not normalize"
                        ))
                    })?;
                if matches!(normalized.kind(), TyKind::Alias(..) | TyKind::Infer(..)) {
                    not_supported!("dropping {ty:?}, which does not normalize");
                }
                normalized
            }
            _ => ty,
        };
        let base = steps.len();
        match ty.kind() {
            TyKind::Adt(adt_def, subst) => {
                let id = adt_def.def_id();
                if let AdtId::StructId(s) = id
                    && StructSignature::of(self.db, s).flags.contains(StructFlags::IS_MANUALLY_DROP)
                {
                    return Ok(());
                }
                let layout = self.layout_adt(id, subst)?;
                if adt_def.is_box() {
                    let AdtId::StructId(s) = id else {
                        not_supported!("a box that is not a struct")
                    };
                    let field_types = self.db.field_types(s.into());
                    let Some((_, pointer)) = field_types.iter().next() else {
                        not_supported!("a box with no pointer");
                    };
                    let pointer = pointer.ty().instantiate(self.interner(), subst).skip_norm_wip();
                    let size = self.size_of_sized(pointer, locals, "pointer of a box")?;
                    let offset = layout.fields.offset(0).bytes_usize();
                    let bytes = self.read_memory(addr.offset(offset), size)?;
                    let (data, contents_metadata) = bytes.split_at(self.ptr_size());
                    let contents = Address::from_bytes(data, self.cached_ptr_size)?;
                    let contents_metadata = OwnedBytes::from_slice(contents_metadata);
                    steps.push(DropStep::Glue(subst.type_at(0), contents, contents_metadata));
                    steps.push(DropStep::DropImpl(ty, addr));
                } else if !fields_only {
                    steps.push(DropStep::Fields(ty, addr, OwnedBytes::from_slice(metadata)));
                    steps.push(DropStep::DropImpl(ty, addr));
                    return Ok(());
                }
                let bytes;
                let (variant, layout): (VariantId, Cow<'_, Layout>) = match id {
                    AdtId::StructId(s) => (s.into(), Cow::Borrowed(&*layout)),
                    AdtId::UnionId(_) => return Ok(()),
                    AdtId::EnumId(e) => {
                        bytes = self.read_memory(addr, layout.size.bytes_usize())?;
                        match detect_variant_from_bytes(
                            &layout,
                            self.db,
                            self.target_data_layout,
                            bytes,
                            e,
                        ) {
                            Some((variant, layout)) => (variant.into(), layout),
                            None => not_supported!("dropping {ty:?}: no variant has its tag"),
                        }
                    }
                };
                let field_types = self.db.field_types(variant);
                let fields = variant.fields(self.db).fields();
                let last = fields.iter().count().checked_sub(1);
                for (index, (field, _)) in fields.iter().enumerate() {
                    let offset =
                        layout.fields.offset(u32::from(field.into_raw()) as usize).bytes_usize();
                    let ty =
                        field_types[field].ty().instantiate(self.interner(), subst).skip_norm_wip();
                    // The metadata of an unsized struct is its last field's (`RefCell<dyn FnMut()>`).
                    let metadata = match Some(index) == last {
                        true => OwnedBytes::from_slice(metadata),
                        false => OwnedBytes::new(),
                    };
                    steps.push(DropStep::Glue(ty, addr.offset(offset), metadata));
                }
            }
            TyKind::Tuple(fields) => self.field_steps(ty, fields.iter(), addr, steps)?,
            TyKind::Closure(_, args) => {
                let upvars = args.as_closure().tupled_upvars_ty();
                self.field_steps(ty, upvars.tuple_fields().iter(), addr, steps)?;
            }
            TyKind::Array(inner, len) => {
                let Some(len) = try_const_usize(self.db, len) else {
                    not_supported!("dropping an array of no evaluated length");
                };
                self.element_steps(inner, len as usize, locals, addr, steps)?;
            }
            TyKind::Slice(inner) => {
                let len = from_bytes!(usize, self.cached_ptr_size, metadata);
                self.element_steps(inner, len, locals, addr, steps)?;
            }
            TyKind::Dynamic(..) => {
                if metadata.is_empty() {
                    not_supported!("dropping {ty:?} with no vtable");
                }
                let concrete_ty = self.vtable_map.ty_of_bytes(metadata)?;
                steps.push(DropStep::Glue(concrete_ty, addr, OwnedBytes::new()));
            }
            _ => not_supported!("drop glue of {ty:?}"),
        }
        steps[base..].reverse();
        Ok(())
    }

    /// Whether dropping a value of `ty` does anything, once per type.
    fn has_drop_glue(&mut self, ty: Ty<'db>) -> bool {
        if let Some(&it) = self.drop_glue.get(&ty) {
            return it;
        }
        let it = has_drop_glue(&self.infcx, ty, self.param_env.param_env) != DropGlue::None;
        self.drop_glue.insert(ty, it);
        it
    }

    /// `<ty as Drop>::drop` on the value at `addr`, if `ty` has a `Drop` impl: its frame,
    /// going on at `then`, unless that is [`Then::Nested`] and it ran.
    fn call_drop_impl(
        &mut self,
        ty: Ty<'db>,
        locals: &Locals<'db>,
        addr: Address,
        span: MirSpan,
        then: Then,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        let Some(drop_fn) = self.lang_items().Drop_drop else { return Ok(None) };
        let generic_args = GenericArgs::new_from_slice(&[ty.into()]);
        let (drop_impl, drop_args) = self.db.lookup_impl_method(
            ParamEnvAndCrate { param_env: self.param_env.param_env, krate: self.crate_id },
            drop_fn,
            generic_args,
        );
        if let Either::Left(drop_impl) = drop_impl
            && matches!(drop_impl.lookup(self.db).container, ItemContainerId::ImplId(_))
        {
            let body = super::optimize::interpreted_body(
                self.db,
                super::MirOwner::Body(drop_impl.into()),
                drop_args.store(),
                self.param_env.store(),
            )
            .map_err(|it| {
                // The error stack holds the frames that called, not the one running, so the
                // drop is named here with where it was made.
                MirEvalError::InFunction(
                    // alloc: error path
                    Box::new(MirEvalError::MirLowerError(drop_impl, it)),
                    vec![(Either::Left(drop_impl), span, locals.body.owner)], // alloc: error path
                )
            })?;
            let this = self.pointer_value(addr, None)?;
            return self.exec_looked_up_function(
                body,
                locals,
                drop_impl,
                iter::once(this),
                span,
                Interval { addr: Address::invalid(0), size: 0 },
                then,
            );
        }
        Ok(None)
    }

    /// The drops of the fields of the tuple or closure `ty` at `addr`, of types `fields`, in order.
    fn field_steps(
        &mut self,
        ty: Ty<'db>,
        fields: impl Iterator<Item = Ty<'db>>,
        addr: Address,
        parts: &mut Vec<DropStep<'db>>,
    ) -> Result<'db, ()> {
        let layout = self.layout(ty)?;
        for (i, field) in fields.enumerate() {
            let offset = layout.fields.offset(i).bytes_usize();
            parts.push(DropStep::Glue(field, addr.offset(offset), OwnedBytes::new()));
        }
        Ok(())
    }

    /// The drops of `len` elements of type `inner` from `addr` on, first to last.
    fn element_steps(
        &mut self,
        inner: Ty<'db>,
        len: usize,
        locals: &Locals<'db>,
        addr: Address,
        parts: &mut Vec<DropStep<'db>>,
    ) -> Result<'db, ()> {
        if !self.has_drop_glue(inner) {
            return Ok(());
        }
        let size = self.size_of_sized(inner, locals, "element to drop")?;
        let element = |i: usize| DropStep::Glue(inner, addr.offset(i * size), OwnedBytes::new());
        parts.extend((0..len).map(element));
        Ok(())
    }

    fn write_to_stdout(&mut self, interval: Interval) -> Result<'db, ()> {
        // alloc: the program's output, at the edge
        self.stdout.extend(interval.get(self)?.to_vec());
        Ok(())
    }

    fn write_to_stderr(&mut self, interval: Interval) -> Result<'db, ()> {
        // alloc: the program's output, at the edge
        self.stderr.extend(interval.get(self)?.to_vec());
        Ok(())
    }
}

/// The body an evaluation was made for, as an error names it: "the constant `GOAL`".
fn evaluated(db: &Db, owner: InferBodyId<'_>) -> String {
    // alloc: error text, every arm
    match owner {
        InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(id)) => {
            format!("the constant `{}`", GeneralConstId::ConstId(id).name(db))
        }
        InferBodyId::DefWithBodyId(DefWithBodyId::StaticId(id)) => {
            format!("the static `{}`", GeneralConstId::StaticId(id).name(db))
        }
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(id)) => format!(
            "the function `{}`",
            hir_def::signatures::FunctionSignature::of(db, id).name.as_str()
        ),
        InferBodyId::DefWithBodyId(DefWithBodyId::VariantId(_)) => {
            "an enum variant's discriminant".to_owned()
        }
        InferBodyId::AnonConstId(_) => "an anonymous constant".to_owned(),
    }
}

pub fn render_const_using_debug_impl<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    c: Allocation<'db>,
    ty: Ty<'db>,
) -> Result<'db, String> {
    let mut evaluator = Evaluator::new(db, owner, None)?;
    let locals = &Locals {
        ptr: ArenaMap::new(),
        body: db
            .runtime_mir_body(owner)
            // alloc: error path
            .map_err(|_| MirEvalError::NotSupported("unreachable".to_owned()))?,
    };
    // alloc: error path
    let whose = || format!("a constant rendered for {}", evaluated(db, owner));
    let data = evaluator.allocate_allocation_in_heap(locals, c, &whose, Mutability::Not)?;
    let lang_items = evaluator.interner().lang_items();
    let resolver = owner.resolver(db);
    let Some(debug_fmt_fn) = lang_items.Debug_fmt else {
        not_supported!("core::fmt::Debug::fmt not found");
    };
    let ptr_size = evaluator.ptr_size();
    // Construct the arguments of `format_args!("{:?}", THE_CONST)` directly in memory and hand
    // them to `std::fmt::format`.
    //
    // `core::fmt::rt::Argument` is a niche-encoded `Placeholder { value: NonNull<()>, formatter }`,
    // i.e. two words: a pointer to the value, and the type-erased `<T as Debug>::fmt` function.
    // A non-null `value` is what distinguishes the `Placeholder` variant from `Count`.
    let argument = evaluator.heap_allocate(ptr_size * 2, ptr_size)?;
    evaluator.write_memory(argument, &data.addr.to_bytes(ptr_size))?;
    let debug_fmt_fn_ptr = evaluator.vtable_map.id(Ty::new_fn_def(
        evaluator.interner(),
        CallableDefId::FunctionId(debug_fmt_fn).into(),
        GenericArgs::new_from_slice(&[ty.into()]),
    ));
    evaluator.write_memory(argument.offset(ptr_size), &word_bytes(debug_fmt_fn_ptr, ptr_size))?;
    // Since Rust 1.93 `core::fmt::Arguments` is two words wide:
    //   struct Arguments<'a> { template: NonNull<u8>, args: NonNull<Argument<'a>> }
    // `template` points at a byte-encoded format string; `format_args!("{:?}", x)` encodes to a
    // single default placeholder (`0xC0`) followed by the end marker (`0x00`). `args` points at
    // our one-element argument array, and must stay pointer-aligned: `core` uses the low bit of
    // `args` as a tag (1 = inline `&str` form, 0 = placeholder form), and heap allocations here
    // are pointer-aligned so the bit is 0 as required.
    let template = evaluator.heap_allocate(2, 1)?;
    evaluator.write_memory(template, &[0xC0, 0x00])?;
    let arguments = evaluator.heap_allocate(ptr_size * 2, ptr_size)?;
    evaluator.write_memory(arguments, &template.to_bytes(ptr_size))?;
    evaluator.write_memory(arguments.offset(ptr_size), &argument.to_bytes(ptr_size))?;
    let Some(ValueNs::FunctionId(format_fn)) = resolver.resolve_path_in_value_ns_fully(
        db,
        hir_def::expr_store::path::Path::from_known_path_with_no_generic(&path![std::fmt::format]),
        HygieneId::ROOT,
    ) else {
        not_supported!("std::fmt::format not found");
    };
    let interval = evaluator.interpret_mir(
        db.runtime_mir_body(format_fn.into())
            .map_err(|e| MirEvalError::MirLowerError(format_fn, e))?,
        [IntervalOrOwned::Borrowed(Interval { addr: arguments, size: ptr_size * 2 })].into_iter(),
        None,
    )?;
    let message_string = interval.get(&evaluator)?;
    let words = [
        from_bytes!(usize, ptr_size, message_string[0..ptr_size]),
        from_bytes!(usize, ptr_size, message_string[ptr_size..2 * ptr_size]),
        from_bytes!(usize, ptr_size, message_string[2 * ptr_size..3 * ptr_size]),
    ];
    let Some(addr) = words
        .into_iter()
        .map(|it| Address::from_usize(it, ptr_size))
        .find(|it| matches!(it.at(), Heap(_)))
    else {
        // No heap buffer means the formatted string is empty.
        // alloc: the formatted text is the result, and an empty `String` holds nothing
        return Ok(String::new());
    };
    let size = words
        .into_iter()
        .filter(|&it| !matches!(Address::from_usize(it, ptr_size).at(), Heap(_)))
        .min()
        .unwrap_or(0);
    // alloc: the formatted text is the result
    Ok(std::string::String::from_utf8_lossy(evaluator.read_memory(addr, size)?).into_owned())
}

macro_rules! for_each_int_type {
    ($call_macro:path, $args:tt) => {
        $call_macro! {
            $args
            I8
            U8
            I16
            U16
            I32
            U32
            I64
            U64
            I128
            U128
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum IntValue {
    I8(i8),
    U8(u8),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    I128(i128),
    U128(u128),
}

macro_rules! checked_int_op {
    ( [ $op:ident ] $( $int_ty:ident )+ ) => {
        fn $op(self, other: Self) -> Option<Self> {
            match (self, other) {
                $( (Self::$int_ty(a), Self::$int_ty(b)) => a.$op(b).map(Self::$int_ty), )+
                _ => panic!("incompatible integer types"),
            }
        }
    };
}

macro_rules! wrapping_int_op {
    ( [ $op:ident ] $( $int_ty:ident )+ ) => {
        fn $op(self, other: Self) -> Self {
            match (self, other) {
                $( (Self::$int_ty(a), Self::$int_ty(b)) => Self::$int_ty(a.$op(b)), )+
                _ => panic!("incompatible integer types"),
            }
        }
    };
}

macro_rules! int_bit_shifts {
    ( [ $op:ident ] $( $int_ty:ident )+ ) => {
        fn $op(self, amount: u32) -> Option<Self> {
            match self {
                $( Self::$int_ty(this) => this.$op(amount).map(Self::$int_ty), )+
            }
        }
    };
}

macro_rules! unchecked_int_op {
    ( [ $name:ident, $op:tt ]  $( $int_ty:ident )+ ) => {
        fn $name(self, other: Self) -> Self {
            match (self, other) {
                $( (Self::$int_ty(a), Self::$int_ty(b)) => Self::$int_ty(a $op b), )+
                _ => panic!("incompatible integer types"),
            }
        }
    };
}

impl IntValue {
    fn from_bytes<'db>(bytes: &[u8], is_signed: bool) -> Result<'db, Self> {
        Ok(match (bytes.len(), is_signed) {
            (1, false) => Self::U8(u8::from_le_bytes(bytes.try_into().unwrap())),
            (1, true) => Self::I8(i8::from_le_bytes(bytes.try_into().unwrap())),
            (2, false) => Self::U16(u16::from_le_bytes(bytes.try_into().unwrap())),
            (2, true) => Self::I16(i16::from_le_bytes(bytes.try_into().unwrap())),
            (4, false) => Self::U32(u32::from_le_bytes(bytes.try_into().unwrap())),
            (4, true) => Self::I32(i32::from_le_bytes(bytes.try_into().unwrap())),
            (8, false) => Self::U64(u64::from_le_bytes(bytes.try_into().unwrap())),
            (8, true) => Self::I64(i64::from_le_bytes(bytes.try_into().unwrap())),
            (16, false) => Self::U128(u128::from_le_bytes(bytes.try_into().unwrap())),
            (16, true) => Self::I128(i128::from_le_bytes(bytes.try_into().unwrap())),
            (len, is_signed) => {
                return Err(MirEvalError::InternalError(
                    format!("an integer operand of {len} bytes, signed: {is_signed}").into(), // alloc: error path
                ));
            }
        })
    }

    fn to_bytes(self) -> OwnedBytes {
        macro_rules! m {
            ( [] $( $int_ty:ident )+ ) => {
                match self {
                    $( Self::$int_ty(v) => OwnedBytes::from_slice(&v.to_le_bytes()) ),+
                }
            };
        }
        for_each_int_type! { m, [] }
    }

    fn as_u32(self) -> Option<u32> {
        macro_rules! m {
            ( [] $( $int_ty:ident )+ ) => {
                match self {
                    $( Self::$int_ty(v) => v.try_into().ok() ),+
                }
            };
        }
        for_each_int_type! { m, [] }
    }

    for_each_int_type!(wrapping_int_op, [wrapping_add]);
    for_each_int_type!(wrapping_int_op, [wrapping_sub]);
    for_each_int_type!(wrapping_int_op, [wrapping_mul]);
    for_each_int_type!(checked_int_op, [checked_add]);
    for_each_int_type!(checked_int_op, [checked_sub]);
    for_each_int_type!(checked_int_op, [checked_div]);
    for_each_int_type!(checked_int_op, [checked_rem]);
    for_each_int_type!(checked_int_op, [checked_mul]);

    for_each_int_type!(int_bit_shifts, [checked_shl]);
    for_each_int_type!(int_bit_shifts, [checked_shr]);
}

impl std::ops::BitAnd for IntValue {
    type Output = Self;
    for_each_int_type!(unchecked_int_op, [bitand, &]);
}
impl std::ops::BitOr for IntValue {
    type Output = Self;
    for_each_int_type!(unchecked_int_op, [bitor, |]);
}
impl std::ops::BitXor for IntValue {
    type Output = Self;
    for_each_int_type!(unchecked_int_op, [bitxor, ^]);
}

/// The last field of a struct or tuple, which may be unsized ([`Evaluator::last_field`]).
struct LastField<'db> {
    ty: Ty<'db>,
    index: usize,
    /// Its struct's `repr(packed(n))`, which caps the field's alignment.
    pack: Option<usize>,
}
