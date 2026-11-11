//! `&uniq T` under `#![blaze::uniq]` (docs/v2/plan/design/edition.md, change 10) is `&mut T`:
//! each spelling is accepted where the other is asked for, in arguments, `let` types, returns,
//! receivers, `ref` bindings and reference patterns, both ways; a shared reference is still
//! not; and the crate's types display as it spells them, `&uniq u32`.

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
pub fn takes_u32(_: u32) {}
pub fn patterns(a: &uniq u32, b: &mut u32, c: (&uniq u32, u8)) {
    let &uniq by_uniq = a;
    takes_u32(by_uniq);
    let &uniq of_mut = b;
    takes_u32(of_mut);
    let (&mut in_tuple, _) = c;
    takes_u32(in_tuple);
    let closure = |&uniq in_closure: &uniq u32| takes_u32(in_closure);
}
pub fn refused_pattern(r: &u32) {
    let &uniq x = r;
}
"#;

/// The fixture's function `name`, free or in an impl.
fn function(db: &base_db::Db, name: &str) -> FunctionId {
    function_in(db, fixture::test_crate(db), name)
}

/// The fixture's crate `name`.
fn crate_named(db: &base_db::Db, name: &str) -> base_db::Crate {
    *base_db::all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap_or_else(|| panic!("the fixture's crate `{name}`"))
}

