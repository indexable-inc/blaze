//! Wasmtime 64 (`memory64`) export writer & binary spec validator for `blaze export`.
//!
//! Emits deterministic, spec-compliant WebAssembly 64-bit linear memory (`memory64`, Wasmtime
//! `config.wasm_memory64(true)`) modules targeting Blaze's batched sans-IO host ring ABI
//! (`module = "blaze_host_v1"`):
//!
//! - **Only 3 host imports** (`submit_wait(i64, i32, i32, i64) -> i32`,
//!   `host_caps(i64, i32) -> i32`, `host_log(i32, i64, i32)`): all `bstd` capabilities ride over
//!   a single zero-copy 64-bit submission/completion ring (`bstd::wasm64::Wasm64Ring`).
//! - **64-bit linear memory (`memory64`)**: limits byte `0x04` (min) or `0x05` (min + max) with
//!   64-bit LEB128 page counts (`64 KiB` pages, supporting `> 4 GiB` address spaces).
//! - **Active data segments** use `i64.const` (`0x42`) offsets (required by `memory64`).
//! - **Custom `"blaze.profile"` section** embeds the persisted `BPG1` PGO profile digest and the
//!   16-byte BLAKE3 module identity.
//!
//! Pure sans-IO, `#![no_std]`, `#![forbid(unsafe_code)]`, explicit allocator only.

use core::fmt;

use blaze_ids::Hasher;
use blaze_own::{AVec, Alloc, Vec};

/// WebAssembly magic bytes (`\0asm`) and binary version `1`.
pub const WASM_MAGIC: [u8; 4] = *b"\0asm";
pub const WASM_VERSION: [u8; 4] = [1, 0, 0, 0];

/// Canonical Wasm64 host import module name for Blaze's sans-IO ring provider.
pub const HOST_MODULE: &str = "blaze_host_v1";
/// Number of built-in host imports (`submit_wait`, `host_caps`, `host_log`).
pub const HOST_IMPORT_COUNT: u32 = 3;
/// Current sans-IO ring ABI version exported as immutable global `blaze_ring_abi_version`.
pub const RING_ABI_VERSION: i32 = 1;

/// Built-in host import signatures (type indices `0`, `1`, `2`).
pub const HOST_IMPORT_SIGS: [(&[ValType], &[ValType]); HOST_IMPORT_COUNT as usize] = [
    (&[ValType::I64, ValType::I32, ValType::I32, ValType::I64], &[ValType::I32]),
    (&[ValType::I64, ValType::I32], &[ValType::I32]),
    (&[ValType::I32, ValType::I64, ValType::I32], &[]),
];

/// WebAssembly section IDs.
const SEC_CUSTOM: u8 = 0;
const SEC_TYPE: u8 = 1;
const SEC_IMPORT: u8 = 2;
const SEC_FUNCTION: u8 = 3;
const SEC_TABLE: u8 = 4;
const SEC_MEMORY: u8 = 5;
const SEC_GLOBAL: u8 = 6;
const SEC_EXPORT: u8 = 7;
const SEC_ELEMENT: u8 = 9;
const SEC_CODE: u8 = 10;
const SEC_DATA: u8 = 11;

/// `funcref` reference type byte for the indirect-call table.
const FUNCREF: u8 = 0x70;

/// WebAssembly value types.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ValType {
    I32 = 0x7F,
    I64 = 0x7E,
    F32 = 0x7D,
    F64 = 0x7C,
}

/// Memory limits flag bits for `memory64`.
const MEM64_MIN_ONLY: u8 = 0x04;
const MEM64_MIN_MAX: u8 = 0x05;

/// One Wasm64 function body in the program.
#[derive(Clone, Copy, Debug)]
pub struct Wasm64Func<'a> {
    /// Export name if exported (e.g. `"_start"` or user export), or `None` for internal functions.
    pub export_name: Option<&'a str>,
    pub params: &'a [ValType],
    pub results: &'a [ValType],
    /// Local declarations as `(count, ValType)` pairs.
    pub locals: &'a [(u32, ValType)],
    /// Raw WebAssembly instructions (WITHOUT trailing `0x0B` `end` opcode — the emitter appends it).
    pub body: &'a [u8],
}

/// One initialized static/rodata segment placed at a 64-bit linear memory address.
#[derive(Clone, Copy, Debug)]
pub struct Wasm64Data<'a> {
    /// 64-bit linear memory offset (`i64.const`).
    pub offset: u64,
    pub bytes: &'a [u8],
}

/// Memory and ABI configuration for a Wasmtime 64 export.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Wasm64Opts {
    /// Initial linear memory size in 64 KiB pages (default: `16` = 1 MiB).
    pub min_pages: u64,
    /// Optional maximum linear memory size in 64 KiB pages (e.g. `131_072` = 8 GiB, `> 4 GiB`!).
    pub max_pages: Option<u64>,
    /// Initial 64-bit stack pointer (`$__stack_pointer`, grows downward).
    pub stack_top: u64,
    /// Base 64-bit linear address of the guest heap (`$__heap_base`).
    pub heap_base: u64,
}

