//! Copy and constant propagation with folding (docs/v2/plan/design/backend-next.md, D100
//! point 1: the third pass of the linear simplify stage).
//!
//! Facts, each a true invariant of every run, found over the reached blocks until none is new
//! (each value gains at most one fact, so the passes are at most one more than the values):
//!
//! - a value STANDS FOR another: a block parameter whose every argument, on the edges from
//!   reached blocks, is one value or the parameter itself (the value dominates the block, so
//!   every use may read it); a `select` on a known condition, or with one value on both arms;
//! - a value is a KNOWN integer: `iconst` and `iconst_wide`; an integer operation without
//!   flags whose operands are known, folded by the evaluator's own arithmetic
//!   ([`crate::arith`]: `binary`, `unary`, `overflow`, `compare`, `convert`), so the constant is
//!   what the evaluator gives for every operation and width; a `project` of a known overflow
//!   pair; a `freeze` of a known integer; a block parameter whose every argument is one known
//!   integer.
//!
//! What is not folded, by rule: an operation that traps on its operands (division by zero,
//! `sdiv` of `MIN` by -1) stays, so its trap stays; an instruction with any flags stays (a
//! promise such as `nsw` or `exact` could make the folded value undefined, and the evaluator
//! models no flag); floats, vectors and pointers are not folded.
//!
//! The output ([`super::dce::rebuild`]): every use reads the value its value stands for; an
//! instruction with a known integer, not a parameter, is that `iconst` (or `iconst_wide`) in
//! place. Nothing is removed: what this leaves unused, [`super::dead`] removes.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Edit, NONE, filled, incoming, rebuild, reachable},
};
use crate::{
    Block, Flags, Func, Op, Ty, View,
    arith::{binary, compare, convert, iconst_value, overflow, unary, wide_value},
    verify,
};

/// A planted wrong rewrite, for the pass's control test (rule 53); [`Plant::None`] in every
/// build. Read only under `cfg!(test)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// `sub` of two known integers folded as their sum.
    FoldSubAsAdd,
}

/// What is known of a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Known {
    No,
    /// A scalar integer, its bits above its width zero.
    Int(u128),
    /// An overflow operation's `{T, i1}`.
    Pair(u128, bool),
    /// A function pointer produced by `Op::FuncAddr(r)` (or merged from identical `FuncAddr`s).
    FuncRef(u32),
}

/// A fact found for one instruction.
enum Fact {
    None,
    Stands(u32),
    Known(Known),
}

/// The function `view` with its copies and constants propagated and its integer operations
/// on constants folded, in `alloc`, verified.
pub fn propagate<A: Allocator + Copy>(view: View<'_>, alloc: A) -> Result<Func<A>, SimplifyError> {
    propagate_planted(view, Plant::None, alloc)
}

