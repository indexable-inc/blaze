//! The corpus's own proc macros (`crates/blaze-macro-eval/tests/corpus.rs`): one of each kind,
//! one that panics, one that reads a file of the calling package, and one that reads the calling
//! crate's variables; the one that counts its calls is `corpus-state`'s.

extern crate proc_macro;

mod imp;

use proc_macro::TokenStream;

#[proc_macro]
pub fn make_fn(input: TokenStream) -> TokenStream {
    imp::make_fn(input)
}

#[proc_macro_derive(Describe, attributes(describe))]
pub fn describe(input: TokenStream) -> TokenStream {
    imp::describe(input)
}

#[proc_macro_attribute]
pub fn label(attr: TokenStream, item: TokenStream) -> TokenStream {
    imp::label(attr, item)
}

#[proc_macro]
pub fn boom(input: TokenStream) -> TokenStream {
    imp::boom(input)
}

#[proc_macro]
pub fn include_data(input: TokenStream) -> TokenStream {
    imp::include_data(input)
}

#[proc_macro]
pub fn env_name(input: TokenStream) -> TokenStream {
    imp::env_name(input)
}
