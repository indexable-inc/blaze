//! The pinned native inputs of a build ([`blaze_units::query::NativeInput`], docs/cargo.md,
//! "Pinned native inputs"), fetched and laid out before any build script runs: each archive got
//! through blaze-fetch by its sha256 (from the store when it has it, else downloaded, checked and
//! kept there), unpacked with every entry checked, and laid out read-only at
//! `<BLAZE_HOME>/native/sha256-<hex>`, the one place a build script is pointed at.
//!
//! The layout is by digest, so one archive is one directory whatever names it, and an edit to
//! the pin is a new directory, never a change to one a build script was already given. It is
//! made under a lock in a directory of its own beside the final one, every file and directory
//! synced, its mark (`.blaze-native`) written last, and renamed into place whole. The mark says
//! the digest and lists every file and link the archive unpacked to, each with its kind, BLAKE3
//! and length ([`blaze_fetch::tree`]'s listing): before a daemon first hands a layout to a build
//! script it walks the layout and holds it to that listing, so a file changed since (its mode is
//! all that kept it) is a refusal naming the file, never a build against it.
//!
//! What is unpacked goes through blaze-fetch's one checked unpacker ([`blaze_fetch::unpack`],
//! which says what it holds every entry to) with links kept, names compared as a
//! case-insensitive disk compares them, and none named as the mark. The bytes unpacked are
//! hashed as they are read and held to the pinned sha256 before the layout is renamed into
//! place, so what was unpacked is what was pinned, whatever happened to the copy read from.
//! The xz stream's dictionary is bounded ([`XZ_MEMORY_KIB`]), as are the entries, bytes and
//! path lengths.
//!
//! Called on an IO thread of the plane ([`super::work::Steps::native`]), once per distinct list
//! of inputs: jobs that ask the same list share one plane node (`job.rs`, `native_key`). The
//! file lock is for other processes, and its wait looks at the job's cancellation each time.

#![forbid(unsafe_code)]

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use blaze_api::ApiError;
use blaze_core::lock::Lock;
use blaze_fetch::{
    tree,
    unpack::{self, Modes, NameRules, Rules, Twice},
};
use blaze_units::query::NativeInput;

/// The directory under blaze's home the inputs are laid out in.
pub(crate) const NATIVE: &str = "native";

/// The file, at the top of a layout, that says which digest it is of and lists what it holds.
/// Written last, so a layout with it is whole.
const MARK: &str = ".blaze-native";

/// The most bytes of files one archive may unpack to. LLVM's is a few GiB.
const MOST_BYTES: u64 = 16 << 30;

/// The most entries one archive may have.
const MOST_ENTRIES: u64 = 1 << 20;

/// The most bytes of one entry's path, or of a link's target.
const MOST_PATH: usize = 4096;

/// The most bytes of a mark: its listing of every file.
const MOST_MARK: u64 = 256 << 20;

/// The most memory the xz decoder may take, in KiB: its dictionary, as each block's header
/// says it, is refused past this before anything is allocated. `xz -9` takes 64 MiB.
const XZ_MEMORY_KIB: u32 = 256 << 10;

/// How long one layout waits for another process laying out the same digest, and how often it
/// looks: a download of a few hundred MB and its unpacking, on a slow line.
const LOCK_WAIT: Duration = Duration::from_secs(60 * 60);
const LOCK_POLL: Duration = Duration::from_millis(200);

/// One input laid out: the directory its build scripts are given.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NativeDir {
    pub input: NativeInput,
    /// `<home>/native/sha256-<hex>`, absolute, read-only, whole.
    pub dir: PathBuf,
}

/// The digests whose layouts this daemon has walked and held to their marks (or laid out
/// itself): each is walked once a daemon.
pub(crate) struct Verified(Lock<BTreeSet<String>>);

impl Default for Verified {
    fn default() -> Verified {
        Verified(Lock::new("serve.native.verified", BTreeSet::new()))
    }
}

/// Where the archive pinned by `sha256` is laid out under `native` (`<home>/native`).
pub(crate) fn dir_of(native: &Path, sha256: &str) -> PathBuf {
    native.join(format!("sha256-{sha256}"))
}

/// The first line of a mark for `sha256`.
fn mark_line(sha256: &str) -> String {
    format!("sha256:{sha256}")
}

/// How to remove `dir`, a layout made read-only, from a shell.
fn remove_hint(dir: &Path) -> String {
    let quoted = format!("'{}'", dir.display().to_string().replace('\'', r"'\''"));
    format!("remove it with `chmod -R u+w {quoted} && rm -rf {quoted}`")
}

/// An error about `input`, naming it, its target, URL, pinned digest and pin file.
fn refused(input: &NativeInput, what: &str) -> ApiError {
    ApiError::new(
        format!(
            "pinned native input `{}` for target `{}` ({}, sha256:{}, pinned in {}): {what}",
            input.name, input.target, input.url, input.sha256, input.pin
        ),
        Some(input.pin.clone()), // clone: an error's place, once
    )
}

