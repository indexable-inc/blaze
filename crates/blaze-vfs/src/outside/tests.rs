use std::{path::Path, time::Duration};

use blaze_core::{
    Completion, Done, Driver, Effect, FileName, IoError, Tag,
    sim::{Faults, Sim},
};

use super::{Dirs, FLUSH, Outside, STAT, UNCONFIRMED, WATCH};

const TAGS: u64 = 1 << 40;

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

fn path(it: &str) -> &Path {
    Path::new(it)
}

/// Runs the simulation until nothing is left to complete, handing every completion to `outside`.
fn pump(sim: &mut Sim, outside: &mut Outside<'_>, effects: &mut Vec<Effect>) {
    let mut done = Vec::new();
    loop {
        sim.submit(effects);
        if sim.poll(&mut done, Duration::ZERO) == 0 {
            return;
        }
        for completion in done.drain(..) {
            assert!(outside.owns(completion.tag), "{completion:?}");
            outside.complete(completion, effects);
        }
    }
}

/// A round run to its end.
fn round(sim: &mut Sim, outside: &mut Outside<'_>, effects: &mut Vec<Effect>) {
    outside.flush(effects);
    pump(sim, outside, effects);
    assert!(outside.flushed());
}

/// A simulation with `tc/lib/a.h` on its disk and a table wanting `/tc`, watched and confirmed.
fn confirmed(dirs: &Dirs) -> (Sim, Outside<'_>, Vec<Effect>) {
    let mut sim = Sim::new(1, Faults::default());
    sim.change(&name("tc/lib/a.h"), b"int a;");
    dirs.want(path("/tc"));
    let mut outside = Outside::new(dirs, "/", TAGS);
    let mut effects = Vec::new();
    round(&mut sim, &mut outside, &mut effects);
    assert!(dirs.dir(0).since() < UNCONFIRMED, "the watch is confirmed");
    (sim, outside, effects)
}

#[test]
fn a_confirmed_watch_vouches_for_what_is_under_it() {
    let dirs = Dirs::new();
    let (mut sim, mut outside, mut effects) = confirmed(&dirs);
    let generation = dirs.generation();
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    // A round with nothing changed keeps it.
    round(&mut sim, &mut outside, &mut effects);
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    // A generation from before the watch was confirmed is not vouched for.
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), dirs.dir(0).since() - 1));
}

#[test]
fn a_change_reported_after_a_stamp_stops_vouching() {
    let dirs = Dirs::new();
    let (mut sim, mut outside, mut effects) = confirmed(&dirs);
    let generation = dirs.generation();
    sim.change(&name("tc/lib/a.h"), b"int b;");
    pump(&mut sim, &mut outside, &mut effects);
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    assert!(!dirs.unchanged_since(path("/tc/lib/other.h"), generation), "the whole directory");
    // Read after the change was heard, a generation is vouched for again.
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), dirs.generation()));
    // A change elsewhere is not one under it.
    let generation = dirs.generation();
    sim.change(&name("elsewhere/b.h"), b"int c;");
    round(&mut sim, &mut outside, &mut effects);
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), generation));
}

#[test]
fn a_watch_not_yet_confirmed_never_vouches() {
    let dirs = Dirs::new();
    let mut sim = Sim::new(2, Faults::default());
    sim.change(&name("tc/lib/a.h"), b"int a;");
    dirs.want(path("/tc"));
    let mut outside = Outside::new(&dirs, "/", TAGS);
    let mut effects = Vec::new();
    outside.flush(&mut effects);
    assert!(matches!(effects[..], [Effect::Watch { .. }, Effect::Stat { .. }]), "{effects:?}");
    // Nothing of the round is a flush: it is in at once, and the watch is not yet confirmed.
    assert!(outside.flushed());
    assert_eq!(dirs.dir(0).since(), UNCONFIRMED);
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), dirs.generation()));
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), u64::MAX - 1));
    pump(&mut sim, &mut outside, &mut effects);
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), dirs.generation()));
}

