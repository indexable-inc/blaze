//! A dependency with records in the store is never resolved by the daemon that finds them
//! (restore.md, F2): the front end awaits the crates lowered into it
//! (`blaze_lower::lower_awaiting`, `hir_def::deps::awaited`), and the serve that asks about a
//! crate reads it from its image in the write that releases it.

use blaze_ids::ContentId;

use super::{super::deps, d_ran, root_values, script_cfg_front};

/// How many passes name resolution ran `krate` in since the records were last taken.
fn resolved(lowered: &mut blaze_lower::Lowered, krate: base_db::Crate) -> usize {
    let mut all: Vec<base_db::Maintained> = Vec::new();
    lowered.served_db().take_maintained(&mut all);
    all.iter().filter(|it| it.maintainer == "nameres" && it.krate == krate).count()
}

/// The invariant: a crate lowered into a daemon's front end is awaited until a serve asks
/// about it, so neither the lowering nor a build script's run applied resolves it. The first
/// daemon, over an empty store, resolves `d` once, in the serve's write (before F2: once as
/// it was lowered, again after its script's run, again after the serve); the next daemon,
/// which finds `d`'s image, never resolves it: name resolution reports no pass over `d`, and
/// `d`'s names are read from the image.
///
/// The count's control is the first daemon, where the same counter is 1; that a front end
/// which awaits nothing resolves at the add is blaze-lower's
/// `the_control_a_front_end_that_awaits_nothing_resolves_every_crate_as_it_is_added`.
#[test]
fn a_dependency_with_records_is_never_resolved_by_the_daemon_that_finds_them() {
    let mut sim = blaze_core::sim::Sim::new(1, blaze_core::sim::Faults::default());
    let mut store = deps::tests::opened(&mut sim);
    // A reader sees what is put after it is made.
    let reader = store.reader();
    let macros = deps::tests::no_macros();
    let build = ContentId::of("blaze v2 cargo test build", b"a dependency with records");
    let mode = hir_def::deps::Mode::Serve;

    // One daemon, an empty store: `d` waits through the lowering and its script's run, and is
    // resolved once, by the serve that finds no image of it.
    let mut first = script_cfg_front();
    first.served_db().keep_maintained(true);
    let d = first.krate(2).unwrap();
    assert!(hir_def::deps::is_awaited(first.db(), d), "lowered, not asked about");
    first.ran(1, &d_ran(&[])).unwrap();
    assert!(hir_def::deps::is_awaited(first.db(), d), "its script's run applied, not asked about");
    assert_eq!(resolved(&mut first, d), 0, "nothing resolved `d` before the serve");
    let mut session = deps::session(&first, &reader, &macros, &[]).unwrap();
    let served = deps::images(&mut first, &[d], build, &mut session, mode)
        .unwrap()
        .expect("`d` is keyed");
    assert_eq!(served.hits, Vec::new(), "a cold store");
    assert!(!hir_def::deps::is_awaited(first.db(), d), "asked about: released");
    assert_eq!(resolved(&mut first, d), 1, "resolved once, by the serve that found no image");
    assert_eq!(root_values(&first, d), ["slow"]);
    let kept = deps::Kept { served, mode };
    let (puts, _) = deps::store(&first, Some(&kept), &mut session, &[], "/w").unwrap();
    for it in puts {
        store.put(it.put.key, it.put.kind, &it.put.bytes).unwrap();
    }
    drop((session, first));

    // The next daemon finds the image: `d` is never resolved.
    let mut next = script_cfg_front();
    next.served_db().keep_maintained(true);
    next.ran(1, &d_ran(&[])).unwrap();
    let d = next.krate(2).unwrap();
    let mut session = deps::session(&next, &reader, &macros, &[]).unwrap();
    let served = deps::images(&mut next, &[d], build, &mut session, mode)
        .unwrap()
        .expect("`d` is keyed");
    assert_eq!(served.hits, [d], "`d` is read from its image");
    assert_eq!(root_values(&next, d), ["slow"]);
    assert_eq!(resolved(&mut next, d), 0, "name resolution never ran `d`");
}
