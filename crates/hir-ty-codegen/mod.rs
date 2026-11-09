//! blaze spike: lower RA MIR to native code with Cranelift.
//!
//! Every local lives in memory. Calls follow rustc's (unstable) Rust ABI so the
//! output can be linked against rustc-compiled objects.

#![feature(allocator_api, allocator_ext)]
// Unsafe code is in `llvm` (LLVM's C API) and `lent` alone (docs/v2/plan/design/unsafe.md).
#![deny(unsafe_code)]

extern crate ra_ap_rustc_ast_ir as rustc_ast_ir;
extern crate ra_ap_rustc_index as rustc_index;
extern crate rustc_abi;
extern crate rustc_type_ir;

use std::{borrow::Cow, fmt, sync::LazyLock};

use base_db::{Crate, Db};
use blaze_mem::Frame;
use frame_map::{FrameHashMap, FrameHashSet, FrameMap};
use std::cell::Cell;

use blaze_hash::{HashMap, HashSet};
use blaze_query::db::{AsId, FromId};
use cranelift_codegen::{
    ir::{
        self, AbiParam, ArgumentPurpose, AtomicRmwOp, InstBuilder, MemFlagsData, Signature,
        StackSlotData, StackSlotKind, TrapCode, Type, Value,
        condcodes::{FloatCC, IntCC},
        types,
    },
    isa,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Switch};
use cranelift_module::{
    DataDescription, DataId, FuncId, Linkage, Module, ModuleReloc, ModuleRelocTarget,
    default_libcall_names,
};
use cranelift_object::{ObjectBuilder, ObjectModule};
use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, FunctionId, HasModule, ItemContainerId, Lookup, StaticId,
    TraitId, VariantId,
    attrs::AttrFlags,
    expr_store::{
        Body,
        scope::{ExprScopes, ScopeId},
    },
    hir::{BindingId, ExprId},
    layout::{BackendRepr, Primitive, Scalar, TagEncoding, Variants},
    signatures::{
        ConstSignature, FunctionSignature, StaticFlags, StaticSignature, StructFlags,
        StructSignature,
    },
    src::HasSource,
};
use hir_expand::{FileRange, HirFileId, InFile, MacroDefKind, builtin::BuiltinFnLikeExpander};
use rustc_abi::{ExternAbi, Integer, ReprFlags, Size};
use rustc_ast_ir::Mutability;
use rustc_index::Idx;
use rustc_type_ir::UintTy;
use rustc_type_ir::inherent::{
    AdtDef as _, BoundExistentialPredicates as _, GenericArgs as _, IntoKind, SliceLike, Ty as _,
};
use std::alloc::Global;
use syntax::{TextRange, TextSize};
use triomphe::Arc;

use hir_def::lang_item::LangItems;
use hir_ty::{
    InferBodyId, InferenceResult, MemoryMap, ParamEnvAndCrate, PointerAt, PointerCast,
    consteval::{ConstEvalError, try_const_usize},
    db::HirDatabase,
    db::{GeneralConstId, InternedClosureId, InternedCoroutineId},
    drop::{DropGlue, destructor, has_drop_glue},
    intrinsic::{AtomicOp, Intrinsic, SimdOp},
    layout::{CoroutineFrame, Layout, RustcEnumVariantIdx, RustcFieldIdx},
    method_resolution::{is_dyn_method, lookup_impl_const},
    next_solver::{
        AliasConst, AliasConstKind, Allocation, AllocationData, AnonConstIdWrapper, ConstKind,
        DbInterner, FreeConstAliasId, GenericArgs, InherentAssocConstId, Region, RegionExt,
        StoredGenericArgs, StoredTy, TraitAssocConstId, TraitRef, Ty, TyKind, TypingMode, ValTree,
        infer::{DbInternerInferExt, InferCtxt},
    },
    traits::StoredParamEnvAndCrate,
    utils::detect_variant_from_bytes,
};
use hir_ty_mir::mir::is_by_move_coroutine;

use self::debuginfo::{DebugContext, FunctionDebug, LineRow, SlotPlaces, VariableDebug};
use self::fragment::{
    Code, CompiledFunction, CompiledStatic, DataRequest, FuncRequest, FuncTarget, Named, Recorder,
    Reloc as CodeReloc, RelocName, Relocs, Requests, TextPart, Variable,
};
use self::unwind::FunctionUnwind;
use self::vtable::{Slot, Upcast, VtableLayout};
pub use hir_ty_mir::mir::location::name_sources;
use hir_ty_mir::mir::{
    AggregateKind, AssertKind, BasicBlockId, BinOp, CallerLocation, CastKind, FrameStorage,
    LocalId, MirBody, MirEvalError, MirOptLevel, MirOptions, MirOwner, MirPhase, MirSpan, MirStage,
    Operand, OperandKind, PlaceTy, ProjectionElem, Rvalue, StatementKind, StoredPlace,
    TerminatorKind, UnOp, caller_location, dispatch, interpret_mir,
    location::{
        ConcatPart, concat_parts, line_and_column, located_concats, location_macros, location_of,
        macro_location_of, reported_range, source_location_in_file, source_range, track_caller,
        written_call, written_range,
    },
    monomorphized, monomorphized_promoted_mir_body_query,
    overflow::crate_checks_overflow,
    owner_of, source_span, written_checked_at,
};

mod asm;
pub use asm::Assembled;
mod avx512;
mod backend;
use backend::Step;
pub use backend::{Backend, builds_edits, links_own, sections_of_their_own};
mod copies;
mod debuginfo;
pub use debuginfo::Sidecar;
mod f16_f128;
mod float_pair;
mod fncache;
mod fragment;
mod frame_map;
#[cfg(feature = "llvm")]
mod imports;
#[cfg(feature = "llvm")]
#[allow(unsafe_code)]
mod llvm;
#[cfg(feature = "llvm")]
pub use llvm::Unhandled as LlvmUnhandled;

/// The CPU of this host and its features as LLVM names them (`+avx2,-avx512f,...`), what
/// rustc's `-C target-cpu=native` is; `None` without LLVM.
pub fn host_cpu() -> Option<(String, String)> {
    #[cfg(feature = "llvm")]
    {
        Some(llvm::host_cpu())
    }
    #[cfg(not(feature = "llvm"))]
    {
        None
    }
}
mod neon;
mod overlap;
mod places;
use self::places::{row_location, source_location};
#[allow(unsafe_code)]
mod lent;
mod promote;
use lent::Lent;
mod spare;
pub(crate) use spare::with_context;
mod stable;
pub use stable::{block_place, function_place, lower_declared, static_place};
mod target;
pub use target::{Arch, Calls, Features, ObjectFormat, Target, UnwindFormat, VariableArguments};
pub mod tier;

mod type_id;
mod unwind;
pub mod v2;
mod vtable;
pub mod wasm;
mod x86;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CodegenError {
    /// The function uses something this backend can't compile yet. The build stops on it:
    /// nothing else compiles the function instead.
    Unsupported(String),
    /// MIR lowering or layout failed in RA.
    Frontend(String),
    Backend(String),
    /// LLVM, which `BLAZE_BACKEND=llvm` or a tier asked for, has no code for the function.
    /// The build stops on it: Cranelift never compiles the function in LLVM's place.
    #[cfg(feature = "llvm")]
    LlvmRefused(LlvmUnhandled),
    /// What blaze can't do and no other path may stand in for: the build stops with this
    /// error, naming what and where. Never left to rustc, never a build in full instead.
    Refused(String),
    /// Cranelift's verifier refused the IR blaze made of the function ([`verified`]): a bug
    /// of blaze's IR builder (`stage` "built") or of the passes after it ("settled").
    InvalidIr {
        stage: &'static str,
        errors: String,
    },
}

impl fmt::Display for CodegenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodegenError::Unsupported(it) => write!(f, "unsupported: {it}"),
            CodegenError::Frontend(it) => write!(f, "frontend: {it}"),
            CodegenError::Backend(it) => write!(f, "backend: {it}"),
            #[cfg(feature = "llvm")]
            CodegenError::LlvmRefused(why) => write!(f, "LLVM has no code for the function: {why}"),
            CodegenError::Refused(it) => write!(f, "{it}"),
            CodegenError::InvalidIr { stage, errors } => write!(
                f,
                "blaze made IR that Cranelift's verifier refuses (as {stage}), a bug of blaze's \
                 code generator: {errors}"
            ),
        }
    }
}

type Result<T> = std::result::Result<T, CodegenError>;

macro_rules! unsupported {
    ($($arg:tt)*) => {
        // alloc: the error's message
        return Err(CodegenError::Unsupported(format!($($arg)*)))
    };
}

// After `unsupported!`, which they use.
mod arbitrary_int;
use arbitrary_int::NarrowInt;
mod fma;
mod sve;
mod wasm_simd;

/// Stops the build with an error ([`CodegenError::Refused`]).
macro_rules! refused {
    ($($arg:tt)*) => {
        // alloc: the refusal's message
        return Err(CodegenError::Refused(format!($($arg)*)))
    };
}

/// Where cargo keeps the sources it fetched of registries and of git, as what is stored names
/// it: the same on every machine, whoever's home the directory is in.
pub const CARGO_HOME: &str = "/cargo";

/// `path` with the directory `to` in place of the directory `from` it is under.
pub fn moved(path: &str, from: &str, to: &str) -> Option<String> {
    let rest = path.strip_prefix(from)?;
    // alloc: the moved path, which stable.rs keeps as a file's stored name (owned there)
    rest.starts_with(['/', '\\']).then(|| format!("{to}{rest}"))
}

/// The sources of the standard library: rustc names them where they are on this machine, and
/// what is stored names them where they were when the library was built, as every machine can.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibrarySources {
    /// The directory they are in here: the library's, `library/`.
    pub here: String,
    /// The directory they were built in, `/rustc/<commit>/library`.
    pub built_in: String,
}

impl LibrarySources {
    fn stored(&self, path: &str) -> Option<String> {
        moved(path, &self.here, &self.built_in)
    }

    fn local(&self, path: &str) -> Option<String> {
        moved(path, &self.built_in, &self.here)
    }
}

/// Symbol names come from the rustc-built artifacts we link against.
/// What rustc's copy of a static under a symbol LLVM gave it is to a copy of ours, when it
/// is not simply the whole static: rustc keeps a static the code of one codegen unit uses to
/// that unit, and LLVM renames it as it optimizes (`SERVING.0`, `SERVING.llvm.<hash>`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RustcStaticCopy {
    /// A part LLVM split off the static, which is the whole when it has this many bytes.
    Sized(u64),
    /// No copy of ours can be that one, for the reason given.
    Unshared(String),
}

pub trait SymbolResolver {
    fn function(&self, func: FunctionId) -> Option<intern::Symbol>;
    fn static_(&self, id: StaticId) -> Option<intern::Symbol>;
    /// What rustc's copy under `symbol`, which [`Self::static_`] named, is to ours, when
    /// that is not simply the whole static.
    fn static_copy(&self, _symbol: &str) -> Option<RustcStaticCopy> {
        None
    }
    /// Whether rustc built the function under a symbol that only its own object can link to.
    fn private(&self, _func: FunctionId) -> bool {
        false
    }
    /// A non-generic function by path, e.g. `core::panicking::panic_bounds_check`.
    fn path(&self, path: &str) -> Option<&str>;
    /// The path of an item, e.g. `core::any::TypeId`.
    fn item(&self, item: hir_def::ModuleDefId) -> Option<String>;
    /// The struct, enum or union [`Self::item`] spells `path`, when one of a module does.
    fn adt_at(&self, _path: &str) -> Option<hir_def::AdtId> {
        None
    }
    /// The name rustc gives a source file in panic messages.
    fn source_path(&self, file: base_db::FileId) -> Option<intern::Symbol>;
    /// The directory the names of [`Self::source_path`] are relative to, when the build is
    /// to tell a debugger where its code comes from.
    fn debug_directory(&self) -> Option<String> {
        None
    }
    /// The file a debugger reads the entries of the code from, when the build keeps them
    /// apart from what it links: the object itself, which has them in sections a linker
    /// leaves out.
    fn debug_apart(&self) -> Option<String> {
        None
    }
    /// Whether a debugger is told of the variables of the code too, and not of its lines
    /// alone.
    fn describes_variables(&self) -> bool {
        true
    }
    /// Whether the object is split into parts that a linker reads apart, so that each unit
    /// of its description is best in sections of its own, beside the functions it describes.
    fn debug_grouped(&self) -> bool {
        false
    }
    /// Whether every function keeps its frame pointer, as `-C force-frame-pointers` has it:
    /// code that walks its own frames (bun's backtraces) reads them.
    fn keeps_frame_pointers(&self) -> bool {
        false
    }
    /// Whether the program's rustc build has the symbol, defined or referred to.
    fn defines(&self, _symbol: &str) -> bool {
        false
    }
    /// Whether what the program is linked with has the symbol: rustc's build, or an object
    /// blaze built before, which an entry of the store may name.
    fn provides(&self, symbol: &str) -> bool {
        self.defines(symbol)
    }
    /// Whether the executable the patch goes into has the symbol: rustc's own link of it
    /// leaves out every function nothing calls.
    fn linked(&self, _symbol: &str) -> bool {
        true
    }
    /// The copies the executable a patch goes into has of the instances of `func`, a generic
    /// function of a module, each by its path as [`Spelling::instance_path`] writes it with
    /// rustc's symbol of it. `None` when that can't be told of each, as of one copied twice.
    fn rustc_instances(&self, _func: FunctionId) -> Option<Vec<(String, String)>> {
        None
    }
    /// Whether the executable a patch goes into has no copy of `func`, which is not generic:
    /// the patch has the one there is.
    fn rustc_has_none(&self, _func: FunctionId) -> bool {
        false
    }
    /// rustc's symbol of the closure at `expr` in the body of `owner`, which is not generic.
    fn closure(&self, _owner: FunctionId, _expr: hir_def::hir::ExprId) -> Option<intern::Symbol> {
        None
    }
    /// The build is no patch of what rustc linked but one in full, which is linked: what
    /// only that executable has is not to be named.
    fn in_full(&self) {}
    /// Whether the build, in full, names what only the executable rustc linked has, for it is
    /// spliced into that: the store keeps nothing of it, which another program can't use.
    fn onto_rustcs(&self) -> bool {
        false
    }
    /// Adds to `out` the `StableCrateId` of each crate that compiled a copy of an instance of a
    /// function named `function`. The caller lends `out` empty, from a buffer it keeps.
    fn makers(&self, _function: &str, _out: &mut Vec<u64>) {}
    /// Adds to `out` the copies rustc made of the instance `symbol` is one of, each with the
    /// `StableCrateId` of the crate that compiled it, in the order the table has them. The
    /// caller lends `out` empty, from a buffer it keeps.
    fn copies(&self, _symbol: &str, _out: &mut Vec<(intern::Symbol, u64)>) {}
    /// Whether rustc compiled the crate, so that what is skipped of it is in the program.
    fn rustc_built(&self, _krate: Crate) -> bool {
        true
    }
    /// Whether nothing linked into the program is rustc's, so that what rustc adds at the
    /// link, the allocator's shim, is the build's to add.
    fn nothing_of_rustc(&self) -> bool {
        false
    }
    /// The name a static of a crate rustc built nothing of is defined under, for objects
    /// built apart from this one to refer to it by. `None`: only this object refers to it.
    fn own_static(&self, _id: StaticId) -> Option<String> {
        None
    }
    /// rustc's `StableCrateId` of a crate, which its symbols carry.
    fn stable_crate_id(&self, krate: Crate) -> Option<u64>;
    /// Where cargo keeps the sources it fetched on this machine, when the crates of the
    /// program tell: [`CARGO_HOME`] stands for it in what is stored.
    fn cargo_home(&self) -> Option<String> {
        None
    }
    /// Where the sources of the standard library are, when the toolchain tells where they
    /// were built.
    fn library_sources(&self) -> Option<LibrarySources> {
        None
    }
    /// Whether data has in it where the checkout is, which another checkout's is not.
    fn names_place(&self, bytes: &[u8]) -> bool {
        _ = bytes;
        false
    }
    /// The function a type is declared in the body of, when nothing else is around it there
    /// and no other type of the body has its name.
    fn declared_in(&self, adt: hir_def::AdtId) -> Option<FunctionId> {
        _ = adt;
        None
    }
    /// How many `impl`s of its module rustc numbers before this one.
    fn impl_place(&self, id: hir_def::ImplId) -> Option<u32> {
        _ = id;
        None
    }
    /// Runs `work` for each of `0..count`, on as many threads as there are, each with a
    /// database of its own and this resolver over it. What `work` computes is kept by the
    /// queries it asks.
    fn in_parallel(&self, count: usize, work: &ParallelWork<'_>) {
        self.in_parallel_beside(count, work, &mut || ());
    }
    /// [`Self::in_parallel`], with `beside` run on this thread while the others work.
    // dyn: a method of the object-safe `SymbolResolver`, run once per parallel phase of a build
    fn in_parallel_beside(&self, count: usize, work: &ParallelWork<'_>, beside: &mut dyn FnMut()) {
        _ = (count, work);
        beside();
    }
}

// dyn: a method of the object-safe `SymbolResolver`, run once per parallel phase of a build
pub type ParallelWork<'a> = dyn Fn(&Db, &dyn SymbolResolver, usize) + Sync + 'a;

/// An x86 operation on two vectors that `FunctionCx::lanewise` does lane by lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lanewise {
    Average,
    MulHigh { signed: bool },
    ShiftLeft,
    ShiftRight,
    ShiftRightArithmetic,
}

/// The lane-by-lane operation and lane type of an LLVM x86 intrinsic `name` (without
/// `llvm.`): `x86.{sse2,avx2,avx512}.{pavg,pmulh,pmulhu,psllv,psrlv,psrav}.{b,w,d,q}[.width]`.
fn x86_lanewise(name: &str) -> Option<(Lanewise, Type)> {
    let mut parts = name.split('.');
    let (Some("x86"), Some("sse2" | "avx2" | "avx512"), Some(op), Some(lane)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if !matches!(parts.next(), None | Some("128" | "256" | "512")) || parts.next().is_some() {
        return None;
    }
    let lane = match lane {
        "b" => types::I8,
        "w" => types::I16,
        "d" => types::I32,
        "q" => types::I64,
        _ => return None,
    };
    let op = match op {
        "pavg" if lane.bits() <= 16 => Lanewise::Average,
        "pmulh" if lane == types::I16 => Lanewise::MulHigh { signed: true },
        "pmulhu" if lane == types::I16 => Lanewise::MulHigh { signed: false },
        "psllv" if lane.bits() >= 16 => Lanewise::ShiftLeft,
        "psrlv" if lane.bits() >= 16 => Lanewise::ShiftRight,
        "psrav" if lane.bits() >= 16 => Lanewise::ShiftRightArithmetic,
        _ => return None,
    };
    Some((op, lane))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum OverflowOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Neg,
    Shl,
    Shr,
    DivByZero,
    RemByZero,
}

/// How a value crosses a call boundary in rustc's Rust ABI.
#[derive(Debug, Clone, Copy)]
enum PassMode {
    Ignore,
    Direct(AbiScalar),
    Pair(AbiScalar, AbiScalar, Size),
    /// Small aggregate passed as one integer register.
    Cast(Type),
    /// Aggregate the C ABI splits over two registers, the second holding what lies eight
    /// bytes in.
    CastPair(Type, Type),
    /// Aggregate AAPCS64 hands over in `count` registers of one type, each holding the next
    /// `unit` bytes of it. `skipped` registers of that kind go unused first: the convention
    /// leaves them out when the value has to start at an even register, and takes them all
    /// out of use when the value has to go to the stack, where the units then land. A unit
    /// of 128 bits is the one that is on the stack at a multiple of sixteen bytes.
    Uniform {
        unit: Type,
        count: u8,
        skipped: u8,
    },
    Indirect,
    /// An unsized value (`unsized_fn_params`): rustc's `PassMode::Indirect` with
    /// `meta_attrs`, the address of the value and then its metadata (length or vtable), the
    /// value staying where the caller has it. In the codegen the place of such a value is a
    /// pair of those two words in memory, as an unsized local's slot is.
    IndirectUnsized,
    /// A vector of AArch64's shorter than 64 bits, whose lanes LLVM widens to make a 64-bit
    /// vector of the same count (`<4 x i8>` as `<4 x i16>`): in the low 64 bits of a vector
    /// register, each lane of `lane` at the bottom of `stride` bytes, the bits above it
    /// undefined.
    Spread {
        lane: Type,
        stride: u8,
    },
    /// The C ABI's by-value aggregate in memory: the caller copies `size` bytes into its
    /// outgoing arguments, which Cranelift does given a pointer. Cranelift has such a copy
    /// at the next multiple of eight bytes, so one that is aligned to sixteen has eight
    /// bytes of nothing passed the same way ahead of it when it would not be at one:
    /// `padded`.
    OnStack {
        size: u32,
        padded: bool,
    },
    /// A `Pair` with a 128-bit integer half that finds one integer register left by the
    /// System V x86-64 convention: that half is passed as `OnStack` passes sixteen bytes,
    /// `Some(padded)`, and the other as a value (see `wide_ints_on_stack`).
    SpilledPair {
        a: AbiScalar,
        b: AbiScalar,
        offset: Size,
        spilled: [Option<bool>; 2],
    },
    /// A `Pair` with a 128-bit integer half that AAPCS64 puts on the stack while integer
    /// registers are left: `skipped[i]` integer registers go unused before half `i`, which
    /// the convention takes out of use (see `wide_ints_after_registers`).
    SkippingPair {
        a: AbiScalar,
        b: AbiScalar,
        offset: Size,
        skipped: [u8; 2],
    },
}

impl PassMode {
    /// What fills one register of the kind the units of a `Uniform` go to.
    fn register(unit: Type) -> Type {
        if unit == types::I128 { types::I64 } else { unit }
    }
}

/// How every value of a call crosses the boundary.
struct FnAbi {
    ret: PassMode,
    args: Vec<PassMode>,
}

/// The first 128-bit integer parameter that finds one integer register left by the System V
/// x86-64 convention, and how many bytes of the stack the parameters before it take, as
/// Cranelift places them. Cranelift and LLVM both put such an integer on the stack, but
/// LLVM gives the register that is left to the next integer parameter and Cranelift gives
/// it to nobody, so the two place the parameters after it differently.
pub(crate) fn lone_register_i128(params: &[AbiParam]) -> Option<(usize, u32)> {
    const INTS: usize = 6;
    const FLOATS: usize = 8;
    let (mut ints, mut floats, mut stack) = (0, 0, 0u32);
    for (at, param) in params.iter().enumerate() {
        let ty = param.value_type;
        if let ArgumentPurpose::StructArgument(size) = param.purpose {
            stack += size;
        } else if ty == types::I128 {
            if ints == INTS - 1 {
                return Some((at, stack));
            }
            if ints > INTS - 2 {
                stack = stack.next_multiple_of(16) + 16;
            }
            ints += 2;
        } else if ty.is_float() || ty.is_vector() {
            if floats < FLOATS {
                floats += 1;
            } else {
                let size = ty.bytes().max(8);
                stack = stack.next_multiple_of(size) + size;
            }
        } else if ints < INTS {
            ints += 1;
        } else {
            stack += 8;
        }
    }
    None
}

/// The first 128-bit integer parameter that AAPCS64 puts on the stack while an integer
/// register is still free, and how many are, as Cranelift places the parameters: such an
/// integer starts at a register that is a multiple of `even` (2, or 1 on Apple's platforms).
/// The convention takes the registers that are left out of use, as LLVM (so rustc) does, but
/// Cranelift gives them to the integer parameters after it, so the two place those
/// differently.
pub(crate) fn stranded_i128(params: &[AbiParam], even: usize) -> Option<(usize, u8)> {
    const REGS: usize = 8;
    let mut ints = 0;
    for (at, param) in params.iter().enumerate() {
        let ty = param.value_type;
        if param.purpose == ArgumentPurpose::StructReturn || ty.is_float() || ty.is_vector() {
            continue;
        }
        if ty == types::I128 {
            let start = ints + ints % even;
            if start + 2 <= REGS {
                ints = start + 2;
            } else if ints < REGS {
                return Some((at, (REGS - ints) as u8));
            }
        } else if ints < REGS {
            ints += 1;
        }
    }
    None
}

/// How AAPCS64 sorts a value, before the registers that are left decide where it goes.
enum Aapcs64 {
    Ignore,
    Scalar(AbiScalar),
    /// A homogeneous aggregate of `count` floats of the type `unit`.
    Floats {
        unit: Type,
        count: u8,
    },
    /// Any other aggregate of at most sixteen bytes.
    Ints {
        size: u64,
        align: u64,
    },
    /// A larger one, which the caller copies and passes the address of.
    Memory,
}

/// What the System V x86-64 ABI calls the class of an eightbyte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RegClass {
    Int,
    Sse,
    /// The upper half of the register of the `Sse` eightbyte before it: an `f128`'s.
    SseUp,
}

#[derive(Debug, Clone, Copy)]
struct AbiScalar {
    ty: Type,
    signed: bool,
}

impl AbiScalar {
    fn param(self) -> AbiParam {
        let param = AbiParam::new(self.ty);
        match (self.ty.is_int() && self.ty.bits() < 32, self.signed) {
            (false, _) => param,
            (true, true) => param.sext(),
            (true, false) => param.uext(),
        }
    }
}

/// A function with concrete generic arguments that we compile ourselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Instance {
    def: InstanceDef,
    args: StoredGenericArgs,
    /// Whether the crate that compiles it has `-C overflow-checks`: the one that asks for it,
    /// until [`ObjectCodegen::settled`] says which crate rustc had compile it.
    checked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum InstanceDef {
    Fn(FunctionId),
    /// An `InternedClosureId`, stored without its database lifetime.
    Closure(blaze_query::db::Id),
    /// An `InternedCoroutineId`: the `Future::poll` of an `async` block. Polled with a null
    /// context, it drops whatever the block holds instead.
    Coroutine(blaze_query::db::Id),
    /// The same of the future an `async` closure gives when it is called by value though it
    /// could be called again: that future owns what the closure captured.
    CoroutineByMove(blaze_query::db::Id),
    /// `drop_in_place` for the type that is the instance's only generic argument.
    DropGlue,
    /// `FnOnce::call_once` in the vtable of the closure type that is the instance's only
    /// generic argument: calls the closure through the pointer it is handed, then drops it.
    CallOnceShim,
    /// A method that takes `self` by value, in a vtable: the slot is called with a pointer to
    /// the value, which the shim hands to the method as the method takes it.
    VtableShim(FunctionId),
    /// What a closure that captures nothing coerces to as a function pointer: calls the
    /// closure type that is the instance's only generic argument, which borrows a receiver,
    /// with a dangling one.
    FnPointerShim,
    /// A function to point at for the fn item or fn pointer type that is the instance's only
    /// generic argument, where what the type names has no address of its own: a constructor,
    /// or a trait method called on a `dyn`. With `receiver`, the function is what the
    /// `Fn`-trait vtable of the type holds, and so also takes the `self` it has no use for.
    ReifyShim {
        receiver: bool,
    },
}

impl blaze_query::db::NamesIds for InstanceDef {
    fn names_ids(&self, out: &mut Vec<blaze_query::db::Id>) {
        match *self {
            InstanceDef::Fn(func) | InstanceDef::VtableShim(func) => out.push(func.as_id()),
            InstanceDef::Closure(id)
            | InstanceDef::Coroutine(id)
            | InstanceDef::CoroutineByMove(id) => out.push(id),
            InstanceDef::DropGlue
            | InstanceDef::CallOnceShim
            | InstanceDef::FnPointerShim
            | InstanceDef::ReifyShim { .. } => {}
        }
    }
}

impl blaze_query::db::NamesIds for Instance {
    fn names_ids(&self, out: &mut Vec<blaze_query::db::Id>) {
        use blaze_query::db::NamesIds;
        self.def.names_ids(out);
        self.args.names_ids(out);
    }
}

impl Instance {
    /// Whether it is compiled per instance: a function, closure or coroutine that is
    /// `hir_ty_mir::mir::home::is_generic`, or a shim. One that is not is compiled once, in its
    /// own crate, as rustc does (`hir_ty_mir::mir::home::env_of` gives it that crate's
    /// environment). A vtable shim is generic here, for keying (its origin is the asker's),
    /// but one of a non-generic method is worked out in that method's crate (`env_of`'s
    /// `own_crate`, given the method's crate as its code).
    fn is_generic(&self) -> bool {
        match self.def {
            InstanceDef::Fn(_)
            | InstanceDef::Closure(_)
            | InstanceDef::Coroutine(_)
            | InstanceDef::CoroutineByMove(_) => {
                hir_ty_mir::mir::home::is_generic(self.args.as_ref())
            }
            InstanceDef::DropGlue
            | InstanceDef::CallOnceShim
            | InstanceDef::VtableShim(_)
            | InstanceDef::FnPointerShim
            | InstanceDef::ReifyShim { .. } => true,
        }
    }

    /// Whether rustc compiles it into each crate that uses it rather than linking every user to
    /// one copy compiled in its own crate: a shim that [`Self::is_generic`] says, else
    /// `hir_ty_mir::mir::home::compiled_by_user` (a generic, a closure or coroutine, an
    /// `#[inline]` function), never a function linked by a symbol of its own
    /// ([`linked_symbol`]: a symbol has one definition; `compiled_by_user` already says no for
    /// every such attribute but the `eh_personality` lang item's, which no attribute names).
    /// The one rule of which origin v2 compiles an instance for (`HirFrontend::compiled_for`),
    /// which walk its references are listed for, whether it heads its own recursion path, and
    /// which crate's `-C debug-assertions` its MIR's UB checks follow ([`body_options`]).
    fn compiled_by_user(&self, db: &Db) -> bool {
        use hir_ty_mir::mir::dispatch::Body;
        let body = match self.def {
            InstanceDef::Fn(func) => Body::Fn(func),
            InstanceDef::Closure(id) => Body::Closure(id),
            InstanceDef::Coroutine(id) => Body::Coroutine(id),
            InstanceDef::CoroutineByMove(id) => Body::CoroutineByMove(id),
            InstanceDef::DropGlue
            | InstanceDef::CallOnceShim
            | InstanceDef::VtableShim(_)
            | InstanceDef::FnPointerShim
            | InstanceDef::ReifyShim { .. } => return self.is_generic(),
        };
        // The usual answer first, memoized; the symbol only of what it says yes to.
        hir_ty_mir::mir::home::compiled_by_user(db, body, self.args.as_ref())
            && !matches!(self.def, InstanceDef::Fn(func)
                if !self.is_generic() && linked_symbol(db, func).is_some())
    }

    /// The environment the instance is compiled in when a function compiled in `asked` asks
    /// for it: `hir_ty_mir::mir::home::env_of`, the one rule purity's walk and identity use
    /// too. Not generic, it is its own crate's: rustc compiles it there once, whoever calls it.
    /// Generic, rustc instantiates it in the asker, and it is compiled in its home asked from
    /// `asked`, a crate that answers every question its code asks as the asker does.
    fn home_env(&self, db: &Db, asked: &StoredParamEnvAndCrate) -> StoredParamEnvAndCrate {
        let code = match self.def {
            InstanceDef::Fn(func) | InstanceDef::VtableShim(func) => Some(func.krate(db)),
            InstanceDef::Closure(id) => {
                Some(InternedClosureId::from_id(id).loc(db).owner.krate(db))
            }
            InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
                Some(InternedCoroutineId::from_id(id).loc(db).owner.krate(db))
            }
            InstanceDef::DropGlue
            | InstanceDef::CallOnceShim
            | InstanceDef::FnPointerShim
            | InstanceDef::ReifyShim { .. } => None,
        };
        hir_ty_mir::mir::home::env_of(db, asked, code, self.args.as_ref())
    }
}

impl InstanceDef {
    /// Whose MIR the instance runs: `None` for a shim, which has none.
    fn mir_owner<'db>(self) -> Option<MirOwner<'db>> {
        match self {
            InstanceDef::Fn(func) => Some(MirOwner::Body(func.into())),
            InstanceDef::Closure(id) => Some(MirOwner::Closure(InternedClosureId::from_id(id))),
            InstanceDef::Coroutine(id) => {
                Some(MirOwner::Coroutine(InternedCoroutineId::from_id(id)))
            }
            InstanceDef::CoroutineByMove(id) => {
                Some(MirOwner::CoroutineByMove(InternedCoroutineId::from_id(id)))
            }
            InstanceDef::DropGlue
            | InstanceDef::CallOnceShim
            | InstanceDef::VtableShim(_)
            | InstanceDef::FnPointerShim
            | InstanceDef::ReifyShim { .. } => None,
        }
    }
}

/// What a call transfers control to.
#[derive(Clone, Copy)]
enum Callee<'db> {
    Instance(InstanceDef, GenericArgs<'db>),
    /// With how many of the arguments are for named parameters, when the rest are for `...`.
    Pointer(Value, ExternAbi, Option<usize>),
}

/// The code generator of a unit's [`Target`] with the baseline's features: what reads the
/// target's conventions, pointer width and registers, which no set of features changes, and
/// v1's compiles.
pub(crate) fn isa_for(target: &'static Target) -> Result<&'static isa::OwnedTargetIsa> {
    target_of(target, Features::BASELINE, Isa::Usual, false)
}

/// [`isa_for`] with `features`: what a function of that set is compiled with.
pub(crate) fn target_with(
    target: &'static Target,
    features: Features,
) -> Result<&'static isa::OwnedTargetIsa> {
    target_of(target, features, Isa::Usual, false)
}

/// The target `krate` is built for, from its `TargetData`: what a query lowering one of its
/// items compiles for. A wasm32 unit's crate and a native one's are two crates, so their
/// queries never share an answer.
pub(crate) fn target_of_crate(db: &Db, krate: Crate) -> Result<&'static Target> {
    match &krate.workspace_data(db).target {
        Ok(data) => Target::of(data.tuple.as_str()).map_err(CodegenError::Backend),
        // alloc: error path
        Err(error) => {
            Err(CodegenError::Backend(format!("the crate's target did not load: {error}")))
        }
    }
}

/// Which of the process's code generators a function is compiled with.
#[derive(Clone, Copy)]
enum Isa {
    Usual,
    /// For a function of more than [`SINGLE_PASS`] instructions. The usual register
    /// allocator takes more than linear time: ten seconds for one of 150 000 instructions,
    /// which a whole build then waits for.
    Large,
    /// For a function that asks for its frame pointer at its start, as one that takes `...`
    /// does to find the arguments on the stack: Cranelift answers only where every function
    /// keeps a frame pointer, which the others have no use for.
    FramePointer,
}

/// The code generator of a kind for the target features `features`, at Cranelift's
/// `opt_level=speed` for code no edit reaches (`speed`) and at `none` for the rest. Each is
/// made once, the first time it is asked for: a few per process, one per set a function is
/// compiled with (the toolchains' and the `#[target_feature]` sets over them), for each target
/// a unit names: a set's number is the process's, and the baseline's is every target's.
#[expect(
    clippy::disallowed_types,
    reason = "the process's ISAs, each made once for a target, feature set, kind and speed and shared by every unit"
)]
fn target_of(
    target: &'static Target,
    features: Features,
    kind: Isa,
    speed: bool,
) -> Result<&'static isa::OwnedTargetIsa> {
    type Made = std::sync::OnceLock<std::result::Result<isa::OwnedTargetIsa, String>>;
    static TARGETS: [[[[Made; 3]; 2]; target::MOST_FEATURE_SETS]; target::MOST_TARGETS] = [const {
        [const { [const { [const { Made::new() }; 3] }; 2] }; target::MOST_FEATURE_SETS]
    };
        target::MOST_TARGETS];
    let (regalloc, frame_pointers) = match kind {
        Isa::Usual => (regalloc(), false),
        Isa::Large => ("single_pass", false),
        Isa::FramePointer => (regalloc(), true),
    };
    let opt = if speed {
        cranelift_codegen::settings::OptLevel::Speed
    } else {
        cranelift_codegen::settings::OptLevel::None
    };
    TARGETS[target.index()][features.index()][usize::from(speed)][kind as usize]
        .get_or_init(|| target.isa(features, regalloc, frame_pointers, opt))
        .as_ref()
        // clone: the error's message
        .map_err(|e| CodegenError::Backend(e.clone()))
}

/// The register allocator of a function that is not over [`SINGLE_PASS`]: `BLAZE_REGALLOC` names one of
/// Cranelift's, `backtracking` (the default) or `single_pass`. The second allocates in one
/// walk over the code: it took a fifth off Cranelift's time on cold builds of ripgrep and of
/// just, and none off their wall time, and its code was twice the size and up to 1.8 times
/// slower (the demo's Life step). Code in the store is kept whichever made it, so compare the
/// two with a store of their own each.
fn regalloc() -> &'static str {
    static REGALLOC: LazyLock<&'static str> =
        LazyLock::new(|| match std::env::var("BLAZE_REGALLOC").as_deref() {
            Ok("single_pass") => "single_pass",
            _ => "backtracking",
        });
    *REGALLOC
}

/// The most instructions a function has that LLVM optimizes.
const LARGE: usize = 50_000;

/// The most instructions a function has that Cranelift gives its usual register allocator,
/// which spends 1 to 2 µs an instruction on one this large and more on larger ones. On
/// dev-aws-6 nushell's `main`, of 35 780 instructions, took 75 ms of Cranelift with it and
/// 47 ms with the single-pass one, on every edit to it; the largest functions of a cold build
/// of nushell took 80 to 220 ms each. The single-pass allocator's code is larger and slower,
/// and it gives every value live across blocks a stack slot of its own: hir-ty's
/// `<Ty as HirDisplay>::hir_fmt`, of 22 503 instructions and recursive, had a frame of 10.7 KB
/// with it and 2.5 KB without, and hir-ty-mir's `recursive_vars_2` overflowed its 2 MB stack. The
/// largest functions of blaze's own hir-ty-mir tests, up to 128 217 instructions, took at most
/// 85 ms each with the usual one.
const SINGLE_PASS: usize = 50_000;

/// The most cases of a `match` lowered as a chain of comparisons for LLVM to make a `switch`
/// of; a larger one, which a function Cranelift compiles after all would test one case at a
/// time, keeps `Switch`'s tree.
const CHAINED: usize = 256;

/// What tells compiled code from any other: equal for the same code however often an edit
/// went away from it and came back, so that a patch holds what differs from its base alone.
fn print_of(code: &impl std::hash::Hash) -> u128 {
    let mut hasher = rustc_stable_hash::StableSipHasher128::new();
    code.hash(&mut hasher);
    let type_id::Fingerprint(low, high) = hasher.finish();
    u128::from(low) | u128::from(high) << 64
}

/// The print of the function's code. The database runs this again only when compiling gave
/// something else, not each time it compiled.
#[blaze_query::tracked(returns(copy))]
fn root_serial(db: &Db, func: FunctionId) -> u128 {
    compiled_root(db, func).as_ref().map_or(0, print_of)
}

// Nothing in its arguments is ever collected (the query database frees no interned value),
// so they stay interned and are never verified.
#[blaze_query::tracked(returns(copy))]
fn instance_serial(db: &Db, instance: Instance, env: StoredParamEnvAndCrate) -> u128 {
    compiled_instance(db, instance, env).as_ref().map_or(0, print_of)
}

#[blaze_query::tracked(returns(copy))]
fn static_serial(db: &Db, id: StaticId) -> u128 {
    compiled_static(db, id).as_ref().map_or(0, print_of)
}

/// Where the code from an offset on is written: the file, the line and the column.
type Row = (u32, base_db::FileId, u32, u32);

/// The rows of the code of a non-generic function, in the order of [`Code::lines`].
#[blaze_query::tracked(returns(ref))]
fn root_rows(db: &Db, func: FunctionId) -> Vec<Row> {
    compiled_root(db, func).as_ref().map(|it| rows_of(db, it)).unwrap_or_default()
}

/// The rows of the code of an instance, in the order of [`Code::lines`].
#[blaze_query::tracked(returns(ref))]
fn instance_rows(db: &Db, instance: Instance, env: StoredParamEnvAndCrate) -> Vec<Row> {
    compiled_instance(db, instance, env).as_ref().map(|it| rows_of(db, it)).unwrap_or_default()
}

fn rows_of(db: &Db, compiled: &CompiledFunction) -> Vec<Row> {
    // alloc: empty, allocates nothing
    let Some(source) = &compiled.source else { return Vec::new() };
    let rows = compiled.code.lines.iter().filter_map(|row| {
        let at = source.written(row.span, compiled.closure)?;
        let (file, line, column) = row_location(db, at.owner, at.closure, at.span)?;
        Some((row.offset, file, line, column))
    });
    // alloc: the rows the memo of `root_rows`/`instance_rows` keeps, once per compile
    rows.collect()
}

/// The code of a non-generic function.
#[blaze_query::tracked(returns(ref))]
fn compiled_root(db: &Db, func: FunctionId) -> Result<CompiledFunction> {
    let _span = tracing::info_span!(target: "blaze", "compiled_root").entered();
    root_code(db, func, false, &mut tier::for_build()).map(Produced::code)
}

/// The last name of what an instance is of.
fn callee_name(db: &Db, def: InstanceDef) -> &str {
    match def {
        InstanceDef::Fn(func) => FunctionSignature::of(db, func).name.as_str(),
        InstanceDef::Closure(_) => "{closure}",
        InstanceDef::Coroutine(_) | InstanceDef::CoroutineByMove(_) => "{async}",
        InstanceDef::DropGlue => "drop_in_place",
        InstanceDef::CallOnceShim => "call_once",
        InstanceDef::VtableShim(_) => "{vtable}",
        InstanceDef::FnPointerShim => "{closure}::fn_pointer",
        InstanceDef::ReifyShim { .. } => "{reify}",
    }
}

/// What a backtrace of a rustc build calls the instance, as far as that is reproduced:
/// the last name of it otherwise.
fn backtrace_name(db: &Db, instance: &Instance) -> String {
    let args = instance.args.as_ref();
    let ty = || args.as_slice().first().and_then(|arg| arg.ty());
    let of_closure = |owner: InferBodyId<'_>, expr| match owner {
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) => {
            hir_ty_print::rustc_closure_name(db, func, args.as_slice(), expr)
        }
        _ => None,
    };
    let name = match instance.def {
        InstanceDef::Fn(func) => hir_ty_print::rustc_function_name(db, func, args.as_slice()),
        InstanceDef::Closure(id) => {
            let at = InternedClosureId::from_id(id).loc(db);
            of_closure(at.owner, at.expr)
        }
        InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
            let at = InternedCoroutineId::from_id(id).loc(db);
            of_closure(at.owner, at.expr)
        }
        InstanceDef::DropGlue => ty()
            .and_then(|ty| hir_ty_print::rustc_symbol_type(db, ty))
            // alloc: the backtrace name, owned by later.rs, watch.rs and v2's callers
            .map(|ty| format!("core::ptr::drop_glue::<{ty}>")),
        InstanceDef::CallOnceShim
        | InstanceDef::VtableShim(_)
        | InstanceDef::FnPointerShim
        | InstanceDef::ReifyShim { .. } => None,
    };
    // alloc: the backtrace name, owned by later.rs, watch.rs and v2's callers
    name.unwrap_or_else(|| callee_name(db, instance.def).to_owned())
}

/// The code of a non-generic function: Cranelift's, or, for a compile at a level above 1
/// (`tier`), its IR for LLVM. `inlining` is what the compile builds in, handed back with what
/// it built in (see `compile_as`).
fn root_code(
    db: &Db,
    func: FunctionId,
    tier: bool,
    inlining: &mut Option<tier::Inlining>,
) -> Result<Produced> {
    let (body, env) = root_body(db, func)?;
    if dump_mir_of().is_some_and(|want| FunctionSignature::of(db, func).name.as_str() == want) {
        dump_mir(body);
    }
    let signature = FunctionSignature::of(db, func);
    let (abi, variadic) = (signature.abi, signature.is_varargs());
    let body = body_for_abi(abi, Cow::Borrowed(body))?;
    let args = GenericArgs::new_from_slice(&[]).store();
    let checked = crate_checks_overflow(db, func.krate(db)).unwrap_or(false);
    compile_as(db, env, Some(&*body), track_caller(db, func), checked, tier, None, inlining, |fx| {
        fx.abi = abi;
        fx.variadic = variadic;
        fx.args = Some(args);
        fx.codegen()
    })
}

/// The crates of the program of `krate`: what it names
/// ([`hir_def::nameres::named::named_dependencies`]), as rustc loads them, and what that names
/// in turn, but not through a proc macro, which is a library of the compiler's. A dependency
/// nothing names is not linked, nor its constructors, `#[used]` statics or allocator.
///
/// One panic runtime (a `#![panic_runtime]` crate), as rustc's `inject_panic_runtime`,
/// `activate_injected_dep` and `dependency_format::verify_ok` choose it. std and test depend on
/// both `panic_unwind` and `panic_abort` and name neither, and the standard library's crates are
/// taken to name every dependency, so a runtime reached only through them is an offer: the
/// program links the one its strategy names (`panic_unwind` for `panic = "unwind"`,
/// `panic_abort` for `"abort"`). A runtime a crate of the program names itself (`extern crate
/// panic_abort;`, or a runtime of its own) replaces every offer. Either way the runtime must be
/// built with the program's strategy, and two named are an error, each naming the crate that
/// named it.
///
/// Then rustc's `RequiredPanicStrategy` (`dependency_format::verify_ok`): every other crate
/// linked but the compiler's builtins that requires a strategy has the program's. A panic
/// runtime requires its own and a crate built to abort requires `abort`; any other may be linked
/// either way, so a program that aborts links the standard library built to unwind, as the
/// toolchain's sysroot has it, with `panic_abort`. rustc also has a crate built to unwind that
/// makes a call that may unwind from foreign code (`has_ffi_unwind_calls`) require `unwind`;
/// that reads every body of every linked crate, so blaze does not check it. It holds of the
/// standard library (the sysroot's, built to unwind, links into every program rustc builds that
/// aborts), so a program that aborts links a crate built to unwind only from the standard
/// library: any other is refused, naming it, never linked unchecked. Cargo's unit graphs make
/// none (a profile's `panic` is uniform but in what always unwinds: tests, benches, build
/// scripts). rustc's `IncompatiblePanicInDropStrategy` can't fail:
/// blaze refuses `-Zpanic-in-drop`, so every crate has the default. Both toolchain runtimes define
/// `__rust_start_panic` and `__rust_panic_cleanup` under the same symbols: linking both let each
/// symbol resolve to either, so a `catch_unwind` began an unwind with `panic_unwind` and
/// cleaned it up with `panic_abort`, whose cleanup is `unreachable!()` (#1611).
fn linked_crates(db: &Db, krate: Crate) -> std::result::Result<HashSet<Crate, Global>, String> {
    use hir_def::nameres::named::{is_compiler_builtins, is_panic_runtime, named_dependencies};
    let program = krate;
    let mut linked = HashSet::new_in(Global);
    // Panic runtimes only the standard library's crates depend on, and those a crate of the
    // program names. Few: std's and test's two, and what the program names.
    // alloc: once per program linked, setup
    let mut offered: Vec<Crate> = Vec::new();
    // Each runtime a crate of the program names, with the first crate that names it.
    // alloc: once per program linked, setup
    let mut named: Vec<(Crate, Crate)> = Vec::new();
    // alloc: once per program linked, setup
    let mut stack: Vec<Crate> = vec![krate];
    let walk = |stack: &mut Vec<Crate>,
                linked: &mut HashSet<Crate, Global>,
                offered: &mut Vec<Crate>,
                named: &mut Vec<(Crate, Crate)>| {
        while let Some(krate) = stack.pop() {
            if !linked.insert(krate) {
                continue;
            }
            let lang = krate.data(db).origin.is_lang();
            for &dep in named_dependencies(db, krate).iter() {
                if dep.data(db).target_kind.is_proc_macro() {
                    continue;
                }
                if is_panic_runtime(db, dep) {
                    if lang {
                        if !offered.contains(&dep) {
                            offered.push(dep);
                        }
                        continue;
                    }
                    if !named.iter().any(|&(it, _)| it == dep) {
                        named.push((dep, krate));
                    }
                }
                stack.push(dep);
            }
        }
    };
    walk(&mut stack, &mut linked, &mut offered, &mut named);
    if offered.is_empty() && named.is_empty() {
        return Ok(linked);
    }
    let strategy = panic_strategy(db, program)?;
    let name = |it: Crate| it.named(db).to_string(); // alloc: a crate's name in the errors
    // The crate that named the runtime, for the errors: the one to edit.
    let mut named_by = None;
    let runtime = match named.as_slice() {
        [] => {
            let wanted = match strategy {
                PanicStrategy::Unwind => "panic_unwind",
                PanicStrategy::Abort => "panic_abort",
            };
            let Some(&runtime) =
                offered.iter().find(|&&it| it.rust_name(db).is_ok_and(|it| it.as_str() == wanted))
            else {
                let mut names: Vec<String> = offered.iter().map(|&it| name(it)).collect();
                names.sort_unstable();
                // alloc: the error's message
                return Err(format!(
                    "the program of crate `{}` panics by `{strategy}`, and the standard library \
                     depends on no panic runtime `{wanted}` (only {names:?})",
                    program.named(db)
                ));
            };
            runtime
        }
        &[(runtime, by)] => {
            named_by = Some(by);
            runtime
        }
        &[a, b, ..] => {
            let mut names = [(name(a.0), name(a.1)), (name(b.0), name(b.1))];
            names.sort_unstable();
            let [(first, first_by), (second, second_by)] = names;
            // alloc: the error's message
            return Err(format!(
                "the program of crate `{}` names two panic runtimes, `{first}` (named by crate \
                 `{first_by}`) and `{second}` (named by crate `{second_by}`): a program links one",
                name(program)
            ));
        }
    };
    // rustc's `BadPanicStrategy`: the runtime is built with the strategy it implements.
    let built = panic_strategy(db, runtime)?;
    if built != strategy {
        let by = match named_by {
            // alloc: the error's message
            Some(by) => format!(" (named by crate `{}`)", name(by)),
            None => String::new(), // alloc: none, an empty string
        };
        // alloc: the error's message
        return Err(format!(
            "the program of crate `{}` panics by `{strategy}`, and it links the panic runtime \
             `{}`{by}, built to panic by `{built}`: a program's panic runtime is built with its \
             panic strategy",
            name(program),
            name(runtime)
        ));
    }
    // A named runtime is linked by the walk; an offered one is linked now, with what it names.
    stack.push(runtime);
    walk(&mut stack, &mut linked, &mut offered, &mut named);
    // rustc's `RequiredPanicStrategy`, of every crate linked: the doc says what requires what.
    // alloc: the error's names, none for a program that links
    let mut refused: Vec<String> = Vec::new();
    let mut found = None;
    // Crates built to unwind, not the standard library's, in a program that aborts: the
    // FFI-unwind arm blaze does not check (the doc).
    // alloc: the error's names, none for a program that links
    let mut unchecked: Vec<String> = Vec::new();
    for &it in &linked {
        if it == program || it == runtime || is_compiler_builtins(db, it) {
            continue;
        }
        let built = panic_strategy(db, it)?;
        let required = is_panic_runtime(db, it) || built == PanicStrategy::Abort;
        if required && built != strategy {
            refused.push(name(it));
            found = Some(built);
        } else if strategy == PanicStrategy::Abort
            && built == PanicStrategy::Unwind
            && !it.data(db).origin.is_lang()
        {
            unchecked.push(name(it));
        }
    }
    let crates = |names: &mut Vec<String>| {
        names.sort_unstable();
        match names.as_slice() {
            // alloc: the error's message
            [one] => format!("crate `{one}`"),
            // alloc: the error's message
            many => format!("crates `{}`", many.join("`, `")),
        }
    };
    if let Some(found) = found {
        let crates = crates(&mut refused);
        // alloc: the error's message
        return Err(format!(
            "the program of crate `{}` panics by `{strategy}`, and it links {crates}, built to \
             panic by `{found}`: a crate built to panic by `{found}` is linked only into a \
             program that panics by `{found}`",
            name(program)
        ));
    }
    if !unchecked.is_empty() {
        let crates = crates(&mut unchecked);
        // alloc: the error's message
        return Err(format!(
            "the program of crate `{}` panics by `abort`, and it links {crates}, built to panic \
             by `unwind` and not the standard library's: blaze does not check a crate built to \
             unwind for calls that may unwind out of foreign code (rustc's \
             `has_ffi_unwind_calls`), so a program that aborts links one only from the standard \
             library",
            name(program)
        ));
    }
    Ok(linked)
}

/// A crate's `-C panic`, rustc's `PanicStrategy`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PanicStrategy {
    Unwind,
    Abort,
}

impl fmt::Display for PanicStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PanicStrategy::Unwind => "unwind",
            PanicStrategy::Abort => "abort",
        })
    }
}

/// The panic strategy `krate` is built with: its `cfg(panic = "..")`, which its `-C panic`
/// sets.
fn panic_strategy(db: &Db, krate: Crate) -> std::result::Result<PanicStrategy, String> {
    let cfg = krate.cfg_options(db);
    let strategy = |value: intern::Symbol| {
        cfg.check_atom(&cfg::CfgAtom::KeyValue { key: intern::sym::panic, value })
    };
    if strategy(intern::sym::abort) {
        return Ok(PanicStrategy::Abort);
    }
    if strategy(intern::Symbol::intern("unwind")) {
        return Ok(PanicStrategy::Unwind);
    }
    // alloc: the error's message
    Err(format!(
        "crate `{}` has a panic strategy that names no panic runtime: its cfg has neither \
         `panic = \"unwind\"` nor `panic = \"abort\"`",
        krate.named(db)
    ))
}

/// The code of a function, closure, `async` block or shim with its generic arguments filled in.
// Nothing in its arguments is ever collected (the query database frees no interned value),
// so they stay interned and are never verified.
#[blaze_query::tracked(returns(ref), lru = 0)]
fn compiled_instance<'db>(
    db: &'db Db,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Result<CompiledFunction> {
    let _span = tracing::info_span!(target: "blaze", "compiled_instance").entered();
    instance_code(db, instance, env, Detach::No, None, &mut tier::for_build()).map(Produced::code)
}

/// Whether a compile leaves LLVM's part to later ([`Produced::Pending`]): not at all, for a
/// compile at a level above 1, which only a function, closure or `async` block of the source
/// is compiled at, or for a unit of a build ([`units`]), which takes shims too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Detach {
    No,
    Tier,
    Unit,
}

/// The code of an instance: Cranelift's, or, detached, its IR for LLVM.
///
/// `unit` is the v2 unit's settings its body is read under; `None` in v1. `inlining` is what
/// the compile builds in, handed back with what it built in (see `compile_as`).
fn instance_code(
    db: &Db,
    instance: Instance,
    env: StoredParamEnvAndCrate,
    detach: Detach,
    unit: Option<v2::build::Build>,
    inlining: &mut Option<tier::Inlining>,
) -> Result<Produced> {
    let stored_env = env;
    let env = stored_env.as_ref(db);
    let ty = || instance.args.as_ref().type_at(0);
    let (tier, shim) = (detach != Detach::No, detach == Detach::Unit);
    let checked = instance.checked;
    if detach == Detach::Tier
        && !matches!(
            instance.def,
            InstanceDef::Fn(_)
                | InstanceDef::Closure(_)
                | InstanceDef::Coroutine(_)
                | InstanceDef::CoroutineByMove(_)
        )
    {
        unsupported!("a shim stays at level 1");
    }
    let body = match instance.def {
        InstanceDef::DropGlue => {
            return compile_as(db, env, None, false, checked, shim, unit, inlining, |fx| {
                let ty = ty();
                fx.drop_glue(ty)?;
                if dumps_clif() {
                    eprintln!("drop glue for {ty:?}\n{}", fx.builder.func.display());
                }
                Ok(())
            });
        }
        InstanceDef::CallOnceShim => {
            return compile_as(db, env, None, false, checked, shim, unit, inlining, |fx| {
                fx.call_once_shim(ty())
            });
        }
        InstanceDef::VtableShim(func) => {
            let args = instance.args.as_ref();
            return compile_as(db, env, None, false, checked, shim, unit, inlining, |fx| {
                fx.vtable_shim(func, args)
            });
        }
        InstanceDef::FnPointerShim => {
            return compile_as(db, env, None, false, checked, shim, unit, inlining, |fx| {
                fx.fn_pointer_shim(ty())
            });
        }
        InstanceDef::ReifyShim { receiver } => {
            return compile_as(db, env, None, false, checked, shim, unit, inlining, |fx| {
                fx.reify_shim(ty(), receiver)
            });
        }
        InstanceDef::Fn(_)
        | InstanceDef::Closure(_)
        | InstanceDef::Coroutine(_)
        | InstanceDef::CoroutineByMove(_) => instance_body(db, &instance, stored_env, unit)?,
    };
    let body = &*body;
    let dump = dump_mir_of();
    let (tracked, abi, variadic) = match instance.def {
        InstanceDef::Fn(func) => {
            let signature = FunctionSignature::of(db, func);
            if dump == Some(signature.name.as_str()) {
                dump_mir(body);
            }
            (track_caller(db, func), signature.abi, signature.is_varargs())
        }
        _ => {
            if dump == Some("{closure}") {
                dump_mir(body);
            }
            (false, ExternAbi::Rust, false)
        }
    };
    let args = instance.args;
    let closure = match instance.def {
        InstanceDef::Closure(id) => Some(InternedClosureId::from_id(id).loc(db).expr),
        InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
            Some(InternedCoroutineId::from_id(id).loc(db).expr)
        }
        _ => None,
    };
    let compiled = compile_as(db, env, Some(body), tracked, checked, tier, unit, inlining, |fx| {
        fx.abi = abi;
        fx.variadic = variadic;
        fx.args = Some(args);
        fx.codegen()
    })?;
    Ok(match compiled {
        Produced::Code(compiled) => Produced::Code(CompiledFunction { closure, ..compiled }),
        Produced::Pending(pending) => Produced::Pending(Pending { closure, ..pending }),
    })
}

/// How many of the argument registers of a C convention the named parameters of a variadic
/// function take. What is left of them holds the first of the variable arguments. The
/// conventions a function that takes `...` is compiled for are x86-64's System V and
/// AAPCS64 as Linux has it.
#[derive(Clone, Copy)]
struct VariadicRegisters {
    arch: Arch,
    ints: u32,
    floats: u32,
    /// How many bytes of the caller's stack the named parameters take, which the variable
    /// arguments there come after.
    stack: u32,
}

impl VariadicRegisters {
    const FLOATS: u32 = 8;
    /// What `va_arg` expects a saved vector register to take.
    const FLOAT_BYTES: u32 = 16;

    /// How many integer registers hold arguments.
    fn int_count(arch: Arch) -> u32 {
        if arch == Arch::Aarch64 { 8 } else { 6 }
    }

    fn named(sig: &Signature, arch: Arch) -> Result<Self> {
        let mut named = VariadicRegisters { arch, ints: 0, floats: 0, stack: 0 };
        if arch == Arch::Aarch64 {
            return named.aapcs64(sig);
        }
        for param in &sig.params {
            match param.value_type {
                // AAPCS64 has a register of its own for where the result goes.
                _ if param.purpose == ArgumentPurpose::StructReturn && arch == Arch::Aarch64 => {}
                _ if param.purpose != ArgumentPurpose::Normal
                    && param.purpose != ArgumentPurpose::StructReturn =>
                {
                    unsupported!("a variadic function with a named parameter on the stack")
                }
                ty if ty.is_int() && ty.bytes() <= 8 => named.ints += 1,
                types::F32 | types::F64 => named.floats += 1,
                ty => unsupported!("a variadic function with a named {ty}"),
            }
        }
        if named.ints > Self::int_count(arch) || named.floats > Self::FLOATS {
            unsupported!("a variadic function with a named parameter on the stack");
        }
        Ok(named)
    }

    /// Where AAPCS64, as Cranelift has it, puts the named parameters: in the next register
    /// of their kind, or on the stack when those are out, in eight bytes each and a 128-bit
    /// integer in sixteen at a multiple of sixteen.
    fn aapcs64(mut self, sig: &Signature) -> Result<Self> {
        let each = Self::int_count(self.arch);
        for param in &sig.params {
            let on_stack = match param.value_type {
                // The address of what is returned has a register of its own.
                _ if param.purpose == ArgumentPurpose::StructReturn => continue,
                _ if param.purpose != ArgumentPurpose::Normal => {
                    unsupported!("a variadic function with a named aggregate on the stack")
                }
                types::I128 => {
                    let first = self.ints + self.ints % 2;
                    if first + 2 <= each {
                        self.ints = first + 2;
                        continue;
                    }
                    16
                }
                ty if ty.is_int() && self.ints < each => {
                    self.ints += 1;
                    continue;
                }
                types::F32 | types::F64 if self.floats < Self::FLOATS => {
                    self.floats += 1;
                    continue;
                }
                ty if ty.is_int() || ty == types::F32 || ty == types::F64 => 8,
                ty => unsupported!("a variadic function with a named {ty}"),
            };
            self.stack = self.stack.next_multiple_of(on_stack) + on_stack;
        }
        Ok(self)
    }

    /// Adds the registers the named parameters left to `sig`, so that the function is given
    /// all of them whatever it was called with, and on x86-64 the stack past them. Cranelift
    /// has no such parameter for AArch64, where the frame pointer finds the stack.
    fn rest(self, sig: &mut Signature) {
        let ints = Self::int_count(self.arch);
        sig.params.extend((self.ints..ints).map(|_| AbiParam::new(types::I64)));
        sig.params.extend((self.floats..Self::FLOATS).map(|_| AbiParam::new(types::F64)));
        if self.arch == Arch::X86_64 {
            sig.params.push(AbiParam::special(types::I64, ArgumentPurpose::StructArgument(8)));
        }
    }
}

/// How many of the four argument slots of Windows x64 the named parameters of a variadic
/// function take. An argument has the slot of its position whatever its type, the caller
/// leaves room above the return address for the four that come in registers, and the ones
/// after them follow that room on the stack: written there, the registers and the stack are
/// the one run of 8-byte slots a `va_list` walks.
#[derive(Clone, Copy)]
struct HomedRegisters {
    named: u32,
}

impl HomedRegisters {
    const SLOTS: u32 = 4;

    fn named(sig: &Signature) -> Result<Self> {
        for param in &sig.params {
            let plain =
                matches!(param.purpose, ArgumentPurpose::Normal | ArgumentPurpose::StructReturn);
            if !plain || param.value_type.bytes() > 8 {
                unsupported!("a variadic function with a named {}", param.value_type);
            }
        }
        match sig.params.len() as u32 {
            // The first slot is where the exception of a guarded call arrives.
            0 => unsupported!("a variadic function without a named parameter"),
            named => Ok(HomedRegisters { named }),
        }
    }

    /// The registers the named parameters left.
    fn left(self) -> u32 {
        Self::SLOTS.saturating_sub(self.named)
    }

    /// Adds the registers the named parameters left, as the integers a caller passes a
    /// variable argument in, and the stack past the room for the four, to `sig`.
    fn rest(self, sig: &mut Signature) {
        sig.params.extend((0..self.left()).map(|_| AbiParam::new(types::I64)));
        sig.params.push(AbiParam::special(types::I64, ArgumentPurpose::StructArgument(8)));
    }
}

/// How the variable arguments of a function defined here reach it.
#[derive(Clone, Copy)]
enum Variadic {
    SystemV(VariadicRegisters),
    Windows(HomedRegisters),
    /// All of them on the stack, after the `named` bytes of it that the named parameters
    /// take.
    Apple {
        named: u32,
    },
    /// In memory the caller wrote, whose address follows the named parameters.
    Buffer,
}

impl Variadic {
    /// For a function whose named parameters are `sig`. `None` on a target a function that
    /// takes `...` is not compiled for.
    fn of(target: &Target, sig: &Signature) -> Result<Option<Self>> {
        Ok(match target.variable_arguments() {
            Some(VariableArguments::Registers) => {
                Some(Variadic::SystemV(VariadicRegisters::named(sig, target.arch)?))
            }
            Some(VariableArguments::Homed) => Some(Variadic::Windows(HomedRegisters::named(sig)?)),
            Some(VariableArguments::Stack) => {
                Some(Variadic::Apple { named: Self::apple_stack(sig)? })
            }
            Some(VariableArguments::Buffer) => Some(Variadic::Buffer),
            None => None,
        })
    }

    /// `pointer` is the unit target's address, which a buffer of the arguments is passed as.
    fn rest(self, sig: &mut Signature, pointer: Type) {
        match self {
            Variadic::SystemV(named) => named.rest(sig),
            Variadic::Windows(named) => named.rest(sig),
            Variadic::Apple { .. } => (),
            Variadic::Buffer => sig.params.push(AbiParam::new(pointer)),
        }
    }

    /// How many bytes of stack the named parameters of `sig` take under Apple's AArch64
    /// convention: the ones the eight registers of their kind do not hold, each at a
    /// multiple of its own size. The variable arguments start at the next multiple of eight.
    fn apple_stack(sig: &Signature) -> Result<u32> {
        const EACH: u32 = 8;
        let (mut ints, mut floats, mut stack) = (0, 0, 0u32);
        for param in &sig.params {
            let ty = param.value_type;
            let used = match param.purpose {
                // The address of what is returned has a register of its own.
                ArgumentPurpose::StructReturn => continue,
                ArgumentPurpose::Normal if ty.is_int() => &mut ints,
                ArgumentPurpose::Normal if ty == types::F32 || ty == types::F64 => &mut floats,
                _ => unsupported!("a variadic function with a named {ty}"),
            };
            // A 128-bit integer is in two registers or in none.
            let registers = ty.bytes().div_ceil(8);
            if *used + registers <= EACH {
                *used += registers;
            } else {
                // And nothing after it is in a register of that kind.
                *used = EACH;
                stack = stack.next_multiple_of(ty.bytes()) + ty.bytes();
            }
        }
        Ok(stack.next_multiple_of(8))
    }
}

/// The MIR optimization level the environment asks for with `BLAZE_MIR_OPT` in place of
/// rustc's ([`mir_opt_level_of`]): 0 compiles the runtime body; 1 and 2 that body after
/// rustc's MIR passes at its `-Zmir-opt-level` 1 and 2 (`hir_ty_mir::mir::MirOptLevel`).
pub fn mir_opt_level() -> Result<Option<MirOptLevel>> {
    static LEVEL: LazyLock<std::result::Result<Option<MirOptLevel>, String>> =
        LazyLock::new(|| mir_opt_level_in(std::env::var_os(MIR_OPT).as_deref()));
    match &*LEVEL {
        Ok(level) => Ok(*level),
        // clone: the error's message
        Err(why) => Err(CodegenError::Backend(why.clone())),
    }
}

/// The level `BLAZE_MIR_OPT` set to `asked` asks for: none when unset, an error naming the
/// value when it is not one.
fn mir_opt_level_in(
    asked: Option<&std::ffi::OsStr>,
) -> std::result::Result<Option<MirOptLevel>, String> {
    let Some(asked) = asked else { return Ok(None) };
    asked
        .to_str()
        .and_then(|it| it.parse().ok())
        .and_then(MirOptLevel::from_number)
        .map(Some)
        // alloc: the error's message
        .ok_or_else(|| format!("{MIR_OPT}={}: expected 0, 1 or 2", asked.display()))
}

pub const MIR_OPT: &str = "BLAZE_MIR_OPT";

/// `BLAZE_DUMP_MIR`: the name of the function whose MIR is printed as it is compiled
/// (`{closure}` for closures). These switches are read once: `std::env::var` takes the
/// process's environment lock and allocates, and they are asked about once per function.
fn dump_mir_of() -> Option<&'static str> {
    static NAME: LazyLock<Option<String>> = LazyLock::new(|| std::env::var("BLAZE_DUMP_MIR").ok());
    NAME.as_deref()
}

/// `BLAZE_DUMP_CLIF`: print the IR of drop glue as it is built.
fn dumps_clif() -> bool {
    static SET: LazyLock<bool> = LazyLock::new(|| std::env::var_os("BLAZE_DUMP_CLIF").is_some());
    *SET
}

/// `BLAZE_DUMP_TIER`: say why a compile above level 1 is not shown or given a body.
pub(crate) fn dumps_tier() -> bool {
    static SET: LazyLock<bool> = LazyLock::new(|| std::env::var_os("BLAZE_DUMP_TIER").is_some());
    *SET
}

/// The MIR optimization level of the bodies of `krate`: [`mir_opt_level`] where it is asked
/// for, else rustc's ([`rustc_mir_opt_level`]).
fn mir_opt_level_of(db: &Db, krate: Crate) -> Result<MirOptLevel> {
    Ok(mir_opt_level()?.unwrap_or_else(|| {
        rustc_mir_opt_level(krate.data(db).origin.is_lang(), backend::above_o0())
    }))
}

/// The `-Zmir-opt-level` rustc's code of a crate is generated from: 1 at `-Copt-level=0`, 2
/// above it, and 2 for the library's crates, whose MIR rustc ships optimized for release.
fn rustc_mir_opt_level(lang: bool, optimized: bool) -> MirOptLevel {
    if lang || optimized { MirOptLevel::Release } else { MirOptLevel::Debug }
}

/// What the bodies of `krate` are optimized for: [`mir_opt_level_of`] it, rustc's UB checks
/// where its `-C debug-assertions` is on (`sess.ub_checks()` of the crate whose MIR it is,
/// not [`ub_checks_of`], the calling crate's `ub_checks()`), lifetime markers kept, and
/// inlined as rustc's `Inline::is_enabled` says at level 2 ([`rustc_inlines`]).
fn mir_options_of(db: &Db, krate: Crate) -> Result<MirOptions> {
    let level = mir_opt_level_of(db, krate)?;
    Ok(MirOptions {
        level,
        ub_checks: debug_assertions(db, krate),
        lifetime_markers: true,
        inline: rustc_inlines(level, krate.data(db).origin.is_lang(), backend::optimizing()),
        // v1 knows nothing of the program its crates are linked into, so not what allocator
        // it has: no allocation moves to the stack.
        heap_to_stack: false,
        // Nor what trait objects it makes.
        world: None,
    })
}

/// rustc's `Inline::is_enabled` at `-Zmir-opt-level` `level`: never below 2, and at 2 for
/// `-Copt-level=2` and 3 (`optimized`), and for the library's crates, whose MIR rustc ships
/// built at 3. rustc also leaves an incremental build uninlined; blaze's builds are not
/// rustc's incremental ones, and inlining changes code, never behaviour
/// ([`v2::build::Build::options`] says the same of a v2 unit).
fn rustc_inlines(level: MirOptLevel, lang: bool, optimized: bool) -> bool {
    level == MirOptLevel::Release && (lang || optimized)
}

/// What a body of `written` is asked with, `per_user` where it is an instance each crate that
/// uses it compiles ([`Instance::compiled_by_user`]): what the v2 unit's settings say
/// ([`v2::build::Build::options`]) when `build` is a unit's, else what v1 reads of the crate
/// and the environment ([`mir_options_of`]). Never the crate the instance is worked out in
/// (its home): that is a choice of sharing, which no store key names.
fn body_options(
    db: &Db,
    build: Option<v2::build::Build>,
    written: Crate,
    per_user: bool,
) -> Result<MirOptions> {
    match build {
        Some(build) => Ok(build.options(db, written, per_user)),
        None => mir_options_of(db, written),
    }
}

/// The body of an instance that is not a shim, with the instance's types in place of the
/// generic parameters. The database does not keep it: nothing but compiling the instance
/// reads it, and what was compiled is kept.
///
/// `build` is the v2 unit's settings the body is read under (`v2::backend::built`'s `build` in the
/// IR builder, [`v2::build::Build::WALK`] for the walk), `None` in v1.
fn instance_body<'db>(
    db: &'db Db,
    instance: &Instance,
    env: StoredParamEnvAndCrate,
    build: Option<v2::build::Build>,
) -> Result<Cow<'db, MirBody<'db>>> {
    let args = instance.args;
    let tally = backend::Tally::global();
    let filled = |body, args: &StoredGenericArgs, env: &StoredParamEnvAndCrate| {
        tally.step(Step::Monomorphize, || monomorphized(db, body, args, env).map(Cow::Owned))
    };
    let Some(owner) = instance.def.mir_owner() else { unsupported!("the body of a shim") };
    let written = owner.written_in(db).module(db).krate(db);
    let options = body_options(db, build, written, instance.compiled_by_user(db))?;
    let body = tally
        .step(Step::Mir, || db.mir(owner, MirStage::compiled(owner, options)))
        .and_then(|it| filled(it, &args, &env));
    let body = body.map_err(|e| mir_failed(db, owner, &e))?;
    match instance.def {
        InstanceDef::Fn(func) => body_for_abi(FunctionSignature::of(db, func).abi, body),
        _ => Ok(body),
    }
}

/// The error of a body whose MIR did not build or fill: the body by its path (a closure's or
/// coroutine's by the body it is written in), and the lowering error in words, which for a call
/// with no function is the callee as written, where the call is and why it resolved to nothing,
/// in the body it was met in (rule 19).
fn mir_failed(
    db: &Db,
    owner: MirOwner<'_>,
    error: &hir_ty::mir::MirLowerError<'_>,
) -> CodegenError {
    use blaze_items::lowering;
    use std::fmt::Write as _;
    let written = owner.written_in(db);
    // alloc: the error's message
    let mut what = format!("the MIR of `{}`", lowering::owner_path(db, written));
    match owner {
        MirOwner::Body(_) => {}
        MirOwner::Closure(_) => what.push_str(" (a closure in it)"),
        MirOwner::Coroutine(_) | MirOwner::CoroutineByMove(_) => {
            what.push_str(" (a coroutine in it)")
        }
    }
    what.push_str(" did not lower: ");
    if let hir_ty::mir::MirLowerError::MissingFunctionDefinition(met, _) = error
        && *met != written
    {
        _ = write!(what, "in `{}`, ", lowering::owner_path(db, *met));
    }
    lowering::write(db, error, &mut what);
    CodegenError::Frontend(what)
}

/// The body as its native entry point receives its arguments: the final tuple of an
/// `extern "rust-call"` function arrives one field at a time, as rustc passes it.
fn body_for_abi<'db>(
    abi: ExternAbi,
    mut body: Cow<'db, MirBody<'db>>,
) -> Result<Cow<'db, MirBody<'db>>> {
    if abi == ExternAbi::RustCall && !body.to_mut().spread_rust_call_tuple() {
        unsupported!("the last parameter of a rust-call function is not a tuple");
    }
    Ok(body)
}

/// The types a body takes and returns, which is all its callers have to know about it. Asked
/// for on its own so that they do not depend on the rest of the body.
#[derive(Clone, PartialEq, Eq)]
struct BodyTypes {
    params: Vec<StoredTy>,
    returned: StoredTy,
    /// For the body of an `async` block, the `Poll` it hands to the caller of `poll`.
    poll: Option<StoredTy>,
}

#[blaze_query::tracked(returns(ref))]
fn body_types<'db>(
    db: &'db Db,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Result<BodyTypes> {
    // What a body takes and returns is the same under every unit's settings, and a query
    // must not read the thread's unit.
    let body = &*instance_body(db, &instance, env, None)?;
    Ok(BodyTypes {
        // alloc: the query's kept result, once per instance
        params: body.param_locals.iter().map(|&local| body.locals[local].ty).collect(),
        returned: body.locals[returned_local(body)].ty,
        poll: body.coroutine.map(|it| body.locals[it.poll].ty),
    })
}

/// The future of a non-generic `async fn` as `-Zprint-type-sizes` names it, with the offsets
/// of what each variant holds (the captures, and the locals kept there) in increasing order:
/// `{async fn body of f()} @coroutine 0,16;0,8,16`. `harness/layoutdiff.py` compares it with
/// rustc's, which `--layouts` otherwise skips: a future both compilers make and poll has to be
/// laid out alike to the byte.
fn coroutine_layout_line<'db>(db: &'db Db, ty: Ty<'db>, layout: &Layout) -> Option<String> {
    use hir_def::hir::{ClosureKind, CoroutineSource, Expr};
    let TyKind::Coroutine(id, args) = ty.kind() else { return None };
    if !args.as_coroutine().parent_args().is_empty() {
        return None;
    }
    let closure = id.0.loc(db);
    let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) = closure.owner else {
        return None;
    };
    let store =
        hir_def::expr_store::ExpressionStore::of(db, closure.owner.expression_store_owner(db));
    let Expr::Closure {
        closure_kind: ClosureKind::Coroutine { source: CoroutineSource::Fn, .. },
        ..
    } = store.expr(closure.expr)
    else {
        return None;
    };
    let Variants::Multiple { tag_field, variants, .. } = &layout.variants else { return None };
    let upvars: Vec<u64> = (0..layout.fields.count())
        .filter(|&field| field != rustc_index::Idx::index(*tag_field))
        .map(|field| layout.fields.offset(field).bytes())
        // alloc: the `--layouts` comparison's line only
        .collect();
    let variants: Vec<String> = variants
        .iter()
        .map(|variant| {
            // clone: the `--layouts` comparison's line only
            let mut offsets = upvars.clone();
            offsets.extend(variant.field_offsets.iter().map(|it| it.bytes()));
            blaze_ids::radix::sort(&mut offsets);
            // alloc: the `--layouts` comparison's line only
            offsets.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
        })
        // alloc: the `--layouts` comparison's line only
        .collect();
    // alloc: the `--layouts` comparison's line only
    let name = FunctionSignature::of(db, func).name.as_str().to_owned();
    // alloc: the `--layouts` comparison's line only
    Some(format!("{{async fn body of {name}()}} @coroutine {}", variants.join(";")))
}

/// Whether `func` runs only when something has gone wrong, so its code goes after every other
/// function and a block calling it after the rest of its function
/// (docs/v2/plan/design/code-layout.md): it is `#[cold]` (`handle_alloc_error`,
/// `unwrap_failed`, core's panics), the panic machinery by its lang items (the
/// `#[panic_handler]`, `begin_panic`, the personality routine unwinding runs), or
/// `#[track_caller]` and never returns (a panic helper of a crate's own). A profile that saw it
/// run wins.
pub(crate) fn cold_fn(db: &Db, lang_items: &LangItems, func: FunctionId) -> bool {
    let flags = AttrFlags::query(db, func.into());
    if flags.contains(AttrFlags::COLD) {
        return true;
    }
    if [lang_items.PanicImpl, lang_items.BeginPanic, lang_items.EhPersonality].contains(&Some(func))
    {
        return true;
    }
    flags.contains(AttrFlags::TRACK_CALLER) && {
        let signature = FunctionSignature::of(db, func);
        signature.ret_type.is_some_and(|ret| {
            matches!(signature.store.type_ref(ret), hir_def::hir::type_ref::TypeRef::Never)
        })
    }
}

/// The methods of the `Fn` traits and of the `AsyncFn` traits, each from the one that borrows
/// the callee to the one that takes it.
fn fn_trait_methods(lang_items: &LangItems) -> [[Option<FunctionId>; 3]; 2] {
    [
        [lang_items.Fn_call, lang_items.FnMut_call_mut, lang_items.FnOnce_call_once],
        [
            lang_items.AsyncFn_async_call,
            lang_items.AsyncFnMut_async_call_mut,
            lang_items.AsyncFnOnce_async_call_once,
        ],
    ]
}

/// The `Fn`-trait method (`call`, `call_mut` or `call_once`) that `func` is the impl of,
/// when it is one written for a type.
fn implemented_fn_method(db: &Db, lang_items: &LangItems, func: FunctionId) -> Option<FunctionId> {
    let ItemContainerId::ImplId(impl_) = func.lookup(db).container else { return None };
    let trait_ref = db.impl_trait(impl_)?;
    let name = &FunctionSignature::of(db, func).name;
    let method = trait_ref.skip_binder().def_id.0.trait_items(db).method_by_name(name)?;
    let [fn_methods, _] = fn_trait_methods(lang_items);
    fn_methods.contains(&Some(method)).then_some(method)
}

/// The trait's declaration of `func`, a method of a trait impl; `func` itself otherwise.
fn declared_method(db: &Db, func: FunctionId) -> FunctionId {
    let ItemContainerId::ImplId(impl_) = func.lookup(db).container else { return func };
    let Some(trait_ref) = db.impl_trait(impl_) else { return func };
    let name = &FunctionSignature::of(db, func).name;
    trait_ref.skip_binder().def_id.0.trait_items(db).method_by_name(name).unwrap_or(func)
}

/// The instance that is the `poll` of the future `ty`, when that is one of an `async` block.
fn coroutine_instance<'db>(db: &'db Db, ty: Ty<'db>) -> Option<(InstanceDef, GenericArgs<'db>)> {
    let (body, parent_args) = dispatch::coroutine_poll(db, ty)?;
    Some((instance_def(body), parent_args))
}

/// The instance of a body the dispatch names.
fn instance_def(body: dispatch::Body) -> InstanceDef {
    match body {
        dispatch::Body::Fn(func) => InstanceDef::Fn(func),
        dispatch::Body::Closure(id) => InstanceDef::Closure(id),
        dispatch::Body::Coroutine(id) => InstanceDef::Coroutine(id),
        dispatch::Body::CoroutineByMove(id) => InstanceDef::CoroutineByMove(id),
    }
}

/// The instances the walk for [`TheirStatics`] went through. What it found stays as it is
/// until the code of one of them, or of a static it found, changes.
type Walked = Vec<(Instance, StoredParamEnvAndCrate)>;

/// The value of a static with an initializer: any but one of an `extern` block, whose value is
/// another object's (readers ask `v2::statics::source`, which tells the two apart).
#[blaze_query::tracked(returns(ref))]
fn compiled_static(db: &Db, id: StaticId) -> Result<CompiledStatic> {
    let checked = crate_checks_overflow(db, id.krate(db)).unwrap_or(false);
    let target = target_of_crate(db, id.krate(db))?;
    // A query keyed by the static alone: its value is no unit's.
    let mut cg = Recorder::new(db, &**isa_for(target)?, target, checked, None);
    let env =
        ParamEnvAndCrate { param_env: hir_ty::next_solver::ParamEnv::empty(), krate: id.krate(db) };
    let mut value = None;
    build(&mut cg, env, None, false, |fx| {
        value = Some(fx.static_value(id)?);
        Ok(())
    })?;
    let (blob, writable) = value.expect("`build` ran the closure");
    Ok(CompiledStatic { requests: cg.finish().0, blob, writable })
}

/// The error the build stops with when the evaluation of `of`, or of a constant it reads, is
/// refused, naming the constant: it ran past its budget (`const-eval` under
/// `[workspace.metadata.blaze]`), with what it ran out of, where, and the setting that raises
/// it; or its value points to memory allocated while evaluating it; or it calls a function whose
/// body blaze could not lower, which is named with where it was called. `None` for any other
/// error.
fn refused_const_eval(
    db: &Db,
    of: GeneralConstId<'_>,
    e: &ConstEvalError<'_>,
) -> Option<CodegenError> {
    let name = |of: GeneralConstId<'_>| match of {
        GeneralConstId::ConstId(id) => {
            // alloc: the error's message
            ConstSignature::of(db, id).name.as_ref().map(|it| format!("`{}`", it.as_str()))
        }
        GeneralConstId::StaticId(id) => {
            // alloc: the error's message
            Some(format!("`{}`", StaticSignature::of(db, id).name.as_str()))
        }
        GeneralConstId::AnonConstId(_) => None,
    };
    // alloc: the error's message
    let mut what = name(of).unwrap_or_else(|| "a constant expression".to_owned());
    let ConstEvalError::MirEvalError(e) = e else { return None };
    let mut e: &MirEvalError<'_> = e;
    // Where the innermost call was made, from the stack the evaluation unwound through.
    let mut called = None;
    loop {
        match e {
            MirEvalError::InFunction(inner, stack) => {
                called = stack.first().map(|&(_, span, owner)| (owner, span)).or(called);
                e = &**inner;
            }
            MirEvalError::ConstEvalError(inner, error) => {
                let ConstEvalError::MirEvalError(inner_error) = &**error else { return None };
                // alloc: the error's message
                what = format!("`{inner}`");
                e = inner_error;
            }
            MirEvalError::OverBudget { spent, at } => {
                let at = at
                    .and_then(|(owner, span)| fncache::location(db, owner, span))
                    // alloc: the error's message
                    .map(|it| format!(" at {it}"))
                    .unwrap_or_default();
                // alloc: the error's message
                return Some(CodegenError::Refused(format!(
                    "const eval of {what} {spent}{at} (raise {} under `[workspace.metadata.blaze]`)",
                    spent.setting()
                )));
            }
            MirEvalError::LongRunning { at } => {
                let at = at
                    .and_then(|(owner, span)| fncache::location(db, owner, span))
                    // alloc: the error's message
                    .map(|it| format!(" at {it}"))
                    .unwrap_or_default();
                // alloc: the error's message
                return Some(CodegenError::Refused(format!(
                    "the evaluation of {what}{at}: constant evaluation is taking a long time \
                     (`long_running_const_eval` is denied there; `#[allow]` it to go on)"
                )));
            }
            MirEvalError::HeapInValue { .. } => {
                // alloc: the error's message
                return Some(CodegenError::Refused(format!("the value of {what}: {e:?}")));
            }
            // `#![feature(blaze_comptime)]`'s refusals: a call through a function pointer or
            // vtable that reached a function that is not const, or what a `const static` block
            // cannot do at compile time; named, with where it was.
            MirEvalError::Comptime(error) => {
                let at = error
                    .at
                    .and_then(|(owner, span)| fncache::location(db, owner, span))
                    // alloc: the error's message
                    .map(|it| format!(" at {it}"))
                    .unwrap_or_default();
                let text = match error.at {
                    Some((owner, _)) => {
                        let krate = owner.module(db).krate(db);
                        let target = hir_ty::display::DisplayTarget::from_crate(db, krate);
                        hir_ty_mir::mir_pretty::comptime_error_text(db, error, target)
                    }
                    // alloc: the error's message
                    None => format!("{error:?}"),
                };
                // alloc: the error's message
                return Some(CodegenError::Refused(format!("const eval of {what}{at}: {text}")));
            }
            MirEvalError::MirLowerError(func, error) => {
                // alloc: the error's message
                let name = hir_ty_print::rustc_function_name(db, *func, &[])
                    .unwrap_or_else(|| FunctionSignature::of(db, *func).name.as_str().to_owned());
                let at = called
                    .and_then(|(owner, span)| fncache::location(db, owner, span))
                    // alloc: the error's message
                    .map(|it| format!(", called at {it}"))
                    .unwrap_or_default();
                // alloc: the error's message
                return Some(CodegenError::Refused(format!(
                    "const eval of {what}: the body of `{name}`{at} is not lowered: {error:?}"
                )));
            }
            _ => return None,
        }
    }
}

/// File, line and column of a place in a body for a debugger, as rustc gives code its lines.
/// They are those of [`source_location`] but for what a macro writes: that is of where the
/// macro is called only if the macro is another crate's or the compiler's, and of where the
/// macro has it otherwise, as it is of what a macro writes of the code of `closure`, or of
/// the body when there is none, where that is itself what the macro writes.
fn row_location_in_file(
    db: &Db,
    owner: DefWithBodyId,
    closure: Option<ExprId>,
    span: MirSpan,
) -> Option<(base_db::FileId, u32, u32)> {
    let (file_id, range) = source_range(db, owner, span)?;
    if file_id.macro_file().is_none() {
        return source_location_in_file(db, owner, span);
    }
    let source_map = Body::source_map(db, owner);
    let code = closure.unwrap_or_else(|| Body::of(db, owner).root_expr());
    let within = match source_map.expr_syntax(code) {
        Ok(body) => {
            let start = TextRange::empty(body.value.syntax_node_ptr().text_range().start());
            body.file_id.span_map(db).span_for_range(start).ctx
        }
        Err(_) => return source_location_in_file(db, owner, span),
    };
    written_place(db, file_id, range, Some(within))
}

/// File, line and column of where `range` of `file_id` is for a debugger: where the macro
/// has it while the macro is the crate's own, and where the first macro that is not is
/// called from, up to the expansion `within` or to the source.
fn written_place(
    db: &Db,
    file_id: HirFileId,
    range: TextRange,
    within: Option<span::SyntaxContext>,
) -> Option<(base_db::FileId, u32, u32)> {
    let mut at = file_id.span_map(db).span_for_range(TextRange::empty(range.start()));
    let mut kept = at;
    while !at.ctx.is_root() && Some(at.ctx) != within {
        let Some(call) = at.ctx.outer_expn(db) else { break };
        let call = hir_expand::MacroCallId::from(call).loc(db);
        let written = call.to_node_view(db);
        let range = TextRange::empty(written_range(written.value).start());
        at = written.file_id.span_map(db).span_for_range(range);
        let own = call.def.krate == call.krate && call.def.kind.is_declarative();
        if !own {
            kept = at;
        }
    }
    line_and_column(db, hir_expand::resolve_span(db, kept))
}

/// File, line and column of where a function is declared, from its visibility or `fn` on:
/// what a pointer to a `#[track_caller]` function reports as its caller, as rustc's shim does.
#[blaze_query::tracked]
fn declaration_location(db: &Db, func: FunctionId) -> Option<(base_db::FileId, u32, u32)> {
    let source = written_node(db, func.lookup(db));
    location_of(db, source.file_id, written_range(source.value))
}

/// The node `loc` is written as, viewed in the tree the database keeps for its file: what
/// `source(db)` is, with no handle made.
fn written_node<'db>(db: &'db Db, loc: &impl HasSource) -> InFile<syntax::view::Node<'db>> {
    let InFile { file_id, value } = loc.ast_ptr(db);
    InFile::new(file_id, syntax::view::to_node(value.syntax_node_ptr(), file_id.root(db)))
}

fn is_thread_local(db: &Db, id: StaticId) -> bool {
    AttrFlags::query(db, id.into()).contains(AttrFlags::THREAD_LOCAL)
}

/// Whether a value of `ty` is an integer in a register: a `bool`, a `char` or an integer.
fn is_integer(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Uint(_))
}

/// The symbol a function is exported under when an attribute names it: the runtime or the
/// program loading the library looks it up by that name, so it is compiled whether or not
/// anything calls it.
pub fn exported_symbol(db: &Db, func: FunctionId) -> Option<String> {
    if let Some(name) = AttrFlags::export_name(db, func.into()) {
        // alloc: the symbol written into the object; callers outside this range take a `String`
        return Some(name.as_str().to_owned());
    }
    AttrFlags::query(db, func.into())
        .contains(AttrFlags::NO_MANGLE)
        // alloc: the symbol written into the object; callers outside this range take a `String`
        .then(|| FunctionSignature::of(db, func).name.as_str().to_owned())
}

/// Whether a function is `#[rustc_comptime]`: one the const evaluator alone runs, such as
/// `TypeId::size` over the reflection intrinsics, of which rustc compiles no code.
pub fn compile_time_only(db: &Db, func: FunctionId) -> bool {
    AttrFlags::has_word(db, func.into(), "rustc_comptime")
}

/// The refusal of `method` of the impl a builtin derive makes for `self_ty`, as `lookup_impl_method`
/// finds it in a database that records builtin derives instead of expanding them: such an impl
/// has no body to compile, where rustc compiles what the derive expands to.
pub(crate) fn unexpanded_derive(
    db: &Db,
    (impl_, method): (
        hir_def::BuiltinDeriveImplId,
        hir_def::builtin_derive::BuiltinDeriveImplMethod,
    ),
    self_ty: impl std::fmt::Debug,
) -> String {
    let derive = impl_.loc(db).trait_.name();
    let (derive, method) = (derive.as_str(), method.name());
    // A type's `Debug` reads its interned parts from the attached database.
    blaze_query::db::attach(db, || {
        // alloc: the error's message
        format!(
            "`{derive}::{}` of `{self_ty:?}`, which `#[derive({derive})]` implements: the database \
             records builtin derives unexpanded, so it has no body to compile (a build's database \
             expands them, `hir_def::nameres::set_expand_builtin_derives`)",
            method.as_str()
        )
    })
}

/// The method of the impl a call of `func` with `args` runs, or `func` itself where its impl
/// is chosen later or is the compiler's ([`NoImplMethod::is_expected`]). Any other reason no
/// impl's method is found stops the call ([`no_impl_method`]).
///
/// [`NoImplMethod::is_expected`]: hir_ty::method_resolution::NoImplMethod::is_expected
pub(crate) fn impl_method<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    func: FunctionId,
    args: GenericArgs<'db>,
) -> std::result::Result<(dispatch::ImplMethod, GenericArgs<'db>), String> {
    dispatch::impl_method(db, env, func, args).map_err(|why| no_impl_method(db, func, args, why))
}

/// `trait P method m for <args>: why`, of a call of the trait method `func` with `args` that
/// no impl's method was found for.
pub(crate) fn no_impl_method(
    db: &Db,
    func: FunctionId,
    args: GenericArgs<'_>,
    why: hir_ty::method_resolution::NoImplMethod,
) -> String {
    let method = FunctionSignature::of(db, func).name.as_str();
    let trait_ = match func.lookup(db).container {
        ItemContainerId::TraitId(it) => Some(hir_def::signatures::TraitSignature::of(db, it)),
        _ => None,
    };
    let (why, args) = (why.describe(), args.as_slice());
    // A type's `Debug` reads its interned parts from the attached database.
    blaze_query::db::attach(db, || match trait_ {
        // alloc: the error's message
        Some(trait_) => {
            format!("trait {} method {method} for {args:?}: {why}", trait_.name.as_str())
        }
        // alloc: the error's message
        None => format!("method {method} for {args:?}: {why}"),
    })
}

/// Whether `name` is of core's `check_language_ub` or `check_library_ub`, each
/// `ub_checks() && ..` and the rest true at run time.
fn is_ub_check(name: &intern::Symbol) -> bool {
    *name == intern::sym::check_language_ub || *name == intern::sym::check_library_ub
}

/// The symbol a static of the program, not of an `extern` block, is exported under when an
/// attribute names it: code of another language finds it by that name, whatever its
/// visibility and whether or not Rust code uses it.
pub fn exported_static(db: &Db, id: StaticId) -> Option<intern::Symbol> {
    let signature = StaticSignature::of(db, id);
    if signature.flags.contains(StaticFlags::EXTERN) {
        return None;
    }
    AttrFlags::export_name(db, id.into()).or_else(|| {
        let unmangled = AttrFlags::query(db, id.into()).contains(AttrFlags::NO_MANGLE);
        // clone: an interned `Symbol`, returned owned to callers outside this range
        unmangled.then(|| signature.name.symbol().clone())
    })
}

/// The symbol an item of an `extern` block links to, on its crate's target. A target that
/// does not load is an error: the symbol depends on it.
pub fn link_name(db: &Db, func: FunctionId) -> Result<Option<String>> {
    let ItemContainerId::ExternBlockId(_) = func.lookup(db).container else {
        return Ok(None);
    };
    Ok(link_name_for(db, target_of_crate(db, func.krate(db))?, func))
}

/// [`link_name`] on `target`, the target of the code that names the item.
///
/// On wasm, an item of a block that names its module (`#[link(wasm_import_module = "m")]`) is
/// that module's import and never a definition the link finds by name, so its symbol is
/// `m.field` ([`wasm_import`] has the field): it never takes the place of a definition of the
/// same name, as `wasi_rt`'s `unlink` beside the seed's `blaze_seed.unlink` import. rustc
/// mangles such an import's symbol for the same reason. A block that names no module imports
/// from `env` by the plain name, which a definition of that name satisfies.
pub(crate) fn link_name_for(db: &Db, target: &Target, func: FunctionId) -> Option<String> {
    let ItemContainerId::ExternBlockId(block) = func.lookup(db).container else {
        return None;
    };
    let written = AttrFlags::link_name(db, func.into());
    let name = match &written {
        Some(it) => it.as_str(),
        None => FunctionSignature::of(db, func).name.as_str(),
    };
    if target.is_wasm()
        && let Some(module) = AttrFlags::wasm_import_module(db, block)
    {
        // alloc: the symbol written into the object; callers outside this range take a `String`
        return Some(format!("{}.{name}", module.as_str()));
    }
    // alloc: as above
    Some(as_declared(name, global_prefix(target)).to_owned())
}

/// The module and field a wasm module imports `func`, an item of an `extern` block, as: its
/// block's `#[link(wasm_import_module)]`, or `None` for `env`, and its link name.
pub(crate) fn wasm_import(db: &Db, func: FunctionId) -> Option<(Option<String>, String)> {
    let ItemContainerId::ExternBlockId(block) = func.lookup(db).container else {
        return None;
    };
    let written = AttrFlags::link_name(db, func.into());
    let field = match &written {
        Some(it) => it.as_str(),
        None => FunctionSignature::of(db, func).name.as_str(),
    };
    // alloc: the import's module and field, which it keeps
    Some((
        AttrFlags::wasm_import_module(db, block).map(|it| it.as_str().to_owned()),
        field.to_owned(),
    ))
}

/// The prefix the object writer puts in front of every name declared to it for `target`:
/// `_` on Mach-O, nothing elsewhere. 32-bit Windows prefixes too, and blaze emits no code for
/// it. The target's own object format decides, never its ISA's triple: a wasm32 target
/// borrows a native ISA's settings, whose triple on a Mac is Mach-O.
fn global_prefix(target: &Target) -> Option<char> {
    (target.object_format == ObjectFormat::MachO).then_some('_')
}

/// The name an `extern` item written with `link_name` is declared under. One that starts with
/// `\u{1}` is the symbol as the object has it, which nothing is put in front of, as rustc
/// takes it: bindgen writes `"\u{1}_aws_lc_0_44_0_SHA384"` for macOS. The object writer puts
/// the target's `prefix` in front of what is declared, so it comes off here; declared with it,
/// the call named `_\u{1}_aws_lc_0_44_0_SHA384`, and the link found nothing of that name. One
/// that does not start with the prefix is kept whole, and the link says it can't find it.
fn as_declared(name: &str, prefix: Option<char>) -> &str {
    let Some(written) = name.strip_prefix('\u{1}') else { return name };
    match prefix {
        None => written,
        Some(prefix) => written.strip_prefix(prefix).unwrap_or(name),
    }
}

/// The symbol a function that isn't exported takes in a crate rustc built nothing of, for the
/// declarations in other crates of the standard library that name it as written: rustc gives
/// it a name of its own. core declares the `#[panic_handler]` as `panic_impl`, and the
/// personality is the one the unwinding tables name.
pub fn linked_symbol(db: &Db, func: FunctionId) -> Option<String> {
    if let ItemContainerId::ExternBlockId(_) = func.lookup(db).container {
        return None;
    }
    let flags = AttrFlags::query(db, func.into());
    if flags.contains(AttrFlags::LINKED_BY_NAME) {
        if AttrFlags::has_word(db, func.into(), "panic_handler") {
            // alloc: the symbol written into the object; callers outside this range take a `String`
            return Some("panic_impl".to_owned());
        }
        // alloc: the symbol written into the object; callers outside this range take a `String`
        return Some(FunctionSignature::of(db, func).name.as_str().to_owned());
    }
    let lang = flags.lang_item_with_attrs(db, func.into())?;
    // alloc: the symbol written into the object; callers outside this range take a `String`
    (lang == intern::sym::eh_personality).then(|| unwind::PERSONALITY.to_owned())
}

/// The section of an object, and of what it is linked into, that holds the messages of its
/// stand-ins and nothing else: what a link keeps of it is what the program can reach. On ELF
/// each message is a section of its own, for the link to take out those nothing reaches;
/// Apple's linker takes them out symbol by symbol from the one section, as it has room for few.
pub fn stand_in_section(format: object::BinaryFormat) -> (&'static str, &'static str) {
    match format {
        object::BinaryFormat::MachO => ("__TEXT", "__blaze_stand"),
        // An image's section name has room for 8 bytes.
        object::BinaryFormat::Coff | object::BinaryFormat::Pe => ("", ".rjstand"),
        _ => ("", ".blaze.stand_in"),
    }
}

/// Whether a crate declares the `#[global_allocator]`: whether it has the function the
/// attribute's expansion exports for `__rust_alloc` (the attribute itself is gone from the
/// static once expanded). The expansion names the function as it exports it, so each module
/// is asked for the name rather than for the attributes of all its functions.
#[blaze_query::tracked(returns(copy))]
fn declares_global_allocator(db: &Db, krate: Crate) -> bool {
    global_allocator_export(db, krate, "__blaze_global_alloc").is_some()
}

/// The entries of the global allocator that `alloc` calls by name, with the function the
/// `#[global_allocator]`'s expansion exports for each: what rustc's shim calls, and what ours
/// goes to instead. The number of pointer-sized arguments, and whether one is returned.
const ALLOCATOR_ENTRIES: [(&str, &str, usize, bool); 4] = [
    ("__rust_alloc", "__blaze_global_alloc", 2, true),
    ("__rust_dealloc", "__blaze_global_dealloc", 3, false),
    ("__rust_realloc", "__blaze_global_realloc", 4, true),
    ("__rust_alloc_zeroed", "__blaze_global_alloc_zeroed", 2, true),
];

/// The function of `krate` a builtin attribute's expansion exports as `exported`: the
/// `#[global_allocator]`'s functions, the `#[alloc_error_handler]`'s `__rg_oom`.
///
/// The expansion writes the function at the attribute macro's def site, so its binding is
/// hygienic: the module binds it under a [keyed](hir_expand::name::Name::keyed) name that no
/// path can write, as rustc's does. Each module's values are matched by the name's text,
/// whatever its key, and the function must still export `exported`.
fn global_allocator_export(db: &Db, krate: Crate, exported: &str) -> Option<FunctionId> {
    let def_map = hir_def::nameres::crate_def_map(db, krate);
    def_map.modules(db).find_map(|(module_id, module)| {
        module.scope(db).values().find_map(|(name, item)| {
            let hir_def::ModuleDefId::FunctionId(id) = item.def else { return None };
            if name.as_str() != exported || item.import.is_some() {
                return None;
            }
            let exports =
                AttrFlags::export_name(db, id.into()).is_some_and(|it| it.as_str() == exported);
            (id.lookup(db).container == ItemContainerId::ModuleId(module_id) && exports)
                .then_some(id)
        })
    })
}

/// The symbol a static of an `extern` block links to, on its crate's target.
fn static_link_name(db: &Db, id: StaticId) -> Result<Option<String>> {
    if !StaticSignature::of(db, id).flags.contains(StaticFlags::EXTERN) {
        return Ok(None);
    }
    Ok(static_link_name_for(db, target_of_crate(db, id.krate(db))?, id))
}

/// [`static_link_name`] on `target`, the target of the code that names the static.
fn static_link_name_for(db: &Db, target: &Target, id: StaticId) -> Option<String> {
    let signature = StaticSignature::of(db, id);
    if !signature.flags.contains(StaticFlags::EXTERN) {
        return None;
    }
    let written = AttrFlags::link_name(db, id.into());
    let name = written.as_ref().map_or_else(|| signature.name.as_str(), |it| it.as_str());
    // alloc: the symbol written into the object; callers outside this range take a `String`
    Some(as_declared(name, global_prefix(target)).to_owned())
}

/// Builds the IR of one function and runs Cranelift on it.
fn compile<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    body: Option<&'db MirBody<'db>>,
    tracked: bool,
    checked: bool,
    fill: impl for<'cx> FnOnce(&mut FunctionCx<'cx, 'db>) -> Result<()>,
) -> Result<CompiledFunction> {
    compile_as(db, env, body, tracked, checked, false, None, &mut None, fill).map(Produced::code)
}

/// Builds the IR of one function and compiles it with Cranelift, or, for a compile at a level
/// above 1 (`tier`), leaves it to LLVM to compile once the database is let go of. `unit` is
/// the v2 unit's settings it is built under, as its caller was asked for them; `None` in v1.
/// `inlining` is what the compile may build in ([`tier::Inlining`]): the compile's `Recorder`
/// holds it while the IR is built, and it is handed back with what was built in, which a
/// compile at a level above 1 reads. The caller owns it, never the thread: the build waits
/// in queries, and a fiber resumes on any thread (rule 27).
#[allow(clippy::too_many_arguments)]
fn compile_as<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    body: Option<&'db MirBody<'db>>,
    tracked: bool,
    checked: bool,
    tier: bool,
    unit: Option<v2::build::Build>,
    inlining: &mut Option<tier::Inlining>,
    fill: impl for<'cx> FnOnce(&mut FunctionCx<'cx, 'db>) -> Result<()>,
) -> Result<Produced> {
    let target = target_of_crate(db, env.krate)?;
    let isa = &**isa_for(target)?;
    let mut cg = Recorder::new(db, isa, target, checked, unit);
    cg.inlining = inlining.take();
    let tally = backend::Tally::global();
    let built = tally.step(Step::Translate, || build(&mut cg, env, body, tracked, fill));
    // What the builder made is checked before the passes change it, and what they made of it
    // after: a bad instruction of either is this function's error, never code made from it.
    let built = built.and_then(|mut ctx| {
        verified(&ctx.func, isa, "built")?;
        tally.step(Step::Translate, || settle(&mut ctx, &cg))?;
        verified(&ctx.func, isa, "settled")?;
        Result::Ok(ctx)
    });
    // Back to the caller before anything can return: nothing after the build reads it.
    *inlining = cg.inlining.take();
    let mut ctx = built?;
    let code = match cg.naked.take() {
        // The function is its assembly: what was built of it is only its signature.
        Some((text, syms)) => {
            // alloc: once per naked function, which is its assembly
            let assembled = asm::assemble(&isa.triple().to_string(), &text)
                .map_err(CodegenError::Unsupported)?;
            let mut relocs = Relocs::with_capacity(assembled.relocs.len());
            for named in &assembled.relocs {
                let Some(name) = syms.get(named.operand as usize).cloned().flatten() else {
                    unsupported!("inline assembly that names a symbol");
                };
                let reloc = ModuleReloc {
                    offset: named.offset,
                    kind: named.kind,
                    name,
                    addend: named.addend,
                };
                relocs.push_module(&reloc, named.elf_type)?;
            }
            Code {
                // clone: once per naked function; `ctx` goes back to the spares whole
                signature: ctx.func.signature.clone(),
                // clone: once per naked function; the assembler keeps its cached copy
                bytes: assembled.code.clone(),
                align: 16,
                relocs,
                unwind: None,
                // alloc: empty, allocates nothing
                lines: Vec::new(),
                slots: SlotPlaces::default(),
            }
        }
        None if tier => {
            // alloc: the pending compile keeps them, once per function
            let named = cg.named_slots().collect();
            let (source, variables) = described(&mut cg, env, body)?;
            let built_in = built_in_of(&mut cg, body);
            let (wants, layouts) = cg.detach();
            let func = std::mem::replace(&mut ctx.func, ir::Function::new());
            spare::keep_context(ctx);
            return Ok(Produced::Pending(Pending {
                func,
                target,
                wants,
                named,
                source,
                variables,
                layouts,
                closure: None,
                moves_to_frame: body.is_some_and(|it| it.inlined.moved_to_frame != 0),
                built_in,
            }));
        }
        None => Backend::emit(&mut cg, &mut ctx)?,
    };
    spare::keep_context(ctx);
    let (source, variables) = described(&mut cg, env, body)?;
    let (requests, layouts) = cg.finish();
    Ok(Produced::Code(CompiledFunction {
        requests,
        code,
        source,
        closure: None,
        variables,
        layouts,
    }))
}

/// Cranelift's verifier over the IR blaze made of a function, `stage` saying which: what the
/// builder `built`, or what [`settle`] `settled` it to. Its errors are the function's
/// [`CodegenError::InvalidIr`], which its caller names the function in: no backend (the
/// stencils, LLVM, the wasm lowering) is handed IR whose types or flow are wrong, which none of
/// them checks and each would make code of. The flags alone, not the ISA: a wasm function's
/// IR is made under a native ISA's settings with the target's own addresses
/// ([`Target::frontend_config`]), which the ISA's checks of pointer types would refuse.
/// Every build runs it, release too: on a cold debug build of regex, serde and serde_json with
/// std (2026-11-03), its two walks of each function were 0.76 to 0.79 s of 144 to 148 s of CPU.
// dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private
fn verified(func: &ir::Function, isa: &dyn isa::TargetIsa, stage: &'static str) -> Result<()> {
    let errors = backend::Tally::global()
        .step(Step::Verify, || cranelift_codegen::verify_function(func, isa.flags()));
    let Err(errors) = errors else { return Ok(()) };
    // alloc: the error's message
    let refused: Vec<&cranelift_codegen::verifier::VerifierError> =
        errors.0.iter().filter(|it| !only_cranelifts_backend(func, it)).collect();
    if refused.is_empty() {
        return Ok(());
    }
    /// The most errors a message lists: the first is what went wrong, the rest its echoes.
    const SHOWN: usize = 8;
    // alloc: the error's message
    let mut shown: Vec<String> = refused.iter().take(SHOWN).map(|it| it.to_string()).collect();
    if refused.len() > SHOWN {
        // alloc: the error's message
        shown.push(format!("and {} more", refused.len() - SHOWN));
    }
    Err(CodegenError::InvalidIr { stage, errors: shown.join("; ") })
}

/// Whether the verifier's `error` is a limit of Cranelift's own code generator rather than of
/// the IR: a `return_call` whose callee's calling convention is one Cranelift's machine
/// backends can't tail call from. None of blaze's backends is Cranelift's (rule 10): the
/// stencils lower a tail call under the platform's convention (blaze-stencil's
/// `return_call_signature`) and LLVM makes it a `musttail` call; which targets take a tail call
/// at all is [`refuse_tail_calls`]'s to say. The verifier's other rules for a tail call (the
/// callee's convention and results match the caller's) still hold: only the error of that one
/// rule, on such a call, is dropped.
fn only_cranelifts_backend(
    func: &ir::Function,
    error: &cranelift_codegen::verifier::VerifierError,
) -> bool {
    let ir::entities::AnyEntity::Inst(inst) = error.location else { return false };
    let sig = match func.dfg.insts[inst] {
        ir::InstructionData::Call { opcode: ir::Opcode::ReturnCall, func_ref, .. } => {
            func.dfg.ext_funcs[func_ref].signature
        }
        ir::InstructionData::CallIndirect {
            opcode: ir::Opcode::ReturnCallIndirect,
            sig_ref,
            ..
        } => sig_ref,
        _ => return false,
    };
    let convention = func.dfg.signatures[sig].call_conv;
    // alloc: once per error, only when the verifier refused something
    !convention.supports_tail_calls()
        && error.message == format!("calling convention `{convention}` does not support tail calls")
}

/// What the IR `build` gives goes through before a backend compiles it: the bodies `cg` built
/// in put where they are called, the locals a register holds taken out of the frame, and the
/// copies of a known small size made loads and stores, as LLVM's -O0 makes them.
fn settle(ctx: &mut cranelift_codegen::Context, cg: &Recorder) -> Result<()> {
    inline(ctx, &cg.inlined)?;
    promote::promote(&mut ctx.func, cg.named_slots());
    if copies::lower_small(&mut ctx.func) {
        promote::promote(&mut ctx.func, cg.named_slots());
    }
    Ok(())
}

/// What `compile_as` gives: the code, or, for a compile at a level above 1, what LLVM makes
/// it of once the database is let go of.
pub(crate) enum Produced {
    Code(CompiledFunction),
    Pending(Pending),
}

impl Produced {
    /// The code of a compile at level 1, which leaves nothing for later.
    fn code(self) -> CompiledFunction {
        match self {
            Produced::Code(code) => code,
            Produced::Pending(_) => unreachable!("only a compile at a level above 1 is pending"),
        }
    }
}

/// A function whose IR is built, and all that was asked of the database for it, which LLVM
/// compiles without it: an edit applied meanwhile waits for nothing.
pub struct Pending {
    func: ir::Function,
    /// The unit's target, which LLVM compiles the function for.
    target: &'static Target,
    wants: fragment::Wants,
    named: Vec<ir::StackSlot>,
    source: Option<fragment::Source>,
    variables: Option<(StoredParamEnvAndCrate, Vec<Variable>)>,
    layouts: Vec<(String, u64, u64)>,
    closure: Option<ExprId>,
    /// Its body has allocations `HeapToStack` moved into its frame
    /// (`Inlined::moved_to_frame`): it is built into no other function.
    moves_to_frame: bool,
    /// The instances whose bodies its code has in it ([`built_in_of`]).
    built_in: Vec<Instance>,
}

impl Pending {
    pub(crate) fn func(&self) -> &ir::Function {
        &self.func
    }

    /// Whether its body has allocations moved into its own frame, which no importer may build
    /// into its frame: the importer may recur, and `HeapToStack` keeps a moved slot out of
    /// every frame that may.
    pub(crate) fn moves_to_frame(&self) -> bool {
        self.moves_to_frame
    }

    /// The instances whose bodies its code has in it, at any depth, each once, as the build
    /// put them there ([`built_in_of`]): what its template's key must hold
    /// (`v2::frontend::inlined`).
    pub(crate) fn built_in(&self) -> &[Instance] {
        &self.built_in
    }

    /// The code LLVM makes of the function, at the level `request` asks for.
    pub(crate) fn produce(mut self, request: &tier::Request) -> Result<CompiledFunction> {
        let code = tier::emit(&mut self.wants, self.target, &self.named, &mut self.func, request)?;
        Ok(CompiledFunction {
            requests: self.wants.into_requests(),
            code,
            source: self.source,
            closure: self.closure,
            variables: self.variables,
            layouts: self.layouts,
        })
    }
}

/// The instances whose bodies the code of a function built from `body` by `cg` has in it, at
/// any depth, each once: those the MIR inliner put in `body` (`MirBody::inlined`, whose list
/// is transitive) and those the IR builder built in (`Recorder::built_in`, which holds what
/// each of those has in it, `built_in_body`). The one record of what a compile built in, as
/// the build did it under its unit's settings: the walk that keys a unit lists the same ahead
/// of the build (`v2/refs.rs` `Walk::inlined_callees`, `Walk::instance_call`), and the driver
/// refuses a compile whose key misses one (`blaze-codegen` `unkept_inline`).
fn built_in_of(cg: &mut Recorder<'_>, body: Option<&MirBody<'_>>) -> Vec<Instance> {
    let mut built_in = std::mem::take(&mut cg.built_in);
    for callee in body.into_iter().flat_map(|it| &it.inlined.callees) {
        let def = match callee.body {
            hir_ty_mir::mir::InlinedBody::Fn(func) => InstanceDef::Fn(func),
            hir_ty_mir::mir::InlinedBody::Closure(closure) => InstanceDef::Closure(closure.as_id()),
        };
        let inlined = Instance { def, args: callee.args, checked: cg.checked };
        if !built_in.contains(&inlined) {
            built_in.push(inlined);
        }
    }
    built_in
}

/// What tells the code of a function's lines and variables: the statements of `body` its
/// instructions were built for, where each is written ([`fragment::Source`]), and its locals
/// with a name.
fn described<'db>(
    cg: &mut Recorder<'db>,
    env: ParamEnvAndCrate<'db>,
    body: Option<&'db MirBody<'db>>,
) -> Result<(Option<fragment::Source>, Option<(StoredParamEnvAndCrate, Vec<Variable>)>)> {
    let source = match body {
        Some(body) => match body.owner {
            InferBodyId::DefWithBodyId(owner) => {
                let spans = cg.take_spans();
                let source = fragment::Source::of(cg.db, owner, body, spans)
                    .map_err(CodegenError::Unsupported)?;
                Some(source)
            }
            InferBodyId::AnonConstId(_) => None,
        },
        None => None,
    };
    let variables = std::mem::take(&mut cg.named)
        .into_iter()
        .map(|Named { binding, param, ty, slot, pointer }| Variable {
            binding,
            param,
            ty,
            slot: slot.as_u32(),
            pointer,
        })
        // alloc: the compiled function keeps them, once per function
        .collect::<Vec<_>>();
    Ok((source, Some((env.store(), variables)).filter(|it| !it.1.is_empty())))
}

/// Whether the crate has `panic = "abort"`. The standard library's crates are compiled
/// before, for whichever the program has, and whatever of theirs is compiled again is in the
/// program's crates.
fn panics_abort(db: &Db, krate: Crate) -> bool {
    let abort = cfg::CfgAtom::KeyValue { key: intern::sym::panic, value: intern::sym::abort };
    !krate.data(db).origin.is_lang() && krate.cfg_options(db).check_atom(&abort)
}

/// `-C debug-assertions` of `krate`: the library's crates are as rustc ships them, without.
fn debug_assertions(db: &Db, krate: Crate) -> bool {
    !krate.data(db).origin.is_lang()
        && krate.cfg_options(db).check_atom(&cfg::CfgAtom::Flag(intern::sym::debug_assertions))
}

/// What `intrinsics::ub_checks()` is in a body of the crate `written`, compiled by a crate
/// whose `-C debug-assertions` is `compiling`: see [`ub_checks_in`].
fn ub_checks_of(db: &Db, compiling: bool, written: Crate) -> bool {
    use base_db::{CrateOrigin, LangCrateOrigin};
    let preserves = matches!(
        written.data(db).origin,
        CrateOrigin::Lang(LangCrateOrigin::Core | LangCrateOrigin::Alloc | LangCrateOrigin::Std)
    );
    ub_checks_in(compiling, preserves, debug_assertions(db, written))
}

/// rustc's rule for `ub_checks()`: InstSimplify folds it to the `-C debug-assertions` of the
/// crate whose MIR it is (`written`), unless that crate has `#![rustc_preserve_ub_checks]`
/// (core, alloc and std do), whose MIR keeps it for codegen to make the compiling crate's.
fn ub_checks_in(compiling: bool, preserves: bool, written: bool) -> bool {
    if preserves { compiling } else { written }
}

fn build<'db>(
    cg: &mut Recorder<'db>,
    env: ParamEnvAndCrate<'db>,
    body: Option<&'db MirBody<'db>>,
    tracked: bool,
    fill: impl for<'cx> FnOnce(&mut FunctionCx<'cx, 'db>) -> Result<()>,
) -> Result<cranelift_codegen::Context> {
    let interner = DbInterner::new_with(cg.db, env.krate);
    let kept = KeptLayouts::new();
    let mut ctx = spare::context();
    ctx.func.signature.call_conv = cg.isa.default_call_conv();
    // Source locations are kept as distances from the first one set, and the distance of
    // the one just below it is taken for none at all.
    ctx.func.params.ensure_base_srcloc(ir::SourceLoc::new(0));
    let mut builder_ctx = spare::builder();
    let aborts = cg.unit.map_or_else(|| panics_abort(cg.db, env.krate), |it| it.aborts);
    let compiling =
        cg.unit.map_or_else(|| debug_assertions(cg.db, env.krate), |it| it.debug_assertions);
    let ub_checks = match body {
        Some(body) => ub_checks_of(cg.db, compiling, body.owner.module(cg.db).krate(cg.db)),
        None => compiling,
    };
    let dl = cg.db.target_data_layout(env.krate).map_err(|error| {
        // alloc: error path
        CodegenError::Refused(format!(
            "crate `{}` has no target data layout to compile for: {error}",
            env.krate.named(cg.db)
        ))
    })?;
    // The build's tables (its locals, blocks, layouts, drop glue, registers and the rest of
    // `FunctionCx`) are in a frame of the worker's, reset when the build returns.
    let guarded = blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| -> Result<bool> {
        let mut fx = FunctionCx {
            cg,
            frame,
            body,
            env,
            dl,
            infcx: interner.infer_ctxt().build(TypingMode::Codegen),
            interner,
            builder: FunctionBuilder::new(&mut ctx.func, &mut builder_ctx),
            locals: FrameMap::with_capacity_in(body.map_or(0, |it| it.locals.len()), frame),
            blocks: FrameMap::with_capacity_in(body.map_or(0, |it| it.basic_blocks.len()), frame),
            layouts: Layouts::new(&kept, frame),
            core_panics: Vec::new_in(frame),
            glue: std::cell::RefCell::new(frame_map::hash_map(frame)),
            inner_pad: None,
            unwind_to: None,
            pads: Vec::new_in(frame),
            poison: None,
            exception: None,
            argument_slots: Vec::new_in(frame),
            free_argument_slots: Vec::new_in(frame),
            given_away: GivenAway::new_in(frame),
            block: None,
            in_cleanup: false,
            aborts,
            debug_assertions: compiling,
            ub_checks,
            guarded: false,
            ret_mode: PassMode::Ignore,
            tracked,
            variadic: false,
            abi: ExternAbi::Rust,
            location: None,
            in_drop_glue: false,
            span: MirSpan::Unknown,
            constant_locals: Vec::new_in(frame),
            registers: FrameMap::new_in(frame),
            args: None,
            suspended: None,
            held_at: Vec::new_in(frame),
        };
        fill(&mut fx)?;
        fx.note_variables();
        fx.builder.seal_all_blocks();
        let config = fx.cg.target_config();
        let guarded = fx.guarded;
        fx.builder.finalize(config);
        Ok(guarded)
    })?;
    kept.keep();
    spare::keep_builder(builder_ctx);
    if guarded && let Some(entry) = ctx.func.layout.entry_block() {
        // The slot a guarded call looks for an exception in starts empty.
        use cranelift_codegen::cursor::{Cursor as _, FuncCursor};
        let pointer = cg.target.pointer();
        let mut cursor = FuncCursor::new(&mut ctx.func).at_first_insertion_point(entry);
        let frame = cursor.ins().get_frame_pointer(pointer);
        let none = cursor.ins().iconst(pointer, 0);
        cursor.ins().store(MEM, none, frame, unwind::seh::ARRIVAL);
    }
    Ok(ctx)
}

/// The body of a small function, built to be put where the function is called.
#[blaze_query::tracked(returns(ref), no_eq, cycle_result = inlinable_cycle)]
fn inlinable(
    db: &Db,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Option<fragment::Inlinable> {
    /// The most blocks and statements the body of a function that is inlined has, and
    /// the most instructions it is built to with what it calls inlined into it.
    const BLOCKS: usize = 4;
    const STATEMENTS: usize = 12;
    const INSTRUCTIONS: usize = 64;
    // Its `Recorder` is its own, which builds in no compile's hot functions (`tier::Inlining`).
    let InstanceDef::Fn(func) = instance.def else { return None };
    let generic = db.mir_body(func.into()).ok()?;
    let statements: usize = generic.basic_blocks.iter().map(|it| it.1.statements.len()).sum();
    if generic.basic_blocks.len() > BLOCKS || statements > STATEMENTS {
        return None;
    }
    // v1's alone: a v2 unit builds in `always_inlinable` bodies only.
    built_in_body(db, &instance, env, INSTRUCTIONS, None)
}

/// The body of an `#[inline(always)]` function of any size, built to be put where the function
/// is called, as LLVM's always-inliner puts it at every level rustc compiles at. `unit` is the
/// v2 unit's settings it is built under (debug assertions, panic strategy, MIR options), part
/// of the key; `None` in v1.
#[blaze_query::tracked(returns(ref), no_eq, cycle_result = always_inlinable_cycle)]
fn always_inlinable(
    db: &Db,
    instance: Instance,
    env: StoredParamEnvAndCrate,
    unit: Option<v2::build::Build>,
) -> Option<fragment::Inlinable> {
    built_in_body(db, &instance, env, usize::MAX, unit)
}

/// The body of the function `instance`, with what it calls built into it, where it is at
/// most `instructions` long: none of a closure or coroutine, or of a body that defines one.
/// It is built under `unit`'s settings, never the thread's: the callers are queries.
fn built_in_body(
    db: &Db,
    instance: &Instance,
    env: StoredParamEnvAndCrate,
    instructions: usize,
    unit: Option<v2::build::Build>,
) -> Option<fragment::Inlinable> {
    let InstanceDef::Fn(func) = instance.def else { return None };
    // A v2 caller asks under `Build::built_in`, a copy's settings (`MirOptions::built_in`).
    let generic = db.mir_body(func.into()).ok()?;
    if !generic.closures.is_empty() || generic.coroutine.is_some() {
        return None;
    }
    let at = env.as_ref(db);
    let body = instance_body(db, instance, env, unit).ok()?;
    // Not built in, never built for a guessed target: the compile asking for the body is of
    // `at.krate` and loaded its target with `?`, so a target that fails reaches it as that error.
    let target = target_of_crate(db, at.krate).ok()?;
    let mut cg = Recorder::new(db, &**isa_for(target).ok()?, target, instance.checked, unit);
    cg.unlocated = true;
    let args = instance.args;
    let mut ctx = build(&mut cg, at, Some(&*body), false, |fx| {
        fx.args = Some(args);
        fx.codegen()
    })
    .ok()?;
    if cg.naked.is_some() || cg.opaque {
        return None;
    }
    settle(&mut ctx, &cg).ok()?;
    let layout = &ctx.func.layout;
    if layout.blocks().map(|it| layout.block_insts(it).count()).sum::<usize>() > instructions {
        return None;
    }
    let pointees = std::mem::take(&mut cg.pointees);
    // The callees the MIR inliner put in the body under `unit`'s settings are in it as much
    // as the ones built in: a caller that builds this body in has their code, so its walk
    // lists them as built in (`v2/refs.rs` `Walk::instance_call`) and its key holds their code
    // (rule 21). The callee's own unit may inline none of them: `winnow`'s
    // `#[inline(always)] <&str as Stream>::eof_offset`, built into an `opt-level = 3` caller,
    // has `<str>::len` and `<str>::as_bytes` in it, which at its crate's level 0 it calls.
    let built_in = built_in_of(&mut cg, Some(&*body));
    let func = std::mem::replace(&mut ctx.func, ir::Function::new());
    spare::keep_context(ctx);
    // alloc: empty, allocates nothing
    fragment::Inlinable::of(func, cg.finish().0, pointees, Vec::new(), built_in)
}

fn inlinable_cycle(
    _: &Db,
    _: blaze_query::db::Id,
    _: Instance,
    _: StoredParamEnvAndCrate,
) -> Option<fragment::Inlinable> {
    None
}

fn always_inlinable_cycle(
    _: &Db,
    _: blaze_query::db::Id,
    _: Instance,
    _: StoredParamEnvAndCrate,
    _: Option<v2::build::Build>,
) -> Option<fragment::Inlinable> {
    None
}

/// Puts the bodies of `inlined` where `ctx` calls them.
fn inline(ctx: &mut cranelift_codegen::Context, inlined: &[ir::Function]) -> Result<()> {
    use cranelift_codegen::inline::{Inline, InlineCommand};
    struct Bodies<'a> {
        bodies: &'a [ir::Function],
        put: Vec<Put>,
    }
    /// A body that was put where it was called.
    struct Put {
        body: usize,
        /// How many global values and instructions the caller had before.
        globals: usize,
        insts: usize,
        /// The statement the call was built for.
        statement: ir::SourceLoc,
    }
    impl Inline for Bodies<'_> {
        fn inline(
            &mut self,
            caller: &ir::Function,
            call: ir::Inst,
            _: ir::Opcode,
            callee: ir::FuncRef,
            _: &[Value],
        ) -> InlineCommand<'_> {
            let ir::ExternalName::User(name) = caller.dfg.ext_funcs[callee].name else {
                return InlineCommand::KeepCall;
            };
            let name = &caller.params.user_named_funcs()[name];
            if name.namespace != fragment::INLINED {
                return InlineCommand::KeepCall;
            }
            let body = name.index as usize;
            self.put.push(Put {
                body,
                globals: caller.global_values.len(),
                insts: caller.dfg.num_insts(),
                statement: caller.srcloc(call),
            });
            InlineCommand::Inline { callee: Cow::Borrowed(&self.bodies[body]), visit_callee: true }
        }
    }
    if inlined.is_empty() {
        return Ok(());
    }
    let mut bodies = Bodies { bodies: inlined, put: Vec::with_capacity(inlined.len()) };
    // alloc: the error's message
    ctx.inline(&mut bodies).map_err(|err| CodegenError::Unsupported(err.to_string()))?;
    // Cranelift copies a symbol of the callee with the name it has among the callee's
    // names, which among the caller's is another symbol's.
    let ends = bodies.put.iter().skip(1).map(|it| it.insts).chain([ctx.func.dfg.num_insts()]);
    let longest = inlined.iter().map(|it| it.dfg.num_insts()).max().unwrap_or(0);
    let (mut dfs, mut order) =
        (cranelift_codegen::traversals::Dfs::new(), Vec::with_capacity(longest));
    for (put, end) in bodies.put.iter().zip(ends) {
        let body = &inlined[put.body];
        // Cranelift makes the instructions of a body it inlines in the order it visits the
        // body's, and gives them no source location: an `offset`'s mark goes by that order,
        // where every instruction is the one it is made from (a return becomes a jump).
        order.clear();
        order.extend(dfs.pre_order_iter(body).flat_map(|block| body.layout.block_insts(block)));
        let made = |at: usize| ir::Inst::from_u32((put.insts + at) as u32);
        let in_order = order.len() <= end - put.insts
            && order.iter().enumerate().all(|(at, &it)| {
                let from = body.dfg.insts[it].opcode();
                from.is_return() || ctx.func.dfg.insts[made(at)].opcode() == from
            });
        // What a body is built to is of the statement that called it.
        for at in 0..end - put.insts {
            let from = order.get(at).filter(|_| in_order);
            let marked = from.is_some_and(|&it| unwind::seh::is_in_bounds(body.srcloc(it)));
            let loc = if marked { unwind::seh::in_bounds(put.statement) } else { put.statement };
            ctx.func.set_srcloc(made(at), loc);
        }
        for (global, data) in body.global_values.iter() {
            let ir::GlobalValueData::Symbol { name: ir::ExternalName::User(name), .. } = data
            else {
                continue;
            };
            // clone: two `u32`s; Cranelift's `UserExternalName` is not `Copy`
            let name = body.params.user_named_funcs()[*name].clone();
            let name = ctx.func.declare_imported_user_function(name);
            let copy = ir::GlobalValue::from_u32(put.globals as u32 + global.as_u32());
            let ir::GlobalValueData::Symbol { name: copied, .. } =
                &mut ctx.func.global_values[copy]
            else {
                unreachable!("a symbol is copied as one")
            };
            *copied = ir::ExternalName::User(name);
        }
    }
    Ok(())
}

/// [`emit_lent`] into lists of its own, at Cranelift's default optimization: v1's compile
/// (`Backend::emit`, and the shims it defines), and what the tests compile a function with.
fn emit(ctx: &mut cranelift_codegen::Context, target: &'static Target) -> Result<Code> {
    // alloc: none until the code's rows are written, then exactly that many: the caller keeps them
    emit_with(
        ctx,
        target,
        false,
        Features::BASELINE,
        backend::V1_FRAME_POINTERS,
        Relocs::default(),
        Vec::new(),
    )
}

/// Runs Cranelift on a finished function: v2's compile (`v2/backend.rs`), at
/// `opt_level=speed` when `speed` (code no edit reaches, the standard library's and
/// dependencies', whose unit's key says so: `blaze_codegen::Backend::Cranelift { speed }`),
/// with the function's target features (its unit's and its `#[target_feature]`'s), keeping
/// every frame pointer when `frame_pointers`, its unit's `Toolchain::frame_pointers`.
/// v2 keeps what it compiles in the back-end memo by the function's shape, which is its printed
/// IR with names by place, so Cranelift's incremental cache would be a second key over the same
/// IR (a serialization and hash of the whole function, and a lookup in v1's store) that the
/// memo already missed on. It compiles with none. The relocations and line rows are written
/// into the thread's kept lists ([`spare::relocs`], [`spare::lines`]), which v2's conversion
/// gives back ([`spare::keep_relocs`], [`spare::keep_lines`]).
fn emit_lent(
    ctx: &mut cranelift_codegen::Context,
    target: &'static Target,
    speed: bool,
    features: Features,
    frame_pointers: bool,
) -> Result<Code> {
    emit_with(ctx, target, speed, features, frame_pointers, spare::relocs(), spare::lines())
}

/// Runs Cranelift on a finished function, writing its relocations and line rows into `relocs`
/// and `lines`, which are empty. Cranelift panics on IR it has no lowering for (i128 corners,
/// for one); that must cost us one function, not the process.
fn emit_with(
    ctx: &mut cranelift_codegen::Context,
    target: &'static Target,
    speed: bool,
    features: Features,
    frame_pointers: bool,
    relocs: Relocs,
    lines: Vec<LineRow>,
) -> Result<Code> {
    refuse_tail_calls(&ctx.func, target)?;
    if target.is_wasm() {
        return emit_wasm(&ctx.func, target.calls(), wasm::Address::of(target));
    }
    let large = ctx.func.dfg.num_insts() > SINGLE_PASS;
    let kind = if large { Isa::Large } else { Isa::Usual };
    let mut isa = &**target_of(target, features, kind, speed)?;
    let func = &ctx.func;
    // Cranelift reads the return address from the function's frame record, which only a
    // function compiled to keep its frame pointer has (`core::intrinsics::return_address`).
    let reads_return_address = || {
        func.layout.blocks().any(|block| {
            func.layout
                .block_insts(block)
                .any(|inst| func.dfg.insts[inst].opcode() == ir::Opcode::GetReturnAddress)
        })
    };
    // On Apple's targets every function keeps one, as their ABI wants of every function that
    // calls (`Target::requires_frame_pointers`): Cranelift has no "non-leaf only", and its
    // code kept them there before the toolchain stopped saying so for the target.
    if !isa.flags().preserve_frame_pointers()
        && (frame_pointers
            || target.requires_frame_pointers()
            || func.layout.entry_block().is_some_and(|entry| {
                func.layout
                    .block_insts(entry)
                    .any(|inst| func.dfg.insts[inst].opcode() == ir::Opcode::GetFramePointer)
            })
            || reads_return_address())
    {
        isa = &**target_of(target, features, Isa::FramePointer, speed)?;
    }
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.compile(isa, &mut Default::default())
            .map(drop)
            // alloc: the error's message
            .map_err(|e| format!("{:?}", e.inner))?;
        backend::Tally::global().passes();
        let Some(compiled) = ctx.compiled_code() else {
            // alloc: the error's message
            return Err("no compiled code".to_owned());
        };
        let mut relocs = relocs;
        // alloc: the error's message
        relocs.extend_mach(compiled.buffer.relocs(), &ctx.func).map_err(|e| e.to_string())?;
        let mut lines = lines;
        LineRow::extend_ranges(
            &mut lines,
            compiled.buffer.get_srclocs_sorted().iter().map(|it| (it.start, it.loc.as_abs())),
        );
        Ok(Code {
            // clone: the kept code's signature, a few parameters
            signature: ctx.func.signature.clone(),
            // alloc: the kept code, written once at its exact length
            bytes: compiled.buffer.data().to_vec(),
            align: compiled.buffer.min_alignment.into(),
            relocs,
            unwind: FunctionUnwind::of(isa, ctx)?,
            lines,
            slots: SlotPlaces::default(),
        })
    }));
    match outcome {
        Ok(Ok(code)) => Ok(code),
        Ok(Err(e)) => Err(CodegenError::Backend(e)),
        Err(panic) => {
            let message = stdx::panic_text(&*panic).unwrap_or("unknown");
            // alloc: the error's message
            Err(CodegenError::Backend(format!("backend panic: {message}")))
        }
    }
}

/// Why a `become` of `func` can't be a tail call, where it can't: the compiler supplies what
/// it does in the caller, so it is no function to jump to (rustc refuses the intrinsics and
/// the `Fn` traits' methods before codegen; the rest are the standard library's, whose code
/// has no `become` of them), or it is C-variadic. The IR builder and the refs walk ask the
/// same question, so the walk lists nothing the builder would not call.
fn tail_call_refused(
    db: &Db,
    target: &Target,
    interner: DbInterner<'_>,
    func: FunctionId,
) -> Option<String> {
    let signature = FunctionSignature::of(db, func);
    let name = signature.name.as_str();
    let lang_items = interner.lang_items();
    if FunctionSignature::is_intrinsic(db, func)
        || fncache::is_code(db, func)
        || fn_trait_methods(lang_items).iter().any(|it| it.contains(&Some(func)))
        || [lang_items.DropInPlace, lang_items.DropGlue].contains(&Some(func))
        || func.krate(db).data(db).origin.is_lang() && is_ub_check(signature.name.symbol())
        || link_name_for(db, target, func).is_some_and(|it| it.starts_with("llvm."))
    {
        // alloc: the error's message
        return Some(format!(
            "a tail call (`become`) of `{name}`, whose body the compiler supplies where it is \
             called: it is no function to jump to"
        ));
    }
    // alloc: the error's message
    signature.is_varargs().then(|| format!("a tail call (`become`) of the C-variadic `{name}`"))
}

/// Refuses a function with a tail call (`become`'s `return_call`) where the code is
/// Cranelift's own or blaze's WebAssembly: Cranelift makes a tail call only under its
/// `tail` convention, which no Rust function has (a call and a return in its place would
/// grow the stack `become` promises not to), and the WebAssembly lowering has no
/// `return_call` yet. The copy-and-patch stencils and LLVM (`musttail`) make it, and v2's
/// plan gives native code to no other (`blaze_codegen::plan::Policy::choose`), so a native
/// refusal here is a tiered run's Cranelift tier (`blaze_stencil::tier`), or v1's compile.
fn refuse_tail_calls(func: &ir::Function, target: &'static Target) -> Result<()> {
    let convention = func.signature.call_conv;
    if !target.is_wasm() && convention.supports_tail_calls() {
        return Ok(());
    }
    let tail_calls = func.layout.blocks().any(|block| {
        func.layout.block_insts(block).any(|inst| {
            matches!(
                func.dfg.insts[inst].opcode(),
                ir::Opcode::ReturnCall | ir::Opcode::ReturnCallIndirect
            )
        })
    });
    if tail_calls {
        match target.is_wasm() {
            true => unsupported!(
                "a tail call (`become`) in code for WebAssembly, whose lowering has no \
                 `return_call` yet"
            ),
            false => unsupported!(
                "a tail call (`become`) in code Cranelift's native back end makes: it \
                 guarantees one only under its `tail` convention, never the target's \
                 `{convention}`; `become` builds with the copy-and-patch stencils or LLVM, \
                 the only native back ends of v2's plan (`Policy::choose`)"
            ),
        }
    }
    Ok(())
}

/// Lowers a finished function to WebAssembly. The bytes are a [`wasm::WasmFunction`], which
/// goes through the object like machine code and is made a module of by a linker.
fn emit_wasm(
    func: &cranelift_codegen::ir::Function,
    calls: Calls,
    address: wasm::Address,
) -> Result<Code> {
    let (function, mut relocs) =
        wasm::lower(func, calls, address).map_err(CodegenError::Unsupported)?;
    let (bytes, body) = function.encode().map_err(CodegenError::Backend)?;
    for reloc in &mut relocs {
        reloc.offset += body;
    }
    Ok(Code {
        // clone: the kept code's signature, a few parameters; `func` is the caller's
        signature: func.signature.clone(),
        bytes,
        align: 1,
        relocs: Relocs::of_module(&relocs, &[])?,
        unwind: None,
        // alloc: empty, allocates nothing
        lines: Vec::new(),
        slots: SlotPlaces::default(),
    })
}

/// The body of a non-generic function, with the types it only knows as `impl Trait`
/// resolved.
fn root_body<'db>(
    db: &'db Db,
    func: FunctionId,
) -> Result<(&'db MirBody<'db>, ParamEnvAndCrate<'db>)> {
    let owner: InferBodyId<'db> = func.into();
    let tally = backend::Tally::global();
    tally.step(Step::Infer, || _ = InferenceResult::of(db, DefWithBodyId::from(func)));
    let krate = owner.module(db).krate(db);
    // v1's alone (its callers are v1's, several of them queries): a v2 unit reads bodies
    // through `instance_body` with its own settings.
    let options = mir_options_of(db, krate)?;
    let stage = MirStage::compiled(MirOwner::Body(owner), options);
    let body = tally.step(Step::Mir, || db.mir(MirOwner::Body(owner), stage));
    let body = body.map_err(|e| mir_failed(db, MirOwner::Body(owner), &e))?;
    // Not the function's own bounds: with nothing generic they are bounds on concrete
    // types, which hold through impls, and the calls have to resolve to those.
    let env = ParamEnvAndCrate { param_env: hir_ty::next_solver::ParamEnv::empty(), krate };
    // The future an `async fn` returns is `impl Future` to the body that calls it.
    let opaque = body.locals.iter().any(|(_, local)| {
        rustc_type_ir::TypeVisitableExt::has_type_flags(
            &local.ty.as_ref(),
            rustc_type_ir::TypeFlags::HAS_TY_OPAQUE,
        )
    });
    let body = if opaque {
        let args = GenericArgs::new_from_slice(&[]).store();
        tally
            .step(Step::Monomorphize, || {
                db.monomorphized_mir(MirOwner::Body(owner), stage, args, env.store())
            })
            .map_err(|e| mir_failed(db, MirOwner::Body(owner), &e))?
    } else {
        body
    };
    Ok((body, env))
}

thread_local! {
    /// The relocations of the function [`ObjectCodegen::define_code`] puts in the object, and
    /// those of them Cranelift is given: kept by the thread from one function to the next. A
    /// call inside one that has them taken finds them empty, and grows its own.
    // alloc: empty, allocates nothing
    static RELOCS: Cell<Vec<(ModuleReloc, Option<u32>)>> = const { Cell::new(Vec::new()) };
    // alloc: empty, allocates nothing
    static PLAIN_RELOCS: Cell<Vec<ModuleReloc>> = const { Cell::new(Vec::new()) };
}

/// What a call to an intrinsic turned into.
enum Lowered {
    /// Lowered inline; continue at the call's target.
    Inline,
    /// Lowered to a trap; the block is already terminated.
    Diverges,
    /// Not known here; call it like a normal function.
    Unknown,
}

/// The pointers of one constant, and the data objects made for what they point to.
struct Relocations<'a, 'db> {
    map: &'a MemoryMap<'db>,
    /// The type of the constant whose value this relocates, for an error to name.
    of: Ty<'db>,
    /// Which constant it is, for an error to name (rule 19).
    whose: ConstOf<'db>,
    in_static: bool,
    /// The data object of each allocation made so far, by start.
    defined: HashMap<usize, Defined<'db>, Global>,
    /// The allocations whose data objects are being made, by start: what const eval made of a
    /// constant points in no circle (a static is named, not walked).
    walking: Vec<usize>,
    /// The words a walk by type left as they are on purpose, though they have provenance: a
    /// reference to nothing, made its alignment.
    handled: Vec<PointerAt>,
    /// The bytes a type says are a number (an integer, a float, a `bool`, a `char`, a slice's
    /// length), by the memory they are in (`None` for the constant's value): a word of them
    /// equal to one of const eval's ids is that number. Only where a type can be trusted, not
    /// behind a raw pointer.
    numbers: HashMap<Option<usize>, Vec<(usize, usize)>, Global>,
    /// The unions walked, by the memory they are in: where they are and their type, for an
    /// error about a word in one to name it.
    unions: Vec<(Option<usize>, usize, usize, Ty<'db>)>,
}

impl<'db> Relocations<'_, 'db> {
    /// The alignment to give the data object made for the memory at `start`: what const eval
    /// made it with. A literal's map (`Simple`) is the bytes of a string or byte string, of
    /// alignment 1. `None` where const eval recorded none, which the caller refuses: a guess
    /// could under-align a pointee that needs more (`#[repr(align(64))]`), as rustc never does.
    fn align_of(&self, start: usize) -> Option<u64> {
        match self.map {
            MemoryMap::Simple(_) => Some(1),
            MemoryMap::Empty | MemoryMap::Complex(_) => {
                self.map.align_at(start).map(|align| align as u64)
            }
        }
    }

    /// Whether the word at `at` is a pointer with provenance. A map of a literal's reference
    /// says nothing of provenance, and its pointers are what its type says.
    fn provenance(&self, at: PointerAt) -> bool {
        match self.map {
            MemoryMap::Complex(_) => self.map.has_provenance(at),
            MemoryMap::Empty | MemoryMap::Simple(_) => true,
        }
    }

    /// Whether the map says which words have provenance, as const eval's always does.
    fn tracks_provenance(&self) -> bool {
        matches!(self.map, MemoryMap::Complex(_))
    }

    /// Notes that the `size` bytes at `start` of the memory at `kept` are a number.
    fn number(&mut self, kept: Option<usize>, start: usize, size: usize) {
        if size > 0 {
            // alloc: the numbers of one allocation, for its walk
            self.numbers.entry(kept).or_insert_with(Vec::new).push((start, start + size));
        }
    }

    /// Whether a type says every byte of the `word` at `at` of the memory at `kept` is a
    /// number.
    fn is_number(&self, kept: Option<usize>, at: usize, word: usize) -> bool {
        let Some(ranges) = self.numbers.get(&kept) else { return false };
        let mut pos = at;
        // Each step moves `pos` to the end of a range that holds it, so at most one a range.
        for _ in 0..=ranges.len() {
            if pos >= at + word {
                return true;
            }
            match ranges.iter().find(|&&(start, end)| start <= pos && pos < end) {
                Some(&(_, end)) => pos = end,
                None => return false,
            }
        }
        pos >= at + word
    }

    /// The union the byte at `at` of the memory at `kept` is in, if a walk went through one.
    fn union_at(&self, kept: Option<usize>, at: usize) -> Option<Ty<'db>> {
        (self.unions.iter()).find(|it| it.0 == kept && it.1 <= at && at < it.2).map(|it| it.3)
    }
}

/// Whether a value of `ty` is a number, with no pointer in it.
fn is_number(ty: Ty<'_>) -> bool {
    matches!(
        ty.kind(),
        TyKind::Int(_) | TyKind::Uint(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
    )
}

/// Where the bytes a walk goes through were in const eval, and whether a type describes them.
#[derive(Clone, Copy)]
struct Walk {
    /// The address of the memory const eval kept they are, or `None` for the constant's value.
    kept: Option<usize>,
    /// Reached through a raw pointer, whose pointee type may be a cast's: a word that type
    /// says is a pointer is one only with provenance.
    raw: bool,
}

impl Walk {
    /// Where const eval's provenance of the word at `offset` is.
    fn word(self, offset: usize) -> PointerAt {
        match self.kept {
            Some(start) => PointerAt::Memory(start + offset),
            None => PointerAt::Value(offset),
        }
    }
}

/// How a pointer reaches an allocation: the type it walks it as, at an offset.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Reach<'db> {
    ty: Ty<'db>,
    /// The pointer's type's pointee: `ty` but for a `dyn`, whose `ty` is its concrete type.
    pointee: Ty<'db>,
    at: usize,
    raw: bool,
    /// A slice's or `str`'s length.
    len: Option<usize>,
    /// The concrete type of the `dyn` tail of a struct.
    tail: Option<Ty<'db>>,
}

/// The data object made of an allocation, how it was walked, and what it was made of, which
/// a later walk of it as another type must make again.
struct Defined<'db> {
    data: DataId,
    by: Option<Reach<'db>>,
    blob: ConstBlob,
}

/// The memory const eval kept that holds `addr`, with the address it starts at, or that `addr`
/// is one past the end of, as a pointer made by offsetting to the end is.
fn memory_holding<'m>(map: &'m MemoryMap<'_>, addr: usize) -> Option<(usize, &'m [u8])> {
    map.memory_at(addr).or_else(|| {
        let (start, memory) = map.memory_at(addr.checked_sub(1)?)?;
        (addr - start == memory.len()).then_some((start, memory))
    })
}

/// Whether a data object can be the same object as another with the same content.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde_derive::Serialize, serde_derive::Deserialize,
)]
enum DataIdentity {
    /// Read-only and one object with every equal constant, as the same constant is for rustc
    /// wherever it is named.
    Shared,
    /// Read-only with an address of its own, as a static has.
    Own,
    Writable,
}

/// Read-only data whose pointers refer to other data objects.
#[derive(
    Debug, Default, Clone, PartialEq, Eq, Hash, serde_derive::Serialize, serde_derive::Deserialize,
)]
struct ConstBlob {
    bytes: Vec<u8>,
    relocs: Vec<(u32, DataId, i64)>,
    fn_relocs: Vec<(u32, FuncId)>,
    align: u64,
    /// The `TypeId`s in `bytes` by offset, which hold the ids const eval gave their types
    /// until [`ObjectCodegen::fill_data`] writes rustc's hashes there. A blob is kept with
    /// the hashes in it.
    #[serde(skip)]
    type_ids: Vec<(u32, StoredTy)>,
}

impl ConstBlob {
    /// Whether the bytes are all there is to it, with no address among them.
    #[cfg(feature = "llvm")]
    fn points_nowhere(&self) -> bool {
        self.relocs.is_empty() && self.fn_relocs.is_empty()
    }

    #[cfg(feature = "llvm")]
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Puts the relocations in the order of their offsets, which one offset has at most one
    /// of: a blob is compared and content-hashed with them, whatever order a walk found them.
    fn in_order(&mut self) {
        self.relocs.sort_unstable_by_key(|it| it.0);
        self.fn_relocs.sort_unstable_by_key(|it| it.0);
        self.type_ids.sort_unstable_by_key(|it| it.0);
    }

    /// Makes the pointer at `offset` the address `at` bytes into `target`.
    fn point_to(&mut self, offset: usize, target: DataId, at: usize, word: usize) {
        self.bytes[offset..offset + word].fill(0);
        self.relocs.push((offset as u32, target, at as i64));
    }

    fn usize_at(&self, offset: usize, word: usize) -> Option<usize> {
        let bytes = self.bytes.get(offset..offset + word)?;
        Some(bytes.iter().rev().fold(0, |value, &byte| value << 8 | usize::from(byte)))
    }
}

/// A place in memory. `meta` is the slice length (or vtable) when the place came from a fat
/// pointer.
#[derive(Clone, Copy)]
struct PlaceRef<'db> {
    addr: Value,
    meta: Option<Value>,
    ty: PlaceTy<'db>,
}

/// A field that some statement moves out of a local, as the field indices leading to it.
/// The integer type drop glue reads an enum's discriminant as, to pick the variant.
#[derive(Clone, Copy)]
enum DiscriminantWidth {
    Word,
    /// `#[repr(i128)]` and `#[repr(u128)]` enums.
    Wide,
}

impl DiscriminantWidth {
    fn of(tag: &rustc_abi::Scalar) -> Self {
        match tag.primitive() {
            Primitive::Int(Integer::I128, _) => Self::Wide,
            _ => Self::Word,
        }
    }

    fn scalar(self) -> AbiScalar {
        match self {
            Self::Word => AbiScalar { ty: types::I64, signed: true },
            Self::Wide => AbiScalar { ty: types::I128, signed: true },
        }
    }

    /// The switch entry for a variant whose discriminant is `value`.
    fn entry(self, value: i128) -> u128 {
        match self {
            Self::Word => value as i64 as u64 as u128,
            Self::Wide => value as u128,
        }
    }
}

/// The locals a call's operands move whole, whose memory the callee can be handed as it is:
/// one for the whole build, emptied at each call ([`GivenAway::start`]).
struct GivenAway<'f> {
    /// While a call is generated: the local its result is written to.
    result: Option<LocalId>,
    whole: FrameHashSet<'f, Value>,
    /// The address of every place the call's operands read.
    read: Vec<Value, &'f Frame>,
}

impl<'f> GivenAway<'f> {
    fn new_in(frame: &'f Frame) -> Self {
        GivenAway { result: None, whole: frame_map::hash_set(frame), read: Vec::new_in(frame) }
    }

    /// Starts the call whose result goes to `result`, forgetting the last call's.
    fn start(&mut self, result: LocalId) {
        self.result = Some(result);
        self.whole.clear();
        self.read.clear();
    }

    /// Whether, while a call is generated, the memory at `addr` is a local moved whole that no
    /// other operand reads: a field at the start of a local has the local's address.
    fn gives(&self, addr: Value) -> bool {
        self.result.is_some()
            && self.whole.contains(&addr)
            && self.read.iter().filter(|&&it| it == addr).count() == 1
    }
}

/// A local kept in a register.
#[derive(Clone, Copy)]
struct Register {
    var: cranelift_frontend::Variable,
    ty: Type,
}

#[derive(Clone, Copy)]
struct LocalSlot {
    addr: Value,
    /// The local's type is unsized, and the slot holds a fat pointer to the value where it
    /// was moved from in place of the value.
    borrowed: bool,
}

struct FunctionCx<'cx, 'db> {
    cg: &'cx mut Recorder<'db>,
    /// The build's frame: what the tables below are kept in, reset when the build returns.
    frame: &'cx Frame,
    /// `None` while generating a shim that has no MIR.
    body: Option<&'db MirBody<'db>>,
    env: ParamEnvAndCrate<'db>,
    /// The target layout of `env.krate`, read once in [`build`]: a crate without one stops it.
    dl: &'db rustc_abi::TargetDataLayout,
    infcx: InferCtxt<'db>,
    interner: DbInterner<'db>,
    builder: FunctionBuilder<'cx>,
    locals: FrameMap<'cx, LocalId, LocalSlot>,
    blocks: FrameMap<'cx, BasicBlockId, ir::Block>,
    /// The layouts asked for so far: a body asks for that of the same few types many times.
    layouts: Layouts<'cx, 'db>,
    /// The panic functions of core called so far, by name: resolving one walks core's modules
    /// by name, and every checked `+` of the body calls one.
    core_panics: Vec<(&'static str, Option<FunctionId>), &'cx Frame>,
    /// Whether dropping each type asked about so far does anything.
    glue: std::cell::RefCell<FrameHashMap<'cx, Ty<'db>, DropGlue>>,
    /// A landing pad that takes over from the cleanup block of `unwind_to` for the calls
    /// made while it is set: drop glue unwinding from one field's drop to the rest.
    inner_pad: Option<ir::Block>,
    /// The cleanup block that what is being generated unwinds to. The body is in
    /// `MirPhase::Runtime`: its drops are elaborated and it names its cleanup blocks.
    unwind_to: Option<BasicBlockId>,
    /// The landing pad of each cleanup block something unwinds to.
    pads: Vec<(BasicBlockId, ir::Block), &'cx Frame>,
    /// In an `async` body: the landing pad of what unwinds with nothing to clean up, which
    /// marks the body as panicked.
    poison: Option<ir::Block>,
    /// Where the exception is kept while the cleanup blocks run.
    exception: Option<ir::StackSlot>,
    /// The slots the arguments of the calls so far were copied to since the last were
    /// freed, each with its size and the power of two it is aligned to.
    argument_slots: Vec<(ir::StackSlot, (u32, u8)), &'cx Frame>,
    /// The ones whose call has returned.
    free_argument_slots: Vec<(ir::StackSlot, (u32, u8)), &'cx Frame>,
    /// While a call is generated (`result` is set): the local its result goes to, and where
    /// the locals moved whole into it are. Nothing reads those again, so the callee can be
    /// handed them.
    given_away: GivenAway<'cx>,
    /// The block of the body being generated.
    block: Option<BasicBlockId>,
    in_cleanup: bool,
    /// The crate it is compiled for has `panic = "abort"`: no call of it unwinds (rustc's
    /// `fn_can_unwind`), and so none has a landing pad.
    aborts: bool,
    /// `-C debug-assertions` of the crate it is compiled for: what `check_language_ub()` and
    /// `check_library_ub()` of core are at run time.
    debug_assertions: bool,
    /// What `intrinsics::ub_checks()` is in the body ([`ub_checks_in`]).
    ub_checks: bool,
    /// Whether a call was made by [`Self::guarded_call`].
    guarded: bool,
    ret_mode: PassMode,
    /// Whether the function is `#[track_caller]` and so takes its caller's location as a last,
    /// hidden argument.
    tracked: bool,
    /// The function takes `...`, and its last parameter is the list of what came for that.
    variadic: bool,
    /// The calling convention of the function being built.
    abi: ExternAbi,
    location: Option<Value>,
    /// The function is drop glue, which is given no `Location`: a `#[track_caller]` callee
    /// (a `Drop::drop` with it) is told the declaration of `core::ptr::drop_glue`, as rustc
    /// spans its drop shim at the definition of its `DefId` (`build_drop_shim`).
    in_drop_glue: bool,
    /// Where the statement being compiled comes from.
    span: MirSpan,
    /// Promoted locals that are read-only data instead of a slot the body writes to.
    constant_locals: Vec<LocalId, &'cx Frame>,
    /// The locals kept in a register in place of their slot: scalars nothing takes the
    /// address of. The slot is still there, written when something asks where the local is.
    registers: FrameMap<'cx, LocalId, Register>,
    /// What the body's generic parameters stand for, when it is the body of an instance.
    args: Option<StoredGenericArgs>,
    /// While compiling an `async` body: where it says where it left off.
    suspended: Option<SuspendedState>,
    /// The locals an `async` body keeps in its future, and how far into it each is.
    held_at: Vec<(LocalId, u32), &'cx Frame>,
}

/// The layouts a function's build asked for, each asked of the database once and then lent
/// out as `&Layout` for the whole build. A layout is hash-consed for the life of the process
/// (`hir_ty::layout::InternedLayout`), so what is lent is the interned one itself, with no
/// reference count to write. A lookup is one hash probe of an interned `Ty` (a pointer)
/// and no write outside the thread.
struct Layouts<'cx, 'db> {
    kept: &'cx KeptLayouts,
    of: std::cell::RefCell<FrameHashMap<'cx, Ty<'db>, &'cx Layout>>,
    /// The layout whose fields are a variant's, and the tag a value of it has (where, as what
    /// and its value), by the variant and the arguments of its type: `None` for a variant the
    /// type has no room for. Building one copies the variant's field offsets, which each
    /// aggregate of the variant would otherwise do again.
    variants: std::cell::RefCell<
        FrameHashMap<'cx, (VariantId, GenericArgs<'db>), Option<VariantShape<'cx>>>,
    >,
}

/// What [`Layouts`] lends a variant's layout from, owned by `build` for the whole of it and
/// freed after: the variant's own copy (`Box`), dropped at the end of the build. A layout the
/// database gives is interned and needs no keeping. The list of pointers is the thread's
/// ([`spare::layout_list`]), given back emptied. It is not in the build's frame: lent from
/// for `'cx` and dropped after it, a list whose allocator is `&'cx Frame` would still be
/// borrowed when it drops.
struct KeptLayouts {
    variants: Lent<Box<Layout>>,
}

impl KeptLayouts {
    fn new() -> Self {
        Self { variants: Lent::from_room(spare::layout_list()) }
    }

    /// Drops what it kept and gives its list back to the thread.
    fn keep(self) {
        spare::keep_layout_list(self.variants.into_room());
    }
}

/// A variant's layout and its tag, as [`FunctionCx::variant_shape`] gives them.
type Tag = Option<(usize, types::Type, u128)>;
type VariantShape<'cx> = (&'cx Layout, Tag);

impl<'cx> Layouts<'cx, '_> {
    fn new(kept: &'cx KeptLayouts, frame: &'cx Frame) -> Self {
        let of = std::cell::RefCell::new(frame_map::hash_map(frame));
        Self { kept, of, variants: std::cell::RefCell::new(frame_map::hash_map(frame)) }
    }
}

/// Where an `async` body says where it left off, and what that says of its locals.
struct SuspendedState {
    addr: Value,
    ty: Type,
    held: hir_ty_mir::mir::HeldLocals,
}

/// `args` with every lifetime but a bound one erased: lifetimes change nothing about the
/// code, and the ones a caller passes on are its own parameters, which mean nothing in the
/// callee. The fold is compile identity's ([`hir_ty_identity::instance::erased`]), which folds
/// each distinct type once: a root like rust#37311's `A70` (`(&'static A69, &'static A69)`) is
/// 2^70 types as a tree, and a fold with no memo never ends on it (review 7, P1).
///
/// What was erased is kept per thread. The arguments of a closure hold the types of all it
/// captured, and of the closures those hold: erased again at every call, they are interned
/// again each time, and every thread of a build waits on the interner's locks.
pub(crate) fn erase_regions<'db>(
    interner: DbInterner<'db>,
    args: GenericArgs<'db>,
) -> GenericArgs<'db> {
    use rustc_type_ir::TypeVisitableExt;
    /// How many are kept before they are let go, as each keeps its types interned.
    const KEPT: usize = 1 << 16;
    thread_local! {
        static ERASED: std::cell::RefCell<HashMap<StoredGenericArgs, StoredGenericArgs, Global>> =
            std::cell::RefCell::default();
    }
    // Binders are anonymized too (`erased`), so a type with bound variables but no free region
    // is erased as well: `for<'a> fn(&'a u8)` and `for<'b> fn(&'b u8)` are one instance.
    if !args.has_type_flags(hir_ty_identity::instance::ERASES) {
        return args;
    }
    let key = args.store();
    if let Some(known) = ERASED.with_borrow(|it| it.get(&key).map(|it| it.as_ref())) {
        return known;
    }
    let erased = hir_ty_identity::instance::erased(interner, args);
    ERASED.with_borrow_mut(|it| {
        if it.len() >= KEPT {
            it.clear();
        }
        it.insert(key, erased.store());
    });
    erased
}

/// Where an `async` body left off: before its first statement, after its last, part of the way
/// through unwinding, or at the await with the index that is added to `SUSPENDED`.
const UNRESUMED: i64 = 0;
const RETURNED: i64 = 1;
const PANICKED: i64 = 2;
const SUSPENDED: i64 = 3;

/// The local a body's caller gets the value of. An `async` body computes the value of its
/// block in the return slot, and hands back a `Poll` of it.
fn returned_local(body: &MirBody<'_>) -> LocalId {
    body.coroutine.map_or(return_slot(), |it| it.poll)
}

/// The local a body returns in, as `hir_ty_mir::mir`'s own private `return_slot`.
fn return_slot() -> LocalId {
    LocalId::from_raw(la_arena::RawIdx::from(0))
}

/// A local of a body that holds a promoted constant: `expr` of the expression store of
/// `owner`, evaluated with the arguments of `callee` of the body's inlined callees, or with the
/// instance's own for `None`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Promoted<'db> {
    pub(crate) local: LocalId,
    pub(crate) owner: InferBodyId<'db>,
    pub(crate) expr: ExprId,
    pub(crate) callee: Option<u32>,
}

impl Promoted<'_> {
    /// The arguments the constant is evaluated with: of the callee it was inlined from, or
    /// `own`, the instance's.
    pub(crate) fn args(
        &self,
        body: &MirBody<'_>,
        own: Option<&StoredGenericArgs>,
    ) -> Option<StoredGenericArgs> {
        match self.callee {
            Some(callee) => Some(body.inlined.callees[callee as usize].args),
            None => own.copied(),
        }
    }
}

/// The promoted constants of `body`: its own (`MirBody::promoted_locals`) and those of the
/// bodies inlined into it (`Inlined::promoted`).
pub(crate) fn promoted_constants<'a, 'db>(
    db: &'db Db,
    body: &'a MirBody<'db>,
) -> impl Iterator<Item = Promoted<'db>> + 'a {
    let own = body.promoted_locals.iter().map(|&(local, expr)| Promoted {
        local,
        owner: body.owner,
        expr,
        callee: None,
    });
    let inlined = body.inlined.promoted.iter().map(move |it| Promoted {
        local: it.local,
        owner: owner_of(db, body.inlined.callees[it.callee as usize].body),
        expr: it.expr,
        callee: Some(it.callee),
    });
    own.chain(inlined)
}

/// The body `owner` as an error names it: its item's path in backticks, or "an anonymous
/// constant".
fn body_path(db: &Db, owner: InferBodyId<'_>) -> String {
    match owner {
        InferBodyId::DefWithBodyId(def) => {
            format!("`{}`", blaze_items::key::path(db, blaze_items::ItemId::of_body(db, def))) // alloc: an error's text
        }
        InferBodyId::AnonConstId(_) => "an anonymous constant".to_owned(), // alloc: an error's text
    }
}

/// Which constant a value [`FunctionCx::allocation_blob`] relocates is, for its errors to
/// name by path (rule 19), not only by type.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ConstOf<'db> {
    /// A named constant, a static, or an anonymous constant.
    Item(GeneralConstId<'db>),
    /// A promoted constant of a body.
    Promoted(Promoted<'db>),
    /// A value the MIR of a body holds in place (a literal's allocation), in the body being
    /// built (`None`: of no body).
    InPlace(Option<InferBodyId<'db>>),
}

impl ConstOf<'_> {
    /// The constant as an error names it: "the constant `krate::m::C`", "the promoted
    /// constant ExprId(3) of `krate::f`".
    fn describe(self, db: &Db) -> String {
        use blaze_items::{ItemId, key::path};
        match self {
            ConstOf::Item(GeneralConstId::ConstId(id)) => {
                format!("the constant `{}`", path(db, ItemId::Const(id))) // alloc: an error's text
            }
            ConstOf::Item(GeneralConstId::StaticId(id)) => {
                format!("the static `{}`", path(db, ItemId::Static(id))) // alloc: an error's text
            }
            ConstOf::Item(GeneralConstId::AnonConstId(_)) => {
                "an anonymous constant".to_owned() // alloc: an error's text
            }
            ConstOf::Promoted(promoted) => blaze_query::db::attach(db, || {
                // alloc: an error's text
                format!(
                    "the promoted constant {:?} of {}",
                    promoted.expr,
                    body_path(db, promoted.owner)
                )
            }),
            // alloc: an error's text
            ConstOf::InPlace(Some(owner)) => {
                format!("a constant value in {}", body_path(db, owner))
            }
            ConstOf::InPlace(None) => "a constant value of no body".to_owned(), // alloc: an error's text
        }
    }
}

/// The error of a promoted constant whose MIR does not build, naming it and the body it is of.
pub(crate) fn no_promoted_mir(
    db: &Db,
    promoted: Promoted<'_>,
    error: impl std::fmt::Debug,
) -> CodegenError {
    // alloc: the error's message
    CodegenError::Refused(blaze_query::db::attach(db, || {
        format!("{} has no MIR: {error:?}", ConstOf::Promoted(promoted).describe(db))
    }))
}

/// The locals of `body` that hold a promoted constant, its own and its inlined callees'.
fn promoted_locals<'a>(body: &'a MirBody<'_>) -> impl Iterator<Item = LocalId> + 'a {
    let own = body.promoted_locals.iter().map(|it| it.0);
    own.chain(body.inlined.promoted.iter().map(|it| it.local))
}

fn is_promoted(body: &MirBody<'_>, local: LocalId) -> bool {
    promoted_locals(body).any(|it| it == local)
}
/// Field accesses can be unaligned (packed structs, chunked copies).
const MEM: MemFlagsData = MemFlagsData::new().with_notrap();

/// The function of AArch64's crypto instruction `llvm.{name}` names, whose operands are in
/// LLVM's order: the first is the one the instruction writes over.
fn neon_crypto(name: &str) -> String {
    let op = name.trim_start_matches("aarch64.crypto.");
    let (extension, operands) = match op {
        "aese" | "aesd" => ("aes", "v0.16b, v1.16b"),
        "aesmc" | "aesimc" => ("aes", "v0.16b, v0.16b"),
        "sha256h" | "sha256h2" => ("sha2", "q0, q1, v2.4s"),
        "sha256su1" | "sha1su0" => ("sha2", "v0.4s, v1.4s, v2.4s"),
        "sha256su0" | "sha1su1" => ("sha2", "v0.4s, v1.4s"),
        "sha512h" | "sha512h2" => ("sha3", "q0, q1, v2.2d"),
        "sha512su1" => ("sha3", "v0.2d, v1.2d, v2.2d"),
        _ => ("sha3", "v0.2d, v1.2d"),
    };
    blaze_mem::scratch::with_string(|instruction| {
        use std::fmt::Write as _;
        _ = write!(instruction, "{op} {operands}");
        asm::neon_instruction(extension, instruction)
    })
}

/// The vector of 16 bytes a SIMD operation on `count` integer lanes of `lane` is done in, and
/// how many of them there are: one Cranelift and LLVM both have for every target.
fn simd_chunks(lane: Type, count: u64) -> Option<(Type, i32)> {
    let bytes = u64::from(lane.bytes()) * count;
    if !lane.is_int() || lane.bits() > 64 || bytes == 0 || bytes % 16 != 0 {
        return None;
    }
    Some((lane.by(16 / lane.bytes())?, i32::try_from(bytes / 16).ok()?))
}

impl<'cx, 'db> FunctionCx<'cx, 'db> {
    /// Whether pointers to `ty` carry metadata: slices, `str`, trait objects and structs
    /// ending in one.
    /// Where the `Box` of type `boxed` at `addr` points, and the pointee's metadata when it is
    /// unsized. Beside an allocator that has a size, the pointer is a field anywhere in it.
    fn box_pointer(
        &mut self,
        addr: Value,
        boxed: Ty<'db>,
        pointee: Ty<'db>,
    ) -> Result<(Value, Option<Value>)> {
        let at = self.layout(boxed)?.fields.offset(0).bytes() as i32;
        let meta = self.is_unsized(pointee).then(|| {
            self.builder.ins().load(
                self.cg.target.pointer(),
                MEM,
                addr,
                at + self.cg.target.meta_at(),
            )
        });
        Ok((self.builder.ins().load(self.cg.target.pointer(), MEM, addr, at), meta))
    }

    fn is_unsized(&self, ty: Ty<'db>) -> bool {
        match ty.kind() {
            TyKind::Slice(_) | TyKind::Str | TyKind::Dynamic(..) => true,
            TyKind::Adt(..) | TyKind::Tuple(_) => {
                self.layout(ty).is_ok_and(|layout| layout.is_unsized())
            }
            _ => false,
        }
    }

    /// The last field of a struct or tuple, which is the only one that can be unsized.
    fn last_field(&self, ty: Ty<'db>) -> Result<(usize, Ty<'db>)> {
        match ty.kind() {
            TyKind::Tuple(tys) if !tys.is_empty() => {
                Ok((tys.len() - 1, tys.iter().last().expect("non-empty tuple")))
            }
            TyKind::Adt(adt_def, subst) => {
                let AdtId::StructId(id) = adt_def.def_id() else {
                    unsupported!("unsized {ty:?}");
                };
                let field_types = self.cg.db.field_types(id.into());
                let Some((field, field_ty)) = field_types.iter().last() else {
                    unsupported!("unsized {ty:?} without fields");
                };
                let field_ty = field_ty.ty().instantiate(self.interner, subst).skip_norm_wip();
                Ok((u32::from(field.into_raw()) as usize, self.normalize(field_ty)))
            }
            _ => unsupported!("unsized {ty:?}"),
        }
    }

    /// `packed(N)` of a struct, which caps the alignment of its fields.
    fn pack(&self, ty: Ty<'db>) -> Option<u64> {
        let TyKind::Adt(adt_def, _) = ty.kind() else { return None };
        let repr = hir_def::attrs::AttrFlags::repr(self.cg.db, adt_def.def_id())?;
        repr.pack.map(|pack| pack.bytes())
    }

    /// Size and alignment of the value behind a pointer with metadata `meta`, as
    /// `size_and_align_of_dst` in rustc's backends.
    fn size_and_align_of_val(
        &mut self,
        ty: Ty<'db>,
        meta: Option<Value>,
    ) -> Result<(Value, Value)> {
        let layout = self.layout(ty)?;
        let align = layout.align.abi.bytes() as i64;
        let meta = match meta {
            Some(meta) if self.is_unsized(ty) => meta,
            _ => {
                let size =
                    self.builder.ins().iconst(self.cg.target.pointer(), layout.size.bytes() as i64);
                return Ok((size, self.builder.ins().iconst(self.cg.target.pointer(), align)));
            }
        };
        match ty.kind() {
            TyKind::Str => Ok((meta, self.builder.ins().iconst(self.cg.target.pointer(), 1))),
            TyKind::Slice(elem) => {
                let elem = self.layout(elem)?;
                let size = self.builder.ins().imul_imm(meta, elem.size.bytes() as i64);
                Ok((
                    size,
                    self.builder
                        .ins()
                        .iconst(self.cg.target.pointer(), elem.align.abi.bytes() as i64),
                ))
            }
            // Size and align are the vtable's second and third words.
            TyKind::Dynamic(..) => Ok((
                self.builder.ins().load(
                    self.cg.target.pointer(),
                    MEM,
                    meta,
                    self.cg.target.meta_at(),
                ),
                self.builder.ins().load(
                    self.cg.target.pointer(),
                    MEM,
                    meta,
                    2 * self.cg.target.meta_at(),
                ),
            )),
            _ => {
                let (field, field_ty) = self.last_field(ty)?;
                let offset = layout.fields.offset(field).bytes() as i64;
                let (tail_size, mut tail_align) =
                    self.size_and_align_of_val(field_ty, Some(meta))?;
                if let Some(pack) = self.pack(ty) {
                    let pack = self.builder.ins().iconst(self.cg.target.pointer(), pack as i64);
                    tail_align = self.builder.ins().umin(tail_align, pack);
                }
                let sized_align = self.builder.ins().iconst(self.cg.target.pointer(), align);
                let full_align = self.builder.ins().umax(tail_align, sized_align);
                let offset = self.builder.ins().iconst(self.cg.target.pointer(), offset);
                let offset = self.emit_align_up(offset, tail_align);
                let size = self.builder.ins().iadd(offset, tail_size);
                Ok((self.emit_align_up(size, full_align), full_align))
            }
        }
    }

    /// Where to drop a field of `container`: destructors take an aligned `&mut self`, so
    /// a field a packed struct leaves misaligned is moved out first, as rustc does.
    fn aligned_for_drop(
        &mut self,
        addr: Value,
        field: Ty<'db>,
        container: Ty<'db>,
    ) -> Result<Value> {
        let Some(pack) = self.pack(container) else { return Ok(addr) };
        let field = self.normalize(field);
        let layout = self.layout(field)?;
        if layout.align.abi.bytes() <= pack || self.glue_of(field) == DropGlue::None {
            return Ok(addr);
        }
        let slot = self.stack_slot(&layout);
        self.copy(slot, addr, layout.size);
        Ok(slot)
    }

    fn emit_align_up(&mut self, value: Value, align: Value) -> Value {
        let mask = self.builder.ins().iadd_imm(align, -1);
        let value = self.builder.ins().iadd(value, mask);
        self.builder.ins().band_not(value, mask)
    }

    /// Address of a field whose type is unsized. A trait object's alignment, and so the
    /// field's offset, is only known from the vtable.
    fn unsized_field_addr(
        &mut self,
        addr: Value,
        meta: Option<Value>,
        container: Ty<'db>,
        offset: Size,
        field_ty: Ty<'db>,
    ) -> Result<Value> {
        let field_ty = self.normalize(field_ty);
        let mut tail = field_ty;
        while matches!(tail.kind(), TyKind::Adt(..) | TyKind::Tuple(_)) && self.is_unsized(tail) {
            tail = self.last_field(tail)?.1;
        }
        let (Some(meta), TyKind::Dynamic(..)) = (meta, tail.kind()) else {
            return Ok(self.builder.ins().iadd_imm(addr, offset.bytes() as i64));
        };
        let (_, mut align) = self.size_and_align_of_val(field_ty, Some(meta))?;
        if let Some(pack) = self.pack(container) {
            let pack = self.builder.ins().iconst(self.cg.target.pointer(), pack as i64);
            align = self.builder.ins().umin(align, pack);
        }
        let offset = self.builder.ins().iconst(self.cg.target.pointer(), offset.bytes() as i64);
        let offset = self.emit_align_up(offset, align);
        Ok(self.builder.ins().iadd(addr, offset))
    }

    fn glue_of(&self, ty: Ty<'db>) -> DropGlue {
        if let Some(glue) = self.glue.borrow().get(&ty) {
            return *glue;
        }
        let glue = has_drop_glue(&self.infcx, ty, self.env.param_env);
        self.glue.borrow_mut().insert(ty, glue);
        glue
    }

    fn layout(&self, ty: Ty<'db>) -> Result<&'cx Layout> {
        if let Some(&layout) = self.layouts.of.borrow().get(&ty) {
            return Ok(layout);
        }
        let layout = self
            .cg
            .db
            .layout_of_ty(ty.store(), self.env.store())
            // alloc: the error's message
            .map_err(|e| CodegenError::Frontend(format!("layout of {ty:?}: {e:?}")))?;
        if let Some(log) = &self.cg.layouts {
            log.borrow_mut().entry(ty).or_insert((layout.size.bytes(), layout.align.abi.bytes()));
            if let Some(line) = coroutine_layout_line(self.cg.db, ty, &layout) {
                let (size, align) = (layout.size.bytes(), layout.align.abi.bytes());
                self.cg.coroutine_layouts.borrow_mut().push((line, size, align));
            }
        }
        // What is laid out at the least vector length is stored for the largest.
        let layout: &'cx Layout = sve::storage(layout.get());
        Ok(*self.layouts.of.borrow_mut().entry(ty).or_insert(layout))
    }

    /// The shape of a scalable vector type, `None` for any other.
    fn scalable(&self, ty: Ty<'db>) -> Result<Option<sve::Shape>> {
        Ok(sve::Shape::of(&*self.layout(ty)?))
    }

    fn scalar(&self, scalar: Scalar) -> Result<AbiScalar> {
        Ok(match scalar.primitive() {
            Primitive::Int(int, signed) => AbiScalar {
                ty: match int {
                    Integer::I8 => types::I8,
                    Integer::I16 => types::I16,
                    Integer::I32 => types::I32,
                    Integer::I64 => types::I64,
                    Integer::I128 => types::I128,
                },
                signed,
            },
            Primitive::Pointer(_) => AbiScalar { ty: self.cg.target.pointer(), signed: false },
            Primitive::Float(rustc_abi::Float::F32) => AbiScalar { ty: types::F32, signed: true },
            Primitive::Float(rustc_abi::Float::F64) => AbiScalar { ty: types::F64, signed: true },
            // `f16b` has no arithmetic in Rust, only its bits moved, and LLVM passes it where it
            // passes an `f16`: in Cranelift's `F16` it is a 16-bit value of a float register.
            // x86-64's LLVM rounds it on entry and between blocks, which `receive_bfloat` and
            // `check_bfloat_blocks` follow.
            Primitive::Float(rustc_abi::Float::F16 | rustc_abi::Float::F16B)
                if f16_f128::soft_floats(self.cg.target) =>
            {
                AbiScalar { ty: types::F16, signed: true }
            }
            Primitive::Float(rustc_abi::Float::F128) if f16_f128::soft_floats(self.cg.target) => {
                AbiScalar { ty: types::F128, signed: true }
            }
            Primitive::Float(_) => unsupported!("f16, f16b, f128 and ppcf128"),
        })
    }

    fn scalar_of(&self, ty: Ty<'db>) -> Result<AbiScalar> {
        match self.layout(ty)?.backend_repr {
            BackendRepr::Scalar(scalar) => self.scalar(scalar),
            _ => unsupported!("expected scalar, got {ty:?}"),
        }
    }

    fn pass_mode(&self, layout: &Layout, is_ret: bool) -> Result<PassMode> {
        if let BackendRepr::Memory { sized: false } = layout.backend_repr {
            // rustc's `make_indirect` of an unsized argument; nothing returns one.
            if is_ret {
                unsupported!("an unsized value returned");
            }
            return Ok(PassMode::IndirectUnsized);
        }
        if layout.is_zst() {
            return Ok(PassMode::Ignore);
        }
        // rustc passes a scalable vector in Z and P registers, which Cranelift has no type
        // for: between blaze's own functions it goes by reference. Where foreign code would
        // pass it, `fn_abi` and `callee` refuse.
        if let BackendRepr::SimdScalableVector { .. } = layout.backend_repr {
            return Ok(PassMode::Indirect);
        }
        // rustc returns in memory what is more than two registers hold.
        let wide = is_ret && layout.size.bytes() > 2 * self.cg.target.word() as u64;
        Ok(match layout.backend_repr {
            BackendRepr::Scalar(_) if wide => PassMode::Indirect,
            BackendRepr::Scalar(scalar) => PassMode::Direct(self.scalar(scalar)?),
            BackendRepr::ScalarPair { a, b, b_offset: offset } => {
                let (a, b) = (self.scalar(a)?, self.scalar(b)?);
                // A pair the Rust ABI returns is two values on every target, as rustc's
                // `PassMode::Pair` is. On WebAssembly, where a function returns one value,
                // the wasm lowering writes them through a pointer that is the first
                // parameter, at the offsets of LLVM's struct of the two (`wasm::Returned`, the
                // one home of LLVM's `canLowerReturn`), as LLVM demotes rustc's pair: the
                // same for every blaze, whatever it was built with.
                if is_ret && (wide || a.ty == types::I128 || b.ty == types::I128) {
                    // More than the two return registers hold, so LLVM returns it in memory.
                    PassMode::Indirect
                } else {
                    PassMode::Pair(a, b, offset)
                }
            }
            BackendRepr::Memory { sized: true } => {
                let max = self.cg.target.word() as u64;
                match layout.size.bytes() {
                    size if size > max => PassMode::Indirect,
                    1 => PassMode::Cast(types::I8),
                    2 => PassMode::Cast(types::I16),
                    // An integer of exactly the aggregate's size; the bits above it are undefined.
                    3 | 4 => PassMode::Cast(types::I32),
                    _ => PassMode::Cast(types::I64),
                }
            }
            BackendRepr::SimdVector { element, count } => {
                self.rust_simd_pass_mode(layout, element, count, is_ret)?
            }
            _ => unsupported!("abi of {:?}", layout.backend_repr),
        })
    }

    /// How the Rust ABI passes a SIMD vector. Mirrors rustc's `adjust_for_rust_abi`
    /// (`can_pass_simd_directly`): on x86-64 and AArch64, whose every target has the vector
    /// registers, a vector of at most 128 bits is an immediate of LLVM's, in what register LLVM
    /// gives that vector type; any other goes in memory, so that callers and callees built with
    /// different target features agree.
    fn rust_simd_pass_mode(
        &self,
        layout: &Layout,
        element: Scalar,
        count: rustc_abi::BackendLaneCount,
        is_ret: bool,
    ) -> Result<PassMode> {
        let target = &self.cg.target;
        let size = layout.size.bytes();
        if !matches!(target.arch, Arch::X86_64 | Arch::Aarch64) || size > 16 {
            return Ok(PassMode::Indirect);
        }
        let lanes = count.as_u64();
        if target.arch == Arch::Aarch64 && size < 8 {
            // AArch64's LLVM makes a vector of fewer than 64 bits one of 64 (`v1i64` and
            // `v1f64`, NEON's `*x1_t` of 64-bit lanes, are in a D register already, as the
            // 64-bit vectors below are). One lane, or floats, it widens with more lanes
            // after them (`<1 x i8>` as `<8 x i8>`, `<2 x half>` as `<4 x half>`): their
            // bytes are the low ones of the register, as an `f32` is in its S register. The
            // lanes of a vector of integers it promotes to as many lanes of 64 bits in all
            // (`<4 x i8>` as `<4 x i16>`, `<2 x i8>` as `<2 x i32>`).
            let float = matches!(element.primitive(), Primitive::Float(_));
            return Ok(match lanes {
                1 if size == 4 => PassMode::Cast(types::F32),
                1 => PassMode::Cast(types::F64),
                _ if !lanes.is_power_of_two() => {
                    unsupported!("a SIMD vector of {lanes} lanes by value in the Rust ABI")
                }
                _ if float => PassMode::Cast(types::F64),
                _ => PassMode::Spread {
                    lane: Type::int((size / lanes * 8) as u16).expect("a lane of 8 or 16 bits"),
                    stride: (8 / lanes) as u8,
                },
            });
        }
        if lanes == 1 {
            let lane = self.scalar(element)?.ty;
            match target.arch {
                // x86-64's LLVM makes a vector of one lane its scalar, in that scalar's
                // register under either convention: an integer's or pointer's in a general
                // register, a float's in an XMM register, as the lane alone would be.
                Arch::X86_64 if lane != types::I128 => return Ok(PassMode::Cast(lane)),
                Arch::Aarch64 if size == 8 => (),
                _ => unsupported!("a SIMD vector of one {lane:?} lane by value in the Rust ABI"),
            }
        }
        if target.call_conv == isa::CallConv::WindowsFastcall {
            // LLVM's Windows convention passes an argument of vector type by reference and
            // returns it in `xmm0`, widened to 128 bits.
            return Ok(if is_ret { PassMode::Cast(types::I64X2) } else { PassMode::Indirect });
        }
        Ok(match (target.arch, size) {
            // A whole vector register, whatever its lanes.
            (_, 16) => PassMode::Cast(types::I64X2),
            // x86-64 widens a shorter vector to 128 bits, its lanes at the bottom.
            (Arch::X86_64, _) => PassMode::Cast(types::I64X2),
            // AArch64 has a 64-bit vector in the low half of a vector register, as an `f64`.
            (Arch::Aarch64, 8) => PassMode::Cast(types::F64),
            _ => unsupported!("a SIMD vector of {size} bytes by value in the Rust ABI"),
        })
    }

    /// How a call with these types passes its values. Mirrors rustc: `adjust_for_rust_abi`
    /// for Rust's own conventions, and `callconv/x86_64.rs` or `callconv/aarch64.rs` for C's.
    fn fn_abi(
        &self,
        abi: ExternAbi,
        ret: Ty<'db>,
        args: impl IntoIterator<Item = Ty<'db>>,
    ) -> Result<FnAbi> {
        let args = smallvec::SmallVec::<[Ty<'db>; 8]>::from_iter(args);
        if !abi.is_rustic_abi() {
            for &ty in args.iter().chain([&ret]) {
                if self.scalable(ty)?.is_some() {
                    unsupported!(
                        "a scalable vector in the {abi:?} ABI, which rustc passes in SVE registers"
                    );
                }
            }
        }
        if abi.is_rustic_abi() {
            let ret = self.pass_mode(&*self.layout(ret)?, true)?;
            let args = args
                .into_iter()
                .map(|ty| self.pass_mode(&*self.layout(ty)?, false))
                .collect::<Result<_>>()?;
            let mut abi = FnAbi { ret, args };
            self.wide_ints_on_stack(&mut abi)?;
            self.wide_ints_after_registers(&mut abi)?;
            return Ok(abi);
        }
        if args.iter().any(|&ty| self.is_unsized(ty)) {
            unsupported!("an unsized value passed in calling convention {abi:?}");
        }
        // rustc takes `extern "custom"` only for a naked function of no arguments and no
        // result, which nothing calls from Rust: its body is the whole of it, and no value
        // crosses the boundary.
        if abi == ExternAbi::Custom {
            if !args.is_empty() || !ret.is_unit() {
                unsupported!("calling convention {abi:?} with arguments or a result");
            }
            // alloc: empty, allocates nothing
            return Ok(FnAbi { ret: PassMode::Ignore, args: Vec::new() });
        }
        if self.cg.target.call_conv == isa::CallConv::WindowsFastcall {
            if matches!(abi, ExternAbi::SysV64 { .. }) {
                unsupported!("calling convention {abi:?}");
            }
            let ret = self.win64_pass_mode(ret, true)?;
            let args = args
                .into_iter()
                .map(|ty| self.win64_pass_mode(ty, false))
                .collect::<Result<_>>()?;
            return Ok(FnAbi { ret, args });
        }
        if self.cg.target.is_wasm() {
            return self.wasm_c_abi(ret, args);
        }
        let arch = self.cg.target.arch;
        match (abi, arch) {
            (ExternAbi::C { .. } | ExternAbi::System { .. }, Arch::X86_64)
            | (ExternAbi::SysV64 { .. }, Arch::X86_64) => {
                let mut abi = self.sysv_x86_64_abi(ret, args)?;
                self.wide_ints_on_stack(&mut abi)?;
                Ok(abi)
            }
            (ExternAbi::C { .. } | ExternAbi::System { .. }, Arch::Aarch64) => {
                self.aapcs64_abi(ret, args)
            }
            _ => unsupported!("calling convention {abi:?}"),
        }
    }

    /// Passes a 128-bit integer that finds one integer register left as memory on the stack.
    ///
    /// By the System V x86-64 convention such an integer goes on the stack, and LLVM (so
    /// rustc) gives the register that is left to the next integer argument, where Cranelift
    /// gives it to nobody. As memory on the stack the integer takes no register in either,
    /// so Cranelift places every argument where rustc does, and where LLVM does.
    fn wide_ints_on_stack(&self, abi: &mut FnAbi) -> Result<()> {
        let target = &self.cg.target;
        if target.arch != Arch::X86_64 || target.call_conv != isa::CallConv::SystemV {
            return Ok(());
        }
        // alloc: empty until a 128-bit integer is found, then reused for every argument
        let mut these = Vec::new();
        while let Some((at, stack)) = lone_register_i128(&self.signature(&abi.args, abi.ret).params)
        {
            // The argument the parameter is of, and which of the argument's parameters it is.
            let mut first = usize::from(matches!(abi.ret, PassMode::Indirect));
            let arg = abi
                .args
                .iter()
                .position(|&mode| {
                    these.clear();
                    self.push_abi_params(mode, &mut these);
                    if first + these.len() > at {
                        return true;
                    }
                    first += these.len();
                    false
                })
                .expect("every parameter comes from an argument");
            let padded = stack % 16 != 0;
            abi.args[arg] = match abi.args[arg] {
                PassMode::Direct(_) => PassMode::OnStack { size: 16, padded },
                PassMode::Pair(a, b, offset) => {
                    let mut spilled = [None; 2];
                    spilled[usize::from(at > first)] = Some(padded);
                    PassMode::SpilledPair { a, b, offset, spilled }
                }
                // Both halves are 128-bit integers, and the second finds the register the
                // first left.
                PassMode::SpilledPair { a, b, offset, spilled: [half @ Some(_), None] } => {
                    PassMode::SpilledPair { a, b, offset, spilled: [half, Some(padded)] }
                }
                mode => unreachable!("a 128-bit integer passed as {mode:?}"),
            };
        }
        Ok(())
    }

    /// Has the integer registers that a 128-bit integer going to the stack leaves go unused.
    ///
    /// By AAPCS64 such an integer takes every integer register that is left out of use, and
    /// LLVM (so rustc) does so, where Cranelift gives them to the integer arguments after it.
    /// Passed to nobody first, they are where neither backend puts anything else.
    fn wide_ints_after_registers(&self, abi: &mut FnAbi) -> Result<()> {
        let target = &self.cg.target;
        if target.arch != Arch::Aarch64 {
            return Ok(());
        }
        let even = if target.call_conv == isa::CallConv::AppleAarch64 { 1 } else { 2 };
        // alloc: empty until a 128-bit integer is found, then reused for every argument
        let mut these = Vec::new();
        while let Some((at, skipped)) =
            stranded_i128(&self.signature(&abi.args, abi.ret).params, even)
        {
            let mut first = usize::from(matches!(abi.ret, PassMode::Indirect));
            let arg = abi
                .args
                .iter()
                .position(|&mode| {
                    these.clear();
                    self.push_abi_params(mode, &mut these);
                    if first + these.len() > at {
                        return true;
                    }
                    first += these.len();
                    false
                })
                .expect("every parameter comes from an argument");
            abi.args[arg] = match abi.args[arg] {
                PassMode::Direct(_) => PassMode::Uniform { unit: types::I128, count: 1, skipped },
                PassMode::Pair(a, b, offset) => {
                    let mut gaps = [0; 2];
                    gaps[usize::from(at > first)] = skipped;
                    PassMode::SkippingPair { a, b, offset, skipped: gaps }
                }
                mode => unreachable!("a 128-bit integer passed as {mode:?}"),
            };
        }
        Ok(())
    }

    /// The C convention of the System V x86-64 psABI, as rustc's `callconv/x86_64.rs` has it.
    fn sysv_x86_64_abi(
        &self,
        ret: Ty<'db>,
        args: impl IntoIterator<Item = Ty<'db>>,
    ) -> Result<FnAbi> {
        let (mut int_regs, mut sse_regs) = (6usize, 8usize);
        let ret = match self.c_pass_mode(ret)? {
            PassMode::OnStack { .. } => {
                // The pointer to the return value takes the first integer register.
                int_regs -= 1;
                PassMode::Indirect
            }
            mode => mode,
        };
        // How many bytes of the stack the arguments before this one take, as Cranelift
        // places them.
        let mut stack = 0u32;
        let args = args.into_iter();
        let mut modes = Vec::with_capacity(args.size_hint().0);
        for ty in args {
            if self.decays_in_c(ty)? {
                int_regs = int_regs.saturating_sub(1);
                modes.push(PassMode::Indirect);
                continue;
            }
            let mode = self.c_pass_mode(ty)?;
            let (ints, sses) = match mode {
                PassMode::Ignore | PassMode::Indirect | PassMode::OnStack { .. } => (0, 0),
                PassMode::Direct(s) => self.count_regs(&[s.ty]),
                PassMode::Pair(a, b, _) => self.count_regs(&[a.ty, b.ty]),
                PassMode::Cast(ty) => self.count_regs(&[ty]),
                PassMode::CastPair(a, b) => self.count_regs(&[a, b]),
                PassMode::Uniform { .. } => unreachable!("x86-64 has no uniform aggregate"),
                PassMode::Spread { .. } => unreachable!("only the Rust ABI spreads lanes"),
                PassMode::SpilledPair { .. } => unreachable!("only the stack spills a pair"),
                PassMode::SkippingPair { .. } => unreachable!("only AAPCS64 skips registers"),
                PassMode::IndirectUnsized => {
                    unreachable!("only the Rust ABI passes unsized values")
                }
            };
            let aggregate = matches!(mode, PassMode::Cast(_) | PassMode::CastPair(..));
            let in_registers = match (int_regs.checked_sub(ints), sse_regs.checked_sub(sses)) {
                (Some(int_left), Some(sse_left)) if !matches!(mode, PassMode::OnStack { .. }) => {
                    (int_regs, sse_regs) = (int_left, sse_left);
                    true
                }
                _ => false,
            };
            modes.push(match mode {
                _ if in_registers => mode,
                // Out of registers, or too large for them: an aggregate goes to memory
                // whole, at a multiple of its alignment.
                _ if aggregate || matches!(mode, PassMode::OnStack { .. }) => {
                    let layout = self.layout(ty)?;
                    let size = layout.size.bytes().next_multiple_of(8) as u32;
                    let padded = match layout.align.abi.bytes() {
                        ..=8 => false,
                        16 => stack % 16 != 0,
                        // FIXME: the stack is at a multiple of sixteen and no more when a
                        // function is called.
                        _ => unsupported!("an argument in memory aligned to more than 16 bytes"),
                    };
                    stack += size + if padded { 8 } else { 0 };
                    PassMode::OnStack { size, padded }
                }
                // Cranelift moves anything else to the stack by itself: eight bytes for
                // each value, and a 128-bit integer or float whole at a multiple of sixteen.
                PassMode::Direct(AbiScalar { ty: types::I128 | types::F128, .. }) => {
                    stack = stack.next_multiple_of(16) + 16;
                    mode
                }
                PassMode::Pair(..) => {
                    stack += 16;
                    mode
                }
                _ => {
                    stack += 8;
                    mode
                }
            });
        }
        Ok(FnAbi { ret, args: modes })
    }

    /// The C convention of AArch64 (AAPCS64), as rustc's `callconv/aarch64.rs` has it.
    ///
    /// Cranelift places scalars the way the convention does, so what is decided here is
    /// what an aggregate turns into, and which registers it leaves unused: Cranelift hands
    /// out registers one value at a time and knows nothing of a value that takes several
    /// or none.
    fn aapcs64_abi(&self, ret: Ty<'db>, args: impl IntoIterator<Item = Ty<'db>>) -> Result<FnAbi> {
        const REGS: u8 = 8;
        let ret = match self.aapcs64_class(ret)? {
            Aapcs64::Ignore => PassMode::Ignore,
            Aapcs64::Scalar(scalar) => PassMode::Direct(scalar),
            Aapcs64::Floats { unit, count } => PassMode::Uniform { unit, count, skipped: 0 },
            Aapcs64::Ints { size, .. } => Self::aapcs64_ints(size, 0),
            // Its address comes in a register of its own, which is not an argument's.
            Aapcs64::Memory => PassMode::Indirect,
        };
        // What a value aligned to sixteen bytes starts at a multiple of, counted in
        // registers: Apple's convention starts it at whichever register is next.
        let even = match self.cg.target.call_conv {
            isa::CallConv::AppleAarch64 => 1,
            _ => 2,
        };
        let (mut ints, mut floats) = (0u8, 0u8);
        let args = args.into_iter();
        let mut modes = Vec::with_capacity(args.size_hint().0);
        for ty in args {
            let class = match self.decays_in_c(ty)? {
                true => Aapcs64::Memory,
                false => self.aapcs64_class(ty)?,
            };
            modes.push(match class {
                Aapcs64::Ignore => PassMode::Ignore,
                Aapcs64::Scalar(scalar) => {
                    let (used, wide) = match scalar.ty {
                        ty if ty.is_float() => (&mut floats, false),
                        ty => (&mut ints, ty == types::I128),
                    };
                    if wide {
                        // Cranelift starts it at an even register, or puts it on the stack
                        // whole. The registers it did not fit in stay free there, where
                        // the convention has them out of use.
                        let skipped = REGS - *used;
                        if *used + *used % even + 2 <= REGS {
                            *used += *used % even + 2;
                            PassMode::Direct(scalar)
                        } else {
                            *used = REGS;
                            PassMode::Uniform { unit: types::I128, count: 1, skipped }
                        }
                    } else {
                        *used = (*used + 1).min(REGS);
                        PassMode::Direct(scalar)
                    }
                }
                Aapcs64::Floats { unit, count } if floats + count <= REGS => {
                    floats += count;
                    PassMode::Uniform { unit, count, skipped: 0 }
                }
                Aapcs64::Floats { unit, count } => {
                    let skipped = REGS - floats;
                    floats = REGS;
                    let size = u64::from(unit.bytes()) * u64::from(count);
                    PassMode::Uniform { unit: types::F64, count: size.div_ceil(8) as u8, skipped }
                }
                Aapcs64::Ints { size, align } => {
                    let count = size.div_ceil(8) as u8;
                    let odd = if align > 8 { ints % even } else { 0 };
                    if ints + odd + count <= REGS {
                        ints += odd + count;
                        Self::aapcs64_ints(size, odd)
                    } else {
                        let skipped = REGS - ints;
                        ints = REGS;
                        // On the stack, one aligned to sixteen bytes is at a multiple of
                        // sixteen, as a 128-bit integer is.
                        match align > 8 {
                            true => PassMode::Uniform { unit: types::I128, count: 1, skipped },
                            false => PassMode::Uniform { unit: types::I64, count, skipped },
                        }
                    }
                }
                Aapcs64::Memory => {
                    ints = (ints + 1).min(REGS);
                    PassMode::Indirect
                }
            });
        }
        Ok(FnAbi { ret, args: modes })
    }

    /// An aggregate of at most sixteen bytes in integer registers, `skipped` of them unused.
    fn aapcs64_ints(size: u64, skipped: u8) -> PassMode {
        match size {
            // An integer of exactly the aggregate's size; the bits above it are undefined.
            1 if skipped == 0 => PassMode::Cast(types::I8),
            2 if skipped == 0 => PassMode::Cast(types::I16),
            3 | 4 if skipped == 0 => PassMode::Cast(types::I32),
            5..=8 if skipped == 0 => PassMode::Cast(types::I64),
            _ => PassMode::Uniform { unit: types::I64, count: size.div_ceil(8) as u8, skipped },
        }
    }

    /// What AAPCS64 makes of a value before it looks for registers.
    fn aapcs64_class(&self, ty: Ty<'db>) -> Result<Aapcs64> {
        let layout = self.layout(ty)?;
        if layout.is_zst() {
            return Ok(Aapcs64::Ignore);
        }
        match layout.backend_repr {
            BackendRepr::Scalar(scalar) => return Ok(Aapcs64::Scalar(self.scalar(scalar)?)),
            BackendRepr::SimdVector { .. } => unsupported!("vector in the C ABI"),
            _ => (),
        }
        let size = layout.size.bytes();
        let mut leaves = smallvec::SmallVec::new();
        self.scalar_leaves(ty, 0, &mut leaves)?;
        // The members of a union lie over each other.
        blaze_ids::radix::sort_copy_by_key(&mut leaves, |&(offset, _)| offset);
        leaves.dedup();
        // A homogeneous floating-point aggregate: one to four floats of one type and
        // nothing else, which go in as many vector registers.
        if let Some(&(_, first)) = leaves.first()
            && let Primitive::Float(_) = first.primitive()
            && leaves.len() <= 4
        {
            let unit = self.scalar(first)?.ty;
            if f16_f128::soft(unit) {
                unsupported!("a homogeneous aggregate of {unit} in the C ABI");
            }
            let step = u64::from(unit.bytes());
            let packed = leaves.iter().zip(0..).all(|(&(offset, scalar), index)| {
                offset == index * step && scalar.primitive() == first.primitive()
            });
            if packed && leaves.len() as u64 * step == size {
                return Ok(Aapcs64::Floats { unit, count: leaves.len() as u8 });
            }
        }
        if size > 16 {
            return Ok(Aapcs64::Memory);
        }
        // rustc has one aligned to sixteen bytes in units of 128 bits, which start at an
        // even register and at a multiple of sixteen on the stack. AAPCS64 goes by the
        // alignment the fields ask for, and Apple by the one `repr(align)` may have raised.
        let align = match self.cg.target.call_conv {
            isa::CallConv::AppleAarch64 => layout.align.abi,
            _ => layout.unadjusted_abi_align,
        };
        Ok(Aapcs64::Ints { size, align: align.bytes() })
    }

    /// Every scalar a value of the C ABI is made of, with where in the value it is.
    fn scalar_leaves(
        &self,
        ty: Ty<'db>,
        offset: u64,
        leaves: &mut smallvec::SmallVec<[(u64, Scalar); 8]>,
    ) -> Result<()> {
        let layout = self.layout(ty)?;
        if layout.is_zst() {
            return Ok(());
        }
        match layout.backend_repr {
            BackendRepr::Scalar(a) => leaves.push((offset, a)),
            BackendRepr::ScalarPair { a, b, b_offset } => {
                leaves.extend([(offset, a), (offset + b_offset.bytes(), b)]);
            }
            BackendRepr::Memory { sized: true } => match ty.kind() {
                TyKind::Tuple(tys) => {
                    for (i, ty) in tys.iter().enumerate() {
                        self.scalar_leaves(ty, offset + layout.fields.offset(i).bytes(), leaves)?;
                    }
                }
                TyKind::Array(element, _) => {
                    let stride = self.layout(element)?.size.bytes();
                    for i in 0..layout.fields.count() as u64 {
                        // More than a register class holds: nothing asks what the rest is.
                        if leaves.len() > 16 {
                            break;
                        }
                        self.scalar_leaves(element, offset + i * stride, leaves)?;
                    }
                }
                TyKind::Adt(def, subst) => {
                    let variant: VariantId = match def.def_id() {
                        AdtId::StructId(id) => id.into(),
                        AdtId::UnionId(id) => id.into(),
                        AdtId::EnumId(_) => unsupported!("enum {ty:?} in the C ABI"),
                    };
                    for (field, ty) in self.cg.db.field_types(variant).iter() {
                        let ty = ty.ty().instantiate(self.interner, subst).skip_norm_wip();
                        let field = layout.fields.offset(u32::from(field.into_raw()) as usize);
                        self.scalar_leaves(ty, offset + field.bytes(), leaves)?;
                    }
                }
                _ => unsupported!("{ty:?} in the C ABI"),
            },
            _ => unsupported!("{ty:?} in the C ABI"),
        }
        Ok(())
    }

    /// The C ABI of wasm, as rustc's `callconv/wasm.rs`: a scalar is a value, an aggregate
    /// that is one scalar and nothing else (`[f32; 1]`, a `repr(C)` struct of a `u32` and
    /// fields of no size) is that scalar, and anything else is in memory, an argument as the
    /// address of a copy.
    fn wasm_c_abi(&self, ret: Ty<'db>, args: impl IntoIterator<Item = Ty<'db>>) -> Result<FnAbi> {
        let mode = |ty: Ty<'db>, is_ret: bool| -> Result<PassMode> {
            let layout = self.layout(ty)?;
            Ok(match layout.backend_repr {
                _ if layout.is_zst() => PassMode::Ignore,
                BackendRepr::Scalar(scalar) => PassMode::Direct(self.scalar(scalar)?),
                // A vector is no aggregate to the convention: it goes as a `v128`.
                BackendRepr::SimdVector { .. } if layout.size.bytes() == 16 && self.simd128() => {
                    PassMode::Cast(types::I64X2)
                }
                BackendRepr::SimdVector { .. } => {
                    unsupported!("a SIMD vector in the wasm C ABI without `simd128`")
                }
                // One of 128 bits is two `i64`s as an argument, and returned in memory.
                _ => match self.lone_scalar(ty)? {
                    Some(scalar) if !is_ret || scalar.size(self.dl()).bits() <= 64 => {
                        PassMode::Cast(self.scalar(scalar)?.ty)
                    }
                    _ => PassMode::Indirect,
                },
            })
        };
        let ret = mode(ret, true)?;
        let args = args.into_iter().map(|ty| mode(ty, false)).collect::<Result<_>>()?;
        Ok(FnAbi { ret, args })
    }

    /// The one scalar a value of `ty` is made of, if it has one and nothing else besides
    /// values of no size, and fills its size: rustc's homogeneous aggregate of one unit. A
    /// union is none, as rustc has it.
    fn lone_scalar(&self, ty: Ty<'db>) -> Result<Option<Scalar>> {
        let layout = self.layout(ty)?;
        let size = layout.size;
        let fits = |scalar: Option<Scalar>| scalar.filter(|it| it.size(self.dl()) == size);
        let fields: smallvec::SmallVec<[Ty<'db>; 8]> =
            match layout.backend_repr {
                BackendRepr::Scalar(scalar) => return Ok(fits(Some(scalar))),
                BackendRepr::Memory { sized: true } => match ty.kind() {
                    TyKind::Tuple(tys) => smallvec::SmallVec::from_iter(tys.iter()),
                    TyKind::Array(element, _) if layout.fields.count() == 1 => {
                        smallvec::SmallVec::from_slice(&[element])
                    }
                    TyKind::Adt(def, subst) => {
                        let AdtId::StructId(id) = def.def_id() else { return Ok(None) };
                        let fields = self.cg.db.field_types(id.into());
                        smallvec::SmallVec::from_iter(fields.iter().map(|(_, ty)| {
                            ty.ty().instantiate(self.interner, subst).skip_norm_wip()
                        }))
                    }
                    _ => return Ok(None),
                },
                _ => return Ok(None),
            };
        let mut lone = None;
        for field in fields {
            if self.layout(field)?.is_zst() {
                continue;
            }
            if lone.is_some() {
                return Ok(None);
            }
            lone = fits(self.lone_scalar(field)?);
            if lone.is_none() {
                return Ok(None);
            }
        }
        Ok(lone)
    }

    fn count_regs(&self, types: &[Type]) -> (usize, usize) {
        let sse = |ty: &&Type| ty.is_float() || ty.is_vector();
        let floats = types.iter().filter(sse).count();
        let ints = types.iter().filter(|ty| !sse(ty)).map(|ty| ty.bytes().div_ceil(8));
        (ints.sum::<u32>() as usize, floats)
    }

    /// How the C ABI of 64-bit Windows passes one value. Mirrors rustc's
    /// `callconv/x86_win64.rs`: what is not the size of a register goes by reference.
    fn win64_pass_mode(&self, ty: Ty<'db>, is_ret: bool) -> Result<PassMode> {
        let layout = self.layout(ty)?;
        if layout.is_zst() {
            // As clang and gcc on windows-gnu, a `repr(C)` value of no size (or a transparent
            // wrapper around one) goes by reference, as an argument and as a return value; a
            // `repr(Rust)` one, such as `()`, takes no place, so that `()` matches C's `void`.
            return Ok(if layout.is_repr_c() { PassMode::Indirect } else { PassMode::Ignore });
        }
        if !is_ret && self.decays_in_c(ty)? {
            return Ok(PassMode::Indirect);
        }
        Ok(match (layout.backend_repr, layout.size.bytes()) {
            // rustc hands LLVM the vector as it is, and LLVM's Win64 convention passes one of
            // 16, 32 or 64 bytes by reference and returns one of 16 in `xmm0`.
            (BackendRepr::SimdVector { .. }, 16 | 32 | 64) if !is_ret => PassMode::Indirect,
            (BackendRepr::SimdVector { .. }, 16) => PassMode::Cast(types::I64X2),
            (BackendRepr::SimdVector { .. }, size) => {
                unsupported!("a vector of {size} bytes in the C ABI of 64-bit Windows")
            }
            (BackendRepr::Scalar(scalar), size) if size <= 8 => {
                PassMode::Direct(self.scalar(scalar)?)
            }
            // A 128-bit integer is returned in `xmm0`.
            (BackendRepr::Scalar(_), 16) if is_ret => PassMode::Cast(types::I64X2),
            (BackendRepr::Scalar(_), _) => PassMode::Indirect,
            (_, 1) => PassMode::Cast(types::I8),
            (_, 2) => PassMode::Cast(types::I16),
            (_, 4) => PassMode::Cast(types::I32),
            (_, 8) => PassMode::Cast(types::I64),
            _ => PassMode::Indirect,
        })
    }

    /// Whether a C function is handed the address of a `ty` where Rust hands over the value,
    /// as C does with a `va_list` that is an array. Mirrors rustc's
    /// `TyAndLayout::pass_indirectly_in_non_rustic_abis`, which looks through transparent
    /// wrappers for a struct marked `#[rustc_pass_indirectly_in_non_rustic_abis]`.
    fn decays_in_c(&self, ty: Ty<'db>) -> Result<bool> {
        let TyKind::Adt(def, subst) = ty.kind() else { return Ok(false) };
        let AdtId::StructId(id) = def.def_id() else { return Ok(false) };
        // rustc makes the attribute do nothing on a 1-ZST, whose wrappers it cannot peel.
        if self.layout(ty)?.is_1zst() {
            return Ok(false);
        }
        let db = self.cg.db;
        if AttrFlags::query(db, id.into())
            .contains(AttrFlags::RUSTC_PASS_INDIRECTLY_IN_NON_RUSTIC_ABIS)
        {
            return Ok(true);
        }
        let transparent = AttrFlags::repr(db, id.into())
            .is_some_and(|repr| repr.flags.contains(ReprFlags::IS_TRANSPARENT));
        if !transparent {
            return Ok(false);
        }
        for (_, field) in db.field_types(id.into()).iter() {
            let field = field.ty().instantiate(self.interner, subst).skip_norm_wip();
            if !self.layout(field)?.is_1zst() {
                return self.decays_in_c(field);
            }
        }
        Ok(false)
    }

    /// How the C ABI passes one value when registers are left for it.
    fn c_pass_mode(&self, ty: Ty<'db>) -> Result<PassMode> {
        let layout = self.layout(ty)?;
        if layout.is_zst() {
            return Ok(PassMode::Ignore);
        }
        let size = layout.size.bytes();
        let on_stack = PassMode::OnStack { size: size.next_multiple_of(8) as u32, padded: false };
        match layout.backend_repr {
            BackendRepr::Scalar(scalar) => return Ok(PassMode::Direct(self.scalar(scalar)?)),
            // An `__m128` and its kin are one `Sse` eightbyte and its `SseUp` half, one XMM
            // register whatever the lanes (wasmtime's libcalls take and return `__m128i`); a
            // shorter vector is one `Sse` eightbyte. A longer one is in a YMM or ZMM register
            // when the function has AVX, and in memory when not.
            BackendRepr::SimdVector { .. } => {
                return Ok(PassMode::Cast(match size {
                    16 => types::I64X2,
                    8 => types::F64,
                    4 => types::F32,
                    _ => unsupported!("a vector of {size} bytes in the C ABI"),
                }));
            }
            _ => (),
        }
        if size > 16 {
            return Ok(on_stack);
        }
        let mut classes = [None; 2];
        if !self.classify(ty, 0, &mut classes)? {
            return Ok(on_stack);
        }
        // An `f128` alone is a vector register's 16 bytes. An upper half whose lower half is
        // not an `Sse` eightbyte's is an `Sse` one itself.
        if classes == [Some(RegClass::Sse), Some(RegClass::SseUp)] {
            return Ok(PassMode::Cast(types::I64X2));
        }
        for class in &mut classes {
            if *class == Some(RegClass::SseUp) {
                *class = Some(RegClass::Sse);
            }
        }
        let reg = |class: Option<RegClass>, bytes: u64| match (class, bytes) {
            (Some(RegClass::Sse), 4) => types::F32,
            (Some(RegClass::Sse), _) => types::F64,
            (_, 1) => types::I8,
            (_, 2) => types::I16,
            (_, 3 | 4) => types::I32,
            _ => types::I64,
        };
        Ok(if size <= 8 {
            PassMode::Cast(reg(classes[0], size))
        } else {
            PassMode::CastPair(reg(classes[0], 8), reg(classes[1], size - 8))
        })
    }

    /// Classifies the eightbytes of a value of at most sixteen bytes at `offset`; false if it
    /// has to go in memory.
    fn classify(
        &self,
        ty: Ty<'db>,
        offset: u64,
        classes: &mut [Option<RegClass>; 2],
    ) -> Result<bool> {
        let layout = self.layout(ty)?;
        if layout.is_zst() {
            return Ok(true);
        }
        if offset % layout.align.abi.bytes() != 0 {
            return Ok(false);
        }
        let mut scalar = |scalar: Scalar, offset: u64| {
            let mut class = match scalar.primitive() {
                Primitive::Float(_) => RegClass::Sse,
                Primitive::Int(..) | Primitive::Pointer(_) => RegClass::Int,
            };
            let last = (offset + scalar.size(self.dl()).bytes() - 1) / 8;
            for slot in &mut classes[(offset / 8) as usize..=last as usize] {
                *slot = Some(slot.map_or(class, |old: RegClass| old.min(class)));
                if class == RegClass::Sse {
                    class = RegClass::SseUp;
                }
            }
        };
        match layout.backend_repr {
            BackendRepr::Scalar(a) => scalar(a, offset),
            BackendRepr::ScalarPair { a, b, b_offset } => {
                scalar(a, offset);
                scalar(b, offset + b_offset.bytes());
            }
            BackendRepr::Memory { sized: true } => match ty.kind() {
                TyKind::Tuple(tys) => {
                    for (i, ty) in tys.iter().enumerate() {
                        if !self.classify(ty, offset + layout.fields.offset(i).bytes(), classes)? {
                            return Ok(false);
                        }
                    }
                }
                TyKind::Array(element, _) => {
                    let stride = self.layout(element)?.size.bytes();
                    for i in 0..layout.fields.count() as u64 {
                        if !self.classify(element, offset + i * stride, classes)? {
                            return Ok(false);
                        }
                    }
                }
                TyKind::Adt(def, subst) => {
                    let variant: VariantId = match def.def_id() {
                        AdtId::StructId(id) => id.into(),
                        AdtId::UnionId(id) => id.into(),
                        AdtId::EnumId(_) => unsupported!("enum {ty:?} in the C ABI"),
                    };
                    for (field, ty) in self.cg.db.field_types(variant).iter() {
                        let ty = ty.ty().instantiate(self.interner, subst).skip_norm_wip();
                        let field = layout.fields.offset(u32::from(field.into_raw()) as usize);
                        if !self.classify(ty, offset + field.bytes(), classes)? {
                            return Ok(false);
                        }
                    }
                }
                _ => unsupported!("{ty:?} in the C ABI"),
            },
            _ => unsupported!("{ty:?} in the C ABI"),
        }
        Ok(true)
    }

    fn dl(&self) -> &'db rustc_abi::TargetDataLayout {
        self.dl
    }

    fn push_abi_params(&self, mode: PassMode, params: &mut Vec<AbiParam>) {
        let copied = |size| {
            AbiParam::special(self.cg.target.pointer(), ArgumentPurpose::StructArgument(size))
        };
        match mode {
            PassMode::Ignore => (),
            PassMode::Direct(s) => params.push(s.param()),
            PassMode::Pair(a, b, _) => params.extend([a.param(), b.param()]),
            PassMode::Cast(ty) => params.push(AbiParam::new(ty)),
            PassMode::CastPair(a, b) => params.extend([AbiParam::new(a), AbiParam::new(b)]),
            PassMode::Uniform { unit, count, skipped } => {
                params.extend((0..skipped).map(|_| AbiParam::new(PassMode::register(unit))));
                params.extend((0..count).map(|_| AbiParam::new(unit)));
            }
            PassMode::Indirect => params.push(AbiParam::new(self.cg.target.pointer())),
            PassMode::IndirectUnsized => params.extend([
                AbiParam::new(self.cg.target.pointer()),
                AbiParam::new(self.cg.target.pointer()),
            ]),
            PassMode::Spread { .. } => params.push(AbiParam::new(types::F64)),
            PassMode::OnStack { size, padded } => {
                params.extend(padded.then(|| copied(8)));
                params.push(copied(size));
            }
            PassMode::SpilledPair { a, b, spilled, .. } => {
                for (half, spilled) in [a, b].into_iter().zip(spilled) {
                    match spilled {
                        None => params.push(half.param()),
                        Some(padded) => {
                            params.extend(padded.then(|| copied(8)));
                            params.push(copied(16));
                        }
                    }
                }
            }
            PassMode::SkippingPair { a, b, skipped, .. } => {
                for (half, skipped) in [a, b].into_iter().zip(skipped) {
                    params.extend((0..skipped).map(|_| AbiParam::new(types::I64)));
                    params.push(half.param());
                }
            }
        }
    }

    fn signature(&self, args: &[PassMode], ret: PassMode) -> Signature {
        let mut sig = self.cg.make_signature();
        match ret {
            PassMode::Indirect => sig
                .params
                .push(AbiParam::special(self.cg.target.pointer(), ArgumentPurpose::StructReturn)),
            mode => self.push_abi_params(mode, &mut sig.returns),
        }
        for &arg in args {
            self.push_abi_params(arg, &mut sig.params);
        }
        sig
    }

    /// Loads the register image of a `PassMode::Cast` aggregate of `size` bytes.
    fn load_cast(&mut self, ty: Type, addr: Value, size: Size) -> Value {
        if u64::from(ty.bytes()) == size.bytes() {
            return self.builder.ins().load(ty, MEM, addr, 0);
        }
        let tmp = if ty.bytes() > 8 { self.scratch_wide() } else { self.scratch() };
        self.copy(tmp, addr, size);
        self.builder.ins().load(ty, MEM, tmp, 0)
    }

    fn store_cast(&mut self, value: Value, addr: Value, size: Size) {
        let ty = self.builder.func.dfg.value_type(value);
        if u64::from(ty.bytes()) == size.bytes() {
            self.builder.ins().store(MEM, value, addr, 0);
            return;
        }
        let tmp = if ty.bytes() > 8 { self.scratch_wide() } else { self.scratch() };
        self.builder.ins().store(MEM, value, tmp, 0);
        self.copy(addr, tmp, size);
    }

    /// The values that carry the argument at `addr` to a callee.
    fn pass(&mut self, mode: PassMode, addr: Value, layout: &Layout, values: &mut Vec<Value>) {
        match mode {
            PassMode::Ignore => (),
            PassMode::Direct(s) => values.push(self.builder.ins().load(s.ty, MEM, addr, 0)),
            PassMode::Pair(a, b, offset) => {
                values.push(self.builder.ins().load(a.ty, MEM, addr, 0));
                values.push(self.builder.ins().load(b.ty, MEM, addr, offset.bytes() as i32));
            }
            PassMode::Cast(ty) => values.push(self.load_cast(ty, addr, layout.size)),
            PassMode::Spread { lane, stride } => {
                let spread = self.scratch();
                for i in 0..(layout.size.bytes() / u64::from(lane.bytes())) as i32 {
                    let value = self.builder.ins().load(lane, MEM, addr, i * lane.bytes() as i32);
                    self.builder.ins().store(MEM, value, spread, i * i32::from(stride));
                }
                values.push(self.builder.ins().load(types::F64, MEM, spread, 0));
            }
            PassMode::CastPair(a, b) => {
                values.push(self.builder.ins().load(a, MEM, addr, 0));
                let rest = self.builder.ins().iadd_imm(addr, 8);
                values.push(self.load_cast(b, rest, layout.size - Size::from_bytes(8)));
            }
            PassMode::Uniform { unit, count, skipped } => {
                for _ in 0..skipped {
                    values.push(self.zero(PassMode::register(unit)));
                }
                let step = u64::from(unit.bytes());
                for index in 0..u64::from(count) {
                    let part = self.builder.ins().iadd_imm(addr, (index * step) as i64);
                    let left = layout.size - Size::from_bytes(index * step);
                    values.push(self.load_cast(unit, part, left.min(Size::from_bytes(step))));
                }
            }
            PassMode::Indirect => {
                // The callee owns the memory it is handed.
                if self.given_away.gives(addr) {
                    values.push(addr);
                    return;
                }
                let copy = self.argument_slot(layout);
                self.copy_value(copy, addr, layout);
                values.push(copy);
            }
            // The callee is handed the value where it is, as rustc does: the operand is a
            // place moved out of, whose memory nothing reads again.
            PassMode::IndirectUnsized => {
                values.push(self.builder.ins().load(self.cg.target.pointer(), MEM, addr, 0));
                values.push(self.builder.ins().load(
                    self.cg.target.pointer(),
                    MEM,
                    addr,
                    self.cg.target.meta_at(),
                ));
            }
            PassMode::OnStack { padded, .. } => {
                if padded {
                    values.push(self.scratch());
                }
                values.push(addr);
            }
            PassMode::SpilledPair { a, b, offset, spilled } => {
                for (half, at, spilled) in [(a, 0, spilled[0]), (b, offset.bytes(), spilled[1])] {
                    match spilled {
                        None => values.push(self.builder.ins().load(half.ty, MEM, addr, at as i32)),
                        Some(padded) => {
                            if padded {
                                values.push(self.scratch());
                            }
                            values.push(self.builder.ins().iadd_imm(addr, at as i64));
                        }
                    }
                }
            }
            PassMode::SkippingPair { a, b, offset, skipped } => {
                for (half, at, skipped) in [(a, 0, skipped[0]), (b, offset.bytes(), skipped[1])] {
                    for _ in 0..skipped {
                        values.push(self.zero(types::I64));
                    }
                    values.push(self.builder.ins().load(half.ty, MEM, addr, at as i32));
                }
            }
        }
    }

    /// Stores what `pass` or a return sent, taking the values off the front of `values`.
    fn receive(
        &mut self,
        mode: PassMode,
        values: &mut impl Iterator<Item = Value>,
        addr: Value,
        layout: &Layout,
    ) {
        let mut next = || values.next().expect("a value for every abi parameter");
        match mode {
            PassMode::Ignore | PassMode::Indirect | PassMode::OnStack { .. } => (),
            PassMode::IndirectUnsized => {
                let (data, meta) = (next(), next());
                self.builder.ins().store(MEM, data, addr, 0);
                self.builder.ins().store(MEM, meta, addr, self.cg.target.meta_at());
            }
            PassMode::Direct(_) => {
                let value = next();
                self.builder.ins().store(MEM, value, addr, 0);
            }
            PassMode::Pair(_, _, offset) => {
                let (a, b) = (next(), next());
                self.builder.ins().store(MEM, a, addr, 0);
                self.builder.ins().store(MEM, b, addr, offset.bytes() as i32);
            }
            PassMode::Cast(_) => {
                let value = next();
                self.store_cast(value, addr, layout.size);
            }
            PassMode::Spread { lane, stride } => {
                let spread = self.scratch();
                let value = next();
                self.builder.ins().store(MEM, value, spread, 0);
                for i in 0..(layout.size.bytes() / u64::from(lane.bytes())) as i32 {
                    let value = self.builder.ins().load(lane, MEM, spread, i * i32::from(stride));
                    self.builder.ins().store(MEM, value, addr, i * lane.bytes() as i32);
                }
            }
            PassMode::CastPair(..) => {
                let (a, b) = (next(), next());
                self.builder.ins().store(MEM, a, addr, 0);
                let rest = self.builder.ins().iadd_imm(addr, 8);
                self.store_cast(b, rest, layout.size - Size::from_bytes(8));
            }
            PassMode::Uniform { unit, count, skipped } => {
                for _ in 0..skipped {
                    next();
                }
                let step = u64::from(unit.bytes());
                for index in 0..u64::from(count) {
                    let value = next();
                    let part = self.builder.ins().iadd_imm(addr, (index * step) as i64);
                    let left = layout.size - Size::from_bytes(index * step);
                    self.store_cast(value, part, left.min(Size::from_bytes(step)));
                }
            }
            PassMode::SpilledPair { a, b, offset, spilled } => {
                for (half, at, spilled) in [(a, 0, spilled[0]), (b, offset.bytes(), spilled[1])] {
                    let value = match spilled {
                        None => next(),
                        Some(padded) => {
                            if padded {
                                next();
                            }
                            let copy = next();
                            self.builder.ins().load(half.ty, MEM, copy, 0)
                        }
                    };
                    self.builder.ins().store(MEM, value, addr, at as i32);
                }
            }
            PassMode::SkippingPair { offset, skipped, .. } => {
                for (at, skipped) in [(0, skipped[0]), (offset.bytes(), skipped[1])] {
                    for _ in 0..skipped {
                        next();
                    }
                    let value = next();
                    self.builder.ins().store(MEM, value, addr, at as i32);
                }
            }
        }
    }

    /// The value of `ty` that is all zero bits.
    fn zero(&mut self, ty: Type) -> Value {
        match ty {
            types::F32 => self.builder.ins().f32const(0.0),
            types::F64 => self.builder.ins().f64const(0.0),
            ty if f16_f128::soft(ty) => self.soft_zero(ty),
            _ => self.builder.ins().iconst(ty, 0),
        }
    }

    /// Eight bytes of stack.
    fn scratch(&mut self) -> Value {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            3,
        ));
        self.builder.ins().stack_addr(self.cg.target.pointer(), slot, 0)
    }

    /// Storage for an integer of 128 bits.
    fn scratch_wide(&mut self) -> Value {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            16,
            4,
        ));
        self.builder.ins().stack_addr(self.cg.target.pointer(), slot, 0)
    }

    /// Storage for a fat pointer.
    fn scratch_pair(&mut self) -> Value {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            16,
            3,
        ));
        self.builder.ins().stack_addr(self.cg.target.pointer(), slot, 0)
    }

    /// Storage for a temporary that is borrowed for `'static` and that [`Self::promoted_constant`]
    /// could not evaluate: one with a place in the source inside it that is not the whole of
    /// it (`&S { f: file!(), l: line!() }`, which expect-test's `expect![..]` is, its line read
    /// where the code runs), or one const eval refused. The function writes the same constant
    /// there every time it runs, so it is writable data: in v2 the function instance's own
    /// ([`blaze_codegen::Data::owned`] of its key), so two instances never share one. Not a
    /// stack slot: the borrow may outlive the call, and an `async` body never holds a promoted
    /// constant across a yield (`MirBody::promoted_constants`), so a slot would be gone when
    /// it resumes.
    fn promoted_slot(&mut self, layout: &Layout) -> Result<Value> {
        let align = layout.align.abi.bytes();
        if layout.size.bytes() == 0 {
            return Ok(self.builder.ins().iconst(self.cg.target.pointer(), align as i64));
        }
        let blob = ConstBlob {
            // alloc: the data object's bytes, which `define_data` takes and the unit keeps
            bytes: vec![0; layout.size.bytes_usize()],
            relocs: Vec::new(),    // alloc: empty, allocates nothing
            fn_relocs: Vec::new(), // alloc: empty, allocates nothing
            align,
            type_ids: Vec::new(), // alloc: empty, allocates nothing
        };
        let data = self.cg.define_data(blob, true)?;
        let data = self.cg.declare_data_in_func(data, self.builder.func);
        Ok(self.builder.ins().symbol_value(self.cg.target.pointer(), data))
    }

    /// The read-only data a promoted `&expr` points to, when `expr` can be evaluated now.
    /// Equal constants share an address, as they do for rustc. A type that is not `plain` is
    /// promoted when the value is: a `None` of an `Option<Box<T>>` has nothing to drop. A
    /// `file!()` or a `concat!` with it is made here, where it is written, since const eval sees
    /// only the path's placeholder.
    fn promoted_constant(
        &mut self,
        body: &'db MirBody<'db>,
        promoted: Promoted<'db>,
        layout: &Layout,
        plain: bool,
    ) -> Result<Option<Value>> {
        let db = self.cg.db;
        if layout.size.bytes() == 0 {
            return Ok(None);
        }
        if self.mentions_location(promoted.owner, promoted.expr) {
            return Ok(self.builtin_location(promoted.owner, promoted.expr).ok().flatten());
        }
        let Some(args) = promoted.args(body, self.args.as_ref()) else { return Ok(None) };
        // A promoted constant whose MIR does not build is an error, never left unpromoted:
        // compile identity refuses it too (`hir_ty_identity`'s sites), so the two agree.
        let constant = monomorphized_promoted_mir_body_query(
            db,
            promoted.owner,
            promoted.expr,
            args,
            self.env.store(),
        )
        .map_err(|error| no_promoted_mir(db, promoted, error))?;
        let Some(allocation) =
            interpret_mir(db, constant, Some(self.env)).ok().and_then(|it| it.0.ok())
        else {
            return Ok(None);
        };
        let ty = allocation.ty;
        let Ok(blob) = self.allocation_blob(allocation, ConstOf::Promoted(promoted), false) else {
            return Ok(None);
        };
        if blob.bytes.len() != layout.size.bytes_usize()
            || !plain && !self.is_plain_value(&blob.bytes, 0, ty)
        {
            return Ok(None);
        }
        Ok(self.blob_addr(blob).ok())
    }

    /// Whether a value of `ty` can be read-only data that is never dropped.
    fn is_plain(&self, ty: Ty<'db>) -> bool {
        self.is_freeze(ty) && has_drop_glue(&self.infcx, ty, self.env.param_env) == DropGlue::None
    }

    /// Whether the value of `ty` at `offset` in `bytes` has nothing to drop and no cell in
    /// it, which a value of a type that is not plain can be: an enum by its variant.
    fn is_plain_value(&self, bytes: &[u8], offset: usize, ty: Ty<'db>) -> bool {
        if self.is_plain(ty) {
            return true;
        }
        let db = self.cg.db;
        let Ok(layout) = self.layout(ty) else { return false };
        match ty.kind() {
            TyKind::Tuple(fields) => fields.iter().enumerate().all(|(i, field)| {
                let at = offset + layout.fields.offset(i).bytes_usize();
                self.is_plain_value(bytes, at, field)
            }),
            TyKind::Array(element, len) => {
                let (Some(len), Ok(element_layout)) =
                    (try_const_usize(db, len), self.layout(element))
                else {
                    return false;
                };
                let size = element_layout.size.bytes_usize();
                (0..len as usize).all(|i| self.is_plain_value(bytes, offset + i * size, element))
            }
            TyKind::Adt(def, args) => {
                let adt = def.def_id();
                if destructor(db, adt).is_some() {
                    return false;
                }
                let (variant, layout): (VariantId, Cow<'_, Layout>) = match adt {
                    AdtId::StructId(id) if Some(id) == self.interner.lang_items().UnsafeCell => {
                        return false;
                    }
                    AdtId::StructId(id) => (id.into(), Cow::Borrowed(&*layout)),
                    AdtId::UnionId(_) => return false,
                    AdtId::EnumId(id) => {
                        let Some(bytes) = bytes.get(offset..offset + layout.size.bytes_usize())
                        else {
                            return false;
                        };
                        match detect_variant_from_bytes(&layout, db, self.dl(), bytes, id) {
                            Some((variant, layout)) => (variant.into(), layout),
                            None => return false,
                        }
                    }
                };
                db.field_types(variant).iter().enumerate().all(|(i, (_, field))| {
                    let field = field.ty().instantiate(self.interner, args).skip_norm_wip();
                    let at = offset + layout.fields.offset(i).bytes_usize();
                    self.is_plain_value(bytes, at, field)
                })
            }
            _ => false,
        }
    }

    /// Storage for the copy of an argument a callee is handed. The copy is the callee's
    /// until it returns and nobody's after, so a later call has its own in the same place:
    /// a function that calls a hundred others has the frame of its widest call and not
    /// of all of them.
    fn argument_slot(&mut self, layout: &Layout) -> Value {
        let (size, align) = (layout.size.bytes() as u32, layout.align.abi.bytes());
        if align > 16 {
            return self.stack_slot(layout);
        }
        let align_shift = self.slot_align_shift(layout);
        let free = self.free_argument_slots.iter().position(|it| it.1 == (size, align_shift));
        let slot = match free {
            Some(free) => self.free_argument_slots.swap_remove(free).0,
            None => self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                size,
                align_shift,
            )),
        };
        self.argument_slots.push((slot, (size, align_shift)));
        self.builder.ins().stack_addr(self.cg.target.pointer(), slot, 0)
    }

    /// The address `addr` is, taken anew when it is of a slot of the frame. One taken where
    /// the function starts is kept in a register or spilled for as long as the function
    /// runs, and a function has more locals than there are registers.
    fn slot_addr(&mut self, addr: Value) -> Value {
        let dfg = &self.builder.func.dfg;
        if let ir::ValueDef::Result(inst, _) = dfg.value_def(addr)
            && let ir::InstructionData::StackAddr {
                opcode: ir::Opcode::StackAddr,
                stack_slot,
                offset,
            } = dfg.insts[inst]
        {
            return self.builder.ins().stack_addr(self.cg.target.pointer(), stack_slot, offset);
        }
        addr
    }

    /// The alignment of a slot of `layout` of at most 16, as a shift: the layout's, or 16 on
    /// AArch64 for a value `copy_value` copies in vectors, whose vectors are then at offsets
    /// of the frame a load or store of 16 bytes reaches (see `reaches_vector`).
    fn slot_align_shift(&self, layout: &Layout) -> u8 {
        let shift = layout.align.abi.bytes().trailing_zeros() as u8;
        if self.cg.target.arch == Arch::Aarch64 && self.copies_in_vectors(layout) {
            return shift.max(4);
        }
        shift
    }

    fn stack_slot(&mut self, layout: &Layout) -> Value {
        let (size, align) = (layout.size.bytes() as u32, layout.align.abi.bytes());
        if align <= 16 {
            let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                size,
                self.slot_align_shift(layout),
            ));
            return self.builder.ins().stack_addr(self.cg.target.pointer(), slot, 0);
        }
        // Cranelift keeps the frame 16-byte aligned and no more, so over-allocate and round up.
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size + align as u32,
            4,
        ));
        let base = self.builder.ins().stack_addr(self.cg.target.pointer(), slot, 0);
        let bumped = self.builder.ins().iadd_imm(base, align as i64 - 1);
        self.builder.ins().band_imm(bumped, -(align as i64))
    }

    /// The signature a MIR body is compiled with.
    fn body_abi(&self, body: &MirBody<'db>, abi: ExternAbi) -> Result<FnAbi> {
        self.fn_abi(
            abi,
            body.locals[returned_local(body)].ty.as_ref(),
            body.param_locals.iter().map(|&local| body.locals[local].ty.as_ref()),
        )
    }

    fn body_types(&self, def: InstanceDef, args: GenericArgs<'db>) -> Result<&'db BodyTypes> {
        // The types of a body are the same however it is compiled.
        let instance = Instance { def, args: args.store(), checked: false };
        body_types(self.cg.db, instance, self.env.store()).as_ref().map_err(Clone::clone)
    }

    /// The signature the body of an instance is compiled with.
    fn body_signature(&self, body: &BodyTypes) -> Result<Signature> {
        let abi = self.fn_abi(
            ExternAbi::Rust,
            body.returned.as_ref(),
            body.params.iter().map(|ty| ty.as_ref()),
        )?;
        Ok(self.signature(&abi.args, abi.ret))
    }

    fn codegen(&mut self) -> Result<()> {
        let body = self.body.expect("MIR body");
        self.check_bfloat_blocks(body)?;
        let FnAbi { ret, args: mut param_modes } = self.body_abi(body, self.abi)?;
        self.ret_mode = ret;
        if self.variadic {
            // The list is made here of what came, and is not itself something that came.
            param_modes.pop();
        }
        let param_layouts = body
            .param_locals
            .iter()
            .map(|&l| self.layout(body.locals[l].ty.as_ref()))
            .collect::<Result<Vec<_>>>()?;
        let mut sig = self.signature(&param_modes, ret);
        if self.tracked {
            sig.params.push(AbiParam::new(self.cg.target.pointer()));
        }
        let variadic = match self.variadic {
            true => {
                let native = match self.cg.target.variable_arguments() {
                    Some(VariableArguments::Registers) => {
                        matches!(self.abi, ExternAbi::C { .. })
                            || matches!(self.abi, ExternAbi::SysV64 { .. })
                                && self.cg.target.arch == Arch::X86_64
                    }
                    Some(VariableArguments::Homed) => {
                        matches!(self.abi, ExternAbi::C { .. } | ExternAbi::Win64 { .. })
                    }
                    Some(VariableArguments::Stack | VariableArguments::Buffer) => {
                        matches!(self.abi, ExternAbi::C { .. })
                    }
                    None => false,
                };
                let Some(variadic) = Variadic::of(self.cg.target, &sig)?.filter(|_| native) else {
                    unsupported!("a function that takes `...` for this target");
                };
                variadic.rest(&mut sig, self.cg.target.pointer());
                Some(variadic)
            }
            false => None,
        };
        self.builder.func.signature = sig;
        self.cg.pointees = self.pointees(body, ret, &param_modes)?;

        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let mut incoming =
            smallvec::SmallVec::<[Value; 16]>::from_slice(self.builder.block_params(entry))
                .into_iter();

        if let PassMode::Indirect = self.ret_mode {
            let addr = incoming.next().expect("sret param");
            self.locals.insert(returned_local(body), LocalSlot { addr, borrowed: false });
        }
        for ((&local, layout), mode) in
            body.param_locals.iter().zip(&param_layouts).zip(param_modes)
        {
            let addr = match mode {
                PassMode::OnStack { padded: true, .. } => {
                    incoming.nth(1).expect("a parameter in memory after its padding")
                }
                PassMode::Indirect | PassMode::OnStack { .. } => {
                    incoming.next().expect("indirect param")
                }
                PassMode::IndirectUnsized => self.scratch_pair(),
                _ => self.stack_slot(layout),
            };
            self.receive(mode, &mut incoming, addr, layout);
            if self.cg.target.arch == Arch::X86_64
                && let BackendRepr::Scalar(scalar) = layout.backend_repr
                && scalar.primitive() == Primitive::Float(rustc_abi::Float::F16B)
            {
                self.receive_bfloat(addr)?;
            }
            // An unsized parameter's slot is where the value is and its metadata, as
            // rustc's `LocalRef::UnsizedPlace`.
            let borrowed = matches!(mode, PassMode::IndirectUnsized);
            self.locals.insert(local, LocalSlot { addr, borrowed });
            if matches!(mode, PassMode::Indirect | PassMode::OnStack { .. }) {
                self.note_passed_in_memory(body, local, addr);
            }
        }
        if self.tracked {
            self.location = incoming.next();
        }
        if let Some(Variadic::Windows(named)) = variadic {
            let list = *body.param_locals.last().expect("the list of a variadic function");
            let layout = self.layout(body.locals[list].ty.as_ref())?;
            if layout.size.bytes() != 8 {
                unsupported!("a `VaList` that is not the one of this target");
            }
            let addr = self.stack_slot(&layout);
            let homed = smallvec::SmallVec::<[Value; 4]>::from_iter(
                (0..named.left())
                    .map(|_| incoming.next().expect("a register of the variable arguments")),
            );
            let stack = incoming.next().expect("the variable arguments on the stack");
            let first = -i64::from(named.left() * 8);
            let first = self.builder.ins().iadd_imm(stack, first);
            for (at, value) in homed.into_iter().enumerate() {
                self.builder.ins().store(MEM, value, first, at as i32 * 8);
            }
            self.builder.ins().store(MEM, first, addr, 0);
            self.locals.insert(list, LocalSlot { addr, borrowed: false });
        }
        if let Some(Variadic::SystemV(named)) = variadic {
            let list = *body.param_locals.last().expect("the list of a variadic function");
            let layout = self.layout(body.locals[list].ty.as_ref())?;
            let (int_count, floats_bytes) = (
                VariadicRegisters::int_count(named.arch),
                VariadicRegisters::FLOATS * VariadicRegisters::FLOAT_BYTES,
            );
            if layout.size.bytes() != if named.arch == Arch::Aarch64 { 32 } else { 24 } {
                unsupported!("a `VaList` that is not the one of this target");
            }
            let addr = self.stack_slot(&layout);
            // Where `va_arg` expects the registers saved: the integer ones, then the vector
            // ones, each kind at the place it has among all of its kind.
            let saved = self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                int_count * 8 + floats_bytes,
                4,
            ));
            let saved = self.builder.ins().stack_addr(self.cg.target.pointer(), saved, 0);
            for offset in (named.ints..int_count).map(|at| at * 8).chain(
                (named.floats..VariadicRegisters::FLOATS)
                    .map(|at| int_count * 8 + at * VariadicRegisters::FLOAT_BYTES),
            ) {
                let value = incoming.next().expect("a register of the variable arguments");
                self.builder.ins().store(MEM, value, saved, offset as i32);
            }
            let floats_at = int_count * 8 + named.floats * VariadicRegisters::FLOAT_BYTES;
            if named.arch == Arch::Aarch64 {
                // The list has where each kind of saved register ends, and how far before
                // that the next one is. The arguments on the stack are above the frame
                // pointer and the return address, the named ones first. Asking for the
                // frame pointer here is what has `emit` keep one.
                let frame = self.builder.ins().get_frame_pointer(self.cg.target.pointer());
                let stack = self.builder.ins().iadd_imm(frame, 16 + i64::from(named.stack));
                let ints_end = self.builder.ins().iadd_imm(saved, i64::from(int_count * 8));
                let floats_end =
                    self.builder.ins().iadd_imm(saved, i64::from(int_count * 8 + floats_bytes));
                let ints = i64::from(named.ints * 8) - i64::from(int_count * 8);
                let ints = self.builder.ins().iconst(types::I32, ints);
                let floats = i64::from(floats_at) - i64::from(int_count * 8 + floats_bytes);
                let floats = self.builder.ins().iconst(types::I32, floats);
                self.builder.ins().store(MEM, stack, addr, 0);
                self.builder.ins().store(MEM, ints_end, addr, 8);
                self.builder.ins().store(MEM, floats_end, addr, 16);
                self.builder.ins().store(MEM, ints, addr, 24);
                self.builder.ins().store(MEM, floats, addr, 28);
            } else {
                // The list has how far into what was saved the next of each kind is.
                let stack = incoming.next().expect("the variable arguments on the stack");
                let ints = self.builder.ins().iconst(types::I32, i64::from(named.ints * 8));
                let floats = self.builder.ins().iconst(types::I32, i64::from(floats_at));
                self.builder.ins().store(MEM, ints, addr, 0);
                self.builder.ins().store(MEM, floats, addr, 4);
                self.builder.ins().store(MEM, stack, addr, 8);
                self.builder.ins().store(MEM, saved, addr, 16);
            }
            self.locals.insert(list, LocalSlot { addr, borrowed: false });
        }
        if let Some(Variadic::Apple { named }) = variadic {
            let list = *body.param_locals.last().expect("the list of a variadic function");
            let layout = self.layout(body.locals[list].ty.as_ref())?;
            if layout.size.bytes() != 8 {
                unsupported!("a `VaList` that is not the one of this target");
            }
            let addr = self.stack_slot(&layout);
            // What the caller left on the stack is right above the frame record, which is
            // the frame pointer and the return address: the named parameters that are
            // there, then the variable ones.
            let frame = self.builder.ins().get_frame_pointer(self.cg.target.pointer());
            let stack = self.builder.ins().iadd_imm(frame, 16 + i64::from(named));
            self.builder.ins().store(MEM, stack, addr, 0);
            self.locals.insert(list, LocalSlot { addr, borrowed: false });
        }
        if let Some(Variadic::Buffer) = variadic {
            let list = *body.param_locals.last().expect("the list of a variadic function");
            let layout = self.layout(body.locals[list].ty.as_ref())?;
            if layout.size.bytes() != u64::from(self.cg.target.pointer().bytes()) {
                unsupported!("a `VaList` that is not the one of this target");
            }
            let addr = self.stack_slot(&layout);
            let buffer = incoming.next().expect("where the variable arguments are");
            self.builder.ins().store(MEM, buffer, addr, 0);
            self.locals.insert(list, LocalSlot { addr, borrowed: false });
        }
        if body.phase != MirPhase::Runtime {
            unsupported!("a body whose drops are not elaborated");
        }
        // An `async` body keeps its locals in the future, where they survive a return.
        let frame = match body.coroutine {
            Some(_) => {
                let receiver = body.param_locals[0];
                let Some(ty) = body.locals[receiver].ty.as_ref().builtin_deref(true) else {
                    unsupported!("async body without a reference to its future");
                };
                let TyKind::Coroutine(_, args) = ty.kind() else {
                    unsupported!("async body of {ty:?}");
                };
                let upvars = args.as_coroutine().tupled_upvars_ty().tuple_fields().len();
                let held = body.locals_held_across_yields();
                let frame = CoroutineFrame { upvars };
                let addr = self.builder.ins().load(
                    self.cg.target.pointer(),
                    MEM,
                    self.locals[receiver].addr,
                    0,
                );
                Some((frame, self.layout(ty)?, addr, ty, held))
            }
            None => None,
        };
        let in_frame = |this: &mut Self, field: usize| {
            let (_, layout, addr, ..) = frame.as_ref().expect("frame");
            this.builder.ins().iadd_imm(*addr, layout.fields.offset(field).bytes() as i64)
        };
        // Where a local is among the ones the future keeps, when it is one.
        let held_at = |local: LocalId| {
            let (.., held) = frame.as_ref()?;
            held.locals.binary_search(&local).ok()
        };
        // In an `async` body a temporary may be kept in the future where its binding is not.
        let mut forwarded =
            if frame.is_none() { forwarded_temps(body) } else { HashMap::new_in(Global) };
        let in_place = match self.ret_mode {
            PassMode::Indirect if frame.is_none() => {
                returned_in_place(body, &forwarded).filter(|it| !forwarded.contains_key(it))
            }
            _ => None,
        };
        if let Some(local) = in_place {
            self.locals.insert(local, self.locals[returned_local(body)]);
        }
        // A local kept in a register gets a slot only when it needs memory: see `local_slot`.
        let mut registers = Vec::new_in(self.frame);
        if frame.is_none() {
            for local in register_locals(body, &forwarded) {
                if !self.locals.contains_idx(local)
                    && let Some(ty) = self.register_ty(body.locals[local].ty.as_ref())
                {
                    registers.push((local, ty));
                }
            }
        }
        for &(local, ty) in &registers {
            let var = self.builder.declare_var(ty);
            self.registers.insert(local, Register { var, ty });
        }
        if frame.is_none() {
            for (temp, from) in moved_arguments(body, &forwarded) {
                if !self.registers.contains_idx(temp) && !self.registers.contains_idx(from) {
                    forwarded.insert(temp, from);
                }
            }
        }
        let unmentioned = unmentioned_locals(body);
        let mut shareable: Vec<(LocalId, ir::StackSlot), &Frame> = Vec::new_in(self.frame);
        for (local, data) in body.locals.iter() {
            if self.locals.contains_idx(local)
                || forwarded.contains_key(&local)
                || self.registers.contains_idx(local)
            {
                continue;
            }
            let layout = self.layout(data.ty.as_ref())?;
            // Nothing reads or writes it, so it needs no memory: a closure's body has a local
            // for each capture and for each binding of the function it is written in, and a
            // closure that captures a large value by move would otherwise have room for it
            // twice in every call's frame (bug D: two such frames overflowed CoreAudio's
            // 512 KB thread). The address is as a value of no size has it.
            if frame.is_none() && unmentioned.contains(&local) && !layout.is_unsized() {
                let addr = self
                    .builder
                    .ins()
                    .iconst(self.cg.target.pointer(), layout.align.abi.bytes() as i64);
                self.locals.insert(local, LocalSlot { addr, borrowed: false });
                continue;
            }
            // A capture an `async` body uses in place is the future's field.
            if let Some((_, frame_layout, frame_addr, ..)) = &frame
                && let Some(&(_, field)) = body.upvars_in_place.iter().find(|it| it.0 == local)
            {
                let offset = frame_layout.fields.offset(field as usize).bytes();
                let addr = self.builder.ins().iadd_imm(*frame_addr, offset as i64);
                self.locals.insert(local, LocalSlot { addr, borrowed: false });
                continue;
            }
            if let Some((_, frame_layout, frame_addr, _, held)) = &frame
                && let Some(at) = held_at(local)
            {
                let Some(offset) = CoroutineFrame::local_offset(frame_layout, held, at) else {
                    unsupported!("a held local that is kept nowhere");
                };
                let addr = self.builder.ins().iadd_imm(*frame_addr, offset.bytes() as i64);
                self.locals.insert(local, LocalSlot { addr, borrowed: false });
                self.held_at.push((local, offset.bytes() as u32));
                continue;
            }
            let promoted = promoted_constants(self.cg.db, body).find(|it| it.local == local);
            // A slot of its own, which it may share with locals whose storage it never
            // overlaps: see `overlap`.
            if promoted.is_none()
                && frame.is_none()
                && !layout.is_unsized()
                && layout.align.abi.bytes() <= 16
            {
                let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    layout.size.bytes() as u32,
                    layout.align.abi.bytes().trailing_zeros() as u8,
                ));
                let addr = self.builder.ins().stack_addr(self.cg.target.pointer(), slot, 0);
                self.locals.insert(local, LocalSlot { addr, borrowed: false });
                shareable.push((local, slot));
                continue;
            }
            let addr = if let Some(promoted) = promoted {
                let ty = data.ty.as_ref();
                let plain = self.is_plain(ty);
                match self.promoted_constant(body, promoted, &layout, plain)? {
                    Some(addr) => {
                        self.constant_locals.push(local);
                        addr
                    }
                    None if plain => self.promoted_slot(&layout)?,
                    None => self.stack_slot(&layout),
                }
            } else {
                self.stack_slot(&layout)
            };
            if layout.is_unsized() {
                let addr = self.scratch_pair();
                self.locals.insert(local, LocalSlot { addr, borrowed: true });
                continue;
            }
            self.locals.insert(local, LocalSlot { addr, borrowed: false });
        }
        self.note_shareable(body, &shareable, &forwarded);
        for (temp, to) in forwarded {
            self.locals.insert(temp, self.locals[to]);
        }
        if frame.is_none()
            && let Some((name, lowered)) = self.neon_fused()?
        {
            let args = smallvec::SmallVec::<[(Value, Ty<'db>); 8]>::from_iter(
                body.param_locals
                    .iter()
                    .map(|&local| (self.locals[local].addr, body.locals[local].ty.as_ref())),
            );
            let returned = returned_local(body);
            let dst = self.locals[returned].addr;
            self.neon_call(&name, &lowered, &args, dst, body.locals[returned].ty.as_ref())?;
            return self.return_();
        }
        for (id, _) in body.basic_blocks.iter() {
            let block = self.builder.create_block();
            self.blocks.insert(id, block);
        }

        // Only blocks that can run are compiled: `if cfg!(miri) { .. }` calls functions that do
        // not exist.
        let mut reachable = blaze_hash::HashSet::new_in(Global);
        let off = self.checks_off(body);
        // rustc's RemoveNoopLandingPads: such a pad costs a call LLVM doesn't know resumes.
        let resumes_only = match frame {
            Some(_) => HashSet::new_in(Global),
            None => hir_ty_mir::mir::noop_landing_pads(body),
        };
        // Switches on a value known now, and the one way they go.
        let mut decided = HashMap::new_in(Global);
        let mut worklist = Vec::with_capacity(body.basic_blocks.len());
        worklist.push(body.start_block);
        while let Some(id) = worklist.pop() {
            if !reachable.insert(id) {
                continue;
            }
            match body.basic_blocks[id].terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::SwitchInt { discr, targets }) => {
                    match self.switch_bits(&body.basic_blocks[id], discr, &off) {
                        Some(bits) => {
                            let target = targets.target_for_value(bits);
                            decided.insert(id, target);
                            worklist.push(target)
                        }
                        None => worklist.extend_from_slice(targets.all_targets()),
                    }
                }
                Some(
                    TerminatorKind::Goto { target }
                    | TerminatorKind::Drop { target, .. }
                    | TerminatorKind::DropAndReplace { target, .. }
                    | TerminatorKind::Assert { target, .. }
                    | TerminatorKind::FalseEdge { real_target: target, .. }
                    | TerminatorKind::FalseUnwind { real_target: target, .. },
                ) => worklist.push(*target),
                Some(TerminatorKind::Call { target, .. }) => worklist.extend(*target),
                Some(TerminatorKind::Yield { resume, drop, .. }) => {
                    worklist.push(*resume);
                    worklist.extend(*drop);
                }
                _ => (),
            }
            let block = &body.basic_blocks[id];
            let unwind = block.terminator.as_ref().and_then(|it| it.kind.unwind());
            worklist.extend(unwind.filter(|it| !self.aborts && !resumes_only.contains(it)));
        }
        let state = frame.as_ref().map(|(frame, ..)| frame.state()).map(|it| in_frame(self, it));
        match (frame, state) {
            (Some((_, layout, addr, ty, held)), Some(state)) => {
                let Some(tag) = CoroutineFrame::state_of(&layout) else {
                    unsupported!("an async block without a state");
                };
                let state = SuspendedState { addr: state, ty: self.scalar(tag)?.ty, held };
                self.enter_coroutine(body, &state, addr, ty, &layout)?;
                self.suspended = Some(state);
            }
            _ => {
                self.builder.ins().jump(self.blocks[body.start_block], &[]);
            }
        }
        for (id, block) in body.basic_blocks.iter() {
            if !reachable.contains(&id) {
                continue;
            }
            self.builder.switch_to_block(self.blocks[id]);
            self.block = Some(id);
            self.in_cleanup = block.is_cleanup;
            if block.is_cleanup || self.calls_cold(block.terminator.as_ref().map(|it| &it.kind)) {
                self.builder.set_cold_block(self.blocks[id]);
            }
            for statement in &block.statements {
                self.at(statement.span);
                // No statement unwinds: what panics is an `Assert`.
                self.unwind_to = None;
                match &statement.kind {
                    // A promoted constant already holds its value.
                    StatementKind::Assign(place, _)
                        if self.constant_locals.contains(&place.local) => {}
                    StatementKind::Assign(place, rvalue) => self.assign(place, rvalue)?,
                    StatementKind::Deinit(_) => unsupported!("deinit"),
                    StatementKind::InlineAsm(asm) => self.inline_asm(asm)?,
                    StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::FakeRead(_)
                    | StatementKind::Nop => (),
                }
            }
            let Some(terminator) = &block.terminator else {
                unsupported!("block without terminator");
            };
            self.at(terminator.span);
            self.unwind_to =
                terminator.kind.unwind().filter(|it| !self.aborts && !resumes_only.contains(it));
            let target = match &terminator.kind {
                // A promoted call already holds its value, and that value is read-only.
                TerminatorKind::Call { destination, target: Some(target), .. }
                    if self.constant_locals.contains(&destination.local) =>
                {
                    Some(*target)
                }
                _ => decided.get(&id).copied(),
            };
            match target {
                Some(target) => {
                    self.builder.ins().jump(self.blocks[target], &[]);
                }
                None => self.terminator(&terminator.kind)?,
            }
        }
        self.in_cleanup = false;
        self.unwind_to = None;
        self.emit_pads()?;
        Ok(())
    }

    /// Field types can still be projections such as `<str as ToOwned>::Owned`. Every place of
    /// every statement asks this of its local and of each projection.
    fn normalize(&self, ty: Ty<'db>) -> Ty<'db> {
        hir_ty_mir::mir::normalize_or_keep(&self.infcx, self.env.param_env, ty)
    }

    /// The value of an integer, `bool` or `char` literal operand.
    fn const_bits(&self, operand: &Operand) -> Option<u128> {
        let OperandKind::Constant { konst, ty } = &operand.kind else { return None };
        let ty = ty.as_ref();
        if !matches!(ty.kind(), TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Uint(_)) {
            return None;
        }
        let ConstKind::Value(value) = konst.as_ref().kind() else { return None };
        let layout = self.layout(ty).ok()?;
        Some(value.value.inner().to_leaf().to_bits(layout.size))
    }

    /// The value `block` switches on, when it is a constant: a literal, or an associated
    /// constant such as `T::IS_ZST` that the block has just read. rustc does not compile the
    /// arms such a switch never takes either, and they may not even type check for this `T`.
    fn switch_bits(
        &mut self,
        block: &hir_ty_mir::mir::BasicBlock,
        discr: &Operand,
        off: &HashSet<LocalId, Global>,
    ) -> Option<u128> {
        if let Some(bits) = self.const_bits(discr) {
            return Some(bits);
        }
        let (OperandKind::Copy(place) | OperandKind::Move(place)) = &discr.kind else {
            return None;
        };
        if !place.projection.as_slice().is_empty() {
            return None;
        }
        if off.contains(&place.local) {
            return Some(0);
        }
        let assigned = block.statements.iter().rev().find_map(|it| match &it.kind {
            StatementKind::Assign(to, rvalue) => Some((to, rvalue)),
            _ => None,
        });
        let Some((to, Rvalue::Use(Operand { kind: OperandKind::Constant { konst, ty }, .. }))) =
            assigned
        else {
            return None;
        };
        if to != place || !matches!(ty.as_ref().kind(), TyKind::Bool) {
            return None;
        }
        let ConstKind::Alias(_, unevaluated) = konst.as_ref().kind() else { return None };
        let allocation = self.eval_const(unevaluated).ok()?;
        Some(*allocation.memory.first()? as u128)
    }

    /// The locals of `body` that only a call of a check that is off here assigns, once:
    /// `ub_checks()` and the like without debug assertions are
    /// false, and what they guard is not compiled, nor asks
    /// for the bodies of what it calls (`precondition_check`, the panics), which would else
    /// use up what a compile may build in before the code that runs is reached.
    fn checks_off(&mut self, body: &MirBody<'db>) -> HashSet<LocalId, Global> {
        let db = self.cg.db;
        let mut assigned: HashMap<LocalId, usize, Global> = HashMap::new_in(Global);
        let mut borrowed = HashSet::new_in(Global);
        let mut off = HashSet::new_in(Global);
        for (_, block) in body.basic_blocks.iter() {
            for statement in &block.statements {
                if let StatementKind::Assign(place, rvalue) = &statement.kind {
                    *assigned.entry(place.local).or_default() += 1;
                    if let Rvalue::Ref(_, place) = rvalue {
                        borrowed.insert(place.local);
                    }
                }
            }
            let Some(TerminatorKind::Call { func, destination, intrinsic, .. }) =
                block.terminator.as_ref().map(|it| &it.kind)
            else {
                continue;
            };
            *assigned.entry(destination.local).or_default() += 1;
            let check = match intrinsic {
                Some(Intrinsic::UbChecks) => !self.ub_checks,
                Some(_) => false,
                None => {
                    let Ok(ty) = self.operand_ty(func) else { continue };
                    let TyKind::FnDef(def, _) = ty.kind() else { continue };
                    let CallableDefId::FunctionId(callee) = def.0 else { continue };
                    callee.krate(db).data(db).origin.is_lang()
                        && is_ub_check(FunctionSignature::of(db, callee).name.symbol())
                        && !self.debug_assertions
                }
            };
            if check && destination.projection.as_slice().is_empty() {
                off.insert(destination.local);
            }
        }
        off.retain(|local| assigned.get(local) == Some(&1) && !borrowed.contains(local));
        off
    }

    fn place(&mut self, place: &StoredPlace) -> Result<(Value, PlaceTy<'db>)> {
        let place = self.place_ref(place)?;
        Ok((place.addr, place.ty))
    }

    /// The type of the register a local of this type fits, when it is one a place of has
    /// no part of.
    fn register_ty(&self, ty: Ty<'db>) -> Option<Type> {
        let ty = self.normalize(ty);
        let plain = match ty.kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::RawPtr(..)
            | TyKind::Ref(..)
            | TyKind::FnPtr(..) => true,
            _ => false,
        };
        if !plain {
            return None;
        }
        match self.layout(ty).ok()?.backend_repr {
            BackendRepr::Scalar(scalar) => Some(self.scalar(scalar).ok()?.ty),
            _ => None,
        }
    }

    /// The slot of a local. One kept in a register gets its slot the first time something
    /// needs it in memory, and most never do.
    fn local_slot(&mut self, local: LocalId) -> LocalSlot {
        if let Some(&slot) = self.locals.get(local) {
            return slot;
        }
        let ty = self.body.expect("MIR body").locals[local].ty.as_ref();
        let layout = self.layout(ty).expect("the layout of a local kept in a register");
        let lazy = self.cg.frame_locals.as_ref().and_then(|it| it.lazy.get(&local).copied());
        let addr = match lazy {
            Some(at) if !layout.is_unsized() && layout.align.abi.bytes() <= 16 => {
                let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    layout.size.bytes() as u32,
                    layout.align.abi.bytes().trailing_zeros() as u8,
                ));
                self.cg.frame_locals.as_mut().expect("the frame's locals").slots[at] = Some(slot);
                self.builder.ins().stack_addr(self.cg.target.pointer(), slot, 0)
            }
            _ => self.stack_slot(&layout),
        };
        let slot = LocalSlot { addr, borrowed: false };
        self.locals.insert(local, slot);
        slot
    }

    /// Where a local is, with what its register holds written there first.
    fn local_addr(&mut self, local: LocalId) -> Value {
        let addr = self.local_slot(local).addr;
        let addr = self.slot_addr(addr);
        if let Some(register) = self.registers.get(local) {
            let value = self.builder.use_var(register.var);
            self.builder.ins().store(MEM, value, addr, 0);
        }
        addr
    }

    /// The register an operand is the whole of.
    fn register_of(&self, operand: &Operand) -> Option<Register> {
        let (OperandKind::Copy(place) | OperandKind::Move(place)) = &operand.kind else {
            return None;
        };
        self.whole_register(place)
    }

    fn whole_register(&self, place: &StoredPlace) -> Option<Register> {
        if self.registers.is_empty() || !place.projection.as_slice().is_empty() {
            return None;
        }
        self.registers.get(place.local).copied()
    }

    fn place_ref(&mut self, place: &StoredPlace) -> Result<PlaceRef<'db>> {
        let borrowed = self.locals.get(place.local).is_some_and(|it| it.borrowed);
        // A pointer in a register is where it points with no load.
        let mut pointer = match place.projection.as_slice().first() {
            Some(ProjectionElem::Deref) => self.registers.get(place.local).copied(),
            _ => None,
        };
        let mut addr = match pointer {
            Some(register) => self.builder.use_var(register.var),
            None => self.local_addr(place.local),
        };
        let mut meta = None;
        if borrowed {
            meta = Some(self.builder.ins().load(
                self.cg.target.pointer(),
                MEM,
                addr,
                self.cg.target.meta_at(),
            ));
            addr = self.builder.ins().load(self.cg.target.pointer(), MEM, addr, 0);
        }
        let mut ty = PlaceTy::from_ty(self.body.expect("MIR body").locals[place.local].ty.as_ref());
        ty.ty = self.normalize(ty.ty);
        for proj in place.projection.as_slice() {
            let prev = ty;
            ty = ty.projection_ty(&self.infcx, proj, self.env.param_env);
            // The pointee or field can be `Self::Buffer`, which has to be an array to index.
            ty.ty = self.normalize(ty.ty);
            if self.scalable(prev.ty)?.is_some() && !matches!(proj, ProjectionElem::Deref) {
                unsupported!("a projection into the scalable vector {:?}", prev.ty);
            }
            // A scalable value is only ever in blaze's storage, sized for the largest vector
            // length; through a raw pointer it is anywhere, and rustc reads exactly one vector
            // length of it there.
            if let (ProjectionElem::Deref, TyKind::RawPtr(..)) = (proj, prev.ty.kind())
                && self.scalable(ty.ty)?.is_some()
            {
                unsupported!("a scalable vector read or written through a raw pointer");
            }
            match proj {
                ProjectionElem::Deref => match self.layout(prev.ty)?.backend_repr {
                    BackendRepr::Scalar(_) if pointer.take().is_some() => (),
                    BackendRepr::Scalar(_) => {
                        addr = self.builder.ins().load(self.cg.target.pointer(), MEM, addr, 0);
                        meta = None;
                    }
                    BackendRepr::ScalarPair { .. } if !matches!(prev.ty.kind(), TyKind::Adt(def, _) if def.is_box()) =>
                    {
                        meta = Some(self.builder.ins().load(
                            self.cg.target.pointer(),
                            MEM,
                            addr,
                            self.cg.target.meta_at(),
                        ));
                        addr = self.builder.ins().load(self.cg.target.pointer(), MEM, addr, 0);
                    }
                    BackendRepr::ScalarPair { .. } | BackendRepr::Memory { .. } if matches!(prev.ty.kind(), TyKind::Adt(def, _) if def.is_box()) =>
                    {
                        (addr, meta) = self.box_pointer(addr, prev.ty, ty.ty)?;
                    }
                    _ => unsupported!("deref of {:?}", prev.ty),
                },
                ProjectionElem::Field(field) => {
                    let layout = self.layout(prev.ty)?;
                    let offset = match &layout.variants {
                        // A variant without values has no layout of its own, and code
                        // reading its fields cannot be reached.
                        Variants::Single { .. } | Variants::Empty
                            if field.0 as usize >= layout.fields.count() =>
                        {
                            Size::ZERO
                        }
                        Variants::Single { .. } | Variants::Empty => {
                            layout.fields.offset(field.0 as usize)
                        }
                        // What an `async` block captured: its variants are its own affair.
                        Variants::Multiple { .. }
                            if matches!(prev.ty.kind(), TyKind::Coroutine(..)) =>
                        {
                            layout.fields.offset(field.0 as usize)
                        }
                        Variants::Multiple { variants, .. } => {
                            let Some(VariantId::EnumVariantId(variant)) = prev.variant_id else {
                                unsupported!("field of enum without downcast");
                            };
                            variants[RustcEnumVariantIdx(variant.index(self.cg.db))].field_offsets
                                [RustcFieldIdx::new(field.0 as usize)]
                        }
                    };
                    addr = self.unsized_field_addr(addr, meta, prev.ty, offset, ty.ty)?;
                }
                ProjectionElem::Downcast(_) => (),
                ProjectionElem::Index(local) => {
                    let index = match self.registers.get(*local) {
                        Some(register) => self.builder.use_var(register.var),
                        None => {
                            let index = self.slot_addr(self.locals[*local].addr);
                            self.builder.ins().load(self.cg.target.pointer(), MEM, index, 0)
                        }
                    };
                    addr = self.element(addr, prev.ty, index)?;
                    meta = None;
                }
                ProjectionElem::ConstantIndex { offset, from_end, .. } => {
                    let index = if *from_end {
                        let len = self.len(prev.ty, meta)?;
                        self.builder.ins().iadd_imm(len, -(*offset as i64))
                    } else {
                        self.builder.ins().iconst(self.cg.target.pointer(), *offset as i64)
                    };
                    addr = self.element(addr, prev.ty, index)?;
                    meta = None;
                }
                ProjectionElem::Subslice { from, to } => {
                    let size = self.element_size(prev.ty)?;
                    // FIXME: rustc checks `from + to <= len` when matching the pattern.
                    addr = self.builder.ins().iadd_imm(addr, (*from * size) as i64);
                    meta = match meta {
                        Some(len) => {
                            Some(self.builder.ins().iadd_imm(len, -((*from + *to) as i64)))
                        }
                        None => None,
                    };
                }
                ProjectionElem::ReflectedField(_) => {
                    unsupported!(
                        "{} (in {:?})",
                        hir_ty::mir::ComptimeUnexpanded::new(
                            hir_ty::mir::MirComptimeForm::ReflectedField,
                            "the code generator",
                        ),
                        self.body.map(|it| it.owner),
                    )
                }
            }
        }
        Ok(PlaceRef { addr, meta, ty })
    }

    fn element_size(&self, container: Ty<'db>) -> Result<u64> {
        match container.kind() {
            TyKind::Array(elem, _) | TyKind::Slice(elem) => Ok(self.layout(elem)?.size.bytes()),
            _ => unsupported!("indexing into {container:?}"),
        }
    }

    /// Number of elements of an array, slice or `str` place.
    fn len(&mut self, container: Ty<'db>, meta: Option<Value>) -> Result<Value> {
        match (container.kind(), meta) {
            (TyKind::Array(_, len), _) => {
                let Some(len) = try_const_usize(self.cg.db, len) else {
                    unsupported!("array with unevaluated length");
                };
                Ok(self.builder.ins().iconst(self.cg.target.pointer(), len as i64))
            }
            (TyKind::Slice(_) | TyKind::Str, Some(len)) => Ok(len),
            _ => unsupported!("length of {container:?}"),
        }
    }

    /// Address of `container[index]`: the `Assert` before what reads it has checked the index.
    fn element(&mut self, addr: Value, container: Ty<'db>, index: Value) -> Result<Value> {
        let size = self.element_size(container)?;
        let offset = self.builder.ins().imul_imm(index, size as i64);
        Ok(self.builder.ins().iadd(addr, offset))
    }

    /// Branches to `core::panicking::panic_bounds_check(index, len, location)` when `cond` is set.
    fn bounds_panic_if(&mut self, cond: Value, index: Value, len: Value) -> Result<()> {
        let panic_block = self.builder.create_block();
        let ok_block = self.builder.create_block();
        self.builder.set_cold_block(panic_block);
        self.builder.ins().brif(cond, panic_block, &[], ok_block, &[]);

        self.builder.switch_to_block(panic_block);
        let location = self.caller_location()?;
        self.call_panic(&["panicking", "panic_bounds_check"], &[index, len, location])?;
        self.builder.ins().trap(TrapCode::user(1).expect("non-zero trap code"));

        self.builder.switch_to_block(ok_block);
        Ok(())
    }

    /// Calls the panic function core declares at `path` with `args`, each passed as the type
    /// it has (pointer-sized but for `panic_invalid_enum_construction`'s `u128`): the
    /// function itself, as any other call of it, or by its path when core isn't in the graph.
    fn call_panic(&mut self, path: &[&'static str], args: &[Value]) -> Result<()> {
        let mut sig = self.cg.make_signature();
        let dfg = &self.builder.func.dfg;
        sig.params.extend(args.iter().map(|&it| AbiParam::new(dfg.value_type(it))));
        let callee = match self.core_panic(path) {
            Some(func) => {
                let none = GenericArgs::new_from_slice(&[]);
                self.cg.callee(InstanceDef::Fn(func), none, self.env, &sig)?
            }
            // alloc: only for a crate built without core in its graph, the symbol name imported
            None => self.cg.import_path(&format!("core::{}", path.join("::")), &sig),
        };
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        self.invoke(callee, args);
        Ok(())
    }

    /// [`Self::core_function`] of a panic function, found once per function built: core's
    /// panic functions each have a name of their own, which keys them.
    fn core_panic(&mut self, path: &[&'static str]) -> Option<FunctionId> {
        let name = *path.last()?;
        if let Some(&(_, found)) = self.core_panics.iter().find(|it| it.0 == name) {
            return found;
        }
        let found = self.core_function(path);
        self.core_panics.push((name, found));
        found
    }

    /// Whether a block ending in `terminator` runs only when something has gone wrong: it
    /// calls a [`cold_fn`], or `FromResidual::from_residual` into a `Result`, which is the
    /// `Err` arm of `?`. `?` on an `Option` or a `ControlFlow` is not: a `None` or a `Break`
    /// is as often the answer as the error. Cranelift lays a cold block out after the rest of
    /// its function; LLVM is told so by a branch weight, which a profile's replaces.
    fn calls_cold(&self, terminator: Option<&TerminatorKind>) -> bool {
        let Some(TerminatorKind::Call { func, .. }) = terminator else { return false };
        let OperandKind::Constant { ty, .. } = &func.kind else { return false };
        let TyKind::FnDef(def, args) = ty.as_ref().kind() else { return false };
        let CallableDefId::FunctionId(callee) = def.0 else { return false };
        let db = self.cg.db;
        let lang_items = self.interner.lang_items();
        if Some(callee) == lang_items.TryTraitFromResidual {
            // `<Self as FromResidual<R>>::from_residual`: `Self` is what the `?` returns.
            let result = lang_items.ResultErr.map(|it| AdtId::EnumId(it.lookup(db).parent));
            return matches!(args.skip_binder().type_at(0).kind(), TyKind::Adt(adt, _) if Some(adt.def_id()) == result);
        }
        cold_fn(db, lang_items, callee)
    }

    /// The function core declares at `path`, its modules then its name.
    fn core_function(&self, path: &[&str]) -> Option<FunctionId> {
        let db = self.cg.db;
        let core = self.interner.lang_items().PanicBoundsCheck?.krate(db);
        let (name, modules) = path.split_last()?;
        let mut module = hir_def::nameres::crate_def_map(db, core).root_module_id();
        for it in modules {
            let children = module.def_map(db).module(db, module).children(db);
            module = *children.get(&hir_expand::name::Name::new_root(it))?;
        }
        let scope = module.def_map(db).module(db, module).scope(db);
        match scope.get(&hir_expand::name::Name::new_root(name)).take_values()? {
            hir_def::ModuleDefId::FunctionId(func) => Some(func),
            _ => None,
        }
    }

    fn static_ty(&self, id: StaticId) -> Ty<'db> {
        let db = self.cg.db;
        let ty = InferenceResult::of(db, DefWithBodyId::from(id))
            .expr_ty(Body::of(db, id.into()).root_expr());
        Ty::new_ref(self.interner, Region::new_erased(self.interner), ty, Mutability::Not)
    }

    fn operand_ty(&mut self, operand: &Operand) -> Result<Ty<'db>> {
        Ok(match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place)
                if place.projection.as_slice().is_empty() =>
            {
                let ty = self.body.expect("MIR body").locals[place.local].ty.as_ref();
                self.normalize(ty)
            }
            OperandKind::Copy(place) | OperandKind::Move(place) => self.place(place)?.1.ty,
            OperandKind::Constant { ty, .. } => ty.as_ref(),
            OperandKind::Allocation { allocation } => allocation.as_ref().ty,
            OperandKind::Static(id) => self.static_ty(*id),
        })
    }

    /// Address of memory holding the operand's value. Constants live in read-only data, so the
    /// memory must not be written through.
    fn operand(&mut self, operand: &Operand) -> Result<(Value, Ty<'db>)> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                let (addr, ty) = self.place(place)?;
                if self.given_away.result.is_some() {
                    self.given_away.read.push(addr);
                }
                if let OperandKind::Move(_) = operand.kind
                    && place.projection.as_slice().is_empty()
                    && self.glue_of(ty.ty) != DropGlue::None
                    && self.given_away.result.is_some_and(|it| it != place.local)
                {
                    self.given_away.whole.insert(addr);
                }
                Ok((addr, ty.ty))
            }
            OperandKind::Constant { konst, ty } => {
                let ty = ty.as_ref();
                if let Some((owner, expr)) = self.written_expr(operand.span())
                    && let Some(addr) = self.builtin_location(owner, expr)?
                {
                    return Ok((addr, ty));
                }
                let blob = match konst.as_ref().kind() {
                    ConstKind::Value(value) => {
                        if is_integer(ty) {
                            let layout = self.layout(ty)?;
                            let bits = value.value.inner().to_leaf().to_bits(layout.size);
                            return Ok((self.integer_slot(ty, bits)?, ty));
                        }
                        self.valtree_blob(ty, value.value)?
                    }
                    ConstKind::Alias(_, unevaluated) => {
                        let (allocation, whose) = self.eval_named_const(unevaluated)?;
                        self.allocation_blob(allocation, whose, false)?
                    }
                    kind => unsupported!("constant {kind:?}"),
                };
                Ok((self.blob_or_immediate(ty, blob)?, ty))
            }
            OperandKind::Allocation { allocation } => {
                let allocation = allocation.as_ref();
                let ty = allocation.ty;
                if let Some((owner, expr)) = self.written_expr(operand.span())
                    && let Some(addr) = self.builtin_location(owner, expr)?
                {
                    return Ok((addr, ty));
                }
                let blob = self.allocation_blob(allocation, self.in_place(), false)?;
                Ok((self.blob_or_immediate(ty, blob)?, ty))
            }
            OperandKind::Static(id) => {
                let data = self.cg.static_data(*id);
                let data = self.cg.declare_data_in_func(data, self.builder.func);
                let value = if is_thread_local(self.cg.db, *id) {
                    self.builder.ins().tls_value(self.cg.target.pointer(), data)
                } else {
                    self.builder.ins().symbol_value(self.cg.target.pointer(), data)
                };
                let ty = self.static_ty(*id);
                let layout = self.layout(ty)?;
                let addr = self.stack_slot(&layout);
                self.builder.ins().store(MEM, value, addr, 0);
                Ok((addr, ty))
            }
        }
    }

    /// The value of a static with an initializer, and whether it can be written to.
    fn static_value(&mut self, id: StaticId) -> Result<(ConstBlob, bool)> {
        let db = self.cg.db;
        let flags = StaticSignature::of(db, id).flags;
        if flags.contains(StaticFlags::EXTERN) {
            // A reader that gets here asked for the value of a static that has none.
            unsupported!(
                "the value of `{}`, a static of an `extern` block: it has no initializer, and its \
                 symbol is linked to (`v2::statics::source`)",
                blaze_items::key::path(db, blaze_items::ItemId::Static(id))
            );
        }
        let allocation = db.const_eval_static(id).map_err(|e| {
            refused_const_eval(db, GeneralConstId::StaticId(id), &e)
                // alloc: the error's message
                .unwrap_or_else(|| CodegenError::Frontend(format!("const eval: {e:?}")))
        })?;
        // Nothing can be written to a static of no size.
        let writable = !allocation.memory.is_empty()
            && (flags.contains(StaticFlags::MUTABLE)
                || is_thread_local(db, id)
                || !self.is_freeze(allocation.ty));
        Ok((
            self.allocation_blob(allocation, ConstOf::Item(GeneralConstId::StaticId(id)), true)?,
            writable,
        ))
    }

    /// Whether `ty` is known to have no `UnsafeCell` outside of indirections.
    fn is_freeze(&self, ty: Ty<'db>) -> bool {
        hir_ty::drop::is_freeze(&self.infcx, ty, self.env.param_env)
    }

    /// The parameters of the function being built that are addresses, with what their types
    /// say of the memory there, by their place among the parameters of its IR. As rustc's
    /// `arg_attrs_for_rust_scalar`: a reference is aligned and not null, and a `&mut T` of a
    /// `T` that is `Unpin` and a `&T` of one without an `UnsafeCell` are the only way to their
    /// memory, which may be read at any time; an aggregate passed or returned in memory is at
    /// an address of its own of its size.
    fn pointees(
        &self,
        body: &MirBody<'db>,
        ret: PassMode,
        modes: &[PassMode],
    ) -> Result<Vec<(u32, fragment::Pointee)>> {
        use fragment::Pointee;
        // The body of a coroutine is resumed with a null context too, which it tells apart.
        if body.coroutine.is_some() {
            return Ok(Vec::new()); // alloc: empty, allocates nothing
        }
        let mut found = Vec::with_capacity(modes.len() + 1);
        let mut at = 0u32;
        let in_memory = |layout: &Layout| Pointee {
            unique: false,
            frozen: false,
            size: layout.size.bytes(),
            align: layout.align.abi.bytes(),
        };
        if let PassMode::Indirect = ret {
            let ty = body.locals[returned_local(body)].ty.as_ref();
            found.push((at, in_memory(&*self.layout(ty)?)));
            at += 1;
        }
        let mut params = Vec::with_capacity(4);
        for (&local, &mode) in body.param_locals.iter().zip(modes) {
            let ty = body.locals[local].ty.as_ref();
            let pointee = match mode {
                PassMode::Indirect => Some(in_memory(&*self.layout(ty)?)),
                PassMode::Direct(_) | PassMode::Pair(..) => self.reference_pointee(ty)?,
                _ => None,
            };
            if let Some(pointee) = pointee {
                found.push((at, pointee));
            }
            params.clear();
            self.push_abi_params(mode, &mut params);
            at += params.len() as u32;
        }
        Ok(found)
    }

    /// What a reference of type `ty` says of its memory, if `ty` is one that is the only way
    /// to its memory. Only a reference to plain data is looked at: whether a type with a
    /// coroutine, a closure or a trait object in it is `Unpin` takes more than its fields to
    /// know, and a wrong yes is a miscompile.
    fn reference_pointee(&self, ty: Ty<'db>) -> Result<Option<fragment::Pointee>> {
        let TyKind::Ref(_, pointee, mutability) = ty.kind() else { return Ok(None) };
        let element = match pointee.kind() {
            TyKind::Str => None,
            TyKind::Slice(element) => Some(element),
            _ => Some(pointee),
        };
        let plain = |it: Ty<'db>| {
            hir_ty_share::is_plain_data(
                self.cg.db,
                self.interner,
                &self.infcx,
                self.env.param_env,
                it,
            )
        };
        if !element.is_none_or(plain) {
            return Ok(None);
        }
        let unique = match mutability {
            Mutability::Not => self.is_freeze(pointee),
            Mutability::Mut => self.interner.lang_items().Unpin.is_some_and(|unpin| {
                hir_ty::traits::implements_trait_unique(pointee, self.cg.db, self.env, unpin)
            }),
        };
        if !unique {
            return Ok(None);
        }
        let (size, align) = match pointee.kind() {
            TyKind::Str => (0, 1),
            TyKind::Slice(element) => (0, self.layout(element)?.align.abi.bytes()),
            _ => {
                let layout = self.layout(pointee)?;
                (layout.size.bytes(), layout.align.abi.bytes())
            }
        };
        Ok(Some(fragment::Pointee { unique, frozen: mutability == Mutability::Not, size, align }))
    }

    fn eval_const(&mut self, unevaluated: AliasConst<'db>) -> Result<Allocation<'db>> {
        self.eval_named_const(unevaluated).map(|it| it.0)
    }

    /// [`Self::eval_const`], with the constant it evaluated: a trait's constant resolved to
    /// its impl's.
    fn eval_named_const(
        &mut self,
        unevaluated: AliasConst<'db>,
    ) -> Result<(Allocation<'db>, ConstOf<'db>)> {
        let db = self.cg.db;
        let args = unevaluated.args;
        let frontend = |def: GeneralConstId<'db>, e: ConstEvalError<'db>| {
            refused_const_eval(db, def, &e)
                // alloc: the error's message
                .unwrap_or_else(|| CodegenError::Frontend(format!("const eval: {e:?}")))
        };
        match unevaluated.kind {
            AliasConstKind::Projection { def_id: TraitAssocConstId(id) }
            | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(id) }
            | AliasConstKind::Free { def_id: FreeConstAliasId(id) } => {
                let (id, args) = lookup_impl_const(&self.infcx, self.env.param_env, id, args);
                let allocation =
                    db.const_eval(id, args, Some(self.env)).map_err(|e| frontend(id.into(), e))?;
                Ok((allocation, ConstOf::Item(id.into())))
            }
            AliasConstKind::Anon { def_id: AnonConstIdWrapper(id) } => {
                let allocation = db
                    .anon_const_eval(id, args, Some(self.env))
                    .map_err(|e| frontend(id.into(), e))?;
                Ok((allocation, ConstOf::Item(id.into())))
            }
            AliasConstKind::InherentSelf { .. } => {
                unsupported!("the inherent constant {unevaluated:?} in Self form after type check")
            }
        }
    }

    /// The address of a stack slot holding the integer `bits` of type `ty`.
    fn integer_slot(&mut self, ty: Ty<'db>, bits: u128) -> Result<Value> {
        let layout = self.layout(ty)?;
        let scalar = self.scalar_of(ty)?;
        let addr = self.stack_slot(&layout);
        let value = self.iconst(scalar.ty, bits);
        self.builder.ins().store(MEM, value, addr, 0);
        Ok(addr)
    }

    /// The address of a constant's value: an integer is an immediate, however the MIR gives
    /// it (a value, a literal's allocation or a named constant), so that what runs the code
    /// without its data (wasm-bindgen's interpreter of its descriptors) sees it.
    fn blob_or_immediate(&mut self, ty: Ty<'db>, blob: ConstBlob) -> Result<Value> {
        if is_integer(ty) && blob.relocs.is_empty() && blob.bytes.len() <= 16 {
            let mut bytes = [0; 16];
            bytes[..blob.bytes.len()].copy_from_slice(&blob.bytes);
            return self.integer_slot(ty, u128::from_le_bytes(bytes));
        }
        self.blob_addr(blob)
    }

    fn blob_addr(&mut self, blob: ConstBlob) -> Result<Value> {
        let data = self.cg.define_const(blob)?;
        let data = self.cg.declare_data_in_func(data, self.builder.func);
        Ok(self.builder.ins().symbol_value(self.cg.target.pointer(), data))
    }

    fn valtree_blob(&mut self, ty: Ty<'db>, valtree: ValTree<'db>) -> Result<ConstBlob> {
        let layout = self.layout(ty)?;
        let align = layout.align.abi.bytes();
        let pointee = match ty.kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::RawPtr(..) => {
                let bits = valtree.inner().to_leaf().to_bits(layout.size);
                // alloc: the constant's bytes, owned by the blob `define_const` keeps
                let bytes = bits.to_le_bytes()[..layout.size.bytes_usize()].to_vec();
                return Ok(ConstBlob {
                    bytes,
                    relocs: Vec::new(),    // alloc: empty, allocates nothing
                    fn_relocs: Vec::new(), // alloc: empty, allocates nothing
                    align,
                    type_ids: Vec::new(), // alloc: empty, allocates nothing
                });
            }
            TyKind::Ref(_, pointee, _) => pointee,
            TyKind::Adt(..) | TyKind::Tuple(_) | TyKind::Array(..) => {
                return self.aggregate_valtree_blob(ty, valtree);
            }
            _ => unsupported!("constant of type {ty:?}"),
        };
        // alloc: the constant's bytes, owned by the blob `define_const` keeps
        let mut bytes = vec![0; layout.size.bytes_usize()];
        let target = match pointee.kind() {
            TyKind::Str | TyKind::Slice(_) => {
                let mut target = ConstBlob {
                    // alloc: the pointee's bytes, filled below and kept by `define_const`
                    bytes: Vec::new(),
                    relocs: Vec::new(),    // alloc: empty, allocates nothing
                    fn_relocs: Vec::new(), // alloc: empty, allocates nothing
                    align: 1,
                    type_ids: Vec::new(), // alloc: empty, allocates nothing
                };
                let items = valtree.inner().to_branch();
                for item in items.iter() {
                    let ConstKind::Value(item) = item.kind() else {
                        unsupported!("unevaluated const in a slice constant");
                    };
                    let item = self.valtree_blob(item.ty, item.value)?;
                    if !item.relocs.is_empty() {
                        unsupported!("slice constant of pointers");
                    }
                    target.align = target.align.max(item.align);
                    target.bytes.extend(item.bytes);
                }
                bytes[self.cg.target.word()..2 * self.cg.target.word()]
                    .copy_from_slice(&(items.len() as u64).to_le_bytes()[..self.cg.target.word()]);
                target
            }
            TyKind::Dynamic(..) => unsupported!("`dyn` constant"),
            _ => self.valtree_blob(pointee, valtree)?,
        };
        let target = self.cg.define_const(target)?;
        Ok(ConstBlob {
            bytes,
            relocs: vec![(0, target, 0)], // alloc: the pointer's relocation, owned by the blob
            fn_relocs: Vec::new(),        // alloc: empty, allocates nothing
            align,
            type_ids: Vec::new(), // alloc: empty, allocates nothing
        })
    }

    /// A struct, enum, tuple or array that a const generic argument is (`adt_const_params`):
    /// const eval gives its fields in order, an enum's after the index of its variant.
    fn aggregate_valtree_blob(&mut self, ty: Ty<'db>, valtree: ValTree<'db>) -> Result<ConstBlob> {
        let layout = self.layout(ty)?;
        let mut blob = ConstBlob {
            // alloc: the constant's bytes, owned by the blob `define_const` keeps
            bytes: vec![0; layout.size.bytes_usize()],
            // alloc: empty until a field has a pointer, then owned by the blob
            relocs: Vec::new(),
            // alloc: empty until a field has a function pointer, then owned by the blob
            fn_relocs: Vec::new(),
            align: layout.align.abi.bytes(),
            type_ids: Vec::new(), // alloc: empty, allocates nothing
        };
        let items = valtree.inner().to_branch();
        let (shape, fields) = match ty.kind() {
            TyKind::Adt(def, args) => match def.def_id() {
                AdtId::StructId(id) => (self.variant_shape(id.into(), args)?, &items[..]),
                AdtId::UnionId(_) => unsupported!("a union constant"),
                AdtId::EnumId(id) => {
                    let index = match items.first().map(|it| it.kind()) {
                        Some(ConstKind::Value(it)) => it.value.inner().to_leaf().to_u32() as usize,
                        _ => unsupported!("an enum constant without its variant"),
                    };
                    let variants = &id.enum_variants(self.cg.db).variants;
                    if index >= variants.len() {
                        unsupported!("an enum constant of variant {index} of {}", variants.len());
                    }
                    (self.variant_shape(variants[index].0.into(), args)?, &items[1..])
                }
            },
            _ => (Some((layout, None)), &items[..]),
        };
        let Some((variant, tag)) = shape else {
            unsupported!("a constant of a variant its type has no room for");
        };
        if let Some((offset, tag_ty, tag)) = tag {
            let len = tag_ty.bytes() as usize;
            blob.bytes[offset..offset + len].copy_from_slice(&tag.to_le_bytes()[..len]);
        }
        for (i, field) in fields.iter().enumerate() {
            let ConstKind::Value(field) = field.kind() else {
                unsupported!("an unevaluated constant in a constant");
            };
            let field = self.valtree_blob(field.ty, field.value)?;
            let at = variant.fields.offset(i).bytes_usize();
            let Some(place) = blob.bytes.get_mut(at..at + field.bytes.len()) else {
                unsupported!("a field of a constant out of its bounds");
            };
            place.copy_from_slice(&field.bytes);
            let at = at as u32;
            blob.relocs
                .extend(field.relocs.into_iter().map(|(it, data, add)| (it + at, data, add)));
            blob.fn_relocs.extend(field.fn_relocs.into_iter().map(|(it, func)| (it + at, func)));
        }
        Ok(blob)
    }

    /// `in_static` says the value is a static's, where memory behind a mutable pointer can be
    /// written to; `whose` names the constant in its errors.
    fn allocation_blob(
        &mut self,
        allocation: Allocation<'db>,
        whose: ConstOf<'db>,
        in_static: bool,
    ) -> Result<ConstBlob> {
        let AllocationData { ty, memory, memory_map } = &*allocation;
        let layout = self.layout(*ty)?;
        let size = layout.size.bytes_usize();
        // alloc: the constant's bytes, owned by the blob `define_const` keeps
        let mut bytes = memory.to_vec();
        match (bytes.len(), size) {
            (len, size) if len == size => (),
            // Const eval stores some scalars as 16 bytes.
            (16, size) if size < 16 => bytes.truncate(size),
            (len, 16) if len < 16 => bytes.resize(16, 0),
            _ => unsupported!("constant whose size differs from its layout"),
        }
        let mut blob = ConstBlob {
            bytes,
            // alloc: empty until `relocate` finds a pointer, then owned by the blob
            relocs: Vec::new(),
            // alloc: empty until `relocate` finds a function pointer, then owned by the blob
            fn_relocs: Vec::new(),
            align: layout.align.abi.bytes(),
            // alloc: empty until `relocate` finds a `TypeId`, then owned by the blob
            type_ids: Vec::new(),
        };
        let mut map = Relocations {
            map: memory_map,
            of: *ty,
            whose,
            in_static,
            defined: HashMap::new_in(Global),
            walking: Vec::new(), // alloc: empty until a pointer is followed
            handled: Vec::new(), // alloc: empty until a reference to nothing is found
            numbers: HashMap::new_in(Global),
            unions: Vec::new(), // alloc: empty until a union is walked
        };
        let walk = Walk { kept: None, raw: false };
        self.relocate(&mut blob, walk, 0, *ty, &mut map)?;
        self.relocate_by_provenance(&mut blob, None, Mutability::Not, &mut map)?;
        self.refuse_ids_left(&blob, None, &map)?;
        blob.in_order();
        Ok(blob)
    }

    /// Const eval gives pointers as keys into `map`. Emits each target as its own data object
    /// and records a relocation for the pointer, walking `ty` to find them. `walk` says where
    /// `blob`'s bytes were in const eval and whether a type can be trusted to describe them.
    fn relocate(
        &mut self,
        blob: &mut ConstBlob,
        walk: Walk,
        offset: usize,
        ty: Ty<'db>,
        map: &mut Relocations<'_, 'db>,
    ) -> Result<()> {
        if let MemoryMap::Empty = map.map {
            return Ok(());
        }
        let db = self.cg.db;
        let word = self.cg.target.word();
        match ty.kind() {
            TyKind::Pat(base, _) => self.relocate(blob, walk, offset, base, map)?,
            // A reference and a raw pointer are relocated alike, as rustc relocates every
            // pointer by its provenance, metadata included.
            TyKind::Ref(_, pointee, mutability) | TyKind::RawPtr(pointee, mutability) => {
                self.relocate_pointer(blob, walk, offset, ty, pointee, mutability, map)?
            }
            // Const eval gives function pointers as ids into its table of fn item types. In
            // memory reached through a raw pointer the type may be a cast's, and only a word
            // with provenance is a pointer.
            TyKind::FnPtr(..) => {
                let Some(id) = blob.usize_at(offset, word) else {
                    unsupported!("constant pointer out of bounds");
                };
                let fn_ty = match map.map.function_of(id) {
                    Some(fn_ty) if !walk.raw || map.provenance(walk.word(offset)) => fn_ty,
                    _ if walk.raw || id == 0 => return Ok(()),
                    _ => unsupported!(
                        "a `{}` in a constant whose value is no function const eval made",
                        self.spell(ty)
                    ),
                };
                let target = self.fn_item(fn_ty)?;
                blob.bytes[offset..offset + word].fill(0);
                blob.fn_relocs.push((offset as u32, target));
            }
            // A `DynMetadata` holds a vtable as const eval's id of it, as a `&dyn` does
            // (`ptr::metadata`, reflection's `trait_info_of`), which the walk by provenance
            // relocates: the id names the type and the trait, also for a `DynMetadata` of no
            // `dyn` (`type_id_vtable`'s `DynMetadata<*const ()>`).
            TyKind::Adt(def, args)
                if self
                    .interner
                    .lang_items()
                    .DynMetadata
                    .is_some_and(|it| def.def_id() == AdtId::StructId(it)) =>
            {
                // `_vtable_ptr`, its first field.
                let at = offset + self.layout(ty)?.fields.offset(0).bytes_usize();
                let dyn_ty = self.normalized(args.type_at(0), ty)?;
                let dyn_ty = matches!(dyn_ty.kind(), TyKind::Dynamic(..)).then_some(dyn_ty);
                self.vtable_word(blob, walk, at, ty, dyn_ty, map)?;
            }
            // Const eval gives a `TypeId` as an id into its table of types, which the hash rustc
            // gives the type replaces once the object is stitched.
            TyKind::Adt(def, _)
                if self
                    .interner
                    .lang_items()
                    .TypeId
                    .is_some_and(|it| def.def_id() == AdtId::StructId(it)) =>
            {
                let id = blob.bytes.get(offset..offset + 16).and_then(|it| it.try_into().ok());
                let Some(of) = id.and_then(|it| map.map.type_id_ty(u128::from_le_bytes(it))) else {
                    if walk.raw {
                        return Ok(());
                    }
                    unsupported!("a `TypeId` in a constant that const eval did not make");
                };
                blob.type_ids.push((offset as u32, of.store()));
            }
            TyKind::Adt(def, args) => {
                let layout = self.layout(ty)?;
                let (variant, variant_layout): (VariantId, Cow<'_, Layout>) = match def.def_id() {
                    AdtId::StructId(id) => (id.into(), Cow::Borrowed(layout)),
                    AdtId::UnionId(id) => {
                        return self.relocate_union(blob, walk, offset, ty, id, map);
                    }
                    AdtId::EnumId(id) => {
                        let Some(bytes) =
                            blob.bytes.get(offset..offset + layout.size.bytes_usize())
                        else {
                            unsupported!("constant enum out of bounds");
                        };
                        let Some((variant, layout)) =
                            detect_variant_from_bytes(&layout, db, self.dl(), bytes, id)
                        else {
                            return Ok(());
                        };
                        (variant.into(), layout)
                    }
                };
                let offsets = (0..variant_layout.fields.count())
                    .map(|i| variant_layout.fields.offset(i).bytes_usize());
                for ((_, field), field_offset) in db.field_types(variant).iter().zip(offsets) {
                    let field = field.ty().instantiate(self.interner, args).skip_norm_wip();
                    self.relocate(blob, walk, offset + field_offset, field, map)?;
                }
            }
            TyKind::Tuple(tys) => {
                let layout = self.layout(ty)?;
                for (i, field) in tys.iter().enumerate() {
                    let field_offset = layout.fields.offset(i).bytes_usize();
                    self.relocate(blob, walk, offset + field_offset, field, map)?;
                }
            }
            // A closure's captures, as a struct's fields.
            TyKind::Closure(_, args) => {
                let layout = self.layout(ty)?;
                let upvars = args.as_closure().tupled_upvars_ty();
                for (i, field) in upvars.tuple_fields().iter().enumerate() {
                    let field_offset = layout.fields.offset(i).bytes_usize();
                    self.relocate(blob, walk, offset + field_offset, field, map)?;
                }
            }
            TyKind::Array(elem, len) => {
                let Some(len) = try_const_usize(db, len) else {
                    unsupported!("array with unevaluated length");
                };
                let size = self.layout(elem)?.size.bytes_usize();
                if is_number(elem) {
                    if !walk.raw {
                        map.number(walk.kept, offset, size * len as usize);
                    }
                    return Ok(());
                }
                for i in 0..len as usize {
                    self.relocate(blob, walk, offset + i * size, elem, map)?;
                }
            }
            TyKind::Alias(..) => {
                let normalized = self.normalized(ty, ty)?;
                self.relocate(blob, walk, offset, normalized, map)?;
            }
            // A word a type says is a number is one, whatever its value; behind a raw pointer
            // the type may be a cast's.
            _ if is_number(ty) => {
                if !walk.raw {
                    map.number(walk.kept, offset, self.layout(ty)?.size.bytes_usize());
                }
            }
            _ => (),
        }
        Ok(())
    }

    /// The union `ty` at `offset` in `blob`. Const eval does not say which of its fields it
    /// holds: its provenance says which words are pointers and vtables, and its ids which
    /// function or which type's vtable for which trait, which the walk by provenance after
    /// this one relocates. One whose fields but one are empty (`MaybeUninit<T>`'s `value`,
    /// besides `()`) holds that one or nothing, which is walked as memory reached through a
    /// raw pointer: a word is a pointer or a vtable only with provenance. A map without
    /// provenance (a literal's) holds no union.
    fn relocate_union(
        &mut self,
        blob: &mut ConstBlob,
        walk: Walk,
        offset: usize,
        ty: Ty<'db>,
        id: hir_def::UnionId,
        map: &mut Relocations<'_, 'db>,
    ) -> Result<()> {
        let TyKind::Adt(_, args) = ty.kind() else { return Ok(()) };
        let layout = self.layout(ty)?;
        map.unions.push((walk.kept, offset, offset + layout.size.bytes_usize(), ty));
        let mut held = None;
        for (i, (_, field)) in self.cg.db.field_types(id.into()).iter().enumerate() {
            let field = field.ty().instantiate(self.interner, args).skip_norm_wip();
            let field = self.normalized(field, ty)?;
            if self.layout(field)?.size.bytes() == 0 {
                continue;
            }
            if held.is_some() {
                return Ok(());
            }
            held = Some((layout.fields.offset(i).bytes_usize(), field));
        }
        if let Some((at, field)) = held {
            self.relocate(blob, Walk { raw: true, ..walk }, offset + at, field, map)?;
        }
        Ok(())
    }

    /// A value the MIR of the body being built holds in place, as [`ConstOf`] names it.
    fn in_place(&self) -> ConstOf<'db> {
        ConstOf::InPlace(self.body.map(|it| it.owner))
    }

    /// `ty` as an error spells it: by its path, cut short.
    fn spell(&self, ty: Ty<'db>) -> String {
        use hir_ty::display::HirDisplay as _;
        let target = hir_ty::display::DisplayTarget::from_crate(self.cg.db, self.env.krate);
        ty.display_truncated(self.cg.db, Some(80), target).to_string() // alloc: an error's text
    }

    /// `ty` normalized; one that stays an alias is an error naming `of`, the type it is in.
    fn normalized(&self, ty: Ty<'db>, of: Ty<'db>) -> Result<Ty<'db>> {
        let normalized = self.normalize(ty);
        if matches!(normalized.kind(), TyKind::Alias(..) | TyKind::Infer(..)) {
            unsupported!(
                "`{}` in a constant's `{}` does not normalize",
                self.spell(ty),
                self.spell(of)
            );
        }
        Ok(normalized)
    }
    /// The pointer of type `ty` to `pointee` at `offset` in `blob`, a reference or a raw
    /// pointer: both are relocated as rustc relocates every pointer, by its provenance, the
    /// metadata's too: a vtable is the one const eval's id names, of a type for a trait, which
    /// the walk by provenance relocates; the type here only checks it. The memory pointed to is
    /// walked as the pointee's type, for a raw pointer only where a value of it fits (one past
    /// the end, or cast from another type, it need not), and then by its provenance.
    #[allow(clippy::too_many_arguments, reason = "the walk's state, passed down as `relocate`'s")]
    fn relocate_pointer(
        &mut self,
        blob: &mut ConstBlob,
        walk: Walk,
        offset: usize,
        ty: Ty<'db>,
        pointee: Ty<'db>,
        mutability: Mutability,
        map: &mut Relocations<'_, 'db>,
    ) -> Result<()> {
        let word = self.cg.target.word();
        let raw = matches!(ty.kind(), TyKind::RawPtr(..));
        let Some(addr) = blob.usize_at(offset, word) else {
            unsupported!("constant pointer out of bounds");
        };
        let provenance = map.provenance(walk.word(offset));
        // Behind a raw pointer the type may be a cast's: a word without provenance is a
        // number, its metadata too.
        if walk.raw && !provenance {
            return Ok(());
        }
        let pointee = self.normalized(pointee, ty)?;
        let tail = hir_ty::layout::struct_tail_erasing_lifetimes(self.cg.db, pointee, |it| {
            self.normalize(it)
        });
        let tail = self.normalized(tail, ty)?;
        // A slice's length is a number.
        if !walk.raw && matches!(tail.kind(), TyKind::Str | TyKind::Slice(_)) {
            map.number(walk.kept, offset + word, word);
        }
        // Const eval gives a vtable as its id of the concrete type and the trait, which the
        // walk by provenance relocates; the pointee is walked as that concrete type.
        let mut erased = None;
        if let TyKind::Dynamic(..) = tail.kind() {
            let Some(concrete) =
                self.vtable_word(blob, walk, offset + word, ty, Some(tail), map)?
            else {
                return Ok(());
            };
            erased = Some(concrete);
        }
        // A raw pointer whose provenance is a function or a vtable keeps const eval's id of it
        // (a function cast to a pointer, a `&dyn`'s metadata transmuted to a `*const ()`):
        // rustc keeps a raw pointer to either in a constant's value, so it points at that
        // function or vtable, which the id names. A reference holding one is refused below as
        // dangling: it has no memory const eval kept.
        if raw && provenance {
            if let Some(fn_ty) = map.map.function_of(addr) {
                let target = self.fn_item(fn_ty)?;
                blob.bytes[offset..offset + word].fill(0);
                blob.fn_relocs.push((offset as u32, target));
                return Ok(());
            }
            if let Some((concrete, dyn_ty)) = map.map.vtable_of(addr) {
                let vtable = self.vtable_by_id(concrete, dyn_ty, offset, walk.kept)?;
                blob.point_to(offset, vtable, 0, word);
                return Ok(());
            }
        }
        if let Some((id, at)) = map.map.static_at(addr) {
            let target = self.cg.static_data(id);
            blob.point_to(offset, target, at, word);
            return Ok(());
        }
        // What is walked by type: the concrete type behind a `dyn`, else the pointee.
        let walked = match (pointee.kind(), erased) {
            (TyKind::Dynamic(..), Some(erased)) => erased,
            _ => pointee,
        };
        // A slice's or `str`'s length, or a struct's whose tail is one.
        let len = match tail.kind() {
            TyKind::Str | TyKind::Slice(_) => {
                let Some(len) = blob.usize_at(offset + word, word) else {
                    unsupported!("constant slice length out of bounds");
                };
                Some(len)
            }
            _ => None,
        };
        // A reference one past its memory's end is to nothing, and taken below to be that.
        let holding = if raw { memory_holding(map.map, addr) } else { map.map.memory_at(addr) };
        // A reference to nothing (no bytes, or an empty slice of no memory const eval kept) is
        // its alignment, as rustc makes a reference to a zero-sized allocation.
        let empty = match holding {
            _ if raw => false,
            Some((_, memory)) if memory.is_empty() => true,
            _ if matches!(walked.kind(), TyKind::Str | TyKind::Slice(_)) => len == Some(0),
            Some(_) => false,
            None => self.layout(walked).is_ok_and(|it| it.is_sized() && it.size.bytes() == 0),
        };
        // An empty slice into memory const eval kept points into it, where it is: its address
        // is the allocation's, as rustc's provenance keeps it (`a[1..1]`, `a[a.len()..]`).
        if empty
            && provenance
            && map.tracks_provenance()
            && let Some((start, memory)) = map.map.kept_around(addr)
            && !memory.is_empty()
        {
            let target = self.walk_allocation(start, memory, None, mutability, map)?;
            blob.point_to(offset, target, addr - start, word);
            return Ok(());
        }
        if empty {
            let align = match walked.kind() {
                TyKind::Str => 1,
                TyKind::Slice(elem) => self.layout(elem)?.align.abi.bytes(),
                _ => self.layout(walked)?.align.abi.bytes(),
            };
            blob.bytes[offset..offset + word].copy_from_slice(&align.to_le_bytes()[..word]);
            map.handled.push(walk.word(offset));
            return Ok(());
        }
        let Some((start, memory)) = holding else {
            // A pointer with provenance to memory const eval did not keep would be a dangling
            // pointer, which rustc refuses in a constant's value; without it, a number.
            if provenance && map.tracks_provenance() {
                unsupported!(
                    "a `{}` in a constant (at {addr:#x}) to memory const eval did not keep: \
                     rustc refuses a dangling pointer in a constant's value",
                    self.spell(ty)
                );
            }
            return Ok(());
        };
        let at = addr - start;
        let tail = erased.filter(|_| !matches!(pointee.kind(), TyKind::Dynamic(..)));
        let by = Reach { ty: walked, pointee, at, raw: walk.raw || raw, len, tail };
        let target = self.walk_allocation(start, memory, Some(by), mutability, map)?;
        blob.point_to(offset, target, at, word);
        Ok(())
    }

    /// The concrete type of the vtable whose id is the word at `at` of `blob`, the metadata of
    /// a `ty`, a pointer to `dyn_ty` or a `DynMetadata` of it (`None`: of no `dyn`): what const
    /// eval's id names where its provenance says the word is a vtable, as rustc's
    /// `GlobalAlloc::VTable(ty, dyn_ty)`. The walk by provenance relocates the word from the id,
    /// which names the trait; the type is only checked against it, as rustc's validity check
    /// refuses a vtable of another trait than the pointer's (`wrong trait in wide pointer
    /// vtable`). Behind a raw pointer, whose type may be a cast's, a word that is no vtable is
    /// `None`, and the trait is not checked; elsewhere either is an error naming the type.
    #[allow(clippy::too_many_arguments, reason = "the walk's state, passed down as `relocate`'s")]
    fn vtable_word(
        &self,
        blob: &ConstBlob,
        walk: Walk,
        at: usize,
        ty: Ty<'db>,
        dyn_ty: Option<Ty<'db>>,
        map: &Relocations<'_, 'db>,
    ) -> Result<Option<Ty<'db>>> {
        let id = blob.usize_at(at, self.cg.target.word());
        let entry =
            id.and_then(|id| map.map.vtable_of(id)).filter(|_| map.provenance(walk.word(at)));
        let Some((concrete, of)) = entry else {
            if walk.raw {
                return Ok(None);
            }
            unsupported!(
                "a `{}` in a constant whose metadata (at {at} of {:x?}) is no vtable const eval \
                 made: its word is {}",
                self.spell(ty),
                walk.kept,
                match id.map(|id| map.map.vtable_ty(id)) {
                    Some(Ok(_)) if !map.provenance(walk.word(at)) =>
                        "an id with no provenance, a vtable made a number",
                    Some(Ok(_)) => "the id of a function",
                    _ => "no id const eval gave",
                }
            );
        };
        if let Some(dyn_ty) = dyn_ty
            && !walk.raw
            && !hir_ty::mir::same_vtable_trait(
                DbInterner::new_with(self.cg.db, self.env.krate),
                of,
                dyn_ty,
            )
        {
            unsupported!(
                "a `{}` in {} holds the vtable of `{}` for `{}`, where its type says \
                 `{}`: rustc refuses a vtable of another trait in a constant's value",
                self.spell(ty),
                map.whose.describe(self.cg.db),
                self.spell(concrete),
                self.spell(of),
                self.spell(dyn_ty)
            );
        }
        Ok(Some(concrete))
    }

    /// The data object of the allocation const eval kept at `start`, `memory`, its pointers
    /// relocated: walked as `by` says, a type at an offset, where it is reached by a pointer
    /// of a type, then by its provenance. One allocation is one data object however many
    /// pointers reach it; one reached as two types that relocate it differently (a vtable a
    /// pointer of one type says is there and one of another does not) is an error.
    fn walk_allocation(
        &mut self,
        start: usize,
        memory: &[u8],
        by: Option<Reach<'db>>,
        mutability: Mutability,
        map: &mut Relocations<'_, 'db>,
    ) -> Result<DataId> {
        let first = map.defined.get(&start).map(|it| (it.data, it.by));
        match first {
            // A walk by provenance finds no more than one by type, which walks by provenance
            // after it.
            Some((data, _)) if by.is_none() => return Ok(data),
            Some((data, first)) if first.is_some() && first == by => return Ok(data),
            _ => (),
        }
        if map.walking.contains(&start) {
            unsupported!(
                "a pointer in a constant that points back into memory it is in (at {start:#x}): \
                 const eval's memory of a constant points in no circle"
            );
        }
        let Some(align) = map.align_of(start) else {
            unsupported!(
                "the memory {} (of `{}`) points to (const eval kept it at {start:#x}, {} \
                 bytes) has no alignment const eval recorded: blaze does not guess one, which \
                 could under-align it",
                map.whose.describe(self.cg.db),
                self.spell(map.of),
                memory.len()
            );
        };
        let mut target = ConstBlob {
            // alloc: the pointee's bytes, owned by the blob `define_data` keeps
            bytes: memory.to_vec(),
            // alloc: empty until a pointer is found in it, then owned by the blob
            relocs: Vec::new(),
            // alloc: empty until a function pointer is found in it, then owned by the blob
            fn_relocs: Vec::new(),
            align,
            // alloc: empty until a `TypeId` is found in it, then owned by the blob
            type_ids: Vec::new(),
        };
        map.walking.push(start);
        if let Some(by) = by {
            let walk = Walk { kept: Some(start), raw: by.raw };
            let at = by.at;
            let fits = |size: usize| at.checked_add(size).is_some_and(|end| end <= memory.len());
            match by.ty.kind() {
                TyKind::Str => (),
                TyKind::Slice(elem) => {
                    let size = self.layout(elem)?.size.bytes_usize();
                    let len = by.len.unwrap_or(0);
                    if !by.raw || size.checked_mul(len).is_some_and(fits) {
                        for i in 0..len {
                            self.relocate(&mut target, walk, at + i * size, elem, map)?;
                        }
                    }
                }
                _ if !by.raw => self.relocate(&mut target, walk, at, by.ty, map)?,
                // A raw pointer's pointee need not have a layout (an extern type), and then
                // only its provenance says what is behind it. One into memory that is an array
                // of its pointee, as `as_ptr` makes it, reaches every element: each is walked,
                // at the pointee's stride, as memory a raw pointer reaches.
                _ => {
                    let layout = self.layout(by.ty).ok().filter(|it| it.is_sized());
                    let size = layout.map(|it| it.size.bytes_usize());
                    match size {
                        Some(size)
                            if size > 0
                                && !is_number(by.ty)
                                && at % size == 0
                                && memory.len() % size == 0 =>
                        {
                            for pos in (0..memory.len()).step_by(size) {
                                self.relocate(&mut target, walk, pos, by.ty, map)?;
                            }
                        }
                        Some(size) if fits(size) => {
                            self.relocate(&mut target, walk, at, by.ty, map)?
                        }
                        _ => (),
                    }
                }
            }
            // A struct or tuple whose tail is a slice: its elements, after the sized fields.
            if let Some(len) = by.len
                && !matches!(by.ty.kind(), TyKind::Str | TyKind::Slice(_))
                && let Some((tail, elem)) = self.slice_tail(by.pointee)?
            {
                let size = self.layout(elem)?.size.bytes_usize();
                let whole = size.checked_mul(len).and_then(|it| it.checked_add(tail));
                if is_number(elem) {
                    if !by.raw {
                        map.number(walk.kept, at + tail, size * len);
                    }
                } else if !by.raw || whole.is_some_and(fits) {
                    for i in 0..len {
                        self.relocate(&mut target, walk, at + tail + i * size, elem, map)?;
                    }
                }
            }
            // A struct or tuple with a `dyn` tail: the tail is its concrete type, placed by
            // the rule const eval and code identity share.
            if let Some(erased) = by.tail {
                let align = self.layout(erased)?.align.abi.bytes_usize();
                let size = self.layout(erased)?.size.bytes_usize();
                let place = hir_ty::layout::dyn_tail_place(
                    self.cg.db,
                    by.pointee,
                    align,
                    |it| self.normalized(it, by.pointee),
                    |it| self.layout(it),
                )?;
                if let Some((tail, _)) = place
                    && (!by.raw || fits(tail + size))
                {
                    self.relocate(&mut target, walk, at + tail, erased, map)?;
                }
            }
        }
        self.relocate_by_provenance(&mut target, Some(start), mutability, map)?;
        self.refuse_ids_left(&target, Some(start), map)?;
        map.walking.pop();
        target.in_order();
        if let Some(first) = map.defined.get(&start) {
            if first.blob == target {
                return Ok(first.data);
            }
            let name = |it: Option<Reach<'db>>| match it {
                // alloc: error path
                Some(it) => format!("`{}` at {}", self.spell(it.ty), it.at),
                None => "memory of no type".to_owned(), // alloc: error path
            };
            let differs = Self::first_difference(&first.blob, &target);
            unsupported!(
                "one allocation of a constant (at {start:#x}) is reached as {} and as {}, which \
                 relocate it differently ({differs}): one type says a word is a `TypeId` or a \
                 reference to nothing that the other does not, which no provenance says",
                name(first.by),
                name(by)
            );
        }
        let writable = map.in_static && mutability == Mutability::Mut;
        let kept = target.clone(); // clone: compared with a later walk of the same allocation
        let data = self.cg.define_data(target, writable)?;
        map.defined.insert(start, Defined { data, by, blob: kept });
        Ok(data)
    }

    /// Where the slice at the end of `ty`'s chain of last fields (through structs and tuples)
    /// starts in a value of it, and its element type; `None` when the tail is no slice. A
    /// slice's alignment is its element's, which the layout of `ty` places it at.
    fn slice_tail(&self, ty: Ty<'db>) -> Result<Option<(usize, Ty<'db>)>> {
        let (mut at, mut offset) = (ty, 0);
        // A type's chain of last fields is as deep as the type: fuel for a cycle a bug makes.
        for _ in 0..256 {
            let (index, last) = match at.kind() {
                TyKind::Slice(elem) => return Ok(Some((offset, elem))),
                TyKind::Adt(def, args) => {
                    let AdtId::StructId(id) = def.def_id() else { return Ok(None) };
                    let fields = self.cg.db.field_types(id.into());
                    let Some((_, last)) = fields.iter().next_back() else { return Ok(None) };
                    let last = last.ty().instantiate(self.interner, args).skip_norm_wip();
                    (fields.iter().count() - 1, last)
                }
                TyKind::Tuple(tys) => match tys.iter().next_back() {
                    Some(last) => (tys.len() - 1, last),
                    None => return Ok(None),
                },
                _ => return Ok(None),
            };
            offset += self.layout(at)?.fields.offset(index).bytes_usize();
            at = self.normalized(last, ty)?;
        }
        unsupported!("the tail of `{}` is more than 256 fields deep", self.spell(ty));
    }

    /// Where two blobs of one allocation first differ, for an error to say.
    fn first_difference(a: &ConstBlob, b: &ConstBlob) -> String {
        let at = |blob: &ConstBlob, offset: u32| {
            let reloc = blob.relocs.iter().find(|it| it.0 == offset);
            let func = blob.fn_relocs.iter().find(|it| it.0 == offset);
            let type_id = blob.type_ids.iter().any(|it| it.0 == offset);
            match (reloc, func, type_id) {
                (Some(_), ..) => "a pointer",
                (_, Some(_), _) => "a function",
                (_, _, true) => "a `TypeId`",
                _ => "a number",
            }
        };
        // alloc: error path, the offsets either blob relocates
        let mut offsets: Vec<u32> = (a.relocs.iter().map(|it| it.0))
            .chain(b.relocs.iter().map(|it| it.0))
            .chain(a.fn_relocs.iter().chain(&b.fn_relocs).map(|it| it.0))
            .chain(a.type_ids.iter().chain(&b.type_ids).map(|it| it.0))
            .collect();
        offsets.sort_unstable();
        offsets.dedup();
        for offset in offsets {
            let (x, y) = (at(a, offset), at(b, offset));
            let same = (a.relocs.iter().find(|it| it.0 == offset))
                == (b.relocs.iter().find(|it| it.0 == offset))
                && x == y;
            if !same {
                // alloc: error path
                return format!("the word at {offset} is {x} as the first and {y} as the second");
            }
        }
        "their bytes differ".to_owned() // alloc: error path
    }

    /// Points each word of `target` that is a pointer with provenance and that no walk by
    /// type relocated at what it points to, as rustc relocates an allocation by its
    /// provenance whatever type reaches it: memory behind a raw pointer cast from another
    /// type, the rest of an allocation a reference points into, a union. `kept` is where
    /// `target` was: the memory const eval kept at that address, or the constant's value.
    fn relocate_by_provenance(
        &mut self,
        target: &mut ConstBlob,
        kept: Option<usize>,
        mutability: Mutability,
        map: &mut Relocations<'_, 'db>,
    ) -> Result<()> {
        let word = self.cg.target.word();
        let pointers = match kept {
            Some(start) => map.map.pointers_in(start..start + target.bytes.len()),
            None => map.map.value_pointers(),
        };
        if pointers.is_empty() {
            return Ok(());
        }
        // alloc: the pointers' offsets in `target`, copied out of the map while it is walked
        let pointers: Vec<usize> = pointers.iter().map(|it| it - kept.unwrap_or(0)).collect();
        let done = Self::relocated(target, word);
        let walk = Walk { kept, raw: true };
        for at in pointers {
            if done.binary_search(&(at as u32)).is_ok() || map.handled.contains(&walk.word(at)) {
                continue;
            }
            let Some(addr) = target.usize_at(at, word) else { continue };
            // A vtable: its id names the concrete type and the `dyn` type, as rustc's
            // `GlobalAlloc::VTable(ty, dyn_ty)`, whatever type the word is reached as.
            if let Some((concrete, dyn_ty)) = map.map.vtable_of(addr) {
                let vtable = self.vtable_by_id(concrete, dyn_ty, at, kept)?;
                target.point_to(at, vtable, 0, word);
                continue;
            }
            if let Some(of) = map.map.function_of(addr) {
                let func = self.fn_item(of)?;
                target.bytes[at..at + word].fill(0);
                target.fn_relocs.push((at as u32, func));
                continue;
            }
            if let Ok(of) = map.map.vtable_ty(addr) {
                unsupported!(
                    "the word at {at} of a constant's memory ({kept:x?}) is const eval's id of \
                     `{}`, which is no function and names no trait of a vtable",
                    self.spell(of)
                );
            }
            if let Some((id, offset)) = map.map.static_at(addr) {
                let data = self.cg.static_data(id);
                target.point_to(at, data, offset, word);
                continue;
            }
            let Some((start, memory)) = memory_holding(map.map, addr) else {
                unsupported!(
                    "a pointer in a constant (at {at} of {kept:x?}) to memory const eval did not \
                     keep ({addr:#x}): rustc refuses a dangling pointer in a constant's value"
                );
            };
            let data = self.walk_allocation(start, memory, None, mutability, map)?;
            target.point_to(at, data, addr - start, word);
        }
        Ok(())
    }

    /// The data object of the vtable const eval's id names, of `concrete` for `dyn_ty`, held by
    /// the word at `at` of the memory at `kept` (`None`: the constant's value): rustc's
    /// `GlobalAlloc::VTable(ty, dyn_ty)`. One for what is no `dyn` type is an error.
    fn vtable_by_id(
        &mut self,
        concrete: Ty<'db>,
        dyn_ty: Ty<'db>,
        at: usize,
        kept: Option<usize>,
    ) -> Result<DataId> {
        if !matches!(dyn_ty.kind(), TyKind::Dynamic(..)) {
            unsupported!(
                "the vtable of `{}` in a constant's memory (at {at} of {kept:x?}) is for `{}`, \
                 which is no `dyn` type",
                self.spell(concrete),
                self.spell(dyn_ty)
            );
        }
        let trait_ref = self.principal(dyn_ty, concrete);
        self.vtable(concrete, trait_ref)
    }

    /// The offsets in `blob` of the words already relocated, sorted: pointers, functions and
    /// the words of a `TypeId`, which hold const eval's id for its type.
    fn relocated(blob: &ConstBlob, word: usize) -> Vec<u32> {
        // alloc: the offsets already relocated, sorted, for one allocation's walk
        let mut done: Vec<u32> = (blob.relocs.iter().map(|it| it.0))
            .chain(blob.fn_relocs.iter().map(|it| it.0))
            .chain(
                (blob.type_ids.iter())
                    .flat_map(|it| (0..16).step_by(word).map(move |at| it.0 + at as u32)),
            )
            .collect();
        done.sort_unstable();
        done
    }

    /// Refuses a word of `blob` left equal to one of const eval's ids of a function or a
    /// vtable: a pointer no type of the constant and no provenance said was there (every
    /// function and vtable const eval makes has provenance), which would be emitted as a
    /// number. The ids are far from any number a program plausibly holds
    /// (`VTableMap::FIRST_ID`).
    fn refuse_ids_left(
        &self,
        blob: &ConstBlob,
        kept: Option<usize>,
        map: &Relocations<'_, 'db>,
    ) -> Result<()> {
        let MemoryMap::Complex(_) = map.map else { return Ok(()) };
        let word = self.cg.target.word();
        let done = Self::relocated(blob, word);
        for at in (0..blob.bytes.len().saturating_sub(word - 1)).step_by(word) {
            if done.binary_search(&(at as u32)).is_ok() {
                continue;
            }
            let Some(id) = blob.usize_at(at, word) else { continue };
            if id < hir_ty::mir::VTableMap::FIRST_ID || map.is_number(kept, at, word) {
                continue;
            }
            if let Ok(of) = map.map.vtable_ty(id) {
                let place = match kept {
                    Some(start) => format!("memory (at {start:#x})"), // alloc: error path
                    None => "value".to_owned(),                       // alloc: error path
                };
                let why = match map.union_at(kept, at) {
                    // alloc: error path
                    Some(union) => format!(
                        "it is in the union `{}`, whose field holding it const eval does not say",
                        self.spell(union)
                    ),
                    None => "it has no provenance, so const eval did not make it a pointer: a \
                             number equal to an id, or a function or vtable pointer made a number"
                        .to_owned(), // alloc: error path
                };
                unsupported!(
                    "the word at {at} of a constant's {place} is const eval's id of `{}` (a \
                     function or a vtable), which no type of the constant says is a pointer: \
                     {why}",
                    self.spell(of)
                );
            }
        }
        Ok(())
    }

    fn iconst(&mut self, ty: Type, bits: u128) -> Value {
        if ty.bits() < 64 {
            // Cranelift wants narrow immediates without stray high bits.
            let bits = bits as u64 & ((1 << ty.bits()) - 1);
            return self.builder.ins().iconst(ty, bits as i64);
        }
        if ty == types::I128 {
            let lo = self.builder.ins().iconst(types::I64, bits as u64 as i64);
            let hi = self.builder.ins().iconst(types::I64, (bits >> 64) as u64 as i64);
            self.builder.ins().iconcat(lo, hi)
        } else {
            self.builder.ins().iconst(ty, bits as u64 as i64)
        }
    }

    /// Whether `block` is the `asm!` of `panic_unwind`'s `throw_exception` on Windows: it
    /// lays out the `_ThrowInfo` of a Rust panic (the C++ runtime's description of the
    /// exception) in `.rdata` with image-relative offsets, and gives its address. Its operands
    /// are that address (out), `exception_cleanup`, `TYPE_DESCRIPTOR`, the size of `Exception`
    /// and `exception_copy`.
    fn is_seh_throw_info(&self, block: &hir_ty_mir::mir::InlineAsm) -> bool {
        use hir_ty_mir::mir::AsmOperand::{Const, Reg, Sym};
        self.cg.target.object_format == ObjectFormat::Coff
            && !block.naked
            && matches!(
                &*block.operands,
                [Reg { output: Some(_), input: None, .. }, Sym(_), Sym(_), Const(_), Sym(_)]
            )
    }

    /// That block, as code. A static `_ThrowInfo` holds image-relative offsets, which an
    /// assembled block cannot have (Cranelift has no such relocation), so they are written
    /// when the exception is thrown, to a static of the object: the runtime reads it again
    /// when the exception is caught or dropped, after this frame is gone.
    fn seh_throw_info(&mut self, block: &hir_ty_mir::mir::InlineAsm, size: i128) -> Result<()> {
        use hir_ty_mir::mir::AsmOperand;
        let [
            AsmOperand::Reg { output: Some(output), .. },
            AsmOperand::Sym(cleanup),
            AsmOperand::Sym(type_desc),
            _,
            AsmOperand::Sym(copy),
        ] = &*block.operands
        else {
            unsupported!("the `asm!` of `throw_exception`");
        };
        let OperandKind::Static(type_desc) = &type_desc.kind else {
            unsupported!("the `asm!` of `throw_exception`");
        };
        let mut function = |this: &mut Self, operand: &Operand| -> Result<Value> {
            let ty = this.operand_ty(operand)?;
            let id = this.fn_item(ty)?;
            let id = this.cg.declare_func_address_in_func(id, this.builder.func);
            Ok(this.builder.ins().func_addr(this.cg.target.pointer(), id))
        };
        let (cleanup, copy) = (function(self, cleanup)?, function(self, copy)?);
        let type_desc = self.cg.static_data(*type_desc);
        let type_desc = self.cg.declare_data_in_func(type_desc, self.builder.func);
        let type_desc = self.builder.ins().symbol_value(self.cg.target.pointer(), type_desc);
        // 14 words: `_ThrowInfo` (4), `_CatchableTypeArray` (2) and `_CatchableType` (7).
        let info = self.cg.define_data(
            ConstBlob {
                // alloc: once, for `panic_unwind`'s `throw_exception`; the static's bytes
                bytes: vec![0; 56],
                relocs: Vec::new(),    // alloc: empty, allocates nothing
                fn_relocs: Vec::new(), // alloc: empty, allocates nothing
                align: 8,
                type_ids: Vec::new(), // alloc: empty, allocates nothing
            },
            true,
        )?;
        let info = self.cg.declare_data_in_func(info, self.builder.func);
        let info = self.builder.ins().symbol_value(self.cg.target.pointer(), info);
        // The image's base is what `GetModuleHandleW(NULL)` gives.
        let mut sig = self.cg.make_signature();
        sig.params.push(AbiParam::new(self.cg.target.pointer()));
        sig.returns.push(AbiParam::new(self.cg.target.pointer()));
        let handle = self.cg.import("GetModuleHandleW", &sig)?;
        let handle = self.cg.declare_func_in_func(handle, self.builder.func);
        let null = self.builder.ins().iconst(self.cg.target.pointer(), 0);
        let call = self.builder.ins().call(handle, &[null]);
        let base = self.builder.inst_results(call)[0];
        let b = &mut self.builder;
        let rva = |b: &mut FunctionBuilder, address: Value| {
            let offset = b.ins().isub(address, base);
            b.ins().ireduce(types::I32, offset)
        };
        let (array, catchable) = (b.ins().iadd_imm(info, 16), b.ins().iadd_imm(info, 24));
        let words = [
            (0, b.ins().iconst(types::I32, 0)),
            (4, rva(b, cleanup)),
            (8, b.ins().iconst(types::I32, 0)),
            (12, rva(b, array)),
            (16, b.ins().iconst(types::I32, 1)),
            (20, rva(b, catchable)),
            (24, b.ins().iconst(types::I32, 0)),
            (28, rva(b, type_desc)),
            (32, b.ins().iconst(types::I32, 0)),
            (36, b.ins().iconst(types::I32, -1)),
            (40, b.ins().iconst(types::I32, 0)),
            (44, b.ins().iconst(types::I32, size as i64)),
            (48, rva(b, copy)),
        ];
        for (offset, word) in words {
            b.ins().store(MEM, word, info, offset);
        }
        let (addr, _) = self.place(output)?;
        self.builder.ins().store(MEM, info, addr, 0);
        Ok(())
    }

    /// Calls the function that an `asm!` block is assembled as: see [`asm`].
    fn inline_asm(&mut self, block: &hir_ty_mir::mir::InlineAsm) -> Result<()> {
        use hir_ty_mir::mir::AsmOperand;
        let seh = self.is_seh_throw_info(block);
        let mut operands = Vec::with_capacity(block.operands.len());
        let mut syms = Vec::with_capacity(block.operands.len());
        for operand in &block.operands {
            syms.push(match operand {
                AsmOperand::Sym(_) if !block.naked && !seh => unsupported!("`asm!` with a `sym`"),
                AsmOperand::Sym(Operand { kind: OperandKind::Static(id), .. }) => {
                    if is_thread_local(self.cg.db, *id) {
                        unsupported!("inline assembly with a `sym` that is thread local");
                    }
                    Some(ModuleRelocTarget::user(1, self.cg.static_data(*id).as_u32()))
                }
                AsmOperand::Sym(function) => {
                    let ty = self.operand_ty(function)?;
                    Some(ModuleRelocTarget::user(0, self.fn_item(ty)?.as_u32()))
                }
                AsmOperand::Reg { .. } | AsmOperand::Const(_) | AsmOperand::Label => None,
            });
            operands.push(match operand {
                AsmOperand::Sym(_) => asm::Operand::Sym,
                AsmOperand::Reg { reg, input, output, written } => {
                    let mut size = None;
                    let tys = [
                        input.as_ref().map(|it| self.operand_ty(it)).transpose()?,
                        output.as_ref().map(|it| self.place(it).map(|it| it.1.ty)).transpose()?,
                    ];
                    for ty in tys.into_iter().flatten() {
                        let bytes = self.layout(ty)?.size.bytes() as u32;
                        if size.is_some_and(|it| it != bytes) {
                            unsupported!(
                                "an `asm!` register that goes in and comes out in two sizes"
                            );
                        }
                        size = Some(bytes);
                    }
                    asm::Operand::Reg {
                        reg,
                        size,
                        input: input.is_some(),
                        output: output.is_some(),
                        written: *written,
                    }
                }
                AsmOperand::Const(value) => {
                    // A literal, or the anonymous constant MIR makes of what else it is.
                    let ty = self.operand_ty(value)?;
                    let bits = match &value.kind {
                        OperandKind::Allocation { allocation }
                            if matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_)) =>
                        {
                            let mut bytes = [0; 16];
                            let memory = &allocation.as_ref().memory;
                            bytes[..memory.len()].copy_from_slice(memory);
                            Some(u128::from_le_bytes(bytes))
                        }
                        OperandKind::Constant { konst, .. }
                            if matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_))
                                && let ConstKind::Alias(_, named) = konst.as_ref().kind() =>
                        {
                            let mut bytes = [0; 16];
                            let memory = &self.eval_const(named)?.memory;
                            bytes[..memory.len()].copy_from_slice(memory);
                            Some(u128::from_le_bytes(bytes))
                        }
                        _ => self.const_bits(value),
                    };
                    let Some(bits) = bits else {
                        unsupported!("an `asm!` constant that is not a literal");
                    };
                    let unused = 128 - self.layout(ty)?.size.bits() as u32;
                    asm::Operand::Const(match ty.kind() {
                        TyKind::Int(_) => (bits << unused) as i128 >> unused,
                        _ => bits as i128,
                    })
                }
                AsmOperand::Label => asm::Operand::Label,
            });
        }
        if seh {
            let Some(asm::Operand::Const(size)) = operands.get(3) else {
                unsupported!("the `asm!` of `throw_exception`");
            };
            return self.seh_throw_info(block, *size);
        }
        let arch = self.cg.target.arch;
        let features = self.body.and_then(|it| it.owner.as_function());
        let features = features.map(|func| AttrFlags::target_features(self.cg.db, func));
        let extensions =
            asm::extensions(arch, features.into_iter().flatten().map(|it| it.as_str()));
        if block.naked {
            let text = asm::naked(arch, &block.template, block.options, &operands)
                .map_err(CodegenError::Unsupported)?;
            self.cg.naked = Some((extensions + &text, syms));
            return Ok(());
        }
        let mut wrapper = asm::wrapper(&self.cg.target, &block.template, block.options, &operands)
            .map_err(CodegenError::Unsupported)?;
        // LLVM is not given the features `extensions` has the assembler take.
        let inline = extensions.is_empty().then(|| {
            asm::inline(
                arch,
                &block.template,
                block.options,
                &operands,
                &wrapper.offsets,
                block.clobber_abi,
            )
        });
        wrapper.text.insert_str(0, &extensions);
        asm::keep_inline(&wrapper.text, inline.flatten());
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            wrapper.slot_size,
            4,
        ));
        let slot = self.builder.ins().stack_addr(self.cg.target.pointer(), slot, 0);
        for (operand, offset) in block.operands.iter().zip(&wrapper.offsets) {
            if let (AsmOperand::Reg { input: Some(input), .. }, Some(offset)) = (operand, offset) {
                let (addr, ty) = self.operand(input)?;
                let size = self.layout(ty)?.size;
                let to = self.builder.ins().iadd_imm(slot, *offset as i64);
                self.copy(to, addr, size);
            }
        }
        let mut sig = self.cg.make_signature();
        sig.params.push(AbiParam::new(self.cg.target.pointer()));
        if block.taken.is_some() {
            sig.returns.push(AbiParam::new(types::I64));
        }
        let callee = self.cg.assembly(wrapper.text, &sig);
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        let call = self.builder.ins().call(callee, &[slot]);
        if let Some(taken) = &block.taken {
            let taken = self.place(taken)?.0;
            let which = self.builder.inst_results(call)[0];
            self.builder.ins().store(MEM, which, taken, 0);
        }
        for (operand, offset) in block.operands.iter().zip(&wrapper.offsets) {
            if let (AsmOperand::Reg { output: Some(output), .. }, Some(offset)) = (operand, offset)
            {
                let (addr, ty) = self.place(output)?;
                let size = self.layout(ty.ty)?.size;
                let from = self.builder.ins().iadd_imm(slot, *offset as i64);
                self.copy(addr, from, size);
            }
        }
        Ok(())
    }

    /// The value of an integer operand that an intrinsic takes as an immediate, which rustc
    /// requires to be a constant, as `core::arch` writes one: a literal (`cmpps(a, b, 0)`,
    /// which comes as the bytes const eval made of it), an inline or named constant
    /// (`const { IMM5 as u8 }`), or a local assigned once from one, through an `as` between
    /// integers (`IMM5 as i8`). The bits are those of the operand's type.
    fn immediate(&mut self, operand: &Operand) -> Result<Option<u128>> {
        if let Some((bits, _)) = self.literal_bits(operand) {
            return Ok(Some(bits));
        }
        let ty = self.operand_ty(operand)?;
        if !matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_)) {
            return Ok(None);
        }
        let bits = self.layout(ty)?.size.bits();
        let mask = u128::MAX >> (128 - bits);
        match &operand.kind {
            OperandKind::Constant { konst, .. } => {
                let ConstKind::Alias(_, named) = konst.as_ref().kind() else {
                    return Ok(None);
                };
                let allocation = self.eval_const(named)?;
                let Some(memory) = allocation.memory.get(..bits as usize / 8) else {
                    return Ok(None);
                };
                let mut bytes = [0; 16];
                bytes[..memory.len()].copy_from_slice(memory);
                Ok(Some(u128::from_le_bytes(bytes)))
            }
            OperandKind::Copy(_) | OperandKind::Move(_) => {
                let Some(Rvalue::Use(from) | Rvalue::Cast(CastKind::IntToInt, from, _)) =
                    self.assigned_once(operand)
                else {
                    return Ok(None);
                };
                let from_ty = self.operand_ty(from)?;
                let Some(value) = self.immediate(from)? else { return Ok(None) };
                // An `as` to a wider type extends by the sign of the narrower; one to an
                // arbitrary width is normalized to it.
                let unused = 128 - self.layout(from_ty)?.size.bits() as u32;
                let value = match from_ty.kind() {
                    TyKind::Int(_) => ((value << unused) as i128 >> unused) as u128,
                    _ => value,
                };
                Ok(Some(NarrowInt::of(ty).map_or(value & mask, |int| int.normalize_bits(value))))
            }
            _ => Ok(None),
        }
    }

    /// What the local `operand` reads is assigned, where the body assigns it once and whole.
    fn assigned_once(&self, operand: &Operand) -> Option<&'db Rvalue> {
        let (OperandKind::Copy(place) | OperandKind::Move(place)) = &operand.kind else {
            return None;
        };
        if !place.projection.as_slice().is_empty() {
            return None;
        }
        let mut assigned = self.body?.basic_blocks.iter().flat_map(|(_, block)| {
            block.statements.iter().filter_map(|it| match &it.kind {
                StatementKind::Assign(to, rvalue) if to.local == place.local => Some((to, rvalue)),
                _ => None,
            })
        });
        match (assigned.next(), assigned.next()) {
            (Some((to, rvalue)), None) if to == place => Some(rvalue),
            _ => None,
        }
    }

    fn load_scalar(&mut self, operand: &Operand) -> Result<(Value, AbiScalar)> {
        if let Some(register) = self.register_of(operand) {
            let ty = self.operand_ty(operand)?;
            return Ok((self.builder.use_var(register.var), self.scalar_of(ty)?));
        }
        if let Some((bits, ty)) = self.literal_bits(operand) {
            let scalar = self.scalar_of(ty)?;
            return Ok((self.iconst(scalar.ty, bits), scalar));
        }
        let (addr, ty) = self.operand(operand)?;
        let scalar = self.scalar_of(ty)?;
        Ok((self.builder.ins().load(scalar.ty, MEM, addr, 0), scalar))
    }

    /// The value of an operand that is an integer, `bool` or `char` whose bytes are known
    /// here, which is read without being put in memory first. `line!()` and `column!()` are
    /// literals whose bytes are placeholders.
    fn literal_bits(&self, operand: &Operand) -> Option<(u128, Ty<'db>)> {
        let ty = match &operand.kind {
            OperandKind::Constant { ty, .. } => ty.as_ref(),
            OperandKind::Allocation { allocation } => allocation.as_ref().ty,
            _ => return None,
        };
        if !matches!(ty.kind(), TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Uint(_)) {
            return None;
        }
        if let Some((owner, expr)) = self.written_expr(operand.span())
            && self.location_macro(owner, expr).is_some()
        {
            return None;
        }
        let OperandKind::Allocation { allocation } = &operand.kind else {
            return Some((self.const_bits(operand)?, ty));
        };
        let memory = &allocation.as_ref().memory;
        let size = self.layout(ty).ok()?.size.bytes_usize();
        // Const eval stores some scalars as 16 bytes.
        if memory.len() != size && memory.len() != 16 {
            return None;
        }
        let mut bytes = [0; 16];
        bytes[..size].copy_from_slice(&memory[..size]);
        Some((u128::from_le_bytes(bytes), ty))
    }

    fn copy(&mut self, dst: Value, src: Value, size: Size) {
        // A temporary and the binding it initializes are one slot.
        if dst == src {
            return;
        }
        // A load and a store per word of a future holding the whole program's state is
        // more IR than fits in memory.
        if size.bytes() > 128 {
            let config = self.cg.target_config();
            let bytes = self.builder.ins().iconst(self.cg.target.pointer(), size.bytes() as i64);
            self.builder.call_memcpy(config, dst, src, bytes);
            return;
        }
        self.copy_in(dst, src, size, &[types::I64, types::I32, types::I16, types::I8]);
    }

    /// Copies a value of `layout`. A SIMD vector, or an array (the lanes of one), goes in whole
    /// vectors of 16 bytes: it is written and read by single loads and stores of 16 bytes,
    /// and a load the processor can't forward from one earlier store waits for the stores to
    /// reach the cache (sha2's SHA-NI rounds ran at a ninth of rustc's unoptimized build's
    /// speed). Anything else goes in words, as its fields are written.
    fn copy_value(&mut self, dst: Value, src: Value, layout: &Layout) {
        if !self.copies_in_vectors(layout)
            || dst == src
            || !self.reaches_vector(dst)
            || !self.reaches_vector(src)
        {
            return self.copy(dst, src, layout.size);
        }
        self.copy_in(dst, src, layout.size, &[types::I8X16]);
    }

    /// Whether `copy_value` copies a value of `layout` in vectors of 16 bytes.
    fn copies_in_vectors(&self, layout: &Layout) -> bool {
        matches!(
            (&layout.backend_repr, &layout.fields),
            (BackendRepr::SimdVector { .. }, _) | (_, rustc_abi::FieldsShape::Array { .. })
        ) && layout.size.bytes() % 16 == 0
            && layout.size.bytes() <= 128
            && self.makes_vectors()
    }

    /// Whether loads and stores of 16 bytes at `addr` and the 16-byte steps after it are
    /// reached by an immediate offset. AArch64 reaches a slot of the frame from `sp`, and
    /// Cranelift reaches an offset a 16-byte access can't encode (one past -256..256 that is
    /// not a multiple of 16) by a register extended by `sxtx`, which valgrind can't decode
    /// (it stops the program with SIGILL). A slot at an offset that is a multiple of 16 is
    /// reached by a scaled immediate.
    fn reaches_vector(&self, addr: Value) -> bool {
        if self.cg.target.arch != Arch::Aarch64 {
            return true;
        }
        let func = &self.builder.func;
        let constant = |value: Value| match func.dfg.value_def(func.dfg.resolve_aliases(value)) {
            ir::ValueDef::Result(inst, _) => match func.dfg.insts[inst] {
                ir::InstructionData::UnaryImm { opcode: ir::Opcode::Iconst, imm } => {
                    Some(imm.bits())
                }
                _ => None,
            },
            _ => None,
        };
        let (mut addr, mut offset) = (addr, 0i64);
        loop {
            let ir::ValueDef::Result(inst, _) = func.dfg.value_def(func.dfg.resolve_aliases(addr))
            else {
                return true;
            };
            match func.dfg.insts[inst] {
                ir::InstructionData::StackAddr {
                    opcode: ir::Opcode::StackAddr,
                    stack_slot,
                    offset: at,
                } => {
                    let aligned = func.sized_stack_slots[stack_slot].align_shift >= 4;
                    return aligned && (offset + i64::from(at)) % 16 == 0;
                }
                ir::InstructionData::Binary { opcode: ir::Opcode::Iadd, args: [a, b] } => {
                    let (base, by) = match (constant(a), constant(b)) {
                        (_, Some(by)) => (a, by),
                        (Some(by), _) => (b, by),
                        _ => return true,
                    };
                    offset = offset.wrapping_add(by);
                    addr = base;
                }
                _ => return true,
            }
        }
    }

    /// Stores `lanes` of `lane` to `dst` as one vector of 16 bytes, when they are 16 bytes and
    /// the target has vectors, so that a read of the whole vector is forwarded from the store
    /// (see `copy_value`). Says whether it did.
    fn store_whole(&mut self, lanes: &[Value], lane: Type, dst: Value) -> bool {
        let bits = lane.bits() as usize;
        if lanes.len() * bits != 128 || bits > 64 || !self.makes_vectors() {
            return false;
        }
        if !self.reaches_vector(dst) {
            return false;
        }
        let int = Type::int(bits as u16).expect("a lane of 8 to 64 bits");
        let mut words = smallvec::SmallVec::<[Value; 2]>::new();
        for word in lanes.chunks(64 / bits) {
            let mut out = self.builder.ins().iconst(types::I64, 0);
            for (i, &value) in word.iter().enumerate() {
                let mut value = value;
                if lane.is_float() {
                    value = self.builder.ins().bitcast(int, MemFlagsData::new(), value);
                }
                if bits < 64 {
                    value = self.builder.ins().uextend(types::I64, value);
                }
                if i > 0 {
                    value = self.builder.ins().ishl_imm(value, (i * bits) as i64);
                }
                out = self.builder.ins().bor(out, value);
            }
            words.push(out);
        }
        let whole = self.builder.ins().iconcat(words[0], words[1]);
        let flags = MemFlagsData::new().with_endianness(ir::Endianness::Little);
        let vector = self.builder.ins().bitcast(types::I8X16, flags, whole);
        self.builder.ins().store(MEM, vector, dst, 0);
        true
    }

    /// Copies `size` bytes in loads and stores of `types`, each as many times as fits.
    fn copy_in(&mut self, dst: Value, src: Value, size: Size, types: &[Type]) {
        let mut offset = 0i32;
        let mut left = size.bytes() as i32;
        for &ty in types {
            while left >= ty.bytes() as i32 {
                let value = self.builder.ins().load(ty, MEM, src, offset);
                self.builder.ins().store(MEM, value, dst, offset);
                offset += ty.bytes() as i32;
                left -= ty.bytes() as i32;
            }
        }
    }

    fn assign(&mut self, place: &StoredPlace, rvalue: &Rvalue) -> Result<()> {
        let Some(register) = self.whole_register(place) else {
            return self.assign_to_slot(place, rvalue, None, &Cell::new(false));
        };
        if let Rvalue::Use(operand) = rvalue
            && let Ok(scalar) = self.operand_ty(operand).and_then(|ty| self.scalar_of(ty))
            && scalar.ty == register.ty
        {
            let (value, _) = self.load_scalar(operand)?;
            self.builder.def_var(register.var, value);
            return Ok(());
        }
        let registered = Cell::new(false);
        self.assign_to_slot(place, rvalue, Some(register), &registered)?;
        if !registered.get() {
            let addr = self.local_slot(place.local).addr;
            let addr = self.slot_addr(addr);
            let value = self.builder.ins().load(register.ty, MEM, addr, 0);
            self.builder.def_var(register.var, value);
        }
        Ok(())
    }

    /// Assigns through memory, or to the register when the value is one: `registered` is
    /// set then, and the slot of the local is left as it was.
    fn assign_to_slot(
        &mut self,
        place: &StoredPlace,
        rvalue: &Rvalue,
        register: Option<Register>,
        registered: &Cell<bool>,
    ) -> Result<()> {
        if self.locals.get(place.local).is_some_and(|it| it.borrowed)
            && place.projection.as_slice().is_empty()
        {
            let Rvalue::Use(operand) = rvalue else {
                unsupported!("unsized local assigned {rvalue:?}");
            };
            let (OperandKind::Copy(from) | OperandKind::Move(from)) = &operand.kind else {
                unsupported!("unsized local assigned a constant");
            };
            let PlaceRef { addr, meta: Some(meta), .. } = self.place_ref(from)? else {
                unsupported!("unsized local assigned a place without metadata");
            };
            // Marks the place as moved out of.
            self.operand(operand)?;
            let slot = self.slot_addr(self.locals[place.local].addr);
            self.builder.ins().store(MEM, addr, slot, 0);
            self.builder.ins().store(MEM, meta, slot, self.cg.target.meta_at());
            return Ok(());
        }
        let (dst, dst_ty) = match register {
            // What the register holds is about to be replaced, and is not written back.
            Some(_) => {
                let ty = self.body.expect("MIR body").locals[place.local].ty.as_ref();
                let addr = self.local_slot(place.local).addr;
                let addr = self.slot_addr(addr);
                (addr, PlaceTy::from_ty(self.normalize(ty)))
            }
            None => self.place(place)?,
        };
        let store = |this: &mut Self, value: Value| match register {
            Some(register) if this.builder.func.dfg.value_type(value) == register.ty => {
                this.builder.def_var(register.var, value);
                registered.set(true);
            }
            _ => {
                this.builder.ins().store(MEM, value, dst, 0);
            }
        };
        match rvalue {
            Rvalue::Use(operand) => {
                let (src, ty) = self.operand(operand)?;
                let layout = self.layout(ty)?;
                self.copy_value(dst, src, &layout);
            }
            Rvalue::CopyForDeref(place) => {
                let (src, ty) = self.place(place)?;
                let layout = self.layout(ty.ty)?;
                self.copy_value(dst, src, &layout);
            }
            Rvalue::Ref(_, place) => {
                let place = self.place_ref(place)?;
                store(self, place.addr);
                if let BackendRepr::ScalarPair { .. } = self.layout(dst_ty.ty)?.backend_repr {
                    let Some(meta) = place.meta else {
                        unsupported!("fat reference to a place without metadata");
                    };
                    self.builder.ins().store(MEM, meta, dst, self.cg.target.meta_at());
                }
            }
            Rvalue::Len(place) => {
                let place = self.place_ref(place)?;
                let len = self.len(place.ty.ty, place.meta)?;
                store(self, len);
            }
            Rvalue::Discriminant(place) => {
                let (addr, ty) = self.place(place)?;
                let to = self.scalar_of(dst_ty.ty)?;
                let value = self.discriminant(addr, ty.ty, to)?;
                store(self, value);
            }
            Rvalue::Repeat(operand, len) => {
                let Some(len) = try_const_usize(self.cg.db, len.as_ref()) else {
                    unsupported!("repeat with unevaluated length");
                };
                let (src, ty) = self.operand(operand)?;
                let size = self.layout(ty)?.size;
                if len == 0 || size.bytes() == 0 {
                    // No element, or elements that take no room: nothing is written. A value
                    // `[value; 0]` does not take is dropped by the MIR before.
                } else if len <= 16 {
                    for i in 0..len as u64 {
                        let element = self.builder.ins().iadd_imm(dst, (i * size.bytes()) as i64);
                        self.copy(element, src, size);
                    }
                } else {
                    let header = self.builder.create_block();
                    let body = self.builder.create_block();
                    let done = self.builder.create_block();
                    self.builder.append_block_param(header, self.cg.target.pointer());
                    let zero = self.builder.ins().iconst(self.cg.target.pointer(), 0);
                    self.builder.ins().jump(header, &[zero.into()]);
                    self.builder.switch_to_block(header);
                    let index = self.builder.block_params(header)[0];
                    let more =
                        self.builder.ins().icmp_imm(IntCC::UnsignedLessThan, index, len as i64);
                    self.builder.ins().brif(more, body, &[], done, &[]);
                    self.builder.switch_to_block(body);
                    let offset = self.builder.ins().imul_imm(index, size.bytes() as i64);
                    let element = self.builder.ins().iadd(dst, offset);
                    self.copy(element, src, size);
                    let next = self.builder.ins().iadd_imm(index, 1);
                    self.builder.ins().jump(header, &[next.into()]);
                    self.builder.switch_to_block(done);
                }
            }
            Rvalue::BinaryOp(
                op @ (BinOp::AddWithOverflow | BinOp::SubWithOverflow | BinOp::MulWithOverflow),
                lhs,
                rhs,
            ) => {
                let (value, overflow) = self.overflowing_op(op, lhs, rhs)?;
                let Some(overflow) = overflow else { unsupported!("{op:?} of floats") };
                let layout = self.layout(dst_ty.ty)?;
                let [value_at, flag_at] =
                    [0, 1].map(|field| layout.fields.offset(field).bytes() as i32);
                self.builder.ins().store(MEM, value, dst, value_at);
                self.builder.ins().store(MEM, overflow, dst, flag_at);
            }
            Rvalue::BinaryOp(op, lhs, rhs) => {
                let value = self.binary_op(op, lhs, rhs)?;
                store(self, value);
            }
            Rvalue::UnaryOp(op, operand) => {
                let (value, _) = self.load_scalar(operand)?;
                let ty = self.operand_ty(operand)?;
                let is_bool = matches!(ty.kind(), TyKind::Bool);
                let narrow = NarrowInt::of(ty);
                let value = match op {
                    UnOp::Not if is_bool => self.builder.ins().bxor_imm(value, 1),
                    UnOp::Not => match narrow {
                        Some(int) => self.narrow_not(value, int),
                        None => self.builder.ins().bnot(value),
                    },
                    UnOp::Neg if f16_f128::soft(self.builder.func.dfg.value_type(value)) => {
                        self.soft_neg(value)
                    }
                    UnOp::Neg if self.builder.func.dfg.value_type(value).is_float() => {
                        self.builder.ins().fneg(value)
                    }
                    UnOp::Neg => {
                        let ty = self.builder.func.dfg.value_type(value);
                        let zero = self.builder.ins().iconst(types::I64, 0);
                        let zero = match ty {
                            types::I128 => self.builder.ins().uextend(ty, zero),
                            types::I64 => zero,
                            _ => self.builder.ins().ireduce(ty, zero),
                        };
                        // An `Assert` before it has ruled out `MIN`, or it wraps to `MIN` at
                        // an arbitrary width as at a standard one.
                        let value = self.builder.ins().isub(zero, value);
                        self.normalize_if(value, narrow)
                    }
                };
                store(self, value);
            }
            Rvalue::Cast(kind, operand, target) => {
                self.cast(kind, operand, target.as_ref(), dst)?
            }
            Rvalue::Aggregate(kind, operands) => match kind {
                AggregateKind::Tuple(ty) => {
                    let layout = self.layout(ty.as_ref())?;
                    self.write_fields(dst, &layout, operands)?;
                }
                AggregateKind::Adt(variant, args) => {
                    if let Some(layout) = self.variant_layout(dst, *variant, args.as_ref())? {
                        self.write_fields(dst, &layout, operands)?;
                    }
                }
                AggregateKind::Array(elem) => {
                    let size = self.layout(elem.as_ref())?.size;
                    let scalar = self.scalar_of(elem.as_ref()).ok().filter(|it| {
                        u64::from(it.ty.bytes()) == size.bytes()
                            && size.bytes() * operands.len() as u64 == 16
                    });
                    if let Some(scalar) = scalar {
                        let mut lanes = smallvec::SmallVec::<[Value; 16]>::new();
                        for operand in operands.iter() {
                            let (src, _) = self.operand(operand)?;
                            lanes.push(self.builder.ins().load(scalar.ty, MEM, src, 0));
                        }
                        if self.store_whole(&lanes, scalar.ty, dst) {
                            return Ok(());
                        }
                    }
                    for (i, operand) in operands.iter().enumerate() {
                        let (src, _) = self.operand(operand)?;
                        let element =
                            self.builder.ins().iadd_imm(dst, (i as u64 * size.bytes()) as i64);
                        self.copy(element, src, size);
                    }
                }
                AggregateKind::Union(..) => {
                    let [operand] = &**operands else {
                        unsupported!("union aggregate without exactly one field");
                    };
                    let (src, ty) = self.operand(operand)?;
                    let size = self.layout(ty)?.size;
                    self.copy(dst, src, size);
                }
                AggregateKind::Closure(ty) => {
                    let layout = self.layout(ty.as_ref())?;
                    self.write_fields(dst, &layout, operands)?;
                    if let TyKind::Coroutine(..) = ty.as_ref().kind() {
                        self.start_coroutine(dst, &layout)?;
                    }
                }
            },
            _ => unsupported!("rvalue {rvalue:?}"),
        }
        Ok(())
    }

    fn cast(
        &mut self,
        kind: &CastKind,
        operand: &Operand,
        to_ty: Ty<'db>,
        dst: Value,
    ) -> Result<()> {
        let (src, from_ty) = self.operand(operand)?;
        let from = self.layout(from_ty)?;
        let to = self.layout(to_ty)?;
        match kind {
            // RA records an unsizing coercion on both a block and its tail expression; the
            // outer one has nothing left to do.
            CastKind::PointerCoercion(PointerCast::Unsize) if from_ty == to_ty => {
                self.copy(dst, src, to.size)
            }
            CastKind::PointerCoercion(PointerCast::Unsize) => {
                self.unsize(src, from_ty, dst, to_ty)?
            }
            CastKind::PointerCoercion(
                PointerCast::ReifyFnPointer | PointerCast::ClosureFnPointer(_),
            ) => {
                let callee = self.fn_item(from_ty)?;
                let callee = self.cg.declare_func_address_in_func(callee, self.builder.func);
                let pointer = self.builder.ins().func_addr(self.cg.target.pointer(), callee);
                self.builder.ins().store(MEM, pointer, dst, 0);
            }
            CastKind::PointerCoercion(
                PointerCast::MutToConstPointer
                | PointerCast::ArrayToPointer
                | PointerCast::UnsafeFnPointer,
            ) => self.copy(dst, src, to.size),
            CastKind::IntToInt
            | CastKind::PtrToPtr
            | CastKind::PointerExposeAddress
            | CastKind::PointerFromExposedAddress
            | CastKind::FnPtrToPtr => match (from.backend_repr, to.backend_repr) {
                (BackendRepr::Scalar(from), BackendRepr::Scalar(to)) => {
                    let (from, to) = (self.scalar(from)?, self.scalar(to)?);
                    let value = self.builder.ins().load(from.ty, MEM, src, 0);
                    let value = self.int_cast(value, from, to);
                    let value = self.narrow_cast(value, from_ty, from, to_ty);
                    self.builder.ins().store(MEM, value, dst, 0);
                }
                // Fat to fat keeps the metadata, fat to thin drops it.
                (
                    BackendRepr::ScalarPair { .. },
                    BackendRepr::ScalarPair { .. } | BackendRepr::Scalar(_),
                ) => self.copy(dst, src, to.size),
                _ => unsupported!("cast from {from_ty:?} to {to_ty:?}"),
            },
            CastKind::IntToFloat | CastKind::FloatToInt | CastKind::FloatToFloat => {
                let (from, to) = (self.scalar_of(from_ty)?, self.scalar_of(to_ty)?);
                let value = self.builder.ins().load(from.ty, MEM, src, 0);
                let value = self.float_cast(value, from, to)?;
                // Saturated at the storage's bounds, which an arbitrary width's are inside.
                let value = match NarrowInt::of(to_ty) {
                    Some(int) => self.narrow_saturate(value, int),
                    None => value,
                };
                self.builder.ins().store(MEM, value, dst, 0);
            }
            // The bits of the one as the other (rustc's `CastKind::Transmute`), of one size.
            CastKind::Transmute if from.size == to.size => self.copy(dst, src, to.size),
            CastKind::Transmute => {
                unsupported!("transmute of {from_ty:?} to {to_ty:?}, of another size")
            }
            _ => unsupported!("cast {kind:?}"),
        }
        Ok(())
    }

    /// The function a fn item type names, declared so its address can be taken.
    fn fn_item(&mut self, fn_ty: Ty<'db>) -> Result<FuncId> {
        if let TyKind::Closure(..) = fn_ty.kind() {
            return self.closure_fn_pointer(fn_ty);
        }
        let TyKind::FnDef(def, generic_args) = fn_ty.kind() else {
            unsupported!("fn pointer to {fn_ty:?}");
        };
        let generic_args = generic_args.skip_binder();
        let CallableDefId::FunctionId(func) = def.0 else {
            return self.reified(fn_ty, false);
        };
        let db = self.cg.db;
        if FunctionSignature::is_intrinsic(db, func) {
            unsupported!("fn pointer to an intrinsic");
        }
        if is_dyn_method(self.interner, self.env.param_env, func, generic_args).is_some() {
            return self.reified(fn_ty, false);
        }
        let (callee, callee_args) =
            impl_method(db, self.env, func, generic_args).map_err(CodegenError::Refused)?;
        let callee = match callee {
            either::Either::Left(callee) => callee,
            either::Either::Right(derived) => {
                let derived = unexpanded_derive(db, derived, generic_args.type_at(0));
                unsupported!("fn pointer to {derived}");
            }
        };
        // The function takes its caller's location as well, which the shim supplies.
        if track_caller(db, callee) {
            return self.reified(fn_ty, false);
        }
        let sig = self.interner.instantiate_bound_regions_with_erased(fn_ty.fn_sig(self.interner));
        let inputs = Self::abi_inputs(sig.abi(), sig.inputs())?;
        let abi = self.fn_abi(sig.abi(), sig.output(), inputs.iter().copied())?;
        let mut sig = self.signature(&abi.args, abi.ret);
        self.declare_variadic(callee, &mut sig)?;
        self.cg.callee(InstanceDef::Fn(callee), callee_args, self.env, &sig)
    }

    /// Makes `sig`, of the named parameters of `func`, the one `func` is defined with when it
    /// takes `...` and is compiled here: every call of it declares it the same.
    fn declare_variadic(&self, func: FunctionId, sig: &mut Signature) -> Result<()> {
        let signature = FunctionSignature::of(self.cg.db, func);
        if signature.is_varargs()
            && signature.has_body()
            && let Some(variadic) = Variadic::of(self.cg.target, sig)?
        {
            variadic.rest(sig, self.cg.target.pointer());
        }
        Ok(())
    }

    /// The discriminant of the enum at `addr`, as a `to`.
    fn discriminant(&mut self, addr: Value, ty: Ty<'db>, to: AbiScalar) -> Result<Value> {
        let db = self.cg.db;
        let TyKind::Adt(def, _) = ty.kind() else {
            return Ok(self.iconst(to.ty, 0));
        };
        let AdtId::EnumId(id) = def.def_id() else {
            return Ok(self.iconst(to.ty, 0));
        };
        let layout = self.layout(ty)?;
        let enum_variants = id.enum_variants(db);
        let discr_of = |index: usize| {
            db.const_eval_discriminant(enum_variants.variants[index].0)
                // alloc: the error's message
                .map_err(|e| CodegenError::Frontend(format!("discriminant: {e:?}")))
        };
        match &layout.variants {
            // There is no value to read it from, so this cannot be reached; rustc leaves the
            // result undefined.
            Variants::Empty => Ok(self.iconst(to.ty, 0)),
            Variants::Single { index } => {
                let discr = discr_of(index.0)?;
                Ok(self.iconst(to.ty, discr as u128))
            }
            Variants::Multiple { tag, tag_encoding, tag_field, variants } => {
                let tag = self.scalar(*tag)?;
                let offset = layout.fields.offset(tag_field.index()).bytes() as i32;
                let value = self.builder.ins().load(tag.ty, MEM, addr, offset);
                match tag_encoding {
                    TagEncoding::Direct => Ok(self.int_cast(value, tag, to)),
                    TagEncoding::Niche { untagged_variant, niche_variants, niche_start } => {
                        for (index, _) in variants.iter_enumerated() {
                            if discr_of(index.0)? != index.0 as i128 {
                                unsupported!("niche enum with explicit discriminants");
                            }
                        }
                        let first = niche_variants.start.0;
                        let count = niche_variants.last.0 - first;
                        let start = self.iconst(tag.ty, *niche_start);
                        let relative = self.builder.ins().isub(value, start);
                        let is_niche = self.builder.ins().icmp_imm(
                            IntCC::UnsignedLessThanOrEqual,
                            relative,
                            count as i64,
                        );
                        // The index is worked out in a register and widened last: an
                        // `i128` of it is two of every instruction.
                        let narrow = match to.ty {
                            types::I128 if tag.ty != types::I128 => {
                                AbiScalar { ty: types::I64, signed: false, ..to }
                            }
                            _ => to,
                        };
                        let relative =
                            self.int_cast(relative, AbiScalar { signed: false, ..tag }, narrow);
                        let niche = self.builder.ins().iadd_imm(relative, first as i64);
                        let untagged = self.iconst(narrow.ty, untagged_variant.0 as u128);
                        let index = self.builder.ins().select(is_niche, niche, untagged);
                        Ok(self.int_cast(index, narrow, to))
                    }
                }
            }
        }
    }

    /// Layout whose `fields` describe `variant`. Writes the tag for multi-variant enums.
    /// `None` for a variant the layout has no room for, which is one without values: code
    /// building it cannot be reached.
    fn variant_layout(
        &mut self,
        dst: Value,
        variant: VariantId,
        args: GenericArgs<'db>,
    ) -> Result<Option<&'cx Layout>> {
        let Some((layout, tag)) = self.variant_shape(variant, args)? else { return Ok(None) };
        if let Some((offset, ty, tag)) = tag {
            let value = self.iconst(ty, tag);
            self.builder.ins().store(MEM, value, dst, offset as i32);
        }
        Ok(Some(layout))
    }

    /// Layout whose `fields` describe `variant`, and the tag of a variant of a multi-variant
    /// enum that has one: where it goes, as what and its value. `None` for a variant the
    /// layout has no room for.
    fn variant_shape(
        &mut self,
        variant: VariantId,
        args: GenericArgs<'db>,
    ) -> Result<Option<(&'cx Layout, Tag)>> {
        if let Some(&shape) = self.layouts.variants.borrow().get(&(variant, args)) {
            return Ok(shape);
        }
        let shape = self.variant_shape_of(variant, args)?;
        Ok(*self.layouts.variants.borrow_mut().entry((variant, args)).or_insert(shape))
    }

    fn variant_shape_of(
        &mut self,
        variant: VariantId,
        args: GenericArgs<'db>,
    ) -> Result<Option<VariantShape<'cx>>> {
        let db = self.cg.db;
        let adt: AdtId = variant.adt_id(db);
        let ty = Ty::new_adt(self.interner, adt, args);
        let layout = self.layout(ty)?;
        let (tag, tag_encoding, tag_field) = match &layout.variants {
            Variants::Multiple { tag, tag_encoding, tag_field, .. } => {
                (tag, tag_encoding, tag_field)
            }
            Variants::Single { index } => {
                let inhabited = match variant {
                    VariantId::EnumVariantId(variant) => variant.index(db) == index.0,
                    _ => true,
                };
                return Ok(inhabited.then_some((layout, None)));
            }
            Variants::Empty => return Ok(None),
        };
        let VariantId::EnumVariantId(variant) = variant else {
            unsupported!("multi-variant non-enum");
        };
        let index = variant.index(db);
        let tag_ty = self.scalar(*tag)?.ty;
        let offset = layout.fields.offset(tag_field.index()).bytes_usize();
        let tag_value = match tag_encoding {
            TagEncoding::Direct => {
                let discr = db
                    .const_eval_discriminant(variant)
                    // alloc: the error's message
                    .map_err(|e| CodegenError::Frontend(format!("discriminant: {e:?}")))?;
                Some(discr as u128)
            }
            TagEncoding::Niche { untagged_variant, .. } if untagged_variant.0 == index => None,
            TagEncoding::Niche { niche_variants, niche_start, .. } => {
                Some(((index - niche_variants.start.0) as u128).wrapping_add(*niche_start))
            }
        };
        let kept: &'cx KeptLayouts = self.layouts.kept;
        // A `Layout` owns `Vec`s, so its copy can't be in the frame, whose values are never
        // dropped.
        // alloc: one per variant asked for, kept for the build
        let variant_layout = Box::new(Layout::for_variant(layout, RustcEnumVariantIdx(index)));
        let layout: &'cx Layout = kept.variants.alloc(variant_layout);
        Ok(Some((layout, tag_value.map(|it| (offset, tag_ty, it)))))
    }

    fn write_fields(&mut self, dst: Value, layout: &Layout, operands: &[Operand]) -> Result<()> {
        if operands.len() > layout.fields.count() {
            unsupported!("constructor with more operands than the layout has fields");
        }
        for (i, operand) in operands.iter().enumerate() {
            let (src, ty) = self.operand(operand)?;
            let field_layout = self.layout(ty)?;
            let field = self.builder.ins().iadd_imm(dst, layout.fields.offset(i).bytes() as i64);
            self.copy_value(field, src, &field_layout);
        }
        Ok(())
    }

    fn binary_op(&mut self, op: &BinOp, lhs: &Operand, rhs: &Operand) -> Result<Value> {
        Ok(self.overflowing_op(op, lhs, rhs)?.0)
    }

    /// `lhs op rhs`, which never panics: runtime MIR checks with an `Assert` before it. For
    /// `AddWithOverflow` and the like, also whether it overflowed.
    fn overflowing_op(
        &mut self,
        op: &BinOp,
        lhs: &Operand,
        rhs: &Operand,
    ) -> Result<(Value, Option<Value>)> {
        let (op, overflowing) = match op {
            BinOp::AddWithOverflow => (&BinOp::Add, true),
            BinOp::SubWithOverflow => (&BinOp::Sub, true),
            BinOp::MulWithOverflow => (&BinOp::Mul, true),
            op => (op, false),
        };
        let (mut l_addr, mut ty) = self.operand(lhs)?;
        let (mut r_addr, mut r_ty) = self.operand(rhs)?;
        // RA lowers comparisons of references without the deref.
        // Where the innermost reference is stored, for the length of a `str`.
        let (mut l_ref, mut r_ref) = (None, None);
        while let TyKind::Ref(_, inner, _) = ty.kind() {
            ty = inner;
            l_ref = Some(l_addr);
            l_addr = self.builder.ins().load(self.cg.target.pointer(), MEM, l_addr, 0);
        }
        while let TyKind::Ref(_, inner, _) = r_ty.kind() {
            r_ty = inner;
            r_ref = Some(r_addr);
            r_addr = self.builder.ins().load(self.cg.target.pointer(), MEM, r_addr, 0);
        }
        // A constant pattern of a type that has no `==` of its own: rustc matches such a
        // constant field by field, never through `PartialEq`.
        if let BinOp::Eq | BinOp::Ne = op
            && matches!(
                ty.kind(),
                TyKind::Adt(..)
                    | TyKind::Tuple(_)
                    | TyKind::Array(..)
                    | TyKind::Slice(_)
                    | TyKind::Str
            )
        {
            let metas = match (l_ref, r_ref) {
                (Some(l_ref), Some(r_ref)) if self.is_unsized(ty) => Some((
                    self.builder.ins().load(
                        self.cg.target.pointer(),
                        MEM,
                        l_ref,
                        self.cg.target.meta_at(),
                    ),
                    self.builder.ins().load(
                        self.cg.target.pointer(),
                        MEM,
                        r_ref,
                        self.cg.target.meta_at(),
                    ),
                )),
                _ => None,
            };
            let (differ, done) = (self.builder.create_block(), self.builder.create_block());
            self.builder.append_block_param(done, types::I8);
            self.branch_unless_equal(l_addr, r_addr, metas, ty, differ)?;
            let equal = self.builder.ins().iconst(types::I8, matches!(op, BinOp::Eq) as i64);
            self.builder.ins().jump(done, &[equal.into()]);
            self.builder.switch_to_block(differ);
            let unequal = self.builder.ins().iconst(types::I8, matches!(op, BinOp::Ne) as i64);
            self.builder.ins().jump(done, &[unequal.into()]);
            self.builder.switch_to_block(done);
            return Ok((self.builder.block_params(done)[0], None));
        }
        if matches!(ty.kind(), TyKind::RawPtr(..))
            && matches!(self.layout(ty)?.backend_repr, BackendRepr::ScalarPair { .. })
        {
            // Wide pointers compare by address, then by metadata.
            let cc = match op {
                BinOp::Eq => IntCC::Equal,
                BinOp::Ne => IntCC::NotEqual,
                BinOp::Lt => IntCC::UnsignedLessThan,
                BinOp::Le => IntCC::UnsignedLessThanOrEqual,
                BinOp::Gt => IntCC::UnsignedGreaterThan,
                BinOp::Ge => IntCC::UnsignedGreaterThanOrEqual,
                _ => unsupported!("{op:?} on wide pointers"),
            };
            let [l_ptr, r_ptr] = [l_addr, r_addr]
                .map(|it| self.builder.ins().load(self.cg.target.pointer(), MEM, it, 0));
            let [l_meta, r_meta] = [l_addr, r_addr].map(|it| {
                self.builder.ins().load(self.cg.target.pointer(), MEM, it, self.cg.target.meta_at())
            });
            let same_ptr = self.builder.ins().icmp(IntCC::Equal, l_ptr, r_ptr);
            let by_meta = self.builder.ins().icmp(cc, l_meta, r_meta);
            let by_ptr = self.builder.ins().icmp(cc, l_ptr, r_ptr);
            let value = match op {
                BinOp::Eq => self.builder.ins().band(same_ptr, by_meta),
                BinOp::Ne => self.builder.ins().bor(by_ptr, by_meta),
                _ => self.builder.ins().select(same_ptr, by_meta, by_ptr),
            };
            return Ok((value, None));
        }
        let scalar = self.scalar_of(ty)?;
        let r_scalar = self.scalar_of(r_ty)?;
        // A literal is an immediate, and the constant it was given an address as is not read.
        let l = match self.literal_bits(lhs) {
            Some((bits, _)) => self.iconst(scalar.ty, bits),
            None => self.builder.ins().load(scalar.ty, MEM, l_addr, 0),
        };
        let r = match self.literal_bits(rhs) {
            Some((bits, _)) => self.iconst(r_scalar.ty, bits),
            None => self.builder.ins().load(r_scalar.ty, MEM, r_addr, 0),
        };
        let signed = scalar.signed;
        if f16_f128::soft(scalar.ty) {
            return Ok((self.soft_binary_op(op, l, r)?, None));
        }
        if scalar.ty.is_float() {
            return Ok((self.float_binary_op(op, l, r)?, None));
        }
        let cc = match (op, signed) {
            (BinOp::Eq, _) => Some(IntCC::Equal),
            (BinOp::Ne, _) => Some(IntCC::NotEqual),
            (BinOp::Lt, true) => Some(IntCC::SignedLessThan),
            (BinOp::Lt, false) => Some(IntCC::UnsignedLessThan),
            (BinOp::Le, true) => Some(IntCC::SignedLessThanOrEqual),
            (BinOp::Le, false) => Some(IntCC::UnsignedLessThanOrEqual),
            (BinOp::Gt, true) => Some(IntCC::SignedGreaterThan),
            (BinOp::Gt, false) => Some(IntCC::UnsignedGreaterThan),
            (BinOp::Ge, true) => Some(IntCC::SignedGreaterThanOrEqual),
            (BinOp::Ge, false) => Some(IntCC::UnsignedGreaterThanOrEqual),
            _ => None,
        };
        if let Some(cc) = cc {
            return Ok((self.builder.ins().icmp(cc, l, r), None));
        }
        // An arbitrary width's result is normalized to it, and overflows when it leaves it.
        let narrow = NarrowInt::of(ty);
        if let BinOp::Mul = op
            && self.builder.func.dfg.value_type(l) == types::I128
        {
            let (value, overflow) = self.wide_mul_overflow(l, r, signed)?;
            if let Some(int) = narrow {
                let flag = overflowing.then_some(overflow);
                return Ok(self.narrow_overflowing(int, value, flag, true));
            }
            return Ok((value, overflowing.then_some(overflow)));
        }
        if let (Some(int), BinOp::Shl | BinOp::Shr) = (narrow, op) {
            let value = self.narrow_shift(l, r, r_ty, matches!(op, BinOp::Shl), int);
            return Ok((value, None));
        }
        let ins = self.builder.ins();
        let value = match op {
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                unreachable!("comparisons return above")
            }
            BinOp::BitAnd => ins.band(l, r),
            BinOp::BitOr => ins.bor(l, r),
            BinOp::BitXor => ins.bxor(l, r),
            BinOp::Add | BinOp::Sub | BinOp::Mul => {
                let (value, overflow) = match (op, signed) {
                    (BinOp::Add, true) => ins.sadd_overflow(l, r),
                    (BinOp::Add, false) => ins.uadd_overflow(l, r),
                    (BinOp::Sub, true) => ins.ssub_overflow(l, r),
                    (BinOp::Sub, false) => ins.usub_overflow(l, r),
                    (BinOp::Mul, true) => ins.smul_overflow(l, r),
                    (_, _) => ins.umul_overflow(l, r),
                };
                if let Some(int) = narrow {
                    let (flag, mul) = (overflowing.then_some(overflow), matches!(op, BinOp::Mul));
                    return Ok(self.narrow_overflowing(int, value, flag, mul));
                }
                return Ok((value, overflowing.then_some(overflow)));
            }
            // An `Assert` before it has ruled out a zero divisor and `MIN / -1`, whatever the
            // overflow checks: what is left of an arbitrary width's is of its width.
            BinOp::Div | BinOp::Rem => self.divide(l, r, matches!(op, BinOp::Div), signed)?,
            // The amount is taken modulo the width, as rustc's `Shl` and `Shr` do. One of an
            // arbitrary width is read at its own width ([`Self::unsigned_amount`]).
            BinOp::Shl | BinOp::Shr => {
                let r = self.unsigned_amount(r, r_ty);
                let (r, _) = self.shift_amount_word(r);
                match (op, signed) {
                    (BinOp::Shl, _) => self.builder.ins().ishl(l, r),
                    (_, true) => self.builder.ins().sshr(l, r),
                    (_, false) => self.builder.ins().ushr(l, r),
                }
            }
            BinOp::Offset => unsupported!("pointer offset"),
            BinOp::AddWithOverflow | BinOp::SubWithOverflow | BinOp::MulWithOverflow => {
                unreachable!("taken apart above")
            }
        };
        Ok((value, None))
    }

    /// A shift amount as at most one machine word, and whether the part cut off was not zero.
    /// A shift takes its amount modulo the width of the value, which the low word decides.
    fn shift_amount_word(&mut self, amount: Value) -> (Value, Option<Value>) {
        if self.builder.func.dfg.value_type(amount) != types::I128 {
            return (amount, None);
        }
        let (low, high) = self.builder.ins().isplit(amount);
        let beyond = self.builder.ins().icmp_imm(IntCC::NotEqual, high, 0);
        (low, Some(beyond))
    }

    fn float_binary_op(&mut self, op: &BinOp, l: Value, r: Value) -> Result<Value> {
        let ins = self.builder.ins();
        Ok(match op {
            BinOp::Eq => ins.fcmp(FloatCC::Equal, l, r),
            BinOp::Ne => ins.fcmp(FloatCC::NotEqual, l, r),
            BinOp::Lt => ins.fcmp(FloatCC::LessThan, l, r),
            BinOp::Le => ins.fcmp(FloatCC::LessThanOrEqual, l, r),
            BinOp::Gt => ins.fcmp(FloatCC::GreaterThan, l, r),
            BinOp::Ge => ins.fcmp(FloatCC::GreaterThanOrEqual, l, r),
            BinOp::Add => ins.fadd(l, r),
            BinOp::Sub => ins.fsub(l, r),
            BinOp::Mul => ins.fmul(l, r),
            BinOp::Div => ins.fdiv(l, r),
            BinOp::Rem => {
                let ty = self.builder.func.dfg.value_type(l);
                let name = if ty == types::F32 { "fmodf" } else { "fmod" };
                self.float_libcall(name, ty, &[l, r])?
            }
            _ => unsupported!("float operator {op:?}"),
        })
    }

    /// Calls a libm function taking and returning `ty`.
    fn float_libcall(&mut self, name: &str, ty: Type, args: &[Value]) -> Result<Value> {
        let mut sig = self.cg.make_signature();
        sig.params.extend(args.iter().map(|_| AbiParam::new(ty)));
        sig.returns.push(AbiParam::new(ty));
        let callee = self.cg.import(name, &sig)?;
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        Ok(self.invoke(callee, args)[0])
    }

    /// `as` between a float and an integer or another float.
    fn float_cast(&mut self, value: Value, from: AbiScalar, to: AbiScalar) -> Result<Value> {
        if f16_f128::soft(from.ty) || f16_f128::soft(to.ty) {
            return self.soft_cast(value, from, to);
        }
        if from.ty == types::I128 || to.ty == types::I128 {
            // compiler-builtins' conversions. What one of a float does out of range differs
            // between it and libgcc's, so the bounds are checked around it, as LLVM does.
            let name = match (from.ty, from.signed, to.ty, to.signed) {
                (types::F32, _, _, true) => "__fixsfti",
                (types::F32, _, _, false) => "__fixunssfti",
                (types::F64, _, _, true) => "__fixdfti",
                (types::F64, _, _, false) => "__fixunsdfti",
                (_, true, types::F32, _) => "__floattisf",
                (_, false, types::F32, _) => "__floatuntisf",
                (_, true, types::F64, _) => "__floattidf",
                (_, false, types::F64, _) => "__floatuntidf",
                _ => unsupported!("cast from {} to {}", from.ty, to.ty),
            };
            let converted = self.builtin(name, &[from.ty], to.ty, &[value])?;
            if !from.ty.is_float() {
                return Ok(converted);
            }
            return Ok(self.saturate_wide(value, from.ty, converted, to.signed));
        }
        let ins = self.builder.ins();
        Ok(match (from.ty.is_float(), to.ty.is_float()) {
            (true, true) if from.ty == to.ty => value,
            (true, true) if to.ty == types::F64 => ins.fpromote(types::F64, value),
            (true, true) => ins.fdemote(types::F32, value),
            (false, true) => {
                let wide = if from.ty.bits() < 32 {
                    if from.signed {
                        ins.sextend(types::I32, value)
                    } else {
                        ins.uextend(types::I32, value)
                    }
                } else {
                    value
                };
                if from.signed {
                    self.builder.ins().fcvt_from_sint(to.ty, wide)
                } else {
                    self.builder.ins().fcvt_from_uint(to.ty, wide)
                }
            }
            (true, false) => {
                // Saturating, with NaN becoming zero, as `as` requires.
                if to.ty.bits() >= 32 {
                    if to.signed {
                        ins.fcvt_to_sint_sat(to.ty, value)
                    } else {
                        ins.fcvt_to_uint_sat(to.ty, value)
                    }
                } else {
                    let wide = ins.fcvt_to_sint_sat(types::I32, value);
                    let bits = to.ty.bits();
                    let (min, max) = if to.signed {
                        (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1)
                    } else {
                        (0, (1i64 << bits) - 1)
                    };
                    let min = self.builder.ins().iconst(types::I32, min);
                    let max = self.builder.ins().iconst(types::I32, max);
                    let clamped = self.builder.ins().smax(wide, min);
                    let clamped = self.builder.ins().smin(clamped, max);
                    self.builder.ins().ireduce(to.ty, clamped)
                }
            }
            (false, false) => unreachable!("float cast between integers"),
        })
    }

    /// What `as` makes of the `f32` or `f64` `value` as a 128-bit integer, given what a
    /// conversion that is right in range made of it: the least or greatest integer below or
    /// above the range, and zero for NaN.
    fn saturate_wide(&mut self, value: Value, ty: Type, converted: Value, signed: bool) -> Value {
        let single = ty == types::F32;
        // The least value in range, and the greatest float that is in range.
        let (low, high): (u64, u64) = match (single, signed) {
            (true, true) => (0xff00_0000, 0x7eff_ffff),
            (true, false) => (0, 0x7f7f_ffff),
            (false, true) => (0xc7e0_0000_0000_0000, 0x47df_ffff_ffff_ffff),
            (false, false) => (0, 0x47ef_ffff_ffff_ffff),
        };
        let [low, high] = [low, high].map(|bits| match single {
            true => self.builder.ins().f32const(f32::from_bits(bits as u32)),
            false => self.builder.ins().f64const(f64::from_bits(bits)),
        });
        let (min, max) = match signed {
            true => (1u128 << 127, (1u128 << 127) - 1),
            false => (0, u128::MAX),
        };
        let (min, max) = (self.iconst(types::I128, min), self.iconst(types::I128, max));
        // NaN is not in range either, and is below it for an unsigned integer.
        let in_low = self.builder.ins().fcmp(FloatCC::GreaterThanOrEqual, value, low);
        let converted = self.builder.ins().select(in_low, converted, min);
        let above = self.builder.ins().fcmp(FloatCC::GreaterThan, value, high);
        let converted = self.builder.ins().select(above, max, converted);
        if !signed {
            return converted;
        }
        let ordered = self.builder.ins().fcmp(FloatCC::Ordered, value, value);
        let zero = self.iconst(types::I128, 0);
        self.builder.ins().select(ordered, converted, zero)
    }

    /// Whether arithmetic of the statement being compiled panics when it overflows: as the
    /// body its span is written in says (an inlined callee's crate decides for its code).
    fn checks_overflow(&self) -> Result<bool> {
        let Some(body) = self.body else { return Ok(false) };
        if !written_checked_at(self.cg.db, body, self.span) {
            return Ok(false);
        }
        Ok(self.cg.checked)
    }

    /// Branches to a call of core's `panic_const_*` when `cond` is set.
    /// Calls `core::panicking::{name}` with `args` and the caller's location where `cond`
    /// holds: the panics of rustc's UB checks (`check_pointers`, `check_enums`), which are
    /// `#[track_caller]` and do not unwind.
    fn ub_check_panic_if(&mut self, cond: Value, name: &'static str, args: &[Value]) -> Result<()> {
        let panic_block = self.builder.create_block();
        let ok_block = self.builder.create_block();
        self.builder.set_cold_block(panic_block);
        self.builder.ins().brif(cond, panic_block, &[], ok_block, &[]);

        self.builder.switch_to_block(panic_block);
        let location = self.caller_location()?;
        let mut with = smallvec::SmallVec::<[Value; 3]>::from_slice(args);
        with.push(location);
        self.call_panic(&["panicking", name], &with)?;
        self.builder.ins().trap(TrapCode::user(1).expect("non-zero trap code"));

        self.builder.switch_to_block(ok_block);
        Ok(())
    }

    /// Branches to the panic of a failed check of `op` when `cond` is set: core's
    /// `panic_const_*`, or for a check of an arbitrary width `checked`, core's `panic` with
    /// the message naming it.
    fn panic_if(&mut self, cond: Value, op: OverflowOp, checked: Option<NarrowInt>) -> Result<()> {
        let name = match op {
            OverflowOp::Add => "panic_const_add_overflow",
            OverflowOp::Sub => "panic_const_sub_overflow",
            OverflowOp::Mul => "panic_const_mul_overflow",
            OverflowOp::Div => "panic_const_div_overflow",
            OverflowOp::Rem => "panic_const_rem_overflow",
            OverflowOp::Neg => "panic_const_neg_overflow",
            OverflowOp::Shl => "panic_const_shl_overflow",
            OverflowOp::Shr => "panic_const_shr_overflow",
            OverflowOp::DivByZero => "panic_const_div_by_zero",
            OverflowOp::RemByZero => "panic_const_rem_by_zero",
        };
        let panic_block = self.builder.create_block();
        let ok_block = self.builder.create_block();
        self.builder.set_cold_block(panic_block);
        self.builder.ins().brif(cond, panic_block, &[], ok_block, &[]);

        self.builder.switch_to_block(panic_block);
        if let Some(int) = checked {
            self.narrow_check_panic(op, int)?;
        } else {
            let location = self.caller_location()?;
            self.call_panic(&["panicking", "panic_const", name], &[location])?;
        }
        self.builder.ins().trap(TrapCode::user(1).expect("non-zero trap code"));

        self.builder.switch_to_block(ok_block);
        Ok(())
    }

    /// The expression `span` of the body being compiled is, and the body whose expression
    /// store it is of: of an inlined callee for its code ([`source_span`]).
    fn written_expr(&self, span: Option<MirSpan>) -> Option<(InferBodyId<'db>, ExprId)> {
        match source_span(self.cg.db, self.body?, span?) {
            (owner, MirSpan::ExprId(expr)) => Some((owner, expr)),
            _ => None,
        }
    }

    /// Whether `expr` of the body of `owner` is what `file!()` expanded to, and the expansion
    /// it is in.
    fn location_macro(&self, owner: InferBodyId<'db>, expr: ExprId) -> Option<HirFileId> {
        let InferBodyId::DefWithBodyId(owner) = owner else { return None };
        location_macros(self.cg.db, owner).get(&expr).copied()
    }

    /// Whether a promoted expression of the body of `owner` takes its value from where it is
    /// in the source.
    fn mentions_location(&self, owner: InferBodyId<'db>, expr: ExprId) -> bool {
        let InferBodyId::DefWithBodyId(def) = owner else { return false };
        hir_ty_mir::mir::location::mentions_location(self.cg.db, def, expr)
    }

    /// The `&str` of a literal that a `concat!` with `file!()` among its arguments expanded
    /// to, in the body of `owner`: the expansion has the path's placeholder in it, so the
    /// arguments are read again, `line!()` and `column!()` among them.
    fn located_concat(&mut self, owner: InferBodyId<'db>, expr: ExprId) -> Result<Option<Value>> {
        let db = self.cg.db;
        let InferBodyId::DefWithBodyId(owner) = owner else { return Ok(None) };
        let Some(call) = located_concats(db, owner)
            .get(&expr)
            .and_then(|file_id| file_id.macro_file())
            .map(|it| it.loc(db).to_node_view(db))
        else {
            return Ok(None);
        };
        let Some(parts) = concat_parts(call.value) else {
            unsupported!("`concat!` of a place in the source and what is not a literal");
        };
        // alloc: the parts of the data object, which `text` keeps
        let mut text = Vec::with_capacity(parts.len());
        for part in parts {
            text.push(match part {
                ConcatPart::Text(text) => TextPart::Text(text),
                ConcatPart::Location(expander, range) => {
                    let Some((file, line, col)) = macro_location_of(db, call.file_id, range) else {
                        unsupported!("`concat!` of a place that is not in a file");
                    };
                    // alloc: the text of the data object, which a `TextPart` owns; once per
                    // `line!()` or `column!()` in a `concat!` with `file!()`
                    match expander {
                        // alloc: as above
                        BuiltinFnLikeExpander::Line => TextPart::Text(line.to_string()),
                        // alloc: as above
                        BuiltinFnLikeExpander::Column => TextPart::Text(col.to_string()),
                        _ => TextPart::FilePath(file),
                    }
                }
            });
        }
        let text = self.cg.text(text);
        let text = self.cg.declare_data_in_func(text, self.builder.func);
        Ok(Some(self.builder.ins().symbol_value(self.cg.target.pointer(), text)))
    }

    /// The `&str` of a literal that `file!()`, or a `concat!` with it, expanded to, in the body
    /// of `owner`: the address of read-only data, which a `'static` borrow of a promoted one
    /// may outlive the call with. The expansion has a placeholder for the path, which is the
    /// build's rule's; `line!()` and `column!()` expand to their values.
    fn builtin_location(&mut self, owner: InferBodyId<'db>, expr: ExprId) -> Result<Option<Value>> {
        let db = self.cg.db;
        if let Some(text) = self.located_concat(owner, expr)? {
            return Ok(Some(text));
        }
        let Some(file_id) = self.location_macro(owner, expr) else {
            return Ok(None);
        };
        let Some(file) = hir_ty_mir::mir::location::location_macro_file(db, file_id) else {
            return Ok(None);
        };
        let path = self.cg.file_path(file);
        let path = self.cg.declare_data_in_func(path, self.builder.func);
        Ok(Some(self.builder.ins().symbol_value(self.cg.target.pointer(), path)))
    }

    /// Tells `overlap` which of the slots of `shareable` and of the locals kept in registers
    /// may hold a value at once: those of two locals whose storage may overlap, a temporary
    /// written to a binding's slot (`forwarded`) counting as part of the binding.
    fn note_shareable(
        &mut self,
        body: &MirBody<'_>,
        shareable: &[(LocalId, ir::StackSlot)],
        forwarded: &HashMap<LocalId, LocalId, Global>,
    ) {
        let frame = self.frame;
        // A local kept in a register gets its slot later, if ever: see `local_slot`. In index
        // order: what sorting the keys of a hash map gave.
        let mut registers: Vec<LocalId, &Frame> =
            Vec::with_capacity_in(self.registers.len(), frame);
        registers.extend(self.registers.keys());
        let count = shareable.len() + registers.len();
        if count < 2 {
            return;
        }
        let mut at: FrameHashMap<'_, LocalId, usize> =
            frame_map::hash_map_with_capacity(shareable.len(), frame);
        at.extend(shareable.iter().enumerate().map(|(at, &(local, _))| (local, at)));
        // Each local with the slot it is kept in, by the slot's index.
        let mut members: Vec<(LocalId, usize), &Frame> =
            Vec::with_capacity_in(count + forwarded.len(), frame);
        members.extend(shareable.iter().enumerate().map(|(at, &(local, _))| (local, at)));
        // alloc: kept in the recorder's frame locals (`overlap::FrameLocals`) past the build
        let mut lazy: HashMap<LocalId, usize, Global> =
            HashMap::with_capacity_in(registers.len(), Global);
        lazy.extend(registers.iter().enumerate().map(|(at, &local)| (local, shareable.len() + at)));
        members.extend(registers.iter().map(|local| (*local, lazy[local])));
        for (&temp, to) in forwarded {
            if let Some(&slot) = at.get(to) {
                members.push((temp, slot));
            }
        }
        let locals: &[LocalId] = frame.alloc_slice_from_iter(members.iter().map(|it| it.0));
        // A debugger reads a named local's slot all through its scope (`note_variables`), so it
        // keeps the slot to itself there, not only until its last read.
        let mut named: FrameHashSet<'_, LocalId> = frame_map::hash_set(frame);
        if Self::names_are_written(body) {
            named.extend(body.binding_locals.iter().map(|it| *it.1));
        }
        let storage = body.frame_storage_conflicts(locals, |local| named.contains(&local));
        let mut conflicts = blaze_ids::bits::BitMatrix::new(count, count);
        for (a, &(_, slot_a)) in members.iter().enumerate() {
            for (b, &(_, slot_b)) in members.iter().enumerate().skip(a + 1) {
                if slot_a != slot_b && storage.conflict(a, b) {
                    conflicts.insert(slot_a, slot_b);
                    conflicts.insert(slot_b, slot_a);
                }
            }
        }
        // alloc: kept in the recorder's frame locals past the build, as `lazy` is
        let mut slots: Vec<Option<ir::StackSlot>> = shareable.iter().map(|it| Some(it.1)).collect();
        slots.resize(count, None);
        self.cg.frame_locals = Some(overlap::FrameLocals { slots, conflicts, lazy });
    }

    /// Notes the locals a debugger can be told of: the ones written with a name whose value
    /// is in a stack slot of its own, or in the future of an `async` body. What a closure
    /// captured is no local of its body, and is told of by the type of the closure alone.
    fn note_variables(&mut self) {
        let Some(body) = self.body.filter(|body| Self::names_are_written(body)) else { return };
        // The slot an `async` body has the address of its future in.
        let future = match (body.coroutine.is_some(), body.param_locals.first()) {
            (true, Some(&receiver)) => self.slot_of(receiver),
            _ => None,
        };
        for (binding, &local) in body.binding_locals.iter() {
            let param = body.param_locals.contains(&local);
            let ty = body.locals[local].ty;
            let held = self.held_at.iter().find(|it| it.0 == local).map(|it| it.1);
            if let (Some(slot), Some(offset)) = (future, held) {
                self.cg.named.push(Named { binding, param, ty, slot, pointer: Some(offset) });
            } else if let Some(slot) = self.slot_of(local) {
                self.cg.named.push(Named { binding, param, ty, slot, pointer: None });
            }
        }
    }

    /// The stack slot a local is the whole of, when it is kept in one.
    fn slot_of(&self, local: LocalId) -> Option<ir::StackSlot> {
        let &LocalSlot { addr, borrowed: false } = self.locals.get(local)? else { return None };
        let dfg = &self.builder.func.dfg;
        let ir::ValueDef::Result(inst, 0) = dfg.value_def(addr) else { return None };
        match dfg.insts[inst] {
            ir::InstructionData::StackAddr { stack_slot, offset, .. } if i32::from(offset) == 0 => {
                Some(stack_slot)
            }
            _ => None,
        }
    }

    /// Whether the locals of a body are of names its source has.
    fn names_are_written(body: &MirBody) -> bool {
        matches!(body.owner, InferBodyId::DefWithBodyId(_))
    }

    /// Keeps where a parameter passed in memory is in a slot of the frame, when it is
    /// written with a name, for a debugger to find it by: the register it comes in is soon
    /// used for something else.
    fn note_passed_in_memory(&mut self, body: &MirBody, local: LocalId, addr: Value) {
        if !Self::names_are_written(body) {
            return;
        }
        let Some((binding, _)) = body.binding_locals.iter().find(|it| *it.1 == local) else {
            return;
        };
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            self.cg.target.pointer().bytes(),
            3,
        ));
        let kept = self.builder.ins().stack_addr(self.cg.target.pointer(), slot, 0);
        self.builder.ins().store(MEM, addr, kept, 0);
        let ty = body.locals[local].ty;
        self.cg.named.push(Named { binding, param: true, ty, slot, pointer: Some(0) });
    }

    /// Has what is built from here on be of the statement at `span`.
    fn at(&mut self, span: MirSpan) {
        self.span = span;
        let loc = self.cg.source_loc(span);
        self.builder.set_srcloc(loc);
    }

    /// A `core::panic::Location` (`{ file: &str, line: u32, col: u32 }`) for `#[track_caller]`:
    /// where the statement being compiled is written, out through each `#[track_caller]` body
    /// inlined around it to the call it was inlined at (rustc's `caller_location_span`), or
    /// the location the caller passes where the function and every body inlined around the
    /// statement are `#[track_caller]`. A shim passes on what it was given.
    fn caller_location(&mut self) -> Result<Value> {
        let Some(body) = self.body else {
            match (self.location, self.in_drop_glue) {
                (Some(location), _) => return Ok(location),
                (None, true) => {
                    let Some(glue) = self.interner.lang_items().DropGlue else {
                        unsupported!(
                            "a `Location` in drop glue, which is spanned at the `drop_glue` lang \
                             item, and the library has none"
                        );
                    };
                    let location = self.cg.declaration(glue);
                    let location = self.cg.declare_data_in_func(location, self.builder.func);
                    return Ok(self.builder.ins().symbol_value(self.cg.target.pointer(), location));
                }
                (None, false) => unsupported!("a `Location` in a shim that was given none"),
            }
        };
        let (owner, span) = match caller_location(self.cg.db, body, self.span, self.tracked) {
            CallerLocation::Argument => match self.location {
                Some(location) => return Ok(location),
                None => unsupported!(
                    "the caller's `Location` at {:?} of {:?}, which the function is not given",
                    self.span,
                    body.owner
                ),
            },
            CallerLocation::At(InferBodyId::DefWithBodyId(owner), span) => (owner, span),
            CallerLocation::At(owner, span) => unsupported!(
                "a `Location` at {span:?} of {owner:?}, which is no item's body: it has no \
                 place in the source"
            ),
        };
        let location = self.cg.location(Some((owner, span)));
        let location = self.cg.declare_data_in_func(location, self.builder.func);
        Ok(self.builder.ins().symbol_value(self.cg.target.pointer(), location))
    }

    fn terminator(&mut self, terminator: &TerminatorKind) -> Result<()> {
        match terminator {
            TerminatorKind::Goto { target }
            | TerminatorKind::FalseEdge { real_target: target, .. }
            | TerminatorKind::FalseUnwind { real_target: target, .. } => {
                self.builder.ins().jump(self.blocks[*target], &[]);
            }
            TerminatorKind::SwitchInt { discr, targets } => {
                if let Some(bits) = self.const_bits(discr) {
                    let target = self.blocks[targets.target_for_value(bits)];
                    self.builder.ins().jump(target, &[]);
                    return Ok(());
                }
                let register = self.register_of(discr);
                let (addr, ty) = match register {
                    Some(_) => (self.iconst(self.cg.target.pointer(), 0), self.operand_ty(discr)?),
                    None => self.operand(discr)?,
                };
                let scalar = self.scalar_of(ty)?;
                let otherwise = self.blocks[targets.otherwise()];
                // A discriminant is an `i128` to MIR and a small number: with the high half
                // told to be zero first, the rest is a switch on a register.
                let small = targets.iter().all(|(case, _)| case <= i64::MAX as u128);
                let value = if let Some(register) = register {
                    let value = self.builder.use_var(register.var);
                    let widened = match self.builder.func.dfg.value_def(value) {
                        ir::ValueDef::Result(inst, 0) => match self.builder.func.dfg.insts[inst] {
                            ir::InstructionData::Unary { opcode: ir::Opcode::Uextend, arg } => {
                                Some(arg)
                            }
                            _ => None,
                        },
                        _ => None,
                    }
                    .filter(|&arg| self.builder.func.dfg.value_type(arg) == types::I64);
                    if let (Some(low), true) = (widened, scalar.ty == types::I128 && small) {
                        // The high half is zero when the value was a word a moment ago.
                        low
                    } else if scalar.ty == types::I128 && small {
                        let (low, high) = self.builder.ins().isplit(value);
                        let fits = self.builder.create_block();
                        self.builder.ins().brif(high, otherwise, &[], fits, &[]);
                        self.builder.switch_to_block(fits);
                        low
                    } else {
                        value
                    }
                } else if scalar.ty == types::I128
                    && small
                    && self.cg.target_config().pointer_bytes() == 8
                {
                    let high = self.builder.ins().load(types::I64, MEM, addr, 8);
                    let low = self.builder.create_block();
                    self.builder.ins().brif(high, otherwise, &[], low, &[]);
                    self.builder.switch_to_block(low);
                    self.builder.ins().load(types::I64, MEM, addr, 0)
                } else {
                    self.builder.ins().load(scalar.ty, MEM, addr, 0)
                };
                let ty = self.builder.func.dfg.value_type(value);
                if backend::optimizing() && ty != types::I128 && targets.iter().count() <= CHAINED {
                    // LLVM folds a chain of comparisons of one value into a `switch`, whose
                    // jump tables, bit tests and lookup tables it picks itself; the tree of
                    // ranges `Switch` builds it keeps as it is.
                    for (case, target) in targets.iter() {
                        let case = self.builder.ins().iconst(ty, case as i64);
                        let hit = self.builder.ins().icmp(IntCC::Equal, value, case);
                        let next = self.builder.create_block();
                        self.builder.ins().brif(hit, self.blocks[target], &[], next, &[]);
                        self.builder.switch_to_block(next);
                    }
                    self.builder.ins().jump(otherwise, &[]);
                } else {
                    let mut switch = Switch::new();
                    for (case, target) in targets.iter() {
                        switch.set_entry(case, self.blocks[target]);
                    }
                    switch.emit(&mut self.builder, value, otherwise);
                }
            }
            TerminatorKind::Return => {
                if let Some(state) = &self.suspended {
                    let (addr, ty) = (state.addr, state.ty);
                    self.store_state(addr, ty, RETURNED);
                }
                self.return_()?;
            }
            TerminatorKind::Yield { resume, .. } => {
                let Some(state) = &self.suspended else {
                    unsupported!("yield outside of an async body");
                };
                let Some(index) = state.held.yields.iter().position(|it| it.resume == *resume)
                else {
                    unsupported!("yield to a block that is not resumed in");
                };
                let (addr, ty) = (state.addr, state.ty);
                self.store_state(addr, ty, SUSPENDED + index as i64);
                self.return_()?;
            }
            TerminatorKind::Unreachable => {
                self.builder.ins().trap(TrapCode::user(2).expect("non-zero trap code"));
            }
            TerminatorKind::Drop { place, target, .. } => {
                let place = self.place_ref(place)?;
                self.drop_value(place.addr, place.meta, place.ty.ty)?;
                self.builder.ins().jump(self.blocks[*target], &[]);
            }
            TerminatorKind::UnwindResume => {
                // An `async` body that unwinds is poisoned, as rustc has it.
                if let Some(state) = &self.suspended {
                    let (addr, ty) = (state.addr, state.ty);
                    self.store_state(addr, ty, PANICKED);
                }
                let slot = self.exception_slot();
                let exception = self.builder.ins().stack_load(
                    self.cg.target.pointer(),
                    self.cg.target.pointer(),
                    slot,
                    0,
                );
                self.resume_unwinding(exception)?;
            }
            // A suspended future has been dropped: its state already says it returned.
            TerminatorKind::CoroutineDrop => self.return_()?,
            TerminatorKind::Assert { cond, expected, msg, target, .. } => {
                // rustc's codegen skips an overflow check where overflow is not checked.
                if !msg.is_optional_overflow_check() || self.checks_overflow()? {
                    let (holds, _) = self.load_scalar(cond)?;
                    let cc = if *expected { IntCC::Equal } else { IntCC::NotEqual };
                    let failed = self.builder.ins().icmp_imm(cc, holds, 0);
                    let op = match msg {
                        AssertKind::BoundsCheck { len, index } => {
                            let (len, _) = self.load_scalar(len)?;
                            let (index, _) = self.load_scalar(index)?;
                            self.bounds_panic_if(failed, index, len)?;
                            None
                        }
                        AssertKind::MisalignedPointerDereference { required, found } => {
                            let (required, _) = self.load_scalar(required)?;
                            let (found, _) = self.load_scalar(found)?;
                            let name = "panic_misaligned_pointer_dereference";
                            self.ub_check_panic_if(failed, name, &[required, found])?;
                            None
                        }
                        AssertKind::NullPointerDereference => {
                            self.ub_check_panic_if(failed, "panic_null_pointer_dereference", &[])?;
                            None
                        }
                        AssertKind::NullReferenceConstructed => {
                            let name = "panic_null_reference_constructed";
                            self.ub_check_panic_if(failed, name, &[])?;
                            None
                        }
                        AssertKind::InvalidEnumConstruction(source) => {
                            let (source, _) = self.load_scalar(source)?;
                            let ty = self.builder.func.dfg.value_type(source);
                            if ty != types::I128 {
                                unsupported!("an invalid enum's value as {ty}, not `u128`");
                            }
                            let name = "panic_invalid_enum_construction";
                            self.ub_check_panic_if(failed, name, &[source])?;
                            None
                        }
                        AssertKind::Overflow(BinOp::Add, ..) => Some(OverflowOp::Add),
                        AssertKind::Overflow(BinOp::Sub, ..) => Some(OverflowOp::Sub),
                        AssertKind::Overflow(BinOp::Mul, ..) => Some(OverflowOp::Mul),
                        AssertKind::Overflow(BinOp::Div, ..) => Some(OverflowOp::Div),
                        AssertKind::Overflow(BinOp::Rem, ..) => Some(OverflowOp::Rem),
                        AssertKind::Overflow(BinOp::Shl, ..) => Some(OverflowOp::Shl),
                        AssertKind::Overflow(BinOp::Shr, ..) => Some(OverflowOp::Shr),
                        AssertKind::Overflow(op, ..) => unsupported!("overflow check of {op:?}"),
                        AssertKind::OverflowNeg(_) => Some(OverflowOp::Neg),
                        AssertKind::DivisionByZero(_) => Some(OverflowOp::DivByZero),
                        AssertKind::RemainderByZero(_) => Some(OverflowOp::RemByZero),
                    };
                    // A check of an arbitrary width names it in the panic.
                    let checked = match msg {
                        AssertKind::Overflow(_, of, _)
                        | AssertKind::OverflowNeg(of)
                        | AssertKind::DivisionByZero(of)
                        | AssertKind::RemainderByZero(of) => NarrowInt::of(self.operand_ty(of)?),
                        _ => None,
                    };
                    if let Some(op) = op {
                        self.panic_if(failed, op, checked)?;
                    }
                }
                self.builder.ins().jump(self.blocks[*target], &[]);
            }
            TerminatorKind::Call { func, args, destination, target, intrinsic, .. } => {
                self.call(func, args, destination, *target, *intrinsic)?
            }
            TerminatorKind::TailCall { func, args, .. } => self.tail_call(func, args)?,
            _ => unsupported!("terminator {terminator:?}"),
        }
        Ok(())
    }

    fn call(
        &mut self,
        func: &Operand,
        args: &[Operand],
        destination: &StoredPlace,
        target: Option<BasicBlockId>,
        intrinsic: Option<Intrinsic>,
    ) -> Result<()> {
        self.given_away.start(destination.local);
        let done = self.call_with(func, args, destination, target, intrinsic);
        self.given_away.result = None;
        done
    }

    /// `become callee(args)` (`TerminatorKind::TailCall`): a `return_call` that hands the
    /// callee this function's own return place and its own incoming argument memory, so
    /// the callee's frame takes this one's place and a chain of them runs in constant
    /// stack. The front end has refused every `become` rustc refuses (signatures, ABIs,
    /// borrows of locals) and dropped the locals before it; what is left here is a
    /// call the convention can make with nothing of this frame alive across it:
    ///
    /// - an indirect result is written where this function's caller asked for its own;
    /// - an argument passed by reference (`PassMode::Indirect`) is copied first into a
    ///   temporary and then into the memory this function was handed for the parameter at
    ///   its place, as rustc does (`tail_call_temporaries`): the two steps let
    ///   `become f(b, a)` swap two such parameters;
    /// - the call is never a `try_call`: no landing pad of this frame outlives it.
    ///
    /// A back end that can't guarantee the tail call refuses it, naming it (`emit_with`
    /// for Cranelift's native code and WebAssembly, LLVM where the callee's prototype is not
    /// the caller's); none makes a plain call in its place.
    fn tail_call(&mut self, func: &Operand, args: &[Operand]) -> Result<()> {
        let body = self.body.expect("MIR body");
        let db = self.cg.db;
        if self.suspended.is_some() {
            unsupported!("a tail call (`become`) in the `async` body {:?}", body.owner);
        }
        if self.tracked || self.variadic || self.abi == ExternAbi::RustCall {
            // The front end refuses each (`check_tail_calls`), as rustc does.
            unsupported!(
                "a tail call (`become`) in {:?}, which is `#[track_caller]`, C-variadic or \
                 `extern \"rust-call\"`",
                body.owner
            );
        }
        let fn_ty = self.operand_ty(func)?;
        let mut values = self.operands(args)?;
        let callee = match fn_ty.kind() {
            TyKind::FnPtr(..) => {
                let (addr, _) = self.operand(func)?;
                let pointer = self.builder.ins().load(self.cg.target.pointer(), MEM, addr, 0);
                let sig = fn_ty.fn_sig(self.interner);
                if sig.c_variadic() {
                    unsupported!("a tail call (`become`) through a C-variadic `{fn_ty:?}`");
                }
                self.check_abort_unwinding(sig.abi())?;
                Callee::Pointer(pointer, sig.abi(), None)
            }
            TyKind::FnDef(def, generic_args) => {
                let generic_args = generic_args.skip_binder();
                let CallableDefId::FunctionId(func) = def.0 else {
                    unsupported!("a tail call (`become`) of a constructor, {fn_ty:?}");
                };
                let signature = FunctionSignature::of(db, func);
                let name = signature.name.as_str();
                if let ItemContainerId::ExternBlockId(_) = func.lookup(db).container {
                    self.check_abort_unwinding(signature.abi)?;
                }
                if let Some(why) = tail_call_refused(db, self.cg.target, self.interner, func) {
                    unsupported!("{why}");
                }
                let callee =
                    dispatch::callee(&self.infcx, self.env, func, generic_args).map_err(|why| {
                        CodegenError::Refused(no_impl_method(db, func, generic_args, why))
                    })?;
                match callee {
                    dispatch::Callee::Fn(callee, callee_args) => {
                        // rustc tail-calls the shim a pointer to it would be, and warns that
                        // the callee is then told no caller (`tail_call_track_caller`).
                        if track_caller(db, callee) {
                            let def = CallableDefId::FunctionId(callee).into();
                            let item = Ty::new_fn_def(self.interner, def, callee_args);
                            let args = GenericArgs::new_from_slice(&[item.into()]);
                            Callee::Instance(InstanceDef::ReifyShim { receiver: false }, args)
                        } else {
                            Callee::Instance(InstanceDef::Fn(callee), callee_args)
                        }
                    }
                    dispatch::Callee::Poll(body, parent_args) => {
                        Callee::Instance(instance_def(body), parent_args)
                    }
                    dispatch::Callee::Virtual => {
                        if track_caller(db, func) {
                            unsupported!(
                                "a tail call (`become`) through a vtable of `{name}`, which is \
                                 `#[track_caller]`"
                            );
                        }
                        let Some(&(receiver, receiver_ty)) = values.first() else {
                            unsupported!("vtable call without a receiver");
                        };
                        let (pointer, thin, thin_ty) = self.virtual_callee(
                            func,
                            generic_args.type_at(0),
                            receiver,
                            receiver_ty,
                        )?;
                        values[0] = (thin, thin_ty);
                        Callee::Pointer(pointer, ExternAbi::Rust, None)
                    }
                    dispatch::Callee::Derived(impl_, method) => {
                        let derived =
                            unexpanded_derive(db, (impl_, method), generic_args.type_at(0));
                        unsupported!("a tail call (`become`) of {derived}");
                    }
                    dispatch::Callee::CloneBuiltin(_) | dispatch::Callee::FnPtr(_) => unsupported!(
                        "a tail call (`become`) of `{name}`, whose body the compiler supplies \
                         where it is called: it is no function to jump to"
                    ),
                }
            }
            _ => unsupported!("a tail call (`become`) of {fn_ty:?}"),
        };
        let abi = match callee {
            Callee::Instance(InstanceDef::Fn(func), _) => FunctionSignature::of(db, func).abi,
            Callee::Instance(..) => ExternAbi::Rust,
            Callee::Pointer(_, abi, _) => abi,
        };
        if abi == ExternAbi::RustCall {
            unsupported!("a tail call (`become`) of an `extern \"rust-call\"` function");
        }
        let returned = body.locals[returned_local(body)].ty.as_ref();
        let FnAbi { ret: ret_mode, args: arg_modes } =
            self.fn_abi(abi, returned, values.iter().map(|&(_, ty)| ty))?;
        let sig = self.signature(&arg_modes, ret_mode);
        let FnAbi { args: own_modes, .. } = self.body_abi(body, self.abi)?;
        let indirect = |mode| matches!(mode, PassMode::Indirect);
        if indirect(ret_mode) != indirect(self.ret_mode)
            || sig.returns != self.builder.func.signature.returns
        {
            unsupported!(
                "a tail call (`become`) whose callee returns {ret_mode:?} where {:?} returns \
                 {:?}",
                body.owner,
                self.ret_mode
            );
        }
        // Where this function was handed its own: the return place, and the memory of each
        // parameter passed by reference.
        let entry = self.builder.func.layout.entry_block().expect("the entry block");
        let incoming =
            smallvec::SmallVec::<[Value; 16]>::from_slice(self.builder.block_params(entry));
        let mut at = usize::from(matches!(self.ret_mode, PassMode::Indirect));
        let mut own_memory = smallvec::SmallVec::<[Option<Value>; 8]>::new();
        let mut these = Vec::new(); // alloc: a parameter's few, reused, of a tail call only
        for &mode in &own_modes {
            own_memory.push(matches!(mode, PassMode::Indirect).then(|| incoming[at]));
            these.clear();
            self.push_abi_params(mode, &mut these);
            at += these.len();
        }
        // The calls before this one have returned.
        self.free_argument_slots.append(&mut self.argument_slots);
        let mut call_values = Vec::with_capacity(sig.params.len()); // alloc: the call's arguments
        if let PassMode::Indirect = ret_mode {
            call_values.push(incoming[0]);
        }
        // Each by-reference argument's temporary, and the place among the values of the
        // pointer that hands it over: every argument is read before any of this function's
        // own memory is written, which some of them may be read from.
        let mut moved = smallvec::SmallVec::<[(usize, usize, Value); 4]>::new();
        for (index, (&(addr, ty), &mode)) in values.iter().zip(&arg_modes).enumerate() {
            let layout = self.layout(ty)?;
            match mode {
                PassMode::Indirect => {
                    let temporary = self.argument_slot(&layout);
                    self.copy_value(temporary, addr, &layout);
                    moved.push((index, call_values.len(), temporary));
                    call_values.push(temporary);
                }
                PassMode::OnStack { .. }
                | PassMode::SpilledPair { spilled: [Some(_), _] | [_, Some(_)], .. } => {
                    unsupported!(
                        "a tail call (`become`) that passes `{ty:?}` by value on the stack, \
                         which blaze does not make yet"
                    );
                }
                PassMode::IndirectUnsized => {
                    unsupported!("a tail call (`become`) with the unsized argument `{ty:?}`");
                }
                mode => self.pass(mode, addr, &layout, &mut call_values),
            }
        }
        for (index, value_at, temporary) in moved {
            let (_, ty) = values[index];
            let Some(Some(own)) = own_memory.get(index).copied() else {
                unsupported!(
                    "a tail call (`become`) that passes argument {index} (`{ty:?}`) by \
                     reference where {:?} is not handed its own parameter {index} so",
                    body.owner
                );
            };
            let layout = self.layout(ty)?;
            let own_ty = body.locals[body.param_locals[index]].ty.as_ref();
            let own_layout = self.layout(own_ty)?;
            if own_layout.size < layout.size || own_layout.align.abi < layout.align.abi {
                unsupported!(
                    "a tail call (`become`) that passes argument {index} (`{ty:?}`) where \
                     {:?} was handed `{own_ty:?}`, which is smaller",
                    body.owner
                );
            }
            self.copy_value(own, temporary, &layout);
            call_values[value_at] = own;
        }
        match callee {
            Callee::Instance(def, generic_args) => {
                let own = &self.builder.func.signature;
                if sig.params != own.params || sig.returns != own.returns {
                    unsupported!(
                        "a tail call (`become`) of {def:?} from {:?}, whose signatures differ \
                         in this target's convention ({sig} and {own}): a guaranteed tail \
                         call needs the caller's",
                        body.owner
                    );
                }
                let callee = self.cg.callee(def, generic_args, self.env, &sig)?;
                let callee = self.cg.declare_func_in_func(callee, self.builder.func);
                self.builder.ins().return_call(callee, &call_values);
            }
            Callee::Pointer(pointer, ..) => {
                let sig = self.builder.import_signature(sig);
                self.builder.ins().return_call_indirect(sig, pointer, &call_values);
            }
        }
        Ok(())
    }

    fn call_with(
        &mut self,
        func: &Operand,
        args: &[Operand],
        destination: &StoredPlace,
        target: Option<BasicBlockId>,
        intrinsic: Option<Intrinsic>,
    ) -> Result<()> {
        let (dst, dst_ty) = self.place(destination)?;
        let fn_ty = self.operand_ty(func)?;
        if let TyKind::FnPtr(..) = fn_ty.kind() {
            let (addr, _) = self.operand(func)?;
            let pointer = self.builder.ins().load(self.cg.target.pointer(), MEM, addr, 0);
            let values = self.operands(args)?;
            let sig = fn_ty.fn_sig(self.interner);
            self.check_abort_unwinding(sig.abi())?;
            let named = sig.c_variadic().then(|| sig.inputs().skip_binder().len());
            self.emit_call(Callee::Pointer(pointer, sig.abi(), named), &values, dst, dst_ty.ty)?;
            return self.finish_call(destination, target);
        }
        let TyKind::FnDef(def, generic_args) = fn_ty.kind() else {
            unsupported!("call of {fn_ty:?}");
        };
        let generic_args = generic_args.skip_binder();
        let func = match def.0 {
            CallableDefId::FunctionId(func) => func,
            // A direct call is a literal, which the MIR pass packs; this is the constructor as a
            // function value, called through `Fn*`.
            CallableDefId::StructId(id) if hir_ty::layout::is_bits_struct(self.cg.db, id) => {
                let name = hir_def::signatures::StructSignature::of(self.cg.db, id).name.as_str();
                refused!(
                    "the constructor of `#[blaze::bits]` struct `{name}` used as a function \
                     value: no stencil packs its fields from a call; call it in a closure, \
                     `|..| {name}(..)`"
                );
            }
            CallableDefId::StructId(id) => {
                if let Some(layout) = self.variant_layout(dst, id.into(), generic_args)? {
                    self.write_fields(dst, &layout, args)?;
                }
                return self.finish_call(destination, target);
            }
            CallableDefId::EnumVariantId(id) => {
                if let Some(layout) = self.variant_layout(dst, id.into(), generic_args)? {
                    self.write_fields(dst, &layout, args)?;
                }
                return self.finish_call(destination, target);
            }
        };
        let db = self.cg.db;
        if let ItemContainerId::ExternBlockId(_) = func.lookup(db).container {
            self.check_abort_unwinding(FunctionSignature::of(db, func).abi)?;
        }
        if fncache::is_code(db, func) {
            self.cache_code(generic_args, dst)?;
            return self.finish_call(destination, target);
        }
        let lang_items = self.interner.lang_items();
        if fn_trait_methods(lang_items).iter().any(|family| family.contains(&Some(func))) {
            self.call_fn_trait(func, generic_args, args, dst, dst_ty.ty)?;
            return self.finish_call(destination, target);
        }
        if [lang_items.DropInPlace, lang_items.DropGlue].contains(&Some(func)) {
            // The body is a placeholder for what the compiler generates.
            let [pointer] = args else {
                unsupported!("drop glue with {} arguments", args.len());
            };
            let ty = generic_args.type_at(0);
            let (src, _) = self.operand(pointer)?;
            let addr = self.builder.ins().load(self.cg.target.pointer(), MEM, src, 0);
            let meta = self.is_unsized(ty).then(|| {
                self.builder.ins().load(
                    self.cg.target.pointer(),
                    MEM,
                    src,
                    self.cg.target.meta_at(),
                )
            });
            self.drop_value(addr, meta, ty)?;
            return self.finish_call(destination, target);
        }
        if intrinsic.is_none() && FunctionSignature::is_intrinsic(db, func) {
            unsupported!(
                "a call of the intrinsic `{}` that MIR lowering did not recognize",
                FunctionSignature::of(db, func).name.as_str()
            );
        }
        if intrinsic == Some(Intrinsic::ConstEvalSelect) {
            // `const_eval_select((a, b), const_fn, runtime_fn)` is `runtime_fn(a, b)` at run time.
            let [tuple, _, runtime] = args else {
                unsupported!("const_eval_select with {} arguments", args.len());
            };
            let TyKind::FnDef(runtime, runtime_args) = self.operand_ty(runtime)?.kind() else {
                unsupported!("const_eval_select with a non-fn-item runtime function");
            };
            let runtime_args = runtime_args.skip_binder();
            let CallableDefId::FunctionId(runtime) = runtime.0 else {
                unsupported!("const_eval_select with a constructor");
            };
            let (tuple_addr, tuple_ty) = self.operand(tuple)?;
            let TyKind::Tuple(fields) = tuple_ty.kind() else {
                unsupported!("const_eval_select with non-tuple arguments");
            };
            let layout = self.layout(tuple_ty)?;
            let mut values = smallvec::SmallVec::<[(Value, Ty<'db>); 8]>::new();
            for (i, ty) in fields.iter().enumerate() {
                let offset = layout.fields.offset(i).bytes() as i64;
                values.push((self.builder.ins().iadd_imm(tuple_addr, offset), ty));
            }
            self.call_direct(runtime, runtime_args, &values, dst, dst_ty.ty)?;
            return self.finish_call(destination, target);
        }
        // Each is `ub_checks() && ..` in core, and the rest is true at run time: the check that
        // asks is then a branch on a constant, which `tier::fold_known_branches` takes out
        // where it is off.
        if intrinsic.is_none()
            && func.krate(db).data(db).origin.is_lang()
            && is_ub_check(FunctionSignature::of(db, func).name.symbol())
        {
            let value = self.builder.ins().iconst(types::I8, i64::from(self.debug_assertions));
            self.builder.ins().store(MEM, value, dst, 0);
            return self.finish_call(destination, target);
        }
        if let Some(intrinsic) = intrinsic {
            match self.intrinsic(intrinsic, generic_args, args, dst, dst_ty.ty)? {
                Lowered::Inline => return self.finish_call(destination, target),
                Lowered::Diverges => return Ok(()),
                Lowered::Unknown => {
                    let overridden = AttrFlags::query(db, func.into())
                        .contains(AttrFlags::RUSTC_INTRINSIC_MUST_BE_OVERRIDDEN);
                    // One only the const evaluator answers has nothing to run. The body of any
                    // other is what rustc runs too: `const_allocate` gives a null pointer,
                    // `const_deallocate` does nothing.
                    if overridden {
                        unsupported!("intrinsic `{intrinsic}`, which must be overridden");
                    }
                    if compile_time_only(db, func) || !FunctionSignature::of(db, func).has_body() {
                        unsupported!("intrinsic `{intrinsic}`, which has no body to run");
                    }
                }
            }
        }
        if let Some(name) = link_name_for(self.cg.db, self.cg.target, func)
            && let Some(name) = name.strip_prefix("llvm.")
        {
            self.llvm_intrinsic(name, args, dst, dst_ty.ty)?;
            return self.finish_call(destination, target);
        }
        if self.x86_inlined_fma(func, args, destination, target, dst, dst_ty.ty)? {
            return self.finish_call(destination, target);
        }
        let values = self.operands(args)?;
        self.call_direct(func, generic_args, &values, dst, dst_ty.ty)?;
        self.finish_call(destination, target)
    }

    fn operands(
        &mut self,
        operands: &[Operand],
    ) -> Result<smallvec::SmallVec<[(Value, Ty<'db>); 8]>> {
        let mut values = smallvec::SmallVec::with_capacity(operands.len());
        for operand in operands {
            values.push(self.argument(operand)?);
        }
        Ok(values)
    }

    /// An argument of a call. An unsized one (`unsized_fn_params`) is a place moved out of,
    /// and is where a pair of its address and metadata is, which `PassMode::IndirectUnsized`
    /// passes.
    fn argument(&mut self, operand: &Operand) -> Result<(Value, Ty<'db>)> {
        let (OperandKind::Copy(place) | OperandKind::Move(place)) = &operand.kind else {
            return self.operand(operand);
        };
        let ty = self.operand_ty(operand)?;
        if !self.is_unsized(ty) {
            return self.operand(operand);
        }
        let PlaceRef { addr, meta: Some(meta), .. } = self.place_ref(place)? else {
            unsupported!("unsized argument {ty:?} without metadata");
        };
        // What the operand reads, for `given_away`.
        self.operand(operand)?;
        let pair = self.scratch_pair();
        self.builder.ins().store(MEM, addr, pair, 0);
        self.builder.ins().store(MEM, meta, pair, self.cg.target.meta_at());
        Ok((pair, ty))
    }

    /// Appends the addresses of the fields of the tuple at `tuple`, in declaration order: the
    /// arguments a `rust-call` passes for it.
    fn spread_tuple(
        &mut self,
        args: &mut Vec<(Value, Ty<'db>)>,
        tuple: Value,
        ty: Ty<'db>,
    ) -> Result<()> {
        let TyKind::Tuple(fields) = ty.kind() else {
            unsupported!("the final argument of a rust-call is {ty:?}, not a tuple");
        };
        let layout = self.layout(ty)?;
        args.extend(fields.iter().enumerate().map(|(i, field)| {
            let offset = layout.fields.offset(i).bytes() as i64;
            (self.builder.ins().iadd_imm(tuple, offset), field)
        }));
        Ok(())
    }

    /// Puts arguments received one by one back in the tuple of type `ty` a body uses them as.
    fn pack_call_tuple(&mut self, ty: Ty<'db>, values: &[(Value, Ty<'db>)]) -> Result<Value> {
        let layout = self.layout(ty)?;
        let tuple = self.stack_slot(&layout);
        for (i, &(src, field_ty)) in values.iter().enumerate() {
            let size = self.layout(field_ty)?.size;
            let offset = layout.fields.offset(i).bytes() as i64;
            let field = self.builder.ins().iadd_imm(tuple, offset);
            self.copy(field, src, size);
        }
        Ok(tuple)
    }

    /// The types a function with this signature is passed, one by one: the final tuple of a
    /// `rust-call` signature is one argument per field, as its compiled body takes them.
    fn abi_inputs<'a>(abi: ExternAbi, inputs: &'a [Ty<'db>]) -> Result<Cow<'a, [Ty<'db>]>> {
        if abi != ExternAbi::RustCall {
            return Ok(Cow::Borrowed(inputs));
        }
        let Some((&last, prefix)) = inputs.split_last() else {
            unsupported!("a rust-call signature without its tuple");
        };
        let TyKind::Tuple(fields) = last.kind() else {
            unsupported!("a rust-call signature whose last parameter is {last:?}, not a tuple");
        };
        // alloc: a `rust-call` signature only; callers in `v2/refs/shims.rs` take the `Cow`
        Ok(Cow::Owned(prefix.iter().copied().chain(fields.iter()).collect()))
    }

    /// `Fn::call`, `FnMut::call_mut` or `FnOnce::call_once`. For a call expression MIR passes
    /// the callee first and then the arguments one by one, not as a tuple.
    fn call_fn_trait(
        &mut self,
        method: FunctionId,
        method_args: GenericArgs<'db>,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        let Some((callee, rest)) = args.split_first() else {
            unsupported!("Fn-trait call without a callee");
        };
        // A `dyn FnOnce` called by value, as `Box<dyn FnOnce>` does with what it holds: the
        // `call_once` slot takes a pointer to the callee and leaves it dropped.
        if let OperandKind::Copy(place) | OperandKind::Move(place) = &callee.kind {
            let place = self.place_ref(place)?;
            if let TyKind::Dynamic(..) = place.ty.ty.kind() {
                let dyn_ty = place.ty.ty;
                let Some(vtable) = place.meta else {
                    unsupported!("Fn-trait call on {dyn_ty:?} without a vtable");
                };
                // Marks the callee as moved out of.
                self.operand(callee)?;
                let Some(trait_ref) = self.principal(dyn_ty, dyn_ty) else {
                    unsupported!("vtable call on {dyn_ty:?}");
                };
                let Some(slot) = self.vtable_layout(trait_ref).method(method) else {
                    unsupported!("method missing from the vtable of {dyn_ty:?}");
                };
                let pointer = self.builder.ins().load(
                    self.cg.target.pointer(),
                    MEM,
                    vtable,
                    slot as i32 * self.cg.target.meta_at(),
                );
                let thin = self.scratch();
                self.builder.ins().store(MEM, place.addr, thin, 0);
                let thin_ty = Ty::new_imm_ptr(self.interner, Ty::new_unit(self.interner));
                // alloc: a `dyn FnOnce` called by value; `spread_tuple` takes a `Vec`
                let mut values = vec![(thin, thin_ty)];
                let tuple_ty = method_args.type_at(1);
                match (&self.operands(rest)?[..], tuple_ty.kind()) {
                    (&[(tuple, ty)], TyKind::Tuple(_)) if ty == tuple_ty => {
                        self.spread_tuple(&mut values, tuple, ty)?;
                    }
                    (rest, _) => values.extend_from_slice(rest),
                }
                let callee = Callee::Pointer(pointer, ExternAbi::Rust, None);
                return self.emit_call(callee, &values, dst, dst_ty);
            }
        }
        let (mut addr, mut ty) = self.operand(callee)?;
        let mut values = self.operands(rest)?;
        // A call written as `f.call_mut(args)` does pass the tuple, which the callee takes
        // apart all the same.
        let tuple_ty = method_args.type_at(1);
        if let (&[(tuple, ty)], TyKind::Tuple(_)) = (&values[..], tuple_ty.kind())
            && ty == tuple_ty
        {
            // alloc: only for a call written with the tuple itself; `spread_tuple` takes a `Vec`
            let mut spread = Vec::new();
            self.spread_tuple(&mut spread, tuple, ty)?;
            values = smallvec::SmallVec::from_vec(spread);
        }
        // The innermost reference to the callee and the address it is stored at.
        let mut by_ref = None;
        while let TyKind::Ref(_, inner, mutability) = ty.kind() {
            if self.implements_fn_itself(method, ty, tuple_ty).map_err(CodegenError::Refused)? {
                break;
            }
            by_ref = Some((addr, ty, mutability));
            if let TyKind::Dynamic(..) = inner.kind() {
                let Some(method) = self.fn_method_of_object(inner) else {
                    unsupported!("Fn-trait call on {inner:?}");
                };
                return self.call_virtual(method, inner, addr, ty, &values, dst, dst_ty);
            }
            addr = self.builder.ins().load(self.cg.target.pointer(), MEM, addr, 0);
            ty = inner;
        }
        match ty.kind() {
            TyKind::FnDef(def, generic_args) => {
                let func = match def.0 {
                    CallableDefId::FunctionId(func) => func,
                    CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => {
                        let TyKind::Adt(_, adt_args) = dst_ty.kind() else {
                            unsupported!("constructor of {dst_ty:?}");
                        };
                        let layout = match def.0 {
                            CallableDefId::EnumVariantId(id) => {
                                self.variant_layout(dst, id.into(), adt_args)?
                            }
                            _ => Some(self.layout(dst_ty)?),
                        };
                        let Some(layout) = layout else {
                            return Ok(());
                        };
                        for (i, &(src, ty)) in values.iter().enumerate() {
                            let size = self.layout(ty)?.size;
                            let offset = layout.fields.offset(i).bytes() as i64;
                            let field = self.builder.ins().iadd_imm(dst, offset);
                            self.copy(field, src, size);
                        }
                        return Ok(());
                    }
                };
                if FunctionSignature::is_intrinsic(self.cg.db, func) {
                    unsupported!("intrinsic called through an Fn trait");
                }
                self.call_direct(func, generic_args.skip_binder(), &values, dst, dst_ty)
            }
            TyKind::FnPtr(..) => {
                let pointer = self.builder.ins().load(self.cg.target.pointer(), MEM, addr, 0);
                let abi = ty.fn_sig(self.interner).abi();
                self.emit_call(Callee::Pointer(pointer, abi, None), &values, dst, dst_ty)
            }
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
                let (id, parent_args) = match ty.kind() {
                    TyKind::Closure(id, generic_args) => {
                        (id.0, generic_args.as_closure().parent_args())
                    }
                    TyKind::CoroutineClosure(id, generic_args) => {
                        let closure = generic_args.as_coroutine_closure();
                        if let TyKind::Coroutine(future, future_args) = dst_ty.kind()
                            && is_by_move_coroutine(
                                self.cg.db,
                                future.0,
                                future_args.as_coroutine(),
                            )
                        {
                            let captured = closure.tupled_upvars_ty();
                            return self.future_by_move(dst, dst_ty, &values, addr, ty, captured);
                        }
                        (id.0.as_closure(self.cg.db), closure.parent_args())
                    }
                    _ => unreachable!(),
                };
                let parent_args = GenericArgs::new_from_slice(parent_args);
                let body = self.body_types(InstanceDef::Closure(id.as_id()), parent_args)?;
                if body.params.len() != values.len() + 1 {
                    unsupported!("closure called with a tuple of arguments");
                }
                // The body takes the closure by reference or by value depending on its kind,
                // whichever trait method the call went through.
                let self_ty = body.params[0].as_ref();
                let self_arg = if self_ty.as_reference().is_some() {
                    let slot = self.scratch();
                    self.builder.ins().store(MEM, addr, slot, 0);
                    (slot, self_ty)
                } else {
                    (addr, ty)
                };
                values.insert(0, self_arg);
                let def = InstanceDef::Closure(id.as_id());
                let call = |this: &mut Self| {
                    this.emit_call(Callee::Instance(def, parent_args), &values, dst, dst_ty)
                };
                // A closure that is passed by value to a body that only borrows it is the
                // caller's to drop, whether the body returns or unwinds.
                if by_ref.is_none() && self_ty.as_reference().is_some() {
                    self.guarded(call, |this| this.drop_value(addr, None, ty))?;
                    self.drop_value(addr, None, ty)
                } else {
                    call(self)
                }
            }
            // A type with its own `Fn` impls, which take the arguments as a tuple.
            TyKind::Adt(..) | TyKind::Ref(..) => {
                let tuple = self.pack_call_tuple(tuple_ty, &values)?;
                let [by_shared, by_mut, _] = fn_trait_methods(self.interner.lang_items())
                    .into_iter()
                    .find(|family| family.contains(&Some(method)))
                    .expect("an Fn-trait method");
                let (method, self_arg) = match by_ref {
                    None => (Some(method), (addr, ty)),
                    Some((slot, ref_ty, Mutability::Not)) => (by_shared, (slot, ref_ty)),
                    Some((slot, ref_ty, Mutability::Mut)) => (by_mut, (slot, ref_ty)),
                };
                let Some(method) = method else {
                    unsupported!("Fn-trait call on {ty:?}");
                };
                let args = GenericArgs::new_from_slice(&[ty.into(), tuple_ty.into()]);
                self.call_direct(method, args, &[self_arg, (tuple, tuple_ty)], dst, dst_ty)
            }
            _ => unsupported!("Fn-trait call on {ty:?}"),
        }
    }

    /// The future of an `async` closure that is called by value though it could be called
    /// again: it takes over the arguments and what the closure at `closure` captured.
    fn future_by_move(
        &mut self,
        dst: Value,
        future_ty: Ty<'db>,
        arguments: &[(Value, Ty<'db>)],
        closure: Value,
        closure_ty: Ty<'db>,
        captured: Ty<'db>,
    ) -> Result<()> {
        let layout = self.layout(future_ty)?;
        let closure_layout = self.layout(closure_ty)?;
        let mut fields = smallvec::SmallVec::<[(Value, Ty<'db>); 8]>::from_slice(arguments);
        for (i, ty) in captured.tuple_fields().iter().enumerate() {
            let offset = closure_layout.fields.offset(i).bytes() as i64;
            fields.push((self.builder.ins().iadd_imm(closure, offset), ty));
        }
        for (i, (src, ty)) in fields.into_iter().enumerate() {
            let size = self.layout(ty)?.size;
            let field = self.builder.ins().iadd_imm(dst, layout.fields.offset(i).bytes() as i64);
            self.copy(field, src, size);
        }
        self.start_coroutine(dst, &layout)
    }

    /// Marks the future at `dst` as not polled yet.
    fn start_coroutine(&mut self, dst: Value, layout: &Layout) -> Result<()> {
        // The last field says where the body left off.
        let Some(tag) = CoroutineFrame::state_of(layout) else {
            unsupported!("an async block without a state");
        };
        let tag = self.scalar(tag)?.ty;
        let state = layout.fields.offset(layout.fields.count() - 1).bytes();
        let state = self.builder.ins().iadd_imm(dst, state as i64);
        self.store_state(state, tag, UNRESUMED);
        Ok(())
    }

    /// Whether the reference type `ty` has an `Fn` impl written for it, as opposed to the ones
    /// every reference to a callable has, which call what it points to. A call no impl's
    /// method is found for, other than as [`impl_method`] expects, is an error.
    fn implements_fn_itself(
        &self,
        method: FunctionId,
        ty: Ty<'db>,
        tuple_ty: Ty<'db>,
    ) -> std::result::Result<bool, String> {
        let db = self.cg.db;
        let args = GenericArgs::new_from_slice(&[ty.into(), tuple_ty.into()]);
        let (either::Either::Left(callee), _) = impl_method(db, self.env, method, args)? else {
            return Ok(false);
        };
        let ItemContainerId::ImplId(impl_) = callee.lookup(db).container else {
            return Ok(false);
        };
        Ok(match db.impl_self_ty(impl_).skip_binder().kind() {
            TyKind::Ref(_, pointee, _) => !matches!(pointee.kind(), TyKind::Param(_)),
            _ => false,
        })
    }

    /// The method `func` of the vtable of the `dyn_ty` the receiver at `receiver` points
    /// to, and where the data pointer alone is, of type `*const ()`, which it is called with.
    fn virtual_callee(
        &mut self,
        func: FunctionId,
        dyn_ty: Ty<'db>,
        receiver: Value,
        receiver_ty: Ty<'db>,
    ) -> Result<(Value, Value, Ty<'db>)> {
        let receiver_layout = self.layout(receiver_ty)?;
        // A `dyn Trait` taken by value is where the pair of its data pointer and vtable is
        // (`argument`), as a fat pointer to it would be; rustc hands the method the data
        // pointer as well.
        let by_value = matches!(receiver_ty.kind(), TyKind::Dynamic(..));
        if !by_value
            && (!matches!(receiver_layout.backend_repr, BackendRepr::ScalarPair { .. })
                || receiver_layout.size.bytes_usize() != 2 * self.cg.target.word())
        {
            unsupported!("vtable call with receiver {receiver_ty:?}");
        }
        let Some(trait_ref) = self.principal(dyn_ty, dyn_ty) else {
            unsupported!("vtable call on {dyn_ty:?}");
        };
        let Some(slot) = self.vtable_layout(trait_ref).method(func) else {
            unsupported!("method missing from the vtable of {dyn_ty:?}");
        };
        let data = self.builder.ins().load(self.cg.target.pointer(), MEM, receiver, 0);
        let vtable = self.builder.ins().load(
            self.cg.target.pointer(),
            MEM,
            receiver,
            self.cg.target.meta_at(),
        );
        let pointer = self.builder.ins().load(
            self.cg.target.pointer(),
            MEM,
            vtable,
            slot as i32 * self.cg.target.meta_at(),
        );
        // The method takes the data pointer alone where the caller holds a fat pointer.
        let thin = self.scratch();
        self.builder.ins().store(MEM, data, thin, 0);
        let thin_ty = Ty::new_imm_ptr(self.interner, Ty::new_unit(self.interner));
        Ok((pointer, thin, thin_ty))
    }

    /// Calls trait method `func` on the trait object behind the fat pointer at `receiver`.
    #[allow(clippy::too_many_arguments)]
    fn call_virtual(
        &mut self,
        func: FunctionId,
        dyn_ty: Ty<'db>,
        receiver: Value,
        receiver_ty: Ty<'db>,
        rest: &[(Value, Ty<'db>)],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        let (pointer, thin, thin_ty) = self.virtual_callee(func, dyn_ty, receiver, receiver_ty)?;
        let mut values = smallvec::SmallVec::<[(Value, Ty<'db>); 8]>::new();
        values.push((thin, thin_ty));
        values.extend_from_slice(rest);
        if track_caller(self.cg.db, func) {
            let location = self.caller_location()?;
            let slot = self.scratch();
            self.builder.ins().store(MEM, location, slot, 0);
            values.push((slot, thin_ty));
        }
        self.emit_call(Callee::Pointer(pointer, ExternAbi::Rust, None), &values, dst, dst_ty)
    }

    /// The method a call through a reference to `dyn_ty` goes to: `Fn::call` where the
    /// object's trait is `Fn` or has it above it, `FnMut::call_mut` likewise. Which one is in
    /// the vtable depends on the object's trait, not on the trait the call was written against.
    fn fn_method_of_object(&self, dyn_ty: Ty<'db>) -> Option<FunctionId> {
        let lang_items = self.interner.lang_items();
        let vtable = self.vtable_layout(self.principal(dyn_ty, dyn_ty)?);
        [lang_items.Fn_call, lang_items.FnMut_call_mut]
            .into_iter()
            .flatten()
            .find(|&method| vtable.method(method).is_some())
    }

    /// The principal trait of `dyn_ty`, as implemented by `self_ty`.
    fn principal(&self, dyn_ty: Ty<'db>, self_ty: Ty<'db>) -> Option<TraitRef<'db>> {
        let TyKind::Dynamic(bounds, _) = dyn_ty.kind() else {
            return None;
        };
        let principal = bounds.principal()?.with_self_ty(self.interner, self_ty);
        Some(self.plain_trait(self.interner.instantiate_bound_regions_with_erased(principal)))
    }

    /// `trait_ref` without its lifetimes and with its projections resolved: two ways of
    /// writing one trait are one trait in a vtable.
    fn plain_trait(&self, trait_ref: TraitRef<'db>) -> TraitRef<'db> {
        let erased = vtable::without_lifetimes(self.interner, trait_ref);
        hir_ty::next_solver::obligation_ctxt::ObligationCtxt::new(&self.infcx)
            .deeply_normalize(
                &hir_ty::next_solver::infer::traits::ObligationCause::dummy(),
                self.env.param_env,
                erased,
            )
            .unwrap_or(erased)
    }

    /// The vtable of `trait_ref`, with each supertrait in the one form `plain_trait` gives it.
    fn vtable_layout(&self, trait_ref: TraitRef<'db>) -> VtableLayout<'db> {
        VtableLayout::of(self.interner, trait_ref, &|it| self.plain_trait(it))
    }

    /// Writes the value of `from_ty` at `src` to `dst` as `to_ty`, which it is unsized to: a
    /// pointer gets its metadata, and a struct around one is unsized through its fields.
    fn unsize(&mut self, src: Value, from_ty: Ty<'db>, dst: Value, to_ty: Ty<'db>) -> Result<()> {
        let (from, to) = (self.layout(from_ty)?, self.layout(to_ty)?);
        if from_ty == to_ty {
            self.copy(dst, src, to.size);
            return Ok(());
        }
        match (from_ty.kind(), to_ty.kind()) {
            (
                TyKind::RawPtr(from_pointee, _) | TyKind::Ref(_, from_pointee, _),
                TyKind::RawPtr(to_pointee, _) | TyKind::Ref(_, to_pointee, _),
            ) => {
                let old = (from.size.bytes() == 2 * self.cg.target.word() as u64).then(|| {
                    self.builder.ins().load(
                        self.cg.target.pointer(),
                        MEM,
                        src,
                        self.cg.target.meta_at(),
                    )
                });
                let meta = self.unsize_metadata(from_pointee, to_pointee, old)?;
                let ptr = self.builder.ins().load(self.cg.target.pointer(), MEM, src, 0);
                self.builder.ins().store(MEM, ptr, dst, 0);
                self.builder.ins().store(MEM, meta, dst, self.cg.target.meta_at());
            }
            // `NonNull` holds its pointer as a pattern type.
            (TyKind::Pat(from_ty, _), TyKind::Pat(to_ty, _)) => {
                self.unsize(src, from_ty, dst, to_ty)?
            }
            (TyKind::Adt(def, args), TyKind::Adt(to_def, to_args))
                if def.def_id() == to_def.def_id() =>
            {
                let AdtId::StructId(id) = def.def_id() else {
                    unsupported!("unsizing {from_ty:?} to {to_ty:?}");
                };
                // `CoerceUnsized` changes one field, `PhantomData` aside, and where the others
                // are may change with it.
                let field_types = self.cg.db.field_types(id.into());
                for (i, (_, field)) in field_types.iter().enumerate() {
                    let from_field = field.ty().instantiate(self.interner, args).skip_norm_wip();
                    let to_field = field.ty().instantiate(self.interner, to_args).skip_norm_wip();
                    let (from_field, to_field) =
                        (self.normalize(from_field), self.normalize(to_field));
                    if self.layout(to_field)?.size.bytes() == 0 {
                        continue;
                    }
                    let src =
                        self.builder.ins().iadd_imm(src, from.fields.offset(i).bytes() as i64);
                    let dst = self.builder.ins().iadd_imm(dst, to.fields.offset(i).bytes() as i64);
                    self.unsize(src, from_field, dst, to_field)?;
                }
            }
            _ => unsupported!("unsizing {from_ty:?} to {to_ty:?}"),
        }
        Ok(())
    }

    /// The metadata of a pointer to `from` after unsizing its pointee to `to`. `old` is the
    /// metadata the pointer has now, if it is fat already.
    fn unsize_metadata(&mut self, from: Ty<'db>, to: Ty<'db>, old: Option<Value>) -> Result<Value> {
        match (from.kind(), to.kind()) {
            (TyKind::Array(_, len), TyKind::Slice(_)) => {
                let Some(len) = try_const_usize(self.cg.db, len) else {
                    unsupported!("array with unevaluated length");
                };
                Ok(self.builder.ins().iconst(self.cg.target.pointer(), len as i64))
            }
            (TyKind::Dynamic(..), TyKind::Dynamic(..)) => {
                let Some(vtable) = old else {
                    unsupported!("upcast of a thin pointer");
                };
                let (Some(trait_ref), Some(target)) =
                    (self.principal(from, from), self.principal(to, from))
                else {
                    // Dropping the principal: every vtable has the header.
                    return Ok(vtable);
                };
                if trait_ref == target {
                    // Only auto traits or lifetimes change.
                    return Ok(vtable);
                }
                match self.vtable_layout(trait_ref).upcast(target) {
                    Some(Upcast::Same) => Ok(vtable),
                    Some(Upcast::Load(slot)) => Ok(self.builder.ins().load(
                        self.cg.target.pointer(),
                        MEM,
                        vtable,
                        slot as i32 * self.cg.target.meta_at(),
                    )),
                    None => unsupported!("upcast of {from:?} to {to:?}"),
                }
            }
            (_, TyKind::Dynamic(..)) => {
                if self.is_unsized(from) {
                    unsupported!("unsizing {from:?} to {to:?}");
                }
                let trait_ref = self.principal(to, from);
                let vtable = self.vtable(from, trait_ref)?;
                let vtable = self.cg.declare_data_in_func(vtable, self.builder.func);
                Ok(self.builder.ins().symbol_value(self.cg.target.pointer(), vtable))
            }
            (TyKind::Adt(def, args), TyKind::Adt(to_def, to_args))
                if def.def_id() == to_def.def_id() =>
            {
                // A struct is unsized through its last field.
                let AdtId::StructId(id) = def.def_id() else {
                    unsupported!("unsizing {from:?}");
                };
                let field_types = self.cg.db.field_types(id.into());
                let Some((_, last)) = field_types.iter().last() else {
                    unsupported!("unsizing a struct without fields");
                };
                // The field may be an associated type of a parameter: an array of one, a slice
                // of the other.
                let from = last.ty().instantiate(self.interner, args).skip_norm_wip();
                let to = last.ty().instantiate(self.interner, to_args).skip_norm_wip();
                let (from, to) = (self.normalize(from), self.normalize(to));
                self.unsize_metadata(from, to, old)
            }
            _ => unsupported!("unsizing {from:?} to {to:?}"),
        }
    }

    /// The vtable of `ty` for `trait_ref`, or with the header alone when the trait object has
    /// no principal trait. Keyed with free regions erased, error regions too (inference's
    /// writeback makes every region variable one, `hir-ty/src/infer/unify.rs`
    /// `Resolver::fold_region`), and binders anonymized, as no lifetime changes a vtable:
    /// `&'{region error} T` and `&'static T` are one vtable, as are `fn(&T)` and
    /// `for<'a> fn(&'a T)`, and one type in the dyn facts.
    fn vtable(&mut self, ty: Ty<'db>, trait_ref: Option<TraitRef<'db>>) -> Result<DataId> {
        let ty = hir_ty_identity::instance::erased(self.interner, ty);
        let trait_ref = trait_ref.map(|it| hir_ty_identity::instance::erased(self.interner, it));
        let key = (ty.store(), trait_ref.map(|it| (it.def_id.0, it.args.store())));
        if let Some(id) = self.cg.vtable(&key) {
            return Ok(id);
        }
        let layout = self.layout(ty)?;
        let slots = trait_ref.map_or_else(Vec::new, |it| self.vtable_layout(it).slots);
        // alloc: once per vtable (the key is looked up above), the vtable's bytes
        let mut bytes = vec![0u8; (vtable::HEADER + slots.len()) * self.cg.target.word()];
        bytes[self.cg.target.word()..2 * self.cg.target.word()]
            .copy_from_slice(&layout.size.bytes().to_le_bytes()[..self.cg.target.word()]);
        bytes[2 * self.cg.target.word()..3 * self.cg.target.word()]
            .copy_from_slice(&layout.align.abi.bytes().to_le_bytes()[..self.cg.target.word()]);
        let mut blob = ConstBlob {
            bytes,
            // alloc: once per vtable, the pointers to its supertraits' vtables
            relocs: Vec::new(),
            // alloc: once per vtable, its drop glue and methods
            fn_relocs: Vec::new(),
            align: self.cg.target.word() as u64,
            type_ids: Vec::new(), // alloc: empty, allocates nothing
        };
        if self.glue_of(ty) != DropGlue::None {
            let mut sig = self.cg.make_signature();
            sig.params.push(AbiParam::new(self.cg.target.pointer()));
            let args = GenericArgs::new_from_slice(&[ty.into()]);
            let glue = self.cg.callee(InstanceDef::DropGlue, args, self.env, &sig)?;
            blob.fn_relocs.push((0, glue));
        }
        for (index, slot) in slots.into_iter().enumerate() {
            let offset = ((vtable::HEADER + index) * self.cg.target.word()) as u32;
            match slot {
                // A slot nothing can call through is left empty, as rustc leaves it.
                Slot::Method(func, trait_ref) if self.method_bounds_hold(func, trait_ref) => {
                    let method = self.vtable_method(func, trait_ref)?;
                    blob.fn_relocs.push((offset, method));
                }
                Slot::Method(..) => {}
                Slot::Vptr(trait_ref) => {
                    let vptr = self.vtable(ty, Some(trait_ref))?;
                    blob.relocs.push((offset, vptr, 0));
                }
            }
        }
        Ok(self.cg.define_vtable(key, blob))
    }

    /// What trait method `func` is called with through the vtable of `trait_ref`: only
    /// lifetimes can be generic in a method that is in a vtable.
    fn vtable_method_args(
        &self,
        func: FunctionId,
        trait_ref: TraitRef<'db>,
    ) -> Option<GenericArgs<'db>> {
        let interner = self.interner;
        let mut generic = false;
        let args =
            GenericArgs::fill_rest(interner, func.into(), trait_ref.args.iter(), |_, id, _| {
                generic |= !matches!(id, hir_def::GenericParamId::LifetimeParamId(_));
                Region::new_erased(interner).into()
            });
        (!generic).then_some(args)
    }

    /// Whether the `where` clauses trait method `func` has of its own hold for `trait_ref`.
    /// A method they do not hold for can never be called.
    fn method_bounds_hold(&self, func: FunctionId, trait_ref: TraitRef<'db>) -> bool {
        let Some(args) = self.vtable_method_args(func, trait_ref) else { return true };
        let interner = self.interner;
        let cause = hir_ty::next_solver::infer::traits::ObligationCause::dummy();
        let mut ocx = hir_ty::next_solver::obligation_ctxt::ObligationCtxt::new(&self.infcx);
        let bounds = hir_ty::GenericPredicates::query_own_explicit(self.cg.db, func.into());
        for bound in bounds.iter_instantiated(interner, args.as_slice()) {
            ocx.register_obligation(hir_ty::next_solver::infer::traits::Obligation::new(
                interner,
                cause,
                self.env.param_env,
                bound.skip_norm_wip(),
            ));
        }
        ocx.try_evaluate_obligations().is_empty()
    }

    /// The function a vtable slot for trait method `func` points to.
    fn vtable_method(&mut self, func: FunctionId, trait_ref: TraitRef<'db>) -> Result<FuncId> {
        let db = self.cg.db;
        let interner = self.interner;
        let self_ty = trait_ref.args.type_at(0);
        let lang_items = interner.lang_items();
        let [fn_methods, _] = fn_trait_methods(lang_items);
        let once = Some(func) == lang_items.FnOnce_call_once;
        let written =
            !matches!(self_ty.kind(), TyKind::Closure(..) | TyKind::FnDef(..) | TyKind::FnPtr(..));
        if fn_methods.contains(&Some(func)) && !written {
            return self.vtable_fn_method(self_ty, once);
        }
        // A type with an impl of the trait, as `Box<dyn Fn()>` and `&mut dyn FnMut()` have:
        // the slot is called with the data pointer and the arguments one by one. The method is
        // `rust-call` and so takes the arguments one by one as well, but `call_once` takes the
        // receiver by value, which the slot has a pointer to.
        if fn_methods.contains(&Some(func)) {
            let Some(callee_args) = self.vtable_method_args(func, trait_ref) else {
                unsupported!("generic `Fn`-trait method in a vtable");
            };
            let (callee, callee_args) =
                impl_method(db, self.env, func, callee_args).map_err(CodegenError::Refused)?;
            let either::Either::Left(callee) = callee else {
                unsupported!("builtin `Fn`-trait method in the vtable of {self_ty:?}");
            };
            if !FunctionSignature::of(db, callee).has_body() {
                let method = FunctionSignature::of(db, callee).name.as_str();
                unsupported!("no impl of `{method}` found for the vtable of {self_ty:?}");
            }
            let body = self.body_types(InstanceDef::Fn(callee), callee_args)?;
            let (_, abi) = self.spread_shim_abi(body)?;
            let sig = self.signature(&abi.args, abi.ret);
            return self.cg.callee(InstanceDef::VtableShim(callee), callee_args, self.env, &sig);
        }
        if Some(func) == lang_items.FuturePoll
            && let Some((def, parent_args)) = coroutine_instance(db, self_ty)
        {
            let sig = self.body_signature(self.body_types(def, parent_args)?)?;
            return self.cg.callee(def, parent_args, self.env, &sig);
        }
        let Some(args) = self.vtable_method_args(func, trait_ref) else {
            unsupported!("generic method in a vtable");
        };
        let (callee, callee_args) =
            impl_method(db, self.env, func, args).map_err(CodegenError::Refused)?;
        let callee = match callee {
            either::Either::Left(callee) => callee,
            either::Either::Right(derived) => {
                let derived = unexpanded_derive(db, derived, self_ty);
                unsupported!("{derived}, in a vtable");
            }
        };
        if !FunctionSignature::of(db, callee).has_body() {
            let method = FunctionSignature::of(db, callee).name.as_str();
            unsupported!("no impl of `{method}` found for the vtable of {self_ty:?}");
        }
        let body = self.body_types(InstanceDef::Fn(callee), callee_args)?;
        let Some(receiver) = body.params.first() else {
            unsupported!("vtable method without a receiver");
        };
        // A by-value `self` reaches the method through a pointer and needs a shim, unless the
        // method takes it through a pointer too. rustc's shim (`ShimKind::VTable`) is of the
        // trait's method, so one that tracks its caller only in the impl is given the place of
        // the trait's declaration by the shim however `self` is passed.
        if receiver.as_ref() == self_ty {
            let layout = self.layout(self_ty)?;
            let mode = self.pass_mode(&layout, false)?;
            if !matches!(mode, PassMode::Indirect) || track_caller(db, callee) {
                let mut sig = self.body_signature(body)?;
                if track_caller(db, func) {
                    sig.params.push(AbiParam::new(self.cg.target.pointer()));
                }
                let sig = Self::by_pointer(&sig, mode, self.cg.target.pointer());
                return self.cg.callee(
                    InstanceDef::VtableShim(callee),
                    callee_args,
                    self.env,
                    &sig,
                );
            }
        }
        // A method declared `#[track_caller]` in the trait is given a location by the call. One
        // that is so only in the impl is called without, and the shim supplies it.
        if track_caller(db, callee) && !track_caller(db, func) {
            let def = CallableDefId::FunctionId(callee).into();
            return self.reified(Ty::new_fn_def(interner, def, callee_args), false);
        }
        let mut sig = self.body_signature(body)?;
        if track_caller(db, callee) {
            sig.params.push(AbiParam::new(self.cg.target.pointer()));
        }
        self.cg.callee(InstanceDef::Fn(callee), callee_args, self.env, &sig)
    }

    /// `call`, `call_mut` or `call_once` in the vtable of a closure.
    fn vtable_fn_method(&mut self, ty: Ty<'db>, once: bool) -> Result<FuncId> {
        let TyKind::Closure(id, generic_args) = ty.kind() else {
            return self.reified(ty, true);
        };
        let parent_args = GenericArgs::new_from_slice(generic_args.as_closure().parent_args());
        let body = self.body_types(InstanceDef::Closure(id.0.as_id()), parent_args)?;
        let sig = self.body_signature(body)?;
        let Some(receiver) = body.params.first() else {
            unsupported!("closure without a receiver");
        };
        let receiver_ty = receiver.as_ref();
        let closure = InstanceDef::Closure(id.0.as_id());
        if receiver_ty.as_reference().is_none() {
            // The body takes the closure by value. Only when that means "through a pointer to
            // memory the body owns" is it what the `call_once` slot is called with.
            if !once {
                let kind = generic_args.as_closure().kind();
                unsupported!(
                    "vtable shim for a closure taken by value, in the `call` or `call_mut` slot \
                     of a closure of kind {kind:?} (a body takes its closure by value only for \
                     `FnOnce`)"
                );
            }
            let layout = self.layout(receiver_ty)?;
            let mode = self.pass_mode(&layout, false)?;
            if matches!(mode, PassMode::Indirect) {
                return self.cg.callee(closure, parent_args, self.env, &sig);
            }
            let sig = Self::by_pointer(&sig, mode, self.cg.target.pointer());
            let args = GenericArgs::new_from_slice(&[ty.into()]);
            return self.cg.callee(InstanceDef::CallOnceShim, args, self.env, &sig);
        }
        if once {
            let args = GenericArgs::new_from_slice(&[ty.into()]);
            return self.cg.callee(InstanceDef::CallOnceShim, args, self.env, &sig);
        }
        self.cg.callee(closure, parent_args, self.env, &sig)
    }

    /// The `ReifyShim` of a fn item or fn pointer type.
    fn reified(&mut self, ty: Ty<'db>, receiver: bool) -> Result<FuncId> {
        let sig = self.reify_signature(ty, receiver)?.1;
        let args = GenericArgs::new_from_slice(&[ty.into()]);
        self.cg.callee(InstanceDef::ReifyShim { receiver }, args, self.env, &sig)
    }

    /// What a `ReifyShim` is called with and how.
    fn reify_signature(
        &mut self,
        ty: Ty<'db>,
        receiver: bool,
    ) -> Result<(rustc_type_ir::FnSig<DbInterner<'db>>, Signature, FnAbi)> {
        if !matches!(ty.kind(), TyKind::FnDef(..) | TyKind::FnPtr(..)) {
            unsupported!("Fn-trait vtable of {ty:?}");
        }
        let fn_sig = self.interner.instantiate_bound_regions_with_erased(ty.fn_sig(self.interner));
        if fn_sig.c_variadic() {
            unsupported!("shim for a variadic function");
        }
        // A vtable is called as a Rust function, whatever the function behind it is, and its
        // receiver is an argument like the rest: where it goes moves where they do.
        let abi = if receiver { ExternAbi::Rust } else { fn_sig.abi() };
        let pointer = receiver.then(|| Ty::new_imm_ptr(self.interner, Ty::new_unit(self.interner)));
        let inputs = Self::abi_inputs(abi, fn_sig.inputs())?;
        let inputs = pointer.into_iter().chain(inputs.iter().copied());
        let mut abi = self.fn_abi(abi, fn_sig.output(), inputs)?;
        let sig = self.signature(&abi.args, abi.ret);
        if receiver {
            abi.args.remove(0);
        }
        Ok((fn_sig, sig, abi))
    }

    /// The body of a `ReifyShim`: calls what the type names with the arguments it is handed.
    fn reify_shim(&mut self, ty: Ty<'db>, receiver: bool) -> Result<()> {
        let (fn_sig, sig, FnAbi { ret, args: modes }) = self.reify_signature(ty, receiver)?;
        self.builder.func.signature = sig;
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let mut incoming =
            smallvec::SmallVec::<[Value; 16]>::from_slice(self.builder.block_params(entry))
                .into_iter();
        let dst_ty = fn_sig.output();
        let dst_layout = self.layout(dst_ty)?;
        let dst = match ret {
            PassMode::Indirect => incoming.next().expect("sret param"),
            _ => self.stack_slot(&dst_layout),
        };
        let receiver = if receiver { incoming.next() } else { None };
        let abi = if receiver.is_some() { ExternAbi::Rust } else { fn_sig.abi() };
        let inputs = Self::abi_inputs(abi, fn_sig.inputs())?;
        let mut values = smallvec::SmallVec::<[(Value, Ty<'db>); 8]>::new();
        for (&ty, mode) in inputs.iter().zip(modes) {
            let layout = self.layout(ty)?;
            let addr = match mode {
                PassMode::OnStack { padded: true, .. } => {
                    incoming.nth(1).expect("a parameter in memory after its padding")
                }
                PassMode::Indirect | PassMode::OnStack { .. } => {
                    incoming.next().expect("indirect param")
                }
                PassMode::IndirectUnsized => self.scratch_pair(),
                _ => self.stack_slot(&layout),
            };
            self.receive(mode, &mut incoming, addr, &layout);
            values.push((addr, ty));
        }
        // What the callee is called with has the tuple, which the call spreads again.
        if abi == ExternAbi::RustCall {
            let (&tuple, prefix) =
                fn_sig.inputs().split_last().expect("abi_inputs checked the tuple");
            let addr = self.pack_call_tuple(tuple, &values[prefix.len()..])?;
            values.truncate(prefix.len());
            values.push((addr, tuple));
        }
        match ty.kind() {
            TyKind::FnDef(def, generic_args) => match def.0 {
                CallableDefId::FunctionId(func) => {
                    let generic_args = generic_args.skip_binder();
                    if FunctionSignature::is_intrinsic(self.cg.db, func) {
                        unsupported!("shim for an intrinsic");
                    }
                    let db = self.cg.db;
                    let (callee, _) = impl_method(db, self.env, func, generic_args)
                        .map_err(CodegenError::Refused)?;
                    if let either::Either::Left(callee) = callee
                        && track_caller(db, callee)
                    {
                        let location = self.cg.declaration(callee);
                        let location = self.cg.declare_data_in_func(location, self.builder.func);
                        self.location = Some(
                            self.builder.ins().symbol_value(self.cg.target.pointer(), location),
                        );
                    }
                    self.call_direct(func, generic_args, &values, dst, dst_ty)?;
                }
                CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => {
                    let TyKind::Adt(_, adt_args) = dst_ty.kind() else {
                        unsupported!("constructor of {dst_ty:?}");
                    };
                    let layout = match def.0 {
                        CallableDefId::EnumVariantId(id) => {
                            self.variant_layout(dst, id.into(), adt_args)?
                        }
                        _ => Some(self.layout(dst_ty)?),
                    };
                    if let Some(layout) = layout {
                        for (i, &(src, ty)) in values.iter().enumerate() {
                            let size = self.layout(ty)?.size;
                            let offset = layout.fields.offset(i).bytes() as i64;
                            let field = self.builder.ins().iadd_imm(dst, offset);
                            self.copy(field, src, size);
                        }
                    }
                }
            },
            _ => {
                let receiver = receiver.expect("a fn pointer is only reified for a vtable");
                let pointer = self.builder.ins().load(self.cg.target.pointer(), MEM, receiver, 0);
                self.emit_call(Callee::Pointer(pointer, fn_sig.abi(), None), &values, dst, dst_ty)?;
            }
        }
        // alloc: once per shim; `pass` takes a `Vec`
        let mut results = Vec::new();
        if !matches!(ret, PassMode::Indirect) {
            self.pass(ret, dst, &dst_layout, &mut results);
        }
        self.builder.ins().return_(&results);
        Ok(())
    }

    /// The body of the `main` the C runtime calls: `lang_start` with the program's `main`,
    /// as rustc writes it.
    fn start_shim(&mut self, main: FunctionId) -> Result<()> {
        let Some(start) = self.interner.lang_items().Start else {
            let krate = main.krate(self.cg.db).named(self.cg.db);
            unsupported!(
                "the C `main` of crate `{krate}`: no `start` lang item (std's `lang_start`) in \
                 the crates `{krate}` loads, which its `main` is handed to"
            );
        };
        let fn_ty = |func: FunctionId, args| {
            Ty::new_fn_def(self.interner, CallableDefId::FunctionId(func).into(), args)
        };
        let sig_of = |ty: Ty<'db>| {
            self.interner.instantiate_bound_regions_with_erased(ty.fn_sig(self.interner))
        };
        let main_ty = fn_ty(main, GenericArgs::new_from_slice(&[]));
        let returned = self.normalize(sig_of(main_ty).output());
        let start_args = GenericArgs::new_from_slice(&[returned.into()]);
        let start_sig = sig_of(fn_ty(start, start_args));
        let (_, with_arguments) = self.cg.target.entry();
        let mut sig = Signature::new(self.cg.isa.default_call_conv());
        if with_arguments {
            // alloc: once per program, the signature of its `main`
            sig.params = vec![AbiParam::new(types::I32), AbiParam::new(self.cg.target.pointer())];
        }
        // alloc: once per program, the signature of its `main`
        sig.returns = vec![AbiParam::new(types::I32)];
        self.builder.func.signature = sig;
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let (argc, argv) = match *self.builder.block_params(entry) {
            [argc, argv] => (argc, argv),
            [] => (
                self.builder.ins().iconst(types::I32, 0),
                self.builder.ins().iconst(self.cg.target.pointer(), 0),
            ),
            _ => unsupported!("`main` is called with a count and the arguments"),
        };
        // `argc` is an `isize` for `lang_start`.
        let argc = if self.cg.target.pointer() == types::I32 {
            argc
        } else {
            self.builder.ins().sextend(self.cg.target.pointer(), argc)
        };
        let callee = self.fn_item(main_ty)?;
        let callee = self.cg.declare_func_address_in_func(callee, self.builder.func);
        let pointer = self.builder.ins().func_addr(self.cg.target.pointer(), callee);
        // What `SIGPIPE` does is left as the runtime has it.
        let sigpipe = self.builder.ins().iconst(types::I8, 0);
        let &[main_param, argc_ty, argv_ty, sigpipe_ty] = start_sig.inputs().as_slice() else {
            unsupported!("a `lang_start` that takes other than four arguments");
        };
        let mut values = smallvec::SmallVec::<[(Value, Ty<'db>); 4]>::new();
        for (value, ty) in
            [(pointer, main_param), (argc, argc_ty), (argv, argv_ty), (sigpipe, sigpipe_ty)]
        {
            let ty = self.normalize(ty);
            let layout = self.layout(ty)?;
            let slot = self.stack_slot(&layout);
            self.builder.ins().store(MEM, value, slot, 0);
            values.push((slot, ty));
        }
        let code_ty = start_sig.output();
        let layout = self.layout(code_ty)?;
        let code = self.stack_slot(&layout);
        self.call_direct(start, start_args, &values, code, code_ty)?;
        let code = self.builder.ins().load(self.cg.target.pointer(), MEM, code, 0);
        let code = if self.cg.target.pointer() == types::I32 {
            code
        } else {
            self.builder.ins().ireduce(types::I32, code)
        };
        self.builder.ins().return_(&[code]);
        Ok(())
    }

    /// The function a closure that captures nothing is as a function pointer.
    fn closure_fn_pointer(&mut self, ty: Ty<'db>) -> Result<FuncId> {
        let TyKind::Closure(id, generic_args) = ty.kind() else {
            unsupported!("fn pointer to {ty:?}");
        };
        let parent_args = GenericArgs::new_from_slice(generic_args.as_closure().parent_args());
        let body = self.body_types(InstanceDef::Closure(id.0.as_id()), parent_args)?;
        let Some(receiver) = body.params.first() else {
            unsupported!("closure without a receiver");
        };
        if receiver.as_ref().as_reference().is_none() {
            // Taken by value, a closure without captures is not passed at all.
            let sig = self.body_signature(body)?;
            return self.cg.callee(InstanceDef::Closure(id.0.as_id()), parent_args, self.env, &sig);
        }
        let abi = self.closure_pointer_abi(body)?;
        let sig = self.signature(&abi.args, abi.ret);
        let args = GenericArgs::new_from_slice(&[ty.into()]);
        self.cg.callee(InstanceDef::FnPointerShim, args, self.env, &sig)
    }

    /// How a fn pointer made of a closure that takes itself by reference is called: as the
    /// closure body without its receiver.
    fn closure_pointer_abi(&self, body: &BodyTypes) -> Result<FnAbi> {
        let params = body.params.iter().skip(1).map(|ty| ty.as_ref());
        self.fn_abi(ExternAbi::Rust, body.returned.as_ref(), params)
    }

    /// The body of a `FnPointerShim`: calls the closure body with a receiver, passing on the
    /// arguments, which go elsewhere when the receiver moves them along the registers.
    fn fn_pointer_shim(&mut self, ty: Ty<'db>) -> Result<()> {
        let TyKind::Closure(id, generic_args) = ty.kind() else {
            unsupported!("fn pointer shim for {ty:?}");
        };
        let parent_args = GenericArgs::new_from_slice(generic_args.as_closure().parent_args());
        let body = self.body_types(InstanceDef::Closure(id.0.as_id()), parent_args)?;
        let sig = self.body_signature(body)?;
        let abi = self.closure_pointer_abi(body)?;
        let shim = self.signature(&abi.args, abi.ret);
        let sret = matches!(abi.ret, PassMode::Indirect);
        // Whether the shim's parameters are the body's but for the receiver.
        let (before, after) = sig.params.split_at(sret as usize);
        let after = &after[1..];
        let forwarded = shim.params.len() == before.len() + after.len()
            && shim.params[..before.len()] == *before
            && shim.params[before.len()..] == *after;
        self.builder.func.signature = shim;
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        // alloc: once per shim, the values it passes on, which `pass` adds to as a `Vec`
        let params = self.builder.block_params(entry).to_vec();
        let receiver = self.builder.ins().iconst(self.cg.target.pointer(), 1);
        let values = if forwarded {
            let mut values = params;
            values.insert(sret as usize, receiver);
            values
        } else {
            let to = self.fn_abi(
                ExternAbi::Rust,
                body.returned.as_ref(),
                body.params.iter().map(|ty| ty.as_ref()),
            )?;
            let mut incoming = params.into_iter();
            // alloc: once per shim; `pass` takes a `Vec`
            let mut values = Vec::new();
            if sret {
                values.push(incoming.next().expect("sret param"));
            }
            values.push(receiver);
            for ((ty, mode), &to) in body.params.iter().skip(1).zip(abi.args).zip(&to.args[1..]) {
                let layout = self.layout(ty.as_ref())?;
                let addr = match mode {
                    PassMode::OnStack { padded: true, .. } => {
                        incoming.nth(1).expect("a parameter in memory after its padding")
                    }
                    PassMode::Indirect | PassMode::OnStack { .. } => {
                        incoming.next().expect("indirect param")
                    }
                    PassMode::IndirectUnsized => self.scratch_pair(),
                    _ => self.stack_slot(&layout),
                };
                self.receive(mode, &mut incoming, addr, &layout);
                self.pass(to, addr, &layout, &mut values);
            }
            values
        };
        let callee =
            self.cg.callee(InstanceDef::Closure(id.0.as_id()), parent_args, self.env, &sig)?;
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        let results = self.invoke(callee, &values);
        self.builder.ins().return_(&results);
        Ok(())
    }

    /// The body of a `VtableShim`: calls the method with the value the shim is handed a
    /// pointer to.
    fn vtable_shim(&mut self, func: FunctionId, args: GenericArgs<'db>) -> Result<()> {
        if let Some(method) = implemented_fn_method(self.cg.db, self.interner.lang_items(), func) {
            return self.spread_shim(func, args, method);
        }
        let body = self.body_types(InstanceDef::Fn(func), args)?;
        let mut sig = self.body_signature(body)?;
        let Some(receiver) = body.params.first() else {
            unsupported!("vtable method without a receiver");
        };
        let layout = self.layout(receiver.as_ref())?;
        let mode = self.pass_mode(&layout, false)?;
        // The shim is handed the caller's place when the trait's declaration tracks it, and
        // passes it on; otherwise a method that tracks it is given the declaration's place, the
        // span of rustc's shim.
        let declared = declared_method(self.cg.db, func);
        let handed = track_caller(self.cg.db, declared);
        if handed {
            sig.params.push(AbiParam::new(self.cg.target.pointer()));
        }
        let placed = (!handed && track_caller(self.cg.db, func)).then_some(declared);
        self.call_by_pointer(func, args, &sig, mode, &layout, placed)
    }

    /// What the vtable slot for an `Fn`-trait method written for a type, whose body is
    /// `body`, is called with: the data pointer of the object, then the arguments one by one,
    /// as every call through an `Fn`-trait vtable passes them. The method is `rust-call`, so
    /// its body takes the arguments one by one too, after the receiver; this also gives the
    /// tuple they make, which a call of the method is handed.
    fn spread_shim_abi(&self, body: &BodyTypes) -> Result<(Ty<'db>, FnAbi)> {
        let Some((_, fields)) = body.params.split_first() else {
            unsupported!("an `Fn`-trait method without a receiver");
        };
        let fields: smallvec::SmallVec<[Ty<'db>; 8]> =
            fields.iter().map(|it| it.as_ref()).collect(); // alloc: none, inline
        let tuple = Ty::new_tup(self.interner, &fields);
        let thin = Ty::new_imm_ptr(self.interner, Ty::new_unit(self.interner));
        let inputs = std::iter::once(thin).chain(fields);
        Ok((tuple, self.fn_abi(ExternAbi::Rust, body.returned.as_ref(), inputs)?))
    }

    /// The body of a `VtableShim` for `func`, the `Fn`-trait method `method` written for a
    /// type: puts the arguments it is handed one by one in the tuple a call of `func` is
    /// written with, which the call spreads again, and hands `func` the receiver, which for
    /// `call_once` it has a pointer to.
    fn spread_shim(
        &mut self,
        func: FunctionId,
        args: GenericArgs<'db>,
        method: FunctionId,
    ) -> Result<()> {
        if track_caller(self.cg.db, func) {
            unsupported!("an `Fn`-trait method in a vtable that tracks its caller");
        }
        let body = self.body_types(InstanceDef::Fn(func), args)?;
        let receiver_ty = body.params[0].as_ref();
        let dst_ty = body.returned.as_ref();
        let (tuple_ty, FnAbi { ret, args: modes }) = self.spread_shim_abi(body)?;
        self.builder.func.signature = self.signature(&modes, ret);
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let mut incoming =
            smallvec::SmallVec::<[Value; 8]>::from_slice(self.builder.block_params(entry))
                .into_iter();
        let dst_layout = self.layout(dst_ty)?;
        let dst = match ret {
            PassMode::Indirect => incoming.next().expect("sret param"),
            _ => self.stack_slot(&dst_layout),
        };
        let thin = Ty::new_imm_ptr(self.interner, Ty::new_unit(self.interner));
        let tuple_layout = self.layout(tuple_ty)?;
        let tuple = self.stack_slot(&tuple_layout);
        let inputs = std::iter::once(thin).chain(tuple_ty.tuple_fields().iter());
        let mut data = None;
        for (i, (ty, mode)) in inputs.zip(modes).enumerate() {
            let layout = self.layout(ty)?;
            let addr = match mode {
                PassMode::OnStack { padded: true, .. } => {
                    incoming.nth(1).expect("a parameter in memory after its padding")
                }
                PassMode::Indirect | PassMode::OnStack { .. } => {
                    incoming.next().expect("indirect param")
                }
                _ => self.stack_slot(&layout),
            };
            self.receive(mode, &mut incoming, addr, &layout);
            match i.checked_sub(1) {
                None => data = Some(addr),
                Some(field) => {
                    let offset = tuple_layout.fields.offset(field).bytes() as i64;
                    let at = self.builder.ins().iadd_imm(tuple, offset);
                    self.copy(at, addr, layout.size);
                }
            }
        }
        let data = data.expect("the data pointer of the object");
        // `call` and `call_mut` borrow the object, which is what the data pointer is.
        // `call_once` takes it by value, and the data pointer is where it is.
        let receiver = match Some(method) == self.interner.lang_items().FnOnce_call_once {
            true => self.builder.ins().load(self.cg.target.pointer(), MEM, data, 0),
            false => data,
        };
        let values = [(receiver, receiver_ty), (tuple, tuple_ty)];
        self.emit_call(Callee::Instance(InstanceDef::Fn(func), args), &values, dst, dst_ty)?;
        let mut results = Vec::with_capacity(self.builder.func.signature.returns.len());
        if !matches!(ret, PassMode::Indirect) {
            self.pass(ret, dst, &dst_layout, &mut results);
        }
        self.builder.ins().return_(&results);
        Ok(())
    }

    /// The body of a shim that is handed a pointer to the receiver of `callee`, which takes
    /// it by value as `mode` says, and, with `placed`, the place that function is declared
    /// as its caller's.
    fn call_by_pointer(
        &mut self,
        callee: FunctionId,
        callee_args: GenericArgs<'db>,
        sig: &Signature,
        mode: PassMode,
        layout: &Layout,
        placed: Option<FunctionId>,
    ) -> Result<()> {
        self.builder.func.signature = Self::by_pointer(sig, mode, self.cg.target.pointer());
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let mut params = Vec::with_capacity(sig.params.len() + usize::from(placed.is_some()));
        params.extend_from_slice(self.builder.block_params(entry));
        let sret = sig.params.first().is_some_and(|it| it.purpose == ArgumentPurpose::StructReturn);
        let Some(&receiver) = params.get(sret as usize) else {
            unsupported!("shim for a receiver that is not passed");
        };
        let with_place;
        let callee_sig = match placed {
            Some(_) => {
                // clone: a `#[track_caller]` method's shim only, whose callee takes the place too
                let mut with = sig.clone();
                with.params.push(AbiParam::new(self.cg.target.pointer()));
                with_place = with;
                &with_place
            }
            None => sig,
        };
        let callee = self.cg.callee(InstanceDef::Fn(callee), callee_args, self.env, callee_sig)?;
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        params.remove(sret as usize);
        let rest = params.len();
        self.pass(mode, receiver, layout, &mut params);
        let passed = params.len() - rest;
        params[sret as usize..].rotate_right(passed);
        if let Some(declared) = placed {
            let location = self.cg.declaration(declared);
            let location = self.cg.declare_data_in_func(location, self.builder.func);
            params.push(self.builder.ins().symbol_value(self.cg.target.pointer(), location));
        }
        let results = self.invoke(callee, &params);
        self.builder.ins().return_(&results);
        Ok(())
    }

    /// The body of a `CallOnceShim`: same signature as the closure body, which borrows the
    /// closure, but the shim owns it and so drops it after the call.
    fn call_once_shim(&mut self, ty: Ty<'db>) -> Result<()> {
        let TyKind::Closure(id, generic_args) = ty.kind() else {
            unsupported!("call_once shim for {ty:?}");
        };
        let parent_args = GenericArgs::new_from_slice(generic_args.as_closure().parent_args());
        let body = self.body_types(InstanceDef::Closure(id.0.as_id()), parent_args)?;
        let sig = self.body_signature(body)?;
        let Some(receiver) = body.params.first() else {
            unsupported!("closure without a receiver");
        };
        let receiver_ty = receiver.as_ref();
        let by_value = receiver_ty.as_reference().is_none();
        let layout = self.layout(receiver_ty)?;
        let mode = self.pass_mode(&layout, false)?;
        let sret = sig.params.first().is_some_and(|it| it.purpose == ArgumentPurpose::StructReturn);
        let count = sig.params.len();
        let callee_sig = if by_value {
            self.builder.func.signature = Self::by_pointer(&sig, mode, self.cg.target.pointer());
            Some(sig)
        } else {
            self.builder.func.signature = sig;
            None
        };
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let mut params = Vec::with_capacity(count);
        params.extend_from_slice(self.builder.block_params(entry));
        let Some(&closure) = params.get(sret as usize) else {
            unsupported!("call_once shim for a closure that is not passed");
        };
        let sig = callee_sig.as_ref().unwrap_or(&self.builder.func.signature);
        let callee =
            self.cg.callee(InstanceDef::Closure(id.0.as_id()), parent_args, self.env, sig)?;
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        if by_value {
            // The body owns the closure and takes it in registers.
            params.remove(sret as usize);
            let rest = params.len();
            self.pass(mode, closure, &layout, &mut params);
            let passed = params.len() - rest;
            params[sret as usize..].rotate_right(passed);
            let results = self.invoke(callee, &params);
            self.builder.ins().return_(&results);
            return Ok(());
        }
        let results = self.invoke(callee, &params);
        self.drop_value(closure, None, ty)?;
        self.builder.ins().return_(&results);
        Ok(())
    }

    /// `sig` with the receiver, which is passed as `mode` says, behind a pointer instead.
    fn by_pointer(sig: &Signature, mode: PassMode, pointer: Type) -> Signature {
        let sret = sig.params.first().is_some_and(|it| it.purpose == ArgumentPurpose::StructReturn);
        let count = match mode {
            PassMode::Ignore => 0,
            PassMode::Pair(..) | PassMode::IndirectUnsized => 2,
            PassMode::SpilledPair { spilled, .. } => {
                spilled.iter().map(|it| it.map_or(1, |padded| 1 + usize::from(padded))).sum()
            }
            PassMode::SkippingPair { skipped, .. } => {
                2 + skipped.into_iter().map(usize::from).sum::<usize>()
            }
            PassMode::Uniform { count, skipped, .. } => usize::from(count) + usize::from(skipped),
            _ => 1,
        };
        let mut sig = sig.clone(); // clone: the shim's own signature, built from the body's
        sig.params.splice(sret as usize..sret as usize + count, [AbiParam::new(pointer)]);
        sig
    }

    /// Calls `func` with arguments that live at the given addresses.
    fn call_direct(
        &mut self,
        func: FunctionId,
        generic_args: GenericArgs<'db>,
        args: &[(Value, Ty<'db>)],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        let db = self.cg.db;
        let callee = dispatch::callee(&self.infcx, self.env, func, generic_args)
            .map_err(|why| CodegenError::Refused(no_impl_method(db, func, generic_args, why)))?;
        let (callee, callee_args) = match callee {
            dispatch::Callee::Poll(body, parent_args) => {
                let callee = Callee::Instance(instance_def(body), parent_args);
                return self.emit_call(callee, args, dst, dst_ty);
            }
            dispatch::Callee::Virtual => {
                let Some((&(receiver, receiver_ty), rest)) = args.split_first() else {
                    unsupported!("vtable call without a receiver");
                };
                let dyn_ty = generic_args.type_at(0);
                return self.call_virtual(func, dyn_ty, receiver, receiver_ty, rest, dst, dst_ty);
            }
            dispatch::Callee::Derived(impl_, method) => {
                let derived = unexpanded_derive(db, (impl_, method), generic_args.type_at(0));
                unsupported!("call to {derived}");
            }
            dispatch::Callee::CloneBuiltin(callee_args) => {
                if let &[(receiver, _)] = args {
                    let src = self.builder.ins().load(self.cg.target.pointer(), MEM, receiver, 0);
                    return self.clone_builtin(func, src, dst, dst_ty);
                }
                (func, callee_args)
            }
            // `FnPtr::addr` is the compiler's to supply as well: a function pointer is its address.
            dispatch::Callee::FnPtr(callee_args) => {
                if let &[(receiver, _)] = args {
                    let addr = self.builder.ins().load(self.cg.target.pointer(), MEM, receiver, 0);
                    self.builder.ins().store(MEM, addr, dst, 0);
                    return Ok(());
                }
                (func, callee_args)
            }
            dispatch::Callee::Fn(callee, callee_args) => (callee, callee_args),
        };
        self.emit_call(Callee::Instance(InstanceDef::Fn(callee), callee_args), args, dst, dst_ty)
    }

    /// `Clone::clone` of a type whose impl the compiler supplies: tuples and arrays clone their
    /// parts, and what is left is `Copy`.
    fn clone_builtin(
        &mut self,
        clone: FunctionId,
        src: Value,
        dst: Value,
        ty: Ty<'db>,
    ) -> Result<()> {
        let layout = self.layout(ty)?;
        let clone_part = |this: &mut Self, src: Value, dst: Value, part: Ty<'db>| {
            let reference = Ty::new_ref(
                this.interner,
                Region::new_erased(this.interner),
                part,
                Mutability::Not,
            );
            let slot = this.scratch();
            this.builder.ins().store(MEM, src, slot, 0);
            let args = GenericArgs::new_from_slice(&[part.into()]);
            this.call_direct(clone, args, &[(slot, reference)], dst, part)
        };
        // A closure clones as the tuple of what it captured.
        let fields = match ty.kind() {
            TyKind::Tuple(fields) => Some(fields),
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
                match ty.closure_upvars_ty().map(|it| it.kind()) {
                    Some(TyKind::Tuple(fields)) => Some(fields),
                    _ => unsupported!("builtin `Clone` of a closure without tupled upvars"),
                }
            }
            _ => None,
        };
        match ty.kind() {
            _ if let Some(fields) = fields => {
                for (i, field) in fields.iter().enumerate() {
                    let offset = layout.fields.offset(i).bytes() as i64;
                    let src = self.builder.ins().iadd_imm(src, offset);
                    let part = self.builder.ins().iadd_imm(dst, offset);
                    self.guarded(
                        |this| clone_part(this, src, part, field),
                        |this| {
                            for (done, field) in fields.iter().enumerate().take(i) {
                                let offset = layout.fields.offset(done).bytes() as i64;
                                let done = this.builder.ins().iadd_imm(dst, offset);
                                this.drop_value(done, None, field)?;
                            }
                            Ok(())
                        },
                    )?;
                }
            }
            TyKind::Array(element, _) => {
                let size = self.layout(element)?.size.bytes() as i64;
                let len = self
                    .builder
                    .ins()
                    .iconst(self.cg.target.pointer(), layout.fields.count() as i64);
                let header = self.builder.create_block();
                let body = self.builder.create_block();
                let done = self.builder.create_block();
                self.builder.append_block_param(header, self.cg.target.pointer());
                let zero = self.builder.ins().iconst(self.cg.target.pointer(), 0);
                self.builder.ins().jump(header, &[zero.into()]);
                self.builder.switch_to_block(header);
                let index = self.builder.block_params(header)[0];
                let more = self.builder.ins().icmp(IntCC::UnsignedLessThan, index, len);
                self.builder.ins().brif(more, body, &[], done, &[]);
                self.builder.switch_to_block(body);
                let offset = self.builder.ins().imul_imm(index, size);
                let src = self.builder.ins().iadd(src, offset);
                let part = self.builder.ins().iadd(dst, offset);
                self.guarded(
                    |this| clone_part(this, src, part, element),
                    |this| this.drop_elements(dst, index, element),
                )?;
                let next = self.builder.ins().iadd_imm(index, 1);
                self.builder.ins().jump(header, &[next.into()]);
                self.builder.switch_to_block(done);
            }
            TyKind::FnDef(..)
            | TyKind::FnPtr(..)
            | TyKind::Never
            | TyKind::RawPtr(..)
            | TyKind::Ref(_, _, Mutability::Not) => self.copy(dst, src, layout.size),
            _ => unsupported!("builtin `Clone` of {ty:?}"),
        }
        Ok(())
    }

    /// The body of a callee that is small and of the standard library, built to be put where
    /// it is called instead of compiled as a function of its own. rustc gets such a function
    /// with its helpers already inlined into it, and so compiles far fewer of them.
    fn inlined_callee(
        &mut self,
        def: InstanceDef,
        args: GenericArgs<'db>,
        sig: &Signature,
    ) -> Option<ir::FuncRef> {
        let db = self.cg.db;
        // A v2 unit builds in only the `#[inline(always)]` callees of crates no edit reaches,
        // whose places its walk lists (`v2/refs.rs` `Walk::instance_call`).
        if self.cg.unit.is_some() {
            return self.always_inlined(def, args, sig);
        }
        if self.cg.target.writes_wasm_objects() {
            return self.always_inlined(def, args, sig);
        }
        if !self.cg.target.exception_in_register() || self.cg.target.is_wasm() {
            return None;
        }
        let erased = |args| erase_regions(DbInterner::new_with(db, self.env.krate), args);
        // A closure is built in by a compile at a level above 1 alone.
        if let InstanceDef::Closure(_) = def {
            let instance = Instance { def, args: erased(args).store(), checked: self.cg.checked };
            let env = self.env.store();
            let limit = tier::inline_limit(db, self.cg.inlining.as_mut(), &instance)?;
            let body = tier::hot_body(db, &mut self.cg.inlining, &instance, &env, limit, false)?;
            return (body.func.signature == *sig)
                .then(|| self.cg.declare_inlined(&instance, &body, self.builder.func));
        }
        let InstanceDef::Fn(func) = def else { return None };
        let own =
            self.body.is_some_and(|it| it.owner.module(db).krate(db).data(db).origin.is_local());
        if fncache::kept_a_call(db, func, args, own) {
            return None;
        }
        let signature = FunctionSignature::of(db, func);
        if signature.abi != ExternAbi::Rust
            || signature.is_varargs()
            || FunctionSignature::is_intrinsic(db, func)
        {
            return None;
        }
        let args = erased(args);
        let instance = Instance { def, args: args.store(), checked: self.cg.checked };
        // A compile at a level above 1 builds in what it calls that is hot or small, of any
        // crate, as rustc's optimized builds do.
        let env = self.env.store();
        if let Some(limit) = tier::inline_limit(db, self.cg.inlining.as_mut(), &instance)
            && let Some(body) = tier::hot_body(
                db,
                &mut self.cg.inlining,
                &instance,
                &env,
                limit,
                track_caller(db, func),
            )
            && body.func.signature == *sig
        {
            return Some(self.cg.declare_inlined(&instance, &body, self.builder.func));
        }
        if track_caller(db, func) || tier::builds_in_nothing(self.cg.inlining.as_ref()) {
            return None;
        }
        if let Some(callee) = self.always_inlined(def, args, sig) {
            return Some(callee);
        }
        if !func.krate(db).data(db).origin.is_lang() {
            return None;
        }
        // A function that is neither generic nor `#[inline]` is in the library as rustc
        // compiled it, and calling that one is less work than building it again.
        let flags = AttrFlags::query(db, func.into());
        if !flags.contains(AttrFlags::IS_INLINE) && args.iter().all(|it| it.region().is_some()) {
            return None;
        }
        let body = inlinable(db, instance, self.env.store()).as_ref()?;
        if body.func.signature != *sig {
            return None;
        }
        Some(self.cg.declare_inlined(&instance, body, self.builder.func))
    }

    /// The body of an `#[inline(always)]` callee of a crate no edit changes, which rustc
    /// builds into every caller at every level: LLVM's always-inliner runs at -O0 too, and
    /// the hot loops of a dependency (`memchr`'s, `aho-corasick`'s, `regex-automata`'s with
    /// `perf-inline`) are written as such callees. What reads a wasm module counts on it:
    /// wasm-bindgen finds each of its descriptor shims by the call of its marker import,
    /// which `describe_generic_import` has to put in the shim itself.
    fn always_inlined(
        &mut self,
        def: InstanceDef,
        args: GenericArgs<'db>,
        sig: &Signature,
    ) -> Option<ir::FuncRef> {
        let (instance, body) = self.always_inlinable_callee(def, args)?;
        (body.func.signature == *sig)
            .then(|| self.cg.declare_inlined(&instance, body, self.builder.func))
    }

    /// The callee [`Self::always_inlined`] builds in and its body, without the signature
    /// check or declaring it: the v2 walk (`v2/refs.rs` `Walk::instance_call`) asks this, so
    /// the walk and the IR decide what is built in by one function. The body is built under
    /// the unit's settings (`cg.unit`), which key it.
    fn always_inlinable_callee(
        &self,
        def: InstanceDef,
        args: GenericArgs<'db>,
    ) -> Option<(Instance, &'db fragment::Inlinable)> {
        let db = self.cg.db;
        let InstanceDef::Fn(func) = def else { return None };
        let signature = FunctionSignature::of(db, func);
        if !AttrFlags::query(db, func.into()).contains(AttrFlags::INLINE_ALWAYS)
            || func.krate(db).data(db).origin.is_local()
            || signature.abi != ExternAbi::Rust
            || signature.is_varargs()
            || FunctionSignature::is_intrinsic(db, func)
            || track_caller(db, func)
        {
            return None;
        }
        let args = erase_regions(DbInterner::new_with(db, self.env.krate), args);
        let instance = Instance { def, args: args.store(), checked: self.cg.checked };
        // A body built into this one moves no allocation to the stack (`Build::built_in`).
        let unit = self.cg.unit.map(v2::build::Build::built_in);
        let body = always_inlinable(db, instance, self.env.store(), unit).as_ref()?;
        Some((instance, body))
    }

    /// Shows LLVM the body of the callee `id` a compile at a level above 1 calls, where
    /// `tier::seen_body` has one.
    fn show_callee(
        &mut self,
        def: InstanceDef,
        args: GenericArgs<'db>,
        sig: &Signature,
        id: FuncId,
    ) {
        if self.cg.unit.is_some() {
            return;
        }
        let db = self.cg.db;
        let tracked = match def {
            InstanceDef::Fn(func) => {
                let signature = FunctionSignature::of(db, func);
                if signature.abi != ExternAbi::Rust
                    || signature.is_varargs()
                    || FunctionSignature::is_intrinsic(db, func)
                {
                    return;
                }
                track_caller(db, func)
            }
            InstanceDef::Closure(_) | InstanceDef::DropGlue => false,
            _ => return,
        };
        let args = erase_regions(DbInterner::new_with(db, self.env.krate), args);
        let instance = Instance { def, args: args.store(), checked: self.cg.checked };
        let env = self.env.store();
        if let Some((body, hint)) =
            tier::seen_body(db, &mut self.cg.inlining, &instance, &env, tracked)
        {
            if body.func.signature == *sig {
                self.cg.declare_seen(id, &body, hint);
            } else if dumps_tier() {
                eprintln!("tier: signature differs: {} vs {}", body.func.signature, sig);
            }
        }
    }

    /// Calls `callee` with `values`, which are what `sig` says, and stores the result in `dst`.
    fn call_as(
        &mut self,
        callee: Callee<'db>,
        sig: Signature,
        values: &[Value],
        ret_mode: PassMode,
        dst: Value,
        ret_layout: &Layout,
    ) -> Result<()> {
        let results = match callee {
            Callee::Instance(def, generic_args) => {
                let callee = match self.inlined_callee(def, generic_args, &sig) {
                    Some(callee) => callee,
                    None => {
                        let callee = self.cg.callee(def, generic_args, self.env, &sig)?;
                        self.show_callee(def, generic_args, &sig, callee);
                        self.cg.declare_func_in_func(callee, self.builder.func)
                    }
                };
                self.invoke(callee, values)
            }
            Callee::Pointer(pointer, ..) => {
                let sig = self.builder.import_signature(sig);
                self.invoke_indirect(sig, pointer, values)
            }
        };
        self.receive(ret_mode, &mut results.into_iter(), dst, ret_layout);
        Ok(())
    }

    /// Passes the arguments, which live at the given addresses, the way the Rust ABI wants them
    /// and stores the result in `dst`.
    fn emit_call(
        &mut self,
        callee: Callee<'db>,
        args: &[(Value, Ty<'db>)],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        let ret_layout = self.layout(dst_ty)?;
        let abi = match callee {
            Callee::Instance(InstanceDef::Fn(func), _) => {
                FunctionSignature::of(self.cg.db, func).abi
            }
            Callee::Instance(..) => ExternAbi::Rust,
            Callee::Pointer(_, abi, _) => abi,
        };
        // The final tuple of a `rust-call` is passed one field at a time.
        let spread;
        let args = if abi == ExternAbi::RustCall {
            let Some((&(tuple, ty), fixed)) = args.split_last() else {
                unsupported!("a rust-call without its tuple");
            };
            let fields = match ty.kind() {
                TyKind::Tuple(fields) => fields.len(),
                _ => 0,
            };
            let mut values = Vec::with_capacity(fixed.len() + fields);
            values.extend_from_slice(fixed);
            self.spread_tuple(&mut values, tuple, ty)?;
            spread = values;
            &spread[..]
        } else {
            args
        };
        let FnAbi { ret: ret_mode, args: arg_modes } =
            self.fn_abi(abi, dst_ty, args.iter().map(|&(_, ty)| ty))?;
        let mut sig = self.signature(&arg_modes, ret_mode);
        // One more for the caller's location of a `#[track_caller]` callee.
        let mut values = Vec::with_capacity(sig.params.len() + 1);
        if let PassMode::Indirect = ret_mode {
            values.push(dst);
        }
        // The calls before this one have returned.
        self.free_argument_slots.append(&mut self.argument_slots);
        for (&(addr, ty), &mode) in args.iter().zip(&arg_modes) {
            let layout = self.layout(ty)?;
            self.pass(mode, addr, &layout, &mut values);
        }
        if let Callee::Instance(InstanceDef::Fn(func), _) = callee
            && track_caller(self.cg.db, func)
        {
            sig.params.push(AbiParam::new(self.cg.target.pointer()));
            values.push(self.caller_location()?);
        }
        // How many of the arguments are for named parameters, when the rest are for `...`.
        let named = match callee {
            Callee::Instance(InstanceDef::Fn(func), _) => {
                let signature = FunctionSignature::of(self.cg.db, func);
                signature.is_varargs().then(|| signature.params.len())
            }
            Callee::Instance(..) => None,
            Callee::Pointer(_, _, named) => named,
        };
        let Some(named) = named else {
            return self.call_as(callee, sig, &values, ret_mode, dst, &ret_layout);
        };
        if self.cg.target.is_wasm() {
            // The arguments after the named ones are written to memory, each at its
            // own alignment, and the function is given where.
            if arg_modes.iter().any(|mode| !matches!(mode, PassMode::Direct(_))) {
                unsupported!("variadic call with an argument that is not one scalar");
            }
            let rest = values.split_off(named);
            sig.params.truncate(named);
            sig.params.push(AbiParam::new(self.cg.target.pointer()));
            let mut offsets = Vec::with_capacity(rest.len());
            let mut size = 0u32;
            for &value in &rest {
                let bytes = self.builder.func.dfg.value_type(value).bytes();
                size = size.next_multiple_of(bytes);
                offsets.push(size);
                size += bytes;
            }
            let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                size.max(1),
                4,
            ));
            let buffer = self.builder.ins().stack_addr(self.cg.target.pointer(), slot, 0);
            for (value, offset) in rest.into_iter().zip(offsets) {
                self.builder.ins().store(MEM, value, buffer, offset as i32);
            }
            values.push(buffer);
            return self.call_as(callee, sig, &values, ret_mode, dst, &ret_layout);
        }
        // Each call of a variadic function has a signature of its own, and the
        // function is declared with the one of its named parameters.
        let mut params = Vec::new(); // alloc: one buffer, for a call of a C `...` function only
        let named = arg_modes[..named]
            .iter()
            .map(|&mode| {
                params.clear();
                self.push_abi_params(mode, &mut params);
                params.len()
            })
            .sum::<usize>()
            + usize::from(matches!(ret_mode, PassMode::Indirect));
        let mut pointer = match callee {
            Callee::Instance(def, generic_args) => {
                // clone: the declaration's own signature, of the named parameters of a `...` call
                let mut declared = sig.clone();
                declared.params.truncate(named);
                if let InstanceDef::Fn(func) = def {
                    self.declare_variadic(func, &mut declared)?;
                }
                let callee = self.cg.callee(def, generic_args, self.env, &declared)?;
                let callee = self.cg.declare_func_address_in_func(callee, self.builder.func);
                self.builder.ins().func_addr(self.cg.target.pointer(), callee)
            }
            Callee::Pointer(pointer, ..) => pointer,
        };
        let floats = sig.params.iter().any(|param| param.value_type.is_float());
        match (self.cg.target.call_conv, self.cg.target.arch) {
            (isa::CallConv::AppleAarch64, _) => self.apple_variadic(&mut sig, &mut values, named),
            (isa::CallConv::SystemV, Arch::X86_64) if floats => {
                pointer = self.counted_variadic(&mut sig, &mut values, pointer)?;
            }
            (isa::CallConv::WindowsFastcall, _) if floats => {
                // The callee reads its variable arguments from where it saved the
                // integer registers, so a float goes in one of those.
                for (param, value) in sig.params[named..].iter_mut().zip(&mut values[named..]) {
                    if param.value_type == types::F64 {
                        let flags = MemFlagsData::new();
                        *value = self.builder.ins().bitcast(types::I64, flags, *value);
                        *param = AbiParam::new(types::I64);
                    }
                }
            }
            // The variable arguments are passed as named ones are.
            (isa::CallConv::SystemV, Arch::Aarch64) => (),
            _ if floats => unsupported!("float passed to a variadic function"),
            _ => (),
        }
        let sig = self.builder.import_signature(sig);
        let results = self.invoke_indirect(sig, pointer, &values);
        self.receive(ret_mode, &mut results.into_iter(), dst, &ret_layout);
        Ok(())
    }

    /// Makes a call of the variadic function at `callee` say how many vector registers hold
    /// arguments, as the System V convention of x86-64 has it say in `al`. Cranelift has no
    /// way to, so the call goes through a function that says it, which takes the address of
    /// the callee as one more argument after the others. Returns its address.
    fn counted_variadic(
        &mut self,
        sig: &mut Signature,
        values: &mut Vec<Value>,
        callee: Value,
    ) -> Result<Value> {
        let (mut ints, mut floats, mut stack) = (0, 0, 0);
        for param in &sig.params {
            match param.purpose {
                ArgumentPurpose::Normal | ArgumentPurpose::StructReturn => (),
                _ => unsupported!("an argument in memory with a float to a variadic function"),
            }
            match param.value_type {
                ty if ty.is_float() && floats < 8 => floats += 1,
                ty if ty.is_float() => stack += 1,
                ty if ty.is_int() && ty.bytes() <= 8 && ints < 6 => ints += 1,
                ty if ty.is_int() && ty.bytes() <= 8 => stack += 1,
                _ => unsupported!("a float and a wide argument to a variadic function"),
            }
        }
        sig.params.push(AbiParam::new(self.cg.target.pointer()));
        values.push(callee);
        let through = self.cg.assembly(asm::counted_variadic(ints, stack), sig);
        let through = self.cg.declare_func_address_in_func(through, self.builder.func);
        Ok(self.builder.ins().func_addr(self.cg.target.pointer(), through))
    }

    /// Makes a call pass what follows its first `named` parameters the way Apple passes the
    /// variable arguments of a C function on AArch64: each in eight bytes of stack, whatever
    /// registers are free. Cranelift knows no such thing, so arguments that carry nothing
    /// take the registers that are left.
    fn apple_variadic(&mut self, sig: &mut Signature, values: &mut Vec<Value>, named: usize) {
        let (mut ints, mut floats) = (8usize, 8usize);
        for param in &sig.params[..named] {
            match param.purpose {
                ArgumentPurpose::StructReturn => (),
                _ if param.value_type.is_float() => floats = floats.saturating_sub(1),
                _ => ints = ints.saturating_sub(param.value_type.bytes().div_ceil(8) as usize),
            }
        }
        for (param, value) in sig.params[named..].iter_mut().zip(&mut values[named..]) {
            if param.value_type.is_int() && param.value_type.bits() < 64 {
                *value = match param.extension {
                    ir::ArgumentExtension::Sext => self.builder.ins().sextend(types::I64, *value),
                    _ => self.builder.ins().uextend(types::I64, *value),
                };
                *param = AbiParam::new(types::I64);
            }
        }
        let zero = self.builder.ins().iconst(types::I64, 0);
        let zero_float = self.builder.ins().f64const(0.0);
        sig.params.splice(
            named..named,
            std::iter::repeat_n(AbiParam::new(types::I64), ints)
                .chain(std::iter::repeat_n(AbiParam::new(types::F64), floats)),
        );
        values.splice(
            named..named,
            std::iter::repeat_n(zero, ints).chain(std::iter::repeat_n(zero_float, floats)),
        );
    }

    fn intrinsic(
        &mut self,
        intrinsic: Intrinsic,
        generic_args: GenericArgs<'db>,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<Lowered> {
        use Intrinsic as I;
        use SimdOp as S;
        if self.scalable_intrinsic(intrinsic, generic_args, args, dst, dst_ty)? {
            return Ok(Lowered::Inline);
        }
        if self.wasm_simd_intrinsic(intrinsic, args, dst, dst_ty)? {
            return Ok(Lowered::Inline);
        }
        match (intrinsic, args) {
            (
                I::WrappingAdd
                | I::UncheckedAdd
                | I::WrappingSub
                | I::UncheckedSub
                | I::WrappingMul
                | I::UncheckedMul
                | I::UncheckedDiv
                | I::ExactDiv
                | I::UncheckedRem
                | I::UncheckedShl
                | I::UncheckedShr
                | I::RotateLeft
                | I::RotateRight
                | I::SaturatingAdd
                | I::SaturatingSub
                | I::ThreeWayCompare,
                [lhs, rhs],
            ) => {
                let (l, scalar) = self.load_scalar(lhs)?;
                let (r, r_scalar) = self.load_scalar(rhs)?;
                let signed = scalar.signed;
                if intrinsic != I::ThreeWayCompare
                    && let Some(int) = NarrowInt::of(self.operand_ty(lhs)?)
                {
                    let r_ty = self.operand_ty(rhs)?;
                    let value = self.narrow_binary_intrinsic(intrinsic, (l, r), r_ty, int)?;
                    self.builder.ins().store(MEM, value, dst, 0);
                    return Ok(Lowered::Inline);
                }
                let r = match intrinsic {
                    I::UncheckedShl | I::UncheckedShr => self.shift_amount_word(r).0,
                    _ => r,
                };
                let value = match intrinsic {
                    I::WrappingAdd | I::UncheckedAdd => self.builder.ins().iadd(l, r),
                    I::WrappingSub | I::UncheckedSub => self.builder.ins().isub(l, r),
                    I::WrappingMul | I::UncheckedMul => self.builder.ins().imul(l, r),
                    I::UncheckedDiv | I::ExactDiv => self.divide(l, r, true, signed)?,
                    I::UncheckedRem => self.divide(l, r, false, signed)?,
                    I::UncheckedShl => self.builder.ins().ishl(l, r),
                    I::UncheckedShr if signed => self.builder.ins().sshr(l, r),
                    I::UncheckedShr => self.builder.ins().ushr(l, r),
                    I::RotateLeft => self.builder.ins().rotl(l, r),
                    I::RotateRight => self.builder.ins().rotr(l, r),
                    I::ThreeWayCompare => {
                        let (gt, lt) = if signed {
                            (IntCC::SignedGreaterThan, IntCC::SignedLessThan)
                        } else {
                            (IntCC::UnsignedGreaterThan, IntCC::UnsignedLessThan)
                        };
                        let gt = self.builder.ins().icmp(gt, l, r);
                        let lt = self.builder.ins().icmp(lt, l, r);
                        self.builder.ins().isub(gt, lt)
                    }
                    _ => self.saturating(intrinsic == I::SaturatingAdd, l, r, scalar),
                };
                let _ = r_scalar;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::AddWithOverflow | I::SubWithOverflow | I::MulWithOverflow, [lhs, rhs]) => {
                let (l, scalar) = self.load_scalar(lhs)?;
                let (r, _) = self.load_scalar(rhs)?;
                let wide = intrinsic == I::MulWithOverflow && scalar.ty == types::I128;
                let ins = self.builder.ins();
                let (value, overflow) = match (intrinsic, scalar.signed) {
                    (_, signed) if wide => self.wide_mul_overflow(l, r, signed)?,
                    (I::AddWithOverflow, true) => ins.sadd_overflow(l, r),
                    (I::AddWithOverflow, false) => ins.uadd_overflow(l, r),
                    (I::SubWithOverflow, true) => ins.ssub_overflow(l, r),
                    (I::SubWithOverflow, false) => ins.usub_overflow(l, r),
                    (_, true) => ins.smul_overflow(l, r),
                    (_, false) => ins.umul_overflow(l, r),
                };
                let (value, overflow) = match NarrowInt::of(self.operand_ty(lhs)?) {
                    Some(int) => {
                        let mul = intrinsic == I::MulWithOverflow;
                        let (value, out) = self.narrow_overflowing(int, value, Some(overflow), mul);
                        (value, out.unwrap_or(overflow))
                    }
                    None => (value, overflow),
                };
                let layout = self.layout(dst_ty)?;
                self.builder.ins().store(MEM, value, dst, layout.fields.offset(0).bytes() as i32);
                self.builder.ins().store(
                    MEM,
                    overflow,
                    dst,
                    layout.fields.offset(1).bytes() as i32,
                );
            }
            (
                I::Ctpop
                | I::Ctlz
                | I::CtlzNonzero
                | I::Cttz
                | I::CttzNonzero
                | I::Bswap
                | I::Bitreverse,
                [arg],
            ) => {
                let (value, from) = self.load_scalar(arg)?;
                let value = match NarrowInt::of(self.operand_ty(arg)?) {
                    Some(int) => self.narrow_bits_intrinsic(intrinsic, value, int)?,
                    None => match intrinsic {
                        I::Ctpop => self.builder.ins().popcnt(value),
                        I::Ctlz | I::CtlzNonzero => self.builder.ins().clz(value),
                        I::Cttz | I::CttzNonzero => self.builder.ins().ctz(value),
                        I::Bswap if from.ty == types::I8 => value,
                        I::Bswap => self.builder.ins().bswap(value),
                        _ => self.builder.ins().bitrev(value),
                    },
                };
                let to = self.scalar_of(dst_ty)?;
                let value = self.int_cast(value, AbiScalar { signed: false, ..from }, to);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::SizeOf | I::AlignOf, []) => {
                let ty = generic_args.type_at(0);
                // rustc's size of a scalable vector is that at the least vector length, not of
                // the storage codegen gives it.
                let layout = self
                    .cg
                    .db
                    .layout_of_ty(ty.store(), self.env.store())
                    // alloc: the error's message
                    .map_err(|e| CodegenError::Frontend(format!("layout of {ty:?}: {e:?}")))?;
                let bytes = match intrinsic {
                    I::SizeOf => layout.size.bytes(),
                    _ => layout.align.abi.bytes(),
                };
                let value = self.builder.ins().iconst(self.cg.target.pointer(), bytes as i64);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::TypeName, []) => {
                let ty = generic_args.type_at(0);
                let Some(name) = hir_ty_print::rustc_type_name(self.cg.db, ty) else {
                    unsupported!("`type_name` of `{ty:?}`");
                };
                let len = self.builder.ins().iconst(self.cg.target.pointer(), name.len() as i64);
                let data = self.cg.define_const(ConstBlob {
                    bytes: name.into_bytes(),
                    relocs: Vec::new(),    // alloc: empty, allocates nothing
                    fn_relocs: Vec::new(), // alloc: empty, allocates nothing
                    align: 1,
                    type_ids: Vec::new(), // alloc: empty, allocates nothing
                })?;
                let data = self.cg.declare_data_in_func(data, self.builder.func);
                let ptr = self.builder.ins().symbol_value(self.cg.target.pointer(), data);
                self.builder.ins().store(MEM, ptr, dst, 0);
                self.builder.ins().store(MEM, len, dst, self.cg.target.meta_at());
            }
            (I::NeedsDrop, []) => {
                let ty = generic_args.type_at(0);
                let needs = self.glue_of(ty) != DropGlue::None;
                let value = self.builder.ins().iconst(types::I8, i64::from(needs));
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::UbChecks, []) => {
                let value = self.builder.ins().iconst(types::I8, i64::from(self.ub_checks));
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::IsValStaticallyKnown, _) => {
                let value = self.builder.ins().iconst(types::I8, 0);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            // `x as D` between two integers of any width, standard or not (core's items of the
            // integers of `#![blaze::arbitrary_ints]` are written with it): what `IntToInt`
            // does, the destination's held form after, and never a panic.
            (I::BlazeIntCast, [arg]) => {
                let (from_ty, to_ty) = (generic_args.type_at(0), generic_args.type_at(1));
                let int = |ty: Ty<'db>| matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_));
                if !int(from_ty) || !int(to_ty) {
                    unsupported!(
                        "intrinsic `blaze_int_cast` from `{from_ty:?}` to `{to_ty:?}`, which are not both integers"
                    );
                }
                let (value, from) = self.load_scalar(arg)?;
                let to = self.scalar_of(to_ty)?;
                let value = self.int_cast(value, from, to);
                let value = self.narrow_cast(value, from_ty, from, to_ty);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            // rustc's MIR keeps it for codegen only where the body checks overflow itself or
            // inherits its caller's checks, and is false everywhere else.
            (I::OverflowChecks, []) => {
                let checks = self.checks_overflow()?;
                let value = self.builder.ins().iconst(types::I8, i64::from(checks));
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::VaArg, [list])
                if self.cg.target.variable_arguments() == Some(VariableArguments::Buffer) =>
            {
                let scalar = self.scalar_of(dst_ty)?;
                // What is wider is in pieces whose places the caller decides with LLVM.
                if scalar.ty.bytes() > 8 {
                    unsupported!("`va_arg` of {dst_ty:?}");
                }
                // The list is the address after the argument before, and each is as aligned
                // as it is wide.
                let bytes = i64::from(scalar.ty.bytes());
                let (list, _) = self.load_scalar(list)?;
                let after = self.builder.ins().load(self.cg.target.pointer(), MEM, list, 0);
                let slot = self.builder.ins().iadd_imm(after, bytes - 1);
                let slot = self.builder.ins().band_imm(slot, -bytes);
                let next = self.builder.ins().iadd_imm(slot, bytes);
                self.builder.ins().store(MEM, next, list, 0);
                self.copy(dst, slot, Size::from_bytes(bytes as u64));
            }
            (I::VaArg, [list])
                if matches!(
                    self.cg.target.variable_arguments(),
                    Some(VariableArguments::Homed | VariableArguments::Stack)
                ) =>
            {
                let size = self.layout(dst_ty)?.size;
                let bytes = size.bytes() as i64;
                let homed = self.cg.target.variable_arguments() == Some(VariableArguments::Homed);
                // Windows has what does not fill a register exactly behind an address, which
                // is what the caller passed: a copy of its own.
                let by_reference = homed && !matches!(bytes, 1 | 2 | 4 | 8);
                // Apple has a scalar wider than eight bytes at a multiple of its size.
                let aligned = !homed && bytes > 8;
                if aligned {
                    self.scalar_of(dst_ty)?;
                }
                // The list is the address of the next argument, each of them in eight bytes,
                // or in as many as it is wide.
                let (list, _) = self.load_scalar(list)?;
                let mut slot = self.builder.ins().load(self.cg.target.pointer(), MEM, list, 0);
                if aligned {
                    slot = self.builder.ins().iadd_imm(slot, bytes - 1);
                    slot = self.builder.ins().band_imm(slot, -bytes);
                }
                let step = if homed { 8 } else { (bytes + 7) & !7 };
                let next = self.builder.ins().iadd_imm(slot, step);
                self.builder.ins().store(MEM, next, list, 0);
                let from = match by_reference {
                    true => self.builder.ins().load(self.cg.target.pointer(), MEM, slot, 0),
                    false => slot,
                };
                self.copy(dst, from, size);
            }
            (I::VaArg, [list]) => {
                if self.cg.target.variable_arguments() != Some(VariableArguments::Registers) {
                    unsupported!("intrinsic `va_arg` for this target");
                }
                let arch = self.cg.target.arch;
                let scalar = self.scalar_of(dst_ty)?;
                // A 128-bit integer is in two registers that start at an even one, or in
                // sixteen bytes of the stack at a multiple of sixteen.
                let wide = scalar.ty == types::I128 && arch == Arch::Aarch64;
                if f16_f128::soft(scalar.ty) {
                    unsupported!("`va_arg` of {dst_ty:?}");
                }
                if scalar.ty.bytes() > 8 && !wide {
                    unsupported!("`va_arg` of {dst_ty:?}");
                }
                let float = scalar.ty.is_float();
                let step = match (float, wide) {
                    (true, _) => VariadicRegisters::FLOAT_BYTES,
                    (false, true) => 16,
                    (false, false) => 8,
                };
                let (list, _) = self.load_scalar(list)?;
                let [saved, stack, done] = [(); 3].map(|()| self.builder.create_block());
                self.builder.append_block_param(done, self.cg.target.pointer());
                // An argument is in the registers the function saved while its kind of
                // register lasts, and on the stack after that.
                let stack_at = if arch == Arch::Aarch64 {
                    // The list counts up to zero, from the end of what was saved.
                    let (at, end) = if float { (28, 16) } else { (24, 8) };
                    let offset = self.builder.ins().load(types::I32, MEM, list, at);
                    let some = self.builder.create_block();
                    let none =
                        self.builder.ins().icmp_imm(IntCC::SignedGreaterThanOrEqual, offset, 0);
                    self.builder.ins().brif(none, stack, &[], some, &[]);

                    // What does not fit in the registers left takes them out of use.
                    self.builder.switch_to_block(some);
                    let offset = match wide {
                        true => {
                            let up = self.builder.ins().iadd_imm(offset, 15);
                            self.builder.ins().band_imm(up, -16)
                        }
                        false => offset,
                    };
                    let next = self.builder.ins().iadd_imm(offset, i64::from(step));
                    self.builder.ins().store(MEM, next, list, at);
                    let fits = self.builder.ins().icmp_imm(IntCC::SignedLessThanOrEqual, next, 0);
                    self.builder.ins().brif(fits, saved, &[], stack, &[]);

                    self.builder.switch_to_block(saved);
                    let area = self.builder.ins().load(self.cg.target.pointer(), MEM, list, end);
                    let far = self.builder.ins().sextend(self.cg.target.pointer(), offset);
                    let from = self.builder.ins().iadd(area, far);
                    self.builder.ins().jump(done, &[from.into()]);
                    0
                } else {
                    let ints = VariadicRegisters::int_count(arch) * 8;
                    let (at, end) = match float {
                        false => (0, ints),
                        true => {
                            (4, ints + VariadicRegisters::FLOATS * VariadicRegisters::FLOAT_BYTES)
                        }
                    };
                    let offset = self.builder.ins().load(types::I32, MEM, list, at);
                    let fits = self.builder.ins().icmp_imm(
                        IntCC::UnsignedLessThanOrEqual,
                        offset,
                        i64::from(end - step),
                    );
                    self.builder.ins().brif(fits, saved, &[], stack, &[]);

                    self.builder.switch_to_block(saved);
                    let area = self.builder.ins().load(self.cg.target.pointer(), MEM, list, 16);
                    let wide = self.builder.ins().uextend(self.cg.target.pointer(), offset);
                    let from = self.builder.ins().iadd(area, wide);
                    let next = self.builder.ins().iadd_imm(offset, i64::from(step));
                    self.builder.ins().store(MEM, next, list, at);
                    self.builder.ins().jump(done, &[from.into()]);
                    8
                };

                self.builder.switch_to_block(stack);
                let from = self.builder.ins().load(self.cg.target.pointer(), MEM, list, stack_at);
                let from = match wide {
                    true => {
                        let up = self.builder.ins().iadd_imm(from, 15);
                        self.builder.ins().band_imm(up, -16)
                    }
                    false => from,
                };
                let next = self.builder.ins().iadd_imm(from, if wide { 16 } else { 8 });
                self.builder.ins().store(MEM, next, list, stack_at);
                self.builder.ins().jump(done, &[from.into()]);

                self.builder.switch_to_block(done);
                let from = self.builder.block_params(done)[0];
                let value = self.builder.ins().load(scalar.ty, MEM, from, 0);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            // Out of range is undefined, so `as`'s saturating cast is one of its meanings.
            (I::FloatToIntUnchecked, [arg]) => {
                let (value, from) = self.load_scalar(arg)?;
                let to = self.scalar_of(dst_ty)?;
                let value = self.float_cast(value, from, to)?;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::Transmute | I::TransmuteUnchecked | I::BlackBox, [arg]) => {
                self.cg.opaque |= intrinsic == I::BlackBox;
                let (src, ty) = self.operand(arg)?;
                match (self.scalable(ty)?, self.scalable(dst_ty)?) {
                    (Some(from), Some(to)) if from.same_image(to) => (),
                    (None, None) => (),
                    (Some(_), Some(_)) => {
                        unsupported!("transmute between scalable vectors of other register images")
                    }
                    _ => unsupported!("transmute between a scalable vector and another type"),
                }
                if let TyKind::Ref(_, pointee, _) = dst_ty.kind()
                    && self.scalable(pointee)?.is_some()
                    && !matches!(ty.kind(), TyKind::Ref(..))
                {
                    unsupported!("a reference to a scalable vector made of another type");
                }
                let (from, to) = (self.layout(ty)?, self.layout(dst_ty)?);
                if from.size != to.size {
                    unsupported!("transmute between differently sized types");
                }
                let vector = matches!(from.backend_repr, BackendRepr::SimdVector { .. });
                self.copy_value(dst, src, if vector { &from } else { &to });
            }
            // Cranelift's atomics are always sequentially consistent, the strongest ordering.
            (I::Atomic(AtomicOp::Load, _), [ptr]) => {
                let scalar = self.scalar_of(dst_ty)?;
                let (ptr, _) = self.load_scalar(ptr)?;
                let value = match self.wide_atomics(scalar.ty)? {
                    true => {
                        let zero = self.builder.ins().iconst(types::I64, 0);
                        let zero = self.builder.ins().uextend(types::I128, zero);
                        self.cas16(ptr, zero, zero)?
                    }
                    false => self.builder.ins().atomic_load(scalar.ty, MEM, ptr),
                };
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::Atomic(AtomicOp::Store, _), [ptr, value]) => {
                let (ptr, _) = self.load_scalar(ptr)?;
                let (value, ty) = self.load_scalar(value)?;
                match self.wide_atomics(ty.ty)? {
                    true => _ = self.rmw16(AtomicRmwOp::Xchg, ptr, value)?,
                    false => _ = self.builder.ins().atomic_store(MEM, value, ptr),
                }
            }
            (
                I::Atomic(
                    op @ (AtomicOp::Xchg
                    | AtomicOp::Xadd
                    | AtomicOp::Xsub
                    | AtomicOp::And
                    | AtomicOp::Nand
                    | AtomicOp::Or
                    | AtomicOp::Xor
                    | AtomicOp::Max
                    | AtomicOp::Min
                    | AtomicOp::Umax
                    | AtomicOp::Umin),
                    _,
                ),
                [ptr, value],
            ) => {
                let op = match op {
                    AtomicOp::Xchg => AtomicRmwOp::Xchg,
                    AtomicOp::Xadd => AtomicRmwOp::Add,
                    AtomicOp::Xsub => AtomicRmwOp::Sub,
                    AtomicOp::And => AtomicRmwOp::And,
                    AtomicOp::Nand => AtomicRmwOp::Nand,
                    AtomicOp::Or => AtomicRmwOp::Or,
                    AtomicOp::Xor => AtomicRmwOp::Xor,
                    AtomicOp::Max => AtomicRmwOp::Smax,
                    AtomicOp::Min => AtomicRmwOp::Smin,
                    AtomicOp::Umax => AtomicRmwOp::Umax,
                    _ => AtomicRmwOp::Umin,
                };
                let scalar = self.scalar_of(dst_ty)?;
                let (ptr, _) = self.load_scalar(ptr)?;
                let (value, _) = self.load_scalar(value)?;
                let old = match self.wide_atomics(scalar.ty)? {
                    true => self.rmw16(op, ptr, value)?,
                    false => self.builder.ins().atomic_rmw(scalar.ty, MEM, op, ptr, value),
                };
                self.builder.ins().store(MEM, old, dst, 0);
            }
            (I::Atomic(AtomicOp::Cxchg | AtomicOp::CxchgWeak, _), [ptr, expected, new]) => {
                let (ptr, _) = self.load_scalar(ptr)?;
                let (expected, ty) = self.load_scalar(expected)?;
                let (new, _) = self.load_scalar(new)?;
                let old = match self.wide_atomics(ty.ty)? {
                    true => self.cas16(ptr, expected, new)?,
                    false => self.builder.ins().atomic_cas(MEM, ptr, expected, new),
                };
                let swapped = self.builder.ins().icmp(IntCC::Equal, old, expected);
                let layout = self.layout(dst_ty)?;
                self.builder.ins().store(MEM, old, dst, layout.fields.offset(0).bytes() as i32);
                self.builder.ins().store(MEM, swapped, dst, layout.fields.offset(1).bytes() as i32);
            }
            (I::Atomic(AtomicOp::Fence | AtomicOp::SingleThreadFence, _), []) => {
                self.builder.ins().fence();
            }
            (I::ReadViaCopy | I::VolatileLoad | I::UnalignedVolatileLoad, [ptr]) => {
                let (ptr, _) = self.load_scalar(ptr)?;
                let size = self.layout(dst_ty)?.size;
                self.copy(dst, ptr, size);
            }
            (I::WriteViaMove | I::VolatileStore | I::UnalignedVolatileStore, [ptr, value]) => {
                let (ptr, _) = self.load_scalar(ptr)?;
                let (src, ty) = self.operand(value)?;
                let size = self.layout(ty)?.size;
                self.copy(ptr, src, size);
            }
            (I::WriteBoxViaMove, [boxed, value]) => {
                let (boxed, boxed_ty) = self.operand(boxed)?;
                let (src, ty) = self.operand(value)?;
                let size = self.layout(ty)?.size;
                let ptr = self.builder.ins().load(self.cg.target.pointer(), MEM, boxed, 0);
                self.copy(ptr, src, size);
                let size = self.layout(boxed_ty)?.size;
                self.copy(dst, boxed, size);
            }
            (I::Offset | I::ArithOffset, [ptr, count]) => {
                let Some(pointee) = self.operand_ty(ptr)?.builtin_deref(true) else {
                    unsupported!("offset of a non-pointer");
                };
                let size = self.layout(pointee)?.size.bytes() as i64;
                let (ptr, _) = self.load_scalar(ptr)?;
                let (count, _) = self.load_scalar(count)?;
                let bytes = self.builder.ins().imul_imm(count, size);
                let value = self.builder.ins().iadd(ptr, bytes);
                if intrinsic == I::Offset {
                    let inst = self.builder.func.dfg.value_def(value).unwrap_inst();
                    let loc = unwind::seh::in_bounds(self.builder.func.srcloc(inst));
                    self.builder.func.set_srcloc(inst, loc);
                }
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::PtrMask, [ptr, mask]) => {
                let (ptr, _) = self.load_scalar(ptr)?;
                let (mask, _) = self.load_scalar(mask)?;
                let value = self.builder.ins().band(ptr, mask);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::PtrOffsetFrom | I::PtrOffsetFromUnsigned, [a, b]) => {
                let size = self.layout(generic_args.type_at(0))?.size.bytes() as i64;
                let (a, _) = self.load_scalar(a)?;
                let (b, _) = self.load_scalar(b)?;
                let bytes = self.builder.ins().isub(a, b);
                let size = self.builder.ins().iconst(self.cg.target.pointer(), size);
                let value = self.builder.ins().sdiv(bytes, size);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::Copy | I::CopyNonoverlapping | I::WriteBytes, [a, b, count]) => {
                let size = self.layout(generic_args.type_at(0))?.size.bytes() as i64;
                let (a, _) = self.load_scalar(a)?;
                let (b, _) = self.load_scalar(b)?;
                let (count, _) = self.load_scalar(count)?;
                let bytes = self.builder.ins().imul_imm(count, size);
                let config = self.cg.target_config();
                match intrinsic {
                    // `copy(src, dst, count)`
                    I::Copy => self.builder.call_memmove(config, b, a, bytes),
                    I::CopyNonoverlapping => self.builder.call_memcpy(config, b, a, bytes),
                    // `write_bytes(dst, value, count)`
                    _ => self.builder.call_memset(config, a, b, bytes),
                }
            }
            (
                I::Assume
                | I::ColdPath
                | I::Forget
                | I::AssertInhabited
                | I::AssertZeroValid
                | I::AssertMemUninitializedValid,
                _,
            ) => (),
            (I::Unreachable | I::AbortImmediate | I::Breakpoint, _) => {
                self.builder.ins().trap(TrapCode::user(2).expect("non-zero trap code"));
                return Ok(Lowered::Diverges);
            }
            (I::RawEq | I::CompareBytes, [a, b, ..]) => {
                let (a, _) = self.load_scalar(a)?;
                let (b, _) = self.load_scalar(b)?;
                if intrinsic == I::RawEq {
                    let size = self.layout(generic_args.type_at(0))?.size.bytes();
                    if let Some(equal) =
                        copies::equal_bytes(&mut self.builder, self.cg.target.pointer(), a, b, size)
                    {
                        self.builder.ins().store(MEM, equal, dst, 0);
                        return Ok(Lowered::Inline);
                    }
                }
                let len = match args {
                    [_, _, len] => self.load_scalar(len)?.0,
                    _ => {
                        let size = self.layout(generic_args.type_at(0))?.size.bytes();
                        self.builder.ins().iconst(self.cg.target.pointer(), size as i64)
                    }
                };
                let mut value = self.memcmp(a, b, len)?;
                if intrinsic == I::RawEq {
                    value = self.builder.ins().icmp_imm(IntCC::Equal, value, 0);
                }
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::Simd(S::Fma | S::RelaxedFma), _) if self.x86_fused() => {
                self.x86_fma(intrinsic, args, dst, dst_ty)?
            }
            (
                I::Simd(
                    op @ (S::Add
                    | S::Sub
                    | S::Mul
                    | S::Div
                    | S::Eq
                    | S::Ge
                    | S::Gt
                    | S::Le
                    | S::Lt
                    | S::Neg
                    | S::Fabs
                    | S::Fsqrt
                    | S::Floor
                    | S::Ceil
                    | S::Fma
                    | S::Cast),
                ),
                _,
            ) if self.half_lanes(args, dst_ty)? => self.half_vector(op, args, dst, dst_ty)?,
            (
                I::Simd(
                    op @ (S::And
                    | S::Or
                    | S::Xor
                    | S::Add
                    | S::Sub
                    | S::Mul
                    | S::Shl
                    | S::Shr
                    | S::Eq
                    | S::Ne
                    | S::Lt
                    | S::Le
                    | S::Gt
                    | S::Ge
                    | S::Div
                    | S::SaturatingAdd
                    | S::SaturatingSub),
                ),
                [lhs, rhs],
            ) => {
                let (l, ty) = self.operand(lhs)?;
                let (r, _) = self.operand(rhs)?;
                let (lane, count) = self.simd_lanes(ty)?;
                let (out, _) = self.simd_lanes(dst_ty)?;
                let signed = lane.signed;
                if lane.ty.is_float() {
                    self.simd_float_lanes(op, (l, r), dst, lane, out, count)?;
                    return Ok(Lowered::Inline);
                }
                if op == S::Div {
                    unsupported!("`simd_div` of integers");
                }
                // What x86's baseline, SSE2, has an instruction for.
                let vectors = self.makes_vectors();
                let chunks = simd_chunks(lane.ty, count).filter(|_| {
                    vectors
                        && out.ty == lane.ty
                        && match (op, signed) {
                            (S::And | S::Or | S::Xor | S::Add | S::Sub, _) => true,
                            (S::Eq | S::Ne, _) | (S::Lt | S::Gt, true) => lane.ty.bits() <= 32,
                            (S::SaturatingAdd | S::SaturatingSub, _) => lane.ty.bits() <= 16,
                            _ => false,
                        }
                });
                if let Some((vector, chunks)) = chunks {
                    for chunk in 0..chunks {
                        let offset = chunk * 16;
                        let a = self.builder.ins().load(vector, MEM, l, offset);
                        let b = self.builder.ins().load(vector, MEM, r, offset);
                        let ins = self.builder.ins();
                        let value = match (op, signed) {
                            (S::Eq, _) => ins.icmp(IntCC::Equal, a, b),
                            (S::Ne, _) => ins.icmp(IntCC::NotEqual, a, b),
                            (S::Lt, _) => ins.icmp(IntCC::SignedLessThan, a, b),
                            (S::Gt, _) => ins.icmp(IntCC::SignedGreaterThan, a, b),
                            (S::And, _) => ins.band(a, b),
                            (S::Or, _) => ins.bor(a, b),
                            (S::Xor, _) => ins.bxor(a, b),
                            (S::Add, _) => ins.iadd(a, b),
                            (S::Sub, _) => ins.isub(a, b),
                            (S::SaturatingAdd, true) => ins.sadd_sat(a, b),
                            (S::SaturatingAdd, false) => ins.uadd_sat(a, b),
                            (S::SaturatingSub, true) => ins.ssub_sat(a, b),
                            _ => ins.usub_sat(a, b),
                        };
                        self.builder.ins().store(MEM, value, dst, offset);
                    }
                    return Ok(Lowered::Inline);
                }
                for i in 0..count {
                    let offset = (i * lane.ty.bytes() as u64) as i32;
                    let a = self.builder.ins().load(lane.ty, MEM, l, offset);
                    let b = self.builder.ins().load(lane.ty, MEM, r, offset);
                    let cc = match (op, signed) {
                        (S::Eq, _) => Some(IntCC::Equal),
                        (S::Ne, _) => Some(IntCC::NotEqual),
                        (S::Lt, true) => Some(IntCC::SignedLessThan),
                        (S::Lt, false) => Some(IntCC::UnsignedLessThan),
                        (S::Le, true) => Some(IntCC::SignedLessThanOrEqual),
                        (S::Le, false) => Some(IntCC::UnsignedLessThanOrEqual),
                        (S::Gt, true) => Some(IntCC::SignedGreaterThan),
                        (S::Gt, false) => Some(IntCC::UnsignedGreaterThan),
                        (S::Ge, true) => Some(IntCC::SignedGreaterThanOrEqual),
                        (S::Ge, false) => Some(IntCC::UnsignedGreaterThanOrEqual),
                        _ => None,
                    };
                    let value = match (cc, op) {
                        // A true lane is all ones.
                        (Some(cc), _) => {
                            let flag = self.builder.ins().icmp(cc, a, b);
                            let flag = self.builder.ins().uextend(types::I64, flag);
                            let mask = self.builder.ins().ineg(flag);
                            if out.ty.bits() < 64 {
                                self.builder.ins().ireduce(out.ty, mask)
                            } else {
                                mask
                            }
                        }
                        (None, S::And) => self.builder.ins().band(a, b),
                        (None, S::Or) => self.builder.ins().bor(a, b),
                        (None, S::Xor) => self.builder.ins().bxor(a, b),
                        (None, S::Add) => self.builder.ins().iadd(a, b),
                        (None, S::Sub) => self.builder.ins().isub(a, b),
                        (None, S::Shl) => self.builder.ins().ishl(a, b),
                        (None, S::Shr) if signed => self.builder.ins().sshr(a, b),
                        (None, S::Shr) => self.builder.ins().ushr(a, b),
                        (None, S::SaturatingAdd) => self.saturating(true, a, b, lane),
                        (None, S::SaturatingSub) => self.saturating(false, a, b, lane),
                        (None, _) => self.builder.ins().imul(a, b),
                    };
                    self.builder.ins().store(MEM, value, dst, (i * out.ty.bytes() as u64) as i32);
                }
            }
            (I::Simd(S::Select), [mask, on_true, on_false]) => {
                let (mask, mask_ty) = self.operand(mask)?;
                let (on_true, _) = self.operand(on_true)?;
                let (on_false, _) = self.operand(on_false)?;
                let (mask_lane, count) = self.simd_lanes(mask_ty)?;
                let (lane, _) = self.simd_moved_lanes(dst_ty)?;
                for i in 0..count {
                    let offset = (i * lane.ty.bytes() as u64) as i32;
                    let mask_offset = (i * mask_lane.ty.bytes() as u64) as i32;
                    // The top bit of a mask lane decides.
                    let chosen = self.builder.ins().load(mask_lane.ty, MEM, mask, mask_offset);
                    let chosen = self.builder.ins().icmp_imm(IntCC::SignedLessThan, chosen, 0);
                    let a = self.builder.ins().load(lane.ty, MEM, on_true, offset);
                    let b = self.builder.ins().load(lane.ty, MEM, on_false, offset);
                    let value = self.builder.ins().select(chosen, a, b);
                    self.builder.ins().store(MEM, value, dst, offset);
                }
            }
            (I::Simd(S::SelectBitmask), [mask, on_true, on_false]) => {
                let (mask, _) = self.operand(mask)?;
                let (on_true, _) = self.operand(on_true)?;
                let (on_false, _) = self.operand(on_false)?;
                let (lane, count) = self.simd_moved_lanes(dst_ty)?;
                // Lane i by bit i of the mask, an integer or bytes: its bytes in memory in order.
                let lanes: LaneValues = (0..count)
                    .map(|i| {
                        let offset = (i * lane.ty.bytes() as u64) as i32;
                        let byte = self.builder.ins().load(types::I8, MEM, mask, (i / 8) as i32);
                        let bit = self.builder.ins().ushr_imm(byte, (i % 8) as i64);
                        let bit = self.builder.ins().band_imm(bit, 1);
                        let a = self.builder.ins().load(lane.ty, MEM, on_true, offset);
                        let b = self.builder.ins().load(lane.ty, MEM, on_false, offset);
                        self.builder.ins().select(bit, a, b)
                    })
                    .collect(); // alloc: none, inline up to 64 lanes
                for (i, value) in lanes.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, i as i32 * lane.ty.bytes() as i32);
                }
            }
            (I::Simd(S::Fma | S::RelaxedFma), [a, b, c]) => {
                let (a, _) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                let (c, _) = self.operand(c)?;
                let (lane, count) = self.simd_lanes(dst_ty)?;
                if !lane.ty.is_float() {
                    unsupported!("`{intrinsic}` of integers");
                }
                // AArch64's LLVM makes a vector of more lanes than one `fmla z, y, x`, the
                // factors the other way round, which decides which of two NaNs comes out; a
                // vector of one lane is the scalar's `fmadd`, in order.
                let swapped = self.cg.target.arch == Arch::Aarch64 && count > 1;
                // Whole vectors of 16 bytes, or one of 8, so that LLVM's SLP vectorizer, which
                // joins lanes of scalar fmas into a vector fma with the factors in their order
                // and so picks the other NaN, has nothing to join.
                let bytes = count * u64::from(lane.ty.bytes());
                let chunk = match bytes {
                    8 => 8,
                    _ if bytes % 16 == 0 => 16,
                    _ => 0,
                };
                if swapped && matches!(lane.ty, types::F32 | types::F64) && chunk != 0 {
                    let vector = lane.ty.by((chunk / u64::from(lane.ty.bytes())) as u32);
                    let vector = vector.expect("8 or 16 bytes of lanes");
                    for at in (0..bytes).step_by(chunk as usize) {
                        let at = at as i32;
                        let [x, y, z] =
                            [a, b, c].map(|v| self.builder.ins().load(vector, MEM, v, at));
                        let value = self.builder.ins().fma(y, x, z);
                        self.builder.ins().store(MEM, value, dst, at);
                    }
                } else {
                    // x86-64's LLVM makes `simd_relaxed_fma` (`llvm.fmuladd`) of a CPU without
                    // FMA a multiply and then an add (`mulps`, `addps`), rounded twice.
                    let unfused =
                        intrinsic == I::Simd(S::RelaxedFma) && self.cg.target.arch == Arch::X86_64;
                    let lanes: LaneValues = (0..count)
                        .map(|i| {
                            let offset = (i * lane.ty.bytes() as u64) as i32;
                            let [x, y, z] =
                                [a, b, c].map(|v| self.builder.ins().load(lane.ty, MEM, v, offset));
                            if unfused {
                                let product = self.builder.ins().fmul(x, y);
                                self.builder.ins().fadd(product, z)
                            } else if swapped {
                                self.builder.ins().fma(y, x, z)
                            } else {
                                self.builder.ins().fma(x, y, z)
                            }
                        })
                        .collect(); // alloc: none, inline up to 64 lanes
                    for (i, value) in lanes.into_iter().enumerate() {
                        let offset = i as i32 * lane.ty.bytes() as i32;
                        self.builder.ins().store(MEM, value, dst, offset);
                    }
                }
            }
            (I::Simd(S::Extract), [vector, index]) => {
                let (vector, ty) = self.operand(vector)?;
                let (index, _) = self.load_scalar(index)?;
                let (lane, _) = self.simd_moved_lanes(ty)?;
                let index = self.builder.ins().uextend(self.cg.target.pointer(), index);
                let offset = self.builder.ins().imul_imm(index, lane.ty.bytes() as i64);
                let addr = self.builder.ins().iadd(vector, offset);
                let value = self.builder.ins().load(lane.ty, MEM, addr, 0);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::Simd(S::Insert), [vector, index, value]) => {
                let (vector, ty) = self.operand(vector)?;
                let (index, _) = self.load_scalar(index)?;
                let (value, _) = self.load_scalar(value)?;
                let (lane, _) = self.simd_moved_lanes(ty)?;
                let size = self.layout(ty)?.size;
                self.copy(dst, vector, size);
                let index = self.builder.ins().uextend(self.cg.target.pointer(), index);
                let offset = self.builder.ins().imul_imm(index, lane.ty.bytes() as i64);
                let addr = self.builder.ins().iadd(dst, offset);
                self.builder.ins().store(MEM, value, addr, 0);
            }
            (I::Simd(S::Shuffle), [first, second, indices]) => {
                let (first, ty) = self.operand(first)?;
                let (second, _) = self.operand(second)?;
                // `[u32; N]`, or a wrapper of one. Lanes of `second` come after `first`'s.
                let (indices, _) = self.operand(indices)?;
                let layout = self.layout(ty)?;
                let (lane, _) = self.simd_moved_lanes(ty)?;
                let (_, count) = self.simd_moved_lanes(dst_ty)?;
                let lane_size = lane.ty.bytes() as u64;
                let both = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    (layout.size.bytes() * 2) as u32,
                    layout.align.abi.bytes().min(16).trailing_zeros() as u8,
                ));
                let both = self.builder.ins().stack_addr(self.cg.target.pointer(), both, 0);
                self.copy(both, first, layout.size);
                let upper = self.builder.ins().iadd_imm(both, layout.size.bytes() as i64);
                self.copy(upper, second, layout.size);
                let mut lanes = LaneValues::new();
                for i in 0..count {
                    let index = self.builder.ins().load(types::I32, MEM, indices, (i * 4) as i32);
                    let index = self.builder.ins().uextend(self.cg.target.pointer(), index);
                    let offset = self.builder.ins().imul_imm(index, lane_size as i64);
                    let addr = self.builder.ins().iadd(both, offset);
                    lanes.push(self.builder.ins().load(lane.ty, MEM, addr, 0));
                }
                if !self.store_whole(&lanes, lane.ty, dst) {
                    for (i, value) in lanes.into_iter().enumerate() {
                        self.builder.ins().store(MEM, value, dst, (i as u64 * lane_size) as i32);
                    }
                }
            }
            (I::Simd(S::Splat), [arg]) => {
                let (src, _) = self.operand(arg)?;
                let (lane, count) = self.simd_moved_lanes(dst_ty)?;
                let value = self.builder.ins().load(lane.ty, MEM, src, 0);
                for i in 0..count {
                    let offset = (i * lane.ty.bytes() as u64) as i32;
                    self.builder.ins().store(MEM, value, dst, offset);
                }
            }
            // `simd_cast` leaves a float out of the range of the integer undefined; saturating
            // as `simd_as` does is one of the answers it may give.
            (I::Simd(S::Cast | S::As), [arg]) => {
                let (src, ty) = self.operand(arg)?;
                let (from, count) = self.simd_lanes(ty)?;
                let (to, _) = self.simd_lanes(dst_ty)?;
                for i in 0..count {
                    let offset = (i * from.ty.bytes() as u64) as i32;
                    let value = self.builder.ins().load(from.ty, MEM, src, offset);
                    let value = if from.ty.is_float() || to.ty.is_float() {
                        self.float_cast(value, from, to)?
                    } else {
                        self.int_cast(value, from, to)
                    };
                    self.builder.ins().store(MEM, value, dst, (i * to.ty.bytes() as u64) as i32);
                }
            }
            (I::Simd(S::Bitmask), [arg]) => {
                let (src, ty) = self.operand(arg)?;
                let (lane, count) = self.simd_lanes(ty)?;
                let BackendRepr::Scalar(_) = self.layout(dst_ty)?.backend_repr else {
                    unsupported!("simd_bitmask into {dst_ty:?}");
                };
                let to = self.scalar_of(dst_ty)?;
                if count > 64 || lane.ty.bits() > 64 {
                    unsupported!("simd_bitmask of {count} lanes");
                }
                // Bit `i` of the result is the top bit of lane `i`.
                let mut bits = self.builder.ins().iconst(types::I64, 0);
                let vectors = self.makes_vectors();
                let chunks =
                    simd_chunks(lane.ty, count).filter(|_| vectors && lane.ty.bits() != 16);
                for chunk in 0..chunks.map_or(0, |it| it.1) {
                    let vector = chunks.expect("chunks").0;
                    let value = self.builder.ins().load(vector, MEM, src, chunk * 16);
                    let top = self.builder.ins().vhigh_bits(types::I64, value);
                    let top = self
                        .builder
                        .ins()
                        .ishl_imm(top, i64::from(chunk) * i64::from(vector.lane_count()));
                    bits = self.builder.ins().bor(bits, top);
                }
                for i in (0..count).filter(|_| chunks.is_none()) {
                    let offset = (i * lane.ty.bytes() as u64) as i32;
                    let value = self.builder.ins().load(lane.ty, MEM, src, offset);
                    let top = self.builder.ins().ushr_imm(value, lane.ty.bits() as i64 - 1);
                    let top = if lane.ty.bits() < 64 {
                        self.builder.ins().uextend(types::I64, top)
                    } else {
                        top
                    };
                    let top = self.builder.ins().ishl_imm(top, i as i64);
                    bits = self.builder.ins().bor(bits, top);
                }
                let from = AbiScalar { ty: types::I64, signed: false };
                let value = self.int_cast(bits, from, to);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::Simd(op @ (S::ReduceAnd | S::ReduceOr | S::ReduceXor)), [arg]) => {
                let (src, ty) = self.operand(arg)?;
                let (lane, count) = self.simd_lanes(ty)?;
                if lane.ty.is_float() {
                    unsupported!("`{intrinsic}` of floats");
                }
                let mut acc = self.builder.ins().load(lane.ty, MEM, src, 0);
                for i in 1..count {
                    let offset = (i * lane.ty.bytes() as u64) as i32;
                    let value = self.builder.ins().load(lane.ty, MEM, src, offset);
                    acc = match op {
                        S::ReduceAnd => self.builder.ins().band(acc, value),
                        S::ReduceOr => self.builder.ins().bor(acc, value),
                        _ => self.builder.ins().bxor(acc, value),
                    };
                }
                self.builder.ins().store(MEM, acc, dst, 0);
            }
            // Of floats these ignore a NaN lane, as `maxnum` does, which `fmax` does not.
            (I::Simd(op @ (S::ReduceMax | S::ReduceMin)), [arg]) => {
                let (src, ty) = self.operand(arg)?;
                let (lane, count) = self.simd_lanes(ty)?;
                if lane.ty.is_float() {
                    unsupported!("`{intrinsic}` of floats");
                }
                let mut acc = self.builder.ins().load(lane.ty, MEM, src, 0);
                for i in 1..count {
                    let offset = (i * lane.ty.bytes() as u64) as i32;
                    let value = self.builder.ins().load(lane.ty, MEM, src, offset);
                    acc = match (op == S::ReduceMax, lane.signed) {
                        (true, true) => self.builder.ins().smax(acc, value),
                        (true, false) => self.builder.ins().umax(acc, value),
                        (false, true) => self.builder.ins().smin(acc, value),
                        (false, false) => self.builder.ins().umin(acc, value),
                    };
                }
                self.builder.ins().store(MEM, acc, dst, 0);
            }
            // Each float lane to an integral float, as rustc's LLVM `trunc`, `round` (ties
            // away from zero) and `rint` (in the current mode, which is to even) select on
            // AArch64, of lanes of any float's width.
            (I::Simd(op @ (S::Trunc | S::Round | S::RoundTiesEven)), [arg])
                if self.cg.target.arch == Arch::Aarch64 =>
            {
                let ty = self.operand_ty(arg)?;
                let layout = self.layout(ty)?;
                let lanes = match layout.backend_repr {
                    BackendRepr::SimdVector { element, count } => match element.primitive() {
                        Primitive::Float(float) => {
                            neon::Lanes::floats(float.size().bits() as u32, count.as_u64() as u32)
                        }
                        _ => unsupported!("`{intrinsic}` of integers"),
                    },
                    _ => unsupported!("vector operation on {ty:?}"),
                };
                let instruction = match op {
                    S::Trunc => "frintz",
                    S::Round => "frinta",
                    _ => "frintx",
                };
                let Some(lowered) = neon::rounding(instruction, lanes) else {
                    unsupported!("`{intrinsic}` of {ty:?}");
                };
                self.neon_assembly(intrinsic.symbol().as_str(), lowered, args, dst, dst_ty)?;
            }
            (
                I::Simd(
                    op @ (S::Ctpop
                    | S::Ctlz
                    | S::Cttz
                    | S::Bswap
                    | S::Bitreverse
                    | S::Neg
                    | S::Fabs
                    | S::Fsqrt
                    | S::Floor
                    | S::Ceil),
                ),
                [arg],
            ) => {
                let (src, ty) = self.operand(arg)?;
                let (lane, count) = self.simd_lanes(ty)?;
                let float = lane.ty.is_float();
                match op {
                    S::Ctpop | S::Ctlz | S::Cttz | S::Bswap | S::Bitreverse if float => {
                        unsupported!("`{intrinsic}` of floats")
                    }
                    S::Fabs | S::Fsqrt | S::Floor | S::Ceil if !float => {
                        unsupported!("`{intrinsic}` of integers")
                    }
                    // Cranelift rounds and takes square roots of 32- and 64-bit floats only.
                    S::Fsqrt | S::Floor | S::Ceil
                        if lane.ty != types::F32 && lane.ty != types::F64 =>
                    {
                        unsupported!("`{intrinsic}` of {:?} lanes", lane.ty)
                    }
                    _ => (),
                }
                let values: LaneValues = (0..count)
                    .map(|i| {
                        let offset = (i * lane.ty.bytes() as u64) as i32;
                        let value = self.builder.ins().load(lane.ty, MEM, src, offset);
                        let ins = self.builder.ins();
                        match op {
                            S::Ctpop => ins.popcnt(value),
                            // A lane of zero has as many leading zeros as it has bits, as
                            // LLVM's `ctlz` with `is_zero_poison` false, which rustc emits.
                            S::Ctlz => ins.clz(value),
                            S::Cttz => ins.ctz(value),
                            // A byte is its own byte swap; Cranelift swaps 16 bits and more.
                            S::Bswap if lane.ty.bits() == 8 => value,
                            S::Bswap => ins.bswap(value),
                            S::Bitreverse => ins.bitrev(value),
                            // Integer lanes wrap, as rustc's `sub 0, x`.
                            S::Neg if float => ins.fneg(value),
                            S::Neg => ins.ineg(value),
                            S::Fabs => ins.fabs(value),
                            S::Fsqrt => ins.sqrt(value),
                            S::Floor => ins.floor(value),
                            _ => ins.ceil(value),
                        }
                    })
                    .collect(); // alloc: none, inline up to 64 lanes
                for (i, value) in values.into_iter().enumerate() {
                    self.builder.ins().store(
                        MEM,
                        value,
                        dst,
                        (i as u64 * lane.ty.bytes() as u64) as i32,
                    );
                }
            }
            (I::Simd(op @ (S::FunnelShl | S::FunnelShr)), [high, low, shift]) => {
                let (high, ty) = self.operand(high)?;
                let (low, _) = self.operand(low)?;
                let (shift, _) = self.operand(shift)?;
                let (lane, count) = self.simd_lanes(ty)?;
                if lane.ty.is_float() {
                    unsupported!("`{intrinsic}` of floats");
                }
                let bits = lane.ty.bits() as i64;
                let left = op == S::FunnelShl;
                // Each lane is `high:low` shifted by the shift's lane modulo the lane's width,
                // as LLVM's `fshl`/`fshr` that rustc emits (a shift at or past the width is
                // UB in Rust). Cranelift shifts by the count modulo the width too, so the
                // half shifted the other way goes one bit first and then by `width - 1 - s`,
                // which is `s ^ (width - 1)`: a shift of zero leaves the other half out.
                let values: LaneValues = (0..count)
                    .map(|i| {
                        let offset = (i * lane.ty.bytes() as u64) as i32;
                        let h = self.builder.ins().load(lane.ty, MEM, high, offset);
                        let l = self.builder.ins().load(lane.ty, MEM, low, offset);
                        let s = self.builder.ins().load(lane.ty, MEM, shift, offset);
                        let b = &mut self.builder;
                        let s = b.ins().band_imm(s, bits - 1);
                        let rest = b.ins().bxor_imm(s, bits - 1);
                        let (h, l) = if left {
                            let l = b.ins().ushr_imm(l, 1);
                            (b.ins().ishl(h, s), b.ins().ushr(l, rest))
                        } else {
                            let h = b.ins().ishl_imm(h, 1);
                            (b.ins().ishl(h, rest), b.ins().ushr(l, s))
                        };
                        b.ins().bor(h, l)
                    })
                    .collect(); // alloc: none, inline up to 64 lanes
                for (i, value) in values.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, i as i32 * lane.ty.bytes() as i32);
                }
            }
            (I::Simd(op @ (S::ReduceAll | S::ReduceAny)), [arg]) => {
                let (src, ty) = self.operand(arg)?;
                let (lane, count) = self.simd_lanes(ty)?;
                let all = op == S::ReduceAll;
                let mut acc = self.builder.ins().iconst(types::I8, all as i64);
                for i in 0..count {
                    let offset = (i * lane.ty.bytes() as u64) as i32;
                    let value = self.builder.ins().load(lane.ty, MEM, src, offset);
                    let set = self.builder.ins().icmp_imm(IntCC::NotEqual, value, 0);
                    acc = if all {
                        self.builder.ins().band(acc, set)
                    } else {
                        self.builder.ins().bor(acc, set)
                    };
                }
                self.builder.ins().store(MEM, acc, dst, 0);
            }
            (
                I::Simd(
                    op @ (S::ReduceAddOrdered
                    | S::ReduceMulOrdered
                    | S::ReduceAddUnordered
                    | S::ReduceMulUnordered),
                ),
                [arg, start @ ..],
            ) if start.len()
                == matches!(op, S::ReduceAddOrdered | S::ReduceMulOrdered) as usize =>
            {
                let (src, ty) = self.operand(arg)?;
                let (lane, count) = self.simd_lanes(ty)?;
                let lane_at = |this: &mut Self, i: u64| {
                    this.builder.ins().load(lane.ty, MEM, src, (i * lane.ty.bytes() as u64) as i32)
                };
                let (mut acc, first) = match start {
                    [start] => (self.load_scalar(start)?.0, 0),
                    _ => (lane_at(self, 0), 1),
                };
                let add = matches!(op, S::ReduceAddOrdered | S::ReduceAddUnordered);
                for i in first..count {
                    let value = lane_at(self, i);
                    let ins = self.builder.ins();
                    acc = match (add, lane.ty.is_float()) {
                        (true, true) => ins.fadd(acc, value),
                        (true, false) => ins.iadd(acc, value),
                        (false, true) => ins.fmul(acc, value),
                        (false, false) => ins.imul(acc, value),
                    };
                }
                self.builder.ins().store(MEM, acc, dst, 0);
            }
            // Only the lanes a mask has are read or written: the others may be past the end of
            // what the pointer may reach.
            (I::Simd(op @ (S::MaskedLoad | S::MaskedStore)), [mask, pointer, values]) => {
                let (mask, mask_ty) = self.operand(mask)?;
                let (pointer, _) = self.load_scalar(pointer)?;
                let (values, ty) = self.operand(values)?;
                let (mask_lane, count) = self.simd_lanes(mask_ty)?;
                let (lane, _) = self.simd_lanes(ty)?;
                let chosen: LaneValues = (0..count)
                    .map(|i| {
                        let offset = (i * mask_lane.ty.bytes() as u64) as i32;
                        let chosen = self.builder.ins().load(mask_lane.ty, MEM, mask, offset);
                        self.builder.ins().icmp_imm(IntCC::SignedLessThan, chosen, 0)
                    })
                    .collect(); // alloc: none, inline up to 64 lanes
                let load = op == S::MaskedLoad;
                if load {
                    let size = self.layout(ty)?.size;
                    self.copy(dst, values, size);
                }
                for (i, chosen) in chosen.into_iter().enumerate() {
                    let offset = i as i32 * lane.ty.bytes() as i32;
                    let (on, next) = (self.builder.create_block(), self.builder.create_block());
                    self.builder.ins().brif(chosen, on, &[], next, &[]);
                    self.builder.switch_to_block(on);
                    if load {
                        let value = self.builder.ins().load(lane.ty, MEM, pointer, offset);
                        self.builder.ins().store(MEM, value, dst, offset);
                    } else {
                        let value = self.builder.ins().load(lane.ty, MEM, values, offset);
                        self.builder.ins().store(MEM, value, pointer, offset);
                    }
                    self.builder.ins().jump(next, &[]);
                    self.builder.switch_to_block(next);
                }
            }
            (
                I::Fabs | I::Sqrt | I::Floor | I::Ceil | I::Trunc | I::RoundTiesEven | I::Round,
                [arg],
            ) => {
                let (value, scalar) = self.load_scalar(arg)?;
                if !scalar.ty.is_float() {
                    unsupported!("intrinsic `{intrinsic}` on {:?}", scalar.ty);
                }
                if f16_f128::soft(scalar.ty) {
                    let value = self.soft_unary(intrinsic, value)?;
                    self.builder.ins().store(MEM, value, dst, 0);
                    return Ok(Lowered::Inline);
                }
                let value = match intrinsic {
                    I::Fabs => self.builder.ins().fabs(value),
                    I::Sqrt => self.builder.ins().sqrt(value),
                    I::Floor => self.builder.ins().floor(value),
                    I::Ceil => self.builder.ins().ceil(value),
                    I::Trunc => self.builder.ins().trunc(value),
                    I::RoundTiesEven => self.builder.ins().nearest(value),
                    _ => {
                        let libm = if scalar.ty == types::F32 { "roundf" } else { "round" };
                        self.float_libcall(libm, scalar.ty, &[value])?
                    }
                };
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::Powi, [base, power]) => {
                let (base, scalar) = self.load_scalar(base)?;
                let (power, _) = self.load_scalar(power)?;
                if f16_f128::soft(scalar.ty) {
                    let value = self.soft_powi(base, power)?;
                    self.builder.ins().store(MEM, value, dst, 0);
                    return Ok(Lowered::Inline);
                }
                let builtin = if scalar.ty == types::F32 { "__powisf2" } else { "__powidf2" };
                let value =
                    self.builtin(builtin, &[scalar.ty, types::I32], scalar.ty, &[base, power])?;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (
                I::CopySign(_)
                | I::Powf
                | I::FaddFast
                | I::FsubFast
                | I::FmulFast
                | I::FdivFast
                | I::FremFast
                | I::FaddAlgebraic
                | I::FsubAlgebraic
                | I::FmulAlgebraic
                | I::FdivAlgebraic
                | I::FremAlgebraic,
                [a, b],
            ) => {
                let (l, scalar) = self.load_scalar(a)?;
                let (r, _) = self.load_scalar(b)?;
                if !scalar.ty.is_float() {
                    unsupported!("intrinsic `{intrinsic}` on {:?}", scalar.ty);
                }
                if f16_f128::soft(scalar.ty) {
                    let op = match intrinsic {
                        I::CopySign(_) => None,
                        I::FaddFast | I::FaddAlgebraic => Some(BinOp::Add),
                        I::FsubFast | I::FsubAlgebraic => Some(BinOp::Sub),
                        I::FmulFast | I::FmulAlgebraic => Some(BinOp::Mul),
                        I::FdivFast | I::FdivAlgebraic => Some(BinOp::Div),
                        I::FremFast | I::FremAlgebraic => Some(BinOp::Rem),
                        _ => unsupported!("intrinsic `{intrinsic}` on {:?}", scalar.ty),
                    };
                    let value = match op {
                        Some(op) => self.soft_binary_op(&op, l, r)?,
                        None => self.soft_copysign(l, r),
                    };
                    self.builder.ins().store(MEM, value, dst, 0);
                    return Ok(Lowered::Inline);
                }
                let single = scalar.ty == types::F32;
                let value = match intrinsic {
                    I::CopySign(_) => self.builder.ins().fcopysign(l, r),
                    I::Powf => {
                        self.float_libcall(if single { "powf" } else { "pow" }, scalar.ty, &[l, r])?
                    }
                    I::FaddFast | I::FaddAlgebraic => self.builder.ins().fadd(l, r),
                    I::FsubFast | I::FsubAlgebraic => self.builder.ins().fsub(l, r),
                    I::FmulFast | I::FmulAlgebraic => self.builder.ins().fmul(l, r),
                    // rustc emits `frem`, which LLVM lowers to C's `fmod`, as `%` is.
                    I::FremFast | I::FremAlgebraic => {
                        if !single && scalar.ty != types::F64 {
                            unsupported!("intrinsic `{intrinsic}` on {:?}", scalar.ty);
                        }
                        self.float_binary_op(&BinOp::Rem, l, r)?
                    }
                    _ => self.builder.ins().fdiv(l, r),
                };
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::Fma | I::Fmuladd, [_, _, _])
                if self.x86_fused() && self.scalar_of(dst_ty)?.ty != types::F128 =>
            {
                self.x86_fma(intrinsic, args, dst, dst_ty)?
            }
            (I::Fma | I::Fmuladd, [a, b, c]) => {
                let (a, scalar) = self.load_scalar(a)?;
                let (b, _) = self.load_scalar(b)?;
                let (c, _) = self.load_scalar(c)?;
                if f16_f128::soft(scalar.ty) {
                    let value = self.soft_fma(a, b, c)?;
                    self.builder.ins().store(MEM, value, dst, 0);
                    return Ok(Lowered::Inline);
                }
                let value = if intrinsic == I::Fmuladd {
                    let product = self.builder.ins().fmul(a, b);
                    self.builder.ins().fadd(product, c)
                } else {
                    let libm = if scalar.ty == types::F32 { "fmaf" } else { "fma" };
                    self.float_libcall(libm, scalar.ty, &[a, b, c])?
                };
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::DiscriminantValue, [arg]) => {
                // The reference may be a wide one, of a struct that ends in a slice.
                let (reference, _) = self.operand(arg)?;
                let ptr = self.builder.ins().load(self.cg.target.pointer(), MEM, reference, 0);
                let to = self.scalar_of(dst_ty)?;
                let value = self.discriminant(ptr, generic_args.type_at(0), to)?;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::PtrMetadata, [arg]) => {
                let (src, ty) = self.operand(arg)?;
                if let BackendRepr::ScalarPair { .. } = self.layout(ty)?.backend_repr {
                    let meta = self.builder.ins().load(
                        self.cg.target.pointer(),
                        MEM,
                        src,
                        self.cg.target.meta_at(),
                    );
                    self.builder.ins().store(MEM, meta, dst, 0);
                }
            }
            (I::AggregateRawPtr, [data, meta]) => {
                let (data, _) = self.operand(data)?;
                let data = self.builder.ins().load(self.cg.target.pointer(), MEM, data, 0);
                self.builder.ins().store(MEM, data, dst, 0);
                if let BackendRepr::ScalarPair { .. } = self.layout(dst_ty)?.backend_repr {
                    let (meta, _) = self.load_scalar(meta)?;
                    self.builder.ins().store(MEM, meta, dst, self.cg.target.meta_at());
                }
            }
            (I::SliceGetUnchecked, [slice, index]) => {
                let size = self.layout(generic_args.type_at(2))?.size.bytes() as i64;
                let (slice, _) = self.operand(slice)?;
                let ptr = self.builder.ins().load(self.cg.target.pointer(), MEM, slice, 0);
                let (index, _) = self.load_scalar(index)?;
                let offset = self.builder.ins().imul_imm(index, size);
                let value = self.builder.ins().iadd(ptr, offset);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::VtableSize | I::VtableAlign, [vtable]) => {
                let (vtable, _) = self.load_scalar(vtable)?;
                let offset = if intrinsic == I::VtableSize {
                    self.cg.target.meta_at()
                } else {
                    2 * self.cg.target.meta_at()
                };
                let value = self.builder.ins().load(self.cg.target.pointer(), MEM, vtable, offset);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::SizeOfVal | I::AlignOfVal, [arg]) => {
                let (src, _) = self.operand(arg)?;
                let ty = generic_args.type_at(0);
                let meta = self.is_unsized(ty).then(|| {
                    self.builder.ins().load(
                        self.cg.target.pointer(),
                        MEM,
                        src,
                        self.cg.target.meta_at(),
                    )
                });
                let (size, align) = self.size_and_align_of_val(ty, meta)?;
                let value = if intrinsic == I::SizeOfVal { size } else { align };
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (I::CatchUnwind, [try_fn, data, catch_fn]) => {
                let (try_fn, _) = self.load_scalar(try_fn)?;
                let (data, _) = self.load_scalar(data)?;
                let (catch_fn, _) = self.load_scalar(catch_fn)?;
                let mut sig = self.cg.make_signature();
                sig.params.push(AbiParam::new(self.cg.target.pointer()));
                let try_sig = self.builder.import_signature(sig);
                let mut sig = self.cg.make_signature();
                sig.params.extend([AbiParam::new(self.cg.target.pointer()); 2]);
                let catch_sig = self.builder.import_signature(sig);

                // Whether it unwound is an `i32` or a `bool`, by the toolchain.
                let outcome = self.layout(dst_ty)?.size.bytes();
                let outcome = Type::int_with_byte_size(outcome as u16).ok_or_else(|| {
                    // alloc: the error's message
                    CodegenError::Unsupported(format!("`catch_unwind` giving {outcome} bytes"))
                })?;
                if self.aborts {
                    // Nothing unwinds to be caught, as rustc has it.
                    self.builder.ins().call_indirect(try_sig, try_fn, &[data]);
                    let zero = self.builder.ins().iconst(outcome, 0);
                    self.builder.ins().store(MEM, zero, dst, 0);
                    return Ok(Lowered::Inline);
                }
                let returned = self.builder.create_block();
                let caught = self.builder.create_block();
                let done = self.builder.create_block();
                self.builder.append_block_param(caught, self.cg.target.pointer());
                self.builder.append_block_param(done, outcome);
                if self.cg.target.exception_in_register() {
                    let normal = self.builder.func.dfg.block_call(returned, &[]);
                    let unwound =
                        self.builder.func.dfg.block_call(caught, &[ir::BlockArg::TryCallExn(0)]);
                    let tag = ir::ExceptionTag::with_number(unwind::CATCH).expect("a small tag");
                    let table =
                        self.builder.func.dfg.exception_tables.push(ir::ExceptionTableData::new(
                            try_sig,
                            normal,
                            [ir::ExceptionTableItem::Tag(tag, unwound)],
                        ));
                    self.builder.ins().try_call_indirect(try_fn, &[data], table);
                } else {
                    self.guarded_call(caught, |builder| {
                        builder.ins().call_indirect(try_sig, try_fn, &[data])
                    });
                    self.builder.ins().jump(returned, &[]);
                }

                self.builder.switch_to_block(returned);
                let zero = self.builder.ins().iconst(outcome, 0);
                self.builder.ins().jump(done, &[zero.into()]);

                self.builder.switch_to_block(caught);
                self.builder.set_cold_block(caught);
                let exception = self.builder.block_params(caught)[0];
                if self.cg.target.exception_in_register() {
                    self.invoke_indirect(catch_sig, catch_fn, &[data, exception]);
                } else {
                    // The handler takes the thrown object, and what it was brought in is ours.
                    let object = self.builder.ins().iadd_imm(exception, unwind::seh::OBJECT);
                    self.invoke_indirect(catch_sig, catch_fn, &[data, object]);
                    let mut sig = self.cg.make_signature();
                    sig.params.push(AbiParam::new(self.cg.target.pointer()));
                    let release = self.cg.import(unwind::seh::RELEASE, &sig)?;
                    let release = self.cg.declare_func_in_func(release, self.builder.func);
                    self.builder.ins().call(release, &[exception]);
                }
                let one = self.builder.ins().iconst(outcome, 1);
                self.builder.ins().jump(done, &[one.into()]);

                self.builder.switch_to_block(done);
                let result = self.builder.block_params(done)[0];
                self.builder.ins().store(MEM, result, dst, 0);
            }
            (I::CallerLocation, []) => {
                let value = self.caller_location()?;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            // LLVM gives it as `llvm.returnaddress(0)`; Cranelift compiles a function that
            // reads it keeping its frame pointer (`emit_with`).
            (I::ReturnAddress, []) => {
                if self.cg.target.is_wasm() {
                    unsupported!("`return_address` on WebAssembly, which has no return address");
                }
                let value = self.builder.ins().get_return_address(self.cg.target.pointer());
                self.builder.ins().store(MEM, value, dst, 0);
            }
            _ => return Ok(Lowered::Unknown),
        }
        Ok(Lowered::Inline)
    }

    /// The signed `wide` lanes of two vectors, each saturated to half its width, signed or
    /// not: every 16 bytes of the result are those of `a` and then those of `b`.
    /// `cmpps` and its kin: each lane of `a` against the same of `b` by the predicate of the
    /// immediate, as `vcmpps` reads its low five bits (SSE's eight are the first eight), all
    /// ones where it holds. One that signals and one that does not answer alike. A scalar
    /// compare does the first lane alone and keeps the others of `a`.
    fn compare_lanes(
        &mut self,
        a: &Operand,
        b: &Operand,
        predicate: u8,
        dst: Value,
        lane: Type,
        scalar: bool,
    ) -> Result<()> {
        let (a, ty) = self.operand(a)?;
        let (b, _) = self.operand(b)?;
        let size = self.layout(ty)?.size;
        let int = Type::int(lane.bits() as u16).expect("a lane of 32 or 64 bits");
        let cc = match predicate & 0xf {
            0 => Ok(FloatCC::Equal),
            1 => Ok(FloatCC::LessThan),
            2 => Ok(FloatCC::LessThanOrEqual),
            3 => Ok(FloatCC::Unordered),
            4 => Ok(FloatCC::NotEqual),
            5 => Ok(FloatCC::UnorderedOrGreaterThanOrEqual),
            6 => Ok(FloatCC::UnorderedOrGreaterThan),
            7 => Ok(FloatCC::Ordered),
            8 => Ok(FloatCC::UnorderedOrEqual),
            9 => Ok(FloatCC::UnorderedOrLessThan),
            10 => Ok(FloatCC::UnorderedOrLessThanOrEqual),
            11 => Err(0),
            12 => Ok(FloatCC::OrderedNotEqual),
            13 => Ok(FloatCC::GreaterThanOrEqual),
            14 => Ok(FloatCC::GreaterThan),
            _ => Err(-1),
        };
        let lanes = if scalar { 1 } else { size.bytes() / u64::from(lane.bytes()) };
        let values: LaneValues = (0..lanes as i32)
            .map(|i| {
                let at = i * lane.bytes() as i32;
                let ones = self.builder.ins().iconst(int, -1);
                let zero = self.builder.ins().iconst(int, 0);
                match cc {
                    Ok(cc) => {
                        let l = self.builder.ins().load(lane, MEM, a, at);
                        let r = self.builder.ins().load(lane, MEM, b, at);
                        let holds = self.builder.ins().fcmp(cc, l, r);
                        self.builder.ins().select(holds, ones, zero)
                    }
                    Err(0) => zero,
                    Err(_) => ones,
                }
            })
            .collect(); // alloc: none, inline up to 64 lanes
        if scalar {
            self.copy(dst, a, size);
        }
        for (i, value) in values.into_iter().enumerate() {
            self.builder.ins().store(MEM, value, dst, i as i32 * lane.bytes() as i32);
        }
        Ok(())
    }

    fn pack_lanes(
        &mut self,
        a: &Operand,
        b: &Operand,
        dst: Value,
        wide: Type,
        signed: bool,
    ) -> Result<()> {
        let (a, ty) = self.operand(a)?;
        let (b, _) = self.operand(b)?;
        let narrow = wide.half_width().expect("a lane of 16 or 32 bits");
        let bits = narrow.bits();
        let (least, most) = if signed {
            (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1)
        } else {
            (0, (1i64 << bits) - 1)
        };
        let (wide_bytes, narrow_bytes) = (wide.bytes() as i32, narrow.bytes() as i32);
        let per_half = 8 / wide_bytes;
        for half in (0..self.layout(ty)?.size.bytes() as i32).step_by(16) {
            for (from, src) in [a, b].into_iter().enumerate() {
                for lane in 0..per_half {
                    let value = self.builder.ins().load(wide, MEM, src, half + lane * wide_bytes);
                    let most = self.builder.ins().iconst(wide, most);
                    let least = self.builder.ins().iconst(wide, least);
                    let value = self.builder.ins().smin(value, most);
                    let value = self.builder.ins().smax(value, least);
                    let value = self.builder.ins().ireduce(narrow, value);
                    let at = half + (from as i32 * per_half + lane) * narrow_bytes;
                    self.builder.ins().store(MEM, value, dst, at);
                }
            }
        }
        Ok(())
    }

    /// `dst`'s lanes of type `lane`, each `op` of the lanes of `a` and `b` in its place.
    fn lanewise(
        &mut self,
        a: &Operand,
        b: &Operand,
        dst: Value,
        op: Lanewise,
        lane: Type,
    ) -> Result<()> {
        let (a, ty) = self.operand(a)?;
        let (b, _) = self.operand(b)?;
        let bytes = lane.bytes() as i32;
        let bits = i64::from(lane.bits());
        for at in (0..self.layout(ty)?.size.bytes() as i32).step_by(bytes as usize) {
            let l = self.builder.ins().load(lane, MEM, a, at);
            let r = self.builder.ins().load(lane, MEM, b, at);
            let value = match op {
                // `(l + r + 1) >> 1` of the unsigned lanes, in twice their width.
                Lanewise::Average => {
                    let wide = lane.double_width().expect("a lane of 8 or 16 bits");
                    let l = self.builder.ins().uextend(wide, l);
                    let r = self.builder.ins().uextend(wide, r);
                    let sum = self.builder.ins().iadd(l, r);
                    let sum = self.builder.ins().iadd_imm(sum, 1);
                    let half = self.builder.ins().ushr_imm(sum, 1);
                    self.builder.ins().ireduce(lane, half)
                }
                // The high half of the product, the lanes signed or not.
                Lanewise::MulHigh { signed } => {
                    let wide = lane.double_width().expect("a lane of 16 bits");
                    let (l, r) = if signed {
                        (self.builder.ins().sextend(wide, l), self.builder.ins().sextend(wide, r))
                    } else {
                        (self.builder.ins().uextend(wide, l), self.builder.ins().uextend(wide, r))
                    };
                    let product = self.builder.ins().imul(l, r);
                    let high = self.builder.ins().ushr_imm(product, bits);
                    self.builder.ins().ireduce(lane, high)
                }
                // A count of the lane's width or more shifts every bit out: 0, or for the
                // arithmetic shift the sign in every bit. Cranelift's shifts take the count
                // modulo the width, so the count is checked first.
                Lanewise::ShiftLeft | Lanewise::ShiftRight => {
                    let shifted = match op {
                        Lanewise::ShiftLeft => self.builder.ins().ishl(l, r),
                        _ => self.builder.ins().ushr(l, r),
                    };
                    let out =
                        self.builder.ins().icmp_imm(IntCC::UnsignedGreaterThanOrEqual, r, bits);
                    let zero = self.builder.ins().iconst(lane, 0);
                    self.builder.ins().select(out, zero, shifted)
                }
                Lanewise::ShiftRightArithmetic => {
                    let most = self.builder.ins().iconst(lane, bits - 1);
                    let count = self.builder.ins().umin(r, most);
                    self.builder.ins().sshr(l, count)
                }
            };
            self.builder.ins().store(MEM, value, dst, at);
        }
        Ok(())
    }

    /// Calls a function of one instruction on two vectors, with where it reads a third
    /// operand and writes its result.
    fn vector_assembly(
        &mut self,
        text: String,
        a: &Operand,
        b: &Operand,
        dst: Value,
    ) -> Result<()> {
        let (a, _) = self.operand(a)?;
        let (b, _) = self.operand(b)?;
        let mut sig = self.cg.make_signature();
        sig.params.extend([AbiParam::new(self.cg.target.pointer()); 3]);
        let callee = self.cg.assembly(text, &sig);
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        self.builder.ins().call(callee, &[a, b, dst]);
        Ok(())
    }

    /// NEON's operations on pairs of lanes of integers and its table lookup, lane by lane, as
    /// `name` says after `aarch64.neon.`: `umaxp.v16i8` is the larger of each pair of `a`'s
    /// lanes then of `b`'s, `tbl1.v16i8` the bytes of a table its indices name. False for
    /// another.
    fn neon_lanes(&mut self, name: &str, args: &[Operand], dst: Value) -> Result<bool> {
        let mut parts = name.split('.');
        let op = parts.next().unwrap_or_default();
        let first = parts.next();
        let last = parts.last().or(first);
        let Some((lanes, ty)) = last.and_then(|it| {
            let (lanes, bits) = it.strip_prefix('v')?.split_once('i')?;
            Some((lanes.parse::<i32>().ok()?, Type::int(bits.parse().ok()?)?))
        }) else {
            return Ok(false);
        };
        let width = ty.bytes() as i32;
        let combine = |b: &mut FunctionBuilder, l: Value, r: Value| match &op[..op.len() - 1] {
            "umax" => b.ins().umax(l, r),
            "smax" => b.ins().smax(l, r),
            "umin" => b.ins().umin(l, r),
            "smin" => b.ins().smin(l, r),
            _ => b.ins().iadd(l, r),
        };
        match (op, args) {
            // Cranelift's pairwise add is NEON's of 16 bytes, which LLVM selects again from it.
            ("addp", [a, b]) if lanes * width == 16 && ty.bits() <= 32 => {
                let vector = ty.by(lanes as u32).expect("16 bytes of lanes");
                let (a, _) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                let a = self.builder.ins().load(vector, MEM, a, 0);
                let b = self.builder.ins().load(vector, MEM, b, 0);
                let sum = self.builder.ins().iadd_pairwise(a, b);
                self.builder.ins().store(MEM, sum, dst, 0);
            }
            ("umaxp" | "smaxp" | "uminp" | "sminp" | "addp", [a, b]) => {
                let (a, _) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                let half = lanes / 2;
                let values: LaneValues = (0..lanes)
                    .map(|lane| {
                        let from = if lane < half { a } else { b };
                        let at = (lane % half) * 2 * width;
                        let l = self.builder.ins().load(ty, MEM, from, at);
                        let r = self.builder.ins().load(ty, MEM, from, at + width);
                        combine(&mut self.builder, l, r)
                    })
                    .collect(); // alloc: none, inline up to 64 lanes
                for (lane, value) in values.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, lane as i32 * width);
                }
            }
            ("sqdmulh", [a, b]) if ty.bits() <= 32 => {
                let (a, _) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                let wide = ty.double_width().expect("a lane of at most 32 bits");
                let bits = i64::from(ty.bits());
                // The high half of twice the product, which is past the lane's range only for
                // the least by itself: that is the most.
                let values: LaneValues = (0..lanes)
                    .map(|lane| {
                        let l = self.builder.ins().load(ty, MEM, a, lane * width);
                        let r = self.builder.ins().load(ty, MEM, b, lane * width);
                        let l = self.builder.ins().sextend(wide, l);
                        let r = self.builder.ins().sextend(wide, r);
                        let product = self.builder.ins().imul(l, r);
                        let high = self.builder.ins().sshr_imm(product, bits - 1);
                        let most = self.builder.ins().iconst(wide, (1 << (bits - 1)) - 1);
                        let high = self.builder.ins().smin(high, most);
                        self.builder.ins().ireduce(ty, high)
                    })
                    .collect(); // alloc: none, inline up to 64 lanes
                for (lane, value) in values.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, lane as i32 * width);
                }
            }
            ("uaddlp" | "saddlp", [a]) if ty.bits() <= 32 => {
                let (a, _) = self.operand(a)?;
                let wide = ty.double_width().expect("a lane of at most 32 bits");
                // Each lane twice as wide is the sum of a pair of `a`'s.
                let values: LaneValues = (0..lanes / 2)
                    .map(|pair| {
                        let l = self.builder.ins().load(ty, MEM, a, pair * 2 * width);
                        let r = self.builder.ins().load(ty, MEM, a, pair * 2 * width + width);
                        let (l, r) = if op == "saddlp" {
                            (
                                self.builder.ins().sextend(wide, l),
                                self.builder.ins().sextend(wide, r),
                            )
                        } else {
                            (
                                self.builder.ins().uextend(wide, l),
                                self.builder.ins().uextend(wide, r),
                            )
                        };
                        self.builder.ins().iadd(l, r)
                    })
                    .collect(); // alloc: none, inline up to 64 lanes
                for (pair, value) in values.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, pair as i32 * 2 * width);
                }
            }
            ("uaddlv" | "saddlv" | "uaddv" | "saddv", [a]) => {
                let Some(sum_ty) =
                    first.and_then(|it| Type::int(it.strip_prefix('i')?.parse().ok()?))
                else {
                    return Ok(false);
                };
                let (a, _) = self.operand(a)?;
                // The sum of every lane, each as wide as the sum.
                let mut sum = self.builder.ins().iconst(sum_ty, 0);
                for lane in 0..lanes {
                    let value = self.builder.ins().load(ty, MEM, a, lane * width);
                    let value = if sum_ty.bits() < ty.bits() {
                        self.builder.ins().ireduce(sum_ty, value)
                    } else if sum_ty.bits() == ty.bits() {
                        value
                    } else if op.starts_with('s') {
                        self.builder.ins().sextend(sum_ty, value)
                    } else {
                        self.builder.ins().uextend(sum_ty, value)
                    };
                    sum = self.builder.ins().iadd(sum, value);
                }
                self.builder.ins().store(MEM, sum, dst, 0);
            }
            ("sqdmull", [a, b]) if ty.bits() >= 32 => {
                let (a, _) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                let narrow = ty.half_width().expect("a lane of at least 32 bits");
                let least = -(1i64 << (narrow.bits() - 1));
                // Twice the product, past the lane's range only for the least by itself: that
                // is the most.
                let values: LaneValues = (0..lanes)
                    .map(|lane| {
                        let at = lane * narrow.bytes() as i32;
                        let l = self.builder.ins().load(narrow, MEM, a, at);
                        let r = self.builder.ins().load(narrow, MEM, b, at);
                        let l_least = self.builder.ins().icmp_imm(IntCC::Equal, l, least);
                        let r_least = self.builder.ins().icmp_imm(IntCC::Equal, r, least);
                        let both = self.builder.ins().band(l_least, r_least);
                        let l = self.builder.ins().sextend(ty, l);
                        let r = self.builder.ins().sextend(ty, r);
                        let product = self.builder.ins().imul(l, r);
                        let twice = self.builder.ins().ishl_imm(product, 1);
                        let most = match ty.bits() {
                            64 => i64::MAX,
                            bits => (1i64 << (bits - 1)) - 1,
                        };
                        let most = self.builder.ins().iconst(ty, most);
                        self.builder.ins().select(both, most, twice)
                    })
                    .collect(); // alloc: none, inline up to 64 lanes
                for (lane, value) in values.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, lane as i32 * width);
                }
            }
            ("tbl1" | "tbx1", [.., table, indices]) if ty == types::I8 => {
                let kept = match args {
                    [kept, _, _] if op == "tbx1" => Some(self.operand(kept)?.0),
                    [_, _] if op == "tbl1" => None,
                    _ => return Ok(false),
                };
                let (table, _) = self.operand(table)?;
                let (indices, _) = self.operand(indices)?;
                // A lane is the table's byte its index names; past the table's 16, 0 or (for
                // `tbx1`) the lane of `kept`.
                let values: LaneValues = (0..lanes)
                    .map(|lane| {
                        let index = self.builder.ins().load(types::I8, MEM, indices, lane);
                        let slot = self.builder.ins().band_imm(index, 0x0f);
                        let slot = self.builder.ins().uextend(self.cg.target.pointer(), slot);
                        let addr = self.builder.ins().iadd(table, slot);
                        let found = self.builder.ins().load(types::I8, MEM, addr, 0);
                        let outside = match kept {
                            Some(kept) => self.builder.ins().load(types::I8, MEM, kept, lane),
                            None => self.builder.ins().iconst(types::I8, 0),
                        };
                        let past =
                            self.builder.ins().icmp_imm(IntCC::UnsignedGreaterThan, index, 15);
                        self.builder.ins().select(past, outside, found)
                    })
                    .collect(); // alloc: none, inline up to 64 lanes
                for (lane, value) in values.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, lane as i32);
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Calls the instructions `neon::lower` made of the intrinsic `name` on its
    /// operands and writes what they leave where `dst` is, once each operand and the result
    /// is of the size the name says.
    fn neon_assembly(
        &mut self,
        name: &str,
        mut lowered: neon::Lowered,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        // alloc: none, inline up to 8 operands
        let mut args: smallvec::SmallVec<[&Operand; 8]> = args.iter().collect();
        let mut with_immediate = None;
        if let Some(immediate) = &lowered.immediate {
            if immediate.arg >= args.len() {
                unsupported!("`{name}` with {} arguments", args.len());
            }
            let arg = args.remove(immediate.arg);
            let value = match immediate.splat {
                Some(lane) => self.splat_immediate(arg, lane)?,
                None => self.immediate(arg)?,
            };
            let range = &immediate.range;
            let Some(value) =
                value.and_then(|it| u32::try_from(it).ok()).filter(|it| range.contains(it))
            else {
                unsupported!(
                    "`{name}` with an immediate that is not a constant of {}..={}",
                    range.start(),
                    range.end()
                );
            };
            // alloc: the function's text with its immediate, which is the output
            with_immediate = Some(lowered.text.replace("{imm}", &value.to_string()));
        }
        if let Some(text) = with_immediate {
            lowered.text = text;
        }
        let args = args
            .into_iter()
            .map(|arg| Ok((self.operand(arg)?.0, self.operand_ty(arg)?)))
            .collect::<Result<smallvec::SmallVec<[_; 8]>>>()?;
        self.neon_call(name, &lowered, &args, dst, dst_ty)
    }

    /// The lane each lane of a constant vector operand is, as an immediate of lanes of
    /// `lane` bytes: `core::arch` passes a shift so (`const { int8x8_t([N as i8; 8]) }`).
    /// It comes as a constant, or a local assigned the vector (`int8x8_t(lanes)`) of an array
    /// of the lane repeated (`[N as i8; 8]`).
    fn splat_immediate(&mut self, operand: &Operand, lane: u32) -> Result<Option<u128>> {
        let given;
        let blob;
        let evaluated;
        let memory: &[u8] = match &operand.kind {
            OperandKind::Allocation { allocation } => {
                given = allocation.as_ref();
                &given.memory[..]
            }
            OperandKind::Constant { konst, ty } => match konst.as_ref().kind() {
                ConstKind::Value(value) => {
                    blob = self.valtree_blob(ty.as_ref(), value.value)?.bytes;
                    &blob[..]
                }
                ConstKind::Alias(_, named) => {
                    evaluated = self.eval_const(named)?;
                    &evaluated.memory[..]
                }
                _ => return Ok(None),
            },
            OperandKind::Copy(_) | OperandKind::Move(_) => {
                return match self.assigned_once(operand) {
                    Some(Rvalue::Use(from)) => self.splat_immediate(from, lane),
                    Some(Rvalue::Aggregate(AggregateKind::Adt(..), fields))
                        if fields.len() == 1 =>
                    {
                        self.splat_immediate(&fields[0], lane)
                    }
                    Some(Rvalue::Repeat(from, _)) => {
                        let ty = self.operand_ty(from)?;
                        if self.layout(ty)?.size.bytes() != u64::from(lane) {
                            return Ok(None);
                        }
                        self.immediate(from)
                    }
                    _ => Ok(None),
                };
            }
            _ => return Ok(None),
        };
        let ty = self.operand_ty(operand)?;
        let size = self.layout(ty)?.size.bytes_usize();
        let Some(memory) = memory.get(..size) else { return Ok(None) };
        let mut lanes = memory.chunks_exact(lane as usize);
        let Some(first) = lanes.next() else { return Ok(None) };
        if !lanes.remainder().is_empty() || lanes.any(|it| it != first) {
            return Ok(None);
        }
        let mut bytes = [0; 16];
        bytes[..first.len()].copy_from_slice(first);
        Ok(Some(u128::from_le_bytes(bytes)))
    }

    /// The instruction LLVM selects for this body when it is one of `core::arch`'s NEON fused
    /// multiply-adds on AArch64 ([`neon::fused`]), and the function's name. Refused where
    /// rustc's `-C link-dead-code` decides the instruction ([`neon::splats_c`]).
    fn neon_fused(&self) -> Result<Option<(String, neon::Lowered)>> {
        let db = self.cg.db;
        if self.cg.target.arch != Arch::Aarch64 {
            return Ok(None);
        }
        let Some(func) = self.body.and_then(|it| it.owner.as_function()) else { return Ok(None) };
        let name = FunctionSignature::of(db, func).name.as_str();
        if !func.krate(db).data(db).origin.is_lang() || !name.starts_with("vfm") {
            return Ok(None);
        }
        let args = self.args.as_ref().map(|it| it.as_ref());
        let lane = match args.and_then(|it| it.iter().find_map(|it| it.konst())) {
            Some(konst) => {
                let ConstKind::Value(lane) = konst.kind() else { return Ok(None) };
                Some(lane.value.inner().to_leaf().to_bits(Size::from_bytes(4)) as u32)
            }
            None => None,
        };
        let Some(lowered) = neon::fused(name, lane) else { return Ok(None) };
        if backend::links_dead_code() && neon::splats_c(name, lane) {
            unsupported!(
                "`{name}` where rustc has `-C link-dead-code`: whether LLVM sees the splat of \
                 `c`, which decides the NaN that comes out, is up to the codegen unit rustc \
                 keeps `splat` in"
            );
        }
        // alloc: once per body of one of `core::arch`'s `vfm*`, whose callers take the name owned
        Ok(Some((name.to_owned(), lowered)))
    }

    /// [`Self::neon_assembly`] of the operands where `args` are.
    fn neon_call(
        &mut self,
        name: &str,
        lowered: &neon::Lowered,
        args: &[(Value, Ty<'db>)],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        if args.len() != lowered.args.len() {
            unsupported!("`{name}` with {} arguments", args.len());
        }
        // alloc: none, inline up to 8 registers
        let mut values: smallvec::SmallVec<[Value; 8]> = smallvec::smallvec![dst];
        for (&(arg, ty), expected) in args.iter().zip(&lowered.args) {
            let pointer = matches!(ty.kind(), TyKind::RawPtr(..));
            let fits = match expected {
                None => pointer,
                Some(bytes) => !pointer && self.layout(ty)?.size.bytes() == u64::from(*bytes),
            };
            if !fits {
                unsupported!("`{name}` of {ty:?}, which is not what its name says");
            }
            values.push(arg);
        }
        // One result, or a tuple of vectors (`int8x16x2_t`) whose fields are the registers.
        let layout = self.layout(dst_ty)?;
        let size: u64 = lowered.results.iter().map(|&it| u64::from(it)).sum();
        let results: smallvec::SmallVec<[(u32, u32); 4]> = match lowered.results.as_slice() {
            [] if layout.is_zst() => smallvec::SmallVec::new(),
            &[bytes] if layout.size.bytes() == u64::from(bytes) => {
                smallvec::smallvec![(0, bytes)] // alloc: none, inline
            }
            many if layout.size.bytes() == size
                && layout.fields.count() == many.len()
                && (0..many.len())
                    .all(|i| layout.fields.offset(i).bytes() == i as u64 * u64::from(many[0])) =>
            {
                // alloc: none, inline up to 4, the registers of `int8x16x4_t`
                many.iter().enumerate().map(|(i, &bytes)| (i as u32 * bytes, bytes)).collect()
            }
            _ => unsupported!("`{name}` returning {dst_ty:?}, which is not what its name says"),
        };
        let directives = asm::extensions(Arch::Aarch64, lowered.features.iter().copied());
        let text = asm::neon_function(&directives, &lowered.args, &lowered.text, &results);
        let mut sig = self.cg.make_signature();
        sig.params.extend(values.iter().map(|_| AbiParam::new(self.cg.target.pointer())));
        let callee = self.cg.assembly(text, &sig);
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        self.builder.ins().call(callee, &values);
        Ok(())
    }

    /// Calls the instruction `avx512::lower` makes of the x86 intrinsic `name` on its operands
    /// and writes what it leaves where `dst` is. False for an intrinsic it does not know or of
    /// operands other than its name says.
    fn avx512_assembly(
        &mut self,
        name: &str,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<bool> {
        let mut given = Vec::with_capacity(args.len());
        for arg in args {
            let ty = self.operand_ty(arg)?;
            given.push(avx512::Given {
                bytes: self.layout(ty)?.size.bytes() as u32,
                pointer: matches!(ty.kind(), TyKind::RawPtr(..)),
                constant: self.immediate(arg)?,
            });
        }
        let layout = self.layout(dst_ty)?;
        let func = self.body.and_then(|it| it.owner.as_function());
        let features = func.map_or(hir_ty::TargetFeatureSet::EMPTY, |func| {
            hir_ty::TargetFeatures::from_fn_no_implications(self.cg.db, func).known()
        });
        let lowered = match avx512::lower(name, &given, layout.size.bytes() as u32, features) {
            None => return Ok(false),
            Some(lowered) => lowered.map_err(CodegenError::Unsupported)?,
        };
        if let avx512::Out::Masks(_, at) = lowered.out
            && (layout.fields.count() != 2 || layout.fields.offset(1).bytes() != u64::from(at))
        {
            unsupported!("`llvm.{name}` returning {dst_ty:?}");
        }
        self.call_lowered(&lowered, args, dst)?;
        Ok(true)
    }

    /// Calls the function of `lowered`'s instructions on `args`, which it loads as it says,
    /// and writes what it leaves where `dst` is.
    fn call_lowered(
        &mut self,
        lowered: &avx512::Lowered,
        args: &[Operand],
        dst: Value,
    ) -> Result<()> {
        let mut addrs = Vec::with_capacity(args.len());
        for (arg, load) in args.iter().zip(&lowered.loads) {
            addrs.push(match load {
                avx512::Load::Immediate => None,
                _ => Some(self.operand(arg)?.0),
            });
        }
        self.call_assembled_at(lowered, &addrs, dst);
        Ok(())
    }

    /// Calls the function of `lowered`'s instructions on the operands at `addrs`.
    fn call_assembled(&mut self, lowered: &avx512::Lowered, addrs: &[Value], dst: Value) {
        let addrs: smallvec::SmallVec<[Option<Value>; 8]> =
            addrs.iter().copied().map(Some).collect(); // alloc: none, inline up to 8 operands
        self.call_assembled_at(lowered, &addrs, dst);
    }

    fn call_assembled_at(
        &mut self,
        lowered: &avx512::Lowered,
        addrs: &[Option<Value>],
        dst: Value,
    ) {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8 * (1 + addrs.len() as u32),
            3,
        ));
        let slot = self.builder.ins().stack_addr(self.cg.target.pointer(), slot, 0);
        self.builder.ins().store(MEM, dst, slot, 0);
        for (i, addr) in addrs.iter().enumerate() {
            if let Some(addr) = *addr {
                self.builder.ins().store(MEM, addr, slot, 8 * (i as i32 + 1));
            }
        }
        let text = asm::avx512_function(
            self.cg.target.call_conv,
            &lowered.loads,
            &lowered.text,
            lowered.out,
        );
        let mut sig = self.cg.make_signature();
        sig.params.push(AbiParam::new(self.cg.target.pointer()));
        let callee = self.cg.assembly(text, &sig);
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        self.builder.ins().call(callee, &[slot]);
    }

    /// Calls the instructions `x86::lower` made of the intrinsic `llvm.{name}`: the arguments
    /// are copied to a slot the function reads its registers from, and it writes each result
    /// where `dst` is, a result of several registers field by field as its layout places them.
    fn x86_assembly(
        &mut self,
        name: &str,
        lowered: &x86::Lowered,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        if args.len() != lowered.args.len() {
            unsupported!("`llvm.{name}` with {} arguments", args.len());
        }
        let mut text = std::borrow::Cow::Borrowed(lowered.text.as_str());
        let mut reads = Vec::with_capacity(args.len());
        let mut copies = Vec::with_capacity(args.len());
        let mut size = 0;
        for (arg, register) in args.iter().zip(&lowered.args) {
            let Some(register) = register else {
                let Some(immediate) = self.immediate(arg)?.filter(|it| *it <= 0xff) else {
                    unsupported!("`llvm.{name}` with an immediate that is not a constant byte");
                };
                // alloc: the function's text with its immediate, which is the output
                text = std::borrow::Cow::Owned(text.replace("{imm}", &immediate.to_string()));
                continue;
            };
            let bytes = asm::x86_register_bytes(register).expect("a register of x86");
            let ty = self.operand_ty(arg)?;
            if self.layout(ty)?.size.bytes() != u64::from(bytes) {
                unsupported!("`llvm.{name}` of {ty:?}, which is not what its name says");
            }
            copies.push((self.operand(arg)?.0, size, bytes));
            reads.push((*register, size));
            size += bytes;
        }
        let layout = self.layout(dst_ty)?;
        let register_bytes = |it: &str| u64::from(asm::x86_register_bytes(it).expect("x86's"));
        let writes: smallvec::SmallVec<[(&str, u32); 8]> = match lowered.results.as_slice() {
            [] if layout.is_zst() => smallvec::SmallVec::new(),
            &[register] if layout.size.bytes() == register_bytes(register) => {
                smallvec::smallvec![(register, 0)] // alloc: none, inline
            }
            many => {
                let fields: smallvec::SmallVec<[Ty<'db>; 8]> = match dst_ty.kind() {
                    TyKind::Tuple(tys) => tys.iter().collect(), // alloc: none, inline up to 8
                    TyKind::Adt(adt_def, subst) if let AdtId::StructId(id) = adt_def.def_id() => {
                        let field_types = self.cg.db.field_types(id.into());
                        field_types
                            .iter()
                            .map(|(_, it)| {
                                let ty = it.ty().instantiate(self.interner, subst).skip_norm_wip();
                                self.normalize(ty)
                            })
                            .collect() // alloc: none, inline up to 8
                    }
                    _ => smallvec::SmallVec::new(),
                };
                let mut writes = smallvec::SmallVec::new();
                for (i, (&register, &field)) in many.iter().zip(&fields).enumerate() {
                    if self.layout(field)?.size.bytes() != register_bytes(register) {
                        break;
                    }
                    writes.push((register, layout.fields.offset(i).bytes() as u32));
                }
                if fields.len() != many.len() || writes.len() != many.len() {
                    unsupported!(
                        "`llvm.{name}` returning {dst_ty:?}, which is not what its name says"
                    );
                }
                writes
            }
        };
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size.max(1),
            4,
        ));
        let slot = self.builder.ins().stack_addr(self.cg.target.pointer(), slot, 0);
        for (from, at, bytes) in copies {
            let to = self.builder.ins().iadd_imm(slot, i64::from(at));
            self.copy(to, from, Size::from_bytes(bytes));
        }
        let text =
            asm::x86_function(self.cg.target.call_conv, &text, &reads, &writes, &lowered.clobbers)
                .map_err(CodegenError::Unsupported)?;
        let mut sig = self.cg.make_signature();
        sig.params.extend([AbiParam::new(self.cg.target.pointer()); 2]);
        let callee = self.cg.assembly(text, &sig);
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        self.builder.ins().call(callee, &[slot, dst]);
        Ok(())
    }

    /// A function `core::arch` declares as `llvm.{name}`: something LLVM implements, with no
    /// body anywhere.
    fn llvm_intrinsic(
        &mut self,
        name: &str,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        if let Some(neon) = name.strip_prefix("aarch64.neon.")
            && (self.neon_vector(neon, args, dst)? || self.neon_lanes(neon, args, dst)?)
        {
            return Ok(());
        }
        if self.x86_vector(name, args, dst)? {
            return Ok(());
        }
        if name.starts_with("aarch64.sve.")
            || name.starts_with("vector.reverse.nxv")
            || name.starts_with("vector.insert.nxv")
        {
            return self.sve_intrinsic(name, args, dst, dst_ty);
        }
        if self.wasm_llvm_intrinsic(name, args, dst)? {
            return Ok(());
        }
        match (name, args) {
            (
                "x86.sse.cmp.ps" | "x86.sse2.cmp.pd" | "x86.avx.cmp.ps.256" | "x86.avx.cmp.pd.256"
                | "x86.sse.cmp.ss" | "x86.sse2.cmp.sd",
                [a, b, predicate],
            ) => {
                let Some(predicate) = self.immediate(predicate)? else {
                    unsupported!("`llvm.{name}` with a predicate that is not a constant");
                };
                let double = name.contains(".pd") || name.ends_with(".sd");
                let lane = if double { types::F64 } else { types::F32 };
                let scalar = name.ends_with(".ss") || name.ends_with(".sd");
                self.compare_lanes(a, b, predicate as u8, dst, lane, scalar)?
            }
            ("x86.sse3.ldu.dq" | "x86.avx.ldu.dq.256", [ptr]) => {
                let (ptr, _) = self.load_scalar(ptr)?;
                let size = self.layout(dst_ty)?.size;
                self.copy(dst, ptr, size);
            }
            (
                "x86.ssse3.pshuf.b.128" | "x86.avx2.pshuf.b" | "x86.avx512.pshuf.b.512",
                [table, indices],
            ) => {
                let (table, ty) = self.operand(table)?;
                let (indices, _) = self.operand(indices)?;
                let size = self.layout(ty)?.size.bytes();
                // Each 16-byte half looks up bytes in itself.
                for i in 0..size {
                    let index = self.builder.ins().load(types::I8, MEM, indices, i as i32);
                    let slot = self.builder.ins().band_imm(index, 0x0f);
                    let slot = self.builder.ins().uextend(self.cg.target.pointer(), slot);
                    let addr = self.builder.ins().iadd(table, slot);
                    let found = self.builder.ins().load(types::I8, MEM, addr, (i & !15) as i32);
                    let zero = self.builder.ins().iconst(types::I8, 0);
                    let cleared = self.builder.ins().icmp_imm(IntCC::SignedLessThan, index, 0);
                    let value = self.builder.ins().select(cleared, zero, found);
                    self.builder.ins().store(MEM, value, dst, i as i32);
                }
            }
            (_, [table, indices]) if name.starts_with("x86.avx512.permvar.") => {
                let (table, ty) = self.operand(table)?;
                let (indices, _) = self.operand(indices)?;
                // `permvar.{qi,hi,si,sf,di,df}.N`: each lane is the lane of `table` its index
                // names, by as many low bits as name a lane of the whole vector.
                let width = match name.split('.').nth(3) {
                    Some("qi") => types::I8,
                    Some("hi") => types::I16,
                    Some("si" | "sf") => types::I32,
                    _ => types::I64,
                };
                let bytes = width.bytes() as i64;
                let lanes = self.layout(ty)?.size.bytes() as i64 / bytes;
                let values: LaneValues = (0..lanes)
                    .map(|lane| {
                        let at = (lane * bytes) as i32;
                        let index = self.builder.ins().load(width, MEM, indices, at);
                        let slot = self.builder.ins().band_imm(index, lanes - 1);
                        let slot = match width == self.cg.target.pointer() {
                            true => slot,
                            false => self.builder.ins().uextend(self.cg.target.pointer(), slot),
                        };
                        let offset = self.builder.ins().imul_imm(slot, bytes);
                        let addr = self.builder.ins().iadd(table, offset);
                        self.builder.ins().load(width, MEM, addr, 0)
                    })
                    .collect(); // alloc: none, inline up to 64 lanes
                for (lane, value) in values.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, lane as i32 * bytes as i32);
                }
            }
            ("x86.avx2.permd" | "x86.avx2.permps", [table, indices]) => {
                let (table, _) = self.operand(table)?;
                let (indices, _) = self.operand(indices)?;
                // Each of the eight lanes is the lane its index names, by its low three bits.
                let lanes: LaneValues = (0..8)
                    .map(|lane| {
                        let index = self.builder.ins().load(types::I32, MEM, indices, lane * 4);
                        let slot = self.builder.ins().band_imm(index, 7);
                        let slot = self.builder.ins().uextend(self.cg.target.pointer(), slot);
                        let offset = self.builder.ins().ishl_imm(slot, 2);
                        let addr = self.builder.ins().iadd(table, offset);
                        self.builder.ins().load(types::I32, MEM, addr, 0)
                    })
                    .collect(); // alloc: none, inline up to 64 lanes
                for (lane, value) in lanes.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, lane as i32 * 4);
                }
            }
            ("x86.sse2.psad.bw" | "x86.avx2.psad.bw" | "x86.avx512.psad.bw.512", [a, b]) => {
                let (a, ty) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                // Each 64 bits are the sum of how far apart eight bytes of the two are.
                for lane in (0..self.layout(ty)?.size.bytes() as i32).step_by(8) {
                    let mut sum = self.builder.ins().iconst(types::I64, 0);
                    for byte in lane..lane + 8 {
                        let l = self.builder.ins().load(types::I8, MEM, a, byte);
                        let r = self.builder.ins().load(types::I8, MEM, b, byte);
                        let more = self.builder.ins().umax(l, r);
                        let less = self.builder.ins().umin(l, r);
                        let apart = self.builder.ins().isub(more, less);
                        let apart = self.builder.ins().uextend(types::I64, apart);
                        sum = self.builder.ins().iadd(sum, apart);
                    }
                    self.builder.ins().store(MEM, sum, dst, lane);
                }
            }
            ("x86.sse2.pmadd.wd" | "x86.avx2.pmadd.wd" | "x86.avx512.pmaddw.d.512", [a, b]) => {
                let (a, ty) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                // Each 32 bits are the sum of the products of two pairs of signed words.
                for lane in (0..self.layout(ty)?.size.bytes() as i32).step_by(4) {
                    let mut sum = self.builder.ins().iconst(types::I32, 0);
                    for word in [lane, lane + 2] {
                        let l = self.builder.ins().load(types::I16, MEM, a, word);
                        let r = self.builder.ins().load(types::I16, MEM, b, word);
                        let l = self.builder.ins().sextend(types::I32, l);
                        let r = self.builder.ins().sextend(types::I32, r);
                        let product = self.builder.ins().imul(l, r);
                        sum = self.builder.ins().iadd(sum, product);
                    }
                    self.builder.ins().store(MEM, sum, dst, lane);
                }
            }
            (
                "x86.ssse3.pmadd.ub.sw.128" | "x86.avx2.pmadd.ub.sw" | "x86.avx512.pmaddubs.w.512",
                [a, b],
            ) => {
                let (a, ty) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                // Each 16 bits are the saturated sum of two products: an unsigned byte of
                // `a` by a signed one of `b`.
                for lane in (0..self.layout(ty)?.size.bytes() as i32).step_by(2) {
                    let mut sum = self.builder.ins().iconst(types::I32, 0);
                    for byte in [lane, lane + 1] {
                        let l = self.builder.ins().load(types::I8, MEM, a, byte);
                        let r = self.builder.ins().load(types::I8, MEM, b, byte);
                        let l = self.builder.ins().uextend(types::I32, l);
                        let r = self.builder.ins().sextend(types::I32, r);
                        let product = self.builder.ins().imul(l, r);
                        sum = self.builder.ins().iadd(sum, product);
                    }
                    let most = self.builder.ins().iconst(types::I32, i64::from(i16::MAX));
                    let least = self.builder.ins().iconst(types::I32, i64::from(i16::MIN));
                    let sum = self.builder.ins().smin(sum, most);
                    let sum = self.builder.ins().smax(sum, least);
                    let sum = self.builder.ins().ireduce(types::I16, sum);
                    self.builder.ins().store(MEM, sum, dst, lane);
                }
            }
            // `minps` and `maxps`: the second operand when either is a NaN, or both are equal.
            (
                "x86.sse.min.ps" | "x86.sse.max.ps" | "x86.avx.min.ps.256" | "x86.avx.max.ps.256"
                | "x86.sse2.min.pd" | "x86.sse2.max.pd" | "x86.avx.min.pd.256"
                | "x86.avx.max.pd.256",
                [a, b],
            ) => {
                let (a, ty) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                let (lane, step) = if name.ends_with("pd") || name.contains(".pd.") {
                    (types::F64, 8)
                } else {
                    (types::F32, 4)
                };
                let cond =
                    if name.contains(".min.") { FloatCC::LessThan } else { FloatCC::GreaterThan };
                for at in (0..self.layout(ty)?.size.bytes() as i32).step_by(step) {
                    let l = self.builder.ins().load(lane, MEM, a, at);
                    let r = self.builder.ins().load(lane, MEM, b, at);
                    let first = self.builder.ins().fcmp(cond, l, r);
                    let chosen = self.builder.ins().select(first, l, r);
                    self.builder.ins().store(MEM, chosen, dst, at);
                }
            }
            // `roundps` and `roundpd`: the mode is the low two bits of the immediate, or the
            // processor's, which is to nearest (`4` and above).
            (
                "x86.sse41.round.ps"
                | "x86.avx.round.ps.256"
                | "x86.sse41.round.pd"
                | "x86.avx.round.pd.256",
                [a, mode],
            ) => {
                let (a, ty) = self.operand(a)?;
                let Some(mode) = self.immediate(mode)? else {
                    unsupported!("LLVM intrinsic `{name}` with a mode that is not a constant");
                };
                let (lane, step) = if name.ends_with("pd") || name.contains(".pd.") {
                    (types::F64, 8)
                } else {
                    (types::F32, 4)
                };
                for at in (0..self.layout(ty)?.size.bytes() as i32).step_by(step) {
                    let value = self.builder.ins().load(lane, MEM, a, at);
                    let rounded = match if mode & 4 != 0 { 0 } else { mode & 3 } {
                        0 => self.builder.ins().nearest(value),
                        1 => self.builder.ins().floor(value),
                        2 => self.builder.ins().ceil(value),
                        _ => self.builder.ins().trunc(value),
                    };
                    self.builder.ins().store(MEM, rounded, dst, at);
                }
            }
            // `cvttps2dq`: truncation, and the "integer indefinite" for what does not fit,
            // which saturation gives for the negative side only.
            ("x86.sse2.cvttps2dq" | "x86.avx.cvtt.ps2dq.256", [a]) => {
                let (a, ty) = self.operand(a)?;
                for at in (0..self.layout(ty)?.size.bytes() as i32).step_by(4) {
                    let value = self.builder.ins().load(types::F32, MEM, a, at);
                    let truncated = self.builder.ins().fcvt_to_sint_sat(types::I32, value);
                    let limit = self.builder.ins().f32const(2147483648.0);
                    let over = self.builder.ins().fcmp(
                        FloatCC::UnorderedOrGreaterThanOrEqual,
                        value,
                        limit,
                    );
                    let indefinite = self.builder.ins().iconst(types::I32, i64::from(i32::MIN));
                    let result = self.builder.ins().select(over, indefinite, truncated);
                    self.builder.ins().store(MEM, result, dst, at);
                }
            }
            // `psrlw` by one count for all the lanes: the low 64 bits of the second operand,
            // and zero when it is above 15.
            ("x86.sse2.psrl.w" | "x86.avx2.psrl.w", [a, count]) => {
                let (a, ty) = self.operand(a)?;
                let (count, _) = self.operand(count)?;
                let count = self.builder.ins().load(types::I64, MEM, count, 0);
                let over = self.builder.ins().icmp_imm(IntCC::UnsignedGreaterThan, count, 15);
                let count = self.builder.ins().ireduce(types::I16, count);
                let zero = self.builder.ins().iconst(types::I16, 0);
                for at in (0..self.layout(ty)?.size.bytes() as i32).step_by(2) {
                    let word = self.builder.ins().load(types::I16, MEM, a, at);
                    let shifted = self.builder.ins().ushr(word, count);
                    let result = self.builder.ins().select(over, zero, shifted);
                    self.builder.ins().store(MEM, result, dst, at);
                }
            }
            // Lane by lane: the rounded average, the high half of the product and the shifts
            // by a count per lane (the toolchain's `core::arch` calls LLVM for these since
            // nightly-2026-10-25, where 1.99.0's wrote them with `simd_*`).
            (_, [a, b]) if let Some((op, lane)) = x86_lanewise(name) => {
                self.lanewise(a, b, dst, op, lane)?
            }
            ("x86.sse2.packsswb.128" | "x86.avx2.packsswb" | "x86.avx512.packsswb.512", [a, b]) => {
                self.pack_lanes(a, b, dst, types::I16, true)?
            }
            ("x86.sse2.packuswb.128" | "x86.avx2.packuswb" | "x86.avx512.packuswb.512", [a, b]) => {
                self.pack_lanes(a, b, dst, types::I16, false)?
            }
            ("x86.sse2.packssdw.128" | "x86.avx2.packssdw" | "x86.avx512.packssdw.512", [a, b]) => {
                self.pack_lanes(a, b, dst, types::I32, true)?
            }
            ("x86.sse41.packusdw" | "x86.avx2.packusdw" | "x86.avx512.packusdw.512", [a, b]) => {
                self.pack_lanes(a, b, dst, types::I32, false)?
            }
            (
                "x86.sha1msg1" | "x86.sha1msg2" | "x86.sha1nexte" | "x86.sha256msg1"
                | "x86.sha256msg2",
                [a, b],
            ) => {
                // alloc: the instruction's text, which is the output
                let instruction = format!("{} xmm0, xmm1", name.trim_start_matches("x86."));
                self.vector_assembly(
                    asm::vector_instruction(self.cg.target.call_conv, &instruction, "xmm0"),
                    a,
                    b,
                    dst,
                )?;
            }
            ("x86.sha256rnds2", [a, b, k]) => {
                let (k, ty) = self.operand(k)?;
                let layout = self.layout(ty)?;
                self.copy_value(dst, k, &layout);
                // The instruction takes its third vector in `xmm0`.
                let rounds = "movdqa xmm3, xmm0\nmovdqa xmm0, xmm2\nsha256rnds2 xmm3, xmm1";
                self.vector_assembly(
                    asm::vector_instruction(self.cg.target.call_conv, rounds, "xmm3"),
                    a,
                    b,
                    dst,
                )?;
            }
            (
                "aarch64.crypto.sha256su0"
                | "aarch64.crypto.sha512su0"
                | "aarch64.crypto.sha1su1"
                | "aarch64.crypto.aese"
                | "aarch64.crypto.aesd",
                [a, b],
            ) => {
                let instruction = neon_crypto(name);
                self.vector_assembly(instruction, a, b, dst)?;
            }
            ("aarch64.crypto.aesmc" | "aarch64.crypto.aesimc", [a]) => {
                let instruction = neon_crypto(name);
                self.vector_assembly(instruction, a, a, dst)?;
            }
            (
                "aarch64.crypto.sha256h"
                | "aarch64.crypto.sha256h2"
                | "aarch64.crypto.sha256su1"
                | "aarch64.crypto.sha512h"
                | "aarch64.crypto.sha512h2"
                | "aarch64.crypto.sha512su1"
                | "aarch64.crypto.sha1su0",
                [a, b, c],
            ) => {
                let (c, ty) = self.operand(c)?;
                let size = self.layout(ty)?.size;
                self.copy(dst, c, size);
                let instruction = neon_crypto(name);
                self.vector_assembly(instruction, a, b, dst)?;
            }
            ("x86.sha1rnds4", [a, b, function]) => {
                let (function, _) = self.load_scalar(function)?;
                self.builder.ins().store(MEM, function, dst, 0);
                self.vector_assembly(asm::sha1rnds4(self.cg.target.call_conv), a, b, dst)?;
            }
            ("x86.xgetbv", [register]) => {
                let (register, _) = self.load_scalar(register)?;
                let mut sig = self.cg.make_signature();
                sig.params.push(AbiParam::new(types::I32));
                sig.returns.push(AbiParam::new(types::I64));
                let call_conv = self.cg.target.call_conv;
                let callee = self.cg.assembly(asm::xgetbv(call_conv), &sig);
                let callee = self.cg.declare_func_in_func(callee, self.builder.func);
                let call = self.builder.ins().call(callee, &[register]);
                let value = self.builder.inst_results(call)[0];
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (
                "aarch64.crc32b" | "aarch64.crc32h" | "aarch64.crc32w" | "aarch64.crc32x"
                | "aarch64.crc32cb" | "aarch64.crc32ch" | "aarch64.crc32cw" | "aarch64.crc32cx",
                [crc, data],
            ) => {
                let (crc, _) = self.load_scalar(crc)?;
                let (data, scalar) = self.load_scalar(data)?;
                let mut sig = self.cg.make_signature();
                sig.params.extend([AbiParam::new(types::I32), AbiParam::new(scalar.ty)]);
                sig.returns.push(AbiParam::new(types::I32));
                let callee = self.cg.assembly(asm::crc32(&name["aarch64.".len()..]), &sig);
                let callee = self.cg.declare_func_in_func(callee, self.builder.func);
                let call = self.builder.ins().call(callee, &[crc, data]);
                let value = self.builder.inst_results(call)[0];
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (
                "x86.sse42.crc32.32.8"
                | "x86.sse42.crc32.32.16"
                | "x86.sse42.crc32.32.32"
                | "x86.sse42.crc32.64.64",
                [crc, data],
            ) => {
                let (crc, sum) = self.load_scalar(crc)?;
                let (data, scalar) = self.load_scalar(data)?;
                let mut sig = self.cg.make_signature();
                sig.params.extend([AbiParam::new(sum.ty), AbiParam::new(scalar.ty)]);
                sig.returns.push(AbiParam::new(sum.ty));
                let call_conv = self.cg.target.call_conv;
                let callee = self.cg.assembly(asm::x86_crc32(call_conv, scalar.ty.bits()), &sig);
                let callee = self.cg.declare_func_in_func(callee, self.builder.func);
                let call = self.builder.ins().call(callee, &[crc, data]);
                let value = self.builder.inst_results(call)[0];
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (_, [a, b, c, table]) if name.starts_with("x86.avx512.pternlog.") => {
                let (a, ty) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                let (c, _) = self.operand(c)?;
                let (table, scalar) = self.load_scalar(table)?;
                let table = match scalar.ty {
                    types::I64 => table,
                    _ => self.builder.ins().uextend(types::I64, table),
                };
                // Each bit of the result is the bit of `table` that the bits of `a`, `b` and
                // `c` at its place number, `a`'s the highest: an or of the eight minterms
                // `table` has set. Lanes do not matter to bitwise logic, so 64 bits at a time.
                for at in (0..self.layout(ty)?.size.bytes() as i32).step_by(8) {
                    let bits = [a, b, c].map(|it| self.builder.ins().load(types::I64, MEM, it, at));
                    let mut out = self.builder.ins().iconst(types::I64, 0);
                    for minterm in 0..8i64 {
                        let mut term = self.builder.ins().iconst(types::I64, -1);
                        for (place, value) in bits.into_iter().enumerate() {
                            let set = minterm >> (2 - place) & 1 == 1;
                            let value = if set { value } else { self.builder.ins().bnot(value) };
                            term = self.builder.ins().band(term, value);
                        }
                        let chosen = self.builder.ins().ushr_imm(table, minterm);
                        let chosen = self.builder.ins().band_imm(chosen, 1);
                        let mask = self.builder.ins().ineg(chosen);
                        let term = self.builder.ins().band(term, mask);
                        out = self.builder.ins().bor(out, term);
                    }
                    self.builder.ins().store(MEM, out, dst, at);
                }
            }
            (_, [a, b, c])
                if name.starts_with("x86.avx512.vpmadd52l.uq.")
                    || name.starts_with("x86.avx512.vpmadd52h.uq.") =>
            {
                // IFMA, `a` the sum as core::arch passes it: each 64-bit lane is `a`'s plus
                // the low (`l`) or high (`h`) 52 bits of the 104-bit product of the low 52 bits
                // of `b`'s and `c`'s, wrapping.
                let (a, ty) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                let (c, _) = self.operand(c)?;
                let high = name.starts_with("x86.avx512.vpmadd52h.");
                const LOW_52: i64 = (1 << 52) - 1;
                for at in (0..self.layout(ty)?.size.bytes() as i32).step_by(8) {
                    let [sum, x, y] =
                        [a, b, c].map(|it| self.builder.ins().load(types::I64, MEM, it, at));
                    let x = self.builder.ins().band_imm(x, LOW_52);
                    let y = self.builder.ins().band_imm(y, LOW_52);
                    let low = self.builder.ins().imul(x, y);
                    let part = if high {
                        let upper = self.builder.ins().umulhi(x, y);
                        let upper = self.builder.ins().ishl_imm(upper, 12);
                        let lower = self.builder.ins().ushr_imm(low, 52);
                        self.builder.ins().bor(upper, lower)
                    } else {
                        self.builder.ins().band_imm(low, LOW_52)
                    };
                    let value = self.builder.ins().iadd(sum, part);
                    self.builder.ins().store(MEM, value, dst, at);
                }
            }
            // A barrier of the kind its option names, or the hint its number names, as the
            // instruction's word: `dsb` of option 0 and 4 are the barriers `ssbb` and
            // `pssbb`, which an assembler may take only by those names.
            ("aarch64.dmb" | "aarch64.dsb" | "aarch64.hint", [option]) => {
                let Some(option) = self.immediate(option)? else {
                    unsupported!("`llvm.{name}` of an option that is not a constant");
                };
                let (word, most, at) = match name {
                    "aarch64.dmb" => (0xd503_30bf_u32, 15, 8),
                    "aarch64.dsb" => (0xd503_309f, 15, 8),
                    _ => (0xd503_201f, 127, 5),
                };
                if option > most {
                    unsupported!("`llvm.{name}` of option {option}");
                }
                let word = word | (option as u32) << at;
                let sig = self.cg.make_signature();
                let callee = self.cg.assembly(asm::instruction_word(word), &sig);
                let callee = self.cg.declare_func_in_func(callee, self.builder.func);
                self.builder.ins().call(callee, &[]);
            }
            // A random number, and whether there was none (`rndrrs` reseeds first): the
            // register reads as 0 with the Z flag set then. The result is `(u64, bool)`.
            ("aarch64.rndr" | "aarch64.rndrrs", []) => {
                let layout = self.layout(dst_ty)?;
                if layout.size.bytes() != 16 || layout.fields.offset(1).bytes() != 8 {
                    unsupported!("`llvm.{name}` returning {dst_ty:?}");
                }
                let mut sig = self.cg.make_signature();
                sig.params.push(AbiParam::new(self.cg.target.pointer()));
                let callee = self.cg.assembly(asm::random(name == "aarch64.rndrrs"), &sig);
                let callee = self.cg.declare_func_in_func(callee, self.builder.func);
                self.builder.ins().call(callee, &[dst]);
            }
            ("x86.sse2.pause", []) | ("aarch64.isb", [_]) => {
                let text = if name == "aarch64.isb" { asm::isb() } else { asm::pause() };
                let sig = self.cg.make_signature();
                let callee = self.cg.assembly(text, &sig);
                let callee = self.cg.declare_func_in_func(callee, self.builder.func);
                self.builder.ins().call(callee, &[]);
            }
            ("x86.vcvtps2ph.128" | "x86.vcvtps2ph.256", [singles, rounding]) => {
                let (singles, _) = self.operand(singles)?;
                let (rounding, _) = self.load_scalar(rounding)?;
                let mut sig = self.cg.make_signature();
                sig.params.extend([
                    AbiParam::new(self.cg.target.pointer()),
                    AbiParam::new(self.cg.target.pointer()),
                ]);
                sig.params.push(AbiParam::new(types::I32));
                let register = if name.ends_with("256") { "ymm0" } else { "xmm0" };
                let call_conv = self.cg.target.call_conv;
                let callee = self.cg.assembly(asm::vcvtps2ph(call_conv, register), &sig);
                let callee = self.cg.declare_func_in_func(callee, self.builder.func);
                self.builder.ins().call(callee, &[singles, dst, rounding]);
            }
            ("x86.pclmulqdq" | "x86.pclmulqdq.256" | "x86.pclmulqdq.512", [a, b, halves]) => {
                let (a, ty) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                let (halves, _) = self.load_scalar(halves)?;
                // Bit 0 says which half of each 128 bits of `a` is multiplied, bit 4 of `b`.
                let half = |fx: &mut Self, bit: i64| {
                    let chosen = fx.builder.ins().ushr_imm(halves, bit);
                    let chosen = fx.builder.ins().band_imm(chosen, 1);
                    let chosen = fx.builder.ins().uextend(fx.cg.target.pointer(), chosen);
                    fx.builder.ins().ishl_imm(chosen, 3)
                };
                let (of_a, of_b) = (half(self, 0), half(self, 4));
                for lane in (0..self.layout(ty)?.size.bytes() as i32).step_by(16) {
                    let l = self.builder.ins().iadd(a, of_a);
                    let l = self.builder.ins().load(types::I64, MEM, l, lane);
                    let r = self.builder.ins().iadd(b, of_b);
                    let r = self.builder.ins().load(types::I64, MEM, r, lane);
                    // The product without carries: `l` shifted by every bit set in `r`, xored.
                    let mut low = self.builder.ins().iconst(types::I64, 0);
                    let mut high = low;
                    for bit in 0..64 {
                        let set = self.builder.ins().ushr_imm(r, bit);
                        let set = self.builder.ins().band_imm(set, 1);
                        let mask = self.builder.ins().ineg(set);
                        let part = self.builder.ins().ishl_imm(l, bit);
                        let part = self.builder.ins().band(part, mask);
                        low = self.builder.ins().bxor(low, part);
                        if bit != 0 {
                            let part = self.builder.ins().ushr_imm(l, 64 - bit);
                            let part = self.builder.ins().band(part, mask);
                            high = self.builder.ins().bxor(high, part);
                        }
                    }
                    self.builder.ins().store(MEM, low, dst, lane);
                    self.builder.ins().store(MEM, high, dst, lane + 8);
                }
            }
            (_, [a, indices, b]) if name.starts_with("x86.avx512.vpermi2var.") => {
                let (a, ty) = self.operand(a)?;
                let (indices, _) = self.operand(indices)?;
                let (b, _) = self.operand(b)?;
                let width: i64 = match name.split('.').nth(3) {
                    Some("qi") => 1,
                    Some("hi") => 2,
                    Some("d" | "ps") => 4,
                    Some("q" | "pd") => 8,
                    _ => unsupported!("LLVM intrinsic `{name}`"),
                };
                let lanes = self.layout(ty)?.size.bytes() as i64 / width;
                let int = Type::int(width as u16 * 8).expect("a lane of 1 to 8 bytes");
                // Each lane is the one its index names of `a` then `b`, by its low bits.
                let values: LaneValues = (0..lanes)
                    .map(|lane| {
                        let index =
                            self.builder.ins().load(int, MEM, indices, (lane * width) as i32);
                        let index = self.builder.ins().band_imm(index, 2 * lanes - 1);
                        let index = if width < 8 {
                            self.builder.ins().uextend(self.cg.target.pointer(), index)
                        } else {
                            index
                        };
                        let second = self.builder.ins().icmp_imm(
                            IntCC::UnsignedGreaterThanOrEqual,
                            index,
                            lanes,
                        );
                        let base = self.builder.ins().select(second, b, a);
                        let slot = self.builder.ins().band_imm(index, lanes - 1);
                        let offset = self.builder.ins().imul_imm(slot, width);
                        let addr = self.builder.ins().iadd(base, offset);
                        self.builder.ins().load(int, MEM, addr, 0)
                    })
                    .collect(); // alloc: none, inline up to 64 lanes
                for (lane, value) in values.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, lane as i32 * width as i32);
                }
            }
            ("x86.sse41.phminposuw", [words]) => {
                let (words, _) = self.operand(words)?;
                // The least of eight unsigned words, then the first lane that holds it, then zeros.
                let mut least = self.builder.ins().load(types::I16, MEM, words, 0);
                let mut at = self.builder.ins().iconst(types::I16, 0);
                for lane in 1..8 {
                    let word = self.builder.ins().load(types::I16, MEM, words, lane * 2);
                    let less = self.builder.ins().icmp(IntCC::UnsignedLessThan, word, least);
                    let index = self.builder.ins().iconst(types::I16, i64::from(lane));
                    least = self.builder.ins().select(less, word, least);
                    at = self.builder.ins().select(less, index, at);
                }
                let zero = self.builder.ins().iconst(types::I16, 0);
                self.builder.ins().store(MEM, least, dst, 0);
                self.builder.ins().store(MEM, at, dst, 2);
                for lane in 2..8 {
                    self.builder.ins().store(MEM, zero, dst, lane * 2);
                }
            }
            // Clears the upper halves of the vector registers, in which no value is kept from
            // one of blaze's instructions to the next.
            ("x86.avx.vzeroupper", []) => {}
            // A hint to the cache, which the program behaves the same without.
            ("prefetch.p0", [_address, _write, _locality, _cache]) => {}
            _ if self.cg.target.arch == Arch::Aarch64
                && let Some(lowered) = neon::lower(name) =>
            {
                // alloc: the name its refusals give; `neon_call` takes it whole, also for a body
                self.neon_assembly(&format!("llvm.{name}"), lowered, args, dst, dst_ty)?
            }
            _ if self.cg.target.arch == Arch::X86_64
                && [
                    "x86.avx512.",
                    "x86.avx512fp16.",
                    "x86.avx512bf16.",
                    "x86.avx2.vpdp",
                    "x86.avx2.gather.",
                    "x86.vcvtne",
                    "x86.vbcstne",
                ]
                .iter()
                .any(|it| name.starts_with(it))
                && self.avx512_assembly(name, args, dst, dst_ty)? => {}
            _ if self.cg.target.arch == Arch::X86_64
                && let Some(lowered) = x86::lower(name) =>
            {
                self.x86_assembly(name, &lowered, args, dst, dst_ty)?
            }
            _ => unsupported!("LLVM intrinsic `{name}`"),
        }
        Ok(())
    }

    /// The lanes of a vector type for an operation that only moves them, which moves the bits
    /// of a lane of `f16` as an integer of 16 bits.
    fn simd_moved_lanes(&self, ty: Ty<'db>) -> Result<(AbiScalar, u64)> {
        if let BackendRepr::SimdVector { element, count } = self.layout(ty)?.backend_repr
            && let Primitive::Float(rustc_abi::Float::F16) = element.primitive()
        {
            return Ok((AbiScalar { ty: types::I16, signed: false }, count.as_u64()));
        }
        self.simd_lanes(ty)
    }

    /// Whether the first of `args` or the result is a vector of `f16`.
    fn half_lanes(&mut self, args: &[Operand], dst_ty: Ty<'db>) -> Result<bool> {
        let first = match args.first() {
            Some(arg) => Some(self.operand_ty(arg)?),
            None => None,
        };
        for ty in first.into_iter().chain([dst_ty]) {
            if let BackendRepr::SimdVector { element, .. } = self.layout(ty)?.backend_repr
                && let Primitive::Float(rustc_abi::Float::F16) = element.primitive()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The lanes of a vector type as `neon` names them, and whether its integers are signed.
    fn neon_lanes_of(&self, ty: Ty<'db>) -> Result<(neon::Lanes, bool)> {
        let BackendRepr::SimdVector { element, count } = self.layout(ty)?.backend_repr else {
            unsupported!("vector operation on {ty:?}");
        };
        let count = count.as_u64() as u32;
        Ok(match element.primitive() {
            Primitive::Float(float) => {
                (neon::Lanes::floats(float.size().bits() as u32, count), true)
            }
            Primitive::Int(int, signed) => {
                (neon::Lanes { count, bits: int.size().bits() as u32, float: false }, signed)
            }
            _ => unsupported!("vector operation on {ty:?}"),
        })
    }

    /// A generic intrinsic on vectors of `f16`, as [`neon::half_vector`] has it, in a function
    /// with the half-precision instructions; elsewhere LLVM works on the lanes as `f32`s, but
    /// for the conversions between the two, which are the same instructions there.
    fn half_vector(
        &mut self,
        op: SimdOp,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        let intrinsic = Intrinsic::Simd(op);
        let Some(first) = args.first() else {
            unsupported!("`{intrinsic}` without operands");
        };
        let ty = self.operand_ty(first)?;
        let (a, a_signed) = self.neon_lanes_of(ty)?;
        let (r, r_signed) = self.neon_lanes_of(dst_ty)?;
        let widths = (r.float && a.float).then_some((r.bits, a.bits));
        if self.cg.target.arch == Arch::X86_64 {
            return self.x86_half_vector(op, args, dst, (a, r));
        }
        let single_and_half = op == SimdOp::Cast && matches!(widths, Some((16, 32) | (32, 16)));
        if self.cg.target.arch != Arch::Aarch64 || !(single_and_half || self.half_precision()) {
            unsupported!(
                "vector operation on lanes of f16 without the half-precision instructions"
            );
        }
        let signed = if a.float { r_signed } else { a_signed };
        let Some(lowered) = neon::half_vector(op, r, a, signed) else {
            unsupported!("`{intrinsic}` of lanes of f16 into {dst_ty:?}");
        };
        self.neon_assembly(intrinsic.symbol().as_str(), lowered, args, dst, dst_ty)
    }

    /// A generic intrinsic on a vector of `f16` on x86-64 (`a` its operand's lanes, `r` its
    /// result's), as LLVM selects it in the function: by AVX512-FP16's instructions where the
    /// function has those, `simd_cast` to single floats by F16C's where it has only those.
    /// Each instruction is `avx512::half_vector`'s.
    fn x86_half_vector(
        &mut self,
        op: SimdOp,
        args: &[Operand],
        dst: Value,
        (a, r): (neon::Lanes, neon::Lanes),
    ) -> Result<()> {
        if op == SimdOp::Neg
            && self.function_enables(hir_ty::TargetFeature::AVX512FP16)
            && a.float
            && a.bits == 16
        {
            // `vpxor` with the sign of each lane, LLVM's `fneg`; one an fma folds is
            // `Self::x86_fma`'s.
            let [arg] = args else { unsupported!("`simd_neg` with {} arguments", args.len()) };
            let (src, _) = self.operand(arg)?;
            for i in 0..a.count as i32 {
                let bits = self.builder.ins().load(types::I16, MEM, src, 2 * i);
                let flipped = self.builder.ins().bxor_imm(bits, 0x8000);
                self.builder.ins().store(MEM, flipped, dst, 2 * i);
            }
            return Ok(());
        }
        let lowered = avx512::half_vector(op, a.count * a.bits / 8, r.count * r.bits / 8, |it| {
            self.function_enables(it)
        });
        let Some(lowered) = lowered.filter(|it| it.loads.len() == args.len()) else {
            unsupported!(
                "`{}` of lanes of f16 on x86-64 without an instruction for it here",
                Intrinsic::Simd(op)
            );
        };
        self.call_lowered(&lowered, args, dst)
    }

    /// The lane type and lane count of a vector type. Lanes sit in memory in order.
    fn simd_lanes(&self, ty: Ty<'db>) -> Result<(AbiScalar, u64)> {
        match self.layout(ty)?.backend_repr {
            BackendRepr::SimdVector { element, count } => {
                let lane = self.scalar(element)?;
                if f16_f128::soft(lane.ty) {
                    unsupported!("vector operation on lanes of {}", lane.ty);
                }
                Ok((lane, count.as_u64()))
            }
            _ => unsupported!("vector operation on {ty:?}"),
        }
    }

    /// Calls a function of compiler-builtins, which are C's by convention.
    fn builtin(&mut self, name: &str, params: &[Type], ret: Type, args: &[Value]) -> Result<Value> {
        // 64-bit Windows takes an integer of 128 bits by reference and returns it as a
        // vector.
        let windows = self.cg.target.call_conv == isa::CallConv::WindowsFastcall;
        let wide = |ty: Type| windows && ty == types::I128;
        let mut sig = self.cg.make_signature();
        sig.params.extend(
            params
                .iter()
                .map(|&ty| AbiParam::new(if wide(ty) { self.cg.target.pointer() } else { ty })),
        );
        sig.returns.push(AbiParam::new(if wide(ret) { types::I64X2 } else { ret }));
        let callee = self.cg.import(name, &sig)?;
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        let mut passed = Vec::with_capacity(args.len());
        for (&value, &ty) in args.iter().zip(params) {
            passed.push(if wide(ty) {
                let slot = self.scratch_wide();
                self.builder.ins().store(MEM, value, slot, 0);
                slot
            } else {
                value
            });
        }
        let value = self.invoke(callee, &passed)[0];
        if !wide(ret) {
            return Ok(value);
        }
        let slot = self.scratch_wide();
        self.builder.ins().store(MEM, value, slot, 0);
        Ok(self.builder.ins().load(types::I128, MEM, slot, 0))
    }

    /// Whether an atomic access of `ty` is made of compare-and-swaps of compiler_builtins:
    /// Cranelift has no atomics of 128 bits on AArch64. That is how LLVM makes them at `-O0`
    /// on a target with outline atomics (`aarch64-unknown-linux-gnu`): it expands a load, a
    /// store and a read-modify-write to compare-and-swaps (`AtomicExpansionKind::CmpXChg`),
    /// and a compare-and-swap is a call of `__aarch64_cas16_acq_rel` for one sequentially
    /// consistent, which takes CASP when the CPU has LSE and LDXP/STXP when not. Without
    /// outline atomics there is no such function to call, and the access is refused.
    fn wide_atomics(&self, ty: Type) -> Result<bool> {
        if ty != types::I128 || self.cg.target.arch != Arch::Aarch64 {
            return Ok(false);
        }
        let features = hir_ty::crate_target_features(self.cg.db, self.env.krate);
        if !features.contains(hir_ty::TargetFeature::OUTLINE_ATOMICS) {
            unsupported!("an atomic of 128 bits on AArch64 without outline atomics");
        }
        Ok(true)
    }

    /// What `ptr` held, swapped for `new` when it was `expected`: compiler_builtins'
    /// compare-and-swap of 128 bits ([`Self::wide_atomics`]).
    fn cas16(&mut self, ptr: Value, expected: Value, new: Value) -> Result<Value> {
        let params = [types::I128, types::I128, self.cg.target.pointer()];
        self.builtin("__aarch64_cas16_acq_rel", &params, types::I128, &[expected, new, ptr])
    }

    /// `op` of 128 bits at `ptr` with `value`, and what was there before: LLVM's expansion
    /// of an `atomicrmw` to a loop of compare-and-swaps, from a plain load of what is there.
    fn rmw16(&mut self, op: AtomicRmwOp, ptr: Value, value: Value) -> Result<Value> {
        let first = self.builder.ins().load(types::I128, MEM, ptr, 0);
        let again = self.builder.create_block();
        let done = self.builder.create_block();
        let seen = self.builder.append_block_param(again, types::I128);
        let old = self.builder.append_block_param(done, types::I128);
        self.builder.ins().jump(again, &[first.into()]);
        self.builder.switch_to_block(again);
        let ins = self.builder.ins();
        let new = match op {
            AtomicRmwOp::Xchg => value,
            AtomicRmwOp::Add => ins.iadd(seen, value),
            AtomicRmwOp::Sub => ins.isub(seen, value),
            AtomicRmwOp::And => ins.band(seen, value),
            AtomicRmwOp::Nand => {
                let and = ins.band(seen, value);
                self.builder.ins().bnot(and)
            }
            AtomicRmwOp::Or => ins.bor(seen, value),
            AtomicRmwOp::Xor => ins.bxor(seen, value),
            AtomicRmwOp::Smax | AtomicRmwOp::Smin | AtomicRmwOp::Umax | AtomicRmwOp::Umin => {
                let cc = match op {
                    AtomicRmwOp::Smax => IntCC::SignedGreaterThan,
                    AtomicRmwOp::Smin => IntCC::SignedLessThanOrEqual,
                    AtomicRmwOp::Umax => IntCC::UnsignedGreaterThan,
                    _ => IntCC::UnsignedLessThanOrEqual,
                };
                let keep = ins.icmp(cc, seen, value);
                self.builder.ins().select(keep, seen, value)
            }
        };
        let was = self.cas16(ptr, seen, new)?;
        let swapped = self.builder.ins().icmp(IntCC::Equal, was, seen);
        self.builder.ins().brif(swapped, done, &[was.into()], again, &[was.into()]);
        self.builder.switch_to_block(done);
        Ok(old)
    }

    /// The quotient of two integers, or the remainder. Cranelift has neither for 128 bits, so
    /// those call compiler-builtins.
    fn divide(&mut self, l: Value, r: Value, div: bool, signed: bool) -> Result<Value> {
        if self.builder.func.dfg.value_type(l) == types::I128 {
            let mut sig = self.cg.make_signature();
            sig.params.extend([AbiParam::new(types::I128), AbiParam::new(types::I128)]);
            sig.returns.push(AbiParam::new(types::I128));
            let name = match (div, signed) {
                (true, true) => "__divti3",
                (true, false) => "__udivti3",
                (false, true) => "__modti3",
                (false, false) => "__umodti3",
            };
            return self.builtin(name, &[types::I128; 2], types::I128, &[l, r]);
        }
        let ins = self.builder.ins();
        Ok(match (div, signed) {
            (true, true) => ins.sdiv(l, r),
            (true, false) => ins.udiv(l, r),
            (false, true) => ins.srem(l, r),
            (false, false) => ins.urem(l, r),
        })
    }

    /// Cranelift has no 128-bit multiply that reports overflow, so this calls the one in
    /// `compiler_builtins`.
    fn wide_mul_overflow(&mut self, l: Value, r: Value, signed: bool) -> Result<(Value, Value)> {
        let name = if signed { "__rust_i128_mulo" } else { "__rust_u128_mulo" };
        let overflow = self.scratch();
        let params = [types::I128, types::I128, self.cg.target.pointer()];
        let value = self.builtin(name, &params, types::I128, &[l, r, overflow])?;
        let overflow = self.builder.ins().load(types::I32, MEM, overflow, 0);
        let overflow = self.builder.ins().icmp_imm(IntCC::NotEqual, overflow, 0);
        Ok((value, overflow))
    }

    fn memcmp(&mut self, a: Value, b: Value, len: Value) -> Result<Value> {
        let mut sig = self.cg.make_signature();
        sig.params.extend([AbiParam::new(self.cg.target.pointer()); 3]);
        sig.returns.push(AbiParam::new(types::I32));
        let memcmp = self.cg.import("memcmp", &sig)?;
        let memcmp = self.cg.declare_func_in_func(memcmp, self.builder.func);
        let call = self.builder.ins().call(memcmp, &[a, b, len]);
        Ok(self.builder.inst_results(call)[0])
    }

    /// A vector operation of two operands on lanes that are floats, one lane at a time. A
    /// comparison is all ones in the lanes where it holds, in integers as wide as the floats.
    fn simd_float_lanes(
        &mut self,
        op: SimdOp,
        (l, r): (Value, Value),
        dst: Value,
        lane: AbiScalar,
        out: AbiScalar,
        count: u64,
    ) -> Result<()> {
        let cc = match op {
            SimdOp::Eq => Some(FloatCC::Equal),
            SimdOp::Ne => Some(FloatCC::NotEqual),
            SimdOp::Lt => Some(FloatCC::LessThan),
            SimdOp::Le => Some(FloatCC::LessThanOrEqual),
            SimdOp::Gt => Some(FloatCC::GreaterThan),
            SimdOp::Ge => Some(FloatCC::GreaterThanOrEqual),
            _ => None,
        };
        for i in 0..count {
            let offset = (i * lane.ty.bytes() as u64) as i32;
            let a = self.builder.ins().load(lane.ty, MEM, l, offset);
            let b = self.builder.ins().load(lane.ty, MEM, r, offset);
            let value = match (cc, op) {
                (Some(cc), _) => {
                    let flag = self.builder.ins().fcmp(cc, a, b);
                    let flag = self.builder.ins().uextend(types::I64, flag);
                    let mask = self.builder.ins().ineg(flag);
                    if out.ty.bits() < 64 { self.builder.ins().ireduce(out.ty, mask) } else { mask }
                }
                (None, SimdOp::Add) => self.builder.ins().fadd(a, b),
                (None, SimdOp::Sub) => self.builder.ins().fsub(a, b),
                (None, SimdOp::Mul) => self.builder.ins().fmul(a, b),
                (None, SimdOp::Div) => self.builder.ins().fdiv(a, b),
                (None, _) => unsupported!("`{}` of floats", Intrinsic::Simd(op)),
            };
            self.builder.ins().store(MEM, value, dst, (i * out.ty.bytes() as u64) as i32);
        }
        Ok(())
    }

    fn saturating(&mut self, add: bool, l: Value, r: Value, scalar: AbiScalar) -> Value {
        let bits = scalar.ty.bits();
        let ins = self.builder.ins();
        let (value, overflow) = match (add, scalar.signed) {
            (true, true) => ins.sadd_overflow(l, r),
            (true, false) => ins.uadd_overflow(l, r),
            (false, true) => ins.ssub_overflow(l, r),
            (false, false) => ins.usub_overflow(l, r),
        };
        let limit = if scalar.signed {
            // Overflow went past MAX when the left operand is non-negative, past MIN otherwise.
            let min = self.iconst(scalar.ty, 1u128 << (bits - 1));
            let max = self.iconst(scalar.ty, (1u128 << (bits - 1)) - 1);
            let negative = self.builder.ins().icmp_imm(IntCC::SignedLessThan, l, 0);
            self.builder.ins().select(negative, min, max)
        } else if add {
            self.iconst(scalar.ty, u128::MAX >> (128 - bits))
        } else {
            self.iconst(scalar.ty, 0)
        };
        self.builder.ins().select(overflow, limit, value)
    }

    fn int_cast(&mut self, value: Value, from: AbiScalar, to: AbiScalar) -> Value {
        if to.ty.bits() < from.ty.bits() {
            self.builder.ins().ireduce(to.ty, value)
        } else if to.ty.bits() == from.ty.bits() {
            value
        } else if from.signed {
            self.builder.ins().sextend(to.ty, value)
        } else {
            self.builder.ins().uextend(to.ty, value)
        }
    }

    /// Calls a function that may unwind. When this function has values to drop, the call gets a
    /// landing pad that drops them and resumes unwinding.
    fn invoke(&mut self, callee: ir::FuncRef, args: &[Value]) -> Vec<Value> {
        let Some(pad) = self.landing_pad_for_call() else {
            let call = self.builder.ins().call(callee, args);
            // alloc: callers take the results as a `Vec`
            return self.builder.inst_results(call).to_vec();
        };
        if !self.cg.target.exception_in_register() {
            return self.guarded_call(pad, |builder| builder.ins().call(callee, args));
        }
        let sig = self.builder.func.dfg.ext_funcs[callee].signature;
        let (table, after) = self.exception_table(sig, pad);
        self.builder.ins().try_call(callee, args, table);
        self.builder.switch_to_block(after);
        self.builder.block_params(after).to_vec() // alloc: callers take the results as a `Vec`
    }

    fn invoke_indirect(&mut self, sig: ir::SigRef, pointer: Value, args: &[Value]) -> Vec<Value> {
        let Some(pad) = self.landing_pad_for_call() else {
            let call = self.builder.ins().call_indirect(sig, pointer, args);
            // alloc: callers take the results as a `Vec`
            return self.builder.inst_results(call).to_vec();
        };
        if !self.cg.target.exception_in_register() {
            return self
                .guarded_call(pad, |builder| builder.ins().call_indirect(sig, pointer, args));
        }
        let (table, after) = self.exception_table(sig, pad);
        self.builder.ins().try_call_indirect(pointer, args, table);
        self.builder.switch_to_block(after);
        self.builder.block_params(after).to_vec() // alloc: callers take the results as a `Vec`
    }

    /// Turns down a call that can unwind in code that cannot: rustc ends the program when
    /// it does (`AbortUnwindingCalls`).
    fn check_abort_unwinding(&self, abi: ExternAbi) -> Result<()> {
        if self.aborts && abi.as_str().ends_with("-unwind") {
            unsupported!("a call of an `extern \"{}\"` function with `panic = \"abort\"`", abi);
        }
        Ok(())
    }

    /// The block a call unwinds to: the one that cleans up after it, or in an `async` body
    /// with nothing to clean up the one that marks the body as panicked. None when unwinding
    /// just goes on.
    fn landing_pad_for_call(&mut self) -> Option<ir::Block> {
        if self.in_cleanup || self.aborts {
            return None;
        }
        if self.inner_pad.is_some() {
            return self.inner_pad;
        }
        let Some(target) = self.unwind_to else {
            // rustc's coroutine transform has every call that unwinds out of an `async` body
            // unwind to a block that poisons it, so that polling it again panics.
            self.suspended.as_ref()?;
            return Some(*self.poison.get_or_insert_with(|| {
                let pad = self.builder.create_block();
                self.builder.append_block_param(pad, self.cg.target.pointer());
                self.builder.set_cold_block(pad);
                pad
            }));
        };
        if let Some(&(_, pad)) = self.pads.iter().find(|(it, _)| *it == target) {
            return Some(pad);
        }
        let pad = self.builder.create_block();
        self.builder.append_block_param(pad, self.cg.target.pointer());
        self.builder.set_cold_block(pad);
        self.pads.push((target, pad));
        Some(pad)
    }

    fn exception_table(
        &mut self,
        sig: ir::SigRef,
        cleanup: ir::Block,
    ) -> (ir::ExceptionTable, ir::Block) {
        let after = self.builder.create_block();
        let returns = &self.builder.func.dfg.signatures[sig].returns;
        // alloc: none, inline up to 4 results
        let returns: smallvec::SmallVec<[_; 4]> = returns.iter().map(|it| it.value_type).collect();
        let mut results = smallvec::SmallVec::<[ir::BlockArg; 4]>::with_capacity(returns.len());
        for (i, &ty) in returns.iter().enumerate() {
            self.builder.append_block_param(after, ty);
            results.push(ir::BlockArg::TryCallRet(i as u32));
        }
        let normal = self.builder.func.dfg.block_call(after, &results);
        let unwound = self.builder.func.dfg.block_call(cleanup, &[ir::BlockArg::TryCallExn(0)]);
        let tag = ir::ExceptionTag::with_number(unwind::CLEANUP).expect("a small tag");
        let table = self.builder.func.dfg.exception_tables.push(ir::ExceptionTableData::new(
            sig,
            normal,
            [ir::ExceptionTableItem::Tag(tag, unwound)],
        ));
        (table, after)
    }

    /// Makes a call that goes to `pad` when it unwinds, where a call cannot name its pad:
    /// the unwinder has the call return with the exception in a slot of the frame that is
    /// empty otherwise, and the call is followed by a look at it.
    fn guarded_call(
        &mut self,
        pad: ir::Block,
        call: impl FnOnce(&mut FunctionBuilder<'_>) -> ir::Inst,
    ) -> Vec<Value> {
        self.guarded = true;
        let loc = self.cg.source_loc(self.span);
        self.builder.set_srcloc(unwind::seh::guarded(loc));
        let call = call(&mut self.builder);
        self.at(self.span);
        let results = self.builder.inst_results(call).to_vec(); // alloc: returned as a `Vec`
        let frame = self.builder.ins().get_frame_pointer(self.cg.target.pointer());
        let exception =
            self.builder.ins().load(self.cg.target.pointer(), MEM, frame, unwind::seh::ARRIVAL);
        let (unwound, returned) = (self.builder.create_block(), self.builder.create_block());
        self.builder.set_cold_block(unwound);
        self.builder.ins().brif(exception, unwound, &[], returned, &[]);
        self.builder.switch_to_block(unwound);
        let none = self.builder.ins().iconst(self.cg.target.pointer(), 0);
        self.builder.ins().store(MEM, none, frame, unwind::seh::ARRIVAL);
        self.builder.ins().jump(pad, &[exception.into()]);
        self.builder.switch_to_block(returned);
        results
    }

    /// Fills in the landing pads of a body in `MirPhase::Runtime`: each keeps the exception
    /// and goes on to its cleanup block, which ends in `UnwindResume`.
    fn emit_pads(&mut self) -> Result<()> {
        for (target, pad) in std::mem::replace(&mut self.pads, Vec::new_in(self.frame)) {
            self.builder.switch_to_block(pad);
            let exception = self.builder.block_params(pad)[0];
            let slot = self.exception_slot();
            self.builder.ins().stack_store(self.cg.target.pointer(), exception, slot, 0);
            self.builder.ins().jump(self.blocks[target], &[]);
        }
        if let (Some(pad), Some(state)) = (self.poison.take(), &self.suspended) {
            let (addr, ty) = (state.addr, state.ty);
            self.builder.switch_to_block(pad);
            let exception = self.builder.block_params(pad)[0];
            self.store_state(addr, ty, PANICKED);
            self.resume_unwinding(exception)?;
        }
        Ok(())
    }

    fn exception_slot(&mut self) -> ir::StackSlot {
        *self.exception.get_or_insert_with(|| {
            self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                3,
            ))
        })
    }

    fn store_state(&mut self, state: Value, ty: Type, value: i64) {
        let value = self.builder.ins().iconst(ty, value);
        self.builder.ins().store(MEM, value, state, 0);
    }

    /// Returns what is in the local that holds the function's result.
    fn return_(&mut self) -> Result<()> {
        let body = self.body.expect("MIR body");
        let local = returned_local(body);
        let mut values = Vec::with_capacity(self.builder.func.signature.returns.len());
        if !matches!(self.ret_mode, PassMode::Indirect) {
            let layout = self.layout(body.locals[local].ty.as_ref())?;
            let addr = self.slot_addr(self.locals[local].addr);
            self.pass(self.ret_mode, addr, &layout, &mut values);
        }
        self.builder.ins().return_(&values);
        Ok(())
    }

    /// The way into an `async` body. `poll` carries on from where the body last returned
    /// `Poll::Pending`. A null context instead asks for the future to be dropped: what it
    /// captured if it never ran, and what is in scope where it yielded if it is suspended,
    /// which the yield's drop path in the MIR does.
    fn enter_coroutine(
        &mut self,
        body: &MirBody<'db>,
        state: &SuspendedState,
        addr: Value,
        ty: Ty<'db>,
        layout: &Layout,
    ) -> Result<()> {
        let Some(hir_ty_mir::mir::CoroutineLocals { context, span, .. }) = body.coroutine else {
            unsupported!("async body without a context");
        };
        let context =
            self.builder.ins().load(self.cg.target.pointer(), MEM, self.locals[context].addr, 0);
        let at = self.builder.ins().load(state.ty, MEM, state.addr, 0);
        let at = match state.ty {
            types::I64 => at,
            _ => self.builder.ins().uextend(types::I64, at),
        };
        let (poll, drop) = (self.builder.create_block(), self.builder.create_block());
        self.builder.ins().brif(context, poll, &[], drop, &[]);

        let finished = self.builder.create_block();
        self.builder.switch_to_block(poll);
        let mut switch = Switch::new();
        switch.set_entry(UNRESUMED as u128, self.blocks[body.start_block]);
        for (index, point) in state.held.yields.iter().enumerate() {
            switch.set_entry(SUSPENDED as u128 + index as u128, self.blocks[point.resume]);
        }
        switch.emit(&mut self.builder, at, finished);
        self.builder.switch_to_block(finished);
        self.resumed_after_end(span, at)?;

        let (unresumed, done) = (self.builder.create_block(), self.builder.create_block());
        // Dropping is laid out behind the body, so that the code a function begins with is
        // that of its first statement and not of the drops of a future given up half-way.
        for block in [drop, unresumed] {
            self.builder.set_cold_block(block);
        }
        self.builder.switch_to_block(drop);
        self.store_state(state.addr, state.ty, RETURNED);
        let mut dropping = Switch::new();
        dropping.set_entry(UNRESUMED as u128, unresumed);
        for (index, point) in state.held.yields.iter().enumerate() {
            let Some(dropped) = point.drop else {
                unsupported!("a yield without a drop path");
            };
            dropping.set_entry(SUSPENDED as u128 + index as u128, self.blocks[dropped]);
        }
        dropping.emit(&mut self.builder, at, done);
        self.in_cleanup = true;
        self.builder.switch_to_block(unresumed);
        self.at(Self::returns_at(body));
        let TyKind::Coroutine(_, args) = ty.kind() else {
            unsupported!("async body of {ty:?}");
        };
        for (i, field) in args.as_coroutine().tupled_upvars_ty().tuple_fields().iter().enumerate() {
            let field_addr =
                self.builder.ins().iadd_imm(addr, layout.fields.offset(i).bytes() as i64);
            self.drop_value(field_addr, None, field)?;
        }
        self.builder.ins().jump(done, &[]);
        self.in_cleanup = false;
        self.builder.switch_to_block(done);
        self.return_()
    }

    /// Polling a future again after it returned or panicked panics at its body, as rustc's
    /// coroutine transform has it. `state` is one of the two: the others resume the body.
    fn resumed_after_end(&mut self, span: MirSpan, state: Value) -> Result<()> {
        self.at(span);
        let (returned, panicked) = (self.builder.create_block(), self.builder.create_block());
        let ended = self.builder.ins().icmp_imm(IntCC::Equal, state, RETURNED);
        self.builder.ins().brif(ended, returned, &[], panicked, &[]);
        // rustc's panic here unwinds straight out (`UnwindAction::Continue`): it does not
        // poison the state, so a future polled after it returned says so on every poll.
        self.in_cleanup = true;
        for (block, name) in [
            (returned, "panic_const_async_fn_resumed"),
            (panicked, "panic_const_async_fn_resumed_panic"),
        ] {
            self.builder.set_cold_block(block);
            self.builder.switch_to_block(block);
            let location = self.caller_location()?;
            self.call_panic(&["panicking", "panic_const", name], &[location])?;
            self.builder.ins().trap(TrapCode::user(1).expect("non-zero trap code"));
        }
        self.in_cleanup = false;
        Ok(())
    }

    /// Where the body is written to end, which is where rustc has an `async` block dropped.
    fn returns_at(body: &MirBody<'db>) -> MirSpan {
        let returns = body.basic_blocks.iter().find_map(|(_, block)| match &block.terminator {
            Some(hir_ty_mir::mir::Terminator { kind: TerminatorKind::Return, span }) => Some(*span),
            _ => None,
        });
        returns.unwrap_or(MirSpan::Unknown)
    }

    /// Drops the `async` block at `addr`, by polling it with no context.
    fn drop_coroutine(&mut self, addr: Value, ty: Ty<'db>) -> Result<()> {
        let Some((def, parent_args)) = coroutine_instance(self.cg.db, ty) else {
            unsupported!("drop glue for {ty:?}");
        };
        let body = self.body_types(def, parent_args)?;
        let Some(poll_ty) = &body.poll else {
            unsupported!("async block whose body is not one");
        };
        let mut values = smallvec::SmallVec::<[(Value, Ty<'db>); 2]>::new();
        for (param, value) in body.params.iter().zip([Some(addr), None]) {
            let slot = self.scratch();
            let value =
                value.unwrap_or_else(|| self.builder.ins().iconst(self.cg.target.pointer(), 0));
            self.builder.ins().store(MEM, value, slot, 0);
            values.push((slot, param.as_ref()));
        }
        let poll_ty = poll_ty.as_ref();
        let layout = self.layout(poll_ty)?;
        let ignored = self.stack_slot(&layout);
        self.emit_call(Callee::Instance(def, parent_args), &values, ignored, poll_ty)
    }

    fn resume_unwinding(&mut self, exception: Value) -> Result<()> {
        let mut sig = self.cg.make_signature();
        sig.params.push(AbiParam::new(self.cg.target.pointer()));
        let resume = if self.cg.target.exception_in_register() {
            "_Unwind_Resume"
        } else {
            unwind::seh::RESUME
        };
        let resume = self.cg.import(resume, &sig)?;
        let resume = self.cg.declare_func_in_func(resume, self.builder.func);
        self.builder.ins().call(resume, &[exception]);
        self.builder.ins().trap(ir::TrapCode::user(1).expect("a nonzero trap code"));
        Ok(())
    }

    /// Runs a type's `Drop` impl. If that unwinds the fields are dropped all the same, on the
    /// way out.
    fn call_drop_impl_guarded(
        &mut self,
        addr: Value,
        meta: Option<Value>,
        ty: Ty<'db>,
    ) -> Result<()> {
        self.guarded(
            |this| this.call_drop_impl(addr, meta, ty),
            |this| this.drop_adt_fields(addr, meta, ty),
        )
    }

    /// Runs `body`, and `cleanup` on the way out if a call that `body` makes unwinds.
    fn guarded(
        &mut self,
        body: impl FnOnce(&mut Self) -> Result<()>,
        cleanup: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<()> {
        if self.in_cleanup || self.aborts {
            return body(self);
        }
        let outer = self.landing_pad_for_call();
        let pad = self.builder.create_block();
        self.builder.append_block_param(pad, self.cg.target.pointer());
        self.builder.set_cold_block(pad);
        let enclosing = self.inner_pad.replace(pad);
        let done = body(self);
        self.inner_pad = enclosing;
        done?;
        if self.builder.current_block().is_none() {
            unsupported!("guarded call without a block to return to");
        }
        // Cranelift wants a block finished before it switches away from it: this one ends here,
        // the code after the call goes on in `after`.
        let after = self.builder.create_block();
        self.builder.ins().jump(after, &[]);
        self.builder.switch_to_block(pad);
        let exception = self.builder.block_params(pad)[0];
        self.in_cleanup = true;
        let cleaned = cleanup(self);
        self.in_cleanup = false;
        cleaned?;
        match outer {
            Some(outer) => {
                self.builder.ins().jump(outer, &[exception.into()]);
            }
            None => self.resume_unwinding(exception)?,
        }
        self.builder.switch_to_block(after);
        Ok(())
    }

    fn drop_value(&mut self, addr: Value, meta: Option<Value>, ty: Ty<'db>) -> Result<()> {
        let ty = self.normalize(ty);
        if self.glue_of(ty) == DropGlue::None {
            return Ok(());
        }
        let mut sig = self.cg.make_signature();
        sig.params.push(AbiParam::new(self.cg.target.pointer()));
        let mut values = smallvec::SmallVec::<[Value; 2]>::new();
        values.push(addr);
        if self.is_unsized(ty) {
            let Some(meta) = meta else {
                unsupported!("drop of unsized {ty:?} without metadata");
            };
            sig.params.push(AbiParam::new(self.cg.target.pointer()));
            values.push(meta);
        }
        let args = GenericArgs::new_from_slice(&[ty.into()]);
        let callee = self.cg.callee(InstanceDef::DropGlue, args, self.env, &sig)?;
        self.show_callee(InstanceDef::DropGlue, args, &sig, callee);
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        self.invoke(callee, &values);
        Ok(())
    }

    /// The body of `drop_in_place::<ty>`.
    fn drop_glue(&mut self, ty: Ty<'db>) -> Result<()> {
        self.in_drop_glue = true;
        let mut sig = self.cg.make_signature();
        sig.params.push(AbiParam::new(self.cg.target.pointer()));
        if self.is_unsized(ty) {
            sig.params.push(AbiParam::new(self.cg.target.pointer()));
        }
        self.builder.func.signature = sig;
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let params = self.builder.block_params(entry);
        let (addr, meta) = (params[0], params.get(1).copied());
        self.drop_contents(addr, meta, ty)?;
        self.builder.ins().return_(&[]);
        Ok(())
    }

    /// Drops the fields of a struct or of the variant an enum is in.
    /// Jumps to `differ` unless the values at `l` and `r` are the same value of `ty`, compared
    /// the way a constant pattern is: field by field and through references. `metas` are the
    /// lengths of the two when `ty` is unsized.
    fn branch_unless_equal(
        &mut self,
        l: Value,
        r: Value,
        metas: Option<(Value, Value)>,
        ty: Ty<'db>,
        differ: ir::Block,
    ) -> Result<()> {
        let db = self.cg.db;
        let ty = self.normalize(ty);
        match ty.kind() {
            TyKind::Ref(_, pointee, _) => {
                let metas = self.is_unsized(pointee).then(|| {
                    (
                        self.builder.ins().load(
                            self.cg.target.pointer(),
                            MEM,
                            l,
                            self.cg.target.meta_at(),
                        ),
                        self.builder.ins().load(
                            self.cg.target.pointer(),
                            MEM,
                            r,
                            self.cg.target.meta_at(),
                        ),
                    )
                });
                let l = self.builder.ins().load(self.cg.target.pointer(), MEM, l, 0);
                let r = self.builder.ins().load(self.cg.target.pointer(), MEM, r, 0);
                self.branch_unless_equal(l, r, metas, pointee, differ)
            }
            TyKind::Str | TyKind::Slice(_) | TyKind::Array(..) => {
                let element = match ty.kind() {
                    TyKind::Slice(element) | TyKind::Array(element, _) => element,
                    _ => Ty::new_uint(self.interner, rustc_type_ir::UintTy::U8),
                };
                let len = match (ty.kind(), metas) {
                    (TyKind::Array(_, len), _) => {
                        let Some(len) = try_const_usize(db, len) else {
                            unsupported!("array with unevaluated length");
                        };
                        self.builder.ins().iconst(self.cg.target.pointer(), len as i64)
                    }
                    (_, Some((l_len, r_len))) => {
                        let same = self.builder.ins().icmp(IntCC::Equal, l_len, r_len);
                        self.branch_unless(same, differ);
                        l_len
                    }
                    (_, None) => unsupported!("comparison of a slice without a length"),
                };
                let size = self.layout(element)?.size.bytes() as i64;
                // Elements without padding or references are equal exactly when their bytes
                // are.
                if matches!(
                    element.kind(),
                    TyKind::Int(_) | TyKind::Uint(_) | TyKind::Bool | TyKind::Char
                ) {
                    let bytes = self.builder.ins().imul_imm(len, size);
                    let order = self.memcmp(l, r, bytes)?;
                    let same = self.builder.ins().icmp_imm(IntCC::Equal, order, 0);
                    self.branch_unless(same, differ);
                    return Ok(());
                }
                let header = self.builder.create_block();
                let body = self.builder.create_block();
                let done = self.builder.create_block();
                self.builder.append_block_param(header, self.cg.target.pointer());
                let zero = self.builder.ins().iconst(self.cg.target.pointer(), 0);
                self.builder.ins().jump(header, &[zero.into()]);
                self.builder.switch_to_block(header);
                let index = self.builder.block_params(header)[0];
                let more = self.builder.ins().icmp(IntCC::UnsignedLessThan, index, len);
                self.builder.ins().brif(more, body, &[], done, &[]);
                self.builder.switch_to_block(body);
                let offset = self.builder.ins().imul_imm(index, size);
                let l = self.builder.ins().iadd(l, offset);
                let r = self.builder.ins().iadd(r, offset);
                self.branch_unless_equal(l, r, None, element, differ)?;
                let next = self.builder.ins().iadd_imm(index, 1);
                self.builder.ins().jump(header, &[next.into()]);
                self.builder.switch_to_block(done);
                Ok(())
            }
            TyKind::Tuple(tys) => {
                let layout = self.layout(ty)?;
                for (i, field) in tys.iter().enumerate() {
                    let offset = layout.fields.offset(i).bytes() as i64;
                    let l = self.builder.ins().iadd_imm(l, offset);
                    let r = self.builder.ins().iadd_imm(r, offset);
                    self.branch_unless_equal(l, r, None, field, differ)?;
                }
                Ok(())
            }
            TyKind::Adt(adt_def, subst) => {
                let layout = self.layout(ty)?;
                match adt_def.def_id() {
                    AdtId::UnionId(_) => unsupported!("comparison of a union"),
                    AdtId::StructId(id) => {
                        self.branch_unless_fields_equal(l, r, &layout, id.into(), subst, differ)
                    }
                    AdtId::EnumId(id) => {
                        let wide = match &layout.variants {
                            Variants::Multiple { tag, .. } => DiscriminantWidth::of(tag),
                            Variants::Empty => return Ok(()),
                            Variants::Single { index } => {
                                let variant = id
                                    .enum_variants(db)
                                    .variants
                                    .values()
                                    .map(|&(variant, _)| variant)
                                    .find(|variant| variant.index(db) == index.0);
                                let Some(variant) = variant else { return Ok(()) };
                                return self.branch_unless_fields_equal(
                                    l,
                                    r,
                                    &layout,
                                    variant.into(),
                                    subst,
                                    differ,
                                );
                            }
                        };
                        let Variants::Multiple { .. } = &layout.variants else {
                            return Ok(());
                        };
                        let l_discr = self.discriminant(l, ty, wide.scalar())?;
                        let r_discr = self.discriminant(r, ty, wide.scalar())?;
                        let same = self.builder.ins().icmp(IntCC::Equal, l_discr, r_discr);
                        self.branch_unless(same, differ);
                        let done = self.builder.create_block();
                        let mut switch = Switch::new();
                        let mut arms = Vec::new_in(self.frame);
                        for &(variant, _) in id.enum_variants(db).variants.values() {
                            if db.field_types(variant.into()).iter().next().is_none() {
                                continue;
                            }
                            let value = db.const_eval_discriminant(variant).map_err(|e| {
                                // alloc: the error's message
                                CodegenError::Frontend(format!("discriminant: {e:?}"))
                            })?;
                            let block = self.builder.create_block();
                            switch.set_entry(wide.entry(value), block);
                            arms.push((block, variant));
                        }
                        switch.emit(&mut self.builder, l_discr, done);
                        let mut seen = Vec::with_capacity(4);
                        for (block, variant) in arms {
                            self.builder.switch_to_block(block);
                            // A constant of a pattern is never in such a variant, and here
                            // the two are in the same one.
                            seen.clear();
                            seen.push(ty);
                            if !self.fields_compare_in_parts(variant.into(), subst, &mut seen) {
                                self.builder.ins().jump(differ, &[]);
                                continue;
                            }
                            let index = RustcEnumVariantIdx(variant.index(db));
                            let layout = &Layout::for_variant(&layout, index);
                            self.branch_unless_fields_equal(
                                l,
                                r,
                                layout,
                                variant.into(),
                                subst,
                                differ,
                            )?;
                            self.builder.ins().jump(done, &[]);
                        }
                        self.builder.switch_to_block(done);
                        Ok(())
                    }
                }
            }
            // `NonNull` holds its pointer as a pattern type.
            TyKind::Pat(base, _) => self.branch_unless_equal(l, r, metas, base, differ),
            // A pointer to something unsized is equal in its address and its metadata.
            TyKind::RawPtr(pointee, _) if self.is_unsized(pointee) => {
                for offset in [0, self.cg.target.meta_at()] {
                    let l = self.builder.ins().load(self.cg.target.pointer(), MEM, l, offset);
                    let r = self.builder.ins().load(self.cg.target.pointer(), MEM, r, offset);
                    let same = self.builder.ins().icmp(IntCC::Equal, l, r);
                    self.branch_unless(same, differ);
                }
                Ok(())
            }
            _ => {
                let scalar = self.scalar_of(ty)?;
                let l = self.builder.ins().load(scalar.ty, MEM, l, 0);
                let r = self.builder.ins().load(scalar.ty, MEM, r, 0);
                let same = if f16_f128::soft(scalar.ty) {
                    self.soft_binary_op(&BinOp::Eq, l, r)?
                } else if scalar.ty.is_float() {
                    self.builder.ins().fcmp(FloatCC::Equal, l, r)
                } else {
                    self.builder.ins().icmp(IntCC::Equal, l, r)
                };
                self.branch_unless(same, differ);
                Ok(())
            }
        }
    }

    /// Whether [`Self::branch_unless_equal`] can compare two values of `ty`. `seen` are the
    /// types that hold this one.
    fn compares_in_parts(&self, ty: Ty<'db>, seen: &mut Vec<Ty<'db>>) -> bool {
        let ty = self.normalize(ty);
        if seen.contains(&ty) {
            return true;
        }
        match ty.kind() {
            TyKind::Str => true,
            TyKind::Ref(_, inner, _) | TyKind::Slice(inner) | TyKind::Array(inner, _) => {
                self.compares_in_parts(inner, seen)
            }
            TyKind::Tuple(tys) => tys.iter().all(|ty| self.compares_in_parts(ty, seen)),
            TyKind::Adt(adt_def, subst) => match adt_def.def_id() {
                AdtId::UnionId(_) => false,
                // Each variant is looked at once the two are known to be in it.
                AdtId::EnumId(_) => true,
                AdtId::StructId(id) => {
                    seen.push(ty);
                    let compares = self.fields_compare_in_parts(id.into(), subst, seen);
                    seen.pop();
                    compares
                }
            },
            _ => self.scalar_of(ty).is_ok(),
        }
    }

    fn fields_compare_in_parts(
        &self,
        variant: VariantId,
        subst: GenericArgs<'db>,
        seen: &mut Vec<Ty<'db>>,
    ) -> bool {
        self.cg.db.field_types(variant).iter().all(|(_, ty)| {
            let ty = ty.ty().instantiate(self.interner, subst).skip_norm_wip();
            self.compares_in_parts(ty, seen)
        })
    }

    fn branch_unless_fields_equal(
        &mut self,
        l: Value,
        r: Value,
        layout: &Layout,
        variant: VariantId,
        subst: GenericArgs<'db>,
        differ: ir::Block,
    ) -> Result<()> {
        let field_types = self.cg.db.field_types(variant);
        for (field, ty) in field_types.iter() {
            let ty = ty.ty().instantiate(self.interner, subst).skip_norm_wip();
            let offset = layout.fields.offset(u32::from(field.into_raw()) as usize).bytes() as i64;
            let l = self.builder.ins().iadd_imm(l, offset);
            let r = self.builder.ins().iadd_imm(r, offset);
            self.branch_unless_equal(l, r, None, ty, differ)?;
        }
        Ok(())
    }

    /// Jumps to `otherwise` when `condition` is false, and carries on in a new block.
    fn branch_unless(&mut self, condition: Value, otherwise: ir::Block) {
        let next = self.builder.create_block();
        self.builder.ins().brif(condition, next, &[], otherwise, &[]);
        self.builder.switch_to_block(next);
    }

    fn drop_adt_fields(&mut self, addr: Value, meta: Option<Value>, ty: Ty<'db>) -> Result<()> {
        let db = self.cg.db;
        let TyKind::Adt(adt_def, subst) = ty.kind() else { return Ok(()) };
        {
            {
                let layout = self.layout(ty)?;
                match adt_def.def_id() {
                    AdtId::UnionId(_) => (),
                    AdtId::StructId(id) => {
                        let manually_drop = StructSignature::of(db, id)
                            .flags
                            .contains(StructFlags::IS_MANUALLY_DROP);
                        if !manually_drop {
                            self.drop_fields(addr, &layout, id.into(), subst, ty, meta)?;
                        }
                    }
                    AdtId::EnumId(id) => match &layout.variants {
                        Variants::Empty => (),
                        Variants::Single { index } => {
                            let variant = id
                                .enum_variants(db)
                                .variants
                                .values()
                                .map(|&(variant, _)| variant)
                                .find(|variant| variant.index(db) == index.0);
                            if let Some(variant) = variant {
                                self.drop_fields(addr, &layout, variant.into(), subst, ty, meta)?;
                            }
                        }
                        Variants::Multiple { tag, .. } => {
                            let wide = DiscriminantWidth::of(tag);
                            let discr = self.discriminant(addr, ty, wide.scalar())?;
                            let done = self.builder.create_block();
                            let mut switch = Switch::new();
                            let mut arms = Vec::new_in(self.frame);
                            for &(variant, _) in id.enum_variants(db).variants.values() {
                                let value = db.const_eval_discriminant(variant).map_err(|e| {
                                    // alloc: the error's message
                                    CodegenError::Frontend(format!("discriminant: {e:?}"))
                                })?;
                                let block = self.builder.create_block();
                                switch.set_entry(wide.entry(value), block);
                                arms.push((block, variant));
                            }
                            switch.emit(&mut self.builder, discr, done);
                            for (block, variant) in arms {
                                self.builder.switch_to_block(block);
                                let index = RustcEnumVariantIdx(variant.index(db));
                                let layout = &Layout::for_variant(&layout, index);
                                self.drop_fields(addr, layout, variant.into(), subst, ty, meta)?;
                                self.builder.ins().jump(done, &[]);
                            }
                            self.builder.switch_to_block(done);
                        }
                    },
                }
            }
        }
        Ok(())
    }

    fn drop_contents(&mut self, addr: Value, meta: Option<Value>, ty: Ty<'db>) -> Result<()> {
        let db = self.cg.db;
        // The future an `async fn` returns is still `impl Future` in its signature.
        let ty = self.normalize(ty);
        match ty.kind() {
            TyKind::Adt(adt_def, subst) => {
                if adt_def.is_box() {
                    // The compiler drops the pointee; `Box`'s `Drop` impl only frees the memory.
                    let pointee = subst.type_at(0);
                    let (pointer, pointee_meta) = self.box_pointer(addr, ty, pointee)?;
                    // The memory is freed even if dropping what it holds unwinds.
                    self.guarded(
                        |this| this.drop_value(pointer, pointee_meta, pointee),
                        |this| {
                            this.call_drop_impl(addr, meta, ty)?;
                            this.drop_adt_fields(addr, meta, ty)
                        },
                    )?;
                }
                if adt_def.destructor(self.interner).is_some() {
                    self.call_drop_impl_guarded(addr, meta, ty)?;
                }
                self.drop_adt_fields(addr, meta, ty)?;
            }
            TyKind::Tuple(tys) => self.drop_tuple_fields(addr, ty, tys.iter())?,
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
                let Some(TyKind::Tuple(tys)) = ty.closure_upvars_ty().map(|it| it.kind()) else {
                    unsupported!("drop glue for a closure without tupled upvars");
                };
                self.drop_tuple_fields(addr, ty, tys.iter())?
            }
            TyKind::Coroutine(..) => self.drop_coroutine(addr, ty)?,
            TyKind::Array(element, len) => {
                let Some(len) = try_const_usize(db, len) else {
                    unsupported!("array with unevaluated length");
                };
                let len = self.builder.ins().iconst(self.cg.target.pointer(), len as i64);
                self.drop_elements(addr, len, element)?;
            }
            TyKind::Slice(element) => {
                let Some(len) = meta else {
                    unsupported!("drop of a slice without a length");
                };
                self.drop_elements(addr, len, element)?;
            }
            TyKind::Dynamic(..) => {
                let Some(vtable) = meta else {
                    unsupported!("drop of `dyn` without a vtable");
                };
                // The first vtable entry is `drop_in_place`, null when there is nothing to drop.
                let glue = self.builder.ins().load(self.cg.target.pointer(), MEM, vtable, 0);
                let call = self.builder.create_block();
                let done = self.builder.create_block();
                self.builder.ins().brif(glue, call, &[], done, &[]);
                self.builder.switch_to_block(call);
                let mut sig = self.cg.make_signature();
                sig.params.push(AbiParam::new(self.cg.target.pointer()));
                let sig = self.builder.import_signature(sig);
                self.invoke_indirect(sig, glue, &[addr]);
                self.builder.ins().jump(done, &[]);
                self.builder.switch_to_block(done);
            }
            _ => unsupported!("drop glue for {ty:?}"),
        }
        Ok(())
    }

    fn drop_fields(
        &mut self,
        addr: Value,
        layout: &Layout,
        variant: VariantId,
        subst: GenericArgs<'db>,
        container: Ty<'db>,
        meta: Option<Value>,
    ) -> Result<()> {
        let field_types = self.cg.db.field_types(variant);
        let mut fields = Vec::new_in(self.frame);
        fields.extend(
            field_types
                .iter()
                .map(|(field, ty)| {
                    let ty = ty.ty().instantiate(self.interner, subst).skip_norm_wip();
                    (layout.fields.offset(u32::from(field.into_raw()) as usize), ty)
                })
                .filter(|&(_, ty)| self.needs_drop(ty)),
        );
        self.drop_ladder(fields.len(), |this, at| {
            let (offset, ty) = fields[at];
            if this.is_unsized(this.normalize(ty)) {
                let field_addr = this.unsized_field_addr(addr, meta, container, offset, ty)?;
                this.drop_value(field_addr, meta, ty)
            } else {
                let field_addr = this.builder.ins().iadd_imm(addr, offset.bytes() as i64);
                let field_addr = this.aligned_for_drop(field_addr, ty, container)?;
                this.drop_value(field_addr, None, ty)
            }
        })
    }

    fn drop_tuple_fields(
        &mut self,
        addr: Value,
        ty: Ty<'db>,
        fields: impl IntoIterator<Item = Ty<'db>>,
    ) -> Result<()> {
        let layout = self.layout(ty)?;
        let mut kept = Vec::new_in(self.frame);
        kept.extend(fields.into_iter().enumerate().filter(|&(_, field)| self.needs_drop(field)));
        let fields = kept;
        self.drop_ladder(fields.len(), |this, at| {
            let (index, field) = fields[at];
            let offset = layout.fields.offset(index).bytes() as i64;
            let field_addr = this.builder.ins().iadd_imm(addr, offset);
            this.drop_value(field_addr, None, field)
        })
    }

    fn needs_drop(&self, ty: Ty<'db>) -> bool {
        let ty = self.normalize(ty);
        self.glue_of(ty) != DropGlue::None
    }

    /// Runs `drop` for each of `0..count` in turn. If one unwinds the ones after it still run,
    /// on the way out.
    fn drop_ladder(
        &mut self,
        count: usize,
        mut drop: impl FnMut(&mut Self, usize) -> Result<()>,
    ) -> Result<()> {
        if self.in_cleanup || self.aborts || count < 2 {
            return (0..count).try_for_each(|at| drop(self, at));
        }
        let outer = self.landing_pad_for_call();
        let mut pads: Vec<ir::Block, &Frame> = Vec::with_capacity_in(count - 1, self.frame);
        pads.extend((1..count).map(|_| {
            let pad = self.builder.create_block();
            self.builder.append_block_param(pad, self.cg.target.pointer());
            self.builder.set_cold_block(pad);
            pad
        }));
        let enclosing = self.inner_pad;
        let mut dropped = Ok(());
        for at in 0..count {
            self.inner_pad = pads.get(at).copied().or(enclosing);
            dropped = drop(self, at);
            if dropped.is_err() {
                break;
            }
        }
        self.inner_pad = enclosing;
        dropped?;
        if self.builder.current_block().is_none() {
            unsupported!("drop without a block to return to");
        }
        // The pads are built before the code after the drops, and Cranelift wants a block
        // finished before it switches away from it: this one ends here, the rest goes on in `after`.
        let after = self.builder.create_block();
        self.builder.ins().jump(after, &[]);
        self.in_cleanup = true;
        let mut cleaned = Ok(());
        for at in 1..count {
            self.builder.switch_to_block(pads[at - 1]);
            let exception = self.builder.block_params(pads[at - 1])[0];
            cleaned = drop(self, at);
            if cleaned.is_err() {
                break;
            }
            match (pads.get(at), outer) {
                (Some(&next), _) | (None, Some(next)) => {
                    self.builder.ins().jump(next, &[exception.into()]);
                }
                (None, None) => cleaned = self.resume_unwinding(exception),
            }
        }
        self.in_cleanup = false;
        cleaned?;
        self.builder.switch_to_block(after);
        Ok(())
    }

    fn drop_elements(&mut self, addr: Value, len: Value, element: Ty<'db>) -> Result<()> {
        let size = self.layout(element)?.size.bytes() as i64;
        let done = self.builder.create_block();
        let zero = self.builder.ins().iconst(self.cg.target.pointer(), 0);
        if self.in_cleanup || self.aborts {
            self.drop_elements_from(addr, len, element, size, zero, None, done)?;
        } else {
            // If an element's drop unwinds, the ones after it are still dropped on the way out.
            let outer = self.landing_pad_for_call();
            let pad = self.builder.create_block();
            self.builder.append_block_param(pad, self.cg.target.pointer());
            self.builder.set_cold_block(pad);
            let index = self.drop_elements_from(addr, len, element, size, zero, Some(pad), done)?;
            self.builder.switch_to_block(pad);
            let exception = self.builder.block_params(pad)[0];
            let rest = self.builder.ins().iadd_imm(index, 1);
            let resume = self.builder.create_block();
            self.builder.set_cold_block(resume);
            self.in_cleanup = true;
            let cleaned = self.drop_elements_from(addr, len, element, size, rest, None, resume);
            self.in_cleanup = false;
            cleaned?;
            self.builder.switch_to_block(resume);
            match outer {
                Some(outer) => {
                    self.builder.ins().jump(outer, &[exception.into()]);
                }
                None => self.resume_unwinding(exception)?,
            }
        }
        self.builder.switch_to_block(done);
        Ok(())
    }

    /// Drops the elements from `start` on and jumps to `done`, unwinding to `pad` if there is
    /// one. Returns the index of the element being dropped.
    #[allow(clippy::too_many_arguments)]
    fn drop_elements_from(
        &mut self,
        addr: Value,
        len: Value,
        element: Ty<'db>,
        size: i64,
        start: Value,
        pad: Option<ir::Block>,
        done: ir::Block,
    ) -> Result<Value> {
        let header = self.builder.create_block();
        let body = self.builder.create_block();
        self.builder.append_block_param(header, self.cg.target.pointer());
        self.builder.ins().jump(header, &[start.into()]);
        self.builder.switch_to_block(header);
        let index = self.builder.block_params(header)[0];
        let more = self.builder.ins().icmp(IntCC::UnsignedLessThan, index, len);
        self.builder.ins().brif(more, body, &[], done, &[]);
        self.builder.switch_to_block(body);
        let offset = self.builder.ins().imul_imm(index, size);
        let element_addr = self.builder.ins().iadd(addr, offset);
        let enclosing = self.inner_pad;
        if pad.is_some() {
            self.inner_pad = pad;
        }
        let dropped = self.drop_value(element_addr, None, element);
        self.inner_pad = enclosing;
        dropped?;
        let next = self.builder.ins().iadd_imm(index, 1);
        self.builder.ins().jump(header, &[next.into()]);
        Ok(index)
    }

    /// Calls `<ty as Drop>::drop` on the value at `addr`, whose metadata is `meta` when the
    /// type is unsized.
    fn call_drop_impl(&mut self, addr: Value, meta: Option<Value>, ty: Ty<'db>) -> Result<()> {
        let Some(drop_fn) = self.interner.lang_items().Drop_drop else {
            unsupported!("no `Drop::drop` lang item");
        };
        let reference =
            Ty::new_ref(self.interner, Region::new_erased(self.interner), ty, Mutability::Mut);
        let slot = self.scratch_pair();
        self.builder.ins().store(MEM, addr, slot, 0);
        if let Some(meta) = meta {
            self.builder.ins().store(MEM, meta, slot, self.cg.target.meta_at());
        }
        let args = GenericArgs::new_from_slice(&[ty.into()]);
        let unit = Ty::new_unit(self.interner);
        self.call_direct(drop_fn, args, &[(slot, reference)], slot, unit)
    }

    fn finish_call(
        &mut self,
        destination: &StoredPlace,
        target: Option<BasicBlockId>,
    ) -> Result<()> {
        let _ = destination;
        match target {
            Some(target) => self.builder.ins().jump(self.blocks[target], &[]),
            None => self.builder.ins().trap(TrapCode::user(2).expect("non-zero trap code")),
        };
        Ok(())
    }
}

/// Prints a body one statement a line, for debugging with `BLAZE_DUMP_MIR`.
fn dump_mir(body: &MirBody<'_>) {
    let compact = |text: String| {
        let mut text = text
            .replace("Idx::<Local>(", "_(")
            .replace("Idx::<BasicBlock>(", "bb(")
            .replace(", projection: [] }", " }")
            .replace("StoredPlace { local: ", "{ ")
            .replace(", span: None }", " }")
            .replace("Operand { kind: ", "{ ");
        while let Some(start) = text.find(", span: Some(") {
            let end = text[start..].find("))").map_or(text.len(), |end| start + end + 2);
            text.replace_range(start..end, "");
        }
        text
    };
    for (local, data) in body.locals.iter() {
        // alloc: debug dump only
        eprintln!("  let {}: {:?}", compact(format!("{local:?}")), data.ty);
    }
    for (id, block) in body.basic_blocks.iter() {
        eprintln!("{}:", compact(format!("{id:?}"))); // alloc: debug dump only
        for statement in &block.statements {
            eprintln!("    {}", compact(format!("{:?}", statement.kind))); // alloc: debug dump only
        }
        if let Some(terminator) = &block.terminator {
            // alloc: debug dump only
            eprintln!("    -> {}", compact(format!("{:?}", terminator.kind)));
        }
    }
}

/// The locals that can be kept in a register as far as the body goes: temporaries written
/// whole by assignments only, that nothing takes the address of.
fn register_locals(
    body: &MirBody<'_>,
    forwarded: &HashMap<LocalId, LocalId, Global>,
) -> Vec<LocalId> {
    let whole = |place: &StoredPlace| place.projection.as_slice().is_empty().then_some(place.local);
    // Every mention of the whole local, and the ones that are a value read or an assignment.
    let mut mentions: HashMap<LocalId, (usize, usize), Global> = HashMap::new_in(Global);
    body.visit_places(|place| {
        if let Some(local) = whole(place) {
            mentions.entry(local).or_default().0 += 1;
        }
    });
    let mut plain = |place: &StoredPlace| {
        if let Some(local) = whole(place) {
            mentions.entry(local).or_default().1 += 1;
        }
    };
    fn read(operand: &Operand, plain: &mut impl FnMut(&StoredPlace)) {
        if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind {
            plain(place);
        }
    }
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(to, rvalue) = &statement.kind {
                plain(to);
                for operand in hir_ty_mir::mir::purity::operands(rvalue) {
                    read(operand, &mut plain);
                }
            }
        }
        match block.terminator.as_ref().map(|it| &it.kind) {
            Some(TerminatorKind::SwitchInt { discr, .. }) => read(discr, &mut plain),
            Some(TerminatorKind::Call { func, args, .. }) => {
                read(func, &mut plain);
                for arg in args.iter() {
                    read(arg, &mut plain);
                }
            }
            _ => (),
        }
    }
    let bound: blaze_hash::HashSet<LocalId, Global> = body
        .binding_locals
        .values()
        .chain(&body.param_locals)
        .copied()
        .chain(promoted_locals(body))
        .chain([returned_local(body)])
        .chain(forwarded.iter().flat_map(|(&temp, &to)| [temp, to]))
        .collect(); // alloc: once per body, which this function has no frame for
    body.locals
        .iter()
        .map(|(local, _)| local)
        .filter(|local| {
            !bound.contains(local) && mentions.get(local).is_none_or(|(all, plain)| all == plain)
        })
        .collect() // alloc: the result, once per body, which the caller takes owned
}

/// The locals of `body` that no place mentions, which hold nothing the body ever reads: their
/// storage markers are all there is of them. The parameters, the return slot and the promoted
/// locals are not among them, whatever mentions them: they are written from outside the body,
/// or read by it.
fn unmentioned_locals(body: &MirBody<'_>) -> HashSet<LocalId, Global> {
    let mut mentioned = HashSet::new_in(Global);
    body.visit_places(|place| {
        mentioned.insert(place.local);
    });
    body.locals
        .iter()
        .map(|(local, _)| local)
        .filter(|local| {
            !mentioned.contains(local)
                && !body.param_locals.contains(local)
                && *local != returned_local(body)
                && !is_promoted(body, *local)
                && !body.upvars_in_place.iter().any(|it| it.0 == *local)
        })
        .collect() // alloc: the result, once per body, which the caller takes owned
}

/// The local every return of `body` moves to the return slot, when nothing else mentions the
/// slot, as rustc's `RenameReturnPlace` finds it: in the caller's memory the value is built
/// where the caller wants it and returning copies nothing. A call's destination is a temporary
/// or a fresh binding, so the callee reaches that memory through nothing else.
fn returned_in_place(
    body: &MirBody<'_>,
    forwarded: &HashMap<LocalId, LocalId, Global>,
) -> Option<LocalId> {
    if body.coroutine.is_some() {
        return None;
    }
    let whole = |place: &StoredPlace| place.projection.as_slice().is_empty().then_some(place.local);
    let slot = return_slot();
    // Whether what runs from statement `at` of `block` on is storage markers, constants written
    // to other locals (drop flags) and jumps to a return: nothing touches `returned` after it
    // is moved.
    let returns = |returned: LocalId, mut block: BasicBlockId, mut at: usize| {
        let inert = |kind: &StatementKind| match kind {
            StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::Nop => {
                true
            }
            StatementKind::Assign(
                to,
                Rvalue::Use(Operand {
                    kind: OperandKind::Constant { .. } | OperandKind::Allocation { .. },
                    ..
                }),
            ) => whole(to).is_some_and(|it| {
                it != returned && it != slot && forwarded.get(&it) != Some(&returned)
            }),
            _ => false,
        };
        for _ in 0..body.basic_blocks.len() {
            let data = &body.basic_blocks[block];
            if !data.statements[at..].iter().all(|it| inert(&it.kind)) {
                return false;
            }
            match data.terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::Return) => return true,
                Some(TerminatorKind::Goto { target }) => (block, at) = (*target, 0),
                _ => return false,
            }
        }
        false
    };
    let mut returned = None;
    let mut moves = 0;
    for (id, block) in body.basic_blocks.iter() {
        for (at, statement) in block.statements.iter().enumerate() {
            let StatementKind::Assign(to, rvalue) = &statement.kind else { continue };
            if whole(to) != Some(slot) {
                continue;
            }
            let Rvalue::Use(Operand {
                kind: OperandKind::Copy(from) | OperandKind::Move(from),
                ..
            }) = rvalue
            else {
                return None;
            };
            let from = whole(from)?;
            if *returned.get_or_insert(from) != from || !returns(from, id, at + 1) {
                return None;
            }
            moves += 1;
        }
    }
    let local = returned.filter(|&it| it != slot)?;
    let mut mentions = 0;
    body.visit_places(|place| mentions += usize::from(place.local == slot));
    let eligible = mentions == moves
        && body.locals[local].ty == body.locals[slot].ty
        && !body.param_locals.contains(&local)
        && !body.upvars_in_place.iter().any(|it| it.0 == local)
        && !is_promoted(body, local);
    eligible.then_some(local)
}

/// How many times each local is dropped by a terminator or read by a `FakeRead`, counted in one
/// pass: a body of many blocks has many temporaries, and neither is a use of what one holds.
fn drops_and_fake_reads(body: &MirBody<'_>) -> HashMap<LocalId, usize, Global> {
    let mut counted: HashMap<LocalId, usize, Global> = HashMap::new_in(Global);
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::FakeRead(place) = &statement.kind {
                *counted.entry(place.local).or_default() += 1;
            }
        }
        if let Some(TerminatorKind::Drop { place, .. }) =
            block.terminator.as_ref().map(|it| &it.kind)
        {
            *counted.entry(place.local).or_default() += 1;
        }
    }
    counted
}

/// The temporaries a call's argument is moved through, each with the local moved into it:
/// `f(value)` moves `value` to a temporary and the temporary to `f`. The two can be one slot,
/// nothing reading or writing `value` between the move and the call, and the callee then gets
/// `value`'s memory, as rustc passes a moved local.
fn moved_arguments(
    body: &MirBody<'_>,
    forwarded: &HashMap<LocalId, LocalId, Global>,
) -> Vec<(LocalId, LocalId)> {
    let whole = |place: &StoredPlace| place.projection.as_slice().is_empty().then_some(place.local);
    let slot = return_slot();
    let mut found = Vec::new(); // alloc: the result, once per body, which the caller takes owned
    for (_, block) in body.basic_blocks.iter() {
        let Some(TerminatorKind::Call { func, args, destination, .. }) =
            block.terminator.as_ref().map(|it| &it.kind)
        else {
            continue;
        };
        let plain = |operand: &Operand| match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => whole(place),
            _ => None,
        };
        for arg in args.iter() {
            let OperandKind::Move(temp) = &arg.kind else { continue };
            let Some(temp) = whole(temp) else { continue };
            let Some(at) = block.statements.iter().rposition(
                |it| matches!(&it.kind, StatementKind::Assign(to, _) if to.local == temp),
            ) else {
                continue;
            };
            let StatementKind::Assign(
                _,
                Rvalue::Use(Operand { kind: OperandKind::Move(from), .. }),
            ) = &block.statements[at].kind
            else {
                continue;
            };
            let Some(from) = whole(from) else { continue };
            let apart = |local: Option<LocalId>| local.is_some_and(|it| it != from && it != temp);
            let between = block.statements[at + 1..].iter().all(|it| match &it.kind {
                StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::Nop => true,
                StatementKind::Assign(to, Rvalue::Use(operand)) => {
                    apart(whole(to))
                        && match operand.kind {
                            OperandKind::Constant { .. } | OperandKind::Allocation { .. } => true,
                            _ => apart(plain(operand)),
                        }
                }
                _ => false,
            });
            let others = args.iter().filter(|it| !std::ptr::eq(*it, arg)).all(|it| {
                matches!(it.kind, OperandKind::Constant { .. } | OperandKind::Allocation { .. })
                    || apart(plain(it))
            });
            let callee =
                matches!(func.kind, OperandKind::Constant { .. } | OperandKind::Allocation { .. })
                    || apart(plain(func));
            if between && others && callee && apart(whole(destination)) {
                found.push((temp, from));
            }
        }
    }
    if found.is_empty() {
        return found;
    }
    let mut mentions: HashMap<LocalId, usize, Global> = HashMap::new_in(Global);
    body.visit_places(|place| *mentions.entry(place.local).or_default() += 1);
    let not_uses = drops_and_fake_reads(body);
    let dropped_or_read = |local: LocalId| not_uses.get(&local).copied().unwrap_or(0);
    let bound: blaze_hash::HashSet<LocalId, Global> = body
        .binding_locals
        .values()
        .chain(&body.param_locals)
        .copied()
        .chain(promoted_locals(body))
        .chain(body.upvars_in_place.iter().map(|it| it.0))
        .chain([slot])
        .chain(forwarded.values().copied())
        .collect(); // alloc: once per body with a moved argument, which this has no frame for
    found.retain(|&(temp, from)| {
        !bound.contains(&temp)
            && !forwarded.contains_key(&temp)
            && !forwarded.contains_key(&from)
            && from != slot
            && !is_promoted(body, from)
            && mentions[&temp] - dropped_or_read(temp) == 2
            && body.locals[temp].ty == body.locals[from].ty
    });
    // alloc: once per body with a moved argument, which this has no frame for
    let temps: blaze_hash::HashSet<LocalId, Global> = found.iter().map(|it| it.0).collect();
    found.retain(|(_, from)| !temps.contains(from));
    blaze_ids::radix::sort_copy_by_key(&mut found, |&(temp, from)| {
        blaze_ids::radix::pack_32(temp.into_raw().into_u32(), from.into_raw().into_u32())
    });
    found.dedup();
    found
}

/// The temporaries a `let` is initialized through, each with the binding it is copied to right
/// after it is written: `let buffer = [0u8; N]` builds the array in a temporary and copies it.
/// The two can be one slot, the binding holding nothing yet when the temporary is written, and
/// a frame is not twice the size of what it declares.
fn forwarded_temps(body: &MirBody<'_>) -> HashMap<LocalId, LocalId, Global> {
    let whole = |place: &StoredPlace| place.projection.as_slice().is_empty().then_some(place.local);
    let mut forwarded = HashMap::new_in(Global);
    if body.coroutine.is_some() {
        return forwarded;
    }
    for (_, block) in body.basic_blocks.iter() {
        // The temporary written last, by a statement of this block or by the call that led
        // here, and whether the binding's storage has started since.
        let mut written = None;
        for statement in &block.statements {
            match &statement.kind {
                StatementKind::StorageLive(local) => {
                    if let Some((_, live)) = &mut written {
                        *live = Some(*local);
                    }
                }
                StatementKind::FakeRead(_) | StatementKind::Nop => (),
                StatementKind::Assign(
                    to,
                    Rvalue::Use(Operand {
                        kind: OperandKind::Copy(from) | OperandKind::Move(from),
                        ..
                    }),
                ) if let Some((temp, live)) = written
                    && whole(from) == Some(temp)
                    && whole(to).is_some()
                    && whole(to) == live =>
                {
                    forwarded.insert(temp, to.local);
                    written = None;
                }
                // A constant written to another local in between, as a drop flag is.
                StatementKind::Assign(
                    to,
                    Rvalue::Use(Operand {
                        kind: OperandKind::Constant { .. } | OperandKind::Allocation { .. },
                        ..
                    }),
                ) if let Some((temp, live)) = written
                    && let Some(to) = whole(to)
                    && to != temp
                    && Some(to) != live => {}
                StatementKind::Assign(to, _) => written = whole(to).map(|it| (it, None)),
                _ => written = None,
            }
        }
        // FIXME: a temporary a call returns into is copied in the block the call leads to.
    }
    if forwarded.is_empty() {
        return forwarded;
    }
    // Written once and read once, and by nothing else: the binding is not what it is made of.
    let mut mentions: HashMap<LocalId, usize, Global> = HashMap::new_in(Global);
    body.visit_places(|place| *mentions.entry(place.local).or_default() += 1);
    // A drop of the temporary is not a use: it holds a value only between the two statements,
    // which unwind to nowhere, and a drop of it then finds the slot holding its value.
    let not_uses = drops_and_fake_reads(body);
    let dropped_or_read = |local: LocalId| not_uses.get(&local).copied().unwrap_or(0);
    let bound: blaze_hash::HashSet<LocalId, Global> = body
        .binding_locals
        .values()
        .chain(&body.param_locals)
        .copied()
        .chain(promoted_locals(body))
        .chain([returned_local(body)])
        .collect(); // alloc: once per body with a forwarded temporary, which this has no frame for
    forwarded.retain(|&temp, to| {
        !bound.contains(&temp)
            && mentions[&temp] - dropped_or_read(temp) == 2
            && body.locals[temp].ty == body.locals[*to].ty
            && !is_promoted(body, *to)
            && *to != returned_local(body)
    });
    forwarded
}

/// The lanes of a SIMD vector, each a value: inline up to 64, the lanes of 512 bits of bytes.
type LaneValues = smallvec::SmallVec<[Value; 64]>;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

#[cfg(test)]
mod tests {
    use cranelift_codegen::ir::{self, AbiParam, InstBuilder, types};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

    /// core's two UB checks are known by their pre-interned symbols, however the name was
    /// interned; `ub_checks` is the intrinsic, not one of them.
    #[test]
    fn ub_checks_are_known_by_symbol() {
        use super::{Intrinsic, is_ub_check};
        use intern::Symbol;
        assert!(is_ub_check(&intern::sym::check_language_ub));
        assert!(is_ub_check(&Symbol::intern("check_library_ub")));
        assert!(!is_ub_check(&Symbol::intern("ub_checks")));
        assert!(!is_ub_check(&Symbol::intern("contract_checks")));
        assert_eq!(Intrinsic::from_symbol(&Symbol::intern("ub_checks")), Some(Intrinsic::UbChecks));
        assert_eq!(Intrinsic::from_symbol(&Symbol::intern("contract_checks")), None);
        assert_eq!(Intrinsic::from_symbol(&intern::sym::check_language_ub), None);
    }

    /// A layout lent for a build is the database's own, not a copy: the interned layout the
    /// query gave, and a variant's layout is the copy the build kept, however many more it
    /// keeps after it. Seeded layouts of every integer width, enough for the list to grow many
    /// times.
    #[test]
    fn a_lent_layout_is_the_one_the_query_gave() {
        use super::{KeptLayouts, Layout, Primitive, Scalar, sve};
        use hir_ty::layout::intern_layout;
        use rustc_abi::Integer;
        let dl = rustc_abi::TargetDataLayout::default();
        let widths = [Integer::I8, Integer::I16, Integer::I32, Integer::I64, Integer::I128];
        let kept = KeptLayouts::new();
        let mut asked = Vec::new();
        let mut variants = Vec::new();
        let mut rng = blaze_ids::rng::SplitMix64::new(0x9e37_79b9);
        for _ in 0..4096 {
            let seed = rng.next_u32();
            let int = widths[seed as usize % widths.len()];
            let scalar = Scalar::Union { value: Primitive::Int(int, seed & 1 == 0) };
            let given = intern_layout(Layout::scalar(&dl, scalar)).get();
            asked.push((sve::storage(given), given));
            let copy = Box::new(Layout::scalar(&dl, scalar));
            let at: *const Layout = &*copy;
            variants.push((kept.variants.alloc(copy), at));
        }
        for (lent, given) in asked {
            assert!(std::ptr::eq(lent, &given.0));
        }
        for (lent, at) in variants {
            assert!(std::ptr::eq(lent, at));
        }
        kept.keep();
        assert!(
            super::spare::layout_list().is_empty(),
            "a kept list holds a layout past its build"
        );
    }

    /// The panic of each overflow check is named whole, as core declares it: `call_panic`
    /// finds it by that name and keeps it by it.
    #[test]
    fn each_overflow_check_has_a_panic_of_its_own() {
        let names = [
            "panic_const_add_overflow",
            "panic_const_sub_overflow",
            "panic_const_mul_overflow",
            "panic_const_div_overflow",
            "panic_const_rem_overflow",
            "panic_const_neg_overflow",
            "panic_const_shl_overflow",
            "panic_const_shr_overflow",
            "panic_const_div_by_zero",
            "panic_const_rem_by_zero",
        ];
        let distinct: blaze_hash::HashSet<_, std::alloc::Global> = names.iter().collect();
        assert_eq!(distinct.len(), names.len());
    }

    /// A function compiled in a context the thread kept, whose register allocator and
    /// dominator tree another function grew, gives the bytes a fresh context gives.
    #[test]
    fn a_kept_context_compiles_what_a_fresh_one_does() {
        let isa = super::isa_for(super::Target::for_host()).unwrap();
        let build = |blocks: usize, by: i64| {
            let mut sig = ir::Signature::new(isa.default_call_conv());
            sig.params.push(AbiParam::new(types::I64));
            sig.returns.push(AbiParam::new(types::I64));
            let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), sig);
            let mut builder_ctx = FunctionBuilderContext::new();
            let mut builder = FunctionBuilder::new(&mut func, &mut builder_ctx);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            let mut value = builder.block_params(entry)[0];
            for _ in 0..blocks {
                let next = builder.create_block();
                let param = builder.append_block_param(next, types::I64);
                value = builder.ins().imul_imm(value, by);
                builder.ins().jump(next, &[value.into()]);
                builder.switch_to_block(next);
                value = param;
            }
            builder.ins().return_(&[value]);
            builder.seal_all_blocks();
            builder.finalize(isa.frontend_config());
            func
        };
        let fresh = {
            let mut context = cranelift_codegen::Context::for_function(build(2, 41));
            super::emit(&mut context, super::Target::for_host()).unwrap()
        };
        super::with_context(build(64, 3), |context| {
            super::emit(context, super::Target::for_host()).unwrap()
        });
        let kept = super::with_context(build(2, 41), |context| {
            super::emit(context, super::Target::for_host()).unwrap()
        });
        assert_eq!(kept.bytes, fresh.bytes);
        assert_eq!(kept.relocs.len(), fresh.relocs.len());
        assert_eq!(kept.align, fresh.align);
    }

    /// A `link_name` that starts with `\u{1}` is the object's symbol: the object writer's
    /// prefix comes off it, so that the symbol is not prefixed twice.
    #[test]
    fn a_link_name_written_as_the_symbol_is_not_prefixed_again() {
        use super::as_declared;
        let sha = "\u{1}_aws_lc_0_44_0_SHA384";
        assert_eq!(as_declared(sha, Some('_')), "aws_lc_0_44_0_SHA384");
        assert_eq!(as_declared("\u{1}aws_lc_0_44_0_SHA384", None), "aws_lc_0_44_0_SHA384");
        assert_eq!(as_declared("SHA384", Some('_')), "SHA384");
        assert_eq!(as_declared("\u{1}SHA384", Some('_')), "\u{1}SHA384");
    }

    /// `emit` names the function itself by a placeholder id until it is stitched, and
    /// `FuncId::from_u32` asserts `u32::MAX` away where debug assertions are on, as in
    /// `cargo test`: a placeholder of `u32::MAX` refused every function as a backend panic.
    #[test]
    fn emit_compiles_a_function_with_debug_assertions_on() {
        let isa = super::isa_for(super::Target::for_host()).unwrap();
        let mut sig = ir::Signature::new(isa.default_call_conv());
        sig.returns.push(AbiParam::new(types::I32));
        let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), sig);
        let mut builder_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut builder_ctx);
        let entry = builder.create_block();
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let zero = builder.ins().iconst(types::I32, 0);
        builder.ins().return_(&[zero]);
        builder.finalize(isa.frontend_config());
        let mut ctx = cranelift_codegen::Context::for_function(func);
        let code = super::emit(&mut ctx, super::Target::for_host()).unwrap();
        assert!(!code.bytes.is_empty());
    }

    /// An object with `count` statics of a `u32` in `section` and as many pointers to a
    /// function in `pointers`, as `#[link_section]` and `inventory::submit!` give.
    fn object_of_statics(
        triple: target_lexicon::Triple,
        data_sections: bool,
        used: bool,
        (section, pointers): (&str, &str),
        count: u32,
    ) -> Vec<u8> {
        use cranelift_codegen::settings::{self, Configurable};
        use cranelift_module::{DataDescription, Linkage, Module, default_libcall_names};
        use cranelift_object::{ObjectBuilder, ObjectModule};

        let mut flags = settings::builder();
        flags.set("is_pic", "true").unwrap();
        let isa = cranelift_codegen::isa::lookup(triple)
            .unwrap()
            .finish(settings::Flags::new(flags))
            .unwrap();
        let mut builder = ObjectBuilder::new(isa, "statics", default_libcall_names()).unwrap();
        builder.per_data_object_section(data_sections);
        let mut module = ObjectModule::new(builder);
        let sig = module.make_signature();
        let init = module.declare_function("init", Linkage::Local, &sig).unwrap();
        // `ret` on aarch64.
        module.define_function_bytes(init, 4, &[0xc0, 0x03, 0x5f, 0xd6], &[]).unwrap();
        for i in 0..count {
            let entry =
                module.declare_data(&format!("ENTRY_{i}"), Linkage::Local, false, false).unwrap();
            let mut data = DataDescription::new();
            match i {
                0 => data.define_zeroinit(4),
                _ => data.define(Box::new(i.to_le_bytes())),
            }
            data.set_align(4);
            data.set_custom_section(section);
            data.set_used(used);
            module.define_data(entry, &data).unwrap();

            let ctor =
                module.declare_data(&format!("CTOR_{i}"), Linkage::Local, false, false).unwrap();
            let mut data = DataDescription::new();
            data.define(Box::new([0; 8]));
            data.set_align(8);
            data.set_custom_section(pointers);
            data.set_used(used);
            let func = module.declare_func_in_data(init, &mut data);
            data.write_function_addr(0, func);
            module.define_data(ctor, &data).unwrap();
        }
        module.finish().emit().unwrap()
    }

    /// Statics that name one section share it, as in LLVM's objects: a Mach-O object has room
    /// for 255 sections, and blaze writes a program as one (#724).
    #[test]
    fn statics_of_one_section_share_it_in_a_mach_o_object() {
        use object::macho;
        use object::read::macho::{MachOFile64, Nlist, Section as _};
        use object::{Endianness, Object, ObjectSection, ObjectSymbol};

        let count = 300;
        let names = ("__DATA,__blaze_many", "__DATA,__mod_init_func,mod_init_funcs");
        let triple = target_lexicon::triple!("aarch64-apple-darwin");
        let bytes = object_of_statics(triple, false, true, names, count);
        let file = MachOFile64::<Endianness>::parse(&*bytes).unwrap();
        assert!(file.sections().count() < 255);
        let named = |name: &str| {
            let mut found = file.sections().filter(|it| it.name() == Ok(name));
            let section = found.next().unwrap();
            assert!(found.next().is_none(), "two sections named `{name}`");
            section
        };

        let many = named("__blaze_many");
        assert_eq!(many.segment_name(), Ok(Some("__DATA")));
        assert_eq!(many.align(), 4);
        let values: Vec<u32> = many
            .data()
            .unwrap()
            .chunks(4)
            .map(|it| u32::from_le_bytes(it.try_into().unwrap()))
            .collect();
        assert_eq!(values, (0..count).collect::<Vec<_>>());
        let ctors = named("__mod_init_func");
        assert_eq!(ctors.size(), 8 * u64::from(count));
        assert_eq!(
            ctors.macho_section().flags(Endianness::Little),
            macho::S_MOD_INIT_FUNC_POINTERS.to_flags()
        );
        let mut at: Vec<u64> = ctors.relocations().map(|(offset, _)| offset).collect();
        at.sort_unstable();
        assert_eq!(at, (0..u64::from(count)).map(|i| 8 * i).collect::<Vec<_>>());

        // Each keeps its own symbol, which Apple's linker keeps or strips on its own.
        for (prefix, section, size) in [("_ENTRY_", &many, 4), ("_CTOR_", &ctors, 8)] {
            let mut offsets: Vec<u64> = file
                .symbols()
                .filter(|it| it.name().is_ok_and(|name| name.starts_with(prefix)))
                .map(|it| {
                    assert_eq!(it.section_index(), Some(section.index()));
                    let n_desc = it.macho_symbol().n_desc(Endianness::Little);
                    assert!(n_desc.intersects(macho::N_NO_DEAD_STRIP));
                    it.address() - section.address()
                })
                .collect();
            offsets.sort_unstable();
            assert_eq!(offsets, (0..u64::from(count)).map(|i| size * i).collect::<Vec<_>>());
        }
    }

    /// In an ELF object the section is what the linker keeps or drops: one asked for each piece
    /// of data, or one `#[used]` keeps (`SHF_GNU_RETAIN`), stays one per static, which the
    /// linker merges by name; otherwise the statics of a section share it.
    #[test]
    fn statics_of_one_section_in_an_elf_object_share_it_unless_each_is_kept_alone() {
        use object::{Object, ObjectSection, SectionFlags, elf};

        let count = 300;
        let names = ("blaze_many", ".init_array");
        let sections = |data_sections: bool, used: bool| {
            let triple = target_lexicon::triple!("aarch64-unknown-linux-gnu");
            let bytes = object_of_statics(triple, data_sections, used, names, count);
            let file = object::File::parse(&*bytes).unwrap();
            let of = |name: &str| {
                let found: Vec<_> = file
                    .sections()
                    .filter(|it| it.name() == Ok(name))
                    .map(|it| it.flags())
                    .collect();
                let retained = found.iter().filter(|flags| match flags {
                    SectionFlags::Elf { sh_flags, .. } => sh_flags.intersects(elf::SHF_GNU_RETAIN),
                    _ => unreachable!(),
                });
                (found.len(), retained.count())
            };
            (of(names.0), of(names.1))
        };
        let count = count as usize;
        assert_eq!(sections(false, false), ((1, 0), (1, 0)));
        assert_eq!(sections(true, false), ((count, 0), (count, 0)));
        assert_eq!(sections(false, true), ((count, count), (count, count)));
    }

    /// A COFF symbol does not say how large it is: each static keeps a section of its own,
    /// whose size the PE splice reads as the static's. Shared, a `u32` before one aligned to
    /// 8 read as 8 bytes, and an edit of it was linked in full (`splices.py`, "a static
    /// pointed to").
    #[test]
    fn statics_of_one_section_in_a_coff_object_each_keep_their_own() {
        use object::{Object, ObjectSection, ObjectSymbol};

        let count = 300;
        let names = (".rdata$static", ".CRT$XCU");
        let triple = target_lexicon::triple!("x86_64-pc-windows-msvc");
        let bytes = object_of_statics(triple, false, false, names, count);
        let file = object::File::parse(&*bytes).unwrap();
        for (name, prefix, size) in [(names.0, "ENTRY_", 4), (names.1, "CTOR_", 8)] {
            let found: Vec<_> = file.sections().filter(|it| it.name() == Ok(name)).collect();
            assert_eq!(found.len(), count as usize, "sections named `{name}`");
            assert!(found.iter().all(|it| it.size() == size));
            for symbol in file.symbols().filter(|it| it.name().is_ok_and(|n| n.starts_with(prefix)))
            {
                let section = file.section_by_index(symbol.section_index().unwrap()).unwrap();
                assert_eq!((section.name(), symbol.address()), (Ok(name), 0));
            }
        }
    }

    /// An ISA for `triple`, as an object of it is written with.
    // dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private, one made per process
    fn isa_of(triple: &str) -> &'static dyn cranelift_codegen::isa::TargetIsa {
        use cranelift_codegen::settings::{self, Configurable};
        let mut flags = settings::builder();
        flags.set("is_pic", "true").unwrap();
        let isa = cranelift_codegen::isa::lookup(triple.parse().unwrap())
            .unwrap()
            .finish(settings::Flags::new(flags))
            .unwrap();
        &**Box::leak(Box::new(isa))
    }

    /// The relocations of `func` compiled for `isa`.
    fn relocs_of(
        // dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private, one made per process
        isa: &dyn cranelift_codegen::isa::TargetIsa,
        func: ir::Function,
    ) -> Vec<cranelift_codegen::binemit::Reloc> {
        let mut ctx = cranelift_codegen::Context::for_function(func);
        let compiled = ctx.compile(isa, &mut Default::default()).unwrap();
        compiled.buffer.relocs().iter().map(|it| it.kind).collect()
    }

    /// Each call names its callee in place, a `bl` or a `call`, as rustc's do, on every
    /// format: an AArch64 ELF or Mach-O call loaded the callee's address from the GOT first
    /// and branched to the register, an instruction and a load more on each call.
    #[test]
    fn a_call_names_its_callee_in_place_on_every_format() {
        use cranelift_codegen::binemit::Reloc;
        for (triple, call) in [
            ("aarch64-unknown-linux-gnu", Reloc::Arm64Call),
            ("aarch64-apple-darwin", Reloc::Arm64Call),
            ("x86_64-apple-darwin", Reloc::X86CallPCRel4),
            ("x86_64-unknown-linux-gnu", Reloc::X86CallPCRel4),
        ] {
            let isa = isa_of(triple);
            let mut wants = super::fragment::Wants::new(crate::Target::for_host(), isa);
            let sig = ir::Signature::new(isa.default_call_conv());
            let callee = wants.import("callee", &sig).unwrap();
            let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), sig);
            let called = wants.declare_func_in_func(callee, &mut func);
            assert!(func.dfg.ext_funcs[called].colocated, "{triple}");
            let mut builder_ctx = FunctionBuilderContext::new();
            let mut builder = FunctionBuilder::new(&mut func, &mut builder_ctx);
            let entry = builder.create_block();
            builder.switch_to_block(entry);
            builder.seal_block(entry);
            builder.ins().call(called, &[]);
            builder.ins().return_(&[]);
            builder.finalize(isa.frontend_config());
            assert_eq!(relocs_of(isa, func), [call], "{triple}");
        }
    }

    /// A function of two pointers that `copy`s `size` bytes from the second to the first
    /// with `memcpy`, or `memmove` where `overlap`, as `copy_nonoverlapping::<[u64; 3]>(.., 1)`
    /// is built: the count times the size of the type.
    fn copying(size: (i64, i64), overlap: bool) -> ir::Function {
        let isa = super::isa_for(super::Target::for_host()).unwrap();
        let pointer = isa.pointer_type();
        let mut sig = ir::Signature::new(isa.default_call_conv());
        sig.params.extend([AbiParam::new(pointer), AbiParam::new(pointer)]);
        let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), sig);
        let mut builder_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut builder_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let &[to, from] = builder.block_params(entry) else { unreachable!() };
        let count = builder.ins().iconst(pointer, size.0);
        let bytes = builder.ins().imul_imm(count, size.1);
        let config = isa.frontend_config();
        if overlap {
            builder.call_memmove(config, to, from, bytes);
        } else {
            builder.call_memcpy(config, to, from, bytes);
        }
        builder.ins().return_(&[]);
        builder.finalize(config);
        func
    }

    /// The opcodes of `func` that load, store or call, in order.
    fn accesses(func: &ir::Function) -> Vec<(ir::Opcode, Option<ir::Type>)> {
        let insts = func.layout.blocks().flat_map(|it| func.layout.block_insts(it));
        insts
            .filter_map(|inst| {
                let opcode = func.dfg.insts[inst].opcode();
                let ty = match opcode {
                    ir::Opcode::Load => Some(func.dfg.value_type(func.dfg.first_result(inst))),
                    ir::Opcode::Store => Some(func.dfg.value_type(func.dfg.inst_args(inst)[0])),
                    ir::Opcode::Call => None,
                    _ => return None,
                };
                Some((opcode, ty))
            })
            .collect()
    }

    /// A copy of a size known once the function is built, and at most 128 bytes, is loads
    /// and stores, as LLVM's -O0 makes it, not a call of `memcpy`; one of `memmove` loads all
    /// it copies before it stores any.
    #[test]
    fn a_small_copy_of_a_known_size_is_loads_and_stores() {
        use ir::Opcode::{Load, Store};
        let mut func = copying((1, 24), false);
        assert!(super::copies::lower_small(&mut func));
        let i64 = Some(types::I64);
        assert_eq!(accesses(&func), [(Load, i64), (Store, i64)].repeat(3));

        let mut func = copying((7, 1), true);
        assert!(super::copies::lower_small(&mut func));
        let loads = [(Load, Some(types::I32)), (Load, Some(types::I16)), (Load, Some(types::I8))];
        let stores = loads.map(|(_, ty)| (Store, ty));
        assert_eq!(accesses(&func), [&loads[..], &stores[..]].concat());
        let mut ctx = cranelift_codegen::Context::for_function(func);
        assert!(!super::emit(&mut ctx, super::Target::for_host()).unwrap().bytes.is_empty());
    }

    /// A wasm32 function's `memcpy` takes and gives the target's 32-bit addresses, not the
    /// 64-bit ones of the native ISA its IR is made under: the call type-checks, and a small
    /// copy lowers to loads and stores whose call result becomes the destination, of the same
    /// type. With the ISA's own config the call took and gave `i64`s, and the lowering
    /// aliased the `i64` result to the `i32` destination, which Cranelift's debug assertion
    /// refused building a proc macro on aarch64 Linux ("Aliasing v462 to v458 would change
    /// its type i64 to i32").
    #[test]
    fn a_wasm32_copys_memcpy_takes_and_gives_32_bit_addresses() {
        for (triple, pointer) in
            [("wasm32-wasip1", types::I32), ("wasm64-blaze-wasip1", types::I64)]
        {
            let target = super::Target::of(triple).unwrap();
            let isa = &**super::isa_for(target).unwrap();
            let config = target.frontend_config(isa);
            assert_eq!(config.pointer_type(), pointer, "{triple}");
            let mut sig = ir::Signature::new(config.default_call_conv);
            sig.params.extend([AbiParam::new(pointer), AbiParam::new(pointer)]);
            let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), sig);
            let mut builder_ctx = FunctionBuilderContext::new();
            let mut builder = FunctionBuilder::new(&mut func, &mut builder_ctx);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            builder.seal_block(entry);
            let &[to, from] = builder.block_params(entry) else { unreachable!() };
            let bytes = builder.ins().iconst(pointer, 24);
            builder.call_memcpy(config, to, from, bytes);
            builder.ins().return_(&[]);
            builder.finalize(config);
            cranelift_codegen::verify_function(&func, isa.flags())
                .unwrap_or_else(|e| panic!("{triple}: {e}"));
            assert!(super::copies::lower_small(&mut func), "{triple}");
            cranelift_codegen::verify_function(&func, isa.flags())
                .unwrap_or_else(|e| panic!("{triple}: {e}"));
        }
    }

    /// IR the verifier refuses is the function's typed error, naming the stage it was refused
    /// at and the instruction, before any backend or pass is given it: the IR of dfgalias's
    /// bug, a wasm32 `memcpy` declared under the native ISA's own 64-bit addresses and called
    /// with 32-bit ones, which only a debug assertion of the copy lowering caught after it, so
    /// a release blaze kept it.
    #[test]
    fn ir_the_verifier_refuses_is_the_functions_error_before_any_pass() {
        let target = super::Target::of("wasm32-wasip1").unwrap();
        let isa = &**super::isa_for(target).unwrap();
        let ill = isa.frontend_config();
        assert_eq!(ill.pointer_type(), types::I64, "the premise: the native ISA's own addresses");
        let mut sig = ir::Signature::new(ill.default_call_conv);
        sig.params.extend([AbiParam::new(types::I32), AbiParam::new(types::I32)]);
        let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), sig);
        let mut builder_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut builder_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let &[to, from] = builder.block_params(entry) else { unreachable!() };
        let bytes = builder.ins().iconst(types::I32, 24);
        builder.call_memcpy(ill, to, from, bytes);
        builder.ins().return_(&[]);
        builder.finalize(target.frontend_config(isa));
        let error = super::verified(&func, isa, "built").expect_err("ill-typed IR verified");
        let super::CodegenError::InvalidIr { stage, ref errors } = error else { panic!("{error}") };
        assert_eq!(stage, "built");
        assert!(errors.contains("call"), "{errors}");
        assert!(error.to_string().contains("Cranelift's verifier refuses (as built)"), "{error}");
    }

    /// One of more than 128 bytes stays a call, of `memcpy` named in place.
    #[test]
    fn a_large_copy_stays_a_call_named_in_place() {
        let mut func = copying((17, 8), false);
        assert!(!super::copies::lower_small(&mut func));
        assert_eq!(accesses(&func), [(ir::Opcode::Call, None)]);
        assert!(func.dfg.ext_funcs.values().all(|it| it.colocated));
    }

    /// `raw_eq` of at most two words compares integers, as rustc's does, not bytes with
    /// `memcmp`; of more, it has no such comparison.
    #[test]
    fn raw_eq_of_two_words_compares_integers() {
        let isa = super::isa_for(super::Target::for_host()).unwrap();
        let pointer = isa.pointer_type();
        let mut sig = ir::Signature::new(isa.default_call_conv());
        sig.params.extend([AbiParam::new(pointer), AbiParam::new(pointer)]);
        sig.returns.push(AbiParam::new(types::I8));
        let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), sig);
        let mut builder_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut builder_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let &[a, b] = builder.block_params(entry) else { unreachable!() };
        let words = 2 * u64::from(pointer.bytes());
        assert!(super::copies::equal_bytes(&mut builder, pointer, a, b, words + 1).is_none());
        let equal = super::copies::equal_bytes(&mut builder, pointer, a, b, 12).unwrap();
        builder.ins().return_(&[equal]);
        builder.finalize(isa.frontend_config());
        let loads: Vec<_> = accesses(&func).into_iter().map(|it| it.1).collect();
        let (i64, i32) = (Some(types::I64), Some(types::I32));
        assert_eq!(loads, [i64, i64, i32, i32]);
        let mut ctx = cranelift_codegen::Context::for_function(func);
        assert!(super::emit(&mut ctx, super::Target::for_host()).unwrap().relocs.is_empty());
    }

    /// A function that reads its return address, as the fuzz engine's comparison callbacks do,
    /// compiles in a unit that keeps no frame pointers: it alone keeps one.
    #[test]
    fn a_function_reading_its_return_address_keeps_its_frame_pointer() {
        if super::Target::for_host().is_wasm() {
            return;
        }
        let isa = super::isa_for(super::Target::for_host()).unwrap();
        let pointer = isa.pointer_type();
        let mut sig = ir::Signature::new(isa.default_call_conv());
        sig.returns.push(AbiParam::new(pointer));
        let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), sig);
        let mut builder_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut builder_ctx);
        let entry = builder.create_block();
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let address = builder.ins().get_return_address(pointer);
        builder.ins().return_(&[address]);
        builder.finalize(isa.frontend_config());
        let mut ctx = cranelift_codegen::Context::for_function(func);
        assert!(!super::emit(&mut ctx, super::Target::for_host()).unwrap().bytes.is_empty());
    }

    /// `ub_checks()` is what rustc makes it: in core, alloc and std the compiling crate's
    /// debug assertions, in any other crate its own, whoever compiles the body. It was
    /// false everywhere, and a debug build ran none of the library's precondition checks.
    #[test]
    fn ub_checks_are_the_compiling_crates_in_the_library_and_the_writing_crates_elsewhere() {
        use super::ub_checks_in;
        for compiling in [false, true] {
            assert_eq!(ub_checks_in(compiling, true, false), compiling);
            assert_eq!(ub_checks_in(compiling, true, true), compiling);
            assert!(!ub_checks_in(compiling, false, false));
            assert!(ub_checks_in(compiling, false, true));
        }
    }

    /// Code is generated from MIR at rustc's level: 1 for a crate at -O0, 2 for the library's
    /// crates and above -O0. It was the runtime body, unoptimized, unless `BLAZE_MIR_OPT`.
    #[test]
    fn mir_is_optimized_at_rustcs_level() {
        use hir_ty_mir::mir::MirOptLevel::{Debug, Release};
        assert_eq!(super::rustc_mir_opt_level(false, false), Debug);
        assert_eq!(super::rustc_mir_opt_level(true, false), Release);
        assert_eq!(super::rustc_mir_opt_level(false, true), Release);
    }

    /// `BLAZE_MIR_OPT` unset leaves rustc's level; a value that is no level is an error
    /// naming it, never rustc's level in its place.
    #[test]
    fn a_mir_opt_level_that_is_none_is_an_error() {
        use hir_ty_mir::mir::MirOptLevel::{Off, Release};
        use std::ffi::OsStr;
        assert_eq!(super::mir_opt_level_in(None), Ok(None));
        assert_eq!(super::mir_opt_level_in(Some(OsStr::new("0"))), Ok(Some(Off)));
        assert_eq!(super::mir_opt_level_in(Some(OsStr::new("2"))), Ok(Some(Release)));
        let refused = super::mir_opt_level_in(Some(OsStr::new("3"))).unwrap_err();
        assert!(refused.contains("BLAZE_MIR_OPT=3"), "{refused}");
    }

    /// MIR is inlined where rustc's `Inline` runs at level 2: the library's crates always,
    /// another crate only at -O2 and -O3, and never at level 1 or below.
    #[test]
    fn mir_is_inlined_where_rustcs_is() {
        use super::rustc_inlines;
        use hir_ty_mir::mir::MirOptLevel::{Debug, Off, Release};
        assert!(rustc_inlines(Release, true, false));
        assert!(rustc_inlines(Release, false, true));
        assert!(!rustc_inlines(Release, false, false));
        for level in [Off, Debug] {
            assert!(!rustc_inlines(level, true, true));
            assert!(!rustc_inlines(level, false, true));
        }
    }
}
