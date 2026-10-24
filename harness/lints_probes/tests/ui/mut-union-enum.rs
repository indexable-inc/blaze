#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub union U { pub a: u8, pub b: i8 }
pub enum E { A(u8), B { x: Vec<u8> } }
pub fn f(e: E) -> u8 {
    let mut u = U { a: 1 };
    u.b = 2;
    let mut w = U { a: 1 };
    let r = unsafe { w.a };
    let mut e2 = E::A(1);
    if let E::A(ref mut n) = e2 {
        *n += 1;
    }
    let mut e3 = E::A(1);
    let k = match e3 { E::A(n) => n, E::B { .. } => 0 };
    match e {
        E::A(mut n) => { n += 1; n }
        E::B { mut x } => x.len() as u8,
    };
    r + k
}
