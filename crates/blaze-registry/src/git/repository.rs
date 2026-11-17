//! The repository of a checkout as its files: the `.git` blaze keeps with a git package's tree.
//!
//! cargo's checkout of a git dependency is a clone, so a build script of a package in it that
//! asks git about its source gets an answer: `git rev-parse HEAD` is the locked commit, `git
//! status --porcelain` is empty, `git describe --always` and `git log -1` say what the commit
//! is (vergen's `Gitcl`, in reth-node-core's build script, which every project that depends on
//! reth by git runs). blaze keeps a package as its files, by content, and the clone it fetched
//! through is removed with the fetch, so the files alone left such a script "not a git
//! repository".
//!
//! What is kept is the least repository that answers for the commit, and the same bytes
//! wherever and whenever it is fetched, so the package's identity does not change with the
//! machine, the server's packs or the history fetched beside it:
//!
//! - `HEAD`, detached at the commit, and `refs/commit/<commit>`, the ref cargo's own fetch of
//!   a locked commit makes;
//! - the commit, every tree it reaches and the blob of each symlink as loose objects, each
//!   the object's own bytes in a zlib stream of stored blocks (no compressor's choices in the
//!   bytes);
//! - `shallow`, naming the commit when it has parents: its history is not here, and git says
//!   so (`git rev-parse --is-shallow-repository`) where cargo's clone has it;
//! - the index of the commit's tree, as `git read-tree` writes it at version 2 with no
//!   extension that depends on the machine: every entry's id and mode and no file's times, so
//!   git compares each file's content once and finds the tree clean.
//!
//! No file's blob is kept: the files are the checkout's, and git hashes a file to compare it.
//! A symlink's blob is kept, since git compares a link with its blob. A command that needs
//! another blob (`git show <commit>:<path>`, a diff of a changed file) fails naming the
//! object. A submodule's files are in the tree with no repository of their own.

use std::{
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context as _, bail};

use super::Checkout;

/// The index made for the kept repository, in the fetch's own clone, and the ids asked of
/// `git cat-file`: both go with the clone.
const INDEX: &str = "blaze-kept-index";
const ASKED: &str = "blaze-kept-objects";
/// The mode of a symlink's entry in a tree.
const SYMLINK: &[u8] = b"120000";

impl Checkout {
    /// The files of the repository kept with this checkout, each by its path under the
    /// checkout's root (`.git/...`) with its bytes, in no order. See the module.
    pub fn repository(&self) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
        files(&self.root, &self.rev)
    }
}

fn files(root: &Path, commit: &str) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    let dot_git = git_dir(root)?;
    let tree = String::from_utf8(git(root, &["rev-parse", "--verify", &format!("{commit}^{{tree}}")])?)
        .context("git named a tree that is not UTF-8")?;
    let mut ids = vec![commit.to_owned(), tree.trim().to_owned()];
    let mut links: Vec<String> = Vec::new();
    // `<mode> <type> <id>\t<path>\0`, every tree below the root among them (`-t`).
    let listed = git(root, &["ls-tree", "-r", "-t", "-z", "--full-tree", commit])?;
    for entry in listed.split(|it| *it == 0).filter(|it| !it.is_empty()) {
        let head = entry.split(|it| *it == b'\t').next().unwrap_or_default();
        let mut parts = head.split(|it| *it == b' ');
        let (Some(mode), Some(kind), Some(id)) = (parts.next(), parts.next(), parts.next()) else {
            bail!("`git ls-tree` printed `{}`, which is no entry", String::from_utf8_lossy(entry));
        };
        let id = || String::from_utf8(id.to_vec()).context("an object id that is not UTF-8");
        match (kind, mode) {
            (b"tree", _) => ids.push(id()?),
            // git compares a symlink with its blob, where it hashes a file.
            (b"blob", SYMLINK) => links.push(id()?),
            _ => {}
        }
    }
    links.sort_unstable();
    links.dedup();
    ids.extend(links.iter().cloned());
    ids.sort_unstable();
    ids.dedup();
    if let Some(id) = ids.iter().find(|it| !is_id(it)) {
        bail!("git named the object `{id}`, which is no object id");
    }

    let mut out = Vec::with_capacity(ids.len() + 5);
    let mut has_parents = false;
    for (id, kind, bytes) in objects(root, &dot_git, &ids)? {
        match (id == commit, kind.as_str()) {
            (true, "commit") => has_parents = header_lines(&bytes).any(|it| it.starts_with(b"parent ")),
            (false, "tree") => {}
            (false, "blob") if links.binary_search(&id).is_ok() => {}
            _ => bail!("the object `{id}` of the commit `{commit}` is a {kind}"),
        }
        let mut object = format!("{kind} {}\0", bytes.len()).into_bytes();
        object.extend_from_slice(&bytes);
        out.push((format!(".git/objects/{}/{}", &id[..2], &id[2..]), zlib_stored(&object)));
    }
    out.push((".git/HEAD".to_owned(), format!("{commit}\n").into_bytes()));
    out.push((format!(".git/refs/commit/{commit}"), format!("{commit}\n").into_bytes()));
    out.push((".git/config".to_owned(), config(commit).into_bytes()));
    if has_parents {
        out.push((".git/shallow".to_owned(), format!("{commit}\n").into_bytes()));
    }
    out.push((".git/index".to_owned(), index(root, &dot_git, commit)?));
    Ok(out)
}

