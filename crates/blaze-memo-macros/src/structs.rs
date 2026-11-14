//! What the input and interned structs share: the id struct itself and its `Id` and `Home`
//! impls.

use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use syn::{Ident, ItemStruct, Lifetime};

use crate::plumbing;

/// The item as written, without the field attributes the macro reads: what is given back with
/// an error.
pub(crate) fn stripped(item: &ItemStruct) -> TokenStream {
    const HELPERS: &[&str] = &["returns", "get", "set", "default", "tracked", "no_eq", "value"];
    // clone: expansion time, the item given back with an error
    let mut item = item.clone();
    for field in &mut item.fields {
        field.attrs.retain(|attr| !HELPERS.iter().any(|it| attr.path().is_ident(it)));
    }
    item.into_token_stream()
}

/// The struct the user's becomes: a `Gid`, and a lifetime when it has one.
pub(crate) fn declaration(item: &ItemStruct, lifetime: Option<&Lifetime>) -> TokenStream {
    let plumbing = plumbing();
    let ItemStruct { attrs, vis, ident, .. } = item;
    match lifetime {
        Some(lifetime) => quote! {
            #(#attrs)*
            #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
            #vis struct #ident<#lifetime>(#plumbing::Gid, ::core::marker::PhantomData<&#lifetime ()>);
        },
        None => quote! {
            #(#attrs)*
            #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
            #vis struct #ident(#plumbing::Gid);
        },
    }
}

/// `Self(gid)` for the struct.
pub(crate) fn construct(lifetime: Option<&Lifetime>, gid: &TokenStream) -> TokenStream {
    match lifetime {
        Some(_) => quote!(Self(#gid, ::core::marker::PhantomData)),
        None => quote!(Self(#gid)),
    }
}

/// `Id` and `Home`: the struct is an id, homed in its arena.
pub(crate) fn id_impls(ident: &Ident, lifetime: Option<&Lifetime>) -> TokenStream {
    let plumbing = plumbing();
    let (generics, ty) = match lifetime {
        Some(lifetime) => (quote!(<#lifetime>), quote!(#ident<#lifetime>)),
        None => (TokenStream::new(), quote!(#ident)),
    };
    let make = construct(lifetime, &quote!(gid));
    quote! {
        impl #generics #plumbing::Id for #ty {
            #[inline]
            fn gid(self) -> #plumbing::Gid {
                self.0
            }
            #[inline]
            fn from_gid(gid: #plumbing::Gid) -> Self {
                #make
            }
        }

        impl #generics #plumbing::Home for #ty {
            #[inline]
            fn home(&self, homes: &mut #plumbing::Homes<'_>) {
                #plumbing::Home::home(&self.0, homes);
            }
        }
    }
}
