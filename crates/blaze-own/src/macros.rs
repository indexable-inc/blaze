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
