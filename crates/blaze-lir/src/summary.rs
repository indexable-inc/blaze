//! ThinLTO function summaries ([`FnSummary`]): compact, deterministic, single-pass metadata
//! computed over a LIR [`View`] to drive cost-model inlining, specialization scoring, and
//! incremental ThinLTO early cutoff.

use alloc::alloc::Allocator;
use core::convert::Infallible;

use blaze_own::Vec;

use crate::{
    Block, Flags, Op, Ty, View,
    func::{FnAttrs, Inline, RefId, RefKind},
    op::{Field, Layout, Word, walk},
};

/// Compact, deterministic summary of a LIR function body, computed in a single linear pass.
///
/// Used by [`crate::inline`] to avoid repeatedly rescanning callees or re-running cycle checks
/// across multiple call sites, and by [`crate::lto`] for ThinLTO early cutoff via
/// [`FnSummary::summary_hash`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FnSummary {
    /// Total number of instructions in the function.
    pub insts: u32,
    /// Total number of basic blocks in the function.
    pub blocks: u32,
    /// Whether the function's CFG contains a cycle (any loop).
    pub has_loops: bool,
    /// Whether the function contains an operation that cannot be spliced (`tail_call`,
    /// `tail_call_indirect`, `suspend`, `cancelled`, `asm`, `asm_br`, `frame_address`,
    /// `return_address`).
    pub has_forbidden_ops: bool,
    /// Whether the function contains at least one `return` terminator.
    pub has_return: bool,
    /// Whether every reference named by the function is its own plain data (no relocations,
    /// no source positions, no external `Func`/`Static`/`Tls`/`Vtable` refs).
    pub is_closed: bool,
    /// Whether the function carries `FnAttrs::NOUNWIND`.
    pub is_nounwind: bool,
    /// Whether the function carries `FnAttrs::COLD`.
    pub is_cold: bool,
    /// The function's `#[inline(...)]` attribute.
    pub inline_attr: Inline,
    /// Bit `i` is set if entry parameter `i` (`i < 64`) feeds a `br`, `switch`, `select`,
    /// `icmp`, or `fcmp` — passing a constant argument unlocks branch/comparison folding.
    pub const_param_mask: u64,
    /// Bit `i` is set if entry parameter `i` (`i < 64`) has type `ptr`, is accessed by at least
    /// one non-volatile `load` or `store`, and is used solely by non-volatile loads/stores and
    /// constant-offset `ptr_add`s — passing a `slot_addr` unlocks post-inline SROA.
    pub sroa_ptr_param_mask: u64,
    /// Canonical BLAKE3 digest over all summary fields for incremental ThinLTO early cutoff.
    pub summary_hash: [u8; 32],
}

/// Whether a reference stays meaningful in another function without importing external refs:
/// the body's own data, with no relocations and no source positions.
#[inline]
pub(crate) fn carried(view: View<'_>, reference: RefId) -> bool {
    let rec = view.reference(reference);
    rec.kind == RefKind::Data as u32
        && view
            .datas
            .get(rec.place as usize)
            .is_some_and(|data| data.relocs_len == 0 && data.positions_len == 0)
}

/// Error returned by [`FnSummary::try_compute`] on a malformed (unverified) view.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MalformedView {
    pub(crate) inst: usize,
    pub(crate) what: &'static str,
}

impl FnSummary {
    /// Computes the [`FnSummary`] of `view` in a single linear pass using scratch in `alloc`.
    ///
    /// Expects a verified LIR [`View`]. If an unverified view has malformed operands or block
    /// edges, conservatively treats those edges as cyclic/open rather than panicking.
    pub fn compute<A: Allocator>(view: View<'_>, alloc: &A) -> FnSummary {
        compute_impl(view, false, alloc).expect("unchecked compute never returns Err")
    }

    /// Checked variant for callers that refuse malformed unverified callees by error.
    pub(crate) fn try_compute<A: Allocator>(
        view: View<'_>,
        alloc: &A,
    ) -> Result<FnSummary, MalformedView> {
        compute_impl(view, true, alloc)
    }

    /// Computes the canonical BLAKE3 hash over all summary fields (excluding `summary_hash`).
    fn compute_hash(&self) -> [u8; 32] {
        let mut h = blaze_ids::Hasher::new("blaze v2 lir fn_summary 1");
        h.update(&self.insts.to_le_bytes());
        h.update(&self.blocks.to_le_bytes());
        let flags: u8 = (self.has_loops as u8)
            | ((self.has_forbidden_ops as u8) << 1)
            | ((self.has_return as u8) << 2)
            | ((self.is_closed as u8) << 3)
            | ((self.is_nounwind as u8) << 4)
            | ((self.is_cold as u8) << 5);
        h.update(&[flags, self.inline_attr as u8]);
        h.update(&self.const_param_mask.to_le_bytes());
        h.update(&self.sroa_ptr_param_mask.to_le_bytes());
        h.finish().0
    }
}

