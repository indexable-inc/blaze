use base_db::{Crate, Db};
use blaze_lint_defs::{Level, LintId, LintOptions};
use hir_def::{
    DefWithBodyId, ExpressionStoreOwnerId, GenericDefId, ModuleDefId, VariantId, expr_store::ExpressionStore,
    hir::{Expr, ExprId},
    nameres::crate_def_map,
};
use syntax::view::{self as ast, AstNode as _, HasName as _};
use hir_expand::fixture;

use crate::{LevelCx, LevelError, Levels, attrs, crate_lint_level, lint_level, lint_level_at, query::owner_node};

const LINT: LintId = LintId::LONG_RUNNING_CONST_EVAL;

fn db_of(fixture: &str) -> (Db, Crate) {
    let db = fixture::db(fixture).into_db();
    let krate = *base_db::all_crates(&db).iter().find(|it| it.data(&db).origin.is_local()).expect("a local crate");
    (db, krate)
}

/// The item of `krate` named `name`, of any module.
fn item(db: &Db, krate: Crate, name: &str) -> ModuleDefId {
    let def_map = crate_def_map(db, krate);
    for (_, module) in def_map.modules(db) {
        for def in module.scope(db).declarations() {
            let owner = match def {
                ModuleDefId::FunctionId(id) => ExpressionStoreOwnerId::Body(id.into()),
                ModuleDefId::ConstId(id) => ExpressionStoreOwnerId::Body(id.into()),
                ModuleDefId::StaticId(id) => ExpressionStoreOwnerId::Body(id.into()),
                ModuleDefId::AdtId(id) => ExpressionStoreOwnerId::Signature(GenericDefId::AdtId(id)),
                _ => continue,
            };
            let node = owner_node(db, owner).value;
            let named = ast::Fn::cast(node)
                .and_then(|it| it.name())
                .or_else(|| ast::Const::cast(node).and_then(|it| it.name()))
                .or_else(|| ast::Static::cast(node).and_then(|it| it.name()))
                .or_else(|| ast::Struct::cast(node).and_then(|it| it.name()));
            if named.is_some_and(|it| it.text().to_string() == name) {
                return def;
            }
        }
    }
    panic!("no item `{name}`")
}

fn body(db: &Db, krate: Crate, name: &str) -> DefWithBodyId {
    match item(db, krate, name) {
        ModuleDefId::FunctionId(id) => id.into(),
        ModuleDefId::ConstId(id) => id.into(),
        ModuleDefId::StaticId(id) => id.into(),
        other => panic!("`{name}` is {other:?}, which has no body"),
    }
}

/// The level at each `const { .. }` of `owner`'s store, in the order of their expressions.
fn at_inline_consts(db: &Db, owner: ExpressionStoreOwnerId) -> Vec<Level> {
    let consts: Vec<ExprId> =
        ExpressionStore::of(db, owner).exprs().filter(|(_, it)| matches!(it, Expr::Const(_))).map(|(id, _)| id).collect();
    consts.into_iter().map(|expr| lint_level_at(db, owner, Some(expr), LINT).expect("an inline const has syntax")).collect()
}

/// The level at the root of an anon const of `owner`'s store: every expression of a struct's
/// fields or a signature here is in its one array length.
fn at_anon_const(db: &Db, owner: ExpressionStoreOwnerId) -> Level {
    let exprs: Vec<ExprId> = ExpressionStore::of(db, owner).exprs().map(|(id, _)| id).collect();
    assert!(!exprs.is_empty(), "{owner:?} has an anon const");
    let levels: Vec<Level> = exprs.iter().map(|&expr| lint_level_at(db, owner, Some(expr), LINT).expect("an array length has syntax")).collect();
    assert!(levels.windows(2).all(|it| it[0] == it[1]), "one level across one anon const: {levels:?}");
    levels[0]
}

