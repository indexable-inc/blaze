//@ expect: reject E0382
fn main() {
    let b = Box::new((String::from("a"), String::from("b")));
    let a = b.0;
    let whole = b; //~ ERROR E0382
    println!("{a} {}", whole.1);
}
