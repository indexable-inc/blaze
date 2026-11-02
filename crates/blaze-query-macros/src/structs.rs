//! What the input, interned and tracked structs share: the ID struct itself, its ID traits and
//! its `Value` impl.

use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use syn::{Ident, ItemStruct, Lifetime};

use crate::{db, plumbing};

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

/// The struct the user's becomes: an `Id`, and a lifetime when it has one.
pub(crate) fn declaration(item: &ItemStruct, lifetime: Option<&Lifetime>) -> TokenStream {
    let db = db();
    let ItemStruct { attrs, vis, ident, .. } = item;
    match lifetime {
        Some(lifetime) => quote! {
            #(#attrs)*
            #[derive(Copy, Clone, PartialEq, Eq, Hash)]
            #vis struct #ident<#lifetime>(#db::Id, ::core::marker::PhantomData<&#lifetime ()>);
        },
        None => quote! {
            #(#attrs)*
            #[derive(Copy, Clone, PartialEq, Eq, Hash)]
            #vis struct #ident(#db::Id);
        },
    }
}

/// `Self(id)` for the struct.
pub(crate) fn construct(lifetime: Option<&Lifetime>, id: &TokenStream) -> TokenStream {
    match lifetime {
        Some(_) => quote!(Self(#id, ::core::marker::PhantomData)),
        None => quote!(Self(#id)),
    }
}

/// `AsId`, `FromId` and `FromIdIn` (by the owner of the ID's page) and `Value`: the struct is an
/// ID, which never points into the storage.
pub(crate) fn id_impls(
    ident: &Ident,
    lifetime: Option<&Lifetime>,
    ingredient: &Ident,
) -> TokenStream {
    let db = db();
    let plumbing = plumbing();
    let (generics, ty) = match lifetime {
        Some(lifetime) => (quote!(<#lifetime>), quote!(#ident<#lifetime>)),
        None => (TokenStream::new(), quote!(#ident)),
    };
    let make = construct(lifetime, &quote!(id));
    quote! {
        impl #generics #db::AsId for #ty {
            #[inline]
            fn as_id(&self) -> #db::Id {
                self.0
            }
        }

        #[doc(hidden)]
        impl #generics #db::FromId for #ty {
            #[inline]
            fn from_id(id: #db::Id) -> Self {
                #make
            }
        }

        impl #generics #db::FromIdIn for #ty {
            #[inline]
            fn from_id_in(storage: &#db::Storage, id: #db::Id) -> ::core::option::Option<Self> {
                if #plumbing::owner_is(storage, id, &#ingredient) {
                    ::core::option::Option::Some(#make)
                } else {
                    ::core::option::Option::None
                }
            }
        }

        // SAFETY: the struct is an `Id` (and a `PhantomData`), which never points into the
        // storage: a stored one is an index the storage checks when it is read.
        unsafe impl #generics #db::Value for #ty {}
    }
}

/// `Interned::ids_of` for a tuple of `count` fields: each field's id when it is `AsId`, what it
/// names when it is `NamesIds`, nothing otherwise, in field order.
pub(crate) fn ids_of(count: usize) -> TokenStream {
    let db = db();
    let plumbing = plumbing();
    let indices = (0..count).map(syn::Index::from);
    quote! {
        fn ids_of(fields: &Self::Fields, out: &mut ::std::vec::Vec<#db::Id>) {
            #[allow(unused_imports)]
            use #plumbing::{IdOfAsId as _, IdOfNamed as _, IdOfOther as _};
            let _ = (&fields, &out);
            #((&&&#plumbing::IdsOf(&fields.#indices)).push_ids(out);)*
        }
    }
}
