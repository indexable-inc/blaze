#![no_std]

extern crate proc_macro;

use proc_macro::TokenStream;

/// `answer!()` is the literal `2021`.
#[proc_macro]
pub fn answer(_: TokenStream) -> TokenStream {
    "2021".parse().unwrap()
}
