//@ expect: reject E0507
fn call<F: Fn() -> usize>(f: F) -> usize {
    f()
}

fn main() {
    let s = String::from("c");
    let n = call(|| { let t = s; t.len() }); //~ ERROR E0507
    println!("{n}");
}
