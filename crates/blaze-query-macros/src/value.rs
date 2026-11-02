//! `blaze_query::Value`: the check that a type naming the database lifetime may be stored with
//! that lifetime erased, and `#[derive(Value)]`.

// alloc: expansion time, once per annotated item; no query runs this

use blaze_hash::HashSet;
use proc_macro2::TokenStream;
use quote::{quote, quote_spanned};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::visit_mut::VisitMut;
use syn::{Attribute, Lifetime, Type, WherePredicate};

use crate::lifetimes::{self, ReplaceSelf};

/// A function the expansion puts in its `const _` block: it names `assert_value::<T>()` for each
/// type that names `db_lt`, so a type that can't be stored with `db_lt` erased is a compile
/// error at the user's span. A `&'db T` is refused outright: it points into the database.
pub(crate) fn check(name: &syn::Ident, db_lt: &Lifetime, types: &[&Type]) -> TokenStream {
    // alloc: expansion time, once per query
    let unproven: Vec<(&Type, Option<&Attribute>)> = types.iter().map(|ty| (*ty, None)).collect();
    check_proven(name, db_lt, &unproven, None).unwrap_or_else(syn::Error::into_compile_error)
}

/// As [`check`], for fields that may carry `#[value(unsafe(..))]`: an unconditional proof
/// skips the field, a conditional one is checked by its predicates (with `Self` as `self_ty`).
pub(crate) fn check_proven(
    name: &syn::Ident,
    db_lt: &Lifetime,
    types: &[(&Type, Option<&Attribute>)],
    self_ty: Option<&Type>,
) -> syn::Result<TokenStream> {
    let db = crate::db();
    let mut asserts = TokenStream::new();
    for (ty, proof) in types {
        match proof.map(parse_proof).transpose()? {
            Some(Proof::Unconditional) => continue,
            Some(Proof::Conditional(predicates)) => {
                let predicates = predicates.into_iter().map(|mut predicate| {
                    if let Some(self_ty) = self_ty {
                        ReplaceSelf { self_ty }.visit_where_predicate_mut(&mut predicate);
                    }
                    predicate
                });
                asserts.extend(quote! {{
                    #[allow(unused_lifetimes)]
                    fn proof<#db_lt>() where #(#predicates,)* {}
                    proof::<#db_lt>();
                }});
                continue;
            }
            None => {}
        }
        let ty = *ty;
        if !lifetimes::names(ty, db_lt) {
            continue;
        }
        if is_db_reference(ty, db_lt) {
            asserts.extend(
                syn::Error::new_spanned(
                    ty,
                    "a reference tied to the database lifetime can't be stored; store an owned \
                     value or an ID instead",
                )
                .into_compile_error(),
            );
            continue;
        }
        asserts.extend(quote_spanned! {ty.span()=> assert_value::<#ty>(); });
    }
    if asserts.is_empty() {
        return Ok(TokenStream::new());
    }
    Ok(quote! {
        #[allow(dead_code, unused_lifetimes, clippy::extra_unused_lifetimes)]
        fn #name<#db_lt>() {
            fn assert_value<T: ?::core::marker::Sized + #db::Value>() {}
            #asserts
        }
    })
}

fn is_db_reference(ty: &Type, db_lt: &Lifetime) -> bool {
    matches!(
        ty,
        Type::Reference(reference)
            if reference.lifetime.as_ref().is_some_and(|it| it.ident == db_lt.ident)
    )
}

enum Proof {
    Conditional(Vec<WherePredicate>),
    Unconditional,
}

pub(crate) fn expand(input: &syn::DeriveInput) -> syn::Result<TokenStream> {
    let fields: Vec<&syn::Field> = match &input.data {
        // alloc: expansion time, once per annotated item; no query runs this
        syn::Data::Struct(data) => data.fields.iter().collect(),
        syn::Data::Enum(data) => {
            for variant in &data.variants {
                refuse_attributes(&variant.attrs, "a variant")?;
            }
            // alloc: expansion time, once per annotated item; no query runs this
            data.variants.iter().flat_map(|it| it.fields.iter()).collect()
        }
        syn::Data::Union(union) => {
            return Err(syn::Error::new_spanned(
                union.union_token,
                "`derive(Value)` does not support a `union`",
            ));
        }
    };
    refuse_attributes(&input.attrs, "a type")?;

    let mut declared = input.generics.lifetimes();
    // clone: expansion time, a lifetime
    let db_lt = declared.next().map(|it| it.lifetime.clone());
    if declared.next().is_some() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "`derive(Value)` supports at most one lifetime parameter",
        ));
    }

    let ident = &input.ident;
    let (_, type_generics, _) = input.generics.split_for_impl();
    let self_ty: Type = syn::parse_quote!(#ident #type_generics);
    let type_params: HashSet<&syn::Ident> =
        // alloc: expansion time, once per annotated item; no query runs this
        input.generics.type_params().map(|it| &it.ident).collect();

    // clone: expansion time, the item's generics extended with bounds
    let mut generics = input.generics.clone();
    // alloc: expansion time, once per annotated item; no query runs this
    let mut checked: Vec<Type> = Vec::new();
    for field in &fields {
        let ty = ReplaceSelf { self_ty: &self_ty }.in_type(&field.ty);
        match proof(field)? {
            Some(Proof::Conditional(predicates)) => {
                for mut predicate in predicates {
                    ReplaceSelf { self_ty: &self_ty }.visit_where_predicate_mut(&mut predicate);
                    generics.make_where_clause().predicates.push(predicate);
                }
            }
            Some(Proof::Unconditional) => {}
            None if uses_type_param(&ty, &type_params) => {
                let db = crate::db();
                generics.make_where_clause().predicates.push(syn::parse_quote!(#ty: #db::Value));
            }
            None => checked.push(ty),
        }
    }
    let (impl_generics, _, where_clause) = generics.split_for_impl();
    let db = crate::db();

    let assertions = match &db_lt {
        None => TokenStream::new(),
        Some(db_lt) => {
            let mut asserts = TokenStream::new();
            for ty in &checked {
                if !lifetimes::names(ty, db_lt) {
                    continue;
                }
                if is_db_reference(ty, db_lt) {
                    return Err(syn::Error::new_spanned(
                        ty,
                        "a reference tied to the database lifetime can't be stored; store an \
                         owned value or an ID instead",
                    ));
                }
                asserts.extend(quote_spanned! {ty.span()=> assert_value::<#ty>(); });
            }
            quote! {
                const _: () = {
                    #[allow(dead_code, unused_lifetimes, clippy::extra_unused_lifetimes)]
                    fn fields_are_values #impl_generics () #where_clause {
                        fn assert_value<T: ?::core::marker::Sized + #db::Value>() {}
                        #asserts
                    }
                };
            }
        }
    };

    Ok(quote! {
        #[automatically_derived]
        // SAFETY: every field is checked: a field naming the lifetime is `Value` (asserted
        // below), a field naming a type parameter is bounded by `Value`, a field naming neither
        // is `'static`, and a field with `#[value(unsafe(..))]` carries the user's proof.
        unsafe impl #impl_generics #db::Value for #self_ty #where_clause {}
        #assertions
    })
}

fn uses_type_param(ty: &Type, params: &HashSet<&syn::Ident>) -> bool {
    struct Uses<'a, 'b> {
        params: &'a HashSet<&'b syn::Ident>,
        found: bool,
    }
    impl<'ast> Visit<'ast> for Uses<'_, '_> {
        fn visit_type_path(&mut self, path: &'ast syn::TypePath) {
            if path.qself.is_none()
                && let Some(first) = path.path.segments.first()
            {
                self.found |= self.params.contains(&first.ident);
            }
            if !self.found {
                syn::visit::visit_type_path(self, path);
            }
        }
    }
    let mut uses = Uses { params, found: false };
    uses.visit_type(ty);
    uses.found
}

fn proof(field: &syn::Field) -> syn::Result<Option<Proof>> {
    let mut attrs = field.attrs.iter().filter(|it| it.path().is_ident("value"));
    let Some(attr) = attrs.next() else { return Ok(None) };
    if let Some(second) = attrs.next() {
        return Err(syn::Error::new_spanned(second, "a field takes one `#[value(..)]`"));
    }
    parse_proof(attr).map(Some)
}

/// `#[value(unsafe(prove(P, ..)))]` or `#[value(unsafe(prove_safe_to_retain_manually))]`.
fn parse_proof(attr: &Attribute) -> syn::Result<Proof> {
    attr.parse_args_with(|input: ParseStream<'_>| {
        if !input.peek(syn::Token![unsafe]) {
            return Err(input.error(
                "`#[value(..)]` is `unsafe(prove(P, ..))` or \
                 `unsafe(prove_safe_to_retain_manually)`",
            ));
        }
        input.parse::<syn::Token![unsafe]>()?;
        let content;
        syn::parenthesized!(content in input);
        let word: syn::Ident = content.parse()?;
        let proof = if word == "prove" {
            let predicates;
            syn::parenthesized!(predicates in content);
            if predicates.is_empty() {
                return Err(predicates.error("`prove(..)` needs at least one predicate"));
            }
            let parsed = predicates.parse_terminated(WherePredicate::parse, syn::Token![,])?;
            // alloc: expansion time, the field's predicates
            Proof::Conditional(parsed.into_iter().collect())
        } else if word == "prove_safe_to_retain_manually" {
            Proof::Unconditional
        } else {
            return Err(syn::Error::new(
                word.span(),
                "expected `prove(..)` or `prove_safe_to_retain_manually`",
            ));
        };
        if !content.is_empty() {
            return Err(content.error("unexpected tokens"));
        }
        Ok(proof)
    })
}

fn refuse_attributes(attrs: &[Attribute], what: &str) -> syn::Result<()> {
    match attrs.iter().find(|it| it.path().is_ident("value")) {
        Some(attr) => Err(syn::Error::new_spanned(
            attr,
            // alloc: expansion time, the error's message
            format!("`#[value]` goes on a field, not on {what}"),
        )),
        None => Ok(()),
    }
}
