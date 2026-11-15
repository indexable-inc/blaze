//! blaze-memo's attributes, with blaze-query's call shapes: `#[tracked]` on free functions and
//! on the functions of a tracked `impl`, `#[input]`, `#[interned]` and `#[derive(Home)]`.
//! Generated code names `::blaze_memo::plumbing` only. blaze-query's options and attributes the
//! memo has no meaning for (`lru`, `weigh`, `no_eq`, `scope`, `settled`, `mem_tag`, `#[set]`,
//! `#[default]`, tracked structs, `Value`, `Supertype`) are refused with what replaces them.

#![forbid(unsafe_code)]
#![feature(allocator_api)]

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;

mod fields;
mod home;
mod hygiene;
mod input;
mod interned;
mod lifetimes;
mod options;
mod query;
mod returns;
mod structs;
mod tracked_fn;
mod tracked_impl;

/// `#[blaze_memo::tracked(..)]` on a function (a query), or on an `impl` whose functions carry
/// their own `#[blaze_memo::tracked(..)]`.
#[proc_macro_attribute]
pub fn tracked(args: TokenStream, item: TokenStream) -> TokenStream {
    tracked_tokens(args.into(), item.into()).into()
}

/// `#[blaze_memo::input(singleton, debug)]` on a struct with named fields.
#[proc_macro_attribute]
pub fn input(args: TokenStream, item: TokenStream) -> TokenStream {
    input_tokens(args.into(), item.into()).into()
}

/// `#[blaze_memo::interned(debug, constructor = name, unsafe(no_lifetime))]` on a struct with
/// named fields.
#[proc_macro_attribute]
pub fn interned(args: TokenStream, item: TokenStream) -> TokenStream {
    interned_tokens(args.into(), item.into()).into()
}

/// `blaze_memo::Home` for a struct or enum: the greatest arena its fields name; a field marked
/// `#[home(skip)]` is not looked at.
#[proc_macro_derive(Home, attributes(home))]
pub fn home(item: TokenStream) -> TokenStream {
    syn::parse2::<syn::DeriveInput>(item.into())
        .and_then(|input| home::expand(&input))
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn tracked_tokens(args: TokenStream2, item: TokenStream2) -> TokenStream2 {
    // clone: expansion time, once per annotated item; no query runs this
    match syn::parse2::<syn::Item>(item.clone()) {
        Ok(syn::Item::Fn(it)) => {
            tracked_fn::expand(args, &it).unwrap_or_else(|error| with_error(item, error))
        }
        Ok(syn::Item::Impl(it)) => tracked_impl::expand(args, &it)
            .unwrap_or_else(|(error, stripped)| with_error(stripped, error)),
        Ok(syn::Item::Struct(it)) => syn::Error::new_spanned(
            &it.ident,
            "tracked structs are gone: make it `#[blaze_memo::interned]` (its id is the same \
             for equal fields in one snapshot)",
        )
        .into_compile_error(),
        Ok(other) => syn::Error::new_spanned(
            other,
            "`#[blaze_memo::tracked]` goes on a function or an `impl`",
        )
        .into_compile_error(),
        Err(error) => error.into_compile_error(),
    }
}

fn input_tokens(args: TokenStream2, item: TokenStream2) -> TokenStream2 {
    match syn::parse2::<syn::ItemStruct>(item) {
        Ok(it) => {
            input::expand(args, &it).unwrap_or_else(|(error, stripped)| with_error(stripped, error))
        }
        Err(error) => error.into_compile_error(),
    }
}

fn interned_tokens(args: TokenStream2, item: TokenStream2) -> TokenStream2 {
    match syn::parse2::<syn::ItemStruct>(item) {
        Ok(it) => interned::expand(args, &it)
            .unwrap_or_else(|(error, stripped)| with_error(stripped, error)),
        Err(error) => error.into_compile_error(),
    }
}

/// The item as rustc can take it without the attribute (so its other uses report no errors of
/// their own), and the error.
fn with_error(mut item: TokenStream2, error: syn::Error) -> TokenStream2 {
    item.extend(error.into_compile_error());
    item
}

/// The path every generated item reaches the memo by.
pub(crate) fn plumbing() -> TokenStream2 {
    quote::quote!(::blaze_memo::plumbing)
}
