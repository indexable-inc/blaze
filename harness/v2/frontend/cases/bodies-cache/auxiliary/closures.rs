//@ edition: 2021
pub fn adder(n: u32) -> impl Fn(u32) -> u32 { move |x| x + n }
pub fn compose<A, B, C>(f: impl Fn(A) -> B, g: impl Fn(B) -> C) -> impl Fn(A) -> C { move |x| g(f(x)) }
pub fn apply_n(f: &dyn Fn(u64) -> u64, n: usize, x: u64) -> u64 { (0..n).fold(x, |acc, _| f(acc)) }
pub fn counter() -> impl FnMut() -> u32 { let mut n = 0; move || { n += 1; n } }
pub fn nested(x: u32) -> u32 { let outer = || { let inner = || x * 3; inner() + 1 }; outer() }
