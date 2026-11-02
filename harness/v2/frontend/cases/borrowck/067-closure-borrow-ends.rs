//@ expect: accept
fn main() {
    let mut x = 0;
    let mut inc = || x += 1;
    inc();
    inc();
    println!("{x}");
    let s = String::from("cap");
    let len = || s.len();
    let n = len();
    let t = s;
    println!("{n} {t}");
}
