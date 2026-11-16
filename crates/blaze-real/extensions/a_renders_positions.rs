//! renders_positions: `#[blaze::renders_positions]` marks the one function allowed to read positions; an unmarked caller reaches it. A working layer refuses the caller (`blaze::reads_positions`).
//! expect: refuse
#![deny(blaze::reads_positions)]

#[blaze::renders_positions]
fn line_of(offset: i32) -> i32 {
    offset / 80
}

fn key_of(offset: i32) -> i32 {
    line_of(offset)
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    key_of(3360)
}