/// The clone's own directory: `<root>/.git`, a directory as cargo's checkout has it.
fn git_dir(root: &Path) -> anyhow::Result<std::path::PathBuf> {
    let dot_git = root.join(".git");
    if !dot_git.is_dir() {
        bail!("the checkout `{}` has no `.git` directory to read its commit from", root.display());
    }
    Ok(dot_git)
}

fn is_id(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|it| matches!(it, b'0'..=b'9' | b'a'..=b'f'))
}

/// The header lines of a commit object: those before its first empty line.
fn header_lines(commit: &[u8]) -> impl Iterator<Item = &[u8]> {
    commit.split(|it| *it == b'\n').take_while(|it| !it.is_empty())
}

/// A repository's configuration: of SHA-1 objects, or of SHA-256 ones for a commit so named.
fn config(commit: &str) -> String {
    match commit.len() {
        64 => "[core]\n\trepositoryformatversion = 1\n\tfilemode = true\n\tbare = false\n[extensions]\n\tobjectformat = sha256\n".to_owned(),
        _ => "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n".to_owned(),
    }
}

/// Each object of `ids`, in their order, with its type: one `git cat-file --batch`, the ids
/// read from a file so that a pipe of them cannot fill while git's answers wait to be read.
fn objects(root: &Path, dot_git: &Path, ids: &[String]) -> anyhow::Result<Vec<(String, String, Vec<u8>)>> {
    let asked = dot_git.join(ASKED);
    std::fs::write(&asked, ids.iter().map(|it| format!("{it}\n")).collect::<String>())
        .with_context(|| format!("failed to write `{}`", asked.display()))?;
    let stdin = std::fs::File::open(&asked)
        .with_context(|| format!("failed to open `{}`", asked.display()))?;
    let mut command = command(root);
    command.args(["cat-file", "--batch"]).stdin(Stdio::from(stdin));
    let printed = run(&mut command)?;
    let mut rest = &printed[..];
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        // `<id> <type> <size>\n<bytes>\n`
        let end = rest.iter().position(|it| *it == b'\n').with_context(|| {
            format!("`git cat-file --batch` ended before the object `{id}`")
        })?;
        let head = std::str::from_utf8(&rest[..end]).context("an object header that is not UTF-8")?;
        let mut parts = head.split(' ');
        let (Some(named), Some(kind), Some(size)) = (parts.next(), parts.next(), parts.next()) else {
            bail!("`git cat-file --batch` said `{head}` of the object `{id}`");
        };
        let size: usize = size.parse().with_context(|| format!("the size in `{head}`"))?;
        let from = end + 1;
        if named != id || rest.len() < from + size + 1 {
            bail!("`git cat-file --batch` said `{head}` of the object `{id}`");
        }
        out.push((id.clone(), kind.to_owned(), rest[from..from + size].to_vec()));
        rest = &rest[from + size + 1..];
    }
    Ok(out)
}

