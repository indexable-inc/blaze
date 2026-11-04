//! The pinned native inputs of a build ([`blaze_units::query::NativeInput`], docs/cargo.md,
//! "Pinned native inputs"), fetched and laid out before any build script runs: each archive got
//! through blaze-fetch by its sha256 (from the store when it has it, else downloaded, checked and
//! kept there), unpacked with every entry checked, and laid out read-only at
//! `<BLAZE_HOME>/native/sha256-<hex>`, the one place a build script is pointed at.
//!
//! The layout is by digest, so one archive is one directory whatever names it, and an edit to
//! the pin is a new directory, never a change to one a build script was already given. It is
//! made under a lock in a directory of its own beside the final one, its mark (`.blaze-native`,
//! the digest) written last, and renamed into place whole: a directory there is one this wrote
//! for that digest, or something else, which is refused naming it. What is unpacked is held to:
//!
//! - one top directory, stripped (as `llvm.py pack` writes it, and as Nix unpacks a tarball);
//! - no absolute path, no `..`, no `\` and no NUL in any name; none named `.blaze-native`;
//! - regular files, directories, symbolic links whose target stays inside the layout and never
//!   passes through another link of the archive, and hard links to a file the archive wrote
//!   before (copied); anything else (devices, FIFOs, sparse files) is refused;
//! - no entry under a link, and no path twice;
//! - at most [`MOST_ENTRIES`] entries, [`MOST_BYTES`] bytes of files and [`MOST_PATH`] bytes
//!   of any path.
//!
//! Called on an IO thread of the plane ([`super::work::Steps::native`]).

#![forbid(unsafe_code)]