/// `input` laid out under `native` (`<home>/native`), fetched through `fetcher` when it is not
/// there already: its directory. A wait for another process stops once `cancelled` is set.
/// The daemon asks it once a digest at a time (one task a digest, every job joining it), so
/// the file lock is only ever waited on for another process.
pub(crate) fn ensure(
    input: &NativeInput,
    native: &Path,
    fetcher: &blaze_fetch::Fetcher,
    verified: &Verified,
    cancelled: &AtomicBool,
) -> Result<PathBuf, ApiError> {
    let dir = dir_of(native, &input.sha256);
    if laid_out(input, &dir, verified)? {
        return Ok(dir);
    }
    let failed_io = |what: &str, error: io::Error| refused(input, &format!("{what}: {error}"));
    fs::create_dir_all(native)
        .map_err(|e| failed_io(&format!("creating {}", native.display()), e))?;
    let lock_path = native.join(format!(".sha256-{}.lock", input.sha256));
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| failed_io(&format!("opening {}", lock_path.display()), e))?;
    let _lock = lock_within(input, lock, &lock_path, cancelled)?;
    // One that held the lock before may have laid it out.
    if laid_out(input, &dir, verified)? {
        return Ok(dir);
    }
    let _span = tracing::info_span!(
        target: "blaze",
        "lay out pinned native input",
        input = %input.name,
        target_triple = %input.target,
        url = %input.url,
    )
    .entered();
    // What a layout of this digest that stopped left, by this process or any other: the lock
    // is held, so none is being written.
    remove_stale(native, &input.sha256).map_err(|e| {
        failed_io(&format!("removing what a layout that stopped left in {}", native.display()), e)
    })?;
    let digest = blaze_fetch::Digest::parse(&format!("sha256:{}", input.sha256))
        .map_err(|e| refused(input, &format!("the pinned sha256 is not a digest: {e}")))?;
    // clone: once an input
    let source = blaze_fetch::Source::Url { url: input.url.clone(), digest };
    let object = fetcher.get(&source).map_err(|e| refused(input, &format!("fetching it: {e}")))?;
    let downloads = native.join(".downloads");
    let archive = fetcher
        .materialize(&object, &downloads)
        .map_err(|e| refused(input, &format!("writing it out: {e}")))?;
    let part = native.join(format!(".sha256-{}.part-{}", input.sha256, std::process::id()));
    let made = lay_out(input, &digest, &archive, &part, &dir, native);
    // The archive stays in the store, by its digest; this copy was only to read it from.
    _ = fs::remove_file(&archive);
    if let Err(why) = made {
        _ = unpack::remove_all(&part);
        return Err(refused(input, &format!("unpacking {}: {why}", archive.display())));
    }
    verified.0.lock().insert(input.sha256.clone()); // clone: once a digest a daemon
    tracing::info!(target: "blaze", input = %input.name, dir = %dir.display(), "laid out pinned native input");
    Ok(dir)
}

