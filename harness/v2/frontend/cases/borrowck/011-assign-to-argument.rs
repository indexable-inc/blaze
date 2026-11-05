//@ expect: reject E0384
fn bump(x: i32) -> i32 {
    x = x + 1; //~ ERROR E0384
    x
}

fn main() {
    println!("{}", bump(1));
}
