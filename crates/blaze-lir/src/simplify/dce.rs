//! Dead code and trivial branches (docs/v2/plan/design/backend-next.md, D100 point 1: the
//! last pass of the linear simplify stage), and the one rewrite every pass of the stage makes
//! its output with.
//!
//! [`dead`]: a `br` on a constant, a `br` whose two targets are one block with one argument
//! list, and a `switch` on a constant become a `jump`; blocks no edge from the entry reaches
//! go; an instruction whose value nothing live uses goes when it has no effect (a list of
//! operations by name, [`removable`]: an operation not on it is kept, never guessed at); a
//! block parameter nothing live uses goes with its argument on every edge, when every edge
//! into its block is a `jump`/`br`/`switch` target (an unwind or `invoke` edge's block and
//! the entry keep theirs). Liveness is one mark from the effects over a work list, a
//! parameter's arguments marked only when the parameter is.
//!
//! [`rebuild`]: the function's tables copied whole, its instruction and block columns made
//! again from an [`Edit`]: which blocks and instructions stay, which value stands for which,
//! which instructions become an integer constant, which terminators become a `jump`. A kept
//! instruction is one instruction of the output, so every value's new number is known before
//! the copy and no use waits on a fixup. The output is verified ([`verify`]) before it is
//! returned.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::SimplifyError;
use crate::{
    Block, Flags, Func, Op, Ty, View,
    arith::{iconst_value, sext, wide_value},
    op::{Field, Layout},
    verify,
};

/// No value, block or target.
pub(super) const NONE: u32 = u32::MAX;

/// One edge out of a terminator.
#[derive(Clone, Copy, Debug)]
pub(super) struct Edge {
    /// The block it leads to.
    pub(super) block: u32,
    /// For a block-and-arguments target: where its arguments start in `extra`, and how many.
    /// `None` for an edge with no arguments (an unwind edge, an `invoke`'s normal edge, an
    /// `asm_br` label).
    pub(super) args: Option<(usize, usize)>,
    /// For a target: its place among the instruction's targets (a `br`'s `then` 0 and
    /// `otherwise` 1; a `switch`'s default 0 and case `k` `k + 1`). [`NONE`] otherwise.
    pub(super) ordinal: u32,
}

/// One operand of an instruction, as [`each`] hands it on.
#[derive(Clone, Copy, Debug)]
pub(super) enum Item {
    /// A value read by the instruction itself (not a target's argument).
    Value(u32),
    /// An edge, its arguments with it.
    Edge(Edge),
}

/// Hands `f` every value the instruction `inst` reads and every edge it has, in operand
/// order. The mirror of [`crate::op::walk`] with positions, on a verified function.
pub(super) fn each(view: &View<'_>, inst: usize, mut f: impl FnMut(Item)) {
    let fields = match view.op(inst).info().layout {
        Layout::None => return,
        Layout::Data(Field::Value) => return f(Item::Value(view.data[inst])),
        Layout::Data(_) => return,
        Layout::Extra(fields) => fields,
    };
    let words = view.extra;
    let mut at = view.data[inst] as usize;
    let mut ordinal = 0u32;
    let mut scrutinee = None;
    let target = |at: &mut usize, ordinal: &mut u32, f: &mut dyn FnMut(Item)| {
        let block = words[*at];
        let n = words[*at + 1] as usize;
        f(Item::Edge(Edge { block, args: Some((*at + 2, n)), ordinal: *ordinal }));
        *ordinal += 1;
        *at += 2 + n;
    };
    for &field in fields {
        match field {
            Field::Value => {
                scrutinee = Some(words[at]);
                f(Item::Value(words[at]));
                at += 1;
            }
            Field::Block => {
                f(Item::Edge(Edge { block: words[at], args: None, ordinal: NONE }));
                at += 1;
            }
            Field::Imm
            | Field::Offset
            | Field::IntCond
            | Field::FloatCond
            | Field::Rmw
            | Field::Ref
            | Field::Sig
            | Field::Slot
            | Field::Intrinsic
            | Field::Asm => at += 1,
            Field::Target => target(&mut at, &mut ordinal, &mut f),
            Field::Values => {
                let n = words[at] as usize;
                for k in 0..n {
                    f(Item::Value(words[at + 1 + k]));
                }
                at += 1 + n;
            }
            Field::Blocks => {
                let n = words[at] as usize;
                for k in 0..n {
                    f(Item::Edge(Edge { block: words[at + 1 + k], args: None, ordinal: NONE }));
                }
                at += 1 + n;
            }
            Field::Mask => at += 1 + words[at] as usize,
            Field::Const => at += Ty(view.tys[inst]).const_words() as usize,
            Field::Cases => {
                let width = scrutinee.map_or(0, |it: u32| Ty(view.tys[it as usize]).const_words()) as usize;
                let n = words[at] as usize;
                at += 1;
                for _ in 0..n {
                    at += width;
                    target(&mut at, &mut ordinal, &mut f);
                }
            }
        }
    }
}

