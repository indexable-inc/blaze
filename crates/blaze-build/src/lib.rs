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
//! falling alignment, a `repr(C)` one's kept in order; a `repr(packed)` one's at the packed
//! alignment, and a function that makes its row claims at most that alignment in every plain
//! access after (walk.rs `layout`; a packed part that needs drop is refused by name). A
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
//! address copied into a temporary, nothing, or divergence. `caller_location` is its site:
//! the location a `#[track_caller]` fn was passed, else a `Location` of the call's own place
//! (walk/track_caller.rs, "`#[track_caller]`" there). An error is [`BuildError::Intrinsic`].
//!
//! # `#[track_caller]`
//!
//! A fn [`Adts::track_caller`] says is tracked takes one hidden last parameter, a pointer to
//! its caller's `Location`: a call from an untracked fn passes a data row of the call's own
//! place ([`Adts::caller_site`]), a call from a tracked fn forwards what it was given (the
//! Reference, "The `track_caller` attribute", [attributes.codegen.track_caller]). A `Fn*` shim
//! of a tracked fn item passes a row of the `Fn*` method's definition site ([`Adts::def_site`],
//! as rustc's call shim does). A tracked fn taken as a fn pointer points to its `ReifyShim`
//! ([`build_reify_shim`], named by [`REIFY_SHIM_ITEM`]), which has the plain fn-pointer ABI,
//! materializes a `Location` row for the fn's own definition site ([`Adts::def_site`]), and calls
//! the tracked fn with that location passed last.

#![feature(allocator_api)]

pub mod abi;
pub mod cabi;
pub mod data;
pub mod drop_facts;
mod dyn_tail;
pub mod imports;
pub mod instance;
pub mod intrinsics;
pub mod layout;
pub mod coroutine_niche;
pub mod coroutine_state;
mod normalize;
#[cfg(any(test, feature = "testing"))]
pub mod read_back;
pub mod start;
pub mod tls;
pub mod type_name;
pub mod vtable;
mod walk;

#[cfg(test)]
mod tests;

use core::alloc::Allocator;
use core::fmt;
use std::alloc::Global;

use blaze_lir::VerifyError;
use blaze_typeck::{Adt, BodyView, Checked, Expr, ExprId, FmtTrait, FnSig, ItemKey, LangItem, List, Mutbl, PTy, PTys, PatId, PortableTys};

