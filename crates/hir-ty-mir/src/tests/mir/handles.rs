//! `#![blaze::handles]` run (docs/v2/plan/design/edition.md, change 1): a move of a handle that
//! is used again clones it once, the last use clones nothing, and a move in a loop clones each
//! turn. `H` counts its clones.

use super::eval::check_pass_and_stdio;

/// (a) and (d).
#[test]
fn a_handle_is_cloned_once_per_use_before_another() {
    check_pass_and_stdio(
        r#"
//- minicore: sized, copy, clone, handle, add, panic
#![blaze::handles]

static mut CLONES: u32 = 0;

struct H(u32);
impl Clone for H {
    fn clone(&self) -> H {
        unsafe { CLONES += 1; }
        H(self.0)
    }
}
impl core::clone::Share for H {}

fn take(h: H) -> u32 { h.0 }

fn clones() -> u32 { unsafe { CLONES } }

fn once() -> u32 {
    let h = H(1);
    take(h)
}

fn twice() -> u32 {
    let h = H(1);
    take(h) + take(h)
}

fn looped() -> u32 {
    let h = H(1);
    let mut sum = 0;
    let mut turn = 0;
    while turn < 3 {
        sum += take(h);
        turn += 1;
    }
    sum
}

fn main() {
    if once() != 1 || clones() != 0 {
        panic!("the last use cloned");
    }
    if twice() != 2 || clones() != 1 {
        panic!("a use with another after it did not clone once");
    }
    if looped() != 3 || clones() != 4 {
        panic!("a move in a loop did not clone each turn");
    }
}
"#,
        "",
        "",
    );
}