/// `len` copies of `item` in `alloc`.
pub(super) fn filled<T: Clone, A: Allocator + Copy>(len: usize, item: T, alloc: &A) -> Vec<T, A> {
    Vec::from_elem_in(item, len, *alloc)
}

/// Per block, whether an edge path from the entry reaches it; a terminator with a `jump`
/// choice (`jump[inst] != NONE`) has only that target as an edge. A work list.
pub(super) fn reachable<A: Allocator + Copy>(view: &View<'_>, jump: &[u32], alloc: &A) -> Vec<bool, A> {
    let mut seen = filled(view.blocks(), false, alloc);
    let mut work: Vec<u32, A> = Vec::with_capacity_in(view.blocks(), *alloc);
    seen[0] = true;
    work.push(0);
    while let Some(b) = work.pop() {
        let end = view.terminator(Block(b));
        let chosen = jump[end];
        each(view, end, |item| {
            if let Item::Edge(edge) = item {
                if (chosen == NONE || edge.ordinal == chosen) && !seen[edge.block as usize] {
                    seen[edge.block as usize] = true;
                    work.push(edge.block);
                }
            }
        });
    }
    seen
}

/// The edges into each block from the reached blocks, as their arguments' places in `extra`
/// (CSR), and whether every such edge is a target with arguments.
pub(super) struct Incoming<A: Allocator> {
    starts: Vec<u32, A>,
    args: Vec<u32, A>,
    /// Per block: every edge into it is a target with arguments, and it is not the entry: its
    /// parameters may be dropped with their arguments.
    pub(super) targets_only: Vec<bool, A>,
}

impl<A: Allocator> Incoming<A> {
    /// Where the arguments of each edge into `block` start in `extra`.
    pub(super) fn edges_into(&self, block: usize) -> &[u32] {
        &self.args[self.starts[block] as usize..self.starts[block + 1] as usize]
    }
}

pub(super) fn incoming<A: Allocator + Copy>(
    view: &View<'_>,
    reached: &[bool],
    jump: &[u32],
    alloc: &A,
) -> Incoming<A> {
    let blocks = view.blocks();
    let mut counts = filled(blocks + 1, 0u32, alloc);
    let mut targets_only = filled(blocks, true, alloc);
    targets_only[0] = false;
    let live = |b: usize, edge: &Edge| reached[b] && (jump[view.terminator(Block(b as u32))] == NONE || edge.ordinal == jump[view.terminator(Block(b as u32))]);
    for b in (0..blocks).filter(|&b| reached[b]) {
        each(view, view.terminator(Block(b as u32)), |item| {
            if let Item::Edge(edge) = item {
                if !live(b, &edge) {
                    return;
                }
                match edge.args {
                    Some(_) => counts[edge.block as usize + 1] += 1,
                    None => targets_only[edge.block as usize] = false,
                }
            }
        });
    }
    for b in 0..blocks {
        counts[b + 1] += counts[b];
    }
    let starts = counts;
    let mut fill = filled(blocks, 0u32, alloc);
    let mut args = filled(starts[blocks] as usize, 0u32, alloc);
    for b in (0..blocks).filter(|&b| reached[b]) {
        each(view, view.terminator(Block(b as u32)), |item| {
            if let Item::Edge(edge) = item {
                if let (true, Some((at, _))) = (live(b, &edge), edge.args) {
                    let to = edge.block as usize;
                    args[(starts[to] + fill[to]) as usize] = at as u32;
                    fill[to] += 1;
                }
            }
        });
    }
    Incoming { starts, args, targets_only }
}

/// Per instruction, its block.
pub(super) fn owners<A: Allocator + Copy>(view: &View<'_>, alloc: &A) -> Vec<u32, A> {
    let mut owner = filled(view.len(), NONE, alloc);
    for b in 0..view.blocks() {
        for inst in view.block(Block(b as u32)) {
            owner[inst] = b as u32;
        }
    }
    owner
}

