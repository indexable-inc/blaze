//@ expect: accept
fn main() {
    let mut x = 1;
    let mut s = String::from("unused mut");
    println!("{x} {}", s.len());
}
