//! The IO of [`super::retire`]: a reader's [`Pin`] on a history's directory, holding the
//! manifest it read, and the writer's look at the pins ([`pinned`]).

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use super::{
    HistoryError, io_error,
    manifest::{self, Manifest},
    retire::{self, PINS, PinFile, Pinned},
};

/// A reader's hold on a history's directory: while it lives, no file the manifest it read
/// lists is deleted. Held until the reader's last read of those files has ended (DuckDB has
/// exited).
pub struct Pin {
    /// Read by nothing: dropped with the pin, it lets the writer delete the manifest's files.
    _held: Held,
    manifest: Manifest,
}

/// A pin's files in [`PINS`]: its lock file, held locked, and the file saying its generation.
/// Dropped, they are deleted, the lock file last and while still locked, so the writer never
/// finds a live reader's lock file unlocked.
struct Held {
    lock: Option<fs::File>,
    lock_path: PathBuf,
    said: Option<PathBuf>,
}

impl Drop for Held {
    fn drop(&mut self) {
        // A file left behind is a stale pin: the writer deletes it once it finds it unlocked.
        if let Some(said) = self.said.take() {
            _ = fs::remove_file(said);
        }
        _ = fs::remove_file(&self.lock_path);
        drop(self.lock.take());
    }
}

impl Pin {
    /// Pins the history `dir` and reads its manifest ([`manifest::read`]): its lock file is
    /// made and locked under a hidden name and named into [`PINS`], then the manifest is read,
    /// then the generation it is of is said (the order [`super::retire`] proves safe).
    pub fn take(dir: &Path) -> Result<Pin, HistoryError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let pins = dir.join(PINS);
        fs::create_dir_all(&pins)
            .map_err(io_error(&pins, "making the directory of readers' pins"))?;
        // alloc: a pin's id, once a pin
        let id = format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        // Files of a dead process that had this pid and number say nothing of this pin.
        for entry in fs::read_dir(&pins).map_err(io_error(&pins, "listing"))? {
            let entry = entry.map_err(io_error(&pins, "listing"))?;
            let name = entry.file_name();
            if name.to_str().and_then(PinFile::parse).is_some_and(|it| it.id() == id) {
                let path = entry.path();
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(io_error(&path, "deleting a dead reader's pin file")(error));
                    }
                }
            }
        }
        let making = pins.join(PinFile::Making { id: &id }.file_name());
        let lock = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&making)
            .map_err(io_error(&making, "making a reader's pin"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(io_error(&making, "locking a reader's pin, which another holds")(
                    io::ErrorKind::WouldBlock.into(),
                ));
            }
            Err(fs::TryLockError::Error(error)) => {
                return Err(io_error(&making, "locking a reader's pin")(error));
            }
        }
        let lock_path = pins.join(PinFile::Lock { id: &id }.file_name());
        let mut held = Held { lock: Some(lock), lock_path, said: None };
        if let Err(error) = fs::rename(&making, &held.lock_path) {
            let error = io_error(&making, "naming a reader's pin into place")(error);
            held.lock_path = making;
            return Err(error);
        }
        let manifest = manifest::read(dir)?;
        let generation = manifest.retired().generation;
        let said = pins.join(PinFile::Said { id: &id, generation }.file_name());
        fs::write(&said, b"").map_err(io_error(&said, "saying the generation a reader read"))?;
        held.said = Some(said);
        Ok(Pin { _held: held, manifest })
    }

    /// The manifest the reader read: the files it reads, which stay on disk while the pin
    /// lives.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
}

/// What the live pins of the history `dir` hold, for the writer, which has committed every
/// retirement it reaps before it calls this. A lock file no one holds is a dead reader's: it is
/// deleted, with what that reader said, and so are a dead process's half-made pins.
pub(crate) fn pinned(dir: &Path) -> Result<Vec<Pinned>, HistoryError> {
    let pins = dir.join(PINS);
    let entries = match fs::read_dir(&pins) {
        Ok(it) => it,
        // alloc: nothing, no reader ever pinned
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error(&pins, "listing")(error)),
    };
    // alloc: the pins' names, once a reap
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_error(&pins, "listing"))?;
        if let Ok(name) = entry.file_name().into_string() {
            names.push(name);
        }
    }
    names.sort_unstable();
    let remove = |path: &Path| match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(path, "deleting a dead reader's pin file")(error)),
    };
    // Whether the lock file `path` is held; `None` when it is gone.
    let held = |path: &Path| -> Result<Option<bool>, HistoryError> {
        let file = match fs::File::open(path) {
            Ok(it) => it,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(path, "opening a reader's pin")(error)),
        };
        match file.try_lock() {
            Ok(()) => {
                // Deleted while this holds its lock, so no reader takes it meanwhile.
                remove(path)?;
                Ok(Some(false))
            }
            Err(fs::TryLockError::WouldBlock) => Ok(Some(true)),
            Err(fs::TryLockError::Error(error)) => {
                Err(io_error(path, "locking a reader's pin")(error))
            }
        }
    };
    // alloc: once a reap
    let mut live: Vec<&str> = Vec::new();
    // alloc: once a reap
    let mut said: Vec<(&str, u64)> = Vec::new();
    for name in &names {
        let path = pins.join(name);
        match PinFile::parse(name) {
            Some(PinFile::Lock { id }) => {
                if held(&path)? == Some(true) {
                    live.push(id);
                }
            }
            Some(PinFile::Said { id, generation }) => said.push((id, generation)),
            // Pins nothing until it is named; a dead process's is deleted.
            // (Never a live process's: it may be between making the file and locking it.)
            Some(it @ PinFile::Making { .. }) => {
                if it.pid().is_some_and(|pid| !super::alive(pid)) {
                    held(&path)?;
                }
            }
            None => {}
        }
    }
    // What a reader whose lock file is not held said: a dead reader's (its lock file was just
    // deleted), or one that ended between the listing and the look (it deletes its own first).
    // One whose lock file the listing missed while it was named into place then pins every
    // file until it ends: never fewer.
    for &(id, generation) in &said {
        if !live.contains(&id) {
            remove(&pins.join(PinFile::Said { id, generation }.file_name()))?;
        }
    }
    Ok(retire::pinned(&live, &said))
}

/// The highest generation any reader in [`PINS`] of the history `dir` said, live or not (0:
/// none): where an adopted history's generation starts, so no reader still running took it to
/// cover files retired after adoption.
pub(crate) fn said_most(dir: &Path) -> Result<u64, HistoryError> {
    let pins = dir.join(PINS);
    let entries = match fs::read_dir(&pins) {
        Ok(it) => it,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(io_error(&pins, "listing")(error)),
    };
    let mut most = 0;
    for entry in entries {
        let entry = entry.map_err(io_error(&pins, "listing"))?;
        if let Some(PinFile::Said { generation, .. }) =
            entry.file_name().to_str().and_then(PinFile::parse)
        {
            most = most.max(generation);
        }
    }
    Ok(most)
}
