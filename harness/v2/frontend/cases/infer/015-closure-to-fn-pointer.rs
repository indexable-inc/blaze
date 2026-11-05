//@ expect: accept
fn apply(f: fn(u32) -> u32, x: u32) -> u32 {
    f(x)
}

fn main() {
    let add_one = |x: u32| x + 1;
    let f: fn(u32) -> u32 = add_one;
    assert!(apply(f, 1) == 2);
    assert!(apply(|x| x * 10, 2) == 20);
    let g: unsafe fn(u32) -> u32 = |x| x + 5;
    assert!(unsafe { g(1) } == 6);
}
