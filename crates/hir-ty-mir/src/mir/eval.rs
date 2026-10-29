//! This module provides a MIR interpreter, which is used in const eval.

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::BTreeMap,
    iter, mem,
    ops::Range,
};

use base_db::{ConstEvalBudget, Crate};
use either::Either;
use hir_def::{
    AdtId, DefWithBodyId, EnumVariantId, FunctionId, HasModule, ItemContainerId, Lookup, StaticId,
    VariantId,
    expr_store::{Body, ExpressionStore, HygieneId},
    item_tree::FieldsShape,
    lang_item::LangItems,
    layout::{TagEncoding, Variants},
    resolver::{HasResolver, ValueNs},
    signatures::{EnumSignature, StaticFlags, StaticSignature, StructFlags, StructSignature},
};
use hir_expand::{InFile, mod_path::path};
use la_arena::ArenaMap;
use rustc_abi::{Size, TargetDataLayout};
use rustc_apfloat::{
    Float, FloatConvert,
    ieee::{Double, Half as f16, Quad as f128, Single},
};
use rustc_ast_ir::Mutability;
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::{
    AliasTyKind,
    inherent::{GenericArgs as _, IntoKind, Region as _, SliceLike, Ty as _},
};
use span::FileId;
use stdx::never;
use syntax::{SyntaxNodePtr, TextRange};
use triomphe::Arc;

use crate::{
    CallableDefId, ComplexMemoryMap, InferBodyId, InferenceResult, MemoryMap, ParamEnvAndCrate,
    consteval::{self, ConstEvalError, try_const_usize},
    db::{AnonConstId, GeneralConstId, HirDatabase, InternedClosureId},
    drop::{DropGlue, has_drop_glue},
    infer::PointerCast,
    layout::{Layout, LayoutError, RustcEnumVariantIdx, RustcFieldIdx},
    method_resolution::{is_dyn_method, lookup_impl_const},
    next_solver::{
        AliasTy, Allocation, AllocationData, Const, ConstKind, DbInterner, ErrorGuaranteed,
        GenericArgs, Region, Ty, TyKind, TypingMode, UnevaluatedConst, ValTree,
        infer::{DbInternerInferExt, InferCtxt, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    },
    traits::FnTrait,
    utils::detect_variant_from_bytes,
};

use super::{
    AggregateKind, AssertKind, BasicBlockId, BinOp, CastKind, LocalId, MirBody, MirPhase, MirSpan,
    Operand, OperandKind, PlaceElem, PlaceTy, ProjectionElem, Rvalue, StatementKind, StoredPlace,
    TerminatorKind, UnOp, return_slot,
};
use super::{Budget, IsSigned, MirEvalError, VTableMap, pad16};

mod shim;

macro_rules! from_bytes {
    // A `usize` of the target, which is as wide as what holds it.
    (usize, $value:expr) => {
        match $crate::mir::eval::usize_of(AsRef::<[u8]>::as_ref(&$value)) {
            Some(it) => it,
            None => return Err(MirEvalError::InternalError("mismatched size in constructing usize".into())),
        }
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

pub(super) mod macro_call;

/// A float as an integer of `bits` bits, as `as` makes it and rustc's evaluation computes it
/// with rustc_apfloat: rounded toward zero, saturating, and NaN as zero.
fn apfloat_to_int<F: Float>(value: F, bits: usize, signed: bool) -> u128 {
    if signed { value.to_i128(bits).value as u128 } else { value.to_u128(bits).value }
}

/// The bytes of a float converted to the float type `to` as rustc's evaluation does: rounded
/// to nearest, ties to even, and a NaN made the positive quiet NaN without a payload.
fn convert_apfloat<F>(value: F, to: rustc_type_ir::FloatTy) -> Vec<u8>
where
    F: FloatConvert<f16> + FloatConvert<Single> + FloatConvert<Double> + FloatConvert<f128>,
{
    fn to_float<F: FloatConvert<T>, T: Float>(value: F, bytes: usize) -> Vec<u8> {
        let converted = if value.is_nan() { T::NAN } else { value.convert(&mut false).value };
        converted.to_bits().to_le_bytes()[0..bytes].to_vec()
    }
    match to {
        rustc_type_ir::FloatTy::F16 => to_float::<F, f16>(value, 2),
        rustc_type_ir::FloatTy::F32 => to_float::<F, Single>(value, 4),
        rustc_type_ir::FloatTy::F64 => to_float::<F, Double>(value, 8),
        rustc_type_ir::FloatTy::F128 => to_float::<F, f128>(value, 16),
    }
}

macro_rules! not_supported {
    ($it: expr) => {
        return Err($crate::mir::MirEvalError::NotSupported(format!($it)))
    };
}
use not_supported;

/// The type a vtable's id in memory names.
trait VTableOfBytes<'db> {
    fn ty_of_bytes(&self, bytes: &[u8]) -> Result<'db, Ty<'db>>;
}

impl<'db> VTableOfBytes<'db> for VTableMap<'db> {
    fn ty_of_bytes(&self, bytes: &[u8]) -> Result<'db, Ty<'db>> {
        let id = from_bytes!(usize, bytes);
        self.ty(id)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct TlsData {
    keys: Vec<u128>,
}

impl TlsData {
    fn create_key(&mut self) -> usize {
        self.keys.push(0);
        self.keys.len() - 1
    }

    fn get_key(&mut self, key: usize) -> Result<'static, u128> {
        let r = self.keys.get(key).ok_or_else(|| {
            MirEvalError::UndefinedBehavior(format!("Getting invalid tls key {key}"))
        })?;
        Ok(*r)
    }

    fn set_key(&mut self, key: usize, value: u128) -> Result<'static, ()> {
        let r = self.keys.get_mut(key).ok_or_else(|| {
            MirEvalError::UndefinedBehavior(format!("Setting invalid tls key {key}"))
        })?;
        *r = value;
        Ok(())
    }
}

struct StackFrame<'db> {
    locals: Locals<'db>,
    destination: Option<BasicBlockId>,
    prev_stack_ptr: usize,
    span: (MirSpan, InferBodyId<'db>),
    /// The function called, when it is not a closure or what an evaluation starts from.
    callee: Option<FunctionId>,
}

#[derive(Clone)]
enum MirOrDynIndex<'db> {
    Mir(&'db MirBody<'db>),
    Dyn(usize),
}

/// A stack or heap allocation: where it starts, its size, and what its start is a multiple of.
#[derive(Debug, Clone, Copy)]
struct Allocated {
    start: usize,
    size: usize,
    align: usize,
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
}

/// A block of the heap: the size and alignment it was asked for, the bytes it takes, of a
/// size class ([`size_class`]) a freed block of the same class and alignment is reused for,
/// and what made it.
/// What is left of a drop ([`Evaluator::run_drop_glue_deep`]).
enum DropStep<'db> {
    /// The drop glue of a value: its type, where it is, and its metadata if unsized.
    Glue(Ty<'db>, Address, Vec<u8>),
    /// The `Drop` impl of a `Box`, once what it points to is dropped.
    DropImpl(Ty<'db>, Address),
}

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

pub struct Evaluator<'db> {
    db: &'db dyn HirDatabase,
    param_env: ParamEnvAndCrate<'db>,
    target_data_layout: &'db TargetDataLayout,
    stack: Vec<u8>,
    heap: Vec<u8>,
    /// Every stack allocation, in address order. A raw pointer in a constant doesn't say how
    /// much memory it can reach, its allocation does.
    stack_allocations: Vec<Allocated>,
    /// Every live block of the heap, by its offset in [`Self::heap`].
    heap_blocks: BTreeMap<usize, HeapBlock>,
    /// The offsets of freed heap blocks, by size class and alignment, for the next block of
    /// that class and alignment.
    free_blocks: FxHashMap<(usize, usize), Vec<usize>>,
    /// Where a promoted temporary of a body (`&[1, 2]`) is: in the heap, as its borrow
    /// outlives the call, once for every call of the body.
    promoted: FxHashMap<(usize, LocalId), Address>,
    /// Whether dropping a value of a type does anything ([`Self::has_drop_glue`]).
    drop_glue: FxHashMap<Ty<'db>, bool>,
    /// Where the value of a constant operand is, once read: the program never writes it, so
    /// every use of the constant reads the same copy.
    constants: FxHashMap<Either<Const<'db>, Allocation<'db>>, Interval>,
    code_stack: Vec<StackFrame<'db>>,
    /// The `callee` and `span` of the frame being run, which is not on `code_stack`.
    running: Option<(Option<FunctionId>, (MirSpan, InferBodyId<'db>))>,
    /// Stores the global location of the statics. We const evaluate every static first time we need it
    /// and see it's missing, then we add it to this to reuse.
    static_locations: FxHashMap<StaticId, Address>,
    /// The memory of each evaluated static, as start and size.
    static_memory: Vec<(usize, usize, StaticId)>,
    /// The statics whose value is read, when the others are left unevaluated: a static that
    /// is only pointed to needs no value, and may itself point back at the one being
    /// evaluated. `None` evaluates every static that is named.
    read_statics: Option<FxHashSet<StaticId>>,
    /// Heap memory standing in for the statics left unevaluated, as start and size.
    unevaluated_statics: Vec<(usize, usize, StaticId)>,
    /// The unevaluated static whose memory was read, which fails the evaluation.
    missing_static: Cell<Option<StaticId>>,
    /// Running a static's initializer, where the memory of every static is read-only, as in
    /// rustc.
    in_static: bool,
    /// We don't really have function pointers, i.e. pointers to some assembly instructions that we can run. Instead, we
    /// store the type as an interned id in place of function and vtable pointers, and we recover back the type at the
    /// time of use.
    vtable_map: VTableMap<'db>,
    thread_local_storage: TlsData,
    random_state: oorandom::Rand64,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    layout_cache: RefCell<FxHashMap<Ty<'db>, Arc<Layout>>>,
    projected_ty_cache: RefCell<FxHashMap<(PlaceTy<'db>, PlaceElem), PlaceTy<'db>>>,
    not_special_fn_cache: RefCell<FxHashSet<FunctionId>>,
    mir_or_dyn_index_cache: RefCell<FxHashMap<(FunctionId, GenericArgs<'db>), MirOrDynIndex<'db>>>,
    /// Constantly dropping and creating `Locals` is very costly. We store
    /// old locals that we normally want to drop here, to reuse their allocations
    /// later.
    unused_locals_store: RefCell<FxHashMap<InferBodyId<'db>, Vec<Locals<'db>>>>,
    cached_ptr_size: usize,
    cached_fn_trait_func: Option<FunctionId>,
    cached_fn_mut_trait_func: Option<FunctionId>,
    cached_fn_once_trait_func: Option<FunctionId>,
    crate_id: Crate,
    /// What the `TypeId`s made so far stand for. An id here is its place in this list, which
    /// says whether two are of one type and is not the hash compiled code has.
    type_ids: Vec<TypeIdOf<'db>>,
    // FIXME: This is a workaround, see the comment on `interpret_mir`
    assert_placeholder_ty_is_unused: bool,
    /// What the evaluation may spend, of the workspace of the crate it is for.
    budget: ConstEvalBudget,
    /// The steps of [`Self::budget`] not spent yet.
    steps_left: u64,
    /// An additional limit on stack depth, to prevent stack overflow
    stack_depth_limit: usize,
    /// Every call takes a frame of [`Self::stack_depth_limit`], which is the crate's recursion
    /// limit, as in rustc: the evaluation is of a constant's or static's initializer. Running a
    /// function, only the nested runs of `interpret_mir` do.
    limits_frames: bool,
    infcx: InferCtxt<'db>,
}

/// What a `TypeId` of the evaluation stands for: a type, or the type that stands for a field
/// of one in reflection (`type_id_field_representing_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeIdOf<'db> {
    Ty(Ty<'db>),
    Field { base: Ty<'db>, variant: usize, field: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Address {
    Stack(usize),
    Heap(usize),
    Invalid(usize),
}

use Address::*;

#[derive(Debug, Clone, Copy)]
struct Interval {
    addr: Address,
    size: usize,
}

#[derive(Debug, Clone)]
struct IntervalAndTy<'db> {
    interval: Interval,
    ty: Ty<'db>,
}

impl Interval {
    fn new(addr: Address, size: usize) -> Self {
        Self { addr, size }
    }

    fn get<'b, 'db>(&self, memory: &'b Evaluator<'db>) -> Result<'db, &'b [u8]> {
        memory.read_memory(self.addr, self.size)
    }

    fn write_from_bytes<'db>(&self, memory: &mut Evaluator<'db>, bytes: &[u8]) -> Result<'db, ()> {
        memory.write_memory(self.addr, bytes)
    }

    fn write_from_interval<'db>(
        &self,
        memory: &mut Evaluator<'db>,
        interval: Interval,
    ) -> Result<'db, ()> {
        memory.copy_from_interval(self.addr, interval)
    }

    fn slice(self, range: Range<usize>) -> Interval {
        Interval { addr: self.addr.offset(range.start), size: range.len() }
    }
}

