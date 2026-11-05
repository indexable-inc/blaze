//@ expect: accept
struct P {
    a: u32,
    b: u32,
    name: String,
}

fn main() {
    let mut p = P { a: 1, b: 2, name: String::from("p") };
    let ra = &mut p.a;
    let rb = &mut p.b;
    let rn = &p.name;
    *ra += *rb;
    *rb += 1;
    let n = rn.len();
    p.name.push('!');
    println!("{} {} {} {}", p.a, p.b, p.name, n);
    let mut t = (1, 2);
    let (x, y) = (&mut t.0, &mut t.1);
    std::mem::swap(x, y);
    println!("{:?}", t);
}
