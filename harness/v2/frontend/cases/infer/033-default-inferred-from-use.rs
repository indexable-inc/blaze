//@ expect: accept
trait Zero {
    fn zero() -> Self;
}

impl Zero for u32 {
    fn zero() -> Self {
        0
    }
}

impl Zero for u8 {
    fn zero() -> Self {
        0
    }
}

fn main() {
    let a = Zero::zero();
    let b = Zero::zero();
    let x: u32 = a;
    let y: u8 = b;
    assert!(x == 0 && y == 0);
}
