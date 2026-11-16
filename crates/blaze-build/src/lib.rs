//! A type-checked body to LIR (crates/blaze-cli/tests/ladder/README.md, step (b)): one body in
//! blaze-typeck's form ([`BodyView`]), its check's record ([`Checked`]) and its owner's
//! signature ([`FnSig`]) to one LIR function ([`blaze_lir::Func`]), verified.
//!
//! The function it makes is the baseline tier's input: every local and every value that
//! outlives its expression lives in a LIR stack slot (`slot_addr`, `load`, `store`); `if`,
//! `match`, `loop`, `break`, `continue` and `return` are blocks and branches; a call of a fn
//! item is a `call` of function reference `place`, where `refs[place]` is the callee's
//! [`Instance`]. A generic fn's body is checked once and built once per instance ([`Subst`]):
//! every type is grounded at the instance's arguments before it is laid out.
//! The walk of the body uses explicit stacks: no recursion on the input.
//!
//! Anything outside the subset is [`BuildError::Unsupported`], naming the expression and the
//! construct: never a silent skip (AGENTS.md rule 3).
//!
//! # Values
//!
//! An integer is its width (`isize`/`usize` are `i64`: the target's pointers are 64 bits), a
//! `bool` an `i8` of 0 or 1, a reference or raw pointer a `ptr`; `()`, `!`, a fn item and every
//! zero-sized type have no value. An aggregate (a struct, a non-empty tuple, an array) is a
//! place: a pointer and an offset, most often into a stack slot. Every size, alignment, field
//! offset, array stride, tag and niche is [`layout::Layouts`]' (rule 30: one layout), from
//! the facts [`Adts::facts`] gives: a `repr(Rust)` struct's or tuple's fields are reordered by
//! falling alignment, a `repr(C)` one's kept in order; `repr(packed)` is refused by name. A
//! fn pointer is a `ptr`. A by-value use of an aggregate place copies its scalar
//! leaves where the value lands (a binding, a field of a literal, a caller's copy, the result
//! slot): unrolled up to 16 leaves, in a loop over an array's elements beyond that, never
//! reading padding. Code after a diverging expression is built into blocks no edge reaches,
//! where a value a later expression needs is `uninit`.
//!
//! An index `a[i]` of an array compares `i` with the length first; out of range it calls the
//! program's `panic_bounds_check` lang item fn (`Adts::lang_fn`) with `(i, len)`, which never
//! returns.
//!
//! # blaze's Rust ABI
//!
//! Rust's ABI is unspecified, so blaze defines its own, over the target's C convention
//! (blaze-lir's `Conv::C`; the ladder's shortcut 10):
//!
//! - A scalar parameter or result is one integer or pointer, a `bool` and integers under 32
//!   bits widened by their signedness.
//! - A parameter or result of a zero-sized type has no value and no parameter.
//! - An aggregate parameter is passed as a pointer to a copy the caller made for the call: the
//!   callee owns that memory for the call's length and uses it in place as the binding's home
//!   (no second copy).
//! - An aggregate result is written by the callee through a hidden first pointer parameter to
//!   a slot of the caller's (`sret` by convention; no LLVM attribute); the function then
//!   returns nothing.
//!
//! # The C ABI
//!
//! An `extern "C"` fn, built or called, and a call through an `extern "C"` fn pointer follow
//! AAPCS64 as Apple's arm64 varies it, each value classed by [`cabi::class_of`]:
//!
//! - An integer, `bool` or pointer (or a newtype of one) is one register; one under 32 bits
//!   is zero- or sign-extended by the caller.
//! - A composite of at most 16 bytes is one or two `i64` registers read from its place; the
//!   last is read in loads of falling powers of two inside the size (12 bytes: an `i64`,
//!   then an `i32` zero-extended), so no byte past the value is read. A result of 9 to 16
//!   bytes is two `i64` returns (`project` 0 and 1) stored at offsets 0 and 8.
//! - A composite past 16 bytes is passed by the address of a copy (`byval`) and returned
//!   through `sret`.
//! - Refused by name ([`BuildError::CAbi`]): a float, a homogeneous float aggregate (the SIMD
//!   registers), a 16-byte-aligned composite (its even register pair), a 128-bit integer, and
//!   what [`cabi`] refuses (a union, an enum with fields).
//!
//! # Fn pointers
//!
//! A fn item coerced to a pointer (`ReifyFnPointer`) is `func_addr` of its instance's `Func`
//! reference. A call through a pointer is `call_indirect` of a signature lowered from the
//! call's argument and result types by the pointer's ABI (Rust's, or C's above).
//!
//! # Statics
//!
//! A path to a static ([`Adts::static_item`]) is a place: `data_addr` of a `Static`
//! reference, or `tls_addr` of a `Tls` one for a `#[thread_local]`; [`Built::statics`] and
//! [`Built::thread_locals`] list them by reference place. A read copies from it; an assignment
//! writes through it.
//!
//! # Consts
//!
//! A path to a const is built from its evaluated record ([`Adts::const_value`]), never from its
//! body: a scalar is a constant of the root's bytes at its width, little-endian; anything else
//! is a copy from the root's data row, each allocation of the record a row of its own, their
//! relocations to each other, to statics and (refused by name for now) to fns.
//!
//! # Drops
//!
//! Drop elaboration follows the Rust reference's "Destructors" chapter; each rule here names
//! the case it covers.
//!
//! - **What needs drop.** A type needs drop when it has a `Drop` impl ([`Adts::drop_impl`],
//!   selected by the checker's solver) or a field, element or tuple part that needs drop.
//!   Scalars, references and raw pointers never do.
//! - **Drop glue.** A drop is a call of the program's `drop_in_place::<T>` lang fn with the
//!   value's address. Its written body is never built: its instance at `T` is built as the
//!   glue (rustc's shim): `<T as Drop>::drop(&mut *p)` when `T` has the impl, then each field
//!   that needs drop in declaration order, a tuple's parts in order, an array's elements first
//!   to last.
//! - **Scopes.** Each block is a drop scope; a fn's parameters are the outermost. When control
//!   leaves a scope its bindings that are still initialized are dropped in reverse order of
//!   declaration: at the block's end (after its value is made, a value in a dropped binding's
//!   memory copied out first), and on `break`, `continue` and `return` for every scope they
//!   leave, innermost first, parameters last.
//! - **Moves.** Using a binding by value moves it: it is no longer initialized and its scope
//!   does not drop it. A value moved into a call is the callee's parameter, dropped by the
//!   callee. Assigning to a binding or place that needs drop drops the old value first (none
//!   for a binding not yet initialized), after the right side and the place are evaluated.
//! - **Drop flags.** A binding's state is known statically unless a move or an assignment of
//!   it sits under a branch or a loop inside its scope (an `if` arm, a `match` arm, a loop
//!   body, the right side of `&&`/`||`): only then does it get a flag, a byte slot set at its
//!   `let`, cleared on a move and tested at each drop.
//! - **Temporaries.** The value of an expression statement is dropped at the end of the
//!   statement; so is the value of `let _ = <value expression>`, while `let _ = <place>` moves
//!   nothing.
//!
//! Refused by name, never skipped: a move out of a field, an index or a dereference (a
//! partial move), a borrowed temporary whose type needs drop (its drop at the end of the
//! enclosing statement, and the `let` lifetime extension, are not built), a `_` part of a
//! pattern over a part that needs drop, and a zero-sized value that needs drop.
//!
//! A `match` by value on an enum that needs drop (docs/v2/plan/design/codegen.md, "A `match`
//! by value on an enum that needs drop"): a local scrutinee is matched in place, an arm's
//! by-value binding of a field moves it out and clears its bit in the local's field flags,
//! and the local's drops drop the live variant's fields still flagged. A temporary's rest is
//! dropped at its arm's end, after the arm's bindings, with no flag. Refused by name: a move
//! below a field, a guard on a moving arm, a temporary inside a larger expression, a move out
//! of an enum with `Drop`, a field past 64. A `match` through a reference (`match &x`, the
//! default binding mode `ref`) binds each part by its address and moves nothing; an enum's
//! glue switches on its live variant, a union's drops no field, `ManuallyDrop`'s
//! ([`Adts::lang_adt`]) neither.
//!
//! # Enums
//!
//! An enum's layout is [`layout::Layouts`]': a tag, or a niche in a field (`Option<&T>` is
//! a pointer, `None` null), its variants' fields where it places them. The walk reads it, and
//! writes and decodes it only through layout's helpers:
//!
//! - **The discriminant.** Read by [`intrinsics::read_discriminant`], the one decoder of a
//!   tag or a niche (`discriminant_value` uses it too), as an `i64`.
//! - **No variant.** An enum of no inhabited variant is uninhabited: it has no value, and
//!   code that would hold one is unreachable. A variant that is absent from the layout
//!   (uninhabited, its fields all zero-sized) has no fields to write.
//! - **Copies.** A copy switches on the discriminant, copies that variant's fields alone,
//!   then writes its tag ([`layout::Layouts::tag_write`]; none for a niche variant whose
//!   field holds it): a copy never reads a byte that the value's variant does not define.
//! - **Construction.** A tuple variant's ctor call `V(a, b)`, a unit variant `V` and a struct
//!   variant literal `V { .. }` write the tag, then each field, in a temporary (layout keeps
//!   another variant's fields clear of a niche, so the order is free).
//! - **`match`.** The scrutinee's discriminant is read once and switched on, each variant
//!   pattern's case its discriminant masked to 64 bits; an arm's variant
//!   pattern binds its fields from the scrutinee's place, by copy. The patterns inside a
//!   variant pattern must be irrefutable (bindings, `_`, tuples); a refutable one is refused
//!   by name. Which variant a pattern or path names is the checker's (`Checked::pat_defs`,
//!   `expr_defs`), never resolved again here.
//!
//! # Wide pointers
//!
//! A reference or raw pointer to `str` or a slice is a pair: the data's address at offset 0,
//! its length (a `usize`) at 8; it is held as a 16-byte aggregate, so it is passed and
//! returned under the Rust ABI as any aggregate (by pointer). A string literal is its bytes as
//! data and that pair.
//!
//! # Adjustments
//!
//! The check records each expression's adjustments as one list: dereferences, then at most
//! one borrow, then coercions. The walk builds the dereferences and the borrow as a pointer to
//! the place they reach, never a copy: a builtin step loads the next pointer from the place, an
//! overloaded one calls [`Adts::deref_fn`]'s method at `[Self]` with a pointer to the `Self`
//! (borrowed in place, so a local dereferenced that way is not moved). With no borrow, the
//! value is loaded from that place. The coercions follow as value steps: an unsizing of
//! `&[T; N]` (or `&mut`, or a raw pointer) to `&[T]` is the pair of the address and `N`; a fn
//! item reified is its instance's address; a raw pointer's mutability and a fn pointer's
//! `unsafe` change no bits; `!` to any type is a value of the target's form in unreachable
//! code. Refused by name: an unsizing to `dyn Trait` (the checker's table has no such type and
//! no vtable is built), a `Box` or other `CoerceUnsized` unsizing (never recorded), a `&mut`
//! borrow through a dereference recorded as `Deref::deref` (the check must record
//! `DerefMut`), and a place behind an overloaded dereference to an unsized target (`*string`
//! as a place: a place carries no length). A closure to a fn pointer has no adjustment kind
//! in the checker yet.
//!
//! # Intrinsics
//!
//! A call of a fn [`Adts::intrinsic`] names ([`Intrinsic::Table`]) is built in place by
//! [`intrinsics::build`]: its generic arguments grounded, each argument a scalar (a
//! one-scalar aggregate's scalar too), an aggregate's address, or nothing for a zero-sized
//! one; its result a scalar, a `(T, bool)` pair written at the tuple's offsets, a value at an
//! address copied into a temporary, nothing, or divergence. No call site is passed:
//! `caller_location` is refused by name. An error is [`BuildError::Intrinsic`].

