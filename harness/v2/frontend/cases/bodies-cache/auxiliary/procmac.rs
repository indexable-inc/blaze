//@ edition: 2021
#![crate_type = "proc-macro"]
extern crate proc_macro;
use proc_macro::TokenStream;
#[proc_macro]
pub fn make_answer(_: TokenStream) -> TokenStream { "42u32".parse().unwrap() }
#[proc_macro_derive(Named)]
pub fn derive_named(input: TokenStream) -> TokenStream {
    let text = input.to_string();
    let name = text.split_whitespace().skip_while(|w| *w != "struct").nth(1).unwrap().trim_end_matches(';').to_string();
    format!("impl {name} {{ pub fn name() -> &'static str {{ \"{name}\" }} }}").parse().unwrap()
}
