use proc_macro::TokenStream;

#[proc_macro]
pub fn make(_: TokenStream) -> TokenStream {
    r#"{
        struct Source;
        impl crate::Select for &Source {
            type Output = u8;
            fn selected(&self) -> u8 { 37 }
        }
        impl crate::Select for Source {
            type Output = ();
            fn selected(&self) {}
        }
        let value: u8 = (&&Source).selected();
        value
    }"#
    .parse()
    .unwrap()
}

#[proc_macro]
pub fn attach(_: TokenStream) -> TokenStream {
    "impl crate::Flag for crate::Global { fn flag(&self) -> u8 { 91 } }".parse().unwrap()
}
