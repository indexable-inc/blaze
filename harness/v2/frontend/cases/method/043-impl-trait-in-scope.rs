//@ expect: accept
//@ edition: 2021
// The trait an impl implements is in scope in its methods, inside blocks too.
mod m {
    pub trait Hello {
        fn hello(&self) -> u8;
    }
}

struct S;

impl m::Hello for S {
    fn hello(&self) -> u8 {
        let inner = {
            struct Local;
            let _ = Local;
            1u8.hello()
        };
        inner + 1
    }
}

impl m::Hello for u8 {
    fn hello(&self) -> u8 {
        *self
    }
}

fn main() {
    println!("{}", m::Hello::hello(&S));
}
