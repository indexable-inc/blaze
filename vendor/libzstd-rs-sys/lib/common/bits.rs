#[inline]
pub(crate) const fn ZSTD_highbit32(val: u32) -> u32 {
    val.ilog2()
}
