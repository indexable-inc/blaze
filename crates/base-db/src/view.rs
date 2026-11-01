//! One view's files as the front end's inputs (docs/v2/plan/lanes/vfs.md section 2): a view is a root of
//! blaze-vfs, a map of every file to its content ID, and setting one writes only the inputs
//! whose content changed, so the query engine sees no change where there is none.
//!
//! A view's path maps to a file and a source root through what the database has already: a
//! path a source root has named keeps its [`FileId`] for good, and a
//! path none has is given an unused one and put in the source root that the partition the roots
//! were made with ([`FileSetConfig`]) puts it in.
//!
//! The content ID a view gives is blaze-vfs's ([`blaze_vfs::FILE_DOMAIN`], of the bytes
//! alone), checked against the bytes: the one time a file's bytes are hashed, every file of a
//! call in one batch. A file's [`FileText::content`] is built on it
//! ([`FileText::content_of_file`], adding the module the compiler appends), and blaze-vfs's is
//! kept to tell next time that a file is unchanged without reading it.
//!
//! A file the view has as it was costs a check of its path, one lookup of the path as the view
//! names it in its base's [`ViewIndex`], and two lookups by file ID: nothing is allocated, joined
//! or hashed with BLAKE3 for it. Only a file the source roots gain or lose rebuilds its root's
//! file set; its content ID is updated in place ([`RootContent::insert`]).

use std::{borrow::Cow, cell::RefCell, fmt};

use blaze_ids::{ContentId, radix};
use blaze_mem::{Frame, Tag, frame, scratch::Scratch};
use blaze_hash::{FixedState, HashMap};
use blaze_query::db::{Durability, Setter};
use triomphe::Arc;
use crate::{AbsPath, AbsPathBuf, FileId, FileSet, FileSetConfig, VfsPath};

use crate::{
    Db, FileText, Files, FxIndexMap, SourceRoot, SourceRootId, content,
    files::{RootContent, Roots, Writer, index_file},
};

/// One file of a view, as blaze-vfs reads it.
#[derive(Clone, Copy, Debug)]
pub struct ViewFile<'a> {
    /// Relative to the view's root and `/`-separated, as blaze-vfs names it.
    pub path: &'a str,
    /// Its blaze-vfs content ID.
    pub id: ContentId,
    /// Its bytes as blaze-vfs holds them (a heap file's allocation or a range of the mapped
    /// image): a file with text keeps them as its text ([`blaze_vfs::text_of`]), with no copy.
    pub bytes: &'a blaze_vfs::Bytes,
}

/// Every file of one view, and where the database's source roots put them.
#[derive(Clone, Copy, Debug)]
pub struct ViewRoot<'a> {
    /// The directory the view is checked out as: its paths are relative to this.
    pub base: &'a AbsPath,
    /// Directories under `base` whose files are not the view's (what a build writes): their
    /// files are left as they are.
    pub built: &'a [AbsPathBuf],
    /// The partition the source roots were made with: set `i` is `SourceRootId(i)`.
    pub partition: &'a FileSetConfig,
    /// The source root of the files no set of `partition` has.
    pub other: SourceRootId,
    pub files: &'a [ViewFile<'a>],
}

/// What setting a view wrote, each list sorted.
#[derive(Clone, Debug, Default, PartialEq, Eq)] // clone: once per view change, the edge
pub struct ViewChange {
    /// Files that were in a source root already whose text was set.
    pub set: Vec<FileId>,
    /// Files put in a source root.
    pub added: Vec<FileId>,
    /// Files under the view's base taken out of their source root: the view has them no more.
    pub removed: Vec<FileId>,
    /// Of the files set or added, those whose bytes are not UTF-8: they have no text
    /// ([`Db::has_file_text`] is false).
    pub no_text: Vec<FileId>,
    /// Source roots whose files were added or removed: a crate root may have come or gone.
    pub roots: Vec<SourceRootId>,
}