#[test]
fn a_directory_not_there_is_confirmed_once_it_is() {
    let dirs = Dirs::new();
    let mut sim = Sim::new(3, Faults::default());
    dirs.want(path("/tc"));
    let mut outside = Outside::new(&dirs, "/", TAGS);
    let mut effects = Vec::new();
    round(&mut sim, &mut outside, &mut effects);
    assert_eq!(dirs.dir(0).since(), UNCONFIRMED, "a stat of nothing confirms nothing");
    sim.change(&name("tc/a.h"), b"int a;");
    pump(&mut sim, &mut outside, &mut effects);
    assert!(dirs.dir(0).since() < UNCONFIRMED, "the change under it asks again");
    assert!(dirs.unchanged_since(path("/tc/a.h"), dirs.generation()));
}

#[test]
fn a_lost_event_stops_vouching() {
    let dirs = Dirs::new();
    let mut sim = Sim::new(4, Faults { lose_change_one_in: 1, ..Faults::default() });
    sim.change(&name("tc/lib/a.h"), b"int a;");
    dirs.want(path("/tc"));
    let mut outside = Outside::new(&dirs, "/", TAGS);
    let mut effects = Vec::new();
    round(&mut sim, &mut outside, &mut effects);
    let generation = dirs.generation();
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    sim.change(&name("tc/lib/a.h"), b"int b;");
    round(&mut sim, &mut outside, &mut effects);
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    assert!(!dirs.unchanged_since(path("/tc/x"), generation), "lost names nothing: all of it");
    // What was dropped may have been its own end: it is watched and confirmed again.
    assert!(dirs.dir(0).since() > generation);
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), dirs.generation()));
}

#[test]
fn an_unwatch_stops_vouching_until_watched_again() {
    let dirs = Dirs::new();
    let (mut sim, mut outside, mut effects) = confirmed(&dirs);
    let generation = dirs.generation();
    outside.complete(Completion { tag: outside.tag(0, WATCH), done: Done::Unwatched }, &mut effects);
    assert_eq!(dirs.dir(0).since(), UNCONFIRMED);
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    assert!(effects.is_empty(), "made again by the next round, not at once: {effects:?}");
    round(&mut sim, &mut outside, &mut effects);
    assert!(dirs.dir(0).since() > generation, "confirmed again, after the break");
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), dirs.generation()));
}

#[test]
fn a_failed_watch_stops_vouching_and_is_asked_again_later() {
    let dirs = Dirs::new();
    let (mut sim, mut outside, mut effects) = confirmed(&dirs);
    let generation = dirs.generation();
    let failed = Done::Failed(IoError("injected".into()));
    outside.complete(Completion { tag: outside.tag(0, WATCH), done: failed }, &mut effects);
    assert_eq!(dirs.dir(0).since(), UNCONFIRMED);
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    // Backed off one round, then watched and confirmed again.
    for _ in 0..2 {
        round(&mut sim, &mut outside, &mut effects);
    }
    assert!(dirs.dir(0).since() > generation);
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), dirs.generation()));
}

#[test]
fn a_failed_flush_stops_vouching_and_ends_the_round() {
    let dirs = Dirs::new();
    let (_sim, mut outside, mut effects) = confirmed(&dirs);
    let generation = dirs.generation();
    outside.flush(&mut effects);
    let flush = outside.tag(0, FLUSH);
    assert_eq!(effects, vec![Effect::Flush { watch: outside.tag(0, WATCH), tag: flush }]);
    effects.clear();
    assert!(!outside.flushed());
    let failed = Done::Failed(IoError("injected".into()));
    outside.complete(Completion { tag: flush, done: failed }, &mut effects);
    assert!(outside.flushed());
    assert_eq!(dirs.dir(0).since(), UNCONFIRMED);
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation));
}

