//! Pure build scripts (docs/v2/plan.md): a script whose MIR proves it
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
use blaze_cas::{Guard, Key, Reader, Store};
use blaze_core::Traced;
use blaze_ids::ContentId;

use crate::{
    Given, RunKey, RunKind,
    index::{self, FindError, Found, KeepError},
    roots::{NoRoot, Rooted, Roots, verify},
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

/// The run key a pure script's result is kept under: its identity, the roles of its roots
/// ([`crate::roots`]), and nothing else given. Its working directory is its package, whose
/// files are in its shape written against the package's root; its arguments and standard
/// input are empty.
pub fn run_key(proof: &Proof, roots: &Roots<'_>) -> RunKey {
    let mut given = Given::new(RunKind::BuildScript);
    given.part("pure", GIVEN);
    given.roots(roots);
    RunKey { code: proof.identity, given: given.finish() }
}

/// The result of the pure script of `proof` for its inputs as they are now, in `inputs` (the
/// script's environment as its run would be given it, and the machine's files: a
/// [`crate::inputs::Disk`]), if the store has one: then it is neither compiled nor run.
///
/// `roots` are the run's, as [`crate::script::prepare`] makes them: its package, its
/// `OUT_DIR` and the build's.
pub fn find<I: Inputs>(
    store: &Reader,
    guard: &Guard,
    proof: &Proof,
    roots: Roots<'_>,
    inputs: &mut I,
) -> Result<Option<Value>, String>
where
    I::Error: std::fmt::Debug + From<NoRoot>,
{
    find_hit(store, guard, proof, roots, inputs).map(|it| it.map(|hit| hit.value))
}

/// What [`find_hit`] found under a pure key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// The kept value, with this run's roots.
    pub value: Value,
    /// The key of its unmarked result when it was verified across checkouts
    /// ([`Found::Hit`]'s `verified`): a run of the script that gives otherwise retracts it
    /// ([`verify::retraction`] under [`run_key`]).
    pub verified: Option<Key>,
}

/// [`find`], with whether what it found was verified across checkouts.
pub fn find_hit<I: Inputs>(
    store: &Reader,
    guard: &Guard,
    proof: &Proof,
    roots: Roots<'_>,
    inputs: &mut I,
) -> Result<Option<Hit>, String>
where
    I::Error: std::fmt::Debug + From<NoRoot>,
{
    match index::find(store, guard, &run_key(proof, &roots), &mut Rooted::new(roots, inputs)) {
        Ok(Found::Hit { value, verified, .. }) => Value::decode(value)
            .and_then(|value| roots.restored(value).map_err(String::from))
            .map(|value| Some(Hit { value, verified })),
        Ok(Found::Miss { .. }) => Ok(None),
        // alloc: error path
        Err(error) => Err(find_error(error)),
    }
}

/// Under `BLAZE_SCRIPT_CACHE=check` (or any rerun of a script whose pure key hit): `ran`, what
/// the pure script of `proof` gave run again in the checkout of `roots`, against `hit`, what
/// its pure key found. When the hit was verified across checkouts and `ran` differs from it
/// ([`crate::run::differs_with_stderr`], as sharing compares), the
/// record that the run depends on where its roots are is put ([`verify::retraction`]) and
/// how it differs is given with the record's tickets: no checkout is given the value again.
/// None when the hit was this checkout's own or `ran` is the same. A record whose put fails
/// part way gives the tickets of what landed in [`verify::NotKept`].
pub fn retract(
    store: &mut Store,
    proof: &Proof,
    roots: Roots<'_>,
    hit: &Hit,
    ran: &Value,
) -> Result<Option<(String, Vec<blaze_cas::Ticket>)>, verify::NotKept> {
    let Some(unmarked) = hit.verified else { return Ok(None) };
    let Some(what) = crate::run::differs_with_stderr(&hit.value, ran) else { return Ok(None) };
    let record = verify::retraction(&run_key(proof, &roots), &unmarked);
    let mut tickets = Vec::new(); // alloc: the record's tickets, once per retraction
    match index::keep_into(
        store,
        blaze_cas::Unit::SHARED,
        &record.run,
        &record.traced,
        &record.value,
        &mut tickets,
    ) {
        Ok(()) => Ok(Some((what, tickets))),
        Err(error) => {
            let retracted = tickets.first().copied();
            Err(verify::NotKept { error, tickets, retracted })
        }
    }
}

