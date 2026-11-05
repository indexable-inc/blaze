//@ expect: reject E0594
struct P {
    a: i32,
}

fn set(p: &P) {
    p.a = 1; //~ ERROR E0594
}

fn main() {
    let p = P { a: 0 };
    set(&p);
    println!("{}", p.a);
}
