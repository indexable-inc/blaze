//! A NATIVE build script's `links` dependencies (D112, brief N8, main's ruling of 2026-11-16):
//! where a producer's output tree is seen is a fact of the CONSUMING run, never of a record.
//!
//! A producer's record names each path-valued metadata output (`cargo::metadata=include=..`,
//! `root=..`) under one of two trees: its `OUT_DIR` tree (its outputs record's files) or the
//! tree it was served (its package). What a value means is its tree and its place in it;
//! [`super::resolve`] is THE one function that turns it into the text a run is given, by the
//! run's [`Sight`] of those trees:
//!
//! - a wasm run ([`Sight::Wasm`], [`super::view`]) sees them at virtual mounts of its sandbox;
//! - a native run ([`Sight::Native`], [`lay`] here) sees them at REAL folders under a private
//!   root of its own (`<work>/<rel>`): `<root>/<links>/out` and `<root>/<links>/package`, the
//!   same relative places inside. [`lay`] writes exactly the producers' recorded files there
//!   before the script starts, and the run grants those folders READ-ONLY (they go in
//!   [`crate::native_script::NativeScript::reads`]; the only folder written stays `OUT_DIR`).
//!
//! A value under neither tree is refused, naming the variable and the producer
//! ([`DepViewError::Outside`]); nothing is passed through as written (rule 3).
//!
//! What the native script prints back that names a laid folder is written as the wasm sight
//! names it ([`Laid::shown`]), so its record holds no path of this run (rule 25), the same bytes
//! a wasm run of the key would keep.
//!
//! Deterministic: the values are a function of the producers' records and `root`; the folders
//! are sorted, each once; their files are the records' bytes, in their sorted order.
//!
//! Floor: the producers' files are written once per run that is not answered by its record
//! (one pass over bytes the run reads anyway); a record hit lays nothing. Cloning from the
//! store where the volume allows is the next step down (the producers' bytes are held in
//! memory here, `DepProducer::out`, not as store files).

use core::{fmt, time::Duration};
use std::path::Path;

use blaze_build_script::{
    dep_paths::{DepMount, Sight},
    env::DepMetadata,
};
use blaze_own::{AString, AVec, Alloc};
use bstd::{fs::Durable, mem::TASK_FRAME};

use super::{DepProducer, DepViewError, resolve};
use crate::{
    native_script::{listed_under, remove},
    record::native_world,
    script_sandbox::Tree,
};

/// What a native run is given of its `links` dependencies.
#[derive(Debug)]
pub struct Laid<'a> {
    /// Each pair's value as the native run sees it, in the order of the `deps` given.
    pub values: AVec<'a, AString<'a>>,
    /// The folders laid, sorted, each once: granted read-only, and nothing else of the
    /// producers is.
    pub folders: AVec<'a, AString<'a>>,
    /// Each laid folder with the text the wasm sight gives it, for the run's output: what the
    /// script prints under a laid folder is kept as the record's form.
    pub shown: AVec<'a, (AString<'a>, AString<'a>)>,
}

/// Why a native run's `links` dependencies could not be laid; each names the thing.
#[derive(Debug)]
pub enum LayError {
    /// A value cannot be resolved (outside both trees, no producer, climbing out).
    View(DepViewError),
    /// A producer whose served tree is a workspace folder: a native run is laid fetched
    /// packages' trees only (not built: refused by name, rule 3).
    MemberTree { links: String },
    /// A `links` value that is not one plain path part, so it cannot name a folder.
    LinksName { links: String },
    /// A folder or file could not be made.
    Files { folder: String, why: String },
}

impl From<DepViewError> for LayError {
    fn from(error: DepViewError) -> LayError {
        LayError::View(error)
    }
}

impl fmt::Display for LayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayError::View(error) => write!(f, "{error}"),
            LayError::MemberTree { links } => write!(
                f,
                "the `links = \"{links}\"` package is a workspace member whose metadata names its workspace: a native build script is given fetched packages' trees only (D112, brief N8; not built)"
            ),
            LayError::LinksName { links } => {
                write!(f, "`links = \"{links}\"` is not one plain path part, so its files cannot be laid for a native build script (D112, brief N8)")
            }
            LayError::Files { folder, why } => write!(f, "{folder}: laying a `links` dependency's files for a native build script: {why}"),
        }
    }
}

impl core::error::Error for LayError {}

