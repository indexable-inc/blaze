//! [`canon`]: a flow's canonical text. Two flows print the same text exactly when they are one
//! flow up to the numbering of blocks, locals, loans and origins, goto chains, unreachable
//! blocks and spans: what the move and borrow checks read of a flow and nothing else.
//!
//! What it does, in order:
//!
//! 1. Keeps only the blocks reachable from the entry (blaze-flow-lower's `fresh()` block after
//!    a `return` is unreachable; a projection leaves no such block).
//! 2. Joins goto chains: a block that is the target of exactly one edge, a `goto`, and is not
//!    the entry, is printed in place of that `goto` (a body's call or drop row ends its block,
//!    a flow's does not, so the projection's `call; goto` is the flow-lower's `call` and the
//!    next row). A goto adds nothing to either check: no access, no point that a loan or a
//!    subset can be at that the next statement does not also have.
//! 3. Numbers the joined blocks in depth-first preorder from the entry, successors in their
//!    order, so `bb0` is the entry and the rest follow their first edge.
//! 4. Numbers locals by first appearance in that walk, `_0..=_args` kept as they are (the
//!    return place and the arguments are the signature's), then the locals only `mentions`
//!    names; loans by first appearance; placeholder origins first in their own order (both
//!    lowerings make them from the signature), then the other origins by first appearance.
//! 5. Prints no spans and no statement numbers; prints the placeholders, the `mentions` and the
//!    `known` facts sorted after the blocks (`Flow::print` leaves them out).
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;
use core::fmt::{self, Write};

use blaze_body::NONE;
use blaze_flow::{Block, BorrowKind, Flow, Operand, Origin, Place, Proj, StmtKind, Value};

/// Writes `flow`'s canonical text (the module doc says what is kept) to `out`.
pub fn canon<A: Allocator + Copy>(flow: &Flow<A>, out: &mut impl Write, alloc: A) -> fmt::Result {
    let nb = flow.blocks.len();
    let mut preds = Vec::with_capacity_in(nb, alloc);
    preds.resize(nb, 0u32);
    let mut reached = Vec::with_capacity_in(nb, alloc);
    reached.resize(nb, false);
    let mut stack: Vec<Block, A> = Vec::new_in(alloc);
    if nb > 0 {
        stack.push(Block(0));
    }
    while let Some(b) = stack.pop() {
        if reached[b.index()] {
            continue;
        }
        reached[b.index()] = true;
        for_each_target(flow, b, |t| {
            preds[t.index()] += 1;
            stack.push(t);
        });
    }
    // The block a goto may join: the only edge into it is that goto.
    let mut joined = Vec::with_capacity_in(nb, alloc);
    joined.resize(nb, false);
    for b in 0..nb {
        if reached[b]
            && let StmtKind::Goto(t) = terminator(flow, Block(b as u32))
            && t.0 != 0
            && preds[t.index()] == 1
        {
            joined[t.index()] = true;
        }
    }

    // Heads in preorder; a head's number is its index here.
    let mut order: Vec<Block, A> = Vec::new_in(alloc);
    let mut number = Vec::with_capacity_in(nb, alloc);
    number.resize(nb, NONE);
    let mut succs: Vec<Block, A> = Vec::new_in(alloc);
    if nb > 0 {
        stack.push(Block(0));
    }
    while let Some(h) = stack.pop() {
        if number[h.index()] != NONE {
            continue;
        }
        number[h.index()] = order.len() as u32;
        order.push(h);
        let last = last_of_chain(flow, &joined, h);
        succs.clear();
        for_each_target(flow, last, |t| succs.push(t));
        for &t in succs.iter().rev() {
            stack.push(t);
        }
    }

    let mut p = Printer {
        flow,
        local: filled(flow.local_places.len(), alloc),
        next_local: flow.args + 1,
        loan: filled(flow.loans.len(), alloc),
        next_loan: 0,
        origin: filled(flow.origins as usize, alloc),
        next_origin: 0,
        number: &number,
    };
    for l in 0..=flow.args {
        if let Some(slot) = p.local.get_mut(l as usize) {
            *slot = l;
        }
    }
    for o in 0..flow.origins {
        if flow.placeholder[o as usize] {
            p.origin[o as usize] = p.next_origin;
            p.next_origin += 1;
        }
    }

    writeln!(out, "args {}", flow.args)?;
    for (n, &h) in order.iter().enumerate() {
        writeln!(out, "bb{n}:")?;
        let mut b = h;
        loop {
            let range = flow.blocks[b.index()];
            for s in range.start..range.end {
                let kind = flow.stmts[s as usize].kind;
                if let StmtKind::Goto(t) = kind
                    && joined[t.index()]
                {
                    break;
                }
                out.write_str("  ")?;
                p.stmt(kind, out)?;
                let subs = &flow.subsets;
                let from = subs.partition_point(|&(at, _, _)| at.0 < s);
                for &(at, f, i) in subs[from..].iter() {
                    if at.0 != s {
                        break;
                    }
                    out.write_str("; ")?;
                    p.origin(f, out)?;
                    out.write_str(": ")?;
                    p.origin(i, out)?;
                }
                out.write_str("\n")?;
            }
            match terminator(flow, b) {
                StmtKind::Goto(t) if joined[t.index()] => b = t,
                _ => break,
            }
        }
    }

    out.write_str("placeholders")?;
    for o in 0..flow.origins {
        if flow.placeholder[o as usize] {
            out.write_str(" ")?;
            p.origin(Origin(o), out)?;
        }
    }
    out.write_str("\n")?;
    // Locals only `mentions` names are numbered in local order, after the walk's.
    for &(l, _) in flow.local_origins.iter() {
        if p.local[l.index()] == NONE {
            p.local[l.index()] = p.next_local;
            p.next_local += 1;
        }
    }
    let mut pairs: Vec<(u32, u32), A> = Vec::new_in(alloc);
    for &(l, o) in flow.local_origins.iter() {
        pairs.push((p.local[l.index()], p.origin_number(o)));
    }
    pairs.sort_unstable();
    out.write_str("mentions")?;
    for &(l, o) in pairs.iter() {
        write!(out, " _{l} 'o{o}")?;
    }
    out.write_str("\n")?;
    pairs.clear();
    for &(f, i) in flow.known.iter() {
        let f = p.origin_number(f);
        pairs.push((f, p.origin_number(i)));
    }
    pairs.sort_unstable();
    out.write_str("known")?;
    for &(f, i) in pairs.iter() {
        write!(out, " 'o{f}: 'o{i}")?;
    }
    out.write_str("\n")
}

