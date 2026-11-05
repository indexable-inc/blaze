//@ expect: accept
//@ edition: 2024
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

fn tail() -> usize {
    let _local = D("local");
    D("temporary").len()
}

fn main() {
    let n = tail();
    println!("{n}");
    let m = {
        let _inner = D("inner");
        D("block tail").len()
    };
    println!("{m}");
}