/// The index of `commit`'s tree with no file's times in it: what `git read-tree` writes into
/// an index of its own, at version 2, with each setting that would add what depends on the
/// machine or on git's version off.
fn index(root: &Path, dot_git: &Path, commit: &str) -> anyhow::Result<Vec<u8>> {
    let at = dot_git.join(INDEX);
    let mut command = command(root);
    for setting in [
        "index.version=2",
        "index.skipHash=false",
        "index.sparse=false",
        "index.recordEndOfIndexEntries=false",
        "index.recordOffsetTable=false",
        "core.splitIndex=false",
        "core.untrackedCache=false",
        "core.fsmonitor=false",
        "feature.manyFiles=false",
    ] {
        command.args(["-c", setting]);
    }
    command.args(["read-tree", commit]).env("GIT_INDEX_FILE", &at);
    run(&mut command)?;
    std::fs::read(&at).with_context(|| format!("failed to read `{}`", at.display()))
}

fn git(root: &Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    run(command(root).args(args))
}

/// `git` on the fetch's clone at `root`, with none of the user's or the system's
/// configuration, as the clone was made.
#[expect(clippy::disallowed_methods, reason = "every `git` is run in the repository it is about")]
fn command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
        .env("GIT_NO_LAZY_FETCH", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .stdin(Stdio::null());
    command
}

