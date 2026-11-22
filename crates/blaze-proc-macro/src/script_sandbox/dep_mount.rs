//! A build script's read-only view of its `links` dependencies (brief S1, decision D96 (5)).
//!
//! Cargo hands a script `DEP_<LINKS>_<KEY>` for each `cargo::metadata` pair of a direct
//! dependency with `links` (the cargo book, "The links Manifest Key"), and those values are
//! often paths: libusb1-sys prints `include=$OUT_DIR/include`, which rusb's script then reads;
//! zstd-sys prints `include=/package/zstd/lib`, the tree IT was served. What the reader sees of
//! each path is planned by `blaze_build_script::dep_paths::plan` (sans-IO, its one home):
//!
//! - a path under the producer's `OUT_DIR` keeps its text, and the reader's sandbox serves the
//!   producer's `OUT_DIR` files (its outputs record's files) there, read-only;
//! - a path under the producer's served tree (`/package` or `/workspace`) is renamed to
//!   `/deps/<links>/..`, and that tree is served there, read-only: the reader's own `/package`
//!   is ANOTHER tree, so the text alone named the wrong package's file (rule 21);
//! - anything else is refused by name ([`DepViewError`]), never passed on (rule 3).
//!
//! A write anywhere under a mount stops the script ([`Refusal::Path`], naming the dependency).
//!
//! Keys (rule 21). The renamed values are in the environment, so in the run's key. What the
//! mounts serve is not text of the reader, so its content is put in the key too
//! ([`DepViews::key`], computed before the run from the producers' records): every file of a
//! producer's `OUT_DIR`, and every file of a producer's fetched tree. The one tree that is not
//! hashed is a member producer's workspace folder, which is the reader's own folder by
//! construction (a member reader in the same workspace; any other reader is refused): its reads
//! are recorded in the reader's read list under the same subjects as the reader's own reads,
//! and [`super::answer_now`] answers them in the reader's tree.
//!
//! Floor: a dependent's key costs one pass over its producers' served bytes (a few MB for
//! zstd-sys) per run or lookup, against reading them anyway when it runs; nothing is copied:
//! the trees and `OUT_DIR` files are shared (`Arc`) with the producers' runs.

use core::fmt;
use std::sync::Arc;

use blaze_build_script::Outputs;
use blaze_build_script::dep_paths::{self, DEPS_ROOT, DepMount, DepPathError, Producer, Sight};
use blaze_build_script::env::DepMetadata;
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AString, AVec, Alloc};
use blaze_wasi::{Answer, Errno, Filestat, Filetype, Open, Stop};

use super::{Node, Opened, Place, Refusal, ScriptRoot, ScriptSandbox, Tree, filestat};

/// A native script's view: the same values laid as real read-only folders (D112, brief N8).
pub mod native;

/// One `links` dependency's script as its dependents' scripts are given it: the tree it was
/// served, what it saw as its root, and its outputs' `OUT_DIR` and files.
#[derive(Clone, Debug)]
pub struct DepProducer {
    pub tree: Tree,
    pub root: ScriptRoot,
    /// Its `OUT_DIR` as its outputs record holds it.
    pub out_dir: String,
    /// The files it left in `OUT_DIR`, by path from it, sorted (its outputs record's files).
    pub out: Arc<Vec<(String, Vec<u8>)>>,
    /// The names of the files its outputs record names and does not hold (a native script's,
    /// in the home's file area), sorted: its `OUT_DIR` is not served to wasm scripts while any is
    /// ([`DepViewError::StoredOut`]); native scripts materialize them from the file area.
    pub stored: Arc<Vec<String>>,
    /// The `(name, sha256)` pairs of stored files in the home's file area, sorted by name.
    pub stored_files: Arc<Vec<(String, [u8; 32])>>,
    /// The scripts upstream of it (its transitive `links_inputs` closure), each with its
    /// `links`, once, sorted by `links` (brief S1b): its metadata may forward their paths
    /// (aws-lc-rs re-emits aws-lc-sys's `include`), which its readers are then served too.
    pub upstream: Arc<Vec<(String, DepProducer)>>,
}

impl DepProducer {
    /// The producer of a run that gave `outputs`, over the `tree` it was served at `root`,
    /// with nothing upstream ([`DepProducer::with_upstream`] adds it).
    pub fn of(tree: Tree, root: ScriptRoot, outputs: &Outputs<'_>) -> DepProducer {
        let mut out: Vec<(String, Vec<u8>)> = outputs.files.iter().map(|file| (file.name.as_str().to_owned(), file.bytes.to_vec())).collect();
        // `Outputs::add_file` keeps them sorted; sorted here too, so `Tree::Mem`'s search holds.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        let stored: Vec<String> = outputs.stored.iter().map(|file| file.name.as_str().to_owned()).collect();
        let mut stored_files: Vec<(String, [u8; 32])> = outputs.stored.iter().map(|file| (file.name.as_str().to_owned(), file.sha256)).collect();
        stored_files.sort_by(|a, b| a.0.cmp(&b.0));
        DepProducer {
            tree,
            root,
            out_dir: outputs.out_dir.as_str().to_owned(),
            out: Arc::new(out),
            stored: Arc::new(stored),
            stored_files: Arc::new(stored_files),
            upstream: Arc::new(Vec::new()),
        }
    }

