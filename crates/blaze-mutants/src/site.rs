//! What the front end says of a function for mutation, and of the tests that may reach it.
//!
//! A [`Site`] is one place in a function's checked MIR where an operator applies, described by
//! what the operator needs and nothing else: the operation and the class of its operands' type,
//! a constant and its type's range, the alternatives of the same type (arguments, fields,
//! variants, arms). The front end (`hir-ty-mir`, after type checking and before the optimizer)
//! makes them, and applies a chosen [`crate::operator::Mutation`] to the same MIR; because a
//! site lists only alternatives the type checker already accepts, no mutant is ever rejected
//! (the cost of a mutant is one codegen at the fast tier).
//!
//! Every [`Role`] is decided from the typed tree (what a call or macro resolves to, which
//! attribute an item carries), never by matching text: a function called `log_size` that
//! computes a size is behaviour.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

use blaze_ids::ContentId;
use blaze_program::key::{BodyKey, ItemId};
use blaze_program::record::TestKey;

use crate::operator::Operator;

/// A position inside an item: the line counted from the item's first line, and the byte column
/// (rule 71). It becomes `file:line:col` only when printed ([`crate::report::LineTable`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct ItemPos {
    pub line: u32,
    pub col: u32,
}

/// Where a site is in the function's MIR: a block and a statement in it, or its terminator.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct MirLoc {
    pub block: u32,
    /// [`MirLoc::TERMINATOR`] for the block's terminator.
    pub statement: u32,
}

impl MirLoc {
    pub const TERMINATOR: u32 = u32::MAX;
}

/// What an operator needs to know of a value's type.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum TyClass {
    Bool,
    /// An integer of `bits` (1 to 128), signed or not.
    Int { signed: bool, bits: u8 },
    Float,
    Char,
    Other,
}

/// A comparison, as MIR's `BinOp` has it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum CmpOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

impl CmpOp {
    pub fn symbol(self) -> &'static [u8] {
        match self {
            CmpOp::Lt => b"<",
            CmpOp::Le => b"<=",
            CmpOp::Gt => b">",
            CmpOp::Ge => b">=",
            CmpOp::Eq => b"==",
            CmpOp::Ne => b"!=",
        }
    }
}

/// An arithmetic or bitwise operation, as MIR's `BinOp` has it (a checked `+` is its
/// `AddWithOverflow` pair: the front end rewrites the pair together).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

impl ArithOp {
    pub fn symbol(self) -> &'static [u8] {
        match self {
            ArithOp::Add => b"+",
            ArithOp::Sub => b"-",
            ArithOp::Mul => b"*",
            ArithOp::Div => b"/",
            ArithOp::Rem => b"%",
            ArithOp::BitAnd => b"&",
            ArithOp::BitOr => b"|",
            ArithOp::BitXor => b"^",
            ArithOp::Shl => b"<<",
            ArithOp::Shr => b">>",
        }
    }

    /// Whether the operation is defined on operands of `ty`.
    pub fn valid_on(self, ty: TyClass) -> bool {
        match ty {
            TyClass::Int { .. } => true,
            TyClass::Float => matches!(self, ArithOp::Add | ArithOp::Sub | ArithOp::Mul | ArithOp::Div | ArithOp::Rem),
            TyClass::Bool => matches!(self, ArithOp::BitAnd | ArithOp::BitOr | ArithOp::BitXor),
            TyClass::Char | TyClass::Other => false,
        }
    }
}

/// What the front end proved about how a constant is used, so that a change it absorbs is known
/// equivalent without running anything.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Absorb {
    None,
    /// Every use clamps the value to `lo..=hi` first (`.min`, `.max`, `.clamp` on it, a
    /// saturating conversion): a value that clamps to the original's clamp is the same program.
    Clamp { lo: i128, hi: i128 },
}

/// A statement with an effect.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Effectful {
    /// A write to a place.
    Write,
    /// A call, by whether its callee is proved pure (`hir-ty-pure`'s effect summary, or
    /// `#[blaze::pure]`) and whether its result is used.
    Call { pure: bool, result_used: bool },
}