#[inline]
fn is_int_const(view: &View<'_>, v: u32) -> bool {
    let at = v as usize;
    at < view.len()
        && view.flags(at) == Flags::NONE
        && matches!(view.op(at), Op::Iconst | Op::IconstWide)
}

#[inline]
fn propagates_const_dep(op: Op) -> bool {
    matches!(
        op,
        Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Neg
            | Op::Udiv
            | Op::Sdiv
            | Op::Urem
            | Op::Srem
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
            | Op::Clz
            | Op::Ctz
            | Op::Popcnt
            | Op::Bswap
            | Op::Bitrev
            | Op::Zext
            | Op::Sext
            | Op::Trunc
            | Op::Bitcast
            | Op::Freeze
            | Op::Fneg
            | Op::Fabs
            | Op::Fadd
            | Op::Fsub
            | Op::Fmul
            | Op::Fdiv
            | Op::Fpext
            | Op::Fptrunc
            | Op::FptoUi
            | Op::FptoSi
            | Op::UitoFp
            | Op::SitoFp
            | Op::Project
            | Op::Select
    )
}

fn compute_impl<A: Allocator>(
    view: View<'_>,
    strict: bool,
    alloc: &A,
) -> Result<FnSummary, MalformedView> {
    let len = view.len();
    let blocks = view.blocks();
    let attrs = FnAttrs(view.head.attrs);

    let mut has_forbidden_ops = false;
    let mut has_return = false;
    let mut is_closed = true;

    let entry_params = if blocks > 0 { view.params(Block(0)) } else { 0..0 };
    let tracked_params = entry_params.len().min(64);

    // Per-instruction dependency bitmasks (only allocated when entry has tracked params).
    let mut const_dep: Vec<u64, &A> = if tracked_params > 0 {
        Vec::from_elem_in(0u64, len, alloc)
    } else {
        Vec::new_in(alloc)
    };

    let mut ptr_candidate = 0u64;
    for i in 0..tracked_params {
        let p = entry_params.start + i;
        const_dep[p] = 1u64 << i;
        if Ty(view.tys[p]) == Ty::PTR {
            ptr_candidate |= 1u64 << i;
        }
    }

    let mut ptr_origin: Vec<u64, &A> = if ptr_candidate != 0 {
        let mut v = Vec::from_elem_in(0u64, len, alloc);
        for i in 0..tracked_params {
            let p = entry_params.start + i;
            if (ptr_candidate & (1u64 << i)) != 0 {
                v[p] = 1u64 << i;
            }
        }
        v
    } else {
        Vec::new_in(alloc)
    };

    let mut const_param_mask = 0u64;
    let mut ptr_used = 0u64;
    let mut ptr_escaped = 0u64;
    let mut saw_back_or_self_edge = false;
    let mut saw_forward_val_ref = false;

    // Single linear pass over blocks and their instructions.
    for b in 0..blocks {
        let range = view.block(Block(b as u32));
        for at in range {
            let op = view.op(at);
            match op {
                Op::TailCall
                | Op::TailCallIndirect
                | Op::Suspend
                | Op::Cancelled
                | Op::Asm
                | Op::AsmBr
                | Op::FrameAddress
                | Op::ReturnAddress => has_forbidden_ops = true,
                Op::Return => has_return = true,
                _ => {}
            }

            // Inspect operands (refs, successor blocks, and value dependencies).
            let mut dep_union = 0u64;
            let mut op_val_idx = 0u32;
            let mut first_val = 0u32;
            let mut second_val = 0u32;

            let mut visit_word = |word: Word| -> Result<(), Infallible> {
                match word {
                    Word::Of(Field::Ref, r) => {
                        if !carried(view, RefId(r)) {
                            is_closed = false;
                        }
                    }
                    Word::Block(to) => {
                        if (to as usize) <= b {
                            saw_back_or_self_edge = true;
                        }
                    }
                    Word::Value(v) => {
                        if op_val_idx == 0 {
                            first_val = v;
                        } else if op_val_idx == 1 {
                            second_val = v;
                        }
                        op_val_idx += 1;
                        let vs = v as usize;
                        if vs >= at && tracked_params > 0 {
                            saw_forward_val_ref = true;
                        }
                        if tracked_params > 0 && vs < len {
                            dep_union |= const_dep[vs];
                        }
                    }
                    Word::Of(..) => {}
                }
                Ok(())
            };

            match op.info().layout {
                Layout::None => {}
                Layout::Data(Field::Ref) => {
                    let _ = visit_word(Word::Of(Field::Ref, view.data[at]));
                }
                Layout::Data(Field::Value) => {
                    let _ = visit_word(Word::Value(view.data[at]));
                }
                Layout::Data(_) => {}
                Layout::Extra(fields) => {
                    let words = &view.extra[view.data[at] as usize..];
                    if walk(words, fields, Ty(view.tys[at]), view.tys, &mut visit_word).is_err() {
                        if strict {
                            return Err(MalformedView {
                                inst: at,
                                what: "operands that run past their words",
                            });
                        }
                        is_closed = false;
                        saw_back_or_self_edge = true;
                    }
                }
            }

            if tracked_params > 0 {
                // Const-param sensitivity: Br, Switch, Select, Icmp, Fcmp.
                match op {
                    Op::Br | Op::Switch | Op::Select => {
                        if (first_val as usize) < len {
                            const_param_mask |= const_dep[first_val as usize];
                        }
                    }
                    Op::Icmp | Op::Fcmp => {
                        if (first_val as usize) < len {
                            const_param_mask |= const_dep[first_val as usize];
                        }
                        if (second_val as usize) < len {
                            const_param_mask |= const_dep[second_val as usize];
                        }
                    }
                    _ => {}
                }
                if propagates_const_dep(op) {
                    const_dep[at] |= dep_union;
                }
            }

            // SROA pointer-param tracking.
            if ptr_candidate != 0 && op != Op::Param {
                match op {
                    Op::Load => {
                        let addr = first_val as usize;
                        if addr < len {
                            let origin = ptr_origin[addr];
                            if origin != 0 {
                                if view.flags(at).contains(Flags::VOLATILE) {
                                    ptr_escaped |= origin;
                                } else {
                                    ptr_used |= origin;
                                }
                            }
                        }
                    }
                    Op::Store => {
                        // Operand 0 is stored value (escapes if it's a tracked pointer);
                        // Operand 1 is destination address (non-escaping if non-volatile).
                        let val = first_val as usize;
                        let addr = second_val as usize;
                        if val < len {
                            ptr_escaped |= ptr_origin[val];
                        }
                        if addr < len {
                            let origin = ptr_origin[addr];
                            if origin != 0 {
                                if view.flags(at).contains(Flags::VOLATILE) {
                                    ptr_escaped |= origin;
                                } else {
                                    ptr_used |= origin;
                                }
                            }
                        }
                    }
                    Op::PtrAdd => {
                        let base = first_val as usize;
                        if base < len {
                            let origin = ptr_origin[base];
                            if origin != 0 {
                                if is_int_const(&view, second_val) {
                                    ptr_origin[at] |= origin;
                                } else {
                                    ptr_escaped |= origin;
                                }
                            }
                        }
                    }
                    _ => {
                        // Any other instruction reading a tracked pointer escapes it.
                        mark_escaped_values(&view, at, &ptr_origin, &mut ptr_escaped);
                    }
                }
            }
        }
    }

    // Rare fallback: if a non-RPO block layout caused a forward value reference (`v >= at`),
    // run one cleanup pass so `const_param_mask` and `ptr_escaped` are 100% exact.
    if saw_forward_val_ref && tracked_params > 0 {
        for at in 0..len {
            let op = view.op(at);
            if op == Op::Param {
                continue;
            }
            if ptr_candidate != 0 {
                match op {
                    Op::Load => {
                        let addr = view.operands(at).value().0 as usize;
                        if addr < len {
                            let origin = ptr_origin[addr];
                            if origin != 0 {
                                if view.flags(at).contains(Flags::VOLATILE) {
                                    ptr_escaped |= origin;
                                } else {
                                    ptr_used |= origin;
                                }
                            }
                        }
                    }
                    Op::Store => {
                        let mut o = view.operands(at);
                        let val = o.value().0 as usize;
                        let addr = o.value().0 as usize;
                        if val < len {
                            ptr_escaped |= ptr_origin[val];
                        }
                        if addr < len {
                            let origin = ptr_origin[addr];
                            if origin != 0 {
                                if view.flags(at).contains(Flags::VOLATILE) {
                                    ptr_escaped |= origin;
                                } else {
                                    ptr_used |= origin;
                                }
                            }
                        }
                    }
                    Op::PtrAdd => {
                        let mut o = view.operands(at);
                        let base = o.value().0 as usize;
                        let off = o.value().0;
                        if base < len {
                            let origin = ptr_origin[base];
                            if origin != 0 && !is_int_const(&view, off) {
                                ptr_escaped |= origin;
                            }
                        }
                    }
                    _ => mark_escaped_values(&view, at, &ptr_origin, &mut ptr_escaped),
                }
            }
            match op {
                Op::Br | Op::Switch | Op::Select => {
                    let v = view.operands(at).value().0 as usize;
                    if v < len {
                        const_param_mask |= const_dep[v];
                    }
                }
                Op::Icmp | Op::Fcmp => {
                    let mut o = view.operands(at);
                    o.word();
                    let a = o.value().0 as usize;
                    let b = o.value().0 as usize;
                    if a < len {
                        const_param_mask |= const_dep[a];
                    }
                    if b < len {
                        const_param_mask |= const_dep[b];
                    }
                }
                _ => {}
            }
        }
    }

    // Exact cycle check: fast-path zero allocations when all edges go forward (`to > from`).
    let has_loops = if !saw_back_or_self_edge {
        false
    } else {
        !is_acyclic_kahn(view, strict, alloc)?
    };

    let sroa_ptr_param_mask = (ptr_candidate & ptr_used) & !ptr_escaped;

    let mut summary = FnSummary {
        insts: len as u32,
        blocks: blocks as u32,
        has_loops,
        has_forbidden_ops,
        has_return,
        is_closed,
        is_nounwind: (view.head.attrs & FnAttrs::NOUNWIND) != 0,
        is_cold: (view.head.attrs & FnAttrs::COLD) != 0,
        inline_attr: attrs.inline(),
        const_param_mask,
        sroa_ptr_param_mask,
        summary_hash: [0u8; 32],
    };
    summary.summary_hash = summary.compute_hash();
    Ok(summary)
}

