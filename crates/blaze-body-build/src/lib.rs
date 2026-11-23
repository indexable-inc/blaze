//! One drop-elaborated body at one instance's ground arguments to one LIR function
//! (docs/v2/plan/design/backend-next.md, "One lowering", D119 point 5): a linear pass with a
//! substitution. Blocks become LIR blocks and rows become instructions; nothing the body already
//! says (scopes, temporaries, match lowering, drop flags, overflow checks) is decided again here.
//!
//! The output is blaze-build's [`Built`], the walk's own: the two builders are one seam with two
//! instantiations (rule 78), tested equal by running what each builds.
//!
//! # The frame (frame.rs)
//!
//! A local is an LIR value when its address is never taken and its layout is one scalar or a
//! scalar pair; every other local is a stack slot. One pass over the rows finds the
//! address-taken locals: a borrow or raw borrow of the local or a place inside it, a projection
//! written through it, a drop of it. A local of an enum with a tag is held in a slot too, so its
//! tag is written and read in memory by layout's one encoding. A parameter passed by reference
//! is held at the caller's copy, and the return place of a result passed by reference at the
//! hidden result pointer.
//!
//! # Block parameters
//!
//! Every value local is a parameter of every body block, in local order: a block starts with
//! the values its predecessor holds for each, and an edge passes them (undefined, `uninit`, for
//! one not yet written). A cleanup block's first parameter is the exception. The entry is a
//! block of its own, the function's parameters, which jumps to the body's first block.
//!
//! # Unwinding
//!
//! Under [`blaze_build::Panic::Unwind`] a call, drop or panic whose row names a cleanup block
//! (and whose callee may unwind) is an `invoke`: its normal edge goes to a fresh block that
//! writes the result and jumps on, its unwind edge to a fresh landing block that takes the
//! exception and jumps to the body's cleanup block with it and the values. Under
//! [`blaze_build::Panic::Abort`] every call is plain and no cleanup block is built (D119 point 2).
#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]


extern crate alloc;
use alloc::vec::Vec as AllocVec;
use blaze_own::Vec;
mod frame;
mod term;
mod value;

#[cfg(test)]
mod tests;

use core::alloc::Allocator;
use core::fmt;

use blaze_body::{Block as BodyBlock, Local, Stmt, Verified};
use blaze_build::drop_facts::NeedsDropError;
use blaze_build::layout::{LayoutError, LayoutId, Layouts};
use blaze_build::{Adts, Built, Instance, Panic, Subst};
use blaze_lir::func::{RefId, SlotId};
use blaze_lir::{Block, Builder, Full, Ty, Value};
use blaze_typeck::{ItemKey, PTy, PTys};

/// The target's pointer width: the walk's (blaze-build `PTR_BITS`).
const PTR_BITS: u32 = 64;

