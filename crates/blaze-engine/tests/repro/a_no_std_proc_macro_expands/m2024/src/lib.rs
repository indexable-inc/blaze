#![no_std]

extern crate proc_macro;

use proc_macro::TokenStream;

/// The item as it is.
#[proc_macro_attribute]
pub fn pass(_: TokenStream, item: TokenStream) -> TokenStream {
    item
}
