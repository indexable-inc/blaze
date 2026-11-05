//@ edition: 2021
pub fn shout(s: &str) -> String { s.to_uppercase() + "!" }
pub fn words(s: &str) -> Vec<String> { s.split_whitespace().map(|w| w.to_string()).collect() }
pub fn padded(n: u32) -> String { format!("[{:>5}|{:<5}|{:^5}]", n, n, n) }
pub fn precision(x: f64) -> String { format!("{:.3} {:e}", x, x) }
pub fn reverse_chars(s: &str) -> String { s.chars().rev().collect() }
pub fn count_utf8(s: &str) -> (usize, usize) { (s.len(), s.chars().count()) }
