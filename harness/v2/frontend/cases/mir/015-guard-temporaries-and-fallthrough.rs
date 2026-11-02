//@ expect: accept
struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn check(d: &D, n: u32) -> bool {
    println!("check {} {n}", d.0);
    n > 2
}

fn classify(value: Option<u32>) -> &'static str {
    match value {
        Some(n) if check(&D("guard one"), n) => "big",
        Some(n) if n == 0 && check(&D("guard two"), n) => "zero",
        Some(_) => "small",
        None => "none",
    }
}

fn main() {
    for v in [Some(5), Some(0), Some(1), None] {
        println!("{}", classify(v));
    }
}
