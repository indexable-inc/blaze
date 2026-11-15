//! `returns(..)`: what a query or a field getter hands out for the `&'db V` it reads.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Ident, Lifetime, Type};

use crate::plumbing;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Ref,
    Copy,
    Clone,
    Deref,
    AsRef,
    AsDeref,
}

impl Mode {
    pub(crate) fn parse(name: &Ident) -> syn::Result<Mode> {
        // alloc: expansion time, once per `returns`
        Ok(match name.to_string().as_str() {
            "ref" => Mode::Ref,
            "copy" => Mode::Copy,
            "clone" => Mode::Clone,
            "deref" => Mode::Deref,
            "as_ref" => Mode::AsRef,
            "as_deref" => Mode::AsDeref,
            _ => {
                return Err(syn::Error::new(
                    name.span(),
                    "`returns` is one of `ref`, `copy`, `clone`, `deref`, `as_ref`, `as_deref`",
                ));
            }
        })
    }

    /// The type handed out for a `ty` read as `&'db ty`.
    pub(crate) fn ty(self, db_lt: &Lifetime, ty: &Type) -> TokenStream {
        let plumbing = plumbing();
        match self {
            Mode::Ref => quote!(&#db_lt #ty),
            Mode::Copy | Mode::Clone => quote!(#ty),
            Mode::Deref => quote!(&#db_lt <#ty as ::core::ops::Deref>::Target),
            Mode::AsRef => quote!(<#ty as #plumbing::ReturnsAsRef>::AsRef<#db_lt>),
            Mode::AsDeref => quote!(<#ty as #plumbing::ReturnsAsDeref>::AsDeref<#db_lt>),
        }
    }

    /// What is handed out for `value: &'db V`.
    pub(crate) fn expr(self, value: &TokenStream) -> TokenStream {
        let plumbing = plumbing();
        match self {
            Mode::Ref => quote!(#value),
            Mode::Copy => quote!(*#value),
            Mode::Clone => quote!(::core::clone::Clone::clone(#value)),
            Mode::Deref => quote!(::core::ops::Deref::deref(#value)),
            Mode::AsRef => quote!(#plumbing::ReturnsAsRef::as_ref(#value)),
            Mode::AsDeref => quote!(#plumbing::ReturnsAsDeref::as_deref(#value)),
        }
    }
}
