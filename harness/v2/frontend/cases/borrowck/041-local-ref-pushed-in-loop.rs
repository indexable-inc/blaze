//@ expect: reject E0597
fn main() {
    let mut v: Vec<&u32> = Vec::new();
    for i in 0..3 {
        let x = i;
        v.push(&x); //~ ERROR E0597
    }
    println!("{}", v.len());
}
