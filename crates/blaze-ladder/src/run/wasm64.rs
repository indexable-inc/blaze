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
    NULL_STATIC_PLACE, compile_lir_wasm64,
};
use blaze_export::wasm64::{
    HOST_IMPORT_COUNT, HOST_IMPORT_SIGS, HOST_MODULE, ValType, Wasm64Error, Wasm64Exported,
    Wasm64Opts,
};
use blaze_lir::builder::{Param, Variadic};
use blaze_lir::func::{Conv, RefId, RefKind, SigId};
use blaze_lir::ty::{Form, Scalar, Ty};
use blaze_lir::{BlockKind, Builder as LirBuilder, Flags, Full, Func, Op, View};
use blaze_lower::head;
use blaze_own::{AVec, Alloc, Vec};
use blaze_surface::ItemKind;
use blaze_typeck::ItemKey;

use super::{BuiltLir, List, Maker, PortableTys, RecordsProgram, Typing};
use crate::body_ids::{DeclaredList, Grant, Make, RowKind, Walk};
use crate::code_records::Callee;
use crate::code_records::statics::StaticEntryView;
use crate::front2::heads::CrateCfg;
use crate::front2::keys::checker_of;
use crate::front2::source::{NamesSource as _, Records};
use crate::run::RunError;
use crate::run::special::Roots;
use crate::symbols::{encode_instance, fn_symbol_of_name, static_symbol, tls_symbol};

/// Inline memory intrinsic module recognized by `blaze_export::lir_wasm64`.
const INTRINSIC_MODULE: &str = "__blaze_wasm64";

/// Maximum compound type nesting depth handled iteratively without recursion (Rule 18).
const MAX_COMPOUND_DEPTH: usize = 64;

/// Reserve gap between defined function places (plus optional synthesized `_start`) and import
/// places so the two place namespaces stay strictly disjoint.
const IMPORT_PLACE_RESERVE: usize = 16;

/// Word encoding of `RefKind::Func` in `blaze_lir::func::RefRec::kind`.
const FUNC_REF_KIND: u32 = 0;
const _: () = assert!(matches!(RefKind::from_word(FUNC_REF_KIND), Some(RefKind::Func)));

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
    UnresolvedImport { name_hash: [u8; 32], name: [u8; 64], name_len: u8, what: &'static str },
    ImportSigMismatch { what: &'static str },
    CallSigMismatch {
        caller: u32,
        inst: usize,
        callee: u32,
        caller_name: [u8; 64],
        caller_name_len: u8,
        target_name: [u8; 64],
        target_name_len: u8,
        call_sig: [u8; 32],
        call_sig_len: u8,
        exp_sig: [u8; 32],
        exp_sig_len: u8,
    },
    Export(LirWasm64Error),
}

fn unresolved_import(c_name: &str, what: &'static str) -> Wasm64BuildError {
    let mut name = [0u8; 64];
    let bytes = c_name.as_bytes();
    let len = bytes.len().min(64);
    name[..len].copy_from_slice(&bytes[..len]);
    Wasm64BuildError::UnresolvedImport {
        name_hash: Symbol::of_name(c_name).0,
        name,
        name_len: len as u8,
        what,
    }
}

fn unresolved_import_hash(name_hash: [u8; 32], c_name: Option<&str>, what: &'static str) -> Wasm64BuildError {
    let mut name = [0u8; 64];
    let mut name_len = 0u8;
    if let Some(s) = c_name {
        let bytes = s.as_bytes();
        let len = bytes.len().min(64);
        name[..len].copy_from_slice(&bytes[..len]);
        name_len = len as u8;
    }
    Wasm64BuildError::UnresolvedImport { name_hash, name, name_len, what }
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
            Wasm64BuildError::UnresolvedImport { name_hash, name, name_len, what } => {
                let shown = core::str::from_utf8(&name[..usize::from(*name_len)]).unwrap_or("");
                if shown.is_empty() {
                    write!(f, "wasm64 link: {what} (name hash {}) (wasm64.rs)", Hex(name_hash))
                } else {
                    write!(f, "wasm64 link: {what} `{shown}` (name hash {}) (wasm64.rs)", Hex(name_hash))
                }
            }
            Wasm64BuildError::ImportSigMismatch { what } => {
                write!(f, "wasm64 link: import signature mismatch: {what} (wasm64.rs)")
            }
            Wasm64BuildError::CallSigMismatch {
                caller,
                inst,
                callee,
                caller_name,
                caller_name_len,
                target_name,
                target_name_len,
                call_sig,
                call_sig_len,
                exp_sig,
                exp_sig_len,
            } => {
                let cshown = core::str::from_utf8(&caller_name[..usize::from(*caller_name_len)]).unwrap_or("");
                let tshown = core::str::from_utf8(&target_name[..usize::from(*target_name_len)]).unwrap_or("");
                let csig = core::str::from_utf8(&call_sig[..usize::from(*call_sig_len)]).unwrap_or("");
                let esig = core::str::from_utf8(&exp_sig[..usize::from(*exp_sig_len)]).unwrap_or("");
                write!(
                    f,
                    "wasm64 link: call sig mismatch in fn {caller} (`{cshown}`) inst {inst} -> callee {callee} (`{tshown}`): call ({csig}) vs target ({esig}) (wasm64.rs)"
                )
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

        // blaze_seed (64-bit filesystem mutations used by `wasi_rt`)
        "abi_version" => Some(("blaze_seed", "abi_version")),
        "create_dir" => Some(("blaze_seed", "create_dir")),
        "remove_dir" => Some(("blaze_seed", "remove_dir")),
        "unlink" => Some(("blaze_seed", "unlink")),
        "rename" => Some(("blaze_seed", "rename")),
        "hard_link" => Some(("blaze_seed", "hard_link")),
        "symlink" => Some(("blaze_seed", "symlink")),
        "set_times" => Some(("blaze_seed", "set_times")),
        "fd_set_times" => Some(("blaze_seed", "fd_set_times")),
        "set_len" => Some(("blaze_seed", "set_len")),
        "sync" => Some(("blaze_seed", "sync")),
        "pread" => Some(("blaze_seed", "pread")),
        "pwrite" => Some(("blaze_seed", "pwrite")),
        "decommit" => Some(("blaze_seed", "decommit")),

        // blaze_host_v1
        "submit_wait" | "blaze_host_submit_wait" => Some((HOST_MODULE, "submit_wait")),
        "host_caps" | "blaze_host_caps" => Some((HOST_MODULE, "host_caps")),
        "host_log" | "blaze_host_log" => Some((HOST_MODULE, "host_log")),

        _ => None,
    }
}

/// One Rust-defined `#[no_mangle]` or `#[export_name = "..."]` function or static in the crate
/// graph (e.g., `wasi_rt`'s `malloc`, `write`, `getenv`, `errno`, `_start`, or `compiler_builtins`).
struct RustCExport<'a> {
    name: blaze_own::String<Alloc<'a>>,
    krate: usize,
    item: ItemKey,
    is_static: bool,
}