fn filled<A: Allocator>(n: usize, alloc: A) -> Vec<u32, A> {
    let mut v = Vec::with_capacity_in(n, alloc);
    v.resize(n, NONE);
    v
}

fn terminator<A: Allocator>(flow: &Flow<A>, b: Block) -> StmtKind {
    flow.stmts[flow.blocks[b.index()].end as usize - 1].kind
}

fn for_each_target<A: Allocator>(flow: &Flow<A>, b: Block, mut f: impl FnMut(Block)) {
    match terminator(flow, b) {
        StmtKind::Goto(t) => f(t),
        StmtKind::Switch { targets, .. } => {
            for &t in &flow.succ[targets.start as usize..targets.end as usize] {
                f(t);
            }
        }
        StmtKind::Suspend { resume, cancel } => {
            f(resume);
            f(cancel);
        }
        _ => {}
    }
}

/// The last block of the chain `h` heads: each goto into a joined block goes on into it.
/// Ends: a cycle of joined blocks would have no edge from outside it, so it is unreached.
fn last_of_chain<A: Allocator>(flow: &Flow<A>, joined: &[bool], h: Block) -> Block {
    let mut b = h;
    while let StmtKind::Goto(t) = terminator(flow, b)
        && joined[t.index()]
    {
        b = t;
    }
    b
}

struct Printer<'f, A: Allocator> {
    flow: &'f Flow<A>,
    local: Vec<u32, A>,
    next_local: u32,
    loan: Vec<u32, A>,
    next_loan: u32,
    origin: Vec<u32, A>,
    next_origin: u32,
    number: &'f [u32],
}

