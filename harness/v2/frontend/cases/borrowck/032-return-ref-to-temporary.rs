//@ expect: reject E0515
fn make(_x: &u8) -> &String {
    &String::from("t") //~ ERROR E0515
}

fn main() {
    println!("{}", make(&0));
}
