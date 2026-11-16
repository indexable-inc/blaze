//! Records by key (drive.md, "The system side", "Store"). The trait is the seam the plain store
//! (blaze-cas `plain::Driven`, store-next.md) plugs into once it has a bstd driver: its
//! `open(root, settings)` then `machine(&[checkout, home])`, `get_into` by `locate` and one
//! `pread`, and after the answer `seal()` then `compact(n)`, its `report()` into the profile.
//! [`FileStore`] is the stand-in: one file per record under `.blaze/v2/records/`, named by the
//! key's 64 hex digits, written whole and never rewritten (a key holds one value, rule 21).

use blaze_drive::Key;
use blaze_drive::profile::Profile;
use blaze_own::Vec;
use bstd::cx::Cx;
use bstd::fs::{Content, Dir, Durable, ReadWrite};
use bstd::mem::Bytes;
use bstd::path::Path;

use crate::CliError;

pub trait Store {
    /// The record at `key` into `into`; `false` when there is none.
    fn get_into(&mut self, cx: &Cx, key: Key, into: &mut Bytes) -> Result<bool, CliError>;
    /// `bytes` at `key`. A key already held is left as it is: two builders of one key write
    /// equal bytes (rule 25).
    fn put(&mut self, cx: &Cx, key: Key, bytes: &[u8]) -> Result<(), CliError>;
    /// After the answer is printed: the plain store seals and compacts here and reports into
    /// `profile`. Never waited on by the answer.
    fn after_answer(&mut self, cx: &Cx, profile: &mut Profile) -> Result<(), CliError>;
}

const RECORDS: &str = ".blaze/v2/records";

pub struct FileStore<'d> {
    records: Dir<ReadWrite>,
    _root: &'d Dir<ReadWrite>,
}

impl<'d> FileStore<'d> {
    pub fn open(cx: &Cx, root: &'d Dir<ReadWrite>) -> Result<FileStore<'d>, CliError> {
        let records = root.subdir(cx, &Path::new(RECORDS)?)?;
        Ok(FileStore { records, _root: root })
    }

    fn path(key: Key) -> Result<Path, CliError> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut name = [0u8; 64];
        for (at, byte) in key.0.0.iter().enumerate() {
            name[at * 2] = HEX[usize::from(byte >> 4)];
            name[at * 2 + 1] = HEX[usize::from(byte & 15)];
        }
        let name = core::str::from_utf8(&name).map_err(|_| CliError::Text { what: "a key's hex" })?;
        Ok(Path::new(name)?)
    }
}

impl Store for FileStore<'_> {
    fn get_into(&mut self, cx: &Cx, key: Key, into: &mut Bytes) -> Result<bool, CliError> {
        let path = FileStore::path(key)?;
        if self.records.stat(cx, &path)?.is_none() {
            return Ok(false);
        }
        self.records.read_all(cx, &path, into)?;
        Ok(true)
    }

    fn put(&mut self, cx: &Cx, key: Key, bytes: &[u8]) -> Result<(), CliError> {
        let path = FileStore::path(key)?;
        if self.records.stat(cx, &path)?.is_some() {
            return Ok(());
        }
        self.records.publish(cx, &path, Content::bytes(bytes), Durable::No)?;
        Ok(())
    }

    fn after_answer(&mut self, _cx: &Cx, _profile: &mut Profile) -> Result<(), CliError> {
        // Nothing to seal: each record is its own file, published whole.
        let _: Option<Vec<u8, &bstd::mem::Frame>> = None;
        Ok(())
    }
}
