//@ expect: reject E0507
fn take(o: &Option<String>) -> usize {
    match *o { //~ ERROR E0507
        Some(s) => s.len(),
        None => 0,
    }
}

fn main() {
    println!("{}", take(&Some(String::from("m"))));
}
