//@ expect: accept
fn pick(flag: bool) -> u32 {
    let x: u32 = if flag { 1 } else { return 9 };
    x
}

fn main() {
    assert!(pick(true) == 1);
    assert!(pick(false) == 9);
}
