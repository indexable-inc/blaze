//! The stability walk that steps over the quiet bodies against the whole walk: the refinement
//! over the stability fixtures and the pieces test's edit kinds, and the count of what a quiet
//! body has walked, each with its planted bug.

use std::{
    alloc::Global,
    collections::HashMap,
    sync::atomic::{AtomicBool, Ordering},
};

use base_db::Db;
use hir::HirFileId;
use span::FileId;
use syntax::TextRange;

use super::walk::{Bodies, walk};
use crate::{
    Files, Options,
    cx::Cx,
    pieces::{
        Piece,
        tests::{EDITS, edited},
    },
    tests::{CHECKED, check_all, fixture_db},
};

/// A bug planted in one database's stability walk, for the oracle that must catch it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Plant {
    /// [`super::body_quiet`] is false of every body: each is walked.
    NeverQuiet,
    /// [`super::body_quiet`] does not look at the modules the prefixes of a body's paths name.
    PrefixModulesIgnored,
}

/// The bug planted in each database, by its nonce.
static PLANTED: blaze_core::lock::Lock<Vec<(base_db::Nonce, Plant)>> =
    blaze_core::lock::Lock::new("test.diagnostics.stability.planted", Vec::new());

/// The bug planted in `db`, if any.
pub(super) fn planted(db: &Db) -> Option<Plant> {
    let nonce = db.nonce_and_revision().0;
    PLANTED.lock().iter().find(|(of, _)| *of == nonce).map(|&(_, plant)| plant)
}

fn plant(db: &Db, plant: Plant) {
    PLANTED.lock().push((db.nonce_and_revision().0, plant));
}

/// Whether any database records what the walk visits: the walk of every other test reads
/// this and no lock.
static RECORDING: AtomicBool = AtomicBool::new(false);

/// The databases whose walks record what they visit, and what each visited: the file and
/// range of each node the walk came to.
static VISITED: blaze_core::lock::Lock<Vec<(base_db::Nonce, HirFileId, TextRange)>> =
    blaze_core::lock::Lock::new("test.diagnostics.stability.visited", Vec::new());
static RECORDED: blaze_core::lock::Lock<Vec<base_db::Nonce>> =
    blaze_core::lock::Lock::new("test.diagnostics.stability.recorded", Vec::new());

/// The walk of `db` came to the node at `range` of `file` (`Walker::visit`).
pub(super) fn visited(db: &Db, file: HirFileId, range: TextRange) {
    if !RECORDING.load(Ordering::Relaxed) {
        return;
    }
    let nonce = db.nonce_and_revision().0;
    if RECORDED.lock().contains(&nonce) {
        VISITED.lock().push((nonce, file, range));
    }
}

fn record(db: &Db) {
    RECORDED.lock().push(db.nonce_and_revision().0);
    RECORDING.store(true, Ordering::Relaxed);
}

/// What the walks of `db` visited since the last take.
fn take_visited(db: &Db) -> Vec<(HirFileId, TextRange)> {
    let nonce = db.nonce_and_revision().0;
    let mut visited = VISITED.lock();
    let mut own = Vec::new();
    visited.retain(|&(of, file, range)| {
        let mine = of == nonce;
        if mine {
            own.push((file, range));
        }
        !mine
    });
    own
}

/// What one crate's two walks found and how many of its bodies are quiet.
struct Walked {
    whole: String,
    skipping: String,
    quiet: usize,
}

/// Of each workspace crate of `db` that parses, the stability findings of the whole walk and
/// of the walk that steps over the quiet bodies, each as text, in a context given the check's
/// pieces as `check::run` gives them.
fn both_walks(db: &Db, names: &HashMap<FileId, String>, options: &Options) -> Vec<Walked> {
    blaze_query::db::attach(db, || {
        let mut out = Vec::new();
        for krate in hir::check::workspace_crates(db) {
            let frame = crate::tests::frame();
            let cx = Cx::new(db, krate, Files::new(names, ""), &frame).with_lints(&options.lints);
            if crate::lower::parse::parsed(&cx).fatal {
                continue;
            }
            let all: &crate::pieces::Pieces<'_> = frame.alloc(crate::pieces::pieces(&cx));
            cx.set_pieces(all);
            let whole = format!("{:?}", walk(&cx, Bodies::All));
            let skipping = format!("{:?}", walk(&cx, Bodies::QuietSkipped));
            let quiet = all
                .list
                .iter()
                .filter(|it| match it {
                    Piece::Body(owner) => super::body_quiet(db, *owner, krate.base()),
                    Piece::Outline(_) => false,
                })
                .count();
            out.push(Walked { whole, skipping, quiet });
        }
        out
    })
}

/// The refinement of one fixture's state: each crate's walk that steps over the quiet bodies
/// finds exactly what the whole walk finds. How many bodies were quiet.
fn holds(db: &Db, names: &HashMap<FileId, String>, options: &Options, what: &str) -> usize {
    let mut quiet = 0;
    for walked in both_walks(db, names, options) {
        assert_eq!(walked.whole, walked.skipping, "{what}: the stability findings by quiet bodies");
        quiet += walked.quiet;
    }
    quiet
}

