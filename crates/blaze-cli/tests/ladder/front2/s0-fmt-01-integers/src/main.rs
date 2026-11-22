//! s0-fmt-01: integers through the real `std`'s `println!` (edition 2024, plain `fn main()`).
//!
//! First to need: `std::rt::lang_start` as the entry, `Stdout`'s `LineWriter`, `format_args!`
//! with width, fill, alignment, sign, `#` and `0` flags, and the integer formatting traits
//! (`Display`, `LowerHex`, `UpperHex`, `Octal`, `Binary`, `LowerExp`) of the pinned `core`
//! (library/core/src/fmt/num.rs). The rules, from the `std::fmt` module docs (library/alloc/src/
//! fmt.rs): numbers are right-aligned by default; `^` centres with the odd pad on the right
//! (`Alignment::Center => (padding / 2, (padding + 1) / 2)` in core/src/fmt/mod.rs `padding`);
//! `0` pads after the sign; `#x` prefixes `0x` and the width counts the prefix; a signed value in
//! hex is its two's complement at its own width; `{:e}` on an integer drops trailing zeros.

fn main() {
    let n = 42;
    println!("{}", n);
    println!("[{:5}]", n);
    println!("[{:<5}]", n);
    println!("[{:^6}]", n);
    println!("[{:^7}]", n);
    println!("[{:05}]", -42);
    println!("{:+}", 7);
    println!("{:x} {:X} {:o} {:b}", 255, 255, 8, 5);
    println!("{:#x} {:#b} {:#o}", 255, 5, 8);
    println!("{:#010x}", 255);
    println!("{} {}", i64::MIN, u64::MAX);
    println!("{}", i128::MAX);
    println!("{}", u128::MAX);
    println!("{:x}", -1i8);
    println!("{}", 1_000_000u32 * 3);
    println!("{:>w$}|", 7, w = 4);
    println!("{:e}", 1234500);
}
