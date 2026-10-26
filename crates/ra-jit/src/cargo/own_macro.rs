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
    collections::BTreeMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::Context as _;
use paths::AbsPathBuf;
use sha2::{Digest as _, Sha256};

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

/// Whether a proc macro cargo asks the wrapper for is ra-jit's to compile. The load names the
/// server it found ([`server`]) to its cargo, so the wrapper reads only that it is named.
pub(crate) fn wanted() -> bool {
    std::env::var_os(SERVER).is_some() && std::env::var_os(OWN_TARGET).is_some()
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
    // A build of ra-jit that has no server, as one from crates.io, is as it was before.
    if named.is_some() {
        say::fallback(
            &format!("no proc-macro server is beside {}", exe.display()),
            "rustc compiles the proc macros and the toolchain's server expands them",
        );
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
    // One ra-jit's own target directory was seeded with is recorded in cargo's.
    let parts: Vec<_> = dir.components().collect();
    let cargos = parts.iter().rposition(|it| it.as_os_str() == "rust-analyzer").map(|at| {
        let parts = parts.iter().enumerate().filter(|(it, _)| *it != at);
        parts.map(|(_, it)| it).collect::<PathBuf>()
    });
    let found = [Some(dir.to_owned()), cargos].into_iter().flatten().find_map(|dir| {
        let records = Record::dir_for(&dir);
        Some((Record::described(&records, unit)?, records))
    });
    let Some((record, records)) = found else { return Ok(None) };
    let dir = records.join(unit).join(OWN);
    let extra = record.codegen("extra-filename").unwrap_or_default();
    let made = Made {
        name: &record.crate_name,
        proc_macro: true,
        exe: dir.join(file),
        dep_info: dir.join(format!("{}{extra}.d", record.crate_name)),
    };
    std::fs::create_dir_all(&dir).map_err(|it| it.to_string())?;
    own_script::compile_record(sysroot, &record, &made, &dir)?;
    Ok(Some(made.exe))
}

/// What the store keeps ra-jit's proc-macro libraries under in place of a toolchain: a
/// library is told by its description, which its key has.
const LIBRARIES: &str = "proc-macro libraries 1";

/// The most a library and the files it was linked from may have for the store to keep it.
const MOST_KEPT: usize = 256 << 20;

/// The key of the library ra-jit links of the first crate of `described`, the text of its
/// `rust-project.json`.
pub(crate) fn library_key(described: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    for field in [LIBRARIES.as_bytes(), described.as_bytes()] {
        hash.update((field.len() as u64).to_le_bytes());
        hash.update(field);
    }
    hash.finalize().into()
}

fn store() -> Option<&'static ra_jit_store::Store> {
    ra_jit_store::compiles_against(LIBRARIES);
    ra_jit_store::Store::global_or(super::daemon::store().as_deref())
}

/// Writes the library the store keeps under `key`, and its dep-info, where `made` says, when
/// every file it was linked from has what it had. Whether it did.
pub(crate) fn restored(key: &[u8; 32], made: &Made<'_>) -> bool {
    let Some(kept) = store().and_then(|it| it.get(key)).and_then(|it| Kept::read(&it)) else {
        return false;
    };
    if !kept.still() {
        return false;
    }
    let write = |path: &Path, bytes: &[u8], executable: bool| {
        let new = path.with_extension(format!("{}.new", std::process::id()));
        std::fs::write(&new, bytes)
            .and_then(|()| if executable { super::script::make_executable(&new) } else { Ok(()) })
            .and_then(|()| std::fs::rename(&new, path))
    };
    write(&made.exe, &kept.library, true)
        .and_then(|()| write(&made.dep_info, &kept.dep_info, false))
        .is_ok()
}

/// Keeps the library ra-jit linked as `made` says under `key`, with what each file it was
/// linked from has: the files of its dep-info, and each of `inputs`, the dep-info of a crate
/// it links with the directory its paths are from, and their files. Nothing is kept when one
/// of them is not there.
pub(crate) fn keep<'a>(
    key: &[u8; 32],
    made: &Made<'_>,
    inputs: impl Iterator<Item = (&'a Path, &'a Path)>,
) {
    let Some(store) = store() else { return };
    let (Ok(library), Ok(dep_info)) = (std::fs::read(&made.exe), std::fs::read(&made.dep_info))
    else {
        return;
    };
    let mut files = BTreeMap::new();
    let read = |files: &mut BTreeMap<PathBuf, Option<Vec<u8>>>, info: &[u8], from: &Path| {
        let info = crate::dep_info::DepInfo::parse(&String::from_utf8_lossy(info));
        for file in info.files {
            let path = from.join(&file);
            let digest = std::fs::read(&path).ok().map(|it| Sha256::digest(it).to_vec());
            files.insert(path, digest);
        }
    };
    let parent = made.dep_info.parent().unwrap_or(Path::new(""));
    read(&mut files, &dep_info, parent);
    for (info, from) in inputs {
        let Ok(bytes) = std::fs::read(info) else { return };
        files.insert(info.to_owned(), Some(Sha256::digest(&bytes).to_vec()));
        read(&mut files, &bytes, from);
    }
    let Some(files) = files.into_iter().map(|(path, it)| Some((path, it?))).collect() else {
        return;
    };
    let kept = Kept { library, dep_info, files };
    if kept.size() <= MOST_KEPT {
        store.settled();
        store.insert(key, &kept.bytes());
        store.flush();
    }
}

/// A library ra-jit linked, its dep-info, and each file it was linked from with the digest of
/// what it had: the library stands for another of the same key while they still have it.
struct Kept {
    library: Vec<u8>,
    dep_info: Vec<u8>,
    files: Vec<(PathBuf, Vec<u8>)>,
}

impl Kept {
    fn still(&self) -> bool {
        let digest = |path: &Path| std::fs::read(path).ok().map(|it| Sha256::digest(it).to_vec());
        self.files.iter().all(|(path, was)| digest(path).as_ref() == Some(was))
    }

    fn size(&self) -> usize {
        self.library.len() + self.dep_info.len()
    }

    fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut put = |bytes: &[u8]| {
            out.extend((bytes.len() as u64).to_le_bytes());
            out.extend(bytes);
        };
        put(LIBRARIES.as_bytes());
        put(&self.library);
        put(&self.dep_info);
        put(&(self.files.len() as u64).to_le_bytes());
        for (path, digest) in &self.files {
            put(path.as_os_str().as_encoded_bytes());
            put(digest);
        }
        out
    }

    fn read(bytes: &[u8]) -> Option<Kept> {
        let rest = &mut &*bytes;
        if take(rest)? != LIBRARIES.as_bytes() {
            return None;
        }
        let (library, dep_info) = (take(rest)?.to_vec(), take(rest)?.to_vec());
        let count = u64::from_le_bytes(take(rest)?.try_into().ok()?);
        let mut files = Vec::new();
        for _ in 0..count {
            let path = PathBuf::from(std::str::from_utf8(take(rest)?).ok()?);
            files.push((path, take(rest)?.to_vec()));
        }
        Some(Kept { library, dep_info, files })
    }
}

/// The next field of `rest`, which is its length and then its bytes.
fn take<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = usize::try_from(u64::from_le_bytes(rest.get(..8)?.try_into().ok()?)).ok()?;
    let field = rest.get(8..8usize.checked_add(len)?)?;
    *rest = &rest[8 + len..];
    Some(field)
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
        let Some(unit) = Record::id_of_file(&library, prefix, suffix) else { continue };
        let unit = &unit;
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
