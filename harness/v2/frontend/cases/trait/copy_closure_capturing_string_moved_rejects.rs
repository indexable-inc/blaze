//@ expect: reject E0382
//@ edition: 2021
// A closure that captures a `String` by move is not Copy: using it after a move is E0382.
fn main() {
    let s = String::from("x");
    let c = move || s.len();
    let d = c;
    println!("{} {}", c(), d());
}
