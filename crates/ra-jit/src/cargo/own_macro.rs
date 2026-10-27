//! Proc macros compiled and linked by ra-jit (docs/no-rustc.md, step J). When rust-analyzer's
//! `cargo check --compile-time-deps` of a load into ra-jit's own target directory asks rustc
//! for a proc macro, and the load names a server that reads the list of macros ra-jit writes
//! into a library ([`SERVER`], `vendor/proc-macro-srv`), the wrapper describes the proc macro
//! and the libraries it links as it does a build script (`own_script.rs`), and ra-jit links it
//! as a shared library where rustc would have put it, with the dep-info cargo reads. The
//! load expands with it by that server. Cargo's own builds never read that directory, and
//! nothing there is rustc's (`wrapper.rs`, `for_load`): the libraries a proc macro links are
//! only described, and a proc macro without a server fails the load.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::Context as _;

use super::{
    own_script::{self, Made},
    say,
    units::Record,
};

/// The variable that names the proc-macro server that reads the list of macros of a library
/// ra-jit linked: a path, `1` for the one a release ships beside ra-jit ([`SHIPPED`]), which
/// is also what is used when it is not set and that server is there, or `0` for none.
pub(crate) const SERVER: &str = "RA_JIT_PROC_MACRO_SERVER";

/// The server's executable a release has beside ra-jit's (`.github/scripts/build_release.py`).
pub(crate) const SHIPPED: &str = "blaze-proc-macro-srv";

/// The variable ra-jit sets for the cargo of a load into its own target directory.
pub(crate) const OWN_TARGET: &str = "RA_JIT_OWN_TARGET";

/// Whether [`SERVER`] turns ra-jit's proc macros off, `0` or `off`, as the harness's reference
/// builds do: a load's cargo is then told so, and rustc builds what it asks for.
pub(crate) fn turned_off() -> bool {
    matches!(std::env::var_os(SERVER).as_deref().and_then(OsStr::to_str), Some("0" | "off"))
}

/// The directory in cargo's target directory of a load's own target directory when ra-jit
/// links the proc macros, where libraries are only described: cargo is given empty files for
/// them (`wrapper.rs`, `described_only`).
pub(crate) const OWN_DIR: &str = "rust-analyzer";

/// The same when [`turned_off`], where rustc builds every unit. Cargo does not fingerprint
/// [`SERVER`], so in one directory it would take the empty files of a load with ra-jit's proc
/// macros for rustc's libraries, and rustc then fails reading them (E0786, #460).
pub(crate) const BY_RUSTC_DIR: &str = "rust-analyzer-rustc";

/// The load's own target directory in cargo's, for the mode [`SERVER`] says now.
pub(crate) fn own_dir() -> &'static str {
    if turned_off() { BY_RUSTC_DIR } else { OWN_DIR }
}

/// What a load whose proc macro ra-jit has no server for is told: where the server was
/// looked for, and the two ways through.
pub(crate) fn missing_server() -> String {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| {
            Some(exe.parent()?.join(format!("{SHIPPED}{}", std::env::consts::EXE_SUFFIX)))
        })
        .map_or_else(|| SHIPPED.to_owned(), |it| it.display().to_string());
    format!(
        "no proc-macro server is beside ra-jit (looked for {beside}), and ra-jit links proc \
         macros only for its own. Build it there from a blaze checkout with `ra-jit \
         vendor/proc-macro-srv --crate rust_analyzer_proc_macro_srv --crate proc_macro --out \
         <scratch>/server.o --exe {beside}` (what `.github/scripts/build_release.py --server` \
         runs; the releases ship it on Linux), or have rustc build the load's proc macros \
         with `{SERVER}=0`"
    )
}

/// The server that expands with the libraries ra-jit links, as [`SERVER`] says, relative to
/// the current directory: `None` when it says none, or no server is beside ra-jit when it
/// says `1` or nothing. A load names it to its cargo, or takes the variable away, so the
/// wrapper links a proc macro only for a server there is.
pub(crate) fn server() -> Option<&'static Path> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND.get_or_init(find).as_deref()
}

fn find() -> Option<PathBuf> {
    let named = std::env::var_os(SERVER);
    match named.as_deref().and_then(OsStr::to_str) {
        Some("0" | "off") => return None,
        Some("1") | None => {}
        Some(_) => return named.and_then(|it| std::path::absolute(it).ok()),
    }
    let exe = std::env::current_exe().ok()?;
    let shipped = exe.parent()?.join(format!("{SHIPPED}{}", std::env::consts::EXE_SUFFIX));
    if shipped.is_file() {
        return Some(shipped);
    }
    // A build of ra-jit that has no server, as one from crates.io, links no proc macro: a load
    // into its own target directory that has one fails.
    if named.is_some() {
        eprintln!("warning: no proc-macro server is beside {}", exe.display());
    }
    None
}

