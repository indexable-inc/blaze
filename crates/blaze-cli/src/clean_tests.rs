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
    let fs = io.need_fs().expect("fs");
    let result = super::run_with(&cx, &fs, words.iter().copied(), Some("/h"), &io.need_out().expect("out"));
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
        (OWN_SEG, "segment bytes"),
        (OWN_HEAD, "head"),
        ("h/.blaze/v2/toolchain/abc/lib.rlib", "toolchain"),
        ("h/.blaze/v2/sources/x.crate", "source"),
    ])
}

/// This blaze's store files in the fixtures: under plain_place.rs `STORE_DIR` (a test below
/// holds them there).
const OWN_SEG: &str = "h/.blaze/v2/store/f5s3/seg-0000000000000001";
const OWN_HEAD: &str = "h/.blaze/v2/store/f5s3/head";

#[test]
fn the_fixtures_name_this_blazes_store() {
    assert!(OWN_HEAD.starts_with(&format!("h/.blaze/{}/", crate::store::plain_place::STORE_DIR)), "update OWN_SEG and OWN_HEAD to {}", crate::store::plain_place::STORE_DIR);
    assert!(OWN_SEG.starts_with(&format!("h/.blaze/{}/", crate::store::plain_place::STORE_DIR)));
}

/// No flag, a home holding the legacy layout and an older blaze's folder beside this one's:
/// both other forms go (the legacy layout's own files alone), this blaze's folder, the cache
/// tag, a name of no store's shape and the checkout's sources stay.
#[test]
fn clean_removes_the_other_forms_and_keeps_this_blazes_store() {
    let io = world(&[
        ("ws/src/lib.rs", "fn f() {}\n"),
        ("h/.blaze/v2/store/CACHEDIR.TAG", TAG),
        (OWN_HEAD, "head"),
        ("h/.blaze/v2/store/head", "old head"),
        ("h/.blaze/v2/store/seg-0000000000000009", "old segment"),
        ("h/.blaze/v2/store/f4s2/head", "older"),
        ("h/.blaze/v2/store/notes", "mine"),
    ]);
    let (result, said) = clean(&io, &["ws"]);
    println!("blaze clean ws, with other forms:\n{said}");
    assert!(result.is_ok(), "{result:?}");
    let legacy = "old head".len() + "old segment".len();
    assert_eq!(
        said,
        format!(
            "removed the format 4 store's files directly in /h/.blaze/v2/store: 2 files, {legacy} bytes freed\nremoved /h/.blaze/v2/store/f4s2, a store of another blaze's forms: 1 file, 5 bytes freed\n"
        )
    );
    for gone in ["h/.blaze/v2/store/head", "h/.blaze/v2/store/seg-0000000000000009", "h/.blaze/v2/store/f4s2"] {
        assert!(!there(&io, gone), "{gone} was kept");
    }
    for kept in [OWN_HEAD, "h/.blaze/v2/store/CACHEDIR.TAG", "h/.blaze/v2/store/notes", "ws/src/lib.rs"] {
        assert!(there(&io, kept), "{kept} was removed");
    }
    let (again, said) = clean(&io, &["ws"]);
    assert!(again.is_ok(), "{again:?}");
    assert_eq!(said, format!("{NOTHING}\n"));
}

