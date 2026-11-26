//! Direct `blaze-lir` -> WebAssembly 64-bit (`memory64`, Wasmtime 64) compiler backend.
//!
//! Compiles a module of [`blaze_lir::View`] functions, static data segments, and `blaze_host_v1`
//! ring imports directly into a deterministic, spec-compliant `.wasm` binary via
//! [`crate::wasm64::export_wasm64`].
//!
//! Design:
//! - **Zero register allocation**: every SSA value maps 1:1 to Wasm local(s)
//!   (`i1`/`i8`/`i16`/`i32` -> `i32` kept canonically zero-extended; `i64`/`ptr` -> `i64`;
//!   `f32` -> `f32`; `f64` -> `f64`; `i128` -> `(lo: i64, hi: i64)`; compounds `{T, ..}` ->
//!   flattened member locals).
//! - **General CFG support**: straight-line emission for single-block non-looping functions;
//!   `loop` + `br_table` state-machine dispatcher for arbitrary multi-block / cyclic CFGs, using
//!   the Wasm operand stack for cycle-safe parallel block-argument copies.
//! - **Shadow stack**: `$__stack_pointer` (global 0) grows downward from `opts.stack_top` above
//!   a 4 KiB null-guard page `[0..4096)`, restored before every `return` / `tail_call`.
//! - **Static & function data**: laid out aligned from `opts.stack_top.max(4096)` with
//!   `DataRec` relocations patched in place; `$__heap_base` is set to the 16-byte-aligned end.
//! - **Indirect calls**: `func_addr` interns functions into table 0 starting at slot 1 (slot 0
//!   reserved as null trap); `call_indirect` wraps/bounds-checks the 64-bit pointer to an `i32`
//!   table index and dispatches by deterministic Type section index.
//!
//! Pure sans-IO, `#![no_std]`, `#![forbid(unsafe_code)]`, explicit allocator only.

use core::fmt;

use blaze_lir::{
    AtomicRmw, Block, FloatCond, IntCond, Op, Scalar, Ty, Value, View,
    func::{DataId, RefId, RefKind, SigId, SlotId},
    op::Target,
    ty::Form,
};
use blaze_own::{AVec, Alloc, Vec};

use crate::wasm64::{
    HOST_IMPORT_COUNT, HOST_IMPORT_SIGS, HOST_MODULE, ValType, Wasm64Data, Wasm64Error,
    Wasm64Exported, Wasm64Func, Wasm64Opts, Wasm64Program, export_wasm64, push_i32_leb,
    push_i64_leb, push_u32_leb, push_u64_leb,
};

// ---- Public API -----------------------------------------------------------------------------

/// One LIR function to compile into a Wasm64 module.
#[derive(Clone, Copy, Debug)]
pub struct LirWasm64Func<'a> {
    pub name: &'a str,
    pub export_name: Option<&'a str>,
    pub place: u32,
    pub view: View<'a>,
}

/// One module-level static/rodata segment placed in 64-bit linear memory.
#[derive(Clone, Copy, Debug)]
pub struct LirWasm64Static<'a> {
    pub place: u32,
    pub align: u32,
    pub bytes: &'a [u8],
}

/// One imported host function bound to a LIR `RefKind::Func` place.
#[derive(Clone, Copy, Debug)]
pub struct LirWasm64Import<'a> {
    pub place: u32,
    pub module: &'a str,
    pub field: &'a str,
}

/// Complete LIR module input to [`compile_lir_wasm64`].
#[derive(Clone, Copy, Debug)]
pub struct LirWasm64Module<'a> {
    pub name: &'a str,
    pub funcs: &'a [LirWasm64Func<'a>],
    pub statics: &'a [LirWasm64Static<'a>],
    pub imports: &'a [LirWasm64Import<'a>],
    pub opts: Wasm64Opts,
    pub profile_digest: Option<[u8; 32]>,
}

/// Error returned when lowering or exporting a LIR module to Wasm64.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LirWasm64Error {
    Export(Wasm64Error),
    UnsupportedOp { func: u32, inst: usize, op: Op },
    UnsupportedType { func: u32, inst: usize, ty: Ty },
    UnsupportedRef { func: u32, inst: usize, kind: u32, place: u32 },
    UnresolvedFunc { func: u32, inst: usize, place: u32 },
    UnresolvedStatic { func: u32, inst: usize, place: u32 },
    UnsupportedImport { place: u32 },
    DuplicatePlace { kind: &'static str, place: u32 },
    InvalidDataReloc { func: u32, data_id: u32, offset: u32 },
    InvalidAlign { align: u32 },
    InvalidCompound { func: u32, inst: usize, index: u32 },
    EmptyFunction { func: u32 },
    InvalidPtrBits { func: u32, ptr_bits: u32 },
    InvalidSignature { func: u32, inst: usize, what: &'static str },
    Overflow { what: &'static str },
}

impl From<Wasm64Error> for LirWasm64Error {
    fn from(err: Wasm64Error) -> Self {
        LirWasm64Error::Export(err)
    }
}

impl fmt::Display for LirWasm64Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LirWasm64Error::Export(e) => write!(f, "lir_wasm64: {e}"),
            LirWasm64Error::UnsupportedOp { func, inst, op } => {
                write!(f, "lir_wasm64: func place {func} inst {inst}: unsupported op `{}`", op.name())
            }
            LirWasm64Error::UnsupportedType { func, inst, ty } => {
                write!(f, "lir_wasm64: func place {func} inst {inst}: unsupported type `{ty}`")
            }
            LirWasm64Error::UnsupportedRef { func, inst, kind, place } => {
                write!(
                    f,
                    "lir_wasm64: func place {func} inst {inst}: unsupported ref kind {kind} place {place}"
                )
            }
            LirWasm64Error::UnresolvedFunc { func, inst, place } => {
                write!(
                    f,
                    "lir_wasm64: func place {func} inst {inst}: unresolved function ref place {place}"
                )
            }
            LirWasm64Error::UnresolvedStatic { func, inst, place } => {
                write!(
                    f,
                    "lir_wasm64: func place {func} inst {inst}: unresolved static ref place {place}"
                )
            }
            LirWasm64Error::UnsupportedImport { place } => {
                write!(
                    f,
                    "lir_wasm64: import place {place} is not a supported `{HOST_MODULE}` host import"
                )
            }
            LirWasm64Error::DuplicatePlace { kind, place } => {
                write!(f, "lir_wasm64: duplicate {kind} place {place}")
            }
            LirWasm64Error::InvalidDataReloc { func, data_id, offset } => {
                write!(
                    f,
                    "lir_wasm64: func place {func} data {data_id}: invalid relocation at offset {offset}"
                )
            }
            LirWasm64Error::InvalidAlign { align } => {
                write!(f, "lir_wasm64: invalid alignment {align}")
            }
            LirWasm64Error::InvalidCompound { func, inst, index } => {
                write!(
                    f,
                    "lir_wasm64: func place {func} inst {inst}: invalid compound member index {index}"
                )
            }
            LirWasm64Error::EmptyFunction { func } => {
                write!(f, "lir_wasm64: func place {func} has no blocks")
            }
            LirWasm64Error::InvalidPtrBits { func, ptr_bits } => {
                write!(f, "lir_wasm64: func place {func} has ptr_bits {ptr_bits}, expected 64")
            }
            LirWasm64Error::InvalidSignature { func, inst, what } => {
                write!(f, "lir_wasm64: func place {func} inst {inst}: {what}")
            }
            LirWasm64Error::Overflow { what } => {
                write!(f, "lir_wasm64: arithmetic overflow computing {what}")
            }
        }
    }
}

impl core::error::Error for LirWasm64Error {}

// ---- WebAssembly opcodes --------------------------------------------------------------------

const OP_UNREACHABLE: u8 = 0x00;
const OP_BLOCK: u8 = 0x02;
const OP_LOOP: u8 = 0x03;
const OP_IF: u8 = 0x04;
const OP_ELSE: u8 = 0x05;
const OP_END: u8 = 0x0B;
const OP_BR: u8 = 0x0C;
const OP_BR_IF: u8 = 0x0D;
const OP_BR_TABLE: u8 = 0x0E;
const OP_RETURN: u8 = 0x0F;
const OP_CALL: u8 = 0x10;
const OP_CALL_INDIRECT: u8 = 0x11;

const OP_SELECT: u8 = 0x1B;

const OP_LOCAL_GET: u8 = 0x20;
const OP_LOCAL_SET: u8 = 0x21;
const OP_LOCAL_TEE: u8 = 0x22;
const OP_GLOBAL_GET: u8 = 0x23;
const OP_GLOBAL_SET: u8 = 0x24;

const OP_I32_LOAD: u8 = 0x28;
const OP_I64_LOAD: u8 = 0x29;
const OP_F32_LOAD: u8 = 0x2A;
const OP_F64_LOAD: u8 = 0x2B;
const OP_I32_LOAD8_U: u8 = 0x2D;
const OP_I32_LOAD16_U: u8 = 0x2F;
const OP_I32_STORE: u8 = 0x36;
const OP_I64_STORE: u8 = 0x37;
const OP_F32_STORE: u8 = 0x38;
const OP_F64_STORE: u8 = 0x39;
const OP_I32_STORE8: u8 = 0x3A;
const OP_I32_STORE16: u8 = 0x3B;

const OP_I32_CONST: u8 = 0x41;
const OP_I64_CONST: u8 = 0x42;
const OP_F32_CONST: u8 = 0x43;
const OP_F64_CONST: u8 = 0x44;

const OP_I32_EQZ: u8 = 0x45;
const OP_I32_EQ: u8 = 0x46;
const OP_I32_NE: u8 = 0x47;
const OP_I32_LT_S: u8 = 0x48;
const OP_I32_LT_U: u8 = 0x49;
const OP_I32_GT_S: u8 = 0x4A;
const OP_I32_GT_U: u8 = 0x4B;
const OP_I32_LE_S: u8 = 0x4C;
const OP_I32_LE_U: u8 = 0x4D;
const OP_I32_GE_S: u8 = 0x4E;
const OP_I32_GE_U: u8 = 0x4F;

const OP_I64_EQZ: u8 = 0x50;
const OP_I64_EQ: u8 = 0x51;
const OP_I64_NE: u8 = 0x52;
const OP_I64_LT_S: u8 = 0x53;
const OP_I64_LT_U: u8 = 0x54;
const OP_I64_GT_S: u8 = 0x55;
const OP_I64_GT_U: u8 = 0x56;
const OP_I64_LE_S: u8 = 0x57;
const OP_I64_LE_U: u8 = 0x58;
const OP_I64_GE_S: u8 = 0x59;
const OP_I64_GE_U: u8 = 0x5A;

const OP_F32_EQ: u8 = 0x5B;
const OP_F32_NE: u8 = 0x5C;
const OP_F32_LT: u8 = 0x5D;
const OP_F32_GT: u8 = 0x5E;
const OP_F32_LE: u8 = 0x5F;
const OP_F32_GE: u8 = 0x60;

const OP_F64_EQ: u8 = 0x61;
const OP_F64_NE: u8 = 0x62;
const OP_F64_LT: u8 = 0x63;
const OP_F64_GT: u8 = 0x64;
const OP_F64_LE: u8 = 0x65;
const OP_F64_GE: u8 = 0x66;

const OP_I32_CLZ: u8 = 0x67;
const OP_I32_CTZ: u8 = 0x68;
const OP_I32_POPCNT: u8 = 0x69;
const OP_I32_ADD: u8 = 0x6A;
const OP_I32_SUB: u8 = 0x6B;
const OP_I32_MUL: u8 = 0x6C;
const OP_I32_DIV_S: u8 = 0x6D;
const OP_I32_DIV_U: u8 = 0x6E;
const OP_I32_REM_S: u8 = 0x6F;
const OP_I32_REM_U: u8 = 0x70;
const OP_I32_AND: u8 = 0x71;
const OP_I32_OR: u8 = 0x72;
const OP_I32_XOR: u8 = 0x73;
const OP_I32_SHL: u8 = 0x74;
const OP_I32_SHR_S: u8 = 0x75;
const OP_I32_SHR_U: u8 = 0x76;
const OP_I32_ROTL: u8 = 0x77;
const OP_I32_ROTR: u8 = 0x78;

const OP_I64_CLZ: u8 = 0x79;
const OP_I64_CTZ: u8 = 0x7A;
const OP_I64_POPCNT: u8 = 0x7B;
const OP_I64_ADD: u8 = 0x7C;
const OP_I64_SUB: u8 = 0x7D;
const OP_I64_MUL: u8 = 0x7E;
const OP_I64_DIV_S: u8 = 0x7F;
const OP_I64_DIV_U: u8 = 0x80;
const OP_I64_REM_S: u8 = 0x81;
const OP_I64_REM_U: u8 = 0x82;
const OP_I64_AND: u8 = 0x83;
const OP_I64_OR: u8 = 0x84;
const OP_I64_XOR: u8 = 0x85;
const OP_I64_SHL: u8 = 0x86;
const OP_I64_SHR_S: u8 = 0x87;
const OP_I64_SHR_U: u8 = 0x88;
const OP_I64_ROTL: u8 = 0x89;
const OP_I64_ROTR: u8 = 0x8A;

const OP_F32_ABS: u8 = 0x8B;
const OP_F32_NEG: u8 = 0x8C;
const OP_F32_CEIL: u8 = 0x8D;
const OP_F32_FLOOR: u8 = 0x8E;
const OP_F32_TRUNC: u8 = 0x8F;
const OP_F32_NEAREST: u8 = 0x90;
const OP_F32_SQRT: u8 = 0x91;
const OP_F32_ADD: u8 = 0x92;
const OP_F32_SUB: u8 = 0x93;
const OP_F32_MUL: u8 = 0x94;
const OP_F32_DIV: u8 = 0x95;
const OP_F32_MIN: u8 = 0x96;
const OP_F32_MAX: u8 = 0x97;
const OP_F32_COPYSIGN: u8 = 0x98;

const OP_F64_ABS: u8 = 0x99;
const OP_F64_NEG: u8 = 0x9A;
const OP_F64_CEIL: u8 = 0x9B;
const OP_F64_FLOOR: u8 = 0x9C;
const OP_F64_TRUNC: u8 = 0x9D;
const OP_F64_NEAREST: u8 = 0x9E;
const OP_F64_SQRT: u8 = 0x9F;
const OP_F64_ADD: u8 = 0xA0;
const OP_F64_SUB: u8 = 0xA1;
const OP_F64_MUL: u8 = 0xA2;
const OP_F64_DIV: u8 = 0xA3;
const OP_F64_MIN: u8 = 0xA4;
const OP_F64_MAX: u8 = 0xA5;
const OP_F64_COPYSIGN: u8 = 0xA6;

const OP_I32_WRAP_I64: u8 = 0xA7;
const OP_I64_EXTEND_I32_S: u8 = 0xAC;
const OP_I64_EXTEND_I32_U: u8 = 0xAD;
const OP_F32_CONVERT_I32_S: u8 = 0xB2;
const OP_F32_CONVERT_I32_U: u8 = 0xB3;
const OP_F32_CONVERT_I64_S: u8 = 0xB4;
const OP_F32_CONVERT_I64_U: u8 = 0xB5;
const OP_F32_DEMOTE_F64: u8 = 0xB6;
const OP_F64_CONVERT_I32_S: u8 = 0xB7;
const OP_F64_CONVERT_I32_U: u8 = 0xB8;
const OP_F64_CONVERT_I64_S: u8 = 0xB9;
const OP_F64_CONVERT_I64_U: u8 = 0xBA;
const OP_F64_PROMOTE_F32: u8 = 0xBB;
const OP_I32_REINTERPRET_F32: u8 = 0xBC;
const OP_I64_REINTERPRET_F64: u8 = 0xBD;
const OP_F32_REINTERPRET_I32: u8 = 0xBE;
const OP_F64_REINTERPRET_I64: u8 = 0xBF;

const OP_PREFIX_FC: u8 = 0xFC;
const FC_I32_TRUNC_SAT_F32_S: u32 = 0;
const FC_I32_TRUNC_SAT_F32_U: u32 = 1;
const FC_I32_TRUNC_SAT_F64_S: u32 = 2;
const FC_I32_TRUNC_SAT_F64_U: u32 = 3;
const FC_I64_TRUNC_SAT_F32_S: u32 = 4;
const FC_I64_TRUNC_SAT_F32_U: u32 = 5;
const FC_I64_TRUNC_SAT_F64_S: u32 = 6;
const FC_I64_TRUNC_SAT_F64_U: u32 = 7;
const FC_MEMORY_COPY: u32 = 10;
const FC_MEMORY_FILL: u32 = 11;

const BLOCKTYPE_VOID: u8 = 0x40;
const GLOBAL_STACK_POINTER: u32 = 0;
const NULL_GUARD_BYTES: u64 = 4096;
const PAGE_BYTES: u64 = 65_536;
const MAX_ALIGN_LOG2: u32 = 29;

// ---- Helpers --------------------------------------------------------------------------------

#[inline]
fn align_up(value: u64, align: u64) -> Result<u64, LirWasm64Error> {
    let rem = value & (align - 1);
    if rem == 0 {
        Ok(value)
    } else {
        value.checked_add(align - rem).ok_or(LirWasm64Error::Overflow { what: "memory alignment" })
    }
}

#[inline]
fn sub32_mask(scalar: Scalar) -> Option<u32> {
    match scalar {
        Scalar::I1 => Some(0x1),
        Scalar::I8 => Some(0xFF),
        Scalar::I16 => Some(0xFFFF),
        _ => None,
    }
}

#[inline]
fn sub32_bits(scalar: Scalar) -> u32 {
    match scalar {
        Scalar::I1 => 1,
        Scalar::I8 => 8,
        Scalar::I16 => 16,
        Scalar::I32 => 32,
        _ => 0,
    }
}

#[inline]
fn sub32_smin(scalar: Scalar) -> i32 {
    match scalar {
        Scalar::I1 => -1,
        Scalar::I8 => i8::MIN as i32,
        Scalar::I16 => i16::MIN as i32,
        _ => i32::MIN,
    }
}

#[inline]
fn sub32_smax(scalar: Scalar) -> i32 {
    match scalar {
        Scalar::I1 => 0,
        Scalar::I8 => i8::MAX as i32,
        Scalar::I16 => i16::MAX as i32,
        _ => i32::MAX,
    }
}

const MAX_COMPOUND_DEPTH: usize = 64;

fn push_ty_wasm_types(
    view: View<'_>,
    ty: Ty,
    func: u32,
    inst: usize,
    dst: &mut AVec<'_, ValType>,
) -> Result<(), LirWasm64Error> {
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
                    return Err(LirWasm64Error::UnsupportedType { func, inst, ty: cur });
                }
            },
            Form::Compound => {
                let members = view.members(cur);
                if top + members.len() > MAX_COMPOUND_DEPTH {
                    return Err(LirWasm64Error::Overflow { what: "compound type nesting depth" });
                }
                for &member in members.iter().rev() {
                    stack[top] = Ty(member);
                    top += 1;
                }
            }
            Form::Vector | Form::Scalable => {
                return Err(LirWasm64Error::UnsupportedType { func, inst, ty: cur });
            }
        }
    }
    Ok(())
}

fn wasm_slot_count(view: View<'_>, ty: Ty, func: u32, inst: usize) -> Result<u16, LirWasm64Error> {
    let mut stack = [Ty::NONE; MAX_COMPOUND_DEPTH];
    let mut top = 0usize;
    stack[top] = ty;
    top += 1;
    let mut sum = 0u16;
    while top > 0 {
        top -= 1;
        let cur = stack[top];
        if cur.is_none() {
            continue;
        }
        match cur.form() {
            Form::Scalar => {
                let c = match cur.elem() {
                    Scalar::None => 0u16,
                    Scalar::I1
                    | Scalar::I8
                    | Scalar::I16
                    | Scalar::I32
                    | Scalar::I64
                    | Scalar::Ptr
                    | Scalar::F32
                    | Scalar::F64 => 1u16,
                    Scalar::I128 => 2u16,
                    Scalar::F16 | Scalar::F128 => {
                        return Err(LirWasm64Error::UnsupportedType { func, inst, ty: cur });
                    }
                };
                sum = sum.checked_add(c).ok_or(LirWasm64Error::Overflow { what: "compound slots" })?;
            }
            Form::Compound => {
                let members = view.members(cur);
                if top + members.len() > MAX_COMPOUND_DEPTH {
                    return Err(LirWasm64Error::Overflow { what: "compound type nesting depth" });
                }
                for &member in members.iter().rev() {
                    stack[top] = Ty(member);
                    top += 1;
                }
            }
            Form::Vector | Form::Scalable => {
                return Err(LirWasm64Error::UnsupportedType { func, inst, ty: cur });
            }
        }
    }
    Ok(sum)
}

fn flatten_sig<'a>(
    view: View<'_>,
    sig_id: SigId,
    func: u32,
    inst: usize,
    alloc: Alloc<'a>,
) -> Result<(AVec<'a, ValType>, AVec<'a, ValType>), LirWasm64Error> {
    if sig_id.index() >= view.sigs.len() {
        return Err(LirWasm64Error::InvalidSignature { func, inst, what: "sig index out of range" });
    }
    let sig = view.sig(sig_id);
    let mut params = Vec::new_in(alloc);
    for p in sig.params {
        push_ty_wasm_types(view, Ty(p.ty), func, inst, &mut params)?;
    }
    let mut results = Vec::new_in(alloc);
    for r in sig.rets {
        push_ty_wasm_types(view, Ty(r.ty), func, inst, &mut results)?;
    }
    Ok((params, results))
}

// ---- Module-level state ---------------------------------------------------------------------

