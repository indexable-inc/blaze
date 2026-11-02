//@ edition: 2021
pub const fn fib(n: u32) -> u64 { let mut a = 0; let mut b = 1; let mut i = 0; while i < n { let t = a + b; a = b; b = t; i += 1; } a }
pub const FIB10: u64 = fib(10);
pub const LEN: usize = 2 + 3;
pub static TABLE: [u8; LEN] = [1, 2, 3, 4, 5];
pub fn table_sum() -> u32 { TABLE.iter().map(|&b| b as u32).sum() }
pub fn inline_const() -> u32 { const { 6 * 7 } }
#[repr(i32)]
pub enum Level { Low = 1, Mid = LEN as i32, High = 10 }
pub fn level_value(l: Level) -> i32 { l as i32 }
