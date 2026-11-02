//! Pure build scripts (docs/v2/plan/lanes/pure-scripts/design.md): a script whose MIR proves it
//! only reads the variables cargo gives it and its package's files, writes its `OUT_DIR` and
//! prints (`hir_ty_pure`) is kept a second time, under a key that needs no compile: its
//! instance graph's identity, the variables its code can read and the package files its first
//! run read.
//!
//! - [`run_key`]: the run key, of the identity alone; [`find`] finds a result under it for the
//!   inputs as they are now, before the script is compiled.
//! - [`check`]: what the first, sandboxed run's trace shows, against the proof: an effect the
//!   proof said absent is an error naming the script and the effect (a bug in the analysis),
//!   never a silent reclassification. It gives the package files the run read.
//! - [`checked_value`]: what the run printed and wrote, against the proof.
//! - [`keep`]: the result kept under the pure key, beside the run's own.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use blaze_build_script::Error;
use blaze_cas::{Guard, Reader, Store, Ticket};
use blaze_core::Traced;
use blaze_ids::ContentId;

use crate::{
    Given, RunKey, RunKind,
    index::{self, FindError, Found, KeepError},
    run::Value,
    trace::Inputs,
};

/// What a pure script's code may do, as `hir_ty_pure::Proof` says: made from it by the
/// caller, which has the front end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proof {
    /// The Merkle hash of its instance graph.
    pub identity: ContentId,
    /// Every variable its code can read, sorted, each once.
    pub env: Vec<OsString>,
    /// It may read files inside its package, or test whether one exists.
    pub reads: bool,
    /// It may write inside its `OUT_DIR`.
    pub writes: bool,
    /// It may print.
    pub prints: bool,
}

/// What a pure run is given beside its identity: nothing but that it is one, so a pure key is
/// never a sandboxed run's.
const GIVEN: &[u8] = b"blaze v2 pure build script 1";

/// The run key a pure script's result is kept under: its identity, and nothing given. Its
/// working directory is its package, whose files are in its shape by absolute path; its
/// arguments and standard input are empty.
pub fn run_key(proof: &Proof) -> RunKey {
    let mut given = Given::new(RunKind::BuildScript);
    given.part("pure", GIVEN);
    RunKey { code: proof.identity, given: given.finish() }
}

/// The result of the pure script of `proof` for its inputs as they are now, in `inputs` (the
/// script's environment as its run would be given it, and the machine's files: a
/// [`crate::inputs::Disk`]), if the store has one: then it is neither compiled nor run.
pub fn find<I: Inputs>(store: &Reader, guard: &Guard, proof: &Proof, inputs: &mut I) -> Result<Option<Value>, String>
where
    I::Error: std::fmt::Debug,
{
    match index::find(store, guard, &run_key(proof), inputs) {
        Ok(Found::Hit { value, .. }) => Value::decode(value).map(Some),
        Ok(Found::Miss { .. }) => Ok(None),
        // alloc: error path
        Err(error) => Err(find_error(error)),
    }
}

fn find_error<E: std::fmt::Debug>(error: FindError<E>) -> String {
    match error {
        // alloc: error path
        FindError::Corrupt(corrupt) => format!("the store's record of a pure build script: {corrupt:?}"),
        // alloc: error path
        FindError::NotAShape { key, why } => format!("the pure build script's shape slot {key:?}: {why}"),
        // alloc: error path
        FindError::Inputs(error) => format!("hashing what the pure build script reads: {error:?}"),
    }
}

/// Where a script's run is, for [`check`].
#[derive(Clone, Copy, Debug)]
pub struct Places<'a> {
    /// The script's package: its manifest's directory.
    pub package: &'a Path,
    pub out_dir: &'a Path,
    /// The build's target directory: never a package file, even when it is inside the package.
    pub target_dir: &'a Path,
    /// The script's program, which the loader reads.
    pub program: &'a Path,
    /// The toolchain's directories the sandbox lets every script read (blaze's stand-ins for
    /// cargo's tools, the launcher), which the runtime may read before `main`.
    pub toolchain: &'a [PathBuf],
}

/// The directories and files the dynamic loader and the runtime read before `main` on this
/// platform: shared libraries, the loader's caches and configuration. A pure script's code reads
/// none of them, so a read there is no input of it.
#[cfg(target_os = "linux")]
pub const RUNTIME: &[&str] = &[
    "/lib",
    "/lib32",
    "/lib64",
    "/usr/lib",
    "/usr/lib32",
    "/usr/lib64",
    "/nix/store",
    "/etc/ld.so.cache",
    "/etc/ld.so.preload",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/proc/self",
    "/sys/devices/system/cpu",
];
#[cfg(target_os = "macos")]
pub const RUNTIME: &[&str] =
    &["/usr/lib", "/System", "/Library/Apple", "/private/var/db/dyld", "/nix/store", "/dev"];