/// `ra-jit proc-macro-library <sysroot> <library>`: makes ra-jit's library of the proc macro
/// rustc compiled at `<library>` and prints where it is ([`libraries_of`]).
pub(crate) const COMMAND: &str = "proc-macro-library";

/// The directory, in the record of a proc macro rustc compiled, of ra-jit's library of it.
const OWN: &str = "ra-jit";

/// How many libraries are made at once: each is a build that uses every core itself.
const AT_ONCE: usize = 4;

/// ra-jit's library of each of `libraries`, proc macros rustc compiled in a build through the
/// wrapper, which recorded them (`units.rs`): the store's, or linked from source over the
/// standard library of `sysroot`, each in a process of its own. `None` for one ra-jit could
/// not make, which is said, and whose library of rustc's the load then expands with.
pub(crate) fn libraries_of(sysroot: Option<&Path>, libraries: &[PathBuf]) -> Vec<Option<PathBuf>> {
    let (Some(sysroot), Ok(exe)) = (sysroot, std::env::current_exe()) else {
        return vec![None; libraries.len()];
    };
    let next = std::sync::atomic::AtomicUsize::new(0);
    let made = std::sync::Mutex::new(vec![None; libraries.len()]);
    std::thread::scope(|scope| {
        for _ in 0..AT_ONCE.min(libraries.len()) {
            scope.spawn(|| {
                loop {
                    let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(library) = libraries.get(at) else { break };
                    let mut command = std::process::Command::new(&exe);
                    command.arg(COMMAND).arg(sysroot).arg(library);
                    let ran = command.stdin(std::process::Stdio::null()).output();
                    let found = match ran {
                        Ok(ran) if ran.status.success() => {
                            let said = String::from_utf8_lossy(&ran.stdout).trim_end().to_owned();
                            Ok((!said.is_empty()).then(|| PathBuf::from(said)))
                        }
                        Ok(ran) => Err(String::from_utf8_lossy(&ran.stderr).trim_end().to_owned()),
                        Err(error) => Err(error.to_string()),
                    };
                    let name = library.file_name().unwrap_or_default().to_string_lossy();
                    match found {
                        Ok(it) => made.lock().unwrap()[at] = it,
                        Err(why) => say::fallback(
                            &format!("ra-jit did not make the proc macro {name}: {why}"),
                            "rustc's library of it is expanded by the toolchain's server",
                        ),
                    }
                }
            });
        }
    });
    made.into_inner().unwrap()
}

/// [`COMMAND`]. It prints nothing for a library rustc compiled in a build that did not go
/// through the wrapper, which recorded nothing of it.
pub(crate) fn command(mut args: impl Iterator<Item = std::ffi::OsString>) -> anyhow::Result<()> {
    let sysroot = PathBuf::from(args.next().context("the sysroot is named first")?);
    let library = PathBuf::from(args.next().context("then rustc's library")?);
    match library_of(&sysroot, &library) {
        Ok(own) => {
            if let Some(own) = own {
                println!("{}", own.display());
            }
            Ok(())
        }
        Err(why) => {
            eprintln!("{why}");
            std::process::exit(1)
        }
    }
}

/// ra-jit's library of the proc macro rustc compiled at `library`, made from its record over
/// the standard library of `sysroot`, in that record. `None` when it has no record.
fn library_of(sysroot: &Path, library: &Path) -> Result<Option<PathBuf>, String> {
    let (prefix, suffix) = (std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
    let file = library.file_name().and_then(OsStr::to_str).ok_or("a library unnamed")?;
    let stem = file.strip_prefix(prefix).and_then(|it| it.strip_suffix(suffix));
    let stem = stem.ok_or("not a library")?;
    let dir = library.parent().ok_or("a library without a directory")?;
    let unit = &super::layout::unit_of_file(stem, dir);
    let Some((record, records)) = Record::described_or_seeded(dir, unit) else { return Ok(None) };
    let dir = records.join(unit).join(OWN);
    let extra = record.codegen("extra-filename").unwrap_or_default();
    let made = Made {
        name: &record.crate_name,
        proc_macro: true,
        wasi: false,
        exe: dir.join(file),
        dep_info: dir.join(format!("{}{extra}.d", record.crate_name)),
    };
    std::fs::create_dir_all(&dir).map_err(|it| it.to_string())?;
    own_script::compile_record(sysroot, &record, &made, &dir)?;
    Ok(Some(made.exe))
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
        wasi: false,
        exe: out_dir.join(format!("{prefix}{name}{extra}{suffix}")),
        dep_info: out_dir.join(format!("{name}{extra}.d")),
    })
}
