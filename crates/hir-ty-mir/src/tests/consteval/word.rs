//! The evaluator's word is the evaluated crate's: a `usize` and an address take as many bytes
//! as its target's data layout says, whichever crate the process evaluated before.

use hegel::{TestCase, generators as gs};

use super::*;

const WASM32: &str = r#"
//- target_data_layout: e-m:e-p:32:32-p10:8:8-p20:8:8-i64:64-i128:128-n32:64-S128-ni:1:10:20
//- target_arch: wasm32
//- minicore: size_of
const GOAL: usize = size_of::<usize>() * 100 + size_of::<&[u8]>();
"#;

const NATIVE: &str = r#"
//- minicore: size_of
const GOAL: usize = size_of::<usize>() * 100 + size_of::<&[u8]>();
"#;

#[test]
fn a_wasm32_crate_has_four_byte_words() {
    check_number(WASM32, 408);
    check_number(NATIVE, 816);
}

#[test]
fn a_wasm32_usize_wraps_at_32_bits() {
    check_number(
        r#"
//- target_data_layout: e-m:e-p:32:32-p10:8:8-p20:8:8-i64:64-i128:128-n32:64-S128-ni:1:10:20
//- target_arch: wasm32
const GOAL: u64 = (!0usize) as u64;
"#,
        0xFFFF_FFFF,
    );
}

/// No crate's word leaks into another's: one process evaluates wasm32 and native crates in an
/// order Hegel picks, and each sees its own.
#[hegel::test]
fn a_crates_word_is_its_own_whatever_ran_before(tc: TestCase) {
    let order: Vec<bool> = tc.draw(gs::vecs(gs::booleans()).min_size(1).max_size(6));
    for wasm in order {
        match wasm {
            true => check_number(WASM32, 408),
            false => check_number(NATIVE, 816),
        }
    }
}
