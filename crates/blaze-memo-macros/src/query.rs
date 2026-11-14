//! What a tracked function and a tracked method share: the struct named for the query, its
//! key (the arguments after `db`, stored as they are, nothing interned), its kind and `Query`
//! impl, and the fetch the public function calls.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Attribute, Expr, Ident, Lifetime, Type, Visibility};

use crate::hygiene::Hygiene;
use crate::lifetimes::Both;
use crate::options::Options;
use crate::plumbing;
use crate::returns::Mode;

pub(crate) struct Arg {
    pub(crate) ident: Ident,
    pub(crate) ty: Both,
}

pub(crate) struct Query<'a> {
    pub(crate) options: &'a Options,
    /// The struct named for the query: `f`, or `TypeName_method` in a tracked `impl`.
    pub(crate) name: Ident,
    /// The query's name, before the module path: `f`, or `TypeName::method`.
    pub(crate) query_name: TokenStream,
    pub(crate) vis: Visibility,
    /// The `cfg` attributes of the function, which every generated item carries too.
    pub(crate) cfgs: Vec<Attribute>,
    pub(crate) db_lt: Lifetime,
    pub(crate) db_ty: Type,
    pub(crate) args: Vec<Arg>,
    pub(crate) output: Both,
    /// The user's body run on `db` and the arguments (`Arg::ident`s, database forms): an
    /// expression of the output's database form.
    pub(crate) call: TokenStream,
    /// Items the `call` needs, put in the same `const _` block: a free function's body.
    pub(crate) inner: TokenStream,
    /// Cycle options with `Self` replaced: they are called outside the `impl`.
    pub(crate) cycle_fn: Option<Expr>,
    pub(crate) cycle_initial: Option<Expr>,
    pub(crate) cycle_result: Option<Expr>,
    pub(crate) hygiene: &'a Hygiene,
}