    /// The producer of a run whose outputs RECORD is `outputs` (read in place: what a reader of
    /// the store, or of a run kept as its record, is handed), over the `tree` it was served at
    /// `root`, with nothing upstream. Equal to [`DepProducer::of`] of the outputs the record
    /// was encoded from: its inline files copied, its stored files named and never read.
    pub fn of_record(tree: Tree, root: ScriptRoot, outputs: &blaze_build_script::OutputsRef<'_>) -> DepProducer {
        let mut out: Vec<(String, Vec<u8>)> = outputs.files().map(|(path, bytes)| (path.to_owned(), bytes.to_vec())).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        let stored: Vec<String> = outputs.stored().map(|file| file.name.to_owned()).collect();
        let mut stored_files: Vec<(String, [u8; 32])> = outputs.stored().map(|file| (file.name.to_owned(), *file.sha256)).collect();
        stored_files.sort_by(|a, b| a.0.cmp(&b.0));
        DepProducer {
            tree,
            root,
            out_dir: outputs.out_dir().to_owned(),
            out: Arc::new(out),
            stored: Arc::new(stored),
            stored_files: Arc::new(stored_files),
            upstream: Arc::new(Vec::new()),
        }
    }

    /// This producer with `upstream`, its closure ([`DepProducer::closure`] of its inputs).
    pub fn with_upstream(mut self, upstream: Vec<(String, DepProducer)>) -> DepProducer {
        self.upstream = Arc::new(upstream);
        self
    }

    /// The transitive `links_inputs` closure above a script whose direct `links` producers are
    /// `inputs`: each input and every script upstream of it, once by `links`, sorted by it.
    /// Each input already holds its own closure, so this is one flatten (no recursion, no walk
    /// of the graph); the clones share their files (`Arc`).
    pub fn closure(inputs: &[(&str, &DepProducer)]) -> Vec<(String, DepProducer)> {
        let mut out: Vec<(String, DepProducer)> = Vec::new();
        for &(links, producer) in inputs {
            out.push((links.to_owned(), producer.clone()));
            out.extend(producer.upstream.iter().cloned());
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.dedup_by(|a, b| a.0 == b.0);
        out
    }
}

/// What a mount serves.
#[derive(Clone, Debug)]
enum Served {
    /// Files whose content is in the reader's key: a producer's `OUT_DIR`, or its fetched tree.
    Keyed(Arc<Vec<(String, Vec<u8>)>>),
    /// Native producer's `OUT_DIR` with inline files and content-addressed stored files.
    Stored {
        inline: Arc<Vec<(String, Vec<u8>)>>,
        stored: Arc<Vec<(String, [u8; 32])>>,
    },
    /// The reader's own tree (a member producer of the reader's workspace): reads recorded.
    ReaderTree,
}

/// One read-only mount of a reader's sandbox.
#[derive(Clone, Debug)]
struct Mounted {
    /// Where the reader sees it, as parts from `/`.
    parts: Vec<String>,
    /// The producer's `links`, for errors.
    links: String,
    served: Served,
}

impl Mounted {
    fn at(&self) -> String {
        let mut at = String::new();
        for part in &self.parts {
            at.push('/');
            at.push_str(part);
        }
        at
    }

    fn is_under(&self, parts: &[&str]) -> bool {
        parts.len() < self.parts.len() && self.parts.iter().zip(parts).all(|(a, b)| a == b)
    }
}

/// The read-only mounts of one reader's sandbox, and the digest of what they serve.
#[derive(Clone, Debug, Default)]
pub struct DepViews {
    /// Sorted by where they are seen.
    mounts: Vec<Mounted>,
    /// `None` when nothing is mounted.
    content: Option<ContentId>,
}

impl DepViews {
    /// No mounts: a script with no `links` dependency, or a caller that gives none.
    pub fn none() -> DepViews {
        DepViews::default()
    }

    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }

    /// The run's key `base` with what the mounts serve: `base` itself when nothing is mounted,
    /// so the keys of scripts with no `links` dependency stand.
    pub fn key(&self, base: ContentId) -> ContentId {
        match &self.content {
            None => base,
            Some(content) => Hasher::new("blaze build-script links mounts 1").id(&base).id(content).finish(),
        }
    }

    /// Where `parts` is, if a mount or an ancestor of one holds it; `own_out` is the reader's
    /// `OUT_DIR` as parts, listed beside the mounts in a shared ancestor. `None`: neither (the
    /// sandbox's other places answer, as with no mounts).
    pub(super) fn place(&self, parts: &[&str], own_out: &[&str]) -> Option<Place> {
        for (at, mount) in self.mounts.iter().enumerate() {
            let len = mount.parts.len();
            if parts.len() >= len && mount.parts.iter().zip(parts).all(|(a, b)| a == b) {
                return Some(Place::Dep { mount: at, rel: parts[len..].join("/") });
            }
        }
        if !self.mounts.iter().any(|mount| mount.is_under(parts)) {
            return None;
        }
        let mut entries: Vec<(Vec<u8>, Filetype)> = self.mounts.iter().filter(|m| m.is_under(parts)).map(|m| (m.parts[parts.len()].as_bytes().to_vec(), Filetype::Directory)).collect();
        if parts.len() < own_out.len() && own_out[..parts.len()] == parts[..] {
            entries.push((own_out[parts.len()].as_bytes().to_vec(), Filetype::Directory));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.dedup_by(|a, b| a.0 == b.0);
        Some(Place::Ancestor(entries))
    }
}

/// A `links` dependency's metadata the reading script cannot be given, by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepViewError {
    /// An absolute path under neither the producer's `OUT_DIR` nor its served tree.
    Outside { links: String, key: String, path: String },
    /// A pair whose `links` names no producer given.
    NoProducer { links: String, key: String },
    /// A path that climbs out (`..`) of the tree it starts in.
    Climbs { links: String, key: String, path: String },
    /// A member producer's workspace, for a reader not served that same workspace folder.
    OtherWorkspace { links: String },
    /// A path under the producer's `OUT_DIR`, whose files include one its record names and does
    /// not hold (a native script's, in the home's file area): serving that folder from the
    /// file area is not built, and it is never served without the file (rule 3).
    StoredOut { links: String, file: String },
}