#![feature(allocator_api)]

pub mod cabi;
pub mod data;
pub mod imports;
pub mod instance;
pub mod intrinsics;
pub mod layout;
mod normalize;
#[cfg(any(test, feature = "testing"))]
pub mod read_back;
pub mod tls;
pub mod vtable;
mod walk;

#[cfg(test)]
mod tests;

use core::alloc::Allocator;
use core::fmt;
use std::alloc::Global;

use blaze_lir::{Func, VerifyError};
use blaze_typeck::{Adt, BodyView, Checked, Expr, ExprId, FnSig, ItemKey, LangItem, List, Mutbl, PTy, PTys, PatId, PortableTys};

pub use instance::{Instance, InstanceKey, ground, instance_key, instance_key_normalized};

/// The instance being built: its generic arguments, types of `ground`, the program's one table
/// of ground types. Every type the build lays out is first carried into `ground` with each
/// `Param(i)` replaced by `args[i]`; a non-generic fn is the instance with no arguments.
pub struct Subst<'g, 't> {
    pub args: &'g [PTy],
    pub ground: &'g mut PortableTys<'t>,
    /// The allocator of grounding's scratch columns.
    pub scratch: blaze_own::Alloc<'g>,
}

/// What the build reads from outside the body form: the structs a body's types name (their
/// fields, for layout), and the values of the body's literals, which blaze-typeck's body form
/// does not hold (its `Lit::Int` keeps the suffix, `Lit::ByteStr` the length).
///
/// The literal answers default to none, which the build refuses with
/// [`BuildError::NoLiteralValue`] naming the literal: a lowering that makes literals answers
/// them, until the body form carries the values itself.
pub trait Adts {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>>;