pub use instance::{
    Instance, InstanceKey, REIFY_SHIM_ITEM, closure_method_key_args, copy_written, ground, instance_key, instance_key_normalized, reify_shim_instance, reify_shim_key_args, reify_shim_of,
};
pub use walk::arith_checks::PanicConst;
pub use walk::coroutine::{CoroutineFacts, CoroutineOf, StateShape, coroutine_facts, coroutine_of, coroutines_in, identity as coroutine_identity};
pub use coroutine_niche::{NicheRefused, StateNiche, niche_of, row_niche};

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

    /// The fn whose head made the opaque type `opaque` (an `impl Trait` in its return type, an
    /// `async fn`'s future; for a type alias's, the alias) and the opaque's index among its
    /// owner's: what names the question of its hidden type in a code entry's read list, so a
    /// verifier makes that head, by name, before it asks (blaze-ladder code_reads.rs
    /// `ReadKind::OpaqueHidden`). Defaults to none: a world with no opaques.
    fn opaque_of(&self, opaque: ItemKey) -> Option<(ItemKey, u32)> {
        let _ = opaque;
        None
    }

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

    /// The bytes of string or byte string literal pattern `pat` (`Pat::Lit(Lit::Str)`, `Pat::Lit(
    /// Lit::ByteStr(_))`) of the body of `owner`, unescaped.
    fn str_pat(&self, owner: ItemKey, pat: PatId) -> Option<&[u8]> {
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

    /// The names a type is printed with (type_name.rs): the paths of items, for the messages
    /// of the validity assertions (walk/validity.rs). `None`, the default: such a message is
    /// refused by name ([`BuildError::Intrinsic`]), never printed without its paths.
    fn type_names(&self) -> Option<&dyn type_name::TypeNames> {
        None
    }

    /// What a panic does in this build (part of every code record's key): with
    /// [`Panic::Unwind`] a call that may unwind past live values gets a landing block that drops
    /// them (walk/unwind.rs). The default builds none, as before unwinding was built.
    fn panic(&self) -> Panic {
        Panic::Abort
    }

    /// Whether the body of fn item `owner` may not unwind (walk/nounwind.rs): its own ABI is of
    /// the Reference's "non-unwinding" category ("C", "system", ...: not "Rust", not one ending
    /// in "-unwind"), or it is `#[rustc_nounwind]`. `None`: not said; then a Rust body unwinds
    /// and an `extern` body built under [`Panic::Unwind`] is refused by name, because
    /// [`Abi::C`] stands for both "C" and "C-unwind".
    fn nounwind(&self, owner: ItemKey) -> Option<bool> {
        let _ = owner;
        None
    }

    /// Code-generation function attributes (`#[inline]`, `#[inline(always)]`, `#[inline(never)]`, `#[cold]`).
    fn fn_attrs(&self, owner: ItemKey) -> blaze_lir::func::FnAttrs {
        let _ = owner;
        blaze_lir::func::FnAttrs::default()
    }

    /// Whether `+ - *`, shifts and unary `-` on integers panic on overflow (the Reference,
    /// [expr.operator.int-overflow]: "Integer operators will panic when they overflow when
    /// compiled in debug mode. The -C debug-assertions and -C overflow-checks compiler flags
    /// can be used to control this more directly"); off, they wrap (walk/arith_checks.rs).
    /// Division and remainder are checked in every build either way. Part of every code
    /// record's key, as [`Adts::panic`] is. No default (decision D101): every builder context
    /// says it, from its caller, which reads the profile (Cargo's `overflow-checks`: on in
    /// `dev` and `test`, off in `release`, the manifest's `[profile.<name>]` overriding).
    fn overflow_checks(&self) -> OverflowChecks;

    /// What core's `intrinsics::ub_checks()` is at run time in the code built here ([`UbChecks`]):
    /// decided where the instance is built, as the pinned core's doc says ("evaluation is
    /// delayed until monomorphization"), from the caller's profile. Part of every code record's
    /// key (code_records.rs `Settings::debug_assertions`). No default, as `overflow_checks`.
    fn ub_checks(&self) -> UbChecks;

    /// The trait method `which` names (`Fn::call`, `FnMut::call_mut`, `FnOnce::call_once`): a
    /// closure's body is the instance of its kind's at `[closure, (args..)]`, and a direct call
    /// of a closure calls it (codegen.md, "Closures and the Fn traits"). With no answer the
    /// call is refused with [`BuildError::NoLangFn`] naming `call`, `call_mut` or `call_once`.
    fn fn_trait_method(&self, which: FnTrait) -> Option<ItemKey> {
        let _ = which;
        None
    }

    /// The trait method `Future::poll` (core's future/future.rs: `fn poll(self: Pin<&mut
    /// Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;`): an `.await` calls its instance at
    /// `[the held future's type]`, and a coroutine's split `poll` function is that instance at
    /// `[the coroutine]` (coroutine.rs; [`coroutine_of`] reads the identity from the type). With
    /// no answer an `.await` is refused with [`BuildError::NoLangFn`] naming `poll`.
    fn future_poll(&self) -> Option<ItemKey> {
        None
    }

    /// The library items an `.await` and a coroutine's entries are built from ([`AsyncItems`]:
    /// the `IntoFuture` trait, the `Poll` enum with its two variants, the `Pin` struct, and
    /// the item a coroutine's resume fn is named by). With no answer an async block's body
    /// and an `.await` are refused with [`BuildError::NoLangFn`] naming this method.
    fn async_items(&self) -> Option<AsyncItems> {
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

    /// The ABI a call of fn item `item` is made with, asked of every fn built or called; no
    /// answer is refused with [`BuildError::NoAbi`]. For a fn with a body, its own. For a
    /// declaration the front end binds to another fn's definition (alloc's `extern "Rust"`
    /// `__rust_alloc` .. run std's `extern "C"` `__rdl_alloc` ..), the DEFINITION's: the walk
    /// lowers the call from this answer and the caller's argument types, and the instance the
    /// call resolves to is built from its own, so the two must agree (an `Alignment`, a Rust
    /// aggregate, crosses a "Rust" call by address and a "C" call as its `usize`).
    fn abi(&self, item: ItemKey) -> Option<Abi> {
        let _ = item;
        None
    }

    /// For a C-variadic foreign fn `item` (`fn printf(format: *const c_char, ...)`), its
    /// declared parameter count: a call's arguments past it are the `...` ones
    /// (walk/c_variadic.rs). `None`: not variadic. The checker refuses more arguments than
    /// declared to any other fn (E0061), so a world that does not know `...` (the default)
    /// never sees such a call checked.
    fn c_variadic(&self, item: ItemKey) -> Option<u32> {
        let _ = item;
        None
    }

    /// For fn item `item` with `#[rustc_legacy_const_generics(i, ..)]` (stdarch's
    /// `vget_lane_u64(v, 0)`), the positions of a call's arguments that are its own const
    /// generic arguments: the checker made them the callee's generic arguments (blaze-typeck
    /// rules/legacy_const.rs), so the call passes only the arguments at the other positions.
    /// Empty for any other fn; a world that does not know the attribute (the default) never
    /// sees such a call checked, as the checker refuses its count (E0061).
    fn legacy_const_generics(&self, item: ItemKey) -> &[u32] {
        let _ = item;
        &[]
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
    /// refused with [`BuildError::NoConstValue`]. `args` are the path's recorded arguments
    /// (blaze-typeck `Checked::const_args_of`) grounded at the instance: `[Self, trait args..]`
    /// for a trait's constant, the impl's for an inherent impl's, empty for a free `const`.
    fn const_value(&self, item: ItemKey, args: &[PTy]) -> Option<&data::DataRecord<Global>> {
        let _ = (item, args);
        None
    }

    /// The evaluated value of `const { .. }` block `block` of `owner`'s body at the instance's
    /// ground arguments `args` (the owner's generics, in order), as the constant evaluator
    /// recorded it: the same record form as [`Adts::const_value`], read by the same code. The
    /// Reference, "Block expressions", "Const blocks": a const block is evaluated at compile
    /// time with the enclosing generic parameters in scope, so each instance has its own value.
    /// No answer is refused with [`BuildError::NoConstBlockValue`]; the block's code is never
    /// built in its place.
    fn const_block_value(&self, owner: ItemKey, block: ExprId, args: &[PTy]) -> Option<&data::DataRecord<Global>> {
        let _ = (owner, block, args);
        None
    }

    /// The evaluated value of promoted borrow `borrow` (`&expr` listed in `Checked::promoted`) of
    /// `owner`'s body at the instance's ground arguments `args`: the constant evaluator's record
    /// of `expr`, read like a const block's (walk/promote.rs). The Reference, "Constant
    /// promotion": the expression "can be evaluated at compile-time". No answer is refused with
    /// [`BuildError::NoPromotedValue`]; the operand is never built as a frame temporary.
    fn promoted_value(&self, owner: ItemKey, borrow: ExprId, args: &[PTy]) -> Option<&data::DataRecord<Global>> {
        let _ = (owner, borrow, args);
        None
    }

    /// The fn instance that key `key` names in an evaluated constant's record (a
    /// `DataTarget::Fn(key)` of a [`Adts::const_value`] or [`Adts::const_block_value`] answer:
    /// `const F: fn() = f::<u8>;`), its arguments interned in `ground`. The fn is named in the
    /// constant's body, not the function being built, so only the evaluator's side knows which
    /// instance it keyed; the walk files the answer as a `Func` reference, as a direct fn-item
    /// to fn-pointer coercion does (walk/promote.rs `data_rows`, which also asks it for a
    /// promoted record's fn target its operand does not name). No answer is refused with
    /// [`BuildError::NoConstFnInstance`]. An evaluator's record names the instance of each of
    /// its fn words (blaze-consteval `ConstRecord::fns`): a world answers from that, never by
    /// searching its fns for one whose key matches.
    fn const_fn_instance(&self, key: InstanceKey, ground: &mut PortableTys<'_>) -> Option<Instance> {
        let _ = (key, ground);
        None
    }

    /// The template of `format_args!` expression `expr` of the body of `owner`: its literal
    /// texts (escapes and `{{`/`}}` processed) and placeholders in order, one placeholder per
    /// row of the expression's `fmt_uses` that is not a count (walk/fmt.rs). The checker's
    /// body form keeps the uses but not the template. No answer refuses the expression by name.
    fn format_template(&self, owner: ItemKey, expr: ExprId) -> Option<&[FmtPiece<'_>]> {
        let _ = (owner, expr);
        None
    }

    /// The pinned library's `core::fmt::rt::Argument` constructor for `trait_`
    /// (`Argument::new_display` for `Display`, `new_debug` for `Debug`, ...; walk/fmt.rs quotes
    /// them), a fn generic over the argument's type `T` alone. No answer is refused with
    /// [`BuildError::NoLangFn`] naming the constructor.
    fn fmt_argument_new(&self, trait_: FmtTrait) -> Option<ItemKey> {
        let _ = trait_;
        None
    }

    /// The struct `core::fmt::rt::Argument` (lang `format_argument`). No answer is refused
    /// with [`BuildError::NoLangFn`] naming `format_argument`.
    fn fmt_argument_adt(&self) -> Option<ItemKey> {
        None
    }

    /// Whether the fn `instance` resolves to (its arguments in `ground`; a trait method's call
    /// resolved to the impl's fn, as the program builds it) is `#[track_caller]`. The
    /// Reference, "The `track_caller` attribute": "When applied to a function in a trait
    /// declaration, the attribute applies to all implementations. If the trait provides a
    /// default implementation with the attribute, then the attribute also applies to override
    /// implementations." Such a fn takes one hidden last parameter, its caller's
    /// `&'static Location<'static>` (walk/track_caller.rs), so the builder of the fn and every
    /// caller ask this one question and agree. `None`: not said; then no location is passed,
    /// and a `caller_location` or a forwarded location that depends on the answer is refused
    /// by name ([`BuildError::TrackCaller`]), never given a wrong place.
    fn track_caller(&self, instance: Instance, ground: &mut PortableTys<'_>) -> Option<bool> {
        let _ = (instance, ground);
        None
    }

    /// Where expression `expr` of the body of `owner` is, in the form a code record may hold
    /// ([`CallerSite`]: the file's name as the program observes it, the item, and the
    /// expression's byte offset from the item's start), never a line and column: those are
    /// absolute, so a line inserted above the item would change the record of every tracked
    /// call below it (rule 71). The expression is the one rustc's `Location` reports (a call's,
    /// an operator's, an index's, a `caller_location` call's own), and for one inside a macro's
    /// expansion its outermost call's. Asked only when a `Location` is made for it
    /// (walk/track_caller.rs). No answer is refused with [`BuildError::TrackCaller`] carrying
    /// the front's [`SiteGap`].
    fn caller_site(&self, owner: ItemKey, expr: ExprId) -> Result<CallerSite<'_>, SiteGap> {
        let _ = (owner, expr);
        Err(SiteGap::Unanswered)
    }

    /// Where fn `item` is DEFINED, in [`Adts::caller_site`]'s item-relative form (rule 71):
    /// rustc's `def_span` of a fn (rustc_middle hir/map.rs `hir_span`: its `sig.span`, which
    /// rustc_parse function.rs `parse_fn` starts at `sig_lo`, the first token after the outer
    /// attributes and doc comments, its visibility included), for a fn inside a macro's
    /// expansion its outermost call's. What a call shim passes as the `Location` of a
    /// `#[track_caller]` fn it calls: a `Fn*` shim of a fn item passes the site of the `Fn*`
    /// method it is (rustc_mir_transform shim.rs `build_call_shim`: the call's span is
    /// `tcx.def_span` of the shim's def, `FnOnce::call_once`, `FnMut::call_mut` or `Fn::call`;
    /// the shim itself is never tracked, rustc_middle instance.rs `requires_caller_location`).
    /// No answer is refused with [`BuildError::TrackCaller`] carrying the front's [`SiteGap`].
    fn def_site(&self, item: ItemKey) -> Result<CallerSite<'_>, SiteGap> {
        let _ = item;
        Err(SiteGap::Unanswered)
    }

    /// Expression `expr` of `owner`'s body as a trap row keeps it: its first byte counted from
    /// `owner`'s first byte (rule 71; for one inside a macro's expansion its outermost call's),
    /// with no file. What a trap's site is (walk.rs `mark_trap`): a trap needs no `file!()`, so
    /// an item whose file has no shown path (a toolchain crate's, an expansion's) still has
    /// one; the report places it by the item (blaze-cli run_fault.rs). By default the offset of
    /// [`Adts::caller_site`]'s answer.
    fn expr_site(&self, owner: ItemKey, expr: ExprId) -> Option<u32> {
        self.caller_site(owner, expr).ok().map(|site| site.offset)
    }

    /// The struct `core::panic::Location` (lang `panic_location`), the hidden parameter's
    /// pointee. No answer is refused with [`BuildError::NoLangFn`] naming `panic_location`.
    fn location_adt(&self) -> Option<ItemKey> {
        None
    }

    /// The bug this build plants (test builds of this crate only): a test's control, proving
    /// the test fails when the rule it tests is broken (tests/guards.rs, tests/operators.rs).
    #[cfg(test)]
    fn planted(&self) -> Planted {
        Planted::Nothing
    }
}