impl From<DepPathError<'_>> for DepViewError {
    fn from(error: DepPathError<'_>) -> DepViewError {
        match error {
            DepPathError::Outside { links, key, path } => DepViewError::Outside { links: links.to_owned(), key: key.to_owned(), path: path.to_owned() },
            DepPathError::NoProducer { links, key } => DepViewError::NoProducer { links: links.to_owned(), key: key.to_owned() },
            DepPathError::Climbs { links, key, path } => DepViewError::Climbs { links: links.to_owned(), key: key.to_owned(), path: path.to_owned() },
        }
    }
}

impl fmt::Display for DepViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DepViewError::Outside { links, key, path } => write!(
                f,
                "DEP_{links}_{key} names `{path}`, outside the `links = \"{links}\"` package's OUT_DIR and its tree: a path of the machine is never given to a build script (decision D96 (5))"
            ),
            DepViewError::NoProducer { links, key } => write!(f, "DEP_{links}_{key}: the `links = \"{links}\"` package's run was not given, so its paths cannot be served (decision D96 (5))"),
            DepViewError::Climbs { links, key, path } => write!(f, "DEP_{links}_{key} names `{path}`, which climbs (`..`) out of the tree it is under (decision D96 (5))"),
            DepViewError::OtherWorkspace { links } => write!(
                f,
                "the `links = \"{links}\"` package is a workspace member whose paths name its workspace, and this script is not served that workspace (decision D96 (5))"
            ),
            DepViewError::StoredOut { links, file } => write!(
                f,
                "a `links = \"{links}\"` path names that package's OUT_DIR, whose file `{file}` is a stored file of its record (in the home's file area, not held in the record): serving a stored file to a dependent's build script is not built, so it is refused (rule 3)"
            ),
        }
    }
}

impl core::error::Error for DepViewError {}

/// What a reading script is given of its `links` dependencies: each pair's value as it sees it
/// (in the order of the `deps` given) and its mounts.
#[derive(Debug)]
pub struct Viewed<'a> {
    pub values: AVec<'a, AString<'a>>,
    pub views: DepViews,
}

/// Plans `deps` (the reader's metadata pairs) against `producers` (each with its `links`) and
/// makes the mounts the plan names; `reader` is the tree the reading script is served. The
/// wasm sight of [`resolve`].
pub fn view<'a>(reader: &Tree, producers: &[(&str, &DepProducer)], deps: &[DepMetadata<'_>], alloc: Alloc<'a>) -> Result<Viewed<'a>, DepViewError> {
    view_inner(reader, producers, deps, false, alloc)
}

/// Like [`view`], but permits native producers whose `OUT_DIR` files are stored in the
/// home's content-addressed file area (keyed by `(name, sha256)`).
pub fn view_with_stored<'a>(reader: &Tree, producers: &[(&str, &DepProducer)], deps: &[DepMetadata<'_>], alloc: Alloc<'a>) -> Result<Viewed<'a>, DepViewError> {
    view_inner(reader, producers, deps, true, alloc)
}

fn view_inner<'a>(reader: &Tree, producers: &[(&str, &DepProducer)], deps: &[DepMetadata<'_>], allow_stored: bool, alloc: Alloc<'a>) -> Result<Viewed<'a>, DepViewError> {
    let resolved = resolve(Sight::Wasm, producers, deps, alloc)?;
    let mut mounts: Vec<Mounted> = Vec::new();
    for &(mount, producer) in resolved.serve.iter() {
        mounts.push(mounted(mount, producer, reader, allow_stored)?);
    }
    mounts.sort_by(|a, b| a.parts.cmp(&b.parts));
    // One mount serves every pair under it (two pairs under one `OUT_DIR`, say).
    mounts.dedup_by(|a, b| a.parts == b.parts);
    let content = (!mounts.is_empty()).then(|| content_id(&mounts));
    Ok(Viewed { values: resolved.values, views: DepViews { mounts, content } })
}

/// A reader's `links` values resolved for a run of one [`Sight`], and the trees they are under.
pub(crate) struct Resolved<'p, 'a> {
    /// Each pair's value as that run sees it, in the order of the `deps` given.
    pub(crate) values: AVec<'a, AString<'a>>,
    /// Each tree a value is under, with the producer that serves it, in the order met (one
    /// tree may be named twice; the callers sort and keep each once).
    pub(crate) serve: Vec<(DepMount<'p>, &'p DepProducer)>,
}

