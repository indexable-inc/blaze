//! The check by pieces against the whole check: the refinement over fixtures and edit kinds,
//! and the count of probes a body edit runs, each with its planted bug.

use std::{alloc::Global, collections::HashMap, fmt::Write as _};

use base_db::Db;
use span::FileId;
use syntax::{
    SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use super::{Group, Piece, PieceGate, pieces, unclean};
use crate::{
    Files, Options,
    cx::Cx,
    tests::{CHECKED, check_all, fixture_db},
};

/// Each run of [`super::piece_clean`], by the database it ran in: what the tests count to say
/// which pieces a check probed, a memo's verification counting for none.
pub(super) static PROBED: blaze_core::lock::Lock<Vec<(base_db::Nonce, Piece, Group)>> =
    blaze_core::lock::Lock::new("test.diagnostics.pieces.probed", Vec::new());

/// A bug planted in one database's check by pieces, for the oracle that must catch it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    /// A file whose outline is clean is left out of the scoped walk, its unclean bodies too.
    CleanOutlineSkipsBodies,
    /// A piece's memo reads the live tree of its file, so any edit of the file runs it again.
    KeyedByFile,
    /// The crate's settings a probe is given leave out the tools `#![register_tool]`
    /// registers (`checks::settings::crate_settings`).
    SettingsWithoutTools,
}

/// The bug planted in each database, by its nonce.
static PLANTED: blaze_core::lock::Lock<Vec<(base_db::Nonce, Plant)>> =
    blaze_core::lock::Lock::new("test.diagnostics.pieces.planted", Vec::new());

/// The bug planted in `db`, if any.
pub(crate) fn planted(db: &Db) -> Option<Plant> {
    let nonce = db.nonce_and_revision().0;
    PLANTED.lock().iter().find(|(of, _)| *of == nonce).map(|&(_, plant)| plant)
}

/// [`Plant::KeyedByFile`]'s read, in `piece`'s memo, when it is planted in `db`.
pub(super) fn plant_reads(db: &Db, piece: Piece) {
    if planted(db) != Some(Plant::KeyedByFile) {
        return;
    }
    let file = match piece {
        Piece::Outline(file) => hir::HirFileId::from(file),
        Piece::Body(def) => def.erased_ast_id(db).file_id,
    };
    let _ = file.root(db);
}

fn plant(db: &Db, plant: Plant) {
    PLANTED.lock().push((db.nonce_and_revision().0, plant));
}

/// The pieces of the syntax group probed in `db` since the last take.
fn take_probed(db: &Db) -> usize {
    let nonce = db.nonce_and_revision().0;
    let mut probed = PROBED.lock();
    let mut own = 0;
    probed.retain(|&(of, _, group)| {
        let mine = of == nonce;
        own += usize::from(mine && group == Group::Syntax);
        !mine
    });
    own
}

/// The six kinds of edit of the plan, each of a file's text.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Edit {
    /// A statement added to the first fn's body.
    BodyStatement,
    /// A comment line above the first item, which moves everything after it.
    CommentAbove,
    /// The first fn's first parameter renamed.
    ParamRenamed,
    /// An item added at the end.
    ItemAdded,
    /// The last item removed.
    ItemRemoved,
    /// The first `use` removed.
    UseRemoved,
}

pub(crate) const EDITS: [Edit; 6] = [
    Edit::BodyStatement,
    Edit::CommentAbove,
    Edit::ParamRenamed,
    Edit::ItemAdded,
    Edit::ItemRemoved,
    Edit::UseRemoved,
];

