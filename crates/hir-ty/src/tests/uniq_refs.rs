//! `&uniq T` under `#![blaze::uniq]` (docs/v2/plan/design/edition.md, change 10) is `&mut T`:
//! each spelling is accepted where the other is asked for, in arguments, `let` types, returns,
//! receivers and `ref` bindings, both ways; a shared reference is still not; and the crate's
//! types display as it spells them, `&uniq u32`.

use hir_def::{
    AssocItemId, DefWithBodyId, FunctionId, ModuleDefId,
    expr_store::Body,
    hir::Pat,
    nameres::crate_def_map,
    signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{
    InferBodyId, InferenceResult,
    display::{DisplayTarget, HirDisplay},
};

const ACCEPTED: &str = r#"
//- minicore: sized
#![blaze::uniq]
pub struct S { n: u32 }
impl S {
    pub fn by_uniq(&uniq self) { self.n = 1; }
    pub fn by_mut(&mut self) { self.n = 2; }
    pub fn field<'a>(&'a uniq self) -> &'a mut u32 { &uniq self.n }
    pub fn field_mut<'a>(&'a mut self) -> &'a uniq u32 { &mut self.n }
}
pub fn takes_uniq(r: &uniq u32) { *r = 1; }
pub fn takes_mut(r: &mut u32) { *r = 2; }
pub fn main() {
    let mut n = 0u32;
    takes_uniq(&mut n);
    takes_mut(&uniq n);
    let a: &mut u32 = &uniq n;
    takes_uniq(a);
    let b: &uniq u32 = &mut n;
    takes_mut(b);
    let mut s = S { n: 0 };
    s.by_uniq();
    s.by_mut();
    let c: &uniq u32 = s.field();
    takes_mut(c);
    let d: &mut u32 = s.field_mut();
    takes_uniq(d);
    let (ref uniq e, _) = (1u32, 2u32);
    takes_mut(e);
    let (ref mut g, _) = (1u32, 2u32);
    takes_uniq(g);
}
pub fn refused(r: &u32) {
    takes_uniq(r);
}
"#;

/// The fixture's function `name`, free or in an impl.
fn function(db: &base_db::Db, name: &str) -> FunctionId {
    let mut found = Vec::new();
    for (_, module) in crate_def_map(db, fixture::test_crate(db)).modules(db) {
        let scope = module.scope(db);
        for def in scope.declarations() {
            if let ModuleDefId::FunctionId(it) = def {
                found.push(it);
            }
        }
        for impl_ in scope.impls() {
            for &(_, item) in impl_.impl_items(db).items.iter() {
                if let AssocItemId::FunctionId(it) = item {
                    found.push(it);
                }
            }
        }
    }
    found
        .into_iter()
        .find(|&it| FunctionSignature::of(db, it).name.as_str() == name)
        .unwrap_or_else(|| panic!("the fixture's `{name}`"))
}

#[test]
fn uniq_and_mut_references_are_accepted_for_each_other() {
    let db = fixture::db(ACCEPTED).into_db();
    blaze_query::db::attach(&db, || {
        for name in ["by_uniq", "by_mut", "field", "field_mut", "takes_uniq", "takes_mut", "main"] {
            let def = DefWithBodyId::FunctionId(function(&db, name));
            let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
            assert!(!infer.has_type_mismatches(), "`{name}`: {:?}", infer.diagnostics());
        }
        // The control: a shared reference is no unique one, in either spelling.
        let def = DefWithBodyId::FunctionId(function(&db, "refused"));
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        assert!(infer.has_type_mismatches(), "`&u32` is accepted as `&uniq u32`");
    });
}

#[test]
fn a_gated_crate_displays_unique_references_as_uniq() {
    for (fixture, spelled) in [
        (ACCEPTED, "&uniq u32"),
        (
            "//- minicore: sized\npub fn main() { let mut n = 0u32; let a: &mut u32 = &mut n; }\n",
            "&mut u32",
        ),
    ] {
        let db = fixture::db(fixture).into_db();
        blaze_query::db::attach(&db, || {
            let krate = fixture::test_crate(&db);
            let target = DisplayTarget::from_crate(&db, krate);
            let def = DefWithBodyId::FunctionId(function(&db, "main"));
            let body = Body::of(&db, def);
            let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
            let a = body
                .pats()
                .find_map(|(pat, it)| match it {
                    Pat::Bind { id, .. } if body[id].name.as_str() == "a" => Some(pat),
                    _ => None,
                })
                .expect("the binding `a`");
            let shown = infer.pat_ty(a).display_test(&db, target).to_string();
            assert_eq!(shown, spelled);
        });
    }
}
