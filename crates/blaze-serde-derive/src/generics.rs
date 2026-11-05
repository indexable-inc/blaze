//! An item's generics: its parameters as written (bounds kept, defaults dropped, as an impl
//! writes them) and its `where` predicates, and what a field's type says about them.

use proc_macro::{Delimiter, Ident, Spacing, TokenStream, TokenTree};

use crate::{
    out::Out,
    parse::{Error, at, split_commas},
};

#[derive(Default)]
pub(crate) struct Generics {
    pub(crate) params: Vec<Param>,
    pub(crate) predicates: Vec<TokenStream>,
}

pub(crate) enum Param {
    /// `'a: 'b`, named `'a`.
    Lifetime { name: String, decl: TokenStream },
    /// `T: Bound`, its default dropped.
    Type { ident: Ident, decl: TokenStream },
    /// `const N: usize`, its default dropped.
    Const { ident: Ident, decl: TokenStream },
}

/// The parameters between an item's `<` and `>`.
pub(crate) fn parse(item: &str, tokens: Vec<TokenTree>) -> Result<Generics, Error> {
    let mut params = Vec::new();
    for piece in split_commas(tokens) {
        let param = match piece.as_slice() {
            [TokenTree::Punct(quote), TokenTree::Ident(name), ..] if quote.as_char() == '\'' => {
                let name = format!("'{name}");
                if name == "'de" {
                    return Err(Error::new(
                        quote.span(),
                        format!(
                            "`{item}` ({}): a lifetime named `'de` is the one the derived `Deserialize<'de>` takes",
                            at(quote.span())
                        ),
                    ));
                }
                Param::Lifetime { name, decl: piece.iter().cloned().collect() }
            }
            [TokenTree::Ident(keyword), TokenTree::Ident(ident), ..]
                if keyword.to_string() == "const" =>
            {
                Param::Const { ident: ident.clone(), decl: before_default(&piece) }
            }
            [TokenTree::Ident(ident), ..] => {
                Param::Type { ident: ident.clone(), decl: before_default(&piece) }
            }
            _ => {
                let span = piece.first().map_or_else(proc_macro::Span::call_site, TokenTree::span);
                return Err(Error::new(
                    span,
                    format!(
                        "`{item}` ({}): a generic parameter blaze-serde-derive does not read",
                        at(span)
                    ),
                ));
            }
        };
        params.push(param);
    }
    Ok(Generics { params, predicates: Vec::new() })
}

/// `piece` up to its `=` (the default) outside `<..>`.
fn before_default(piece: &[TokenTree]) -> TokenStream {
    let mut depth = 0u32;
    let mut out = Vec::new();
    for tree in piece {
        if let TokenTree::Punct(punct) = tree {
            match punct.as_char() {
                '<' => depth += 1,
                '>' => depth = depth.saturating_sub(1),
                '=' if depth == 0 && punct.spacing() == Spacing::Alone => break,
                _ => {}
            }
        }
        out.push(tree.clone());
    }
    out.into_iter().collect()
}

impl Generics {
    /// The type parameters' names.
    pub(crate) fn type_params(&self) -> impl Iterator<Item = &Ident> {
        self.params.iter().filter_map(|it| match it {
            Param::Type { ident, .. } => Some(ident),
            _ => None,
        })
    }

    /// `<lead.., params..>` of an `impl`: `lead` first (`'de`), then the item's parameters.
    pub(crate) fn write_impl(&self, out: &mut Out, lead: &[&str]) {
        if lead.is_empty() && self.params.is_empty() {
            return;
        }
        out.s("<");
        for it in lead {
            out.s(it).s(",");
        }
        for param in &self.params {
            match param {
                Param::Lifetime { decl, .. }
                | Param::Type { decl, .. }
                | Param::Const { decl, .. } => {
                    out.tokens(decl).s(",");
                }
            }
        }
        out.s(">");
    }

    /// `<'a, T, N>`, the item's parameters as arguments.
    pub(crate) fn write_args(&self, out: &mut Out) {
        if self.params.is_empty() {
            return;
        }
        out.s("<");
        for param in &self.params {
            match param {
                Param::Lifetime { name, .. } => out.s(name).s(","),
                Param::Type { ident, .. } | Param::Const { ident, .. } => out.ident(ident).s(","),
            };
        }
        out.s(">");
    }

    /// `where` with the item's predicates and `extra`; nothing when both are empty.
    pub(crate) fn write_where(&self, out: &mut Out, extra: &[TokenStream]) {
        if self.predicates.is_empty() && extra.is_empty() {
            return;
        }
        out.s("where");
        for predicate in self.predicates.iter().chain(extra) {
            out.tokens(predicate).s(",");
        }
    }
}

/// The lifetimes `ty` names, in order, each once, less `'static` and `'_`.
pub(crate) fn lifetimes_in(ty: &TokenStream) -> Vec<String> {
    let mut out = Vec::new();
    collect_lifetimes(ty, &mut out);
    out
}

fn collect_lifetimes(ty: &TokenStream, out: &mut Vec<String>) {
    let mut quote = false;
    for tree in ty.clone() {
        match &tree {
            TokenTree::Punct(punct) if punct.as_char() == '\'' => {
                quote = true;
                continue;
            }
            TokenTree::Ident(ident) if quote => {
                let name = format!("'{ident}");
                if name != "'static" && name != "'_" && !out.contains(&name) {
                    out.push(name);
                }
            }
            TokenTree::Group(group) => collect_lifetimes(&group.stream(), out),
            _ => {}
        }
        quote = false;
    }
}

/// The lifetime of `ty` when it is `&'a str` or `&'a [u8]`, which serde borrows from the
/// input without `#[serde(borrow)]`.
pub(crate) fn implicit_borrow(ty: &TokenStream) -> Option<String> {
    let tokens: Vec<TokenTree> = ty.clone().into_iter().collect();
    let [TokenTree::Punct(amp), TokenTree::Punct(quote), TokenTree::Ident(lifetime), inner] =
        tokens.as_slice()
    else {
        return None;
    };
    if amp.as_char() != '&' || quote.as_char() != '\'' {
        return None;
    }
    let borrows = match inner {
        TokenTree::Ident(ident) => ident.to_string() == "str",
        TokenTree::Group(group) => {
            group.delimiter() == Delimiter::Bracket
                && group.stream().to_string().replace(' ', "") == "u8"
        }
        _ => false,
    };
    let name = format!("'{lifetime}");
    (borrows && name != "'static").then_some(name)
}

/// Whether `ty` names `ident` (as a path's first segment, not after `::`).
pub(crate) fn mentions(ty: &TokenStream, ident: &Ident) -> bool {
    let name = ident.to_string();
    let mut after_colons = false;
    let mut quote = false;
    for tree in ty.clone() {
        let found = match &tree {
            TokenTree::Ident(it) => !after_colons && !quote && it.to_string() == name,
            TokenTree::Group(group) => mentions(&group.stream(), ident),
            TokenTree::Punct(_) | TokenTree::Literal(_) => false,
        };
        if found {
            return true;
        }
        after_colons = matches!(&tree, TokenTree::Punct(it) if it.as_char() == ':');
        quote = matches!(&tree, TokenTree::Punct(it) if it.as_char() == '\'');
    }
    false
}
