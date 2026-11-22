//! x-int-02: checked arithmetic returns `None` exactly when the result leaves the type.
//!
//! core's docs: `checked_add` "Checked integer addition. Computes `self + rhs`, returning `None`
//! if overflow occurred."; `checked_div` "Checked integer division. Computes `self / rhs`,
//! returning `None` if `rhs == 0` or the division results in overflow." The Reference,
//! "Arithmetic and Logical Binary Operators": "Integer division rounds towards zero."
//! 1. 200u8 + 56 = 256 > 255: None.          2. 200u8 + 55 = 255: Some(255).
//! 3. u32::MAX * 2 > u32::MAX: None.
//! 4. 65536 * 65535 = 2^32 - 2^16 = 4294967296 - 65536 = 4294901760 <= u32::MAX: Some.
//! 5. i8::MIN / -1 = 128 > i8::MAX: None.     6. 7i8 / 0: None.
//! 7. -7i8 / 2 = -3.5, toward zero: Some(-3).
//! A wrong builder that computes at a wider width and never reports overflow exits 1; one
//! that rounds division down instead of toward zero exits 7.

fn check(a: u8, m: u32, n: i8) -> u8 {
    if a.checked_add(56) != None {
        return 1;
    }
    if a.checked_add(55) != Some(255) {
        return 2;
    }
    if m.checked_mul(2).is_some() {
        return 3;
    }
    if 65536u32.checked_mul(65535) != Some(4294901760) {
        return 4;
    }
    if n.checked_div(-1).is_some() {
        return 5;
    }
    if 7i8.checked_div(n - n).is_some() {
        return 6;
    }
    if (-7i8).checked_div(2) != Some(-3) {
        return 7;
    }
    0
}

fn main() -> std::entry::Status {
    std::entry::Status(check(200, u32::MAX, i8::MIN))
}
