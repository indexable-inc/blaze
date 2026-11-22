/// A [`Vec`](crate::Vec) of the values given, in the allocator given first:
/// `vec_in![alloc; a, b, c]`, or `vec_in![alloc; value; n]` for `n` clones of `value`. There is
/// no form without an allocator.
#[macro_export]
macro_rules! vec_in {
    ($alloc:expr $(;)?) => {
        $crate::Vec::new_in($alloc)
    };
    ($alloc:expr; $value:expr; $n:expr) => {
        $crate::Vec::from_elem_in($value, $n, $alloc)
    };
    ($alloc:expr; $($value:expr),* $(,)?) => {
        $crate::Vec::from_array_in([$($value),*], $alloc)
    };
}

/// `format!` into the allocator given first: `format_in!(frame, "{name}-{n}")` is a
/// [`String<&Frame>`](crate::String).
#[macro_export]
macro_rules! format_in {
    ($alloc:expr, $($arg:tt)*) => {
        $crate::text::format($alloc, ::core::format_args!($($arg)*))
    };
}

/// `format!` appended to the caller's buffer, in place of a new string: `format_into!(&mut
/// buf, "{name}-{n}")` writes into `buf` (a [`String<A>`](crate::String) or any `fmt::Write`),
/// so a loop that formats reuses one buffer and allocates only when it grows.
#[macro_export]
macro_rules! format_into {
    ($out:expr, $($arg:tt)*) => {
        $crate::text::format_into($out, ::core::format_args!($($arg)*))
    };
}
