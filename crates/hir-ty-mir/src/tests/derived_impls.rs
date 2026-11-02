use std::{any::TypeId, sync};

use base_db::{Db, all_crates};
use hir_def::{AssocItemId, DefWithBodyId, nameres::assoc::ImplItems, nameres::crate_def_map};
use hir_expand::{
    builtin::quote::quote,
    proc_macro::{
        ProcMacro, ProcMacroCall, ProcMacroExpander, ProcMacroExpansionError, ProcMacroKind,
    },
    tt::{self, TopSubtree},
};
use intern::Symbol;
use hir_expand::fixture;

use crate::InferenceResult;

/// Ignores its input and writes an impl of `a::Deserializable`, every token at the call site, as
/// `"...".parse()` in a real proc macro does.
#[derive(Debug)]
struct ImplDeserializableExpander;
impl ProcMacroExpander for ImplDeserializableExpander {
    fn expand(
        &self,
        _: &Db,
        ProcMacroCall { call_site, .. }: ProcMacroCall<'_>,
    ) -> Result<TopSubtree, ProcMacroExpansionError> {
        Ok(quote! { call_site =>
            impl a::Deserializable for FixKind {
                fn deserialize(name: &str) -> Option<Self> {
                    let _text = a::Text::deserialize(name);
                    None
                }
            }
        })
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool { // dyn: hir-expand's `ProcMacroExpander::eq_dyn`; compared when a crate's macros are set
        other.type_id() == TypeId::of::<Self>()
    }
}

/// The inference diagnostics of each function of the impls in crate `main`.
fn impl_fn_diagnostics(#[rust_analyzer::rust_fixture] ra_fixture: &str) -> Vec<String> {
    let proc_macros = vec![(
        r#"
#[proc_macro_derive(ImplDeserializable)]
pub fn impl_deserializable(_item: TokenStream) -> TokenStream {
    TokenStream::new()
}
"#
        .into(),
        ProcMacro {
            name: Symbol::intern("ImplDeserializable"),
            kind: ProcMacroKind::CustomDerive,
            expander: sync::Arc::new(ImplDeserializableExpander),
            disabled: false,
        },
    )];
    let db = fixture::db_with_proc_macros(ra_fixture, proc_macros).into_db();
    crate::attach_db(&db, || {
        let krate = *all_crates(&db)
            .iter()
            .find(|krate| {
                krate.extra_data(&db).display_name.as_ref().map(|it| it.canonical_name().as_str())
                    == Some("main")
            })
            .unwrap();
        let def_map = crate_def_map(&db, krate);
        let mut fns = 0;
        let mut found = Vec::new();
        for (_, module) in def_map.modules(&db) {
            for impl_ in module.scope(&db).impls() {
                for (_, item) in ImplItems::of(&db, impl_).0.items.iter() {
                    let AssocItemId::FunctionId(func) = *item else { continue };
                    fns += 1;
                    let infer = InferenceResult::of(&db, DefWithBodyId::from(func));
                    found.extend(infer.diagnostics().iter().map(|it| format!("{it:?}")));
                }
            }
        }
        assert_eq!(fns, 1, "the impl's function was not found");
        found
    })
}

#[test]
fn derived_impl_uses_its_own_trait() {
    let found = impl_fn_diagnostics(
        r#"
//- proc_macros: impl_deserializable
//- minicore: derive, option
//- /c.rs crate:c
pub struct Text;
//- /a.rs crate:a deps:c
pub use c::Text;
pub trait Deserializable: Sized { fn deserialize(name: &str) -> Option<Self>; }
impl Deserializable for Text { fn deserialize(name: &str) -> Option<Self> { None } }
//- /main.rs crate:main deps:a
#[derive(proc_macros::ImplDeserializable)]
pub enum FixKind { A, B }
"#,
    );
    assert_eq!(found, Vec::<String>::new());
}

#[test]
fn hand_written_impl_uses_its_own_trait() {
    let found = impl_fn_diagnostics(
        r#"
//- minicore: option
//- /c.rs crate:c
pub struct Text;
//- /a.rs crate:a deps:c
pub use c::Text;
pub trait Deserializable: Sized { fn deserialize(name: &str) -> Option<Self>; }
impl Deserializable for Text { fn deserialize(name: &str) -> Option<Self> { None } }
//- /main.rs crate:main deps:a
pub enum FixKind { A, B }
impl a::Deserializable for FixKind {
    fn deserialize(name: &str) -> Option<Self> {
        let _text = a::Text::deserialize(name);
        None
    }
}
"#,
    );
    assert_eq!(found, Vec::<String>::new());
}
