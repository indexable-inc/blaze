//@ expect: reject E0594
struct P {
    a: i32,
}

fn main() {
    let p = P { a: 0 };
    p.a = 2; //~ ERROR E0594
    println!("{}", p.a);
}
