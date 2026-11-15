//! `#[blaze_memo::input(singleton, debug)]`: a struct of fields the driver makes while it builds
//! a snapshot, each input one fact of its records; a getter's read lists that fact.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{ItemStruct, Lifetime};

use crate::fields::{self, Field, Kind};
use crate::hygiene::Hygiene;
use crate::options::{Options, Site};
use crate::{plumbing, structs};

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
    let plumbing = plumbing();
    let ident = &item.ident;
    let config = hygiene.ident("Config");
    let kind_static = hygiene.ident("KIND");
    let database = hygiene.ident("D");
    let lt = Lifetime::new("'db", Span::call_site());

    // alloc: expansion time, once per annotated item; no query runs this
    let tys: Vec<&syn::Type> = fields.iter().map(|it| &it.ty).collect();
    // alloc: expansion time, once per annotated item; no query runs this
    let idents: Vec<&syn::Ident> = fields.iter().map(|it| &it.ident).collect();
    let singleton = options.singleton.is_some();

    let mut methods = TokenStream::new();
    for (index, field) in fields.iter().enumerate() {
        let Field { vis, ty, mode, getter, attrs, .. } = field;
        let tuple_index = syn::Index::from(index);
        let out_ty = mode.ty(&lt, ty);
        let out = mode.expr(&quote!(&fields.#tuple_index));
        methods.extend(quote! {
            #(#attrs)*
            #vis fn #getter<#lt, #database>(self, db: &#lt #database) -> #out_ty
            where
                #database: ?::core::marker::Sized + #plumbing::Database,
            {
                let fields: &#lt (#(#tys,)*) =
                    #plumbing::input::<#config>(#plumbing::Database::memo(db), self.0);
                #out
            }
        });
    }

    let makers = if singleton {
        quote! {
            /// Sets the singleton in the session arena of the snapshot being built: the fact
            /// the driver knows it by and its fields.
            pub fn set(
                inputs: &#plumbing::Inputs<'_>,
                fact: #plumbing::Fact,
                #(#idents: #tys),*
            ) {
                #plumbing::set_singleton::<#config>(inputs, fact, (#(#idents,)*));
            }

            /// The singleton, if the driver set it in this snapshot.
            pub fn try_get<#database>(db: &#database) -> ::core::option::Option<Self>
            where
                #database: ?::core::marker::Sized + #plumbing::Database,
            {
                #plumbing::try_singleton::<#config>(#plumbing::Database::memo(db)).map(Self)
            }

            /// The singleton; the driver must have set it.
            #[track_caller]
            pub fn get<#database>(db: &#database) -> Self
            where
                #database: ?::core::marker::Sized + #plumbing::Database,
            {
                Self(#plumbing::singleton::<#config>(#plumbing::Database::memo(db)))
            }
        }
    } else {
        quote! {
            /// Makes the input in the arena of `inputs` (a snapshot being built): the fact the
            /// driver knows it by and its fields.
            pub fn new(
                inputs: &#plumbing::Inputs<'_>,
                fact: #plumbing::Fact,
                #(#idents: #tys),*
            ) -> Self {
                Self(#plumbing::new_input::<#config>(inputs, fact, (#(#idents,)*)))
            }
        }
    };

    let debug = options.debug.as_ref().map(|_| {
        // alloc: expansion time, once per annotated item; no query runs this
        let names = fields.iter().map(|it| it.ident.to_string());
        let indices = (0..fields.len()).map(syn::Index::from);
        quote! {
            impl ::core::fmt::Debug for #ident {
                fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    let this = *self;
                    #plumbing::attached_input::<#config, _>(this.0, |fields| match fields {
                        ::core::option::Option::Some(fields) => f
                            .debug_struct(::core::stringify!(#ident))
                            .field("[id]", &this.0)
                            #(.field(#names, &fields.#indices))*
                            .finish(),
                        ::core::option::Option::None => f
                            .debug_struct(::core::stringify!(#ident))
                            .field("[id]", &this.0)
                            .finish(),
                    })
                }
            }
        }
    });

    let declaration = structs::declaration(item, None);
    let id_impls = structs::id_impls(ident, None);
    Ok(quote! {
        #declaration

        #[allow(non_camel_case_types, dead_code, clippy::all)]
        const _: () = {
            static #kind_static: #plumbing::Kind = #plumbing::Kind::new(
                ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#ident)),
                #plumbing::CycleKind::Panic,
            );

            struct #config;

            impl #plumbing::Input for #config {
                type Fields = (#(#tys,)*);
                const KIND: &'static #plumbing::Kind = &#kind_static;
            }

            #id_impls

            impl #ident {
                #makers
                #methods
            }

            #debug
        };
    })
}
