//@ expect: reject E0506
struct P {
    a: u32,
    b: u32,
}

fn main() {
    let mut p = P { a: 1, b: 2 };
    let r = &p.a;
    p.a = 5; //~ ERROR E0506
    println!("{} {}", r, p.b);
}
