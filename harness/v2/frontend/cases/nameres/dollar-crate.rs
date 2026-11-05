//@ expect: accept
//@ edition: 2021
pub mod util {
    pub fn helper() -> u32 {
        42
    }
}

macro_rules! call {
    () => {
        $crate::util::helper()
    };
}

mod inner {
    mod util {
        pub fn helper() -> u32 {
            0
        }
    }
    pub fn go() -> u32 {
        let _ = util::helper();
        call!()
    }
}

fn main() {
    println!("{}", inner::go());
}
