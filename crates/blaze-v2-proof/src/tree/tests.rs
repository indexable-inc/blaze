//! The tree digest against a one-thread model of the lines `engine.tree_digest` hashed, and what
//! each kind of change does to it.

use std::{fs, path::Path};

use super::*;

/// The harness's walk, one directory at a time: every file's and link's line, sorted, hashed.
fn model(root: &Path, skip: &[String]) -> [u8; 32] {
    fn visit(dir: &Path, rel: &str, skip: &[String], files: &mut Vec<String>, links: &mut Vec<String>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|it| it.unwrap()).collect();
        entries.sort_by_key(|it| it.file_name());
        for entry in entries {
            let name = entry.file_name().into_string().unwrap();
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).unwrap();
            if rel.is_empty() && skip.contains(&name) && fs::metadata(&path).is_ok_and(|it| it.is_dir()) {
                continue;
            }
            let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            if meta.file_type().is_symlink() {
                links.push(format!("l\0{child}\0{}\n", fs::read_link(&path).unwrap().to_str().unwrap()));
            } else if meta.is_dir() {
                visit(&path, &child, skip, files, links);
            } else if meta.is_file() {
                let bytes = fs::read(&path).unwrap();
                let mut digest = Vec::new();
                hex(&b3::hash(&bytes), &mut digest);
                let executable = u8::from(super::executable(&path, &meta));
                files.push(format!("f\0{child}\0{executable}\0{}\n", String::from_utf8(digest).unwrap()));
            }
        }
    }
    let (mut files, mut links) = (Vec::new(), Vec::new());
    visit(root, "", skip, &mut files, &mut links);
    files.sort();
    links.sort();
    let mut hasher = b3::Hasher::new();
    hasher.update(DOMAIN);
    for line in files.iter().chain(&links) {
        hasher.update(line.as_bytes());
    }
    hasher.finalize()
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("blaze-v2-proof-tree-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn skip() -> Vec<String> {
    vec!["target".to_owned()]
}

/// A tree of many directories, more files in one than a task takes, one file past
/// [`MAPPED_FROM`], an empty file and a built `target` to skip.
fn populate(root: &Path) {
    fs::create_dir_all(root.join("target/debug")).unwrap();
    fs::write(root.join("target/debug/out"), "built\n").unwrap();
    for d in 0..20 {
        let dir = root.join(format!("src/m{d}/inner"));
        fs::create_dir_all(&dir).unwrap();
        for f in 0..(if d == 3 { 3 * FILES_A_TASK + 5 } else { 4 }) {
            fs::write(dir.join(format!("f{f}.rs")), format!("fn f{d}_{f}() {{}}\n")).unwrap();
        }
    }
    fs::write(root.join("empty"), "").unwrap();
    fs::write(root.join("big.bin"), vec![7u8; MAPPED_FROM as usize + 3]).unwrap();
    fs::write(root.join("lib.rs"), "fn f() {}\n").unwrap();
    fs::write(root.join("build.sh"), "#!/bin/sh\n").unwrap();
}

#[test]
fn the_digest_is_the_model_s() {
    let root = scratch("model");
    populate(&root);
    assert_eq!(digest(&root, &skip()).unwrap(), model(&root, &skip()));
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn every_change_but_a_skipped_one_changes_the_digest() {
    let root = scratch("changes");
    populate(&root);
    let mut before = digest(&root, &skip()).unwrap();
    fs::write(root.join("target/debug/out"), "built again\n").unwrap();
    assert_eq!(digest(&root, &skip()).unwrap(), before, "a skipped directory is read");
    fs::write(root.join("src/m3/inner/f2.rs"), "fn g() {}\n").unwrap();
    let after = digest(&root, &skip()).unwrap();
    assert_ne!(after, before, "an edit is missed");
    before = after;
    fs::rename(root.join("lib.rs"), root.join("main.rs")).unwrap();
    let after = digest(&root, &skip()).unwrap();
    assert_ne!(after, before, "a rename is missed");
    assert_eq!(after, model(&root, &skip()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        before = after;
        fs::set_permissions(root.join("build.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let after = digest(&root, &skip()).unwrap();
        assert_ne!(after, before, "a mode change is missed");
        before = after;
        std::os::unix::fs::symlink("main.rs", root.join("alias.rs")).unwrap();
        std::os::unix::fs::symlink("src", root.join("target-link")).unwrap();
        let after = digest(&root, &skip()).unwrap();
        assert_ne!(after, before, "a link is missed");
        assert_eq!(after, model(&root, &skip()));
    }
    fs::remove_dir_all(&root).unwrap();
}

#[cfg(unix)]
#[test]
fn a_name_that_is_not_utf8_is_an_error_naming_it() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
    let root = scratch("utf8");
    let bad = root.join(OsStr::from_bytes(b"bad\xff"));
    if fs::write(&bad, "x").is_err() {
        // A file system that refuses such names (APFS) can't hold the case.
        return fs::remove_dir_all(&root).unwrap();
    }
    let error = digest(&root, &[]).unwrap_err();
    assert!(error.to_string().contains("not UTF-8"), "{error}");
    fs::remove_dir_all(&root).unwrap();
}
