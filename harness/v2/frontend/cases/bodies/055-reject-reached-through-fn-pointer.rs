//@ expect: reject E0308
fn broken(x: u32) -> u32 {
    x > 1 //~ ERROR E0308
}

fn main() {
    let f: fn(u32) -> u32 = broken;
    println!("{}", f(1));
}
