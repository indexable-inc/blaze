//! The user's home as a second tree (`Fs::home`): natively a driver over the home, wherever it
//! is relative to the start folder; in the simulation a folder beneath its one root. A home
//! that is not absolute, has `.` or `..` in it, or has a file where the folder should be, is a
//! typed `HomeError` naming the home.

use std::sync::Arc;

use super::{path, root, write_synced};
use crate::{
    fs::{FsKind, HomeWhy},
    io::{Io, Limits, NativeWorld, SimWorld},
};

/// A home outside the start folder, natively: a file written through the home's folder lands
/// at `<home>/.blaze/<name>` on the real disk, and nothing under the start folder.
#[test]
fn a_native_home_outside_the_start_folder_is_written() {
    let base = std::env::temp_dir().join(std::format!("bstd-home-{}", std::process::id()));
    let (start, home) = (base.join("proj"), base.join("home"));
    std::fs::create_dir_all(&start).expect("the start folder is made");
    std::fs::create_dir_all(&home).expect("the home is made");
    let io = Io::native(NativeWorld {
        root: &start,
        threads: 1,
        env: crate::env::Vars::given(Vec::new()),
        args: crate::args::ArgList::given(Vec::new()),
        host_name: Arc::from(""),
        seed: Some(7),
        above: 0,
        limits: Limits::USUAL,
    })
    .expect("a native world");
    let cx = root(&io);
    let fs = io.need_fs().expect("fs");
    let home_text = home.to_str().expect("a UTF-8 temporary path");
    let dir = fs.home(&cx, home_text, &path(".blaze")).expect("the home's folder");
    write_synced(&cx, &dir, &path("v2/shared/x"), b"made").expect("a file in the home's folder");
    assert_eq!(std::fs::read(home.join(".blaze/v2/shared/x")).expect("the file is on the disk"), b"made");
    assert!(!start.join(".blaze").exists(), "nothing was written under the start folder");

    // A file where the folder should be: refused, naming the home.
    std::fs::write(home.join("plain"), b"").expect("a plain file");
    let refused = fs.home(&cx, home_text, &path("plain")).expect_err("a file is not a folder");
    assert_eq!(refused.home, home_text);
    assert!(matches!(refused.why, HomeWhy::Fs(ref error) if error.kind == FsKind::NotADirectory), "{refused}");
    assert!(std::format!("{refused}").contains(home_text), "{refused}");

    let relative = fs.home(&cx, "home/x", &path(".blaze")).expect_err("a relative home");
    assert_eq!(relative.why, HomeWhy::NotAbsolute);
    let dotted = fs.home(&cx, "/tmp/../etc", &path(".blaze")).expect_err("a dotted home");
    assert_eq!(dotted.why, HomeWhy::Dotted);
    drop(dir);
    std::fs::remove_dir_all(&base).expect("the test's folder is removed");
}

/// In the simulation the home is a folder beneath its root, apart from the project's folder;
/// a file in the way is the same typed refusal.
#[test]
fn a_simulated_home_is_a_folder_beneath_the_root() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let cx = root(&io);
    let fs = io.need_fs().expect("fs");
    let dir = fs.home(&cx, "/home/sim", &path(".blaze")).expect("the home's folder");
    write_synced(&cx, &dir, &path("v2/x"), b"made").expect("a file in the home's folder");
    let seen = fs.root().stat(&cx, &path("home/sim/.blaze/v2/x")).expect("a stat");
    assert!(seen.is_some_and(|meta| meta.len == 4), "the file is at home/sim/.blaze/v2/x: {seen:?}");
    write_synced(&cx, &fs.root(), &path("home/other/.blaze"), b"in the way").expect("a file in the way");
    let refused = fs.home(&cx, "/home/other", &path(".blaze")).expect_err("a file is not a folder");
    assert!(matches!(refused.why, HomeWhy::Fs(ref error) if error.kind == FsKind::NotADirectory), "{refused}");
}
