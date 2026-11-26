#![forbid(unsafe_code)]
//! Whole-program LIR-to-Wasm64 (`memory64`) linker over a record walk.
//!
//! Compiles any walked Rust program (`Walk` + `Maker`) from verified `blaze-lir` (`BuiltLir`) and
//! sealed `DatumView` statics, thread-locals, and vtables directly into a runnable `wasm64`
//! (`memory64`) `.wasm` binary via [`blaze_export::lir_wasm64::compile_lir_wasm64`].
//!
//! Pure sans-IO, explicit allocator only, zero `unsafe`, zero recursion.

use core::fmt;

use blaze_code::datum::{DatumView, OWN_DATUM, Place as DatumPlace};
use blaze_code::record::{Space, Symbol};
use blaze_export::lir_wasm64::{
    LirWasm64Error, LirWasm64Func, LirWasm64Global, LirWasm64GlobalValue, LirWasm64Import,
    LirWasm64Module, LirWasm64RelocTarget, LirWasm64Static, LirWasm64StaticReloc,
    compile_lir_wasm64,
};
use blaze_export::wasm64::{
    HOST_IMPORT_COUNT, HOST_IMPORT_SIGS, HOST_MODULE, ValType, Wasm64Error, Wasm64Exported,
    Wasm64Opts,
};
use blaze_lir::builder::{Param, Variadic};
use blaze_lir::func::{Conv, RefId, RefKind, SigId};
use blaze_lir::ty::{Form, Scalar, Ty};
use blaze_lir::{BlockKind, Builder as LirBuilder, Flags, Full, Func, Op, View};
use blaze_own::{AVec, Alloc, Vec};
use blaze_typeck::ItemKey;

use super::{BuiltLir, Maker, RecordsProgram, Typing};
use crate::body_ids::{Grant, Make, RowKind, Walk};
use crate::code_records::Callee;
use crate::code_records::statics::StaticEntryView;
use crate::run::RunError;
use crate::symbols::{static_symbol, tls_symbol};

/// Inline memory intrinsic module recognized by `blaze_export::lir_wasm64`.
const INTRINSIC_MODULE: &str = "__blaze_wasm64";

/// Maximum compound type nesting depth handled iteratively without recursion (Rule 18).
const MAX_COMPOUND_DEPTH: usize = 64;

/// Reserve gap between defined function places (plus optional synthesized `_start`) and import
/// places so the two place namespaces stay strictly disjoint.
const IMPORT_PLACE_RESERVE: usize = 16;

/// Configuration for [`compile_walk_to_wasm64`].
#[derive(Clone, Copy, Debug)]
pub struct Wasm64BuildOpts<'a> {
    pub module_name: &'a str,
    /// Optional entry symbol; when `Some(sym)` and no export is already named `"_start"`,
    /// synthesizes a `_start() -> ()` wrapper that calls `sym` (passing zero arguments for any
    /// C-main `(i32, ptr)` parameters and discarding any return values).
    pub start_symbol: Option<Symbol>,
    /// Defined function symbols to export from the `.wasm` module by name.
    pub exports: &'a [(Symbol, &'a str)],
    /// Exported immutable `i64` globals resolving to the linear-memory address of the given
    /// static/vtable `Symbol`.
    pub export_globals: &'a [(&'a str, Symbol)],
    /// Whether to include the 3 built-in `blaze_host_v1` sans-IO ring imports.
    pub include_blaze_host_v1: bool,
    /// Memory layout options for the emitted `memory64` module.
    pub opts: Wasm64Opts,
}

/// Summary returned by [`compile_walk_to_wasm64`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Wasm64BuildOut {
    pub func_count: usize,
    pub static_count: usize,
    pub import_count: usize,
    pub exported: Wasm64Exported,
}

