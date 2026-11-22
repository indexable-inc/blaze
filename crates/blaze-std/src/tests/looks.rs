//! [`Dir::looks`](crate::fs::Dir::looks): many paths beneath a directory looked at in ONE
//! effect, each look the same as a `stat` of that path (std.md, "End state: efficiency",
//! piece 4).
//!
//! The oracle is the per-path [`Dir::stat`](crate::fs::Dir::stat) of the same world. A wrong
//! fix fails it: names not joined beneath the directory (every look gone), the looks out of the
//! order given, or one effect a path in place of one for the column (the count of effects).

use blaze_core::{Effect, FileName};
use blaze_sim::{Broke, Config, Tape};

use super::{CONTROL_SEEDS, check, path, root, world, write_synced};
use crate::{
    Plant,
    fs::{ByCount, Dir, EachPath, EntryKind, FactsError, Look, Meta, ReadWrite, SkipsLast as PlantedMerge, Strategy, Whole},
    io::Io,
    path::Path,
};

/// Prints `looks: 5 paths in 1 effect, each the same as its stat`.
#[test]
fn looks_are_one_effect_and_each_the_same_as_its_stat() {
    let io = Io::sim(41);
    let cx = root(&io);
    let fs = io.need_fs().expect("a file tree");
    let src = fs.open_dir(&cx, &path("src")).expect("the folder src");
    write_synced(&cx, &src, &path("a.rs"), b"fn a() {}").expect("src/a.rs is written");
    write_synced(&cx, &src, &path("m/b.rs"), b"fn b() -> u8 { 1 }").expect("src/m/b.rs is written");
    write_synced(&cx, &src, &path("m/c.rs"), b"").expect("src/m/c.rs is written");

    let asked: [Path; 5] = [path("a.rs"), path("gone.rs"), path("m"), path("m/b.rs"), path("m/c.rs")];
    let stats: std::vec::Vec<Option<Meta>> =
        asked.iter().map(|at| src.stat(&cx, at).expect("a stat of the oracle")).collect();

    io.record();
    io.asked();
    let looks = src.looks(&cx, asked.iter()).expect("the looks");
    let effects = io.asked();
    assert_eq!(effects.len(), 1, "one effect for the whole column, not one a path: {effects:?}");
    assert!(matches!(effects[0], Effect::Stamp { .. }), "the batched stat: {:?}", effects[0]);

    assert_eq!(looks.len(), asked.len());
    for (at, (look, stat)) in looks.iter().zip(stats.iter()).enumerate() {
        match stat {
            Some(meta) => assert!(look.same_as(meta), "path {at} ({}): look {look:?}, stat {meta:?}", asked[at].as_str()),
            None => assert_eq!(look.kind, EntryKind::Gone, "path {at} ({}) has nothing there", asked[at].as_str()),
        }
    }
    assert_eq!(looks.get(2).map(|look| look.kind), Some(EntryKind::Dir), "src/m is a directory");

    // The same paths named from the root: the same looks, so the names were joined beneath src.
    let from_root: [Path; 5] = [path("src/a.rs"), path("src/gone.rs"), path("src/m"), path("src/m/b.rs"), path("src/m/c.rs")];
    let again = fs.root().looks(&cx, from_root.iter()).expect("the looks from the root");
    assert!(looks.iter().eq(again.iter()), "looks beneath src equal looks of src/.. from the root");

    // A change to one file moves its look and no other file's. The folder `m` (index 2) is
    // not asked: a name made in it by the publish's rename may move its times, as on a disk.
    write_synced(&cx, &src, &path("m/b.rs"), b"fn b() -> u16 { 300 }").expect("src/m/b.rs is rewritten");
    let after = src.looks(&cx, asked.iter()).expect("the looks after the change");
    for at in [0, 1, 3, 4] {
        let moved = after.get(at) != looks.get(at);
        assert_eq!(moved, at == 3, "path {at} ({}): moved {moved}", asked[at].as_str());
    }

    // No path: no effect.
    io.asked();
    let none = src.looks(&cx, core::iter::empty::<&Path>()).expect("no looks");
    assert!(none.is_empty() && io.asked().is_empty(), "no path asks nothing of the driver");

    std::println!("looks: {} paths in 1 effect, each the same as its stat", asked.len());
}

// `Dir::facts_of`: sorted paths of many folders, each folder listed whole or its paths stamped.
// The oracle is again the per-path `Dir::stat` of the same world; every strategy (`ByCount`,
// `Whole`, `EachPath`) must give it row for row, so they give each other equal rows. A wrong
// merge (the planted `SkipsLast`) fails it; a listing of every name of a folder is counted.

