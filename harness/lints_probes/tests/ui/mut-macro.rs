#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
macro_rules! inc { ($x:ident) => { $x += 1 } }
macro_rules! decl { ($x:ident) => { let mut $x = 1u8; } }
macro_rules! inner { () => {{ let mut k = 1u8; k }} }
macro_rules! bind { ($p:pat) => { let $p = 2u8; } }
pub fn f() -> u8 {
    let mut x = 0u8;
    inc!(x);
    let mut y = 0u8;
    println!("{}", y);
    decl!(z);
    let mut w = vec![1u8];
    assert_eq!(w.len(), 1);
    let mut v = vec![1u8];
    write(&mut v);
    bind!(mut q);
    let mut s = String::new();
    s += &format!("{}", 1);
    x + inner!() + q
}
fn write(v: &mut Vec<u8>) { v.push(1) }
