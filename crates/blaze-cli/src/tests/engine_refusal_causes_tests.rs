//! Every refusal says its cause (lane RH3): a check scope cut or not read names the crate and
//! why (engine_bodies.rs `scope_of`, `CutCause`, `ScopeRefusal`); a body refused at planning
//! prints its label (`planned_label`); a head's constant from a dependency or a macro expansion
//! is placed (engine_head_consts.rs `text_cell`); the bodies' seed freezer evaluates constants
//! (engine_bodies.rs `run_stretch`, `add_eval_reads`); a body refused for a head that failed
//! when first frozen quotes that head's own reason (`own_reason`); and whether types and
//! borrows were checked whole is a fact of the answer, never of this request's work alone
//! (engine_check.rs, engine_lines.rs `not_checked_whole`). A child of main.rs's tests, over
//! engine_check_tests.rs's helpers.

use super::engine_check_tests::{engine_opts, entries, failed_dep, opts, said, three_crates};
use super::*;

/// One request with `opts` as given: its whole printed text and its counts.
fn printed_with(io: &Io, opts: engine_check::Opts<'static>) -> (std::string::String, engine_check::Checked) {
    let before = stdout(io).len();
    let (_, checked) = engine_opts(io, opts);
    (stdout(io).split_off(before), checked)
}

/// The records of the bodies the last check of `io` did not check (D164): its noted findings
/// that count a body unchecked, whatever their rule.
fn refused(io: &Io) -> std::vec::Vec<super::world_run::Noted> {
    use blaze_report::frame::words::Kind;
    super::world_run::noted(io).into_iter().filter(|one| matches!(one.kind, Kind::Unchecked | Kind::Grouped | Kind::Failure)).collect()
}

/// (a) A DEPENDENCY'S NAMES RECORD THAT CANNOT BE READ IS NAMED, WITH ITS ERROR: `a`'s names
/// record planted unreadable (engine_records.rs `scope_records`) in the chain `c` -> `b` -> `a`:
/// every refused body's note names `a` and says its record could not be read, and no line says
/// "has no names record" (the words of a crate with no record at all). Rule 23: without
/// piece 1 `scope_of` cut the scope at `a` with no cause (engine_bodies.rs `scope_of`, the
/// `Err(_) => break` of the records read) and the notes said "has no names record".
/// (c) A BODY REFUSED AT PLANNING PRINTS ITS LABEL: `a`'s own bodies are refused before any
/// record named them (its own scope not read) and print `f` and `k`, never "the body of ``".
/// Rule 23: without piece 2 the refusal pushed in `run_phase` kept the planned task's empty label.
#[test]
fn an_unreadable_dependency_record_is_named_with_its_error() {
    let io = Io::simulated(SimWorld::new(41, Limits::USUAL));
    three_crates(&io);
    let mut planted = opts(&io, 2, true);
    planted.unreadable = Some("a");
    let (text, checked) = printed_with(&io, planted);
    println!("`a` unreadable: {}\n{text}", entries(&checked));
    // The records (D164): each refused body's cause key names `a` and the read error.
    let notes = refused(&io);
    for member in ["b/src/lib.rs", "c/src/lib.rs"] {
        let mine: std::vec::Vec<&super::world_run::Noted> = notes.iter().filter(|one| one.file == member).collect();
        assert!(!mine.is_empty(), "{member}'s bodies are refused (`a` is in its scope): {notes:?}\n{text}");
        for one in mine {
            assert!(one.of.contains("`a`") && one.of.contains("could not be read"), "{member}'s cause names `a` and the read error: {one:?}");
        }
    }
    assert!(notes.iter().all(|one| !one.of.contains("has no names record")), "a record that was there but did not read is not said as none: {notes:?}");
    // (c) `a`'s own bodies, refused at planning: their subjects are their labels.
    let own: std::vec::Vec<(&str, &str)> = notes.iter().filter(|one| one.file == "a/src/lib.rs" || one.file == "a/src/m.rs").map(|one| (one.file.as_str(), one.item.as_str())).collect();
    assert!(!own.is_empty(), "`a`'s own bodies are refused (its own record did not read): {notes:?}");
    assert!(own.iter().all(|(_, item)| !item.is_empty()), "a body refused at planning has its label: {own:?}");
    assert!(own.contains(&("a/src/lib.rs", "f")) && own.contains(&("a/src/m.rs", "k")), "`a`'s bodies by their labels: {own:?}");
    // The renderer's snapshot: the default view's line names a body by its label.
    assert!(!text.contains("the body of `` ") && text.contains("`f` (a/src/lib.rs)"), "a body refused at planning prints its label:\n{text}");
    // CONTROL: unplanted, the same project checks clean (the plant is the cause).
    let (clean, _) = printed_with(&io, opts(&io, 2, true));
    let after = refused(&io);
    assert!(after.is_empty(), "control: no body refused without the plant: {after:?}\n{clean}");
}