#[test]
fn an_anon_const_is_at_its_parents_level() {
    let (db, krate) = db_of(
        r#"
//- /main.rs crate:main edition:2021
#[allow(long_running_const_eval)]
struct Allowed { a: [u8; 1 + 1] }
struct Denied { a: [u8; 2 + 2] }
#[warn(long_running_const_eval)]
fn warned() -> [u8; 3 * 3] { [0; 9] }
fn main() {}
"#,
    );
    let fields = |name: &str| match item(&db, krate, name) {
        ModuleDefId::AdtId(hir_def::AdtId::StructId(id)) => ExpressionStoreOwnerId::VariantFields(VariantId::StructId(id)),
        other => panic!("`{name}` is {other:?}"),
    };
    assert_eq!(at_anon_const(&db, fields("Allowed")), Level::Allow);
    assert_eq!(at_anon_const(&db, fields("Denied")), Level::Deny);
    let ModuleDefId::FunctionId(warned) = item(&db, krate, "warned") else { panic!("`warned` is a function") };
    assert_eq!(at_anon_const(&db, ExpressionStoreOwnerId::Signature(GenericDefId::FunctionId(warned))), Level::Warn);
}

#[test]
fn an_inline_const_is_at_its_parents_level_where_it_is() {
    let (db, krate) = db_of(
        r#"
//- /main.rs crate:main edition:2021
fn f() {
    #[allow(long_running_const_eval)]
    let _a = const { 1 };
    let _b = const { 2 };
    #[warn(long_running_const_eval)]
    {
        let _c = const { 3 };
    }
}
#[allow(long_running_const_eval)]
fn g() {
    let _d = const { 4 };
}
fn main() {}
"#,
    );
    let f = ExpressionStoreOwnerId::Body(body(&db, krate, "f"));
    assert_eq!(at_inline_consts(&db, f), [Level::Allow, Level::Deny, Level::Warn]);
    let g = ExpressionStoreOwnerId::Body(body(&db, krate, "g"));
    assert_eq!(at_inline_consts(&db, g), [Level::Allow]);
    assert_eq!(lint_level(&db, body(&db, krate, "g"), LINT), Level::Allow);
    assert_eq!(lint_level(&db, body(&db, krate, "f"), LINT), Level::Deny);
}

#[test]
fn an_items_level_comes_through_mod_files_and_macro_calls() {
    let (db, krate) = db_of(
        r#"
//- /main.rs crate:main edition:2021
#[deny(long_running_const_eval)]
mod file;
macro_rules! make { () => { pub const MADE: u8 = 1; } }
#[warn(long_running_const_eval)]
mod inline {
    make!();
    pub static S: u8 = 0;
}
fn main() {}
//- /file.rs
#![allow(long_running_const_eval)]
pub const IN_FILE: u8 = 1;
"#,
    );
    assert_eq!(lint_level(&db, body(&db, krate, "IN_FILE"), LINT), Level::Allow);
    assert_eq!(lint_level(&db, body(&db, krate, "MADE"), LINT), Level::Warn);
    assert_eq!(lint_level(&db, body(&db, krate, "S"), LINT), Level::Warn);
    assert_eq!(crate_lint_level(&db, krate, LINT), Level::Deny);
}

#[test]
fn the_crate_root_has_its_crate_attrs() {
    let (db, krate) = db_of(
        r#"
//- /main.rs crate:main edition:2021 crate-attr:allow(long_running_const_eval)
const C: u8 = 1;
fn main() {}
"#,
    );
    assert_eq!(crate_lint_level(&db, krate, LINT), Level::Allow);
    assert_eq!(lint_level(&db, body(&db, krate, "C"), LINT), Level::Allow);
}

#[test]
fn the_command_line_warnings_and_the_cap_are_in_the_level() {
    let fixture = r#"
//- /main.rs crate:main edition:2021
#![warn(long_running_const_eval)]
const C: u8 = 1;
#[allow(long_running_const_eval)]
const D: u8 = 1;
fn main() {}
"#;
    let with = |args: &[&str]| {
        let (mut db, krate) = db_of(fixture);
        let args: Vec<String> = args.iter().map(|&it| it.to_owned()).collect();
        krate.set_lint_options(&mut db, LintOptions::parse(&args).unwrap());
        (lint_level(&db, body(&db, krate, "C"), LINT), lint_level(&db, body(&db, krate, "D"), LINT))
    };
    assert_eq!(with(&[]), (Level::Warn, Level::Allow));
    assert_eq!(with(&["-D", "warnings"]), (Level::Deny, Level::Allow));
    assert_eq!(with(&["--cap-lints", "allow"]), (Level::Allow, Level::Allow));
    assert_eq!(with(&["--force-warn", "long-running-const-eval"]), (Level::ForceWarn, Level::ForceWarn));
    // As rustc's `shallow_lint_levels_on`, the HIR's levels don't hold a `forbid` against the
    // attributes inside it: the early pass's E0453 is what stops the build.
    assert_eq!(with(&["-F", "long-running-const-eval"]), (Level::Warn, Level::Allow));
}