impl<'db> IntervalAndTy<'db> {
    fn get<'b>(&self, memory: &'b Evaluator<'db>) -> Result<'db, &'b [u8]> {
        memory.read_memory(self.interval.addr, self.interval.size)
    }

    fn new(
        addr: Address,
        ty: Ty<'db>,
        evaluator: &Evaluator<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, IntervalAndTy<'db>> {
        let size = evaluator.size_of_sized(ty, locals, "type of interval")?;
        Ok(IntervalAndTy { interval: Interval { addr, size }, ty })
    }
}

enum IntervalOrOwned {
    Owned(Vec<u8>),
    Borrowed(Interval),
}

impl From<Interval> for IntervalOrOwned {
    fn from(it: Interval) -> IntervalOrOwned {
        IntervalOrOwned::Borrowed(it)
    }
}

impl IntervalOrOwned {
    fn get<'b, 'db>(&'b self, memory: &'b Evaluator<'db>) -> Result<'db, &'b [u8]> {
        Ok(match self {
            IntervalOrOwned::Owned(o) => o,
            IntervalOrOwned::Borrowed(b) => b.get(memory)?,
        })
    }
}

/// Where the addresses of the stack and of the heap start, which have to fit an address
/// of the target.
fn stack_offset() -> usize {
    1 << (target_word() * 8 - 4)
}

fn heap_offset() -> usize {
    1 << (target_word() * 8 - 5)
}

/// How many bytes an address of the target takes.
fn target_word() -> usize {
    crate::mir::target::word()
}

/// The `usize` of the target that `bytes` are.
fn usize_of(bytes: &[u8]) -> Option<usize> {
    let mut word = [0; size_of::<usize>()];
    (bytes.len() == target_word()).then(|| word[..bytes.len()].copy_from_slice(bytes))?;
    Some(usize::from_le_bytes(word))
}

/// `value` as a `usize` of the target.
fn word_bytes(value: usize) -> smallvec::SmallVec<[u8; 8]> {
    smallvec::SmallVec::from_slice(&value.to_le_bytes()[..target_word()])
}

impl Address {
    fn from_bytes<'db>(it: &[u8]) -> Result<'db, Self> {
        Ok(Address::from_usize(from_bytes!(usize, it)))
    }

    fn from_usize(it: usize) -> Self {
        if it > stack_offset() {
            Stack(it - stack_offset())
        } else if it > heap_offset() {
            Heap(it - heap_offset())
        } else {
            Invalid(it)
        }
    }

    /// Whether this is a number made into a pointer to `size` bytes, none: no memory is
    /// behind it, and the number is the whole of its value.
    fn is_of_nothing(&self, size: usize) -> bool {
        size == 0 && matches!(self, Invalid(_))
    }

    fn to_bytes(&self) -> smallvec::SmallVec<[u8; 8]> {
        word_bytes(self.to_usize())
    }

    fn to_usize(&self) -> usize {
        match self {
            Stack(it) => *it + stack_offset(),
            Heap(it) => *it + heap_offset(),
            Invalid(it) => *it,
        }
    }

    fn map(&self, f: impl FnOnce(usize) -> usize) -> Address {
        match self {
            Stack(it) => Stack(f(*it)),
            Heap(it) => Heap(f(*it)),
            Invalid(it) => Invalid(f(*it)),
        }
    }

    fn offset(&self, offset: usize) -> Address {
        self.map(|it| it + offset)
    }
}

type Result<'db, T> = std::result::Result<T, MirEvalError<'db>>;

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
        String::from_utf8_lossy(&self.stdout)
    }
    pub fn stderr(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.stderr)
    }
}

pub fn interpret_mir<'db>(
    db: &'db dyn HirDatabase,
    body: &'db MirBody<'db>,
    // FIXME: This is workaround. Ideally, const generics should have a separate body (issue #7434), but now
    // they share their body with their parent, so in MIR lowering we have locals of the parent body, which
    // might have placeholders. With this argument, we (wrongly) assume that every placeholder type has
    // a zero size, hoping that they are all outside of our current body. Even without a fix for #7434, we can
    // (and probably should) do better here, for example by excluding bindings outside of the target expression.
    assert_placeholder_ty_is_unused: bool,
    trait_env: Option<ParamEnvAndCrate<'db>>,
) -> Result<'db, (Result<'db, Allocation<'db>>, MirOutput)> {
    let ty = body.locals[return_slot()].ty.as_ref();
    let mut read_statics = FxHashSet::default();
    loop {
        let mut evaluator =
            Evaluator::new(db, body.owner, assert_placeholder_ty_is_unused, trait_env)?;
        evaluator.read_statics = Some(read_statics.clone());
        let it = evaluator.evaluate(body, ty);
        if let Some(read) = evaluator.missing_static.take()
            && read_statics.insert(read)
        {
            continue;
        }
        return Ok((it, MirOutput { stdout: evaluator.stdout, stderr: evaluator.stderr }));
    }
}

impl<'db> Evaluator<'db> {
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
        if !self.type_ids.is_empty() && !self.holds_no_type_id(ty, &mut FxHashSet::default()) {
            let of = |it: &TypeIdOf<'db>| match *it {
                TypeIdOf::Ty(ty) => Some(ty),
                TypeIdOf::Field { .. } => None,
            };
            memory_map.type_ids = self.type_ids.iter().map(of).collect();
        }
        let bytes = Box::from(bytes);
        let memory_map = if memory_map.memory.is_empty()
            && memory_map.statics.is_empty()
            && memory_map.type_ids.is_empty()
            && self.vtable_map.is_empty()
        {
            MemoryMap::Empty
        } else {
            memory_map.vtable = mem::take(&mut self.vtable_map);
            memory_map.vtable.shrink_to_fit();
            MemoryMap::Complex(Box::new(memory_map))
        };
        Ok(Allocation::new(AllocationData { ty, memory: bytes, memory_map }))
    }
}