impl<A: Allocator> Printer<'_, A> {
    fn origin_number(&mut self, o: Origin) -> u32 {
        let slot = &mut self.origin[o.index()];
        if *slot == NONE {
            *slot = self.next_origin;
            self.next_origin += 1;
        }
        *slot
    }

    fn origin(&mut self, o: Origin, out: &mut impl Write) -> fmt::Result {
        let n = self.origin_number(o);
        write!(out, "'o{n}")
    }

    fn local(&mut self, l: u32, out: &mut impl Write) -> fmt::Result {
        let slot = &mut self.local[l as usize];
        if *slot == NONE {
            *slot = self.next_local;
            self.next_local += 1;
        }
        write!(out, "_{}", *slot)
    }

    fn loan(&mut self, l: blaze_flow::Loan, out: &mut impl Write) -> fmt::Result {
        let slot = &mut self.loan[l.index()];
        if *slot == NONE {
            *slot = self.next_loan;
            self.next_loan += 1;
        }
        write!(out, "L{}", *slot)
    }

    fn block(&self, b: Block, out: &mut impl Write) -> fmt::Result {
        write!(out, "bb{}", self.number[b.index()])
    }

    fn place(&mut self, p: Place, out: &mut impl Write) -> fmt::Result {
        let row = self.flow.place(p);
        match (row.parent, row.proj) {
            (Some(up), Some(proj)) => {
                self.place(up, out)?;
                match proj {
                    Proj::Field(i) => write!(out, ".{i}")?,
                    Proj::Deref => out.write_str(".*")?,
                    Proj::DerefBox => out.write_str(".box")?,
                    Proj::Index => out.write_str("[]")?,
                    Proj::ConstantIndex { offset, min_length, from_end: false } => write!(out, "[{offset} of {min_length}]")?,
                    Proj::ConstantIndex { offset, min_length, from_end: true } => write!(out, "[-{offset} of {min_length}]")?,
                    Proj::Subslice { from, to, from_end: false } => write!(out, "[{from}..{to}]")?,
                    Proj::Subslice { from, to, from_end: true } => write!(out, "[{from}..-{to}]")?,
                    Proj::Downcast(v) => write!(out, "#{v}")?,
                }
                if row.of_drop_type {
                    out.write_str("!")?;
                }
                Ok(())
            }
            _ => self.local(row.local.0, out),
        }
    }

    fn operand(&mut self, o: Operand, out: &mut impl Write) -> fmt::Result {
        match o {
            Operand::Copy(p) => {
                out.write_str("copy ")?;
                self.place(p, out)
            }
            Operand::Move(p) => {
                out.write_str("move ")?;
                self.place(p, out)
            }
            Operand::Const => out.write_str("const"),
        }
    }

    fn ops(&mut self, ops: blaze_flow::flow::Ops, out: &mut impl Write) -> fmt::Result {
        let flow = self.flow;
        for (i, &o) in flow.ops(ops).iter().enumerate() {
            if i > 0 {
                out.write_str(", ")?;
            }
            self.operand(o, out)?;
        }
        Ok(())
    }

    fn stmt(&mut self, kind: StmtKind, out: &mut impl Write) -> fmt::Result {
        match kind {
            StmtKind::Assign { dest, value } => {
                self.place(dest, out)?;
                out.write_str(" = ")?;
                match value {
                    Value::Use(o) => self.operand(o, out),
                    Value::Borrow { kind, place, loan } => {
                        out.write_str(match kind {
                            BorrowKind::Shared => "&",
                            BorrowKind::Mut => "&mut ",
                            BorrowKind::TwoPhase => "&2ph ",
                            BorrowKind::Shallow => "&shallow ",
                        })?;
                        self.place(place, out)?;
                        out.write_str(" [")?;
                        self.loan(loan, out)?;
                        out.write_str(" in ")?;
                        let o = self.flow.loan(loan).origin;
                        self.origin(o, out)?;
                        out.write_str("]")
                    }
                    Value::Aggregate(ops) => {
                        out.write_str("(")?;
                        self.ops(ops, out)?;
                        out.write_str(")")
                    }
                }
            }
            StmtKind::Call { args, dest } => {
                self.place(dest, out)?;
                out.write_str(" = call(")?;
                self.ops(args, out)?;
                out.write_str(")")
            }
            StmtKind::StorageLive(l) => {
                out.write_str("live ")?;
                self.local(l.0, out)
            }
            StmtKind::StorageDead(l) => {
                out.write_str("dead ")?;
                self.local(l.0, out)
            }
            StmtKind::Drop(p) => {
                out.write_str("drop ")?;
                self.place(p, out)
            }
            StmtKind::Activate(l) => {
                out.write_str("activate ")?;
                self.loan(l, out)
            }
            StmtKind::Mention(p) => {
                out.write_str("mention ")?;
                self.place(p, out)
            }
            StmtKind::Goto(b) => {
                out.write_str("goto ")?;
                self.block(b, out)
            }
            StmtKind::Switch { on, targets } => {
                out.write_str("switch ")?;
                self.place(on, out)?;
                out.write_str(" ->")?;
                let flow = self.flow;
                for &t in &flow.succ[targets.start as usize..targets.end as usize] {
                    out.write_str(" ")?;
                    self.block(t, out)?;
                }
                Ok(())
            }
            StmtKind::Return => out.write_str("return"),
            StmtKind::Unreachable => out.write_str("unreachable"),
            StmtKind::Suspend { resume, cancel } => {
                out.write_str("suspend -> ")?;
                self.block(resume, out)?;
                out.write_str(", cancel ")?;
                self.block(cancel, out)
            }
        }
    }
}
