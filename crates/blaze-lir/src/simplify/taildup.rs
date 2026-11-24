//! Interpreter / State-Machine `Switch` Tail Duplication (`blaze_lir::taildup`).
//!
//! Bytecode VMs (such as `revm` in `reth`), parsers, and state machines spend most of their time
//! jumping from each opcode arm back to a single shared `Op::Switch` dispatch block. A single
//! shared indirect jump (`br x17` / `jmp rax`) aliases every opcode transition in the CPU's
//! indirect branch target buffer (BTB), causing heavy mispredictions.
//!
//! This pass detects dense `Op::Switch` dispatch headers (`>= 4` cases, small read-only dispatch
//! logic) in both:
//! 1. **Direct 1-block dispatch**: `disp` ends in `Op::Switch` and arms end in `Jump(disp, args)`.
//! 2. **Bounded 2-block interpreter dispatch**: `guard` ends in `Op::Br(cond, decode, exit)` where
//!    `decode` has sole predecessor `guard` and ends in `Op::Switch`, and arms end in
//!    `Jump(guard, args)`.
//!
//! For each compact arm, the pass duplicates the bounds check (if any) + opcode fetch +
//! `Op::Switch` directly into the arm's tail (placing each arm's dedicated `decode_k` block
//! immediately after `arm_k` so live intervals stay disjoint across arms for zero-move register
//! coalescing and straight-line I-cache fallthrough) — giving the hardware branch predictor
//! **per-opcode indirect transition history** (computed-goto threaded dispatch with zero source
//! changes)!
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, each, filled},
};
use crate::{
    Block, BlockKind, Func, Op, Ty, View,
    op::{Field, Layout},
    verify,
};

/// Telemetry returned by [`tail_duplicate_dispatch`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct TailDupStats {
    pub dispatches_found: u32,
    pub arms_duplicated: u32,
}

/// Output of [`tail_duplicate_dispatch`].
#[derive(Debug)]
pub struct TailDupOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: TailDupStats,
}

const MIN_SWITCH_CASES: usize = 4;
const MAX_DISPATCH_INSTS: usize = 12;
const MAX_GUARD_INSTS: usize = 6;
const MAX_ARM_INSTS: usize = 24;
const MAX_DUPLICATED_ARMS: usize = 16;

enum DispatchKind {
    /// Single-block dispatch: `disp` ends directly in `Op::Switch`.
    Direct { disp: usize },
    /// Two-block bounded dispatch: `guard` ends in `Op::Br(cond, decode, exit)` and `decode` ends
    /// in `Op::Switch`.
    Bounded {
        guard: usize,
        decode: usize,
        decode_is_then: bool,
    },
}

struct DispatchPlan<A: Allocator> {
    kind: DispatchKind,
    promoted: Vec<Vec<u32, A>, A>,
    dup_arm: Vec<bool, A>,
    dup_count: usize,
}

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(0)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn at<T: Copy + Default>(s: &[T], i: usize) -> T {
    s.get(i).copied().unwrap_or_default()
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn op_byte(op: Op) -> u8 {
    Op::ALL.iter().position(|&c| c == op).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

#[inline]
fn is_duplicable_dispatch_op(op: Op) -> bool {
    !matches!(
        op,
        Op::Store
            | Op::Call
            | Op::CallIndirect
            | Op::Invoke
            | Op::InvokeIndirect
            | Op::AtomicStore
            | Op::AtomicRmw
            | Op::Cmpxchg
            | Op::Fence
            | Op::Memcpy
            | Op::Memmove
            | Op::Memset
            | Op::Asm
            | Op::AsmBr
    )
}

fn copy_remapped<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    blk_map: &[u32],
    func: &mut Func<A>,
) -> u32 {
    let op = view.op(inst);
    let flags = at(view.flags, inst);
    let ty = at(view.tys, inst);
    let attr = at(view.attrs, inst);
    let loc = at(view.locs, inst);
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => at(val_map, to_idx(at(view.data, inst))),
        Layout::Data(Field::Block) => at(blk_map, to_idx(at(view.data, inst))),
        Layout::Data(_) => at(view.data, inst),
        Layout::Extra(fields) => {
            let src = to_idx(at(view.data, inst));
            let dst = to_u32(func.extra.len());
            let mut p = src;
            for &f in fields {
                match f {
                    Field::Value => {
                        func.extra.push(at(val_map, to_idx(at(view.extra, p))));
                        p += 1;
                    }
                    Field::Values => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Block => {
                        func.extra.push(at(blk_map, to_idx(at(view.extra, p))));
                        p += 1;
                    }
                    Field::Blocks => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(blk_map, to_idx(at(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Target => {
                        func.extra.push(at(blk_map, to_idx(at(view.extra, p))));
                        let n_u32 = at(view.extra, p + 1);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 2;
                        for k in 0..n {
                            func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Cases => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        let scrutinee = at(view.extra, src);
                        let w = to_idx(Ty(at(view.tys, to_idx(scrutinee))).const_words());
                        for _ in 0..n {
                            for k in 0..w {
                                func.extra.push(at(view.extra, p + k));
                            }
                            p += w;
                            func.extra.push(at(blk_map, to_idx(at(view.extra, p))));
                            let an_u32 = at(view.extra, p + 1);
                            let an = to_idx(an_u32);
                            func.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
                            }
                            p += an;
                        }
                    }
                    Field::Mask => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(view.extra, p + k));
                        }
                        p += n;
                    }
                    Field::Const => {
                        let w = to_idx(Ty(ty).const_words());
                        for k in 0..w {
                            func.extra.push(at(view.extra, p + k));
                        }
                        p += w;
                    }
                    _ => {
                        func.extra.push(at(view.extra, p));
                        p += 1;
                    }
                }
            }
            dst
        }
    };
    let idx = to_u32(func.ops.len());
    func.ops.push(at(view.ops, inst));
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    idx
}

