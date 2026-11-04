//! `#[derive(Layout)]` and `#[derive(Component)]` for harness-native-flecs.
//!
//! `Layout` writes the type's recipe (hyperion-layout): every field's name, `offset_of!` and the recipe of its type, the size and
//! the alignment, so its layout hash changes whenever any of those does, in this type or in a type it holds. A fieldless enum is
//! an enum recipe with its constants (reflected as a flecs enum); an enum with data is a flecs opaque type that reads as a choice
//! of its variants (each holds at most one value), hashed by its size, alignment, variant names and field type names (a change
//! inside a field's own type does not reach that hash).
//! `Component` is `Layout` plus the world name (the type's name) by which every image, the host and each module, finds it.
use proc_macro::TokenStream;
use proc_macro2::TokenStream as T2;
use quote::quote;
use syn::{Data, DeriveInput, Fields, parse_macro_input};

#[proc_macro_derive(Layout, attributes(den))]
pub fn derive_layout(input: TokenStream) -> TokenStream {
    let di = parse_macro_input!(input as DeriveInput);
    layout(&di).unwrap_or_else(syn::Error::into_compile_error).into()
}

#[proc_macro_derive(Component, attributes(den))]
pub fn derive_component(input: TokenStream) -> TokenStream {
    let di = parse_macro_input!(input as DeriveInput);
    let name = &di.ident;
    let world_name = name.to_string();
    let out = layout(&di).map(|l| {
        quote! {
            #l
            impl ::harness_native_flecs::__private::ApiComponent for #name {
                const NAME: &'static str = #world_name;
                const API_CRATE: &'static str = env!("CARGO_PKG_NAME");
            }
        }
    });
    out.unwrap_or_else(syn::Error::into_compile_error).into()
}

fn layout(di: &DeriveInput) -> syn::Result<T2> {
    if !di.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(&di.generics, "a harness-native-flecs layout type cannot be generic"));
    }
    let name = &di.ident;
    let p = quote!(::harness_native_flecs::__private);
    let recipe = match &di.data {
        Data::Struct(s) => {
            let fields: Vec<T2> = match &s.fields {
                Fields::Named(f) => f
                    .named
                    .iter()
                    .map(|f| {
                        let id = f.ident.as_ref().expect("named field");
                        let ty = &f.ty;
                        let n = id.to_string();
                        quote!(#p::Field { name: #p::raw_name(#n), offset: ::core::mem::offset_of!(#name, #id), ty: <#ty as #p::Layout>::RECIPE })
                    })
                    .collect(),
                Fields::Unnamed(f) => f
                    .unnamed
                    .iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let idx = syn::Index::from(i);
                        let ty = &f.ty;
                        let n = i.to_string();
                        quote!(#p::Field { name: #n, offset: ::core::mem::offset_of!(#name, #idx), ty: <#ty as #p::Layout>::RECIPE })
                    })
                    .collect(),
                Fields::Unit => Vec::new(),
            };
            quote!(#p::Recipe::Struct { fields: &[#(#fields),*], size: ::core::mem::size_of::<#name>(), align: ::core::mem::align_of::<#name>() })
        }
        Data::Enum(e) if e.variants.iter().all(|v| matches!(v.fields, Fields::Unit)) => {
            let consts = e.variants.iter().map(|v| {
                let vi = &v.ident;
                let n = vi.to_string();
                quote!((#n, #name::#vi as i64))
            });
            quote!(#p::Recipe::Enum {
                kind: #p::EnumKind::Enum,
                consts: &[#(#consts),*],
                tag: match ::core::mem::size_of::<#name>() {
                    1 => <u8 as #p::Layout>::RECIPE,
                    2 => <u16 as #p::Layout>::RECIPE,
                    4 => <u32 as #p::Layout>::RECIPE,
                    _ => <u64 as #p::Layout>::RECIPE,
                },
                size: ::core::mem::size_of::<#name>(),
                align: ::core::mem::align_of::<#name>(),
            })
        }
        Data::Enum(e) => {
            let variants: Vec<String> = e
                .variants
                .iter()
                .map(|v| {
                    let tys: Vec<String> = v.fields.iter().map(|f| f.ty.clone()).map(|t| quote!(#t).to_string()).collect();
                    format!("{}({})", v.ident, tys.join(", "))
                })
                .collect();
            let text = format!("enum {name} {{ {} }}", variants.join(", "));
            let mut described = Vec::new();
            let mut arms = Vec::new();
            for (i, v) in e.variants.iter().enumerate() {
                let vi = &v.ident;
                let n = vi.to_string();
                match &v.fields {
                    Fields::Unit => {
                        described.push(quote!(#p::Variant { name: #n, data: None }));
                        arms.push(quote!(#name::#vi => (#i, ::core::ptr::null())));
                    }
                    Fields::Unnamed(f) if f.unnamed.len() == 1 => {
                        let ty = &f.unnamed[0].ty;
                        described.push(quote!(#p::Variant { name: #n, data: Some(<#ty as #p::Layout>::RECIPE) }));
                        arms.push(quote!(#name::#vi(x) => (#i, ::core::ptr::from_ref(x).cast())));
                    }
                    Fields::Named(f) if f.named.len() == 1 => {
                        let field = f.named[0].ident.as_ref().expect("named field");
                        let ty = &f.named[0].ty;
                        described.push(quote!(#p::Variant { name: #n, data: Some(<#ty as #p::Layout>::RECIPE) }));
                        arms.push(quote!(#name::#vi { #field } => (#i, ::core::ptr::from_ref(#field).cast())));
                    }
                    _ => {
                        return Err(syn::Error::new_spanned(
                            v,
                            "a variant of a harness-native-flecs enum holds at most one value: put several in a struct, so flecs can name them",
                        ));
                    }
                }
            }
            quote!(#p::Recipe::Opaque {
                shape: &#p::Recipe::Prim { name: #text, kind: #p::Prim::Ptr, size: ::core::mem::size_of::<#name>(), align: ::core::mem::align_of::<#name>() },
                form: #p::Form::OneOf {
                    variants: &[#(#described),*],
                    get: {
                        unsafe fn which(src: *const u8) -> (usize, *const u8) {
                            // SAFETY: the caller's contract: a live value of the enum.
                            match unsafe { &*src.cast::<#name>() } {
                                #(#arms),*
                            }
                        }
                        which
                    },
                },
                serialize: #p::serialize::<#name>,
            })
        }
        Data::Union(_) => return Err(syn::Error::new_spanned(name, "a harness-native-flecs layout type cannot be a union")),
    };
    let golden = golden::emit(di, &proc_macro::Span::call_site().file())?;
    Ok(quote! {
        impl #p::Layout for #name {
            const RECIPE: &'static #p::Recipe = &#recipe;
        }
        #golden
    })
}

mod golden;
