//! The source of the standard library blaze compiles: `library/` of the repository this blaze
//! was built from (docs/toolchain.md, "The standard library: library/"), the Rust project's
//! at the commit of blaze's nightly with blaze's changes to it. It is known by
//! its digest (`library/DIGEST`, written by `harness/library.py`), which is what the store keys
//! the standard library by, and it is the only source of the standard library blaze reads: no
//! toolchain's `rust-src` is.
//!
//! Where it is, found with nothing run or fetched ([`found`]), the first of:
//!
//! - the directory [`VAR`] names, for a process started by blaze, which says where it found
//!   it, or a person trying a changed library (its own `DIGEST` is then the one used);
//! - `share/blaze/library/` in a directory above the program (Nix's `$out`);
//! - blaze's own copy, `<machine>/library/<digest>/library` (`serve::machine`, once a machine), which [`fetch`] makes once from
//!   the checkout this blaze was built in while it has the same library, else downloaded as
//!   the release's `library-<digest>.tar.xz`. It is checked file by file against the digest
//!   whichever way it came.
//!
//! Every shipped tree [`place`]d has its texts beside it, made once a machine as it is placed
//! (or by the first daemon to mount a tree an older blaze placed, [`write_texts`]): one mapped
//! file a daemon mounts the tree from with no file of the tree read ([`texts`], [`open_texts`]).
//!
//! It is never read in a checkout: what blaze stores of a crate whose data names the checkout
//! built is that checkout's alone, and the standard library's is everyone's.

use std::{
    io::Read as _,
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_registry::http::HyperHttp;

#[blaze::sans_io]
pub(crate) mod texts;

/// The variable that names the library's directory to a process blaze starts.
pub const VAR: &str = "BLAZE_LIBRARY";

/// The variable that names where [`fetch`] gets the library from in place of the checkout or
/// the release: a release's address or a directory with its `library-<digest>.tar.xz`, or a
/// library to copy.
const FROM_VAR: &str = "BLAZE_LIBRARY_FROM";

/// The file of the library that has its digest, and is not part of what it is a digest of.
const DIGEST_FILE: &str = "DIGEST";

/// The digest of the library this blaze was built with.
const BUILT_WITH: &str = include_str!("../../../../library/DIGEST");

/// What the library this blaze was built with is a copy of (`library/UPSTREAM`).
const UPSTREAM: &str = include_str!("../../../../library/UPSTREAM");

/// How many times the archive is asked for before the download gives up.
const TRIES: u32 = 4;

/// The digest of the library this blaze was built with.
pub fn digest() -> &'static str {
    BUILT_WITH.trim()
}

/// The commit of rust-lang/rust the library is a copy of, from `UPSTREAM`.
pub fn upstream_commit(upstream: &str) -> Option<&str> {
    upstream.lines().find_map(|it| it.strip_prefix("commit ")).map(str::trim)
}

/// A library blaze can read: its directory, its digest and the commit it is a copy of.
#[derive(Clone, Debug)]
pub struct Library {
    pub dir: PathBuf,
    pub digest: String,
    pub commit: String,
}

impl Library {
    /// The library at `dir`, when it has a digest and a commit. It is in a directory named
    /// `library`, as in rust-lang/rust, which the paths of its files are named from.
    fn at(dir: PathBuf) -> Option<Library> {
        if dir.file_name().is_none_or(|it| it != "library") {
            return None;
        }
        let digest = std::fs::read_to_string(dir.join(DIGEST_FILE)).ok()?.trim().to_owned();
        let upstream = std::fs::read_to_string(dir.join("UPSTREAM")).ok()?;
        let commit = upstream_commit(&upstream)?.to_owned();
        let whole = dir.join("Cargo.toml").is_file() && dir.join("core/src/lib.rs").is_file();
        (whole && !digest.is_empty()).then_some(Library { dir, digest, commit })
    }

