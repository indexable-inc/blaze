//! The corpus's macro that keeps state in a static, alone in its crate: a crate one of whose
//! macros leaves state expands every macro of it in a session, which would keep the others'
//! expansions from being kept.

extern crate proc_macro;

use std::sync::atomic::{AtomicU32, Ordering};

use proc_macro::{Ident, Literal, Punct, Spacing, Span, TokenStream, TokenTree};

static CALLS: AtomicU32 = AtomicU32::new(0);

/// `counter!(NAME)` is `pub const NAME: u32 = n;`, `n` the number of calls of it before this
/// one in the compilation.
#[proc_macro]
pub fn counter(input: TokenStream) -> TokenStream {
    let name = match input.into_iter().collect::<Vec<_>>().as_slice() {
        [TokenTree::Ident(name)] => name.clone(),
        _ => panic!("counter!: expected one identifier"),
    };
    let calls = CALLS.fetch_add(1, Ordering::Relaxed);
    let ident = |name: &str| TokenTree::Ident(Ident::new(name, Span::call_site()));
    let punct = |ch: char| TokenTree::Punct(Punct::new(ch, Spacing::Alone));
    [
        ident("pub"),
        ident("const"),
        TokenTree::Ident(name),
        punct(':'),
        ident("u32"),
        punct('='),
        TokenTree::Literal(Literal::u32_unsuffixed(calls)),
        punct(';'),
    ]
    .into_iter()
    .collect()
}
