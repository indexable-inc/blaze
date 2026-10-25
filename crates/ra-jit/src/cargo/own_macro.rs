//! Proc macros compiled and linked by ra-jit (docs/no-rustc.md, step J). When rust-analyzer's
//! `cargo check --compile-time-deps` of a load into ra-jit's own target directory asks rustc
//! for a proc macro, and the load names a server that reads the list of macros ra-jit writes
//! into a library ([`SERVER`], `vendor/proc-macro-srv`), the wrapper describes the proc macro
//! and the libraries it links as it does a build script (`own_script.rs`), and ra-jit links it
//! as a shared library where rustc would have put it, with the dep-info cargo reads. The
//! load expands with it by that server. Cargo's own builds never read that directory; a crate
//! rustc compiles there that links such a library has rustc compile the proc macro first,
//! over ra-jit's library, as rustc loads only its own.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::Context as _;
use paths::AbsPathBuf;

use super::{
    own_script::{self, Made},
    say,
    units::Record,
};

/// The variable that names the proc-macro server that reads the list of macros of a library
/// ra-jit linked: a path, or `1` for the one a release ships beside ra-jit ([`SHIPPED`]).
pub(crate) const SERVER: &str = "RA_JIT_PROC_MACRO_SERVER";

/// The server's executable a release has beside ra-jit's (`.github/scripts/build_release.py`).
pub(crate) const SHIPPED: &str = "blaze-proc-macro-srv";

/// The variable ra-jit sets for the cargo of a load into its own target directory.
pub(crate) const OWN_TARGET: &str = "RA_JIT_OWN_TARGET";

/// Whether a proc macro cargo asks the wrapper for is ra-jit's to compile. The load names the
/// server it found ([`server`]) to its cargo, so the wrapper reads only that it is named.
pub(crate) fn wanted() -> bool {
    std::env::var_os(SERVER).is_some() && std::env::var_os(OWN_TARGET).is_some()
}

/// The server that expands with the libraries ra-jit links, as [`SERVER`] says, relative to
/// the current directory: `None` when it names none, or `1` and no server is beside ra-jit.
/// A load names it to its cargo, or takes the variable away, so the wrapper links a proc
/// macro only for a server there is.
pub(crate) fn server() -> Option<&'static Path> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND.get_or_init(find).as_deref()
}

fn find() -> Option<PathBuf> {
    let named = std::env::var_os(SERVER)?;
    if named == "1" {
        let exe = std::env::current_exe().ok()?;
        let shipped = exe.parent()?.join(format!("{SHIPPED}{}", std::env::consts::EXE_SUFFIX));
        if !shipped.is_file() {
            say::fallback(
                &format!("no proc-macro server is beside {}", exe.display()),
                "rustc compiles the proc macros and the toolchain's server expands them",
            );
            return None;
        }
        return Some(shipped);
    }
    std::path::absolute(named).ok()
}

/// What ra-jit makes of the proc macro `record` is of: its library, where rustc would have
/// put it, and the dep-info cargo reads. `None` when cargo names no place for them.
pub(crate) fn made(record: &Record) -> Option<Made<'_>> {
    let (prefix, suffix) = (std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
    let name = record.crate_name.as_str();
    let extra = record.codegen("extra-filename").unwrap_or_default();
    let out_dir = record.out_dir()?;
    Some(Made {
        name,
        proc_macro: true,
        exe: out_dir.join(format!("{prefix}{name}{extra}{suffix}")),
        dep_info: out_dir.join(format!("{name}{extra}.d")),
    })
}

/// Has rustc compile, over ra-jit's library, each proc macro ra-jit linked that the crate
/// cargo asked rustc for with `args`, in `cwd`, links. `rustc` is the compiler cargo named.
pub(crate) fn rustc_first(rustc: &OsStr, args: &[String], cwd: &Path) -> anyhow::Result<()> {
    let externs = args.windows(2).filter(|it| it[0] == "--extern").map(|it| it[1].as_str());
    let (prefix, suffix) = (std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
    for (_, path) in externs.filter_map(|it| it.split_once('=')) {
        let library = cwd.join(path);
        let Some(unit) = library.file_name().and_then(|it| it.to_str()) else { continue };
        let Some(unit) = unit.strip_prefix(prefix).and_then(|it| it.strip_suffix(suffix)) else {
            continue;
        };
        let Some(listed) = library.to_str().and_then(|it| AbsPathBuf::try_from(it).ok()) else {
            continue;
        };
        let listed = listed.as_path();
        if !load_cargo::is_listed(listed) {
            continue;
        }
        let records =
            library.parent().map(Record::dir_for).context("a library without a directory")?;
        let record = Record::described(&records, unit).with_context(|| {
            format!("ra-jit linked {} and described it nowhere", library.display())
        })?;
        // Every crate that links it waits here for the one that has rustc compile it.
        let lock = std::fs::File::create(records.join(unit).join("rustc.lock"))?;
        lock.lock()?;
        if !load_cargo::is_listed(listed) {
            continue;
        }
        // Kept beside its record, for `RA_JIT_PROC_MACRO_LIBS` to compare with rustc's.
        let kept = records.join(unit).join(library.file_name().context("a library unnamed")?);
        std::fs::copy(&library, &kept)
            .with_context(|| format!("copying {} to {}", library.display(), kept.display()))?;
        let mut compile = super::wrapper::rustc_of(rustc);
        for (name, _) in std::env::vars().filter(|(name, _)| own_script::read_by_crates(name)) {
            compile.env_remove(name);
        }
        compile.args(&record.args).envs(&record.crate_env).current_dir(&record.cwd);
        let compiled = compile.env_remove(crate::answers::ONLY_ASKS).output()?;
        anyhow::ensure!(
            compiled.status.success(),
            "rustc did not compile the proc macro `{}`: {}",
            record.crate_name,
            String::from_utf8_lossy(&compiled.stderr).trim_end()
        );
        let package = std::env::var("CARGO_PKG_NAME").unwrap_or_default();
        say::fallback(
            &format!("`{package}` is rustc's and links the proc macro `{}`", record.crate_name),
            "rustc compiles the proc macro too",
        );
    }
    Ok(())
}