    /// The library at `dir` when it is the one this blaze was built with.
    fn ours_at(dir: PathBuf) -> Option<Library> {
        Library::at(dir).filter(|it| it.digest == digest())
    }
}

#[expect(
    clippy::disallowed_types,
    reason = "where this blaze's standard library is: looked for on disk once, then fixed for the process"
)]
static FOUND: std::sync::OnceLock<Result<Library, String>> = std::sync::OnceLock::new();

/// What [`fetch`] downloaded, when the library was not found before.
#[expect(
    clippy::disallowed_types,
    reason = "the standard library this process fetched, once, then fixed for the process"
)]
static FETCHED: std::sync::OnceLock<Library> = std::sync::OnceLock::new();

/// Where the library is, found with nothing fetched (or fetched by this process); `Err` says
/// why there is none.
pub fn found() -> Result<&'static Library, String> {
    if let Some(fetched) = FETCHED.get() {
        return Ok(fetched);
    }
    FOUND.get_or_init(look).as_ref().map_err(Clone::clone)
}

/// The library's directory, when [`found`].
pub fn dir() -> Option<&'static Path> {
    found().ok().map(|it| it.dir.as_path())
}

/// Whether `path` is a file of the library.
pub fn has(path: &Path) -> bool {
    dir().is_some_and(|dir| path.starts_with(dir))
}

fn look() -> Result<Library, String> {
    if let Some(named) = std::env::var_os(VAR).filter(|it| !it.is_empty()) {
        let dir = PathBuf::from(&named);
        return Library::at(dir.clone()).ok_or_else(|| {
            format!(
                "{VAR} names {}, which is not a standard library blaze can read: it is a \
                 directory named `library` with `Cargo.toml`, `core/src/lib.rs`, `UPSTREAM` and \
                 `DIGEST` (`python3 \
                 harness/library.py digest '{{}}'` writes it)",
                dir.display()
            )
        });
    }
    candidates()?
        .into_iter()
        .find_map(Library::ours_at)
        .ok_or_else(|| format!("blaze's standard library (digest {}) is not here", short()))
}

/// Where a copy of the library this blaze was built with can be, with nothing fetched.
fn candidates() -> Result<Vec<PathBuf>, String> {
    let exe = std::env::current_exe().and_then(std::fs::canonicalize).map_err(|error| {
        format!("finding blaze's own program, beside which its standard library can be: {error}")
    })?;
    let mut dirs: Vec<PathBuf> =
        exe.ancestors().skip(1).take(3).map(|it| it.join("share/blaze/library")).collect();
    dirs.extend(own());
    Ok(dirs)
}

/// blaze's own copy of the library: `<machine>/library/<digest>/library`
/// ([`crate::serve::machine`]), in a directory of its own, so that every file under the one it
/// is in is the library's. Once a machine, never under a home: the sealed tree every home's
/// crates of it are read from, and what tells them sealed (`serve::sealed::Placed`).
fn own() -> Option<PathBuf> {
    Some(tree_dir("library", digest()).ok()?.join("library"))
}

/// The `DIGEST` file of `library` when it is blaze's own copy ([`own`]): a tree blaze placed
/// from a digest-checked source and that no one else writes, so that file's stamp, unchanged,
/// stands for every file under it (the daemon's `serve::local::LibraryMark`). `None` for a
/// library [`VAR`] names or one beside the program: a developer's tree, walked file by file.
pub fn owned(library: &Library) -> Option<PathBuf> {
    let own = own()?;
    library.dir.starts_with(&own).then(|| library.dir.join(DIGEST_FILE))
}

/// The library of the checkout this blaze was built in.
fn checkout() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library")
}

fn short() -> &'static str {
    &digest()[..digest().len().min(12)]
}

