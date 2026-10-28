//! ra-jit spike: lower RA MIR to native code with Cranelift.
//!
//! Every local lives in memory. Calls follow rustc's (unstable) Rust ABI so the
//! output can be linked against rustc-compiled objects.

extern crate ra_ap_rustc_abi as rustc_abi;
extern crate ra_ap_rustc_ast_ir as rustc_ast_ir;
extern crate ra_ap_rustc_index as rustc_index;
extern crate ra_ap_rustc_type_ir as rustc_type_ir;

use std::{borrow::Cow, fmt, sync::LazyLock};

use base_db::Crate;
use std::cell::Cell;

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
    signatures::{FunctionSignature, StaticFlags, StaticSignature, StructFlags, StructSignature},
    src::HasSource,
};
use hir_expand::{FileRange, HirFileId, InFile, MacroDefKind, builtin::BuiltinFnLikeExpander};
use la_arena::ArenaMap;
use rustc_abi::{ExternAbi, Integer, ReprFlags, Size};
use rustc_ast_ir::Mutability;
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_index::Idx;
use rustc_type_ir::UintTy;
use rustc_type_ir::inherent::{
    AdtDef as _, BoundExistentialPredicates as _, GenericArgs as _, IntoKind, SliceLike, Ty as _,
};
use salsa::plumbing::{AsId, FromId};
use syntax::{AstNode, SyntaxKind, SyntaxNode, TextRange, TextSize, ast};
use triomphe::Arc;

use hir_def::lang_item::LangItems;
use hir_ty::mir::is_by_move_coroutine;
use hir_ty::{
    InferBodyId, InferenceResult, MemoryMap, ParamEnvAndCrate, PointerCast,
    consteval::try_const_usize,
    db::HirDatabase,
    db::{GeneralConstId, InternedClosureId, InternedCoroutineId},
    drop::{DropGlue, destructor, has_drop_glue},
    layout::{CoroutineFrame, Layout, RustcEnumVariantIdx, RustcFieldIdx},
    method_resolution::{is_dyn_method, lookup_impl_const},
    next_solver::{
        Allocation, AllocationData, ConstKind, DbInterner, GenericArgs, Region, StoredGenericArgs,
        StoredTy, TraitRef, Ty, TyKind, TypingMode, UnevaluatedConst, ValTree,
        infer::{DbInternerInferExt, InferCtxt},
    },
    traits::StoredParamEnvAndCrate,
    utils::detect_variant_from_bytes,
};

use self::debuginfo::{DebugContext, FunctionDebug, LineRow, SlotPlaces, VariableDebug};
use self::fragment::{
    Code, CompiledFunction, CompiledStatic, DataRequest, FuncRequest, FuncTarget, Named, Recorder,
    Requests, TextPart, Variable,
};
use self::unwind::{FunctionUnwind, UnwindContext};
use self::vtable::{Slot, Upcast, VtableLayout};
pub use hir_ty::mir::location::name_sources;
use hir_ty::mir::{
    AggregateKind, AssertKind, BasicBlockId, BinOp, CastKind, LocalId, MirBody, MirOptLevel,
    MirPhase, MirSpan, Operand, OperandKind, PlaceTy, ProjectionElem, Rvalue, StatementKind,
    StoredPlace, TerminatorKind, UnOp, interpret_mir, is_builtin_index,
    location::{
        ConcatPart, column_width, concat_parts, line_and_column, located_concats, location_macros,
        location_of, written_call, written_range,
    },
    monomorphized, monomorphized_promoted_mir_body_query,
    overflow::{body_written_checked, crate_checks_overflow},
};

mod asm;
mod avx512;
mod backend;
use backend::Step;
pub use backend::{Backend, builds_edits, links_own, sections_of_their_own};
mod cached;
mod debuginfo;
pub use debuginfo::Sidecar;
mod f16_f128;
mod float_pair;
mod fncache;
pub use fncache::{AlwaysRun, Reach, tests_reach};
mod fragment;
mod home;
mod later;
pub use home::body_file;
#[cfg(feature = "llvm")]
mod imports;
#[cfg(feature = "llvm")]
mod llvm;

/// The CPU of this host and its features as LLVM names them (`+avx2,-avx512f,...`), what
/// rustc's `-C target-cpu=native` is; `None` without LLVM.
pub fn host_cpu() -> Option<(String, String)> {
    #[cfg(feature = "llvm")]
    {
        use inkwell::llvm_sys::{core::LLVMDisposeMessage, target_machine as tm};
        let taken = |message: *mut std::ffi::c_char| {
            // SAFETY: LLVM returns a C string the caller owns and disposes of.
            let text = unsafe { std::ffi::CStr::from_ptr(message) }.to_string_lossy().into_owned();
            unsafe { LLVMDisposeMessage(message) };
            text
        };
        // SAFETY: neither reads anything but the host's CPU.
        let (name, features) =
            unsafe { (taken(tm::LLVMGetHostCPUName()), taken(tm::LLVMGetHostCPUFeatures())) };
        Some((name, features))
    }
    #[cfg(not(feature = "llvm"))]
    None
}
mod neon;
mod overlap;
mod patch;
mod places;
#[cfg(feature = "llvm")]
mod units;
use self::places::{row_location, source_location};
mod promote;
mod settled;
mod stable;
use ra_jit_store as store;
pub use stable::{block_place, function_place, lower_declared, static_place};
mod target;
pub use target::{Arch, Calls, ObjectFormat, Target, UnwindFormat, VariableArguments};
pub mod tier;

pub use patch::{OutOfDate, Stale, StaleSteps};
/// For `crates/ra-jit-bench` alone.
#[doc(hidden)]
pub mod bench {
    use ra_jit_store::KEY;

    /// The key of the entry for `name`, as a build asks for it: one that tells a debugger of
    /// its variables when `debug`.
    pub fn entry_key(name: &str, debug: bool, program: &[u8; KEY]) -> Option<[u8; KEY]> {
        let told = super::cached::Told::of(debug, debug);
        super::cached::Entry::key(name, told, Some(program))
    }
}
mod type_id;
mod unwind;
mod vtable;
pub mod wasm;
mod wasm_object;
mod x86;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodegenError {
    /// The function uses something this backend can't compile yet; fall back to rustc.
    Unsupported(String),
    /// MIR lowering or layout failed in RA.
    Frontend(String),
    Backend(String),
    /// rustc exports nothing for the function, so there is nothing to replace: only code in
    /// its own crate can call it.
    NoSymbol,
    /// rustc built the function under a symbol private to its object, so its copy stays the
    /// one that code of that object calls, and goes on using rustc's statics.
    Private,
    /// What ra-jit can't do and no other path may stand in for: the build stops with this
    /// error, naming what and where. Never left to rustc, never a build in full instead.
    Refused(String),
}

impl CodegenError {
    /// Whether rustc has no symbol of the function that ours could take the place of.
    pub fn no_symbol(&self) -> bool {
        matches!(self, CodegenError::NoSymbol | CodegenError::Private)
    }
}

impl fmt::Display for CodegenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodegenError::Unsupported(it) => write!(f, "unsupported: {it}"),
            CodegenError::Frontend(it) => write!(f, "frontend: {it}"),
            CodegenError::Backend(it) => write!(f, "backend: {it}"),
            CodegenError::NoSymbol | CodegenError::Private => {
                write!(f, "unsupported: no rustc symbol for function")
            }
            CodegenError::Refused(it) => write!(f, "{it}"),
        }
    }
}

type Result<T> = std::result::Result<T, CodegenError>;

macro_rules! unsupported {
    ($($arg:tt)*) => {
        return Err(CodegenError::Unsupported(format!($($arg)*)))
    };
}

// After `unsupported!`, which they use.
mod fma;
mod sve;
mod wasm_simd;

/// Stops the build with an error ([`CodegenError::Refused`]).
macro_rules! refused {
    ($($arg:tt)*) => {
        return Err(CodegenError::Refused(format!($($arg)*)))
    };
}

/// What a crate is by what is written in it and what it is built on: a SHA-256, all of it.
/// The names made from it are what another machine's store is asked for code by, so it is
/// as hard to find two crates of one id as two files of one hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentId(pub [u8; 32]);

impl fmt::Display for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// Where cargo keeps the sources it fetched of registries and of git, as what is stored names
/// it: the same on every machine, whoever's home the directory is in.
pub const CARGO_HOME: &str = "/cargo";

/// What the name of a static a patch defines anew starts with, which is the same in every
/// patch after for the same static and value: a running program keeps the copy the first gave
/// it (`docs/live.md#types`).
pub const LASTING_STATIC: &str = "ra_jit::static::lasting::";

/// What the name of a data object of a patch starts with that says the patch left the base's
/// copy of a static behind, the rest of the name being that copy's: the patch has one of its
/// own, and everything that refers to it (`patch::Base::moved`), so nothing that runs refers to
/// the base's.
pub const MOVED_STATIC: &str = "ra_jit::static::moved::";

/// `path` with the directory `to` in place of the directory `from` it is under.
pub fn moved(path: &str, from: &str, to: &str) -> Option<String> {
    let rest = path.strip_prefix(from)?;
    rest.starts_with(['/', '\\']).then(|| format!("{to}{rest}"))
}

/// `path` as rustc spells it in a location or in debug info. For a file of the installed
/// standard library rustc joins `lib/rustlib/src/rust` to the sysroot as one piece, and then
/// the path the library was built from, whose crate's directory the build named as
/// `library/core`: on Windows it reads `<sysroot>\lib/rustlib/src/rust\library/core\src\..`
/// (nightly-2026-10-25, harness/breakers.py `library_panic_location`) where the file system
/// names it with backslashes throughout. Elsewhere the two are already the same.
fn as_rustc_spells(path: &str) -> String {
    path.replacen(r"\lib\rustlib\src\rust\library\", r"\lib/rustlib/src/rust\library/", 1)
}

/// The sources of the standard library: rustc names them where they are on this machine, and
/// what is stored names them where they were when the library was built, as every machine can.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibrarySources {
    /// The directory they are in here.
    pub here: String,
    /// The directory they were built in, `/rustc/<commit>`.
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
    fn function(&self, func: FunctionId) -> Option<String>;
    fn static_(&self, id: StaticId) -> Option<String>;
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
    fn path(&self, path: &str) -> Option<String>;
    /// The path of an item, e.g. `core::any::TypeId`.
    fn item(&self, item: hir_def::ModuleDefId) -> Option<String>;
    /// The struct, enum or union [`Self::item`] spells `path`, when one of a module does.
    fn adt_at(&self, _path: &str) -> Option<hir_def::AdtId> {
        None
    }
    /// The name rustc gives a source file in panic messages.
    fn source_path(&self, file: base_db::FileId) -> Option<String>;
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
    /// ra-jit built before, which an entry of the store may name.
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
    fn closure(&self, _owner: FunctionId, _expr: hir_def::hir::ExprId) -> Option<String> {
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
    /// The `StableCrateId` of each crate that compiled a copy of an instance of a function
    /// named `function`.
    fn makers(&self, _function: &str) -> Vec<u64> {
        Vec::new()
    }
    /// The copies rustc made of the instance `symbol` is one of, each with the
    /// `StableCrateId` of the crate that compiled it.
    fn copies(&self, _symbol: &str) -> Vec<(String, u64)> {
        Vec::new()
    }
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
    /// What stands for a crate that is not rustc's alone to build, the same wherever the
    /// same sources are built on the same crates.
    fn content_id(&self, krate: Crate) -> Option<ContentId> {
        _ = krate;
        None
    }
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
    fn in_parallel_beside(&self, count: usize, work: &ParallelWork<'_>, beside: &mut dyn FnMut()) {
        _ = (count, work);
        beside();
    }
}

/// What an instance is called in every run of the daemon, if anything.
type StableName = std::sync::Arc<str>;

pub type ParallelWork<'a> = dyn Fn(&dyn HirDatabase, &dyn SymbolResolver, usize) + Sync + 'a;

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

/// The result of compiling a set of root functions into one object file.
pub struct CodegenUnit {
    /// The object, unless the one of the build before still stands and `patch` amends it.
    pub object: Option<Vec<u8>>,
    /// With [`Rejections::patching`], an object to link ahead of the last `object`: its
    /// definitions replace those of the same name there.
    pub patch: Option<Vec<u8>>,
    /// The files that go with `object`: what a debugger reads that is kept apart from it.
    pub apart: Vec<Sidecar>,
    /// Roots that were compiled, with their exported symbol.
    pub compiled: Vec<(FunctionId, String)>,
    /// Roots that have to come from rustc.
    pub skipped: Vec<(FunctionId, CodegenError)>,
    /// Callees without a rustc symbol (generic or inlined) that were compiled as local functions.
    pub instances: usize,
}

/// The functions a build starts from.
#[derive(Clone, Copy)]
pub struct Roots<'a> {
    /// Compiled whether or not anything calls them.
    pub entries: &'a [FunctionId],
    /// Compiled once code in the object refers to them. A function not reached keeps rustc's
    /// copy, which nothing in the program calls.
    pub on_demand: &'a [FunctionId],
    /// The `main` of a program rustc built nothing of: the `main` the C runtime calls is
    /// then ours to write as well.
    pub start: Option<FunctionId>,
    /// The object is linked into a shared library of its own, which has the allocator shim
    /// a program's `start` otherwise brings.
    pub shared: bool,
    /// Whether rustc built nothing of any crate of the program but the standard library's,
    /// whatever its `main` is: bun's is C's, and every one of its functions is an entry.
    pub whole: bool,
    /// The statics a linker or a runtime finds though no code names them, in the crates
    /// rustc built nothing of: `#[used]` ones, and those put in a section by name.
    pub kept: &'a [StaticId],
    /// Statics defined, where they can be, for objects built apart from this one: those of a
    /// library built before the programs that use it. One that cannot be is left out, and a
    /// function that uses it is refused.
    pub offered: &'a [StaticId],
}

impl CodegenUnit {
    /// Compiles every root it can, together with the instances those roots call.
    ///
    /// A callee that can't be compiled poisons its callers, so codegen restarts with that callee
    /// rejected up front until a round finishes without a new rejection.
    pub fn compile(
        db: &dyn HirDatabase,
        symbols: &dyn SymbolResolver,
        roots: Roots<'_>,
        denied: &FxHashSet<FunctionId>,
        known: &mut Rejections,
    ) -> Result<CodegenUnit> {
        let unit =
            CodegenUnit::compile_or_hold(db, symbols, roots, denied, known, &|| false, false)?;
        Ok(unit.expect("a build that is never held makes a unit"))
    }

    /// As [`CodegenUnit::compile`], but an edit that can't be a patch is built whole only if
    /// `hold` says not to hold it. `None` when it is held: nothing is built, and the next edit
    /// is still a patch of the last full build. A text that does not compile is held, so that
    /// the build of the next one that does is not of what this one made of the program.
    ///
    /// With `undoable`, a full build that replaces another keeps what it replaced until
    /// [`Rejections::keep_built`] or [`Rejections::undo_built`]: a build the daemon refuses
    /// once it is made is undone, and the next edit is a patch of the build before it.
    pub fn compile_or_hold(
        db: &dyn HirDatabase,
        symbols: &dyn SymbolResolver,
        roots: Roots<'_>,
        denied: &FxHashSet<FunctionId>,
        known: &mut Rejections,
        hold: &dyn Fn() -> bool,
        undoable: bool,
    ) -> Result<Option<CodegenUnit>> {
        backend::keep_frame_pointers(symbols.keeps_frame_pointers());
        let on_demand: FxHashSet<FunctionId> = roots.on_demand.iter().copied().collect();
        let all = || roots.entries.iter().chain(roots.on_demand).copied();
        // Code of the checkout comes from the store in the first build alone: what a patch is
        // of has to be compiled here, to tell what an edit changes. The build that makes a base
        // while edits are patches of rustc's executable is that first build too.
        let hydrate = known.base.is_none() && !known.full;
        let mut replaced = match known.base.take() {
            Some(base) => Some(base),
            None if !known.full => known.rustc.take(),
            None => None,
        };
        let over_rustc = replaced.as_ref().is_some_and(|it| it.rustc);
        let forgot = known.forgot_rustc.take().filter(|_| replaced.is_none());
        let mut declined =
            replaced.as_ref().filter(|it| it.rustc).and_then(|it| it.why_outdated()).or(forgot);
        if let Some(base) = replaced.take_if(|it| !it.outdated()) {
            let start = stdx::time::Instant::now();
            // Every static is rustc's, which rustc's code reads.
            let own_statics = match base.rustc {
                true => OwnStatics::none(),
                false => OwnStatics::new(
                    db,
                    symbols,
                    all().map(|root| root.krate(db)).collect(),
                    known.theirs.clone(),
                    known.unknown.clone(),
                    !roots.on_demand.is_empty(),
                ),
            };
            let statics = start.elapsed();
            let mut stale = std::mem::take(&mut known.stale);
            if stale.is_empty() {
                stale.push(base.stale(db, Some(symbols), 0, 1));
            }
            let patch = loop {
                let patch = base.patch(
                    db,
                    symbols,
                    roots,
                    &on_demand,
                    denied,
                    known,
                    own_statics.clone(),
                    stale.clone(),
                );
                let why = match patch {
                    Ok(Ok(patch)) => break Some(patch),
                    Ok(Err(why)) => Some(why),
                    Err(e @ CodegenError::Refused(_)) => return Err(e),
                    Err(_) => None,
                };
                // Code compiled at a higher level that a patch can't hold goes, not the
                // program the patch is for.
                let told = why.unwrap_or("it could not be compiled");
                if !known.tiers.refuse(&format!("no patch can hold its code: {told}")) {
                    declined = why;
                    break None;
                }
            };
            if let Some(patch) = patch {
                backend::Tally::global().report();
                let compiled_now = known.compiled_since_last_build();
                known.stats = BuildStats {
                    bodies: base.bodies,
                    reused: base.bodies.saturating_sub(compiled_now),
                    compiled: compiled_now,
                    phases: Phases { statics, ..patch.phases },
                    rounds: 1,
                    patched: Some(patch.bodies),
                    declined: None,
                    over_rustc: base.rustc,
                    onto_rustcs: false,
                };
                let unit = CodegenUnit {
                    object: None,
                    patch: Some(patch.object),
                    apart: Vec::new(),
                    compiled: base.compiled.clone(),
                    skipped: base.skipped.clone(),
                    instances: base.instances.len(),
                };
                known.put_base(base);
                return Ok(Some(unit));
            }
            replaced = Some(base);
        }
        if let Some(base) = replaced.take_if(|_| hold()) {
            known.stats = BuildStats {
                bodies: base.bodies,
                reused: base.bodies,
                declined,
                ..BuildStats::default()
            };
            known.put_base(base);
            return Ok(None);
        }
        known.undo = None;
        if let Some(base) = replaced.filter(|_| undoable) {
            let mut kept = known.clone();
            kept.put_base(base);
            known.undo = Some(Box::new(kept));
        }
        if over_rustc {
            symbols.in_full();
        }
        let patching = known.patching;
        let used_by_rustc = known.used_by_rustc;
        // FIXME: a rejection outlives an edit that would lift it. Dropping them here costs one
        // full pass per rejected call chain, and they go away with the rustc fallback.
        let Rejections {
            rejected,
            stats,
            theirs,
            unknown,
            left,
            futures,
            base,
            rustc,
            full,
            forgot_rustc: _,
            patching: _,
            counted,
            stale: _,
            frozen,
            used_by_rustc: _,
            tiers,
            undo,
        } = known;
        // A full build is linked again, and the program that runs it starts at level 1: once
        // it is kept, when it can be undone.
        if undo.is_some() {
            tiers.stop_trials();
        } else {
            tiers.clear();
        }
        if (roots.start.is_some() || roots.whole) && !backend::edits_follow() {
            backend::set_whole_program();
        }
        let root_crates: FxHashSet<Crate> = all().map(|root| root.krate(db)).collect();
        let mut own_statics = OwnStatics::new(
            db,
            symbols,
            root_crates.iter().copied().collect(),
            std::mem::take(theirs),
            std::mem::take(unknown),
            !roots.on_demand.is_empty(),
        );
        let mut rounds = 0;
        let mut discarded = std::time::Duration::ZERO;
        let mut start = roots.start;
        // The entries read from the store, for every round: one that starts over for an
        // instance that did not compile would read them all again.
        let mut entries: Option<std::sync::Arc<cached::Entries>> = None;
        loop {
            rounds += 1;
            let round = stdx::time::Instant::now();
            let mut start_over = |why: &str| {
                discarded += round.elapsed();
                if std::env::var_os("RA_JIT_STATS").is_some() {
                    eprintln!("round {rounds}: {:.1?}, started over: {why}", round.elapsed());
                }
            };
            let mut cg = ObjectCodegen::new(
                db,
                symbols,
                rejected,
                own_statics.clone(),
                &on_demand,
                None,
                hydrate,
            )?;
            match &entries {
                Some(read) if cg.entries.is_some() => cg.entries = Some(read.clone()),
                _ => entries = cg.entries.clone(),
            }
            cg.patching = patching;
            cg.rustc_futures = futures.clone();
            cg.rustc_values = frozen.rustc_values();
            let mut compiled = Vec::new();
            let mut skipped = Vec::new();
            let mut failed = Vec::new();
            let mut queue = roots.entries.to_vec();
            let mut queued: FxHashSet<FunctionId> = queue.iter().copied().collect();
            let mut next = 0;
            if let Some(main) = start {
                cg.define_start(main)?;
                cg.define_allocator_shim(main.krate(db))?;
            } else if roots.shared
                && let Some(first) = roots.entries.first()
            {
                cg.define_allocator_shim(first.krate(db))?;
            }
            cg.define_rustc_allocator_entries(&root_crates)?;
            for &id in roots.kept {
                cg.static_data(id)?;
            }
            for &id in roots.offered {
                _ = cg.static_data(id);
            }
            // Every root at once: an entry calls roots a later round is to find.
            cg.find_stored_roots(&all().collect::<Vec<_>>());
            loop {
                // Every core compiles the roots found so far, before they are put in the
                // object one by one.
                let ours: Vec<FunctionId> = queue[next..]
                    .iter()
                    .copied()
                    .filter(|root| !denied.contains(root) && !left.contains(root))
                    .collect();
                let start = stdx::time::Instant::now();
                cg.find_stored_roots(&ours);
                // One without a symbol is not compiled, and is still lowered to be told why.
                let (told, stored) = (cg.tells_variables().then_some(&cg.told), &cg.stored_roots);
                if cg.units() {
                    #[cfg(feature = "llvm")]
                    cg.compile_units(&ours, Vec::new());
                } else {
                    symbols.in_parallel(ours.len(), &|db, symbols, at| {
                        if symbols.function(ours[at]).is_none() {
                            _ = db.mir_body(ours[at].into());
                        } else if stored.get(&ours[at]).is_some_and(Option::is_some) {
                        } else if let (Ok(compiled), Some(told)) =
                            (compiled_root(db, ours[at]).as_ref(), told)
                        {
                            told.ahead(db, compiled);
                        }
                    });
                    cg.phases.compile += start.elapsed();
                }
                // The other threads go on to what the roots call while these are put in the
                // object.
                cg.compile_ahead(ours, &mut |cg, mut ahead| {
                    // The roots go in the object in the order they were found, whichever
                    // the threads are through with first: a root still being compiled is
                    // waited for, so that every build lays the program out the same (#200).
                    loop {
                        let Some(&root) = queue.get(next) else { break };
                        next += 1;
                        if let Some(ahead) = &ahead
                            && !denied.contains(&root)
                            && !left.contains(&root)
                        {
                            ahead.wait_root(root);
                        }
                        let result = if denied.contains(&root) {
                            Err(CodegenError::Unsupported("left to rustc on request".to_owned()))
                        } else if left.contains(&root) {
                            let reason = "shares an `async` block with a function left to rustc";
                            Err(CodegenError::Unsupported(reason.to_owned()))
                        } else {
                            cg.define_function(root)
                        };
                        match result {
                            Ok(symbol) => compiled.push((root, symbol)),
                            Err(e @ CodegenError::Refused(_)) => {
                                cg.refused.get_or_insert(e);
                                break;
                            }
                            Err(e) => {
                                // rustc's copy calls what ours would have, when it has one.
                                if !on_demand.is_empty()
                                    && !e.no_symbol()
                                    && !cg.own_statics.unbuilt.contains(&root.krate(db))
                                {
                                    let unseen = cg.want_reachable_from(root);
                                    cg.want_all_called(&unseen);
                                }
                                skipped.push((root, e));
                            }
                        }
                        let found = queue.len();
                        queue.extend(cg.wanted.drain(..).filter(|&func| queued.insert(func)));
                        if let Some(ahead) = &mut ahead {
                            let found = queue[found..].iter().copied();
                            ahead.root(
                                found.filter(|it| !denied.contains(it) && !left.contains(it)),
                            );
                        }
                    }
                    failed.extend(cg.define_waves(ahead));
                });
                queue.extend(cg.wanted.drain(..).filter(|&func| queued.insert(func)));
                if next == queue.len() || cg.refused.is_some() {
                    break;
                }
            }
            cg.make_later();
            if let Some(refused) = cg.refused.take() {
                return Err(refused);
            }
            if cg.misfit {
                // What was read of the entry that did not fit, and of those that call it.
                entries = None;
                start_over("an entry of the store did not fit");
                rejected.extend(failed);
                continue;
            }
            // The `main` the C runtime calls would call a `main` that is not in the object.
            if start.is_some_and(|main| skipped.iter().any(|(root, _)| *root == main)) {
                start = None;
                rejected.extend(failed);
                continue;
            }
            let fallback_start = stdx::time::Instant::now();
            let from_rustc: FxHashSet<FunctionId> = left_to_rustc(db, symbols, &skipped).collect();
            // A function left to rustc keeps using rustc's copy of a private static, so we
            // can't bring our own.
            let found = TheirStatics::of(db, from_rustc.iter().copied());
            let mut split: Vec<StaticId> =
                found.statics.iter().copied().filter(|it| cg.statics.contains_key(it)).collect();
            own_statics.theirs.extend(found.statics);
            let before = own_statics.unknown.len();
            own_statics.unknown.extend(found.unknown.iter().copied());
            if own_statics.unknown.len() != before {
                split.extend(cg.statics.keys().copied().filter(|&it| !own_statics.owns(db, it)));
            }
            if !split.is_empty() {
                let named = split.iter().take(4).map(|&it| {
                    let name = StaticSignature::of(db, it).name.as_str().to_owned();
                    format!("`{name}` of {}", fncache::crate_name(db, it.krate(db)))
                });
                let named = named.collect::<Vec<_>>().join(", ");
                let unknown = found.unknown.iter().map(|&it| fncache::crate_name(db, it));
                let unknown = unknown.collect::<Vec<_>>().join(", ");
                start_over(&format!(
                    "{} statics are used by a function left to rustc: {named}; \
                     crates of functions left to rustc that refer to what is not known: [{unknown}]",
                    split.len()
                ));
                cg.reject(&mut failed, &compiled, &from_rustc, &mut own_statics);
                rejected.extend(failed);
                continue;
            }
            // The copy rustc makes of a function without a symbol, where it is used, handles
            // the futures the function does.
            let generic: Vec<FunctionId> = match used_by_rustc {
                true => skipped.iter().filter(|(_, e)| e.no_symbol()).map(|it| it.0).collect(),
                false => Vec::new(),
            };
            // An instance of ours can still poll a future rustc's code made: one of a generic
            // function that awaits a function left to rustc, or that rustc's copy of a
            // forwarded instance hands back.
            if failed.is_empty() && futures_kept_apart() {
                let before = futures.len();
                for root in from_rustc.iter().chain(&generic) {
                    futures.extend(async_blocks(db, *root).iter().flatten().copied());
                }
                for (instance, env) in &cg.envs {
                    if cg.forwarded.contains(instance) {
                        futures.extend(made_futures(db, instance, env));
                    }
                }
                if futures.len() != before {
                    start_over("a future rustc's code makes is polled by code of ours");
                    continue;
                }
            }
            if failed.is_empty()
                && futures_kept_apart()
                && (!from_rustc.is_empty() || !generic.is_empty() || !futures.is_empty())
            {
                let mut theirs = Some(futures.clone());
                for root in from_rustc.iter().chain(&generic) {
                    let blocks = async_blocks(db, *root).clone();
                    theirs = theirs.zip(blocks).map(|(mut all, blocks)| {
                        all.extend(blocks);
                        all
                    });
                }
                let before = left.len();
                left.extend(compiled.iter().map(|it| it.0).filter(|&root| {
                    match (async_blocks(db, root).clone(), &theirs) {
                        (Some(ours), Some(theirs)) => !ours.is_disjoint(theirs),
                        (Some(ours), None) => !ours.is_empty(),
                        (None, _) => true,
                    }
                }));
                if left.len() != before {
                    start_over("an `async` block is shared with a function left to rustc");
                    continue;
                }
            }
            if failed.is_empty() && symbols.nothing_of_rustc() {
                for (root, e) in &skipped {
                    if let Some(symbol) = symbols.function(*root) {
                        cg.define_refused(&symbol, &e.to_string())?;
                    }
                }
            }
            if failed.is_empty() {
                let instances = cg.instances.len();
                if let Some(store) = store::Store::global() {
                    let mut misses: Vec<_> = cg.names.misses.take().into_iter().collect();
                    misses.sort_unstable_by_key(|it| std::cmp::Reverse(it.1));
                    let (mut kept, writer) = (std::mem::take(&mut cg.kept), cg.writer.take());
                    // The object and the link do not wait for the store to have what was
                    // kept: whoever reads the store next does.
                    store.settle_with(move || {
                        if let Some(mut writer) = writer {
                            kept.written(writer.finish());
                        }
                        store.flush();
                        let (found, added) = store.take_counts();
                        if found + added != 0 {
                            eprintln!("store: {found} functions found, {added} compiled and added");
                        }
                        kept.report();
                        if !misses.is_empty() {
                            eprintln!("without a stable name: {misses:?}");
                        }
                    });
                }
                let total = COMPILED.load(std::sync::atomic::Ordering::Relaxed);
                let compiled_now = total - std::mem::replace(counted, total);
                *stats = BuildStats {
                    bodies: cg.bodies,
                    reused: cg.bodies.saturating_sub(compiled_now),
                    compiled: compiled_now,
                    phases: Phases { fallback: fallback_start.elapsed(), discarded, ..cg.phases },
                    rounds,
                    patched: None,
                    declined,
                    over_rustc: false,
                    onto_rustcs: false,
                };
                if patching {
                    *base = patch::Base::record(db, &cg, roots.entries, &compiled, &skipped);
                }
                // The executable is linked from this object from now on.
                (*rustc, *full) = (None, false);
                if let Some(path) = std::env::var_os("RA_JIT_LAYOUTS") {
                    let lines = cg
                        .layouts
                        .iter()
                        .map(|(ty, (size, align))| format!("{ty}\t{size}\t{align}\n"))
                        .collect::<String>();
                    // Appended: isolated builds of single functions go through here too.
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .and_then(|mut file| std::io::Write::write_all(&mut file, lines.as_bytes()))
                        .map_err(|e| CodegenError::Backend(format!("layout log: {e}")))?;
                }
                // The object frees them as it is written, on a thread of its own.
                drop(entries.take());
                let start = stdx::time::Instant::now();
                let (object, apart) = cg.finish()?;
                stats.phases.object = start.elapsed();
                backend::Tally::global().report();
                (*theirs, *unknown) = (own_statics.theirs, own_statics.unknown);
                let patch = if patching { Some(patch::empty_object()?) } else { None };
                return Ok(Some(CodegenUnit {
                    object: Some(object),
                    patch,
                    apart,
                    compiled,
                    skipped,
                    instances,
                }));
            }
            start_over(&format!(
                "{} instances did not compile: {}",
                failed.len(),
                why_most(&failed)
            ));
            cg.reject(&mut failed, &compiled, &from_rustc, &mut own_statics);
            rejected.extend(failed);
        }
    }
}

/// The reasons the most of `failed` did not compile, with how many each: what they called that
/// failed is left out, so one function that fails is one reason for all its callers.
fn why_most<T>(failed: &[(T, Rejected)]) -> String {
    let mut counts: FxHashMap<&str, usize> = FxHashMap::default();
    for (_, rejected) in failed {
        let mut why = rejected.why.as_str();
        while let Some((_, rest)) = why
            .strip_prefix("unsupported: ")
            .unwrap_or(why)
            .strip_prefix("callee `")
            .and_then(|it| it.split_once("`: "))
        {
            why = rest;
        }
        *counts.entry(why.get(..200).unwrap_or(why)).or_default() += 1;
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    counts.iter().take(6).map(|(why, n)| format!("{n} × {why}")).collect::<Vec<_>>().join("; ")
}

/// Which statics we define ourselves rather than use rustc's. A function left to rustc uses
/// rustc's copy of a static, so the ones it refers to stay rustc's, along with whatever refers
/// to them.
#[derive(Clone)]
struct OwnStatics {
    crates: FxHashSet<Crate>,
    /// Those of `crates` that a crate of the program left to rustc depends on: its code refers
    /// to the statics rustc exports from them.
    shared: FxHashSet<Crate>,
    theirs: FxHashSet<StaticId>,
    /// Crates with a function left to rustc that refers to we don't know what. That can be
    /// any static of the crate: one declared in the body of another function too, through
    /// rustc's copy of that function, which it calls.
    unknown: FxHashSet<Crate>,
    /// The crates of the program rustc built nothing of, whose statics have no copy but ours.
    unbuilt: FxHashSet<Crate>,
}

impl OwnStatics {
    /// None: those of a patch of what rustc linked, all of whose statics are rustc's.
    fn none() -> OwnStatics {
        OwnStatics {
            crates: FxHashSet::default(),
            shared: FxHashSet::default(),
            theirs: FxHashSet::default(),
            unknown: FxHashSet::default(),
            unbuilt: FxHashSet::default(),
        }
    }

    fn new(
        db: &dyn HirDatabase,
        symbols: &dyn SymbolResolver,
        crates: FxHashSet<Crate>,
        theirs: FxHashSet<StaticId>,
        unknown: FxHashSet<Crate>,
        is_program: bool,
    ) -> OwnStatics {
        // With on-demand roots the build is of a program whose `main` is in `crates`, and only
        // what that depends on is linked. Without, anything may be linked on top.
        let program: FxHashSet<Crate> = if is_program {
            linked_crates(db, crates.iter().copied())
        } else {
            base_db::all_crates(db).iter().copied().collect()
        };
        let mut dependents = FxHashMap::<Crate, Vec<Crate>>::default();
        for &krate in &program {
            for dep in &krate.data(db).dependencies {
                dependents.entry(dep.crate_id).or_default().push(krate);
            }
        }
        let is_shared = |krate: &Crate| {
            let mut seen = FxHashSet::default();
            let mut stack = vec![*krate];
            while let Some(krate) = stack.pop() {
                for &dependent in dependents.get(&krate).into_iter().flatten() {
                    if !crates.contains(&dependent) {
                        return true;
                    }
                    if seen.insert(dependent) {
                        stack.push(dependent);
                    }
                }
            }
            false
        };
        let shared = crates.iter().copied().filter(is_shared).collect();
        let unbuilt =
            program.iter().copied().filter(|&krate| !symbols.rustc_built(krate)).collect();
        OwnStatics { crates, shared, theirs, unknown, unbuilt }
    }

    fn owns(&self, db: &dyn HirDatabase, id: StaticId) -> bool {
        let krate = id.module(db).krate(db);
        if self.unbuilt.contains(&krate) {
            return true;
        }
        self.crates.contains(&krate) && !self.theirs.contains(&id) && !self.unknown.contains(&krate)
    }

    /// Whether our copy of the static stands in for the one rustc exports. rustc's is what the
    /// static was when rustc last built the crate, which an edit to its value leaves behind.
    // FIXME: `#[used]` and `#[link_section]` statics are found by the linker or a runtime
    // through rustc's copy and should stay rustc's too.
    fn replaces(&self, db: &dyn HirDatabase, id: StaticId) -> bool {
        self.owns(db, id)
            && !self.shared.contains(&id.krate(db))
            && !AttrFlags::query(db, id.into()).contains(AttrFlags::NO_MANGLE)
            && AttrFlags::export_name(db, id.into()).is_none()
    }
}

/// Instances already found impossible to compile, kept between builds so a warm build does
/// not rediscover them one restart at a time.
/// An instance that could not be compiled, and what it was compiled for.
#[derive(Clone)]
pub(crate) struct Rejected {
    env: StoredParamEnvAndCrate,
    why: String,
}

/// What [`Rejections`] knows of the executable rustc linked, as a base to patch.
pub struct RustcBase(patch::Base);

/// The functions of compiler-builtins that compiled code calls for what Cranelift or LLVM has
/// no instruction of: 128-bit division and checked multiplication, and conversions between
/// 128-bit integers and floats, and halves. rustc's code may call none of them, and an
/// executable it linked then has none for a patch to call.
pub const BUILTINS: [&str; 18] = [
    "__divti3",
    "__udivti3",
    "__modti3",
    "__umodti3",
    "__rust_i128_mulo",
    "__rust_u128_mulo",
    "__muloti4",
    "__fixsfti",
    "__fixunssfti",
    "__fixdfti",
    "__fixunsdfti",
    "__floattisf",
    "__floatuntisf",
    "__floattidf",
    "__floatuntidf",
    "__extendhfsf2",
    "__truncsfhf2",
    "__truncdfhf2",
];

/// An object that defines nothing, for the target the process compiles for.
pub fn empty_object() -> Result<Vec<u8>> {
    patch::empty_object()
}

#[derive(Default, Clone)]
pub struct Rejections {
    rejected: FxHashMap<Instance, Rejected>,
    /// The statics that functions left to rustc refer to, and the crates where that is unknown.
    theirs: FxHashSet<StaticId>,
    unknown: FxHashSet<Crate>,
    /// Roots given up on because a function left to rustc handles the same future.
    left: FxHashSet<FunctionId>,
    /// The `async` blocks whose futures rustc's code makes: a poll or a drop of ours would read
    /// them as laid out by the other compiler.
    futures: FxHashSet<salsa::Id>,
    /// Whether to answer an edit with a patch for the object of the last full build.
    pub patching: bool,
    /// Whether rustc compiles a crate that uses the ones built, as it does the examples of a
    /// library: it then has code of its own for every function of theirs without a symbol.
    pub used_by_rustc: bool,
    base: Option<patch::Base>,
    /// The executable rustc linked, which an edit is a patch of until there is a `base`.
    rustc: Option<patch::Base>,
    /// Whether the next build is a full one though `rustc` could be patched: the one that
    /// makes a `base`, while nobody waits.
    full: bool,
    /// Why `rustc` was let go of by an edit, for the build in full it makes to say.
    forgot_rustc: Option<&'static str>,
    frozen: patch::Frozen,
    stale: Vec<Stale>,
    /// How many functions had been compiled when the last build finished.
    counted: usize,
    pub stats: BuildStats,
    /// The functions at a level above 1, which a patch has at that level.
    pub tiers: tier::Tiers,
    /// What the last full build replaced, while it can still be undone.
    undo: Option<Box<Rejections>>,
}

impl Rejections {
    /// Keeps the last full build: the next edit is a patch of it.
    pub fn keep_built(&mut self) {
        if self.undo.take().is_some() {
            self.tiers.clear();
        }
    }

    /// Puts back what the last full build replaced, when it was built undoably and replaced
    /// a build: the next edit is a patch of that one, as if the full build had not been made.
    /// Whether there was one to put back.
    pub fn undo_built(&mut self) -> bool {
        let Some(kept) = self.undo.take() else { return false };
        let (counted, stats) = (self.counted, self.stats);
        *self = *kept;
        (self.counted, self.stats) = (counted, stats);
        true
    }

    /// What these found could not be compiled, and nothing else, for
    /// [`Rejections::learn_from`].
    pub fn rejections(&self) -> Rejections {
        Rejections { rejected: self.rejected.clone(), ..Rejections::default() }
    }

    /// Knows what `other` found could not be compiled, for the first build of another program
    /// of the same database and the same code of rustc's: an instance fails the same way in
    /// each.
    pub fn learn_from(&mut self, other: &Rejections) {
        for (instance, rejected) in &other.rejected {
            self.rejected.entry(instance.clone()).or_insert_with(|| rejected.clone());
        }
    }

    /// Leaves what was compiled since the last build out of the next one's count: it was
    /// another build's, made before it.
    pub fn not_counted(&mut self) {
        self.compiled_since_last_build();
    }

    fn compiled_since_last_build(&mut self) -> usize {
        let total = COMPILED.load(std::sync::atomic::Ordering::Relaxed);
        total - std::mem::replace(&mut self.counted, total)
    }

    /// Looks at one part in `parts` of what the last full build compiled for what an edit
    /// changed, which threads can do a part each. `None` when no patch is to be made.
    /// `symbols` names what the last build had from the store.
    pub fn stale(
        &self,
        db: &dyn HirDatabase,
        symbols: Option<&dyn SymbolResolver>,
        part: usize,
        parts: usize,
    ) -> Option<Stale> {
        Some(self.patched()?.stale(db, symbols, part, parts))
    }

    /// What the next build is a patch of, if it is one.
    fn patched(&self) -> Option<&patch::Base> {
        self.base.as_ref().or(self.rustc.as_ref().filter(|it| !self.full && !it.outdated()))
    }

    /// Whether the next build is a patch of the last one, or of what rustc linked. A build
    /// that is interrupted leaves nothing to patch.
    pub fn patches(&self) -> bool {
        self.base.is_some() || self.patched().is_some()
    }

    /// Has the next builds patch the executable rustc linked, until one in full makes a base
    /// of ours: rustc compiled what the daemon reads. `debug` is whether a patch says which
    /// lines its code is of.
    pub fn start_from_rustc(&mut self, debug: bool) {
        self.rustc = Some(patch::Base::of_rustc(debug));
    }

    /// Whether the edits are patches of what rustc linked, which a build in full while
    /// nobody waits would replace with a base of ours.
    pub fn wants_base(&self) -> bool {
        self.base.is_none() && self.rustc.as_ref().is_some_and(|it| !it.outdated())
    }

    /// Makes the next build the full one that [`Rejections::wants_base`] asks for.
    pub fn build_base(&mut self) {
        self.full = true;
    }

    /// Whether the next build is the one [`Rejections::build_base`] asked for.
    pub fn builds_base(&self) -> bool {
        self.full && self.rustc.is_some() && self.base.is_none()
    }

    /// Takes what rustc linked as a base out, to keep it through what forgets the rest.
    pub fn take_rustc(&mut self) -> Option<RustcBase> {
        self.full = false;
        self.rustc.take().map(RustcBase)
    }

    pub fn put_rustc(&mut self, rustc: Option<RustcBase>) {
        self.rustc = rustc.map(|it| it.0);
    }

    /// Forgets what the builds found, for a program that is built as if never built: after a
    /// build that was interrupted or panicked, or once its checkout is loaded again. What
    /// rustc's copies are copies of is kept, since they are not built again: the first build
    /// takes the source as it is then for what rustc compiled, which after an edit it is not.
    pub fn start_again(&mut self) {
        let Rejections { patching, used_by_rustc, .. } = *self;
        let frozen = std::mem::take(&mut self.frozen);
        *self = Rejections { patching, used_by_rustc, frozen, ..Rejections::default() };
    }

    /// Puts a base taken out for a build back where it was: what rustc linked or ours.
    fn put_base(&mut self, base: patch::Base) {
        match base.rustc {
            true => self.rustc = Some(base),
            false => self.base = Some(base),
        }
    }

    /// Stops patching what rustc linked: the executable is no longer the one it linked.
    pub fn forget_rustc(&mut self) {
        (self.rustc, self.full) = (None, false);
    }

    /// [`Rejections::forget_rustc`] for an edit no patch of it can be, which the build in full
    /// that follows says.
    pub fn forget_rustc_for(&mut self, why: &'static str) {
        if self.base.is_none() && self.rustc.is_some() && !self.full {
            self.forgot_rustc = Some(why);
        }
        self.forget_rustc();
    }

    /// Notes what rustc compiled of `file`, which is about to change: the functions written
    /// in it and the statics declared there and in their bodies. Asked before the edit.
    /// `callers` are functions written elsewhere that call the generic ones of `file`, or
    /// call others that do: what rustc copied those into.
    pub fn home(
        &mut self,
        db: &dyn HirDatabase,
        symbols: &dyn SymbolResolver,
        file: base_db::FileId,
        functions: &[FunctionId],
        statics: &[StaticId],
        callers: &[FunctionId],
    ) {
        if self.base.is_none()
            && let Some(rustc) = &mut self.rustc
        {
            rustc.home(db, symbols, file, functions, statics, callers);
        }
    }

    /// Notes the first value of each static with state of `crates` and those that depend on
    /// them, which an edit is about to reach: as rustc built them, while no edit has.
    /// Asked before the edit.
    pub fn take_first_values(&mut self, db: &dyn HirDatabase, crates: &[Crate]) {
        let reached = crates.iter().flat_map(|it| it.transitive_rev_deps(db)).collect();
        self.frozen.take_values(db, reached);
    }

    /// Whether the patches of what rustc linked replace each of rustc's copies of the generic
    /// functions of the modules of `file` named `names`, which [`Self::home`] took in.
    pub fn copies_homed(&self, file: base_db::FileId, names: &[String]) -> bool {
        self.base.is_none() && self.rustc.as_ref().is_some_and(|it| it.copies_homed(file, names))
    }

    /// Whether [`Self::home`] has to note `file` yet.
    pub fn homes_file(&self, file: base_db::FileId) -> bool {
        self.homes() && self.rustc.as_ref().is_some_and(|it| !it.homed(file))
    }

    /// Whether a patch of what rustc linked has to know `file` before it changes.
    pub fn homes(&self) -> bool {
        self.base.is_none() && self.rustc.as_ref().is_some_and(|it| !it.outdated())
    }

    /// The function of the last full build that is defined under `symbol`, as the object has
    /// it (without the underscore Mach-O puts in front).
    pub fn tier_node(&self, symbol: &str) -> Option<tier::Node> {
        let base = self.base.as_ref()?;
        if let Some((root, _)) = base.compiled.iter().find(|(_, it)| it == symbol) {
            return Some(tier::Node(tier::Kind::Root(*root)));
        }
        if let Some((instance, known)) = base.instances.iter().find(|(_, it)| it.symbol == symbol) {
            return Some(tier::Node(tier::Kind::Instance(instance.clone(), known.env.clone())));
        }
        let (instance, env) = base.found_instances.get(symbol)?;
        Some(tier::Node(tier::Kind::Instance(instance.clone(), env.clone())))
    }

    /// The symbol `node` is defined under, as [`Rejections::tier_node`] takes it.
    pub fn tier_symbol(&self, node: &tier::Node) -> Option<String> {
        let base = self.base.as_ref()?;
        match &node.0 {
            tier::Kind::Root(root) => {
                base.compiled.iter().find(|(it, _)| it == root).map(|it| it.1.clone())
            }
            tier::Kind::Instance(instance, env) => {
                base.instances.get(instance).map(|it| it.symbol.clone()).or_else(|| {
                    let mut found = base.found_instances.iter();
                    found.find(|(_, it)| it.0 == *instance && it.1 == *env).map(|it| it.0.clone())
                })
            }
        }
    }

    /// Makes the next build compile the code of the checkout that the last one took from the
    /// store, which no patch can replace: built while nothing waits, the first edit after it
    /// is a patch. Whether the next build has that to do and no edit had asked for it.
    pub fn dehydrate(&mut self) -> bool {
        self.base.as_mut().is_some_and(|it| it.dehydrate())
    }

    /// Makes the next build a full one. A patch looks at everything the last build compiled,
    /// and what an edit took away can only be looked at where a panic can be caught.
    pub fn forget_base(&mut self) {
        self.base = None;
        self.forget_rustc();
    }

    /// Notes that rustc compiled `crates` from other source than they are first built from
    /// here, so that what it compiled of them and of those that see them is not to be linked.
    pub fn distrust(&mut self, db: &dyn HirDatabase, crates: &[Crate]) {
        if !crates.is_empty() {
            self.forget_rustc();
        }
        self.frozen.distrust(crates.iter().flat_map(|it| it.transitive_rev_deps(db)));
    }

    /// Notes that a file changed: one of `crates`, or with `None` one we can't place. Every
    /// change since the last build has to be noted before [`Rejections::stale`] looks.
    pub fn touch(&mut self, db: &dyn HirDatabase, crates: Option<&[Crate]>) {
        match crates {
            Some(crates) => {
                self.frozen.edit(crates.iter().flat_map(|it| it.transitive_rev_deps(db)))
            }
            None => self.frozen.edit(base_db::all_crates(db).iter().copied()),
        }
        for base in [&mut self.base, &mut self.rustc].into_iter().flatten() {
            base.touch(db, crates);
        }
        self.forget();
    }

    /// [`Rejections::touch`] with `None`, for a change to files that `include!`,
    /// `include_str!` or `include_bytes!` read: what was compiled of any crate can be of
    /// them, but a first value of a static only of `includers`, the crates that read them,
    /// and of what depends on them.
    pub fn touch_included(&mut self, db: &dyn HirDatabase, includers: &[Crate]) {
        let values = includers.iter().flat_map(|it| it.transitive_rev_deps(db));
        self.frozen.edit_apart(base_db::all_crates(db).iter().copied(), values);
        for base in [&mut self.base, &mut self.rustc].into_iter().flatten() {
            base.touch(db, None);
        }
        self.forget();
    }

    /// Notes that files rustc read, as it read them, were taken in for the first time: a build
    /// of the daemon's own compiled without them, and what rustc linked is of them.
    pub fn touch_own(&mut self, db: &dyn HirDatabase) {
        self.frozen.edit(base_db::all_crates(db).iter().copied());
        if let Some(base) = &mut self.base {
            base.touch(db, None);
        }
        self.forget();
    }

    /// What was found impossible to compile was found so in the source before the edit, and
    /// the edit can be what mends it.
    fn forget(&mut self) {
        self.rejected.clear();
        self.theirs.clear();
        self.unknown.clear();
        self.left.clear();
        self.futures.clear();
    }

    /// Notes that functions were added to a file of `crates`, or taken away, and nothing else of
    /// it changed but bodies that nothing else sees into, where no other crate depends on
    /// `crates`, no file of them named the new functions before (or names those taken away
    /// after), and `files` are their files: a patch of what rustc linked looks at all of their
    /// code again.
    pub fn touch_added(
        &mut self,
        db: &dyn HirDatabase,
        crates: &[Crate],
        files: &[base_db::FileId],
    ) {
        self.frozen.edit(crates.iter().flat_map(|it| it.transitive_rev_deps(db)));
        if let Some(base) = &mut self.base {
            base.touch(db, Some(crates));
        }
        if let Some(rustc) = &mut self.rustc {
            rustc.touch_added(db, crates, files);
        }
        self.forget();
    }

    /// Notes that a file changed in the bodies of functions alone, none of which the rest of
    /// the program sees into: not `const` or `async`, not returning `impl Trait`, declaring no
    /// items, and not handed to a macro.
    pub fn touch_bodies(&mut self, db: &dyn HirDatabase, file: base_db::FileId) {
        self.frozen.edit_bodies(file);
        for base in [&mut self.base, &mut self.rustc].into_iter().flatten() {
            base.touch_bodies(db, file);
        }
        self.forget();
    }

    /// A function the last build left to rustc, or an instance it calls rustc's copy of, that
    /// is not what rustc compiled, and why it was left. rustc's copies are those of the build
    /// the daemon started from: the first call, to be made after that build, notes what they
    /// are copies of.
    pub fn out_of_date(&mut self, db: &dyn HirDatabase) -> Option<OutOfDate> {
        self.frozen.out_of_date(db, self.base.as_ref()?)
    }

    /// Hands the next build what [`Rejections::stale`] found, every part of it and with no
    /// edit in between.
    pub fn set_stale(&mut self, db: &dyn HirDatabase, mut stale: Vec<Stale>) {
        let patched = self.patched().is_some();
        if let Some(base) = &mut self.base {
            base.settle(db, &mut stale);
        } else if let Some(rustc) = self.rustc.as_mut().filter(|_| patched) {
            rustc.settle(db, &mut stale);
        }
        self.stale = stale;
    }

    /// The longest any thread took over each step of looking for what is stale.
    pub fn stale_steps(&self) -> StaleSteps {
        let longest = |step: fn(&StaleSteps) -> std::time::Duration| {
            self.stale.iter().map(|it| step(&it.steps)).max().unwrap_or_default()
        };
        StaleSteps {
            roots: longest(|it| it.roots),
            instances: longest(|it| it.instances),
            statics: longest(|it| it.statics),
            left: longest(|it| it.left),
            roots_asked: self.stale.iter().map(|it| it.steps.roots_asked).sum(),
            instances_asked: self.stale.iter().map(|it| it.steps.instances_asked).sum(),
            roots_unchanged: self.stale.iter().map(|it| it.steps.roots_unchanged).sum(),
            instances_unchanged: self.stale.iter().map(|it| it.steps.instances_unchanged).sum(),
            slowest: longest(|it| it.slowest),
        }
    }
}

/// Measurements of the last build.
#[derive(Default, Clone, Copy)]
pub struct BuildStats {
    /// Compiled functions in the object.
    pub bodies: usize,
    /// How many of them were compiled before this build, and how many during it.
    pub reused: usize,
    pub compiled: usize,
    /// Where the last round of the build spent its time.
    pub phases: Phases,
    /// How many times stitching started over because a callee turned out not to compile.
    pub rounds: usize,
    /// How many functions the patch defines, when the build made one.
    pub patched: Option<usize>,
    /// Why the build wrote the whole object again, when a patch was tried.
    pub declined: Option<&'static str>,
    /// Whether the patch is of the executable rustc linked, which only a splice adds it to:
    /// a link puts rustc's objects behind it, whose calls within themselves stay there.
    pub over_rustc: bool,
    /// Whether the build in full names what only the executable rustc linked has, for it is
    /// spliced into that rather than linked: the symbol resolver says.
    pub onto_rustcs: bool,
}

/// Time spent in each step of the last round.
#[derive(Default, Clone, Copy)]
pub struct Phases {
    /// Asking for each compiled function: checking that what it was compiled from is unchanged,
    /// and compiling it when not.
    pub compile: std::time::Duration,
    /// Finding what each compiled function refers to in the object.
    pub resolve: std::time::Duration,
    /// Handing the code to the object.
    pub define: std::time::Duration,
    /// Writing the object file.
    pub object: std::time::Duration,
    /// Working out what the functions left to rustc keep the rest from compiling.
    pub fallback: std::time::Duration,
    /// The rounds before the last, whose object was not kept.
    pub discarded: std::time::Duration,
    /// Telling which statics are the program's own (of a patch).
    pub statics: std::time::Duration,
    /// Setting up the object to put the code in (of a patch).
    pub setup: std::time::Duration,
}

/// A function with concrete generic arguments that we compile ourselves.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    Closure(salsa::Id),
    /// An `InternedCoroutineId`: the `Future::poll` of an `async` block. Polled with a null
    /// context, it drops whatever the block holds instead.
    Coroutine(salsa::Id),
    /// The same of the future an `async` closure gives when it is called by value though it
    /// could be called again: that future owns what the closure captured.
    CoroutineByMove(salsa::Id),
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

/// What a call transfers control to.
enum Callee<'db> {
    Instance(InstanceDef, GenericArgs<'db>),
    /// With how many of the arguments are for named parameters, when the rest are for `...`.
    Pointer(Value, ExternAbi, Option<usize>),
}

/// How rustc's demangled symbols spell the types and instances of the program.
struct Spelling<'a, 'db> {
    db: &'db dyn HirDatabase,
    symbols: &'a dyn SymbolResolver,
}

impl<'db> Spelling<'_, 'db> {
    /// A generic function with its arguments as it reads in a demangled rustc symbol:
    /// `<Vec<u8> as Clone>::clone`, `<Option<u8>>::map::<u8, F>`, `core::mem::swap::<u8>`.
    fn instance_path(&self, func: FunctionId, args: GenericArgs<'db>) -> Option<String> {
        let db = self.db;
        let signature = FunctionSignature::of(db, func);
        // The future of an `async fn` is laid out differently by the two compilers.
        if signature.is_async() {
            return None;
        }
        let name = signature.name.as_str();
        let interner = DbInterner::new_no_crate(db);
        let types = |args: &[hir_ty::next_solver::GenericArg<'db>]| {
            args.iter()
                .filter(|it| it.region().is_none())
                .map(|it| self.symbol_arg(*it))
                .collect::<Option<Vec<_>>>()
        };
        let parent = hir_ty::generics::generics(db, func.into()).len_parent();
        let (parent, own) = args.as_slice().split_at_checked(parent)?;
        let mut path = match func.lookup(db).container {
            ItemContainerId::ModuleId(_) => self.symbols.item(func.into())?,
            ItemContainerId::TraitId(trait_) => {
                let (self_ty, rest) = parent.split_first()?;
                let mut path = format!(
                    "<{} as {}",
                    self.symbol_type(self_ty.ty()?)?,
                    self.symbols.item(trait_.into())?
                );
                let rest = types(rest)?;
                if !rest.is_empty() {
                    path.push_str(&format!("<{}>", rest.join(", ")));
                }
                format!("{path}>::{name}")
            }
            ItemContainerId::ImplId(impl_) => {
                let parent_args = GenericArgs::new_from_slice(parent);
                let self_ty =
                    db.impl_self_ty(impl_).instantiate(interner, parent_args).skip_norm_wip();
                let self_ty = self.symbol_type(self_ty)?;
                match db.impl_trait(impl_) {
                    None => format!("<{self_ty}>::{name}"),
                    Some(trait_ref) => {
                        let trait_ref =
                            trait_ref.instantiate(interner, parent_args).skip_norm_wip();
                        let mut path = format!(
                            "<{self_ty} as {}",
                            self.symbols.item(trait_ref.def_id.0.into())?
                        );
                        let rest = types(&trait_ref.args.as_slice()[1..])?;
                        if !rest.is_empty() {
                            path.push_str(&format!("<{}>", rest.join(", ")));
                        }
                        format!("{path}>::{name}")
                    }
                }
            }
            ItemContainerId::ExternBlockId(_) => return None,
        };
        let own = types(own)?;
        if !own.is_empty() {
            path.push_str(&format!("::<{}>", own.join(", ")));
        }
        Some(path)
    }

    /// The arguments of the instance of `func` that [`Self::instance_path`] spells `path`, for
    /// a function whose parent has no types for parameters: rustc's copy says what it is of.
    /// Lifetimes are erased; a type this does not read back declines.
    fn instance_args(&self, func: FunctionId, path: &str) -> Option<GenericArgs<'db>> {
        let (_, own) = path.strip_suffix('>')?.rsplit_once("::<")?;
        let mut own = split_top(own).into_iter().map(|it| self.read_type(it));
        let interner = DbInterner::new_no_crate(self.db);
        let erased = Region::new_erased(interner);
        let mut read = true;
        let parent = hir_ty::generics::generics(self.db, func.into()).len_parent();
        let args = GenericArgs::for_item(interner, func.into(), |at, param, _, _| {
            match (param, at as usize >= parent) {
                (hir_def::GenericParamId::LifetimeParamId(_), _) => return erased.into(),
                (hir_def::GenericParamId::TypeParamId(_), true) => {
                    if let Some(Some(ty)) = own.next() {
                        return ty.into();
                    }
                }
                _ => {}
            }
            read = false;
            erased.into()
        });
        let all = read && own.next().is_none();
        (all && self.instance_path(func, args).as_deref() == Some(path)).then_some(args)
    }

    /// A type as [`Self::symbol_type`] spells it, of those that are not `dyn`, closures or
    /// function pointers.
    fn read_type(&self, text: &str) -> Option<Ty<'db>> {
        let interner = DbInterner::new_no_crate(self.db);
        let erased = Region::new_erased(interner);
        let builtin = match text {
            "bool" => Some(TyKind::Bool),
            "char" => Some(TyKind::Char),
            "str" => Some(TyKind::Str),
            "!" => Some(TyKind::Never),
            _ => None,
        };
        if let Some(kind) = builtin {
            return Some(Ty::new(interner, kind));
        }
        let numbers = [
            ("i8", TyKind::Int(rustc_type_ir::IntTy::I8)),
            ("i16", TyKind::Int(rustc_type_ir::IntTy::I16)),
            ("i32", TyKind::Int(rustc_type_ir::IntTy::I32)),
            ("i64", TyKind::Int(rustc_type_ir::IntTy::I64)),
            ("i128", TyKind::Int(rustc_type_ir::IntTy::I128)),
            ("isize", TyKind::Int(rustc_type_ir::IntTy::Isize)),
            ("u8", TyKind::Uint(UintTy::U8)),
            ("u16", TyKind::Uint(UintTy::U16)),
            ("u32", TyKind::Uint(UintTy::U32)),
            ("u64", TyKind::Uint(UintTy::U64)),
            ("u128", TyKind::Uint(UintTy::U128)),
            ("usize", TyKind::Uint(UintTy::Usize)),
            ("f16", TyKind::Float(rustc_type_ir::FloatTy::F16)),
            ("f32", TyKind::Float(rustc_type_ir::FloatTy::F32)),
            ("f64", TyKind::Float(rustc_type_ir::FloatTy::F64)),
            ("f128", TyKind::Float(rustc_type_ir::FloatTy::F128)),
        ];
        if let Some((_, kind)) = numbers.into_iter().find(|(name, _)| *name == text) {
            return Some(Ty::new(interner, kind));
        }
        let pointers = [
            ("&mut ", None, Mutability::Mut),
            ("&", None, Mutability::Not),
            ("*mut ", Some(()), Mutability::Mut),
            ("*const ", Some(()), Mutability::Not),
        ];
        for (prefix, raw, mutability) in pointers {
            if let Some(pointee) = text.strip_prefix(prefix) {
                let pointee = self.read_type(pointee)?;
                return Some(match raw {
                    Some(()) => Ty::new_ptr(interner, pointee, mutability),
                    None => Ty::new_ref(interner, erased, pointee, mutability),
                });
            }
        }
        if let Some(inner) = text.strip_prefix('[').and_then(|it| it.strip_suffix(']')) {
            return match split_top_on(inner, "; ").as_slice() {
                [element] => Some(Ty::new_slice(interner, self.read_type(element)?)),
                [element, len] => {
                    Some(Ty::new_array(interner, self.read_type(element)?, len.parse().ok()?))
                }
                _ => None,
            };
        }
        if let Some(inner) = text.strip_prefix('(').and_then(|it| it.strip_suffix(')')) {
            let inner = inner.strip_suffix(',').unwrap_or(inner);
            let tys = split_top(inner).into_iter().map(|it| self.read_type(it));
            return Some(Ty::new_tup(interner, &tys.collect::<Option<Vec<_>>>()?));
        }
        if text.starts_with(['<', '{']) || text.starts_with("fn(") || text.starts_with("dyn ") {
            return None;
        }
        let (path, args) = match text.split_once('<') {
            Some((path, args)) => (path, split_top(args.strip_suffix('>')?)),
            None => (text, Vec::new()),
        };
        let adt = self.symbols.adt_at(path)?;
        let mut spelled = args.into_iter().map(|it| self.read_type(it));
        let defaults = self.db.generic_defaults(adt.into());
        let mut read = true;
        let args = GenericArgs::for_item(interner, adt.into(), |at, param, _, before| {
            match param {
                hir_def::GenericParamId::LifetimeParamId(_) => return erased.into(),
                hir_def::GenericParamId::TypeParamId(_) => match spelled.next() {
                    Some(Some(ty)) => return ty.into(),
                    Some(None) => {}
                    // rustc leaves out the arguments that say what the default says.
                    None => {
                        if let Some(default) = defaults.get(at as usize) {
                            return default.instantiate(interner, before).skip_norm_wip();
                        }
                    }
                },
                hir_def::GenericParamId::ConstParamId(_) => {}
            }
            read = false;
            erased.into()
        });
        (read && spelled.next().is_none()).then(|| Ty::new_adt(interner, adt, args))
    }

    /// A type or a `usize` an instance is instantiated with, as rustc's symbols spell it.
    fn symbol_arg(&self, arg: hir_ty::next_solver::GenericArg<'db>) -> Option<String> {
        match (arg.ty(), arg.konst()) {
            (Some(ty), _) => self.symbol_type(ty),
            (None, Some(len)) => Some(try_const_usize(self.db, len)?.to_string()),
            (None, None) => None,
        }
    }

    /// A type as it reads in a demangled rustc symbol. `None` for the types whose spelling
    /// there this does not know: closures, `dyn`, function pointers, anything with a lifetime
    /// argument.
    fn symbol_type(&self, ty: Ty<'db>) -> Option<String> {
        use std::fmt::Write;
        let db = self.db;
        let list = |tys: &mut dyn Iterator<Item = Ty<'db>>| {
            tys.map(|it| self.symbol_type(it)).collect::<Option<Vec<_>>>().map(|it| it.join(", "))
        };
        Some(match ty.kind() {
            TyKind::Bool => "bool".to_owned(),
            TyKind::Char => "char".to_owned(),
            TyKind::Str => "str".to_owned(),
            TyKind::Never => "!".to_owned(),
            TyKind::Int(it) => it.name_str().to_owned(),
            TyKind::Uint(it) => it.name_str().to_owned(),
            TyKind::Float(it) => it.name_str().to_owned(),
            TyKind::Ref(_, pointee, Mutability::Not) => format!("&{}", self.symbol_type(pointee)?),
            TyKind::Ref(_, pointee, Mutability::Mut) => {
                format!("&mut {}", self.symbol_type(pointee)?)
            }
            TyKind::RawPtr(pointee, Mutability::Not) => {
                format!("*const {}", self.symbol_type(pointee)?)
            }
            TyKind::RawPtr(pointee, Mutability::Mut) => {
                format!("*mut {}", self.symbol_type(pointee)?)
            }
            TyKind::Slice(element) => format!("[{}]", self.symbol_type(element)?),
            TyKind::Array(element, len) => {
                format!("[{}; {}]", self.symbol_type(element)?, try_const_usize(db, len)?)
            }
            TyKind::Tuple(tys) if tys.len() == 1 => format!("({},)", list(&mut tys.iter())?),
            TyKind::Tuple(tys) => format!("({})", list(&mut tys.iter())?),
            TyKind::Adt(adt, args) => {
                let mut path = self.symbols.item(adt.def_id().into())?;
                let interner = DbInterner::new_no_crate(db);
                let defaults = db.generic_defaults(adt.def_id().into());
                let mut args = args.as_slice();
                // rustc leaves out the arguments that say what the default says.
                while let Some((last, rest)) = args.split_last()
                    && let Some(default) = defaults.get(rest.len())
                    && default.instantiate(interner, rest).skip_norm_wip() == *last
                {
                    args = rest;
                }
                let args = args.iter().filter(|it| it.region().is_none());
                let args = args.map(|it| self.symbol_arg(*it)).collect::<Option<Vec<_>>>()?;
                if !args.is_empty() {
                    write!(path, "<{}>", args.join(", ")).ok()?;
                }
                path
            }
            TyKind::FnPtr(..) => {
                let sig = ty.fn_sig(DbInterner::new_no_crate(db)).no_bound_vars()?;
                if sig.abi() != ExternAbi::Rust
                    || sig.c_variadic()
                    || sig.safety() != hir_ty::next_solver::abi::Safety::Safe
                {
                    return None;
                }
                let mut path = format!("fn({})", list(&mut sig.inputs().iter().copied())?);
                if !sig.output().is_unit() {
                    write!(path, " -> {}", self.symbol_type(sig.output())?).ok()?;
                }
                path
            }
            TyKind::Dynamic(bounds, _) => {
                let principal = bounds.principal()?.no_bound_vars()?;
                let mut path = format!("dyn {}", self.symbols.item(principal.def_id.0.into())?);
                let args = principal.args.iter().filter(|it| it.region().is_none());
                let mut args = args.map(|it| self.symbol_arg(it)).collect::<Option<Vec<_>>>()?;
                for bound in bounds.projection_bounds() {
                    let bound = bound.no_bound_vars()?;
                    let hir_ty::next_solver::TermId::TypeAliasId(alias) = bound.def_id.0 else {
                        return None;
                    };
                    let name = hir_def::signatures::TypeAliasSignature::of(db, alias).name.clone();
                    let ty = rustc_type_ir::inherent::Term::as_type(&bound.term)?;
                    args.push(format!("{} = {}", name.as_str(), self.symbol_type(ty)?));
                }
                if !args.is_empty() {
                    write!(path, "<{}>", args.join(", ")).ok()?;
                }
                for auto in bounds.auto_traits() {
                    write!(path, " + {}", self.symbols.item(auto.0.into())?).ok()?;
                }
                path
            }
            _ => return None,
        })
    }
}

/// The parts of a list of types that `, ` separates outside of brackets.
fn split_top(text: &str) -> Vec<&str> {
    if text.is_empty() { Vec::new() } else { split_top_on(text, ", ") }
}

fn split_top_on<'t>(text: &'t str, separator: &str) -> Vec<&'t str> {
    let (mut parts, mut depth, mut start) = (Vec::new(), 0i32, 0);
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' | b')' | b']' => depth -= 1,
            _ if depth == 0 && text[at..].starts_with(separator) => {
                parts.push(&text[start..at]);
                at += separator.len();
                start = at;
                continue;
            }
            _ => {}
        }
        at += 1;
    }
    parts.push(&text[start..]);
    parts
}

/// Stitches compiled functions into one object: gives every function and data object they
/// ask for an id in the module, and defines the ones that are ours.
struct ObjectCodegen<'a, 'db> {
    db: &'db dyn HirDatabase,
    symbols: &'a dyn SymbolResolver,
    module: ObjectModule,
    unwind: UnwindContext,
    /// What a debugger is told about the functions, when the build tells one anything.
    debug: Option<debuginfo::Describer>,
    /// What those types are told as, and the ones the threads that compile told of.
    told: ToldTypes,
    /// Writes what is kept to the store, once there is something to keep.
    writer: Option<cached::Writer>,
    imports: FxHashMap<String, FuncId>,
    /// The functions of `asm!` blocks, by their assembly.
    assemblies: FxHashMap<String, FuncId>,
    instances: FxHashMap<Instance, FuncId>,
    pending: Vec<(Instance, StoredParamEnvAndCrate, FuncId)>,
    /// Every instance that went through `pending`.
    envs: Vec<(Instance, StoredParamEnvAndCrate)>,
    homes: home::Homes,
    /// What [`Self::compile_ahead`] has compiled or tried to.
    ahead: FxHashSet<(Instance, StoredParamEnvAndCrate)>,
    /// The code of the roots and instances compiled in units ([`units`]), yet to be put in
    /// the object, and the roots units compiled or tried to.
    unit_roots: FxHashMap<FunctionId, CompiledFunction>,
    unit_instances: FxHashMap<Instance, CompiledFunction>,
    unit_tried: FxHashSet<FunctionId>,
    /// The instances that call rustc's copy instead of having code of their own.
    forwarded: FxHashSet<Instance>,
    /// [`Rejections::futures`] as the round started.
    rustc_futures: FxHashSet<salsa::Id>,
    /// Whether instances and statics get names another object can refer to.
    patching: bool,
    /// The object this one is a patch for, and what of it is no longer what the source says.
    base: Option<&'a patch::Base>,
    replaced: patch::Unchanged,
    /// The statics given a value here, in the order their values were asked for.
    own_static_order: Vec<StaticId>,
    /// Functions compiled only once something refers to them, and the ones referred to since
    /// the list was last taken.
    on_demand: &'a FxHashSet<FunctionId>,
    wanted: Vec<FunctionId>,
    on_demand_crates: FxHashSet<Crate>,
    /// [`Self::on_demand`] by crate, made when first asked for, and the crates all of whose
    /// functions are wanted already: once queued, a function is not queued again, so a crate
    /// is gone through once, not once per static or root that may call into it.
    on_demand_by_crate: Option<FxHashMap<Crate, Vec<FunctionId>>>,
    crates_wanted: FxHashSet<Crate>,
    /// Instances that failed in an earlier round, with the reason.
    rejected: &'a FxHashMap<Instance, Rejected>,
    /// Size and alignment of every type laid out, collected when `RA_JIT_LAYOUTS` names a file
    /// to write them to. The harness compares them with rustc's.
    layouts: FxHashMap<String, (u64, u64)>,
    phases: Phases,
    /// One line per function for `harness/shapes.py`, when `RA_JIT_SHAPES` is set.
    shapes: Option<std::io::BufWriter<std::fs::File>>,
    /// How many compiled functions went into the object.
    bodies: usize,
    vtables: FxHashMap<(StoredTy, Option<(TraitId, StoredGenericArgs)>), DataId>,
    own_statics: OwnStatics,
    /// The crates of the program, when they do not all check overflow alike.
    mixed: Option<settled::Mixed>,
    /// What `finish` adds to the object of the relocations of the functions defined.
    elf_relocs: Vec<ElfReloc>,
    /// Symbols an `extern_weak` static points to, which `finish` makes weak: absent, they are 0.
    weak_imports: Vec<cranelift_module::FuncOrDataId>,
    /// Functions exported before their code failed, which nothing defines yet.
    unstitched: FxHashSet<FuncId>,
    /// Functions `finish` hides from other objects: see [`ObjectCodegen::define_refused`].
    hidden: Vec<FuncId>,
    /// The messages of the stand-ins, which `finish` puts in [`stand_in_section`].
    stand_ins: Vec<(DataId, Box<[u8]>)>,
    /// The bytes of each `#[link_section]` static of a wasm object, and the custom section
    /// they go in.
    custom_sections: Vec<(String, Vec<u8>)>,
    statics: FxHashMap<StaticId, DataId>,
    /// Statics nothing exports and nothing can write to, which we give a copy of the value.
    frozen_statics: FxHashMap<StaticId, DataId>,
    imported_statics: FxHashMap<StaticId, DataId>,
    /// Those of `imported_statics` that are rustc's for their state alone, which a base
    /// keeps to tell an edit to their first value by.
    shared_statics: FxHashMap<StaticId, DataId>,
    /// rustc's first values of statics with state, to tell whether rustc's copy is still
    /// of the source.
    rustc_values: patch::RustcValues,
    consts: FxHashMap<ConstBlob, DataId>,
    source_paths: FxHashMap<base_db::FileId, Option<String>>,
    builder_ctx: FunctionBuilderContext,
    location_counter: u32,
    names: stable::StableNames<'a, 'db>,
    /// Whether the code of the checkout's crates can come from the store.
    hydrate: bool,
    stable_names: FxHashMap<Instance, Option<StableName>>,
    kept: Kept,
    /// The functions in the object whose description and entry of the store are still to
    /// be made ([`later`]).
    later: Vec<later::Later<'db>>,
    /// The entries of the store to build from in place of compiling, if any.
    entries: Option<std::sync::Arc<cached::Entries>>,
    /// The functions of this object by their stable names.
    named: FxHashMap<StableName, FuncId>,
    /// The ones declared for an entry, and those of them yet to be put in the object.
    found: Vec<(StableName, FuncId)>,
    found_pending: Vec<(StableName, FuncId)>,
    named_vtables: FxHashMap<String, DataId>,
    /// The `TypeId::of` of each hash, and back.
    type_id_shims: FxHashMap<u128, FuncId>,
    type_ids: FxHashMap<FuncId, u128>,
    /// An entry did not fit the object, which now lacks a function it declares.
    misfit: bool,
    /// What stops the build ([`CodegenError::Refused`]), found by a thread that can't return it.
    refused: Option<CodegenError>,
    /// The functions of a patch that are at a level above 1, whose code it has at that level.
    tiers: Option<&'a tier::Tiers>,
    /// What each function built from an entry of the store is an instance of.
    found_instances: FxHashMap<FuncId, (Instance, StoredParamEnvAndCrate)>,
    /// Whether the store has the code of a root whole, by its name, for the roots asked
    /// about: those it has are put in the object from it, not compiled.
    stored_roots: FxHashMap<FunctionId, Option<StableName>>,
    /// The root each name of a root is of, as [`ObjectCodegen::find_stored_roots`] named
    /// them; `None` for a name more than one has.
    root_names: FxHashMap<StableName, Option<FunctionId>>,
}

/// Why an entry with a file of its own under what stands for cargo's directory is not kept.
const OF_CARGO: &str = "a file under what stands for cargo's directory";

/// What became of the instances an object was built from, as entries of the store.
#[derive(Default)]
struct Kept {
    added: usize,
    found: usize,
    /// Instances that name something an edit can reach.
    unnamed: usize,
    /// Instances that refer to something an entry has no name for.
    unnamed_requests: usize,
    /// Entries of the store that differ from what was compiled, with `RA_JIT_CACHE_CHECK`.
    differing: usize,
    same: usize,
    /// Why instances with a name were not added.
    unkept: FxHashMap<&'static str, usize>,
    /// Why instances with a name were compiled.
    unfound: FxHashMap<&'static str, usize>,
    /// Why entries of the checkout were kept with no mark, and the first of each.
    untracked: FxHashMap<&'static str, (usize, Box<str>)>,
}

impl Kept {
    fn merge(&mut self, other: Kept) {
        let Kept {
            added,
            found,
            unnamed,
            unnamed_requests,
            differing,
            same,
            unkept,
            unfound,
            untracked,
        } = other;
        self.added += added;
        self.found += found;
        self.unnamed += unnamed;
        self.unnamed_requests += unnamed_requests;
        self.differing += differing;
        self.same += same;
        for (why, count) in unkept {
            *self.unkept.entry(why).or_default() += count;
        }
        for (why, count) in unfound {
            *self.unfound.entry(why).or_default() += count;
        }
        for (why, (count, first)) in untracked {
            self.untracked.entry(why).or_insert((0, first)).0 += count;
        }
    }

    fn untracked(&mut self, name: &str, why: &'static str) {
        self.untracked.entry(why).or_insert_with(|| (0, name.into())).0 += 1;
    }

    fn written(&mut self, written: cached::Written) {
        self.added += written.added;
        if written.of_cargo != 0 {
            *self.unkept.entry(OF_CARGO).or_default() += written.of_cargo;
        }
    }

    fn unfound<T>(&mut self, why: &'static str) -> Option<T> {
        *self.unfound.entry(why).or_default() += 1;
        None
    }

    fn unkept<T>(&mut self, why: &'static str) -> Option<T> {
        *self.unkept.entry(why).or_default() += 1;
        None
    }

    fn report(&self) {
        let Kept {
            added,
            found,
            unnamed,
            unnamed_requests,
            differing,
            same,
            unkept,
            unfound,
            untracked,
        } = self;
        for (why, (count, first)) in untracked {
            eprintln!("kept with no mark: {count}, {why}, as `{first}`");
        }
        if added + found + unnamed + unnamed_requests + differing + same != 0 {
            eprintln!(
                "entries: {found} found, {added} added, {same} as stored, {differing} not as stored, \
                 {unnamed} instances and {unnamed_requests} requests without a stable name"
            );
        }
        if !unkept.is_empty() {
            let mut unkept: Vec<_> = unkept.iter().collect();
            unkept.sort_unstable_by_key(|it| std::cmp::Reverse(it.1));
            eprintln!("not kept: {unkept:?}");
        }
        if !unfound.is_empty() {
            let mut unfound: Vec<_> = unfound.iter().collect();
            unfound.sort_unstable_by_key(|it| std::cmp::Reverse(it.1));
            eprintln!("not found: {unfound:?}");
        }
    }
}

/// The ids in a module of what a compiled function or static asked for.
struct Resolved {
    funcs: Vec<FuncId>,
    datas: Vec<DataId>,
}

impl Resolved {
    fn blob(&self, blob: &ConstBlob) -> ConstBlob {
        ConstBlob {
            bytes: blob.bytes.clone(),
            relocs: blob
                .relocs
                .iter()
                .map(|&(offset, target, addend)| {
                    (offset, self.datas[target.as_u32() as usize], addend)
                })
                .collect(),
            fn_relocs: blob
                .fn_relocs
                .iter()
                .map(|&(offset, target)| (offset, self.funcs[target.as_u32() as usize]))
                .collect(),
            align: blob.align,
            type_ids: blob.type_ids.clone(),
        }
    }

    fn reloc(&self, reloc: &ModuleReloc, own: FuncId) -> ModuleReloc {
        let name = match reloc.name {
            ModuleRelocTarget::User { namespace: 0, index } => {
                ModuleRelocTarget::user(0, self.funcs[index as usize].as_u32())
            }
            ModuleRelocTarget::User { index, .. } => {
                ModuleRelocTarget::user(1, self.datas[index as usize].as_u32())
            }
            ModuleRelocTarget::FunctionOffset(_, offset) => {
                ModuleRelocTarget::FunctionOffset(own, offset)
            }
            ref other => other.clone(),
        };
        ModuleReloc { name, ..reloc.clone() }
    }
}

/// The code generator of the one [`Target`] the process compiles for.
fn target() -> Result<&'static isa::OwnedTargetIsa> {
    static TARGET: std::sync::OnceLock<std::result::Result<isa::OwnedTargetIsa, String>> =
        std::sync::OnceLock::new();
    TARGET
        .get_or_init(|| Target::selected()?.isa(regalloc(), false))
        .as_ref()
        .map_err(|e| CodegenError::Backend(e.clone()))
}

/// The register allocator of a function that is not over [`SINGLE_PASS`]: `RA_JIT_REGALLOC` names one of
/// Cranelift's, `backtracking` (the default) or `single_pass`. The second allocates in one
/// walk over the code: it took a fifth off Cranelift's time on cold builds of ripgrep and of
/// just, and none off their wall time, and its code was twice the size and up to 1.8 times
/// slower (the demo's Life step). Code in the store is kept whichever made it, so compare the
/// two with a store of their own each.
fn regalloc() -> &'static str {
    static REGALLOC: LazyLock<&'static str> =
        LazyLock::new(|| match std::env::var("RA_JIT_REGALLOC").as_deref() {
            Ok("single_pass") => "single_pass",
            _ => "backtracking",
        });
    *REGALLOC
}

/// The same target for a function of more than [`SINGLE_PASS`] instructions. The usual
/// register allocator takes more than linear time: ten seconds for one of 150 000
/// instructions, which a whole build then waits for.
fn target_for_large() -> Result<&'static isa::OwnedTargetIsa> {
    static TARGET: std::sync::OnceLock<std::result::Result<isa::OwnedTargetIsa, String>> =
        std::sync::OnceLock::new();
    TARGET
        .get_or_init(|| Target::selected()?.isa("single_pass", false))
        .as_ref()
        .map_err(|e| CodegenError::Backend(e.clone()))
}

/// The same target for a function that asks for its frame pointer at its start, as one
/// that takes `...` does to find the arguments on the stack: Cranelift answers only where
/// every function keeps a frame pointer, which the others have no use for.
fn target_with_frame_pointer() -> Result<&'static isa::OwnedTargetIsa> {
    static TARGET: std::sync::OnceLock<std::result::Result<isa::OwnedTargetIsa, String>> =
        std::sync::OnceLock::new();
    TARGET
        .get_or_init(|| Target::selected()?.isa(regalloc(), true))
        .as_ref()
        .map_err(|e| CodegenError::Backend(e.clone()))
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
/// with it and 2.5 KB without, and hir-ide's `recursive_vars_2` overflowed its 2 MB stack. The
/// largest functions of blaze's own hir-ide tests, up to 128 217 instructions, took at most
/// 85 ms each with the usual one.
const SINGLE_PASS: usize = 50_000;

/// The most cases of a `match` lowered as a chain of comparisons for LLVM to make a `switch`
/// of; a larger one, which a function Cranelift compiles after all would test one case at a
/// time, keeps `Switch`'s tree.
const CHAINED: usize = 256;

/// A relocation of a function that Cranelift has no kind for, which the object is given
/// once Cranelift has written it: see [`Code::elf_types`].
struct ElfReloc {
    func: FuncId,
    reloc: ModuleReloc,
    r_type: u32,
}

impl ElfReloc {
    fn add(
        &self,
        product: &mut cranelift_object::ObjectProduct,
    ) -> std::result::Result<(), String> {
        use cranelift_object::object::{self, write};
        if product.object.format() != object::BinaryFormat::Elf {
            return Err("a relocation by its ELF type in an object that is not ELF".to_owned());
        }
        let symbol = match self.reloc.name {
            ModuleRelocTarget::User { namespace: 0, index } => {
                product.function_symbol(FuncId::from_u32(index))
            }
            ModuleRelocTarget::User { namespace: 1, index } => {
                product.data_symbol(DataId::from_u32(index))
            }
            ref other => return Err(format!("a relocation by its ELF type against {other}")),
        };
        let own = product.object.symbol(product.function_symbol(self.func));
        let write::SymbolSection::Section(section) = own.section else {
            return Err("a function that is not in a section".to_owned());
        };
        let relocation = write::Relocation {
            offset: own.value + u64::from(self.reloc.offset),
            symbol,
            addend: self.reloc.addend,
            flags: object::RelocationFlags::Elf {
                r_type: object::elf::RelocationType(self.r_type),
            },
        };
        product.object.add_relocation(section, relocation).map_err(|e| e.to_string())
    }
}

fn selected_target() -> Result<&'static Target> {
    Target::selected().map_err(CodegenError::Backend)
}

/// How many functions went through Cranelift since the process started.
static COMPILED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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
#[salsa::tracked(returns(copy))]
fn root_serial(db: &dyn HirDatabase, func: FunctionId) -> u128 {
    compiled_root(db, func).as_ref().map_or(0, print_of)
}

/// A hash of the text of `func` and of where it starts, which says what rustc compiled of it
/// and of each block in it, and where its panics are. `None` for one a macro writes.
fn written_print(db: &dyn HirDatabase, func: FunctionId) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let source = func.lookup(db).source(db);
    if source.file_id.macro_file().is_some() {
        return None;
    }
    let mut hasher = rustc_hash::FxHasher::default();
    (*declaration_location(db, func))?.hash(&mut hasher);
    source.value.syntax().text().to_string().hash(&mut hasher);
    Some(hasher.finish())
}

/// The closures written in the body of `func` in the order the body has them: each as the
/// instance its code is compiled as, with a hash of its type, which says what it captures
/// and how it is called. `None` for one that is not a plain closure (`async`). `None` in all
/// for a generic function, whose closures are instances of its arguments.
#[allow(clippy::type_complexity)]
fn closures_of(
    db: &dyn HirDatabase,
    func: FunctionId,
) -> Option<Vec<(hir_def::hir::ExprId, Option<(Instance, u64)>)>> {
    use std::hash::{Hash, Hasher};
    let body = Body::of(db, func.into());
    let infer = InferenceResult::of(db, DefWithBodyId::from(func));
    let checked = crate_checks_overflow(db, func.krate(db)).unwrap_or(false);
    let closures = body.exprs().filter(|(_, it)| matches!(it, hir_def::hir::Expr::Closure { .. }));
    closures
        .map(|(expr, _)| {
            let ty = infer.expr_ty(expr);
            let TyKind::Closure(id, args) = ty.kind() else { return Some((expr, None)) };
            let parent = args.as_closure().parent_args();
            if !parent.is_empty() {
                return None;
            }
            let instance = Instance {
                def: InstanceDef::Closure(id.0.as_id()),
                args: GenericArgs::new_from_slice(parent).store(),
                checked,
            };
            let mut hasher = rustc_hash::FxHasher::default();
            format!("{args:?}").hash(&mut hasher);
            Some((expr, Some((instance, hasher.finish()))))
        })
        .collect()
}

// Nothing in its arguments is collected by salsa (no tracked struct, no interned value with
// fewer revisions), so they stay interned and are never verified.
#[salsa::tracked(returns(copy), revisions = usize::MAX)]
fn instance_serial(db: &dyn HirDatabase, instance: Instance, env: StoredParamEnvAndCrate) -> u128 {
    compiled_instance(db, instance, env).as_ref().map_or(0, print_of)
}

#[salsa::tracked(returns(copy))]
fn static_serial(db: &dyn HirDatabase, id: StaticId) -> u128 {
    compiled_static(db, id).as_ref().map_or(0, print_of)
}

/// What the threads of [`compile_unnamed`] have left to compile.
#[derive(Default)]
pub struct Unnamed {
    state: std::sync::Mutex<UnnamedState>,
    told: ToldTypes,
    /// The generic instances the functions reach are compiled too.
    follow: bool,
}

/// The types [`compile_unnamed`] told of, for the build that comes next.
// FIXME: a build after an edit must not find these.
static TOLD_UNNAMED: std::sync::Mutex<Option<ToldTypes>> = std::sync::Mutex::new(None);

#[derive(Default)]
struct UnnamedState {
    roots: Vec<FunctionId>,
    queue: Vec<(Instance, StoredParamEnvAndCrate)>,
    seen: FxHashSet<(Instance, StoredParamEnvAndCrate)>,
    working: usize,
    compiled: usize,
}

impl Unnamed {
    pub fn new(roots: Vec<FunctionId>) -> Unnamed {
        let state = UnnamedState { roots, ..Default::default() };
        let told = ToldTypes::default();
        *TOLD_UNNAMED.lock().unwrap_or_else(|e| e.into_inner()) = Some(told.clone());
        Unnamed { state: std::sync::Mutex::new(state), told, follow: true }
    }

    /// The functions alone, without the instances they reach: what an edit's first look at
    /// the functions of its file compiles.
    pub fn alone(roots: Vec<FunctionId>) -> Unnamed {
        Unnamed { follow: false, ..Unnamed::new(roots) }
    }

    /// How many functions and instances were compiled.
    pub fn compiled(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).compiled
    }
}

/// Whether rustc copies `func` into each crate that calls it, for a type or constant it is
/// given.
pub fn copied_by_rustc(db: &dyn HirDatabase, func: FunctionId) -> bool {
    patch::copied_by_rustc(db, func)
}

/// Compiles a function written in a file [`Rejections::home`] notes next, which compiles them
/// one after another, so that threads share that work. One rustc copies into its callers,
/// which `home` passes over, is not compiled.
pub fn compile_written(db: &dyn HirDatabase, func: FunctionId) {
    if !patch::copied_by_rustc(db, func) {
        let compile = || compiled_root(db, func).is_ok();
        // One that panics does again as `home` compiles it, which says so.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(compile));
    }
}

/// Compiles the functions `unnamed` starts from and the generic instances they reach, until
/// `done`, as one of the threads that do. Which symbols rustc built is not known yet: the
/// code of a function is the same whichever they are, and a build that knows them finds it
/// compiled. A call of a function without generic arguments is not followed, since rustc
/// may have built the one called.
pub fn compile_unnamed(db: &dyn HirDatabase, unnamed: &Unnamed, done: &dyn Fn() -> bool) {
    let mut homes = home::Homes::default();
    loop {
        let job = {
            let mut state = unnamed.state.lock().unwrap_or_else(|e| e.into_inner());
            let job = match state.roots.pop() {
                Some(root) => Some(either::Either::Left(root)),
                None => state.queue.pop().map(either::Either::Right),
            };
            if job.is_none() && state.working == 0 {
                return;
            }
            state.working += usize::from(job.is_some());
            job
        };
        if done() {
            return;
        }
        let Some(job) = job else {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        };
        let compiled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &job {
            either::Either::Left(root) => {
                root_rows(db, *root);
                compiled_root(db, *root).as_ref().ok()
            }
            either::Either::Right((instance, env)) => {
                instance_rows(db, instance.clone(), env.clone());
                compiled_instance(db, instance.clone(), env.clone()).as_ref().ok()
            }
        }));
        let given_up = compiled.is_err() && done();
        let mut found = Vec::new();
        if let Ok(Some(compiled)) = compiled {
            unnamed.told.ahead(db, compiled);
        }
        let followed = compiled.iter().flatten().filter(|_| unnamed.follow);
        for request in followed.flat_map(|it| &it.requests.funcs) {
            let FuncTarget::Instance(callee, asked) = &request.target else { continue };
            let named = matches!(callee.def, InstanceDef::Fn(_))
                && callee.args.as_ref().iter().all(|it| it.region().is_some());
            if !named {
                found.push((callee.clone(), homes.home(db, callee, asked)));
            }
        }
        let mut state = unnamed.state.lock().unwrap_or_else(|e| e.into_inner());
        state.working -= 1;
        state.compiled += usize::from(!given_up);
        for it in found {
            if state.seen.insert(it.clone()) {
                state.queue.push(it);
            }
        }
    }
}

/// Where the code from an offset on is written: the file, the line and the column.
type Row = (u32, base_db::FileId, u32, u32);

/// The rows of the code of a non-generic function, in the order of [`Code::lines`].
#[salsa::tracked(returns(ref))]
fn root_rows(db: &dyn HirDatabase, func: FunctionId) -> Vec<Row> {
    compiled_root(db, func).as_ref().map(|it| rows_of(db, it)).unwrap_or_default()
}

/// The rows of the code of an instance, in the order of [`Code::lines`].
#[salsa::tracked(returns(ref))]
fn instance_rows(
    db: &dyn HirDatabase,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Vec<Row> {
    compiled_instance(db, instance, env).as_ref().map(|it| rows_of(db, it)).unwrap_or_default()
}

fn rows_of(db: &dyn HirDatabase, compiled: &CompiledFunction) -> Vec<Row> {
    let Some((owner, spans)) = &compiled.source else { return Vec::new() };
    let rows = compiled.code.lines.iter().filter_map(|row| {
        let span = *spans.get(row.span as usize)?;
        let (file, line, column) = row_location(db, *owner, compiled.closure, span)?;
        Some((row.offset, file, line, column))
    });
    rows.collect()
}

/// The code of a non-generic function.
#[salsa::tracked(returns(ref))]
fn compiled_root<'db>(db: &'db dyn HirDatabase, func: FunctionId) -> Result<CompiledFunction> {
    let _span = tracing::info_span!(target: "ra_jit", "compiled_root").entered();
    tier::for_build(|| root_code(db, func, false)).map(Produced::code)
}

/// The instance a non-generic function is compiled as under its own symbol.
fn root_instance(db: &dyn HirDatabase, func: FunctionId) -> Instance {
    Instance {
        def: InstanceDef::Fn(func),
        args: GenericArgs::new_from_slice(&[]).store(),
        checked: crate_checks_overflow(db, func.krate(db)).unwrap_or(false),
    }
}

/// Whether the instance is compiled with the flags of the crate its function is written
/// in, as the copy under the symbol of the function is. A crate that uses an `#[inline]`
/// function of another compiles a copy with its own.
fn as_its_crate_compiles(
    db: &dyn HirDatabase,
    symbols: &dyn SymbolResolver,
    instance: &Instance,
) -> bool {
    let InstanceDef::Fn(func) = instance.def else { return false };
    match crate_checks_overflow(db, func.krate(db)) {
        Some(written_with) => written_with == instance.checked,
        // The standard library is built without the checks. rustc compiles an `#[inline]`
        // function of it in each crate that calls it, where what it calls that takes the
        // checks from its caller (`#[rustc_inherit_overflow_checks]`) has that crate's:
        // the copy ra-jit's build of the standard library exports has none.
        None => {
            !instance.checked
                || !symbols.nothing_of_rustc()
                || !AttrFlags::query(db, func.into()).contains(AttrFlags::IS_INLINE)
        }
    }
}

/// Whether rustc's callers of the symbol of `func` pass it scalable vectors in SVE registers,
/// where ra-jit's pass them by reference: ra-jit's code for it is a copy of its own, never
/// put in the object under the symbol. Only where nothing of rustc's is linked are all the
/// callers ra-jit's.
fn rustcs_callers_pass_scalable(
    db: &dyn HirDatabase,
    symbols: &dyn SymbolResolver,
    func: FunctionId,
) -> bool {
    if symbols.nothing_of_rustc() {
        return false;
    }
    let env = ParamEnvAndCrate {
        param_env: hir_ty::next_solver::ParamEnv::empty(),
        krate: func.krate(db),
    };
    sve::passes_scalable(db, func, None, &env.store())
}

/// The last name of what an instance is of.
fn callee_name(db: &dyn HirDatabase, def: InstanceDef) -> &str {
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

/// Writes into `blob` the `TypeId`s it has the places of.
fn hash_type_ids(
    db: &dyn HirDatabase,
    symbols: &dyn SymbolResolver,
    crates: &FxHashSet<Crate>,
    blob: &mut ConstBlob,
) -> Result<()> {
    for (offset, of) in std::mem::take(&mut blob.type_ids) {
        let of = of.as_ref();
        let mut hasher = type_id::TypeIdHasher::new(db, symbols, crates);
        let Some(hash) = hasher.type_id(of) else {
            let stuck = hasher.stuck.unwrap_or_default();
            unsupported!("`TypeId` of `{of:?}` in a constant, not hashed (at `{stuck}`)");
        };
        let at = offset as usize;
        blob.bytes[at..at + 16].copy_from_slice(&hash.to_le_bytes());
    }
    Ok(())
}

/// What a debugger of a rustc build calls the code of a closure of `instance`.
fn debug_closure<'db>(
    db: &'db dyn HirDatabase,
    instance: &Instance,
    owner: InferBodyId<'db>,
    expr: ExprId,
) -> Option<String> {
    let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) = owner else {
        return None;
    };
    let args = instance.args.as_ref();
    hir_ty::mir::type_name::rustc_debug_closure(db, func, args.as_slice(), expr)
}

/// What a backtrace of a rustc build calls the instance, as far as that is reproduced:
/// the last name of it otherwise.
fn backtrace_name(db: &dyn HirDatabase, instance: &Instance) -> String {
    let args = instance.args.as_ref();
    let ty = || args.as_slice().first().and_then(|arg| arg.ty());
    let of_closure = |owner: InferBodyId<'_>, expr| match owner {
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) => {
            hir_ty::mir::type_name::rustc_closure_name(db, func, args.as_slice(), expr)
        }
        _ => None,
    };
    let name = match instance.def {
        InstanceDef::Fn(func) => {
            hir_ty::mir::type_name::rustc_function_name(db, func, args.as_slice())
        }
        InstanceDef::Closure(id) => {
            let at = InternedClosureId::from_id(id).loc(db);
            of_closure(at.owner, at.expr)
        }
        InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
            let at = InternedCoroutineId::from_id(id).loc(db);
            of_closure(at.owner, at.expr)
        }
        InstanceDef::DropGlue => ty()
            .and_then(|ty| hir_ty::mir::type_name::rustc_symbol_type(db, ty))
            .map(|ty| format!("core::ptr::drop_glue::<{ty}>")),
        InstanceDef::CallOnceShim
        | InstanceDef::VtableShim(_)
        | InstanceDef::FnPointerShim
        | InstanceDef::ReifyShim { .. } => None,
    };
    name.unwrap_or_else(|| callee_name(db, instance.def).to_owned())
}

/// The code of a non-generic function: Cranelift's, or, for a compile at a level above 1
/// (`tier`), its IR for LLVM.
fn root_code(db: &dyn HirDatabase, func: FunctionId, tier: bool) -> Result<Produced> {
    let (body, env) = root_body(db, func)?;
    if std::env::var("RA_JIT_DUMP_MIR")
        .is_ok_and(|want| FunctionSignature::of(db, func).name.as_str() == want)
    {
        dump_mir(body);
    }
    let signature = FunctionSignature::of(db, func);
    let (abi, variadic) = (signature.abi, signature.is_varargs());
    let args = GenericArgs::new_from_slice(&[]).store();
    let checked = crate_checks_overflow(db, func.krate(db)).unwrap_or(false);
    compile_as(db, env, Some(body), track_caller(db, func), checked, tier, |fx| {
        fx.abi = abi;
        fx.variadic = variadic;
        fx.args = Some(args);
        fx.codegen()
    })
}

/// The crates of a program of `crates`: what they depend on, but not through a proc macro,
/// which is a library of the compiler's.
fn linked_crates(
    db: &dyn HirDatabase,
    crates: impl IntoIterator<Item = Crate>,
) -> FxHashSet<Crate> {
    let mut linked = FxHashSet::default();
    let mut stack: Vec<Crate> = crates.into_iter().collect();
    while let Some(krate) = stack.pop() {
        if linked.insert(krate) {
            let deps = krate.data(db).dependencies.iter().map(|it| it.crate_id);
            stack.extend(deps.filter(|it| !it.data(db).target_kind.is_proc_macro()));
        }
    }
    linked
}

/// The `main` the C runtime calls, for a program whose `main` is `main`.
#[salsa::tracked(returns(ref))]
fn compiled_start<'db>(db: &'db dyn HirDatabase, main: FunctionId) -> Result<CompiledFunction> {
    let krate = main.krate(db);
    let env = ParamEnvAndCrate { param_env: hir_ty::next_solver::ParamEnv::empty(), krate };
    let checked = crate_checks_overflow(db, krate).unwrap_or(false);
    compile(db, env, None, false, checked, |fx| fx.start_shim(main))
}

/// The code of a function, closure, `async` block or shim with its generic arguments filled in.
// Nothing in its arguments is collected by salsa (no tracked struct, no interned value with
// fewer revisions), so they stay interned and are never verified.
#[salsa::tracked(returns(ref), revisions = usize::MAX, lru = 0)]
fn compiled_instance<'db>(
    db: &'db dyn HirDatabase,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Result<CompiledFunction> {
    let _span = tracing::info_span!(target: "ra_jit", "compiled_instance").entered();
    tier::for_build(|| instance_code(db, instance, env, Detach::No)).map(Produced::code)
}

/// Whether a compile leaves LLVM's part to later ([`Produced::Pending`]): not at all, for a
/// compile at a level above 1, which only a function, closure or `async` block of the source
/// is compiled at, or for a unit of a build ([`units`]), which takes shims too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Detach {
    No,
    Tier,
    #[cfg_attr(not(feature = "llvm"), expect(dead_code))]
    Unit,
}

/// The code of an instance: Cranelift's, or, detached, its IR for LLVM.
fn instance_code(
    db: &dyn HirDatabase,
    instance: Instance,
    env: StoredParamEnvAndCrate,
    detach: Detach,
) -> Result<Produced> {
    let stored_env = env.clone();
    let env = env.as_ref(db);
    let ty = || instance.args.as_ref().type_at(0);
    let (tier, shim) = (detach != Detach::No, detach == Detach::Unit);
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
            return compile_as(db, env, None, false, instance.checked, shim, |fx| {
                let ty = ty();
                fx.drop_glue(ty)?;
                if std::env::var_os("RA_JIT_DUMP_CLIF").is_some() {
                    eprintln!("drop glue for {ty:?}\n{}", fx.builder.func.display());
                }
                Ok(())
            });
        }
        InstanceDef::CallOnceShim => {
            return compile_as(db, env, None, false, instance.checked, shim, |fx| {
                fx.call_once_shim(ty())
            });
        }
        InstanceDef::VtableShim(func) => {
            let args = instance.args.as_ref();
            return compile_as(db, env, None, false, instance.checked, shim, |fx| {
                fx.vtable_shim(func, args)
            });
        }
        InstanceDef::FnPointerShim => {
            return compile_as(db, env, None, false, instance.checked, shim, |fx| {
                fx.fn_pointer_shim(ty())
            });
        }
        InstanceDef::ReifyShim { receiver } => {
            return compile_as(db, env, None, false, instance.checked, shim, |fx| {
                fx.reify_shim(ty(), receiver)
            });
        }
        InstanceDef::Fn(_)
        | InstanceDef::Closure(_)
        | InstanceDef::Coroutine(_)
        | InstanceDef::CoroutineByMove(_) => instance_body(db, &instance, stored_env)?,
    };
    let body = &*body;
    let dump = std::env::var("RA_JIT_DUMP_MIR").ok();
    let (tracked, abi, variadic) = match instance.def {
        InstanceDef::Fn(func) => {
            let signature = FunctionSignature::of(db, func);
            if dump.as_deref() == Some(signature.name.as_str()) {
                dump_mir(body);
            }
            (track_caller(db, func), signature.abi, signature.is_varargs())
        }
        _ => {
            if dump.as_deref() == Some("{closure}") {
                dump_mir(body);
            }
            (false, ExternAbi::Rust, false)
        }
    };
    let args = instance.args.clone();
    let closure = match instance.def {
        InstanceDef::Closure(id) => Some(InternedClosureId::from_id(id).loc(db).expr),
        InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
            Some(InternedCoroutineId::from_id(id).loc(db).expr)
        }
        _ => None,
    };
    let compiled = compile_as(db, env, Some(body), tracked, instance.checked, tier, |fx| {
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
            sig.params.push(AbiParam::special(ptr(), ArgumentPurpose::StructArgument(8)));
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
        sig.params.push(AbiParam::special(ptr(), ArgumentPurpose::StructArgument(8)));
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

    fn rest(self, sig: &mut Signature) {
        match self {
            Variadic::SystemV(named) => named.rest(sig),
            Variadic::Windows(named) => named.rest(sig),
            Variadic::Apple { .. } => (),
            Variadic::Buffer => sig.params.push(AbiParam::new(ptr())),
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

/// The MIR optimization level the environment asks for with `RA_JIT_MIR_OPT`: 0, the default,
/// compiles the runtime body; 1 and 2 that body after rustc's MIR passes at its
/// `-Zmir-opt-level` 1 and 2 (`hir_ty::mir::MirOptLevel`).
pub fn mir_opt_level() -> MirOptLevel {
    static LEVEL: LazyLock<MirOptLevel> = LazyLock::new(|| {
        let Some(asked) = std::env::var_os(MIR_OPT) else { return MirOptLevel::Off };
        let level =
            asked.to_str().and_then(|it| it.parse().ok()).and_then(MirOptLevel::from_number);
        level.unwrap_or_else(|| {
            eprintln!("blaze: {MIR_OPT}={}: expected 0, 1 or 2; using 0", asked.display());
            MirOptLevel::Off
        })
    });
    *LEVEL
}

pub const MIR_OPT: &str = "RA_JIT_MIR_OPT";

/// Says whether rustc builds the program with `-C link-dead-code`, before anything of it is
/// compiled: rustc then keeps every function, `#[inline(always)]` ones too, as one copy in
/// one codegen unit, which LLVM inlines only into the code of that unit.
pub fn rustc_links_dead_code(links: bool) {
    backend::link_dead_code(links);
}

/// The body the code generator compiles of `def`: the runtime body, optimized at
/// [`mir_opt_level`].
fn compiled_mir_body<'db>(
    db: &'db dyn HirDatabase,
    def: InferBodyId<'db>,
) -> std::result::Result<&'db MirBody<'db>, hir_ty::mir::MirLowerError<'db>> {
    match mir_opt_level() {
        MirOptLevel::Off => db.runtime_mir_body(def),
        level => db.optimized_mir_body(def, level),
    }
}

fn compiled_mir_body_for_closure<'db>(
    db: &'db dyn HirDatabase,
    closure: InternedClosureId<'db>,
) -> std::result::Result<&'db MirBody<'db>, hir_ty::mir::MirLowerError<'db>> {
    match mir_opt_level() {
        MirOptLevel::Off => db.runtime_mir_body_for_closure(closure),
        level => db.optimized_mir_body_for_closure(closure, level),
    }
}

/// The body of an instance that is not a shim, with the instance's types in place of the
/// generic parameters. The database does not keep it unless `RA_JIT_LEAN=0`: nothing but
/// compiling the instance reads it, and what was compiled is kept.
fn instance_body<'db>(
    db: &'db dyn HirDatabase,
    instance: &Instance,
    env: StoredParamEnvAndCrate,
) -> Result<Cow<'db, MirBody<'db>>> {
    static LEAN: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("RA_JIT_LEAN").is_none_or(|it| it != "0"));
    let args = instance.args.clone();
    let tally = backend::Tally::global();
    let filled = |body, args: &StoredGenericArgs, env: &StoredParamEnvAndCrate| {
        tally.step(Step::Monomorphize, || monomorphized(db, body, args, env).map(Cow::Owned))
    };
    // The database keeps a monomorphized body of the runtime body only.
    let lean = *LEAN || mir_opt_level() != MirOptLevel::Off;
    let body = match instance.def {
        InstanceDef::Fn(func) if lean => tally
            .step(Step::Mir, || compiled_mir_body(db, func.into()))
            .and_then(|it| filled(it, &args, &env)),
        InstanceDef::Closure(id) if lean => {
            compiled_mir_body_for_closure(db, InternedClosureId::from_id(id))
                .and_then(|it| filled(it, &args, &env))
        }
        InstanceDef::Coroutine(id) if *LEAN => db
            .runtime_mir_body_for_coroutine(InternedCoroutineId::from_id(id))
            .and_then(|it| filled(it, &args, &env)),
        InstanceDef::CoroutineByMove(id) if *LEAN => db
            .runtime_mir_body_for_coroutine_by_move(InternedCoroutineId::from_id(id))
            .and_then(|it| filled(it, &args, &env)),
        InstanceDef::Fn(func) => {
            db.monomorphized_runtime_mir_body(func.into(), args, env).map(Cow::Borrowed)
        }
        InstanceDef::Closure(id) => db
            .monomorphized_runtime_mir_body_for_closure(InternedClosureId::from_id(id), args, env)
            .map(Cow::Borrowed),
        InstanceDef::Coroutine(id) => db
            .monomorphized_runtime_mir_body_for_coroutine(
                InternedCoroutineId::from_id(id),
                args,
                env,
            )
            .map(Cow::Borrowed),
        InstanceDef::CoroutineByMove(id) => db
            .monomorphized_runtime_mir_body_for_coroutine_by_move(
                InternedCoroutineId::from_id(id),
                args,
                env,
            )
            .map(Cow::Borrowed),
        InstanceDef::DropGlue
        | InstanceDef::CallOnceShim
        | InstanceDef::VtableShim(_)
        | InstanceDef::FnPointerShim
        | InstanceDef::ReifyShim { .. } => unsupported!("the body of a shim"),
    };
    body.map_err(|e| CodegenError::Frontend(format!("mir: {e:?}")))
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

#[salsa::tracked(returns(ref))]
fn body_types<'db>(
    db: &'db dyn HirDatabase,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Result<BodyTypes> {
    let body = &*instance_body(db, &instance, env)?;
    Ok(BodyTypes {
        params: body.param_locals.iter().map(|&local| body.locals[local].ty.clone()).collect(),
        returned: body.locals[returned_local(body)].ty.clone(),
        poll: body.coroutine.map(|it| body.locals[it.poll].ty.clone()),
    })
}

/// The future of a non-generic `async fn` as `-Zprint-type-sizes` names it, with the offsets
/// of what each variant holds (the captures, and the locals kept there) in increasing order:
/// `{async fn body of f()} @coroutine 0,16;0,8,16`. `harness/layoutdiff.py` compares it with
/// rustc's, which `--layouts` otherwise skips: a future both compilers make and poll has to be
/// laid out alike to the byte.
fn coroutine_layout_line<'db>(
    db: &'db dyn HirDatabase,
    ty: Ty<'db>,
    layout: &Layout,
) -> Option<String> {
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
    } = &store[closure.expr]
    else {
        return None;
    };
    let Variants::Multiple { tag_field, variants, .. } = &layout.variants else { return None };
    let upvars: Vec<u64> = (0..layout.fields.count())
        .filter(|&field| field != rustc_index::Idx::index(*tag_field))
        .map(|field| layout.fields.offset(field).bytes())
        .collect();
    let variants: Vec<String> = variants
        .iter()
        .map(|variant| {
            let mut offsets = upvars.clone();
            offsets.extend(variant.field_offsets.iter().map(|it| it.bytes()));
            offsets.sort_unstable();
            offsets.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
        })
        .collect();
    let name = FunctionSignature::of(db, func).name.as_str().to_owned();
    Some(format!("{{async fn body of {name}()}} @coroutine {}", variants.join(";")))
}

/// Whether a future and everything that makes, polls or drops it have to come from one
/// compiler. They need not: ra-jit lays a future out as rustc does, from the same MIR
/// (`docs/mir-passes.md` of the fork). `RA_JIT_SPLIT_FUTURES=1` keeps them apart as before,
/// a temporary way back until mixing is verified (`harness/mixfutures`).
// FIXME: delete with what it guards once mixed futures are verified on den-native.
fn futures_kept_apart() -> bool {
    static APART: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("RA_JIT_SPLIT_FUTURES").is_some_and(|it| it != "0"));
    *APART
}

/// Whether a non-generic function returns a future of an `async` block, by itself or inside
/// another type. rustc lays such a future out differently, so the function that makes one has
/// to come from the compiler that compiles what holds and polls it.
#[salsa::tracked]
fn returns_async_block<'db>(db: &'db dyn HirDatabase, func: FunctionId) -> bool {
    struct Finder<'db> {
        opaque: bool,
        coroutine: bool,
        seen: FxHashSet<Ty<'db>>,
    }
    impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for Finder<'db> {
        type Result = ();

        fn visit_ty(&mut self, ty: Ty<'db>) {
            if !self.seen.insert(ty) {
                return;
            }
            match ty.kind() {
                TyKind::Coroutine(..) => self.coroutine = true,
                TyKind::Alias(..) => self.opaque = true,
                _ => {}
            }
            rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self)
        }
    }
    let mut finder = Finder { opaque: false, coroutine: false, seen: FxHashSet::default() };
    // The declared type names a future only as an `impl Trait`, which the body's own does not.
    let declared = db.callable_item_signature(func.into()).skip_binder().skip_binder().output();
    rustc_type_ir::TypeVisitable::visit_with(&declared, &mut finder);
    if !finder.opaque && !FunctionSignature::of(db, func).is_async() {
        return false;
    }
    let Ok((body, _)) = root_body(db, func) else { return false };
    let returned = body.locals[returned_local(body)].ty.as_ref();
    rustc_type_ir::TypeVisitable::visit_with(&returned, &mut finder);
    finder.coroutine
}

/// Whether a caller of the function gets a value of a type that only its body says: the
/// future of an `async fn`, or what an `impl Trait` it returns stands for. The caller has the
/// code for that type, compiled from the body as it was then.
fn hands_out_opaque(db: &dyn HirDatabase, func: FunctionId) -> bool {
    struct Finder(bool);
    impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for Finder {
        type Result = ();

        fn visit_ty(&mut self, ty: Ty<'db>) {
            if let TyKind::Alias(rustc_type_ir::AliasTy {
                kind: rustc_type_ir::AliasTyKind::Opaque { .. },
                ..
            }) = ty.kind()
            {
                self.0 = true;
            }
            rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self)
        }
    }
    if FunctionSignature::of(db, func).is_async() {
        return true;
    }
    let mut finder = Finder(false);
    let declared = db.callable_item_signature(func.into()).skip_binder().skip_binder().output();
    rustc_type_ir::TypeVisitable::visit_with(&declared, &mut finder);
    finder.0
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
fn implemented_fn_method(
    db: &dyn HirDatabase,
    lang_items: &LangItems,
    func: FunctionId,
) -> Option<FunctionId> {
    let ItemContainerId::ImplId(impl_) = func.lookup(db).container else { return None };
    let trait_ref = db.impl_trait(impl_)?;
    let name = &FunctionSignature::of(db, func).name;
    let method = trait_ref.skip_binder().def_id.0.trait_items(db).method_by_name(name)?;
    let [fn_methods, _] = fn_trait_methods(lang_items);
    fn_methods.contains(&Some(method)).then_some(method)
}

/// The instance that is the `poll` of the future `ty`, when that is one of an `async` block.
fn coroutine_instance<'db>(
    db: &'db dyn HirDatabase,
    ty: Ty<'db>,
) -> Option<(InstanceDef, GenericArgs<'db>)> {
    let TyKind::Coroutine(id, args) = ty.kind() else { return None };
    let args = args.as_coroutine();
    let def = match is_by_move_coroutine(db, id.0, args) {
        true => InstanceDef::CoroutineByMove(id.0.as_id()),
        false => InstanceDef::Coroutine(id.0.as_id()),
    };
    Some((def, GenericArgs::new_from_slice(args.parent_args())))
}

/// The `async` blocks whose futures the body of `func` holds, or `None` when the body
/// can't be read. rustc lays a future out differently, so every function that touches one
/// has to come from the same compiler.
#[salsa::tracked(returns(ref))]
fn async_blocks<'db>(db: &'db dyn HirDatabase, func: FunctionId) -> Option<FxHashSet<salsa::Id>> {
    let Ok((body, env)) = root_body(db, func) else {
        return (!infers_a_future(db, func)).then(FxHashSet::default);
    };
    held_blocks(db, body, env)
}

/// The `async` blocks whose futures `body` holds, itself or in the closures and blocks it
/// holds, or `None` when one of those can't be read.
fn held_blocks<'db>(
    db: &'db dyn HirDatabase,
    body: &MirBody<'db>,
    env: ParamEnvAndCrate<'db>,
) -> Option<FxHashSet<salsa::Id>> {
    struct Collector<'db>(Vec<Ty<'db>>);
    impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for Collector<'db> {
        type Result = ();

        fn visit_ty(&mut self, ty: Ty<'db>) {
            if let TyKind::Coroutine(..) | TyKind::Closure(..) | TyKind::CoroutineClosure(..) =
                ty.kind()
            {
                self.0.push(ty);
            }
            rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self)
        }
    }
    let (mut found, mut seen) = (FxHashSet::default(), FxHashSet::default());
    let mut bodies = vec![body];
    while let Some(body) = bodies.pop() {
        let mut collector = Collector(Vec::new());
        for (_, local) in body.locals.iter() {
            rustc_type_ir::TypeVisitable::visit_with(&local.ty.as_ref(), &mut collector);
        }
        for ty in collector.0 {
            if !seen.insert(ty) {
                continue;
            }
            let body = match ty.kind() {
                TyKind::Coroutine(id, args) => {
                    found.insert(id.0.as_id());
                    let args = args.as_coroutine();
                    let parent = GenericArgs::new_from_slice(args.parent_args()).store();
                    match is_by_move_coroutine(db, id.0, args) {
                        true => db.monomorphized_mir_body_for_coroutine_by_move(
                            id.0,
                            parent,
                            env.store(),
                        ),
                        false => db.monomorphized_mir_body_for_coroutine(id.0, parent, env.store()),
                    }
                }
                TyKind::CoroutineClosure(id, args) => {
                    let parent =
                        GenericArgs::new_from_slice(args.as_coroutine_closure().parent_args());
                    let closure = id.0.as_closure(db);
                    db.monomorphized_mir_body_for_closure(closure, parent.store(), env.store())
                }
                TyKind::Closure(id, args) => {
                    let parent = GenericArgs::new_from_slice(args.as_closure().parent_args());
                    db.monomorphized_mir_body_for_closure(id.0, parent.store(), env.store())
                }
                _ => continue,
            };
            bodies.push(body.ok()?);
        }
    }
    Some(found)
}

/// Whether a type inferred in `func`'s body is or holds a future, an `impl Trait` included:
/// for a body without MIR, which then holds no future when this is `false`.
fn infers_a_future(db: &dyn HirDatabase, func: FunctionId) -> bool {
    struct Finder<'db> {
        interner: DbInterner<'db>,
        seen: FxHashSet<Ty<'db>>,
        found: bool,
    }
    impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for Finder<'db> {
        type Result = ();

        fn visit_ty(&mut self, ty: Ty<'db>) {
            if self.found || !self.seen.insert(ty) {
                return;
            }
            match ty.kind() {
                TyKind::Coroutine(..) | TyKind::CoroutineClosure(..) => self.found = true,
                TyKind::Alias(rustc_type_ir::AliasTy {
                    kind: rustc_type_ir::AliasTyKind::Opaque { def_id },
                    args,
                    ..
                }) => {
                    let hidden = rustc_type_ir::Interner::type_of(self.interner, def_id.into())
                        .instantiate(self.interner, args)
                        .skip_norm_wip();
                    // What its defining body did not tell is polled as a future only when it is
                    // declared one.
                    if matches!(hidden.kind(), TyKind::Error(_)) {
                        let future = self.interner.lang_items().Future;
                        let bounds =
                            rustc_type_ir::Interner::item_bounds(self.interner, def_id.into());
                        self.found = bounds.skip_binder().into_iter().any(|clause| {
                            rustc_type_ir::inherent::Clause::as_trait_clause(clause)
                                .is_some_and(|it| Some(it.skip_binder().def_id().0) == future)
                        });
                    } else {
                        self.visit_ty(hidden);
                    }
                }
                _ => rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self),
            }
        }
    }
    let infer = InferenceResult::of(db, DefWithBodyId::from(func));
    let interner = DbInterner::new_with(db, func.krate(db));
    let mut finder = Finder { interner, seen: FxHashSet::default(), found: false };
    for ty in infer.expression_types().map(|it| it.1).chain(infer.pattern_types().map(|it| it.1)) {
        rustc_type_ir::TypeVisitable::visit_with(&ty, &mut finder);
    }
    finder.found
}

/// The `async` blocks whose futures rustc's copy of `instance` holds: those of its body, with
/// its types filled in, or else those its body names before they are.
fn made_futures(
    db: &dyn HirDatabase,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
) -> FxHashSet<salsa::Id> {
    let InstanceDef::Fn(func) = instance.def else { return FxHashSet::default() };
    instance_body(db, instance, env.clone())
        .ok()
        .and_then(|body| held_blocks(db, &body, env.as_ref(db)))
        .unwrap_or_else(|| named_futures(db, func))
}

/// The `async` blocks of `func`'s body and of the closures and blocks in it, read before its
/// generic parameters are filled in.
fn named_futures(db: &dyn HirDatabase, func: FunctionId) -> FxHashSet<salsa::Id> {
    struct Collector<'db>(Vec<Ty<'db>>);
    impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for Collector<'db> {
        type Result = ();

        fn visit_ty(&mut self, ty: Ty<'db>) {
            if let TyKind::Coroutine(..) | TyKind::Closure(..) | TyKind::CoroutineClosure(..) =
                ty.kind()
            {
                self.0.push(ty);
            }
            rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self)
        }
    }
    let (mut found, mut seen) = (FxHashSet::default(), FxHashSet::default());
    let mut bodies: Vec<&MirBody<'_>> = db.mir_body(func.into()).into_iter().collect();
    while let Some(body) = bodies.pop() {
        let mut collector = Collector(Vec::new());
        for (_, local) in body.locals.iter() {
            rustc_type_ir::TypeVisitable::visit_with(&local.ty.as_ref(), &mut collector);
        }
        for ty in collector.0 {
            if !seen.insert(ty) {
                continue;
            }
            let body = match ty.kind() {
                TyKind::Coroutine(id, _) => {
                    found.insert(id.0.as_id());
                    db.mir_body_for_coroutine(id.0)
                }
                TyKind::Closure(id, _) => db.mir_body_for_closure(id.0),
                TyKind::CoroutineClosure(id, _) => db.mir_body_for_closure(id.0.as_closure(db)),
                _ => continue,
            };
            bodies.extend(body.ok());
        }
    }
    found
}

/// The functions skipped that rustc has a copy of. One of a crate rustc built nothing of has
/// none: it is missing from the program, and no code of rustc's uses a static in its place.
fn left_to_rustc<'a>(
    db: &'a dyn HirDatabase,
    symbols: &'a dyn SymbolResolver,
    skipped: &'a [(FunctionId, CodegenError)],
) -> impl Iterator<Item = FunctionId> + 'a {
    skipped
        .iter()
        .filter(|(_, e)| !e.no_symbol())
        .map(|it| it.0)
        .filter(|&func| symbols.rustc_built(func.krate(db)))
}

/// The statics that functions left to rustc refer to, themselves or through the instances they
/// name.
#[derive(Default, Clone, PartialEq, Eq)]
struct TheirStatics {
    statics: FxHashSet<StaticId>,
    /// The crates with a function for which that is not all of them: what could not be
    /// compiled refers to who knows what.
    unknown: FxHashSet<Crate>,
}

/// The crates whose functions code that could not be compiled may call, which nothing then
/// tells: rustc's copy of it calls rustc's copies of them, unless ours are compiled.
#[derive(Default)]
struct Unseen {
    all: bool,
    crates: FxHashSet<Crate>,
}

impl Unseen {
    fn calls(&self, krate: Crate) -> bool {
        self.all || self.crates.contains(&krate)
    }
}

/// The instances the walk for [`TheirStatics`] went through. What it found stays as it is
/// until the code of one of them, or of a static it found, changes.
type Walked = Vec<(Instance, StoredParamEnvAndCrate)>;

impl TheirStatics {
    /// The functions of a crate are walked together: they mostly reach the same instances.
    fn of(db: &dyn HirDatabase, funcs: impl IntoIterator<Item = FunctionId>) -> TheirStatics {
        TheirStatics::walk(db, funcs).0
    }

    fn walk(
        db: &dyn HirDatabase,
        funcs: impl IntoIterator<Item = FunctionId>,
    ) -> (TheirStatics, Walked) {
        let mut by_crate = FxHashMap::<Crate, Vec<FunctionId>>::default();
        for func in funcs {
            by_crate.entry(func.krate(db)).or_default().push(func);
        }
        let mut theirs = TheirStatics::default();
        let mut walked = Walked::new();
        for (krate, funcs) in by_crate {
            let (mut seen, mut statics) = (FxHashSet::default(), FxHashSet::default());
            let mut complete = true;
            let mut stack = Vec::new();
            for func in funcs {
                match compiled_root(db, func).as_ref() {
                    Ok(compiled) => stack.push(&compiled.requests),
                    Err(_) => complete = false,
                }
            }
            while let Some(requests) = stack.pop() {
                for request in &requests.funcs {
                    let FuncTarget::Instance(instance, env) = &request.target else { continue };
                    if matches!(instance.def, InstanceDef::Fn(_))
                        && instance.args.as_ref().is_empty()
                        || !seen.insert(instance)
                    {
                        continue;
                    }
                    walked.push((instance.clone(), env.clone()));
                    match compiled_instance(db, instance.clone(), env.clone()).as_ref() {
                        Ok(compiled) => stack.push(&compiled.requests),
                        Err(_) => complete = false,
                    }
                }
                for request in &requests.datas {
                    if let DataRequest::Static(id) = request
                        && statics.insert(*id)
                    {
                        match compiled_static(db, *id).as_ref() {
                            Ok(compiled) => stack.push(&compiled.requests),
                            Err(_) => complete = false,
                        }
                    }
                }
            }
            theirs.statics.extend(statics);
            if !complete {
                theirs.unknown.insert(krate);
            }
        }
        (theirs, walked)
    }
}

/// The value of a static that rustc exports no symbol for.
#[salsa::tracked(returns(ref))]
fn compiled_static<'db>(db: &'db dyn HirDatabase, id: StaticId) -> Result<CompiledStatic> {
    let checked = crate_checks_overflow(db, id.krate(db)).unwrap_or(false);
    let mut cg = Recorder::new(db, &**target()?, selected_target()?, checked);
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

/// What the threads that compile ahead have to do and have done.
struct Ahead {
    /// Compiled functions whose calls are still to be looked through, in the order the
    /// object is given them.
    roots: std::collections::VecDeque<FunctionId>,
    /// Every function that has been one of `roots`, and those a thread is through with.
    rooted: FxHashSet<FunctionId>,
    rooted_done: FxHashSet<FunctionId>,
    /// Instances to compile, in the order they were asked for, which is the order the object
    /// is given them: the threads then finish first what it wants first.
    queue: std::collections::VecDeque<(Instance, StoredParamEnvAndCrate)>,
    seen: FxHashSet<(Instance, StoredParamEnvAndCrate)>,
    done: FxHashSet<(Instance, StoredParamEnvAndCrate)>,
    working: usize,
    /// A thread was stopped in the middle of an instance, as a write to the database
    /// does: the others are not to wait for what it would have found.
    stopped: bool,
    /// Whether the thread that puts functions in the object can still find roots, which
    /// the threads that run out of work wait for.
    open: bool,
    /// How many threads compile ahead: with none, the one that puts functions in the
    /// object compiles them.
    alive: usize,
}

impl Ahead {
    /// Whether the threads are through with everything they will compile: what they have
    /// not compiled by now, the one that puts functions in the object compiles itself.
    fn ended(&self) -> bool {
        self.stopped
            || self.alive == 0
            || (self.roots.is_empty() && self.queue.is_empty() && self.working == 0)
    }
}

/// What the threads that compile ahead are through with, for the one that puts the
/// instances in the object: it would wait for one of them on each it asked for too early.
///
/// The object is given functions in the order they were asked for, never in the order the
/// threads happen to finish them: a function's name, the ids of what it calls and its place
/// in the object all follow that order, so the same source gives the same program every
/// build (#200). The one that puts them in the object waits for the next in line while the
/// threads go on with the rest.
struct Ready<'a> {
    ahead: &'a std::sync::Mutex<Ahead>,
    /// What the threads that compile ahead wait on for work, and the one that puts the
    /// instances in the object for them to be through with some: each of a hundred threads
    /// woken whenever one is through with a function has them take turns at the lock.
    work: &'a std::sync::Condvar,
    progress: &'a std::sync::Condvar,
    /// The instances to put in the object, in the order they were asked for.
    waiting: std::collections::VecDeque<((Instance, StoredParamEnvAndCrate), FuncId)>,
}

impl Ready<'_> {
    /// Takes from `pending` and from what waited the instances next in line that no thread
    /// is still to compile, waiting until there is one. Nothing is left when it gives none.
    fn take(
        &mut self,
        pending: &mut Vec<(Instance, StoredParamEnvAndCrate, FuncId)>,
    ) -> Vec<(Instance, StoredParamEnvAndCrate, FuncId)> {
        let mut state = self.ahead.lock().unwrap_or_else(|e| e.into_inner());
        let mut queued = false;
        for (instance, env, id) in pending.drain(..) {
            let it = (instance, env);
            // One the threads did not come to, as one only data asks for.
            if !state.done.contains(&it) && state.alive > 0 && state.seen.insert(it.clone()) {
                state.queue.push_back(it.clone());
                queued = true;
            }
            self.waiting.push_back((it, id));
        }
        if queued {
            self.work.notify_all();
        }
        loop {
            let ended = state.ended();
            let mut ready = Vec::new();
            while let Some((it, _)) = self.waiting.front()
                && (ended || state.done.contains(it))
            {
                let ((instance, env), id) = self.waiting.pop_front().expect("the front");
                ready.push((instance, env, id));
            }
            if !ready.is_empty() || self.waiting.is_empty() {
                return ready;
            }
            state = self.progress.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Waits until no thread is still to compile `root`, or none is going to.
    fn wait_root(&self, root: FunctionId) {
        let mut state = self.ahead.lock().unwrap_or_else(|e| e.into_inner());
        while !Self::through(&state, root) && !state.ended() {
            state = self.progress.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn through(state: &Ahead, root: FunctionId) -> bool {
        state.stopped || !state.rooted.contains(&root) || state.rooted_done.contains(&root)
    }

    /// Has the threads compile `roots` too, which putting the others in the object found.
    fn root(&mut self, roots: impl IntoIterator<Item = FunctionId>) {
        let mut state = self.ahead.lock().unwrap_or_else(|e| e.into_inner());
        for root in roots {
            if state.rooted.insert(root) {
                state.roots.push_back(root);
            }
        }
        self.work.notify_all();
    }
}

/// What the types of variables are told as, for every thread: the threads that compile
/// find it ahead of the one that puts the functions in the object, which has it to wait
/// for otherwise.
#[derive(Default, Clone)]
struct ToldTypes(std::sync::Arc<TypesTold>);

#[derive(Default)]
struct TypesTold {
    /// What the type of each variable is told as.
    roots: debuginfo::ByEnv<StoredTy, Option<debuginfo::TypeSet>>,
    /// The sets of the types they are made of, which are shared.
    sets: debuginfo::Sets,
}

impl ToldTypes {
    fn told(
        &self,
        db: &dyn HirDatabase,
        env: &StoredParamEnvAndCrate,
        ty: &StoredTy,
    ) -> Option<debuginfo::TypeSet> {
        if let Some(told) = self.0.roots.get(env, ty) {
            return told;
        }
        let told = debuginfo::described(db, env, ty, &self.0.sets);
        // Another thread may have told of it meanwhile, and the functions share one set.
        self.0.roots.or_insert(env, ty, told)
    }

    /// Tells of the types of the variables of `compiled`, for [`describe_variables`] to find.
    fn ahead(&self, db: &dyn HirDatabase, compiled: &CompiledFunction) {
        if let (Some(_), Some((env, variables))) = (&compiled.source, &compiled.variables) {
            variables.iter().for_each(|it| _ = self.told(db, env, &it.ty));
        }
    }
}

/// What a debugger is told of `variables`, which are of the body of `owner` and are where
/// `debug` has the places of, each in the scope `scopes` has for its binding. A variable of
/// a type that is not told of is left out.
fn describe_variables<'db>(
    db: &'db dyn HirDatabase,
    owner: DefWithBodyId,
    (told, env): (&ToldTypes, &StoredParamEnvAndCrate),
    variables: &[Variable],
    places: &SlotPlaces,
    scopes: &FxHashMap<BindingId, u32>,
    debug: &mut FunctionDebug,
) {
    let body = Body::of(db, owner);
    // Where the type of each variable told of is among the types of the function.
    let mut firsts = FxHashMap::default();
    for variable in variables {
        let name = &body[variable.binding].name;
        // A name a macro of the compiler's made up is no name of the source.
        if name.as_str().starts_with('<') {
            continue;
        }
        let Some(place) = places.of(variable.slot) else { continue };
        let place = match variable.pointer {
            Some(offset) => place.pointee(offset),
            None => place.clone(),
        };
        let ty = variable.ty.as_ref();
        let ty = match firsts.get(&ty) {
            Some(&first) => first,
            None => {
                let Some(described) = told.told(db, env, &variable.ty) else { continue };
                let first = debug.types.len() as u32;
                debug.types.push(described);
                firsts.insert(ty, first);
                first
            }
        };
        debug.variables.push(VariableDebug {
            name: name.as_str().to_owned(),
            param: variable.param,
            ty,
            place,
            scope: scopes.get(&variable.binding).copied(),
        });
    }
    debug.frame_base = places.frame_base.clone();
}

/// The scopes of the body of `owner` that `variables` are bound in, added to `debug`, and
/// which of them each binding is of. The code of a scope is the code of the expressions
/// that names are looked up from it or from a scope inside it for: `rows` has, in the order
/// of the code, where the code of each expression starts, and `len` is where the last ends.
/// A binding of the outermost scope, which a parameter is, is known in all of the function
/// and is not among what is returned.
fn describe_scopes(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    variables: &[Variable],
    rows: &[(u32, MirSpan)],
    len: u32,
    debug: &mut FunctionDebug,
) -> FxHashMap<BindingId, u32> {
    let scopes = ExprScopes::of(db, owner);
    let body = Body::of(db, owner);
    let outermost = scopes.scope_chain(scopes.scope_for(body.root_expr())).last();
    // The scope each binding is an entry of.
    let mut bound = FxHashMap::default();
    let mut seen = FxHashSet::default();
    for (_, &scope) in scopes.scope_by_expr().iter() {
        for scope in scopes.scope_chain(Some(scope)).take_while(|&it| seen.insert(it)) {
            bound.extend(scopes.entries(scope).iter().map(|entry| (entry.binding(), scope)));
        }
    }
    // The scopes told of, outer ones first, and where each is among them.
    let mut told: FxHashMap<ScopeId, u32> = FxHashMap::default();
    let mut of_binding = FxHashMap::default();
    for variable in variables {
        let Some(&scope) = bound.get(&variable.binding).filter(|&&it| Some(it) != outermost) else {
            continue;
        };
        let mut chain: Vec<ScopeId> = scopes
            .scope_chain(Some(scope))
            .take_while(|it| Some(*it) != outermost && !told.contains_key(it))
            .filter(|&it| it == scope || !scopes.entries(it).is_empty())
            .collect();
        chain.reverse();
        for scope in chain {
            let parent =
                scopes.scope_chain(Some(scope)).skip(1).find_map(|it| told.get(&it).copied());
            told.insert(scope, debug.scopes.len() as u32);
            debug.scopes.push(debuginfo::Scope { parent, ranges: Vec::new() });
        }
        of_binding.insert(variable.binding, told[&scope]);
    }
    if told.is_empty() {
        return of_binding;
    }
    // The code that tries the pattern of an arm is of the scope its `match` is in.
    let mut tried = FxHashMap::default();
    for (expr, it) in body.exprs() {
        if let hir_def::hir::Expr::Match { arms, .. } = it {
            for arm in arms.iter() {
                body.walk_pats(arm.pat, &mut |pat| drop(tried.insert(pat, expr)));
            }
        }
    }
    // Other code that is of no expression is of the scope of the code before it.
    let mut scope = None;
    let ends = rows.iter().skip(1).map(|row| row.0).chain([len]);
    for (&(from, span), to) in rows.iter().zip(ends) {
        match span {
            MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => scope = scopes.scope_for(expr),
            MirSpan::PatId(pat) => {
                if let Some(&of) = tried.get(&pat) {
                    scope = scopes.scope_for(of);
                }
            }
            _ => {}
        }
        if from >= to {
            continue;
        }
        for index in scopes.scope_chain(scope).filter_map(|it| told.get(&it)) {
            let ranges = &mut debug.scopes[*index as usize].ranges;
            match ranges.last_mut() {
                Some(last) if last.1 == from => last.1 = to,
                _ => ranges.push((from, to)),
            }
        }
    }
    of_binding
}

/// File, line and column of a place in a body, as rustc reports them, found from where the
/// body is in its file: [`places::source_location`] finds them from the body alone.
#[salsa::tracked]
fn source_location_in_file<'db>(
    db: &'db dyn HirDatabase,
    owner: DefWithBodyId,
    span: MirSpan,
) -> Option<(base_db::FileId, u32, u32)> {
    let (file_id, range) = source_range(db, owner, span)?;
    location_of(db, file_id, range)
}

/// File, line and column of a place in a body for a debugger, as rustc gives code its lines.
/// They are those of [`source_location`] but for what a macro writes: that is of where the
/// macro is called only if the macro is another crate's or the compiler's, and of where the
/// macro has it otherwise, as it is of what a macro writes of the code of `closure`, or of
/// the body when there is none, where that is itself what the macro writes.
#[salsa::tracked]
fn row_location_in_file<'db>(
    db: &'db dyn HirDatabase,
    owner: DefWithBodyId,
    closure: Option<ExprId>,
    span: MirSpan,
) -> Option<(base_db::FileId, u32, u32)> {
    let (file_id, range) = source_range(db, owner, span)?;
    if file_id.macro_file().is_none() {
        return *source_location_in_file(db, owner, span);
    }
    let source_map = &Body::with_source_map(db, owner).1;
    let code = closure.unwrap_or_else(|| Body::of(db, owner).root_expr());
    let within = match source_map.expr_syntax(code) {
        Ok(body) => {
            let start = TextRange::empty(body.value.syntax_node_ptr().text_range().start());
            body.file_id.span_map(db).span_for_range(start).ctx
        }
        Err(_) => return *source_location_in_file(db, owner, span),
    };
    written_place(db, file_id, range, Some(within))
}

/// File, line and column of where `range` of `file_id` is for a debugger: where the macro
/// has it while the macro is the crate's own, and where the first macro that is not is
/// called from, up to the expansion `within` or to the source.
fn written_place(
    db: &dyn HirDatabase,
    file_id: HirFileId,
    range: TextRange,
    within: Option<span::SyntaxContext>,
) -> Option<(base_db::FileId, u32, u32)> {
    let mut at = file_id.span_map(db).span_for_range(TextRange::empty(range.start()));
    let mut kept = at;
    while !at.ctx.is_root() && Some(at.ctx) != within {
        let Some(call) = at.ctx.outer_expn(db) else { break };
        let call = hir_expand::MacroCallId::from(call).loc(db);
        let written = call.to_node(db);
        let range = TextRange::empty(written_range(&written.value).start());
        at = written.file_id.span_map(db).span_for_range(range);
        let own = call.def.krate == call.krate && call.def.kind.is_declarative();
        if !own {
            kept = at;
        }
    }
    line_and_column(db, hir_expand::resolve_span(db, kept))
}

/// The file a place in a body is written in, which may be what a macro expands to, and
/// where in it rustc has the place begin.
fn source_range<'db>(
    db: &'db dyn HirDatabase,
    owner: DefWithBodyId,
    span: MirSpan,
) -> Option<(HirFileId, TextRange)> {
    let source_map = &Body::with_source_map(db, owner).1;
    let source = match span {
        MirSpan::ExprId(expr) => source_map.expr_syntax(expr).ok()?,
        // The block an `async fn` is taken to wrap its body in is not written anywhere, and
        // ends where the body does.
        MirSpan::ExprEnd(expr) => {
            match (source_map.expr_syntax(expr), &Body::of(db, owner)[expr]) {
                (Ok(written), _) => written,
                (Err(_), hir_def::hir::Expr::Block { tail: Some(tail), .. }) => {
                    source_map.expr_syntax(*tail).ok()?
                }
                (Err(_), _) => return None,
            }
        }
        MirSpan::PatId(pat) => source_map.pat_syntax(pat).ok()?,
        MirSpan::BindingId(_) | MirSpan::SelfParam | MirSpan::Unknown => return None,
    };
    if let MirSpan::ExprEnd(_) = span {
        // The brace that closes a block, which is where rustc has a scope left.
        let range = source.value.syntax_node_ptr().text_range();
        let last = range.end().checked_sub(TextSize::new(1)).filter(|it| *it >= range.start());
        return Some((source.file_id, TextRange::new(last?, range.end())));
    }
    let range = match span {
        MirSpan::ExprId(expr) => {
            let root = source.file_id.parse_or_expand(db);
            let node = source.value.syntax_node_ptr().to_node(&root);
            reported_range(db, owner, expr, source.file_id, &node)?
        }
        _ => source.value.syntax_node_ptr().text_range(),
    };
    Some((source.file_id, range))
}

/// The part of an expression that rustc's span of it covers where it panics: a method call
/// from the method's name on, an index through `Index` from its bracket on, and any other
/// expression together with the parentheses it is written in.
fn reported_range(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    expr: ExprId,
    file_id: HirFileId,
    node: &SyntaxNode,
) -> Option<TextRange> {
    let end = node.text_range().end();
    if let Some(call) = ast::MethodCallExpr::cast(node.clone()) {
        return Some(TextRange::new(call.name_ref()?.syntax().text_range().start(), end));
    }
    if let Some(written) = ast::IndexExpr::cast(node.clone())
        && let hir_def::hir::Expr::Index { base, index } = Body::of(db, owner)[expr]
        && !is_builtin_index(InferenceResult::of(db, owner), base, index)
    {
        return Some(TextRange::new(written.l_brack_token()?.text_range().start(), end));
    }
    Some(parenthesized(db, file_id, node))
}

/// The range of an expression with the parentheses around it. rustc's span of an expression
/// includes them when both are written in one place, and not when a macro put the
/// parentheses around what it was passed.
fn parenthesized(db: &dyn HirDatabase, file_id: HirFileId, node: &SyntaxNode) -> TextRange {
    let passed_in = |range: TextRange| {
        file_id.macro_file().is_none()
            || file_id.span_map(db).span_for_range(TextRange::empty(range.start())).ctx.is_root()
    };
    let inner = passed_in(node.text_range());
    node.ancestors()
        .skip(1)
        .take_while(|it| it.kind() == SyntaxKind::PAREN_EXPR && passed_in(it.text_range()) == inner)
        .last()
        .map_or(node.text_range(), |it| it.text_range())
}

/// File, line and column of where a function is declared, from its visibility or `fn` on:
/// what a pointer to a `#[track_caller]` function reports as its caller, as rustc's shim does.
#[salsa::tracked]
fn declaration_location<'db>(
    db: &'db dyn HirDatabase,
    func: FunctionId,
) -> Option<(base_db::FileId, u32, u32)> {
    let source = func.lookup(db).source(db);
    location_of(db, source.file_id, written_range(source.value.syntax()))
}

/// File, line and column a debugger is told a function is declared on, and its code ahead of
/// the first statement is of. They are those of [`declaration_location`] but for a function
/// a macro writes, which is where the macro has it as [`row_location`] has the statements
/// of the body, and as rustc tells. gdb on aarch64 takes the lines after the first one that
/// are above it as more of the prologue, so a function a macro of the crate writes, declared
/// on the line of the call below the macro, had its whole body taken as prologue: stepping
/// into it put the breakpoint past the calls, where a panic in them never got.
#[salsa::tracked]
fn debug_declaration_location<'db>(
    db: &'db dyn HirDatabase,
    func: FunctionId,
) -> Option<(base_db::FileId, u32, u32)> {
    let source = func.lookup(db).source(db);
    if source.file_id.macro_file().is_none() {
        return *declaration_location(db, func);
    }
    written_place(db, source.file_id, written_range(source.value.syntax()), None)
}

/// File, line and column of where the struct or the variant `def` constructs is written,
/// which is where rustc has the code of a constructor from. `None` for a function.
fn constructor_location(
    db: &dyn HirDatabase,
    def: CallableDefId,
) -> Option<(base_db::FileId, u32, u32)> {
    let (file, node) = match def {
        CallableDefId::FunctionId(_) => return None,
        CallableDefId::StructId(it) => {
            let source = it.lookup(db).source(db);
            (source.file_id, source.value.syntax().clone())
        }
        CallableDefId::EnumVariantId(it) => {
            let source = it.lookup(db).source(db);
            (source.file_id, source.value.syntax().clone())
        }
    };
    location_of(db, file, written_range(&node))
}

fn is_thread_local(db: &dyn HirDatabase, id: StaticId) -> bool {
    AttrFlags::query(db, id.into()).contains(AttrFlags::THREAD_LOCAL)
}

/// Whether a value of `ty` is an integer in a register: a `bool`, a `char` or an integer.
fn is_integer(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Uint(_))
}

/// The symbol a function is exported under when an attribute names it: the runtime or the
/// program loading the library looks it up by that name, so it is compiled whether or not
/// anything calls it.
pub fn exported_symbol(db: &dyn HirDatabase, func: FunctionId) -> Option<String> {
    if let Some(name) = AttrFlags::export_name(db, func.into()) {
        return Some(name.as_str().to_owned());
    }
    AttrFlags::query(db, func.into())
        .contains(AttrFlags::NO_MANGLE)
        .then(|| FunctionSignature::of(db, func).name.as_str().to_owned())
}

/// Whether a function is `#[rustc_comptime]`: one the const evaluator alone runs, such as
/// `TypeId::size` over the reflection intrinsics, of which rustc compiles no code.
pub fn compile_time_only(db: &dyn HirDatabase, func: FunctionId) -> bool {
    AttrFlags::has_word(db, func.into(), "rustc_comptime")
}

/// The symbol a static of the program, not of an `extern` block, is exported under when an
/// attribute names it: code of another language finds it by that name, whatever its
/// visibility and whether or not Rust code uses it.
pub fn exported_static(db: &dyn HirDatabase, id: StaticId) -> Option<intern::Symbol> {
    let signature = StaticSignature::of(db, id);
    if signature.flags.contains(StaticFlags::EXTERN) {
        return None;
    }
    AttrFlags::export_name(db, id.into()).or_else(|| {
        let unmangled = AttrFlags::query(db, id.into()).contains(AttrFlags::NO_MANGLE);
        unmangled.then(|| signature.name.symbol().clone())
    })
}

/// The symbol an item of an `extern` block links to.
pub fn link_name(db: &dyn HirDatabase, func: FunctionId) -> Option<String> {
    let ItemContainerId::ExternBlockId(_) = func.lookup(db).container else {
        return None;
    };
    let name = AttrFlags::link_name(db, func.into())
        .unwrap_or_else(|| FunctionSignature::of(db, func).name.symbol().clone());
    Some(as_declared(name.as_str(), global_prefix()))
}

/// The module of a wasm module's imports an item of an `extern` block is one of, on a target
/// whose objects are wasm's.
fn wasm_import_module(db: &dyn HirDatabase, func: FunctionId) -> Option<String> {
    let ItemContainerId::ExternBlockId(block) = func.lookup(db).container else { return None };
    if !Target::current().writes_wasm_objects() {
        return None;
    }
    AttrFlags::wasm_import_module(db, block).map(|it| it.as_str().to_owned())
}

/// The prefix the object writer puts in front of every name declared to it for the target:
/// `_` on Mach-O and on 32-bit Windows, nothing elsewhere.
fn global_prefix() -> Option<char> {
    let triple = target().ok()?.triple();
    let prefixed = triple.binary_format == target_lexicon::BinaryFormat::Macho
        || triple.binary_format == target_lexicon::BinaryFormat::Coff
            && matches!(triple.architecture, target_lexicon::Architecture::X86_32(_));
    prefixed.then_some('_')
}

/// The name an `extern` item written with `link_name` is declared under. One that starts with
/// `\u{1}` is the symbol as the object has it, which nothing is put in front of, as rustc
/// takes it: bindgen writes `"\u{1}_aws_lc_0_44_0_SHA384"` for macOS. The object writer puts
/// the target's `prefix` in front of what is declared, so it comes off here; declared with it,
/// the call named `_\u{1}_aws_lc_0_44_0_SHA384`, and the link found nothing of that name. One
/// that does not start with the prefix is kept whole, and the link says it can't find it.
fn as_declared(name: &str, prefix: Option<char>) -> String {
    let Some(written) = name.strip_prefix('\u{1}') else { return name.to_owned() };
    match prefix {
        None => written.to_owned(),
        Some(prefix) => written.strip_prefix(prefix).unwrap_or(name).to_owned(),
    }
}

/// The symbol a function that isn't exported takes in a crate rustc built nothing of, for the
/// declarations in other crates of the standard library that name it as written: rustc gives
/// it a name of its own. core declares the `#[panic_handler]` as `panic_impl`, and the
/// personality is the one the unwinding tables name.
pub fn linked_symbol(db: &dyn HirDatabase, func: FunctionId) -> Option<String> {
    if let ItemContainerId::ExternBlockId(_) = func.lookup(db).container {
        return None;
    }
    let flags = AttrFlags::query(db, func.into());
    if flags.contains(AttrFlags::LINKED_BY_NAME) {
        if AttrFlags::has_word(db, func.into(), "panic_handler") {
            return Some("panic_impl".to_owned());
        }
        return Some(FunctionSignature::of(db, func).name.as_str().to_owned());
    }
    let lang = flags.lang_item_with_attrs(db, func.into())?;
    (lang == intern::sym::eh_personality).then(|| unwind::PERSONALITY.to_owned())
}

/// How the message of a stand-in for a function ra-jit could not compile starts
/// ([`ObjectCodegen::define_refused`]): what is left of them once linked is what is reachable.
pub const STAND_IN_SAYS: &str = "ra-jit could not compile ";

/// The section of an object, and of what it is linked into, that holds the messages of its
/// stand-ins and nothing else, each a line that starts with [`STAND_IN_SAYS`]: what a link
/// keeps of it is what the program can reach. It is read by name, so a program that merely
/// has the words in its data (blaze itself) is not taken for one with stand-ins. On ELF each
/// message is a section of its own, for the link to take out those nothing reaches; Apple's
/// linker takes them out symbol by symbol from the one section, as it has room for few.
pub fn stand_in_section(format: object::BinaryFormat) -> (&'static str, &'static str) {
    match format {
        object::BinaryFormat::MachO => ("__TEXT", "__ra_jit_stand"),
        // An image's section name has room for 8 bytes.
        object::BinaryFormat::Coff | object::BinaryFormat::Pe => ("", ".rjstand"),
        _ => ("", ".ra_jit.stand_in"),
    }
}

/// The bytes of the object `product` makes, read once `product` is gone: they are written to
/// a file first, which the kernel can keep on disk, so that a large program's object of
/// gigabytes is not in memory twice. Where no such file can be made they are made in memory.
fn write_object(product: cranelift_object::ObjectProduct) -> Result<Vec<u8>> {
    use std::io::{Read as _, Seek as _, Write as _};
    let failed = |e: &dyn fmt::Display| CodegenError::Backend(format!("writing the object: {e}"));
    let Some(mut file) = unnamed_file() else {
        return product.emit().map_err(|e| failed(&e));
    };
    let mut out = std::io::BufWriter::with_capacity(1 << 20, &file);
    product.object.write_stream(&mut out).map_err(|e| failed(&e))?;
    out.flush().map_err(|e| failed(&e))?;
    drop((out, product));
    let len = file.seek(std::io::SeekFrom::End(0)).map_err(|e| failed(&e))?;
    file.rewind().map_err(|e| failed(&e))?;
    let mut object = Vec::with_capacity(len as usize);
    file.read_to_end(&mut object).map_err(|e| failed(&e))?;
    Ok(object)
}

/// A file of the temporary directory that is removed once it is closed, if one can be made.
fn unnamed_file() -> Option<std::fs::File> {
    static MADE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let made = MADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = format!("ra-jit-object-{}-{made}", std::process::id());
    let path = std::env::temp_dir().join(name);
    let file = std::fs::File::options().read(true).write(true).create_new(true).open(&path).ok()?;
    // Windows removes a file that is open once it is closed.
    _ = std::fs::remove_file(&path);
    Some(file)
}

/// Gives the messages of the stand-ins of `product` their bytes, in [`stand_in_section`].
fn place_stand_ins(
    product: &mut cranelift_object::ObjectProduct,
    stand_ins: Vec<(DataId, Box<[u8]>)>,
) {
    use cranelift_object::object::{BinaryFormat, SectionKind};
    let format = product.object.format();
    let (segment, name) = stand_in_section(match format {
        BinaryFormat::MachO => object::BinaryFormat::MachO,
        BinaryFormat::Coff => object::BinaryFormat::Coff,
        BinaryFormat::Wasm => object::BinaryFormat::Wasm,
        _ => object::BinaryFormat::Elf,
    });
    let mut shared = None;
    for (id, message) in stand_ins {
        let mut add = || {
            let (segment, name) = (segment.as_bytes().to_vec(), name.as_bytes().to_vec());
            product.object.add_section(segment, name, SectionKind::ReadOnlyData)
        };
        let section = match format {
            BinaryFormat::Elf => add(),
            _ => *shared.get_or_insert_with(add),
        };
        let symbol = product.data_symbol(id);
        product.object.add_symbol_data(symbol, section, &message, 1);
    }
}

/// Whether a crate declares the `#[global_allocator]`: whether it has the function the
/// attribute's expansion exports for `__rust_alloc` (the attribute itself is gone from the
/// static once expanded). The expansion names the function as it exports it, so each module
/// is asked for the name rather than for the attributes of all its functions.
#[salsa::tracked(returns(copy))]
fn declares_global_allocator(db: &dyn HirDatabase, krate: Crate) -> bool {
    global_allocator_export(db, krate, "__ra_jit_global_alloc").is_some()
}

/// The entries of the global allocator that `alloc` calls by name, with the function the
/// `#[global_allocator]`'s expansion exports for each: what rustc's shim calls, and what ours
/// goes to instead. The number of pointer-sized arguments, and whether one is returned.
const ALLOCATOR_ENTRIES: [(&str, &str, usize, bool); 4] = [
    ("__rust_alloc", "__ra_jit_global_alloc", 2, true),
    ("__rust_dealloc", "__ra_jit_global_dealloc", 3, false),
    ("__rust_realloc", "__ra_jit_global_realloc", 4, true),
    ("__rust_alloc_zeroed", "__ra_jit_global_alloc_zeroed", 2, true),
];

/// The function of `krate` the `#[global_allocator]`'s expansion exports as `exported`.
fn global_allocator_export(
    db: &dyn HirDatabase,
    krate: Crate,
    exported: &str,
) -> Option<FunctionId> {
    let def_map = hir_def::nameres::crate_def_map(db, krate);
    let name = hir_expand::name::Name::new_root(exported);
    def_map.modules(db).find_map(|(module_id, module)| {
        let Some(hir_def::ModuleDefId::FunctionId(id)) = module.scope(db).get(&name).take_values()
        else {
            return None;
        };
        let exports =
            AttrFlags::export_name(db, id.into()).is_some_and(|it| it.as_str() == exported);
        (id.lookup(db).container == ItemContainerId::ModuleId(module_id) && exports).then_some(id)
    })
}

/// The symbol a static of an `extern` block links to.
fn static_link_name(db: &dyn HirDatabase, id: StaticId) -> Option<String> {
    let signature = StaticSignature::of(db, id);
    if !signature.flags.contains(StaticFlags::EXTERN) {
        return None;
    }
    let name =
        AttrFlags::link_name(db, id.into()).unwrap_or_else(|| signature.name.symbol().clone());
    Some(as_declared(name.as_str(), global_prefix()))
}

/// Builds the IR of one function and runs Cranelift on it.
fn compile<'db>(
    db: &'db dyn HirDatabase,
    env: ParamEnvAndCrate<'db>,
    body: Option<&'db MirBody<'db>>,
    tracked: bool,
    checked: bool,
    fill: impl for<'cx> FnOnce(&mut FunctionCx<'cx, 'db>) -> Result<()>,
) -> Result<CompiledFunction> {
    compile_as(db, env, body, tracked, checked, false, fill).map(Produced::code)
}

/// Builds the IR of one function and compiles it with Cranelift, or, for a compile at a level
/// above 1 (`tier`), leaves it to LLVM to compile once the database is let go of.
fn compile_as<'db>(
    db: &'db dyn HirDatabase,
    env: ParamEnvAndCrate<'db>,
    body: Option<&'db MirBody<'db>>,
    tracked: bool,
    checked: bool,
    tier: bool,
    fill: impl for<'cx> FnOnce(&mut FunctionCx<'cx, 'db>) -> Result<()>,
) -> Result<Produced> {
    let isa = &**target()?;
    let mut cg = Recorder::new(db, isa, selected_target()?, checked);
    let tally = backend::Tally::global();
    /// `RA_JIT_PROMOTE=0` leaves every local in its slot of the frame.
    static PROMOTE: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("RA_JIT_PROMOTE").is_none_or(|it| it != "0"));
    let mut ctx = tally.step(Step::Translate, || {
        let mut ctx = build(&mut cg, env, body, tracked, fill)?;
        inline(&mut ctx, &cg.inlined)?;
        if *PROMOTE {
            promote::promote(&mut ctx.func, cg.named_slots());
        }
        Result::Ok(ctx)
    })?;
    let code = match cg.naked.take() {
        // The function is its assembly: what was built of it is only its signature.
        Some((text, syms)) => {
            let assembled = asm::assemble(&isa.triple().to_string(), &text)
                .map_err(CodegenError::Unsupported)?;
            let relocs = assembled.relocs.iter().map(|named| {
                let name = syms.get(named.operand as usize).cloned().flatten()?;
                Some(ModuleReloc {
                    offset: named.offset,
                    kind: named.kind,
                    name,
                    addend: named.addend,
                })
            });
            let Some(relocs) = relocs.collect() else {
                unsupported!("inline assembly that names a symbol");
            };
            Code {
                signature: ctx.func.signature.clone(),
                bytes: assembled.code.clone(),
                align: 16,
                relocs,
                elf_types: assembled
                    .relocs
                    .iter()
                    .filter_map(|named| Some((named.offset, named.elf_type?)))
                    .collect(),
                unwind: None,
                lines: Vec::new(),
                slots: SlotPlaces::default(),
            }
        }
        None if tier => {
            let named = cg.named_slots().collect();
            let (source, variables) = described(&mut cg, env, body);
            let (wants, layouts) = cg.detach();
            return Ok(Produced::Pending(Pending {
                func: ctx.func,
                wants,
                named,
                source,
                variables,
                layouts,
                closure: None,
            }));
        }
        None => Backend::emit(&mut cg, &mut ctx)?,
    };
    let (source, variables) = described(&mut cg, env, body);
    let (requests, layouts) = cg.finish();
    // What a build compiled, which a compile at a level above 1 is not part of.
    if !tier {
        COMPILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(Produced::Code(CompiledFunction {
        requests,
        code,
        source,
        closure: None,
        variables,
        layouts,
    }))
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
    wants: fragment::Wants,
    named: Vec<ir::StackSlot>,
    source: Option<(DefWithBodyId, Vec<MirSpan>)>,
    variables: Option<(StoredParamEnvAndCrate, Vec<Variable>)>,
    layouts: Vec<(String, u64, u64)>,
    closure: Option<ExprId>,
}

impl Pending {
    pub(crate) fn func(&self) -> &ir::Function {
        &self.func
    }

    /// The code LLVM makes of the function, at the level `request` asks for.
    pub(crate) fn produce(mut self, request: &tier::Request) -> Result<CompiledFunction> {
        let code = tier::emit(&mut self.wants, &self.named, &mut self.func, request)?;
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

/// What tells the code of a function's lines and variables: the statements of `body` its
/// instructions were built for, and its locals with a name.
fn described<'db>(
    cg: &mut Recorder<'db>,
    env: ParamEnvAndCrate<'db>,
    body: Option<&'db MirBody<'db>>,
) -> (Option<(DefWithBodyId, Vec<MirSpan>)>, Option<(StoredParamEnvAndCrate, Vec<Variable>)>) {
    let source = match body.map(|body| body.owner) {
        Some(InferBodyId::DefWithBodyId(owner)) => Some((owner, cg.take_spans())),
        _ => None,
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
        .collect::<Vec<_>>();
    (source, Some((env.store(), variables)).filter(|it| !it.1.is_empty()))
}

/// Whether the crate has `panic = "abort"`. The standard library's crates are compiled
/// before, for whichever the program has, and whatever of theirs is compiled again is in the
/// program's crates.
fn panics_abort(db: &dyn HirDatabase, krate: Crate) -> bool {
    let abort =
        cfg::CfgAtom::KeyValue { key: intern::sym::panic, value: intern::Symbol::intern("abort") };
    !krate.data(db).origin.is_lang() && krate.cfg_options(db).check_atom(&abort)
}

fn build<'db>(
    cg: &mut Recorder<'db>,
    env: ParamEnvAndCrate<'db>,
    body: Option<&'db MirBody<'db>>,
    tracked: bool,
    fill: impl for<'cx> FnOnce(&mut FunctionCx<'cx, 'db>) -> Result<()>,
) -> Result<cranelift_codegen::Context> {
    let interner = DbInterner::new_with(cg.db, env.krate);
    let mut ctx = cranelift_codegen::Context::new();
    ctx.func.signature.call_conv = cg.isa.default_call_conv();
    // Source locations are kept as distances from the first one set, and the distance of
    // the one just below it is taken for none at all.
    ctx.func.params.ensure_base_srcloc(ir::SourceLoc::new(0));
    let mut builder_ctx = FunctionBuilderContext::new();
    let aborts = panics_abort(cg.db, env.krate);
    let mut fx = FunctionCx {
        cg,
        body,
        env,
        infcx: interner.infer_ctxt().build(TypingMode::PostAnalysis),
        interner,
        builder: FunctionBuilder::new(&mut ctx.func, &mut builder_ctx),
        locals: ArenaMap::new(),
        blocks: ArenaMap::new(),
        layouts: Default::default(),
        glue: Default::default(),
        inner_pad: None,
        unwind_to: None,
        pads: Vec::new(),
        exception: None,
        argument_slots: Vec::new(),
        free_argument_slots: Vec::new(),
        given_away: None,
        block: None,
        in_cleanup: false,
        aborts,
        guarded: false,
        ret_mode: PassMode::Ignore,
        tracked,
        variadic: false,
        abi: ExternAbi::Rust,
        location: None,
        span: MirSpan::Unknown,
        constant_locals: Vec::new(),
        registers: FxHashMap::default(),
        args: None,
        suspended: None,
        held_at: Vec::new(),
    };
    fill(&mut fx)?;
    fx.note_variables();
    fx.builder.seal_all_blocks();
    let config = fx.cg.target_config();
    let guarded = fx.guarded;
    fx.builder.finalize(config);
    if guarded && let Some(entry) = ctx.func.layout.entry_block() {
        // The slot a guarded call looks for an exception in starts empty.
        use cranelift_codegen::cursor::{Cursor as _, FuncCursor};
        let mut cursor = FuncCursor::new(&mut ctx.func).at_first_insertion_point(entry);
        let frame = cursor.ins().get_frame_pointer(ptr());
        let none = cursor.ins().iconst(ptr(), 0);
        cursor.ins().store(MEM, none, frame, unwind::seh::ARRIVAL);
    }
    Ok(ctx)
}

/// How many compiled instances and inlinable bodies salsa keeps the value of once a revision
/// is over; 0 keeps every one.
pub fn set_code_capacity(db: &mut dyn HirDatabase, compiled: usize, inlinable_bodies: usize) {
    compiled_instance::set_lru_capacity(db, compiled);
    inlinable::set_lru_capacity(db, inlinable_bodies);
}

/// The body of a small function, built to be put where the function is called.
#[salsa::tracked(returns(ref), no_eq, cycle_result = inlinable_cycle, lru = 0)]
fn inlinable(
    db: &dyn HirDatabase,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Option<fragment::Inlinable> {
    tier::plain(|| small_body(db, instance, env))
}

fn small_body(
    db: &dyn HirDatabase,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Option<fragment::Inlinable> {
    /// The most blocks and statements the body of a function that is inlined has, and
    /// the most instructions it is built to with what it calls inlined into it.
    const BLOCKS: usize = 4;
    const STATEMENTS: usize = 12;
    const INSTRUCTIONS: usize = 64;
    let InstanceDef::Fn(func) = instance.def else { return None };
    let generic = db.mir_body(func.into()).ok()?;
    let statements: usize = generic.basic_blocks.iter().map(|it| it.1.statements.len()).sum();
    if generic.basic_blocks.len() > BLOCKS
        || statements > STATEMENTS
        || !generic.closures.is_empty()
        || generic.coroutine.is_some()
    {
        return None;
    }
    let body = instance_body(db, &instance, env.clone()).ok()?;
    let mut cg = Recorder::new(db, &**target().ok()?, selected_target().ok()?, instance.checked);
    cg.unlocated = true;
    let args = instance.args.clone();
    let mut ctx = build(&mut cg, env.as_ref(db), Some(&*body), false, |fx| {
        fx.args = Some(args);
        fx.codegen()
    })
    .ok()?;
    if cg.naked.is_some() || cg.opaque {
        return None;
    }
    inline(&mut ctx, &cg.inlined).ok()?;
    if ctx.func.dfg.num_insts() > INSTRUCTIONS {
        return None;
    }
    let pointees = std::mem::take(&mut cg.pointees);
    fragment::Inlinable::of(ctx.func, cg.finish().0, pointees, Vec::new())
}

fn inlinable_cycle(
    _: &dyn HirDatabase,
    _: salsa::Id,
    _: Instance,
    _: StoredParamEnvAndCrate,
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
    let mut bodies = Bodies { bodies: inlined, put: Vec::new() };
    ctx.inline(&mut bodies).map_err(|err| CodegenError::Unsupported(err.to_string()))?;
    // Cranelift copies a symbol of the callee with the name it has among the callee's
    // names, which among the caller's is another symbol's.
    let ends = bodies.put.iter().skip(1).map(|it| it.insts).chain([ctx.func.dfg.num_insts()]);
    for (put, end) in bodies.put.iter().zip(ends) {
        let body = &inlined[put.body];
        // Cranelift makes the instructions of a body it inlines in the order it visits the
        // body's, and gives them no source location: an `offset`'s mark goes by that order,
        // where every instruction is the one it is made from (a return becomes a jump).
        let order: Vec<ir::Inst> = cranelift_codegen::traversals::Dfs::new()
            .pre_order_iter(body)
            .flat_map(|block| body.layout.block_insts(block))
            .collect();
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

/// The store as Cranelift asks for it: one of these per function compiled.
struct IncrementalCache(&'static store::Store);

impl cranelift_codegen::incremental_cache::CacheKvStore for IncrementalCache {
    fn get(&self, key: &[u8]) -> Option<Cow<'_, [u8]>> {
        let key = <&[u8; store::KEY]>::try_from(key).ok()?;
        self.0.lookup(key).map(Cow::Owned)
    }

    fn insert(&mut self, key: &[u8], val: Vec<u8>) {
        if let Ok(key) = <&[u8; store::KEY]>::try_from(key) {
            self.0.insert(key, &val);
        }
    }
}

/// Runs Cranelift on a finished function. Cranelift panics on IR it has no lowering for
/// (i128 corners, for one); that must cost us one function, not the process.
fn emit(ctx: &mut cranelift_codegen::Context) -> Result<Code> {
    if Target::current().is_wasm() {
        return emit_wasm(&ctx.func, Target::current().calls());
    }
    let large = ctx.func.dfg.num_insts() > SINGLE_PASS;
    let mut isa = &**if large { target_for_large()? } else { target()? };
    let func = &ctx.func;
    if !isa.flags().preserve_frame_pointers()
        && (backend::frame_pointers()
            || func.layout.entry_block().is_some_and(|entry| {
                func.layout
                    .block_insts(entry)
                    .any(|inst| func.dfg.insts[inst].opcode() == ir::Opcode::GetFramePointer)
            }))
    {
        isa = &**target_with_frame_pointer()?;
    }
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match store::Store::global() {
            Some(store) => ctx
                .compile_with_cache(isa, &mut IncrementalCache(store), &mut Default::default())
                .map(drop),
            None => ctx.compile(isa, &mut Default::default()).map(drop),
        }
        .map_err(|e| format!("{:?}", e.inner))?;
        backend::Tally::global().passes();
        let Some(compiled) = ctx.compiled_code() else {
            return Err("no compiled code".to_owned());
        };
        // Relocations against the function itself get its id when it is stitched. One below
        // `u32::MAX` — `from_u32(u32::MAX)` asserts the reserved value away in debug builds.
        let own = FuncId::from_u32(u32::MAX - 1);
        Ok(Code {
            signature: ctx.func.signature.clone(),
            bytes: compiled.buffer.data().to_vec(),
            align: compiled.buffer.alignment.into(),
            relocs: compiled
                .buffer
                .relocs()
                .iter()
                .map(|reloc| ModuleReloc::from_mach_reloc(reloc, &ctx.func, own))
                .collect(),
            elf_types: Vec::new(),
            unwind: FunctionUnwind::of(isa, ctx)?,
            lines: LineRow::of_ranges(
                compiled.buffer.get_srclocs_sorted().iter().map(|it| (it.start, it.loc)),
            ),
            slots: SlotPlaces::default(),
        })
    }));
    match outcome {
        Ok(Ok(code)) => Ok(code),
        Ok(Err(e)) => Err(CodegenError::Backend(e)),
        Err(panic) => {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("unknown");
            Err(CodegenError::Backend(format!("backend panic: {message}")))
        }
    }
}

/// Lowers a finished function to WebAssembly. The bytes are a [`wasm::WasmFunction`], which
/// goes through the object like machine code and is made a module of by a linker.
fn emit_wasm(func: &cranelift_codegen::ir::Function, calls: Calls) -> Result<Code> {
    let (function, mut relocs) = wasm::lower(func, calls).map_err(CodegenError::Unsupported)?;
    let (bytes, body) = function.encode().map_err(CodegenError::Backend)?;
    for reloc in &mut relocs {
        reloc.offset += body;
    }
    Ok(Code {
        signature: func.signature.clone(),
        bytes,
        align: 1,
        relocs,
        elf_types: Vec::new(),
        unwind: None,
        lines: Vec::new(),
        slots: SlotPlaces::default(),
    })
}

/// The body of a non-generic function, with the types it only knows as `impl Trait`
/// resolved.
fn root_body<'db>(
    db: &'db dyn HirDatabase,
    func: FunctionId,
) -> Result<(&'db MirBody<'db>, ParamEnvAndCrate<'db>)> {
    let owner: InferBodyId<'db> = func.into();
    let tally = backend::Tally::global();
    tally.step(Step::Infer, || _ = InferenceResult::of(db, DefWithBodyId::from(func)));
    let body = tally.step(Step::Mir, || compiled_mir_body(db, owner));
    let body = body.map_err(|e| CodegenError::Frontend(format!("mir: {e:?}")))?;
    let krate = owner.module(db).krate(db);
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
            .step(Step::Monomorphize, || match mir_opt_level() {
                MirOptLevel::Off => db.monomorphized_runtime_mir_body(owner, args, env.store()),
                level => db.monomorphized_optimized_mir_body(owner, level, args, env.store()),
            })
            .map_err(|e| CodegenError::Frontend(format!("mir: {e:?}")))?
    } else {
        body
    };
    Ok((body, env))
}

impl<'a, 'db> ObjectCodegen<'a, 'db> {
    fn new(
        db: &'db dyn HirDatabase,
        symbols: &'a dyn SymbolResolver,
        rejected: &'a FxHashMap<Instance, Rejected>,
        own_statics: OwnStatics,
        on_demand: &'a FxHashSet<FunctionId>,
        base: Option<&'a patch::Base>,
        hydrate: bool,
    ) -> Result<Self> {
        let start = stdx::time::Instant::now();
        let isa = target()?.clone();
        // A program's own link takes out what nothing reaches section by section, as of
        // rustc's objects, and what only that refers to need be defined nowhere: bun's calls of
        // Windows on Linux. The daemon's links keep everything for edits to be spliced into.
        // Apple's linker takes out symbols, and has room for 255 sections.
        let sections = backend::sections_of_their_own()
            && isa.triple().binary_format == target_lexicon::BinaryFormat::Elf;
        let mut builder = ObjectBuilder::new(isa, "ra-jit", default_libcall_names())?;
        // A wasm object is written from this one, a segment per piece of data aligned as its
        // section is: a section shared by several has the largest alignment of them.
        let data_sections = sections || Target::current().writes_wasm_objects();
        builder.per_function_section(sections).per_data_object_section(data_sections);
        let mut module = ObjectModule::new(builder);
        let unwind = UnwindContext::new(&mut module).map_err(CodegenError::Backend)?;
        let built = start.elapsed();
        // A patch is added to a program that is linked already, and is read with it.
        let apart = symbols.debug_apart().filter(|_| base.is_none());
        let elf = module.isa().triple().binary_format == target_lexicon::BinaryFormat::Elf;
        let debug = symbols.debug_directory().and_then(|directory| {
            DebugContext::new(
                module.isa(),
                &directory,
                symbols.describes_variables(),
                apart,
                sections && symbols.debug_grouped(),
            )
            .map(|context| debuginfo::Describer::new(context, elf))
        });
        let debugged = cached::Told::of(debug.is_some(), symbols.describes_variables());
        let described = start.elapsed();
        let mixed = settled::Mixed::of(db, symbols, &own_statics.crates);
        let mixed_at = start.elapsed();
        // Without an identity for such a program, nothing of it is kept or found.
        let program = mixed.as_ref().map(|it| it.key);
        let store = store::Store::global().filter(|_| program != Some(None));
        // A patch calls the base's copy of what the last build kept, so it does not wait for
        // the store to have it: after a build from nothing, writing it takes longer than the
        // build's own compile.
        if let Some(store) = store.filter(|_| base.is_none()) {
            store.settled();
        }
        let settled_at = start.elapsed();
        // A patch names what its base has from the store as the base did, to call it there.
        let names = match base.and_then(|it| it.frozen_ids()) {
            Some(ids) => stable::StableNames::new(db, symbols, own_statics.crates.clone(), true)
                .frozen(ids.clone()),
            None => stable::StableNames::new(db, symbols, own_statics.crates.clone(), hydrate),
        };
        if std::env::var_os("RA_JIT_STATS").is_some() {
            let us = |it: std::time::Duration| it.as_micros();
            eprintln!(
                "setup-us: module={} debug={} mixed={} settled={} names={}",
                us(built),
                us(described - built),
                us(mixed_at - described),
                us(settled_at - mixed_at),
                us(start.elapsed() - settled_at),
            );
        }
        Ok(ObjectCodegen {
            db,
            symbols,
            module,
            unwind,
            debug,
            told: TOLD_UNNAMED.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default(),
            writer: None,
            imports: FxHashMap::default(),
            assemblies: FxHashMap::default(),
            instances: FxHashMap::default(),
            pending: Vec::new(),
            envs: Vec::new(),
            homes: home::Homes::default(),
            ahead: FxHashSet::default(),
            unit_roots: FxHashMap::default(),
            unit_instances: FxHashMap::default(),
            unit_tried: FxHashSet::default(),
            forwarded: FxHashSet::default(),
            rustc_futures: FxHashSet::default(),
            patching: false,
            base,
            replaced: Default::default(),
            own_static_order: Vec::new(),
            on_demand,
            wanted: Vec::new(),
            on_demand_crates: on_demand.iter().map(|func| func.krate(db)).collect(),
            on_demand_by_crate: None,
            crates_wanted: FxHashSet::default(),
            rejected,
            layouts: FxHashMap::default(),
            phases: Phases::default(),
            shapes: std::env::var_os("RA_JIT_SHAPES")
                .and_then(|path| {
                    use std::io::Write;
                    let mut file =
                        std::fs::File::options().append(true).create(true).open(path).ok()?;
                    writeln!(file, "R").ok()?;
                    Some(file)
                })
                .map(std::io::BufWriter::new),
            bodies: 0,
            vtables: FxHashMap::default(),
            statics: FxHashMap::default(),
            frozen_statics: FxHashMap::default(),
            imported_statics: FxHashMap::default(),
            shared_statics: FxHashMap::default(),
            rustc_values: patch::RustcValues::default(),
            consts: FxHashMap::default(),
            source_paths: FxHashMap::default(),
            builder_ctx: FunctionBuilderContext::new(),
            location_counter: 0,
            names,
            hydrate,
            stable_names: FxHashMap::default(),
            kept: Kept::default(),
            later: Vec::new(),
            // A patch gets these from its base, and a check compiles them to compare.
            entries: store
                .filter(|_| base.is_none() && std::env::var_os("RA_JIT_CACHE_CHECK").is_none())
                .map(|store| {
                    let (home, library) = (symbols.cargo_home(), symbols.library_sources());
                    std::sync::Arc::new(cached::Entries::new(
                        store,
                        debugged,
                        program.flatten(),
                        home,
                        library,
                    ))
                }),
            named: FxHashMap::default(),
            found: Vec::new(),
            found_pending: Vec::new(),
            named_vtables: FxHashMap::default(),
            type_id_shims: FxHashMap::default(),
            type_ids: FxHashMap::default(),
            misfit: false,
            refused: None,
            own_statics,
            mixed,
            elf_relocs: Vec::new(),
            weak_imports: Vec::new(),
            unstitched: FxHashSet::default(),
            hidden: Vec::new(),
            stand_ins: Vec::new(),
            custom_sections: Vec::new(),
            tiers: None,
            found_instances: FxHashMap::default(),
            stored_roots: FxHashMap::default(),
            root_names: FxHashMap::default(),
        })
    }

    /// The object, and the files of what a debugger reads that is kept apart from it.
    fn finish(mut self) -> Result<(Vec<u8>, Vec<Sidecar>)> {
        self.make_later();
        if Target::current().writes_wasm_objects() {
            return Ok((self.finish_wasm()?, Vec::new()));
        }
        self.unwind.name_personality(&mut self.module).map_err(CodegenError::Backend)?;
        let mut product = self.module.finish();
        place_stand_ins(&mut product, std::mem::take(&mut self.stand_ins));
        for reloc in &self.elf_relocs {
            reloc.add(&mut product).map_err(CodegenError::Backend)?;
        }
        for &import in &self.weak_imports {
            let symbol = match import {
                cranelift_module::FuncOrDataId::Func(id) => product.function_symbol(id),
                cranelift_module::FuncOrDataId::Data(id) => product.data_symbol(id),
            };
            product.object.symbol_mut(symbol).weak = true;
        }
        for &id in &self.hidden {
            let symbol = product.function_symbol(id);
            product.object.symbol_mut(symbol).scope =
                cranelift_object::object::SymbolScope::Linkage;
        }
        self.unwind.emit(&mut product).map_err(CodegenError::Backend)?;
        let apart = match self.debug {
            Some(debug) => backend::Tally::global()
                .step(Step::Describe, || debug.emit(&mut product))
                .map_err(CodegenError::Backend)?,
            None => Vec::new(),
        };
        // Freeing every type told of, and every entry read from the store, takes as long as
        // writing the object: another thread does it, where there is one to start.
        let freed = (std::mem::take(&mut self.told), self.entries.take());
        if cfg!(target_family = "wasm") {
            drop(freed);
        } else {
            std::thread::spawn(move || drop(freed));
        }
        let macho = product.object.format() == cranelift_object::object::BinaryFormat::MachO;
        let mut object = write_object(product)?;
        if macho {
            debuginfo::settle_macho(&mut object).map_err(CodegenError::Backend)?;
        }
        Ok((object, apart))
    }

    /// The object as a relocatable wasm object. Nothing unwinds there, so there are no
    /// unwind tables to add.
    fn finish_wasm(self) -> Result<Vec<u8>> {
        let mut signatures = FxHashMap::default();
        for (_, declared) in self.module.declarations().get_functions() {
            let Some(name) = &declared.name else { continue };
            let signature =
                wasm::WasmSignature::of(&declared.signature).map_err(CodegenError::Backend)?;
            signatures.insert(name.clone(), signature);
        }
        let crates: FxHashSet<Crate> = self.envs.iter().map(|(_, env)| env.krate).collect();
        let features = crates.into_iter().flat_map(|krate| {
            let values = krate.cfg_options(self.db).get_cfg_values("target_feature");
            values.map(|it| it.as_str().to_owned()).collect::<Vec<_>>()
        });
        let mut custom_sections = self.custom_sections;
        custom_sections.push(wasm_object::target_features(features));
        let mut product = self.module.finish();
        place_stand_ins(&mut product, self.stand_ins);
        let elf = product.emit().map_err(|e| CodegenError::Backend(e.to_string()))?;
        wasm_object::from_elf(&elf, &signatures, &custom_sections).map_err(CodegenError::Backend)
    }

    fn want(&mut self, func: FunctionId) {
        if self.on_demand.contains(&func) {
            self.wanted.push(func);
        }
    }

    /// Wants every function compiled on demand of the crates that code nothing walked may
    /// call. Each crate's are wanted once: this was a walk of all of them for every static
    /// rustc's code has, which made the build in full of a large program wait on one thread
    /// (#259).
    fn want_all_called(&mut self, unseen: &Unseen) {
        let db = self.db;
        let on_demand = self.on_demand;
        let by_crate = self.on_demand_by_crate.get_or_insert_with(|| {
            let mut by_crate: FxHashMap<Crate, Vec<FunctionId>> = FxHashMap::default();
            for &func in on_demand {
                by_crate.entry(func.krate(db)).or_default().push(func);
            }
            by_crate
        });
        for (&krate, funcs) in by_crate.iter() {
            if unseen.calls(krate) && self.crates_wanted.insert(krate) {
                self.wanted.extend_from_slice(funcs);
            }
        }
    }

    /// Wants the functions that `func` and the instances it calls refer to, for when `func`
    /// itself is left to rustc, and says what of the code it reaches can't be compiled to find
    /// them.
    fn want_reachable_from(&mut self, func: FunctionId) -> Unseen {
        let db = self.db;
        let Ok(compiled) = compiled_root(db, func).as_ref() else {
            return Unseen { all: false, crates: FxHashSet::from_iter([func.krate(db)]) };
        };
        self.want_requested(&compiled.requests)
    }

    /// Wants the functions the requests and the instances they name refer to, without putting
    /// any of it in the object.
    fn want_requested(&mut self, requests: &'db Requests) -> Unseen {
        let db = self.db;
        let (mut seen, mut seen_statics) = (FxHashSet::default(), FxHashSet::default());
        let mut unseen = Unseen::default();
        let mut stack = vec![requests];
        while let Some(requests) = stack.pop() {
            for request in &requests.funcs {
                let FuncTarget::Instance(instance, env) = &request.target else { continue };
                match instance.def {
                    InstanceDef::Fn(func) if instance.args.as_ref().is_empty() => self.want(func),
                    _ if !seen.insert(instance) => {}
                    _ => match compiled_instance(db, instance.clone(), env.clone()) {
                        Ok(compiled) => stack.push(&compiled.requests),
                        Err(_) => match home::mentioned_crates(db, instance) {
                            Some(crates) => unseen.crates.extend(crates),
                            None => unseen.all = true,
                        },
                    },
                }
            }
            for request in &requests.datas {
                if let DataRequest::Static(id) = request
                    && seen_statics.insert(*id)
                    && let Ok(compiled) = compiled_static(db, *id).as_ref()
                {
                    stack.push(&compiled.requests);
                }
            }
        }
        unseen
    }

    /// Puts a non-generic function in the object, exported under its rustc symbol name.
    fn define_function(&mut self, func: FunctionId) -> Result<String> {
        let db = self.db;
        // Whether its code is compiled or from the store.
        if let Some(symbol) = self.symbols.function(func)
            && rustcs_callers_pass_scalable(db, self.symbols, func)
        {
            unsupported!(
                "`{symbol}`, which rustc's callers pass scalable vectors in SVE registers"
            );
        }
        let stored = match self.stored_roots.get(&func) {
            Some(name) => name.clone(),
            None => self.stored_root(func),
        };
        if let Some(name) = stored
            && let Some(symbol) = self.symbols.function(func)
            && let Some(entry) = self.entries.as_ref().and_then(|it| it.get(&name))
        {
            let id =
                self.module.declare_function(&symbol, Linkage::Export, entry.code.signature())?;
            if let Err(e) = self.stitch_entry(&name, id) {
                eprintln!("entry of `{name}` does not fit: {e}");
                cached::Entries::ban(&name);
                self.misfit = true;
            }
            return Ok(symbol);
        }
        db.mir_body(func.into()).map_err(|e| CodegenError::Frontend(format!("mir: {e:?}")))?;
        let Some(symbol) = self.symbols.function(func) else {
            // FIXME: with a `main` of ours only what it reaches is compiled, and a function
            // left to rustc can still call one private to its object.
            let private = self.on_demand_crates.is_empty() && self.symbols.private(func);
            return Err(if private { CodegenError::Private } else { CodegenError::NoSymbol });
        };
        let start = stdx::time::Instant::now();
        let tiers = self.tiers;
        let tiered = tiers.and_then(|it| it.code(db, &tier::Node(tier::Kind::Root(func))));
        let unit = self.unit_roots.remove(&func).filter(|_| tiered.is_none());
        let compiled = match (tiered, &unit) {
            (Some(code), _) => code,
            (None, Some(unit)) => unit,
            (None, None) => compiled_root(db, func).as_ref().map_err(Clone::clone)?,
        };
        let took = start.elapsed();
        self.phases.compile += took;
        let id =
            self.module.declare_function(&symbol, Linkage::Export, &compiled.code.signature)?;
        self.shape_origin(id, InstanceDef::Fn(func), &func, false, took);
        let ids = match self.stitch(id, compiled) {
            Ok(ids) => ids,
            Err(e) => {
                self.unstitched.insert(id);
                return Err(e);
            }
        };
        let of = later::Of::Root { func, symbol: symbol.clone() };
        let mut later = later::Later::new(&self.module, id, of, compiled, ids);
        // The rows of the Cranelift code are not those of LLVM's.
        later.unit_rows = unit.as_ref().filter(|_| self.debug.is_some()).map(|it| rows_of(db, it));
        (later.tiered, later.keeps) =
            (tiered.is_some(), tiered.is_none() && self.roots_from_store());
        match (tiered, &unit) {
            (None, None) if self.defers() => {
                let compiled = compiled_root(db, func).as_ref().map_err(Clone::clone)?;
                self.later.push(later::Later { compiled, ..later });
            }
            _ => self.make_now(&later),
        }
        Ok(symbol)
    }

    /// Whether the types of variables are told of. A build that tells a debugger of its lines
    /// alone writes none of them, so describing them would be work thrown away.
    fn tells_variables(&self) -> bool {
        self.debug.is_some() && self.symbols.describes_variables()
    }

    /// Whether the functions of the build are compiled in units ([`units`]).
    fn units(&self) -> bool {
        #[cfg(feature = "llvm")]
        return self.base.is_none() && units::enabled();
        #[cfg(not(feature = "llvm"))]
        false
    }

    /// Whether the checkout's own functions are kept in the store and put in the object from
    /// it: in a build that is no patch, with what rustc built linked beside it or not, where
    /// every one is a root under a symbol of ours. A build spliced into rustc's executable
    /// names what only that one has.
    fn roots_from_store(&self) -> bool {
        self.hydrate
            && self.base.is_none()
            && self.tiers.is_none()
            && self.entries.is_some()
            && !self.symbols.onto_rustcs()
    }

    /// The name of `func` when the store has its code whole, for a root no round asked about.
    fn stored_root(&mut self, func: FunctionId) -> Option<StableName> {
        if !self.roots_from_store() {
            return None;
        }
        let name = self.stable_name(&root_instance(self.db, func))?;
        let symbols = self.symbols;
        let usable = |entries: &cached::Entries| {
            entries.usable(&name, &|symbol| symbols.provides(symbol) && symbols.linked(symbol))
        };
        let stored =
            stable::StableNames::of_checkout(&name) && self.entries.as_deref().is_some_and(usable);
        let stored = stored.then_some(name);
        self.stored_roots.insert(func, stored.clone());
        stored
    }

    /// Names `roots` on every core, and finds which of them the store has, for them not to be
    /// compiled ahead.
    fn find_stored_roots(&mut self, roots: &[FunctionId]) {
        if !self.roots_from_store() {
            return;
        }
        let Some(entries) = self.entries.clone() else { return };
        let roots: Vec<FunctionId> =
            roots.iter().copied().filter(|it| !self.stored_roots.contains_key(it)).collect();
        if roots.is_empty() {
            return;
        }
        let roots = &roots[..];
        let start = stdx::time::Instant::now();
        let owned = &self.own_statics.crates;
        let next = std::sync::atomic::AtomicUsize::new(0);
        let named = std::sync::Mutex::new(Vec::new());
        let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
        self.symbols.in_parallel(threads, &|db, symbols, _| {
            let names = stable::StableNames::new(db, symbols, owned.clone(), true);
            let mut here = Vec::new();
            while let Some(&func) =
                roots.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
            {
                let Some(name) = names.instance(&root_instance(db, func)) else { continue };
                here.push((func, StableName::from(name)));
            }
            named.lock().unwrap_or_else(|e| e.into_inner()).extend(here);
        });
        let named = named.into_inner().unwrap_or_else(|e| e.into_inner());
        // Every entry the roots need, read at once: one by one, each read unpacked a block.
        let of_checkout = named.iter().filter(|(_, name)| stable::StableNames::of_checkout(name));
        entries.prefetch(of_checkout.map(|(_, name)| name.clone()));
        let next = std::sync::atomic::AtomicUsize::new(0);
        let found = std::sync::Mutex::new(Vec::new());
        self.symbols.in_parallel(threads, &|_, symbols, _| {
            let mut here = Vec::new();
            while let Some((func, name)) =
                named.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
            {
                let usable = |symbol: &str| symbols.provides(symbol) && symbols.linked(symbol);
                let stored =
                    stable::StableNames::of_checkout(name) && entries.usable(name, &usable);
                here.push((*func, name.clone(), stored));
            }
            found.lock().unwrap_or_else(|e| e.into_inner()).extend(here);
        });
        let found = found.into_inner().unwrap_or_else(|e| e.into_inner());
        let mut stored = 0;
        for (func, name, whole) in found {
            self.root_names
                .entry(name.clone())
                .and_modify(|it| *it = it.filter(|&it| it == func))
                .or_insert(Some(func));
            stored += usize::from(whole);
            self.stored_roots.insert(func, whole.then_some(name));
        }
        if std::env::var_os("RA_JIT_STATS").is_some() {
            eprintln!("stored roots: {stored} of {} in {:.1?}", roots.len(), start.elapsed());
        }
    }

    /// Puts the `main` the C runtime calls in the object.
    fn define_start(&mut self, main: FunctionId) -> Result<()> {
        let compiled = compiled_start(self.db, main).as_ref().map_err(Clone::clone)?;
        let (symbol, _) = Target::current().entry();
        let id = self.module.declare_function(symbol, Linkage::Export, &compiled.code.signature)?;
        self.stitch(id, compiled)?;
        Ok(())
    }

    /// What rustc's allocator shim puts in a program: the entries of the global allocator
    /// that `alloc` calls by name, going to the `#[global_allocator]`'s (the functions its
    /// expansion exports) or else to std's defaults. Only in a program with nothing of
    /// rustc's: one linked with rustc's `alloc` has rustc's shim. `krate` is one of the
    /// program's, whose lang items are every crate's.
    fn define_allocator_shim(&mut self, krate: Crate) -> Result<()> {
        let db = self.db;
        if !self.symbols.nothing_of_rustc() {
            return Ok(());
        }
        let Some(boxed) = DbInterner::new_with(db, krate).lang_items().OwnedBox else {
            return Ok(());
        };
        let program = linked_crates(db, self.own_statics.crates.iter().copied());
        if !program.contains(&boxed.krate(db)) {
            return Ok(());
        }
        let declared = program.into_iter().any(|it| declares_global_allocator(db, it));
        for (name, ours, params, returns) in ALLOCATOR_ENTRIES {
            let default = format!("__rdl_{}", name.trim_start_matches("__rust_"));
            let target = if declared { ours } else { &default };
            self.define_forwarding(name, Linkage::Export, Some(target), params, returns)?;
        }
        self.define_forwarding(
            "__rust_alloc_error_handler",
            Linkage::Export,
            Some("__rdl_alloc_error_handler"),
            2,
            false,
        )?;
        self.define_forwarding(
            "__rust_no_alloc_shim_is_unstable_v2",
            Linkage::Export,
            None,
            0,
            false,
        )
    }

    /// rustc's entries of the global allocator in a program linked with rustc's code whose
    /// `#[global_allocator]` is in one of `crates`, going to the functions rust-analyzer's
    /// expansion exports, which are compiled with the rest. rustc's expansion has the
    /// allocator's methods inlined into them at opt-level 1 and up, so they run the source as
    /// rustc compiled it, and nothing else calls those methods. A program whose shim already
    /// goes to ours (a stand-in's forwarding allocator, `stub.rs`) defines that function.
    fn define_rustc_allocator_entries(&mut self, crates: &FxHashSet<Crate>) -> Result<()> {
        let db = self.db;
        if self.symbols.nothing_of_rustc() || self.symbols.defines(ALLOCATOR_ENTRIES[0].1) {
            return Ok(());
        }
        for &krate in crates {
            if !declares_global_allocator(db, krate) {
                continue;
            }
            for (name, ours, params, returns) in ALLOCATOR_ENTRIES {
                let Some(rustcs) = self.symbols.path(&format!("__rustc::{name}")) else {
                    continue;
                };
                let Some(func) = global_allocator_export(db, krate, ours) else {
                    let krate = fncache::crate_name(db, krate);
                    refused!(
                        "`#[global_allocator]` of crate `{krate}` exports no `{ours}`: rustc's \
                         `{name}` would run its allocator as rustc compiled it"
                    );
                };
                self.wanted.push(func);
                self.define_forwarding(&rustcs, Linkage::Hidden, Some(ours), params, returns)?;
            }
        }
        Ok(())
    }

    /// Defines `name`, of `params` pointer-sized arguments and a pointer returned if `returns`,
    /// as a call of `target` with them, or as nothing.
    fn define_forwarding(
        &mut self,
        name: &str,
        linkage: Linkage,
        target: Option<&str>,
        params: usize,
        returns: bool,
    ) -> Result<()> {
        let mut sig = self.module.make_signature();
        sig.params.extend((0..params).map(|_| AbiParam::new(ptr())));
        sig.returns.extend(returns.then(|| AbiParam::new(ptr())));
        let id = self
            .module
            .declare_function(name, linkage, &sig)
            .map_err(|e| CodegenError::Backend(e.to_string()))?;
        let target = target.map(|it| self.import(it, &sig)).transpose()?;
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        let mut builder_ctx = std::mem::take(&mut self.builder_ctx);
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let block = builder.create_block();
        builder.append_block_params_for_function_params(block);
        builder.switch_to_block(block);
        let args = builder.block_params(block).to_vec();
        let results = match target {
            Some(target) => {
                let callee = self.module.declare_func_in_func(target, builder.func);
                let call = builder.ins().call(callee, &args);
                builder.inst_results(call).to_vec()
            }
            None => Vec::new(),
        };
        builder.ins().return_(&results);
        builder.seal_all_blocks();
        builder.finalize(self.module.isa().frontend_config());
        self.builder_ctx = builder_ctx;
        self.define_shim(id, &mut ctx)
    }

    /// A function of ours that could not be compiled and that something in the object calls,
    /// in a build with nothing of rustc's to call instead: the call links, and says why when
    /// it runs. Hidden, so that a program built over this object compiles its own and is told.
    fn define_refused(&mut self, symbol: &str, reason: &str) -> Result<()> {
        let backend = |e: cranelift_module::ModuleError| CodegenError::Backend(e.to_string());
        let declarations = self.module.declarations();
        let Some(cranelift_module::FuncOrDataId::Func(id)) = declarations.get_name(symbol) else {
            return Ok(());
        };
        let decl = declarations.get_function_decl(id);
        let sig = decl.signature.clone();
        let id = match decl.linkage {
            Linkage::Import => {
                self.module.declare_function(symbol, Linkage::Hidden, &sig).map_err(backend)?
            }
            Linkage::Export if self.unstitched.remove(&id) => {
                self.hidden.push(id);
                id
            }
            _ => return Ok(()),
        };
        // One line, for the reader of the section to tell one message from the next.
        let reason = reason.replace('\n', " ");
        let message = format!("{STAND_IN_SAYS}`{symbol}`: {reason}\n").into_bytes();
        let len = message.len() as i64;
        // Defined empty here and given its bytes by `finish`, in the section of stand-ins.
        let text = self.declare_local_data(false)?;
        let mut data = DataDescription::new();
        data.define(Box::new([]));
        self.module.define_data(text, &data).map_err(backend)?;
        self.stand_ins.push((text, message.into_boxed_slice()));
        // With no C library the stand-in only traps: the link still has its message.
        let said = if Target::current().has_libc() {
            let mut write = self.module.make_signature();
            write.params.extend([types::I32, ptr(), ptr()].map(AbiParam::new));
            write.returns.push(AbiParam::new(ptr()));
            Some((
                self.import("write", &write)?,
                self.import("abort", &self.module.make_signature())?,
            ))
        } else {
            None
        };
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        let mut builder_ctx = std::mem::take(&mut self.builder_ctx);
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let block = builder.create_block();
        builder.append_block_params_for_function_params(block);
        builder.switch_to_block(block);
        let text = self.module.declare_data_in_func(text, builder.func);
        let text = builder.ins().symbol_value(ptr(), text);
        if let Some((write, abort)) = said {
            let stderr = builder.ins().iconst(types::I32, 2);
            let len = builder.ins().iconst(ptr(), len);
            let write = self.module.declare_func_in_func(write, builder.func);
            builder.ins().call(write, &[stderr, text, len]);
            let abort = self.module.declare_func_in_func(abort, builder.func);
            builder.ins().call(abort, &[]);
        }
        builder.ins().trap(TrapCode::user(1).expect("non-zero trap code"));
        builder.seal_all_blocks();
        builder.finalize(self.module.isa().frontend_config());
        self.builder_ctx = builder_ctx;
        self.define_shim(id, &mut ctx)
    }

    /// Puts the instances requested by calls in the object. Returns the ones that failed.
    fn define_instances(&mut self) -> Vec<(Instance, Rejected)> {
        let mut failed = Vec::new();
        self.compile_ahead(Vec::new(), &mut |cg, ahead| failed.extend(cg.define_waves(ahead)));
        failed
    }

    /// Puts what is pending in the object, and what that calls. With `ahead`, the other
    /// threads are compiling all of it, and an instance is put in the object once they have
    /// it. Without, each wave is compiled on every core before it is put in the object.
    fn define_waves(&mut self, mut ahead: Option<&mut Ready<'_>>) -> Vec<(Instance, Rejected)> {
        let mut failed = Vec::new();
        loop {
            let wave = match &mut ahead {
                Some(ahead) => ahead.take(&mut self.pending),
                None if self.pending.is_empty() => Vec::new(),
                #[cfg(feature = "llvm")]
                None if self.units() => {
                    let wave = std::mem::take(&mut self.pending);
                    let fresh = wave.iter().filter(|it| !self.unit_instances.contains_key(&it.0));
                    let fresh = fresh.map(|(instance, env, _)| (instance.clone(), env.clone()));
                    let fresh = fresh.collect();
                    self.compile_units(&[], fresh);
                    wave
                }
                None => {
                    let wave = std::mem::take(&mut self.pending);
                    let start = stdx::time::Instant::now();
                    let told = self.tells_variables().then_some(&self.told);
                    self.symbols.in_parallel(wave.len(), &|db, _, at| {
                        let (instance, env, _) = &wave[at];
                        let compiled = compiled_instance(db, instance.clone(), env.clone());
                        if let (Ok(compiled), Some(told)) = (compiled.as_ref(), told) {
                            told.ahead(db, compiled);
                            instance_rows(db, instance.clone(), env.clone());
                        }
                    });
                    self.phases.compile += start.elapsed();
                    wave
                }
            };
            if wave.is_empty() && self.found_pending.is_empty() {
                break;
            }
            for (instance, env, id) in wave {
                let defined = self.define_instance(&instance, &env, id);
                if let Err(e) = defined {
                    if let CodegenError::Refused(_) = e {
                        self.refused.get_or_insert(e);
                    } else if self.forward_to_rustc(&instance, id).is_some() {
                        self.forwarded.insert(instance);
                    } else {
                        failed.push((instance, Rejected { env, why: e.to_string() }));
                    }
                }
            }
            let start = stdx::time::Instant::now();
            while let Some((name, id)) = self.found_pending.pop() {
                if let Err(e) = self.stitch_entry(&name, id) {
                    eprintln!("entry of `{name}` does not fit: {e}");
                    cached::Entries::ban(&name);
                    self.misfit = true;
                }
            }
            self.phases.define += start.elapsed();
        }
        failed
    }

    /// Compiles what is pending, what the compiled `roots` call and everything those call, on
    /// every core. A thread goes on to what an instance calls as soon as that instance is
    /// compiled, where the waves of [`Self::define_instances`] each wait for their slowest
    /// function. What this compiles that the object has no use for is only wasted. `beside`
    /// runs on this thread meanwhile, and is told whether anything is compiled ahead of it.
    fn compile_ahead(
        &mut self,
        roots: Vec<FunctionId>,
        beside: &mut dyn FnMut(&mut Self, Option<&mut Ready<'_>>),
    ) {
        // A patch calls the base's copy of most of what it reaches. Units are compiled
        // before they are put in the object.
        if self.base.is_some() || self.units() || (self.pending.is_empty() && roots.is_empty()) {
            return beside(self, None);
        }
        type Waits = (std::sync::Condvar, std::sync::Condvar);
        /// Tells the threads that wait when the one compiling is stopped.
        struct Working<'a>(&'a std::sync::Mutex<Ahead>, &'a Waits);
        impl Drop for Working<'_> {
            fn drop(&mut self) {
                if std::thread::panicking() {
                    self.0.lock().unwrap_or_else(|e| e.into_inner()).stopped = true;
                    self.1.0.notify_all();
                    self.1.1.notify_all();
                }
            }
        }
        let mut seen = std::mem::take(&mut self.ahead);
        let queue: std::collections::VecDeque<_> = self
            .pending
            .iter()
            .map(|(instance, env, _)| (instance.clone(), env.clone()))
            .filter(|it| seen.insert(it.clone()))
            .collect();
        let ahead = std::sync::Mutex::new(Ahead {
            rooted: roots.iter().copied().collect(),
            rooted_done: FxHashSet::default(),
            roots: roots.into(),
            queue,
            seen,
            done: FxHashSet::default(),
            working: 0,
            stopped: false,
            open: true,
            alive: 0,
        });
        // Work for the threads, and what they are through with for the one beside them.
        let waits: Waits = Default::default();
        let (work_to_do, progress) = (&waits.0, &waits.1);
        let (rejected, hydrate) = (self.rejected, self.hydrate);
        let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
        let (entries, owned) = (self.entries.clone(), &self.own_statics.crates.clone());
        let entries = entries.as_deref();
        let told = self.tells_variables().then(|| self.told.clone());
        let told = told.as_ref();
        let on_demand = self.on_demand;
        let (stored, roots_from_store) = (&self.stored_roots.clone(), self.roots_from_store());
        // What the store is given of a function names each static it refers to by a name its
        // crate is listed to find again: listed here, beside the compiling, and not by every
        // thread at once after it.
        let (listings, keeps) = (self.names.listings(), self.defers() && entries.is_some());
        let run = |db: &dyn HirDatabase, symbols: &dyn SymbolResolver| {
            let names = stable::StableNames::new(db, symbols, owned.clone(), hydrate)
                .listing(listings.clone());
            let usable = |name: &str| {
                let Some(entries) = entries else { return false };
                entries.usable(name, &|symbol| symbols.provides(symbol) && symbols.linked(symbol))
            };
            // What the store has is not compiled, nor looked into for what it calls.
            let in_store = |callee: &Instance| names.instance(callee).is_some_and(|it| usable(&it));
            // A root only called is asked about here, as it is reached.
            let stored_root = |root: FunctionId| match stored.get(&root) {
                Some(name) => name.is_some(),
                None => {
                    roots_from_store
                        && names.instance(&root_instance(db, root)).is_some_and(|name| {
                            stable::StableNames::of_checkout(&name) && usable(&name)
                        })
                }
            };
            let mut homes = home::Homes::default();
            let mut state = ahead.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stopped {
                    return;
                }
                let job = match state.roots.pop_front() {
                    Some(root) => either::Either::Left(root),
                    None => match state.queue.pop_front() {
                        Some(it) => either::Either::Right(it),
                        None if state.working == 0 && !state.open => {
                            work_to_do.notify_all();
                            progress.notify_all();
                            return;
                        }
                        None => {
                            state = work_to_do.wait(state).unwrap_or_else(|e| e.into_inner());
                            continue;
                        }
                    },
                };
                state.working += 1;
                drop(state);
                let working = Working(&ahead, &waits);
                let (mut found, mut called) = (Vec::new(), Vec::new());
                let compiled = match &job {
                    // One without a symbol is left to rustc, and so is what only it calls.
                    either::Either::Left(root) if stored_root(*root) => None,
                    either::Either::Left(root) => symbols.function(*root).and_then(|_| {
                        if told.is_some() {
                            root_rows(db, *root);
                        }
                        compiled_root(db, *root).as_ref().ok()
                    }),
                    either::Either::Right((instance, env)) => {
                        let compiled = compiled_instance(db, instance.clone(), env.clone());
                        let compiled = compiled.as_ref().ok();
                        if let (Some(compiled), Some(told)) = (compiled, told) {
                            told.ahead(db, compiled);
                            instance_rows(db, instance.clone(), env.clone());
                        }
                        compiled
                    }
                };
                // The thread that puts the function in the object asks for the value of each
                // static it names and the place of each panic: it would find them one by one.
                let mut statics = Vec::new();
                for request in compiled.iter().flat_map(|it| &it.requests.datas) {
                    match request {
                        DataRequest::Static(id) => statics.push(*id),
                        &DataRequest::Location(Some((owner, span))) => {
                            _ = source_location(db, owner, span);
                        }
                        _ => {}
                    }
                }
                let mut warmed = FxHashSet::default();
                while let Some(id) = statics.pop() {
                    if !warmed.insert(id) {
                        continue;
                    }
                    if keeps {
                        _ = names.static_(id);
                    }
                    if let Ok(value) = compiled_static(db, id) {
                        let named = value.requests.datas.iter().filter_map(|it| match it {
                            DataRequest::Static(id) => Some(*id),
                            _ => None,
                        });
                        statics.extend(named);
                    }
                }
                for request in compiled.iter().flat_map(|it| &it.requests.funcs) {
                    let FuncTarget::Instance(callee, asked) = &request.target else {
                        continue;
                    };
                    // A function without generic arguments is a root, called by its
                    // symbol when rustc compiled it. The thread that puts the caller in
                    // the object asks for the symbol, and finds it here.
                    let root = match callee.def {
                        InstanceDef::Fn(func)
                            if callee.args.as_ref().iter().all(|it| it.region().is_some()) =>
                        {
                            // One compiled only when called is, by the thread that puts
                            // its caller in the object: it is compiled for it here.
                            if on_demand.contains(&func) {
                                called.push(func);
                            }
                            symbols.function(func).is_some()
                        }
                        _ => false,
                    };
                    if !root && !rejected.contains_key(callee) && !in_store(callee) {
                        found.push((callee.clone(), homes.home(db, callee, asked)));
                    }
                }
                drop(working);
                state = ahead.lock().unwrap_or_else(|e| e.into_inner());
                state.working -= 1;
                if let either::Either::Left(root) = job {
                    state.rooted_done.insert(root);
                }
                if let either::Either::Right(it) = job {
                    state.done.insert(it);
                }
                let mut added = 0;
                for it in found {
                    if state.seen.insert(it.clone()) {
                        state.queue.push_back(it);
                        added += 1;
                    }
                }
                for root in called {
                    if state.rooted.insert(root) {
                        state.roots.push_back(root);
                        added += 1;
                    }
                }
                // A thread for each new one, and every thread once there is nothing to do.
                if state.working == 0 && state.queue.is_empty() && state.roots.is_empty() {
                    work_to_do.notify_all();
                } else {
                    (0..added.min(threads)).for_each(|_| work_to_do.notify_one());
                }
                progress.notify_all();
            }
        };
        let work = |db: &dyn HirDatabase, symbols: &dyn SymbolResolver, _: usize| {
            ahead.lock().unwrap_or_else(|e| e.into_inner()).alive += 1;
            /// Counts the thread out however it stops.
            struct Leaving<'a>(&'a std::sync::Mutex<Ahead>, &'a std::sync::Condvar);
            impl Drop for Leaving<'_> {
                fn drop(&mut self) {
                    self.0.lock().unwrap_or_else(|e| e.into_inner()).alive -= 1;
                    self.1.notify_all();
                }
            }
            let _leaving = Leaving(&ahead, progress);
            run(db, symbols);
        };
        let waiting = std::collections::VecDeque::new();
        let mut ready = Ready { ahead: &ahead, work: work_to_do, progress, waiting };
        self.symbols.in_parallel_beside(threads, &work, &mut || {
            beside(self, Some(&mut ready));
            ahead.lock().unwrap_or_else(|e| e.into_inner()).open = false;
            work_to_do.notify_all();
            progress.notify_all();
        });
        self.ahead = ahead.into_inner().unwrap_or_else(|e| e.into_inner()).seen;
    }

    fn define_instance(
        &mut self,
        instance: &Instance,
        env: &StoredParamEnvAndCrate,
        id: FuncId,
    ) -> Result<()> {
        let start = stdx::time::Instant::now();
        let tiers = self.tiers;
        let node = tier::Node(tier::Kind::Instance(instance.clone(), env.clone()));
        let tiered = tiers.and_then(|it| it.code(self.db, &node));
        let unit = self.unit_instances.remove(instance).filter(|_| tiered.is_none());
        let compiled = match (tiered, &unit) {
            (Some(code), _) => code,
            (None, Some(unit)) => unit,
            (None, None) => compiled_instance(self.db, instance.clone(), env.clone())
                .as_ref()
                .map_err(Clone::clone)?,
        };
        let took = start.elapsed();
        self.phases.compile += took;
        let generic = instance.args.as_ref().iter().any(|arg| arg.ty().is_some());
        self.shape_origin(id, instance.def, &(instance, env), generic, took);
        // The call site declared the instance with the signature it computed from its operands.
        let declared = &self.module.declarations().get_function_decl(id).signature;
        let has_body = matches!(
            instance.def,
            InstanceDef::Fn(_)
                | InstanceDef::Closure(_)
                | InstanceDef::Coroutine(_)
                | InstanceDef::CoroutineByMove(_)
        );
        if has_body && *declared != compiled.code.signature {
            unsupported!(
                "call site and body disagree on the signature: {declared} and {}",
                compiled.code.signature
            );
        }
        let ids = self.stitch(id, compiled)?;
        let of = later::Of::Instance { instance: instance.clone(), env: env.clone() };
        let mut later = later::Later::new(&self.module, id, of, compiled, ids);
        later.unit_rows =
            unit.as_ref().filter(|_| self.debug.is_some()).map(|it| rows_of(self.db, it));
        (later.tiered, later.keeps) = (tiered.is_some(), tiered.is_none());
        match (tiered, &unit) {
            (None, None) if self.defers() => {
                let compiled = compiled_instance(self.db, instance.clone(), env.clone());
                let compiled = compiled.as_ref().map_err(Clone::clone)?;
                self.later.push(later::Later { compiled, ..later });
            }
            _ => self.make_now(&later),
        }
        Ok(())
    }

    fn stable_name(&mut self, instance: &Instance) -> Option<StableName> {
        if let Some(known) = self.stable_names.get(instance) {
            return known.clone();
        }
        let name = self.names.instance(instance).map(StableName::from);
        self.stable_names.insert(instance.clone(), name.clone());
        name
    }

    /// Whether what is made of a function once it is in the object waits for every function
    /// to be in it ([`later`]): not in a program with code of rustc's, which settles what it
    /// refers to by the build, nor in a patch, nor for units, nor when entries are checked.
    fn defers(&self) -> bool {
        self.mixed.is_none()
            && self.base.is_none()
            && !self.units()
            && std::env::var_os("RA_JIT_CACHE_CHECK").is_none()
    }

    fn keeper(&mut self, settled: Option<Vec<Option<Instance>>>) -> later::Keeper<'_, 'db> {
        let program = self.mixed.as_ref().map(|it| it.key);
        let markable = self.mixed.is_none() && !self.units();
        later::Keeper {
            db: self.db,
            symbols: self.symbols,
            names: &self.names,
            stable_names: &mut self.stable_names,
            source_paths: &mut self.source_paths,
            kept: &mut self.kept,
            declarations: self.module.declarations(),
            type_ids: &self.type_ids,
            root_names: &self.root_names,
            own_crates: &self.own_statics.crates,
            told: &self.told,
            debug: self.debug.is_some(),
            variables: self.symbols.describes_variables(),
            program,
            own: self.base.is_none() && program != Some(None) && !self.symbols.onto_rustcs(),
            markable,
            settled,
        }
    }

    /// Makes what is made of a function once it is in the object, here and now.
    fn make_now(&mut self, later: &later::Later<'_>) {
        let settled = self.mixed.is_some().then(|| {
            let funcs = later.compiled.requests.funcs.iter();
            let settled = funcs.map(|request| match &request.target {
                FuncTarget::Instance(callee, _) => self.settled(callee).ok(),
                _ => None,
            });
            settled.collect()
        });
        let made = self.keeper(settled).make(later);
        self.apply(later, made);
    }

    fn apply(&mut self, later: &later::Later<'_>, made: later::Made) {
        if let (Some(debug), Some(describer)) = (made.debug, &self.debug) {
            describer.add_function(later.id, later.compiled.code.bytes.len() as u32, debug);
        }
        if let Some(kept) = made.kept
            && let Some(store) = store::Store::global()
        {
            self.writer(store).keep(kept);
        }
    }

    /// Makes what is made of the functions in the object that waited for it ([`later`]),
    /// on every core, and tells the debugger and the store of them in the order they were
    /// put in the object.
    fn make_later(&mut self) {
        let later = std::mem::take(&mut self.later);
        if later.is_empty() {
            return;
        }
        const AT_ONCE: usize = 64;
        let parts: Vec<_> = later.chunks(AT_ONCE).collect();
        type Part = (Vec<later::Made>, Kept, FxHashMap<&'static str, usize>);
        let made: Vec<std::sync::Mutex<Option<Part>>> =
            parts.iter().map(|_| Default::default()).collect();
        {
            let (owned, hydrate) = (&self.own_statics.crates, self.hydrate);
            let (declarations, type_ids, root_names) =
                (self.module.declarations(), &self.type_ids, &self.root_names);
            let program = self.mixed.as_ref().map(|it| it.key);
            let (debug, told) = (self.debug.is_some(), &self.told);
            // Each thread names what it makes with names of its own, which it asks the same
            // of over and over: they are not made again for each part.
            let next = std::sync::atomic::AtomicUsize::new(0);
            let listings = self.names.listings();
            let work = |db: &dyn HirDatabase, symbols: &dyn SymbolResolver, _: usize| {
                let names = stable::StableNames::new(db, symbols, owned.clone(), hydrate)
                    .listing(listings.clone());
                let (mut stable_names, mut source_paths) = Default::default();
                let mut kept = Kept::default();
                let mut keeper = later::Keeper {
                    db,
                    symbols,
                    names: &names,
                    stable_names: &mut stable_names,
                    source_paths: &mut source_paths,
                    kept: &mut kept,
                    declarations,
                    type_ids,
                    root_names,
                    own_crates: owned,
                    told,
                    debug,
                    variables: symbols.describes_variables(),
                    program,
                    own: program != Some(None) && !symbols.onto_rustcs(),
                    markable: true,
                    settled: None,
                };
                loop {
                    let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(part) = parts.get(at) else { break };
                    let part = part.iter().map(|it| keeper.make(it)).collect();
                    let kept = std::mem::take(&mut *keeper.kept);
                    let misses = names.misses.take();
                    *made[at].lock().unwrap_or_else(|e| e.into_inner()) =
                        Some((part, kept, misses));
                }
            };
            let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
            self.symbols.in_parallel(threads.min(parts.len()), &work);
        }
        for (part, made) in parts.iter().zip(made) {
            let made = made.into_inner().unwrap_or_else(|e| e.into_inner());
            // A resolver without threads leaves the work to whoever asks for it.
            let Some((part_made, kept, misses)) = made else {
                for it in part.iter() {
                    let made = self.keeper(None).make(it);
                    self.apply(it, made);
                }
                continue;
            };
            for (it, made) in part.iter().zip(part_made) {
                self.apply(it, made);
            }
            self.kept.merge(kept);
            let mut ours = self.names.misses.borrow_mut();
            for (why, count) in misses {
                *ours.entry(why).or_default() += count;
            }
        }
    }

    fn writer(&mut self, store: &'static store::Store) -> &mut cached::Writer {
        let symbols = self.symbols;
        self.writer.get_or_insert_with(|| {
            cached::Writer::new(store, symbols.cargo_home(), symbols.library_sources())
        })
    }

    /// Whether the instance is compiled with the flags of the crate its function is written
    /// in, as the copy under the symbol of the function is. A crate that uses an `#[inline]`
    /// function of another compiles a copy with its own.
    fn as_its_crate_compiles(&self, instance: &Instance) -> bool {
        as_its_crate_compiles(self.db, self.symbols, instance)
    }

    /// The instance as the crate rustc had compile it does: `instance` is as the crate that
    /// asks for it would. See [`settled`].
    fn settled(&self, instance: &Instance) -> Result<Instance> {
        let Some(mixed) = &self.mixed else { return Ok(instance.clone()) };
        let (db, asking) = (self.db, instance.checked);
        let nameless = || {
            // Only the crates that made a copy of a function of its name, or of a closure
            // written in one, can have.
            let makers = home::written_in(db, instance).map(|func| {
                let name = FunctionSignature::of(db, func).name.as_str();
                let makers = self.symbols.makers(name).into_iter();
                makers.filter_map(|id| mixed.holder(db, id)).collect::<Vec<_>>()
            });
            let mentions = home::mentioned_crates(db, instance);
            let reachable = mixed.reachable(asking, mentions.as_ref(), makers.as_deref());
            let Some(checked) = reachable else {
                unsupported!(
                    "an instance with no name to find rustc's copies by, which a crate with \
                     other overflow checks may have compiled"
                );
            };
            Ok(checked)
        };
        let checked = match instance.def {
            InstanceDef::Fn(func) => self.settled_function(mixed, func, instance, &nameless)?,
            InstanceDef::Closure(_)
            | InstanceDef::Coroutine(_)
            | InstanceDef::CoroutineByMove(_) => nameless()?,
            // No arithmetic of their own, and what they call is settled when they call it.
            InstanceDef::DropGlue
            | InstanceDef::CallOnceShim
            | InstanceDef::VtableShim(_)
            | InstanceDef::FnPointerShim
            | InstanceDef::ReifyShim { .. } => asking,
        };
        if checked != asking && self.base.is_some() {
            unsupported!("a copy rustc compiled with other overflow checks, in a patch");
        }
        Ok(Instance { checked, ..instance.clone() })
    }

    fn settled_function(
        &self,
        mixed: &settled::Mixed,
        func: FunctionId,
        instance: &Instance,
        nameless: &dyn Fn() -> Result<bool>,
    ) -> Result<bool> {
        let (db, asking) = (self.db, instance.checked);
        let args = instance.args.as_ref();
        let checked = if !args.iter().all(|arg| arg.region().is_some()) {
            let Some(path) = self.instance_path(func, args) else { return nameless() };
            let copies = self.symbols.path(&path).map(|it| self.symbols.copies(&it));
            let holders: Vec<_> =
                copies.iter().flatten().filter_map(|(_, id)| mixed.holder(db, *id)).collect();
            let Some(checked) = mixed.reached(asking, &holders) else {
                unsupported!(
                    "`{path}`, which rustc compiled with and without overflow checks: which \
                     copy a crate without one of its own calls is rustc's to pick"
                );
            };
            checked
        } else if AttrFlags::query(db, func.into()).contains(AttrFlags::IS_INLINE) {
            asking
        } else {
            // One copy under the function's symbol when rustc made one. Without, rustc found
            // the function small enough to compile in each crate that calls it.
            let own = || self.symbols.function(func).is_some_and(|it| self.symbols.defines(&it));
            match crate_checks_overflow(db, func.krate(db)) {
                Some(written_with) => written_with && (asking || own()),
                None => asking,
            }
        };
        Ok(checked)
    }

    /// rustc's copy of the instance `symbol` is one of that is compiled as `checked` says, and
    /// that the executable has: the one its crate's symbol names may be one nothing called.
    fn rustc_copy(&self, symbol: String, checked: bool) -> Option<String> {
        let linked = |it: &str| self.symbols.linked(it);
        let Some(mixed) = &self.mixed else {
            if linked(&symbol) {
                return Some(symbol);
            }
            return self.symbols.copies(&symbol).into_iter().map(|it| it.0).find(|it| linked(it));
        };
        let copies = self.symbols.copies(&symbol);
        if copies.is_empty() {
            return Some(symbol).filter(|it| linked(it));
        }
        let of = |id: u64| mixed.holder(self.db, id).is_some_and(|it| it.1 == checked);
        copies.into_iter().find(|it| of(it.1) && linked(&it.0)).map(|it| it.0)
    }

    /// The function of the program that an instance without arguments is of, and its symbol.
    fn root_named(&self, name: &str) -> Option<(FunctionId, String)> {
        if let Some(&Some(func)) = self.root_names.get(name)
            && let Some(symbol) = self.symbols.function(func)
        {
            return Some((func, symbol));
        }
        let of = self.names.functions_named(name);
        of.into_iter().find_map(|it| Some((it, self.symbols.function(it)?)))
    }

    /// The function of an entry and everything it calls, declared here and to be defined from
    /// the store. `None` when the store does not have all of it, or `body` is given and is
    /// not the signature the entry was compiled with.
    fn found_callee(&mut self, name: &str, body: Option<&Signature>) -> Result<Option<FuncId>> {
        if let Some(&id) = self.named.get(name) {
            return Ok(Some(id));
        }
        let Some(entries) = self.entries.clone() else { return Ok(None) };
        let symbols = self.symbols;
        if !entries.usable(name, &|symbol| symbols.provides(symbol) && symbols.linked(symbol)) {
            let why = match entries.get(name) {
                None => "no entry",
                Some(it)
                    if !it.links(&|symbol| symbols.provides(symbol) && symbols.linked(symbol)) =>
                {
                    "an entry linked against what the program lacks"
                }
                Some(_) => "an entry that calls what cannot be used",
            };
            return Ok(self.kept.unfound(why));
        }
        let Some(entry) = entries.get(name) else { return Ok(None) };
        if body.is_some_and(|it| it != entry.code.signature()) {
            return Ok(self.kept.unfound("an entry of another signature"));
        }
        let symbol = format!("ra_jit::{}::n{}", entry.label, self.found.len());
        let linkage = if self.patching { Linkage::Hidden } else { Linkage::Local };
        let id = self
            .module
            .declare_function(&symbol, linkage, entry.code.signature())
            .map_err(|e| CodegenError::Backend(e.to_string()))?;
        let name = StableName::from(name);
        self.named.insert(name.clone(), id);
        self.found.push((name.clone(), id));
        self.found_pending.push((name, id));
        Ok(Some(id))
    }

    /// Defines `id` as the function of an entry, and declares what the entry refers to.
    fn stitch_entry(&mut self, name: &str, id: FuncId) -> Result<()> {
        use cached::{CachedData, CachedTarget};
        let Some(entry) = self.entries.as_ref().and_then(|it| it.get(name)) else {
            unsupported!("an entry that was in the store is not");
        };
        let mut ids = Resolved {
            funcs: Vec::with_capacity(entry.funcs.len()),
            datas: Vec::with_capacity(entry.datas.len()),
        };
        for func in &entry.funcs {
            ids.funcs.push(match &func.target {
                CachedTarget::Import { symbol, .. } => self.import(symbol, &func.sig)?,
                CachedTarget::Asm(text) => self.assembly(text, &func.sig)?,
                CachedTarget::TypeId(hash) => self.type_id_shim(&func.sig, *hash)?,
                CachedTarget::Root(callee) => {
                    let Some((of, symbol)) = self.root_named(callee) else {
                        unsupported!("an entry calls `{callee}`, which no crate declares");
                    };
                    self.want(of);
                    self.import(&symbol, &func.sig)?
                }
                CachedTarget::Copy { name: callee, symbol } => {
                    let Some((of, _)) = self.root_named(callee) else {
                        unsupported!("an entry calls `{callee}`, which no crate declares");
                    };
                    self.want(of);
                    self.import(symbol, &func.sig)?
                }
                CachedTarget::Named(callee) => {
                    let Some(id) = self.found_callee(callee, None)? else {
                        unsupported!("an entry calls `{callee}`, which the store lacks");
                    };
                    id
                }
            });
        }
        for data in &entry.datas {
            let id = match data {
                CachedData::Blob { blob, identity } => {
                    self.define_data_as(ids.blob(blob), *identity)?
                }
                CachedData::Vtable { key, blob } => match self.named_vtables.get(key) {
                    Some(&id) => id,
                    None => {
                        let id = self.define_data_as(ids.blob(blob), DataIdentity::Own)?;
                        self.named_vtables.insert(key.clone(), id);
                        id
                    }
                },
                CachedData::Static { symbol, thread_local } => {
                    self.declare_named_data(symbol, Linkage::Import, false, *thread_local, false)?
                }
                CachedData::OwnStatic(name) => {
                    let Some(of) = self.names.static_named(name) else {
                        unsupported!("an entry refers to `{name}`, which no crate declares");
                    };
                    self.static_data(of)?
                }
                CachedData::Location(at) => self.define_location(at.clone())?,
                CachedData::FilePath(path) => self.define_file_path(path.clone())?,
            };
            ids.datas.push(id);
        }
        let code = entry.code.code();
        let relocs: Vec<_> = code.relocs.iter().map(|it| ids.reloc(it, id)).collect();
        self.define_code(id, &code, &relocs)?;
        if let (Some(debug), Some(of), Some(entries)) = (&self.debug, &entry.debug, &self.entries) {
            let len = code.bytes.len() as u32;
            let entries = entries.clone();
            backend::Tally::global()
                .step(Step::Describe, || debug.add_stored(id, len, of.clone(), entries));
        }
        self.bodies += 1;
        self.kept.found += 1;
        Ok(())
    }

    /// Defines `id` as a compiled function, and declares everything the function refers to.
    fn stitch(&mut self, id: FuncId, compiled: &CompiledFunction) -> Result<Resolved> {
        let start = stdx::time::Instant::now();
        let ids = self.resolve(&compiled.requests);
        self.phases.resolve += start.elapsed();
        let ids = ids?;
        let start = stdx::time::Instant::now();
        let relocs: Vec<_> = compiled.code.relocs.iter().map(|it| ids.reloc(it, id)).collect();
        self.shape_code(id, &compiled.code, &relocs);
        self.define_code(id, &compiled.code, &relocs)?;
        for (ty, size, align) in &compiled.layouts {
            self.layouts.entry(ty.clone()).or_insert((*size, *align));
        }
        self.bodies += 1;
        self.phases.define += start.elapsed();
        Ok(ids)
    }

    fn shape_origin(
        &mut self,
        id: FuncId,
        def: InstanceDef,
        // The symbol of an instance changes from one round to the next.
        identity: &impl std::hash::Hash,
        generic: bool,
        took: std::time::Duration,
    ) {
        use std::hash::Hasher;
        use std::io::Write;
        if self.shapes.is_none() {
            return;
        }
        let db = self.db;
        let (name, derive) = match def {
            InstanceDef::Fn(func) => {
                let derive = func.loc(db).id.file_id.macro_file().is_some_and(|call| {
                    matches!(call.loc(db).def.kind, MacroDefKind::BuiltInDerive(..))
                });
                (FunctionSignature::of(db, func).name.as_str().to_owned(), derive)
            }
            _ => (String::new(), false),
        };
        let decls = self.module.declarations();
        let symbol = decls.get_function_decl(id).linkage_name(id).into_owned();
        let kind = format!("{def:?}");
        let kind = kind.split(['(', ' ']).next().unwrap_or_default().to_owned();
        let origin = match def {
            InstanceDef::Fn(func) => format!("{func:?}"),
            InstanceDef::Closure(it)
            | InstanceDef::Coroutine(it)
            | InstanceDef::CoroutineByMove(it) => format!("{it:?}"),
            _ => kind.clone(),
        };
        let mut hasher = rustc_hash::FxHasher::default();
        identity.hash(&mut hasher);
        if let Some(out) = &mut self.shapes {
            let _ = writeln!(
                out,
                "O\t{symbol}\t{kind}\t{origin}\t{name}\t{}\t{}\t{}\t{:x}",
                generic as u8,
                derive as u8,
                took.as_micros(),
                hasher.finish()
            );
        }
    }

    fn shape_code(&mut self, id: FuncId, code: &Code, relocs: &[ModuleReloc]) {
        use std::hash::{Hash, Hasher};
        use std::io::Write;
        if self.shapes.is_none() {
            return;
        }
        let decls = self.module.declarations();
        let mut hasher = rustc_hash::FxHasher::default();
        code.bytes.hash(&mut hasher);
        let mut targets = Vec::new();
        for reloc in relocs {
            (reloc.offset, format!("{:?}", reloc.kind), reloc.addend).hash(&mut hasher);
            targets.push(match reloc.name {
                ModuleRelocTarget::User { namespace: 0, index } => {
                    let id = FuncId::from_u32(index);
                    decls.get_function_decl(id).linkage_name(id).into_owned()
                }
                ModuleRelocTarget::User { index, .. } => {
                    let id = DataId::from_u32(index);
                    format!("data:{}", decls.get_data_decl(id).linkage_name(id))
                }
                ModuleRelocTarget::FunctionOffset(..) => "self".to_owned(),
                ref other => format!("{other:?}"),
            });
        }
        let symbol = decls.get_function_decl(id).linkage_name(id).into_owned();
        if let Some(out) = &mut self.shapes {
            let _ = writeln!(
                out,
                "C\t{symbol}\t{}\t{:x}\t{}",
                code.bytes.len(),
                hasher.finish(),
                targets.join(" ")
            );
        }
    }

    fn define_code(&mut self, id: FuncId, code: &Code, relocs: &[ModuleReloc]) -> Result<()> {
        if code.elf_types.is_empty() {
            self.module.define_function_bytes(id, code.align, &code.bytes, relocs)?;
        } else {
            // Cranelift is given the relocations it has a kind for, and `finish` adds the
            // others to the object.
            let elf_type = |reloc: &ModuleReloc| {
                code.elf_types.iter().find(|(offset, _)| *offset == reloc.offset).map(|it| it.1)
            };
            let (by_type, plain): (Vec<_>, Vec<_>) =
                relocs.iter().cloned().partition(|reloc| elf_type(reloc).is_some());
            self.module.define_function_bytes(id, code.align, &code.bytes, &plain)?;
            self.elf_relocs.extend(by_type.into_iter().filter_map(|reloc| {
                let r_type = elf_type(&reloc)?;
                Some(ElfReloc { func: id, reloc, r_type })
            }));
        }
        if let Some(unwind) = &code.unwind {
            self.unwind
                .add_function(&mut self.module, id, unwind)
                .map_err(CodegenError::Backend)?;
        }
        Ok(())
    }

    /// Compiles a function built here, whose IR names what it uses by the ids of the module.
    fn define_shim(&mut self, id: FuncId, ctx: &mut cranelift_codegen::Context) -> Result<()> {
        let mut code = emit(ctx)?;
        for reloc in &mut code.relocs {
            if let ModuleRelocTarget::FunctionOffset(_, offset) = reloc.name {
                reloc.name = ModuleRelocTarget::FunctionOffset(id, offset);
            }
        }
        let relocs = std::mem::take(&mut code.relocs);
        self.define_code(id, &code, &relocs)
    }

    fn resolve(&mut self, requests: &Requests) -> Result<Resolved> {
        let mut ids = Resolved {
            funcs: Vec::with_capacity(requests.funcs.len()),
            datas: Vec::with_capacity(requests.datas.len()),
        };
        for request in &requests.funcs {
            ids.funcs.push(self.func(request)?);
        }
        // A data object only points to the ones requested before it.
        for request in &requests.datas {
            let id = self.data(request, &ids)?;
            ids.datas.push(id);
        }
        Ok(ids)
    }

    fn func(&mut self, request: &FuncRequest) -> Result<FuncId> {
        match &request.target {
            FuncTarget::Symbol(symbol) => {
                let own = self.unwind.runtime(&mut self.module, symbol);
                match own.map_err(CodegenError::Backend)? {
                    Some(id) => Ok(id),
                    None => self.import(symbol, &request.sig),
                }
            }
            FuncTarget::Path(path) => {
                let Some(symbol) = self.symbols.path(path) else {
                    unsupported!("call to `{path}`, which rustc has no symbol for");
                };
                self.import(&symbol, &request.sig)
            }
            FuncTarget::Instance(instance, env) => self.callee(instance, env, &request.sig),
            FuncTarget::Asm(text) => self.assembly(text, &request.sig),
        }
    }

    /// The function of an `asm!` block, which is local to the object.
    fn assembly(&mut self, text: &str, sig: &Signature) -> Result<FuncId> {
        if let Some(&id) = self.assemblies.get(text) {
            return Ok(id);
        }
        let triple = self.module.isa().triple().to_string();
        let assembled = asm::assemble(&triple, text).map_err(CodegenError::Unsupported)?;
        let name = format!("ra_jit::asm::{}", self.assemblies.len());
        let id = self
            .module
            .declare_function(&name, Linkage::Local, sig)
            .map_err(|e| CodegenError::Backend(e.to_string()))?;
        self.module.define_function_bytes(id, 16, &assembled.code, &[])?;
        self.assemblies.insert(text.to_owned(), id);
        Ok(id)
    }

    fn data(&mut self, request: &DataRequest, ids: &Resolved) -> Result<DataId> {
        match request {
            DataRequest::Blob { blob, identity } => self.define_data_as(ids.blob(blob), *identity),
            DataRequest::Vtable { ty, trait_ref, blob } => {
                let key = (ty.clone(), trait_ref.clone());
                if let Some(&id) = self.vtables.get(&key) {
                    return Ok(id);
                }
                let id = self.define_data_as(ids.blob(blob), DataIdentity::Own)?;
                self.vtables.insert(key, id);
                Ok(id)
            }
            DataRequest::Static(id) => self.static_data(*id),
            DataRequest::Location(at) => self.location(*at),
            DataRequest::Declaration(func) => {
                let found = self.declaration_at(*func);
                self.define_location(found)
            }
            DataRequest::FilePath(file) => {
                let Some(path) = self.source_path(*file) else {
                    unsupported!("`file!()` in a file without a path");
                };
                self.define_file_path(path)
            }
            DataRequest::Text(parts) => {
                let Some(text) = self.text(parts) else {
                    unsupported!("`file!()` in a file without a path");
                };
                self.define_file_path(text)
            }
        }
    }

    /// The text of the parts, with each file's path for the file.
    fn text(&mut self, parts: &[TextPart]) -> Option<String> {
        parts
            .iter()
            .map(|part| match part {
                TextPart::Text(text) => Some(text.clone()),
                TextPart::FilePath(file) => self.source_path(*file),
            })
            .collect()
    }

    /// The `&str` that `file!()` is, or a `concat!` with it.
    fn define_file_path(&mut self, path: String) -> Result<DataId> {
        let len = path.len();
        let path = self.define_const(ConstBlob {
            bytes: path.into_bytes(),
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align: 1,
            type_ids: Vec::new(),
        })?;
        let mut fat = ConstBlob {
            bytes: vec![0; 2 * word()],
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align: word() as u64,
            type_ids: Vec::new(),
        };
        fat.point_to(0, path, 0);
        fat.bytes[word()..].copy_from_slice(&(len as u64).to_le_bytes()[..word()]);
        self.define_const(fat)
    }

    fn source_path(&mut self, file: base_db::FileId) -> Option<String> {
        let symbols = self.symbols;
        let spelled = || symbols.source_path(file).map(|it| as_rustc_spells(&it));
        self.source_paths.entry(file).or_insert_with(spelled).clone()
    }

    /// The data object of a static: rustc's when it exports one, else our own copy of its value.
    /// The name the linker knows a static by: one of an `extern` block, or one that an
    /// attribute exports and rustc's build defines.
    fn linked_static(&self, id: StaticId) -> Result<Option<intern::Symbol>> {
        let db = self.db;
        let signature = StaticSignature::of(db, id);
        if !signature.flags.contains(StaticFlags::EXTERN) {
            let exported = exported_static(db, id);
            return Ok(exported.filter(|name| self.symbols.defines(name.as_str())));
        }
        Ok(static_link_name(db, id).map(|it| intern::Symbol::intern(&it)))
    }

    /// The name of a static a patch defines that the executable has no copy of: the same for
    /// the same static with the same type and value in every patch after, so that a program
    /// that took an earlier patch goes on with the one it has (its address and what was
    /// written to it), and another for one whose declaration or value changed, which starts
    /// from the new value. `None` for one with no path to name it by.
    fn lasting_static_name(&self, id: StaticId, bytes: &[u8], writable: bool) -> Option<String> {
        let path = self.names.static_across_edits(id)?;
        let declared = id.lookup(self.db).source(self.db).value.syntax().text().to_string();
        let print = print_of(&(path, declared, bytes, writable));
        Some(format!("{LASTING_STATIC}{print:032x}"))
    }

    fn static_data(&mut self, id: StaticId) -> Result<DataId> {
        let db = self.db;
        let known = [&self.imported_statics, &self.statics, &self.frozen_statics];
        if let Some(&data) = known.into_iter().find_map(|it| it.get(&id)) {
            return Ok(data);
        }
        // rustc's copy when ours has no value to give. A static whose value changes as the
        // program runs (a thread-local, or one written through a cell) stays rustc's wherever
        // rustc linked one: rustc's code that no walk sees writes it too, as its allocator shim
        // does with what the `#[global_allocator]` inlines, and a copy of ours would split the
        // state in two (`docs/live.md#statics`). That holds while rustc's copy starts from the
        // first value the source gives it now: after an edit to that value, ours is the one
        // with the program's value. A patch keeps what its base chose, and declines an edit
        // to the first value of one the base shares (`Base::stale`).
        let stateful = || {
            is_thread_local(db, id) || compiled_static(db, id).as_ref().is_ok_and(|it| it.writable)
        };
        let mut symbol = self.symbols.static_(id);
        // rustc's copy under a name LLVM gave it, or one private to its object, is shared
        // only when it is the whole static and ours can link to it. When it can't be, a static
        // with state is not ours to copy either: rustc's code that has it inlined (a function
        // left to rustc, rustc's copy of an instance we could not compile, its allocator shim)
        // reads and writes rustc's, and a copy of ours would split the program's state in two
        // with nothing to tell (`docs/live.md#statics`). What refers to it is left to rustc.
        if let Some(found) = symbol.as_deref()
            && let Some(copy) = self.symbols.static_copy(found)
        {
            let size = || compiled_static(db, id).as_ref().ok().map(|it| it.blob.bytes.len());
            let why = match copy {
                RustcStaticCopy::Sized(whole) if size() == usize::try_from(whole).ok() => None,
                RustcStaticCopy::Sized(part) => {
                    Some(format!("is a part LLVM split off it, of {part} bytes"))
                }
                RustcStaticCopy::Unshared(why) => Some(why),
            };
            if let Some(why) = why {
                if stateful() && self.symbols.linked(found) {
                    let name = StaticSignature::of(db, id).name.as_str();
                    let krate = fncache::crate_name(db, id.krate(db));
                    refused!(
                        "static `{name}` of crate `{krate}` with state, whose copy in rustc's code (`{found}`) \
                         {why}: a copy of ours would split the state"
                    );
                }
                symbol = None;
            }
        }
        let in_base = self.base.and_then(|base| base.statics.get(&id));
        let stateful_linked = !self.symbols.nothing_of_rustc()
            && symbol.as_ref().is_some_and(|it| self.symbols.linked(it))
            && stateful();
        let shared = stateful_linked
            && in_base.map_or_else(|| self.rustc_values.kept(db, id), |it| it.shared);
        // The program has the base's copy, and what it wrote there.
        let base_owns = stateful_linked && in_base.is_some_and(|it| !it.shared);
        let ours = !shared
            && (base_owns || self.own_statics.replaces(db, id))
            && compiled_static(db, id).is_ok();
        // rustc's copy started from the old first value, and ours can't stand in for it.
        if stateful_linked && !shared && !ours && compiled_static(db, id).is_ok() {
            let name = StaticSignature::of(db, id).name.as_str();
            let krate = fncache::crate_name(db, id.krate(db));
            refused!(
                "static `{name}` of crate `{krate}` with state, whose first value is not the one \
                 rustc built it with (or an edit reached it before that was known): rustc's copy \
                 has the old value, and rustc's code, or its `#[no_mangle]` or `#[export_name]` \
                 symbol, keeps a copy of ours from standing in for it"
            );
        }
        let rustcs = symbol.filter(|_| !ours);
        // One the linker left out of what rustc linked is used by none of rustc's code there.
        let unlinked = rustcs.as_ref().is_some_and(|it| !self.symbols.linked(it));
        let theirs = || rustcs.filter(|_| !unlinked);
        let weak = AttrFlags::linkage(db, id.into()).is_some_and(|it| it.as_str() == "extern_weak");
        if weak && let Some(symbol) = static_link_name(db, id) {
            let data = self.weak_static(&symbol)?;
            self.imported_statics.insert(id, data);
            return Ok(data);
        }
        if let Some(symbol) = static_link_name(db, id).or_else(theirs) {
            let data = self.declare_named_data(
                &symbol,
                Linkage::Import,
                false,
                is_thread_local(db, id),
                false,
            )?;
            self.imported_statics.insert(id, data);
            if stateful_linked {
                self.shared_statics.insert(id, data);
            }
            // rustc's value of the static points to rustc's copies of functions.
            if self.on_demand_crates.contains(&id.krate(db))
                && let Ok(compiled) = compiled_static(db, id).as_ref()
            {
                let unseen = self.want_requested(&compiled.requests);
                self.want_all_called(&unseen);
            }
            return Ok(data);
        }
        if let Some(symbol) = self.linked_static(id)? {
            let thread_local = is_thread_local(db, id);
            let data = self.declare_named_data(
                symbol.as_str(),
                Linkage::Import,
                false,
                thread_local,
                false,
            )?;
            self.imported_statics.insert(id, data);
            return Ok(data);
        }
        let node = patch::Node::Static(id);
        let mut known = self.base.and_then(|base| base.statics.get(&id));
        // One whose value no longer fits where the base has it is a static of the patch's own,
        // named as one new since the base is.
        if let Some(moved) = known.filter(|_| self.replaced.moved.contains(&id)) {
            self.leave_behind(&moved.symbol)?;
            known = None;
        }
        // One with another value keeps its name, and what refers to it by that gets the new.
        let again = known.filter(|_| self.replaced.changed.contains(&node));
        if let Some(known) = known.filter(|_| again.is_none()) {
            if self.replaced.broken.contains(&node) {
                unsupported!("a static of the base changed");
            }
            if known.symbol.is_empty() {
                unsupported!("a static that rustc's symbols name no copy of");
            }
            self.bring_behind(&node)?;
            // A thread-local of the base is one still: the patch's code reaches it as one.
            let thread_local = is_thread_local(db, id);
            let data = self.declare_named_data(
                &known.symbol,
                Linkage::Import,
                known.writable,
                thread_local,
                false,
            )?;
            self.imported_statics.insert(id, data);
            return Ok(data);
        }
        let compiled = compiled_static(db, id).as_ref().map_err(Clone::clone)?;
        if compiled.writable && !self.own_statics.owns(db, id) && !unlinked {
            let name = hir_def::signatures::StaticSignature::of(db, id).name.as_str();
            unsupported!("writable static `{name}` without a rustc symbol");
        }
        // Not shared with equal data: a static has an address of its own. Known before its
        // value is: what the value points to may point back at the static.
        let thread_local = is_thread_local(db, id);
        let own = (!self.patching).then(|| self.symbols.own_static(id)).flatten();
        let exported = exported_static(db, id).filter(|_| !self.patching);
        let data = if let Some(known) = again {
            self.declare_named_data(
                &known.symbol,
                Linkage::Hidden,
                compiled.writable,
                false,
                false,
            )?
        } else if let Some(name) = (self.base.is_some() && !thread_local && exported.is_none())
            .then(|| self.lasting_static_name(id, &compiled.blob.bytes, compiled.writable))
            .flatten()
        {
            // A patch's own static is named after itself and its value, so that the running
            // program keeps the one an earlier patch gave it under the name (`live.md#types`).
            if self.module.declarations().get_name(&name).is_some() {
                let name = StaticSignature::of(db, id).name.as_str();
                unsupported!("a static `{name}` written as another is in the same place");
            }
            self.declare_named_data(&name, Linkage::Hidden, compiled.writable, false, false)?
        } else if let Some(name) = own.or_else(|| exported.map(|it| it.as_str().to_owned())) {
            // Nothing rustc built defines it: what finds it by name finds ours.
            self.declare_named_data(&name, Linkage::Export, compiled.writable, thread_local, false)?
        } else if self.patching || thread_local {
            // Nothing else defines the statics of a program compiled to WebAssembly, and
            // under its own name a static is the same one after a build in full.
            let own = if cfg!(feature = "wasm") { self.symbols.static_(id) } else { None };
            let numbered = own.is_none();
            let name = match own {
                Some(name) => name,
                None => self.own_data_name("ra_jit::static::"),
            };
            let linkage = if self.patching { Linkage::Hidden } else { Linkage::Local };
            self.declare_named_data(&name, linkage, compiled.writable, thread_local, numbered)?
        } else {
            self.declare_local_data(compiled.writable)?
        };
        // One the build after this may replace where it is has to say how large it is.
        let named = (again.is_some() || self.patching) && !thread_local;
        let asked = AttrFlags::link_section(db, id.into());
        // On wasm the section a static asks for is a custom section of the module, which
        // holds a copy of its bytes; the static itself stays in memory like any other.
        let custom = asked.as_ref().filter(|_| Target::current().writes_wasm_objects());
        let section =
            asked.as_ref().filter(|_| custom.is_none()).map(|it| it.as_str()).or_else(|| {
                named.then(|| Target::current().section_of_static(compiled.writable)).flatten()
            });
        let known = if compiled.writable { &mut self.statics } else { &mut self.frozen_statics };
        known.insert(id, data);
        let begun = self.own_static_order.len();
        self.own_static_order.push(id);
        let resolved = self.resolve(&compiled.requests).map(|ids| ids.blob(&compiled.blob));
        // Nothing relocates a custom section: what is in one is bytes alone.
        let resolved = resolved.and_then(|blob| match custom {
            Some(custom) if !blob.relocs.is_empty() || !blob.fn_relocs.is_empty() => {
                let name = StaticSignature::of(db, id).name.as_str();
                Err(CodegenError::Unsupported(format!(
                    "static `{name}` in custom section `{}` holds an address",
                    custom.as_str()
                )))
            }
            _ => Ok(blob),
        });
        match resolved {
            Ok(blob) => {
                if let Some(custom) = custom {
                    self.custom_sections.push((custom.as_str().to_owned(), blob.bytes.clone()));
                }
                self.fill_data(data, blob, section)?;
                Ok(data)
            }
            Err(e) => {
                // The statics defined since may point to this one, which has no value.
                for id in self.own_static_order.drain(begun..) {
                    self.statics.remove(&id);
                    self.frozen_statics.remove(&id);
                }
                let nothing = ConstBlob {
                    bytes: Vec::new(),
                    relocs: Vec::new(),
                    fn_relocs: Vec::new(),
                    align: 1,
                    type_ids: Vec::new(),
                };
                self.fill_data(data, nothing, section)?;
                Err(e)
            }
        }
    }

    /// A `core::panic::Location` (`{ file: &str, line: u32, col: u32 }`).
    // FIXME: the layout of `Location` should come from `layout_of_ty`.
    fn location(&mut self, at: Option<(DefWithBodyId, MirSpan)>) -> Result<DataId> {
        let found = self.location_at(at);
        self.define_location(found)
    }

    fn location_at(&mut self, at: Option<(DefWithBodyId, MirSpan)>) -> Option<(String, u32, u32)> {
        at.and_then(|(owner, span)| source_location(self.db, owner, span))
            .and_then(|(file, line, col)| Some((self.source_path(file)?, line, col)))
    }

    fn declaration_at(&mut self, func: FunctionId) -> Option<(String, u32, u32)> {
        declaration_location(self.db, func)
            .and_then(|(file, line, col)| Some((self.source_path(file)?, line, col)))
    }

    /// A `core::panic::Location`, of no place in particular when the place is not known.
    fn define_location(&mut self, found: Option<(String, u32, u32)>) -> Result<DataId> {
        let (file, line, col) = found.unwrap_or_else(|| ("<ra-jit>".to_owned(), 0u32, 0u32));
        let mut bytes = vec![0u8; 2 * word() + 8];
        bytes[word()..2 * word()].copy_from_slice(&(file.len() as u64).to_le_bytes()[..word()]);
        bytes[2 * word()..2 * word() + 4].copy_from_slice(&line.to_le_bytes());
        bytes[2 * word() + 4..].copy_from_slice(&col.to_le_bytes());
        // The name is followed by a NUL that its length leaves out.
        let mut name = file.into_bytes();
        name.push(0);
        let name = self.define_const(ConstBlob {
            bytes: name,
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align: 1,
            type_ids: Vec::new(),
        })?;
        let mut location = ConstBlob {
            bytes,
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align: word() as u64,
            type_ids: Vec::new(),
        };
        location.point_to(0, name, 0);
        self.define_const(location)
    }

    /// The function to call for `func` instantiated with `args`: the rustc-compiled one when it
    /// has a symbol we can link against, otherwise an instance compiled into this object.
    fn callee(
        &mut self,
        instance: &Instance,
        env: &StoredParamEnvAndCrate,
        sig: &Signature,
    ) -> Result<FuncId> {
        let instance = &self.settled(instance)?;
        let (def, args) = (instance.def, instance.args.as_ref());
        if let InstanceDef::Fn(func) = def
            && args.iter().all(|arg| arg.region().is_some())
            && let Some(symbol) = self.symbols.function(func)
            && self.as_its_crate_compiles(instance)
            && let Some(symbol) = self.rustc_copy(symbol, instance.checked)
            // rustc's copy of a function of another crate would hand out rustc's future.
            && (self.own_statics.crates.contains(&func.krate(self.db))
                || !futures_kept_apart()
                || !returns_async_block(self.db, func))
            // rustc's copy takes and gives scalable vectors in SVE registers, ra-jit's code by
            // reference: it calls a copy of its own. With nothing of rustc's linked the copy is
            // ra-jit's.
            && (self.symbols.nothing_of_rustc()
                || !sve::passes_scalable(self.db, func, Some(args), env))
        {
            self.want(func);
            return self.import(&symbol, sig);
        }
        let signature = match def {
            InstanceDef::Fn(func) => Some(FunctionSignature::of(self.db, func)),
            InstanceDef::Closure(_)
            | InstanceDef::Coroutine(_)
            | InstanceDef::CoroutineByMove(_)
            | InstanceDef::DropGlue
            | InstanceDef::CallOnceShim
            | InstanceDef::VtableShim(_)
            | InstanceDef::FnPointerShim
            | InstanceDef::ReifyShim { .. } => None,
        };
        let name = self.callee_name(def);
        if let Some(Rejected { why, .. }) = self.rejected.get(instance) {
            unsupported!("callee `{name}`: {why}");
        }
        if self.handles_rustc_future(instance) {
            unsupported!("callee `{name}`: handles a future rustc lays out");
        }
        if let Some(&id) = self.instances.get(instance) {
            return Ok(id);
        }
        if let Some(known) = self.base.and_then(|base| base.instances.get(instance)) {
            let node = patch::Node::Instance(instance.clone());
            if self.replaced.broken.contains(&node) {
                unsupported!("callee `{name}`: an instance of the base gained or lost code");
            }
            // The base's copy is of the source before the edit, or the link left it out.
            if self.replaced.changed.contains(&node) || !self.symbols.linked(&known.symbol) {
                return self.compile_again(instance, known);
            }
            self.bring_behind(&node)?;
            return self.import(&known.symbol, sig);
        }
        if let InstanceDef::Fn(func) = def
            && let Some(symbol) = self.any_method(func, args)
        {
            return self.import(&symbol, sig);
        }
        // A `TypeId` is a hash only rustc can compute, and the two compilers have to agree on
        // it for `downcast` to work between their code. So take rustc's.
        if let InstanceDef::Fn(func) = def
            && let Some(of) = self.type_id_of(func, args)
        {
            let mut hasher =
                type_id::TypeIdHasher::new(self.db, self.symbols, &self.own_statics.crates);
            if let Some(hash) = hasher.type_id(of) {
                return self.constant_type_id(instance.clone(), sig, hash);
            }
            let stuck = hasher.stuck.unwrap_or_default();
            let Some(ty) = self.symbol_type(of) else {
                unsupported!(
                    "`TypeId` of `{of:?}`, neither hashed (at `{stuck}`) nor named by a rustc symbol"
                );
            };
            if let Some(symbol) = self.symbols.path(&format!("<core::any::TypeId>::of::<{ty}>")) {
                return self.import(&symbol, sig);
            }
            // rustc inlines `TypeId::of` into most of its callers. What is left then is the
            // `Any` method a `dyn Any` of the type calls, which makes no use of its `self`.
            let Some(symbol) = self.symbols.path(&format!("<{ty} as core::any::Any>::type_id"))
            else {
                unsupported!(
                    "`TypeId` of `{ty}`, not hashed (at `{stuck}`) and without a rustc symbol"
                );
            };
            let mut method = sig.clone();
            method.params.push(AbiParam::new(ptr()));
            let method = self.import(&symbol, &method)?;
            let id = self
                .module
                .declare_function(
                    &format!("ra_jit::type_id::{}", self.instances.len()),
                    Linkage::Local,
                    sig,
                )
                .map_err(|e| CodegenError::Backend(e.to_string()))?;
            let mut ctx = self.module.make_context();
            ctx.func.signature = sig.clone();
            let mut builder_ctx = std::mem::take(&mut self.builder_ctx);
            let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
            let block = builder.create_block();
            builder.append_block_params_for_function_params(block);
            builder.switch_to_block(block);
            let mut values = builder.block_params(block).to_vec();
            // A `&self` that points at nothing: any aligned address will do.
            values.push(builder.ins().iconst(ptr(), 16));
            let method = self.module.declare_func_in_func(method, builder.func);
            let call = builder.ins().call(method, &values);
            let results = builder.inst_results(call).to_vec();
            builder.ins().return_(&results);
            builder.seal_all_blocks();
            builder.finalize(self.module.isa().frontend_config());
            self.builder_ctx = builder_ctx;
            self.define_shim(id, &mut ctx)?;
            self.instances.insert(instance.clone(), id);
            return Ok(id);
        }
        if signature.is_some_and(|it| !it.has_body()) {
            // The allocator entry points are declared in `alloc` and defined at link time,
            // under names in a crate of their own.
            if let Some(symbol) = self.symbols.path(&format!("__rustc::{name}")) {
                return self.import(&symbol, sig);
            }
            // FIXME: this passes arguments the way Rust functions take them, which is only
            // what a foreign function expects for scalars and pointers.
            if let InstanceDef::Fn(func) = def
                && let Some(symbol) = link_name(self.db, func)
                && !symbol.starts_with("llvm.")
            {
                if let Some(module) = wasm_import_module(self.db, func) {
                    return self.import(&wasm_object::import_symbol(&module, &symbol), sig);
                }
                return self.import(&symbol, sig);
            }
            if let InstanceDef::Fn(func) = def
                && let ItemContainerId::TraitId(_) = func.lookup(self.db).container
            {
                let called =
                    self.instance_path(func, args).unwrap_or_else(|| format!("{name}::<{args:?}>"));
                unsupported!("call to `{called}`: no impl of the trait was selected for it");
            }
            unsupported!("call to `{name}`, which has no body and no rustc symbol");
        }
        let from_store = self.entries.is_some() || self.base.is_some_and(|it| !it.found.is_empty());
        let stable = if from_store { self.stable_name(instance) } else { None };
        if let Some(name) = &stable {
            let found = self.base.and_then(|base| base.found.get(name));
            if let Some(symbol) = found.filter(|it| self.symbols.linked(it)) {
                return self.import(symbol, sig);
            }
            let body = signature.is_some().then_some(sig);
            if let Some(id) = self.found_callee(name, body)? {
                self.instances.insert(instance.clone(), id);
                let home = self.homes.home(self.db, instance, env);
                self.found_instances.insert(id, (instance.clone(), home));
                return Ok(id);
            }
        }
        // A patch is linked with its base, whose names these must not take.
        let env = &self.homes.home(self.db, instance, env);
        let prefix = if self.base.is_some() { "ra-jit.patch" } else { "ra-jit" };
        let symbol = format!("{prefix}::{name}::{}", self.instances.len());
        let linkage = if self.patching { Linkage::Hidden } else { Linkage::Local };
        let id = self
            .module
            .declare_function(&symbol, linkage, sig)
            .map_err(|e| CodegenError::Backend(e.to_string()))?;
        self.instances.insert(instance.clone(), id);
        self.pending.push((instance.clone(), env.clone(), id));
        self.envs.push((instance.clone(), env.clone()));
        if let Some(name) = stable {
            self.named.insert(name, id);
        }
        Ok(id)
    }

    /// The base's instance compiled into the patch under the base's symbol, which the splice
    /// sends what called the base's copy on to.
    fn compile_again(
        &mut self,
        instance: &Instance,
        known: &patch::BaseInstance,
    ) -> Result<FuncId> {
        if let Some(&id) = self.instances.get(instance) {
            return Ok(id);
        }
        let compiled = compiled_instance(self.db, instance.clone(), known.env.clone());
        let compiled = compiled.as_ref().map_err(Clone::clone)?;
        let id = self.module.declare_function(
            &known.symbol,
            Linkage::Hidden,
            &compiled.code.signature,
        )?;
        self.instances.insert(instance.clone(), id);
        self.pending.push((instance.clone(), known.env.clone(), id));
        Ok(id)
    }

    /// Compiles what the base's copy of `node` reaches that the edit changed and nothing the
    /// patch compiled reached before ([`patch::Unchanged::behind`]): the patch refers to that
    /// copy now, and through it to code of the source before.
    fn bring_behind(&mut self, node: &patch::Node) -> Result<()> {
        let Some(behind) = self.replaced.behind.remove(node) else { return Ok(()) };
        let base = self.base.expect("only a patch has what it left behind");
        for below in behind {
            if self.replaced.broken.contains(&below) {
                unsupported!("what the patch reaches again reaches code of the base that changed");
            }
            match &below {
                patch::Node::Instance(instance) => {
                    self.compile_again(instance, &base.instances[instance])?;
                }
                patch::Node::Static(id) => {
                    self.static_data(*id)?;
                }
                patch::Node::Root(_) => {}
            }
        }
        Ok(())
    }

    fn callee_name(&self, def: InstanceDef) -> &'db str {
        callee_name(self.db, def)
    }

    /// Adds to `failed` the instances that call one of them, and so on up: the next round
    /// would reject those, and the round after it their callers. One missed here only costs
    /// that round.
    fn poison_callers(&self, failed: &mut Vec<(Instance, Rejected)>) {
        if failed.is_empty() {
            return;
        }
        let mut callers: FxHashMap<&Instance, Vec<usize>> = FxHashMap::default();
        for (at, (instance, env)) in self.envs.iter().enumerate() {
            let Ok(compiled) = compiled_instance(self.db, instance.clone(), env.clone()).as_ref()
            else {
                continue;
            };
            for request in &compiled.requests.funcs {
                if let FuncTarget::Instance(callee, _) = &request.target {
                    callers.entry(callee).or_default().push(at);
                }
            }
        }
        let mut seen: FxHashSet<Instance> = failed.iter().map(|it| it.0.clone()).collect();
        let mut next = 0;
        while let Some((callee, Rejected { why, .. })) = failed.get(next) {
            next += 1;
            let why = format!("callee `{}`: {why}", self.callee_name(callee.def));
            for &at in callers.get(callee).into_iter().flatten() {
                let (caller, env) = &self.envs[at];
                if !self.stands_in(caller) && seen.insert(caller.clone()) {
                    failed.push((caller.clone(), Rejected { env: env.clone(), why: why.clone() }));
                }
            }
        }
    }

    /// Makes `failed` what the next round rejects, with their callers, and the statics of the
    /// roots it then leaves to rustc theirs from its start: found in it, they would start it
    /// over again.
    fn reject(
        &self,
        failed: &mut Vec<(Instance, Rejected)>,
        compiled: &[(FunctionId, String)],
        from_rustc: &FxHashSet<FunctionId>,
        own_statics: &mut OwnStatics,
    ) {
        if failed.is_empty() {
            return;
        }
        self.poison_callers(failed);
        let left = self.left_after(compiled, failed);
        let found = TheirStatics::of(self.db, from_rustc.iter().copied().chain(left));
        own_statics.theirs.extend(found.statics);
        own_statics.unknown.extend(found.unknown);
    }

    /// Whether rustc's copy of `instance` stands in for it when it is not compiled, which its
    /// callers then call.
    fn stands_in(&self, instance: &Instance) -> bool {
        self.forwarded.contains(instance)
            || matches!(instance.def, InstanceDef::Fn(func) if self
                .instance_path(func, instance.args.as_ref())
                .and_then(|path| self.symbols.path(&path))
                .is_some_and(|it| self.rustc_copy(it, instance.checked).is_some()))
    }

    /// The roots of `compiled` that the next round leaves to rustc, `failed` being rejected
    /// and their callers poisoned: those that call one rustc has no copy of.
    fn left_after(
        &self,
        compiled: &[(FunctionId, String)],
        failed: &[(Instance, Rejected)],
    ) -> Vec<FunctionId> {
        let lost: FxHashSet<&Instance> =
            failed.iter().map(|it| &it.0).filter(|it| !self.stands_in(it)).collect();
        let calls_lost = |root: FunctionId| {
            compiled_root(self.db, root).as_ref().is_ok_and(|it| {
                it.requests.funcs.iter().any(|request| {
                    matches!(&request.target, FuncTarget::Instance(callee, _) if lost.contains(callee))
                })
            })
        };
        compiled
            .iter()
            .map(|it| it.0)
            .filter(|&root| self.symbols.rustc_built(root.krate(self.db)) && calls_lost(root))
            .collect()
    }

    /// Whether `instance` is the poll or the drop of a future rustc's code makes. Everything
    /// that reads such a future goes through one of those, so refusing them refuses the rest.
    fn handles_rustc_future(&self, instance: &Instance) -> bool {
        if self.rustc_futures.is_empty() || !futures_kept_apart() {
            return false;
        }
        let block = match instance.def {
            InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => Some(id),
            InstanceDef::DropGlue => match instance.args.as_ref().type_at(0).kind() {
                TyKind::Coroutine(id, _) => Some(id.0.as_id()),
                _ => None,
            },
            _ => None,
        };
        block.is_some_and(|id| self.rustc_futures.contains(&id))
    }

    /// Makes `id` a call of the copy of `instance` rustc compiled, when there is one: an
    /// instance we cannot compile is still one we can call.
    fn forward_to_rustc(&mut self, instance: &Instance, id: FuncId) -> Option<()> {
        let InstanceDef::Fn(func) = instance.def else { return None };
        let symbol = self.symbols.path(&self.instance_path(func, instance.args.as_ref())?)?;
        let symbol = self.rustc_copy(symbol, instance.checked)?;
        let sig = self.module.declarations().get_function_decl(id).signature.clone();
        let target = self.import(&symbol, &sig).ok()?;
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        let mut builder_ctx = std::mem::take(&mut self.builder_ctx);
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let block = builder.create_block();
        builder.append_block_params_for_function_params(block);
        builder.switch_to_block(block);
        let values = builder.block_params(block).to_vec();
        let target = self.module.declare_func_in_func(target, builder.func);
        let call = builder.ins().call(target, &values);
        let results = builder.inst_results(call).to_vec();
        builder.ins().return_(&results);
        builder.seal_all_blocks();
        builder.finalize(self.module.isa().frontend_config());
        self.builder_ctx = builder_ctx;
        self.define_shim(id, &mut ctx).ok()
    }

    fn spelling(&self) -> Spelling<'_, 'db> {
        Spelling { db: self.db, symbols: self.symbols }
    }

    fn instance_path(&self, func: FunctionId, args: GenericArgs<'db>) -> Option<String> {
        self.spelling().instance_path(func, args)
    }

    /// A `TypeId::of::<T>` that returns `hash`, the two words of which are its value.
    fn constant_type_id(
        &mut self,
        instance: Instance,
        sig: &Signature,
        hash: u128,
    ) -> Result<FuncId> {
        if let Some(&id) = self.instances.get(&instance) {
            return Ok(id);
        }
        let id = self.type_id_shim(sig, hash)?;
        self.instances.insert(instance, id);
        Ok(id)
    }

    /// The `TypeId::of` that returns `hash`.
    fn type_id_shim(&mut self, sig: &Signature, hash: u128) -> Result<FuncId> {
        if let Some(&id) = self.type_id_shims.get(&hash) {
            return Ok(id);
        }
        let name = format!("ra_jit::type_id::h{}", self.type_id_shims.len());
        let id = self
            .module
            .declare_function(&name, Linkage::Local, sig)
            .map_err(|e| CodegenError::Backend(e.to_string()))?;
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig.clone();
        let mut builder_ctx = std::mem::take(&mut self.builder_ctx);
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let block = builder.create_block();
        builder.append_block_params_for_function_params(block);
        builder.switch_to_block(block);
        let words = [hash as u64, (hash >> 64) as u64].map(|it| it as i64);
        let returned = match *sig.returns.as_slice() {
            [] => {
                let &[out] = builder.block_params(block) else {
                    unsupported!("a `TypeId` returned as `{sig}`");
                };
                for (word, offset) in words.into_iter().zip([0, 8]) {
                    let word = builder.ins().iconst(types::I64, word);
                    builder.ins().store(MEM, word, out, offset);
                }
                Vec::new()
            }
            [low, high] if low.value_type == types::I64 && high.value_type == types::I64 => {
                words.map(|it| builder.ins().iconst(types::I64, it)).to_vec()
            }
            [whole] if whole.value_type == types::I128 => {
                let [low, high] = words.map(|it| builder.ins().iconst(types::I64, it));
                vec![builder.ins().iconcat(low, high)]
            }
            _ => unsupported!("a `TypeId` returned as `{sig}`"),
        };
        builder.ins().return_(&returned);
        builder.seal_all_blocks();
        builder.finalize(self.module.isa().frontend_config());
        self.builder_ctx = builder_ctx;
        self.define_shim(id, &mut ctx)?;
        self.type_id_shims.insert(hash, id);
        self.type_ids.insert(id, hash);
        Ok(id)
    }

    /// The `T` of `TypeId::of::<T>`, when `func` is that function.
    fn type_id_of(&self, func: FunctionId, args: GenericArgs<'db>) -> Option<Ty<'db>> {
        let db = self.db;
        if FunctionSignature::of(db, func).name.as_str() != "of" {
            return None;
        }
        let ItemContainerId::ImplId(impl_) = func.lookup(db).container else { return None };
        let TyKind::Adt(adt, _) = db.impl_self_ty(impl_).skip_binder().kind() else { return None };
        if self.symbols.item(adt.def_id().into())? != "core::any::TypeId" {
            return None;
        }
        args.as_slice().first()?.ty()
    }

    fn symbol_type(&self, ty: Ty<'db>) -> Option<String> {
        self.spelling().symbol_type(ty)
    }

    /// rustc's own `<dyn Any>::is::<T>` or `downcast_ref::<T>`, when `func` is such a method
    /// and rustc compiled it for this `T`. It has the `TypeId` of `T` inlined in it, which may
    /// be the one place to get it.
    fn any_method(&self, func: FunctionId, args: GenericArgs<'db>) -> Option<String> {
        let db = self.db;
        let name = FunctionSignature::of(db, func).name.as_str();
        let ItemContainerId::ImplId(impl_) = func.lookup(db).container else { return None };
        let self_ty = db.impl_self_ty(impl_).skip_binder();
        let TyKind::Dynamic(bounds, _) = self_ty.kind() else { return None };
        let principal = bounds.principal_def_id()?.0;
        if self.symbols.item(principal.into())? != "core::any::Any" {
            return None;
        }
        let ty = self.symbol_type(args.as_slice().first()?.ty()?)?;
        self.symbols.path(&format!("<{}>::{name}::<{ty}>", self.symbol_type(self_ty)?))
    }

    /// The symbol a function declared in an `extern` block stands for.
    fn define_const(&mut self, blob: ConstBlob) -> Result<DataId> {
        self.define_data(blob, false)
    }

    fn define_data(&mut self, blob: ConstBlob, writable: bool) -> Result<DataId> {
        let identity = if writable { DataIdentity::Writable } else { DataIdentity::Shared };
        self.define_data_as(blob, identity)
    }

    fn define_data_as(&mut self, blob: ConstBlob, identity: DataIdentity) -> Result<DataId> {
        let writable = identity == DataIdentity::Writable;
        let shared = (identity == DataIdentity::Shared).then(|| blob.clone());
        if let Some(&id) = shared.as_ref().and_then(|blob| self.consts.get(blob)) {
            return Ok(id);
        }
        let id = self.declare_local_data(writable)?;
        self.fill_data(id, blob, None)?;
        if let Some(blob) = shared {
            self.consts.insert(blob, id);
        }
        Ok(id)
    }

    fn declare_local_data(&mut self, writable: bool) -> Result<DataId> {
        let name = self.own_data_name(".Lra-jit.const.");
        self.declare_named_data(&name, Linkage::Local, writable, false, true)
    }

    /// The next name of the object's own for a static or a constant, under `prefix`. A patch
    /// imports the base's statics by the names the build in full gave them, from the same
    /// numbering: a patch's own are numbered under names of their own, which never meet
    /// those (#291). A build in full that patches come after has `patching` but no base.
    fn own_data_name(&mut self, prefix: &str) -> String {
        let n = self.location_counter;
        self.location_counter += 1;
        match self.base.is_some() {
            true => format!("{prefix}patch.{n}"),
            false => format!("{prefix}{n}"),
        }
    }

    /// [`Module::declare_data`], with an error naming the symbol where cranelift-module would
    /// panic or quietly make two things one: a name declared once thread-local and once not
    /// ("Can't change TLS data object to normal", #291), a function's name, or, for a name the
    /// object numbered for data of its own (`own`), one already declared, whose definition
    /// would then stand for both.
    fn declare_named_data(
        &mut self,
        name: &str,
        linkage: Linkage,
        writable: bool,
        thread_local: bool,
        own: bool,
    ) -> Result<DataId> {
        use cranelift_module::FuncOrDataId;
        let shown = |tls: bool, linkage: Linkage| {
            format!("{}{linkage:?}", if tls { "thread-local " } else { "" })
        };
        match self.module.declarations().get_name(name) {
            Some(FuncOrDataId::Func(_)) => {
                return Err(CodegenError::Backend(format!(
                    "data `{name}` has the name of a function of the object"
                )));
            }
            Some(FuncOrDataId::Data(id)) => {
                let was = self.module.declarations().get_data_decl(id);
                if was.tls != thread_local || own {
                    return Err(CodegenError::Backend(format!(
                        "data `{name}` declared as {} is declared again as {}{}",
                        shown(was.tls, was.linkage),
                        shown(thread_local, linkage),
                        if own { ", of the object's own" } else { "" },
                    )));
                }
            }
            None => {}
        }
        Ok(self.module.declare_data(name, linkage, writable, thread_local)?)
    }

    /// An `extern_weak` static: not the memory at `symbol` but a pointer to it, 0 when nothing
    /// linked defines it.
    fn weak_static(&mut self, symbol: &str) -> Result<DataId> {
        use cranelift_module::FuncOrDataId;
        let target = match self.module.declarations().get_name(symbol) {
            Some(known) => known,
            None => FuncOrDataId::Data(
                self.module
                    .declare_data(symbol, Linkage::Import, false, false)
                    .map_err(|e| CodegenError::Backend(e.to_string()))?,
            ),
        };
        self.weak_imports.push(target);
        let data = self.declare_local_data(false)?;
        let bytes = self.module.isa().pointer_bytes();
        let mut description = DataDescription::new();
        description.define_zeroinit(bytes.into());
        description.set_align(bytes.into());
        match target {
            FuncOrDataId::Func(id) => {
                let func = self.module.declare_func_in_data(id, &mut description);
                description.write_function_addr(0, func);
            }
            FuncOrDataId::Data(id) => {
                let global = self.module.declare_data_in_data(id, &mut description);
                description.write_data_addr(0, global, 0);
            }
        }
        self.module
            .define_data(data, &description)
            .map_err(|e| CodegenError::Backend(e.to_string()))?;
        Ok(data)
    }

    /// Says that the patch leaves the base's copy of a static, `symbol`, behind ([`MOVED_STATIC`]).
    fn leave_behind(&mut self, symbol: &str) -> Result<()> {
        if symbol.is_empty() {
            return Ok(());
        }
        let name = format!("{MOVED_STATIC}{symbol}");
        let data = self.declare_named_data(&name, Linkage::Hidden, false, false, true)?;
        let mut description = DataDescription::new();
        description.define_zeroinit(1);
        self.module
            .define_data(data, &description)
            .map_err(|e| CodegenError::Backend(e.to_string()))
    }

    /// Writes rustc's hash of each type in `blob.type_ids` at its place in the bytes.
    fn hash_type_ids(&self, blob: &mut ConstBlob) -> Result<()> {
        hash_type_ids(self.db, self.symbols, &self.own_statics.crates, blob)
    }

    /// Defines `id` as `blob`, in the section named `section` when there is one.
    fn fill_data(&mut self, id: DataId, mut blob: ConstBlob, section: Option<&str>) -> Result<()> {
        self.hash_type_ids(&mut blob)?;
        let mut data = DataDescription::new();
        if let Some(section) = section {
            data.set_custom_section(section);
        }
        data.define(blob.bytes.into_boxed_slice());
        data.set_align(blob.align);
        for (offset, target, addend) in blob.relocs {
            let target = self.module.declare_data_in_data(target, &mut data);
            data.write_data_addr(offset, target, addend);
        }
        for (offset, target) in blob.fn_relocs {
            let target = self.module.declare_func_in_data(target, &mut data);
            data.write_function_addr(offset, target);
        }
        self.module.define_data(id, &data).map_err(|e| CodegenError::Backend(e.to_string()))
    }

    fn import(&mut self, symbol: &str, sig: &Signature) -> Result<FuncId> {
        if let Some(&id) = self.imports.get(symbol) {
            return Ok(id);
        }
        let id = self
            .module
            .declare_function(symbol, Linkage::Import, sig)
            .map_err(|e| CodegenError::Backend(e.to_string()))?;
        self.imports.insert(symbol.to_owned(), id);
        Ok(id)
    }
}

/// What a call to an intrinsic turned into.
enum Intrinsic {
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
    in_static: bool,
    defined: FxHashMap<usize, DataId>,
    /// The allocations [`FunctionCx::relocate_data_pointer`] is in the middle of, by start.
    walking: Vec<usize>,
}

impl Relocations<'_, '_> {
    /// The alignment to give the data object made for the memory at `start`: what const eval
    /// made it with, or where it does not say, the most a value could need.
    fn align_of(&self, start: usize) -> u64 {
        self.map.align_at(start).map_or(16, |align| align as u64)
    }
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

    /// Whether every pointer in the blob is whole inside it and to one of the first `funcs`
    /// functions or the first `datas` data objects, which a blob read from the store is
    /// not known to be.
    fn sound(&self, funcs: usize, datas: usize) -> bool {
        let inside = |offset: u32| {
            self.bytes.len().checked_sub(word()).is_some_and(|last| offset as usize <= last)
        };
        (self.align == 0 || self.align.is_power_of_two())
            && self.relocs.iter().all(|&(at, to, _)| inside(at) && (to.as_u32() as usize) < datas)
            && self.fn_relocs.iter().all(|&(at, to)| inside(at) && (to.as_u32() as usize) < funcs)
    }

    /// Makes the pointer at `offset` the address `at` bytes into `target`.
    fn point_to(&mut self, offset: usize, target: DataId, at: usize) {
        self.bytes[offset..offset + word()].fill(0);
        self.relocs.push((offset as u32, target, at as i64));
    }

    fn usize_at(&self, offset: usize) -> Option<usize> {
        let bytes = self.bytes.get(offset..offset + word())?;
        Some(bytes.iter().rev().fold(0, |word, &byte| word << 8 | usize::from(byte)))
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

/// The locals a call's operands move whole, whose memory the callee can be handed as it is.
struct GivenAway {
    /// The local the call's result is written to.
    result: LocalId,
    whole: FxHashSet<Value>,
    /// The address of every place the call's operands read.
    read: Vec<Value>,
}

impl GivenAway {
    /// Whether the memory at `addr` is a local moved whole that no other operand reads: a
    /// field at the start of a local has the local's address.
    fn gives(&self, addr: Value) -> bool {
        self.whole.contains(&addr) && self.read.iter().filter(|&&it| it == addr).count() == 1
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
    /// `None` while generating a shim that has no MIR.
    body: Option<&'db MirBody<'db>>,
    env: ParamEnvAndCrate<'db>,
    infcx: InferCtxt<'db>,
    interner: DbInterner<'db>,
    builder: FunctionBuilder<'cx>,
    locals: ArenaMap<LocalId, LocalSlot>,
    blocks: ArenaMap<BasicBlockId, ir::Block>,
    /// The layouts asked for so far: a body asks for that of the same few types many times.
    layouts: std::cell::RefCell<FxHashMap<Ty<'db>, Arc<Layout>>>,
    /// Whether dropping each type asked about so far does anything.
    glue: std::cell::RefCell<FxHashMap<Ty<'db>, DropGlue>>,
    /// A landing pad that takes over from the cleanup block of `unwind_to` for the calls
    /// made while it is set: drop glue unwinding from one field's drop to the rest.
    inner_pad: Option<ir::Block>,
    /// The cleanup block that what is being generated unwinds to. The body is in
    /// `MirPhase::Runtime`: its drops are elaborated and it names its cleanup blocks.
    unwind_to: Option<BasicBlockId>,
    /// The landing pad of each cleanup block something unwinds to.
    pads: Vec<(BasicBlockId, ir::Block)>,
    /// Where the exception is kept while the cleanup blocks run.
    exception: Option<ir::StackSlot>,
    /// The slots the arguments of the calls so far were copied to since the last were
    /// freed, each with its size and the power of two it is aligned to.
    argument_slots: Vec<(ir::StackSlot, (u32, u8))>,
    /// The ones whose call has returned.
    free_argument_slots: Vec<(ir::StackSlot, (u32, u8))>,
    /// While a call is generated: the local its result goes to, and where the locals moved
    /// whole into it are. Nothing reads those again, so the callee can be handed them.
    given_away: Option<GivenAway>,
    /// The block of the body being generated.
    block: Option<BasicBlockId>,
    in_cleanup: bool,
    /// The crate it is compiled for has `panic = "abort"`: no call of it unwinds (rustc's
    /// `fn_can_unwind`), and so none has a landing pad.
    aborts: bool,
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
    /// Where the statement being compiled comes from.
    span: MirSpan,
    /// Promoted locals that are read-only data instead of a slot the body writes to.
    constant_locals: Vec<LocalId>,
    /// The locals kept in a register in place of their slot: scalars nothing takes the
    /// address of. The slot is still there, written when something asks where the local is.
    registers: FxHashMap<LocalId, Register>,
    /// What the body's generic parameters stand for, when it is the body of an instance.
    args: Option<StoredGenericArgs>,
    /// While compiling an `async` body: where it says where it left off.
    suspended: Option<SuspendedState>,
    /// The locals an `async` body keeps in its future, and how far into it each is.
    held_at: Vec<(LocalId, u32)>,
}

/// Where an `async` body says where it left off, and what that says of its locals.
struct SuspendedState {
    addr: Value,
    ty: Type,
    held: hir_ty::mir::HeldLocals,
}

/// `args` with every lifetime but a bound one erased: lifetimes change nothing about the
/// code, and the ones a caller passes on are its own parameters, which mean nothing in the
/// callee.
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
        static ERASED: std::cell::RefCell<FxHashMap<StoredGenericArgs, StoredGenericArgs>> =
            std::cell::RefCell::default();
    }
    if !args.has_free_regions() {
        return args;
    }
    let key = args.store();
    if let Some(known) = ERASED.with_borrow(|it| it.get(&key).map(|it| it.as_ref())) {
        return known;
    }
    let erased = rustc_type_ir::fold_regions(interner, args, |region, _| match region.kind() {
        rustc_type_ir::RegionKind::ReBound(..) => region,
        _ => Region::new_erased(interner),
    });
    ERASED.with_borrow_mut(|it| {
        if it.len() >= KEPT {
            it.clear();
        }
        it.insert(key, erased.store());
    });
    erased
}

/// Whether calls to `func` pass a `&Location` after the declared arguments. A method of a trait
/// impl inherits the attribute from the trait's declaration.
fn track_caller(db: &dyn HirDatabase, func: FunctionId) -> bool {
    let has =
        |func: FunctionId| AttrFlags::query(db, func.into()).contains(AttrFlags::TRACK_CALLER);
    has(func) || trait_declaration(db, func).is_some_and(has)
}

/// The trait method declared by `func` or implemented by it.
fn trait_declaration(db: &dyn HirDatabase, func: FunctionId) -> Option<FunctionId> {
    match func.lookup(db).container {
        ItemContainerId::TraitId(_) => Some(func),
        ItemContainerId::ImplId(impl_id) => {
            let trait_ref = db.impl_trait(impl_id)?;
            let name = &FunctionSignature::of(db, func).name;
            trait_ref.skip_binder().def_id.0.trait_items(db).method_by_name(name)
        }
        _ => None,
    }
}

/// The integer an address is on the target.
fn ptr() -> Type {
    Target::current().pointer()
}

/// How many bytes an address takes on the target.
fn word() -> usize {
    Target::current().word()
}

/// Where the second word of a wide pointer is.
fn meta_at() -> i32 {
    word() as i32
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

/// The local a body returns in, as `hir_ty::mir`'s own private `return_slot`.
fn return_slot() -> LocalId {
    LocalId::from_raw(la_arena::RawIdx::from(0))
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
    asm::neon_instruction(extension, &format!("{op} {operands}"))
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
        let meta = self
            .is_unsized(pointee)
            .then(|| self.builder.ins().load(ptr(), MEM, addr, at + meta_at()));
        Ok((self.builder.ins().load(ptr(), MEM, addr, at), meta))
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
                let size = self.builder.ins().iconst(ptr(), layout.size.bytes() as i64);
                return Ok((size, self.builder.ins().iconst(ptr(), align)));
            }
        };
        match ty.kind() {
            TyKind::Str => Ok((meta, self.builder.ins().iconst(ptr(), 1))),
            TyKind::Slice(elem) => {
                let elem = self.layout(elem)?;
                let size = self.builder.ins().imul_imm(meta, elem.size.bytes() as i64);
                Ok((size, self.builder.ins().iconst(ptr(), elem.align.abi.bytes() as i64)))
            }
            // Size and align are the vtable's second and third words.
            TyKind::Dynamic(..) => Ok((
                self.builder.ins().load(ptr(), MEM, meta, meta_at()),
                self.builder.ins().load(ptr(), MEM, meta, 2 * meta_at()),
            )),
            _ => {
                let (field, field_ty) = self.last_field(ty)?;
                let offset = layout.fields.offset(field).bytes() as i64;
                let (tail_size, mut tail_align) =
                    self.size_and_align_of_val(field_ty, Some(meta))?;
                if let Some(pack) = self.pack(ty) {
                    let pack = self.builder.ins().iconst(ptr(), pack as i64);
                    tail_align = self.builder.ins().umin(tail_align, pack);
                }
                let sized_align = self.builder.ins().iconst(ptr(), align);
                let full_align = self.builder.ins().umax(tail_align, sized_align);
                let offset = self.builder.ins().iconst(ptr(), offset);
                let offset = self.align_up(offset, tail_align);
                let size = self.builder.ins().iadd(offset, tail_size);
                Ok((self.align_up(size, full_align), full_align))
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

    fn align_up(&mut self, value: Value, align: Value) -> Value {
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
            let pack = self.builder.ins().iconst(ptr(), pack as i64);
            align = self.builder.ins().umin(align, pack);
        }
        let offset = self.builder.ins().iconst(ptr(), offset.bytes() as i64);
        let offset = self.align_up(offset, align);
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

    fn layout(&self, ty: Ty<'db>) -> Result<Arc<Layout>> {
        if let Some(layout) = self.layouts.borrow().get(&ty) {
            return Ok(layout.clone());
        }
        let layout = self
            .cg
            .db
            .layout_of_ty(ty.store(), self.env.store())
            .map_err(|e| CodegenError::Frontend(format!("layout of {ty:?}: {e:?}")))?;
        if let Some(log) = &self.cg.layouts {
            log.borrow_mut().entry(ty).or_insert((layout.size.bytes(), layout.align.abi.bytes()));
            if let Some(line) = coroutine_layout_line(self.cg.db, ty, &layout) {
                let (size, align) = (layout.size.bytes(), layout.align.abi.bytes());
                self.cg.coroutine_layouts.borrow_mut().push((line, size, align));
            }
        }
        // What is laid out at the least vector length is stored for the largest.
        let layout = sve::storage(layout);
        self.layouts.borrow_mut().insert(ty, layout.clone());
        Ok(layout)
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
            Primitive::Pointer(_) => AbiScalar { ty: ptr(), signed: false },
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
        // for: between ra-jit's own functions it goes by reference. Where foreign code would
        // pass it, `fn_abi` and `callee` refuse.
        if let BackendRepr::SimdScalableVector { .. } = layout.backend_repr {
            return Ok(PassMode::Indirect);
        }
        // rustc returns in memory what is more than two registers hold.
        let wide = is_ret && layout.size.bytes() > 2 * word() as u64;
        Ok(match layout.backend_repr {
            BackendRepr::Scalar(_) if wide => PassMode::Indirect,
            BackendRepr::Scalar(scalar) => PassMode::Direct(self.scalar(scalar)?),
            BackendRepr::ScalarPair { a, b, b_offset: offset } => {
                let (a, b) = (self.scalar(a)?, self.scalar(b)?);
                // LLVM has a WebAssembly function return one value: of a pair it makes a
                // pointer to where the pair goes, the first parameter.
                let one_result = cfg!(feature = "wasm");
                if is_ret && (wide || one_result || a.ty == types::I128 || b.ty == types::I128) {
                    // More than the two return registers hold, so LLVM returns it in memory.
                    PassMode::Indirect
                } else {
                    PassMode::Pair(a, b, offset)
                }
            }
            BackendRepr::Memory { sized: true } => {
                let max = word() as u64;
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
        let args: Vec<Ty<'db>> = args.into_iter().collect();
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
        let args: Vec<_> = args.into_iter().collect();
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
        if Target::current().is_wasm() {
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
        while let Some((at, stack)) = lone_register_i128(&self.signature(&abi.args, abi.ret).params)
        {
            // The argument the parameter is of, and which of the argument's parameters it is.
            let mut first = usize::from(matches!(abi.ret, PassMode::Indirect));
            let arg = abi
                .args
                .iter()
                .position(|&mode| {
                    let mut these = Vec::new();
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
        while let Some((at, skipped)) =
            stranded_i128(&self.signature(&abi.args, abi.ret).params, even)
        {
            let mut first = usize::from(matches!(abi.ret, PassMode::Indirect));
            let arg = abi
                .args
                .iter()
                .position(|&mode| {
                    let mut these = Vec::new();
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
        let mut modes = Vec::new();
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
        let mut modes = Vec::new();
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
        let mut leaves = Vec::new();
        self.scalar_leaves(ty, 0, &mut leaves)?;
        // The members of a union lie over each other.
        leaves.sort_by_key(|&(offset, _)| offset);
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
        leaves: &mut Vec<(u64, Scalar)>,
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
        let fields: Vec<Ty<'db>> = match layout.backend_repr {
            BackendRepr::Scalar(scalar) => return Ok(fits(Some(scalar))),
            BackendRepr::Memory { sized: true } => match ty.kind() {
                TyKind::Tuple(tys) => tys.iter().collect(),
                TyKind::Array(element, _) if layout.fields.count() == 1 => vec![element],
                TyKind::Adt(def, subst) => {
                    let AdtId::StructId(id) = def.def_id() else { return Ok(None) };
                    let fields = self.cg.db.field_types(id.into());
                    fields
                        .iter()
                        .map(|(_, ty)| ty.ty().instantiate(self.interner, subst).skip_norm_wip())
                        .collect()
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
            (BackendRepr::SimdVector { .. }, _) => unsupported!("vector in the C ABI"),
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
            BackendRepr::SimdVector { .. } => unsupported!("vector in the C ABI"),
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
        self.cg.db.target_data_layout_or_default(self.env.krate)
    }

    fn push_abi_params(&self, mode: PassMode, params: &mut Vec<AbiParam>) {
        let copied = |size| AbiParam::special(ptr(), ArgumentPurpose::StructArgument(size));
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
            PassMode::Indirect => params.push(AbiParam::new(ptr())),
            PassMode::IndirectUnsized => {
                params.extend([AbiParam::new(ptr()), AbiParam::new(ptr())])
            }
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
            PassMode::Indirect => {
                sig.params.push(AbiParam::special(ptr(), ArgumentPurpose::StructReturn))
            }
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
                if self.given_away.as_ref().is_some_and(|it| it.gives(addr)) {
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
                values.push(self.builder.ins().load(ptr(), MEM, addr, 0));
                values.push(self.builder.ins().load(ptr(), MEM, addr, meta_at()));
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
                self.builder.ins().store(MEM, meta, addr, meta_at());
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
        self.builder.ins().stack_addr(ptr(), slot, 0)
    }

    /// Storage for an integer of 128 bits.
    fn scratch_wide(&mut self) -> Value {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            16,
            4,
        ));
        self.builder.ins().stack_addr(ptr(), slot, 0)
    }

    /// Storage for a fat pointer.
    fn scratch_pair(&mut self) -> Value {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            16,
            3,
        ));
        self.builder.ins().stack_addr(ptr(), slot, 0)
    }

    /// Storage for a temporary that is borrowed for `'static`. The function writes the same
    /// constant there every time it runs.
    // FIXME: evaluate the constant and make this read-only data.
    fn promoted_slot(&mut self, layout: &Layout) -> Result<Value> {
        let align = layout.align.abi.bytes();
        if layout.size.bytes() == 0 {
            return Ok(self.builder.ins().iconst(ptr(), align as i64));
        }
        let blob = ConstBlob {
            bytes: vec![0; layout.size.bytes_usize()],
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align,
            type_ids: Vec::new(),
        };
        let data = self.cg.define_data(blob, true)?;
        let data = self.cg.declare_data_in_func(data, self.builder.func);
        Ok(self.builder.ins().symbol_value(ptr(), data))
    }

    /// The read-only data a promoted `&expr` points to, when `expr` can be evaluated now.
    /// Equal constants share an address, as they do for rustc. A type that is not `plain` is
    /// promoted when the value is: a `None` of an `Option<Box<T>>` has nothing to drop.
    fn promoted_constant(
        &mut self,
        body: &'db MirBody<'db>,
        expr: ExprId,
        layout: &Layout,
        plain: bool,
    ) -> Option<Value> {
        let db = self.cg.db;
        if layout.size.bytes() == 0 || self.mentions_location(body, expr) {
            return None;
        }
        let constant = monomorphized_promoted_mir_body_query(
            db,
            body.owner,
            expr,
            self.args.clone()?,
            self.env.store(),
        )
        .ok()?;
        let allocation = interpret_mir(db, constant, false, Some(self.env)).ok()?.0.ok()?;
        let ty = allocation.ty;
        let blob = self.allocation_blob(allocation, false).ok()?;
        if blob.bytes.len() != layout.size.bytes_usize()
            || !plain && !self.is_plain_value(&blob.bytes, 0, ty)
        {
            return None;
        }
        self.blob_addr(blob).ok()
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
        self.builder.ins().stack_addr(ptr(), slot, 0)
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
            return self.builder.ins().stack_addr(ptr(), stack_slot, offset);
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
            return self.builder.ins().stack_addr(ptr(), slot, 0);
        }
        // Cranelift keeps the frame 16-byte aligned and no more, so over-allocate and round up.
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size + align as u32,
            4,
        ));
        let base = self.builder.ins().stack_addr(ptr(), slot, 0);
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
            sig.params.push(AbiParam::new(ptr()));
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
                variadic.rest(&mut sig);
                Some(variadic)
            }
            false => None,
        };
        self.builder.func.signature = sig;
        self.cg.pointees = self.pointees(body, ret, &param_modes)?;

        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let mut incoming = self.builder.block_params(entry).to_vec().into_iter();

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
            let homed: Vec<Value> = (0..named.left())
                .map(|_| incoming.next().expect("a register of the variable arguments"))
                .collect();
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
            let saved = self.builder.ins().stack_addr(ptr(), saved, 0);
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
                let frame = self.builder.ins().get_frame_pointer(ptr());
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
            let frame = self.builder.ins().get_frame_pointer(ptr());
            let stack = self.builder.ins().iadd_imm(frame, 16 + i64::from(named));
            self.builder.ins().store(MEM, stack, addr, 0);
            self.locals.insert(list, LocalSlot { addr, borrowed: false });
        }
        if let Some(Variadic::Buffer) = variadic {
            let list = *body.param_locals.last().expect("the list of a variadic function");
            let layout = self.layout(body.locals[list].ty.as_ref())?;
            if layout.size.bytes() != u64::from(ptr().bytes()) {
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
                let addr = self.builder.ins().load(ptr(), MEM, self.locals[receiver].addr, 0);
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
            if frame.is_none() { forwarded_temps(body) } else { FxHashMap::default() };
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
        let mut registers = Vec::new();
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
                if !self.registers.contains_key(&temp) && !self.registers.contains_key(&from) {
                    forwarded.insert(temp, from);
                }
            }
        }
        let unmentioned = unmentioned_locals(body);
        let mut shareable: Vec<(LocalId, ir::StackSlot)> = Vec::new();
        for (local, data) in body.locals.iter() {
            if self.locals.contains_idx(local)
                || forwarded.contains_key(&local)
                || self.registers.contains_key(&local)
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
                let addr = self.builder.ins().iconst(ptr(), layout.align.abi.bytes() as i64);
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
            let promoted = body.promoted_locals.iter().find(|it| it.0 == local);
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
                let addr = self.builder.ins().stack_addr(ptr(), slot, 0);
                self.locals.insert(local, LocalSlot { addr, borrowed: false });
                shareable.push((local, slot));
                continue;
            }
            let addr = if let Some(&(_, expr)) = promoted {
                let plain = self.is_plain(data.ty.as_ref());
                match self.promoted_constant(body, expr, &layout, plain) {
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
            let args: Vec<_> = body
                .param_locals
                .iter()
                .map(|&local| (self.locals[local].addr, body.locals[local].ty.as_ref()))
                .collect();
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
        let mut reachable = rustc_hash::FxHashSet::default();
        let off = self.checks_off(body);
        let resumes_only =
            if frame.is_some() { FxHashSet::default() } else { Self::resumes_only(body) };
        // Switches on a value known now, and the one way they go.
        let mut decided = FxHashMap::default();
        let mut worklist = vec![body.start_block];
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
            if block.is_cleanup {
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
        self.emit_pads();
        Ok(())
    }

    /// Field types can still be projections such as `<str as ToOwned>::Owned`.
    fn normalize(&self, ty: Ty<'db>) -> Ty<'db> {
        hir_ty::next_solver::obligation_ctxt::ObligationCtxt::new(&self.infcx)
            .structurally_normalize_ty(
                &hir_ty::next_solver::infer::traits::ObligationCause::dummy(),
                self.env.param_env,
                ty,
            )
            .unwrap_or(ty)
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
        block: &hir_ty::mir::BasicBlock,
        discr: &Operand,
        off: &FxHashSet<LocalId>,
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
        let ConstKind::Unevaluated(unevaluated) = konst.as_ref().kind() else { return None };
        let allocation = self.eval_const(unevaluated).ok()?;
        Some(*allocation.memory.first()? as u128)
    }

    /// The locals of `body` that only a call of a check that is off here assigns, once:
    /// `ub_checks()` and the like are false, and what they guard is not compiled, nor asks
    /// for the bodies of what it calls (`precondition_check`, the panics), which would else
    /// use up what a compile may build in before the code that runs is reached.
    fn checks_off(&mut self, body: &MirBody<'db>) -> FxHashSet<LocalId> {
        let db = self.cg.db;
        let mut assigned: FxHashMap<LocalId, usize> = FxHashMap::default();
        let mut borrowed = FxHashSet::default();
        let mut off = FxHashSet::default();
        for (_, block) in body.basic_blocks.iter() {
            for statement in &block.statements {
                if let StatementKind::Assign(place, rvalue) = &statement.kind {
                    *assigned.entry(place.local).or_default() += 1;
                    if let Rvalue::Ref(_, place) = rvalue {
                        borrowed.insert(place.local);
                    }
                }
            }
            let Some(TerminatorKind::Call { func, destination, .. }) =
                block.terminator.as_ref().map(|it| &it.kind)
            else {
                continue;
            };
            *assigned.entry(destination.local).or_default() += 1;
            let Ok(ty) = self.operand_ty(func) else { continue };
            let TyKind::FnDef(def, _) = ty.kind() else { continue };
            let CallableDefId::FunctionId(callee) = def.0 else { continue };
            let check = callee.krate(db).data(db).origin.is_lang()
                && matches!(
                    FunctionSignature::of(db, callee).name.as_str(),
                    "ub_checks" | "contract_checks" | "check_language_ub" | "check_library_ub"
                );
            if check && destination.projection.as_slice().is_empty() {
                off.insert(destination.local);
            }
        }
        off.retain(|local| assigned.get(local) == Some(&1) && !borrowed.contains(local));
        off
    }

    /// The cleanup blocks of `body` that do nothing but go on unwinding, which a call that
    /// unwinds to one needs no landing pad for, as rustc's `RemoveNoopLandingPads` has it: a
    /// pad costs a call that LLVM does not know resumes, and that LLVM counts against
    /// building the function into its callers.
    fn resumes_only(body: &MirBody<'db>) -> FxHashSet<BasicBlockId> {
        let mut found = FxHashSet::default();
        loop {
            let before = found.len();
            for (id, block) in body.basic_blocks.iter() {
                let idle = block.is_cleanup
                    && block.statements.iter().all(|it| {
                        matches!(
                            it.kind,
                            StatementKind::StorageLive(_)
                                | StatementKind::StorageDead(_)
                                | StatementKind::FakeRead(_)
                                | StatementKind::Nop
                        )
                    });
                let resumes = match block.terminator.as_ref().map(|it| &it.kind) {
                    Some(TerminatorKind::UnwindResume) => true,
                    Some(TerminatorKind::Goto { target }) => found.contains(target),
                    _ => false,
                };
                if idle && resumes {
                    found.insert(id);
                }
            }
            if found.len() == before {
                return found;
            }
        }
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
                self.builder.ins().stack_addr(ptr(), slot, 0)
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
        if let Some(register) = self.registers.get(&local) {
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
        self.registers.get(&place.local).copied()
    }

    fn place_ref(&mut self, place: &StoredPlace) -> Result<PlaceRef<'db>> {
        let borrowed = self.locals.get(place.local).is_some_and(|it| it.borrowed);
        // A pointer in a register is where it points with no load.
        let mut pointer = match place.projection.as_slice().first() {
            Some(ProjectionElem::Deref) => self.registers.get(&place.local).copied(),
            _ => None,
        };
        let mut addr = match pointer {
            Some(register) => self.builder.use_var(register.var),
            None => self.local_addr(place.local),
        };
        let mut meta = None;
        if borrowed {
            meta = Some(self.builder.ins().load(ptr(), MEM, addr, meta_at()));
            addr = self.builder.ins().load(ptr(), MEM, addr, 0);
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
            // A scalable value is only ever in ra-jit's storage, sized for the largest vector
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
                        addr = self.builder.ins().load(ptr(), MEM, addr, 0);
                        meta = None;
                    }
                    BackendRepr::ScalarPair { .. } if !matches!(prev.ty.kind(), TyKind::Adt(def, _) if def.is_box()) =>
                    {
                        meta = Some(self.builder.ins().load(ptr(), MEM, addr, meta_at()));
                        addr = self.builder.ins().load(ptr(), MEM, addr, 0);
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
                    let index = match self.registers.get(local) {
                        Some(register) => self.builder.use_var(register.var),
                        None => {
                            let index = self.slot_addr(self.locals[*local].addr);
                            self.builder.ins().load(ptr(), MEM, index, 0)
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
                        self.builder.ins().iconst(ptr(), *offset as i64)
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
                Ok(self.builder.ins().iconst(ptr(), len as i64))
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

    /// Calls the panic function core declares at `path` with pointer-sized `args`: the
    /// function itself, as any other call of it, or by its path when core isn't in the graph.
    fn call_panic(&mut self, path: &[&str], args: &[Value]) -> Result<()> {
        let mut sig = self.cg.make_signature();
        sig.params.extend(args.iter().map(|_| AbiParam::new(ptr())));
        let callee = match self.core_function(path) {
            Some(func) => {
                let none = GenericArgs::new_from_slice(&[]);
                self.cg.callee(InstanceDef::Fn(func), none, self.env, &sig)?
            }
            None => self.cg.import_path(&format!("core::{}", path.join("::")), &sig),
        };
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        self.invoke(callee, args);
        Ok(())
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
                if let Some(given) = &mut self.given_away {
                    given.read.push(addr);
                }
                if let OperandKind::Move(_) = operand.kind
                    && place.projection.as_slice().is_empty()
                    && self.glue_of(ty.ty) != DropGlue::None
                    && let Some(given) = &mut self.given_away
                    && given.result != place.local
                {
                    given.whole.insert(addr);
                }
                Ok((addr, ty.ty))
            }
            OperandKind::Constant { konst, ty } => {
                let ty = ty.as_ref();
                if let Some(MirSpan::ExprId(expr)) = operand.span()
                    && let Some(addr) = self.builtin_location(expr, ty)?
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
                    ConstKind::Unevaluated(unevaluated) => {
                        let allocation = self.eval_const(unevaluated)?;
                        self.allocation_blob(allocation, false)?
                    }
                    kind => unsupported!("constant {kind:?}"),
                };
                Ok((self.blob_or_immediate(ty, blob)?, ty))
            }
            OperandKind::Allocation { allocation } => {
                let allocation = allocation.as_ref();
                let ty = allocation.ty;
                if let Some(MirSpan::ExprId(expr)) = operand.span()
                    && let Some(addr) = self.builtin_location(expr, ty)?
                {
                    return Ok((addr, ty));
                }
                let blob = self.allocation_blob(allocation, false)?;
                Ok((self.blob_or_immediate(ty, blob)?, ty))
            }
            OperandKind::Static(id) => {
                let data = self.cg.static_data(*id);
                let data = self.cg.declare_data_in_func(data, self.builder.func);
                let value = if is_thread_local(self.cg.db, *id) {
                    self.builder.ins().tls_value(ptr(), data)
                } else {
                    self.builder.ins().symbol_value(ptr(), data)
                };
                let ty = self.static_ty(*id);
                let layout = self.layout(ty)?;
                let addr = self.stack_slot(&layout);
                self.builder.ins().store(MEM, value, addr, 0);
                Ok((addr, ty))
            }
        }
    }

    /// The value of a static that rustc exports no symbol for, and whether it can be written to.
    fn static_value(&mut self, id: StaticId) -> Result<(ConstBlob, bool)> {
        let db = self.cg.db;
        let flags = StaticSignature::of(db, id).flags;
        if flags.contains(StaticFlags::EXTERN) {
            unsupported!("extern static without a rustc symbol");
        }
        let allocation = db
            .const_eval_static(id)
            .map_err(|e| CodegenError::Frontend(format!("const eval: {e:?}")))?;
        // Nothing can be written to a static of no size.
        let writable = !allocation.memory.is_empty()
            && (flags.contains(StaticFlags::MUTABLE)
                || is_thread_local(db, id)
                || !self.is_freeze(allocation.ty));
        Ok((self.allocation_blob(allocation, true)?, writable))
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
        let mut found = Vec::new();
        // The body of a coroutine is resumed with a null context too, which it tells apart.
        if body.coroutine.is_some() {
            return Ok(found);
        }
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
            let mut params = Vec::new();
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
        if !element.is_none_or(|it| self.is_plain_data(it, 0)) {
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

    /// Whether `ty` is made of scalars, pointers and references only, through arrays, tuples
    /// and the fields of structs and enums, `depth` deep in a type that is.
    fn is_plain_data(&self, ty: Ty<'db>, depth: u32) -> bool {
        if depth > 8 {
            return false;
        }
        match self.normalize(ty).kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Never
            | TyKind::RawPtr(..)
            | TyKind::Ref(..)
            | TyKind::FnPtr(..)
            | TyKind::FnDef(..) => true,
            TyKind::Array(element, _) | TyKind::Pat(element, _) => {
                self.is_plain_data(element, depth + 1)
            }
            TyKind::Tuple(fields) => fields.iter().all(|it| self.is_plain_data(it, depth + 1)),
            TyKind::Adt(def, args) => {
                let variants: Vec<hir_def::VariantId> = match def.def_id() {
                    AdtId::StructId(id) => vec![id.into()],
                    AdtId::EnumId(id) => id
                        .enum_variants(self.cg.db)
                        .variants
                        .iter()
                        .map(|(_, it)| it.0.into())
                        .collect(),
                    AdtId::UnionId(_) => return false,
                };
                variants.into_iter().all(|variant| {
                    self.cg.db.field_types(variant).iter().all(|(_, field)| {
                        let ty = field.ty().instantiate(self.interner, args).skip_norm_wip();
                        self.is_plain_data(ty, depth + 1)
                    })
                })
            }
            _ => false,
        }
    }

    fn eval_const(&mut self, unevaluated: UnevaluatedConst<'db>) -> Result<Allocation<'db>> {
        let db = self.cg.db;
        let UnevaluatedConst { def, args } = unevaluated;
        let frontend = |e| CodegenError::Frontend(format!("const eval: {e:?}"));
        match def.0 {
            GeneralConstId::ConstId(id) => {
                let (id, args) = lookup_impl_const(&self.infcx, self.env.param_env, id, args);
                db.const_eval(id, args, Some(self.env)).map_err(frontend)
            }
            GeneralConstId::StaticId(id) => db.const_eval_static(id).map_err(frontend),
            GeneralConstId::AnonConstId(id) => {
                db.anon_const_eval(id, args, Some(self.env)).map_err(frontend)
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
        Ok(self.builder.ins().symbol_value(ptr(), data))
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
                let bytes = bits.to_le_bytes()[..layout.size.bytes_usize()].to_vec();
                return Ok(ConstBlob {
                    bytes,
                    relocs: Vec::new(),
                    fn_relocs: Vec::new(),
                    align,
                    type_ids: Vec::new(),
                });
            }
            TyKind::Ref(_, pointee, _) => pointee,
            TyKind::Adt(..) | TyKind::Tuple(_) | TyKind::Array(..) => {
                return self.aggregate_valtree_blob(ty, valtree);
            }
            _ => unsupported!("constant of type {ty:?}"),
        };
        let mut bytes = vec![0; layout.size.bytes_usize()];
        let target = match pointee.kind() {
            TyKind::Str | TyKind::Slice(_) => {
                let mut target = ConstBlob {
                    bytes: Vec::new(),
                    relocs: Vec::new(),
                    fn_relocs: Vec::new(),
                    align: 1,
                    type_ids: Vec::new(),
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
                bytes[word()..2 * word()]
                    .copy_from_slice(&(items.len() as u64).to_le_bytes()[..word()]);
                target
            }
            TyKind::Dynamic(..) => unsupported!("`dyn` constant"),
            _ => self.valtree_blob(pointee, valtree)?,
        };
        let target = self.cg.define_const(target)?;
        Ok(ConstBlob {
            bytes,
            relocs: vec![(0, target, 0)],
            fn_relocs: Vec::new(),
            align,
            type_ids: Vec::new(),
        })
    }

    /// A struct, enum, tuple or array that a const generic argument is (`adt_const_params`):
    /// const eval gives its fields in order, an enum's after the index of its variant.
    fn aggregate_valtree_blob(&mut self, ty: Ty<'db>, valtree: ValTree<'db>) -> Result<ConstBlob> {
        let layout = self.layout(ty)?;
        let mut blob = ConstBlob {
            bytes: vec![0; layout.size.bytes_usize()],
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align: layout.align.abi.bytes(),
            type_ids: Vec::new(),
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
            _ => (Some((layout.clone(), None)), &items[..]),
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
    /// written to.
    fn allocation_blob(
        &mut self,
        allocation: Allocation<'db>,
        in_static: bool,
    ) -> Result<ConstBlob> {
        let AllocationData { ty, memory, memory_map } = &*allocation;
        let layout = self.layout(*ty)?;
        let size = layout.size.bytes_usize();
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
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align: layout.align.abi.bytes(),
            type_ids: Vec::new(),
        };
        let mut map = Relocations {
            map: memory_map,
            in_static,
            defined: FxHashMap::default(),
            walking: Vec::new(),
        };
        self.relocate(&mut blob, 0, *ty, &mut map)?;
        Ok(blob)
    }

    /// Const eval gives pointers as keys into `map`. Emits each target as its own data object
    /// and records a relocation for the pointer, walking `ty` to find them.
    fn relocate(
        &mut self,
        blob: &mut ConstBlob,
        offset: usize,
        ty: Ty<'db>,
        map: &mut Relocations<'_, 'db>,
    ) -> Result<()> {
        if let MemoryMap::Empty = map.map {
            return Ok(());
        }
        let db = self.cg.db;
        match ty.kind() {
            TyKind::Pat(base, _) => self.relocate(blob, offset, base, map)?,
            TyKind::Ref(_, pointee, mutability) => {
                let Some(addr) = blob.usize_at(offset) else {
                    unsupported!("constant pointer out of bounds");
                };
                // Const eval gives a vtable as an id into its table of types.
                let pointee = match pointee.kind() {
                    TyKind::Dynamic(..) => {
                        let erased = blob
                            .usize_at(offset + word())
                            .and_then(|id| map.map.vtable_ty(id).ok());
                        let Some(erased) = erased else {
                            unsupported!("`dyn` constant without a known type");
                        };
                        let trait_ref = self.principal(pointee, erased);
                        let vtable = self.vtable(erased, trait_ref)?;
                        blob.point_to(offset + word(), vtable, 0);
                        erased
                    }
                    _ => pointee,
                };
                if let Some((id, at)) = map.map.static_at(addr) {
                    let target = self.cg.static_data(id);
                    blob.point_to(offset, target, at);
                    return Ok(());
                }
                let Some((start, memory)) = map.map.memory_at(addr) else { return Ok(()) };
                let elem = match pointee.kind() {
                    TyKind::Str => None,
                    TyKind::Slice(elem) => Some(elem),
                    _ => Some(pointee),
                };
                let empty = match pointee.kind() {
                    TyKind::Slice(_) => blob.usize_at(offset + word()) == Some(0),
                    _ => false,
                };
                // An empty slice can be at the address of an allocation that is not its own.
                if memory.is_empty() || empty {
                    // Nothing is behind the pointer, and rustc makes it the alignment.
                    let align = match elem {
                        Some(elem) => self.layout(elem)?.align.abi.bytes(),
                        None => 1,
                    };
                    blob.bytes[offset..offset + word()]
                        .copy_from_slice(&align.to_le_bytes()[..word()]);
                    return Ok(());
                }
                if let Some(&target) = map.defined.get(&start) {
                    blob.point_to(offset, target, addr - start);
                    return Ok(());
                }
                let mut target = ConstBlob {
                    bytes: memory.to_vec(),
                    relocs: Vec::new(),
                    fn_relocs: Vec::new(),
                    align: map.align_of(start),
                    type_ids: Vec::new(),
                };
                let at = addr - start;
                match pointee.kind() {
                    TyKind::Str => (),
                    TyKind::Slice(elem) => {
                        let Some(len) = blob.usize_at(offset + word()) else {
                            unsupported!("constant slice length out of bounds");
                        };
                        let size = self.layout(elem)?.size.bytes_usize();
                        for i in 0..len {
                            self.relocate(&mut target, at + i * size, elem, map)?;
                        }
                    }
                    _ => self.relocate(&mut target, at, pointee, map)?,
                }
                let writable = map.in_static && mutability == Mutability::Mut;
                let target = self.cg.define_data(target, writable)?;
                map.defined.insert(start, target);
                blob.point_to(offset, target, at);
            }
            // Const eval gives function pointers as ids into its table of fn item types. A
            // function cast to a raw pointer keeps that id, so a raw pointer holding one is
            // taken to be a function. FIXME: const eval should track provenance instead.
            TyKind::FnPtr(..) | TyKind::RawPtr(..) => {
                let Some(id) = blob.usize_at(offset) else {
                    unsupported!("constant pointer out of bounds");
                };
                let fn_ty = match map.map.vtable_ty(id) {
                    Ok(fn_ty)
                        if matches!(fn_ty.kind(), TyKind::FnDef(..) | TyKind::Closure(..)) =>
                    {
                        fn_ty
                    }
                    _ if let TyKind::RawPtr(_, mutability) = ty.kind() => {
                        self.relocate_data_pointer(blob, offset, id, mutability, map)?;
                        return Ok(());
                    }
                    _ if id == 0 => return Ok(()),
                    _ => unsupported!("fn pointer in a constant"),
                };
                let target = self.fn_item(fn_ty)?;
                blob.bytes[offset..offset + word()].fill(0);
                blob.fn_relocs.push((offset as u32, target));
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
                    unsupported!("a `TypeId` in a constant that const eval did not make");
                };
                blob.type_ids.push((offset as u32, of.store()));
            }
            TyKind::Adt(def, args) => {
                let layout = self.layout(ty)?;
                let (variant, offsets): (VariantId, Vec<usize>) = match def.def_id() {
                    AdtId::StructId(id) => (
                        id.into(),
                        (0..layout.fields.count())
                            .map(|i| layout.fields.offset(i).bytes_usize())
                            .collect(),
                    ),
                    // Const eval does not say which bytes of a union are a pointer, so every
                    // word that is the address of something it kept is taken to be one.
                    AdtId::UnionId(_) => {
                        let align = layout.align.abi.bytes_usize();
                        let mut may_hold_fn = None;
                        if align.is_multiple_of(word()) {
                            for index in 0..layout.size.bytes_usize() / word() {
                                let at = offset + index * word();
                                let Some(id) = blob.usize_at(at) else { continue };
                                // And every word that is the id of a function is taken to
                                // be that function, in a union that may hold one, as the
                                // one of a `LazyLock` does.
                                if may_hold_fn.get_or_insert_with(|| self.holds_fn_pointer(ty, 8))
                                    == &true
                                    && let Ok(fn_ty) = map.map.vtable_ty(id)
                                    && matches!(
                                        fn_ty.kind(),
                                        TyKind::FnDef(..) | TyKind::Closure(..)
                                    )
                                {
                                    let target = self.fn_item(fn_ty)?;
                                    blob.bytes[at..at + word()].fill(0);
                                    blob.fn_relocs.push((at as u32, target));
                                    continue;
                                }
                                self.relocate_data_pointer(blob, at, id, Mutability::Not, map)?;
                            }
                        }
                        return Ok(());
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
                        (
                            variant.into(),
                            (0..layout.fields.count())
                                .map(|i| layout.fields.offset(i).bytes_usize())
                                .collect(),
                        )
                    }
                };
                for ((_, field), field_offset) in db.field_types(variant).iter().zip(offsets) {
                    let field = field.ty().instantiate(self.interner, args).skip_norm_wip();
                    self.relocate(blob, offset + field_offset, field, map)?;
                }
            }
            TyKind::Tuple(tys) => {
                let layout = self.layout(ty)?;
                for (i, field) in tys.iter().enumerate() {
                    let field_offset = layout.fields.offset(i).bytes_usize();
                    self.relocate(blob, offset + field_offset, field, map)?;
                }
            }
            TyKind::Array(elem, len) => {
                let Some(len) = try_const_usize(db, len) else {
                    unsupported!("array with unevaluated length");
                };
                let size = self.layout(elem)?.size.bytes_usize();
                for i in 0..len as usize {
                    self.relocate(blob, offset + i * size, elem, map)?;
                }
            }
            TyKind::Alias(..) => {
                let normalized =
                    hir_ty::next_solver::obligation_ctxt::ObligationCtxt::new(&self.infcx)
                        .structurally_normalize_ty(
                            &hir_ty::next_solver::infer::traits::ObligationCause::dummy(),
                            self.env.param_env,
                            ty,
                        )
                        .unwrap_or(ty);
                if !matches!(normalized.kind(), TyKind::Alias(..)) {
                    self.relocate(blob, offset, normalized, map)?;
                }
            }
            _ => (),
        }
        Ok(())
    }

    /// Whether a value of the type has a function pointer in it, not behind a pointer, looking
    /// `depth` types deep.
    fn holds_fn_pointer(&self, ty: Ty<'db>, depth: u32) -> bool {
        if depth == 0 {
            return false;
        }
        let db = self.cg.db;
        match ty.kind() {
            TyKind::FnPtr(..) => true,
            TyKind::Tuple(tys) => tys.iter().any(|it| self.holds_fn_pointer(it, depth - 1)),
            TyKind::Array(elem, _) => self.holds_fn_pointer(elem, depth - 1),
            TyKind::Adt(def, args) => {
                let variants: Vec<VariantId> = match def.def_id() {
                    AdtId::StructId(id) => vec![id.into()],
                    AdtId::UnionId(id) => vec![id.into()],
                    AdtId::EnumId(id) => {
                        id.enum_variants(db).variants.iter().map(|it| it.1.0.into()).collect()
                    }
                };
                variants.into_iter().any(|variant| {
                    db.field_types(variant).iter().any(|(_, field)| {
                        let field = field.ty().instantiate(self.interner, args).skip_norm_wip();
                        self.holds_fn_pointer(field, depth - 1)
                    })
                })
            }
            _ => false,
        }
    }

    /// Points the word at `offset`, which const eval left holding address `addr`, at the static
    /// or the allocation that address is in.
    fn relocate_data_pointer(
        &mut self,
        blob: &mut ConstBlob,
        offset: usize,
        addr: usize,
        mutability: Mutability,
        map: &mut Relocations<'_, 'db>,
    ) -> Result<()> {
        if let Some((id, at)) = map.map.static_at(addr) {
            let target = self.cg.static_data(id);
            blob.point_to(offset, target, at);
            return Ok(());
        }
        // Const eval keeps the whole allocation a raw pointer points into.
        let Some((start, memory)) = map.map.memory_at(addr) else { return Ok(()) };
        let target = match map.defined.get(&start) {
            Some(&target) => target,
            None => {
                let mut target = ConstBlob {
                    bytes: memory.to_vec(),
                    relocs: Vec::new(),
                    fn_relocs: Vec::new(),
                    align: map.align_of(start),
                    type_ids: Vec::new(),
                };
                // What the allocation is of is not known here, so as in a union, every word
                // of it that is the address of something const eval kept is taken to be one,
                // but one of an allocation being walked: what const eval made of a constant
                // points in no circle (a static is named, not walked), so that word is a
                // number, and following it would not end.
                map.walking.push(start);
                for index in 0..target.bytes.len() / word() {
                    let at = index * word();
                    if let Some(inner) = target.usize_at(at)
                        && inner != 0
                        && map.map.memory_at(inner).is_some_and(|it| !map.walking.contains(&it.0))
                    {
                        self.relocate_data_pointer(&mut target, at, inner, mutability, map)?;
                    }
                }
                map.walking.pop();
                let writable = map.in_static && mutability == Mutability::Mut;
                let target = self.cg.define_data(target, writable)?;
                map.defined.insert(start, target);
                target
            }
        };
        blob.point_to(offset, target, addr - start);
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
    fn is_seh_throw_info(&self, block: &hir_ty::mir::InlineAsm) -> bool {
        use hir_ty::mir::AsmOperand::{Const, Reg, Sym};
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
    fn seh_throw_info(&mut self, block: &hir_ty::mir::InlineAsm, size: i128) -> Result<()> {
        use hir_ty::mir::AsmOperand;
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
            Ok(this.builder.ins().func_addr(ptr(), id))
        };
        let (cleanup, copy) = (function(self, cleanup)?, function(self, copy)?);
        let type_desc = self.cg.static_data(*type_desc);
        let type_desc = self.cg.declare_data_in_func(type_desc, self.builder.func);
        let type_desc = self.builder.ins().symbol_value(ptr(), type_desc);
        // 14 words: `_ThrowInfo` (4), `_CatchableTypeArray` (2) and `_CatchableType` (7).
        let info = self.cg.define_data(
            ConstBlob {
                bytes: vec![0; 56],
                relocs: Vec::new(),
                fn_relocs: Vec::new(),
                align: 8,
                type_ids: Vec::new(),
            },
            true,
        )?;
        let info = self.cg.declare_data_in_func(info, self.builder.func);
        let info = self.builder.ins().symbol_value(ptr(), info);
        // The image's base is what `GetModuleHandleW(NULL)` gives.
        let mut sig = self.cg.make_signature();
        sig.params.push(AbiParam::new(ptr()));
        sig.returns.push(AbiParam::new(ptr()));
        let handle = self.cg.import("GetModuleHandleW", &sig)?;
        let handle = self.cg.declare_func_in_func(handle, self.builder.func);
        let null = self.builder.ins().iconst(ptr(), 0);
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
    fn inline_asm(&mut self, block: &hir_ty::mir::InlineAsm) -> Result<()> {
        use hir_ty::mir::AsmOperand;
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
                                && let ConstKind::Unevaluated(named) = konst.as_ref().kind() =>
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
        let slot = self.builder.ins().stack_addr(ptr(), slot, 0);
        for (operand, offset) in block.operands.iter().zip(&wrapper.offsets) {
            if let (AsmOperand::Reg { input: Some(input), .. }, Some(offset)) = (operand, offset) {
                let (addr, ty) = self.operand(input)?;
                let size = self.layout(ty)?.size;
                let to = self.builder.ins().iadd_imm(slot, *offset as i64);
                self.copy(to, addr, size);
            }
        }
        let mut sig = self.cg.make_signature();
        sig.params.push(AbiParam::new(ptr()));
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
                let ConstKind::Unevaluated(named) = konst.as_ref().kind() else {
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
                // An `as` to a wider type extends by the sign of the narrower.
                let unused = 128 - self.layout(from_ty)?.size.bits() as u32;
                let value = match from_ty.kind() {
                    TyKind::Int(_) => ((value << unused) as i128 >> unused) as u128,
                    _ => value,
                };
                Ok(Some(value & mask))
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
        if let Some(MirSpan::ExprId(expr)) = operand.span()
            && self.body.is_some_and(|body| self.location_macro(body, expr).is_some())
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
            let bytes = self.builder.ins().iconst(ptr(), size.bytes() as i64);
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
        let mut words = Vec::new();
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
            self.builder.ins().store(MEM, meta, slot, meta_at());
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
                    self.builder.ins().store(MEM, meta, dst, meta_at());
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
                    self.builder.append_block_param(header, ptr());
                    let zero = self.builder.ins().iconst(ptr(), 0);
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
                let is_bool = matches!(self.operand_ty(operand)?.kind(), TyKind::Bool);
                let value = match op {
                    UnOp::Not if is_bool => self.builder.ins().bxor_imm(value, 1),
                    UnOp::Not => self.builder.ins().bnot(value),
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
                        // An `Assert` before it has ruled out `MIN`.
                        self.builder.ins().isub(zero, value)
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
                        let mut lanes = Vec::new();
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
                let pointer = self.builder.ins().func_addr(ptr(), callee);
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
                self.builder.ins().store(MEM, value, dst, 0);
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
        let (callee, callee_args) = db.lookup_impl_method(self.env, func, generic_args);
        let either::Either::Left(callee) = callee else {
            unsupported!("fn pointer to a builtin derive method");
        };
        // The function takes its caller's location as well, which the shim supplies.
        if track_caller(db, callee) {
            return self.reified(fn_ty, false);
        }
        let sig = self.interner.instantiate_bound_regions_with_erased(fn_ty.fn_sig(self.interner));
        let abi = self.fn_abi(sig.abi(), sig.output(), sig.inputs().iter().copied())?;
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
            variadic.rest(sig);
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
    ) -> Result<Option<Arc<Layout>>> {
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
    ) -> Result<Option<(Arc<Layout>, Option<(usize, types::Type, u128)>)>> {
        let db = self.cg.db;
        let adt: AdtId = variant.adt_id(db);
        let layout = self.layout(Ty::new_adt(self.interner, adt, args))?;
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
                    .map_err(|e| CodegenError::Frontend(format!("discriminant: {e:?}")))?;
                Some(discr as u128)
            }
            TagEncoding::Niche { untagged_variant, .. } if untagged_variant.0 == index => None,
            TagEncoding::Niche { niche_variants, niche_start, .. } => {
                Some(((index - niche_variants.start.0) as u128).wrapping_add(*niche_start))
            }
        };
        let layout = Arc::new(Layout::for_variant(&layout, RustcEnumVariantIdx(index)));
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
            l_addr = self.builder.ins().load(ptr(), MEM, l_addr, 0);
        }
        while let TyKind::Ref(_, inner, _) = r_ty.kind() {
            r_ty = inner;
            r_ref = Some(r_addr);
            r_addr = self.builder.ins().load(ptr(), MEM, r_addr, 0);
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
                    self.builder.ins().load(ptr(), MEM, l_ref, meta_at()),
                    self.builder.ins().load(ptr(), MEM, r_ref, meta_at()),
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
            let [l_ptr, r_ptr] =
                [l_addr, r_addr].map(|it| self.builder.ins().load(ptr(), MEM, it, 0));
            let [l_meta, r_meta] =
                [l_addr, r_addr].map(|it| self.builder.ins().load(ptr(), MEM, it, meta_at()));
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
        if let BinOp::Mul = op
            && self.builder.func.dfg.value_type(l) == types::I128
        {
            let (value, overflow) = self.wide_mul_overflow(l, r, signed)?;
            return Ok((value, overflowing.then_some(overflow)));
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
                return Ok((value, overflowing.then_some(overflow)));
            }
            // An `Assert` before it has ruled out a zero divisor and `MIN / -1`.
            BinOp::Div | BinOp::Rem => self.divide(l, r, matches!(op, BinOp::Div), signed)?,
            // The amount is taken modulo the width, as rustc's `Shl` and `Shr` do.
            BinOp::Shl | BinOp::Shr => {
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

    /// Whether arithmetic of the body being compiled panics when it overflows.
    fn checks_overflow(&self) -> Result<bool> {
        let Some(body) = self.body else { return Ok(false) };
        if !body_written_checked(self.cg.db, body) {
            return Ok(false);
        }
        Ok(self.cg.checked)
    }

    /// Branches to a call of core's `panic_const_*` when `cond` is set.
    fn panic_if(&mut self, cond: Value, op: OverflowOp) -> Result<()> {
        let name = match op {
            OverflowOp::Add => "add_overflow",
            OverflowOp::Sub => "sub_overflow",
            OverflowOp::Mul => "mul_overflow",
            OverflowOp::Div => "div_overflow",
            OverflowOp::Rem => "rem_overflow",
            OverflowOp::Neg => "neg_overflow",
            OverflowOp::Shl => "shl_overflow",
            OverflowOp::Shr => "shr_overflow",
            OverflowOp::DivByZero => "div_by_zero",
            OverflowOp::RemByZero => "rem_by_zero",
        };
        let name = format!("panic_const_{name}");
        let panic_block = self.builder.create_block();
        let ok_block = self.builder.create_block();
        self.builder.set_cold_block(panic_block);
        self.builder.ins().brif(cond, panic_block, &[], ok_block, &[]);

        self.builder.switch_to_block(panic_block);
        let location = self.caller_location()?;
        self.call_panic(&["panicking", "panic_const", &name], &[location])?;
        self.builder.ins().trap(TrapCode::user(1).expect("non-zero trap code"));

        self.builder.switch_to_block(ok_block);
        Ok(())
    }

    /// Whether `expr` is what `line!()`, `column!()` or `file!()` expanded to, and the
    /// expansion it is in.
    fn location_macro(
        &self,
        body: &MirBody<'db>,
        expr: ExprId,
    ) -> Option<(BuiltinFnLikeExpander, HirFileId)> {
        let InferBodyId::DefWithBodyId(owner) = body.owner else { return None };
        location_macros(self.cg.db, owner).get(&expr).copied()
    }

    /// Whether a promoted expression takes its value from where it is in the source.
    fn mentions_location(&self, body: &MirBody<'db>, expr: ExprId) -> bool {
        let InferBodyId::DefWithBodyId(owner) = body.owner else { return false };
        if self.location_macro(body, expr).is_some()
            || located_concats(self.cg.db, owner).contains_key(&expr)
        {
            return true;
        }
        let mut found = false;
        Body::of(self.cg.db, owner)
            .walk_child_exprs(expr, |child| found |= self.mentions_location(body, child));
        found
    }

    /// The `&str` of a literal that a `concat!` with `line!()`, `column!()` or `file!()`
    /// among its arguments expanded to: the expansion has their placeholders in it.
    fn located_concat(&mut self, expr: ExprId) -> Result<Option<Value>> {
        let db = self.cg.db;
        let Some(InferBodyId::DefWithBodyId(owner)) = self.body.map(|it| it.owner) else {
            return Ok(None);
        };
        let Some(call) = located_concats(db, owner)
            .get(&expr)
            .and_then(|file_id| file_id.macro_file())
            .map(|it| it.loc(db).to_node(db))
        else {
            return Ok(None);
        };
        let Some(parts) = concat_parts(&call.value) else {
            unsupported!("`concat!` of a place in the source and what is not a literal");
        };
        let mut text = Vec::new();
        for part in parts {
            text.push(match part {
                ConcatPart::Text(text) => TextPart::Text(text),
                ConcatPart::Location(expander, range) => {
                    let Some((file, line, col)) = location_of(db, call.file_id, range) else {
                        unsupported!("`concat!` of a place that is not in a file");
                    };
                    match expander {
                        BuiltinFnLikeExpander::Line => TextPart::Text(line.to_string()),
                        BuiltinFnLikeExpander::Column => TextPart::Text(col.to_string()),
                        _ => TextPart::FilePath(file),
                    }
                }
            });
        }
        let text = self.cg.text(text);
        let text = self.cg.declare_data_in_func(text, self.builder.func);
        Ok(Some(self.builder.ins().symbol_value(ptr(), text)))
    }

    /// The value of a literal that `line!()`, `column!()` or `file!()` expanded to.
    /// rust-analyzer expands them to placeholders, since a real value would make every body
    /// below an edit change.
    fn builtin_location(&mut self, expr: ExprId, ty: Ty<'db>) -> Result<Option<Value>> {
        let db = self.cg.db;
        if let Some(text) = self.located_concat(expr)? {
            return Ok(Some(text));
        }
        let Some((expander, file_id)) = self.body.and_then(|it| self.location_macro(it, expr))
        else {
            return Ok(None);
        };
        let Some(site) = written_call(db, file_id) else { return Ok(None) };
        let range = InFile::new(site.file_id, written_range(&site.value));
        let FileRange { file_id, range } = range.original_node_file_range_rooted(db);
        let Some((file, line, col)) = location_of(db, file_id.into(), range) else {
            return Ok(None);
        };
        let number = match expander {
            BuiltinFnLikeExpander::Line => line,
            BuiltinFnLikeExpander::Column => col,
            _ => {
                let path = self.cg.file_path(file);
                let path = self.cg.declare_data_in_func(path, self.builder.func);
                return Ok(Some(self.builder.ins().symbol_value(ptr(), path)));
            }
        };
        if !matches!(ty.kind(), TyKind::Uint(UintTy::U32)) {
            return Ok(None);
        }
        let layout = self.layout(ty)?;
        let addr = self.stack_slot(&layout);
        let value = self.builder.ins().iconst(types::I32, number as i64);
        self.builder.ins().store(MEM, value, addr, 0);
        Ok(Some(addr))
    }

    /// Tells `overlap` which of the slots of `shareable` and of the locals kept in registers
    /// may hold a value at once: those of two locals whose storage may overlap, a temporary
    /// written to a binding's slot (`forwarded`) counting as part of the binding.
    fn note_shareable(
        &mut self,
        body: &MirBody<'_>,
        shareable: &[(LocalId, ir::StackSlot)],
        forwarded: &FxHashMap<LocalId, LocalId>,
    ) {
        // A local kept in a register gets its slot later, if ever: see `local_slot`.
        let mut registers: Vec<LocalId> = self.registers.keys().copied().collect();
        registers.sort_unstable_by_key(|it| it.into_raw().into_u32());
        let count = shareable.len() + registers.len();
        if count < 2 {
            return;
        }
        let at: FxHashMap<LocalId, usize> =
            shareable.iter().enumerate().map(|(at, &(local, _))| (local, at)).collect();
        // Each local with the slot it is kept in, by the slot's index.
        let mut members: Vec<(LocalId, usize)> =
            shareable.iter().enumerate().map(|(at, &(local, _))| (local, at)).collect();
        let lazy: FxHashMap<LocalId, usize> = registers
            .iter()
            .enumerate()
            .map(|(at, &local)| (local, shareable.len() + at))
            .collect();
        members.extend(registers.iter().map(|local| (*local, lazy[local])));
        for (&temp, to) in forwarded {
            if let Some(&slot) = at.get(to) {
                members.push((temp, slot));
            }
        }
        let locals: Vec<LocalId> = members.iter().map(|it| it.0).collect();
        // A debugger reads a named local's slot all through its scope (`note_variables`), so it
        // keeps the slot to itself there, not only until its last read.
        let named: FxHashSet<LocalId> = match Self::names_are_written(body) {
            true => body.binding_locals.iter().map(|it| *it.1).collect(),
            false => FxHashSet::default(),
        };
        let storage = body.frame_storage_conflicts(&locals, |local| named.contains(&local));
        let mut conflicts = rustc_index::bit_set::BitMatrix::new(count, count);
        for (a, &(_, slot_a)) in members.iter().enumerate() {
            for (b, &(_, slot_b)) in members.iter().enumerate().skip(a + 1) {
                if slot_a != slot_b && storage.conflict(a, b) {
                    conflicts.insert(slot_a, slot_b);
                    conflicts.insert(slot_b, slot_a);
                }
            }
        }
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
            let ty = body.locals[local].ty.clone();
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
            ptr().bytes(),
            3,
        ));
        let kept = self.builder.ins().stack_addr(ptr(), slot, 0);
        self.builder.ins().store(MEM, addr, kept, 0);
        let ty = body.locals[local].ty.clone();
        self.cg.named.push(Named { binding, param: true, ty, slot, pointer: Some(0) });
    }

    /// Has what is built from here on be of the statement at `span`.
    fn at(&mut self, span: MirSpan) {
        self.span = span;
        let loc = self.cg.source_loc(span);
        self.builder.set_srcloc(loc);
    }

    /// A `core::panic::Location` (`{ file: &str, line: u32, col: u32 }`) for `#[track_caller]`.
    fn caller_location(&mut self) -> Result<Value> {
        if let Some(location) = self.location {
            return Ok(location);
        }
        let at = match self.body.map(|body| body.owner) {
            Some(InferBodyId::DefWithBodyId(owner)) => Some((owner, self.span)),
            _ => None,
        };
        let location = self.cg.location(at);
        let location = self.cg.declare_data_in_func(location, self.builder.func);
        Ok(self.builder.ins().symbol_value(ptr(), location))
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
                    Some(_) => (self.iconst(ptr(), 0), self.operand_ty(discr)?),
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
                let exception = self.builder.ins().stack_load(ptr(), ptr(), slot, 0);
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
                    if let Some(op) = op {
                        self.panic_if(failed, op)?;
                    }
                }
                self.builder.ins().jump(self.blocks[*target], &[]);
            }
            TerminatorKind::Call { func, args, destination, target, .. } => {
                self.call(func, args, destination, *target)?
            }
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
    ) -> Result<()> {
        self.given_away = Some(GivenAway {
            result: destination.local,
            whole: FxHashSet::default(),
            read: Vec::new(),
        });
        let done = self.call_with(func, args, destination, target);
        self.given_away = None;
        done
    }

    fn call_with(
        &mut self,
        func: &Operand,
        args: &[Operand],
        destination: &StoredPlace,
        target: Option<BasicBlockId>,
    ) -> Result<()> {
        let (dst, dst_ty) = self.place(destination)?;
        let fn_ty = self.operand_ty(func)?;
        if let TyKind::FnPtr(..) = fn_ty.kind() {
            let (addr, _) = self.operand(func)?;
            let pointer = self.builder.ins().load(ptr(), MEM, addr, 0);
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
        let func = match def.0 {
            CallableDefId::FunctionId(func) => func,
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
            let addr = self.builder.ins().load(ptr(), MEM, src, 0);
            let meta =
                self.is_unsized(ty).then(|| self.builder.ins().load(ptr(), MEM, src, meta_at()));
            self.drop_value(addr, meta, ty)?;
            return self.finish_call(destination, target);
        }
        if FunctionSignature::is_intrinsic(db, func)
            && FunctionSignature::of(db, func).name.as_str() == "const_eval_select"
        {
            // `const_eval_select((a, b), const_fn, runtime_fn)` is `runtime_fn(a, b)` at run time.
            let [tuple, _, runtime] = args else {
                unsupported!("const_eval_select with {} arguments", args.len());
            };
            let TyKind::FnDef(runtime, runtime_args) = self.operand_ty(runtime)?.kind() else {
                unsupported!("const_eval_select with a non-fn-item runtime function");
            };
            let CallableDefId::FunctionId(runtime) = runtime.0 else {
                unsupported!("const_eval_select with a constructor");
            };
            let (tuple_addr, tuple_ty) = self.operand(tuple)?;
            let TyKind::Tuple(fields) = tuple_ty.kind() else {
                unsupported!("const_eval_select with non-tuple arguments");
            };
            let layout = self.layout(tuple_ty)?;
            let mut values = Vec::new();
            for (i, ty) in fields.iter().enumerate() {
                let offset = layout.fields.offset(i).bytes() as i64;
                values.push((self.builder.ins().iadd_imm(tuple_addr, offset), ty));
            }
            self.call_direct(runtime, runtime_args, &values, dst, dst_ty.ty)?;
            return self.finish_call(destination, target);
        }
        // Each is `ub_checks() && ..`, and `ub_checks()` is false here: the check that asks is
        // then a branch on a constant, which `tier::fold_known_branches` takes out.
        if func.krate(db).data(db).origin.is_lang()
            && matches!(
                FunctionSignature::of(db, func).name.as_str(),
                "check_language_ub" | "check_library_ub"
            )
        {
            let value = self.builder.ins().iconst(types::I8, 0);
            self.builder.ins().store(MEM, value, dst, 0);
            return self.finish_call(destination, target);
        }
        if FunctionSignature::is_intrinsic(db, func) {
            match self.intrinsic(func, generic_args, args, dst, dst_ty.ty)? {
                Intrinsic::Inline => return self.finish_call(destination, target),
                Intrinsic::Diverges => return Ok(()),
                Intrinsic::Unknown => {
                    let overridden = AttrFlags::query(db, func.into())
                        .contains(AttrFlags::RUSTC_INTRINSIC_MUST_BE_OVERRIDDEN);
                    let signature = FunctionSignature::of(db, func);
                    // One only the const evaluator answers has nothing to run. The body of any
                    // other is what rustc runs too: `const_allocate` gives a null pointer,
                    // `const_deallocate` does nothing.
                    if overridden || compile_time_only(db, func) || !signature.has_body() {
                        unsupported!("intrinsic `{}`", signature.name.as_str());
                    }
                }
            }
        }
        if let Some(name) = link_name(self.cg.db, func)
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

    fn operands(&mut self, operands: &[Operand]) -> Result<Vec<(Value, Ty<'db>)>> {
        operands.iter().map(|it| self.argument(it)).collect()
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
        self.builder.ins().store(MEM, meta, pair, meta_at());
        Ok((pair, ty))
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
                let pointer = self.builder.ins().load(ptr(), MEM, vtable, slot as i32 * meta_at());
                let thin = self.scratch();
                self.builder.ins().store(MEM, place.addr, thin, 0);
                let thin_ty = Ty::new_imm_ptr(self.interner, Ty::new_unit(self.interner));
                let mut values = vec![(thin, thin_ty)];
                let tuple_ty = method_args.type_at(1);
                match (&self.operands(rest)?[..], tuple_ty.kind()) {
                    (&[(tuple, ty)], TyKind::Tuple(fields)) if ty == tuple_ty => {
                        let layout = self.layout(tuple_ty)?;
                        values.extend(fields.iter().enumerate().map(|(i, field)| {
                            let offset = layout.fields.offset(i).bytes() as i64;
                            (self.builder.ins().iadd_imm(tuple, offset), field)
                        }));
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
        if let (&[(tuple, ty)], TyKind::Tuple(fields)) = (&values[..], tuple_ty.kind())
            && ty == tuple_ty
        {
            let layout = self.layout(tuple_ty)?;
            values = fields
                .iter()
                .enumerate()
                .map(|(i, field)| {
                    let offset = layout.fields.offset(i).bytes() as i64;
                    (self.builder.ins().iadd_imm(tuple, offset), field)
                })
                .collect();
        }
        // The innermost reference to the callee and the address it is stored at.
        let mut by_ref = None;
        while let TyKind::Ref(_, inner, mutability) = ty.kind() {
            if self.implements_fn_itself(method, ty, tuple_ty) {
                break;
            }
            by_ref = Some((addr, ty, mutability));
            if let TyKind::Dynamic(..) = inner.kind() {
                let Some(method) = self.fn_method_of_object(inner) else {
                    unsupported!("Fn-trait call on {inner:?}");
                };
                return self.call_virtual(method, inner, addr, ty, &values, dst, dst_ty);
            }
            addr = self.builder.ins().load(ptr(), MEM, addr, 0);
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
                self.call_direct(func, generic_args, &values, dst, dst_ty)
            }
            TyKind::FnPtr(..) => {
                let pointer = self.builder.ins().load(ptr(), MEM, addr, 0);
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
                let layout = self.layout(tuple_ty)?;
                let tuple = self.stack_slot(&layout);
                for (i, &(src, ty)) in values.iter().enumerate() {
                    let size = self.layout(ty)?.size;
                    let offset = layout.fields.offset(i).bytes() as i64;
                    let field = self.builder.ins().iadd_imm(tuple, offset);
                    self.copy(field, src, size);
                }
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
        let mut fields = arguments.to_vec();
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
    /// every reference to a callable has, which call what it points to.
    fn implements_fn_itself(&self, method: FunctionId, ty: Ty<'db>, tuple_ty: Ty<'db>) -> bool {
        let db = self.cg.db;
        let args = GenericArgs::new_from_slice(&[ty.into(), tuple_ty.into()]);
        let (either::Either::Left(callee), _) = db.lookup_impl_method(self.env, method, args)
        else {
            return false;
        };
        let ItemContainerId::ImplId(impl_) = callee.lookup(db).container else {
            return false;
        };
        match db.impl_self_ty(impl_).skip_binder().kind() {
            TyKind::Ref(_, pointee, _) => !matches!(pointee.kind(), TyKind::Param(_)),
            _ => false,
        }
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
        let receiver_layout = self.layout(receiver_ty)?;
        // A `dyn Trait` taken by value is where the pair of its data pointer and vtable is
        // (`argument`), as a fat pointer to it would be; rustc hands the method the data
        // pointer as well.
        let by_value = matches!(receiver_ty.kind(), TyKind::Dynamic(..));
        if !by_value
            && (!matches!(receiver_layout.backend_repr, BackendRepr::ScalarPair { .. })
                || receiver_layout.size.bytes_usize() != 2 * word())
        {
            unsupported!("vtable call with receiver {receiver_ty:?}");
        }
        let Some(trait_ref) = self.principal(dyn_ty, dyn_ty) else {
            unsupported!("vtable call on {dyn_ty:?}");
        };
        let Some(slot) = self.vtable_layout(trait_ref).method(func) else {
            unsupported!("method missing from the vtable of {dyn_ty:?}");
        };
        let data = self.builder.ins().load(ptr(), MEM, receiver, 0);
        let vtable = self.builder.ins().load(ptr(), MEM, receiver, meta_at());
        let pointer = self.builder.ins().load(ptr(), MEM, vtable, slot as i32 * meta_at());
        // The method takes the data pointer alone where the caller holds a fat pointer.
        let thin = self.scratch();
        self.builder.ins().store(MEM, data, thin, 0);
        let thin_ty = Ty::new_imm_ptr(self.interner, Ty::new_unit(self.interner));
        let mut values = vec![(thin, thin_ty)];
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
                let old = (from.size.bytes() == 2 * word() as u64)
                    .then(|| self.builder.ins().load(ptr(), MEM, src, meta_at()));
                let meta = self.unsize_metadata(from_pointee, to_pointee, old)?;
                let ptr = self.builder.ins().load(ptr(), MEM, src, 0);
                self.builder.ins().store(MEM, ptr, dst, 0);
                self.builder.ins().store(MEM, meta, dst, meta_at());
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
                Ok(self.builder.ins().iconst(ptr(), len as i64))
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
                    Some(Upcast::Load(slot)) => {
                        Ok(self.builder.ins().load(ptr(), MEM, vtable, slot as i32 * meta_at()))
                    }
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
                Ok(self.builder.ins().symbol_value(ptr(), vtable))
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
    /// no principal trait.
    fn vtable(&mut self, ty: Ty<'db>, trait_ref: Option<TraitRef<'db>>) -> Result<DataId> {
        let key = (ty.store(), trait_ref.map(|it| (it.def_id.0, it.args.store())));
        if let Some(id) = self.cg.vtable(&key) {
            return Ok(id);
        }
        let layout = self.layout(ty)?;
        let slots = trait_ref.map_or_else(Vec::new, |it| self.vtable_layout(it).slots);
        let mut bytes = vec![0u8; (vtable::HEADER + slots.len()) * word()];
        bytes[word()..2 * word()].copy_from_slice(&layout.size.bytes().to_le_bytes()[..word()]);
        bytes[2 * word()..3 * word()]
            .copy_from_slice(&layout.align.abi.bytes().to_le_bytes()[..word()]);
        let mut blob = ConstBlob {
            bytes,
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align: word() as u64,
            type_ids: Vec::new(),
        };
        if self.glue_of(ty) != DropGlue::None {
            let mut sig = self.cg.make_signature();
            sig.params.push(AbiParam::new(ptr()));
            let args = GenericArgs::new_from_slice(&[ty.into()]);
            let glue = self.cg.callee(InstanceDef::DropGlue, args, self.env, &sig)?;
            blob.fn_relocs.push((0, glue));
        }
        for (index, slot) in slots.into_iter().enumerate() {
            let offset = ((vtable::HEADER + index) * word()) as u32;
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
        // the slot is called with the arguments one by one and, for `call_once`, a pointer to
        // the receiver, where the method takes them as a tuple and the receiver by value.
        if fn_methods.contains(&Some(func)) {
            let Some(callee_args) = self.vtable_method_args(func, trait_ref) else {
                unsupported!("generic `Fn`-trait method in a vtable");
            };
            let (callee, callee_args) = db.lookup_impl_method(self.env, func, callee_args);
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
        let (callee, callee_args) = db.lookup_impl_method(self.env, func, args);
        let either::Either::Left(callee) = callee else {
            unsupported!("builtin derive method in a vtable");
        };
        if !FunctionSignature::of(db, callee).has_body() {
            let method = FunctionSignature::of(db, callee).name.as_str();
            unsupported!("no impl of `{method}` found for the vtable of {self_ty:?}");
        }
        let body = self.body_types(InstanceDef::Fn(callee), callee_args)?;
        let Some(receiver) = body.params.first() else {
            unsupported!("vtable method without a receiver");
        };
        // A by-value `self` reaches the method through a pointer. Its shim adapts that to
        // the concrete ABI and supplies the trait declaration location for impl-only tracking.
        if receiver.as_ref() == self_ty {
            let layout = self.layout(self_ty)?;
            let mode = self.pass_mode(&layout, false)?;
            if !matches!(mode, PassMode::Indirect)
                || (track_caller(db, callee) && !track_caller(db, func))
            {
                let mut sig = Self::by_pointer(&self.body_signature(body)?, mode);
                if track_caller(db, func) {
                    sig.params.push(AbiParam::new(ptr()));
                }
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
            sig.params.push(AbiParam::new(ptr()));
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
                unsupported!("vtable shim for a closure taken by value");
            }
            let layout = self.layout(receiver_ty)?;
            let mode = self.pass_mode(&layout, false)?;
            if matches!(mode, PassMode::Indirect) {
                return self.cg.callee(closure, parent_args, self.env, &sig);
            }
            let sig = Self::by_pointer(&sig, mode);
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
        let inputs = pointer.into_iter().chain(fn_sig.inputs().iter().copied());
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
        let mut incoming = self.builder.block_params(entry).to_vec().into_iter();
        let dst_ty = fn_sig.output();
        let dst_layout = self.layout(dst_ty)?;
        let dst = match ret {
            PassMode::Indirect => incoming.next().expect("sret param"),
            _ => self.stack_slot(&dst_layout),
        };
        let receiver = if receiver { incoming.next() } else { None };
        let mut values = Vec::new();
        for (&ty, mode) in fn_sig.inputs().iter().zip(modes) {
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
        match ty.kind() {
            TyKind::FnDef(def, generic_args) => match def.0 {
                CallableDefId::FunctionId(func) => {
                    if FunctionSignature::is_intrinsic(self.cg.db, func) {
                        unsupported!("shim for an intrinsic");
                    }
                    let db = self.cg.db;
                    let (callee, _) = db.lookup_impl_method(self.env, func, generic_args);
                    if let either::Either::Left(callee) = callee
                        && track_caller(db, callee)
                    {
                        let location = self.cg.declaration(callee);
                        let location = self.cg.declare_data_in_func(location, self.builder.func);
                        self.location = Some(self.builder.ins().symbol_value(ptr(), location));
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
                let pointer = self.builder.ins().load(ptr(), MEM, receiver, 0);
                self.emit_call(Callee::Pointer(pointer, fn_sig.abi(), None), &values, dst, dst_ty)?;
            }
        }
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
            unsupported!("no `lang_start`");
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
        let (_, with_arguments) = Target::current().entry();
        let mut sig = Signature::new(self.cg.isa.default_call_conv());
        if with_arguments {
            sig.params = vec![AbiParam::new(types::I32), AbiParam::new(ptr())];
        }
        sig.returns = vec![AbiParam::new(types::I32)];
        self.builder.func.signature = sig;
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let (argc, argv) = match *self.builder.block_params(entry) {
            [argc, argv] => (argc, argv),
            [] => (self.builder.ins().iconst(types::I32, 0), self.builder.ins().iconst(ptr(), 0)),
            _ => unsupported!("`main` is called with a count and the arguments"),
        };
        // `argc` is an `isize` for `lang_start`.
        let argc = if ptr() == types::I32 { argc } else { self.builder.ins().sextend(ptr(), argc) };
        let callee = self.fn_item(main_ty)?;
        let callee = self.cg.declare_func_address_in_func(callee, self.builder.func);
        let pointer = self.builder.ins().func_addr(ptr(), callee);
        // What `SIGPIPE` does is left as the runtime has it.
        let sigpipe = self.builder.ins().iconst(types::I8, 0);
        let &[main_param, argc_ty, argv_ty, sigpipe_ty] = start_sig.inputs().as_slice() else {
            unsupported!("a `lang_start` that takes other than four arguments");
        };
        let mut values = Vec::new();
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
        let code = self.builder.ins().load(ptr(), MEM, code, 0);
        let code =
            if ptr() == types::I32 { code } else { self.builder.ins().ireduce(types::I32, code) };
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
        let mut forwarded = sig.clone();
        forwarded.params.remove(sret as usize);
        self.builder.func.signature = shim.clone();
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let params = self.builder.block_params(entry).to_vec();
        let receiver = self.builder.ins().iconst(ptr(), 1);
        let values = if forwarded.params == shim.params {
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
        let sig = self.body_signature(body)?;
        let Some(receiver) = body.params.first() else {
            unsupported!("vtable method without a receiver");
        };
        let layout = self.layout(receiver.as_ref())?;
        let mode = self.pass_mode(&layout, false)?;
        self.call_by_pointer(func, args, &sig, mode, &layout)
    }

    /// What the vtable slot for an `Fn`-trait method written for a type, whose body is
    /// `body`, is called with: the data pointer of the object, then the arguments one by one,
    /// as every call through an `Fn`-trait vtable passes them. The method takes them as the
    /// tuple this also gives.
    fn spread_shim_abi(&self, body: &BodyTypes) -> Result<(Ty<'db>, FnAbi)> {
        let [_, tuple] = &body.params[..] else {
            unsupported!("an `Fn`-trait method that takes other than a receiver and a tuple");
        };
        let tuple = tuple.as_ref();
        let TyKind::Tuple(fields) = tuple.kind() else {
            unsupported!("an `Fn`-trait method that takes {tuple:?}, not a tuple");
        };
        let thin = Ty::new_imm_ptr(self.interner, Ty::new_unit(self.interner));
        let inputs = std::iter::once(thin).chain(fields.iter());
        Ok((tuple, self.fn_abi(ExternAbi::Rust, body.returned.as_ref(), inputs)?))
    }

    /// The body of a `VtableShim` for `func`, the `Fn`-trait method `method` written for a
    /// type: puts the arguments it is handed one by one back in the tuple `func` takes, and
    /// hands `func` the receiver, which for `call_once` it has a pointer to.
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
        let mut incoming = self.builder.block_params(entry).to_vec().into_iter();
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
            true => self.builder.ins().load(ptr(), MEM, data, 0),
            false => data,
        };
        let values = [(receiver, receiver_ty), (tuple, tuple_ty)];
        self.emit_call(Callee::Instance(InstanceDef::Fn(func), args), &values, dst, dst_ty)?;
        let mut results = Vec::new();
        if !matches!(ret, PassMode::Indirect) {
            self.pass(ret, dst, &dst_layout, &mut results);
        }
        self.builder.ins().return_(&results);
        Ok(())
    }

    /// The body of a shim that is handed a pointer to the receiver of `callee`, which takes
    /// it by value as `mode` says.
    fn call_by_pointer(
        &mut self,
        callee: FunctionId,
        callee_args: GenericArgs<'db>,
        sig: &Signature,
        mode: PassMode,
        layout: &Layout,
    ) -> Result<()> {
        self.builder.func.signature = Self::by_pointer(sig, mode);
        let tracked = track_caller(self.cg.db, callee);
        let Some(method) = trait_declaration(self.cg.db, callee) else {
            unsupported!("vtable shim for a method without a trait");
        };
        let inherited = track_caller(self.cg.db, method);
        if inherited {
            self.builder.func.signature.params.push(AbiParam::new(ptr()));
        }
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let mut params = self.builder.block_params(entry).to_vec();
        let sret = sig.params.first().is_some_and(|it| it.purpose == ArgumentPurpose::StructReturn);
        let Some(&receiver) = params.get(sret as usize) else {
            unsupported!("shim for a receiver that is not passed");
        };
        let mut callee_sig = sig.clone();
        if tracked {
            callee_sig.params.push(AbiParam::new(ptr()));
            if !inherited {
                // The trait's ABI has no caller argument. The vtable adapter calls the
                // tracked implementation at the trait method's declaration.
                let location = self.cg.declaration(method);
                let location = self.cg.declare_data_in_func(location, self.builder.func);
                params.push(self.builder.ins().symbol_value(ptr(), location));
            }
        }
        let callee = self.cg.callee(InstanceDef::Fn(callee), callee_args, self.env, &callee_sig)?;
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        let mut values = Vec::new();
        self.pass(mode, receiver, layout, &mut values);
        params.splice(sret as usize..sret as usize + 1, values);
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
        self.builder.func.signature =
            if by_value { Self::by_pointer(&sig, mode) } else { sig.clone() };
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let mut params = self.builder.block_params(entry).to_vec();
        let sret = sig.params.first().is_some_and(|it| it.purpose == ArgumentPurpose::StructReturn);
        let Some(&closure) = params.get(sret as usize) else {
            unsupported!("call_once shim for a closure that is not passed");
        };
        let callee =
            self.cg.callee(InstanceDef::Closure(id.0.as_id()), parent_args, self.env, &sig)?;
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        if by_value {
            // The body owns the closure and takes it in registers.
            let mut values = Vec::new();
            self.pass(mode, closure, &layout, &mut values);
            params.splice(sret as usize..sret as usize + 1, values);
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
    fn by_pointer(sig: &Signature, mode: PassMode) -> Signature {
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
        let mut sig = sig.clone();
        sig.params.splice(sret as usize..sret as usize + count, [AbiParam::new(ptr())]);
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
        if Some(func) == self.interner.lang_items().FuturePoll
            && let Some(callee) = self.coroutine_callee(self.normalize(generic_args.type_at(0)))
        {
            return self.emit_call(callee, args, dst, dst_ty);
        }
        if is_dyn_method(self.interner, self.env.param_env, func, generic_args).is_some() {
            let Some((&(receiver, receiver_ty), rest)) = args.split_first() else {
                unsupported!("vtable call without a receiver");
            };
            let dyn_ty = generic_args.type_at(0);
            return self.call_virtual(func, dyn_ty, receiver, receiver_ty, rest, dst, dst_ty);
        }
        let (callee, callee_args) = db.lookup_impl_method(self.env, func, generic_args);
        let either::Either::Left(callee) = callee else {
            unsupported!("call to builtin derive method");
        };
        if callee == func
            && let ItemContainerId::TraitId(trait_) = func.lookup(db).container
            && Some(trait_) == self.interner.lang_items().Clone
            && FunctionSignature::of(db, func).name.as_str() == "clone"
            && let &[(receiver, _)] = args
        {
            let src = self.builder.ins().load(ptr(), MEM, receiver, 0);
            return self.clone_builtin(func, src, dst, dst_ty);
        }
        // `FnPtr::addr` is the compiler's to supply as well: a function pointer is its address.
        if callee == func
            && let ItemContainerId::TraitId(trait_) = func.lookup(db).container
            && Some(trait_) == self.interner.lang_items().FnPtrTrait
            && let &[(receiver, _)] = args
        {
            let addr = self.builder.ins().load(ptr(), MEM, receiver, 0);
            self.builder.ins().store(MEM, addr, dst, 0);
            return Ok(());
        }
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
                let len = self.builder.ins().iconst(ptr(), layout.fields.count() as i64);
                let header = self.builder.create_block();
                let body = self.builder.create_block();
                let done = self.builder.create_block();
                self.builder.append_block_param(header, ptr());
                let zero = self.builder.ins().iconst(ptr(), 0);
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
        /// `RA_JIT_INLINE=0` calls every function.
        static ENABLED: LazyLock<bool> =
            LazyLock::new(|| std::env::var_os("RA_JIT_INLINE").is_none_or(|it| it != "0"));
        let db = self.cg.db;
        if !*ENABLED {
            return None;
        }
        if Target::current().writes_wasm_objects() {
            return self.always_inlined(def, args, sig);
        }
        if !self.cg.target.exception_in_register() || Target::current().is_wasm() {
            return None;
        }
        let erased = |args| erase_regions(DbInterner::new_with(db, self.env.krate), args);
        // A closure is built in by a compile at a level above 1 alone.
        if let InstanceDef::Closure(_) = def {
            let instance = Instance { def, args: erased(args).store(), checked: self.cg.checked };
            let limit = tier::inline_limit(db, &instance)?;
            let body = tier::hot_body(db, &instance, &self.env.store(), limit, false)?;
            return (body.func.signature == *sig)
                .then(|| self.cg.declare_inlined(&body, self.builder.func));
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
        if let Some(limit) = tier::inline_limit(db, &instance)
            && let Some(body) =
                tier::hot_body(db, &instance, &self.env.store(), limit, track_caller(db, func))
            && body.func.signature == *sig
        {
            return Some(self.cg.declare_inlined(&body, self.builder.func));
        }
        if track_caller(db, func) || tier::builds_in_nothing() {
            return None;
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
        Some(self.cg.declare_inlined(body, self.builder.func))
    }

    /// The body of an `#[inline(always)]` callee of a crate no edit changes, which rustc
    /// builds into every caller at every level. What reads a wasm module counts on it:
    /// wasm-bindgen finds each of its descriptor shims by the call of its marker import,
    /// which `describe_generic_import` has to put in the shim itself.
    fn always_inlined(
        &mut self,
        def: InstanceDef,
        args: GenericArgs<'db>,
        sig: &Signature,
    ) -> Option<ir::FuncRef> {
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
        let body = inlinable(db, instance, self.env.store()).as_ref()?;
        (body.func.signature == *sig).then(|| self.cg.declare_inlined(body, self.builder.func))
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
        if let Some((body, hint)) = tier::seen_body(db, &instance, &self.env.store(), tracked) {
            if body.func.signature == *sig {
                self.cg.declare_seen(id, &body, hint);
            } else if std::env::var_os("RA_JIT_DUMP_TIER").is_some() {
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
        let FnAbi { ret: ret_mode, args: arg_modes } =
            self.fn_abi(abi, dst_ty, args.iter().map(|&(_, ty)| ty))?;
        let mut values = Vec::new();
        if let PassMode::Indirect = ret_mode {
            values.push(dst);
        }
        // The calls before this one have returned.
        self.free_argument_slots.append(&mut self.argument_slots);
        for (&(addr, ty), &mode) in args.iter().zip(&arg_modes) {
            let layout = self.layout(ty)?;
            self.pass(mode, addr, &layout, &mut values);
        }
        let mut sig = self.signature(&arg_modes, ret_mode);
        if let Callee::Instance(InstanceDef::Fn(func), _) = callee
            && track_caller(self.cg.db, func)
        {
            sig.params.push(AbiParam::new(ptr()));
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
        if Target::current().is_wasm() {
            // The arguments after the named ones are written to memory, each at its
            // own alignment, and the function is given where.
            if arg_modes.iter().any(|mode| !matches!(mode, PassMode::Direct(_))) {
                unsupported!("variadic call with an argument that is not one scalar");
            }
            let rest = values.split_off(named);
            sig.params.truncate(named);
            sig.params.push(AbiParam::new(ptr()));
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
            let buffer = self.builder.ins().stack_addr(ptr(), slot, 0);
            for (value, offset) in rest.into_iter().zip(offsets) {
                self.builder.ins().store(MEM, value, buffer, offset as i32);
            }
            values.push(buffer);
            return self.call_as(callee, sig, &values, ret_mode, dst, &ret_layout);
        }
        // Each call of a variadic function has a signature of its own, and the
        // function is declared with the one of its named parameters.
        let named = arg_modes[..named]
            .iter()
            .map(|&mode| {
                let mut params = Vec::new();
                self.push_abi_params(mode, &mut params);
                params.len()
            })
            .sum::<usize>()
            + usize::from(matches!(ret_mode, PassMode::Indirect));
        let mut pointer = match callee {
            Callee::Instance(def, generic_args) => {
                let mut declared = sig.clone();
                declared.params.truncate(named);
                if let InstanceDef::Fn(func) = def {
                    self.declare_variadic(func, &mut declared)?;
                }
                let callee = self.cg.callee(def, generic_args, self.env, &declared)?;
                let callee = self.cg.declare_func_address_in_func(callee, self.builder.func);
                self.builder.ins().func_addr(ptr(), callee)
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
        sig.params.push(AbiParam::new(ptr()));
        values.push(callee);
        let through = self.cg.assembly(asm::counted_variadic(ints, stack), sig);
        let through = self.cg.declare_func_address_in_func(through, self.builder.func);
        Ok(self.builder.ins().func_addr(ptr(), through))
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
        func: FunctionId,
        generic_args: GenericArgs<'db>,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<Intrinsic> {
        let name = FunctionSignature::of(self.cg.db, func).name.as_str();
        if self.scalable_intrinsic(name, generic_args, args, dst, dst_ty)? {
            return Ok(Intrinsic::Inline);
        }
        if self.wasm_simd_intrinsic(name, args, dst, dst_ty)? {
            return Ok(Intrinsic::Inline);
        }
        match (name, args) {
            (
                "wrapping_add" | "unchecked_add" | "wrapping_sub" | "unchecked_sub"
                | "wrapping_mul" | "unchecked_mul" | "unchecked_div" | "exact_div"
                | "unchecked_rem" | "unchecked_shl" | "unchecked_shr" | "rotate_left"
                | "rotate_right" | "saturating_add" | "saturating_sub" | "three_way_compare",
                [lhs, rhs],
            ) => {
                let (l, scalar) = self.load_scalar(lhs)?;
                let (r, r_scalar) = self.load_scalar(rhs)?;
                let signed = scalar.signed;
                let r = match name {
                    "unchecked_shl" | "unchecked_shr" => self.shift_amount_word(r).0,
                    _ => r,
                };
                let value = match name {
                    "wrapping_add" | "unchecked_add" => self.builder.ins().iadd(l, r),
                    "wrapping_sub" | "unchecked_sub" => self.builder.ins().isub(l, r),
                    "wrapping_mul" | "unchecked_mul" => self.builder.ins().imul(l, r),
                    "unchecked_div" | "exact_div" => self.divide(l, r, true, signed)?,
                    "unchecked_rem" => self.divide(l, r, false, signed)?,
                    "unchecked_shl" => self.builder.ins().ishl(l, r),
                    "unchecked_shr" if signed => self.builder.ins().sshr(l, r),
                    "unchecked_shr" => self.builder.ins().ushr(l, r),
                    "rotate_left" => self.builder.ins().rotl(l, r),
                    "rotate_right" => self.builder.ins().rotr(l, r),
                    "three_way_compare" => {
                        let (gt, lt) = if signed {
                            (IntCC::SignedGreaterThan, IntCC::SignedLessThan)
                        } else {
                            (IntCC::UnsignedGreaterThan, IntCC::UnsignedLessThan)
                        };
                        let gt = self.builder.ins().icmp(gt, l, r);
                        let lt = self.builder.ins().icmp(lt, l, r);
                        self.builder.ins().isub(gt, lt)
                    }
                    _ => self.saturating(name == "saturating_add", l, r, scalar),
                };
                let _ = r_scalar;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("add_with_overflow" | "sub_with_overflow" | "mul_with_overflow", [lhs, rhs]) => {
                let (l, scalar) = self.load_scalar(lhs)?;
                let (r, _) = self.load_scalar(rhs)?;
                let wide = name == "mul_with_overflow" && scalar.ty == types::I128;
                let ins = self.builder.ins();
                let (value, overflow) = match (name, scalar.signed) {
                    (_, signed) if wide => self.wide_mul_overflow(l, r, signed)?,
                    ("add_with_overflow", true) => ins.sadd_overflow(l, r),
                    ("add_with_overflow", false) => ins.uadd_overflow(l, r),
                    ("sub_with_overflow", true) => ins.ssub_overflow(l, r),
                    ("sub_with_overflow", false) => ins.usub_overflow(l, r),
                    (_, true) => ins.smul_overflow(l, r),
                    (_, false) => ins.umul_overflow(l, r),
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
                "ctpop" | "ctlz" | "ctlz_nonzero" | "cttz" | "cttz_nonzero" | "bswap"
                | "bitreverse",
                [arg],
            ) => {
                let (value, from) = self.load_scalar(arg)?;
                let value = match name {
                    "ctpop" => self.builder.ins().popcnt(value),
                    "ctlz" | "ctlz_nonzero" => self.builder.ins().clz(value),
                    "cttz" | "cttz_nonzero" => self.builder.ins().ctz(value),
                    "bswap" if from.ty == types::I8 => value,
                    "bswap" => self.builder.ins().bswap(value),
                    _ => self.builder.ins().bitrev(value),
                };
                let to = self.scalar_of(dst_ty)?;
                let value = self.int_cast(value, AbiScalar { signed: false, ..from }, to);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("size_of" | "align_of" | "min_align_of" | "pref_align_of", []) => {
                let ty = generic_args.type_at(0);
                // rustc's size of a scalable vector is that at the least vector length, not of
                // the storage codegen gives it.
                let layout = self
                    .cg
                    .db
                    .layout_of_ty(ty.store(), self.env.store())
                    .map_err(|e| CodegenError::Frontend(format!("layout of {ty:?}: {e:?}")))?;
                let bytes =
                    if name == "size_of" { layout.size.bytes() } else { layout.align.abi.bytes() };
                let value = self.builder.ins().iconst(ptr(), bytes as i64);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("type_name", []) => {
                let ty = generic_args.type_at(0);
                let Some(name) = hir_ty::mir::type_name::rustc_type_name(self.cg.db, ty) else {
                    unsupported!("`type_name` of `{ty:?}`");
                };
                let len = self.builder.ins().iconst(ptr(), name.len() as i64);
                let data = self.cg.define_const(ConstBlob {
                    bytes: name.into_bytes(),
                    relocs: Vec::new(),
                    fn_relocs: Vec::new(),
                    align: 1,
                    type_ids: Vec::new(),
                })?;
                let data = self.cg.declare_data_in_func(data, self.builder.func);
                let ptr = self.builder.ins().symbol_value(ptr(), data);
                self.builder.ins().store(MEM, ptr, dst, 0);
                self.builder.ins().store(MEM, len, dst, meta_at());
            }
            ("needs_drop", []) => {
                let ty = generic_args.type_at(0);
                let needs = self.glue_of(ty) != DropGlue::None;
                let value = self.builder.ins().iconst(types::I8, i64::from(needs));
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("ub_checks" | "contract_checks" | "is_val_statically_known", _) => {
                let value = self.builder.ins().iconst(types::I8, 0);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("overflow_checks", []) => {
                let value = self.builder.ins().iconst(types::I8, i64::from(self.cg.checked));
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("va_arg", [list])
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
                let after = self.builder.ins().load(ptr(), MEM, list, 0);
                let slot = self.builder.ins().iadd_imm(after, bytes - 1);
                let slot = self.builder.ins().band_imm(slot, -bytes);
                let next = self.builder.ins().iadd_imm(slot, bytes);
                self.builder.ins().store(MEM, next, list, 0);
                self.copy(dst, slot, Size::from_bytes(bytes as u64));
            }
            ("va_arg", [list])
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
                let mut slot = self.builder.ins().load(ptr(), MEM, list, 0);
                if aligned {
                    slot = self.builder.ins().iadd_imm(slot, bytes - 1);
                    slot = self.builder.ins().band_imm(slot, -bytes);
                }
                let step = if homed { 8 } else { (bytes + 7) & !7 };
                let next = self.builder.ins().iadd_imm(slot, step);
                self.builder.ins().store(MEM, next, list, 0);
                let from = match by_reference {
                    true => self.builder.ins().load(ptr(), MEM, slot, 0),
                    false => slot,
                };
                self.copy(dst, from, size);
            }
            ("va_arg", [list]) => {
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
                self.builder.append_block_param(done, ptr());
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
                    let area = self.builder.ins().load(ptr(), MEM, list, end);
                    let far = self.builder.ins().sextend(ptr(), offset);
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
                    let area = self.builder.ins().load(ptr(), MEM, list, 16);
                    let wide = self.builder.ins().uextend(ptr(), offset);
                    let from = self.builder.ins().iadd(area, wide);
                    let next = self.builder.ins().iadd_imm(offset, i64::from(step));
                    self.builder.ins().store(MEM, next, list, at);
                    self.builder.ins().jump(done, &[from.into()]);
                    8
                };

                self.builder.switch_to_block(stack);
                let from = self.builder.ins().load(ptr(), MEM, list, stack_at);
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
            ("float_to_int_unchecked", [arg]) => {
                let (value, from) = self.load_scalar(arg)?;
                let to = self.scalar_of(dst_ty)?;
                let value = self.float_cast(value, from, to)?;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("transmute" | "transmute_unchecked" | "black_box" | "likely" | "unlikely", [arg]) => {
                self.cg.opaque |= name == "black_box";
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
            ("atomic_load", [ptr]) => {
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
            ("atomic_store", [ptr, value]) => {
                let (ptr, _) = self.load_scalar(ptr)?;
                let (value, ty) = self.load_scalar(value)?;
                match self.wide_atomics(ty.ty)? {
                    true => _ = self.rmw16(AtomicRmwOp::Xchg, ptr, value)?,
                    false => _ = self.builder.ins().atomic_store(MEM, value, ptr),
                }
            }
            (
                "atomic_xchg" | "atomic_xadd" | "atomic_xsub" | "atomic_and" | "atomic_nand"
                | "atomic_or" | "atomic_xor" | "atomic_max" | "atomic_min" | "atomic_umax"
                | "atomic_umin",
                [ptr, value],
            ) => {
                let op = match name {
                    "atomic_xchg" => AtomicRmwOp::Xchg,
                    "atomic_xadd" => AtomicRmwOp::Add,
                    "atomic_xsub" => AtomicRmwOp::Sub,
                    "atomic_and" => AtomicRmwOp::And,
                    "atomic_nand" => AtomicRmwOp::Nand,
                    "atomic_or" => AtomicRmwOp::Or,
                    "atomic_xor" => AtomicRmwOp::Xor,
                    "atomic_max" => AtomicRmwOp::Smax,
                    "atomic_min" => AtomicRmwOp::Smin,
                    "atomic_umax" => AtomicRmwOp::Umax,
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
            ("atomic_cxchg" | "atomic_cxchgweak", [ptr, expected, new]) => {
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
            ("atomic_fence" | "atomic_singlethreadfence", []) => {
                self.builder.ins().fence();
            }
            ("read_via_copy" | "volatile_load" | "unaligned_volatile_load", [ptr]) => {
                let (ptr, _) = self.load_scalar(ptr)?;
                let size = self.layout(dst_ty)?.size;
                self.copy(dst, ptr, size);
            }
            ("write_via_move" | "volatile_store" | "unaligned_volatile_store", [ptr, value]) => {
                let (ptr, _) = self.load_scalar(ptr)?;
                let (src, ty) = self.operand(value)?;
                let size = self.layout(ty)?.size;
                self.copy(ptr, src, size);
            }
            ("write_box_via_move", [boxed, value]) => {
                let (boxed, boxed_ty) = self.operand(boxed)?;
                let (src, ty) = self.operand(value)?;
                let size = self.layout(ty)?.size;
                let ptr = self.builder.ins().load(ptr(), MEM, boxed, 0);
                self.copy(ptr, src, size);
                let size = self.layout(boxed_ty)?.size;
                self.copy(dst, boxed, size);
            }
            ("offset" | "arith_offset", [ptr, count]) => {
                let Some(pointee) = self.operand_ty(ptr)?.builtin_deref(true) else {
                    unsupported!("offset of a non-pointer");
                };
                let size = self.layout(pointee)?.size.bytes() as i64;
                let (ptr, _) = self.load_scalar(ptr)?;
                let (count, _) = self.load_scalar(count)?;
                let bytes = self.builder.ins().imul_imm(count, size);
                let value = self.builder.ins().iadd(ptr, bytes);
                if name == "offset" {
                    let inst = self.builder.func.dfg.value_def(value).unwrap_inst();
                    let loc = unwind::seh::in_bounds(self.builder.func.srcloc(inst));
                    self.builder.func.set_srcloc(inst, loc);
                }
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("ptr_mask", [ptr, mask]) => {
                let (ptr, _) = self.load_scalar(ptr)?;
                let (mask, _) = self.load_scalar(mask)?;
                let value = self.builder.ins().band(ptr, mask);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("ptr_offset_from" | "ptr_offset_from_unsigned", [a, b]) => {
                let size = self.layout(generic_args.type_at(0))?.size.bytes() as i64;
                let (a, _) = self.load_scalar(a)?;
                let (b, _) = self.load_scalar(b)?;
                let bytes = self.builder.ins().isub(a, b);
                let size = self.builder.ins().iconst(ptr(), size);
                let value = self.builder.ins().sdiv(bytes, size);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("copy" | "copy_nonoverlapping" | "write_bytes", [a, b, count]) => {
                let size = self.layout(generic_args.type_at(0))?.size.bytes() as i64;
                let (a, _) = self.load_scalar(a)?;
                let (b, _) = self.load_scalar(b)?;
                let (count, _) = self.load_scalar(count)?;
                let bytes = self.builder.ins().imul_imm(count, size);
                let config = self.cg.target_config();
                match name {
                    // `copy(src, dst, count)`
                    "copy" => self.builder.call_memmove(config, b, a, bytes),
                    "copy_nonoverlapping" => self.builder.call_memcpy(config, b, a, bytes),
                    // `write_bytes(dst, value, count)`
                    _ => self.builder.call_memset(config, a, b, bytes),
                }
            }
            (
                "assume"
                | "cold_path"
                | "forget"
                | "assert_inhabited"
                | "assert_zero_valid"
                | "assert_mem_uninitialized_valid",
                _,
            ) => (),
            ("unreachable" | "abort" | "abort_immediate" | "breakpoint", _) => {
                self.builder.ins().trap(TrapCode::user(2).expect("non-zero trap code"));
                return Ok(Intrinsic::Diverges);
            }
            ("raw_eq" | "compare_bytes", [a, b, ..]) => {
                let (a, _) = self.load_scalar(a)?;
                let (b, _) = self.load_scalar(b)?;
                let len = match args {
                    [_, _, len] => self.load_scalar(len)?.0,
                    _ => {
                        let size = self.layout(generic_args.type_at(0))?.size.bytes();
                        self.builder.ins().iconst(ptr(), size as i64)
                    }
                };
                let mut value = self.memcmp(a, b, len)?;
                if name == "raw_eq" {
                    value = self.builder.ins().icmp_imm(IntCC::Equal, value, 0);
                }
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("simd_fma" | "simd_relaxed_fma", _) if self.x86_fused() => {
                self.x86_fma(name, args, dst, dst_ty)?
            }
            (
                "simd_add" | "simd_sub" | "simd_mul" | "simd_div" | "simd_eq" | "simd_ge"
                | "simd_gt" | "simd_le" | "simd_lt" | "simd_neg" | "simd_fabs" | "simd_fsqrt"
                | "simd_floor" | "simd_ceil" | "simd_fma" | "simd_cast",
                _,
            ) if self.half_lanes(args, dst_ty)? => self.half_vector(name, args, dst, dst_ty)?,
            (
                "simd_and"
                | "simd_or"
                | "simd_xor"
                | "simd_add"
                | "simd_sub"
                | "simd_mul"
                | "simd_shl"
                | "simd_shr"
                | "simd_eq"
                | "simd_ne"
                | "simd_lt"
                | "simd_le"
                | "simd_gt"
                | "simd_ge"
                | "simd_div"
                | "simd_saturating_add"
                | "simd_saturating_sub",
                [lhs, rhs],
            ) => {
                let (l, ty) = self.operand(lhs)?;
                let (r, _) = self.operand(rhs)?;
                let (lane, count) = self.simd_lanes(ty)?;
                let (out, _) = self.simd_lanes(dst_ty)?;
                let signed = lane.signed;
                if lane.ty.is_float() {
                    self.simd_float_lanes(name, (l, r), dst, lane, out, count)?;
                    return Ok(Intrinsic::Inline);
                }
                if name == "simd_div" {
                    unsupported!("`simd_div` of integers");
                }
                // What x86's baseline, SSE2, has an instruction for.
                let vectors = self.makes_vectors();
                let chunks = simd_chunks(lane.ty, count).filter(|_| {
                    vectors
                        && out.ty == lane.ty
                        && match (name, signed) {
                            ("simd_and" | "simd_or" | "simd_xor" | "simd_add" | "simd_sub", _) => {
                                true
                            }
                            ("simd_eq" | "simd_ne", _) | ("simd_lt" | "simd_gt", true) => {
                                lane.ty.bits() <= 32
                            }
                            ("simd_saturating_add" | "simd_saturating_sub", _) => {
                                lane.ty.bits() <= 16
                            }
                            _ => false,
                        }
                });
                if let Some((vector, chunks)) = chunks {
                    for chunk in 0..chunks {
                        let offset = chunk * 16;
                        let a = self.builder.ins().load(vector, MEM, l, offset);
                        let b = self.builder.ins().load(vector, MEM, r, offset);
                        let ins = self.builder.ins();
                        let value = match (name, signed) {
                            ("simd_eq", _) => ins.icmp(IntCC::Equal, a, b),
                            ("simd_ne", _) => ins.icmp(IntCC::NotEqual, a, b),
                            ("simd_lt", _) => ins.icmp(IntCC::SignedLessThan, a, b),
                            ("simd_gt", _) => ins.icmp(IntCC::SignedGreaterThan, a, b),
                            ("simd_and", _) => ins.band(a, b),
                            ("simd_or", _) => ins.bor(a, b),
                            ("simd_xor", _) => ins.bxor(a, b),
                            ("simd_add", _) => ins.iadd(a, b),
                            ("simd_sub", _) => ins.isub(a, b),
                            ("simd_saturating_add", true) => ins.sadd_sat(a, b),
                            ("simd_saturating_add", false) => ins.uadd_sat(a, b),
                            ("simd_saturating_sub", true) => ins.ssub_sat(a, b),
                            _ => ins.usub_sat(a, b),
                        };
                        self.builder.ins().store(MEM, value, dst, offset);
                    }
                    return Ok(Intrinsic::Inline);
                }
                for i in 0..count {
                    let offset = (i * lane.ty.bytes() as u64) as i32;
                    let a = self.builder.ins().load(lane.ty, MEM, l, offset);
                    let b = self.builder.ins().load(lane.ty, MEM, r, offset);
                    let cc = match (name, signed) {
                        ("simd_eq", _) => Some(IntCC::Equal),
                        ("simd_ne", _) => Some(IntCC::NotEqual),
                        ("simd_lt", true) => Some(IntCC::SignedLessThan),
                        ("simd_lt", false) => Some(IntCC::UnsignedLessThan),
                        ("simd_le", true) => Some(IntCC::SignedLessThanOrEqual),
                        ("simd_le", false) => Some(IntCC::UnsignedLessThanOrEqual),
                        ("simd_gt", true) => Some(IntCC::SignedGreaterThan),
                        ("simd_gt", false) => Some(IntCC::UnsignedGreaterThan),
                        ("simd_ge", true) => Some(IntCC::SignedGreaterThanOrEqual),
                        ("simd_ge", false) => Some(IntCC::UnsignedGreaterThanOrEqual),
                        _ => None,
                    };
                    let value = match (cc, name) {
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
                        (None, "simd_and") => self.builder.ins().band(a, b),
                        (None, "simd_or") => self.builder.ins().bor(a, b),
                        (None, "simd_xor") => self.builder.ins().bxor(a, b),
                        (None, "simd_add") => self.builder.ins().iadd(a, b),
                        (None, "simd_sub") => self.builder.ins().isub(a, b),
                        (None, "simd_shl") => self.builder.ins().ishl(a, b),
                        (None, "simd_shr") if signed => self.builder.ins().sshr(a, b),
                        (None, "simd_shr") => self.builder.ins().ushr(a, b),
                        (None, "simd_saturating_add") => self.saturating(true, a, b, lane),
                        (None, "simd_saturating_sub") => self.saturating(false, a, b, lane),
                        (None, _) => self.builder.ins().imul(a, b),
                    };
                    self.builder.ins().store(MEM, value, dst, (i * out.ty.bytes() as u64) as i32);
                }
            }
            ("simd_select", [mask, on_true, on_false]) => {
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
            ("simd_select_bitmask", [mask, on_true, on_false]) => {
                let (mask, _) = self.operand(mask)?;
                let (on_true, _) = self.operand(on_true)?;
                let (on_false, _) = self.operand(on_false)?;
                let (lane, count) = self.simd_moved_lanes(dst_ty)?;
                // Lane i by bit i of the mask, an integer or bytes: its bytes in memory in order.
                let lanes: Vec<Value> = (0..count)
                    .map(|i| {
                        let offset = (i * lane.ty.bytes() as u64) as i32;
                        let byte = self.builder.ins().load(types::I8, MEM, mask, (i / 8) as i32);
                        let bit = self.builder.ins().ushr_imm(byte, (i % 8) as i64);
                        let bit = self.builder.ins().band_imm(bit, 1);
                        let a = self.builder.ins().load(lane.ty, MEM, on_true, offset);
                        let b = self.builder.ins().load(lane.ty, MEM, on_false, offset);
                        self.builder.ins().select(bit, a, b)
                    })
                    .collect();
                for (i, value) in lanes.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, i as i32 * lane.ty.bytes() as i32);
                }
            }
            ("simd_fma" | "simd_relaxed_fma", [a, b, c]) => {
                let (a, _) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                let (c, _) = self.operand(c)?;
                let (lane, count) = self.simd_lanes(dst_ty)?;
                if !lane.ty.is_float() {
                    unsupported!("`{name}` of integers");
                }
                // AArch64's LLVM makes a vector of more lanes than one `fmla z, y, x`, the
                // factors the other way round, which decides which of two NaNs comes out; a
                // vector of one lane is the scalar's `fmadd`, in order.
                let swapped = self.cg.target.arch == Arch::Aarch64 && count > 1;
                // x86-64's LLVM makes `simd_relaxed_fma` (`llvm.fmuladd`) of a CPU without FMA
                // a multiply and then an add (`mulps`, `addps`), rounded twice.
                let unfused = name == "simd_relaxed_fma" && self.cg.target.arch == Arch::X86_64;
                let lanes: Vec<Value> = (0..count)
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
                    .collect();
                for (i, value) in lanes.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, i as i32 * lane.ty.bytes() as i32);
                }
            }
            ("simd_extract", [vector, index]) => {
                let (vector, ty) = self.operand(vector)?;
                let (index, _) = self.load_scalar(index)?;
                let (lane, _) = self.simd_moved_lanes(ty)?;
                let index = self.builder.ins().uextend(ptr(), index);
                let offset = self.builder.ins().imul_imm(index, lane.ty.bytes() as i64);
                let addr = self.builder.ins().iadd(vector, offset);
                let value = self.builder.ins().load(lane.ty, MEM, addr, 0);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("simd_insert", [vector, index, value]) => {
                let (vector, ty) = self.operand(vector)?;
                let (index, _) = self.load_scalar(index)?;
                let (value, _) = self.load_scalar(value)?;
                let (lane, _) = self.simd_moved_lanes(ty)?;
                let size = self.layout(ty)?.size;
                self.copy(dst, vector, size);
                let index = self.builder.ins().uextend(ptr(), index);
                let offset = self.builder.ins().imul_imm(index, lane.ty.bytes() as i64);
                let addr = self.builder.ins().iadd(dst, offset);
                self.builder.ins().store(MEM, value, addr, 0);
            }
            ("simd_shuffle", [first, second, indices]) => {
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
                let both = self.builder.ins().stack_addr(ptr(), both, 0);
                self.copy(both, first, layout.size);
                let upper = self.builder.ins().iadd_imm(both, layout.size.bytes() as i64);
                self.copy(upper, second, layout.size);
                let mut lanes = Vec::new();
                for i in 0..count {
                    let index = self.builder.ins().load(types::I32, MEM, indices, (i * 4) as i32);
                    let index = self.builder.ins().uextend(ptr(), index);
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
            ("simd_splat", [arg]) => {
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
            ("simd_cast" | "simd_as", [arg]) => {
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
            ("simd_bitmask", [arg]) => {
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
            ("simd_reduce_and" | "simd_reduce_or" | "simd_reduce_xor", [arg]) => {
                let (src, ty) = self.operand(arg)?;
                let (lane, count) = self.simd_lanes(ty)?;
                if lane.ty.is_float() {
                    unsupported!("`{name}` of floats");
                }
                let mut acc = self.builder.ins().load(lane.ty, MEM, src, 0);
                for i in 1..count {
                    let offset = (i * lane.ty.bytes() as u64) as i32;
                    let value = self.builder.ins().load(lane.ty, MEM, src, offset);
                    acc = match name {
                        "simd_reduce_and" => self.builder.ins().band(acc, value),
                        "simd_reduce_or" => self.builder.ins().bor(acc, value),
                        _ => self.builder.ins().bxor(acc, value),
                    };
                }
                self.builder.ins().store(MEM, acc, dst, 0);
            }
            // Of floats these ignore a NaN lane, as `maxnum` does, which `fmax` does not.
            ("simd_reduce_max" | "simd_reduce_min", [arg]) => {
                let (src, ty) = self.operand(arg)?;
                let (lane, count) = self.simd_lanes(ty)?;
                if lane.ty.is_float() {
                    unsupported!("`{name}` of floats");
                }
                let mut acc = self.builder.ins().load(lane.ty, MEM, src, 0);
                for i in 1..count {
                    let offset = (i * lane.ty.bytes() as u64) as i32;
                    let value = self.builder.ins().load(lane.ty, MEM, src, offset);
                    acc = match (name == "simd_reduce_max", lane.signed) {
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
            ("simd_trunc" | "simd_round" | "simd_round_ties_even", [arg])
                if self.cg.target.arch == Arch::Aarch64 =>
            {
                let ty = self.operand_ty(arg)?;
                let layout = self.layout(ty)?;
                let lanes = match layout.backend_repr {
                    BackendRepr::SimdVector { element, count } => match element.primitive() {
                        Primitive::Float(float) => {
                            neon::Lanes::floats(float.size().bits() as u32, count.as_u64() as u32)
                        }
                        _ => unsupported!("`{name}` of integers"),
                    },
                    _ => unsupported!("vector operation on {ty:?}"),
                };
                let instruction = match name {
                    "simd_trunc" => "frintz",
                    "simd_round" => "frinta",
                    _ => "frintx",
                };
                let Some(lowered) = neon::rounding(instruction, lanes) else {
                    unsupported!("`{name}` of {ty:?}");
                };
                self.neon_assembly(name, &lowered, args, dst, dst_ty)?;
            }
            (
                "simd_ctpop" | "simd_ctlz" | "simd_cttz" | "simd_bswap" | "simd_bitreverse"
                | "simd_neg" | "simd_fabs" | "simd_fsqrt" | "simd_floor" | "simd_ceil",
                [arg],
            ) => {
                let (src, ty) = self.operand(arg)?;
                let (lane, count) = self.simd_lanes(ty)?;
                let float = lane.ty.is_float();
                match name {
                    "simd_ctpop" | "simd_ctlz" | "simd_cttz" | "simd_bswap" | "simd_bitreverse"
                        if float =>
                    {
                        unsupported!("`{name}` of floats")
                    }
                    "simd_fabs" | "simd_fsqrt" | "simd_floor" | "simd_ceil" if !float => {
                        unsupported!("`{name}` of integers")
                    }
                    // Cranelift rounds and takes square roots of 32- and 64-bit floats only.
                    "simd_fsqrt" | "simd_floor" | "simd_ceil"
                        if lane.ty != types::F32 && lane.ty != types::F64 =>
                    {
                        unsupported!("`{name}` of {:?} lanes", lane.ty)
                    }
                    _ => (),
                }
                let values: Vec<Value> = (0..count)
                    .map(|i| {
                        let offset = (i * lane.ty.bytes() as u64) as i32;
                        let value = self.builder.ins().load(lane.ty, MEM, src, offset);
                        let ins = self.builder.ins();
                        match name {
                            "simd_ctpop" => ins.popcnt(value),
                            // A lane of zero has as many leading zeros as it has bits, as
                            // LLVM's `ctlz` with `is_zero_poison` false, which rustc emits.
                            "simd_ctlz" => ins.clz(value),
                            "simd_cttz" => ins.ctz(value),
                            // A byte is its own byte swap; Cranelift swaps 16 bits and more.
                            "simd_bswap" if lane.ty.bits() == 8 => value,
                            "simd_bswap" => ins.bswap(value),
                            "simd_bitreverse" => ins.bitrev(value),
                            // Integer lanes wrap, as rustc's `sub 0, x`.
                            "simd_neg" if float => ins.fneg(value),
                            "simd_neg" => ins.ineg(value),
                            "simd_fabs" => ins.fabs(value),
                            "simd_fsqrt" => ins.sqrt(value),
                            "simd_floor" => ins.floor(value),
                            _ => ins.ceil(value),
                        }
                    })
                    .collect();
                for (i, value) in values.into_iter().enumerate() {
                    self.builder.ins().store(
                        MEM,
                        value,
                        dst,
                        (i as u64 * lane.ty.bytes() as u64) as i32,
                    );
                }
            }
            ("simd_funnel_shl" | "simd_funnel_shr", [high, low, shift]) => {
                let (high, ty) = self.operand(high)?;
                let (low, _) = self.operand(low)?;
                let (shift, _) = self.operand(shift)?;
                let (lane, count) = self.simd_lanes(ty)?;
                if lane.ty.is_float() {
                    unsupported!("`{name}` of floats");
                }
                let bits = lane.ty.bits() as i64;
                let left = name == "simd_funnel_shl";
                // Each lane is `high:low` shifted by the shift's lane modulo the lane's width,
                // as LLVM's `fshl`/`fshr` that rustc emits (a shift at or past the width is
                // UB in Rust). Cranelift shifts by the count modulo the width too, so the
                // half shifted the other way goes one bit first and then by `width - 1 - s`,
                // which is `s ^ (width - 1)`: a shift of zero leaves the other half out.
                let values: Vec<Value> = (0..count)
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
                    .collect();
                for (i, value) in values.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, i as i32 * lane.ty.bytes() as i32);
                }
            }
            ("simd_reduce_all" | "simd_reduce_any", [arg]) => {
                let (src, ty) = self.operand(arg)?;
                let (lane, count) = self.simd_lanes(ty)?;
                let all = name == "simd_reduce_all";
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
                "simd_reduce_add_ordered"
                | "simd_reduce_mul_ordered"
                | "simd_reduce_add_unordered"
                | "simd_reduce_mul_unordered",
                [arg, start @ ..],
            ) if start.len() == name.ends_with("_ordered") as usize => {
                let (src, ty) = self.operand(arg)?;
                let (lane, count) = self.simd_lanes(ty)?;
                let lane_at = |this: &mut Self, i: u64| {
                    this.builder.ins().load(lane.ty, MEM, src, (i * lane.ty.bytes() as u64) as i32)
                };
                let (mut acc, first) = match start {
                    [start] => (self.load_scalar(start)?.0, 0),
                    _ => (lane_at(self, 0), 1),
                };
                let add = name.starts_with("simd_reduce_add");
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
            ("simd_masked_load", [mask, pointer, values])
            | ("simd_masked_store", [mask, pointer, values]) => {
                let (mask, mask_ty) = self.operand(mask)?;
                let (pointer, _) = self.load_scalar(pointer)?;
                let (values, ty) = self.operand(values)?;
                let (mask_lane, count) = self.simd_lanes(mask_ty)?;
                let (lane, _) = self.simd_lanes(ty)?;
                let chosen: Vec<Value> = (0..count)
                    .map(|i| {
                        let offset = (i * mask_lane.ty.bytes() as u64) as i32;
                        let chosen = self.builder.ins().load(mask_lane.ty, MEM, mask, offset);
                        self.builder.ins().icmp_imm(IntCC::SignedLessThan, chosen, 0)
                    })
                    .collect();
                let load = name == "simd_masked_load";
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
                "fabs"
                | "fabsf32"
                | "fabsf64"
                | "sqrtf32"
                | "sqrtf64"
                | "floorf32"
                | "floorf64"
                | "ceilf32"
                | "ceilf64"
                | "truncf32"
                | "truncf64"
                | "round_ties_even_f32"
                | "round_ties_even_f64"
                | "roundf32"
                | "roundf64"
                | "sqrt"
                | "floor"
                | "ceil"
                | "trunc"
                | "round_ties_even"
                | "round",
                [arg],
            ) => {
                let (value, scalar) = self.load_scalar(arg)?;
                if !scalar.ty.is_float() {
                    unsupported!("intrinsic `{name}` on {:?}", scalar.ty);
                }
                let op = name.trim_end_matches("f32").trim_end_matches("f64").trim_end_matches('_');
                if f16_f128::soft(scalar.ty) {
                    let value = self.soft_unary(op, value)?;
                    self.builder.ins().store(MEM, value, dst, 0);
                    return Ok(Intrinsic::Inline);
                }
                let value = match op {
                    "fabs" => self.builder.ins().fabs(value),
                    "sqrt" => self.builder.ins().sqrt(value),
                    "floor" => self.builder.ins().floor(value),
                    "ceil" => self.builder.ins().ceil(value),
                    "trunc" => self.builder.ins().trunc(value),
                    "round_ties_even" => self.builder.ins().nearest(value),
                    _ => {
                        let libm = if scalar.ty == types::F32 { "roundf" } else { "round" };
                        self.float_libcall(libm, scalar.ty, &[value])?
                    }
                };
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("powif32" | "powif64" | "powi", [base, power]) => {
                let (base, scalar) = self.load_scalar(base)?;
                let (power, _) = self.load_scalar(power)?;
                if f16_f128::soft(scalar.ty) {
                    let value = self.soft_powi(base, power)?;
                    self.builder.ins().store(MEM, value, dst, 0);
                    return Ok(Intrinsic::Inline);
                }
                let builtin = if scalar.ty == types::F32 { "__powisf2" } else { "__powidf2" };
                let value =
                    self.builtin(builtin, &[scalar.ty, types::I32], scalar.ty, &[base, power])?;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (
                "copysignf32" | "copysignf64" | "copysignf16" | "copysignf128" | "minnumf32"
                | "minnumf64" | "maxnumf32" | "maxnumf64" | "powf32" | "powf64" | "fadd_fast"
                | "fsub_fast" | "fmul_fast" | "fdiv_fast" | "frem_fast" | "fadd_algebraic"
                | "fsub_algebraic" | "fmul_algebraic" | "fdiv_algebraic" | "frem_algebraic",
                [a, b],
            ) => {
                let (l, scalar) = self.load_scalar(a)?;
                let (r, _) = self.load_scalar(b)?;
                if !scalar.ty.is_float() {
                    unsupported!("intrinsic `{name}` on {:?}", scalar.ty);
                }
                if f16_f128::soft(scalar.ty) {
                    let op = match name {
                        "copysignf16" | "copysignf128" => None,
                        "fadd_fast" | "fadd_algebraic" => Some(BinOp::Add),
                        "fsub_fast" | "fsub_algebraic" => Some(BinOp::Sub),
                        "fmul_fast" | "fmul_algebraic" => Some(BinOp::Mul),
                        "fdiv_fast" | "fdiv_algebraic" => Some(BinOp::Div),
                        "frem_fast" | "frem_algebraic" => Some(BinOp::Rem),
                        _ => unsupported!("intrinsic `{name}` on {:?}", scalar.ty),
                    };
                    let value = match op {
                        Some(op) => self.soft_binary_op(&op, l, r)?,
                        None => self.soft_copysign(l, r),
                    };
                    self.builder.ins().store(MEM, value, dst, 0);
                    return Ok(Intrinsic::Inline);
                }
                let single = scalar.ty == types::F32;
                let value = match name {
                    "copysignf32" | "copysignf64" => self.builder.ins().fcopysign(l, r),
                    "minnumf32" | "minnumf64" => self.float_libcall(
                        if single { "fminf" } else { "fmin" },
                        scalar.ty,
                        &[l, r],
                    )?,
                    "maxnumf32" | "maxnumf64" => self.float_libcall(
                        if single { "fmaxf" } else { "fmax" },
                        scalar.ty,
                        &[l, r],
                    )?,
                    "powf32" | "powf64" => {
                        self.float_libcall(if single { "powf" } else { "pow" }, scalar.ty, &[l, r])?
                    }
                    "fadd_fast" | "fadd_algebraic" => self.builder.ins().fadd(l, r),
                    "fsub_fast" | "fsub_algebraic" => self.builder.ins().fsub(l, r),
                    "fmul_fast" | "fmul_algebraic" => self.builder.ins().fmul(l, r),
                    // rustc emits `frem`, which LLVM lowers to C's `fmod`, as `%` is.
                    "frem_fast" | "frem_algebraic" => {
                        if !single && scalar.ty != types::F64 {
                            unsupported!("intrinsic `{name}` on {:?}", scalar.ty);
                        }
                        self.float_binary_op(&BinOp::Rem, l, r)?
                    }
                    _ => self.builder.ins().fdiv(l, r),
                };
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("powif32" | "powif64", [base, power]) => {
                let (base, float) = self.load_scalar(base)?;
                let (power, _) = self.load_scalar(power)?;
                // compiler-builtins multiplies as LLVM's `llvm.powi` is lowered to do.
                let name = if float.ty == types::F32 { "__powisf2" } else { "__powidf2" };
                let value =
                    self.builtin(name, &[float.ty, types::I32], float.ty, &[base, power])?;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("fmaf32" | "fmaf64" | "fmuladdf32" | "fmuladdf64" | "fma", [_, _, _])
                if self.x86_fused() && self.scalar_of(dst_ty)?.ty != types::F128 =>
            {
                self.x86_fma(name, args, dst, dst_ty)?
            }
            ("fmaf32" | "fmaf64" | "fmuladdf32" | "fmuladdf64" | "fma", [a, b, c]) => {
                let (a, scalar) = self.load_scalar(a)?;
                let (b, _) = self.load_scalar(b)?;
                let (c, _) = self.load_scalar(c)?;
                if f16_f128::soft(scalar.ty) {
                    let value = self.soft_fma(a, b, c)?;
                    self.builder.ins().store(MEM, value, dst, 0);
                    return Ok(Intrinsic::Inline);
                }
                let value = if name.starts_with("fmuladd") {
                    let product = self.builder.ins().fmul(a, b);
                    self.builder.ins().fadd(product, c)
                } else {
                    let libm = if scalar.ty == types::F32 { "fmaf" } else { "fma" };
                    self.float_libcall(libm, scalar.ty, &[a, b, c])?
                };
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("discriminant_value", [arg]) => {
                // The reference may be a wide one, of a struct that ends in a slice.
                let (reference, _) = self.operand(arg)?;
                let ptr = self.builder.ins().load(ptr(), MEM, reference, 0);
                let to = self.scalar_of(dst_ty)?;
                let value = self.discriminant(ptr, generic_args.type_at(0), to)?;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("ptr_metadata", [arg]) => {
                let (src, ty) = self.operand(arg)?;
                if let BackendRepr::ScalarPair { .. } = self.layout(ty)?.backend_repr {
                    let meta = self.builder.ins().load(ptr(), MEM, src, meta_at());
                    self.builder.ins().store(MEM, meta, dst, 0);
                }
            }
            ("aggregate_raw_ptr", [data, meta]) => {
                let (data, _) = self.operand(data)?;
                let data = self.builder.ins().load(ptr(), MEM, data, 0);
                self.builder.ins().store(MEM, data, dst, 0);
                if let BackendRepr::ScalarPair { .. } = self.layout(dst_ty)?.backend_repr {
                    let (meta, _) = self.load_scalar(meta)?;
                    self.builder.ins().store(MEM, meta, dst, meta_at());
                }
            }
            ("slice_get_unchecked", [slice, index]) => {
                let size = self.layout(generic_args.type_at(2))?.size.bytes() as i64;
                let (slice, _) = self.operand(slice)?;
                let ptr = self.builder.ins().load(ptr(), MEM, slice, 0);
                let (index, _) = self.load_scalar(index)?;
                let offset = self.builder.ins().imul_imm(index, size);
                let value = self.builder.ins().iadd(ptr, offset);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("vtable_size" | "vtable_align", [vtable]) => {
                let (vtable, _) = self.load_scalar(vtable)?;
                let offset = if name == "vtable_size" { meta_at() } else { 2 * meta_at() };
                let value = self.builder.ins().load(ptr(), MEM, vtable, offset);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("size_of_val" | "align_of_val", [arg]) => {
                let (src, _) = self.operand(arg)?;
                let ty = generic_args.type_at(0);
                let meta = self
                    .is_unsized(ty)
                    .then(|| self.builder.ins().load(ptr(), MEM, src, meta_at()));
                let (size, align) = self.size_and_align_of_val(ty, meta)?;
                let value = if name == "size_of_val" { size } else { align };
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("catch_unwind", [try_fn, data, catch_fn]) => {
                let (try_fn, _) = self.load_scalar(try_fn)?;
                let (data, _) = self.load_scalar(data)?;
                let (catch_fn, _) = self.load_scalar(catch_fn)?;
                let mut sig = self.cg.make_signature();
                sig.params.push(AbiParam::new(ptr()));
                let try_sig = self.builder.import_signature(sig.clone());
                sig.params.push(AbiParam::new(ptr()));
                let catch_sig = self.builder.import_signature(sig);

                // Whether it unwound is an `i32` or a `bool`, by the toolchain.
                let outcome = self.layout(dst_ty)?.size.bytes();
                let outcome = Type::int_with_byte_size(outcome as u16).ok_or_else(|| {
                    CodegenError::Unsupported(format!("`catch_unwind` giving {outcome} bytes"))
                })?;
                if self.aborts {
                    // Nothing unwinds to be caught, as rustc has it.
                    self.builder.ins().call_indirect(try_sig, try_fn, &[data]);
                    let zero = self.builder.ins().iconst(outcome, 0);
                    self.builder.ins().store(MEM, zero, dst, 0);
                    return Ok(Intrinsic::Inline);
                }
                let returned = self.builder.create_block();
                let caught = self.builder.create_block();
                let done = self.builder.create_block();
                self.builder.append_block_param(caught, ptr());
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
                    sig.params.push(AbiParam::new(ptr()));
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
            ("caller_location", []) => {
                let value = self.caller_location()?;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            _ => return Ok(Intrinsic::Unknown),
        }
        Ok(Intrinsic::Inline)
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
        let values: Vec<Value> = (0..lanes as i32)
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
            .collect();
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
        sig.params.extend([AbiParam::new(ptr()); 3]);
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
        let parts: Vec<&str> = parts.collect();
        let Some((lanes, ty)) = parts.last().and_then(|it| {
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
                let values: Vec<Value> = (0..lanes)
                    .map(|lane| {
                        let from = if lane < half { a } else { b };
                        let at = (lane % half) * 2 * width;
                        let l = self.builder.ins().load(ty, MEM, from, at);
                        let r = self.builder.ins().load(ty, MEM, from, at + width);
                        combine(&mut self.builder, l, r)
                    })
                    .collect();
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
                let values: Vec<Value> = (0..lanes)
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
                    .collect();
                for (lane, value) in values.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, lane as i32 * width);
                }
            }
            ("uaddlp" | "saddlp", [a]) if ty.bits() <= 32 => {
                let (a, _) = self.operand(a)?;
                let wide = ty.double_width().expect("a lane of at most 32 bits");
                // Each lane twice as wide is the sum of a pair of `a`'s.
                let values: Vec<Value> = (0..lanes / 2)
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
                    .collect();
                for (pair, value) in values.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, pair as i32 * 2 * width);
                }
            }
            ("uaddlv" | "saddlv" | "uaddv" | "saddv", [a]) => {
                let Some(sum_ty) =
                    parts.first().and_then(|it| Type::int(it.strip_prefix('i')?.parse().ok()?))
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
                let values: Vec<Value> = (0..lanes)
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
                    .collect();
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
                let values: Vec<Value> = (0..lanes)
                    .map(|lane| {
                        let index = self.builder.ins().load(types::I8, MEM, indices, lane);
                        let slot = self.builder.ins().band_imm(index, 0x0f);
                        let slot = self.builder.ins().uextend(ptr(), slot);
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
                    .collect();
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
        lowered: &neon::Lowered,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        let mut args: Vec<&Operand> = args.iter().collect();
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
            let text = lowered.text.replace("{imm}", &value.to_string());
            with_immediate = Some(neon::Lowered { text, ..lowered.clone() });
        }
        let args = args
            .into_iter()
            .map(|arg| Ok((self.operand(arg)?.0, self.operand_ty(arg)?)))
            .collect::<Result<Vec<_>>>()?;
        self.neon_call(name, with_immediate.as_ref().unwrap_or(lowered), &args, dst, dst_ty)
    }

    /// The lane each lane of a constant vector operand is, as an immediate of lanes of
    /// `lane` bytes: `core::arch` passes a shift so (`const { int8x8_t([N as i8; 8]) }`).
    /// It comes as a constant, or a local assigned the vector (`int8x8_t(lanes)`) of an array
    /// of the lane repeated (`[N as i8; 8]`).
    fn splat_immediate(&mut self, operand: &Operand, lane: u32) -> Result<Option<u128>> {
        let memory = match &operand.kind {
            OperandKind::Allocation { allocation } => allocation.as_ref().memory.to_vec(),
            OperandKind::Constant { konst, ty } => match konst.as_ref().kind() {
                ConstKind::Value(value) => self.valtree_blob(ty.as_ref(), value.value)?.bytes,
                ConstKind::Unevaluated(named) => self.eval_const(named)?.memory.to_vec(),
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
        let mut values = vec![dst];
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
        let results: Vec<(u32, u32)> = match lowered.results.as_slice() {
            [] if layout.is_zst() => Vec::new(),
            &[bytes] if layout.size.bytes() == u64::from(bytes) => vec![(0, bytes)],
            many if layout.size.bytes() == size
                && layout.fields.count() == many.len()
                && (0..many.len())
                    .all(|i| layout.fields.offset(i).bytes() == i as u64 * u64::from(many[0])) =>
            {
                many.iter().enumerate().map(|(i, &bytes)| (i as u32 * bytes, bytes)).collect()
            }
            _ => unsupported!("`{name}` returning {dst_ty:?}, which is not what its name says"),
        };
        let directives = asm::extensions(Arch::Aarch64, lowered.features.iter().copied());
        let text = asm::neon_function(&directives, &lowered.args, &lowered.text, &results);
        let mut sig = self.cg.make_signature();
        sig.params.extend(values.iter().map(|_| AbiParam::new(ptr())));
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
        let features = func.map(|func| AttrFlags::target_features(self.cg.db, func));
        let features: Vec<&str> = features.into_iter().flatten().map(|it| it.as_str()).collect();
        let lowered = match avx512::lower(name, &given, layout.size.bytes() as u32, &features) {
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
        let addrs: Vec<Option<Value>> = addrs.iter().copied().map(Some).collect();
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
        let slot = self.builder.ins().stack_addr(ptr(), slot, 0);
        self.builder.ins().store(MEM, dst, slot, 0);
        for (i, addr) in addrs.iter().enumerate() {
            if let Some(addr) = *addr {
                self.builder.ins().store(MEM, addr, slot, 8 * (i as i32 + 1));
            }
        }
        let text = asm::avx512_function(&lowered.loads, &lowered.text, lowered.out);
        let mut sig = self.cg.make_signature();
        sig.params.push(AbiParam::new(ptr()));
        let callee = self.cg.assembly(text, &sig);
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        self.builder.ins().call(callee, &[slot]);
    }

    /// Calls the instructions `x86::lower` made of the intrinsic `name`: the arguments are
    /// copied to a slot the function reads its registers from, and it writes each result
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
            unsupported!("`{name}` with {} arguments", args.len());
        }
        let mut text = lowered.text.clone();
        let mut reads = Vec::new();
        let mut copies = Vec::new();
        let mut size = 0;
        for (arg, register) in args.iter().zip(&lowered.args) {
            let Some(register) = register else {
                let Some(immediate) = self.immediate(arg)?.filter(|it| *it <= 0xff) else {
                    unsupported!("`{name}` with an immediate that is not a constant byte");
                };
                text = text.replace("{imm}", &immediate.to_string());
                continue;
            };
            let bytes = asm::x86_register_bytes(register).expect("a register of x86");
            let ty = self.operand_ty(arg)?;
            if self.layout(ty)?.size.bytes() != u64::from(bytes) {
                unsupported!("`{name}` of {ty:?}, which is not what its name says");
            }
            copies.push((self.operand(arg)?.0, size, bytes));
            reads.push((*register, size));
            size += bytes;
        }
        let layout = self.layout(dst_ty)?;
        let register_bytes = |it: &str| u64::from(asm::x86_register_bytes(it).expect("x86's"));
        let writes: Vec<(&str, u32)> = match lowered.results.as_slice() {
            [] if layout.is_zst() => Vec::new(),
            &[register] if layout.size.bytes() == register_bytes(register) => vec![(register, 0)],
            many => {
                let fields: Vec<Ty<'db>> = match dst_ty.kind() {
                    TyKind::Tuple(tys) => tys.iter().collect(),
                    TyKind::Adt(adt_def, subst) if let AdtId::StructId(id) = adt_def.def_id() => {
                        let field_types = self.cg.db.field_types(id.into());
                        field_types
                            .iter()
                            .map(|(_, it)| {
                                let ty = it.ty().instantiate(self.interner, subst).skip_norm_wip();
                                self.normalize(ty)
                            })
                            .collect()
                    }
                    _ => Vec::new(),
                };
                let mut writes = Vec::new();
                for (i, (&register, &field)) in many.iter().zip(&fields).enumerate() {
                    if self.layout(field)?.size.bytes() != register_bytes(register) {
                        break;
                    }
                    writes.push((register, layout.fields.offset(i).bytes() as u32));
                }
                if fields.len() != many.len() || writes.len() != many.len() {
                    unsupported!("`{name}` returning {dst_ty:?}, which is not what its name says");
                }
                writes
            }
        };
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size.max(1),
            4,
        ));
        let slot = self.builder.ins().stack_addr(ptr(), slot, 0);
        for (from, at, bytes) in copies {
            let to = self.builder.ins().iadd_imm(slot, i64::from(at));
            self.copy(to, from, Size::from_bytes(bytes));
        }
        let text = asm::x86_function(&text, &reads, &writes, &lowered.clobbers)
            .map_err(CodegenError::Unsupported)?;
        let mut sig = self.cg.make_signature();
        sig.params.extend([AbiParam::new(ptr()); 2]);
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
            && self.neon_lanes(neon, args, dst)?
        {
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
                    let slot = self.builder.ins().uextend(ptr(), slot);
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
                let values: Vec<Value> = (0..lanes)
                    .map(|lane| {
                        let at = (lane * bytes) as i32;
                        let index = self.builder.ins().load(width, MEM, indices, at);
                        let slot = self.builder.ins().band_imm(index, lanes - 1);
                        let slot = match width == ptr() {
                            true => slot,
                            false => self.builder.ins().uextend(ptr(), slot),
                        };
                        let offset = self.builder.ins().imul_imm(slot, bytes);
                        let addr = self.builder.ins().iadd(table, offset);
                        self.builder.ins().load(width, MEM, addr, 0)
                    })
                    .collect();
                for (lane, value) in values.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, lane as i32 * bytes as i32);
                }
            }
            ("x86.avx2.permd" | "x86.avx2.permps", [table, indices]) => {
                let (table, _) = self.operand(table)?;
                let (indices, _) = self.operand(indices)?;
                // Each of the eight lanes is the lane its index names, by its low three bits.
                let lanes: Vec<Value> = (0..8)
                    .map(|lane| {
                        let index = self.builder.ins().load(types::I32, MEM, indices, lane * 4);
                        let slot = self.builder.ins().band_imm(index, 7);
                        let slot = self.builder.ins().uextend(ptr(), slot);
                        let offset = self.builder.ins().ishl_imm(slot, 2);
                        let addr = self.builder.ins().iadd(table, offset);
                        self.builder.ins().load(types::I32, MEM, addr, 0)
                    })
                    .collect();
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
                let instruction = format!("{} xmm0, xmm1", name.trim_start_matches("x86."));
                self.vector_assembly(asm::vector_instruction(&instruction, "xmm0"), a, b, dst)?;
            }
            ("x86.sha256rnds2", [a, b, k]) => {
                let (k, ty) = self.operand(k)?;
                let layout = self.layout(ty)?;
                self.copy_value(dst, k, &layout);
                // The instruction takes its third vector in `xmm0`.
                let rounds = "movdqa xmm3, xmm0\nmovdqa xmm0, xmm2\nsha256rnds2 xmm3, xmm1";
                self.vector_assembly(asm::vector_instruction(rounds, "xmm3"), a, b, dst)?;
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
                self.vector_assembly(asm::sha1rnds4(), a, b, dst)?;
            }
            ("x86.xgetbv", [register]) => {
                let (register, _) = self.load_scalar(register)?;
                let mut sig = self.cg.make_signature();
                sig.params.push(AbiParam::new(types::I32));
                sig.returns.push(AbiParam::new(types::I64));
                let callee = self.cg.assembly(asm::xgetbv(), &sig);
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
                let callee = self.cg.assembly(asm::x86_crc32(scalar.ty.bits()), &sig);
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
                sig.params.push(AbiParam::new(ptr()));
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
                sig.params.extend([AbiParam::new(ptr()), AbiParam::new(ptr())]);
                sig.params.push(AbiParam::new(types::I32));
                let register = if name.ends_with("256") { "ymm0" } else { "xmm0" };
                let callee = self.cg.assembly(asm::vcvtps2ph(register), &sig);
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
                    let chosen = fx.builder.ins().uextend(ptr(), chosen);
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
                let values: Vec<Value> = (0..lanes)
                    .map(|lane| {
                        let index =
                            self.builder.ins().load(int, MEM, indices, (lane * width) as i32);
                        let index = self.builder.ins().band_imm(index, 2 * lanes - 1);
                        let index = if width < 8 {
                            self.builder.ins().uextend(ptr(), index)
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
                    .collect();
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
            // one of ra-jit's instructions to the next.
            ("x86.avx.vzeroupper", []) => {}
            // A hint to the cache, which the program behaves the same without.
            ("prefetch.p0", [_address, _write, _locality, _cache]) => {}
            _ if self.cg.target.arch == Arch::Aarch64
                && let Some((mut lowered, max_shift)) = neon::narrowing_shift(name) =>
            {
                let [value, shift] = args else {
                    unsupported!("`{name}` with {} arguments", args.len());
                };
                let Some(shift) =
                    self.immediate(shift)?.filter(|it| (1..=u128::from(max_shift)).contains(it))
                else {
                    unsupported!("`{name}` with a shift that is not a constant in 1..={max_shift}");
                };
                lowered.text = lowered.text.replace("{shift}", &shift.to_string());
                self.neon_assembly(
                    &format!("llvm.{name}"),
                    &lowered,
                    std::slice::from_ref(value),
                    dst,
                    dst_ty,
                )?;
            }
            _ if self.cg.target.arch == Arch::Aarch64
                && let Some(lowered) = neon::lower(name) =>
            {
                self.neon_assembly(&format!("llvm.{name}"), &lowered, args, dst, dst_ty)?
            }
            _ if self.cg.target.arch == Arch::X86_64
                && [
                    "x86.avx512.",
                    "x86.avx512fp16.",
                    "x86.avx512bf16.",
                    "x86.avx2.vpdp",
                    "x86.vcvtne",
                    "x86.vbcstne",
                ]
                .iter()
                .any(|it| name.starts_with(it))
                && self.avx512_assembly(name, args, dst, dst_ty)? => {}
            _ if self.cg.target.arch == Arch::X86_64
                && let Some(lowered) = x86::lower(name) =>
            {
                self.x86_assembly(&format!("llvm.{name}"), &lowered, args, dst, dst_ty)?
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
        name: &str,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        let Some(first) = args.first() else {
            unsupported!("`{name}` without operands");
        };
        let ty = self.operand_ty(first)?;
        let (a, a_signed) = self.neon_lanes_of(ty)?;
        let (r, r_signed) = self.neon_lanes_of(dst_ty)?;
        let widths = (r.float && a.float).then_some((r.bits, a.bits));
        if self.cg.target.arch == Arch::X86_64 {
            return self.x86_half_vector(name, args, dst, (a, r));
        }
        let single_and_half = name == "simd_cast" && matches!(widths, Some((16, 32) | (32, 16)));
        if self.cg.target.arch != Arch::Aarch64 || !(single_and_half || self.half_precision()) {
            unsupported!(
                "vector operation on lanes of f16 without the half-precision instructions"
            );
        }
        let signed = if a.float { r_signed } else { a_signed };
        let Some(lowered) = neon::half_vector(name, r, a, signed) else {
            unsupported!("`{name}` of lanes of f16 into {dst_ty:?}");
        };
        self.neon_assembly(name, &lowered, args, dst, dst_ty)
    }

    /// A generic intrinsic on a vector of `f16` on x86-64 (`a` its operand's lanes, `r` its
    /// result's), as LLVM selects it in the function: by AVX512-FP16's instructions where the
    /// function has those, `simd_cast` to single floats by F16C's where it has only those.
    /// Each instruction is `avx512::half_vector`'s.
    fn x86_half_vector(
        &mut self,
        name: &str,
        args: &[Operand],
        dst: Value,
        (a, r): (neon::Lanes, neon::Lanes),
    ) -> Result<()> {
        if name == "simd_neg" && self.function_enables("avx512fp16") && a.float && a.bits == 16 {
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
        let lowered = avx512::half_vector(name, a.count * a.bits / 8, r.count * r.bits / 8, |it| {
            self.function_enables(it)
        });
        let Some(lowered) = lowered.filter(|it| it.loads.len() == args.len()) else {
            unsupported!("`{name}` of lanes of f16 on x86-64 without an instruction for it here");
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
        sig.params
            .extend(params.iter().map(|&ty| AbiParam::new(if wide(ty) { ptr() } else { ty })));
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
        let outline = cfg::CfgAtom::KeyValue {
            key: intern::sym::target_feature,
            value: intern::Symbol::intern("outline-atomics"),
        };
        if !self.env.krate.cfg_options(self.cg.db).check_atom(&outline) {
            unsupported!("an atomic of 128 bits on AArch64 without outline atomics");
        }
        Ok(true)
    }

    /// What `ptr` held, swapped for `new` when it was `expected`: compiler_builtins'
    /// compare-and-swap of 128 bits ([`Self::wide_atomics`]).
    fn cas16(&mut self, ptr: Value, expected: Value, new: Value) -> Result<Value> {
        let params = [types::I128, types::I128, self::ptr()];
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
        let params = [types::I128, types::I128, ptr()];
        let value = self.builtin(name, &params, types::I128, &[l, r, overflow])?;
        let overflow = self.builder.ins().load(types::I32, MEM, overflow, 0);
        let overflow = self.builder.ins().icmp_imm(IntCC::NotEqual, overflow, 0);
        Ok((value, overflow))
    }

    fn memcmp(&mut self, a: Value, b: Value, len: Value) -> Result<Value> {
        let mut sig = self.cg.make_signature();
        sig.params.extend([AbiParam::new(ptr()); 3]);
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
        name: &str,
        (l, r): (Value, Value),
        dst: Value,
        lane: AbiScalar,
        out: AbiScalar,
        count: u64,
    ) -> Result<()> {
        let cc = match name {
            "simd_eq" => Some(FloatCC::Equal),
            "simd_ne" => Some(FloatCC::NotEqual),
            "simd_lt" => Some(FloatCC::LessThan),
            "simd_le" => Some(FloatCC::LessThanOrEqual),
            "simd_gt" => Some(FloatCC::GreaterThan),
            "simd_ge" => Some(FloatCC::GreaterThanOrEqual),
            _ => None,
        };
        for i in 0..count {
            let offset = (i * lane.ty.bytes() as u64) as i32;
            let a = self.builder.ins().load(lane.ty, MEM, l, offset);
            let b = self.builder.ins().load(lane.ty, MEM, r, offset);
            let value = match (cc, name) {
                (Some(cc), _) => {
                    let flag = self.builder.ins().fcmp(cc, a, b);
                    let flag = self.builder.ins().uextend(types::I64, flag);
                    let mask = self.builder.ins().ineg(flag);
                    if out.ty.bits() < 64 { self.builder.ins().ireduce(out.ty, mask) } else { mask }
                }
                (None, "simd_add") => self.builder.ins().fadd(a, b),
                (None, "simd_sub") => self.builder.ins().fsub(a, b),
                (None, "simd_mul") => self.builder.ins().fmul(a, b),
                (None, "simd_div") => self.builder.ins().fdiv(a, b),
                (None, _) => unsupported!("`{name}` of floats"),
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
            return self.builder.inst_results(call).to_vec();
        };
        if !self.cg.target.exception_in_register() {
            return self.guarded_call(pad, |builder| builder.ins().call(callee, args));
        }
        let sig = self.builder.func.dfg.ext_funcs[callee].signature;
        let (table, after) = self.exception_table(sig, pad);
        self.builder.ins().try_call(callee, args, table);
        self.builder.switch_to_block(after);
        self.builder.block_params(after).to_vec()
    }

    fn invoke_indirect(&mut self, sig: ir::SigRef, pointer: Value, args: &[Value]) -> Vec<Value> {
        let Some(pad) = self.landing_pad_for_call() else {
            let call = self.builder.ins().call_indirect(sig, pointer, args);
            return self.builder.inst_results(call).to_vec();
        };
        if !self.cg.target.exception_in_register() {
            return self
                .guarded_call(pad, |builder| builder.ins().call_indirect(sig, pointer, args));
        }
        let (table, after) = self.exception_table(sig, pad);
        self.builder.ins().try_call_indirect(pointer, args, table);
        self.builder.switch_to_block(after);
        self.builder.block_params(after).to_vec()
    }

    /// Turns down a call that can unwind in code that cannot: rustc ends the program when
    /// it does (`AbortUnwindingCalls`).
    fn check_abort_unwinding(&self, abi: ExternAbi) -> Result<()> {
        if self.aborts && abi.as_str().ends_with("-unwind") {
            unsupported!("a call of an `extern \"{}\"` function with `panic = \"abort\"`", abi);
        }
        Ok(())
    }

    /// The block that cleans up after a call that unwinds, if there is anything to clean up.
    fn landing_pad_for_call(&mut self) -> Option<ir::Block> {
        if self.in_cleanup || self.aborts {
            return None;
        }
        if self.inner_pad.is_some() {
            return self.inner_pad;
        }
        let target = self.unwind_to?;
        if let Some(&(_, pad)) = self.pads.iter().find(|(it, _)| *it == target) {
            return Some(pad);
        }
        let pad = self.builder.create_block();
        self.builder.append_block_param(pad, ptr());
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
        let returns: Vec<_> =
            self.builder.func.dfg.signatures[sig].returns.iter().map(|it| it.value_type).collect();
        let mut results = Vec::new();
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
        let results = self.builder.inst_results(call).to_vec();
        let frame = self.builder.ins().get_frame_pointer(ptr());
        let exception = self.builder.ins().load(ptr(), MEM, frame, unwind::seh::ARRIVAL);
        let (unwound, returned) = (self.builder.create_block(), self.builder.create_block());
        self.builder.set_cold_block(unwound);
        self.builder.ins().brif(exception, unwound, &[], returned, &[]);
        self.builder.switch_to_block(unwound);
        let none = self.builder.ins().iconst(ptr(), 0);
        self.builder.ins().store(MEM, none, frame, unwind::seh::ARRIVAL);
        self.builder.ins().jump(pad, &[exception.into()]);
        self.builder.switch_to_block(returned);
        results
    }

    /// Fills in the landing pads of a body in `MirPhase::Runtime`: each keeps the exception
    /// and goes on to its cleanup block, which ends in `UnwindResume`.
    fn emit_pads(&mut self) {
        for (target, pad) in std::mem::take(&mut self.pads) {
            self.builder.switch_to_block(pad);
            let exception = self.builder.block_params(pad)[0];
            let slot = self.exception_slot();
            self.builder.ins().stack_store(ptr(), exception, slot, 0);
            self.builder.ins().jump(self.blocks[target], &[]);
        }
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
        let mut values = Vec::new();
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
        let Some(hir_ty::mir::CoroutineLocals { context, .. }) = body.coroutine else {
            unsupported!("async body without a context");
        };
        let context = self.builder.ins().load(ptr(), MEM, self.locals[context].addr, 0);
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
        // FIXME: rustc panics with "`async fn` resumed after completion".
        self.builder.switch_to_block(finished);
        self.builder.ins().trap(TrapCode::user(3).expect("non-zero trap code"));

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

    /// Where the body is written to end, which is where rustc has an `async` block dropped.
    fn returns_at(body: &MirBody<'db>) -> MirSpan {
        let returns = body.basic_blocks.iter().find_map(|(_, block)| match &block.terminator {
            Some(hir_ty::mir::Terminator { kind: TerminatorKind::Return, span }) => Some(*span),
            _ => None,
        });
        returns.unwrap_or(MirSpan::Unknown)
    }

    /// Calls the `poll` of the `async` block of type `ty`, which is what `Future::poll` is for
    /// it. `args` are the pinned reference and the context.
    fn coroutine_callee(&mut self, ty: Ty<'db>) -> Option<Callee<'db>> {
        let (def, parent_args) = coroutine_instance(self.cg.db, ty)?;
        Some(Callee::Instance(def, parent_args))
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
        let mut values = Vec::new();
        for (param, value) in body.params.iter().zip([Some(addr), None]) {
            let slot = self.scratch();
            let value = value.unwrap_or_else(|| self.builder.ins().iconst(ptr(), 0));
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
        sig.params.push(AbiParam::new(ptr()));
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
        self.builder.append_block_param(pad, ptr());
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
        sig.params.push(AbiParam::new(ptr()));
        let mut values = vec![addr];
        if self.is_unsized(ty) {
            let Some(meta) = meta else {
                unsupported!("drop of unsized {ty:?} without metadata");
            };
            sig.params.push(AbiParam::new(ptr()));
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
        let mut sig = self.cg.make_signature();
        sig.params.push(AbiParam::new(ptr()));
        if self.is_unsized(ty) {
            sig.params.push(AbiParam::new(ptr()));
        }
        self.builder.func.signature = sig;
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let params = self.builder.block_params(entry).to_vec();
        self.drop_contents(params[0], params.get(1).copied(), ty)?;
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
                        self.builder.ins().load(ptr(), MEM, l, meta_at()),
                        self.builder.ins().load(ptr(), MEM, r, meta_at()),
                    )
                });
                let l = self.builder.ins().load(ptr(), MEM, l, 0);
                let r = self.builder.ins().load(ptr(), MEM, r, 0);
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
                        self.builder.ins().iconst(ptr(), len as i64)
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
                self.builder.append_block_param(header, ptr());
                let zero = self.builder.ins().iconst(ptr(), 0);
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
                        let mut arms = Vec::new();
                        for &(variant, _) in id.enum_variants(db).variants.values() {
                            if db.field_types(variant.into()).iter().next().is_none() {
                                continue;
                            }
                            let value = db.const_eval_discriminant(variant).map_err(|e| {
                                CodegenError::Frontend(format!("discriminant: {e:?}"))
                            })?;
                            let block = self.builder.create_block();
                            switch.set_entry(wide.entry(value), block);
                            arms.push((block, variant));
                        }
                        switch.emit(&mut self.builder, l_discr, done);
                        for (block, variant) in arms {
                            self.builder.switch_to_block(block);
                            // A constant of a pattern is never in such a variant, and here
                            // the two are in the same one.
                            if !self.fields_compare_in_parts(variant.into(), subst, &mut vec![ty]) {
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
                for offset in [0, meta_at()] {
                    let l = self.builder.ins().load(ptr(), MEM, l, offset);
                    let r = self.builder.ins().load(ptr(), MEM, r, offset);
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
                            let mut arms = Vec::new();
                            for &(variant, _) in id.enum_variants(db).variants.values() {
                                let value = db.const_eval_discriminant(variant).map_err(|e| {
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
            TyKind::Tuple(tys) => self.drop_tuple_fields(addr, ty, tys.iter().collect())?,
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
                let Some(TyKind::Tuple(tys)) = ty.closure_upvars_ty().map(|it| it.kind()) else {
                    unsupported!("drop glue for a closure without tupled upvars");
                };
                self.drop_tuple_fields(addr, ty, tys.iter().collect())?
            }
            TyKind::Coroutine(..) => self.drop_coroutine(addr, ty)?,
            TyKind::Array(element, len) => {
                let Some(len) = try_const_usize(db, len) else {
                    unsupported!("array with unevaluated length");
                };
                let len = self.builder.ins().iconst(ptr(), len as i64);
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
                let glue = self.builder.ins().load(ptr(), MEM, vtable, 0);
                let call = self.builder.create_block();
                let done = self.builder.create_block();
                self.builder.ins().brif(glue, call, &[], done, &[]);
                self.builder.switch_to_block(call);
                let mut sig = self.cg.make_signature();
                sig.params.push(AbiParam::new(ptr()));
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
        let fields: Vec<_> = field_types
            .iter()
            .map(|(field, ty)| {
                let ty = ty.ty().instantiate(self.interner, subst).skip_norm_wip();
                (layout.fields.offset(u32::from(field.into_raw()) as usize), ty)
            })
            .filter(|&(_, ty)| self.needs_drop(ty))
            .collect();
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

    fn drop_tuple_fields(&mut self, addr: Value, ty: Ty<'db>, fields: Vec<Ty<'db>>) -> Result<()> {
        let layout = self.layout(ty)?;
        let fields: Vec<_> =
            fields.into_iter().enumerate().filter(|&(_, field)| self.needs_drop(field)).collect();
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
        let pads: Vec<ir::Block> = (1..count)
            .map(|_| {
                let pad = self.builder.create_block();
                self.builder.append_block_param(pad, ptr());
                self.builder.set_cold_block(pad);
                pad
            })
            .collect();
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
        let zero = self.builder.ins().iconst(ptr(), 0);
        if self.in_cleanup || self.aborts {
            self.drop_elements_from(addr, len, element, size, zero, None, done)?;
        } else {
            // If an element's drop unwinds, the ones after it are still dropped on the way out.
            let outer = self.landing_pad_for_call();
            let pad = self.builder.create_block();
            self.builder.append_block_param(pad, ptr());
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
        self.builder.append_block_param(header, ptr());
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
            self.builder.ins().store(MEM, meta, slot, meta_at());
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

/// Prints a body one statement a line, for debugging with `RA_JIT_DUMP_MIR`.
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
        eprintln!("  let {}: {:?}", compact(format!("{local:?}")), data.ty);
    }
    for (id, block) in body.basic_blocks.iter() {
        eprintln!("{}:", compact(format!("{id:?}")));
        for statement in &block.statements {
            eprintln!("    {}", compact(format!("{:?}", statement.kind)));
        }
        if let Some(terminator) = &block.terminator {
            eprintln!("    -> {}", compact(format!("{:?}", terminator.kind)));
        }
    }
}

/// The locals that can be kept in a register as far as the body goes: temporaries written
/// whole by assignments only, that nothing takes the address of.
fn register_locals(body: &MirBody<'_>, forwarded: &FxHashMap<LocalId, LocalId>) -> Vec<LocalId> {
    let whole = |place: &StoredPlace| place.projection.as_slice().is_empty().then_some(place.local);
    // Every mention of the whole local, and the ones that are a value read or an assignment.
    let mut mentions: FxHashMap<LocalId, (usize, usize)> = FxHashMap::default();
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
    let read = |operand: &Operand, plain: &mut dyn FnMut(&StoredPlace)| {
        if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind {
            plain(place);
        }
    };
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(to, rvalue) = &statement.kind {
                plain(to);
                for operand in operands_of(rvalue) {
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
    let bound: rustc_hash::FxHashSet<LocalId> = body
        .binding_locals
        .values()
        .chain(&body.param_locals)
        .copied()
        .chain(body.promoted_locals.iter().map(|it| it.0))
        .chain([returned_local(body)])
        .chain(forwarded.iter().flat_map(|(&temp, &to)| [temp, to]))
        .collect();
    body.locals
        .iter()
        .map(|(local, _)| local)
        .filter(|local| {
            !bound.contains(local) && mentions.get(local).is_none_or(|(all, plain)| all == plain)
        })
        .collect()
}

/// The locals of `body` that no place mentions, which hold nothing the body ever reads: their
/// storage markers are all there is of them. The parameters, the return slot and the promoted
/// locals are not among them, whatever mentions them: they are written from outside the body,
/// or read by it.
fn unmentioned_locals(body: &MirBody<'_>) -> FxHashSet<LocalId> {
    let mut mentioned = FxHashSet::default();
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
                && !body.promoted_locals.iter().any(|it| it.0 == *local)
                && !body.upvars_in_place.iter().any(|it| it.0 == *local)
        })
        .collect()
}

/// The local every return of `body` moves to the return slot, when nothing else mentions the
/// slot, as rustc's `RenameReturnPlace` finds it: in the caller's memory the value is built
/// where the caller wants it and returning copies nothing. A call's destination is a temporary
/// or a fresh binding, so the callee reaches that memory through nothing else.
fn returned_in_place(
    body: &MirBody<'_>,
    forwarded: &FxHashMap<LocalId, LocalId>,
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
        && !body.promoted_locals.iter().any(|it| it.0 == local);
    eligible.then_some(local)
}

/// How many times each local is dropped by a terminator or read by a `FakeRead`, counted in one
/// pass: a body of many blocks has many temporaries, and neither is a use of what one holds.
fn drops_and_fake_reads(body: &MirBody<'_>) -> FxHashMap<LocalId, usize> {
    let mut counted: FxHashMap<LocalId, usize> = FxHashMap::default();
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
    forwarded: &FxHashMap<LocalId, LocalId>,
) -> Vec<(LocalId, LocalId)> {
    let whole = |place: &StoredPlace| place.projection.as_slice().is_empty().then_some(place.local);
    let slot = return_slot();
    let mut found = Vec::new();
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
    let mut mentions: FxHashMap<LocalId, usize> = FxHashMap::default();
    body.visit_places(|place| *mentions.entry(place.local).or_default() += 1);
    let not_uses = drops_and_fake_reads(body);
    let dropped_or_read = |local: LocalId| not_uses.get(&local).copied().unwrap_or(0);
    let bound: rustc_hash::FxHashSet<LocalId> = body
        .binding_locals
        .values()
        .chain(&body.param_locals)
        .copied()
        .chain(body.promoted_locals.iter().map(|it| it.0))
        .chain(body.upvars_in_place.iter().map(|it| it.0))
        .chain([slot])
        .chain(forwarded.values().copied())
        .collect();
    found.retain(|&(temp, from)| {
        !bound.contains(&temp)
            && !forwarded.contains_key(&temp)
            && !forwarded.contains_key(&from)
            && from != slot
            && !body.promoted_locals.iter().any(|it| it.0 == from)
            && mentions[&temp] - dropped_or_read(temp) == 2
            && body.locals[temp].ty == body.locals[from].ty
    });
    let temps: rustc_hash::FxHashSet<LocalId> = found.iter().map(|it| it.0).collect();
    found.retain(|(_, from)| !temps.contains(from));
    found.sort();
    found.dedup();
    found
}

/// The temporaries a `let` is initialized through, each with the binding it is copied to right
/// after it is written: `let buffer = [0u8; N]` builds the array in a temporary and copies it.
/// The two can be one slot, the binding holding nothing yet when the temporary is written, and
/// a frame is not twice the size of what it declares.
fn forwarded_temps(body: &MirBody<'_>) -> FxHashMap<LocalId, LocalId> {
    let whole = |place: &StoredPlace| place.projection.as_slice().is_empty().then_some(place.local);
    let mut forwarded = FxHashMap::default();
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
    let mut mentions: FxHashMap<LocalId, usize> = FxHashMap::default();
    body.visit_places(|place| *mentions.entry(place.local).or_default() += 1);
    // A drop of the temporary is not a use: it holds a value only between the two statements,
    // which unwind to nowhere, and a drop of it then finds the slot holding its value.
    let not_uses = drops_and_fake_reads(body);
    let dropped_or_read = |local: LocalId| not_uses.get(&local).copied().unwrap_or(0);
    let bound: rustc_hash::FxHashSet<LocalId> = body
        .binding_locals
        .values()
        .chain(&body.param_locals)
        .copied()
        .chain(body.promoted_locals.iter().map(|it| it.0))
        .chain([returned_local(body)])
        .collect();
    forwarded.retain(|&temp, to| {
        !bound.contains(&temp)
            && mentions[&temp] - dropped_or_read(temp) == 2
            && body.locals[temp].ty == body.locals[*to].ty
            && !body.promoted_locals.iter().any(|it| it.0 == *to)
            && *to != returned_local(body)
    });
    forwarded
}

/// The operands an rvalue reads.
fn operands_of(rvalue: &Rvalue) -> Vec<&Operand> {
    match rvalue {
        Rvalue::UnaryOp(_, it)
        | Rvalue::Cast(_, it, _)
        | Rvalue::Repeat(it, _)
        | Rvalue::Use(it) => {
            vec![it]
        }
        Rvalue::CheckedBinaryOp(_, left, right) => vec![left, right],
        Rvalue::Aggregate(_, operands) => operands.iter().collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use cranelift_codegen::ir::{self, AbiParam, InstBuilder, types};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

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
        let isa = super::target().unwrap();
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
        let code = super::emit(&mut ctx).unwrap();
        assert!(!code.bytes.is_empty());
    }

    /// A function Cranelift's incremental cache has the code of tells where its variables
    /// are as one compiled afresh does. The cache keeps the frame's layout without the slots
    /// at its end that are at the bottom of the frame, and the variables in them were left
    /// out of what a build from a store that had the code wrote (#242).
    #[test]
    fn a_slot_at_the_bottom_of_the_frame_has_its_place_from_the_cache_too() {
        use cranelift_codegen::incremental_cache::CacheKvStore;
        use std::{borrow::Cow, collections::HashMap};

        #[derive(Default)]
        struct Memory(HashMap<Vec<u8>, Vec<u8>>);
        impl CacheKvStore for Memory {
            fn get(&self, key: &[u8]) -> Option<Cow<'_, [u8]>> {
                self.0.get(key).map(|it| Cow::Borrowed(&it[..]))
            }
            fn insert(&mut self, key: &[u8], val: Vec<u8>) {
                self.0.insert(key.to_vec(), val);
            }
        }

        let isa = super::target().unwrap();
        // A leaf that stores to its one slot, which is at the bottom of its frame.
        let function = || {
            let sig = ir::Signature::new(isa.default_call_conv());
            let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), sig);
            let mut builder_ctx = FunctionBuilderContext::new();
            let mut builder = FunctionBuilder::new(&mut func, &mut builder_ctx);
            let slot = builder.create_sized_stack_slot(ir::StackSlotData::new(
                ir::StackSlotKind::ExplicitSlot,
                8,
                3,
            ));
            let entry = builder.create_block();
            builder.switch_to_block(entry);
            builder.seal_block(entry);
            let value = builder.ins().iconst(types::I64, 7);
            builder.ins().stack_store(isa.pointer_type(), value, slot, 0);
            builder.ins().return_(&[]);
            builder.finalize(isa.frontend_config());
            (func, slot)
        };
        let mut cache = Memory::default();
        let mut places = |hit: bool| {
            let (func, slot) = function();
            let mut ctx = cranelift_codegen::Context::for_function(func);
            let (compiled, cached) =
                ctx.compile_with_cache(&**isa, &mut cache, &mut Default::default()).unwrap();
            assert_eq!(cached, hit);
            let frame = compiled.buffer.frame_layout().unwrap().clone();
            super::SlotPlaces::of_frame(&**isa, &frame, &ctx.func, std::iter::once(slot))
        };
        let afresh = places(false);
        assert_eq!(afresh.slots.len(), 1);
        assert_eq!(places(true), afresh);
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
            macho::S_MOD_INIT_FUNC_POINTERS
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
                    assert_ne!(n_desc & macho::N_NO_DEAD_STRIP, 0);
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
                    SectionFlags::Elf { sh_flags } => {
                        sh_flags & u64::from(elf::SHF_GNU_RETAIN) != 0
                    }
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
}