fn run(command: &mut Command) -> anyhow::Result<Vec<u8>> {
    let output = command.output().context("could not run `git`")?;
    if !output.status.success() {
        bail!(
            "`git {}` failed ({}): {}",
            command.get_args().map(|it| it.to_string_lossy()).collect::<Vec<_>>().join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }
    Ok(output.stdout)
}

/// `data` as a zlib stream of stored blocks: the two header bytes of a 32 KiB window with no
/// preset dictionary, each block its length, the length's complement and its bytes, then
/// Adler-32 of the data. What any inflater reads, and the same bytes from every writer.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 0xFFFF;
    let blocks = data.len().div_ceil(BLOCK).max(1);
    let mut out = Vec::with_capacity(2 + data.len() + 5 * blocks + 4);
    out.extend_from_slice(&[0x78, 0x01]);
    let mut chunks = data.chunks(BLOCK).peekable();
    if chunks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(chunk) = chunks.next() {
        let len = chunk.len() as u16;
        out.push(u8::from(chunks.peek().is_none()));
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of the test's own, removed when it ends.
    struct Scratch(blaze_core::temp::TempDir);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            Scratch(blaze_core::temp::TempDir::for_test(&format!("blaze-kept-repository-{name}-")))
        }

        fn path(&self) -> &Path {
            self.0.path()
        }
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        let mut command = command(dir);
        command
            .args(args)
            .env("GIT_AUTHOR_NAME", "a")
            .env("GIT_AUTHOR_EMAIL", "a@localhost")
            .env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05Z")
            .env("GIT_COMMITTER_NAME", "a")
            .env("GIT_COMMITTER_EMAIL", "a@localhost")
            .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05Z");
        String::from_utf8(run(&mut command).unwrap()).unwrap().trim().to_owned()
    }

    /// A clone with two commits, the second checked out: a file, an executable, a file in a
    /// directory and, on unix, a symlink.
    fn clone(dir: &Path) -> Checkout {
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        sh(dir, &["init", "--quiet", "--template=", "--initial-branch=main"]);
        std::fs::write(dir.join("first"), "one\n").unwrap();
        sh(dir, &["add", "."]);
        sh(dir, &["commit", "--quiet", "-m", "first"]);
        std::fs::write(dir.join("build.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("src/deep/lib.rs"), "pub fn f() {}\n").unwrap();
        std::fs::write(dir.join("run.sh"), "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            std::os::unix::fs::symlink("src/deep/lib.rs", dir.join("link")).unwrap();
        }
        sh(dir, &["add", "."]);
        sh(dir, &["commit", "--quiet", "-m", "second, the one locked"]);
        Checkout { root: dir.to_owned(), rev: sh(dir, &["rev-parse", "HEAD"]) }
    }

    /// The checkout as blaze keeps it at `to`: the clone's files but its `.git`, read-only,
    /// and the kept repository's.
    fn keep(from: &Checkout, to: &Path, kept: &[(String, Vec<u8>)]) {
        for name in ["first", "build.rs", "src/deep/lib.rs", "run.sh"] {
            let at = to.join(name);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::copy(from.root.join(name), &at).unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink("src/deep/lib.rs", to.join("link")).unwrap();
        for (path, bytes) in kept {
            let at = to.join(path);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, bytes).unwrap();
        }
    }

    /// The invariant: in a checkout kept as its files and [`Checkout::repository`]'s, git
    /// answers for the locked commit as it does in the clone: `HEAD` is the commit, the tree
    /// is clean, and the commit's own words are read; the history is not there and git says
    /// the repository is shallow. A changed file is seen as changed. reth-node-core's build
    /// script (vergen's `Gitcl`: `git rev-parse --is-inside-work-tree`, `rev-parse HEAD`,
    /// `describe --always --tags --dirty`, `status --porcelain`) failed in blaze's checkout of
    /// reth, which had no `.git`, and stopped tempo's build.
    #[test]
    fn git_answers_for_the_commit_in_a_checkout_kept_as_files() {
        let dir = Scratch::new("answers");
        let from = clone(&dir.path().join("clone"));
        let kept = from.repository().unwrap();
        let to = dir.path().join("kept");
        keep(&from, &to, &kept);

        assert_eq!(sh(&to, &["rev-parse", "--is-inside-work-tree"]), "true");
        assert_eq!(sh(&to, &["rev-parse", "HEAD"]), from.rev);
        assert_eq!(sh(&to, &["status", "--porcelain"]), "");
        assert_eq!(sh(&to, &["describe", "--always", "--tags", "--dirty"]), sh(&from.root, &["describe", "--always", "--tags", "--dirty"]));
        for format in ["%H %T %an %ae %aI %cI", "%s"] {
            let log = ["log", "-1", &format!("--format={format}")];
            assert_eq!(sh(&to, &log), sh(&from.root, &log));
        }
        assert_eq!(sh(&to, &["ls-files"]), sh(&from.root, &["ls-files"]));
        assert_eq!(sh(&to, &["rev-parse", "--is-shallow-repository"]), "true");
        assert_eq!(sh(&to, &["rev-list", "--count", "HEAD"]), "1");

        // The control: git reads the kept files, so one that differs is seen.
        std::fs::write(to.join("first"), "two\n").unwrap();
        assert_eq!(sh(&to, &["status", "--porcelain"]), "M first");
        assert!(sh(&to, &["describe", "--always", "--dirty"]).ends_with("-dirty"));
    }

    /// The kept repository is the same bytes from a second clone of the same commit made at
    /// another time in another place: nothing of the clone's files' times, its packs or its
    /// path is in it.
    #[test]
    fn the_kept_repository_is_the_same_bytes_from_another_clone() {
        let dir = Scratch::new("bytes");
        let first = clone(&dir.path().join("a"));
        let again = dir.path().join("elsewhere/b");
        std::fs::create_dir_all(&again).unwrap();
        sh(&again, &["clone", "--quiet", first.root.to_str().unwrap(), "."]);
        sh(&again, &["gc", "--quiet"]);
        let again = Checkout { root: again, rev: first.rev.clone() };
        let (mut a, mut b) = (first.repository().unwrap(), again.repository().unwrap());
        a.sort();
        b.sort();
        assert_eq!(a.iter().map(|it| &it.0).collect::<Vec<_>>(), b.iter().map(|it| &it.0).collect::<Vec<_>>());
        assert!(a == b, "the same paths with other bytes");
    }

    #[test]
    fn a_zlib_stream_of_stored_blocks_inflates_to_its_data() {
        use std::io::Read as _;
        for len in [0usize, 1, 0xFFFE, 0xFFFF, 0x10000, 0x2_0001] {
            let data: Vec<u8> = (0..len).map(|it| (it * 31 % 251) as u8).collect();
            let mut back = Vec::new();
            flate2::read::ZlibDecoder::new(&zlib_stored(&data)[..]).read_to_end(&mut back).unwrap();
            assert!(back == data, "{len} bytes");
        }
    }
}
