//@ expect: accept
struct D(u32);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn shape(items: &[u32]) -> String {
    match items {
        [] => "empty".into(),
        [one] => format!("one {one}"),
        [first, .., 0] => format!("ends in zero after {first}"),
        [a, b] => format!("two {a} {b}"),
        [first, rest @ ..] if rest.len() > 3 => format!("long from {first}"),
        [.., x, y] => format!("ends {x} {y}"),
    }
}

fn owned(items: Vec<D>) -> u32 {
    match items.as_slice() {
        [D(a), .., D(b)] if a < b => a + b,
        [D(only)] => *only,
        _ => 0,
    }
}

fn arrays(pair: [D; 3]) -> u32 {
    let [first, _, last] = pair;
    first.0 * 10 + last.0
}

fn main() {
    let cases: [&[u32]; 6] = [&[], &[1], &[4, 0], &[2, 3], &[1, 2, 3, 4, 5], &[7, 8, 9]];
    for items in cases {
        println!("{}", shape(items));
    }
    println!("{}", owned(vec![D(1), D(2), D(3)]));
    println!("{}", owned(vec![D(9)]));
    println!("{}", arrays([D(4), D(5), D(6)]));
}