/// Emits `Op::Switch` at `inst`, remapping target blocks through `blk_map` and appending promoted
/// dispatch defs onto any target block's edge args.
fn copy_switch_with_promoted<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    blk_map: &[u32],
    promoted: &[Vec<u32, A>],
    func: &mut Func<A>,
) -> u32 {
    let src = to_idx(at(view.data, inst));
    let dst = to_u32(func.extra.len());
    let scrutinee = at(view.extra, src);
    let w = to_idx(Ty(at(view.tys, to_idx(scrutinee))).const_words());
    func.extra.push(at(val_map, to_idx(scrutinee)));
    let mut p = src + 1;
    // Default target:
    let def_b = at(view.extra, p);
    let def_an = to_idx(at(view.extra, p + 1));
    let def_extra = promoted.get(to_idx(def_b)).map(Vec::as_slice).unwrap_or(&[]);
    func.extra.push(at(blk_map, to_idx(def_b)));
    func.extra.push(to_u32(def_an + def_extra.len()));
    p += 2;
    for k in 0..def_an {
        func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
    }
    for &pv in def_extra.iter() {
        func.extra.push(at(val_map, to_idx(pv)));
    }
    p += def_an;
    // Cases:
    let n_cases_u32 = at(view.extra, p);
    let n_cases = to_idx(n_cases_u32);
    func.extra.push(n_cases_u32);
    p += 1;
    for _ in 0..n_cases {
        for k in 0..w {
            func.extra.push(at(view.extra, p + k));
        }
        p += w;
        let cb = at(view.extra, p);
        let can = to_idx(at(view.extra, p + 1));
        let c_extra = promoted.get(to_idx(cb)).map(Vec::as_slice).unwrap_or(&[]);
        func.extra.push(at(blk_map, to_idx(cb)));
        func.extra.push(to_u32(can + c_extra.len()));
        p += 2;
        for k in 0..can {
            func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
        }
        for &pv in c_extra.iter() {
            func.extra.push(at(val_map, to_idx(pv)));
        }
        p += can;
    }
    let idx = to_u32(func.ops.len());
    func.ops.push(op_byte(Op::Switch));
    func.flags.push(at(view.flags, inst));
    func.tys.push(at(view.tys, inst));
    func.data.push(dst);
    func.attrs.push(at(view.attrs, inst));
    func.locs.push(at(view.locs, inst));
    idx
}

/// Emits the `Op::Br` terminator of a bounded 2-block dispatch `guard`, optionally redirecting its
/// `decode` edge to a dedicated per-arm `decode_override` block (with 0 args) and appending any
/// promoted `guard` defs onto the `exit` edge.
fn copy_br_for_guard<A: Allocator>(
    view: &View<'_>,
    guard_term: usize,
    val_map: &[u32],
    blk_map: &[u32],
    promoted: &[Vec<u32, A>],
    decode_is_then: bool,
    decode_override: Option<u32>,
    force_flags: Option<u8>,
    func: &mut Func<A>,
) -> u32 {
    let hat = to_idx(at(view.data, guard_term));
    let cond_v = at(view.extra, hat);
    let t_blk = at(view.extra, hat + 1);
    let t_argc = to_idx(at(view.extra, hat + 2));
    let e_at = hat + 3 + t_argc;
    let e_blk = at(view.extra, e_at);
    let e_argc = to_idx(at(view.extra, e_at + 1));

    let dst = to_u32(func.extra.len());
    func.extra.push(at(val_map, to_idx(cond_v)));

    // `then` edge:
    if decode_is_then {
        if let Some(dup_dec) = decode_override {
            func.extra.push(dup_dec);
            func.extra.push(0);
        } else {
            func.extra.push(at(blk_map, to_idx(t_blk)));
            func.extra.push(to_u32(t_argc));
            for k in 0..t_argc {
                func.extra.push(at(val_map, to_idx(at(view.extra, hat + 3 + k))));
            }
        }
    } else {
        let ex_extra = promoted.get(to_idx(t_blk)).map(Vec::as_slice).unwrap_or(&[]);
        func.extra.push(at(blk_map, to_idx(t_blk)));
        func.extra.push(to_u32(t_argc + ex_extra.len()));
        for k in 0..t_argc {
            func.extra.push(at(val_map, to_idx(at(view.extra, hat + 3 + k))));
        }
        for &pv in ex_extra.iter() {
            func.extra.push(at(val_map, to_idx(pv)));
        }
    }

    // `else` edge:
    if !decode_is_then {
        if let Some(dup_dec) = decode_override {
            func.extra.push(dup_dec);
            func.extra.push(0);
        } else {
            func.extra.push(at(blk_map, to_idx(e_blk)));
            func.extra.push(to_u32(e_argc));
            for k in 0..e_argc {
                func.extra.push(at(val_map, to_idx(at(view.extra, e_at + 2 + k))));
            }
        }
    } else {
        let ex_extra = promoted.get(to_idx(e_blk)).map(Vec::as_slice).unwrap_or(&[]);
        func.extra.push(at(blk_map, to_idx(e_blk)));
        func.extra.push(to_u32(e_argc + ex_extra.len()));
        for k in 0..e_argc {
            func.extra.push(at(val_map, to_idx(at(view.extra, e_at + 2 + k))));
        }
        for &pv in ex_extra.iter() {
            func.extra.push(at(val_map, to_idx(pv)));
        }
    }

    let idx = to_u32(func.ops.len());
    func.ops.push(op_byte(Op::Br));
    func.flags.push(force_flags.unwrap_or_else(|| at(view.flags, guard_term)));
    func.tys.push(at(view.tys, guard_term));
    func.data.push(dst);
    func.attrs.push(at(view.attrs, guard_term));
    func.locs.push(at(view.locs, guard_term));
    idx
}