    /// The value of integer literal `expr` of the body of `owner`, as written (unsigned; a
    /// negative literal is a `Unary { op: Neg }` of it).
    fn int_lit(&self, owner: ItemKey, expr: ExprId) -> Option<u128> {
        let _ = (owner, expr);
        None
    }

    /// The magnitude of integer literal pattern `pat` (`Pat::Lit` or `Pat::NegLit`) of the body
    /// of `owner`; the build negates a `NegLit`.
    fn int_pat(&self, owner: ItemKey, pat: PatId) -> Option<u128> {
        let _ = (owner, pat);
        None
    }

    /// The bits of float literal pattern `pat` (`Pat::Lit` or `Pat::NegLit` of `Lit::Float`)
    /// of the body of `owner` at its type (an `f32`'s in the low 32), as [`Adts::float_lit`]
    /// gives an expression's; the magnitude: the build flips the sign of a `NegLit`.
    fn float_pat(&self, owner: ItemKey, pat: PatId) -> Option<u128> {
        let _ = (owner, pat);
        None
    }

    /// The text of string literal pattern `pat` (`Pat::Lit(Lit::Str)`) of the body of `owner`.
    fn str_pat(&self, owner: ItemKey, pat: PatId) -> Option<&str> {
        let _ = (owner, pat);
        None
    }

