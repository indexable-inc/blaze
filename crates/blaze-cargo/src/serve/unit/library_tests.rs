//! A shipped tree mounted from its texts file ([`super::library_from_texts`]).

use blaze_core::FileName;
use blaze_ids::ContentId;

use super::{SourceFile, library_files, library_from_texts};
use crate::toolchain::library::{digest_of, texts, write_texts};

/// The root the VFS makes of `files`, each a whole file with its mode.
fn root_of(files: Vec<SourceFile>) -> ContentId {
    let (mut vfs, _) = blaze_vfs::Vfs::recover(FileName::new("journal").unwrap(), 0, &[]);
    let empty = vfs.empty();
    vfs.apply_files(empty, files.into_iter().map(|it| (it.path, it.bytes, it.mode))).unwrap()
}

/// A shipped tree at `<work>/t/<digest>/t` with its `DIGEST`, and that digest.
fn tree(work: &std::path::Path) -> (std::path::PathBuf, String) {
    let staged = work.join("staged/t");
    std::fs::create_dir_all(staged.join("core/src")).unwrap();
    std::fs::write(staged.join("Cargo.toml"), "[workspace]\n").unwrap();
    std::fs::write(staged.join("core/src/lib.rs"), "#![no_std]\npub fn f() {}\n").unwrap();
    std::fs::write(staged.join("core/src/x.rs"), "").unwrap();
    let digest = digest_of(&staged).unwrap();
    std::fs::write(staged.join("DIGEST"), format!("{digest}\n")).unwrap();
    let tree = work.join("t").join(&digest).join("t");
    std::fs::create_dir_all(tree.parent().unwrap()).unwrap();
    std::fs::rename(&staged, &tree).unwrap();
    (tree, digest)
}

/// The invariant: a shipped tree with its texts file is mounted from that file alone (the
/// tree's directory is gone when it is mounted), and the root its source makes is the root of
/// the tree read file by file; with the same fingerprint kept at the same place, no source is
/// read. The control: a texts file with one byte of one text changed (decoded as it is, a
/// start reads no text) gives another root.
#[test]
fn an_owned_library_is_mounted_from_its_texts_and_reads_no_file_of_it() {
    let work = tempfile::tempdir().unwrap();
    let (tree, digest) = tree(work.path());
    let mut read = Vec::new();
    library_files(&tree, "", &mut read).unwrap();
    let by_file = root_of(read);
    write_texts(&tree, &digest).unwrap();
    std::fs::remove_dir_all(&tree).unwrap();

    let named = "/rustc/abc/library".to_owned();
    let mounted = library_from_texts(&tree, &digest, named.clone(), &[]).unwrap();
    let source = mounted.source.expect("nothing kept");
    assert!(source.files.iter().all(|it| it.bytes.shared().is_none()), "lent, not copied");
    assert_eq!(root_of(source.files), by_file, "the texts mount the tree's root");
    let kept = [(mounted.fingerprint.clone(), named.clone())];
    let again = library_from_texts(&tree, &digest, named.clone(), &kept).unwrap();
    assert_eq!((again.fingerprint, again.source), (mounted.fingerprint.clone(), None));

    // The control. Replaced by rename, as only blaze writes it.
    let at = texts::name(&tree).unwrap();
    let mut bytes = std::fs::read(&at).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    let part = at.with_extension("changed");
    std::fs::write(&part, &bytes).unwrap();
    std::fs::rename(&part, &at).unwrap();
    let changed = library_from_texts(&tree, &digest, named, &[]).unwrap();
    assert_ne!(root_of(changed.source.unwrap().files), by_file, "the oracle misses a changed text");
}