impl<'db> Evaluator<'db> {
    pub fn new(
        db: &'db dyn HirDatabase,
        owner: InferBodyId<'db>,
        assert_placeholder_ty_is_unused: bool,
        trait_env: Option<ParamEnvAndCrate<'db>>,
    ) -> Result<'db, Evaluator<'db>> {
        let module = owner.module(db);
        let crate_id = module.krate(db);
        let target_data_layout = match db.target_data_layout(crate_id) {
            Ok(target_data_layout) => target_data_layout,
            Err(e) => return Err(MirEvalError::TargetDataLayoutNotAvailable(e)),
        };
        let cached_ptr_size = target_data_layout.pointer_size().bytes_usize();
        // Impls are looked up from the crate the body is evaluated for: the ones its generic
        // arguments select can be in crates the body's own doesn't depend on.
        let interner = DbInterner::new_with(db, trait_env.map_or(crate_id, |env| env.krate));
        let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
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
            target_data_layout,
            stack: vec![0],
            heap: vec![0],
            stack_allocations: Vec::new(),
            heap_blocks: BTreeMap::new(),
            free_blocks: FxHashMap::default(),
            promoted: FxHashMap::default(),
            drop_glue: FxHashMap::default(),
            constants: FxHashMap::default(),
            code_stack: vec![],
            running: None,
            vtable_map: VTableMap::default(),
            thread_local_storage: TlsData::default(),
            static_locations: Default::default(),
            static_memory: Vec::new(),
            read_statics: None,
            unevaluated_statics: Vec::new(),
            missing_static: Cell::new(None),
            in_static: matches!(owner, InferBodyId::DefWithBodyId(DefWithBodyId::StaticId(_))),
            db,
            random_state: oorandom::Rand64::new(0),
            param_env: trait_env.unwrap_or_else(|| ParamEnvAndCrate {
                param_env: db.trait_environment(owner.generic_def(db)),
                krate: crate_id,
            }),
            crate_id,
            stdout: vec![],
            stderr: vec![],
            type_ids: Vec::new(),
            assert_placeholder_ty_is_unused,
            stack_depth_limit,
            limits_frames,
            budget,
            steps_left: budget.steps,
            layout_cache: RefCell::new(Default::default()),
            projected_ty_cache: RefCell::new(Default::default()),
            not_special_fn_cache: RefCell::new(Default::default()),
            mir_or_dyn_index_cache: RefCell::new(Default::default()),
            unused_locals_store: RefCell::new(Default::default()),
            cached_ptr_size,
            cached_fn_trait_func: lang_items.Fn_call,
            cached_fn_mut_trait_func: lang_items.FnMut_call_mut,
            cached_fn_once_trait_func: lang_items.FnOnce_call_once,
            infcx,
        })
    }

    #[inline]
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
        let place_addr_and_ty = self.place_addr_and_ty_and_metadata(p, locals)?;
        Ok(Interval {
            addr: place_addr_and_ty.0,
            size: self.size_of_sized(
                place_addr_and_ty.1,
                locals,
                "Type of place that we need its interval",
            )?,
        })
    }

    fn ptr_size(&self) -> usize {
        self.cached_ptr_size
    }

    fn resolve_mir_span(
        &self,
        owner: InferBodyId<'db>,
        span: MirSpan,
    ) -> Option<(FileId, TextRange)> {
        let (source_map, self_param_syntax) = match owner {
            InferBodyId::DefWithBodyId(def) => {
                let body = &Body::with_source_map(self.db, def).1;
                (&**body, body.self_param_syntax())
            }
            InferBodyId::AnonConstId(def) => {
                (ExpressionStore::with_source_map(self.db, def.loc(self.db).owner).1, None)
            }
        };
        let span: InFile<SyntaxNodePtr> = match span {
            MirSpan::ExprId(e) | MirSpan::ExprEnd(e) => {
                source_map.expr_syntax(e).ok()?.map(|it| it.into())
            }
            MirSpan::PatId(p) => source_map.pat_syntax(p).ok()?.map(|it| it.syntax_node_ptr()),
            MirSpan::BindingId(b) => source_map
                .patterns_for_binding(b)
                .iter()
                .find_map(|p| source_map.pat_syntax(*p).ok())?
                .map(|it| it.syntax_node_ptr()),
            MirSpan::SelfParam => self_param_syntax?.map(|it| it.syntax_node_ptr()),
            MirSpan::Unknown => return None,
        };
        let file_id = span.file_id.original_file(self.db);
        Some((file_id.file_id(self.db), span.value.text_range()))
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
    ) -> Result<'db, (Address, Ty<'db>, Option<IntervalOrOwned>)> {
        let mut addr = locals.ptr[p.local].addr;
        let mut ty = PlaceTy::from_ty(locals.body.locals[p.local].ty.as_ref());
        let mut metadata: Option<IntervalOrOwned> = None; // locals are always sized
        for proj in p.projection.as_slice() {
            let prev_ty = ty;
            ty = self.projected_ty(ty, *proj);
            match proj {
                ProjectionElem::Deref => {
                    metadata = if self.size_align_of(ty.ty, locals)?.is_none() {
                        Some(
                            Interval { addr: addr.offset(self.ptr_size()), size: self.ptr_size() }
                                .into(),
                        )
                    } else {
                        None
                    };
                    let it = from_bytes!(usize, self.read_memory(addr, self.ptr_size())?);
                    addr = Address::from_usize(it);
                }
                ProjectionElem::Index(op) => {
                    let offset = from_bytes!(
                        usize,
                        self.read_memory(locals.ptr[*op].addr, self.ptr_size())?
                    );
                    metadata = None; // Result of index is always sized
                    let ty_size = self.size_of_sized(ty.ty, locals, "array inner type")?;
                    addr = addr.offset(ty_size * offset);
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
                            TyKind::Slice(_) => match metadata {
                                Some(it) => from_bytes!(usize, it.get(self)?) as u64,
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
                    addr = addr.offset(ty_size * offset);
                }
                &ProjectionElem::Subslice { from, to } => {
                    let inner_ty = match ty.ty.kind() {
                        TyKind::Array(inner, _) | TyKind::Slice(inner) => inner,
                        _ => Ty::new_error(self.interner(), ErrorGuaranteed),
                    };
                    metadata = match metadata {
                        Some(it) => {
                            let prev_len = from_bytes!(usize, it.get(self)?) as u64;
                            Some(IntervalOrOwned::Owned(
                                word_bytes((prev_len - from - to) as usize).to_vec(),
                            ))
                        }
                        None => None,
                    };
                    let ty_size = self.size_of_sized(inner_ty, locals, "array inner type")?;
                    addr = addr.offset(ty_size * (from as usize));
                }
                ProjectionElem::Field(f) => {
                    let layout = self.layout(prev_ty.ty)?;
                    let offset = match &layout.variants {
                        Variants::Empty => {
                            return Err(MirEvalError::UndefinedBehavior(
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
                    addr = addr.offset(offset);
                    // Unsized field metadata is equal to the metadata of the struct
                    if self.size_align_of(ty.ty, locals)?.is_some() {
                        metadata = None;
                    }
                }
                ProjectionElem::Downcast(_) => {
                    // no runtime effect
                }
            }
        }
        Ok((addr, ty.ty, metadata))
    }

    fn layout(&self, ty: Ty<'db>) -> Result<'db, Arc<Layout>> {
        if let Some(x) = self.layout_cache.borrow().get(&ty) {
            return Ok(x.clone());
        }
        let r = self
            .db
            .layout_of_ty(ty.store(), self.param_env.store())
            .map_err(|e| MirEvalError::LayoutError(e, ty.store()))?;
        self.layout_cache.borrow_mut().insert(ty, r.clone());
        Ok(r)
    }

    fn layout_adt(&self, adt: AdtId, subst: GenericArgs<'db>) -> Result<'db, Arc<Layout>> {
        self.layout(Ty::new_adt(self.interner(), adt, subst))
    }

    fn place_ty<'b>(&'b self, p: &StoredPlace, locals: &'b Locals<'db>) -> Result<'db, Ty<'db>> {
        Ok(self.place_addr_and_ty_and_metadata(p, locals)?.1)
    }

    /// Whether arithmetic of the body being evaluated panics when it overflows. A function's
    /// does what the code compiled from it does. The initializer of a constant, a static or a
    /// discriminant always checks, as rustc builds it, whatever `-C overflow-checks` says.
    fn checks_overflow(&self, locals: &Locals<'db>) -> bool {
        !matches!(locals.body.owner, InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(_)))
            || crate::mir::overflow::body_checks_overflow(self.db, locals.body).unwrap_or(true)
    }

    fn operand_ty(&self, o: &Operand, locals: &Locals<'db>) -> Result<'db, Ty<'db>> {
        Ok(match &o.kind {
            OperandKind::Copy(p) | OperandKind::Move(p) => self.place_ty(p, locals)?,
            OperandKind::Constant { konst: _, ty } => ty.as_ref(),
            OperandKind::Allocation { allocation } => allocation.as_ref().ty,
            &OperandKind::Static(s) => {
                let ty = InferenceResult::of(self.db, DefWithBodyId::from(s))
                    .expr_ty(Body::of(self.db, s.into()).root_expr());
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
        args: impl Iterator<Item = IntervalOrOwned>,
        called: Option<(FunctionId, MirSpan, InferBodyId<'db>)>,
    ) -> Result<'db, Interval> {
        let depth_limit = self.stack_depth_limit;
        if let Some(it) = self.stack_depth_limit.checked_sub(1) {
            self.stack_depth_limit = it;
        } else {
            return Err(MirEvalError::StackOverflow);
        }
        let mut current_block_idx = body.start_block;
        let (mut locals, prev_stack_ptr) = self.create_locals_for_body(body, None)?;
        self.fill_locals_for_body(body, &mut locals, args)?;
        let prev_code_stack = mem::take(&mut self.code_stack);
        let prev_running = self.running;
        let (callee, span) = match called {
            Some((callee, span, caller)) => (Some(callee), (span, caller)),
            None => (None, (MirSpan::Unknown, body.owner)),
        };
        let frame = StackFrame { locals, destination: None, prev_stack_ptr, span, callee };
        self.code_stack.push(frame);
        'stack: loop {
            let Some(mut my_stack_frame) = self.code_stack.pop() else {
                not_supported!("missing stack frame");
            };
            let frame_body = my_stack_frame.locals.body;
            self.running = Some((my_stack_frame.callee, my_stack_frame.span));
            let e = (|| {
                let locals = &mut my_stack_frame.locals;
                let body = locals.body.clone();
                loop {
                    let current_block = &body.basic_blocks[current_block_idx];
                    if let Some(it) = self.steps_left.checked_sub(1) {
                        self.steps_left = it;
                    } else {
                        return Err(MirEvalError::OverBudget {
                            spent: Budget::Steps(self.budget.steps),
                            at: None,
                        });
                    }
                    for statement in &current_block.statements {
                        match &statement.kind {
                            StatementKind::Assign(l, r) => {
                                let addr = self.place_addr(l, locals)?;
                                let result = self.eval_rvalue(r, locals)?;
                                self.copy_from_interval_or_owned(addr, result)?;
                            }
                            StatementKind::Deinit(_) => not_supported!("de-init statement"),
                            StatementKind::InlineAsm(_) => not_supported!("inline assembly"),
                            StatementKind::StorageLive(_)
                            | StatementKind::FakeRead(_)
                            | StatementKind::StorageDead(_)
                            | StatementKind::Nop => (),
                        }
                    }
                    let Some(terminator) = current_block.terminator.as_ref() else {
                        not_supported!("block without terminator");
                    };
                    match &terminator.kind {
                        TerminatorKind::Assert { cond, expected, msg, target, .. } => {
                            let holds = self.eval_operand(cond, locals)?.get(self)?[0] != 0;
                            let skipped =
                                msg.is_optional_overflow_check() && !self.checks_overflow(locals);
                            if holds != *expected && !skipped {
                                return Err(MirEvalError::Panic(self.assert_message(msg, locals)?));
                            }
                            current_block_idx = *target;
                        }
                        TerminatorKind::Goto { target } => {
                            current_block_idx = *target;
                        }
                        TerminatorKind::Call {
                            func,
                            args,
                            destination,
                            target,
                            cleanup: _,
                            from_hir_call: _,
                        } => {
                            let destination_interval = self.place_interval(destination, locals)?;
                            let fn_ty = self.operand_ty(func, locals)?;
                            let args = args
                                .iter()
                                .map(|it| self.operand_ty_and_eval(it, locals))
                                .collect::<Result<'db, Vec<_>>>()?;
                            let stack_frame = match fn_ty.kind() {
                                TyKind::FnPtr(..) => {
                                    let bytes = self.eval_operand(func, locals)?;
                                    self.exec_fn_pointer(
                                        bytes,
                                        destination_interval,
                                        &args,
                                        locals,
                                        *target,
                                        terminator.span,
                                    )?
                                }
                                TyKind::FnDef(def, generic_args) => self.exec_fn_def(
                                    def.0,
                                    generic_args,
                                    destination_interval,
                                    &args,
                                    locals,
                                    *target,
                                    terminator.span,
                                )?,
                                it => not_supported!("unknown function type {it:?}"),
                            };
                            if let Some(stack_frame) = stack_frame {
                                if self.limits_frames {
                                    let Some(it) = self.stack_depth_limit.checked_sub(1) else {
                                        return Err(MirEvalError::StackOverflow);
                                    };
                                    self.stack_depth_limit = it;
                                }
                                self.code_stack.push(my_stack_frame);
                                current_block_idx = stack_frame.locals.body.start_block;
                                self.code_stack.push(stack_frame);
                                return Ok(None);
                            } else {
                                current_block_idx =
                                    target.ok_or(MirEvalError::UndefinedBehavior(
                                        "Diverging function returned".to_owned(),
                                    ))?;
                            }
                        }
                        TerminatorKind::SwitchInt { discr, targets } => {
                            let val = u128::from_le_bytes(pad16(
                                self.eval_operand(discr, locals)?.get(self)?,
                                IsSigned::No,
                            ));
                            current_block_idx = targets.target_for_value(val);
                        }
                        TerminatorKind::Return => {
                            break;
                        }
                        TerminatorKind::Unreachable => {
                            return Err(MirEvalError::UndefinedBehavior(
                                "unreachable executed".to_owned(),
                            ));
                        }
                        TerminatorKind::Drop { place, target, unwind: _ } => {
                            self.drop_place(place, locals, terminator.span)?;
                            current_block_idx = *target;
                        }
                        _ => not_supported!("unknown terminator"),
                    }
                }
                Ok(Some(my_stack_frame))
            })();
            let my_stack_frame = match e {
                Ok(None) => continue 'stack,
                Ok(Some(x)) => x,
                Err(mut e) => {
                    if let MirEvalError::OverBudget { at: at @ None, .. } = &mut e {
                        let block = &frame_body.basic_blocks[current_block_idx];
                        let span = block.terminator.as_ref().map_or(MirSpan::Unknown, |it| it.span);
                        *at = Some((frame_body.owner, span));
                    }
                    let my_code_stack = mem::replace(&mut self.code_stack, prev_code_stack);
                    self.running = prev_running;
                    self.stack_depth_limit = depth_limit;
                    let mut error_stack = vec![];
                    for frame in my_code_stack.into_iter().rev() {
                        if let Some(f) = frame.locals.body.owner.as_function() {
                            error_stack.push((Either::Left(f), frame.span.0, frame.span.1));
                        }
                    }
                    return Err(MirEvalError::InFunction(Box::new(e), error_stack));
                }
            };
            let return_interval = my_stack_frame.locals.ptr[return_slot()];
            self.unused_locals_store
                .borrow_mut()
                .entry(my_stack_frame.locals.body.owner)
                .or_default()
                .push(my_stack_frame.locals);
            match my_stack_frame.destination {
                None => {
                    self.code_stack = prev_code_stack;
                    self.running = prev_running;
                    self.stack_depth_limit += 1;
                    return Ok(return_interval);
                }
                Some(bb) => {
                    if self.limits_frames {
                        self.stack_depth_limit += 1;
                    }
                    self.free_stack(my_stack_frame.prev_stack_ptr);
                    current_block_idx = bb;
                }
            }
        }
    }

    /// Gives back the stack above `len`, of the frames that returned: nothing points there
    /// any more but what a dangling pointer would, as the temporaries whose borrows outlive
    /// a call are in the heap ([`Self::promoted`]).
    fn free_stack(&mut self, len: usize) {
        self.truncate_stack(len);
    }

    fn truncate_stack(&mut self, len: usize) {
        self.stack.truncate(len);
        let end = Stack(len).to_usize();
        let kept = self.stack_allocations.partition_point(|it| it.start < end);
        self.stack_allocations.truncate(kept);
    }

    fn fill_locals_for_body(
        &mut self,
        body: &'db MirBody<'db>,
        locals: &mut Locals<'db>,
        args: impl Iterator<Item = IntervalOrOwned>,
    ) -> Result<'db, ()> {
        let mut remain_args = body.param_locals.len();
        for ((_, interval), value) in locals.ptr.iter().skip(1).zip(args) {
            match value {
                IntervalOrOwned::Owned(value) => interval.write_from_bytes(self, &value)?,
                IntervalOrOwned::Borrowed(value) => interval.write_from_interval(self, value)?,
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
                    locals.ptr.insert(id, Interval { addr: Invalid(align), size });
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
                locals.ptr.insert(id, Interval { addr: Stack(my_ptr), size });
                self.stack_allocations.push(Allocated {
                    start: Stack(my_ptr).to_usize(),
                    size,
                    align,
                });
            }
            stack_ptr - self.stack.len()
        };
        let prev_stack_pointer = self.stack.len();
        self.check_memory(stack_size)?;
        self.stack.extend(std::iter::repeat_n(0, stack_size));
        Ok((locals, prev_stack_pointer))
    }

    fn eval_rvalue(
        &mut self,
        r: &Rvalue,
        locals: &mut Locals<'db>,
    ) -> Result<'db, IntervalOrOwned> {
        use IntervalOrOwned::*;
        Ok(match r {
            Rvalue::Use(it) => Borrowed(self.eval_operand(it, locals)?),
            Rvalue::Ref(_, p) => {
                let (addr, _, metadata) = self.place_addr_and_ty_and_metadata(p, locals)?;
                let mut r = addr.to_bytes().to_vec();
                if let Some(metadata) = metadata {
                    r.extend(metadata.get(self)?);
                }
                Owned(r)
            }
            Rvalue::Len(p) => {
                let (_, _, metadata) = self.place_addr_and_ty_and_metadata(p, locals)?;
                match metadata {
                    Some(m) => m,
                    None => {
                        return Err(MirEvalError::InternalError(
                            "type without metadata is used for Rvalue::Len".into(),
                        ));
                    }
                }
            }
            Rvalue::UnaryOp(op, val) => {
                let mut c = self.eval_operand(val, locals)?.get(self)?;
                let mut ty = self.operand_ty(val, locals)?;
                while let TyKind::Ref(_, z, _) = ty.kind() {
                    ty = z;
                    let size = self.size_of_sized(ty, locals, "operand of unary op")?;
                    c = self.read_memory(Address::from_bytes(c)?, size)?;
                }
                if let TyKind::Float(f) = ty.kind() {
                    match f {
                        rustc_type_ir::FloatTy::F16 => {
                            let c = -from_bytes!(f16, u16, c);
                            Owned(u16::try_from(c.to_bits()).unwrap().to_le_bytes().into())
                        }
                        rustc_type_ir::FloatTy::F32 => {
                            let c = -from_bytes!(f32, c);
                            Owned(c.to_le_bytes().into())
                        }
                        rustc_type_ir::FloatTy::F64 => {
                            let c = -from_bytes!(f64, c);
                            Owned(c.to_le_bytes().into())
                        }
                        rustc_type_ir::FloatTy::F128 => {
                            let c = -from_bytes!(f128, u128, c);
                            Owned(c.to_bits().to_le_bytes().into())
                        }
                    }
                } else {
                    let mut c = c.to_vec();
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
                let lc = self.eval_operand(lhs, locals)?;
                let rc = self.eval_operand(rhs, locals)?;
                let mut lc = lc.get(self)?;
                let mut rc = rc.get(self)?;
                let mut ty = self.operand_ty(lhs, locals)?;
                while let TyKind::Ref(_, z, _) = ty.kind() {
                    ty = z;
                    // A string or byte string pattern: the lengths, then the bytes. Elements
                    // without padding are equal exactly when their bytes are.
                    let element = match ty.kind() {
                        TyKind::Str => Some(1),
                        TyKind::Slice(element)
                            if matches!(
                                element.kind(),
                                TyKind::Int(_) | TyKind::Uint(_) | TyKind::Bool | TyKind::Char
                            ) =>
                        {
                            Some(self.size_of_sized(element, locals, "element of a slice")?)
                        }
                        _ => None,
                    };
                    let size = if let Some(element) = element {
                        if *op != BinOp::Eq {
                            return Err(MirEvalError::NotSupported(
                                "Only eq is builtin for `str` and slices".to_owned(),
                            ));
                        }
                        let ls = from_bytes!(usize, &lc[self.ptr_size()..self.ptr_size() * 2]);
                        let rs = from_bytes!(usize, &rc[self.ptr_size()..self.ptr_size() * 2]);
                        if ls != rs {
                            break 'binary_op Owned(vec![0]);
                        }
                        lc = &lc[..self.ptr_size()];
                        rc = &rc[..self.ptr_size()];
                        lc = self.read_memory(Address::from_bytes(lc)?, ls * element)?;
                        rc = self.read_memory(Address::from_bytes(rc)?, ls * element)?;
                        break 'binary_op Owned(vec![u8::from(lc == rc)]);
                    } else {
                        self.size_of_sized(ty, locals, "operand of binary op")?
                    };
                    lc = self.read_memory(Address::from_bytes(lc)?, size)?;
                    rc = self.read_memory(Address::from_bytes(rc)?, size)?;
                }
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
                                    Owned(vec![r])
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
                                    Owned(
                                        u16::try_from(r.value.to_bits())
                                            .unwrap()
                                            .to_le_bytes()
                                            .into(),
                                    )
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
                                    Owned(vec![r])
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
                                    Owned(r.to_le_bytes().into())
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
                                    Owned(vec![r])
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
                                    Owned(r.to_le_bytes().into())
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
                                    Owned(vec![r])
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
                                    Owned(r.value.to_bits().to_le_bytes().into())
                                }
                                it => not_supported!(
                                    "invalid binop {it:?} on floating point operators"
                                ),
                            }
                        }
                    }
                } else {
                    let is_signed = matches!(ty.kind(), TyKind::Int(_));
                    let l128 = IntValue::from_bytes(lc, is_signed);
                    let r128 = IntValue::from_bytes(rc, is_signed);
                    match op {
                        BinOp::Ge | BinOp::Gt | BinOp::Le | BinOp::Lt | BinOp::Eq | BinOp::Ne => {
                            let r = op.run_compare(l128, r128) as u8;
                            Owned(vec![r])
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
                                    MirEvalError::Panic(format!("Overflow in {op:?}"))
                                })?,
                                BinOp::Mul => l128.checked_mul(r128).ok_or_else(|| {
                                    MirEvalError::Panic(format!("Overflow in {op:?}"))
                                })?,
                                BinOp::Div => l128.checked_div(r128).ok_or_else(|| {
                                    MirEvalError::Panic(format!("Overflow in {op:?}"))
                                })?,
                                BinOp::Rem => l128.checked_rem(r128).ok_or_else(|| {
                                    MirEvalError::Panic(format!("Overflow in {op:?}"))
                                })?,
                                BinOp::Sub => l128.checked_sub(r128).ok_or_else(|| {
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
                                let amount = if checked && self.checks_overflow(locals) {
                                    r128.as_u32()
                                } else {
                                    // An unchecked shift takes the low bits of its amount.
                                    Some(u32::from(r128.to_bytes()[0]) % bits)
                                };
                                if let Some(shift_amount) = amount {
                                    let r = match op {
                                        BinOp::Shl => l128.checked_shl(shift_amount),
                                        BinOp::Shr => l128.checked_shr(shift_amount),
                                        _ => unreachable!(),
                                    };
                                    if shift_amount as usize >= lc.len() * 8 {
                                        return Err(MirEvalError::Panic(format!(
                                            "Overflow in {op:?}"
                                        )));
                                    }
                                    if let Some(r) = r {
                                        break 'b r;
                                    }
                                };
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
                            // `(T, bool)`: the value, the flag, and padding to twice `T`.
                            let mut bytes = value.to_bytes();
                            bytes.push(u8::from(overflowed));
                            bytes.resize(2 * lc.len(), 0);
                            Owned(bytes)
                        }
                        BinOp::Offset => not_supported!("offset binop"),
                    }
                }
            }
            Rvalue::Discriminant(p) => {
                let ty = self.place_ty(p, locals)?;
                let bytes = self.eval_place(p, locals)?.get(self)?;
                let result = self.compute_discriminant(ty, bytes)?;
                Owned(result.to_le_bytes().to_vec())
            }
            Rvalue::Repeat(it, len) => {
                let len = match try_const_usize(self.db, len.as_ref()) {
                    Some(it) => it as usize,
                    None => not_supported!("non evaluatable array len in repeat Rvalue"),
                };
                let val = self.eval_operand(it, locals)?.get(self)?;
                let size = len * val.len();
                Owned(val.iter().copied().cycle().take(size).collect())
            }
            Rvalue::CopyForDeref(_) => not_supported!("copy for deref"),
            Rvalue::Aggregate(kind, values) => {
                let values = values
                    .iter()
                    .map(|it| self.eval_operand(it, locals))
                    .collect::<Result<'db, Vec<_>>>()?;
                match kind {
                    AggregateKind::Array(_) => {
                        let mut r = vec![];
                        for it in values {
                            let value = it.get(self)?;
                            r.extend(value);
                        }
                        Owned(r)
                    }
                    AggregateKind::Tuple(ty) => {
                        let layout = self.layout(ty.as_ref())?;
                        Owned(self.construct_with_layout(
                            layout.size.bytes_usize(),
                            &layout,
                            None,
                            values.iter().map(|&it| it.into()),
                        )?)
                    }
                    AggregateKind::Union(it, f) => {
                        let layout = self.layout_adt((*it).into(), GenericArgs::empty())?;
                        let offset = layout
                            .fields
                            .offset(u32::from(f.local_id.into_raw()) as usize)
                            .bytes_usize();
                        let op = values[0].get(self)?;
                        let mut result = vec![0; layout.size.bytes_usize()];
                        result[offset..offset + op.len()].copy_from_slice(op);
                        Owned(result)
                    }
                    AggregateKind::Adt(it, subst) => {
                        let (size, variant_layout, tag) =
                            self.layout_of_variant(*it, subst.as_ref(), locals)?;
                        Owned(self.construct_with_layout(
                            size,
                            &variant_layout,
                            tag,
                            values.iter().map(|&it| it.into()),
                        )?)
                    }
                    AggregateKind::Closure(ty) => {
                        let layout = self.layout(ty.as_ref())?;
                        Owned(self.construct_with_layout(
                            layout.size.bytes_usize(),
                            &layout,
                            None,
                            values.iter().map(|&it| it.into()),
                        )?)
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
                            Owned(id.to_le_bytes()[0..ptr_size].to_vec())
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
                CastKind::IntToInt
                | CastKind::PtrToPtr
                | CastKind::PointerExposeAddress
                | CastKind::PointerFromExposedAddress => {
                    let current_ty = self.operand_ty(operand, locals)?;
                    let is_signed = matches!(current_ty.kind(), TyKind::Int(_)).into();
                    let current = pad16(self.eval_operand(operand, locals)?.get(self)?, is_signed);
                    let dest_size = self.size_of_sized(
                        target_ty.as_ref(),
                        locals,
                        "destination of int to int cast",
                    )?;
                    Owned(current[0..dest_size].to_vec())
                }
                CastKind::FloatToInt => {
                    let ty = self.operand_ty(operand, locals)?;
                    let TyKind::Float(ty) = ty.kind() else {
                        not_supported!("invalid float to int cast");
                    };
                    let value = self.eval_operand(operand, locals)?.get(self)?;
                    let is_signed = matches!(target_ty.as_ref().kind(), TyKind::Int(_));
                    let dest_size = self.size_of_sized(
                        target_ty.as_ref(),
                        locals,
                        "destination of float to int cast",
                    )?;
                    let soft = match ty {
                        rustc_type_ir::FloatTy::F16 => Some(apfloat_to_int(
                            from_bytes!(f16, u16, value),
                            dest_size * 8,
                            is_signed,
                        )),
                        rustc_type_ir::FloatTy::F128 => Some(apfloat_to_int(
                            from_bytes!(f128, u128, value),
                            dest_size * 8,
                            is_signed,
                        )),
                        _ => None,
                    };
                    if let Some(bits) = soft {
                        return Ok(Owned(bits.to_le_bytes()[0..dest_size].to_vec()));
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
                    Owned(result[0..dest_size].to_vec())
                }
                CastKind::FloatToFloat => {
                    let ty = self.operand_ty(operand, locals)?;
                    let TyKind::Float(ty) = ty.kind() else {
                        not_supported!("invalid float to int cast");
                    };
                    let value = self.eval_operand(operand, locals)?.get(self)?;
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
                        rustc_type_ir::FloatTy::F32 => Owned((value as f32).to_le_bytes().to_vec()),
                        rustc_type_ir::FloatTy::F64 => Owned(value.to_le_bytes().to_vec()),
                        rustc_type_ir::FloatTy::F16 | rustc_type_ir::FloatTy::F128 => {
                            unreachable!("converted above")
                        }
                    }
                }
                CastKind::IntToFloat => {
                    let current_ty = self.operand_ty(operand, locals)?;
                    let is_signed = matches!(current_ty.kind(), TyKind::Int(_)).into();
                    let value = pad16(self.eval_operand(operand, locals)?.get(self)?, is_signed);
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
                        rustc_type_ir::FloatTy::F32 => Owned(single.to_le_bytes().to_vec()),
                        rustc_type_ir::FloatTy::F64 => Owned(double.to_le_bytes().to_vec()),
                        rustc_type_ir::FloatTy::F16 => {
                            Owned((of::<f16>(value, signed) as u16).to_le_bytes().to_vec())
                        }
                        rustc_type_ir::FloatTy::F128 => {
                            Owned(of::<f128>(value, signed).to_le_bytes().to_vec())
                        }
                    }
                }
                // The pointer keeps the function's id, like the fn pointer it came from.
                CastKind::FnPtrToPtr => {
                    Owned(self.eval_operand(operand, locals)?.get(self)?.to_vec())
                }
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
                let len = from_bytes!(usize, self.eval_operand(len, locals)?.get(self)?);
                let index = from_bytes!(usize, self.eval_operand(index, locals)?.get(self)?);
                format!("index out of bounds: the len is {len} but the index is {index}")
            }
            AssertKind::Overflow(op, ..) => format!("Overflow in {op:?}"),
            AssertKind::OverflowNeg(_) => "Overflow in Neg".to_owned(),
            AssertKind::DivisionByZero(_) => "Overflow in Div".to_owned(),
            AssertKind::RemainderByZero(_) => "Overflow in Rem".to_owned(),
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

    fn coerce_unsized_look_through_fields<T>(
        &self,
        ty: Ty<'db>,
        goal: impl Fn(TyKind<'db>) -> Option<T>,
    ) -> Result<'db, T> {
        let kind = ty.kind();
        if let Some(it) = goal(kind) {
            return Ok(it);
        }
        match kind {
            TyKind::Adt(adt_ef, subst) if let AdtId::StructId(struct_id) = adt_ef.def_id() => {
                let field_types = self.db.field_types(struct_id.into());
                if let Some(ty) = field_types
                    .iter()
                    .last()
                    .map(|it| it.1.ty().instantiate(self.interner(), subst))
                {
                    return self.coerce_unsized_look_through_fields(ty.skip_norm_wip(), goal);
                }
            }
            TyKind::Pat(ty, _) => return self.coerce_unsized_look_through_fields(ty, goal),
            _ => (),
        }
        Err(MirEvalError::CoerceUnsizedError(ty.store()))
    }

    fn coerce_unsized(
        &mut self,
        addr: Interval,
        current_ty: Ty<'db>,
        target_ty: Ty<'db>,
    ) -> Result<'db, IntervalOrOwned> {
        fn for_ptr<'db>(it: TyKind<'db>) -> Option<Ty<'db>> {
            match it {
                TyKind::RawPtr(ty, _) | TyKind::Ref(_, ty, _) => Some(ty),
                _ => None,
            }
        }
        let target_ty = self.coerce_unsized_look_through_fields(target_ty, for_ptr)?;
        let current_ty = self.coerce_unsized_look_through_fields(current_ty, for_ptr)?;

        self.unsizing_ptr_from_addr(target_ty, current_ty, addr)
    }

    /// Adds metadata to the address and create the fat pointer result of the unsizing operation.
    fn unsizing_ptr_from_addr(
        &mut self,
        target_ty: Ty<'db>,
        current_ty: Ty<'db>,
        addr: Interval,
    ) -> Result<'db, IntervalOrOwned> {
        use IntervalOrOwned::*;
        Ok(match &target_ty.kind() {
            TyKind::Slice(_) => match &current_ty.kind() {
                TyKind::Array(_, size) => {
                    let len = match try_const_usize(self.db, *size) {
                        None => {
                            not_supported!("unevaluatble len of array in coerce unsized")
                        }
                        Some(it) => it as usize,
                    };
                    let mut r = Vec::with_capacity(16);
                    let addr = addr.get(self)?;
                    r.extend(addr.iter().copied());
                    r.extend(word_bytes(len));
                    Owned(r)
                }
                t => {
                    not_supported!("slice unsizing from non array type {t:?}")
                }
            },
            TyKind::Dynamic(..) => {
                let vtable = self.vtable_map.id(current_ty);
                let mut r = Vec::with_capacity(16);
                let addr = addr.get(self)?;
                r.extend(addr.iter().copied());
                r.extend(word_bytes(vtable));
                Owned(r)
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

    fn layout_of_variant(
        &mut self,
        it: VariantId,
        subst: GenericArgs<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, (usize, Arc<Layout>, Option<(usize, usize, i128)>)> {
        let adt = it.adt_id(self.db);
        if let Some(f) = locals.body.owner.as_variant()
            && let VariantId::EnumVariantId(it) = it
            && let AdtId::EnumId(e) = adt
            && f.lookup(self.db).parent == e
        {
            // Computing the exact size of enums require resolving the enum discriminants. In order to prevent loops (and
            // infinite sized type errors) we use a dummy layout
            let i = self.const_eval_discriminant(it)?;
            return Ok((16, self.layout(Ty::new_empty_tuple(self.interner()))?, Some((0, 16, i))));
        }
        let layout = self.layout_adt(adt, subst)?;
        Ok(match &layout.variants {
            Variants::Single { .. } | Variants::Empty => (layout.size.bytes_usize(), layout, None),
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
                    Arc::new(variant_layout),
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
    ) -> Result<'db, Vec<u8>> {
        let mut result = vec![0; size];
        if let Some((offset, size, value)) = tag {
            match result.get_mut(offset..offset + size) {
                Some(it) => it.copy_from_slice(&value.to_le_bytes()[0..size]),
                None => {
                    return Err(MirEvalError::InternalError(
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
                        format!("field offset ({offset}) is out of bounds 0..{size}").into(),
                    ));
                }
            }
        }
        Ok(result)
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
                            let interval = self.allocate_allocation_in_heap(locals, allocation)?;
                            self.constants.insert(Either::Right(allocation), interval);
                            interval
                        }
                    },
                }
            }
        })
    }

    /// The value of a literal that `line!()`, `column!()`, `file!()` or a `concat!` of them
    /// expanded to. The expansion has a placeholder there, for the code not to change with
    /// every edit above it, and a constant is what it is where it is written.
    fn placed(
        &mut self,
        it: &Operand,
        ty: Ty<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, Option<Interval>> {
        use crate::mir::location::Placed;
        let (Some(MirSpan::ExprId(expr)), InferBodyId::DefWithBodyId(owner)) =
            (it.span, locals.body.owner)
        else {
            return Ok(None);
        };
        Ok(match (crate::mir::location::placed(self.db, owner, expr), ty.kind()) {
            (Some(Placed::Number(number)), TyKind::Uint(rustc_type_ir::UintTy::U32)) => {
                let addr = self.heap_allocate(4, 4)?;
                self.write_memory(addr, &number.to_le_bytes())?;
                Some(Interval::new(addr, 4))
            }
            (Some(Placed::Text(text)), TyKind::Ref(_, to, _)) if to.is_str() => {
                let at = self.heap_allocate(text.len(), 1)?;
                self.write_memory(at, text.as_bytes())?;
                let size = self.ptr_size();
                let addr = self.heap_allocate(2 * size, size)?;
                self.write_memory(addr, &word_bytes(at.to_usize()))?;
                self.write_memory(addr.offset(size), &word_bytes(text.len()))?;
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

    fn allocate_valtree_in_heap(
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
                    let ref_addr = self.heap_allocate(self.ptr_size() * 2, self.ptr_size())?;
                    self.write_memory(ref_addr, &bytes_addr.to_bytes())?;
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
                    let ref_addr = self.heap_allocate(self.ptr_size() * 2, self.ptr_size())?;
                    self.write_memory(ref_addr, &items_addr.to_bytes())?;
                    let mut len = [0; 16];
                    len[..size_of::<usize>()].copy_from_slice(&items.len().to_le_bytes());
                    self.write_memory(ref_addr.offset(self.ptr_size()), &len[..self.ptr_size()])?;
                    Ok(Interval::new(ref_addr, self.ptr_size() * 2))
                }
                TyKind::Dynamic(..) => not_supported!("`dyn Trait` consts not supported yet"),
                _ => {
                    let inner_addr = self.allocate_valtree_in_heap(inner_ty, valtree, locals)?;
                    let ref_addr = self.heap_allocate(self.ptr_size(), self.ptr_size())?;
                    self.write_memory(ref_addr, &inner_addr.addr.to_bytes())?;
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
            ConstKind::Unevaluated(UnevaluatedConst { def: const_id, args: subst }) => {
                let mut id = const_id.0;
                let mut subst = subst;
                if let GeneralConstId::ConstId(c) = id {
                    let (c, s) = lookup_impl_const(&self.infcx, self.param_env.param_env, c, subst);
                    id = GeneralConstId::ConstId(c);
                    subst = s;
                }
                if let GeneralConstId::AnonConstId(konst) = id
                    && konst.is_inline_const(self.db)
                {
                    return self.inline_const_in_heap(konst, subst, locals);
                }
                let allocation = match id {
                    GeneralConstId::ConstId(const_id) => {
                        self.db.const_eval(const_id, subst, Some(self.param_env)).map_err(|e| {
                            let name = id.name(self.db);
                            MirEvalError::ConstEvalError(name, Box::new(e))
                        })?
                    }
                    GeneralConstId::StaticId(static_id) => {
                        self.db.const_eval_static(static_id).map_err(|e| {
                            let name = id.name(self.db);
                            MirEvalError::ConstEvalError(name, Box::new(e))
                        })?
                    }
                    GeneralConstId::AnonConstId(anon_const_id) => self
                        .db
                        .anon_const_eval(anon_const_id, subst, Some(self.param_env))
                        .map_err(|e| {
                            let name = id.name(self.db);
                            MirEvalError::ConstEvalError(name, Box::new(e))
                        })?,
                };
                self.allocate_allocation_in_heap(locals, allocation)
            }
            _ => not_supported!("evaluating unknown const"),
        }
    }

    /// Whether every use of `konst` reads the same copy ([`Self::constants`]): not of a
    /// static, which may be written, nor of an inline `const { .. }`, which is run again as
    /// its `TypeId`s are this evaluation's ([`Self::inline_const_in_heap`]).
    fn shares_one_copy(&self, konst: Const<'db>) -> bool {
        match konst.kind() {
            ConstKind::Value(_) => true,
            ConstKind::Unevaluated(UnevaluatedConst { def, .. }) => match def.0 {
                GeneralConstId::ConstId(_) => true,
                GeneralConstId::StaticId(_) => false,
                GeneralConstId::AnonConstId(it) => !it.is_inline_const(self.db),
            },
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
            konst.into(),
            subst.store(),
            self.param_env.store(),
        )
        .map_err(|e| {
            let name = GeneralConstId::AnonConstId(konst).name(self.db);
            MirEvalError::ConstEvalError(name, Box::new(ConstEvalError::MirLowerError(e)))
        })?;
        let mark = self.stack.len();
        let result = self.interpret_mir(body, iter::empty(), None)?;
        let ty = body.locals[return_slot()].ty.as_ref();
        let align = self.size_align_of(ty, locals)?.map_or(1, |it| it.1);
        let addr = self.heap_allocate(result.size, align)?;
        let interval = Interval::new(addr, result.size);
        interval.write_from_interval(self, result)?;
        self.free_stack(mark);
        Ok(interval)
    }

    fn allocate_allocation_in_heap(
        &mut self,
        locals: &Locals<'db>,
        allocation: Allocation<'db>,
    ) -> Result<'db, Interval> {
        let (size, align) =
            self.size_align_of(allocation.ty, locals)?.unwrap_or((allocation.memory.len(), 1));
        let addr = self.heap_allocate(size, align)?;
        self.write_allocation(locals, allocation, addr, size)?;
        Ok(Interval::new(addr, size))
    }

    /// Copies a constant's value to `addr`, along with the memory it points to.
    fn write_allocation(
        &mut self,
        locals: &Locals<'db>,
        allocation: Allocation<'db>,
        addr: Address,
        size: usize,
    ) -> Result<'db, ()> {
        let AllocationData { ty, memory: ref v, ref memory_map } = *allocation;
        // Its ids are of the evaluation that made it, and mean another type here.
        if memory_map.has_type_ids() {
            not_supported!("a constant computed from a `TypeId`, read by another evaluation");
        }
        let patch_map = memory_map.transform_addresses(|b, align| {
            let addr = self.heap_allocate(b.len(), align)?;
            self.write_memory(addr, b)?;
            Ok(addr.to_usize())
        })?;
        let v: Cow<'_, [u8]> = if size != v.len() {
            // Handle self enum
            if size == 16 && v.len() < 16 {
                Cow::Owned(pad16(v, IsSigned::No).to_vec())
            } else if size < 16 && v.len() == 16 {
                Cow::Borrowed(&v[0..size])
            } else {
                return Err(MirEvalError::InvalidConst);
            }
        } else {
            Cow::Borrowed(v)
        };
        self.write_memory(addr, &v)?;
        self.patch_addresses(
            &patch_map,
            memory_map,
            |bytes| match memory_map {
                MemoryMap::Empty | MemoryMap::Simple(_) => {
                    Err(MirEvalError::InvalidVTableId(from_bytes!(usize, bytes)))
                }
                MemoryMap::Complex(cm) => cm.vtable.ty_of_bytes(bytes),
            },
            addr,
            ty,
            locals,
        )
    }

    fn eval_place(&mut self, p: &StoredPlace, locals: &Locals<'db>) -> Result<'db, Interval> {
        let addr = self.place_addr(p, locals)?;
        Ok(Interval::new(
            addr,
            self.size_of_sized(self.place_ty(p, locals)?, locals, "type of this place")?,
        ))
    }

    fn read_memory(&self, addr: Address, size: usize) -> Result<'db, &[u8]> {
        if size == 0 {
            return Ok(&[]);
        }
        let (mem, pos) = match addr {
            Stack(it) => (&self.stack, it),
            Heap(it) => {
                self.check_static_is_evaluated(it, size)?;
                (&self.heap, it)
            }
            Invalid(it) => {
                return Err(MirEvalError::UndefinedBehavior(format!(
                    "read invalid memory address {it} with size {size}"
                )));
            }
        };
        mem.get(pos..pos + size)
            .ok_or_else(|| MirEvalError::UndefinedBehavior("out of bound memory read".to_owned()))
    }

    /// Fails a read of heap memory that stands in for a static left unevaluated.
    fn check_static_is_evaluated(&self, pos: usize, size: usize) -> Result<'db, ()> {
        let read = self
            .unevaluated_statics
            .iter()
            .find(|&&(start, len, _)| pos < start + len && start < pos + size);
        let Some(&(.., id)) = read else { return Ok(()) };
        self.missing_static.set(Some(id));
        not_supported!("reading a static that was left unevaluated")
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
        Err(MirEvalError::UndefinedBehavior(format!(
            "modifying the initial value of static `{}` from a static's initializer",
            StaticSignature::of(self.db, id).name.as_str()
        )))
    }

    fn write_memory_using_ref(&mut self, addr: Address, size: usize) -> Result<'db, &mut [u8]> {
        if size == 0 {
            return Ok(&mut []);
        }
        if let Heap(pos) = addr {
            self.check_static_write(pos, size)?;
        }
        let (mem, pos) = match addr {
            Stack(it) => (&mut self.stack, it),
            Heap(it) => (&mut self.heap, it),
            Invalid(it) => {
                return Err(MirEvalError::UndefinedBehavior(format!(
                    "write invalid memory address {it} with size {size}"
                )));
            }
        };
        mem.get_mut(pos..pos + size)
            .ok_or_else(|| MirEvalError::UndefinedBehavior("out of bound memory write".to_owned()))
    }

    fn write_memory(&mut self, addr: Address, r: &[u8]) -> Result<'db, ()> {
        if r.is_empty() {
            return Ok(());
        }
        self.write_memory_using_ref(addr, r.len())?.copy_from_slice(r);
        Ok(())
    }

    fn copy_from_interval_or_owned(
        &mut self,
        addr: Address,
        r: IntervalOrOwned,
    ) -> Result<'db, ()> {
        match r {
            IntervalOrOwned::Borrowed(r) => self.copy_from_interval(addr, r),
            IntervalOrOwned::Owned(r) => self.write_memory(addr, &r),
        }
    }

    fn copy_from_interval(&mut self, addr: Address, r: Interval) -> Result<'db, ()> {
        if r.size == 0 {
            return Ok(());
        }

        let oob = || MirEvalError::UndefinedBehavior("out of bounds memory write".to_owned());

        if let Heap(src) = r.addr {
            self.check_static_is_evaluated(src, r.size)?;
        }
        if let Heap(dst) = addr {
            self.check_static_write(dst, r.size)?;
        }
        match (addr, r.addr) {
            (Stack(dst), Stack(src)) => {
                if self.stack.len() < src + r.size || self.stack.len() < dst + r.size {
                    return Err(oob());
                }
                self.stack.copy_within(src..src + r.size, dst)
            }
            (Heap(dst), Heap(src)) => {
                if self.heap.len() < src + r.size || self.heap.len() < dst + r.size {
                    return Err(oob());
                }
                self.heap.copy_within(src..src + r.size, dst)
            }
            (Stack(dst), Heap(src)) => {
                self.stack
                    .get_mut(dst..dst + r.size)
                    .ok_or_else(oob)?
                    .copy_from_slice(self.heap.get(src..src + r.size).ok_or_else(oob)?);
            }
            (Heap(dst), Stack(src)) => {
                self.heap
                    .get_mut(dst..dst + r.size)
                    .ok_or_else(oob)?
                    .copy_from_slice(self.stack.get(src..src + r.size).ok_or_else(oob)?);
            }
            _ => {
                return Err(MirEvalError::UndefinedBehavior(format!(
                    "invalid memory write at address {addr:?}"
                )));
            }
        }

        Ok(())
    }

    fn size_align_of(
        &self,
        ty: Ty<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, Option<(usize, usize)>> {
        if let Some(layout) = self.layout_cache.borrow().get(&ty) {
            return Ok(layout
                .is_sized()
                .then(|| (layout.size.bytes_usize(), layout.align.bytes() as usize)));
        }
        if let Some(f) = locals.body.owner.as_variant()
            && let Some((AdtId::EnumId(e), _)) = ty.as_adt()
            && f.lookup(self.db).parent == e
        {
            // Computing the exact size of enums require resolving the enum discriminants. In order to prevent loops (and
            // infinite sized type errors) we use a dummy size
            return Ok(Some((16, 16)));
        }
        let layout = self.layout(ty);
        if self.assert_placeholder_ty_is_unused
            && matches!(layout, Err(MirEvalError::LayoutError(LayoutError::HasPlaceholder, _)))
        {
            return Ok(Some((0, 1)));
        }
        let layout = layout?;
        Ok(layout.is_sized().then(|| (layout.size.bytes_usize(), layout.align.bytes() as usize)))
    }

    /// The size of a value of a struct whose last field is a `str` or a slice, or of one of
    /// those, given the pointer metadata. `None` for a `dyn` tail.
    fn size_of_unsized_struct(&self, ty: Ty<'db>, metadata: &[u8]) -> Result<'db, Option<usize>> {
        let len = from_bytes!(usize, metadata);
        Ok(match ty.kind() {
            TyKind::Str => Some(len),
            TyKind::Slice(inner) => Some(self.layout(inner)?.size.bytes_usize() * len),
            TyKind::Adt(adt, subst) => {
                let AdtId::StructId(id) = adt.def_id() else { return Ok(None) };
                let layout = self.layout(ty)?;
                let field_types = self.db.field_types(id.into());
                let Some((_, last)) = field_types.iter().next_back() else { return Ok(None) };
                let last = last.ty().instantiate(self.interner(), subst).skip_norm_wip();
                let Some(tail) = self.size_of_unsized_struct(last, metadata)? else {
                    return Ok(None);
                };
                let start = layout.fields.offset(field_types.iter().count() - 1).bytes_usize();
                let align = layout.align.bytes() as usize;
                Some((start + tail).next_multiple_of(align))
            }
            _ => None,
        })
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
        match Address::from_usize(addr) {
            Stack(_) => {
                let i = self.stack_allocations.partition_point(|it| it.start <= addr);
                let &allocation = self.stack_allocations.get(i.checked_sub(1)?)?;
                (addr - allocation.start < allocation.size).then_some(allocation)
            }
            Heap(offset) => {
                let (start, block) = self.heap_block_at(offset)?;
                let start = Heap(start).to_usize();
                Some(Allocated { start, size: block.size, align: block.align })
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
    /// aligned to, or less when it is at an offset that is a multiple of less.
    fn note_align(&self, map: &mut ComplexMemoryMap<'db>, addr: usize) {
        let Some(allocation) = self.allocation_at(addr) else { return };
        let align = match addr - allocation.start {
            0 => allocation.align,
            offset => allocation.align.min(1 << offset.trailing_zeros()),
        };
        map.note_align(addr, align);
    }

    /// Whether `more` bytes of stack or heap fit the evaluation's budget.
    fn check_memory(&self, more: usize) -> Result<'db, ()> {
        let used = self.stack.len() as u64 + self.heap.len() as u64;
        if (more as u64).checked_add(used).is_none_or(|it| it > self.budget.memory) {
            return Err(MirEvalError::OverBudget {
                spent: Budget::Memory(self.budget.memory),
                at: None,
            });
        }
        Ok(())
    }

    /// Memory of the evaluator's own ([`Origin::Evaluator`]), zeroed.
    fn heap_allocate(&mut self, size: usize, align: usize) -> Result<'db, Address> {
        self.allocate(size, align, Origin::Evaluator)
    }

    /// A zeroed block of the heap: a freed one of its size class and alignment when there is
    /// one, else one at the end.
    fn allocate(&mut self, size: usize, align: usize, origin: Origin) -> Result<'db, Address> {
        if !align.is_power_of_two() || align > 10000 {
            return Err(MirEvalError::UndefinedBehavior(format!("Alignment {align} is invalid")));
        }
        if size == 0 {
            // As a local of no size: aligned, of no block, so not where the next block starts.
            return Ok(Invalid(align));
        }
        let class = size_class(size);
        let pos = match self.free_blocks.get_mut(&(class, align)).and_then(Vec::pop) {
            Some(pos) => {
                self.heap[pos..pos + class].fill(0);
                pos
            }
            None => {
                let pos = self.heap.len().next_multiple_of(align);
                self.check_memory((pos - self.heap.len()).saturating_add(class))?;
                self.heap.resize(pos + class, 0);
                pos
            }
        };
        self.heap_blocks.insert(pos, HeapBlock { size, align, class, origin });
        Ok(Heap(pos))
    }

    /// Frees the block `addr` starts, which `origin` made; `what` names the call, for its
    /// error. As rustc's evaluation, freeing anything else is undefined behavior.
    fn deallocate(&mut self, addr: Address, origin: Origin, what: &str) -> Result<'db, ()> {
        let block = match addr {
            Heap(pos) => self.heap_blocks.get(&pos).copied().map(|it| (pos, it)),
            _ => None,
        };
        let Some((pos, block)) = block.filter(|(_, it)| it.origin == origin) else {
            return Err(MirEvalError::UndefinedBehavior(format!(
                "`{what}` of a pointer that is not the start of memory allocated by it"
            )));
        };
        self.heap_blocks.remove(&pos);
        self.free_blocks.entry((block.class, block.align)).or_default().push(pos);
        Ok(())
    }

    /// `__rust_realloc`: the block grows or shrinks in place while it fits its size class,
    /// else its bytes move to a new block and it is freed.
    fn reallocate(&mut self, addr: Address, new_size: usize) -> Result<'db, Address> {
        let block = match addr {
            Heap(pos) => self.heap_blocks.get(&pos).copied().map(|it| (pos, it)),
            _ => None,
        };
        let Some((pos, block)) = block.filter(|(_, it)| it.origin == Origin::Allocator) else {
            return Err(MirEvalError::UndefinedBehavior(
                "`__rust_realloc` of a pointer that is not the start of memory allocated by it"
                    .to_owned(),
            ));
        };
        if new_size != 0 && new_size <= block.class {
            if new_size > block.size {
                self.heap[pos + block.size..pos + new_size].fill(0);
            }
            self.heap_blocks.insert(pos, HeapBlock { size: new_size, ..block });
            return Ok(addr);
        }
        let new = self.allocate(new_size, block.align, Origin::Allocator)?;
        let kept = block.size.min(new_size);
        Interval { addr: new, size: kept }
            .write_from_interval(self, Interval { addr, size: kept })?;
        self.deallocate(addr, Origin::Allocator, "__rust_realloc")?;
        Ok(new)
    }

    /// `const_make_global`: the `const_allocate` block `addr` starts may be in the value of
    /// a constant from now on, and is immutable.
    fn make_global(&mut self, addr: Address) -> Result<'db, ()> {
        let block = match addr {
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
                    .to_owned(),
            )),
        }
    }

    /// Refuses `addr` in the value of the constant: memory the program allocated, which it
    /// would free at run time, or `const_allocate` memory not made global. rustc refuses the
    /// latter alike ("encountered `const_allocate` pointer in final value that was not made
    /// global"), and has no `Box` or `Vec` that allocates in a constant.
    fn may_be_in_value(&self, addr: usize) -> Result<'db, ()> {
        let Heap(offset) = Address::from_usize(addr) else { return Ok(()) };
        match self.heap_block_at(offset) {
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
                let variants: Vec<VariantId> = match adt.def_id() {
                    AdtId::StructId(it) => vec![it.into()],
                    AdtId::UnionId(it) => vec![it.into()],
                    AdtId::EnumId(it) => it
                        .enum_variants(self.db)
                        .variants
                        .values()
                        .map(|&(v, _)| v.into())
                        .collect(),
                };
                variants.into_iter().all(|variant| {
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
    fn holds_no_type_id(&self, ty: Ty<'db>, seen: &mut FxHashSet<Ty<'db>>) -> bool {
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
                let variants: Vec<VariantId> = match adt.def_id() {
                    AdtId::StructId(it) => vec![it.into()],
                    AdtId::UnionId(it) => vec![it.into()],
                    AdtId::EnumId(it) => it
                        .enum_variants(self.db)
                        .variants
                        .values()
                        .map(|&(v, _)| v.into())
                        .collect(),
                };
                variants.into_iter().all(|variant| {
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
        fn rec<'db>(
            this: &Evaluator<'db>,
            bytes: &[u8],
            ty: Ty<'db>,
            locals: &Locals<'db>,
            mm: &mut ComplexMemoryMap<'db>,
            stack_depth_limit: usize,
        ) -> Result<'db, ()> {
            if stack_depth_limit.checked_sub(1).is_none() {
                return Err(MirEvalError::StackOverflow);
            }
            if let TyKind::RawPtr(..) | TyKind::Ref(..) = ty.kind()
                && let Some(addr) = bytes.get(..this.ptr_size())
            {
                let addr = from_bytes!(usize, addr);
                let pointee = this.static_memory.iter().find(|&&(start, size, _)| {
                    addr.checked_sub(start).is_some_and(|offset| offset < size.max(1))
                });
                if let Some(&(start, size, id)) = pointee {
                    let pointee = crate::StaticMemory { start, size, id };
                    if !mm.statics.contains(&pointee) {
                        mm.statics.push(pointee);
                    }
                    return Ok(());
                }
                this.may_be_in_value(addr)?;
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
                TyKind::Pat(base, _) => rec(this, bytes, base, locals, mm, stack_depth_limit - 1)?,
                TyKind::RawPtr(..) => {
                    let Some(addr) = bytes.get(..this.ptr_size()) else { return Ok(()) };
                    let addr = from_bytes!(usize, addr);
                    behind_raw(this, addr, mm, stack_depth_limit - 1)?;
                }
                TyKind::Ref(_, t, _) => {
                    let size = this.size_align_of(t, locals)?;
                    match size {
                        Some((size, _)) => {
                            let addr_usize = from_bytes!(usize, bytes);
                            let addr = Address::from_usize(addr_usize);
                            if addr.is_of_nothing(size) {
                                return Ok(());
                            }
                            let bytes = this.read_memory(addr, size)?.to_vec();
                            mm.insert(addr_usize, bytes.clone().into());
                            this.note_align(mm, addr_usize);
                            rec(this, &bytes, t, locals, mm, stack_depth_limit - 1)?;
                        }
                        None => {
                            let mut check_inner = None;
                            let (addr, meta) = bytes.split_at(bytes.len() / 2);
                            if let TyKind::Adt(..) = t.kind() {
                                // A struct with an unsized tail. FIXME: pointers in the
                                // struct itself are not followed.
                                if let Some(size) = this.size_of_unsized_struct(t, meta)? {
                                    let addr = Address::from_bytes(addr)?;
                                    let b = this.read_memory(addr, size)?;
                                    mm.insert(addr.to_usize(), b.into());
                                    this.note_align(mm, addr.to_usize());
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
                                _ => from_bytes!(usize, meta),
                            };
                            let size = element_size * count;
                            let addr = Address::from_bytes(addr)?;
                            if addr.is_of_nothing(size) {
                                return Ok(());
                            }
                            let b = this.read_memory(addr, size)?;
                            mm.insert(addr.to_usize(), b.into());
                            this.note_align(mm, addr.to_usize());
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
                        )?;
                    }
                }
                TyKind::Adt(adt, subst) => match adt.def_id() {
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
                                rec(this, word, pointer, locals, mm, stack_depth_limit - 1)?;
                            }
                        }
                    }
                },
                TyKind::Alias(AliasTy { kind: AliasTyKind::Projection { .. }, .. }) => {
                    let mut ocx = ObligationCtxt::new(&this.infcx);
                    let ty = ocx
                        .structurally_normalize_ty(
                            &ObligationCause::dummy(),
                            this.param_env.param_env,
                            ty,
                        )
                        .map_err(|_| MirEvalError::NotSupported("couldn't normalize".to_owned()))?;

                    rec(this, bytes, ty, locals, mm, stack_depth_limit - 1)?;
                }
                _ => (),
            }
            Ok(())
        }
        /// Keeps the whole allocation `addr` is in, the type of which a raw pointer does not
        /// say, and what every word of it that is in another allocation or a static points
        /// to: the reader of the map takes each such word to be a pointer, as it is in a
        /// `NonNull<()>` to a value with a `&str` in it (std's `io::Error` of a message).
        fn behind_raw<'db>(
            this: &Evaluator<'db>,
            addr: usize,
            mm: &mut ComplexMemoryMap<'db>,
            stack_depth_limit: usize,
        ) -> Result<'db, ()> {
            if stack_depth_limit.checked_sub(1).is_none() {
                return Err(MirEvalError::StackOverflow);
            }
            let pointee = this.static_memory.iter().find(|&&(start, size, _)| {
                addr.checked_sub(start).is_some_and(|offset| offset < size.max(1))
            });
            if let Some(&(start, size, id)) = pointee {
                let pointee = crate::StaticMemory { start, size, id };
                if !mm.statics.contains(&pointee) {
                    mm.statics.push(pointee);
                }
                return Ok(());
            }
            this.may_be_in_value(addr)?;
            let Some(Allocated { start, size, align }) = this.allocation_at(addr) else {
                return Ok(());
            };
            if mm.memory.get(&start).is_some_and(|it| it.len() >= size) {
                return Ok(());
            }
            let Ok(memory) = this.read_memory(Address::from_usize(start), size) else {
                return Ok(());
            };
            let memory = memory.to_vec();
            mm.insert(start, memory.clone().into());
            mm.note_align(start, align);
            for word in memory.chunks_exact(this.ptr_size()) {
                let inner = from_bytes!(usize, word);
                if inner != 0 {
                    behind_raw(this, inner, mm, stack_depth_limit - 1)?;
                }
            }
            Ok(())
        }
        let mut mm = ComplexMemoryMap::default();
        rec(self, bytes, ty, locals, &mut mm, self.stack_depth_limit - 1)?;
        Ok(mm)
    }

    fn patch_addresses(
        &mut self,
        patch_map: &FxHashMap<usize, usize>,
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
        use rustc_type_ir::TyKind;
        // The vtable of a `dyn` pointer is the id the value's own evaluation gave the type it
        // points at, and this one has another for it.
        if let TyKind::RawPtr(t, _) | TyKind::Ref(_, t, _) = ty.kind()
            && let TyKind::Dynamic(..) = t.kind()
        {
            let at = addr.offset(self.ptr_size());
            let concrete = ty_of_bytes(self.read_memory(at, self.ptr_size())?)?;
            let new_id = self.vtable_map.id(concrete);
            self.write_memory(at, &word_bytes(new_id))?;
        }
        // A pointer into a static points into this evaluator's copy of it.
        if let TyKind::RawPtr(..) | TyKind::Ref(..) = ty.kind() {
            let current = from_bytes!(usize, self.read_memory(addr, self.ptr_size())?);
            if let Some((id, offset)) = source.static_at(current) {
                let location = self.eval_static(id, locals)?;
                let start = from_bytes!(usize, self.read_memory(location, self.ptr_size())?);
                return self.write_memory(addr, &word_bytes(start + offset));
            }
        }
        match ty.kind() {
            TyKind::Ref(_, t, _) => {
                let size = self.size_align_of(t, locals)?;
                match size {
                    Some(_) => {
                        let current = from_bytes!(usize, self.read_memory(addr, my_size)?);
                        let patched = match patch_map.get(&current) {
                            Some(it) => {
                                self.write_memory(addr, &word_bytes(*it))?;
                                *it
                            }
                            None => current,
                        };
                        self.patch_addresses(
                            patch_map,
                            source,
                            ty_of_bytes,
                            Address::from_usize(patched),
                            t,
                            locals,
                        )?;
                    }
                    None => {
                        let bytes = self.read_memory(addr, my_size)?;
                        let (current, metadata) = bytes.split_at(my_size / 2);
                        let metadata = metadata.to_vec();
                        let current = from_bytes!(usize, current);
                        let patched = match patch_map.get(&current) {
                            Some(it) => {
                                self.write_memory(addr, &word_bytes(*it))?;
                                *it
                            }
                            None => current,
                        };
                        let patched = Address::from_usize(patched);
                        if let TyKind::Dynamic(..) = t.kind() {
                            let concrete = self.vtable_map.ty_of_bytes(&metadata)?;
                            self.patch_addresses(
                                patch_map,
                                source,
                                ty_of_bytes,
                                patched,
                                concrete,
                                locals,
                            )?;
                        } else if let TyKind::Slice(inner) = t.kind() {
                            let len = from_bytes!(usize, metadata);
                            let size = self.size_of_sized(inner, locals, "slice item to patch")?;
                            let len = if size == 0 { 0 } else { len };
                            for i in 0..len {
                                self.patch_addresses(
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
                self.patch_addresses(patch_map, source, ty_of_bytes, addr, base, locals)?
            }
            // A raw pointer can point anywhere into the allocation that was kept for it.
            TyKind::RawPtr(..) => {
                let current = from_bytes!(usize, self.read_memory(addr, self.ptr_size())?);
                if let Some((start, _)) = source.memory_at(current)
                    && let Some(new_start) = patch_map.get(&start)
                {
                    self.write_memory(addr, &word_bytes(new_start + (current - start)))?;
                } else if let Ok(fn_ty) = ty_of_bytes(&word_bytes(current))
                    && matches!(fn_ty.kind(), TyKind::FnDef(..) | TyKind::Closure(..))
                {
                    // A function cast to a raw pointer is still the id the value's own
                    // evaluation gave the function, and this one has another for it.
                    let new_id = self.vtable_map.id(fn_ty);
                    self.write_memory(addr, &word_bytes(new_id))?;
                }
            }
            TyKind::FnPtr(_, _) => {
                let ty = ty_of_bytes(self.read_memory(addr, my_size)?)?;
                let new_id = self.vtable_map.id(ty);
                self.write_memory(addr, &word_bytes(new_id))?;
            }
            TyKind::Adt(id, args) => match id.def_id() {
                AdtId::StructId(s) => {
                    for (i, (_, field)) in self.db.field_types(s.into()).iter().enumerate() {
                        let offset = layout.fields.offset(i).bytes_usize();
                        let ty = field.ty().instantiate(self.interner(), args).skip_norm_wip();
                        self.patch_addresses(
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
                let normalized = ObligationCtxt::new(&self.infcx)
                    .structurally_normalize_ty(
                        &ObligationCause::dummy(),
                        self.param_env.param_env,
                        ty,
                    )
                    .unwrap_or(ty);
                if !matches!(normalized.kind(), TyKind::Alias(..)) {
                    self.patch_addresses(patch_map, source, ty_of_bytes, addr, normalized, locals)?;
                }
            }
        }
        Ok(())
    }

    fn exec_fn_pointer(
        &mut self,
        bytes: Interval,
        destination: Interval,
        args: &[IntervalAndTy<'db>],
        locals: &Locals<'db>,
        target_bb: Option<BasicBlockId>,
        span: MirSpan,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        let id = from_bytes!(usize, bytes.get(self)?);
        let next_ty = self.vtable_map.ty(id)?;
        use rustc_type_ir::TyKind;
        match next_ty.kind() {
            TyKind::FnDef(def, generic_args) => {
                self.exec_fn_def(def.0, generic_args, destination, args, locals, target_bb, span)
            }
            TyKind::Closure(id, generic_args) => self.exec_closure(
                id.0,
                bytes.slice(0..0),
                generic_args,
                destination,
                args,
                locals,
                span,
                None,
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
    ) -> Result<'db, Option<StackFrame<'db>>> {
        let mir_body = super::optimize::interpreted_closure_body(
            self.db,
            closure,
            generic_args.store(),
            self.param_env.store(),
        )
        .map_err(|it| MirEvalError::MirLowerErrorForClosure(closure, it))?;
        let by_reference =
            mir_body.locals[mir_body.param_locals[0]].ty.as_ref().as_reference().is_some();
        let closure_addr = closure_data.addr;
        let closure_data = if by_reference {
            closure_addr.to_bytes().to_vec()
        } else {
            closure_data.get(self)?.to_owned()
        };
        let arg_bytes = iter::once(Ok(closure_data))
            .chain(args.iter().map(|it| Ok(it.get(self)?.to_owned())))
            .collect::<Result<'db, Vec<_>>>()?;
        let mark = self.stack.len();
        let interval = self
            .interpret_mir(mir_body, arg_bytes.into_iter().map(IntervalOrOwned::Owned), None)
            .map_err(|e| {
                MirEvalError::InFunction(
                    Box::new(e),
                    vec![(Either::Right(closure), span, locals.body.owner)],
                )
            })?;
        destination.write_from_interval(self, interval)?;
        // A `Fn` or `FnMut` closure called by value through `FnOnce::call_once` is dropped
        // after the call, as rustc's `ClosureOnceShim` does: nothing else owns it then. One
        // whose body takes it by value dropped it itself.
        if let Some(closure_ty) = consumed
            && by_reference
        {
            self.run_drop_glue_deep(closure_ty, locals, closure_addr, &[], span)?;
        }
        self.free_stack(mark);
        Ok(None)
    }

    fn exec_fn_def(
        &mut self,
        def: CallableDefId,
        generic_args: GenericArgs<'db>,
        destination: Interval,
        args: &[IntervalAndTy<'db>],
        locals: &Locals<'db>,
        target_bb: Option<BasicBlockId>,
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
                        target_bb,
                        span,
                    );
                }
                self.exec_fn_with_args(
                    def,
                    args,
                    generic_args,
                    locals,
                    destination,
                    target_bb,
                    span,
                )
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
            return Ok(r.clone());
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
            let Either::Left(imp) = imp else {
                not_supported!("evaluating builtin derive impls is not supported")
            };

            let mir_body = super::optimize::interpreted_body(
                self.db,
                imp.into(),
                generic_args.store(),
                self.param_env.store(),
            )
            .map_err(|e| {
                MirEvalError::InFunction(
                    Box::new(MirEvalError::MirLowerError(imp, e)),
                    vec![(Either::Left(imp), span, locals.body.owner)],
                )
            })?;
            MirOrDynIndex::Mir(mir_body)
        };
        self.mir_or_dyn_index_cache.borrow_mut().insert((def, generic_args), r.clone());
        Ok(r)
    }

    fn exec_fn_with_args(
        &mut self,
        mut def: FunctionId,
        args: &[IntervalAndTy<'db>],
        generic_args: GenericArgs<'db>,
        locals: &Locals<'db>,
        destination: Interval,
        target_bb: Option<BasicBlockId>,
        span: MirSpan,
    ) -> Result<'db, Option<StackFrame<'db>>> {
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
                let first_arg = arg_bytes.clone().next().unwrap();
                let first_arg = first_arg.get(self)?;
                let ty = self
                    .vtable_map
                    .ty_of_bytes(&first_arg[self.ptr_size()..self.ptr_size() * 2])?;
                let mut args_for_target = args.to_vec();
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
                self.exec_fn_with_args(
                    def,
                    &args_for_target,
                    generics_for_target,
                    locals,
                    destination,
                    target_bb,
                    span,
                )
            }
            MirOrDynIndex::Mir(body) => self.exec_looked_up_function(
                body,
                locals,
                def,
                arg_bytes,
                span,
                destination,
                target_bb,
            ),
        }
    }

    fn exec_looked_up_function(
        &mut self,
        mir_body: &'db MirBody<'db>,
        locals: &Locals<'db>,
        def: FunctionId,
        arg_bytes: impl Iterator<Item = IntervalOrOwned>,
        span: MirSpan,
        destination: Interval,
        target_bb: Option<BasicBlockId>,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        if let Some(target_bb) = target_bb {
            let span = (span, locals.body.owner);
            let (mut locals, prev_stack_ptr) =
                self.create_locals_for_body(mir_body, Some(destination))?;
            self.fill_locals_for_body(mir_body, &mut locals, arg_bytes.into_iter())?;
            Ok(Some(StackFrame {
                locals,
                destination: Some(target_bb),
                prev_stack_ptr,
                span,
                callee: Some(def),
            }))
        } else {
            let mark = self.stack.len();
            let called = Some((def, span, locals.body.owner));
            let result = self.interpret_mir(mir_body, arg_bytes, called).map_err(|e| {
                MirEvalError::InFunction(
                    Box::new(e),
                    vec![(Either::Left(def), span, locals.body.owner)],
                )
            })?;
            destination.write_from_interval(self, result)?;
            self.free_stack(mark);
            Ok(None)
        }
    }

    fn exec_fn_trait(
        &mut self,
        def: FunctionId,
        args: &[IntervalAndTy<'db>],
        generic_args: GenericArgs<'db>,
        locals: &Locals<'db>,
        destination: Interval,
        target_bb: Option<BasicBlockId>,
        span: MirSpan,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        let func = args
            .first()
            .ok_or_else(|| MirEvalError::InternalError("fn trait with no arg".into()))?;
        let mut func_ty = func.ty;
        let mut func_data = func.interval;
        while let TyKind::Ref(_, z, _) = func_ty.kind() {
            func_ty = z;
            if matches!(func_ty.kind(), TyKind::Dynamic(..)) {
                let id =
                    from_bytes!(usize, &func_data.get(self)?[self.ptr_size()..self.ptr_size() * 2]);
                func_data = func_data.slice(0..self.ptr_size());
                func_ty = self.vtable_map.ty(id)?;
            }
            let size = self.size_of_sized(func_ty, locals, "self type of fn trait")?;
            func_data = Interval { addr: Address::from_bytes(func_data.get(self)?)?, size };
        }
        match func_ty.kind() {
            TyKind::FnDef(def, subst) => {
                self.exec_fn_def(def.0, subst, destination, &args[1..], locals, target_bb, span)
            }
            TyKind::FnPtr(..) => {
                self.exec_fn_pointer(func_data, destination, &args[1..], locals, target_bb, span)
            }
            TyKind::Closure(closure, subst) => {
                let consumed = (Some(def) == self.cached_fn_once_trait_func
                    && func_ty == func.ty)
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
                    &[arg0.clone(), arg1],
                    generic_args,
                    locals,
                    destination,
                    target_bb,
                    span,
                )
            }
        }
    }

    fn eval_static(&mut self, st: StaticId, locals: &Locals<'db>) -> Result<'db, Address> {
        if let Some(o) = self.static_locations.get(&st) {
            return Ok(*o);
        };
        let static_data = StaticSignature::of(self.db, st);
        let is_read = self.read_statics.as_ref().is_none_or(|read| read.contains(&st));
        let unevaluated = if is_read || static_data.flags.contains(StaticFlags::EXTERN) {
            None
        } else {
            let ty = self.db.type_for_static(st).instantiate_identity().skip_norm_wip();
            self.size_align_of(ty, locals)?
        };
        let result = if let Some((size, align)) = unevaluated {
            let addr = self.heap_allocate(size, align)?;
            if let Heap(start) = addr {
                self.unevaluated_statics.push((start, size, st));
            }
            Interval::new(addr, size)
        } else if !static_data.flags.contains(StaticFlags::EXTERN) {
            let allocation = self.db.const_eval_static(st).map_err(|e| {
                MirEvalError::ConstEvalError(static_data.name.as_str().to_owned(), Box::new(e))
            })?;
            let (size, align) =
                self.size_align_of(allocation.ty, locals)?.unwrap_or((allocation.memory.len(), 1));
            let result = Interval::new(self.heap_allocate(size, align)?, size);
            // Known before its value is written: what it points to may point back at it.
            let location = self.locate_static(st, result.addr)?;
            self.write_allocation(locals, allocation, result.addr, size)?;
            self.static_memory.push((result.addr.to_usize(), size, st));
            return Ok(location);
        } else {
            let ty = InferenceResult::of(self.db, DefWithBodyId::from(st))
                .expr_ty(Body::of(self.db, st.into()).root_expr());
            let Some((size, align)) = self.size_align_of(ty, locals)? else {
                not_supported!("unsized extern static");
            };
            let addr = self.heap_allocate(size, align)?;
            Interval::new(addr, size)
        };
        self.static_memory.push((result.addr.to_usize(), result.size, st));
        self.locate_static(st, result.addr)
    }

    /// Records where the memory of a static is, and gives the address of a pointer to it.
    fn locate_static(&mut self, st: StaticId, memory: Address) -> Result<'db, Address> {
        let addr = self.heap_allocate(self.ptr_size(), self.ptr_size())?;
        self.write_memory(addr, &memory.to_bytes())?;
        self.static_locations.insert(st, addr);
        Ok(addr)
    }

    fn const_eval_discriminant(&self, variant: EnumVariantId) -> Result<'db, i128> {
        let r = self.db.const_eval_discriminant(variant);
        match r {
            Ok(r) => Ok(r),
            Err(e) => {
                let db = self.db;
                let loc = variant.lookup(db);
                let edition = self.crate_id.data(self.db).edition;
                let name = format!(
                    "{}::{}",
                    EnumSignature::of(self.db, loc.parent).name.display(db, edition),
                    loc.parent
                        .enum_variants(self.db)
                        .variant_name_by_id(variant)
                        .unwrap()
                        .display(db, edition),
                );
                Err(MirEvalError::ConstEvalError(name, Box::new(e)))
            }
        }
    }

    fn drop_place(
        &mut self,
        place: &StoredPlace,
        locals: &mut Locals<'db>,
        span: MirSpan,
    ) -> Result<'db, ()> {
        let (addr, ty, metadata) = self.place_addr_and_ty_and_metadata(place, locals)?;
        let metadata = match metadata {
            Some(it) => it.get(self)?.to_vec(),
            None => vec![],
        };
        self.run_drop_glue_deep(ty, locals, addr, &metadata, span)
    }

    /// Drops the value of type `ty` at `addr`, as rustc's `drop_in_place::<ty>`: its `Drop`
    /// impl, then its parts in order (a struct's or tuple's fields, a closure's captures, the
    /// fields of the variant an enum holds, an array's or slice's elements). A `Box` drops what
    /// it points to before its `Drop` frees it.
    ///
    /// The parts wait on a stack of steps rather than the native one: a list of a thousand
    /// boxes is a thousand levels deep.
    fn run_drop_glue_deep(
        &mut self,
        ty: Ty<'db>,
        locals: &Locals<'db>,
        addr: Address,
        metadata: &[u8],
        span: MirSpan,
    ) -> Result<'db, ()> {
        if self.lang_items().Drop_drop.is_none() {
            return Ok(());
        }
        let mut steps = vec![DropStep::Glue(ty, addr, metadata.to_vec())];
        while let Some(step) = steps.pop() {
            match step {
                DropStep::Glue(ty, addr, metadata) => {
                    self.drop_glue_step(ty, locals, addr, &metadata, span, &mut steps)?
                }
                DropStep::DropImpl(ty, addr) => self.call_drop_impl(ty, locals, addr, span)?,
            }
        }
        Ok(())
    }

    /// One step of [`Self::run_drop_glue_deep`]: runs what comes first in dropping the value
    /// and pushes the rest onto `steps`, last first.
    fn drop_glue_step(
        &mut self,
        ty: Ty<'db>,
        locals: &Locals<'db>,
        addr: Address,
        metadata: &[u8],
        span: MirSpan,
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
                        MirEvalError::NotSupported(format!("dropping {ty:?}, which does not normalize"))
                    })?;
                if matches!(normalized.kind(), TyKind::Alias(..) | TyKind::Infer(..)) {
                    not_supported!("dropping {ty:?}, which does not normalize");
                }
                normalized
            }
            _ => ty,
        };
        let mut parts = Vec::new();
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
                    let AdtId::StructId(s) = id else { not_supported!("a box that is not a struct") };
                    let field_types = self.db.field_types(s.into());
                    let Some((_, pointer)) = field_types.iter().next() else {
                        not_supported!("a box with no pointer");
                    };
                    let pointer = pointer.ty().instantiate(self.interner(), subst).skip_norm_wip();
                    let size = self.size_of_sized(pointer, locals, "pointer of a box")?;
                    let offset = layout.fields.offset(0).bytes_usize();
                    let bytes = self.read_memory(addr.offset(offset), size)?.to_vec();
                    let (data, contents_metadata) = bytes.split_at(self.ptr_size());
                    parts.push(DropStep::Glue(
                        subst.type_at(0),
                        Address::from_bytes(data)?,
                        contents_metadata.to_vec(),
                    ));
                    parts.push(DropStep::DropImpl(ty, addr));
                } else {
                    self.call_drop_impl(ty, locals, addr, span)?;
                }
                let bytes;
                let (variant, layout): (VariantId, Cow<'_, Layout>) = match id {
                    AdtId::StructId(s) => (s.into(), Cow::Borrowed(&*layout)),
                    AdtId::UnionId(_) => return Ok(()),
                    AdtId::EnumId(e) => {
                        bytes = self.read_memory(addr, layout.size.bytes_usize())?.to_vec();
                        match detect_variant_from_bytes(
                            &layout,
                            self.db,
                            self.target_data_layout,
                            &bytes,
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
                    let ty = field_types[field].ty().instantiate(self.interner(), subst).skip_norm_wip();
                    // The metadata of an unsized struct is its last field's (`RefCell<dyn FnMut()>`).
                    let metadata = if Some(index) == last { metadata.to_vec() } else { Vec::new() };
                    parts.push(DropStep::Glue(ty, addr.offset(offset), metadata));
                }
            }
            TyKind::Tuple(fields) => self.field_steps(ty, fields.iter(), addr, &mut parts)?,
            TyKind::Closure(_, args) => {
                let upvars = args.as_closure().tupled_upvars_ty();
                self.field_steps(ty, upvars.tuple_fields().iter(), addr, &mut parts)?;
            }
            TyKind::Array(inner, len) => {
                let Some(len) = try_const_usize(self.db, len) else {
                    not_supported!("dropping an array of no evaluated length");
                };
                self.element_steps(inner, len as usize, locals, addr, &mut parts)?;
            }
            TyKind::Slice(inner) => {
                let len = from_bytes!(usize, metadata);
                self.element_steps(inner, len, locals, addr, &mut parts)?;
            }
            TyKind::Dynamic(..) => {
                if metadata.is_empty() {
                    not_supported!("dropping {ty:?} with no vtable");
                }
                let concrete_ty = self.vtable_map.ty_of_bytes(metadata)?;
                parts.push(DropStep::Glue(concrete_ty, addr, Vec::new()));
            }
            _ => not_supported!("drop glue of {ty:?}"),
        }
        steps.extend(parts.into_iter().rev());
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

    /// `<ty as Drop>::drop` on the value at `addr`, if `ty` has a `Drop` impl.
    fn call_drop_impl(
        &mut self,
        ty: Ty<'db>,
        locals: &Locals<'db>,
        addr: Address,
        span: MirSpan,
    ) -> Result<'db, ()> {
        let Some(drop_fn) = self.lang_items().Drop_drop else { return Ok(()) };
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
                drop_impl.into(),
                drop_args.store(),
                self.param_env.store(),
            )
            .map_err(|it| MirEvalError::MirLowerError(drop_impl, it))?;
            self.exec_looked_up_function(
                body,
                locals,
                drop_impl,
                iter::once(IntervalOrOwned::Owned(addr.to_bytes().to_vec())),
                span,
                Interval { addr: Address::Invalid(0), size: 0 },
                None,
            )?;
        }
        Ok(())
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
            parts.push(DropStep::Glue(field, addr.offset(offset), Vec::new()));
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
        parts.extend((0..len).map(|i| DropStep::Glue(inner, addr.offset(i * size), Vec::new())));
        Ok(())
    }

    fn write_to_stdout(&mut self, interval: Interval) -> Result<'db, ()> {
        self.stdout.extend(interval.get(self)?.to_vec());
        Ok(())
    }

    fn write_to_stderr(&mut self, interval: Interval) -> Result<'db, ()> {
        self.stderr.extend(interval.get(self)?.to_vec());
        Ok(())
    }
}

