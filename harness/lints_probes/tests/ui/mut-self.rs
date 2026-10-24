#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub struct S(pub u8);
impl S {
    pub fn read(mut self) -> u8 { self.0 }
    pub fn write(mut self) -> Self { self.0 = 1; self }
    pub fn bump(&mut self) { self.0 += 1 }
    pub fn via(mut self) -> u8 { self.bump(); self.0 }
    pub fn boxed(mut self: Box<Self>) -> u8 { self.0 }
    pub fn boxed_write(mut self: Box<Self>) -> u8 { self.0 = 2; self.0 }
}
pub trait T: Sized {
    fn m(&mut self);
    fn by(mut self) -> Self { self.m(); self }
    fn not(mut self) -> Self { self }
}
