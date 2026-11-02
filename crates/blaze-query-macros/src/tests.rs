//! What each attribute refuses, and that it says so naming the option. What the attributes
//! generate is tested against the engine in `tests/`.

use proc_macro2::TokenStream;
use quote::quote;

use crate::{input_tokens, interned_tokens, tracked_tokens};

/// The text of every `compile_error!` in `tokens`.
fn errors(tokens: &TokenStream) -> String {
    let text = tokens.to_string();
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(at) = rest.find("compile_error") {
        rest = &rest[at..];
        let end = rest.find('}').map_or(rest.len(), |it| it + 1);
        out.push_str(&rest[..end]);
        out.push('\n');
        rest = &rest[end..];
    }
    out
}

#[track_caller]
fn refused(tokens: TokenStream, message: &str) {
    let found = errors(&tokens);
    assert!(found.contains(message), "expected an error containing {message:?}, got: {found:?}");
}

#[track_caller]
fn accepted(tokens: TokenStream) {
    let found = errors(&tokens);
    assert!(found.is_empty(), "expected no error, got: {found}");
    syn::parse2::<syn::File>(tokens).expect("the expansion parses as items");
}

fn query() -> TokenStream {
    quote!(
        fn f(db: &Db, a: A) -> V {
            todo_body(db, a)
        }
    )
}

#[test]
fn options_blaze_query_does_not_have_are_unknown() {
    for option in ["specify", "persist", "heap_size = h", "db = D", "accumulator"] {
        let option: TokenStream = option.parse().unwrap();
        let name = option.to_string();
        let name = name.split_whitespace().next().unwrap().to_owned();
        refused(tracked_tokens(option, query()), &format!("unknown option `{name}`"));
    }
}

#[test]
fn unknown_options_are_refused() {
    refused(tracked_tokens(quote!(frobnicate), query()), "unknown option `frobnicate`");
    refused(
        interned_tokens(
            quote!(unsafe(non_update_types)),
            quote!(
                struct S {
                    a: u32,
                }
            ),
        ),
        "`unsafe(non_update_types)` is not an option",
    );
}

#[test]
fn an_option_given_twice_is_refused() {
    refused(tracked_tokens(quote!(lru = 1, lru = 2), query()), "option `lru` is given twice");
}

#[test]
fn options_of_another_site_are_refused() {
    refused(tracked_tokens(quote!(singleton), query()), "`singleton` is not an option");
    refused(
        input_tokens(
            quote!(lru = 3),
            quote!(
                struct S {
                    a: u32,
                }
            ),
        ),
        "`lru` is not an option of `#[blaze_query::input]`",
    );
    refused(
        interned_tokens(
            quote!(singleton),
            quote!(
                struct S {
                    a: u32,
                }
            ),
        ),
        "`singleton` is not an option",
    );
}

#[test]
fn cycle_options_combine_one_way_out() {
    refused(tracked_tokens(quote!(cycle_fn = g), query()), "`cycle_fn` needs `cycle_initial`");
    refused(
        tracked_tokens(quote!(cycle_result = r, cycle_initial = i), query()),
        "`cycle_result` is one way out of a cycle",
    );
    refused(
        tracked_tokens(quote!(no_eq, cycle_initial = i, cycle_fn = g), query()),
        "`no_eq` can't be used with `cycle_fn`",
    );
    accepted(tracked_tokens(quote!(cycle_initial = |_, _, _| 0, cycle_fn = g), query()));
}