/// Why a body was not built: each names the row (or the item) it stopped at and what it is.
#[derive(Debug)]
pub enum BuildError {
    /// Row `stmt` is a construct this builder does not translate yet.
    Unsupported { stmt: Stmt, what: &'static str },
    /// The body or its facts contradict themselves at `stmt` (`None`: in the frame, before any
    /// row).
    Malformed { stmt: Option<Stmt>, what: &'static str },
    /// A type with no layout, asked for at `stmt`.
    Layout { stmt: Option<Stmt>, error: LayoutError },
    /// A question to the facts seam failed at `stmt` (grounding a type, a projection, a
    /// `Drop` impl).
    Facts { stmt: Option<Stmt>, error: blaze_build::BuildError },
    /// Row `stmt` needs lang fn `name`, which the program does not declare.
    NoLangFn { stmt: Stmt, name: &'static str },
    /// Whether a type needs drop has no answer: the dropped place's at row `stmt`, or (`None`)
    /// a zero-sized parameter's or result's in the frame, whose pass depends on it.
    NeedsDrop { stmt: Option<Stmt>, error: NeedsDropError<blaze_build::BuildError> },
    /// The facts give no ABI for fn item `item`.
    NoAbi { item: ItemKey },
    /// Row `stmt` calls `#[track_caller]` fn `name`, whose hidden location argument is not
    /// built here yet.
    TrackCaller { stmt: Stmt, name: &'static str },
    /// The LIR builder or its verifier refused the function.
    Lir(blaze_lir::BuildError),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = |s: &Option<Stmt>| s.map_or(u32::MAX, |s| s.0);
        match self {
            BuildError::Unsupported { stmt, what } => write!(f, "row {} of the body: {what} is not built yet", stmt.0),
            BuildError::Malformed { stmt, what } => write!(f, "row {} of the body: a malformed body or fact: {what}", at(stmt)),
            BuildError::Layout { stmt, error } => write!(f, "row {} of the body: {error}", at(stmt)),
            BuildError::Facts { stmt, error } => write!(f, "row {} of the body: {error}", at(stmt)),
            BuildError::NoLangFn { stmt, name } => write!(f, "row {} of the body needs lang fn `{name}`, which the program does not declare", stmt.0),
            BuildError::NeedsDrop { stmt, error } => write!(f, "row {} of the body: {error}", at(stmt)),
            BuildError::NoAbi { item } => write!(f, "the facts give no ABI for fn item {:#x}", item.0),
            BuildError::TrackCaller { stmt, name } => {
                write!(f, "row {} of the body calls `#[track_caller]` fn `{name}`, whose location argument is not built yet", stmt.0)
            }
            BuildError::Lir(error) => write!(f, "the LIR function: {error}"),
        }
    }
}

impl core::error::Error for BuildError {}

/// How a local is held (the module doc, "The frame").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Held {
    /// A zero-sized value: nothing.
    None,
    /// One LIR value of this type.
    One(Ty),
    /// Two LIR values: a scalar pair's halves.
    Two(Ty, Ty),
    /// Memory: a stack slot of the frame's own.
    Slot(SlotId),
    /// Memory at a pointer the function was given: a parameter's caller's copy, or the hidden
    /// result pointer.
    At(Value),
}

impl Held {
    pub(crate) fn is_value(self) -> bool {
        matches!(self, Held::One(_) | Held::Two(..))
    }
}

/// A value as the builder holds it while it builds one row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum V {
    None,
    One(Value),
    Two(Value, Value),
    /// An aggregate in memory, by its address and its layout.
    Mem { ptr: Value, off: i32, lay: LayoutId },
}

/// How a value crosses a call of the Rust ABI: blaze-build's one plan, the walk's.
pub(crate) use blaze_build::abi::Pass;

/// The state of one build.
pub(crate) struct Cx<'a, 'g, 't, A: Allocator + Copy> {
    pub(crate) v: Verified<'a>,
    /// The body's own type table, which its locals, constants and projections name.
    pub(crate) from: PTys<'a>,
    pub(crate) subst: &'a mut Subst<'g, 't>,
    pub(crate) adts: &'a dyn Adts,
    pub(crate) alloc: A,
    pub(crate) b: Builder<A, Full>,
    pub(crate) layouts: Layouts<A>,
    pub(crate) panic: Panic,
    /// By local: its ground type, its layout and how it is held.
    pub(crate) tys: Vec<PTy, A>,
    pub(crate) lays: Vec<LayoutId, A>,
    pub(crate) held: Vec<Held, A>,
    /// By value local: the values it holds now, in the block being built.
    pub(crate) cur: Vec<[Option<Value>; 2], A>,
    /// By body block: its LIR block (`None`: a cleanup block under `Panic::Abort`, never built).
    pub(crate) blocks: Vec<Option<Block>, A>,
    /// The exception, in a cleanup block.
    pub(crate) exn: Option<Value>,
    /// How the result leaves: `_0`'s pass.
    pub(crate) ret: Pass,
    /// By function reference place: the instance, and the reference made for it.
    pub(crate) refs: AllocVec<Instance, A>,
    pub(crate) ref_ids: Vec<RefId, A>,
    /// By static reference place: the static item, and the reference made for it.
    pub(crate) statics: AllocVec<ItemKey, A>,
    pub(crate) static_ids: Vec<RefId, A>,
    /// By thread-local reference place: the static item, and the reference made for it.
    pub(crate) thread_locals: AllocVec<ItemKey, A>,
    pub(crate) tls_ids: Vec<RefId, A>,
    /// The row being built, which every error names.
    pub(crate) stmt: Option<Stmt>,
}