/// Folders the plans put files in and ask of; `f` never has a file of its own unless drawn.
const FOLDERS: [&str; 7] = ["", "a", "a/b", "c", "c/d", "c/d/e", "f"];
/// Names in a folder: none is a folder's name, so no file stands where a folder is.
const LEAVES: [&str; 5] = ["v", "w.txt", "x.rs", "y", "z.rs"];

#[derive(Debug)]
struct Plan {
    seed: u64,
    /// Asked of `src`, not of the root.
    beneath: bool,
    files: std::vec::Vec<(usize, usize)>,
    asked: std::vec::Vec<(usize, usize)>,
    folders_asked: std::vec::Vec<usize>,
    /// A folder whose files are removed once written: gone by the time of the ask.
    vanish: Option<usize>,
}

fn joined(folder: &str, leaf: &str) -> std::string::String {
    match folder.is_empty() {
        true => leaf.to_string(),
        false => format!("{folder}/{leaf}"),
    }
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let beneath = tape.one_in(2, "beneath src");
    let pick = |label: &'static str| {
        let folder = tape.int_in(0..=FOLDERS.len() as u64 - 1, label) as usize;
        (folder, tape.int_in(0..=LEAVES.len() as u64 - 1, "leaf") as usize)
    };
    let files = (0..tape.int_in(0..=20u64, "files")).map(|_| pick("a file's folder")).collect();
    let asked = (0..tape.int_in(1..=24u64, "asked")).map(|_| pick("an asked path's folder")).collect();
    let folders_asked =
        (0..tape.int_in(0..=3u64, "folders asked")).map(|_| tape.int_in(1..=FOLDERS.len() as u64 - 1, "a folder asked") as usize).collect();
    let vanish = tape.one_in(3, "a folder vanishes").then(|| tape.int_in(1..=FOLDERS.len() as u64 - 1, "the vanishing folder") as usize);
    Plan { seed, beneath, files, asked, folders_asked, vanish }
}

/// The plan's world, the directory asked of, and the paths asked, sorted by their bytes.
fn build(plan: &Plan) -> Result<(Io, crate::cx::Cx, Dir<ReadWrite>, std::vec::Vec<Path>), std::string::String> {
    let io = world(plan.seed, Plant::None);
    let cx = root(&io);
    let fs = io.need_fs().map_err(|e| format!("harness: {e}"))?;
    let dir = match plan.beneath {
        true => {
            write_synced(&cx, &fs.root(), &path("src/keep"), b"").map_err(|e| format!("harness: {e}"))?;
            fs.open_dir(&cx, &path("src")).map_err(|e| format!("harness: {e}"))?
        }
        false => fs.root(),
    };
    let files: std::collections::BTreeSet<(usize, usize)> = plan.files.iter().copied().collect();
    for &(folder, leaf) in &files {
        write_synced(&cx, &dir, &path(&joined(FOLDERS[folder], LEAVES[leaf])), b"fn f() {}").map_err(|e| format!("harness: {e}"))?;
    }
    if let Some(gone) = plan.vanish {
        for &(folder, leaf) in files.iter().filter(|(folder, _)| *folder == gone) {
            dir.remove_synced(&cx, &path(&joined(FOLDERS[folder], LEAVES[leaf]))).map_err(|e| format!("harness: {e}"))?;
        }
        // A folder stays when its last file goes, as on a real disk (blaze-core's conformance
        // suite, `a_folder_outlives_its_last_file`): removed too, so the vanished folder is not
        // there, where its files made it and no folder under it holds one.
        let made = files.iter().any(|(folder, _)| *folder == gone);
        let holds_more = files.iter().any(|(folder, _)| {
            FOLDERS[*folder].strip_prefix(FOLDERS[gone]).is_some_and(|rest| rest.starts_with('/'))
        });
        if made && !holds_more {
            dir.remove_dir_synced(&cx, &path(FOLDERS[gone])).map_err(|e| format!("harness: {e}"))?;
        }
    }
    let mut texts: std::vec::Vec<std::string::String> =
        plan.asked.iter().map(|&(folder, leaf)| joined(FOLDERS[folder], LEAVES[leaf])).collect();
    texts.extend(plan.folders_asked.iter().map(|&folder| FOLDERS[folder].to_string()));
    texts.sort();
    texts.dedup();
    let asked = texts.iter().map(|text| path(text)).collect();
    Ok((io, cx, dir, asked))
}

