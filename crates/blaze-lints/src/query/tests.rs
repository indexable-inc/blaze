//! The lint queries over a database edited between runs: which queries run again, and what a
//! hit costs.

use base_db::{Db, FileId};
use hir_def::DefWithBodyId;
use hir_expand::fixture;

use super::{LintSet, body_lints};
use crate::{
    check::{Setup, check},
    conf::Conf,
    level::CommandLine,
    lints::Implemented,
};

const TWO: &str = "\
fn a(x: bool, y: bool) {
    if x {
        if y {
            let _ = 1;
        }
    }
}
fn b(x: bool) {
    if x {
        let _ = 2;
    }
}
";

fn db(text: &str) -> (Db, FileId) {
    let fx = fixture::db(&format!("//- /main.rs crate:main\n{text}"));
    let file = fx.only_file();
    let db = fx.into_db();
    let file = file.file_id(&db);
    (db, file)
}

fn setup(flags: &str) -> Setup {
    let flags: Vec<String> = flags.split_whitespace().map(str::to_owned).collect();
    Setup { command_line: CommandLine::parse(&flags).expect("the flags read"), conf: Conf::default() }
}

/// The lints of the crate `main`, as many as were found.
fn lint(db: &Db, setup: &Setup) -> usize {
    let krate = crate::tests::main_crate(db);
    let name_of = |_: FileId| Some("main.rs".to_owned());
    check(db, krate, setup, &name_of).expect("every file is named").len()
}

/// How many times `query` is among the ingredient names in `executed`.
fn runs(executed: &[String], query: &str) -> usize {
    executed.iter().filter(|it| it.trim_end_matches('_').ends_with(query)).count()
}

/// The body of the function `name` of the crate `main`'s root module.
fn body_named(db: &Db, name: &str) -> DefWithBodyId {
    let root = crate::tests::main_crate(db).root_module(db);
    root.declarations(db)
        .into_iter()
        .find_map(|def| match def {
            hir::ModuleDef::Function(it) if it.name(db).as_str() == name => {
                Some(DefWithBodyId::FunctionId(it.try_into().expect("a function of the source")))
            }
            _ => None,
        })
        .expect("a function of that name")
}

#[test]
fn an_edit_of_one_body_lints_that_body_and_no_other() {
    let (mut db, file) = db(TWO);
    let setup = setup("");
    let cold = db.log_executed(|| _ = lint(&db, &setup));
    assert_eq!(runs(&cold, "body_lints"), 2, "{cold:#?}");

    // A statement more in `a` moves `b` in the file without changing it.
    let edited = TWO.replacen("let _ = 1;", "let _ = 1;\n            let _ = 3;", 1);
    db.set_file_text(file, &edited);
    let warm = db.log_executed(|| _ = lint(&db, &setup));
    assert_eq!(runs(&warm, "body_lints"), 1, "{warm:#?}");

    // An edit of what is in no body lints no body again.
    db.set_file_text(file, &format!("struct S;\n{edited}"));
    let items = db.log_executed(|| _ = lint(&db, &setup));
    assert_eq!(runs(&items, "body_lints"), 0, "{items:#?}");
    assert_eq!(runs(&items, "item_lints"), 1, "{items:#?}");
}

#[test]
fn what_an_edit_moves_is_found_where_it_moved_to() {
    let (mut db, file) = db(TWO);
    let setup = setup("-A clippy::all -W clippy::collapsible_if");
    let krate = crate::tests::main_crate(&db);
    let name_of = |_: FileId| Some("main.rs".to_owned());
    let lo = |db: &Db| {
        let found = check(db, krate, &setup, &name_of).expect("every file is named");
        found.iter().map(|it| it.diagnostic.span.primary_span().map_or(0, |it| it.lo)).collect::<Vec<_>>()
    };
    let before = lo(&db);
    assert_eq!(before.len(), 1, "`a`'s `if`s collapse; `b` has one");
    db.set_file_text(file, &format!("struct S;\n{TWO}"));
    assert_eq!(lo(&db), [before[0] + "struct S;\n".len() as u32]);
}

#[test]
fn a_crate_linted_with_syntactic_lints_only_infers_nothing() {
    let (db, _) = db(TWO);
    let setup = setup("-A clippy::all -W clippy::collapsible_if");
    let mut found = 0;
    let executed = db.log_executed(|| found = lint(&db, &setup));
    assert_eq!(found, 1);
    assert!(runs(&executed, "body_lints") > 0, "{executed:#?}");
    assert_eq!(runs(&executed, "infer_body_query"), 0, "{executed:#?}");
}

#[test]
fn a_body_lints_hit_allocates_nothing() {
    let (db, _) = db(TWO);
    hir::attach_db(&db, || {
        let def = body_named(&db, "a");
        let set = LintSet::new(&db, Implemented::CollapsibleIf.bit(), Conf::default());
        assert_eq!(body_lints(&db, def, set).findings.len(), 1);
        let (lints, used) = blaze_alloc_count::measure(|| body_lints(&db, def, set).findings.len());
        assert_eq!(lints, 1);
        assert_eq!(used.allocations, 0, "a hit of `body_lints` allocates nothing");
    });
}