fn unquote_attr_string(raw: &str) -> Option<&str> {
    let trimmed = raw.trim().trim_start_matches('=').trim();
    let inner = trimmed.strip_prefix('"')?.strip_suffix('"')?;
    if inner.is_empty() || inner.contains('\\') {
        return None;
    }
    Some(inner)
}

struct ForeignStaticInfo<'a> {
    item: ItemKey,
    name: blaze_own::String<Alloc<'a>>,
    weak: bool,
}

/// Scans all crates in `records` for Rust-defined `#[no_mangle]` / `#[export_name = "..."]`
/// functions and statics so `extern "C"` calls/statics inside the program (such as `std` calling
/// `wasi_rt`) resolve internally without external linkers or `wasi-libc`.
fn collect_rust_c_exports<'a, 'n>(
    records: &'n Records<'n>,
    alloc: Alloc<'a>,
) -> (AVec<'a, RustCExport<'a>>, AVec<'a, ForeignStaticInfo<'a>>) {
    let mut exports: AVec<'a, RustCExport<'a>> = Vec::new_in(alloc);
    let mut foreign_statics: AVec<'a, ForeignStaticInfo<'a>> = Vec::new_in(alloc);
    let crate_count = records.crate_count();
    let mut items_buf: AVec<'a, blaze_id::ItemKey> = Vec::new_in(alloc);

    for krate in 0..crate_count {
        let Some(atoms) = records.cfg_atoms(krate) else { continue };
        let Some(edition) = records.edition_of(krate) else { continue };
        items_buf.clear();
        records.each_item(krate, &mut |key, _| {
            if matches!(records.kind_of_item(key), Some(ItemKind::Function | ItemKind::Static)) {
                items_buf.push(key);
            }
        });
        for &lib_key in items_buf.iter() {
            // Skip foreign declarations inside `extern { ... }` blocks; we want definitions only.
            // Record foreign statics (`extern "C" { static FOO: ...; }`) with their linkage so
            // static-to-static relocations can alias them to a Rust `#[no_mangle]` definition or
            // materialize zero when `#[linkage = "extern_weak"]` is absent.
            if records.extern_abi(lib_key).flatten().is_some() {
                if matches!(records.kind_of_item(lib_key), Some(ItemKind::Static))
                    && let Some(sym_name) = records.name_of(lib_key)
                {
                    let mut weak = false;
                    let mut owned = blaze_own::String::new_in(alloc);
                    if let Some(item_text) = records.signature_text_of(lib_key)
                        && (item_text.text.contains("linkage") || item_text.text.contains("link_name"))
                        && let Ok(sig) = blaze_lower::lower_signature(item_text.text, edition, &mut CrateCfg(atoms), alloc)
                    {
                        let v = sig.view();
                        if let Ok(attrs) = v.rows(v.list(head::ATTRS)) {
                            for &n in attrs {
                                let Ok(a) = v.attr(n) else { continue };
                                let Ok(path) = v.text_at(a.path) else { continue };
                                let path = path.trim();
                                if path == "linkage"
                                    && let Ok(args_text) = v.text_at(a.args)
                                    && unquote_attr_string(args_text) == Some("extern_weak")
                                {
                                    weak = true;
                                } else if path == "link_name"
                                    && let Ok(args_text) = v.text_at(a.args)
                                    && let Some(explicit) = unquote_attr_string(args_text)
                                {
                                    owned.clear();
                                    owned.push_str(explicit);
                                }
                            }
                        }
                    }
                    if owned.is_empty() {
                        owned.push_str(sym_name);
                    }
                    foreign_statics.push(ForeignStaticInfo {
                        item: checker_of(lib_key),
                        name: owned,
                        weak,
                    });
                }
                continue;
            }
            let Some(item_text) = records.signature_text_of(lib_key) else { continue };
            let text = item_text.text;
            if !(text.contains("no_mangle") || text.contains("export_name")) {
                continue;
            }
            let Ok(sig) = blaze_lower::lower_signature(text, edition, &mut CrateCfg(atoms), alloc)
            else {
                continue;
            };
            let v = sig.view();
            let Ok(attrs) = v.rows(v.list(head::ATTRS)) else { continue };
            let mut no_mangle = false;
            let mut export_name: Option<&str> = None;
            for &n in attrs {
                let Ok(a) = v.attr(n) else { continue };
                let Ok(path) = v.text_at(a.path) else { continue };
                let path = path.trim();
                if path == "no_mangle" {
                    no_mangle = true;
                } else if path == "export_name"
                    && let Ok(args_text) = v.text_at(a.args)
                    && let Some(val) = unquote_attr_string(args_text)
                {
                    export_name = Some(val);
                }
            }
            let sym_name = match (export_name, no_mangle) {
                (Some(n), _) => n,
                (None, true) => match records.name_of(lib_key) {
                    Some(n) => n,
                    None => continue,
                },
                (None, false) => continue,
            };
            let is_static = matches!(records.kind_of_item(lib_key), Some(ItemKind::Static));
            let mut owned = blaze_own::String::with_capacity_in(sym_name.len(), alloc);
            owned.push_str(sym_name);
            exports.push(RustCExport { name: owned, krate, item: checker_of(lib_key), is_static });
        }
    }
    exports.sort_unstable_by(|a, b| {
        a.name
            .as_str()
            .cmp(b.name.as_str())
            .then_with(|| a.is_static.cmp(&b.is_static))
            .then_with(|| a.krate.cmp(&b.krate))
            .then_with(|| a.item.0.cmp(&b.item.0))
    });
    exports.dedup_by(|a, b| a.name.as_str() == b.name.as_str() && a.is_static == b.is_static);
    (exports, foreign_statics)
}

fn find_rust_c_export<'a>(
    exports: &'a [RustCExport<'a>],
    c_name: &str,
    is_static: bool,
) -> Option<ItemKey> {
    let idx = exports
        .binary_search_by(|e| e.name.as_str().cmp(c_name).then_with(|| e.is_static.cmp(&is_static)))
        .ok()?;
    exports.get(idx).map(|e| e.item)
}

