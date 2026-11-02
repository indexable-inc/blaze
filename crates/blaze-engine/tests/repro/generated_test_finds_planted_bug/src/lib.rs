#![feature(blaze_verify)]

/// The planted bug: `x * 2` is not more than `x` when `x` is 0.
#[blaze::contract_test]
pub fn double(x: u32) -> (r: u32)
    requires x < 1000
    ensures r > x
{
    x * 2
}

/// A contract that holds, over a borrowed argument.
#[blaze::contract_test]
pub fn count(s: &str) -> (n: usize)
    ensures n <= s.len()
{
    s.chars().count()
}

/// Left out: no test is generated.
#[blaze::contract_test(skip)]
pub fn halve(x: u32) -> (r: u32)
    ensures r > x
{
    x / 2
}