/// Collects compact back-edge arms `arm > header` (excluding `skip_a` and `skip_b`) ending in
/// `Jump(header, args)` with `args.len() == view.params(header).len()`.
fn collect_dup_arms<A: Allocator + Copy>(
    view: &View<'_>,
    header: usize,
    skip_a: usize,
    skip_b: usize,
    alloc: A,
) -> (Vec<bool, A>, usize) {
    let blocks = view.blocks();
    let mut dup_arm = filled(blocks, false, &alloc);
    let mut dup_count = 0usize;
    let header_params = view.params(Block(to_u32(header))).len();
    for b in (header + 1)..blocks {
        if dup_count >= MAX_DUPLICATED_ARMS {
            break;
        }
        if b == skip_a || b == skip_b {
            continue;
        }
        let b_u32 = to_u32(b);
        if view.block_kind(Block(b_u32)) != BlockKind::Normal {
            continue;
        }
        let range = view.block(Block(b_u32));
        let term = range.end - 1;
        if view.op(term) != Op::Jump {
            continue;
        }
        if term - range.start > MAX_ARM_INSTS {
            continue;
        }
        let jat = to_idx(at(view.data, term));
        if to_idx(at(view.extra, jat)) == header && to_idx(at(view.extra, jat + 1)) == header_params {
            set_at(&mut dup_arm, b, true);
            dup_count += 1;
        }
    }
    (dup_arm, dup_count)
}

