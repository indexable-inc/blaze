//@ expect: accept
use core::ops::Deref;

struct Text([u8; 3]);

impl Deref for Text {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

fn len(s: &[u8]) -> usize {
    s.len()
}

fn main() {
    let t = Text(*b"abc");
    let a = [0u8; 5];
    let mut total = 0;
    total += len(&t);
    total += len(&a);
    total += len(&t);
    total += len(&a);
    total += len(&t);
    total += len(&a);
    assert!(total == 24);
}
