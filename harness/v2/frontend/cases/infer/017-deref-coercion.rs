//@ expect: accept
use core::ops::Deref;

struct Inner {
    value: u32,
}

struct Holder {
    inner: Inner,
}

impl Deref for Holder {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.inner
    }
}

fn value(inner: &Inner) -> u32 {
    inner.value
}

fn main() {
    let h = Holder { inner: Inner { value: 8 } };
    let i: &Inner = &h;
    assert!(value(i) == 8);
    assert!(value(&h) == 8);
}