/// The archive at `archive` unpacked into `part`, held to `digest` as it is read, marked,
/// synced, sealed and renamed to `dir`, under `native`.
fn lay_out(
    input: &NativeInput,
    digest: &blaze_fetch::Digest,
    archive: &Path,
    part: &Path,
    dir: &Path,
    native: &Path,
) -> Result<(), String> {
    fs::create_dir(part).map_err(|e| format!("creating {}: {e}", part.display()))?;
    let file = File::open(archive).map_err(|e| format!("opening {}: {e}", archive.display()))?;
    let mut raw = HashingRead { inner: file, hashing: blaze_fetch::Hashing::new(digest.algo()) };
    let mut unpacked = {
        let mut xz = lzma_rust2::XzReader::new_mem_limit(
            BufReader::with_capacity(1 << 20, &mut raw),
            false,
            XZ_MEMORY_KIB,
        );
        let rules = Rules {
            names: NameRules {
                top: None,
                fold: true,
                twice: Twice::Refuse,
                reserved: &[MARK],
                most_path: MOST_PATH,
            },
            links: true,
            modes: Modes::ReadOnly,
            skip: &[],
            include: &every,
            sync: true,
            most_entries: MOST_ENTRIES,
            most_bytes: MOST_BYTES,
        };
        let unpacked = unpack::unpack(&mut xz, part, &rules).map_err(|e| e.to_string())?;
        // The rest of the stream, so its checks (its index, its check of what was read) run.
        io::copy(&mut xz, &mut io::sink()).map_err(|e| format!("reading the archive: {e}"))?;
        unpacked
    };
    // And whatever follows it, so every byte of the file is hashed.
    io::copy(&mut raw, &mut io::sink()).map_err(|e| format!("reading the archive: {e}"))?;
    let got = raw.hashing.finish().digest;
    if got != *digest {
        return Err(format!(
            "the bytes unpacked are {got}, not the pinned {digest}: the copy changed as it was read"
        ));
    }
    let mut mark = mark_line(&input.sha256).into_bytes();
    mark.push(b'\n');
    mark.extend(
        tree::listing(&mut unpacked.entries)
            .map_err(|e| format!("listing what it unpacked to: {e}"))?,
    );
    if mark.len() as u64 > MOST_MARK {
        return Err(format!("its listing is {} bytes, more than {MOST_MARK}", mark.len()));
    }
    let mark_path = part.join(MARK);
    // New, so nothing the archive left there (none is let through) is written through.
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&mark_path)
        .map_err(|e| format!("creating {}: {e}", mark_path.display()))?;
    out.write_all(&mark).map_err(|e| format!("writing {}: {e}", mark_path.display()))?;
    out.sync_all().map_err(|e| format!("syncing {}: {e}", mark_path.display()))?;
    drop(out);
    seal(&mark_path).map_err(|e| format!("making {} read-only: {e}", mark_path.display()))?;
    unpack::sync_dir(part).map_err(|e| format!("syncing {}: {e}", part.display()))?;
    // Sealed after it moves: macOS refuses to rename a read-only directory even within one
    // parent (EACCES on macOS 15.8 APFS, the CI runners'), which Linux allows. Its mark is
    // sealed and synced already, so what a crash between the two leaves is a whole layout
    // that is still writable.
    fs::rename(part, dir)
        .map_err(|e| format!("moving {} to {}: {e}", part.display(), dir.display()))?;
    unpack::seal_dir(dir).map_err(|e| format!("making {} read-only: {e}", dir.display()))?;
    unpack::sync_dir(native).map_err(|e| format!("syncing {}: {e}", native.display()))
}

fn every(_: &Path) -> bool {
    true
}

/// A reader that hashes what is read through it.
struct HashingRead<R> {
    inner: R,
    hashing: blaze_fetch::Hashing,
}

impl<R: Read> Read for HashingRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.hashing.update(&buf[..read]);
        Ok(read)
    }
}

/// `file` made read-only.
fn seal(file: &Path) -> io::Result<()> {
    let mut perms = fs::metadata(file)?.permissions();
    perms.set_readonly(true);
    fs::set_permissions(file, perms)
}

/// Every `.sha256-<sha256>.part-*` under `native` removed.
fn remove_stale(native: &Path, sha256: &str) -> io::Result<()> {
    let prefix = format!(".sha256-{sha256}.part-");
    for entry in fs::read_dir(native)? {
        let entry = entry?;
        if entry.file_name().to_str().is_some_and(|it| it.starts_with(&prefix)) {
            unpack::remove_all(&entry.path())?;
        }
    }
    Ok(())
}

/// Whether `dir` is `input`'s layout, whole and as it was laid out. A directory there without
/// the mark of its digest, or (walked once a daemon) holding other than its mark lists, is
/// refused, naming what is wrong and how to remove it.
fn laid_out(input: &NativeInput, dir: &Path, verified: &Verified) -> Result<bool, ApiError> {
    // Looked at on the disk by every job, and counted: no watch vouches for a layout
    // (`blaze_vfs::outside::looks`).
    blaze_vfs::outside::looked(dir);
    match fs::symlink_metadata(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(refused(input, &format!("reading {}: {error}", dir.display()))),
        Ok(meta) if !meta.is_dir() => {
            return Err(refused(
                input,
                &format!("{} is there and is no directory; {}", dir.display(), remove_hint(dir)),
            ));
        }
        Ok(_) => {}
    }
    let mark_path = dir.join(MARK);
    let mark = match read_mark(&mark_path) {
        Ok(it) => it,
        Err(error) => {
            return Err(refused(
                input,
                &format!(
                    "{} is there without a mark {} it can read ({error}): blaze did not lay it out; {}",
                    dir.display(),
                    mark_path.display(),
                    remove_hint(dir)
                ),
            ));
        }
    };
    let changed = |what: String| {
        refused(
            input,
            &format!("{what}: the layout was changed since it was laid out; {}", remove_hint(dir)),
        )
    };
    let (line, listing) =
        mark.split_at(mark.iter().position(|it| *it == b'\n').map_or(mark.len(), |at| at + 1));
    if line.strip_suffix(b"\n") != Some(mark_line(&input.sha256).as_bytes()) {
        return Err(changed(format!(
            "{} says `{}`, not this digest",
            mark_path.display(),
            String::from_utf8_lossy(line).trim()
        )));
    }
    if verified.0.lock().contains(&input.sha256) {
        return Ok(true);
    }
    // Every byte of the layout hashed again, once a digest a daemon: the span says how many
    // and how long, so a slow first build names it. The hash is the check; nothing cheaper
    // (sizes, times) stands in for it.
    let span = tracing::info_span!(
        target: "blaze",
        "check pinned native input layout",
        input = %input.name,
        dir = %dir.display(),
        files = tracing::field::Empty,
        bytes = tracing::field::Empty,
    );
    let _entered = span.enter();
    let want = tree::parse(listing)
        .map_err(|e| changed(format!("{} lists nothing blaze reads: {e}", mark_path.display())))?;
    let got = unpack::walk(dir, &[MARK], true)
        .map_err(|e| changed(format!("in {}, {e}", dir.display())))?;
    span.record("files", got.len() as u64);
    span.record("bytes", got.iter().map(|it| it.len).sum::<u64>());
    if let Some((path, how)) = unpack::first_difference(&got, &want) {
        return Err(changed(format!("{} {how}", dir.join(path).display())));
    }
    verified.0.lock().insert(input.sha256.clone()); // clone: once a digest a daemon
    Ok(true)
}