/// A bug planted in the walk by a test's control ([`Adts::planted`]).
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Planted {
    Nothing,
    /// A false guard goes to the next ARM, never to the next `|` alternative of its own arm
    /// (the wrong answer of rung x0-match-02-guards, 2026-11-16).
    GuardToNextArm,
    /// A comparison of two references compares the two ADDRESSES, not the referents (the
    /// wrong answer of rung x0-match-05-nested-enums, 2026-11-16).
    CompareAddresses,
    /// `MIN % -1` is not checked: it gives the machine's 0 where Rust panics (the wrong answer
    /// WRONG-NOTES.md S2 found, 2026-11-16).
    NoRemOverflowCheck,
    /// A box whose contents moved out is dropped by its whole glue (the moved contents dropped
    /// again: tests/box_moves.rs's control).
    MovedBoxDroppedWhole,
    /// A box whose contents moved out is never freed (a leak: tests/box_moves.rs's control).
    MovedBoxNotFreed,
}

/// Where an expression is, item-relative ([`Adts::caller_site`]; rule 71). A `Location`'s line
/// and column are made from it only when the program is placed: from the item's present start
/// in its file and that file's line starts, so the code record that names it is the same
/// wherever the item moves in its file.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CallerSite<'a> {
    /// The file's path as the program observes it (`file!()`).
    pub file: &'a str,
    /// The item whose text holds the expression (the body's owner).
    pub item: ItemKey,
    /// The expression's first byte, counted from the item's first byte.
    pub offset: u32,
}