#[test]
fn a_change_naming_the_directory_itself_watches_it_again() {
    let dirs = Dirs::new();
    let (mut sim, mut outside, mut effects) = confirmed(&dirs);
    let generation = dirs.generation();
    let replaced = Done::Changed { files: vec![name("tc")], lost: false };
    outside.complete(Completion { tag: outside.tag(0, WATCH), done: replaced }, &mut effects);
    assert_eq!(dirs.dir(0).since(), UNCONFIRMED);
    assert_eq!(effects, vec![Effect::Unwatch { tag: outside.tag(0, WATCH) }]);
    pump(&mut sim, &mut outside, &mut effects);
    // Ended, then watched and stat'ed again at once.
    assert!(dirs.dir(0).since() > generation);
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), dirs.generation()));
}

#[test]
fn a_change_made_before_a_flush_is_seen_once_it_completes() {
    for seed in 0..64 {
        let dirs = Dirs::new();
        let mut sim = Sim::new(seed, Faults { max_latency: Duration::from_millis(5), ..Faults::default() });
        sim.change(&name("tc/lib/a.h"), b"int a;");
        dirs.want(path("/tc"));
        let mut outside = Outside::new(&dirs, "/", TAGS);
        let mut effects = Vec::new();
        round(&mut sim, &mut outside, &mut effects);
        let generation = dirs.generation();
        // Changed on the disk; its event is still on its way.
        sim.change(&name("tc/lib/a.h"), b"int b;");
        outside.flush(&mut effects);
        assert!(!outside.flushed(), "seed {seed}");
        let mut done = Vec::new();
        while !outside.flushed() {
            sim.submit(&mut effects);
            assert!(sim.poll(&mut done, Duration::ZERO) > 0, "seed {seed}: the flush never came");
            for completion in done.drain(..) {
                outside.complete(completion, &mut effects);
            }
            assert!(
                outside.flushed() || !dirs.unchanged_since(path("/tc/lib/a.h"), generation),
                "seed {seed}: vouched while the round is out"
            );
        }
        assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation), "seed {seed}");
    }
}

#[test]
fn nothing_vouches_while_a_round_is_out() {
    let dirs = Dirs::new();
    let (mut sim, mut outside, mut effects) = confirmed(&dirs);
    let generation = dirs.generation();
    outside.flush(&mut effects);
    assert!(!outside.flushed());
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), dirs.generation()));
    // A round asked while one is out: the flush still in flight covers less, and is asked again.
    sim.submit(&mut effects);
    outside.flush(&mut effects);
    assert!(effects.is_empty(), "one flush in flight at a time: {effects:?}");
    pump(&mut sim, &mut outside, &mut effects);
    assert!(outside.flushed());
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), generation));
}

#[test]
fn nothing_vouches_before_the_first_round_or_once_undriven() {
    let dirs = Dirs::new();
    dirs.want(path("/tc"));
    assert!(!dirs.unchanged_since(path("/tc/a.h"), dirs.generation()));
    let generation = {
        let (_sim, _outside, _effects) = confirmed(&dirs);
        let generation = dirs.generation();
        assert!(dirs.unchanged_since(path("/tc/a.h"), generation));
        generation
    };
    assert_eq!(dirs.dir(0).since(), UNCONFIRMED);
    assert!(!dirs.unchanged_since(path("/tc/a.h"), generation));
}

#[test]
fn ancestors_are_found_and_only_whole_components() {
    let dirs = Dirs::new();
    let (_sim, _outside, _effects) = confirmed(&dirs);
    let generation = dirs.generation();
    for under in ["/tc", "/tc/", "/tc/lib", "/tc/lib/a.h", "/tc/lib/deep/er/x.h"] {
        assert!(dirs.unchanged_since(path(under), generation), "{under}");
    }
    for not in ["/tcx", "/tcx/a.h", "/t", "/", "/other/tc/a.h", ""] {
        assert!(!dirs.unchanged_since(path(not), generation), "{not}");
    }
}

