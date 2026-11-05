//! A dependency's bodies part (`hir_ty::check_cache`) whose bodies' inference evaluated constants
//! in types reads back in another database, whose ids differ, as that database's own inference.
//! Here, not with hir-ty's other `check_cache` tests: the solver evaluates such a constant by MIR,
//! which only this crate builds.

use std::{alloc::Global, sync::Arc};

use base_db::{Crate, Db, all_crates};
use blaze_format::Aligned;
use blaze_hash::HashMap;
use blaze_query::db::AsId;
use hir_def::{
    DefWithBodyId, FunctionId, LoweringMode, nameres::demand::crate_body_owners,
    signatures::FunctionSignature,
};
use hir_expand::{fixture, name::Name};

use crate::{check_cache, infer_query_with_inspect};

/// A dependency whose array types have lengths the solver evaluates: an expression, and one
/// naming a constant item.
const DEP: &str = r#"
//- minicore: sized, copy, add
//- /dep.rs crate:dep deps:core new_source_root:library
pub const N: usize = 3;
pub fn sum() -> u8 { let a = [1u8; 4]; let b: [u8; 2 + 2] = a; let [x, ..] = b; x }
pub fn named() -> u8 { let a = [2u8; 4]; let b: [u8; N + 1] = a; let [.., y] = b; y }
//- /main.rs crate:main deps:core,dep new_source_root:local
fn main() { let _ = dep::sum() + dep::named(); }
"#;

/// A crate of nothing `DEP`'s crates name, its functions interned before theirs.
const PAD: &str = "//- /pad.rs crate:pad deps:core new_source_root:library
pub fn p1() {}
pub fn p2() {}
pub const P: usize = 1 + 1;
";

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

fn function(db: &Db, krate: Crate, name: &str) -> FunctionId {
    crate_body_owners(db, krate)
        .into_iter()
        .find_map(|def| match def {
            DefWithBodyId::FunctionId(it)
                if FunctionSignature::of(db, it).name == Name::new_root(name) =>
            {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}`"))
}

#[test]
fn a_part_of_evaluated_constants_reads_back_in_another_database() {
    let first = fixture::db(DEP).into_db();
    let first_dep = crate_named(&first, "dep");
    let bytes = check_cache::encode(&first, first_dep, &|_| true).unwrap();

    let at = DEP.find("//- /").expect("a fixture with a file");
    let mut db = fixture::db(&format!("{}{PAD}{}", &DEP[..at], &DEP[at..])).into_db();
    let dep = crate_named(&db, "dep");
    let bits = |db: &Db, krate| function(db, krate, "sum").as_id().as_u32();
    assert_ne!(bits(&first, first_dep), bits(&db, dep), "the two databases' ids differ");

    check_cache::open(&bytes).unwrap();
    // alloc: test setup
    let mut parts = HashMap::new_in(Global);
    parts.insert(dep, Arc::new(Aligned::copy_from(&bytes)) as hir_def::deps::ImageBuf);
    check_cache::set_served(&mut db, parts);

    for name in ["sum", "named"] {
        let def = DefWithBodyId::FunctionId(function(&db, dep, name));
        let served =
            check_cache::served_inference(&db, def).unwrap_or_else(|| panic!("`{name}` is served"));
        let own = infer_query_with_inspect(&db, def, None, LoweringMode::Analysis);
        assert!(!own.has_errors, "`{name}` infers with no error: {own:?}");
        assert!(served == own, "`{name}`: served {served:?}\nown {own:?}");
        assert!(check_cache::served_passed(&db, def), "`{name}`'s verdict");
    }
}