/// Lays the trees `deps` name under `<work>/<rel>` and gives each pair's value as the native
/// run sees it. `work` is the command's canonical work folder; `rel` a relative folder of this
/// run's own (`deps/<run key>`), emptied first. `producers` are the reading script's `links`
/// producers, each with its `links`.
pub fn lay<'a>(work: &str, rel: &str, producers: &[(&str, &DepProducer)], deps: &[DepMetadata<'_>], alloc: Alloc<'a>) -> Result<Laid<'a>, LayError> {
    let work = work.trim_end_matches('/');
    let rel = rel.trim_matches('/');
    let root = format!("{work}/{rel}");
    let resolved = resolve(Sight::Native { root: &root }, producers, deps, alloc)?;

    // Each folder once, sorted: (where the native run sees it, where the wasm sight does, its
    // files). Two pairs under one tree share it.
    let mut places: Vec<(AString<'a>, AString<'a>, &[(String, Vec<u8>)])> = Vec::with_capacity(resolved.serve.len());
    for &(mount, producer) in resolved.serve.iter() {
        let links = match mount {
            DepMount::Out { links, .. } | DepMount::Tree { links } => links,
        };
        if links.is_empty() || links == "." || links == ".." || links.contains('/') || links.contains('\\') {
            return Err(LayError::LinksName { links: links.to_owned() });
        }
        let files: &[(String, Vec<u8>)] = match mount {
            DepMount::Out { .. } => match producer.stored.first() {
                Some(file) => return Err(LayError::View(DepViewError::StoredOut { links: links.to_owned(), file: file.clone() })),
                None => producer.out.as_slice(),
            },
            DepMount::Tree { .. } => match &producer.tree {
                Tree::Mem { files, .. } => files.as_slice(),
                // The wasm sight serves a member's workspace as the reader's own tree; a
                // native run is not given a workspace folder (not built).
                Tree::Dir(_) => return Err(LayError::MemberTree { links: links.to_owned() }),
            },
        };
        let mut native = AString::new_in(alloc);
        Sight::Native { root: &root }.seen_at(mount, &mut native);
        let mut shown = AString::new_in(alloc);
        Sight::Wasm.seen_at(mount, &mut shown);
        places.push((native, shown, files));
    }
    places.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    places.dedup_by(|a, b| a.0.as_str() == b.0.as_str());

    empty(&root)?;
    let (io, world) = native_world(Path::new(work)).map_err(|e| LayError::Files { folder: work.to_owned(), why: e.to_string() })?;
    let mut folders: AVec<'a, AString<'a>> = AVec::with_capacity_in(places.len(), alloc);
    let mut shown: AVec<'a, (AString<'a>, AString<'a>)> = AVec::with_capacity_in(places.len(), alloc);
    for (native, wasm, files) in places {
        let folder = native.as_str();
        let failed = |why: String| LayError::Files { folder: folder.to_owned(), why };
        // Under `work` by construction (`root` is `<work>/<rel>`).
        let under = folder.strip_prefix(work).map(|r| r.trim_start_matches('/')).ok_or_else(|| failed("not under the work folder".to_owned()))?;
        let cx = io.cx(Duration::from_secs(60), TASK_FRAME).map_err(|_| failed("its world stopped".to_owned()))?;
        // An empty tree is laid as an empty folder (a marker written, then removed).
        let marker: [(String, Vec<u8>); 1] = [(".blaze-made".to_owned(), Vec::new())];
        let all = if files.is_empty() { &marker[..] } else { files };
        for (name, bytes) in all {
            let at = format!("{under}/{name}");
            let path = bstd::path::Path::new(&at).map_err(|_| failed(format!("{name}: not a relative path")))?;
            let file = world.create(&cx, &path).map_err(|e| failed(format!("{name}: {e}")))?;
            file.write_at(&cx, 0, bytes, Durable::No).map_err(|e| failed(format!("{name}: {e}")))?;
            file.close(&cx, Durable::No).map_err(|e| failed(format!("{name}: {e}")))?;
        }
        if files.is_empty() {
            remove(folder, ".blaze-made").map_err(|e| failed(e.to_string()))?;
        }
        folders.push(blaze_own::String::from_str_in(native.as_str(), alloc));
        shown.push((native, wasm));
    }
    Ok(Laid { values: resolved.values, folders, shown })
}

/// `root` holding nothing: what an earlier run of the same key left there is removed.
fn empty(root: &str) -> Result<(), LayError> {
    if !Path::new(root).exists() {
        return Ok(());
    }
    let failed = |why: String| LayError::Files { folder: root.to_owned(), why };
    for left in listed_under(root).map_err(|e| failed(e.to_string()))? {
        remove(root, &left.name).map_err(|e| failed(e.to_string()))?;
    }
    Ok(())
}