impl Wasm64Opts {
    /// Default Wasmtime 64 layout: 1 MiB initial memory, 8 GiB max (`memory64`), 256 KiB stack.
    pub const DEFAULT: Wasm64Opts = Wasm64Opts {
        min_pages: 16,
        max_pages: Some(131_072),
        stack_top: 256 * 1024,
        heap_base: 256 * 1024,
    };

    /// Full user-program / build-script / proc-macro layout: 16 MiB shadow stack (matching deep
    /// recursive Rust compilers and code generators such as `cranelift-isle` and `syn`),
    /// 8 GiB max (`memory64`).
    pub const USER_PROGRAM: Wasm64Opts = Wasm64Opts {
        min_pages: 256,
        max_pages: Some(131_072),
        stack_top: 16 * 1024 * 1024,
        heap_base: 16 * 1024 * 1024,
    };
}

/// One imported host function declared in the Wasm64 Import section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wasm64Import<'a> {
    pub module: &'a str,
    pub field: &'a str,
    pub params: &'a [ValType],
    pub results: &'a [ValType],
}

/// One exported global appended after the built-in globals (`$__stack_pointer`, `$__heap_base`,
/// `blaze_ring_abi_version`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wasm64ExportGlobal<'a> {
    pub name: &'a str,
    pub val_type: ValType,
    pub mutable: bool,
    pub init_i64: i64,
}

/// Complete Wasm64 module input to [`export_wasm64`].
#[derive(Clone, Copy, Debug)]
pub struct Wasm64Program<'a> {
    pub name: &'a str,
    pub funcs: &'a [Wasm64Func<'a>],
    pub data: &'a [Wasm64Data<'a>],
    /// Function indices (in Wasm index space: `0..import_count()` for imports,
    /// `import_count()..` for `funcs`) placed into indirect-call table 0 starting at slot 1
    /// (slot 0 is reserved as a null-trap entry). When empty, no Table or Element section is
    /// emitted.
    pub indirect_table: &'a [u32],
    /// Additional `(params, results)` function types appended to the Type section after the
    /// import types and `funcs.len()` module function types, so `call_indirect` can reference
    /// any signature by deterministic type index.
    pub extra_types: &'a [(&'a [ValType], &'a [ValType])],
    /// When `false`, the 3 `blaze_host_v1` imports (`submit_wait`, `host_caps`, `host_log`) are
    /// emitted at function indices `0..3` before `extra_imports`. When `true`, only
    /// `extra_imports` are emitted starting at function index `0`.
    pub omit_blaze_host_v1: bool,
    /// Additional imported host functions appended after the 3 `blaze_host_v1` imports when
    /// `!omit_blaze_host_v1`, or starting at function index `0` when `omit_blaze_host_v1`.
    pub extra_imports: &'a [Wasm64Import<'a>],
    /// Additional exported globals appended after the 3 built-in globals (`$__stack_pointer`,
    /// `$__heap_base`, `blaze_ring_abi_version`) and exported by `name`.
    pub extra_globals: &'a [Wasm64ExportGlobal<'a>],
    /// Optional persisted `BPG1` PGO profile digest embedded into custom section `"blaze.profile"`.
    pub profile_digest: Option<[u8; 32]>,
    pub opts: Wasm64Opts,
}

impl<'a> Wasm64Program<'a> {
    /// Number of built-in `blaze_host_v1` imports included in this module (`0` or `3`).
    #[inline]
    pub const fn blaze_host_import_count(&self) -> u32 {
        if self.omit_blaze_host_v1 { 0 } else { HOST_IMPORT_COUNT }
    }

    /// Total number of imported functions (`blaze_host_v1` when not omitted + `extra_imports`).
    #[inline]
    pub const fn import_count(&self) -> u32 {
        self.blaze_host_import_count() + self.extra_imports.len() as u32
    }

    /// Deterministic Type section index of `self.extra_types[extra_idx]`.
    #[inline]
    pub const fn extra_type_index(&self, extra_idx: usize) -> u32 {
        self.import_count() + self.funcs.len() as u32 + extra_idx as u32
    }