use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{self, File},
    io::{self, BufReader, Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use blaze_api::ApiError;
use blaze_units::query::NativeInput;

/// The directory under blaze's home the inputs are laid out in.
pub(crate) const NATIVE: &str = "native";

/// The file, at the top of a layout, that says which digest it is of. Written last, so a
/// layout with it is whole.
const MARK: &str = ".blaze-native";

/// The most bytes of files one archive may unpack to. LLVM's is a few GiB.
const MOST_BYTES: u64 = 16 << 30;

/// The most entries one archive may have.
const MOST_ENTRIES: u64 = 1 << 20;

/// The most bytes of one entry's path, or of a link's target.
const MOST_PATH: usize = 4096;

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

/// Where the archive pinned by `sha256` is laid out under `native` (`<home>/native`).
pub(crate) fn dir_of(native: &Path, sha256: &str) -> PathBuf {
    native.join(format!("sha256-{sha256}"))
}

/// What a mark says for `sha256`.
fn mark_of(sha256: &str) -> String {
    format!("sha256:{sha256}\n")
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
/// there already.
pub(crate) fn ensure(input: &NativeInput, native: &Path, fetcher: &blaze_fetch::Fetcher) -> Result<NativeDir, ApiError> {
    let dir = dir_of(native, &input.sha256);
    let done = |dir: PathBuf| NativeDir { input: input.clone(), dir }; // clone: once an input a job
    if laid_out(input, &dir)? {
        return Ok(done(dir));
    }
    let failed_io = |what: &str, error: io::Error| refused(input, &format!("{what}: {error}"));
    fs::create_dir_all(native).map_err(|e| failed_io(&format!("creating {}", native.display()), e))?;
    let lock_path = native.join(format!(".sha256-{}.lock", input.sha256));
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| failed_io(&format!("opening {}", lock_path.display()), e))?;
    let _lock = lock_within(input, lock, &lock_path)?;
    // One that held the lock before may have laid it out.
    if laid_out(input, &dir)? {
        return Ok(done(dir));
    }
    let _span = tracing::info_span!(
        target: "blaze",
        "lay out pinned native input",
        input = %input.name,
        target_triple = %input.target,
        url = %input.url,
    )
    .entered();
    let digest = blaze_fetch::Digest::parse(&format!("sha256:{}", input.sha256))
        .map_err(|e| refused(input, &format!("the pinned sha256 is not a digest: {e}")))?;
    // clone: once an input
    let source = blaze_fetch::Source::Url { url: input.url.clone(), digest };
    let object = fetcher.get(&source).map_err(|e| refused(input, &format!("fetching it: {e}")))?;
    let downloads = native.join(".downloads");
    let archive = fetcher.materialize(&object, &downloads).map_err(|e| refused(input, &format!("writing it out: {e}")))?;
    let part = native.join(format!(".sha256-{}.part-{}", input.sha256, std::process::id()));
    remove_part(&part).map_err(|e| failed_io(&format!("removing {}, left by a layout that stopped", part.display()), e))?;
    let made = (|| {
        fs::create_dir(&part).map_err(|e| format!("creating {}: {e}", part.display()))?;
        let file = File::open(&archive).map_err(|e| format!("opening {}: {e}", archive.display()))?;
        unpack(file, &part)?;
        let mark = part.join(MARK);
        fs::write(&mark, mark_of(&input.sha256)).map_err(|e| format!("writing {}: {e}", mark.display()))?;
        seal(&mark, false).map_err(|e| format!("making {} read-only: {e}", mark.display()))?;
        fs::rename(&part, &dir).map_err(|e| format!("moving {} to {}: {e}", part.display(), dir.display()))?;
        // The top last: renamed while it could still be written, as some systems ask of a
        // directory that moves.
        seal_dir(&dir).map_err(|e| format!("making {} read-only: {e}", dir.display()))
    })();
    if let Err(why) = made {
        _ = remove_part(&part);
        return Err(refused(input, &format!("unpacking {}: {why}", archive.display())));
    }
    // The archive stays in the store, by its digest; this copy was only to read it from.
    _ = fs::remove_file(&archive);
    tracing::info!(target: "blaze", input = %input.name, dir = %dir.display(), "laid out pinned native input");
    Ok(done(dir))
}

/// Whether `dir` is `input`'s layout, whole. A directory there without the mark of its digest
/// is refused: blaze did not make it, or it was changed since.
fn laid_out(input: &NativeInput, dir: &Path) -> Result<bool, ApiError> {
    match fs::symlink_metadata(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(refused(input, &format!("reading {}: {error}", dir.display()))),
        Ok(meta) if !meta.is_dir() => {
            return Err(refused(input, &format!("{} is there and is no directory; remove it", dir.display())));
        }
        Ok(_) => {}
    }
    let mark = dir.join(MARK);
    match fs::read_to_string(&mark) {
        Ok(said) if said == mark_of(&input.sha256) => Ok(true),
        Ok(said) => Err(refused(
            input,
            &format!("{} says `{}`, not this digest: the layout was changed; remove {}", mark.display(), said.trim(), dir.display()),
        )),
        Err(error) => Err(refused(
            input,
            &format!("{} is there without its mark {} ({error}): blaze did not lay it out; remove it", dir.display(), mark.display()),
        )),
    }
}

/// `lock` taken, waiting at most [`LOCK_WAIT`] for another process laying out the same input.
fn lock_within(input: &NativeInput, lock: File, path: &Path) -> Result<stdx::file_lock::Locked, ApiError> {
    let mut lock = lock;
    let start = Instant::now();
    let mut span = None;
    loop {
        match stdx::file_lock::Locked::try_lock(lock) {
            Ok(Ok(locked)) => return Ok(locked),
            Ok(Err(file)) => lock = file,
            Err(error) => return Err(refused(input, &format!("locking {}: {error}", path.display()))),
        }
        let waited = start.elapsed();
        if waited >= LOCK_WAIT {
            return Err(refused(
                input,
                &format!("another blaze has held {} for {waited:?} laying it out; it is still not done", path.display()),
            ));
        }
        span.get_or_insert_with(|| {
            tracing::info_span!(
                target: "blaze",
                "waits for another blaze laying out a pinned native input",
                input = %input.name,
                lock = %path.display(),
            )
            .entered()
        });
        wait_out(LOCK_POLL);
    }
}

/// Holds this IO thread for one look at a lock another process holds: the driver's timer.
#[allow(clippy::disallowed_methods, reason = "an IO thread's poll of a file lock another process holds, bounded by LOCK_WAIT")]
fn wait_out(wait: Duration) {
    std::thread::sleep(wait);
}

/// `name` of an archive as a path under the layout: its top directory taken off. `top` is the
/// top directory every entry is under, set by the first. None for the top directory itself.
fn under_top(name: &[u8], top: &mut Option<OsString>) -> Result<Option<PathBuf>, String> {
    let shown = || String::from_utf8_lossy(name).into_owned();
    if name.len() > MOST_PATH {
        return Err(format!("an entry's path is {} bytes, more than {MOST_PATH}", name.len()));
    }
    if name.contains(&b'\\') || name.contains(&0) {
        return Err(format!("`{}` has a `\\` or a NUL in it", shown()));
    }
    let text = std::str::from_utf8(name).map_err(|_| format!("`{}` is not UTF-8", shown()))?;
    let mut parts = Vec::new(); // alloc: an entry's parts, once an entry
    for part in Path::new(text).components() {
        match part {
            Component::Normal(it) => parts.push(it.to_owned()),
            Component::CurDir => {}
            Component::ParentDir => return Err(format!("`{text}` has a `..` in it")),
            Component::RootDir | Component::Prefix(_) => return Err(format!("`{text}` is absolute")),
        }
    }
    let Some((first, rest)) = parts.split_first() else { return Err(format!("`{text}` names nothing")) };
    match top.as_ref() {
        Some(it) if it != first => {
            return Err(format!(
                "`{text}` is not under `{}`, the top directory of the entries before it: the archive is not one directory",
                it.to_string_lossy()
            ));
        }
        Some(_) => {}
        None => *top = Some(first.clone()),
    }
    if rest.is_empty() {
        return Ok(None);
    }
    if rest[0] == MARK {
        return Err(format!("`{text}` is named as blaze's mark `{MARK}`"));
    }
    Ok(Some(rest.iter().collect()))
}

/// Whether `target`, a symbolic link's at `at` (under the layout), stays inside the layout when
/// resolved from `at`'s directory, passing through none of `links` (every link of the archive)
/// on the way: a link through a link could climb out by a `..` the first one hides.
fn link_stays_in(at: &Path, target: &str, links: &BTreeSet<PathBuf>) -> bool {
    if target.is_empty() || target.len() > MOST_PATH || target.contains(['\\', '\0']) {
        return false;
    }
    let mut now: Vec<&std::ffi::OsStr> = at.parent().map(|it| it.iter().collect()).unwrap_or_default();
    let parts: Vec<Component<'_>> = Path::new(target).components().collect();
    for (i, part) in parts.iter().copied().enumerate() {
        match part {
            Component::Normal(it) => {
                now.push(it);
                let last = i + 1 == parts.len();
                if !last && links.contains(&now.iter().collect::<PathBuf>()) {
                    return false;
                }
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if now.pop().is_none() {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// The `.tar.xz` read from `from` unpacked into `into`, an empty directory, as the module says;
/// every file and directory read-only once written (the top directory is the caller's).
pub(crate) fn unpack(from: impl Read, into: &Path) -> Result<(), String> {
    let mut xz = lzma_rust2::XzReader::new(BufReader::with_capacity(1 << 20, from), false);
    let mut archive = tar::Archive::new(&mut xz);
    let mut top = None;
    let (mut entries, mut bytes) = (0u64, 0u64);
    // alloc: every path written, a set for the duplicate and through-a-link checks, once an archive
    let mut written: BTreeSet<PathBuf> = BTreeSet::new();
    let mut files: BTreeSet<PathBuf> = BTreeSet::new(); // alloc: as above
    let mut dirs: Vec<PathBuf> = Vec::new(); // alloc: as above
    // Links are made last, once every link of the archive is known ([`link_stays_in`]).
    let mut links: Vec<(PathBuf, String)> = Vec::new(); // alloc: as above
    let read = |e: io::Error| format!("reading the archive: {e}");
    for entry in archive.entries().map_err(read)? {
        let mut entry = entry.map_err(read)?;
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() {
            continue;
        }
        entries += 1;
        if entries > MOST_ENTRIES {
            return Err(format!("it has more than {MOST_ENTRIES} entries"));
        }
        let name = entry.path_bytes().into_owned(); // alloc: an entry's name, once an entry
        let Some(rel) = under_top(&name, &mut top)? else {
            if kind.is_dir() {
                continue;
            }
            return Err(format!("`{}`, the top of the archive, is no directory", String::from_utf8_lossy(&name)));
        };
        if !written.insert(rel.clone()) && !(kind.is_dir() && dirs.contains(&rel)) {
            return Err(format!("`{}` is in it twice", rel.display()));
        }
        let at = into.join(&rel);
        let parent_of = |at: &Path| -> Result<(), String> {
            let parent = at.parent().unwrap_or(into);
            fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))
        };
        if kind.is_dir() {
            fs::create_dir_all(&at).map_err(|e| format!("creating {}: {e}", at.display()))?;
            dirs.push(rel);
        } else if kind.is_file() || kind == tar::EntryType::Continuous {
            let size = entry.size();
            bytes = bytes.saturating_add(size);
            if bytes > MOST_BYTES {
                return Err(format!("its files are more than {MOST_BYTES} bytes"));
            }
            let exec = entry.header().mode().map_err(read)? & 0o111 != 0;
            parent_of(&at)?;
            let mut out =
                fs::OpenOptions::new().write(true).create_new(true).open(&at).map_err(|e| format!("creating {}: {e}", at.display()))?;
            let copied = io::copy(&mut (&mut entry).take(size), &mut out).map_err(|e| format!("writing {}: {e}", at.display()))?;
            if copied != size {
                return Err(format!("`{}` ends after {copied} of its {size} bytes", rel.display()));
            }
            out.flush().map_err(|e| format!("writing {}: {e}", at.display()))?;
            drop(out);
            seal(&at, exec).map_err(|e| format!("making {} read-only: {e}", at.display()))?;
            files.insert(rel);
        } else if kind.is_hard_link() {
            let target = entry.link_name_bytes().ok_or_else(|| format!("hard link `{}` names no target", rel.display()))?;
            let Some(from) = under_top(&target, &mut top)? else {
                return Err(format!("hard link `{}` is to the archive's top directory", rel.display()));
            };
            if !files.contains(&from) {
                return Err(format!(
                    "hard link `{}` is to `{}`, which is no file the archive wrote before it",
                    rel.display(),
                    from.display()
                ));
            }
            let source = into.join(&from);
            let size = fs::metadata(&source).map_err(|e| format!("reading {}: {e}", source.display()))?.len();
            bytes = bytes.saturating_add(size);
            if bytes > MOST_BYTES {
                return Err(format!("its files are more than {MOST_BYTES} bytes"));
            }
            parent_of(&at)?;
            fs::copy(&source, &at).map_err(|e| format!("copying {} to {}: {e}", source.display(), at.display()))?;
            files.insert(rel);
        } else if kind.is_symlink() {
            let target = entry.link_name_bytes().ok_or_else(|| format!("link `{}` names no target", rel.display()))?;
            let target = String::from_utf8(target.into_owned()).map_err(|_| format!("link `{}`'s target is not UTF-8", rel.display()))?;
            links.push((rel, target));
        } else {
            return Err(format!("`{}` is a {kind:?} entry, which is no file, directory or link", rel.display()));
        }
    }
    drop(archive);
    // The rest of the stream, so its checks (its index, its check of what was read) run.
    io::copy(&mut xz, &mut io::sink()).map_err(read)?;
    let link_set: BTreeSet<PathBuf> = links.iter().map(|(at, _)| at.clone()).collect(); // alloc: once an archive
    for path in &written {
        if let Some(link) = path.ancestors().skip(1).find(|it| link_set.contains(*it)) {
            return Err(format!("`{}` is under `{}`, a link", path.display(), link.display()));
        }
    }
    for (rel, target) in &links {
        if !link_stays_in(rel, target, &link_set) {
            return Err(format!(
                "link `{}` is to `{target}`, which is not inside the archive or goes through another link",
                rel.display()
            ));
        }
        let at = into.join(rel);
        let parent = at.parent().unwrap_or(into);
        fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
        symlink(target, &at).map_err(|e| format!("linking {} to `{target}`: {e}", at.display()))?;
    }
    // Deepest first, so each is written while its parent still can be.
    dirs.sort_by_key(|it| std::cmp::Reverse(it.components().count()));
    for dir in &dirs {
        let at = into.join(dir);
        seal_dir(&at).map_err(|e| format!("making {} read-only: {e}", at.display()))?;
    }
    // A directory made as a file's parent, which the archive has no entry for, is sealed too.
    seal_unlisted(into).map_err(|e| format!("making {} read-only: {e}", into.display()))?;
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &str, at: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, at)
}

#[cfg(not(unix))]
fn symlink(target: &str, at: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, format!("no symbolic link is made here (to `{target}` at {})", at.display())))
}

/// `file` made read-only, and executable when `exec`.
fn seal(file: &Path, exec: bool) -> io::Result<()> {
    let mut perms = fs::metadata(file)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(if exec { 0o555 } else { 0o444 });
    }
    #[cfg(not(unix))]
    {
        let _ = exec;
        perms.set_readonly(true);
    }
    fs::set_permissions(file, perms)
}