#[test]
fn a_directory_the_root_cannot_name_is_never_watched() {
    let dirs = Dirs::new();
    dirs.want(path("/elsewhere/include"));
    dirs.want(path("/root/tc"));
    let mut sim = Sim::new(5, Faults::default());
    sim.change(&name("tc/a.h"), b"int a;");
    sim.change(&name("elsewhere/include/b.h"), b"int b;");
    let mut outside = Outside::new(&dirs, "/root", TAGS);
    let mut effects = Vec::new();
    outside.flush(&mut effects);
    assert_eq!(
        effects,
        vec![
            Effect::Watch { dir: name("tc"), tag: outside.tag(1, WATCH) },
            Effect::Stat { file: name("tc"), tag: outside.tag(1, STAT) },
        ]
    );
    pump(&mut sim, &mut outside, &mut effects);
    let generation = dirs.generation();
    assert!(dirs.unchanged_since(path("/root/tc/a.h"), generation));
    assert!(!dirs.unchanged_since(path("/elsewhere/include/b.h"), generation));
    round(&mut sim, &mut outside, &mut effects);
    assert_eq!(dirs.dir(0).since(), UNCONFIRMED);
}

#[test]
fn a_directory_is_wanted_once_and_found_past_growth() {
    let dirs = Dirs::new();
    dirs.want(path("/"));
    assert!(dirs.is_empty(), "the file system's root is not watched");
    dirs.want(path("/tc"));
    dirs.want(path("/tc/"));
    dirs.want(path("/tc/./"));
    assert_eq!(dirs.len(), 1);
    for at in 0..5000 {
        dirs.want(Path::new(&format!("/many/{at}")));
    }
    assert_eq!(dirs.len(), 5001);
    for at in 0..5000 {
        let wanted = format!("/many/{at}");
        let key = wanted.as_bytes();
        let dir = dirs.find(key, super::tag(key)).unwrap_or_else(|| panic!("{wanted}"));
        assert_eq!(dir.path(), Path::new(&wanted));
    }
    assert!(dirs.find(b"/many/5000", super::tag(b"/many/5000")).is_none());
    assert!(dirs.find(b"/tc", super::tag(b"/tc")).is_some());
}

#[test]
fn a_directory_under_one_wanted_is_covered_by_it() {
    let dirs = Dirs::new();
    dirs.want(path("/tc"));
    dirs.want(path("/tc/lib"));
    dirs.want(path("/tc/lib/deep"));
    assert_eq!(dirs.len(), 1);
    dirs.want(path("/tcx"));
    assert_eq!(dirs.len(), 2, "a sibling with the same start is not under it");
}

#[test]
fn a_directory_wanted_above_others_retires_their_watches() {
    let dirs = Dirs::new();
    let mut sim = Sim::new(6, Faults::default());
    sim.change(&name("sdk/include/a.h"), b"int a;");
    sim.change(&name("sdk/lib/b.h"), b"int b;");
    dirs.want(path("/sdk/include"));
    dirs.want(path("/sdk/lib"));
    let mut outside = Outside::new(&dirs, "/", TAGS);
    let mut effects = Vec::new();
    round(&mut sim, &mut outside, &mut effects);
    assert!(dirs.dir(0).since() < UNCONFIRMED && dirs.dir(1).since() < UNCONFIRMED);

    dirs.want(path("/sdk"));
    assert_eq!((dirs.dir(0).subsumed_by(), dirs.dir(1).subsumed_by()), (Some(2), Some(2)));
    outside.flush(&mut effects);
    let unwatched: Vec<Tag> = effects
        .iter()
        .filter_map(|it| match it {
            Effect::Unwatch { tag } => Some(*tag),
            _ => None,
        })
        .collect();
    assert_eq!(unwatched, vec![outside.tag(0, WATCH), outside.tag(1, WATCH)]);
    assert!(effects.contains(&Effect::Watch { dir: name("sdk"), tag: outside.tag(2, WATCH) }));
    let flushes = effects.iter().filter(|it| matches!(it, Effect::Flush { .. })).count();
    assert_eq!(flushes, 0, "retired watches are not flushed, and the new one is not confirmed yet");
    pump(&mut sim, &mut outside, &mut effects);
    // One watch, one flush per round from now on.
    outside.flush(&mut effects);
    assert_eq!(effects, vec![Effect::Flush { watch: outside.tag(2, WATCH), tag: outside.tag(2, FLUSH) }]);
    pump(&mut sim, &mut outside, &mut effects);
    assert!(outside.flushed());
    assert_eq!((dirs.dir(0).since(), dirs.dir(1).since()), (UNCONFIRMED, UNCONFIRMED));
    let generation = dirs.generation();
    assert!(dirs.unchanged_since(path("/sdk/include/a.h"), generation));
    assert!(dirs.unchanged_since(path("/sdk/lib/b.h"), generation));
    sim.change(&name("sdk/lib/b.h"), b"int c;");
    round(&mut sim, &mut outside, &mut effects);
    assert!(!dirs.unchanged_since(path("/sdk/lib/b.h"), generation));
}