/// (b) A CRATE WHOSE NAMES TASK REALLY FAILED IS SAID AS SUCH: `p` (a proc-macro crate the
/// shelf cannot build) fails; its dependents' notes name `p` and its `CrateScope` task. Rule 23:
/// without piece 1 the note said only "has no names record" (engine_bodies.rs `CutCause`,
/// `CutWhy::NoNamesRecord { failed: true }`).
#[test]
fn a_failed_names_task_is_named_in_its_dependents_notes() {
    let io = Io::simulated(SimWorld::new(43, Limits::USUAL));
    failed_dep(&io);
    let (text, checked) = printed_with(&io, opts(&io, 2, true));
    println!("p fails: {}\n{text}", entries(&checked));
    let notes = refused(&io);
    for member in ["b/src/lib.rs", "c/src/lib.rs"] {
        assert!(
            notes.iter().any(|one| one.file == member && one.of.contains("holds `p`") && one.of.contains("its `CrateScope` task failed")),
            "{member}'s cause names `p` and its failed task: {notes:?}\n{text}"
        );
    }
}

/// (d) A MEMBER HEAD'S CONSTANT FROM A DEPENDENCY AND FROM A MACRO EXPANSION IS PLACED:
/// `takes_dep(x: [u8; ::core::FIXTURE_LEN])` (a toolchain crate's constant) and
/// `takes_mac(x: [u8; M])` (`M` written by `make!`) are both frozen, and `uses` checks clean.
/// Rule 23: without piece 3 the evaluation's read of either text had no cell
/// (engine_head_consts.rs `text_cell`, "a constant its head holds is of no crate of its scope").
#[test]
fn a_head_constant_of_a_dependency_or_an_expansion_is_placed() {
    let io = Io::simulated(SimWorld::new(47, Limits::USUAL));
    write(&io, "Cargo.toml", "[package]\nname = \"heads\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(
        &io,
        "src/lib.rs",
        "macro_rules! make {\n    () => {\n        pub const M: usize = 3;\n    };\n}\n\nmake!();\n\npub fn takes_dep(x: [u8; ::core::FIXTURE_LEN]) -> u8 {\n    x[0]\n}\n\npub fn takes_mac(x: [u8; M]) -> u8 {\n    x[0]\n}\n\npub fn uses() -> u8 {\n    takes_dep([1; 2]) + takes_mac([1; 3])\n}\n",
    );
    let (_, lines, checked) = said(&io, true);
    let text = lines.join("\n");
    println!("{}\n{text}", entries(&checked));
    assert!(!text.contains("could not be frozen"), "both heads frozen:\n{text}");
    assert!(!text.contains("no crate of its scope"), "both constants placed:\n{text}");
    assert!(checked.bodies.0.is_empty(), "every body checked clean: {:?}\n{text}", checked.bodies.0);
}