/// Why a view was refused. Nothing is written when one is.
#[derive(Clone, Debug, PartialEq, Eq)] // clone: an error, cold
pub enum ViewError {
    /// Empty, absolute, or with an empty, `.` or `..` part.
    BadPath(String),
    /// Two files of the view have the path.
    Duplicate(String),
    /// The content ID given for the file is not that of its bytes.
    Mismatch { path: String, given: ContentId, bytes: ContentId },
    /// The partition puts the new file in a source root the database does not have.
    NoSourceRoot { path: String, root: SourceRootId },
    /// Every file ID is taken.
    NoFileId(String),
}

impl fmt::Display for ViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ViewError::BadPath(path) => write!(f, "{path:?} is not a path of a view"),
            ViewError::Duplicate(path) => write!(f, "the view has {path} twice"),
            ViewError::Mismatch { path, given, bytes } => write!(
                f,
                "{path} was given the content ID {given:x}, but its bytes are {bytes:x}"
            ),
            ViewError::NoSourceRoot { path, root } => {
                write!(f, "{path} belongs in the source root {root:?}, which the database lacks")
            }
            ViewError::NoFileId(path) => write!(f, "no file ID is left for {path}"),
        }
    }
}

impl std::error::Error for ViewError {}

/// Every path under one view base that a source root has named, as a view names it (relative,
/// `/`-separated), with its file: what [`Files::set_view_root`] looks a view's path up in
/// without joining it to the base. Made from [`Writer::by_path`] the first time the base is
/// set, then kept by [`Writer::note_path`].
///
/// Per path: its text once, its file, and a 4-byte mark of the last call that saw it, so
/// finding the paths a view lacks is a pass over the marks.
#[derive(Default)]
pub(crate) struct ViewIndex {
    paths: FxIndexMap<Box<str>, FileId>,
    seen: Vec<u32>,
    call: u32,
}

impl ViewIndex {
    pub(crate) fn new(base: &AbsPath, by_path: &HashMap<VfsPath, FileId>) -> ViewIndex {
        let mut index = ViewIndex::default();
        for (path, &file) in by_path {
            index.note(base, path, file);
        }
        index
    }

    /// Notes that a source root names `file` at `path`, if `path` is under `base`.
    pub(crate) fn note(&mut self, base: &AbsPath, path: &VfsPath, file: FileId) {
        let Some(rel) = path.as_path().and_then(|path| path.strip_prefix(base)) else { return };
        let rel = rel_key(rel.as_str());
        if rel.is_empty() {
            return;
        }
        let (at, _) = self.paths.insert_full(Box::from(&*rel), file);
        if at == self.seen.len() {
            self.seen.push(0);
        }
    }

    /// Starts a call: no path is marked seen by it yet.
    fn begin(&mut self) -> u32 {
        self.call = self.call.wrapping_add(1);
        if self.call == 0 {
            self.seen.fill(0);
            self.call = 1;
        }
        self.call
    }

    fn find(&self, rel: &str) -> Option<usize> {
        self.paths.get_index_of(rel)
    }

    fn file(&self, at: usize) -> FileId {
        self.paths[at]
    }

    /// Marks the path at `at` seen by `call`; false if it was already.
    fn mark(&mut self, at: usize, call: u32) -> bool {
        let seen = &mut self.seen[at];
        let first = *seen != call;
        *seen = call;
        first
    }

    /// The paths `call` has not seen, with their files.
    fn unseen(&self, call: u32) -> impl Iterator<Item = (&str, FileId)> + '_ {
        self.paths
            .iter()
            .zip(&self.seen)
            .filter(move |(_, seen)| **seen != call)
            .map(|((rel, file), _)| (&**rel, *file))
    }
}

/// A path relative to a view's base as the view names it: `/`-separated.
fn rel_key(rel: &str) -> Cow<'_, str> {
    if cfg!(windows) && rel.contains('\\') { Cow::Owned(rel.replace('\\', "/")) } else { Cow::Borrowed(rel) }
}