/// One kind of site, with exactly the alternatives its type admits.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum SiteKind {
    /// The function's return value. `default` is the text of a default of the return type when
    /// the front end can build one (a `Default` impl it can call, an integer, `()`, `None`);
    /// `args` the arguments of the return type, by index and name; `fields`, when the return
    /// type is a struct, the fields whose type has a default, by index and name.
    Return {
        default: Option<Vec<u8, Global>>,
        args: Vec<(u8, Vec<u8, Global>), Global>,
        fields: Vec<(u16, Vec<u8, Global>), Global>,
    },
    /// A `bool` that decides a branch (`SwitchInt` on it).
    Branch,
    Compare { op: CmpOp, ty: TyClass },
    Arith { op: ArithOp, ty: TyClass },
    /// A constant operand. A `bool` is 0 or 1 with [`TyClass::Bool`].
    Const { value: i128, ty: TyClass, absorb: Absorb },
    /// A statement whose effect is a write or a call.
    Effect(Effectful),
    /// A `match`: each arm's body as one line of text, and the pairs `(arm, from)` where arm
    /// `arm`'s body may be replaced by arm `from`'s (same bindings, same type).
    Arms { bodies: Vec<Vec<u8, Global>, Global>, pairs: Vec<(u16, u16), Global> },
    /// A call's arguments as text, and the pairs of the same type that may be swapped.
    CallArgs { args: Vec<Vec<u8, Global>, Global>, pairs: Vec<(u8, u8), Global> },
    /// An early return the rest of the function is well-typed without (`if c { return x; }`, not
    /// a `?` whose value is used).
    EarlyReturn,
    /// An enum value constructed: its variant, and the variants of the same enum that take the
    /// same fields, by index and name.
    Construct { variant: u16, alternatives: Vec<(u16, Vec<u8, Global>), Global> },
}

/// What part a site plays, from the typed tree.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Role {
    Behaviour,
    /// Inside a call that resolves into a logging facade (`log`'s macros, `eprintln!`).
    Logging,
    /// A tracing span or event, or a field of one.
    Tracing,
    /// A `Debug` impl's body, or a `format_args!` whose only use is a `Debug` or `Display` for
    /// people.
    DebugFormat,
    /// A message for people: a panic's or an assertion's message, an error's `Display` text.
    Message,
    /// A counter or metric update (`blaze_profile`'s counters, a metrics facade).
    Metrics,
    /// A choice that changes speed, not behaviour: `with_capacity`, `reserve`, `shrink_to_fit`,
    /// a prefetch, an inline or likely hint.
    PerfHint,
    /// Code under `#[cfg(test)]` or in a test function.
    CfgTest,
    /// An `assert!`'s or `debug_assert!`'s condition and body.
    Assertion,
    /// Code only on a path to `unreachable!`, `unimplemented!`, `todo!` or an
    /// `unreachable_unchecked`.
    Unreachable,
    /// Code a macro from outside the crate generated (a derive, a proc macro).
    Generated,
}

/// One mutation site.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Site {
    pub kind: SiteKind,
    pub at: MirLoc,
    pub pos: ItemPos,
    pub role: Role,
    /// Whether some path reads the value the site makes (for a write: the place it writes).
    pub read: bool,
    /// The source line, trimmed: what a diagnostic shows as the original.
    pub text: Vec<u8, Global>,
    /// The bytes of `text` a mutation replaces (an operator, a constant, a condition, a
    /// statement, a return expression, an argument list, a variant's name).
    pub token: (u32, u32),
}

/// A mutant a person or an agent judged equivalent, as the item's attribute says it:
/// `#[blaze::equivalent_mutant(rule = "mutants::cmp_boundary", line = 3, reason = "...")]`.
/// Never a comment: the attribute is checked (its rule and line must name a site) and counted
/// by the ratchet ([`crate::report::Score::marked`]).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Mark {
    pub operator: Operator,
    /// The item-relative line; `None` for every site of the operator in the item.
    pub line: Option<u32>,
    pub reason: Vec<u8, Global>,
}

/// One function, as the front end gives it for mutation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FnFacts {
    pub item: ItemId,
    /// Its body's key now: a verdict is keyed by it.
    pub body: BodyKey,
    /// Its path, for people (`crate::module::name`).
    pub path: Vec<u8, Global>,
    /// The role of the whole function (a `Debug::fmt`, a test, generated code), or
    /// [`Role::Behaviour`].
    pub role: Role,
    pub sites: Vec<Site, Global>,
    /// The functions that took this one's code in at the tier under test, at any depth
    /// (`blaze_program::program::Program`'s inliners table): a mutant remakes them too.
    pub inliners: Vec<ItemId, Global>,
    pub marks: Vec<Mark, Global>,
    /// What one fast-tier codegen of it costs, from its last instance record (nanoseconds).
    pub make_ns: u64,
}

/// A test program (a test binary's manifest) by its manifest's key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ProgramRef(pub ContentId);

/// One test, as its last record and its program's manifest give it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TestInfo {
    pub key: TestKey,
    /// Its path, for people.
    pub name: Vec<u8, Global>,
    pub program: ProgramRef,
    /// How long its last run took (nanoseconds): its timeout and the budget's estimate.
    pub recorded_ns: u64,
    /// The items whose instances its reach lists (`blaze_program::reach::Reach::functions`),
    /// sorted, each once.
    pub reach: Vec<ItemId, Global>,
}
