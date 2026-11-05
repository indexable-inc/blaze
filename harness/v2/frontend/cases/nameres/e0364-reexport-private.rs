//@ expect: reject E0364
//@ edition: 2021
#![crate_type = "lib"]

pub mod m {
    fn f() {}
    pub use self::f as g;
}
