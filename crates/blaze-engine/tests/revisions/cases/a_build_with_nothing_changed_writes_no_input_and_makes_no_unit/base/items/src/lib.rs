macro_rules! item {
    ($name:ident) => {
        #[derive(mac::Count)]
        pub struct $name {
            pub a: u32,
        }
    };
}

item!(One);

pub fn count() -> u32 {
    One::count()
}
