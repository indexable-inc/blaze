//! `Dir::walk`, the walk bounded by its caller (crates/blaze-std/src/fs/walk.rs), on the
//! simulated disk:
//!
//! (i) on random trees, a walk entering every folder hands over exactly the files published
//!     (the set the deleted whole-depth `Dir::list` answered), and in the stated order:
//!     folders depth first in part-wise name order, each folder's files in name order before
//!     those of its sub-folders; a reference walk over `Dir::entries` (breadth first, then
//!     sorted by (folder, name)) agrees;
//! (ii) a walk declining every folder named `target` asks the driver for nothing under one:
//!     the effects it hands over are exactly one `Entries` per folder not under a `target`,
//!     in pre-order, and nothing else;
//! (iii) each limit, passed, is its typed error naming the folder, and the walk answers only
//!     the error;
//! (iv) a tree 10,000 levels deep is walked without overflowing the stack;
//! (v) the planted bug (the walk enters a sub-folder the visitor declined,
//!     [`WalkPlant::EntersDeclined`]) is caught by (ii)'s oracle.

use std::collections::VecDeque;

use blaze_core::{DirName, Effect};
use blaze_sim::{Broke, Config, Tape};

use super::{CONTROL_SEEDS, path, root};
use crate::{
    fs::{Content, Dir, Durable, EntryKind, ReadWrite, WalkError, WalkLimit, WalkLimits, WalkPlant, Walked},
    io::{Io, Limits, SimWorld},
};

/// Folder names a random tree draws from: `a-b` sorts before `a/...` by joined bytes and after
/// `a` part by part, so the order's statement is tested; `target` is the one (ii) declines.
const FOLDERS: [&str; 4] = ["a", "a-b", "b", "target"];
/// File names: none is a folder's, so no path is both.
const LEAVES: [&str; 3] = ["f.rs", "g", "z.txt"];

/// Limits no test tree reaches.
const ROOMY: WalkLimits = WalkLimits { folders: 1 << 12, entries: 1 << 16, depth: 64 };

#[derive(Debug)]
struct Plan {
    seed: u64,
    /// The files published, each a path beneath the root.
    files: Vec<String>,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let mut files: Vec<String> = (0..tape.int_in(1..=24u64, "files"))
        .map(|_| {
            let mut parts: Vec<&str> = (0..tape.int_in(0..=3u64, "depth")).map(|_| *tape.one_of(&FOLDERS, "folder")).collect();
            parts.push(*tape.one_of(&LEAVES, "leaf"));
            parts.join("/")
        })
        .collect();
    files.sort();
    files.dedup();
    Plan { seed, files }
}

/// A simulated world of `seed` holding `files` beneath its root.
fn tree(seed: u64, files: &[String]) -> (Io, Dir<ReadWrite>) {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    let top = io.need_fs().expect("Io::sim offers files").root();
    let cx = root(&io);
    for file in files {
        top.publish(&cx, &path(file), Content::bytes(b"x"), Durable::No).expect("a publish");
    }
    (io, top)
}

/// The stated order of files: by (folder compared part by part, then name).
fn stated_order(files: &mut [String]) {
    files.sort_by(|x, y| {
        let (xf, xn) = x.rsplit_once('/').unwrap_or(("", x));
        let (yf, yn) = y.rsplit_once('/').unwrap_or(("", y));
        let parts = |f: &str| f.split('/').filter(|it| !it.is_empty()).map(str::to_owned).collect::<Vec<_>>();
        (parts(xf), xn).cmp(&(parts(yf), yn))
    });
}

