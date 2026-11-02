//@ edition: 2021
//@ aux-crate: chain_c=chain_c.rs
pub fn left() -> u32 { chain_c::base() + 1 }