/// The integer value of `value` when an `iconst` or `iconst_wide` of a scalar integer type
/// makes it: the evaluator's reading of the constant ([`iconst_value`], [`wide_value`]).
pub(super) fn constant_of(view: &View<'_>, value: u32) -> Option<u128> {
    let at = value as usize;
    let ty = Ty(view.tys[at]);
    if !ty.is_scalar() || !ty.is_int() || view.flags(at) != Flags::NONE {
        return None;
    }
    let width = ty.bits(view.head.ptr_bits);
    match view.op(at) {
        Op::Iconst => Some(iconst_value(view.data[at], width)),
        Op::IconstWide => {
            let start = view.data[at] as usize;
            Some(wide_value(&view.extra[start..start + ty.const_words() as usize], width))
        }
        _ => None,
    }
}

/// What a rewrite keeps and changes: [`rebuild`]'s input.
pub(super) struct Edit<A: Allocator> {
    /// Per block: it stays.
    pub(super) block_kept: Vec<bool, A>,
    /// Per instruction: it stays (a parameter's arguments go with it).
    pub(super) inst_kept: Vec<bool, A>,
    /// Per value: the value every use of it reads instead, itself if none; never a value
    /// that itself stands for another.
    pub(super) stands: Vec<u32, A>,
    /// Per instruction: the integer it becomes (an `iconst` or `iconst_wide` of its type).
    pub(super) constant: Vec<Option<u128>, A>,
    /// Per instruction: [`NONE`], or for a `br` or `switch` the target ([`Edge::ordinal`])
    /// it becomes a `jump` to.
    pub(super) jump: Vec<u32, A>,
    /// Per instruction: [`NONE`], or for a `call_indirect` the `RefId` (of `RefKind::Func`)
    /// it devirtualizes into a direct `call` of.
    pub(super) devirt: Vec<u32, A>,
    /// Per block: [`NONE`], or the sole-predecessor block `B` coalesced into this block across
    /// its `jump` terminator (dropping this block's `jump` and `B`'s parameters).
    pub(super) merge_next: Vec<u32, A>,
    /// Per instruction: `Some([a, b, c])` when an `fadd(fmul(a, b), c)` with `Flags::CONTRACT`
    /// is fused into a single hardware `Op::Fma(a, b, c)`.
    pub(super) fma_fuse: Vec<Option<[u32; 3]>, A>,
}