/// Builds `body`, whose types are of `from`, as fn item `item`'s instance at `subst`'s ground
/// arguments, asking `adts` what the body does not say; every column in `alloc`.
///
/// The body is drop-elaborated and verified (`Verified` is the proof); the result is verified
/// LIR. An unsupported row is refused by name ([`BuildError::Unsupported`]), never skipped.
pub fn build<A: Allocator + Copy>(
    body: Verified<'_>,
    from: PTys<'_>,
    item: ItemKey,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    match adts.abi(item) {
        Some(blaze_build::Abi::Rust) => {}
        Some(blaze_build::Abi::C | blaze_build::Abi::RustCall) => {
            return Err(BuildError::Unsupported { stmt: body.entry(), what: "a body of a fn of the C or \"rust-call\" ABI" });
        }
        None => return Err(BuildError::NoAbi { item }),
    }
    let mut cx = Cx {
        v: body,
        from,
        subst,
        adts,
        alloc: alloc,
        b: Builder::new_in(PTR_BITS, alloc),
        layouts: Layouts::new_in(alloc),
        panic: adts.panic(),
        tys: Vec::new_in(alloc),
        lays: Vec::new_in(alloc),
        held: Vec::new_in(alloc),
        cur: Vec::new_in(alloc),
        blocks: Vec::new_in(alloc),
        exn: None,
        ret: Pass::Ignore,
        refs: AllocVec::new_in(alloc),
        ref_ids: Vec::new_in(alloc),
        statics: AllocVec::new_in(alloc),
        static_ids: Vec::new_in(alloc),
        thread_locals: AllocVec::new_in(alloc),
        tls_ids: Vec::new_in(alloc),
        stmt: None,
    };
    cx.frame()?;
    cx.blocks()?;
    let func = cx.b.finish_verified().map_err(BuildError::Lir)?;
    Ok(Built {
        func,
        refs: cx.refs,
        statics: cx.statics,
        thread_locals: cx.thread_locals,
        // This builder marks no `unreachable` with its construct yet: its records carry no trap
        // rows (blaze-code record.rs `TrapSource`), so a fault in them is said by its code alone.
        traps: AllocVec::new_in(alloc),
        vtables: AllocVec::new_in(alloc),
        // This builder has no `copy` of its own (blaze-build walk.rs): none counted.
        copy_insts: 0,
    })
}

impl<A: Allocator + Copy> Cx<'_, '_, '_, A> {
    pub(crate) fn malformed(&self, what: &'static str) -> BuildError {
        BuildError::Malformed { stmt: self.stmt, what }
    }

    pub(crate) fn unsupported(&self, what: &'static str) -> BuildError {
        match self.stmt {
            Some(stmt) => BuildError::Unsupported { stmt, what },
            None => BuildError::Malformed { stmt: None, what },
        }
    }

    pub(crate) fn facts(&self, error: blaze_build::BuildError) -> BuildError {
        BuildError::Facts { stmt: self.stmt, error }
    }

    /// The row being built, for the errors that must name one.
    pub(crate) fn row(&self) -> Result<Stmt, BuildError> {
        self.stmt.ok_or(BuildError::Malformed { stmt: None, what: "a row's question asked outside any row" })
    }

    /// The LIR block of body block `b`.
    pub(crate) fn lir_block(&self, b: BodyBlock) -> Result<Block, BuildError> {
        match self.blocks.get(b.index()) {
            Some(Some(block)) => Ok(*block),
            Some(None) => Err(self.malformed("an edge to a cleanup block in a build that aborts on panic")),
            None => Err(self.malformed("an edge to a block past the body")),
        }
    }

    /// Local `l`'s index into the frame's columns.
    pub(crate) fn ix(&self, l: Local) -> Result<usize, BuildError> {
        let i = l.index();
        if i < self.held.len() { Ok(i) } else { Err(self.malformed("a local past the frame")) }
    }
}
