//! The corpus's macro that settles as proc-macro2's one-time detection does, alone in its
//! crate: its first call sets a static flag, and what every call gives is the same whether the
//! flag was set (`tests/kept.rs`).

extern crate proc_macro;

use std::sync::atomic::{AtomicBool, Ordering};

use proc_macro::{Ident, Literal, Punct, Spacing, Span, TokenStream, TokenTree};

static DETECTED: AtomicBool = AtomicBool::new(false);

/// `detect!(NAME)` is `pub const NAME: u32 = 1;`, the flag set by the first call and read by
/// none.
#[proc_macro]
pub fn detect(input: TokenStream) -> TokenStream {
    let name = match input.into_iter().collect::<Vec<_>>().as_slice() {
        [TokenTree::Ident(name)] => name.clone(),
        _ => panic!("detect!: expected one identifier"),
    };
    if !DETECTED.load(Ordering::Relaxed) {
        DETECTED.store(true, Ordering::Relaxed);
    }
    let ident = |name: &str| TokenTree::Ident(Ident::new(name, Span::call_site()));
    let punct = |ch: char| TokenTree::Punct(Punct::new(ch, Spacing::Alone));
    [
        ident("pub"),
        ident("const"),
        TokenTree::Ident(name),
        punct(':'),
        ident("u32"),
        punct('='),
        TokenTree::Literal(Literal::u32_unsuffixed(1)),
        punct(';'),
    ]
    .into_iter()
    .collect()
}
