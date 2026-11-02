//@ expect: accept
//@ edition: 2021
// Prelude traits are in scope: `ToString`, `Clone`, `Iterator`.
fn main() {
    let s = 5.to_string();
    let t = s.clone();
    let n = [1, 2, 3].iter().map(|x| x * 2).sum::<i32>();
    println!("{} {} {}", s, t, n);
}
