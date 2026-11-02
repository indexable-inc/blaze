//! `#[blaze_query::tracked]` on an inherent `impl`: each function in it with its own
//! `#[blaze_query::tracked(..)]` is a query. Its body stays in the `impl` as a private method,
//! so `Self` and the impl's lifetime mean what they did; the query's struct is
//! `TypeName_method`, beside the `impl`.

use proc_macro2::TokenStream;
use quote::{ToTokens, format_ident, quote};
use syn::{
    Attribute, FnArg, GenericParam, ImplItem, ImplItemFn, ItemImpl, Lifetime, Type, parse_quote,
};

use crate::hygiene::Hygiene;
use crate::lifetimes::{self, Both, ChangeLt, ReplaceSelf};
use crate::options::{Options, Site};
use crate::query::{Arg, Query};
use crate::tracked_fn::{cfgs, db_param, output_type, refuse_modifiers, split_attrs};

pub(crate) fn expand(
    args: TokenStream,
    item: &ItemImpl,
) -> Result<TokenStream, (syn::Error, TokenStream)> {
    let stripped = |item: &ItemImpl| {
        // clone: expansion time, the item given back with the attributes it can't keep
        let mut item = item.clone();
        for member in &mut item.items {
            if let ImplItem::Fn(it) = member {
                it.attrs.retain(|attr| !is_tracked(attr));
            }
        }
        item.into_token_stream()
    };
    match expand_in(args, item) {
        Ok(tokens) => Ok(tokens),
        Err(error) => Err((error, stripped(item))),
    }
}

fn expand_in(args: TokenStream, item: &ItemImpl) -> syn::Result<TokenStream> {
    if !args.is_empty() {
        return Err(syn::Error::new_spanned(
            args,
            "`#[blaze_query::tracked]` on an `impl` takes no options; they go on its functions",
        ));
    }
    if let Some((_, path, _)) = &item.trait_ {
        return Err(syn::Error::new_spanned(
            path,
            "`#[blaze_query::tracked]` goes on an inherent `impl`, not a trait's",
        ));
    }
    let mut impl_lt = None;
    for param in &item.generics.params {
        match param {
            GenericParam::Lifetime(it) if impl_lt.is_none() => impl_lt = Some(&it.lifetime),
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "a tracked `impl` takes at most one lifetime, the database's, and no type \
                     or const parameters",
                ));
            }
        }
    }
    let type_name = match &*item.self_ty {
        Type::Path(path) if path.qself.is_none() => match path.path.segments.last() {
            Some(last) => &last.ident,
            None => return Err(syn::Error::new_spanned(&item.self_ty, "expected a type")),
        },
        other => {
            return Err(syn::Error::new_spanned(other, "a tracked `impl` is for a named type"));
        }
    };

    let hygiene = Hygiene::of(item);
    // clone: expansion time, the impl rebuilt with its queries' public methods
    let mut out = item.clone();
    let mut beside = TokenStream::new();
    // alloc: expansion time, once per annotated item; no query runs this
    let mut bodies = Vec::new();
    for member in &mut out.items {
        let ImplItem::Fn(method) = member else { continue };
        let Some(position) = method.attrs.iter().position(is_tracked) else { continue };
        let attr = method.attrs.remove(position);
        let options = match &attr.meta {
            syn::Meta::Path(_) => Options::default(),
            syn::Meta::List(list) => Options::parse(Site::TrackedFn, list.tokens.clone())?, // clone: expansion time, the attribute's tokens
            syn::Meta::NameValue(it) => {
                return Err(syn::Error::new_spanned(it, "expected `#[blaze_query::tracked(..)]`"));
            }
        };
        let (body, generated) = method_query(item, impl_lt, type_name, method, &options, &hygiene)?;
        bodies.push(ImplItem::Fn(body));
        beside.extend(generated);
    }
    out.items.extend(bodies);
    Ok(quote! {
        #out
        #beside
    })
}

fn is_tracked(attr: &Attribute) -> bool {
    let path = attr.path();
    // alloc: expansion time, once per annotated item; no query runs this
    let names: Vec<_> = path.segments.iter().map(|it| &it.ident).collect();
    names.len() == 2
        && names[0] == "blaze_query"
        && names[1] == "tracked"
        && path.segments.iter().all(|it| it.arguments.is_empty())
}