/// The function `view` with `edit` made, in `alloc`, verified.
pub(super) fn rebuild<A: Allocator + Copy>(view: View<'_>, edit: &Edit<A>, alloc: A) -> Result<Func<A>, SimplifyError> {
    if !edit.block_kept[0] {
        return Err(SimplifyError::Refused { inst: 0, what: "a rewrite without the entry block" });
    }
    let blocks = view.blocks();
    let len = view.len();
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

    let has_merges = edit.merge_next.iter().any(|&m| m != NONE);
    if !has_merges {
        // Fast path: kept instructions and blocks keep their order: their new numbers are their ranks.
        let mut new_inst = filled(len, NONE, &alloc);
        let mut next = 0u32;
        for inst in 0..len {
            if edit.inst_kept[inst] {
                new_inst[inst] = next;
                next += 1;
            }
        }
        let mut new_block = filled(blocks, NONE, &alloc);
        let mut next = 0u32;
        for b in 0..blocks {
            if edit.block_kept[b] {
                new_block[b] = next;
                next += 1;
            }
        }
        let mut out = Out { view, edit, stands: &edit.stands, new_inst, new_block, func };
        for b in (0..blocks).filter(|&b| edit.block_kept[b]) {
            out.func.block_starts.push(out.func.ops.len() as u32);
            out.func.block_kinds.push(view.block_kinds[b]);
            for inst in view.block(Block(b as u32)).filter(|&inst| edit.inst_kept[inst]) {
                out.inst(inst)?;
            }
        }
        let func = out.func;
        verify(func.view()).map_err(SimplifyError::Verify)?;
        return Ok(func);
    }

    // Coalescing path: merge `A -> Jump(B, args)` chains into single blocks.
    let mut merged_into_pred = filled(blocks, false, &alloc);
    let mut resolved_stands = Vec::with_capacity_in(len, alloc);
    resolved_stands.extend_from_slice(&edit.stands);

    for a in 0..blocks {
        if !edit.block_kept[a] {
            continue;
        }
        let b = edit.merge_next[a];
        if b == NONE {
            continue;
        }
        let b_idx = b as usize;
        merged_into_pred[b_idx] = true;
        // Redirect `B`'s parameters to `A`'s `Jump` arguments.
        let term = view.terminator(Block(a as u32));
        let jump_at = view.data[term] as usize;
        let arg_count = view.extra[jump_at + 1] as usize;
        let params = view.params(Block(b));
        if params.len() != arg_count {
            return Err(SimplifyError::Refused {
                inst: term,
                what: "a coalesced jump given another number of arguments than its target's parameters",
            });
        }
        for k in 0..arg_count {
            let arg = view.extra[jump_at + 2 + k];
            resolved_stands[params.start + k] = edit.stands[arg as usize];
        }
    }
    // Path-compress `resolved_stands` so transitive chains across arbitrary block order resolve in O(1).
    for v in 0..len {
        let mut cur = resolved_stands[v];
        for _ in 0..=len {
            let next = resolved_stands[cur as usize];
            if next == cur {
                break;
            }
            cur = next;
        }
        resolved_stands[v] = cur;
    }

    let mut new_block = filled(blocks, NONE, &alloc);
    let mut new_inst = filled(len, NONE, &alloc);
    let mut next_block = 0u32;
    let mut next_inst = 0u32;
    for head in 0..blocks {
        if !edit.block_kept[head] || merged_into_pred[head] {
            continue;
        }
        new_block[head] = next_block;
        next_block += 1;
        let mut cur = head;
        for _ in 0..blocks {
            let range = view.block(Block(cur as u32));
            let start = if cur == head { range.start } else { view.params(Block(cur as u32)).end };
            let next = edit.merge_next[cur];
            let end = if next != NONE { range.end - 1 } else { range.end };
            for inst in start..end {
                if edit.inst_kept[inst] {
                    new_inst[inst] = next_inst;
                    next_inst += 1;
                }
            }
            if next == NONE {
                break;
            }
            cur = next as usize;
        }
    }

    let mut out = Out { view, edit, stands: &resolved_stands, new_inst, new_block, func };
    for head in 0..blocks {
        if !edit.block_kept[head] || merged_into_pred[head] {
            continue;
        }
        out.func.block_starts.push(out.func.ops.len() as u32);
        out.func.block_kinds.push(view.block_kinds[head]);
        let mut cur = head;
        for _ in 0..blocks {
            let range = view.block(Block(cur as u32));
            let start = if cur == head { range.start } else { view.params(Block(cur as u32)).end };
            let next = edit.merge_next[cur];
            let end = if next != NONE { range.end - 1 } else { range.end };
            for inst in start..end {
                if edit.inst_kept[inst] {
                    out.inst(inst)?;
                }
            }
            if next == NONE {
                break;
            }
            cur = next as usize;
        }
    }
    let func = out.func;
    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok(func)
}

/// The output being made by [`rebuild`].
struct Out<'v, 'e, A: Allocator + Copy> {
    view: View<'v>,
    edit: &'e Edit<A>,
    stands: &'e [u32],
    new_inst: Vec<u32, A>,
    new_block: Vec<u32, A>,
    func: Func<A>,
}

impl<A: Allocator + Copy> Out<'_, '_, A> {
    fn raw(&mut self, op: Op, flags: u8, ty: u16, data: u32, attrs: u32, loc: u32) {
        self.func.ops.push(op as u8);
        self.func.flags.push(flags);
        self.func.tys.push(ty);
        self.func.data.push(data);
        self.func.attrs.push(attrs);
        self.func.locs.push(loc);
    }

    /// The output's number of the value a use of `value` in `inst` reads.
    fn value(&self, inst: usize, value: u32) -> Result<u32, SimplifyError> {
        let reads = self.stands[value as usize];
        match self.new_inst[reads as usize] {
            NONE => Err(SimplifyError::Refused { inst, what: "a use of a value the rewrite removed" }),
            it => Ok(it),
        }
    }