    /// Returns the first Type section index matching `(params, results)` across:
    /// 1. Built-in `blaze_host_v1` import types (`0..HOST_IMPORT_COUNT`, when `!omit_blaze_host_v1`),
    /// 2. `extra_imports` types (`blaze_host_import_count()..import_count()`),
    /// 3. Module function types (`import_count()..import_count() + funcs.len()`),
    /// 4. `extra_types` (`import_count() + funcs.len()..`).
    pub fn type_index_for(&self, params: &[ValType], results: &[ValType]) -> Option<u32> {
        let base_imports = self.blaze_host_import_count();
        if !self.omit_blaze_host_v1 {
            for (i, &(p, r)) in HOST_IMPORT_SIGS.iter().enumerate() {
                if p == params && r == results {
                    return Some(i as u32);
                }
            }
        }
        for (i, imp) in self.extra_imports.iter().enumerate() {
            if imp.params == params && imp.results == results {
                return Some(base_imports + i as u32);
            }
        }
        let import_count = self.import_count();
        for (i, f) in self.funcs.iter().enumerate() {
            if f.params == params && f.results == results {
                return Some(import_count + i as u32);
            }
        }
        for (i, &(p, r)) in self.extra_types.iter().enumerate() {
            if p == params && r == results {
                return Some(self.extra_type_index(i));
            }
        }
        None
    }
}

/// Summary returned by [`export_wasm64`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Wasm64Exported {
    pub funcs: u32,
    pub exports: u32,
    pub data_segments: u32,
    pub min_pages: u64,
    pub max_pages: Option<u64>,
    pub len: u64,
    pub identity: [u8; 16],
    pub profile: Option<[u8; 32]>,
}

/// Validation or emission error for Wasm64 modules.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wasm64Error {
    NoStartExport,
    InvalidMemoryRange { min_pages: u64, max_pages: u64 },
    InvalidTableFunc { func_idx: u32, func_count: u32 },
    DuplicateExport,
    BadMagic,
    BadVersion,
    Truncated,
    SectionOutOfOrder { prev: u8, got: u8 },
    NotMemory64 { flags: u8 },
    InvalidTable,
    InvalidElement,
    MissingHostImport,
    MissingMemoryExport,
    DataOffsetNotI64,
}

impl fmt::Display for Wasm64Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Wasm64Error::NoStartExport => write!(f, "wasm64 export: no function exports `_start`"),
            Wasm64Error::InvalidMemoryRange { min_pages, max_pages } => {
                write!(f, "wasm64 export: min_pages ({min_pages}) exceeds max_pages ({max_pages})")
            }
            Wasm64Error::InvalidTableFunc { func_idx, func_count } => {
                write!(
                    f,
                    "wasm64 export: indirect_table function index {func_idx} out of range (total funcs {func_count})"
                )
            }
            Wasm64Error::DuplicateExport => write!(f, "wasm64 export: duplicate export name"),
            Wasm64Error::BadMagic => write!(f, "wasm64 check: missing \\0asm magic"),
            Wasm64Error::BadVersion => write!(f, "wasm64 check: unsupported wasm binary version"),
            Wasm64Error::Truncated => write!(f, "wasm64 check: unexpected end of module"),
            Wasm64Error::SectionOutOfOrder { prev, got } => {
                write!(f, "wasm64 check: section {got} appeared after section {prev}")
            }
            Wasm64Error::NotMemory64 { flags } => {
                write!(f, "wasm64 check: memory limits flags {flags:#x} lack memory64 bit (0x04)")
            }
            Wasm64Error::InvalidTable => {
                write!(f, "wasm64 check: invalid indirect-call table section")
            }
            Wasm64Error::InvalidElement => {
                write!(f, "wasm64 check: invalid element section (expected active i32.const offset)")
            }
            Wasm64Error::MissingHostImport => {
                write!(f, "wasm64 check: missing required `{HOST_MODULE}` sans-IO ring import")
            }
            Wasm64Error::MissingMemoryExport => write!(f, "wasm64 check: missing `memory` export"),
            Wasm64Error::DataOffsetNotI64 => {
                write!(f, "wasm64 check: active data segment offset must use i64.const (0x42)")
            }
        }
    }
}

impl core::error::Error for Wasm64Error {}

/// Verified structural facts read back by [`check_wasm64`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Wasm64Check {
    pub memory64: bool,
    pub min_pages: u64,
    pub max_pages: Option<u64>,
    pub host_imports: u32,
    pub import_count: u32,
    pub has_blaze_host_v1: bool,
    pub func_count: u32,
    pub global_count: u32,
    pub table_size: u32,
    pub elem_count: u32,
    pub export_count: u32,
    pub data_segments: u32,
    pub has_memory_export: bool,
    pub has_start_export: bool,
    pub has_profile_section: bool,
}

// ---- LEB128 helpers -------------------------------------------------------------------------