/// The reference: every file beneath the root by `Dir::entries`, breadth first (a queue, not
/// the walk's stack), then put in the stated order.
fn reference(io: &Io, top: &Dir<ReadWrite>) -> Vec<String> {
    let cx = root(io);
    let mut files = Vec::new();
    let mut queue = VecDeque::from([top.own_entries(&cx).expect("the root lists itself")]);
    while let Some(entries) = queue.pop_front() {
        for i in 0..entries.len() {
            let at = entries.path(i).expect("a path");
            match entries.kind(i) {
                EntryKind::Dir => queue.push_back(top.entries(&cx, &at).expect("a listing").expect("the folder is there")),
                _ => files.push(at.as_str().to_owned()),
            }
        }
    }
    stated_order(&mut files);
    files
}

/// A folder's name as the effects name it: `.` for the root.
fn shown(dir: &DirName) -> String {
    dir.name().map_or(".", |name| name.as_str()).to_owned()
}

/// Every file a walk from the root entering `enter` (a folder's name) hands over, in its
/// order, and the folders it asked the driver for, in order (every effect handed over must be
/// an `Entries`).
fn walk_files(io: &Io, top: &Dir<ReadWrite>, plant: WalkPlant, enter: impl Fn(&str) -> bool) -> Result<(Vec<String>, Vec<String>), String> {
    let cx = root(io);
    let mut files = Vec::new();
    io.record();
    io.asked();
    top.walk_in(&cx, None, ROOMY, cx.frame(), plant, |entries, into| {
        for i in 0..entries.len() {
            match entries.kind(i) {
                EntryKind::Dir if enter(entries.name(i)) => into.enter(i),
                EntryKind::Dir => {}
                _ => files.push(entries.path(i).expect("a path").as_str().to_owned()),
            }
        }
        Ok::<(), core::convert::Infallible>(())
    })
    .map_err(|e| format!("the walk failed: {e:?}"))?;
    let mut asked = Vec::new();
    for effect in io.asked() {
        match effect {
            Effect::Entries { dir, .. } => asked.push(shown(&dir)),
            other => return Err(format!("the walk handed the driver {other:?}, not a listing")),
        }
    }
    Ok((files, asked))
}

/// (i) and (ii) on one plan.
fn run(plan: &Plan, plant: WalkPlant) -> Result<(), String> {
    let (io, top) = tree(plan.seed, &plan.files);
    // (i) Entering everything: the files published, in the stated order, as the reference.
    let (all, _) = walk_files(&io, &top, plant, |_| true)?;
    let mut published = plan.files.clone();
    stated_order(&mut published);
    if all != published {
        return Err(format!("(i) the walk handed over {all:?}, the tree holds {published:?} (in the stated order)"));
    }
    let by_entries = reference(&io, &top);
    if all != by_entries {
        return Err(format!("(i) the walk handed over {all:?}, the reference over `entries` found {by_entries:?}"));
    }
    // (ii) Declining `target`: one listing per folder not under one, in pre-order, nothing else.
    let (_, asked) = walk_files(&io, &top, plant, |name| name != "target")?;
    let mut folders: Vec<Vec<&str>> = Vec::new();
    for file in &plan.files {
        let parts: Vec<&str> = file.split('/').collect();
        for n in 1..parts.len() {
            let folder = &parts[..n];
            if !folder.contains(&"target") && !folders.iter().any(|it| it.as_slice() == folder) {
                folders.push(folder.to_vec());
            }
        }
    }
    folders.sort();
    let expected: Vec<String> = core::iter::once(".".to_owned()).chain(folders.iter().map(|it| it.join("/"))).collect();
    if asked != expected {
        return Err(format!("(ii) the walk declining `target` asked for {asked:?}, the folders outside one are {expected:?}"));
    }
    Ok(())
}

/// The refinement: `Dir::walk` yields every file in the stated order matching the reference walk and never enters or lists declined folders.
/// (i) and (ii): on random trees, a walk entering every folder hands over exactly the files
/// published, in the stated order, as a reference walk over `entries` finds them; a walk
/// declining `target` lists exactly the folders outside one, in pre-order, and asks nothing
/// else. Prints nothing; 200 cases pass.
#[test]
fn a_walk_hands_over_every_file_in_order_and_lists_nothing_declined() {
    blaze_sim::check(Config::cases(200), |tape| {
        let plan = plan(tape);
        if let Err(why) = run(&plan, WalkPlant::None) {
            panic!("{why}\nreplay: plan {plan:?}");
        }
    });
}