/// THE one resolution of a reader's `DEP_*` values (D112, brief N8): each value's tree (a
/// producer's `OUT_DIR` tree or its served tree) and its place in it, written where a run of
/// `sight` sees that tree. Used by the wasm sandbox ([`view`]) and the native run
/// ([`native::lay`]) alike; a value under neither tree of any candidate is refused by name.
pub(crate) fn resolve<'p, 'a>(sight: Sight<'_>, producers: &[(&'p str, &'p DepProducer)], deps: &[DepMetadata<'p>], alloc: Alloc<'a>) -> Result<Resolved<'p, 'a>, DepViewError> {
    let mut values: AVec<'a, AString<'a>> = AVec::with_capacity_in(deps.len(), alloc);
    let mut serve: Vec<(DepMount<'p>, &'p DepProducer)> = Vec::new();
    for dep in deps {
        let direct = find(producers, dep.links).map_err(|_| DepViewError::NoProducer { links: dep.links.to_owned(), key: dep.key.to_owned() })?;
        // The direct producer first, then each script upstream of it in `links` order (brief
        // S1b: a producer may forward an upstream script's paths): the first whose `OUT_DIR`
        // or tree holds the value serves it. Under none: `Outside`, naming this pair.
        let candidates = core::iter::once((dep.links, direct)).chain(direct.upstream.iter().map(|(links, producer)| (links.as_str(), producer)));
        let mut planned = None;
        for (links, producer) in candidates {
            let one = [Producer { links, seen: producer.root.seen(), out_dir: producer.out_dir.as_str() }];
            let pair = [DepMetadata { links, key: dep.key, value: dep.value }];
            match dep_paths::plan_seen(sight, &one, &pair, alloc) {
                Ok(plan) => {
                    planned = Some((plan, producer));
                    break;
                }
                Err(DepPathError::Outside { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let Some((plan, producer)) = planned else {
            return Err(DepViewError::Outside { links: dep.links.to_owned(), key: dep.key.to_owned(), path: dep.value.to_owned() });
        };
        for mount in plan.mounts.iter() {
            serve.push((*mount, producer));
        }
        let mut value = AString::new_in(alloc);
        if let Some(seen) = plan.values.first() {
            value.push_str(seen.as_str());
        }
        values.push(value);
    }
    Ok(Resolved { values, serve })
}

/// The mount `mount` of a plan made against `producer`, as `reader`'s sandbox serves it.
fn mounted(mount: DepMount<'_>, producer: &DepProducer, reader: &Tree, allow_stored: bool) -> Result<Mounted, DepViewError> {
    let (links, at, served) = match mount {
        DepMount::Out { links, at } => {
            if let Some(file) = producer.stored.first() {
                if !allow_stored {
                    return Err(DepViewError::StoredOut { links: links.to_owned(), file: file.clone() });
                }
                (links, at.to_owned(), Served::Stored { inline: Arc::clone(&producer.out), stored: Arc::clone(&producer.stored_files) })
            } else {
                (links, at.to_owned(), Served::Keyed(Arc::clone(&producer.out)))
            }
        }
        DepMount::Tree { links } => {
            let served = match (&producer.tree, reader) {
                (Tree::Mem { files, .. }, _) => Served::Keyed(Arc::clone(files)),
                (Tree::Dir(theirs), Tree::Dir(ours)) if theirs == ours => Served::ReaderTree,
                (Tree::Dir(_), _) => return Err(DepViewError::OtherWorkspace { links: links.to_owned() }),
            };
            (links, format!("{DEPS_ROOT}/{links}"), served)
        }
    };
    let parts = at.split('/').filter(|part| !part.is_empty()).map(str::to_owned).collect();
    Ok(Mounted { parts, links: links.to_owned(), served })
}

fn find<'p>(producers: &[(&str, &'p DepProducer)], links: &str) -> Result<&'p DepProducer, DepViewError> {
    producers.iter().find(|(name, _)| *name == links).map(|(_, producer)| *producer).ok_or_else(|| DepViewError::NoProducer { links: links.to_owned(), key: String::new() })
}

/// The digest of what `mounts` serve: each mount's place and, when keyed, every file's path
/// and bytes, in sorted order.
fn content_id(mounts: &[Mounted]) -> ContentId {
    let mut hasher = Hasher::new("blaze build-script links mount content 1");
    hasher.update(&(mounts.len() as u64).to_le_bytes());
    for mount in mounts {
        hasher.update(&(mount.parts.len() as u64).to_le_bytes());
        for part in &mount.parts {
            hasher.part(part.as_bytes());
        }
        match &mount.served {
            Served::Keyed(files) => {
                hasher.update(&[1]).update(&(files.len() as u64).to_le_bytes());
                for (path, bytes) in files.iter() {
                    hasher.part(path.as_bytes()).part(bytes);
                }
            }
            Served::Stored { inline, stored } => {
                hasher.update(&[3]).update(&(inline.len() as u64).to_le_bytes());
                for (path, bytes) in inline.iter() {
                    hasher.part(path.as_bytes()).part(bytes);
                }
                hasher.update(&(stored.len() as u64).to_le_bytes());
                for (path, digest) in stored.iter() {
                    hasher.part(path.as_bytes()).update(digest);
                }
            }
            // The reader's own tree: its reads are in the reader's read list.
            Served::ReaderTree => {
                hasher.update(&[2]);
            }
        }
    }
    hasher.finish()
}

