fn greeting() -> &'static str {
    "hello"
}

fn main() {
    let words: Vec<String> = (1..=3).map(|n| format!("{} {n}", greeting())).collect();
    println!("{}", words.join(", "));
    eprintln!("to standard error: {} words", words.len());
    std::process::exit(3);
}