/// Synthesizes `__main_void() -> i32` (or `_start() -> ()` when `returns_i32 == false`) calling
/// either `entry_place(0, ...)` or `lang_start(func_addr(main_place), 0, null, 0)`.
fn synthesize_entry_wrapper<'a>(
    callee_place: u32,
    callee_view: View<'_>,
    first_arg_fn: Option<u32>,
    returns_i32: bool,
    alloc: Alloc<'a>,
) -> Result<Func<Alloc<'a>>, Wasm64BuildError> {
    let entry_sig_id = SigId(callee_view.head.sig);
    if entry_sig_id.index() >= callee_view.sigs.len() {
        return Err(Wasm64BuildError::ImportSigMismatch {
            what: "entry callee signature index out of range",
        });
    }
    let entry_sig = callee_view.sig(entry_sig_id);

    let mut b = LirBuilder::<Alloc<'a>, Full>::new_in(64, alloc);
    let wrapper_sig = if returns_i32 {
        b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I32)])
    } else {
        b.sig(Conv::C, Variadic::No, &[], &[])
    };
    b.set_signature(wrapper_sig);

    let mut call_params: AVec<'a, Param> = Vec::with_capacity_in(entry_sig.params.len(), alloc);
    for p in entry_sig.params {
        let ty = copy_ty_into_builder(callee_view, Ty(p.ty), &mut b, alloc)?;
        call_params.push(Param::of(ty));
    }
    let mut call_rets: AVec<'a, Param> = Vec::with_capacity_in(entry_sig.rets.len(), alloc);
    for r in entry_sig.rets {
        let ty = copy_ty_into_builder(callee_view, Ty(r.ty), &mut b, alloc)?;
        call_rets.push(Param::of(ty));
    }

    let call_sig = b.sig(Conv::C, Variadic::No, call_params.as_slice(), call_rets.as_slice());
    let callee_ref = b.reference(RefKind::Func, callee_place);
    let main_ref = first_arg_fn.map(|mp| b.reference(RefKind::Func, mp));
    let entry_block = b.create_block(BlockKind::Normal);
    b.begin(entry_block);

    let mut args: AVec<'a, blaze_lir::Value> = Vec::with_capacity_in(call_params.len(), alloc);
    for (idx, p) in call_params.iter().enumerate() {
        let ty = p.ty;
        let val = if idx == 0 && let Some(mref) = main_ref {
            b.func_addr(mref)
        } else if ty.is_scalar() && ty.is_int() {
            b.iconst(ty, 0)
        } else if ty == Ty::PTR {
            b.null()
        } else if ty.is_scalar() && ty.is_float() {
            b.fconst(ty, 0)
        } else {
            return Err(Wasm64BuildError::ImportSigMismatch {
                what: "entry callee has unsupported parameter type for synthesized wrapper",
            });
        };
        args.push(val);
    }

    let ret_val = b.call(Flags::NONE, callee_ref, call_sig, args.as_slice());
    if returns_i32 {
        let status = match call_rets.first().map(|r| r.ty) {
            Some(Ty::I32) => ret_val,
            Some(Ty::I64) => b.convert(Op::Trunc, Flags::NONE, Ty::I32, ret_val),
            _ => b.iconst(Ty::I32, 0),
        };
        b.ret(&[status]);
    } else {
        b.ret(&[]);
    }
    b.finish_checked().map_err(|_| Wasm64BuildError::ImportSigMismatch {
        what: "synthesized entry wrapper function failed LIR verification",
    })
}

/// Synthesizes an empty `() -> ()` no-op function (e.g. `__wasm_call_ctors` when no ctors exist).
fn synthesize_noop_void_func<'a>(alloc: Alloc<'a>) -> Result<Func<Alloc<'a>>, Wasm64BuildError> {
    let mut b = LirBuilder::<Alloc<'a>, Full>::new_in(64, alloc);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    b.ret(&[]);
    b.finish_checked().map_err(|_| Wasm64BuildError::ImportSigMismatch {
        what: "synthesized no-op function failed LIR verification",
    })
}

struct OwnedFunc<'a> {
    place: u32,
    tabled: ItemKey,
    queue_idx: usize,
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

struct WorkItem<'a> {
    kind: RowKind,
    tabled: ItemKey,
    symbol: Symbol,
    name: AVec<'a, u8>,
}

fn encode_no_args_name<'a>(
    item: ItemKey,
    alloc: Alloc<'a>,
) -> Result<(Symbol, AVec<'a, u8>), Wasm64BuildError> {
    let mut name: AVec<'a, u8> = Vec::new_in(alloc);
    let ground = PortableTys::new_in(alloc);
    encode_instance(item, List::EMPTY, ground.view(), alloc, &mut name).map_err(|_| {
        Wasm64BuildError::ImportSigMismatch {
            what: "could not encode no-arg instance name for Rust C export",
        }
    })?;
    let sym = fn_symbol_of_name(name.as_slice());
    Ok((sym, name))
}

fn enqueue_unique<'a>(
    queue: &mut AVec<'a, WorkItem<'a>>,
    seen_syms: &mut AVec<'a, [u8; 32]>,
    kind: RowKind,
    tabled: ItemKey,
    symbol: Symbol,
    name_bytes: &[u8],
    alloc: Alloc<'a>,
) {
    if seen_syms.iter().any(|s| *s == symbol.0) {
        return;
    }
    seen_syms.push(symbol.0);
    let mut name: AVec<'a, u8> = Vec::with_capacity_in(name_bytes.len(), alloc);
    name.extend_from_slice(name_bytes);
    queue.push(WorkItem { kind, tabled, symbol, name });
}

/// Whole-program linker compiling every reached instance, static, thread-local, and vtable in
/// `walk` directly into a deterministic, spec-compliant `wasm64` (`memory64`) `.wasm` binary.
///
/// Automatically resolves internal Rust `#[no_mangle]` / `#[export_name]` functions and statics
/// (such as `wasi_rt`, `compiler_builtins`, and `panic_abort`) transitively on demand.
#[expect(
    clippy::too_many_arguments,
    reason = "whole-program wasm64 linker over walk, grant closure, maker, options, import resolver, output buffer, and allocator"
)]
pub fn compile_walk_to_wasm64<'a, 'n, G, T>(
    walk: &Walk<'_>,
    grant_of: impl FnMut(ItemKey) -> Option<G>,
    maker: &mut Maker<'n, G, T>,
    opts: &Wasm64BuildOpts<'_>,
    resolve_import: impl FnMut(&str) -> Option<(&'static str, &'static str)>,
    out: &mut AVec<'a, u8>,
    alloc: Alloc<'a>,
) -> Result<Wasm64BuildOut, Wasm64BuildError>
where
    G: Grant,
    T: Typing<RecordsProgram<'n>>,
{
    let mut queue: AVec<'a, WorkItem<'a>> = Vec::with_capacity_in(walk.len(), alloc);
    let mut seen_syms: AVec<'a, [u8; 32]> = Vec::with_capacity_in(walk.len(), alloc);
    for row in 0..walk.len() {
        enqueue_unique(
            &mut queue,
            &mut seen_syms,
            walk.kind(row),
            walk.tabled(row),
            walk.symbol(row),
            walk.name(row),
            alloc,
        );
    }
    compile_worklist_to_wasm64(queue, seen_syms, None, grant_of, maker, opts, resolve_import, out, alloc)
}