/// [`propagate`] with `plant` (a test's control; ignored outside tests).
pub fn propagate_planted<A: Allocator + Copy>(view: View<'_>, plant: Plant, alloc: A) -> Result<Func<A>, SimplifyError> {
    if view.blocks() == 0 {
        return Err(SimplifyError::Refused { inst: 0, what: "a function with no blocks" });
    }
    let len = view.len();
    let no_jumps = filled(len, NONE, &alloc);
    let reached = reachable(&view, &no_jumps, &alloc);
    let into = incoming(&view, &reached, &no_jumps, &alloc);

    // Single pass over reached blocks: build CSR def-use edges for values that can ever gain a
    // fact, record each eligible block parameter's block, and seed the worklist.
    let mut param_block = filled(len, NONE, &alloc);
    let mut use_counts = filled(len + 1, 0u32, &alloc);
    let mut raw_edges: Vec<(u32, u32), A> = Vec::new_in(alloc);
    let mut call_indirects: Vec<u32, A> = Vec::new_in(alloc);
    let mut queue = Queue::new(len, &alloc);
    let mut in_work = filled(len, false, &alloc);

    for b in (0..view.blocks()).filter(|&b| reached[b]) {
        let block = Block(b as u32);
        let params = view.params(block);
        if into.targets_only[b] {
            let edges = into.edges_into(b);
            for (k, param) in (params.start..params.end).enumerate() {
                param_block[param] = b as u32;
                let p = param as u32;
                for &args in edges {
                    let arg = view.extra[args as usize + k];
                    if arg != p {
                        use_counts[arg as usize + 1] += 1;
                        raw_edges.push((arg, p));
                    }
                }
                queue.push(p, &mut in_work);
            }
        }
        for inst in params.end..view.block(block).end {
            if view.op(inst) == Op::CallIndirect {
                call_indirects.push(inst as u32);
                continue;
            }
            let Some((n, deps, seed)) = foldable_deps(&view, inst) else {
                continue;
            };
            let u = inst as u32;
            for &d in &deps[..n as usize] {
                use_counts[d as usize + 1] += 1;
                raw_edges.push((d, u));
            }
            if seed {
                queue.push(u, &mut in_work);
            }
        }
    }

    for v in 0..len {
        use_counts[v + 1] += use_counts[v];
    }
    let use_start = use_counts;
    let mut fill = filled(len, 0u32, &alloc);
    let mut use_list = filled(use_start[len] as usize, 0u32, &alloc);
    for &(def, user) in raw_edges.iter() {
        let d = def as usize;
        use_list[(use_start[d] + fill[d]) as usize] = user;
        fill[d] += 1;
    }

    let mut facts = Facts {
        view,
        plant: cfg!(test) && plant == Plant::FoldSubAsAdd,
        stands: Vec::with_capacity_in(len, alloc),
        known: filled(len, Known::No, &alloc),
    };
    for value in 0..len as u32 {
        facts.stands.push(value);
    }

    // Intrusive forest of `stands` children: if `facts.stands[x] == r`, `x` sits in `r`'s list.
    // When `r` gains a fact, every node in `r`'s subtree now resolves to the new fact too, so
    // its undecided users must be woken.
    let mut stand_head = filled(len, NONE, &alloc);
    let mut stand_next = filled(len, NONE, &alloc);
    let mut relay_stack: Vec<u32, A> = Vec::new_in(alloc);

    let mut any_stands = false;
    let mut any_folded = false;

    while let Some(v_u32) = queue.pop(&mut in_work) {
        let v = v_u32 as usize;
        if facts.decided(v) {
            continue;
        }
        let b = param_block[v];
        let fact = if b != NONE {
            let k = v - view.block_starts[b as usize] as usize;
            facts.param(v, k, into.edges_into(b as usize))
        } else {
            facts.inst(v)
        };
        if !facts.take(v, fact) {
            continue;
        }
        if facts.stands[v] != v_u32 {
            any_stands = true;
            let root = facts.stands[v] as usize;
            stand_next[v] = stand_head[root];
            stand_head[root] = v_u32;
        } else if matches!(facts.known[v], Known::Int(_))
            && !matches!(view.op(v), Op::Param | Op::Iconst | Op::IconstWide)
        {
            any_folded = true;
        }
        relay_stack.push(v_u32);
        while let Some(u) = relay_stack.pop() {
            let us = u as usize;
            for &user in &use_list[use_start[us] as usize..use_start[us + 1] as usize] {
                if !facts.decided(user as usize) {
                    queue.push(user, &mut in_work);
                }
            }
            let mut child = stand_head[us];
            while child != NONE {
                relay_stack.push(child);
                child = stand_next[child as usize];
            }
        }
    }

    let mut devirt = filled(len, NONE, &alloc);
    let mut any_devirt = false;
    for &ci in call_indirects.iter() {
        let inst = ci as usize;
        let callee = view.operands(inst).value().0;
        if let Some(target_ref) = facts.func_ref(callee) {
            devirt[inst] = target_ref;
            any_devirt = true;
        }
    }

    if !any_stands && !any_folded && !any_devirt {
        verify(view).map_err(SimplifyError::Verify)?;
        return Ok(view.to_func_in(alloc));
    }

    let mut stands = Vec::with_capacity_in(len, alloc);
    let mut constant = filled(len, None, &alloc);
    for value in 0..len {
        stands.push(facts.resolve(value as u32));
        if let (Known::Int(it), true) = (facts.known[value], view.op(value) != Op::Param) {
            constant[value] = Some(it);
        }
    }
    let edit = Edit {
        block_kept: filled(view.blocks(), true, &alloc),
        inst_kept: filled(len, true, &alloc),
        stands,
        constant,
        jump: no_jumps,
        devirt,
        merge_next: filled(view.blocks(), NONE, &alloc),
        fma_fuse: filled(len, None, &alloc),
    };
    rebuild(view, &edit, alloc)
}

