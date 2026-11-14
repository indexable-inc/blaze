//! `blaze prove --emit`: one theorem the translator states, one it refuses for its return type.
#![feature(register_tool)]
#![register_tool(blaze)]

#[blaze::theorem]
pub fn add_commutes(a: u8, b: u8) -> bool {
    a.wrapping_add(b) == b.wrapping_add(a)
}

#[blaze::theorem]
pub fn not_bool() -> u32 {
    0
}
