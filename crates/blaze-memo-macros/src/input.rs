//! `#[blaze_memo::input(singleton, debug, parts)]`: a struct of fields the driver makes while it
//! builds a snapshot, each input one fact of its records; a getter's read lists that fact (none
//! with `parts`: an image read item by item, whose accessors list each item's fact).

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
    let parts = options.parts.is_some();

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
        if !singleton {
            let getter_mut = quote::format_ident!("{}_mut", getter);
            methods.extend(quote! {
                /// The field to write in place, in a snapshot built and not yet run
                /// (`Memo::input_mut`, which refuses any other).
                #[track_caller]
                #vis fn #getter_mut(self, memo: &mut #plumbing::Memo) -> &mut #ty {
                    &mut memo.input_mut::<#config>(self.0).#tuple_index
                }
            });
        }
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
        quote! {
            // An input's print is its kind and id, attached or not. Its fields are its value
            // (a crate's data, a file's text), which the host sets and changes, not what it
            // is: an interned struct's fields are its identity and are printed (one level,
            // bounded); an input's are not, so a key that names a crate costs a message a few
            // bytes and never a kilobyte of the crate's data. A message that wants the
            // crate's name or the file's path prints that.
            impl ::core::fmt::Debug for #ident {
                fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    f.debug_struct(::core::stringify!(#ident)).field("[id]", &self.0).finish()
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
                const PARTS: bool = #parts;
            }

            #id_impls

            /// An input is known across snapshots by the fact the driver made it with.
            impl #plumbing::Stable for #ident {
                fn stable(&self, memo: &#plumbing::Memo, out: &mut #plumbing::StableKey) {
                    out.id(&#plumbing::input_fact::<#config>(memo, self.0).id);
                }
            }

            impl #ident {
                #makers
                #methods
            }

            // The input's kind and fact by its own name: the `Input` impl is of a type hidden
            // here (its `Fields` would put the struct's private field types in a public
            // interface), so a caller that classifies a read row asks the struct.
            impl #ident {
                /// This input's kind: what a `Read::Input` row of one carries.
                pub const INPUT_KIND: &'static #plumbing::Kind = &#kind_static;

                /// The fact the driver made this input with. Lists no read and notes no borrow
                /// (`input_fact`).
                pub fn input_fact(self, memo: &#plumbing::Memo) -> #plumbing::Fact {
                    #plumbing::input_fact::<#config>(memo, self.0)
                }
            }

            #debug
        };
    })
}
