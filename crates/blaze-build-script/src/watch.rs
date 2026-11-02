//! What cargo's rule watches of a run, as data: for a run that read what can't be traced (the
//! clock, randomness, the network), which is keyed as cargo keys it (#1450). cargo's rule is
//! `build_script_local_fingerprints` and `local_fingerprints_deps` in
//! `src/core/compiler/fingerprint/mod.rs`, and for the whole package `PathSource::list_files`
//! (`list_files_walk`) in `src/sources/path.rs`, in rust-lang/cargo at 5f94df478; [`Rerun`]
//! is the same rule as cargo checks it on disk.
//!
//! [`Rerun`]: crate::apply::Rerun

use std::{
    borrow::{Borrow, Cow},
    collections::hash_map::Entry,
    ffi::OsStr,
    hash::{BuildHasher, Hash},
    path::{Component, Path, PathBuf},
};

use blaze_hash::{FixedState, HashMap};

use crate::{
    Error,
    output::{self, Context, Directive, Output, Sink},
};

/// What a change to reruns a script, as cargo's rule has it.
#[derive(Debug, PartialEq, Eq)]
pub struct Watched {
    /// Whole paths, in the order printed and each once: a file, or a directory meaning
    /// everything below it (links followed). cargo reruns a script for a path that is
    /// missing on every build, until it is there. With `whole_package`, the package's
    /// directory alone, less what [`skipped`] leaves out.
    pub paths: Vec<PathBuf>,
    /// The script printed neither `rerun-if-changed` nor `rerun-if-env-changed`: any change to
    /// the package's files reruns it. (For a package from a registry or git, whose files
    /// don't change, cargo watches its version alone.)
    pub whole_package: bool,
    /// `rerun-if-env-changed`, in the order printed and each once. cargo compares the value
    /// `[env]` gives a variable when it has it, else its own environment's.
    pub env: Vec<String>,
}

/// What `output` makes cargo watch, for the package whose directory is `package_dir`.
pub fn watched(output: &Output, package_dir: &Path) -> Watched {
    watched_of(&output.rerun_if_changed, &output.rerun_if_env_changed, package_dir)
}

/// [`watched`] of the [`Output`] `stdout` parses to under `cx`, read without making it: the
/// pass keeps the `rerun-if-*` values alone, borrowed from `stdout`. The same error as
/// [`output::parse`] when the output is refused.
pub fn watched_in(stdout: &[u8], cx: &Context<'_>, package_dir: &Path) -> Result<Watched, Error> {
    let mut reruns = Reruns::default();
    output::read(stdout, cx, &mut reruns)?;
    Ok(watched_of(&reruns.paths, &reruns.env, package_dir))
}

/// The `rerun-if-*` values of a run's output, as [`watched_in`] reads them.
#[derive(Default)]
struct Reruns<'a> {
    paths: Vec<Cow<'a, Path>>,
    env: Vec<Cow<'a, str>>,
}

impl<'a> Sink<'a> for Reruns<'a> {
    fn directive(&mut self, directive: Directive<'a>) {
        match directive {
            Directive::RerunIfChanged(Cow::Borrowed(path)) => {
                self.paths.push(Cow::Borrowed(Path::new(path)));
            }
            Directive::RerunIfChanged(Cow::Owned(path)) => {
                self.paths.push(Cow::Owned(PathBuf::from(path)));
            }
            Directive::RerunIfEnvChanged(name) => self.env.push(name),
            _ => {}
        }
    }
}

/// [`watched`] of the paths and names printed, made whole in one buffer reused for every path
/// and copied out only for one not met before.
fn watched_of<P: AsRef<Path>, E: AsRef<str>>(
    printed: &[P],
    names: &[E],
    package_dir: &Path,
) -> Watched {
    if printed.is_empty() && names.is_empty() {
        // alloc: the result: the package directory alone.
        let paths = vec![package_dir.to_owned()];
        // alloc: none: an empty list.
        return Watched { paths, whole_package: true, env: Vec::new() };
    }
    let mut paths: Vec<PathBuf> = Vec::with_capacity(printed.len());
    let mut seen = Seen::for_len(printed.len());
    let mut whole = PathBuf::new();
    for path in printed {
        whole.as_mut_os_string().clear();
        whole.push(package_dir);
        whole.push(path);
        if seen.first(&paths, whole.as_path()) {
            // clone: the result: a path met for the first time.
            paths.push(whole.clone());
        }
    }
    let mut env: Vec<String> = Vec::with_capacity(names.len());
    let mut seen = Seen::for_len(names.len());
    for name in names {
        let name = name.as_ref();
        if seen.first(&env, name) {
            // alloc: the result: a name met for the first time.
            env.push(name.to_owned());
        }
    }
    Watched { paths, whole_package: false, env }
}

/// Lists no longer than this are kept once each by comparing with what is kept, which is
/// faster than hashing at their length; longer ones (a script naming every file it reads)
/// by hash.
const SCAN: usize = 16;

/// What is kept of a list so far, for keeping each value once.
enum Seen {
    Scan,
    /// Each hash met, with the index of the first value kept that has it.
    Hashed(HashMap<u64, u32>),
}

impl Seen {
    fn for_len(len: usize) -> Seen {
        if len <= SCAN {
            Seen::Scan
        } else {
            Seen::Hashed(HashMap::with_capacity_and_hasher(len, FixedState))
        }
    }

    /// Whether `value` is not among `kept`, the values this has said so of, in order.
    fn first<K: Borrow<T>, T: Hash + PartialEq + ?Sized>(&mut self, kept: &[K], value: &T) -> bool {
        let unseen = |kept: &[K]| !kept.iter().any(|it| it.borrow() == value);
        match self {
            Seen::Scan => unseen(kept),
            Seen::Hashed(hashes) => match hashes.entry(FixedState.hash_one(value)) {
                Entry::Vacant(entry) => {
                    entry.insert(kept.len() as u32);
                    true
                }
                // Two values sharing a hash: the whole list decides.
                Entry::Occupied(entry) => {
                    kept[*entry.get() as usize].borrow() != value && unseen(kept)
                }
            },
        }
    }
}

/// Whether cargo's rule for the whole package leaves out the directory at `relative` (a path
/// below the package's directory) and everything below it: the package's own `target`, and a
/// directory that holds a `Cargo.toml` (`has_manifest`), which is another package. Nothing
/// else is left out: the manifest's `include` and `exclude` and a git checkout's ignore files
/// only narrow cargo's listing, and watching more than cargo is only slower.
pub fn skipped(relative: &Path, has_manifest: bool) -> bool {
    let mut parts = relative.components().filter(|it| *it != Component::CurDir);
    let Some(first) = parts.next() else { return false };
    has_manifest || (first == Component::Normal(OsStr::new("target")) && parts.next().is_none())
}

#[cfg(test)]
mod tests;
