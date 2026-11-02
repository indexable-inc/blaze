//@ edition: 2021
pub struct Countdown(pub u32);
impl Iterator for Countdown { type Item = u32; fn next(&mut self) -> Option<u32> { if self.0 == 0 { None } else { self.0 -= 1; Some(self.0 + 1) } } }
pub fn evens_squared(n: u32) -> Vec<u32> { (1..=n).filter(|x| x % 2 == 0).map(|x| x * x).collect() }
pub fn zipped() -> Vec<(char, u32)> { "abc".chars().zip(Countdown(3)).collect() }
pub fn windows_max(xs: &[i32]) -> Vec<i32> { xs.windows(2).map(|w| w[0].max(w[1])).collect() }
