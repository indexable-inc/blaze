//@ edition: 2021
pub fn add(a: u32, b: u32) -> u32 { a + b }
pub fn mul(a: u32, b: u32) -> u32 { a * b }
pub const fn square(x: u32) -> u32 { x * x }
pub const NINE: u32 = square(3);
pub static TWELVE: u32 = NINE + 3;
pub fn takes_u32(x: u32) -> u32 { x + 1 }
