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
    let exe = std::env::current_exe()?;
    Ok(hex(&tree::hash_file(&exe)?.1))
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
        let dir = std::env::temp_dir().join(format!("blaze-identity-{name}-{}", std::process::id()));
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
