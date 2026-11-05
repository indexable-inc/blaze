//! A constant's value asked again is the query's memo, with nothing allocated.

use hir_def::{ConstId, ModuleDefId, signatures::ConstSignature};
use hir_expand::EditionedFileId;
use hir_expand::fixture;

use crate::{
    db::HirDatabase,
    next_solver::GenericArgs,
    test_db::{TestDB, TestDBExt as _},
};

fn const_named(db: &TestDB, file_id: EditionedFileId, name: &str) -> ConstId {
    let module = db.module_for_file(file_id.file_id(db));
    let named = |konst| {
        ConstSignature::of(db, konst)
            .name
            .as_ref()
            .is_some_and(|it| it.display(db, file_id.edition(db)).to_string() == name)
    };
    module
        .def_map(db)
        .module(db, module)
        .scope(db)
        .declarations()
        .find_map(|def| match def {
            ModuleDefId::ConstId(konst) if named(konst) => Some(konst),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no constant {name}"))
}

#[test]
fn a_memo_hit_allocates_nothing() {
    let fx = fixture::db("const A: u8 = 1; const B: u8 = A + 2;");
    let file_id = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let (_, control) = blaze_alloc_count::measure(|| std::hint::black_box(Box::new(1u64)));
        assert_eq!(control.allocations, 1, "the test binary counts allocations");
        let b = const_named(&db, file_id, "B");
        let first = db.const_eval(b, GenericArgs::empty(), None).unwrap();
        let hit = blaze_alloc_count::assert_at_most(0, "a memo hit", || {
            db.const_eval(b, GenericArgs::empty(), None).unwrap()
        });
        assert_eq!(first, hit);
    });
}
