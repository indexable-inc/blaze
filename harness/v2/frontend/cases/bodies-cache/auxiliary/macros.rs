//@ edition: 2021
#[macro_export]
macro_rules! square { ($e:expr) => { $e * $e }; }
macro_rules! private_sum { ($($e:expr),*) => { 0 $(+ $e)* }; }
pub fn sum3() -> u32 { private_sum!(1, 2, 3) }
pub fn squared(x: u32) -> u32 { square!(x) }