pub fn render_const_using_debug_impl<'db>(
    db: &'db dyn HirDatabase,
    owner: InferBodyId<'db>,
    c: Allocation<'db>,
    ty: Ty<'db>,
) -> Result<'db, String> {
    let mut evaluator = Evaluator::new(db, owner, false, None)?;
    let locals = &Locals {
        ptr: ArenaMap::new(),
        body: db
            .runtime_mir_body(owner)
            .map_err(|_| MirEvalError::NotSupported("unreachable".to_owned()))?,
    };
    let data = evaluator.allocate_allocation_in_heap(locals, c)?;
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
    evaluator.write_memory(argument, &data.addr.to_bytes())?;
    let debug_fmt_fn_ptr = evaluator.vtable_map.id(Ty::new_fn_def(
        evaluator.interner(),
        CallableDefId::FunctionId(debug_fmt_fn).into(),
        GenericArgs::new_from_slice(&[ty.into()]),
    ));
    evaluator.write_memory(argument.offset(ptr_size), &word_bytes(debug_fmt_fn_ptr))?;
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
    evaluator.write_memory(arguments, &template.to_bytes())?;
    evaluator.write_memory(arguments.offset(ptr_size), &argument.to_bytes())?;
    let Some(ValueNs::FunctionId(format_fn)) = resolver.resolve_path_in_value_ns_fully(
        db,
        &hir_def::expr_store::path::Path::from_known_path_with_no_generic(path![std::fmt::format]),
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
        from_bytes!(usize, message_string[0..ptr_size]),
        from_bytes!(usize, message_string[ptr_size..2 * ptr_size]),
        from_bytes!(usize, message_string[2 * ptr_size..3 * ptr_size]),
    ];
    let Some(addr) = words.into_iter().map(Address::from_usize).find(|it| matches!(it, Heap(_)))
    else {
        // No heap buffer means the formatted string is empty.
        return Ok(String::new());
    };
    let size = words
        .into_iter()
        .filter(|&it| !matches!(Address::from_usize(it), Heap(_)))
        .min()
        .unwrap_or(0);
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
    fn from_bytes(bytes: &[u8], is_signed: bool) -> Self {
        match (bytes.len(), is_signed) {
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
                never!("invalid integer size: {len}, signed: {is_signed}");
                Self::I32(0)
            }
        }
    }

    fn to_bytes(self) -> Vec<u8> {
        macro_rules! m {
            ( [] $( $int_ty:ident )+ ) => {
                match self {
                    $( Self::$int_ty(v) => v.to_le_bytes().to_vec() ),+
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