/// The refinement of one fixture: before any edit and after each edit kind of its first file
/// (the database checked before the edit, as a build's is). How many bodies were quiet over
/// all of them.
fn refine(fixture: &str, prefix: &str, options: &Options) -> usize {
    let (mut db, names, first) = fixture_db(fixture, prefix);
    check_all(&db, &names, options);
    let mut quiet = holds(&db, &names, options, &format!("unedited\n{fixture}"));
    let text = db.file_text(first).text(&db).as_str().to_owned();
    for edit in EDITS {
        let Some(new) = edited(&text, edit) else { continue };
        db.set_file_text(first, &new);
        check_all(&db, &names, options);
        quiet += holds(&db, &names, options, &format!("{edit:?} of\n{fixture}"));
        db.set_file_text(first, &text);
        check_all(&db, &names, options);
    }
    quiet
}

/// The control's fixture: a stable function of a staged crate reached through its unstable
/// module, whose only finding is the E0658 of the module's segment (`Checker::visit_path`'s
/// parent segments), in a body that names nothing else deprecated or unstable.
const THROUGH_AN_UNSTABLE_MODULE: &str = r#"//- /dep.rs crate:dep edition:2021 library
#![feature(staged_api)]
#![stable(feature = "dep", since = "1.0.0")]
#[unstable(feature = "um", issue = "none")]
pub mod um {
    #[stable(feature = "dep", since = "1.0.0")]
    pub fn f() {}
}
//- /main.rs crate:other:main edition:2021 deps:dep
fn main() {
    dep::um::f();
}
"#;

/// What a quiet body is beside: a use of each kind the walk checks, deprecated, in a body
/// that is not quiet.
const DEPRECATED_USES: &str = r#"//- /main.rs crate:main edition:2021
use std::fmt;
#[deprecated]
pub fn d() {}
#[deprecated]
pub mod old {
    pub fn f() {}
    pub struct S;
}
pub mod new {
    #[deprecated(note = "gone")]
    pub fn g() {}
}
pub struct P {
    #[deprecated]
    pub a: u32,
    pub b: u32,
}
pub struct M;
impl M {
    #[deprecated]
    pub fn m(&self) {}
    pub fn n(&self) {}
}
pub trait T {
    #[deprecated]
    fn t(&self) {}
}
impl T for M {}
pub fn quiet(x: u32) -> u32 {
    let y = x + 1;
    y
}
/// Not quiet: the struct a field is read of has a deprecated field, which the quiet test
/// takes as named.
pub fn a_sibling_field_deprecated(p: P) -> u32 {
    let M = M;
    p.b
}
pub fn calls() {
    d();
    old::f();
    let _ = old::S;
    new::g();
    M.m();
    M.n();
    M.t();
}
pub fn fields(p: P) -> u32 {
    let P { a, .. } = p;
    a + p.a
}
pub fn with_macro() {
    let _ = vec![1u32];
}
pub fn nested() {
    fn inner() { d(); }
    inner();
}
pub fn show(f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("x")
}
fn main() {}
"#;

/// The fixtures the refinement always checks: its own, and rustc's UI tests of the stability
/// checks that need no other crate.
fn own() -> Vec<String> {
    let ui = [
        ("deprecation-in-future.rs", include_str!("../../../tests/lints/names/stability/deprecation-in-future.rs")),
        ("deprecated-macro_escape.rs", include_str!("../../../tests/lints/names/stability/deprecated-macro_escape.rs")),
        (
            "deprecated-macro_escape-inner.rs",
            include_str!("../../../tests/lints/names/stability/deprecated-macro_escape-inner.rs"),
        ),
        ("unit_and_tuple_struct.rs", include_str!("../../../tests/lints/names/stability/unit_and_tuple_struct.rs")),
        (
            "stability-attribute-trait-impl.rs",
            include_str!("../../../tests/lints/names/stability/stability-attribute-trait-impl.rs"),
        ),
        ("deprecation-sanity.rs", include_str!("../../../tests/lints/names/stability/deprecation-sanity.rs")),
    ];
    let mut out = vec![THROUGH_AN_UNSTABLE_MODULE.to_owned(), DEPRECATED_USES.to_owned()];
    for (name, source) in ui {
        out.push(format!("//- /{name} crate:other:main edition:2015\n{source}"));
    }
    out
}

/// Whether a fixture the suite checked is one of the stability checks': it names what they
/// look at.
fn of_stability(fixture: &str) -> bool {
    ["deprecated", "stable(", "macro_escape"].iter().any(|it| fixture.contains(it))
}