struct ModuleCtx<'a> {
    alloc: Alloc<'a>,
    /// `(place, wasm_func_idx)` across both `module.funcs` and `module.imports`.
    func_map: AVec<'a, (u32, u32)>,
    /// `(place, linear_addr)` for `module.statics`.
    static_addrs: AVec<'a, (u32, u64)>,
    /// Per-function `datas` addresses: `func_data_addrs[func_idx][data_id]`.
    func_data_addrs: AVec<'a, AVec<'a, u64>>,
    /// Pre-flattened `(params, results)` for every `module.funcs[i]` (type indices `3..3+N`).
    func_sigs: AVec<'a, (AVec<'a, ValType>, AVec<'a, ValType>)>,
    /// Indirect-call table entries (Wasm func indices placed at table slots `1..=len`).
    indirect_table: AVec<'a, u32>,
    /// Extra `(params, results)` signatures appended to Type section for `call_indirect`.
    extra_types: AVec<'a, (AVec<'a, ValType>, AVec<'a, ValType>)>,
}

impl<'a> ModuleCtx<'a> {
    fn resolve_func(&self, func: u32, inst: usize, place: u32) -> Result<u32, LirWasm64Error> {
        for &(p, idx) in self.func_map.as_slice() {
            if p == place {
                return Ok(idx);
            }
        }
        Err(LirWasm64Error::UnresolvedFunc { func, inst, place })
    }

    fn resolve_static(&self, func: u32, inst: usize, place: u32) -> Result<u64, LirWasm64Error> {
        for &(p, addr) in self.static_addrs.as_slice() {
            if p == place {
                return Ok(addr);
            }
        }
        Err(LirWasm64Error::UnresolvedStatic { func, inst, place })
    }

    fn intern_indirect_func(&mut self, wasm_func_idx: u32) -> u32 {
        for (i, &existing) in self.indirect_table.as_slice().iter().enumerate() {
            if existing == wasm_func_idx {
                return (i + 1) as u32;
            }
        }
        self.indirect_table.push(wasm_func_idx);
        self.indirect_table.len() as u32
    }

    fn intern_type(&mut self, params: &[ValType], results: &[ValType]) -> u32 {
        for (i, &(hp, hr)) in HOST_IMPORT_SIGS.iter().enumerate() {
            if hp == params && hr == results {
                return i as u32;
            }
        }
        for (i, (fp, fr)) in self.func_sigs.as_slice().iter().enumerate() {
            if fp.as_slice() == params && fr.as_slice() == results {
                return HOST_IMPORT_COUNT + i as u32;
            }
        }
        for (i, (ep, er)) in self.extra_types.as_slice().iter().enumerate() {
            if ep.as_slice() == params && er.as_slice() == results {
                return HOST_IMPORT_COUNT + self.func_sigs.len() as u32 + i as u32;
            }
        }
        let mut p = Vec::new_in(self.alloc);
        p.extend_from_slice(params);
        let mut r = Vec::new_in(self.alloc);
        r.extend_from_slice(results);
        let idx = HOST_IMPORT_COUNT + self.func_sigs.len() as u32 + self.extra_types.len() as u32;
        self.extra_types.push((p, r));
        idx
    }

    fn expected_wasm_sig(&self, wasm_func_idx: u32) -> (&[ValType], &[ValType]) {
        if wasm_func_idx < HOST_IMPORT_COUNT {
            HOST_IMPORT_SIGS[wasm_func_idx as usize]
        } else {
            let (p, r) = &self.func_sigs[(wasm_func_idx - HOST_IMPORT_COUNT) as usize];
            (p.as_slice(), r.as_slice())
        }
    }
}

fn resolve_host_import(imp: &LirWasm64Import<'_>) -> Result<u32, LirWasm64Error> {
    if imp.module != HOST_MODULE {
        return Err(LirWasm64Error::UnsupportedImport { place: imp.place });
    }
    match imp.field {
        "submit_wait" => Ok(0),
        "host_caps" => Ok(1),
        "host_log" => Ok(2),
        _ => Err(LirWasm64Error::UnsupportedImport { place: imp.place }),
    }
}

// ---- Compiled function buffer ---------------------------------------------------------------

struct CompiledFunc<'a> {
    params: AVec<'a, ValType>,
    results: AVec<'a, ValType>,
    locals: AVec<'a, (u32, ValType)>,
    body: AVec<'a, u8>,
}

// ---- Main entry point -----------------------------------------------------------------------

/// Compiles a [`LirWasm64Module`] directly into a deterministic Wasmtime 64 (`memory64`) `.wasm`
/// binary written into `out`.
pub fn compile_lir_wasm64<'a>(
    module: &LirWasm64Module<'_>,
    out: &mut AVec<'a, u8>,
    alloc: Alloc<'a>,
) -> Result<Wasm64Exported, LirWasm64Error> {
    // 1. Validate places & build function/import index map + flatten every function's signature.
    let mut func_map = Vec::with_capacity_in(module.funcs.len() + module.imports.len(), alloc);
    let mut func_sigs = Vec::with_capacity_in(module.funcs.len(), alloc);

    for (i, f) in module.funcs.iter().enumerate() {
        if f.view.head.ptr_bits != 64 {
            return Err(LirWasm64Error::InvalidPtrBits {
                func: f.place,
                ptr_bits: f.view.head.ptr_bits,
            });
        }
        if f.view.blocks() == 0 {
            return Err(LirWasm64Error::EmptyFunction { func: f.place });
        }
        for &(existing, _) in func_map.as_slice() {
            if existing == f.place {
                return Err(LirWasm64Error::DuplicatePlace { kind: "func", place: f.place });
            }
        }
        func_map.push((f.place, HOST_IMPORT_COUNT + i as u32));
        let sig = flatten_sig(f.view, SigId(f.view.head.sig), f.place, 0, alloc)?;
        func_sigs.push(sig);
    }

    for imp in module.imports {
        let host_idx = resolve_host_import(imp)?;
        for &(existing, _) in func_map.as_slice() {
            if existing == imp.place {
                return Err(LirWasm64Error::DuplicatePlace { kind: "import", place: imp.place });
            }
        }
        func_map.push((imp.place, host_idx));
    }

    // 2. Lay out module.statics and per-function DataRec segments from data_base.
    let data_base = module.opts.stack_top.max(NULL_GUARD_BYTES);
    let mut cursor = data_base;
    let mut static_addrs = Vec::with_capacity_in(module.statics.len(), alloc);
    let mut data_bufs: AVec<'a, (u64, AVec<'a, u8>)> = Vec::new_in(alloc);

    for st in module.statics {
        for &(existing, _) in static_addrs.as_slice() {
            if existing == st.place {
                return Err(LirWasm64Error::DuplicatePlace { kind: "static", place: st.place });
            }
        }
        let raw_align = st.align.max(1);
        if raw_align > (1u32 << MAX_ALIGN_LOG2) {
            return Err(LirWasm64Error::InvalidAlign { align: st.align });
        }
        let align = (raw_align as u64).next_power_of_two();
        cursor = align_up(cursor, align)?;
        static_addrs.push((st.place, cursor));
        if !st.bytes.is_empty() {
            let mut copy = Vec::with_capacity_in(st.bytes.len(), alloc);
            copy.extend_from_slice(st.bytes);
            data_bufs.push((cursor, copy));
            cursor = cursor
                .checked_add(st.bytes.len() as u64)
                .ok_or(LirWasm64Error::Overflow { what: "static data size" })?;
        }
    }

    // Reserve indices in `data_bufs` for each function's DataRec so we can patch relocs in place.
    let mut func_data_addrs: AVec<'a, AVec<'a, u64>> = Vec::with_capacity_in(module.funcs.len(), alloc);
    let mut func_data_buf_idx: AVec<'a, AVec<'a, Option<usize>>> =
        Vec::with_capacity_in(module.funcs.len(), alloc);

    for f in module.funcs {
        let mut addrs = Vec::with_capacity_in(f.view.datas.len(), alloc);
        let mut buf_idxs = Vec::with_capacity_in(f.view.datas.len(), alloc);
        for d in 0..f.view.datas.len() {
            let rec = f.view.datas[d];
            if rec.align > MAX_ALIGN_LOG2 {
                return Err(LirWasm64Error::InvalidAlign { align: rec.align });
            }
            let align = 1u64 << rec.align;
            cursor = align_up(cursor, align)?;
            addrs.push(cursor);
            let raw = f.view.data_bytes_of(DataId(d as u32));
            if !raw.is_empty() {
                let mut copy = Vec::with_capacity_in(raw.len(), alloc);
                copy.extend_from_slice(raw);
                let idx = data_bufs.len();
                data_bufs.push((cursor, copy));
                buf_idxs.push(Some(idx));
                cursor = cursor
                    .checked_add(raw.len() as u64)
                    .ok_or(LirWasm64Error::Overflow { what: "function data size" })?;
            } else {
                buf_idxs.push(None);
            }
        }
        func_data_addrs.push(addrs);
        func_data_buf_idx.push(buf_idxs);
    }

    let heap_base = align_up(cursor, 16)?;
    let mut opts = module.opts;
    opts.heap_base = heap_base;
    let needed_pages = heap_base.div_ceil(PAGE_BYTES);
    if needed_pages > opts.min_pages {
        opts.min_pages = needed_pages;
    }

    let mut ctx = ModuleCtx {
        alloc,
        func_map,
        static_addrs,
        func_data_addrs,
        func_sigs,
        indirect_table: Vec::new_in(alloc),
        extra_types: Vec::new_in(alloc),
    };

    // 3. Patch all DataRec relocations (which may intern functions into `indirect_table`).
    for (func_idx, f) in module.funcs.iter().enumerate() {
        for d in 0..f.view.datas.len() {
            let relocs = f.view.data_relocs_of(DataId(d as u32));
            if relocs.is_empty() {
                continue;
            }
            let Some(buf_idx) = func_data_buf_idx[func_idx][d] else {
                return Err(LirWasm64Error::InvalidDataReloc {
                    func: f.place,
                    data_id: d as u32,
                    offset: relocs[0].offset,
                });
            };
            for reloc in relocs {
                let target_ref = RefId(reloc.target);
                if target_ref.index() >= f.view.refs.len() {
                    return Err(LirWasm64Error::InvalidDataReloc {
                        func: f.place,
                        data_id: d as u32,
                        offset: reloc.offset,
                    });
                }
                let rec = f.view.reference(target_ref);
                let base_addr: u64 = match RefKind::from_word(rec.kind) {
                    Some(RefKind::Data) => {
                        let data_row = rec.place as usize;
                        *ctx.func_data_addrs[func_idx].get(data_row).ok_or(
                            LirWasm64Error::InvalidDataReloc {
                                func: f.place,
                                data_id: d as u32,
                                offset: reloc.offset,
                            },
                        )?
                    }
                    Some(RefKind::Static | RefKind::Tls | RefKind::Vtable) => {
                        ctx.resolve_static(f.place, 0, rec.place)?
                    }
                    Some(RefKind::Func) => {
                        let wasm_idx = ctx.resolve_func(f.place, 0, rec.place)?;
                        u64::from(ctx.intern_indirect_func(wasm_idx))
                    }
                    None => {
                        return Err(LirWasm64Error::UnsupportedRef {
                            func: f.place,
                            inst: 0,
                            kind: rec.kind,
                            place: rec.place,
                        });
                    }
                };
                let patched = base_addr.wrapping_add(reloc.addend as u64);
                let off = reloc.offset as usize;
                let end = off.checked_add(8).ok_or(LirWasm64Error::InvalidDataReloc {
                    func: f.place,
                    data_id: d as u32,
                    offset: reloc.offset,
                })?;
                let bytes = &mut data_bufs[buf_idx].1;
                if end > bytes.len() {
                    return Err(LirWasm64Error::InvalidDataReloc {
                        func: f.place,
                        data_id: d as u32,
                        offset: reloc.offset,
                    });
                }
                bytes[off..end].copy_from_slice(&patched.to_le_bytes());
            }
        }
    }

    // 4. Pre-scan all `Op::FuncAddr` across functions so `indirect_table` is finalized before
    //    lowering any `Op::CallIndirect` (deterministic slot ordering).
    for f in module.funcs {
        for inst in 0..f.view.len() {
            if f.view.op(inst) == Op::FuncAddr {
                let ref_id = f.view.operands(inst).reference();
                if ref_id.index() >= f.view.refs.len() {
                    return Err(LirWasm64Error::UnsupportedRef {
                        func: f.place,
                        inst,
                        kind: u32::MAX,
                        place: ref_id.0,
                    });
                }
                let rec = f.view.reference(ref_id);
                if RefKind::from_word(rec.kind) != Some(RefKind::Func) {
                    return Err(LirWasm64Error::UnsupportedRef {
                        func: f.place,
                        inst,
                        kind: rec.kind,
                        place: rec.place,
                    });
                }
                let wasm_idx = ctx.resolve_func(f.place, inst, rec.place)?;
                ctx.intern_indirect_func(wasm_idx);
            }
        }
    }

    // 5. Lower every function body to Wasm64 bytecode.
    let mut compiled_funcs = Vec::with_capacity_in(module.funcs.len(), alloc);
    for (func_idx, f) in module.funcs.iter().enumerate() {
        let cf = lower_func(&mut ctx, func_idx, f)?;
        compiled_funcs.push(cf);
    }

    // 6. Assemble borrowed `Wasm64Program` and emit `.wasm` bytes via `export_wasm64`.
    let mut wasm_funcs = Vec::with_capacity_in(compiled_funcs.len(), alloc);
    for (cf, f) in compiled_funcs.as_slice().iter().zip(module.funcs) {
        wasm_funcs.push(Wasm64Func {
            export_name: f.export_name,
            params: cf.params.as_slice(),
            results: cf.results.as_slice(),
            locals: cf.locals.as_slice(),
            body: cf.body.as_slice(),
        });
    }
    let mut wasm_data = Vec::with_capacity_in(data_bufs.len(), alloc);
    for (offset, bytes) in data_bufs.as_slice() {
        wasm_data.push(Wasm64Data { offset: *offset, bytes: bytes.as_slice() });
    }
    let mut wasm_extra_types = Vec::with_capacity_in(ctx.extra_types.len(), alloc);
    for (p, r) in ctx.extra_types.as_slice() {
        wasm_extra_types.push((p.as_slice(), r.as_slice()));
    }

    let program = Wasm64Program {
        name: module.name,
        funcs: wasm_funcs.as_slice(),
        data: wasm_data.as_slice(),
        indirect_table: ctx.indirect_table.as_slice(),
        extra_types: wasm_extra_types.as_slice(),
        profile_digest: module.profile_digest,
        opts,
    };

    Ok(export_wasm64(&program, out, alloc)?)
}

// ---- Per-function lowerer -------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct ValLocals {
    first: u32,
    count: u16,
}

struct FuncEmitter<'a, 'c, 'v> {
    ctx: &'c mut ModuleCtx<'a>,
    func_idx: usize,
    place: u32,
    view: View<'v>,
    param_local_count: u32,
    declared_locals: AVec<'a, ValType>,
    val_locals: AVec<'a, ValLocals>,
    slot_offsets: AVec<'a, u64>,
    frame_size: u64,
    sp_local: Option<u32>,
    pc_local: Option<u32>,
    temps_i32: AVec<'a, u32>,
    temps_i64: AVec<'a, u32>,
    temps_f32: AVec<'a, u32>,
    temps_f64: AVec<'a, u32>,
    code: AVec<'a, u8>,
}

fn lower_func<'a, 'v>(
    ctx: &mut ModuleCtx<'a>,
    func_idx: usize,
    func: &LirWasm64Func<'v>,
) -> Result<CompiledFunc<'a>, LirWasm64Error> {
    let alloc = ctx.alloc;
    let view = func.view;
    let place = func.place;

    // Copy pre-flattened signature.
    let (sig_params, sig_results) = {
        let (p, r) = &ctx.func_sigs[func_idx];
        let mut pc = Vec::with_capacity_in(p.len(), alloc);
        pc.extend_from_slice(p.as_slice());
        let mut rc = Vec::with_capacity_in(r.len(), alloc);
        rc.extend_from_slice(r.as_slice());
        (pc, rc)
    };

    let param_local_count = sig_params.len() as u32;
    let mut declared_locals = Vec::new_in(alloc);

    // Map every SSA instruction to its Wasm local range.
    let mut val_locals = Vec::with_capacity_in(view.len(), alloc);
    val_locals.resize(view.len(), ValLocals::default());

    let entry_params = view.params(Block(0));
    let mut param_cursor = 0u32;
    for inst in entry_params.clone() {
        let ty = view.ty(Value(inst as u32));
        let count = wasm_slot_count(view, ty, place, inst)?;
        val_locals[inst] = ValLocals { first: param_cursor, count };
        param_cursor += u32::from(count);
    }
    if param_cursor != param_local_count {
        return Err(LirWasm64Error::InvalidSignature {
            func: place,
            inst: 0,
            what: "entry block params do not match function signature",
        });
    }

    let mut tmp_tys = Vec::new_in(alloc);
    for inst in 0..view.len() {
        if entry_params.contains(&inst) {
            continue;
        }
        let ty = view.ty(Value(inst as u32));
        if ty.is_none() {
            continue;
        }
        tmp_tys.clear();
        push_ty_wasm_types(view, ty, place, inst, &mut tmp_tys)?;
        let first = param_local_count + declared_locals.len() as u32;
        declared_locals.extend_from_slice(tmp_tys.as_slice());
        val_locals[inst] = ValLocals { first, count: tmp_tys.len() as u16 };
    }

    // Shadow stack slot layout.
    let mut slot_offsets = Vec::with_capacity_in(view.slots.len(), alloc);
    let mut off = 0u64;
    for slot in view.slots {
        if u32::from(slot.align) > MAX_ALIGN_LOG2 {
            return Err(LirWasm64Error::InvalidAlign { align: u32::from(slot.align) });
        }
        let align = 1u64 << slot.align;
        off = align_up(off, align)?;
        slot_offsets.push(off);
        off = off
            .checked_add(u64::from(slot.size))
            .ok_or(LirWasm64Error::Overflow { what: "stack frame" })?;
    }
    let frame_size = if view.slots.is_empty() { 0 } else { align_up(off, 16)? };

    let sp_local = if frame_size > 0 {
        let idx = param_local_count + declared_locals.len() as u32;
        declared_locals.push(ValType::I64);
        Some(idx)
    } else {
        None
    };

    // Determine whether we need the loop+br_table dispatcher.
    let block_count = view.blocks();
    let needs_dispatch = if block_count > 1 {
        true
    } else {
        let term = view.op(view.terminator(Block(0)));
        matches!(
            term,
            Op::Jump | Op::Br | Op::Switch | Op::Invoke | Op::InvokeIndirect
        )
    };

    let pc_local = if needs_dispatch {
        let idx = param_local_count + declared_locals.len() as u32;
        declared_locals.push(ValType::I32);
        Some(idx)
    } else {
        None
    };

    let mut em = FuncEmitter {
        ctx,
        func_idx,
        place,
        view,
        param_local_count,
        declared_locals,
        val_locals,
        slot_offsets,
        frame_size,
        sp_local,
        pc_local,
        temps_i32: Vec::new_in(alloc),
        temps_i64: Vec::new_in(alloc),
        temps_f32: Vec::new_in(alloc),
        temps_f64: Vec::new_in(alloc),
        code: Vec::new_in(alloc),
    };

    // Function prologue: normalize sub-32-bit entry params + allocate shadow stack frame.
    for inst in entry_params {
        let ty = view.ty(Value(inst as u32));
        if ty.is_scalar()
            && let Some(mask) = sub32_mask(ty.elem())
        {
            let loc = em.val_locals[inst].first;
            em.local_get(loc);
            em.i32_const(mask as i32);
            em.op(OP_I32_AND);
            em.local_set(loc);
        }
    }
    if let Some(sp) = em.sp_local {
        em.global_get(GLOBAL_STACK_POINTER);
        em.i64_const(em.frame_size as i64);
        em.op(OP_I64_SUB);
        em.local_tee(sp);
        em.global_set(GLOBAL_STACK_POINTER);
    }

    if !needs_dispatch {
        em.emit_block(Block(0), None)?;
    } else {
        let pc = em.pc_local.expect("pc_local allocated when needs_dispatch");
        // loop $dispatch
        em.op(OP_LOOP);
        em.code.push(BLOCKTYPE_VOID);
        // Open N nested blocks: $b_{N-1} (outermost) .. $b_0 (innermost).
        for _ in 0..block_count {
            em.op(OP_BLOCK);
            em.code.push(BLOCKTYPE_VOID);
        }
        // Dispatch on $pc.
        em.local_get(pc);
        em.op(OP_BR_TABLE);
        push_u32_leb(&mut em.code, block_count as u32);
        for b in 0..block_count {
            push_u32_leb(&mut em.code, b as u32);
        }
        // Default label (unreachable for valid $pc in 0..N).
        push_u32_leb(&mut em.code, (block_count - 1) as u32);

        for b in 0..block_count {
            em.op(OP_END); // closes $b_b
            // Inside $b_{b+1}..$b_{N-1} and $dispatch: depth to $dispatch is (N - 1 - b).
            let dispatch_depth = (block_count - 1 - b) as u32;
            em.emit_block(Block(b as u32), Some(dispatch_depth))?;
        }
        em.op(OP_UNREACHABLE);
        em.op(OP_END); // closes $dispatch
        em.op(OP_UNREACHABLE);
    }

    // Run-length encode declared locals.
    let mut rle_locals: AVec<'a, (u32, ValType)> = Vec::new_in(alloc);
    for &ty in em.declared_locals.as_slice() {
        if let Some(last) = rle_locals.last_mut()
            && last.1 == ty
        {
            last.0 += 1;
        } else {
            rle_locals.push((1, ty));
        }
    }

    Ok(CompiledFunc {
        params: sig_params,
        results: sig_results,
        locals: rle_locals,
        body: em.code,
    })
}