/// Why a front end gave no line and column for a place (blaze-ladder `FrontEnd::source_position`),
/// or no place of an item in its file ([`SiteGap::Placement`]). One home (rule 30): the seam
/// re-exports it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PositionGap {
    /// The front end does not answer the question (the old front).
    Unanswered,
    /// The item is not one the front end places: not tabled, or no first byte today.
    NoItem,
    /// The item's text is not in a file of its crate (a macro's expansion), or the file is not
    /// one the front end holds.
    NoFile,
    /// The site is past the end of the file's text, or not on a char's first byte.
    PastText,
}

impl fmt::Display for PositionGap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PositionGap::Unanswered => "this front end does not place source positions (`FrontEnd::source_position`)",
            PositionGap::NoItem => "the front end places no such item today",
            PositionGap::NoFile => "the item's text is in no file the front end holds (a macro's expansion, or an unknown file)",
            PositionGap::PastText => "the site is past its file's text, or inside a char",
        })
    }
}

/// Why [`Adts::caller_site`] or [`Adts::def_site`] gave no place: each step of the front end's
/// rule by name (rule 19; blaze-ladder lazy seam_rules.rs `caller_site`, `def_site`,
/// `body_anchor`), so a refused `#[track_caller]` location says which fact was missing. Before
/// 2026-11-21 every step was a bare `None` ("the world does not say where the expression is").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SiteGap {
    /// The front end places nothing (a test's world, the trait's default).
    Unanswered,
    /// `owner`'s body is not live on this front end: asked outside its build.
    NotLive { owner: ItemKey },
    /// The lowered body of `owner` kept no range for `expr`.
    NoRange { owner: ItemKey, expr: ExprId },
    /// `item`'s own head (its text) is not made on this front end.
    NoRow { item: ItemKey },
    /// Body-local `local`'s enclosing item `enclosing` has no head on this front end: the make did
    /// not ask for it (blaze-ladder make_one.rs asks the chain up front, `FrontEnd::site_heads`).
    NoEnclosingRow { local: ItemKey, enclosing: ItemKey },
    /// Body-local `local`'s range is not inside its enclosing item's text (or past `u32`).
    LocalOutsideEnclosing { local: ItemKey },
    /// The chain of body-locals from `owner` is longer than the locals kept (rule 18).
    TooDeep { owner: ItemKey, kept: usize },
    /// Member `owner` has no row in its container's members.
    NoMemberRow { owner: ItemKey },
    /// `owner` names no library item: for a body-local, its enclosing body was never located on
    /// this front end (no local head is filed for it).
    NoLibrary { owner: ItemKey },
    /// Item `item` has neither a text of its own nor a member row to count from.
    NoAnchor { item: ItemKey },
    /// Item `item`'s place in its file was not read: the names source answered `gap`.
    Placement { item: ItemKey, gap: PositionGap },
    /// A written body-local's declaration at byte `declaration` of `item`'s text keys no source local.
    LocalKey { item: ItemKey, declaration: u32 },
    /// `item`'s key clashes with another in the front end's key map.
    KeyClash { item: ItemKey },
    /// File `file` of crate `krate`, where `item` is written, has no shown path (`file!()`):
    /// over records, a toolchain or dependency crate's file row may keep none (the names source).
    NoShownFile { item: ItemKey, krate: usize, file: u32 },
    /// The place `offset` of `item` is before its declaration's first token at `first`.
    BeforeDeclaration { item: ItemKey, offset: u32, first: u32 },
    /// No token of `item`'s declaration follows its outer attributes, or its text does not lex.
    PastAttributes { item: ItemKey },
    /// The text of `item` is not in the names source.
    NoText { item: ItemKey },
    /// Crate `krate` of `item` has no edition in the names source.
    NoEdition { item: ItemKey, krate: usize },
}

