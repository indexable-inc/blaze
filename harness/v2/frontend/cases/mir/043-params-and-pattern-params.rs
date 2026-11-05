//@ expect: accept
struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn params(_first: D, (a, _b): (D, D), _third: D) {
    println!("body with {}", a.0);
}

struct S(D);

impl S {
    fn method(self, _arg: D) {
        println!("method");
    }
}

fn main() {
    params(D("first"), (D("a"), D("b")), D("third"));
    S(D("self")).method(D("arg"));
    let closure = |_x: D, (_y, z): (D, D)| println!("closure {}", z.0);
    closure(D("x"), (D("y"), D("z")));
}
