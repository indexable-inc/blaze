//@ expect: accept
fn negate(x: i32) -> i32 {
    -x
}

fn main() {
    let f: fn(i32) -> i32 = negate;
    let g: unsafe fn(i32) -> i32 = f;
    assert!(unsafe { g(4) } == -4);
}
