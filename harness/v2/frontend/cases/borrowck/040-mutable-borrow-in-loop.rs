//@ expect: reject E0499
fn main() {
    let mut s = String::new();
    let mut refs: Vec<&mut String> = Vec::new();
    for _ in 0..2 {
        refs.push(&mut s); //~ ERROR E0499
    }
    println!("{}", refs.len());
}
