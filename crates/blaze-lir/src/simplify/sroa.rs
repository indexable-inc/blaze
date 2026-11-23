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
    Block, Func, Op, Ty, View,
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
    let cfg = Cfg::of(&view, &alloc)?;
    let mut found = find(&view, &cfg, &alloc)?;
    if found.kept.iter().all(|&k| k) {
        if cfg!(debug_assertions) {
            verify(view).map_err(SimplifyError::Verify)?;
        }
        return Ok(view.to_func_in(alloc));
    }
    let phis = place(&view, &cfg, &mut found, &alloc);
    if found.kept.iter().all(|&k| k) {
        if cfg!(debug_assertions) {
            verify(view).map_err(SimplifyError::Verify)?;
        }
        return Ok(view.to_func_in(alloc));
    }
    // Input values and new parameters share one numbering while the output is written.
    if view.len() + phis.field.len() >= NONE as usize {
        return Err(SimplifyError::Refused { inst: 0, what: "more values than LIR numbers" });
    }
    let renamed = rename(&view, &cfg, &found, &phis, plant, &alloc)?;
    emit(&view, &found, &phis, &renamed, alloc)
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Load,
    /// A store of `value`.
    Store { value: u32 },
    /// A `slot_live` or `slot_dead`: every field of the slot is undefined after it.
    Reset,
}

/// A load, store or reset of a slot, in a reachable block.
#[derive(Clone, Copy, Debug)]
struct Access {
    slot: u32,
    /// Bytes from the slot's start (0 for a reset).
    offset: i128,
    /// The type loaded or stored ([`Ty::NONE`] for a reset).
    ty: Ty,
    kind: Kind,
    /// Its field, once fields are known; [`NONE`] for a reset or a kept slot's access.
    field: u32,
}

#[derive(Clone, Copy, Debug)]
struct FieldRec {
    slot: u32,
    ty: Ty,
}

/// What the first walk found.
struct Found<A: Allocator> {
    /// Per slot: kept in memory (its address escapes, or it cannot be replaced).
    kept: Vec<bool, A>,
    accesses: Vec<Access, A>,
    /// Per block reached, its accesses: `accesses[start..end]`.
    block_acc: Vec<(u32, u32), A>,
    /// Per instruction: its access, for a load or store of a slot; else [`NONE`].
    access_of: Vec<u32, A>,
    fields: Vec<FieldRec, A>,
    /// Per slot: its fields, `fields[first..first + count]`.
    slot_fields: Vec<(u32, u32), A>,
}

impl<A: Allocator> Found<A> {
    fn fields_of(&self, slot: u32) -> core::ops::Range<u32> {
        let (first, count) = self.slot_fields[slot as usize];
        first..first + count
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
        let flagless = view.flags[at] == 0;
        let mut o = view.operands(at);
        let allowed = match (op, k) {
            (Op::Load, 0) if flagless => {
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
            (Op::Store, 1) if flagless => {
                let stored = o.value().0;
                o.value();
                let ty = Ty(view.tys[stored as usize]);
                match (ty.is_scalar(), base.checked_add(i128::from(o.offset()))) {
                    (true, Some(offset)) => {
                        self.access(at, slot, offset, ty, Kind::Store { value: stored });
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
    // The loads and stores of slots not kept, by slot, offset and type.
    let mut order = Vec::new_in(*alloc);
    for (i, access) in accesses.iter().enumerate() {
        if access.kind != Kind::Reset && !kept[access.slot as usize] {
            order.push(i as u32);
        }
    }
    order.sort_unstable_by_key(|&i| {
        let access = &accesses[i as usize];
        (access.slot, access.offset, access.ty.0, i)
    });
    // A slot whose accesses leave it, or overlap without being the same field, is kept.
    let mut last: Option<(u32, i128, u16, i128)> = None;
    for &i in order.iter() {
        let access = accesses[i as usize];
        let size = bytes(access.ty, view.head.ptr_bits);
        let end = access.offset + size;
        if access.offset < 0 || end > i128::from(view.slots[access.slot as usize].size) {
            kept[access.slot as usize] = true;
        }
        last = match last {
            Some((slot, offset, ty, reach)) if slot == access.slot => {
                if (offset, ty) != (access.offset, access.ty.0) && reach > access.offset {
                    kept[slot as usize] = true;
                }
                Some((slot, access.offset, access.ty.0, reach.max(end)))
            }
            _ => Some((access.slot, access.offset, access.ty.0, end)),
        };
    }
    // The fields of the slots not kept, in (slot, offset, type) order.
    let mut fields = Vec::new_in(*alloc);
    let mut slot_fields = filled(slots, (0u32, 0u32), alloc);
    let mut last: Option<(u32, i128, u16)> = None;
    for &i in order.iter() {
        let access = accesses[i as usize];
        if kept[access.slot as usize] {
            continue;
        }
        let key = (access.slot, access.offset, access.ty.0);
        if last != Some(key) {
            if slot_fields[access.slot as usize].1 == 0 {
                slot_fields[access.slot as usize].0 = fields.len() as u32;
            }
            slot_fields[access.slot as usize].1 += 1;
            fields.push(FieldRec { slot: access.slot, ty: access.ty });
            last = Some(key);
        }
        accesses[i as usize].field = fields.len() as u32 - 1;
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

/// What reaches a load or an edge: the input's value, a new parameter, or nothing written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Def {
    Undef,
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
                    Kind::Store { value } => {
                        let f = access.field as usize;
                        // Planted (test builds only): a second store is not seen.
                        let stale = cfg!(test) && plant == Plant::KeepStaleStore && matches!(cur[f], Def::Old(_));
                        if !stale {
                            undo.push((access.field, cur[f]));
                            cur[f] = Def::Old(value);
                        }
                    }
                    Kind::Reset => {
                        for f in found.fields_of(access.slot) {
                            undo.push((f, cur[f as usize]));
                            cur[f as usize] = Def::Undef;
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
    let mut extras: Vec<(u32, bool), A> = Vec::new_in(alloc);
    for b in 0..blocks as u32 {
        out.func.block_starts.push(out.func.ops.len() as u32);
        out.func.block_kinds.push(view.block_kinds[b as usize]);
        cache.clear();
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
            let i = found.access_of[at];
            if i != NONE && !found.kept[found.accesses[i as usize].slot as usize] {
                if found.accesses[i as usize].kind == Kind::Load {
                    match renamed.load_def[i as usize] {
                        Def::Old(value) => redirect[at] = value,
                        Def::Phi(k) => redirect[at] = n as u32 + k,
                        Def::Undef => map[at] = out.undef(&mut cache, Ty(view.tys[at]), loc),
                    }
                }
                continue;
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
