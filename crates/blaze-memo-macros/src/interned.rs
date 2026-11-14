//! `#[blaze_memo::interned(debug, constructor = name, unsafe(no_lifetime))]`: a struct of
//! fields stored once in their home arena, its id the same for equal fields in one snapshot.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{ItemStruct, Lifetime};

use crate::fields::{self, Field, Kind};
use crate::hygiene::Hygiene;
use crate::lifetimes::Both;
use crate::options::{Options, Site};
use crate::{plumbing, structs};

pub(crate) fn expand(
    args: TokenStream,
    item: &ItemStruct,
) -> Result<TokenStream, (syn::Error, TokenStream)> {
    expand_in(args, item).map_err(|error| (error, structs::stripped(item)))
}

fn expand_in(args: TokenStream, item: &ItemStruct) -> syn::Result<TokenStream> {
    let options = Options::parse(Site::Interned, args)?;
    let has_lifetime = options.no_lifetime.is_none();
    let mut declared = None;
    for param in &item.generics.params {
        match param {
            syn::GenericParam::Lifetime(it) if declared.is_none() && has_lifetime => {
                declared = Some(&it.lifetime);
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    if has_lifetime {
                        "an interned struct takes one lifetime, the database's, and no type or \
                         const parameters"
                    } else {
                        "an `unsafe(no_lifetime)` interned struct takes no generic parameters"
                    },
                ));
            }
        }
    }
    if let Some(clause) = &item.generics.where_clause {
        return Err(syn::Error::new_spanned(clause, "an interned struct takes no `where` clause"));
    }
    let fields = fields::parse(Kind::Interned, item)?;
    let hygiene = Hygiene::of(item);
    let plumbing = plumbing();
    let ident = &item.ident;
    let config = hygiene.ident("Config");
    let kind_static = hygiene.ident("KIND");
    let database = hygiene.ident("D");
    let db_lt = match declared {
        Some(it) => it.clone(), // clone: expansion time, a lifetime
        None => Lifetime::new("'db", item.ident.span()),
    };
    let struct_lt = has_lifetime.then_some(&db_lt);
    let self_ty: syn::Type = match struct_lt {
        Some(lt) => syn::parse_quote!(#ident<#lt>),
        None => syn::parse_quote!(#ident),
    };

    // alloc: expansion time, once per annotated item; no query runs this
    let both: Vec<Both> = fields.iter().map(|it| Both::of(&it.ty, &db_lt)).collect();
    // alloc: expansion time, once per annotated item; no query runs this
    let stored: Vec<&syn::Type> = both.iter().map(|it| &it.stored).collect();
    // alloc: expansion time, once per annotated item; no query runs this
    let field_db: Vec<&syn::Type> = both.iter().map(|it| &it.db).collect();
    // alloc: expansion time, once per annotated item; no query runs this
    let field_idents: Vec<&syn::Ident> = fields.iter().map(|it| &it.ident).collect();
    // alloc: expansion time, once per annotated item; no query runs this
    let indices: Vec<syn::Index> = (0..fields.len()).map(syn::Index::from).collect();
    let stores = both.iter().zip(&field_idents).map(|(it, ident)| it.store(&quote!(#ident)));

    let constructor = match &options.constructor {
        Some(it) => it.clone(), // clone: expansion time, an identifier
        None => syn::Ident::new("new", ident.span()),
    };
    let make = structs::construct(struct_lt, &quote!(gid));
    let method_lt = if has_lifetime { TokenStream::new() } else { quote!(#db_lt,) };

    let mut getters = TokenStream::new();
    for ((field, it), index) in fields.iter().zip(&both).zip(&indices) {
        let Field { vis, mode, getter, attrs, .. } = field;
        let out_ty = mode.ty(&db_lt, &it.db);
        let value = it.load_ref(&quote!(&fields.#index));
        let out = mode.expr(&quote!(field));
        let field_ty = &it.db;
        getters.extend(quote! {
            #(#attrs)*
            #vis fn #getter<#method_lt #database>(self, db: &#db_lt #database) -> #out_ty
            where
                #database: ?::core::marker::Sized + #plumbing::Database,
            {
                let fields = #plumbing::interned::<#config>(
                    #plumbing::Database::memo(db),
                    self.0,
                );
                let field: &#db_lt #field_ty = #value;
                #out
            }
        });
    }
    let impl_lt = if has_lifetime { quote!(<#db_lt>) } else { TokenStream::new() };

    let debug = options.debug.as_ref().map(|_| {
        // alloc: expansion time, once per annotated item; no query runs this
        let names = fields.iter().map(|it| it.ident.to_string());
        let loads =
            both.iter().zip(&indices).map(|(it, index)| it.load_ref(&quote!(&fields.#index)));
        quote! {
            impl #impl_lt ::core::fmt::Debug for #self_ty {
                fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    let this = *self;
                    #plumbing::attached_interned::<#config, _>(this.0, |fields| match fields {
                        ::core::option::Option::Some(fields) => f
                            .debug_struct(::core::stringify!(#ident))
                            #(.field(#names, #loads))*
                            .finish(),
                        ::core::option::Option::None => {
                            f.debug_tuple(::core::stringify!(#ident)).field(&this.0).finish()
                        }
                    })
                }
            }
        }
    });

    let declaration = structs::declaration(item, struct_lt);
    let id_impls = structs::id_impls(ident, struct_lt);
    Ok(quote! {
        #declaration

        #[allow(non_camel_case_types, non_snake_case, dead_code, clippy::all)]
        const _: () = {
            static #kind_static: #plumbing::Kind = #plumbing::Kind::new(
                ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#ident)),
                #plumbing::CycleKind::Panic,
            );

            struct #config;

            impl #plumbing::Interned for #config {
                type Fields = (#(#stored,)*);
                const KIND: &'static #plumbing::Kind = &#kind_static;
            }

            #id_impls

            impl #impl_lt #self_ty {
                /// The struct of these fields, interned in their home arena: the one made
                /// before in this snapshot for equal fields.
                #[inline]
                pub fn #constructor<#method_lt #database>(
                    db: &#db_lt #database,
                    #(#field_idents: #field_db),*
                ) -> Self
                where
                    #database: ?::core::marker::Sized + #plumbing::Database,
                {
                    let gid = #plumbing::intern::<#config>(
                        #plumbing::Database::memo(db),
                        (#(#stores,)*),
                    );
                    #make
                }

                #getters
            }

            #debug
        };
    })
}
