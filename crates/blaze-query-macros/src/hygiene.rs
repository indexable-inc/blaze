//! Names for generated items that no identifier of the user's item uses, so a generated
//! `Configuration` never shadows the user's own.

// alloc: expansion time, once per annotated item; no query runs this

use blaze_hash::HashSet;
use proc_macro2::{Span, TokenStream, TokenTree};
use quote::ToTokens;

pub(crate) struct Hygiene {
    used: HashSet<String>,
}

impl Hygiene {
    pub(crate) fn of(item: &impl ToTokens) -> Hygiene {
        let mut used = HashSet::default();
        collect(item.to_token_stream(), &mut used);
        Hygiene { used }
    }

    /// `base_`, with more `_` until no user identifier is it.
    pub(crate) fn ident(&self, base: &str) -> syn::Ident {
        // alloc: expansion time, once per generated name
        let mut name = format!("{base}_");
        while self.used.contains(&name) {
            name.push('_');
        }
        syn::Ident::new(&name, Span::call_site())
    }
}

fn collect(tokens: TokenStream, used: &mut HashSet<String>) {
    for token in tokens {
        match token {
            TokenTree::Group(group) => collect(group.stream(), used),
            TokenTree::Ident(ident) => {
                // alloc: expansion time, once per identifier of the item
                used.insert(ident.to_string());
            }
            TokenTree::Punct(_) | TokenTree::Literal(_) => {}
        }
    }
}