/// Whether `rel` is `dir` or under it, both relative to one base; `""` is the base.
fn under(rel: &str, dir: &str) -> bool {
    dir.is_empty()
        || rel.strip_prefix(dir).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

thread_local! {
    static ROOT_SORT: RefCell<Scratch<SourceRootId>> = const { RefCell::new(Scratch::new()) };
    static FILE_SORT: RefCell<Scratch<FileId>> = const { RefCell::new(Scratch::new()) };
}

fn sort_roots(roots: &mut [SourceRootId]) {
    ROOT_SORT.with_borrow_mut(|scratch| radix::sort_copy_by_key_in(roots, |it| it.0, scratch.take()));
}

fn sort_files(files: &mut [FileId]) {
    FILE_SORT.with_borrow_mut(|scratch| {
        radix::sort_copy_by_key_in(files, |it| it.index(), scratch.take())
    });
}

/// `list` as the vector a [`ViewChange`] hands its caller.
fn owned<T: Copy>(list: &[T]) -> Vec<T> {
    // alloc: the caller's list, at its size; none for an empty one.
    list.to_vec()
}

/// What a view will write, worked out before anything is.
struct Plan<'f> {
    /// Each file whose input may change, in the view's order.
    writes: Vec<Write, &'f Frame>,
    /// The files put in a source root, with their paths, in the view's order.
    added: Vec<(FileId, SourceRootId, VfsPath), &'f Frame>,
    /// The files taken out of a source root, with it.
    removed: Vec<(SourceRootId, FileId), &'f Frame>,
    /// [`Writer::next_file`] once the plan is written.
    next_file: u32,
}

struct Write {
    /// Where it is in [`ViewRoot::files`].
    at: usize,
    id: FileId,
    root: SourceRootId,
    added: bool,
}

impl Files {
    /// Sets the inputs of the files of `view`, and only those whose content changed: a file
    /// whose blaze-vfs ID is the one it was last set from is not read again (nor its bytes
    /// checked), one whose text is what it has is not written, so setting a view twice writes
    /// nothing. Files under `view.base` that the view lacks are taken out of their source
    /// root. Bytes that are not UTF-8 are a file with no text. Every file's ID is checked
    /// against its bytes before anything is written, and a mismatch refuses the whole view.
    pub fn set_view_root(
        &self,
        db: &mut Db,
        view: ViewRoot<'_>,
    ) -> Result<ViewChange, ViewError> {
        frame::task(Tag::Parse, |frame| {
            let mut writer = self.lock();
            // The paths a source root names are normalized (`VfsPath::from`), so the base must be.
            let normalized;
            let base = match view.base.components().any(|it| matches!(it.as_str(), "." | "..")) {
                true => {
                    // alloc: only for a base with a `.` or `..` part.
                    normalized = view.base.normalize();
                    &*normalized
                }
                false => view.base,
            };
            let mut index = match writer.views.get_mut(base) {
                Some(index) => std::mem::take(index),
                None => ViewIndex::new(base, &writer.by_path),
            };
            let plan = self.plan_view(&writer, &mut index, &view, frame);
            match writer.views.get_mut(base) {
                Some(slot) => *slot = index,
                None => {
                    // alloc: the base's key, once per view base.
                    writer.views.insert(base.to_path_buf(), index);
                }
            }
            Ok(self.write_view(&mut writer, db, &view, plan?, frame))
        })
    }