impl<'a, 'c, 'v> FuncEmitter<'a, 'c, 'v> {
    // ---- Low-level Wasm emission helpers ----------------------------------------------------

    #[inline]
    fn op(&mut self, byte: u8) {
        self.code.push(byte);
    }

    #[inline]
    fn fc_op(&mut self, sub: u32) {
        self.code.push(OP_PREFIX_FC);
        push_u32_leb(&mut self.code, sub);
    }

    #[inline]
    fn local_get(&mut self, idx: u32) {
        self.code.push(OP_LOCAL_GET);
        push_u32_leb(&mut self.code, idx);
    }

    #[inline]
    fn local_set(&mut self, idx: u32) {
        self.code.push(OP_LOCAL_SET);
        push_u32_leb(&mut self.code, idx);
    }

    #[inline]
    fn local_tee(&mut self, idx: u32) {
        self.code.push(OP_LOCAL_TEE);
        push_u32_leb(&mut self.code, idx);
    }

    #[inline]
    fn global_get(&mut self, idx: u32) {
        self.code.push(OP_GLOBAL_GET);
        push_u32_leb(&mut self.code, idx);
    }

    #[inline]
    fn global_set(&mut self, idx: u32) {
        self.code.push(OP_GLOBAL_SET);
        push_u32_leb(&mut self.code, idx);
    }

    #[inline]
    fn i32_const(&mut self, v: i32) {
        self.code.push(OP_I32_CONST);
        push_i32_leb(&mut self.code, v);
    }

    #[inline]
    fn i64_const(&mut self, v: i64) {
        self.code.push(OP_I64_CONST);
        push_i64_leb(&mut self.code, v);
    }

    #[inline]
    fn f32_const_bits(&mut self, bits: u32) {
        self.code.push(OP_F32_CONST);
        self.code.extend_from_slice(&bits.to_le_bytes());
    }

    #[inline]
    fn f64_const_bits(&mut self, bits: u64) {
        self.code.push(OP_F64_CONST);
        self.code.extend_from_slice(&bits.to_le_bytes());
    }

    #[inline]
    fn br(&mut self, depth: u32) {
        self.code.push(OP_BR);
        push_u32_leb(&mut self.code, depth);
    }

    #[inline]
    fn br_if(&mut self, depth: u32) {
        self.code.push(OP_BR_IF);
        push_u32_leb(&mut self.code, depth);
    }

    #[inline]
    fn memarg(&mut self, align_log2: u32, offset: u64) {
        push_u32_leb(&mut self.code, align_log2);
        push_u64_leb(&mut self.code, offset);
    }

    fn alloc_local(&mut self, ty: ValType) -> u32 {
        let idx = self.param_local_count + self.declared_locals.len() as u32;
        self.declared_locals.push(ty);
        idx
    }

    fn temp_i32(&mut self, slot: usize) -> u32 {
        while self.temps_i32.len() <= slot {
            let loc = self.alloc_local(ValType::I32);
            self.temps_i32.push(loc);
        }
        self.temps_i32[slot]
    }

    fn temp_i64(&mut self, slot: usize) -> u32 {
        while self.temps_i64.len() <= slot {
            let loc = self.alloc_local(ValType::I64);
            self.temps_i64.push(loc);
        }
        self.temps_i64[slot]
    }

    fn temp_f32(&mut self, slot: usize) -> u32 {
        while self.temps_f32.len() <= slot {
            let loc = self.alloc_local(ValType::F32);
            self.temps_f32.push(loc);
        }
        self.temps_f32[slot]
    }

    fn temp_f64(&mut self, slot: usize) -> u32 {
        while self.temps_f64.len() <= slot {
            let loc = self.alloc_local(ValType::F64);
            self.temps_f64.push(loc);
        }
        self.temps_f64[slot]
    }

    #[inline]
    fn loc(&self, v: Value) -> u32 {
        self.val_locals[v.index()].first
    }

    #[inline]
    fn loc_pair(&self, v: Value) -> (u32, u32) {
        let vl = self.val_locals[v.index()];
        (vl.first, vl.first + 1)
    }

    #[inline]
    fn push_val(&mut self, v: Value) {
        let vl = self.val_locals[v.index()];
        for i in 0..u32::from(vl.count) {
            self.local_get(vl.first + i);
        }
    }

    #[inline]
    fn pop_val(&mut self, inst: usize) {
        let vl = self.val_locals[inst];
        for i in (0..u32::from(vl.count)).rev() {
            self.local_set(vl.first + i);
        }
    }

    fn emit_epilogue(&mut self) {
        if self.frame_size > 0 {
            self.global_get(GLOBAL_STACK_POINTER);
            self.i64_const(self.frame_size as i64);
            self.op(OP_I64_ADD);
            self.global_set(GLOBAL_STACK_POINTER);
        }
    }

    /// Mask an `i32` on top of the stack to `scalar`'s width if `< 32` bits.
    #[inline]
    fn mask_sub32(&mut self, scalar: Scalar) {
        if let Some(mask) = sub32_mask(scalar) {
            self.i32_const(mask as i32);
            self.op(OP_I32_AND);
        }
    }

    /// Sign-extend a canonical (zero-extended) `scalar` on top of the stack to `i32`.
    #[inline]
    fn sext_to_i32(&mut self, scalar: Scalar) {
        let bits = sub32_bits(scalar);
        if bits > 0 && bits < 32 {
            let shift = (32 - bits) as i32;
            self.i32_const(shift);
            self.op(OP_I32_SHL);
            self.i32_const(shift);
            self.op(OP_I32_SHR_S);
        }
    }

    /// Push value `v` (any scalar int `<= 64` bits) sign-extended to `i64`.
    fn push_sext_to_i64(&mut self, v: Value) {
        let scalar = self.view.ty(v).elem();
        match scalar {
            Scalar::I1 | Scalar::I8 | Scalar::I16 => {
                self.local_get(self.loc(v));
                self.sext_to_i32(scalar);
                self.op(OP_I64_EXTEND_I32_S);
            }
            Scalar::I32 => {
                self.local_get(self.loc(v));
                self.op(OP_I64_EXTEND_I32_S);
            }
            Scalar::I64 | Scalar::Ptr => {
                self.local_get(self.loc(v));
            }
            Scalar::I128 => {
                self.local_get(self.loc(v)); // low 64 bits
            }
            _ => unreachable!(),
        }
    }

    /// Push value `v` (any scalar int `<= 64` bits) zero-extended to `i64`.
    fn push_zext_to_i64(&mut self, v: Value) {
        let scalar = self.view.ty(v).elem();
        match scalar {
            Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 => {
                self.local_get(self.loc(v));
                self.op(OP_I64_EXTEND_I32_U);
            }
            Scalar::I64 | Scalar::Ptr | Scalar::I128 => {
                self.local_get(self.loc(v));
            }
            _ => unreachable!(),
        }
    }

    /// Emit `target` jump: cycle-safe parallel copy of block arguments via operand stack,
    /// then `$pc = target.block` and `br depth`.
    fn emit_jump_target(&mut self, target: Target<'_>, dispatch_depth: u32) {
        let params = self.view.params(target.block);
        let arg_count = target.args.len();
        if arg_count == 1 {
            let arg = target.args.get(0);
            let param_inst = params.start;
            let src = self.val_locals[arg.index()];
            let dst = self.val_locals[param_inst];
            if src.first != dst.first {
                for i in 0..u32::from(src.count) {
                    self.local_get(src.first + i);
                    self.local_set(dst.first + i);
                }
            }
        } else if arg_count > 1 {
            // Push all source locals forward, pop into destination parameter locals in reverse.
            // The Wasm operand stack handles arbitrary parallel-copy cycles with zero temps.
            for arg in target.args.iter() {
                self.push_val(arg);
            }
            for param_inst in params.rev() {
                self.pop_val(param_inst);
            }
        }
        let pc = self.pc_local.expect("pc_local must exist for CFG jump");
        self.i32_const(target.block.0 as i32);
        self.local_set(pc);
        self.br(dispatch_depth);
    }

    // ---- Basic block emission ---------------------------------------------------------------

    fn emit_block(
        &mut self,
        block: Block,
        dispatch_depth: Option<u32>,
    ) -> Result<(), LirWasm64Error> {
        let range = self.view.block(block);
        for inst in range {
            self.emit_inst(inst, dispatch_depth)?;
        }
        Ok(())
    }

