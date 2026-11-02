//@ edition: 2021
pub fn first_pair(limit: u32) -> (u32, u32) {
    let mut found = (0, 0);
    'outer: for a in 1..limit { for b in a..limit { if a * b == 12 && a + b == 7 { found = (a, b); break 'outer; } } }
    found
}
pub fn collatz(mut n: u64) -> u32 { let mut steps = 0; while n != 1 { n = if n % 2 == 0 { n / 2 } else { 3 * n + 1 }; steps += 1; } steps }
pub fn loop_value() -> u32 { let mut i = 0; loop { i += 3; if i > 10 { break i * 2; } } }