/// Computes the canonical no-generic-argument function symbol for `item`, matching the symbol
/// used in `Wasm64BuildOpts::exports` when compiling via [`compile_items_to_wasm64`].
pub fn fn_symbol_of_item<'a>(item: ItemKey, alloc: Alloc<'a>) -> Result<Symbol, Wasm64BuildError> {
    let (sym, _) = encode_no_args_name(item, alloc)?;
    Ok(sym)
}

/// Single-pass whole-program Wasm64 compiler starting from explicit root functions and root
/// statics (e.g., proc-macro reactors exporting `_initialize`, `blaze_proc_macro_alloc`,
/// `blaze_proc_macro_expand`, and `#[rustc_proc_macro_decls] static _DECLS`).
#[expect(
    clippy::too_many_arguments,
    reason = "single-pass wasm64 compiler over explicit root items, grant closure, maker, options, import resolver, output buffer, and allocator"
)]
pub fn compile_items_to_wasm64<'a, 'n, G, T>(
    root_fns: &[ItemKey],
    root_statics: &[ItemKey],
    grant_of: impl FnMut(ItemKey) -> Option<G>,
    maker: &mut Maker<'n, G, T>,
    opts: &Wasm64BuildOpts<'_>,
    resolve_import: impl FnMut(&str) -> Option<(&'static str, &'static str)>,
    out: &mut AVec<'a, u8>,
    alloc: Alloc<'a>,
) -> Result<Wasm64BuildOut, Wasm64BuildError>
where
    G: Grant,
    T: Typing<RecordsProgram<'n>>,
{
    let cap = root_fns.len().saturating_add(root_statics.len()).saturating_add(4);
    let mut queue: AVec<'a, WorkItem<'a>> = Vec::with_capacity_in(cap, alloc);
    let mut seen_syms: AVec<'a, [u8; 32]> = Vec::with_capacity_in(cap, alloc);
    for &item in root_fns {
        let (sym, name_bytes) = encode_no_args_name(item, alloc)?;
        enqueue_unique(&mut queue, &mut seen_syms, RowKind::Instance, item, sym, name_bytes.as_slice(), alloc);
    }
    for &item in root_statics {
        let sym = static_symbol(item);
        enqueue_unique(&mut queue, &mut seen_syms, RowKind::Static, item, sym, &[], alloc);
    }
    compile_worklist_to_wasm64(queue, seen_syms, None, grant_of, maker, opts, resolve_import, out, alloc)
}