impl fmt::Display for SiteGap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SiteGap::Unanswered => f.write_str("this front end places no expressions"),
            SiteGap::NotLive { owner } => write!(f, "the body of {:#x} is not live on the front end (asked outside its build)", owner.0),
            SiteGap::NoRange { owner, expr } => write!(f, "expression {} of {:#x} kept no range in its lowering", expr.0, owner.0),
            SiteGap::NoRow { item } => write!(f, "the head of {:#x} is not made on this front end", item.0),
            SiteGap::NoEnclosingRow { local, enclosing } => {
                write!(f, "the body-local {:#x}'s enclosing item {:#x} has no head on this front end (asked up front: `FrontEnd::site_heads`)", local.0, enclosing.0)
            }
            SiteGap::LocalOutsideEnclosing { local } => write!(f, "the body-local {:#x}'s range is not inside its enclosing item's text", local.0),
            SiteGap::TooDeep { owner, kept } => write!(f, "the body-locals enclosing {:#x} are nested deeper than the {kept} kept (rule 18)", owner.0),
            SiteGap::NoMemberRow { owner } => write!(f, "the member {:#x} has no row among its container's members", owner.0),
            SiteGap::NoLibrary { owner } => write!(f, "{:#x} names no library item (a body-local whose enclosing body was not located here)", owner.0),
            SiteGap::NoAnchor { item } => write!(f, "{:#x} has neither a text of its own nor a member row to count from", item.0),
            SiteGap::Placement { item, gap } => write!(f, "the place of {:#x} in its file: {gap}", item.0),
            SiteGap::LocalKey { item, declaration } => write!(f, "the body-local declared at byte {declaration} of {:#x} keys no source local", item.0),
            SiteGap::KeyClash { item } => write!(f, "the key of {:#x} clashes in the front end's key map", item.0),
            SiteGap::NoShownFile { item, krate, file } => {
                write!(f, "file {file} of crate {krate}, where {:#x} is written, has no shown path (`file!()`) in the names source (a dependency or toolchain crate's file row over records)", item.0)
            }
            SiteGap::BeforeDeclaration { item, offset, first } => write!(f, "byte {offset} of {:#x} is before its declaration's first token at {first}", item.0),
            SiteGap::PastAttributes { item } => write!(f, "no token of {:#x}'s declaration follows its outer attributes", item.0),
            SiteGap::NoText { item } => write!(f, "the text of {:#x} is not in the names source", item.0),
            SiteGap::NoEdition { item, krate } => write!(f, "crate {krate} of {:#x} has no edition in the names source", item.0),
        }
    }
}

/// One piece of a `format_args!` template ([`Adts::format_template`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FmtPiece<'a> {
    /// Literal text, written as is.
    Text(&'a str),
    /// A placeholder formatting the next of the expression's non-count uses. `spec`: its format
    /// spec, the text after the `:` (empty with none; `#018x` for `{:#018x}`), its string
    /// escapes processed as the template's texts are; walk/fmt.rs encodes its options.
    Arg { spec: &'a str },
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
    /// A target intrinsic (a foreign fn of an `"unadjusted"` block whose `link_name` is
    /// `llvm.*`, THREADS.md D1), built by [`intrinsics::target::build`].
    Target(intrinsics::target::Target),
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
    /// The function with its proof (D163): verified once where it was built, and handed to the
    /// emitter as `func.verified()`, never checked again.
    pub func: blaze_lir::VerifiedFunc<A>,
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
    /// Per `unreachable` the walk wrote to end a construct, which construct (blaze-code
    /// record.rs `TrapSource`: by its ordinal among the function's `unreachable`s, its kind and
    /// its item-relative site): the code record's trap table, what a fault's report says.
    pub traps: Vec<blaze_code::record::TrapSource, A>,
    /// How many of `func`'s instructions the walk's aggregate copies wrote (walk.rs `copy`:
    /// leaf by leaf, a switch per enum): the make's size report names it (rule 16). A
    /// coroutine's resume and drop carry the count of the walk they were split from.
    pub copy_insts: u64,
}

/// What a panic does. The Reference, "Panic": "unwind — unwinds the stack and is potentially
/// recoverable"; "if the panic handler is set to abort, panicking will always terminate the
/// process without destructors being run" (walk/unwind.rs quotes the rest).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Panic {
    /// The stack unwinds, running destructors: every call that may unwind past live values
    /// gets a cleanup.
    Unwind,
    /// The process aborts: no landing block is built.
    Abort,
}

/// Whether integer `+ - *`, shifts and unary `-` panic on overflow ([`Adts::overflow_checks`]).
/// The Reference, [expr.operator.int-overflow]: "Integer operators will panic when they overflow
/// when compiled in debug mode. The -C debug-assertions and -C overflow-checks compiler flags can
/// be used to control this more directly." A profile's setting (decision D101): part of every
/// code record's key, so two profiles never share code.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum OverflowChecks {
    /// Overflow calls core's `panic_const_*_overflow` (walk/arith_checks.rs): the `dev` and
    /// `test` profiles.
    On,
    /// Overflow wraps (two's complement): the `release` profile.
    Off,
}

impl OverflowChecks {
    /// Whether overflow is checked.
    pub const fn on(self) -> bool {
        matches!(self, OverflowChecks::On)
    }
}

/// What core's `intrinsics::ub_checks()` answers at run time ([`Adts::ub_checks`]). The pinned
/// core: "Returns whether we should perform some UB-checking at runtime. This eventually
/// evaluates to `cfg!(ub_checks)`, but behaves different from `cfg!` when mixing crates built
/// with different flags": the answer is the building profile's, never the library crate's
/// own cfg. rustc's `ub_checks` follows `-C debug-assertions`. In a constant it is `true`
/// whatever this says (blaze-consteval machine.rs `intrinsic`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum UbChecks {
    /// `ub_checks()` is `true`: `assert_unsafe_precondition!`'s checks run.
    On,
    /// `ub_checks()` is `false`: the checks are branches never taken.
    Off,
}

impl UbChecks {
    /// The setting of a profile whose `debug-assertions` is `debug_assertions`.
    pub const fn of_debug_assertions(debug_assertions: bool) -> UbChecks {
        if debug_assertions { UbChecks::On } else { UbChecks::Off }
    }

    /// Whether the checks run.
    pub const fn on(self) -> bool {
        matches!(self, UbChecks::On)
    }
}

