//! The small form the transform works on (coroutines.md, "The form"): typed ids, a body as flat
//! tables (locals, one statement array, blocks as ranges of it, a terminator per block), a
//! builder for hand-made bodies and a printer with a stable text.
//!
//! Locals `0..upvars` are the coroutine's upvars (its captures, or an `async fn`'s arguments:
//! the Reference, Async functions: "The async move block in the body captures all function
//! parameters"); they are initialised when the coroutine is made. Block 0 is the entry.
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;
use core::fmt;

macro_rules! ids {
    ($($(#[$m:meta])* $name:ident = $tag:literal;)*) => {$(
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        pub struct $name(pub u32);
        impl $name {
            pub fn index(self) -> usize {
                self.0 as usize
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($tag, "{}"), self.0)
            }
        }
    )*};
}

ids! {
    /// A local of the body; `0..upvars` are the upvars.
    LocalId = "_";
    /// A block: a range of the statement array and one terminator.
    BlockId = "bb";
    /// A suspension point, numbered from 0 in the order the adapter met them.
    PointId = "s";
    /// A slot of the state: one saved local.
    SlotId = "f";
}

/// How a statement reads a local, or a constant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Operand {
    Copy(LocalId),
    /// The local is uninitialised after.
    Move(LocalId),
    Const(i64),
}

/// A range of the operand table (a call's arguments) or of the target table (a switch's).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span32 {
    pub first: u32,
    pub len: u32,
}

impl Span32 {
    pub fn range(self) -> core::ops::Range<usize> {
        self.first as usize..(self.first + self.len) as usize
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stmt {
    /// `dest = src`.
    Use { dest: LocalId, src: Operand },
    /// `dest = src + by`, on integers.
    Step { dest: LocalId, src: LocalId, by: i64 },
    /// `dest = &of` (shared or mutable: the transform does not tell them apart).
    Borrow { dest: LocalId, of: LocalId },
    /// `dest = *via`, a copy.
    Load { dest: LocalId, via: LocalId },
    /// `*via = src`.
    Store { via: LocalId, src: Operand },
    /// `dest = func(args..)`: observable.
    Call { dest: LocalId, func: u32, args: Span32 },
    /// `dest = Future::poll(Pin::new_unchecked(&mut fut), cx)` as 0 (Pending) or 1 (Ready):
    /// observable; the answer is the schedule's.
    Poll { dest: LocalId, fut: LocalId },
    /// Runs the local's destructor: observable. The local must be initialised, and is not after.
    Drop(LocalId),
    /// The local's storage ends: no reference to it is used after.
    StorageDead(LocalId),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Term {
    Goto(BlockId),
    /// Target `min(max(on, 0), len - 1)`: 0 is the first, anything larger the last.
    Switch { on: LocalId, targets: Span32 },
    /// The coroutine's result: `Poll::Ready(value)`.
    Return(Operand),
    /// A suspension point (input only): `resume` when polled again, `drop` when the state is
    /// dropped while parked here.
    Suspend { point: PointId, resume: BlockId, drop: BlockId },
    /// The end of a cancel path: the state's drop glue returns.
    Cancelled,
    /// A machine's suspension (output only): store the point's tag, return `Poll::Pending`.
    Park(PointId),
    Unreachable,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BlockRow {
    pub stmts: Span32,
    pub term: Term,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LocalRow {
    /// Whether dropping the coroutine before its first poll must drop this upvar, and whether a
    /// `Drop` of it does anything. The adapter's fact, from the local's type.
    pub needs_drop: bool,
}

/// How a statement or terminator touches a local.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    /// Reads it (a move and a drop too).
    Use,
    /// Writes all of it.
    Def,
    /// Takes its address.
    Borrow,
    /// Ends its storage.
    Dead,
}

/// A malformed body, named (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IrError {
    /// The body has no block.
    Empty,
    /// A block was reserved and never filled.
    UnfilledBlock(BlockId),
    /// A block was begun twice.
    FilledTwice(BlockId),
    /// A statement or terminator was added with no block begun.
    NotInBlock,
    /// A block was begun while another was open.
    BlockOpen(BlockId),
    /// An upvar was declared after a local: upvars are the first locals.
    UpvarAfterLocal(LocalId),
    /// A statement or terminator of the block names a local past the table.
    NoSuchLocal(BlockId, LocalId),
    /// A terminator of the block names a block past the table.
    NoSuchBlock(BlockId, BlockId),
    /// A suspension names a point past the count.
    NoSuchPoint(BlockId, PointId),
    /// Two suspensions share a point.
    PointTwice(PointId),
    /// A point no suspension uses.
    PointUnused(PointId),
    /// A switch with no target.
    EmptySwitch(BlockId),
}

impl fmt::Display for IrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            IrError::Empty => write!(f, "the body has no block"),
            IrError::UnfilledBlock(b) => write!(f, "{b} was reserved and never filled"),
            IrError::FilledTwice(b) => write!(f, "{b} was begun twice"),
            IrError::NotInBlock => write!(f, "a statement or terminator outside a block"),
            IrError::BlockOpen(b) => write!(f, "a block was begun while {b} was open"),
            IrError::UpvarAfterLocal(l) => write!(f, "upvar {l} was declared after a local"),
            IrError::NoSuchLocal(b, l) => write!(f, "{b} names {l}, past the local table"),
            IrError::NoSuchBlock(b, t) => write!(f, "{b} names {t}, past the block table"),
            IrError::NoSuchPoint(b, p) => write!(f, "{b} names {p}, past the point count"),
            IrError::PointTwice(p) => write!(f, "two suspensions share {p}"),
            IrError::PointUnused(p) => write!(f, "no suspension uses {p}"),
            IrError::EmptySwitch(b) => write!(f, "the switch of {b} has no target"),
        }
    }
}

