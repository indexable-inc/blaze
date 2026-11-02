//! `#[derive(Supertype)]`: an enum whose every variant holds one ID type is itself an ID, its
//! variant found by which ingredient owns the ID's page.

use proc_macro2::TokenStream;
use quote::quote;

use crate::db;

pub(crate) fn expand(input: &syn::DeriveInput) -> syn::Result<TokenStream> {
    let syn::Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`derive(Supertype)` goes on an enum whose variants each hold one ID",
        ));
    };
    if data.variants.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "a `Supertype` has at least one variant",
        ));
    }
    let db = db();
    let ident = &input.ident;
    let mut as_id = TokenStream::new();
    let mut from_id = TokenStream::new();
    for variant in &data.variants {
        let name = &variant.ident;
        let ty = match &variant.fields {
            syn::Fields::Unnamed(fields) if fields.unnamed.len() == 1 => &fields.unnamed[0].ty,
            _ => {
                return Err(syn::Error::new_spanned(
                    variant,
                    "each variant of a `Supertype` holds one ID: `Variant(Id)`",
                ));
            }
        };
        as_id.extend(quote!(#ident::#name(it) => #db::AsId::as_id(it),));
        from_id.extend(quote! {
            if let ::core::option::Option::Some(it) =
                <#ty as #db::FromIdIn>::from_id_in(storage, id)
            {
                return ::core::option::Option::Some(#ident::#name(it));
            }
        });
    }
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();
    Ok(quote! {
        impl #impl_generics #db::AsId for #ident #type_generics #where_clause {
            #[inline]
            fn as_id(&self) -> #db::Id {
                match self {
                    #as_id
                }
            }
        }

        impl #impl_generics #db::FromIdIn for #ident #type_generics #where_clause {
            #[inline]
            fn from_id_in(storage: &#db::Storage, id: #db::Id) -> ::core::option::Option<Self> {
                #from_id
                ::core::option::Option::None
            }
        }
    })
}
