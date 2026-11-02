//! A type-relative value path records what its qualifier resolved to, where rustc's HIR has a
//! `QPath::TypeRelative` whose `qself` is a path: `S::new`, `T::make`, `u16::make`,
//! `Make::make`, `Alias::A`. A path rustc resolves whole (`E::A`, `f`, a local) or whose
//! qualifier is a written type (`<S>::new`) has none.

use base_db::{Db, all_crates};
use hir_def::{
    AdtId, DefWithBodyId, FunctionId, ModuleDefId,
    builtin_type::{BuiltinType, BuiltinUint},
    expr_store::Body,
    hir::{Expr, ExprOrPatId},
    nameres::crate_def_map,
    resolver::TypeNs,
    signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{InferBodyId, InferenceResult};

const FIXTURE: &str = r#"
struct S;
impl S {
    fn new() -> S {
        S
    }
}
enum E {
    A,
}
type Alias = E;
trait Make {
    fn make() -> Self;
}
impl Make for u16 {
    fn make() -> Self {
        0
    }
}
fn f() {}

fn adt() {
    S::new();
}
fn param<T: Make>() {
    T::make();
}
fn builtin() {
    u16::make();
}
fn trait_() {
    let _: u16 = Make::make();
}
fn alias() {
    Alias::A;
}
fn variant() {
    E::A;
}
fn anchored() {
    <S>::new();
}
fn plain() {
    f();
}
fn local() {
    let x = 0u8;
    x;
}
"#;

enum Expected {
    Adt(&'static str),
    Param,
    Builtin(BuiltinType),
    Trait,
    Alias,
    None,
}

fn function(db: &Db, name: &str) -> FunctionId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = def
                    && FunctionSignature::of(db, it).name.as_str() == name
                {
                    return it;
                }
            }
        }
    }
    panic!("no function `{name}` in the fixture")
}

#[test]
fn a_type_relative_value_path_records_its_qualifier() {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let cases = [
            ("adt", Expected::Adt("S")),
            ("param", Expected::Param),
            ("builtin", Expected::Builtin(BuiltinType::Uint(BuiltinUint::U16))),
            ("trait_", Expected::Trait),
            ("alias", Expected::Alias),
            ("variant", Expected::None),
            ("anchored", Expected::None),
            ("plain", Expected::None),
            ("local", Expected::None),
        ];
        for (name, expected) in cases {
            let def = DefWithBodyId::FunctionId(function(&db, name));
            let body = Body::of(&db, def);
            let mut paths = body.exprs().filter(|(_, it)| matches!(it, Expr::Path(_))).map(|(id, _)| id);
            let path = paths.next().unwrap_or_else(|| panic!("{name}: no path expression"));
            assert!(paths.next().is_none(), "{name}: more than one path expression");
            let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
            assert!(infer.value_resolution(ExprOrPatId::ExprId(path)).is_some(), "{name}: unresolved");
            let got = infer.qualifier_resolution(ExprOrPatId::ExprId(path));
            let ok = match (&expected, got) {
                (Expected::Adt(adt), Some((TypeNs::AdtId(AdtId::StructId(it)), 1))) => {
                    hir_def::signatures::StructSignature::of(&db, it).name.as_str() == *adt
                }
                (Expected::Param, Some((TypeNs::GenericParam(_), 1))) => true,
                (Expected::Builtin(ty), Some((TypeNs::BuiltinType(it), 1))) => it == *ty,
                (Expected::Trait, Some((TypeNs::TraitId(_), 1))) => true,
                (Expected::Alias, Some((TypeNs::TypeAliasId(_), 1))) => true,
                (Expected::None, None) => true,
                _ => false,
            };
            assert!(ok, "{name}: qualifier {got:?}");
        }
    });
}