/// The refinement: for every stability fixture (this module's, rustc's UI tests of the
/// stability checks that need no other crate, and each one naming `deprecated`, `stable(` or
/// `macro_escape` the `checked*` helpers checked before this test ran), before any edit and
/// after each edit kind of the pieces test (a body's statement added, a comment line above the
/// first item, a signature's parameter renamed, an item added, an item removed, a `use`
/// removed), the stability findings of the walk that steps over every quiet body equal byte
/// for byte those of the whole walk, and some bodies are quiet. Control:
/// [`Plant::PrefixModulesIgnored`], a quiet test blind to the unstable module a path goes
/// through, is caught by [`THROUGH_AN_UNSTABLE_MODULE`].
#[test]
fn the_walk_over_quiet_bodies_equals_the_whole_walk() {
    // clone: the fixtures checked so far, taken out of the lock before checking any
    let recorded = CHECKED.lock().clone();
    let mut seen: blaze_hash::HashSet<String, Global> = blaze_hash::HashSet::new_in(Global); // alloc: a test's set of fixtures seen
    let default = Options::default();
    let own = own();
    let mut fixtures = 0;
    let mut quiet = 0;
    let own = own.iter().map(|it| (it.as_str(), "", &default));
    let recorded = recorded
        .iter()
        .filter(|(fixture, _, _)| of_stability(fixture))
        .map(|(fixture, prefix, options)| (fixture.as_str(), prefix.as_str(), options));
    for (fixture, prefix, options) in own.chain(recorded) {
        // alloc: a test's set of fixtures seen
        if !seen.insert(fixture.to_owned()) {
            continue;
        }
        fixtures += 1;
        quiet += refine(fixture, prefix, options);
    }
    assert!(fixtures >= 8 && quiet > 0, "{fixtures} fixtures, {quiet} quiet bodies");

    let (db, names, _) = fixture_db(THROUGH_AN_UNSTABLE_MODULE, "");
    let rows = both_walks(&db, &names, &default);
    assert!(rows.iter().all(|it| it.whole == it.skipping), "the control's fixture holds unplanted");
    assert!(rows.iter().any(|it| it.whole.contains("E0658")), "its segment is unstable");
    let (db, names, _) = fixture_db(THROUGH_AN_UNSTABLE_MODULE, "");
    plant(&db, Plant::PrefixModulesIgnored);
    let rows = both_walks(&db, &names, &default);
    assert!(rows.iter().any(|it| it.whole != it.skipping), "the planted quiet test is caught");
}

/// A crate with one body that names something deprecated and one that names nothing the
/// stability checks look at.
const ONE_QUIET: &str = "//- /main.rs crate:main edition:2021
#[deprecated]
pub fn d() {}
pub fn loud() { d(); }
pub fn quiet() -> u32 { let x = 1; x + 2 }
fn main() {}
";

/// Of [`ONE_QUIET`] walked with the quiet bodies stepped over, `plant_it` planted: whether the
/// walk came to a node of `loud`'s body, and to one of `quiet`'s.
fn walked_bodies(plant_it: Option<Plant>) -> (bool, bool) {
    let (db, names, main) = fixture_db(ONE_QUIET, "");
    if let Some(it) = plant_it {
        plant(&db, it);
    }
    record(&db);
    let rows = both_walks(&db, &names, &Options::default());
    assert!(rows.iter().any(|it| it.whole.contains("deprecated")), "`loud` lints: {}", rows.len());
    // `both_walks` walks the whole crate first: only what the second walk visits counts.
    let _ = take_visited(&db);
    let skipping: Vec<String> = blaze_query::db::attach(&db, || {
        hir::check::workspace_crates(&db)
            .into_iter()
            .map(|krate| {
                let frame = crate::tests::frame();
                let options = Options::default();
                let cx = Cx::new(&db, krate, Files::new(&names, ""), &frame)
                    .with_lints(&options.lints);
                let all: &crate::pieces::Pieces<'_> = frame.alloc(crate::pieces::pieces(&cx));
                cx.set_pieces(all);
                format!("{:?}", walk(&cx, Bodies::QuietSkipped))
            })
            .collect()
    });
    assert!(skipping.iter().any(|it| it.contains("deprecated")), "the walk still lints `loud`");
    let text = db.file_text(main).text(&db).as_str().to_owned();
    let body = |of: &str| {
        let start = text.find(of).expect("the fixture has the body");
        TextRange::at(start.try_into().expect("a small fixture"), of.len().try_into().expect("a small fixture"))
    };
    let (loud, quiet) = (body("{ d(); }"), body("{ let x = 1; x + 2 }"));
    let visited = take_visited(&db);
    let in_body = |cut: TextRange| {
        visited.iter().any(|&(file, range)| {
            file.file_id().is_some_and(|it| it.file_id(&db) == main) && cut.contains_range(range)
        })
    };
    (in_body(loud), in_body(quiet))
}

/// The invariant: a body naming nothing deprecated or unstable is not walked: the walk comes to
/// no node of it, and still to the nodes of a body that names something deprecated. Control:
/// [`Plant::NeverQuiet`], every body walked, comes to the quiet body's nodes.
#[test]
fn a_quiet_body_is_not_walked() {
    assert_eq!(walked_bodies(None), (true, false), "the quiet body is stepped over");
    assert_eq!(walked_bodies(Some(Plant::NeverQuiet)), (true, true), "planted: every body walked");
}