#[inline]
pub(crate) fn push_u32_leb(out: &mut AVec<'_, u8>, mut v: u32) {
    loop {
        let mut byte = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

#[inline]
pub(crate) fn push_u64_leb(out: &mut AVec<'_, u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

#[inline]
pub(crate) fn push_i32_leb(out: &mut AVec<'_, u8>, mut v: i32) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        let done = (v == 0 && (byte & 0x40) == 0) || (v == -1 && (byte & 0x40) != 0);
        out.push(if done { byte } else { byte | 0x80 });
        if done {
            break;
        }
    }
}

#[inline]
pub(crate) fn push_i64_leb(out: &mut AVec<'_, u8>, mut v: i64) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        let done = (v == 0 && (byte & 0x40) == 0) || (v == -1 && (byte & 0x40) != 0);
        out.push(if done { byte } else { byte | 0x80 });
        if done {
            break;
        }
    }
}

#[inline]
fn push_name(out: &mut AVec<'_, u8>, name: &str) {
    push_u32_leb(out, name.len() as u32);
    out.extend_from_slice(name.as_bytes());
}

fn push_section(out: &mut AVec<'_, u8>, id: u8, payload: &[u8]) {
    out.push(id);
    push_u32_leb(out, payload.len() as u32);
    out.extend_from_slice(payload);
}

fn push_functype(out: &mut AVec<'_, u8>, params: &[ValType], results: &[ValType]) {
    out.push(0x60);
    push_u32_leb(out, params.len() as u32);
    for &p in params {
        out.push(p as u8);
    }
    push_u32_leb(out, results.len() as u32);
    for &r in results {
        out.push(r as u8);
    }
}

/// Emits a complete, deterministic Wasmtime 64 (`memory64`) `.wasm` module into `out`.
pub fn export_wasm64<'a>(
    program: &Wasm64Program<'_>,
    out: &mut AVec<'a, u8>,
    alloc: Alloc<'a>,
) -> Result<Wasm64Exported, Wasm64Error> {
    if let Some(max_pages) = program.opts.max_pages
        && program.opts.min_pages > max_pages
    {
        return Err(Wasm64Error::InvalidMemoryRange {
            min_pages: program.opts.min_pages,
            max_pages,
        });
    }

    let mut has_start = false;
    let mut exported_funcs = 0u32;
    for (idx, f) in program.funcs.iter().enumerate() {
        if let Some(name) = f.export_name {
            exported_funcs += 1;
            if name == "_start" {
                has_start = true;
            }
            if name == "memory" || name == "blaze_ring_abi_version" {
                return Err(Wasm64Error::DuplicateExport);
            }
            for prev in &program.funcs[..idx] {
                if prev.export_name == Some(name) {
                    return Err(Wasm64Error::DuplicateExport);
                }
            }
        }
    }
    for (idx, g) in program.extra_globals.iter().enumerate() {
        if g.name == "memory" || g.name == "blaze_ring_abi_version" {
            return Err(Wasm64Error::DuplicateExport);
        }
        for f in program.funcs {
            if f.export_name == Some(g.name) {
                return Err(Wasm64Error::DuplicateExport);
            }
        }
        for prev in &program.extra_globals[..idx] {
            if prev.name == g.name {
                return Err(Wasm64Error::DuplicateExport);
            }
        }
    }
    if program.omit_blaze_host_v1 {
        if exported_funcs == 0 {
            return Err(Wasm64Error::NoStartExport);
        }
    } else if !has_start {
        return Err(Wasm64Error::NoStartExport);
    }

    let base_imports = program.blaze_host_import_count();
    let import_count = program.import_count();
    let total_funcs = import_count + program.funcs.len() as u32;
    for &func_idx in program.indirect_table {
        if func_idx >= total_funcs {
            return Err(Wasm64Error::InvalidTableFunc { func_idx, func_count: total_funcs });
        }
    }

    out.clear();
    out.extend_from_slice(&WASM_MAGIC);
    out.extend_from_slice(&WASM_VERSION);

    let mut sec = Vec::new_in(alloc);

    // ---- 1. Type Section -------------------------------------------------------------------
    // Deterministic order matching `Wasm64Program::type_index_for` & `extra_type_index`:
    // - When `!omit_blaze_host_v1`: types 0..3 for `submit_wait`, `host_caps`, `host_log`.
    // - Next `extra_imports.len()`: one per `extra_imports` entry.
    // - Next `funcs.len()`: one per guest function.
    // - Final `extra_types.len()`: one per `extra_types` entry (for `call_indirect`).
    sec.clear();
    let type_count =
        import_count + program.funcs.len() as u32 + program.extra_types.len() as u32;
    push_u32_leb(&mut sec, type_count);
    if !program.omit_blaze_host_v1 {
        for &(params, results) in &HOST_IMPORT_SIGS {
            push_functype(&mut sec, params, results);
        }
    }
    for imp in program.extra_imports {
        push_functype(&mut sec, imp.params, imp.results);
    }
    for f in program.funcs {
        push_functype(&mut sec, f.params, f.results);
    }
    for &(params, results) in program.extra_types {
        push_functype(&mut sec, params, results);
    }
    push_section(out, SEC_TYPE, &sec);

    // ---- 2. Import Section -----------------------------------------------------------------
    if import_count > 0 {
        sec.clear();
        push_u32_leb(&mut sec, import_count);
        if !program.omit_blaze_host_v1 {
            for (name, ty_idx) in [("submit_wait", 0u32), ("host_caps", 1u32), ("host_log", 2u32)] {
                push_name(&mut sec, HOST_MODULE);
                push_name(&mut sec, name);
                sec.push(0x00); // func import
                push_u32_leb(&mut sec, ty_idx);
            }
        }
        for (i, imp) in program.extra_imports.iter().enumerate() {
            push_name(&mut sec, imp.module);
            push_name(&mut sec, imp.field);
            sec.push(0x00); // func import
            push_u32_leb(&mut sec, base_imports + i as u32);
        }
        push_section(out, SEC_IMPORT, &sec);
    }

    // ---- 3. Function Section ---------------------------------------------------------------
    sec.clear();
    push_u32_leb(&mut sec, program.funcs.len() as u32);
    for i in 0..program.funcs.len() {
        push_u32_leb(&mut sec, import_count + i as u32);
    }
    push_section(out, SEC_FUNCTION, &sec);

    // ---- 4. Table Section (funcref, 32-bit limits; slot 0 reserved as null trap) -----------
    if !program.indirect_table.is_empty() {
        sec.clear();
        push_u32_leb(&mut sec, 1);
        sec.push(FUNCREF);
        sec.push(0x00); // table32 limits: min only (table indices remain i32 in memory64)
        push_u32_leb(&mut sec, 1 + program.indirect_table.len() as u32);
        push_section(out, SEC_TABLE, &sec);
    }

    // ---- 5. Memory Section (`memory64`: flags 0x04 or 0x05, u64 LEB128 pages) --------------
    sec.clear();
    push_u32_leb(&mut sec, 1);
    match program.opts.max_pages {
        None => {
            sec.push(MEM64_MIN_ONLY);
            push_u64_leb(&mut sec, program.opts.min_pages);
        }
        Some(max_pages) => {
            sec.push(MEM64_MIN_MAX);
            push_u64_leb(&mut sec, program.opts.min_pages);
            push_u64_leb(&mut sec, max_pages);
        }
    }
    push_section(out, SEC_MEMORY, &sec);

    // ---- 6. Global Section -----------------------------------------------------------------
    // Global 0: $__stack_pointer (mut i64)
    // Global 1: $__heap_base (const i64)
    // Global 2: $blaze_ring_abi_version (const i32)
    // Globals 3..3+G: extra exported globals (`program.extra_globals`)
    sec.clear();
    push_u32_leb(&mut sec, 3 + program.extra_globals.len() as u32);
    // Global 0: mut i64
    sec.push(ValType::I64 as u8);
    sec.push(0x01); // mut
    sec.push(0x42); // i64.const
    push_i64_leb(&mut sec, program.opts.stack_top as i64);
    sec.push(0x0B); // end
    // Global 1: const i64
    sec.push(ValType::I64 as u8);
    sec.push(0x00); // const
    sec.push(0x42); // i64.const
    push_i64_leb(&mut sec, program.opts.heap_base as i64);
    sec.push(0x0B); // end
    // Global 2: const i32
    sec.push(ValType::I32 as u8);
    sec.push(0x00); // const
    sec.push(0x41); // i32.const
    push_i32_leb(&mut sec, RING_ABI_VERSION);
    sec.push(0x0B); // end
    // Extra globals
    for g in program.extra_globals {
        sec.push(g.val_type as u8);
        sec.push(if g.mutable { 0x01 } else { 0x00 });
        match g.val_type {
            ValType::I32 => {
                sec.push(0x41); // i32.const
                push_i32_leb(&mut sec, g.init_i64 as i32);
            }
            ValType::I64 => {
                sec.push(0x42); // i64.const
                push_i64_leb(&mut sec, g.init_i64);
            }
            ValType::F32 => {
                sec.push(0x43); // f32.const
                sec.extend_from_slice(&(g.init_i64 as u32).to_le_bytes());
            }
            ValType::F64 => {
                sec.push(0x44); // f64.const
                sec.extend_from_slice(&(g.init_i64 as u64).to_le_bytes());
            }
        }
        sec.push(0x0B); // end
    }
    push_section(out, SEC_GLOBAL, &sec);

    // ---- 7. Export Section -----------------------------------------------------------------
    let export_count = 2u32 + exported_funcs + program.extra_globals.len() as u32;
    sec.clear();
    push_u32_leb(&mut sec, export_count);
    // Export memory 0
    push_name(&mut sec, "memory");
    sec.push(0x02); // memory
    push_u32_leb(&mut sec, 0);
    // Export global 2 ("blaze_ring_abi_version")
    push_name(&mut sec, "blaze_ring_abi_version");
    sec.push(0x03); // global
    push_u32_leb(&mut sec, 2);
    // Export guest functions (function indices start at `import_count`)
    for (i, f) in program.funcs.iter().enumerate() {
        if let Some(name) = f.export_name {
            push_name(&mut sec, name);
            sec.push(0x00); // func
            push_u32_leb(&mut sec, import_count + i as u32);
        }
    }
    // Export extra globals (global indices start at 3)
    for (i, g) in program.extra_globals.iter().enumerate() {
        push_name(&mut sec, g.name);
        sec.push(0x03); // global
        push_u32_leb(&mut sec, 3 + i as u32);
    }
    push_section(out, SEC_EXPORT, &sec);

    // ---- 9. Element Section (active segment at i32.const 1 populating table 0) -------------
    if !program.indirect_table.is_empty() {
        sec.clear();
        push_u32_leb(&mut sec, 1); // 1 element segment
        sec.push(0x00); // active segment, table 0, vec(funcidx)
        sec.push(0x41); // i32.const (table indices are 32-bit in memory64)
        push_i32_leb(&mut sec, 1); // slot 0 reserved as null trap
        sec.push(0x0B); // end
        push_u32_leb(&mut sec, program.indirect_table.len() as u32);
        for &func_idx in program.indirect_table {
            push_u32_leb(&mut sec, func_idx);
        }
        push_section(out, SEC_ELEMENT, &sec);
    }

    // ---- 10. Code Section ------------------------------------------------------------------
    sec.clear();
    push_u32_leb(&mut sec, program.funcs.len() as u32);
    let mut body_buf = Vec::new_in(alloc);
    for f in program.funcs {
        body_buf.clear();
        push_u32_leb(&mut body_buf, f.locals.len() as u32);
        for &(count, ty) in f.locals {
            push_u32_leb(&mut body_buf, count);
            body_buf.push(ty as u8);
        }
        body_buf.extend_from_slice(f.body);
        body_buf.push(0x0B); // end
        push_u32_leb(&mut sec, body_buf.len() as u32);
        sec.extend_from_slice(&body_buf);
    }
    push_section(out, SEC_CODE, &sec);

    // ---- 11. Data Section (active segments with `i64.const` offsets for memory64) ----------
    if !program.data.is_empty() {
        sec.clear();
        push_u32_leb(&mut sec, program.data.len() as u32);
        for seg in program.data {
            sec.push(0x00); // active segment, memory 0
            sec.push(0x42); // i64.const (required for memory64)
            push_i64_leb(&mut sec, seg.offset as i64);
            sec.push(0x0B); // end
            push_u32_leb(&mut sec, seg.bytes.len() as u32);
            sec.extend_from_slice(seg.bytes);
        }
        push_section(out, SEC_DATA, &sec);
    }

    // Compute canonical 16-byte BLAKE3 identity over the module bytes + profile digest.
    let mut hasher = Hasher::new("blaze-export wasm64 identity 1");
    hasher.update(program.name.as_bytes());
    hasher.update(out);
    if let Some(p) = program.profile_digest {
        hasher.update(&p);
    }
    let full_hash = hasher.finish().0;
    let mut identity = [0u8; 16];
    identity.copy_from_slice(&full_hash[..16]);

    // ---- 0. Custom Section `"blaze.profile"` (carries identity + PGO digest) ---------------
    sec.clear();
    push_name(&mut sec, "blaze.profile");
    sec.extend_from_slice(&identity);
    match program.profile_digest {
        Some(digest) => {
            sec.push(1);
            sec.extend_from_slice(&digest);
        }
        None => sec.push(0),
    }
    push_section(out, SEC_CUSTOM, &sec);

    Ok(Wasm64Exported {
        funcs: program.funcs.len() as u32,
        exports: export_count,
        data_segments: program.data.len() as u32,
        min_pages: program.opts.min_pages,
        max_pages: program.opts.max_pages,
        len: out.len() as u64,
        identity,
        profile: program.profile_digest,
    })
}

