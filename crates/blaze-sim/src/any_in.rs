//! A draw IN AN ALLOCATOR the caller hands (D109's drawn parameters): [`AnyIn<A>`].
//!
//! [`Any`] draws a type by its own rule with no allocator, so its lists and texts are the
//! global heap's (`Vec<T>`, `String`). A value a program of the blaze edition takes is made
//! in an allocator it was handed (`Vec<u32, &Frame>`, bstd's `String<&Frame>`): `AnyIn<A>`
//! draws it there, `alloc` passed in by the caller, nothing ambient (rule 75). bstd's
//! `#[bstd::test]` draws each drawn parameter with it, in the start's frame.
//!
//! Every draw here is built from the tape's public draws and `Any`'s, so a value drawn with
//! `AnyIn` and the same type drawn with `Any` take the SAME choices: a list's length and spans
//! are `Tape::vec`'s (its own list holds `()`, which allocates nothing, and the elements are
//! pushed into the caller's list as they are drawn, as `Tape::text` does), so it shrinks and
//! replays as `Any`'s does, with `edge-first`'s list shapes too.
//!
//! Not a blanket over `Any`: `Vec<T, Global>` is `Any` (draw.rs), so `impl<T: Any, A> AnyIn<A>
//! for T` would overlap `impl AnyIn<A> for Vec<T, A>` at `A = Global`. The leaf types are
//! listed; a type of a user's that draws with no allocator is made one with
//! [`any_in_by_any!`](crate::any_in_by_any).
//!
//! Known gap: under `edge-first` a text drawn here is `Tape::char`s in `Tape::vec`'s list, not
//! `edge_text`'s shapes (those write into a global `String`); a text drawn with `Any` there may
//! take other choices than the same tape gives here.

use core::alloc::Allocator;

use crate::tape::Tape;

/// The most elements of a list, and characters of a text, drawn here: `Any`'s bound
/// (draw.rs `ANY_LONGEST`), so both draw the same lengths.
pub const LONGEST: usize = 16;

/// A type drawn in the allocator `A`, which the caller hands: calm is its simplest value.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be drawn in the allocator `{A}`: it is not `blaze_sim::AnyIn<{A}>`",
    note = "a type with no allocator in it that is `blaze_sim::Any` is made one with `blaze_sim::any_in_by_any!(Type);`; a type with lists in it implements `AnyIn<A>` by drawing each field with `AnyIn::any_in(tape, label, alloc.clone())`"
)]
pub trait AnyIn<A: Allocator>: Sized {
    fn any_in(tape: &Tape, label: &'static str, alloc: A) -> Self;
}

/// `AnyIn<A>` for every `A`, for types that hold no allocator: drawn by their `Any`. For a
/// user's type, at the crate that defines it: `blaze_sim::any_in_by_any!(Point, Side);`.
#[macro_export]
macro_rules! any_in_by_any {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl<A: ::core::alloc::Allocator> $crate::AnyIn<A> for $ty {
                fn any_in(tape: &$crate::Tape, label: &'static str, _alloc: A) -> $ty {
                    <$ty as $crate::Any>::any(tape, label)
                }
            }
        )+
    };
}

any_in_by_any!(bool, char, u8, u16, u32, u64, usize, u128, i8, i16, i32, i64, isize, i128, f32, f64, ());

/// Tuples: each element in order, each handed its own copy of the allocator.
macro_rules! any_in_tuple {
    ($($element:ident),+) => {
        impl<Al: Allocator + Clone, $($element: AnyIn<Al>),+> AnyIn<Al> for ($($element,)+) {
            fn any_in(tape: &Tape, label: &'static str, alloc: Al) -> Self {
                ($($element::any_in(tape, label, alloc.clone()),)+)
            }
        }
    };
}

any_in_tuple!(B);
any_in_tuple!(B, C);
any_in_tuple!(B, C, D);
any_in_tuple!(B, C, D, E);
any_in_tuple!(B, C, D, E, F);
any_in_tuple!(B, C, D, E, F, G);

impl<A: Allocator + Clone, T: AnyIn<A>, const N: usize> AnyIn<A> for [T; N] {
    fn any_in(tape: &Tape, label: &'static str, alloc: A) -> [T; N] {
        tape.array(label, |tape| T::any_in(tape, label, alloc.clone()))
    }
}

impl<A: Allocator + Clone, T: AnyIn<A>> AnyIn<A> for Option<T> {
    fn any_in(tape: &Tape, label: &'static str, alloc: A) -> Option<T> {
        tape.option(label, |tape| T::any_in(tape, label, alloc))
    }
}

/// Up to [`LONGEST`] elements, the list `Tape::vec` decides.
impl<A: Allocator + Clone, T: AnyIn<A>> AnyIn<A> for Vec<T, A> {
    fn any_in(tape: &Tape, label: &'static str, alloc: A) -> Vec<T, A> {
        let mut out = Vec::new_in(alloc.clone());
        // `Tape::vec`'s own list holds `()`: it decides the length and the spans, and
        // allocates nothing.
        let _shape: Vec<()> = tape.vec(0..=LONGEST, label, |tape| out.push(T::any_in(tape, label, alloc.clone())));
        out
    }
}

/// Up to [`LONGEST`] elements, as the list above.
impl<A: Allocator + Clone, T: AnyIn<A>> AnyIn<A> for blaze_own::Vec<T, A> {
    fn any_in(tape: &Tape, label: &'static str, alloc: A) -> blaze_own::Vec<T, A> {
        let mut out = blaze_own::Vec::with_capacity_in(0, alloc.clone());
        let _shape: Vec<()> = tape.vec(0..=LONGEST, label, |tape| out.push(T::any_in(tape, label, alloc.clone())));
        out
    }
}

/// Up to [`LONGEST`] characters of `Tape::char`, each its own span: `Tape::text`'s choices
/// without `edge-first` (the module's known gap).
impl<A: Allocator> AnyIn<A> for blaze_own::String<A> {
    fn any_in(tape: &Tape, label: &'static str, alloc: A) -> blaze_own::String<A> {
        let mut out = blaze_own::String::new_in(alloc);
        let _shape: Vec<()> = tape.vec(0..=LONGEST, label, |tape| out.push(tape.char(label)));
        out
    }
}

impl<A: Allocator + Clone, T: AnyIn<A>> AnyIn<A> for Box<T, A> {
    fn any_in(tape: &Tape, label: &'static str, alloc: A) -> Box<T, A> {
        Box::new_in(T::any_in(tape, label, alloc.clone()), alloc)
    }
}

#[cfg(test)]
mod tests;