fn find_error<E: std::fmt::Debug>(error: FindError<E>) -> String {
    match error {
        // alloc: error path
        FindError::Corrupt(corrupt) => {
            format!("the store's record of a pure build script: {corrupt:?}")
        }
        // alloc: error path
        FindError::NotAShape { key, why } => {
            format!("the pure build script's shape slot {key:?}: {why}")
        }
        // alloc: error path
        FindError::Inputs(error) => format!("hashing what the pure build script reads: {error:?}"),
        // alloc: error path
        FindError::TooManyShapes { run } => {
            format!(
                "the pure build script's run {run:?} has more than {} shapes",
                crate::index::MOST_SHAPES
            )
        }
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
    /// The run's library search path: the directories blaze put in the script's library path
    /// variable for this run (`blaze_build_script::env::dylib_search_path` of its environment),
    /// `target/debug` and `target/debug/deps` among them. The loader walks them before `main`
    /// looking for each library the program needs ([`probe`]).
    pub search: &'a [PathBuf],
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
    // NixOS's glibc reads its preload list under this name.
    "/etc/ld-nix.so.preload",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/proc/self",
    "/sys/devices/system/cpu",
    // glibc's malloc reads the huge page settings when a tunable asks (`__malloc_hugepages`,
    // `__malloc_default_thp_pagesize`), and its `sysconf` the page sizes.
    "/sys/kernel/mm/transparent_hugepage",
    "/sys/kernel/mm/hugepages",
    // glibc's `__libc_alloca_cutoff`/`malloc` (`check_may_shrink_heap`) and `qsort`'s
    // allocation bound.
    "/proc/sys/vm/overcommit_memory",
    // musl's loader: its search path file, by the machine's name.
    "/etc/ld-musl-aarch64.path",
    "/etc/ld-musl-x86_64.path",
];
#[cfg(target_os = "macos")]
pub const RUNTIME: &[&str] =
    &["/usr/lib", "/System", "/Library/Apple", "/private/var/db/dyld", "/nix/store", "/dev"];
#[cfg(windows)]
pub const RUNTIME: &[&str] =
    &["C:\\Windows\\System32", "C:\\Windows\\SysWOW64", "C:\\Windows\\WinSxS"];
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub const RUNTIME: &[&str] = &["/lib", "/usr/lib", "/usr/local/lib", "/libexec"];

/// Each package file the run read, with its hash then: what the pure key is made of.
pub type Read = Vec<(PathBuf, Option<[u8; 32]>)>;

/// What `traced`, the trace of the first sandboxed run of the pure script `script` (its
/// package's description, for the error), shows, against `proof`: the package files it read,
/// or an error naming the script and the first effect its proof said it never has. Called
/// before anything is taken out of the trace (the run's own key's `keyed`), so it sees every
/// read.
///
/// What the sandbox answered from the run's provider (`getrandom`, an open of
/// `/dev/urandom`: docs/v2/plan/design/determinism.md, section C) is not in `traced` at all
/// ([`crate::trace::answered`]): it was made of the run's key, so it is no effect the proof
/// has to deny. What the sandbox could not answer still is one, and is refused here.
pub fn check(
    script: &str,
    proof: &Proof,
    places: &Places<'_>,
    traced: &Traced,
) -> Result<Read, Error> {
    // alloc: error path
    // The one message every refusal here starts with: the script, and that it was proved pure.
    let refusal =
        |what: String| Error(format!("the build script of `{script}` was proved pure, but {what}"));
    let bug = |what: String| {
        refusal(format!("its run {what}: a bug in blaze's purity proof (hir-ty-pure)"))
    };
    if let Some(it) = traced.untraceable.first() {
        // Who read it, as the run's sandbox described the caller (`crate::calls`): the
        // process, its program, the system call and where in the process it came from.
        let who = crate::calls::said(
            &crate::calls::of(places.program),
            crate::trace::untraceable_name(*it),
        );
        // alloc: error path
        let Error(what) = bug(format!(
            "got what the sandbox could neither trace nor answer from its provider ({it:?}), which its proof says it never does"
        ));
        // alloc: error path
        return Err(Error(format!("{what}{who}")));
    }
    // A listing is never the loader's: it opens a search directory's files by name and looks at
    // the directory, and lists neither. So a listed search directory is refused as any other.
    if let Some((dir, _)) = traced.dirs.iter().find(|(dir, _)| !runtime(dir, places)) {
        // alloc: error path
        return Err(bug(format!("listed {}, which its proof says it never does", dir.display())));
    }
    let mut read = Vec::new(); // alloc: the edge, once per miss: the package files the key keeps
    for (path, hash) in &traced.files {
        if path.starts_with(places.out_dir) || path == places.program {
            continue;
        }
        // The loader's walk of the run's library search path, wherever the path's directories
        // are; inside the package a script that may read keeps them as its reads instead, the
        // stricter of the two.
        let keeps = proof.reads
            && path.starts_with(places.package)
            && !path.starts_with(places.target_dir);
        if !keeps {
            match probe(path, hash.as_ref(), places.search) {
                Some(Probe::Directory | Probe::Missed) => continue,
                // No key of a pure script has a library found on its search path: the proof
                // is of its code's instance graph, and says nothing of a library it loads.
                Some(Probe::Found) => {
                    // alloc: error path
                    return Err(Error(format!(
                        "the build script of `{script}` was proved pure, but its loader found {} on the library search path blaze gave its run: a library loaded from there is an input its pure key does not cover (keys-cover-every-input), so the script is not pure",
                        path.display()
                    )));
                }
                None => {}
            }
        }
        if path.starts_with(places.target_dir) {
            // alloc: error path
            return Err(bug(format!(
                "read {} in the target directory, which its proof says it never does",
                path.display()
            )));
        }
        if path.starts_with(places.package) {
            if !proof.reads {
                // alloc: error path
                return Err(bug(format!(
                    "read {} in its package, which its proof says it never does",
                    path.display()
                )));
            }
            read.push((path.clone(), *hash)); // clone: the edge, the path the key keeps
            continue;
        }
        if !runtime(path, places) {
            // A directory that was only looked at (or a name with nothing there) is what a
            // loader's search of a library path leaves, and no code of the script's is shown
            // to have asked for it: the proof is not what this refuses. A sandbox describes
            // who made a call only for what it can't trace and for each program started
            // (`crate::calls`), so the reader is not known here; the run's search path is.
            let looked = hash.is_none_or(|it| it == crate::trace::DIRECTORY);
            if looked {
                // alloc: error path
                let search: Vec<String> =
                    places.search.iter().map(|it| it.display().to_string()).collect();
                // alloc: error path
                return Err(refusal(format!(
                    "a process of its run looked at {} (a directory, or a name with nothing there), outside its package and OUT_DIR and on no library search path blaze gave it ({}), and read no file there: the search of a loader for a library (a RUNPATH of the program or of a library loaded into it, the sandbox's own among them) rather than a read its proof denies; which program and call made it is not recorded for a read that is traced. Nothing is kept: its pure key does not cover that directory (keys-cover-every-input)",
                    path.display(),
                    search.join(":")
                )));
            }
            // alloc: error path
            return Err(bug(format!(
                "read the file {}, outside its package and OUT_DIR, which its proof says it never does",
                path.display()
            )));
        }
    }
    Ok(read)
}

/// Whether `path` is one the loader or the runtime reads before `main`: [`RUNTIME`]'s, or the
/// toolchain's.
fn runtime(path: &Path, places: &Places<'_>) -> bool {
    RUNTIME.iter().any(|root| path.starts_with(root))
        || places.toolchain.iter().any(|root| path.starts_with(root))
}

/// What a read is of the loader's walk of a library search path ([`probe`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Probe {
    /// A search directory itself, looked at (the loader's `stat` of a directory none of its
    /// opens found anything in), there or not.
    Directory,
    /// A library's name looked for in a search directory and not there.
    Missed,
    /// A library's name looked for in a search directory and found: the loader loaded it.
    Found,
}

