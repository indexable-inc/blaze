//! `#[blaze_query::interned(debug, constructor = name, unsafe(no_lifetime))]`:
//! a struct of fields stored once, its ID the same for equal fields.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{ItemStruct, Lifetime};

use crate::fields::{self, Field, Kind};
use crate::hygiene::Hygiene;
use crate::lifetimes::Both;
use crate::options::{Options, Site};
use crate::{db, plumbing, structs, value};

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
    let db = db();
    let plumbing = plumbing();
    let ident = &item.ident;
    let config = hygiene.ident("Configuration");
    let vtable = hygiene.ident("VTABLE");
    let ingredient = hygiene.ident("INGREDIENT");
    let database = hygiene.ident("D");
    let value_check = hygiene.ident("fields_are_values");
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
    let checked: Vec<(&syn::Type, Option<&syn::Attribute>)> =
        // alloc: expansion time, once per annotated item; no query runs this
        both.iter().zip(&fields).map(|(it, field)| (&it.db, field.proven.as_ref())).collect();
    let checks = value::check_proven(&value_check, &db_lt, &checked, Some(&self_ty))?;

    let constructor = match &options.constructor {
        Some(it) => it.clone(), // clone: expansion time, an identifier
        None => syn::Ident::new("new", ident.span()),
    };
    // alloc: expansion time, once per annotated item; no query runs this
    let field_idents: Vec<&syn::Ident> = fields.iter().map(|it| &it.ident).collect();
    // alloc: expansion time, one type parameter per field
    let lookups: Vec<syn::Ident> =
        // alloc: expansion time, once per annotated item; no query runs this
        (0..fields.len()).map(|index| hygiene.ident(&format!("L{index}"))).collect();
    // alloc: expansion time, once per annotated item; no query runs this
    let indices: Vec<syn::Index> = (0..fields.len()).map(syn::Index::from).collect();
    let eq = if fields.is_empty() {
        quote!(true)
    } else {
        let compares = both.iter().zip(&indices).map(|(it, index)| {
            let field = it.load_ref(&quote!(&fields.#index));
            quote!(#plumbing::HashEqLike::eq(#field, &key.#index))
        });
        quote!(#(#compares)&&*)
    };
    let owned = both
        .iter()
        .zip(&indices)
        .map(|(it, index)| it.store(&quote!(#plumbing::Lookup::into_owned(key.#index))));
    let make = structs::construct(struct_lt, &quote!(id));

    let mut getters = TokenStream::new();
    for ((field, it), index) in fields.iter().zip(&both).zip(&indices) {
        let Field { vis, mode, getter, attrs, .. } = field;
        let out_ty = mode.ty(&db_lt, &it.db);
        let value = it.load_ref(&quote!(&fields.#index));
        let out = mode.expr(&quote!(field));
        let method_lt = if has_lifetime { TokenStream::new() } else { quote!(#db_lt,) };
        let field_ty = &it.db;
        getters.extend(quote! {
            #(#attrs)*
            #vis fn #getter<#method_lt #database>(self, db: &#db_lt #database) -> #out_ty
            where
                #database: ?::core::marker::Sized + #db::Database,
            {
                let fields = #plumbing::interned_fields::<#config>(
                    #db::Database::storage(db),
                    self.0,
                );
                let field: &#db_lt #field_ty = #value;
                #out
            }
        });
    }
    let (impl_lt, method_lt) = if has_lifetime {
        (quote!(<#db_lt>), TokenStream::new())
    } else {
        (TokenStream::new(), quote!(#db_lt,))
    };

    let debug = options.debug.as_ref().map(|_| {
        // alloc: expansion time, once per annotated item; no query runs this
        let names = fields.iter().map(|it| it.ident.to_string());
        let loads =
            both.iter().zip(&indices).map(|(it, index)| it.load_ref(&quote!(&fields.#index)));
        let impl_generics = struct_lt.map(|lt| quote!(<#lt>));
        quote! {
            impl #impl_generics ::core::fmt::Debug for #self_ty {
                fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    let this = *self;
                    #db::with_attached(|storage| match storage {
                        ::core::option::Option::Some(storage)
                            if #plumbing::owner_is(storage, this.0, &#ingredient) =>
                        {
                            let fields = #plumbing::interned_fields::<#config>(storage, this.0);
                            f.debug_struct(::core::stringify!(#ident))
                                #(.field(#names, #loads))*
                                .finish()
                        }
                        _ => f.debug_tuple(::core::stringify!(#ident)).field(&this.0).finish(),
                    })
                }
            }
        }
    });

    let ids_of = structs::ids_of(fields.len());
    let declaration = structs::declaration(item, struct_lt);
    let id_impls = structs::id_impls(ident, struct_lt, &ingredient);
    Ok(quote! {
        #declaration

        #[allow(non_camel_case_types, non_snake_case, dead_code, clippy::all)]
        const _: () = {
            struct #config;
            static #vtable: #plumbing::Vtable = #plumbing::Vtable::interned::<#config>();
            static #ingredient: #plumbing::Ingredient = #plumbing::Ingredient::new(&#vtable);

            impl #plumbing::Interned for #config {
                type Fields = (#(#stored,)*);
                const NAME: &'static str = ::core::stringify!(#ident);
                const FILE: &'static str = ::core::file!();
                #[inline]
                fn ingredient() -> &'static #plumbing::Ingredient {
                    &#ingredient
                }
                #ids_of
            }

            #id_impls
            #checks

            impl #impl_lt #self_ty {
                /// The struct of these fields: the one made before for equal fields.
                #[inline]
                pub fn #constructor<#method_lt #database, #(#lookups),*>(
                    db: &#db_lt #database,
                    #(#field_idents: #lookups),*
                ) -> Self
                where
                    #database: ?::core::marker::Sized + #db::Database,
                    #(#lookups: #plumbing::Lookup<#field_db> + ::core::hash::Hash,)*
                    #(#field_db: #plumbing::HashEqLike<#lookups>,)*
                {
                    let id = #plumbing::intern_by::<#config, _>(
                        #db::Database::storage(db),
                        (#(#field_idents,)*),
                        |key, fields| #eq,
                        |key| (#(#owned,)*),
                    );
                    #make
                }

                #getters
            }

            #debug
        };
    })
}