/// `text` after `edit`, if it has what the edit changes.
pub(crate) fn edited(text: &str, edit: Edit) -> Option<String> {
    let parse = syntax::SourceFile::parse(text, span::Edition::CURRENT);
    let root = parse.root();
    let items = || root.children().filter(|it| ast::Item::can_cast(it.kind()));
    let splice = |range: TextRange, with: &str| {
        let mut out = text.to_owned();
        out.replace_range(usize::from(range.start())..usize::from(range.end()), with);
        out
    };
    let under = |node: ast::Node<'_>, kinds: &[SyntaxKind]| {
        let mut at = Some(node);
        kinds.iter().all(|&kind| {
            at = at.and_then(|it| it.parent());
            at.is_some_and(|it| it.kind() == kind)
        })
    };
    match edit {
        Edit::BodyStatement => {
            let list = root.descendants().find(|it| {
                it.kind() == SyntaxKind::STMT_LIST
                    && under(*it, &[SyntaxKind::BLOCK_EXPR, SyntaxKind::FN])
            })?;
            let at = list.text_range().start() + TextSize::from(1);
            Some(splice(TextRange::empty(at), " let _ = 0;"))
        }
        Edit::CommentAbove => {
            let first = items().next()?;
            Some(splice(TextRange::empty(first.text_range().start()), "// an edit\n"))
        }
        Edit::ParamRenamed => {
            let name = root.descendants().find(|it| {
                it.kind() == SyntaxKind::NAME
                    && under(*it, &[SyntaxKind::IDENT_PAT, SyntaxKind::PARAM])
            })?;
            Some(splice(name.text_range(), "renamed_by_an_edit"))
        }
        Edit::ItemAdded => Some(format!("{text}\nfn added_by_an_edit() {{}}\n")),
        Edit::ItemRemoved => Some(splice(items().last()?.text_range(), "")),
        Edit::UseRemoved => {
            Some(splice(items().find(|it| it.kind() == SyntaxKind::USE)?.text_range(), ""))
        }
    }
}

/// Of each workspace crate of `db` that parses, the passes of the syntax group (the syntax
/// lints, then `checks::check`, whose passes of the group walk the context given beside the
/// build's) of its whole walk and of the walk of the pieces its sweep left unclean
/// ([`Cx::scoped`]), each as text.
fn syntax_by_pieces(
    db: &Db,
    names: &HashMap<FileId, String>,
    options: &Options,
) -> Vec<(String, String)> {
    blaze_query::db::attach(db, || {
        let mut out = Vec::new();
        for krate in hir::check::workspace_crates(db) {
            let frame = crate::tests::frame();
            let cx = Cx::new(db, krate, Files::new(names, ""), &frame).with_lints(&options.lints);
            if crate::lower::parse::parsed(&cx).fatal {
                continue;
            }
            let whole = format!(
                "{:?}\n{:?}",
                crate::lints::syntax::lints(&cx, &options.lints),
                crate::checks::check(&cx, &cx),
            );
            let all = pieces(&cx);
            let swept = unclean(&cx, &all, PieceGate::of(&cx, Group::Syntax, &options.lints));
            let scoped = cx.scoped(&all, &swept);
            let by_pieces = format!(
                "{:?}\n{:?}",
                crate::lints::syntax::lints(&scoped, &options.lints),
                crate::checks::check(&cx, &scoped),
            );
            out.push((whole, by_pieces));
        }
        out
    })
}

/// The refinement of one fixture: after each edit kind of its first file, the check of a
/// database that checked before the edit equals the check of a fresh database of the edited
/// text, and the syntax lints of the pieces left unclean equal those of the whole walk. How
/// many edits applied.
fn refine(fixture: &str, prefix: &str, options: &Options) -> usize {
    let (mut db, names, first) = fixture_db(fixture, prefix);
    check_all(&db, &names, options);
    let text = db.file_text(first).text(&db).as_str().to_owned();
    let mut applied = 0;
    for edit in EDITS {
        let Some(new) = edited(&text, edit) else { continue };
        applied += 1;
        db.set_file_text(first, &new);
        let warm = format!("{:?}", check_all(&db, &names, options));
        for (whole, by_pieces) in syntax_by_pieces(&db, &names, options) {
            assert_eq!(whole, by_pieces, "{edit:?} of\n{fixture}\nthe syntax group by piece");
        }
        let (mut fresh, fresh_names, fresh_first) = fixture_db(fixture, prefix);
        fresh.set_file_text(fresh_first, &new);
        let cold = format!("{:?}", check_all(&fresh, &fresh_names, options));
        assert_eq!(warm, cold, "{edit:?} of\n{fixture}\nthe check after the edit");
        db.set_file_text(first, &text);
        check_all(&db, &names, options);
    }
    applied
}

