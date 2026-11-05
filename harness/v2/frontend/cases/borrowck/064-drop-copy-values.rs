//@ expect: accept
fn main() {
    let x = 5u32;
    drop(x);
    let y = x + 1;
    let p = (1u8, 2u8);
    drop(p);
    let r = &y;
    drop(r);
    println!("{x} {y} {:?} {r}", p);
}