/// Why whole-program Wasm64 linking stopped, naming the symbol or mismatch (Rule 3).
#[derive(Debug)]
pub enum Wasm64BuildError {
    NotRunnable { symbol: [u8; 32], tabled: ItemKey },
    BuildLir { symbol: [u8; 32], error: RunError },
    StaticDatum { symbol: [u8; 32], shown: &'static str },
    UnresolvedSymbol { space: &'static str, symbol: [u8; 32] },
    UnresolvedImport { name_hash: [u8; 32], what: &'static str },
    ImportSigMismatch { what: &'static str },
    Export(LirWasm64Error),
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

impl fmt::Display for Wasm64BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Wasm64BuildError::NotRunnable { symbol, tabled } => write!(
                f,
                "wasm64 link: reached item {:#x} (symbol {}) has no runnable grant (wasm64.rs)",
                tabled.0,
                Hex(symbol)
            ),
            Wasm64BuildError::BuildLir { symbol, error } => {
                write!(f, "wasm64 link: building LIR/static for {}: {error} (wasm64.rs)", Hex(symbol))
            }
            Wasm64BuildError::StaticDatum { symbol, shown } => {
                write!(f, "wasm64 link: invalid static datum for {}: {shown} (wasm64.rs)", Hex(symbol))
            }
            Wasm64BuildError::UnresolvedSymbol { space, symbol } => {
                write!(f, "wasm64 link: unresolved {space} symbol {} (wasm64.rs)", Hex(symbol))
            }
            Wasm64BuildError::UnresolvedImport { name_hash, what } => {
                write!(f, "wasm64 link: {what} (name hash {}) (wasm64.rs)", Hex(name_hash))
            }
            Wasm64BuildError::ImportSigMismatch { what } => {
                write!(f, "wasm64 link: import signature mismatch: {what} (wasm64.rs)")
            }
            Wasm64BuildError::Export(error) => {
                write!(f, "wasm64 link: export failed: {error} (wasm64.rs)")
            }
        }
    }
}

impl core::error::Error for Wasm64BuildError {}

/// Maps standard C-extern names used by Blaze's Wasm64 guests (inline memory intrinsics,
/// `wasi_snapshot_preview1_64`, `blaze_build_script_64`, `blaze_proc_macro_64`, and
/// `blaze_host_v1`) to their `(module, field)` import pair.
pub fn default_wasm64_import(c_name: &str) -> Option<(&'static str, &'static str)> {
    match c_name {
        // Built-in memory intrinsics
        "llvm.wasm.memory.grow.i64" | "__blaze_wasm64_memory_grow" => {
            Some((INTRINSIC_MODULE, "memory_grow"))
        }
        "llvm.wasm.memory.size.i64" | "__blaze_wasm64_memory_size" => {
            Some((INTRINSIC_MODULE, "memory_size"))
        }
        "__blaze_wasm64_heap_base" => Some((INTRINSIC_MODULE, "heap_base")),

        // wasi_snapshot_preview1_64
        "args_get" => Some(("wasi_snapshot_preview1_64", "args_get")),
        "args_sizes_get" => Some(("wasi_snapshot_preview1_64", "args_sizes_get")),
        "clock_res_get" => Some(("wasi_snapshot_preview1_64", "clock_res_get")),
        "clock_time_get" => Some(("wasi_snapshot_preview1_64", "clock_time_get")),
        "environ_get" => Some(("wasi_snapshot_preview1_64", "environ_get")),
        "environ_sizes_get" => Some(("wasi_snapshot_preview1_64", "environ_sizes_get")),
        "fd_close" => Some(("wasi_snapshot_preview1_64", "fd_close")),
        "fd_datasync" => Some(("wasi_snapshot_preview1_64", "fd_datasync")),
        "fd_fdstat_get" => Some(("wasi_snapshot_preview1_64", "fd_fdstat_get")),
        "fd_fdstat_set_flags" => Some(("wasi_snapshot_preview1_64", "fd_fdstat_set_flags")),
        "fd_filestat_get" => Some(("wasi_snapshot_preview1_64", "fd_filestat_get")),
        "fd_filestat_set_size" => Some(("wasi_snapshot_preview1_64", "fd_filestat_set_size")),
        "fd_prestat_dir_name" => Some(("wasi_snapshot_preview1_64", "fd_prestat_dir_name")),
        "fd_prestat_get" => Some(("wasi_snapshot_preview1_64", "fd_prestat_get")),
        "fd_read" => Some(("wasi_snapshot_preview1_64", "fd_read")),
        "fd_readdir" => Some(("wasi_snapshot_preview1_64", "fd_readdir")),
        "fd_seek" => Some(("wasi_snapshot_preview1_64", "fd_seek")),
        "fd_sync" => Some(("wasi_snapshot_preview1_64", "fd_sync")),
        "fd_tell" => Some(("wasi_snapshot_preview1_64", "fd_tell")),
        "fd_write" => Some(("wasi_snapshot_preview1_64", "fd_write")),
        "path_create_directory" => Some(("wasi_snapshot_preview1_64", "path_create_directory")),
        "path_filestat_get" => Some(("wasi_snapshot_preview1_64", "path_filestat_get")),
        "path_link" => Some(("wasi_snapshot_preview1_64", "path_link")),
        "path_open" => Some(("wasi_snapshot_preview1_64", "path_open")),
        "path_readlink" => Some(("wasi_snapshot_preview1_64", "path_readlink")),
        "path_remove_directory" => Some(("wasi_snapshot_preview1_64", "path_remove_directory")),
        "path_rename" => Some(("wasi_snapshot_preview1_64", "path_rename")),
        "path_unlink_file" => Some(("wasi_snapshot_preview1_64", "path_unlink_file")),
        "poll_oneoff" => Some(("wasi_snapshot_preview1_64", "poll_oneoff")),
        "proc_exit" => Some(("wasi_snapshot_preview1_64", "proc_exit")),
        "random_get" => Some(("wasi_snapshot_preview1_64", "random_get")),
        "sched_yield" => Some(("wasi_snapshot_preview1_64", "sched_yield")),

        // blaze_build_script_64
        "blaze_bs_found" | "blaze_found" => Some(("blaze_build_script_64", "found")),
        "blaze_bs_spawn" | "blaze_spawn" => Some(("blaze_build_script_64", "spawn")),
        "blaze_bs_reply" | "blaze_reply" => Some(("blaze_build_script_64", "reply")),

        // blaze_proc_macro_64
        "blaze_pm_dispatch" => Some(("blaze_proc_macro_64", "dispatch")),
        "blaze_pm_reply" => Some(("blaze_proc_macro_64", "reply")),
        "blaze_pm_output" => Some(("blaze_proc_macro_64", "output")),
        "blaze_pm_env_var" => Some(("blaze_proc_macro_64", "env_var")),
        "blaze_pm_current_dir" => Some(("blaze_proc_macro_64", "current_dir")),

        // blaze_host_v1
        "submit_wait" | "blaze_host_submit_wait" => Some((HOST_MODULE, "submit_wait")),
        "host_caps" | "blaze_host_caps" => Some((HOST_MODULE, "host_caps")),
        "host_log" | "blaze_host_log" => Some((HOST_MODULE, "host_log")),

        _ => None,
    }
}

struct OwnedFunc<'a> {
    place: u32,
    built: BuiltLir<Alloc<'a>>,
}

struct OwnedDatum<'a> {
    symbol: Symbol,
    encoded: AVec<'a, u8>,
}

