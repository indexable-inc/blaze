//! `#[blaze_query::tracked(debug, constructor = name)]` on a struct: made by a query, its
//! identity the untracked fields (interned with the creating query and occurrence), each
//! `#[tracked]` field a cell the query writes and readers read with an edge.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{ItemStruct, ext::IdentExt as _};

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
    let options = Options::parse(Site::TrackedStruct, args)?;
    let mut lifetimes = item.generics.params.iter();
    let db_lt = match (lifetimes.next(), lifetimes.next()) {
        (Some(syn::GenericParam::Lifetime(it)), None) => &it.lifetime,
        _ => {
            return Err(syn::Error::new_spanned(
                &item.generics,
                "a tracked struct takes one lifetime, the database's: `struct T<'db>`",
            ));
        }
    };
    if let Some(clause) = &item.generics.where_clause {
        return Err(syn::Error::new_spanned(clause, "a tracked struct takes no `where` clause"));
    }
    let fields = fields::parse(Kind::Tracked, item)?;
    let hygiene = Hygiene::of(item);
    let db = db();
    let plumbing = plumbing();
    let ident = &item.ident;
    let config = hygiene.ident("Configuration");
    let vtable = hygiene.ident("VTABLE");
    let ingredient = hygiene.ident("INGREDIENT");
    let database = hygiene.ident("D");
    let value_check = hygiene.ident("fields_are_values");
    let self_ty: syn::Type = syn::parse_quote!(#ident<#db_lt>);

    // alloc: expansion time, once per annotated item; no query runs this
    let both: Vec<Both> = fields.iter().map(|it| Both::of(&it.ty, db_lt)).collect();
    // alloc: expansion time, once per annotated item; no query runs this
    let checked: Vec<&syn::Type> = both.iter().map(|it| &it.db).collect();
    let checks = value::check(&value_check, db_lt, &checked);

    // Each field's place: in the identity or among the tracked fields, and its number there.
    // alloc: expansion time, once per annotated item; no query runs this
    let mut identity_tys = Vec::new();
    // alloc: expansion time, once per annotated item; no query runs this
    let mut tracked_tys = Vec::new();
    // alloc: expansion time, once per annotated item; no query runs this
    let mut places = Vec::new();
    for (field, it) in fields.iter().zip(&both) {
        if field.tracked {
            places.push((true, tracked_tys.len()));
            tracked_tys.push(&it.stored);
        } else {
            places.push((false, identity_tys.len()));
            identity_tys.push(&it.stored);
        }
    }
    let tracked = fields.iter().filter(|it| it.tracked);
    // alloc: expansion time, once per annotated item; no query runs this
    let tracked_names = tracked.map(|it| it.ident.unraw().to_string());
    let tracked_count = u32::try_from(tracked_tys.len())
        .map_err(|_| syn::Error::new_spanned(ident, "too many tracked fields"))?;

    // alloc: expansion time, once per annotated item; no query runs this
    let field_idents: Vec<&syn::Ident> = fields.iter().map(|it| &it.ident).collect();
    // alloc: expansion time, once per annotated item; no query runs this
    let field_db: Vec<&syn::Type> = both.iter().map(|it| &it.db).collect();
    let identity_values = fields.iter().zip(&both).filter(|(it, _)| !it.tracked).map(|(it, b)| {
        let ident = &it.ident;
        b.store(&quote!(#ident))
    });
    let tracked_values = fields.iter().zip(&both).filter(|(it, _)| it.tracked).map(|(it, b)| {
        let ident = &it.ident;
        b.store(&quote!(#ident))
    });

    let mut getters = TokenStream::new();
    // alloc: expansion time, once per annotated item; no query runs this
    let mut debug_fields = Vec::new();
    for ((field, it), (tracked, number)) in fields.iter().zip(&both).zip(&places) {
        let Field { vis, mode, getter, attrs, ident: field_ident, .. } = field;
        let index = syn::Index::from(*number);
        let field_ty = &it.db;
        let out_ty = mode.ty(db_lt, field_ty);
        let value = it.load_ref(&quote!(&fields.#index));
        let out = mode.expr(&quote!(field));
        let read = if *tracked {
            let number = u32::try_from(*number).expect("checked above");
            quote!(#plumbing::tracked_fields::<#config>(storage, self.0, #number))
        } else {
            quote!(#plumbing::tracked_identity::<#config>(storage, self.0))
        };
        getters.extend(quote! {
            #(#attrs)*
            #vis fn #getter<#database>(self, db: &#db_lt #database) -> #out_ty
            where
                #database: ?::core::marker::Sized + #db::Database,
            {
                let storage = #db::Database::storage(db);
                let fields = #read;
                let field: &#db_lt #field_ty = #value;
                #out
            }
        });
        // alloc: expansion time, once per annotated item; no query runs this
        let name = field_ident.to_string();
        let read_debug = if *tracked {
            let number = u32::try_from(*number).expect("checked above");
            quote!(#plumbing::tracked_fields::<#config>(storage, this.0, #number))
        } else {
            quote!(#plumbing::tracked_identity::<#config>(storage, this.0))
        };
        debug_fields.push(quote! {
            .field(#name, {
                let fields = #read_debug;
                #value
            })
        });
    }

    let equal_arms = fields.iter().filter(|it| it.tracked).enumerate().map(|(number, field)| {
        let number = u32::try_from(number).expect("checked above");
        let index = syn::Index::from(number as usize);
        if field.no_eq {
            quote!(#number => false,)
        } else {
            quote!(#number => old.#index == new.#index,)
        }
    });

    let constructor = match &options.constructor {
        Some(it) => it.clone(), // clone: expansion time, an identifier
        None => syn::Ident::new("new", ident.span()),
    };
    let make = structs::construct(Some(db_lt), &quote!(id));

    let debug = options.debug.as_ref().map(|_| {
        quote! {
            impl<#db_lt> ::core::fmt::Debug for #self_ty {
                fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    let this = *self;
                    #db::with_attached(|storage| match storage {
                        ::core::option::Option::Some(storage)
                            if #plumbing::owner_is(storage, this.0, &#ingredient) =>
                        {
                            f.debug_struct(::core::stringify!(#ident))
                                .field("[id]", &this.0)
                                #(#debug_fields)*
                                .finish()
                        }
                        _ => f
                            .debug_struct(::core::stringify!(#ident))
                            .field("[id]", &this.0)
                            .finish(),
                    })
                }
            }
        }
    });

    let declaration = structs::declaration(item, Some(db_lt));
    let id_impls = structs::id_impls(ident, Some(db_lt), &ingredient);
    Ok(quote! {
        #declaration

        #[allow(non_camel_case_types, dead_code, clippy::all)]
        const _: () = {
            struct #config;
            static #vtable: #plumbing::Vtable = #plumbing::Vtable::tracked::<#config>();
            static #ingredient: #plumbing::Ingredient = #plumbing::Ingredient::new(&#vtable);

            impl #plumbing::Tracked for #config {
                type Identity = (#(#identity_tys,)*);
                type Fields = (#(#tracked_tys,)*);
                const NAME: &'static str = ::core::stringify!(#ident);
                const FILE: &'static str = ::core::file!();
                const TRACKED_FIELDS: u32 = #tracked_count;
                const TRACKED_FIELD_NAMES: &'static [&'static str] = &[#(#tracked_names),*];
                #[inline]
                fn ingredient() -> &'static #plumbing::Ingredient {
                    &#ingredient
                }
                fn equal_field(field: u32, old: &Self::Fields, new: &Self::Fields) -> bool {
                    let _ = (old, new);
                    match field {
                        #(#equal_arms)*
                        _ => ::core::unreachable!(
                            "`{}` has no tracked field {}",
                            <Self as #plumbing::Tracked>::NAME,
                            field,
                        ),
                    }
                }
            }

            #id_impls
            #checks

            impl<#db_lt> #self_ty {
                /// Makes the struct: inside a query, which owns it.
                #[inline]
                pub fn #constructor<#database>(
                    db: &#db_lt #database,
                    #(#field_idents: #field_db),*
                ) -> Self
                where
                    #database: ?::core::marker::Sized + #db::Database,
                {
                    let id = #plumbing::tracked_new::<#config>(
                        #db::Database::storage(db),
                        (#(#identity_values,)*),
                        (#(#tracked_values,)*),
                    );
                    #make
                }

                #getters
            }

            #debug
        };
    })
}