#[test]
fn the_tag_blaze_writes_is_the_tag_clean_wants() {
    assert_eq!(super::HOME_STORE, format!(".blaze/{}", crate::store::plain_place::STORE_ROOT));
    assert!(crate::store::plain_place::STORE_DIR.starts_with(&format!("{}/", crate::store::plain_place::STORE_ROOT)), "this blaze's store is not a folder of the root");
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

/// Every entry of the one list is a name under `CHECKOUT_STATE`, its path that name there, and
/// the writers name theirs from it.
#[test]
fn the_checkout_list_is_the_writers_list() {
    use super::{CHECKOUT_ENTRIES, CHECKOUT_STATE, CheckoutEntry};
    for (entry, name) in CheckoutEntry::ALL.iter().zip(CHECKOUT_ENTRIES) {
        assert_eq!(entry.name(), name);
        assert_eq!(entry.path(), format!("{CHECKOUT_STATE}/{name}"), "{entry:?}'s path is not its name under {CHECKOUT_STATE}");
    }
    for written in [crate::print::PROFILE, crate::engine_start::START_PROFILE, crate::store::OLD_CHECKOUT[0], crate::store::OLD_CHECKOUT[1]] {
        let name = written.strip_prefix(&format!("{CHECKOUT_STATE}/")).unwrap_or_else(|| panic!("{written} is not under {CHECKOUT_STATE}"));
        assert!(CHECKOUT_ENTRIES.contains(&name), "{written} is written but `clean` refuses it");
    }
}

/// `run`'s writer (engine_start.rs `write_start_profile`, which every `run` and `test` calls after
/// its start, well or not) and the check's profile, then `clean`: both are blaze's, removed, and
/// the clean succeeds. Before the one list, `start.txt` was refused by name after any `run`.
#[test]
fn clean_after_run_removes_the_starts_profile() {
    let io = world(&[("ws/Cargo.toml", "[package]\n"), ("ws/src/main.rs", "fn main() {}\n")]);
    {
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let fs = io.need_fs().expect("files");
        let ws = fs.open_dir(&cx, &Path::new("ws").expect("a path")).expect("the checkout");
        ws.make_dir(&cx, &Path::new(".blaze").expect("a path"), Durable::No).expect("a folder");
        ws.make_dir(&cx, &Path::new(super::CHECKOUT_STATE).expect("a path"), Durable::No).expect("a folder");
        ws.publish(&cx, &Path::new(crate::print::PROFILE).expect("a path"), Content::bytes(b"check profile\n"), Durable::No).expect("the check's profile");
        let phases = crate::phases::Phases::new(io.need_clock().expect("a clock"));
        crate::engine_start::write_start_profile(&cx, &ws, &phases, cx.frame()).expect("the start's profile");
    }
    assert!(there(&io, "ws/.blaze/v2/start.txt"), "run's writer wrote no start.txt");
    let (result, said) = clean(&io, &["ws"]);
    println!("blaze clean ws, after a run:\n{said}");
    assert!(result.is_ok(), "clean refused after a run: {result:?}");
    assert!(said.starts_with("removed ws/.blaze/v2: 2 files, "), "{said}");
    assert!(!there(&io, "ws/.blaze/v2"));
    assert!(there(&io, "ws/src/main.rs"));
}

/// A home with no store root at all (a new user, one who never ran blaze), and a home that
/// does not exist (HOME names a folder never made): `clean` without and with `--store`
/// succeeds, nothing of the home to remove.
#[test]
fn a_home_with_no_store_is_nothing_to_remove() {
    for words in [&["ws"][..], &["ws", "--store"][..]] {
        let io = world(&[("ws/src/lib.rs", "fn f() {}\n"), ("h/.blaze/v2/toolchain/abc/lib.rlib", "toolchain")]);
        let (result, said) = clean(&io, words);
        assert!(matches!(result, Ok(bstd::entry::Status::OK)), "{words:?}: {result:?}");
        assert_eq!(said, format!("{NOTHING}\n"), "{words:?}");
        assert!(there(&io, "h/.blaze/v2/toolchain/abc/lib.rlib"));
        let io = world(&[("ws/src/lib.rs", "fn f() {}\n")]);
        assert!(!there(&io, "h"), "the fixture has a home");
        let (result, said) = clean(&io, words);
        assert!(matches!(result, Ok(bstd::entry::Status::OK)), "{words:?}, no home folder: {result:?}");
        assert_eq!(said, format!("{NOTHING}\n"), "{words:?}, no home folder");
    }
}

/// Without `--store`, a home's store root with no blaze tag is not looked into, said so; the
/// checkout is still cleaned, and the root's files stay. (`--store` refuses it: the test above.)
#[test]
fn an_untagged_store_root_is_said_and_left_without_store() {
    let io = world(&[("ws/.blaze/v2/profile.txt", "profile\n"), ("h/.blaze/v2/store/seg-0000000000000001", "segment bytes")]);
    let (result, said) = clean(&io, &["ws"]);
    println!("blaze clean ws, an untagged home store:\n{said}");
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        said,
        "removed ws/.blaze/v2: 1 file, 8 bytes freed\nthe home's .blaze/v2/store holds no CACHEDIR.TAG, so it is not shown to be blaze's store: no store in it was looked at or removed\n"
    );
    assert!(there(&io, "h/.blaze/v2/store/seg-0000000000000001"));
    assert!(!there(&io, "ws/.blaze/v2"));
}

