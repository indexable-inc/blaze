//! `#[blaze_query::tracked(..)]` on a free function: the user's function becomes the query's
//! body, and a function of the same name and arguments fetches it.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Attribute, FnArg, GenericParam, ItemFn, Signature, Type};

use crate::hygiene::Hygiene;
use crate::lifetimes::{self, Both};
use crate::options::{Options, Site};
use crate::query::{Arg, Query};

pub(crate) fn expand(args: TokenStream, item: &ItemFn) -> syn::Result<TokenStream> {
    let options = Options::parse(Site::TrackedFn, args)?;
    let hygiene = Hygiene::of(item);
    let sig = &item.sig;
    refuse_modifiers(sig)?;

    let mut declared = None;
    for param in &sig.generics.params {
        match param {
            GenericParam::Lifetime(it) if declared.is_none() => {
                if let Some(bound) = it.bounds.first() {
                    return Err(syn::Error::new_spanned(
                        bound,
                        "the database lifetime of a tracked function takes no bounds",
                    ));
                }
                declared = Some(&it.lifetime);
            }
            GenericParam::Lifetime(it) => {
                return Err(syn::Error::new_spanned(
                    it,
                    "a tracked function takes one lifetime, the database's",
                ));
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "a tracked function takes no type or const parameters",
                ));
            }
        }
    }
    if let Some(clause) = &sig.generics.where_clause {
        return Err(syn::Error::new_spanned(clause, "a tracked function takes no `where` clause"));
    }
    let db_lt = match declared {
        // clone: expansion time, a lifetime
        Some(it) => it.clone(),
        None => lifetimes::conjure(sig, None, sig.ident.span()),
    };

    let mut inputs = sig.inputs.iter();
    let db_ty = db_param(inputs.next(), sig)?;
    // alloc: expansion time, once per annotated item; no query runs this
    let mut call_args = vec![quote!(db)];
    // alloc: expansion time, once per annotated item; no query runs this
    let mut public_params = Vec::new();
    // alloc: expansion time, once per annotated item; no query runs this
    let mut query_args = Vec::new();
    for (index, input) in inputs.enumerate() {
        let FnArg::Typed(typed) = input else {
            return Err(syn::Error::new_spanned(input, "a free function takes no `self`"));
        };
        // alloc: expansion time, one name per argument
        let ident = hygiene.ident(&format!("arg{index}"));
        let ty = Both::of(&typed.ty, &db_lt);
        let db_form = &ty.db;
        public_params.push(quote!(#ident: #db_form));
        call_args.push(quote!(#ident));
        query_args.push(Arg { ident, ty });
    }
    let output = output_type(sig);
    let output = Both::of(&output, &db_lt);

    let (public_attrs, inner_attrs) = split_attrs(&item.attrs);
    let cfgs = cfgs(&item.attrs);
    // alloc: expansion time, once per annotated item; no query runs this
    let inner_name = hygiene.ident(&sig.ident.to_string());
    let mut inner_fn = item.clone(); // clone: expansion time, the user's function moved inside
    inner_fn.attrs = inner_attrs;
    inner_fn.vis = syn::Visibility::Inherited;
    inner_fn.sig.ident = inner_name.clone(); // clone: expansion time, an identifier

    let name = &sig.ident;
    let query_name = match &options.self_ty {
        Some(self_ty) => {
            quote!(::core::concat!(::core::stringify!(#self_ty), "::", ::core::stringify!(#name)))
        }
        None => quote!(::core::stringify!(#name)),
    };
    let query = Query {
        options: &options,
        name: name.clone(), // clone: expansion time, an identifier
        query_name,
        vis: item.vis.clone(), // clone: expansion time, a visibility
        cfgs,
        db_lt: db_lt.clone(), // clone: expansion time, a lifetime
        db_ty: db_ty.clone(), // clone: expansion time, a type
        args: query_args,
        output,
        call: quote!(#inner_name(#(#call_args),*)),
        inner: quote!(#inner_fn),
        cycle_fn: options.cycle_fn.clone(), // clone: expansion time, an option
        cycle_initial: options.cycle_initial.clone(), // clone: expansion time, an option
        cycle_result: options.cycle_result.clone(), // clone: expansion time, an option
        hygiene: &hygiene,
    };

    let vis = &item.vis;
    let public_output = query.public_output();
    let fetch_args: Vec<TokenStream> = call_args;
    let body = query.public_body(&fetch_args);
    let generated = query.expand();
    Ok(quote! {
        #(#public_attrs)*
        #[allow(clippy::needless_lifetimes, clippy::too_many_arguments)]
        #vis fn #name<#db_lt>(db: &#db_lt #db_ty, #(#public_params),*) -> #public_output {
            #body
        }
        #generated
    })
}

pub(crate) fn refuse_modifiers(sig: &Signature) -> syn::Result<()> {
    let refuse = |what: &dyn quote::ToTokens, name: &str| {
        Err(syn::Error::new_spanned(
            what,
            // alloc: expansion time, the error's message
            format!("a tracked function can't be `{name}`"),
        ))
    };
    if let Some(it) = &sig.constness {
        return refuse(it, "const");
    }
    if let Some(it) = &sig.asyncness {
        return refuse(it, "async");
    }
    if let Some(it) = &sig.unsafety {
        return refuse(it, "unsafe");
    }
    if let Some(it) = &sig.abi {
        return refuse(it, "extern");
    }
    if let Some(it) = &sig.variadic {
        return Err(syn::Error::new_spanned(it, "a tracked function can't be variadic"));
    }
    Ok(())
}

/// The database parameter `db: &Db` (or `&'db Db`): its `Db`.
pub(crate) fn db_param(input: Option<&FnArg>, sig: &Signature) -> syn::Result<Type> {
    let Some(input) = input else {
        return Err(syn::Error::new_spanned(
            &sig.ident,
            "a tracked function takes the database as its first argument",
        ));
    };
    let FnArg::Typed(typed) = input else {
        return Err(syn::Error::new_spanned(input, "the database argument is `db: &Db`"));
    };
    match &*typed.ty {
        Type::Reference(reference) if reference.mutability.is_none() => {
            Ok((*reference.elem).clone()) // clone: expansion time, a type
        }
        other => Err(syn::Error::new_spanned(
            other,
            "the database argument of a tracked function is a shared reference, `&Db`",
        )),
    }
}

pub(crate) fn output_type(sig: &Signature) -> Type {
    match &sig.output {
        syn::ReturnType::Default => syn::parse_quote!(()),
        syn::ReturnType::Type(_, ty) => (**ty).clone(), // clone: expansion time, a type
    }
}

/// The attributes of the public function and of the body. Docs are the public function's;
/// `#[expect(..)]` is the body's (on both, one of them would be unfulfilled); the rest go on
/// both.
pub(crate) fn split_attrs(attrs: &[Attribute]) -> (Vec<Attribute>, Vec<Attribute>) {
    // alloc: expansion time, once per annotated item; no query runs this
    let mut public = Vec::new();
    // alloc: expansion time, once per annotated item; no query runs this
    let mut inner = Vec::new();
    for attr in attrs {
        let path = attr.path();
        if path.is_ident("doc") {
            public.push(attr.clone()); // clone: expansion time, an attribute
        } else if path.is_ident("expect") {
            inner.push(attr.clone()); // clone: expansion time, an attribute
        } else {
            public.push(attr.clone()); // clone: expansion time, an attribute
            inner.push(attr.clone()); // clone: expansion time, an attribute
        }
    }
    (public, inner)
}

pub(crate) fn cfgs(attrs: &[Attribute]) -> Vec<Attribute> {
    attrs
        .iter()
        .filter(|it| it.path().is_ident("cfg"))
        .cloned() // clone: expansion time, an attribute
        .collect()
}