    fn emit_inst(
        &mut self,
        inst: usize,
        dispatch_depth: Option<u32>,
    ) -> Result<(), LirWasm64Error> {
        let op = self.view.op(inst);
        let ty = self.view.ty(Value(inst as u32));
        if ty.is_vector() {
            return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty });
        }

        match op {
            // ---- Values & Constants ----
            Op::Param | Op::SlotLive | Op::SlotDead | Op::Assume | Op::Fence => {}
            Op::Iconst => {
                let word = self.view.operands(inst).imm();
                match ty.elem() {
                    Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 => {
                        let v = match sub32_mask(ty.elem()) {
                            Some(m) => word & m,
                            None => word,
                        };
                        self.i32_const(v as i32);
                        self.local_set(self.val_locals[inst].first);
                    }
                    Scalar::I64 => {
                        self.i64_const(i64::from(word as i32));
                        self.local_set(self.val_locals[inst].first);
                    }
                    Scalar::I128 => {
                        let lo = i64::from(word as i32);
                        let hi = lo >> 63;
                        let (l_lo, l_hi) = (self.val_locals[inst].first, self.val_locals[inst].first + 1);
                        self.i64_const(lo);
                        self.local_set(l_lo);
                        self.i64_const(hi);
                        self.local_set(l_hi);
                    }
                    _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty }),
                }
            }
            Op::IconstWide => {
                let words = self.view.operands(inst).constant();
                match ty.elem() {
                    Scalar::I64 => {
                        let val = u64::from(words[0]) | (u64::from(words[1]) << 32);
                        self.i64_const(val as i64);
                        self.local_set(self.val_locals[inst].first);
                    }
                    Scalar::I128 => {
                        let lo = u64::from(words[0]) | (u64::from(words[1]) << 32);
                        let hi = u64::from(words[2]) | (u64::from(words[3]) << 32);
                        let l_lo = self.val_locals[inst].first;
                        self.i64_const(lo as i64);
                        self.local_set(l_lo);
                        self.i64_const(hi as i64);
                        self.local_set(l_lo + 1);
                    }
                    _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty }),
                }
            }
            Op::Fconst => {
                let bits = self.view.operands(inst).imm();
                match ty.elem() {
                    Scalar::F32 => {
                        self.f32_const_bits(bits);
                        self.local_set(self.val_locals[inst].first);
                    }
                    _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty }),
                }
            }
            Op::FconstWide => {
                let words = self.view.operands(inst).constant();
                match ty.elem() {
                    Scalar::F64 => {
                        let bits = u64::from(words[0]) | (u64::from(words[1]) << 32);
                        self.f64_const_bits(bits);
                        self.local_set(self.val_locals[inst].first);
                    }
                    _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty }),
                }
            }
            Op::Null => {
                self.i64_const(0);
                self.local_set(self.val_locals[inst].first);
            }
            Op::Uninit => {
                let vl = self.val_locals[inst];
                match ty.form() {
                    Form::Scalar => match ty.elem() {
                        Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 => {
                            self.i32_const(0);
                            self.local_set(vl.first);
                        }
                        Scalar::I64 | Scalar::Ptr => {
                            self.i64_const(0);
                            self.local_set(vl.first);
                        }
                        Scalar::F32 => {
                            self.f32_const_bits(0);
                            self.local_set(vl.first);
                        }
                        Scalar::F64 => {
                            self.f64_const_bits(0);
                            self.local_set(vl.first);
                        }
                        Scalar::I128 => {
                            self.i64_const(0);
                            self.local_set(vl.first);
                            self.i64_const(0);
                            self.local_set(vl.first + 1);
                        }
                        _ => {
                            return Err(LirWasm64Error::UnsupportedType {
                                func: self.place,
                                inst,
                                ty,
                            });
                        }
                    },
                    _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty }),
                }
            }

            // ---- Integer Binary Ops ----
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Udiv
            | Op::Sdiv
            | Op::Urem
            | Op::Srem
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Shl
            | Op::Lshr
            | Op::Ashr
            | Op::Rotl
            | Op::Rotr
            | Op::Umulhi
            | Op::Smulhi
            | Op::Umin
            | Op::Umax
            | Op::Smin
            | Op::Smax
            | Op::UaddSat
            | Op::SaddSat
            | Op::UsubSat
            | Op::SsubSat => {
                let mut o = self.view.operands(inst);
                let (a, b) = (o.value(), o.value());
                self.emit_int_binary(inst, op, ty, a, b)?;
            }

            // ---- Integer Overflow Ops `{T, i1}` ----
            Op::UaddOverflow
            | Op::SaddOverflow
            | Op::UsubOverflow
            | Op::SsubOverflow
            | Op::UmulOverflow
            | Op::SmulOverflow => {
                let mut o = self.view.operands(inst);
                let (a, b) = (o.value(), o.value());
                self.emit_int_overflow(inst, op, a, b)?;
            }

            // ---- Integer Unary Ops ----
            Op::Neg | Op::Not | Op::Clz | Op::Ctz | Op::Popcnt | Op::Bswap | Op::Bitrev => {
                let a = self.view.operands(inst).value();
                self.emit_int_unary(inst, op, ty, a)?;
            }

            // ---- Comparisons & Select ----
            Op::Icmp => {
                let mut o = self.view.operands(inst);
                let cond = o.int_cond();
                let (a, b) = (o.value(), o.value());
                self.emit_icmp(inst, cond, a, b)?;
            }
            Op::Fcmp => {
                let mut o = self.view.operands(inst);
                let cond = o.float_cond();
                let (a, b) = (o.value(), o.value());
                self.emit_fcmp(inst, cond, a, b)?;
            }
            Op::Select => {
                let mut o = self.view.operands(inst);
                let (cond, t_val, f_val) = (o.value(), o.value(), o.value());
                let c_loc = self.loc(cond);
                let tv = self.val_locals[t_val.index()];
                let fv = self.val_locals[f_val.index()];
                let dst = self.val_locals[inst];
                for i in 0..u32::from(dst.count) {
                    self.local_get(tv.first + i);
                    self.local_get(fv.first + i);
                    self.local_get(c_loc);
                    self.op(OP_SELECT);
                    self.local_set(dst.first + i);
                }
            }

            // ---- Conversions ----
            Op::Zext => {
                let a = self.view.operands(inst).value();
                self.emit_zext(inst, ty, a)?;
            }
            Op::Sext => {
                let a = self.view.operands(inst).value();
                self.emit_sext(inst, ty, a)?;
            }
            Op::Trunc => {
                let a = self.view.operands(inst).value();
                self.emit_trunc(inst, ty, a)?;
            }
            Op::Fpext => {
                let a = self.view.operands(inst).value();
                if self.view.ty(a) == Ty::F32 && ty == Ty::F64 {
                    self.local_get(self.loc(a));
                    self.op(OP_F64_PROMOTE_F32);
                    self.local_set(self.val_locals[inst].first);
                } else {
                    return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty });
                }
            }
            Op::Fptrunc => {
                let a = self.view.operands(inst).value();
                if self.view.ty(a) == Ty::F64 && ty == Ty::F32 {
                    self.local_get(self.loc(a));
                    self.op(OP_F32_DEMOTE_F64);
                    self.local_set(self.val_locals[inst].first);
                } else {
                    return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty });
                }
            }
            Op::FptoUi | Op::FptoSi => {
                let a = self.view.operands(inst).value();
                self.emit_fp_to_int(inst, op == Op::FptoSi, ty, a)?;
            }
            Op::UitoFp | Op::SitoFp => {
                let a = self.view.operands(inst).value();
                self.emit_int_to_fp(inst, op == Op::SitoFp, ty, a)?;
            }
            Op::Bitcast => {
                let a = self.view.operands(inst).value();
                let from_ty = self.view.ty(a);
                let dst = self.val_locals[inst].first;
                let src = self.loc(a);
                match (from_ty.elem(), ty.elem()) {
                    (Scalar::I32, Scalar::F32) => {
                        self.local_get(src);
                        self.op(OP_F32_REINTERPRET_I32);
                        self.local_set(dst);
                    }
                    (Scalar::F32, Scalar::I32) => {
                        self.local_get(src);
                        self.op(OP_I32_REINTERPRET_F32);
                        self.local_set(dst);
                    }
                    (Scalar::I64, Scalar::F64) => {
                        self.local_get(src);
                        self.op(OP_F64_REINTERPRET_I64);
                        self.local_set(dst);
                    }
                    (Scalar::F64, Scalar::I64) => {
                        self.local_get(src);
                        self.op(OP_I64_REINTERPRET_F64);
                        self.local_set(dst);
                    }
                    (x, y) if x == y => {
                        let count = self.val_locals[inst].count;
                        for i in 0..u32::from(count) {
                            self.local_get(src + i);
                            self.local_set(dst + i);
                        }
                    }
                    _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty }),
                }
            }

            // ---- Floating-point Ops ----
            Op::Fadd
            | Op::Fsub
            | Op::Fmul
            | Op::Fdiv
            | Op::Fcopysign
            | Op::FminNum
            | Op::FmaxNum
            | Op::Fminimum
            | Op::Fmaximum => {
                let mut o = self.view.operands(inst);
                let (a, b) = (o.value(), o.value());
                self.emit_float_binary(inst, op, ty, a, b)?;
            }
            Op::Fneg
            | Op::Fabs
            | Op::Sqrt
            | Op::Floor
            | Op::Ceil
            | Op::Ftrunc
            | Op::Nearest
            | Op::Fround => {
                let a = self.view.operands(inst).value();
                self.emit_float_unary(inst, op, ty, a)?;
            }
            Op::Fma | Op::Fmuladd => {
                let mut o = self.view.operands(inst);
                let (a, b, c) = (o.value(), o.value(), o.value());
                let dst = self.val_locals[inst].first;
                match ty.elem() {
                    Scalar::F32 => {
                        if op == Op::Fma {
                            // Promote to f64 so a * b has exact intermediate product.
                            self.local_get(self.loc(a));
                            self.op(OP_F64_PROMOTE_F32);
                            self.local_get(self.loc(b));
                            self.op(OP_F64_PROMOTE_F32);
                            self.op(OP_F64_MUL);
                            self.local_get(self.loc(c));
                            self.op(OP_F64_PROMOTE_F32);
                            self.op(OP_F64_ADD);
                            self.op(OP_F32_DEMOTE_F64);
                        } else {
                            self.local_get(self.loc(a));
                            self.local_get(self.loc(b));
                            self.op(OP_F32_MUL);
                            self.local_get(self.loc(c));
                            self.op(OP_F32_ADD);
                        }
                        self.local_set(dst);
                    }
                    Scalar::F64 => {
                        self.local_get(self.loc(a));
                        self.local_get(self.loc(b));
                        self.op(OP_F64_MUL);
                        self.local_get(self.loc(c));
                        self.op(OP_F64_ADD);
                        self.local_set(dst);
                    }
                    _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty }),
                }
            }

            // ---- Freeze ----
            Op::Freeze => {
                let a = self.view.operands(inst).value();
                let src = self.val_locals[a.index()];
                let dst = self.val_locals[inst];
                for i in 0..u32::from(dst.count) {
                    self.local_get(src.first + i);
                    self.local_set(dst.first + i);
                }
            }

            // ---- Pointers & Addresses ----
            Op::PtrAdd => {
                let mut o = self.view.operands(inst);
                let (ptr, offset) = (o.value(), o.value());
                self.local_get(self.loc(ptr));
                self.push_sext_to_i64(offset);
                self.op(OP_I64_ADD);
                self.local_set(self.val_locals[inst].first);
            }
            Op::PtrAddr | Op::Expose | Op::FromExposed => {
                let a = self.view.operands(inst).value();
                self.local_get(self.loc(a));
                self.local_set(self.val_locals[inst].first);
            }
            Op::PtrWithAddr => {
                let mut o = self.view.operands(inst);
                let (_ptr, addr) = (o.value(), o.value());
                self.local_get(self.loc(addr));
                self.local_set(self.val_locals[inst].first);
            }
            Op::FuncAddr => {
                let ref_id = self.view.operands(inst).reference();
                let rec = self.view.reference(ref_id);
                if RefKind::from_word(rec.kind) != Some(RefKind::Func) {
                    return Err(LirWasm64Error::UnsupportedRef {
                        func: self.place,
                        inst,
                        kind: rec.kind,
                        place: rec.place,
                    });
                }
                let wasm_idx = self.ctx.resolve_func(self.place, inst, rec.place)?;
                let slot = self.ctx.intern_indirect_func(wasm_idx);
                self.i64_const(i64::from(slot));
                self.local_set(self.val_locals[inst].first);
            }
            Op::DataAddr => {
                let ref_id = self.view.operands(inst).reference();
                if ref_id.index() >= self.view.refs.len() {
                    return Err(LirWasm64Error::UnsupportedRef {
                        func: self.place,
                        inst,
                        kind: u32::MAX,
                        place: ref_id.0,
                    });
                }
                let rec = self.view.reference(ref_id);
                let addr: u64 = match RefKind::from_word(rec.kind) {
                    Some(RefKind::Data) => *self.ctx.func_data_addrs[self.func_idx]
                        .get(rec.place as usize)
                        .ok_or(LirWasm64Error::UnsupportedRef {
                            func: self.place,
                            inst,
                            kind: rec.kind,
                            place: rec.place,
                        })?,
                    Some(RefKind::Static | RefKind::Vtable | RefKind::Tls) => {
                        self.ctx.resolve_static(self.place, inst, rec.place)?
                    }
                    _ => {
                        return Err(LirWasm64Error::UnsupportedRef {
                            func: self.place,
                            inst,
                            kind: rec.kind,
                            place: rec.place,
                        });
                    }
                };
                self.i64_const(addr as i64);
                self.local_set(self.val_locals[inst].first);
            }
            Op::TlsAddr => {
                let ref_id = self.view.operands(inst).reference();
                let rec = self.view.reference(ref_id);
                let addr = self.ctx.resolve_static(self.place, inst, rec.place)?;
                self.i64_const(addr as i64);
                self.local_set(self.val_locals[inst].first);
            }
            Op::SlotAddr => {
                let slot_id: SlotId = self.view.operands(inst).slot();
                let sp = self.sp_local.ok_or(LirWasm64Error::Overflow { what: "missing stack frame" })?;
                let slot_off = self.slot_offsets[slot_id.index()];
                self.local_get(sp);
                if slot_off != 0 {
                    self.i64_const(slot_off as i64);
                    self.op(OP_I64_ADD);
                }
                self.local_set(self.val_locals[inst].first);
            }
            Op::StackPointer => {
                self.global_get(GLOBAL_STACK_POINTER);
                self.local_set(self.val_locals[inst].first);
            }

            // ---- Memory Ops ----
            Op::Load => {
                let mut o = self.view.operands(inst);
                let ptr = o.value();
                let offset = o.offset();
                let align_hint = self.view.mem_attrs(inst).map_or(0, |a| u32::from(a.align));
                self.emit_load(inst, ty, self.loc(ptr), offset, align_hint, self.val_locals[inst].first)?;
            }
            Op::Store => {
                let mut o = self.view.operands(inst);
                let val = o.value();
                let ptr = o.value();
                let offset = o.offset();
                let align_hint = self.view.mem_attrs(inst).map_or(0, |a| u32::from(a.align));
                let val_ty = self.view.ty(val);
                self.emit_store(inst, val_ty, self.loc(val), self.loc(ptr), offset, align_hint)?;
            }
            Op::Memcpy | Op::Memmove => {
                let mut o = self.view.operands(inst);
                let (dst, src, len) = (o.value(), o.value(), o.value());
                self.local_get(self.loc(dst));
                self.local_get(self.loc(src));
                self.push_zext_to_i64(len);
                self.fc_op(FC_MEMORY_COPY);
                self.code.push(0x00); // dst mem 0
                self.code.push(0x00); // src mem 0
            }
            Op::Memset => {
                let mut o = self.view.operands(inst);
                let (dst, byte, len) = (o.value(), o.value(), o.value());
                self.local_get(self.loc(dst));
                self.local_get(self.loc(byte));
                self.push_zext_to_i64(len);
                self.fc_op(FC_MEMORY_FILL);
                self.code.push(0x00); // mem 0
            }
            Op::Memcmp => {
                let mut o = self.view.operands(inst);
                let (a, b, len) = (o.value(), o.value(), o.value());
                self.emit_memcmp(inst, a, b, len);
            }
            Op::AtomicLoad => {
                let ptr = self.view.operands(inst).value();
                let align_hint = self.view.mem_attrs(inst).map_or(0, |a| u32::from(a.align));
                self.emit_load(inst, ty, self.loc(ptr), 0, align_hint, self.val_locals[inst].first)?;
            }
            Op::AtomicStore => {
                let mut o = self.view.operands(inst);
                let (val, ptr) = (o.value(), o.value());
                let align_hint = self.view.mem_attrs(inst).map_or(0, |a| u32::from(a.align));
                self.emit_store(inst, self.view.ty(val), self.loc(val), self.loc(ptr), 0, align_hint)?;
            }
            Op::AtomicRmw => {
                let mut o = self.view.operands(inst);
                let rmw = o.rmw();
                let (ptr, val) = (o.value(), o.value());
                self.emit_atomic_rmw(inst, rmw, ty, ptr, val)?;
            }
            Op::Cmpxchg => {
                let mut o = self.view.operands(inst);
                let (ptr, expected, new_val) = (o.value(), o.value(), o.value());
                self.emit_cmpxchg(inst, ptr, expected, new_val)?;
            }

            // ---- Calls & Compound Projection ----
            Op::Call => {
                let mut o = self.view.operands(inst);
                let callee_ref = o.reference();
                let sig_id = o.sig();
                let args = o.values();
                let wasm_idx = self.resolve_call_ref(inst, callee_ref)?;
                self.check_direct_call_sig(inst, sig_id, wasm_idx)?;
                for arg in args.iter() {
                    self.push_val(arg);
                }
                self.op(OP_CALL);
                push_u32_leb(&mut self.code, wasm_idx);
                self.pop_val(inst);
            }
            Op::CallIndirect => {
                let mut o = self.view.operands(inst);
                let callee = o.value();
                let sig_id = o.sig();
                let args = o.values();
                self.emit_call_indirect(inst, callee, sig_id, args)?;
                self.pop_val(inst);
            }
            Op::Project => {
                let mut o = self.view.operands(inst);
                let compound = o.value();
                let member_idx = o.imm();
                let compound_ty = self.view.ty(compound);
                let members = self.view.members(compound_ty);
                if (member_idx as usize) >= members.len() {
                    return Err(LirWasm64Error::InvalidCompound {
                        func: self.place,
                        inst,
                        index: member_idx,
                    });
                }
                let mut offset = 0u32;
                for &m in &members[..member_idx as usize] {
                    offset += u32::from(wasm_slot_count(self.view, Ty(m), self.place, inst)?);
                }
                let src_base = self.val_locals[compound.index()].first + offset;
                let dst = self.val_locals[inst];
                for i in 0..u32::from(dst.count) {
                    self.local_get(src_base + i);
                    self.local_set(dst.first + i);
                }
            }

            // ---- Terminators ----
            Op::Jump => {
                let target = self.view.operands(inst).target();
                let depth = dispatch_depth.expect("jump requires dispatcher");
                self.emit_jump_target(target, depth);
            }
            Op::Br => {
                let mut o = self.view.operands(inst);
                let cond = o.value();
                let t_then = o.target();
                let t_else = o.target();
                let depth = dispatch_depth.expect("br requires dispatcher");
                self.local_get(self.loc(cond));
                self.op(OP_IF);
                self.code.push(BLOCKTYPE_VOID);
                self.emit_jump_target(t_then, depth + 1);
                self.op(OP_ELSE);
                self.emit_jump_target(t_else, depth + 1);
                self.op(OP_END);
            }
            Op::Switch => {
                let mut o = self.view.operands(inst);
                let scrutinee = o.value();
                let default = o.target();
                let cases = o.cases(scrutinee);
                let depth = dispatch_depth.expect("switch requires dispatcher");
                let scr_ty = self.view.ty(scrutinee);
                for (case_words, target) in cases {
                    match scr_ty.elem() {
                        Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 => {
                            let val = match sub32_mask(scr_ty.elem()) {
                                Some(m) => case_words[0] & m,
                                None => case_words[0],
                            };
                            self.local_get(self.loc(scrutinee));
                            self.i32_const(val as i32);
                            self.op(OP_I32_EQ);
                        }
                        Scalar::I64 => {
                            let val = u64::from(case_words[0]) | (u64::from(case_words[1]) << 32);
                            self.local_get(self.loc(scrutinee));
                            self.i64_const(val as i64);
                            self.op(OP_I64_EQ);
                        }
                        Scalar::I128 => {
                            let lo = u64::from(case_words[0]) | (u64::from(case_words[1]) << 32);
                            let hi = u64::from(case_words[2]) | (u64::from(case_words[3]) << 32);
                            let (s_lo, s_hi) = self.loc_pair(scrutinee);
                            self.local_get(s_lo);
                            self.i64_const(lo as i64);
                            self.op(OP_I64_EQ);
                            self.local_get(s_hi);
                            self.i64_const(hi as i64);
                            self.op(OP_I64_EQ);
                            self.op(OP_I32_AND);
                        }
                        _ => {
                            return Err(LirWasm64Error::UnsupportedType {
                                func: self.place,
                                inst,
                                ty: scr_ty,
                            });
                        }
                    }
                    self.op(OP_IF);
                    self.code.push(BLOCKTYPE_VOID);
                    self.emit_jump_target(target, depth + 1);
                    self.op(OP_END);
                }
                self.emit_jump_target(default, depth);
            }
            Op::Return => {
                let vals = self.view.operands(inst).values();
                self.emit_epilogue();
                for v in vals.iter() {
                    self.push_val(v);
                }
                self.op(OP_RETURN);
            }
            Op::Unreachable | Op::Trap | Op::Resume => {
                self.op(OP_UNREACHABLE);
            }
            Op::Invoke => {
                let mut o = self.view.operands(inst);
                let callee_ref = o.reference();
                let sig_id = o.sig();
                let normal_block = o.block();
                let _unwind_block = o.block();
                let args = o.values();
                let depth = dispatch_depth.expect("invoke requires dispatcher");
                let wasm_idx = self.resolve_call_ref(inst, callee_ref)?;
                self.check_direct_call_sig(inst, sig_id, wasm_idx)?;
                for arg in args.iter() {
                    self.push_val(arg);
                }
                self.op(OP_CALL);
                push_u32_leb(&mut self.code, wasm_idx);
                self.pop_val(inst);
                let pc = self.pc_local.expect("pc_local for invoke");
                self.i32_const(normal_block.0 as i32);
                self.local_set(pc);
                self.br(depth);
            }
            Op::InvokeIndirect => {
                let mut o = self.view.operands(inst);
                let callee = o.value();
                let sig_id = o.sig();
                let normal_block = o.block();
                let _unwind_block = o.block();
                let args = o.values();
                let depth = dispatch_depth.expect("invoke_indirect requires dispatcher");
                self.emit_call_indirect(inst, callee, sig_id, args)?;
                self.pop_val(inst);
                let pc = self.pc_local.expect("pc_local for invoke_indirect");
                self.i32_const(normal_block.0 as i32);
                self.local_set(pc);
                self.br(depth);
            }
            Op::TailCall => {
                let mut o = self.view.operands(inst);
                let callee_ref = o.reference();
                let sig_id = o.sig();
                let args = o.values();
                let wasm_idx = self.resolve_call_ref(inst, callee_ref)?;
                self.check_direct_call_sig(inst, sig_id, wasm_idx)?;
                self.emit_epilogue();
                for arg in args.iter() {
                    self.push_val(arg);
                }
                self.op(OP_CALL);
                push_u32_leb(&mut self.code, wasm_idx);
                self.op(OP_RETURN);
            }
            Op::TailCallIndirect => {
                let mut o = self.view.operands(inst);
                let callee = o.value();
                let sig_id = o.sig();
                let args = o.values();
                self.emit_epilogue();
                self.emit_call_indirect(inst, callee, sig_id, args)?;
                self.op(OP_RETURN);
            }

            // ---- Unsupported ops (fail loud — Rule 3) ----
            _ => return Err(LirWasm64Error::UnsupportedOp { func: self.place, inst, op }),
        }
        Ok(())
    }

    // ---- Call helpers -----------------------------------------------------------------------

    fn resolve_call_ref(&self, inst: usize, callee_ref: RefId) -> Result<u32, LirWasm64Error> {
        if callee_ref.index() >= self.view.refs.len() {
            return Err(LirWasm64Error::UnsupportedRef {
                func: self.place,
                inst,
                kind: u32::MAX,
                place: callee_ref.0,
            });
        }
        let rec = self.view.reference(callee_ref);
        if RefKind::from_word(rec.kind) != Some(RefKind::Func) {
            return Err(LirWasm64Error::UnsupportedRef {
                func: self.place,
                inst,
                kind: rec.kind,
                place: rec.place,
            });
        }
        self.ctx.resolve_func(self.place, inst, rec.place)
    }

    fn check_direct_call_sig(
        &self,
        inst: usize,
        sig_id: SigId,
        wasm_idx: u32,
    ) -> Result<(), LirWasm64Error> {
        let (call_p, call_r) = flatten_sig(self.view, sig_id, self.place, inst, self.ctx.alloc)?;
        let (exp_p, exp_r) = self.ctx.expected_wasm_sig(wasm_idx);
        if call_p.as_slice() != exp_p || call_r.as_slice() != exp_r {
            return Err(LirWasm64Error::InvalidSignature {
                func: self.place,
                inst,
                what: "call signature does not match target function signature",
            });
        }
        Ok(())
    }

    fn emit_call_indirect(
        &mut self,
        inst: usize,
        callee: Value,
        sig_id: SigId,
        args: blaze_lir::op::Values<'_>,
    ) -> Result<(), LirWasm64Error> {
        // If no function pointer was ever taken anywhere in the module, no table exists and
        // every indirect call must trap cleanly.
        if self.ctx.indirect_table.is_empty() {
            self.op(OP_UNREACHABLE);
            return Ok(());
        }
        let (call_p, call_r) = flatten_sig(self.view, sig_id, self.place, inst, self.ctx.alloc)?;
        let type_idx = self.ctx.intern_type(call_p.as_slice(), call_r.as_slice());

        for arg in args.iter() {
            self.push_val(arg);
        }
        // Convert 64-bit callee slot to 32-bit table index; any non-zero high 32 bits map to
        // slot 0 (null trap) instead of wrapping into a valid table slot.
        let c_loc = self.loc(callee);
        self.i32_const(0);
        self.local_get(c_loc);
        self.op(OP_I32_WRAP_I64);
        self.local_get(c_loc);
        self.i64_const(32);
        self.op(OP_I64_SHR_U);
        self.i64_const(0);
        self.op(OP_I64_NE);
        self.op(OP_SELECT);

        self.op(OP_CALL_INDIRECT);
        push_u32_leb(&mut self.code, type_idx);
        push_u32_leb(&mut self.code, 0); // table 0
        Ok(())
    }

    // ---- Integer binary ops -----------------------------------------------------------------

    fn emit_int_binary(
        &mut self,
        inst: usize,
        op: Op,
        ty: Ty,
        a: Value,
        b: Value,
    ) -> Result<(), LirWasm64Error> {
        let scalar = ty.elem();
        match scalar {
            Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 => {
                self.emit_i32_binary(inst, op, scalar, a, b)
            }
            Scalar::I64 => self.emit_i64_binary(inst, op, a, b),
            Scalar::I128 => self.emit_i128_binary(inst, op, a, b),
            _ => Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty }),
        }
    }

    fn emit_i32_binary(
        &mut self,
        inst: usize,
        op: Op,
        scalar: Scalar,
        a: Value,
        b: Value,
    ) -> Result<(), LirWasm64Error> {
        let dst = self.val_locals[inst].first;
        let (la, lb) = (self.loc(a), self.loc(b));
        let bits = sub32_bits(scalar);

        match op {
            Op::Add => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I32_ADD);
                self.mask_sub32(scalar);
            }
            Op::Sub => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I32_SUB);
                self.mask_sub32(scalar);
            }
            Op::Mul => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I32_MUL);
                self.mask_sub32(scalar);
            }
            Op::And => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I32_AND);
            }
            Op::Or => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I32_OR);
            }
            Op::Xor => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I32_XOR);
            }
            Op::Udiv => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I32_DIV_U);
            }
            Op::Urem => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I32_REM_U);
            }
            Op::Sdiv => {
                if bits < 32 {
                    let ta = self.temp_i32(0);
                    let tb = self.temp_i32(1);
                    self.local_get(la);
                    self.sext_to_i32(scalar);
                    self.local_set(ta);
                    self.local_get(lb);
                    self.sext_to_i32(scalar);
                    self.local_set(tb);
                    // Trap on MIN / -1 (Wasm i32.div_s only traps on i32::MIN / -1).
                    self.local_get(ta);
                    self.i32_const(sub32_smin(scalar));
                    self.op(OP_I32_EQ);
                    self.local_get(tb);
                    self.i32_const(-1);
                    self.op(OP_I32_EQ);
                    self.op(OP_I32_AND);
                    self.op(OP_IF);
                    self.code.push(BLOCKTYPE_VOID);
                    self.op(OP_UNREACHABLE);
                    self.op(OP_END);
                    self.local_get(ta);
                    self.local_get(tb);
                    self.op(OP_I32_DIV_S);
                    self.mask_sub32(scalar);
                } else {
                    self.local_get(la);
                    self.local_get(lb);
                    self.op(OP_I32_DIV_S);
                }
            }
            Op::Srem => {
                self.local_get(la);
                self.sext_to_i32(scalar);
                self.local_get(lb);
                self.sext_to_i32(scalar);
                self.op(OP_I32_REM_S);
                self.mask_sub32(scalar);
            }
            Op::Shl => {
                self.local_get(la);
                if bits < 32 {
                    self.local_get(lb);
                    self.i32_const((bits - 1) as i32);
                    self.op(OP_I32_AND);
                } else {
                    self.local_get(lb);
                }
                self.op(OP_I32_SHL);
                self.mask_sub32(scalar);
            }
            Op::Lshr => {
                self.local_get(la);
                if bits < 32 {
                    self.local_get(lb);
                    self.i32_const((bits - 1) as i32);
                    self.op(OP_I32_AND);
                } else {
                    self.local_get(lb);
                }
                self.op(OP_I32_SHR_U);
            }
            Op::Ashr => {
                self.local_get(la);
                self.sext_to_i32(scalar);
                if bits < 32 {
                    self.local_get(lb);
                    self.i32_const((bits - 1) as i32);
                    self.op(OP_I32_AND);
                } else {
                    self.local_get(lb);
                }
                self.op(OP_I32_SHR_S);
                self.mask_sub32(scalar);
            }
            Op::Rotl => {
                if bits == 32 {
                    self.local_get(la);
                    self.local_get(lb);
                    self.op(OP_I32_ROTL);
                } else if bits == 1 {
                    self.local_get(la);
                } else {
                    let amt = self.temp_i32(0);
                    self.local_get(lb);
                    self.i32_const((bits - 1) as i32);
                    self.op(OP_I32_AND);
                    self.local_set(amt);
                    // ((a << amt) | (a >>_u (bits - amt))) & mask; valid even when amt == 0
                    // because upper bits of `a` are zero so `a >>_u bits == 0`.
                    self.local_get(la);
                    self.local_get(amt);
                    self.op(OP_I32_SHL);
                    self.local_get(la);
                    self.i32_const(bits as i32);
                    self.local_get(amt);
                    self.op(OP_I32_SUB);
                    self.op(OP_I32_SHR_U);
                    self.op(OP_I32_OR);
                    self.mask_sub32(scalar);
                }
            }
            Op::Rotr => {
                if bits == 32 {
                    self.local_get(la);
                    self.local_get(lb);
                    self.op(OP_I32_ROTR);
                } else if bits == 1 {
                    self.local_get(la);
                } else {
                    let amt = self.temp_i32(0);
                    self.local_get(lb);
                    self.i32_const((bits - 1) as i32);
                    self.op(OP_I32_AND);
                    self.local_set(amt);
                    self.local_get(la);
                    self.local_get(amt);
                    self.op(OP_I32_SHR_U);
                    self.local_get(la);
                    self.i32_const(bits as i32);
                    self.local_get(amt);
                    self.op(OP_I32_SUB);
                    self.op(OP_I32_SHL);
                    self.op(OP_I32_OR);
                    self.mask_sub32(scalar);
                }
            }
            Op::Umulhi => {
                if bits < 32 {
                    self.local_get(la);
                    self.local_get(lb);
                    self.op(OP_I32_MUL);
                    self.i32_const(bits as i32);
                    self.op(OP_I32_SHR_U);
                    self.mask_sub32(scalar);
                } else {
                    self.local_get(la);
                    self.op(OP_I64_EXTEND_I32_U);
                    self.local_get(lb);
                    self.op(OP_I64_EXTEND_I32_U);
                    self.op(OP_I64_MUL);
                    self.i64_const(32);
                    self.op(OP_I64_SHR_U);
                    self.op(OP_I32_WRAP_I64);
                }
            }
            Op::Smulhi => {
                if bits < 32 {
                    self.local_get(la);
                    self.sext_to_i32(scalar);
                    self.local_get(lb);
                    self.sext_to_i32(scalar);
                    self.op(OP_I32_MUL);
                    self.i32_const(bits as i32);
                    self.op(OP_I32_SHR_S);
                    self.mask_sub32(scalar);
                } else {
                    self.local_get(la);
                    self.op(OP_I64_EXTEND_I32_S);
                    self.local_get(lb);
                    self.op(OP_I64_EXTEND_I32_S);
                    self.op(OP_I64_MUL);
                    self.i64_const(32);
                    self.op(OP_I64_SHR_S);
                    self.op(OP_I32_WRAP_I64);
                }
            }
            Op::Umin | Op::Umax => {
                self.local_get(la);
                self.local_get(lb);
                self.local_get(la);
                self.local_get(lb);
                self.op(if op == Op::Umin { OP_I32_LT_U } else { OP_I32_GT_U });
                self.op(OP_SELECT);
            }
            Op::Smin | Op::Smax => {
                self.local_get(la);
                self.local_get(lb);
                self.local_get(la);
                self.sext_to_i32(scalar);
                self.local_get(lb);
                self.sext_to_i32(scalar);
                self.op(if op == Op::Smin { OP_I32_LT_S } else { OP_I32_GT_S });
                self.op(OP_SELECT);
            }
            Op::UaddSat => {
                let sum = self.temp_i32(0);
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I32_ADD);
                self.local_set(sum);
                if let Some(mask) = sub32_mask(scalar) {
                    self.i32_const(mask as i32);
                    self.local_get(sum);
                    self.local_get(sum);
                    self.i32_const(mask as i32);
                    self.op(OP_I32_GT_U);
                    self.op(OP_SELECT);
                } else {
                    self.i32_const(-1);
                    self.local_get(sum);
                    self.local_get(sum);
                    self.local_get(la);
                    self.op(OP_I32_LT_U);
                    self.op(OP_SELECT);
                }
            }
            Op::UsubSat => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I32_SUB);
                self.i32_const(0);
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I32_GE_U);
                self.op(OP_SELECT);
            }
            Op::SaddSat | Op::SsubSat => {
                if bits < 32 {
                    let s = self.temp_i32(0);
                    let lo = sub32_smin(scalar);
                    let hi = sub32_smax(scalar);
                    self.local_get(la);
                    self.sext_to_i32(scalar);
                    self.local_get(lb);
                    self.sext_to_i32(scalar);
                    self.op(if op == Op::SaddSat { OP_I32_ADD } else { OP_I32_SUB });
                    self.local_set(s);
                    // clamp to [lo, hi]
                    self.i32_const(lo);
                    self.local_get(s);
                    self.local_get(s);
                    self.i32_const(lo);
                    self.op(OP_I32_LT_S);
                    self.op(OP_SELECT);
                    self.local_set(s);
                    self.i32_const(hi);
                    self.local_get(s);
                    self.local_get(s);
                    self.i32_const(hi);
                    self.op(OP_I32_GT_S);
                    self.op(OP_SELECT);
                    self.mask_sub32(scalar);
                } else {
                    let s = self.temp_i64(0);
                    self.local_get(la);
                    self.op(OP_I64_EXTEND_I32_S);
                    self.local_get(lb);
                    self.op(OP_I64_EXTEND_I32_S);
                    self.op(if op == Op::SaddSat { OP_I64_ADD } else { OP_I64_SUB });
                    self.local_set(s);
                    self.i64_const(i64::from(i32::MIN));
                    self.local_get(s);
                    self.local_get(s);
                    self.i64_const(i64::from(i32::MIN));
                    self.op(OP_I64_LT_S);
                    self.op(OP_SELECT);
                    self.local_set(s);
                    self.i64_const(i64::from(i32::MAX));
                    self.local_get(s);
                    self.local_get(s);
                    self.i64_const(i64::from(i32::MAX));
                    self.op(OP_I64_GT_S);
                    self.op(OP_SELECT);
                    self.op(OP_I32_WRAP_I64);
                }
            }
            _ => return Err(LirWasm64Error::UnsupportedOp { func: self.place, inst, op }),
        }
        self.local_set(dst);
        Ok(())
    }

    fn emit_i64_binary(
        &mut self,
        inst: usize,
        op: Op,
        a: Value,
        b: Value,
    ) -> Result<(), LirWasm64Error> {
        let dst = self.val_locals[inst].first;
        let (la, lb) = (self.loc(a), self.loc(b));
        match op {
            Op::Add => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_ADD);
            }
            Op::Sub => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_SUB);
            }
            Op::Mul => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_MUL);
            }
            Op::Udiv => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_DIV_U);
            }
            Op::Sdiv => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_DIV_S);
            }
            Op::Urem => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_REM_U);
            }
            Op::Srem => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_REM_S);
            }
            Op::And => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_AND);
            }
            Op::Or => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_OR);
            }
            Op::Xor => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_XOR);
            }
            Op::Shl => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_SHL);
            }
            Op::Lshr => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_SHR_U);
            }
            Op::Ashr => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_SHR_S);
            }
            Op::Rotl => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_ROTL);
            }
            Op::Rotr => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_ROTR);
            }
            Op::Umulhi => {
                self.emit_umulhi64_to(la, lb, dst);
                return Ok(());
            }
            Op::Smulhi => {
                self.emit_smulhi64_to(la, lb, dst);
                return Ok(());
            }
            Op::Umin | Op::Umax | Op::Smin | Op::Smax => {
                let cmp = match op {
                    Op::Umin => OP_I64_LT_U,
                    Op::Umax => OP_I64_GT_U,
                    Op::Smin => OP_I64_LT_S,
                    _ => OP_I64_GT_S,
                };
                self.local_get(la);
                self.local_get(lb);
                self.local_get(la);
                self.local_get(lb);
                self.op(cmp);
                self.op(OP_SELECT);
            }
            Op::UaddSat => {
                let sum = self.temp_i64(0);
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_ADD);
                self.local_set(sum);
                self.i64_const(-1);
                self.local_get(sum);
                self.local_get(sum);
                self.local_get(la);
                self.op(OP_I64_LT_U);
                self.op(OP_SELECT);
            }
            Op::UsubSat => {
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_SUB);
                self.i64_const(0);
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_GE_U);
                self.op(OP_SELECT);
            }
            Op::SaddSat => {
                let sum = self.temp_i64(0);
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_ADD);
                self.local_set(sum);
                // sat_val = (a < 0) ? i64::MIN : i64::MAX
                self.i64_const(i64::MIN);
                self.i64_const(i64::MAX);
                self.local_get(la);
                self.i64_const(0);
                self.op(OP_I64_LT_S);
                self.op(OP_SELECT);
                self.local_get(sum);
                // overflow iff ((a ^ sum) & (b ^ sum)) < 0
                self.local_get(la);
                self.local_get(sum);
                self.op(OP_I64_XOR);
                self.local_get(lb);
                self.local_get(sum);
                self.op(OP_I64_XOR);
                self.op(OP_I64_AND);
                self.i64_const(0);
                self.op(OP_I64_LT_S);
                self.op(OP_SELECT);
            }
            Op::SsubSat => {
                let diff = self.temp_i64(0);
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_SUB);
                self.local_set(diff);
                self.i64_const(i64::MIN);
                self.i64_const(i64::MAX);
                self.local_get(la);
                self.i64_const(0);
                self.op(OP_I64_LT_S);
                self.op(OP_SELECT);
                self.local_get(diff);
                // overflow iff ((a ^ b) & (a ^ diff)) < 0
                self.local_get(la);
                self.local_get(lb);
                self.op(OP_I64_XOR);
                self.local_get(la);
                self.local_get(diff);
                self.op(OP_I64_XOR);
                self.op(OP_I64_AND);
                self.i64_const(0);
                self.op(OP_I64_LT_S);
                self.op(OP_SELECT);
            }
            _ => return Err(LirWasm64Error::UnsupportedOp { func: self.place, inst, op }),
        }
        self.local_set(dst);
        Ok(())
    }

    /// Computes unsigned high 64 bits of `la * lb` into `out_local` (uses `temps_i64[0..=5]`).
    fn emit_umulhi64_to(&mut self, la: u32, lb: u32, out_local: u32) {
        let a0 = self.temp_i64(0);
        let a1 = self.temp_i64(1);
        let b0 = self.temp_i64(2);
        let b1 = self.temp_i64(3);
        let p01 = self.temp_i64(4);
        let p10 = self.temp_i64(5);
        let mask32 = 0xFFFF_FFFFi64;

        self.local_get(la);
        self.i64_const(mask32);
        self.op(OP_I64_AND);
        self.local_set(a0);
        self.local_get(la);
        self.i64_const(32);
        self.op(OP_I64_SHR_U);
        self.local_set(a1);

        self.local_get(lb);
        self.i64_const(mask32);
        self.op(OP_I64_AND);
        self.local_set(b0);
        self.local_get(lb);
        self.i64_const(32);
        self.op(OP_I64_SHR_U);
        self.local_set(b1);

        self.local_get(a0);
        self.local_get(b1);
        self.op(OP_I64_MUL);
        self.local_set(p01);

        self.local_get(a1);
        self.local_get(b0);
        self.op(OP_I64_MUL);
        self.local_set(p10);

        // mid = (p00 >> 32) + (p01 & mask32) + (p10 & mask32)
        // hi = (a1 * b1) + (p01 >> 32) + (p10 >> 32) + (mid >> 32)
        self.local_get(a1);
        self.local_get(b1);
        self.op(OP_I64_MUL);
        self.local_get(p01);
        self.i64_const(32);
        self.op(OP_I64_SHR_U);
        self.op(OP_I64_ADD);
        self.local_get(p10);
        self.i64_const(32);
        self.op(OP_I64_SHR_U);
        self.op(OP_I64_ADD);

        // mid >> 32
        self.local_get(a0);
        self.local_get(b0);
        self.op(OP_I64_MUL);
        self.i64_const(32);
        self.op(OP_I64_SHR_U);
        self.local_get(p01);
        self.i64_const(mask32);
        self.op(OP_I64_AND);
        self.op(OP_I64_ADD);
        self.local_get(p10);
        self.i64_const(mask32);
        self.op(OP_I64_AND);
        self.op(OP_I64_ADD);
        self.i64_const(32);
        self.op(OP_I64_SHR_U);

        self.op(OP_I64_ADD);
        self.local_set(out_local);
    }

    fn emit_smulhi64_to(&mut self, la: u32, lb: u32, out_local: u32) {
        self.emit_umulhi64_to(la, lb, out_local);
        // if a < 0 { hi -= b }
        self.local_get(out_local);
        self.local_get(lb);
        self.i64_const(0);
        self.local_get(la);
        self.i64_const(0);
        self.op(OP_I64_LT_S);
        self.op(OP_SELECT);
        self.op(OP_I64_SUB);
        // if b < 0 { hi -= a }
        self.local_get(la);
        self.i64_const(0);
        self.local_get(lb);
        self.i64_const(0);
        self.op(OP_I64_LT_S);
        self.op(OP_SELECT);
        self.op(OP_I64_SUB);
        self.local_set(out_local);
    }

    fn emit_i128_binary(
        &mut self,
        inst: usize,
        op: Op,
        a: Value,
        b: Value,
    ) -> Result<(), LirWasm64Error> {
        let (a_lo, a_hi) = self.loc_pair(a);
        let (b_lo, b_hi) = self.loc_pair(b);
        let d_lo = self.val_locals[inst].first;
        let d_hi = d_lo + 1;

        match op {
            Op::Add => {
                self.local_get(a_lo);
                self.local_get(b_lo);
                self.op(OP_I64_ADD);
                self.local_tee(d_lo);
                self.local_get(a_lo);
                self.op(OP_I64_LT_U);
                self.op(OP_I64_EXTEND_I32_U);
                self.local_get(a_hi);
                self.local_get(b_hi);
                self.op(OP_I64_ADD);
                self.op(OP_I64_ADD);
                self.local_set(d_hi);
            }
            Op::Sub => {
                self.local_get(a_lo);
                self.local_get(b_lo);
                self.op(OP_I64_SUB);
                self.local_set(d_lo);
                self.local_get(a_hi);
                self.local_get(b_hi);
                self.op(OP_I64_SUB);
                self.local_get(a_lo);
                self.local_get(b_lo);
                self.op(OP_I64_LT_U);
                self.op(OP_I64_EXTEND_I32_U);
                self.op(OP_I64_SUB);
                self.local_set(d_hi);
            }
            Op::Mul => {
                self.emit_umulhi64_to(a_lo, b_lo, d_hi);
                self.local_get(d_hi);
                self.local_get(a_lo);
                self.local_get(b_hi);
                self.op(OP_I64_MUL);
                self.op(OP_I64_ADD);
                self.local_get(a_hi);
                self.local_get(b_lo);
                self.op(OP_I64_MUL);
                self.op(OP_I64_ADD);
                self.local_set(d_hi);
                self.local_get(a_lo);
                self.local_get(b_lo);
                self.op(OP_I64_MUL);
                self.local_set(d_lo);
            }
            Op::And | Op::Or | Op::Xor => {
                let wop = match op {
                    Op::And => OP_I64_AND,
                    Op::Or => OP_I64_OR,
                    _ => OP_I64_XOR,
                };
                self.local_get(a_lo);
                self.local_get(b_lo);
                self.op(wop);
                self.local_set(d_lo);
                self.local_get(a_hi);
                self.local_get(b_hi);
                self.op(wop);
                self.local_set(d_hi);
            }
            Op::Shl => {
                let s = self.temp_i64(0);
                self.local_get(b_lo);
                self.i64_const(127);
                self.op(OP_I64_AND);
                self.local_tee(s);
                self.op(OP_I64_EQZ);
                self.op(OP_IF);
                self.code.push(BLOCKTYPE_VOID);
                self.local_get(a_lo);
                self.local_set(d_lo);
                self.local_get(a_hi);
                self.local_set(d_hi);
                self.op(OP_ELSE);
                self.local_get(s);
                self.i64_const(64);
                self.op(OP_I64_LT_U);
                self.op(OP_IF);
                self.code.push(BLOCKTYPE_VOID);
                self.local_get(a_lo);
                self.local_get(s);
                self.op(OP_I64_SHL);
                self.local_set(d_lo);
                self.local_get(a_hi);
                self.local_get(s);
                self.op(OP_I64_SHL);
                self.local_get(a_lo);
                self.i64_const(64);
                self.local_get(s);
                self.op(OP_I64_SUB);
                self.op(OP_I64_SHR_U);
                self.op(OP_I64_OR);
                self.local_set(d_hi);
                self.op(OP_ELSE);
                self.i64_const(0);
                self.local_set(d_lo);
                self.local_get(a_lo);
                self.local_get(s);
                self.op(OP_I64_SHL); // Wasm masks shift by 63, so s - 64 == s mod 64
                self.local_set(d_hi);
                self.op(OP_END);
                self.op(OP_END);
            }
            Op::Lshr | Op::Ashr => {
                let s = self.temp_i64(0);
                let hi_shr = if op == Op::Ashr { OP_I64_SHR_S } else { OP_I64_SHR_U };
                self.local_get(b_lo);
                self.i64_const(127);
                self.op(OP_I64_AND);
                self.local_tee(s);
                self.op(OP_I64_EQZ);
                self.op(OP_IF);
                self.code.push(BLOCKTYPE_VOID);
                self.local_get(a_lo);
                self.local_set(d_lo);
                self.local_get(a_hi);
                self.local_set(d_hi);
                self.op(OP_ELSE);
                self.local_get(s);
                self.i64_const(64);
                self.op(OP_I64_LT_U);
                self.op(OP_IF);
                self.code.push(BLOCKTYPE_VOID);
                self.local_get(a_lo);
                self.local_get(s);
                self.op(OP_I64_SHR_U);
                self.local_get(a_hi);
                self.i64_const(64);
                self.local_get(s);
                self.op(OP_I64_SUB);
                self.op(OP_I64_SHL);
                self.op(OP_I64_OR);
                self.local_set(d_lo);
                self.local_get(a_hi);
                self.local_get(s);
                self.op(hi_shr);
                self.local_set(d_hi);
                self.op(OP_ELSE);
                self.local_get(a_hi);
                self.local_get(s);
                self.op(hi_shr);
                self.local_set(d_lo);
                if op == Op::Ashr {
                    self.local_get(a_hi);
                    self.i64_const(63);
                    self.op(OP_I64_SHR_S);
                } else {
                    self.i64_const(0);
                }
                self.local_set(d_hi);
                self.op(OP_END);
                self.op(OP_END);
            }
            Op::Umin | Op::Umax | Op::Smin | Op::Smax => {
                let cond = match op {
                    Op::Umin => IntCond::Ult,
                    Op::Umax => IntCond::Ugt,
                    Op::Smin => IntCond::Slt,
                    _ => IntCond::Sgt,
                };
                let c = self.temp_i32(0);
                self.push_i128_cmp(cond, a_lo, a_hi, b_lo, b_hi);
                self.local_set(c);
                self.local_get(a_lo);
                self.local_get(b_lo);
                self.local_get(c);
                self.op(OP_SELECT);
                self.local_set(d_lo);
                self.local_get(a_hi);
                self.local_get(b_hi);
                self.local_get(c);
                self.op(OP_SELECT);
                self.local_set(d_hi);
            }
            _ => return Err(LirWasm64Error::UnsupportedOp { func: self.place, inst, op }),
        }
        Ok(())
    }

    // ---- Overflow ops `{T, i1}` -------------------------------------------------------------

    fn emit_int_overflow(
        &mut self,
        inst: usize,
        op: Op,
        a: Value,
        b: Value,
    ) -> Result<(), LirWasm64Error> {
        let a_ty = self.view.ty(a);
        let scalar = a_ty.elem();
        let dst_first = self.val_locals[inst].first;

        match scalar {
            Scalar::I1 | Scalar::I8 | Scalar::I16 => {
                let (la, lb) = (self.loc(a), self.loc(b));
                let mask = sub32_mask(scalar).unwrap();
                let lo = sub32_smin(scalar);
                let hi = sub32_smax(scalar);
                let raw = self.temp_i32(0);
                let (res_loc, ovf_loc) = (dst_first, dst_first + 1);
                match op {
                    Op::UaddOverflow => {
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I32_ADD);
                        self.local_tee(raw);
                        self.i32_const(mask as i32);
                        self.op(OP_I32_AND);
                        self.local_set(res_loc);
                        self.local_get(raw);
                        self.i32_const(mask as i32);
                        self.op(OP_I32_GT_U);
                        self.local_set(ovf_loc);
                    }
                    Op::UsubOverflow => {
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I32_SUB);
                        self.i32_const(mask as i32);
                        self.op(OP_I32_AND);
                        self.local_set(res_loc);
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I32_LT_U);
                        self.local_set(ovf_loc);
                    }
                    Op::UmulOverflow => {
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I32_MUL);
                        self.local_tee(raw);
                        self.i32_const(mask as i32);
                        self.op(OP_I32_AND);
                        self.local_set(res_loc);
                        self.local_get(raw);
                        self.i32_const(mask as i32);
                        self.op(OP_I32_GT_U);
                        self.local_set(ovf_loc);
                    }
                    Op::SaddOverflow | Op::SsubOverflow | Op::SmulOverflow => {
                        let wop = match op {
                            Op::SaddOverflow => OP_I32_ADD,
                            Op::SsubOverflow => OP_I32_SUB,
                            _ => OP_I32_MUL,
                        };
                        self.local_get(la);
                        self.sext_to_i32(scalar);
                        self.local_get(lb);
                        self.sext_to_i32(scalar);
                        self.op(wop);
                        self.local_tee(raw);
                        self.i32_const(mask as i32);
                        self.op(OP_I32_AND);
                        self.local_set(res_loc);
                        self.local_get(raw);
                        self.i32_const(lo);
                        self.op(OP_I32_LT_S);
                        self.local_get(raw);
                        self.i32_const(hi);
                        self.op(OP_I32_GT_S);
                        self.op(OP_I32_OR);
                        self.local_set(ovf_loc);
                    }
                    _ => unreachable!(),
                }
            }
            Scalar::I32 => {
                let (la, lb) = (self.loc(a), self.loc(b));
                let (res_loc, ovf_loc) = (dst_first, dst_first + 1);
                match op {
                    Op::UaddOverflow => {
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I32_ADD);
                        self.local_tee(res_loc);
                        self.local_get(la);
                        self.op(OP_I32_LT_U);
                        self.local_set(ovf_loc);
                    }
                    Op::UsubOverflow => {
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I32_SUB);
                        self.local_set(res_loc);
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I32_LT_U);
                        self.local_set(ovf_loc);
                    }
                    Op::SaddOverflow => {
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I32_ADD);
                        self.local_set(res_loc);
                        self.local_get(la);
                        self.local_get(res_loc);
                        self.op(OP_I32_XOR);
                        self.local_get(lb);
                        self.local_get(res_loc);
                        self.op(OP_I32_XOR);
                        self.op(OP_I32_AND);
                        self.i32_const(0);
                        self.op(OP_I32_LT_S);
                        self.local_set(ovf_loc);
                    }
                    Op::SsubOverflow => {
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I32_SUB);
                        self.local_set(res_loc);
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I32_XOR);
                        self.local_get(la);
                        self.local_get(res_loc);
                        self.op(OP_I32_XOR);
                        self.op(OP_I32_AND);
                        self.i32_const(0);
                        self.op(OP_I32_LT_S);
                        self.local_set(ovf_loc);
                    }
                    Op::UmulOverflow => {
                        let prod = self.temp_i64(0);
                        self.local_get(la);
                        self.op(OP_I64_EXTEND_I32_U);
                        self.local_get(lb);
                        self.op(OP_I64_EXTEND_I32_U);
                        self.op(OP_I64_MUL);
                        self.local_tee(prod);
                        self.op(OP_I32_WRAP_I64);
                        self.local_set(res_loc);
                        self.local_get(prod);
                        self.i64_const(32);
                        self.op(OP_I64_SHR_U);
                        self.i64_const(0);
                        self.op(OP_I64_NE);
                        self.local_set(ovf_loc);
                    }
                    Op::SmulOverflow => {
                        let prod = self.temp_i64(0);
                        self.local_get(la);
                        self.op(OP_I64_EXTEND_I32_S);
                        self.local_get(lb);
                        self.op(OP_I64_EXTEND_I32_S);
                        self.op(OP_I64_MUL);
                        self.local_tee(prod);
                        self.op(OP_I32_WRAP_I64);
                        self.local_tee(res_loc);
                        self.op(OP_I64_EXTEND_I32_S);
                        self.local_get(prod);
                        self.op(OP_I64_NE);
                        self.local_set(ovf_loc);
                    }
                    _ => unreachable!(),
                }
            }
            Scalar::I64 => {
                let (la, lb) = (self.loc(a), self.loc(b));
                let (res_loc, ovf_loc) = (dst_first, dst_first + 1);
                match op {
                    Op::UaddOverflow => {
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I64_ADD);
                        self.local_tee(res_loc);
                        self.local_get(la);
                        self.op(OP_I64_LT_U);
                        self.local_set(ovf_loc);
                    }
                    Op::UsubOverflow => {
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I64_SUB);
                        self.local_set(res_loc);
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I64_LT_U);
                        self.local_set(ovf_loc);
                    }
                    Op::SaddOverflow => {
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I64_ADD);
                        self.local_set(res_loc);
                        self.local_get(la);
                        self.local_get(res_loc);
                        self.op(OP_I64_XOR);
                        self.local_get(lb);
                        self.local_get(res_loc);
                        self.op(OP_I64_XOR);
                        self.op(OP_I64_AND);
                        self.i64_const(0);
                        self.op(OP_I64_LT_S);
                        self.local_set(ovf_loc);
                    }
                    Op::SsubOverflow => {
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I64_SUB);
                        self.local_set(res_loc);
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I64_XOR);
                        self.local_get(la);
                        self.local_get(res_loc);
                        self.op(OP_I64_XOR);
                        self.op(OP_I64_AND);
                        self.i64_const(0);
                        self.op(OP_I64_LT_S);
                        self.local_set(ovf_loc);
                    }
                    Op::UmulOverflow => {
                        let hi = self.temp_i64(6);
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I64_MUL);
                        self.local_set(res_loc);
                        self.emit_umulhi64_to(la, lb, hi);
                        self.local_get(hi);
                        self.i64_const(0);
                        self.op(OP_I64_NE);
                        self.local_set(ovf_loc);
                    }
                    Op::SmulOverflow => {
                        let hi = self.temp_i64(6);
                        self.local_get(la);
                        self.local_get(lb);
                        self.op(OP_I64_MUL);
                        self.local_set(res_loc);
                        self.emit_smulhi64_to(la, lb, hi);
                        self.local_get(hi);
                        self.local_get(res_loc);
                        self.i64_const(63);
                        self.op(OP_I64_SHR_S);
                        self.op(OP_I64_NE);
                        self.local_set(ovf_loc);
                    }
                    _ => unreachable!(),
                }
            }
            Scalar::I128 => {
                let (a_lo, a_hi) = self.loc_pair(a);
                let (b_lo, b_hi) = self.loc_pair(b);
                let (r_lo, r_hi, ovf_loc) = (dst_first, dst_first + 1, dst_first + 2);
                match op {
                    Op::UaddOverflow => {
                        let carry = self.temp_i64(0);
                        let s1 = self.temp_i64(1);
                        self.local_get(a_lo);
                        self.local_get(b_lo);
                        self.op(OP_I64_ADD);
                        self.local_tee(r_lo);
                        self.local_get(a_lo);
                        self.op(OP_I64_LT_U);
                        self.op(OP_I64_EXTEND_I32_U);
                        self.local_set(carry);

                        self.local_get(a_hi);
                        self.local_get(b_hi);
                        self.op(OP_I64_ADD);
                        self.local_tee(s1);
                        self.local_get(carry);
                        self.op(OP_I64_ADD);
                        self.local_set(r_hi);

                        self.local_get(s1);
                        self.local_get(a_hi);
                        self.op(OP_I64_LT_U);
                        self.local_get(r_hi);
                        self.local_get(s1);
                        self.op(OP_I64_LT_U);
                        self.op(OP_I32_OR);
                        self.local_set(ovf_loc);
                    }
                    Op::UsubOverflow => {
                        let borrow = self.temp_i64(0);
                        let d1 = self.temp_i64(1);
                        self.local_get(a_lo);
                        self.local_get(b_lo);
                        self.op(OP_I64_SUB);
                        self.local_set(r_lo);
                        self.local_get(a_lo);
                        self.local_get(b_lo);
                        self.op(OP_I64_LT_U);
                        self.op(OP_I64_EXTEND_I32_U);
                        self.local_set(borrow);

                        self.local_get(a_hi);
                        self.local_get(b_hi);
                        self.op(OP_I64_SUB);
                        self.local_tee(d1);
                        self.local_get(borrow);
                        self.op(OP_I64_SUB);
                        self.local_set(r_hi);

                        self.local_get(a_hi);
                        self.local_get(b_hi);
                        self.op(OP_I64_LT_U);
                        self.local_get(d1);
                        self.local_get(borrow);
                        self.op(OP_I64_LT_U);
                        self.op(OP_I32_OR);
                        self.local_set(ovf_loc);
                    }
                    Op::SaddOverflow => {
                        self.local_get(a_lo);
                        self.local_get(b_lo);
                        self.op(OP_I64_ADD);
                        self.local_tee(r_lo);
                        self.local_get(a_lo);
                        self.op(OP_I64_LT_U);
                        self.op(OP_I64_EXTEND_I32_U);
                        self.local_get(a_hi);
                        self.local_get(b_hi);
                        self.op(OP_I64_ADD);
                        self.op(OP_I64_ADD);
                        self.local_set(r_hi);

                        self.local_get(a_hi);
                        self.local_get(r_hi);
                        self.op(OP_I64_XOR);
                        self.local_get(b_hi);
                        self.local_get(r_hi);
                        self.op(OP_I64_XOR);
                        self.op(OP_I64_AND);
                        self.i64_const(0);
                        self.op(OP_I64_LT_S);
                        self.local_set(ovf_loc);
                    }
                    Op::SsubOverflow => {
                        self.local_get(a_lo);
                        self.local_get(b_lo);
                        self.op(OP_I64_SUB);
                        self.local_set(r_lo);
                        self.local_get(a_hi);
                        self.local_get(b_hi);
                        self.op(OP_I64_SUB);
                        self.local_get(a_lo);
                        self.local_get(b_lo);
                        self.op(OP_I64_LT_U);
                        self.op(OP_I64_EXTEND_I32_U);
                        self.op(OP_I64_SUB);
                        self.local_set(r_hi);

                        self.local_get(a_hi);
                        self.local_get(b_hi);
                        self.op(OP_I64_XOR);
                        self.local_get(a_hi);
                        self.local_get(r_hi);
                        self.op(OP_I64_XOR);
                        self.op(OP_I64_AND);
                        self.i64_const(0);
                        self.op(OP_I64_LT_S);
                        self.local_set(ovf_loc);
                    }
                    _ => return Err(LirWasm64Error::UnsupportedOp { func: self.place, inst, op }),
                }
            }
            _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty: a_ty }),
        }
        Ok(())
    }

    // ---- Integer unary ops ------------------------------------------------------------------

    fn emit_int_unary(
        &mut self,
        inst: usize,
        op: Op,
        ty: Ty,
        a: Value,
    ) -> Result<(), LirWasm64Error> {
        let scalar = ty.elem();
        let dst = self.val_locals[inst].first;

        match scalar {
            Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 => {
                let la = self.loc(a);
                let bits = sub32_bits(scalar);
                match op {
                    Op::Neg => {
                        self.i32_const(0);
                        self.local_get(la);
                        self.op(OP_I32_SUB);
                        self.mask_sub32(scalar);
                    }
                    Op::Not => {
                        self.local_get(la);
                        self.i32_const(sub32_mask(scalar).unwrap_or(u32::MAX) as i32);
                        self.op(OP_I32_XOR);
                    }
                    Op::Clz => {
                        self.local_get(la);
                        self.op(OP_I32_CLZ);
                        if bits < 32 {
                            self.i32_const((32 - bits) as i32);
                            self.op(OP_I32_SUB);
                        }
                    }
                    Op::Ctz => {
                        self.local_get(la);
                        if bits < 32 {
                            self.i32_const(1i32 << bits);
                            self.op(OP_I32_OR);
                        }
                        self.op(OP_I32_CTZ);
                    }
                    Op::Popcnt => {
                        self.local_get(la);
                        self.op(OP_I32_POPCNT);
                    }
                    Op::Bswap => match scalar {
                        Scalar::I1 | Scalar::I8 => self.local_get(la),
                        Scalar::I16 => {
                            self.local_get(la);
                            self.i32_const(0xFF);
                            self.op(OP_I32_AND);
                            self.i32_const(8);
                            self.op(OP_I32_SHL);
                            self.local_get(la);
                            self.i32_const(8);
                            self.op(OP_I32_SHR_U);
                            self.op(OP_I32_OR);
                        }
                        Scalar::I32 => {
                            let t = self.temp_i32(0);
                            self.local_get(la);
                            self.i32_const(0x00FF_00FF);
                            self.op(OP_I32_AND);
                            self.i32_const(8);
                            self.op(OP_I32_SHL);
                            self.local_get(la);
                            self.i32_const(8);
                            self.op(OP_I32_SHR_U);
                            self.i32_const(0x00FF_00FF);
                            self.op(OP_I32_AND);
                            self.op(OP_I32_OR);
                            self.local_set(t);
                            self.local_get(t);
                            self.i32_const(16);
                            self.op(OP_I32_ROTL);
                        }
                        _ => unreachable!(),
                    },
                    Op::Bitrev => {
                        let t = self.temp_i32(0);
                        self.local_get(la);
                        self.local_set(t);
                        for (mask, shift) in [
                            (0x5555_5555i32, 1i32),
                            (0x3333_3333i32, 2i32),
                            (0x0F0F_0F0Fi32, 4i32),
                            (0x00FF_00FFi32, 8i32),
                        ] {
                            self.local_get(t);
                            self.i32_const(mask);
                            self.op(OP_I32_AND);
                            self.i32_const(shift);
                            self.op(OP_I32_SHL);
                            self.local_get(t);
                            self.i32_const(shift);
                            self.op(OP_I32_SHR_U);
                            self.i32_const(mask);
                            self.op(OP_I32_AND);
                            self.op(OP_I32_OR);
                            self.local_set(t);
                        }
                        self.local_get(t);
                        self.i32_const(16);
                        self.op(OP_I32_ROTL);
                        if bits < 32 {
                            self.i32_const((32 - bits) as i32);
                            self.op(OP_I32_SHR_U);
                        }
                    }
                    _ => return Err(LirWasm64Error::UnsupportedOp { func: self.place, inst, op }),
                }
                self.local_set(dst);
            }
            Scalar::I64 => {
                let la = self.loc(a);
                match op {
                    Op::Neg => {
                        self.i64_const(0);
                        self.local_get(la);
                        self.op(OP_I64_SUB);
                    }
                    Op::Not => {
                        self.local_get(la);
                        self.i64_const(-1);
                        self.op(OP_I64_XOR);
                    }
                    Op::Clz => {
                        self.local_get(la);
                        self.op(OP_I64_CLZ);
                    }
                    Op::Ctz => {
                        self.local_get(la);
                        self.op(OP_I64_CTZ);
                    }
                    Op::Popcnt => {
                        self.local_get(la);
                        self.op(OP_I64_POPCNT);
                    }
                    Op::Bswap => {
                        let t = self.temp_i64(0);
                        self.local_get(la);
                        self.local_set(t);
                        for (mask, shift) in [
                            (0x00FF_00FF_00FF_00FFu64 as i64, 8i64),
                            (0x0000_FFFF_0000_FFFFu64 as i64, 16i64),
                        ] {
                            self.local_get(t);
                            self.i64_const(mask);
                            self.op(OP_I64_AND);
                            self.i64_const(shift);
                            self.op(OP_I64_SHL);
                            self.local_get(t);
                            self.i64_const(shift);
                            self.op(OP_I64_SHR_U);
                            self.i64_const(mask);
                            self.op(OP_I64_AND);
                            self.op(OP_I64_OR);
                            self.local_set(t);
                        }
                        self.local_get(t);
                        self.i64_const(32);
                        self.op(OP_I64_ROTL);
                    }
                    Op::Bitrev => {
                        let t = self.temp_i64(0);
                        self.local_get(la);
                        self.local_set(t);
                        for (mask, shift) in [
                            (0x5555_5555_5555_5555u64 as i64, 1i64),
                            (0x3333_3333_3333_3333u64 as i64, 2i64),
                            (0x0F0F_0F0F_0F0F_0F0Fu64 as i64, 4i64),
                            (0x00FF_00FF_00FF_00FFu64 as i64, 8i64),
                            (0x0000_FFFF_0000_FFFFu64 as i64, 16i64),
                        ] {
                            self.local_get(t);
                            self.i64_const(mask);
                            self.op(OP_I64_AND);
                            self.i64_const(shift);
                            self.op(OP_I64_SHL);
                            self.local_get(t);
                            self.i64_const(shift);
                            self.op(OP_I64_SHR_U);
                            self.i64_const(mask);
                            self.op(OP_I64_AND);
                            self.op(OP_I64_OR);
                            self.local_set(t);
                        }
                        self.local_get(t);
                        self.i64_const(32);
                        self.op(OP_I64_ROTL);
                    }
                    _ => return Err(LirWasm64Error::UnsupportedOp { func: self.place, inst, op }),
                }
                self.local_set(dst);
            }
            Scalar::I128 => {
                let (a_lo, a_hi) = self.loc_pair(a);
                let (d_lo, d_hi) = (dst, dst + 1);
                match op {
                    Op::Neg => {
                        self.i64_const(0);
                        self.local_get(a_lo);
                        self.op(OP_I64_SUB);
                        self.local_set(d_lo);
                        self.i64_const(0);
                        self.local_get(a_hi);
                        self.op(OP_I64_SUB);
                        self.local_get(a_lo);
                        self.i64_const(0);
                        self.op(OP_I64_NE);
                        self.op(OP_I64_EXTEND_I32_U);
                        self.op(OP_I64_SUB);
                        self.local_set(d_hi);
                    }
                    Op::Not => {
                        self.local_get(a_lo);
                        self.i64_const(-1);
                        self.op(OP_I64_XOR);
                        self.local_set(d_lo);
                        self.local_get(a_hi);
                        self.i64_const(-1);
                        self.op(OP_I64_XOR);
                        self.local_set(d_hi);
                    }
                    Op::Clz => {
                        self.local_get(a_lo);
                        self.op(OP_I64_CLZ);
                        self.i64_const(64);
                        self.op(OP_I64_ADD);
                        self.local_get(a_hi);
                        self.op(OP_I64_CLZ);
                        self.local_get(a_hi);
                        self.op(OP_I64_EQZ);
                        self.op(OP_SELECT);
                        self.local_set(d_lo);
                        self.i64_const(0);
                        self.local_set(d_hi);
                    }
                    Op::Ctz => {
                        self.local_get(a_hi);
                        self.op(OP_I64_CTZ);
                        self.i64_const(64);
                        self.op(OP_I64_ADD);
                        self.local_get(a_lo);
                        self.op(OP_I64_CTZ);
                        self.local_get(a_lo);
                        self.op(OP_I64_EQZ);
                        self.op(OP_SELECT);
                        self.local_set(d_lo);
                        self.i64_const(0);
                        self.local_set(d_hi);
                    }
                    Op::Popcnt => {
                        self.local_get(a_lo);
                        self.op(OP_I64_POPCNT);
                        self.local_get(a_hi);
                        self.op(OP_I64_POPCNT);
                        self.op(OP_I64_ADD);
                        self.local_set(d_lo);
                        self.i64_const(0);
                        self.local_set(d_hi);
                    }
                    _ => return Err(LirWasm64Error::UnsupportedOp { func: self.place, inst, op }),
                }
            }
            _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty }),
        }
        Ok(())
    }

    // ---- Comparisons ------------------------------------------------------------------------

    fn emit_icmp(
        &mut self,
        inst: usize,
        cond: IntCond,
        a: Value,
        b: Value,
    ) -> Result<(), LirWasm64Error> {
        let ty = self.view.ty(a);
        let dst = self.val_locals[inst].first;
        match ty.elem() {
            Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 => {
                let (la, lb) = (self.loc(a), self.loc(b));
                let signed = matches!(
                    cond,
                    IntCond::Slt | IntCond::Sle | IntCond::Sgt | IntCond::Sge
                );
                self.local_get(la);
                if signed {
                    self.sext_to_i32(ty.elem());
                }
                self.local_get(lb);
                if signed {
                    self.sext_to_i32(ty.elem());
                }
                self.op(match cond {
                    IntCond::Eq => OP_I32_EQ,
                    IntCond::Ne => OP_I32_NE,
                    IntCond::Ult => OP_I32_LT_U,
                    IntCond::Ule => OP_I32_LE_U,
                    IntCond::Ugt => OP_I32_GT_U,
                    IntCond::Uge => OP_I32_GE_U,
                    IntCond::Slt => OP_I32_LT_S,
                    IntCond::Sle => OP_I32_LE_S,
                    IntCond::Sgt => OP_I32_GT_S,
                    IntCond::Sge => OP_I32_GE_S,
                });
                self.local_set(dst);
            }
            Scalar::I64 | Scalar::Ptr => {
                let (la, lb) = (self.loc(a), self.loc(b));
                self.local_get(la);
                self.local_get(lb);
                self.op(match cond {
                    IntCond::Eq => OP_I64_EQ,
                    IntCond::Ne => OP_I64_NE,
                    IntCond::Ult => OP_I64_LT_U,
                    IntCond::Ule => OP_I64_LE_U,
                    IntCond::Ugt => OP_I64_GT_U,
                    IntCond::Uge => OP_I64_GE_U,
                    IntCond::Slt => OP_I64_LT_S,
                    IntCond::Sle => OP_I64_LE_S,
                    IntCond::Sgt => OP_I64_GT_S,
                    IntCond::Sge => OP_I64_GE_S,
                });
                self.local_set(dst);
            }
            Scalar::I128 => {
                let (a_lo, a_hi) = self.loc_pair(a);
                let (b_lo, b_hi) = self.loc_pair(b);
                self.push_i128_cmp(cond, a_lo, a_hi, b_lo, b_hi);
                self.local_set(dst);
            }
            _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty }),
        }
        Ok(())
    }

    fn push_i128_cmp(&mut self, cond: IntCond, a_lo: u32, a_hi: u32, b_lo: u32, b_hi: u32) {
        match cond {
            IntCond::Eq => {
                self.local_get(a_lo);
                self.local_get(b_lo);
                self.op(OP_I64_EQ);
                self.local_get(a_hi);
                self.local_get(b_hi);
                self.op(OP_I64_EQ);
                self.op(OP_I32_AND);
            }
            IntCond::Ne => {
                self.local_get(a_lo);
                self.local_get(b_lo);
                self.op(OP_I64_NE);
                self.local_get(a_hi);
                self.local_get(b_hi);
                self.op(OP_I64_NE);
                self.op(OP_I32_OR);
            }
            IntCond::Ult
            | IntCond::Ule
            | IntCond::Ugt
            | IntCond::Uge
            | IntCond::Slt
            | IntCond::Sle
            | IntCond::Sgt
            | IntCond::Sge => {
                let (hi_op, lo_op) = match cond {
                    IntCond::Ult => (OP_I64_LT_U, OP_I64_LT_U),
                    IntCond::Ule => (OP_I64_LT_U, OP_I64_LE_U),
                    IntCond::Ugt => (OP_I64_GT_U, OP_I64_GT_U),
                    IntCond::Uge => (OP_I64_GT_U, OP_I64_GE_U),
                    IntCond::Slt => (OP_I64_LT_S, OP_I64_LT_U),
                    IntCond::Sle => (OP_I64_LT_S, OP_I64_LE_U),
                    IntCond::Sgt => (OP_I64_GT_S, OP_I64_GT_U),
                    _ => (OP_I64_GT_S, OP_I64_GE_U),
                };
                // (a_hi hi_op b_hi) | ((a_hi == b_hi) & (a_lo lo_op b_lo))
                self.local_get(a_hi);
                self.local_get(b_hi);
                self.op(hi_op);
                self.local_get(a_hi);
                self.local_get(b_hi);
                self.op(OP_I64_EQ);
                self.local_get(a_lo);
                self.local_get(b_lo);
                self.op(lo_op);
                self.op(OP_I32_AND);
                self.op(OP_I32_OR);
            }
        }
    }

    fn emit_fcmp(
        &mut self,
        inst: usize,
        cond: FloatCond,
        a: Value,
        b: Value,
    ) -> Result<(), LirWasm64Error> {
        let ty = self.view.ty(a);
        let (la, lb) = (self.loc(a), self.loc(b));
        let dst = self.val_locals[inst].first;
        let (eq, ne, lt, gt, le, ge) = match ty.elem() {
            Scalar::F32 => (OP_F32_EQ, OP_F32_NE, OP_F32_LT, OP_F32_GT, OP_F32_LE, OP_F32_GE),
            Scalar::F64 => (OP_F64_EQ, OP_F64_NE, OP_F64_LT, OP_F64_GT, OP_F64_LE, OP_F64_GE),
            _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty }),
        };

        match cond {
            FloatCond::False => self.i32_const(0),
            FloatCond::True => self.i32_const(1),
            FloatCond::Oeq => {
                self.local_get(la);
                self.local_get(lb);
                self.op(eq);
            }
            FloatCond::Ogt => {
                self.local_get(la);
                self.local_get(lb);
                self.op(gt);
            }
            FloatCond::Oge => {
                self.local_get(la);
                self.local_get(lb);
                self.op(ge);
            }
            FloatCond::Olt => {
                self.local_get(la);
                self.local_get(lb);
                self.op(lt);
            }
            FloatCond::Ole => {
                self.local_get(la);
                self.local_get(lb);
                self.op(le);
            }
            FloatCond::One => {
                self.local_get(la);
                self.local_get(lb);
                self.op(lt);
                self.local_get(la);
                self.local_get(lb);
                self.op(gt);
                self.op(OP_I32_OR);
            }
            FloatCond::Ord => {
                self.local_get(la);
                self.local_get(la);
                self.op(eq);
                self.local_get(lb);
                self.local_get(lb);
                self.op(eq);
                self.op(OP_I32_AND);
            }
            FloatCond::Uno => {
                self.local_get(la);
                self.local_get(la);
                self.op(ne);
                self.local_get(lb);
                self.local_get(lb);
                self.op(ne);
                self.op(OP_I32_OR);
            }
            FloatCond::Ueq => {
                self.local_get(la);
                self.local_get(lb);
                self.op(lt);
                self.local_get(la);
                self.local_get(lb);
                self.op(gt);
                self.op(OP_I32_OR);
                self.op(OP_I32_EQZ);
            }
            FloatCond::Ugt => {
                self.local_get(la);
                self.local_get(lb);
                self.op(le);
                self.op(OP_I32_EQZ);
            }
            FloatCond::Uge => {
                self.local_get(la);
                self.local_get(lb);
                self.op(lt);
                self.op(OP_I32_EQZ);
            }
            FloatCond::Ult => {
                self.local_get(la);
                self.local_get(lb);
                self.op(ge);
                self.op(OP_I32_EQZ);
            }
            FloatCond::Ule => {
                self.local_get(la);
                self.local_get(lb);
                self.op(gt);
                self.op(OP_I32_EQZ);
            }
            FloatCond::Une => {
                self.local_get(la);
                self.local_get(lb);
                self.op(ne);
            }
        }
        self.local_set(dst);
        Ok(())
    }

    // ---- Conversions ------------------------------------------------------------------------

    fn emit_zext(&mut self, inst: usize, to_ty: Ty, a: Value) -> Result<(), LirWasm64Error> {
        let from_ty = self.view.ty(a);
        let dst = self.val_locals[inst].first;
        match (from_ty.elem(), to_ty.elem()) {
            (
                Scalar::I1 | Scalar::I8 | Scalar::I16,
                Scalar::I8 | Scalar::I16 | Scalar::I32,
            ) => {
                // Already canonically zero-extended in i32.
                self.local_get(self.loc(a));
                self.local_set(dst);
            }
            (Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32, Scalar::I64) => {
                self.local_get(self.loc(a));
                self.op(OP_I64_EXTEND_I32_U);
                self.local_set(dst);
            }
            (Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32, Scalar::I128) => {
                self.local_get(self.loc(a));
                self.op(OP_I64_EXTEND_I32_U);
                self.local_set(dst);
                self.i64_const(0);
                self.local_set(dst + 1);
            }
            (Scalar::I64, Scalar::I128) => {
                self.local_get(self.loc(a));
                self.local_set(dst);
                self.i64_const(0);
                self.local_set(dst + 1);
            }
            _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty: to_ty }),
        }
        Ok(())
    }

    fn emit_sext(&mut self, inst: usize, to_ty: Ty, a: Value) -> Result<(), LirWasm64Error> {
        let from_ty = self.view.ty(a);
        let dst = self.val_locals[inst].first;
        match (from_ty.elem(), to_ty.elem()) {
            (
                Scalar::I1 | Scalar::I8 | Scalar::I16,
                Scalar::I8 | Scalar::I16 | Scalar::I32,
            ) => {
                self.local_get(self.loc(a));
                self.sext_to_i32(from_ty.elem());
                self.mask_sub32(to_ty.elem());
                self.local_set(dst);
            }
            (Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32, Scalar::I64) => {
                self.local_get(self.loc(a));
                self.sext_to_i32(from_ty.elem());
                self.op(OP_I64_EXTEND_I32_S);
                self.local_set(dst);
            }
            (Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32, Scalar::I128) => {
                self.local_get(self.loc(a));
                self.sext_to_i32(from_ty.elem());
                self.op(OP_I64_EXTEND_I32_S);
                self.local_tee(dst);
                self.i64_const(63);
                self.op(OP_I64_SHR_S);
                self.local_set(dst + 1);
            }
            (Scalar::I64, Scalar::I128) => {
                self.local_get(self.loc(a));
                self.local_tee(dst);
                self.i64_const(63);
                self.op(OP_I64_SHR_S);
                self.local_set(dst + 1);
            }
            _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty: to_ty }),
        }
        Ok(())
    }

    fn emit_trunc(&mut self, inst: usize, to_ty: Ty, a: Value) -> Result<(), LirWasm64Error> {
        let from_ty = self.view.ty(a);
        let dst = self.val_locals[inst].first;
        match (from_ty.elem(), to_ty.elem()) {
            (Scalar::I8 | Scalar::I16 | Scalar::I32, Scalar::I1 | Scalar::I8 | Scalar::I16) => {
                self.local_get(self.loc(a));
                self.mask_sub32(to_ty.elem());
                self.local_set(dst);
            }
            (Scalar::I64 | Scalar::I128, Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32) => {
                self.local_get(self.loc(a)); // low 64 bits for I128
                self.op(OP_I32_WRAP_I64);
                self.mask_sub32(to_ty.elem());
                self.local_set(dst);
            }
            (Scalar::I128, Scalar::I64) => {
                self.local_get(self.loc(a)); // low 64 bits
                self.local_set(dst);
            }
            _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty: to_ty }),
        }
        Ok(())
    }

    fn emit_fp_to_int(
        &mut self,
        inst: usize,
        signed: bool,
        to_ty: Ty,
        a: Value,
    ) -> Result<(), LirWasm64Error> {
        let from_ty = self.view.ty(a);
        let la = self.loc(a);
        let dst = self.val_locals[inst].first;
        match (from_ty.elem(), to_ty.elem()) {
            (
                Scalar::F32 | Scalar::F64,
                Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32,
            ) => {
                let sub = match (from_ty.elem(), signed) {
                    (Scalar::F32, true) => FC_I32_TRUNC_SAT_F32_S,
                    (Scalar::F32, false) => FC_I32_TRUNC_SAT_F32_U,
                    (Scalar::F64, true) => FC_I32_TRUNC_SAT_F64_S,
                    (Scalar::F64, false) => FC_I32_TRUNC_SAT_F64_U,
                    _ => unreachable!(),
                };
                self.local_get(la);
                self.fc_op(sub);
                if let Some(mask) = sub32_mask(to_ty.elem()) {
                    let t = self.temp_i32(0);
                    self.local_set(t);
                    if signed {
                        let lo = sub32_smin(to_ty.elem());
                        let hi = sub32_smax(to_ty.elem());
                        self.i32_const(lo);
                        self.local_get(t);
                        self.local_get(t);
                        self.i32_const(lo);
                        self.op(OP_I32_LT_S);
                        self.op(OP_SELECT);
                        self.local_set(t);
                        self.i32_const(hi);
                        self.local_get(t);
                        self.local_get(t);
                        self.i32_const(hi);
                        self.op(OP_I32_GT_S);
                        self.op(OP_SELECT);
                        self.i32_const(mask as i32);
                        self.op(OP_I32_AND);
                    } else {
                        self.i32_const(mask as i32);
                        self.local_get(t);
                        self.local_get(t);
                        self.i32_const(mask as i32);
                        self.op(OP_I32_GT_U);
                        self.op(OP_SELECT);
                    }
                }
                self.local_set(dst);
            }
            (Scalar::F32 | Scalar::F64, Scalar::I64) => {
                let sub = match (from_ty.elem(), signed) {
                    (Scalar::F32, true) => FC_I64_TRUNC_SAT_F32_S,
                    (Scalar::F32, false) => FC_I64_TRUNC_SAT_F32_U,
                    (Scalar::F64, true) => FC_I64_TRUNC_SAT_F64_S,
                    (Scalar::F64, false) => FC_I64_TRUNC_SAT_F64_U,
                    _ => unreachable!(),
                };
                self.local_get(la);
                self.fc_op(sub);
                self.local_set(dst);
            }
            _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty: to_ty }),
        }
        Ok(())
    }

    fn emit_int_to_fp(
        &mut self,
        inst: usize,
        signed: bool,
        to_ty: Ty,
        a: Value,
    ) -> Result<(), LirWasm64Error> {
        let from_ty = self.view.ty(a);
        let la = self.loc(a);
        let dst = self.val_locals[inst].first;
        match (from_ty.elem(), to_ty.elem()) {
            (
                Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32,
                Scalar::F32 | Scalar::F64,
            ) => {
                self.local_get(la);
                if signed {
                    self.sext_to_i32(from_ty.elem());
                }
                self.op(match (to_ty.elem(), signed) {
                    (Scalar::F32, true) => OP_F32_CONVERT_I32_S,
                    (Scalar::F32, false) => OP_F32_CONVERT_I32_U,
                    (Scalar::F64, true) => OP_F64_CONVERT_I32_S,
                    (Scalar::F64, false) => OP_F64_CONVERT_I32_U,
                    _ => unreachable!(),
                });
                self.local_set(dst);
            }
            (Scalar::I64, Scalar::F32 | Scalar::F64) => {
                self.local_get(la);
                self.op(match (to_ty.elem(), signed) {
                    (Scalar::F32, true) => OP_F32_CONVERT_I64_S,
                    (Scalar::F32, false) => OP_F32_CONVERT_I64_U,
                    (Scalar::F64, true) => OP_F64_CONVERT_I64_S,
                    (Scalar::F64, false) => OP_F64_CONVERT_I64_U,
                    _ => unreachable!(),
                });
                self.local_set(dst);
            }
            _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty: to_ty }),
        }
        Ok(())
    }

    // ---- Floating-point ops -----------------------------------------------------------------

    fn emit_float_binary(
        &mut self,
        inst: usize,
        op: Op,
        ty: Ty,
        a: Value,
        b: Value,
    ) -> Result<(), LirWasm64Error> {
        let (la, lb) = (self.loc(a), self.loc(b));
        let dst = self.val_locals[inst].first;
        let is_f64 = match ty.elem() {
            Scalar::F32 => false,
            Scalar::F64 => true,
            _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty }),
        };

        match op {
            Op::Fadd | Op::Fsub | Op::Fmul | Op::Fdiv | Op::Fcopysign | Op::Fminimum | Op::Fmaximum => {
                self.local_get(la);
                self.local_get(lb);
                let wop = match (op, is_f64) {
                    (Op::Fadd, false) => OP_F32_ADD,
                    (Op::Fadd, true) => OP_F64_ADD,
                    (Op::Fsub, false) => OP_F32_SUB,
                    (Op::Fsub, true) => OP_F64_SUB,
                    (Op::Fmul, false) => OP_F32_MUL,
                    (Op::Fmul, true) => OP_F64_MUL,
                    (Op::Fdiv, false) => OP_F32_DIV,
                    (Op::Fdiv, true) => OP_F64_DIV,
                    (Op::Fcopysign, false) => OP_F32_COPYSIGN,
                    (Op::Fcopysign, true) => OP_F64_COPYSIGN,
                    (Op::Fminimum, false) => OP_F32_MIN,
                    (Op::Fminimum, true) => OP_F64_MIN,
                    (Op::Fmaximum, false) => OP_F32_MAX,
                    (Op::Fmaximum, true) => OP_F64_MAX,
                    _ => unreachable!(),
                };
                self.op(wop);
            }
            Op::FminNum | Op::FmaxNum => {
                // IEEE 754-2008 minNum/maxNum: if a is NaN -> b; else if b is NaN -> a; else min/max.
                let (ne_op, mm_op) = match (op, is_f64) {
                    (Op::FminNum, false) => (OP_F32_NE, OP_F32_MIN),
                    (Op::FminNum, true) => (OP_F64_NE, OP_F64_MIN),
                    (Op::FmaxNum, false) => (OP_F32_NE, OP_F32_MAX),
                    (Op::FmaxNum, true) => (OP_F64_NE, OP_F64_MAX),
                    _ => unreachable!(),
                };
                // select(b, select(a, mm(a, b), b != b), a != a)
                self.local_get(lb);
                self.local_get(la);
                self.local_get(la);
                self.local_get(lb);
                self.op(mm_op);
                self.local_get(lb);
                self.local_get(lb);
                self.op(ne_op);
                self.op(OP_SELECT);
                self.local_get(la);
                self.local_get(la);
                self.op(ne_op);
                self.op(OP_SELECT);
            }
            _ => return Err(LirWasm64Error::UnsupportedOp { func: self.place, inst, op }),
        }
        self.local_set(dst);
        Ok(())
    }

    fn emit_float_unary(
        &mut self,
        inst: usize,
        op: Op,
        ty: Ty,
        a: Value,
    ) -> Result<(), LirWasm64Error> {
        let la = self.loc(a);
        let dst = self.val_locals[inst].first;
        let is_f64 = match ty.elem() {
            Scalar::F32 => false,
            Scalar::F64 => true,
            _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty }),
        };

        if op == Op::Fround {
            // Ties away from zero: t = trunc(a); if abs(a - t) >= 0.5 { t + copysign(1.0, a) } else { t }
            let t = if is_f64 { self.temp_f64(0) } else { self.temp_f32(0) };
            self.local_get(la);
            self.op(if is_f64 { OP_F64_TRUNC } else { OP_F32_TRUNC });
            self.local_set(t);

            // val1 (if >= 0.5): t + copysign(1.0, a)
            self.local_get(t);
            if is_f64 {
                self.f64_const_bits(1.0f64.to_bits());
            } else {
                self.f32_const_bits(1.0f32.to_bits());
            }
            self.local_get(la);
            self.op(if is_f64 { OP_F64_COPYSIGN } else { OP_F32_COPYSIGN });
            self.op(if is_f64 { OP_F64_ADD } else { OP_F32_ADD });

            // val2: t
            self.local_get(t);

            // cond: abs(a - t) >= 0.5
            self.local_get(la);
            self.local_get(t);
            self.op(if is_f64 { OP_F64_SUB } else { OP_F32_SUB });
            self.op(if is_f64 { OP_F64_ABS } else { OP_F32_ABS });
            if is_f64 {
                self.f64_const_bits(0.5f64.to_bits());
                self.op(OP_F64_GE);
            } else {
                self.f32_const_bits(0.5f32.to_bits());
                self.op(OP_F32_GE);
            }
            self.op(OP_SELECT);
            self.local_set(dst);
            return Ok(());
        }

        self.local_get(la);
        let wop = match (op, is_f64) {
            (Op::Fneg, false) => OP_F32_NEG,
            (Op::Fneg, true) => OP_F64_NEG,
            (Op::Fabs, false) => OP_F32_ABS,
            (Op::Fabs, true) => OP_F64_ABS,
            (Op::Sqrt, false) => OP_F32_SQRT,
            (Op::Sqrt, true) => OP_F64_SQRT,
            (Op::Floor, false) => OP_F32_FLOOR,
            (Op::Floor, true) => OP_F64_FLOOR,
            (Op::Ceil, false) => OP_F32_CEIL,
            (Op::Ceil, true) => OP_F64_CEIL,
            (Op::Ftrunc, false) => OP_F32_TRUNC,
            (Op::Ftrunc, true) => OP_F64_TRUNC,
            (Op::Nearest, false) => OP_F32_NEAREST,
            (Op::Nearest, true) => OP_F64_NEAREST,
            _ => return Err(LirWasm64Error::UnsupportedOp { func: self.place, inst, op }),
        };
        self.op(wop);
        self.local_set(dst);
        Ok(())
    }

    // ---- Memory ops -------------------------------------------------------------------------

    fn emit_effective_addr(&mut self, ptr_loc: u32, offset: i32) -> u64 {
        self.local_get(ptr_loc);
        if offset >= 0 {
            offset as u64
        } else {
            self.i64_const(i64::from(offset));
            self.op(OP_I64_ADD);
            0
        }
    }

    fn emit_load(
        &mut self,
        inst: usize,
        ty: Ty,
        ptr_loc: u32,
        offset: i32,
        align_hint: u32,
        dst_first: u32,
    ) -> Result<(), LirWasm64Error> {
        match ty.elem() {
            Scalar::I1 => {
                let mem_off = self.emit_effective_addr(ptr_loc, offset);
                self.op(OP_I32_LOAD8_U);
                self.memarg(0, mem_off);
                self.i32_const(1);
                self.op(OP_I32_AND);
                self.local_set(dst_first);
            }
            Scalar::I8 => {
                let mem_off = self.emit_effective_addr(ptr_loc, offset);
                self.op(OP_I32_LOAD8_U);
                self.memarg(0, mem_off);
                self.local_set(dst_first);
            }
            Scalar::I16 => {
                let mem_off = self.emit_effective_addr(ptr_loc, offset);
                self.op(OP_I32_LOAD16_U);
                self.memarg(align_hint.min(1), mem_off);
                self.local_set(dst_first);
            }
            Scalar::I32 => {
                let mem_off = self.emit_effective_addr(ptr_loc, offset);
                self.op(OP_I32_LOAD);
                self.memarg(align_hint.min(2), mem_off);
                self.local_set(dst_first);
            }
            Scalar::I64 | Scalar::Ptr => {
                let mem_off = self.emit_effective_addr(ptr_loc, offset);
                self.op(OP_I64_LOAD);
                self.memarg(align_hint.min(3), mem_off);
                self.local_set(dst_first);
            }
            Scalar::F32 => {
                let mem_off = self.emit_effective_addr(ptr_loc, offset);
                self.op(OP_F32_LOAD);
                self.memarg(align_hint.min(2), mem_off);
                self.local_set(dst_first);
            }
            Scalar::F64 => {
                let mem_off = self.emit_effective_addr(ptr_loc, offset);
                self.op(OP_F64_LOAD);
                self.memarg(align_hint.min(3), mem_off);
                self.local_set(dst_first);
            }
            Scalar::I128 => {
                let align = align_hint.min(3);
                if offset >= 0 {
                    let mem_off = offset as u64;
                    self.local_get(ptr_loc);
                    self.op(OP_I64_LOAD);
                    self.memarg(align, mem_off);
                    self.local_set(dst_first);
                    self.local_get(ptr_loc);
                    self.op(OP_I64_LOAD);
                    self.memarg(align, mem_off + 8);
                    self.local_set(dst_first + 1);
                } else {
                    let base = self.temp_i64(0);
                    self.local_get(ptr_loc);
                    self.i64_const(i64::from(offset));
                    self.op(OP_I64_ADD);
                    self.local_set(base);
                    self.local_get(base);
                    self.op(OP_I64_LOAD);
                    self.memarg(align, 0);
                    self.local_set(dst_first);
                    self.local_get(base);
                    self.op(OP_I64_LOAD);
                    self.memarg(align, 8);
                    self.local_set(dst_first + 1);
                }
            }
            _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty }),
        }
        Ok(())
    }

    fn emit_store(
        &mut self,
        inst: usize,
        ty: Ty,
        val_first: u32,
        ptr_loc: u32,
        offset: i32,
        align_hint: u32,
    ) -> Result<(), LirWasm64Error> {
        match ty.elem() {
            Scalar::I1 | Scalar::I8 => {
                let mem_off = self.emit_effective_addr(ptr_loc, offset);
                self.local_get(val_first);
                self.op(OP_I32_STORE8);
                self.memarg(0, mem_off);
            }
            Scalar::I16 => {
                let mem_off = self.emit_effective_addr(ptr_loc, offset);
                self.local_get(val_first);
                self.op(OP_I32_STORE16);
                self.memarg(align_hint.min(1), mem_off);
            }
            Scalar::I32 => {
                let mem_off = self.emit_effective_addr(ptr_loc, offset);
                self.local_get(val_first);
                self.op(OP_I32_STORE);
                self.memarg(align_hint.min(2), mem_off);
            }
            Scalar::I64 | Scalar::Ptr => {
                let mem_off = self.emit_effective_addr(ptr_loc, offset);
                self.local_get(val_first);
                self.op(OP_I64_STORE);
                self.memarg(align_hint.min(3), mem_off);
            }
            Scalar::F32 => {
                let mem_off = self.emit_effective_addr(ptr_loc, offset);
                self.local_get(val_first);
                self.op(OP_F32_STORE);
                self.memarg(align_hint.min(2), mem_off);
            }
            Scalar::F64 => {
                let mem_off = self.emit_effective_addr(ptr_loc, offset);
                self.local_get(val_first);
                self.op(OP_F64_STORE);
                self.memarg(align_hint.min(3), mem_off);
            }
            Scalar::I128 => {
                let align = align_hint.min(3);
                if offset >= 0 {
                    let mem_off = offset as u64;
                    self.local_get(ptr_loc);
                    self.local_get(val_first);
                    self.op(OP_I64_STORE);
                    self.memarg(align, mem_off);
                    self.local_get(ptr_loc);
                    self.local_get(val_first + 1);
                    self.op(OP_I64_STORE);
                    self.memarg(align, mem_off + 8);
                } else {
                    let base = self.temp_i64(0);
                    self.local_get(ptr_loc);
                    self.i64_const(i64::from(offset));
                    self.op(OP_I64_ADD);
                    self.local_set(base);
                    self.local_get(base);
                    self.local_get(val_first);
                    self.op(OP_I64_STORE);
                    self.memarg(align, 0);
                    self.local_get(base);
                    self.local_get(val_first + 1);
                    self.op(OP_I64_STORE);
                    self.memarg(align, 8);
                }
            }
            _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty }),
        }
        Ok(())
    }

    /// Inline byte-compare loop returning `-1`, `0`, or `+1` (`i32`) matching `blaze_lir::eval`.
    fn emit_memcmp(&mut self, inst: usize, a: Value, b: Value, len: Value) {
        let pa = self.temp_i64(0);
        let pb = self.temp_i64(1);
        let rem = self.temp_i64(2);
        let ba = self.temp_i32(0);
        let bb = self.temp_i32(1);
        let res = self.val_locals[inst].first;

        self.local_get(self.loc(a));
        self.local_set(pa);
        self.local_get(self.loc(b));
        self.local_set(pb);
        self.push_zext_to_i64(len);
        self.local_set(rem);
        self.i32_const(0);
        self.local_set(res);

        self.op(OP_BLOCK);
        self.code.push(BLOCKTYPE_VOID);
        self.op(OP_LOOP);
        self.code.push(BLOCKTYPE_VOID);

        self.local_get(rem);
        self.op(OP_I64_EQZ);
        self.br_if(1); // break $done

        self.local_get(pa);
        self.op(OP_I32_LOAD8_U);
        self.memarg(0, 0);
        self.local_tee(ba);

        self.local_get(pb);
        self.op(OP_I32_LOAD8_U);
        self.memarg(0, 0);
        self.local_tee(bb);

        self.op(OP_I32_NE);
        self.op(OP_IF);
        self.code.push(BLOCKTYPE_VOID);
        self.i32_const(-1);
        self.i32_const(1);
        self.local_get(ba);
        self.local_get(bb);
        self.op(OP_I32_LT_U);
        self.op(OP_SELECT);
        self.local_set(res);
        self.br(2); // break $done
        self.op(OP_END);

        self.local_get(pa);
        self.i64_const(1);
        self.op(OP_I64_ADD);
        self.local_set(pa);

        self.local_get(pb);
        self.i64_const(1);
        self.op(OP_I64_ADD);
        self.local_set(pb);

        self.local_get(rem);
        self.i64_const(1);
        self.op(OP_I64_SUB);
        self.local_set(rem);

        self.br(0); // continue $cmp
        self.op(OP_END); // end $cmp
        self.op(OP_END); // end $done
    }

    fn emit_atomic_rmw(
        &mut self,
        inst: usize,
        rmw: AtomicRmw,
        ty: Ty,
        ptr: Value,
        operand: Value,
    ) -> Result<(), LirWasm64Error> {
        let align_hint = self.view.mem_attrs(inst).map_or(0, |a| u32::from(a.align));
        let ptr_loc = self.loc(ptr);
        let op_loc = self.loc(operand);
        let old_loc = self.val_locals[inst].first;

        self.emit_load(inst, ty, ptr_loc, 0, align_hint, old_loc)?;

        let scalar = ty.elem();
        match scalar {
            Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 => {
                let next_loc = self.temp_i32(0);
                match rmw {
                    AtomicRmw::Xchg => self.local_get(op_loc),
                    AtomicRmw::Add => {
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.op(OP_I32_ADD);
                        self.mask_sub32(scalar);
                    }
                    AtomicRmw::Sub => {
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.op(OP_I32_SUB);
                        self.mask_sub32(scalar);
                    }
                    AtomicRmw::And => {
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.op(OP_I32_AND);
                    }
                    AtomicRmw::Nand => {
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.op(OP_I32_AND);
                        self.i32_const(sub32_mask(scalar).unwrap_or(u32::MAX) as i32);
                        self.op(OP_I32_XOR);
                    }
                    AtomicRmw::Or => {
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.op(OP_I32_OR);
                    }
                    AtomicRmw::Xor => {
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.op(OP_I32_XOR);
                    }
                    AtomicRmw::Umin | AtomicRmw::Umax => {
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.op(if rmw == AtomicRmw::Umin { OP_I32_LT_U } else { OP_I32_GT_U });
                        self.op(OP_SELECT);
                    }
                    AtomicRmw::Min | AtomicRmw::Max => {
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.local_get(old_loc);
                        self.sext_to_i32(scalar);
                        self.local_get(op_loc);
                        self.sext_to_i32(scalar);
                        self.op(if rmw == AtomicRmw::Min { OP_I32_LT_S } else { OP_I32_GT_S });
                        self.op(OP_SELECT);
                    }
                }
                self.local_set(next_loc);
                self.emit_store(inst, ty, next_loc, ptr_loc, 0, align_hint)?;
            }
            Scalar::I64 | Scalar::Ptr => {
                let next_loc = self.temp_i64(0);
                match rmw {
                    AtomicRmw::Xchg => self.local_get(op_loc),
                    AtomicRmw::Add => {
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.op(OP_I64_ADD);
                    }
                    AtomicRmw::Sub => {
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.op(OP_I64_SUB);
                    }
                    AtomicRmw::And => {
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.op(OP_I64_AND);
                    }
                    AtomicRmw::Nand => {
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.op(OP_I64_AND);
                        self.i64_const(-1);
                        self.op(OP_I64_XOR);
                    }
                    AtomicRmw::Or => {
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.op(OP_I64_OR);
                    }
                    AtomicRmw::Xor => {
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.op(OP_I64_XOR);
                    }
                    AtomicRmw::Umin | AtomicRmw::Umax | AtomicRmw::Min | AtomicRmw::Max => {
                        let cmp = match rmw {
                            AtomicRmw::Umin => OP_I64_LT_U,
                            AtomicRmw::Umax => OP_I64_GT_U,
                            AtomicRmw::Min => OP_I64_LT_S,
                            _ => OP_I64_GT_S,
                        };
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.local_get(old_loc);
                        self.local_get(op_loc);
                        self.op(cmp);
                        self.op(OP_SELECT);
                    }
                }
                self.local_set(next_loc);
                self.emit_store(inst, ty, next_loc, ptr_loc, 0, align_hint)?;
            }
            _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty }),
        }
        Ok(())
    }

    fn emit_cmpxchg(
        &mut self,
        inst: usize,
        ptr: Value,
        expected: Value,
        new_val: Value,
    ) -> Result<(), LirWasm64Error> {
        let align_hint = self.view.mem_attrs(inst).map_or(0, |a| u32::from(a.align));
        let val_ty = self.view.ty(expected);
        let ptr_loc = self.loc(ptr);
        let dst_first = self.val_locals[inst].first;

        self.emit_load(inst, val_ty, ptr_loc, 0, align_hint, dst_first)?;

        let flag_loc = match val_ty.elem() {
            Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 => {
                self.local_get(dst_first);
                self.local_get(self.loc(expected));
                self.op(OP_I32_EQ);
                dst_first + 1
            }
            Scalar::I64 | Scalar::Ptr => {
                self.local_get(dst_first);
                self.local_get(self.loc(expected));
                self.op(OP_I64_EQ);
                dst_first + 1
            }
            Scalar::I128 => {
                let (e_lo, e_hi) = self.loc_pair(expected);
                self.push_i128_cmp(IntCond::Eq, dst_first, dst_first + 1, e_lo, e_hi);
                dst_first + 2
            }
            _ => return Err(LirWasm64Error::UnsupportedType { func: self.place, inst, ty: val_ty }),
        };

        self.local_tee(flag_loc);
        self.op(OP_IF);
        self.code.push(BLOCKTYPE_VOID);
        self.emit_store(inst, val_ty, self.loc(new_val), ptr_loc, 0, align_hint)?;
        self.op(OP_END);
        Ok(())
    }
}