struct PreparedStatic<'a> {
    place: u32,
    align: u32,
    bytes: AVec<'a, u8>,
    relocs: AVec<'a, LirWasm64StaticReloc>,
}

struct OwnedImport<'a> {
    place: u32,
    module: &'static str,
    field: &'static str,
    params: AVec<'a, ValType>,
    results: AVec<'a, ValType>,
}

fn find_place_by_sym(sorted: &[([u8; 32], u32)], sym: [u8; 32]) -> Option<u32> {
    let idx = sorted.binary_search_by(|row| row.0.cmp(&sym)).ok()?;
    sorted.get(idx).map(|row| row.1)
}

fn find_static_place(sorted: &[OwnedDatum<'_>], sym: [u8; 32]) -> Option<u32> {
    let idx = sorted.binary_search_by(|row| row.symbol.0.cmp(&sym)).ok()?;
    u32::try_from(idx).ok()
}

fn find_export_place(exports: &[(&str, u32)], name: &str) -> Option<u32> {
    for &(exp_name, place) in exports {
        if exp_name == name {
            return Some(place);
        }
    }
    None
}

fn push_unique_datum<'a>(
    dst: &mut AVec<'a, OwnedDatum<'a>>,
    symbol: Symbol,
    bytes: &[u8],
    alloc: Alloc<'a>,
) {
    if dst.iter().any(|d| d.symbol == symbol) {
        return;
    }
    let mut encoded: AVec<'a, u8> = Vec::with_capacity_in(bytes.len(), alloc);
    encoded.extend_from_slice(bytes);
    dst.push(OwnedDatum { symbol, encoded });
}

fn flatten_ty_wasm(
    view: View<'_>,
    ty: Ty,
    dst: &mut AVec<'_, ValType>,
) -> Result<(), Wasm64BuildError> {
    let mut stack = [Ty::NONE; MAX_COMPOUND_DEPTH];
    let mut top = 0usize;
    stack[top] = ty;
    top += 1;
    while top > 0 {
        top -= 1;
        let cur = stack[top];
        if cur.is_none() {
            continue;
        }
        match cur.form() {
            Form::Scalar => match cur.elem() {
                Scalar::None => {}
                Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 => dst.push(ValType::I32),
                Scalar::I64 | Scalar::Ptr => dst.push(ValType::I64),
                Scalar::F32 => dst.push(ValType::F32),
                Scalar::F64 => dst.push(ValType::F64),
                Scalar::I128 => {
                    dst.push(ValType::I64);
                    dst.push(ValType::I64);
                }
                Scalar::F16 | Scalar::F128 => {
                    return Err(Wasm64BuildError::ImportSigMismatch {
                        what: "unsupported f16/f128 scalar in import signature",
                    });
                }
            },
            Form::Compound => {
                let members = view.members(cur);
                if top + members.len() > MAX_COMPOUND_DEPTH {
                    return Err(Wasm64BuildError::ImportSigMismatch {
                        what: "compound nesting depth exceeded limit in import signature",
                    });
                }
                for &member in members.iter().rev() {
                    stack[top] = Ty(member);
                    top += 1;
                }
            }
            Form::Vector | Form::Scalable => {
                return Err(Wasm64BuildError::ImportSigMismatch {
                    what: "unsupported vector type in import signature",
                });
            }
        }
    }
    Ok(())
}

fn flatten_sig_wasm<'a>(
    view: View<'_>,
    sig_id: SigId,
    alloc: Alloc<'a>,
) -> Result<(AVec<'a, ValType>, AVec<'a, ValType>), Wasm64BuildError> {
    if sig_id.index() >= view.sigs.len() {
        return Err(Wasm64BuildError::ImportSigMismatch {
            what: "call-site signature index out of range",
        });
    }
    let sig = view.sig(sig_id);
    let mut params = Vec::new_in(alloc);
    for p in sig.params {
        flatten_ty_wasm(view, Ty(p.ty), &mut params)?;
    }
    let mut results = Vec::new_in(alloc);
    for r in sig.rets {
        flatten_ty_wasm(view, Ty(r.ty), &mut results)?;
    }
    Ok((params, results))
}