/// One package `seeded`: `uses` calls the toolchain's `fixture_len`, whose head holds a
/// constant (blaze-world fixture.rs `CORE`): a head the body's seed freezes, never a head task.
pub(super) fn toolchain_constant(io: &Io) {
    write(io, "Cargo.toml", "[package]\nname = \"seeded\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "src/lib.rs", "pub fn uses() -> u8 {\n    ::core::fixture_len([1; 2])\n}\n");
}

/// (e) THE SEED FREEZER EVALUATES A CONSTANT, UNDER BOTH STRATEGIES (rule 78): `fixture_len`'s
/// head is frozen at the body's seed (`HeadsFrom::Frozen`) or lowered by the body
/// (`HeadsFrom::Lowered`), and `uses` checks clean under each, with equal notes. Rule 23:
/// without piece 4 the Frozen side said "refused at a seed for a constant: ... the freeze has
/// no constant evaluator" (engine_bodies.rs `run_stretch`, `consts: None`).
#[test]
fn the_seed_freezer_evaluates_a_constant_under_both_strategies() {
    let io = Io::simulated(SimWorld::new(53, Limits::USUAL));
    toolchain_constant(&io);
    let statuses: std::vec::Vec<_> = [(crate::engine_seeds::HeadsFrom::Frozen, "Frozen"), (crate::engine_seeds::HeadsFrom::Lowered, "Lowered")]
        .into_iter()
        .map(|(from, shown)| {
            let mut each = opts(&io, 2, true);
            each.heads_from = from;
            let (text, checked) = printed_with(&io, each);
            println!("{shown}: {}\n{text}", entries(&checked));
            assert!(!text.contains("no constant evaluator"), "{shown}: the freeze has its evaluator:\n{text}");
            assert!(checked.bodies.0.is_empty(), "{shown}: `uses` checks clean: {:?}\n{text}", checked.bodies.0);
            checked.status.0
        })
        .collect();
    assert_eq!(statuses[0], statuses[1], "both strategies end with one status");
}

/// One package `again`: `bad`'s signature holds a constant with no value, and two other
/// members of its impl call it, each body asking `bad`'s head in one stretch (one worker).
pub(super) fn asked_twice(io: &Io) {
    write(io, "Cargo.toml", "[package]\nname = \"again\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(
        io,
        "src/lib.rs",
        "pub struct S;\n\npub const BAD: usize = not_defined();\n\nimpl S {\n    pub fn bad(&self, x: [u8; BAD]) -> u8 {\n        x[0]\n    }\n\n    pub fn one(&self, x: [u8; 4]) -> u8 {\n        self.bad(x)\n    }\n\n    pub fn two(&self, x: [u8; 4]) -> u8 {\n        self.bad(x)\n    }\n}\n",
    );
}

/// (f) A BODY REFUSED FOR A HEAD THAT FAILED WHEN FIRST FROZEN QUOTES THAT HEAD'S OWN REASON:
/// every note of a body that reads `bad`'s head carries the constant's reason ("the value of a
/// constant its type holds", or the first freeze's "needs the value of constant `BAD`"), and
/// none says only that it "failed when first frozen". Rule 23: without piece 5 the second ask's
/// note was `FreezeError::Again`'s own words, "not made again, as cell .. failed when first
/// frozen" (engine_bodies.rs `run_stretch`, the seed-refused words), with no reason.
#[test]
fn a_head_asked_again_quotes_its_own_reason() {
    let io = Io::simulated(SimWorld::new(59, Limits::USUAL));
    asked_twice(&io);
    let (text, checked) = printed_with(&io, opts(&io, 1, true));
    println!("{}\n{text}", entries(&checked));
    // The records (D164): each body that reads `bad`'s head is refused, its cause key quoting
    // the head's own reason.
    let notes = refused(&io);
    let readers: std::vec::Vec<&super::world_run::Noted> = notes.iter().filter(|one| one.item == "one" || one.item == "two").collect();
    assert!(!readers.is_empty(), "control: the bodies that call `bad` are refused: {notes:?}\n{text}");
    for one in readers {
        assert!(one.of.contains("the value of a constant its type holds") || one.of.contains("needs the value of constant `"), "the cause quotes `bad`'s own reason: {one:?}");
        assert!(!one.of.contains("failed when first frozen"), "never only that it failed before: {one:?}");
    }
}

/// The coverage fixture: one body refused (`calls_bad`, its head's constant has no value) and
/// clean ones, in `src/lib.rs` as `text`.
const COVERAGE: &str = "pub struct S;\n\npub const BAD: usize = not_defined();\n\nimpl S {\n    pub fn good(&self) -> u8 {\n        1\n    }\n\n    pub fn bad(&self, x: [u8; BAD]) -> u8 {\n        x[0]\n    }\n}\n\npub fn calls_good(s: &S) -> u8 {\n    s.good()\n}\n\npub fn calls_bad(s: &S, x: [u8; 4]) -> u8 {\n    s.bad(x)\n}\n\npub fn other() -> u8 {\n    7\n}\n";
/// (a) a blank line added above `other`.
const COVERAGE_BLANK: &str = "pub struct S;\n\npub const BAD: usize = not_defined();\n\nimpl S {\n    pub fn good(&self) -> u8 {\n        1\n    }\n\n    pub fn bad(&self, x: [u8; BAD]) -> u8 {\n        x[0]\n    }\n}\n\npub fn calls_good(s: &S) -> u8 {\n    s.good()\n}\n\npub fn calls_bad(s: &S, x: [u8; 4]) -> u8 {\n    s.bad(x)\n}\n\n\npub fn other() -> u8 {\n    7\n}\n";
/// (b) a clean body's literal edited (`other` returns 8).
const COVERAGE_LITERAL: &str = "pub struct S;\n\npub const BAD: usize = not_defined();\n\nimpl S {\n    pub fn good(&self) -> u8 {\n        1\n    }\n\n    pub fn bad(&self, x: [u8; BAD]) -> u8 {\n        x[0]\n    }\n}\n\npub fn calls_good(s: &S) -> u8 {\n    s.good()\n}\n\npub fn calls_bad(s: &S, x: [u8; 4]) -> u8 {\n    s.bad(x)\n}\n\n\npub fn other() -> u8 {\n    8\n}\n";

/// The lines that say a stage was not checked whole.
fn not_checked(lines: &[std::string::String]) -> std::vec::Vec<std::string::String> {
    // The notes alone: the closing line also says "NOT checked: ..", beside this request's
    // time and count of tasks, which differ between an incremental and a fresh request.
    lines.iter().filter(|line| line.starts_with("note: NOT checked: ")).cloned().collect()
}

/// AN INCREMENTAL REQUEST NEVER PRINTS A PASS FOR A STAGE THAT DID NOT RUN WHOLE (the owner's
/// rule; engine_check.rs `bodies_whole`): after a cold request of a project with one refused
/// body, (a) a blank line added above another fn, (b) a clean body's literal edited and (c)
/// nothing changed (the certificate) each print exactly the fresh run's "NOT checked: types" /
/// "NOT checked: borrows" lines and its status, while `calls_bad`'s refusal stands from the
/// stored answer. Rule 23: with `whole()` reading only this request's bodies (engine_bodies.rs
/// `BodiesStage::whole`, before the standing rows were counted), (a) and (b) entered only clean
/// bodies and printed no "NOT checked" line (lane RX5's run 214226).
#[test]
fn a_standing_refusal_keeps_its_stage_not_whole() {
    let io = Io::simulated(SimWorld::new(61, Limits::USUAL));
    write(&io, "Cargo.toml", "[package]\nname = \"cover\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "src/lib.rs", COVERAGE);
    let (_, cold_lines, cold) = said(&io, false);
    println!("cold: {}\n{}", entries(&cold), cold_lines.join("\n"));
    let cold_not = not_checked(&cold_lines);
    assert!(!cold_not.is_empty(), "control: the cold request has a stage not checked whole:\n{}", cold_lines.join("\n"));
    for (row, text) in [("(a) a blank line", Some(COVERAGE_BLANK)), ("(b) a literal edited", Some(COVERAGE_LITERAL)), ("(c) nothing changed", None)] {
        if let Some(text) = text {
            write(&io, "src/lib.rs", text);
        }
        let (_, now_lines, now) = said(&io, false);
        let (_, fresh_lines, fresh) = said(&io, true);
        println!("{row}: {}\n{}\nfresh:\n{}", entries(&now), now_lines.join("\n"), fresh_lines.join("\n"));
        assert!(now_lines.iter().any(|line| line.contains("calls_bad")), "{row}: `calls_bad`'s refusal stands:\n{}", now_lines.join("\n"));
        assert_eq!(not_checked(&now_lines), not_checked(&fresh_lines), "{row}: the fresh run's NOT-checked lines, no fewer");
        assert_eq!(now.status.0, fresh.status.0, "{row}: the fresh run's status");
    }
    // (c) after the fresh request: nothing changed is the certificate.
    let (_, cert_lines, cert) = said(&io, false);
    assert_eq!(not_checked(&cert_lines), cold_not, "the certificate prints the cold request's NOT-checked lines");
    println!("certificate: {} (certified {})", entries(&cert), cert.certified);
}

/// Two members, `a` and `b`, each with one body reading the toolchain's `FixtureBad`, whose
/// field's length (`FIXTURE_BAD = 0 - 1`) does not evaluate (blaze-world fixture.rs `CORE`): a
/// dependency's head no head task freezes, frozen (and failed) by each crate's body stretch.
fn two_readers_of_a_failed_dependency_head(io: &Io) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
    write(io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "a/src/lib.rs", "pub fn first(x: &::core::FixtureBad) -> u8 {\n    x.x[0]\n}\n");
    write(io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "b/src/lib.rs", "pub fn second(x: &::core::FixtureBad) -> u8 {\n    x.x[0]\n}\n");
}

/// A DEPENDENCY HEAD'S FAILURE KEEPS ITS REASON ACROSS STRETCHES, WORKERS AND CALLERS: on 1
/// and on 8 workers, `first` (in `a`) and `second` (in `b`, another stretch) are each refused
/// with a note quoting `FixtureBad`'s own reason ("the value of a constant its type holds"),
/// none says "was not kept" or "never told why", and the closing lines group the failed head
/// ("heads: 1 head(s) failed: ..: first `..FixtureBad`"), the head a body froze, not a head task.
/// Rule 23: a body's check first asks the head on the spot (front2 world_items.rs
/// `store_want`, check.rs `make`), which drops the freeze's error and leaves it to the round's
/// fill; that fill's ask is `FreezeError::Again`, which carries no reason. With the reason kept
/// only where a body's task error carried it (the per-stretch table before), every body met
/// only `Again` and printed "its reason was not kept"; without `RequestSeed::seed` telling the
/// request (front2 tables/seeds.rs, `RequestHeads::refused`), each prints "never told why".
#[test]
fn a_failed_dependency_head_keeps_its_reason_for_every_reader() {
    for workers in [1, 8] {
        let io = Io::simulated(SimWorld::new(67, Limits::USUAL));
        two_readers_of_a_failed_dependency_head(&io);
        let (text, checked) = printed_with(&io, opts(&io, workers, true));
        println!("{workers} worker(s): {}\n{text}", entries(&checked));
        let notes = refused(&io);
        for (member, body) in [("a/src/lib.rs", "first"), ("b/src/lib.rs", "second")] {
            // Its record (D164), said in its own words.
            let note = notes.iter().find(|one| one.file == member && one.item == body).map(|one| one.words.as_str()).unwrap_or_else(|| panic!("{workers}: `{body}` is refused with a record: {notes:?}\n{text}"));
            // Since a body never freezes a member's head itself (lanes RU1..RU3), `first` is
            // refused for ITS OWN head (`a::first`, whose signature names `FixtureBad`: its head
            // task failed for the constant's reason), not for `core::FixtureBad` frozen by its
            // seed: the note names the member head and quotes the same reason. OWED: a row whose
            // body alone names `FixtureBad` (not its signature), so a dependency head frozen by a
            // seed and failing is exercised again (the path this test was written for).
            assert!((note.contains("FixtureBad`") || note.contains(&std::format!("::{body}`"))) && note.contains("the value of a constant its type holds"), "{workers}: `{body}`'s note quotes the head's own reason: {note}");
        }
        assert!(!text.contains("was not kept"), "{workers}: no reason lost below the cap:\n{text}");
        assert!(!text.contains("never told why"), "{workers}: every failure told the request:\n{text}");
        let grouped: std::vec::Vec<&str> = text.lines().filter(|line| line.contains("heads: ") && line.contains("head(s) failed: ")).collect();
        assert_eq!(grouped.len(), 1, "{workers}: one grouped reason:\n{text}");
        // One reason, three heads: `core::FixtureBad` (frozen by the bodies' seeds) and the two
        // fns whose signatures name it (`a::first`, `b::second`: their head tasks fail for the
        // same cause), as the first run printed: "heads: 3 head(s) failed: ..: first `a::first`,
        // `b::second`, `core::FixtureBad`".
        // Under the strict member-head rule the group is the two member heads (`a::first`,
        // `b::second`): no body's seed reaches `core::FixtureBad` any more (see above).
        assert!(grouped[0].contains("head(s) failed: ") && grouped[0].contains("the value of a constant its type holds") && grouped[0].contains("`a::first`") && grouped[0].contains("`b::second`"), "{workers}: the group names each failed head once: {}", grouped[0]);
    }
}

/// Refusal classes ignore item keys, expression IDs, type IDs and byte ranges, while keeping
/// distinct words separate. The most frequent class is printed first, as the profile promises.
#[test]
fn refusal_profile_groups_normalized_causes() {
    use crate::engine_bodies::{refused_cause, Said};
    use bstd::mem::{Frame, Tag};

    let frame = Frame::with_reserve(1 << 16, Tag::Other).expect("the profile test's frame");
    let notes = [
        Said::Refused("scope ItemKey(abcdef1234) at ExprId(3): PTy(4), bytes 12..24"),
        Said::Refused("scope ItemKey(deadbeef) at ExprId(9): PTy(17), bytes 88..99"),
        Said::Refused("different ItemKey(aa55) at ExprId(2): PTy(6)"),
    ];
    let mut grouped = std::collections::BTreeMap::<std::string::String, usize>::new();
    for note in &notes {
        let cause = refused_cause(note, None, &frame);
        *grouped.entry(std::string::String::from(cause.as_str())).or_default() += 1;
    }
    let mut lines: std::vec::Vec<std::string::String> = grouped
        .into_iter()
        .map(|(cause, count)| std::format!("bodies: refused by cause: {count} {cause}"))
        .collect();
    lines.sort_by(|left, right| right.cmp(left));
    assert_eq!(
        lines,
        [
            "bodies: refused by cause: 2 Refused: scope ItemKey(#) at ExprId(#): PTy(#), bytes #..#",
            "bodies: refused by cause: 1 Refused: different ItemKey(#) at ExprId(#): PTy(#)",
        ],
        "the profile classes normalize keys and IDs, then order by count",
    );
}