#[test]
fn a_cover_answers_as_the_path_does_without_a_walk() {
    let dirs = Dirs::new();
    assert!(dirs.cover(path("/tc/lib/a.h")).is_none(), "nothing wanted");
    let (mut sim, mut outside, mut effects) = confirmed(&dirs);
    let cover = dirs.cover(path("/tc/lib/a.h")).expect("under /tc");
    assert_eq!(cover.dir().path(), Path::new("/tc"));
    assert!(dirs.cover(path("/other/a.h")).is_none());
    let generation = dirs.generation();
    assert!(cover.unchanged_since(generation));
    outside.flush(&mut effects);
    assert!(!cover.unchanged_since(generation), "a round is out");
    pump(&mut sim, &mut outside, &mut effects);
    assert!(cover.unchanged_since(generation));
    sim.change(&name("tc/lib/a.h"), b"int b;");
    round(&mut sim, &mut outside, &mut effects);
    assert!(!cover.unchanged_since(generation));
    assert_eq!(cover.unchanged_since(generation), dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    assert!(cover.unchanged_since(dirs.generation()));
}

#[test]
fn a_tag_is_the_same_whole_or_found_as_a_prefix() {
    let path = b"/usr/include/c++/v1/vector";
    let mut found = Vec::new();
    super::prefixes(path, |end, tag| {
        found.push((end, tag));
        true
    });
    let ends: Vec<usize> = found.iter().map(|it| it.0).collect();
    assert_eq!(ends, vec![4, 12, 16, 19, 26]);
    for (end, tag) in found {
        assert_eq!(tag, super::tag(&path[..end]), "{}", String::from_utf8_lossy(&path[..end]));
    }
}

#[cfg(not(blaze_check))] // many threads, long loops
#[test]
fn reads_race_with_wants_and_find_every_published_directory() {
    let dirs = Dirs::new();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for at in 0..2000 {
                dirs.want(Path::new(&format!("/race/{at}")));
            }
        });
        for _ in 0..2 {
            scope.spawn(|| {
                let mut seen = 0;
                while seen < 2000 {
                    let len = dirs.len();
                    for at in seen..len {
                        let wanted = format!("/race/{at}");
                        let key = wanted.as_bytes();
                        assert!(dirs.find(key, super::tag(key)).is_some(), "{wanted}");
                    }
                    seen = len;
                }
            });
        }
    });
}

/// A check on another thread while the driver adds a directory, confirms it and ends a round:
/// under every interleaving and every value a load may read, it finds the directory whole or
/// not at all, and vouches only for a generation read after the confirmation.
#[cfg(blaze_check)]
#[test]
fn model_a_check_sees_a_directory_whole_and_vouches_only_after_it_is_confirmed() {
    use std::sync::Arc;

    blaze_check::model(|| {
        let dirs = Arc::new(Dirs::new());
        let reader = {
            let dirs = dirs.clone();
            crate::sync::thread::spawn(move || {
                let generation = dirs.generation();
                if let Some(dir) = dirs.find(b"/tc", super::tag(b"/tc")) {
                    assert_eq!(dir.path(), Path::new("/tc"));
                }
                if dirs.unchanged_since(path("/tc/a.h"), generation) {
                    assert!(generation >= 1, "vouched for a generation before the confirmation");
                }
            })
        };
        dirs.want(path("/tc"));
        dirs.dir(0).entry.since.store(dirs.bump(), crate::sync::atomic::Ordering::Release);
        dirs.settle(true);
        reader.join().unwrap();
    });
}