fn find_call_site_sig<'a>(
    view: View<'_>,
    target_ref: RefId,
    alloc: Alloc<'a>,
) -> Result<Option<(AVec<'a, ValType>, AVec<'a, ValType>)>, Wasm64BuildError> {
    let mut found: Option<(AVec<'a, ValType>, AVec<'a, ValType>)> = None;
    for inst in 0..view.len() {
        match view.op(inst) {
            Op::Call | Op::Invoke | Op::TailCall => {
                let mut o = view.operands(inst);
                let callee_ref = o.reference();
                let sig_id = o.sig();
                if callee_ref != target_ref {
                    continue;
                }
                let (params, results) = flatten_sig_wasm(view, sig_id, alloc)?;
                if let Some((ref exp_p, ref exp_r)) = found {
                    if exp_p.as_slice() != params.as_slice() || exp_r.as_slice() != results.as_slice()
                    {
                        return Err(Wasm64BuildError::ImportSigMismatch {
                            what: "C extern called with conflicting signatures in one function",
                        });
                    }
                } else {
                    found = Some((params, results));
                }
            }
            _ => {}
        }
    }
    Ok(found)
}

fn builtin_import_sig(module: &str, field: &str) -> Option<(&'static [ValType], &'static [ValType])> {
    if module == INTRINSIC_MODULE {
        return match field {
            "memory_grow" => Some((&[ValType::I64], &[ValType::I64])),
            "memory_size" | "heap_base" => Some((&[], &[ValType::I64])),
            _ => None,
        };
    }
    if module == HOST_MODULE {
        return match field {
            "submit_wait" => Some(HOST_IMPORT_SIGS[0]),
            "host_caps" => Some(HOST_IMPORT_SIGS[1]),
            "host_log" => Some(HOST_IMPORT_SIGS[2]),
            _ => None,
        };
    }
    None
}

fn intern_import<'a>(
    imports: &mut AVec<'a, OwnedImport<'a>>,
    import_place_base: u32,
    module: &'static str,
    field: &'static str,
    params: AVec<'a, ValType>,
    results: AVec<'a, ValType>,
) -> Result<u32, Wasm64BuildError> {
    for imp in imports.iter() {
        if imp.module == module && imp.field == field {
            if imp.params.as_slice() != params.as_slice()
                || imp.results.as_slice() != results.as_slice()
            {
                return Err(Wasm64BuildError::ImportSigMismatch {
                    what: "import called with conflicting signatures across functions",
                });
            }
            return Ok(imp.place);
        }
    }
    let idx = u32::try_from(imports.len()).map_err(|_| Wasm64BuildError::Export(LirWasm64Error::Overflow { what: "import count" }))?;
    let place = import_place_base
        .checked_add(idx)
        .ok_or(Wasm64BuildError::Export(LirWasm64Error::Overflow { what: "import place" }))?;
    imports.push(OwnedImport { place, module, field, params, results });
    Ok(place)
}

/// Iteratively copies `ty` (including compound types) from `src` into `dst` builder.
fn copy_ty_into_builder<'a>(
    src: View<'_>,
    ty: Ty,
    dst: &mut LirBuilder<Alloc<'a>, Full>,
    alloc: Alloc<'a>,
) -> Result<Ty, Wasm64BuildError> {
    if !matches!(ty.form(), Form::Compound) {
        return Ok(ty);
    }
    // Iterative post-order traversal over compound trees (bounded by MAX_COMPOUND_DEPTH).
    let mut work: AVec<'a, (Ty, bool)> = Vec::new_in(alloc);
    let mut values: AVec<'a, Ty> = Vec::new_in(alloc);
    work.push((ty, false));
    let mut steps = 0usize;
    while let Some((cur, expanded)) = work.pop() {
        steps += 1;
        if steps > MAX_COMPOUND_DEPTH * 8 {
            return Err(Wasm64BuildError::ImportSigMismatch {
                what: "compound type too deep for synthesized _start signature",
            });
        }
        if !matches!(cur.form(), Form::Compound) {
            values.push(cur);
            continue;
        }
        let members = src.members(cur);
        if expanded {
            let start = values.len().checked_sub(members.len()).ok_or(
                Wasm64BuildError::ImportSigMismatch {
                    what: "internal compound stack underflow",
                },
            )?;
            let built = dst.compound(&values.as_slice()[start..]);
            values.truncate(start);
            values.push(built);
        } else {
            work.push((cur, true));
            for &m in members.iter().rev() {
                work.push((Ty(m), false));
            }
        }
    }
    values.pop().ok_or(Wasm64BuildError::ImportSigMismatch {
        what: "empty compound copy result",
    })
}