    /// What `view` changes, or why it is refused: reads only.
    fn plan_view<'f>(
        &self,
        writer: &Writer,
        index: &mut ViewIndex,
        view: &ViewRoot<'_>,
        frame: &'f Frame,
    ) -> Result<Plan<'f>, ViewError> {
        let call = index.begin();
        let mut fresh: hashbrown::HashSet<&str, FixedState, &Frame> =
            hashbrown::HashSet::with_hasher_in(FixedState, frame);
        let mut plan = Plan {
            writes: Vec::new_in(frame),
            added: Vec::new_in(frame),
            removed: Vec::new_in(frame),
            next_file: writer.next_file,
        };
        for (at, file) in view.files.iter().enumerate() {
            check_path(file.path)?;
            let known = match index.find(file.path) {
                Some(slot) => {
                    if !index.mark(slot, call) {
                        // alloc: the refusal's message (an error path).
                        return Err(ViewError::Duplicate(file.path.to_owned()));
                    }
                    Some(index.file(slot))
                }
                None => {
                    if !fresh.insert(file.path) {
                        // alloc: the refusal's message (an error path).
                        return Err(ViewError::Duplicate(file.path.to_owned()));
                    }
                    None
                }
            };
            let in_root = known.and_then(|id| writer.roots_of(id).first().copied());
            if let (Some(id), Some(_)) = (known, in_root)
                && writer.view_ids.get(id) == Some(&file.id)
            {
                continue;
            }
            let (id, root, added) = match (known, in_root) {
                (Some(id), Some(root)) => (id, root, false),
                _ => {
                    let path = VfsPath::from(view.base.join(file.path));
                    let root = view
                        .partition
                        .classify_path(&path)
                        .map_or(view.other, |set| SourceRootId(set as u32));
                    if !self.has_source_root(root) {
                        // alloc: the refusal's message (an error path).
                        return Err(ViewError::NoSourceRoot { path: file.path.to_owned(), root });
                    }
                    let id = match known {
                        Some(id) => id,
                        None => self
                            .unused_file_id(writer, &mut plan.next_file)
                            // alloc: the refusal's message (an error path).
                            .ok_or_else(|| ViewError::NoFileId(file.path.to_owned()))?,
                    };
                    plan.added.push((id, root, path));
                    (id, root, true)
                }
            };
            plan.writes.push(Write { at, id, root, added });
        }

        let bytes =
            frame.alloc_slice_from_iter(plan.writes.iter().map(|it| &view.files[it.at].bytes[..]));
        let ids = content::hash_many(blaze_vfs::FILE_DOMAIN, bytes, frame);
        for (write, &bytes) in plan.writes.iter().zip(ids) {
            let file = &view.files[write.at];
            if bytes != file.id {
                return Err(ViewError::Mismatch {
                    // alloc: the refusal's message (an error path).
                    path: file.path.to_owned(),
                    given: file.id,
                    bytes,
                });
            }
        }

        let mut built: Vec<Cow<'_, str>, &Frame> = Vec::with_capacity_in(view.built.len(), frame);
        built.extend(view.built.iter().filter_map(|built| match view.base.starts_with(built) {
            true => Some(Cow::Borrowed("")),
            false => Some(rel_key(built.strip_prefix(view.base)?.as_str())),
        }));
        for (rel, id) in index.unseen(call) {
            if built.iter().any(|built| under(rel, built)) {
                continue;
            }
            plan.removed.extend(writer.roots_of(id).iter().map(|&root| (root, id)));
        }
        Ok(plan)
    }

    /// Writes what [`Files::plan_view`] found, and says what it wrote.
    fn write_view(
        &self,
        writer: &mut Writer,
        db: &mut Db,
        view: &ViewRoot<'_>,
        plan: Plan<'_>,
        frame: &Frame,
    ) -> ViewChange {
        let mut texts: Vec<Option<syntax::Text>, &Frame> =
            Vec::with_capacity_in(plan.writes.len(), frame);
        texts.extend(
            plan.writes
                .iter()
                .map(|it| blaze_vfs::text_of(view.files[it.at].bytes.clone()).ok()), // clone: a count, the text holds the bytes
        );
        let mut bytes = blaze_mem::scratch::lend_bytes();
        let mut ranges = Vec::new_in(frame);
        for (write, text) in plan.writes.iter().zip(&texts) {
            if text.is_none() {
                continue;
            }
            let appended = self.input_of(writer, write.id).and_then(|it| it.appended_root_module(&*db));
            let start = bytes.len();
            FileText::content_bytes(&view.files[write.at].id, appended, &mut bytes);
            ranges.push(start..bytes.len());
        }
        let mut contents =
            content::hash_ranges(FileText::DOMAIN, &bytes, &ranges, frame).iter().copied();
        let (mut set, mut added, mut removed, mut no_text) =
            (Vec::new_in(frame), Vec::new_in(frame), Vec::new_in(frame), Vec::new_in(frame));

        let mut changed_roots = Roots::new();
        for (write, text) in plan.writes.iter().zip(texts) {
            let content = match text {
                Some(_) => contents.next().expect("a text ID for each file with text"),
                None => {
                    no_text.push(write.id);
                    FileText::no_text()
                }
            };
            if write.added {
                added.push(write.id);
            }
            let unchanged = match (&text, self.text_input(write.id)) {
                (Some(_), Some(input)) => input.content(&*db) == content,
                (None, None) => true,
                (Some(_), None) | (None, Some(_)) => false,
            };
            if unchanged {
                continue;
            }
            let is_library = writer
                .root_contents
                .get(&write.root)
                .expect("a source root's content is kept from when it is set")
                .is_library;
            let durability = if is_library { Durability::HIGH } else { Durability::LOW };
            let file = view.files[write.at].id;
            let text_of = text.as_ref().map(|text| (text, file));
            self.write_aliases(writer, db, write.id, text_of, Some(durability), |alias| set.push(alias));
            let content = self.write_text(
                writer,
                db,
                write.id,
                text.map(|text| (text, file, content)),
                Some(durability),
            );
            index_file(writer, write.id, content, &mut changed_roots);
            if !write.added {
                set.push(write.id);
            }
        }

        for &(id, root, _) in &plan.added {
            self.set_file_source_root_in(writer, db, id, root, Durability::MEDIUM);
        }
        let mut roots: Vec<SourceRootId, &Frame> =
            Vec::with_capacity_in(plan.added.len() + plan.removed.len(), frame);
        roots.extend(plan.added.iter().map(|it| it.1).chain(plan.removed.iter().map(|it| it.0)));
        sort_roots(&mut roots);
        roots.dedup();
        self.write_root_files(writer, db, &plan, &roots, &mut changed_roots, frame);
        let gone = plan.removed.iter().map(|&(_, id)| id);
        self.drop_orphan_aliases(writer, db, gone, |alias| removed.push(alias));
        self.write_root_contents(writer, db, &changed_roots, frame);
        writer.named.publish(db, frame);

        for &(_, id) in &plan.removed {
            writer.view_ids.remove(id);
            removed.push(id);
        }
        for write in &plan.writes {
            writer.view_ids.insert(write.id, view.files[write.at].id);
        }
        writer.next_file = plan.next_file;
        for list in [&mut set, &mut added, &mut removed, &mut no_text] {
            sort_files(list);
            list.dedup();
        }
        ViewChange {
            set: owned(&set),
            added: owned(&added),
            removed: owned(&removed),
            no_text: owned(&no_text),
            roots: owned(&roots),
        }
    }

    /// Puts the plan's added files in their source roots and takes its removed ones out: each
    /// of `roots` gets its new file set, and its content is updated for the files it gained
    /// and lost (the paths of those gained hashed in one batch), not made again from all of
    /// them. Adds to `changed` the roots whose content IDs are to be written.
    fn write_root_files(
        &self,
        writer: &mut Writer,
        db: &mut Db,
        plan: &Plan<'_>,
        roots: &[SourceRootId],
        changed: &mut Roots,
        frame: &Frame,
    ) {
        let mut keys = Vec::with_capacity_in(plan.added.len(), frame);
        keys.extend(plan.added.iter().map(|(_, _, path)| content::path_key(path)));
        let key_bytes = frame.alloc_slice_from_iter(keys.iter().map(|key| &**key));
        let path_hashes = content::hash_many(SourceRoot::PATH_DOMAIN, key_bytes, frame);
        let mut keys: Vec<Option<(Box<[u8]>, ContentId)>, &Frame> = {
            let mut placed = Vec::with_capacity_in(keys.len(), frame);
            placed.extend(keys.into_iter().zip(path_hashes).map(|(key, &hash)| Some((key, hash))));
            placed
        };

        for &root in roots {
            let input = self.source_root(root);
            let mut gone: hashbrown::HashSet<FileId, FixedState, &Frame> =
                hashbrown::HashSet::with_hasher_in(FixedState, frame);
            gone.extend(plan.removed.iter().filter(|it| it.0 == root).map(|it| it.1));
            // The root's set, taken out of its input to be edited where it is: no reader holds
            // it by then (they read it by reference), so `make_mut` copies nothing.
            let spare = writer.spare_root();
            let mut source_root =
                input.set_source_root_field(db).with_durability(Durability::MEDIUM).to(spare);
            let index = writer
                .root_contents
                .get_mut(&root)
                .expect("a source root's content is kept from when it is set");
            let added = plan.added.iter().filter(|it| it.1 == root);
            index.one_to_one = match index.one_to_one {
                true => {
                    // clone: only for a set a reader still holds (copy on write).
                    let files = Arc::make_mut(&mut source_root);
                    for &id in &gone {
                        files.remove_file(id);
                    }
                    let mut one_to_one = true;
                    for (id, _, path) in added {
                        one_to_one &= files.path_for_file(id).is_none() && files.file_for_path(path).is_none();
                        // clone: an added file's path in the set.
                        files.insert_file(*id, path.clone());
                    }
                    one_to_one
                }
                false => {
                    let (files, one_to_one) = rebuilt(&source_root, &gone, added);
                    // alloc: the set made again, for a set that names a path for two files.
                    source_root = Arc::new(files);
                    one_to_one
                }
            };
            for &id in &gone {
                index.remove(id);
            }
            for (at, (id, added_root, _)) in plan.added.iter().enumerate() {
                if *added_root != root {
                    continue;
                }
                let (key, path) = keys[at].take().expect("an added file is put in one root");
                index.insert(*id, key, path, self.entry_content(&*db, *id));
            }
            for &id in &gone {
                if let Some(roots) = writer.in_roots.get_mut(id) {
                    roots.retain(|it| *it != root);
                }
            }
            for (id, added_root, path) in &plan.added {
                if *added_root != root {
                    continue;
                }
                let roots = writer.in_roots.slot(*id).get_or_insert_default();
                if !roots.contains(&root) {
                    roots.push(root);
                }
                writer.note_path(path, *id);
            }
            input.set_source_root_field(db).with_durability(Durability::MEDIUM).to(source_root);
            if !changed.contains(&root) {
                changed.push(root);
            }
        }
    }
}

