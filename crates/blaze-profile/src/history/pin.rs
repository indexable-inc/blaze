//! A reader's [`Pin`] on a history's directory, holding the manifest it read: the driver of the
//! core's pin job ([`super::files`]), which makes and locks the pin's files in [`super::PINS`],
//! reads the manifest, and says the generation it is of, in the order [`super::retire`] proves
//! safe. The writer's look at the pins is the core's too (its reap).

use std::{
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use super::{
    HistoryError,
    drive::{Core, Live, mismatch},
    files::{Files, Outcome, Pinned},
    io_error,
    manifest::Manifest,
};

/// A reader's hold on a history's directory: while it lives, no file the manifest it read
/// lists is deleted. Held until the reader's last read of those files has ended (DuckDB has
/// exited). Its core's driver holds the pin's lock: it lives as long as the pin.
pub struct Pin {
    core: Core<Live>,
    /// Let go of as the pin drops; `None` only while it is.
    pinned: Option<Pinned>,
}

impl Pin {
    /// Pins the history `dir` and reads its manifest: its lock file is made and locked under a
    /// hidden name and named into [`super::PINS`], then the manifest is read, then the
    /// generation it is of is said.
    pub fn take(dir: &Path) -> Result<Pin, HistoryError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::fs::create_dir_all(dir).map_err(io_error(dir, "making the directory"))?;
        // alloc: a pin's id, once a pin
        let id = format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let mut core = Core::new(Live::new(dir), dir)?;
        match core.job(|it| it.pin(id))? {
            Outcome::Pinned(pinned) => Ok(Pin { core, pinned: Some(pinned?) }),
            other => Err(mismatch(dir, "pinning", &other)),
        }
    }

    /// The manifest the reader read: the files it reads, which stay on disk while the pin
    /// lives.
    pub fn manifest(&self) -> &Manifest {
        match &self.pinned {
            Some(pinned) => &pinned.manifest,
            None => unreachable!("a pin holds what it pinned until it drops"),
        }
    }
}

impl Drop for Pin {
    /// Lets go of the pin: its files are deleted, the lock file last and while still locked.
    /// One left behind is a stale pin, which the writer's reap deletes once it finds it
    /// unlocked.
    fn drop(&mut self) {
        let Some(pinned) = self.pinned.take() else { return };
        // clone: the pin's name, for the warning if it can't be let go of
        let named = pinned.lock_file.clone();
        match self.core.job(|it: &mut Files<Live>| it.unpin(pinned)) {
            Ok(Outcome::Unpinned) => {}
            Ok(other) => {
                let error = mismatch(self.core.dir(), "letting go of a reader's pin of", &other);
                tracing::warn!(target: "blaze", "letting go of the reader's pin {named}: {error}");
            }
            Err(error) => {
                tracing::warn!(target: "blaze", "letting go of the reader's pin {named}: {error}");
            }
        }
    }
}
