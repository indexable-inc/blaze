//! Allocation budgets of the checker's per-event hooks: once an allocation, its tags and its
//! sites exist, an access, a copy and a step allocate nothing.

use blaze_alloc_count::assert_at_most;
use hir_ty::{InferBodyId, mir::MirSpan};
use hir_ty_mir::mir::{Access, Machine, MemKind, Ptr, Site};
use hir_ty_ub::{
    Config,
    check::{Checker, Shadow, Sites},
    memory::Bytes,
};
use hir_expand::fixture;

use crate::main_of;

const ROUNDS: usize = 1_000;

/// A checker with one live 64-byte heap allocation at `0x1000`, written and read once, and
/// the pointer to it.
fn warm() -> (Checker<'static>, Ptr<hir_ty_ub::ids::Prov>) {
    let mut checker = Checker::new(8, &Config::default(), 0);
    let prov = checker.alloc(0x1000, 64, 8, MemKind::Rust, false).expect("the allocation");
    let ptr = Ptr { addr: 0x1000, prov: Some(prov) };
    let value = Shadow(Bytes::init(8));
    checker.write(ptr, 8, 8, Access::Plain, &value).expect("the first write");
    checker.read(ptr, 8, 8, Access::Plain).expect("the first read");
    let dst = Ptr { addr: 0x1020, prov: Some(prov) };
    checker.copy(dst, ptr, 8, 8, false).expect("the first copy");
    (checker, ptr)
}

#[test]
fn plain_accesses_allocate_nothing() {
    let (mut checker, ptr) = warm();
    let value = Shadow(Bytes::init(8));
    assert_at_most(0, "a plain read and write of 8 bytes", || {
        for _ in 0..ROUNDS {
            checker.write(ptr, 8, 8, Access::Plain, &value).expect("a write");
            checker.read(ptr, 8, 8, Access::Plain).expect("a read");
        }
    });
    assert!(checker.found().is_none(), "{:?}", checker.found());
}

#[test]
fn copies_allocate_nothing() {
    let (mut checker, ptr) = warm();
    let dst = Ptr { addr: 0x1020, prov: ptr.prov };
    assert_at_most(0, "a copy of 8 bytes", || {
        for _ in 0..ROUNDS {
            checker.copy(dst, ptr, 8, 8, false).expect("a copy");
        }
    });
}

#[test]
fn steps_and_site_hits_allocate_nothing() {
    let fx = fixture::db(program!("sized", "fn main() {}\n"));
    let file = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let owner = InferBodyId::DefWithBodyId(main_of(&db, file).into());
        let a = Site { owner, span: MirSpan::Unknown };
        let b = Site { owner, span: MirSpan::SelfParam };

        let mut sites = Sites::default();
        sites.intern(a);
        sites.intern(b);
        assert_at_most(0, "interning a site already interned", || {
            for _ in 0..ROUNDS {
                sites.intern(a);
                sites.intern(b);
            }
        });

        let mut checker = Checker::new(8, &Config::default(), 0);
        checker.step(a);
        checker.step(b);
        assert_at_most(0, "a step to a site already seen", || {
            for _ in 0..ROUNDS {
                checker.step(a);
                checker.step(b);
            }
        });
    });
}
