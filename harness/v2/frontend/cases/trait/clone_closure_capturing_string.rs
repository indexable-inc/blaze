//@ expect: accept
//@ edition: 2021
// A closure is Clone when its captures are: the clone has its own copy of the `String`.
fn main() {
    let s = String::from("ab");
    let c = move || s.len();
    let d = c.clone();
    println!("{} {}", c(), d());
}
