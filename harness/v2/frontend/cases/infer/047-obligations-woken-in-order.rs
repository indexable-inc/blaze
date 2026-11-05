//@ expect: accept
trait Pick {
    fn pick(self) -> u32;
}

impl Pick for u8 {
    fn pick(self) -> u32 {
        8
    }
}

impl Pick for u16 {
    fn pick(self) -> u32 {
        16
    }
}

fn pick<T: Pick>(t: T) -> u32 {
    t.pick()
}

fn main() {
    let a = Default::default();
    let b = Default::default();
    let c = Default::default();
    let total = pick(c) + pick(a) + pick(b);
    let _: u8 = a;
    let _: u16 = b;
    let _: u8 = c;
    assert!(total == 32);
}
