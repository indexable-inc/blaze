//@ expect: accept
//@ edition: 2021
macro_rules! v {
    () => {
        1
    };
}

fn first() -> u8 {
    v!()
}

macro_rules! v {
    () => {
        2
    };
}

fn main() {
    println!("{} {}", first(), v!());
}
