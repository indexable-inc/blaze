// Found by edits.py, seed blaze-real-edit-20261026-29, edition 2024, backend cranelift.
// A new caller reaches an unchanged generic helper whose callee changed.
mod c0 {
    use super::*;
    #[inline(never)] fn leaf<T>() { feed(1); }
    #[inline(never)] fn bridge<T>() { leaf::<T>(); }
    #[inline(never)] fn old() { bridge::<u8>(); }
    pub fn run() { old(); }
}
//// then
mod c0 {
    use super::*;
    #[inline(never)] fn leaf<T>() { feed(2); }
    #[inline(never)] fn bridge<T>() { leaf::<T>(); }
    #[inline(never)] fn old() {}
    pub fn run() { bridge::<u8>(); }
}