/// [`found`], else blaze's own copy, made in `<machine>/library` once: from where [`FROM_VAR`]
/// names, else copied from the checkout this blaze was built in while it has the same library,
/// else downloaded from the release this blaze is (the tag of its version, `v0.YYYYMMDD.N`).
/// `Err` says what failed and what to do.
pub fn fetch() -> Result<Library, String> {
    if let Ok(found) = found() {
        return Ok(found.clone());
    }
    let dir = own().ok_or_else(|| {
        "blaze keeps its standard library once a machine, in the directory its daemons share \
         (`XDG_CACHE_HOME`, else `.cache` in your home directory), and none could be named: \
         `XDG_CACHE_HOME` names where to keep it"
            .to_owned()
    })?;
    let given = super::given_var(FROM_VAR).map_err(|it| it.to_string())?;
    let from = match (given, Library::ours_at(checkout())) {
        (Some(given), _) => given,
        (None, Some(_)) => checkout().to_string_lossy().into_owned(),
        // The release this blaze is: its version, which only the engine's build.rs knows
        // (`BLAZE_RELEASE`), so that no release changes this crate.
        (None, None) => format!("{}/v{}", crate::release::RELEASES, crate::release::get().version),
    };
    let fetched = fetch_into(&dir, &from, digest()).map_err(|e| {
        format!(
            "blaze compiles the standard library from its own source (digest {}), which it \
             gets once, and getting it from {from} failed: {e:#}. Running the command again \
             tries again; a checkout of blaze at the commit this blaze was built from has it in \
             library/, which `{VAR}` can name",
            short()
        )
    })?;
    Ok(FETCHED.get_or_init(|| fetched).clone())
}

/// The name of the archive of the library with `digest` among a release's assets.
pub fn archive_name(digest: &str) -> String {
    tree_archive("library", digest)
}

/// The name of the archive of the shipped tree `name` with `digest` among a release's assets:
/// `<name>-<digest>.tar.xz`, whose one top directory is `<name>`.
pub fn tree_archive(name: &str, digest: &str) -> String {
    format!("{name}-{digest}.tar.xz")
}

/// Where the shipped tree `name` with `digest` is placed on this machine: `<machine>/<name>/<digest>`,
/// the tree's own files in `<name>` under it, so that every file under the directory is the
/// tree's. Once a machine, never under a home. What a crate read from under it is told sealed
/// by (`serve::sealed::Placed`).
pub fn tree_dir(name: &str, digest: &str) -> Result<PathBuf, String> {
    let machine = crate::serve::machine().map_err(|error| format!("{error:#}"))?;
    Ok(machine.join(name).join(digest))
}

/// The shipped tree `name` with `digest` placed on this machine ([`tree_dir`]) unless it is
/// there: copied from `from` when that is the tree (its `DIGEST` says `digest`), else unpacked
/// from [`tree_archive`] in `from` (a release's address, or a directory that has it), else, with
/// no `from`, downloaded from the release this blaze is. Checked file by file against `digest`
/// whichever way it came ([`digest_of`]). The library is one such tree ([`fetch`]); the list of
/// the trees a toolchain ships is `cli::store::shipped`'s.
pub fn place(name: &str, digest: &str, from: Option<&str>) -> Result<PathBuf, String> {
    let dir = tree_dir(name, digest)?.join(name);
    let release = || format!("{}/v{}", crate::release::RELEASES, crate::release::get().version);
    let from = from.map_or_else(release, str::to_owned);
    fetch_tree(&dir, &from, name, digest).map_err(|error| format!("placing blaze's {name} {digest} from {from}: {error:#}"))?;
    Ok(dir)
}

/// The library with digest `want` in `dir`, unless it is there ([`fetch_tree`]), read as a
/// library.
fn fetch_into(dir: &Path, from: &str, want: &str) -> anyhow::Result<Library> {
    fetch_tree(dir, from, "library", want)?;
    let fetched = Library::at(dir.to_owned()).filter(|it| it.digest == want);
    fetched.ok_or_else(|| anyhow::anyhow!("{} has no library whose DIGEST says {want}", dir.display()))
}

/// The digest a tree at `dir` states (its `DIGEST`), when it states one.
fn stated(dir: &Path) -> Option<String> {
    let digest = std::fs::read_to_string(dir.join(DIGEST_FILE)).ok()?.trim().to_owned();
    (!digest.is_empty()).then_some(digest)
}

