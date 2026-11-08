//! The `gpu` crate's macros (docs/future/gpu/shaders.md). Each only lists what it is given, the
//! parameters' names and types or the fields' names and types; what a type means (a uniform, a
//! built-in, a flat varying) is its trait constant in `gpu`, and the rules (slots, locations, what a
//! stage may take) are `const fn`s there. So nothing here reads a name or the spelling of a type.
//!
//! - `#[gpu::vertex]`, `#[gpu::fragment]`: the function unchanged, and beside it a module of its
//!   name holding `ENTRY: gpu::EntryDesc`, `PARAMS`, and one `gpu::Slot` per parameter, named like it.
//! - `#[derive(gpu::Varyings)]`: `gpu::Varyings` (the fields and their locations), and `gpu::Param`
//!   and `gpu::Output`, so the struct is what a vertex stage returns and a fragment stage takes.
//! - `#[derive(gpu::Pod)]`: `gpu::Pod` for a `#[repr(C)]` struct whose fields are all `Pod` and
//!   which has no padding, both checked when it compiles.
use proc_macro::TokenStream;
use proc_macro2::TokenStream as T2;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields, FnArg, ItemFn, Pat, ReturnType, Type, parse_macro_input};

#[proc_macro_attribute]
pub fn vertex(attr: TokenStream, item: TokenStream) -> TokenStream {
    entry(attr, item, "Vertex")
}

#[proc_macro_attribute]
pub fn fragment(attr: TokenStream, item: TokenStream) -> TokenStream {
    entry(attr, item, "Fragment")
}

fn entry(attr: TokenStream, item: TokenStream, stage: &str) -> TokenStream {
    let attr = T2::from(attr);
    if !attr.is_empty() {
        return syn::Error::new_spanned(attr, format!("#[gpu::{}] takes no arguments", stage.to_lowercase()))
            .into_compile_error()
            .into();
    }
    let f = parse_macro_input!(item as ItemFn);
    described(&f, stage).unwrap_or_else(syn::Error::into_compile_error).into()
}

fn described(f: &ItemFn, stage: &str) -> syn::Result<T2> {
    let sig = &f.sig;
    let what = format!("#[gpu::{}] fn {}", stage.to_lowercase(), sig.ident);
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
    let ReturnType::Type(_, ret) = &sig.output else {
        return Err(syn::Error::new_spanned(
            &sig.ident,
            format!("{what}: an entry point returns its varyings (vertex) or its colour (fragment)"),
        ));
    };
    let n = names.len();
    let name = &sig.ident;
    let name_s = name.to_string();
    let vis = &f.vis;
    let stage = format_ident!("{stage}");
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
                stage: ::gpu::Stage::#stage,
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
