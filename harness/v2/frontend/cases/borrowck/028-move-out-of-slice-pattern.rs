//@ expect: reject E0508
fn first(v: &[String]) -> usize {
    if let [a, ..] = *v { //~ ERROR E0508
        a.len()
    } else {
        0
    }
}

fn main() {
    println!("{}", first(&[String::from("s")]));
}