/// A check on another thread while the writer appends a second directory to the columns: it
/// finds it whole, its path's bytes and all, or not at all, and finds the first throughout.
#[cfg(blaze_check)]
#[test]
fn model_a_directory_appended_is_found_whole_or_not_at_all() {
    use std::sync::Arc;

    blaze_check::model(|| {
        let dirs = Arc::new(Dirs::new());
        dirs.want(path("/tc"));
        let reader = {
            let dirs = dirs.clone();
            crate::sync::thread::spawn(move || {
                let len = dirs.len();
                assert!(dirs.find(b"/tc", super::tag(b"/tc")).is_some());
                if let Some(dir) = dirs.find(b"/sdk", super::tag(b"/sdk")) {
                    assert_eq!(dir.path(), Path::new("/sdk"));
                    assert_eq!(dir.since(), UNCONFIRMED);
                }
                if len == 2 {
                    assert_eq!(dirs.dir(1).path(), Path::new("/sdk"));
                }
            })
        };
        dirs.want(path("/sdk"));
        reader.join().unwrap();
        assert_eq!(dirs.len(), 2);
    });
}

#[cfg(not(blaze_check))] // the model's atomics keep state of their own
#[test]
fn checks_wants_and_rounds_allocate_nothing() {
    use blaze_alloc_count::assert_at_most;

    let dirs = Dirs::new();
    let (mut sim, mut outside, mut effects) = confirmed(&dirs);
    let header = path("/tc/lib/deep/er/a.h");
    let generation = assert_at_most(0, "generation", || dirs.generation());
    let cover = assert_at_most(0, "a cover found", || dirs.cover(header)).expect("under /tc");
    assert!(assert_at_most(0, "a check through a cover", || cover.unchanged_since(generation)));
    assert!(assert_at_most(0, "a check by path", || dirs.unchanged_since(header, generation)));
    assert!(!assert_at_most(0, "a check of a path nothing covers", || {
        dirs.unchanged_since(path("/elsewhere/include/b.h"), generation)
    }));
    assert_at_most(0, "a want of a directory wanted", || dirs.want(path("/tc")));
    assert_at_most(0, "a want of a directory covered", || dirs.want(path("/tc/lib/deep")));

    // A round of confirmed watches, into a buffer kept from the last, and its flushes heard.
    effects.reserve(8);
    let mut done = Vec::with_capacity(8);
    for _ in 0..3 {
        assert_at_most(0, "a round asked", || outside.flush(&mut effects));
        assert_eq!(effects.len(), 1);
        while !outside.flushed() {
            sim.submit(&mut effects);
            sim.poll(&mut done, Duration::ZERO);
            for completion in done.drain(..) {
                assert_at_most(0, "a flush heard", || outside.complete(completion, &mut effects));
            }
        }
    }

    // A change heard, once the driver made its report.
    sim.change(&name("tc/lib/a.h"), b"int b;");
    let mut heard = false;
    while !heard {
        sim.submit(&mut effects);
        assert!(sim.poll(&mut done, Duration::ZERO) > 0, "the change was never told");
        for completion in done.drain(..) {
            heard |= matches!(completion.done, Done::Changed { .. });
            assert_at_most(0, "a change heard", || outside.complete(completion, &mut effects));
        }
    }
    assert!(!cover.unchanged_since(generation));
}

