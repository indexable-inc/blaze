//! [`Show`]: a type's human form, written straight into a [`Put`], in place of `Display` for
//! blaze's own types. Wrappers make a writer's form a value, so it can stand in a column
//! ([`Writes::right`]) or a list ([`Writes::join`]): [`Fixed`], [`Hex`], [`Nanos`], [`Bytes`],
//! [`Quoted`].

use core::alloc::Allocator;

use blaze_own::{Arc, BTreeMap, BTreeSet, Box, Rc, SortedMap, SortedSet, String, Vec, VecDeque};

use crate::float::Float;
use crate::int::Integer;
use crate::parse::ParseError;
use blaze_own::Overflow;

use crate::put::Put;
use crate::write::Writes;

/// A value's human form.
pub trait Show {
    fn show<P: Put + ?Sized>(&self, out: &mut P);
}

macro_rules! show_ints {
    ($($t:ident)*) => {$(
        impl Show for $t {
            #[inline]
            fn show<P: Put + ?Sized>(&self, out: &mut P) {
                out.int(*self);
            }
        }
    )*};
}

show_ints!(u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize);

impl Show for f64 {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.float(*self);
    }
}

impl Show for f32 {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.float(*self);
    }
}

impl Show for bool {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.bool(*self);
    }
}

impl Show for char {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.char(*self);
    }
}

impl Show for str {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.put_str(self);
    }
}

impl<T: Show + ?Sized> Show for &T {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        (**self).show(out);
    }
}

impl<T: Show + ?Sized> Show for &mut T {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        (**self).show(out);
    }
}

/// The value, or `none`.
impl<T: Show> Show for Option<T> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            Some(value) => value.show(out),
            None => out.put_str("none"),
        }
    }
}

/// `[a, b, c]`.
impl<T: Show> Show for [T] {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.byte(b'[').join(self, ", ").byte(b']');
    }
}

impl<T: Show, const N: usize> Show for [T; N] {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        self.as_slice().show(out);
    }
}

impl<A: Allocator> Show for String<A> {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.put_str(self.as_str());
    }
}

impl<T: Show, A: Allocator> Show for Vec<T, A> {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        self.as_slice().show(out);
    }
}

impl<T: Show, A: Allocator> Show for VecDeque<T, A> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.byte(b'[').join(self.iter(), ", ").byte(b']');
    }
}

impl<T: Show + ?Sized, A: Allocator> Show for Box<T, A> {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        (**self).show(out);
    }
}

impl<T: Show + ?Sized, A: Allocator> Show for Arc<T, A> {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        (**self).show(out);
    }
}

impl<T: Show + ?Sized, A: Allocator> Show for Rc<T, A> {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        (**self).show(out);
    }
}

/// `{k: v, k: v}`, in the map's order.
fn map<'a, K: Show + 'a, V: Show + 'a, P: Put + ?Sized>(entries: impl Iterator<Item = (&'a K, &'a V)>, out: &mut P) {
    out.byte(b'{');
    let mut first = true;
    for (k, v) in entries {
        if !first {
            out.str(", ");
        }
        first = false;
        out.show(k).str(": ").show(v);
    }
    out.byte(b'}');
}

impl<K: Show + Ord, V: Show, A: Allocator> Show for SortedMap<K, V, A> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        map(self.as_slice().iter().map(|(k, v)| (k, v)), out);
    }
}

impl<K: Show, V: Show, A: Allocator> Show for BTreeMap<K, V, A> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        map(self.iter(), out);
    }
}

/// `{a, b}`, in the set's order.
impl<T: Show + Ord, A: Allocator> Show for SortedSet<T, A> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.byte(b'{').join(self.as_slice(), ", ").byte(b'}');
    }
}

impl<T: Show, A: Allocator> Show for BTreeSet<T, A> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.byte(b'{').join(self.iter(), ", ").byte(b'}');
    }
}

/// `expected X at byte N`.
impl Show for ParseError {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.str("expected ").str(self.expected).str(" at byte ").int(self.at);
    }
}

/// `text needs N bytes; the buffer holds M`.
impl Show for Overflow {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        // As blaze-own's `Display` for it says.
        out.str("a fixed string of ").int(self.capacity).str(" bytes was asked for ").int(self.needed);
    }
}

/// A float with a fixed number of places: `Fixed(x, 2)` shows as [`Writes::float_fixed`].
#[derive(Clone, Copy, Debug)]
pub struct Fixed<F: Float>(pub F, pub usize);

impl<F: Float> Show for Fixed<F> {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.float_fixed(self.0, self.1);
    }
}

/// An integer in lowercase hex, no prefix.
#[derive(Clone, Copy, Debug)]
pub struct Hex<I: Integer>(pub I);

impl<I: Integer> Show for Hex<I> {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.hex(self.0);
    }
}

/// Nanoseconds as [`Writes::duration`].
#[derive(Clone, Copy, Debug)]
pub struct Nanos(pub u64);

impl Show for Nanos {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.duration(self.0);
    }
}

/// A byte count as [`Writes::byte_count`].
#[derive(Clone, Copy, Debug)]
pub struct Bytes(pub u64);

impl Show for Bytes {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.byte_count(self.0);
    }
}

/// Any value in `{:?}`'s layout, as [`Writes::debug`]: what `{:?}` was at a format site.
pub struct Dbg<'a, T: ?Sized>(pub &'a T);

impl<T: ?Sized> Show for Dbg<'_, T> {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.debug(self.0);
    }
}

/// Text as a quoted Rust literal, as [`Writes::escaped`].
#[derive(Clone, Copy, Debug)]
pub struct Quoted<'a>(pub &'a str);

impl Show for Quoted<'_> {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.escaped(self.0);
    }
}

/// A [`Show`] made from a writing closure: what was `format_args!(..)` handed on to be
/// written later. As large as what the closure captures; zero-sized when it captures nothing.
pub struct ShowWith<F: Fn(&mut dyn Put)>(pub F);

/// `show_with(|out| _ = out.str("x = ").int(x))`: a value that writes itself by the closure.
#[inline]
pub fn show_with<F: Fn(&mut dyn Put)>(write: F) -> ShowWith<F> {
    ShowWith(write)
}

impl<F: Fn(&mut dyn Put)> Show for ShowWith<F> {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        let mut sink: &mut P = out;
        (self.0)(&mut sink);
    }
}