/// (v): the planted walk that enters the declined `target` folders is caught by (ii)'s
/// oracle on some seeded plan, and the clean walk passes all of them.
#[test]
fn the_oracle_catches_a_walk_that_enters_a_declined_folder() {
    blaze_sim::control(Config::cases(CONTROL_SEEDS), WalkPlant::None, WalkPlant::EntersDeclined, "", |tape, plant| {
        let plan = plan(tape);
        run(&plan, plant).map_err(|why| Broke::Oracle(format!("{why}\nplan {plan:?}")))
    });
}

/// A checkout's shape: sources beside a build folder.
const CHECKOUT: [&str; 6] = [
    "Cargo.toml",
    "src/lib.rs",
    "src/m/x.rs",
    "target/debug/deps/a.rlib",
    "target/debug/build/b/out/c.o",
    "target/release/y",
];

/// (ii) on a checkout's shape, with the counts in the open. Prints:
///
/// ```text
/// folders asked: 3 (declining target): [".", "src", "src/m"]
/// folders asked: 10 (the whole checkout, on purpose)
/// ```
#[test]
fn a_walk_declining_target_lists_three_folders_of_a_checkout() {
    let files: Vec<String> = CHECKOUT.iter().map(|it| (*it).to_owned()).collect();
    let (io, top) = tree(5, &files);
    let (found, asked) = walk_files(&io, &top, WalkPlant::None, |name| name != "target").expect("the walk");
    println!("folders asked: {} (declining target): {asked:?}", asked.len());
    assert_eq!(asked, [".", "src", "src/m"]);
    assert_eq!(found, ["Cargo.toml", "src/lib.rs", "src/m/x.rs"]);
    let (found, asked) = walk_files(&io, &top, WalkPlant::None, |_| true).expect("the whole walk");
    println!("folders asked: {} (the whole checkout, on purpose)", asked.len());
    assert_eq!(asked.len(), 10, "{asked:?}");
    assert_eq!(found.len(), CHECKOUT.len(), "{found:?}");
}