/// One coroutine body.
pub struct Body<A: Allocator> {
    pub locals: Vec<LocalRow, A>,
    pub upvars: u32,
    pub stmts: Vec<Stmt, A>,
    pub operands: Vec<Operand, A>,
    pub targets: Vec<BlockId, A>,
    pub blocks: Vec<BlockRow, A>,
    pub points: u32,
}

fn operand_access(o: Operand, f: &mut impl FnMut(Access, LocalId)) {
    match o {
        Operand::Copy(l) | Operand::Move(l) => f(Access::Use, l),
        Operand::Const(_) => {}
    }
}

impl<A: Allocator> Body<A> {
    pub fn entry(&self) -> BlockId {
        BlockId(0)
    }

    pub fn stmts_of(&self, b: BlockId) -> &[Stmt] {
        &self.stmts[self.blocks[b.index()].stmts.range()]
    }

    pub fn term(&self, b: BlockId) -> Term {
        self.blocks[b.index()].term
    }

    pub fn args(&self, s: Span32) -> &[Operand] {
        &self.operands[s.range()]
    }

    pub fn targets_of(&self, s: Span32) -> &[BlockId] {
        &self.targets[s.range()]
    }

    /// The successors of a block, in order; a suspension's are its resume block, then its
    /// cancel path.
    pub fn for_each_succ(&self, b: BlockId, mut f: impl FnMut(BlockId)) {
        match self.term(b) {
            Term::Goto(t) => f(t),
            Term::Switch { targets, .. } => {
                for &t in self.targets_of(targets) {
                    f(t);
                }
            }
            Term::Suspend { resume, drop, .. } => {
                f(resume);
                f(drop);
            }
            Term::Return(_) | Term::Cancelled | Term::Park(_) | Term::Unreachable => {}
        }
    }

    /// Every local a statement touches, and how.
    pub fn stmt_accesses(&self, s: Stmt, mut f: impl FnMut(Access, LocalId)) {
        match s {
            Stmt::Use { dest, src } => {
                operand_access(src, &mut f);
                f(Access::Def, dest);
            }
            Stmt::Step { dest, src, .. } => {
                f(Access::Use, src);
                f(Access::Def, dest);
            }
            Stmt::Borrow { dest, of } => {
                f(Access::Borrow, of);
                f(Access::Def, dest);
            }
            Stmt::Load { dest, via } => {
                f(Access::Use, via);
                f(Access::Def, dest);
            }
            Stmt::Store { via, src } => {
                f(Access::Use, via);
                operand_access(src, &mut f);
            }
            Stmt::Call { dest, args, .. } => {
                for &o in self.args(args) {
                    operand_access(o, &mut f);
                }
                f(Access::Def, dest);
            }
            Stmt::Poll { dest, fut } => {
                f(Access::Use, fut);
                f(Access::Def, dest);
            }
            Stmt::Drop(l) => f(Access::Use, l),
            Stmt::StorageDead(l) => f(Access::Dead, l),
        }
    }

    /// Every local a terminator reads.
    pub fn term_accesses(&self, t: Term, mut f: impl FnMut(Access, LocalId)) {
        match t {
            Term::Switch { on, .. } => f(Access::Use, on),
            Term::Return(o) => operand_access(o, &mut f),
            Term::Goto(_) | Term::Suspend { .. } | Term::Cancelled | Term::Park(_) | Term::Unreachable => {}
        }
    }