    /// The bits of float literal `expr` of the body of `owner` at its type (an `f32`'s in the
    /// low 32), the literal's text rounded to the nearest value of the type (the Reference,
    /// "Floating-point literal expressions").
    fn float_lit(&self, owner: ItemKey, expr: ExprId) -> Option<u128> {
        let _ = (owner, expr);
        None
    }

    /// The bytes of byte string literal `expr` of the body of `owner` (of a C string literal,
    /// its bytes without the nul).
    fn byte_str(&self, owner: ItemKey, expr: ExprId) -> Option<&[u8]> {
        let _ = (owner, expr);
        None
    }

    /// The fn item of lang item `name` (`panic_bounds_check`, `drop_in_place`), if the program
    /// declares it. The build refuses a use of an absent one with [`BuildError::NoLangFn`].
    fn lang_fn(&self, name: &str) -> Option<ItemKey> {
        let _ = name;
        None
    }

    /// The trait method `which` names (`Fn::call`, `FnMut::call_mut`, `FnOnce::call_once`): a
    /// closure's body is the instance of its kind's at `[closure, (args..)]`, and a direct call
    /// of a closure calls it (codegen.md, "Closures and the Fn traits"). With no answer the
    /// call is refused with [`BuildError::NoLangFn`] naming `call`, `call_mut` or `call_once`.
    fn fn_trait_method(&self, which: FnTrait) -> Option<ItemKey> {
        let _ = which;
        None
    }

    /// The `Deref::deref` (`Mutbl::Not`) or `DerefMut::deref_mut` (`Mutbl::Mut`) trait method,
    /// which a call at `[Self]` runs as `Self`'s impl's, as with [`Adts::drop_impl`]'s fn. An
    /// overloaded dereference with no answer is refused with [`BuildError::NoLangFn`] naming
    /// `deref` or `deref_mut`.
    fn deref_fn(&self, mutbl: Mutbl) -> Option<ItemKey> {
        let _ = mutbl;
        None
    }

    /// The `Index::index` (`Mutbl::Not`) or `IndexMut::index_mut` (`Mutbl::Mut`) trait method,
    /// which a call at `[Self, Idx]` runs as `Self`'s impl's, as with [`Adts::deref_fn`]. An
    /// overloaded index (walk/index.rs) with no answer is refused with [`BuildError::NoLangFn`]
    /// naming `index`.
    fn index_fn(&self, mutbl: Mutbl) -> Option<ItemKey> {
        let _ = mutbl;
        None
    }