/// The tree `name` with digest `want` in `dir`, unless it is there: copied from `from` when that
/// is a tree stating `want`, else unpacked from its archive in `from` (a release's address, or a
/// directory that has it), checked against `want` either way. One way for every shipped tree.
fn fetch_tree(dir: &Path, from: &str, name: &str, want: &str) -> anyhow::Result<()> {
    let (Some(parent), Some(leaf)) = (dir.parent(), dir.file_name()) else {
        anyhow::bail!("{} has no place for it", dir.display());
    };
    let leaf = leaf.to_string_lossy();
    std::fs::create_dir_all(parent)?;
    // Held until the tree and its texts are made, and let go of then even while a child
    // spawned meanwhile still has the file open.
    let _lock = lock_tree(dir)?;
    if stated(dir).is_some_and(|it| it == want) {
        return texts_unless_there(dir, want);
    }
    let archive = tree_archive(name, want);
    let copied = Some(PathBuf::from(from)).filter(|it| stated(it).is_some_and(|it| it == want));
    let short = &want[..want.len().min(12)];
    // A copy says nothing: the first process may be one answering for rustc (`-vV`), whose
    // output is rustc's alone.
    let said = copied.is_none();
    if said {
        blaze_report::errln!("Downloading blaze's {name} (digest {short}, once) from {from}...");
    }
    let start = std::time::Instant::now();
    let staging = parent.join(format!(".{leaf}.unpacking"));
    _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let tree = staging.join(name);
    let unpacked = (|| {
        match &copied {
            Some(from) => copy_all(from, &tree)?,
            None => {
                let bytes = download(from, &archive)?;
                let mut xz = lzma_rust2::XzReader::new(std::io::Cursor::new(&bytes), false);
                tar::Archive::new(&mut xz).unpack(&staging)?;
                std::io::copy(&mut xz, &mut std::io::sink())?;
            }
        }
        let got = digest_of(&tree)?;
        if got != want {
            let what = copied.as_ref().map_or(archive.clone(), |it| it.display().to_string());
            anyhow::bail!(
                "{what} is not the {name} blaze was built with: its files' digest is {got} \
                 (`python3 harness/library.py digest '{{}}'` writes a changed library's DIGEST)"
            );
        }
        _ = std::fs::remove_dir_all(dir);
        std::fs::rename(&tree, dir)?;
        anyhow::Ok(())
    })();
    _ = std::fs::remove_dir_all(&staging);
    unpacked?;
    match stated(dir) {
        Some(it) if it == want => {}
        _ => anyhow::bail!("{archive} has no DIGEST that says {want}"),
    }
    texts_unless_there(dir, want)?;
    if said {
        blaze_report::errln!("Made blaze's {name} {short} in {:.1?}.", start.elapsed());
    }
    Ok(())
}

/// The lock of the shipped tree at `dir` (`<dir's parent>/.<dir's name>.lock`): held by
/// whoever places the tree or its texts.
fn lock_tree(dir: &Path) -> anyhow::Result<stdx::file_lock::Locked> {
    let (Some(parent), Some(leaf)) = (dir.parent(), dir.file_name()) else {
        anyhow::bail!("{} has no place for its lock", dir.display());
    };
    let lock = std::fs::File::create(parent.join(format!(".{}.lock", leaf.to_string_lossy())))?;
    Ok(stdx::file_lock::Locked::lock(lock)?)
}

/// The texts of the tree at `dir` ([`texts`]) made unless a file is at their name, the tree's
/// lock held. What is there is not read here: [`open_texts`] checks it where it is mapped.
fn texts_unless_there(dir: &Path, digest: &str) -> anyhow::Result<()> {
    let at = texts::name(dir)
        .ok_or_else(|| anyhow::anyhow!("{} has no place for its texts", dir.display()))?;
    if at.exists() {
        return Ok(());
    }
    make_texts(dir, digest).map_err(anyhow::Error::msg)
}

