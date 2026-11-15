use std::path::Path;

use blaze_core::Stamp;

use super::{LookKind, Looks, holds};

fn path(it: &str) -> &Path {
    Path::new(it)
}

/// A file's stamp: changed at `changed_ns`, by a clock of nanoseconds (not whole seconds).
fn file(ino: u64, size: u64, changed_ns: u64) -> Stamp {
    Stamp { ino, size, mtime_ns: changed_ns, ctime_ns: changed_ns, mode: 0o100644 }
}

/// A planted bug of [`holds`], for the oracle's control.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    /// The racy rule is skipped: a stamp looked at in the tick it was written is trusted.
    RacyTrusted,
    /// The mode is not compared: a chmod with nothing else moved holds.
    ModeIgnored,
}

fn holds_planted(plant: Option<Plant>, was: &Stamp, looked_ns: u64, now: &Stamp) -> bool {
    match plant {
        None => holds(was, looked_ns, now),
        Some(Plant::RacyTrusted) => !now.is_unknown() && was == now,
        Some(Plant::ModeIgnored) => {
            !now.is_unknown() && Stamp { mode: now.mode, ..*was } == *now && !crate::racy(was, looked_ns)
        }
    }
}

/// The oracle: a file is unchanged since a look when no write, chmod, replace or removal
/// came after it and none came in its window ([`crate::window_ns`]) before it. Each case is
/// (what was stamped, the look's instant, the stamp now, whether the file is unchanged).
fn oracle(plant: Option<Plant>) -> Result<(), String> {
    // A sub-second stamp, whose window is the fine one.
    const CHANGED: u64 = 1_000_000_007;
    let window = crate::FINE_NS;
    let steady = file(7, 10, CHANGED);
    assert_eq!(crate::window_ns(&steady), window);
    let looked = CHANGED + 10 * window;
    let cases = [
        ("the same, steady", steady, looked, steady, true),
        ("written again, same size", steady, looked, file(7, 10, looked + 1), false),
        ("replaced by a rename", steady, looked, file(8, 10, CHANGED), false),
        ("chmodded", steady, looked, Stamp { mode: 0o100755, ..steady }, false),
        ("removed", steady, looked, Stamp::GONE, false),
        ("unreadable now", steady, looked, Stamp::UNKNOWN, false),
        ("still absent", Stamp::GONE, looked, Stamp::GONE, true),
        ("made where nothing was", Stamp::GONE, looked, steady, false),
        // Looked at in the tick it was written: a second write in that tick, to the same
        // length, leaves this stamp over other bytes.
        ("looked at in its write's tick", steady, CHANGED + window / 2, steady, false),
    ];
    for (what, was, looked_ns, now, unchanged) in cases {
        if holds_planted(plant, &was, looked_ns, &now) && !unchanged {
            return Err(format!("{what}: held"));
        }
        if plant.is_none() && !holds_planted(plant, &was, looked_ns, &now) && unchanged {
            return Err(format!("{what}: refused, which costs a read the floor does not"));
        }
    }
    Ok(())
}

/// The invariant: [`holds`] says a leaf is as it was only when its file is unchanged since
/// the look and was not written in the look's tick.
#[test]
fn a_leaf_holds_only_while_its_file_is_unchanged() {
    oracle(None).unwrap();
}

#[test]
fn the_oracle_catches_a_racy_stamp_trusted() {
    let error = oracle(Some(Plant::RacyTrusted)).unwrap_err();
    assert!(error.starts_with("looked at in its write's tick"), "{error}");
}

#[test]
fn the_oracle_catches_a_chmod_unseen() {
    let error = oracle(Some(Plant::ModeIgnored)).unwrap_err();
    assert!(error.starts_with("chmodded"), "{error}");
}

/// The invariant: a look is in the list of the build it was made for and of no other
/// ([`super::Looks`]), with no cap.
#[test]
fn outside_looks_of_two_builds_share_nothing_and_none_is_dropped() {
    let (one, two) = (Looks::new(), Looks::new());
    std::thread::scope(|scope| {
        for (looks, name) in [(&one, "one"), (&two, "two")] {
            scope.spawn(move || {
                for at in 0..1000 {
                    looks.push(Path::new(&format!("/{name}/{at}")), LookKind::File, Some([at as u8; 32]));
                }
            });
        }
    });
    for (looks, name) in [(&one, "one"), (&two, "two")] {
        let all = looks.all();
        assert_eq!(all.len(), 1000, "every look of a build is kept, past any cap");
        assert!(all.iter().all(|it| it.path.starts_with(format!("/{name}"))), "a look of another build");
        assert!(all.is_sorted_by(|a, b| a.path <= b.path), "sorted by path, whatever the order pushed");
    }
    // A clone is the same list.
    let clone = one.clone(); // clone: the test's second handle
    clone.push(path("/one/more"), LookKind::Entry, None);
    assert_eq!(one.len(), 1001);
    assert_eq!(two.len(), 1000);
}

#[test]
fn outside_looks_keep_a_path_at_two_answers_as_two_rows() {
    let looks = Looks::new();
    let stamp = file(1, 1, 1);
    looks.push_stamped(path("/a/x.h"), LookKind::File, Some([1; 32]), stamp, 5);
    looks.push(path("/a/x.h"), LookKind::File, Some([1; 32]));
    looks.push(path("/a/x.h"), LookKind::Entry, Some([1; 32]));
    looks.push(path("/a/x.h"), LookKind::Listing, None);
    let all = looks.all();
    assert_eq!(all.len(), 3, "the same look twice is one row; another kind is another");
    assert_eq!((all[0].stamp, all[0].looked_ns), (stamp, 5), "the first look's stamp is kept");
    assert!(all[1].stamp.is_unknown() && !all[1].holds(&Stamp::UNKNOWN), "a look with no stamp never holds");
    assert_eq!(Looks::twice(&all).count(), 0, "one answer a path and kind");
    looks.push(path("/a/x.h"), LookKind::File, Some([2; 32]));
    let all = looks.all();
    assert_eq!(all.len(), 4);
    let twice: Vec<_> = Looks::twice(&all).collect();
    assert_eq!(twice.len(), 1);
    assert_eq!((twice[0].0.kind, twice[0].0.answer, twice[0].1.answer), (LookKind::File, Some([1; 32]), Some([2; 32])));
    assert!(twice[0].1.by.file().ends_with("tests.rs"), "a look names its caller");
    // A build that used another's task holds that task's looks, stamps and all.
    let other = Looks::new();
    other.extend(&all);
    assert_eq!(other.all(), all);
}

#[test]
fn outside_looks_of_no_build_keep_nothing() {
    let none = Looks::none();
    none.push(path("/a/x.h"), LookKind::File, None);
    none.extend(&Looks::new().all());
    assert!(!none.keeps() && none.is_empty() && none.all().is_empty());
}
