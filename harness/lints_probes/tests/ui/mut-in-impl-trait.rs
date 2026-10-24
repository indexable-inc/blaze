#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
use std::ops::{AddAssign, Index, IndexMut};
pub struct V(pub Vec<u8>);
impl Index<usize> for V { type Output = u8; fn index(&self, i: usize) -> &u8 { &self.0[i] } }
impl IndexMut<usize> for V { fn index_mut(&mut self, i: usize) -> &mut u8 { &mut self.0[i] } }
#[derive(Clone, Copy)]
pub struct N(pub u8);
impl AddAssign for N { fn add_assign(&mut self, o: N) { self.0 += o.0 } }
pub fn f() -> u8 {
    let mut a = V(vec![1]);
    a[0] = 2;
    let mut b = V(vec![1]);
    let r = b[0];
    let mut c = N(1);
    c += N(2);
    let mut d = N(1);
    let e = d;
    a[0] + r + c.0 + e.0
}
