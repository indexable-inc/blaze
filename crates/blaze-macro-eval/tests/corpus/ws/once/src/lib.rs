//! The corpus's macro that emits a helper once, alone in its crate: a static flag, set by its
//! first call, and a helper item only while the flag is not set. Its first call changes the
//! fresh state and no call changes the state the first left, so blaze's verdict of it, learned
//! from what runs do to the state, is that it settles; yet what a call gives depends on whether
//! it ran first (`tests/kept.rs`).

extern crate proc_macro;

use std::sync::atomic::{AtomicBool, Ordering};

use proc_macro::{Ident, Literal, Punct, Spacing, Span, TokenStream, TokenTree};

static EMITTED: AtomicBool = AtomicBool::new(false);

/// `once!(NAME)` is `pub const NAME: u32 = 1;`, and, from the first call alone, `pub fn
/// once_helper() {}`.
#[proc_macro]
pub fn once(input: TokenStream) -> TokenStream {
    let name = match input.into_iter().collect::<Vec<_>>().as_slice() {
        [TokenTree::Ident(name)] => name.clone(),
        _ => panic!("once!: expected one identifier"),
    };
    let first = !EMITTED.swap(true, Ordering::Relaxed);
    let ident = |name: &str| TokenTree::Ident(Ident::new(name, Span::call_site()));
    let punct = |ch: char| TokenTree::Punct(Punct::new(ch, Spacing::Alone));
    let mut out = vec![
        ident("pub"),
        ident("const"),
        TokenTree::Ident(name),
        punct(':'),
        ident("u32"),
        punct('='),
        TokenTree::Literal(Literal::u32_unsuffixed(1)),
        punct(';'),
    ];
    if first {
        let body = |delimiter| TokenTree::Group(proc_macro::Group::new(delimiter, TokenStream::new()));
        out.extend([ident("pub"), ident("fn"), ident("once_helper"), body(proc_macro::Delimiter::Parenthesis), body(proc_macro::Delimiter::Brace)]);
    }
    out.into_iter().collect()
}
