//! A test's native world (`bstd::test::native`, plan port27/bstd-capabilities.md P3): made with
//! a test's root of real-OS authority, `blaze_core::testing::os()`, so a native test needs
//! neither `Io::native` (which reaches the process's root itself and goes in batch B1) nor
//! `Os::process()` (the program's entry's). Behind the `testing` feature (and bstd's own tests).

use crate::io::{Io, Limits, NativeWorld, OpenError};

use super::RootPath;

/// A native world over `dir`: one file thread, an empty environment and command line, the
/// host's name empty, the seed of `Io::rng` fixed at 0 (a test says its seed), no folder above
/// `dir`, and [`Limits::USUAL`]. `dir` must exist; the driver's refusal is the error, naming it.
pub fn native(dir: &RootPath) -> Result<Io, OpenError> {
    Io::native_in(
        blaze_core::testing::os(),
        NativeWorld {
            root: dir,
            threads: 1,
            // alloc: none: an empty environment and command line.
            env: crate::env::Vars::given(Default::default()),
            args: crate::args::ArgList::given(Default::default()),
            // alloc: once per test world, the host's name (empty).
            host_name: "".into(),
            seed: Some(0),
            above: 0,
            limits: Limits::USUAL,
        },
    )
}
