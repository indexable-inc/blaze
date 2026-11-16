//! bits: `#[blaze::bits(u8)]`, a bit-packed struct whose fields are narrow integers. A working layer accepts it (size 1).
//! expect: accept
#![blaze::arbitrary_ints]

#[blaze::bits(u8)]
struct Flags {
    kind: u3,
    level: u5,
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let f = Flags { kind: 2, level: 10 };
    f.level as i32 * 4 + f.kind as i32
}