    /// The ADT that is the lang item `item`. The walk asks only for `LangItem::ManuallyDrop`,
    /// whose field is never dropped (so a `ManuallyDrop<T>` needs drop only by its own impl,
    /// which core gives it none of); with no answer, a `ManuallyDrop<T>` counts as needing
    /// drop when `T` does, which drops what the program meant to keep.
    fn lang_adt(&self, item: LangItem) -> Option<ItemKey> {
        let _ = item;
        None
    }

    /// Trait `t` as a trait object sees it (docs/v2/plan/design/codegen.md, "Trait objects"):
    /// its supertraits and its methods in declaration order, whether each method has a slot,
    /// and the shape hash every table's key covers. With no answer, a vtable of `t` (or of a
    /// trait below it) is refused by name ([`vtable::VtableError::NoTrait`]).
    fn object_trait(&self, t: ItemKey) -> Option<vtable::ObjectTrait<'_>> {
        let _ = t;
        None
    }

    /// Whether ground type `ty` (of `ground`) has a `Drop` impl: if so, the `Drop` trait's
    /// `drop` fn, which a call at `[ty]` runs as the impl's. Asked once per aggregate laid out,
    /// to know whether it needs drop (the module doc's "Drops"). No default: a world that
    /// answers nothing would leak every value silently.
    fn drop_impl(&self, ty: PTy, ground: &mut PortableTys<'_>) -> Result<Option<ItemKey>, BuildError>;

    /// The ABI fn item `item` is declared with, asked of every fn built or called; no answer
    /// is refused with [`BuildError::NoAbi`].
    fn abi(&self, item: ItemKey) -> Option<Abi> {
        let _ = item;
        None
    }

    /// The text of string literal `expr` of the body of `owner` (its escapes processed).
    fn str_lit(&self, owner: ItemKey, expr: ExprId) -> Option<&str> {
        let _ = (owner, expr);
        None
    }

    /// Whether fn item `item` is a compiler intrinsic (`#[rustc_intrinsic]`, no body), and
    /// which. A call of one is built in place, never as a call. The hook the build-support lane
    /// fills; the walk builds those it names in [`Intrinsic`].
    fn intrinsic(&self, item: ItemKey) -> Option<Intrinsic> {
        let _ = item;
        None
    }

    /// What layout reads of the program's ADTs ([`layout::LayoutFacts`]): their definitions,
    /// `repr` attributes, explicit discriminants and valid ranges. Every size, alignment,
    /// offset, tag and niche the build uses is [`layout::Layouts`]' answer from these (rule 30:
    /// one layout). No default: a world that answered nothing would lay out wrongly silently.
    fn facts(&self) -> &dyn layout::LayoutFacts;

    /// Whether item `item`, named by a path, is a `static`, and which kind. A path to one is a
    /// place (module doc, "Statics"). Defaults to none: a front that refuses `static` items
    /// answers none.
    fn static_item(&self, item: ItemKey) -> Option<StaticItem> {
        let _ = item;
        None
    }

    /// The evaluated value of const item `item` at generic arguments `args` (types of the
    /// ground table), as the constant evaluator recorded it. A path to the const is built
    /// from these bytes and never from the const's body (module doc, "Consts"). No answer is
    /// refused with [`BuildError::NoConstValue`]. The checker records no generic arguments
    /// of a const path yet, so `args` is empty.
    fn const_value(&self, item: ItemKey, args: &[PTy]) -> Option<&data::DataRecord<Global>> {
        let _ = (item, args);
        None
    }
}

/// A `static` item's kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StaticItem {
    /// `#[thread_local]`: one copy per thread, reached by `tls_addr`.
    pub thread_local: bool,
    /// `static mut`.
    pub mutable: bool,
}

/// A compiler intrinsic the build makes in place (module doc, "Intrinsics").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Intrinsic {
    /// An intrinsic of [`intrinsics::TABLE`], built by [`intrinsics::build`].
    Table(intrinsics::Which),
}

/// The ABI a fn is declared with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Abi {
    /// blaze's Rust ABI (the module doc).
    Rust,
    /// `extern "C"`.
    C,
    /// `extern "rust-call"` (core's `Fn*::call*`): blaze's Rust ABI with the last parameter, a
    /// tuple, crossing the call as its fields, each a Rust parameter of its own (codegen.md,
    /// "Closures and the Fn traits"). Callers spread a written tuple the same way, so a call
    /// through the trait and a direct call of the same fn agree.
    RustCall,
}

