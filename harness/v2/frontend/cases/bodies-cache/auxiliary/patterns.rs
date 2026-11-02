//@ edition: 2021
pub fn classify(xs: &[i32]) -> &'static str {
    match xs { [] => "empty", [_] => "one", [first, .., last] if first == last => "same ends", [_, rest @ ..] if rest.len() > 2 => "long", _ => "short" }
}
pub fn bucket(n: u32) -> u32 { match n { 0 => 0, 1..=9 => 1, 10 | 20 | 30 => 2, x @ 11..=99 => x / 10, _ => 9 } }
