//@ expect: accept
#![feature(derive_coerce_pointee)]

use core::marker::CoercePointee;

#[derive(CoercePointee)]
#[repr(transparent)]
struct MyPtr<'a, #[pointee] T: ?Sized>(&'a T);

trait Named {
    fn id(&self) -> u32;
}

struct Two;

impl Named for Two {
    fn id(&self) -> u32 {
        2
    }
}

fn main() {
    let p: MyPtr<dyn Named> = MyPtr(&Two);
    assert!(p.0.id() == 2);
    let a = [7u8; 4];
    let s: MyPtr<[u8]> = MyPtr(&a);
    assert!(s.0.len() == 4);
}