/// What `path`, traced with `hash` (`None` for nothing there, [`crate::trace::DIRECTORY`] for
/// a directory), is of the loader's walk of `search`, the run's library search path, before
/// `main`; `None` when it is no part of that walk, and so a read of the script's own.
///
/// The loader takes each library the program needs and, for each search directory in order,
/// opens `<directory>/<name>`; glibc then looks at a directory nothing was found in. So the
/// walk is exactly: a search directory, and a direct child of one named as a library is
/// ([`library_name`]). glibc also looks under `<directory>/glibc-hwcaps/<level>` for each
/// level the machine has (elf/dl-hwcaps.c: `x86-64-v2`.. on x86-64, `power9`.., `z13`..; none
/// on aarch64): that directory, and a library's name in it. Nothing else under a search
/// directory is the loader's: another unit's `.rlib`, `.d` or `.rmeta` there is refused.
pub fn probe(path: &Path, hash: Option<&[u8; 32]>, search: &[PathBuf]) -> Option<Probe> {
    let directory = || match hash {
        None => Some(Probe::Directory),
        Some(it) if *it == crate::trace::DIRECTORY => Some(Probe::Directory),
        // A file where a search directory is named: nothing the loader walks.
        Some(_) => None,
    };
    let on_path = |dir: &Path| search.iter().any(|it| it.as_path() == dir);
    // glibc's `<directory>/glibc-hwcaps/<level>`, on Linux alone.
    let hwcaps = |dir: &Path| {
        cfg!(target_os = "linux")
            && dir.parent().is_some_and(|caps| {
                caps.file_name() == Some(OsStr::new("glibc-hwcaps"))
                    && caps.parent().is_some_and(|it| on_path(it))
            })
    };
    if on_path(path) || hwcaps(path) {
        return directory();
    }
    let (dir, name) = (path.parent()?, path.file_name()?);
    if !(on_path(dir) || hwcaps(dir)) || !library_name(name, Loader::HOST) {
        return None;
    }
    match hash {
        None => Some(Probe::Missed),
        // A directory named as a library is: the loader's open of it reads nothing.
        Some(it) if *it == crate::trace::DIRECTORY => Some(Probe::Missed),
        Some(_) => Some(Probe::Found),
    }
}