/// Fixed-capacity FIFO ring buffer bounded by the value count (`in_work` guarantees at most
/// `len` entries are queued at once).
struct Queue<A: Allocator> {
    buf: Vec<u32, A>,
    head: usize,
    tail: usize,
    count: usize,
}

impl<A: Allocator + Copy> Queue<A> {
    fn new(len: usize, alloc: &A) -> Self {
        Self { buf: filled(len.max(1), 0u32, alloc), head: 0, tail: 0, count: 0 }
    }

    #[inline]
    fn push(&mut self, v: u32, in_work: &mut [bool]) {
        if !in_work[v as usize] {
            in_work[v as usize] = true;
            self.buf[self.tail] = v;
            self.tail += 1;
            if self.tail == self.buf.len() {
                self.tail = 0;
            }
            self.count += 1;
        }
    }

    #[inline]
    fn pop(&mut self, in_work: &mut [bool]) -> Option<u32> {
        if self.count == 0 {
            return None;
        }
        let v = self.buf[self.head];
        self.head += 1;
        if self.head == self.buf.len() {
            self.head = 0;
        }
        self.count -= 1;
        in_work[v as usize] = false;
        Some(v)
    }
}

#[inline]
fn dedup3(c: u32, a: u32, b: u32) -> (u8, [u32; 3]) {
    let mut out = [c, 0, 0];
    let mut n = 1u8;
    if a != c {
        out[n as usize] = a;
        n += 1;
    }
    if b != c && b != a {
        out[n as usize] = b;
        n += 1;
    }
    (n, out)
}

/// Value operands `Facts::inst(inst)` reads, if `inst` can ever gain a fact:
/// `(count, deps, seed_initially)`.
#[inline]
fn foldable_deps(view: &View<'_>, inst: usize) -> Option<(u8, [u32; 3], bool)> {
    if view.flags(inst) != Flags::NONE {
        return None;
    }
    let op = view.op(inst);
    let ty = Ty(view.tys[inst]);
    if !(ty.is_scalar() && ty.is_int()) {
        return match op {
            Op::FuncAddr => Some((0, [0, 0, 0], true)),
            Op::Freeze if ty == Ty::PTR => {
                let mut o = view.operands(inst);
                Some((1, [o.value().0, 0, 0], false))
            }
            Op::UaddOverflow
            | Op::SaddOverflow
            | Op::UsubOverflow
            | Op::SsubOverflow
            | Op::UmulOverflow
            | Op::SmulOverflow => {
                let mut o = view.operands(inst);
                let (a, b) = (o.value().0, o.value().0);
                let (n, deps) = if a == b { (1, [a, 0, 0]) } else { (2, [a, b, 0]) };
                Some((n, deps, false))
            }
            Op::Select => {
                let mut o = view.operands(inst);
                let (c, a, b) = (o.value().0, o.value().0, o.value().0);
                let (n, deps) = dedup3(c, a, b);
                Some((n, deps, a == b))
            }
            _ => None,
        };
    }
    match op {
        Op::Iconst | Op::IconstWide => Some((0, [0, 0, 0], true)),
        Op::Sub | Op::And | Op::Or | Op::Xor => {
            let mut o = view.operands(inst);
            let (a, b) = (o.value().0, o.value().0);
            let (n, deps) = if a == b { (1, [a, 0, 0]) } else { (2, [a, b, 0]) };
            Some((n, deps, a == b))
        }
        Op::Add | Op::Mul | Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem
        | Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr
        | Op::Umulhi | Op::Smulhi | Op::UaddSat | Op::SaddSat | Op::UsubSat | Op::SsubSat
        | Op::Umin | Op::Umax | Op::Smin | Op::Smax => {
            let mut o = view.operands(inst);
            let (a, b) = (o.value().0, o.value().0);
            let (n, deps) = if a == b { (1, [a, 0, 0]) } else { (2, [a, b, 0]) };
            Some((n, deps, false))
        }
        Op::Icmp => {
            let mut o = view.operands(inst);
            o.int_cond();
            let (a, b) = (o.value().0, o.value().0);
            let (n, deps) = if a == b { (1, [a, 0, 0]) } else { (2, [a, b, 0]) };
            Some((n, deps, false))
        }
        Op::Neg
        | Op::Not
        | Op::Clz
        | Op::Ctz
        | Op::Popcnt
        | Op::Bswap
        | Op::Bitrev
        | Op::Project
        | Op::Zext
        | Op::Sext
        | Op::Trunc
        | Op::Freeze => {
            let mut o = view.operands(inst);
            Some((1, [o.value().0, 0, 0], false))
        }
        Op::Select => {
            let mut o = view.operands(inst);
            let (c, a, b) = (o.value().0, o.value().0, o.value().0);
            let (n, deps) = dedup3(c, a, b);
            Some((n, deps, a == b))
        }
        _ => None,
    }
}

