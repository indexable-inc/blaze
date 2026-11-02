//@ expect: accept
struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

impl D {
    fn len(&self) -> usize {
        self.0.len()
    }
}

fn pair(a: &D, b: &D) -> usize {
    a.len() + b.len()
}

fn main() {
    let _kept = D("kept");
    let n = pair(&D("first"), &D("second"));
    println!("after the let: {n}");
    pair(&D("third"), &D("fourth"));
    println!("after the statement");
}
