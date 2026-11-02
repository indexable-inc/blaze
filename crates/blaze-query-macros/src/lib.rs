//! The front end's query attributes over `blaze_query::db`: `#[tracked]` on free functions, on
//! the functions of a tracked `impl` and on structs, `#[input]`, `#[interned]`,
//! `#[derive(Supertype)]` and `#[derive(Value)]`. Generated code names `::blaze_query::db::plumbing` and the public
//! `::blaze_query::db` items only, as docs/v2/plan/lanes/db/query-unify.md ("The contract")
//! fixes them. Every option the front end doesn't use is refused with an error naming it.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;

mod fields;
mod hygiene;
mod input;
mod interned;
mod lifetimes;
mod options;
mod query;
mod returns;
mod structs;
mod supertype;
mod tracked_fn;
mod tracked_impl;
mod tracked_struct;
mod value;

#[cfg(test)]
mod tests;

/// `#[blaze_query::tracked(..)]` on a function (a query), on an `impl` whose functions carry
/// their own `#[blaze_query::tracked(..)]`, or on a struct (a tracked struct).
#[proc_macro_attribute]
pub fn tracked(args: TokenStream, item: TokenStream) -> TokenStream {
    tracked_tokens(args.into(), item.into()).into()
}

/// `#[blaze_query::input(singleton, debug)]` on a struct with named fields.
#[proc_macro_attribute]
pub fn input(args: TokenStream, item: TokenStream) -> TokenStream {
    input_tokens(args.into(), item.into()).into()
}

/// `#[blaze_query::interned(debug, constructor = name, unsafe(no_lifetime))]` on
/// a struct with named fields.
#[proc_macro_attribute]
pub fn interned(args: TokenStream, item: TokenStream) -> TokenStream {
    interned_tokens(args.into(), item.into()).into()
}

/// `AsId` and `FromIdIn` for an enum whose every variant holds one ID type.
#[proc_macro_derive(Supertype)]
pub fn supertype(item: TokenStream) -> TokenStream {
    derive_tokens(item.into(), supertype::expand).into()
}

/// `blaze_query::Value` for a type whose fields are values, checked field by field. A field
/// whose retention a type's own invariants prove says so with `#[value(unsafe(prove(P, ..)))]`
/// or `#[value(unsafe(prove_safe_to_retain_manually))]`.
#[proc_macro_derive(Value, attributes(value))]
pub fn value(item: TokenStream) -> TokenStream {
    derive_tokens(item.into(), value::expand).into()
}

pub(crate) fn tracked_tokens(args: TokenStream2, item: TokenStream2) -> TokenStream2 {
    // clone: expansion time, once per annotated item; no query runs this
    match syn::parse2::<syn::Item>(item.clone()) {
        Ok(syn::Item::Fn(it)) => {
            tracked_fn::expand(args, &it).unwrap_or_else(|error| with_error(item, error))
        }
        Ok(syn::Item::Impl(it)) => tracked_impl::expand(args, &it)
            .unwrap_or_else(|(error, stripped)| with_error(stripped, error)),
        Ok(syn::Item::Struct(it)) => tracked_struct::expand(args, &it)
            .unwrap_or_else(|(error, stripped)| with_error(stripped, error)),
        Ok(other) => syn::Error::new_spanned(
            other,
            "`#[blaze_query::tracked]` goes on a function, an `impl` or a struct",
        )
        .into_compile_error(),
        Err(error) => error.into_compile_error(),
    }
}

pub(crate) fn input_tokens(args: TokenStream2, item: TokenStream2) -> TokenStream2 {
    match syn::parse2::<syn::ItemStruct>(item) {
        Ok(it) => {
            input::expand(args, &it).unwrap_or_else(|(error, stripped)| with_error(stripped, error))
        }
        Err(error) => error.into_compile_error(),
    }
}

pub(crate) fn interned_tokens(args: TokenStream2, item: TokenStream2) -> TokenStream2 {
    match syn::parse2::<syn::ItemStruct>(item) {
        Ok(it) => interned::expand(args, &it)
            .unwrap_or_else(|(error, stripped)| with_error(stripped, error)),
        Err(error) => error.into_compile_error(),
    }
}

fn derive_tokens(
    item: TokenStream2,
    expand: fn(&syn::DeriveInput) -> syn::Result<TokenStream2>,
) -> TokenStream2 {
    syn::parse2::<syn::DeriveInput>(item)
        .and_then(|input| expand(&input))
        .unwrap_or_else(syn::Error::into_compile_error)
}

/// The item as rustc can take it without the attribute (so its other uses report no errors of
/// their own), and the error.
fn with_error(mut item: TokenStream2, error: syn::Error) -> TokenStream2 {
    item.extend(error.into_compile_error());
    item
}

/// The path every generated item reaches the engine by.
pub(crate) fn plumbing() -> TokenStream2 {
    quote::quote!(::blaze_query::db::plumbing)
}

/// The public database items: `Id`, `AsId`, `Database`, `Durability`, ..
pub(crate) fn db() -> TokenStream2 {
    quote::quote!(::blaze_query::db)
}
