use proc_macro::TokenStream;

/// `answer!()` is the literal `42`.
#[proc_macro]
pub fn answer(_: TokenStream) -> TokenStream {
    "42".parse().unwrap()
}

#[test]
fn the_lib_test_runs() {
    assert_eq!(2 + 2, 4);
}