/// Why a body was not built.
#[derive(Debug)]
pub enum BuildError {
    /// A call that may unwind whose cleanup cannot be built exactly (walk/unwind.rs names what
    /// is missing): refused rather than miss or double a destructor.
    NoCleanup { what: &'static str },
    /// An expression outside the subset.
    Unsupported { expr: ExprId, what: &'static str },
    /// Operator `op` at `expr` on operands of types `lhs` and `rhs` (their head nodes at this
    /// instance; a unary operator has no `rhs`) that the build cannot give Rust's meaning:
    /// `why` (walk/operators.rs: the check's callee row decides builtin or call, never how a
    /// value is held).
    Operator { expr: ExprId, op: &'static str, lhs: Option<blaze_typeck::PNode>, rhs: Option<blaze_typeck::PNode>, why: &'static str },
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
    /// A question to the program's impls failed: selecting an ADT's `Drop` impl
    /// ([`Adts::drop_impl`]), normalizing a projection at ground arguments (the ladder's
    /// `normalized`, `<u64 as Int>::Unsigned`), resolving a coroutine's callee. Shown as the
    /// solver's own error, never as a `Drop` question it was not (tempo t1530: 13 tests showed
    /// "selecting a `Drop` impl" for `ImplsOf(<a trait>, u64)` in arbitrary's
    /// `int_in_range_impl<T: Int>`; a primitive's drop asks no solver, the ladder's `drop_impl`).
    Select(blaze_typeck::ResolveError),
    /// [`Adts::abi`] gave no ABI for fn `item`, built or called.
    NoAbi { item: ItemKey },
    /// Path `expr` names static `item`, which [`Adts::static_item`] does not describe.
    NoStatic { expr: ExprId, item: ItemKey },
    /// Path `expr` names const `item`, whose value [`Adts::const_value`] does not hold. The
    /// const's body is never built in its place.
    NoConstValue { expr: ExprId, item: ItemKey },
    /// `const { .. }` block `expr` of `owner`'s body, whose value at the instance
    /// [`Adts::const_block_value`] does not hold. The block's code is never built in its place.
    NoConstBlockValue { expr: ExprId, owner: ItemKey },
    /// Promoted borrow `expr` of `owner`'s body, whose value at the instance
    /// [`Adts::promoted_value`] does not hold. Never built as a frame temporary.
    NoPromotedValue { expr: ExprId, owner: ItemKey },
    /// The value of constant path, const block or promoted borrow `expr` holds a fn pointer to
    /// instance `key`, which [`Adts::const_fn_instance`] does not give.
    NoConstFnInstance { expr: ExprId, key: InstanceKey },
    /// Path pattern `pat` names const `item`, whose value [`Adts::const_value`] does not hold.
    NoPatConstValue { pat: PatId, item: ItemKey },
    /// A layout read a record that is not made yet: `item`'s (an enum's explicit discriminant).
    /// Nothing was cached; the build can be asked again once the record exists.
    MissingRecord { item: ItemKey },
    /// A layout needs the hidden type of `impl Trait` type `opaque` (a type alias's: std's
    /// `backtrace::LazyResolve`, which `Backtrace` holds), read from its defining fn `owner`'s
    /// check record, and `owner` is not checked in this program. Nothing was cached: a program
    /// that checks fns on demand checks `owner` and builds again (blaze-ladder run/make_one.rs);
    /// any other refuses with it.
    OpaqueOwnerUnchecked { opaque: ItemKey, owner: ItemKey },
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
    /// A call of target intrinsic `target` (at `expr` when the walk knows it) was refused,
    /// named by `why` (intrinsics/target.rs).
    Target { target: intrinsics::target::Target, expr: Option<ExprId>, why: &'static str },
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
    /// The coroutine pass refused an `async` block's built body ([`build_coroutine`]), naming
    /// the operation and its rule (unwinding in the body, a compound value live across a
    /// suspend, a cancel path that suspends...).
    Coroutine(blaze_lir::coroutine::CoroutineError),
    /// An unsizing coercion at `expr` the build refuses by the rule `rule` (walk/unsize_nested.rs):
    /// through ADT `adt` (none: at the pointer itself), `why`: a struct where no field or more
    /// than one field holding bytes differs, a union, an enum, a nesting past `UNSIZE_DEPTH`.
    Unsize { expr: ExprId, adt: Option<ItemKey>, why: &'static str, rule: &'static str },
    /// A `#[track_caller]` location (walk/track_caller.rs) at `expr` (none: a call the build
    /// inserted with no expression of its own) for fn `item` was not made, named by `why`: the
    /// world does not say where `expr` is or whether a fn is tracked, or a tracked fn is taken
    /// where its shim would be needed (a fn pointer, a vtable slot).
    /// `gap`: the front end's step that gave no place, when a site was asked ([`SiteGap`]).
    TrackCaller { expr: Option<ExprId>, item: ItemKey, why: &'static str, gap: Option<SiteGap> },
    /// The layout row of declared field `field` of a value of type `ty` (its head node) is
    /// named `found`, not the declared field's name (walk.rs `decl_field`): the rows are not in
    /// declaration order, so no offset is read by index. This crate's bug, named.
    FieldOrder { ty: blaze_typeck::PNode, field: u32, found: blaze_typeck::Name },
}

/// What an `.await` and a coroutine's entries need of the library: the answer of
/// [`Adts::async_items`]. The Reference, "Await expressions", names each: `IntoFuture::
/// into_future`, `Pin::new_unchecked`, `Future::poll`, `Poll::Pending` and `Poll::Ready`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AsyncItems {
    /// The trait `IntoFuture` (core's future/into_future.rs): an `.await` of `T` holds
    /// `<T as IntoFuture>::IntoFuture` while suspended, asked of `LayoutFacts::normalize`
    /// under this trait and the name `IntoFuture`.
    pub into_future: ItemKey,
    /// The enum `Poll` (core's task/poll.rs: `pub enum Poll<T> { Ready(T), Pending }`).
    pub poll: ItemKey,
    /// The index of its variant `Ready`, by declaration.
    pub ready: u32,
    /// The index of its variant `Pending`, by declaration.
    pub pending: u32,
    /// The struct `Pin` (core's pin.rs: `pub struct Pin<Ptr> { pointer: Ptr }`).
    pub pin: ItemKey,
    /// The item a coroutine's RESUME fn is named by: the instance `(resume, [coroutine])` is
    /// the split body, taking the state pointer itself; `Future::poll` at `[coroutine]` is
    /// the entry that reads that pointer out of its `Pin` and calls it. An item no program
    /// calls by this argument list otherwise (the library's `Coroutine::resume` where it has
    /// one).
    pub resume: ItemKey,
}

/// One built coroutine instance ([`build_coroutine`]): three functions and the state's shape.
pub struct BuiltCoroutine<A: Allocator> {
    /// `Future::poll` at `[coroutine]` (the instance `(Adts::future_poll, [coroutine])`): the
    /// entry, which calls [`BuiltCoroutine::resume`].
    pub poll: Built<A>,
    /// The instance `(AsyncItems::resume, [coroutine])`: the body, as the pass's `poll`.
    pub resume: Built<A>,
    /// `drop_in_place` at `[coroutine]`: the pass's `drop`, the state's whole drop glue.
    pub drop: Built<A>,
    /// The state as the pass laid it out: the record `LayoutFacts::coroutine_state` answers
    /// from for this instance.
    pub state: StateShape,
    /// The tag's valid range in that state ([`niche_of`] the pass's layout): the record
    /// `LayoutFacts::coroutine_niche` answers from, kept beside `state`.
    pub niche: StateNiche,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Unsupported { expr, what } => write!(f, "expression {}: {what} is not built to LIR yet", expr.0),
            BuildError::Operator { expr, op, lhs, rhs: Some(rhs), why } => match lhs {
                Some(lhs) => write!(f, "expression {}: operator `{op}` on {lhs:?} and {rhs:?}: {why}", expr.0),
                None => write!(f, "expression {}: operator `{op}` on an unread type and {rhs:?}: {why}", expr.0),
            },
            BuildError::Operator { expr, op, lhs, rhs: None, why } => match lhs {
                Some(lhs) => write!(f, "expression {}: operator `{op}` on {lhs:?}: {why}", expr.0),
                None => write!(f, "expression {}: operator `{op}`: {why}", expr.0),
            },
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
            BuildError::Select(error) => write!(f, "selecting an impl: {error}"),
            BuildError::NoAbi { item } => {
                write!(f, "fn item {:#x}: the build's `Adts` gave no ABI for it", item.0)
            }
            BuildError::NoStatic { expr, item } => {
                write!(f, "expression {}: static item {:#x} is not one the build's `Adts` describes", expr.0, item.0)
            }
            BuildError::NoConstValue { expr, item } => {
                write!(f, "expression {}: const item {:#x} has no evaluated value in the build's `Adts`", expr.0, item.0)
            }
            BuildError::NoConstBlockValue { expr, owner } => {
                write!(f, "expression {}: the const block of fn {:#x} has no evaluated value at this instance in the build's `Adts`", expr.0, owner.0)
            }
            BuildError::NoPromotedValue { expr, owner } => {
                write!(f, "expression {}: the promoted borrow of fn {:#x} has no promoted value at this instance in the build's `Adts`", expr.0, owner.0)
            }
            BuildError::NoConstFnInstance { expr, key } => {
                write!(
                    f,
                    "expression {}: the value of the constant, const block or promoted borrow here holds a fn pointer to instance {key:?}, \
                     which no record the build's `Adts` holds names (every evaluated record names the instance of each of its fn words: \
                     blaze-consteval `ConstRecord::fns`)",
                    expr.0
                )
            }
            BuildError::NoPatConstValue { pat, item } => {
                write!(f, "pattern {}: const item {:#x} has no evaluated value in the build's `Adts`", pat.0, item.0)
            }
            BuildError::MissingRecord { item } => {
                write!(f, "a layout needs the record of item {:#x} (an explicit discriminant), which is not made yet", item.0)
            }
            BuildError::OpaqueOwnerUnchecked { opaque, owner } => write!(
                f,
                "the hidden type of `impl Trait` type {:#x} needs its defining fn {:#x} checked, which is not in the program (not reached, or it did not check)",
                opaque.0, owner.0
            ),
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
            BuildError::Target { target, expr: Some(expr), why } => {
                write!(f, "expression {}: target intrinsic `{}`: {why}", expr.0, target.link_name())
            }
            BuildError::Target { target, expr: None, why } => write!(f, "target intrinsic `{}`: {why}", target.link_name()),
            BuildError::CAbi { item: Some(item), why } => write!(f, "fn item {:#x}: its `extern \"C\"` signature: {why}", item.0),
            BuildError::CAbi { item: None, why } => write!(f, "a call through an `extern \"C\"` fn pointer: {why}"),
            BuildError::Vtable { at: Some(expr), error } => write!(f, "expression {}: a trait object's vtable: {error}", expr.0),
            BuildError::Vtable { at: None, error } => write!(f, "a trait object's vtable: {error}"),
            BuildError::Lir(error) => write!(f, "blaze-lir's builder refused the function: {error}"),
            BuildError::Verify(error) => write!(f, "the LIR built does not verify: {error}"),
            BuildError::Coroutine(error) => write!(f, "an async block's body was not split: {error}"),
            BuildError::Unsize { expr, adt: Some(adt), why, rule } => {
                write!(f, "expression {}: an unsizing coercion through ADT {:#x}: {why} ({rule})", expr.0, adt.0)
            }
            BuildError::Unsize { expr, adt: None, why, rule } => write!(f, "expression {}: an unsizing coercion: {why} ({rule})", expr.0),
            BuildError::NoCleanup { what } => {
                write!(f, "a call that may unwind (panic = unwind): its cleanup is not built exactly: {what} (blaze-build walk/unwind.rs)")
            }
            BuildError::TrackCaller { expr, item, why, gap } => {
                match expr {
                    Some(expr) => write!(f, "expression {}: the `#[track_caller]` location for fn {:#x}: {why}", expr.0, item.0)?,
                    None => write!(f, "a call the build inserted: the `#[track_caller]` location for fn {:#x}: {why}", item.0)?,
                }
                match gap {
                    Some(gap) => write!(f, " ({gap})"),
                    None => Ok(()),
                }
            }
            BuildError::FieldOrder { ty, field, found } => write!(
                f,
                "the layout of {ty:?}: the row of declared field {field} is named {:#x}, not that field's name (rows out of declaration order: blaze-build walk.rs `decl_field`)",
                found.0
            ),
        }
    }
}

impl core::error::Error for BuildError {}

/// Builds coroutine `ordinal` of `body` (the owner's, checked once as `checked`: the
/// `ordinal`-th `async` block in expression order, [`coroutines_in`]) at the owner's instance
/// `subst` names (coroutine.rs, "THE EMIT POINTS"): its entry, its resume fn, its drop and its
/// state's shape. The caller finds the owner, its arguments and the ordinal from the
/// coroutine's ground type ([`coroutine_of`]), for an instance of `Adts::future_poll`, of
/// `AsyncItems::resume` or of `drop_in_place` whose first argument is a coroutine; one build
/// gives all three. Its state is laid out from TYPES before it is built (`plan`:
/// [`coroutine_state::lay_state`] and the flow's table of identities, ASYNC.md P8), and before
/// any body that holds it by value is built; a body that asks a state not laid is refused with
/// [`BuildError::MissingRecord`] naming the owner (never built late). `plan` is `None` only in
/// this crate's unit tests.
pub fn build_coroutine<A: Allocator + Copy>(
    body: &BodyView<'_>,
    checked: &Checked<'_>,
    ordinal: u32,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    plan: Option<coroutine_state::StatePlan<'_>>,
    alloc: A,
) -> Result<BuiltCoroutine<A>, BuildError> {
    if !checked.diags.is_empty() {
        return Err(BuildError::Diagnostics { count: u32::try_from(checked.diags.len()).unwrap_or(u32::MAX) });
    }
    walk::coroutine::build(body, checked, ordinal, subst, adts, plan, alloc)
}

/// Builds the instance `subst` names of `body`, checked once generically as `checked`, of a fn
/// whose signature is `sig`, in `alloc`.
pub fn build<A: Allocator + Copy>(
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
pub fn build_closure<A: Allocator + Copy>(
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

/// Builds the instance of trait method `which` (`Fn::call`, `FnMut::call_mut` or
/// `FnOnce::call_once`) at `[callee, tuple]` where `callee` is a fn ITEM type or a fn POINTER
/// type, not a closure. The Reference, "Function item types": "All function items implement
/// `Fn`, `FnMut`, `FnOnce`, `Copy`, `Clone`, `Send`, and `Sync`"; core's function.rs: "`Fn` is
/// implemented automatically by closures ... as well as (safe) function pointers". The
/// instance is a SHIM: `self` taken as `which` takes it, ignored for a fn item (zero-sized),
/// loaded and called indirectly for a fn pointer, the tuple's fields passed on as the callee's
/// arguments (codegen.md, "Closures and the Fn traits"). For `FnDef(item, args)`, `sig` is
/// `item`'s signature and `subst.args` the list `args` names; for a fn pointer `sig` is `None`
/// and `subst.args` empty. The caller picks this for an `Fn*` method's instance whose first
/// argument [`closure_of`] does not read as a closure.
pub fn build_fn_shim<A: Allocator + Copy>(
    callee: PTy,
    tuple: PTy,
    which: FnTrait,
    sig: Option<&FnSig<'_>>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    walk::build_fn_shim(callee, tuple, which, sig, subst, adts, alloc)
}

/// Builds the `ReifyShim` of `#[track_caller]` fn `callee` (whose signature is `sig`, at the
/// instance `subst` names): the plain fn-pointer entry point (with no hidden `&Location`
/// parameter) that materializes a `Location` row for `callee`'s own definition site
/// ([`Adts::def_site`]) and calls `callee` directly with that location passed last
/// (walk/track_caller.rs).
pub fn build_reify_shim<A: Allocator + Copy>(
    callee: ItemKey,
    sig: &FnSig<'_>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    walk::build_reify_shim(callee, sig, subst, adts, alloc)
}

/// Builds the CTOR FN `ctor` of variant `variant` (a struct's is 0) of a tuple struct or tuple
/// variant at the instance `subst` names: the function the ctor's name is as a VALUE (the
/// Reference, "Structs" and "Enumerations": a tuple-like struct or variant "defines a
/// constructor function"; "Function item types"), its parameters the fields in order, its
/// result the value. `sig` is the ctor's signature. A call written with the ctor's path as its
/// callee is built in place and never calls this; a call through a parenthesized path, a fn
/// item value, a fn pointer or a `Fn*` shim does (ctor_fn.rs).
pub fn build_ctor_fn<A: Allocator + Copy>(
    ctor: ItemKey,
    variant: u32,
    sig: &FnSig<'_>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    walk::build_ctor_fn(ctor, variant, sig, subst, adts, alloc)
}

/// Builds the `Clone::clone` the LANGUAGE gives `Self` (`subst.args`, one type: a tuple, a
/// closure, a fn item or a fn pointer; the Reference, "Special types and traits", `Clone`), at
/// the trait's method `clone`, which no impl's text holds (clone_fn.rs). `copy` is the
/// checker's answer to `Self: Copy` (the seam's `holds`): when it holds, the bytes are copied
/// and no field's `clone` runs; else each field (a closure's captures in order) is cloned by
/// its own `clone`, in order, the clones made dropped if a later one unwinds.
pub fn build_builtin_clone<A: Allocator + Copy>(
    clone: ItemKey,
    copy: bool,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    walk::build_builtin_clone(clone, copy, subst, adts, alloc)
}

/// Builds the builtin `From::from` shim for structural error unions (`subst.args = [Dst, Src]`),
/// injecting a member or remapping a strict sub-union into `Dst`.
pub fn build_builtin_from<A: Allocator + Copy>(
    from: ItemKey,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    walk::build_builtin_from(from, subst, adts, alloc)
}

/// What the instance `subst` names of `body` reads of trait shapes, before it is built: its
/// dynamic calls' slots and its upcasts' words, in expression order. With the instance's
/// [`InstanceKey`] it makes the record's key ([`vtable::record_key`]; codegen.md, "Trait
/// objects: vtables and dynamic calls"): an edit to a trait changes the key of exactly the
/// functions whose slot or word moved.
pub fn object_reads<A: Allocator + Copy>(
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
pub fn build_drop_glue<A: Allocator + Copy>(
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