/// The rows strategy `S` gives, none of them failed.
fn rows<S: Strategy>(cx: &crate::cx::Cx, dir: &Dir<ReadWrite>, asked: &[Path]) -> Result<std::vec::Vec<Look>, std::string::String> {
    let facts = dir.facts_of_by::<S>(cx, asked, cx.frame()).map_err(|e| format!("facts_of: {e}"))?;
    if facts.len() != asked.len() {
        return Err(format!("{} rows for {} paths", facts.len(), asked.len()));
    }
    if let Some((at, error)) = (0..facts.len()).find_map(|at| facts.failure(at).map(|error| (at, error))) {
        return Err(format!("row {at} failed with nothing refused: {error}"));
    }
    Ok(facts.iter().collect())
}

/// Each row the same as a stat of its path.
fn against_stat(cx: &crate::cx::Cx, dir: &Dir<ReadWrite>, asked: &[Path], got: &[Look], strategy: &str) -> Result<(), std::string::String> {
    for (at, (asked, look)) in asked.iter().zip(got).enumerate() {
        let stat: Option<Meta> = dir.stat(cx, asked).map_err(|e| format!("the oracle's stat: {e}"))?;
        let same = match &stat {
            Some(meta) => look.same_as(meta),
            None => look.kind == EntryKind::Gone,
        };
        if !same {
            return Err(format!("{strategy}: path {at} `{}`: row {look:?}, stat {stat:?}", asked.as_str()));
        }
    }
    Ok(())
}

fn run(plan: &Plan, _: Plant) -> Result<(), std::string::String> {
    let (_io, cx, dir, asked) = build(plan)?;
    let by_count = rows::<ByCount>(&cx, &dir, &asked)?;
    let whole = rows::<Whole>(&cx, &dir, &asked)?;
    let each = rows::<EachPath>(&cx, &dir, &asked)?;
    against_stat(&cx, &dir, &asked, &by_count, "ByCount")?;
    against_stat(&cx, &dir, &asked, &whole, "Whole")?;
    against_stat(&cx, &dir, &asked, &each, "EachPath")?;
    match whole == each && by_count == each {
        true => Ok(()),
        false => Err(format!("the strategies' rows differ: whole {whole:?}, each path {each:?}, by count {by_count:?}")),
    }
}

fn run_one<S: Strategy>(plan: &Plan) -> Result<(), std::string::String> {
    let (_io, cx, dir, asked) = build(plan)?;
    let got = rows::<S>(&cx, &dir, &asked)?;
    against_stat(&cx, &dir, &asked, &got, "one")
}

/// The refinement: batched `Dir::looks` under every strategy returns the exact same metadata as per-path `Dir::stat` for every queried path without skipping folder boundaries.
/// (i) and (ii): for random simulated trees (beneath the root or a folder, with a folder that
/// vanished) and random sorted paths in many folders, missing ones and folders among them,
/// every strategy's rows equal a stat of each path, and so each other.
/// Prints `facts: every strategy's rows equal a stat of each path and each other`.
#[test]
fn facts_equal_a_stat_of_each_path_by_every_strategy() {
    blaze_sim::check(Config::cases(200), |tape| check(plan(tape), run));
    std::println!("facts: every strategy's rows equal a stat of each path and each other");
}

#[derive(Clone, Copy, Debug)]
enum Merge {
    Right,
    SkipsLast,
}

/// (iv): the merge that skips the last asked name of a listed folder is caught by (i)'s oracle.
/// Prints `facts: a merge that skips a folder's last name is caught`.
#[test]
fn the_oracle_catches_a_merge_that_skips_a_folders_last_name() {
    blaze_sim::control(Config::cases(CONTROL_SEEDS), Merge::Right, Merge::SkipsLast, "", |tape, merge| {
        let plan = plan(tape);
        let got = match merge {
            Merge::Right => run_one::<Whole>(&plan),
            Merge::SkipsLast => run_one::<PlantedMerge>(&plan),
        };
        got.map_err(|why| Broke::Oracle(format!("{why}\nplan {plan:?}")))
    });
    std::println!("facts: a merge that skips a folder's last name is caught");
}

/// The driver calls `effects` make: a name of a stamp batch is one stat-like call, any other
/// effect one.
fn calls(effects: &[Effect]) -> usize {
    effects
        .iter()
        .map(|effect| match effect {
            Effect::Stamp { files, .. } => files.len(),
            _ => 1,
        })
        .sum()
}