impl Query<'_> {
    pub(crate) fn mode(&self) -> Mode {
        self.options.returns.as_ref().map_or(Mode::Ref, |(_, mode)| *mode)
    }

    /// The return type the public function declares.
    pub(crate) fn public_output(&self) -> TokenStream {
        self.mode().ty(&self.db_lt, &self.output.db)
    }

    /// The public function's body: the fetch, then the return mode.
    pub(crate) fn public_body(&self, fetch_args: &[TokenStream]) -> TokenStream {
        let name = &self.name;
        let value = quote!(value);
        let handed = self.mode().expr(&value);
        quote! {
            let #value = #name::__blaze_memo_fetch(#(#fetch_args),*);
            #handed
        }
    }

    pub(crate) fn expand(&self) -> TokenStream {
        let plumbing = plumbing();
        let Query { name, query_name, vis, cfgs, db_lt, db_ty, args, output, call, inner, .. } =
            self;
        let key = self.hygiene.ident("Key");
        let query = self.hygiene.ident("Q");
        let kind_static = self.hygiene.ident("KIND");

        // alloc: expansion time, once per annotated item; no query runs this
        let idents: Vec<&Ident> = args.iter().map(|it| &it.ident).collect();
        // alloc: expansion time, once per annotated item; no query runs this
        let db_tys: Vec<&Type> = args.iter().map(|it| &it.ty.db).collect();
        // alloc: expansion time, once per annotated item; no query runs this
        let stored_tys: Vec<&Type> = args.iter().map(|it| &it.ty.stored).collect();
        // alloc: expansion time, once per annotated item; no query runs this
        let indices: Vec<syn::Index> = (0..args.len()).map(syn::Index::from).collect();
        let stores = args.iter().map(|it| {
            let ident = &it.ident;
            it.ty.store(&quote!(#ident))
        });
        let loads = args.iter().zip(&indices).map(|(it, index)| {
            let ident = &it.ident;
            let load = it.ty.load(&quote!(key.#index));
            quote!(let #ident = #load;)
        });
        // The arguments back from the key, in their database forms.
        let unpack = quote!(#(#loads)*);
        let writes = indices.iter().map(|index| {
            let comma = (index.index > 0).then(|| quote!(out.write_str(", ")?;));
            quote! {
                #comma
                (&&#plumbing::KeyText(&self.#index)).write_key(out)?;
            }
        });

        let stored_output = &output.stored;
        let store_output = |value: TokenStream| output.store(&value);
        let ran = store_output(quote!(value));
        let (kind, recovery) = match (&self.cycle_result, &self.cycle_initial) {
            (Some(result), _) => {
                let value = store_output(quote!((#result)(db, #(#idents),*)));
                (
                    quote!(Result),
                    quote! {
                        let fallback = || -> #stored_output { #unpack #value };
                        let recovery: #plumbing::Recovery<'_, #stored_output> =
                            #plumbing::Recovery::Result(&fallback);
                    },
                )
            }
            (None, Some(initial)) => {
                let value = store_output(quote!((#initial)(db, #(#idents),*)));
                let recover = match &self.cycle_fn {
                    Some(recover) => {
                        let last = output.load_ref(&quote!(last));
                        let new = output.load(&quote!(new));
                        let value =
                            store_output(quote!((#recover)(db, cycle, #last, #new, #(#idents),*)));
                        quote! {
                            let recover = |
                                cycle: &#plumbing::Cycle<'_>,
                                last: &#stored_output,
                                new: #stored_output,
                            | -> #stored_output {
                                #unpack
                                #value
                            };
                            let recover: ::core::option::Option<
                                &dyn ::core::ops::Fn(
                                    &#plumbing::Cycle<'_>,
                                    &#stored_output,
                                    #stored_output,
                                ) -> #stored_output,
                            > = ::core::option::Option::Some(&recover);
                        }
                    }
                    None => quote!(let recover = ::core::option::Option::None;),
                };
                (
                    quote!(Fixpoint),
                    quote! {
                        let initial = || -> #stored_output { #unpack #value };
                        #recover
                        let recovery: #plumbing::Recovery<'_, #stored_output> =
                            #plumbing::Recovery::Fixpoint {
                            initial: &initial,
                            recover,
                            same: |a: &#stored_output, b: &#stored_output| a == b,
                        };
                    },
                )
            }
            (None, None) => (
                quote!(Panic),
                quote! {
                    let recovery: #plumbing::Recovery<'_, #stored_output> =
                        #plumbing::Recovery::Panic;
                },
            ),
        };

        let loaded = output.load_ref(&quote!(value));
        let output_db = &output.db;
        let homes = indices.iter().map(|index| quote!(#plumbing::Home::home(&self.#index, homes);));
        // `#[tracked(image)]`: the key's stable form (each argument's, in order) and the image
        // codec passed to the fetch; without it the fetch gets `None` and nothing is filed.
        // `owns`: the value may own memory, dropped with its arena; else the no-drop check,
        // here at item level (seen by `cargo check`: the key and value are never generic) as
        // well as at monomorphization in `fetch`.
        let owns = self.options.owns.is_some();
        // `owns` with `cycle_initial` too: every round's value stays where it was placed and is
        // dropped once with its arena (`blaze_memo::plumbing::Query::OWNS`).
        let owns_items = if owns {
            // Counted from the source by blaze-memo's `owns` ratchet (tests/owns.rs), by the
            // attribute's last path segment, however it is imported.
            quote!()
        } else {
            quote! {
                const _: () = ::core::assert!(
                    !::core::mem::needs_drop::<#stored_output>(),
                    ::core::concat!(
                        "the value of ",
                        ::core::module_path!(),
                        "::",
                        #query_name,
                        " owns memory to drop: an arena frees values without a destructor, so \
                         hold `&'db [T]` from `blaze_memo::keep_slice` or ids, or mark the query \
                         `owns` (counted debt)"
                    )
                );
            }
        };
        // `mem_tag = Nameres`: the run's frame's tag, a variant of `blaze_mem::Tag` (as
        // blaze-query took it); `Tag::Query` without it.
        let tag = match &self.options.mem_tag {
            Some(tag) => quote! {{
                #[allow(unused_imports)]
                use #plumbing::Tag::*;
                #tag
            }},
            None => quote!(#plumbing::Tag::Query),
        };
        let imaged_static = self.hygiene.ident("IMAGED");
        let (image_items, image_arg) = match &self.options.image {
            Some(_) => {
                let stables = indices
                    .iter()
                    .map(|index| quote!(#plumbing::Stable::stable(&self.#index, memo, out);));
                (
                    quote! {
                        impl #plumbing::Stable for #key {
                            fn stable(
                                &self,
                                memo: &#plumbing::Memo,
                                out: &mut #plumbing::StableKey,
                            ) {
                                #(#stables)*
                            }
                        }

                        const #imaged_static: #plumbing::Imaged<#key, #stored_output> =
                            #plumbing::Imaged::new();
                    },
                    quote!(::core::option::Option::Some(&#imaged_static)),
                )
            }
            None => (quote!(), quote!(::core::option::Option::None)),
        };

        quote! {
            #[doc(hidden)]
            #[allow(non_camel_case_types)]
            #(#cfgs)*
            #vis struct #name {
                _priv: ::core::convert::Infallible,
            }

            #(#cfgs)*
            #[allow(
                non_camel_case_types,
                non_snake_case,
                dead_code,
                unused_lifetimes,
                unused_variables,
                clippy::all,
            )]
            const _: () = {
                #inner

                /// The arguments after `db`, stored as they are.
                #[derive(Clone, Copy, PartialEq, Eq, Hash)]
                struct #key(#(#stored_tys,)*);

                impl #plumbing::Home for #key {
                    #[inline]
                    fn home(&self, homes: &mut #plumbing::Homes<'_>) {
                        #(#homes)*
                    }
                }

                impl ::core::fmt::Debug for #key {
                    fn fmt(&self, out: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                        #[allow(unused_imports)]
                        use ::core::fmt::Write as _;
                        #[allow(unused_imports)]
                        use #plumbing::{KeyTextDebug as _, KeyTextOther as _};
                        #(#writes)*
                        ::core::result::Result::Ok(())
                    }
                }

                static #kind_static: #plumbing::Kind = #plumbing::Kind::new(
                    ::core::concat!(::core::module_path!(), "::", #query_name),
                    #plumbing::CycleKind::#kind,
                );

                #image_items

                #owns_items

                struct #query;

                impl #plumbing::Query for #query {
                    type Key = #key;
                    type Value = #stored_output;
                    const KIND: &'static #plumbing::Kind = &#kind_static;
                    const OWNS: bool = #owns;
                    const TAG: #plumbing::Tag = #tag;
                }

                impl #name {
                    /// The arguments (stored form) of the entry a read row names, when the row
                    /// is a `Read::Query` of this query in `memo` (a fact boundary's,
                    /// `Builder::mark_boundary`); `None` for any other row.
                    #[allow(dead_code)]
                    #vis fn key_of(
                        memo: &#plumbing::Memo,
                        row: #plumbing::Read,
                    ) -> ::core::option::Option<(#(#stored_tys,)*)> {
                        match row {
                            #plumbing::Read::Query { kind, key }
                                if ::core::ptr::eq(kind.kind(), &#kind_static) =>
                            {
                                #plumbing::boundary_key::<#key>(memo, &#kind_static, key)
                                    .map(|it| (#(it.#indices,)*))
                            }
                            _ => ::core::option::Option::None,
                        }
                    }

                    /// This query's kind name, what `Builder::mark_boundary` takes.
                    #[allow(dead_code)]
                    #vis const KIND_NAME: &'static str =
                        ::core::concat!(::core::module_path!(), "::", #query_name);

                    #[inline]
                    #[allow(clippy::needless_lifetimes, clippy::too_many_arguments)]
                    fn __blaze_memo_fetch<#db_lt>(
                        db: &#db_lt #db_ty,
                        #(#idents: #db_tys,)*
                    ) -> &#db_lt #output_db {
                        let key = #key(#(#stores,)*);
                        let run = || -> #stored_output {
                            #unpack
                            let value = #call;
                            #ran
                        };
                        #recovery
                        let value: &#db_lt #stored_output = #plumbing::fetch::<#query>(
                            #plumbing::Database::memo(db),
                            key,
                            &run,
                            &recovery,
                            #image_arg,
                        );
                        #loaded
                    }
                }

                impl #plumbing::Logged for #name {
                    fn ran_with(ran: &#plumbing::Ran, args: &dyn ::core::any::Any) -> bool {
                        let ::core::option::Option::Some(args) =
                            args.downcast_ref::<(#(#stored_tys,)*)>()
                        else {
                            #plumbing::wrong_args(
                                #kind_static.name(),
                                ::core::any::type_name::<(#(#stored_tys,)*)>(),
                            )
                        };
                        #plumbing::ran_key::<#key>(ran, &#kind_static)
                            .is_some_and(|it| true #(&& it.#indices == args.#indices)*)
                    }
                }
            };
        }
    }
}