/// A node found under a mount, owned.
fn owned(node: Node<'_>) -> Node<'static> {
    match node {
        Node::File(bytes) => Node::File(std::borrow::Cow::Owned(bytes.into_owned())),
        Node::Folder(entries) => Node::Folder(entries),
    }
}

impl ScriptSandbox {
    /// The stop for a write under the mount `mount` at `rel`.
    pub(super) fn dep_write(&mut self, mount: usize, rel: &str) -> Stop {
        let mounted = &self.script.deps.mounts[mount];
        let (at, links) = (mounted.at(), mounted.links.clone());
        let path = if rel.is_empty() { at } else { format!("{at}/{rel}") };
        self.refuse(Refusal::Path(format!(
            "wrote {path}, a file of the `links = \"{links}\"` dependency served read-only (its DEP_* paths), outside OUT_DIR, the only folder a build script may write"
        )))
    }

    /// Serve the operation through the mount whose grant the script used.
    fn dep_node(&mut self, mount: usize, rel: String, op: super::NodeRead) -> Result<Option<Node<'static>>, Stop> {
        let script = Arc::clone(&self.script);
        match &script.deps.mounts[mount].served {
            Served::Keyed(files) | Served::Stored { inline: files, .. } => {
                let tree = Tree::Mem { files: Arc::clone(files), git: None };
                match tree.node(&rel) {
                    Ok(found) => Ok(found.map(owned)),
                    Err(why) => Err(self.refuse(Refusal::Path(why.to_string()))),
                }
            }
            Served::ReaderTree => {
                let found = match script.tree.node(&rel) {
                    Ok(found) => found.map(owned),
                    Err(why) => return Err(self.refuse(Refusal::Path(why.to_string()))),
                };
                // The same subjects as the reader's own reads of its tree: the same folder.
                self.record_node(rel, op, found.as_ref());
                Ok(found)
            }
        }
    }

    /// `path_open` under a mount: read-only; `path` is the whole path, for a folder's entries.
    pub(super) fn dep_open(&mut self, mount: usize, rel: String, path: &[u8], how: Open) -> Answer<u32> {
        if how.create() || how.truncate() || how.write() || how.append() {
            return Err(self.dep_write(mount, &rel));
        }
        let opened = match self.dep_node(mount, rel, super::NodeRead::open(&how))? {
            None => return Ok(Err(Errno::NOENT)),
            Some(Node::Folder(entries)) => Opened::Ro { bytes: Vec::new(), pos: 0, entries: Some(entries), folder: path.to_vec() },
            Some(Node::File(_)) if how.directory() => return Ok(Err(Errno::NOTDIR)),
            Some(Node::File(bytes)) => Opened::Ro { bytes: bytes.into_owned(), pos: 0, entries: None, folder: Vec::new() },
        };
        Ok(Ok(self.slot(opened)))
    }

    /// `path_filestat_get` under a mount.
    pub(super) fn dep_stat(&mut self, mount: usize, rel: String) -> Answer<Filestat> {
        Ok(match self.dep_node(mount, rel, super::NodeRead::Stat)? {
            Some(Node::File(bytes)) => Ok(filestat(Filetype::RegularFile, bytes.len() as u64)),
            Some(Node::Folder(_)) => Ok(filestat(Filetype::Directory, 0)),
            None => Err(Errno::NOENT),
        })
    }

    /// A file's bytes under a mount, for a spawn's snippet.
    pub(super) fn dep_file(&mut self, mount: usize, rel: String) -> Result<Option<Vec<u8>>, Stop> {
        Ok(match self.dep_node(mount, rel, super::NodeRead::Bytes)? {
            Some(Node::File(bytes)) => Some(bytes.into_owned()),
            _ => None,
        })
    }

    /// `path_open` of an ancestor of a mount: a folder of its `entries`, never written.
    pub(super) fn ancestor_open(&mut self, entries: Vec<(Vec<u8>, Filetype)>, path: &[u8], writes: bool) -> Answer<u32> {
        if writes {
            let shown = String::from_utf8_lossy(path).into_owned();
            return Err(self.refuse(Refusal::Path(format!("wrote {shown}, a folder above the read-only `links` dependencies' files and OUT_DIR, outside OUT_DIR"))));
        }
        Ok(Ok(self.slot(Opened::Ro { bytes: Vec::new(), pos: 0, entries: Some(entries), folder: path.to_vec() })))
    }
}

#[cfg(test)]
mod tests {
    use blaze_build_script::key::{CMode, RunLimits};
    use blaze_wasi::{ROOT_FD, System};

    use super::*;
    use crate::script_sandbox::Script;

    const READER_OUT: &str = "/target/debug/build/reader-0000000000000000/out";
    const SYS_OUT: &str = "/target/debug/build/sys-1111111111111111/out";

    fn files(list: &[(&str, &[u8])]) -> Arc<Vec<(String, Vec<u8>)>> {
        let mut out: Vec<(String, Vec<u8>)> = list.iter().map(|(path, bytes)| ((*path).to_owned(), bytes.to_vec())).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Arc::new(out)
    }

