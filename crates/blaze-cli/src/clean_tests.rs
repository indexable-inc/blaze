//! `blaze clean` on scratch folders of the simulated world: the checkout `ws`, the home `/h`
//! (the sim's `h`), each command run as `main` runs it, its printed lines read back.

use bstd::fs::{Content, Durable};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::path::Path;
use bstd::test::BUDGET;

use super::{CleanError, NOTHING};
use crate::CliError;

/// A simulated world holding `files` (path beneath the sim's root, text).
fn world(files: &[(&str, &str)]) -> Io {
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").root();
    for (path, text) in files {
        // Each folder made as a folder, as a real disk holds one (the simulation's `publish`
        // names a file's folders without making them, and `rmdir` of such a name is refused).
        for (at, _) in path.match_indices('/') {
            let folder = Path::new(&path[..at]).expect("a path");
            if root.stat(&cx, &folder).expect("stat").is_none() {
                root.make_dir(&cx, &folder, Durable::No).expect("a folder");
            }
        }
        root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(text.as_bytes()), Durable::No).expect("write");
    }
    io
}

/// `blaze clean <words>` with `HOME=/h`, on the files `io` holds now: its result and what it
/// printed.
fn clean(io: &Io, words: &[&str]) -> (Result<bstd::entry::Status, CliError>, std::string::String) {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let before = io.script(|host| host.out.len()).expect("a simulated world");
    let result = super::run_with(&cx, io, words.iter().copied(), Some("/h"), &io.need_out().expect("out"));
    let said = io.script(|host| host.out[before..].to_vec()).expect("a simulated world");
    (result, std::string::String::from_utf8(said).expect("text"))
}

fn there(io: &Io, path: &str) -> bool {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    io.need_fs().expect("files").root().stat(&cx, &Path::new(path).expect("a path")).expect("stat").is_some()
}

const TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by blaze: its plain store.\n# For information about cache directory tags see https://bford.info/cachedir/\n";

/// A checkout with sources and blaze's state, and a home with a store and a toolchain shelf.
fn usual() -> Io {
    world(&[
        ("ws/Cargo.toml", "[package]\n"),
        ("ws/src/lib.rs", "fn f() {}\n"),
        ("ws/.blaze/v2/profile.txt", "profile\n"),
        ("ws/.blaze/v2/records/r1", "0123456789"),
        ("ws/.blaze/v2/leaves/deep/l1", "abc"),
        ("h/.blaze/v2/store/CACHEDIR.TAG", TAG),
        ("h/.blaze/v2/store/seg-0000000000000001", "segment bytes"),
        ("h/.blaze/v2/store/head", "head"),
        ("h/.blaze/v2/toolchain/abc/lib.rlib", "toolchain"),
        ("h/.blaze/v2/sources/x.crate", "source"),
    ])
}

#[test]
fn the_tag_blaze_writes_is_the_tag_clean_wants() {
    assert_eq!(super::HOME_STORE, format!(".blaze/{}", crate::store::plain_place::STORE_DIR));
    assert_eq!(TAG.as_bytes(), blaze_cas::plain::CACHEDIR_SIGNATURE, "the test's copy of the tag");
    assert!(super::blazes_tag(TAG.as_bytes()));
    assert!(!super::blazes_tag(b"Signature: 8a477f597d28d172789f06886806bc55\n# made by another tool\n"));
    assert!(!super::blazes_tag(b""));
}

/// No flag: the checkout's `.blaze/v2` goes, every source stays, the home is not touched; a
/// second clean has nothing to remove.
#[test]
fn clean_removes_exactly_the_checkouts_state() {
    let io = usual();
    let (result, said) = clean(&io, &["ws"]);
    println!("blaze clean ws:\n{said}");
    assert!(result.is_ok(), "{result:?}");
    let freed = "profile\n".len() + "0123456789".len() + "abc".len();
    assert_eq!(said, format!("removed ws/.blaze/v2: 3 files, {freed} bytes freed\n"));
    assert!(!there(&io, "ws/.blaze/v2"));
    for kept in ["ws/Cargo.toml", "ws/src/lib.rs", "ws/.blaze", "h/.blaze/v2/store/CACHEDIR.TAG", "h/.blaze/v2/toolchain/abc/lib.rlib"] {
        assert!(there(&io, kept), "{kept} was removed");
    }
    let (again, said) = clean(&io, &["ws"]);
    println!("blaze clean ws, again:\n{said}");
    assert!(again.is_ok(), "{again:?}");
    assert_eq!(said, format!("{NOTHING}\n"));
}