/// Duplicates compact `Op::Switch` interpreter dispatch headers (both direct 1-block `Switch`
/// headers and classic 2-block `dispatch(Br) -> decode(Switch)` bounded loops) directly into the
/// tails of hot arms that jump back to them, enabling per-opcode indirect branch prediction.
pub fn tail_duplicate_dispatch<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<TailDupOutput<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 5 {
        return Ok(TailDupOutput { func: view.to_func_in(alloc), stats: TailDupStats::default() });
    }

    let mut inst_block = filled(len, 0u32, &alloc);
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(alloc));
    }
    for b in 0..blocks {
        let b_u32 = to_u32(b);
        for inst in view.block(Block(b_u32)) {
            set_at(&mut inst_block, inst, b_u32);
        }
        let term = view.terminator(Block(b_u32));
        each(&view, term, |item| {
            if let Item::Edge(edge) = item {
                let tb = to_idx(edge.block);
                if let Some(p) = preds.get_mut(tb)
                    && p.scan_for(&b_u32).is_none()
                {
                    p.push(b_u32);
                }
            }
        });
    }

    // Find an eligible direct 1-block `Op::Switch` dispatch or 2-block `guard(Br) -> decode(Switch)`
    // dispatch with at least 2 compact back-edge arms, collecting any header/decode defs directly
    // used in single-predecessor targets for promotion to block parameters.
    let mut plan: Option<DispatchPlan<A>> = None;
    for b in 1..blocks {
        let b_u32 = to_u32(b);
        if view.block_kind(Block(b_u32)) != BlockKind::Normal {
            continue;
        }
        let range = view.block(Block(b_u32));
        let params = view.params(Block(b_u32));
        let term = range.end - 1;
        let term_op = view.op(term);

        if term_op == Op::Switch {
            let body_len = term - params.end;
            if body_len > MAX_DISPATCH_INSTS {
                continue;
            }
            let sw_at = to_idx(at(view.data, term));
            let def_argc = to_idx(at(view.extra, sw_at + 2));
            let n_cases = to_idx(at(view.extra, sw_at + 3 + def_argc));
            if n_cases < MIN_SWITCH_CASES {
                continue;
            }
            if !(params.end..term).all(|i| is_duplicable_dispatch_op(view.op(i))) {
                continue;
            }

            let (dup_arm, dup_count) = collect_dup_arms(&view, b, blocks, blocks, alloc);
            if dup_count < 2 {
                continue;
            }

            let mut is_sw_target = filled(blocks, false, &alloc);
            each(&view, term, |item| {
                if let Item::Edge(edge) = item {
                    set_at(&mut is_sw_target, to_idx(edge.block), true);
                }
            });

            let mut promoted: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
            for _ in 0..blocks {
                promoted.push(Vec::new_in(alloc));
            }
            let mut escapes = false;
            for ob in 0..blocks {
                if ob == b {
                    continue;
                }
                for inst in view.block(Block(to_u32(ob))) {
                    let mut check_val = |v: u32| {
                        let def_b = at(&inst_block, to_idx(v));
                        if def_b == b_u32 {
                            if at(&is_sw_target, ob)
                                && preds.get(ob).is_some_and(|p| p.len() == 1 && p.first().copied() == Some(b_u32))
                            {
                                if let Some(prom) = promoted.get_mut(ob)
                                    && prom.scan_for(&v).is_none()
                                {
                                    prom.push(v);
                                }
                            } else {
                                escapes = true;
                            }
                        } else if to_idx(def_b) > ob {
                            escapes = true;
                        }
                    };
                    each(&view, inst, |item| match item {
                        Item::Value(v) => check_val(v),
                        Item::Edge(edge) => {
                            if let Some((start, count)) = edge.args {
                                for k in 0..count {
                                    check_val(at(view.extra, start + k));
                                }
                            }
                        }
                    });
                    if escapes {
                        break;
                    }
                }
                if escapes {
                    break;
                }
            }
            if escapes {
                continue;
            }
            // Ensure all outside values read by `disp` (including switch edge args!) come from entry block `0`.
            let mut uses_ok = true;
            for inst in params.end..range.end {
                let mut check_src = |v: u32| {
                    let vb = to_idx(at(&inst_block, to_idx(v)));
                    if vb != b && vb != 0 {
                        uses_ok = false;
                    }
                };
                each(&view, inst, |item| match item {
                    Item::Value(v) => check_src(v),
                    Item::Edge(edge) => {
                        if let Some((start, count)) = edge.args {
                            for k in 0..count {
                                check_src(at(view.extra, start + k));
                            }
                        }
                    }
                });
                if !uses_ok {
                    break;
                }
            }
            if !uses_ok {
                continue;
            }
            for prom in promoted.iter_mut() {
                prom.sort_unstable();
            }
            plan = Some(DispatchPlan {
                kind: DispatchKind::Direct { disp: b },
                promoted,
                dup_arm,
                dup_count,
            });
            break;
        }

        if term_op == Op::Br {
            let guard = b;
            let guard_u32 = b_u32;
            let guard_body_len = term - params.end;
            if guard_body_len > MAX_GUARD_INSTS {
                continue;
            }
            if !(params.end..term).all(|i| is_duplicable_dispatch_op(view.op(i))) {
                continue;
            }
            let hat = to_idx(at(view.data, term));
            let t_blk = at(view.extra, hat + 1);
            let t_argc = to_idx(at(view.extra, hat + 2));
            let e_at = hat + 3 + t_argc;
            let e_blk = at(view.extra, e_at);
            let e_argc = to_idx(at(view.extra, e_at + 1));
            if t_blk == e_blk {
                continue;
            }

            let candidates = [(t_blk, e_blk, t_argc, true), (e_blk, t_blk, e_argc, false)];
            let mut matched_bounded = None;
            for (dec_u32, exit_u32, dec_br_argc, decode_is_then) in candidates {
                let dec = to_idx(dec_u32);
                let exit_b = to_idx(exit_u32);
                if dec <= guard || exit_b == guard || exit_b == dec {
                    continue;
                }
                if view.block_kind(Block(dec_u32)) != BlockKind::Normal {
                    continue;
                }
                if preds.get(dec).map(Vec::as_slice) != Some(&[guard_u32]) {
                    continue;
                }
                let dec_params = view.params(Block(dec_u32));
                if dec_params.len() != dec_br_argc {
                    continue;
                }
                let dec_range = view.block(Block(dec_u32));
                let dec_term = dec_range.end - 1;
                if view.op(dec_term) != Op::Switch {
                    continue;
                }
                let dec_body_len = dec_term - dec_params.end;
                if dec_body_len > MAX_DISPATCH_INSTS || guard_body_len + dec_body_len > MAX_DISPATCH_INSTS {
                    continue;
                }
                if !(dec_params.end..dec_term).all(|i| is_duplicable_dispatch_op(view.op(i))) {
                    continue;
                }
                let sw_at = to_idx(at(view.data, dec_term));
                let def_argc = to_idx(at(view.extra, sw_at + 2));
                let n_cases = to_idx(at(view.extra, sw_at + 3 + def_argc));
                if n_cases < MIN_SWITCH_CASES {
                    continue;
                }

                let (dup_arm, dup_count) = collect_dup_arms(&view, guard, dec, exit_b, alloc);
                if dup_count < 2 {
                    continue;
                }

                let mut is_sw_target = filled(blocks, false, &alloc);
                each(&view, dec_term, |item| {
                    if let Item::Edge(edge) = item {
                        set_at(&mut is_sw_target, to_idx(edge.block), true);
                    }
                });

                let mut promoted: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
                for _ in 0..blocks {
                    promoted.push(Vec::new_in(alloc));
                }
                let mut escapes = false;
                for ob in 0..blocks {
                    if ob == guard || ob == dec {
                        continue;
                    }
                    for inst in view.block(Block(to_u32(ob))) {
                        let mut check_val = |v: u32| {
                            let def_b = at(&inst_block, to_idx(v));
                            if def_b == guard_u32 || def_b == dec_u32 {
                                let sw_arm_ok = at(&is_sw_target, ob)
                                    && preds.get(ob).is_some_and(|p| p.len() == 1 && p.first().copied() == Some(dec_u32));
                                let exit_ok = ob == exit_b
                                    && def_b == guard_u32
                                    && !at(&is_sw_target, ob)
                                    && preds.get(ob).is_some_and(|p| p.len() == 1 && p.first().copied() == Some(guard_u32));
                                if sw_arm_ok || exit_ok {
                                    if let Some(prom) = promoted.get_mut(ob)
                                        && prom.scan_for(&v).is_none()
                                    {
                                        prom.push(v);
                                    }
                                } else {
                                    escapes = true;
                                }
                            } else if to_idx(def_b) > ob {
                                escapes = true;
                            }
                        };
                        each(&view, inst, |item| match item {
                            Item::Value(v) => check_val(v),
                            Item::Edge(edge) => {
                                if let Some((start, count)) = edge.args {
                                    for k in 0..count {
                                        check_val(at(view.extra, start + k));
                                    }
                                }
                            }
                        });
                        if escapes {
                            break;
                        }
                    }
                    if escapes {
                        break;
                    }
                }
                if escapes {
                    continue;
                }

                // Ensure outside values read by `guard` come from entry block `0`, and outside
                // values read by `decode` come from entry block `0` or `guard`.
                let mut uses_ok = true;
                for inst in params.end..range.end {
                    let mut check_src = |v: u32| {
                        let vb = to_idx(at(&inst_block, to_idx(v)));
                        if vb != guard && vb != 0 {
                            uses_ok = false;
                        }
                    };
                    each(&view, inst, |item| match item {
                        Item::Value(v) => check_src(v),
                        Item::Edge(edge) => {
                            if let Some((start, count)) = edge.args {
                                for k in 0..count {
                                    check_src(at(view.extra, start + k));
                                }
                            }
                        }
                    });
                    if !uses_ok {
                        break;
                    }
                }
                if uses_ok {
                    for inst in dec_params.end..dec_range.end {
                        let mut check_src = |v: u32| {
                            let vb = to_idx(at(&inst_block, to_idx(v)));
                            if vb != dec && vb != guard && vb != 0 {
                                uses_ok = false;
                            }
                        };
                        each(&view, inst, |item| match item {
                            Item::Value(v) => check_src(v),
                            Item::Edge(edge) => {
                                if let Some((start, count)) = edge.args {
                                    for k in 0..count {
                                        check_src(at(view.extra, start + k));
                                    }
                                }
                            }
                        });
                        if !uses_ok {
                            break;
                        }
                    }
                }
                if !uses_ok {
                    continue;
                }
                // Canonical ascending order guarantees identical param order (and zero-move register
                // coalescing!) across all switch arms.
                for prom in promoted.iter_mut() {
                    prom.sort_unstable();
                }
                matched_bounded = Some(DispatchPlan {
                    kind: DispatchKind::Bounded { guard, decode: dec, decode_is_then },
                    promoted,
                    dup_arm,
                    dup_count,
                });
                break;
            }
            if matched_bounded.is_some() {
                plan = matched_bounded;
                break;
            }
        }
    }

    let Some(DispatchPlan { kind, promoted, dup_arm, dup_count }) = plan else {
        return Ok(TailDupOutput { func: view.to_func_in(alloc), stats: TailDupStats::default() });
    };

    // Assign new block numbers: for `Bounded`, place each arm's dedicated `decode_k` block
    // immediately after `arm_k` so live ranges across arms stay disjoint (zero-move coalescing)
    // and hot fallthrough stays contiguous in L1i!
    let is_bounded = matches!(kind, DispatchKind::Bounded { .. });
    let mut blk_map = filled(blocks, 0u32, &alloc);
    let mut dup_decode_blk = filled(blocks, NONE, &alloc);
    let mut next_blk = 0u32;
    for b in 0..blocks {
        set_at(&mut blk_map, b, next_blk);
        next_blk += 1;
        if is_bounded && at(&dup_arm, b) {
            set_at(&mut dup_decode_blk, b, next_blk);
            next_blk += 1;
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

    let mut val_map = filled(len, NONE, &alloc);
    let mut block_map = filled(len, NONE, &alloc);
    let mut local_map = filled(len, NONE, &alloc);

    let (entry_hdr, switch_blk) = match kind {
        DispatchKind::Direct { disp } => (disp, disp),
        DispatchKind::Bounded { guard, decode, .. } => (guard, decode),
    };
    let entry_hdr_u32 = to_u32(entry_hdr);
    let entry_hdr_params = view.params(Block(entry_hdr_u32));
    let entry_hdr_range = view.block(Block(entry_hdr_u32));
    let entry_hdr_term = entry_hdr_range.end - 1;
    let switch_blk_u32 = to_u32(switch_blk);
    let switch_blk_params = view.params(Block(switch_blk_u32));
    let switch_blk_range = view.block(Block(switch_blk_u32));
    let switch_blk_term = switch_blk_range.end - 1;

    for b in 0..blocks {
        let b_u32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        let range = view.block(Block(b_u32));
        let params_end = view.params(Block(b_u32)).end;

        if let DispatchKind::Bounded { guard, decode_is_then, .. } = kind
            && b == guard
        {
            for inst in range.start..entry_hdr_term {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            let mapped = copy_br_for_guard(
                &view,
                entry_hdr_term,
                &val_map,
                &blk_map,
                &promoted,
                decode_is_then,
                None,
                None,
                &mut func,
            );
            set_at(&mut val_map, entry_hdr_term, mapped);
            continue;
        }

        if b == switch_blk {
            for inst in range.start..switch_blk_term {
                let mapped = copy_remapped(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            let mapped = copy_switch_with_promoted(&view, switch_blk_term, &val_map, &blk_map, &promoted, &mut func);
            set_at(&mut val_map, switch_blk_term, mapped);
            continue;
        }

        // Copy `val_map` into `block_map`, materializing any promoted `Op::Param`s for `b`!
        for (dst, &src) in block_map.iter_mut().zip(val_map.iter()) {
            *dst = src;
        }
        for inst in range.start..params_end {
            let mapped = copy_remapped(&view, inst, &block_map, &blk_map, &mut func);
            set_at(&mut val_map, inst, mapped);
            set_at(&mut block_map, inst, mapped);
        }
        if let Some(prom) = promoted.get(b) {
            for &pv in prom.iter() {
                let new_p = to_u32(func.ops.len());
                func.ops.push(op_byte(Op::Param));
                func.flags.push(0);
                func.tys.push(at(view.tys, to_idx(pv)));
                func.data.push(0);
                func.attrs.push(0);
                func.locs.push(0);
                set_at(&mut block_map, to_idx(pv), new_p);
            }
        }

        if at(&dup_arm, b) {
            let term = range.end - 1;
            for inst in params_end..term {
                let mapped = copy_remapped(&view, inst, &block_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
                set_at(&mut block_map, inst, mapped);
            }
            // Prepare `local_map`: all entry-block (0) defs + header params mapped to `Jump` args!
            for (dst, &src) in local_map.iter_mut().zip(block_map.iter()) {
                *dst = src;
            }
            let jat = to_idx(at(view.data, term));
            for k in 0..entry_hdr_params.len() {
                let arg = at(&block_map, to_idx(at(view.extra, jat + 2 + k)));
                set_at(&mut local_map, entry_hdr_params.start + k, arg);
            }
            match kind {
                DispatchKind::Direct { .. } => {
                    // Splice `disp`'s body + promoted `Op::Switch` directly into the tail of `arm`!
                    for d_inst in entry_hdr_params.end..entry_hdr_term {
                        let mapped = copy_remapped(&view, d_inst, &local_map, &blk_map, &mut func);
                        set_at(&mut local_map, d_inst, mapped);
                    }
                    let mapped = copy_switch_with_promoted(
                        &view,
                        entry_hdr_term,
                        &local_map,
                        &blk_map,
                        &promoted,
                        &mut func,
                    );
                    set_at(&mut local_map, entry_hdr_term, mapped);
                }
                DispatchKind::Bounded { decode, decode_is_then, .. } => {
                    // 1. Splice `guard`'s loop-exit test + `Op::Br(cond, decode_k(), exit(args...))` into `arm`.
                    for g_inst in entry_hdr_params.end..entry_hdr_term {
                        let mapped = copy_remapped(&view, g_inst, &local_map, &blk_map, &mut func);
                        set_at(&mut local_map, g_inst, mapped);
                    }
                    let dup_dec = at(&dup_decode_blk, b);
                    let br_flags: Option<u8> = None;
                    let br_mapped = copy_br_for_guard(
                        &view,
                        entry_hdr_term,
                        &local_map,
                        &blk_map,
                        &promoted,
                        decode_is_then,
                        Some(dup_dec),
                        br_flags,
                        &mut func,
                    );
                    set_at(&mut local_map, entry_hdr_term, br_mapped);

                    // 2. Emit dedicated per-arm `decode_k` block immediately after `arm` with its own
                    //    opcode load, `pc + 1`, and dedicated `Op::Switch` indirect branch!
                    func.block_starts.push(to_u32(func.ops.len()));
                    func.block_kinds.push(at(view.block_kinds, decode));
                    if !switch_blk_params.is_empty() {
                        let hat = to_idx(at(view.data, entry_hdr_term));
                        let t_argc = to_idx(at(view.extra, hat + 2));
                        let dec_args_at = if decode_is_then { hat + 3 } else { hat + 3 + t_argc + 2 };
                        for k in 0..switch_blk_params.len() {
                            let arg = at(&local_map, to_idx(at(view.extra, dec_args_at + k)));
                            set_at(&mut local_map, switch_blk_params.start + k, arg);
                        }
                    }
                    for d_inst in switch_blk_params.end..switch_blk_term {
                        let mapped = copy_remapped(&view, d_inst, &local_map, &blk_map, &mut func);
                        set_at(&mut local_map, d_inst, mapped);
                    }
                    let sw_mapped = copy_switch_with_promoted(
                        &view,
                        switch_blk_term,
                        &local_map,
                        &blk_map,
                        &promoted,
                        &mut func,
                    );
                    set_at(&mut local_map, switch_blk_term, sw_mapped);
                }
            }
        } else {
            for inst in params_end..range.end {
                let mapped = copy_remapped(&view, inst, &block_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
                set_at(&mut block_map, inst, mapped);
            }
        }
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(TailDupOutput {
        func,
        stats: TailDupStats {
            dispatches_found: 1,
            arms_duplicated: to_u32(dup_count),
        },
    })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;
    use super::*;
    use crate::{
        Flags, IntCond,
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        eval::{self, Outcome, Val},
        func::Conv,
    };

    #[test]
    fn tail_duplicate_dispatch_duplicates_switch_into_vm_arms_and_matches_eval() {
        // Mini 4-opcode state machine (opcode = state & 3):
        //   0: acc += 3; state = 1
        //   1: acc ^= 7; state = 2
        //   2: acc += 11; state = 3
        //   3: acc += 1; state = 0 (until steps == 0 -> halt!)
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, disp, op0, op1, op2, op3, halt] = [(); 7].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let steps = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        let k1 = b.iconst(Ty::I64, 1);
        let k3 = b.iconst(Ty::I64, 3);
        let k4 = b.iconst(Ty::I64, 4);
        let k7 = b.iconst(Ty::I64, 7);
        let k11 = b.iconst(Ty::I64, 11);
        b.jump((disp, &[steps, z, z]));

        b.begin(disp);
        let rem = b.param(Ty::I64);
        let st = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let is_z = b.icmp(crate::IntCond::Eq, rem, z);
        let code = b.select(Flags::NONE, is_z, k4, st);
        b.switch(
            code,
            (halt, &[acc]),
            &[
                (0, (op0, &[rem, acc])),
                (1, (op1, &[rem, acc])),
                (2, (op2, &[rem, acc])),
                (3, (op3, &[rem, acc])),
            ],
        );

        let arms = [(op0, Op::Add, k3, 1u128), (op1, Op::Xor, k7, 2), (op2, Op::Add, k11, 3), (op3, Op::Add, k1, 0)];
        for (blk, alu, kv, next_st) in arms {
            b.begin(blk);
            let r = b.param(Ty::I64);
            let a = b.param(Ty::I64);
            let r2 = b.binary(Op::Sub, Flags::NONE, r, k1);
            let a2 = b.binary(alu, Flags::NONE, a, kv);
            let ns = b.iconst(Ty::I64, next_st);
            b.jump((disp, &[r2, ns, a2]));
        }
        b.begin(halt);
        let out = b.param(Ty::I64);
        b.ret(&[out]);
        let func = b.finish_checked().unwrap();

        let dup = tail_duplicate_dispatch(func.view(), TestHeap).unwrap();
        assert_eq!(dup.stats.dispatches_found, 1);
        assert_eq!(dup.stats.arms_duplicated, 4);
        let sw_count = (0..dup.func.view().len()).filter(|&i| dup.func.view().op(i) == Op::Switch).count();
        assert_eq!(sw_count, 5, "expected 1 initial + 4 per-arm Switch dispatches!");

        for &n in &[0u128, 1, 2, 3, 4, 15, 40] {
            let Outcome::Returned(a) = eval::run_in(func.view(), &[Val::Int(n)], 50_000, TestHeap).unwrap() else { panic!() };
            let Outcome::Returned(c) = eval::run_in(dup.func.view(), &[Val::Int(n)], 50_000, TestHeap).unwrap() else { panic!() };
            assert_eq!(a, c, "taildup mismatch at n={n}");
        }
    }

    fn build_bounded_bytecode_vm(decode_in_then: bool) -> Func<TestHeap> {
        // Exact 2-block bounded interpreter loop shape (`dispatch -> Br -> decode(Switch) -> arms -> dispatch`)
        // matching `sota_vs_llvm::lir_bytecode_vm`, reading opcodes from an embedded 32-byte datum.
        let opcodes: [u8; 32] = [
            0, 2, 5, 1, 4, 3, 6, 7, 2, 0, 4, 6, 1, 5, 7, 3,
            3, 6, 0, 4, 2, 7, 5, 1, 6, 1, 3, 0, 5, 2, 4, 7,
        ];
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let ops_data = b.data(&opcodes, 0, &[]);

        let entry = b.create_block(BlockKind::Normal);
        let dispatch = b.create_block(BlockKind::Normal);
        let decode = b.create_block(BlockKind::Normal);
        let arms = [(); 8].map(|_| b.create_block(BlockKind::Normal));
        let exit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let n = b.param(Ty::I64);
        let ops = b.data_addr(ops_data);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        b.jump((dispatch, &[zero, one]));

        b.begin(dispatch);
        let pc = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        if decode_in_then {
            let keep = b.icmp(IntCond::Ult, pc, n);
            b.br(Flags::NONE, keep, (decode, &[]), (exit, &[acc]));
        } else {
            let done = b.icmp(IntCond::Uge, pc, n);
            b.br(Flags::NONE, done, (exit, &[acc]), (decode, &[]));
        }

        b.begin(decode);
        let p = b.ptr_add(Flags::NONE, ops, pc);
        let op = b.load(Ty::I8, Flags::NONE, p, 0, MemAttrs::aligned(0));
        let pc2 = b.binary(Op::Add, Flags::NONE, pc, one);
        b.switch(
            op,
            (arms[7], &[]),
            &[
                (0, (arms[0], &[])),
                (1, (arms[1], &[])),
                (2, (arms[2], &[])),
                (3, (arms[3], &[])),
                (4, (arms[4], &[])),
                (5, (arms[5], &[])),
                (6, (arms[6], &[])),
            ],
        );

        b.begin(arms[0]);
        let c3 = b.iconst(Ty::I64, 3);
        let a0 = b.binary(Op::Add, Flags::NONE, acc, c3);
        b.jump((dispatch, &[pc2, a0]));

        b.begin(arms[1]);
        let a1 = b.binary(Op::Sub, Flags::NONE, acc, one);
        b.jump((dispatch, &[pc2, a1]));

        b.begin(arms[2]);
        let c5 = b.iconst(Ty::I64, 5);
        let a2 = b.binary(Op::Mul, Flags::NONE, acc, c5);
        b.jump((dispatch, &[pc2, a2]));

        b.begin(arms[3]);
        let cx = b.iconst(Ty::I64, 0x55aa);
        let a3 = b.binary(Op::Xor, Flags::NONE, acc, cx);
        b.jump((dispatch, &[pc2, a3]));

        b.begin(arms[4]);
        let s3 = b.iconst(Ty::I64, 3);
        let sh = b.binary(Op::Ashr, Flags::NONE, acc, s3);
        let a4 = b.binary(Op::Add, Flags::NONE, acc, sh);
        b.jump((dispatch, &[pc2, a4]));

        b.begin(arms[5]);
        let sh1 = b.binary(Op::Shl, Flags::NONE, acc, one);
        let c7 = b.iconst(Ty::I64, 7);
        let a5 = b.binary(Op::Xor, Flags::NONE, sh1, c7);
        b.jump((dispatch, &[pc2, a5]));

        b.begin(arms[6]);
        let c17 = b.iconst(Ty::I64, 17);
        let a6 = b.binary(Op::Add, Flags::NONE, acc, c17);
        b.jump((dispatch, &[pc2, a6]));

        b.begin(arms[7]);
        let a7 = b.binary(Op::Xor, Flags::NONE, acc, one);
        b.jump((dispatch, &[pc2, a7]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        b.finish_checked().unwrap()
    }

    #[test]
    fn tail_duplicate_dispatch_duplicates_bounded_2block_bytecode_vm_and_matches_eval() {
        for decode_in_then in [true, false] {
            let func = build_bounded_bytecode_vm(decode_in_then);
            let dup = tail_duplicate_dispatch(func.view(), TestHeap).unwrap();
            assert_eq!(dup.stats.dispatches_found, 1);
            assert_eq!(dup.stats.arms_duplicated, 8, "expected all 8 bytecode arms duplicated (decode_in_then={decode_in_then})");
            let sw_count = (0..dup.func.view().len()).filter(|&i| dup.func.view().op(i) == Op::Switch).count();
            assert_eq!(sw_count, 9, "expected 1 initial + 8 per-arm Switch dispatches!");
            verify(dup.func.view()).unwrap();

            for &n in &[0u128, 1, 2, 3, 7, 16, 32] {
                let Outcome::Returned(a) = eval::run_in(func.view(), &[Val::Int(n)], 50_000, TestHeap).unwrap() else { panic!() };
                let Outcome::Returned(c) = eval::run_in(dup.func.view(), &[Val::Int(n)], 50_000, TestHeap).unwrap() else { panic!() };
                assert_eq!(a, c, "bounded 2-block taildup mismatch at n={n}, decode_in_then={decode_in_then}");
            }
        }
    }
}