/// Rewrites `method` into the public method and returns the body's private method and the
/// query's items.
fn method_query(
    item: &ItemImpl,
    impl_lt: Option<&Lifetime>,
    type_name: &syn::Ident,
    method: &mut ImplItemFn,
    options: &Options,
    hygiene: &Hygiene,
) -> syn::Result<(ImplItemFn, TokenStream)> {
    let sig = &method.sig;
    refuse_modifiers(sig)?;
    let mut method_lt = None;
    for param in &sig.generics.params {
        match param {
            GenericParam::Lifetime(it) if impl_lt.is_none() && method_lt.is_none() => {
                if let Some(bound) = it.bounds.first() {
                    return Err(syn::Error::new_spanned(
                        bound,
                        "the database lifetime of a tracked method takes no bounds",
                    ));
                }
                method_lt = Some(&it.lifetime);
            }
            GenericParam::Lifetime(it) => {
                return Err(syn::Error::new_spanned(
                    it,
                    "a tracked method has one lifetime in scope, the database's: on the `impl` \
                     or on the method",
                ));
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "a tracked method takes no type or const parameters",
                ));
            }
        }
    }
    if let Some(clause) = &sig.generics.where_clause {
        return Err(syn::Error::new_spanned(clause, "a tracked method takes no `where` clause"));
    }
    let declared = impl_lt.or(method_lt);
    let db_lt = match declared {
        // clone: expansion time, a lifetime
        Some(it) => it.clone(),
        None => lifetimes::conjure(sig, Some(&item.self_ty), sig.ident.span()),
    };
    let self_ty = ChangeLt::elided_to(&db_lt).in_type(&item.self_ty);

    let mut inputs = sig.inputs.iter();
    let has_self = match sig.inputs.first() {
        Some(FnArg::Receiver(receiver)) => {
            if receiver.reference.is_some() || receiver.colon_token.is_some() {
                return Err(syn::Error::new_spanned(
                    receiver,
                    "a tracked method takes `self` by value: it is the query's key",
                ));
            }
            inputs.next();
            true
        }
        _ => false,
    };
    let db_input = inputs.next();
    let db_ty = db_param(db_input, sig)?;
    let db_ty = ReplaceSelf { self_ty: &self_ty }.in_type(&db_ty);
    let db_pat = match db_input {
        Some(FnArg::Typed(typed)) => &typed.pat,
        _ => unreachable!("`db_param` accepted it"),
    };

    // alloc: expansion time, once per annotated item; no query runs this
    let mut query_args = Vec::new();
    // alloc: expansion time, once per annotated item; no query runs this
    let mut public_params = Vec::new();
    // alloc: expansion time, once per annotated item; no query runs this
    let mut body_params = Vec::new();
    if has_self {
        let ident = hygiene.ident("arg_self");
        query_args.push(Arg { ident, ty: Both::of(&self_ty, &db_lt) });
    }
    for input in inputs {
        let FnArg::Typed(typed) = input else {
            return Err(syn::Error::new_spanned(input, "expected an argument"));
        };
        // alloc: expansion time, one name per argument
        let ident = hygiene.ident(&format!("arg{}", query_args.len()));
        let ty = ReplaceSelf { self_ty: &self_ty }.in_type(&typed.ty);
        let ty = Both::of(&ty, &db_lt);
        let db_form = &ty.db;
        public_params.push(quote!(#ident: #db_form));
        let pat = &typed.pat;
        let body_ty = ChangeLt::elided_to(&db_lt).in_type(&typed.ty);
        body_params.push(quote!(#pat: #body_ty));
        query_args.push(Arg { ident, ty });
    }
    let output = ReplaceSelf { self_ty: &self_ty }.in_type(&output_type(sig));
    let output = Both::of(&output, &db_lt);

    let name = &sig.ident;
    // `NAME` is what the kind tables and `Db::log` expectations hold: `SelfTy::method`, the
    // type's name without its generics (`Modules::module`), or `self_ty`'s text when given.
    // alloc: expansion time, once per annotated item; no query runs this
    let inner_name = hygiene.ident(&name.to_string());
    let struct_name = format_ident!("{type_name}_{name}");
    let named = options.self_ty.as_ref().map_or_else(|| quote!(#type_name), |it| quote!(#it));
    let query_name =
        quote!(::core::concat!(::core::stringify!(#named), "::", ::core::stringify!(#name)));
    // clone: expansion time, the argument names, used again after the query takes the arguments
    let idents: Vec<syn::Ident> = query_args.iter().map(|it| it.ident.clone()).collect();
    let call = if has_self {
        let rest = &idents[1..];
        let this = &idents[0];
        quote!(<#self_ty>::#inner_name(#this, db, #(#rest),*))
    } else {
        quote!(<#self_ty>::#inner_name(db, #(#idents),*))
    };
    let replace = |expr: &Option<syn::Expr>| {
        expr.as_ref().map(|it| ReplaceSelf { self_ty: &self_ty }.in_expr(it))
    };
    let query = Query {
        options,
        name: struct_name,
        query_name,
        vis: method.vis.clone(), // clone: expansion time, a visibility
        cfgs: cfgs(&method.attrs),
        db_lt: db_lt.clone(), // clone: expansion time, a lifetime
        db_ty: db_ty.clone(), // clone: expansion time, a type
        args: query_args,
        output,
        call,
        inner: TokenStream::new(),
        cycle_fn: replace(&options.cycle_fn),
        cycle_initial: replace(&options.cycle_initial),
        cycle_result: replace(&options.cycle_result),
        hygiene,
    };

    let (public_attrs, inner_attrs) = split_attrs(&method.attrs);
    let declare_lt = if impl_lt.is_none() { quote!(<#db_lt>) } else { TokenStream::new() };
    let body_output = ChangeLt::elided_to(&db_lt).in_type(&output_type(sig));
    let block = &method.block;
    let self_param = if has_self { quote!(self,) } else { TokenStream::new() };
    let body: ImplItemFn = parse_quote! {
        #(#inner_attrs)*
        #[allow(clippy::needless_lifetimes, clippy::too_many_arguments)]
        fn #inner_name #declare_lt (#self_param #db_pat: &#db_lt #db_ty, #(#body_params),*)
            -> #body_output
        #block
    };

    // alloc: expansion time, once per annotated item; no query runs this
    let mut fetch_args = vec![quote!(db)];
    if has_self {
        fetch_args.push(quote!(self));
    }
    fetch_args.extend(idents[usize::from(has_self)..].iter().map(|ident| quote!(#ident)));
    let public_output = query.public_output();
    let public_body = query.public_body(&fetch_args);
    let vis = &method.vis;
    *method = parse_quote! {
        #(#public_attrs)*
        #[allow(clippy::needless_lifetimes, clippy::too_many_arguments)]
        #vis fn #name #declare_lt (#self_param db: &#db_lt #db_ty, #(#public_params),*)
            -> #public_output
        {
            #public_body
        }
    };
    Ok((body, query.expand()))
}