/// The texts file of the shipped tree at `tree` with `digest` made ([`texts`]), unless it is
/// there: what a daemon does once for a tree an older blaze placed, which has none. The tree's
/// lock is held, so two daemons never write it at once; one that finds it made by the other
/// reads it as [`open_texts`] does.
pub(crate) fn write_texts(tree: &Path, digest: &str) -> Result<(), String> {
    let _lock = lock_tree(tree).map_err(|error| format!("{}: {error:#}", tree.display()))?;
    if open_texts(tree, digest)?.is_some() {
        return Ok(());
    }
    make_texts(tree, digest)
}

/// The texts of the tree at `tree`, its lock held: every file under it read (a link is its
/// target's bytes and a linked directory its files, as the tree is mounted), the tree's digest
/// computed from the same reads as [`digest_of`] computes it and refused unless it is `digest`,
/// then [`texts::encode`]d and put in place by [`texts::Place`]'s steps, each synced.
fn make_texts(tree: &Path, digest: &str) -> Result<(), String> {
    let at = texts::name(tree).ok_or_else(|| format!("{} has no place for its texts", tree.display()))?;
    let _s = tracing::info_span!(target: "blaze", "shipped tree: write its texts", at = %at.display()).entered();
    // alloc: the tree's files and its digest's lines, once a tree a machine
    let (mut files, mut lines) = (Vec::new(), Vec::new());
    texts_walk(tree, "", &mut files, &mut lines)?;
    blaze_ids::names::sort_by_name(&mut lines, |it| it.0.as_bytes());
    let mut all = blaze_ids::b3::Hasher::new();
    for (_, line) in &lines {
        all.update(line.as_bytes());
    }
    let got = crate::content::hex(&all.finalize());
    if got != digest {
        return Err(format!(
            "the tree at {} is not the one of digest {digest} its texts are made for: its files' \
             digest is {got}. It is blaze's to write and no one else's; removing it makes blaze \
             place it again",
            tree.display()
        ));
    }
    blaze_ids::names::sort_by_name(&mut files, |it| it.0.as_bytes());
    // alloc: once a tree a machine, the walk's files lent to the encoder
    let borrowed: Vec<(String, blaze_vfs::Mode, &[u8])> =
        files.iter().map(|(path, mode, text)| (path.clone(), *mode, text.as_slice())).collect(); // clone: alloc: the paths, once a tree a machine
    let bytes = texts::encode(digest, &borrowed);
    drop(borrowed);
    drop(files);
    let mut place = texts::Place::new(at.clone(), bytes); // clone: the name, for its errors
    let named = |what: &Path, error: std::io::Error| format!("{}: {error}", what.display());
    for _ in 0..8 {
        match place.step() {
            texts::Step::Write { file, bytes } => {
                std::fs::write(&file, &bytes).map_err(|error| named(&file, error))?
            }
            texts::Step::Sync { file } => std::fs::File::open(&file)
                .and_then(|it| it.sync_all())
                .map_err(|error| named(&file, error))?,
            texts::Step::Rename { from, to } => {
                std::fs::rename(&from, &to).map_err(|error| named(&to, error))?
            }
            texts::Step::SyncDir { dir } => std::fs::File::open(&dir)
                .and_then(|it| it.sync_all())
                .map_err(|error| named(&dir, error))?,
            texts::Step::Done => {
                tracing::info!(target: "blaze", at = %at.display(), files = lines.len(), "a shipped tree's texts are made");
                return Ok(());
            }
        }
    }
    Err(format!("putting {} in place did not end within its five steps", at.display()))
}

