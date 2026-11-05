//@ expect: accept
fn bucket(n: i32) -> &'static str {
    match n {
        i32::MIN..=-100 => "very negative",
        -99..=-1 => "negative",
        0 => "zero",
        1..=10 => "small",
        5 => "five, after small",
        11 | 13 | 17 | 19 => "odd prime-ish",
        10..=20 => "teens",
        42 => "answer",
        21..=50 if n % 2 == 0 => "even middle",
        21..=50 => "odd middle",
        100 => "hundred",
        _ => "large",
    }
}

fn kind(c: char) -> u8 {
    match c {
        'a' | 'e' | 'i' | 'o' | 'u' => 1,
        'a'..='z' => 2,
        'A'..='Z' => 3,
        '0'..='9' => 4,
        '\u{80}'.. => 5,
        _ => 6,
    }
}

fn main() {
    for n in [i32::MIN, -100, -99, -1, 0, 1, 5, 10, 11, 12, 13, 20, 21, 22, 42, 50, 51, 100, 1000] {
        println!("{n} {}", bucket(n));
    }
    for c in ['a', 'b', 'u', 'Z', '7', 'é', ' '] {
        println!("{c:?} {}", kind(c));
    }
}
