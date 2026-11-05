//@ edition: 2021
use std::ops::{Add, Mul, Neg, Index};
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct V2 { pub x: i32, pub y: i32 }
impl Add for V2 { type Output = V2; fn add(self, o: V2) -> V2 { V2 { x: self.x + o.x, y: self.y + o.y } } }
impl Mul<i32> for V2 { type Output = V2; fn mul(self, k: i32) -> V2 { V2 { x: self.x * k, y: self.y * k } } }
impl Neg for V2 { type Output = V2; fn neg(self) -> V2 { V2 { x: -self.x, y: -self.y } } }
pub struct Grid(pub Vec<u8>);
impl Index<usize> for Grid { type Output = u8; fn index(&self, i: usize) -> &u8 { &self.0[i] } }