/// Single-pass whole-program Wasm64 compiler starting directly from `roots` without running a
/// native machine-code `Walk::one` pass first.
#[expect(
    clippy::too_many_arguments,
    reason = "single-pass wasm64 compiler over roots, grant closure, maker, options, import resolver, output buffer, and allocator"
)]
pub fn compile_roots_to_wasm64<'a, 'n, G, T>(
    roots: &Roots<'_>,
    grant_of: impl FnMut(ItemKey) -> Option<G>,
    maker: &mut Maker<'n, G, T>,
    opts: &Wasm64BuildOpts<'_>,
    resolve_import: impl FnMut(&str) -> Option<(&'static str, &'static str)>,
    out: &mut AVec<'a, u8>,
    alloc: Alloc<'a>,
) -> Result<Wasm64BuildOut, Wasm64BuildError>
where
    G: Grant,
    T: Typing<RecordsProgram<'n>>,
{
    let mut queue: AVec<'a, WorkItem<'a>> = Vec::with_capacity_in(roots.len().saturating_add(4), alloc);
    let mut seen_syms: AVec<'a, [u8; 32]> = Vec::with_capacity_in(roots.len().saturating_add(4), alloc);
    for i in 0..roots.len() {
        let (tabled, name_bytes) = roots.row(i);
        let sym = fn_symbol_of_name(name_bytes);
        enqueue_unique(&mut queue, &mut seen_syms, RowKind::Instance, tabled, sym, name_bytes, alloc);
    }
    let lang_start = roots.shim().map(|s| (s.lang_start.symbol, s.main.symbol));
    compile_worklist_to_wasm64(queue, seen_syms, lang_start, grant_of, maker, opts, resolve_import, out, alloc)
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal whole-program wasm64 worklist linker"
)]
fn compile_worklist_to_wasm64<'a, 'n, G, T>(
    mut queue: AVec<'a, WorkItem<'a>>,
    mut seen_syms: AVec<'a, [u8; 32]>,
    lang_start_shim: Option<(Symbol, Symbol)>,
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
    // 0. Discover Rust-defined `#[no_mangle]` / `#[export_name]` fns and statics across all
    //    crates (e.g., `wasi_rt`, `compiler_builtins`, `panic_abort`, user crates).
    let (rust_exports, foreign_statics) = collect_rust_c_exports(maker.names.records, alloc);

    // If the program has a Rust-defined `_start` (e.g. `wasi_rt::_start`) and the caller asked
    // for a `start_symbol` without explicitly exporting `_start`, root `wasi_rt::_start` so WASI
    // environment initialization (`env::init`), `atexit` dtors, and `proc_exit` run automatically.
    let explicit_has_start = opts.exports.iter().any(|&(_, name)| name == "_start");
    let mut crt_start_sym: Option<Symbol> = None;
    if !explicit_has_start
        && opts.start_symbol.is_some()
        && let Some(start_item) = find_rust_c_export(rust_exports.as_slice(), "_start", false)
    {
        let (sym, name_bytes) = encode_no_args_name(start_item, alloc)?;
        enqueue_unique(
            &mut queue,
            &mut seen_syms,
            RowKind::Instance,
            start_item,
            sym,
            name_bytes.as_slice(),
            alloc,
        );
        crt_start_sym = Some(sym);
    }

    // 1. Single-pass transitive closure: lower every reached function to verified LIR and
    //    materialize every reached static/TLS/vtable, discovering Rust-to-Rust `extern "C"`
    //    callees and foreign statics on demand.
    let mut funcs: AVec<'a, OwnedFunc<'a>> = Vec::new_in(alloc);
    let mut func_by_sym: AVec<'a, ([u8; 32], u32)> = Vec::new_in(alloc);
    let mut static_datums: AVec<'a, OwnedDatum<'a>> = Vec::new_in(alloc);
    let mut foreign_static_alias: AVec<'a, ([u8; 32], [u8; 32])> = Vec::new_in(alloc);
    let mut entry_buf: AVec<'a, u8> = Vec::new_in(alloc);
    let mut reached_buf = DeclaredList::new_in(alloc);

    let mut head_idx = 0usize;
    while head_idx < queue.len() {
        let kind = queue[head_idx].kind;
        let tabled = queue[head_idx].tabled;
        let symbol = queue[head_idx].symbol;
        head_idx += 1;

        let Some(grant) = grant_of(tabled) else {
            return Err(Wasm64BuildError::NotRunnable { symbol: symbol.0, tabled });
        };

        match kind {
            RowKind::Instance => {
                reached_buf.clear();
                let name_slice_len = queue[head_idx - 1].name.len();
                let mut name_copy: AVec<'a, u8> = Vec::with_capacity_in(name_slice_len, alloc);
                name_copy.extend_from_slice(queue[head_idx - 1].name.as_slice());

                let built = maker
                    .build_lir_with_reach_in(&grant, tabled, name_copy.as_slice(), Some(&mut reached_buf), alloc)
                    .map_err(|error| Wasm64BuildError::BuildLir { symbol: symbol.0, error })?;

                for (vsym, vbytes) in built.vtable_data.iter() {
                    push_unique_datum(&mut static_datums, *vsym, vbytes.as_slice(), alloc);
                }

                // Enqueue direct Rust callees reached by this body.
                for r in 0..reached_buf.len() {
                    let (callee_item, callee_name) = reached_buf.row(r);
                    let callee_sym = fn_symbol_of_name(callee_name);
                    enqueue_unique(
                        &mut queue,
                        &mut seen_syms,
                        RowKind::Instance,
                        callee_item,
                        callee_sym,
                        callee_name,
                        alloc,
                    );
                }

                // Enqueue non-foreign statics and resolve foreign statics defined by Rust crates.
                for &st_item in built.statics.iter() {
                    let foreign = built.foreign_static(st_item).or_else(|| {
                        foreign_statics
                            .iter()
                            .find(|fs| fs.item == st_item)
                            .map(|fs| (fs.name.as_str(), fs.weak))
                    });
                    if let Some((c_name, _weak)) = foreign {
                        if let Some(def_item) =
                            find_rust_c_export(rust_exports.as_slice(), c_name, true)
                        {
                            let from_sym = static_symbol(st_item);
                            let def_sym = static_symbol(def_item);
                            if !foreign_static_alias.iter().any(|&(k, _)| k == from_sym.0) {
                                foreign_static_alias.push((from_sym.0, def_sym.0));
                            }
                            enqueue_unique(
                                &mut queue,
                                &mut seen_syms,
                                RowKind::Static,
                                def_item,
                                def_sym,
                                &[],
                                alloc,
                            );
                        } else if let Some(fn_item) =
                            find_rust_c_export(rust_exports.as_slice(), c_name, false)
                        {
                            let (fn_sym, fn_name) = encode_no_args_name(fn_item, alloc)?;
                            enqueue_unique(
                                &mut queue,
                                &mut seen_syms,
                                RowKind::Instance,
                                fn_item,
                                fn_sym,
                                fn_name.as_slice(),
                                alloc,
                            );
                        }
                    } else {
                        let st_sym = static_symbol(st_item);
                        enqueue_unique(
                            &mut queue,
                            &mut seen_syms,
                            RowKind::Static,
                            st_item,
                            st_sym,
                            &[],
                            alloc,
                        );
                    }
                }

                // Enqueue thread-locals.
                for &tls_item in built.tls.iter() {
                    let sym = tls_symbol(tls_item);
                    enqueue_unique(
                        &mut queue,
                        &mut seen_syms,
                        RowKind::ThreadLocal,
                        tls_item,
                        sym,
                        &[],
                        alloc,
                    );
                }

                // Enqueue Rust `#[no_mangle]` / `#[export_name]` functions called via `extern "C"`.
                for callee_idx in 0..built.funcs.len() {
                    if let Some(Callee::C(c_name)) = built.callee_at(callee_idx as u32)
                        && let Some(def_item) =
                            find_rust_c_export(rust_exports.as_slice(), c_name, false)
                    {
                        let (def_sym, def_name) = encode_no_args_name(def_item, alloc)?;
                        enqueue_unique(
                            &mut queue,
                            &mut seen_syms,
                            RowKind::Instance,
                            def_item,
                            def_sym,
                            def_name.as_slice(),
                            alloc,
                        );
                    }
                }

                let place = u32::try_from(funcs.len()).map_err(|_| {
                    Wasm64BuildError::Export(LirWasm64Error::Overflow { what: "function count" })
                })?;
                func_by_sym.push((symbol.0, place));
                funcs.push(OwnedFunc { place, tabled, queue_idx: head_idx - 1, built });
            }
            RowKind::Static | RowKind::ThreadLocal => {
                entry_buf.clear();
                let make_res = match kind {
                    RowKind::Static => Make::make_static(maker, &grant, tabled, &mut entry_buf),
                    RowKind::ThreadLocal => {
                        Make::make_thread_local(maker, &grant, tabled, &mut entry_buf)
                    }
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
                entry_view.each_vtable(
                    |vsym, vdatum| {
                        push_unique_datum(&mut static_datums, vsym, vdatum.encoded(), alloc);
                        Ok(())
                    },
                    |_| Wasm64BuildError::StaticDatum {
                        symbol: symbol.0,
                        shown: "malformed vtable carried by static entry",
                    },
                )?;
                entry_view.each_declared(
                    |d| {
                        let d_sym = fn_symbol_of_name(d.name);
                        enqueue_unique(
                            &mut queue,
                            &mut seen_syms,
                            RowKind::Instance,
                            d.tabled,
                            d_sym,
                            d.name,
                            alloc,
                        );
                        Ok(())
                    },
                    |_| Wasm64BuildError::StaticDatum {
                        symbol: symbol.0,
                        shown: "malformed declared fn in static entry",
                    },
                )?;
                let mut static_fn_exports: AVec<'a, ItemKey> = Vec::new_in(alloc);
                entry_view.each_static(|st_item| {
                    if let Some(fs) = foreign_statics.iter().find(|fs| fs.item == st_item) {
                        if let Some(def_item) =
                            find_rust_c_export(rust_exports.as_slice(), fs.name.as_str(), true)
                        {
                            let from_sym = static_symbol(st_item);
                            let def_sym = static_symbol(def_item);
                            if !foreign_static_alias.iter().any(|&(k, _)| k == from_sym.0) {
                                foreign_static_alias.push((from_sym.0, def_sym.0));
                            }
                            enqueue_unique(
                                &mut queue,
                                &mut seen_syms,
                                RowKind::Static,
                                def_item,
                                def_sym,
                                &[],
                                alloc,
                            );
                        } else if let Some(fn_item) =
                            find_rust_c_export(rust_exports.as_slice(), fs.name.as_str(), false)
                        {
                            static_fn_exports.push(fn_item);
                        }
                        return;
                    }
                    let st_sym = static_symbol(st_item);
                    enqueue_unique(
                        &mut queue,
                        &mut seen_syms,
                        RowKind::Static,
                        st_item,
                        st_sym,
                        &[],
                        alloc,
                    );
                });
                for &fn_item in static_fn_exports.iter() {
                    let (fn_sym, fn_name) = encode_no_args_name(fn_item, alloc)?;
                    enqueue_unique(
                        &mut queue,
                        &mut seen_syms,
                        RowKind::Instance,
                        fn_item,
                        fn_sym,
                        fn_name.as_slice(),
                        alloc,
                    );
                }
                // Also resolve any `Space::Outside` relocations pointing to Rust `#[no_mangle]`
                // definitions (e.g. a static holding a fn ptr to a `wasi_rt` function).
                let dview = entry_view.datum;
                for w in 0..dview.words() {
                    let word = dview.word(w);
                    if word.target == OWN_DATUM {
                        continue;
                    }
                    let t = dview.target(word.target as usize);
                    if t.space.is_outside()
                        && let Some(c_name) = dview.name(t.symbol)
                    {
                        if let Some(def_item) =
                            find_rust_c_export(rust_exports.as_slice(), c_name, false)
                        {
                            let (def_sym, def_name) = encode_no_args_name(def_item, alloc)?;
                            enqueue_unique(
                                &mut queue,
                                &mut seen_syms,
                                RowKind::Instance,
                                def_item,
                                def_sym,
                                def_name.as_slice(),
                                alloc,
                            );
                        } else if let Some(def_item) =
                            find_rust_c_export(rust_exports.as_slice(), c_name, true)
                        {
                            let def_sym = static_symbol(def_item);
                            enqueue_unique(
                                &mut queue,
                                &mut seen_syms,
                                RowKind::Static,
                                def_item,
                                def_sym,
                                &[],
                                alloc,
                            );
                        }
                    }
                }
            }
        }
    }
    func_by_sym.sort_unstable();

    // Sort static/TLS/vtable datums by symbol so `static_place` is deterministic and binary-searchable.
    static_datums.sort_unstable_by(|a, b| a.symbol.0.cmp(&b.symbol.0));
    static_datums.dedup_by(|a, b| a.symbol == b.symbol);

    // 2. Build internal C-symbol lookup tables (`rust_c_fn_to_place`, `rust_c_static_to_place`).
    let mut rust_c_fn_to_place: AVec<'a, (&str, u32)> = Vec::new_in(alloc);
    let mut rust_c_static_to_place: AVec<'a, (&str, u32)> = Vec::new_in(alloc);
    for exp in rust_exports.iter() {
        if exp.is_static {
            let st_sym = static_symbol(exp.item);
            if let Some(place) = find_static_place(static_datums.as_slice(), st_sym.0) {
                rust_c_static_to_place.push((exp.name.as_str(), place));
            }
        } else if let Ok((fn_sym, _)) = encode_no_args_name(exp.item, alloc)
            && let Some(place) = find_place_by_sym(func_by_sym.as_slice(), fn_sym.0)
        {
            rust_c_fn_to_place.push((exp.name.as_str(), place));
        }
    }

    // 3. Resolve requested function exports and build `export_name -> func_place`.
    let mut func_export_names: AVec<'a, Option<&str>> = Vec::with_capacity_in(funcs.len(), alloc);
    func_export_names.resize(funcs.len(), None);
    let mut export_name_to_place: AVec<'a, (&str, u32)> =
        Vec::with_capacity_in(opts.exports.len().saturating_add(1), alloc);
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

    if let Some(sym) = crt_start_sym
        && let Some(place) = find_place_by_sym(func_by_sym.as_slice(), sym.0)
    {
        if let Some(slot) = func_export_names.get_mut(place as usize) {
            *slot = Some("_start");
        }
        export_name_to_place.push(("_start", place));
        has_start_export = true;
    }

    // Reserve synthetic helper places right after `funcs` (strictly below `import_place_base`).
    let mut next_synth_place = u32::try_from(funcs.len()).map_err(|_| {
        Wasm64BuildError::Export(LirWasm64Error::Overflow { what: "synthetic function place" })
    })?;
    let mut synth_funcs: AVec<'a, (&'static str, Option<&'static str>, u32, Func<Alloc<'a>>)> =
        Vec::new_in(alloc);
    let mut main_void_place: Option<u32> = None;
    let mut call_ctors_place: Option<u32> = None;

    // Resolve entry callee `(callee_place, optional_main_fn_arg)` for `__main_void` / `_start`:
    // for `std` binaries (`lang_start_shim`), calls `lang_start(func_addr(main), 0, null, 0)`;
    // otherwise calls `opts.start_symbol` directly.
    let resolved_entry: Option<(u32, Option<u32>)> = if let Some((ls_sym, main_sym)) = lang_start_shim {
        let ls_place = find_place_by_sym(func_by_sym.as_slice(), ls_sym.0)
            .ok_or(Wasm64BuildError::UnresolvedSymbol { space: "func", symbol: ls_sym.0 })?;
        let main_place = find_place_by_sym(func_by_sym.as_slice(), main_sym.0)
            .ok_or(Wasm64BuildError::UnresolvedSymbol { space: "func", symbol: main_sym.0 })?;
        Some((ls_place, Some(main_place)))
    } else if let Some(entry_sym) = opts.start_symbol {
        let place = find_place_by_sym(func_by_sym.as_slice(), entry_sym.0)
            .ok_or(Wasm64BuildError::UnresolvedSymbol { space: "func", symbol: entry_sym.0 })?;
        Some((place, None))
    } else {
        None
    };

    // 4. Rewrite every function's `refs` table into unified module places and intern host imports.
    let import_place_base = u32::try_from(funcs.len().saturating_add(IMPORT_PLACE_RESERVE))
        .map_err(|_| Wasm64BuildError::Export(LirWasm64Error::Overflow { what: "import place base" }))?;
    let mut imports: AVec<'a, OwnedImport<'a>> = Vec::new_in(alloc);
    let mut new_refs: AVec<'a, (u32, u32)> = Vec::new_in(alloc);
    let mut needs_main_void = false;
    let mut needs_call_ctors = false;

    for f in funcs.iter() {
        for callee_idx in 0..f.built.funcs.len() {
            if let Some(Callee::C(c_name)) = f.built.callee_at(callee_idx as u32) {
                if c_name == "__main_void"
                    && find_export_place(export_name_to_place.as_slice(), "__main_void").is_none()
                    && find_export_place(rust_c_fn_to_place.as_slice(), "__main_void").is_none()
                {
                    needs_main_void = true;
                } else if c_name == "__wasm_call_ctors"
                    && find_export_place(export_name_to_place.as_slice(), "__wasm_call_ctors").is_none()
                    && find_export_place(rust_c_fn_to_place.as_slice(), "__wasm_call_ctors").is_none()
                {
                    needs_call_ctors = true;
                }
            }
        }
    }

    if needs_main_void && let Some((callee_place, main_arg)) = resolved_entry {
        let entry_view = funcs[callee_place as usize].built.func.view();
        let main_void_func = synthesize_entry_wrapper(callee_place, entry_view, main_arg, true, alloc)?;
        let place = next_synth_place;
        next_synth_place = next_synth_place.checked_add(1).ok_or(Wasm64BuildError::Export(
            LirWasm64Error::Overflow { what: "__main_void place" },
        ))?;
        main_void_place = Some(place);
        synth_funcs.push(("__main_void", None, place, main_void_func));
    }

    if needs_call_ctors {
        let noop_func = synthesize_noop_void_func(alloc)?;
        let place = next_synth_place;
        next_synth_place = next_synth_place.checked_add(1).ok_or(Wasm64BuildError::Export(
            LirWasm64Error::Overflow { what: "__wasm_call_ctors place" },
        ))?;
        call_ctors_place = Some(place);
        synth_funcs.push(("__wasm_call_ctors", None, place, noop_func));
    }

    // Precompute Wasm signatures for Rust `#[no_mangle]`/`#[export_name]` C functions so a host
    // import sharing a POSIX name (e.g. `blaze_seed::unlink(ptr, len) -> i32` vs `wasi_rt`'s
    // `#[no_mangle] extern "C" fn unlink(cstr) -> i32`) is not misbound across mismatched ABIs.
    let mut rust_c_fn_sigs: AVec<'a, (u32, bool, AVec<'a, ValType>, AVec<'a, ValType>)> =
        Vec::with_capacity_in(rust_c_fn_to_place.len(), alloc);
    for &(_, place) in rust_c_fn_to_place.iter() {
        if let Some(target_fn) = funcs.get(place as usize) {
            let tv = target_fn.built.func.view();
            let tsig = SigId(tv.head.sig);
            let variadic = tsig.index() < tv.sigs.len()
                && (tv.sig(tsig).bits & blaze_lir::func::SigRec::VARIADIC) != 0;
            if let Ok((tp, tr)) = flatten_sig_wasm(tv, tsig, alloc) {
                rust_c_fn_sigs.push((place, variadic, tp, tr));
            }
        }
    }

    for f in funcs.iter_mut() {
        new_refs.clear();
        let ref_len = f.built.func.refs.len();
        new_refs.reserve(ref_len);
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
                let (mapped_kind, mapped_place) = match kind {
                    RefKind::Data => (rec.kind, rec.place),
                    RefKind::Static => {
                        let Some(&item) = f.built.statics.get(rec.place as usize) else {
                            return Err(Wasm64BuildError::UnresolvedSymbol {
                                space: "static",
                                symbol: [0u8; 32],
                            });
                        };
                        let foreign = f.built.foreign_static(item).or_else(|| {
                            foreign_statics
                                .iter()
                                .find(|fs| fs.item == item)
                                .map(|fs| (fs.name.as_str(), fs.weak))
                        });
                        if let Some((c_name, weak)) = foreign {
                            if let Some(st_place) =
                                find_export_place(rust_c_static_to_place.as_slice(), c_name)
                            {
                                (rec.kind, st_place)
                            } else if let Some(fn_place) =
                                find_export_place(export_name_to_place.as_slice(), c_name)
                                    .or_else(|| find_export_place(rust_c_fn_to_place.as_slice(), c_name))
                            {
                                // Foreign static naming a Rust `#[no_mangle]`/`#[export_name]`
                                // function: rewrite the ref to `RefKind::Func` so `Op::DataAddr`
                                // and `DataRec` relocs intern the same non-null table slot as
                                // static-datum relocations.
                                (FUNC_REF_KIND, fn_place)
                            } else if weak {
                                (rec.kind, NULL_STATIC_PLACE)
                            } else {
                                return Err(unresolved_import(
                                    c_name,
                                    "unresolved foreign extern static in wasm64",
                                ));
                            }
                        } else {
                            let sym = static_symbol(item);
                            let Some(st_place) = find_static_place(static_datums.as_slice(), sym.0)
                            else {
                                return Err(Wasm64BuildError::UnresolvedSymbol {
                                    space: "static",
                                    symbol: sym.0,
                                });
                            };
                            (rec.kind, st_place)
                        }
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
                        (rec.kind, st_place)
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
                        (rec.kind, st_place)
                    }
                    RefKind::Func => {
                        let fn_place = match f.built.callee_at(rec.place) {
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
                            let ref_id = RefId(u32::try_from(ref_idx).map_err(|_| {
                                Wasm64BuildError::Export(LirWasm64Error::Overflow {
                                    what: "ref index",
                                })
                            })?);
                            let rust_c_match = find_export_place(rust_c_fn_to_place.as_slice(), c_name)
                                .filter(|&internal_place| {
                                    if resolve_import(c_name).is_none() {
                                        return true;
                                    }
                                    let Some((_, variadic, exp_p, exp_r)) =
                                        rust_c_fn_sigs.iter().find(|(p, ..)| *p == internal_place)
                                    else {
                                        return true;
                                    };
                                    if *variadic {
                                        return true;
                                    }
                                    match find_call_site_sig(view, ref_id, alloc) {
                                        Ok(Some((cp, cr))) => {
                                            cp.as_slice() == exp_p.as_slice()
                                                && cr.as_slice() == exp_r.as_slice()
                                        }
                                        _ => true,
                                    }
                                });
                            if let Some(internal_place) =
                                find_export_place(export_name_to_place.as_slice(), c_name)
                            {
                                internal_place
                            } else if let Some(internal_place) = rust_c_match {
                                internal_place
                            } else if c_name == "__main_void"
                                && let Some(place) = main_void_place
                            {
                                place
                            } else if c_name == "__wasm_call_ctors"
                                && let Some(place) = call_ctors_place
                            {
                                place
                            } else {
                                let Some((mod_name, field_name)) = resolve_import(c_name) else {
                                    return Err(unresolved_import(
                                        c_name,
                                        "unresolved C extern import",
                                    ));
                                };
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
                                        return Err(unresolved_import(
                                            c_name,
                                            "C extern referenced without a direct call signature",
                                        ));
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
                    };
                        (rec.kind, fn_place)
                    }
                };
                new_refs.push((mapped_kind, mapped_place));
            }
        }
        for (dst_rec, &(new_kind, new_place)) in f.built.func.refs.iter_mut().zip(new_refs.iter()) {
            dst_rec.kind = new_kind;
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
                        let target_sym = foreign_static_alias
                            .iter()
                            .find(|&&(from, _)| from == t.symbol.0)
                            .map_or(t.symbol.0, |&(_, to)| to);
                        let Some(st_place) =
                            find_static_place(static_datums.as_slice(), target_sym)
                        else {
                            if t.space == Space::Static
                                && let Some(fs) = foreign_statics
                                    .iter()
                                    .find(|fs| static_symbol(fs.item).0 == target_sym)
                            {
                                let reloc_target = if let Some(fn_place) =
                                    find_export_place(export_name_to_place.as_slice(), fs.name.as_str())
                                        .or_else(|| find_export_place(rust_c_fn_to_place.as_slice(), fs.name.as_str()))
                                {
                                    LirWasm64RelocTarget::Func(fn_place)
                                } else if fs.weak {
                                    LirWasm64RelocTarget::Static(NULL_STATIC_PLACE)
                                } else {
                                    return Err(Wasm64BuildError::UnresolvedSymbol {
                                        space,
                                        symbol: target_sym,
                                    });
                                };
                                relocs.push(LirWasm64StaticReloc {
                                    offset: word.at,
                                    target: reloc_target,
                                    addend: word.addend,
                                });
                                continue;
                            }
                            return Err(Wasm64BuildError::UnresolvedSymbol {
                                space,
                                symbol: target_sym,
                            });
                        };
                        LirWasm64RelocTarget::Static(st_place)
                    }
                    Space::Outside | Space::WeakOutside => {
                        let c_name = view.name(t.symbol);
                        if let Some(name) = c_name
                            && let Some(fn_place) =
                                find_export_place(export_name_to_place.as_slice(), name)
                                    .or_else(|| find_export_place(rust_c_fn_to_place.as_slice(), name))
                        {
                            LirWasm64RelocTarget::Func(fn_place)
                        } else if let Some(name) = c_name
                            && let Some(st_place) =
                                find_export_place(rust_c_static_to_place.as_slice(), name)
                        {
                            LirWasm64RelocTarget::Static(st_place)
                        } else if t.space == Space::WeakOutside {
                            LirWasm64RelocTarget::Static(NULL_STATIC_PLACE)
                        } else {
                            return Err(unresolved_import_hash(
                                t.symbol.0,
                                c_name,
                                "unresolved external symbol in static datum",
                            ));
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
        let _ = &mut bytes;
        prepared_statics.push(PreparedStatic { place: own_place, align, bytes, relocs });
    }

    // 6. Synthesize `_start() -> ()` if `resolved_entry` is set and no `"_start"` export exists.
    if !has_start_export && let Some((callee_place, main_arg)) = resolved_entry {
        let entry_view = funcs[callee_place as usize].built.func.view();
        let start_func = synthesize_entry_wrapper(callee_place, entry_view, main_arg, false, alloc)?;
        let start_place = next_synth_place;
        next_synth_place = next_synth_place.checked_add(1).ok_or(Wasm64BuildError::Export(
            LirWasm64Error::Overflow { what: "start function place" },
        ))?;
        let _ = next_synth_place;
        synth_funcs.push(("_start", Some("_start"), start_place, start_func));
    }

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
    let total_funcs = funcs.len() + synth_funcs.len();
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
    for (name, export_name, place, func) in synth_funcs.iter() {
        lir_funcs.push(LirWasm64Func {
            name,
            export_name: *export_name,
            place: *place,
            view: func.view(),
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

    let exported = compile_lir_wasm64(&module, out, alloc).map_err(|err| {
        if let LirWasm64Error::InvalidSignature { func, inst, .. } = err
            && let Some(f) = lir_funcs.get(func as usize)
            && inst < f.view.len()
        {
            let mut o = f.view.operands(inst);
            if matches!(f.view.op(inst), Op::Call | Op::Invoke | Op::TailCall) {
                let cref = o.reference();
                let sig_id = o.sig();
                let target_place = f.view.reference(cref).place;
                if let Ok((cp, cr)) = flatten_sig_wasm(f.view, sig_id, alloc) {
                    let target_sig = lir_funcs
                        .iter()
                        .find(|lf| lf.place == target_place)
                        .and_then(|lf| flatten_sig_wasm(lf.view, SigId(lf.view.head.sig), alloc).ok());
                    let shown_storage = funcs.get(target_place as usize).and_then(|of| {
                        maker.instance_shown(of.tabled, queue[of.queue_idx].name.as_slice())
                    });
                    let target_name = rust_c_fn_to_place
                        .iter()
                        .find(|&&(_, p)| p == target_place)
                        .map(|&(n, _)| n)
                        .or_else(|| {
                            imports
                                .iter()
                                .find(|imp| imp.place == target_place)
                                .map(|imp| imp.field)
                        })
                        .or_else(|| shown_storage.as_ref().and_then(|s| s.item.path()));
                    if let Some((tp, tr)) = target_sig {
                        let caller_shown = funcs.get(func as usize).and_then(|of| {
                            maker.instance_shown(of.tabled, queue[of.queue_idx].name.as_slice())
                        });
                        let caller_str = caller_shown.as_ref().and_then(|s| s.item.path()).unwrap_or("");
                        let mut cn = [0u8; 64];
                        let clen = caller_str.len().min(64);
                        cn[..clen].copy_from_slice(&caller_str.as_bytes()[..clen]);
                        let mut tn = [0u8; 64];
                        let mut tlen = 0u8;
                        if let Some(s) = target_name {
                            let b = s.as_bytes();
                            let n = b.len().min(64);
                            tn[..n].copy_from_slice(&b[..n]);
                            tlen = n as u8;
                        }
                        let encode_sig = |ps: &[ValType], rs: &[ValType]| -> ([u8; 32], u8) {
                            let mut buf = [0u8; 32];
                            let mut pos = 0usize;
                            let ch = |vt: ValType| -> u8 {
                                match vt {
                                    ValType::I32 => b'i',
                                    ValType::I64 => b'l',
                                    ValType::F32 => b'f',
                                    ValType::F64 => b'd',
                                }
                            };
                            for &p in ps {
                                if pos < 32 { buf[pos] = ch(p); pos += 1; }
                            }
                            if pos + 2 <= 32 { buf[pos] = b'-'; buf[pos + 1] = b'>'; pos += 2; }
                            for &r in rs {
                                if pos < 32 { buf[pos] = ch(r); pos += 1; }
                            }
                            (buf, pos as u8)
                        };
                        let (call_sig, call_sig_len) = encode_sig(cp.as_slice(), cr.as_slice());
                        let (exp_sig, exp_sig_len) = encode_sig(tp.as_slice(), tr.as_slice());
                        return Wasm64BuildError::CallSigMismatch {
                            caller: func,
                            inst,
                            callee: target_place,
                            caller_name: cn,
                            caller_name_len: clen as u8,
                            target_name: tn,
                            target_name_len: tlen,
                            call_sig,
                            call_sig_len,
                            exp_sig,
                            exp_sig_len,
                        };
                    }
                }
            }
        }
        Wasm64BuildError::Export(err)
    })?;

    Ok(Wasm64BuildOut {
        func_count: lir_funcs.len(),
        static_count: lir_statics.len(),
        import_count: wasm_import_count,
        exported,
    })
}
