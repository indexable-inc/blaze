//! The real [`Files`]: `lstat`, `read` and a variable's value, under a root. The thin IO driver
//! of [`crate::outside`]; no decision is made here.

use std::{
    alloc::Global,
    ffi::OsStr,
    io::{self, Read},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

use blaze_core::Stamp;

use crate::outside::{Files, FilesError};

/// The files under `root`, and the process's variables.
pub struct Disk {
    root: PathBuf,
}

impl Disk {
    pub fn new(root: impl Into<PathBuf>) -> Disk {
        Disk { root: root.into() }
    }

    fn path(&self, path: &[u8]) -> PathBuf {
        self.root.join(Path::new(OsStr::from_bytes(path)))
    }
}

fn errno(call: &'static str, error: &io::Error) -> FilesError {
    FilesError { call, errno: error.raw_os_error().unwrap_or(0) }
}

impl Files for Disk {
    fn stamp(&self, path: &[u8]) -> Result<Stamp, FilesError> {
        match std::fs::symlink_metadata(self.path(path)) {
            Ok(meta) => Ok(Stamp {
                ino: meta.ino(),
                size: meta.size(),
                mtime_ns: (meta.mtime() as u64).saturating_mul(1_000_000_000).saturating_add(meta.mtime_nsec() as u64),
                ctime_ns: (meta.ctime() as u64).saturating_mul(1_000_000_000).saturating_add(meta.ctime_nsec() as u64),
                mode: meta.mode(),
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Stamp::GONE),
            Err(error) => Err(errno("lstat", &error)),
        }
    }

    fn read(&self, path: &[u8], out: &mut Vec<u8, Global>) -> Result<(), FilesError> {
        out.clear();
        let mut file = std::fs::File::open(self.path(path)).map_err(|error| errno("open", &error))?;
        // Into the answerer's one buffer, kept across asks: no allocation once it has grown.
        file.read_to_end(out).map_err(|error| errno("read", &error))?;
        Ok(())
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "an outside read of a request (`env!`): the variable's value now is the answer the certificate is checked against, not configuration"
    )]
    fn var(&self, name: &[u8], out: &mut Vec<u8, Global>) -> Result<bool, FilesError> {
        out.clear();
        match std::env::var_os(OsStr::from_bytes(name)) {
            Some(value) => {
                out.extend_from_slice(value.as_bytes());
                Ok(true)
            }
            None => Ok(false),
        }
    }
}
