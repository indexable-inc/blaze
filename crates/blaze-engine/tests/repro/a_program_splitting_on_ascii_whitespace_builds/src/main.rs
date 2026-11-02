fn main() {
    let words: Vec<&str> = std::hint::black_box(" -O2  -g\t-Wall ").split_ascii_whitespace().collect();
    println!("{words:?}");
}
