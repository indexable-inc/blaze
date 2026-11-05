//@ expect: accept
struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

enum E {
    Two(D, D),
    One(D),
}

fn main() {
    match (D("x"), D("y"), D("z")) {
        (a, _, c) => {
            println!("arm {} {}", a.0, c.0);
        }
    }
    println!("after the tuple");
    for e in [E::Two(D("p"), D("q")), E::One(D("r"))] {
        match e {
            E::Two(first, second) => println!("two {} {}", first.0, second.0),
            E::One(ref only) => println!("one {}", only.0),
        }
        println!("after an arm");
    }
}