/// Every file under `dir` read into `files` (path from the tree, mode, bytes), and each line of
/// the tree's digest ([`digest_of`]) into `lines` (path, line). A link is a `link` line of where
/// it points and its target's bytes as a file, or, for a link to a directory, its target's
/// files; `DIGEST` at the top has no line.
fn texts_walk(
    dir: &Path,
    prefix: &str,
    files: &mut Vec<(String, blaze_vfs::Mode, Vec<u8>)>,
    lines: &mut Vec<(String, String)>,
) -> Result<(), String> {
    let named = |what: &Path, error: std::io::Error| format!("{}: {error}", what.display());
    for entry in std::fs::read_dir(dir).map_err(|error| named(dir, error))? {
        let entry = entry.map_err(|error| named(dir, error))?;
        let full = entry.path();
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| format!("{}: a name that is not UTF-8", full.display()))?;
        let path = if prefix.is_empty() { name.to_owned() } else { format!("{prefix}/{name}") };
        let kind = entry.file_type().map_err(|error| named(&full, error))?;
        let meta = std::fs::metadata(&full).map_err(|error| named(&full, error))?;
        if kind.is_symlink() {
            let to = std::fs::read_link(&full).map_err(|error| named(&full, error))?;
            let to = to.to_string_lossy().into_owned().into_bytes();
            let hash = crate::content::hex(&blaze_ids::b3::hash(&to));
            lines.push((path.clone(), format!("link {hash} {path}\n"))); // clone: the line's key
        }
        if meta.is_dir() {
            texts_walk(&full, &path, files, lines)?;
            continue;
        }
        let bytes = std::fs::read(&full).map_err(|error| named(&full, error))?;
        if !kind.is_symlink() && path != DIGEST_FILE {
            let hash = crate::content::hex(&blaze_ids::b3::hash(&bytes));
            lines.push((path.clone(), format!("file {hash} {path}\n"))); // clone: the line's key
        }
        #[cfg(unix)]
        let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
        #[cfg(not(unix))]
        let executable = false;
        let mode = if executable { blaze_vfs::Mode::Executable } else { blaze_vfs::Mode::Regular };
        files.push((path, mode, bytes));
    }
    Ok(())
}

/// The texts of the shipped tree at `tree` with `digest`, mapped, and their listing: `None` when
/// no file is at their name ([`texts::name`]). A file there that [`texts::decode`] refuses is an
/// error naming it and why; its texts are not read here (the module doc of [`texts`] says why
/// a start trusts them).
pub(crate) fn open_texts(
    tree: &Path,
    digest: &str,
) -> Result<Option<(std::sync::Arc<blaze_core::Mapping>, texts::Listing)>, String> {
    let at = texts::name(tree).ok_or_else(|| format!("{} has no place for its texts", tree.display()))?;
    let mapping = match blaze_core::Mapping::map_file(&at) {
        Ok(mapping) => mapping,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("mapping {}: {error}", at.display())),
    };
    let listing = texts::decode(digest, mapping.bytes()).map_err(|refused| {
        format!(
            "the texts of the shipped tree at {}, {}, are refused: {refused}. blaze writes \
             that file once, by rename, from the tree checked against its digest, and no one \
             else writes it; removing it makes blaze write it again",
            tree.display(),
            at.display()
        )
    })?;
    // alloc: the mapping's one shared count, once a tree a mount
    Ok(Some((std::sync::Arc::new(mapping), listing)))
}

/// Copies the files, links and directories under `from` to `to`, which is made.
fn copy_all(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (kind, there) = (entry.file_type()?, to.join(entry.file_name()));
        if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &there)?;
            #[cfg(not(unix))]
            std::fs::copy(entry.path(), &there).map(drop)?;
        } else if kind.is_dir() {
            copy_all(&entry.path(), &there)?;
        } else {
            std::fs::copy(entry.path(), &there)?;
        }
    }
    Ok(())
}

