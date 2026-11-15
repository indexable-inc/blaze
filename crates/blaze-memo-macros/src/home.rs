//! `#[derive(blaze_memo::Home)]`: a type's home is the greatest arena any of its fields names.
//! A field that holds no id (or whose ids must not decide the home) is `#[home(skip)]`.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields};

use crate::plumbing;

pub(crate) fn expand(input: &DeriveInput) -> syn::Result<TokenStream> {
    let plumbing = plumbing();
    let ident = &input.ident;
    // clone: expansion time, the type's generics with a bound added per type parameter
    let mut generics = input.generics.clone();
    for param in generics.type_params_mut() {
        param.bounds.push(syn::parse_quote!(#plumbing::Home));
    }
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let body = match &input.data {
        Data::Struct(data) => {
            let (pattern, visits) = fields(&data.fields)?;
            quote! {
                let Self #pattern = self;
                #(#visits)*
            }
        }
        Data::Enum(data) => {
            // alloc: expansion time, once per variant
            let mut arms = Vec::new();
            for variant in &data.variants {
                let name = &variant.ident;
                let (pattern, visits) = fields(&variant.fields)?;
                arms.push(quote!(Self::#name #pattern => { #(#visits)* }));
            }
            quote! {
                match self {
                    #(#arms)*
                    #[allow(unreachable_patterns)]
                    _ => {}
                }
            }
        }
        Data::Union(data) => {
            return Err(syn::Error::new_spanned(
                data.union_token,
                "a union's home can't be derived: implement `blaze_memo::Home` by hand",
            ));
        }
    };
    Ok(quote! {
        impl #impl_generics #plumbing::Home for #ident #ty_generics #where_clause {
            #[inline]
            #[allow(unused_variables)]
            fn home(&self, homes: &mut #plumbing::Homes<'_>) {
                #body
            }
        }
    })
}

/// The pattern binding a struct's or variant's fields and the visit of each not skipped.
fn fields(fields: &Fields) -> syn::Result<(TokenStream, Vec<TokenStream>)> {
    let plumbing = plumbing();
    // alloc: expansion time, once per field
    let mut binds = Vec::new();
    // alloc: expansion time, once per field
    let mut visits = Vec::new();
    for (index, field) in fields.iter().enumerate() {
        let bind = format_ident!("field_{index}");
        if !skipped(field)? {
            visits.push(quote!(#plumbing::Home::home(#bind, homes);));
        }
        binds.push((field.ident.as_ref(), bind));
    }
    let pattern = match fields {
        Fields::Named(_) => {
            let pairs = binds.iter().map(|(name, bind)| quote!(#name: #bind));
            quote!({ #(#pairs),* })
        }
        Fields::Unnamed(_) => {
            let binds = binds.iter().map(|(_, bind)| bind);
            quote!((#(#binds),*))
        }
        Fields::Unit => TokenStream::new(),
    };
    Ok((pattern, visits))
}

fn skipped(field: &syn::Field) -> syn::Result<bool> {
    let mut skip = false;
    for attr in &field.attrs {
        if !attr.path().is_ident("home") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip") {
                skip = true;
                Ok(())
            } else {
                Err(meta.error("the field attribute is `#[home(skip)]`"))
            }
        })?;
    }
    Ok(skip)
}
