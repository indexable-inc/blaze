//@ expect: accept
fn main() {
    let mut a = 1u32;
    let b = 2u32;
    let flag = a < b;
    let r: &u32 = if flag { &mut a } else { &b };
    assert!(*r == 1);
}