fn download(from: &str, file: &str) -> anyhow::Result<Vec<u8>> {
    if !from.contains("://") {
        return Ok(std::fs::read(Path::new(from).join(file))?);
    }
    let address = super::base(from)?;
    let http = HyperHttp::shared()?;
    super::runtime()?.block_on(async {
        let mut tries = 0;
        loop {
            let got = super::read_from(&http, &address, file, 0)
                .await
                .and_then(|it| it.ok_or_else(|| anyhow::anyhow!("not there")));
            match got {
                Ok(bytes) => return Ok(bytes),
                // alloc: error path, the last failure.
                Err(e) if tries + 1 == TRIES => {
                    return Err(e.context(super::GaveUp { file: file.to_owned(), tries: TRIES }));
                }
                Err(_) => tries += 1,
            }
            tokio::time::sleep(Duration::from_secs(tries.into())).await;
        }
    })
}

/// The digest of the library at `dir`: the BLAKE3 hash of a line per file and link under it but
/// `DIGEST`, in the order of their paths' bytes, each `<kind> <BLAKE3 of its bytes, or of
/// where a link points> <path>`, kind `file` or `link`. `harness/library.py` writes the same.
/// Whether a file is executable is not in it: Windows does not say.
pub fn digest_of(dir: &Path) -> std::io::Result<String> {
    let mut found = Vec::new();
    list(dir, "", &mut found)?;
    blaze_ids::names::sort_by_name(&mut found, |it| it.1.as_bytes());
    let mut all = blaze_ids::b3::Hasher::new();
    for (kind, path, full) in found {
        let bytes = match kind {
            "link" => std::fs::read_link(&full)?.to_string_lossy().into_owned().into_bytes(),
            _ => {
                let mut bytes = Vec::new();
                std::fs::File::open(&full)?.read_to_end(&mut bytes)?;
                bytes
            }
        };
        let hash = crate::content::hex(&blaze_ids::b3::hash(&bytes));
        all.update(format!("{kind} {hash} {path}\n").as_bytes());
    }
    Ok(crate::content::hex(&all.finalize()))
}

fn list(
    dir: &Path,
    prefix: &str,
    found: &mut Vec<(&'static str, String, PathBuf)>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            found.push(("link", path, entry.path()));
        } else if kind.is_dir() {
            list(&entry.path(), &path, found)?;
        } else if path != DIGEST_FILE {
            found.push(("file", path, entry.path()));
        }
    }
    Ok(())
}

