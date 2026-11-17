//! A native world is made only from a root of real-OS authority (plan
//! port27/bstd-capabilities.md P3): `bstd::test::native` makes one over a test's directory with
//! `blaze_core::testing::os()`, through `Io::native_in`, the one home of a native world's making.
//!
//! The half of the claim no run can show (that no code makes a native world WITHOUT a root) is
//! the type's: `blaze_core::Os` has a private field and is neither `Clone`, `Copy` nor
//! `Default`, so outside blaze-core's os.rs a `&Os` comes only from `Os::process()` (counted by
//! the reach ratchet outside an entry's `fn main`) or `blaze_core::testing::os()` (the
//! `testing` feature); `Io::native_in` and `blaze_core::native_in` take one. Once batch B1
//! deletes `Io::native` and `blaze_core::native`, these are the only ways to a native driver.

use super::{path, root, write_synced};
use crate::{
    io::Io,
    path::{Path, PathWhy},
};

/// A world from `bstd::test::native` is native and over `dir` alone: a file written through
/// its root lands at `<dir>/a` on the real disk; it may read no folder above `dir`. The control:
/// the same write in a simulated world (`Io::sim`) leaves nothing on the disk, so the disk
/// check tells a native world from one that only looks like it (a `test::native` that made a
/// simulated world would fail it). And a rooted path, `/etc`, is refused by `Path::new` with
/// `PathWhy::Rooted`: the world's root is the only way out of `dir`, and it has none above.
/// Expected: `test tests::native_root::a_native_world_is_made_only_from_a_root ... ok`.
#[test]
fn a_native_world_is_made_only_from_a_root() {
    let dir = std::env::temp_dir().join(std::format!("bstd-native-root-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the test's folder is made");

    let io = crate::test::native(&dir).expect("a native world over the test's folder, from the test's root");
    assert_eq!(io.above_most(), 0, "a test's native world reads no folder above its directory");
    let cx = root(&io);
    let fs = io.need_fs().expect("fs");
    write_synced(&cx, &fs.root(), &path("a"), b"made").expect("a file through the native root");
    assert_eq!(std::fs::read(dir.join("a")).expect("the file is on the real disk"), b"made");

    // The control: a simulated world's write reaches no disk.
    std::fs::remove_file(dir.join("a")).expect("the file is removed");
    let sim = Io::sim(0);
    let sim_cx = root(&sim);
    write_synced(&sim_cx, &sim.need_fs().expect("fs").root(), &path("a"), b"made").expect("a file in the simulation");
    assert!(!dir.join("a").exists(), "a simulated world wrote to the real disk");

    let rooted = Path::new("/etc").expect_err("a rooted path");
    assert_eq!(rooted.why, PathWhy::Rooted, "{rooted}");

    drop((cx, io, sim_cx, sim));
    std::fs::remove_dir_all(&dir).expect("the test's folder is removed");
}
