//@ expect: accept
use core::ops::Deref;

struct Inner(u32);

struct Middle(Inner);

struct Outer(Middle);

impl Deref for Middle {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl Deref for Outer {
    type Target = Middle;
    fn deref(&self) -> &Middle {
        &self.0
    }
}

fn get(inner: &Inner) -> u32 {
    inner.0
}

fn main() {
    let o = Outer(Middle(Inner(5)));
    assert!(get(&o) == 5);
    let r = &&o;
    assert!(get(r) == 5);
}
