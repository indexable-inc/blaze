//! A job's edits by meaning and its build ahead (docs/v2/plan/lanes/fastcompile/intent.md),
//! [`blaze_intent`] over the job's view: an edit reads the view's files as the VFS has them
//! and builds nothing; a build ahead picks the test units its filters select and runs nothing.

use std::cell::OnceCell;

use base_db::FileId;
use blaze_api::{ApiError, Edit, Edited};
use blaze_hash::HashMap;
use blaze_intent::ahead;
use blaze_vfs::Bytes;

use super::{Job, UnitReader, files::Mounted, graph::flag};
use crate::serve::work::Failed;

/// `edits` made in the job's view ([`blaze_intent::apply`]): what they change, for the session
/// to commit. Bytes of the view that fail their check end it with an error naming the file.
pub(crate) fn edit(job: &Job, reader: &UnitReader, edits: &[Edit]) -> Result<Edited, Failed> {
    let files = ViewFiles::new(&job.mounted, reader.paths());
    let applied = blaze_intent::apply(reader.db(), &files, edits);
    if let Some((path, why)) = files.corrupt.into_inner() {
        return Err(ApiError::new(why, Some(path)).into());
    }
    let applied = applied?;
    // alloc: once per edit, one seed per file it reparsed
    *job.seeds.lock() = applied.seeds().collect();
    Ok(applied.edited)
}

/// The job's test units (by their place in its graph) that hold a test `filter` selects, as
/// libtest selects ([`ahead::plan`]). Every test reached nothing yet: the functions each last
/// ran are blaze-testsel's record, which the daemon does not keep yet.
pub(crate) fn ahead(job: &Job, reader: &UnitReader, filter: &[String]) -> Result<Vec<u32>, Failed> {
    let mut listed = Vec::new();
    for at in 0..job.graph.len() {
        if !job.graph.has(at as u32, flag::TEST) {
            continue;
        }
        if job.cancelled() {
            return Err(Failed::Again);
        }
        let unit = job.unit(at)?;
        let roots = super::codegen::test_roots(job, reader, at)
            .map_err(|what| ApiError::new(what, Some(unit.target.name.clone())))?;
        let unit = u32::try_from(at).expect("under 2^32 units");
        listed.push((unit, roots.tests));
    }
    let tests: Vec<Vec<ahead::Test<'_>>> = listed
        .iter()
        .map(|(_, tests)| tests.iter().map(|it| ahead::Test { name: &it.name, reached: &[] }).collect())
        .collect();
    let binaries: Vec<ahead::Binary<'_>> =
        listed.iter().zip(&tests).map(|((unit, _), tests)| ahead::Binary { unit: *unit, tests }).collect();
    Ok(ahead::plan(&binaries, filter).units)
}

/// The front end's files in the view, by their path in it, each one's bytes read from the VFS
/// the first time an edit opens it.
struct ViewFiles<'a> {
    mounted: &'a Mounted,
    files: Vec<ViewFile<'a>>,
    by_path: HashMap<Box<str>, u32>,
    by_id: HashMap<FileId, u32>,
    /// The first file whose bytes failed their check, and why.
    corrupt: OnceCell<(String, String)>,
}

struct ViewFile<'a> {
    id: FileId,
    absolute: &'a str,
    path: Box<str>,
    bytes: OnceCell<Option<Bytes>>,
}

impl<'a> ViewFiles<'a> {
    fn new(mounted: &'a Mounted, paths: &'a std::collections::HashMap<FileId, String>) -> ViewFiles<'a> {
        let mut files = Vec::new();
        let (mut by_path, mut by_id) = (HashMap::default(), HashMap::default());
        for (&id, absolute) in paths {
            let Some(path) = mounted.in_view(absolute) else { continue };
            let at = u32::try_from(files.len()).expect("under 2^32 files");
            let path: Box<str> = path.into();
            by_path.insert(path.clone(), at);
            by_id.insert(id, at);
            files.push(ViewFile { id, absolute, path, bytes: OnceCell::new() });
        }
        ViewFiles { mounted, files, by_path, by_id, corrupt: OnceCell::new() }
    }
}

impl blaze_intent::Files for ViewFiles<'_> {
    fn file(&self, path: &str) -> Option<(FileId, &[u8])> {
        let file = &self.files[*self.by_path.get(path)? as usize];
        let bytes = file.bytes.get_or_init(|| match self.mounted.read(file.absolute) {
            Ok(it) => it.map(|(_, _, bytes)| bytes),
            Err(corrupt) => {
                _ = self.corrupt.set((file.absolute.to_owned(), corrupt.to_string()));
                None
            }
        });
        bytes.as_deref().map(|it| (file.id, it))
    }

    fn path(&self, file: FileId) -> Option<&str> {
        self.by_id.get(&file).map(|&at| &*self.files[at as usize].path)
    }
}

#[cfg(test)]
mod tests {
    use blaze_core::FileName;
    use blaze_intent::Files as _;
    use blaze_units::query::{Mount, Origin};
    use blaze_vfs::{Mode, Op, Vfs};

    use super::*;

    #[test]
    fn only_the_views_files_are_named_and_each_by_its_path_in_it() {
        let (mut vfs, _) = Vfs::restore(FileName::new("vfs/journal").unwrap(), 0, &[], None);
        let write = Op::Write { mode: Mode::Regular, path: "src/lib.rs".into(), bytes: b"pub fn a() {}\n".to_vec() };
        let root = vfs.apply(vfs.empty(), &[write]).unwrap();
        let mounts = [Mount { at: "/w".into(), origin: Origin::View, under: String::new() }];
        let mounted = Mounted::new(vfs.reader(), root, &mounts, "/w");
        let paths: std::collections::HashMap<FileId, String> = [
            (FileId::from_raw(0), "/w/src/lib.rs".to_owned()),
            (FileId::from_raw(1), "/registry/serde/src/lib.rs".to_owned()),
        ]
        .into_iter()
        .collect();
        let files = ViewFiles::new(&mounted, &paths);
        assert_eq!(files.path(FileId::from_raw(0)), Some("src/lib.rs"));
        assert_eq!(files.path(FileId::from_raw(1)), None);
        assert_eq!(files.file("src/lib.rs"), Some((FileId::from_raw(0), &b"pub fn a() {}\n"[..])));
        assert_eq!(files.file("/registry/serde/src/lib.rs"), None);
        assert!(files.corrupt.into_inner().is_none());
    }
}
