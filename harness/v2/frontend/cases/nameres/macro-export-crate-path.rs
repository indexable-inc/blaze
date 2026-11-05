//@ expect: accept
//@ edition: 2021
mod m {
    #[macro_export]
    macro_rules! exported {
        () => {
            6
        };
    }
}

mod user {
    pub fn get() -> u8 {
        crate::exported!()
    }
}

fn main() {
    println!("{} {}", crate::exported!(), user::get());
}
