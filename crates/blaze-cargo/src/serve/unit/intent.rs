//! A job's edits by meaning and its build ahead (docs/v2/plan/design/compile-time.md),
//! [`blaze_intent`] over the job's view: an edit reads the view's files as the VFS has them
//! and builds nothing; a build ahead picks the test units its filters select and runs nothing.

#[expect(
    clippy::disallowed_types,
    reason = "each read once, the first time an edit asks: per edit, not global"
)]
use std::cell::OnceCell;

use base_db::FileId;
use blaze_api::{ApiError, Edit, Edited, query::Rows};
use blaze_hash::HashMap;
use blaze_intent::ahead;
use blaze_units::schema::Mode;
use blaze_vfs::Bytes;
use std::alloc::Allocator;

use super::{Job, UnitReader, answer_macros_built, files::Mounted, graph::flag};
use crate::serve::work::Failed;

/// `edits` made in the job's view ([`blaze_intent::apply`]): what they change, for the session
/// to commit. Bytes of the view that fail their check end it with an error naming the file.
/// A request's [`Edit::Rules`] runs every rewrite of its rules file after the request's other
/// edits ([`blaze_astlog::edit`]); one such edit a request. The request's tables are in `alloc`.
pub(crate) fn edit<A: Allocator + Clone>(
    job: &Job,
    reader: &UnitReader,
    edits: &[Edit],
    alloc: A,
) -> Result<Edited, Failed> {
    macros_built(job, reader, "edit")?;
    let mut rules = edits.iter().filter_map(|it| match it {
        Edit::Rules { rules } => Some(rules.as_str()),
        _ => None,
    });
    let (first, second) = (rules.next(), rules.next());
    if second.is_some() {
        let what = "an edit request carries two astlog rules edits: put every rule in one file";
        return Err(ApiError::new(what, None).into());
    }
    let files = ViewFiles::of(job, reader, alloc.clone()); // clone: the caller's allocator handle
    let crates = read_crates(job, alloc.clone())?; // clone: the caller's allocator handle
    let applied = match first {
        Some(rules) => {
            let mut others = Vec::new_in(alloc.clone()); // clone: the caller's allocator handle
            others.extend(edits.iter().filter(|it| !matches!(it, Edit::Rules { .. })).cloned());
            blaze_astlog::edit(reader.db(), &files, &crates, rules, &others, alloc)
                .map_err(|error| blaze_astlog::api_error(&error))
        }
        None => blaze_intent::apply(reader.db(), &files, &crates, edits, alloc),
    };
    if let Some((path, why)) = files.corrupt.into_inner() {
        return Err(ApiError::new(why, Some(path)).into());
    }
    let applied = applied?;
    // alloc: once per edit, one seed per file it reparsed
    *job.seeds.lock() = applied.seeds().collect();
    Ok(applied.edited)
}

/// `rules` run over the job's view ([`blaze_astlog::query`]): every relation they derive and
/// every lint's finding. Writes nothing and builds nothing past the view's proc macros. The
/// request's tables are in `alloc`.
pub(crate) fn query<A: Allocator + Clone>(
    job: &Job,
    reader: &UnitReader,
    rules: &str,
    alloc: A,
) -> Result<Rows, Failed> {
    macros_built(job, reader, "query")?;
    let files = ViewFiles::of(job, reader, alloc.clone()); // clone: the caller's allocator handle
    let crates = read_crates(job, alloc)?;
    let rows = blaze_astlog::query(reader.db(), &files, &crates, rules)
        .map_err(|error| blaze_astlog::api_error(&error));
    if let Some((path, why)) = files.corrupt.into_inner() {
        return Err(ApiError::new(why, Some(path)).into());
    }
    Ok(rows?)
}

/// The crates an edit or a query reads: the job's plan's, never another job's the front end
/// still has (a test build's beside a default check's), in [`read_order`].
fn read_crates<A: Allocator + Clone>(job: &Job, alloc: A) -> Result<Vec<hir::Crate, A>, Failed> {
    let mut units = Vec::with_capacity_in(job.graph.len(), alloc.clone()); // clone: the caller's allocator handle
    for at in 0..job.graph.len() {
        let unit = job.unit(at)?;
        units.push((unit.mode == Mode::Test || unit.checked_as_test, job.crates.krate(at)));
    }
    Ok(read_order(&units, alloc))
}