    /// The body's text: the same body gives the same text on any machine.
    pub fn print(&self, out: &mut impl fmt::Write) -> fmt::Result {
        write!(out, "locals {} upvars {} points {} drop:", self.locals.len(), self.upvars, self.points)?;
        for (i, row) in self.locals.iter().enumerate() {
            if row.needs_drop {
                write!(out, " _{i}")?;
            }
        }
        writeln!(out)?;
        for b in 0..self.blocks.len() {
            let b = BlockId(b as u32);
            writeln!(out, "{b}:")?;
            for &s in self.stmts_of(b) {
                write!(out, "  ")?;
                match s {
                    Stmt::Use { dest, src } => write!(out, "{dest} = {src}")?,
                    Stmt::Step { dest, src, by } => write!(out, "{dest} = {src} + {by}")?,
                    Stmt::Borrow { dest, of } => write!(out, "{dest} = &{of}")?,
                    Stmt::Load { dest, via } => write!(out, "{dest} = *{via}")?,
                    Stmt::Store { via, src } => write!(out, "*{via} = {src}")?,
                    Stmt::Call { dest, func, args } => {
                        write!(out, "{dest} = call f{func}(")?;
                        for (i, o) in self.args(args).iter().enumerate() {
                            if i > 0 {
                                write!(out, ", ")?;
                            }
                            write!(out, "{o}")?;
                        }
                        write!(out, ")")?;
                    }
                    Stmt::Poll { dest, fut } => write!(out, "{dest} = poll {fut}")?,
                    Stmt::Drop(l) => write!(out, "drop {l}")?,
                    Stmt::StorageDead(l) => write!(out, "dead {l}")?,
                }
                writeln!(out)?;
            }
            write!(out, "  -> ")?;
            match self.term(b) {
                Term::Goto(t) => write!(out, "goto {t}")?,
                Term::Switch { on, targets } => {
                    write!(out, "switch {on} [")?;
                    for (i, t) in self.targets_of(targets).iter().enumerate() {
                        if i > 0 {
                            write!(out, ", ")?;
                        }
                        write!(out, "{t}")?;
                    }
                    write!(out, "]")?;
                }
                Term::Return(o) => write!(out, "return {o}")?,
                Term::Suspend { point, resume, drop } => {
                    write!(out, "suspend {point} resume {resume} drop {drop}")?;
                }
                Term::Cancelled => write!(out, "cancelled")?,
                Term::Park(p) => write!(out, "park {p}")?,
                Term::Unreachable => write!(out, "unreachable")?,
            }
            writeln!(out)?;
        }
        Ok(())
    }
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Operand::Copy(l) => write!(f, "copy {l}"),
            Operand::Move(l) => write!(f, "move {l}"),
            Operand::Const(c) => write!(f, "const {c}"),
        }
    }
}

/// Checks a body's ids and that each point has exactly one suspension (a `Suspend` or a `Park`).
pub fn validate<A: Allocator, B: Allocator>(body: &Body<A>, alloc: B) -> Result<(), IrError> {
    if body.blocks.is_empty() {
        return Err(IrError::Empty);
    }
    let (locals, blocks) = (body.locals.len(), body.blocks.len());
    let mut seen = Vec::with_capacity_in(body.points as usize, alloc);
    seen.resize(body.points as usize, false);
    for b in 0..blocks {
        let b = BlockId(b as u32);
        let mut bad_local = None;
        let mut note = |_: Access, l: LocalId| {
            if l.index() >= locals && bad_local.is_none() {
                bad_local = Some(l);
            }
        };
        for &s in body.stmts_of(b) {
            body.stmt_accesses(s, &mut note);
        }
        let term = body.term(b);
        body.term_accesses(term, &mut note);
        if let Some(l) = bad_local {
            return Err(IrError::NoSuchLocal(b, l));
        }
        let mut bad_block = None;
        body.for_each_succ(b, |t| {
            if t.index() >= blocks && bad_block.is_none() {
                bad_block = Some(t);
            }
        });
        if let Some(t) = bad_block {
            return Err(IrError::NoSuchBlock(b, t));
        }
        match term {
            Term::Switch { targets, .. } if targets.len == 0 => return Err(IrError::EmptySwitch(b)),
            Term::Suspend { point, .. } | Term::Park(point) => {
                if point.0 >= body.points {
                    return Err(IrError::NoSuchPoint(b, point));
                }
                if seen[point.index()] {
                    return Err(IrError::PointTwice(point));
                }
                seen[point.index()] = true;
            }
            _ => {}
        }
    }
    for (p, &used) in seen.iter().enumerate() {
        if !used {
            return Err(IrError::PointUnused(PointId(p as u32)));
        }
    }
    Ok(())
}