/// The query, asked again, reads its memo: nothing is allocated.
#[test]
fn a_level_asked_again_is_a_read() {
    let (db, krate) = db_of(
        r#"
//- /main.rs crate:main edition:2021
#[allow(long_running_const_eval)]
const C: u8 = 1;
fn main() {}
"#,
    );
    let c = body(&db, krate, "C");
    assert_eq!(lint_level(&db, c, LINT), Level::Allow);
    blaze_alloc_count::assert_at_most(0, "a level asked again", || {
        for lint in blaze_lint_defs::lints() {
            lint_level(&db, c, lint.id());
        }
    });
}

/// A level at an expression is found exactly where the expression has syntax; a desugared one
/// is an error naming it, never the item's level.
#[test]
fn a_desugared_expression_is_an_error_not_the_items_level() {
    let (db, krate) = db_of(
        r#"
//- /main.rs crate:main edition:2021
fn f(x: Option<u8>) -> Option<u8> {
    #[allow(long_running_const_eval)]
    for _ in 0..1 {}
    let y = x?;
    Some(y)
}
fn main() {}
"#,
    );
    let owner = ExpressionStoreOwnerId::Body(body(&db, krate, "f"));
    let (store, map) = ExpressionStore::with_source_map(&db, owner);
    for (expr, _) in store.exprs() {
        match map.expr_syntax(expr) {
            Ok(_) => assert!(lint_level_at(&db, owner, Some(expr), LINT).is_ok(), "{expr:?} has syntax"),
            Err(_) => assert_eq!(lint_level_at(&db, owner, Some(expr), LINT), Err(LevelError::NoSyntax { owner, expr })),
        }
    }
}

/// rustc's pre-expansion lint pass gives a module file the command line's levels and its own
/// attributes', not the crate root's nor its `mod` item's (rust-lang/rust#132218); the HIR's
/// walk goes through the `mod` item to the crate.
#[test]
fn a_module_file_before_expansion_has_the_command_lines_levels_and_its_own() {
    let (db, krate) = db_of(
        r#"
//- /main.rs crate:main edition:2015
#![deny(keyword_idents)]
#[warn(keyword_idents_2018)]
mod m;
fn root() {}
//- /m.rs
fn plain() {}
#[forbid(keyword_idents_2018)]
fn own() {}
"#,
    );
    let lint = LintId::named("keyword_idents_2018").lint();
    let opts = LintOptions::default();
    let mut cx = LevelCx::new(&db, krate, &opts);
    let mut levels = Levels::new(&mut cx, &mut (), true, false);
    let at = |name: &str| match item(&db, krate, name) {
        ModuleDefId::FunctionId(id) => attrs::key(owner_node(&db, ExpressionStoreOwnerId::Body(id.into()))),
        other => panic!("`{name}` is {other:?}"),
    };
    let mut level = |name: &str, in_file: bool| {
        let set = match in_file {
            true => levels.set_of_in_file(&mut cx, &mut (), at(name)),
            false => levels.set_of(&mut cx, &mut (), at(name)),
        };
        levels.spec(&cx, lint, set).level
    };
    assert_eq!(level("plain", false), Level::Warn);
    assert_eq!(level("plain", true), Level::Allow);
    assert_eq!(level("own", true), Level::Forbid);
    assert_eq!(level("root", true), Level::Deny);
    assert_eq!(level("plain", false), Level::Warn, "the HIR's table is its own");
}
