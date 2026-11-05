use proc_macro::TokenStream;

/// The words it is given as one string, in capitals.
#[proc_macro]
pub fn shout(input: TokenStream) -> TokenStream {
    format!("{:?}", input.to_string().to_uppercase()).parse().unwrap()
}
