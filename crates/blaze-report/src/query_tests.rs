use super::{Counts, Outcome, groups, outcome, sort};
use crate::finding::{Cause, Finding, Subject};
use crate::frame::words::{Kind, Policy};

fn body(file: &'static str, item: &'static str, cause: Cause<'static>) -> Finding<'static> {
    Finding::of_cause(Subject::Item { file, item }, cause)
}

fn lines(findings: &mut [Finding<'_>]) -> Vec<String> {
    sort(findings);
    groups(findings)
        .map(|group| {
            let mut line = String::new();
            group.line(&mut line).expect("a group has its words");
            line
        })
        .collect()
}

/// The default view's lines equal what engine_lines.rs `under_lines` printed for the same
/// bodies (its format strings, copied): sorted by cause, a body held twice named once.
#[test]
fn a_group_line_is_the_old_line() {
    let call = Cause::RefusedCall { call: "the call of `make` (function-like) in src/lib.rs failed: no expansion" };
    let plan = Cause::PlanRefused { why: "its member's check scope holds `a`, which has no names record" };
    let mut findings = vec![body("src/lib.rs", "there", call), body("src/lib.rs", "here", call), body("src/lib.rs", "here", call), body("src/b.rs", "one", plan)];
    assert_eq!(
        lines(&mut findings),
        [
            "1 body not checked because its member's check scope holds `a`, which has no names record: `one` (src/b.rs)".to_owned(),
            "2 bodies not checked because the call of `make` (function-like) in src/lib.rs failed: no expansion: `here` (src/lib.rs), `there` (src/lib.rs)".to_owned(),
        ]
    );
}

#[test]
fn a_group_names_five_and_counts_the_rest() {
    let call = Cause::RefusedCall { call: "c" };
    let items = ["a", "b", "c", "d", "e", "f", "g"];
    let mut findings: Vec<Finding<'static>> = items.iter().map(|item| body("x.rs", *item, call)).collect();
    assert_eq!(lines(&mut findings), ["7 bodies not checked because c: `a` (x.rs), `b` (x.rs), `c` (x.rs), `d` (x.rs), `e` (x.rs), .. (and 2 more)".to_owned()]);
    let mut five: Vec<Finding<'static>> = items[..5].iter().map(|item| body("x.rs", *item, call)).collect();
    assert!(!lines(&mut five)[0].contains("more"), "five are all named");
}

#[test]
fn a_crate_and_a_file_have_their_nouns() {
    let why = Cause::PlanRefused { why: "w" };
    let mut findings = vec![Finding::of_cause(Subject::Crate { name: "alpha" }, why), Finding::of_cause(Subject::Crate { name: "beta" }, why)];
    assert_eq!(lines(&mut findings), ["2 crates not checked because w: crate `alpha`, crate `beta`".to_owned()]);
    let mut file = vec![Finding::of_cause(Subject::File { path: "a.rs" }, why)];
    assert_eq!(lines(&mut file), ["1 file not checked because w: `a.rs`".to_owned()]);
}

#[test]
fn the_status_is_a_function_of_the_findings() {
    let trusting = Policy { borrows_are_errors: true };
    let not_yet = Policy { borrows_are_errors: false };
    let of = |kinds: &[Kind], policy: Policy, incomplete: bool| {
        let mut counts = Counts::default();
        for kind in kinds {
            counts.add(*kind, policy);
        }
        outcome(&counts, incomplete)
    };
    assert_eq!(of(&[], not_yet, false), Outcome::Ok);
    assert_eq!(of(&[Kind::Warning, Kind::BorrowsUnchecked, Kind::Told, Kind::Waited], not_yet, false), Outcome::Ok);
    assert_eq!(of(&[], not_yet, true), Outcome::Incomplete);
    assert_eq!(of(&[Kind::Grouped], not_yet, true), Outcome::ItemsNotChecked);
    assert_eq!(of(&[Kind::Unchecked, Kind::Error], not_yet, false), Outcome::Failed);
    assert_eq!(of(&[Kind::Failure], not_yet, false), Outcome::Failed);
    assert_eq!(of(&[Kind::Borrows], not_yet, false), Outcome::Ok);
    assert_eq!(of(&[Kind::Borrows], trusting, false), Outcome::Failed);
}
