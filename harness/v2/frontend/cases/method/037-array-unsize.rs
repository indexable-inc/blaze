//@ expect: accept
//@ edition: 2021
// An array receiver reaches slice methods through the unsize step.
fn main() {
    let a = [3, 1, 2];
    let mut b = a;
    b.sort();
    println!("{} {:?} {:?}", a.len(), b, a.iter().max());
}
