//@ expect: reject E0521
fn spawn_with(x: &i32) -> i32 {
    let h = std::thread::spawn(move || x + 1); //~ ERROR E0521
    h.join().unwrap()
}

fn main() {
    println!("{}", spawn_with(&1));
}
