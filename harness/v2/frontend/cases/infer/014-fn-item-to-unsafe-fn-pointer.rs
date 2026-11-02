//@ expect: accept
fn triple(x: u32) -> u32 {
    x * 3
}

fn main() {
    let f: unsafe fn(u32) -> u32 = triple;
    assert!(unsafe { f(2) } == 6);
}
