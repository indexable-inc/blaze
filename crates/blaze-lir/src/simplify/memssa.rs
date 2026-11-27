//! Unified MemorySSA, Alias Partitioning, Forwarding, Dead-Store Elimination,
//! Dead-Temp Bulk-Copy Elision, Stack-Slot Compaction, Small-Memop Scalarization,
//! and Memory LICM (`memssa`).
//!
//! Single canonical home (Rule 30) for LIR memory analysis and memory-to-SSA optimization:
//! - **Alias & Memory Partitioning ([`AliasAnalysis`])**: classifies every local stack slot as
//!   proven non-escaping ([`SlotEscape::NonEscaping`] / [`SlotEscape::NonEscapingDynamic`]) vs
//!   [`SlotEscape::Escaped`]. Non-escaping slots live in isolated per-slot byte-interval
//!   partitions; calls, invokes, atomics, fences, and stores to escaped/unknown pointers ONLY
//!   advance the Escaped/Heap [`MemVersion`] chain and NEVER clobber proven non-escaping slots.
//! - **MemorySSA `MemPhi` Barriers**: computes RPO, CSR predecessors/successors, and Cooper-
//!   Harvey-Kennedy dominators iteratively (bounded, zero recursion). Inserts `MemPhi` barriers
//!   at loop headers (for partitions modified in the loop or with multiple loop entries) and
//!   multi-predecessor joins (for differing predecessor versions), while preserving exact state
//!   along single-predecessor dominator chains and loop-invariant regions.
//! - **Store-to-Load & Uniform-`Memset` Forwarding**: forwards exact-width `[off, off + size)`
//!   stores on both non-escaping slots (across calls and single-pred/loop-invariant regions) and
//!   Heap/Escaped memory; folds uniform constant-byte `Op::Memset` to exact-width `Op::Iconst` /
//!   `Op::IconstWide` ONLY when `[l_lo, l_hi)` is strictly contained in `[m_lo, m_hi)`; and
//!   performs little-endian sub-word (`to_le_bytes`) and mixed-width piece-gather
//!   (`from_le_bytes`) forwarding on non-escaping slots.
//! - **Dead-Store Elimination (DSE)**: removes dead `Op::Store` and `Op::Memset` instructions
//!   completely covered by subsequent writes with no intervening overlapping read, as well as
//!   non-volatile writes to unread or post-exit/`SlotDead` non-escaping slots.
//! - **Dead-Temp Bulk-Copy Elision & Dead Stack-Slot Compaction**: elides single-writer
//!   `Memset -> Memcpy/Memmove` and `Memcpy/Memmove -> Memcpy/Memmove` staging slots and
//!   compacts unreferenced local slots out of `Func::slots` and `Func::variables`.
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, explicit allocator only, bounded loops (Rule 18).

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, dead, each, filled},
};
use crate::{
    Block, Flags, Func, Op, Ty, View,
    arith::{iconst_value, sext, wide_value},
    func::{MemAttrsRec, ParamFactsRec, RefId, RefKind, SlotId},
    op::{Field, Layout},
    verify,
};

/// Opaque SSA version of a memory partition (either the Escaped/Heap partition or a proven
/// non-escaping local stack slot).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct MemVersion(pub u32);

impl MemVersion {
    /// Memory version live on entry to the function.
    pub const LIVE_ON_ENTRY: Self = Self(0);
}

/// Escape classification of a local stack slot (`0..view.slots.len()`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotEscape {
    /// Proven non-escaping and only reached via constant-offset `Op::PtrAdd` chains feeding
    /// `Load`/`Store`/`Memcpy`/`Memmove`/`Memset`/`SlotLive`/`SlotDead`.
    NonEscaping,
    /// Address never escapes the function (never passed to calls/invokes, stored as a value,
    /// exposed via `PtrAddr`/`Expose`, or returned), but reached through a dynamic `PtrAdd`.
    NonEscapingDynamic,
    /// Address may escape (passed to calls/invokes, stored as a value, exposed, returned, or
    /// passed to another operation).
    Escaped,
}

impl SlotEscape {
    /// Returns `true` when the slot's address cannot be observed by external code (`Call`,
    /// `Invoke`, `Atomic*`, `Fence`, or unknown pointers).
    #[inline]
    pub const fn is_non_escaping(self) -> bool {
        !matches!(self, SlotEscape::Escaped)
    }

    /// Returns `true` when the slot is non-escaping AND every pointer derivation is a constant
    /// offset from `Op::SlotAddr`.
    #[inline]
    pub const fn is_const_only(self) -> bool {
        matches!(self, SlotEscape::NonEscaping)
    }

    /// Returns `true` when the slot's address escapes.
    #[inline]
    pub const fn is_escaped(self) -> bool {
        matches!(self, SlotEscape::Escaped)
    }
}

/// One node in the MemorySSA version graph.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemNodeKind {
    /// Initial memory state at function entry ([`MemVersion::LIVE_ON_ENTRY`]).
    LiveOnEntry,
    /// Memory definition created by instruction `inst`, clobbering `prev`.
    Def { inst: u32, prev: MemVersion },
    /// Join or loop-header barrier in `block` merging `phi_args[first_arg..first_arg + arg_len]`.
    Phi { block: u32, first_arg: u32, arg_len: u32 },
}

/// Telemetry from [`optimize_memssa`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct MemSsaStats {
    /// Dead temporary bulk-copy slots elided (`Memset -> Memcpy` / `Memcpy -> Memcpy`).
    pub temp_copies_elided: u32,
    /// Loop-invariant loads hoisted into dominating loop preheaders.
    pub loads_hoisted: u32,
    /// Loads eliminated by MemorySSA store-to-load, memset-to-load, or sub-word forwarding.
    pub loads_forwarded: u32,
    /// Redundant/overwritten/dead stores and `Memset`s eliminated.
    pub dead_stores_eliminated: u32,
    /// Dead local stack slots compacted out of `Func::slots`.
    pub slots_compacted: u32,
    /// `MemPhi` barriers inserted at loop headers and multi-predecessor joins.
    pub mem_phis_inserted: u32,
}

/// Output of [`optimize_memssa`].
#[derive(Debug)]
pub struct MemSsaOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: MemSsaStats,
}

/// Canonical root memory object of a pointer SSA value (Rule 74 & 76).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BaseObj {
    /// Local stack allocation (`Op::SlotAddr`).
    Slot(SlotId),
    /// Immutable static / rodata / vtable (`Op::DataAddr` of `RefKind::Data` or `RefKind::Vtable`).
    /// Never written by any valid store, bulk memory op, or call.
    DataAddr(RefId),
    /// Function entry parameter pointer (`Op::Param` in entry block `0`).
    /// `is_noalias` is `true` when the entry parameter carries `ParamFactsRec::NOALIAS`.
    Param { index: u32, is_noalias: bool },
    /// Unknown, opaque, or potentially writable static pointer base.
    Unknown,
}

impl BaseObj {
    /// Returns `true` when this base object is immutable read-only data (`BaseObj::DataAddr`).
    #[inline]
    pub const fn is_immutable_data(self) -> bool {
        matches!(self, BaseObj::DataAddr(_))
    }

    /// Returns the local stack slot index (`u32`) if `self` is `BaseObj::Slot`, else `NONE`.
    #[inline]
    pub const fn slot_id(self) -> u32 {
        match self {
            BaseObj::Slot(id) => id.0,
            BaseObj::DataAddr(_) | BaseObj::Param { .. } | BaseObj::Unknown => NONE,
        }
    }
}

/// Decomposed memory access `(base, off, size)` with canonical root [`BaseObj`] and optional
/// local stack-slot provenance.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MemLoc {
    /// Deepest SSA pointer value reached via constant `PtrAdd`/`Freeze`/`Bitcast` hops (`NONE` on
    /// offset overflow).
    pub base: u32,
    /// Constant byte offset from `base` (and from `base_obj` when `off_known` is `true`).
    pub off: i64,
    /// Access byte width (`1, 2, 4, 8, 16`, or vector width).
    pub size: i64,
    /// Local stack slot index this pointer derives from (`u32::MAX` if not derived from a slot).
    pub slot: u32,
    /// `true` when `off` is exact relative to the canonical root `base_obj` (false if derived via
    /// dynamic `PtrAdd` or `PtrWithAddr`).
    pub off_known: bool,
    /// Canonical root memory object of this pointer ([`BaseObj`]).
    pub base_obj: BaseObj,
}

// ============================================================================
// Pure conversion, slice-access, and constant helpers (no `as`, no `unwrap`).
// ============================================================================

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn trunc_u32(v: u128) -> u32 {
    u32::try_from(v & u128::from(u32::MAX)).unwrap_or(0)
}