/// The mark at `path`, at most [`MOST_MARK`] bytes.
fn read_mark(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new(); // alloc: a mark, once a layout checked
    File::open(path)?.take(MOST_MARK + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MOST_MARK {
        return Err(io::Error::other(format!("it is more than {MOST_MARK} bytes")));
    }
    Ok(bytes)
}

/// `lock` taken, waiting at most [`LOCK_WAIT`] for another process laying out the same input,
/// and no longer once `cancelled` is set.
fn lock_within(
    input: &NativeInput,
    lock: File,
    path: &Path,
    cancelled: &AtomicBool,
) -> Result<stdx::file_lock::Locked, ApiError> {
    let mut lock = lock;
    let start = Instant::now();
    let mut span = None;
    loop {
        match stdx::file_lock::Locked::try_lock(lock) {
            Ok(Ok(locked)) => return Ok(locked),
            Ok(Err(file)) => lock = file,
            Err(error) => {
                return Err(refused(input, &format!("locking {}: {error}", path.display())));
            }
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(refused(
                input,
                &format!("cancelled while another process held {} laying it out", path.display()),
            ));
        }
        let waited = start.elapsed();
        if waited >= LOCK_WAIT {
            return Err(refused(
                input,
                &format!(
                    "another process (another blaze daemon; this one lays out a digest once) has held {} for {waited:?} \
                     laying it out; it is still not done",
                    path.display()
                ),
            ));
        }
        span.get_or_insert_with(|| {
            tracing::info_span!(
                target: "blaze",
                "waits for another process laying out a pinned native input",
                input = %input.name,
                lock = %path.display(),
            )
            .entered()
        });
        wait_out(LOCK_POLL);
    }
}

/// Holds this IO thread for one look at a lock another process holds: the driver's timer.
#[allow(
    clippy::disallowed_methods,
    reason = "an IO thread's poll of a file lock another process holds, bounded by LOCK_WAIT and the job's cancellation"
)]
fn wait_out(wait: Duration) {
    std::thread::sleep(wait);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries_of(dir: &Path) -> Vec<tree::Entry> {
        unpack::walk(dir, &[MARK], true).unwrap()
    }

    /// A layout whose file was made writable and changed after it was laid out is refused,
    /// naming the file, once the daemon first walks it.
    #[cfg(unix)]
    #[test]
    fn a_changed_layout_is_refused_naming_the_file() {
        use std::os::unix::fs::PermissionsExt;
        let input = NativeInput {
            name: "llvm".to_owned(),
            target: "x".to_owned(),
            env: "LLVM_PREFIX".to_owned(),
            packages: vec!["llvm-sys".to_owned()],
            url: "https://example.com/a.tar.xz".to_owned(),
            sha256: "0".repeat(64),
            pin: "llvm.json".to_owned(),
        };
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path().join("layout");
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::write(dir.join("bin/tool"), b"ok").unwrap();
        fs::set_permissions(dir.join("bin/tool"), fs::Permissions::from_mode(0o555)).unwrap();
        let mut mark = format!("{}\n", mark_line(&input.sha256)).into_bytes();
        mark.extend(tree::listing(&mut entries_of(&dir)).unwrap());
        fs::write(dir.join(MARK), &mark).unwrap();
        assert!(laid_out(&input, &dir, &Verified::default()).unwrap());
        fs::set_permissions(dir.join("bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("bin/tool"), b"changed").unwrap();
        fs::set_permissions(dir.join("bin/tool"), fs::Permissions::from_mode(0o555)).unwrap();
        let error = laid_out(&input, &dir, &Verified::default()).unwrap_err();
        assert!(error.what.contains("bin/tool"), "{}", error.what);
        assert!(error.what.contains("chmod -R u+w"), "{}", error.what);
    }
}