/// (iii): each limit, passed by one, is its typed error naming the folder where it was
/// passed; the walk answers the error alone. The tree: `Cargo.toml`, `src/a.rs`,
/// `src/m/b.rs`: 3 folders (`.`, `src`, `src/m`), 5 entries (2, 2, 1), 2 levels. Prints:
///
/// ```text
/// src/m: a walk passed its limit of 2 folders here (rule bounded-progress: a walk is bounded by its caller)
/// src/m: a walk passed its limit of 4 entries here (rule bounded-progress: a walk is bounded by its caller)
/// src/m: a walk passed its limit of 1 levels deep here (rule bounded-progress: a walk is bounded by its caller)
/// ```
#[test]
fn each_limit_passed_is_a_typed_error_naming_the_folder() {
    let files: Vec<String> = ["Cargo.toml", "src/a.rs", "src/m/b.rs"].iter().map(|it| (*it).to_owned()).collect();
    let (io, top) = tree(9, &files);
    let cx = root(&io);
    let walk = |limits: WalkLimits| {
        top.walk(&cx, None, limits, cx.frame(), |_, enter| {
            enter.enter_all();
            Ok::<(), core::convert::Infallible>(())
        })
    };
    assert_eq!(walk(WalkLimits { folders: 3, entries: 5, depth: 2 }), Ok(Walked { folders: 3, entries: 5 }));
    let src_m = DirName::Named(blaze_core::FileName::new("src/m").expect("a name"));
    for (limits, limit) in [
        (WalkLimits { folders: 2, entries: 5, depth: 2 }, WalkLimit::Folders(2)),
        (WalkLimits { folders: 3, entries: 4, depth: 2 }, WalkLimit::Entries(4)),
        (WalkLimits { folders: 3, entries: 5, depth: 1 }, WalkLimit::Depth(1)),
    ] {
        let refused = walk(limits).expect_err("a limit passed is an error, not a shorter answer");
        println!("{refused}");
        // clone: the expected name, a refcount.
        assert_eq!(refused, WalkError::Limit { folder: src_m.clone(), limit });
        assert!(refused.to_string().starts_with("src/m: "), "{refused}");
    }
    // No folder at all is the start's refusal.
    let refused = walk(WalkLimits { folders: 0, entries: 5, depth: 2 }).expect_err("no folder");
    assert_eq!(refused, WalkError::Limit { folder: DirName::Root, limit: WalkLimit::Folders(0) });
    // Entering a file, or an entry that is not there, is refused naming it.
    let into_file = top.walk(&cx, None, ROOMY, cx.frame(), |entries, enter| {
        enter.enter(entries.find("Cargo.toml").unwrap_or(usize::MAX));
        Ok::<(), core::convert::Infallible>(())
    });
    let cargo = DirName::Named(blaze_core::FileName::new("Cargo.toml").expect("a name"));
    assert_eq!(into_file, Err(WalkError::NotAFolder { entry: cargo, kind: EntryKind::File }));
    let past = top.walk(&cx, None, ROOMY, cx.frame(), |_, enter| {
        enter.enter(99);
        Ok::<(), core::convert::Infallible>(())
    });
    assert_eq!(past, Err(WalkError::NoSuchEntry { folder: DirName::Root, index: 99 }));
    // A start that is not there is walked as nothing.
    let none = top.walk(&cx, Some(&path("missing")), ROOMY, cx.frame(), |_, _| Ok::<(), core::convert::Infallible>(()));
    assert_eq!(none, Ok(Walked { folders: 0, entries: 0 }));
}

/// (iv): a tree 10,000 levels deep (`d/d/.../d/f`) is walked to its one file with the native
/// stack the test thread has: the walk keeps its own stack in the frame. One level short of
/// the depth it needs is refused naming the deepest folder. Prints:
///
/// ```text
/// walked 10001 folders, 10001 entries, 10000 levels deep
/// ```
#[test]
fn a_tree_ten_thousand_levels_deep_does_not_overflow_the_stack() {
    const LEVELS: usize = 10_000;
    let deepest = vec!["d"; LEVELS].join("/");
    let file = format!("{deepest}/f");
    let (io, top) = tree(13, std::slice::from_ref(&file));
    let cx = root(&io);
    let limits = WalkLimits { folders: LEVELS as u32 + 1, entries: LEVELS as u64 + 1, depth: LEVELS as u32 };
    let mut found = Vec::new();
    let walked = top
        .walk(&cx, None, limits, cx.frame(), |entries, enter| {
            for i in 0..entries.len() {
                if entries.kind(i) == EntryKind::File {
                    found.push(entries.path(i).expect("a path").as_str().to_owned());
                }
            }
            enter.enter_all();
            Ok::<(), core::convert::Infallible>(())
        })
        .expect("the deep walk");
    println!("walked {} folders, {} entries, {LEVELS} levels deep", walked.folders, walked.entries);
    assert_eq!(walked, Walked { folders: LEVELS as u32 + 1, entries: LEVELS as u64 + 1 });
    assert_eq!(found, [file]);
    let short = WalkLimits { depth: LEVELS as u32 - 1, ..limits };
    let refused = top.walk(&cx, None, short, cx.frame(), |_, enter| {
        enter.enter_all();
        Ok::<(), core::convert::Infallible>(())
    });
    let named = DirName::Named(blaze_core::FileName::new(&deepest).expect("a name"));
    assert_eq!(refused, Err(WalkError::Limit { folder: named, limit: WalkLimit::Depth(LEVELS as u32 - 1) }));
}
