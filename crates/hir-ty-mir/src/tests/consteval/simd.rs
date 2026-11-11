//! `core::simd::native_lanes!` (docs/v2/plan/design/simd.md): the macro blaze's `core` has
//! (`library/core/src/blaze_simd.rs`, the file itself, mounted in a crate whose `cfg` and
//! `mem::size_of` are minicore's) answers the widest vector the crate's `target_feature` cfgs
//! enable, for each element size.

use super::*;

/// The lanes `native_lanes!(elem)` gives a crate with the target features `features`.
fn check_lanes(features: &[&str], elem: &str, lanes: i128) {
    let cfg = match features {
        [] => String::new(),
        _ => {
            let cfgs: Vec<String> =
                features.iter().map(|it| format!("target_feature={it}")).collect();
            format!(" cfg:{}", cfgs.join(","))
        }
    };
    let source = include_str!("../../../../../library/core/src/blaze_simd.rs");
    let fixture = format!(
        "//- minicore: cfg, size_of\n//- /main.rs{cfg}\n#![feature(decl_macro)]\n\
         pub use core::cfg;\npub mod mem {{ pub use core::mem::size_of; }}\n\
         pub mod blaze_simd {{\n{source}\n}}\n\
         const GOAL: usize = blaze_simd::native_lanes!({elem});\n"
    );
    check_number(&fixture, lanes);
}

#[test]
fn native_lanes_is_the_widest_enabled_vector() {
    let cases: &[(&[&str], &str, i128)] = &[
        // No vector unit: one lane, whatever the element.
        (&[], "f32", 1),
        (&[], "u64", 1),
        // 128 bits: x86-64's baseline, AArch64's, wasm's, and the others' named features.
        (&["sse2"], "f32", 4),
        (&["sse", "sse2"], "f64", 2),
        (&["sse2"], "u8", 16),
        (&["neon"], "f32", 4),
        (&["neon"], "i16", 8),
        (&["simd128"], "u32", 4),
        (&["lsx"], "u64", 2),
        (&["altivec"], "f32", 4),
        (&["vector"], "f64", 2),
        (&["v"], "u8", 16),
        // `avx` alone widens only floats: still 128 bits.
        (&["sse2", "avx"], "f32", 4),
        // 256 bits.
        (&["sse2", "avx", "avx2"], "f32", 8),
        (&["sse2", "avx", "avx2"], "u8", 32),
        (&["lsx", "lasx"], "f64", 4),
        // 512 bits, the widest named wins whatever else is on.
        (&["sse2", "avx", "avx2", "avx512f"], "f32", 16),
        (&["sse2", "avx", "avx2", "avx512f"], "f64", 8),
        (&["sse2", "avx", "avx2", "avx512f"], "u8", 64),
        // An element wider than the register, and one of no size, get one lane.
        (&["sse2"], "[u64; 4]", 1),
        (&["sse2"], "()", 1),
        (&["sse2"], "u128", 1),
    ];
    for (features, elem, lanes) in cases {
        check_lanes(features, elem, *lanes);
    }
}
