//@ expect: reject E0594
fn call<F: Fn()>(f: F) {
    f()
}

fn main() {
    let mut x = 0;
    call(|| x = 1); //~ ERROR E0594
    println!("{x}");
}
