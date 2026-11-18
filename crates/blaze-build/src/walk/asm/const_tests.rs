//! A `const` operand of `asm!` as template text: Rust's `Display` of the integer at its type
//! (the Reference, inline-assembly.html, "const": "formatted as a string"), each expected
//! string written from the value, not from the formatter.

use super::ConstText;

fn text(bits: u128, width: u32, signed: bool) -> String {
    let mut out = Vec::new();
    ConstText { bits, width, signed }.write(&mut out);
    String::from_utf8(out).expect("decimal digits and a sign")
}

/// Prints: `asm const: 9 values formatted as Display formats them`.
#[test]
fn const_text_is_display() {
    let cases: &[(u128, u32, bool, &str)] = &[
        (4, 32, true, "4"),
        (0, 64, false, "0"),
        // `const -1i32`: the walk's bits are the negation of 1, cut to 32 bits.
        (1u128.wrapping_neg(), 32, true, "-1"),
        (0xffff_ffff, 32, false, "4294967295"),
        (0x80, 8, true, "-128"),
        // Bits past the type's width are not the value's.
        (0x1_0000_0004, 32, false, "4"),
        (1 << 127, 128, true, "-170141183460469231731687303715884105728"),
        (u128::MAX, 128, false, "340282366920938463463374607431768211455"),
        (u128::from(u64::MAX), 64, true, "-1"),
    ];
    for &(bits, width, signed, want) in cases {
        assert_eq!(text(bits, width, signed), want, "{bits:#x} at {width} bits, signed {signed}");
    }
    println!("asm const: {} values formatted as Display formats them", cases.len());
}
