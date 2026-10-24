#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
#[derive(Default, Clone)]
pub struct C { pub a: u8, pub b: Vec<u8> }
pub fn f() -> C {
    let mut base = C::default();
    let c = C { a: 1, ..base };
    let mut d = C::default();
    d.b.push(1);
    let mut e = C::default();
    let taken = std::mem::take(&mut e.b);
    let mut g = C::default();
    let h = C { b: g.b, ..C::default() };
    let mut i = C::default();
    std::mem::swap(&mut i.a, &mut 3);
    let mut j = C::default();
    let k = j.clone();
    d
}
