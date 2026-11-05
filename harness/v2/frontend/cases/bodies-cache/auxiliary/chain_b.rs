//@ edition: 2021
//@ aux-crate: chain_c=chain_c.rs
pub const B: u32 = chain_c::base() * 2;
pub fn b_value() -> u32 { B + 1 }
pub fn names() -> String { format!("b<{}>", chain_c::c_name()) }
