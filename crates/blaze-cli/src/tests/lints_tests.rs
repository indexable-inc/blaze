//! `check`'s lints (lints.rs; blaze-drive key.rs `Kind::Lint`): a lint unit is a task keyed
//! before the work by its own bytes, its holes, its scope and the step's rule set; `check` asks
//! for the units of the workspace's own crates alone; a finding prints after its file's other
//! findings, at its place, under its rule's name, as a warning that leaves the exit status as it
//! was. A child of main.rs's tests: its helpers are theirs.
//!
//! The rule tripped is `recursion` (blaze-lint, in `Step::Suggest`): a function that calls itself
//! by name. `down` does; `up` does not. The dependency `dep` (excluded from the members, reached
//! by path) holds the same `down`: linted, it would be a second finding.

use super::*;

/// The workspace root: one member, `app`; `dep` outside the members; the `suggest` step.
const LINTED: &str = "[workspace]\nmembers = [\"app\"]\nexclude = [\"dep\"]\nresolver = \"2\"\n\n[workspace.metadata.blaze]\nlints = \"suggest\"\n";
/// The same root with no step named: `Step::Rust`, no rule.
const UNLINTED: &str = "[workspace]\nmembers = [\"app\"]\nexclude = [\"dep\"]\nresolver = \"2\"\n";
const APP: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n";
const DEP: &str = "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const RECURSIVE: &str = "pub fn down(n: u8) -> u8 {\n    if n == 0 { 0 } else { down(n - 1) }\n}\n";
const OTHER: &str = "pub fn up(n: u8) -> u8 {\n    n\n}\n";
/// `OTHER` edited inside its one item, its length kept: that unit's key alone moves.
const OTHER_EDITED: &str = "pub fn up(n: u8) -> u8 {\n    7\n}\n";

fn workspace(seed: u64, root: &str) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", root);
    write(&io, "app/Cargo.toml", APP);
    write(&io, "app/src/lib.rs", "pub mod a;\npub mod b;\n");
    write(&io, "app/src/a.rs", RECURSIVE);
    write(&io, "app/src/b.rs", OTHER);
    write(&io, "dep/Cargo.toml", DEP);
    write(&io, "dep/src/lib.rs", RECURSIVE);
    io
}

/// How many of `checked`'s findings are `recursion`'s, by their words (blaze-lint message.rs:
/// "`down` calls itself (at `down`): ..."); `ids`' codes are fix.rs `Listed::code`'s, which
/// names no lint yet.
fn recursions(checked: &Checked) -> usize {
    checked.words.iter().filter(|words| words.contains("` calls itself (at `")).count()
}

/// One member item trips `recursion`: `check` prints it once, at `app/src/a.rs:1:8` (the
/// function's name: blaze-lint engine.rs files the finding over `f.name`, the call carried as
/// its argument, "(at `down`)", as blaze-lint tests.rs `recursion` pins with `("recursion",
/// "f")`), under the rule's name, as a warning: no error, and the exit status of the same
/// workspace with no step named. With no step named each lint unit's `Lint` task still runs
/// (engine_lints.rs `plan`) and nothing is printed.
/// Prints `lints: one recursion warning at app/src/a.rs:1:8, 0 errors, status Some(..) as unlinted`.
#[test]
fn a_member_item_trips_a_rule_and_check_prints_it_as_a_warning() {
    let io = workspace(41, LINTED);
    let checked = check(&io);
    let out = stdout(&io);
    assert_eq!(checked.failures, 0, "{out}\n{}", checked.profile);
    assert_eq!(recursions(&checked), 1, "one finding, `down` in app/src/a.rs:\n{:?}\n{out}", checked.ids);
    assert!(out.contains("app/src/a.rs:1:8: warning[recursion]: `down` calls itself (at `down`)"), "the finding is placed at the function's name, naming the call:\n{out}");
    assert!(out.contains("recursion"), "the finding is named by its rule:\n{out}");
    assert_eq!(checked.errors, 0, "a lint is advice, never an error:\n{out}");
    let plain = workspace(41, UNLINTED);
    let unlinted = check(&plain);
    // engine_lints.rs `plan`: one `Lint` task a FILE of each outline WHATEVER the step's rule set
    // (a file's marks may add rules the plan cannot see), so no step named still enters the
    // tasks of `app`'s 3 files (its 7 lint units listed in their records, no row a unit); they
    // find nothing.
    assert_eq!((recursions(&unlinted), unlinted.entered(blaze_engine::Stage::Lints)), (0, 3), "no step, no rule: no finding, and a Lint task each of app's 3 files (engine_lints.rs `plan`: whatever the step's rule set):\n{}", unlinted.profile);
    assert_eq!(checked.status, unlinted.status, "a lint leaves the exit status as it was");
    std::println!("lints: one recursion warning at app/src/a.rs:1:8, {} errors, status {:?} as unlinted", checked.errors, checked.status);
}

/// An unchanged repeat runs no `Lint` task and prints the finding again (from its record). An
/// edit inside `up` in ANOTHER file that keeps its length relints that one unit: `Lint` made 1.
/// Prints `lints: cold made N, repeat made 0, other item's edit made 1`.
#[test]
fn a_repeat_lints_nothing_and_an_edit_relints_its_unit_alone() {
    let io = workspace(42, LINTED);
    let cold = check(&io);
    assert_eq!(cold.failures, 0, "{}", cold.profile);
    let cold_made = cold.entered(blaze_engine::Stage::Lints);
    assert!(cold_made > 0, "the cold check lints:\n{}", cold.profile);
    let repeat = check(&io);
    assert_eq!(repeat.entered(blaze_engine::Stage::Lints), 0, "an unchanged repeat runs no lint task:\n{}", repeat.profile);
    assert_eq!(recursions(&repeat), 1, "the repeat prints the finding from its record: {:?}", repeat.ids);
    write(&io, "app/src/b.rs", OTHER_EDITED);
    let edited = check(&io);
    assert_eq!(edited.entered(blaze_engine::Stage::Lints), 1, "an edit of `up` relints `up` alone:\n{}", edited.profile);
    assert_eq!(recursions(&edited), 1, "`down`'s finding stands: {:?}", edited.ids);
    std::println!("lints: cold made {cold_made}, repeat made 0, other item's edit made 1");
}

/// The dependency `dep` holds the same `down`, and no finding is printed in it: its units are
/// never asked (`check` gates `Lint` parts by an asked root owning their file). Prints
/// `lints: nothing printed in dep/src/lib.rs`.
#[test]
fn a_dependency_is_not_linted() {
    let io = workspace(43, LINTED);
    let checked = check(&io);
    let out = stdout(&io);
    assert_eq!(checked.failures, 0, "{out}\n{}", checked.profile);
    assert!(!out.contains("dep/src/lib.rs"), "a dependency's item was linted:\n{out}");
    assert_eq!(recursions(&checked), 1, "the member's finding alone: {:?}", checked.ids);
    std::println!("lints: nothing printed in dep/src/lib.rs");
}
