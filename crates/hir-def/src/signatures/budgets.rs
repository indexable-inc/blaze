//! Allocation budgets of the signature queries: a warm re-request is a memo hit and allocates
//! nothing; a miss allocates only its value.

use hir_expand::fixture;

use super::*;
use crate::{
    AdtId, ModuleDefId,
    test_db::{TestDB, TestDBExt},
    test_items::{declarations, impls},
};

/// A second request of each signature query is a memo hit: no allocation.
#[test]
fn warm_signature_requests_allocate_nothing() {
    let fx = fixture::db(
        r#"
pub struct S<T: Clone>(T) where T: Copy;
pub union U { a: u8 }
pub enum E { A, B(u8) }
pub const C: u8 = 0;
pub static ST: u8 = 0;
pub trait Tr<T> { type A; fn m(&self); }
impl<T: Copy + Clone> Tr<T> for S<T> { type A = (); fn m(&self) {} }
pub fn f<'a, T: ?Sized>(x: &'a T) -> &'a T { x }
pub type A<T> = (T, T);
"#,
    );
    let _ = fx.only_file();
    let db = fx.into_db();
    let krate = db.fetch_test_crate();
    let defs = declarations(&db, krate);
    let impl_ = *impls(&db, krate).first().expect("the fixture has an impl");
    let ask = || {
        for &def in &defs {
            match def {
                ModuleDefId::AdtId(AdtId::StructId(it)) => {
                    let _ = StructSignature::of(&db, it);
                    let _ = StructSignature::with_source_map(&db, it);
                    let _ = VariantFields::of(&db, it.into());
                }
                ModuleDefId::AdtId(AdtId::UnionId(it)) => {
                    let _ = UnionSignature::of(&db, it);
                    let _ = VariantFields::of(&db, it.into());
                }
                ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                    let _ = EnumSignature::of(&db, it);
                    let _ = EnumVariants::of(&db, it);
                }
                ModuleDefId::ConstId(it) => _ = ConstSignature::of(&db, it),
                ModuleDefId::StaticId(it) => _ = StaticSignature::of(&db, it),
                ModuleDefId::TraitId(it) => _ = TraitSignature::of(&db, it),
                ModuleDefId::FunctionId(it) => {
                    let _ = FunctionSignature::of(&db, it);
                    let _ = FunctionSignature::with_source_map(&db, it);
                }
                ModuleDefId::TypeAliasId(it) => _ = TypeAliasSignature::of(&db, it),
                _ => {}
            }
        }
        let _ = ImplSignature::of(&db, impl_);
        let _ = ImplSignature::with_source_map(&db, impl_);
    };
    ask();
    blaze_alloc_count::assert_at_most(0, "warm signature requests", ask);
}

/// A miss of `EnumVariants::of`, its syntax warm, allocates its map's two tables and nothing
/// else: the variants gathered in the frame, no diagnostics, the bits inline. `Some` and `None`
/// are predefined symbols, so naming them interns nothing. The miss measured is the second:
/// the first makes the storage's tables, and interns the variants, which a whitespace edit of
/// the enum leaves as they were.
#[test]
fn enum_variants_miss_is_its_map() {
    let fx = fixture::db("pub enum E { Some, None }");
    let file = fx.only_file();
    let mut db = fx.into_db();
    let enum_of = |db: &TestDB| {
        declarations(db, db.fetch_test_crate())
            .into_iter()
            .find_map(|it| match it {
                ModuleDefId::AdtId(AdtId::EnumId(it)) => Some(it),
                _ => None,
            })
            .expect("the fixture has an enum")
    };
    let _ = EnumVariants::of(&db, enum_of(&db));
    db.set_file_text(file.file_id(&db), "pub enum E { Some,  None }");
    let e = enum_of(&db);
    let _ = variants_syntax(&db, e);
    let _ = AttrFlags::query(&db, e.into());
    let len = blaze_alloc_count::assert_at_most(2, "an enum-variants miss", || {
        EnumVariants::of(&db, e).0.variants.len()
    });
    assert_eq!(len, 2);
}
