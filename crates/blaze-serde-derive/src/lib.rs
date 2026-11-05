//! `#[derive(Serialize)]` and `#[derive(Deserialize)]` for serde's traits, written against
//! `proc_macro` alone: [`parse`] reads the item from its token trees, [`ser`] and [`de`] write
//! the impls through [`out::Out`]. The impls are serde's usual shape (`serialize_struct`, a
//! visitor with `visit_seq` and `visit_map`, `EnumAccess`), so every serde format reads and
//! writes them; what they share lives in `blaze_serde::__private`, which they name as
//! `::blaze_serde`.
//!
//! The `#[serde(..)]` attributes are the ones blaze's crates use, with serde's meaning
//! ([`parse::Container`], [`parse::FieldAttrs`], [`parse::VariantAttrs`] list them). Any other
//! attribute, or a form of one this derive does not read, is a `compile_error!` naming the
//! attribute, the item and where it is, never a guess.
//!
//! A proc macro runs before any frame allocator exists, so its collections are the global
//! heap's: one item's worth, freed when the expansion ends.

use proc_macro::TokenStream;

mod de;
mod generics;
mod out;
mod parse;
mod ser;

#[proc_macro_derive(Serialize, attributes(serde))]
pub fn derive_serialize(input: TokenStream) -> TokenStream {
    match parse::item(input) {
        Ok(item) => ser::derive(&item).unwrap_or_else(|error| error.into_compile_error()),
        Err(error) => error.into_compile_error(),
    }
}

#[proc_macro_derive(Deserialize, attributes(serde))]
pub fn derive_deserialize(input: TokenStream) -> TokenStream {
    match parse::item(input) {
        Ok(item) => de::derive(&item).unwrap_or_else(|error| error.into_compile_error()),
        Err(error) => error.into_compile_error(),
    }
}