/// A checkout with no state at all, and no home store: nothing to remove, status 0.
#[test]
fn nothing_there_is_nothing_to_remove() {
    let io = world(&[("ws/src/lib.rs", "fn f() {}\n"), ("h/.profile", "")]);
    let (result, said) = clean(&io, &["ws", "--store"]);
    assert!(matches!(result, Ok(bstd::entry::Status::OK)), "{result:?}");
    assert_eq!(said, format!("{NOTHING}\n"));
}

/// `--all`: removes the checkout state, the home store, AND all derived caches listed in
/// `layout.rs` (`Scope::All`: `run/records`, `proc-macro/expansions`, `sdk-answers`, `native-work`,
/// `test/tmp`) while keeping `toolchain`, `sources`, and `roster`.
#[test]
fn clean_all_removes_store_and_derived_caches_and_keeps_toolchain_and_sources() {
    let io = world(&[
        ("ws/src/lib.rs", "fn f() {}\n"),
        ("ws/.blaze/v2/profile.txt", "p"),
        ("h/.blaze/v2/store/CACHEDIR.TAG", TAG),
        (OWN_HEAD, "head"),
        ("h/.blaze/v2/run/records/abc", "jit"),
        ("h/.blaze/v2/proc-macro/expansions/e1", "exp"),
        ("h/.blaze/v2/sdk-answers/s1", "sdk"),
        ("h/.blaze/v2/roster/r1", "ros"),
        ("h/.blaze/v2/native-work/n1", "nat"),
        ("h/.blaze/v2/test/tmp/t1", "tmp"),
        ("h/.blaze/v2/toolchain/abc/lib.rlib", "toolchain"),
        ("h/.blaze/v2/sources/x.crate", "source"),
    ]);
    let (result, said) = clean(&io, &["ws", "--all"]);
    assert!(result.is_ok(), "{result:?}");
    assert!(said.contains("removed /h/.blaze/v2/store:"), "{said}");
    assert!(said.contains("removed /h/.blaze/v2/run/records:"), "{said}");
    assert!(said.contains("removed /h/.blaze/v2/proc-macro/expansions:"), "{said}");
    for gone in [
        "ws/.blaze/v2",
        "h/.blaze/v2/store",
        "h/.blaze/v2/run/records",
        "h/.blaze/v2/proc-macro/expansions",
        "h/.blaze/v2/sdk-answers",
        "h/.blaze/v2/native-work",
        "h/.blaze/v2/test/tmp",
    ] {
        assert!(!there(&io, gone), "{gone} was not removed by --all");
    }
    for kept in [
        "h/.blaze/v2/toolchain/abc/lib.rlib",
        "h/.blaze/v2/sources/x.crate",
        "h/.blaze/v2/roster/r1",
        "ws/src/lib.rs",
    ] {
        assert!(there(&io, kept), "{kept} was removed by --all");
    }
}

/// A checkout a file watch ran in holds its cookies (blaze-core fsevents.rs `COOKIE_AREA`):
/// `clean` removes them with the rest. Before `fsevents` was a `CheckoutEntry`, this clean was
/// refused by that name and nothing went (remove the variant and this test fails so).
#[test]
fn clean_after_a_watch_removes_its_cookies() {
    let io = world(&[("ws/src/lib.rs", "fn f() {}\n"), ("ws/.blaze/v2/profile.txt", "p"), ("ws/.blaze/v2/fsevents/4242-1", "")]);
    assert_eq!(super::CheckoutEntry::Cookies.path(), ".blaze/v2/fsevents", "blaze-core fsevents.rs `COOKIE_AREA` (private there) moved");
    let (result, said) = clean(&io, &["ws"]);
    println!("blaze clean ws, after a watch:\n{said}");
    assert!(result.is_ok(), "a watched checkout's clean was refused: {result:?}");
    assert_eq!(said, "removed ws/.blaze/v2: 2 files, 1 bytes freed\n");
    assert!(!there(&io, "ws/.blaze/v2"));
    assert!(there(&io, "ws/src/lib.rs"));
}

