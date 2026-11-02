//@ expect: reject E0382
struct Pair {
    a: String,
    b: String,
}

fn main() {
    let p = Pair { a: String::from("a"), b: String::from("b") };
    let a = p.a;
    let q = p; //~ ERROR E0382
    println!("{} {}", a, q.b);
}
