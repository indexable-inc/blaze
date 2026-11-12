//! An impl a `const _` block holds is an impl of its crate wherever the const is written: here
//! in a module a procedural macro writes, called from the output of a `macro_rules!` that is
//! itself called by path (`crate::w!`), so its call is resolved in an expansion round and the
//! module is made two expansions deep. tempo-contracts wraps `alloy_sol_types::sol!` so
//! (`crate::sol! { contract Z { .. } }`), and every `impl SolCall` of its contracts was lost:
//! `E0277 relayMessageCall: SolCall`, `E0599 no associated function _tokenize`.

use hir_def::{
    DefWithBodyId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{InferBodyId, InferenceResult};

const BODY: &str = r#"
    pub mod z {
        pub struct S;
        const _: () = {
            impl S { pub fn tok(&self) -> u32 { 1 } }
            impl crate::Tr for S { fn go(&self) -> u32 { S::tok(self) } }
        };
        pub fn user(s: &S) -> u32 { crate::Tr::go(s) + s.tok() }
    }
"#;

fn fixture_of(call: &str) -> String {
    format!(
        r#"
//- minicore: sized, add
//- proc_macros: identity
//- /main.rs crate:main deps:proc_macros
pub trait Tr {{ fn go(&self) -> u32; }}
macro_rules! w {{
    ($($input:tt)*) => {{ proc_macros::identity! {{ $($input)* }} }};
}}
pub(crate) use w;
{call} {{ {BODY} }}
"#
    )
}

/// The diagnostics of `user`'s body in the fixture whose wrapper is called as `call`.
fn user_diagnostics(call: &str) -> String {
    let db = fixture::db(&fixture_of(call)).into_db();
    blaze_query::db::attach(&db, || {
        let user = crate_def_map(&db, fixture::test_crate(&db))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::FunctionId(it)
                    if FunctionSignature::of(&db, it).name.as_str() == "user" =>
                {
                    Some(it)
                }
                _ => None,
            })
            .expect("the fixture's `user`");
        let infer =
            InferenceResult::of(&db, InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(user)));
        format!("{:?}", infer.diagnostics())
    })
}

#[test]
fn a_const_blocks_impls_are_found_when_its_module_is_two_expansions_deep() {
    assert_eq!(user_diagnostics("crate::w!"), "[]", "the wrapper called by path");
    assert_eq!(user_diagnostics("w!"), "[]", "the wrapper called by its textual scope");
}