/// Synthesizes a `_start() -> ()` LIR function that calls `entry_place` (passing zero values for
/// scalar C-main parameters such as `(i32, ptr)` and discarding any return value).
fn synthesize_start_func<'a>(
    entry_place: u32,
    entry_view: View<'_>,
    alloc: Alloc<'a>,
) -> Result<Func<Alloc<'a>>, Wasm64BuildError> {
    let entry_sig_id = SigId(entry_view.head.sig);
    if entry_sig_id.index() >= entry_view.sigs.len() {
        return Err(Wasm64BuildError::ImportSigMismatch {
            what: "start_symbol signature index out of range",
        });
    }
    let entry_sig = entry_view.sig(entry_sig_id);

    let mut b = LirBuilder::<Alloc<'a>, Full>::new_in(64, alloc);
    let start_sig = b.sig(Conv::C, Variadic::No, &[], &[]);
    b.set_signature(start_sig);

    let mut call_params: AVec<'a, Param> = Vec::with_capacity_in(entry_sig.params.len(), alloc);
    for p in entry_sig.params {
        let ty = copy_ty_into_builder(entry_view, Ty(p.ty), &mut b, alloc)?;
        call_params.push(Param::of(ty));
    }
    let mut call_rets: AVec<'a, Param> = Vec::with_capacity_in(entry_sig.rets.len(), alloc);
    for r in entry_sig.rets {
        let ty = copy_ty_into_builder(entry_view, Ty(r.ty), &mut b, alloc)?;
        call_rets.push(Param::of(ty));
    }

    let call_sig = b.sig(Conv::C, Variadic::No, call_params.as_slice(), call_rets.as_slice());
    let callee_ref = b.reference(RefKind::Func, entry_place);
    let entry_block = b.create_block(BlockKind::Normal);
    b.begin(entry_block);

    let mut args: AVec<'a, blaze_lir::Value> = Vec::with_capacity_in(call_params.len(), alloc);
    for p in call_params.iter() {
        let ty = p.ty;
        let val = if ty.is_scalar() && ty.is_int() {
            b.iconst(ty, 0)
        } else if ty == Ty::PTR {
            b.null()
        } else if ty.is_scalar() && ty.is_float() {
            b.fconst(ty, 0)
        } else {
            return Err(Wasm64BuildError::ImportSigMismatch {
                what: "start_symbol has unsupported parameter type for synthesized _start",
            });
        };
        args.push(val);
    }

    let _ = b.call(Flags::NONE, callee_ref, call_sig, args.as_slice());
    b.ret(&[]);
    b.finish_checked().map_err(|_| Wasm64BuildError::ImportSigMismatch {
        what: "synthesized _start function failed LIR verification",
    })
}

