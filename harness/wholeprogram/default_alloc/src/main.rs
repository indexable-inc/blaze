use std::collections::BTreeMap;

fn main() {
    let mut words: BTreeMap<String, usize> = BTreeMap::new();
    for word in "the quick brown fox jumps over the lazy dog and the end".split(' ') {
        *words.entry(word.to_string()).or_default() += 1;
    }
    println!("{words:?}");
    eprintln!("{} words", words.len());
    std::process::exit(words["the"] as i32);
}