#[inline]
fn at_u32(s: &[u32], i: usize) -> u32 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_u16(s: &[u16], i: usize) -> u16 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_u8(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_i64(s: &[i64], i: usize) -> i64 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_bool(s: &[bool], i: usize) -> bool {
    s.get(i).copied().unwrap_or(false)
}

#[inline]
fn at_ver(s: &[MemVersion], i: usize) -> MemVersion {
    s.get(i).copied().unwrap_or(MemVersion::LIVE_ON_ENTRY)
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn op_byte(op: Op) -> u8 {
    Op::ALL.iter().position(|&o| o == op).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

#[inline]
fn const_i64(view: &View<'_>, inst: usize) -> Option<i64> {
    let ty = Ty(view.tys.get(inst).copied()?);
    if !ty.is_scalar() || !ty.is_int() || ty.bits(64) > 64 {
        return None;
    }
    let width = ty.bits(view.head.ptr_bits);
    let v = match view.op(inst) {
        Op::Iconst => iconst_value(view.data.get(inst).copied()?, width),
        Op::IconstWide => {
            let at = to_idx(view.data.get(inst).copied()?);
            let end = at.checked_add(to_idx(ty.const_words()))?;
            wide_value(view.extra.get(at..end)?, width)
        }
        _ => return None,
    };
    i64::try_from(sext(v, width)).ok()
}

#[inline]
fn const_u64(view: &View<'_>, inst: usize) -> Option<u64> {
    let ty = Ty(view.tys.get(inst).copied()?);
    if !ty.is_scalar() || !ty.is_int() {
        return None;
    }
    let width = ty.bits(view.head.ptr_bits);
    let v = match view.op(inst) {
        Op::Iconst => iconst_value(view.data.get(inst).copied()?, width),
        Op::IconstWide => {
            let at = to_idx(view.data.get(inst).copied()?);
            let end = at.checked_add(to_idx(ty.const_words()))?;
            wide_value(view.extra.get(at..end)?, width)
        }
        _ => return None,
    };
    u64::try_from(v).ok()
}

#[inline]
fn const_u8(view: &View<'_>, inst: usize) -> Option<u8> {
    let ty = Ty(view.tys.get(inst).copied()?);
    if ty != Ty::I8 || view.op(inst) != Op::Iconst {
        return None;
    }
    let v = iconst_value(view.data.get(inst).copied()?, 8);
    u8::try_from(v).ok()
}

#[inline]
fn access_bytes(ty: Ty, ptr_bits: u32) -> Option<i64> {
    if ty.is_ptr() {
        Some(i64::from(ptr_bits / 8))
    } else if ty.is_vector() {
        let bits = ty.elem().bits(ptr_bits).checked_mul(ty.lanes())?;
        if bits > 0 && bits % 8 == 0 {
            Some(i64::from(bits / 8))
        } else {
            None
        }
    } else if ty.is_scalar() {
        let b = ty.bits(ptr_bits);
        if matches!(b, 8 | 16 | 32 | 64 | 128) {
            Some(i64::from(b / 8))
        } else {
            None
        }
    } else {
        None
    }
}

#[inline]
fn scalar_int_bytes(ty: Ty) -> Option<i64> {
    if ty == Ty::I8 {
        Some(1)
    } else if ty == Ty::I16 {
        Some(2)
    } else if ty == Ty::I32 {
        Some(4)
    } else if ty == Ty::I64 {
        Some(8)
    } else {
        None
    }
}

#[inline]
fn repeated_byte_int(ty: Ty, byte: u8) -> Option<u128> {
    if ty == Ty::I8 {
        Some(u128::from(byte))
    } else if ty == Ty::I16 {
        Some(u128::from(u16::from_le_bytes([byte; 2])))
    } else if ty == Ty::I32 {
        Some(u128::from(u32::from_le_bytes([byte; 4])))
    } else if ty == Ty::I64 {
        Some(u128::from(u64::from_le_bytes([byte; 8])))
    } else if ty == Ty::I128 {
        Some(u128::from_le_bytes([byte; 16]))
    } else {
        None
    }
}

#[inline]
fn ranges_overlap(a_start: i64, a_len: i64, b_start: i64, b_len: i64) -> bool {
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    a_start < b_end && b_start < a_end
}

#[inline]
fn slot_range_in_bounds(view: &View<'_>, slot: u32, off: i64, len: u64) -> bool {
    let Some(rec) = view.slots.get(to_idx(slot)) else {
        return false;
    };
    let Ok(off_u64) = u64::try_from(off) else {
        return false;
    };
    off_u64.checked_add(len).is_some_and(|end| end <= u64::from(rec.size))
}

#[inline]
fn data_range_in_bounds(view: &View<'_>, ref_id: RefId, off: i64, len: u64) -> bool {
    let Some(rec) = view.refs.get(to_idx(ref_id.0)) else {
        return false;
    };
    if RefKind::from_word(rec.kind) != Some(RefKind::Data) {
        return false;
    }
    let Some(data_rec) = view.datas.get(to_idx(rec.place)) else {
        return false;
    };
    let Ok(off_u64) = u64::try_from(off) else {
        return false;
    };
    off_u64.checked_add(len).is_some_and(|end| end <= u64::from(data_rec.bytes_len))
}

#[inline]
fn param_range_deref(view: &View<'_>, param_idx: u32, off: i64, len: u64) -> bool {
    let Some(sig_rec) = view.sigs.get(to_idx(view.head.sig)) else {
        return false;
    };
    let p_idx = to_idx(param_idx);
    if p_idx >= usize::from(sig_rec.params) {
        return false;
    }
    let Some(param_rec) = view.sig_params.get(to_idx(sig_rec.first).saturating_add(p_idx)) else {
        return false;
    };
    let Some(facts) = view.facts(param_rec) else {
        return false;
    };
    if facts.deref == 0 {
        return false;
    }
    let nonnull_ok = (facts.bits & ParamFactsRec::DEREF_OR_NULL) == 0
        || (facts.bits & ParamFactsRec::NONNULL) != 0;
    if !nonnull_ok {
        return false;
    }
    let Ok(off_u64) = u64::try_from(off) else {
        return false;
    };
    off_u64.checked_add(len).is_some_and(|end| end <= facts.deref)
}

#[inline]
fn op_may_trap_or_diverge(op: Op) -> bool {
    matches!(
        op,
        Op::Udiv
            | Op::Sdiv
            | Op::Urem
            | Op::Srem
            | Op::Call
            | Op::CallIndirect
            | Op::Invoke
            | Op::InvokeIndirect
            | Op::TailCall
            | Op::TailCallIndirect
            | Op::Asm
            | Op::AsmBr
            | Op::TargetIntrinsic
            | Op::Suspend
            | Op::Trap
            | Op::Unreachable
            | Op::Resume
            | Op::Cancelled
    )
}

// ============================================================================
// Canonical Alias & Escape Analysis (One Home, Rule 30)
// ============================================================================

/// Function-wide slot escape classification and `BaseObj` pointer-provenance table (SoA flat arrays).
pub struct AliasAnalysis<A: Allocator> {
    slot_escape: Vec<SlotEscape, A>,
    /// Per SSA instruction: canonical root memory object ([`BaseObj`]) this pointer derives from.
    val_base: Vec<BaseObj, A>,
    /// Per SSA instruction: local stack slot id this pointer derives from, or `NONE`.
    val_slot: Vec<u32, A>,
    /// Per SSA instruction: constant byte offset from canonical root `val_base[v]` when
    /// `val_off_known[v]` is `true`.
    val_off: Vec<i64, A>,
    /// Per SSA instruction: `true` iff `val_off[v]` is an exact constant offset from `val_base[v]`.
    val_off_known: Vec<bool, A>,
    /// Count of proven non-escaping slots (`slot_escape[s].is_non_escaping()`).
    non_escaping_count: usize,
}

impl<A: Allocator + Copy> AliasAnalysis<A> {
    /// Computes canonical slot escape classification and [`BaseObj`] pointer provenance for `view`.
    pub fn analyze(view: &View<'_>, alloc: A) -> Self {
        let nslots = view.slots.len();
        let len = view.len();
        let mut slot_escape = filled(nslots, SlotEscape::NonEscaping, &alloc);
        let mut val_base = filled(len, BaseObj::Unknown, &alloc);
        let mut val_slot = filled(len, NONE, &alloc);
        let mut val_off = filled(len, 0i64, &alloc);
        let mut val_off_known = filled(len, false, &alloc);

        // Seed entry parameter pointer provenance with `ParamFactsRec::NOALIAS`.
        if view.blocks() > 0 {
            let entry_params = view.params(Block(0));
            let own_sig_rec = view.sigs.get(to_idx(view.head.sig)).copied();
            for inst in entry_params.start..entry_params.end {
                if !Ty(at_u16(view.tys, inst)).is_ptr() {
                    continue;
                }
                let p_idx = inst.saturating_sub(entry_params.start);
                let is_noalias = own_sig_rec
                    .and_then(|sig| {
                        if p_idx < usize::from(sig.params) {
                            view.sig_params.get(to_idx(sig.first).saturating_add(p_idx))
                        } else {
                            None
                        }
                    })
                    .and_then(|param_rec| view.facts(param_rec))
                    .is_some_and(|facts| (facts.bits & ParamFactsRec::NOALIAS) != 0);
                set_at(&mut val_base, inst, BaseObj::Param { index: to_u32(p_idx), is_noalias });
                set_at(&mut val_off, inst, 0);
                set_at(&mut val_off_known, inst, true);
            }
        }

        // Seed `Op::SlotAddr` and immutable `Op::DataAddr` (`RefKind::Data | RefKind::Vtable`) definitions.
        for inst in 0..len {
            match view.op(inst) {
                Op::SlotAddr => {
                    let s = at_u32(view.data, inst);
                    set_at(&mut val_base, inst, BaseObj::Slot(SlotId(s)));
                    set_at(&mut val_slot, inst, s);
                    set_at(&mut val_off, inst, 0);
                    set_at(&mut val_off_known, inst, true);
                }
                Op::DataAddr => {
                    let r = at_u32(view.data, inst);
                    if let Some(rec) = view.refs.get(to_idx(r))
                        && matches!(
                            RefKind::from_word(rec.kind),
                            Some(RefKind::Data | RefKind::Vtable)
                        )
                    {
                        set_at(&mut val_base, inst, BaseObj::DataAddr(RefId(r)));
                        set_at(&mut val_off, inst, 0);
                        set_at(&mut val_off_known, inst, true);
                    }
                }
                _ => {}
            }
        }

        // Bounded fixpoint (handles non-RPO layout order and chained `PtrAdd`/`Freeze`/`PtrWithAddr`/
        // `Bitcast` derivations up to depth 16).
        for _ in 0..16 {
            let mut changed = false;
            for inst in 0..len {
                let (next_base, next_slot, next_off, next_known) = match view.op(inst) {
                    Op::PtrAdd => {
                        let at = to_idx(at_u32(view.data, inst));
                        let base = to_idx(at_u32(view.extra, at));
                        let by = to_idx(at_u32(view.extra, at.saturating_add(1)));
                        let base_obj = val_base.get(base).copied().unwrap_or(BaseObj::Unknown);
                        if base_obj == BaseObj::Unknown {
                            continue;
                        }
                        let base_slot = at_u32(&val_slot, base);
                        let (off, known) = if at_bool(&val_off_known, base)
                            && let Some(delta) = const_i64(view, by)
                            && let Some(sum) = at_i64(&val_off, base).checked_add(delta)
                        {
                            (sum, true)
                        } else {
                            (0, false)
                        };
                        (base_obj, base_slot, off, known)
                    }
                    Op::Freeze | Op::Bitcast => {
                        let src = to_idx(at_u32(view.data, inst));
                        let base_obj = val_base.get(src).copied().unwrap_or(BaseObj::Unknown);
                        if base_obj == BaseObj::Unknown {
                            continue;
                        }
                        (
                            base_obj,
                            at_u32(&val_slot, src),
                            at_i64(&val_off, src),
                            at_bool(&val_off_known, src),
                        )
                    }
                    Op::PtrWithAddr => {
                        // Preserves provenance base of its first operand (`ptr`), discarding constant offset.
                        let at = to_idx(at_u32(view.data, inst));
                        let ptr_op = to_idx(at_u32(view.extra, at));
                        let base_obj = val_base.get(ptr_op).copied().unwrap_or(BaseObj::Unknown);
                        if base_obj == BaseObj::Unknown {
                            continue;
                        }
                        (base_obj, at_u32(&val_slot, ptr_op), 0, false)
                    }
                    _ => continue,
                };

                let cur_base = val_base.get(inst).copied().unwrap_or(BaseObj::Unknown);
                if cur_base != next_base
                    || at_u32(&val_slot, inst) != next_slot
                    || at_bool(&val_off_known, inst) != next_known
                    || (next_known && at_i64(&val_off, inst) != next_off)
                {
                    set_at(&mut val_base, inst, next_base);
                    set_at(&mut val_slot, inst, next_slot);
                    set_at(&mut val_off, inst, next_off);
                    set_at(&mut val_off_known, inst, next_known);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        // Any unconverged pointer-derivation chain (>16 hops) escapes its slot conservatively;
        // any dynamic-offset `PtrAdd` or `PtrWithAddr` downgrades `NonEscaping` -> `NonEscapingDynamic`.
        for inst in 0..len {
            match view.op(inst) {
                Op::PtrAdd | Op::PtrWithAddr => {
                    let at = to_idx(at_u32(view.data, inst));
                    let base = to_idx(at_u32(view.extra, at));
                    let rhs = to_idx(at_u32(view.extra, at.saturating_add(1)));
                    let rhs_slot = at_u32(&val_slot, rhs);
                    if rhs_slot != NONE {
                        set_at(&mut slot_escape, to_idx(rhs_slot), SlotEscape::Escaped);
                    }
                    let base_slot = at_u32(&val_slot, base);
                    if base_slot != NONE {
                        let s_idx = to_idx(base_slot);
                        if at_u32(&val_slot, inst) != base_slot {
                            set_at(&mut slot_escape, s_idx, SlotEscape::Escaped);
                        } else if !at_bool(&val_off_known, inst)
                            && slot_escape.get(s_idx).copied() == Some(SlotEscape::NonEscaping)
                        {
                            set_at(&mut slot_escape, s_idx, SlotEscape::NonEscapingDynamic);
                        }
                    }
                }
                Op::Freeze | Op::Bitcast => {
                    let src = to_idx(at_u32(view.data, inst));
                    let src_slot = at_u32(&val_slot, src);
                    if src_slot != NONE {
                        let s_idx = to_idx(src_slot);
                        if at_u32(&val_slot, inst) != src_slot {
                            set_at(&mut slot_escape, s_idx, SlotEscape::Escaped);
                        } else if !at_bool(&val_off_known, inst)
                            && slot_escape.get(s_idx).copied() == Some(SlotEscape::NonEscaping)
                        {
                            set_at(&mut slot_escape, s_idx, SlotEscape::NonEscapingDynamic);
                        }
                    }
                }
                _ => {}
            }
        }

        // Inspect every use of every slot-derived pointer across the function.
        // Only `Load` (ptr), `Store` (ptr), `Memcpy`/`Memmove` (dst, src), `Memset` (dst),
        // `PtrAdd` (base), `Freeze`, `PtrWithAddr` (ptr), `Bitcast`, `SlotLive`, and `SlotDead`
        // preserve non-escaping status. `PtrAddr`, `Expose`, `Call`/`Invoke`, `Return`, stored
        // values, and block arguments escape the slot.
        for inst in 0..len {
            let op = view.op(inst);
            match op {
                Op::SlotAddr
                | Op::SlotLive
                | Op::SlotDead
                | Op::PtrAdd
                | Op::Freeze
                | Op::PtrWithAddr
                | Op::Bitcast
                | Op::Load => {}
                Op::Store => {
                    // Stored VALUE escapes if it holds a slot address; pointer operand does not.
                    let at = to_idx(at_u32(view.data, inst));
                    let val = to_idx(at_u32(view.extra, at));
                    let s = at_u32(&val_slot, val);
                    if s != NONE {
                        set_at(&mut slot_escape, to_idx(s), SlotEscape::Escaped);
                    }
                }
                Op::Memset => {
                    let at = to_idx(at_u32(view.data, inst));
                    let byte_val = to_idx(at_u32(view.extra, at.saturating_add(1)));
                    let len_val = to_idx(at_u32(view.extra, at.saturating_add(2)));
                    let bs = at_u32(&val_slot, byte_val);
                    if bs != NONE {
                        set_at(&mut slot_escape, to_idx(bs), SlotEscape::Escaped);
                    }
                    let ls = at_u32(&val_slot, len_val);
                    if ls != NONE {
                        set_at(&mut slot_escape, to_idx(ls), SlotEscape::Escaped);
                    }
                }
                Op::Memcpy | Op::Memmove => {
                    let at = to_idx(at_u32(view.data, inst));
                    let len_val = to_idx(at_u32(view.extra, at.saturating_add(2)));
                    let ls = at_u32(&val_slot, len_val);
                    if ls != NONE {
                        set_at(&mut slot_escape, to_idx(ls), SlotEscape::Escaped);
                    }
                }
                _ => {
                    each(view, inst, |item| match item {
                        Item::Value(v) => {
                            let s = at_u32(&val_slot, to_idx(v));
                            if s != NONE {
                                set_at(&mut slot_escape, to_idx(s), SlotEscape::Escaped);
                            }
                        }
                        Item::Edge(edge) => {
                            if let Some((arg_start, arg_len)) = edge.args {
                                for k in 0..arg_len {
                                    let v = at_u32(view.extra, arg_start.saturating_add(k));
                                    let s = at_u32(&val_slot, to_idx(v));
                                    if s != NONE {
                                        set_at(&mut slot_escape, to_idx(s), SlotEscape::Escaped);
                                    }
                                }
                            }
                        }
                    });
                }
            }
        }

        let mut non_escaping_count = 0usize;
        for &esc in &slot_escape {
            if esc.is_non_escaping() {
                non_escaping_count = non_escaping_count.saturating_add(1);
            }
        }

        Self { slot_escape, val_base, val_slot, val_off, val_off_known, non_escaping_count }
    }

    #[inline]
    pub fn slot_escape(&self, slot: u32) -> SlotEscape {
        self.slot_escape.get(to_idx(slot)).copied().unwrap_or(SlotEscape::Escaped)
    }

    #[inline]
    pub fn is_non_escaping(&self, slot: u32) -> bool {
        self.slot_escape(slot).is_non_escaping()
    }

    #[inline]
    pub fn has_non_escaping(&self) -> bool {
        self.non_escaping_count > 0
    }

    #[inline]
    pub fn slot_of_val(&self, val: u32) -> u32 {
        at_u32(&self.val_slot, to_idx(val))
    }

    /// Returns the canonical root [`BaseObj`] of pointer SSA value `val`.
    #[inline]
    pub fn base_of(&self, val: u32) -> BaseObj {
        self.val_base.get(to_idx(val)).copied().unwrap_or(BaseObj::Unknown)
    }

    /// Returns `true` if memory accesses rooted at `a` and `b` may alias, or `false` when they
    /// are **provably disjoint** (Rule 74 & 76).
    ///
    /// Disjointness proofs:
    /// - Immutable `BaseObj::DataAddr(_)` (`RefKind::Data | RefKind::Vtable`) vs ANY store/base
    ///   -> `false` (read-only data is never written by any valid store or call).
    /// - `BaseObj::Slot(s1)` vs `BaseObj::Slot(s2)` with `s1 != s2` -> `false`.
    /// - Non-escaping `BaseObj::Slot(s)` vs `Param` / `DataAddr` / `Unknown` -> `false`.
    /// - Distinct `BaseObj::Param`s (`p1 != p2`) when at least one carries `ParamFactsRec::NOALIAS`
    ///   -> `false`.
    #[inline]
    pub fn may_alias_bases(&self, a: BaseObj, b: BaseObj) -> bool {
        match (a, b) {
            (BaseObj::DataAddr(_), _) | (_, BaseObj::DataAddr(_)) => false,
            (BaseObj::Slot(s1), BaseObj::Slot(s2)) => s1 == s2,
            (BaseObj::Slot(s), BaseObj::Param { .. } | BaseObj::Unknown)
            | (BaseObj::Param { .. } | BaseObj::Unknown, BaseObj::Slot(s)) => {
                !self.is_non_escaping(s.0)
            }
            (
                BaseObj::Param { index: p1, is_noalias: n1 },
                BaseObj::Param { index: p2, is_noalias: n2 },
            ) => p1 == p2 || (!n1 && !n2),
            (BaseObj::Param { .. }, BaseObj::Unknown)
            | (BaseObj::Unknown, BaseObj::Param { .. })
            | (BaseObj::Unknown, BaseObj::Unknown) => true,
        }
    }

    /// Resolves `(ptr, inline_off)` to `(slot_id, exact_slot_off)` when rooted at `Op::SlotAddr`
    /// through constant-offset `Op::PtrAdd`/`Freeze`/`Bitcast` hops.
    #[inline]
    pub fn resolve_slot_ptr_const(&self, ptr: u32, inline_off: i64) -> Option<(u32, i64)> {
        let pi = to_idx(ptr);
        let s = at_u32(&self.val_slot, pi);
        if s == NONE || !at_bool(&self.val_off_known, pi) {
            return None;
        }
        let off = at_i64(&self.val_off, pi).checked_add(inline_off)?;
        Some((s, off))
    }

    /// Resolves `(ptr, inline_off)` through up to 16 constant `Op::PtrAdd(_, const)` /
    /// `Op::Freeze` / `Op::Bitcast` hops, returning `(base_ptr, total_off, slot_id, off_known)`.
    ///
    /// When `base_ptr != NONE`, `total_off` is exact relative to SSA value `base_ptr`.
    /// When `off_known` is `true`, `total_off` is also exact relative to `self.base_of(ptr)` for
    /// any non-`Unknown` root. Returns `(NONE, 0, slot_id, false)` on `i64` offset overflow.
    pub fn resolve_ptr_const(
        &self,
        view: &View<'_>,
        ptr: u32,
        inline_off: i64,
    ) -> (u32, i64, u32, bool) {
        let orig_pi = to_idx(ptr);
        let orig_base_obj = self.val_base.get(orig_pi).copied().unwrap_or(BaseObj::Unknown);
        let orig_slot = at_u32(&self.val_slot, orig_pi);

        let mut cur = ptr;
        let mut off = inline_off;
        for _ in 0..16 {
            let pi = to_idx(cur);
            match view.op(pi) {
                Op::PtrAdd => {
                    let at = to_idx(at_u32(view.data, pi));
                    let base = at_u32(view.extra, at);
                    let by = at_u32(view.extra, at.saturating_add(1));
                    if let Some(d) = const_i64(view, to_idx(by)) {
                        if let Some(next_off) = off.checked_add(d) {
                            cur = base;
                            off = next_off;
                            continue;
                        }
                        return (NONE, 0, orig_slot, false);
                    }
                    break;
                }
                Op::Freeze | Op::Bitcast => {
                    cur = at_u32(view.data, pi);
                    continue;
                }
                _ => break,
            }
        }

        if orig_base_obj != BaseObj::Unknown {
            if at_bool(&self.val_off_known, orig_pi) {
                let Some(exp) = at_i64(&self.val_off, orig_pi).checked_add(inline_off) else {
                    return (NONE, 0, orig_slot, false);
                };
                let cur_is_root = matches!(
                    view.op(to_idx(cur)),
                    Op::SlotAddr | Op::DataAddr | Op::Param
                );
                if cur_is_root {
                    return (cur, exp, orig_slot, true);
                }
                return (NONE, exp, orig_slot, true);
            }
            return (cur, off, orig_slot, false);
        }

        (cur, off, orig_slot, true)
    }

    /// Decomposes `(ptr, inline_off, access_ty)` into a [`MemLoc`].
    pub fn decompose_loc(
        &self,
        view: &View<'_>,
        ptr: u32,
        inline_off: i64,
        ty: Ty,
    ) -> Option<MemLoc> {
        let size = access_bytes(ty, view.head.ptr_bits)?;
        let base_obj = self.base_of(ptr);
        let (base, off, slot, off_known) = self.resolve_ptr_const(view, ptr, inline_off);
        Some(MemLoc { base, off, size, slot, off_known, base_obj })
    }

    /// Returns `true` when `a` and `b` are provably disjoint (`NoAlias`).
    #[inline]
    pub fn no_alias(&self, a: MemLoc, b: MemLoc) -> bool {
        if !self.may_alias_bases(a.base_obj, b.base_obj) {
            return true;
        }
        let same_root_known = a.base_obj != BaseObj::Unknown
            && a.base_obj == b.base_obj
            && a.off_known
            && b.off_known;
        let same_ssa_base = a.base != NONE && a.base == b.base;
        if (same_root_known || same_ssa_base)
            && let (Some(a_end), Some(b_end)) =
                (a.off.checked_add(a.size), b.off.checked_add(b.size))
        {
            return a_end <= b.off || b_end <= a.off;
        }
        false
    }
}

/// Resolves `(ptr, off)` through `0..16` constant `Op::PtrAdd(_, const_i64)` hops to
/// `(slot_id, total_slot_off)` when rooted at `Op::SlotAddr`.
fn resolve_slot_ptr_const_view(view: &View<'_>, mut ptr: u32, mut off: i64) -> Option<(u32, i64)> {
    for _ in 0..16 {
        let pi = to_idx(ptr);
        if view.op(pi) == Op::PtrAdd {
            let at = to_idx(at_u32(view.data, pi));
            let base = at_u32(view.extra, at);
            let by = at_u32(view.extra, at.saturating_add(1));
            let d = const_i64(view, to_idx(by))?;
            off = off.checked_add(d)?;
            ptr = base;
            continue;
        }
        break;
    }
    let pi = to_idx(ptr);
    if view.op(pi) == Op::SlotAddr {
        Some((at_u32(view.data, pi), off))
    } else {
        None
    }
}

/// Resolves `ptr` to `(slot_id, slot_off)` when it points into a local `Op::SlotAddr` with
/// `slot_off >= 0` and `slot_off + copy_len <= slot_size`.
fn resolve_slot_copy_range(view: &View<'_>, ptr: u32, copy_len: u64) -> Option<(u32, i64)> {
    let (slot, off) = resolve_slot_ptr_const_view(view, ptr, 0)?;
    if slot_range_in_bounds(view, slot, off, copy_len) {
        Some((slot, off))
    } else {
        None
    }
}

// ============================================================================
// Iterative SoA CSR CFG, RPO, Dominators, and Natural Loops (zero recursion)
// ============================================================================

struct CfgInfo<A: Allocator> {
    blocks: usize,
    /// Per instruction: owning block id.
    inst_block: Vec<u32, A>,
    /// CSR predecessors per block (`pred_items[pred_starts[b]..pred_starts[b+1]]`), unique.
    pred_starts: Vec<u32, A>,
    pred_items: Vec<u32, A>,
    /// Reachable blocks in Reverse Post-Order (entry block `0` always first).
    rpo: Vec<u32, A>,
    /// Per block: position in `rpo`, or `NONE` if unreachable.
    rpo_idx: Vec<u32, A>,
    /// Per block: immediate dominator (`0` for entry, `NONE` if unreachable).
    idom: Vec<u32, A>,
    /// Per block: `true` if block has a reducible loop backedge `latch -> b` where `b` dominates `latch`.
    is_loop_header: Vec<bool, A>,
    /// Per block: `true` if block has an irreducible backedge `p -> b` (`rpo_idx[p] >= rpo_idx[b]`
    /// and `b` does not dominate `p`).
    is_irreducible_header: Vec<bool, A>,
}

impl<A: Allocator + Copy> CfgInfo<A> {
    fn build(view: &View<'_>, alloc: A) -> Self {
        let blocks = view.blocks();
        let len = view.len();

        let mut inst_block = filled(len, 0u32, &alloc);
        let mut succ_starts = filled(blocks.saturating_add(1), 0u32, &alloc);
        let mut succ_items: Vec<u32, A> = Vec::with_capacity_in(blocks.saturating_mul(2), alloc);
        let mut pred_counts = filled(blocks.saturating_add(1), 0u32, &alloc);

        // Pass 1: build CSR successors (deduplicated per block) and count incoming unique edges.
        for b in 0..blocks {
            let b32 = to_u32(b);
            for inst in view.block(Block(b32)) {
                set_at(&mut inst_block, inst, b32);
            }
            let start = succ_items.len();
            set_at(&mut succ_starts, b, to_u32(start));
            let term = view.terminator(Block(b32));
            each(view, term, |item| {
                if let Item::Edge(edge) = item {
                    let existing = succ_items.get(start..).unwrap_or(&[]);
                    if !existing.iter().any(|&s| s == edge.block) {
                        succ_items.push(edge.block);
                        let tb_next = to_idx(edge.block).saturating_add(1);
                        if let Some(c) = pred_counts.get_mut(tb_next) {
                            *c = c.saturating_add(1);
                        }
                    }
                }
            });
        }
        set_at(&mut succ_starts, blocks, to_u32(succ_items.len()));

        // Prefix-sum predecessor CSR offsets and populate `pred_items`.
        for b in 0..blocks {
            let prev = at_u32(&pred_counts, b);
            let next_i = b.saturating_add(1);
            let cur = at_u32(&pred_counts, next_i);
            set_at(&mut pred_counts, next_i, prev.saturating_add(cur));
        }
        let pred_starts = pred_counts;
        let total_preds = to_idx(at_u32(&pred_starts, blocks));
        let mut pred_items = filled(total_preds, 0u32, &alloc);
        let mut pred_fill = filled(blocks, 0u32, &alloc);
        for b in 0..blocks {
            let b32 = to_u32(b);
            let s0 = to_idx(at_u32(&succ_starts, b));
            let s1 = to_idx(at_u32(&succ_starts, b.saturating_add(1)));
            for &succ in succ_items.get(s0..s1).unwrap_or(&[]) {
                let si = to_idx(succ);
                let base = at_u32(&pred_starts, si);
                let off = at_u32(&pred_fill, si);
                set_at(&mut pred_items, to_idx(base.saturating_add(off)), b32);
                set_at(&mut pred_fill, si, off.saturating_add(1));
            }
        }

        // Iterative DFS to compute RPO (bounded by `2 * blocks + 2 * edges + 2`, zero recursion).
        let mut seen = filled(blocks, false, &alloc);
        let mut post: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
        let mut stack_blk: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
        let mut stack_pos: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
        if blocks > 0 {
            set_at(&mut seen, 0, true);
            stack_blk.push(0);
            stack_pos.push(0);
        }
        let dfs_bound = blocks
            .saturating_mul(2)
            .saturating_add(succ_items.len().saturating_mul(2))
            .saturating_add(2);
        for _ in 0..dfs_bound {
            let Some(&b) = stack_blk.last() else {
                break;
            };
            let pos = stack_pos.last().copied().unwrap_or(0);
            let bi = to_idx(b);
            let s0 = to_idx(at_u32(&succ_starts, bi));
            let s1 = to_idx(at_u32(&succ_starts, bi.saturating_add(1)));
            let succs = succ_items.get(s0..s1).unwrap_or(&[]);
            if to_idx(pos) < succs.len() {
                if let Some(top_pos) = stack_pos.last_mut() {
                    *top_pos = pos.saturating_add(1);
                }
                let s = at_u32(succs, to_idx(pos));
                let si = to_idx(s);
                if !at_bool(&seen, si) {
                    set_at(&mut seen, si, true);
                    stack_blk.push(s);
                    stack_pos.push(0);
                }
            } else {
                post.push(b);
                let _ = stack_blk.pop();
                let _ = stack_pos.pop();
            }
        }

        let mut rpo: Vec<u32, A> = Vec::with_capacity_in(post.len(), alloc);
        let mut rpo_idx = filled(blocks, NONE, &alloc);
        for (idx, &b) in post.iter().rev().enumerate() {
            rpo.push(b);
            set_at(&mut rpo_idx, to_idx(b), to_u32(idx));
        }

        // Cooper-Harvey-Kennedy iterative immediate dominators (bounded by `2 * blocks + 2`).
        let mut idom = filled(blocks, NONE, &alloc);
        if blocks > 0 {
            set_at(&mut idom, 0, 0);
        }
        let dom_bound = blocks.saturating_mul(2).saturating_add(2);
        for _ in 0..dom_bound {
            let mut changed = false;
            for &b in rpo.iter().skip(1) {
                let bi = to_idx(b);
                let p0 = to_idx(at_u32(&pred_starts, bi));
                let p1 = to_idx(at_u32(&pred_starts, bi.saturating_add(1)));
                let mut new_dom = NONE;
                for &p in pred_items.get(p0..p1).unwrap_or(&[]) {
                    if at_u32(&idom, to_idx(p)) == NONE {
                        continue;
                    }
                    new_dom = if new_dom == NONE {
                        p
                    } else {
                        intersect_dom(&idom, &rpo_idx, p, new_dom)
                    };
                }
                if new_dom != at_u32(&idom, bi) {
                    set_at(&mut idom, bi, new_dom);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        let mut cfg = Self {
            blocks,
            inst_block,
            pred_starts,
            pred_items,
            rpo,
            rpo_idx,
            idom,
            is_loop_header: filled(blocks, false, &alloc),
            is_irreducible_header: filled(blocks, false, &alloc),
        };

        // Classify reducible natural loop headers and irreducible loop headers.
        for b in 0..blocks {
            let b32 = to_u32(b);
            let b_rpo = at_u32(&cfg.rpo_idx, b);
            if b_rpo == NONE {
                continue;
            }
            let p0 = to_idx(at_u32(&cfg.pred_starts, b));
            let p1 = to_idx(at_u32(&cfg.pred_starts, b.saturating_add(1)));
            let mut has_red_backedge = false;
            let mut has_irred_backedge = false;
            for &p in cfg.pred_items.get(p0..p1).unwrap_or(&[]) {
                let p_rpo = at_u32(&cfg.rpo_idx, to_idx(p));
                if p_rpo == NONE {
                    continue;
                }
                if cfg.dominates(b32, p) {
                    has_red_backedge = true;
                } else if p_rpo >= b_rpo {
                    has_irred_backedge = true;
                }
            }
            if has_red_backedge {
                set_at(&mut cfg.is_loop_header, b, true);
            }
            if has_irred_backedge {
                set_at(&mut cfg.is_irreducible_header, b, true);
            }
        }

        cfg
    }

    #[inline]
    fn preds(&self, b: u32) -> &[u32] {
        let bi = to_idx(b);
        let p0 = to_idx(at_u32(&self.pred_starts, bi));
        let p1 = to_idx(at_u32(&self.pred_starts, bi.saturating_add(1)));
        self.pred_items.get(p0..p1).unwrap_or(&[])
    }

    #[inline]
    fn reachable(&self, b: u32) -> bool {
        at_u32(&self.rpo_idx, to_idx(b)) != NONE
    }

    /// Returns `true` when block `a` dominates block `b` (both reachable).
    fn dominates(&self, a: u32, mut b: u32) -> bool {
        if !self.reachable(a) || !self.reachable(b) {
            return false;
        }
        for _ in 0..=self.blocks {
            if a == b {
                return true;
            }
            if b == 0 {
                return false;
            }
            let next = at_u32(&self.idom, to_idx(b));
            if next == NONE || next == b {
                return false;
            }
            b = next;
        }
        false
    }

    /// Collects all blocks in the natural loop of reducible header `header` into `out_body`,
    /// using scratch flags `in_loop` (cleared on return). Returns `(outside_pred_count, single_preheader)`.
    fn collect_natural_loop(
        &self,
        header: u32,
        in_loop: &mut [bool],
        out_body: &mut Vec<u32, A>,
        work: &mut Vec<u32, A>,
    ) -> (u32, u32) {
        out_body.clear();
        work.clear();
        let mut outside_count = 0u32;
        let mut preheader = NONE;

        set_at(in_loop, to_idx(header), true);
        out_body.push(header);

        for &p in self.preds(header) {
            if !self.reachable(p) {
                continue;
            }
            if self.dominates(header, p) {
                if !at_bool(in_loop, to_idx(p)) {
                    set_at(in_loop, to_idx(p), true);
                    out_body.push(p);
                    work.push(p);
                }
            } else {
                outside_count = outside_count.saturating_add(1);
                preheader = p;
            }
        }

        let bound = self.blocks.saturating_add(2);
        for _ in 0..bound {
            let Some(cur) = work.pop() else {
                break;
            };
            for &p in self.preds(cur) {
                if !self.reachable(p) {
                    continue;
                }
                let pi = to_idx(p);
                if !at_bool(in_loop, pi) {
                    set_at(in_loop, pi, true);
                    out_body.push(p);
                    work.push(p);
                }
            }
        }

        for &bb in out_body.iter() {
            set_at(in_loop, to_idx(bb), false);
        }

        (outside_count, preheader)
    }
}

fn intersect_dom(idom: &[u32], rpo_idx: &[u32], mut a: u32, mut b: u32) -> u32 {
    let bound = idom.len().saturating_mul(2).saturating_add(2);
    for _ in 0..bound {
        if a == b {
            return a;
        }
        let ra = at_u32(rpo_idx, to_idx(a));
        let rb = at_u32(rpo_idx, to_idx(b));
        if ra > rb {
            let next = at_u32(idom, to_idx(a));
            if next == NONE || next == a {
                return b;
            }
            a = next;
        } else if rb > ra {
            let next = at_u32(idom, to_idx(b));
            if next == NONE || next == b {
                return a;
            }
            b = next;
        } else {
            return a;
        }
    }
    a
}

// ============================================================================
// Unified MemorySSA Forwarding (Non-Escaping Slots + Heap/Escaped + Memset + Sub-Word)
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Partition {
    NonEscapingSlot(u32),
    /// Immutable read-only static / rodata / vtable (`BaseObj::DataAddr`).
    /// Never clobbered by any store, bulk copy, call, atomic, fence, or join barrier.
    ImmutableData,
    Heap,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WriteValue {
    Ssa { val: u32, ty: Ty },
    MemsetByte(u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ActiveWrite {
    partition: Partition,
    /// For `NonEscapingSlot(s)`: `s`. For `Heap`/`ImmutableData`: decomposed base pointer SSA value.
    base: u32,
    /// Canonical root memory object ([`BaseObj`]) for disjoint-base alias proofs.
    base_obj: BaseObj,
    /// Local slot id if `Heap` access is from an escaped slot, else `NONE`.
    escaped_slot: u32,
    /// Whether `off` is exact relative to `base_obj` (always true for `NonEscapingSlot`).
    off_known: bool,
    off: i64,
    len: i64,
    value: WriteValue,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Piece {
    val: u32,
    byte_off: u8,
}

const EMPTY_PIECE: Piece = Piece { val: NONE, byte_off: 0 };

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LoadRewrite {
    Keep,
    /// Replace `Load` directly with dominating SSA value `val` (0 instructions emitted).
    ForwardVal(u32),
    /// Replace `Load` with uniform constant integer `value` of type `ty` (`Iconst` / `IconstWide`).
    ConstInt { ty: Ty, value: u128 },
    /// Little-endian narrow scalar-int load extracted from an enclosing wider scalar-int store.
    NarrowFromWide {
        store_val: u32,
        store_ty: Ty,
        load_ty: Ty,
        shift_bits: u32,
    },
    /// Little-endian wide scalar-int load assembled from disjoint covering narrower stores.
    WideFromPieces {
        load_ty: Ty,
        pieces: [Piece; 8],
        count: u8,
    },
}

const MAX_ACTIVE_WRITES: usize = 192;
const MAX_SAVED_WRITE_POOL: usize = 32_768;
const MAX_TRACKED_SLOTS_MATRIX: usize = 65_536;

/// Flat SoA MemorySSA version graph built during forwarding.
pub struct MemSsaGraph<A: Allocator> {
    nodes: Vec<MemNodeKind, A>,
    phi_args: Vec<MemVersion, A>,
    mem_phis_inserted: u32,
}

impl<A: Allocator + Copy> MemSsaGraph<A> {
    fn new(alloc: A) -> Self {
        let mut nodes = Vec::new_in(alloc);
        nodes.push(MemNodeKind::LiveOnEntry);
        Self { nodes, phi_args: Vec::new_in(alloc), mem_phis_inserted: 0 }
    }

    #[inline]
    pub fn node(&self, ver: MemVersion) -> Option<MemNodeKind> {
        self.nodes.get(to_idx(ver.0)).copied()
    }

    #[inline]
    pub fn phi_args(&self, first_arg: u32, arg_len: u32) -> &[MemVersion] {
        let s = to_idx(first_arg);
        let e = s.saturating_add(to_idx(arg_len));
        self.phi_args.get(s..e).unwrap_or(&[])
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    #[inline]
    pub fn mem_phis_inserted(&self) -> u32 {
        self.mem_phis_inserted
    }

    #[inline]
    fn push_def(&mut self, inst: u32, prev: MemVersion) -> MemVersion {
        let id = MemVersion(to_u32(self.nodes.len()));
        self.nodes.push(MemNodeKind::Def { inst, prev });
        id
    }

    #[inline]
    fn push_phi(&mut self, block: u32, args: &[MemVersion]) -> MemVersion {
        let first_arg = to_u32(self.phi_args.len());
        for &a in args {
            self.phi_args.push(a);
        }
        let arg_len = to_u32(args.len());
        let id = MemVersion(to_u32(self.nodes.len()));
        self.nodes.push(MemNodeKind::Phi { block, first_arg, arg_len });
        self.mem_phis_inserted = self.mem_phis_inserted.saturating_add(1);
        id
    }
}

#[inline]
fn load_full_byte_mask(load_bytes: i64) -> Option<u8> {
    match load_bytes {
        2 => Some(0x03),
        4 => Some(0x0f),
        8 => Some(0xff),
        _ => None,
    }
}

#[inline]
fn piece_byte_mask(byte_off: u8, byte_width: u8) -> Option<u8> {
    let base: u8 = match byte_width {
        1 => 0x01,
        2 => 0x03,
        4 => 0x0f,
        _ => return None,
    };
    if byte_off.checked_add(byte_width).is_some_and(|end| end <= 8) {
        Some(base << byte_off)
    } else {
        None
    }
}

#[inline]
fn rewrite_inst_count(rw: LoadRewrite) -> usize {
    match rw {
        LoadRewrite::ForwardVal(_) => 0,
        LoadRewrite::Keep | LoadRewrite::ConstInt { .. } => 1,
        LoadRewrite::NarrowFromWide { shift_bits, .. } => {
            if shift_bits == 0 {
                1
            } else {
                3
            }
        }
        LoadRewrite::WideFromPieces { count, .. } => {
            let k = usize::from(count);
            if k <= 1 {
                1
            } else {
                1usize.saturating_add(4usize.saturating_mul(k.saturating_sub(1)))
            }
        }
    }
}

#[inline]
fn push_data_inst<A: Allocator>(
    func: &mut Func<A>,
    op: u8,
    ty: Ty,
    data: u32,
    loc: u32,
) -> u32 {
    let idx = to_u32(func.ops.len());
    func.ops.push(op);
    func.flags.push(Flags::NONE.0);
    func.tys.push(ty.0);
    func.data.push(data);
    func.attrs.push(0);
    func.locs.push(loc);
    idx
}

#[inline]
fn push_vv_inst<A: Allocator>(
    func: &mut Func<A>,
    op: u8,
    ty: Ty,
    lhs: u32,
    rhs: u32,
    loc: u32,
) -> u32 {
    let extra_at = to_u32(func.extra.len());
    func.extra.push(lhs);
    func.extra.push(rhs);
    push_data_inst(func, op, ty, extra_at, loc)
}

#[inline]
fn emit_int_const<A: Allocator>(
    func: &mut Func<A>,
    iconst_op: u8,
    iconst_wide_op: u8,
    ty: Ty,
    value: u128,
    ptr_bits: u32,
    loc: u32,
) -> u32 {
    let bits = ty.bits(ptr_bits);
    if bits <= 32 {
        let mask = if bits == 32 {
            u32::MAX
        } else {
            (1u32 << bits).saturating_sub(1)
        };
        let word = trunc_u32(value) & mask;
        return push_data_inst(func, iconst_op, ty, word, loc);
    }
    let signed = sext(value, bits);
    if let Ok(small) = i32::try_from(signed) {
        return push_data_inst(func, iconst_op, ty, small.cast_unsigned(), loc);
    }
    let start = to_u32(func.extra.len());
    let words = ty.const_words();
    for w in 0..words {
        let shift = w.saturating_mul(32);
        func.extra.push(trunc_u32(value >> shift));
    }
    push_data_inst(func, iconst_wide_op, ty, start, loc)
}

fn clear_partition<A: Allocator>(writes: &mut Vec<ActiveWrite, A>, target: Partition) {
    let mut i = 0;
    while i < writes.len() {
        if writes.get(i).is_some_and(|w| w.partition == target) {
            writes.remove_swap(i);
        } else {
            i = i.saturating_add(1);
        }
    }
}

/// Invalidates or splits active writes in `NonEscapingSlot(slot)` overlapping `[off, off + len)`.
/// Uniform `MemsetByte` ranges are split around `[off, off + len)` when within capacity so
/// untouched sub-ranges remain foldable; SSA writes overlapping `[off, off + len)` are removed.
fn clobber_slot_range<A: Allocator>(
    writes: &mut Vec<ActiveWrite, A>,
    slot: u32,
    off: i64,
    len: i64,
) {
    let Some(c_end) = off.checked_add(len) else {
        clear_partition(writes, Partition::NonEscapingSlot(slot));
        return;
    };
    let mut split_add = [ActiveWrite {
        partition: Partition::Heap,
        base: 0,
        base_obj: BaseObj::Unknown,
        escaped_slot: NONE,
        off_known: false,
        off: 0,
        len: 0,
        value: WriteValue::MemsetByte(0),
    }; 4];
    let mut split_count = 0usize;

    let mut i = 0;
    while i < writes.len() {
        let Some(&w) = writes.get(i) else {
            break;
        };
        if w.partition == Partition::NonEscapingSlot(slot)
            && ranges_overlap(w.off, w.len, off, len)
        {
            writes.remove_swap(i);
            if let WriteValue::MemsetByte(b) = w.value
                && let Some(w_end) = w.off.checked_add(w.len)
            {
                if w.off < off
                    && let Some(left_len) = off.checked_sub(w.off)
                    && left_len > 0
                    && split_count < split_add.len()
                {
                    set_at(
                        &mut split_add,
                        split_count,
                        ActiveWrite {
                            partition: w.partition,
                            base: w.base,
                            base_obj: w.base_obj,
                            escaped_slot: NONE,
                            off_known: true,
                            off: w.off,
                            len: left_len,
                            value: WriteValue::MemsetByte(b),
                        },
                    );
                    split_count = split_count.saturating_add(1);
                }
                if c_end < w_end
                    && let Some(right_len) = w_end.checked_sub(c_end)
                    && right_len > 0
                    && split_count < split_add.len()
                {
                    set_at(
                        &mut split_add,
                        split_count,
                        ActiveWrite {
                            partition: w.partition,
                            base: w.base,
                            base_obj: w.base_obj,
                            escaped_slot: NONE,
                            off_known: true,
                            off: c_end,
                            len: right_len,
                            value: WriteValue::MemsetByte(b),
                        },
                    );
                    split_count = split_count.saturating_add(1);
                }
            }
        } else {
            i = i.saturating_add(1);
        }
    }

    for piece in split_add.get(..split_count).unwrap_or(&[]) {
        if writes.len() < MAX_ACTIVE_WRITES {
            writes.push(*piece);
        }
    }
}

/// Invalidates Heap active writes that may alias `loc` (never touches `NonEscapingSlot`s or
/// `ImmutableData`).
fn clobber_heap_by_loc<A: Allocator + Copy>(
    writes: &mut Vec<ActiveWrite, A>,
    alias: &AliasAnalysis<A>,
    loc: MemLoc,
) {
    let mut i = 0;
    while i < writes.len() {
        let Some(&w) = writes.get(i) else {
            break;
        };
        if w.partition == Partition::Heap {
            let w_loc = MemLoc {
                base: w.base,
                off: w.off,
                size: w.len,
                slot: w.escaped_slot,
                off_known: w.off_known,
                base_obj: w.base_obj,
            };
            if !alias.no_alias(w_loc, loc) {
                writes.remove_swap(i);
                continue;
            }
        }
        i = i.saturating_add(1);
    }
}

/// Invalidates Heap active writes whose canonical root may alias `dst_base` (used for dynamic-
/// length bulk writes `Memcpy`/`Memmove`/`Memset` and escaped slot lifetime markers; never
/// touches `NonEscapingSlot`s or `ImmutableData`).
fn clobber_heap_by_base<A: Allocator + Copy>(
    writes: &mut Vec<ActiveWrite, A>,
    alias: &AliasAnalysis<A>,
    dst_base: BaseObj,
) {
    let mut i = 0;
    while i < writes.len() {
        let Some(&w) = writes.get(i) else {
            break;
        };
        if w.partition == Partition::Heap && alias.may_alias_bases(w.base_obj, dst_base) {
            writes.remove_swap(i);
            continue;
        }
        i = i.saturating_add(1);
    }
}

#[inline]
fn push_active_write<A: Allocator>(writes: &mut Vec<ActiveWrite, A>, w: ActiveWrite) {
    if writes.len() >= MAX_ACTIVE_WRITES {
        writes.remove_swap(0);
    }
    writes.push(w);
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
struct LoopModSummary {
    /// `true` if any instruction in the loop writes to Heap/Escaped memory.
    mod_heap: bool,
    /// `true` if an opaque instruction (`Call`, `Invoke`, `Atomic*`, `Fence`, `Asm`,
    /// `TargetIntrinsic`, `MaskedStore`, `Scatter`, `Suspend`, volatile write, or unanalyzable
    /// store) modifies Heap/Escaped memory.
    opaque_heap: bool,
}

/// Computes which memory partitions (`heap` and each non-escaping slot) are modified anywhere inside
/// `loop_body`, and collects fine-grained heap store locations and bulk-write bases so provably
/// disjoint preheader heap entries remain available inside the loop.
fn scan_loop_modifications<A: Allocator + Copy>(
    view: &View<'_>,
    alias: &AliasAnalysis<A>,
    loop_body: &[u32],
    mod_slot: &mut [bool],
    heap_stores: &mut Vec<MemLoc, A>,
    heap_bulk_bases: &mut Vec<BaseObj, A>,
    clobbered_escaped_slots: &mut Vec<u32, A>,
) -> LoopModSummary {
    heap_stores.clear();
    heap_bulk_bases.clear();
    clobbered_escaped_slots.clear();
    let mut summary = LoopModSummary::default();
    for &bb in loop_body {
        for inst in view.block(Block(bb)) {
            let op = view.op(inst);
            match op {
                Op::Store => {
                    let at = to_idx(at_u32(view.data, inst));
                    let val = at_u32(view.extra, at);
                    let ptr = at_u32(view.extra, at.saturating_add(1));
                    let off = i64::from(at_u32(view.extra, at.saturating_add(2)).cast_signed());
                    let val_ty = Ty(at_u16(view.tys, to_idx(val)));
                    let volatile = Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE);
                    let s = alias.slot_of_val(ptr);
                    if s != NONE && alias.is_non_escaping(s) {
                        set_at(mod_slot, to_idx(s), true);
                    } else {
                        summary.mod_heap = true;
                        if volatile {
                            summary.opaque_heap = true;
                        } else if let Some(loc) = alias.decompose_loc(view, ptr, off, val_ty)
                            && loc.off.checked_add(loc.size).is_some()
                        {
                            heap_stores.push(loc);
                        } else {
                            heap_bulk_bases.push(alias.base_of(ptr));
                        }
                    }
                }
                Op::Memset | Op::Memcpy | Op::Memmove => {
                    let at = to_idx(at_u32(view.data, inst));
                    let dst = at_u32(view.extra, at);
                    let len_val = at_u32(view.extra, at.saturating_add(2));
                    let volatile = Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE);
                    let s = alias.slot_of_val(dst);
                    if s != NONE && alias.is_non_escaping(s) {
                        set_at(mod_slot, to_idx(s), true);
                    } else {
                        summary.mod_heap = true;
                        if volatile {
                            summary.opaque_heap = true;
                        } else {
                            let dst_base_obj = alias.base_of(dst);
                            let (dst_base, dst_off, dst_slot, dst_off_known) =
                                alias.resolve_ptr_const(view, dst, 0);
                            let const_len = const_u64(view, to_idx(len_val))
                                .and_then(|n| i64::try_from(n).ok())
                                .filter(|&n| n > 0);
                            if let Some(clen) = const_len
                                && dst_off.checked_add(clen).is_some()
                            {
                                heap_stores.push(MemLoc {
                                    base: dst_base,
                                    off: dst_off,
                                    size: clen,
                                    slot: dst_slot,
                                    off_known: dst_off_known,
                                    base_obj: dst_base_obj,
                                });
                            } else {
                                heap_bulk_bases.push(dst_base_obj);
                            }
                        }
                    }
                }
                Op::SlotLive | Op::SlotDead => {
                    let s = at_u32(view.data, inst);
                    if alias.is_non_escaping(s) {
                        set_at(mod_slot, to_idx(s), true);
                    } else {
                        summary.mod_heap = true;
                        clobbered_escaped_slots.push(s);
                        heap_bulk_bases.push(BaseObj::Slot(SlotId(s)));
                    }
                }
                Op::Suspend => {
                    summary.mod_heap = true;
                    summary.opaque_heap = true;
                    for m in mod_slot.iter_mut() {
                        *m = true;
                    }
                }
                Op::Call
                | Op::CallIndirect
                | Op::Invoke
                | Op::InvokeIndirect
                | Op::TailCall
                | Op::TailCallIndirect
                | Op::AtomicLoad
                | Op::AtomicStore
                | Op::AtomicRmw
                | Op::Cmpxchg
                | Op::Fence
                | Op::MaskedStore
                | Op::Scatter
                | Op::Asm
                | Op::AsmBr
                | Op::TargetIntrinsic => {
                    summary.mod_heap = true;
                    summary.opaque_heap = true;
                }
                _ => {}
            }
        }
    }
    summary
}

struct ForwardResult<A: Allocator> {
    func: Option<Func<A>>,
    loads_forwarded: u32,
    mem_phis_inserted: u32,
}

/// Runs unified MemorySSA forwarding over `view` in RPO order:
/// - Tracks isolated exact byte intervals + `Memset` ranges for proven non-escaping stack slots
///   and a separate `MemVersion` chain for Escaped/Heap memory.
/// - Inserts `MemPhi` barriers at loop headers and multi-predecessor joins so forwarding is
///   never unsound across loop backedges or differing predecessors, while tracking single-
///   predecessor dominator chains and loop-invariant partitions cleanly.
fn forward_loads_memssa<A: Allocator + Copy>(
    view: View<'_>,
    alias: &AliasAnalysis<A>,
    cfg: &CfgInfo<A>,
    alloc: A,
) -> Result<ForwardResult<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    let nslots = view.slots.len();
    if blocks == 0 || len == 0 {
        return Ok(ForwardResult { func: None, loads_forwarded: 0, mem_phis_inserted: 0 });
    }

    // Map each non-escaping slot to a compact index `0..ne_count` for SoA per-block version matrix.
    let mut slot_to_ne = filled(nslots, NONE, &alloc);
    let mut ne_to_slot: Vec<u32, A> = Vec::with_capacity_in(alias.non_escaping_count, alloc);
    for s in 0..nslots {
        let s32 = to_u32(s);
        if alias.is_non_escaping(s32) {
            set_at(&mut slot_to_ne, s, to_u32(ne_to_slot.len()));
            ne_to_slot.push(s32);
        }
    }
    let ne_count = ne_to_slot.len();
    let track_cross_block_slots =
        ne_count > 0 && blocks.saturating_mul(ne_count) <= MAX_TRACKED_SLOTS_MATRIX;

    let mut graph = MemSsaGraph::new(alloc);
    let mut rewrites = filled(len, LoadRewrite::Keep, &alloc);
    let mut forwarded = 0u32;

    // Per-block exit memory versions (SoA) and flat arena of exit `ActiveWrite` snapshots.
    let mut block_exit_heap = filled(blocks, MemVersion::LIVE_ON_ENTRY, &alloc);
    let mut block_exit_slot = if track_cross_block_slots {
        filled(blocks.saturating_mul(ne_count), MemVersion::LIVE_ON_ENTRY, &alloc)
    } else {
        Vec::new_in(alloc)
    };
    let mut exit_write_pool: Vec<ActiveWrite, A> = Vec::new_in(alloc);
    let mut exit_write_start = filled(blocks, 0u32, &alloc);
    let mut exit_write_len = filled(blocks, 0u32, &alloc);
    let mut block_done = filled(blocks, false, &alloc);

    // Scratch buffers reused across blocks (zero per-block heap churn).
    let mut cur_writes: Vec<ActiveWrite, A> = Vec::with_capacity_in(32, alloc);
    let mut cur_slot_ver = filled(ne_count, MemVersion::LIVE_ON_ENTRY, &alloc);
    let mut reach_preds: Vec<u32, A> = Vec::new_in(alloc);
    let mut phi_scratch: Vec<MemVersion, A> = Vec::new_in(alloc);
    let mut loop_in_scratch = filled(blocks, false, &alloc);
    let mut loop_body_scratch: Vec<u32, A> = Vec::new_in(alloc);
    let mut loop_work_scratch: Vec<u32, A> = Vec::new_in(alloc);
    let mut loop_mod_slot = filled(nslots, false, &alloc);
    let mut loop_heap_stores: Vec<MemLoc, A> = Vec::new_in(alloc);
    let mut loop_heap_bulk_bases: Vec<BaseObj, A> = Vec::new_in(alloc);
    let mut loop_clobbered_escaped: Vec<u32, A> = Vec::new_in(alloc);

    // Walk reachable blocks in RPO order!
    for &b in &cfg.rpo {
        let bi = to_idx(b);
        cur_writes.clear();
        reach_preds.clear();
        for &p in cfg.preds(b) {
            if cfg.reachable(p) {
                reach_preds.push(p);
            }
        }

        let mut cur_heap_ver = MemVersion::LIVE_ON_ENTRY;
        for v in cur_slot_ver.iter_mut() {
            *v = MemVersion::LIVE_ON_ENTRY;
        }

        if b == 0 || reach_preds.is_empty() {
            // Entry block: starts at `LIVE_ON_ENTRY` with empty active writes.
        } else if at_bool(&cfg.is_irreducible_header, bi) {
            // Irreducible loop header: opaque `MemPhi` barrier on mutable partitions;
            // immutable `DataAddr` loads from `idom[b]` remain valid forever.
            let idom_b = at_u32(&cfg.idom, bi);
            if idom_b != NONE && at_bool(&block_done, to_idx(idom_b)) {
                let di = to_idx(idom_b);
                let ws = to_idx(at_u32(&exit_write_start, di));
                let wl = to_idx(at_u32(&exit_write_len, di));
                for &w in exit_write_pool.get(ws..ws.saturating_add(wl)).unwrap_or(&[]) {
                    if w.partition == Partition::ImmutableData {
                        push_active_write(&mut cur_writes, w);
                    }
                }
            }
            phi_scratch.clear();
            for &p in &reach_preds {
                let pv = if at_bool(&block_done, to_idx(p)) {
                    at_ver(&block_exit_heap, to_idx(p))
                } else {
                    MemVersion::LIVE_ON_ENTRY
                };
                phi_scratch.push(pv);
            }
            cur_heap_ver = graph.push_phi(b, &phi_scratch);
            for ne in 0..ne_count {
                let nv = graph.push_phi(b, &phi_scratch);
                set_at(&mut cur_slot_ver, ne, nv);
            }
        } else if at_bool(&cfg.is_loop_header, bi) {
            // Reducible natural loop header: compute loop body & modified partitions.
            let (outside_count, preheader) = cfg.collect_natural_loop(
                b,
                &mut loop_in_scratch,
                &mut loop_body_scratch,
                &mut loop_work_scratch,
            );
            for m in loop_mod_slot.iter_mut() {
                *m = false;
            }
            let loop_mod = scan_loop_modifications(
                &view,
                alias,
                &loop_body_scratch,
                &mut loop_mod_slot,
                &mut loop_heap_stores,
                &mut loop_heap_bulk_bases,
                &mut loop_clobbered_escaped,
            );
            let single_pre_ready = outside_count == 1
                && preheader != NONE
                && at_bool(&block_done, to_idx(preheader));

            // Immutable `DataAddr` loads from `idom[b]` (or `preheader`) are never clobbered.
            let imm_src = if single_pre_ready {
                preheader
            } else {
                at_u32(&cfg.idom, bi)
            };
            if imm_src != NONE && at_bool(&block_done, to_idx(imm_src)) {
                let si = to_idx(imm_src);
                let ws = to_idx(at_u32(&exit_write_start, si));
                let wl = to_idx(at_u32(&exit_write_len, si));
                for &w in exit_write_pool.get(ws..ws.saturating_add(wl)).unwrap_or(&[]) {
                    if w.partition == Partition::ImmutableData {
                        push_active_write(&mut cur_writes, w);
                    }
                }
            }

            // Heap partition at loop header.
            if !loop_mod.mod_heap && single_pre_ready {
                let pi = to_idx(preheader);
                cur_heap_ver = at_ver(&block_exit_heap, pi);
                let ws = to_idx(at_u32(&exit_write_start, pi));
                let wl = to_idx(at_u32(&exit_write_len, pi));
                for &w in exit_write_pool.get(ws..ws.saturating_add(wl)).unwrap_or(&[]) {
                    if w.partition == Partition::Heap {
                        push_active_write(&mut cur_writes, w);
                    }
                }
            } else {
                phi_scratch.clear();
                for &p in &reach_preds {
                    let pv = if at_bool(&block_done, to_idx(p)) {
                        at_ver(&block_exit_heap, to_idx(p))
                    } else {
                        MemVersion::LIVE_ON_ENTRY
                    };
                    phi_scratch.push(pv);
                }
                cur_heap_ver = graph.push_phi(b, &phi_scratch);
                // Even when the loop modifies Heap memory, preheader writes/loads on bases
                // provably disjoint from every store/bulk-write in the loop survive unchanged!
                if single_pre_ready && !loop_mod.opaque_heap {
                    let pi = to_idx(preheader);
                    let ws = to_idx(at_u32(&exit_write_start, pi));
                    let wl = to_idx(at_u32(&exit_write_len, pi));
                    for &w in exit_write_pool.get(ws..ws.saturating_add(wl)).unwrap_or(&[]) {
                        if w.partition != Partition::Heap {
                            continue;
                        }
                        if w.escaped_slot != NONE
                            && loop_clobbered_escaped.iter().any(|&s| s == w.escaped_slot)
                        {
                            continue;
                        }
                        let w_loc = MemLoc {
                            base: w.base,
                            off: w.off,
                            size: w.len,
                            slot: w.escaped_slot,
                            off_known: w.off_known,
                            base_obj: w.base_obj,
                        };
                        let clobbered_by_store =
                            loop_heap_stores.iter().any(|&st| !alias.no_alias(w_loc, st));
                        let clobbered_by_bulk = loop_heap_bulk_bases
                            .iter()
                            .any(|&b_obj| alias.may_alias_bases(w.base_obj, b_obj));
                        if !clobbered_by_store && !clobbered_by_bulk {
                            push_active_write(&mut cur_writes, w);
                        }
                    }
                }
            }

            // Per non-escaping slot at loop header: loop-invariant slots inherit preheader;
            // loop-modified slots receive a `MemPhi` barrier (preventing unsound backedge forwarding!).
            for ne in 0..ne_count {
                let s = at_u32(&ne_to_slot, ne);
                if !at_bool(&loop_mod_slot, to_idx(s))
                    && single_pre_ready
                    && track_cross_block_slots
                {
                    let pi = to_idx(preheader);
                    let sv = at_ver(&block_exit_slot, pi.saturating_mul(ne_count).saturating_add(ne));
                    set_at(&mut cur_slot_ver, ne, sv);
                    let ws = to_idx(at_u32(&exit_write_start, pi));
                    let wl = to_idx(at_u32(&exit_write_len, pi));
                    for &w in exit_write_pool.get(ws..ws.saturating_add(wl)).unwrap_or(&[]) {
                        if w.partition == Partition::NonEscapingSlot(s) {
                            push_active_write(&mut cur_writes, w);
                        }
                    }
                } else {
                    phi_scratch.clear();
                    for &p in &reach_preds {
                        let pv = if track_cross_block_slots && at_bool(&block_done, to_idx(p)) {
                            at_ver(
                                &block_exit_slot,
                                to_idx(p).saturating_mul(ne_count).saturating_add(ne),
                            )
                        } else {
                            MemVersion::LIVE_ON_ENTRY
                        };
                        phi_scratch.push(pv);
                    }
                    let nv = graph.push_phi(b, &phi_scratch);
                    set_at(&mut cur_slot_ver, ne, nv);
                }
            }
        } else if reach_preds.len() == 1 {
            // Single-predecessor dominator chain: inherit predecessor's exact exit state.
            let p = at_u32(&reach_preds, 0);
            let pi = to_idx(p);
            if at_bool(&block_done, pi) {
                cur_heap_ver = at_ver(&block_exit_heap, pi);
                if track_cross_block_slots {
                    let base_off = pi.saturating_mul(ne_count);
                    for ne in 0..ne_count {
                        set_at(
                            &mut cur_slot_ver,
                            ne,
                            at_ver(&block_exit_slot, base_off.saturating_add(ne)),
                        );
                    }
                }
                let ws = to_idx(at_u32(&exit_write_start, pi));
                let wl = to_idx(at_u32(&exit_write_len, pi));
                for &w in exit_write_pool.get(ws..ws.saturating_add(wl)).unwrap_or(&[]) {
                    push_active_write(&mut cur_writes, w);
                }
            }
        } else {
            // Multi-predecessor acyclic join: preserve partitions unmodified across all incoming
            // paths from `idom[b]`; insert `MemPhi` barriers on any partition modified on any path.
            let idom_b = at_u32(&cfg.idom, bi);
            let idom_ready = idom_b != NONE && at_bool(&block_done, to_idx(idom_b));
            // Immutable `DataAddr` loads from `idom[b]` are always valid across any join.
            if idom_ready {
                let di = to_idx(idom_b);
                let ws = to_idx(at_u32(&exit_write_start, di));
                let wl = to_idx(at_u32(&exit_write_len, di));
                for &w in exit_write_pool.get(ws..ws.saturating_add(wl)).unwrap_or(&[]) {
                    if w.partition == Partition::ImmutableData {
                        push_active_write(&mut cur_writes, w);
                    }
                }
            }
            let idom_heap = if idom_ready {
                at_ver(&block_exit_heap, to_idx(idom_b))
            } else {
                MemVersion::LIVE_ON_ENTRY
            };
            let mut heap_all_match = idom_ready;
            phi_scratch.clear();
            for &p in &reach_preds {
                let pv = if at_bool(&block_done, to_idx(p)) {
                    at_ver(&block_exit_heap, to_idx(p))
                } else {
                    heap_all_match = false;
                    MemVersion::LIVE_ON_ENTRY
                };
                if pv != idom_heap {
                    heap_all_match = false;
                }
                phi_scratch.push(pv);
            }
            if heap_all_match {
                cur_heap_ver = idom_heap;
                let di = to_idx(idom_b);
                let ws = to_idx(at_u32(&exit_write_start, di));
                let wl = to_idx(at_u32(&exit_write_len, di));
                for &w in exit_write_pool.get(ws..ws.saturating_add(wl)).unwrap_or(&[]) {
                    if w.partition == Partition::Heap {
                        push_active_write(&mut cur_writes, w);
                    }
                }
            } else {
                cur_heap_ver = graph.push_phi(b, &phi_scratch);
            }

            if track_cross_block_slots {
                for ne in 0..ne_count {
                    let s = at_u32(&ne_to_slot, ne);
                    let idom_sv = if idom_ready {
                        at_ver(
                            &block_exit_slot,
                            to_idx(idom_b).saturating_mul(ne_count).saturating_add(ne),
                        )
                    } else {
                        MemVersion::LIVE_ON_ENTRY
                    };
                    let mut slot_all_match = idom_ready;
                    phi_scratch.clear();
                    for &p in &reach_preds {
                        let pv = if at_bool(&block_done, to_idx(p)) {
                            at_ver(
                                &block_exit_slot,
                                to_idx(p).saturating_mul(ne_count).saturating_add(ne),
                            )
                        } else {
                            slot_all_match = false;
                            MemVersion::LIVE_ON_ENTRY
                        };
                        if pv != idom_sv {
                            slot_all_match = false;
                        }
                        phi_scratch.push(pv);
                    }
                    if slot_all_match {
                        set_at(&mut cur_slot_ver, ne, idom_sv);
                        let di = to_idx(idom_b);
                        let ws = to_idx(at_u32(&exit_write_start, di));
                        let wl = to_idx(at_u32(&exit_write_len, di));
                        for &w in exit_write_pool.get(ws..ws.saturating_add(wl)).unwrap_or(&[]) {
                            if w.partition == Partition::NonEscapingSlot(s) {
                                push_active_write(&mut cur_writes, w);
                            }
                        }
                    } else {
                        let nv = graph.push_phi(b, &phi_scratch);
                        set_at(&mut cur_slot_ver, ne, nv);
                    }
                }
            }
        }

        // Process instructions in block `b` in program order.
        for inst in view.block(Block(b)) {
            let op = view.op(inst);
            match op {
                Op::Store => {
                    let at = to_idx(at_u32(view.data, inst));
                    let val = at_u32(view.extra, at);
                    let ptr = at_u32(view.extra, at.saturating_add(1));
                    let off = i64::from(at_u32(view.extra, at.saturating_add(2)).cast_signed());
                    let val_ty = Ty(at_u16(view.tys, to_idx(val)));
                    let volatile = Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE);

                    if let Some(loc) = alias.decompose_loc(&view, ptr, off, val_ty) {
                        if loc.slot != NONE && alias.is_non_escaping(loc.slot) {
                            let ne = at_u32(&slot_to_ne, to_idx(loc.slot));
                            if ne != NONE {
                                let prev = at_ver(&cur_slot_ver, to_idx(ne));
                                let next = graph.push_def(to_u32(inst), prev);
                                set_at(&mut cur_slot_ver, to_idx(ne), next);
                            }
                            if !loc.off_known
                                || volatile
                                || loc.off < 0
                                || loc.off.checked_add(loc.size).is_none()
                            {
                                clear_partition(
                                    &mut cur_writes,
                                    Partition::NonEscapingSlot(loc.slot),
                                );
                            } else {
                                clobber_slot_range(&mut cur_writes, loc.slot, loc.off, loc.size);
                                if view.op(to_idx(val)) != Op::Uninit {
                                    push_active_write(
                                        &mut cur_writes,
                                        ActiveWrite {
                                            partition: Partition::NonEscapingSlot(loc.slot),
                                            base: loc.slot,
                                            base_obj: loc.base_obj,
                                            escaped_slot: NONE,
                                            off_known: true,
                                            off: loc.off,
                                            len: loc.size,
                                            value: WriteValue::Ssa { val, ty: val_ty },
                                        },
                                    );
                                }
                            }
                        } else {
                            // Heap / Escaped store: ONLY advances `Heap` version, NEVER touches
                            // proven non-escaping stack slots or immutable `DataAddr`!
                            cur_heap_ver = graph.push_def(to_u32(inst), cur_heap_ver);
                            if volatile || loc.off.checked_add(loc.size).is_none() {
                                clear_partition(&mut cur_writes, Partition::Heap);
                            } else {
                                clobber_heap_by_loc(&mut cur_writes, alias, loc);
                                if view.op(to_idx(val)) != Op::Uninit
                                    && (loc.base != NONE || loc.off_known)
                                    && !loc.base_obj.is_immutable_data()
                                {
                                    push_active_write(
                                        &mut cur_writes,
                                        ActiveWrite {
                                            partition: Partition::Heap,
                                            base: loc.base,
                                            base_obj: loc.base_obj,
                                            escaped_slot: loc.slot,
                                            off_known: loc.off_known,
                                            off: loc.off,
                                            len: loc.size,
                                            value: WriteValue::Ssa { val, ty: val_ty },
                                        },
                                    );
                                }
                            }
                        }
                    } else {
                        let ptr_slot = alias.slot_of_val(ptr);
                        if ptr_slot != NONE && alias.is_non_escaping(ptr_slot) {
                            let ne = at_u32(&slot_to_ne, to_idx(ptr_slot));
                            if ne != NONE {
                                let prev = at_ver(&cur_slot_ver, to_idx(ne));
                                let next = graph.push_def(to_u32(inst), prev);
                                set_at(&mut cur_slot_ver, to_idx(ne), next);
                            }
                            clear_partition(&mut cur_writes, Partition::NonEscapingSlot(ptr_slot));
                        } else {
                            cur_heap_ver = graph.push_def(to_u32(inst), cur_heap_ver);
                            if volatile {
                                clear_partition(&mut cur_writes, Partition::Heap);
                            } else {
                                clobber_heap_by_base(&mut cur_writes, alias, alias.base_of(ptr));
                            }
                        }
                    }
                }
                Op::Memset => {
                    let at = to_idx(at_u32(view.data, inst));
                    let dst = at_u32(view.extra, at);
                    let byte_val = at_u32(view.extra, at.saturating_add(1));
                    let len_val = at_u32(view.extra, at.saturating_add(2));
                    let volatile = Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE);
                    let dst_base_obj = alias.base_of(dst);
                    let (dst_base, dst_off, dst_slot, dst_off_known) =
                        alias.resolve_ptr_const(&view, dst, 0);
                    let const_len = const_u64(&view, to_idx(len_val))
                        .and_then(|n| i64::try_from(n).ok())
                        .filter(|&n| n > 0);
                    let const_byte = const_u8(&view, to_idx(byte_val));

                    if dst_slot != NONE && alias.is_non_escaping(dst_slot) {
                        let ne = at_u32(&slot_to_ne, to_idx(dst_slot));
                        if ne != NONE {
                            let prev = at_ver(&cur_slot_ver, to_idx(ne));
                            let next = graph.push_def(to_u32(inst), prev);
                            set_at(&mut cur_slot_ver, to_idx(ne), next);
                        }
                        if !volatile
                            && dst_off_known
                            && dst_off >= 0
                            && let Some(mlen) = const_len
                            && let Ok(mlen_u64) = u64::try_from(mlen)
                            && dst_off.checked_add(mlen).is_some()
                            && slot_range_in_bounds(&view, dst_slot, dst_off, mlen_u64)
                        {
                            clobber_slot_range(&mut cur_writes, dst_slot, dst_off, mlen);
                            if let Some(byte) = const_byte {
                                push_active_write(
                                    &mut cur_writes,
                                    ActiveWrite {
                                        partition: Partition::NonEscapingSlot(dst_slot),
                                        base: dst_slot,
                                        base_obj: dst_base_obj,
                                        escaped_slot: NONE,
                                        off_known: true,
                                        off: dst_off,
                                        len: mlen,
                                        value: WriteValue::MemsetByte(byte),
                                    },
                                );
                            }
                        } else {
                            clear_partition(&mut cur_writes, Partition::NonEscapingSlot(dst_slot));
                        }
                    } else {
                        // Heap/Escaped Memset: skips provably disjoint Heap bases (`may_alias_bases`),
                        // NEVER touches non-escaping slots or immutable `DataAddr`.
                        cur_heap_ver = graph.push_def(to_u32(inst), cur_heap_ver);
                        if volatile {
                            clear_partition(&mut cur_writes, Partition::Heap);
                        } else if let Some(mlen) = const_len
                            && dst_off.checked_add(mlen).is_some()
                        {
                            let dst_loc = MemLoc {
                                base: dst_base,
                                off: dst_off,
                                size: mlen,
                                slot: dst_slot,
                                off_known: dst_off_known,
                                base_obj: dst_base_obj,
                            };
                            clobber_heap_by_loc(&mut cur_writes, alias, dst_loc);
                            if (dst_base != NONE || dst_off_known)
                                && !dst_base_obj.is_immutable_data()
                                && let Some(byte) = const_byte
                            {
                                push_active_write(
                                    &mut cur_writes,
                                    ActiveWrite {
                                        partition: Partition::Heap,
                                        base: dst_base,
                                        base_obj: dst_base_obj,
                                        escaped_slot: dst_slot,
                                        off_known: dst_off_known,
                                        off: dst_off,
                                        len: mlen,
                                        value: WriteValue::MemsetByte(byte),
                                    },
                                );
                            }
                        } else {
                            clobber_heap_by_base(&mut cur_writes, alias, dst_base_obj);
                        }
                    }
                }
                Op::Memcpy | Op::Memmove => {
                    let at = to_idx(at_u32(view.data, inst));
                    let dst = at_u32(view.extra, at);
                    let len_val = at_u32(view.extra, at.saturating_add(2));
                    let volatile = Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE);
                    let dst_base_obj = alias.base_of(dst);
                    let (dst_base, dst_off, dst_slot, dst_off_known) =
                        alias.resolve_ptr_const(&view, dst, 0);
                    let const_len = const_u64(&view, to_idx(len_val))
                        .and_then(|n| i64::try_from(n).ok())
                        .filter(|&n| n > 0);
                    if dst_slot != NONE && alias.is_non_escaping(dst_slot) {
                        let ne = at_u32(&slot_to_ne, to_idx(dst_slot));
                        if ne != NONE {
                            let prev = at_ver(&cur_slot_ver, to_idx(ne));
                            let next = graph.push_def(to_u32(inst), prev);
                            set_at(&mut cur_slot_ver, to_idx(ne), next);
                        }
                        if !volatile
                            && dst_off_known
                            && dst_off >= 0
                            && let Some(clen) = const_len
                            && dst_off.checked_add(clen).is_some()
                        {
                            clobber_slot_range(&mut cur_writes, dst_slot, dst_off, clen);
                        } else {
                            clear_partition(&mut cur_writes, Partition::NonEscapingSlot(dst_slot));
                        }
                    } else {
                        cur_heap_ver = graph.push_def(to_u32(inst), cur_heap_ver);
                        if volatile {
                            clear_partition(&mut cur_writes, Partition::Heap);
                        } else if let Some(clen) = const_len
                            && dst_off.checked_add(clen).is_some()
                        {
                            let dst_loc = MemLoc {
                                base: dst_base,
                                off: dst_off,
                                size: clen,
                                slot: dst_slot,
                                off_known: dst_off_known,
                                base_obj: dst_base_obj,
                            };
                            clobber_heap_by_loc(&mut cur_writes, alias, dst_loc);
                        } else {
                            clobber_heap_by_base(&mut cur_writes, alias, dst_base_obj);
                        }
                    }
                }
                Op::Load => {
                    if Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE) {
                        continue;
                    }
                    let load_ty = Ty(at_u16(view.tys, inst));
                    let at = to_idx(at_u32(view.data, inst));
                    let ptr = at_u32(view.extra, at);
                    let off = i64::from(at_u32(view.extra, at.saturating_add(1)).cast_signed());
                    let Some(loc) = alias.decompose_loc(&view, ptr, off, load_ty) else {
                        continue;
                    };
                    let Some(load_end) = loc.off.checked_add(loc.size) else {
                        continue;
                    };

                    let (partition, key_base) = if loc.base_obj.is_immutable_data() {
                        (Partition::ImmutableData, loc.base)
                    } else if loc.slot != NONE && alias.is_non_escaping(loc.slot) {
                        if !loc.off_known || loc.off < 0 {
                            continue;
                        }
                        (Partition::NonEscapingSlot(loc.slot), loc.slot)
                    } else {
                        if loc.base == NONE && !loc.off_known {
                            continue;
                        }
                        (Partition::Heap, loc.base)
                    };

                    // Priority 1: Exact `[off, off + size)` match on SSA store / dominating load
                    // (redundant-load CSE), OR strict containment `[l_lo, l_hi) ⊆ [m_lo, m_hi)`
                    // inside a uniform `MemsetByte`.
                    let mut matched = false;
                    for &w in &cur_writes {
                        if w.partition != partition {
                            continue;
                        }
                        let same_base = match partition {
                            Partition::NonEscapingSlot(_) => w.base == key_base,
                            Partition::ImmutableData => {
                                w.base_obj == loc.base_obj
                                    && ((w.off_known && loc.off_known)
                                        || (w.base != NONE && w.base == loc.base))
                            }
                            Partition::Heap => {
                                let same_root = w.base_obj != BaseObj::Unknown
                                    && w.base_obj == loc.base_obj
                                    && w.off_known
                                    && loc.off_known;
                                let same_ssa = w.base != NONE && w.base == loc.base;
                                same_root || same_ssa
                            }
                        };
                        if !same_base {
                            continue;
                        }
                        match w.value {
                            WriteValue::Ssa { val, ty } => {
                                if w.off == loc.off
                                    && w.len == loc.size
                                    && ty == load_ty
                                    && cfg.dominates(at_u32(&cfg.inst_block, to_idx(val)), b)
                                {
                                    set_at(&mut rewrites, inst, LoadRewrite::ForwardVal(val));
                                    forwarded = forwarded.saturating_add(1);
                                    matched = true;
                                    break;
                                }
                            }
                            WriteValue::MemsetByte(byte) => {
                                if loc.off >= w.off
                                    && w.off.checked_add(w.len).is_some_and(|w_end| load_end <= w_end)
                                    && let Some(val_u128) = repeated_byte_int(load_ty, byte)
                                {
                                    set_at(
                                        &mut rewrites,
                                        inst,
                                        LoadRewrite::ConstInt { ty: load_ty, value: val_u128 },
                                    );
                                    forwarded = forwarded.saturating_add(1);
                                    matched = true;
                                    break;
                                }
                            }
                        }
                    }
                    if matched {
                        continue;
                    }

                    // Priority 2: Little-endian sub-word `NarrowFromWide` & mixed-width
                    // `WideFromPieces` forwarding on proven non-escaping stack slots.
                    let mut subword_matched = false;
                    if cfg!(target_endian = "little")
                        && matches!(partition, Partition::NonEscapingSlot(_))
                        && let Some(load_bytes) = scalar_int_bytes(load_ty)
                    {
                        // Case 2a: Narrow scalar-int load completely inside an enclosing wider store.
                        for &w in &cur_writes {
                            if w.partition != partition || w.base != key_base {
                                continue;
                            }
                            if let WriteValue::Ssa { val, ty: store_ty } = w.value
                                && let Some(st_bytes) = scalar_int_bytes(store_ty)
                                && load_bytes < st_bytes
                                && loc.off >= w.off
                                && w.off.checked_add(st_bytes).is_some_and(|w_end| load_end <= w_end)
                                && cfg.dominates(at_u32(&cfg.inst_block, to_idx(val)), b)
                                && let Some(delta) = loc.off.checked_sub(w.off)
                                && let Some(bits_i64) = delta.checked_mul(8)
                                && let Ok(shift_bits) = u32::try_from(bits_i64)
                            {
                                set_at(
                                    &mut rewrites,
                                    inst,
                                    LoadRewrite::NarrowFromWide {
                                        store_val: val,
                                        store_ty,
                                        load_ty,
                                        shift_bits,
                                    },
                                );
                                forwarded = forwarded.saturating_add(1);
                                subword_matched = true;
                                break;
                            }
                        }

                        // Case 2b: `I16`/`I32`/`I64` load reconstructed from strictly disjoint active
                        // narrower scalar-int stores (`I8`/`I16`/`I32`) completely covering `[loc.off, load_end)`.
                        if !subword_matched
                            && let Some(full_mask) = load_full_byte_mask(load_bytes)
                        {
                            let mut pieces = [EMPTY_PIECE; 8];
                            let mut count: usize = 0;
                            let mut covered_mask: u8 = 0;
                            let mut partial_conflict = false;
                            for &w in &cur_writes {
                                if w.partition != partition
                                    || w.base != key_base
                                    || !ranges_overlap(w.off, w.len, loc.off, load_bytes)
                                {
                                    continue;
                                }
                                let WriteValue::Ssa { val, ty: store_ty } = w.value else {
                                    partial_conflict = true;
                                    break;
                                };
                                let Some(st_bytes) = scalar_int_bytes(store_ty) else {
                                    partial_conflict = true;
                                    break;
                                };
                                if !cfg.dominates(at_u32(&cfg.inst_block, to_idx(val)), b) {
                                    partial_conflict = true;
                                    break;
                                }
                                let Some(st_end) = w.off.checked_add(st_bytes) else {
                                    partial_conflict = true;
                                    break;
                                };
                                if st_bytes >= load_bytes || w.off < loc.off || st_end > load_end {
                                    partial_conflict = true;
                                    break;
                                }
                                let Some(rel_i64) = w.off.checked_sub(loc.off) else {
                                    partial_conflict = true;
                                    break;
                                };
                                let (Ok(byte_off), Ok(byte_width)) =
                                    (u8::try_from(rel_i64), u8::try_from(st_bytes))
                                else {
                                    partial_conflict = true;
                                    break;
                                };
                                let Some(mask) = piece_byte_mask(byte_off, byte_width) else {
                                    partial_conflict = true;
                                    break;
                                };
                                if (covered_mask & mask) != 0 || count >= 8 {
                                    partial_conflict = true;
                                    break;
                                }
                                set_at(&mut pieces, count, Piece { val, byte_off });
                                count = count.saturating_add(1);
                                covered_mask |= mask;
                            }
                            if !partial_conflict && count >= 2 && covered_mask == full_mask {
                                if let Some(slice) = pieces.get_mut(..count) {
                                    slice.sort_unstable_by_key(|p| p.byte_off);
                                }
                                if pieces.first().is_some_and(|p| p.byte_off == 0)
                                    && let Ok(piece_count) = u8::try_from(count)
                                {
                                    set_at(
                                        &mut rewrites,
                                        inst,
                                        LoadRewrite::WideFromPieces {
                                            load_ty,
                                            pieces,
                                            count: piece_count,
                                        },
                                    );
                                    forwarded = forwarded.saturating_add(1);
                                    subword_matched = true;
                                }
                            }
                        }
                    }

                    // Record unmatched non-volatile `Op::Load` as an available value for
                    // redundant-load CSE across provably disjoint stores, bulk ops, and calls.
                    if !subword_matched {
                        let rec_base = match partition {
                            Partition::NonEscapingSlot(s) => s,
                            Partition::ImmutableData | Partition::Heap => loc.base,
                        };
                        let rec_escaped_slot = match partition {
                            Partition::Heap => loc.slot,
                            Partition::NonEscapingSlot(_) | Partition::ImmutableData => NONE,
                        };
                        let can_record = match partition {
                            Partition::NonEscapingSlot(_) => loc.off_known && loc.off >= 0,
                            Partition::ImmutableData | Partition::Heap => {
                                loc.base != NONE || loc.off_known
                            }
                        };
                        if can_record {
                            push_active_write(
                                &mut cur_writes,
                                ActiveWrite {
                                    partition,
                                    base: rec_base,
                                    base_obj: loc.base_obj,
                                    escaped_slot: rec_escaped_slot,
                                    off_known: loc.off_known,
                                    off: loc.off,
                                    len: loc.size,
                                    value: WriteValue::Ssa { val: to_u32(inst), ty: load_ty },
                                },
                            );
                        }
                    }
                }
                Op::SlotLive | Op::SlotDead => {
                    let s = at_u32(view.data, inst);
                    if alias.is_non_escaping(s) {
                        let ne = at_u32(&slot_to_ne, to_idx(s));
                        if ne != NONE {
                            let prev = at_ver(&cur_slot_ver, to_idx(ne));
                            let next = graph.push_def(to_u32(inst), prev);
                            set_at(&mut cur_slot_ver, to_idx(ne), next);
                        }
                        clear_partition(&mut cur_writes, Partition::NonEscapingSlot(s));
                    } else {
                        cur_heap_ver = graph.push_def(to_u32(inst), cur_heap_ver);
                        clobber_heap_by_base(&mut cur_writes, alias, BaseObj::Slot(SlotId(s)));
                    }
                }
                Op::Call
                | Op::CallIndirect
                | Op::Invoke
                | Op::InvokeIndirect
                | Op::TailCall
                | Op::TailCallIndirect
                | Op::AtomicLoad
                | Op::AtomicStore
                | Op::AtomicRmw
                | Op::Cmpxchg
                | Op::Fence
                | Op::MaskedStore
                | Op::Scatter
                | Op::Asm
                | Op::AsmBr
                | Op::TargetIntrinsic => {
                    // Non-aliasing guarantee: ONLY advance Escaped/Heap `MemVersion` and clear
                    // Heap/escaped writes; NEVER clobber proven non-escaping `Op::Slot` intervals
                    // or immutable `DataAddr` loads!
                    cur_heap_ver = graph.push_def(to_u32(inst), cur_heap_ver);
                    clear_partition(&mut cur_writes, Partition::Heap);
                }
                Op::Suspend => {
                    // Coroutine suspension parks the frame; clobber mutable partitions while
                    // preserving global immutable `DataAddr` loads.
                    cur_heap_ver = graph.push_def(to_u32(inst), cur_heap_ver);
                    for ne in 0..ne_count {
                        let prev = at_ver(&cur_slot_ver, ne);
                        let next = graph.push_def(to_u32(inst), prev);
                        set_at(&mut cur_slot_ver, ne, next);
                    }
                    let mut i = 0;
                    while i < cur_writes.len() {
                        if cur_writes.get(i).is_some_and(|w| w.partition != Partition::ImmutableData)
                        {
                            cur_writes.remove_swap(i);
                        } else {
                            i = i.saturating_add(1);
                        }
                    }
                }
                _ => {}
            }
        }

        // Save block exit versions and snapshot active writes into flat pool (bounded).
        set_at(&mut block_exit_heap, bi, cur_heap_ver);
        if track_cross_block_slots {
            let base_off = bi.saturating_mul(ne_count);
            for ne in 0..ne_count {
                set_at(
                    &mut block_exit_slot,
                    base_off.saturating_add(ne),
                    at_ver(&cur_slot_ver, ne),
                );
            }
        }
        if exit_write_pool.len().saturating_add(cur_writes.len()) <= MAX_SAVED_WRITE_POOL {
            let start = to_u32(exit_write_pool.len());
            for &w in &cur_writes {
                exit_write_pool.push(w);
            }
            set_at(&mut exit_write_start, bi, start);
            set_at(&mut exit_write_len, bi, to_u32(cur_writes.len()));
        }
        set_at(&mut block_done, bi, true);
    }

    if forwarded == 0 {
        return Ok(ForwardResult {
            func: None,
            loads_forwarded: 0,
            mem_phis_inserted: graph.mem_phis_inserted,
        });
    }

    // Pass 1a: precompute exact old->new instruction index for all emitted instructions.
    let mut val_map = filled(len, NONE, &alloc);
    let mut next_idx: usize = 0;
    for inst in 0..len {
        let rw = rewrites.get(inst).copied().unwrap_or(LoadRewrite::Keep);
        let emitted = rewrite_inst_count(rw);
        if emitted > 0 {
            let result_idx = next_idx.saturating_add(emitted.saturating_sub(1));
            set_at(&mut val_map, inst, to_u32(result_idx));
            next_idx = next_idx.saturating_add(emitted);
        }
    }
    if next_idx >= to_idx(NONE) {
        return Err(SimplifyError::Refused { inst: 0, what: "more values than LIR numbers" });
    }

    // Pass 1b: resolve `ForwardVal` chains transitively (bounded by `len`, handles non-RPO layout).
    for inst in 0..len {
        if let LoadRewrite::ForwardVal(mut target) =
            rewrites.get(inst).copied().unwrap_or(LoadRewrite::Keep)
        {
            for _ in 0..=len {
                match rewrites.get(to_idx(target)).copied().unwrap_or(LoadRewrite::Keep) {
                    LoadRewrite::ForwardVal(next) if next != target => target = next,
                    _ => break,
                }
            }
            let mapped = at_u32(&val_map, to_idx(target));
            if mapped == NONE {
                return Err(SimplifyError::Refused {
                    inst,
                    what: "a forwarded load resolving to a removed value",
                });
            }
            set_at(&mut val_map, inst, mapped);
        }
    }

    // Pass 2: rebuild instructions in layout order.
    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    let iconst_op = op_byte(Op::Iconst);
    let iconst_wide_op = op_byte(Op::IconstWide);
    let lshr_op = op_byte(Op::Lshr);
    let shl_op = op_byte(Op::Shl);
    let or_op = op_byte(Op::Or);
    let trunc_op = op_byte(Op::Trunc);
    let zext_op = op_byte(Op::Zext);
    let ptr_bits = view.head.ptr_bits;

    for b in 0..blocks {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        for inst in view.block(Block(b32)) {
            let loc = at_u32(view.locs, inst);
            let rw = rewrites.get(inst).copied().unwrap_or(LoadRewrite::Keep);
            match rw {
                LoadRewrite::Keep => {
                    let _ = copy_inst_same_blocks(&view, inst, &val_map, &mut func);
                }
                LoadRewrite::ForwardVal(_) => {
                    // Eliminated: every user reads the dominating SSA value via `val_map`.
                }
                LoadRewrite::ConstInt { ty, value } => {
                    let _ = emit_int_const(
                        &mut func,
                        iconst_op,
                        iconst_wide_op,
                        ty,
                        value,
                        ptr_bits,
                        loc,
                    );
                }
                LoadRewrite::NarrowFromWide { store_val, store_ty, load_ty, shift_bits } => {
                    let src_new = at_u32(&val_map, to_idx(store_val));
                    if shift_bits == 0 {
                        let _ = push_data_inst(&mut func, trunc_op, load_ty, src_new, loc);
                    } else {
                        let sh = push_data_inst(&mut func, iconst_op, store_ty, shift_bits, loc);
                        let shifted = push_vv_inst(&mut func, lshr_op, store_ty, src_new, sh, loc);
                        let _ = push_data_inst(&mut func, trunc_op, load_ty, shifted, loc);
                    }
                }
                LoadRewrite::WideFromPieces { load_ty, pieces, count } => {
                    let k = usize::from(count);
                    let first_old = pieces.first().map_or(NONE, |p| p.val);
                    let first_new = at_u32(&val_map, to_idx(first_old));
                    let mut acc = push_data_inst(&mut func, zext_op, load_ty, first_new, loc);
                    for j in 1..k {
                        let piece = pieces.get(j).copied().unwrap_or(EMPTY_PIECE);
                        let p_new = at_u32(&val_map, to_idx(piece.val));
                        let ext = push_data_inst(&mut func, zext_op, load_ty, p_new, loc);
                        let shift_bits = u32::from(piece.byte_off).saturating_mul(8);
                        let sh = push_data_inst(&mut func, iconst_op, load_ty, shift_bits, loc);
                        let part = push_vv_inst(&mut func, shl_op, load_ty, ext, sh, loc);
                        acc = push_vv_inst(&mut func, or_op, load_ty, acc, part, loc);
                    }
                }
            }
        }
    }

    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    Ok(ForwardResult {
        func: Some(func),
        loads_forwarded: forwarded,
        mem_phis_inserted: graph.mem_phis_inserted,
    })
}

// ============================================================================
// Dead-Store Elimination (Non-Escaping Interval & Dead-Slot DSE + Heap DSE)
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct KilledInterval {
    partition: Partition,
    base: u32,
    base_obj: BaseObj,
    escaped_slot: u32,
    off_known: bool,
    off: i64,
    len: i64,
}

const MAX_KILLED_INTERVALS: usize = 64;

/// Returns `true` when `[lo, lo + len)` on `NonEscapingSlot(slot)` is completely covered by the
/// union of `killed` intervals for `slot`. Bounded by `killed.len()`.
fn slot_interval_fully_covered(killed: &[KilledInterval], slot: u32, lo: i64, len: i64) -> bool {
    let Some(hi) = lo.checked_add(len) else {
        return false;
    };
    if len <= 0 {
        return false;
    }
    let mut cursor = lo;
    let bound = killed.len().saturating_add(1);
    for _ in 0..bound {
        if cursor >= hi {
            return true;
        }
        let mut best_end = cursor;
        for &k in killed {
            if k.partition == Partition::NonEscapingSlot(slot)
                && k.off <= cursor
                && let Some(k_end) = k.off.checked_add(k.len)
                && k_end > best_end
            {
                best_end = k_end;
            }
        }
        if best_end <= cursor {
            return false;
        }
        cursor = best_end;
    }
    cursor >= hi
}

fn clear_killed_partition<A: Allocator>(killed: &mut Vec<KilledInterval, A>, target: Partition) {
    let mut i = 0;
    while i < killed.len() {
        if killed.get(i).is_some_and(|k| k.partition == target) {
            killed.remove_swap(i);
        } else {
            i = i.saturating_add(1);
        }
    }
}

/// Removes or splits killed intervals in `NonEscapingSlot(slot)` that overlap read `[r_lo, r_lo + r_len)`.
fn expose_slot_read<A: Allocator>(
    killed: &mut Vec<KilledInterval, A>,
    slot: u32,
    r_lo: i64,
    r_len: i64,
) {
    let Some(r_hi) = r_lo.checked_add(r_len) else {
        clear_killed_partition(killed, Partition::NonEscapingSlot(slot));
        return;
    };
    let mut extras = [KilledInterval {
        partition: Partition::Heap,
        base: 0,
        base_obj: BaseObj::Unknown,
        escaped_slot: NONE,
        off_known: false,
        off: 0,
        len: 0,
    }; 4];
    let mut extra_n = 0usize;

    let mut i = 0;
    while i < killed.len() {
        let Some(&k) = killed.get(i) else {
            break;
        };
        if k.partition == Partition::NonEscapingSlot(slot)
            && ranges_overlap(k.off, k.len, r_lo, r_len)
        {
            killed.remove_swap(i);
            if let Some(k_hi) = k.off.checked_add(k.len) {
                if k.off < r_lo
                    && let Some(l_len) = r_lo.checked_sub(k.off)
                    && l_len > 0
                    && extra_n < extras.len()
                {
                    set_at(
                        &mut extras,
                        extra_n,
                        KilledInterval { off: k.off, len: l_len, ..k },
                    );
                    extra_n = extra_n.saturating_add(1);
                }
                if r_hi < k_hi
                    && let Some(rt_len) = k_hi.checked_sub(r_hi)
                    && rt_len > 0
                    && extra_n < extras.len()
                {
                    set_at(
                        &mut extras,
                        extra_n,
                        KilledInterval { off: r_hi, len: rt_len, ..k },
                    );
                    extra_n = extra_n.saturating_add(1);
                }
            }
        } else {
            i = i.saturating_add(1);
        }
    }

    for item in extras.get(..extra_n).unwrap_or(&[]) {
        if killed.len() < MAX_KILLED_INTERVALS {
            killed.push(*item);
        }
    }
}

fn eliminate_dead_stores_memssa<A: Allocator + Copy>(
    view: View<'_>,
    alias: &AliasAnalysis<A>,
    alloc: A,
) -> Result<(Option<Func<A>>, u32), SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    let nslots = view.slots.len();

    // Scan all reads of local stack slots (`Load`, `Memcpy`/`Memmove` `src`).
    let mut slot_read = filled(nslots, false, &alloc);
    for inst in 0..len {
        match view.op(inst) {
            Op::Load => {
                let at = to_idx(at_u32(view.data, inst));
                let ptr = at_u32(view.extra, at);
                let s = alias.slot_of_val(ptr);
                if s != NONE {
                    set_at(&mut slot_read, to_idx(s), true);
                }
            }
            Op::Memcpy | Op::Memmove => {
                let at = to_idx(at_u32(view.data, inst));
                let src = at_u32(view.extra, at.saturating_add(1));
                let s = alias.slot_of_val(src);
                if s != NONE {
                    set_at(&mut slot_read, to_idx(s), true);
                }
            }
            _ => {}
        }
    }

    let mut kill = filled(len, false, &alloc);
    let mut killed_count = 0u32;
    let mut killed_ivs: Vec<KilledInterval, A> = Vec::with_capacity_in(16, alloc);
    let mut slot_live_after = filled(nslots, true, &alloc);

    for b in 0..blocks {
        killed_ivs.clear();
        let b32 = to_u32(b);
        let range = view.block(Block(b32));
        let term_op = view.op(view.terminator(Block(b32)));
        let exits_frame = matches!(
            term_op,
            Op::Return | Op::TailCall | Op::TailCallIndirect | Op::Unreachable
        );
        for s in 0..nslots {
            set_at(&mut slot_live_after, s, !exits_frame);
        }

        for inst in (range.start..range.end).rev() {
            let op = view.op(inst);
            match op {
                Op::Store => {
                    let at = to_idx(at_u32(view.data, inst));
                    let val_ty = Ty(at_u16(view.tys, to_idx(at_u32(view.extra, at))));
                    let ptr = at_u32(view.extra, at.saturating_add(1));
                    let off = i64::from(at_u32(view.extra, at.saturating_add(2)).cast_signed());
                    let volatile = Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE);
                    if volatile {
                        let s = alias.slot_of_val(ptr);
                        if s != NONE && alias.is_non_escaping(s) {
                            set_at(&mut slot_live_after, to_idx(s), true);
                            clear_killed_partition(&mut killed_ivs, Partition::NonEscapingSlot(s));
                        } else {
                            clear_killed_partition(&mut killed_ivs, Partition::Heap);
                        }
                        continue;
                    }

                    let ptr_slot = alias.slot_of_val(ptr);
                    if ptr_slot != NONE && alias.is_non_escaping(ptr_slot) {
                        let s_idx = to_idx(ptr_slot);
                        if !at_bool(&slot_read, s_idx) || !at_bool(&slot_live_after, s_idx) {
                            set_at(&mut kill, inst, true);
                            killed_count = killed_count.saturating_add(1);
                            continue;
                        }
                        if let Some(loc) = alias.decompose_loc(&view, ptr, off, val_ty)
                            && loc.off_known
                            && loc.off >= 0
                            && loc.off.checked_add(loc.size).is_some()
                        {
                            if slot_interval_fully_covered(&killed_ivs, ptr_slot, loc.off, loc.size)
                            {
                                set_at(&mut kill, inst, true);
                                killed_count = killed_count.saturating_add(1);
                            } else if killed_ivs.len() < MAX_KILLED_INTERVALS {
                                killed_ivs.push(KilledInterval {
                                    partition: Partition::NonEscapingSlot(ptr_slot),
                                    base: ptr_slot,
                                    base_obj: loc.base_obj,
                                    escaped_slot: NONE,
                                    off_known: true,
                                    off: loc.off,
                                    len: loc.size,
                                });
                            }
                        }
                    } else if let Some(loc) = alias.decompose_loc(&view, ptr, off, val_ty) {
                        let loc_end = loc.off.checked_add(loc.size);
                        let covered = loc_end.is_some_and(|l_end| {
                            killed_ivs.iter().any(|&k| {
                                let same_root = k.base_obj != BaseObj::Unknown
                                    && k.base_obj == loc.base_obj
                                    && k.off_known
                                    && loc.off_known;
                                let same_ssa = k.base != NONE && k.base == loc.base;
                                k.partition == Partition::Heap
                                    && k.off <= loc.off
                                    && k.off.checked_add(k.len).is_some_and(|k_end| k_end >= l_end)
                                    && (same_root || same_ssa)
                            })
                        });
                        if covered {
                            set_at(&mut kill, inst, true);
                            killed_count = killed_count.saturating_add(1);
                        } else if killed_ivs.len() < MAX_KILLED_INTERVALS
                            && (loc.base != NONE || loc.off_known)
                        {
                            killed_ivs.push(KilledInterval {
                                partition: Partition::Heap,
                                base: loc.base,
                                base_obj: loc.base_obj,
                                escaped_slot: loc.slot,
                                off_known: loc.off_known,
                                off: loc.off,
                                len: loc.size,
                            });
                        }
                    } else {
                        clear_killed_partition(&mut killed_ivs, Partition::Heap);
                    }
                }
                Op::Memset => {
                    let at = to_idx(at_u32(view.data, inst));
                    let dst = at_u32(view.extra, at);
                    let len_val = at_u32(view.extra, at.saturating_add(2));
                    let volatile = Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE);
                    let dst_base_obj = alias.base_of(dst);
                    let (_, dst_off, dst_slot, dst_off_known) =
                        alias.resolve_ptr_const(&view, dst, 0);
                    let const_len = const_u64(&view, to_idx(len_val))
                        .and_then(|n| i64::try_from(n).ok())
                        .filter(|&n| n > 0);

                    if volatile {
                        if dst_slot != NONE && alias.is_non_escaping(dst_slot) {
                            set_at(&mut slot_live_after, to_idx(dst_slot), true);
                            clear_killed_partition(
                                &mut killed_ivs,
                                Partition::NonEscapingSlot(dst_slot),
                            );
                        } else {
                            clear_killed_partition(&mut killed_ivs, Partition::Heap);
                        }
                        continue;
                    }

                    if dst_slot != NONE && alias.is_non_escaping(dst_slot) {
                        let s_idx = to_idx(dst_slot);
                        if let Some(mlen) = const_len
                            && dst_off_known
                            && dst_off >= 0
                            && let Ok(mlen_u64) = u64::try_from(mlen)
                            && dst_off.checked_add(mlen).is_some()
                            && slot_range_in_bounds(&view, dst_slot, dst_off, mlen_u64)
                        {
                            if !at_bool(&slot_read, s_idx)
                                || !at_bool(&slot_live_after, s_idx)
                                || slot_interval_fully_covered(&killed_ivs, dst_slot, dst_off, mlen)
                            {
                                set_at(&mut kill, inst, true);
                                killed_count = killed_count.saturating_add(1);
                            } else if killed_ivs.len() < MAX_KILLED_INTERVALS {
                                killed_ivs.push(KilledInterval {
                                    partition: Partition::NonEscapingSlot(dst_slot),
                                    base: dst_slot,
                                    base_obj: dst_base_obj,
                                    escaped_slot: NONE,
                                    off_known: true,
                                    off: dst_off,
                                    len: mlen,
                                });
                            }
                        }
                    } else {
                        clear_killed_partition(&mut killed_ivs, Partition::Heap);
                    }
                }
                Op::Load => {
                    let at = to_idx(at_u32(view.data, inst));
                    let ptr = at_u32(view.extra, at);
                    let off = i64::from(at_u32(view.extra, at.saturating_add(1)).cast_signed());
                    let ptr_slot = alias.slot_of_val(ptr);
                    if ptr_slot != NONE {
                        set_at(&mut slot_live_after, to_idx(ptr_slot), true);
                    }
                    if ptr_slot != NONE && alias.is_non_escaping(ptr_slot) {
                        if let Some(loc) =
                            alias.decompose_loc(&view, ptr, off, Ty(at_u16(view.tys, inst)))
                            && loc.off_known
                            && loc.off.checked_add(loc.size).is_some()
                        {
                            expose_slot_read(&mut killed_ivs, ptr_slot, loc.off, loc.size);
                        } else {
                            clear_killed_partition(
                                &mut killed_ivs,
                                Partition::NonEscapingSlot(ptr_slot),
                            );
                        }
                    } else if let Some(loc) =
                        alias.decompose_loc(&view, ptr, off, Ty(at_u16(view.tys, inst)))
                    {
                        // Heap load ONLY removes potentially-aliasing Heap killed intervals,
                        // NEVER touching proven non-escaping stack slots!
                        let mut i = 0;
                        while i < killed_ivs.len() {
                            if let Some(&k) = killed_ivs.get(i)
                                && k.partition == Partition::Heap
                            {
                                let k_loc = MemLoc {
                                    base: k.base,
                                    off: k.off,
                                    size: k.len,
                                    slot: k.escaped_slot,
                                    off_known: k.off_known,
                                    base_obj: k.base_obj,
                                };
                                if !alias.no_alias(loc, k_loc) {
                                    killed_ivs.remove_swap(i);
                                    continue;
                                }
                            }
                            i = i.saturating_add(1);
                        }
                    } else {
                        clear_killed_partition(&mut killed_ivs, Partition::Heap);
                    }
                }
                Op::Memcpy | Op::Memmove => {
                    let at = to_idx(at_u32(view.data, inst));
                    let dst = at_u32(view.extra, at);
                    let src = at_u32(view.extra, at.saturating_add(1));
                    let len_val = at_u32(view.extra, at.saturating_add(2));
                    let volatile = Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE);
                    let dst_base_obj = alias.base_of(dst);
                    let src_base_obj = alias.base_of(src);
                    let (_, dst_off, dst_slot, dst_off_known) =
                        alias.resolve_ptr_const(&view, dst, 0);
                    let (src_base, src_off, src_slot, src_off_known) =
                        alias.resolve_ptr_const(&view, src, 0);
                    let const_len = const_u64(&view, to_idx(len_val))
                        .and_then(|n| i64::try_from(n).ok())
                        .filter(|&n| n > 0);

                    // In backward order: first record write to `dst`, then read of `src`.
                    if !volatile
                        && dst_slot != NONE
                        && alias.is_non_escaping(dst_slot)
                        && dst_off_known
                        && dst_off >= 0
                        && let Some(clen) = const_len
                        && let Ok(clen_u64) = u64::try_from(clen)
                        && dst_off.checked_add(clen).is_some()
                        && slot_range_in_bounds(&view, dst_slot, dst_off, clen_u64)
                        && killed_ivs.len() < MAX_KILLED_INTERVALS
                    {
                        killed_ivs.push(KilledInterval {
                            partition: Partition::NonEscapingSlot(dst_slot),
                            base: dst_slot,
                            base_obj: dst_base_obj,
                            escaped_slot: NONE,
                            off_known: true,
                            off: dst_off,
                            len: clen,
                        });
                    }

                    if src_slot != NONE && alias.is_non_escaping(src_slot) {
                        set_at(&mut slot_live_after, to_idx(src_slot), true);
                        if src_off_known
                            && let Some(clen) = const_len
                            && src_off.checked_add(clen).is_some()
                        {
                            expose_slot_read(&mut killed_ivs, src_slot, src_off, clen);
                        } else {
                            clear_killed_partition(
                                &mut killed_ivs,
                                Partition::NonEscapingSlot(src_slot),
                            );
                        }
                    } else {
                        if src_slot != NONE {
                            set_at(&mut slot_live_after, to_idx(src_slot), true);
                        }
                        if volatile {
                            clear_killed_partition(&mut killed_ivs, Partition::Heap);
                        } else if let Some(clen) = const_len
                            && src_off.checked_add(clen).is_some()
                        {
                            let src_loc = MemLoc {
                                base: src_base,
                                off: src_off,
                                size: clen,
                                slot: src_slot,
                                off_known: src_off_known,
                                base_obj: src_base_obj,
                            };
                            let mut i = 0;
                            while i < killed_ivs.len() {
                                if let Some(&k) = killed_ivs.get(i)
                                    && k.partition == Partition::Heap
                                {
                                    let k_loc = MemLoc {
                                        base: k.base,
                                        off: k.off,
                                        size: k.len,
                                        slot: k.escaped_slot,
                                        off_known: k.off_known,
                                        base_obj: k.base_obj,
                                    };
                                    if !alias.no_alias(src_loc, k_loc) {
                                        killed_ivs.remove_swap(i);
                                        continue;
                                    }
                                }
                                i = i.saturating_add(1);
                            }
                        } else {
                            let mut i = 0;
                            while i < killed_ivs.len() {
                                if let Some(&k) = killed_ivs.get(i)
                                    && k.partition == Partition::Heap
                                    && alias.may_alias_bases(src_base_obj, k.base_obj)
                                {
                                    killed_ivs.remove_swap(i);
                                    continue;
                                }
                                i = i.saturating_add(1);
                            }
                        }
                    }
                }
                Op::SlotLive | Op::SlotDead => {
                    let s = at_u32(view.data, inst);
                    if alias.is_non_escaping(s) {
                        set_at(&mut slot_live_after, to_idx(s), false);
                        clear_killed_partition(&mut killed_ivs, Partition::NonEscapingSlot(s));
                    } else {
                        clear_killed_partition(&mut killed_ivs, Partition::Heap);
                    }
                }
                Op::Suspend => {
                    killed_ivs.clear();
                    for live in slot_live_after.iter_mut() {
                        *live = true;
                    }
                }
                Op::Call
                | Op::CallIndirect
                | Op::Invoke
                | Op::InvokeIndirect
                | Op::TailCall
                | Op::TailCallIndirect
                | Op::Memcmp
                | Op::MaskedLoad
                | Op::Gather
                | Op::MaskedStore
                | Op::Scatter
                | Op::AtomicLoad
                | Op::AtomicStore
                | Op::AtomicRmw
                | Op::Cmpxchg
                | Op::Fence
                | Op::Asm
                | Op::AsmBr
                | Op::TargetIntrinsic
                | Op::Resume
                | Op::Trap
                | Op::Unreachable
                | Op::Cancelled => {
                    // Non-escaping local stack slots survive calls/invokes/atomics/fences!
                    clear_killed_partition(&mut killed_ivs, Partition::Heap);
                }
                _ => {}
            }
        }
    }

    if killed_count == 0 {
        return Ok((None, 0));
    }

    let mut val_map = filled(len, NONE, &alloc);
    let mut next_idx: usize = 0;
    for inst in 0..len {
        if !at_bool(&kill, inst) {
            set_at(&mut val_map, inst, to_u32(next_idx));
            next_idx = next_idx.saturating_add(1);
        }
    }

    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    for b in 0..blocks {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        for inst in view.block(Block(b32)) {
            if at_bool(&kill, inst) {
                continue;
            }
            let _ = copy_inst_same_blocks(&view, inst, &val_map, &mut func);
        }
    }
    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    Ok((Some(func), killed_count))
}

// ============================================================================
// Alias-Aware Memory LICM (using Canonical AliasAnalysis + CfgInfo)
// ============================================================================

/// Returns `true` when `header` unconditionally reaches `target_inst` inside `target_bb` along a
/// chain of unconditional `Op::Jump` terminators with no earlier potentially-trapping or
/// potentially-diverging instruction.
fn header_unconditionally_reaches(
    view: &View<'_>,
    header: u32,
    target_bb: u32,
    target_inst: usize,
    blocks: usize,
) -> bool {
    let mut cur = header;
    for _ in 0..=blocks {
        let range = view.block(Block(cur));
        let stop = if cur == target_bb { target_inst } else { range.end };
        for i in range.start..stop {
            if op_may_trap_or_diverge(view.op(i)) {
                return false;
            }
        }
        if cur == target_bb {
            return true;
        }
        let term = view.terminator(Block(cur));
        if view.op(term) != Op::Jump {
            return false;
        }
        let at = to_idx(at_u32(view.data, term));
        let next = at_u32(view.extra, at);
        if next == cur || next == header {
            return false;
        }
        cur = next;
    }
    false
}

/// Returns `true` when `preheader` already contains a non-volatile `Op::Load` or `Op::Store`
/// covering `[target.off, target.off + target.size)` on the same canonical root or SSA base.
fn preheader_already_derefs<A: Allocator + Copy>(
    view: &View<'_>,
    alias: &AliasAnalysis<A>,
    preheader: u32,
    target: MemLoc,
) -> bool {
    let Some(t_end) = target.off.checked_add(target.size) else {
        return false;
    };
    for inst in view.block(Block(preheader)) {
        if Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE) {
            continue;
        }
        let loc = match view.op(inst) {
            Op::Load => {
                let at = to_idx(at_u32(view.data, inst));
                let ptr = at_u32(view.extra, at);
                let off = i64::from(at_u32(view.extra, at.saturating_add(1)).cast_signed());
                alias.decompose_loc(view, ptr, off, Ty(at_u16(view.tys, inst)))
            }
            Op::Store => {
                let at = to_idx(at_u32(view.data, inst));
                let val = at_u32(view.extra, at);
                let ptr = at_u32(view.extra, at.saturating_add(1));
                let off = i64::from(at_u32(view.extra, at.saturating_add(2)).cast_signed());
                alias.decompose_loc(view, ptr, off, Ty(at_u16(view.tys, to_idx(val))))
            }
            _ => None,
        };
        let Some(p_loc) = loc else {
            continue;
        };
        let same_root = p_loc.base_obj != BaseObj::Unknown
            && p_loc.base_obj == target.base_obj
            && p_loc.off_known
            && target.off_known;
        let same_ssa = p_loc.base != NONE && p_loc.base == target.base;
        if (same_root || same_ssa)
            && p_loc.off <= target.off
            && p_loc.off.checked_add(p_loc.size).is_some_and(|p_end| p_end >= t_end)
        {
            return true;
        }
    }
    false
}

/// Collects up to 4 pure loop-internal address-producing instructions (`DataAddr`, `SlotAddr`,
/// `Iconst`, `IconstWide`, non-speculative `PtrAdd`) required for `root_val` to be available in
/// `preheader`. Returns `None` if `root_val` depends on any loop-variant value.
fn collect_invariant_addr_deps<A: Allocator + Copy>(
    view: &View<'_>,
    cfg: &CfgInfo<A>,
    in_loop: &[bool],
    hoist_to: &[u32],
    preheader: u32,
    header: u32,
    root_val: u32,
    deps: &mut [usize; 4],
) -> Option<usize> {
    let mut work = [NONE; 4];
    let mut w_len = 0usize;
    let mut d_len = 0usize;

    let push_if_needed = |v: u32,
                          work: &mut [u32; 4],
                          w_len: &mut usize,
                          deps: &mut [usize; 4],
                          d_len: &mut usize|
     -> bool {
        let vi = to_idx(v);
        let vb = at_u32(&cfg.inst_block, vi);
        if !at_bool(in_loop, to_idx(vb)) {
            return cfg.dominates(vb, preheader) && at_u32(hoist_to, vi) == NONE;
        }
        let existing_hoist = at_u32(hoist_to, vi);
        if existing_hoist == preheader {
            return true;
        }
        if existing_hoist != NONE {
            return false;
        }
        if deps.get(..*d_len).unwrap_or(&[]).iter().any(|&x| x == vi) {
            return true;
        }
        if *d_len >= 4 || *w_len >= 4 {
            return false;
        }
        set_at(deps, *d_len, vi);
        *d_len = d_len.saturating_add(1);
        set_at(work, *w_len, v);
        *w_len = w_len.saturating_add(1);
        true
    };

    if !push_if_needed(root_val, &mut work, &mut w_len, deps, &mut d_len) {
        return None;
    }

    for _ in 0..8 {
        if w_len == 0 {
            break;
        }
        w_len = w_len.saturating_sub(1);
        let cur = at_u32(&work, w_len);
        let ci = to_idx(cur);
        match view.op(ci) {
            Op::DataAddr | Op::SlotAddr | Op::Iconst | Op::IconstWide => {}
            Op::PtrAdd => {
                if Flags(at_u8(view.flags, ci)).contains(Flags::INBOUNDS)
                    && at_u32(&cfg.inst_block, ci) != header
                {
                    return None;
                }
                let at = to_idx(at_u32(view.data, ci));
                let base = at_u32(view.extra, at);
                let by = at_u32(view.extra, at.saturating_add(1));
                if !push_if_needed(base, &mut work, &mut w_len, deps, &mut d_len)
                    || !push_if_needed(by, &mut work, &mut w_len, deps, &mut d_len)
                {
                    return None;
                }
            }
            _ => return None,
        }
    }

    Some(d_len)
}

fn hoist_invariant_loads_ssa<A: Allocator + Copy>(
    view: View<'_>,
    alias: &AliasAnalysis<A>,
    cfg: &CfgInfo<A>,
    alloc: A,
) -> Result<(Option<Func<A>>, u32), SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 2 {
        return Ok((None, 0));
    }

    let mut hoist_to = filled(len, NONE, &alloc);
    // `(preheader_block, rpo_idx[orig_block], orig_inst)` preserves strict SSA dominance order.
    let mut hoisted_insts: Vec<(u32, u32, usize), A> = Vec::new_in(alloc);
    let mut total_hoisted = 0u32;

    let mut in_loop = filled(blocks, false, &alloc);
    let mut body: Vec<u32, A> = Vec::new_in(alloc);
    let mut work: Vec<u32, A> = Vec::new_in(alloc);
    let mut store_locs: Vec<MemLoc, A> = Vec::new_in(alloc);
    let mut bulk_dst_bases: Vec<BaseObj, A> = Vec::new_in(alloc);
    let mut clobbered_slots = filled(view.slots.len(), false, &alloc);

    for h in 1..blocks {
        if !at_bool(&cfg.is_loop_header, h) || at_bool(&cfg.is_irreducible_header, h) {
            continue;
        }
        let header = to_u32(h);
        let (outside_count, preheader) =
            cfg.collect_natural_loop(header, &mut in_loop, &mut body, &mut work);
        if outside_count != 1 || preheader == NONE {
            continue;
        }
        if view.op(view.terminator(Block(preheader))) != Op::Jump {
            continue;
        }
        for &bb in &body {
            set_at(&mut in_loop, to_idx(bb), true);
        }

        // Collect all memory clobbers inside the loop body, classified via `BaseObj` / `MemLoc`.
        let mut has_opaque_clobber = false;
        let mut has_suspend = false;
        store_locs.clear();
        bulk_dst_bases.clear();
        for c in clobbered_slots.iter_mut() {
            *c = false;
        }
        for &bb in &body {
            for inst in view.block(Block(bb)) {
                let op = view.op(inst);
                match op {
                    Op::Store => {
                        let at = to_idx(at_u32(view.data, inst));
                        let val_ty = Ty(at_u16(view.tys, to_idx(at_u32(view.extra, at))));
                        let ptr = at_u32(view.extra, at.saturating_add(1));
                        let off =
                            i64::from(at_u32(view.extra, at.saturating_add(2)).cast_signed());
                        let volatile = Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE);
                        if volatile {
                            let s = alias.slot_of_val(ptr);
                            if s != NONE && alias.is_non_escaping(s) {
                                set_at(&mut clobbered_slots, to_idx(s), true);
                            } else {
                                has_opaque_clobber = true;
                            }
                        } else if let Some(loc) = alias.decompose_loc(&view, ptr, off, val_ty)
                            && loc.off.checked_add(loc.size).is_some()
                        {
                            store_locs.push(loc);
                        } else {
                            let s = alias.slot_of_val(ptr);
                            if s != NONE && alias.is_non_escaping(s) {
                                set_at(&mut clobbered_slots, to_idx(s), true);
                            } else {
                                bulk_dst_bases.push(alias.base_of(ptr));
                            }
                        }
                    }
                    Op::Memcpy | Op::Memmove | Op::Memset => {
                        let at = to_idx(at_u32(view.data, inst));
                        let dst = at_u32(view.extra, at);
                        let len_val = at_u32(view.extra, at.saturating_add(2));
                        let volatile = Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE);
                        let dst_base_obj = alias.base_of(dst);
                        let (dst_base, dst_off, dst_slot, dst_off_known) =
                            alias.resolve_ptr_const(&view, dst, 0);
                        let const_len = const_u64(&view, to_idx(len_val))
                            .and_then(|n| i64::try_from(n).ok())
                            .filter(|&n| n > 0);
                        if volatile {
                            if dst_slot != NONE && alias.is_non_escaping(dst_slot) {
                                set_at(&mut clobbered_slots, to_idx(dst_slot), true);
                            } else {
                                has_opaque_clobber = true;
                            }
                        } else if let Some(clen) = const_len
                            && dst_off.checked_add(clen).is_some()
                        {
                            store_locs.push(MemLoc {
                                base: dst_base,
                                off: dst_off,
                                size: clen,
                                slot: dst_slot,
                                off_known: dst_off_known,
                                base_obj: dst_base_obj,
                            });
                        } else if dst_slot != NONE && alias.is_non_escaping(dst_slot) {
                            set_at(&mut clobbered_slots, to_idx(dst_slot), true);
                        } else {
                            bulk_dst_bases.push(dst_base_obj);
                        }
                    }
                    Op::SlotLive | Op::SlotDead => {
                        let s = at_u32(view.data, inst);
                        set_at(&mut clobbered_slots, to_idx(s), true);
                        if !alias.is_non_escaping(s) {
                            bulk_dst_bases.push(BaseObj::Slot(SlotId(s)));
                        }
                    }
                    Op::Suspend => {
                        has_suspend = true;
                        has_opaque_clobber = true;
                    }
                    Op::MaskedStore
                    | Op::Scatter
                    | Op::Call
                    | Op::CallIndirect
                    | Op::Invoke
                    | Op::InvokeIndirect
                    | Op::TailCall
                    | Op::TailCallIndirect
                    | Op::AtomicStore
                    | Op::AtomicRmw
                    | Op::Cmpxchg
                    | Op::Fence
                    | Op::Asm
                    | Op::AsmBr
                    | Op::TargetIntrinsic => {
                        has_opaque_clobber = true;
                    }
                    _ => {}
                }
            }
        }

        body.sort_unstable_by_key(|&bb| (at_u32(&cfg.rpo_idx, to_idx(bb)), bb));
        for &bb in &body {
            for inst in view.block(Block(bb)) {
                if at_u32(&hoist_to, inst) != NONE || view.op(inst) != Op::Load {
                    continue;
                }
                if Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE) {
                    continue;
                }
                let at = to_idx(at_u32(view.data, inst));
                let ptr = at_u32(view.extra, at);
                let off = i64::from(at_u32(view.extra, at.saturating_add(1)).cast_signed());
                let mut addr_deps = [0usize; 4];
                let Some(dep_count) = collect_invariant_addr_deps(
                    &view,
                    cfg,
                    &in_loop,
                    &hoist_to,
                    preheader,
                    header,
                    ptr,
                    &mut addr_deps,
                ) else {
                    continue;
                };
                let Some(loc) =
                    alias.decompose_loc(&view, ptr, off, Ty(at_u16(view.tys, inst)))
                else {
                    continue;
                };

                // Crash safety: safe to hoist into `preheader` when guaranteed to execute on
                // every loop entry (unconditional chain from `header` before any trap/call/div)
                // OR proven non-trapping (in-bounds `Slot`, in-bounds immutable `DataAddr`,
                // non-null `dereferenceable(n)` `Param`, or already dereferenced in `preheader`).
                let size_u64 = u64::try_from(loc.size).ok();
                let speculatively_safe = match loc.base_obj {
                    BaseObj::Slot(slot_id) => {
                        loc.off_known
                            && size_u64.is_some_and(|sz| {
                                slot_range_in_bounds(&view, slot_id.0, loc.off, sz)
                            })
                    }
                    BaseObj::DataAddr(ref_id) => {
                        loc.off_known
                            && size_u64.is_some_and(|sz| {
                                data_range_in_bounds(&view, ref_id, loc.off, sz)
                            })
                    }
                    BaseObj::Param { index, .. } => {
                        loc.off_known
                            && size_u64.is_some_and(|sz| {
                                param_range_deref(&view, index, loc.off, sz)
                            })
                    }
                    BaseObj::Unknown => false,
                };
                let guaranteed_deref = speculatively_safe
                    || header_unconditionally_reaches(&view, header, bb, inst, blocks)
                    || preheader_already_derefs(&view, alias, preheader, loc);
                if !guaranteed_deref {
                    continue;
                }

                // Alias / invariance check via `BaseObj` disjointness proofs:
                // - Immutable `BaseObj::DataAddr` is NEVER clobbered by any store, bulk op, or call!
                // - Non-escaping `Slot` survives opaque calls/atomics/fences and disjoint stores.
                // - `NOALIAS` `Param` survives stores/bulk ops to any distinct `Param` or `Slot`.
                if !loc.base_obj.is_immutable_data() {
                    if has_suspend {
                        continue;
                    }
                    if loc.slot != NONE && at_bool(&clobbered_slots, to_idx(loc.slot)) {
                        continue;
                    }
                    let is_invariant_attr = view
                        .mem_attrs(inst)
                        .is_some_and(|m| (m.bits & MemAttrsRec::INVARIANT) != 0);
                    if !is_invariant_attr {
                        if has_opaque_clobber
                            && !(loc.slot != NONE && alias.is_non_escaping(loc.slot))
                        {
                            continue;
                        }
                        if store_locs.iter().any(|&st| !alias.no_alias(loc, st)) {
                            continue;
                        }
                        if bulk_dst_bases
                            .iter()
                            .any(|&b_obj| alias.may_alias_bases(loc.base_obj, b_obj))
                        {
                            continue;
                        }
                    }
                }

                for &dep in addr_deps.get(..dep_count).unwrap_or(&[]) {
                    if at_u32(&hoist_to, dep) == NONE {
                        set_at(&mut hoist_to, dep, preheader);
                        let db = at_u32(&cfg.inst_block, dep);
                        let drpo = at_u32(&cfg.rpo_idx, to_idx(db));
                        hoisted_insts.push((preheader, drpo, dep));
                    }
                }
                set_at(&mut hoist_to, inst, preheader);
                let irpo = at_u32(&cfg.rpo_idx, to_idx(bb));
                hoisted_insts.push((preheader, irpo, inst));
                total_hoisted = total_hoisted.saturating_add(1);
            }
        }

        for &bb in &body {
            set_at(&mut in_loop, to_idx(bb), false);
        }
    }

    if total_hoisted == 0 {
        return Ok((None, 0));
    }

    // Group hoisted instructions by preheader in strict dominance `(rpo_idx, inst)` order.
    hoisted_insts.sort_unstable();

    // Pass 1: precompute exact old->new instruction index map (layout-order independent).
    let mut val_map = filled(len, NONE, &alloc);
    let mut next_idx: usize = 0;
    for b in 0..blocks {
        let b32 = to_u32(b);
        let range = view.block(Block(b32));
        let term = range.end.saturating_sub(1);
        for inst in range.start..term {
            if at_u32(&hoist_to, inst) != NONE {
                continue;
            }
            set_at(&mut val_map, inst, to_u32(next_idx));
            next_idx = next_idx.saturating_add(1);
        }
        let h_start = hoisted_insts.partition_point(|&(pre, _, _)| pre < b32);
        let h_rest = hoisted_insts.get(h_start..).unwrap_or(&[]);
        let h_len = h_rest.partition_point(|&(pre, _, _)| pre == b32);
        for &(_, _, hi) in h_rest.get(..h_len).unwrap_or(&[]) {
            set_at(&mut val_map, hi, to_u32(next_idx));
            next_idx = next_idx.saturating_add(1);
        }
        set_at(&mut val_map, term, to_u32(next_idx));
        next_idx = next_idx.saturating_add(1);
    }

    // Pass 2: rebuild instructions with hoisted loads placed right before each preheader's `Jump`.
    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    for b in 0..blocks {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        let range = view.block(Block(b32));
        let term = range.end.saturating_sub(1);
        for inst in range.start..term {
            if at_u32(&hoist_to, inst) != NONE {
                continue;
            }
            let _ = copy_inst_same_blocks(&view, inst, &val_map, &mut func);
        }
        let h_start = hoisted_insts.partition_point(|&(pre, _, _)| pre < b32);
        let h_rest = hoisted_insts.get(h_start..).unwrap_or(&[]);
        let h_len = h_rest.partition_point(|&(pre, _, _)| pre == b32);
        for &(_, _, hi) in h_rest.get(..h_len).unwrap_or(&[]) {
            let _ = copy_inst_same_blocks(&view, hi, &val_map, &mut func);
        }
        let _ = copy_inst_same_blocks(&view, term, &val_map, &mut func);
    }

    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    Ok((Some(func), total_hoisted))
}

