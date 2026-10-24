#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub fn f() -> u8 {
    let mut x = 1u8;
    let x = x + 1;
    let mut y = 1u8;
    y += 1;
    let mut y = y;
    y += 1;
    let mut z = 1u8;
    {
        let mut z = 2u8;
        z += 1;
    }
    let mut _u = 3u8;
    let mut w = 1u8;
    let w = &mut w;
    *w += 1;
    x + y + z + *w
}