/// Builds a body one block at a time. A mistake is kept (the first one) and returned by
/// [`BodyBuilder::finish`], so a long hand-made body reads straight.
pub struct BodyBuilder<A: Allocator + Clone> {
    body: Body<A>,
    filled: Vec<bool, A>,
    open: Option<(BlockId, u32)>,
    error: Option<IrError>,
}

impl<A: Allocator + Clone> BodyBuilder<A> {
    pub fn new_in(alloc: A) -> BodyBuilder<A> {
        BodyBuilder {
            body: Body {
                locals: Vec::new_in(alloc.clone()),
                upvars: 0,
                stmts: Vec::new_in(alloc.clone()),
                operands: Vec::new_in(alloc.clone()),
                targets: Vec::new_in(alloc.clone()),
                blocks: Vec::new_in(alloc.clone()),
                points: 0,
            },
            filled: Vec::new_in(alloc),
            open: None,
            error: None,
        }
    }

    fn fail(&mut self, e: IrError) {
        if self.error.is_none() {
            self.error = Some(e);
        }
    }

    /// A new upvar: initialised when the coroutine is made. Upvars come before every local.
    pub fn upvar(&mut self, needs_drop: bool) -> LocalId {
        let l = self.local(needs_drop);
        if l.0 == self.body.upvars {
            self.body.upvars += 1;
        } else {
            self.fail(IrError::UpvarAfterLocal(l));
        }
        l
    }

    pub fn local(&mut self, needs_drop: bool) -> LocalId {
        let l = LocalId(self.body.locals.len() as u32);
        self.body.locals.push(LocalRow { needs_drop });
        l
    }

    /// Reserves a block; the first reserved is the entry.
    pub fn block(&mut self) -> BlockId {
        let b = BlockId(self.body.blocks.len() as u32);
        self.body.blocks.push(BlockRow { stmts: Span32 { first: 0, len: 0 }, term: Term::Unreachable });
        self.filled.push(false);
        b
    }

    pub fn point(&mut self) -> PointId {
        let p = PointId(self.body.points);
        self.body.points += 1;
        p
    }

    /// Whether a block is open.
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    pub fn begin(&mut self, b: BlockId) {
        if let Some((open, _)) = self.open {
            self.fail(IrError::BlockOpen(open));
            return;
        }
        match self.filled.get(b.index()).copied() {
            None => self.fail(IrError::NoSuchBlock(b, b)),
            Some(true) => self.fail(IrError::FilledTwice(b)),
            Some(false) => self.open = Some((b, self.body.stmts.len() as u32)),
        }
    }

    pub fn push(&mut self, s: Stmt) {
        if self.open.is_none() {
            self.fail(IrError::NotInBlock);
            return;
        }
        self.body.stmts.push(s);
    }

    pub fn call(&mut self, dest: LocalId, func: u32, args: &[Operand]) {
        let first = self.body.operands.len() as u32;
        for &o in args {
            self.body.operands.push(o);
        }
        self.push(Stmt::Call { dest, func, args: Span32 { first, len: args.len() as u32 } });
    }

    /// Ends the open block with `term`.
    pub fn end(&mut self, term: Term) {
        let Some((b, first)) = self.open.take() else {
            self.fail(IrError::NotInBlock);
            return;
        };
        let len = self.body.stmts.len() as u32 - first;
        self.body.blocks[b.index()] = BlockRow { stmts: Span32 { first, len }, term };
        self.filled[b.index()] = true;
    }

    /// Ends the open block with a switch on `on` over `targets`.
    pub fn switch(&mut self, on: LocalId, targets: &[BlockId]) {
        let first = self.body.targets.len() as u32;
        for &t in targets {
            self.body.targets.push(t);
        }
        self.end(Term::Switch { on, targets: Span32 { first, len: targets.len() as u32 } });
    }

    pub fn finish(self) -> Result<Body<A>, IrError> {
        if let Some(e) = self.error {
            return Err(e);
        }
        if let Some((open, _)) = self.open {
            return Err(IrError::BlockOpen(open));
        }
        for (b, &done) in self.filled.iter().enumerate() {
            if !done {
                return Err(IrError::UnfilledBlock(BlockId(b as u32)));
            }
        }
        let alloc = self.body.locals.allocator().clone();
        validate(&self.body, alloc)?;
        Ok(self.body)
    }
}