/// Wants past every column's high-water mark (its first commit step and the index's tables
/// growing level by level) commit pages of the table's own regions and never call the global
/// allocator; what was found before the growth is found after it, at the same address.
#[cfg(not(blaze_check))] // the model's atomics keep state of their own
#[test]
fn wants_past_the_high_water_mark_allocate_nothing() {
    use std::path::PathBuf;

    use blaze_alloc_count::assert_at_most;

    let dirs = Dirs::new();
    dirs.want(path("/tc"));
    let first: *const super::Entry = dirs.dir(0).entry;
    // More entries than a 64 KB commit step holds (2048), and past index level 7.
    let wanted: Vec<PathBuf> = (0..5000).map(|at| PathBuf::from(format!("/grown/{at:04}"))).collect();
    assert_at_most(0, "wants past the high-water mark", || {
        for dir in &wanted {
            dirs.want(dir);
        }
    });
    assert_eq!(dirs.len(), 5001);
    assert!(dirs.level.load(crate::sync::atomic::Ordering::Relaxed) >= 7);
    assert!(std::ptr::eq(first, dirs.dir(0).entry), "an entry never moves");
    for (at, dir) in wanted.iter().enumerate() {
        assert_eq!(dirs.dir(at as u32 + 1).path(), dir.as_path());
        let key = dir.as_os_str().as_encoded_bytes();
        let found = assert_at_most(0, "a find past growth", || dirs.find(key, super::tag(key)));
        assert_eq!(found.map(|it| it.path()), Some(dir.as_path()));
    }
    assert!(blaze_mem::tag::snapshot().get(blaze_mem::Tag::Vfs).live > 0, "counted under vfs");
}

/// The columns against a reference built the plain way: the same directories in the same
/// order, each found by its bytes and none of the paths only under them.
#[cfg(not(blaze_check))]
#[test]
fn the_columns_answer_as_a_plain_table_does() {
    use std::collections::HashMap;

    use blaze_ids::rng::SplitMix64;

    let dirs = Dirs::new();
    let mut reference: Vec<String> = Vec::new();
    let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    for _ in 0..3000 {
        let seed = rng.next_u64();
        let depth = 1 + seed % 3;
        let mut wanted = String::new();
        for part in 0..depth {
            wanted.push_str(&format!("/d{}", (seed >> (8 * part)) % 40));
        }
        let covered = reference.iter().any(|it| {
            wanted == *it || (wanted.starts_with(it.as_str()) && wanted.as_bytes()[it.len()] == b'/')
        });
        dirs.want(path(&wanted));
        if !covered {
            reference.push(wanted);
        }
    }
    assert_eq!(dirs.len() as usize, reference.len());
    let numbered: HashMap<&str, u32> =
        reference.iter().enumerate().map(|(at, it)| (it.as_str(), at as u32)).collect();
    for (at, wanted) in reference.iter().enumerate() {
        assert_eq!(dirs.dir(at as u32).path(), Path::new(wanted));
        let key = wanted.as_bytes();
        assert!(dirs.find(key, super::tag(key)).is_some(), "{wanted}");
        let above = reference.iter().filter(|it| {
            it.len() < wanted.len() && wanted.starts_with(it.as_str()) && wanted.as_bytes()[it.len()] == b'/'
        });
        let expected = above.map(|it| numbered[it.as_str()]).filter(|&by| by > at as u32).min();
        assert_eq!(dirs.dir(at as u32).subsumed_by(), expected, "{wanted}");
    }
}

#[test]
fn the_tags_are_its_own() {
    let dirs = Dirs::new();
    let outside = Outside::new(&dirs, "/", TAGS);
    assert!(outside.owns(Tag(TAGS)));
    assert!(outside.owns(Tag(TAGS + super::TAGS - 1)));
    assert!(!outside.owns(Tag(TAGS - 1)));
    assert!(!outside.owns(Tag(TAGS + super::TAGS)));
}

#[test]
#[should_panic(expected = "another Outside drives this table")]
fn one_driver_per_table() {
    let dirs = Dirs::new();
    let _one = Outside::new(&dirs, "/", TAGS);
    let _two = Outside::new(&dirs, "/", 2 * TAGS);
}
