//@ edition: 2021
pub fn big(x: u64) -> u128 { (x as u128) * (x as u128) * 3 }
pub fn wrap(x: u8) -> u8 { x.wrapping_add(200).wrapping_mul(3) }
pub fn checked(x: i32) -> Option<i32> { x.checked_mul(1_000_000) }
pub fn bits(x: u32) -> (u32, u32, u32) { (x.count_ones(), x.leading_zeros(), x.rotate_left(5)) }
