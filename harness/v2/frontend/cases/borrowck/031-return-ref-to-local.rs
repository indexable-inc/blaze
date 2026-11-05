//@ expect: reject E0515
fn pick(_a: &i32) -> &i32 {
    let x = 5;
    &x //~ ERROR E0515
}

fn main() {
    println!("{}", pick(&1));
}
