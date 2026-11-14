//! The lint queries over a database edited between runs: which queries run again, and what a
//! hit costs.

use std::alloc::Global;

use base_db::{Db, FileId};
use blaze_memo::testing::Ran;
use hir_def::DefWithBodyId;
use hir_expand::fixture;

use super::{LintSet, body_lints, item_lints};
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
    Setup {
        command_line: CommandLine::parse(&flags).expect("the flags read"),
        conf: Conf::default(),
    }
}

/// The lints of the crate `main`, as many as were found.
fn lint(db: &Db, setup: &Setup) -> usize {
    let krate = crate::tests::main_crate(db);
    let name_of = |_: FileId| Some("main.rs".to_owned());
    check(db, krate, setup, &name_of).expect("every file is named").len()
}

/// The queries `work` ran, from the fixture database's run log.
fn runs_of(db: &Db, work: impl FnOnce()) -> Vec<Ran, Global> {
    let log = db.runs().expect("a fixture's database logs its runs");
    _ = log.take();
    work();
    log.take()
}

/// How many of `executed` ran the query whose kind is named `query` (its `KIND_NAME`),
/// whatever its key.
fn runs(executed: &[Ran], query: &str) -> usize {
    executed.iter().filter(|it| it.query == query).count()
}

/// The kinds the tests count, by their generated names.
const BODY_LINTS: &str = body_lints::KIND_NAME;
const ITEM_LINTS: &str = item_lints::KIND_NAME;
const INFER: &str = hir_ty::kinds::infer_body_query::KIND_NAME;

/// Sets `file`'s text in a new snapshot: one edit and its commit. Every id but the file's is
/// of the snapshot it was taken in, so a test takes them again after.
fn set_file_text(db: &mut Db, file: FileId, text: &str) {
    let mut edit = db.edit();
    edit.set_file_text(file, text);
    _ = edit.commit();
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

/// False by design on blaze-memo (port decision: reuse across edits comes from records, not
/// the memo): it claimed an edit of one body lints that body alone again, and an edit outside
/// every body lints none; an edit of a file now rebuilds its crate's arena, so every body and
/// item of the crate is linted again. Left failing.
#[test]
#[ignore = "blaze-memo: an edit rebuilds its crate's arena; the claim is the host's, on records (docs/v2/plan/design/host.md)"]
fn an_edit_of_one_body_lints_that_body_and_no_other() {
    let (mut db, file) = db(TWO);
    let setup = setup("");
    let cold = runs_of(&db, || _ = lint(&db, &setup));
    assert_eq!(runs(&cold, BODY_LINTS), 2, "{cold:#?}");

    // A statement more in `a` moves `b` in the file without changing it.
    let edited = TWO.replacen("let _ = 1;", "let _ = 1;\n            let _ = 3;", 1);
    set_file_text(&mut db, file, &edited);
    let warm = runs_of(&db, || _ = lint(&db, &setup));
    assert_eq!(runs(&warm, BODY_LINTS), 1, "{warm:#?}");

    // An edit of what is in no body lints no body again.
    set_file_text(&mut db, file, &format!("struct S;\n{edited}"));
    let items = runs_of(&db, || _ = lint(&db, &setup));
    assert_eq!(runs(&items, BODY_LINTS), 0, "{items:#?}");
    assert_eq!(runs(&items, ITEM_LINTS), 1, "{items:#?}");
}

#[test]
fn what_an_edit_moves_is_found_where_it_moved_to() {
    let (mut db, file) = db(TWO);
    let setup = setup("-A clippy::all -W clippy::collapsible_if");
    let name_of = |_: FileId| Some("main.rs".to_owned());
    // The crate taken in each snapshot: its id dies at a commit.
    let lo = |db: &Db| {
        let krate = crate::tests::main_crate(db);
        let found = check(db, krate, &setup, &name_of).expect("every file is named");
        found
            .iter()
            .map(|it| it.diagnostic.span.primary_span().map_or(0, |it| it.lo))
            .collect::<Vec<_>>()
    };
    let before = lo(&db);
    assert_eq!(before.len(), 1, "`a`'s `if`s collapse; `b` has one");
    set_file_text(&mut db, file, &format!("struct S;\n{TWO}"));
    assert_eq!(lo(&db), [before[0] + "struct S;\n".len() as u32]);
}

#[test]
fn a_crate_linted_with_syntactic_lints_only_infers_nothing() {
    let (db, _) = db(TWO);
    // Every lint on by default that reads types is off: clippy's (pedantic and nursery warn by
    // default too, `redundant_clone` reads MIR) and blaze's general lints (`borrow_workaround`
    // infers bodies).
    let setup =
        setup(&format!("{} -A blaze::general -W clippy::collapsible_if", crate::tests::CLIPPY_OFF));
    let mut found = 0;
    let executed = runs_of(&db, || found = lint(&db, &setup));
    assert_eq!(found, 1);
    assert!(runs(&executed, BODY_LINTS) > 0, "{executed:#?}");
    assert_eq!(runs(&executed, INFER), 0, "{executed:#?}");
}

#[test]
fn a_body_lints_hit_allocates_nothing() {
    let (db, _) = db(TWO);
    blaze_memo::attach(&db, || {
        let def = body_named(&db, "a");
        let set = LintSet::new(&db, Implemented::CollapsibleIf.bit(), Conf::default());
        assert_eq!(body_lints(&db, def, set).findings.len(), 1);
        let (lints, used) = blaze_alloc_count::measure(|| body_lints(&db, def, set).findings.len());
        assert_eq!(lints, 1);
        assert_eq!(used.allocations, 0, "a hit of `body_lints` allocates nothing");
    });
}
