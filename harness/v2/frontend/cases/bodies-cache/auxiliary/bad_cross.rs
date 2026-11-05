//@ edition: 2021
//@ aux-crate: arith=arith.rs
pub fn broken() -> u32 { arith::takes_u32(1u8) }
