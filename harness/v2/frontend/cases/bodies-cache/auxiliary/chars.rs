//@ edition: 2021
pub fn caesar(s: &str, k: u8) -> String { s.chars().map(|c| if c.is_ascii_lowercase() { (((c as u8 - b'a' + k) % 26) + b'a') as char } else { c }).collect() }
pub fn digits(s: &str) -> u32 { s.chars().filter_map(|c| c.to_digit(10)).sum() }