/// Whose names a shared library has ([`library_name`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Loader {
    /// ELF's (`ld.so`, musl's): `lib*.so`, `lib*.so.<version>`.
    Elf,
    /// dyld's: `*.dylib`.
    Dyld,
    /// Windows': `*.dll`.
    Windows,
}

impl Loader {
    /// This machine's, which runs the script.
    #[cfg(target_vendor = "apple")]
    pub const HOST: Loader = Loader::Dyld;
    #[cfg(windows)]
    pub const HOST: Loader = Loader::Windows;
    #[cfg(not(any(target_vendor = "apple", windows)))]
    pub const HOST: Loader = Loader::Elf;
}

/// Whether `name`, a file's name, is a shared library's as `loader` names them: ELF's
/// `lib<something>.so` and `lib<something>.so.<digits and dots>` (`libc.so.6`,
/// `libgcc_s.so.1`, `libz.so.1.3.1`), dyld's `<something>.dylib`, Windows' `<something>.dll`
/// (in any case). Nothing else: not `libfoo.rlib`, `libfoo.so.d`, `foo.so` or `libfoo.so.`.
pub fn library_name(name: &OsStr, loader: Loader) -> bool {
    let Some(name) = name.to_str() else { return false };
    let stem_then = |suffix: &str| {
        name.len() > suffix.len()
            && name.is_char_boundary(name.len() - suffix.len())
            && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    };
    match loader {
        Loader::Elf => {
            let Some(rest) = name.strip_prefix("lib") else { return false };
            let Some((stem, version)) = rest.split_once(".so") else { return false };
            // `.so` is the first one: a version has none, so `liba.so.so` is refused below.
            let versioned = version.strip_prefix('.').is_some_and(|it| {
                !it.is_empty()
                    && it.split('.').all(|part| {
                        !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())
                    })
            });
            !stem.is_empty() && (version.is_empty() || versioned)
        }
        Loader::Dyld => name.ends_with(".dylib") && name.len() > ".dylib".len(),
        Loader::Windows => stem_then(".dll"),
    }
}

/// What a succeeded run of the pure script `script` printed and wrote, against `proof`.
pub fn checked_value(script: &str, proof: &Proof, value: &Value) -> Result<(), Error> {
    // alloc: error path
    let bug = |what: &str| {
        Error(format!(
            "the build script of `{script}` was proved pure, but its run {what}: a bug in blaze's purity proof (hir-ty-pure)"
        ))
    };
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
/// files, all written against `roots`; what it printed is kept ported, kept against where its
/// roots are and given to another checkout once one verified it ([`crate::roots::verify`]).
/// `roots` are the run's prepared ones ([`crate::script::prepare`]): their links as they were
/// read then are what [`Roots::relinked`] compares with now.
pub fn keep<I: Inputs>(
    store: &mut Store,
    proof: &Proof,
    roots: Roots<'_>,
    read: Read,
    inputs: &mut I,
    value: &Value,
) -> Result<verify::Kept, verify::NotKept> {
    let env = proof
        .env
        .iter()
        // clone: the edge, once per miss: the names and values the shape and key keep
        .map(|name| (name.clone(), inputs.env(name.as_os_str()).map(OsStr::to_os_string)))
        // alloc: the edge, once per miss: the trace `index::keep` takes owns its list
        .collect();
    // alloc: none, empty lists
    let traced = Traced { files: read, dirs: Vec::new(), env, untraceable: Vec::new() };
    let run = run_key(proof, &roots);
    // alloc: error path, why the scan could not be made
    let (marked, stored) = roots
        .kept(&traced, value)
        .map_err(|error| KeepError::Scan { run, why: error.to_string() })?;
    verify::keep(store, &roots, &run, marked, stored.encode(), crate::run::ported_differs)
}

#[cfg(test)]
mod tests;