/// `dir` made read-only (and searchable).
fn seal_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555))
    }
    #[cfg(not(unix))]
    {
        // A read-only directory means something else on Windows; its files are read-only.
        let _ = dir;
        Ok(())
    }
}

/// Every directory under `dir` (not `dir` itself, not through a link) still writable, sealed.
fn seal_unlisted(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            let path = entry.path();
            seal_unlisted(&path)?;
            if !entry.metadata()?.permissions().readonly() {
                seal_dir(&path)?;
            }
        }
    }
    Ok(())
}

/// `part`, a layout that did not finish, removed: each directory made writable first, never
/// following a link.
fn remove_part(part: &Path) -> io::Result<()> {
    match fs::symlink_metadata(part) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(meta) if !meta.is_dir() => return fs::remove_file(part),
        Ok(_) => {}
    }
    writable(part)?;
    fs::remove_dir_all(part)
}

fn writable(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            writable(&entry.path())?;
        } else if !kind.is_symlink() && cfg!(not(unix)) {
            // Windows refuses to remove a read-only file.
            let mut perms = entry.metadata()?.permissions();
            #[allow(clippy::permissions_set_readonly_false, reason = "removing a layout that did not finish")]
            perms.set_readonly(false);
            fs::set_permissions(entry.path(), perms)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    /// A tar entry whose name and link target are written as given, with no check: what a
    /// hostile archive holds, which `tar::Builder` would refuse to write.
    fn raw(kind: tar::EntryType, name: &str, link: &str, data: &[u8]) -> (tar::Header, Vec<u8>) {
        let mut header = tar::Header::new_old();
        header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        header.as_old_mut().linkname[..link.len()].copy_from_slice(link.as_bytes());
        header.set_entry_type(kind);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        (header, data.to_vec())
    }

    fn tar_xz(entries: &[(tar::Header, Vec<u8>)]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (header, data) in entries {
            tar.append(header, data.as_slice()).unwrap();
        }
        let tarred = tar.into_inner().unwrap();
        let mut xz = lzma_rust2::XzWriter::new(Vec::new(), Default::default()).unwrap();
        xz.write_all(&tarred).unwrap();
        xz.finish().unwrap()
    }

    /// An entry that climbs out with `..`, one that is absolute, and a link out of the layout
    /// (directly, or through another link) are each refused, and nothing is written outside.
    #[cfg(unix)] // Its well-formed archive has a link, which only Unix lays out.
    #[test]
    fn unpack_refuses_paths_out_of_the_layout() {
        use tar::EntryType::{Directory, Regular, Symlink};
        let ok = || raw(Regular, "top/bin/tool", "", b"ok");
        let hostile: [(&str, Vec<(tar::Header, Vec<u8>)>); 5] = [
            ("`..`", vec![raw(Directory, "top/", "", b""), raw(Regular, "top/../escaped", "", b"x")]),
            ("absolute", vec![raw(Regular, "/tmp/escaped", "", b"x")]),
            ("link out", vec![ok(), raw(Symlink, "top/lib/out", "../../..", b"")]),
            ("link through a link", vec![ok(), raw(Symlink, "top/a/up", "..", b""), raw(Symlink, "top/b", "a/up/..", b"")]),
            ("under a link", vec![raw(Symlink, "top/l", ".", b""), raw(Regular, "top/l/x", "", b"x")]),
        ];
        for (what, entries) in hostile {
            let scratch = tempfile::tempdir().unwrap();
            let into = scratch.path().join("layout");
            fs::create_dir(&into).unwrap();
            let refused = unpack(tar_xz(&entries).as_slice(), &into);
            assert!(refused.is_err(), "{what}: unpacked");
            assert!(!scratch.path().join("escaped").exists(), "{what}: wrote outside");
            _ = remove_part(&into);
        }
        // The same shapes, kept inside, unpack.
        let scratch = tempfile::tempdir().unwrap();
        let into = scratch.path().join("layout");
        fs::create_dir(&into).unwrap();
        let fine = [ok(), raw(Symlink, "top/bin/alias", "tool", b"")];
        unpack(tar_xz(&fine).as_slice(), &into).unwrap();
        assert_eq!(fs::read(into.join("bin/alias")).unwrap(), b"ok");
        remove_part(&into).unwrap();
    }
}