/// (iii): every file of a folder of 1,000 asked: listed whole, the folder's own name in the
/// stamp and one listing, 2 calls of the effect layer (on macOS the listing is `open`, two
/// `getattrlistbulk`s of 64 KiB and the empty one, `close`: 6 system calls with the stamp);
/// stamped path by path, 1,000. Prints `folder of 1000: 2 calls (each path stamped: 1000)`.
#[test]
fn a_folder_of_1000_listed_whole_is_two_calls_not_1000() {
    let io = Io::sim(43);
    let cx = root(&io);
    let fs = io.need_fs().expect("a file tree");
    let top = fs.root();
    let mut asked = std::vec::Vec::new();
    for at in 0..1000 {
        // Zero-padded: made in the order of their bytes.
        let file = path(&format!("big/f{at:04}.rs"));
        write_synced(&cx, &top, &file, b"x").expect("a file of the folder is written");
        asked.push(file);
    }
    io.record();
    io.asked();
    let whole = top.facts_of_by::<Whole>(&cx, &asked, cx.frame()).expect("the folder listed");
    let listed = calls(&io.asked());
    let each = top.facts_of_by::<EachPath>(&cx, &asked, cx.frame()).expect("the paths stamped");
    let stamped = calls(&io.asked());
    assert_eq!((whole.listed(), whole.stamped()), (1, 1), "one listing, the folder's own name stamped");
    assert!(whole.iter().eq(each.iter()), "both strategies give equal rows");
    assert!(whole.iter().all(|look| look.kind == EntryKind::File), "every file is there");
    assert_eq!((listed, stamped), (2, 1000));
    std::println!("folder of 1000: {listed} calls (each path stamped: {stamped})");
}

/// The simulation's failures: a folder the user may not read fails its own rows, each naming
/// it, and the rest are answered; a folder gone before the ask leaves its rows gone.
/// Prints `facts: a folder that may not be read fails its 2 rows, 2 answered; a vanished folder's row is gone`.
#[test]
fn a_folder_that_may_not_be_read_fails_its_own_rows_only() {
    let io = Io::sim(44);
    let cx = root(&io);
    let fs = io.need_fs().expect("a file tree");
    let top = fs.root();
    for at in ["a/x.rs", "a/y.rs", "b/x.rs", "gone/x.rs"] {
        write_synced(&cx, &top, &path(at), b"x").expect("a file is written");
    }
    top.remove_synced(&cx, &path("gone/x.rs")).expect("the folder's one file is removed");
    io.with_sim(|sim| sim.fail_lists(FileName::new("a").expect("a name"), 1)).expect("a simulated world");

    let asked = [path("a/x.rs"), path("a/y.rs"), path("b/x.rs"), path("b/z.rs"), path("gone/x.rs")];
    let facts = top.facts_of_by::<Whole>(&cx, &asked, cx.frame()).expect("answered, a folder's rows failed");
    for at in [0, 1] {
        assert_eq!(facts.get(at).map(|look| look.kind), Some(EntryKind::Unknown), "row {at}");
        let error = facts.failure(at).expect("the failed row names its error");
        assert_eq!(error.path.name().map(FileName::as_str), Some("a"), "{error}");
        assert!(error.to_string().contains("Permission denied"), "{error}");
    }
    for at in [2, 3, 4] {
        assert!(facts.failure(at).is_none(), "row {at} is answered");
        let look = facts.get(at).expect("a row");
        match top.stat(&cx, &asked[at]).expect("the oracle's stat") {
            Some(meta) => assert!(look.same_as(&meta), "row {at}: {look:?}, stat {meta:?}"),
            None => assert_eq!(look.kind, EntryKind::Gone, "row {at}"),
        }
    }
    assert_eq!(facts.get(4).map(|look| look.kind), Some(EntryKind::Gone), "the vanished folder's file is gone");
    std::println!("facts: a folder that may not be read fails its 2 rows, 2 answered; a vanished folder's row is gone");
}

/// The order is the caller's, checked: a pair out of order, or a path twice, is refused naming
/// the pair. Prints `facts: out of order refused at 1; a path twice refused at 1`.
#[test]
fn paths_out_of_order_are_refused_naming_the_pair() {
    let io = Io::sim(45);
    let cx = root(&io);
    let top = io.need_fs().expect("a file tree").root();
    let refused = |asked: &[Path]| match top.facts_of(&cx, asked, cx.frame()) {
        Err(FactsError::Unsorted { at, before, after }) => (at, before.as_str().to_string(), after.as_str().to_string()),
        other => panic!("not refused as unsorted: {other:?}"),
    };
    assert_eq!(refused(&[path("b.rs"), path("a.rs")]), (1, "b.rs".to_string(), "a.rs".to_string()));
    assert_eq!(refused(&[path("a.rs"), path("a.rs")]), (1, "a.rs".to_string(), "a.rs".to_string()));
    std::println!("facts: out of order refused at 1; a path twice refused at 1");
}
