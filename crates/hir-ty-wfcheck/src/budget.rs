//! What item well-formedness allocates on the global heap: its diagnostics alone, and
//! nothing to answer again.

use blaze_alloc_count::measure;
use hir_def::{GenericDefId, ModuleDefId, nameres::crate_def_map};
use hir_expand::fixture;

use crate::{WfResult, outlives::tests::db::TestDB, wfcheck};

fn items(db: &TestDB) -> Vec<GenericDefId> {
    let mut found = Vec::new(); // alloc: the test's list of items, outside what is measured
    for &krate in base_db::all_crates(db).iter() {
        for (_, module) in crate_def_map(db, krate).modules(db) {
            found.extend(module.scope(db).declarations().filter_map(|def| match def {
                ModuleDefId::AdtId(it) => Some(GenericDefId::from(it)),
                ModuleDefId::FunctionId(it) => Some(GenericDefId::from(it)),
                ModuleDefId::TraitId(it) => Some(GenericDefId::from(it)),
                _ => None,
            }));
            found.extend(module.scope(db).impls().map(GenericDefId::from));
        }
    }
    found
}

const FIXTURE: &str = r#"
//- minicore: sized, option
struct Wrap<'a, T: 'a>(&'a T);
struct Static<T> { x: &'static T }
enum Many<'a, T> { A(&'a T), B(Option<Wrap<'a, T>>), C, D(u8), E(&'a u8) }
trait Tr<'a> { type Out; }
impl<'a, T: 'a> Tr<'a> for Wrap<'a, T> { type Out = &'a T; }
fn both<'a, T>(x: &'a T, y: Static<T>) -> &'a T where T: 'a { let _ = y; x }
struct Twice<T> { x: &'static T, y: &'static T }
"#;

/// The diagnostics' own blocks: the vector and each message.
fn owned_blocks(result: &WfResult) -> u64 {
    u64::from(result.diagnostics.capacity() > 0)
        + result.diagnostics.iter().map(|it| u64::from(it.message.capacity() > 0)).sum::<u64>()
}

#[test]
fn checking_an_item_allocates_its_diagnostics_alone() {
    let fx = fixture::db(FIXTURE);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let defs = items(&db);
        let mut reported = 0;
        for &def in &defs {
            let result = wfcheck(&db, def);
            reported += result.diagnostics.len();
        }
        assert!(reported > 0, "the fixture's `Static` and `Twice` need `T: 'static`");
        for &def in &defs {
            let (result, used) = measure(|| crate::check_item(&db, def));
            assert_eq!(&result, wfcheck(&db, def), "{def:?}");
            assert_eq!(used.allocations, owned_blocks(&result), "{def:?}: {result:?}");
        }
    });
}

#[test]
fn asking_again_allocates_nothing() {
    let fx = fixture::db(FIXTURE);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        for def in items(&db) {
            wfcheck(&db, def);
            let (_, used) = measure(|| wfcheck(&db, def));
            assert_eq!(used.allocations, 0, "{def:?}");
        }
    });
}
