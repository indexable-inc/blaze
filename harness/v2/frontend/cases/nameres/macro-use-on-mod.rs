//@ expect: accept
//@ edition: 2021
#[macro_use]
mod m {
    macro_rules! five {
        () => {
            5
        };
    }
}

fn main() {
    println!("{}", five!());
}
