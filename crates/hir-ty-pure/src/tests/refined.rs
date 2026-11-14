//! What one invariant buys the prover (docs/v2/plan/design/refined-values.md, "What one
//! invariant buys"): a body that takes a value of a type with an invariant assumes it, so the
//! overflow check of `t.x * 2` is shown never to fail; the same body over a bare `i32` is not.
//! And the reading of clauses: what is in the fragment, and what is implied by the field types.

use hir_def::{AdtId, DefWithBodyId, ModuleDefId};
use hir_expand::fixture;

use super::{fixture::crate_named, fns::fn_named};
use crate::{reach, refined};

const MINICORE: &str = "//- minicore: sized, copy, clone, drop, option, result, fn, panic, index, slice, builtin_impls, reach\n";

const KERNEL: &str = r#"
#[blaze::invariant(0 <= self.x && self.x <= i32::MAX / 2)]
pub struct Twice {
    x: i32,
}
#[blaze::invariant(self.small <= 255 && self.len <= self.cap)]
pub struct Buf {
    small: u8,
    len: u32,
    cap: u32,
}
pub fn doubled(t: &Twice) -> i32 {
    t.x * 2
}
pub fn doubled_raw(x: i32) -> i32 {
    x * 2
}
pub fn room(b: &Buf) -> u32 {
    b.cap - b.len
}
"#;

fn source() -> String {
    format!("{MINICORE}//- /lib.rs crate:kernel\n{}", KERNEL.trim_start_matches('\n')) // alloc: a test's fixture
}

/// Whether every integer operation of the root function `name` is shown to fit.
fn fits(db: &base_db::Db, name: &str) -> bool {
    let krate = crate_named(db, "kernel");
    let func = fn_named(db, krate, name);
    let found = reach::overflow_of_body(db, DefWithBodyId::FunctionId(func));
    found.refused.is_empty() && found.sites.iter().all(|it| it.2.is_none())
}

/// The struct named `name` at the root of `kernel`.
fn struct_named(db: &base_db::Db, name: &str) -> AdtId {
    let krate = crate_named(db, "kernel");
    let def_map = hir_def::nameres::crate_def_map(db, krate);
    def_map
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .find_map(|decl| match decl {
            ModuleDefId::AdtId(adt @ AdtId::StructId(_)) if refined::adt_name(db, adt) == name => {
                Some(adt)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no struct `{name}`"))
}

/// The invariant: a body that takes `&Twice` may assume `0 <= t.x <= i32::MAX / 2`, which every
/// value of `Twice` keeps (shown where each is built), so `t.x * 2` fits; over a bare `i32`
/// it is not shown. A struct field ordered by another (`len <= cap`) makes `cap - len` fit.
#[test]
fn an_invariant_discharges_the_overflow_checks_it_implies() {
    let db = fixture::db(&source()).into_db();
    blaze_query::db::attach(&db, || {
        assert!(fits(&db, "doubled"), "`t.x * 2` of a `Twice` fits");
        assert!(!fits(&db, "doubled_raw"), "`x * 2` of an `i32` is not shown to fit");
        assert!(fits(&db, "room"), "`cap - len` under `len <= cap` fits");
    });
}

/// `0 <= self.x && self.x <= i32::MAX / 2` is read as two comparisons and none is implied by
/// `i32`; `self.small <= 255` is implied by `u8` and needs no site to show it.
#[test]
fn invariants_are_read_and_the_implied_part_set_aside() {
    let db = fixture::db(&source()).into_db();
    blaze_query::db::attach(&db, || {
        let twice = refined::invariant_of(&db, struct_named(&db, "Twice")).expect("stated");
        assert!(twice.refused.is_none(), "{twice:?}");
        assert_eq!(twice.clauses.len(), 1);
        assert!(twice.implied.is_empty());
        let buf = refined::invariant_of(&db, struct_named(&db, "Buf")).expect("stated");
        assert!(buf.refused.is_none(), "{buf:?}");
        // The one attribute is one clause: `&&` of an implied and a needed part is needed.
        assert_eq!(buf.clauses.len(), 1);
    });
}