    /// Appends the target to `block` with the arguments at `extra[at..at + n]`, the arguments
    /// of removed parameters left out.
    fn target(&mut self, inst: usize, block: u32, at: usize, n: usize) -> Result<(), SimplifyError> {
        let to = self.new_block[block as usize];
        if to == NONE {
            return Err(SimplifyError::Refused { inst, what: "an edge to a block the rewrite removed" });
        }
        let params = self.view.params(Block(block));
        if params.len() != n {
            return Err(SimplifyError::Refused { inst, what: "a target given another number of arguments than its parameters" });
        }
        self.func.extra.push(to);
        let count_at = self.func.extra.len();
        self.func.extra.push(0);
        let mut kept = 0u32;
        for k in 0..n {
            if self.edit.inst_kept[params.start + k] {
                let value = self.value(inst, self.view.extra[at + k])?;
                self.func.extra.push(value);
                kept += 1;
            }
        }
        self.func.extra[count_at] = kept;
        Ok(())
    }

    /// Appends the operands `fields` of `inst`, from `extra[at..]`, as the output names them.
    fn rewrite(&mut self, inst: usize, fields: &[Field]) -> Result<(), SimplifyError> {
        let view = self.view;
        let words = view.extra;
        let mut at = view.data[inst] as usize;
        let mut scrutinee = None;
        for &field in fields {
            match field {
                Field::Value => {
                    scrutinee = Some(words[at]);
                    let value = self.value(inst, words[at])?;
                    self.func.extra.push(value);
                    at += 1;
                }
                Field::Block => {
                    let to = self.new_block[words[at] as usize];
                    if to == NONE {
                        return Err(SimplifyError::Refused { inst, what: "an edge to a block the rewrite removed" });
                    }
                    self.func.extra.push(to);
                    at += 1;
                }
                Field::Imm
                | Field::Offset
                | Field::IntCond
                | Field::FloatCond
                | Field::Rmw
                | Field::Ref
                | Field::Sig
                | Field::Slot
                | Field::Intrinsic
                | Field::Asm => {
                    self.func.extra.push(words[at]);
                    at += 1;
                }
                Field::Target => {
                    let (block, n) = (words[at], words[at + 1] as usize);
                    self.target(inst, block, at + 2, n)?;
                    at += 2 + n;
                }
                Field::Values => {
                    let n = words[at] as usize;
                    self.func.extra.push(n as u32);
                    for k in 0..n {
                        let value = self.value(inst, words[at + 1 + k])?;
                        self.func.extra.push(value);
                    }
                    at += 1 + n;
                }
                Field::Blocks => {
                    let n = words[at] as usize;
                    self.func.extra.push(n as u32);
                    for k in 0..n {
                        let to = self.new_block[words[at + 1 + k] as usize];
                        if to == NONE {
                            return Err(SimplifyError::Refused { inst, what: "an edge to a block the rewrite removed" });
                        }
                        self.func.extra.push(to);
                    }
                    at += 1 + n;
                }
                Field::Mask => {
                    let n = words[at] as usize;
                    self.func.extra.extend_from_slice(&words[at..at + 1 + n]);
                    at += 1 + n;
                }
                Field::Const => {
                    let n = Ty(view.tys[inst]).const_words() as usize;
                    self.func.extra.extend_from_slice(&words[at..at + n]);
                    at += n;
                }
                Field::Cases => {
                    let width = scrutinee.map_or(0, |it: u32| Ty(view.tys[it as usize]).const_words()) as usize;
                    let n = words[at] as usize;
                    self.func.extra.push(n as u32);
                    at += 1;
                    for _ in 0..n {
                        self.func.extra.extend_from_slice(&words[at..at + width]);
                        at += width;
                        let (block, args) = (words[at], words[at + 1] as usize);
                        self.target(inst, block, at + 2, args)?;
                        at += 2 + args;
                    }
                }
            }
        }
        Ok(())
    }

