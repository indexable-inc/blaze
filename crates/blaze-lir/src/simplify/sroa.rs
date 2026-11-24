//! Scalar replacement of slots that do not escape (backend-next.md, D100 point 1): a stack slot
//! whose address is only loaded from and stored to, at offsets known from the text, becomes one
//! value per field it is accessed as, so the Fast tier keeps an `Option`, a range or an
//! iterator's state in registers once the trivial inliner has spliced its methods in.
//!
//! # What is replaced
//!
//! A slot is replaced when every use of every pointer into it is
//!
//! - the address of a `load` without flags whose result is a scalar,
//! - the address of a `store` without flags whose stored value is a scalar, or
//! - the base of a `ptr_add` by an `iconst`, whose result is used only so in turn,
//!
//! every use is in a block reached from the entry, the accesses lie within the slot, and two
//! accesses either name the same bytes with the same type (one field) or bytes apart. Anything
//! else (the address passed to a call, stored, compared, passed to a block, offset by a value, a
//! `volatile` access, two types over one byte) keeps the slot in memory, unchanged: that is the
//! analysis's answer, not a refusal. A field live into a block that only an unwind, return,
//! label or suspend edge reaches would need a parameter such an edge cannot carry: its slot
//! stays too.
//!
//! # How
//!
//! Cytron's construction on block parameters, pruned by liveness: one walk over the blocks in
//! reverse postorder finds the slots, their accesses and the fields; each field's parameters go
//! at the iterated dominance frontier of the blocks that store it (or `slot_live`/`slot_dead`
//! it), where it is live; one walk down the dominator tree gives every load the value that
//! reaches it and every edge its new arguments; one walk in layout order writes the output, its
//! values renumbered and every operand fixed up at the end (a use may come before its value in
//! layout order). A field read before anything wrote it (bytes uninitialised, or after
//! `slot_live`/`slot_dead`) is undefined behaviour in the input; the output reads `iconst 0` for
//! an integer field there and `uninit` for any other, both refinements.
//!
//! Owed: a replaced named local's debug row (its `variables` row goes with its slot, as the
//! coroutine pass drops an unkept slot's).

use core::{alloc::Allocator, convert::Infallible};

use blaze_own::Vec;

use super::SimplifyError;
use crate::{
    Block, Flags, Func, Op, Ty, View,
    func::NONE,
    op::{Field, Layout, Word, walk},
    verify,
};

/// A planted bug, for the simulation's control. Only test builds act on one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// A store to a field that already holds a stored value is not seen: later loads read
    /// the earlier, stale value.
    KeepStaleStore,
}

/// Replaces every slot of `view` whose address does not escape by values, every buffer in
/// `alloc`. `view` must be verified (`crate::verify`): the stage's inliner hands on a verified
/// function, and the pass relies on its dominance. The output is verified in debug builds.
pub fn scalar_replace<A: Allocator + Copy>(view: View<'_>, alloc: A) -> Result<Func<A>, SimplifyError> {
    scalar_replace_planted(view, Plant::None, alloc)
}

/// [`scalar_replace`] with a planted bug, which only test builds act on.
pub fn scalar_replace_planted<A: Allocator + Copy>(
    view: View<'_>,
    plant: Plant,
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    if view.slots.is_empty() {
        if cfg!(debug_assertions) {
            verify(view).map_err(SimplifyError::Verify)?;
        }
        return Ok(view.to_func_in(alloc));
    }
    let promoted = promote_slot_addr_phis(view, alloc)?;
    let work_view = match promoted.as_ref() {
        Some(func) => func.view(),
        None => view,
    };
    let cfg = Cfg::of(&work_view, &alloc)?;
    let mut found = find(&work_view, &cfg, &alloc)?;
    if found.kept.iter().all(|&k| k) {
        if cfg!(debug_assertions) {
            verify(work_view).map_err(SimplifyError::Verify)?;
        }
        return Ok(match promoted {
            Some(func) => func,
            None => view.to_func_in(alloc),
        });
    }
    let phis = place(&work_view, &cfg, &mut found, &alloc);
    if found.kept.iter().all(|&k| k) {
        if cfg!(debug_assertions) {
            verify(work_view).map_err(SimplifyError::Verify)?;
        }
        return Ok(match promoted {
            Some(func) => func,
            None => view.to_func_in(alloc),
        });
    }
    // Input values and new parameters share one numbering while the output is written.
    if work_view.len() + phis.field.len() >= to_idx(NONE) {
        return Err(SimplifyError::Refused { inst: 0, what: "more values than LIR numbers" });
    }
    let renamed = rename(&work_view, &cfg, &found, &phis, plant, &alloc)?;
    emit(&work_view, &found, &phis, &renamed, alloc)
}

fn filled<T: Clone, A: Allocator + Copy>(len: usize, item: T, alloc: &A) -> Vec<T, A> {
    Vec::from_elem_in(item, len, *alloc)
}

fn layout_error(inst: usize) -> SimplifyError {
    SimplifyError::Refused { inst, what: "operands that do not fit their layout" }
}

/// Calls `f(k, value)` with the `k`-th value operand of instruction `at`, in layout order
/// (a target's arguments and a list's values included).
fn each_value(view: &View<'_>, at: usize, mut f: impl FnMut(usize, u32)) -> Result<(), SimplifyError> {
    match view.op(at).info().layout {
        Layout::None => Ok(()),
        Layout::Data(Field::Value) => {
            f(0, view.data[at]);
            Ok(())
        }
        Layout::Data(_) => Ok(()),
        Layout::Extra(fields) => {
            let words = &view.extra[view.data[at] as usize..];
            let mut k = 0;
            let mut visit = |word: Word| -> Result<(), Infallible> {
                if let Word::Value(value) = word {
                    f(k, value);
                    k += 1;
                }
                Ok(())
            };
            walk(words, fields, Ty(view.tys[at]), view.tys, &mut visit).map(|_| ()).map_err(|_| layout_error(at))
        }
    }
}

/// Calls `f(to, plain)` with every edge from the terminator `at`, in operand order: `plain`
/// when the edge is a target that carries arguments (`jump`, `br`, `switch`), not when it is
/// an `invoke`'s return or unwind edge, an `asm_br`'s label or a `suspend`'s.
fn each_edge(view: &View<'_>, at: usize, mut f: impl FnMut(u32, bool)) -> Result<(), SimplifyError> {
    let op = view.op(at);
    let plain = matches!(op, Op::Jump | Op::Br | Op::Switch);
    let Layout::Extra(fields) = op.info().layout else { return Ok(()) };
    let words = &view.extra[view.data[at] as usize..];
    let mut visit = |word: Word| -> Result<(), Infallible> {
        if let Word::Block(block) = word {
            f(block, plain);
        }
        Ok(())
    };
    walk(words, fields, Ty(view.tys[at]), view.tys, &mut visit).map(|_| ()).map_err(|_| layout_error(at))
}

/// The slot a pointer value points into: through `ptr_add`s to a `slot_addr`, if it is one.
fn root(view: &View<'_>, mut value: u32) -> Option<u32> {
    // Each step goes to an operand; a verified function has no cycle of `ptr_add`s outside
    // a block parameter, so as many steps as instructions is a bound never reached.
    for _ in 0..view.len() {
        match view.op(value as usize) {
            Op::SlotAddr => return Some(view.data[value as usize]),
            Op::PtrAdd => value = view.operands(value as usize).value().0,
            _ => return None,
        }
    }
    None
}

/// `value`'s constant, sign-extended from its width, if it is an integer constant.
fn constant(view: &View<'_>, value: u32) -> Option<i128> {
    let at = value as usize;
    let ty = Ty(view.tys[at]);
    if !(ty.is_scalar() && ty.is_int()) {
        return None;
    }
    let width = ty.bits(view.head.ptr_bits);
    let bits = match view.op(at) {
        Op::Iconst => {
            let word = view.data[at];
            match width {
                ..=32 => u128::from(word),
                _ => (word as i32 as i128) as u128,
            }
        }
        Op::IconstWide => {
            view.operands(at).constant().iter().rev().fold(0u128, |it, &word| (it << 32) | u128::from(word))
        }
        _ => return None,
    };
    let shift = 128 - width;
    Some(((bits << shift) as i128) >> shift)
}

/// The bytes a scalar of `ty` takes.
fn bytes(ty: Ty, ptr_bits: u32) -> i128 {
    i128::from(ty.bits(ptr_bits).div_ceil(8))
}

/// The function's block graph: edges both ways, reverse postorder, dominators and the
/// dominator tree's children.
struct Cfg<A: Allocator> {
    pred_start: Vec<u32, A>,
    preds: Vec<u32, A>,
    /// Per block, whether every edge into it carries arguments.
    plain_only: Vec<bool, A>,
    /// The blocks the entry reaches, in reverse postorder.
    rpo: Vec<u32, A>,
    /// Per block its immediate dominator, the entry's itself; [`NONE`] for an unreachable one.
    idom: Vec<u32, A>,
    child_start: Vec<u32, A>,
    children: Vec<u32, A>,
}

impl<A: Allocator + Copy> Cfg<A> {
    fn of(view: &View<'_>, alloc: &A) -> Result<Cfg<A>, SimplifyError> {
        let blocks = view.blocks();
        let mut succ_start = Vec::with_capacity_in(blocks + 1, *alloc);
        let mut succs = Vec::new_in(*alloc);
        let mut plain_only = filled(blocks, true, alloc);
        // (to, from), one per edge.
        let mut edges: Vec<(u32, u32), A> = Vec::new_in(*alloc);
        for b in 0..blocks {
            succ_start.push(succs.len() as u32);
            each_edge(view, view.terminator(Block(b as u32)), |to, plain| {
                succs.push(to);
                edges.push((to, b as u32));
                if !plain {
                    plain_only[to as usize] = false;
                }
            })?;
        }
        succ_start.push(succs.len() as u32);
        edges.sort_unstable();
        let mut pred_start = filled(blocks + 1, 0u32, alloc);
        for &(to, _) in edges.iter() {
            pred_start[to as usize + 1] += 1;
        }
        for b in 0..blocks {
            pred_start[b + 1] += pred_start[b];
        }
        let mut preds = Vec::with_capacity_in(edges.len(), *alloc);
        for &(_, from) in edges.iter() {
            preds.push(from);
        }
        // Reverse postorder from the entry, by an explicit stack.
        let mut post = Vec::with_capacity_in(blocks, *alloc);
        let mut seen = filled(blocks, false, alloc);
        let mut stack: Vec<(u32, u32), A> = Vec::new_in(*alloc);
        if blocks > 0 {
            seen[0] = true;
            stack.push((0, succ_start[0]));
        }
        while let Some(top) = stack.last_mut() {
            let (b, next) = *top;
            if next < succ_start[b as usize + 1] {
                top.1 += 1;
                let s = succs[next as usize];
                if !seen[s as usize] {
                    seen[s as usize] = true;
                    stack.push((s, succ_start[s as usize]));
                }
            } else {
                post.push(b);
                stack.pop();
            }
        }
        let mut rpo = Vec::with_capacity_in(post.len(), *alloc);
        rpo.extend_from_slice(&post);
        rpo.reverse();
        let mut order = filled(blocks, NONE, alloc);
        for (i, &b) in rpo.iter().enumerate() {
            order[b as usize] = i as u32;
        }
        // Cooper, Harvey and Kennedy's iteration: it settles in at most as many rounds as the
        // graph is deep, so twice the blocks is a bound it never reaches.
        let mut idom = filled(blocks, NONE, alloc);
        if blocks > 0 {
            idom[0] = 0;
        }
        let mut settled = false;
        for _ in 0..2 * blocks + 2 {
            let mut changed = false;
            for &b in rpo.iter().skip(1) {
                let mut new = NONE;
                for &p in &preds[pred_start[b as usize] as usize..pred_start[b as usize + 1] as usize] {
                    if idom[p as usize] == NONE {
                        continue;
                    }
                    new = match new {
                        NONE => p,
                        _ => intersect(&idom, &order, p, new),
                    };
                }
                if new != idom[b as usize] {
                    idom[b as usize] = new;
                    changed = true;
                }
            }
            if !changed {
                settled = true;
                break;
            }
        }
        if !settled {
            return Err(SimplifyError::Refused { inst: 0, what: "a block graph whose dominators did not settle" });
        }
        // The dominator tree's children, by block.
        let mut tree: Vec<(u32, u32), A> = Vec::new_in(*alloc);
        for &b in rpo.iter().skip(1) {
            tree.push((idom[b as usize], b));
        }
        tree.sort_unstable();
        let mut child_start = filled(blocks + 1, 0u32, alloc);
        for &(parent, _) in tree.iter() {
            child_start[parent as usize + 1] += 1;
        }
        for b in 0..blocks {
            child_start[b + 1] += child_start[b];
        }
        let mut children = Vec::with_capacity_in(tree.len(), *alloc);
        for &(_, child) in tree.iter() {
            children.push(child);
        }
        Ok(Cfg { pred_start, preds, plain_only, rpo, idom, child_start, children })
    }

    fn reachable(&self, b: u32) -> bool {
        self.idom[b as usize] != NONE
    }

    fn preds_of(&self, b: u32) -> &[u32] {
        &self.preds[self.pred_start[b as usize] as usize..self.pred_start[b as usize + 1] as usize]
    }
}