/// The crates of a plan's `units` (each whether it is built with `cfg(test)`, and its crate),
/// each once: the test units' first, then the rest, each in plan order. Of two that have one
/// file (a lib and its test unit), the test unit's answers for it
/// (docs/v2/plan/design/astlog-sema.md, "One crate a file").
pub(super) fn read_order<A: Allocator>(units: &[(bool, Option<base_db::Crate>)], alloc: A) -> Vec<hir::Crate, A> {
    let mut crates = Vec::new_in(alloc);
    for test in [true, false] {
        for &(tested, krate) in units {
            let Some(krate) = krate.filter(|_| tested == test).map(hir::Crate::from) else {
                continue;
            };
            if !crates.contains(&krate) {
                crates.push(krate);
            }
        }
    }
    crates
}

/// Every crate of the view has its proc macros built and applied: an edit or a query resolves
/// names in any of them. `Jobs::next` posted the job only after they were; checked again here.
fn macros_built(job: &Job, reader: &UnitReader, what: &str) -> Result<(), Failed> {
    for at in 0..job.graph.len() {
        let Some(krate) = job.crates.krate(at) else { continue };
        if job.cancelled() {
            return Err(Failed::Again);
        }
        answer_macros_built(reader.db(), krate, &job.unit(at)?.target.name, what)?;
    }
    Ok(())
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
        // Its tests are listed from its def map: its proc macros were built and applied before
        // this was posted (`Jobs::next`); checked again here.
        if let Some(krate) = job.crates.krate(at) {
            answer_macros_built(reader.db(), krate, &unit.target.name, "ahead")?;
        }
        let roots = super::codegen::test_roots(job, reader, at)
            .map_err(|what| ApiError::new(what, Some(unit.target.name.clone())))?;
        let unit = u32::try_from(at).expect("under 2^32 units");
        listed.push((unit, roots.tests));
    }
    let tests: Vec<Vec<ahead::Test<'_>>> = listed
        .iter()
        .map(|(_, tests)| {
            tests.iter().map(|it| ahead::Test { name: &it.name, reached: &[] }).collect()
        })
        .collect();
    let binaries: Vec<ahead::Binary<'_>> = listed
        .iter()
        .zip(&tests)
        .map(|((unit, _), tests)| ahead::Binary { unit: *unit, tests })
        .collect();
    Ok(ahead::plan(&binaries, filter).units)
}

/// The front end's files in the view, by their path in it, each one's bytes read from the VFS
/// the first time an edit opens it.
#[expect(
    clippy::disallowed_types,
    reason = "each read once, the first time an edit asks: per edit, not global"
)]
struct ViewFiles<'a, A: Allocator> {
    mounted: &'a Mounted,
    files: Vec<ViewFile<'a>, A>,
    by_path: HashMap<Box<str>, u32, A>,
    by_id: HashMap<FileId, u32, A>,
    /// The first file whose bytes failed their check, and why.
    corrupt: OnceCell<(String, String)>,
}

#[expect(
    clippy::disallowed_types,
    reason = "each read once, the first time an edit asks: per edit, not global"
)]
struct ViewFile<'a> {
    id: FileId,
    absolute: &'a str,
    path: Box<str>,
    bytes: OnceCell<Option<Bytes>>,
}

impl<'a, A: Allocator + Clone> ViewFiles<'a, A> {
    /// The job's view over the reader's front end, its tables in `alloc`.
    fn of(job: &'a Job, reader: &'a UnitReader, alloc: A) -> ViewFiles<'a, A> {
        let files = reader.db().files();
        ViewFiles::new(&job.mounted, reader.paths(), |id| files.alias_of(id).is_some(), alloc)
    }

    /// `paths`' files in the view. A path names the file on disk, never an alias of it
    /// (`is_alias`: a crate root blaze appended a module to), whatever order `paths` is in.
    #[expect(
        blaze::narrowed_len,
        reason = "view files are u32 indices: a view of 2^32 files needs a typed refusal from \
                  the intent's builder"
    )]
    fn new(
        mounted: &'a Mounted,
        paths: &'a std::collections::HashMap<FileId, String>,
        is_alias: impl Fn(FileId) -> bool,
        alloc: A,
    ) -> ViewFiles<'a, A> {
        let mut files = Vec::new_in(alloc.clone()); // clone: the caller's allocator handle
        let mut by_path = HashMap::new_in(alloc.clone()); // clone: the caller's allocator handle
        let mut by_id = HashMap::new_in(alloc);
        for (&id, absolute) in paths {
            let Some(path) = mounted.in_view(absolute) else { continue };
            let at = u32::try_from(files.len()).expect("under 2^32 files");
            let path: Box<str> = path.into();
            if !is_alias(id) || !by_path.contains_key(&path) {
                by_path.insert(path.clone(), at);
            }
            by_id.insert(id, at);
            files.push(ViewFile { id, absolute, path, bytes: Default::default() });
        }
        ViewFiles { mounted, files, by_path, by_id, corrupt: Default::default() }
    }
}

