#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
use std::cell::RefCell;
use std::rc::Rc;
pub fn f() -> u8 {
    let mut b = Box::new(1u8);
    *b = 2;
    let mut c = Box::new(1u8);
    let y = *c;
    let mut d = Box::new((1u8, 2u8));
    d.0 = 3;
    let mut cell = RefCell::new(1u8);
    *cell.borrow_mut() = 2;
    let mut rc = Rc::new(1u8);
    let z = *rc;
    let mut rc2 = Rc::new(1u8);
    if let Some(m) = Rc::get_mut(&mut rc2) {
        *m = 3;
    }
    let mut s = String::from("a");
    let r: &mut str = &mut s;
    *b + y + d.0 + z
}
