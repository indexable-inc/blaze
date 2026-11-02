//@ expect: accept
struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

enum E {
    A(D, u8),
    B(u8, D),
    C,
}

fn name(e: E) -> String {
    match e {
        E::A(d, n) | E::B(n, d) if n > 1 => format!("{} {n}", d.0),
        E::A(_, n) | E::B(n, _) => format!("low {n}"),
        E::C => "c".to_string(),
    }
}

fn main() {
    println!("{}", name(E::A(D("a"), 2)));
    println!("{}", name(E::B(3, D("b"))));
    println!("{}", name(E::A(D("a low"), 0)));
    println!("{}", name(E::B(1, D("b low"))));
    println!("{}", name(E::C));
    let pairs = [(1, 2), (2, 1), (3, 3), (4, 0)];
    for pair in pairs {
        match pair {
            (1, x) | (x, 1) => println!("one and {x}"),
            (x, y) | (y, x) if x == 0 || y == 0 => println!("zero pair"),
            (x, y) => println!("{x} {y}"),
        }
    }
}
