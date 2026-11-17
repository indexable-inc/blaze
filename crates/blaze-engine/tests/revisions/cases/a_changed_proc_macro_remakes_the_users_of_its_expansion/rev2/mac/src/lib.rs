use proc_macro::TokenStream;

#[proc_macro]
pub fn answer(_input: TokenStream) -> TokenStream {
    "42".parse().unwrap()
}