/// The function `name` of `krate`, free or in an impl.
fn function_in(db: &base_db::Db, krate: base_db::Crate, name: &str) -> FunctionId {
    let mut found = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
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

/// `&uniq pat` is the reference pattern `&mut pat`: it takes a unique reference apart, of
/// either spelling, binding what it refers to; and it is refused a shared reference, as
/// `&mut pat` is.
#[test]
fn a_uniq_reference_pattern_is_a_mut_reference_pattern() {
    let db = fixture::db(ACCEPTED).into_db();
    blaze_query::db::attach(&db, || {
        let krate = fixture::test_crate(&db);
        for binding in ["by_uniq", "of_mut", "in_tuple", "in_closure"] {
            let (shown, mismatched) = binding_ty(&db, krate, "patterns", binding);
            assert_eq!(shown, "u32", "`{binding}`");
            assert!(!mismatched, "`patterns` has a type mismatch");
        }
        // The control: a shared reference does not match a unique reference pattern.
        let def = DefWithBodyId::FunctionId(function(&db, "refused_pattern"));
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        assert!(infer.has_type_mismatches(), "`&uniq x` matches a `&u32`");
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

/// The type of the binding `binding` of `krate`'s function `func`, as `krate` displays it, and
/// whether the function has a type mismatch.
fn binding_ty(db: &base_db::Db, krate: base_db::Crate, func: &str, binding: &str) -> (String, bool) {
    let target = DisplayTarget::from_crate(db, krate);
    let def = DefWithBodyId::FunctionId(function_in(db, krate, func));
    let body = Body::of(db, def);
    let infer = InferenceResult::of(db, InferBodyId::DefWithBodyId(def));
    let pat = body
        .pats()
        .find_map(|(pat, it)| match it {
            Pat::Bind { id, .. } if body[id].name.as_str() == binding => Some(pat),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the binding `{binding}` of `{func}`"));
    (infer.pat_ty(pat).display_test(db, target).to_string(), infer.has_type_mismatches())
}

/// `&uniq` where a macro carries it. `rust` has no gate and exports two macros; `gated` says
/// `#![blaze::uniq]`, has macros of its own and uses `rust`'s; `plain` has no gate and uses
/// `gated`'s.
const IN_MACROS: &str = r#"
//- minicore: sized, assert, panic
//- /rust.rs crate:rust
#[macro_export]
macro_rules! rust_own { ($place:expr) => { &uniq $place }; }
#[macro_export]
macro_rules! rust_pass { ($e:expr) => { $e }; }
//- /gated.rs crate:gated deps:rust
#![blaze::uniq]
#[macro_export]
macro_rules! own { ($place:expr) => { &uniq $place }; }
#[macro_export]
macro_rules! pass { ($e:expr) => { $e }; }
macro_rules! list { ($($e:expr),*) => { [$($e),*] }; }
macro_rules! typed { ($name:ident, $t:ty) => { pub fn $name(r: $t) -> $t { r } }; }
typed!(typed_uniq, &uniq u32);
pub fn takes_mut(r: &mut u32) -> bool { *r = 2; true }
pub fn in_calls() {
    let mut n = 0u32;
    let handed = pass!(&uniq n);
    takes_mut(handed);
    let written = own!(n);
    takes_mut(written);
    let through = typed_uniq(&uniq n);
    takes_mut(through);
    let listed = list![&uniq n];
}
pub fn asserted() {
    let mut n = 0u32;
    assert!({ let inside = &uniq n; takes_mut(inside) });
}
#[test]
fn under_test() {
    let mut n = 0u32;
    let tested: &uniq u32 = &uniq n;
    takes_mut(tested);
}
pub fn rusts() {
    let mut n = 0u32;
    let handed = rust::rust_pass!(&uniq n);
    takes_mut(handed);
    let written = rust::rust_own!(n);
}
//- /plain.rs crate:plain deps:gated
pub fn gateds() {
    let mut n = 0u32;
    let written = gated::own!(n);
    gated::takes_mut(written);
    let handed = gated::pass!(&uniq n);
}
"#;

/// In a crate with the gate, `&uniq` handed to a macro call (a `macro_rules!` fragment, an
/// array of them, the builtin `assert!`), written by the crate's own macro, in a type a macro
/// is handed, and in a function under `#[test]` is the unique reference it is outside a
/// macro: the same types, no mismatch.
#[test]
fn uniq_in_a_gated_crates_macros_is_the_unique_reference() {
    let db = fixture::db(IN_MACROS).into_db();
    blaze_query::db::attach(&db, || {
        let gated = crate_named(&db, "gated");
        for (func, binding, ty) in [
            ("in_calls", "handed", "&uniq u32"),
            ("in_calls", "written", "&uniq u32"),
            ("in_calls", "through", "&uniq u32"),
            ("in_calls", "listed", "[&uniq u32; 1]"),
            ("under_test", "tested", "&uniq u32"),
        ] {
            let (shown, mismatched) = binding_ty(&db, gated, func, binding);
            assert_eq!(shown, ty, "`{binding}` of `{func}`");
            assert!(!mismatched, "`{func}` has a type mismatch");
        }
        assert_eq!(binding_ty(&db, gated, "asserted", "inside").0, "&uniq u32");
    });
}

/// A token is parsed under the gate of the crate that wrote it, through any macro: a gated
/// crate's `&uniq` handed to a macro of a crate without the gate is the unique reference, and
/// so is the `&uniq` a gated crate's macro writes when a crate without the gate calls it;
/// while the `&uniq` a crate without the gate writes is Rust's syntax error, in its own macro
/// expanded in a gated crate and handed to a gated crate's macro alike.
#[test]
fn uniq_in_a_macro_is_read_under_the_gate_of_the_crate_that_wrote_it() {
    let db = fixture::db(IN_MACROS).into_db();
    blaze_query::db::attach(&db, || {
        let (gated, plain) = (crate_named(&db, "gated"), crate_named(&db, "plain"));
        assert!(gated.extensions(&db).uniq() && plain.extensions(&db).is_none());
        // `gated` calls `rust`'s macros.
        assert_eq!(binding_ty(&db, gated, "rusts", "handed").0, "&uniq u32");
        assert_ne!(binding_ty(&db, gated, "rusts", "written").0, "&uniq u32");
        // `plain` calls `gated`'s macros; it displays the reference as Rust spells it.
        assert_eq!(binding_ty(&db, plain, "gateds", "written").0, "&mut u32");
        assert_ne!(binding_ty(&db, plain, "gateds", "handed").0, "&mut u32");
    });
}