/// `--all` on a home holding every row's folder AND folders no row names: beside a row (`mine`,
/// `run/other` and `test/keep` in a row's parent, `sdk-answers-old` a row's name with more)
/// and a file in `.blaze`. Each `All` row's folder goes, said with what it held; every planted
/// folder, the kept rows (each said with why) and the sources stay. Removal is by each row's
/// exact path, so nothing a listing shows is matched.
#[test]
fn clean_all_removes_only_the_tables_folders() {
    use crate::layout::{LAYOUT, Scope, kept_at_home, of};
    let planted = ["h/.blaze/v2/mine/notes", "h/.blaze/v2/run/other/x", "h/.blaze/v2/test/keep/x", "h/.blaze/v2/sdk-answers-old/x", "h/.blaze/notes.txt"];
    let mut files: std::vec::Vec<(std::string::String, &str)> = std::vec![("ws/src/lib.rs".into(), "fn f() {}\n"), ("h/.blaze/v2/store/CACHEDIR.TAG".into(), TAG), (OWN_HEAD.into(), "head")];
    for row in LAYOUT.iter().filter(|row| row.place == super::Place::Home && row.scope != Scope::Store) {
        files.push((format!("h/{}/f", row.path), "x"));
    }
    files.extend(planted.iter().map(|path| (std::string::String::from(*path), "mine")));
    let io = world(&files.iter().map(|(path, text)| (path.as_str(), *text)).collect::<std::vec::Vec<_>>());
    let (result, said) = clean(&io, &["ws", "--all"]);
    println!("blaze clean ws --all, with planted folders:\n{said}");
    assert!(result.is_ok(), "{result:?}");
    for row in of(Scope::All) {
        assert!(!there(&io, &format!("h/{}", row.path)), "{} was not removed by --all", row.path);
        assert!(said.contains(&format!("removed /h/{}: 1 file, 1 bytes freed ({})\n", row.path, row.holds)), "{} not said with what it held:\n{said}", row.path);
    }
    for (row, why) in kept_at_home() {
        assert!(there(&io, &format!("h/{}/f", row.path)), "{} (kept: {why}) was removed", row.path);
        assert!(said.contains(&format!("kept: {}: {why}\n", row.short())), "{} kept without saying why:\n{said}", row.path);
    }
    assert!(said.contains("kept: proc-macro/modules: made by the bootstrap builder, no command rebuilds it\n"), "{said}");
    for path in planted {
        assert!(there(&io, path), "{path}, which no row names, was removed");
    }
    assert!(!there(&io, "h/.blaze/v2/store"));
}

/// A row's folder that is a link (here `test/tmp`, pointing out of the home): `--all` refuses it
/// by name before removing anything, so the checkout's state, the store and every other row's
/// folder stay, and the link's target is never reached.
#[test]
fn clean_all_refuses_a_linked_folder_and_removes_nothing() {
    let io = world(&[
        ("ws/.blaze/v2/profile.txt", "p"),
        ("h/.blaze/v2/store/CACHEDIR.TAG", TAG),
        (OWN_HEAD, "head"),
        ("h/.blaze/v2/sdk-answers/s1", "sdk"),
        ("h/.blaze/v2/run/records/abc", "jit"),
        ("elsewhere/precious", "mine"),
    ]);
    let link = blaze_core::FileName::new(&format!("h/{}", crate::test_drive::TEMPS)).expect("a name");
    io.with_sim(|sim| sim.symlink(&link, "/elsewhere")).expect("a simulated world");
    let (result, said) = clean(&io, &["ws", "--all"]);
    let Err(CliError::Clean(refusal @ CleanError::Link { .. })) = &result else { panic!("a linked folder was not refused by name: {result:?}\n{said}") };
    let words = refusal.to_string();
    println!("blaze clean ws --all, test/tmp a link: {words}");
    assert!(words.contains(&format!("the home's {} is a link", crate::test_drive::TEMPS)), "{words}");
    assert_eq!(said, "", "something was said removed before the refusal");
    for kept in ["ws/.blaze/v2/profile.txt", "h/.blaze/v2/store/CACHEDIR.TAG", OWN_HEAD, "h/.blaze/v2/sdk-answers/s1", "h/.blaze/v2/run/records/abc", "elsewhere/precious"] {
        assert!(there(&io, kept), "{kept} was removed although the clean was refused");
    }
}
