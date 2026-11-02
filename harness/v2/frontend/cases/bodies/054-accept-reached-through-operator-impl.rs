//@ expect: accept
use std::ops::Add;

#[derive(Clone, Copy, Debug, PartialEq)]
struct V2(i32, i32);

impl Add for V2 {
    type Output = V2;

    fn add(self, other: V2) -> V2 {
        V2(self.0 + other.0, self.1 + other.1)
    }
}

fn main() {
    let sum = V2(1, 2) + V2(3, 4);
    assert_eq!(sum, V2(4, 6));
    println!("{sum:?}");
}
