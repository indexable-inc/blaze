//@ expect: accept
#[allow(unused_macros)]
macro_rules! never_expanded {
    () => {
        let x: u8 = "this is only tokens";
        x.no_such_method()
    };
}

fn main() {}