// ---- Spec binary validator (`check_wasm64`) ------------------------------------------------

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, at: 0 }
    }

    fn byte(&mut self) -> Result<u8, Wasm64Error> {
        let &b = self.bytes.get(self.at).ok_or(Wasm64Error::Truncated)?;
        self.at += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Wasm64Error> {
        let end = self.at.checked_add(n).ok_or(Wasm64Error::Truncated)?;
        let slice = self.bytes.get(self.at..end).ok_or(Wasm64Error::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn u32_leb(&mut self) -> Result<u32, Wasm64Error> {
        let mut out = 0u32;
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            out |= u32::from(b & 0x7F) << shift;
            if (b & 0x80) == 0 {
                return Ok(out);
            }
            shift += 7;
            if shift >= 35 {
                return Err(Wasm64Error::Truncated);
            }
        }
    }

    fn u64_leb(&mut self) -> Result<u64, Wasm64Error> {
        let mut out = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            out |= u64::from(b & 0x7F) << shift;
            if (b & 0x80) == 0 {
                return Ok(out);
            }
            shift += 7;
            if shift >= 70 {
                return Err(Wasm64Error::Truncated);
            }
        }
    }

    fn skip_sleb64(&mut self) -> Result<(), Wasm64Error> {
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            if (b & 0x80) == 0 {
                return Ok(());
            }
            shift += 7;
            if shift >= 70 {
                return Err(Wasm64Error::Truncated);
            }
        }
    }

    fn name(&mut self) -> Result<&'a str, Wasm64Error> {
        let len = self.u32_leb()? as usize;
        let raw = self.take(len)?;
        core::str::from_utf8(raw).map_err(|_| Wasm64Error::Truncated)
    }
}