    /// The `links = "x"` producer: a fetched package (served at `/package` when it ran) with
    /// `zstd/lib/zstd.h`, which left `include/a.h` in its `OUT_DIR`.
    fn producer(header: &[u8]) -> DepProducer {
        DepProducer {
            tree: Tree::Mem { files: files(&[("zstd/lib/zstd.h", b"the producer's zstd.h")]), git: None },
            root: ScriptRoot::package(),
            out_dir: SYS_OUT.to_owned(),
            out: files(&[("include/a.h", header)]),
            stored: Arc::new(Vec::new()),
            stored_files: Arc::new(Vec::new()),
            upstream: Arc::new(Vec::new()),
        }
    }

    /// The reading script: also a fetched package at `/package`, with its OWN `zstd/lib/zstd.h`.
    fn reader(views: DepViews, deps: &[(String, String)]) -> Arc<Script> {
        let mut env = vec![("OUT_DIR".to_owned(), READER_OUT.to_owned())];
        env.extend(deps.iter().cloned());
        env.sort();
        Arc::new(Script {
            package: "reader 0.0.0".to_owned(),
            tree: Tree::Mem { files: files(&[("zstd/lib/zstd.h", b"the reader's own zstd.h")]), git: None },
            root: ScriptRoot::package(),
            env,
            target: "aarch64-apple-darwin".to_owned(),
            host: "aarch64-apple-darwin".to_owned(),
            c_mode: CMode::Deferred,
            limits: RunLimits::STANDARD,
            table: Arc::new(Vec::new()),
            probes: None,
            spawner: None,
            step: ContentId::default(),
            spawn_setting: Arc::new(crate::spawn_route::SpawnSetting::off()),
            deps: Arc::new(views),
        })
    }

    const READ: Open = Open { dirflags: 0, oflags: 0, rights_base: 1 << 1, rights_inheriting: 0, fdflags: 0 };
    const CREATE: Open = Open { dirflags: 0, oflags: 1 | 8, rights_base: 1 << 6, rights_inheriting: 0, fdflags: 0 };

    fn read_whole(sandbox: &mut ScriptSandbox, path: &str) -> Result<Vec<u8>, Errno> {
        let fd = match sandbox.open(ROOT_FD, path.as_bytes(), READ) {
            Ok(Ok(fd)) => fd,
            Ok(Err(errno)) => return Err(errno),
            Err(stop) => panic!("{path}: stopped: {}", stop.0),
        };
        let mut buf = vec![0u8; 256];
        let n = match sandbox.read(fd, &mut buf) {
            Ok(Ok(n)) => n,
            Ok(Err(errno)) => return Err(errno),
            Err(stop) => panic!("{path}: stopped: {}", stop.0),
        };
        buf.truncate(n);
        Ok(buf)
    }