// ============================================================================
// Dead-Temp Bulk-Copy Elision & Dead Stack-Slot Compaction (One Home, Rule 30)
// ============================================================================

const MAX_TEMP_COPY_ELISION_PASSES: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TempWriterKind {
    None,
    Memset { inst: u32, byte_val: u32, len: u64 },
    Copy { inst: u32, src: u32, len: u64, src_align: u8 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct TempReader {
    slot: u32,
    inst: u32,
    dst: u32,
    len_val: u32,
    len: u64,
    dst_align: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TempCopyRewrite {
    Keep,
    Delete,
    ToMemset { dst: u32, byte_val: u32, len_val: u32, dst_align: u8 },
    ToCopy { op: Op, dst: u32, src: u32, len_val: u32, dst_align: u8, src_align: u8 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PtrOrigin {
    Slot { slot: u32, off: Option<i64> },
    EntryParam,
    Data,
    Unknown,
}

fn resolve_ptr_origin(view: &View<'_>, block_of: &[u32], mut ptr: u32) -> PtrOrigin {
    let mut off: Option<i64> = Some(0);
    let mut all_inbounds = true;
    let mut hopped = false;
    for _ in 0..8 {
        let pi = to_idx(ptr);
        if view.op(pi) == Op::PtrAdd {
            if !Flags(at_u8(view.flags, pi)).contains(Flags::INBOUNDS) {
                all_inbounds = false;
            }
            let at = to_idx(at_u32(view.data, pi));
            let base = at_u32(view.extra, at);
            let by = at_u32(view.extra, at.saturating_add(1));
            off = off
                .and_then(|cur| const_i64(view, to_idx(by)).and_then(|d| cur.checked_add(d)));
            ptr = base;
            hopped = true;
            continue;
        }
        break;
    }
    let pi = to_idx(ptr);
    match view.op(pi) {
        Op::SlotAddr if off.is_some() || all_inbounds => {
            PtrOrigin::Slot { slot: at_u32(view.data, pi), off }
        }
        Op::Param if at_u32(block_of, pi) == 0 && (!hopped || all_inbounds) => {
            PtrOrigin::EntryParam
        }
        Op::DataAddr if !hopped || all_inbounds => PtrOrigin::Data,
        _ => PtrOrigin::Unknown,
    }
}

fn ptrs_provably_disjoint(
    view: &View<'_>,
    block_of: &[u32],
    a: u32,
    b: u32,
    len: u64,
) -> bool {
    let oa = resolve_ptr_origin(view, block_of, a);
    let ob = resolve_ptr_origin(view, block_of, b);
    match (oa, ob) {
        (PtrOrigin::Slot { slot: sa, off: oa_off }, PtrOrigin::Slot { slot: sb, off: ob_off }) => {
            if sa != sb {
                let a_ok = oa_off.is_none_or(|o| slot_range_in_bounds(view, sa, o, len));
                let b_ok = ob_off.is_none_or(|o| slot_range_in_bounds(view, sb, o, len));
                a_ok && b_ok
            } else if let (Some(off_a), Some(off_b)) = (oa_off, ob_off)
                && slot_range_in_bounds(view, sa, off_a, len)
                && slot_range_in_bounds(view, sb, off_b, len)
                && let Ok(len_i64) = i64::try_from(len)
                && let (Some(end_a), Some(end_b)) =
                    (off_a.checked_add(len_i64), off_b.checked_add(len_i64))
            {
                end_a <= off_b || end_b <= off_a
            } else {
                false
            }
        }
        (PtrOrigin::Slot { slot, off }, PtrOrigin::EntryParam | PtrOrigin::Data)
        | (PtrOrigin::EntryParam | PtrOrigin::Data, PtrOrigin::Slot { slot, off }) => {
            off.is_none_or(|o| slot_range_in_bounds(view, slot, o, len))
        }
        _ => false,
    }
}

fn blocks_temp_copy_elision(
    view: &View<'_>,
    inst: usize,
    temp_slot: u32,
    src_origin: PtrOrigin,
) -> bool {
    match view.op(inst) {
        Op::Store
        | Op::Memcpy
        | Op::Memmove
        | Op::Memset
        | Op::MaskedStore
        | Op::Scatter
        | Op::Call
        | Op::CallIndirect
        | Op::Invoke
        | Op::InvokeIndirect
        | Op::TailCall
        | Op::TailCallIndirect
        | Op::AtomicLoad
        | Op::AtomicStore
        | Op::AtomicRmw
        | Op::Cmpxchg
        | Op::Fence
        | Op::Asm
        | Op::AsmBr
        | Op::TargetIntrinsic
        | Op::Suspend => true,
        Op::Load => Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE),
        Op::SlotLive | Op::SlotDead => {
            let k_slot = at_u32(view.data, inst);
            if k_slot == temp_slot {
                true
            } else {
                match src_origin {
                    PtrOrigin::Slot { slot: s_slot, .. } => k_slot == s_slot,
                    PtrOrigin::EntryParam | PtrOrigin::Data => false,
                    PtrOrigin::Unknown => true,
                }
            }
        }
        _ => false,
    }
}

fn eliminate_temp_bulk_copies_round<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<Option<(Func<A>, u32)>, SimplifyError> {
    let nslots = view.slots.len();
    let len = view.len();
    if nslots == 0 || len == 0 {
        return Ok(None);
    }

    let mut has_bulk_copy = false;
    for inst in 0..len {
        if matches!(view.op(inst), Op::Memcpy | Op::Memmove)
            && Flags(at_u8(view.flags, inst)).0 == 0
        {
            has_bulk_copy = true;
            break;
        }
    }
    if !has_bulk_copy {
        return Ok(None);
    }

    let mut slot_addr_of = filled(len, NONE, &alloc);
    for inst in 0..len {
        if view.op(inst) == Op::SlotAddr {
            set_at(&mut slot_addr_of, inst, at_u32(view.data, inst));
        }
    }

    let blocks = view.blocks();
    let mut block_of = filled(len, 0u32, &alloc);
    for b in 0..blocks {
        let b32 = to_u32(b);
        for inst in view.block(Block(b32)) {
            set_at(&mut block_of, inst, b32);
        }
    }

    let mut ineligible = filled(nslots, false, &alloc);
    let mut writers = filled(nslots, TempWriterKind::None, &alloc);
    let mut readers: Vec<TempReader, A> = Vec::new_in(alloc);

    for inst in 0..len {
        let op = view.op(inst);
        match op {
            Op::Memset => {
                let at = to_idx(at_u32(view.data, inst));
                let dst = at_u32(view.extra, at);
                let byte_val = at_u32(view.extra, at.saturating_add(1));
                let len_val = at_u32(view.extra, at.saturating_add(2));

                let b_slot = at_u32(&slot_addr_of, to_idx(byte_val));
                if b_slot != NONE {
                    set_at(&mut ineligible, to_idx(b_slot), true);
                }
                let l_slot = at_u32(&slot_addr_of, to_idx(len_val));
                if l_slot != NONE {
                    set_at(&mut ineligible, to_idx(l_slot), true);
                }

                let dst_slot = at_u32(&slot_addr_of, to_idx(dst));
                if dst_slot != NONE {
                    let s_idx = to_idx(dst_slot);
                    let const_len = if Flags(at_u8(view.flags, inst)).0 == 0 {
                        const_u64(&view, to_idx(len_val))
                            .filter(|&n| n > 0 && slot_range_in_bounds(&view, dst_slot, 0, n))
                    } else {
                        None
                    };
                    if let Some(n) = const_len
                        && writers.get(s_idx).copied() == Some(TempWriterKind::None)
                    {
                        set_at(
                            &mut writers,
                            s_idx,
                            TempWriterKind::Memset { inst: to_u32(inst), byte_val, len: n },
                        );
                    } else {
                        set_at(&mut ineligible, s_idx, true);
                    }
                }
            }
            Op::Memcpy | Op::Memmove => {
                let at = to_idx(at_u32(view.data, inst));
                let dst = at_u32(view.extra, at);
                let src = at_u32(view.extra, at.saturating_add(1));
                let len_val = at_u32(view.extra, at.saturating_add(2));

                let l_slot = at_u32(&slot_addr_of, to_idx(len_val));
                if l_slot != NONE {
                    set_at(&mut ineligible, to_idx(l_slot), true);
                }

                let dst_slot = at_u32(&slot_addr_of, to_idx(dst));
                let src_slot = at_u32(&slot_addr_of, to_idx(src));
                if dst_slot != NONE && dst_slot == src_slot {
                    set_at(&mut ineligible, to_idx(dst_slot), true);
                    continue;
                }

                let const_len = if Flags(at_u8(view.flags, inst)).0 == 0 {
                    const_u64(&view, to_idx(len_val)).filter(|&n| n > 0)
                } else {
                    None
                };
                let (dst_align, src_align) = match view.mem_attrs(inst) {
                    Some(m) => (m.align, m.src_align),
                    None => (0, 0),
                };

                if dst_slot != NONE {
                    let s_idx = to_idx(dst_slot);
                    if let Some(n) = const_len
                        && slot_range_in_bounds(&view, dst_slot, 0, n)
                        && writers.get(s_idx).copied() == Some(TempWriterKind::None)
                    {
                        set_at(
                            &mut writers,
                            s_idx,
                            TempWriterKind::Copy { inst: to_u32(inst), src, len: n, src_align },
                        );
                    } else {
                        set_at(&mut ineligible, s_idx, true);
                    }
                }

                if src_slot != NONE {
                    let s_idx = to_idx(src_slot);
                    if let Some(n) = const_len
                        && slot_range_in_bounds(&view, src_slot, 0, n)
                    {
                        readers.push(TempReader {
                            slot: src_slot,
                            inst: to_u32(inst),
                            dst,
                            len_val,
                            len: n,
                            dst_align,
                        });
                    } else {
                        set_at(&mut ineligible, s_idx, true);
                    }
                }
            }
            Op::SlotAddr | Op::SlotLive | Op::SlotDead => {}
            _ => {
                each(&view, inst, |item| match item {
                    Item::Value(v) => {
                        let s = at_u32(&slot_addr_of, to_idx(v));
                        if s != NONE {
                            set_at(&mut ineligible, to_idx(s), true);
                        }
                    }
                    Item::Edge(edge) => {
                        if let Some((arg_start, arg_len)) = edge.args {
                            for k in 0..arg_len {
                                let v = at_u32(view.extra, arg_start.saturating_add(k));
                                let s = at_u32(&slot_addr_of, to_idx(v));
                                if s != NONE {
                                    set_at(&mut ineligible, to_idx(s), true);
                                }
                            }
                        }
                    }
                });
            }
        }
    }

    if readers.is_empty() {
        return Ok(None);
    }
    readers.sort_unstable_by_key(|r| (r.slot, r.inst));

    let mut rewrites = filled(len, TempCopyRewrite::Keep, &alloc);
    let mut inst_touched = filled(len, false, &alloc);
    let mut slot_eliminated = filled(nslots, false, &alloc);
    let mut eliminated_count = 0u32;

    for s in 0..nslots {
        if at_bool(&ineligible, s) {
            continue;
        }
        let writer = writers.get(s).copied().unwrap_or(TempWriterKind::None);
        if writer == TempWriterKind::None {
            continue;
        }

        let s32 = to_u32(s);
        let r_start = readers.partition_point(|r| r.slot < s32);
        let r_rest = readers.get(r_start..).unwrap_or(&[]);
        let r_len = r_rest.partition_point(|r| r.slot == s32);
        let slot_readers = r_rest.get(..r_len).unwrap_or(&[]);
        if slot_readers.is_empty() {
            continue;
        }

        match writer {
            TempWriterKind::None => {}
            TempWriterKind::Memset { inst: w_u32, byte_val, len: w_len } => {
                let w_inst = to_idx(w_u32);
                if at_bool(&inst_touched, w_inst) {
                    continue;
                }
                let w_block = at_u32(&block_of, w_inst);
                let mut ok = true;
                for &r in slot_readers {
                    let r_inst = to_idx(r.inst);
                    if r.len != w_len
                        || at_u32(&block_of, r_inst) != w_block
                        || w_inst >= r_inst
                        || at_bool(&inst_touched, r_inst)
                    {
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    continue;
                }
                let max_r_inst = slot_readers.last().map_or(0, |r| to_idx(r.inst));
                for k in w_inst.saturating_add(1)..max_r_inst {
                    if matches!(view.op(k), Op::SlotLive | Op::SlotDead)
                        && at_u32(view.data, k) == s32
                    {
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    continue;
                }

                set_at(&mut inst_touched, w_inst, true);
                set_at(&mut rewrites, w_inst, TempCopyRewrite::Delete);
                for &r in slot_readers {
                    let r_inst = to_idx(r.inst);
                    set_at(&mut inst_touched, r_inst, true);
                    set_at(
                        &mut rewrites,
                        r_inst,
                        TempCopyRewrite::ToMemset {
                            dst: r.dst,
                            byte_val,
                            len_val: r.len_val,
                            dst_align: r.dst_align,
                        },
                    );
                }
                set_at(&mut slot_eliminated, s, true);
                eliminated_count = eliminated_count.saturating_add(1);
            }
            TempWriterKind::Copy {
                inst: w_u32,
                src: w_src,
                len: w_len,
                src_align: w_src_align,
            } => {
                if slot_readers.len() != 1 {
                    continue;
                }
                let Some(&r) = slot_readers.first() else {
                    continue;
                };
                let w_inst = to_idx(w_u32);
                let r_inst = to_idx(r.inst);
                if r.len != w_len
                    || at_u32(&block_of, r_inst) != at_u32(&block_of, w_inst)
                    || w_inst >= r_inst
                    || at_bool(&inst_touched, w_inst)
                    || at_bool(&inst_touched, r_inst)
                {
                    continue;
                }

                let src_origin = resolve_ptr_origin(&view, &block_of, w_src);
                let mut blocked = false;
                for k in w_inst.saturating_add(1)..r_inst {
                    if blocks_temp_copy_elision(&view, k, s32, src_origin) {
                        blocked = true;
                        break;
                    }
                }
                if blocked {
                    continue;
                }

                let copy_op = if ptrs_provably_disjoint(&view, &block_of, r.dst, w_src, w_len) {
                    Op::Memcpy
                } else {
                    Op::Memmove
                };
                set_at(&mut inst_touched, w_inst, true);
                set_at(&mut inst_touched, r_inst, true);
                set_at(&mut rewrites, w_inst, TempCopyRewrite::Delete);
                set_at(
                    &mut rewrites,
                    r_inst,
                    TempCopyRewrite::ToCopy {
                        op: copy_op,
                        dst: r.dst,
                        src: w_src,
                        len_val: r.len_val,
                        dst_align: r.dst_align,
                        src_align: w_src_align,
                    },
                );
                set_at(&mut slot_eliminated, s, true);
                eliminated_count = eliminated_count.saturating_add(1);
            }
        }
    }

    if eliminated_count == 0 {
        return Ok(None);
    }

    for inst in 0..len {
        if matches!(view.op(inst), Op::SlotAddr | Op::SlotLive | Op::SlotDead)
            && at_bool(&slot_eliminated, to_idx(at_u32(view.data, inst)))
        {
            set_at(&mut rewrites, inst, TempCopyRewrite::Delete);
        }
    }

    let mut val_map = filled(len, NONE, &alloc);
    let mut next_idx: usize = 0;
    for inst in 0..len {
        if rewrites.get(inst).copied().unwrap_or(TempCopyRewrite::Keep) != TempCopyRewrite::Delete {
            set_at(&mut val_map, inst, to_u32(next_idx));
            next_idx = next_idx.saturating_add(1);
        }
    }

    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();
    func.slots.clear();
    func.variables.clear();

    let mut slot_row = filled(nslots, NONE, &alloc);
    for (s, rec) in view.slots.iter().enumerate() {
        if !at_bool(&slot_eliminated, s) {
            set_at(&mut slot_row, s, to_u32(func.slots.len()));
            func.slots.push(*rec);
        }
    }
    for var in view.variables {
        let row = at_u32(&slot_row, to_idx(var.slot));
        if row != NONE {
            let mut mapped = *var;
            mapped.slot = row;
            func.variables.push(mapped);
        }
    }

    let memset_op = op_byte(Op::Memset);
    let memcpy_op = op_byte(Op::Memcpy);
    let memmove_op = op_byte(Op::Memmove);

    for b in 0..blocks {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        for inst in view.block(Block(b32)) {
            let loc = at_u32(view.locs, inst);
            let rw = rewrites.get(inst).copied().unwrap_or(TempCopyRewrite::Keep);
            match rw {
                TempCopyRewrite::Delete => {}
                TempCopyRewrite::Keep => {
                    if matches!(view.op(inst), Op::SlotAddr | Op::SlotLive | Op::SlotDead) {
                        let mapped_slot = at_u32(&slot_row, to_idx(at_u32(view.data, inst)));
                        func.ops.push(at_u8(view.ops, inst));
                        func.flags.push(at_u8(view.flags, inst));
                        func.tys.push(at_u16(view.tys, inst));
                        func.data.push(mapped_slot);
                        func.attrs.push(at_u32(view.attrs, inst));
                        func.locs.push(loc);
                    } else {
                        let _ = copy_inst_same_blocks(&view, inst, &val_map, &mut func);
                    }
                }
                TempCopyRewrite::ToMemset { dst, byte_val, len_val, dst_align } => {
                    let new_dst = at_u32(&val_map, to_idx(dst));
                    let new_byte = at_u32(&val_map, to_idx(byte_val));
                    let new_len = at_u32(&val_map, to_idx(len_val));
                    let attr = intern_align_attr(&mut func, dst_align);
                    let extra_at = to_u32(func.extra.len());
                    func.extra.push(new_dst);
                    func.extra.push(new_byte);
                    func.extra.push(new_len);
                    func.ops.push(memset_op);
                    func.flags.push(Flags::NONE.0);
                    func.tys.push(Ty::NONE.0);
                    func.data.push(extra_at);
                    func.attrs.push(attr);
                    func.locs.push(loc);
                }
                TempCopyRewrite::ToCopy { op, dst, src, len_val, dst_align, src_align } => {
                    let new_dst = at_u32(&val_map, to_idx(dst));
                    let new_src = at_u32(&val_map, to_idx(src));
                    let new_len = at_u32(&val_map, to_idx(len_val));
                    let attr = intern_bulk_attr(&mut func, dst_align, src_align);
                    let extra_at = to_u32(func.extra.len());
                    func.extra.push(new_dst);
                    func.extra.push(new_src);
                    func.extra.push(new_len);
                    let raw_op = if op == Op::Memcpy { memcpy_op } else { memmove_op };
                    func.ops.push(raw_op);
                    func.flags.push(Flags::NONE.0);
                    func.tys.push(Ty::NONE.0);
                    func.data.push(extra_at);
                    func.attrs.push(attr);
                    func.locs.push(loc);
                }
            }
        }
    }

    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    Ok(Some((func, eliminated_count)))
}

fn eliminate_temp_bulk_copies_view<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<Option<(Func<A>, u32)>, SimplifyError> {
    let Some((mut cur, first_elided)) = eliminate_temp_bulk_copies_round(view, alloc)? else {
        return Ok(None);
    };
    let mut total = first_elided;
    for _ in 1..MAX_TEMP_COPY_ELISION_PASSES {
        let Some((next, elided)) = eliminate_temp_bulk_copies_round(cur.view(), alloc)? else {
            break;
        };
        cur = next;
        total = total.saturating_add(elided);
    }
    let cleaned = dead(cur.view(), alloc)?;
    Ok(Some((cleaned, total)))
}

/// Eliminates dead temporary local stack slots used solely as single-writer bulk-copy staging
/// buffers (`Op::Memset -> Op::Memcpy/Memmove` and `Op::Memcpy/Memmove -> Op::Memcpy/Memmove`),
/// compacting eliminated stack slots and running up to 4 fixed-point passes followed by `dead`.
pub fn eliminate_temp_bulk_copies<A: Allocator + Copy>(
    func: Func<A>,
    alloc: A,
) -> Result<(Func<A>, u32), SimplifyError> {
    match eliminate_temp_bulk_copies_view(func.view(), alloc)? {
        Some((cleaned, total)) => Ok((cleaned, total)),
        None => Ok((func, 0)),
    }
}

/// Removes local stack slots that have zero remaining `Op::SlotAddr` references (along with any
/// orphaned `Op::SlotLive` / `Op::SlotDead` markers and `Func::variables` rows), compacting
/// `Func::slots` and remapping surviving `SlotId`s.
///
/// Returns `Ok(None)` with zero function rebuild when every slot is still referenced.
pub fn compact_dead_slots<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<Option<(Func<A>, u32)>, SimplifyError> {
    let nslots = view.slots.len();
    let len = view.len();
    if nslots == 0 || len == 0 {
        return Ok(None);
    }

    let mut slot_used = filled(nslots, false, &alloc);
    for inst in 0..len {
        if view.op(inst) == Op::SlotAddr {
            set_at(&mut slot_used, to_idx(at_u32(view.data, inst)), true);
        }
    }

    let mut dead_slots = 0u32;
    for s in 0..nslots {
        if !at_bool(&slot_used, s) {
            dead_slots = dead_slots.saturating_add(1);
        }
    }
    if dead_slots == 0 {
        return Ok(None);
    }

    let mut drop_inst = filled(len, false, &alloc);
    for inst in 0..len {
        if matches!(view.op(inst), Op::SlotLive | Op::SlotDead)
            && !at_bool(&slot_used, to_idx(at_u32(view.data, inst)))
        {
            set_at(&mut drop_inst, inst, true);
        }
    }

    let mut val_map = filled(len, NONE, &alloc);
    let mut next_idx: usize = 0;
    for inst in 0..len {
        if !at_bool(&drop_inst, inst) {
            set_at(&mut val_map, inst, to_u32(next_idx));
            next_idx = next_idx.saturating_add(1);
        }
    }

    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();
    func.slots.clear();
    func.variables.clear();

    let mut slot_row = filled(nslots, NONE, &alloc);
    for (s, rec) in view.slots.iter().enumerate() {
        if at_bool(&slot_used, s) {
            set_at(&mut slot_row, s, to_u32(func.slots.len()));
            func.slots.push(*rec);
        }
    }
    for var in view.variables {
        let row = at_u32(&slot_row, to_idx(var.slot));
        if row != NONE {
            let mut mapped = *var;
            mapped.slot = row;
            func.variables.push(mapped);
        }
    }

    let blocks = view.blocks();
    for b in 0..blocks {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        for inst in view.block(Block(b32)) {
            if at_bool(&drop_inst, inst) {
                continue;
            }
            if matches!(view.op(inst), Op::SlotAddr | Op::SlotLive | Op::SlotDead) {
                let mapped_slot = at_u32(&slot_row, to_idx(at_u32(view.data, inst)));
                func.ops.push(at_u8(view.ops, inst));
                func.flags.push(at_u8(view.flags, inst));
                func.tys.push(at_u16(view.tys, inst));
                func.data.push(mapped_slot);
                func.attrs.push(at_u32(view.attrs, inst));
                func.locs.push(at_u32(view.locs, inst));
            } else {
                let _ = copy_inst_same_blocks(&view, inst, &val_map, &mut func);
            }
        }
    }

    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    Ok(Some((func, dead_slots)))
}

// ============================================================================
// Small-Memop Scalarization (One Home, Rule 30)
// ============================================================================

const MAX_SMALL_COPY_CHUNKS: usize = 8;
const MAX_SMALL_SLOT_COPY_BYTES: u64 = 32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct CopyChunk {
    off: i32,
    ty: Ty,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct SmallCopyPlan {
    chunks: [CopyChunk; MAX_SMALL_COPY_CHUNKS],
    len: u8,
}

impl SmallCopyPlan {
    #[inline]
    fn single(ty: Ty) -> Self {
        let mut chunks = [CopyChunk { off: 0, ty: Ty::NONE }; MAX_SMALL_COPY_CHUNKS];
        set_at(&mut chunks, 0, CopyChunk { off: 0, ty });
        Self { chunks, len: 1 }
    }

    #[inline]
    fn as_slice(&self) -> &[CopyChunk] {
        let n = usize::from(self.len).min(MAX_SMALL_COPY_CHUNKS);
        self.chunks.get(..n).unwrap_or(&[])
    }
}

#[derive(Clone, Copy, Debug)]
struct BulkMemop {
    dst: u32,
    src: u32,
    len: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct SlotAccess {
    slot: u32,
    off: i64,
    size: i64,
    ty: Ty,
}

struct SlotFields<A: Allocator> {
    accesses: Vec<SlotAccess, A>,
}

impl<A: Allocator> SlotFields<A> {
    #[inline]
    fn empty(alloc: A) -> Self {
        Self { accesses: Vec::new_in(alloc) }
    }

    #[inline]
    fn slice_for(&self, slot: u32) -> &[SlotAccess] {
        let start = self.accesses.partition_point(|a| a.slot < slot);
        let rest = self.accesses.get(start..).unwrap_or(&[]);
        let len = rest.partition_point(|a| a.slot == slot);
        rest.get(..len).unwrap_or(&[])
    }
}

#[inline]
fn bulk_small_memop(view: &View<'_>, inst: usize) -> Option<BulkMemop> {
    if !matches!(view.op(inst), Op::Memcpy | Op::Memmove) {
        return None;
    }
    if Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE) {
        return None;
    }
    let at = to_idx(at_u32(view.data, inst));
    let dst = at_u32(view.extra, at);
    let src = at_u32(view.extra, at.checked_add(1)?);
    let len_val = at_u32(view.extra, at.checked_add(2)?);
    let len = const_u64(view, to_idx(len_val))?;
    if len == 0 || len > MAX_SMALL_SLOT_COPY_BYTES {
        return None;
    }
    Some(BulkMemop { dst, src, len })
}

#[inline]
fn field_copy_ty_bytes(ty: Ty, ptr_bits: u32) -> Option<i64> {
    if !ty.is_scalar() {
        return None;
    }
    match ty {
        Ty::I8 => Some(1),
        Ty::I16 => Some(2),
        Ty::I32 | Ty::F32 => Some(4),
        Ty::I64 | Ty::F64 => Some(8),
        Ty::PTR => {
            let b = ptr_bits / 8;
            if matches!(b, 4 | 8) { Some(i64::from(b)) } else { None }
        }
        _ => None,
    }
}

fn collect_slot_fields<A: Allocator + Copy>(view: &View<'_>, alloc: A) -> SlotFields<A> {
    let mut accesses: Vec<SlotAccess, A> = Vec::new_in(alloc);
    let ptr_bits = view.head.ptr_bits;
    let len = view.len();
    for inst in 0..len {
        let op = view.op(inst);
        if !matches!(op, Op::Load | Op::Store) {
            continue;
        }
        if Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE) {
            continue;
        }
        let at = to_idx(at_u32(view.data, inst));
        let (ptr, inline_off, ty) = if op == Op::Load {
            let ptr = at_u32(view.extra, at);
            let off = i64::from(at_u32(view.extra, at.saturating_add(1)).cast_signed());
            let ty = Ty(at_u16(view.tys, inst));
            (ptr, off, ty)
        } else {
            let val = at_u32(view.extra, at);
            let ptr = at_u32(view.extra, at.saturating_add(1));
            let off = i64::from(at_u32(view.extra, at.saturating_add(2)).cast_signed());
            let ty = Ty(at_u16(view.tys, to_idx(val)));
            (ptr, off, ty)
        };
        let Some((slot, off)) = resolve_slot_ptr_const_view(view, ptr, inline_off) else {
            continue;
        };
        if let Some(size) = field_copy_ty_bytes(ty, ptr_bits) {
            accesses.push(SlotAccess { slot, off, size, ty });
        } else {
            let blocker_size = if ty == Ty::I1 { Some(1) } else { access_bytes(ty, ptr_bits) };
            if let Some(size) = blocker_size {
                accesses.push(SlotAccess { slot, off, size, ty: Ty::NONE });
            }
        }
    }
    accesses.sort_unstable();
    accesses.dedup();
    SlotFields { accesses }
}

fn match_slot_fields<A: Allocator>(
    slot_fields: &SlotFields<A>,
    slot: u32,
    slot_off: i64,
    copy_len: u64,
) -> Option<SmallCopyPlan> {
    let copy_len_i64 = i64::try_from(copy_len).ok()?;
    let range_start = slot_off;
    let range_end = range_start.checked_add(copy_len_i64)?;
    let accesses = slot_fields.slice_for(slot);
    if accesses.is_empty() {
        return None;
    }

    let mut chunks = [CopyChunk { off: 0, ty: Ty::NONE }; MAX_SMALL_COPY_CHUNKS];
    let mut count: usize = 0;
    let mut cursor = range_start;

    for &acc in accesses {
        let acc_end = acc.off.checked_add(acc.size)?;
        if acc_end <= range_start {
            continue;
        }
        if acc.off >= range_end {
            break;
        }
        if acc.ty == Ty::NONE || acc.off < range_start || acc_end > range_end {
            return None;
        }
        if acc.off != cursor || count >= MAX_SMALL_COPY_CHUNKS {
            return None;
        }
        let rel_off = i32::try_from(acc.off.checked_sub(range_start)?).ok()?;
        set_at(&mut chunks, count, CopyChunk { off: rel_off, ty: acc.ty });
        count = count.saturating_add(1);
        cursor = acc_end;
    }

    if cursor == range_end && count > 0 {
        let len = u8::try_from(count).ok()?;
        Some(SmallCopyPlan { chunks, len })
    } else {
        None
    }
}

fn small_memop_plan<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    slot_fields: &SlotFields<A>,
) -> Option<SmallCopyPlan> {
    let op = bulk_small_memop(view, inst)?;
    match op.len {
        1 => return Some(SmallCopyPlan::single(Ty::I8)),
        2 => return Some(SmallCopyPlan::single(Ty::I16)),
        4 => return Some(SmallCopyPlan::single(Ty::I32)),
        8 => return Some(SmallCopyPlan::single(Ty::I64)),
        _ => {}
    }

    let src_slot = resolve_slot_copy_range(view, op.src, op.len);
    let dst_slot = resolve_slot_copy_range(view, op.dst, op.len);
    match (src_slot, dst_slot) {
        (Some((s1, _)), Some((s2, _))) if s1 != s2 => None,
        (Some((s1, o1)), Some((s2, o2))) => match_slot_fields(slot_fields, s1, o1, op.len)
            .or_else(|| match_slot_fields(slot_fields, s2, o2, op.len)),
        (Some((slot, slot_off)), None) | (None, Some((slot, slot_off))) => {
            match_slot_fields(slot_fields, slot, slot_off, op.len)
        }
        (None, None) => None,
    }
}

#[inline]
fn chunk_align(base_align: u8, off: i32) -> u8 {
    if base_align == 0 || off == 0 {
        return base_align;
    }
    let tz = u8::try_from(off.unsigned_abs().trailing_zeros()).unwrap_or(0);
    base_align.min(tz)
}

/// Elides dead temporary bulk-copy chains (`Memset -> Memcpy`, `Memcpy -> Memcpy`) and then
/// scalarizes non-volatile `Op::Memcpy` and `Op::Memmove` instructions into overlap-safe
/// `Op::Load` + `Op::Store` sequences (all loads before all stores):
/// - Constant lengths `1`, `2`, `4`, or `8` bytes on any pointer (`Ty::I8`/`I16`/`I32`/`I64`).
/// - Constant lengths `1..=32` bytes when `src` or `dst` resolves to a local `Op::SlotAddr`,
///   matching the slot's observed scalar field layout (so SROA eliminates the slot completely).
///
/// Returns `Ok(None)` with zero heap allocations when no eligible instruction exists.
pub fn scalarize_small_memops<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<Option<Func<A>>, SimplifyError> {
    let elided = eliminate_temp_bulk_copies_view(view, alloc)?;
    let work_view = elided.as_ref().map_or(view, |(f, _)| f.view());
    let len = work_view.len();
    let mut has_plain_small = false;
    let mut has_slot_candidate = false;
    for inst in 0..len {
        let Some(op) = bulk_small_memop(&work_view, inst) else {
            continue;
        };
        if matches!(op.len, 1 | 2 | 4 | 8) {
            has_plain_small = true;
        } else if resolve_slot_copy_range(&work_view, op.src, op.len).is_some()
            || resolve_slot_copy_range(&work_view, op.dst, op.len).is_some()
        {
            has_slot_candidate = true;
            break;
        }
    }
    if !has_plain_small && !has_slot_candidate {
        return Ok(elided.map(|(f, _)| f));
    }

    let slot_fields = if has_slot_candidate {
        collect_slot_fields(&work_view, alloc)
    } else {
        SlotFields::empty(alloc)
    };

    let mut val_map = filled(len, NONE, &alloc);
    let mut next_idx: usize = 0;
    let mut any_rewritten = false;
    for inst in 0..len {
        if let Some(plan) = small_memop_plan(&work_view, inst, &slot_fields) {
            let expanded = usize::from(plan.len).saturating_mul(2);
            next_idx = next_idx.saturating_add(expanded);
            any_rewritten = true;
        } else {
            set_at(&mut val_map, inst, to_u32(next_idx));
            next_idx = next_idx.saturating_add(1);
        }
    }
    if !any_rewritten {
        return Ok(elided.map(|(f, _)| f));
    }
    if next_idx >= to_idx(NONE) {
        return Err(SimplifyError::Refused { inst: 0, what: "more values than LIR numbers" });
    }

    let mut func = work_view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    let load_op = op_byte(Op::Load);
    let store_op = op_byte(Op::Store);
    let blocks = work_view.blocks();
    for b in 0..blocks {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(work_view.block_kinds, b));
        for inst in work_view.block(Block(b32)) {
            if let Some(plan) = small_memop_plan(&work_view, inst, &slot_fields) {
                let at = to_idx(at_u32(work_view.data, inst));
                let old_dst = at_u32(work_view.extra, at);
                let old_src = at_u32(work_view.extra, at.saturating_add(1));
                let new_dst = at_u32(&val_map, to_idx(old_dst));
                let new_src = at_u32(&val_map, to_idx(old_src));
                let loc = at_u32(work_view.locs, inst);
                let (dst_align, src_align) = match work_view.mem_attrs(inst) {
                    Some(m) => (m.align, m.src_align),
                    None => (0, 0),
                };

                let chunks = plan.as_slice();
                let mut load_idxs = [0u32; MAX_SMALL_COPY_CHUNKS];
                for (i, &chunk) in chunks.iter().enumerate() {
                    let load_attr = intern_align_attr(&mut func, chunk_align(src_align, chunk.off));
                    let load_extra = to_u32(func.extra.len());
                    func.extra.push(new_src);
                    func.extra.push(chunk.off.cast_unsigned());
                    let load_idx = to_u32(func.ops.len());
                    func.ops.push(load_op);
                    func.flags.push(Flags::NONE.0);
                    func.tys.push(chunk.ty.0);
                    func.data.push(load_extra);
                    func.attrs.push(load_attr);
                    func.locs.push(loc);
                    set_at(&mut load_idxs, i, load_idx);
                }

                for (i, &chunk) in chunks.iter().enumerate() {
                    let load_idx = at_u32(&load_idxs, i);
                    let store_attr =
                        intern_align_attr(&mut func, chunk_align(dst_align, chunk.off));
                    let store_extra = to_u32(func.extra.len());
                    func.extra.push(load_idx);
                    func.extra.push(new_dst);
                    func.extra.push(chunk.off.cast_unsigned());
                    func.ops.push(store_op);
                    func.flags.push(Flags::NONE.0);
                    func.tys.push(Ty::NONE.0);
                    func.data.push(store_extra);
                    func.attrs.push(store_attr);
                    func.locs.push(loc);
                }
            } else {
                let _ = copy_inst_same_blocks(&work_view, inst, &val_map, &mut func);
            }
        }
    }

    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    Ok(Some(func))
}

// ============================================================================
// Top-Level Unified MemorySSA Pass
// ============================================================================

/// Runs the single unified MemorySSA optimization pipeline on `view`:
/// 1. Dead-temp bulk-copy elision (`Memset -> Memcpy`, `Memcpy -> Memcpy`) + temp slot compaction.
/// 2. Alias-aware Memory LICM over natural loops.
/// 3. Unified MemorySSA store-to-load, uniform-`Memset`, and sub-word/piece-gather forwarding
///    with `MemPhi` barriers at loop headers & multi-predecessor joins.
/// 4. Dead-Store Elimination (overwritten intervals & dead non-escaping slots/memsets).
/// 5. Dead-code cleanup (`dead`) + dead stack-slot compaction ([`compact_dead_slots`]).
pub fn optimize_memssa<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<MemSsaOutput<A>, SimplifyError> {
    let mut stats = MemSsaStats::default();

    // 1. Dead-temp bulk-copy elision (`Memset -> Memcpy` / `Memcpy -> Memcpy`).
    let mut cur: Option<Func<A>> = match eliminate_temp_bulk_copies_view(view, alloc)? {
        Some((f, elided)) => {
            stats.temp_copies_elided = elided;
            Some(f)
        }
        None => None,
    };

    // 2. Alias-aware Memory LICM.
    {
        let work = cur.as_ref().map_or(view, |f| f.view());
        let alias = AliasAnalysis::analyze(&work, alloc);
        let cfg = CfgInfo::build(&work, alloc);
        let (hoisted_fn, hoisted) = hoist_invariant_loads_ssa(work, &alias, &cfg, alloc)?;
        stats.loads_hoisted = hoisted;
        if let Some(f) = hoisted_fn {
            cur = Some(f);
        }
    }

    // 3. Unified MemorySSA forwarding (exact SSA, uniform Memset, sub-word/piece-gather).
    {
        let work = cur.as_ref().map_or(view, |f| f.view());
        let alias = AliasAnalysis::analyze(&work, alloc);
        let cfg = CfgInfo::build(&work, alloc);
        let fwd = forward_loads_memssa(work, &alias, &cfg, alloc)?;
        stats.loads_forwarded = fwd.loads_forwarded;
        stats.mem_phis_inserted = fwd.mem_phis_inserted;
        if let Some(f) = fwd.func {
            cur = Some(f);
        }
    }

    // 4. Dead-Store Elimination on non-escaping slots & heap memory.
    {
        let work = cur.as_ref().map_or(view, |f| f.view());
        let alias = AliasAnalysis::analyze(&work, alloc);
        let (dse_fn, dse_count) = eliminate_dead_stores_memssa(work, &alias, alloc)?;
        stats.dead_stores_eliminated = dse_count;
        if let Some(f) = dse_fn {
            cur = Some(f);
        }
    }

    // 5. Dead-code cleanup + dead stack-slot compaction.
    let changed = stats.loads_hoisted > 0
        || stats.loads_forwarded > 0
        || stats.dead_stores_eliminated > 0;
    let mut out_func = match cur {
        Some(f) if changed => dead(f.view(), alloc)?,
        Some(f) => f,
        None => view.to_func_in(alloc),
    };

    if let Some((compacted, dead_slots)) = compact_dead_slots(out_func.view(), alloc)? {
        stats.slots_compacted = dead_slots;
        out_func = compacted;
    }

    Ok(MemSsaOutput { func: out_func, stats })
}

// ============================================================================
// Shared Instruction-Copy & Memory-Attribute Helpers
// ============================================================================

fn intern_align_attr<A: Allocator>(func: &mut Func<A>, align: u8) -> u32 {
    if align == 0 {
        return 0;
    }
    let rec = MemAttrsRec { range: 0, align, src_align: 0, pointee_align: 0, bits: 0 };
    if let Some(pos) = func.mem_attrs.iter().position(|it| *it == rec) {
        return to_u32(pos).saturating_add(1);
    }
    let row = to_u32(func.mem_attrs.len());
    func.mem_attrs.push(rec);
    row.saturating_add(1)
}

fn intern_bulk_attr<A: Allocator>(func: &mut Func<A>, align: u8, src_align: u8) -> u32 {
    if align == 0 && src_align == 0 {
        return 0;
    }
    let rec = MemAttrsRec { range: 0, align, src_align, pointee_align: 0, bits: 0 };
    if let Some(pos) = func.mem_attrs.iter().position(|it| *it == rec) {
        return to_u32(pos).saturating_add(1);
    }
    let row = to_u32(func.mem_attrs.len());
    func.mem_attrs.push(rec);
    row.saturating_add(1)
}

fn copy_inst_same_blocks<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    func: &mut Func<A>,
) -> u32 {
    let op = view.op(inst);
    let op_raw = at_u8(view.ops, inst);
    let flags = at_u8(view.flags, inst);
    let ty = at_u16(view.tys, inst);
    let attr = at_u32(view.attrs, inst);
    let loc = at_u32(view.locs, inst);
    let inst_data = at_u32(view.data, inst);
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => at_u32(val_map, to_idx(inst_data)),
        Layout::Data(_) => inst_data,
        Layout::Extra(fields) => {
            let src = to_idx(inst_data);
            let dst = to_u32(func.extra.len());
            let mut p = src;
            for &f in fields {
                match f {
                    Field::Value => {
                        func.extra.push(at_u32(val_map, to_idx(at_u32(view.extra, p))));
                        p = p.saturating_add(1);
                    }
                    Field::Values => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p = p.saturating_add(1);
                        for k in 0..n {
                            func.extra.push(at_u32(
                                val_map,
                                to_idx(at_u32(view.extra, p.saturating_add(k))),
                            ));
                        }
                        p = p.saturating_add(n);
                    }
                    Field::Target => {
                        func.extra.push(at_u32(view.extra, p));
                        let n_u32 = at_u32(view.extra, p.saturating_add(1));
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p = p.saturating_add(2);
                        for k in 0..n {
                            func.extra.push(at_u32(
                                val_map,
                                to_idx(at_u32(view.extra, p.saturating_add(k))),
                            ));
                        }
                        p = p.saturating_add(n);
                    }
                    Field::Cases => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p = p.saturating_add(1);
                        let scrutinee = at_u32(view.extra, src);
                        let w = to_idx(Ty(at_u16(view.tys, to_idx(scrutinee))).const_words());
                        for _ in 0..n {
                            for k in 0..w {
                                func.extra.push(at_u32(view.extra, p.saturating_add(k)));
                            }
                            p = p.saturating_add(w);
                            func.extra.push(at_u32(view.extra, p));
                            let an_u32 = at_u32(view.extra, p.saturating_add(1));
                            let an = to_idx(an_u32);
                            func.extra.push(an_u32);
                            p = p.saturating_add(2);
                            for k in 0..an {
                                func.extra.push(at_u32(
                                    val_map,
                                    to_idx(at_u32(view.extra, p.saturating_add(k))),
                                ));
                            }
                            p = p.saturating_add(an);
                        }
                    }
                    Field::Mask | Field::Blocks => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p = p.saturating_add(1);
                        for k in 0..n {
                            func.extra.push(at_u32(view.extra, p.saturating_add(k)));
                        }
                        p = p.saturating_add(n);
                    }
                    Field::Const => {
                        let w = to_idx(Ty(ty).const_words());
                        for k in 0..w {
                            func.extra.push(at_u32(view.extra, p.saturating_add(k)));
                        }
                        p = p.saturating_add(w);
                    }
                    _ => {
                        func.extra.push(at_u32(view.extra, p));
                        p = p.saturating_add(1);
                    }
                }
            }
            dst
        }
    };
    let idx = to_u32(func.ops.len());
    func.ops.push(op_raw);
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    idx
}