/// Validates a written Wasmtime 64 (`.wasm`) binary against the WebAssembly `memory64` spec
/// for both `blaze_host_v1` modules and `omit_blaze_host_v1` (WASI-64 / reactor) modules.
pub fn check_wasm64(bytes: &[u8]) -> Result<Wasm64Check, Wasm64Error> {
    let mut r = Reader::new(bytes);
    if r.take(4)? != WASM_MAGIC {
        return Err(Wasm64Error::BadMagic);
    }
    if r.take(4)? != WASM_VERSION {
        return Err(Wasm64Error::BadVersion);
    }

    let mut last_std_sec = 0u8;
    let mut memory64 = false;
    let mut min_pages = 0u64;
    let mut max_pages = None;
    let mut host_imports = 0u32;
    let mut import_count = 0u32;
    let mut has_submit_wait = false;
    let mut has_host_caps = false;
    let mut has_host_log = false;
    let mut func_count = 0u32;
    let mut code_count = 0u32;
    let mut global_count = 0u32;
    let mut table_size = 0u32;
    let mut elem_count = 0u32;
    let mut export_count = 0u32;
    let mut exported_funcs = 0u32;
    let mut data_segments = 0u32;
    let mut has_memory_export = false;
    let mut has_start_export = false;
    let mut has_profile_section = false;

    while r.at < bytes.len() {
        let sec_id = r.byte()?;
        let sec_len = r.u32_leb()? as usize;
        let payload = r.take(sec_len)?;
        if sec_id != SEC_CUSTOM {
            if sec_id <= last_std_sec {
                return Err(Wasm64Error::SectionOutOfOrder { prev: last_std_sec, got: sec_id });
            }
            last_std_sec = sec_id;
        }
        let mut sr = Reader::new(payload);
        match sec_id {
            SEC_CUSTOM => {
                if sr.name()? == "blaze.profile" {
                    has_profile_section = true;
                }
            }
            SEC_TYPE => {
                let count = sr.u32_leb()?;
                for _ in 0..count {
                    if sr.byte()? != 0x60 {
                        return Err(Wasm64Error::Truncated);
                    }
                    let n_params = sr.u32_leb()? as usize;
                    let _ = sr.take(n_params)?;
                    let n_results = sr.u32_leb()? as usize;
                    let _ = sr.take(n_results)?;
                }
            }
            SEC_IMPORT => {
                let count = sr.u32_leb()?;
                for _ in 0..count {
                    let module = sr.name()?;
                    let field = sr.name()?;
                    let kind = sr.byte()?;
                    match kind {
                        0x00 => {
                            let _ty = sr.u32_leb()?;
                        }
                        0x01 => {
                            let _reftype = sr.byte()?;
                            let flags = sr.byte()?;
                            let _min = sr.u32_leb()?;
                            if (flags & 0x01) != 0 {
                                let _max = sr.u32_leb()?;
                            }
                        }
                        0x02 => {
                            let flags = sr.byte()?;
                            if (flags & MEM64_MIN_ONLY) != 0 {
                                let _min = sr.u64_leb()?;
                                if (flags & 0x01) != 0 {
                                    let _max = sr.u64_leb()?;
                                }
                            } else {
                                let _min = sr.u32_leb()?;
                                if (flags & 0x01) != 0 {
                                    let _max = sr.u32_leb()?;
                                }
                            }
                        }
                        0x03 => {
                            let _val_type = sr.byte()?;
                            let _mutability = sr.byte()?;
                        }
                        _ => return Err(Wasm64Error::Truncated),
                    }
                    import_count = import_count.saturating_add(1);
                    if module == HOST_MODULE && kind == 0x00 {
                        host_imports = host_imports.saturating_add(1);
                        match field {
                            "submit_wait" => has_submit_wait = true,
                            "host_caps" => has_host_caps = true,
                            "host_log" => has_host_log = true,
                            _ => {}
                        }
                    }
                }
            }
            SEC_FUNCTION => {
                func_count = sr.u32_leb()?;
                for _ in 0..func_count {
                    let _ty_idx = sr.u32_leb()?;
                }
            }
            SEC_TABLE => {
                let count = sr.u32_leb()?;
                for i in 0..count {
                    let reftype = sr.byte()?;
                    if reftype != FUNCREF && reftype != 0x6F {
                        return Err(Wasm64Error::InvalidTable);
                    }
                    let flags = sr.byte()?;
                    if (flags & 0x04) != 0 {
                        return Err(Wasm64Error::InvalidTable);
                    }
                    let min = sr.u32_leb()?;
                    if (flags & 0x01) != 0 {
                        let _max = sr.u32_leb()?;
                    }
                    if i == 0 {
                        table_size = min;
                    }
                }
            }
            SEC_MEMORY => {
                let count = sr.u32_leb()?;
                if count >= 1 {
                    let flags = sr.byte()?;
                    if (flags & MEM64_MIN_ONLY) == 0 {
                        return Err(Wasm64Error::NotMemory64 { flags });
                    }
                    memory64 = true;
                    min_pages = sr.u64_leb()?;
                    if (flags & 0x01) != 0 {
                        max_pages = Some(sr.u64_leb()?);
                    }
                }
            }
            SEC_GLOBAL => {
                global_count = sr.u32_leb()?;
                for _ in 0..global_count {
                    let _val_type = sr.byte()?;
                    let _mutability = sr.byte()?;
                    match sr.byte()? {
                        0x41 | 0x42 => sr.skip_sleb64()?,
                        0x43 => {
                            let _ = sr.take(4)?;
                        }
                        0x44 => {
                            let _ = sr.take(8)?;
                        }
                        0x23 => {
                            let _ = sr.u32_leb()?;
                        }
                        _ => return Err(Wasm64Error::Truncated),
                    }
                    if sr.byte()? != 0x0B {
                        return Err(Wasm64Error::Truncated);
                    }
                }
            }
            SEC_EXPORT => {
                export_count = sr.u32_leb()?;
                for _ in 0..export_count {
                    let name = sr.name()?;
                    let kind = sr.byte()?;
                    let _idx = sr.u32_leb()?;
                    if name == "memory" && kind == 0x02 {
                        has_memory_export = true;
                    }
                    if kind == 0x00 {
                        exported_funcs = exported_funcs.saturating_add(1);
                        if name == "_start" {
                            has_start_export = true;
                        }
                    }
                }
            }
            SEC_ELEMENT => {
                let segs = sr.u32_leb()?;
                for _ in 0..segs {
                    let mode = sr.u32_leb()?;
                    if mode == 0 {
                        if sr.byte()? != 0x41 {
                            return Err(Wasm64Error::InvalidElement);
                        }
                        sr.skip_sleb64()?;
                        if sr.byte()? != 0x0B {
                            return Err(Wasm64Error::InvalidElement);
                        }
                        let n = sr.u32_leb()?;
                        for _ in 0..n {
                            let _func_idx = sr.u32_leb()?;
                        }
                        elem_count = elem_count.saturating_add(n);
                    } else {
                        return Err(Wasm64Error::InvalidElement);
                    }
                }
            }
            SEC_CODE => {
                code_count = sr.u32_leb()?;
                for _ in 0..code_count {
                    let body_len = sr.u32_leb()? as usize;
                    let _ = sr.take(body_len)?;
                }
            }
            SEC_DATA => {
                data_segments = sr.u32_leb()?;
                for _ in 0..data_segments {
                    let mode = sr.u32_leb()?;
                    if mode == 0 {
                        let opcode = sr.byte()?;
                        if opcode != 0x42 {
                            return Err(Wasm64Error::DataOffsetNotI64);
                        }
                        sr.skip_sleb64()?;
                        if sr.byte()? != 0x0B {
                            return Err(Wasm64Error::Truncated);
                        }
                    }
                    let len = sr.u32_leb()? as usize;
                    let _ = sr.take(len)?;
                }
            }
            _ => {}
        }
    }

    if !memory64 {
        return Err(Wasm64Error::NotMemory64 { flags: 0 });
    }
    if !has_memory_export {
        return Err(Wasm64Error::MissingMemoryExport);
    }
    if exported_funcs == 0 {
        return Err(Wasm64Error::NoStartExport);
    }
    if code_count != func_count {
        return Err(Wasm64Error::Truncated);
    }

    let has_blaze_host_v1 = has_submit_wait && has_host_caps && has_host_log;

    Ok(Wasm64Check {
        memory64,
        min_pages,
        max_pages,
        host_imports,
        import_count,
        has_blaze_host_v1,
        func_count,
        global_count,
        table_size,
        elem_count,
        export_count,
        data_segments,
        has_memory_export,
        has_start_export,
        has_profile_section,
    })
}