/// `--store`: the store goes too; the toolchain shelf and the sources beside it stay.
#[test]
fn clean_store_removes_the_store_and_leaves_the_toolchain() {
    let io = usual();
    let (result, said) = clean(&io, &["ws", "--store"]);
    println!("blaze clean ws --store:\n{said}");
    assert!(result.is_ok(), "{result:?}");
    let checkout = "profile\n".len() + "0123456789".len() + "abc".len();
    let store = TAG.len() + "segment bytes".len() + "head".len();
    assert_eq!(said, format!("removed ws/.blaze/v2: 3 files, {checkout} bytes freed\nremoved /h/.blaze/v2/store: 3 files, {store} bytes freed\n"));
    assert!(!there(&io, "h/.blaze/v2/store"));
    for kept in ["h/.blaze/v2/toolchain/abc/lib.rlib", "h/.blaze/v2/sources/x.crate", "ws/src/lib.rs"] {
        assert!(there(&io, kept), "{kept} was removed");
    }
    let (again, said) = clean(&io, &["ws", "--store"]);
    println!("blaze clean ws --store, again:\n{said}");
    assert!(again.is_ok(), "{again:?}");
    assert_eq!(said, format!("{NOTHING}\n"));
}

/// A `.blaze/v2` holding a name blaze does not write is refused by that name, and nothing is
/// removed, not even the store asked for beside it.
#[test]
fn a_checkout_folder_not_blazes_is_refused_and_nothing_goes() {
    let io = usual();
    {
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let root = io.need_fs().expect("files").root();
        root.publish(&cx, &Path::new("ws/.blaze/v2/notes.md").expect("a path"), Content::bytes(b"mine"), Durable::No).expect("write");
    }
    let (result, said) = clean(&io, &["ws", "--store"]);
    let Err(CliError::Clean(refusal @ CleanError::Unknown { .. })) = &result else { panic!("not refused by name: {result:?}") };
    let words = refusal.to_string();
    println!("blaze clean ws --store, with ws/.blaze/v2/notes.md: {words}");
    assert!(words.contains("the checkout's .blaze/v2/notes.md is not a name blaze writes under .blaze/v2"), "{words}");
    assert!(words.contains("nothing was removed"), "{words}");
    assert_eq!(said, "");
    for kept in ["ws/.blaze/v2/notes.md", "ws/.blaze/v2/profile.txt", "h/.blaze/v2/store/CACHEDIR.TAG"] {
        assert!(there(&io, kept), "{kept} was removed");
    }
}

/// The home given as the checkout: its `.blaze/v2` holds the store and the shelf, never a
/// checkout's names, so it is refused and nothing goes.
#[test]
fn the_home_as_a_checkout_is_refused() {
    let io = usual();
    let (result, _) = clean(&io, &["h"]);
    let Err(CliError::Clean(refusal @ CleanError::Unknown { .. })) = &result else { panic!("not refused by name: {result:?}") };
    println!("blaze clean h: {refusal}");
    assert!(there(&io, "h/.blaze/v2/store/CACHEDIR.TAG"));
    assert!(there(&io, "h/.blaze/v2/toolchain/abc/lib.rlib"));
}

/// A store folder without blaze's tag (none, or another tool's) is not blaze's store: refused,
/// nothing removed, the checkout's state included.
#[test]
fn a_store_without_blazes_tag_is_refused_and_nothing_goes() {
    for (tag, why) in [(None, "holds no CACHEDIR.TAG"), (Some("Signature: 8a477f597d28d172789f06886806bc55\n# another tool\n"), "holds a CACHEDIR.TAG that blaze did not write")] {
        let mut files = vec![("ws/.blaze/v2/profile.txt", "profile\n"), ("h/.blaze/v2/store/seg-0000000000000001", "segment bytes")];
        if let Some(tag) = tag {
            files.push(("h/.blaze/v2/store/CACHEDIR.TAG", tag));
        }
        let io = world(&files);
        let (result, said) = clean(&io, &["ws", "--store"]);
        let Err(CliError::Clean(refusal @ CleanError::NotTheStore { .. })) = &result else { panic!("not refused by name: {result:?}") };
        let words = refusal.to_string();
        println!("blaze clean ws --store, a store that {why}: {words}");
        assert_eq!(words, format!("blaze clean --store: the home's .blaze/v2/store {why}, so it is not blaze's store; nothing was removed"));
        assert_eq!(said, "");
        assert!(there(&io, "h/.blaze/v2/store/seg-0000000000000001"));
        assert!(there(&io, "ws/.blaze/v2/profile.txt"));
    }
}

/// A checkout with no state at all, and no home store: nothing to remove, status 0.
#[test]
fn nothing_there_is_nothing_to_remove() {
    let io = world(&[("ws/src/lib.rs", "fn f() {}\n"), ("h/.profile", "")]);
    let (result, said) = clean(&io, &["ws", "--store"]);
    assert!(matches!(result, Ok(bstd::entry::Status::OK)), "{result:?}");
    assert_eq!(said, format!("{NOTHING}\n"));
}
