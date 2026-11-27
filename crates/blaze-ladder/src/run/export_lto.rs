//! Whole-program demand-driven cross-function LIR inlining and LTO for `blaze export`.
//!
//! 1. Starts from workspace seed instances already reached by `Walk` and performs a bounded BFS
//!    (default `depth <= 16`, up to `4096` bodies) following normal-block non-cold `Op::Call` edges
//!    first (and `Op::Invoke` edges only with remaining budget) into reachable non-cold,
//!    non-noreturn callees and library callers (up to `max_caller_insts`, default `2048` raw LIR
//!    instructions, including `#[inline(never)]` hot algorithms whose own small callees inline into
//!    them while `lto_optimize_module` continues respecting `Inline::Never` and
//!    `policy.max_callee_insts` when deciding what to inline into callers). Cold (unless
//!    `#[inline(always)]`) and `-> !` noreturn bodies stay excluded.
//! 2. Runs deterministic bottom-up Tarjan SCC ThinLTO (`blaze_lir::lto::lto_optimize_module`) over
//!    the gathered module so iterator loops (`Range::into_iter`/`next`, `slice::Iter::next`),
//!    slice indexing (`Index::index`), wrapping/checked arithmetic, and workspace helpers inline
//!    completely across function boundaries before SOTA loop optimization.
//! 3. Transitively resolves every [`blaze_lir::inline::ImportedRef`] across all [`RefKind`]s
//!    (`Func`, `Static`, `Tls`, `Vtable`, and C externs) back to its originating body's reference
//!    tables, binds soft-float routines at collision-free tail places, and emits each optimized
//!    body into an ephemeral sealed `CodeRecord` via `emit_sota_record_for_machine` targeting the
//!    probed host [`Machine`] (`export_machine`: `LSE | DOTPROD | CRC32 | SME | SME2 | CSSC | SHA3`).
//! 4. Keeps host-specific LTO records strictly in the caller's export allocator — never writing
//!    them back to portable `CodeStore` (Rule 25).

use core::fmt;

use blaze_code::datum::DatumView;
use blaze_code::record::{CodeView, Space, Subject, Symbol, Target as CodeTarget, TrapKind, TrapSource};
use blaze_emit::{
    AbiFnInput, CallConvKind, InternalFastRole, Isa, ScratchOrder, SotaRecordAbiOpts, ZeroAbiOpts,
    can_use_internal_fast_a64, plan_module_abi,
};
use blaze_hash::{FixedState, Map, MapOpts, Set, SetOpts};
use blaze_lir::{
    Block, BlockKind, Field, Flags, Layout, Op, Ty, VerifyError,
    body::{Bodies, BodyId, DatumId, Target},
    func::{Conv, DataId, FnAttrs, Inline, ParamKind, RefId, RefKind, RefRec, SigId, SigRec},
    inline::InlinePolicy,
    lto::{LtoRunner, ModuleBody, OptimizedBody, SequentialLtoRunner, lto_optimize_module_par, tarjan_sccs},
    op::{Word, walk},
    pgo::{PgoRecordError, PgoSiteTable},
    simplify::SimplifyError,
    verify,
};
use blaze_own::{AVec, Alloc, SharedAlloc, Vec};
use blaze_tier::{Arch, CallClobbers, Machine, Reg, RegSet};
use blaze_typeck::ItemKey;

use super::{BuiltCallee, BuiltForeignStatic, BuiltLir, Maker, RecordsProgram, Typing, func_needs_soft_float};
use crate::body_ids::{Grant, RowKind, Walk};
pub use crate::code_records::export_machine;
use crate::code_records::{Callee, CodeRecordError, InstanceNames, emit_code_record_for_machine_with_abi};
use crate::front2::builtins_shim::Routines;
use crate::run::RunError;
use crate::symbols::{static_symbol, tls_symbol};

#[path = "export_lto_all.rs"]
mod all_crates;
pub use all_crates::{
    CachedLtoStatus, CachedLtoView, ExportAllLtoOut, ExportLirBuilder, LtoRowResult,
    MakerLirBuilder, decode_lto_scc_header, extract_direct_calls, optimize_export_all_par_cached,
};

pub use blaze_lir::lto::LtoRunner as ExportLtoRunner;
pub use blaze_lir::simplify::SimplifyError as ExportLtoSimplifyError;
pub use blaze_tier::Machine as ExportLtoMachine;

/// Upper bound on transitive `ImportedRef` resolution hops (Rule 18).
const MAX_IMPORT_HOPS: usize = 32;
/// Exponential decay ratio (`3/4` persisted + fresh static baseline) when merging a persisted
/// cross-worktree `BPG1` PGO record into export LTO (Rule 86).
const PGO_DECAY_NUM: u64 = 3;
const PGO_DECAY_DEN: u64 = 4;
/// Baseline static weight assigned by [`PgoSiteTable::estimate_static`] to non-cold straight-line
/// bodies without loops. Only bodies strictly hotter than this baseline (either containing loops,
/// observed hot in a persisted `BPG1` profile, or directly calling a hot leaf kernel) enter
/// [`ExportLtoOut::hot_symbols`].
const STATIC_STRAIGHT_LINE_WEIGHT: u64 = 10;
/// Minimum own ranking heat (`loop_score + pgo_weight`) for a 0-call leaf body to qualify as a
/// hot leaf kernel that propagates 1-hop heat to its direct callers. A depth-1 leaf with `>= 2`
/// loop blocks (`loop_score >= 20`) or hot persisted PGO weight qualifies, while 0-loop
/// straight-line wrappers (`own_score == STATIC_STRAIGHT_LINE_WEIGHT`) stay out.
const HOT_LEAF_THRESHOLD: u64 = 20;
/// Divisor for 1-hop hot-caller heat propagation (`callee_heat / HOT_CALLER_HEAT_DIV`).
const HOT_CALLER_HEAT_DIV: u64 = 4;
/// Cap loop nesting depth exponent so `10u64.saturating_pow(depth)` stays well within `u64`.
const MAX_LOOP_DEPTH_EXP: u32 = 6;
/// Maximum distinct loop headers analyzed per function when scoring static loop heat (Rule 18).
const MAX_LOOP_HEADERS: usize = 256;
/// Hash domain for deterministic export PGO profile digests (`blaze_export::Profile::digest`).
const EXPORT_PGO_PROFILE_DOMAIN: &str = "blaze v2 export pgo profile 1";

/// Configuration for [`optimize_export_workspace`] (Rule 83: one options struct).
#[derive(Clone, Copy, Debug)]
pub struct ExportLtoOpts<'b> {
    /// Target host machine features (typically from [`export_machine`]).
    pub machine: Machine,
    /// Maximum BFS hop depth from workspace seed functions into reachable non-cold callees.
    pub max_depth: u8,
    /// Maximum total function bodies gathered into the LTO module.
    pub max_bodies: usize,
    /// Maximum LIR instruction count for keeping a non-workspace callee in the LTO module.
    pub max_callee_insts: usize,
    /// Maximum LIR instruction count for keeping a `#[inline(always)]` non-workspace callee.
    pub max_always_inline_insts: usize,
    /// Maximum LIR instruction count for keeping a reachable non-cold library caller (e.g.
    /// `serde_json::Deserializer::parse_value`, derived `Visitor::visit_map`,
    /// `core::slice::sort::drift::sort`, `hashbrown::RawTable::find`) in the LTO module so its
    /// own small callees inline bottom-up into it and it emits with host-ISA SOTA codegen.
    pub max_caller_insts: usize,
    /// Cross-function inlining policy handed to `blaze_lir::lto::lto_optimize_module_par`.
    pub policy: InlinePolicy,
    /// Optional persisted `BPG1` PGO binary record merged with static cold-start estimates
    /// via exponential decay before LTO inlining and hot-function layout ranking.
    pub pgo_bytes: Option<&'b [u8]>,
}

impl ExportLtoOpts<'static> {
    /// Release export defaults: bounded demand-driven BFS (`depth <= 24`, `<= 12288` bodies,
    /// non-cold library callers `<= 8192` pre-SROA LIR insts) and aggressive cross-function
    /// inlining (`lto_optimize_module_par` enforces `Inline::Never` and `policy.max_callee_insts`
    /// when deciding whether a body may be inlined into its own callers).
    pub fn for_export(isa: Isa) -> Self {
        Self {
            machine: export_machine(isa),
            max_depth: 24,
            max_bodies: 12288,
            max_callee_insts: 512,
            max_always_inline_insts: 1024,
            max_caller_insts: 32_768,
            policy: InlinePolicy {
                cost_threshold: 120,
                max_callee_insts: 256,
                hot_multiplier: 4,
                hot_weight_threshold: 10,
                max_depth: 16,
                max_growth: 6144,
                allow_external_refs: true,
                allow_loops: true,
            },
            pgo_bytes: None,
        }
    }
}

/// Why whole-program export LTO stopped, naming the symbol and rule (Rule 19, Rule 3).
#[derive(Debug)]
pub enum ExportLtoError {
    /// A reached instance row lacked a runnable grant when rebuilding LIR.
    NotRunnable { symbol: [u8; 32], tabled: ItemKey },
    /// Building LIR for one reached instance failed.
    BuildLir { symbol: [u8; 32], error: RunError },
    /// Decoding an explicit persisted `BPG1` PGO record failed.
    PgoRecord(PgoRecordError),
    /// `blaze_lir::lto::lto_optimize_module` refused the module.
    Optimize(SimplifyError),
    /// A post-LTO function failed LIR verification.
    Verify { symbol: [u8; 32], error: VerifyError },
    /// An `ImportedRef` pointed to a body not present in the LTO module.
    UnresolvedImport { symbol: [u8; 32], source: [u8; 32] },
    /// A transitive `ImportedRef` chain exceeded [`MAX_IMPORT_HOPS`].
    ImportCycle { symbol: [u8; 32] },
    /// An `ImportedRef` named a reference id or place outside its originating body's table.
    BadRef { symbol: [u8; 32], what: &'static str, ref_id: u32 },
    /// An `ImportedRef` kind disagreed with its originating body's `RefRec` kind.
    RefKindMismatch { symbol: [u8; 32], ref_id: u32 },
    /// Emitting or sealing the post-LTO `CodeRecord` failed.
    Record { symbol: [u8; 32], error: CodeRecordError },
    /// A content-addressed LTO cache record failed lossless decode (Rule 3: no silent fallback).
    CorruptCache { what: &'static str, key: [u8; 32] },
    /// A post-LTO emitted `CodeRecord` or vtable datum failed decode during target verification.
    DecodeRecord { symbol: [u8; 32], what: &'static str },
    /// A post-LTO emitted body or vtable referenced a target neither in `Walk` nor in any LTO
    /// origin's declared tables (Rule 3: export never leaves a hole).
    MissingDeclared { from: [u8; 32], target: CodeTarget },
}

struct Hex<'a>(&'a [u8; 32]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0.iter() {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Display for ExportLtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExportLtoError::NotRunnable { symbol, tabled } => write!(
                f,
                "export LTO: reached fn {:#x} (symbol {}) has no runnable proof (export_lto.rs)",
                tabled.0,
                Hex(symbol)
            ),
            ExportLtoError::BuildLir { symbol, error } => {
                write!(f, "export LTO: building LIR for {}: {error} (export_lto.rs)", Hex(symbol))
            }
            ExportLtoError::PgoRecord(error) => {
                write!(f, "export LTO: persisted BPG1 PGO record refused: {error} (export_lto.rs)")
            }
            ExportLtoError::Optimize(error) => {
                write!(f, "export LTO: module inlining failed: {error} (export_lto.rs)")
            }
            ExportLtoError::Verify { symbol, error } => {
                write!(f, "export LTO: post-LTO LIR failed verify for {}: {error} (export_lto.rs)", Hex(symbol))
            }
            ExportLtoError::UnresolvedImport { symbol, source } => write!(
                f,
                "export LTO: imported ref in {} names unknown source body {} (export_lto.rs)",
                Hex(symbol),
                Hex(source)
            ),
            ExportLtoError::ImportCycle { symbol } => write!(
                f,
                "export LTO: transitive imported ref chain in {} exceeded {MAX_IMPORT_HOPS} hops (export_lto.rs)",
                Hex(symbol)
            ),
            ExportLtoError::BadRef { symbol, what, ref_id } => write!(
                f,
                "export LTO: bad {what} (ref {ref_id}) while resolving imports in {} (export_lto.rs)",
                Hex(symbol)
            ),
            ExportLtoError::RefKindMismatch { symbol, ref_id } => write!(
                f,
                "export LTO: imported ref {ref_id} in {} disagreed with originating RefRec kind (export_lto.rs)",
                Hex(symbol)
            ),
            ExportLtoError::Record { symbol, error } => {
                write!(f, "export LTO: emitting code record for {}: {error} (export_lto.rs)", Hex(symbol))
            }
            ExportLtoError::CorruptCache { what, key } => {
                write!(f, "export LTO: corrupt cached {what} under key {} (export_lto.rs)", Hex(key))
            }
            ExportLtoError::DecodeRecord { symbol, what } => {
                write!(f, "export LTO: decoding post-LTO {what} for {} failed (export_lto.rs)", Hex(symbol))
            }
            ExportLtoError::MissingDeclared { from, target } => write!(
                f,
                "export LTO: post-LTO function {} references {:?} {} with no declaration in walk or LTO origins (export_lto.rs)",
                Hex(from),
                target.space,
                target.symbol
            ),
        }
    }
}

impl core::error::Error for ExportLtoError {}

/// Fine-grained sub-phase timings and cache hit counters for [`optimize_export_workspace_par_cached`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportLtoTimes {
    pub bfs_ns: u64,
    pub pgo_ns: u64,
    pub inline_ns: u64,
    pub prep_ns: u64,
    pub emit_ns: u64,
    pub lir_hits: usize,
    pub lir_misses: usize,
    pub emit_hits: usize,
    pub emit_misses: usize,
}

/// Content-addressed local cache and monotonic clock seam for whole-program export LTO
/// (Rule 14, Rule 21, Rule 51, Rule 75).
///
/// All cache lookups and writes occur on the caller's thread (between parallel worker phases),
/// so implementations can borrow a single-threaded store handle (`Lent`) directly with zero
/// mutex contention or `unsafe`. Entries live strictly in local host storage (`Namespace::LOCAL`).
pub trait ExportLtoCache {
    /// Returns the content-addressed key for the pre-LTO lowered `BuiltLir` of walk `row` under
    /// `machine` (covering body inputs, `content_key`, `machine`, `blaze_lir::persist::SCHEMA`,
    /// and producer identity). Returns `None` if this row cannot be keyed safely.
    fn lir_key(&self, _row: usize, _machine: Machine) -> Option<[u8; 32]> {
        None
    }
    /// Producer identity digest hashed into every post-LTO emit cache key so compiler/emitter
    /// changes invalidate cached host-ISA LTO records (Rule 21). `None` disables emit caching.
    fn producer_digest(&self) -> Option<[u8; 32]> {
        None
    }
    /// Fetches a previously stored LTO cache payload by its 32-byte key.
    fn get(&self, _key: [u8; 32], _alloc: SharedAlloc<'static>) -> Option<Vec<u8, SharedAlloc<'static>>> {
        None
    }
    /// Zero-copy cache probe callback: returns `Some(true)` if `key` hit and `on_hit` ran,
    /// `Some(false)` if `key` missed, or `None` if zero-copy borrows are unavailable (caller falls
    /// back to [`Self::get`]).
    fn get_slice_with(&self, _key: [u8; 32], _on_hit: &mut dyn FnMut(&[u8])) -> Option<bool> {
        None
    }
    /// Persists an LTO cache payload under its 32-byte key.
    fn put(&self, _key: [u8; 32], _bytes: &[u8]) {}
    /// Persists an LTO cache payload tagged by crate class (`is_toolchain` routes toolchain LTO
    /// artifacts into the shared toolchain GC root so all workspaces reuse them).
    fn put_with_class(&self, _is_toolchain: bool, key: [u8; 32], bytes: &[u8]) {
        self.put(key, bytes);
    }
    /// Records a cache-hit key under its crate class's GC root (`is_toolchain`).
    fn touch_with_class(&self, _is_toolchain: bool, _key: [u8; 32]) {}
    /// Monotonic timestamp in nanoseconds for sub-phase timings (`0` when timing is disabled).
    fn now_ns(&self) -> u64 {
        0
    }
}

/// No-op [`ExportLtoCache`] disabling incremental LTO caching and sub-phase timing.
#[derive(Clone, Copy, Default, Debug)]
pub struct NoExportLtoCache;

impl ExportLtoCache for NoExportLtoCache {}

/// Magic + format version for cached pre-LTO [`BuiltLir`] payloads (v2 persists `declared`
/// instance `(ItemKey, name_bytes)` rows so cache hits never drop walk declarations across
/// binaries or incremental rebuilds).
const BUILT_LIR_CACHE_MAGIC: &[u8; 8] = b"BLZLIR02";
/// Hash domain for cached pre-LTO [`BuiltLir`] keys (Rule 21).
const EXPORT_LTO_BUILT_LIR_KEY_DOMAIN: &str = "blaze v2 export lto built lir 1";
/// Magic + format version for cached post-LTO emitted body payloads.
const EMIT_BODY_CACHE_MAGIC: &[u8; 8] = b"BLZEMT01";
/// Hash domain for post-LTO emitted body cache keys (Rule 21).
const EXPORT_LTO_EMIT_KEY_DOMAIN: &str = "blaze v2 export lto emit 1";

/// Computes a deterministic, input-complete content-addressed cache key for one pre-LTO lowered
/// [`BuiltLir`] body (Rule 21), covering `producer`, host `machine`, `blaze_lir::persist::SCHEMA`,
/// `symbol`, verified `content_key`, and the body's `BodyId` (`body_key` + `reads`).
pub fn built_lir_cache_key(
    producer: [u8; 32],
    machine: Machine,
    symbol: Symbol,
    content_key: [u8; 32],
    body_key: [u8; 32],
    reads: &[crate::body_ids::Read],
) -> [u8; 32] {
    let mut h = blaze_ids::Hasher::new(EXPORT_LTO_BUILT_LIR_KEY_DOMAIN);
    h.update(BUILT_LIR_CACHE_MAGIC);
    h.update(&blaze_lir::persist::SCHEMA.0.0);
    h.update(&producer);
    let arch_byte: u8 = match machine.arch {
        Arch::Arm64 => 1,
        Arch::X86_64 => 2,
    };
    let os_byte: u8 = match machine.os {
        blaze_tier::Os::MacOs => 1,
        blaze_tier::Os::Linux => 2,
    };
    h.update(&[arch_byte, os_byte]);
    h.update(&machine.features.to_le_bytes());
    h.update(&symbol.0);
    h.update(&content_key);
    h.update(&body_key);
    let read_len = u64::try_from(reads.len()).unwrap_or(u64::MAX);
    h.update(&read_len.to_le_bytes());
    for r in reads {
        h.update(&r.fact);
        h.update(&r.digest);
    }
    h.finish().0
}

fn trap_kind_to_byte(kind: TrapKind) -> u8 {
    match kind {
        TrapKind::MatchNoArm => 0,
        TrapKind::LetElse => 1,
        TrapKind::NeverValue => 2,
        TrapKind::AfterDivergingCall => 3,
        TrapKind::Intrinsic => 4,
        TrapKind::Discriminant => 5,
        TrapKind::SliceLength => 6,
        TrapKind::NoreturnAsm => 7,
        TrapKind::BodyEnd => 8,
        TrapKind::AfterPanic => 9,
    }
}

fn trap_kind_from_byte(byte: u8) -> Option<TrapKind> {
    match byte {
        0 => Some(TrapKind::MatchNoArm),
        1 => Some(TrapKind::LetElse),
        2 => Some(TrapKind::NeverValue),
        3 => Some(TrapKind::AfterDivergingCall),
        4 => Some(TrapKind::Intrinsic),
        5 => Some(TrapKind::Discriminant),
        6 => Some(TrapKind::SliceLength),
        7 => Some(TrapKind::NoreturnAsm),
        8 => Some(TrapKind::BodyEnd),
        9 => Some(TrapKind::AfterPanic),
        _ => None,
    }
}

fn push_u32(out: &mut Vec<u8, SharedAlloc<'static>>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn push_u64(out: &mut Vec<u8, SharedAlloc<'static>>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn push_u128(out: &mut Vec<u8, SharedAlloc<'static>>, v: u128) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn push_len(out: &mut Vec<u8, SharedAlloc<'static>>, len: usize) -> Option<()> {
    let len_u32 = u32::try_from(len).ok()?;
    push_u32(out, len_u32);
    Some(())
}

struct ByteReader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> ByteReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        let b: [u8; 4] = self.take(4)?.try_into().ok()?;
        Some(u32::from_le_bytes(b))
    }

    fn u64(&mut self) -> Option<u64> {
        let b: [u8; 8] = self.take(8)?.try_into().ok()?;
        Some(u64::from_le_bytes(b))
    }

    fn u128(&mut self) -> Option<u128> {
        let b: [u8; 16] = self.take(16)?.try_into().ok()?;
        Some(u128::from_le_bytes(b))
    }

    fn sym(&mut self) -> Option<Symbol> {
        let b: [u8; 32] = self.take(32)?.try_into().ok()?;
        Some(Symbol(b))
    }

    fn len(&mut self) -> Option<usize> {
        usize::try_from(self.u32()?).ok()
    }

    fn at_end(&self) -> bool {
        self.at == self.bytes.len()
    }
}

/// `Send`-safe subset of [`ExportLtoError`] returned out of parallel worker closures (which never
/// invoke `Maker::build_lir_in` and thus never produce `RunError`).
enum ParLtoErr {
    Verify { symbol: [u8; 32], error: VerifyError },
    UnresolvedImport { symbol: [u8; 32], source: [u8; 32] },
    ImportCycle { symbol: [u8; 32] },
    BadRef { symbol: [u8; 32], what: &'static str, ref_id: u32 },
    RefKindMismatch { symbol: [u8; 32], ref_id: u32 },
    Record { symbol: [u8; 32], error: CodeRecordError },
}

impl From<ParLtoErr> for ExportLtoError {
    fn from(e: ParLtoErr) -> Self {
        match e {
            ParLtoErr::Verify { symbol, error } => ExportLtoError::Verify { symbol, error },
            ParLtoErr::UnresolvedImport { symbol, source } => ExportLtoError::UnresolvedImport { symbol, source },
            ParLtoErr::ImportCycle { symbol } => ExportLtoError::ImportCycle { symbol },
            ParLtoErr::BadRef { symbol, what, ref_id } => ExportLtoError::BadRef { symbol, what, ref_id },
            ParLtoErr::RefKindMismatch { symbol, ref_id } => ExportLtoError::RefKindMismatch { symbol, ref_id },
            ParLtoErr::Record { symbol, error } => ExportLtoError::Record { symbol, error },
        }
    }
}

type DeclaredRow = (ItemKey, Vec<u8, SharedAlloc<'static>>);

/// Lowers hardware-exact IEEE-754 libm calls in `built` in place for `machine` and verifies the
/// rewritten LIR if changed.
pub fn prepare_built_lir_in_place<A: core::alloc::Allocator>(
    built: &mut BuiltLir<A>,
    machine: Machine,
    scratch: Alloc<'_>,
) -> Result<bool, VerifyError> {
    let lowered = lower_ieee_libm_calls(built, machine, scratch);
    if lowered {
        verify(built.func.view())?;
    }
    Ok(lowered)
}

/// Lowers hardware-exact IEEE-754 libm calls in `built`, verifies if rewritten, and encodes
/// `BLZLIR02` bytes for `HomeLtoCache` (shared by walk-time capture and export LTO misses).
pub(crate) fn prepare_and_encode_built_lir(
    built: &mut BuiltLir<SharedAlloc<'static>>,
    declared: &[DeclaredRow],
    machine: Machine,
    scratch: Alloc<'_>,
    shared: SharedAlloc<'static>,
) -> Option<Vec<u8, SharedAlloc<'static>>> {
    if prepare_built_lir_in_place(built, machine, scratch).is_err() {
        return None;
    }
    encode_built_lir(built, declared, shared)
}

fn encode_built_lir(
    built: &BuiltLir<SharedAlloc<'static>>,
    declared: &[DeclaredRow],
    alloc: SharedAlloc<'static>,
) -> Option<Vec<u8, SharedAlloc<'static>>> {
    let func_bytes = blaze_lir::persist::to_bytes_in(&built.func.view(), alloc);
    let est = 64usize
        .saturating_add(func_bytes.len())
        .saturating_add(built.funcs.len().saturating_mul(33))
        .saturating_add(built.statics.len().saturating_mul(16))
        .saturating_add(built.c_statics.len().saturating_mul(25))
        .saturating_add(built.vtables.len().saturating_mul(32))
        .saturating_add(built.tls.len().saturating_mul(16))
        .saturating_add(built.traps.len().saturating_mul(112))
        .saturating_add(built.c_text.len())
        .saturating_add(declared.len().saturating_mul(48));
    let mut out: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(est, alloc);
    out.extend_from_slice(BUILT_LIR_CACHE_MAGIC);
    // 8-byte length after 8-byte magic places `func_bytes` at offset 16 (matching `blaze_format::ALIGN`).
    push_u64(&mut out, u64::try_from(func_bytes.len()).ok()?);
    out.extend_from_slice(func_bytes.as_slice());

    push_len(&mut out, built.funcs.len())?;
    for f in built.funcs.iter() {
        match *f {
            BuiltCallee::Symbol(sym) => {
                out.push(0);
                out.extend_from_slice(&sym.0);
            }
            BuiltCallee::C { start, len } => {
                out.push(1);
                push_u32(&mut out, start);
                push_u32(&mut out, len);
            }
        }
    }

    push_len(&mut out, built.statics.len())?;
    for &item in built.statics.iter() {
        push_u128(&mut out, item.0);
    }

    push_len(&mut out, built.c_statics.len())?;
    for cs in built.c_statics.iter() {
        push_u128(&mut out, cs.item.0);
        push_u32(&mut out, cs.start);
        push_u32(&mut out, cs.len);
        out.push(u8::from(cs.weak));
    }

    push_len(&mut out, built.vtables.len())?;
    for &vsym in built.vtables.iter() {
        out.extend_from_slice(&vsym.0);
    }

    push_len(&mut out, built.vtable_data.len())?;
    for (vsym, bytes) in built.vtable_data.iter() {
        out.extend_from_slice(&vsym.0);
        push_len(&mut out, bytes.len())?;
        out.extend_from_slice(bytes.as_slice());
    }

    push_len(&mut out, built.tls.len())?;
    for &item in built.tls.iter() {
        push_u128(&mut out, item.0);
    }

    push_len(&mut out, built.traps.len())?;
    for t in built.traps.iter() {
        push_u32(&mut out, t.ordinal);
        out.push(trap_kind_to_byte(t.kind));
        match t.site {
            Some((item, at)) => {
                out.push(1);
                push_u128(&mut out, item);
                push_u32(&mut out, at);
            }
            None => out.push(0),
        }
        match t.subject {
            Some(subj) => {
                out.push(1);
                push_u128(&mut out, subj.item);
                push_u32(&mut out, subj.count);
                for &d in subj.discriminants.iter() {
                    push_u64(&mut out, d);
                }
            }
            None => out.push(0),
        }
    }

    push_len(&mut out, built.c_text.len())?;
    out.extend_from_slice(built.c_text.as_slice());

    push_len(&mut out, declared.len())?;
    for (item, name) in declared.iter() {
        push_u128(&mut out, item.0);
        push_len(&mut out, name.len())?;
        out.extend_from_slice(name.as_slice());
    }
    Some(out)
}