/// The facts found so far.
struct Facts<'v, A: Allocator> {
    view: View<'v>,
    /// `Plant::FoldSubAsAdd`, in a test.
    plant: bool,
    /// Per value: the value it stands for, itself if none; set once, always to a value that
    /// stands for none, so the chains end.
    stands: Vec<u32, A>,
    known: Vec<Known, A>,
}

impl<A: Allocator> Facts<'_, A> {
    fn decided(&self, value: usize) -> bool {
        self.stands[value] != value as u32 || self.known[value] != Known::No
    }

    /// The value `value` stands for at the end of its chain.
    fn resolve(&self, mut value: u32) -> u32 {
        // A chain is acyclic and at most one link per value long.
        for _ in 0..=self.stands.len() {
            let next = self.stands[value as usize];
            if next == value {
                return value;
            }
            value = next;
        }
        unreachable!("a chain of `stands` longer than the values: a cycle, which `take` never makes")
    }

    /// The known integer `value` reads, when its type is a scalar integer.
    fn int(&self, value: u32) -> Option<(u128, u32)> {
        let ty = Ty(self.view.tys[value as usize]);
        if !ty.is_scalar() || !ty.is_int() {
            return None;
        }
        match self.known[self.resolve(value) as usize] {
            Known::Int(it) => Some((it, ty.bits(self.view.head.ptr_bits))),
            Known::No | Known::Pair(..) | Known::FuncRef(..) => None,
        }
    }

    /// The known `RefId` (of `RefKind::Func`) `value` resolves to, when `value` is a pointer
    /// originating from `Op::FuncAddr`.
    fn func_ref(&self, value: u32) -> Option<u32> {
        let ty = Ty(self.view.tys[value as usize]);
        if ty != Ty::PTR {
            return None;
        }
        match self.known[self.resolve(value) as usize] {
            Known::FuncRef(r) => Some(r),
            Known::No | Known::Int(..) | Known::Pair(..) => None,
        }
    }

    /// Records `fact` for `value`; whether it is new.
    fn take(&mut self, value: usize, fact: Fact) -> bool {
        match fact {
            Fact::None => false,
            Fact::Stands(other) => {
                let other = self.resolve(other);
                if other == value as u32 {
                    return false;
                }
                self.stands[value] = other;
                true
            }
            Fact::Known(known) => {
                self.known[value] = known;
                true
            }
        }
    }

    /// Block parameter `param`, the `k`th of its block, whose block's edges have their
    /// arguments at `edges` in `extra`.
    fn param(&self, param: usize, k: usize, edges: &[u32]) -> Fact {
        let mut same = None;
        let mut all_same = true;
        let mut constant = None;
        let mut all_constant = true;
        let mut func_ref = None;
        let mut all_func_ref = true;
        for &args in edges {
            let arg = self.resolve(self.view.extra[args as usize + k]);
            if arg == param as u32 {
                continue;
            }
            match same {
                None => same = Some(arg),
                Some(it) if it != arg => all_same = false,
                Some(_) => {}
            }
            match (self.int(arg), constant) {
                (Some((it, _)), None) => constant = Some(it),
                (Some((it, _)), Some(was)) if it != was => all_constant = false,
                (Some(_), Some(_)) => {}
                (None, _) => all_constant = false,
            }
            match (self.func_ref(arg), func_ref) {
                (Some(r), None) => func_ref = Some(r),
                (Some(r), Some(was)) if r != was => all_func_ref = false,
                (Some(_), Some(_)) => {}
                (None, _) => all_func_ref = false,
            }
        }
        let ty = Ty(self.view.tys[param]);
        match (same, all_same, constant, all_constant, func_ref, all_func_ref) {
            (Some(value), true, _, _, _, _) => Fact::Stands(value),
            (Some(_), false, Some(it), true, _, _) if ty.is_scalar() && ty.is_int() => {
                Fact::Known(Known::Int(it))
            }
            (Some(_), false, _, _, Some(r), true) if ty == Ty::PTR => {
                Fact::Known(Known::FuncRef(r))
            }
            _ => Fact::None,
        }
    }

    /// Instruction `inst`, not a parameter.
    fn inst(&self, inst: usize) -> Fact {
        let view = self.view;
        if view.flags(inst) != Flags::NONE {
            return Fact::None;
        }
        let op = view.op(inst);
        let ty = Ty(view.tys[inst]);
        let Some(width) = (ty.is_scalar() && ty.is_int()).then(|| ty.bits(view.head.ptr_bits)) else {
            return match op {
                Op::FuncAddr => Fact::Known(Known::FuncRef(view.data[inst])),
                Op::Freeze if ty == Ty::PTR => match self.func_ref(view.operands(inst).value().0) {
                    Some(r) => Fact::Known(Known::FuncRef(r)),
                    None => Fact::None,
                },
                Op::UaddOverflow | Op::SaddOverflow | Op::UsubOverflow | Op::SsubOverflow | Op::UmulOverflow | Op::SmulOverflow => {
                    let mut o = view.operands(inst);
                    match (self.int(o.value().0), self.int(o.value().0)) {
                        (Some((a, width)), Some((b, _))) => {
                            let (value, flag) = overflow(op, a, b, width);
                            Fact::Known(Known::Pair(value, flag))
                        }
                        _ => Fact::None,
                    }
                }
                Op::Select => self.select(inst),
                _ => Fact::None,
            };
        };
        let mut o = view.operands(inst);
        match op {
            Op::Iconst => Fact::Known(Known::Int(iconst_value(o.imm(), width))),
            Op::IconstWide => Fact::Known(Known::Int(wide_value(o.constant(), width))),
            Op::Add | Op::Sub | Op::Mul | Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem | Op::And
            | Op::Or | Op::Xor | Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr
            | Op::Umulhi | Op::Smulhi | Op::UaddSat | Op::SaddSat | Op::UsubSat | Op::SsubSat
            | Op::Umin | Op::Umax | Op::Smin | Op::Smax => {
                let (ra, rb) = (self.resolve(o.value().0), self.resolve(o.value().0));
                let (ka, kb) = (self.int(ra), self.int(rb));
                if let (Some((a, _)), Some((b, _))) = (ka, kb) {
                    // Planted (test builds only): `sub` folded as `add`.
                    let op = match self.plant && op == Op::Sub {
                        true => Op::Add,
                        false => op,
                    };
                    return match binary(op, a, b, width) {
                        Ok(it) => Fact::Known(Known::Int(it)),
                        // A trap is the instruction's to keep.
                        Err(_) => Fact::None,
                    };
                }
                if self.plant && op == Op::Sub {
                    return Fact::None;
                }
                if view.op(ra as usize) == Op::Uninit || view.op(rb as usize) == Op::Uninit {
                    return Fact::None;
                }
                // Safe flag-preserving algebraic identities (flags == Flags::NONE checked above).
                match op {
                    Op::Add => match (ka, kb) {
                        (_, Some((0, _))) => Fact::Stands(ra),
                        (Some((0, _)), _) => Fact::Stands(rb),
                        _ => Fact::None,
                    },
                    Op::Sub => match (ka, kb) {
                        (_, Some((0, _))) => Fact::Stands(ra),
                        _ if ra == rb => Fact::Known(Known::Int(0)),
                        _ => Fact::None,
                    },
                    Op::Mul => match (ka, kb) {
                        (_, Some((1, _))) => Fact::Stands(ra),
                        (Some((1, _)), _) => Fact::Stands(rb),
                        (_, Some((0, _))) | (Some((0, _)), _) => Fact::Known(Known::Int(0)),
                        _ => Fact::None,
                    },
                    Op::And => match (ka, kb) {
                        (_, Some((0, _))) | (Some((0, _)), _) => Fact::Known(Known::Int(0)),
                        _ if ra == rb => Fact::Stands(ra),
                        _ => Fact::None,
                    },
                    Op::Or => match (ka, kb) {
                        (_, Some((0, _))) => Fact::Stands(ra),
                        (Some((0, _)), _) => Fact::Stands(rb),
                        _ if ra == rb => Fact::Stands(ra),
                        _ => Fact::None,
                    },
                    Op::Xor => match (ka, kb) {
                        (_, Some((0, _))) => Fact::Stands(ra),
                        (Some((0, _)), _) => Fact::Stands(rb),
                        _ if ra == rb => Fact::Known(Known::Int(0)),
                        _ => Fact::None,
                    },
                    _ => Fact::None,
                }
            }
            Op::Neg | Op::Not | Op::Clz | Op::Ctz | Op::Popcnt | Op::Bswap | Op::Bitrev => match self.int(o.value().0) {
                Some((a, _)) => Fact::Known(Known::Int(unary(op, a, width))),
                None => Fact::None,
            },
            Op::Project => {
                let pair = self.resolve(o.value().0);
                match (self.known[pair as usize], o.imm()) {
                    (Known::Pair(value, _), 0) => Fact::Known(Known::Int(value)),
                    (Known::Pair(_, flag), 1) => Fact::Known(Known::Int(u128::from(flag))),
                    _ => Fact::None,
                }
            }
            Op::Icmp => {
                let cond = o.int_cond();
                match (self.int(o.value().0), self.int(o.value().0)) {
                    (Some((a, from)), Some((b, _))) => Fact::Known(Known::Int(u128::from(compare(cond, a, b, from)))),
                    _ => Fact::None,
                }
            }
            Op::Select => self.select(inst),
            Op::Zext | Op::Sext | Op::Trunc => match self.int(o.value().0) {
                Some((a, from)) => Fact::Known(Known::Int(convert(op, a, from, width))),
                None => Fact::None,
            },
            Op::Freeze => match self.int(o.value().0) {
                Some((a, _)) => Fact::Known(Known::Int(a)),
                None => Fact::None,
            },
            _ => Fact::None,
        }
    }

    /// A `select`: the arm a known condition picks, the one value of both arms, or the one
    /// known integer or function reference of both.
    fn select(&self, inst: usize) -> Fact {
        let mut o = self.view.operands(inst);
        let cond = o.value().0;
        let (a, b) = (self.resolve(o.value().0), self.resolve(o.value().0));
        if let Some((cond, _)) = self.int(cond) {
            return Fact::Stands(if cond != 0 { a } else { b });
        }
        if a == b {
            return Fact::Stands(a);
        }
        let ty = Ty(self.view.tys[inst]);
        if ty.is_scalar() && ty.is_int() {
            if let (Some((x, _)), Some((y, _))) = (self.int(a), self.int(b)) {
                if x == y {
                    return Fact::Known(Known::Int(x));
                }
            }
        } else if ty == Ty::PTR {
            if let (Some(rx), Some(ry)) = (self.func_ref(a), self.func_ref(b)) {
                if rx == ry {
                    return Fact::Known(Known::FuncRef(rx));
                }
            }
        }
        Fact::None
    }
}