    /// A scalar integer or float constant `value` of type `ty`, encoded as [`crate::Builder::iconst`]
    /// / [`crate::Builder::fconst`] encode one.
    fn constant(&mut self, ty: Ty, value: u128, loc: u32) {
        if ty.is_float() {
            let bits = ty.bits(self.view.head.ptr_bits);
            let le = value.to_le_bytes();
            let word0 = u32::from_le_bytes([le[0], le[1], le[2], le[3]]);
            if bits <= 32 {
                return self.raw(Op::Fconst, 0, ty.0, word0, 0, loc);
            }
            let start = u32::try_from(self.func.extra.len()).unwrap_or(u32::MAX);
            let words = usize::try_from(ty.const_words()).unwrap_or(0);
            for chunk in le.chunks_exact(4).take(words) {
                if let [b0, b1, b2, b3] = *chunk {
                    self.func.extra.push(u32::from_le_bytes([b0, b1, b2, b3]));
                }
            }
            return self.raw(Op::FconstWide, 0, ty.0, start, 0, loc);
        }
        let bits = ty.bits(self.view.head.ptr_bits);
        if bits <= 32 {
            return self.raw(Op::Iconst, 0, ty.0, value as u32, 0, loc);
        }
        match i32::try_from(sext(value, bits)) {
            Ok(small) => self.raw(Op::Iconst, 0, ty.0, small as u32, 0, loc),
            Err(_) => {
                let start = self.func.extra.len() as u32;
                for word in 0..ty.const_words() {
                    self.func.extra.push((value >> (32 * word)) as u32);
                }
                self.raw(Op::IconstWide, 0, ty.0, start, 0, loc);
            }
        }
    }

    /// Copies instruction `inst` as the edit says.
    fn inst(&mut self, inst: usize) -> Result<(), SimplifyError> {
        let view = self.view;
        let op = view.op(inst);
        let (flags, ty, attrs, loc) = (view.flags[inst], view.tys[inst], view.attrs[inst], view.locs[inst]);
        if let Some(value) = self.edit.constant[inst] {
            self.constant(Ty(ty), value, loc);
            return Ok(());
        }
        if let Some(&Some([a, b, c])) = self.edit.fma_fuse.get(inst) {
            let va = self.value(inst, a)?;
            let vb = self.value(inst, b)?;
            let vc = self.value(inst, c)?;
            let start = u32::try_from(self.func.extra.len()).unwrap_or(u32::MAX);
            self.func.extra.push(va);
            self.func.extra.push(vb);
            self.func.extra.push(vc);
            // `Flags::CONTRACT` has been consumed by emitting `Op::Fma`; dropping a promise refines the IR.
            let fma_flags = Flags(flags).without(Flags::CONTRACT).bits();
            self.raw(Op::Fma, fma_flags, ty, start, 0, loc);
            return Ok(());
        }
        let target_ref = self.edit.devirt[inst];
        if target_ref != NONE {
            let words = view.extra;
            let at = view.data[inst] as usize;
            let sig = words[at + 1];
            let n = words[at + 2] as usize;
            let start = self.func.extra.len() as u32;
            self.func.extra.push(target_ref);
            self.func.extra.push(sig);
            self.func.extra.push(n as u32);
            for k in 0..n {
                let value = self.value(inst, words[at + 3 + k])?;
                self.func.extra.push(value);
            }
            self.raw(Op::Call, flags, ty, start, attrs, loc);
            return Ok(());
        }
        let chosen = self.edit.jump[inst];
        if chosen != NONE {
            let mut taken = None;
            each(&view, inst, |item| {
                if let Item::Edge(edge @ Edge { args: Some(_), .. }) = item {
                    if edge.ordinal == chosen {
                        taken = Some(edge);
                    }
                }
            });
            let Some(Edge { block, args: Some((at, n)), .. }) = taken else {
                return Err(SimplifyError::Refused { inst, what: "a jump to a target the terminator does not have" });
            };
            let start = self.func.extra.len() as u32;
            self.target(inst, block, at, n)?;
            self.raw(Op::Jump, 0, Ty::NONE.0, start, 0, loc);
            return Ok(());
        }
        let data = match op.info().layout {
            Layout::None => 0,
            Layout::Data(Field::Value) => self.value(inst, view.data[inst])?,
            Layout::Data(_) => view.data[inst],
            Layout::Extra(fields) => {
                let start = self.func.extra.len() as u32;
                self.rewrite(inst, fields)?;
                start
            }
        };
        self.raw(op, flags, ty, data, attrs, loc);
        Ok(())
    }
}