/// The control's fixture: a file whose outline is clean and whose one body has a syntax lint.
const OUTLINE_CLEAN_BODY_LINTS: &str = "//- /main.rs crate:main
mod a;
fn main() { a::f(); }
//- /a.rs
pub fn f() {
    while true {}
}
";

/// The settings control's fixture: a crate that registers a tool and uses it in a module, a
/// feature a body's gate reads, a static's value in parentheses (read at its item) and a
/// module file with inner attributes, read as its `mod` item's.
const REGISTERED_TOOL: &str = "//- /main.rs crate:main
#![feature(register_tool, never_type)]
#![register_tool(mytool)]
mod a;
mod b;
fn main() { a::f(); }
//- /a.rs
#[mytool::marked]
pub fn f() -> u32 { let x: Option<!> = None; let _ = x; 1 }
pub static S: u32 = (2);
//- /b.rs
#![allow(dead_code)]
#![allow(dead_code)]
pub fn g() {}
";

/// The fixtures the refinement always checks, beside those the suite checked before it.
const OWN: &[&str] = &[
    OUTLINE_CLEAN_BODY_LINTS,
    REGISTERED_TOOL,
    "//- /main.rs crate:main
use std::fmt;
mod a;
#[allow(unused)]
fn quiet(x: u32) -> u32 { let y = (x); y }
#[derive(Debug)]
struct S { a: u32 }
#[cfg(any())]
fn off() { while true {} }
fn main() {
    let _ = (1);
    let v = vec![1];
    let _ = v;
}
//- /a.rs
use std::fmt::Debug;
pub fn g(x: u32) -> u32 { if (x > 1) { x } else { 0 } }
#[expect(unused_parens)]
pub fn h() -> u32 { (1) }
pub const C: u32 = { 1 + 1 };
pub fn r() { loop { break; } }
",
];

