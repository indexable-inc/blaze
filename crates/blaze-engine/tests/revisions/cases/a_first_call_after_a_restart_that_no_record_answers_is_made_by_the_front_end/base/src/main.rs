// Called only when the program is given an argument: no run before the restart calls it, so
// no start image holds it, and a workspace function has no bind record.
fn second(word: &str) -> String {
    format!("and {word}")
}

fn main() {
    println!("hello");
    if let Some(word) = std::env::args().nth(1) {
        println!("{}", second(&word));
    }
}
