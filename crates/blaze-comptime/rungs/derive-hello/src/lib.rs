//! `#[derive(Hello)]` on a struct or enum gives `impl Name { pub const HELLO: &str = "Name"; }`.
//!
//! Only `proc_macro`: no syn, no quote, no file, no variable, no clock. So every bridge method it
//! calls is one the two paths must answer identically, and its read list and facts are empty on
//! both sides. What it exercises: `TokenStream` iteration (`Group`, `Ident`, `Punct`), `Span`
//! (call site), `Literal::string`, `TokenStream::from_str` and `Display` of an `Ident`.

use proc_macro::{TokenStream, TokenTree};

#[proc_macro_derive(Hello)]
pub fn derive_hello(input: TokenStream) -> TokenStream {
    let mut tokens = input.into_iter();
    let mut name = None;
    while let Some(tree) = tokens.next() {
        if let TokenTree::Ident(ident) = &tree {
            let word = ident.to_string();
            if word == "struct" || word == "enum" {
                if let Some(TokenTree::Ident(found)) = tokens.next() {
                    name = Some(found.to_string());
                }
                break;
            }
        }
    }
    let Some(name) = name else {
        return "compile_error!(\"#[derive(Hello)] takes a struct or an enum\");".parse().unwrap();
    };
    let literal = proc_macro::Literal::string(&name);
    format!("impl {name} {{ pub const HELLO: &'static str = {literal}; }}").parse().unwrap()
}
