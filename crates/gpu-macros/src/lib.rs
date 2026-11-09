//! The `gpu` crate's macros (docs/future/gpu/shaders.md). Each only lists what it is given, the
//! parameters' names and types or the fields' names and types; what a type means (a uniform, a
//! built-in, a flat varying) is its trait constant in `gpu`, and the rules (slots, locations, what a
//! stage may take) are `const fn`s there. So nothing here reads a name or the spelling of a type.
//!
//! - `#[gpu::vertex]`, `#[gpu::fragment]`, `#[gpu::kernel(threads = ...)]`: the function
//!   unchanged, and beside it a module of its name holding `ENTRY: gpu::EntryDesc`, `PARAMS`, and
//!   one `gpu::Slot` per parameter, named like it.
//! - `#[derive(gpu::Varyings)]`: `gpu::Varyings` (the fields and their locations), and `gpu::Param`
//!   and `gpu::Output`, so the struct is what a vertex stage returns and a fragment stage takes.
//! - `#[derive(gpu::Pod)]`: `gpu::Pod` for a `#[repr(C)]` struct whose fields are all `Pod` and
//!   which has no padding, both checked when it compiles.
use proc_macro::TokenStream;
use proc_macro2::TokenStream as T2;
use quote::{format_ident, quote};
use syn::{
    Data, DeriveInput, Expr, Fields, FnArg, ItemFn, Lit, Pat, ReturnType, Token, Type, parse::Parser as _,
    parse_macro_input, punctuated::Punctuated,
};

#[proc_macro_attribute]
pub fn vertex(attr: TokenStream, item: TokenStream) -> TokenStream {
    entry(attr, item, "Vertex")
}

#[proc_macro_attribute]
pub fn fragment(attr: TokenStream, item: TokenStream) -> TokenStream {
    entry(attr, item, "Fragment")
}