/// `old` without `gone` and with `added`, made again from its files in their order: what a
/// set that names a path for two files or a file at two paths is edited as, since there the
/// file a path names after a removal depends on all of them. Whether the new set names each
/// file at one path and each path for one file.
fn rebuilt<'p>(
    old: &SourceRoot,
    gone: &hashbrown::HashSet<FileId, FixedState, &Frame>,
    added: impl Iterator<Item = &'p (FileId, SourceRootId, VfsPath)>,
) -> (SourceRoot, bool) {
    let mut files = FileSet::default();
    for id in old.iter() {
        if gone.contains(&id) {
            continue;
        }
        if let Some(path) = old.path_for_file(&id) {
            // clone: the set made again, as above.
            files.insert(id, path.clone());
        }
    }
    for (id, _, path) in added {
        // clone: an added file's path in the set.
        files.insert(*id, path.clone());
    }
    for (alias, of) in old.aliases() {
        if !gone.contains(&of) {
            files.alias(alias, of);
        }
    }
    let mut ids = 0;
    let mut named_back = 0;
    for id in files.iter() {
        ids += 1;
        named_back += usize::from(files.path_for_file(&id).and_then(|path| files.file_for_path(path)) == Some(&id));
    }
    let one_to_one = named_back == ids && ids == files.len();
    let root = match old.is_library {
        true => SourceRoot::new_library(files),
        false => SourceRoot::new_local(files),
    };
    (root, one_to_one)
}

/// A path as blaze-vfs takes one.
fn check_path(path: &str) -> Result<(), ViewError> {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.split('/').any(|part| part.is_empty() || part == "." || part == "..");
    // alloc: the refusal's message (an error path).
    if bad { Err(ViewError::BadPath(path.to_owned())) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::under;

    #[test]
    fn under_is_by_whole_parts() {
        assert!(under("target/debug/a.rs", "target"));
        assert!(under("target", "target"));
        assert!(!under("targets/a.rs", "target"));
        assert!(under("anything", ""));
    }
}