/// The refinement: for every fixture of this module and every one the `checked*` helpers
/// checked before this test ran (tests/items, tests/lints and the rest of the suite), and
/// after each edit kind (a body's statement added, a comment line above the first item, a
/// signature's parameter renamed, an item added, an item removed, a `use` removed), the check
/// of a database that checked before the edit equals byte for byte the check of a fresh
/// database of the edited text, and the syntax lints of the walk of the pieces the sweep left
/// unclean equal those of the whole walk, the case of every piece unclean: the syntax lints
/// and the syntax group's passes of `checks::check` (`macros`, `feature expansion`, `ast
/// validation`, `feature gates`, `attributes`, `checked attributes`). Controls:
/// [`Plant::CleanOutlineSkipsBodies`] is caught by a fixture whose outline is clean and whose
/// body has a lint; [`Plant::SettingsWithoutTools`], the settings a probe is given leaving out
/// `#![register_tool]`, is caught by the check, which refuses to go on when the settings it
/// reads off its walk are not those (in the refinement alone it would not be: a probe that
/// does not know a tool refuses its attribute, so its piece is only unclean, never wrongly
/// clean; a setting a probe lacks that makes it find less is what the refusal stops).
#[test]
fn the_pieces_check_equals_the_whole_check() {
    // clone: the fixtures checked so far, taken out of the lock before checking any
    let recorded = CHECKED.lock().clone();
    let mut seen: blaze_hash::HashSet<&str, Global> = blaze_hash::HashSet::new_in(Global); // alloc: a test's set of fixtures seen
    let mut fixtures = 0;
    let mut edits = 0;
    let default = Options::default();
    let own = OWN.iter().map(|it| (*it, "", &default));
    let recorded = recorded.iter().map(|(fixture, prefix, options)| (&**fixture, &**prefix, options));
    for (fixture, prefix, options) in own.chain(recorded) {
        if !seen.insert(fixture) {
            continue;
        }
        fixtures += 1;
        edits += refine(fixture, prefix, options);
    }
    assert!(fixtures >= OWN.len() && edits >= 2 * EDITS.len(), "{fixtures} fixtures, {edits} edits");

    let (db, names, _) = fixture_db(OUTLINE_CLEAN_BODY_LINTS, "");
    let rows = syntax_by_pieces(&db, &names, &default);
    assert!(rows.iter().all(|(whole, by)| whole == by), "the control's fixture holds unplanted");
    assert!(rows.iter().any(|(whole, _)| whole.contains("while_true")), "its body lints: {rows:?}");
    let (db, names, _) = fixture_db(OUTLINE_CLEAN_BODY_LINTS, "");
    plant(&db, Plant::CleanOutlineSkipsBodies);
    let rows = syntax_by_pieces(&db, &names, &default);
    assert!(rows.iter().any(|(whole, by)| whole != by), "the planted walk is caught: {rows:?}");

    let (db, names, _) = fixture_db(REGISTERED_TOOL, "");
    let rows = syntax_by_pieces(&db, &names, &default);
    assert!(rows.iter().all(|(whole, by)| whole == by), "the settings control holds unplanted");
    let (db, names, _) = fixture_db(REGISTERED_TOOL, "");
    plant(&db, Plant::SettingsWithoutTools);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        syntax_by_pieces(&db, &names, &default)
    }));
    let Err(payload) = caught else {
        panic!("settings without the registered tools are not caught");
    };
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or_default();
    assert!(message.contains("are not those its walk reads"), "caught by the refusal: {message}");
}

/// A crate whose `a.rs` has `n` functions whose bodies call no macro.
fn many_bodies(n: usize) -> String {
    let mut text = String::from("//- /main.rs crate:main\nmod a;\nfn main() {}\n//- /a.rs\n");
    for i in 0..n {
        let _ = writeln!(text, "pub fn f{i}() -> u32 {{ let x = {i}; x }}");
    }
    text
}

/// How many syntax pieces a check probed after a body edit of `f7` in [`many_bodies`]`(n)`,
/// the check before it having probed every piece, `plant` planted.
fn probed_after_a_body_edit(n: usize, plant_it: Option<Plant>) -> usize {
    let fixture = many_bodies(n);
    let (mut db, names, _) = fixture_db(&fixture, "");
    if let Some(it) = plant_it {
        plant(&db, it);
    }
    let options = Options::default();
    check_all(&db, &names, &options);
    let first = take_probed(&db);
    assert!(first >= n, "the first check probes every body: {first} of {n}");
    let a = *names.iter().find(|(_, name)| *name == "a.rs").expect("the fixture has a.rs").0;
    let text = db.file_text(a).text(&db).as_str().to_owned();
    let new = text.replacen("let x = 7; x", "let x = 7; x + 1", 1);
    assert_ne!(new, text, "the edit applies");
    db.set_file_text(a, &new);
    check_all(&db, &names, &options);
    take_probed(&db)
}

/// The invariant: after a body edit of one function in a 200-function file whose bodies call
/// no macro, `piece_clean` of the syntax group executes once, for the edited body, and
/// verifies the rest: the outline backdates, and no other body's own syntax changed. Control:
/// [`Plant::KeyedByFile`], a memo that reads its file's live tree, executes every piece of the
/// file again.
#[test]
fn a_body_edit_runs_one_piece_of_s() {
    assert_eq!(probed_after_a_body_edit(200, None), 1, "one piece probed after a body edit");
    let planted = probed_after_a_body_edit(200, Some(Plant::KeyedByFile));
    assert!(planted >= 200, "the planted memo keyed by its file runs every piece: {planted}");
}
