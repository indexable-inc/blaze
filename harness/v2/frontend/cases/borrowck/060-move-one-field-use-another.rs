//@ expect: accept
struct Pair {
    a: String,
    b: String,
    c: u32,
}

fn main() {
    let p = Pair { a: "a".to_string(), b: "bb".to_string(), c: 3 };
    let a = p.a;
    println!("{} {} {}", a, p.b, p.c);
    let q = Pair { a: String::from("q"), ..p };
    println!("{} {} {}", q.a, q.b, p.c);
    let t = (String::from("x"), String::from("y"));
    let x = t.0;
    let y = &t.1;
    println!("{x} {y}");
}