/// The commit the library this blaze was built with is a copy of.
pub fn built_commit() -> &'static str {
    upstream_commit(UPSTREAM).expect("library/UPSTREAM has a commit: harness/library.py import")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_library_is_the_one_its_digest_says() {
        assert_eq!(
            digest_of(&checkout()).unwrap(),
            digest(),
            "library/ changed: `python3 harness/library.py digest '{{}}'` writes its DIGEST"
        );
        assert_eq!(upstream_commit(UPSTREAM), Some(super::super::own::commit()), "the nightly's");
    }

    /// The invariant: a tree's texts are made only for the digest its files have, and are
    /// opened only as the texts of that digest; what is opened lists every file of the tree,
    /// `DIGEST` among them, each text its file's bytes. The control: the tree's own digest
    /// makes and opens them.
    #[test]
    fn texts_are_made_and_opened_only_for_the_digest_of_their_tree() {
        let work = tempfile::tempdir().unwrap();
        let tree = work.path().join("t/d/t");
        std::fs::create_dir_all(tree.join("src")).unwrap();
        std::fs::write(tree.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        std::fs::write(tree.join("Cargo.toml"), "[package]\n").unwrap();
        let digest = digest_of(&tree).unwrap();
        std::fs::write(tree.join(DIGEST_FILE), format!("{digest}\n")).unwrap();
        let other = "0".repeat(digest.len());
        let said = write_texts(&tree, &other).unwrap_err();
        assert!(said.contains("is not the one of digest"), "{said}");
        assert!(open_texts(&tree, &digest).unwrap().is_none(), "nothing is written for it");

        write_texts(&tree, &digest).unwrap();
        let (mapping, listing) = open_texts(&tree, &digest).unwrap().expect("made");
        let paths: Vec<&str> = listing.entries.iter().map(|it| it.path.as_str()).collect();
        assert_eq!(paths, ["Cargo.toml", "DIGEST", "src/lib.rs"]);
        for entry in &listing.entries {
            let text = &mapping.bytes()[entry.at as usize..(entry.at + entry.len) as usize];
            assert_eq!(text, std::fs::read(tree.join(&entry.path)).unwrap().as_slice());
        }
        let said = open_texts(&tree, &other).unwrap_err();
        assert!(said.contains("are refused") && said.contains("texts1"), "{said}");
    }

    #[test]
    fn a_library_is_found_by_its_digest_and_fetched_checked() {
        let work = tempfile::tempdir().unwrap();
        let make = |at: &Path, core: &str, digest: Option<&str>| {
            std::fs::create_dir_all(at.join("core/src")).unwrap();
            std::fs::write(at.join("Cargo.toml"), "[workspace]\n").unwrap();
            std::fs::write(at.join("core/src/lib.rs"), core).unwrap();
            std::fs::write(at.join("UPSTREAM"), "commit abc\n").unwrap();
            let digest = digest.map_or_else(|| digest_of(at).unwrap(), str::to_owned);
            std::fs::write(at.join(DIGEST_FILE), format!("{digest}\n")).unwrap();
            digest
        };
        let archived = |dir: &Path, digest: &str, dist: &Path| {
            let mut tar = tar::Builder::new(Vec::new());
            tar.append_dir_all("library", dir).unwrap();
            let mut xz = lzma_rust2::XzWriter::new(Vec::new(), Default::default()).unwrap();
            std::io::Write::write_all(&mut xz, &tar.into_inner().unwrap()).unwrap();
            std::fs::create_dir_all(dist).unwrap();
            std::fs::write(dist.join(archive_name(digest)), xz.finish().unwrap()).unwrap();
        };
        let one = work.path().join("one/library");
        let want = make(&one, "// core\n", None);
        let found = Library::at(one.clone()).unwrap();
        assert_eq!((found.digest.as_str(), found.commit.as_str()), (want.as_str(), "abc"));
        assert!(Library::ours_at(one.clone()).is_none(), "another library than blaze's");
        let elsewhere = work.path().join("elsewhere");
        make(&elsewhere, "// core\n", None);
        assert!(Library::at(elsewhere).is_none(), "its files are named from `library/`");
        // DIGEST is not part of what it is the digest of.
        assert_eq!(digest_of(&one).unwrap(), want);

        // An archive whose files are not what its digest says is refused, and nothing is kept.
        let lying = work.path().join("lying/library");
        make(&lying, "// another core\n", Some(&want));
        let (bad, good) = (work.path().join("bad"), work.path().join("good"));
        archived(&lying, &want, &bad);
        let into = work.path().join("home/library").join(&want).join("library");
        let said = fetch_into(&into, bad.to_str().unwrap(), &want).unwrap_err().to_string();
        assert!(said.contains("is not the library blaze was built with"), "{said}");
        assert!(!into.exists());

        // The library is fetched whole, and found there after with nothing fetched.
        archived(&one, &want, &good);
        let fetched = fetch_into(&into, good.to_str().unwrap(), &want).unwrap();
        assert_eq!((fetched.dir.as_path(), fetched.digest.as_str()), (into.as_path(), &*want));
        assert_eq!(digest_of(&into).unwrap(), want);
        std::fs::remove_dir_all(&good).unwrap();
        assert!(fetch_into(&into, good.to_str().unwrap(), &want).is_ok());

        // A checkout's library is copied, checked as an archive is.
        let copy = work.path().join("home/library/copy/library");
        let said = fetch_into(&copy, lying.to_str().unwrap(), &want).unwrap_err().to_string();
        assert!(said.contains("is not the library blaze was built with"), "{said}");
        let copied = fetch_into(&copy, one.to_str().unwrap(), &want).unwrap();
        assert_eq!(digest_of(&copied.dir).unwrap(), want);
    }
}
