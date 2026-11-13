//! What a build was of, as its `builds` row names it: the digest of its workspace's source tree
//! (`tree`) and the hash of the blaze that ran it (`blaze`). `commit` alone misses a build of a
//! tree that is not a git checkout (a copied snapshot) or has edits on top of its commit; the
//! digest is `blaze-v2-proof tree`'s, so the tree a checkpoint commits can be checked against the
//! row of the build that tested it.

use std::{io, path::Path};

use blaze_v2_proof::tree;

/// The directories at a workspace's top its digest leaves out: build outputs and git's own.
pub const SKIPPED: [&str; 2] = ["target", ".git"];

/// The digest of the source tree at `workspace`, as 64 hex digits: every file's path, whether
/// it is executable and the BLAKE3 hash of its bytes, and every link, [`SKIPPED`] left out. A
/// file that can't be read is the error.
pub fn tree_digest(workspace: &Path) -> io::Result<String> {
    // alloc: once a build, on the history's thread
    let skip = SKIPPED.map(str::to_owned);
    Ok(hex(&tree::digest(workspace, &skip)?))
}

/// The BLAKE3 hash of the executable running now, as 64 hex digits.
pub fn binary() -> io::Result<String> {
    Ok(hex(&binary_hash()?))
}

/// The process's hash of its own executable, once known: [`seed`]ed by the one home of it
/// (blaze-cargo's `serve::local::tool_hash`, by the executable's stamp), or read whole by
/// [`binary_hash`].
static HASH: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();

/// The running executable's file: Linux's `/proc/self/exe`, which follows the running inode
/// even once another file is renamed over its path; elsewhere the path it was started from
/// (`std::env::current_exe`), which holds the running file unless something renamed another
/// over it while it runs.
pub fn running() -> io::Result<std::path::PathBuf> {
    let proc = Path::new("/proc/self/exe");
    match proc.exists() {
        true => Ok(proc.to_path_buf()),
        false => std::env::current_exe(),
    }
}

/// The BLAKE3 hash of the executable running now, read once a process: the executable is not
/// written in place while it runs (see ix-prof's `map`), so the first hash is every later one.
/// [`seed`]ed, it reads nothing. A read that fails is not kept, and the next call tries again.
pub fn binary_hash() -> io::Result<[u8; 32]> {
    if let Some(hash) = HASH.get() {
        return Ok(*hash);
    }
    let hash = tree::hash_file(&running()?)?.1;
    Ok(*HASH.get_or_init(|| hash))
}

/// Gives the process its hash of its own executable, `hash`, as known by the executable's
/// stamp, so no reader of [`binary_hash`] reads the executable; whether it took. A hash
/// already known stays (`false`): the first is every later one.
pub fn seed(hash: [u8; 32]) -> bool {
    HASH.set(hash).is_ok()
}

/// `hash` as 64 hex digits.
pub fn hex_of(hash: &[u8; 32]) -> String {
    hex(hash)
}

fn hex(hash: &[u8; 32]) -> String {
    // alloc: once a build
    let mut out = Vec::with_capacity(64);
    tree::hex(hash, &mut out);
    // alloc: the same bytes, once a build
    String::from_utf8(out).expect("hex digits are ASCII")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("blaze-identity-{name}-{}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        dir
    }

    #[test]
    fn a_trees_digest_changes_with_a_source_file_and_not_with_its_outputs() {
        let dir = scratch("tree");
        fs::write(dir.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        let before = tree_digest(&dir).unwrap();
        assert_eq!(before.len(), 64);
        assert!(before.bytes().all(|it| it.is_ascii_hexdigit()));

        fs::create_dir_all(dir.join("target/debug")).unwrap();
        fs::write(dir.join("target/debug/out"), "built").unwrap();
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(tree_digest(&dir).unwrap(), before, "outputs and git's files are not the tree");

        fs::write(dir.join("src/lib.rs"), "pub fn f() { }\n").unwrap();
        assert_ne!(tree_digest(&dir).unwrap(), before, "an edited source file is another tree");
        _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_running_executables_hash_is_stable() {
        let hash = binary().unwrap();
        assert_eq!(hash.len(), 64);
        assert_eq!(binary().unwrap(), hash);
    }
}
