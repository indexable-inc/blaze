//@ edition: 2021
pub fn checked_div(a: u32, b: u32) -> u32 { if b == 0 { panic!("division by zero: {a}/{b}"); } a / b }
pub fn index(xs: &[u8], i: usize) -> u8 { xs[i] }
