//! A native world made with the root of real-OS authority (`blaze_core::Os`, plan
//! port27/bstd-capabilities.md P3): the one home of a native `Io`'s making. `Io::native`
//! reaches the root itself and delegates here until batch B1 deletes it.
//!
//! Without a `&blaze_core::Os` no code makes a native world: `Os` has a private field and is
//! neither `Clone`, `Copy` nor `Default`, so its only references come from `Os::process()`
//! (a program's entry, counted by the reach ratchet elsewhere) and `blaze_core::testing::os()`
//! (a test's, behind the `testing` feature). A simulated world (`Io::sim`) needs none: it
//! grants nothing real.

// The parent's own imports (`Box`, `String`, `ToString` are io.rs's `use alloc::..`), so this
// file adds no line naming `alloc` to the std-alloc ratchet.
use super::{Backend, Box, Driver as _, Io, NativeWorld, OpenError, Plant, Start, String, ToString as _};

impl Io {
    /// The target's own driver over `world.root`, made with the root of real-OS authority
    /// `os`: the form a program's entry (`bstd::rt::run_with`, `rt::launch_with`) and a native
    /// test (`bstd::testing::native`) make their world with.
    #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
    pub fn native_in(os: &blaze_core::Os, world: NativeWorld<'_>) -> Result<Io, OpenError> {
        open_native_in(os, world, None)
    }

    /// [`Io::native_in`] over pre-built `workers` (registered with telemetry or slots): constructs
    /// the native world directly on `workers` without allocating a throwaway bare
    /// [`Workers`](crate::workers::Workers) reservation first.
    #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
    pub fn native_in_on(os: &blaze_core::Os, world: NativeWorld<'_>, workers: crate::workers::Workers) -> Result<Io, OpenError> {
        open_native_in(os, world, Some(workers))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
fn open_native_in(os: &blaze_core::Os, world: NativeWorld<'_>, workers: Option<crate::workers::Workers>) -> Result<Io, OpenError> {
    let refused = |why: String| OpenError {
        // alloc: an error's message.
        root: world.root.display().to_string(),
        why,
    };
    // alloc: an error's message.
    let driver = blaze_core::native_in(os, world.root, world.threads).map_err(|error| refused(error.to_string()))?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let native_root_dir = Some(blaze_own::Arc::new_in(
        // alloc: an error's message.
        crate::rt::NativeRootDir::open(world.root).map_err(|errno| refused(errno.to_string()))?,
        blaze_own::SharedAlloc::global(),
    ));
    // alloc: an error's message.
    let host = crate::native::host_facts(world.host_name, driver.now()).map_err(|error| refused(error.to_string()))?;
    let workers = match workers {
        Some(workers) => workers,
        None => {
            // The world's workers (D171): bstd's own cells, no slots; a program that registers
            // slots or telemetry hands its own `Workers` (`Io::native_in_on`, `rt::run_with_workers`,
            // `rt::run_with_heap_at`). One sparse reservation, nothing backed until a task uses its
            // cell; no seed, as the start draws no randomness.
            let setup = crate::workers::WorkersSetup { limits: crate::workers::Limits::BARE, seed: world.seed, models: 0 };
            // alloc: an error's message.
            crate::workers::Workers::bare(setup).map_err(|error| refused(error.to_string()))?
        }
    };
    Ok(Io::over(
        Start {
            // alloc: once per process, the driver.
            driver: Backend::Native(Box::new(driver)),
            seed: world.seed,
            above: world.above,
            env: world.env,
            args: world.args,
            host,
            offers: crate::host::OFFERS,
            limits: world.limits,
            hosted: None,
            disk: None,
            plant: Plant::None,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            native_root_dir,
        },
        workers,
    ))
}