/// Whether an instruction of `op` whose value nothing uses may go: it writes nothing, calls
/// nothing, traps never, and its only undefined behaviour is a promise (`nsw`, `in_range`, a
/// lane past the end) whose removal makes the program more defined. Division and remainder
/// trap, loads may be volatile or fault, `expose` exposes, `assume` states a fact: all stay.
fn removable(op: Op) -> bool {
    matches!(
        op,
        Op::Iconst
            | Op::IconstWide
            | Op::Fconst
            | Op::FconstWide
            | Op::Vconst
            | Op::Null
            | Op::Uninit
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Not
            | Op::Shl
            | Op::Lshr
            | Op::Ashr
            | Op::Rotl
            | Op::Rotr
            | Op::Umulhi
            | Op::Smulhi
            | Op::UaddSat
            | Op::SaddSat
            | Op::UsubSat
            | Op::SsubSat
            | Op::Umin
            | Op::Umax
            | Op::Smin
            | Op::Smax
            | Op::UaddOverflow
            | Op::SaddOverflow
            | Op::UsubOverflow
            | Op::SsubOverflow
            | Op::UmulOverflow
            | Op::SmulOverflow
            | Op::Clz
            | Op::Ctz
            | Op::Popcnt
            | Op::Bswap
            | Op::Bitrev
            | Op::Icmp
            | Op::Fcmp
            | Op::Select
            | Op::Zext
            | Op::Sext
            | Op::Trunc
            | Op::Fpext
            | Op::Fptrunc
            | Op::FptoUi
            | Op::FptoSi
            | Op::UitoFp
            | Op::SitoFp
            | Op::Bitcast
            | Op::Fadd
            | Op::Fsub
            | Op::Fmul
            | Op::Fdiv
            | Op::Frem
            | Op::FminNum
            | Op::FmaxNum
            | Op::Fminimum
            | Op::Fmaximum
            | Op::Fcopysign
            | Op::Fneg
            | Op::Fabs
            | Op::Freeze
            | Op::PtrAdd
            | Op::PtrAddr
            | Op::PtrWithAddr
            | Op::FuncAddr
            | Op::DataAddr
            | Op::SlotAddr
            | Op::Splat
            | Op::Extract
            | Op::Insert
            | Op::Shuffle
            | Op::Project
    )
}

/// The target a trivial terminator becomes a `jump` to, [`NONE`] if it is not trivial: a
/// `br` on a constant, a `br` whose two targets are the same block with the same arguments,
/// a `switch` on a constant (its matching case, else its default).
fn trivial(view: &View<'_>, inst: usize) -> u32 {
    match view.op(inst) {
        Op::Br => {
            let mut o = view.operands(inst);
            let cond = o.value();
            let (then, otherwise) = (o.target(), o.target());
            match constant_of(view, cond.0) {
                Some(value) => match value != 0 {
                    true => 0,
                    false => 1,
                },
                None if then.block == otherwise.block && then.args.0 == otherwise.args.0 => 0,
                None => NONE,
            }
        }
        Op::Switch => {
            let mut o = view.operands(inst);
            let scrutinee = o.value();
            let Some(value) = constant_of(view, scrutinee.0) else { return NONE };
            let width = Ty(view.tys[scrutinee.index()]).bits(view.head.ptr_bits);
            o.target();
            let found = o.cases(scrutinee).position(|(words, _)| wide_value(words, width) == value);
            match found {
                Some(case) => 1 + case as u32,
                None => 0,
            }
        }
        _ => NONE,
    }
}