fn intersect(idom: &[u32], order: &[u32], mut a: u32, mut b: u32) -> u32 {
    // Each step moves one finger up the tree; the bound is the tree's size twice.
    for _ in 0..2 * idom.len() + 2 {
        if a == b {
            return a;
        }
        if order[a as usize] > order[b as usize] {
            a = idom[a as usize];
        } else {
            b = idom[b as usize];
        }
    }
    a
}

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn at_u32(s: &[u32], i: usize) -> u32 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_none(s: &[u32], i: usize) -> u32 {
    s.get(i).copied().unwrap_or(NONE)
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
fn at_bool(s: &[bool], i: usize) -> bool {
    s.get(i).copied().unwrap_or(false)
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

/// The pointer-sized scalar integer type (`Ty::I64` on 64-bit, `Ty::I32` on 32-bit).
#[inline]
fn ptr_int_ty(ptr_bits: u32) -> Option<Ty> {
    Ty::int(ptr_bits)
}

const MAX_SLOT_PHI_ROUNDS: usize = 4;
const MAX_SLOT_PHI_FIELDS: usize = 8;
const MAX_PTR_CHAIN_STEPS: usize = 64;

/// Metadata for one promoted read-only `SlotAddr` block parameter in a round.
#[derive(Clone, Copy, Debug)]
struct PlanMeta {
    /// Index of the removed `ptr_phi` parameter within its block's parameter list.
    param_idx: usize,
    /// Instruction index of the removed `ptr_phi` parameter.
    param_inst: u32,
    /// First field index in `plan_fields`.
    field_start: u32,
    /// Number of fields in `plan_fields`.
    field_len: u32,
}

/// Resolves `val` through constant `Op::PtrAdd` steps to `(slot_id, total_byte_offset)`
/// rooted at `Op::SlotAddr(slot_id)`. Returns `None` if not a constant-offset slot pointer.
fn resolve_slot_ptr(view: &View<'_>, mut val: u32) -> Option<(u32, i128)> {
    let mut off: i128 = 0;
    for _ in 0..MAX_PTR_CHAIN_STEPS {
        let at = to_idx(val);
        if at >= view.len() {
            return None;
        }
        match view.op(at) {
            Op::SlotAddr => return Some((at_u32(view.data, at), off)),
            Op::PtrAdd => {
                let extra_at = to_idx(at_u32(view.data, at));
                let base = view.extra.get(extra_at).copied()?;
                let by_val = view.extra.get(extra_at + 1).copied()?;
                let by = constant(view, by_val)?;
                off = off.checked_add(by)?;
                val = base;
            }
            _ => return None,
        }
    }
    None
}

/// Calls `f(target_block, args_extra_start, args_len)` for every plain target
/// (`Jump`, `Br`, `Switch`) of terminator `at`, in exact operand order.
fn each_plain_target(
    view: &View<'_>,
    at: usize,
    mut f: impl FnMut(u32, usize, usize),
) -> Result<(), SimplifyError> {
    let op = view.op(at);
    if !matches!(op, Op::Jump | Op::Br | Op::Switch) {
        return Ok(());
    }
    let mut cursor = to_idx(at_u32(view.data, at));
    let take_target = |cursor: &mut usize, f: &mut dyn FnMut(u32, usize, usize)| -> Result<(), SimplifyError> {
        let block = view.extra.get(*cursor).copied().ok_or_else(|| layout_error(at))?;
        let count = to_idx(view.extra.get(*cursor + 1).copied().ok_or_else(|| layout_error(at))?);
        let args_start = *cursor + 2;
        let args_end = args_start.checked_add(count).ok_or_else(|| layout_error(at))?;
        if args_end > view.extra.len() {
            return Err(layout_error(at));
        }
        f(block, args_start, count);
        *cursor = args_end;
        Ok(())
    };
    match op {
        Op::Jump => take_target(&mut cursor, &mut f)?,
        Op::Br => {
            cursor += 1;
            take_target(&mut cursor, &mut f)?;
            take_target(&mut cursor, &mut f)?;
        }
        Op::Switch => {
            let scrutinee = view.extra.get(cursor).copied().ok_or_else(|| layout_error(at))?;
            let width = to_idx(Ty(at_u16(view.tys, to_idx(scrutinee))).const_words());
            cursor += 1;
            take_target(&mut cursor, &mut f)?;
            let ncases = to_idx(view.extra.get(cursor).copied().ok_or_else(|| layout_error(at))?);
            cursor += 1;
            for _ in 0..ncases {
                cursor = cursor.checked_add(width).ok_or_else(|| layout_error(at))?;
                take_target(&mut cursor, &mut f)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Marks every block in `cfg`'s dominator subtree rooted at `root_block` as `true` in `out`.
fn mark_dom_subtree<A: Allocator + Copy>(
    cfg: &Cfg<A>,
    root_block: u32,
    out: &mut [bool],
    stack: &mut Vec<u32, A>,
) {
    out.fill(false);
    stack.clear();
    set_at(out, to_idx(root_block), true);
    stack.push(root_block);
    while let Some(b) = stack.pop() {
        let bi = to_idx(b);
        let start = to_idx(at_u32(&cfg.child_start, bi));
        let end = to_idx(at_u32(&cfg.child_start, bi + 1));
        if let Some(children) = cfg.children.get(start..end) {
            for &c in children {
                if !at_bool(out, to_idx(c)) {
                    set_at(out, to_idx(c), true);
                    stack.push(c);
                }
            }
        }
    }
}

/// Whether reachable block `b` is a landing block or dominated by one.
fn dom_by_landing<A: Allocator>(view: &View<'_>, cfg: &Cfg<A>, mut b: u32) -> bool {
    let limit = view.blocks();
    for _ in 0..limit {
        if view.block_kind(Block(b)).is_landing() {
            return true;
        }
        if b == 0 {
            return false;
        }
        let next = at_none(&cfg.idom, to_idx(b));
        if next == NONE || next == b {
            return false;
        }
        b = next;
    }
    true
}

/// Iterative pre-pass (up to [`MAX_SLOT_PHI_ROUNDS`]) that promotes read-only `Ty::PTR` block
/// parameters receiving `SlotAddr` / constant-`PtrAdd(SlotAddr)` arguments into direct scalar
/// field parameters loaded at each predecessor edge, unblocking SROA on the incoming slots.
fn promote_slot_addr_phis<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<Option<Func<A>>, SimplifyError> {
    let mut current: Option<Func<A>> = None;
    for _ in 0..MAX_SLOT_PHI_ROUNDS {
        let cur_view = match current.as_ref() {
            Some(func) => func.view(),
            None => view,
        };
        let Some(next) = promote_slot_addr_phis_round(&cur_view, alloc)? else {
            break;
        };
        current = Some(next);
    }
    Ok(current)
}

fn promote_slot_addr_phis_round<A: Allocator + Copy>(
    view: &View<'_>,
    alloc: A,
) -> Result<Option<Func<A>>, SimplifyError> {
    let blocks = view.blocks();
    let n = view.len();
    let nslots = view.slots.len();
    if blocks <= 1 || nslots == 0 {
        return Ok(None);
    }
    let mut any_ptr_param = false;
    for b in 1..blocks {
        let params = view.params(Block(to_u32(b)));
        for at in params.start..params.end {
            if Ty(at_u16(view.tys, at)) == Ty::PTR {
                any_ptr_param = true;
                break;
            }
        }
        if any_ptr_param {
            break;
        }
    }
    if !any_ptr_param {
        return Ok(None);
    }

    let ptr_bits = view.head.ptr_bits;
    let ptr_int = ptr_int_ty(ptr_bits);
    let cfg = Cfg::of(view, &alloc)?;

    // Track `(slot, offset)` for every `SlotAddr` and constant-offset `PtrAdd` in RPO.
    let mut ptr_slot = filled(n, NONE, &alloc);
    let mut ptr_off = filled(n, 0i128, &alloc);
    for &b in cfg.rpo.iter() {
        for at in view.block(Block(b)) {
            match view.op(at) {
                Op::SlotAddr => {
                    set_at(&mut ptr_slot, at, at_u32(view.data, at));
                    set_at(&mut ptr_off, at, 0);
                }
                Op::PtrAdd => {
                    let extra_at = to_idx(at_u32(view.data, at));
                    let base = at_u32(view.extra, extra_at);
                    let by_val = at_u32(view.extra, extra_at + 1);
                    let s = at_none(&ptr_slot, to_idx(base));
                    if s != NONE {
                        if let Some(delta) = constant(view, by_val) {
                            let base_off = ptr_off.get(to_idx(base)).copied().unwrap_or(0);
                            if let Some(new_off) = base_off.checked_add(delta) {
                                set_at(&mut ptr_slot, at, s);
                                set_at(&mut ptr_off, at, new_off);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Pre-scan slot escape / copy / zeroset facts for validating `Memcpy`/`Memmove` partners.
    let mut slot_escapes_any = filled(nslots, false, &alloc);
    let mut slot_copy_count = filled(nslots, 0u32, &alloc);
    let mut slot_has_zeroset = filled(nslots, false, &alloc);
    for &b in cfg.rpo.iter() {
        for at in view.block(Block(b)) {
            let op = view.op(at);
            let non_volatile = !view.flags(at).contains(Flags::VOLATILE);
            each_value(view, at, |k, v| {
                let s = at_none(&ptr_slot, to_idx(v));
                if s == NONE {
                    return;
                }
                let base = ptr_off.get(to_idx(v)).copied().unwrap_or(0);
                let slot_size = view.slots.get(to_idx(s)).map_or(0, |sr| i128::from(sr.size));
                let ok = match (op, k) {
                    (Op::PtrAdd, 0) => at_none(&ptr_slot, at) == s,
                    (Op::Load, 0) if non_volatile => {
                        let ty = Ty(at_u16(view.tys, at));
                        let off = i128::from(at_u32(view.extra, to_idx(at_u32(view.data, at)) + 1).cast_signed());
                        ty.is_scalar()
                            && base
                                .checked_add(off)
                                .is_some_and(|total| total >= 0 && total + bytes(ty, ptr_bits) <= slot_size)
                    }
                    (Op::Store, 1) if non_volatile => {
                        let extra_at = to_idx(at_u32(view.data, at));
                        let stored = at_u32(view.extra, extra_at);
                        let ty = Ty(at_u16(view.tys, to_idx(stored)));
                        let off = i128::from(at_u32(view.extra, extra_at + 2).cast_signed());
                        ty.is_scalar()
                            && base
                                .checked_add(off)
                                .is_some_and(|total| total >= 0 && total + bytes(ty, ptr_bits) <= slot_size)
                    }
                    (Op::Memcpy | Op::Memmove, 0 | 1) if non_volatile => {
                        let extra_at = to_idx(at_u32(view.data, at));
                        let dst = at_u32(view.extra, extra_at);
                        let src = at_u32(view.extra, extra_at + 1);
                        let len_val = at_u32(view.extra, extra_at + 2);
                        let ds = at_none(&ptr_slot, to_idx(dst));
                        let ss = at_none(&ptr_slot, to_idx(src));
                        let doff = ptr_off.get(to_idx(dst)).copied().unwrap_or(-1);
                        let soff = ptr_off.get(to_idx(src)).copied().unwrap_or(-1);
                        let dsz = view.slots.get(to_idx(ds)).map_or(0, |sr| i128::from(sr.size));
                        let ssz = view.slots.get(to_idx(ss)).map_or(0, |sr| i128::from(sr.size));
                        let bounds_ok = ds != NONE
                            && ss != NONE
                            && constant(view, len_val).is_some_and(|len| {
                                len >= 0
                                    && doff >= 0
                                    && soff >= 0
                                    && doff.checked_add(len).is_some_and(|e| e <= dsz)
                                    && soff.checked_add(len).is_some_and(|e| e <= ssz)
                            });
                        if bounds_ok {
                            if let Some(cnt) = slot_copy_count.get_mut(to_idx(s)) {
                                *cnt = cnt.saturating_add(1);
                            }
                        }
                        bounds_ok
                    }
                    (Op::Memset, 0) if non_volatile => {
                        let extra_at = to_idx(at_u32(view.data, at));
                        let byte_val = at_u32(view.extra, extra_at + 1);
                        let len_val = at_u32(view.extra, extra_at + 2);
                        let ok_z = matches!(
                            (constant(view, byte_val), constant(view, len_val)),
                            (Some(0), Some(len))
                                if len >= 0 && base >= 0 && base.checked_add(len).is_some_and(|e| e <= slot_size)
                        );
                        if ok_z {
                            set_at(&mut slot_has_zeroset, to_idx(s), true);
                        }
                        ok_z
                    }
                    _ => false,
                };
                if !ok {
                    set_at(&mut slot_escapes_any, to_idx(s), true);
                }
            })?;
        }
    }
    for b in 0..blocks {
        if cfg.reachable(to_u32(b)) {
            continue;
        }
        for at in view.block(Block(to_u32(b))) {
            each_value(view, at, |_k, v| {
                if let Some(s) = root(view, v) {
                    set_at(&mut slot_escapes_any, to_idx(s), true);
                }
            })?;
        }
    }

    // Round-level plan state.
    let mut block_plan = filled(blocks, NONE, &alloc);
    let mut plans: Vec<PlanMeta, A> = Vec::new_in(alloc);
    let mut plan_fields: Vec<(i32, Ty), A> = Vec::new_in(alloc);
    let mut drop_inst = filled(n, false, &alloc);
    let mut load_to_new_param = filled(n, NONE, &alloc);
    let mut claimed_blocks = filled(blocks, false, &alloc);
    let mut claimed_slots = filled(nslots, false, &alloc);

    // Reusable per-candidate scratch buffers.
    let mut incoming_slot_offs: Vec<(u32, i128), A> = Vec::new_in(alloc);
    let mut derived_off: Vec<Option<i128>, A> = filled(n, None, &alloc);
    let mut phi_loads: Vec<(u32, i128, Ty), A> = Vec::new_in(alloc);
    let mut phi_ptr_adds: Vec<u32, A> = Vec::new_in(alloc);
    let mut unique_fields: Vec<(i128, u16), A> = Vec::new_in(alloc);
    let mut in_set = filled(nslots, false, &alloc);
    let mut dom_by_m = filled(blocks, false, &alloc);
    let mut dom_stack: Vec<u32, A> = Vec::new_in(alloc);
    let mut slot_accesses: Vec<(u32, i128, u16), A> = Vec::new_in(alloc);
    let mut slot_zerosets: Vec<(u32, i128, i128), A> = Vec::new_in(alloc);
    let mut copy_partners: Vec<(u32, i128, u32, i128, i128), A> = Vec::new_in(alloc);

    for m_idx in 1..blocks {
        let m = to_u32(m_idx);
        if !cfg.reachable(m)
            || !at_bool(&cfg.plain_only, m_idx)
            || view.block_kind(Block(m)).is_landing()
            || at_bool(&claimed_blocks, m_idx)
        {
            continue;
        }
        let preds = cfg.preds_of(m);
        if preds.is_empty() {
            continue;
        }
        let mut preds_ok = true;
        for &p in preds {
            if !cfg.reachable(p) || dom_by_landing(view, &cfg, p) {
                preds_ok = false;
                break;
            }
        }
        if !preds_ok {
            continue;
        }

        let params = view.params(Block(m));
        for at in params.start..params.end {
            if Ty(at_u16(view.tys, at)) != Ty::PTR {
                continue;
            }
            let param_idx = at - params.start;

            // Step 1: every incoming edge `P_j -> M` passes `SlotAddr(slot_j) + const_off_j >= 0`.
            incoming_slot_offs.clear();
            let mut step1_ok = true;
            let mut prev_p: Option<u32> = None;
            for &p in preds {
                if Some(p) == prev_p {
                    continue;
                }
                prev_p = Some(p);
                each_plain_target(view, view.terminator(Block(p)), |to, args_start, args_len| {
                    if !step1_ok || to != m {
                        return;
                    }
                    if param_idx >= args_len {
                        step1_ok = false;
                        return;
                    }
                    let arg = at_u32(view.extra, args_start + param_idx);
                    match resolve_slot_ptr(view, arg) {
                        Some((slot_j, base_off_j))
                            if base_off_j >= 0
                                && to_idx(slot_j) < nslots
                                && !at_bool(&claimed_slots, to_idx(slot_j)) =>
                        {
                            incoming_slot_offs.push((slot_j, base_off_j));
                        }
                        _ => step1_ok = false,
                    }
                })?;
                if !step1_ok {
                    break;
                }
            }
            if !step1_ok || incoming_slot_offs.is_empty() {
                continue;
            }

            // Step 2: every transitive use of `ptr_phi` is ONLY const non-neg `PtrAdd` or
            // non-volatile scalar `Load` with `load_off >= 0`.
            derived_off.fill(None);
            set_at(&mut derived_off, at, Some(0));
            phi_loads.clear();
            phi_ptr_adds.clear();
            let mut uses_ok = true;
            for &b in cfg.rpo.iter() {
                for inst in view.block(Block(b)) {
                    let op = view.op(inst);
                    let non_volatile = !view.flags(inst).contains(Flags::VOLATILE);
                    each_value(view, inst, |k, v| {
                        if !uses_ok {
                            return;
                        }
                        let Some(base_off) = derived_off.get(to_idx(v)).copied().flatten() else {
                            return;
                        };
                        match (op, k) {
                            (Op::PtrAdd, 0) => {
                                let extra_at = to_idx(at_u32(view.data, inst));
                                let by_val = at_u32(view.extra, extra_at + 1);
                                match constant(view, by_val) {
                                    Some(by) if by >= 0 => match base_off.checked_add(by) {
                                        Some(new_off) => {
                                            set_at(&mut derived_off, inst, Some(new_off));
                                            phi_ptr_adds.push(to_u32(inst));
                                        }
                                        None => uses_ok = false,
                                    },
                                    _ => uses_ok = false,
                                }
                            }
                            (Op::Load, 0) if non_volatile => {
                                let load_ty = Ty(at_u16(view.tys, inst));
                                let extra_at = to_idx(at_u32(view.data, inst));
                                let load_off = i128::from(at_u32(view.extra, extra_at + 1).cast_signed());
                                if !load_ty.is_scalar() || load_ty.is_none() || load_off < 0 {
                                    uses_ok = false;
                                } else if let Some(total_off) = base_off.checked_add(load_off) {
                                    if total_off >= 0 && i32::try_from(total_off).is_ok() {
                                        phi_loads.push((to_u32(inst), total_off, load_ty));
                                    } else {
                                        uses_ok = false;
                                    }
                                } else {
                                    uses_ok = false;
                                }
                            }
                            _ => uses_ok = false,
                        }
                    })?;
                    if !uses_ok {
                        break;
                    }
                }
                if !uses_ok {
                    break;
                }
            }
            if uses_ok {
                for b in 0..blocks {
                    if cfg.reachable(to_u32(b)) {
                        continue;
                    }
                    for inst in view.block(Block(to_u32(b))) {
                        each_value(view, inst, |_k, v| {
                            if derived_off.get(to_idx(v)).copied().flatten().is_some() {
                                uses_ok = false;
                            }
                        })?;
                    }
                }
            }
            if !uses_ok {
                continue;
            }

            // Step 3: unique sorted `(total_off, load_ty)` fields (capped) and bounds check on every incoming slot.
            unique_fields.clear();
            for &(_, total_off, load_ty) in phi_loads.iter() {
                unique_fields.push((total_off, load_ty.0));
            }
            unique_fields.sort_unstable();
            unique_fields.dedup();
            if unique_fields.len() > MAX_SLOT_PHI_FIELDS {
                continue;
            }
            let mut bounds_ok = true;
            for &(slot_j, base_off_j) in incoming_slot_offs.iter() {
                let slot_size = view.slots.get(to_idx(slot_j)).map_or(0, |s| i128::from(s.size));
                for &(total_off, ty_raw) in unique_fields.iter() {
                    let load_bytes = bytes(Ty(ty_raw), ptr_bits);
                    let in_bounds = base_off_j
                        .checked_add(total_off)
                        .and_then(|o| o.checked_add(load_bytes))
                        .is_some_and(|end| end <= slot_size);
                    if !in_bounds {
                        bounds_ok = false;
                        break;
                    }
                }
                if !bounds_ok {
                    break;
                }
            }
            if !bounds_ok {
                continue;
            }

            // Step 4: verify every incoming `slot_j` does not escape (except into this `ptr_phi`),
            // is never mutated in any block dominated by `M`, and has a compatible field layout.
            in_set.fill(false);
            for &(slot_j, _) in incoming_slot_offs.iter() {
                set_at(&mut in_set, to_idx(slot_j), true);
            }
            mark_dom_subtree(&cfg, m, &mut dom_by_m, &mut dom_stack);
            slot_accesses.clear();
            slot_zerosets.clear();
            copy_partners.clear();
            let mut slots_ok = true;

            for &b in cfg.rpo.iter() {
                let under_m = at_bool(&dom_by_m, to_idx(b));
                for inst in view.block(Block(b)) {
                    let op = view.op(inst);
                    if matches!(op, Op::Jump | Op::Br | Op::Switch) {
                        each_plain_target(view, inst, |to, args_start, args_len| {
                            if !slots_ok {
                                return;
                            }
                            for idx in 0..args_len {
                                let arg = at_u32(view.extra, args_start + idx);
                                let s = at_none(&ptr_slot, to_idx(arg));
                                if s != NONE && at_bool(&in_set, to_idx(s)) && (to != m || idx != param_idx) {
                                    slots_ok = false;
                                }
                            }
                        })?;
                    } else {
                        let non_volatile = !view.flags(inst).contains(Flags::VOLATILE);
                        each_value(view, inst, |k, v| {
                            if !slots_ok {
                                return;
                            }
                            let s = at_none(&ptr_slot, to_idx(v));
                            if s == NONE || !at_bool(&in_set, to_idx(s)) {
                                return;
                            }
                            let base = ptr_off.get(to_idx(v)).copied().unwrap_or(0);
                            let slot_size = view.slots.get(to_idx(s)).map_or(0, |sr| i128::from(sr.size));
                            match (op, k) {
                                (Op::PtrAdd, 0) => {
                                    if at_none(&ptr_slot, inst) != s {
                                        slots_ok = false;
                                    }
                                }
                                (Op::Load, 0) if non_volatile => {
                                    let ty = Ty(at_u16(view.tys, inst));
                                    let off = i128::from(
                                        at_u32(view.extra, to_idx(at_u32(view.data, inst)) + 1).cast_signed(),
                                    );
                                    if let Some(total) = base.checked_add(off) {
                                        if ty.is_scalar()
                                            && total >= 0
                                            && total + bytes(ty, ptr_bits) <= slot_size
                                        {
                                            slot_accesses.push((s, total, ty.0));
                                        } else {
                                            slots_ok = false;
                                        }
                                    } else {
                                        slots_ok = false;
                                    }
                                }
                                (Op::Store, 1) if non_volatile && !under_m => {
                                    let extra_at = to_idx(at_u32(view.data, inst));
                                    let stored = at_u32(view.extra, extra_at);
                                    let ty = Ty(at_u16(view.tys, to_idx(stored)));
                                    let off = i128::from(at_u32(view.extra, extra_at + 2).cast_signed());
                                    if let Some(total) = base.checked_add(off) {
                                        if ty.is_scalar()
                                            && total >= 0
                                            && total + bytes(ty, ptr_bits) <= slot_size
                                        {
                                            slot_accesses.push((s, total, ty.0));
                                        } else {
                                            slots_ok = false;
                                        }
                                    } else {
                                        slots_ok = false;
                                    }
                                }
                                (Op::Memcpy | Op::Memmove, 0 | 1) if non_volatile => {
                                    if k == 0 && under_m {
                                        slots_ok = false;
                                    } else {
                                        let extra_at = to_idx(at_u32(view.data, inst));
                                        let dst = at_u32(view.extra, extra_at);
                                        let src = at_u32(view.extra, extra_at + 1);
                                        let len_val = at_u32(view.extra, extra_at + 2);
                                        let ds = at_none(&ptr_slot, to_idx(dst));
                                        let ss = at_none(&ptr_slot, to_idx(src));
                                        let doff = ptr_off.get(to_idx(dst)).copied().unwrap_or(-1);
                                        let soff = ptr_off.get(to_idx(src)).copied().unwrap_or(-1);
                                        let dsz = view.slots.get(to_idx(ds)).map_or(0, |sr| i128::from(sr.size));
                                        let ssz = view.slots.get(to_idx(ss)).map_or(0, |sr| i128::from(sr.size));
                                        let (other_s, s_off, other_off) =
                                            if k == 0 { (ss, doff, soff) } else { (ds, soff, doff) };
                                        let valid = ds != NONE
                                            && ss != NONE
                                            && !at_bool(&in_set, to_idx(other_s))
                                            && !at_bool(&slot_escapes_any, to_idx(other_s))
                                            && at_u32(&slot_copy_count, to_idx(other_s)) == 1
                                            && !at_bool(&slot_has_zeroset, to_idx(other_s));
                                        match (valid, constant(view, len_val)) {
                                            (true, Some(len))
                                                if len >= 0
                                                    && doff >= 0
                                                    && soff >= 0
                                                    && doff.checked_add(len).is_some_and(|e| e <= dsz)
                                                    && soff.checked_add(len).is_some_and(|e| e <= ssz) =>
                                            {
                                                copy_partners.push((s, s_off, other_s, other_off, len));
                                            }
                                            _ => slots_ok = false,
                                        }
                                    }
                                }
                                (Op::Memset, 0) if non_volatile && !under_m => {
                                    let extra_at = to_idx(at_u32(view.data, inst));
                                    let byte_val = at_u32(view.extra, extra_at + 1);
                                    let len_val = at_u32(view.extra, extra_at + 2);
                                    match (constant(view, byte_val), constant(view, len_val)) {
                                        (Some(0), Some(len))
                                            if len >= 0
                                                && base >= 0
                                                && base.checked_add(len).is_some_and(|e| e <= slot_size) =>
                                        {
                                            slot_zerosets.push((s, base, len));
                                        }
                                        _ => slots_ok = false,
                                    }
                                }
                                _ => slots_ok = false,
                            }
                        })?;
                    }
                    if !slots_ok {
                        break;
                    }
                }
                if !slots_ok {
                    break;
                }
            }
            if slots_ok {
                for b in 0..blocks {
                    if cfg.reachable(to_u32(b)) {
                        continue;
                    }
                    for inst in view.block(Block(to_u32(b))) {
                        each_value(view, inst, |_k, v| {
                            if root(view, v).is_some_and(|s| at_bool(&in_set, to_idx(s))) {
                                slots_ok = false;
                            }
                        })?;
                    }
                }
            }
            if !slots_ok {
                continue;
            }

            // Include speculated loads in `slot_accesses` and map single-hop copy partner fields.
            for &(slot_j, base_off_j) in incoming_slot_offs.iter() {
                for &(total_off, ty_raw) in unique_fields.iter() {
                    if let Some(off) = base_off_j.checked_add(total_off) {
                        slot_accesses.push((slot_j, off, ty_raw));
                    } else {
                        slots_ok = false;
                    }
                }
            }
            if slots_ok && !copy_partners.is_empty() {
                for &(s, s_off, other_s, other_off, len) in copy_partners.iter() {
                    let s_end = s_off + len;
                    let other_end = other_off + len;
                    for &b in cfg.rpo.iter() {
                        for inst in view.block(Block(b)) {
                            let op = view.op(inst);
                            let non_volatile = !view.flags(inst).contains(Flags::VOLATILE);
                            if !non_volatile {
                                continue;
                            }
                            let (ptr_v, acc_ty, acc_off) = match op {
                                Op::Load => {
                                    let extra_at = to_idx(at_u32(view.data, inst));
                                    let pv = at_u32(view.extra, extra_at);
                                    let off = i128::from(at_u32(view.extra, extra_at + 1).cast_signed());
                                    (pv, Ty(at_u16(view.tys, inst)), off)
                                }
                                Op::Store => {
                                    let extra_at = to_idx(at_u32(view.data, inst));
                                    let sv = at_u32(view.extra, extra_at);
                                    let pv = at_u32(view.extra, extra_at + 1);
                                    let off = i128::from(at_u32(view.extra, extra_at + 2).cast_signed());
                                    (pv, Ty(at_u16(view.tys, to_idx(sv))), off)
                                }
                                _ => continue,
                            };
                            if at_none(&ptr_slot, to_idx(ptr_v)) != other_s {
                                continue;
                            }
                            let base = ptr_off.get(to_idx(ptr_v)).copied().unwrap_or(0);
                            let Some(f_off) = base.checked_add(acc_off) else {
                                slots_ok = false;
                                break;
                            };
                            let f_end = f_off + bytes(acc_ty, ptr_bits);
                            if f_off < other_end && f_end > other_off {
                                if f_off < other_off || f_end > other_end {
                                    slots_ok = false;
                                    break;
                                }
                                slot_accesses.push((s, s_off + (f_off - other_off), acc_ty.0));
                            }
                        }
                        if !slots_ok {
                            break;
                        }
                    }
                    if !slots_ok {
                        break;
                    }
                    for &(f_slot, f_off, f_ty_raw) in slot_accesses.iter() {
                        if f_slot != s {
                            continue;
                        }
                        let f_end = f_off + bytes(Ty(f_ty_raw), ptr_bits);
                        if f_off < s_end && f_end > s_off && (f_off < s_off || f_end > s_end) {
                            slots_ok = false;
                            break;
                        }
                    }
                    if !slots_ok {
                        break;
                    }
                }
            }
            if !slots_ok {
                continue;
            }

            slot_accesses.sort_unstable();
            slot_accesses.dedup();
            coalesce_ptr_pun_candidates(&mut slot_accesses, ptr_int);
            let mut last: Option<(u32, i128, u16, i128)> = None;
            for &(s, offset, ty_raw) in slot_accesses.iter() {
                let end = offset + bytes(Ty(ty_raw), ptr_bits);
                last = match last {
                    Some((ps, po, pt, reach)) if ps == s => {
                        if (po, pt) != (offset, ty_raw) && reach > offset {
                            slots_ok = false;
                            break;
                        }
                        Some((s, offset, ty_raw, reach.max(end)))
                    }
                    _ => Some((s, offset, ty_raw, end)),
                };
            }
            if slots_ok && !slot_zerosets.is_empty() {
                for &(zs, z_start, len) in slot_zerosets.iter() {
                    let z_end = z_start + len;
                    for &(f_slot, f_off, f_ty_raw) in slot_accesses.iter() {
                        if f_slot != zs {
                            continue;
                        }
                        let ty = Ty(f_ty_raw);
                        let f_end = f_off + bytes(ty, ptr_bits);
                        if f_off < z_end && f_end > z_start {
                            let zeroable = ty.is_int() || ty == Ty::PTR || ty.is_float();
                            if f_off < z_start || f_end > z_end || !zeroable {
                                slots_ok = false;
                                break;
                            }
                        }
                    }
                    if !slots_ok {
                        break;
                    }
                }
            }
            if !slots_ok {
                continue;
            }

            // Accept this `ptr_phi` for promotion in this round.
            set_at(&mut claimed_blocks, m_idx, true);
            for &(slot_j, _) in incoming_slot_offs.iter() {
                set_at(&mut claimed_slots, to_idx(slot_j), true);
            }
            let field_start = to_u32(plan_fields.len());
            let field_len = to_u32(unique_fields.len());
            for &(total_off, ty_raw) in unique_fields.iter() {
                let off_i32 = i32::try_from(total_off).unwrap_or(0);
                plan_fields.push((off_i32, Ty(ty_raw)));
            }
            let plan_idx = to_u32(plans.len());
            plans.push(PlanMeta { param_idx, param_inst: to_u32(at), field_start, field_len });
            set_at(&mut block_plan, m_idx, plan_idx);
            set_at(&mut drop_inst, at, true);
            for &pa in phi_ptr_adds.iter() {
                set_at(&mut drop_inst, to_idx(pa), true);
            }
            for &(load_at, total_off, load_ty) in phi_loads.iter() {
                if let Some(rel) = unique_fields.iter().position(|&pair| pair == (total_off, load_ty.0)) {
                    set_at(&mut drop_inst, to_idx(load_at), true);
                    set_at(
                        &mut load_to_new_param,
                        to_idx(load_at),
                        to_u32(n) + field_start + to_u32(rel),
                    );
                }
            }
            break;
        }
    }

    if plans.is_empty() {
        return Ok(None);
    }
    if n + plan_fields.len() >= to_idx(NONE) {
        return Err(SimplifyError::Refused { inst: 0, what: "more values than LIR numbers" });
    }

    // Rewrite the function: drop promoted `ptr_phi` params and their `PtrAdd`/`Load` trees,
    // append the new scalar `Op::Param`s to each target block, and emit the corresponding
    // `Op::Load`s right before each incoming predecessor's terminator.
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
    let mut out = Out { func, fixups: Vec::new_in(alloc) };
    let mut map = filled(n + plan_fields.len(), NONE, &alloc);
    let mut edge_loads: Vec<u32, A> = Vec::new_in(alloc);

    for b in 0..blocks {
        out.func.block_starts.push(to_u32(out.func.ops.len()));
        out.func.block_kinds.push(at_u8(view.block_kinds, b));
        let params = view.params(Block(to_u32(b)));
        for at in params.start..params.end {
            if at_bool(&drop_inst, at) {
                continue;
            }
            let new_id = out.raw(
                Op::Param,
                at_u8(view.flags, at),
                at_u16(view.tys, at),
                at_u32(view.data, at),
                at_u32(view.attrs, at),
                at_u32(view.locs, at),
            );
            set_at(&mut map, at, new_id);
        }
        let p_idx = at_none(&block_plan, b);
        if p_idx != NONE {
            if let Some(meta) = plans.get(to_idx(p_idx)).copied() {
                let f_start = to_idx(meta.field_start);
                let f_end = f_start + to_idx(meta.field_len);
                let loc = at_u32(view.locs, to_idx(meta.param_inst));
                for f_idx in f_start..f_end {
                    if let Some(&(_, ty)) = plan_fields.get(f_idx) {
                        let new_id = out.raw(Op::Param, 0, ty.0, 0, 0, loc);
                        set_at(&mut map, n + f_idx, new_id);
                    }
                }
            }
        }
        let block_end = view.block(Block(to_u32(b))).end;
        for at in params.end..block_end {
            if at_bool(&drop_inst, at) {
                continue;
            }
            let op = view.op(at);
            if op.is_terminator() {
                edge_loads.clear();
                let term_loc = at_u32(view.locs, at);
                each_plain_target(view, at, |to, args_start, args_len| {
                    let target_plan = at_none(&block_plan, to_idx(to));
                    if target_plan == NONE {
                        return;
                    }
                    let Some(meta) = plans.get(to_idx(target_plan)).copied() else { return };
                    if meta.param_idx >= args_len {
                        return;
                    }
                    let arg_val = at_u32(view.extra, args_start + meta.param_idx);
                    let f_start = to_idx(meta.field_start);
                    let f_end = f_start + to_idx(meta.field_len);
                    for f_idx in f_start..f_end {
                        if let Some(&(off_i32, load_ty)) = plan_fields.get(f_idx) {
                            let extra_start = to_u32(out.func.extra.len());
                            out.value(arg_val, at);
                            out.func.extra.push(off_i32.cast_unsigned());
                            let ld = out.raw(Op::Load, 0, load_ty.0, extra_start, 0, term_loc);
                            edge_loads.push(ld);
                        }
                    }
                })?;
                let new_term = out.copy_for_phi_promo(view, at, &block_plan, &plans, &edge_loads)?;
                set_at(&mut map, at, new_term);
            } else {
                let new_inst = out.copy_for_phi_promo(view, at, &block_plan, &plans, &[])?;
                set_at(&mut map, at, new_inst);
            }
        }
    }

    let Out { mut func, fixups } = out;
    for &(place, in_data, inst) in fixups.iter() {
        let word_opt = match in_data {
            true => func.data.get_mut(to_idx(place)),
            false => func.extra.get_mut(to_idx(place)),
        };
        let Some(word) = word_opt else {
            return Err(SimplifyError::Refused { inst: to_idx(inst), what: "fixup out of bounds" });
        };
        let mut id = *word;
        if to_idx(id) < n {
            let redir = at_none(&load_to_new_param, to_idx(id));
            if redir != NONE {
                id = redir;
            }
        }
        let to = at_none(&map, to_idx(id));
        if to == NONE {
            return Err(SimplifyError::Refused { inst: to_idx(inst), what: "a use of a value the rewrite removed" });
        }
        *word = to;
    }
    if u32::try_from(func.ops.len()).is_err() || u32::try_from(func.extra.len()).is_err() {
        return Err(SimplifyError::Refused { inst: 0, what: "an output past LIR's columns" });
    }
    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(Some(func))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Load,
    /// A store of `value` at instruction `inst`.
    Store { value: u32, inst: u32 },
    /// A `slot_live` or `slot_dead`: every field of the slot is undefined after it.
    Reset,
    /// A non-volatile `memcpy` or `memmove` of `len` bytes from `(src_slot, src_offset)` into `(slot, offset)`.
    Copy { src_slot: u32, src_offset: i128, len: i128 },
    /// A non-volatile `memset` of `len` zero bytes into `(slot, offset)`.
    ZeroSet { len: i128 },
}

/// A load, store or reset of a slot, in a reachable block.
#[derive(Clone, Copy, Debug)]
struct Access {
    slot: u32,
    /// Bytes from the slot's start (0 for a reset).
    offset: i128,
    /// The type loaded or stored ([`Ty::NONE`] for a reset, copy or zeroset).
    ty: Ty,
    kind: Kind,
    /// Its field, once fields are known; [`NONE`] for a reset, copy, zeroset or a kept slot's access.
    field: u32,
}

#[derive(Clone, Copy, Debug)]
struct FieldRec {
    slot: u32,
    offset: i128,
    ty: Ty,
}

/// What the first walk found.
struct Found<A: Allocator> {
    /// Per slot: kept in memory (its address escapes, or it cannot be replaced).
    kept: Vec<bool, A>,
    accesses: Vec<Access, A>,
    /// Per block reached, its accesses: `accesses[start..end]`.
    block_acc: Vec<(u32, u32), A>,
    /// Per instruction: its access, for a load, store, copy or zeroset of a slot; else [`NONE`].
    access_of: Vec<u32, A>,
    fields: Vec<FieldRec, A>,
    /// Per slot: its fields, `fields[first..first + count]`.
    slot_fields: Vec<(u32, u32), A>,
}

impl<A: Allocator> Found<A> {
    fn fields_of(&self, slot: u32) -> core::ops::Range<u32> {
        let (first, count) = self.slot_fields.get(to_idx(slot)).copied().unwrap_or((0, 0));
        first..first + count
    }

    /// Finds the field index of `slot` at exact byte `offset` with type `ty`.
    fn find_field(&self, slot: u32, offset: i128, ty: Ty) -> Option<u32> {
        let range = self.fields_of(slot);
        let slice = self.fields.get(to_idx(range.start)..to_idx(range.end))?;
        for (idx, rec) in slice.iter().enumerate() {
            if rec.offset == offset && rec.ty == ty {
                return Some(range.start + to_u32(idx));
            }
        }
        None
    }
}

/// The first walk's state: pointers into slots by value.
struct Walk<'v, A: Allocator> {
    view: View<'v>,
    kept: Vec<bool, A>,
    /// Per value: [`NONE`], or its row of `ptrs`: a pointer `offset` bytes into `slot`.
    ptr_at: Vec<u32, A>,
    ptrs: Vec<(u32, i128), A>,
    accesses: Vec<Access, A>,
    access_of: Vec<u32, A>,
}

impl<A: Allocator + Copy> Walk<'_, A> {
    fn inst(&mut self, at: usize, reachable: bool) -> Result<(), SimplifyError> {
        let view = self.view;
        let op = view.op(at);
        match op {
            Op::SlotAddr => {
                self.ptr_at[at] = self.ptrs.len() as u32;
                self.ptrs.push((view.data[at], 0));
            }
            Op::SlotLive | Op::SlotDead if reachable => self.accesses.push(Access {
                slot: view.data[at],
                offset: 0,
                ty: Ty::NONE,
                kind: Kind::Reset,
                field: NONE,
            }),
            _ => {}
        }
        each_value(&view, at, |k, value| self.operand(at, op, k, value, reachable))
    }

    /// The `k`-th value operand of instruction `at`, `value`: an allowed use of a pointer
    /// into a slot, or the slot kept.
    fn operand(&mut self, at: usize, op: Op, k: usize, value: u32, reachable: bool) {
        let view = self.view;
        if !reachable {
            // An unreachable block is never run and its uses are not checked; a pointer used
            // there keeps its slot, so nothing in it names a value the output drops.
            if let Some(slot) = root(&view, value) {
                self.kept[slot as usize] = true;
            }
            return;
        }
        let row = self.ptr_at[value as usize];
        if row == NONE {
            return;
        }
        let (slot, base) = self.ptrs[row as usize];
        let non_volatile = !view.flags(at).contains(Flags::VOLATILE);
        let mut o = view.operands(at);
        let allowed = match (op, k) {
            (Op::Load, 0) if non_volatile => {
                o.value();
                let ty = Ty(view.tys[at]);
                match (ty.is_scalar(), base.checked_add(i128::from(o.offset()))) {
                    (true, Some(offset)) => {
                        self.access(at, slot, offset, ty, Kind::Load);
                        true
                    }
                    _ => false,
                }
            }
            (Op::Store, 1) if non_volatile => {
                let stored = o.value().0;
                o.value();
                let ty = Ty(view.tys[stored as usize]);
                match (ty.is_scalar(), base.checked_add(i128::from(o.offset()))) {
                    (true, Some(offset)) => {
                        self.access(at, slot, offset, ty, Kind::Store { value: stored, inst: to_u32(at) });
                        true
                    }
                    _ => false,
                }
            }
            (Op::PtrAdd, 0) => {
                o.value();
                let by = o.value().0;
                match constant(&view, by).and_then(|by| base.checked_add(by)) {
                    Some(offset) => {
                        self.ptr_at[at] = self.ptrs.len() as u32;
                        self.ptrs.push((slot, offset));
                        true
                    }
                    None => false,
                }
            }
            (Op::Memcpy | Op::Memmove, 0 | 1) if non_volatile => {
                let dst = o.value().0;
                let src = o.value().0;
                let len_val = o.value().0;
                let dst_row = self.ptr_at.get(to_idx(dst)).copied().unwrap_or(NONE);
                let src_row = self.ptr_at.get(to_idx(src)).copied().unwrap_or(NONE);
                if let (Some(&(dst_slot, dst_off)), Some(&(src_slot, src_off)), Some(len)) = (
                    self.ptrs.get(to_idx(dst_row)),
                    self.ptrs.get(to_idx(src_row)),
                    constant(&view, len_val),
                ) {
                    let dst_size = view.slots.get(to_idx(dst_slot)).map_or(0, |s| i128::from(s.size));
                    let src_size = view.slots.get(to_idx(src_slot)).map_or(0, |s| i128::from(s.size));
                    let dst_ok =
                        len >= 0 && dst_off >= 0 && dst_off.checked_add(len).is_some_and(|end| end <= dst_size);
                    let src_ok =
                        len >= 0 && src_off >= 0 && src_off.checked_add(len).is_some_and(|end| end <= src_size);
                    if dst_ok && src_ok {
                        if k == 0 {
                            self.access(
                                at,
                                dst_slot,
                                dst_off,
                                Ty::NONE,
                                Kind::Copy { src_slot, src_offset: src_off, len },
                            );
                        }
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            (Op::Memset, 0) if non_volatile => {
                o.value();
                let byte_val = o.value().0;
                let len_val = o.value().0;
                let slot_size = view.slots.get(to_idx(slot)).map_or(0, |s| i128::from(s.size));
                match (constant(&view, byte_val), constant(&view, len_val)) {
                    (Some(0), Some(len))
                        if len >= 0 && base >= 0 && base.checked_add(len).is_some_and(|end| end <= slot_size) =>
                    {
                        self.access(at, slot, base, Ty::NONE, Kind::ZeroSet { len });
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        };
        if !allowed {
            self.kept[slot as usize] = true;
        }
    }

    fn access(&mut self, at: usize, slot: u32, offset: i128, ty: Ty, kind: Kind) {
        self.access_of[at] = self.accesses.len() as u32;
        self.accesses.push(Access { slot, offset, ty, kind, field: NONE });
    }
}

/// If either slot of a `Copy` is kept in memory, both must stay in memory.
fn propagate_kept_copies(kept: &mut [bool], accesses: &[Access]) {
    let limit = kept.len().saturating_add(1);
    for _ in 0..limit {
        let mut changed = false;
        for access in accesses {
            if let Kind::Copy { src_slot, .. } = access.kind {
                let d = to_idx(access.slot);
                let s = to_idx(src_slot);
                let kd = kept.get(d).copied().unwrap_or(true);
                let ks = kept.get(s).copied().unwrap_or(true);
                if kd && !ks {
                    if let Some(flag) = kept.get_mut(s) {
                        *flag = true;
                        changed = true;
                    }
                } else if ks && !kd {
                    if let Some(flag) = kept.get_mut(d) {
                        *flag = true;
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
}

/// Collapses any `(slot, offset)` group in sorted, deduplicated `cand` whose only candidate
/// types are `Ty::PTR` and the pointer-sized integer `ptr_int` into the single canonical field
/// `(slot, offset, Ty::PTR)`.
fn coalesce_ptr_pun_candidates<A: Allocator>(cand: &mut Vec<(u32, i128, u16), A>, ptr_int: Option<Ty>) {
    let Some(pi) = ptr_int else { return };
    let len = cand.len();
    let mut read = 0usize;
    let mut write = 0usize;
    while read < len {
        let Some(&(slot, offset, first_ty)) = cand.get(read) else { break };
        let mut end = read + 1;
        while let Some(&(s2, o2, _)) = cand.get(end) {
            if s2 != slot || o2 != offset {
                break;
            }
            end += 1;
        }
        if end - read == 2 {
            let second_ty = cand.get(read + 1).map_or(0, |t| t.2);
            let is_pair = (first_ty == Ty::PTR.0 && second_ty == pi.0)
                || (first_ty == pi.0 && second_ty == Ty::PTR.0);
            if is_pair {
                set_at(cand, write, (slot, offset, Ty::PTR.0));
                write += 1;
                read = end;
                continue;
            }
        }
        for idx in read..end {
            if let Some(&item) = cand.get(idx) {
                set_at(cand, write, item);
                write += 1;
            }
        }
        read = end;
    }
    cand.truncate(write);
}

/// Finds the field index of `slot` at exact byte `offset` compatible with `ty`
/// (exact match, or pointer-sized integer accessing a canonical `Ty::PTR` field).
fn find_field_compat(
    fields: &[FieldRec],
    slot_fields: &[(u32, u32)],
    slot: u32,
    offset: i128,
    ty: Ty,
    ptr_int: Option<Ty>,
) -> Option<u32> {
    let (first, count) = slot_fields.get(to_idx(slot)).copied()?;
    let slice = fields.get(to_idx(first)..to_idx(first.saturating_add(count)))?;
    for (idx, rec) in slice.iter().enumerate() {
        if rec.offset == offset && (rec.ty == ty || (rec.ty == Ty::PTR && Some(ty) == ptr_int)) {
            return Some(first.saturating_add(to_u32(idx)));
        }
    }
    None
}

/// The first walk: every reachable block in reverse postorder (a value's definition before
/// its uses), then the unreachable ones; then the fields of the slots not kept.
fn find<A: Allocator + Copy>(view: &View<'_>, cfg: &Cfg<A>, alloc: &A) -> Result<Found<A>, SimplifyError> {
    let (n, blocks, slots) = (view.len(), view.blocks(), view.slots.len());
    let mut w = Walk {
        view: *view,
        kept: filled(slots, false, alloc),
        ptr_at: filled(n, NONE, alloc),
        ptrs: Vec::new_in(*alloc),
        accesses: Vec::new_in(*alloc),
        access_of: filled(n, NONE, alloc),
    };
    let mut block_acc = filled(blocks, (0u32, 0u32), alloc);
    for &b in cfg.rpo.iter() {
        let start = w.accesses.len() as u32;
        for at in view.block(Block(b)) {
            w.inst(at, true)?;
        }
        block_acc[b as usize] = (start, w.accesses.len() as u32);
    }
    for b in (0..blocks as u32).filter(|&b| !cfg.reachable(b)) {
        for at in view.block(Block(b)) {
            w.inst(at, false)?;
        }
    }
    let Walk { mut kept, mut accesses, access_of, .. } = w;
    propagate_kept_copies(&mut kept, &accesses);

    // Direct scalar fields `(slot, offset, ty)` of slots not kept, plus copy/zeroset presence.
    let mut cand: Vec<(u32, i128, u16), A> = Vec::new_in(*alloc);
    let mut any_copy = false;
    let mut any_zeroset = false;
    for access in accesses.iter() {
        if kept.get(to_idx(access.slot)).copied().unwrap_or(true) {
            continue;
        }
        match access.kind {
            Kind::Load | Kind::Store { .. } => {
                let size = bytes(access.ty, view.head.ptr_bits);
                let end = access.offset + size;
                let slot_size = view.slots.get(to_idx(access.slot)).map_or(0, |s| i128::from(s.size));
                if access.offset < 0 || end > slot_size {
                    if let Some(flag) = kept.get_mut(to_idx(access.slot)) {
                        *flag = true;
                    }
                } else {
                    cand.push((access.slot, access.offset, access.ty.0));
                }
            }
            Kind::Copy { .. } => any_copy = true,
            Kind::ZeroSet { .. } => any_zeroset = true,
            Kind::Reset => {}
        }
    }
    cand.sort_unstable();
    cand.dedup();

    // Propagate scalar field layouts bidirectionally across slot-to-slot `Copy` accesses.
    if any_copy {
        const MAX_COPY_PROP_ROUNDS: usize = 64;
        const MAX_PROP_FIELDS: usize = 16_384;
        let mut converged = false;
        let mut new_fields: Vec<(u32, i128, u16), A> = Vec::new_in(*alloc);
        for _ in 0..MAX_COPY_PROP_ROUNDS {
            propagate_kept_copies(&mut kept, &accesses);
            new_fields.clear();
            for access in accesses.iter() {
                let Kind::Copy { src_slot, src_offset, len } = access.kind else { continue };
                let dst_slot = access.slot;
                let dst_offset = access.offset;
                if kept.get(to_idx(dst_slot)).copied().unwrap_or(true)
                    || kept.get(to_idx(src_slot)).copied().unwrap_or(true)
                {
                    continue;
                }
                let dst_end = dst_offset + len;
                let src_end = src_offset + len;
                let dst_size = view.slots.get(to_idx(dst_slot)).map_or(0, |s| i128::from(s.size));
                let src_size = view.slots.get(to_idx(src_slot)).map_or(0, |s| i128::from(s.size));
                for &(f_slot, f_off, f_ty_raw) in cand.iter() {
                    let f_size = bytes(Ty(f_ty_raw), view.head.ptr_bits);
                    let f_end = f_off + f_size;
                    if f_slot == src_slot && f_off < src_end && f_end > src_offset {
                        if f_off < src_offset || f_end > src_end {
                            if let Some(flag) = kept.get_mut(to_idx(dst_slot)) {
                                *flag = true;
                            }
                            if let Some(flag) = kept.get_mut(to_idx(src_slot)) {
                                *flag = true;
                            }
                        } else {
                            let mapped = dst_offset + (f_off - src_offset);
                            if mapped < 0 || mapped + f_size > dst_size {
                                if let Some(flag) = kept.get_mut(to_idx(dst_slot)) {
                                    *flag = true;
                                }
                                if let Some(flag) = kept.get_mut(to_idx(src_slot)) {
                                    *flag = true;
                                }
                            } else {
                                new_fields.push((dst_slot, mapped, f_ty_raw));
                            }
                        }
                    }
                    if f_slot == dst_slot && f_off < dst_end && f_end > dst_offset {
                        if f_off < dst_offset || f_end > dst_end {
                            if let Some(flag) = kept.get_mut(to_idx(dst_slot)) {
                                *flag = true;
                            }
                            if let Some(flag) = kept.get_mut(to_idx(src_slot)) {
                                *flag = true;
                            }
                        } else {
                            let mapped = src_offset + (f_off - dst_offset);
                            if mapped < 0 || mapped + f_size > src_size {
                                if let Some(flag) = kept.get_mut(to_idx(dst_slot)) {
                                    *flag = true;
                                }
                                if let Some(flag) = kept.get_mut(to_idx(src_slot)) {
                                    *flag = true;
                                }
                            } else {
                                new_fields.push((src_slot, mapped, f_ty_raw));
                            }
                        }
                    }
                }
            }
            let before = cand.len();
            cand.extend_from_slice(&new_fields);
            cand.sort_unstable();
            cand.dedup();
            if cand.len() > MAX_PROP_FIELDS {
                break;
            }
            if cand.len() == before {
                converged = true;
                break;
            }
        }
        if !converged {
            for access in accesses.iter() {
                if let Kind::Copy { src_slot, .. } = access.kind {
                    if let Some(flag) = kept.get_mut(to_idx(access.slot)) {
                        *flag = true;
                    }
                    if let Some(flag) = kept.get_mut(to_idx(src_slot)) {
                        *flag = true;
                    }
                }
            }
        }
    }

    // Coalesce same-offset `Ty::PTR` <-> pointer-sized integer accesses to canonical `Ty::PTR`.
    let ptr_int = ptr_int_ty(view.head.ptr_bits);
    coalesce_ptr_pun_candidates(&mut cand, ptr_int);

    // A slot whose fields leave it, or overlap without being the same field, is kept.
    let mut last: Option<(u32, i128, u16, i128)> = None;
    for &(slot, offset, ty_raw) in cand.iter() {
        let size = bytes(Ty(ty_raw), view.head.ptr_bits);
        let end = offset + size;
        let slot_size = view.slots.get(to_idx(slot)).map_or(0, |s| i128::from(s.size));
        if offset < 0 || end > slot_size {
            if let Some(flag) = kept.get_mut(to_idx(slot)) {
                *flag = true;
            }
        }
        last = match last {
            Some((prev_slot, prev_off, prev_ty, reach)) if prev_slot == slot => {
                if (prev_off, prev_ty) != (offset, ty_raw) && reach > offset {
                    if let Some(flag) = kept.get_mut(to_idx(slot)) {
                        *flag = true;
                    }
                }
                Some((slot, offset, ty_raw, reach.max(end)))
            }
            _ => Some((slot, offset, ty_raw, end)),
        };
    }

    // A `ZeroSet` that straddles any field, or touches a non-zeroable field, keeps its slot.
    if any_zeroset {
        for access in accesses.iter() {
            let Kind::ZeroSet { len } = access.kind else { continue };
            let slot = access.slot;
            if kept.get(to_idx(slot)).copied().unwrap_or(true) {
                continue;
            }
            let z_start = access.offset;
            let z_end = z_start + len;
            for &(f_slot, f_off, f_ty_raw) in cand.iter() {
                if f_slot != slot {
                    continue;
                }
                let ty = Ty(f_ty_raw);
                let f_end = f_off + bytes(ty, view.head.ptr_bits);
                if f_off < z_end && f_end > z_start {
                    let zeroable = ty.is_int() || ty == Ty::PTR || ty.is_float();
                    if f_off < z_start || f_end > z_end || !zeroable {
                        if let Some(flag) = kept.get_mut(to_idx(slot)) {
                            *flag = true;
                        }
                    }
                }
            }
        }
    }
    propagate_kept_copies(&mut kept, &accesses);

    // The fields of the slots not kept, in (slot, offset, type) order.
    let mut fields = Vec::new_in(*alloc);
    let mut slot_fields = filled(slots, (0u32, 0u32), alloc);
    for &(slot, offset, ty_raw) in cand.iter() {
        if kept.get(to_idx(slot)).copied().unwrap_or(true) {
            continue;
        }
        if let Some(sf) = slot_fields.get_mut(to_idx(slot)) {
            if sf.1 == 0 {
                sf.0 = to_u32(fields.len());
            }
            sf.1 += 1;
        }
        fields.push(FieldRec { slot, offset, ty: Ty(ty_raw) });
    }
    for access in accesses.iter_mut() {
        if kept.get(to_idx(access.slot)).copied().unwrap_or(true) {
            continue;
        }
        if matches!(access.kind, Kind::Load | Kind::Store { .. }) {
            if let Some(f) =
                find_field_compat(&fields, &slot_fields, access.slot, access.offset, access.ty, ptr_int)
            {
                access.field = f;
            }
        }
    }
    Ok(Found { kept, accesses, block_acc, access_of, fields, slot_fields })
}

/// The new block parameters: per block, `field[start[b]..start[b + 1]]`, by rising field.
struct Phis<A: Allocator> {
    start: Vec<u32, A>,
    field: Vec<u32, A>,
}

impl<A: Allocator> Phis<A> {
    fn of(&self, b: u32) -> core::ops::Range<u32> {
        self.start[b as usize]..self.start[b as usize + 1]
    }
}

/// Places each field's parameters at the iterated dominance frontier of its defining blocks,
/// where the field is live; keeps the slot of a field that would need one where an edge
/// carries no arguments.
fn place<A: Allocator + Copy>(view: &View<'_>, cfg: &Cfg<A>, found: &mut Found<A>, alloc: &A) -> Phis<A> {
    let blocks = view.blocks();
    let nfields = found.fields.len();
    // Defining blocks and blocks reading a field before writing it, as (field, block).
    let mut defs: Vec<(u32, u32), A> = Vec::new_in(*alloc);
    let mut uses: Vec<(u32, u32), A> = Vec::new_in(*alloc);
    let mut stamp = filled(nfields, NONE, alloc);
    for &b in cfg.rpo.iter() {
        let (start, end) = found.block_acc[b as usize];
        for access in &found.accesses[start as usize..end as usize] {
            if found.kept[access.slot as usize] {
                continue;
            }
            match access.kind {
                Kind::Load => {
                    if stamp[access.field as usize] != b {
                        uses.push((access.field, b));
                    }
                }
                Kind::Store { .. } => {
                    stamp[access.field as usize] = b;
                    defs.push((access.field, b));
                }
                Kind::Reset => {
                    for f in found.fields_of(access.slot) {
                        stamp[f as usize] = b;
                        defs.push((f, b));
                    }
                }
                Kind::Copy { src_slot, src_offset, len } => {
                    let dst_offset = access.offset;
                    let dst_end = dst_offset + len;
                    // Record all reads of `src_slot` before stamping writes to `dst_slot`, so
                    // an overlapping self-`memmove` sees the pre-copy values as live-in.
                    for f_dst in found.fields_of(access.slot) {
                        if let Some(rec) = found.fields.get(to_idx(f_dst)).copied() {
                            if rec.offset >= dst_offset && rec.offset < dst_end {
                                let want_src = src_offset + (rec.offset - dst_offset);
                                if let Some(f_src) = found.find_field(src_slot, want_src, rec.ty) {
                                    if stamp.get(to_idx(f_src)).copied().unwrap_or(NONE) != b {
                                        uses.push((f_src, b));
                                    }
                                }
                            }
                        }
                    }
                    for f_dst in found.fields_of(access.slot) {
                        if let Some(rec) = found.fields.get(to_idx(f_dst)).copied() {
                            if rec.offset >= dst_offset && rec.offset < dst_end {
                                if let Some(st) = stamp.get_mut(to_idx(f_dst)) {
                                    *st = b;
                                }
                                defs.push((f_dst, b));
                            }
                        }
                    }
                }
                Kind::ZeroSet { len } => {
                    let z_start = access.offset;
                    let z_end = z_start + len;
                    for f in found.fields_of(access.slot) {
                        if let Some(rec) = found.fields.get(to_idx(f)).copied() {
                            if rec.offset >= z_start && rec.offset < z_end {
                                if let Some(st) = stamp.get_mut(to_idx(f)) {
                                    *st = b;
                                }
                                defs.push((f, b));
                            }
                        }
                    }
                }
            }
        }
    }
    defs.sort_unstable();
    defs.dedup();
    uses.sort_unstable();
    uses.dedup();
    // The dominance frontier, as (block, frontier block), sorted.
    let mut df: Vec<(u32, u32), A> = Vec::new_in(*alloc);
    for &b in cfg.rpo.iter().skip(1) {
        let preds = cfg.preds_of(b);
        if preds.iter().filter(|&&p| cfg.reachable(p)).count() < 2 {
            continue;
        }
        for &p in preds.iter().filter(|&&p| cfg.reachable(p)) {
            let mut runner = p;
            // Up the dominator tree: at most as many steps as blocks.
            for _ in 0..blocks {
                if runner == cfg.idom[b as usize] {
                    break;
                }
                df.push((runner, b));
                runner = cfg.idom[runner as usize];
            }
        }
    }
    df.sort_unstable();
    df.dedup();
    let mut df_start = filled(blocks + 1, 0u32, alloc);
    for &(from, _) in df.iter() {
        df_start[from as usize + 1] += 1;
    }
    for b in 0..blocks {
        df_start[b + 1] += df_start[b];
    }
    // Per field: liveness backward from its reads, then the frontier forward from its writes.
    let (mut def_mark, mut live_mark) = (filled(blocks, NONE, alloc), filled(blocks, NONE, alloc));
    let (mut work_mark, mut phi_mark) = (filled(blocks, NONE, alloc), filled(blocks, NONE, alloc));
    let mut work = Vec::new_in(*alloc);
    let mut placed: Vec<(u32, u32), A> = Vec::new_in(*alloc);
    let (mut d, mut u) = (0, 0);
    for f in 0..nfields as u32 {
        let d_start = d;
        while d < defs.len() && defs[d].0 == f {
            def_mark[defs[d].1 as usize] = f;
            d += 1;
        }
        work.clear();
        while u < uses.len() && uses[u].0 == f {
            let b = uses[u].1;
            if live_mark[b as usize] != f {
                live_mark[b as usize] = f;
                work.push(b);
            }
            u += 1;
        }
        while let Some(b) = work.pop() {
            for &p in cfg.preds_of(b) {
                let p_ = p as usize;
                if !cfg.reachable(p) || live_mark[p_] == f || def_mark[p_] == f {
                    continue;
                }
                live_mark[p_] = f;
                work.push(p);
            }
        }
        work.clear();
        for &(_, b) in &defs[d_start..d] {
            if work_mark[b as usize] != f {
                work_mark[b as usize] = f;
                work.push(b);
            }
        }
        while let Some(x) = work.pop() {
            for &(_, y) in &df[df_start[x as usize] as usize..df_start[x as usize + 1] as usize] {
                let y_ = y as usize;
                if phi_mark[y_] == f || live_mark[y_] != f {
                    continue;
                }
                phi_mark[y_] = f;
                placed.push((y, f));
                if work_mark[y_] != f {
                    work_mark[y_] = f;
                    work.push(y);
                }
            }
        }
    }
    // A parameter where an edge carries no arguments keeps the field's slot.
    for &(y, f) in placed.iter() {
        if !cfg.plain_only[y as usize] {
            found.kept[found.fields[f as usize].slot as usize] = true;
        }
    }
    propagate_kept_copies(&mut found.kept, &found.accesses);
    placed.retain(|&(_, f)| !found.kept[found.fields[f as usize].slot as usize]);
    placed.sort_unstable();
    let mut start = filled(blocks + 1, 0u32, alloc);
    for &(y, _) in placed.iter() {
        start[y as usize + 1] += 1;
    }
    for b in 0..blocks {
        start[b + 1] += start[b];
    }
    let mut field = Vec::with_capacity_in(placed.len(), *alloc);
    for &(_, f) in placed.iter() {
        field.push(f);
    }
    Phis { start, field }
}

/// What reaches a load or an edge: the input's value, a new parameter, zero bytes, or nothing written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Def {
    Undef,
    Zero,
    Old(u32),
    Phi(u32),
}

struct Renamed<A: Allocator> {
    /// Per access: for a load, what it reads.
    load_def: Vec<Def, A>,
    /// The new arguments of every plain edge of every reached block, in operand order.
    edge_args: Vec<Def, A>,
    /// Per block: where its terminator's new arguments start in `edge_args`; [`NONE`] for an
    /// unreachable block, whose new arguments are undefined.
    edge_at: Vec<u32, A>,
}

/// One walk down the dominator tree, an explicit stack and an undo log of field values.
fn rename<A: Allocator + Copy>(
    view: &View<'_>,
    cfg: &Cfg<A>,
    found: &Found<A>,
    phis: &Phis<A>,
    plant: Plant,
    alloc: &A,
) -> Result<Renamed<A>, SimplifyError> {
    let blocks = view.blocks();
    let mut cur = filled(found.fields.len(), Def::Undef, alloc);
    let mut undo: Vec<(u32, Def), A> = Vec::new_in(*alloc);
    let mut copy_snap: Vec<(u32, Def), A> = Vec::new_in(*alloc);
    let mut load_def = filled(found.accesses.len(), Def::Undef, alloc);
    let mut edge_args = Vec::new_in(*alloc);
    let mut edge_at = filled(blocks, NONE, alloc);
    // (block, undo length at its entry, its next child).
    let mut stack: Vec<(u32, u32, u32), A> = Vec::new_in(*alloc);
    let mut enter = if blocks > 0 { Some(0u32) } else { None };
    loop {
        if let Some(b) = enter.take() {
            let mark = undo.len() as u32;
            for k in phis.of(b) {
                let f = phis.field[k as usize];
                undo.push((f, cur[f as usize]));
                cur[f as usize] = Def::Phi(k);
            }
            let (start, end) = found.block_acc[b as usize];
            for i in start..end {
                let access = found.accesses[i as usize];
                if found.kept[access.slot as usize] {
                    continue;
                }
                match access.kind {
                    Kind::Load => load_def[i as usize] = cur[access.field as usize],
                    Kind::Store { value, inst } => {
                        let f = to_idx(access.field);
                        let field_ty = found.fields.get(f).map_or(access.ty, |r| r.ty);
                        let stored_def = if access.ty == field_ty { Def::Old(value) } else { Def::Old(inst) };
                        // Planted (test builds only): a second store is not seen.
                        let stale = cfg!(test)
                            && plant == Plant::KeepStaleStore
                            && matches!(cur.get(f), Some(Def::Old(_)));
                        if !stale {
                            if let Some(slot_cur) = cur.get_mut(f) {
                                undo.push((access.field, *slot_cur));
                                *slot_cur = stored_def;
                            }
                        }
                    }
                    Kind::Reset => {
                        for f in found.fields_of(access.slot) {
                            undo.push((f, cur[f as usize]));
                            cur[f as usize] = Def::Undef;
                        }
                    }
                    Kind::Copy { src_slot, src_offset, len } => {
                        copy_snap.clear();
                        let dst_offset = access.offset;
                        let dst_end = dst_offset + len;
                        for f_dst in found.fields_of(access.slot) {
                            if let Some(rec) = found.fields.get(to_idx(f_dst)).copied() {
                                if rec.offset >= dst_offset && rec.offset < dst_end {
                                    let want_src = src_offset + (rec.offset - dst_offset);
                                    let val = match found.find_field(src_slot, want_src, rec.ty) {
                                        Some(f_src) => cur.get(to_idx(f_src)).copied().unwrap_or(Def::Undef),
                                        None => Def::Undef,
                                    };
                                    copy_snap.push((f_dst, val));
                                }
                            }
                        }
                        for &(f_dst, val) in copy_snap.iter() {
                            if let Some(slot_cur) = cur.get_mut(to_idx(f_dst)) {
                                undo.push((f_dst, *slot_cur));
                                *slot_cur = val;
                            }
                        }
                    }
                    Kind::ZeroSet { len } => {
                        let z_start = access.offset;
                        let z_end = z_start + len;
                        for f in found.fields_of(access.slot) {
                            if let Some(rec) = found.fields.get(to_idx(f)).copied() {
                                if rec.offset >= z_start && rec.offset < z_end {
                                    if let Some(slot_cur) = cur.get_mut(to_idx(f)) {
                                        undo.push((f, *slot_cur));
                                        *slot_cur = Def::Zero;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            edge_at[b as usize] = edge_args.len() as u32;
            each_edge(view, view.terminator(Block(b)), |to, plain| {
                if plain {
                    for k in phis.of(to) {
                        edge_args.push(cur[phis.field[k as usize] as usize]);
                    }
                }
            })?;
            stack.push((b, mark, cfg.child_start[b as usize]));
        }
        let Some(top) = stack.last_mut() else { break };
        let (b, mark, next) = *top;
        if next < cfg.child_start[b as usize + 1] {
            top.2 += 1;
            enter = Some(cfg.children[next as usize]);
            continue;
        }
        while undo.len() > mark as usize {
            if let Some((f, def)) = undo.pop() {
                cur[f as usize] = def;
            }
        }
        stack.pop();
    }
    Ok(Renamed { load_def, edge_args, edge_at })
}

/// The output being written.
struct Out<A: Allocator> {
    func: Func<A>,
    /// The places holding an input value's number (or `len + k` for new parameter `k`), to
    /// be its number here once every block is written: (index, in `data` rather than
    /// `extra`, the input instruction it is an operand of).
    fixups: Vec<(u32, bool, u32), A>,
}

impl<A: Allocator + Copy> Out<A> {
    fn raw(&mut self, op: Op, flags: u8, ty: u16, data: u32, attrs: u32, loc: u32) -> u32 {
        let at = self.func.ops.len() as u32;
        self.func.ops.push(op as u8);
        self.func.flags.push(flags);
        self.func.tys.push(ty);
        self.func.data.push(data);
        self.func.attrs.push(attrs);
        self.func.locs.push(loc);
        at
    }

    /// A value of `ty` whose bits nothing wrote, made once per block: `iconst 0` for an
    /// integer (the evaluator runs it), `uninit` otherwise.
    fn undef<B: Allocator>(&mut self, cache: &mut Vec<(u16, u32), B>, ty: Ty, loc: u32) -> u32 {
        if let Some(&(_, at)) = cache.iter().find(|it| it.0 == ty.0) {
            return at;
        }
        let op = if ty.is_int() { Op::Iconst } else { Op::Uninit };
        let at = self.raw(op, 0, ty.0, 0, 0, loc);
        cache.push((ty.0, at));
        at
    }

    /// A zero-byte scalar of `ty` (from `memset(_, 0, _)`), made once per block.
    fn zero<B: Allocator>(&mut self, cache: &mut Vec<(u16, u32), B>, ty: Ty, loc: u32) -> u32 {
        if let Some(&(_, at)) = cache.iter().find(|it| it.0 == ty.0) {
            return at;
        }
        let at = if ty.is_int() {
            self.raw(Op::Iconst, 0, ty.0, 0, 0, loc)
        } else if ty == Ty::PTR {
            self.raw(Op::Null, 0, ty.0, 0, 0, loc)
        } else if ty.is_float() {
            let bits = ty.bits(self.func.head.ptr_bits);
            if bits <= 32 {
                self.raw(Op::Fconst, 0, ty.0, 0, 0, loc)
            } else {
                let start = to_u32(self.func.extra.len());
                for _ in 0..ty.const_words() {
                    self.func.extra.push(0);
                }
                self.raw(Op::FconstWide, 0, ty.0, start, 0, loc)
            }
        } else {
            self.raw(Op::Uninit, 0, ty.0, 0, 0, loc)
        };
        cache.push((ty.0, at));
        at
    }

    /// A value word of input instruction `inst`, appended to `extra` and fixed up later.
    fn value(&mut self, word: u32, inst: usize) {
        self.fixups.push((self.func.extra.len() as u32, false, inst as u32));
        self.func.extra.push(word);
    }

    /// The target at `words[at..]`, its new arguments `extras[*cursor..]` after its own;
    /// where its words end.
    fn target(
        &mut self,
        words: &[u32],
        mut at: usize,
        inst: usize,
        phis: &Phis<impl Allocator>,
        extras: &[(u32, bool)],
        cursor: &mut usize,
    ) -> usize {
        let block = words[at];
        let n = words[at + 1] as usize;
        let more = phis.of(block).len();
        self.func.extra.push(block);
        self.func.extra.push((n + more) as u32);
        at += 2;
        for _ in 0..n {
            self.value(words[at], inst);
            at += 1;
        }
        for &(word, fixup) in &extras[*cursor..*cursor + more] {
            match fixup {
                true => self.value(word, inst),
                false => self.func.extra.push(word),
            }
        }
        *cursor += more;
        at
    }

    /// Copies input instruction `at`: its values to be fixed up, its kept slot renumbered,
    /// each target given its new arguments from `extras`.
    fn copy(
        &mut self,
        view: &View<'_>,
        at: usize,
        phis: &Phis<impl Allocator>,
        slot_row: &[u32],
        extras: &[(u32, bool)],
    ) -> Result<u32, SimplifyError> {
        let op = view.op(at);
        let new = self.func.ops.len() as u32;
        let mut cursor = 0;
        let data = match op.info().layout {
            Layout::None => view.data[at],
            Layout::Data(Field::Value) => {
                self.fixups.push((new, true, at as u32));
                view.data[at]
            }
            Layout::Data(Field::Slot) => slot_row[view.data[at] as usize],
            Layout::Data(_) => view.data[at],
            Layout::Extra(fields) => {
                let start = self.func.extra.len() as u32;
                let words = &view.extra[view.data[at] as usize..];
                let ty = Ty(view.tys[at]);
                let mut i = 0;
                let mut scrutinee = None;
                for &field in fields {
                    match field {
                        Field::Value => {
                            scrutinee = Some(words[i]);
                            self.value(words[i], at);
                            i += 1;
                        }
                        Field::Slot => {
                            self.func.extra.push(slot_row[words[i] as usize]);
                            i += 1;
                        }
                        Field::Block
                        | Field::Imm
                        | Field::Offset
                        | Field::IntCond
                        | Field::FloatCond
                        | Field::Rmw
                        | Field::Ref
                        | Field::Sig
                        | Field::Intrinsic
                        | Field::Asm => {
                            self.func.extra.push(words[i]);
                            i += 1;
                        }
                        Field::Target => i = self.target(words, i, at, phis, extras, &mut cursor),
                        Field::Values => {
                            let n = words[i] as usize;
                            self.func.extra.push(n as u32);
                            i += 1;
                            for _ in 0..n {
                                self.value(words[i], at);
                                i += 1;
                            }
                        }
                        Field::Blocks | Field::Mask => {
                            let n = words[i] as usize;
                            self.func.extra.extend_from_slice(&words[i..i + 1 + n]);
                            i += 1 + n;
                        }
                        Field::Const => {
                            let n = ty.const_words() as usize;
                            self.func.extra.extend_from_slice(&words[i..i + n]);
                            i += n;
                        }
                        Field::Cases => {
                            let width =
                                scrutinee.map_or(0, |it: u32| Ty(view.tys[it as usize]).const_words()) as usize;
                            let n = words[i] as usize;
                            self.func.extra.push(n as u32);
                            i += 1;
                            for _ in 0..n {
                                self.func.extra.extend_from_slice(&words[i..i + width]);
                                i += width;
                                i = self.target(words, i, at, phis, extras, &mut cursor);
                            }
                        }
                    }
                }
                start
            }
        };
        if cursor != extras.len() {
            return Err(SimplifyError::Refused { inst: at, what: "edges other than the walk down the tree saw" });
        }
        Ok(self.raw(op, view.flags[at], view.tys[at], data, view.attrs[at], view.locs[at]))
    }

    /// Rewrites one `Field::Target` for `promote_slot_addr_phis_round`: if the target block has
    /// a promoted `ptr_phi`, drops its argument at `meta.param_idx` and appends the speculated
    /// loads from `edge_loads[*cursor..]`.
    fn target_for_phi_promo(
        &mut self,
        words: &[u32],
        mut at: usize,
        inst: usize,
        block_plan: &[u32],
        plans: &[PlanMeta],
        edge_loads: &[u32],
        cursor: &mut usize,
    ) -> Result<usize, SimplifyError> {
        let block = words.get(at).copied().ok_or_else(|| layout_error(inst))?;
        let n = to_idx(words.get(at + 1).copied().ok_or_else(|| layout_error(inst))?);
        at += 2;
        let p_idx = block_plan.get(to_idx(block)).copied().unwrap_or(NONE);
        if p_idx == NONE {
            self.func.extra.push(block);
            self.func.extra.push(to_u32(n));
            for _ in 0..n {
                let arg = words.get(at).copied().ok_or_else(|| layout_error(inst))?;
                self.value(arg, inst);
                at += 1;
            }
            return Ok(at);
        }
        let meta = plans.get(to_idx(p_idx)).copied().ok_or_else(|| layout_error(inst))?;
        let added = to_idx(meta.field_len);
        let new_n = n.checked_sub(1).and_then(|v| v.checked_add(added)).ok_or_else(|| layout_error(inst))?;
        self.func.extra.push(block);
        self.func.extra.push(to_u32(new_n));
        for arg_idx in 0..n {
            let arg = words.get(at).copied().ok_or_else(|| layout_error(inst))?;
            at += 1;
            if arg_idx != meta.param_idx {
                self.value(arg, inst);
            }
        }
        let end = (*cursor).checked_add(added).ok_or_else(|| layout_error(inst))?;
        let slice = edge_loads.get(*cursor..end).ok_or_else(|| layout_error(inst))?;
        self.func.extra.extend_from_slice(slice);
        *cursor = end;
        Ok(at)
    }

    /// Copies input instruction `at` during `promote_slot_addr_phis_round`: keeps slot indices
    /// unchanged, queues fixups for all value operands, and rewrites plain targets targeting a
    /// block with a promoted `ptr_phi`.
    fn copy_for_phi_promo(
        &mut self,
        view: &View<'_>,
        at: usize,
        block_plan: &[u32],
        plans: &[PlanMeta],
        edge_loads: &[u32],
    ) -> Result<u32, SimplifyError> {
        let op = view.op(at);
        let new = to_u32(self.func.ops.len());
        let mut cursor = 0usize;
        let data = match op.info().layout {
            Layout::None => at_u32(view.data, at),
            Layout::Data(Field::Value) => {
                self.fixups.push((new, true, to_u32(at)));
                at_u32(view.data, at)
            }
            Layout::Data(_) => at_u32(view.data, at),
            Layout::Extra(fields) => {
                let start = to_u32(self.func.extra.len());
                let extra_off = to_idx(at_u32(view.data, at));
                let words = view.extra.get(extra_off..).ok_or_else(|| layout_error(at))?;
                let ty = Ty(at_u16(view.tys, at));
                let mut i = 0usize;
                let mut scrutinee = None;
                for &field in fields {
                    match field {
                        Field::Value => {
                            let w = words.get(i).copied().ok_or_else(|| layout_error(at))?;
                            scrutinee = Some(w);
                            self.value(w, at);
                            i += 1;
                        }
                        Field::Slot
                        | Field::Block
                        | Field::Imm
                        | Field::Offset
                        | Field::IntCond
                        | Field::FloatCond
                        | Field::Rmw
                        | Field::Ref
                        | Field::Sig
                        | Field::Intrinsic
                        | Field::Asm => {
                            let w = words.get(i).copied().ok_or_else(|| layout_error(at))?;
                            self.func.extra.push(w);
                            i += 1;
                        }
                        Field::Target => {
                            i = self.target_for_phi_promo(words, i, at, block_plan, plans, edge_loads, &mut cursor)?;
                        }
                        Field::Values => {
                            let n = to_idx(words.get(i).copied().ok_or_else(|| layout_error(at))?);
                            self.func.extra.push(to_u32(n));
                            i += 1;
                            for _ in 0..n {
                                let w = words.get(i).copied().ok_or_else(|| layout_error(at))?;
                                self.value(w, at);
                                i += 1;
                            }
                        }
                        Field::Blocks | Field::Mask => {
                            let n = to_idx(words.get(i).copied().ok_or_else(|| layout_error(at))?);
                            let end = i.checked_add(1).and_then(|v| v.checked_add(n)).ok_or_else(|| layout_error(at))?;
                            let slice = words.get(i..end).ok_or_else(|| layout_error(at))?;
                            self.func.extra.extend_from_slice(slice);
                            i = end;
                        }
                        Field::Const => {
                            let n = to_idx(ty.const_words());
                            let end = i.checked_add(n).ok_or_else(|| layout_error(at))?;
                            let slice = words.get(i..end).ok_or_else(|| layout_error(at))?;
                            self.func.extra.extend_from_slice(slice);
                            i = end;
                        }
                        Field::Cases => {
                            let width =
                                scrutinee.map_or(0, |it: u32| to_idx(Ty(at_u16(view.tys, to_idx(it))).const_words()));
                            let n = to_idx(words.get(i).copied().ok_or_else(|| layout_error(at))?);
                            self.func.extra.push(to_u32(n));
                            i += 1;
                            for _ in 0..n {
                                let end = i.checked_add(width).ok_or_else(|| layout_error(at))?;
                                let slice = words.get(i..end).ok_or_else(|| layout_error(at))?;
                                self.func.extra.extend_from_slice(slice);
                                i = end;
                                i = self.target_for_phi_promo(
                                    words,
                                    i,
                                    at,
                                    block_plan,
                                    plans,
                                    edge_loads,
                                    &mut cursor,
                                )?;
                            }
                        }
                    }
                }
                start
            }
        };
        if cursor != edge_loads.len() {
            return Err(SimplifyError::Refused { inst: at, what: "edges other than the phi-promotion walk saw" });
        }
        Ok(self.raw(op, at_u8(view.flags, at), at_u16(view.tys, at), data, at_u32(view.attrs, at), at_u32(view.locs, at)))
    }
}

/// The last walk: the input in layout order, the replaced slots' instructions dropped, each
/// load of a field replaced by what reaches it, each block given its new parameters and each
/// plain edge its new arguments.
fn emit<A: Allocator + Copy>(
    view: &View<'_>,
    found: &Found<A>,
    phis: &Phis<A>,
    renamed: &Renamed<A>,
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    let (n, blocks) = (view.len(), view.blocks());
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
    let mut slot_row = filled(view.slots.len(), NONE, &alloc);
    for (slot, rec) in view.slots.iter().enumerate() {
        if found.kept[slot] {
            slot_row[slot] = func.slots.len() as u32;
            func.slots.push(*rec);
        }
    }
    for var in view.variables {
        let row = slot_row[var.slot as usize];
        if row != NONE {
            let mut var = *var;
            var.slot = row;
            func.variables.push(var);
        }
    }
    let replaced = |slot: u32| slot_row[slot as usize] == NONE;
    let mut out = Out { func, fixups: Vec::new_in(alloc) };
    // Per input value (and `n + k` for new parameter `k`): its number here.
    let mut map = filled(n + phis.field.len(), NONE, &alloc);
    // Per input value: the value a replaced load stands for, to follow before `map`.
    let mut redirect = filled(n, NONE, &alloc);
    let mut cache: Vec<(u16, u32), A> = Vec::new_in(alloc);
    let mut zero_cache: Vec<(u16, u32), A> = Vec::new_in(alloc);
    let mut extras: Vec<(u32, bool), A> = Vec::new_in(alloc);
    for b in 0..blocks as u32 {
        out.func.block_starts.push(out.func.ops.len() as u32);
        out.func.block_kinds.push(view.block_kinds[b as usize]);
        cache.clear();
        zero_cache.clear();
        let params = view.params(Block(b));
        for at in params.start..params.end {
            map[at] = out.raw(Op::Param, view.flags[at], view.tys[at], view.data[at], view.attrs[at], view.locs[at]);
        }
        for k in phis.of(b) {
            let ty = found.fields[phis.field[k as usize] as usize].ty;
            map[n + k as usize] = out.raw(Op::Param, 0, ty.0, 0, 0, 0);
        }
        for at in params.end..view.block(Block(b)).end {
            let op = view.op(at);
            let loc = view.locs[at];
            match op {
                Op::SlotAddr | Op::SlotLive | Op::SlotDead if replaced(view.data[at]) => continue,
                Op::PtrAdd if root(view, at as u32).is_some_and(replaced) => continue,
                _ => {}
            }
            let i = found.access_of.get(at).copied().unwrap_or(NONE);
            if i != NONE {
                if let Some(access) = found.accesses.get(to_idx(i)).copied() {
                    if !at_bool(&found.kept, to_idx(access.slot)) {
                        let field_ty = found.fields.get(to_idx(access.field)).map_or(access.ty, |r| r.ty);
                        match access.kind {
                            Kind::Load => {
                                let def = renamed.load_def.get(to_idx(i)).copied().unwrap_or(Def::Undef);
                                match def {
                                    Def::Old(value) => {
                                        if access.ty == field_ty {
                                            set_at(&mut redirect, at, value);
                                        } else {
                                            let conv = out.raw(Op::PtrAddr, 0, at_u16(view.tys, at), value, 0, loc);
                                            out.fixups.push((conv, true, to_u32(at)));
                                            set_at(&mut map, at, conv);
                                        }
                                    }
                                    Def::Phi(k) => {
                                        let phi_id = to_u32(n) + k;
                                        if access.ty == field_ty {
                                            set_at(&mut redirect, at, phi_id);
                                        } else {
                                            let conv = out.raw(Op::PtrAddr, 0, at_u16(view.tys, at), phi_id, 0, loc);
                                            out.fixups.push((conv, true, to_u32(at)));
                                            set_at(&mut map, at, conv);
                                        }
                                    }
                                    Def::Undef => {
                                        let u = out.undef(&mut cache, Ty(at_u16(view.tys, at)), loc);
                                        set_at(&mut map, at, u);
                                    }
                                    Def::Zero => {
                                        let z = out.zero(&mut zero_cache, Ty(at_u16(view.tys, at)), loc);
                                        set_at(&mut map, at, z);
                                    }
                                }
                            }
                            Kind::Store { value, .. } if access.ty != field_ty => {
                                let conv = if constant(view, value) == Some(0) {
                                    out.raw(Op::Null, 0, Ty::PTR.0, 0, 0, loc)
                                } else {
                                    let c = out.raw(Op::FromExposed, 0, Ty::PTR.0, value, 0, loc);
                                    out.fixups.push((c, true, to_u32(at)));
                                    c
                                };
                                set_at(&mut map, at, conv);
                            }
                            _ => {}
                        }
                        continue;
                    }
                }
            }
            extras.clear();
            if op.is_terminator() {
                let mut cursor = renamed.edge_at[b as usize];
                let reached = cursor != NONE;
                each_edge(view, at, |to, plain| {
                    if !plain {
                        return;
                    }
                    for k in phis.of(to) {
                        let def = match reached {
                            true => {
                                cursor += 1;
                                renamed.edge_args[cursor as usize - 1]
                            }
                            false => Def::Undef,
                        };
                        extras.push(match def {
                            Def::Old(value) => (value, true),
                            Def::Phi(param) => (n as u32 + param, true),
                            Def::Undef => {
                                let ty = found.fields[phis.field[k as usize] as usize].ty;
                                (out.undef(&mut cache, ty, loc), false)
                            }
                            Def::Zero => {
                                let ty = found.fields[phis.field[k as usize] as usize].ty;
                                (out.zero(&mut zero_cache, ty, loc), false)
                            }
                        });
                    }
                })?;
            }
            map[at] = out.copy(view, at, phis, &slot_row, &extras)?;
        }
    }
    // Every input value named in a copied operand becomes its number here.
    let Out { mut func, fixups } = out;
    for &(place, in_data, inst) in fixups.iter() {
        let word = match in_data {
            true => &mut func.data[place as usize],
            false => &mut func.extra[place as usize],
        };
        let mut id = *word;
        // A replaced load stands for a value that dominates it: a chain shorter than the
        // function.
        for _ in 0..=n {
            match (id as usize) < n && redirect[id as usize] != NONE {
                true => id = redirect[id as usize],
                false => break,
            }
        }
        let to = map[id as usize];
        if to == NONE {
            return Err(SimplifyError::Refused { inst: inst as usize, what: "a use of a value the rewrite removed" });
        }
        *word = to;
    }
    if u32::try_from(func.ops.len()).is_err() || u32::try_from(func.extra.len()).is_err() {
        return Err(SimplifyError::Refused { inst: 0, what: "an output past LIR's columns" });
    }
    // As the builder does: the output is checked in debug builds.
    if cfg!(debug_assertions) {
        verify(func.view()).map_err(SimplifyError::Verify)?;
    }
    Ok(func)
}

#[cfg(test)]
mod copy_tests {
    use super::*;
    use crate::{
        BlockKind, Builder, Full, IntCond,
        builder::{MemAttrs, Param, Variadic},
        eval::{Outcome, Val, run_in},
        func::{Conv, SlotKind},
        print,
    };

    const FUEL: u64 = 100_000;

    #[test]
    fn memcpy_between_range_slots_promotes_completely() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let start = b.param(Ty::I64);
        let end = b.param(Ty::I64);
        let range_slot = b.slot(16, 3, SlotKind::Temp);
        let iter_slot = b.slot(16, 3, SlotKind::Temp);
        let p_range = b.slot_addr(range_slot);
        let p_iter = b.slot_addr(iter_slot);
        let off8 = b.iconst(Ty::I64, 8);
        let p_range_end = b.ptr_add(Flags::INBOUNDS, p_range, off8);
        b.store(Flags::NONE, start, p_range, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, end, p_range_end, 0, MemAttrs::aligned(3));
        let len16 = b.iconst(Ty::I64, 16);
        b.bulk(Op::Memcpy, Flags::NONE, p_iter, p_range, len16, MemAttrs::aligned(3));
        let a = b.load(Ty::I64, Flags::NONE, p_iter, 0, MemAttrs::aligned(3));
        let z = b.load(Ty::I64, Flags::NONE, p_iter, 8, MemAttrs::aligned(3));
        let diff = b.binary(Op::Sub, Flags::NONE, z, a);
        b.ret(&[diff]);
        let func = b.finish_checked().expect("valid input");

        let out = scalar_replace(func.view(), alloc).expect("sroa succeeds");
        verify(out.view()).expect("output verifies");
        assert_eq!(out.slots.len(), 0, "both slots must be promoted:\n{}", print(out.view()));
        for inst in 0..out.view().len() {
            assert!(
                !matches!(out.view().op(inst), Op::Memcpy | Op::Load | Op::Store | Op::SlotAddr),
                "unexpected memory op {}:\n{}",
                out.view().op(inst).name(),
                print(out.view())
            );
        }
        let before = run_in(func.view(), &[Val::Int(3), Val::Int(11)], FUEL, alloc).expect("eval input");
        let after = run_in(out.view(), &[Val::Int(3), Val::Int(11)], FUEL, alloc).expect("eval output");
        assert_eq!(before, after);
        assert!(matches!(after, Outcome::Returned(ref vals) if vals[..] == [Val::Int(8)]));
    }

    #[test]
    fn non_volatile_inbounds_load_store_promote_completely() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let slot = b.slot(16, 3, SlotKind::Temp);
        let p = b.slot_addr(slot);
        b.store(Flags::INBOUNDS, x, p, 0, MemAttrs::aligned(3));
        b.store(Flags::INBOUNDS, y, p, 8, MemAttrs::aligned(3));
        let a = b.load(Ty::I64, Flags::INBOUNDS, p, 0, MemAttrs::aligned(3));
        let z = b.load(Ty::I64, Flags::INBOUNDS, p, 8, MemAttrs::aligned(3));
        let diff = b.binary(Op::Sub, Flags::NONE, z, a);
        b.ret(&[diff]);
        let func = b.finish_checked().expect("valid input");

        let out = scalar_replace(func.view(), alloc).expect("sroa succeeds");
        verify(out.view()).expect("output verifies");
        assert_eq!(out.slots.len(), 0, "inbounds slot must be promoted:\n{}", print(out.view()));
        let after = run_in(out.view(), &[Val::Int(3), Val::Int(11)], FUEL, alloc).expect("eval output");
        assert!(matches!(after, Outcome::Returned(ref vals) if vals[..] == [Val::Int(8)]));
    }

    #[test]
    fn partially_written_option_copied_via_memcpy_promotes_without_ub() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, some_blk, none_blk, join, use_val, exit] =
            [(); 6].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let flag = b.param(Ty::I64);
        let x = b.param(Ty::I64);
        let tmp_opt = b.slot(16, 3, SlotKind::Temp);
        let dst_opt = b.slot(16, 3, SlotKind::Temp);
        let zero = b.iconst(Ty::I64, 0);
        let is_some = b.icmp(IntCond::Ne, flag, zero);
        b.br(Flags::NONE, is_some, (some_blk, &[]), (none_blk, &[]));

        b.begin(some_blk);
        let p_tmp = b.slot_addr(tmp_opt);
        let one = b.iconst(Ty::I64, 1);
        b.store(Flags::NONE, one, p_tmp, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, x, p_tmp, 8, MemAttrs::aligned(3));
        b.jump((join, &[]));

        b.begin(none_blk);
        let p_tmp2 = b.slot_addr(tmp_opt);
        let z0 = b.iconst(Ty::I64, 0);
        // Only tag written; payload at +8 is intentionally uninitialized!
        b.store(Flags::NONE, z0, p_tmp2, 0, MemAttrs::aligned(3));
        b.jump((join, &[]));

        b.begin(join);
        let p_src = b.slot_addr(tmp_opt);
        let p_dst = b.slot_addr(dst_opt);
        let len16 = b.iconst(Ty::I64, 16);
        b.bulk(Op::Memcpy, Flags::NONE, p_dst, p_src, len16, MemAttrs::aligned(3));
        let tag = b.load(Ty::I64, Flags::NONE, p_dst, 0, MemAttrs::aligned(3));
        let c1 = b.iconst(Ty::I64, 1);
        let has_val = b.icmp(IntCond::Eq, tag, c1);
        b.br(Flags::NONE, has_val, (use_val, &[]), (exit, &[]));

        b.begin(use_val);
        let p_dst2 = b.slot_addr(dst_opt);
        let payload = b.load(Ty::I64, Flags::NONE, p_dst2, 8, MemAttrs::aligned(3));
        let ten = b.iconst(Ty::I64, 10);
        let res = b.binary(Op::Add, Flags::NONE, payload, ten);
        b.ret(&[res]);

        b.begin(exit);
        let ret0 = b.iconst(Ty::I64, 99);
        b.ret(&[ret0]);

        let func = b.finish_checked().expect("valid input");
        let out = scalar_replace(func.view(), alloc).expect("sroa succeeds");
        verify(out.view()).expect("output verifies");
        assert_eq!(out.slots.len(), 0, "all option slots promoted:\n{}", print(out.view()));
        for args in [[1, 32], [0, 32]] {
            let vals = args.map(Val::Int);
            let before = run_in(func.view(), &vals, FUEL, alloc).expect("eval input");
            let after = run_in(out.view(), &vals, FUEL, alloc).expect("eval output");
            assert_eq!(before, after, "mismatch for args {args:?}");
        }
    }

    #[test]
    fn memset_zero_initializes_promoted_fields() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);
        let s = b.slot(16, 3, SlotKind::Temp);
        let p = b.slot_addr(s);
        let z8 = b.iconst(Ty::I8, 0);
        let len16 = b.iconst(Ty::I64, 16);
        b.bulk(Op::Memset, Flags::NONE, p, z8, len16, MemAttrs::aligned(3));
        b.store(Flags::NONE, x, p, 8, MemAttrs::aligned(3));
        let f0 = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
        let f1 = b.load(Ty::I64, Flags::NONE, p, 8, MemAttrs::aligned(3));
        let sum = b.binary(Op::Add, Flags::NONE, f0, f1);
        b.ret(&[sum]);
        let func = b.finish_checked().expect("valid input");

        let out = scalar_replace(func.view(), alloc).expect("sroa succeeds");
        verify(out.view()).expect("output verifies");
        assert_eq!(out.slots.len(), 0, "memset slot promoted:\n{}", print(out.view()));
        let after = run_in(out.view(), &[Val::Int(42)], FUEL, alloc).expect("eval output");
        assert!(matches!(after, Outcome::Returned(ref vals) if vals[..] == [Val::Int(42)]));
    }

    #[test]
    fn promotes_niche_option_ptr_stored_i64_loaded() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::PTR)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, some_blk, none_blk, join, some_arm, none_arm] =
            [(); 6].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let is_some = b.param(Ty::I64);
        let elem_ptr = b.param(Ty::PTR);
        let opt_slot = b.slot(8, 3, SlotKind::Temp);
        let z0 = b.iconst(Ty::I64, 0);
        let cond = b.icmp(IntCond::Ne, is_some, z0);
        b.br(Flags::NONE, cond, (some_blk, &[]), (none_blk, &[]));

        // `Some(&T)` stores `Ty::PTR` at offset 0.
        b.begin(some_blk);
        let p_some = b.slot_addr(opt_slot);
        b.store(Flags::NONE, elem_ptr, p_some, 0, MemAttrs::aligned(3));
        b.jump((join, &[]));

        // `None` stores `0i64` (`Ty::I64`) at offset 0.
        b.begin(none_blk);
        let p_none = b.slot_addr(opt_slot);
        let zero_tag = b.iconst(Ty::I64, 0);
        b.store(Flags::NONE, zero_tag, p_none, 0, MemAttrs::aligned(3));
        b.jump((join, &[]));

        // `match` reads the niche discriminant as `Ty::I64` at offset 0.
        b.begin(join);
        let p_join = b.slot_addr(opt_slot);
        let disc = b.load(Ty::I64, Flags::NONE, p_join, 0, MemAttrs::aligned(3));
        let z1 = b.iconst(Ty::I64, 0);
        let non_null = b.icmp(IntCond::Ne, disc, z1);
        b.br(Flags::NONE, non_null, (some_arm, &[]), (none_arm, &[]));

        // `Some` arm reads the payload back as `Ty::PTR` at offset 0.
        b.begin(some_arm);
        let p_arm = b.slot_addr(opt_slot);
        let loaded_ptr = b.load(Ty::PTR, Flags::NONE, p_arm, 0, MemAttrs::aligned(3));
        let addr = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, loaded_ptr);
        b.ret(&[addr]);

        b.begin(none_arm);
        let ret_zero = b.iconst(Ty::I64, 0);
        b.ret(&[ret_zero]);

        let func = b.finish_checked().expect("valid input");
        let out = scalar_replace(func.view(), alloc).expect("sroa succeeds");
        verify(out.view()).expect("output verifies");
        assert_eq!(out.slots.len(), 0, "niche Option<&T> slot must promote:\n{}", print(out.view()));
        let mut saw_ptr_addr = false;
        for inst in 0..out.view().len() {
            let op = out.view().op(inst);
            assert!(
                !matches!(op, Op::Load | Op::Store | Op::SlotAddr),
                "unexpected memory op {}:\n{}",
                op.name(),
                print(out.view())
            );
            if op == Op::PtrAddr {
                saw_ptr_addr = true;
            }
        }
        assert!(saw_ptr_addr, "expected Op::PtrAddr for ptr->i64 niche load:\n{}", print(out.view()));

        let non_null_ptr = Val::Ptr { slot: Val::NO_SLOT, offset: 0x1000 };
        for args in [[Val::Int(1), non_null_ptr], [Val::Int(0), non_null_ptr]] {
            let before = run_in(func.view(), &args, FUEL, alloc).expect("eval input");
            let after = run_in(out.view(), &args, FUEL, alloc).expect("eval output");
            assert_eq!(before, after, "mismatch for args {args:?}");
        }
    }

    #[test]
    fn promotes_two_slot_range_next_ptr_phi() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, then_blk, else_blk, merge, some_arm, none_arm] =
            [(); 6].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let i = b.param(Ty::I64);
        let n = b.param(Ty::I64);
        let slot_a = b.slot(16, 3, SlotKind::Temp);
        let slot_b = b.slot(16, 3, SlotKind::Temp);
        let cond = b.icmp(IntCond::Slt, i, n);
        b.br(Flags::NONE, cond, (then_blk, &[]), (else_blk, &[]));

        // `Some(i)` writes `(1, i)` into `slot_a` and passes `SlotAddr(slot_a)` to `merge`.
        b.begin(then_blk);
        let pa = b.slot_addr(slot_a);
        let one = b.iconst(Ty::I64, 1);
        b.store(Flags::NONE, one, pa, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, i, pa, 8, MemAttrs::aligned(3));
        b.jump((merge, &[pa]));

        // `None` writes only `(0)` into `slot_b` (+8 intentionally uninitialized) and passes `SlotAddr(slot_b)`.
        b.begin(else_blk);
        let pb = b.slot_addr(slot_b);
        let zero = b.iconst(Ty::I64, 0);
        b.store(Flags::NONE, zero, pb, 0, MemAttrs::aligned(3));
        b.jump((merge, &[pb]));

        // `merge(ptr_phi)` loads `[ptr_phi + 0]` and branches to `some_arm` / `none_arm`.
        b.begin(merge);
        let ptr_phi = b.param(Ty::PTR);
        let tag = b.load(Ty::I64, Flags::NONE, ptr_phi, 0, MemAttrs::aligned(3));
        let c1 = b.iconst(Ty::I64, 1);
        let is_some = b.icmp(IntCond::Eq, tag, c1);
        b.br(Flags::NONE, is_some, (some_arm, &[]), (none_arm, &[]));

        // `tag == 1` arm loads `[ptr_phi + 8]` through `PtrAdd`.
        b.begin(some_arm);
        let off8 = b.iconst(Ty::I64, 8);
        let payload_ptr = b.ptr_add(Flags::NONE, ptr_phi, off8);
        let val = b.load(Ty::I64, Flags::NONE, payload_ptr, 0, MemAttrs::aligned(3));
        let ten = b.iconst(Ty::I64, 10);
        let res = b.binary(Op::Add, Flags::NONE, val, ten);
        b.ret(&[res]);

        b.begin(none_arm);
        let ret99 = b.iconst(Ty::I64, 99);
        b.ret(&[ret99]);

        let func = b.finish_checked().expect("valid input");
        let out = scalar_replace(func.view(), alloc).expect("sroa succeeds");
        verify(out.view()).expect("output verifies");
        assert_eq!(out.slots.len(), 0, "both range_next slots must promote:\n{}", print(out.view()));
        for inst in 0..out.view().len() {
            let op = out.view().op(inst);
            assert!(
                !matches!(op, Op::Load | Op::Store | Op::SlotAddr),
                "unexpected memory op {}:\n{}",
                op.name(),
                print(out.view())
            );
        }
        for args in [[5, 10], [10, 10]] {
            let vals = args.map(Val::Int);
            let before = run_in(func.view(), &vals, FUEL, alloc).expect("eval input");
            let after = run_in(out.view(), &vals, FUEL, alloc).expect("eval output");
            assert_eq!(before, after, "mismatch for args {args:?}");
        }
    }
}