    #[test]
    fn member_mount_records_the_served_operation_and_a_fetched_reader_has_no_workspace_grant() {
        let dir = std::env::temp_dir().join(format!("blaze-script-member-mount-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::create_dir(dir.join("producer")).unwrap();
        std::fs::write(dir.join("producer/header.h"), b"header").unwrap();
        let mut made = producer(b"out");
        made.tree = Tree::Dir(crate::script_sandbox::RealRoot::resolve(&dir).expect("the fixture's folder resolves"));
        made.root = ScriptRoot::member("producer").unwrap();
        let pairs = [DepMetadata { links: "sys", key: "include", value: "/workspace/producer" }];
        let alloc = blaze_own::testing::heap().alloc();
        let fetched = reader(DepViews::none(), &[]);
        assert!(matches!(view(&fetched.tree, &[("sys", &made)], &pairs, alloc), Err(DepViewError::OtherWorkspace { .. })));

        let viewed = view(&made.tree, &[("sys", &made)], &pairs, alloc).unwrap();
        let mut member = reader(viewed.views, &[]);
        let script = Arc::make_mut(&mut member);
        script.tree = made.tree.clone();
        script.root = ScriptRoot::member("consumer").unwrap();
        let mut sandbox = ScriptSandbox::new(member);
        assert_eq!(read_whole(&mut sandbox, "/deps/sys/producer/header.h"), Ok(b"header".to_vec()));
        assert_eq!(sandbox.reads[0].subject, "file:producer/header.h");
        assert_eq!(sandbox.reads[0].digest, Some(crate::script_sandbox::file_answer(b"header")));
        let directory = Open { oflags: 2, ..READ };
        for mount in ["/deps/sys", "/workspace"] {
            let path = format!("{mount}/producer/header.h");
            assert_eq!(sandbox.open(ROOT_FD, path.as_bytes(), directory).unwrap(), Err(Errno::NOTDIR));
            let path = format!("{mount}/producer/missing");
            assert_eq!(sandbox.open(ROOT_FD, path.as_bytes(), directory).unwrap(), Err(Errno::NOENT));
        }
        assert_eq!(sandbox.reads.len(), 3, "both grants resolve to the same checkout observations");
        for (read, expected) in sandbox.reads[1..].iter().zip([Some(super::super::NOT_A_FOLDER), None]) {
            assert_eq!(read.digest, expected);
            assert_eq!(super::super::answer_now(&made.tree, &read.subject).unwrap(), expected);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_supplied_environment_distinguishes_absence_from_an_empty_value() {
        let alloc = blaze_own::testing::heap().alloc();
        let encoded = |empty: bool| {
            let mut env = blaze_build_script::Env { vars: AVec::new_in(alloc) };
            if empty {
                env.vars.push((AString::from_str_in("OPTION", alloc), AString::new_in(alloc)));
            }
            assert_eq!(env.get("OPTION"), empty.then_some(""));
            let mut key = AVec::new_in(alloc);
            env.encode(&mut key);
            let mut script = reader(DepViews::none(), &[]);
            Arc::make_mut(&mut script).env = super::super::owned_env(&env, alloc);
            let mut run = ScriptSandbox::new(script);
            let (bytes, count) = run.environ().unwrap();
            assert_eq!(count, 1 + usize::from(empty)); // NUM_JOBS, and possibly OPTION
            assert_eq!(bytes.split(|&b| b == 0).any(|pair| pair == b"OPTION="), empty);
            key
        };
        assert_ne!(encoded(false), encoded(true), "enumeration exposes the whole keyed environment");
    }

    /// The cargo book, "The links Manifest Key": a `links` package's `cargo::metadata` pairs
    /// reach its direct dependents' scripts as `DEP_<LINKS>_<KEY>`; rusb reads
    /// `$DEP_USB_1.0_INCLUDE/libusb.h`, a file libusb1-sys's script left in its `OUT_DIR`, and
    /// zstd-sys's `include=/package/zstd/lib` names zstd-sys's tree, not its reader's. So: the
    /// producer's header is read through its `OUT_DIR` path; a write there stops the script by
    /// name; the producer's `/package/zstd/lib` reaches the reader as `/deps/x/zstd/lib` and
    /// reads the PRODUCER's file, while the reader's `/package/zstd/lib/zstd.h` stays its own;
    /// the producer's files are in the reader's key. A wrong fix fails here: refusing the
    /// `OUT_DIR` read (today's bug), passing `/package/zstd/lib` on as written (it reads
    /// "the reader's own zstd.h"), serving the mount writable, or leaving the content out of
    /// the key. Expected line:
    /// `test script_sandbox::dep_mount::tests::a_links_dependency_s_files_are_read_only_and_its_package_is_not_the_reader_s ... ok`.
    #[test]
    fn a_links_dependency_s_files_are_read_only_and_its_package_is_not_the_reader_s() {
        let alloc = blaze_own::testing::heap().alloc();
        let made = producer(b"#define A 1\n");
        let include = format!("{SYS_OUT}/include");
        let deps = [DepMetadata { links: "x", key: "include", value: include.as_str() }, DepMetadata { links: "x", key: "root", value: "/package/zstd/lib" }];
        let reader_tree = Tree::Mem { files: files(&[("zstd/lib/zstd.h", b"the reader's own zstd.h")]), git: None };
        let viewed = view(&reader_tree, &[("x", &made)], &deps, alloc).unwrap_or_else(|e| panic!("{e}"));
        let values: Vec<&str> = viewed.values.iter().map(|v| v.as_str()).collect();
        assert_eq!(values, [include.as_str(), "/deps/x/zstd/lib"], "the producer's own mount is renamed for the reader");
        let env = vec![("DEP_X_INCLUDE".to_owned(), values[0].to_owned()), ("DEP_X_ROOT".to_owned(), values[1].to_owned())];
        let views = viewed.views;
        let base = ContentId([7; 32]);
        let script = reader(views.clone(), &env);
        let mut sandbox = ScriptSandbox::new(Arc::clone(&script));

        // Read: the producer's OUT_DIR file, through the DEP_ path.
        assert_eq!(read_whole(&mut sandbox, &format!("{}/a.h", values[0])), Ok(b"#define A 1\n".to_vec()));
        assert_eq!(read_whole(&mut sandbox, &format!("{SYS_OUT}/include/none.h")), Err(Errno::NOENT));
        // The producer's tree, at its renamed path; the reader's own `/package` is its own.
        assert_eq!(read_whole(&mut sandbox, &format!("{}/zstd.h", values[1])), Ok(b"the producer's zstd.h".to_vec()));
        assert_eq!(read_whole(&mut sandbox, "/package/zstd/lib/zstd.h"), Ok(b"the reader's own zstd.h".to_vec()));
        // A shared ancestor lists the reader's OUT_DIR folder and the producer's.
        let listed = match sandbox.open(ROOT_FD, b"/target/debug/build", READ) {
            Ok(Ok(fd)) => fd,
            other => panic!("/target/debug/build: {other:?}"),
        };
        let mut names = Vec::new();
        let mut cookie = 0;
        while let Ok(Ok(Some(entry))) = sandbox.dir_entry(listed, cookie) {
            names.push(String::from_utf8_lossy(entry.name).into_owned());
            cookie += 1;
        }
        assert_eq!(names, ["reader-0000000000000000", "sys-1111111111111111"]);
        assert!(sandbox.refused.is_none(), "nothing read was refused: {:?}", sandbox.refused);

        // Write: refused by name, the script stopped.
        let wrote = sandbox.open(ROOT_FD, format!("{SYS_OUT}/include/a.h").as_bytes(), CREATE);
        assert!(wrote.is_err(), "a write into a dependency's OUT_DIR stops the script");
        match &sandbox.refused {
            Some(Refusal::Path(why)) => assert!(why.contains("`links = \"x\"`") && why.contains("read-only"), "{why}"),
            other => panic!("refused {other:?}"),
        }

        // Keys: the same producer files give the same key; a changed header another; no
        // mounts leave the base as it was.
        let again = view(&reader_tree, &[("x", &producer(b"#define A 1\n"))], &deps, alloc).unwrap_or_else(|e| panic!("{e}"));
        let changed = view(&reader_tree, &[("x", &producer(b"#define A 2\n"))], &deps, alloc).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(again.views.key(base), views.key(base));
        assert_ne!(changed.views.key(base), views.key(base), "a dependency's changed OUT_DIR file changes its reader's key");
        assert_ne!(views.key(base), base);
        assert_eq!(DepViews::none().key(base), base);

        // A path of the machine is refused by name, never served.
        let system = [DepMetadata { links: "x", key: "include", value: "/usr/include" }];
        assert!(matches!(view(&reader_tree, &[("x", &made)], &system, alloc), Err(DepViewError::Outside { .. })));
    }

    /// Brief S1b: a producer's metadata may forward the paths of a script upstream of it.
    /// aws-lc-rs 1.19.0's build.rs re-emits aws-lc-sys's `include`
    /// (`/target/debug/build/aws-lc-sys-../out/include`) under its own `links`. The test has
    /// three scripts: `a` (links `a`) leaves `include/x.h` in its `OUT_DIR`; `b` (links `b`,
    /// whose input is `a`) prints `include=$DEP_A_INCLUDE`; `c`, reading `b`, opens
    /// `$DEP_B_INCLUDE/x.h` and gets `a`'s file, read-only, with `a`'s files in its key. A path
    /// under an unrelated script's `OUT_DIR` is still `Outside`, naming `b`'s pair. Only the
    /// direct producer tried (today's census, evm2 and rustls "not run") fails the first
    /// unwrap. Expected line:
    /// `test script_sandbox::dep_mount::tests::a_forwarded_upstream_path_is_served_and_an_unrelated_one_is_refused ... ok`.
    #[test]
    fn a_forwarded_upstream_path_is_served_and_an_unrelated_one_is_refused() {
        const A_OUT: &str = "/target/debug/build/a-2222222222222222/out";
        const B_OUT: &str = "/target/debug/build/b-3333333333333333/out";
        let alloc = blaze_own::testing::heap().alloc();
        let a = |header: &[u8]| DepProducer {
            tree: Tree::Mem { files: files(&[]), git: None },
            root: ScriptRoot::package(),
            out_dir: A_OUT.to_owned(),
            out: files(&[("include/x.h", header)]),
            stored: Arc::new(Vec::new()),
            stored_files: Arc::new(Vec::new()),
            upstream: Arc::new(Vec::new()),
        };
        let b_over = |a: &DepProducer| {
            DepProducer { tree: Tree::Mem { files: files(&[]), git: None }, root: ScriptRoot::package(), out_dir: B_OUT.to_owned(), out: files(&[]), stored: Arc::new(Vec::new()), stored_files: Arc::new(Vec::new()), upstream: Arc::new(Vec::new()) }
                .with_upstream(DepProducer::closure(&[("a", a)]))
        };
        let a_made = a(b"#define X 1\n");
        let b = b_over(&a_made);
        assert_eq!(b.upstream.iter().map(|(links, _)| links.as_str()).collect::<Vec<_>>(), ["a"], "b's closure holds a");

        let forwarded = format!("{A_OUT}/include");
        let deps = [DepMetadata { links: "b", key: "include", value: forwarded.as_str() }];
        let reader_tree = Tree::Mem { files: files(&[]), git: None };
        let viewed = view(&reader_tree, &[("b", &b)], &deps, alloc).unwrap_or_else(|e| panic!("a forwarded path: {e}"));
        let values: Vec<&str> = viewed.values.iter().map(|v| v.as_str()).collect();
        assert_eq!(values, [forwarded.as_str()], "an OUT_DIR path keeps its text");
        let env = vec![("DEP_B_INCLUDE".to_owned(), forwarded.clone())];
        let views = viewed.views;
        let mut sandbox = ScriptSandbox::new(reader(views.clone(), &env));
        assert_eq!(read_whole(&mut sandbox, &format!("{forwarded}/x.h")), Ok(b"#define X 1\n".to_vec()));
        // Still read-only.
        assert!(sandbox.open(ROOT_FD, format!("{forwarded}/x.h").as_bytes(), CREATE).is_err());
        assert!(matches!(&sandbox.refused, Some(Refusal::Path(why)) if why.contains("read-only")), "{:?}", sandbox.refused);
        // `a`'s files are in the reader's key.
        let base = ContentId([9; 32]);
        let changed = view(&reader_tree, &[("b", &b_over(&a(b"#define X 2\n")))], &deps, alloc).unwrap_or_else(|e| panic!("{e}"));
        assert_ne!(changed.views.key(base), views.key(base), "an upstream script's changed file changes the reader's key");

        // A path under an unrelated script's OUT_DIR is refused, naming the reader's pair.
        let unrelated = [DepMetadata { links: "b", key: "include", value: "/target/debug/build/other-4444444444444444/out/include" }];
        assert_eq!(
            view(&reader_tree, &[("b", &b)], &unrelated, alloc).map(|_| ()),
            Err(DepViewError::Outside { links: "b".to_owned(), key: "include".to_owned(), path: "/target/debug/build/other-4444444444444444/out/include".to_owned() })
        );
    }
}