fn decode_built_lir(
    bytes: &[u8],
    alloc: SharedAlloc<'static>,
) -> Option<(BuiltLir<SharedAlloc<'static>>, Vec<DeclaredRow, SharedAlloc<'static>>)> {
    let mut r = ByteReader::new(bytes);
    if r.take(8)? != BUILT_LIR_CACHE_MAGIC {
        return None;
    }
    let func_len = usize::try_from(r.u64()?).ok()?;
    let func_bytes = r.take(func_len)?;
    let func = blaze_lir::persist::read_to_func_in(func_bytes, alloc).ok()?;

    let funcs_len = r.len()?;
    let mut funcs: Vec<BuiltCallee, SharedAlloc<'static>> = Vec::with_capacity_in(funcs_len, alloc);
    for _ in 0..funcs_len {
        match r.u8()? {
            0 => funcs.push(BuiltCallee::Symbol(r.sym()?)),
            1 => {
                let start = r.u32()?;
                let len = r.u32()?;
                funcs.push(BuiltCallee::C { start, len });
            }
            _ => return None,
        }
    }

    let statics_len = r.len()?;
    let mut statics: Vec<ItemKey, SharedAlloc<'static>> = Vec::with_capacity_in(statics_len, alloc);
    for _ in 0..statics_len {
        statics.push(ItemKey(r.u128()?));
    }

    let c_statics_len = r.len()?;
    let mut c_statics: Vec<BuiltForeignStatic, SharedAlloc<'static>> =
        Vec::with_capacity_in(c_statics_len, alloc);
    for _ in 0..c_statics_len {
        let item = ItemKey(r.u128()?);
        let start = r.u32()?;
        let len = r.u32()?;
        let weak = match r.u8()? {
            0 => false,
            1 => true,
            _ => return None,
        };
        c_statics.push(BuiltForeignStatic { item, start, len, weak });
    }

    let vtables_len = r.len()?;
    let mut vtables: Vec<Symbol, SharedAlloc<'static>> = Vec::with_capacity_in(vtables_len, alloc);
    for _ in 0..vtables_len {
        vtables.push(r.sym()?);
    }

    let vtable_data_len = r.len()?;
    let mut vtable_data: Vec<(Symbol, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>> =
        Vec::with_capacity_in(vtable_data_len, alloc);
    for _ in 0..vtable_data_len {
        let vsym = r.sym()?;
        let dlen = r.len()?;
        let dbytes = r.take(dlen)?;
        let mut copy: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(dlen, alloc);
        copy.extend_from_slice(dbytes);
        vtable_data.push((vsym, copy));
    }

    let tls_len = r.len()?;
    let mut tls: Vec<ItemKey, SharedAlloc<'static>> = Vec::with_capacity_in(tls_len, alloc);
    for _ in 0..tls_len {
        tls.push(ItemKey(r.u128()?));
    }

    let traps_len = r.len()?;
    let mut traps: Vec<TrapSource, SharedAlloc<'static>> = Vec::with_capacity_in(traps_len, alloc);
    for _ in 0..traps_len {
        let ordinal = r.u32()?;
        let kind = trap_kind_from_byte(r.u8()?)?;
        let site = match r.u8()? {
            0 => None,
            1 => Some((r.u128()?, r.u32()?)),
            _ => return None,
        };
        let subject = match r.u8()? {
            0 => None,
            1 => {
                let item = r.u128()?;
                let count = r.u32()?;
                let mut discriminants = [0u64; 8];
                for d in discriminants.iter_mut() {
                    *d = r.u64()?;
                }
                Some(Subject { item, count, discriminants })
            }
            _ => return None,
        };
        traps.push(TrapSource { ordinal, kind, site, subject });
    }

    let c_text_len = r.len()?;
    let c_text_slice = r.take(c_text_len)?;
    let mut c_text: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(c_text_len, alloc);
    c_text.extend_from_slice(c_text_slice);

    let declared_len = r.len()?;
    let mut declared: Vec<DeclaredRow, SharedAlloc<'static>> = Vec::with_capacity_in(declared_len, alloc);
    for _ in 0..declared_len {
        let item = ItemKey(r.u128()?);
        let nlen = r.len()?;
        let nslice = r.take(nlen)?;
        let mut name: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(nlen, alloc);
        name.extend_from_slice(nslice);
        declared.push((item, name));
    }
    if !r.at_end() {
        return None;
    }

    Some((BuiltLir { func, funcs, statics, c_statics, vtables, vtable_data, tls, traps, c_text }, declared))
}

fn encode_emitted_body(
    record: &[u8],
    extra_declared: &[(ItemKey, Vec<u8, SharedAlloc<'static>>)],
    clobbers: CallClobbers,
    alloc: SharedAlloc<'static>,
) -> Option<Vec<u8, SharedAlloc<'static>>> {
    let est = 32usize
        .saturating_add(record.len())
        .saturating_add(extra_declared.len().saturating_mul(32));
    let mut out: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(est, alloc);
    out.extend_from_slice(EMIT_BODY_CACHE_MAGIC);
    push_u64(&mut out, clobbers.clobbered.0);
    push_len(&mut out, record.len())?;
    out.extend_from_slice(record);
    push_len(&mut out, extra_declared.len())?;
    for (item, name) in extra_declared.iter() {
        push_u128(&mut out, item.0);
        push_len(&mut out, name.len())?;
        out.extend_from_slice(name.as_slice());
    }
    Some(out)
}

fn decode_emitted_body(
    bytes: &[u8],
    alloc: SharedAlloc<'static>,
) -> Option<(
    Vec<u8, SharedAlloc<'static>>,
    Vec<(ItemKey, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>>,
    CallClobbers,
)> {
    let mut r = ByteReader::new(bytes);
    if r.take(8)? != EMIT_BODY_CACHE_MAGIC {
        return None;
    }
    let clobbers = CallClobbers { clobbered: RegSet(r.u64()?) };
    let rec_len = r.len()?;
    let rec_slice = r.take(rec_len)?;
    let mut record: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(rec_len, alloc);
    record.extend_from_slice(rec_slice);

    let extra_len = r.len()?;
    let mut extra_declared: Vec<(ItemKey, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>> =
        Vec::with_capacity_in(extra_len, alloc);
    for _ in 0..extra_len {
        let item = ItemKey(r.u128()?);
        let nlen = r.len()?;
        let nslice = r.take(nlen)?;
        let mut name: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(nlen, alloc);
        name.extend_from_slice(nslice);
        extra_declared.push((item, name));
    }
    if !r.at_end() {
        return None;
    }
    Some((record, extra_declared, clobbers))
}

fn hash_u32(h: &mut blaze_ids::Hasher, v: u32) {
    h.update(&v.to_le_bytes());
}

fn hash_len(h: &mut blaze_ids::Hasher, n: usize) {
    h.update(&(u64::try_from(n).unwrap_or(u64::MAX)).to_le_bytes());
}

fn compute_emit_cache_key(
    producer: [u8; 32],
    view: blaze_lir::View<'_>,
    names: &InstanceNames<'_>,
    routines: Routines<'_>,
    traps: &[TrapSource],
    abi_opts: SotaRecordAbiOpts<'_>,
    scratch: Alloc<'_>,
) -> [u8; 32] {
    let mut h = blaze_ids::Hasher::new(EXPORT_LTO_EMIT_KEY_DOMAIN);
    h.update(EMIT_BODY_CACHE_MAGIC);
    h.update(&producer);
    let arch_byte: u8 = match abi_opts.machine.arch {
        Arch::Arm64 => 1,
        Arch::X86_64 => 2,
    };
    let os_byte: u8 = match abi_opts.machine.os {
        blaze_tier::Os::MacOs => 1,
        blaze_tier::Os::Linux => 2,
    };
    h.update(&[
        arch_byte,
        os_byte,
        u8::from(abi_opts.is_hot),
        u8::from(abi_opts.preserve_all_cold),
        u8::from(abi_opts.internal_fast),
    ]);
    h.update(&abi_opts.machine.features.to_le_bytes());
    match abi_opts.scratch_order {
        Some(so) => {
            h.update(&[1]);
            h.update(&so.gpr);
            h.update(&so.fp);
        }
        None => {
            h.update(&[0]);
        }
    }
    hash_len(&mut h, abi_opts.callee_clobbers.len());
    for &(place, clob) in abi_opts.callee_clobbers {
        hash_u32(&mut h, place);
        h.update(&clob.clobbered.0.to_le_bytes());
    }
    hash_len(&mut h, abi_opts.internal_fast_callees.len());
    for &place in abi_opts.internal_fast_callees {
        hash_u32(&mut h, place);
    }

    hash_len(&mut h, names.funcs.len());
    for f in names.funcs {
        match *f {
            Callee::Symbol(sym) => {
                h.update(&[0]);
                h.update(&sym.0);
            }
            Callee::C(cname) => {
                h.update(&[1]);
                hash_len(&mut h, cname.len());
                h.update(cname.as_bytes());
            }
        }
    }
    hash_len(&mut h, names.statics.len());
    for &item in names.statics {
        h.update(&item.0.to_le_bytes());
    }
    hash_len(&mut h, names.c_statics.len());
    for cs in names.c_statics {
        h.update(&cs.item.0.to_le_bytes());
        h.update(&[u8::from(cs.weak)]);
        hash_len(&mut h, cs.name.len());
        h.update(cs.name.as_bytes());
    }
    hash_len(&mut h, names.vtables.len());
    for &vsym in names.vtables {
        h.update(&vsym.0);
    }
    hash_len(&mut h, names.tls.len());
    for &item in names.tls {
        h.update(&item.0.to_le_bytes());
    }

    routines.hash_into(&mut h);

    hash_len(&mut h, traps.len());
    for t in traps {
        hash_u32(&mut h, t.ordinal);
        h.update(&[trap_kind_to_byte(t.kind)]);
        match t.site {
            Some((item, at)) => {
                h.update(&[1]);
                h.update(&item.to_le_bytes());
                hash_u32(&mut h, at);
            }
            None => {
                h.update(&[0]);
            }
        }
        match t.subject {
            Some(subj) => {
                h.update(&[1]);
                h.update(&subj.item.to_le_bytes());
                hash_u32(&mut h, subj.count);
                for &d in subj.discriminants.iter() {
                    h.update(&d.to_le_bytes());
                }
            }
            None => {
                h.update(&[0]);
            }
        }
    }

    let lir_bytes = blaze_lir::persist::to_bytes_in(&view, scratch);
    hash_len(&mut h, lir_bytes.len());
    h.update(lir_bytes.as_slice());
    h.finish().0
}

/// One LTO-optimized and re-emitted function record for `blaze export`.
pub struct LtoBodyOut {
    /// Row index in `Walk`.
    pub row: usize,
    /// Instance symbol.
    pub symbol: Symbol,
    /// Encoded sealed canonical `CodeRecord` bytes (ready for `CodeView::read` and
    /// `materialize_positionless`).
    pub record: Vec<u8, SharedAlloc<'static>>,
    /// Encoded vtable datums referenced by this body or any of its inlined callees.
    pub vtable_data: Vec<(Symbol, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>>,
    /// Any `SoftFloat` routines that legalization called during emission, as `(tabled, name_bytes)`.
    pub extra_declared: Vec<(ItemKey, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>>,
    /// Count of callee bodies spliced into this function.
    pub inlined_bodies: usize,
}

/// Output of [`optimize_export_workspace`] / [`optimize_export_workspace_par`].
pub struct ExportLtoOut<'a> {
    /// Optimized functions replacing their walk rows in the exported binary.
    pub bodies: Vec<LtoBodyOut, SharedAlloc<'static>>,
    /// Any `Space::Func` targets referenced by post-LTO records or vtables that were not yet in
    /// `Walk`, as `(declarer_row, tabled, name_bytes)` in deterministic body/target order.
    pub extra_walk_instances: Vec<(usize, ItemKey, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>>,
    /// Any `Space::Static` targets referenced by post-LTO records or vtables that were not yet in
    /// `Walk`.
    pub extra_walk_statics: Vec<ItemKey, SharedAlloc<'static>>,
    /// Any `Space::Tls` targets referenced by post-LTO records that were not yet in `Walk`.
    pub extra_walk_tls: Vec<ItemKey, SharedAlloc<'static>>,
    /// Hot module symbols ranked hottest-first for `blaze_export::Profile::hot`
    /// (16 KB iTLB-page-aware Ext-TSP code packing).
    pub hot_symbols: AVec<'a, Symbol>,
    /// Deterministic BLAKE3 digest of the PGO profile and hot-symbol order for
    /// `blaze_export::Profile::digest`.
    pub profile_digest: [u8; 32],
    /// Total bodies gathered into the LTO module.
    pub module_bodies: usize,
    /// Total callee bodies spliced across the module.
    pub total_inlined: usize,
    /// Fine-grained sub-phase timings and incremental LTO cache hit/miss counts.
    pub times: ExportLtoTimes,
}

type SymIndexMap = Map<[u8; 32], usize, FixedState, SharedAlloc<'static>>;

struct LtoOrigin {
    row: usize,
    symbol: Symbol,
    is_seed: bool,
    orig_unreachables: usize,
    built: BuiltLir<SharedAlloc<'static>>,
    declared: Vec<DeclaredRow, SharedAlloc<'static>>,
}

struct LtoBodies<'b> {
    origins: &'b [LtoOrigin],
    by_sym: &'b SymIndexMap,
}

#[inline]
fn find_body_idx(by_sym: &SymIndexMap, sym: [u8; 32]) -> Option<usize> {
    by_sym.get(&sym).copied()
}

impl Bodies for LtoBodies<'_> {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, blaze_lir::View<'_>)> {
        let caller_idx = find_body_idx(self.by_sym, from.0)?;
        let caller = self.origins.get(caller_idx)?;
        let rec = caller.built.func.view().refs.get(reference.index())?;
        if RefKind::from_word(rec.kind)? != RefKind::Func {
            return None;
        }
        let place = usize::try_from(rec.place).ok()?;
        let BuiltCallee::Symbol(callee_sym) = *caller.built.funcs.get(place)? else {
            return None;
        };
        let callee_idx = find_body_idx(self.by_sym, callee_sym.0)?;
        let callee = self.origins.get(callee_idx)?;
        Some((BodyId(callee_sym.0), callee.built.func.view()))
    }

    fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
        let caller_idx = find_body_idx(self.by_sym, from.0)?;
        let caller = self.origins.get(caller_idx)?;
        let rec = caller.built.func.view().refs.get(reference.index())?;
        let kind = RefKind::from_word(rec.kind)?;
        let place = usize::try_from(rec.place).ok()?;
        match kind {
            RefKind::Func => match *caller.built.funcs.get(place)? {
                BuiltCallee::Symbol(sym) => Some(Target::Func(BodyId(sym.0))),
                BuiltCallee::C { .. } => None,
            },
            RefKind::Static => {
                let &item = caller.built.statics.get(place)?;
                if caller.built.foreign_static_name(item).is_some() {
                    None
                } else {
                    Some(Target::Datum(DatumId(static_symbol(item).0)))
                }
            }
            RefKind::Vtable => {
                let &sym = caller.built.vtables.get(place)?;
                Some(Target::Datum(DatumId(sym.0)))
            }
            RefKind::Tls => {
                let &item = caller.built.tls.get(place)?;
                Some(Target::Datum(DatumId(tls_symbol(item).0)))
            }
            RefKind::Data => None,
        }
    }

    fn body(&self, id: BodyId) -> Option<blaze_lir::View<'_>> {
        let idx = find_body_idx(self.by_sym, id.0)?;
        self.origins.get(idx).map(|o| o.built.func.view())
    }
}

/// Transitively resolves an `ImportedRef` (`start_idx`, `start_ref`) back through bottom-up SCC
/// inlining layers to its originating body index and pre-LTO `RefRec`.
fn resolve_ref_origin(
    start_idx: usize,
    start_ref: RefId,
    lto_bodies: &[OptimizedBody<SharedAlloc<'static>>],
    origins: &[LtoOrigin],
    by_sym: &SymIndexMap,
) -> Result<(usize, RefRec), ParLtoErr> {
    let start_sym = origins.get(start_idx).map_or([0u8; 32], |o| o.symbol.0);
    let mut cur_idx = start_idx;
    let mut cur_ref = start_ref;
    for _ in 0..=MAX_IMPORT_HOPS {
        let Some(opt) = lto_bodies.get(cur_idx) else {
            return Err(ParLtoErr::BadRef { symbol: start_sym, what: "LTO body index", ref_id: cur_ref.0 });
        };
        if let Ok(pos) = opt.imported_refs.binary_search_by_key(&cur_ref, |r| r.caller_ref)
            && let Some(imp) = opt.imported_refs.get(pos)
        {
            let Some(next_idx) = find_body_idx(by_sym, imp.source_body.0) else {
                return Err(ParLtoErr::UnresolvedImport { symbol: start_sym, source: imp.source_body.0 });
            };
            cur_idx = next_idx;
            cur_ref = imp.source_ref;
            continue;
        }
        let Some(orig) = origins.get(cur_idx) else {
            return Err(ParLtoErr::BadRef { symbol: start_sym, what: "origin body index", ref_id: cur_ref.0 });
        };
        let Some(&rec) = orig.built.func.view().refs.get(cur_ref.index()) else {
            return Err(ParLtoErr::BadRef { symbol: start_sym, what: "origin RefId", ref_id: cur_ref.0 });
        };
        return Ok((cur_idx, rec));
    }
    Err(ParLtoErr::ImportCycle { symbol: start_sym })
}

/// Writes `val` at `place` in `table` (overwriting an unused slot `< len` or appending at `len`).
fn put_at<T, A: core::alloc::Allocator>(
    table: &mut Vec<T, A>,
    place: usize,
    val: T,
    symbol: [u8; 32],
    what: &'static str,
    ref_id: u32,
) -> Result<(), ParLtoErr> {
    if let Some(slot) = table.get_mut(place) {
        *slot = val;
        return Ok(());
    }
    if place == table.len() {
        table.push(val);
        return Ok(());
    }
    Err(ParLtoErr::BadRef { symbol, what, ref_id })
}

/// Appends `(symbol, bytes)` to `out` if `symbol` is not already present.
fn push_vtable_datum(
    out: &mut Vec<(Symbol, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>>,
    symbol: Symbol,
    bytes: &[u8],
    alloc: SharedAlloc<'static>,
) {
    if out.iter().any(|(s, _)| *s == symbol) {
        return;
    }
    let mut copied: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(bytes.len(), alloc);
    copied.extend_from_slice(bytes);
    out.push((symbol, copied));
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct LibmOp {
    op: Op,
    ty: Ty,
    arity: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct LoweredLibmCall {
    op: Op,
    ty: Ty,
    arity: u8,
    args: [u32; 3],
    /// `Some(normal_blk)` when lowering an `Op::Invoke` terminator (emits `Op::Jump(normal_blk)`
    /// right after the native float op); `None` when lowering an `Op::Call`.
    invoke_normal: Option<u32>,
}

fn op_byte(op: Op) -> Option<u8> {
    let idx = Op::ALL.iter().position(|&o| o == op)?;
    u8::try_from(idx).ok()
}

/// Maps an IEEE-754 C libm symbol name (with or without a leading `_`) to its exact hardware LIR
/// op, gated strictly to ops supported by `blaze-emit`'s `sota` backend for `machine`.
///
/// (`Op::Fcopysign` is intentionally omitted because neither `sota/isel.rs` nor `sota/x86_64.rs`
/// supports it yet.)
fn match_ieee_libm(raw_name: &str, machine: Machine) -> Option<LibmOp> {
    let name = raw_name.strip_prefix('_').unwrap_or(raw_name);
    let candidate = match name {
        "fmaf" => LibmOp { op: Op::Fma, ty: Ty::F32, arity: 3 },
        "fma" => LibmOp { op: Op::Fma, ty: Ty::F64, arity: 3 },
        "sqrtf" => LibmOp { op: Op::Sqrt, ty: Ty::F32, arity: 1 },
        "sqrt" => LibmOp { op: Op::Sqrt, ty: Ty::F64, arity: 1 },
        "fabsf" => LibmOp { op: Op::Fabs, ty: Ty::F32, arity: 1 },
        "fabs" => LibmOp { op: Op::Fabs, ty: Ty::F64, arity: 1 },
        "floorf" => LibmOp { op: Op::Floor, ty: Ty::F32, arity: 1 },
        "floor" => LibmOp { op: Op::Floor, ty: Ty::F64, arity: 1 },
        "ceilf" => LibmOp { op: Op::Ceil, ty: Ty::F32, arity: 1 },
        "ceil" => LibmOp { op: Op::Ceil, ty: Ty::F64, arity: 1 },
        "truncf" => LibmOp { op: Op::Ftrunc, ty: Ty::F32, arity: 1 },
        "trunc" => LibmOp { op: Op::Ftrunc, ty: Ty::F64, arity: 1 },
        "rintf" | "nearbyintf" | "roundevenf" => LibmOp { op: Op::Nearest, ty: Ty::F32, arity: 1 },
        "rint" | "nearbyint" | "roundeven" => LibmOp { op: Op::Nearest, ty: Ty::F64, arity: 1 },
        "roundf" => LibmOp { op: Op::Fround, ty: Ty::F32, arity: 1 },
        "round" => LibmOp { op: Op::Fround, ty: Ty::F64, arity: 1 },
        _ => return None,
    };
    let supported = match machine.arch {
        Arch::Arm64 => true,
        Arch::X86_64 => candidate.op == Op::Fma && (machine.features & Machine::FMA) != 0,
    };
    if supported { Some(candidate) } else { None }
}

/// Checks whether instruction `inst` is a non-variadic `Conv::C` `Op::Call` or `Op::Invoke` to an
/// exact IEEE-754 libm function supported by `sota` on `machine` with matching signature and types.
fn detect_libm_call<A: core::alloc::Allocator>(built: &BuiltLir<A>, inst: usize, machine: Machine) -> Option<LoweredLibmCall> {
    let view = built.func.view();
    let op = Op::from_byte(*view.ops.get(inst)?)?;
    let is_invoke = match op {
        Op::Call => false,
        Op::Invoke => true,
        _ => return None,
    };
    let extra_start = usize::try_from(*view.data.get(inst)?).ok()?;
    let ref_word = *view.extra.get(extra_start)?;
    let sig_word = *view.extra.get(extra_start.checked_add(1)?)?;
    let (invoke_normal, args_len_pos) = if is_invoke {
        let normal_blk = *view.extra.get(extra_start.checked_add(2)?)?;
        let normal_idx = usize::try_from(normal_blk).ok()?;
        if normal_blk == 0 || normal_idx >= view.blocks() || normal_idx >= view.block_kinds.len() {
            return None;
        }
        let normal_block = Block(normal_blk);
        if view.block_kind(normal_block).is_landing() || !view.params(normal_block).is_empty() {
            return None;
        }
        (Some(normal_blk), extra_start.checked_add(4)?)
    } else {
        (None, extra_start.checked_add(2)?)
    };

    let ref_rec = view.refs.get(RefId(ref_word).index())?;
    if RefKind::from_word(ref_rec.kind) != Some(RefKind::Func) {
        return None;
    }
    let place_idx = usize::try_from(ref_rec.place).ok()?;
    let BuiltCallee::C { start, len } = *built.funcs.get(place_idx)? else {
        return None;
    };
    let c_name = built.c_str(start, len)?;
    let libm = match_ieee_libm(c_name, machine)?;
    let arity = usize::from(libm.arity);

    let sig_rec = view.sigs.get(SigId(sig_word).index())?;
    if Conv::from_byte(sig_rec.conv) != Some(Conv::C)
        || (sig_rec.bits & SigRec::VARIADIC) != 0
        || usize::from(sig_rec.params) != arity
        || sig_rec.rets != 1
    {
        return None;
    }
    let sig_first = usize::try_from(sig_rec.first).ok()?;
    let sig_ret_pos = sig_first.checked_add(arity)?;
    let sig_params = view.sig_params.get(sig_first..sig_ret_pos)?;
    let sig_ret = view.sig_params.get(sig_ret_pos)?;
    for p in sig_params {
        if Ty(p.ty) != libm.ty || ParamKind::from_byte(p.kind) != Some(ParamKind::Value) {
            return None;
        }
    }
    if Ty(sig_ret.ty) != libm.ty || ParamKind::from_byte(sig_ret.kind) != Some(ParamKind::Value) {
        return None;
    }

    if Ty(*view.tys.get(inst)?) != libm.ty {
        return None;
    }
    let argc = usize::try_from(*view.extra.get(args_len_pos)?).ok()?;
    if argc != arity {
        return None;
    }
    let args_start = args_len_pos.checked_add(1)?;
    let arg_words = view.extra.get(args_start..args_start.checked_add(arity)?)?;
    let mut args = [0u32; 3];
    for (k, &v) in arg_words.iter().enumerate() {
        let v_idx = usize::try_from(v).ok()?;
        if Ty(*view.tys.get(v_idx)?) != libm.ty {
            return None;
        }
        if let Some(slot) = args.get_mut(k) {
            *slot = v;
        }
    }

    Some(LoweredLibmCall {
        op: libm.op,
        ty: libm.ty,
        arity: libm.arity,
        args,
        invoke_normal,
    })
}

fn remap_val(val_map: &[u32], old: u32) -> Option<u32> {
    let idx = usize::try_from(old).ok()?;
    val_map.get(idx).copied()
}

fn copy_remapped_target<'a>(
    old_extra: &[u32],
    at: &mut usize,
    val_map: &[u32],
    out_extra: &mut AVec<'a, u32>,
) -> Option<()> {
    let block = *old_extra.get(*at)?;
    *at = at.checked_add(1)?;
    out_extra.push(block);
    let n_word = *old_extra.get(*at)?;
    *at = at.checked_add(1)?;
    out_extra.push(n_word);
    let n = usize::try_from(n_word).ok()?;
    for _ in 0..n {
        let v = *old_extra.get(*at)?;
        *at = at.checked_add(1)?;
        out_extra.push(remap_val(val_map, v)?);
    }
    Some(())
}

fn copy_remapped_extra<'a>(
    old_extra: &[u32],
    start: usize,
    fields: &[Field],
    ty: Ty,
    old_tys: &[u16],
    val_map: &[u32],
    out_extra: &mut AVec<'a, u32>,
) -> Option<()> {
    let mut at = start;
    let mut last_value: Option<u32> = None;
    for &field in fields {
        match field {
            Field::Value => {
                let v = *old_extra.get(at)?;
                at = at.checked_add(1)?;
                last_value = Some(v);
                out_extra.push(remap_val(val_map, v)?);
            }
            Field::Block
            | Field::Imm
            | Field::Offset
            | Field::IntCond
            | Field::FloatCond
            | Field::Rmw
            | Field::Ref
            | Field::Sig
            | Field::Slot
            | Field::Intrinsic
            | Field::Asm => {
                let w = *old_extra.get(at)?;
                at = at.checked_add(1)?;
                out_extra.push(w);
            }
            Field::Target => {
                copy_remapped_target(old_extra, &mut at, val_map, out_extra)?;
            }
            Field::Values => {
                let n_word = *old_extra.get(at)?;
                at = at.checked_add(1)?;
                out_extra.push(n_word);
                let n = usize::try_from(n_word).ok()?;
                for _ in 0..n {
                    let v = *old_extra.get(at)?;
                    at = at.checked_add(1)?;
                    out_extra.push(remap_val(val_map, v)?);
                }
            }
            Field::Blocks | Field::Mask => {
                let n_word = *old_extra.get(at)?;
                at = at.checked_add(1)?;
                out_extra.push(n_word);
                let n = usize::try_from(n_word).ok()?;
                for _ in 0..n {
                    let w = *old_extra.get(at)?;
                    at = at.checked_add(1)?;
                    out_extra.push(w);
                }
            }
            Field::Const => {
                let words = usize::try_from(ty.const_words()).ok()?;
                for _ in 0..words {
                    let w = *old_extra.get(at)?;
                    at = at.checked_add(1)?;
                    out_extra.push(w);
                }
            }
            Field::Cases => {
                let scrutinee = last_value?;
                let scrut_idx = usize::try_from(scrutinee).ok()?;
                let scrut_ty = Ty(*old_tys.get(scrut_idx)?);
                let width = usize::try_from(scrut_ty.const_words()).ok()?;
                let n_word = *old_extra.get(at)?;
                at = at.checked_add(1)?;
                out_extra.push(n_word);
                let n = usize::try_from(n_word).ok()?;
                for _ in 0..n {
                    for _ in 0..width {
                        let w = *old_extra.get(at)?;
                        at = at.checked_add(1)?;
                        out_extra.push(w);
                    }
                    copy_remapped_target(old_extra, &mut at, val_map, out_extra)?;
                }
            }
        }
    }
    Some(())
}

/// Lowers hardware-exact IEEE-754 libm `Op::Call` / `Op::Invoke` instructions in `built` to native
/// LIR float ops supported by `sota` on `machine`, rebuilding `extra` contiguously and preserving
/// SSA numbering across `Op::Invoke -> [Op::Fma/Sqrt/..., Op::Jump(normal)]` expansions.
///
/// Running this immediately after `maker.build_lir` turns `std::f32::mul_add` / `std::f64::mul_add`
/// / `sqrt` wrappers into tiny unwind-free pure leaves that `lto_optimize_module` inlines directly
/// into caller loops (exposing `Op::Fma` to `gemm.rs` SME2 `FMOPA.S`/`FMOPA.D`).
/// Returns `true` iff at least one call/invoke was lowered.
fn lower_ieee_libm_calls<A: core::alloc::Allocator>(built: &mut BuiltLir<A>, machine: Machine, alloc: Alloc<'_>) -> bool {
    let n = built.func.ops.len();
    if n == 0 || built.funcs.is_empty() || built.c_text.is_empty() {
        return false;
    }

    // Fast path: scan before allocating any rewrite buffers.
    let mut first_match: Option<(usize, LoweredLibmCall)> = None;
    for i in 0..n {
        if let Some(hit) = detect_libm_call(built, i, machine) {
            first_match = Some((i, hit));
            break;
        }
    }
    let Some((first_idx, first_hit)) = first_match else {
        return false;
    };

    let mut rewrites: AVec<'_, Option<LoweredLibmCall>> = Vec::with_capacity_in(n, alloc);
    rewrites.resize(n, None);
    if let Some(slot) = rewrites.get_mut(first_idx) {
        *slot = Some(first_hit);
    }
    let mut extra_insts = usize::from(first_hit.invoke_normal.is_some());
    for i in (first_idx + 1)..n {
        if let Some(hit) = detect_libm_call(built, i, machine) {
            if hit.invoke_normal.is_some() {
                extra_insts = extra_insts.saturating_add(1);
            }
            if let Some(slot) = rewrites.get_mut(i) {
                *slot = Some(hit);
            }
        }
    }

    let mut val_map: AVec<'_, u32> = Vec::with_capacity_in(n, alloc);
    let mut next_inst = 0u32;
    for i in 0..n {
        val_map.push(next_inst);
        let Some(step1) = next_inst.checked_add(1) else {
            return false;
        };
        next_inst = step1;
        if matches!(rewrites.get(i), Some(Some(r)) if r.invoke_normal.is_some()) {
            let Some(step2) = next_inst.checked_add(1) else {
                return false;
            };
            next_inst = step2;
        }
    }

    let Some(jump_byte) = op_byte(Op::Jump) else {
        return false;
    };
    let new_cap = n.saturating_add(extra_insts);
    let mut new_ops: AVec<'_, u8> = Vec::with_capacity_in(new_cap, alloc);
    let mut new_flags: AVec<'_, u8> = Vec::with_capacity_in(new_cap, alloc);
    let mut new_tys: AVec<'_, u16> = Vec::with_capacity_in(new_cap, alloc);
    let mut new_data: AVec<'_, u32> = Vec::with_capacity_in(new_cap, alloc);
    let mut new_extra: AVec<'_, u32> = Vec::with_capacity_in(built.func.extra.len(), alloc);
    let mut new_attrs: AVec<'_, u32> = Vec::with_capacity_in(new_cap, alloc);
    let mut new_locs: AVec<'_, u32> = Vec::with_capacity_in(new_cap, alloc);

    {
        let view = built.func.view();
        for i in 0..n {
            let Some(&old_loc) = view.locs.get(i) else {
                return false;
            };
            if let Some(Some(hit)) = rewrites.get(i).copied() {
                let Some(byte) = op_byte(hit.op) else {
                    return false;
                };
                let data_word = if hit.arity == 1 {
                    let [a0_old, ..] = hit.args;
                    let Some(a0) = remap_val(val_map.as_slice(), a0_old) else {
                        return false;
                    };
                    a0
                } else {
                    let Ok(pos) = u32::try_from(new_extra.len()) else {
                        return false;
                    };
                    for k in 0..usize::from(hit.arity) {
                        let Some(&arg) = hit.args.get(k) else {
                            return false;
                        };
                        let Some(mapped) = remap_val(val_map.as_slice(), arg) else {
                            return false;
                        };
                        new_extra.push(mapped);
                    }
                    pos
                };
                new_ops.push(byte);
                new_flags.push(Flags::NONE.0);
                new_tys.push(hit.ty.0);
                new_data.push(data_word);
                new_attrs.push(0);
                new_locs.push(old_loc);

                if let Some(normal_blk) = hit.invoke_normal {
                    let Ok(jump_pos) = u32::try_from(new_extra.len()) else {
                        return false;
                    };
                    new_ops.push(jump_byte);
                    new_flags.push(Flags::NONE.0);
                    new_tys.push(Ty::NONE.0);
                    new_data.push(jump_pos);
                    new_attrs.push(0);
                    new_locs.push(old_loc);
                    new_extra.push(normal_blk);
                    new_extra.push(0);
                }
                continue;
            }

            let Some(&old_op_byte) = view.ops.get(i) else {
                return false;
            };
            let Some(old_op) = Op::from_byte(old_op_byte) else {
                return false;
            };
            let Some(&old_flag) = view.flags.get(i) else {
                return false;
            };
            let Some(&old_ty_raw) = view.tys.get(i) else {
                return false;
            };
            let Some(&old_data) = view.data.get(i) else {
                return false;
            };
            let Some(&old_attr) = view.attrs.get(i) else {
                return false;
            };

            let new_data_word = match old_op.info().layout {
                Layout::None => 0,
                Layout::Data(Field::Value) => {
                    let Some(mapped) = remap_val(val_map.as_slice(), old_data) else {
                        return false;
                    };
                    mapped
                }
                Layout::Data(_) => old_data,
                Layout::Extra(fields) => {
                    let Ok(pos) = u32::try_from(new_extra.len()) else {
                        return false;
                    };
                    let Ok(old_start) = usize::try_from(old_data) else {
                        return false;
                    };
                    if copy_remapped_extra(
                        view.extra,
                        old_start,
                        fields,
                        Ty(old_ty_raw),
                        view.tys,
                        val_map.as_slice(),
                        &mut new_extra,
                    )
                    .is_none()
                    {
                        return false;
                    }
                    pos
                }
            };

            new_ops.push(old_op_byte);
            new_flags.push(old_flag);
            new_tys.push(old_ty_raw);
            new_data.push(new_data_word);
            new_attrs.push(old_attr);
            new_locs.push(old_loc);
        }
    }

    let mut new_block_starts: AVec<'_, u32> = Vec::with_capacity_in(built.func.block_starts.len(), alloc);
    for &old_start in built.func.block_starts.iter() {
        let Some(mapped) = remap_val(val_map.as_slice(), old_start) else {
            return false;
        };
        new_block_starts.push(mapped);
    }

    built.func.ops.clear();
    built.func.ops.extend_from_slice(new_ops.as_slice());
    built.func.flags.clear();
    built.func.flags.extend_from_slice(new_flags.as_slice());
    built.func.tys.clear();
    built.func.tys.extend_from_slice(new_tys.as_slice());
    built.func.data.clear();
    built.func.data.extend_from_slice(new_data.as_slice());
    built.func.extra.clear();
    built.func.extra.extend_from_slice(new_extra.as_slice());
    built.func.attrs.clear();
    built.func.attrs.extend_from_slice(new_attrs.as_slice());
    built.func.locs.clear();
    built.func.locs.extend_from_slice(new_locs.as_slice());
    built.func.block_starts.clear();
    built.func.block_starts.extend_from_slice(new_block_starts.as_slice());
    true
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct LoopHeat {
    max_depth: u32,
    loop_blocks: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum HotTier {
    /// Other hot functions (loops or persisted PGO weight above baseline).
    Other = 0,
    /// Non-cold normal-returning direct callers of at least one hot 0-call leaf kernel.
    HotLeafCaller = 1,
    /// Hot 0-call leaf kernels (packed first into pages 0..2).
    HotLeaf = 2,
}

#[derive(Clone, Copy)]
struct BodyHeat {
    symbol: Symbol,
    is_seed: bool,
    eligible: bool,
    can_receive_caller_bonus: bool,
    is_hot_leaf: bool,
    has_own_heat: bool,
    own_score: u64,
}

#[derive(Clone, Copy)]
struct HotCandidate {
    symbol: Symbol,
    tier: HotTier,
    score: u64,
    is_seed: bool,
}

/// Builds the module's PGO weight table from static cold-start estimates (computed in parallel
/// via `runner`), exponentially decay-merged with an explicit persisted `BPG1` record when
/// provided (Rule 86).
fn build_pgo_table_par<R: LtoRunner>(
    mod_bodies: &[ModuleBody<SharedAlloc<'static>>],
    pgo_bytes: Option<&[u8]>,
    alloc: SharedAlloc<'static>,
    runner: &R,
) -> Result<PgoSiteTable<SharedAlloc<'static>>, ExportLtoError> {
    let static_pgo = PgoSiteTable::<SharedAlloc<'static>>::estimate_static_par(mod_bodies, alloc, runner)
        .map_err(ExportLtoError::Optimize)?;
    match pgo_bytes {
        Some(bytes) => {
            let persisted = PgoSiteTable::decode(bytes, alloc).map_err(ExportLtoError::PgoRecord)?;
            Ok(persisted.merge_decayed(&static_pgo, PGO_DECAY_NUM, PGO_DECAY_DEN, alloc))
        }
        None => Ok(static_pgo),
    }
}

#[cfg(test)]
fn build_pgo_table<A: core::alloc::Allocator + Copy>(
    mod_bodies: &[ModuleBody<A>],
    pgo_bytes: Option<&[u8]>,
    alloc: A,
) -> Result<PgoSiteTable<A>, ExportLtoError> {
    let static_pgo = PgoSiteTable::estimate_static(mod_bodies, alloc);
    match pgo_bytes {
        Some(bytes) => {
            let persisted = PgoSiteTable::decode(bytes, alloc).map_err(ExportLtoError::PgoRecord)?;
            Ok(persisted.merge_decayed(&static_pgo, PGO_DECAY_NUM, PGO_DECAY_DEN, alloc))
        }
        None => Ok(static_pgo),
    }
}

fn for_each_normal_succ(view: blaze_lir::View<'_>, b: usize, mut visit: impl FnMut(usize)) {
    let Ok(b_u32) = u32::try_from(b) else {
        return;
    };
    let block = Block(b_u32);
    if view.block_kind(block) != BlockKind::Normal {
        return;
    }
    let range = view.block(block);
    let Some(at) = range.end.checked_sub(1) else {
        return;
    };
    if at < range.start {
        return;
    }
    let op = view.op(at);
    let Layout::Extra(fields) = op.info().layout else {
        return;
    };
    let Some(&data_at) = view.data.get(at) else {
        return;
    };
    let Ok(extra_start) = usize::try_from(data_at) else {
        return;
    };
    let Some(words) = view.extra.get(extra_start..) else {
        return;
    };
    let Some(&ty_raw) = view.tys.get(at) else {
        return;
    };
    let mut on_word = |word: Word| -> Result<(), core::convert::Infallible> {
        if let Word::Block(to) = word
            && let Ok(to_idx) = usize::try_from(to)
            && to_idx < view.blocks()
            && view.block_kind(Block(to)) == BlockKind::Normal
        {
            visit(to_idx);
        }
        Ok(())
    };
    let _ = walk(words, fields, Ty(ty_raw), view.tys, &mut on_word);
}

#[inline]
fn is_dfs_descendant(dfs_in: &[u32], dfs_out: &[u32], ancestor: usize, node: usize) -> bool {
    let (Some(&a_in), Some(&a_out), Some(&n_in), Some(&n_out)) = (
        dfs_in.get(ancestor),
        dfs_out.get(ancestor),
        dfs_in.get(node),
        dfs_out.get(node),
    ) else {
        return false;
    };
    a_in != u32::MAX && n_in != u32::MAX && a_in <= n_in && n_out <= a_out
}

/// Computes static loop nesting depth and loop block count over reachable `BlockKind::Normal`
/// blocks of `view` using iterative DFS back-edge detection and bounded natural-loop reverse BFS.
fn compute_loop_heat<'s>(view: blaze_lir::View<'_>, scratch: Alloc<'s>) -> LoopHeat {
    let blocks = view.blocks();
    if blocks == 0 || view.block_kind(Block(0)) != BlockKind::Normal {
        return LoopHeat { max_depth: 0, loop_blocks: 0 };
    }

    // 1. Build deduplicated forward CSR adjacency over normal blocks.
    let mut succ_head: AVec<'s, usize> = Vec::with_capacity_in(blocks.saturating_add(1), scratch);
    let mut succ_edges: AVec<'s, usize> = Vec::with_capacity_in(blocks.saturating_mul(2), scratch);
    succ_head.push(0);
    for b in 0..blocks {
        let edge_start = succ_edges.len();
        for_each_normal_succ(view, b, |to| succ_edges.push(to));
        if let Some(slice) = succ_edges.get_mut(edge_start..) {
            slice.sort_unstable();
        }
        let mut write = edge_start;
        for read in edge_start..succ_edges.len() {
            let Some(&edge) = succ_edges.get(read) else {
                break;
            };
            let is_dup = write > edge_start
                && write.checked_sub(1).and_then(|prev| succ_edges.get(prev)).copied() == Some(edge);
            if !is_dup {
                if let Some(slot) = succ_edges.get_mut(write) {
                    *slot = edge;
                }
                write = write.saturating_add(1);
            }
        }
        succ_edges.truncate(write);
        succ_head.push(succ_edges.len());
    }

    // 2. Iterative DFS from entry block `0` to compute `(dfs_in, dfs_out)` intervals.
    let mut dfs_in: AVec<'s, u32> = Vec::with_capacity_in(blocks, scratch);
    dfs_in.resize(blocks, u32::MAX);
    let mut dfs_out: AVec<'s, u32> = Vec::with_capacity_in(blocks, scratch);
    dfs_out.resize(blocks, 0);
    let mut stack: AVec<'s, (usize, usize)> = Vec::with_capacity_in(blocks, scratch);
    let mut time = 0u32;
    if let Some(slot) = dfs_in.get_mut(0) {
        *slot = time;
        time = time.saturating_add(1);
    }
    stack.push((0usize, succ_head.first().copied().unwrap_or(0)));
    while let Some((u, edge_idx)) = stack.last().copied() {
        let end = succ_head.get(u.saturating_add(1)).copied().unwrap_or(0);
        if edge_idx < end {
            if let Some(top) = stack.last_mut() {
                top.1 = edge_idx.saturating_add(1);
            }
            if let Some(&v) = succ_edges.get(edge_idx)
                && dfs_in.get(v).copied() == Some(u32::MAX)
            {
                if let Some(slot) = dfs_in.get_mut(v) {
                    *slot = time;
                    time = time.saturating_add(1);
                }
                let v_start = succ_head.get(v).copied().unwrap_or(0);
                stack.push((v, v_start));
            }
        } else {
            if let Some(slot) = dfs_out.get_mut(u) {
                *slot = time;
                time = time.saturating_add(1);
            }
            stack.pop();
        }
    }

    // 3. Gather DFS back-edges `(header, latch)` sorted by header for deterministic merging.
    let mut back_edges: AVec<'s, (usize, usize)> = Vec::new_in(scratch);
    for u in 0..blocks {
        if dfs_in.get(u).copied().unwrap_or(u32::MAX) == u32::MAX {
            continue;
        }
        let start = succ_head.get(u).copied().unwrap_or(0);
        let end = succ_head.get(u.saturating_add(1)).copied().unwrap_or(start);
        if let Some(slice) = succ_edges.get(start..end) {
            for &h in slice {
                if is_dfs_descendant(dfs_in.as_slice(), dfs_out.as_slice(), h, u) {
                    back_edges.push((h, u));
                }
            }
        }
    }
    if back_edges.is_empty() {
        return LoopHeat { max_depth: 0, loop_blocks: 0 };
    }
    back_edges.sort_unstable();

    // 4. Build reverse CSR predecessor table for bounded natural-loop discovery.
    let mut pred_count: AVec<'s, usize> = Vec::with_capacity_in(blocks, scratch);
    pred_count.resize(blocks, 0);
    for &v in succ_edges.iter() {
        if let Some(c) = pred_count.get_mut(v) {
            *c = c.saturating_add(1);
        }
    }
    let mut pred_head: AVec<'s, usize> = Vec::with_capacity_in(blocks.saturating_add(1), scratch);
    let mut running = 0usize;
    pred_head.push(0);
    for &c in pred_count.iter() {
        running = running.saturating_add(c);
        pred_head.push(running);
    }
    let mut pred_cursor: AVec<'s, usize> = Vec::with_capacity_in(blocks, scratch);
    if let Some(starts) = pred_head.get(..blocks) {
        pred_cursor.extend_from_slice(starts);
    }
    let mut pred_edges: AVec<'s, usize> = Vec::with_capacity_in(succ_edges.len(), scratch);
    pred_edges.resize(succ_edges.len(), 0);
    for u in 0..blocks {
        let start = succ_head.get(u).copied().unwrap_or(0);
        let end = succ_head.get(u.saturating_add(1)).copied().unwrap_or(start);
        if let Some(slice) = succ_edges.get(start..end) {
            for &v in slice {
                if let Some(cur) = pred_cursor.get_mut(v) {
                    let pos = *cur;
                    *cur = pos.saturating_add(1);
                    if let Some(slot) = pred_edges.get_mut(pos) {
                        *slot = u;
                    }
                }
            }
        }
    }

    // 5. Accumulate per-block natural loop nesting depth across distinct loop headers.
    let mut depth: AVec<'s, u32> = Vec::with_capacity_in(blocks, scratch);
    depth.resize(blocks, 0);
    let mut seen_stamp: AVec<'s, u32> = Vec::with_capacity_in(blocks, scratch);
    seen_stamp.resize(blocks, 0);
    let mut worklist: AVec<'s, usize> = Vec::with_capacity_in(blocks, scratch);

    let mut stamp = 0u32;
    let mut idx = 0usize;
    let mut headers_done = 0usize;
    while idx < back_edges.len() && headers_done < MAX_LOOP_HEADERS {
        let Some((h, _)) = back_edges.get(idx).copied() else {
            break;
        };
        stamp = stamp.saturating_add(1);
        headers_done = headers_done.saturating_add(1);
        worklist.clear();

        if let Some(s) = seen_stamp.get_mut(h) {
            *s = stamp;
        }
        if let Some(d) = depth.get_mut(h) {
            *d = d.saturating_add(1);
        }

        while let Some((bh, latch)) = back_edges.get(idx).copied() {
            if bh != h {
                break;
            }
            idx = idx.saturating_add(1);
            if seen_stamp.get(latch).copied() != Some(stamp) {
                if let Some(s) = seen_stamp.get_mut(latch) {
                    *s = stamp;
                }
                if let Some(d) = depth.get_mut(latch) {
                    *d = d.saturating_add(1);
                }
                worklist.push(latch);
            }
        }

        let mut wl_head = 0usize;
        while let Some(cur) = worklist.get(wl_head).copied() {
            wl_head = wl_head.saturating_add(1);
            let p_start = pred_head.get(cur).copied().unwrap_or(0);
            let p_end = pred_head.get(cur.saturating_add(1)).copied().unwrap_or(p_start);
            if let Some(preds) = pred_edges.get(p_start..p_end) {
                for &p in preds {
                    if seen_stamp.get(p).copied() != Some(stamp)
                        && is_dfs_descendant(dfs_in.as_slice(), dfs_out.as_slice(), h, p)
                    {
                        if let Some(s) = seen_stamp.get_mut(p) {
                            *s = stamp;
                        }
                        if let Some(d) = depth.get_mut(p) {
                            *d = d.saturating_add(1);
                        }
                        worklist.push(p);
                    }
                }
            }
        }
    }

    let mut max_depth = 0u32;
    let mut loop_blocks = 0u64;
    for &d in depth.iter() {
        if d > 0 {
            loop_blocks = loop_blocks.saturating_add(1);
            if d > max_depth {
                max_depth = d;
            }
        }
    }
    LoopHeat { max_depth, loop_blocks }
}

/// Returns `true` iff `view` has at least one `BlockKind::Normal` block that returns normally
/// (or tail-calls), excluding panic/abort-only wrappers whose normal blocks only terminate in
/// `Op::Trap`, `Op::Unreachable`, or `Op::Resume`.
fn has_normal_return(view: blaze_lir::View<'_>) -> bool {
    for b in 0..view.blocks() {
        let Ok(b_u32) = u32::try_from(b) else {
            break;
        };
        let block = Block(b_u32);
        if view.block_kind(block) != BlockKind::Normal {
            continue;
        }
        let range = view.block(block);
        if let Some(term) = range.end.checked_sub(1)
            && term >= range.start
            && matches!(
                view.op(term),
                Op::Return | Op::TailCall | Op::TailCallIndirect
            )
        {
            return true;
        }
    }
    false
}

/// Returns `true` iff `view` has no non-cold calls/invokes/tail-calls in normal non-panic blocks.
fn is_zero_call_leaf(view: blaze_lir::View<'_>) -> bool {
    for b in 0..view.blocks() {
        let Ok(b_u32) = u32::try_from(b) else {
            break;
        };
        let block = Block(b_u32);
        if view.block_kind(block) != BlockKind::Normal {
            continue;
        }
        let range = view.block(block);
        if let Some(term) = range.end.checked_sub(1)
            && term >= range.start
            && matches!(view.op(term), Op::Trap | Op::Unreachable | Op::Resume)
        {
            continue;
        }
        for at in range {
            if matches!(
                view.op(at),
                Op::Call
                    | Op::CallIndirect
                    | Op::Invoke
                    | Op::InvokeIndirect
                    | Op::TailCall
                    | Op::TailCallIndirect
            ) && !view.flags(at).contains(Flags::COLD)
            {
                return false;
            }
        }
    }
    true
}

/// Collects direct non-cold post-LTO call edges `(caller_sym, callee_sym)` within the LTO module,
/// resolving both original and inliner-imported `RefId`s back to their originating `BuiltCallee`.
fn collect_post_lto_call_edges<'s>(
    origins: &[LtoOrigin],
    lto_bodies: &[OptimizedBody<SharedAlloc<'static>>],
    by_sym: &SymIndexMap,
    scratch: Alloc<'s>,
) -> AVec<'s, (Symbol, Symbol)> {
    let mut edges: AVec<'s, (Symbol, Symbol)> = Vec::new_in(scratch);
    for (idx, (orig, opt)) in origins.iter().zip(lto_bodies.iter()).enumerate() {
        let view = opt.func.view();
        for b in 0..view.blocks() {
            let Ok(b_u32) = u32::try_from(b) else {
                break;
            };
            let block = Block(b_u32);
            if view.block_kind(block) != BlockKind::Normal {
                continue;
            }
            let range = view.block(block);
            if let Some(term) = range.end.checked_sub(1)
                && term >= range.start
                && matches!(view.op(term), Op::Trap | Op::Unreachable | Op::Resume)
            {
                continue;
            }
            for at in range {
                let op = view.op(at);
                if !matches!(op, Op::Call | Op::Invoke | Op::TailCall) {
                    continue;
                }
                if view.flags(at).contains(Flags::COLD) {
                    continue;
                }
                let Some(&data_at) = view.data.get(at) else {
                    continue;
                };
                let Ok(extra_idx) = usize::try_from(data_at) else {
                    continue;
                };
                let Some(&ref_word) = view.extra.get(extra_idx) else {
                    continue;
                };
                let ref_id = RefId(ref_word);
                let Some(rec) = view.refs.get(ref_id.index()) else {
                    continue;
                };
                if RefKind::from_word(rec.kind) != Some(RefKind::Func) {
                    continue;
                }
                let Ok((origin_idx, origin_rec)) = resolve_ref_origin(
                    idx,
                    ref_id,
                    lto_bodies,
                    origins,
                    by_sym,
                ) else {
                    continue;
                };
                if RefKind::from_word(origin_rec.kind) != Some(RefKind::Func) {
                    continue;
                }
                let Ok(src_place) = usize::try_from(origin_rec.place) else {
                    continue;
                };
                let Some(src_origin) = origins.get(origin_idx) else {
                    continue;
                };
                let Some(&BuiltCallee::Symbol(callee_sym)) = src_origin.built.funcs.get(src_place) else {
                    continue;
                };
                if callee_sym == orig.symbol {
                    continue;
                }
                if find_body_idx(by_sym, callee_sym.0).is_some() {
                    edges.push((orig.symbol, callee_sym));
                }
            }
        }
    }
    edges
}

/// Ranks hot module bodies deterministically (Rule 25) for `blaze_export::Profile::hot`.
///
/// Cold and noreturn bodies are excluded. Pass 1 computes each body's own static LIR loop
/// nesting heat (`10^min(max_depth, 6) * loop_blocks`) plus `pgo.body_weight(id)`. Pass 2
/// performs deterministic 1-hop hot-caller heat propagation over `call_edges`: any non-cold
/// normal-returning function directly calling a hot 0-call leaf kernel (`own_score >=
/// HOT_LEAF_THRESHOLD`) receives `callee_heat / 4` (deduplicated per callee and capped safely
/// below its hottest called leaf). Ordering packs `HotLeaf` kernels first (pages 0..2), direct
/// `HotLeafCaller`s immediately after (pages 1..4), then remaining hot bodies, breaking ties by
/// `score` descending, `is_seed` descending, then `Symbol` bytes ascending.
#[cfg(test)]
fn rank_hot_symbols<'a, 's, A: core::alloc::Allocator + Copy>(
    bodies: &[(Symbol, bool, blaze_lir::View<'_>)],
    call_edges: &[(Symbol, Symbol)],
    pgo: &PgoSiteTable<A>,
    alloc: Alloc<'a>,
    scratch: Alloc<'s>,
) -> AVec<'a, Symbol> {
    let mut heats: AVec<'s, LoopHeat> = Vec::with_capacity_in(bodies.len(), scratch);
    for &(_, _, view) in bodies.iter() {
        let is_cold_or_noreturn = (view.head.attrs & (FnAttrs::COLD | FnAttrs::NORETURN)) != 0;
        heats.push(if is_cold_or_noreturn {
            LoopHeat { max_depth: 0, loop_blocks: 0 }
        } else {
            compute_loop_heat(view, scratch)
        });
    }
    rank_hot_symbols_with_heats(bodies, heats.as_slice(), call_edges, pgo, alloc, scratch)
}

fn rank_hot_symbols_with_heats<'a, 's, A: core::alloc::Allocator + Copy>(
    bodies: &[(Symbol, bool, blaze_lir::View<'_>)],
    heats: &[LoopHeat],
    call_edges: &[(Symbol, Symbol)],
    pgo: &PgoSiteTable<A>,
    alloc: Alloc<'a>,
    scratch: Alloc<'s>,
) -> AVec<'a, Symbol> {
    let n = bodies.len();
    let mut sym_index: Map<[u8; 32], usize, FixedState, Alloc<'s>> =
        Map::new(MapOpts { hasher: FixedState, alloc: scratch, capacity: Some(n) });
    let mut body_heats: AVec<'s, BodyHeat> = Vec::with_capacity_in(n, scratch);

    for (idx, &(symbol, is_seed, view)) in bodies.iter().enumerate() {
        sym_index.insert(symbol.0, idx);
        let is_cold = (view.head.attrs & FnAttrs::COLD) != 0;
        let is_noreturn = (view.head.attrs & FnAttrs::NORETURN) != 0;
        if is_cold || is_noreturn {
            body_heats.push(BodyHeat {
                symbol,
                is_seed,
                eligible: false,
                can_receive_caller_bonus: false,
                is_hot_leaf: false,
                has_own_heat: false,
                own_score: 0,
            });
            continue;
        }
        let pgo_w = pgo.body_weight(BodyId(symbol.0));
        let heat = heats.get(idx).copied().unwrap_or(LoopHeat { max_depth: 0, loop_blocks: 0 });
        let loop_score = if heat.max_depth > 0 {
            10u64
                .saturating_pow(heat.max_depth.min(MAX_LOOP_DEPTH_EXP))
                .saturating_mul(heat.loop_blocks)
        } else {
            0
        };
        let own_score = loop_score.saturating_add(pgo_w);
        let has_own_heat = heat.max_depth > 0 || pgo_w > STATIC_STRAIGHT_LINE_WEIGHT;
        let zero_call = is_zero_call_leaf(view);
        let is_hot_leaf = zero_call && has_own_heat && own_score >= HOT_LEAF_THRESHOLD;
        let can_receive_caller_bonus = !zero_call && has_normal_return(view);
        body_heats.push(BodyHeat {
            symbol,
            is_seed,
            eligible: true,
            can_receive_caller_bonus,
            is_hot_leaf,
            has_own_heat,
            own_score,
        });
    }

    let mut hot_edges: AVec<'s, (usize, usize)> = Vec::new_in(scratch);
    for &(caller_sym, callee_sym) in call_edges.iter() {
        if caller_sym == callee_sym {
            continue;
        }
        let Some(&caller_idx) = sym_index.get(&caller_sym.0) else {
            continue;
        };
        let Some(&callee_idx) = sym_index.get(&callee_sym.0) else {
            continue;
        };
        let caller_ok = body_heats.get(caller_idx).is_some_and(|b| b.can_receive_caller_bonus);
        let callee_hot = body_heats.get(callee_idx).is_some_and(|b| b.is_hot_leaf);
        if caller_ok && callee_hot {
            hot_edges.push((caller_idx, callee_idx));
        }
    }
    hot_edges.sort_unstable();

    let mut caller_bonus: AVec<'s, u64> = Vec::with_capacity_in(n, scratch);
    caller_bonus.resize(n, 0);
    let mut caller_max_callee: AVec<'s, u64> = Vec::with_capacity_in(n, scratch);
    caller_max_callee.resize(n, 0);

    let mut prev_edge: Option<(usize, usize)> = None;
    for &(caller_idx, callee_idx) in hot_edges.iter() {
        if prev_edge == Some((caller_idx, callee_idx)) {
            continue;
        }
        prev_edge = Some((caller_idx, callee_idx));
        let Some(callee_heat) = body_heats.get(callee_idx).map(|b| b.own_score) else {
            continue;
        };
        let delta = callee_heat / HOT_CALLER_HEAT_DIV;
        if delta == 0 {
            continue;
        }
        if let Some(slot) = caller_bonus.get_mut(caller_idx) {
            *slot = slot.saturating_add(delta);
        }
        if let Some(max_slot) = caller_max_callee.get_mut(caller_idx)
            && callee_heat > *max_slot
        {
            *max_slot = callee_heat;
        }
    }

    let mut ranked: AVec<'s, HotCandidate> = Vec::with_capacity_in(n, scratch);
    for idx in 0..n {
        let Some(&info) = body_heats.get(idx) else {
            continue;
        };
        if !info.eligible {
            continue;
        }
        let raw_bonus = caller_bonus.get(idx).copied().unwrap_or(0);
        let max_callee = caller_max_callee.get(idx).copied().unwrap_or(0);
        let bonus_cap = max_callee.saturating_sub(max_callee / HOT_CALLER_HEAT_DIV);
        let bonus = raw_bonus.min(bonus_cap);
        if !info.has_own_heat && bonus == 0 {
            continue;
        }
        let tier = if info.is_hot_leaf {
            HotTier::HotLeaf
        } else if bonus > 0 {
            HotTier::HotLeafCaller
        } else {
            HotTier::Other
        };
        let score = info.own_score.saturating_add(bonus);
        ranked.push(HotCandidate { symbol: info.symbol, tier, score, is_seed: info.is_seed });
    }

    ranked.sort_unstable_by(|a, b| {
        b.tier
            .cmp(&a.tier)
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| b.is_seed.cmp(&a.is_seed))
            .then_with(|| a.symbol.0.cmp(&b.symbol.0))
    });
    let mut hot_symbols: AVec<'a, Symbol> = Vec::with_capacity_in(ranked.len(), alloc);
    for item in ranked.iter() {
        hot_symbols.push(item.symbol);
    }
    hot_symbols
}

/// Computes a deterministic BLAKE3 digest over the active `PgoSiteTable` and ranked `hot_symbols`
/// for `blaze_export::Profile::digest` (Rule 21, Rule 25).
fn compute_profile_digest<A: core::alloc::Allocator + Copy>(
    pgo: &PgoSiteTable<A>,
    hot_symbols: &[Symbol],
    alloc: A,
) -> [u8; 32] {
    let encoded = pgo.encode(alloc);
    let mut hasher = blaze_ids::Hasher::new(EXPORT_PGO_PROFILE_DOMAIN);
    hasher.update(encoded.as_slice());
    let count = u64::try_from(hot_symbols.len()).unwrap_or(u64::MAX);
    hasher.update(&count.to_le_bytes());
    for sym in hot_symbols.iter() {
        hasher.update(&sym.0);
    }
    hasher.finish().0
}

/// Prepared post-LTO surviving body ready for whole-module Zero-ABI planning and parallel
/// wavefront bottom-up Tarjan SCC IPRA emission (`Send + Sync` via `SharedAlloc`).
struct PreparedBody<'o, 'm> {
    row: usize,
    symbol: Symbol,
    inlined_bodies: usize,
    is_hot: bool,
    verified: blaze_lir::Verified<'o>,
    funcs: Vec<Callee<'o>, SharedAlloc<'static>>,
    statics: Vec<ItemKey, SharedAlloc<'static>>,
    c_statics: Vec<crate::code_records::ForeignStatic<'o>, SharedAlloc<'static>>,
    vtables: Vec<Symbol, SharedAlloc<'static>>,
    vtable_data: Vec<(Symbol, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>>,
    tls: Vec<ItemKey, SharedAlloc<'static>>,
    traps: &'o [TrapSource],
    routines: Routines<'m>,
}

/// Emitted sealed `CodeRecord` bytes and called soft-float routine declarations for one body.
struct EmittedBodyRec {
    record: Vec<u8, SharedAlloc<'static>>,
    extra_declared: Vec<(ItemKey, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>>,
}

/// Returns `true` iff `view` can soundly use `CallConvKind::ColdPreserveAll` without breaking
/// compact/DWARF unwinding or tail-call lowering:
/// - no `Op::Invoke` / `Op::InvokeIndirect` / `Op::Resume` / non-normal landing-pad blocks,
/// - no `Op::TailCall` / `Op::TailCallIndirect`,
/// - no inline `Op::Asm` / `Op::AsmBr`, and
/// - either explicitly `FnAttrs::NOUNWIND` or a strict zero-call leaf that cannot unwind.
fn is_safe_cold_preserve_all(view: blaze_lir::View<'_>) -> bool {
    for b in 0..view.blocks() {
        let Ok(b_u32) = u32::try_from(b) else {
            return false;
        };
        if view.block_kind(Block(b_u32)) != BlockKind::Normal {
            return false;
        }
    }
    let mut has_call = false;
    for inst in 0..view.len() {
        match view.op(inst) {
            Op::Invoke
            | Op::InvokeIndirect
            | Op::Resume
            | Op::TailCall
            | Op::TailCallIndirect
            | Op::Asm
            | Op::AsmBr => return false,
            Op::Call | Op::CallIndirect => {
                has_call = true;
            }
            _ => {}
        }
    }
    !has_call || (view.head.attrs & FnAttrs::NOUNWIND) != 0
}

/// Returns `true` iff `view`'s own signature has more than 8 scalar GPR or more than 8 FP/NEON
/// arguments (excluding `ParamKind::Sret`), so `CallConvKind::InternalFast` actually promotes
/// stacked arguments into `X9..X15` / `V16..V23`.
fn has_wide_args(view: blaze_lir::View<'_>) -> bool {
    let sig = view.sig(SigId(view.head.sig));
    let mut gpr = 0u32;
    let mut fp = 0u32;
    for p in sig.params {
        if ParamKind::from_byte(p.kind) == Some(ParamKind::Sret) {
            continue;
        }
        let ty = Ty(p.ty);
        if ty.is_float() || ty.is_vector() {
            fp += 1;
        } else {
            gpr += 1;
        }
    }
    gpr > 8 || fp > 8
}

/// Estimates caller live-register pressure (`(gpr_pressure, fp_pressure)`) from post-LTO LIR
/// before emission so [`plan_module_abi`] can assign interprocedural register-avoidance
/// [`ScratchOrder`]s to callees prior to code generation.
fn estimate_caller_live_pressure(view: blaze_lir::View<'_>) -> (RegSet, RegSet) {
    const SCRATCH_GPRS: [u8; 7] = [9, 10, 11, 12, 13, 14, 15];
    const SCRATCH_FPS: [u8; 13] = [16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28];

    let mut last_call: Option<usize> = None;
    for inst in 0..view.len() {
        if matches!(
            view.op(inst),
            Op::Call
                | Op::Invoke
                | Op::CallIndirect
                | Op::InvokeIndirect
                | Op::TailCall
                | Op::TailCallIndirect
        ) {
            last_call = Some(inst);
        }
    }
    let Some(last_call_idx) = last_call else {
        return (RegSet::EMPTY, RegSet::EMPTY);
    };

    let mut gpr_args = 0usize;
    let mut fp_args = 0usize;
    let mut gpr_temps = 0usize;
    let mut fp_temps = 0usize;
    let entry_params = view.params(Block(0));
    for inst in 0..last_call_idx {
        let Some(&ty_raw) = view.tys.get(inst) else {
            break;
        };
        let ty = Ty(ty_raw);
        if ty == Ty::NONE {
            continue;
        }
        let op = view.op(inst);
        if matches!(op, Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide) {
            continue;
        }
        let is_entry_param = inst >= entry_params.start && inst < entry_params.end;
        if ty.is_float() || ty.is_vector() {
            if is_entry_param {
                fp_args += 1;
            } else {
                fp_temps += 1;
            }
        } else if is_entry_param {
            gpr_args += 1;
        } else {
            gpr_temps += 1;
        }
    }

    let mut gpr_set = RegSet::EMPTY;
    for &r in SCRATCH_GPRS.get(..gpr_temps.min(4)).unwrap_or(&[]) {
        gpr_set = gpr_set.with(Reg(r));
    }
    for r in 0..u8::try_from(gpr_args.min(4)).unwrap_or(0) {
        gpr_set = gpr_set.with(Reg(r));
    }

    let mut fp_set = RegSet::EMPTY;
    for &v in SCRATCH_FPS.get(..fp_temps.min(6)).unwrap_or(&[]) {
        fp_set = fp_set.with(Reg(32 + v));
    }
    for v in 0..u8::try_from(fp_args.min(4)).unwrap_or(0) {
        fp_set = fp_set.with(Reg(32 + v));
    }
    (gpr_set, fp_set)
}

/// 2-tier hot/cold classification for post-LTO emission using a precomputed [`LoopHeat`] so
/// `compute_loop_heat` runs once per body instead of twice.
fn body_is_hot_tier_with_heat<A: core::alloc::Allocator + Copy>(
    view: blaze_lir::View<'_>,
    symbol: Symbol,
    heat: LoopHeat,
    pgo: &PgoSiteTable<A>,
) -> bool {
    if (view.head.attrs & (FnAttrs::COLD | FnAttrs::NORETURN)) != 0 {
        return false;
    }
    let pgo_w = pgo.body_weight(BodyId(symbol.0));
    let loop_score = if heat.max_depth > 0 {
        10u64
            .saturating_pow(heat.max_depth.min(MAX_LOOP_DEPTH_EXP))
            .saturating_mul(heat.loop_blocks)
    } else {
        0
    };
    let body_heat = loop_score.saturating_add(pgo_w);
    heat.max_depth > 0 || body_heat > STATIC_STRAIGHT_LINE_WEIGHT
}

#[cfg(test)]
fn body_is_hot_tier<'s, A: core::alloc::Allocator + Copy>(
    view: blaze_lir::View<'_>,
    symbol: Symbol,
    pgo: &PgoSiteTable<A>,
    scratch: Alloc<'s>,
) -> bool {
    if (view.head.attrs & (FnAttrs::COLD | FnAttrs::NORETURN)) != 0 {
        return false;
    }
    let heat = compute_loop_heat(view, scratch);
    body_is_hot_tier_with_heat(view, symbol, heat, pgo)
}

/// Resolves a direct `Op::Call` / `Op::Invoke` / `Op::TailCall` instruction to its surviving-body
/// index in `0..prepared.len()` via `funcs` and `surv_by_sym`.
fn direct_internal_callee_idx(
    view: blaze_lir::View<'_>,
    inst: usize,
    funcs: &[Callee<'_>],
    surv_by_sym: &SymIndexMap,
) -> Option<usize> {
    if !matches!(view.op(inst), Op::Call | Op::Invoke | Op::TailCall) {
        return None;
    }
    let &data_at = view.data.get(inst)?;
    let &ref_word = usize::try_from(data_at).ok().and_then(|at| view.extra.get(at))?;
    let r = view.refs.get(RefId(ref_word).index())?;
    if RefKind::from_word(r.kind) != Some(RefKind::Func) {
        return None;
    }
    let place = usize::try_from(r.place).ok()?;
    let &Callee::Symbol(sym) = funcs.get(place)? else {
        return None;
    };
    find_body_idx(surv_by_sym, sym.0)
}

/// Decides whether a non-workspace callee reached during demand-driven export LTO BFS should have
/// its normal-block callees expanded. Cold (unless `#[inline(always)]`) and `#[noreturn]` bodies
/// are never expanded so panic/format/backtrace subgraphs stay out of LTO, while large non-cold
/// callers (even past `opts.max_caller_insts`) ARE still expanded so their downstream hot call
/// subgraph (`SliceRead::*`, `Vec::push`, etc.) is never severed!
fn should_expand_non_seed(attrs: u32) -> bool {
    let is_cold = (attrs & FnAttrs::COLD) != 0;
    let is_noreturn = (attrs & FnAttrs::NORETURN) != 0;
    let is_naked = (attrs & FnAttrs::NAKED) != 0;
    let inline_attr = FnAttrs(attrs).inline();
    !is_noreturn && !is_naked && (!is_cold || inline_attr == Inline::Always)
}

/// Decides whether a non-workspace callee reached during demand-driven export LTO BFS enters
/// the LTO module. Non-cold, non-noreturn library callers up to `opts.max_caller_insts` (including
/// `#[inline(never)]` hot algorithms such as `core::slice::sort::drift::sort` and derived
/// `serde::de::Visitor::visit_map`) are kept so their own small callees inline bottom-up into them
/// and they emit with host-ISA SOTA codegen, while `lto_optimize_module_par` continues enforcing
/// `Inline::Never` and `opts.policy.max_callee_insts` when deciding whether to inline them into
/// their own callers. Cold and noreturn bodies stay out unless explicitly `#[inline(always)]`.
fn should_keep_non_seed(
    attrs: u32,
    insts: usize,
    in_tiny_reserve: bool,
    tiny_leaf_insts: usize,
    opts: &ExportLtoOpts<'_>,
) -> bool {
    if !should_expand_non_seed(attrs) {
        return false;
    }
    let inline_attr = FnAttrs(attrs).inline();
    if in_tiny_reserve {
        return matches!(inline_attr, Inline::Always | Inline::Hint) && insts <= tiny_leaf_insts;
    }
    let keep_limit = opts
        .max_caller_insts
        .max(if inline_attr == Inline::Always || inline_attr == Inline::Hint {
            opts.max_always_inline_insts
        } else {
            opts.max_callee_insts
        });
    insts <= keep_limit
}

/// Enqueues a candidate target symbol into either `call_queue` (direct `Call`/`Invoke` edges) or
/// `indirect_queue` (`FuncAddr` and vtable/relocated-data targets) while preserving the shallowest
/// observed BFS depth (`min_depth`). If a row was already scanned at `depth >= max_depth` without
/// expanding its callees (`PROCESSED_UNEXPANDED`) and is later reached at a strictly shallower
/// depth `< max_depth`, it is re-queued in `QUEUED_EXPAND_ONLY` mode so its downstream callees are
/// expanded without duplicating the row in `origins`.
const UNSEEN: u8 = 0;
const QUEUED_INDIRECT: u8 = 1;
const QUEUED_CALL: u8 = 2;
const PROCESSED_UNEXPANDED: u8 = 3;
const QUEUED_EXPAND_ONLY: u8 = 4;
const PROCESSED: u8 = 5;

#[expect(clippy::too_many_arguments, reason = "BFS enqueue helper threading explicit queues and state slices without extra allocations")]
fn enqueue_sym(
    walk: &Walk<'_>,
    sym: Symbol,
    direct: bool,
    next_depth: u8,
    max_depth: u8,
    is_eligible_callee: &impl Fn(usize) -> bool,
    visited: &mut [u8],
    min_depth: &mut [u8],
    call_queue: &mut AVec<'_, (usize, u8, bool)>,
    indirect_queue: &mut AVec<'_, (usize, u8, bool)>,
) {
    let Some(callee_row) = walk.row_of(&sym) else {
        return;
    };
    if !is_eligible_callee(callee_row) {
        return;
    }
    let eff_depth = if let Some(best) = min_depth.get_mut(callee_row) {
        let updated = (*best).min(next_depth);
        *best = updated;
        updated
    } else {
        next_depth
    };
    if let Some(state) = visited.get_mut(callee_row) {
        if direct && *state < QUEUED_CALL {
            *state = QUEUED_CALL;
            call_queue.push((callee_row, eff_depth, false));
        } else if !direct && *state == UNSEEN {
            *state = QUEUED_INDIRECT;
            indirect_queue.push((callee_row, eff_depth, false));
        } else if *state == PROCESSED_UNEXPANDED && eff_depth < max_depth {
            *state = QUEUED_EXPAND_ONLY;
            if direct {
                call_queue.push((callee_row, eff_depth, false));
            } else {
                indirect_queue.push((callee_row, eff_depth, false));
            }
        }
    }
}

/// Enqueues every concrete `Space::Func` method symbol referenced by vtable `vsym` in
/// `built.vtable_data` into `indirect_queue` so `&dyn Trait` hot implementations (e.g.
/// `serde_json::read::SliceRead`, `std::io::Read`) are discovered by export LTO BFS.
#[expect(clippy::too_many_arguments, reason = "vtable BFS enqueue helper threading explicit queues and state slices")]
fn enqueue_vtable_methods<A: core::alloc::Allocator>(
    built: &BuiltLir<A>,
    vsym: Symbol,
    walk: &Walk<'_>,
    next_depth: u8,
    max_depth: u8,
    is_eligible_callee: &impl Fn(usize) -> bool,
    visited: &mut [u8],
    min_depth: &mut [u8],
    call_queue: &mut AVec<'_, (usize, u8, bool)>,
    indirect_queue: &mut AVec<'_, (usize, u8, bool)>,
) {
    for (sym, datum_bytes) in built.vtable_data.iter() {
        if *sym != vsym {
            continue;
        }
        let Ok(dv) = DatumView::read(datum_bytes.as_slice()) else {
            continue;
        };
        for i in 0..dv.targets() {
            let target = dv.target(i);
            if target.space == Space::Func {
                enqueue_sym(
                    walk,
                    target.symbol,
                    false,
                    next_depth,
                    max_depth,
                    is_eligible_callee,
                    visited,
                    min_depth,
                    call_queue,
                    indirect_queue,
                );
            }
        }
    }
}

/// Iteratively traverses local `RefKind::Data` relocations starting from `root_data` (following
/// nested `Data -> Data -> Func/Vtable` chains while guarding against cycles via `visited_data`)
/// and enqueues every discovered function or vtable method target into `indirect_queue`.
#[expect(clippy::too_many_arguments, reason = "iterative data-reloc traversal helper threading explicit queues and per-body visited state")]
fn enqueue_data_relocs<A: core::alloc::Allocator>(
    built: &BuiltLir<A>,
    root_data: u32,
    walk: &Walk<'_>,
    next_depth: u8,
    max_depth: u8,
    is_eligible_callee: &impl Fn(usize) -> bool,
    visited: &mut [u8],
    min_depth: &mut [u8],
    call_queue: &mut AVec<'_, (usize, u8, bool)>,
    indirect_queue: &mut AVec<'_, (usize, u8, bool)>,
    visited_data: &mut [bool],
    data_stack: &mut AVec<'_, u32>,
) {
    let Ok(root_idx) = usize::try_from(root_data) else {
        return;
    };
    let Some(seen_root) = visited_data.get_mut(root_idx) else {
        return;
    };
    if *seen_root {
        return;
    }
    *seen_root = true;
    data_stack.clear();
    data_stack.push(root_data);

    let view = built.func.view();
    while let Some(data_place) = data_stack.pop() {
        for reloc in view.data_relocs_of(DataId(data_place)) {
            let Some(trec) = view.refs.get(RefId(reloc.target).index()) else {
                continue;
            };
            match RefKind::from_word(trec.kind) {
                Some(RefKind::Func) => {
                    let Ok(place_idx) = usize::try_from(trec.place) else { continue };
                    let Some(&BuiltCallee::Symbol(callee_sym)) = built.funcs.get(place_idx) else { continue };
                    enqueue_sym(
                        walk,
                        callee_sym,
                        false,
                        next_depth,
                        max_depth,
                        is_eligible_callee,
                        visited,
                        min_depth,
                        call_queue,
                        indirect_queue,
                    );
                }
                Some(RefKind::Vtable) => {
                    let Ok(place_idx) = usize::try_from(trec.place) else { continue };
                    let Some(&vsym) = built.vtables.get(place_idx) else { continue };
                    enqueue_vtable_methods(
                        built,
                        vsym,
                        walk,
                        next_depth,
                        max_depth,
                        is_eligible_callee,
                        visited,
                        min_depth,
                        call_queue,
                        indirect_queue,
                    );
                }
                Some(RefKind::Data) => {
                    let Ok(next_idx) = usize::try_from(trec.place) else { continue };
                    if let Some(seen) = visited_data.get_mut(next_idx)
                        && !*seen
                    {
                        *seen = true;
                        data_stack.push(trec.place);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Enqueues every non-cold callee reachable from `BlockKind::Normal` blocks of `built`:
/// - direct `Op::Call` and `Op::Invoke` targets into `call_queue` (now that unwinding `Op::Invoke`
///   inlines cleanly, normal-block `Op::Invoke` is a first-class hot edge);
/// - `Op::FuncAddr` function-pointer targets and `Op::DataAddr` vtable / nested relocated-data
///   targets into `indirect_queue`.
#[expect(clippy::too_many_arguments, reason = "BFS callee discovery helper threading explicit queues, depth bounds, and scratch allocator")]
fn enqueue_normal_callees<A: core::alloc::Allocator>(
    built: &BuiltLir<A>,
    walk: &Walk<'_>,
    next_depth: u8,
    max_depth: u8,
    is_eligible_callee: &impl Fn(usize) -> bool,
    visited: &mut [u8],
    min_depth: &mut [u8],
    call_queue: &mut AVec<'_, (usize, u8, bool)>,
    indirect_queue: &mut AVec<'_, (usize, u8, bool)>,
    scratch: Alloc<'_>,
) {
    let view = built.func.view();
    let mut visited_data: AVec<'_, bool> = Vec::new_in(scratch);
    let mut data_stack: AVec<'_, u32> = Vec::new_in(scratch);
    for b in 0..view.blocks() {
        let Ok(b_u32) = u32::try_from(b) else { break };
        let block = Block(b_u32);
        if view.block_kind(block) != BlockKind::Normal {
            continue;
        }
        for at in view.block(block) {
            if view.flags(at).contains(Flags::COLD) {
                continue;
            }
            let op = view.op(at);
            match op {
                Op::Call | Op::Invoke => {
                    let Some(&data_at) = view.data.get(at) else { continue };
                    let Ok(extra_idx) = usize::try_from(data_at) else { continue };
                    let Some(&ref_word) = view.extra.get(extra_idx) else { continue };
                    let Some(rec) = view.refs.get(RefId(ref_word).index()) else { continue };
                    if RefKind::from_word(rec.kind) != Some(RefKind::Func) {
                        continue;
                    }
                    let Ok(place_idx) = usize::try_from(rec.place) else { continue };
                    let Some(&BuiltCallee::Symbol(callee_sym)) = built.funcs.get(place_idx) else { continue };
                    enqueue_sym(
                        walk,
                        callee_sym,
                        true,
                        next_depth,
                        max_depth,
                        is_eligible_callee,
                        visited,
                        min_depth,
                        call_queue,
                        indirect_queue,
                    );
                }
                Op::FuncAddr => {
                    let Some(&ref_word) = view.data.get(at) else { continue };
                    let Some(rec) = view.refs.get(RefId(ref_word).index()) else { continue };
                    if RefKind::from_word(rec.kind) != Some(RefKind::Func) {
                        continue;
                    }
                    let Ok(place_idx) = usize::try_from(rec.place) else { continue };
                    let Some(&BuiltCallee::Symbol(callee_sym)) = built.funcs.get(place_idx) else { continue };
                    enqueue_sym(
                        walk,
                        callee_sym,
                        false,
                        next_depth,
                        max_depth,
                        is_eligible_callee,
                        visited,
                        min_depth,
                        call_queue,
                        indirect_queue,
                    );
                }
                Op::DataAddr => {
                    let Some(&ref_word) = view.data.get(at) else { continue };
                    let Some(rec) = view.refs.get(RefId(ref_word).index()) else { continue };
                    match RefKind::from_word(rec.kind) {
                        Some(RefKind::Vtable) => {
                            let Ok(place_idx) = usize::try_from(rec.place) else { continue };
                            let Some(&vsym) = built.vtables.get(place_idx) else { continue };
                            enqueue_vtable_methods(
                                built,
                                vsym,
                                walk,
                                next_depth,
                                max_depth,
                                is_eligible_callee,
                                visited,
                                min_depth,
                                call_queue,
                                indirect_queue,
                            );
                        }
                        Some(RefKind::Data) => {
                            if visited_data.is_empty() && !view.datas.is_empty() {
                                visited_data.resize(view.datas.len(), false);
                            }
                            enqueue_data_relocs(
                                built,
                                rec.place,
                                walk,
                                next_depth,
                                max_depth,
                                is_eligible_callee,
                                visited,
                                min_depth,
                                call_queue,
                                indirect_queue,
                                visited_data.as_mut_slice(),
                                &mut data_stack,
                            );
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

/// Whole-program demand-driven cross-function LIR inlining and host-ISA SOTA emission over
/// workspace seed rows and their reachable non-cold callees (sequential convenience wrapper).
#[expect(clippy::too_many_arguments, reason = "whole-program export LTO over walk, seeds, gate, maker, options, shared output allocator, and scratch allocator")]
pub fn optimize_export_workspace<'a, 'n, 'b, G, T>(
    walk: &Walk<'_>,
    seeds: &[usize],
    is_eligible_callee: impl Fn(usize) -> bool,
    grant_for: impl FnMut(ItemKey) -> Option<G>,
    maker: &mut Maker<'n, G, T>,
    opts: ExportLtoOpts<'b>,
    shared: SharedAlloc<'static>,
    alloc: Alloc<'a>,
) -> Result<ExportLtoOut<'a>, ExportLtoError>
where
    G: Grant,
    T: Typing<RecordsProgram<'n>>,
{
    optimize_export_workspace_par(
        walk,
        seeds,
        is_eligible_callee,
        grant_for,
        maker,
        opts,
        shared,
        alloc,
        &SequentialLtoRunner,
    )
}

/// Whole-program demand-driven cross-function LIR inlining and host-ISA SOTA emission
/// over workspace seed rows and their reachable non-cold callees.
///
/// 1. Demand-driven BFS from `seeds` follows normal-block `Op::Call`, `Op::Invoke`,
///    `Op::FuncAddr`, and vtable/relocated-data `Space::Func` edges without severing callees of
///    oversized non-cold callers.
/// 2. Bottom-up Tarjan SCC ThinLTO (`lto_optimize_module_par`) runs pre-inline simplify and
///    wavefront SCC inlining in parallel across `runner`, guided by static PGO merged with any
///    persisted `BPG1` record.
/// 3. Whole-module Zero-ABI planning (`ScratchOrder`, guarded `ColdPreserveAll`, `InternalFast`)
///    and bottom-up Tarjan SCC IPRA emission (`emit_code_record_for_machine_with_abi`) emit every
///    gathered non-cold body into sealed host-ISA `CodeRecord`s in deterministic index order
///    (Rule 25).
#[expect(clippy::too_many_arguments, reason = "whole-program parallel export LTO over walk, seeds, gate, maker, options, shared output allocator, scratch allocator, and parallel runner")]
pub fn optimize_export_workspace_par<'a, 'n, 'b, G, T, R>(
    walk: &Walk<'_>,
    seeds: &[usize],
    is_eligible_callee: impl Fn(usize) -> bool,
    grant_for: impl FnMut(ItemKey) -> Option<G>,
    maker: &mut Maker<'n, G, T>,
    opts: ExportLtoOpts<'b>,
    shared: SharedAlloc<'static>,
    alloc: Alloc<'a>,
    runner: &R,
) -> Result<ExportLtoOut<'a>, ExportLtoError>
where
    G: Grant,
    T: Typing<RecordsProgram<'n>>,
    R: LtoRunner,
{
    optimize_export_workspace_par_cached(
        walk,
        seeds,
        is_eligible_callee,
        grant_for,
        maker,
        opts,
        shared,
        alloc,
        runner,
        &NoExportLtoCache,
    )
}

struct MissingWalkTargets {
    instances: Vec<(usize, ItemKey, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>>,
    statics: Vec<ItemKey, SharedAlloc<'static>>,
    tls: Vec<ItemKey, SharedAlloc<'static>>,
}

/// Scans every surviving post-LTO [`LtoBodyOut`]'s sealed `CodeRecord` and carried `vtable_data`
/// against `walk`, recovering any `Space::Func`, `Space::Static`, or `Space::Tls` target not yet
/// in `walk` from the `origins`' persisted declared/static/tls tables (regardless of whether the
/// origin came from a fresh `build_lir_with_reach_in` or a `BLZLIR02` / `BLZEMT01` cache hit).
///
/// Refuses with [`ExportLtoError::MissingDeclared`] if any emitted target has no declaration in
/// either `walk` or `origins` (Rule 3: export never leaves a hole).
fn collect_missing_walk_targets<'a>(
    walk: &Walk<'_>,
    origins: &[LtoOrigin],
    out_bodies: &[LtoBodyOut],
    shared: SharedAlloc<'static>,
    alloc: Alloc<'a>,
) -> Result<MissingWalkTargets, ExportLtoError> {
    let mut decl_by_sym: Map<Symbol, (usize, ItemKey, &[u8]), FixedState, Alloc<'a>> =
        Map::new(MapOpts { hasher: FixedState, alloc, capacity: Some(origins.len().saturating_mul(2)) });
    let mut static_by_sym: Map<Symbol, ItemKey, FixedState, Alloc<'a>> =
        Map::new(MapOpts { hasher: FixedState, alloc, capacity: Some(origins.len() / 4) });
    let mut tls_by_sym: Map<Symbol, ItemKey, FixedState, Alloc<'a>> =
        Map::new(MapOpts { hasher: FixedState, alloc, capacity: Some(16) });
    for orig in origins.iter() {
        for (tabled, name) in orig.declared.iter() {
            let sym = crate::symbols::fn_symbol_of_name(name.as_slice());
            decl_by_sym.entry(sym).or_insert((orig.row, *tabled, name.as_slice()));
        }
        for &item in orig.built.statics.iter() {
            if orig.built.foreign_static(item).is_none() {
                static_by_sym.entry(static_symbol(item)).or_insert(item);
            }
        }
        for &item in orig.built.tls.iter() {
            tls_by_sym.entry(tls_symbol(item)).or_insert(item);
        }
    }

    let mut soft_float_syms: Set<Symbol, FixedState, Alloc<'a>> =
        Set::new(SetOpts { hasher: FixedState, alloc, capacity: Some(16) });
    for body in out_bodies.iter() {
        for (_, name) in body.extra_declared.iter() {
            soft_float_syms.insert(crate::symbols::fn_symbol_of_name(name.as_slice()));
        }
    }

    let tls_getter = crate::symbols::tls_getter_symbol();
    let tls_desc = crate::symbols::tls_desc_symbol();
    let mut seen_inst: Set<Symbol, FixedState, Alloc<'a>> =
        Set::new(SetOpts { hasher: FixedState, alloc, capacity: Some(32) });
    let mut seen_static: Set<Symbol, FixedState, Alloc<'a>> =
        Set::new(SetOpts { hasher: FixedState, alloc, capacity: Some(16) });
    let mut seen_tls: Set<Symbol, FixedState, Alloc<'a>> =
        Set::new(SetOpts { hasher: FixedState, alloc, capacity: Some(8) });
    let mut instances: Vec<(usize, ItemKey, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>> = Vec::new_in(shared);
    let mut statics: Vec<ItemKey, SharedAlloc<'static>> = Vec::new_in(shared);
    let mut tls: Vec<ItemKey, SharedAlloc<'static>> = Vec::new_in(shared);

    let mut check_target = |from_sym: [u8; 32], target: CodeTarget| -> Result<(), ExportLtoError> {
        match target.space {
            Space::Func => {
                if walk.row_of(&target.symbol).is_some()
                    || target.symbol == tls_getter
                    || soft_float_syms.contains(&target.symbol)
                    || seen_inst.contains(&target.symbol)
                {
                    return Ok(());
                }
                let Some(&(declarer_row, tabled, name_slice)) = decl_by_sym.get(&target.symbol) else {
                    return Err(ExportLtoError::MissingDeclared { from: from_sym, target });
                };
                seen_inst.insert(target.symbol);
                let mut copied: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(name_slice.len(), shared);
                copied.extend_from_slice(name_slice);
                instances.push((declarer_row, tabled, copied));
                Ok(())
            }
            Space::Static => {
                if walk.row_of(&target.symbol).is_some()
                    || target.symbol == tls_desc
                    || seen_static.contains(&target.symbol)
                {
                    return Ok(());
                }
                let Some(&item) = static_by_sym.get(&target.symbol) else {
                    return Err(ExportLtoError::MissingDeclared { from: from_sym, target });
                };
                seen_static.insert(target.symbol);
                statics.push(item);
                Ok(())
            }
            Space::Tls => {
                if walk.row_of(&target.symbol).is_some() || seen_tls.contains(&target.symbol) {
                    return Ok(());
                }
                let Some(&item) = tls_by_sym.get(&target.symbol) else {
                    return Err(ExportLtoError::MissingDeclared { from: from_sym, target });
                };
                seen_tls.insert(target.symbol);
                tls.push(item);
                Ok(())
            }
            Space::Vtable | Space::Outside | Space::WeakOutside => Ok(()),
        }
    };

    for body in out_bodies.iter() {
        let view = CodeView::read(body.record.as_slice())
            .map_err(|_| ExportLtoError::DecodeRecord { symbol: body.symbol.0, what: "code record" })?;
        for ti in 0..view.targets() {
            check_target(body.symbol.0, view.target(ti))?;
        }
        for (_, datum_bytes) in body.vtable_data.iter() {
            let dv = DatumView::read(datum_bytes.as_slice())
                .map_err(|_| ExportLtoError::DecodeRecord { symbol: body.symbol.0, what: "vtable datum" })?;
            for ti in 0..dv.targets() {
                check_target(body.symbol.0, dv.target(ti))?;
            }
        }
    }

    Ok(MissingWalkTargets { instances, statics, tls })
}

/// Whole-program parallel demand-driven cross-function LIR inlining and host-ISA SOTA emission
/// with content-addressed local incremental caching (`BuiltLir` + post-LTO emitted bodies) and
/// wavefront-parallel bottom-up Tarjan SCC IPRA emission (Rule 21, Rule 25, Rule 51).
#[expect(clippy::too_many_arguments, reason = "whole-program parallel cached export LTO over walk, seeds, gate, maker, options, allocators, runner, and cache seam")]
pub fn optimize_export_workspace_par_cached<'a, 'n, 'b, G, T, R, C>(
    walk: &Walk<'_>,
    seeds: &[usize],
    is_eligible_callee: impl Fn(usize) -> bool,
    mut grant_for: impl FnMut(ItemKey) -> Option<G>,
    maker: &mut Maker<'n, G, T>,
    opts: ExportLtoOpts<'b>,
    shared: SharedAlloc<'static>,
    alloc: Alloc<'a>,
    runner: &R,
    cache: &C,
) -> Result<ExportLtoOut<'a>, ExportLtoError>
where
    G: Grant,
    T: Typing<RecordsProgram<'n>>,
    R: LtoRunner,
    C: ExportLtoCache,
{
    let mut times = ExportLtoTimes::default();
    if seeds.is_empty() || opts.max_bodies == 0 {
        return Ok(ExportLtoOut {
            bodies: Vec::new_in(shared),
            extra_walk_instances: Vec::new_in(shared),
            extra_walk_statics: Vec::new_in(shared),
            extra_walk_tls: Vec::new_in(shared),
            hot_symbols: Vec::new_in(alloc),
            profile_digest: [0u8; 32],
            module_bodies: 0,
            total_inlined: 0,
            times,
        });
    }

    let t_bfs = cache.now_ns();
    let mut visited: AVec<'a, u8> = Vec::with_capacity_in(walk.len(), alloc);
    visited.resize(walk.len(), UNSEEN);
    let mut min_depth: AVec<'a, u8> = Vec::with_capacity_in(walk.len(), alloc);
    min_depth.resize(walk.len(), u8::MAX);
    let mut call_queue: AVec<'a, (usize, u8, bool)> = Vec::with_capacity_in(seeds.len().min(opts.max_bodies), alloc);
    let mut indirect_queue: AVec<'a, (usize, u8, bool)> = Vec::new_in(alloc);
    for &row in seeds {
        if let Some(state) = visited.get_mut(row)
            && *state == UNSEEN
            && is_eligible_callee(row)
        {
            *state = QUEUED_CALL;
            if let Some(best) = min_depth.get_mut(row) {
                *best = 0;
            }
            call_queue.push((row, 0, true));
        }
    }

    // 1. Bounded demand-driven BFS: gather workspace seeds + reachable non-cold dependency bodies.
    // Reserve the final eighth of large LTO budgets for tiny `<= 24`-inst `#[inline]` /
    // `#[inline(always)]` leaf helpers so medium callees never crowd out deep leaf chains.
    const TINY_INLINE_LEAF_INSTS: usize = 24;
    const MAX_RESERVE_SCANS: usize = 2048;
    let tiny_reserve = if opts.max_bodies >= 512 { opts.max_bodies / 8 } else { 0 };
    let soft_limit = opts.max_bodies.saturating_sub(tiny_reserve);
    let max_scanned = opts.max_bodies.saturating_mul(2);
    let mut total_scanned = 0usize;
    let mut reserve_scans = 0usize;
    let mut origins: Vec<LtoOrigin, SharedAlloc<'static>> = Vec::with_capacity_in(call_queue.len(), shared);
    let mut call_head = 0usize;
    let mut indirect_head = 0usize;
    while origins.len() < opts.max_bodies && total_scanned < max_scanned {
        let in_tiny_reserve = tiny_reserve > 0 && origins.len() >= soft_limit;
        if in_tiny_reserve && reserve_scans >= MAX_RESERVE_SCANS {
            break;
        }
        let next = if let Some(&item) = call_queue.get(call_head) {
            call_head += 1;
            item
        } else if !in_tiny_reserve
            && let Some(&item) = indirect_queue.get(indirect_head)
        {
            indirect_head += 1;
            item
        } else {
            break;
        };
        let (row, queued_depth, is_seed) = next;
        let Some(state) = visited.get_mut(row) else {
            continue;
        };
        if *state == PROCESSED || *state == PROCESSED_UNEXPANDED {
            continue;
        }
        let expand_only = *state == QUEUED_EXPAND_ONLY;
        let depth = min_depth.get(row).copied().unwrap_or(queued_depth).min(queued_depth);
        *state = if depth < opts.max_depth { PROCESSED } else { PROCESSED_UNEXPANDED };
        total_scanned += 1;
        if in_tiny_reserve && !is_seed && !expand_only {
            reserve_scans += 1;
        }
        let tabled = walk.tabled(row);
        let symbol = walk.symbol(row);
        let name = walk.name(row);
        let lir_key = cache.lir_key(row, opts.machine);
        let (built, declared) = if let Some(key) = lir_key
            && let Some(cached) = cache.get(key, shared)
        {
            times.lir_hits = times.lir_hits.saturating_add(1);
            decode_built_lir(cached.as_slice(), shared)
                .ok_or(ExportLtoError::CorruptCache { what: "BuiltLir", key })?
        } else {
            times.lir_misses = times.lir_misses.saturating_add(1);
            let Some(grant) = grant_for(tabled) else {
                return Err(ExportLtoError::NotRunnable { symbol: symbol.0, tabled });
            };
            let enclosers = crate::body_ids::local_enclosers_of(walk, row, tabled);
            if !enclosers.locals().is_empty() {
                maker.prepare_local(enclosers.locals(), enclosers.enclosers());
            }
            let mut lto_decl = crate::body_ids::DeclaredList::new_in(alloc);
            let mut built = maker
                .build_lir_with_reach_in(&grant, tabled, name, Some(&mut lto_decl), shared)
                .map_err(|error| ExportLtoError::BuildLir { symbol: symbol.0, error })?;
            let mut declared: Vec<DeclaredRow, SharedAlloc<'static>> = Vec::with_capacity_in(lto_decl.len(), shared);
            for idx in 0..lto_decl.len() {
                let (decl_tabled, decl_name) = lto_decl.row(idx);
                let mut copied = Vec::with_capacity_in(decl_name.len(), shared);
                copied.extend_from_slice(decl_name);
                declared.push((decl_tabled, copied));
            }
            let lowered_libm = lower_ieee_libm_calls(&mut built, opts.machine, alloc);
            if lowered_libm {
                verify(built.func.view())
                    .map_err(|error| ExportLtoError::Verify { symbol: symbol.0, error })?;
            }
            if let Some(key) = lir_key
                && let Some(encoded) = encode_built_lir(&built, declared.as_slice(), shared)
            {
                cache.put(key, encoded.as_slice());
            }
            (built, declared)
        };

        let view = built.func.view();
        if !is_seed && !should_expand_non_seed(view.head.attrs) {
            if let Some(st) = visited.get_mut(row) {
                *st = PROCESSED;
            }
            continue;
        }

        // Always expand normal-block callees of every seed and every non-cold, non-noreturn body
        // BEFORE checking `max_caller_insts` / `in_tiny_reserve` so large callers never sever
        // their downstream call subgraph (`SliceRead::*`, `Vec::push`, etc.).
        if depth < opts.max_depth {
            let next_depth = depth.saturating_add(1);
            enqueue_normal_callees(
                &built,
                walk,
                next_depth,
                opts.max_depth,
                &is_eligible_callee,
                visited.as_mut_slice(),
                min_depth.as_mut_slice(),
                &mut call_queue,
                &mut indirect_queue,
                alloc,
            );
        }

        if expand_only {
            continue;
        }

        if !is_seed && !should_keep_non_seed(view.head.attrs, view.len(), in_tiny_reserve, TINY_INLINE_LEAF_INSTS, &opts) {
            continue;
        }

        let orig_unreachables = view
            .ops
            .iter()
            .filter(|&&op| Op::from_byte(op) == Some(Op::Unreachable))
            .count();
        origins.push(LtoOrigin {
            row,
            symbol,
            is_seed,
            orig_unreachables,
            built,
            declared,
        });
    }
    times.bfs_ns = cache.now_ns().saturating_sub(t_bfs);

    if origins.is_empty() {
        return Ok(ExportLtoOut {
            bodies: Vec::new_in(shared),
            extra_walk_instances: Vec::new_in(shared),
            extra_walk_statics: Vec::new_in(shared),
            extra_walk_tls: Vec::new_in(shared),
            hot_symbols: Vec::new_in(alloc),
            profile_digest: [0u8; 32],
            module_bodies: 0,
            total_inlined: 0,
            times,
        });
    }

    // 2. Parallel bottom-up Tarjan SCC cross-function LIR inlining + SROA/propagate/DCE/CFG-coalesce,
    //    guided by parallel static PGO estimates merged with any persisted cross-worktree `BPG1` record.
    let n = origins.len();
    let t_pgo = cache.now_ns();
    let mut by_sym: SymIndexMap = Map::new(MapOpts { hasher: FixedState, alloc: shared, capacity: Some(n) });
    let mut mod_bodies: Vec<ModuleBody<SharedAlloc<'static>>, SharedAlloc<'static>> = Vec::with_capacity_in(n, shared);
    for (idx, orig) in origins.iter().enumerate() {
        by_sym.insert(orig.symbol.0, idx);
        mod_bodies.push(ModuleBody {
            id: BodyId(orig.symbol.0),
            func: orig.built.func.view().to_func_in(shared),
        });
    }

    let pgo = build_pgo_table_par(mod_bodies.as_slice(), opts.pgo_bytes, shared, runner)?;
    times.pgo_ns = cache.now_ns().saturating_sub(t_pgo);

    let t_inline = cache.now_ns();
    let bodies_view = LtoBodies { origins: origins.as_slice(), by_sym: &by_sym };
    let lto = lto_optimize_module_par(mod_bodies, &bodies_view, &pgo, opts.policy, shared, runner)
        .map_err(ExportLtoError::Optimize)?;
    times.inline_ns = cache.now_ns().saturating_sub(t_inline);

    // Compute static LoopHeat ONCE per post-LTO body in parallel (shared between `rank_hot_symbols`
    // and 2-tier `is_hot` classification).
    let t_prep = cache.now_ns();
    let lto_bodies_slice = lto.bodies.as_slice();
    let loop_heats: Vec<LoopHeat, SharedAlloc<'static>> = runner
        .par_map(n, shared, |idx, scratch| {
            let view = lto_bodies_slice[idx].func.view();
            if (view.head.attrs & (FnAttrs::COLD | FnAttrs::NORETURN)) != 0 {
                LoopHeat { max_depth: 0, loop_blocks: 0 }
            } else {
                compute_loop_heat(view, scratch)
            }
        })
        .map_err(ExportLtoError::Optimize)?;

    let mut hot_inputs: AVec<'a, (Symbol, bool, blaze_lir::View<'_>)> = Vec::with_capacity_in(n, alloc);
    for (orig, opt) in origins.iter().zip(lto.bodies.iter()) {
        hot_inputs.push((orig.symbol, orig.is_seed, opt.func.view()));
    }
    let call_edges = collect_post_lto_call_edges(
        origins.as_slice(),
        lto.bodies.as_slice(),
        &by_sym,
        alloc,
    );
    let hot_symbols = rank_hot_symbols_with_heats(
        hot_inputs.as_slice(),
        loop_heats.as_slice(),
        call_edges.as_slice(),
        &pgo,
        alloc,
        alloc,
    );
    let profile_digest = compute_profile_digest(&pgo, hot_symbols.as_slice(), shared);

    // 3. Prepare soft-float routines once, expand reference tables transitively & verify every
    //    post-LTO body in parallel, plan whole-module Zero-ABI (`ScratchOrder` + guarded
    //    `ColdPreserveAll`) & wavefront-parallel bottom-up Tarjan SCC IPRA emission, and emit
    //    ephemeral host-ISA records with incremental caching (never written to portable `CodeStore`,
    //    preserving exact deterministic output order — Rule 25).
    let bound_routines = maker
        .prepare_routines()
        .map_err(|error| ExportLtoError::Record { symbol: [0u8; 32], error })?;
    let origins_slice = origins.as_slice();
    let by_sym_ref = &by_sym;
    let pgo_ref = &pgo;
    let loop_heats_slice = loop_heats.as_slice();

    let prepared_results = runner
        .par_map(n, shared, |idx, _scratch| -> Result<PreparedBody<'_, '_>, ParLtoErr> {
            let orig = &origins_slice[idx];
            let opt = &lto_bodies_slice[idx];
            let mut funcs: Vec<Callee<'_>, SharedAlloc<'static>> =
                Vec::with_capacity_in(orig.built.funcs.len() + opt.imported_refs.len(), shared);
            for p in 0..orig.built.funcs.len() {
                let p_u32 = u32::try_from(p).map_err(|_| ParLtoErr::BadRef {
                    symbol: orig.symbol.0,
                    what: "original func place",
                    ref_id: u32::MAX,
                })?;
                let Some(callee) = orig.built.callee_at(p_u32) else {
                    return Err(ParLtoErr::BadRef {
                        symbol: orig.symbol.0,
                        what: "original func name",
                        ref_id: p_u32,
                    });
                };
                funcs.push(callee);
            }
            let mut statics: Vec<ItemKey, SharedAlloc<'static>> =
                Vec::with_capacity_in(orig.built.statics.len(), shared);
            statics.extend_from_slice(orig.built.statics.as_slice());
            let mut c_statics: Vec<crate::code_records::ForeignStatic<'_>, SharedAlloc<'static>> =
                Vec::with_capacity_in(orig.built.c_statics.len(), shared);
            for cs in orig.built.c_statics.iter() {
                let Some(cname) = orig.built.c_str(cs.start, cs.len) else {
                    return Err(ParLtoErr::BadRef {
                        symbol: orig.symbol.0,
                        what: "original foreign static name",
                        ref_id: 0,
                    });
                };
                c_statics.push(crate::code_records::ForeignStatic { item: cs.item, name: cname, weak: cs.weak });
            }
            let mut vtables: Vec<Symbol, SharedAlloc<'static>> =
                Vec::with_capacity_in(orig.built.vtables.len(), shared);
            vtables.extend_from_slice(orig.built.vtables.as_slice());
            let mut vtable_data: Vec<(Symbol, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>> =
                Vec::with_capacity_in(orig.built.vtable_data.len(), shared);
            for (vsym, bytes) in orig.built.vtable_data.iter() {
                push_vtable_datum(&mut vtable_data, *vsym, bytes.as_slice(), shared);
            }
            let mut tls: Vec<ItemKey, SharedAlloc<'static>> =
                Vec::with_capacity_in(orig.built.tls.len(), shared);
            tls.extend_from_slice(orig.built.tls.as_slice());

            for imp in opt.imported_refs.iter() {
                let Some(kind) = RefKind::from_word(imp.rec.kind) else {
                    return Err(ParLtoErr::BadRef {
                        symbol: orig.symbol.0,
                        what: "imported ref kind",
                        ref_id: imp.caller_ref.0,
                    });
                };
                let place = usize::try_from(imp.rec.place).map_err(|_| ParLtoErr::BadRef {
                    symbol: orig.symbol.0,
                    what: "imported ref place",
                    ref_id: imp.caller_ref.0,
                })?;
                let (origin_idx, origin_rec) = resolve_ref_origin(
                    idx,
                    imp.caller_ref,
                    lto_bodies_slice,
                    origins_slice,
                    by_sym_ref,
                )?;
                if RefKind::from_word(origin_rec.kind) != Some(kind) {
                    return Err(ParLtoErr::RefKindMismatch {
                        symbol: orig.symbol.0,
                        ref_id: imp.caller_ref.0,
                    });
                }
                let Some(src_origin) = origins_slice.get(origin_idx) else {
                    return Err(ParLtoErr::BadRef {
                        symbol: orig.symbol.0,
                        what: "origin body index",
                        ref_id: imp.caller_ref.0,
                    });
                };
                let src_place = usize::try_from(origin_rec.place).map_err(|_| ParLtoErr::BadRef {
                    symbol: orig.symbol.0,
                    what: "origin ref place",
                    ref_id: imp.caller_ref.0,
                })?;
                match kind {
                    RefKind::Func => {
                        let Some(callee) = src_origin.built.callee_at(origin_rec.place) else {
                            return Err(ParLtoErr::BadRef {
                                symbol: orig.symbol.0,
                                what: "origin func callee",
                                ref_id: imp.caller_ref.0,
                            });
                        };
                        put_at(&mut funcs, place, callee, orig.symbol.0, "imported func place", imp.caller_ref.0)?;
                    }
                    RefKind::Static => {
                        let Some(&item) = src_origin.built.statics.get(src_place) else {
                            return Err(ParLtoErr::BadRef {
                                symbol: orig.symbol.0,
                                what: "origin static item",
                                ref_id: imp.caller_ref.0,
                            });
                        };
                        put_at(&mut statics, place, item, orig.symbol.0, "imported static place", imp.caller_ref.0)?;
                        if let Some((cname, weak)) = src_origin.built.foreign_static(item)
                            && !c_statics.iter().any(|cs| cs.item == item)
                        {
                            c_statics.push(crate::code_records::ForeignStatic { item, name: cname, weak });
                        }
                    }
                    RefKind::Vtable => {
                        let Some(&vsym) = src_origin.built.vtables.get(src_place) else {
                            return Err(ParLtoErr::BadRef {
                                symbol: orig.symbol.0,
                                what: "origin vtable symbol",
                                ref_id: imp.caller_ref.0,
                            });
                        };
                        put_at(&mut vtables, place, vsym, orig.symbol.0, "imported vtable place", imp.caller_ref.0)?;
                        for (sym, bytes) in src_origin.built.vtable_data.iter() {
                            push_vtable_datum(&mut vtable_data, *sym, bytes.as_slice(), shared);
                        }
                    }
                    RefKind::Tls => {
                        let Some(&item) = src_origin.built.tls.get(src_place) else {
                            return Err(ParLtoErr::BadRef {
                                symbol: orig.symbol.0,
                                what: "origin tls item",
                                ref_id: imp.caller_ref.0,
                            });
                        };
                        put_at(&mut tls, place, item, orig.symbol.0, "imported tls place", imp.caller_ref.0)?;
                    }
                    RefKind::Data => {
                        return Err(ParLtoErr::BadRef {
                            symbol: orig.symbol.0,
                            what: "unexpected imported Data ref",
                            ref_id: imp.caller_ref.0,
                        });
                    }
                }
            }

            let routines = if func_needs_soft_float(&opt.func.view()) {
                match bound_routines {
                    Some(bound) => bound
                        .bind_in(&mut funcs)
                        .map_err(|error| ParLtoErr::Record { symbol: orig.symbol.0, error })?,
                    None => Routines::NONE,
                }
            } else {
                Routines::NONE
            };
            let verified = verify(opt.func.view())
                .map_err(|error| ParLtoErr::Verify { symbol: orig.symbol.0, error })?;
            let post_unreachables = opt
                .func
                .view()
                .ops
                .iter()
                .filter(|&&op| Op::from_byte(op) == Some(Op::Unreachable))
                .count();
            let traps: &[TrapSource] = if opt.read.is_empty() && orig.orig_unreachables == post_unreachables {
                orig.built.traps.as_slice()
            } else {
                &[]
            };
            let heat = loop_heats_slice[idx];
            let is_hot = body_is_hot_tier_with_heat(verified.view(), orig.symbol, heat, pgo_ref);
            Ok(PreparedBody {
                row: orig.row,
                symbol: orig.symbol,
                inlined_bodies: opt.read.len(),
                is_hot,
                verified,
                funcs,
                statics,
                c_statics,
                vtables,
                vtable_data,
                tls,
                traps,
                routines,
            })
        })
        .map_err(ExportLtoError::Optimize)?;

    let mut prepared: Vec<PreparedBody<'_, '_>, SharedAlloc<'static>> = Vec::with_capacity_in(n, shared);
    for res in prepared_results {
        prepared.push(res.map_err(ExportLtoError::from)?);
    }

    let m = prepared.len();
    if m == 0 {
        times.prep_ns = cache.now_ns().saturating_sub(t_prep);
        return Ok(ExportLtoOut {
            bodies: Vec::new_in(shared),
            extra_walk_instances: Vec::new_in(shared),
            extra_walk_statics: Vec::new_in(shared),
            extra_walk_tls: Vec::new_in(shared),
            hot_symbols,
            profile_digest,
            module_bodies: n,
            total_inlined: lto.total_inlined_bodies,
            times,
        });
    }

    // 3b. Index surviving bodies, construct post-LTO direct call graph CSR, and compute
    //     reverse-topological (callees-first) Tarjan SCCs.
    let mut surv_by_sym: SymIndexMap = Map::new(MapOpts { hasher: FixedState, alloc: shared, capacity: Some(m) });
    let mut walk_row_to_surv: AVec<'a, u32> = Vec::with_capacity_in(walk.len(), alloc);
    walk_row_to_surv.resize(walk.len(), u32::MAX);
    for (i, prep) in prepared.iter().enumerate() {
        surv_by_sym.insert(prep.symbol.0, i);
        if let Ok(i_u32) = u32::try_from(i)
            && let Some(slot) = walk_row_to_surv.get_mut(prep.row)
        {
            *slot = i_u32;
        }
    }

    let mut adj_offsets: AVec<'a, u32> = Vec::with_capacity_in(m.saturating_add(1), alloc);
    let mut adj: AVec<'a, u32> = Vec::with_capacity_in(m.saturating_mul(2), alloc);
    let mut in_degree: AVec<'a, usize> = Vec::with_capacity_in(m, alloc);
    in_degree.resize(m, 0);
    let mut callees_per_fn: AVec<'a, AVec<'a, usize>> = Vec::with_capacity_in(m, alloc);
    adj_offsets.push(0);

    for prep in prepared.iter() {
        let view = prep.verified.view();
        let edge_start = adj.len();
        for inst in 0..view.len() {
            if let Some(callee_idx) =
                direct_internal_callee_idx(view, inst, prep.funcs.as_slice(), &surv_by_sym)
                && let Ok(callee_u32) = u32::try_from(callee_idx)
            {
                adj.push(callee_u32);
            }
        }
        if let Some(slice) = adj.get_mut(edge_start..) {
            slice.sort_unstable();
        }
        let mut w = edge_start;
        let mut prev: Option<u32> = None;
        for r in edge_start..adj.len() {
            let Some(&val) = adj.get(r) else {
                break;
            };
            if prev != Some(val) {
                if let Some(dst) = adj.get_mut(w) {
                    *dst = val;
                }
                w = w.saturating_add(1);
                prev = Some(val);
            }
        }
        adj.truncate(w);

        let mut row_callees: AVec<'a, usize> = Vec::with_capacity_in(w.saturating_sub(edge_start), alloc);
        for &callee_u32 in adj.get(edge_start..w).unwrap_or(&[]) {
            if let Ok(cidx) = usize::try_from(callee_u32)
                && cidx < m
            {
                row_callees.push(cidx);
                if let Some(deg) = in_degree.get_mut(cidx) {
                    *deg = deg.saturating_add(1);
                }
            }
        }
        callees_per_fn.push(row_callees);

        let next_off = u32::try_from(adj.len()).map_err(|_| ExportLtoError::BadRef {
            symbol: prep.symbol.0,
            what: "post-LTO call graph CSR offset",
            ref_id: 0,
        })?;
        adj_offsets.push(next_off);
    }

    let emit_sccs = tarjan_sccs(m, adj_offsets.as_slice(), adj.as_slice(), alloc);

    // 3c. Identify ABI-pinned roots (walk roots such as `main`/`lang_start`/personality, bodies
    //     declared by any non-surviving walk row or static, bodies with zero internal callers,
    //     and any symbol address-taken via `Op::FuncAddr`, `data_relocs`, or vtable datums) and
    //     run whole-module Zero-ABI planning (`ScratchOrder` + guarded `ColdPreserveAll`).
    let mut is_exported: AVec<'a, bool> = Vec::with_capacity_in(m, alloc);
    let mut address_taken: AVec<'a, bool> = Vec::with_capacity_in(m, alloc);
    for i in 0..m {
        let root_or_uncalled = prepared
            .get(i)
            .is_none_or(|p| walk.declarer(p.row).is_none())
            || in_degree.get(i).copied().unwrap_or(0) == 0;
        is_exported.push(root_or_uncalled);
        address_taken.push(false);
    }
    for r in 0..walk.len() {
        let r_is_surv = walk_row_to_surv.get(r).copied().unwrap_or(u32::MAX) != u32::MAX;
        if r_is_surv {
            continue;
        }
        let from_non_instance = walk.kind(r) != RowKind::Instance;
        for &decl_u32 in walk.declared(r) {
            if let Ok(decl_row) = usize::try_from(decl_u32)
                && let Some(&s_u32) = walk_row_to_surv.get(decl_row)
                && s_u32 != u32::MAX
                && let Ok(si) = usize::try_from(s_u32)
            {
                if let Some(slot) = is_exported.get_mut(si) {
                    *slot = true;
                }
                if from_non_instance
                    && let Some(slot) = address_taken.get_mut(si)
                {
                    *slot = true;
                }
            }
        }
    }
    for prep in prepared.iter() {
        let view = prep.verified.view();
        for inst in 0..view.len() {
            if view.op(inst) == Op::FuncAddr
                && let Some(&ref_word) = view.data.get(inst)
                && let Some(r) = view.refs.get(RefId(ref_word).index())
                && RefKind::from_word(r.kind) == Some(RefKind::Func)
                && let Ok(place) = usize::try_from(r.place)
                && let Some(&Callee::Symbol(sym)) = prep.funcs.get(place)
                && let Some(tidx) = find_body_idx(&surv_by_sym, sym.0)
                && let Some(slot) = address_taken.get_mut(tidx)
            {
                *slot = true;
            }
        }
        for reloc in view.data_relocs {
            if let Some(r) = view.refs.get(RefId(reloc.target).index())
                && RefKind::from_word(r.kind) == Some(RefKind::Func)
                && let Ok(place) = usize::try_from(r.place)
                && let Some(&Callee::Symbol(sym)) = prep.funcs.get(place)
                && let Some(tidx) = find_body_idx(&surv_by_sym, sym.0)
                && let Some(slot) = address_taken.get_mut(tidx)
            {
                *slot = true;
            }
        }
        for (_, bytes) in prep.vtable_data.iter() {
            if let Ok(dv) = DatumView::read(bytes.as_slice()) {
                for ti in 0..dv.targets() {
                    let target = dv.target(ti);
                    if target.space == Space::Func
                        && let Some(tidx) = find_body_idx(&surv_by_sym, target.symbol.0)
                        && let Some(slot) = address_taken.get_mut(tidx)
                    {
                        *slot = true;
                    }
                }
            }
        }
    }

    // Parallel per-body pre-ABI scan: caller live pressure + AArch64 `InternalFast` eligibility +
    // `ColdPreserveAll` safety.
    let is_arm64 = opts.machine.arch == Arch::Arm64;
    let prepared_slice = prepared.as_slice();
    let machine = opts.machine;
    struct BodyAbiProbe {
        pressure_gprs: RegSet,
        pressure_fps: RegSet,
        misel_caller_ok: bool,
        misel_callee_ok: bool,
        safe_cold_preserve_all: bool,
    }
    let abi_probes: Vec<BodyAbiProbe, SharedAlloc<'static>> = runner
        .par_map(m, shared, |i, _scratch| {
            let prep = &prepared_slice[i];
            let view = prep.verified.view();
            let (pressure_gprs, pressure_fps) = estimate_caller_live_pressure(view);
            let (misel_caller_ok, misel_callee_ok) = if is_arm64 {
                let names = InstanceNames {
                    funcs: prep.funcs.as_slice(),
                    statics: prep.statics.as_slice(),
                    c_statics: prep.c_statics.as_slice(),
                    vtables: prep.vtables.as_slice(),
                    tls: prep.tls.as_slice(),
                };
                (
                    can_use_internal_fast_a64(view, &names, machine, InternalFastRole::Caller),
                    can_use_internal_fast_a64(view, &names, machine, InternalFastRole::Callee),
                )
            } else {
                (false, false)
            };
            let safe_cold_preserve_all = is_arm64 && is_safe_cold_preserve_all(view);
            BodyAbiProbe {
                pressure_gprs,
                pressure_fps,
                misel_caller_ok,
                misel_callee_ok,
                safe_cold_preserve_all,
            }
        })
        .map_err(ExportLtoError::Optimize)?;

    let abi_plan = {
        const EMPTY_CALLEES: &[usize] = &[];
        let mut abi_inputs: AVec<'a, AbiFnInput<'_>> = Vec::with_capacity_in(m, alloc);
        for (i, prep) in prepared.iter().enumerate() {
            let view = prep.verified.view();
            let probe = &abi_probes[i];
            abi_inputs.push(AbiFnInput {
                view,
                is_exported: is_exported.get(i).copied().unwrap_or(true),
                address_taken: address_taken.get(i).copied().unwrap_or(true),
                is_cold: (view.head.attrs & FnAttrs::COLD) != 0,
                direct_callees: callees_per_fn.get(i).map_or(EMPTY_CALLEES, Vec::as_slice),
                caller_live_pressure_gprs: probe.pressure_gprs,
                caller_live_pressure_fps: probe.pressure_fps,
            });
        }
        let mut zero_abi_opts = ZeroAbiOpts::arm64_export();
        zero_abi_opts.enable_scc_pinning = false;
        plan_module_abi(abi_inputs.as_slice(), zero_abi_opts, alloc)
    };

    // 3d. Wavefront-parallel bottom-up Tarjan SCC emission with IPRA, `InternalFast`,
    //     interprocedural `ScratchOrder`, guarded `ColdPreserveAll`, 2-tier cold-fast mode, and
    //     content-addressed per-body incremental caching.
    let num_sccs = emit_sccs.len();
    let mut scc_of: AVec<'a, u32> = Vec::with_capacity_in(m, alloc);
    scc_of.resize(m, 0);
    let mut scc_size: AVec<'a, usize> = Vec::with_capacity_in(m, alloc);
    scc_size.resize(m, 0);
    for scc_idx in 0..num_sccs {
        let comp = emit_sccs.component(scc_idx);
        let sz = comp.len();
        let scc_u32 = u32::try_from(scc_idx).unwrap_or(u32::MAX);
        for &node_u32 in comp {
            if let Ok(node_idx) = usize::try_from(node_u32) {
                if let Some(slot) = scc_size.get_mut(node_idx) {
                    *slot = sz;
                }
                if let Some(slot) = scc_of.get_mut(node_idx) {
                    *slot = scc_u32;
                }
            }
        }
    }

    let mut callers_per_fn: AVec<'a, AVec<'a, usize>> = Vec::with_capacity_in(m, alloc);
    for _ in 0..m {
        callers_per_fn.push(Vec::new_in(alloc));
    }
    for (caller_idx, callees) in callees_per_fn.iter().enumerate() {
        for &callee_idx in callees.iter() {
            if let Some(callers) = callers_per_fn.get_mut(callee_idx) {
                callers.push(caller_idx);
            }
        }
    }

    let mut use_internal_fast: Vec<bool, SharedAlloc<'static>> = Vec::with_capacity_in(m, shared);
    for (f, prep) in prepared.iter().enumerate() {
        let ok = is_arm64
            && abi_plan.plans.get(f).is_some_and(|p| p.conv == CallConvKind::InternalFast)
            && has_wide_args(prep.verified.view())
            && abi_probes[f].misel_callee_ok
            && scc_size.get(f).copied().unwrap_or(0) == 1
            && callers_per_fn.get(f).is_some_and(|callers| {
                !callers.is_empty() && callers.iter().all(|&c| abi_probes[c].misel_caller_ok)
            });
        use_internal_fast.push(ok);
    }

    // Group reverse-topological Tarjan SCCs into independent wavefront levels: every cross-SCC
    // callee of level `L` lies strictly in levels `< L`, so all SCCs in level `L` observe the
    // exact same finalized `known_clobbers` as sequential bottom-up SCC emission (Rule 25).
    let mut scc_level: AVec<'a, u32> = Vec::with_capacity_in(num_sccs, alloc);
    scc_level.resize(num_sccs, 0);
    let mut max_level: u32 = 0;
    for scc_idx in 0..num_sccs {
        let comp = emit_sccs.component(scc_idx);
        let mut lvl = 0u32;
        for &node_u32 in comp {
            let Ok(node_idx) = usize::try_from(node_u32) else {
                continue;
            };
            let start = usize::try_from(adj_offsets.get(node_idx).copied().unwrap_or(0)).unwrap_or(0);
            let end = usize::try_from(adj_offsets.get(node_idx.saturating_add(1)).copied().unwrap_or(0))
                .unwrap_or(start);
            for &callee_u32 in adj.get(start..end).unwrap_or(&[]) {
                let Ok(callee_idx) = usize::try_from(callee_u32) else {
                    continue;
                };
                let callee_scc = usize::try_from(scc_of.get(callee_idx).copied().unwrap_or(0)).unwrap_or(0);
                if callee_scc != scc_idx {
                    let next = scc_level.get(callee_scc).copied().unwrap_or(0).saturating_add(1);
                    if next > lvl {
                        lvl = next;
                    }
                }
            }
        }
        if let Some(slot) = scc_level.get_mut(scc_idx) {
            *slot = lvl;
        }
        if lvl > max_level {
            max_level = lvl;
        }
    }
    let num_levels = usize::try_from(max_level).unwrap_or(0).saturating_add(1);
    let mut level_counts: AVec<'a, usize> = Vec::with_capacity_in(num_levels, alloc);
    level_counts.resize(num_levels, 0);
    for &lvl in scc_level.iter() {
        let l = usize::try_from(lvl).unwrap_or(0);
        if let Some(c) = level_counts.get_mut(l) {
            *c = c.saturating_add(1);
        }
    }
    let mut level_offsets: AVec<'a, usize> = Vec::with_capacity_in(num_levels.saturating_add(1), alloc);
    let mut acc = 0usize;
    for &c in level_counts.iter() {
        level_offsets.push(acc);
        acc = acc.saturating_add(c);
    }
    level_offsets.push(acc);
    let mut level_cursor: AVec<'a, usize> = Vec::with_capacity_in(num_levels, alloc);
    level_cursor.extend_from_slice(&level_offsets[..num_levels]);
    let mut level_sccs: AVec<'a, usize> = Vec::with_capacity_in(num_sccs, alloc);
    level_sccs.resize(num_sccs, 0);
    for (scc_idx, &lvl) in scc_level.iter().enumerate() {
        let l = usize::try_from(lvl).unwrap_or(0);
        if let Some(cur) = level_cursor.get_mut(l) {
            if let Some(dst) = level_sccs.get_mut(*cur) {
                *dst = scc_idx;
            }
            *cur = cur.saturating_add(1);
        }
    }
    times.prep_ns = cache.now_ns().saturating_sub(t_prep);

    let t_emit = cache.now_ns();
    let producer_digest = cache.producer_digest();
    let mut known_clobbers: Vec<Option<CallClobbers>, SharedAlloc<'static>> = Vec::with_capacity_in(m, shared);
    let mut emitted_records: Vec<Option<EmittedBodyRec>, SharedAlloc<'static>> = Vec::with_capacity_in(m, shared);
    for _ in 0..m {
        known_clobbers.push(None);
        emitted_records.push(None);
    }

    #[derive(Clone, Copy)]
    struct EmitAbiCfg {
        scratch_order: Option<ScratchOrder>,
        preserve_all_cold: bool,
        internal_fast: bool,
    }

    let mut emit_abi_cfg: Vec<EmitAbiCfg, SharedAlloc<'static>> = Vec::with_capacity_in(m, shared);
    for (idx, &internal_fast) in use_internal_fast.iter().enumerate() {
        let fn_plan = abi_plan.plans.get(idx);
        let scratch_order = if is_arm64 {
            fn_plan.map(|p| ScratchOrder {
                gpr: p.scratch_gpr_order,
                fp: p.scratch_fp_order,
            })
        } else {
            None
        };
        let preserve_all_cold = is_arm64
            && !internal_fast
            && fn_plan.is_some_and(|p| p.conv == CallConvKind::ColdPreserveAll)
            && abi_probes[idx].safe_cold_preserve_all;
        emit_abi_cfg.push(EmitAbiCfg { scratch_order, preserve_all_cold, internal_fast });
    }

    struct EmitNodeOut {
        idx: usize,
        record: Vec<u8, SharedAlloc<'static>>,
        extra_declared: Vec<(ItemKey, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>>,
        clobbers: CallClobbers,
        cache_put: Option<([u8; 32], Vec<u8, SharedAlloc<'static>>)>,
    }

    let surv_by_sym_ref = &surv_by_sym;
    let use_internal_fast_slice = use_internal_fast.as_slice();
    let emit_abi_cfg_slice = emit_abi_cfg.as_slice();
    let mut level_nodes: Vec<usize, SharedAlloc<'static>> = Vec::new_in(shared);
    let mut level_misses: Vec<(usize, Option<[u8; 32]>), SharedAlloc<'static>> = Vec::new_in(shared);
    let mut level_node_clobbers: AVec<'a, Option<CallClobbers>> = Vec::with_capacity_in(m, alloc);
    level_node_clobbers.resize(m, None);

    for lvl in 0..num_levels {
        let l_start = level_offsets.get(lvl).copied().unwrap_or(0);
        let l_end = level_offsets.get(lvl.saturating_add(1)).copied().unwrap_or(l_start);
        let sccs_in_level = level_sccs.get(l_start..l_end).unwrap_or(&[]);
        level_nodes.clear();
        for &scc_idx in sccs_in_level {
            for &node_u32 in emit_sccs.component(scc_idx) {
                let idx = usize::try_from(node_u32).map_err(|_| ExportLtoError::BadRef {
                    symbol: [0u8; 32],
                    what: "SCC node index",
                    ref_id: node_u32,
                })?;
                if idx >= m {
                    return Err(ExportLtoError::BadRef {
                        symbol: [0u8; 32],
                        what: "prepared body index",
                        ref_id: node_u32,
                    });
                }
                level_nodes.push(idx);
            }
        }
        if level_nodes.is_empty() {
            continue;
        }

        let level_nodes_slice = level_nodes.as_slice();
        let known_clobbers_slice = known_clobbers.as_slice();
        level_misses.clear();

        // Phase A: compute content-addressed emit keys in parallel (when caching is active) and
        // batch-probe `cache.get` on the caller's thread with zero lock contention.
        if let Some(producer) = producer_digest {
            let level_keys: Vec<[u8; 32], SharedAlloc<'static>> = runner
                .par_map(level_nodes_slice.len(), shared, |pos, fn_alloc| {
                    let idx = level_nodes_slice[pos];
                    let prep = &prepared_slice[idx];
                    let cfg = emit_abi_cfg_slice[idx];
                    let mut callee_clobbers_buf: AVec<'_, (u32, CallClobbers)> = Vec::new_in(fn_alloc);
                    let mut internal_fast_callees_buf: AVec<'_, u32> = Vec::new_in(fn_alloc);
                    if is_arm64 {
                        for (place, callee) in prep.funcs.iter().enumerate() {
                            if let &Callee::Symbol(callee_sym) = callee
                                && let Some(callee_idx) = find_body_idx(surv_by_sym_ref, callee_sym.0)
                                && let Ok(place_u32) = u32::try_from(place)
                            {
                                if let Some(Some(clob)) = known_clobbers_slice.get(callee_idx) {
                                    callee_clobbers_buf.push((place_u32, *clob));
                                }
                                if use_internal_fast_slice[callee_idx] {
                                    internal_fast_callees_buf.push(place_u32);
                                }
                            }
                        }
                    }
                    let abi_opts = SotaRecordAbiOpts {
                        machine,
                        is_hot: prep.is_hot,
                        scratch_order: cfg.scratch_order,
                        preserve_all_cold: cfg.preserve_all_cold,
                        callee_clobbers: callee_clobbers_buf.as_slice(),
                        internal_fast: cfg.internal_fast,
                        internal_fast_callees: internal_fast_callees_buf.as_slice(),
                    };
                    let names = InstanceNames {
                        funcs: prep.funcs.as_slice(),
                        statics: prep.statics.as_slice(),
                        c_statics: prep.c_statics.as_slice(),
                        vtables: prep.vtables.as_slice(),
                        tls: prep.tls.as_slice(),
                    };
                    compute_emit_cache_key(
                        producer,
                        prep.verified.view(),
                        &names,
                        prep.routines,
                        prep.traps,
                        abi_opts,
                        fn_alloc,
                    )
                })
                .map_err(ExportLtoError::Optimize)?;

            for (&idx, &key) in level_nodes_slice.iter().zip(level_keys.iter()) {
                if let Some(cached) = cache.get(key, shared) {
                    let (record, extra_declared, clobbers) = decode_emitted_body(cached.as_slice(), shared)
                        .ok_or(ExportLtoError::CorruptCache { what: "EmittedBody", key })?;
                    times.emit_hits = times.emit_hits.saturating_add(1);
                    if let Some(slot) = emitted_records.get_mut(idx) {
                        *slot = Some(EmittedBodyRec { record, extra_declared });
                    }
                    if let Some(slot) = level_node_clobbers.get_mut(idx) {
                        *slot = Some(clobbers);
                    }
                } else {
                    level_misses.push((idx, Some(key)));
                }
            }
        } else {
            for &idx in level_nodes_slice.iter() {
                level_misses.push((idx, None));
            }
        }

        // Phase B: emit ONLY cache misses in parallel across `runner` (all 18 cores on cold builds;
        // 1 body on incremental 1-fn edits!).
        if !level_misses.is_empty() {
            let level_misses_slice = level_misses.as_slice();
            let miss_results = runner
                .par_map(level_misses_slice.len(), shared, |pos, fn_alloc| -> Result<EmitNodeOut, ParLtoErr> {
                    let (idx, emit_key) = level_misses_slice[pos];
                    let prep = &prepared_slice[idx];
                    let cfg = emit_abi_cfg_slice[idx];
                    let mut callee_clobbers_buf: AVec<'_, (u32, CallClobbers)> = Vec::new_in(fn_alloc);
                    let mut internal_fast_callees_buf: AVec<'_, u32> = Vec::new_in(fn_alloc);
                    if is_arm64 {
                        for (place, callee) in prep.funcs.iter().enumerate() {
                            if let &Callee::Symbol(callee_sym) = callee
                                && let Some(callee_idx) = find_body_idx(surv_by_sym_ref, callee_sym.0)
                                && let Ok(place_u32) = u32::try_from(place)
                            {
                                if let Some(Some(clob)) = known_clobbers_slice.get(callee_idx) {
                                    callee_clobbers_buf.push((place_u32, *clob));
                                }
                                if use_internal_fast_slice[callee_idx] {
                                    internal_fast_callees_buf.push(place_u32);
                                }
                            }
                        }
                    }

                    let abi_opts = SotaRecordAbiOpts {
                        machine,
                        is_hot: prep.is_hot,
                        scratch_order: cfg.scratch_order,
                        preserve_all_cold: cfg.preserve_all_cold,
                        callee_clobbers: callee_clobbers_buf.as_slice(),
                        internal_fast: cfg.internal_fast,
                        internal_fast_callees: internal_fast_callees_buf.as_slice(),
                    };
                    let names = InstanceNames {
                        funcs: prep.funcs.as_slice(),
                        statics: prep.statics.as_slice(),
                        c_statics: prep.c_statics.as_slice(),
                        vtables: prep.vtables.as_slice(),
                        tls: prep.tls.as_slice(),
                    };

                    let (record_tmp, called_declared, clobbers) = emit_code_record_for_machine_with_abi(
                        prep.verified,
                        &names,
                        prep.routines,
                        prep.traps,
                        abi_opts,
                        fn_alloc,
                    )
                    .map_err(|error| ParLtoErr::Record { symbol: prep.symbol.0, error })?;

                    let mut record: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(record_tmp.len(), shared);
                    record.extend_from_slice(record_tmp.as_slice());
                    let mut extra_declared: Vec<(ItemKey, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>> =
                        Vec::with_capacity_in(called_declared.len(), shared);
                    for d in called_declared.iter() {
                        let mut name_buf: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(d.name.len(), shared);
                        name_buf.extend_from_slice(d.name);
                        extra_declared.push((d.tabled, name_buf));
                    }
                    let cache_put = emit_key.and_then(|key| {
                        encode_emitted_body(record.as_slice(), extra_declared.as_slice(), clobbers, shared)
                            .map(|encoded| (key, encoded))
                    });
                    Ok(EmitNodeOut {
                        idx,
                        record,
                        extra_declared,
                        clobbers,
                        cache_put,
                    })
                })
                .map_err(ExportLtoError::Optimize)?;

            // Deterministically commit this level's missed records and cache writes before moving
            // to level `lvl + 1`.
            for res in miss_results {
                let out = res.map_err(ExportLtoError::from)?;
                times.emit_misses = times.emit_misses.saturating_add(1);
                if let Some((key, encoded)) = out.cache_put {
                    cache.put(key, encoded.as_slice());
                }
                if let Some(slot) = emitted_records.get_mut(out.idx) {
                    *slot = Some(EmittedBodyRec {
                        record: out.record,
                        extra_declared: out.extra_declared,
                    });
                }
                if let Some(slot) = level_node_clobbers.get_mut(out.idx) {
                    *slot = Some(out.clobbers);
                }
            }
        }

        for &scc_idx in sccs_in_level {
            let comp = emit_sccs.component(scc_idx);
            if comp.len() == 1 {
                if let Some(&node_u32) = comp.first()
                    && let Ok(node_idx) = usize::try_from(node_u32)
                    && let Some(clob) = level_node_clobbers.get(node_idx).copied().flatten()
                    && let Some(slot) = known_clobbers.get_mut(node_idx)
                {
                    *slot = Some(clob);
                }
            } else if comp.len() > 1 {
                let mut scc_union = CallClobbers::EMPTY;
                for &node_u32 in comp {
                    if let Ok(node_idx) = usize::try_from(node_u32)
                        && let Some(clob) = level_node_clobbers.get(node_idx).copied().flatten()
                    {
                        scc_union.clobbered = scc_union.clobbered.union(clob.clobbered);
                    }
                }
                for &node_u32 in comp {
                    if let Ok(node_idx) = usize::try_from(node_u32)
                        && let Some(slot) = known_clobbers.get_mut(node_idx)
                    {
                        *slot = Some(scc_union);
                    }
                }
            }
        }
    }
    times.emit_ns = cache.now_ns().saturating_sub(t_emit);

    // 3e. Reassemble `out_bodies` in exact original body order (Rule 25).
    let mut out_bodies: Vec<LtoBodyOut, SharedAlloc<'static>> = Vec::with_capacity_in(m, shared);
    for (prep, rec_opt) in prepared.into_iter().zip(emitted_records.into_iter()) {
        if let Some(emitted) = rec_opt {
            out_bodies.push(LtoBodyOut {
                row: prep.row,
                symbol: prep.symbol,
                record: emitted.record,
                vtable_data: prep.vtable_data,
                extra_declared: emitted.extra_declared,
                inlined_bodies: prep.inlined_bodies,
            });
        }
    }

    let missing = collect_missing_walk_targets(walk, origins.as_slice(), out_bodies.as_slice(), shared, alloc)?;

    Ok(ExportLtoOut {
        bodies: out_bodies,
        extra_walk_instances: missing.instances,
        extra_walk_statics: missing.statics,
        extra_walk_tls: missing.tls,
        hot_symbols,
        profile_digest,
        module_bodies: n,
        total_inlined: lto.total_inlined_bodies,
        times,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_lir::{
        Builder, Full,
        builder::{Param, Variadic},
    };
    use blaze_tier::Os as TierOs;

    fn arm64_machine() -> Machine {
        Machine {
            arch: Arch::Arm64,
            os: TierOs::MacOs,
            features: Machine::LSE | Machine::DOTPROD | Machine::CRC32 | Machine::SME | Machine::SME2,
        }
    }

    fn x64_machine(features: u64) -> Machine {
        Machine {
            arch: Arch::X86_64,
            os: TierOs::Linux,
            features,
        }
    }

    fn make_built_with_c_callees(
        func: blaze_lir::Func<Alloc<'static>>,
        c_names: &[&str],
        alloc: Alloc<'static>,
    ) -> BuiltLir<Alloc<'static>> {
        let mut funcs: AVec<'static, BuiltCallee> = Vec::with_capacity_in(c_names.len(), alloc);
        let mut c_text: AVec<'static, u8> = Vec::new_in(alloc);
        for &name in c_names {
            let start = u32::try_from(c_text.len()).expect("c_text fits u32");
            let len = u32::try_from(name.len()).expect("c_name fits u32");
            c_text.extend_from_slice(name.as_bytes());
            funcs.push(BuiltCallee::C { start, len });
        }
        BuiltLir {
            func,
            funcs,
            statics: Vec::new_in(alloc),
            c_statics: Vec::new_in(alloc),
            vtables: Vec::new_in(alloc),
            vtable_data: Vec::new_in(alloc),
            tls: Vec::new_in(alloc),
            traps: Vec::new_in(alloc),
            c_text,
        }
    }

    fn build_c_call_fn(
        c_name: &str,
        param_tys: &[Ty],
        ret_ty: Ty,
        variadic: Variadic,
        alloc: Alloc<'static>,
    ) -> BuiltLir<Alloc<'static>> {
        let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
        let sig_params: std::vec::Vec<Param> = param_tys.iter().copied().map(Param::of).collect();
        let ret_params = [Param::of(ret_ty)];
        let own_sig = b.sig(Conv::C, Variadic::No, &sig_params, &ret_params);
        let callee_sig = b.sig(Conv::C, variadic, &sig_params, &ret_params);
        b.set_signature(own_sig);
        let callee_ref = b.reference(RefKind::Func, 0);

        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let args: std::vec::Vec<_> = param_tys.iter().copied().map(|ty| b.param(ty)).collect();
        let res = b.call(Flags::NONE, callee_ref, callee_sig, &args);
        b.ret(&[res]);

        let func = b.finish_checked().expect("valid initial LIR");
        make_built_with_c_callees(func, &[c_name], alloc)
    }

    #[test]
    fn lowers_call_fmaf_and_underscored_fma() {
        let alloc = blaze_own::testing::heap().alloc();
        for (name, ty) in [("fmaf", Ty::F32), ("_fmaf", Ty::F32), ("fma", Ty::F64), ("_fma", Ty::F64)] {
            let mut built = build_c_call_fn(name, &[ty, ty, ty], ty, Variadic::No, alloc);
            assert!(lower_ieee_libm_calls(&mut built, arm64_machine(), alloc), "{name} should lower");
            verify(built.func.view()).expect("lowered LIR must verify");

            let view = built.func.view();
            let call_count = (0..view.len()).filter(|&i| matches!(view.op(i), Op::Call | Op::Invoke)).count();
            assert_eq!(call_count, 0, "{name} should have no calls left");

            let fma_inst = (0..view.len()).find(|&i| view.op(i) == Op::Fma).expect("expected Op::Fma");
            assert_eq!(Ty(view.tys[fma_inst]), ty);
            assert_eq!(view.flags(fma_inst), Flags::NONE);
            let mut ops = view.operands(fma_inst);
            assert_eq!(ops.value().0, 0);
            assert_eq!(ops.value().0, 1);
            assert_eq!(ops.value().0, 2);
        }
    }

    #[test]
    fn lowers_call_unary_ieee_libm_ops_on_arm64() {
        let alloc = blaze_own::testing::heap().alloc();
        let cases = [
            ("sqrtf", Ty::F32, Op::Sqrt),
            ("sqrt", Ty::F64, Op::Sqrt),
            ("_sqrtf", Ty::F32, Op::Sqrt),
            ("fabsf", Ty::F32, Op::Fabs),
            ("fabs", Ty::F64, Op::Fabs),
            ("floorf", Ty::F32, Op::Floor),
            ("floor", Ty::F64, Op::Floor),
            ("ceilf", Ty::F32, Op::Ceil),
            ("ceil", Ty::F64, Op::Ceil),
            ("truncf", Ty::F32, Op::Ftrunc),
            ("trunc", Ty::F64, Op::Ftrunc),
            ("rintf", Ty::F32, Op::Nearest),
            ("rint", Ty::F64, Op::Nearest),
            ("roundf", Ty::F32, Op::Fround),
            ("round", Ty::F64, Op::Fround),
        ];
        for (name, ty, expected_op) in cases {
            let mut built = build_c_call_fn(name, &[ty], ty, Variadic::No, alloc);
            assert!(lower_ieee_libm_calls(&mut built, arm64_machine(), alloc), "{name} should lower");
            verify(built.func.view()).expect("lowered unary libm LIR must verify");

            let view = built.func.view();
            assert_eq!(
                (0..view.len()).filter(|&i| matches!(view.op(i), Op::Call | Op::Invoke)).count(),
                0
            );
            let inst = (0..view.len()).find(|&i| view.op(i) == expected_op).expect("expected lowered unary op");
            assert_eq!(Ty(view.tys[inst]), ty);
            assert_eq!(view.flags(inst), Flags::NONE);
            assert_eq!(view.operands(inst).value().0, 0);
        }
    }

    #[test]
    fn lowers_chained_invoke_fmaf_and_sqrtf_and_verifies() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
        let own_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::F32), Param::of(Ty::F32), Param::of(Ty::F32), Param::of(Ty::F32)],
            &[Param::of(Ty::F32)],
        );
        let fma_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::F32), Param::of(Ty::F32), Param::of(Ty::F32)],
            &[Param::of(Ty::F32)],
        );
        let sqrt_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::F32)], &[Param::of(Ty::F32)]);
        b.set_signature(own_sig);

        let fmaf_ref = b.reference(RefKind::Func, 0);
        let sqrtf_ref = b.reference(RefKind::Func, 1);

        let entry = b.create_block(BlockKind::Normal);
        let after_fma = b.create_block(BlockKind::Normal);
        let after_sqrt = b.create_block(BlockKind::Normal);
        let cleanup = b.create_block(BlockKind::Cleanup);

        b.begin(entry);
        let a = b.param(Ty::F32);
        let x = b.param(Ty::F32);
        let c = b.param(Ty::F32);
        let bias = b.param(Ty::F32);
        let fma_val = b.invoke(Flags::NONE, fmaf_ref, fma_sig, &[a, x, c], after_fma, cleanup);

        b.begin(after_fma);
        let shifted = b.binary(Op::Fadd, Flags::NONE, fma_val, bias);
        let sqrt_val = b.invoke(Flags::NONE, sqrtf_ref, sqrt_sig, &[shifted], after_sqrt, cleanup);

        b.begin(after_sqrt);
        let final_val = b.binary(Op::Fadd, Flags::NONE, sqrt_val, fma_val);
        b.ret(&[final_val]);

        b.begin(cleanup);
        let exc = b.param(Ty::PTR);
        b.resume(exc);

        let func = b.finish_checked().expect("valid initial invoke LIR");
        let mut built = make_built_with_c_callees(func, &["fmaf", "sqrtf"], alloc);

        assert!(lower_ieee_libm_calls(&mut built, arm64_machine(), alloc));
        verify(built.func.view()).expect("lowered invoke LIR must verify");

        let view = built.func.view();
        assert_eq!(
            (0..view.len()).filter(|&i| matches!(view.op(i), Op::Call | Op::Invoke)).count(),
            0,
            "all libm invokes should be lowered"
        );
        let fma_inst = (0..view.len()).find(|&i| view.op(i) == Op::Fma).expect("expected Op::Fma");
        let sqrt_inst = (0..view.len()).find(|&i| view.op(i) == Op::Sqrt).expect("expected Op::Sqrt");
        assert_eq!(view.op(fma_inst + 1), Op::Jump);
        assert_eq!(view.op(sqrt_inst + 1), Op::Jump);

        let ret_inst = (0..view.len()).find(|&i| view.op(i) == Op::Return).expect("expected Op::Return");
        let ret_val = view.operands(ret_inst).values().get(0);
        assert_eq!(view.op(ret_val.index()), Op::Fadd);
        let mut final_ops = view.operands(ret_val.index());
        assert_eq!(final_ops.value().index(), sqrt_inst);
        assert_eq!(final_ops.value().index(), fma_inst);
    }

    #[test]
    fn leaves_unknown_mismatched_variadic_and_unsupported_targets_untouched() {
        let alloc = blaze_own::testing::heap().alloc();

        // Unknown C functions and ops unsupported by `sota` (`copysignf`/`copysign`) stay untouched.
        for (name, params, ret, variadic) in [
            ("sinf", &[Ty::F32][..], Ty::F32, Variadic::No),
            ("printf", &[Ty::PTR][..], Ty::I32, Variadic::Yes),
            ("copysignf", &[Ty::F32, Ty::F32][..], Ty::F32, Variadic::No),
            ("copysign", &[Ty::F64, Ty::F64][..], Ty::F64, Variadic::No),
            // Mismatched signatures for known names: wrong precision, wrong arity, or variadic.
            ("fmaf", &[Ty::F64, Ty::F64, Ty::F64][..], Ty::F64, Variadic::No),
            ("fma", &[Ty::F32, Ty::F32, Ty::F32][..], Ty::F32, Variadic::No),
            ("fmaf", &[Ty::F32, Ty::F32][..], Ty::F32, Variadic::No),
            ("sqrtf", &[Ty::F64][..], Ty::F64, Variadic::No),
            ("sqrt", &[Ty::F32][..], Ty::F32, Variadic::No),
            ("fmaf", &[Ty::F32, Ty::F32, Ty::F32][..], Ty::F32, Variadic::Yes),
        ] {
            let mut built = build_c_call_fn(name, params, ret, variadic, alloc);
            assert!(
                !lower_ieee_libm_calls(&mut built, arm64_machine(), alloc),
                "{name} ({params:?} -> {ret:?}, {variadic:?}) must not lower"
            );
            verify(built.func.view()).expect("untouched LIR must verify");
            assert_eq!(
                (0..built.func.view().len()).filter(|&i| built.func.view().op(i) == Op::Call).count(),
                1
            );
        }

        // x86-64 gating: `Op::Fma` requires `Machine::FMA`; `Op::Sqrt` is unsupported on x86-64 sota.
        let mut fma_no_feat = build_c_call_fn("fmaf", &[Ty::F32, Ty::F32, Ty::F32], Ty::F32, Variadic::No, alloc);
        assert!(!lower_ieee_libm_calls(&mut fma_no_feat, x64_machine(0), alloc));

        let mut fma_with_feat = build_c_call_fn("fmaf", &[Ty::F32, Ty::F32, Ty::F32], Ty::F32, Variadic::No, alloc);
        assert!(lower_ieee_libm_calls(&mut fma_with_feat, x64_machine(Machine::FMA), alloc));
        verify(fma_with_feat.func.view()).expect("x86-64 FMA lowered LIR must verify");

        let mut sqrt_x64 = build_c_call_fn("sqrtf", &[Ty::F32], Ty::F32, Variadic::No, alloc);
        assert!(!lower_ieee_libm_calls(&mut sqrt_x64, x64_machine(Machine::FMA | Machine::AVX2), alloc));
    }

    fn build_straight_line_fn(is_cold: bool, alloc: Alloc<'static>) -> blaze_lir::Func<Alloc<'static>> {
        let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        if is_cold {
            b.set_attrs(FnAttrs(FnAttrs::COLD));
        }
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);
        let one = b.iconst(Ty::I64, 1);
        let sum = b.binary(Op::Add, Flags::NONE, x, one);
        b.ret(&[sum]);
        b.finish_checked().expect("valid straight-line LIR")
    }

    fn build_single_loop_leaf(alloc: Alloc<'static>) -> blaze_lir::Func<Alloc<'static>> {
        let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);

        let entry = b.create_block(BlockKind::Normal);
        let header = b.create_block(BlockKind::Normal);
        let body = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let cond = b.icmp(blaze_lir::IntCond::Ult, i, n);
        b.br(Flags::NONE, cond, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let one = b.iconst(Ty::I64, 1);
        let next_i = b.binary(Op::Add, Flags::NONE, i, one);
        let next_acc = b.binary(Op::Add, Flags::NONE, acc, i);
        b.jump((header, &[next_i, next_acc]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        b.finish_checked().expect("valid single-loop leaf LIR")
    }

    fn build_nested_loop_leaf(alloc: Alloc<'static>) -> blaze_lir::Func<Alloc<'static>> {
        let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);

        let entry = b.create_block(BlockKind::Normal);
        let outer_h = b.create_block(BlockKind::Normal);
        let inner_h = b.create_block(BlockKind::Normal);
        let inner_b = b.create_block(BlockKind::Normal);
        let outer_latch = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((outer_h, &[zero, zero]));

        b.begin(outer_h);
        let i = b.param(Ty::I64);
        let outer_acc = b.param(Ty::I64);
        let outer_cond = b.icmp(blaze_lir::IntCond::Ult, i, n);
        b.br(Flags::NONE, outer_cond, (inner_h, &[zero, outer_acc]), (exit, &[outer_acc]));

        b.begin(inner_h);
        let j = b.param(Ty::I64);
        let inner_acc = b.param(Ty::I64);
        let inner_cond = b.icmp(blaze_lir::IntCond::Ult, j, n);
        b.br(Flags::NONE, inner_cond, (inner_b, &[]), (outer_latch, &[inner_acc]));

        b.begin(inner_b);
        let one = b.iconst(Ty::I64, 1);
        let next_j = b.binary(Op::Add, Flags::NONE, j, one);
        let next_acc = b.binary(Op::Add, Flags::NONE, inner_acc, j);
        b.jump((inner_h, &[next_j, next_acc]));

        b.begin(outer_latch);
        let after_inner = b.param(Ty::I64);
        let one2 = b.iconst(Ty::I64, 1);
        let next_i = b.binary(Op::Add, Flags::NONE, i, one2);
        b.jump((outer_h, &[next_i, after_inner]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        b.finish_checked().expect("valid nested-loop leaf LIR")
    }

    #[test]
    fn static_pgo_heat_ranks_nested_loop_leaf_strictly_ahead_of_shallow_and_cold() {
        let alloc = blaze_own::testing::heap().alloc();

        let sym_init = Symbol([0x01; 32]);
        let sym_cold = Symbol([0x02; 32]);
        let sym_shallow_dep = Symbol([0x03; 32]);
        let sym_shallow_seed = Symbol([0x04; 32]);
        let sym_nested_leaf = Symbol([0xFF; 32]);

        let fn_init = build_straight_line_fn(false, alloc);
        let fn_cold = build_straight_line_fn(true, alloc);
        let fn_shallow_dep = build_single_loop_leaf(alloc);
        let fn_shallow_seed = build_single_loop_leaf(alloc);
        let fn_nested_leaf = build_nested_loop_leaf(alloc);

        assert_eq!(compute_loop_heat(fn_init.view(), alloc), LoopHeat { max_depth: 0, loop_blocks: 0 });
        assert_eq!(compute_loop_heat(fn_shallow_seed.view(), alloc), LoopHeat { max_depth: 1, loop_blocks: 2 });
        assert_eq!(compute_loop_heat(fn_nested_leaf.view(), alloc), LoopHeat { max_depth: 2, loop_blocks: 4 });

        let mut mod_bodies: AVec<'static, ModuleBody<Alloc<'static>>> = Vec::new_in(alloc);
        mod_bodies.push(ModuleBody { id: BodyId(sym_init.0), func: fn_init.view().to_func_in(alloc) });
        mod_bodies.push(ModuleBody { id: BodyId(sym_cold.0), func: fn_cold.view().to_func_in(alloc) });
        mod_bodies.push(ModuleBody { id: BodyId(sym_shallow_dep.0), func: fn_shallow_dep.view().to_func_in(alloc) });
        mod_bodies.push(ModuleBody { id: BodyId(sym_shallow_seed.0), func: fn_shallow_seed.view().to_func_in(alloc) });
        mod_bodies.push(ModuleBody { id: BodyId(sym_nested_leaf.0), func: fn_nested_leaf.view().to_func_in(alloc) });

        let pgo = build_pgo_table(mod_bodies.as_slice(), None, alloc).expect("static PGO table");
        let inputs = [
            (sym_init, true, fn_init.view()),
            (sym_cold, true, fn_cold.view()),
            (sym_shallow_dep, false, fn_shallow_dep.view()),
            (sym_shallow_seed, true, fn_shallow_seed.view()),
            (sym_nested_leaf, true, fn_nested_leaf.view()),
        ];
        let hot = rank_hot_symbols(&inputs, &[], &pgo, alloc, alloc);

        // Nested 0-call leaf kernel ranks strictly first despite highest symbol bytes, followed by
        // the single-loop workspace seed, then the single-loop non-seed callee. Straight-line init
        // and `#[cold]` functions stay out of `hot_symbols`.
        assert_eq!(hot.as_slice(), &[sym_nested_leaf, sym_shallow_seed, sym_shallow_dep]);

        // Deterministic across reversed input order.
        let mut rev_inputs = inputs;
        rev_inputs.reverse();
        let hot_rev = rank_hot_symbols(&rev_inputs, &[], &pgo, alloc, alloc);
        assert_eq!(hot.as_slice(), hot_rev.as_slice());
        assert_eq!(
            compute_profile_digest(&pgo, hot.as_slice(), alloc),
            compute_profile_digest(&pgo, hot_rev.as_slice(), alloc),
        );

        // Persisted BPG1 merge promotes an observed-hot straight-line body and refuses corruption.
        let persisted = PgoSiteTable::new_in(
            &[],
            &[blaze_lir::pgo::BodyWeight { body: BodyId(sym_init.0), weight: 10_000 }],
            1,
            alloc,
        );
        let mut encoded = persisted.encode(alloc);
        let merged = build_pgo_table(mod_bodies.as_slice(), Some(encoded.as_slice()), alloc)
            .expect("valid BPG1 merges");
        let hot_merged = rank_hot_symbols(&inputs, &[], &merged, alloc, alloc);
        assert_eq!(hot_merged.as_slice(), &[sym_init, sym_nested_leaf, sym_shallow_seed, sym_shallow_dep]);

        encoded[10] ^= 0x01;
        assert!(matches!(
            build_pgo_table(mod_bodies.as_slice(), Some(encoded.as_slice()), alloc),
            Err(ExportLtoError::PgoRecord(PgoRecordError::BadDigest))
        ));
    }

    fn build_direct_caller_fn(
        is_cold: bool,
        panic_only: bool,
        alloc: Alloc<'static>,
    ) -> blaze_lir::Func<Alloc<'static>> {
        let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        if is_cold {
            b.set_attrs(FnAttrs(FnAttrs::COLD));
        }
        let callee_ref = b.reference(RefKind::Func, 0);
        let entry = b.create_block(BlockKind::Normal);
        let panic_blk = panic_only.then(|| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let x = b.param(Ty::I64);
        let res = b.call(Flags::NONE, callee_ref, sig, &[x]);
        if let Some(trap_blk) = panic_blk {
            b.jump((trap_blk, &[]));
            b.begin(trap_blk);
            b.trap(0);
        } else {
            b.ret(&[res]);
        }
        b.finish_checked().expect("valid direct-caller LIR")
    }

    fn build_loop_caller_fn(alloc: Alloc<'static>) -> blaze_lir::Func<Alloc<'static>> {
        let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let callee_ref = b.reference(RefKind::Func, 0);

        let entry = b.create_block(BlockKind::Normal);
        let header = b.create_block(BlockKind::Normal);
        let body = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let cond = b.icmp(blaze_lir::IntCond::Ult, i, n);
        b.br(Flags::NONE, cond, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let one = b.iconst(Ty::I64, 1);
        let step = b.call(Flags::NONE, callee_ref, sig, &[i]);
        let next_i = b.binary(Op::Add, Flags::NONE, i, one);
        let next_acc = b.binary(Op::Add, Flags::NONE, acc, step);
        b.jump((header, &[next_i, next_acc]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        b.finish_checked().expect("valid loop-caller LIR")
    }

    #[test]
    fn hot_caller_one_hop_bonus_ranks_direct_caller_immediately_after_hot_leaves() {
        let alloc = blaze_own::testing::heap().alloc();

        let sym_nested_leaf = Symbol([0xF1; 32]);
        let sym_shallow_leaf = Symbol([0xF2; 32]);
        let sym_direct_caller_nested = Symbol([0x10; 32]);
        let sym_direct_caller_shallow = Symbol([0x11; 32]);
        let sym_unrelated_loop_caller = Symbol([0x05; 32]);
        let sym_two_hop_caller = Symbol([0x06; 32]);
        let sym_cold_caller = Symbol([0x07; 32]);
        let sym_panic_caller = Symbol([0x08; 32]);
        let sym_unrelated_straight = Symbol([0x09; 32]);

        let fn_nested_leaf = build_nested_loop_leaf(alloc);
        let fn_shallow_leaf = build_single_loop_leaf(alloc);
        let fn_direct_caller_nested = build_direct_caller_fn(false, false, alloc);
        let fn_direct_caller_shallow = build_direct_caller_fn(false, false, alloc);
        let fn_unrelated_loop_caller = build_loop_caller_fn(alloc);
        let fn_two_hop_caller = build_direct_caller_fn(false, false, alloc);
        let fn_cold_caller = build_direct_caller_fn(true, false, alloc);
        let fn_panic_caller = build_direct_caller_fn(false, true, alloc);
        let fn_unrelated_straight = build_straight_line_fn(false, alloc);

        let mut mod_bodies: AVec<'static, ModuleBody<Alloc<'static>>> = Vec::new_in(alloc);
        mod_bodies.push(ModuleBody { id: BodyId(sym_nested_leaf.0), func: fn_nested_leaf.view().to_func_in(alloc) });
        mod_bodies.push(ModuleBody { id: BodyId(sym_shallow_leaf.0), func: fn_shallow_leaf.view().to_func_in(alloc) });
        mod_bodies.push(ModuleBody {
            id: BodyId(sym_direct_caller_nested.0),
            func: fn_direct_caller_nested.view().to_func_in(alloc),
        });
        mod_bodies.push(ModuleBody {
            id: BodyId(sym_direct_caller_shallow.0),
            func: fn_direct_caller_shallow.view().to_func_in(alloc),
        });
        mod_bodies.push(ModuleBody {
            id: BodyId(sym_unrelated_loop_caller.0),
            func: fn_unrelated_loop_caller.view().to_func_in(alloc),
        });
        mod_bodies.push(ModuleBody {
            id: BodyId(sym_two_hop_caller.0),
            func: fn_two_hop_caller.view().to_func_in(alloc),
        });
        mod_bodies.push(ModuleBody { id: BodyId(sym_cold_caller.0), func: fn_cold_caller.view().to_func_in(alloc) });
        mod_bodies.push(ModuleBody { id: BodyId(sym_panic_caller.0), func: fn_panic_caller.view().to_func_in(alloc) });
        mod_bodies.push(ModuleBody {
            id: BodyId(sym_unrelated_straight.0),
            func: fn_unrelated_straight.view().to_func_in(alloc),
        });

        let pgo = build_pgo_table(mod_bodies.as_slice(), None, alloc).expect("static PGO table");
        let inputs = [
            (sym_unrelated_straight, true, fn_unrelated_straight.view()),
            (sym_panic_caller, true, fn_panic_caller.view()),
            (sym_cold_caller, true, fn_cold_caller.view()),
            (sym_two_hop_caller, true, fn_two_hop_caller.view()),
            (sym_unrelated_loop_caller, true, fn_unrelated_loop_caller.view()),
            (sym_direct_caller_shallow, true, fn_direct_caller_shallow.view()),
            (sym_direct_caller_nested, true, fn_direct_caller_nested.view()),
            (sym_shallow_leaf, true, fn_shallow_leaf.view()),
            (sym_nested_leaf, true, fn_nested_leaf.view()),
        ];
        // Include duplicate edges for `sym_direct_caller_shallow` to verify deduplication.
        let edges = [
            (sym_direct_caller_nested, sym_nested_leaf),
            (sym_direct_caller_shallow, sym_shallow_leaf),
            (sym_direct_caller_shallow, sym_shallow_leaf),
            (sym_direct_caller_shallow, sym_shallow_leaf),
            (sym_direct_caller_shallow, sym_shallow_leaf),
            (sym_direct_caller_shallow, sym_shallow_leaf),
            (sym_two_hop_caller, sym_direct_caller_nested),
            (sym_cold_caller, sym_nested_leaf),
            (sym_panic_caller, sym_nested_leaf),
            (sym_unrelated_loop_caller, sym_unrelated_straight),
        ];

        let hot = rank_hot_symbols(&inputs, &edges, &pgo, alloc, alloc);
        // 0-call hot leaves rank first, followed immediately by direct 1-hop callers (ordered by
        // callee heat, deduplicated), then unrelated non-leaf loops. 2-hop callers, `#[cold]`
        // callers, panic-only callers, and unrelated straight-line bodies stay excluded.
        assert_eq!(
            hot.as_slice(),
            &[
                sym_nested_leaf,
                sym_shallow_leaf,
                sym_direct_caller_nested,
                sym_direct_caller_shallow,
                sym_unrelated_loop_caller,
            ]
        );

        // Deterministic across reversed body and call-edge orderings (Rule 25).
        let mut rev_inputs = inputs;
        rev_inputs.reverse();
        let mut rev_edges = edges;
        rev_edges.reverse();
        let hot_rev = rank_hot_symbols(&rev_inputs, &rev_edges, &pgo, alloc, alloc);
        assert_eq!(hot.as_slice(), hot_rev.as_slice());
        assert_eq!(
            compute_profile_digest(&pgo, hot.as_slice(), alloc),
            compute_profile_digest(&pgo, hot_rev.as_slice(), alloc),
        );
    }

    #[test]
    fn depth1_small_loop_leaf_qualifies_as_hot_leaf_ahead_of_hot_callers_while_straight_line_does_not() {
        let alloc = blaze_own::testing::heap().alloc();

        let sym_caller_nested = Symbol([0x01; 32]);
        let sym_caller_depth1 = Symbol([0x02; 32]);
        let sym_caller_warm_straight = Symbol([0x03; 32]);
        let sym_caller_straight = Symbol([0x04; 32]);
        let sym_straight_leaf = Symbol([0x05; 32]);
        let sym_warm_straight_leaf = Symbol([0x06; 32]);
        let sym_nested_leaf = Symbol([0xF1; 32]);
        let sym_depth1_leaf = Symbol([0xF2; 32]);

        let fn_nested_leaf = build_nested_loop_leaf(alloc);
        let fn_depth1_leaf = build_single_loop_leaf(alloc);
        let fn_straight_leaf = build_straight_line_fn(false, alloc);
        let fn_warm_straight_leaf = build_straight_line_fn(false, alloc);
        let fn_caller_nested = build_direct_caller_fn(false, false, alloc);
        let fn_caller_depth1 = build_direct_caller_fn(false, false, alloc);
        let fn_caller_warm_straight = build_direct_caller_fn(false, false, alloc);
        let fn_caller_straight = build_direct_caller_fn(false, false, alloc);

        assert_eq!(
            compute_loop_heat(fn_depth1_leaf.view(), alloc),
            LoopHeat { max_depth: 1, loop_blocks: 2 }
        );
        assert_eq!(
            compute_loop_heat(fn_straight_leaf.view(), alloc),
            LoopHeat { max_depth: 0, loop_blocks: 0 }
        );

        // Simulate post-LTO state where `sym_depth1_leaf` had only baseline straight-line static
        // PGO weight (`10`) pre-LTO before its loop was inlined (`own_score = 20 + 10 = 30`),
        // `sym_warm_straight_leaf` is 0-loop with weight `15` (above baseline `10`, below
        // `HOT_LEAF_THRESHOLD = 20`), and `sym_straight_leaf` sits at baseline `10`.
        let pgo = PgoSiteTable::new_in(
            &[],
            &[
                blaze_lir::pgo::BodyWeight { body: BodyId(sym_nested_leaf.0), weight: 100 },
                blaze_lir::pgo::BodyWeight {
                    body: BodyId(sym_depth1_leaf.0),
                    weight: STATIC_STRAIGHT_LINE_WEIGHT,
                },
                blaze_lir::pgo::BodyWeight {
                    body: BodyId(sym_caller_nested.0),
                    weight: STATIC_STRAIGHT_LINE_WEIGHT,
                },
                blaze_lir::pgo::BodyWeight {
                    body: BodyId(sym_caller_depth1.0),
                    weight: STATIC_STRAIGHT_LINE_WEIGHT,
                },
                blaze_lir::pgo::BodyWeight { body: BodyId(sym_warm_straight_leaf.0), weight: 15 },
                blaze_lir::pgo::BodyWeight {
                    body: BodyId(sym_caller_warm_straight.0),
                    weight: STATIC_STRAIGHT_LINE_WEIGHT,
                },
                blaze_lir::pgo::BodyWeight {
                    body: BodyId(sym_straight_leaf.0),
                    weight: STATIC_STRAIGHT_LINE_WEIGHT,
                },
                blaze_lir::pgo::BodyWeight {
                    body: BodyId(sym_caller_straight.0),
                    weight: STATIC_STRAIGHT_LINE_WEIGHT,
                },
            ],
            1,
            alloc,
        );

        let inputs = [
            (sym_caller_nested, true, fn_caller_nested.view()),
            (sym_caller_depth1, true, fn_caller_depth1.view()),
            (sym_caller_warm_straight, true, fn_caller_warm_straight.view()),
            (sym_caller_straight, true, fn_caller_straight.view()),
            (sym_straight_leaf, true, fn_straight_leaf.view()),
            (sym_warm_straight_leaf, true, fn_warm_straight_leaf.view()),
            (sym_depth1_leaf, false, fn_depth1_leaf.view()),
            (sym_nested_leaf, false, fn_nested_leaf.view()),
        ];
        let edges = [
            (sym_caller_nested, sym_nested_leaf),
            (sym_caller_depth1, sym_depth1_leaf),
            (sym_caller_warm_straight, sym_warm_straight_leaf),
            (sym_caller_straight, sym_straight_leaf),
        ];

        let hot = rank_hot_symbols(&inputs, &edges, &pgo, alloc, alloc);
        // `sym_depth1_leaf` (`own_score = 30 >= HOT_LEAF_THRESHOLD`) qualifies as `HotLeaf` and
        // ranks strictly ahead of `sym_caller_nested` (`HotLeafCaller`, `score = 135`) despite
        // lower numeric score, non-seed status, and higher symbol bytes. `sym_warm_straight_leaf`
        // (`own_score = 15 < HOT_LEAF_THRESHOLD`) stays in `Other` without propagating caller
        // heat, and baseline straight-line `sym_straight_leaf` (`own_score = 10`) stays excluded.
        assert_eq!(
            hot.as_slice(),
            &[
                sym_nested_leaf,
                sym_depth1_leaf,
                sym_caller_nested,
                sym_caller_depth1,
                sym_warm_straight_leaf,
            ]
        );
    }

    #[test]
    fn emit_code_record_with_abi_narrows_leaf_clobbers_and_supports_hot_and_cold_tiers() {
        let alloc = blaze_own::testing::heap().alloc();
        let leaf = build_straight_line_fn(false, alloc);
        let cold_leaf = build_straight_line_fn(true, alloc);
        let verified_leaf = verify(leaf.view()).expect("leaf LIR must verify");
        let verified_cold = verify(cold_leaf.view()).expect("cold leaf LIR must verify");
        let names = InstanceNames { funcs: &[], statics: &[], c_statics: &[], vtables: &[], tls: &[] };
        let machine = arm64_machine();

        // 1. 2-tier hot (`is_hot: true`) vs cold-fast (`is_hot: false`) both emit valid sealed
        //    `CodeRecord`s and produce strictly narrower `CallClobbers` than conservative C ABI.
        let (cold_bytes, cold_decl, cold_clobbers) = emit_code_record_for_machine_with_abi(
            verified_leaf,
            &names,
            crate::front2::builtins_shim::Routines::NONE,
            &[],
            SotaRecordAbiOpts {
                machine,
                is_hot: false,
                scratch_order: Some(ScratchOrder::DEFAULT),
                preserve_all_cold: false,
                callee_clobbers: &[],
                internal_fast: false,
                internal_fast_callees: &[],
            },
            alloc,
        )
        .expect("cold-fast leaf emission must succeed");
        let (hot_bytes, hot_decl, hot_clobbers) = emit_code_record_for_machine_with_abi(
            verified_leaf,
            &names,
            crate::front2::builtins_shim::Routines::NONE,
            &[],
            SotaRecordAbiOpts {
                machine,
                is_hot: true,
                scratch_order: None,
                preserve_all_cold: false,
                callee_clobbers: &[],
                internal_fast: false,
                internal_fast_callees: &[],
            },
            alloc,
        )
        .expect("hot leaf emission must succeed");

        assert!(cold_decl.is_empty());
        assert!(hot_decl.is_empty());
        let cold_view = blaze_code::record::CodeView::read(cold_bytes.as_slice())
            .expect("cold-fast record must decode as valid sealed CodeRecord");
        let hot_view = blaze_code::record::CodeView::read(hot_bytes.as_slice())
            .expect("hot record must decode as valid sealed CodeRecord");
        assert!(!cold_view.code().is_empty());
        assert!(!hot_view.code().is_empty());

        let saved_cold_regs = CallClobbers::C_ABI_ARM64.clobbered.0 & !cold_clobbers.clobbered.0;
        let saved_hot_regs = CallClobbers::C_ABI_ARM64.clobbered.0 & !hot_clobbers.clobbered.0;
        assert_ne!(saved_cold_regs, 0, "leaf helper must have strictly narrower clobbers than C ABI (cold tier)");
        assert_ne!(saved_hot_regs, 0, "leaf helper must have strictly narrower clobbers than C ABI (hot tier)");

        // 2. Guarded `ColdPreserveAll` succeeds on a safe cold leaf and is rejected on unwinding LIR.
        assert!(is_safe_cold_preserve_all(verified_cold.view()));
        let (preserve_bytes, _preserve_decl, preserve_clobbers) = emit_code_record_for_machine_with_abi(
            verified_cold,
            &names,
            crate::front2::builtins_shim::Routines::NONE,
            &[],
            SotaRecordAbiOpts {
                machine,
                is_hot: false,
                scratch_order: Some(ScratchOrder::DEFAULT),
                preserve_all_cold: true,
                callee_clobbers: &[],
                internal_fast: false,
                internal_fast_callees: &[],
            },
            alloc,
        )
        .expect("ColdPreserveAll cold leaf emission must succeed");
        let preserve_view = blaze_code::record::CodeView::read(preserve_bytes.as_slice())
            .expect("ColdPreserveAll record must decode as valid sealed CodeRecord");
        assert!(!preserve_view.code().is_empty());
        assert_ne!(
            CallClobbers::C_ABI_ARM64.clobbered.0 & !preserve_clobbers.clobbered.0,
            0,
            "ColdPreserveAll leaf must expose narrowed clobbers"
        );

        let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        b.set_attrs(FnAttrs(FnAttrs::COLD));
        let callee_ref = b.reference(RefKind::Func, 0);
        let entry = b.create_block(BlockKind::Normal);
        let normal = b.create_block(BlockKind::Normal);
        let cleanup = b.create_block(BlockKind::Cleanup);
        b.begin(entry);
        let arg = b.param(Ty::I64);
        let res = b.invoke(Flags::NONE, callee_ref, sig, &[arg], normal, cleanup);
        b.begin(normal);
        b.ret(&[res]);
        b.begin(cleanup);
        let exc = b.param(Ty::PTR);
        b.resume(exc);
        let unwinding_fn = b.finish_checked().expect("unwinding LIR must verify");
        assert!(
            !is_safe_cold_preserve_all(unwinding_fn.view()),
            "functions with Op::Invoke / Op::Resume must keep preserve_all_cold disabled"
        );
    }

    #[test]
    fn non_seed_filter_keeps_non_cold_library_callers_and_excludes_cold_and_noreturn() {
        let opts = ExportLtoOpts::for_export(Isa { lse: true, os: blaze_emit::Os::Linux });
        let no_inline = FnAttrs(0).with_inline(Inline::Never).0;
        let always_inline = FnAttrs(0).with_inline(Inline::Always).0;
        let hint_inline = FnAttrs(0).with_inline(Inline::Hint).0;

        // Non-cold library callers (even `#[inline(never)]` sorting/hashing kernels) up to
        // `max_caller_insts` enter the module so their own small callees inline into them.
        assert!(should_keep_non_seed(0, 900, false, 24, &opts));
        assert!(should_keep_non_seed(no_inline, 1500, false, 24, &opts));
        assert!(!should_keep_non_seed(0, opts.max_caller_insts + 1, false, 24, &opts));

        // Cold and noreturn bodies stay excluded (except explicitly `#[inline(always)]` cold).
        assert!(!should_keep_non_seed(FnAttrs::COLD, 16, false, 24, &opts));
        assert!(!should_keep_non_seed(FnAttrs::COLD | no_inline, 16, false, 24, &opts));
        assert!(!should_keep_non_seed(FnAttrs::NORETURN, 16, false, 24, &opts));
        assert!(!should_keep_non_seed(FnAttrs::NORETURN | always_inline, 16, false, 24, &opts));
        assert!(should_keep_non_seed(FnAttrs::COLD | always_inline, 16, false, 24, &opts));

        // Tiny-reserve tail admits only `<= 24`-inst `#[inline]` / `#[inline(always)]` leaves.
        assert!(should_keep_non_seed(hint_inline, 24, true, 24, &opts));
        assert!(should_keep_non_seed(always_inline, 12, true, 24, &opts));
        assert!(!should_keep_non_seed(0, 12, true, 24, &opts));
        assert!(!should_keep_non_seed(hint_inline, 25, true, 24, &opts));

        // Oversized non-cold callers are still expanded so their downstream call subgraph is never
        // severed, while cold/noreturn bodies stay unexpanded.
        assert!(should_expand_non_seed(0));
        assert!(should_expand_non_seed(no_inline));
        assert!(should_expand_non_seed(FnAttrs::COLD | always_inline));
        assert!(!should_expand_non_seed(FnAttrs::COLD));
        assert!(!should_expand_non_seed(FnAttrs::NORETURN));
        assert!(!should_expand_non_seed(FnAttrs::NORETURN | always_inline));
    }

    #[test]
    fn enqueue_normal_callees_discovers_call_invoke_func_addr_and_vtable_methods() {
        use blaze_code::datum::{Datum, Place as DatumPlace};
        use blaze_code::record::{Target as CodeTarget, Word as CodeWord};

        let alloc = blaze_own::testing::heap().alloc();

        // Register 4 reachable functions in a Walk: direct_call, direct_invoke, fn_ptr, vtable_method.
        let mut walk = Walk::new_in(alloc);
        let row_call = walk.root::<RunError>((ItemKey(1), b"direct_call"), 64).expect("row_call");
        let row_invoke = walk.root::<RunError>((ItemKey(2), b"direct_invoke"), 64).expect("row_invoke");
        let row_fnptr = walk.root::<RunError>((ItemKey(3), b"fn_ptr"), 64).expect("row_fnptr");
        let row_vmethod = walk.root::<RunError>((ItemKey(4), b"vtable_method"), 64).expect("row_vmethod");
        let sym_call = walk.symbol(row_call);
        let sym_invoke = walk.symbol(row_invoke);
        let sym_fnptr = walk.symbol(row_fnptr);
        let sym_vmethod = walk.symbol(row_vmethod);
        let vsym = Symbol::of_name("test_vtable");

        // Build a vtable datum referencing `sym_vmethod`.
        let mut vt_datum = Datum::new_in(DatumPlace::ReadOnly, 3, alloc);
        vt_datum.bytes.extend_from_slice(&[0u8; 8]);
        let t_idx = vt_datum.target(CodeTarget { space: Space::Func, symbol: sym_vmethod });
        vt_datum.words.push(CodeWord { at: 0, target: t_idx, addend: 0 });
        let mut vt_bytes: AVec<'static, u8> = Vec::new_in(alloc);
        vt_datum.seal().expect("seal").encode(&mut vt_bytes);

        let func = {
            let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
            b.set_signature(sig);
            let r_call = b.reference(RefKind::Func, 0);
            let r_invoke = b.reference(RefKind::Func, 1);
            let r_fnptr = b.reference(RefKind::Func, 2);
            let r_vtable = b.reference(RefKind::Vtable, 0);
            let entry = b.create_block(BlockKind::Normal);
            let norm = b.create_block(BlockKind::Normal);
            let pad = b.create_block(BlockKind::Cleanup);
            b.begin(entry);
            b.call(Flags::NONE, r_call, sig, &[]);
            let _fp = b.func_addr(r_fnptr);
            let _vt = b.data_addr(r_vtable);
            b.invoke(Flags::NONE, r_invoke, sig, &[], norm, pad);
            b.begin(norm);
            b.ret(&[]);
            b.begin(pad);
            let exc = b.param(Ty::PTR);
            b.resume(exc);
            b.finish_checked().expect("valid caller")
        };

        let mut funcs_tbl: AVec<'static, BuiltCallee> = Vec::new_in(alloc);
        funcs_tbl.push(BuiltCallee::Symbol(sym_call));
        funcs_tbl.push(BuiltCallee::Symbol(sym_invoke));
        funcs_tbl.push(BuiltCallee::Symbol(sym_fnptr));
        let mut vtables_tbl: AVec<'static, Symbol> = Vec::new_in(alloc);
        vtables_tbl.push(vsym);
        let mut vtable_data: AVec<'static, (Symbol, AVec<'static, u8>)> = Vec::new_in(alloc);
        vtable_data.push((vsym, vt_bytes));

        let built = BuiltLir {
            func,
            funcs: funcs_tbl,
            statics: Vec::new_in(alloc),
            c_statics: Vec::new_in(alloc),
            vtables: vtables_tbl,
            vtable_data,
            tls: Vec::new_in(alloc),
            traps: Vec::new_in(alloc),
            c_text: Vec::new_in(alloc),
        };

        let mut visited: AVec<'static, u8> = Vec::with_capacity_in(walk.len(), alloc);
        visited.resize(walk.len(), UNSEEN);
        let mut min_depth: AVec<'static, u8> = Vec::with_capacity_in(walk.len(), alloc);
        min_depth.resize(walk.len(), u8::MAX);
        let mut call_queue: AVec<'static, (usize, u8, bool)> = Vec::new_in(alloc);
        let mut indirect_queue: AVec<'static, (usize, u8, bool)> = Vec::new_in(alloc);
        enqueue_normal_callees(
            &built,
            &walk,
            1,
            24,
            &|_| true,
            visited.as_mut_slice(),
            min_depth.as_mut_slice(),
            &mut call_queue,
            &mut indirect_queue,
            alloc,
        );

        assert!(call_queue.iter().any(|&(r, d, _)| r == row_call && d == 1));
        assert!(call_queue.iter().any(|&(r, d, _)| r == row_invoke && d == 1));
        assert!(indirect_queue.iter().any(|&(r, d, _)| r == row_fnptr && d == 1));
        assert!(indirect_queue.iter().any(|&(r, d, _)| r == row_vmethod && d == 1));
    }

    #[test]
    fn enqueue_sym_preserves_shallowest_depth_on_promotion_and_reexpands_unexpanded() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut walk = Walk::new_in(alloc);
        let row_promoted = walk.root::<RunError>((ItemKey(11), b"promoted_target"), 64).expect("row_promoted");
        let row_unexpanded = walk.root::<RunError>((ItemKey(12), b"unexpanded_target"), 64).expect("row_unexpanded");
        let sym_promoted = walk.symbol(row_promoted);
        let sym_unexpanded = walk.symbol(row_unexpanded);

        let max_depth = 4u8;
        let mut visited: AVec<'static, u8> = Vec::with_capacity_in(walk.len(), alloc);
        visited.resize(walk.len(), UNSEEN);
        let mut min_depth: AVec<'static, u8> = Vec::with_capacity_in(walk.len(), alloc);
        min_depth.resize(walk.len(), u8::MAX);
        let mut call_queue: AVec<'static, (usize, u8, bool)> = Vec::new_in(alloc);
        let mut indirect_queue: AVec<'static, (usize, u8, bool)> = Vec::new_in(alloc);

        // 1. Shallow indirect edge (depth 1) followed by deep direct-call promotion (depth == max_depth):
        //    promotion into `call_queue` MUST retain the shallowest depth (`1`), never overwrite with `4`.
        enqueue_sym(
            &walk,
            sym_promoted,
            false,
            1,
            max_depth,
            &|_| true,
            visited.as_mut_slice(),
            min_depth.as_mut_slice(),
            &mut call_queue,
            &mut indirect_queue,
        );
        enqueue_sym(
            &walk,
            sym_promoted,
            true,
            max_depth,
            max_depth,
            &|_| true,
            visited.as_mut_slice(),
            min_depth.as_mut_slice(),
            &mut call_queue,
            &mut indirect_queue,
        );
        assert_eq!(visited[row_promoted], QUEUED_CALL);
        assert_eq!(min_depth[row_promoted], 1);
        assert!(
            call_queue.iter().any(|&(r, d, _)| r == row_promoted && d == 1),
            "promoted direct-queue entry must preserve shallowest depth 1, got {call_queue:?}"
        );

        // 2. A row previously processed at `max_depth` (`PROCESSED_UNEXPANDED`) that is later
        //    reached at depth `2 < max_depth` MUST be re-queued as `QUEUED_EXPAND_ONLY`.
        visited[row_unexpanded] = PROCESSED_UNEXPANDED;
        min_depth[row_unexpanded] = max_depth;
        enqueue_sym(
            &walk,
            sym_unexpanded,
            false,
            2,
            max_depth,
            &|_| true,
            visited.as_mut_slice(),
            min_depth.as_mut_slice(),
            &mut call_queue,
            &mut indirect_queue,
        );
        assert_eq!(visited[row_unexpanded], QUEUED_EXPAND_ONLY);
        assert_eq!(min_depth[row_unexpanded], 2);
        assert!(
            indirect_queue.iter().any(|&(r, d, _)| r == row_unexpanded && d == 2),
            "unexpanded row reached at shallower depth must re-queue for expansion, got {indirect_queue:?}"
        );
    }

    #[test]
    fn enqueue_normal_callees_follows_nested_data_relocs_and_breaks_cycles() {
        use blaze_code::datum::{Datum, Place as DatumPlace};
        use blaze_code::record::{Target as CodeTarget, Word as CodeWord};
        use blaze_lir::builder::DataReloc;

        let alloc = blaze_own::testing::heap().alloc();
        let mut walk = Walk::new_in(alloc);
        let row_nested_fn = walk.root::<RunError>((ItemKey(21), b"nested_data_fn"), 64).expect("row_nested_fn");
        let row_nested_vt = walk.root::<RunError>((ItemKey(22), b"nested_data_vt_method"), 64).expect("row_nested_vt");
        let sym_nested_fn = walk.symbol(row_nested_fn);
        let sym_nested_vt = walk.symbol(row_nested_vt);
        let vsym = Symbol::of_name("nested_test_vtable");

        let mut vt_datum = Datum::new_in(DatumPlace::ReadOnly, 3, alloc);
        vt_datum.bytes.extend_from_slice(&[0u8; 8]);
        let t_idx = vt_datum.target(CodeTarget { space: Space::Func, symbol: sym_nested_vt });
        vt_datum.words.push(CodeWord { at: 0, target: t_idx, addend: 0 });
        let mut vt_bytes: AVec<'static, u8> = Vec::new_in(alloc);
        vt_datum.seal().expect("seal").encode(&mut vt_bytes);

        // Build `outer_data -> mid_data -> (nested_fn, vtable, self-reference cycle)`.
        let func = {
            let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
            b.set_signature(sig);
            let r_fn = b.reference(RefKind::Func, 0);
            let r_vt = b.reference(RefKind::Vtable, 0);
            let inner_data = b.data(
                &[0u8; 16],
                3,
                &[
                    DataReloc { offset: 0, target: r_fn, addend: 0 },
                    DataReloc { offset: 8, target: r_vt, addend: 0 },
                ],
            );
            let mid_data = b.data(
                &[0u8; 8],
                3,
                &[DataReloc { offset: 0, target: inner_data, addend: 0 }],
            );
            // Include a duplicate/back-edge reference alongside `mid_data` to exercise cycle/dedup.
            let outer_data = b.data(
                &[0u8; 16],
                3,
                &[
                    DataReloc { offset: 0, target: mid_data, addend: 0 },
                    DataReloc { offset: 8, target: inner_data, addend: 0 },
                ],
            );
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let _p = b.data_addr(outer_data);
            b.ret(&[]);
            b.finish_checked().expect("valid nested data caller")
        };

        let mut funcs_tbl: AVec<'static, BuiltCallee> = Vec::new_in(alloc);
        funcs_tbl.push(BuiltCallee::Symbol(sym_nested_fn));
        let mut vtables_tbl: AVec<'static, Symbol> = Vec::new_in(alloc);
        vtables_tbl.push(vsym);
        let mut vtable_data: AVec<'static, (Symbol, AVec<'static, u8>)> = Vec::new_in(alloc);
        vtable_data.push((vsym, vt_bytes));

        let built = BuiltLir {
            func,
            funcs: funcs_tbl,
            statics: Vec::new_in(alloc),
            c_statics: Vec::new_in(alloc),
            vtables: vtables_tbl,
            vtable_data,
            tls: Vec::new_in(alloc),
            traps: Vec::new_in(alloc),
            c_text: Vec::new_in(alloc),
        };

        let mut visited: AVec<'static, u8> = Vec::with_capacity_in(walk.len(), alloc);
        visited.resize(walk.len(), UNSEEN);
        let mut min_depth: AVec<'static, u8> = Vec::with_capacity_in(walk.len(), alloc);
        min_depth.resize(walk.len(), u8::MAX);
        let mut call_queue: AVec<'static, (usize, u8, bool)> = Vec::new_in(alloc);
        let mut indirect_queue: AVec<'static, (usize, u8, bool)> = Vec::new_in(alloc);
        enqueue_normal_callees(
            &built,
            &walk,
            2,
            24,
            &|_| true,
            visited.as_mut_slice(),
            min_depth.as_mut_slice(),
            &mut call_queue,
            &mut indirect_queue,
            alloc,
        );

        assert!(
            indirect_queue.iter().any(|&(r, d, _)| r == row_nested_fn && d == 2),
            "expected nested Data -> Data -> Func target in indirect_queue, got {indirect_queue:?}"
        );
        assert!(
            indirect_queue.iter().any(|&(r, d, _)| r == row_nested_vt && d == 2),
            "expected nested Data -> Data -> Vtable method in indirect_queue, got {indirect_queue:?}"
        );
    }

    #[test]
    fn built_lir_and_emitted_body_cache_codec_round_trip_and_corrupt_rejection() {
        let shared = blaze_own::testing::heap().shared();
        let alloc = blaze_own::testing::heap().alloc();
        let func = build_single_loop_leaf(alloc).view().to_func_in(shared);

        let mut c_text: Vec<u8, SharedAlloc<'static>> = Vec::new_in(shared);
        c_text.extend_from_slice(b"malloc_fnextern_stat");
        let mut funcs: Vec<BuiltCallee, SharedAlloc<'static>> = Vec::new_in(shared);
        funcs.push(BuiltCallee::Symbol(Symbol::of_name("callee_a")));
        funcs.push(BuiltCallee::C { start: 0, len: 9 });
        let mut statics: Vec<ItemKey, SharedAlloc<'static>> = Vec::new_in(shared);
        statics.push(ItemKey(0x1122_3344_5566_7788));
        let mut c_statics: Vec<BuiltForeignStatic, SharedAlloc<'static>> = Vec::new_in(shared);
        c_statics.push(BuiltForeignStatic { item: ItemKey(0x1122_3344_5566_7788), start: 9, len: 11, weak: true });
        let mut vtables: Vec<Symbol, SharedAlloc<'static>> = Vec::new_in(shared);
        vtables.push(Symbol::of_name("vt_0"));
        let mut vtable_data: Vec<(Symbol, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>> = Vec::new_in(shared);
        let mut vt_payload: Vec<u8, SharedAlloc<'static>> = Vec::new_in(shared);
        vt_payload.extend_from_slice(&[1, 2, 3, 4, 5]);
        vtable_data.push((Symbol::of_name("vt_0"), vt_payload));
        let mut tls: Vec<ItemKey, SharedAlloc<'static>> = Vec::new_in(shared);
        tls.push(ItemKey(0x99aa_bbcc));
        let mut traps: Vec<TrapSource, SharedAlloc<'static>> = Vec::new_in(shared);
        traps.push(TrapSource {
            ordinal: 0,
            kind: TrapKind::Discriminant,
            site: Some((0xDEAD_BEEF, 42)),
            subject: Some(Subject { item: 0xCAFE_BABE, count: 3, discriminants: [0, 1, 2, 0, 0, 0, 0, 0] }),
        });
        traps.push(TrapSource { ordinal: 1, kind: TrapKind::BodyEnd, site: None, subject: None });

        let built = BuiltLir { func, funcs, statics, c_statics, vtables, vtable_data, tls, traps, c_text };
        let mut decl_name: Vec<u8, SharedAlloc<'static>> = Vec::new_in(shared);
        decl_name.extend_from_slice(&0x5566_7788u128.to_le_bytes());
        decl_name.extend_from_slice(&0u32.to_le_bytes());
        let mut declared: Vec<DeclaredRow, SharedAlloc<'static>> = Vec::new_in(shared);
        declared.push((ItemKey(0x5566_7788), decl_name));
        let encoded = encode_built_lir(&built, declared.as_slice(), shared).expect("encode_built_lir succeeds");
        let (decoded, dec_declared) = decode_built_lir(encoded.as_slice(), shared).expect("decode_built_lir succeeds");
        assert_eq!(
            blaze_lir::persist::to_bytes_in(&built.func.view(), shared).as_slice(),
            blaze_lir::persist::to_bytes_in(&decoded.func.view(), shared).as_slice(),
        );
        assert_eq!(built.funcs.as_slice(), decoded.funcs.as_slice());
        assert_eq!(built.statics.as_slice(), decoded.statics.as_slice());
        assert_eq!(built.c_statics.as_slice(), decoded.c_statics.as_slice());
        assert_eq!(built.vtables.as_slice(), decoded.vtables.as_slice());
        assert_eq!(built.vtable_data.len(), decoded.vtable_data.len());
        assert_eq!(built.vtable_data[0].0, decoded.vtable_data[0].0);
        assert_eq!(built.vtable_data[0].1.as_slice(), decoded.vtable_data[0].1.as_slice());
        assert_eq!(built.tls.as_slice(), decoded.tls.as_slice());
        assert_eq!(built.traps.as_slice(), decoded.traps.as_slice());
        assert_eq!(built.c_text.as_slice(), decoded.c_text.as_slice());
        assert_eq!(dec_declared.len(), 1);
        assert_eq!(dec_declared[0].0, declared[0].0);
        assert_eq!(dec_declared[0].1.as_slice(), declared[0].1.as_slice());

        // Rule 3: truncated or corrupted payloads must decode to `None`.
        assert!(decode_built_lir(&encoded[..encoded.len() - 1], shared).is_none());
        let mut bad_magic = encoded.as_slice().to_vec();
        bad_magic[0] ^= 0xFF;
        assert!(decode_built_lir(&bad_magic, shared).is_none());
    }

    #[test]
    fn collect_missing_walk_targets_recovers_cached_built_lir_and_vtable_targets_and_rejects_hole() {
        use blaze_code::datum::{Datum, Place as DatumPlace};
        use blaze_code::record::{CodeRecord, Site, SiteKind, Word as RecordWord};

        let shared = blaze_own::testing::heap().shared();
        let alloc = blaze_own::testing::heap().alloc();

        // Construct a caller root already in `walk`, plus three targets NOT in `walk`:
        // 1. A direct `Space::Func` callee (e.g. from a cached `BLZLIR02` inlined callee)
        // 2. A `Space::Func` method referenced only inside a carried vtable datum
        // 3. A `Space::Static` and `Space::Tls` referenced by the post-LTO code record.
        let caller_key = ItemKey(0xAAAA_0001);
        let mut caller_name = [0u8; 20];
        caller_name[0..16].copy_from_slice(&caller_key.0.to_le_bytes());
        let caller_sym = crate::symbols::fn_symbol_of_name(&caller_name);

        let missing_callee_key = ItemKey(0xBBBB_0002);
        let mut missing_callee_name = [0u8; 20];
        missing_callee_name[0..16].copy_from_slice(&missing_callee_key.0.to_le_bytes());
        let missing_callee_sym = crate::symbols::fn_symbol_of_name(&missing_callee_name);

        let vtable_fn_key = ItemKey(0xCCCC_0003);
        let mut vtable_fn_name = [0u8; 20];
        vtable_fn_name[0..16].copy_from_slice(&vtable_fn_key.0.to_le_bytes());
        let vtable_fn_sym = crate::symbols::fn_symbol_of_name(&vtable_fn_name);

        let missing_static_key = ItemKey(0xDDDD_0004);
        let missing_static_sym = static_symbol(missing_static_key);
        let missing_tls_key = ItemKey(0xEEEE_0005);
        let missing_tls_sym = tls_symbol(missing_tls_key);

        let mut walk = Walk::new_in(alloc);
        let caller_row = walk.root::<()>( (caller_key, &caller_name), 64 ).expect("root caller");
        assert!(walk.row_of(&missing_callee_sym).is_none());
        assert!(walk.row_of(&vtable_fn_sym).is_none());
        assert!(walk.row_of(&missing_static_sym).is_none());
        assert!(walk.row_of(&missing_tls_sym).is_none());

        // Round-trip `BuiltLir` + `declared` through `BLZLIR02` cache codec to simulate a cache hit
        // written by another binary whose `walk` happened to contain the callee.
        let func = build_single_loop_leaf(alloc).view().to_func_in(shared);
        let mut statics: Vec<ItemKey, SharedAlloc<'static>> = Vec::new_in(shared);
        statics.push(missing_static_key);
        let mut tls: Vec<ItemKey, SharedAlloc<'static>> = Vec::new_in(shared);
        tls.push(missing_tls_key);
        let built = BuiltLir {
            func,
            funcs: Vec::new_in(shared),
            statics,
            c_statics: Vec::new_in(shared),
            vtables: Vec::new_in(shared),
            vtable_data: Vec::new_in(shared),
            tls,
            traps: Vec::new_in(shared),
            c_text: Vec::new_in(shared),
        };
        let mut declared: Vec<DeclaredRow, SharedAlloc<'static>> = Vec::new_in(shared);
        let mut n1: Vec<u8, SharedAlloc<'static>> = Vec::new_in(shared);
        n1.extend_from_slice(&missing_callee_name);
        declared.push((missing_callee_key, n1));
        let mut n2: Vec<u8, SharedAlloc<'static>> = Vec::new_in(shared);
        n2.extend_from_slice(&vtable_fn_name);
        declared.push((vtable_fn_key, n2));

        let cached_bytes = encode_built_lir(&built, declared.as_slice(), shared).expect("encode BLZLIR02");
        let (decoded_built, decoded_declared) =
            decode_built_lir(cached_bytes.as_slice(), shared).expect("decode BLZLIR02");

        // Build a sealed post-LTO CodeRecord referencing `missing_callee_sym`, `missing_static_sym`,
        // `missing_tls_sym`, plus a sealed vtable datum referencing `vtable_fn_sym`.
        let mut rec = CodeRecord::new_in(alloc);
        let call_t = rec.target(CodeTarget { space: Space::Func, symbol: missing_callee_sym });
        let stat_t = rec.target(CodeTarget { space: Space::Static, symbol: missing_static_sym });
        let tls_t = rec.target(CodeTarget { space: Space::Tls, symbol: missing_tls_sym });
        rec.code.extend_from_slice(&0x9000_0010u32.to_le_bytes()); // 0:  adrp x16, #0
        rec.code.extend_from_slice(&0xF940_0210u32.to_le_bytes()); // 4:  ldr x16, [x16]
        rec.code.extend_from_slice(&0xD63F_0200u32.to_le_bytes()); // 8:  blr x16
        rec.code.extend_from_slice(&0x9000_0000u32.to_le_bytes()); // 12: adrp x0, #0
        rec.code.extend_from_slice(&0xF940_0000u32.to_le_bytes()); // 16: ldr x0, [x0]
        rec.code.extend_from_slice(&0x9000_0001u32.to_le_bytes()); // 20: adrp x1, #0
        rec.code.extend_from_slice(&0xF940_0021u32.to_le_bytes()); // 24: ldr x1, [x1]
        rec.code.extend_from_slice(&0xD65F_03C0u32.to_le_bytes()); // 28: ret
        rec.sites.push(Site { at: 0, kind: SiteKind::Call, target: call_t, addend: 0 });
        rec.sites.push(Site { at: 12, kind: SiteKind::Load, target: stat_t, addend: 0 });
        rec.sites.push(Site { at: 20, kind: SiteKind::Load, target: tls_t, addend: 0 });
        let sealed_rec = rec.seal().expect("seal post-LTO record");
        let mut rec_bytes: Vec<u8, SharedAlloc<'static>> = Vec::new_in(shared);
        let mut rec_tmp: AVec<'_, u8> = Vec::new_in(alloc);
        sealed_rec.encode(&mut rec_tmp);
        rec_bytes.extend_from_slice(rec_tmp.as_slice());

        let mut vt = Datum::new_in(DatumPlace::ReadOnly, 3, alloc);
        vt.bytes.resize(8, 0);
        let vt_fn_t = vt.target(CodeTarget { space: Space::Func, symbol: vtable_fn_sym });
        vt.words.push(RecordWord { at: 0, target: vt_fn_t, addend: 0 });
        let sealed_vt = vt.seal().expect("seal vtable datum");
        let mut vt_tmp: AVec<'_, u8> = Vec::new_in(alloc);
        sealed_vt.encode(&mut vt_tmp);
        let mut vt_bytes: Vec<u8, SharedAlloc<'static>> = Vec::new_in(shared);
        vt_bytes.extend_from_slice(vt_tmp.as_slice());
        let mut vtable_data: Vec<(Symbol, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>> = Vec::new_in(shared);
        vtable_data.push((Symbol::of_name("vt_test"), vt_bytes));

        let out_bodies = [LtoBodyOut {
            row: caller_row,
            symbol: caller_sym,
            record: rec_bytes,
            vtable_data,
            extra_declared: Vec::new_in(shared),
            inlined_bodies: 1,
        }];

        // Positive proof: with `BLZLIR02` persisted declarations, EVERY missing Func/vtable-method/
        // Static/Tls target is recovered with the originating declarer row.
        let origins_ok = [LtoOrigin {
            row: caller_row,
            symbol: caller_sym,
            is_seed: true,
            orig_unreachables: 0,
            built: decoded_built,
            declared: decoded_declared,
        }];
        let missing = collect_missing_walk_targets(&walk, &origins_ok, &out_bodies, shared, alloc)
            .expect("all missing targets must be recovered from persisted BLZLIR02 declarations");
        assert_eq!(missing.instances.len(), 2);
        assert_eq!(missing.instances[0].0, caller_row);
        assert_eq!(missing.instances[0].1, missing_callee_key);
        assert_eq!(missing.instances[0].2.as_slice(), &missing_callee_name);
        assert_eq!(missing.instances[1].0, caller_row);
        assert_eq!(missing.instances[1].1, vtable_fn_key);
        assert_eq!(missing.instances[1].2.as_slice(), &vtable_fn_name);
        assert_eq!(missing.statics.as_slice(), &[missing_static_key]);
        assert_eq!(missing.tls.as_slice(), &[missing_tls_key]);

        // Planted-bug control (Rule 23): stripping `declared` (the old BLZLIR01 bug) MUST be
        // refused immediately with `ExportLtoError::MissingDeclared` rather than leaving a hole.
        let (decoded_built_control, _) =
            decode_built_lir(cached_bytes.as_slice(), shared).expect("decode BLZLIR02 control");
        let origins_dropped_decl = [LtoOrigin {
            row: caller_row,
            symbol: caller_sym,
            is_seed: true,
            orig_unreachables: 0,
            built: decoded_built_control,
            declared: Vec::new_in(shared),
        }];
        let err = collect_missing_walk_targets(&walk, &origins_dropped_decl, &out_bodies, shared, alloc)
            .err()
            .expect("missing declaration must fail with ExportLtoError::MissingDeclared");
        assert!(
            matches!(
                err,
                ExportLtoError::MissingDeclared { from, target }
                    if from == caller_sym.0 && target == CodeTarget { space: Space::Func, symbol: missing_callee_sym }
            ),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn emitted_body_cache_codec_round_trip_and_corrupt_rejection() {
        let shared = blaze_own::testing::heap().shared();
        // EmittedBody codec round-trip + corruption check.
        let mut extra_name: Vec<u8, SharedAlloc<'static>> = Vec::new_in(shared);
        extra_name.extend_from_slice(b"__addtf3");
        let mut extras: Vec<(ItemKey, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>> = Vec::new_in(shared);
        extras.push((ItemKey(0x4242), extra_name));
        let clob = CallClobbers { clobbered: RegSet(0x0000_000F | (0x0000_0003u64 << 32)) };
        let rec_bytes = b"bzcode-fake-record-payload";
        let em_enc = encode_emitted_body(rec_bytes, extras.as_slice(), clob, shared).expect("encode_emitted_body");
        let (dec_rec, dec_extra, dec_clob) = decode_emitted_body(em_enc.as_slice(), shared).expect("decode_emitted_body");
        assert_eq!(dec_rec.as_slice(), rec_bytes);
        assert_eq!(dec_clob, clob);
        assert_eq!(dec_extra.len(), 1);
        assert_eq!(dec_extra[0].0, ItemKey(0x4242));
        assert_eq!(dec_extra[0].1.as_slice(), b"__addtf3");
        assert!(decode_emitted_body(&em_enc[..em_enc.len() - 1], shared).is_none());
    }

    #[test]
    fn emit_cache_key_invalidates_on_every_input_planted_controls() {
        let alloc = blaze_own::testing::heap().alloc();
        let func_a = build_straight_line_fn(false, alloc);
        let func_b = build_straight_line_fn(true, alloc);
        let producer_a = [0x11u8; 32];
        let producer_b = [0x22u8; 32];
        let funcs_a = [Callee::Symbol(Symbol::of_name("callee_1"))];
        let funcs_b = [Callee::Symbol(Symbol::of_name("callee_2"))];
        let names_a = InstanceNames { funcs: &funcs_a, statics: &[], c_statics: &[], vtables: &[], tls: &[] };
        let names_b = InstanceNames { funcs: &funcs_b, statics: &[], c_statics: &[], vtables: &[], tls: &[] };
        let clob_a = [(0u32, CallClobbers::EMPTY)];
        let clob_b = [(0u32, CallClobbers::C_ABI_ARM64)];
        let trap_a = [TrapSource { ordinal: 0, kind: TrapKind::BodyEnd, site: None, subject: None }];
        let base_abi = SotaRecordAbiOpts {
            machine: arm64_machine(),
            is_hot: false,
            scratch_order: None,
            preserve_all_cold: false,
            callee_clobbers: &clob_a,
            internal_fast: false,
            internal_fast_callees: &[],
        };
        let k_base = compute_emit_cache_key(producer_a, func_a.view(), &names_a, Routines::NONE, &[], base_abi, alloc);
        let k_same = compute_emit_cache_key(producer_a, func_a.view(), &names_a, Routines::NONE, &[], base_abi, alloc);
        assert_eq!(k_base, k_same, "identical inputs must produce identical emit cache key");

        // Planted controls (Rule 21 & Rule 23): mutating ANY input must invalidate the key!
        assert_ne!(k_base, compute_emit_cache_key(producer_b, func_a.view(), &names_a, Routines::NONE, &[], base_abi, alloc));
        assert_ne!(k_base, compute_emit_cache_key(producer_a, func_b.view(), &names_a, Routines::NONE, &[], base_abi, alloc));
        assert_ne!(k_base, compute_emit_cache_key(producer_a, func_a.view(), &names_b, Routines::NONE, &[], base_abi, alloc));
        assert_ne!(k_base, compute_emit_cache_key(producer_a, func_a.view(), &names_a, Routines::NONE, &trap_a, base_abi, alloc));
        assert_ne!(k_base, compute_emit_cache_key(producer_a, func_a.view(), &names_a, Routines::NONE, &[], SotaRecordAbiOpts { is_hot: true, ..base_abi }, alloc));
        assert_ne!(k_base, compute_emit_cache_key(producer_a, func_a.view(), &names_a, Routines::NONE, &[], SotaRecordAbiOpts { preserve_all_cold: true, ..base_abi }, alloc));
        assert_ne!(k_base, compute_emit_cache_key(producer_a, func_a.view(), &names_a, Routines::NONE, &[], SotaRecordAbiOpts { internal_fast: true, ..base_abi }, alloc));
        assert_ne!(k_base, compute_emit_cache_key(producer_a, func_a.view(), &names_a, Routines::NONE, &[], SotaRecordAbiOpts { callee_clobbers: &clob_b, ..base_abi }, alloc));
        assert_ne!(k_base, compute_emit_cache_key(producer_a, func_a.view(), &names_a, Routines::NONE, &[], SotaRecordAbiOpts { scratch_order: Some(ScratchOrder::DEFAULT), ..base_abi }, alloc));
        let mut m2 = arm64_machine();
        m2.features ^= Machine::SME2;
        assert_ne!(k_base, compute_emit_cache_key(producer_a, func_a.view(), &names_a, Routines::NONE, &[], SotaRecordAbiOpts { machine: m2, ..base_abi }, alloc));
    }

    /// Real multi-threaded [`LtoRunner`] for measuring Stage 3d wavefront parallel + incremental
    /// cached LTO emission speedups on Apple Silicon / Linux arm64 (Rule 16 & Rule 84).
    struct ThreadLtoRunner {
        workers: usize,
    }

    impl LtoRunner for ThreadLtoRunner {
        fn par_map<T: Send, F>(
            &self,
            count: usize,
            out_alloc: SharedAlloc<'static>,
            f: F,
        ) -> Result<Vec<T, SharedAlloc<'static>>, SimplifyError>
        where
            F: Fn(usize, Alloc<'_>) -> T + Sync,
        {
            if self.workers <= 1 || count <= 1 {
                return SequentialLtoRunner.par_map(count, out_alloc, f);
            }
            let w = self.workers.min(count);
            let next = core::sync::atomic::AtomicUsize::new(0);
            let next_ref = &next;
            let f_ref = &f;
            let parts = std::thread::scope(|s| {
                let mut handles = std::vec::Vec::with_capacity(w);
                for _ in 0..w {
                    handles.push(s.spawn(move || {
                        let mut frame = blaze_mem::Frame::with_reserve(256 << 20, blaze_mem::Tag::Codegen)
                            .expect("worker frame");
                        let mut local = std::vec::Vec::new();
                        loop {
                            let idx = next_ref.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                            if idx >= count {
                                break;
                            }
                            frame.reset();
                            let erased = blaze_own::Erased::new(&frame);
                            local.push((idx, f_ref(idx, erased.handle())));
                        }
                        local
                    }));
                }
                handles.into_iter().map(|h| h.join().expect("worker join")).collect::<std::vec::Vec<_>>()
            });
            let mut slots: std::vec::Vec<Option<T>> = (0..count).map(|_| None).collect();
            for part in parts {
                for (idx, val) in part {
                    slots[idx] = Some(val);
                }
            }
            let mut out = Vec::with_capacity_in(count, out_alloc);
            for slot in slots {
                out.push(slot.expect("every slot filled"));
            }
            Ok(out)
        }
    }

    /// Release benchmark comparing Stage 3d SOTA+superopt+IPRA emission:
    /// 1-worker cold vs 8-worker cold vs 16-worker cold vs 1-fn incremental edit with emit cache!
    /// Run with:
    /// `cargo +nightly-2026-10-27 test --release -p blaze-ladder --lib wavefront_emit_and_cache_benchmark -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn wavefront_emit_and_cache_benchmark() {
        let shared = blaze_own::testing::heap().shared();
        let alloc = blaze_own::testing::heap().alloc();
        let num_fns = 1024usize;
        let machine = arm64_machine();
        let producer = [0xABu8; 32];

        // Build 1024 realistic loop-bearing ARM64 functions (hot tier -> full SOTA + superopt).
        let mut funcs: std::vec::Vec<blaze_lir::Func<SharedAlloc<'static>>> = std::vec::Vec::with_capacity(num_fns);
        for i in 0..num_fns {
            let mut b: Builder<SharedAlloc<'static>, Full> = Builder::new_in(64, shared);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let header = b.create_block(BlockKind::Normal);
            let body = b.create_block(BlockKind::Normal);
            let exit = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let n = b.param(Ty::I64);
            let seed = b.param(Ty::I64);
            let zero = b.iconst(Ty::I64, 0);
            b.jump((header, &[zero, seed]));
            b.begin(header);
            let iv = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let cmp = b.icmp(blaze_lir::IntCond::Slt, iv, n);
            b.br(Flags::NONE, cmp, (body, &[]), (exit, &[acc]));
            b.begin(body);
            let k = b.iconst(Ty::I64, (i as u128) + 3);
            let mul = b.binary(Op::Mul, Flags::NONE, iv, k);
            let xor = b.binary(Op::Xor, Flags::NONE, acc, mul);
            let acc2 = b.binary(Op::Add, Flags::NONE, xor, k);
            let one = b.iconst(Ty::I64, 1);
            let iv2 = b.binary(Op::Add, Flags::NONE, iv, one);
            b.jump((header, &[iv2, acc2]));
            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            funcs.push(b.finish_checked().expect("valid bench loop fn"));
        }

        let names = InstanceNames { funcs: &[], statics: &[], c_statics: &[], vtables: &[], tls: &[] };
        let abi_opts = SotaRecordAbiOpts {
            machine,
            is_hot: true,
            scratch_order: Some(ScratchOrder::DEFAULT),
            preserve_all_cold: false,
            callee_clobbers: &[],
            internal_fast: false,
            internal_fast_callees: &[],
        };

        let mut cache: std::collections::HashMap<[u8; 32], std::vec::Vec<u8>> = std::collections::HashMap::new();
        let mut baseline_recs: Option<std::vec::Vec<std::vec::Vec<u8>>> = None;
        let mut seq_ms = 0.0f64;

        for workers in [1usize, 4, 8, 16] {
            let runner = ThreadLtoRunner { workers };
            let t0 = std::time::Instant::now();
            let results = runner
                .par_map(num_fns, shared, |idx, fn_alloc| {
                    let verified = verify(funcs[idx].view()).expect("verified");
                    let key = compute_emit_cache_key(producer, verified.view(), &names, Routines::NONE, &[], abi_opts, fn_alloc);
                    let (rec, _, clob) = emit_code_record_for_machine_with_abi(
                        verified,
                        &names,
                        Routines::NONE,
                        &[],
                        abi_opts,
                        fn_alloc,
                    )
                    .expect("emit succeeds");
                    let enc = encode_emitted_body(rec.as_slice(), &[], clob, shared).expect("encode");
                    (key, rec.as_slice().to_vec(), enc.as_slice().to_vec())
                })
                .expect("par_map succeeds");
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            let fns_s = (num_fns as f64) / (ms / 1000.0);
            let fns_s_core = fns_s / (workers as f64);
            if workers == 1 {
                seq_ms = ms;
                let mut recs = std::vec::Vec::with_capacity(num_fns);
                for (k, r, enc) in results {
                    cache.insert(k, enc);
                    recs.push(r);
                }
                baseline_recs = Some(recs);
            } else {
                for (idx, (_, r, _)) in results.into_iter().enumerate() {
                    assert_eq!(r, baseline_recs.as_ref().unwrap()[idx], "bit-identical mismatch at fn {idx}");
                }
            }
            std::eprintln!(
                "lto_emit cold ({num_fns} hot loop fns) workers={workers:>2}: wall={ms:6.1}ms ({:5.2}x) | {fns_s:7.0} fns/s ({fns_s_core:6.0} fns/s/core)",
                seq_ms / ms
            );
        }

        // Incremental 1-fn edit: modify function 0, probe cache for all 1024 functions, emit ONLY misses!
        {
            let mut b: Builder<SharedAlloc<'static>, Full> = Builder::new_in(64, shared);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let a = b.param(Ty::I64);
            let c = b.param(Ty::I64);
            let sum = b.binary(Op::Add, Flags::NONE, a, c);
            b.ret(&[sum]);
            funcs[0] = b.finish_checked().expect("edited fn 0");
        }
        let runner = ThreadLtoRunner { workers: 8 };
        let t_inc = std::time::Instant::now();
        let keys = runner
            .par_map(num_fns, shared, |idx, fn_alloc| {
                compute_emit_cache_key(producer, funcs[idx].view(), &names, Routines::NONE, &[], abi_opts, fn_alloc)
            })
            .expect("key hash");
        let mut hits = 0usize;
        let mut misses: std::vec::Vec<usize> = std::vec::Vec::new();
        for (idx, &k) in keys.iter().enumerate() {
            if let Some(bytes) = cache.get(&k) {
                let (dec_rec, _, _) = decode_emitted_body(bytes, shared).expect("decode hit");
                assert_eq!(dec_rec.as_slice(), baseline_recs.as_ref().unwrap()[idx].as_slice());
                hits += 1;
            } else {
                misses.push(idx);
            }
        }
        let _reemitted = runner
            .par_map(misses.len(), shared, |p, fn_alloc| {
                let idx = misses[p];
                let verified = verify(funcs[idx].view()).expect("verified");
                let (rec, _, clob) = emit_code_record_for_machine_with_abi(
                    verified,
                    &names,
                    Routines::NONE,
                    &[],
                    abi_opts,
                    fn_alloc,
                )
                .expect("emit miss");
                (rec.as_slice().to_vec(), clob)
            })
            .expect("emit misses");
        let inc_ms = t_inc.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(hits, num_fns - 1);
        assert_eq!(misses.as_slice(), &[0]);
        std::eprintln!(
            "lto_emit incr 1-fn edit ({hits}/{num_fns} hits, 1 miss) workers= 8: wall={inc_ms:6.2}ms ({:5.1}x vs 1-core cold!)",
            seq_ms / inc_ms
        );
        let _ = alloc;
    }
}
