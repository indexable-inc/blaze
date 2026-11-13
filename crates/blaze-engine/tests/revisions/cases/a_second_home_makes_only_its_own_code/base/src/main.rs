fn shout(word: &str) -> String {
    word.to_uppercase()
}

fn main() {
    println!("{}", shout("second"));
}
