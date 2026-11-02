//! `#[blaze_query::input(singleton, debug)]`: a struct of fields set from outside the queries,
//! each read with its own edge.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{ItemStruct, Lifetime, ext::IdentExt as _};

use crate::fields::{self, Field, Kind};
use crate::hygiene::Hygiene;
use crate::options::{Options, Site};
use crate::{db, plumbing, structs};

pub(crate) fn expand(
    args: TokenStream,
    item: &ItemStruct,
) -> Result<TokenStream, (syn::Error, TokenStream)> {
    expand_in(args, item).map_err(|error| (error, structs::stripped(item)))
}

fn expand_in(args: TokenStream, item: &ItemStruct) -> syn::Result<TokenStream> {
    let options = Options::parse(Site::Input, args)?;
    if !item.generics.params.is_empty() || item.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            &item.generics,
            "an input has no lifetime or type parameters",
        ));
    }
    let fields = fields::parse(Kind::Input, item)?;
    let hygiene = Hygiene::of(item);
    let db = db();
    let plumbing = plumbing();
    let ident = &item.ident;
    let config = hygiene.ident("Configuration");
    let vtable = hygiene.ident("VTABLE");
    let ingredient = hygiene.ident("INGREDIENT");
    // alloc: expansion time, once per annotated item; no query runs this
    let builder = hygiene.ident(&format!("{ident}Builder"));
    let database = hygiene.ident("D");
    let lt = Lifetime::new("'db", proc_macro2::Span::call_site());

    let count = fields.len();
    let count_u32 = u32::try_from(count).map_err(|_| {
        syn::Error::new_spanned(&item.ident, "an input has at most `u32::MAX` fields")
    })?;
    // alloc: expansion time, once per annotated item; no query runs this
    let tys: Vec<&syn::Type> = fields.iter().map(|it| &it.ty).collect();
    // alloc: expansion time, once per annotated item; no query runs this
    let field_names = fields.iter().map(|it| it.ident.unraw().to_string());
    let singleton = options.singleton.is_some();

    // alloc: expansion time, once per annotated item; no query runs this
    let required: Vec<&Field> = fields.iter().filter(|it| !it.default).collect();
    // alloc: expansion time, once per annotated item; no query runs this
    let required_idents: Vec<&syn::Ident> = required.iter().map(|it| &it.ident).collect();
    // alloc: expansion time, once per annotated item; no query runs this
    let required_tys: Vec<&syn::Type> = required.iter().map(|it| &it.ty).collect();
    let initial = fields.iter().map(|it| {
        let ident = &it.ident;
        if it.default { quote!(::core::default::Default::default()) } else { quote!(#ident) }
    });

    let mut methods = TokenStream::new();
    let mut builder_methods = TokenStream::new();
    let mut at_fns = TokenStream::new();
    for (index, field) in fields.iter().enumerate() {
        let Field { ident: field_ident, vis, ty, mode, getter, setter, attrs, .. } = field;
        let tuple_index = syn::Index::from(index);
        let number = u32::try_from(index).expect("fewer than `u32::MAX` fields");
        let out_ty = mode.ty(&lt, ty);
        let out = mode.expr(&quote!(&fields.#tuple_index));
        // alloc: expansion time, once per annotated item; no query runs this
        let at = hygiene.ident(&format!("at_{index}"));
        methods.extend(quote! {
            #(#attrs)*
            #vis fn #getter<#lt, #database>(self, db: &#lt #database) -> #out_ty
            where
                #database: ?::core::marker::Sized + #db::Database,
            {
                let storage = #db::Database::storage(db);
                #plumbing::input_read::<#config>(storage, self.0, #number);
                let fields = #plumbing::input_fields::<#config>(storage, self.0);
                #out
            }

            #[must_use]
            #vis fn #setter<#lt, #database>(
                self,
                db: &#lt mut #database,
            ) -> impl #db::Setter<FieldTy = #ty> + #lt
            where
                #database: ?::core::marker::Sized + #db::Database,
            {
                #plumbing::input_set::<#config, #ty>(
                    #db::Database::storage_mut(db),
                    self.0,
                    #number,
                    #at,
                )
            }
        });
        at_fns.extend(quote! {
            fn #at<'fields>(fields: &'fields mut (#(#tys,)*)) -> &'fields mut #ty {
                &mut fields.#tuple_index
            }
        });
        if field.default {
            builder_methods.extend(quote! {
                /// Sets the field's value.
                #[must_use]
                pub fn #field_ident(mut self, value: #ty) -> Self {
                    self.fields.#tuple_index = value;
                    self
                }
            });
        }
        let durability = format_ident!("{}_durability", field_ident);
        builder_methods.extend(quote! {
            /// Sets the field's durability.
            #[must_use]
            pub fn #durability(mut self, durability: #db::Durability) -> Self {
                self.durabilities[#index] = durability;
                self
            }
        });
    }

    if singleton {
        methods.extend(quote! {
            /// The input, if it was made.
            pub fn try_get<#database>(db: &#database) -> ::core::option::Option<Self>
            where
                #database: ?::core::marker::Sized + #db::Database,
            {
                #plumbing::input_singleton::<#config>(#db::Database::storage(db)).map(Self)
            }

            /// The input; it must have been made.
            #[track_caller]
            pub fn get<#database>(db: &#database) -> Self
            where
                #database: ?::core::marker::Sized + #db::Database,
            {
                match Self::try_get(db) {
                    ::core::option::Option::Some(it) => it,
                    ::core::option::Option::None => ::core::panic!(
                        "the singleton input `{}` was never made",
                        <#config as #plumbing::Input>::NAME,
                    ),
                }
            }
        });
    }

    let debug = options.debug.as_ref().map(|_| {
        // alloc: expansion time, once per annotated item; no query runs this
        let names = fields.iter().map(|it| it.ident.to_string());
        let indices = (0..count).map(syn::Index::from);
        quote! {
            impl ::core::fmt::Debug for #ident {
                fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    let this = *self;
                    #db::with_attached(|storage| match storage {
                        ::core::option::Option::Some(storage)
                            if #plumbing::owner_is(storage, this.0, &#ingredient) =>
                        {
                            let fields = #plumbing::input_fields::<#config>(storage, this.0);
                            f.debug_struct(::core::stringify!(#ident))
                                .field("[id]", &this.0)
                                #(.field(#names, &fields.#indices))*
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

    // A singleton's absence is a read (`try_get`), so making it is a write: `&mut`.
    let (db_ref, make) = if singleton {
        let make = quote!(#plumbing::input_new_singleton::<#config, #count>(
            #db::Database::storage_mut(db),
            self.fields,
            self.durabilities,
        ));
        (quote!(&mut), make)
    } else {
        let make = quote!(#plumbing::input_new::<#config, #count>(
            #db::Database::storage(db),
            self.fields,
            self.durabilities,
        ));
        (quote!(&), make)
    };
    let declaration = structs::declaration(item, None);
    let id_impls = structs::id_impls(ident, None, &ingredient);
    Ok(quote! {
        #declaration

        #[allow(non_camel_case_types, dead_code, clippy::all)]
        const _: () = {
            struct #config;
            static #vtable: #plumbing::Vtable = #plumbing::Vtable::input::<#config>();
            static #ingredient: #plumbing::Ingredient = #plumbing::Ingredient::new(&#vtable);

            impl #plumbing::Input for #config {
                type Fields = (#(#tys,)*);
                const NAME: &'static str = ::core::stringify!(#ident);
                const FILE: &'static str = ::core::file!();
                const FIELDS: u32 = #count_u32;
                const FIELD_NAMES: &'static [&'static str] = &[#(#field_names),*];
                const SINGLETON: bool = #singleton;
                #[inline]
                fn ingredient() -> &'static #plumbing::Ingredient {
                    &#ingredient
                }
            }

            #id_impls
            #at_fns

            impl #ident {
                /// Makes the input, every field of durability `LOW`.
                #[inline]
                pub fn new<#database>(db: #db_ref #database, #(#required_idents: #required_tys),*) -> Self
                where
                    #database: ?::core::marker::Sized + #db::Database,
                {
                    Self::builder(#(#required_idents),*).new(db)
                }

                /// A builder of the input, to set durabilities and default fields.
                pub fn builder(#(#required_idents: #required_tys),*) -> #builder {
                    #builder {
                        fields: (#(#initial,)*),
                        durabilities: [#db::Durability::LOW; #count],
                    }
                }

                #methods
            }

            #[must_use]
            pub struct #builder {
                fields: (#(#tys,)*),
                durabilities: [#db::Durability; #count],
            }

            impl #builder {
                /// Sets the durability of every field.
                #[must_use]
                pub fn durability(mut self, durability: #db::Durability) -> Self {
                    self.durabilities = [durability; #count];
                    self
                }

                #builder_methods

                /// Makes the input.
                #[must_use]
                pub fn new<#database>(self, db: #db_ref #database) -> #ident
                where
                    #database: ?::core::marker::Sized + #db::Database,
                {
                    #ident(#make)
                }
            }

            #debug
        };
    })
}