#[inline]
fn mark_escaped_values(view: &View<'_>, at: usize, ptr_origin: &[u64], ptr_escaped: &mut u64) {
    let len = ptr_origin.len();
    match view.op(at).info().layout {
        Layout::None => {}
        Layout::Data(Field::Value) => {
            let v = view.data[at] as usize;
            if v < len {
                *ptr_escaped |= ptr_origin[v];
            }
        }
        Layout::Data(_) => {}
        Layout::Extra(fields) => {
            let words = &view.extra[view.data[at] as usize..];
            let _ = walk(words, fields, Ty(view.tys[at]), view.tys, &mut |word| -> Result<(), Infallible> {
                if let Word::Value(v) = word {
                    let vs = v as usize;
                    if vs < len {
                        *ptr_escaped |= ptr_origin[vs];
                    }
                }
                Ok(())
            });
        }
    }
}

/// Kahn's topological order over `view`'s block graph: `true` iff acyclic.
fn is_acyclic_kahn<A: Allocator>(
    view: View<'_>,
    strict: bool,
    alloc: &A,
) -> Result<bool, MalformedView> {
    let blocks = view.blocks();
    let mut edges: Vec<(u32, u32), &A> = Vec::new_in(alloc);
    let mut starts = Vec::from_elem_in(0usize, blocks + 1, alloc);
    for b in 0..blocks {
        starts[b] = edges.len();
        let from = b as u32;
        let term = view.terminator(Block(from));
        if let Layout::Extra(fields) = view.op(term).info().layout {
            let words = &view.extra[view.data[term] as usize..];
            if walk(words, fields, Ty(view.tys[term]), view.tys, &mut |word| -> Result<(), Infallible> {
                if let Word::Block(to) = word {
                    edges.push((from, to));
                }
                Ok(())
            })
            .is_err()
            {
                if strict {
                    return Err(MalformedView {
                        inst: term,
                        what: "operands that run past their words",
                    });
                }
                return Ok(false);
            }
        }
    }
    starts[blocks] = edges.len();
    let mut incoming = Vec::from_elem_in(0u32, blocks, alloc);
    for &(_, to) in edges.iter() {
        match incoming.get_mut(to as usize) {
            Some(count) => *count += 1,
            None => {
                if strict {
                    return Err(MalformedView {
                        inst: view.terminator(Block(0)),
                        what: "a callee edge to a block past its blocks",
                    });
                }
                return Ok(false);
            }
        }
    }
    let mut ready: Vec<u32, &A> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks as u32 {
        if incoming[b as usize] == 0 {
            ready.push(b);
        }
    }
    let mut done = 0usize;
    while let Some(b) = ready.pop() {
        done += 1;
        for &(_, to) in &edges[starts[b as usize]..starts[b as usize + 1]] {
            incoming[to as usize] -= 1;
            if incoming[to as usize] == 0 {
                ready.push(to);
            }
        }
    }
    Ok(done == blocks)
}
