//! A build reads a crate before the procedural macros it calls are built
//! (`ProcMacroLoadingError::NotYetBuilt`), and is given them later: a write that changes no
//! round-0 log of the crate, whose run makes the modules the macros write. An impl in a body of
//! such a module (`const _: () = { impl .. }`) is the crate's as any other.

use base_db::{Db, ProcMacroLoadingError, all_crates};
use hir_expand::proc_macro::ProcMacros;

use super::{Loaded, check};
use crate::method_resolution::delta::state::SET_SCOPES_ARE_NOT_READ;

/// What the macro is handed: a module with a struct and, in a `const _`, its impls.
const WRITTEN: &str = "pub mod z { pub struct S; const _: () = { impl S { pub fn tok(&self) {} } impl crate::Tr for S {} }; }";

/// The differences of `main`'s tables from the reference, read before the macros of
/// `proc_macros` are built and again after, where `main` calls the wrapper as `call`.
fn given_after_a_read(call: &str) -> String {
    let mut loaded = Loaded::new(&format!(
        "//- proc_macros: pass\n//- /main.rs crate:main\npub trait Tr {{}}\n\
         macro_rules! w {{ ($($input:tt)*) => {{ proc_macros::pass! {{ $($input)* }} }}; }}\n\
         pub(crate) use w;\n{call} {{ {WRITTEN} }}\n"
    ));
    let main = loaded.krate();
    let db: &Db = &loaded.db;
    let macros = *all_crates(db)
        .iter()
        .find(|it| it.data(db).target_kind.is_proc_macro())
        .expect("`proc_macros`");
    let built = ProcMacros::get_for_crate(db, macros)
        .and_then(|it| it.macros().map(<[_]>::to_vec))
        .expect("listed");
    let mut failures = String::new();
    ProcMacros::give(&mut loaded.db, [(macros, Err(ProcMacroLoadingError::NotYetBuilt))]);
    // Attached: a difference is written with the ids of the traits it is of.
    blaze_query::db::attach(&loaded.db, || {
        check(&loaded.db, main, "its macros not yet built", &mut failures);
    });
    ProcMacros::give(&mut loaded.db, [(macros, Ok(built))]);
    blaze_query::db::attach(&loaded.db, || {
        check(&loaded.db, main, "given its built macros", &mut failures);
    });
    failures
}

/// The differences of `main`'s tables from the reference after an edit that leaves a module
/// with a `const _` holding an impl and nothing else: no name is bound, and the module declares
/// no item and no impl of its own.
fn a_const_alone_is_written() -> String {
    let mut loaded = Loaded::new(
        "//- /main.rs crate:main\npub struct S;\npub trait Tr {}\nmod m;\n//- /m.rs\n",
    );
    let main = loaded.krate();
    let mut failures = String::new();
    blaze_query::db::attach(&loaded.db, || {
        check(&loaded.db, main, "the module empty", &mut failures);
    });
    let file = loaded.files[1];
    loaded.db.set_file_text(
        file,
        "const _: () = { impl crate::S { pub fn tok(&self) {} } impl crate::Tr for crate::S {} };\n",
    );
    blaze_query::db::attach(&loaded.db, || {
        check(&loaded.db, main, "a `const _` written", &mut failures);
    });
    failures
}

/// The invariant: the maintained impl tables equal the pull over every module's bodies after a
/// write that sets a module's scope and binds no name in it. One is a write that only gives a
/// crate's procedural macros, for a module the macros write under a `macro_rules!` called by
/// path (tempo-contracts' `crate::sol! { contract Z { .. } }` over `alloy_sol_types::sol!`:
/// every `impl SolCall` of its contracts was missing: E0277, E0599, E0282) or by its textual
/// scope; the other an edit that writes a `const _` alone.
#[test]
fn a_module_whose_scope_a_write_set_has_its_bodies_read() {
    for call in ["crate::w!", "w!"] {
        let failures = given_after_a_read(call);
        assert!(failures.is_empty(), "{call}:\n{failures}");
    }
    let failures = a_const_alone_is_written();
    assert!(failures.is_empty(), "{failures}");
}

/// The control of the invariant above: with the bodies read again only where a def or an impl
/// was touched or a binding changed, the module the macros write and the one the edit gives a
/// `const _` keep the empty parts they had.
#[test]
fn the_oracle_catches_a_set_scope_whose_bodies_are_not_read() {
    SET_SCOPES_ARE_NOT_READ.set(true);
    let (given, written) = (given_after_a_read("crate::w!"), a_const_alone_is_written());
    SET_SCOPES_ARE_NOT_READ.set(false);
    for (failures, what) in [(given, "given its built macros"), (written, "a `const _` written")] {
        assert!(
            failures.contains(&format!("{what}: TraitImpls::for_crate differs from the pull"))
                && failures.contains(&format!("{what}: InherentImpls::for_crate differs")),
            "{what}: {failures}"
        );
    }
}