#[cfg(windows)]
pub const RUNTIME: &[&str] = &["C:\\Windows\\System32", "C:\\Windows\\SysWOW64", "C:\\Windows\\WinSxS"];
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub const RUNTIME: &[&str] = &["/lib", "/usr/lib", "/usr/local/lib", "/libexec"];

/// Each package file the run read, with its hash then: what the pure key is made of.
pub type Read = Vec<(PathBuf, Option<[u8; 32]>)>;

/// What `traced`, the trace of the first sandboxed run of the pure script `script` (its
/// package's description, for the error), shows, against `proof`: the package files it read,
/// or an error naming the script and the first effect its proof said it never has. Called
/// before anything is taken out of the trace (the run's own key's `keyed`), so it sees every
/// read.
pub fn check(script: &str, proof: &Proof, places: &Places<'_>, traced: &Traced) -> Result<Read, Error> {
    // alloc: error path
    let bug = |what: String| Error(format!("the build script of `{script}` was proved pure, but its run {what}: a bug in blaze's purity proof (hir-ty-pure)"));
    if let Some(it) = traced.untraceable.first() {
        // alloc: error path
        return Err(bug(format!("read what can't be traced ({it:?}), which its proof says it never does")));
    }
    if let Some((dir, _)) = traced.dirs.iter().find(|(dir, _)| !runtime(dir, places)) {
        // alloc: error path
        return Err(bug(format!("listed {}, which its proof says it never does", dir.display())));
    }
    let mut read = Vec::new(); // alloc: the edge, once per miss: the package files the key keeps
    for (path, hash) in &traced.files {
        if path.starts_with(places.out_dir) || path == places.program {
            continue;
        }
        if path.starts_with(places.target_dir) {
            // alloc: error path
            return Err(bug(format!("read {} in the target directory, which its proof says it never does", path.display())));
        }
        if path.starts_with(places.package) {
            if !proof.reads {
                // alloc: error path
                return Err(bug(format!("read {} in its package, which its proof says it never does", path.display())));
            }
            read.push((path.clone(), *hash)); // clone: the edge, the path the key keeps
            continue;
        }
        if !runtime(path, places) {
            // alloc: error path
            return Err(bug(format!("read {}, outside its package and OUT_DIR, which its proof says it never does", path.display())));
        }
    }
    Ok(read)
}

/// Whether `path` is one the loader or the runtime reads before `main`: [`RUNTIME`]'s, or the
/// toolchain's.
fn runtime(path: &Path, places: &Places<'_>) -> bool {
    RUNTIME.iter().any(|root| path.starts_with(root)) || places.toolchain.iter().any(|root| path.starts_with(root))
}

/// What a succeeded run of the pure script `script` printed and wrote, against `proof`.
pub fn checked_value(script: &str, proof: &Proof, value: &Value) -> Result<(), Error> {
    // alloc: error path
    let bug = |what: &str| Error(format!("the build script of `{script}` was proved pure, but its run {what}: a bug in blaze's purity proof (hir-ty-pure)"));
    if !proof.prints && (!value.stdout.is_empty() || !value.stderr.is_empty()) {
        return Err(bug("printed, which its proof says it never does"));
    }
    if !proof.writes && !value.files.is_empty() {
        return Err(bug("wrote in its OUT_DIR, which its proof says it never does"));
    }
    Ok(())
}

/// Keeps `value`, the result of a run of the pure script of `proof` that [`check`] found read
/// `read` in its package, under its pure key: the shape is the proof's variables, with their
/// values as `inputs` (the run's environment: its [`crate::inputs::Disk`]) has them, and those
/// files.
pub fn keep<I: Inputs>(store: &mut Store, proof: &Proof, read: Read, inputs: &mut I, value: &Value) -> Result<Vec<Ticket>, KeepError> {
    let env = proof
        .env
        .iter()
        // clone: the edge, once per miss: the names and values the shape and key keep
        .map(|name| (name.clone(), inputs.env(name.as_os_str()).map(OsStr::to_os_string)))
        // alloc: the edge, once per miss: the trace `index::keep` takes owns its list
        .collect();
    // alloc: none, empty lists
    let traced = Traced { files: read, dirs: Vec::new(), env, untraceable: Vec::new() };
    index::keep(store, &run_key(proof), &traced, value.encode())
}

#[cfg(test)]
mod tests;