/// A compute kernel: `#[gpu::kernel(threads = 64)]`, `(8, 8)` or `(4, 4, 4)`, the workgroup's
/// size on each axis (Slang's `numthreads`); an axis left out is 1.
#[proc_macro_attribute]
pub fn kernel(attr: TokenStream, item: TokenStream) -> TokenStream {
    let f = parse_macro_input!(item as ItemFn);
    let what = format!("#[gpu::kernel] fn {}", f.sig.ident);
    let stage = match threads(T2::from(attr), &what) {
        Ok([x, y, z]) => quote! { ::gpu::Stage::Compute { x: #x, y: #y, z: #z } },
        Err(e) => return e.into_compile_error().into(),
    };
    described(&f, stage, "kernel").unwrap_or_else(syn::Error::into_compile_error).into()
}

/// The workgroup size of `threads = N | (x, y) | (x, y, z)`. Its bounds (at least one thread on an
/// axis, at most `gpu::MAX_THREADS` in all) are `gpu::check`'s, not here.
fn threads(attr: T2, what: &str) -> syn::Result<[u32; 3]> {
    let usage = || format!("{what}: write `#[gpu::kernel(threads = 64)]`, `(8, 8)` or `(4, 4, 4)`");
    let span = attr.clone(); // clone: the tokens, for an error's span
    let args = Punctuated::<syn::MetaNameValue, Token![,]>::parse_terminated
        .parse2(attr)
        .map_err(|e| syn::Error::new(e.span(), usage()))?;
    let mut found = None;
    for arg in &args {
        if !arg.path.is_ident("threads") {
            return Err(syn::Error::new_spanned(&arg.path, format!("{what}: the only argument is `threads`")));
        }
        if found.is_some() {
            return Err(syn::Error::new_spanned(arg, format!("{what}: `threads` is given once")));
        }
        found = Some(&arg.value);
    }
    let Some(value) = found else { return Err(syn::Error::new_spanned(span, usage())) };
    let axis = |e: &Expr| -> syn::Result<u32> {
        match e {
            Expr::Lit(l) => match &l.lit {
                Lit::Int(i) => i.base10_parse::<u32>(),
                _ => Err(syn::Error::new_spanned(e, usage())),
            },
            _ => Err(syn::Error::new_spanned(e, usage())),
        }
    };
    let mut size = [1u32; 3];
    match value {
        Expr::Tuple(t) if (1..=3).contains(&t.elems.len()) => {
            for (i, e) in t.elems.iter().enumerate() {
                size[i] = axis(e)?;
            }
        }
        Expr::Paren(p) => size[0] = axis(&p.expr)?,
        e @ Expr::Lit(_) => size[0] = axis(e)?,
        e => return Err(syn::Error::new_spanned(e, usage())),
    }
    Ok(size)
}

fn entry(attr: TokenStream, item: TokenStream, stage: &str) -> TokenStream {
    let attr = T2::from(attr);
    let lower = stage.to_lowercase();
    if !attr.is_empty() {
        return syn::Error::new_spanned(attr, format!("#[gpu::{lower}] takes no arguments")).into_compile_error().into();
    }
    let f = parse_macro_input!(item as ItemFn);
    let stage = format_ident!("{stage}");
    described(&f, quote! { ::gpu::Stage::#stage }, &lower).unwrap_or_else(syn::Error::into_compile_error).into()
}

/// `f` and its module of descriptors, for the stage `stage` (a `gpu::Stage` expression) that
/// `#[gpu::{attr}]` names.
fn described(f: &ItemFn, stage: T2, attr: &str) -> syn::Result<T2> {
    let sig = &f.sig;
    let what = format!("#[gpu::{attr}] fn {}", sig.ident);
    if !sig.generics.params.is_empty() || sig.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(&sig.generics, format!("{what}: an entry point is not generic")));
    }
    if let Some(a) = &sig.asyncness {
        return Err(syn::Error::new_spanned(a, format!("{what}: an entry point is not async")));
    }
    if let Some(v) = &sig.variadic {
        return Err(syn::Error::new_spanned(v, format!("{what}: an entry point takes no variadic arguments")));
    }
    // alloc: one expansion's lists, dropped with the macro's output
    let mut names = Vec::new();
    // alloc: as above
    let mut idents = Vec::new();
    // alloc: as above
    let mut tys: Vec<&Type> = Vec::new();
    for arg in &sig.inputs {
        let FnArg::Typed(pt) = arg else {
            return Err(syn::Error::new_spanned(arg, format!("{what}: an entry point takes no `self`")));
        };
        let Pat::Ident(pi) = &*pt.pat else {
            return Err(syn::Error::new_spanned(
                &pt.pat,
                format!("{what}: each parameter is a plain name, which the binding table is keyed by"),
            ));
        };
        names.push(pi.ident.to_string());
        idents.push(pi.ident.clone());
        tys.push(&pt.ty);
    }
    let unit: Type = syn::parse_quote!(());
    let ret: &Type = match &sig.output {
        ReturnType::Type(_, ret) => ret,
        // A kernel returns nothing; `gpu::check` refuses a kernel that returns something.
        ReturnType::Default if attr == "kernel" => &unit,
        ReturnType::Default => {
            return Err(syn::Error::new_spanned(
                &sig.ident,
                format!("{what}: an entry point returns its varyings (vertex) or its colour (fragment)"),
            ));
        }
    };
    let n = names.len();
    let name = &sig.ident;
    let name_s = name.to_string();
    let vis = &f.vis;
    let slots = idents.iter().enumerate().map(|(i, id)| {
        let doc = format!("Where `{name_s}`'s parameter `{id}` is bound.");
        quote! {
            #[doc = #doc]
            pub const #id: ::gpu::Slot = PARAMS[#i].slot;
        }
    });
    let doc = format!("`{name_s}`'s binding table, from its signature (`gpu::layout`).");
    Ok(quote! {
        #f

        #[doc = #doc]
        #[allow(non_upper_case_globals, non_snake_case, unused_imports)]
        #vis mod #name {
            use super::*;
            const NAMES: [&str; #n] = [#(#names),*];
            const KINDS: [::gpu::Kind; #n] = [#(<#tys as ::gpu::Param>::KIND),*];
            /// Each parameter's name, kind and slot.
            pub const PARAMS: [::gpu::ParamDesc; #n] = ::gpu::layout(NAMES, KINDS);
            /// The entry point as its signature describes it.
            pub const ENTRY: ::gpu::EntryDesc = ::gpu::EntryDesc {
                name: #name_s,
                stage: #stage,
                params: &PARAMS,
                out: <#ret as ::gpu::Output>::OUT,
                sizes: ::gpu::sizes_slot(&PARAMS),
            };
            const _: () = ::gpu::check(&ENTRY);
            #(#slots)*
        }
    })
}

#[proc_macro_derive(Varyings)]
pub fn derive_varyings(input: TokenStream) -> TokenStream {
    let di = parse_macro_input!(input as DeriveInput);
    varyings(&di).unwrap_or_else(syn::Error::into_compile_error).into()
}

fn varyings(di: &DeriveInput) -> syn::Result<T2> {
    let name = &di.ident;
    if !di.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(&di.generics, format!("#[derive(gpu::Varyings)] {name}: not generic")));
    }
    let Data::Struct(s) = &di.data else {
        return Err(syn::Error::new_spanned(name, format!("#[derive(gpu::Varyings)] {name}: a struct with named fields")));
    };
    let Fields::Named(fields) = &s.fields else {
        return Err(syn::Error::new_spanned(name, format!("#[derive(gpu::Varyings)] {name}: a struct with named fields")));
    };
    // alloc: one expansion's lists, dropped with the macro's output
    let names: Vec<String> = fields.named.iter().map(|f| f.ident.as_ref().map(ToString::to_string).unwrap_or_default()).collect();
    // alloc: as above
    let tys: Vec<&Type> = fields.named.iter().map(|f| &f.ty).collect();
    let n = names.len();
    Ok(quote! {
        impl ::gpu::Varyings for #name {
            const FIELDS: &'static [::gpu::Field] =
                &::gpu::layout_fields::<#n>([#(#names),*], [#(<#tys as ::gpu::Varying>::KIND),*]);
        }
        impl ::gpu::Param for #name {
            const KIND: ::gpu::Kind = ::gpu::Kind::Varyings(<#name as ::gpu::Varyings>::FIELDS);
        }
        impl ::gpu::Output for #name {
            const OUT: ::gpu::Out = ::gpu::Out::Varyings(<#name as ::gpu::Varyings>::FIELDS);
        }
    })
}

#[proc_macro_derive(Pod)]
pub fn derive_pod(input: TokenStream) -> TokenStream {
    let di = parse_macro_input!(input as DeriveInput);
    pod(&di).unwrap_or_else(syn::Error::into_compile_error).into()
}

fn pod(di: &DeriveInput) -> syn::Result<T2> {
    let name = &di.ident;
    if !di.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(&di.generics, format!("#[derive(gpu::Pod)] {name}: not generic")));
    }
    let mut repr_c = false;
    for a in di.attrs.iter().filter(|a| a.path().is_ident("repr")) {
        a.parse_nested_meta(|m| {
            repr_c |= m.path.is_ident("C") || m.path.is_ident("transparent");
            // `align(16)` and the like: step over the argument.
            if m.input.peek(syn::token::Paren) {
                let _arg;
                syn::parenthesized!(_arg in m.input);
            }
            Ok(())
        })?;
    }
    if !repr_c {
        return Err(syn::Error::new_spanned(
            name,
            format!("#[derive(gpu::Pod)] {name}: needs #[repr(C)], so host and device lay it out the same"),
        ));
    }
    let Data::Struct(s) = &di.data else {
        return Err(syn::Error::new_spanned(name, format!("#[derive(gpu::Pod)] {name}: a struct")));
    };
    // alloc: one expansion's list, dropped with the macro's output
    let tys: Vec<&Type> = s.fields.iter().map(|f| &f.ty).collect();
    let padding = format!("#[derive(gpu::Pod)] {name}: has padding, which the host would write as garbage");
    Ok(quote! {
        const _: () = {
            const fn pod<T: ::gpu::Pod>() {}
            #(pod::<#tys>();)*
            assert!(::core::mem::size_of::<#name>() == 0 #(+ ::core::mem::size_of::<#tys>())*, #padding);
        };
        // SAFETY: #[repr(C)], every field Pod, and no padding, all checked just above.
        unsafe impl ::gpu::Pod for #name {}
    })
}
