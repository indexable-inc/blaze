//@ expect: accept
fn greet() -> &'static str {
    "hello"
}

fn part() -> &'static str {
    "world"
}

static TABLE: [fn() -> &'static str; 2] = [greet, part];

fn main() {
    for f in TABLE.iter() {
        println!("{}", f());
    }
}
