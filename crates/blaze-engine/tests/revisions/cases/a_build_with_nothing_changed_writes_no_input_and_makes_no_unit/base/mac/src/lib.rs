#![feature(proc_macro_span)]

use proc_macro::{TokenStream, TokenTree};

/// Gives the struct `count()`, the number of its token trees, and asks where they are: the
/// join of its name's span and its last token's (of two items when a `macro_rules!` made the
/// struct: its name is the call's, the rest the macro's) and the line of the last.
#[proc_macro_derive(Count)]
pub fn count(input: TokenStream) -> TokenStream {
    let trees: Vec<TokenTree> = input.into_iter().collect();
    let name = trees
        .iter()
        .skip_while(|it| it.to_string() != "struct")
        .nth(1)
        .expect("a struct's name");
    let last = trees.last().expect("a token").span();
    let _joined = name.span().join(last);
    let _line = last.line();
    format!("impl {name} {{ pub fn count() -> u32 {{ {} }} }}", trees.len()).parse().unwrap()
}