// ---- Unit tests -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_lir::{
        Flags, FloatCond, IntCond, Op, Ty,
        builder::{Builder, DataReloc, Full, MemAttrs, Param, Variadic},
        func::{BlockKind, Conv, Func, RefKind, SlotKind},
    };
    use blaze_own::{Alloc, Vec};

    use super::{
        LirWasm64Error, LirWasm64Func, LirWasm64Import, LirWasm64Module, LirWasm64Static,
        compile_lir_wasm64,
    };
    use crate::wasm64::{HOST_MODULE, Wasm64Error, Wasm64Opts, check_wasm64};

    fn new_builder(params: &[Ty], rets: &[Ty]) -> Builder<Global, Full> {
        let mut b = Builder::<Global, Full>::new_in(64, Global);
        let mut p = Vec::with_capacity_in(params.len(), Alloc::global());
        for &t in params {
            p.push(Param::of(t));
        }
        let mut r = Vec::with_capacity_in(rets.len(), Alloc::global());
        for &t in rets {
            r.push(Param::of(t));
        }
        let sig = b.sig(Conv::C, Variadic::No, p.as_slice(), r.as_slice());
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        b
    }

    fn compile_and_check_deterministic(module: &LirWasm64Module<'_>) -> (Vec<u8, Alloc<'static>>, crate::wasm64::Wasm64Check) {
        let mut out1 = Vec::new_in(Alloc::global());
        let mut out2 = Vec::new_in(Alloc::global());
        let m1 = compile_lir_wasm64(module, &mut out1, Alloc::global()).expect("compile 1");
        let m2 = compile_lir_wasm64(module, &mut out2, Alloc::global()).expect("compile 2");
        assert_eq!(out1.as_slice(), out2.as_slice(), "lir_wasm64 must be byte-deterministic");
        assert_eq!(m1.identity, m2.identity);
        let checked = check_wasm64(out1.as_slice()).expect("check_wasm64 accepts compiled module");
        assert!(checked.memory64);
        assert!(checked.has_memory_export);
        assert!(checked.has_start_export);
        assert_eq!(checked.host_imports, 3);
        (out1, checked)
    }

    #[test]
    fn straight_line_arithmetic_and_bitwise() {
        let mut b = new_builder(&[], &[Ty::I32]);
        let x = b.iconst(Ty::I32, 40);
        let y = b.iconst(Ty::I32, 7);
        let sum = b.binary(Op::Add, Flags::NONE, x, y);
        let xor = b.binary(Op::Xor, Flags::NONE, sum, y);
        let one = b.iconst(Ty::I32, 1);
        let shifted = b.binary(Op::Shl, Flags::NONE, xor, one);
        let cmp = b.icmp(IntCond::Sgt, shifted, x);
        let res = b.select(Flags::NONE, cmp, sum, y);
        b.ret(&[res]);
        let func: Func<Global> = b.finish_checked().unwrap();

        let funcs = [LirWasm64Func {
            name: "_start",
            export_name: Some("_start"),
            place: 0,
            view: func.view(),
        }];
        let module = LirWasm64Module {
            name: "arith",
            funcs: &funcs,
            statics: &[],
            imports: &[],
            opts: Wasm64Opts::DEFAULT,
            profile_digest: None,
        };
        let (_, checked) = compile_and_check_deterministic(&module);
        assert_eq!(checked.func_count, 1);
        assert_eq!(checked.table_size, 0);
    }

    #[test]
    fn multi_block_loop_and_recursion() {
        // sum_to(n: i64) -> i64 using loop with block params
        let mut b = new_builder(&[Ty::I64], &[Ty::I64]);
        let n = b.param(Ty::I64);
        let header = b.create_block(BlockKind::Normal);
        let body = b.create_block(BlockKind::Normal);
        let done = b.create_block(BlockKind::Normal);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let cond = b.icmp(IntCond::Slt, i, n);
        b.br(Flags::NONE, cond, (body, &[]), (done, &[]));

        b.begin(body);
        let one = b.iconst(Ty::I64, 1);
        let next_i = b.binary(Op::Add, Flags::NONE, i, one);
        let next_acc = b.binary(Op::Add, Flags::NONE, acc, next_i);
        b.jump((header, &[next_i, next_acc]));

        b.begin(done);
        b.ret(&[acc]);
        let sum_to = b.finish_checked().unwrap();

        // recursive fact(n: i64) -> i64
        let mut b2 = Builder::<Global, Full>::new_in(64, Global);
        let fact_sig = b2.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b2.set_signature(fact_sig);
        let self_ref = b2.reference(RefKind::Func, 1);
        let entry = b2.create_block(BlockKind::Normal);
        let base = b2.create_block(BlockKind::Normal);
        let rec = b2.create_block(BlockKind::Normal);
        b2.begin(entry);
        let fn_n = b2.param(Ty::I64);
        let one = b2.iconst(Ty::I64, 1);
        let le1 = b2.icmp(IntCond::Sle, fn_n, one);
        b2.br(Flags::NONE, le1, (base, &[]), (rec, &[]));
        b2.begin(base);
        b2.ret(&[one]);
        b2.begin(rec);
        let prev = b2.binary(Op::Sub, Flags::NONE, fn_n, one);
        let sub = b2.call(Flags::NONE, self_ref, fact_sig, &[prev]);
        let prod = b2.binary(Op::Mul, Flags::NONE, fn_n, sub);
        b2.ret(&[prod]);
        let fact = b2.finish_checked().unwrap();

        // _start() -> i32 calling both
        let mut bs = Builder::<Global, Full>::new_in(64, Global);
        let start_sig = bs.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I32)]);
        let i64_sig = bs.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        bs.set_signature(start_sig);
        let r_sum = bs.reference(RefKind::Func, 0);
        let r_fact = bs.reference(RefKind::Func, 1);
        let entry_s = bs.create_block(BlockKind::Normal);
        bs.begin(entry_s);
        let five = bs.iconst(Ty::I64, 5);
        let a = bs.call(Flags::NONE, r_sum, i64_sig, &[five]);
        let c = bs.call(Flags::NONE, r_fact, i64_sig, &[five]);
        let tot = bs.binary(Op::Add, Flags::NONE, a, c);
        let out = bs.convert(Op::Trunc, Flags::NONE, Ty::I32, tot);
        bs.ret(&[out]);
        let start = bs.finish_checked().unwrap();

        let funcs = [
            LirWasm64Func { name: "sum_to", export_name: Some("sum_to"), place: 0, view: sum_to.view() },
            LirWasm64Func { name: "fact", export_name: Some("fact"), place: 1, view: fact.view() },
            LirWasm64Func { name: "_start", export_name: Some("_start"), place: 2, view: start.view() },
        ];
        let module = LirWasm64Module {
            name: "loops-and-rec",
            funcs: &funcs,
            statics: &[],
            imports: &[],
            opts: Wasm64Opts::DEFAULT,
            profile_digest: None,
        };
        let (_, checked) = compile_and_check_deterministic(&module);
        assert_eq!(checked.func_count, 3);
    }

    #[test]
    fn stack_slots_bulk_memory_and_memcmp() {
        let mut b = new_builder(&[], &[Ty::I32]);
        let s1 = b.slot(16, 3, SlotKind::Temp);
        let s2 = b.slot(16, 3, SlotKind::Temp);
        let p1 = b.slot_addr(s1);
        let p2 = b.slot_addr(s2);
        let len = b.iconst(Ty::I64, 16);
        let fill = b.iconst(Ty::I8, 0x5A);
        b.bulk(Op::Memset, Flags::NONE, p1, fill, len, MemAttrs::default());
        b.bulk(Op::Memcpy, Flags::NONE, p2, p1, len, MemAttrs::default());
        let val = b.iconst(Ty::I32, 0x1234_5678);
        b.store(Flags::NONE, val, p1, 4, MemAttrs::default());
        let loaded = b.load(Ty::I32, Flags::NONE, p1, 4, MemAttrs::default());
        let cmp = b.memcmp(p1, p2, len, MemAttrs::default());
        let combined = b.binary(Op::Add, Flags::NONE, loaded, cmp);
        b.ret(&[combined]);
        let f = b.finish_checked().unwrap();

        let funcs = [LirWasm64Func {
            name: "_start",
            export_name: Some("_start"),
            place: 0,
            view: f.view(),
        }];
        let module = LirWasm64Module {
            name: "slots-bulk",
            funcs: &funcs,
            statics: &[],
            imports: &[],
            opts: Wasm64Opts::DEFAULT,
            profile_digest: None,
        };
        compile_and_check_deterministic(&module);
    }

    #[test]
    fn switch_indirect_call_rodata_overflow_floats_and_host_log() {
        // helper(x: i64) -> i64 { x + 1 }
        let mut bh = new_builder(&[Ty::I64], &[Ty::I64]);
        let x = bh.param(Ty::I64);
        let one = bh.iconst(Ty::I64, 1);
        let y = bh.binary(Op::Add, Flags::NONE, x, one);
        bh.ret(&[y]);
        let helper = bh.finish_checked().unwrap();

        // _start() -> i32 exercising Switch, FuncAddr + CallIndirect, DataAddr (own + static),
        // UaddOverflow + Project, F64 ops, and host_log import!
        let mut b = Builder::<Global, Full>::new_in(64, Global);
        let start_sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I32)]);
        let helper_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        let log_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I32), Param::of(Ty::PTR), Param::of(Ty::I32)],
            &[],
        );
        b.set_signature(start_sig);
        let helper_ref = b.reference(RefKind::Func, 1);
        let log_ref = b.reference(RefKind::Func, 100);
        let static_ref = b.reference(RefKind::Static, 7);
        let own_data = b.data(b"own-rodata", 0, &[]);

        let entry = b.create_block(BlockKind::Normal);
        let case1 = b.create_block(BlockKind::Normal);
        let case2 = b.create_block(BlockKind::Normal);
        let def_blk = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let msg_ptr = b.data_addr(static_ref);
        let level = b.iconst(Ty::I32, 1);
        let msg_len = b.iconst(Ty::I32, 5);
        let _ = b.call(Flags::NONE, log_ref, log_sig, &[level, msg_ptr, msg_len]);

        let own_ptr = b.data_addr(own_data);
        let first_byte = b.load(Ty::I8, Flags::NONE, own_ptr, 0, MemAttrs::default());
        let tag = b.convert(Op::Zext, Flags::NONE, Ty::I32, first_byte);
        b.switch(tag, (def_blk, &[]), &[(111, (case1, &[])), (2, (case2, &[]))]);

        b.begin(case1);
        let fp = b.func_addr(helper_ref);
        let arg = b.iconst(Ty::I64, 41);
        let res = b.call_indirect(Flags::NONE, fp, helper_sig, &[arg]);
        let pair = b.overflow(Op::UaddOverflow, res, arg);
        let sum = b.project(pair, 0);
        let ovf = b.project(pair, 1);
        let ovf64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, ovf);
        let combined = b.binary(Op::Add, Flags::NONE, sum, ovf64);
        let fa = b.fconst(Ty::F64, 9.0f64.to_bits() as u128);
        let sq = b.unary(Op::Sqrt, Flags::NONE, fa);
        let fb = b.fconst(Ty::F64, 2.0f64.to_bits() as u128);
        let fmul = b.binary(Op::Fmul, Flags::NONE, sq, fb);
        let fcmp = b.fcmp(FloatCond::Ogt, fmul, fb);
        let f64_i = b.convert(Op::FptoSi, Flags::NONE, Ty::I32, fmul);
        let c32 = b.convert(Op::Trunc, Flags::NONE, Ty::I32, combined);
        let out = b.select(Flags::NONE, fcmp, c32, f64_i);
        b.ret(&[out]);

        b.begin(case2);
        let two = b.iconst(Ty::I32, 2);
        b.ret(&[two]);

        b.begin(def_blk);
        let zero = b.iconst(Ty::I32, 0);
        b.ret(&[zero]);

        let start = b.finish_checked().unwrap();

        let funcs = [
            LirWasm64Func { name: "_start", export_name: Some("_start"), place: 0, view: start.view() },
            LirWasm64Func { name: "helper", export_name: None, place: 1, view: helper.view() },
        ];
        let statics = [LirWasm64Static { place: 7, align: 8, bytes: b"hello" }];
        let imports = [LirWasm64Import { place: 100, module: HOST_MODULE, field: "host_log" }];
        let module = LirWasm64Module {
            name: "full-feature",
            funcs: &funcs,
            statics: &statics,
            imports: &imports,
            opts: Wasm64Opts::DEFAULT,
            profile_digest: Some([0x42; 32]),
        };

        let (_, checked) = compile_and_check_deterministic(&module);
        assert_eq!(checked.func_count, 2);
        assert_eq!(checked.table_size, 2, "slot 0 null + 1 indirect func");
        assert_eq!(checked.elem_count, 1);
        assert_eq!(checked.data_segments, 2, "1 static + 1 own rodata");
        assert!(checked.has_profile_section);
    }

    #[test]
    fn data_relocs_patch_static_and_func_pointers() {
        let mut bh = new_builder(&[], &[Ty::I32]);
        let v = bh.iconst(Ty::I32, 99);
        bh.ret(&[v]);
        let callee = bh.finish_checked().unwrap();

        let mut b = Builder::<Global, Full>::new_in(64, Global);
        let start_sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I32)]);
        b.set_signature(start_sig);
        let callee_ref = b.reference(RefKind::Func, 1);
        let static_ref = b.reference(RefKind::Static, 5);
        let table_data = b.data(
            &[0u8; 16],
            3,
            &[
                DataReloc { offset: 0, target: callee_ref, addend: 0 },
                DataReloc { offset: 8, target: static_ref, addend: 2 },
            ],
        );
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let tbl_ptr = b.data_addr(table_data);
        let fn_ptr = b.load(Ty::PTR, Flags::NONE, tbl_ptr, 0, MemAttrs::aligned(3));
        let res = b.call_indirect(Flags::NONE, fn_ptr, start_sig, &[]);
        b.ret(&[res]);
        let start = b.finish_checked().unwrap();

        let funcs = [
            LirWasm64Func { name: "_start", export_name: Some("_start"), place: 0, view: start.view() },
            LirWasm64Func { name: "callee", export_name: None, place: 1, view: callee.view() },
        ];
        let statics = [LirWasm64Static { place: 5, align: 8, bytes: b"abcdef" }];
        let module = LirWasm64Module {
            name: "data-relocs",
            funcs: &funcs,
            statics: &statics,
            imports: &[],
            opts: Wasm64Opts::DEFAULT,
            profile_digest: None,
        };
        let (_, checked) = compile_and_check_deterministic(&module);
        assert_eq!(checked.table_size, 2, "slot 0 null + 1 relocated func pointer");
        assert_eq!(checked.elem_count, 1);
        assert_eq!(checked.data_segments, 2);
    }

    #[test]
    fn refuses_missing_start_and_unknown_import() {
        let mut b = new_builder(&[], &[Ty::I32]);
        let zero = b.iconst(Ty::I32, 0);
        b.ret(&[zero]);
        let f = b.finish_checked().unwrap();

        let funcs_no_start = [LirWasm64Func {
            name: "not_start",
            export_name: Some("not_start"),
            place: 0,
            view: f.view(),
        }];
        let m1 = LirWasm64Module {
            name: "no-start",
            funcs: &funcs_no_start,
            statics: &[],
            imports: &[],
            opts: Wasm64Opts::DEFAULT,
            profile_digest: None,
        };
        let mut out = Vec::new_in(Alloc::global());
        assert_eq!(
            compile_lir_wasm64(&m1, &mut out, Alloc::global()),
            Err(LirWasm64Error::Export(Wasm64Error::NoStartExport))
        );

        let funcs_start = [LirWasm64Func {
            name: "_start",
            export_name: Some("_start"),
            place: 0,
            view: f.view(),
        }];
        let bad_imports = [LirWasm64Import { place: 9, module: "env", field: "unknown" }];
        let m2 = LirWasm64Module {
            name: "bad-import",
            funcs: &funcs_start,
            statics: &[],
            imports: &bad_imports,
            opts: Wasm64Opts::DEFAULT,
            profile_digest: None,
        };
        assert_eq!(
            compile_lir_wasm64(&m2, &mut out, Alloc::global()),
            Err(LirWasm64Error::UnsupportedImport { place: 9 })
        );
    }
}
