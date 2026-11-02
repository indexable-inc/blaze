//@ expect: accept
fn keyword(word: &str) -> u32 {
    match word {
        "as" => 1,
        "break" => 2,
        "const" => 3,
        "continue" => 4,
        "crate" => 5,
        "else" => 6,
        "enum" => 7,
        "extern" => 8,
        "false" => 9,
        "fn" => 10,
        "for" => 11,
        "if" => 12,
        "impl" => 13,
        "in" => 14,
        "let" => 15,
        "loop" => 16,
        "match" => 17,
        "mod" => 18,
        "move" => 19,
        "mut" => 20,
        "pub" => 21,
        "ref" => 22,
        "return" => 23,
        "self" => 24,
        "static" => 25,
        "struct" => 26,
        "super" => 27,
        "trait" => 28,
        "true" => 29,
        "type" => 30,
        "unsafe" => 31,
        "use" => 32,
        "where" => 33,
        "while" => 34,
        "fn" => 99,
        w if w.starts_with('r') => 100,
        _ => 0,
    }
}

fn main() {
    for w in ["as", "while", "fn", "type", "raw", "rust", "ref", "nothing", ""] {
        println!("{w:?} {}", keyword(w));
    }
}
