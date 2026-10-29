use proc_macro::TokenStream;

/// Whether getrandom filled a buffer while the macro expanded.
#[proc_macro]
pub fn random_filled(_: TokenStream) -> TokenStream {
    let mut bytes = [0u8; 8];
    let filled = getrandom::getrandom(&mut bytes).is_ok();
    TokenStream::from(proc_macro::TokenTree::Ident(proc_macro::Ident::new(
        if filled { "true" } else { "false" },
        proc_macro::Span::call_site(),
    )))
}
