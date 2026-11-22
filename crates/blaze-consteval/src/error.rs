//! Why a constant has no value. Every refusal names the constant evaluated, the body and the
//! item-relative range it happened in (rule 19), and the rule of the Reference that refuses it.

use core::fmt;

use blaze_own::{Alloc, Vec};
use blaze_typeck::{ItemKey, Range};

use crate::lit::LitError;

/// Where in which body: the item whose body it is and the item-relative range of the
/// expression or pattern (rule 71).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct At {
    pub body: ItemKey,
    pub range: Range,
}

/// An arithmetic operation, as an error names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Neg,
    Shl,
    Shr,
}

/// One part of a panic's message, in order: a string argument, or an integer one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanicPart<'a> {
    Str(Vec<u8, Alloc<'a>>),
    Int(i128),
    Uint(u128),
    /// An argument of another type (its type is not printed).
    Other,
}

/// What refused the constant.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal<'a> {
    // The language's refusals (the Reference, "Constant evaluation": an error at compile time
    // where the same operation at run time would panic or be undefined).
    /// Arithmetic overflow: an error in a constant context whatever `overflow-checks` says.
    Overflow { op: Op },
    /// `x / 0`.
    DivByZero,
    /// `x % 0`.
    RemByZero,
    /// A shift by the width or more, or by a negative amount.
    ShiftTooFar { amount: i128, bits: u32 },
    /// `a[i]` with `i >= len`.
    IndexOutOfBounds { index: u64, len: u64 },
    /// A read of bytes never written ("Behavior considered undefined": reading uninitialized memory).
    Uninit { offset: u64 },
    /// An access outside its allocation.
    OutOfBounds { offset: u64, size: u64, len: u64 },
    /// A use of a pointer whose allocation has ended.
    Dangling,
    /// A pointer read or used as an integer: a constant has no addresses.
    PointerAsInt,
    /// An integer made into a pointer and used: it has no provenance.
    IntAsPointer,
    /// Reading only some of a pointer's bytes.
    PartialPointer { offset: u64 },
    /// Comparing two pointers (or a pointer's order): unknown until the program is placed.
    PointerCompare,
    /// A write through a shared reference or into a constant's memory.
    WriteToImmutable,
    /// Reading a `static mut` (or a static with interior mutability) in a constant.
    MutableStatic { item: ItemKey },
    /// A call of a fn that is not `const fn`.
    NotConstFn { item: ItemKey },
    /// A call of a `#[rustc_intrinsic]` fn blaze's intrinsic table does not hold.
    UnknownIntrinsic { item: ItemKey },
    /// A call of `item`, whose kind needs `fact`, which the world does not hold: pending
    /// (machine/late.rs `Wait::FnFact`), never "not a `const fn`".
    FnFact { item: ItemKey, fact: crate::Fact },
    /// A `bool` that is not 0 or 1, a `char` that is not a scalar value, an enum tag no
    /// variant has: invalid values ("Behavior considered undefined").
    InvalidBool { value: u128 },
    InvalidChar { value: u128 },
    InvalidTag { bits: u128 },
    /// `transmute` between types of different sizes.
    TransmuteSize { from: u64, to: u64 },
    /// `unreachable_unchecked` or an uninhabited value reached.
    Unreachable,
    /// `ctlz_nonzero(0)` or `cttz_nonzero(0)` (`name`): undefined (the pinned core,
    /// intrinsics/mod.rs: "returns `undef` when given an `x` with value `0`"; rustc's const
    /// evaluator, interpret/intrinsics.rs, throws "`{name}` called on 0").
    NonzeroOnZero { name: &'static str },
    /// The constant panicked: the message's parts.
    Panic { parts: Vec<PanicPart<'a>, Alloc<'a>> },
    /// An implicit discriminant one past its `repr` type's largest value.
    DiscriminantOverflow { variant: u32 },

    // Bounds (rule 18).
    /// More steps than the evaluation's bound.
    StepLimit { limit: u64 },
    /// More live bytes than the evaluation's bound.
    MemoryLimit { limit: u64 },
    /// A deeper call stack than the bound.
    CallDepth { limit: u32 },
    /// Constants whose values need each other, in the order met; the first is needed again.
    Cycle { chain: Vec<ItemKey, Alloc<'a>> },

    // What is not built: refused by name, never skipped (rule 3).
    Unsupported { what: &'static str },

    // The input is not one the checker makes.
    Malformed { what: &'static str },
    /// A literal whose text gives no value.
    Literal(LitError),
    /// The world has no body, ADT or definition this evaluation reads.
    Missing { what: &'static str, item: ItemKey },
    /// `needs_drop::<T>()` reached a part of `T` it cannot judge (a type that is not ground,
    /// an array length that is not a value, a `Drop` impl the solver could not select), named.
    NeedsDrop { what: &'static str },
    /// A type of the body holds the projection `<_ as trait_>::name` that no impl answered:
    /// pending (machine/late.rs), the front end makes the impls and asks again.
    Projection { trait_: ItemKey, name: blaze_typeck::Name },
    /// The type of field `field` (its row in the ADT's fields, named `name`) of ADT `adt` has
    /// no ground form: `projection` is `(trait, associated type)` when a projection no impl
    /// answered is why; `None` for another grounding refusal (an opaque type not answered).
    FieldGround { adt: ItemKey, field: u32, name: blaze_typeck::Name, projection: Option<(ItemKey, blaze_typeck::Name)> },
    /// A layout asked of the extern type `item` (`extern { type T; }`): it has no size or
    /// alignment (blaze-build `layout::Refusal::NoSize`). Final: no later fact gives it one.
    NoSize { item: ItemKey },
}

/// A refusal of one constant: the constant, where, and what.
#[derive(Debug, PartialEq, Eq)]
pub struct ConstError<'a> {
    /// The constant being evaluated (a const, static, inline const or discriminant).
    pub constant: ItemKey,
    /// Where it happened: `None` for a limit of the whole evaluation.
    pub at: Option<At>,
    pub refusal: Refusal<'a>,
}

impl fmt::Display for ConstError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "constant {:#x}", self.constant.0)?;
        if let Some(at) = self.at {
            write!(f, " (body {:#x}, bytes {}..{})", at.body.0, at.range.start, at.range.end)?;
        }
        write!(f, ": ")?;
        match &self.refusal {
            Refusal::Overflow { op } => write!(f, "attempt to {op:?} with overflow (an error in a constant context)"),
            Refusal::DivByZero => write!(f, "attempt to divide by zero"),
            Refusal::RemByZero => write!(f, "attempt to calculate the remainder with a divisor of zero"),
            Refusal::ShiftTooFar { amount, bits } => write!(f, "attempt to shift by {amount}, past the {bits}-bit width"),
            Refusal::IndexOutOfBounds { index, len } => write!(f, "index out of bounds: the len is {len} but the index is {index}"),
            Refusal::Uninit { offset } => write!(f, "reading uninitialized memory at offset {offset}"),
            Refusal::OutOfBounds { offset, size, len } => {
                write!(f, "an access of {size} bytes at offset {offset} of an allocation of {len} bytes")
            }
            Refusal::Dangling => write!(f, "a use of a dangling pointer"),
            Refusal::PointerAsInt => write!(f, "a pointer used as an integer: a constant has no addresses"),
            Refusal::IntAsPointer => write!(f, "an integer used as a pointer: it points to no allocation"),
            Refusal::PartialPointer { offset } => write!(f, "reading part of a pointer at offset {offset}"),
            Refusal::PointerCompare => write!(f, "pointers compared in a constant: their addresses are not known"),
            Refusal::WriteToImmutable => write!(f, "a write to immutable memory"),
            Refusal::MutableStatic { item } => write!(f, "a constant reads mutable static {:#x}", item.0),
            Refusal::NotConstFn { item } => write!(f, "a call of {:#x}, which is not a `const fn`", item.0),
            Refusal::UnknownIntrinsic { item } => write!(f, "a call of {:#x}, an intrinsic blaze's intrinsics table does not hold", item.0),
            Refusal::FnFact { item, fact } => write!(f, "a call of {:#x}: its kind is not known: {fact}", item.0),
            Refusal::InvalidBool { value } => write!(f, "{value} is not a valid `bool`"),
            Refusal::InvalidChar { value } => write!(f, "{value:#x} is not a valid `char`"),
            Refusal::InvalidTag { bits } => write!(f, "{bits:#x} is the tag of no variant"),
            Refusal::TransmuteSize { from, to } => write!(f, "`transmute` from {from} bytes to {to} bytes"),
            Refusal::Unreachable => write!(f, "entered unreachable code"),
            Refusal::NonzeroOnZero { name } => write!(f, "`{name}` called on 0"),
            Refusal::Panic { parts } => {
                write!(f, "the constant panicked: ")?;
                for p in parts.iter() {
                    match p {
                        PanicPart::Str(s) => write!(f, "{}", core::str::from_utf8(s).unwrap_or("<not UTF-8>"))?,
                        PanicPart::Int(v) => write!(f, "{v}")?,
                        PanicPart::Uint(v) => write!(f, "{v}")?,
                        PanicPart::Other => write!(f, "<value>")?,
                    }
                }
                Ok(())
            }
            Refusal::DiscriminantOverflow { variant } => {
                write!(f, "variant {variant}'s implicit discriminant overflows its `repr` type")
            }
            Refusal::StepLimit { limit } => write!(f, "more than {limit} steps"),
            Refusal::MemoryLimit { limit } => write!(f, "more than {limit} live bytes"),
            Refusal::CallDepth { limit } => write!(f, "calls deeper than {limit}"),
            Refusal::Cycle { chain } => {
                write!(f, "a cycle of constants:")?;
                for k in chain.iter() {
                    write!(f, " {:#x}", k.0)?;
                }
                Ok(())
            }
            Refusal::Unsupported { what } => write!(f, "{what} is not evaluated yet"),
            Refusal::Malformed { what } => write!(f, "a malformed body or record: {what}"),
            Refusal::Literal(e) => write!(f, "a literal with no value: {e:?}"),
            Refusal::Missing { what, item } => write!(f, "the world has no {what} for {:#x}", item.0),
            Refusal::NeedsDrop { what } => write!(f, "`needs_drop` reached {what} (the Reference, \"Destructors\")"),
            Refusal::Projection { trait_, name } => {
                write!(f, "the projection `<_ as {:#x}>::{:#x}` in a type of the body is not normalized", trait_.0, name.0)
            }
            Refusal::FieldGround { adt, field, name, projection } => {
                write!(f, "field {field} (name {:#x}) of ADT {:#x} has no ground form: ", name.0, adt.0)?;
                match projection {
                    Some((tr, assoc)) => write!(f, "the projection `<_ as {:#x}>::{:#x}` is not normalized", tr.0, assoc.0),
                    None => write!(f, "grounding its type was refused"),
                }
            }
            Refusal::NoSize { item } => write!(f, "the extern type {:#x} has no size or alignment: it is never laid out by value", item.0),
        }
    }
}

impl core::error::Error for ConstError<'_> {}
