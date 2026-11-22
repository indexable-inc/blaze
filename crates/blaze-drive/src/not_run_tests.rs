//! `by_cause` puts each waited task under the failure its chain comes down to.

use blaze_id::ContentId;

use super::{by_cause, Cause, NotRunError};
use crate::key::{Kind, Name};
use crate::task::{TaskFailure, Why};

fn name(kind: Kind, n: u8) -> Name {
    Name::new(kind, ContentId([n; 32]))
}

fn waited(task: Name, on: Name) -> TaskFailure {
    TaskFailure { name: task, why: Why::Waited { on } }
}

/// A script fails; its crate's scope waits on it, and a crate reading that scope waits on the
/// scope: both crates come under the SCRIPT (transitively), never under the scope. A need
/// dropped this run is a cause of its own. Prints "test ... ok"; control: in `by_cause`, break
/// the loop at the first `on` (`Some(_) => break Cause::Failed(at)`): the reader comes under
/// the scope and the first assertion fails naming the groups.
#[test]
fn a_waited_task_comes_under_the_failure_its_chain_ends_at() {
    let script = name(Kind::Script, 1);
    let scope = name(Kind::CrateScope, 2);
    let reader = name(Kind::CrateScope, 3);
    let dropped = name(Kind::Surface, 4);
    let orphan = name(Kind::CrateScope, 5);
    let failures = [
        waited(reader, scope),
        TaskFailure { name: script, why: Why::NotBuilt },
        waited(orphan, dropped),
        waited(scope, script),
    ];
    let groups = by_cause(&failures, blaze_own::testing::heap().alloc()).expect("no ring");
    let seen: std::vec::Vec<(Cause, std::vec::Vec<Name>)> = groups.iter().map(|g| (g.cause, g.waited.iter().copied().collect())).collect();
    let mut under_script = std::vec![scope, reader];
    under_script.sort_unstable();
    let mut want = std::vec![(Cause::Failed(script), under_script), (Cause::Dropped(dropped), std::vec![orphan])];
    want.sort_unstable_by_key(|(cause, _)| *cause);
    assert_eq!(seen, want, "groups by cause");
}

/// A ring of waits is refused by name, never broken at a guess (rule 3).
#[test]
fn a_ring_of_waits_is_refused() {
    let a = name(Kind::CrateScope, 1);
    let b = name(Kind::CrateScope, 2);
    let failures = [waited(a, b), waited(b, a)];
    let refused = by_cause(&failures, blaze_own::testing::heap().alloc()).map(|_| ()).expect_err("a ring");
    assert!(matches!(refused, NotRunError::Cycle { .. }), "{refused}");
}