/// The `Fn*` trait a closure is called through, weakest bound first: a closure's kind is the
/// strongest its captures need (codegen.md, "Closures and the Fn traits"). The builder's own
/// until the checker records and exports its kind (one home then: the checker's).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum FnTrait {
    /// `Fn::call(&self, args)`.
    Fn = 0,
    /// `FnMut::call_mut(&mut self, args)`.
    FnMut = 1,
    /// `FnOnce::call_once(self, args)`.
    FnOnce = 2,
}

/// A closure's identity, read from its ground type (typeck.md, "A closure's identity"): what
/// names the instances of its `Fn*` methods across bodies. Its kind, signature and environment
/// are facts of it ([`layout::LayoutFacts::closure`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClosureOf {
    /// The owner fn item whose body the closure is written in.
    pub owner: ItemKey,
    /// The owner's instance: its ground generic arguments.
    pub owner_args: List,
    /// Its position among the owner body's closures in expression order (typeck.md, "A
    /// closure's identity"): no handle local to one body.
    pub ordinal: u32,
}

/// Ground type `ty` as a closure, or `None`.
pub fn closure_of(ground: PTys<'_>, ty: PTy) -> Option<ClosureOf> {
    match ground.node(ty).ok()? {
        blaze_typeck::PNode::Closure { owner, ordinal, args } => Some(ClosureOf { owner, owner_args: args, ordinal }),
        _ => None,
    }
}

/// One built function.
pub struct Built<A: Allocator> {
    /// The function, verified.
    pub func: Func<A>,
    /// By function reference place: the callee instance, its arguments in the ground table.
    /// Two calls of one instance share one place.
    pub refs: Vec<Instance, A>,
    /// By `Static` reference place: the `static` item whose data the reference names.
    pub statics: Vec<ItemKey, A>,
    /// By `Tls` reference place: the `#[thread_local]` static the reference names.
    pub thread_locals: Vec<ItemKey, A>,
    /// By `Vtable` reference place: the (concrete type, principal) table whose address the
    /// function takes. One place per table, however often it is named.
    pub vtables: Vec<vtable::VtableRef, A>,
}

