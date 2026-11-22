//! D158 step 1: `check --names-oracle` decides every crate's names twice, the whole solve and
//! D116's decision on demand (blaze-scope cells/decide.rs `check`), compares every cell and
//! every name the written scopes answer (cells/agree.rs `links_agree`), and fails the command
//! on a disagreement (engine_names.rs `OracleFinding`). A child of main.rs's tests: its helpers
//! are theirs.

use super::*;
use crate::engine_names::NamesOracle;

/// One crate with the shapes the oracle must agree on: a glob cycle (`a` globs `b` and `d`,
/// `b` globs `a`) the root imports `x` through, a glob import, a `macro_rules!` that writes an
/// item, an enum's variants imported by `use`, and a `pub(in path)` item.
fn workspace(io: &Io) {
    write(io, "Cargo.toml", MANIFEST);
    write(
        io,
        "src/lib.rs",
        "mod a;\nmod b;\nmod d;\npub mod shapes;\n\n\
         pub use crate::a::x;\npub use shapes::*;\nuse shapes::Shape::{Circle, Square};\n\n\
         macro_rules! item_maker {\n    ($name:ident) => {\n        pub fn $name() -> u8 {\n            7\n        }\n    };\n}\n\n\
         item_maker!(written);\n\n\
         pub fn top(shape: Shape) -> u8 {\n    let side = match shape {\n        Circle => 1,\n        Square => 2,\n    };\n    x() + written() + side + call()\n}\n",
    );
    write(io, "src/a.rs", "pub use crate::b::*;\npub use crate::d::*;\n");
    write(io, "src/b.rs", "pub use crate::a::*;\n");
    write(io, "src/d.rs", "pub fn x() -> u8 {\n    1\n}\n");
    write(
        io,
        "src/shapes.rs",
        "pub enum Shape {\n    Circle,\n    Square,\n}\n\n\
         pub mod deep {\n    pub(in crate::shapes) fn hidden() -> u8 {\n        3\n    }\n}\n\n\
         pub fn call() -> u8 {\n    deep::hidden()\n}\n",
    );
}

/// The command on the checkout under `oracle`, answered in full: what it printed and did.
fn run(io: &Io, oracle: NamesOracle) -> (std::string::String, engine_check::Checked) {
    let opts = engine_check::Opts { names_oracle: oracle, ..super::engine_check_tests::opts(io, 2, true) };
    let before = stdout(io).len();
    let (_, checked) = super::engine_check_tests::engine_opts(io, opts);
    (stdout(io).split_off(before), checked)
}

/// Expected line: `names oracle clean: ... cell(s) in ... crate(s), 0 disagreement(s)`.
/// Clean: every cell of every crate compared (more than none) and none differs, the names
/// records agree, and the status is the check's own. CONTROL (rule 23): the on-demand decision
/// planted wrong (`NamesOracle::Planted`: a node that read an open node of the glob cycle keeps
/// its value at once) makes the mode report a disagreement naming `x` and fail the command,
/// though the whole solve, and so the answer, is the same.
#[test]
fn the_names_oracle_agrees_and_its_planted_bug_is_caught() {
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    workspace(&io);

    let (said, checked) = run(&io, NamesOracle::On);
    let t = checked.names_oracle.expect("the oracle's sums under --names-oracle");
    assert!(t.crates > 0 && t.cells > 0, "the oracle compared cells: {t:?}\n{said}");
    assert_eq!(t.disagreements(), 0, "the whole solve and the on-demand decision agree: {t:?}\n{said}");
    assert!(t.link_names > 0, "the names records were compared too: {t:?}\n{said}");
    assert!(said.contains("names oracle: "), "the oracle's line is printed:\n{said}");
    assert!(!said.contains("names oracle disagreement"), "no disagreement is listed:\n{said}");
    assert_ne!(checked.status.0, Status::FAILED.0, "a clean oracle run is the check's own status:\n{said}");
    println!("names oracle clean: {} cell(s) in {} crate(s), {} disagreement(s); {} wait(s), {} hop(s), {} name(s) against the records", t.cells, t.crates, t.disagreements(), t.waits, t.hops, t.link_names);

    let (planted, bad) = run(&io, NamesOracle::Planted);
    let p = bad.names_oracle.expect("the oracle's sums under the control");
    assert!(p.differences > 0, "the control: the planted bug is caught: {p:?}\n{planted}");
    assert!(
        planted.lines().any(|line| line.contains("names oracle disagreement") && line.contains("crate `wide`") && line.contains("`x`")),
        "the disagreement names `x` of crate `wide`:\n{planted}"
    );
    assert_eq!(bad.status.0, Status::FAILED.0, "a disagreement fails the command:\n{planted}");
    // The answer is the same: the whole solve is unplanted, so the same tasks enter.
    assert_eq!(bad.entered, checked.entered, "the plant moves the oracle alone:\n{planted}");
    println!("names oracle control: {} difference(s) caught", p.differences);
}
