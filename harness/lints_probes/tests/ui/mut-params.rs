#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub fn two(mut a: u8, mut b: u8) -> u8 { a += 1; a + b }
pub fn pat((mut a, mut b): (u8, u8)) -> u8 { b += 1; a + b }
pub fn vec(mut v: Vec<u8>) -> Vec<u8> { v }
pub fn vec_sorted(mut v: Vec<u8>) -> Vec<u8> { v.sort(); v }
pub fn r(mut r: &mut u8) { *r += 1 }
pub fn rr<'a>(mut r: &'a u8, other: &'a u8) -> &'a u8 { r = other; r }
pub fn slice(mut s: &[u8]) -> u8 { while let [first, rest @ ..] = s { s = rest; } 0 }
pub fn generic<T: Default>(mut t: T) -> T { t = T::default(); t }
pub fn unused_generic<T>(mut t: T) -> T { t }
pub fn nested() -> u8 {
    fn inner(mut x: u8) -> u8 { x }
    inner(1)
}
