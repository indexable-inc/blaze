//! blaze's additions to `core::simd`, re-exported there: blaze's extension, not the Rust
//! project's (`library/UPSTREAM` lists this module among blaze's changes to the standard
//! library; docs/v2/plan/design/simd.md).
//!
//! Every item is unstable under `blaze_simd`, which `#![blaze::simd]` on a crate root turns on
//! with `portable_simd`. Nothing here changes what any other item of `core` does, and the vector
//! types stay portable-simd's: this module adds no type, trait or operator.

/// The number of lanes of element type `$elem` in the widest vector register the crate that
/// calls it is compiled for: `Simd<f32, { native_lanes!(f32) }>` is `f32x4` on x86-64's baseline,
/// `f32x8` with `avx2` and `f32x16` with `avx512f`. Never less than one, so a target with no
/// vector unit gets `Simd<T, 1>`.
///
/// It reads `cfg!(target_feature)` of the calling crate, which is why it is a macro and no
/// function: the answer follows the build's flags (`-C target-cpu`, `-C target-feature`), never
/// the machine the code later runs on, so a type written with it has one layout in every build of
/// those flags. `avx` without `avx2` counts as 16 bytes, since integer vectors of 32 would be
/// split. The element type must be known where the macro is called: `native_lanes!(T)` of a
/// generic `T` in a const argument needs `generic_const_exprs`.
#[unstable(feature = "blaze_simd", issue = "none")]
pub macro native_lanes($elem:ty) {{
    let bytes: usize = if $crate::cfg!(target_feature = "avx512f") {
        64
    } else if $crate::cfg!(any(target_feature = "avx2", target_feature = "lasx")) {
        32
    } else if $crate::cfg!(any(
        target_feature = "sse2",
        target_feature = "neon",
        target_feature = "simd128",
        target_feature = "lsx",
        target_feature = "altivec",
        target_feature = "vector",
        target_feature = "v",
    )) {
        16
    } else {
        0
    };
    let size: usize = $crate::mem::size_of::<$elem>();
    if size == 0 || bytes < size { 1 } else { bytes / size }
}}