/// Whole-program linker compiling every reached instance, static, thread-local, and vtable in
/// `walk` directly into a deterministic, spec-compliant `wasm64` (`memory64`) `.wasm` binary.
#[expect(
    clippy::too_many_arguments,
    reason = "whole-program wasm64 linker over walk, grant closure, maker, options, import resolver, output buffer, and allocator"
)]
pub fn compile_walk_to_wasm64<'a, 'n, G, T>(
    walk: &Walk<'_>,
    mut grant_of: impl FnMut(ItemKey) -> Option<G>,
    maker: &mut Maker<'n, G, T>,
    opts: &Wasm64BuildOpts<'_>,
    mut resolve_import: impl FnMut(&str) -> Option<(&'static str, &'static str)>,
    out: &mut AVec<'a, u8>,
    alloc: Alloc<'a>,
) -> Result<Wasm64BuildOut, Wasm64BuildError>
where
    G: Grant,
    T: Typing<RecordsProgram<'n>>,
{
    // 1. Build verified LIR for every reached function instance and collect carried vtable datums.
    let mut funcs: AVec<'a, OwnedFunc<'a>> = Vec::new_in(alloc);
    let mut func_by_sym: AVec<'a, ([u8; 32], u32)> = Vec::new_in(alloc);
    let mut static_datums: AVec<'a, OwnedDatum<'a>> = Vec::new_in(alloc);

    for row in 0..walk.len() {
        if walk.kind(row) != RowKind::Instance {
            continue;
        }
        let tabled = walk.tabled(row);
        let symbol = walk.symbol(row);
        let name = walk.name(row);
        let Some(grant) = grant_of(tabled) else {
            return Err(Wasm64BuildError::NotRunnable { symbol: symbol.0, tabled });
        };
        let built = maker
            .build_lir_in(&grant, tabled, name, alloc)
            .map_err(|error| Wasm64BuildError::BuildLir { symbol: symbol.0, error })?;
        for (vsym, vbytes) in built.vtable_data.iter() {
            push_unique_datum(&mut static_datums, *vsym, vbytes.as_slice(), alloc);
        }
        let place = u32::try_from(funcs.len())
            .map_err(|_| Wasm64BuildError::Export(LirWasm64Error::Overflow { what: "function count" }))?;
        func_by_sym.push((symbol.0, place));
        funcs.push(OwnedFunc { place, built });
    }
    func_by_sym.sort_unstable();

    // 2. Materialize every reached static and thread-local entry (and any vtables they carry).
    let mut entry_buf: AVec<'a, u8> = Vec::new_in(alloc);
    for row in 0..walk.len() {
        let kind = walk.kind(row);
        if kind == RowKind::Instance {
            continue;
        }
        let tabled = walk.tabled(row);
        let symbol = walk.symbol(row);
        let Some(grant) = grant_of(tabled) else {
            return Err(Wasm64BuildError::NotRunnable { symbol: symbol.0, tabled });
        };
        entry_buf.clear();
        let make_res = match kind {
            RowKind::Static => Make::make_static(maker, &grant, tabled, &mut entry_buf),
            RowKind::ThreadLocal => Make::make_thread_local(maker, &grant, tabled, &mut entry_buf),
            RowKind::Instance => Ok(()),
        };
        make_res.map_err(|error| Wasm64BuildError::BuildLir { symbol: symbol.0, error })?;

        let entry_view = StaticEntryView::read(entry_buf.as_slice()).map_err(|_| {
            Wasm64BuildError::StaticDatum {
                symbol: symbol.0,
                shown: "malformed static or thread-local entry",
            }
        })?;
        push_unique_datum(&mut static_datums, symbol, entry_view.datum.encoded(), alloc);
        entry_view
            .each_vtable(
                |vsym, vdatum| {
                    push_unique_datum(&mut static_datums, vsym, vdatum.encoded(), alloc);
                    Ok(())
                },
                |_| Wasm64BuildError::StaticDatum {
                    symbol: symbol.0,
                    shown: "malformed vtable carried by static entry",
                },
            )?;
    }

    // Sort static/TLS/vtable datums by symbol so `static_place` is deterministic and binary-searchable.
    static_datums.sort_unstable_by(|a, b| a.symbol.0.cmp(&b.symbol.0));
    static_datums.dedup_by(|a, b| a.symbol == b.symbol);

    // 3. Resolve requested function exports and build `export_name -> func_place`.
    let mut func_export_names: AVec<'a, Option<&str>> = Vec::with_capacity_in(funcs.len(), alloc);
    func_export_names.resize(funcs.len(), None);
    let mut export_name_to_place: AVec<'a, (&str, u32)> =
        Vec::with_capacity_in(opts.exports.len(), alloc);
    let mut has_start_export = false;

    for &(sym, exp_name) in opts.exports {
        let Some(place) = find_place_by_sym(func_by_sym.as_slice(), sym.0) else {
            return Err(Wasm64BuildError::UnresolvedSymbol { space: "func", symbol: sym.0 });
        };
        if find_export_place(export_name_to_place.as_slice(), exp_name).is_some() {
            return Err(Wasm64BuildError::Export(LirWasm64Error::Export(
                Wasm64Error::DuplicateExport,
            )));
        }
        if let Some(slot) = func_export_names.get_mut(place as usize) {
            if slot.is_some() {
                return Err(Wasm64BuildError::Export(LirWasm64Error::Export(
                    Wasm64Error::DuplicateExport,
                )));
            }
            *slot = Some(exp_name);
        }
        if exp_name == "_start" {
            has_start_export = true;
        }
        export_name_to_place.push((exp_name, place));
    }

    // 4. Rewrite every function's `refs` table into unified module places and intern host imports.
    let import_place_base = u32::try_from(funcs.len().saturating_add(IMPORT_PLACE_RESERVE))
        .map_err(|_| Wasm64BuildError::Export(LirWasm64Error::Overflow { what: "import place base" }))?;
    let mut imports: AVec<'a, OwnedImport<'a>> = Vec::new_in(alloc);
    let mut new_places: AVec<'a, u32> = Vec::new_in(alloc);

    for f in funcs.iter_mut() {
        new_places.clear();
        let ref_len = f.built.func.refs.len();
        new_places.reserve(ref_len);
        {
            let view = f.built.func.view();
            for (ref_idx, rec) in view.refs.iter().enumerate() {
                let Some(kind) = RefKind::from_word(rec.kind) else {
                    return Err(Wasm64BuildError::Export(LirWasm64Error::UnsupportedRef {
                        func: f.place,
                        inst: 0,
                        kind: rec.kind,
                        place: rec.place,
                    }));
                };
                let mapped = match kind {
                    RefKind::Data => rec.place,
                    RefKind::Static => {
                        let Some(&item) = f.built.statics.get(rec.place as usize) else {
                            return Err(Wasm64BuildError::UnresolvedSymbol {
                                space: "static",
                                symbol: [0u8; 32],
                            });
                        };
                        if let Some(c_name) = f.built.foreign_static_name(item) {
                            return Err(Wasm64BuildError::UnresolvedImport {
                                name_hash: Symbol::of_name(c_name).0,
                                what: "foreign extern static is not supported in wasm64",
                            });
                        }
                        let sym = static_symbol(item);
                        let Some(st_place) = find_static_place(static_datums.as_slice(), sym.0)
                        else {
                            return Err(Wasm64BuildError::UnresolvedSymbol {
                                space: "static",
                                symbol: sym.0,
                            });
                        };
                        st_place
                    }
                    RefKind::Tls => {
                        let Some(&item) = f.built.tls.get(rec.place as usize) else {
                            return Err(Wasm64BuildError::UnresolvedSymbol {
                                space: "tls",
                                symbol: [0u8; 32],
                            });
                        };
                        let sym = tls_symbol(item);
                        let Some(st_place) = find_static_place(static_datums.as_slice(), sym.0)
                        else {
                            return Err(Wasm64BuildError::UnresolvedSymbol {
                                space: "tls",
                                symbol: sym.0,
                            });
                        };
                        st_place
                    }
                    RefKind::Vtable => {
                        let Some(&sym) = f.built.vtables.get(rec.place as usize) else {
                            return Err(Wasm64BuildError::UnresolvedSymbol {
                                space: "vtable",
                                symbol: [0u8; 32],
                            });
                        };
                        let Some(st_place) = find_static_place(static_datums.as_slice(), sym.0)
                        else {
                            return Err(Wasm64BuildError::UnresolvedSymbol {
                                space: "vtable",
                                symbol: sym.0,
                            });
                        };
                        st_place
                    }
                    RefKind::Func => match f.built.callee_at(rec.place) {
                        Some(Callee::Symbol(sym)) => {
                            let Some(fn_place) = find_place_by_sym(func_by_sym.as_slice(), sym.0)
                            else {
                                return Err(Wasm64BuildError::UnresolvedSymbol {
                                    space: "func",
                                    symbol: sym.0,
                                });
                            };
                            fn_place
                        }
                        Some(Callee::C(c_name)) => {
                            if let Some(internal_place) =
                                find_export_place(export_name_to_place.as_slice(), c_name)
                            {
                                internal_place
                            } else {
                                let Some((mod_name, field_name)) = resolve_import(c_name) else {
                                    return Err(Wasm64BuildError::UnresolvedImport {
                                        name_hash: Symbol::of_name(c_name).0,
                                        what: "unresolved C extern import",
                                    });
                                };
                                let ref_id = RefId(u32::try_from(ref_idx).map_err(|_| {
                                    Wasm64BuildError::Export(LirWasm64Error::Overflow {
                                        what: "ref index",
                                    })
                                })?);
                                let (params, results) =
                                    if let Some(sig) = find_call_site_sig(view, ref_id, alloc)? {
                                        sig
                                    } else if let Some((bp, br)) =
                                        builtin_import_sig(mod_name, field_name)
                                    {
                                        let mut p = Vec::with_capacity_in(bp.len(), alloc);
                                        p.extend_from_slice(bp);
                                        let mut r = Vec::with_capacity_in(br.len(), alloc);
                                        r.extend_from_slice(br);
                                        (p, r)
                                    } else {
                                        return Err(Wasm64BuildError::UnresolvedImport {
                                            name_hash: Symbol::of_name(c_name).0,
                                            what: "C extern referenced without a direct call signature",
                                        });
                                    };
                                intern_import(
                                    &mut imports,
                                    import_place_base,
                                    mod_name,
                                    field_name,
                                    params,
                                    results,
                                )?
                            }
                        }
                        None => {
                            return Err(Wasm64BuildError::UnresolvedSymbol {
                                space: "func",
                                symbol: [0u8; 32],
                            });
                        }
                    },
                };
                new_places.push(mapped);
            }
        }
        for (dst_rec, &new_place) in f.built.func.refs.iter_mut().zip(new_places.iter()) {
            dst_rec.place = new_place;
        }
    }

    // 5. Convert every static / TLS / vtable `DatumView` into `PreparedStatic`.
    let mut prepared_statics: AVec<'a, PreparedStatic<'a>> =
        Vec::with_capacity_in(static_datums.len(), alloc);

    for (idx, datum) in static_datums.iter().enumerate() {
        let own_place = u32::try_from(idx)
            .map_err(|_| Wasm64BuildError::Export(LirWasm64Error::Overflow { what: "static count" }))?;
        let view = DatumView::read(datum.encoded.as_slice()).map_err(|_| {
            Wasm64BuildError::StaticDatum {
                symbol: datum.symbol.0,
                shown: "invalid sealed datum bytes",
            }
        })?;
        let align = u32::try_from(view.align())
            .map_err(|_| Wasm64BuildError::Export(LirWasm64Error::InvalidAlign { align: u32::MAX }))?;
        let mut bytes: AVec<'a, u8> = match view.place() {
            DatumPlace::Zeroed { len } => {
                let len_usize = usize::try_from(len).map_err(|_| {
                    Wasm64BuildError::Export(LirWasm64Error::Overflow { what: "zeroed static size" })
                })?;
                let mut buf = Vec::with_capacity_in(len_usize, alloc);
                buf.resize(len_usize, 0);
                buf
            }
            DatumPlace::ReadOnly | DatumPlace::Writable => {
                let raw = view.bytes();
                let mut buf = Vec::with_capacity_in(raw.len(), alloc);
                buf.extend_from_slice(raw);
                buf
            }
        };
        let mut relocs: AVec<'a, LirWasm64StaticReloc> = Vec::with_capacity_in(view.words(), alloc);
        for w in 0..view.words() {
            let word = view.word(w);
            let target = if word.target == OWN_DATUM {
                LirWasm64RelocTarget::Static(own_place)
            } else {
                let t = view.target(word.target as usize);
                match t.space {
                    Space::Func => {
                        let Some(fn_place) = find_place_by_sym(func_by_sym.as_slice(), t.symbol.0)
                        else {
                            return Err(Wasm64BuildError::UnresolvedSymbol {
                                space: "func",
                                symbol: t.symbol.0,
                            });
                        };
                        LirWasm64RelocTarget::Func(fn_place)
                    }
                    Space::Static | Space::Vtable | Space::Tls => {
                        let space = match t.space {
                            Space::Static => "static",
                            Space::Vtable => "vtable",
                            _ => "tls",
                        };
                        let Some(st_place) =
                            find_static_place(static_datums.as_slice(), t.symbol.0)
                        else {
                            return Err(Wasm64BuildError::UnresolvedSymbol {
                                space,
                                symbol: t.symbol.0,
                            });
                        };
                        LirWasm64RelocTarget::Static(st_place)
                    }
                    Space::Outside => {
                        if let Some(c_name) = view.name(t.symbol)
                            && let Some(fn_place) =
                                find_export_place(export_name_to_place.as_slice(), c_name)
                        {
                            LirWasm64RelocTarget::Func(fn_place)
                        } else {
                            return Err(Wasm64BuildError::UnresolvedImport {
                                name_hash: t.symbol.0,
                                what: "unresolved external symbol in static datum",
                            });
                        }
                    }
                }
            };
            relocs.push(LirWasm64StaticReloc {
                offset: word.at,
                target,
                addend: word.addend,
            });
        }
        // Keep `bytes` non-empty if empty zeroed? Empty slice is valid in `lir_wasm64`.
        let _ = &mut bytes;
        prepared_statics.push(PreparedStatic { place: own_place, align, bytes, relocs });
    }

    // 6. Synthesize `_start() -> ()` if `opts.start_symbol` is set and no `"_start"` export exists.
    let synthesized_start: Option<(u32, Func<Alloc<'a>>)> =
        if !has_start_export && let Some(entry_sym) = opts.start_symbol {
            let Some(entry_place) = find_place_by_sym(func_by_sym.as_slice(), entry_sym.0) else {
                return Err(Wasm64BuildError::UnresolvedSymbol {
                    space: "func",
                    symbol: entry_sym.0,
                });
            };
            let entry_view = funcs[entry_place as usize].built.func.view();
            let start_func = synthesize_start_func(entry_place, entry_view, alloc)?;
            let start_place = u32::try_from(funcs.len()).map_err(|_| {
                Wasm64BuildError::Export(LirWasm64Error::Overflow { what: "start function place" })
            })?;
            Some((start_place, start_func))
        } else {
            None
        };

    // 7. Resolve `opts.export_globals`.
    let mut lir_globals: AVec<'a, LirWasm64Global<'_>> =
        Vec::with_capacity_in(opts.export_globals.len(), alloc);
    for &(name, sym) in opts.export_globals {
        let Some(place) = find_static_place(static_datums.as_slice(), sym.0) else {
            return Err(Wasm64BuildError::UnresolvedSymbol { space: "static", symbol: sym.0 });
        };
        lir_globals.push(LirWasm64Global {
            name,
            value: LirWasm64GlobalValue::StaticAddr(place),
        });
    }

    // 8. Assemble borrowed `LirWasm64Module` and emit deterministic `.wasm` binary.
    let total_funcs = funcs.len() + usize::from(synthesized_start.is_some());
    let mut lir_funcs: AVec<'a, LirWasm64Func<'_>> = Vec::with_capacity_in(total_funcs, alloc);
    for (i, f) in funcs.iter().enumerate() {
        let export_name = func_export_names.get(i).copied().flatten();
        lir_funcs.push(LirWasm64Func {
            name: export_name.unwrap_or("fn"),
            export_name,
            place: f.place,
            view: f.built.func.view(),
        });
    }
    if let Some((start_place, ref start_func)) = synthesized_start {
        lir_funcs.push(LirWasm64Func {
            name: "_start",
            export_name: Some("_start"),
            place: start_place,
            view: start_func.view(),
        });
    }

    let mut lir_statics: AVec<'a, LirWasm64Static<'_>> =
        Vec::with_capacity_in(prepared_statics.len(), alloc);
    for st in prepared_statics.iter() {
        lir_statics.push(LirWasm64Static {
            place: st.place,
            align: st.align,
            bytes: st.bytes.as_slice(),
            relocs: st.relocs.as_slice(),
        });
    }

    let mut lir_imports: AVec<'a, LirWasm64Import<'_>> =
        Vec::with_capacity_in(imports.len(), alloc);
    let mut wasm_import_count: usize =
        if opts.include_blaze_host_v1 { HOST_IMPORT_COUNT as usize } else { 0 };
    for imp in imports.iter() {
        if imp.module != INTRINSIC_MODULE
            && !(opts.include_blaze_host_v1 && imp.module == HOST_MODULE)
        {
            wasm_import_count += 1;
        }
        lir_imports.push(LirWasm64Import {
            place: imp.place,
            module: imp.module,
            field: imp.field,
            params: imp.params.as_slice(),
            results: imp.results.as_slice(),
        });
    }

    let module = LirWasm64Module {
        name: opts.module_name,
        funcs: lir_funcs.as_slice(),
        statics: lir_statics.as_slice(),
        imports: lir_imports.as_slice(),
        globals: lir_globals.as_slice(),
        include_blaze_host_v1: opts.include_blaze_host_v1,
        opts: opts.opts,
        profile_digest: None,
    };

    let exported =
        compile_lir_wasm64(&module, out, alloc).map_err(Wasm64BuildError::Export)?;

    Ok(Wasm64BuildOut {
        func_count: lir_funcs.len(),
        static_count: lir_statics.len(),
        import_count: wasm_import_count,
        exported,
    })
}
