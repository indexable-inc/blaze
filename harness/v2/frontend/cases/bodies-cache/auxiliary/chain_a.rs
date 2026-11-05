//@ edition: 2021
//@ aux-crate: chain_b=chain_b.rs
pub const LEN: usize = chain_b::B as usize;
pub fn a_array() -> [u8; LEN] { [7; LEN] }
pub fn a_value() -> u32 { chain_b::b_value() * 10 }
pub fn names() -> String { format!("a<{}>", chain_b::names()) }
