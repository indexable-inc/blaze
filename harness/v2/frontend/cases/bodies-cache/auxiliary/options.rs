//@ edition: 2021
pub fn half(x: i32) -> Option<i32> { if x % 2 == 0 { Some(x / 2) } else { None } }
pub fn quarter(x: i32) -> Option<i32> { half(x).and_then(half) }
pub fn describe(x: i32) -> String { quarter(x).map(|q| format!("q{q}")).unwrap_or_else(|| "odd".into()) }
pub fn first_even(xs: &[i32]) -> Option<&i32> { xs.iter().find(|x| *x % 2 == 0) }
