//! A folder of build-script stored files as readers read them ([`StoredFiles`]): the one
//! `blaze_build_script::outputs::StoredBytes` over files on disk, for the command (the home's
//! `<home>/.blaze/v2/build-script/files`) and for the examples that read exported records
//! (`<dir>/files` beside `tempo_scripts export`'s records).

use std::alloc::Global;
use std::path::PathBuf;

use blaze_build_script::outputs::{StoredBytes, StoredError, StoredRef, StoredWhy};

/// Stored files, each mapped read-only from `<folder>/<sha256 hex>` the first time a reader
/// asks it, and never before. Opening a record maps nothing, so `check` of a crate that
/// includes a native script's `bindings.rs` maps that one file and never the 810 MB archive
/// beside it; a link maps the archive. A mapping reads no page until its bytes are read.
pub struct StoredFiles {
    /// `None`: there is no area (the command has no home, or an exported folder has no
    /// `files`): each ask is refused [`StoredWhy::NoArea`].
    folder: Option<PathBuf>,
    /// One slot per stored file the readers may ask, by digest, sorted and each once.
    #[expect(clippy::disallowed_types, reason = "a slot of one file, mapped on its first ask and read by every reader after: per value, not global state")]
    files: std::vec::Vec<([u8; 32], std::sync::OnceLock<Result<blaze_core::Mapping, StoredWhy>>), Global>,
}

impl StoredFiles {
    /// The files of these digests (any order, repeats allowed) in `folder`: nothing mapped yet.
    /// A digest not given here is answered [`StoredWhy::NotThere`].
    pub fn new(folder: Option<PathBuf>, digests: impl IntoIterator<Item = [u8; 32]>) -> StoredFiles {
        #[expect(clippy::disallowed_types, reason = "see the field")]
        let mut files: std::vec::Vec<([u8; 32], std::sync::OnceLock<Result<blaze_core::Mapping, StoredWhy>>), Global> =
            digests.into_iter().map(|digest| (digest, std::sync::OnceLock::new())).collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files.dedup_by(|a, b| a.0 == b.0);
        StoredFiles { folder, files }
    }
}

impl StoredBytes for StoredFiles {
    fn bytes(&self, file: &StoredRef<'_>) -> Result<&[u8], StoredError> {
        let refused = |why: StoredWhy| StoredError { sha256: *file.sha256, why };
        let folder = self.folder.as_ref().ok_or(refused(StoredWhy::NoArea))?;
        let at = self.files.binary_search_by(|(digest, _)| digest.cmp(file.sha256)).map_err(|_| refused(StoredWhy::NotThere))?;
        let mapped = self.files[at].1.get_or_init(|| {
            let name: std::string::String = file.sha256.iter().map(|byte| std::format!("{byte:02x}")).collect();
            match blaze_core::Mapping::map_file(&folder.join(name)) {
                Ok(mapping) => Ok(mapping),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(StoredWhy::NotThere),
                Err(_) => Err(StoredWhy::Unreadable),
            }
        });
        mapped.as_ref().map(blaze_core::Mapping::bytes).map_err(|why| refused(*why))
    }
}

#[cfg(test)]
mod tests {
    use blaze_build_script::outputs::{FileLimits, FileMode, OutFileRef, Outputs, OutputsRef, StoredWhy};
    use blaze_own::AVec;
    use blaze_unit_graph::UnitId;

    use super::StoredFiles;
    use crate::ScriptRecords;

    /// An exported folder's records read their stored files from `<dir>/files` (the census's
    /// `--scripts` reader): the bytes equal; no area is `NoArea`; a file the area lacks is
    /// `NotThere`, each a typed error naming the digest, never an empty file.
    #[test]
    fn exported_records_read_their_stored_files_from_the_folder() {
        let alloc = blaze_own::testing::heap().alloc();
        let dir = std::env::temp_dir().join(std::format!("blaze-world-stored-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("files")).expect("a test folder");
        let bytes = b"pub const X: u32 = 1;\n";
        // The area names a file by the digest the record gives (the writer computed it); this
        // reader checks lengths, not digests.
        let digest = [9u8; 32];
        let hex: std::string::String = digest.iter().map(|b| std::format!("{b:02x}")).collect();
        std::fs::write(dir.join("files").join(&hex), bytes).expect("the stored file");
        let mut outputs = Outputs::new_in("/target/debug/build/x-0/out", alloc);
        outputs.add_stored("bindings.rs", bytes.len() as u64, FileMode::Plain, digest, FileLimits::DEFAULT, alloc).expect("a stored file");
        outputs.add_stored("gone.rs", 3, FileMode::Plain, [7; 32], FileLimits::DEFAULT, alloc).expect("a stored file");
        let mut record: AVec<'static, u8> = AVec::new_in(alloc);
        outputs.encode(&mut record);
        let record: &'static [u8] = record.leak_static();

        let read = |folder: Option<std::path::PathBuf>| {
            let records = ScriptRecords::new([(UnitId(3), record)], alloc).expect("a record");
            let area: &'static StoredFiles = std::boxed::Box::leak(std::boxed::Box::new(StoredFiles::new(folder, records.stored_digests())));
            let opened: OutputsRef<'static> = records.with_area(area).outputs(UnitId(3)).expect("unit 3's record");
            let file = |name: &str| match opened.file(name) {
                Some(OutFileRef::Stored(stored)) => opened.bytes_of(OutFileRef::Stored(stored)).map(<[u8]>::to_vec).map_err(|e| e.why),
                other => panic!("{name}: {other:?}"),
            };
            (file("bindings.rs"), file("gone.rs"))
        };
        let (with, gone) = read(Some(dir.join("files")));
        let (without, _) = read(None);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(with, Ok(bytes.to_vec()));
        assert_eq!(gone, Err(StoredWhy::NotThere));
        assert_eq!(without, Err(StoredWhy::NoArea));
    }
}
