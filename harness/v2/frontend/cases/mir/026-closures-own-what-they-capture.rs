//@ expect: accept
struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn call<F: FnOnce() -> usize>(f: F) -> usize {
    f()
}

fn main() {
    let a = D("by move");
    let b = D("by ref");
    let moving = move || a.0.len();
    let borrowing = || b.0.len();
    println!("{} {}", borrowing(), moving());
    drop(moving);
    println!("moving closure gone");
    let c = D("consumed");
    let n = call(move || {
        let inner = c;
        inner.0.len()
    });
    println!("{n}");
}
