//@ expect: accept
//@ edition: 2021
// A closure that captures only Copy values by move is Copy.
fn main() {
    let n = 5;
    let add = move |x: i32| x + n;
    let add2 = add;
    let add3 = add;
    println!("{} {} {}", add(1), add2(2), add3(3));
}
