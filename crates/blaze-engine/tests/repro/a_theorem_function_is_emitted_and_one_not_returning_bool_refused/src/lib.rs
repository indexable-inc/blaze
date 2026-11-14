//! `blaze prove --emit`: one theorem the translator states, one it refuses for its return type.
//! `--subset`: a sans-IO module with one function the translator takes, one generic function
//! and one it refuses (a raw pointer).
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

#[blaze::theorem]
pub fn add_commutes(a: u8, b: u8) -> bool {
    a.wrapping_add(b) == b.wrapping_add(a)
}

#[blaze::theorem]
pub fn not_bool() -> u32 {
    0
}

pub mod cores {
    #![blaze::sans_io]

    pub fn double(x: u8) -> u8 {
        x.wrapping_mul(2)
    }

    pub fn first<T: Copy>(x: T, _y: T) -> T {
        x
    }

    pub fn addr(x: &u8) -> *const u8 {
        x as *const u8
    }
}