/// Why a body was not built.
#[derive(Debug)]
pub enum BuildError {
    /// An expression outside the subset.
    Unsupported { expr: ExprId, what: &'static str },
    /// A pattern outside the subset.
    UnsupportedPat { pat: PatId, what: &'static str },
    /// A type outside the subset, met at `expr`.
    UnsupportedType { expr: ExprId, what: &'static str },
    /// The owner's signature is outside the subset (`what` names the part).
    UnsupportedSig { what: &'static str },
    /// The body or its record is not one the checker makes (a bad index, a missing type).
    Malformed { what: &'static str },
    /// The check had diagnostics: a body with type errors is not built.
    Diagnostics { count: u32 },
    /// [`Adts`] gave no value for this literal expression.
    NoLiteralValue { expr: ExprId },
    /// [`Adts`] gave no value for this literal pattern.
    NoPatValue { pat: PatId },
    /// Expression `expr` (none: a drop the build inserted) needs lang item fn `name`, which
    /// [`Adts::lang_fn`] does not know.
    NoLangFn { expr: Option<ExprId>, name: &'static str },
    /// Selecting a type's `Drop` impl failed ([`Adts::drop_impl`]).
    Select(blaze_typeck::ResolveError),
    /// [`Adts::abi`] gave no ABI for fn `item`, built or called.
    NoAbi { item: ItemKey },
    /// Path `expr` names static `item`, which [`Adts::static_item`] does not describe.
    NoStatic { expr: ExprId, item: ItemKey },
    /// Path `expr` names const `item`, whose value [`Adts::const_value`] does not hold. The
    /// const's body is never built in its place.
    NoConstValue { expr: ExprId, item: ItemKey },
    /// Path pattern `pat` names const `item`, whose value [`Adts::const_value`] does not hold.
    NoPatConstValue { pat: PatId, item: ItemKey },
    /// A layout read a record that is not made yet: `item`'s (an enum's explicit discriminant).
    /// Nothing was cached; the build can be asked again once the record exists.
    MissingRecord { item: ItemKey },
    /// A type names ADT `adt`, which [`Adts::adt`] does not describe: the seam tabled no row
    /// for it. The key is kept so the caller can name the ADT by its path.
    NoAdt { adt: ItemKey },
    /// The projection `<_ as trait_>::name` met at an instance was not normalized: the
    /// solver ([`layout::LayoutFacts::normalize`]) gave no answer, or its answers did not settle
    /// within a bounded count (normalize.rs).
    Projection { trait_: ItemKey, name: blaze_typeck::Name },
    /// Intrinsic call `expr` of `which` was not built: its arguments are not of the
    /// intrinsic's signature, or (`unsupported`) a use blaze does not build yet, named by `why`.
    Intrinsic { expr: ExprId, which: intrinsics::Which, why: &'static str, unsupported: bool },
    /// An `extern "C"` signature's C ABI classification was refused (`cabi`), for fn `item`
    /// (none: a call through a fn pointer), naming why.
    CAbi { item: Option<ItemKey>, why: &'static str },
    /// A trait object's table shape was not made (at `at`, an unsizing or a dynamic call),
    /// named by [`vtable::VtableError`]: the world gave no facts for a trait.
    Vtable { at: Option<ExprId>, error: vtable::VtableError },
    /// blaze-lir's builder refused a use of it: this crate's bug, named.
    Lir(blaze_lir::BuildError),
    /// The built function failed blaze-lir's verifier: this crate's bug, named.
    Verify(VerifyError),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Unsupported { expr, what } => write!(f, "expression {}: {what} is not built to LIR yet", expr.0),
            BuildError::UnsupportedPat { pat, what } => write!(f, "pattern {}: {what} is not built to LIR yet", pat.0),
            BuildError::UnsupportedType { expr, what } => {
                write!(f, "expression {}: a value of {what} is not built to LIR yet", expr.0)
            }
            BuildError::UnsupportedSig { what } => write!(f, "the signature: {what} is not built to LIR yet"),
            BuildError::Malformed { what } => write!(f, "a malformed body or record: {what}"),
            BuildError::Diagnostics { count } => write!(f, "the body has {count} type errors"),
            BuildError::NoLiteralValue { expr } => {
                write!(f, "expression {}: the build's `Adts` gave no value for this literal", expr.0)
            }
            BuildError::NoPatValue { pat } => {
                write!(f, "pattern {}: the build's `Adts` gave no value for this literal", pat.0)
            }
            BuildError::NoLangFn { expr: Some(expr), name } => {
                write!(f, "expression {}: needs the `{name}` lang item fn, which the program does not declare", expr.0)
            }
            BuildError::NoLangFn { expr: None, name } => {
                write!(f, "a value's drop needs the `{name}` lang item fn, which the program does not declare")
            }
            BuildError::Select(error) => write!(f, "selecting a `Drop` impl: {error}"),
            BuildError::NoAbi { item } => {
                write!(f, "fn item {:#x}: the build's `Adts` gave no ABI for it", item.0)
            }
            BuildError::NoStatic { expr, item } => {
                write!(f, "expression {}: static item {:#x} is not one the build's `Adts` describes", expr.0, item.0)
            }
            BuildError::NoConstValue { expr, item } => {
                write!(f, "expression {}: const item {:#x} has no evaluated value in the build's `Adts`", expr.0, item.0)
            }
            BuildError::NoPatConstValue { pat, item } => {
                write!(f, "pattern {}: const item {:#x} has no evaluated value in the build's `Adts`", pat.0, item.0)
            }
            BuildError::MissingRecord { item } => {
                write!(f, "a layout needs the record of item {:#x} (an explicit discriminant), which is not made yet", item.0)
            }
            BuildError::NoAdt { adt } => {
                write!(f, "a type names ADT {:#x}, which the build's `Adts` does not describe", adt.0)
            }
            BuildError::Projection { trait_, name } => write!(
                f,
                "the associated type {:#x} of trait {:#x} at this instance was not normalized by the seam's `normalize`",
                name.0, trait_.0
            ),
            BuildError::Intrinsic { expr, which, why, unsupported: false } => {
                write!(f, "expression {}: intrinsic `{}`: {why}", expr.0, which.entry().name)
            }
            BuildError::Intrinsic { expr, which, why, unsupported: true } => {
                write!(f, "expression {}: intrinsic `{}`: {why} is not built to LIR yet", expr.0, which.entry().name)
            }
            BuildError::CAbi { item: Some(item), why } => write!(f, "fn item {:#x}: its `extern \"C\"` signature: {why}", item.0),
            BuildError::CAbi { item: None, why } => write!(f, "a call through an `extern \"C\"` fn pointer: {why}"),
            BuildError::Vtable { at: Some(expr), error } => write!(f, "expression {}: a trait object's vtable: {error}", expr.0),
            BuildError::Vtable { at: None, error } => write!(f, "a trait object's vtable: {error}"),
            BuildError::Lir(error) => write!(f, "blaze-lir's builder refused the function: {error}"),
            BuildError::Verify(error) => write!(f, "the LIR built does not verify: {error}"),
        }
    }
}

impl core::error::Error for BuildError {}

/// Builds the instance `subst` names of `body`, checked once generically as `checked`, of a fn
/// whose signature is `sig`, in `alloc`.
pub fn build<A: Allocator + Clone>(
    body: &BodyView<'_>,
    checked: &Checked<'_>,
    sig: &FnSig<'_>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    if !checked.diags.is_empty() {
        return Err(BuildError::Diagnostics { count: u32::try_from(checked.diags.len()).unwrap_or(u32::MAX) });
    }
    let generics = sig.parent_count.checked_add(sig.own_count).ok_or(BuildError::Malformed { what: "a generic count past u32" })?;
    if subst.args.len() != generics as usize {
        return Err(BuildError::Malformed { what: "an instance whose argument count is not its fn's generic count" });
    }
    walk::build(body, checked, sig, subst, adts, walk::Kind::Body, alloc)
}

/// Builds the instance of trait method `which` at the `ordinal`-th closure written in `body`
/// (the owner's, checked once as `checked`), at the owner's instance `subst` names: `Fn::call`,
/// `FnMut::call_mut` or `FnOnce::call_once` at `[closure, (args..)]` (codegen.md, "Closures
/// and the Fn traits"): the closure's body when `which` is its kind, a thin entry over the body
/// when weaker. The caller finds the owner, its arguments and the ordinal from the closure's
/// ground type ([`closure_of`]) and `which` from the instance's item.
pub fn build_closure<A: Allocator + Clone>(
    body: &BodyView<'_>,
    checked: &Checked<'_>,
    ordinal: u32,
    which: FnTrait,
    sig: &FnSig<'_>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    if !checked.diags.is_empty() {
        return Err(BuildError::Diagnostics { count: u32::try_from(checked.diags.len()).unwrap_or(u32::MAX) });
    }
    let closure = walk::closures_in(body).find(|c| c.0 == ordinal).map(|c| c.1).ok_or(BuildError::Malformed { what: "a closure ordinal past its owner body's closures" })?;
    let Some(Expr::Closure { params, body: root, .. }) = body.exprs.get(closure.ix()).copied() else {
        return Err(BuildError::Malformed { what: "a closure body asked of an expression that is not a closure" });
    };
    let params = body.pat_lists.get(params.range()).ok_or(BuildError::Malformed { what: "a closure's parameters past `pat_lists`" })?;
    let view = BodyView { params, root, ..*body };
    walk::build(&view, checked, sig, subst, adts, walk::Kind::Closure(closure, which), alloc)
}

/// What the instance `subst` names of `body` reads of trait shapes, before it is built: its
/// dynamic calls' slots and its upcasts' words, in expression order. With the instance's
/// [`InstanceKey`] it makes the record's key ([`vtable::record_key`]; codegen.md, "Trait
/// objects: vtables and dynamic calls"): an edit to a trait changes the key of exactly the
/// functions whose slot or word moved.
pub fn object_reads<A: Allocator + Clone>(
    body: &BodyView<'_>,
    checked: &Checked<'_>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Vec<vtable::ObjectRead, A>, BuildError> {
    if !checked.diags.is_empty() {
        return Err(BuildError::Diagnostics { count: u32::try_from(checked.diags.len()).unwrap_or(u32::MAX) });
    }
    walk::object_reads(body, checked, subst, adts, alloc)
}

/// Builds the instance `subst` names of the `drop_in_place` lang fn as the drop glue of its
/// one type argument (module doc, "Drops"): `body`, `checked` and `sig` are that fn's, whose
/// written body is not built. The caller picks this for every instance of
/// `Adts::lang_fn("drop_in_place")`.
pub fn build_drop_glue<A: Allocator + Clone>(
    body: &BodyView<'_>,
    checked: &Checked<'_>,
    sig: &FnSig<'_>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    if subst.args.len() != 1 || sig.inputs.len() != 1 {
        return Err(BuildError::Malformed { what: "a `drop_in_place` not of one type parameter and one parameter" });
    }
    walk::build(body, checked, sig, subst, adts, walk::Kind::DropGlue, alloc)
}