#[test]
fn a_return_mode_is_one_of_six() {
    refused(tracked_tokens(quote!(returns(owned)), query()), "`returns` is one of");
    for mode in ["ref", "copy", "clone", "deref", "as_ref", "as_deref"] {
        let mode: TokenStream = mode.parse().unwrap();
        accepted(tracked_tokens(quote!(returns(#mode)), query()));
    }
}

#[test]
fn every_option_a_tracked_function_takes_expands() {
    accepted(tracked_tokens(
        quote!(returns(ref), lru = 0, mem_tag = Nameres, no_eq, self_ty = Ty),
        query(),
    ));
    accepted(tracked_tokens(quote!(cycle_result = crate::r), query()));
    accepted(tracked_tokens(
        quote!(),
        quote!(
            fn f(db: &Db) -> V {
                v()
            }
        ),
    ));
    accepted(tracked_tokens(
        quote!(),
        quote!(
            pub(crate) fn f<'db>(db: &'db Db, a: A<'db>, b: B) -> V<'db> {
                v()
            }
        ),
    ));
}

#[test]
fn a_tracked_function_has_a_database_and_no_type_parameters() {
    refused(
        tracked_tokens(
            quote!(),
            quote!(
                fn f() -> V {
                    v()
                }
            ),
        ),
        "takes the database",
    );
    refused(
        tracked_tokens(
            quote!(),
            quote!(
                fn f(db: Db) -> V {
                    v()
                }
            ),
        ),
        "a shared reference",
    );
    refused(
        tracked_tokens(
            quote!(),
            quote!(
                fn f<T>(db: &Db, t: T) -> V {
                    v()
                }
            ),
        ),
        "no type or const parameters",
    );
    refused(
        tracked_tokens(
            quote!(),
            quote!(
                async fn f(db: &Db) -> V {
                    v()
                }
            ),
        ),
        "can't be `async`",
    );
}

#[test]
fn a_tracked_impl_is_inherent_and_its_methods_take_self_by_value() {
    refused(
        tracked_tokens(
            quote!(),
            quote!(impl Tr for S { #[blaze_query::tracked] fn m(self, db: &Db) {} }),
        ),
        "an inherent `impl`",
    );
    refused(
        tracked_tokens(
            quote!(),
            quote!(impl S { #[blaze_query::tracked] fn m(&self, db: &Db) {} }),
        ),
        "takes `self` by value",
    );
    accepted(tracked_tokens(
        quote!(),
        quote!(
            impl<'db> S<'db> {
                #[blaze_query::tracked(returns(ref))]
                fn m(self, db: &'db Db, x: X) -> V {
                    Self::helper()
                }
                #[blaze_query::tracked]
                fn assoc(db: &Db) -> V {
                    v()
                }
                fn helper() -> V {
                    v()
                }
            }
        ),
    ));
}

#[test]
fn field_attributes_belong_to_their_struct() {
    refused(
        input_tokens(
            quote!(),
            quote!(
                struct S {
                    #[tracked]
                    a: u32,
                }
            ),
        ),
        "`#[tracked]` is not a field attribute of `#[blaze_query::input]`",
    );
    refused(
        interned_tokens(
            quote!(),
            quote!(
                struct S {
                    #[default]
                    a: u32,
                }
            ),
        ),
        "`#[default]` is not a field attribute",
    );
    refused(
        tracked_tokens(
            quote!(),
            quote!(
                struct S<'db> {
                    #[no_eq]
                    a: u32,
                }
            ),
        ),
        "`#[no_eq]` goes on a `#[tracked]` field",
    );
    refused(
        input_tokens(
            quote!(),
            quote!(
                struct S {
                    new: u32,
                }
            ),
        ),
        "can't be named `new`",
    );
}

#[test]
fn every_struct_form_expands() {
    accepted(input_tokens(
        quote!(singleton, debug),
        quote!(
            pub struct S {
                /// A field.
                #[returns(copy)]
                #[get(get_a)]
                #[set(put_a)]
                pub a: u32,
                #[default]
                b: String,
            }
        ),
    ));
    accepted(interned_tokens(
        quote!(debug, constructor = make),
        quote!(
            pub struct S<'db> {
                #[returns(copy)]
                a: Other<'db>,
                b: String,
            }
        ),
    ));
    accepted(interned_tokens(
        quote!(unsafe(no_lifetime)),
        quote!(
            struct S {
                a: u32,
            }
        ),
    ));
    accepted(tracked_tokens(
        quote!(debug, constructor = new_),
        quote!(
            struct S<'db> {
                root: M<'db>,
                #[tracked]
                #[returns(ref)]
                table: T,
                #[tracked]
                #[no_eq]
                other: U,
            }
        ),
    ));
}

#[test]
fn an_error_gives_the_item_back_so_its_uses_resolve() {
    let tokens = input_tokens(
        quote!(lru = 3),
        quote!(
            struct S {
                #[returns(copy)]
                a: u32,
            }
        ),
    );
    let text = tokens.to_string();
    assert!(text.contains("struct S"), "{text}");
    assert!(!text.contains("returns"), "{text}");
}
