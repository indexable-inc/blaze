//! Proc-macro libraries that list their macros in a static, as blaze links them (`--shared`),
//! which the toolchain's server does not read: a caller that names a server reading that list
//! has such libraries expanded by it, and the others by the toolchain's.

use std::sync::OnceLock;

use ide_db::FxHashMap;
use vfs::{AbsPath, AbsPathBuf};

/// The name of the static the list is in.
const LIST: &[u8] = b"BLAZE_PROC_MACROS";

static SERVER: OnceLock<AbsPathBuf> = OnceLock::new();

/// A library rustc compiled of a proc macro: the name of its crate, where it is, and the
/// `rust-project.json` of the crate and what it depends on, when the load can say it.
pub struct Compiled {
    pub name: String,
    pub library: AbsPathBuf,
    pub description: Option<String>,
}

/// For the sysroot and root of a workspace and libraries rustc compiled, what a caller has of
/// each: a library that lists its macros or a WebAssembly module, `None` to expand with
/// rustc's, or why it has none and the load is to fail.
pub type Own = dyn Fn(Option<&AbsPath>, &AbsPath, &[Compiled]) -> Vec<Result<Option<AbsPathBuf>, String>>
    + Send
    + Sync;

/// Like [`Own`], with the current request's environment for compiler subprocesses.
pub type OwnWithEnv = dyn Fn(
        Option<&AbsPath>,
        &AbsPath,
        &[Compiled],
        &FxHashMap<String, Option<String>>,
    ) -> Vec<Result<Option<AbsPathBuf>, String>>
    + Send
    + Sync;

static OWN: OnceLock<Box<OwnWithEnv>> = OnceLock::new();

/// Has each library a load names that does not list its macros expanded with what `own` has
/// of it. The first call is the one kept.
pub fn own_libraries_with(own: Box<Own>) {
    own_libraries_with_env(Box::new(move |sysroot, workspace, libraries, _| {
        own(sysroot, workspace, libraries)
    }));
}

/// Like [`own_libraries_with`], passing the environment to use for compiler subprocesses.
/// These variables are not part of a library's persisted description.
pub fn own_libraries_with_env(own: Box<OwnWithEnv>) {
    _ = OWN.set(own);
}

/// Whether a caller has its own of the libraries rustc compiled.
pub(crate) fn owned() -> bool {
    OWN.get().is_some()
}

/// What stands for each of `libraries` ([`Own`]).
pub(crate) fn own(
    sysroot: Option<&AbsPath>,
    workspace: &AbsPath,
    libraries: &[Compiled],
    env: &FxHashMap<String, Option<String>>,
) -> Vec<Result<Option<AbsPathBuf>, String>> {
    match OWN.get() {
        Some(own) if !libraries.is_empty() => own(sysroot, workspace, libraries, env),
        _ => vec![Ok(None); libraries.len()],
    }
}

/// Has the libraries with a list of their macros loaded from here on expanded by the server
/// at `server`. The first call is the one kept.
pub fn serve_listed_with(server: AbsPathBuf) {
    _ = SERVER.set(server);
}

pub(crate) fn server() -> Option<&'static AbsPath> {
    SERVER.get().map(AbsPathBuf::as_path)
}

/// Whether the library at `path` lists its macros: whether the name of the list is in it,
/// which a library rustc built has only when its own source names it.
pub fn is_listed(path: &AbsPath) -> bool {
    std::fs::read(path).is_ok_and(|bytes| bytes.windows(LIST.len()).any(|it| it == LIST))
}