impl<A: Allocator> blaze_intent::Files for ViewFiles<'_, A> {
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

    /// A front-end file's bytes as [`Self::file`] keeps them; any other file of the view (a
    /// rules file) read from the VFS each time it is asked, which a load does once a file.
    fn with_bytes<R>(&self, path: &str, with: impl FnOnce(&[u8]) -> R) -> Option<R> {
        if let Some((_, bytes)) = self.file(path) {
            return Some(with(bytes));
        }
        let absolute = format!("{}/{path}", self.mounted.root());
        self.mounted.in_view(&absolute)?;
        match self.mounted.read(&absolute) {
            Ok(found) => found.map(|(_, _, bytes)| with(&bytes)),
            Err(corrupt) => {
                self.corrupt.get_or_init(|| (absolute.clone(), corrupt.to_string()));
                None
            }
        }
    }
}

#[cfg(test)]
mod one_copy;

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
        let write = Op::Write {
            mode: Mode::Regular,
            path: "src/lib.rs".into(),
            bytes: b"pub fn a() {}\n".to_vec(),
        };
        let root = vfs.apply(vfs.empty(), &[write]).unwrap();
        let mounts = [Mount { at: "/w".into(), origin: Origin::View, under: String::new() }];
        let mounted = Mounted::new(vfs.reader(), root, &mounts, "/w");
        let paths: std::collections::HashMap<FileId, String> = [
            (FileId::from_raw(0), "/w/src/lib.rs".to_owned()),
            (FileId::from_raw(1), "/registry/serde/src/lib.rs".to_owned()),
        ]
        .into_iter()
        .collect();
        let files = ViewFiles::new(&mounted, &paths, |_| false, std::alloc::Global);
        assert_eq!(files.path(FileId::from_raw(0)), Some("src/lib.rs"));
        assert_eq!(files.path(FileId::from_raw(1)), None);
        assert_eq!(files.file("src/lib.rs"), Some((FileId::from_raw(0), &b"pub fn a() {}\n"[..])));
        assert_eq!(files.file("/registry/serde/src/lib.rs"), None);
        assert!(files.corrupt.into_inner().is_none());
    }

    #[test]
    fn a_file_the_front_end_does_not_load_is_read_from_the_view_by_its_path_in_it() {
        let (mut vfs, _) = Vfs::restore(FileName::new("vfs/journal").unwrap(), 0, &[], None);
        let writes = [
            Op::Write { mode: Mode::Regular, path: "src/lib.rs".into(), bytes: b"pub fn a() {}\n".to_vec() },
            Op::Write {
                mode: Mode::Regular,
                path: "rules/lib/calls.astlog".into(),
                bytes: b"(rule (r x) (fn-item x))\n".to_vec(),
            },
        ];
        let root = vfs.apply(vfs.empty(), &writes).unwrap();
        let mounts = [Mount { at: "/w".into(), origin: Origin::View, under: String::new() }];
        let mounted = Mounted::new(vfs.reader(), root, &mounts, "/w");
        let paths: std::collections::HashMap<FileId, String> =
            [(FileId::from_raw(0), "/w/src/lib.rs".to_owned())].into_iter().collect();
        let files = ViewFiles::new(&mounted, &paths, |_| false, std::alloc::Global);
        assert_eq!(files.file("rules/lib/calls.astlog"), None, "not a front-end file");
        let read = files.with_bytes("rules/lib/calls.astlog", <[u8]>::to_vec);
        assert_eq!(read.as_deref(), Some(&b"(rule (r x) (fn-item x))\n"[..]));
        let front = files.with_bytes("src/lib.rs", <[u8]>::to_vec);
        assert_eq!(front.as_deref(), Some(&b"pub fn a() {}\n"[..]));
        assert_eq!(files.with_bytes("rules/missing.astlog", <[u8]>::to_vec), None);
        assert!(files.corrupt.into_inner().is_none());
    }
}
