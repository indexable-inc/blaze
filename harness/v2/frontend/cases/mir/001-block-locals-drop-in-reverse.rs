//@ expect: accept
struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn main() {
    let _a = D("a");
    let _b = D("b");
    {
        let _c = D("c");
        let _d = D("d");
        println!("inner end");
    }
    let _e = D("e");
    println!("outer end");
}