// ============================================================================
// Unit Tests (TestHeap only — NO Global / std:: / vec!)
// ============================================================================

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        IntCond,
        builder::{Builder, Facts, Full, MemAttrs, Param, Variadic},
        eval::{Bodies, BodyId, Outcome, Val, run_calls_in, run_in},
        func::{BlockKind, Conv, RefId, RefKind, SlotKind},
    };

    struct OneCallee<'a> {
        view: View<'a>,
    }

    impl Bodies for OneCallee<'_> {
        fn callee(&self, _from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
            match reference {
                RefId(0) => Some((BodyId([2; 32]), self.view)),
                _ => None,
            }
        }
    }

    #[test]
    fn memssa_forwards_non_escaping_slot_across_call() {
        // Callee mutates its escaped pointer argument to 999 and returns 7.
        let mut cb = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let c_sig = cb.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[Param::of(Ty::I64)]);
        cb.set_signature(c_sig);
        let c_entry = cb.create_block(BlockKind::Normal);
        cb.begin(c_entry);
        let cp = cb.param(Ty::PTR);
        let v999 = cb.iconst(Ty::I64, 999);
        cb.store(Flags::NONE, v999, cp, 0, MemAttrs::aligned(3));
        let v7 = cb.iconst(Ty::I64, 7);
        cb.ret(&[v7]);
        let callee_fn = cb.finish_checked().expect("valid callee");

        // Caller stores `x` to a non-escaping slot `local_s` and `100` to an escaped slot `ext_s`,
        // invokes `callee(ext_s)`, then loads both slots and returns `local + ext + call_ret`.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let callee_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[Param::of(Ty::I64)]);
        let callee_ref = b.reference(RefKind::Func, 0);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);

        let local_s = b.slot(16, 3, SlotKind::Named);
        let ext_s = b.slot(8, 3, SlotKind::Named);
        let local_p = b.slot_addr(local_s);
        let ext_p = b.slot_addr(ext_s);

        let c100 = b.iconst(Ty::I64, 100);
        b.store(Flags::NONE, x, local_p, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, c100, ext_p, 0, MemAttrs::aligned(3));
        let call_ret = b.call(Flags::NONE, callee_ref, callee_sig, &[ext_p]);
        let local_val = b.load(Ty::I64, Flags::NONE, local_p, 0, MemAttrs::aligned(3));
        let ext_val = b.load(Ty::I64, Flags::NONE, ext_p, 0, MemAttrs::aligned(3));
        let sum1 = b.binary(Op::Add, Flags::NONE, local_val, ext_val);
        let sum2 = b.binary(Op::Add, Flags::NONE, sum1, call_ret);
        b.ret(&[sum2]);
        let func = b.finish_checked().expect("valid caller");

        let alias = AliasAnalysis::analyze(&func.view(), TestHeap);
        assert_eq!(alias.slot_escape(0), SlotEscape::NonEscaping);
        assert_eq!(alias.slot_escape(1), SlotEscape::Escaped);

        let out = optimize_memssa(func.view(), TestHeap).expect("optimize_memssa succeeds");
        verify(out.func.view()).expect("optimized caller verifies");
        assert_eq!(
            out.stats.loads_forwarded, 1,
            "non-escaping slot load must forward across Op::Call while escaped slot load stays"
        );
        assert_eq!(
            out.stats.slots_compacted, 1,
            "forwarded non-escaping slot must be compacted out after DSE"
        );
        assert_eq!(out.func.slots.len(), 1, "only escaped ext_s may remain");

        let root = BodyId([1; 32]);
        let bodies = OneCallee { view: callee_fn.view() };
        let fuel = 10_000;
        for x_val in [0u64, 42, 0x1234_5678] {
            let args = [Val::Int(u128::from(x_val))];
            let before =
                run_calls_in(func.view(), root, &args, fuel, &bodies, TestHeap).expect("eval before");
            let after = run_calls_in(out.func.view(), root, &args, fuel, &bodies, TestHeap)
                .expect("eval after");
            assert_eq!(before, after, "eval mismatch for x={x_val}");
            let want = u128::from(x_val.wrapping_add(999).wrapping_add(7));
            assert!(
                matches!(after, Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(want)]),
                "unexpected result for x={x_val}: {after:?}"
            );
        }
    }

    #[test]
    fn memssa_does_not_forward_escaped_slot_across_call() {
        // Negative control: callee clobbers escaped slot `*p = 0x1234_5678`.
        let mut cb = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let c_sig = cb.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
        cb.set_signature(c_sig);
        let c_entry = cb.create_block(BlockKind::Normal);
        cb.begin(c_entry);
        let cp = cb.param(Ty::PTR);
        let magic = cb.iconst(Ty::I64, 0x1234_5678);
        cb.store(Flags::NONE, magic, cp, 0, MemAttrs::aligned(3));
        cb.ret(&[]);
        let clobber_fn = cb.finish_checked().expect("valid clobber callee");

        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let c_sig2 = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
        let c_ref = b.reference(RefKind::Func, 0);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);
        let s = b.slot(8, 3, SlotKind::Named);
        let sp = b.slot_addr(s);
        b.store(Flags::NONE, x, sp, 0, MemAttrs::aligned(3));
        let _ = b.call(Flags::NONE, c_ref, c_sig2, &[sp]);
        let after_call = b.load(Ty::I64, Flags::NONE, sp, 0, MemAttrs::aligned(3));
        b.ret(&[after_call]);
        let func = b.finish_checked().expect("valid escaped-slot caller");

        let alias = AliasAnalysis::analyze(&func.view(), TestHeap);
        assert_eq!(alias.slot_escape(0), SlotEscape::Escaped);

        let out = optimize_memssa(func.view(), TestHeap).expect("optimize_memssa succeeds");
        verify(out.func.view()).expect("optimized function verifies");
        assert_eq!(
            out.stats.loads_forwarded, 0,
            "escaped slot load after Op::Call must NEVER be forwarded"
        );
        let has_load = (0..out.func.view().len()).any(|i| out.func.view().op(i) == Op::Load);
        assert!(has_load, "Op::Load of escaped slot must remain after Op::Call");

        let root = BodyId([1; 32]);
        let bodies = OneCallee { view: clobber_fn.view() };
        let fuel = 10_000;
        let args = [Val::Int(42)];
        let before =
            run_calls_in(func.view(), root, &args, fuel, &bodies, TestHeap).expect("eval before");
        let after = run_calls_in(out.func.view(), root, &args, fuel, &bodies, TestHeap)
            .expect("eval after");
        assert_eq!(before, after);
        assert!(
            matches!(after, Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(0x1234_5678)]),
            "must observe callee write 0x1234_5678, got {after:?}"
        );
    }

    #[test]
    fn memssa_folds_uniform_memset_and_rejects_partial_overlap() {
        // Slot 0 (72B): `Memset(s0, 0x00, 72); Memset(s0, 0x5A, 64)`.
        //   - `Load(I32, s0 + 8)` via `PtrAdd` -> strictly inside `[0, 64)` -> folds to `0x5A5A_5A5A`!
        //   - `Load(I64, s0 + 16)` -> strictly inside `[0, 64)` -> folds to `0x5A5A_5A5A_5A5A_5A5A`!
        //   - `Load(I32, s0 + 62)` -> `[62, 66)` straddles `[0, 64)` and `[64, 72)` -> MUST NOT fold!
        // Slot 1 (64B): `Memset(s1, 0x5A, 64); Load(I32, s1 + 8)` -> folds to `0x5A5A_5A5A`,
        //   dead `Memset` eliminated, and `s1` compacted out completely!
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[],
            &[Param::of(Ty::I32), Param::of(Ty::I64), Param::of(Ty::I32), Param::of(Ty::I32)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);

        let s0 = b.slot(72, 3, SlotKind::Named);
        let p0 = b.slot_addr(s0);
        let zero_b = b.iconst(Ty::I8, 0x00);
        let fill_b = b.iconst(Ty::I8, 0x5a);
        let len72 = b.iconst(Ty::I64, 72);
        let len64 = b.iconst(Ty::I64, 64);
        b.bulk(Op::Memset, Flags::NONE, p0, zero_b, len72, MemAttrs::aligned(3));
        b.bulk(Op::Memset, Flags::NONE, p0, fill_b, len64, MemAttrs::aligned(3));

        let off8 = b.iconst(Ty::I64, 8);
        let p0_8 = b.ptr_add(Flags::NONE, p0, off8);
        let full_i32 = b.load(Ty::I32, Flags::NONE, p0_8, 0, MemAttrs::aligned(2));
        let full_i64 = b.load(Ty::I64, Flags::NONE, p0, 16, MemAttrs::aligned(3));
        let partial_i32 = b.load(Ty::I32, Flags::NONE, p0, 62, MemAttrs::aligned(1));

        let s1 = b.slot(64, 3, SlotKind::Temp);
        let p1 = b.slot_addr(s1);
        b.bulk(Op::Memset, Flags::NONE, p1, fill_b, len64, MemAttrs::aligned(3));
        let s1_i32 = b.load(Ty::I32, Flags::NONE, p1, 8, MemAttrs::aligned(2));

        b.ret(&[full_i32, full_i64, partial_i32, s1_i32]);
        let func = b.finish_checked().expect("valid memset test");

        let out = optimize_memssa(func.view(), TestHeap).expect("optimize_memssa succeeds");
        verify(out.func.view()).expect("optimized memset function verifies");
        assert_eq!(
            out.stats.loads_forwarded, 3,
            "three contained loads must fold while partial-overlap [62..66) stays a Load"
        );
        assert_eq!(out.stats.slots_compacted, 1, "fully folded temp slot s1 must be compacted");
        assert_eq!(out.func.slots.len(), 1, "only s0 needed for partial-overlap load remains");

        let ov = out.func.view();
        let remaining_loads = (0..ov.len()).filter(|&i| ov.op(i) == Op::Load).count();
        assert_eq!(remaining_loads, 1, "exactly the partial-overlap Load must remain");

        let fuel = 50_000;
        let before = run_in(func.view(), &[], fuel, TestHeap).expect("eval before");
        let after = run_in(ov, &[], fuel, TestHeap).expect("eval after");
        assert_eq!(before, after, "eval mismatch after uniform-memset folding");
        let want_partial = u128::from(u32::from_le_bytes([0x5a, 0x5a, 0x00, 0x00]));
        assert!(
            matches!(
                after,
                Outcome::Returned(ref vals)
                    if vals.as_slice()
                        == [
                            Val::Int(0x5a5a_5a5a),
                            Val::Int(0x5a5a_5a5a_5a5a_5a5a),
                            Val::Int(want_partial),
                            Val::Int(0x5a5a_5a5a),
                        ]
            ),
            "unexpected memset eval result: {after:?}"
        );
    }

    #[test]
    fn memssa_loop_header_memphi_blocks_unsound_forwarding() {
        // Negative control for loop backedges + positive test for loop-invariant slot:
        // - `acc_slot` is initialized in `entry` to `init` and incremented in the loop body:
        //   loop-header `MemPhi` MUST prevent forwarding `init` into `header`!
        // - `inv_slot` is initialized in `entry` to `scale` and never written in the loop:
        //   loop-invariant forwarding eliminates `inv_slot` cleanly!
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, header, exit] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let init = b.param(Ty::I64);
        let scale = b.param(Ty::I64);
        let acc_slot = b.slot(8, 3, SlotKind::Named);
        let inv_slot = b.slot(8, 3, SlotKind::Named);
        let acc_p = b.slot_addr(acc_slot);
        let inv_p = b.slot_addr(inv_slot);
        b.store(Flags::NONE, init, acc_p, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, scale, inv_p, 0, MemAttrs::aligned(3));
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let total = b.param(Ty::I64);
        let cur = b.load(Ty::I64, Flags::NONE, acc_p, 0, MemAttrs::aligned(3));
        let k = b.load(Ty::I64, Flags::NONE, inv_p, 0, MemAttrs::aligned(3));
        let term = b.binary(Op::Mul, Flags::NONE, cur, k);
        let total2 = b.binary(Op::Add, Flags::NONE, total, term);
        let one = b.iconst(Ty::I64, 1);
        let cur_next = b.binary(Op::Add, Flags::NONE, cur, one);
        b.store(Flags::NONE, cur_next, acc_p, 0, MemAttrs::aligned(3));
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        let keep = b.icmp(IntCond::Ult, i2, n);
        b.br(Flags::NONE, keep, (header, &[i2, total2]), (exit, &[total2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().expect("valid loop memphi test");

        let out = optimize_memssa(func.view(), TestHeap).expect("optimize_memssa succeeds");
        verify(out.func.view()).expect("optimized loop verifies");
        assert!(
            out.stats.mem_phis_inserted >= 1,
            "loop header must insert a MemPhi barrier for loop-modified acc_slot"
        );
        let ov = out.func.view();
        let loads = (0..ov.len()).filter(|&i| ov.op(i) == Op::Load).count();
        assert_eq!(loads, 1, "acc_slot load must stay while loop-invariant inv_slot load forwards");
        assert_eq!(out.func.slots.len(), 1, "only acc_slot may remain after inv_slot compaction");

        let fuel = 50_000;
        for (n_val, init_val, scale_val, want) in [
            (1u64, 10u64, 3u64, 30u64),
            // iter 0..5: cur = 10, 11, 12, 13, 14 -> sum = 60 -> * 3 = 180
            (5u64, 10u64, 3u64, 180u64),
            // iter 0..4: cur = 1, 2, 3, 4 -> sum = 10 -> * 7 = 70
            (4u64, 1u64, 7u64, 70u64),
        ] {
            let args = [
                Val::Int(u128::from(n_val)),
                Val::Int(u128::from(init_val)),
                Val::Int(u128::from(scale_val)),
            ];
            let before = run_in(func.view(), &args, fuel, TestHeap).expect("eval loop before");
            let after = run_in(ov, &args, fuel, TestHeap).expect("eval loop after");
            assert_eq!(before, after, "loop eval mismatch for n={n_val}");
            assert!(
                matches!(after, Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(u128::from(want))]),
                "unexpected loop result for n={n_val}: {after:?}"
            );
        }
    }

    #[test]
    fn memssa_dse_eliminates_overwritten_store_and_dead_memset() {
        // Non-escaping slot `buf` (16B) read via dynamic offset `idx` (0 or 8), so final writes
        // stay live while overwritten earlier stores are eliminated by interval DSE:
        // - `Store(I64, buf+0, k1)` overwritten by `Store(I64, buf+0, k2)` -> DEAD!
        // - `Store(I32, buf+8, v1)` overwritten by `Memset(buf+8, 0x33, 8)` -> DEAD!
        // Unread slot `dead_s` (32B) with `Memset(dead_s, 0xAA, 32)` -> DEAD + compacted!
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let k1 = b.param(Ty::I64);
        let k2 = b.param(Ty::I64);
        let idx = b.param(Ty::I64);

        let buf = b.slot(16, 3, SlotKind::Named);
        let bp = b.slot_addr(buf);
        b.store(Flags::NONE, k1, bp, 0, MemAttrs::aligned(3)); // dead (overwritten by k2)
        b.store(Flags::NONE, k2, bp, 0, MemAttrs::aligned(3)); // live

        let v1 = b.iconst(Ty::I32, 0xdead_beef);
        b.store(Flags::NONE, v1, bp, 8, MemAttrs::aligned(2)); // dead (covered by memset)
        let off8 = b.iconst(Ty::I64, 8);
        let bp8 = b.ptr_add(Flags::NONE, bp, off8);
        let fill33 = b.iconst(Ty::I8, 0x33);
        let len8 = b.iconst(Ty::I64, 8);
        b.bulk(Op::Memset, Flags::NONE, bp8, fill33, len8, MemAttrs::aligned(3)); // live

        let dead_s = b.slot(32, 3, SlotKind::Temp);
        let dp = b.slot_addr(dead_s);
        let fill_aa = b.iconst(Ty::I8, 0xaa);
        let len32 = b.iconst(Ty::I64, 32);
        b.bulk(Op::Memset, Flags::NONE, dp, fill_aa, len32, MemAttrs::aligned(3)); // dead slot

        let dyn_p = b.ptr_add(Flags::NONE, bp, idx);
        let loaded = b.load(Ty::I64, Flags::NONE, dyn_p, 0, MemAttrs::aligned(3));
        b.ret(&[loaded]);
        let func = b.finish_checked().expect("valid DSE test");

        let out = optimize_memssa(func.view(), TestHeap).expect("optimize_memssa succeeds");
        verify(out.func.view()).expect("DSE output verifies");
        assert_eq!(
            out.stats.dead_stores_eliminated, 3,
            "overwritten k1 store, covered v1 store, and unread dead_s memset must all be eliminated"
        );
        assert_eq!(out.stats.slots_compacted, 1, "dead_s must be compacted out");
        assert_eq!(out.func.slots.len(), 1, "only live buf slot may remain");

        let fuel = 50_000;
        for (idx_val, want) in [
            (0u64, 0x2222_3333_4444_5555u64),
            (8u64, 0x3333_3333_3333_3333u64),
        ] {
            let args = [
                Val::Int(0x1111_1111_1111_1111),
                Val::Int(0x2222_3333_4444_5555),
                Val::Int(u128::from(idx_val)),
            ];
            let before = run_in(func.view(), &args, fuel, TestHeap).expect("eval DSE before");
            let after = run_in(out.func.view(), &args, fuel, TestHeap).expect("eval DSE after");
            assert_eq!(before, after, "DSE eval mismatch for idx={idx_val}");
            assert!(
                matches!(after, Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(u128::from(want))]),
                "unexpected DSE result for idx={idx_val}: {after:?}"
            );
        }
    }

    #[test]
    fn memssa_elides_temp_memset_memcpy_chain_and_compacts_slots() {
        // End-to-end MemorySSA synergy:
        // `Memset(t1, 0x42, 256) -> Memcpy(t2, t1, 256) -> Memcpy(out_s, t2, 256) -> Load(I32, out_s + 16)`
        // 1. Dead-temp bulk-copy elision collapses the chain to `Memset(out_s, 0x42, 256)` and drops `t1`, `t2`.
        // 2. Uniform-`Memset` forwarding folds `Load(I32, out_s + 16)` to `0x4242_4242`.
        // 3. DSE removes the now-unread `Memset(out_s, 0x42, 256)`.
        // 4. `compact_dead_slots` compacts `out_s` out -> ZERO slots, ZERO memops!
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I32)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);

        let out_s = b.slot(256, 2, SlotKind::Named);
        let t1 = b.slot(256, 2, SlotKind::Temp);
        let t2 = b.slot(256, 2, SlotKind::Temp);
        let p_out = b.slot_addr(out_s);
        let n256 = b.iconst(Ty::I64, 256);
        let fill = b.iconst(Ty::I8, 0x42);
        let copy_al = MemAttrs { align: 2, src_align: 2, pointee_align: 0, bits: 0, range: None };

        b.slot_live(t1);
        let p_t1 = b.slot_addr(t1);
        b.bulk(Op::Memset, Flags::NONE, p_t1, fill, n256, MemAttrs::aligned(2));

        b.slot_live(t2);
        let p_t2 = b.slot_addr(t2);
        b.bulk(Op::Memcpy, Flags::NONE, p_t2, p_t1, n256, copy_al);
        b.slot_dead(t1);

        b.bulk(Op::Memcpy, Flags::NONE, p_out, p_t2, n256, copy_al);
        b.slot_dead(t2);

        let res = b.load(Ty::I32, Flags::NONE, p_out, 16, MemAttrs::aligned(2));
        b.ret(&[res]);
        let func = b.finish_checked().expect("valid temp memset/memcpy chain");

        let out = optimize_memssa(func.view(), TestHeap).expect("optimize_memssa succeeds");
        verify(out.func.view()).expect("optimized chain verifies");
        assert_eq!(out.stats.temp_copies_elided, 2, "must elide both temp slots t1 and t2");
        assert_eq!(out.stats.loads_forwarded, 1, "must fold Load(I32) from collapsed Memset");
        assert_eq!(out.stats.dead_stores_eliminated, 1, "must eliminate dead collapsed Memset");
        assert_eq!(out.stats.slots_compacted, 1, "must compact out_s after DSE");
        assert!(out.func.slots.is_empty(), "zero stack slots may remain");

        let ov = out.func.view();
        for inst in 0..ov.len() {
            let op = ov.op(inst);
            assert!(
                !matches!(op, Op::Memset | Op::Memcpy | Op::Memmove | Op::Load | Op::Store | Op::SlotAddr),
                "unexpected remaining memory op {op:?} at inst {inst}"
            );
        }

        let fuel = 50_000;
        let before = run_in(func.view(), &[], fuel, TestHeap).expect("eval chain before");
        let after = run_in(ov, &[], fuel, TestHeap).expect("eval chain after");
        assert_eq!(before, after);
        assert!(
            matches!(after, Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(0x4242_4242)]),
            "expected 0x4242_4242, got {after:?}"
        );
    }

    #[test]
    fn memssa_diamond_join_memphi_blocks_unmerged_forwarding() {
        // Multi-predecessor diamond join: `then` stores `a`, `else` stores `b` into `slot`,
        // `merge` loads `slot` -> `MemPhi` barrier MUST prevent forwarding either `a` or `b`.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I1), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, then_b, else_b, merge_b] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let cond = b.param(Ty::I1);
        let a = b.param(Ty::I64);
        let b_val = b.param(Ty::I64);
        let s = b.slot(8, 3, SlotKind::Named);
        let sp = b.slot_addr(s);
        b.br(Flags::NONE, cond, (then_b, &[]), (else_b, &[]));

        b.begin(then_b);
        b.store(Flags::NONE, a, sp, 0, MemAttrs::aligned(3));
        b.jump((merge_b, &[]));

        b.begin(else_b);
        b.store(Flags::NONE, b_val, sp, 0, MemAttrs::aligned(3));
        b.jump((merge_b, &[]));

        b.begin(merge_b);
        let loaded = b.load(Ty::I64, Flags::NONE, sp, 0, MemAttrs::aligned(3));
        b.ret(&[loaded]);
        let func = b.finish_checked().expect("valid diamond join");

        let out = optimize_memssa(func.view(), TestHeap).expect("optimize_memssa succeeds");
        verify(out.func.view()).expect("diamond join verifies");
        assert!(out.stats.mem_phis_inserted >= 1, "diamond join must insert MemPhi barrier");
        assert_eq!(out.stats.loads_forwarded, 0, "must not forward across unmerged diamond join");

        let fuel = 10_000;
        for (cond_v, want) in [(1u128, 0xaaaa_bbbbu128), (0u128, 0xcccc_ddddu128)] {
            let args = [Val::Int(cond_v), Val::Int(0xaaaa_bbbb), Val::Int(0xcccc_dddd)];
            let before = run_in(func.view(), &args, fuel, TestHeap).expect("eval diamond before");
            let after = run_in(out.func.view(), &args, fuel, TestHeap).expect("eval diamond after");
            assert_eq!(before, after);
            assert!(
                matches!(after, Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(want)]),
                "unexpected diamond result for cond={cond_v}: {after:?}"
            );
        }
    }

    #[test]
    fn memssa_forwards_and_hoists_data_addr_and_disjoint_slot_loads_across_heap_stores() {
        let alloc = blaze_own::testing::heap().alloc();

        // Worker signature: `(out_p: ptr [NOALIAS], ro_p: ptr [NOALIAS | READONLY, deref(8)], n: i64, init: i64) -> i64`.
        let out_param_spec = Param {
            facts: Facts { bits: ParamFactsRec::NOALIAS, ..Facts::default() },
            ..Param::of(Ty::PTR)
        };
        let ro_param_spec = Param {
            facts: Facts {
                deref: 8,
                bits: ParamFactsRec::NOALIAS | ParamFactsRec::READONLY,
                ..Facts::default()
            },
            ..Param::of(Ty::PTR)
        };
        let worker_params = [out_param_spec, ro_param_spec, Param::of(Ty::I64), Param::of(Ty::I64)];

        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &worker_params, &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let out_p = b.param(Ty::PTR);
        let ro_p = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let init = b.param(Ty::I64);

        // Immutable rodata table: two little-endian u64 entries (`0x10` at +0, `0x20` at +8).
        let table_bytes: [u8; 16] = [
            0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let data_ref = b.data(&table_bytes, 3, &[]);
        let tbl_p = b.data_addr(data_ref);
        let tbl_frozen = b.unary(Op::Freeze, Flags::NONE, tbl_p);

        // Non-escaping local stack slot initialized in entry and read in the loop body.
        let local_s = b.slot(8, 3, SlotKind::Named);
        let local_p = b.slot_addr(local_s);
        b.store(Flags::NONE, init, local_p, 0, MemAttrs::aligned(3));

        // Redundant-load CSE across a disjoint `NOALIAS` heap store in `entry`.
        let ro_first = b.load(Ty::I64, Flags::NONE, ro_p, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, ro_first, out_p, 0, MemAttrs::aligned(3));
        let ro_dup = b.load(Ty::I64, Flags::NONE, ro_p, 0, MemAttrs::aligned(3));

        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero, ro_dup]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let cond = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, cond, (body, &[]), (exit, &[acc]));

        b.begin(body);
        // All three loads are provably disjoint from `store out_p` and non-trapping in `body`:
        // - `tbl_v`: `BaseObj::DataAddr` (via `Op::Freeze`), in-bounds [8..16] <= 16
        // - `slot_v`: non-escaping `BaseObj::Slot`, in-bounds [0..8] <= 8
        // - `ro_v`: `NOALIAS` + `deref(8)` `BaseObj::Param(1)` (also already dereferenced in `entry`)
        let tbl_v = b.load(Ty::I64, Flags::NONE, tbl_frozen, 8, MemAttrs::aligned(3));
        let slot_v = b.load(Ty::I64, Flags::NONE, local_p, 0, MemAttrs::aligned(3));
        let ro_v = b.load(Ty::I64, Flags::NONE, ro_p, 0, MemAttrs::aligned(3));
        let s1 = b.binary(Op::Add, Flags::NONE, tbl_v, slot_v);
        let s2 = b.binary(Op::Add, Flags::NONE, s1, ro_v);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, s2);
        b.store(Flags::NONE, acc2, out_p, 0, MemAttrs::aligned(3));
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, acc2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let worker = b.finish_checked().expect("valid worker");

        // Verify explicit `BaseObj` disjointness lattice directly.
        let alias = AliasAnalysis::analyze(&worker.view(), alloc);
        assert_eq!(alias.base_of(tbl_p.0), BaseObj::DataAddr(data_ref));
        assert_eq!(alias.base_of(tbl_frozen.0), BaseObj::DataAddr(data_ref));
        assert_eq!(alias.base_of(local_p.0), BaseObj::Slot(local_s));
        assert_eq!(alias.base_of(out_p.0), BaseObj::Param { index: 0, is_noalias: true });
        assert_eq!(alias.base_of(ro_p.0), BaseObj::Param { index: 1, is_noalias: true });
        assert!(!alias.may_alias_bases(BaseObj::DataAddr(data_ref), alias.base_of(out_p.0)));
        assert!(!alias.may_alias_bases(BaseObj::DataAddr(data_ref), BaseObj::Unknown));
        assert!(!alias.may_alias_bases(BaseObj::Slot(local_s), alias.base_of(out_p.0)));
        assert!(!alias.may_alias_bases(alias.base_of(out_p.0), alias.base_of(ro_p.0)));

        let out = optimize_memssa(worker.view(), alloc).expect("optimize_memssa succeeds");
        verify(out.func.view()).expect("optimized worker verifies");
        assert_eq!(
            out.stats.loads_hoisted, 3,
            "DataAddr, non-escaping Slot, and NOALIAS Param loads must all hoist out of the loop"
        );
        assert!(
            out.stats.loads_forwarded >= 3,
            "disjoint Param CSE + hoisted Slot/Param loads must forward across heap store, got {}",
            out.stats.loads_forwarded
        );
        assert!(
            out.func.slots.is_empty(),
            "forwarded & DSE'd non-escaping local slot must be compacted out"
        );

        let ov = out.func.view();
        for loop_blk in [Block(1), Block(2)] {
            for inst in ov.block(loop_blk) {
                assert_ne!(
                    ov.op(inst),
                    Op::Load,
                    "no Op::Load may remain inside loop block {loop_blk} after hoisting"
                );
            }
        }

        // Driver allocates two disjoint slots (`out_slot`, `ro_slot`) and calls `worker`.
        let mut db = Builder::<_, Full>::new_in(64, alloc);
        let d_sig = db.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
        );
        db.set_signature(d_sig);
        let w_sig = db.sig(Conv::C, Variadic::No, &worker_params, &[Param::of(Ty::I64)]);
        let w_ref = db.reference(RefKind::Func, 0);
        let d_entry = db.create_block(BlockKind::Normal);
        db.begin(d_entry);
        let dn = db.param(Ty::I64);
        let dinit = db.param(Ty::I64);
        let dro_init = db.param(Ty::I64);
        let out_slot = db.slot(8, 3, SlotKind::Named);
        let ro_slot = db.slot(8, 3, SlotKind::Named);
        let out_sp = db.slot_addr(out_slot);
        let ro_sp = db.slot_addr(ro_slot);
        let dzero = db.iconst(Ty::I64, 0);
        db.store(Flags::NONE, dzero, out_sp, 0, MemAttrs::aligned(3));
        db.store(Flags::NONE, dro_init, ro_sp, 0, MemAttrs::aligned(3));
        let w_ret = db.call(Flags::NONE, w_ref, w_sig, &[out_sp, ro_sp, dn, dinit]);
        let out_final = db.load(Ty::I64, Flags::NONE, out_sp, 0, MemAttrs::aligned(3));
        db.ret(&[w_ret, out_final]);
        let driver = db.finish_checked().expect("valid driver");

        let root = BodyId([1; 32]);
        let before_bodies = OneCallee { view: worker.view() };
        let after_bodies = OneCallee { view: ov };
        let fuel = 50_000;
        for (n_val, init_val, ro_val, want) in [
            // n = 0: loop runs 0 times -> speculative hoisting of DataAddr/Slot/deref-Param is safe!
            (0u64, 5u64, 7u64, 7u64),
            // n = 1: 7 + (0x20 + 5 + 7) = 51
            (1u64, 5u64, 7u64, 51u64),
            // n = 4: 7 + 4 * (0x20 + 5 + 7) = 183
            (4u64, 5u64, 7u64, 183u64),
        ] {
            let args = [
                Val::Int(u128::from(n_val)),
                Val::Int(u128::from(init_val)),
                Val::Int(u128::from(ro_val)),
            ];
            let before = run_calls_in(driver.view(), root, &args, fuel, &before_bodies, alloc)
                .expect("eval worker before");
            let after = run_calls_in(driver.view(), root, &args, fuel, &after_bodies, alloc)
                .expect("eval worker after");
            assert_eq!(before, after, "eval mismatch for n={n_val}");
            let want_u128 = u128::from(want);
            assert!(
                matches!(
                    after,
                    Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(want_u128), Val::Int(want_u128)]
                ),
                "unexpected result for n={n_val}: {after:?}"
            );
        }
    }

    #[test]
    fn memssa_respects_escaping_slot_and_aliasing_param_negative_controls() {
        let alloc = blaze_own::testing::heap().alloc();

        // ------------------------------------------------------------------
        // Control A: Escaping slot (address stored to memory) must NEVER be
        // treated as disjoint from an unknown/loaded pointer or parameter.
        // ------------------------------------------------------------------
        let mut eb = Builder::<_, Full>::new_in(64, alloc);
        let e_params = [Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)];
        let e_sig = eb.sig(Conv::C, Variadic::No, &e_params, &[Param::of(Ty::I64)]);
        eb.set_signature(e_sig);
        let [e_entry, e_header, e_body, e_exit] = [(); 4].map(|_| eb.create_block(BlockKind::Normal));

        eb.begin(e_entry);
        let holder_p = eb.param(Ty::PTR);
        let en = eb.param(Ty::I64);
        let einit = eb.param(Ty::I64);
        let esc_s = eb.slot(8, 3, SlotKind::Named);
        let esc_p = eb.slot_addr(esc_s);
        eb.store(Flags::NONE, einit, esc_p, 0, MemAttrs::aligned(3));
        // Storing `esc_p` to memory escapes `esc_s`!
        eb.store(Flags::NONE, esc_p, holder_p, 0, MemAttrs::aligned(3));
        let ezero = eb.iconst(Ty::I64, 0);
        eb.jump((e_header, &[ezero, ezero]));

        eb.begin(e_header);
        let ei = eb.param(Ty::I64);
        let eacc = eb.param(Ty::I64);
        let econd = eb.icmp(IntCond::Ult, ei, en);
        eb.br(Flags::NONE, econd, (e_body, &[]), (e_exit, &[eacc]));

        eb.begin(e_body);
        let alias_p = eb.load(Ty::PTR, Flags::NONE, holder_p, 0, MemAttrs::aligned(3));
        let cur = eb.load(Ty::I64, Flags::NONE, esc_p, 0, MemAttrs::aligned(3));
        let eacc2 = eb.binary(Op::Add, Flags::NONE, eacc, cur);
        let eone = eb.iconst(Ty::I64, 1);
        let next_v = eb.binary(Op::Add, Flags::NONE, cur, eone);
        // Mutates `esc_s` through the loaded pointer `alias_p` (`BaseObj::Unknown`).
        eb.store(Flags::NONE, next_v, alias_p, 0, MemAttrs::aligned(3));
        let ei2 = eb.binary(Op::Add, Flags::NONE, ei, eone);
        eb.jump((e_header, &[ei2, eacc2]));

        eb.begin(e_exit);
        let eres = eb.param(Ty::I64);
        eb.ret(&[eres]);
        let esc_fn = eb.finish_checked().expect("valid escaping-slot fn");

        let e_alias = AliasAnalysis::analyze(&esc_fn.view(), alloc);
        assert_eq!(e_alias.slot_escape(0), SlotEscape::Escaped);
        assert_eq!(e_alias.base_of(alias_p.0), BaseObj::Unknown);
        assert!(
            e_alias.may_alias_bases(BaseObj::Slot(esc_s), BaseObj::Unknown),
            "escaping slot MUST may-alias BaseObj::Unknown"
        );
        assert!(
            e_alias.may_alias_bases(BaseObj::Slot(esc_s), e_alias.base_of(holder_p.0)),
            "escaping slot MUST may-alias entry Param"
        );

        let e_out = optimize_memssa(esc_fn.view(), alloc).expect("optimize escaping-slot fn");
        verify(e_out.func.view()).expect("escaping-slot output verifies");
        assert_eq!(e_out.stats.loads_hoisted, 0, "escaping slot load must NOT hoist");
        assert_eq!(e_out.stats.loads_forwarded, 0, "escaping slot load must NOT forward");
        assert_eq!(e_out.func.slots.len(), 1, "escaping slot must remain");

        // Driver invokes `esc_fn(&holder_slot, 4, 10)` -> 10 + 11 + 12 + 13 = 46.
        let mut edb = Builder::<_, Full>::new_in(64, alloc);
        let ed_sig = edb.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
        edb.set_signature(ed_sig);
        let ec_sig = edb.sig(Conv::C, Variadic::No, &e_params, &[Param::of(Ty::I64)]);
        let ec_ref = edb.reference(RefKind::Func, 0);
        let ed_entry = edb.create_block(BlockKind::Normal);
        edb.begin(ed_entry);
        let holder_slot = edb.slot(8, 3, SlotKind::Named);
        let holder_sp = edb.slot_addr(holder_slot);
        let c4 = edb.iconst(Ty::I64, 4);
        let c10 = edb.iconst(Ty::I64, 10);
        let eret = edb.call(Flags::NONE, ec_ref, ec_sig, &[holder_sp, c4, c10]);
        edb.ret(&[eret]);
        let e_driver = edb.finish_checked().expect("valid escaping-slot driver");

        let root = BodyId([1; 32]);
        let fuel = 50_000;
        let e_before = run_calls_in(
            e_driver.view(),
            root,
            &[],
            fuel,
            &OneCallee { view: esc_fn.view() },
            alloc,
        )
        .expect("eval esc before");
        let e_after = run_calls_in(
            e_driver.view(),
            root,
            &[],
            fuel,
            &OneCallee { view: e_out.func.view() },
            alloc,
        )
        .expect("eval esc after");
        assert_eq!(e_before, e_after);
        assert!(
            matches!(e_after, Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(46)]),
            "expected 46 for escaping slot loop, got {e_after:?}"
        );

        // ------------------------------------------------------------------
        // Control B: Two plain (non-`NOALIAS`) pointer parameters must NEVER
        // be treated as disjoint (including through `PtrWithAddr` / `PtrAdd`).
        // ------------------------------------------------------------------
        let mut pb = Builder::<_, Full>::new_in(64, alloc);
        let p_params = [Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)];
        let p_sig = pb.sig(Conv::C, Variadic::No, &p_params, &[Param::of(Ty::I64)]);
        pb.set_signature(p_sig);
        let [p_entry, p_header, p_exit] = [(); 3].map(|_| pb.create_block(BlockKind::Normal));

        pb.begin(p_entry);
        let p1 = pb.param(Ty::PTR);
        let p2 = pb.param(Ty::PTR);
        let pn = pb.param(Ty::I64);
        let c100 = pb.iconst(Ty::I64, 100);
        let c200 = pb.iconst(Ty::I64, 200);
        pb.store(Flags::NONE, c100, p1, 0, MemAttrs::aligned(3));
        pb.store(Flags::NONE, c200, p2, 0, MemAttrs::aligned(3));
        // MUST NOT forward `100` from `p1` across potentially-aliasing store to `p2`!
        let v_entry = pb.load(Ty::I64, Flags::NONE, p1, 0, MemAttrs::aligned(3));
        let pzero = pb.iconst(Ty::I64, 0);
        pb.jump((p_header, &[pzero, v_entry]));

        pb.begin(p_header);
        let pi = pb.param(Ty::I64);
        let pacc = pb.param(Ty::I64);
        // MUST NOT hoist `load p1` out of loop containing store to aliasing `p2`!
        let pcur = pb.load(Ty::I64, Flags::NONE, p1, 0, MemAttrs::aligned(3));
        let pacc2 = pb.binary(Op::Add, Flags::NONE, pacc, pcur);
        let pone = pb.iconst(Ty::I64, 1);
        let pnext = pb.binary(Op::Add, Flags::NONE, pcur, pone);
        pb.store(Flags::NONE, pnext, p2, 0, MemAttrs::aligned(3));
        let pi2 = pb.binary(Op::Add, Flags::NONE, pi, pone);
        let pkeep = pb.icmp(IntCond::Ult, pi2, pn);
        pb.br(Flags::NONE, pkeep, (p_header, &[pi2, pacc2]), (p_exit, &[pacc2]));

        pb.begin(p_exit);
        let pres = pb.param(Ty::I64);
        pb.ret(&[pres]);
        let alias_fn = pb.finish_checked().expect("valid aliasing-params fn");

        let p_alias = AliasAnalysis::analyze(&alias_fn.view(), alloc);
        assert_eq!(p_alias.base_of(p1.0), BaseObj::Param { index: 0, is_noalias: false });
        assert_eq!(p_alias.base_of(p2.0), BaseObj::Param { index: 1, is_noalias: false });
        assert!(
            p_alias.may_alias_bases(p_alias.base_of(p1.0), p_alias.base_of(p2.0)),
            "two non-NOALIAS entry pointer params MUST may-alias"
        );

        let p_out = optimize_memssa(alias_fn.view(), alloc).expect("optimize aliasing-params fn");
        verify(p_out.func.view()).expect("aliasing-params output verifies");
        assert_eq!(p_out.stats.loads_hoisted, 0, "aliasing param load must NOT hoist");
        assert_eq!(p_out.stats.loads_forwarded, 0, "aliasing param load must NOT forward");

        // Driver aliases both params to the SAME slot `shared`:
        // entry: `*shared = 100; *shared = 200; v_entry = 200`
        // loop (3 iters): reads 200, 201, 202 -> total = 200 + 200 + 201 + 202 = 803.
        let mut pdb = Builder::<_, Full>::new_in(64, alloc);
        let pd_sig = pdb.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
        pdb.set_signature(pd_sig);
        let pc_sig = pdb.sig(Conv::C, Variadic::No, &p_params, &[Param::of(Ty::I64)]);
        let pc_ref = pdb.reference(RefKind::Func, 0);
        let pd_entry = pdb.create_block(BlockKind::Normal);
        pdb.begin(pd_entry);
        let shared = pdb.slot(8, 3, SlotKind::Named);
        let shared_p = pdb.slot_addr(shared);
        let c3 = pdb.iconst(Ty::I64, 3);
        let pret = pdb.call(Flags::NONE, pc_ref, pc_sig, &[shared_p, shared_p, c3]);
        pdb.ret(&[pret]);
        let p_driver = pdb.finish_checked().expect("valid aliasing-params driver");

        let p_before = run_calls_in(
            p_driver.view(),
            root,
            &[],
            fuel,
            &OneCallee { view: alias_fn.view() },
            alloc,
        )
        .expect("eval aliasing params before");
        let p_after = run_calls_in(
            p_driver.view(),
            root,
            &[],
            fuel,
            &OneCallee { view: p_out.func.view() },
            alloc,
        )
        .expect("eval aliasing params after");
        assert_eq!(p_before, p_after);
        assert!(
            matches!(p_after, Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(803)]),
            "expected 803 when p1 == p2, got {p_after:?}"
        );

        // Also verify `Op::PtrWithAddr` preserves provenance base while dropping constant offset,
        // and `Op::PtrAddr` / `Op::Expose` mark a local stack slot as `SlotEscape::Escaped`.
        let mut tb = Builder::<_, Full>::new_in(64, alloc);
        let t_noalias = Param {
            facts: Facts { bits: ParamFactsRec::NOALIAS, ..Facts::default() },
            ..Param::of(Ty::PTR)
        };
        let t_sig = tb.sig(Conv::C, Variadic::No, &[t_noalias, Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        tb.set_signature(t_sig);
        let t_entry = tb.create_block(BlockKind::Normal);
        tb.begin(t_entry);
        let tp = tb.param(Ty::PTR);
        let taddr = tb.param(Ty::I64);
        let pwa = tb.ptr_with_addr(tp, taddr);
        let s_exposed = tb.slot(8, 3, SlotKind::Named);
        let sp_exposed = tb.slot_addr(s_exposed);
        let exposed_int = tb.convert(Op::PtrAddr, Flags::NONE, Ty::I64, sp_exposed);
        let pwa_val = tb.load(Ty::I64, Flags::NONE, pwa, 0, MemAttrs::aligned(3));
        let combined = tb.binary(Op::Add, Flags::NONE, exposed_int, pwa_val);
        tb.ret(&[combined]);
        let trace_fn = tb.finish_checked().expect("valid provenance-trace fn");

        let t_alias = AliasAnalysis::analyze(&trace_fn.view(), alloc);
        assert_eq!(
            t_alias.base_of(pwa.0),
            BaseObj::Param { index: 0, is_noalias: true },
            "PtrWithAddr must preserve provenance BaseObj of its pointer operand"
        );
        assert_eq!(
            t_alias.slot_escape(0),
            SlotEscape::Escaped,
            "Op::PtrAddr on a slot pointer must mark the slot as Escaped"
        );
    }
}
