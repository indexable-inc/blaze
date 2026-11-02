//! What a tracked function and a tracked method share: the struct named for the query, its
//! `Function` configuration, the hidden key of its arguments, and the fetch the public function
//! calls.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Attribute, Expr, Ident, Lifetime, Type, Visibility};

use crate::hygiene::Hygiene;
use crate::lifetimes::Both;
use crate::options::Options;
use crate::returns::Mode;
use crate::{db, plumbing, value};

pub(crate) struct Arg {
    pub(crate) ident: Ident,
    pub(crate) ty: Both,
}

pub(crate) struct Query<'a> {
    pub(crate) options: &'a Options,
    /// The struct named for the query: `f`, or `TypeName_method` in a tracked `impl`.
    pub(crate) name: Ident,
    /// `Function::NAME`.
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
            let #value = #name::__blaze_query_fetch(#(#fetch_args),*);
            #handed
        }
    }

    /// Whether the key is the one argument itself (an ID), not a hidden interned tuple.
    fn direct_key(&self) -> bool {
        self.args.len() == 1
    }

    pub(crate) fn expand(&self) -> TokenStream {
        let plumbing = plumbing();
        let db = db();
        let Query { name, query_name, vis, cfgs, db_lt, db_ty, args, output, call, inner, .. } =
            self;
        let config = self.hygiene.ident("Configuration");
        let key_struct = self.hygiene.ident("Arguments");
        let value_check = self.hygiene.ident("values_are_values");
        let vtable = self.hygiene.ident("VTABLE");
        let ingredient = self.hygiene.ident("INGREDIENT");
        let key_vtable = self.hygiene.ident("KEY_VTABLE");
        let key_ingredient = self.hygiene.ident("KEY_INGREDIENT");

        // alloc: expansion time, once per annotated item; no query runs this
        let idents: Vec<&Ident> = args.iter().map(|it| &it.ident).collect();
        // alloc: expansion time, once per annotated item; no query runs this
        let db_tys: Vec<&Type> = args.iter().map(|it| &it.ty.db).collect();
        // alloc: expansion time, once per annotated item; no query runs this
        let stored_tys: Vec<&Type> = args.iter().map(|it| &it.ty.stored).collect();

        // The key type, how a key is made of the arguments in `fetch`, and how `execute` and
        // the cycle functions get the arguments back from `key`.
        let (key_ty, key_items, make_key, unpack) = if self.direct_key() {
            let arg = &args[0];
            let ident = &arg.ident;
            let stored = &arg.ty.stored;
            let store = arg.ty.store(&quote!(#ident));
            let load = arg.ty.load(&quote!(key));
            (quote!(#stored), TokenStream::new(), store, quote!(let #ident = #load;))
        } else {
            let stores = args.iter().map(|it| {
                let ident = &it.ident;
                it.ty.store(&quote!(#ident))
            });
            let fields = quote!((#(#stored_tys,)*));
            let ids_of = crate::structs::ids_of(args.len());
            let interned_name = quote!(::core::concat!(#query_name, "::interned_arguments"));
            let loads = args.iter().enumerate().map(|(index, it)| {
                let ident = &it.ident;
                let index = syn::Index::from(index);
                let load = it.ty.load(&quote!(::core::clone::Clone::clone(&fields.#index)));
                quote!(let #ident = #load;)
            });
            let unpack = if args.is_empty() {
                quote!(let _ = key;)
            } else {
                quote! {
                    let fields = #plumbing::interned_fields::<#key_struct>(
                        #db::Database::storage(db),
                        #db::AsId::as_id(&key),
                    );
                    #(#loads)*
                }
            };
            let items = quote! {
                #[derive(Clone, Copy)]
                struct #key_struct(#db::Id);
                static #key_vtable: #plumbing::Vtable = #plumbing::Vtable::interned::<#key_struct>();
                static #key_ingredient: #plumbing::Ingredient = #plumbing::Ingredient::new(&#key_vtable);
                impl #plumbing::Interned for #key_struct {
                    type Fields = #fields;
                    const NAME: &'static str = #interned_name;
                    const FILE: &'static str = ::core::file!();
                    #[inline]
                    fn ingredient() -> &'static #plumbing::Ingredient {
                        &#key_ingredient
                    }
                    #ids_of
                }
                impl #db::AsId for #key_struct {
                    #[inline]
                    fn as_id(&self) -> #db::Id {
                        self.0
                    }
                }
                #[doc(hidden)]
                impl #db::FromId for #key_struct {
                    #[inline]
                    fn from_id(id: #db::Id) -> Self {
                        #key_struct(id)
                    }
                }
                impl #db::FromIdIn for #key_struct {
                    #[inline]
                    fn from_id_in(storage: &#db::Storage, id: #db::Id) -> ::core::option::Option<Self> {
                        #plumbing::owner_is(storage, id, &#key_ingredient).then_some(#key_struct(id))
                    }
                }
            };
            let make = quote! {
                #key_struct(#plumbing::intern::<#key_struct>(
                    #db::Database::storage(db),
                    (#(#stores,)*),
                ))
            };
            (quote!(#key_struct), items, make, unpack)
        };

        // alloc: expansion time, once per annotated item; no query runs this
        let mut checked: Vec<&Type> = vec![&output.db];
        if !self.direct_key() {
            checked.extend(db_tys.iter().copied());
        }
        let checks = value::check(&value_check, db_lt, &checked);

        let options = self.options;
        let lru = options.lru.as_ref().map_or(0, |(_, cap)| *cap);
        let evictable = options.lru.is_some();
        let no_eq = options.no_eq.is_some();
        let tag = match &options.mem_tag {
            Some(tag) => quote! {{
                #[allow(unused_imports)]
                use #plumbing::Tag::*;
                #tag
            }},
            None => quote!(#plumbing::Tag::Query),
        };

        let stored_output = &output.stored;
        let store_output = |value: TokenStream| output.store(&value);
        let ran = store_output(quote!(value));
        let kind;
        let cycle_result;
        let cycle_initial;
        let cycle_recover;
        let unused = |what: &str| {
            let message =
                // alloc: expansion time, the panic's message
                format!("{{}} has no `{what}`: the engine called it for another cycle kind");
            quote!(::core::unreachable!(#message, <Self as #plumbing::Function>::NAME))
        };
        match (&self.cycle_result, &self.cycle_initial) {
            (Some(result), _) => {
                kind = quote!(Result);
                let value = store_output(quote!((#result)(db, id, #(#idents),*)));
                cycle_result = quote!({ #unpack #value });
                cycle_initial = unused("cycle_initial");
                cycle_recover = unused("cycle_fn");
            }
            (None, Some(initial)) => {
                kind = quote!(Fixpoint);
                let value = store_output(quote!((#initial)(db, id, #(#idents),*)));
                cycle_result = unused("cycle_result");
                cycle_initial = quote!({ #unpack #value });
                cycle_recover = match &self.cycle_fn {
                    Some(recover) => {
                        let last = output.load_ref(&quote!(last));
                        let new = output.load(&quote!(new));
                        let value =
                            store_output(quote!((#recover)(db, cycle, #last, #new, #(#idents),*)));
                        quote!({ #unpack #value })
                    }
                    None => quote!({
                        let _ = (db, cycle, last, key);
                        new
                    }),
                };
            }
            (None, None) => {
                kind = quote!(Panic);
                cycle_result = unused("cycle_result");
                cycle_initial = unused("cycle_initial");
                cycle_recover = unused("cycle_fn");
            }
        }

        let equal = if no_eq {
            quote!({
                let _ = (old, new);
                false
            })
        } else {
            quote!(old == new)
        };
        let hash = if evictable && !no_eq {
            quote!(#plumbing::value_hash(value))
        } else {
            let message = "{} has no `lru`, or is `no_eq`: its values are never hashed";
            quote!({
                let _ = value;
                ::core::unreachable!(#message, <Self as #plumbing::Function>::NAME)
            })
        };

        let loaded = output.load_ref(&quote!(value));
        let output_db = &output.db;
        let fetch = quote! {
            #[inline]
            #[allow(clippy::needless_lifetimes, clippy::too_many_arguments)]
            fn __blaze_query_fetch<#db_lt>(
                db: &#db_lt #db_ty,
                #(#idents: #db_tys,)*
            ) -> &#db_lt #output_db {
                let key = #make_key;
                let value: &#db_lt #stored_output = #plumbing::fetch::<#config>(db, key);
                #loaded
            }
        };

        let evict = if evictable {
            let evict_of = if self.direct_key() {
                let arg = &args[0];
                let stored = &arg.ty.stored;
                let db_ty_arg = &arg.ty.db;
                let store = arg.ty.store(&quote!(key));
                quote! {
                    /// Drops the values of these keys, keeping what verifies them.
                    #vis fn evict_values_of<#db_lt>(
                        db: &mut #db_ty,
                        keys: impl ::core::iter::IntoIterator<Item = #db_ty_arg>,
                    ) -> usize {
                        // alloc: once per eviction pass, under `&mut`, never on a query's path
                        let keys: ::std::vec::Vec<#stored> =
                            // alloc: as above
                            keys.into_iter().map(|key| #store).collect();
                        #plumbing::evict_values_of::<#config>(db, &keys)
                    }
                }
            } else {
                TokenStream::new()
            };
            quote! {
                /// Drops every value of the query, keeping what verifies them.
                #vis fn evict_values(db: &mut #db_ty) -> usize {
                    #plumbing::evict_values::<#config>(db)
                }
                #evict_of

                /// Replaces the query's value cap (0: none) from the next view.
                #vis fn set_lru_capacity(db: &mut #db_ty, capacity: usize) {
                    #plumbing::set_lru_capacity::<#config>(db, capacity)
                }
            }
        } else {
            TokenStream::new()
        };

        let arguments = if args.len() >= 2 {
            let loads = args.iter().enumerate().map(|(index, it)| {
                let index = syn::Index::from(index);
                it.ty.load(&quote!(::core::clone::Clone::clone(&fields.#index)))
            });
            quote! {
                /// The arguments of the key `id`, or `None` when `id` is no key of this query.
                #vis fn arguments<#db_lt>(
                    db: &#db_lt #db_ty,
                    id: #db::Id,
                ) -> ::core::option::Option<(#(#db_tys,)*)> {
                    let storage = #db::Database::storage(db);
                    if !#plumbing::owner_is(storage, id, &#key_ingredient) {
                        return ::core::option::Option::None;
                    }
                    let fields = #plumbing::interned_fields::<#key_struct>(storage, id);
                    ::core::option::Option::Some((#(#loads,)*))
                }
            }
        } else {
            TokenStream::new()
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
                #key_items

                struct #config;
                static #vtable: #plumbing::Vtable = #plumbing::Vtable::function::<#config>();
                static #ingredient: #plumbing::Ingredient = #plumbing::Ingredient::new(&#vtable);

                #checks

                impl #plumbing::Function for #config {
                    type Db = #db_ty;
                    type Key = #key_ty;
                    type Value = #stored_output;
                    const NAME: &'static str = #query_name;
                    const FILE: &'static str = ::core::file!();
                    const LRU: usize = #lru;
                    const EVICTABLE: bool = #evictable;
                    const NO_EQ: bool = #no_eq;
                    const CYCLE: #plumbing::CycleKind = #plumbing::CycleKind::#kind;
                    const TAG: #plumbing::Tag = #tag;

                    #[inline]
                    fn ingredient() -> &'static #plumbing::Ingredient {
                        &#ingredient
                    }

                    fn execute<#db_lt>(db: &#db_lt Self::Db, key: Self::Key) -> Self::Value {
                        #unpack
                        let value = #call;
                        #ran
                    }

                    fn cycle_result<#db_lt>(
                        db: &#db_lt Self::Db,
                        id: #db::Id,
                        key: Self::Key,
                    ) -> Self::Value {
                        #cycle_result
                    }

                    fn cycle_initial<#db_lt>(
                        db: &#db_lt Self::Db,
                        id: #db::Id,
                        key: Self::Key,
                    ) -> Self::Value {
                        #cycle_initial
                    }

                    fn cycle_recover<#db_lt>(
                        db: &#db_lt Self::Db,
                        cycle: &#db::Cycle<'_>,
                        last: &Self::Value,
                        new: Self::Value,
                        key: Self::Key,
                    ) -> Self::Value {
                        #cycle_recover
                    }

                    #[inline]
                    fn equal(old: &Self::Value, new: &Self::Value) -> bool {
                        #equal
                    }

                    fn hash(value: &Self::Value) -> u64 {
                        #hash
                    }
                }

                impl #name {
                    #fetch
                    #evict
                    #arguments
                }
            };
        }
    }
}
