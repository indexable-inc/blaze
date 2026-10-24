//! glam's `Vec4::cmpeq` and `is_finite_mask` compare with `_mm_cmpeq_ps` and `_mm_cmplt_ps`:
//! the compare intrinsics (`llvm.x86.sse.cmp.ps`, `.ss`, `llvm.x86.sse2.cmp.pd`, `.sd`) take
//! the predicate as an immediate, give all ones in a lane where it holds, and have no body.
//! NaN is in the lanes to tell the ordered predicates from the unordered ones.

#[cfg(target_arch = "x86_64")]
pub fn run() -> ([[u32; 4]; 10], [[u64; 2]; 6]) {
    use std::arch::x86_64::*;
    let nan = f32::NAN;
    let (x, y) = ([1.0, 2.0, nan, -0.0], [1.0, 3.0, 1.0, 0.0]);
    let (p, q) = ([1.0, f64::NAN], [2.0, 1.0]);
    unsafe {
        let (a, b) = (_mm_loadu_ps(x.as_ptr()), _mm_loadu_ps(y.as_ptr()));
        let (c, d) = (_mm_loadu_pd(p.as_ptr()), _mm_loadu_pd(q.as_ptr()));
        let singles = [
            _mm_cmpeq_ps(a, b),
            _mm_cmplt_ps(a, b),
            _mm_cmple_ps(a, b),
            _mm_cmpunord_ps(a, b),
            _mm_cmpneq_ps(a, b),
            _mm_cmpnlt_ps(a, b),
            _mm_cmpnle_ps(a, b),
            _mm_cmpord_ps(a, b),
            _mm_cmpeq_ss(b, a),
            _mm_cmplt_ss(a, b),
        ];
        let doubles = [
            _mm_cmpeq_pd(c, d),
            _mm_cmplt_pd(c, d),
            _mm_cmpunord_pd(c, d),
            _mm_cmpnle_pd(c, d),
            _mm_cmplt_sd(c, d),
            _mm_cmpord_sd(d, c),
        ];
        (singles.map(|it| std::mem::transmute(it)), doubles.map(|it| std::mem::transmute(it)))
    }
}

#[cfg(not(target_arch = "x86_64"))]
pub fn run() -> ([[u32; 4]; 10], [[u64; 2]; 6]) {
    ([[0; 4]; 10], [[0; 2]; 6])
}
