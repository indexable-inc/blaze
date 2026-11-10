#![no_std]

extern crate proc_macro;
extern crate std;

use proc_macro::TokenStream;
use std::string::ToString;

/// `answer!()` is the literal `7`.
#[proc_macro]
pub fn answer(_: TokenStream) -> TokenStream {
    7.to_string().parse().unwrap()
}
