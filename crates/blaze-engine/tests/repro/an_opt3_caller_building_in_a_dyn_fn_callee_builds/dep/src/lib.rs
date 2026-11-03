/// As hashbrown's `RawTableInner::resize_inner`: `#[inline(always)]`, generic, and calling
/// through the `&dyn Fn` it is given.
#[inline(always)]
pub fn resize<T>(x: u32, hasher: &dyn Fn(u32) -> u32) -> u32 {
    hasher(x) + 1
}