/// The function `view` without its dead code and trivial branches, in `alloc`, verified:
/// the module's [`dead`] pass.
pub fn dead<A: Allocator + Copy>(view: View<'_>, alloc: A) -> Result<Func<A>, SimplifyError> {
    if view.blocks() == 0 {
        return Err(SimplifyError::Refused { inst: 0, what: "a function with no blocks" });
    }
    let len = view.len();
    let mut jump = filled(len, NONE, &alloc);
    let mut any_jump = false;
    for b in 0..view.blocks() {
        let end = view.terminator(Block(b as u32));
        let chosen = trivial(&view, end);
        if chosen != NONE {
            jump[end] = chosen;
            any_jump = true;
        }
    }
    let reached = reachable(&view, &jump, &alloc);
    let into = incoming(&view, &reached, &jump, &alloc);
    let owner = owners(&view, &alloc);
    let mut live = filled(len, false, &alloc);
    let mut live_count = 0usize;
    let mut work: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mark = |live: &mut Vec<bool, A>, work: &mut Vec<u32, A>, live_count: &mut usize, value: u32| {
        if !live[value as usize] {
            live[value as usize] = true;
            *live_count += 1;
            work.push(value);
        }
    };
    // The roots: every effect of a reached block, and the parameters that must stay.
    for b in (0..view.blocks()).filter(|&b| reached[b]) {
        let block = Block(b as u32);
        for inst in view.block(block) {
            let root = match view.op(inst) {
                Op::Param => !into.targets_only[b],
                op => !removable(op),
            };
            if root {
                mark(&mut live, &mut work, &mut live_count, inst as u32);
            }
        }
    }
    while let Some(value) = work.pop() {
        let at = value as usize;
        if view.op(at) == Op::Param {
            // A live parameter makes its argument on every edge into its block live.
            let b = owner[at] as usize;
            let k = at - view.params(Block(b as u32)).start;
            for &args in into.edges_into(b) {
                mark(&mut live, &mut work, &mut live_count, view.extra[args as usize + k]);
            }
            continue;
        }
        if jump[at] != NONE {
            // A `jump` now: it reads only its target's arguments, live by their parameters.
            continue;
        }
        each(&view, at, |item| {
            if let Item::Value(used) = item {
                mark(&mut live, &mut work, &mut live_count, used);
            }
        });
    }
    let blocks = view.blocks();
    let mut merge_next = filled(blocks, NONE, &alloc);
    let mut any_merge = false;
    for a in 0..blocks {
        if !reached[a] {
            continue;
        }
        let term = view.terminator(Block(a as u32));
        if view.op(term) != Op::Jump {
            continue;
        }
        let b = view.extra[view.data[term] as usize];
        let b_idx = b as usize;
        if b != 0
            && b_idx != a
            && reached[b_idx]
            && into.targets_only[b_idx]
            && into.edges_into(b_idx).len() == 1
            && !view.block_kind(Block(b)).is_landing()
        {
            merge_next[a] = b;
            any_merge = true;
        }
    }
    if !any_jump && !any_merge && live_count == len && reached.iter().all(|&r| r) {
        verify(view).map_err(SimplifyError::Verify)?;
        return Ok(view.to_func_in(alloc));
    }
    let mut stands = Vec::with_capacity_in(len, alloc);
    for value in 0..len as u32 {
        stands.push(value);
    }
    let edit = Edit {
        block_kept: reached,
        inst_kept: live,
        stands,
        constant: filled(len, None, &alloc),
        jump,
        devirt: filled(len, NONE, &alloc),
        merge_next,
        fma_fuse: filled(len, None, &alloc),
    };
    rebuild(view, &edit, alloc)
}

/// Coalesces single-predecessor `jump` chains (`A -> Jump(B, args)` when `B` has `A` as its
/// sole reached predecessor) without altering any live computation, in `alloc`, verified.
pub fn coalesce_cfg<A: Allocator + Copy>(view: View<'_>, alloc: A) -> Result<Func<A>, SimplifyError> {
    if view.blocks() == 0 {
        return Err(SimplifyError::Refused { inst: 0, what: "a function with no blocks" });
    }
    let blocks = view.blocks();
    let len = view.len();
    let no_jumps = filled(len, NONE, &alloc);
    let reached = reachable(&view, &no_jumps, &alloc);
    let into = incoming(&view, &reached, &no_jumps, &alloc);
    let mut merge_next = filled(blocks, NONE, &alloc);
    let mut any_merge = false;
    for a in 0..blocks {
        if !reached[a] {
            continue;
        }
        let term = view.terminator(Block(a as u32));
        if view.op(term) != Op::Jump {
            continue;
        }
        let b = view.extra[view.data[term] as usize];
        let b_idx = b as usize;
        if b != 0
            && b_idx != a
            && reached[b_idx]
            && into.targets_only[b_idx]
            && into.edges_into(b_idx).len() == 1
            && !view.block_kind(Block(b)).is_landing()
        {
            merge_next[a] = b;
            any_merge = true;
        }
    }
    if !any_merge && reached.iter().all(|&r| r) {
        verify(view).map_err(SimplifyError::Verify)?;
        return Ok(view.to_func_in(alloc));
    }
    let mut stands = Vec::with_capacity_in(len, alloc);
    for value in 0..len as u32 {
        stands.push(value);
    }
    let edit = Edit {
        block_kept: reached,
        inst_kept: filled(len, true, &alloc),
        stands,
        constant: filled(len, None, &alloc),
        jump: no_jumps,
        devirt: filled(len, NONE, &alloc),
        merge_next,
        fma_fuse: filled(len, None, &alloc),
    };
    rebuild(view, &edit, alloc)
}
