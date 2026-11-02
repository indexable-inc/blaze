//@ expect: accept
fn double(x: u32) -> u32 {
    x * 2
}

fn triple(x: u32) -> u32 {
    x * 3
}

fn pick(flag: bool) -> fn(u32) -> u32 {
    let f = if flag { double } else { triple };
    f
}

fn main() {
    assert!(pick(true)(5) == 10);
    assert!(pick(false)(5) == 15);
}
