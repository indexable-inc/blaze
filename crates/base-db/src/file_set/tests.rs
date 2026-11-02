use super::*;

fn path(path: &str) -> VfsPath {
    VfsPath::new_virtual_path(path.to_owned())
}

fn config(sets: &[&[&str]]) -> FileSetConfig {
    let mut config = FileSetConfig::builder();
    for set in sets {
        config.add_file_set(set.iter().map(|it| path(it)).collect());
    }
    config.build()
}

/// The size of each set `files` are partitioned into, file `i` numbered `i`.
fn sizes(config: &FileSetConfig, files: &[&str]) -> Vec<usize> {
    let paths: Vec<VfsPath> = files.iter().map(|it| path(it)).collect();
    let files = paths.iter().enumerate().map(|(at, it)| (FileId::from_raw(at as u32), it));
    config.partition(files).into_iter().map(|it| it.len()).collect()
}

#[test]
fn path_prefix() {
    let config = config(&[&["/foo"], &["/foo/bar/baz"]]);
    let files = ["/foo/src/lib.rs", "/foo/src/bar/baz/lib.rs", "/foo/bar/baz/lib.rs", "/quux/lib.rs"];
    assert_eq!(sizes(&config, &files), [2, 1, 1]);
}

#[test]
fn name_prefix() {
    let config = config(&[&["/foo"], &["/foo-things"]]);
    assert_eq!(sizes(&config, &["/foo/src/lib.rs", "/foo-things/src/lib.rs"]), [1, 1, 0]);
}

#[test]
fn classify_path() {
    let config = config(&[&["/foo"], &["/foo/bar/baz"]]);
    let classify = |it: &str| config.classify_path(&path(it));
    assert_eq!(classify("/foo/src/lib.rs"), Some(0));
    assert_eq!(classify("/foo/bar/baz/lib.rs"), Some(1));
    assert_eq!(classify("/quux/lib.rs"), None);
}

#[test]
fn classify_path_default_config() {
    assert_eq!(FileSetConfig::default().classify_path(&path("/foo/lib.rs")), None);
}

/// `/foo/bar_baz.rs` is in `/foo`, not `/foo/bar`.
#[test]
fn name_prefix_partially_matches() {
    let config = config(&[&["/foo"], &["/foo/bar"]]);
    assert_eq!(sizes(&config, &["/foo/lib.rs", "/foo/bar_baz.rs", "/foo/bar/biz.rs"]), [2, 1, 0]);
}

/// A prefix matches by bytes, as the key's prefix: `/foo` takes `/foobar`'s files.
#[test]
fn a_prefix_matches_by_bytes() {
    let config = config(&[&["/foo"]]);
    assert_eq!(config.classify_path(&path("/foobar/lib.rs")), Some(0));
}

#[test]
fn a_prefix_in_two_lists_is_the_first_lists() {
    let config = config(&[&["/a"], &["/a", "/b"]]);
    assert_eq!(config.classify_path(&path("/a/lib.rs")), Some(0));
    assert_eq!(config.classify_path(&path("/b/lib.rs")), Some(1));
}

#[test]
fn a_disk_prefix_takes_no_virtual_path() {
    let dir = std::env::temp_dir().join("blaze-file-set");
    let disk = VfsPath::from(paths::AbsPathBuf::assert_utf8(dir.clone()));
    let mut config = FileSetConfig::builder();
    config.add_file_set(vec![disk]);
    let config = config.build();
    let on_disk = VfsPath::from(paths::AbsPathBuf::assert_utf8(dir.join("lib.rs")));
    assert_eq!(config.classify_path(&on_disk), Some(0));
    #[cfg(unix)]
    assert_eq!(config.classify_path(&path(&format!("{}/lib.rs", dir.display()))), None);
}

/// Every set's classification is the one a scan of every prefix gives: the longest key that
/// is a byte prefix of the directory's key, ties to the first list.
#[test]
fn classify_is_the_longest_prefix_a_scan_finds() {
    let lists: &[&[&str]] = &[&["/a"], &["/a/b", "/c"], &["/a/bc"], &["/a/b/c/d"], &["/c"]];
    let config = config(lists);
    let mut key = Vec::new();
    for file in ["/a/x.rs", "/a/b/x.rs", "/a/bcd/x.rs", "/a/b/c/d/e/x.rs", "/c/x.rs", "/d/x.rs", "/x.rs"] {
        let file = path(file);
        key.clear();
        file.parent().unwrap().encode(&mut key);
        let mut best: Option<(usize, usize)> = None;
        for (set, prefixes) in lists.iter().enumerate() {
            for prefix in *prefixes {
                let mut it = Vec::new();
                path(prefix).encode(&mut it);
                if key.starts_with(&it) && best.is_none_or(|(len, _)| it.len() > len) {
                    best = Some((it.len(), set));
                }
            }
        }
        assert_eq!(config.classify_path(&file), best.map(|(_, set)| set), "{file}");
    }
}

#[test]
fn resolve_path_reads_from_the_anchors_directory() {
    let mut set = FileSet::default();
    let (lib, m, shared) = (FileId::from_raw(0), FileId::from_raw(1), FileId::from_raw(2));
    set.insert(lib, path("/r/src/lib.rs"));
    set.insert(m, path("/r/src/m.rs"));
    set.insert(shared, path("/r/shared.rs"));
    assert_eq!(set.resolve_path(AnchoredPath { anchor: lib, path: "m.rs" }), Some(m));
    assert_eq!(set.resolve_path(AnchoredPath { anchor: m, path: "../shared.rs" }), Some(shared));
    assert_eq!(set.resolve_path(AnchoredPath { anchor: lib, path: "gone.rs" }), None);
    assert_eq!(set.resolve_path(AnchoredPath { anchor: FileId::from_raw(9), path: "m.rs" }), None);
}

#[test]
fn remove_keeps_the_others_in_order_and_a_path_naming_another_file() {
    let mut set = FileSet::default();
    let ids: Vec<FileId> = (0..4).map(FileId::from_raw).collect();
    for (at, id) in ids.iter().enumerate() {
        set.insert(*id, path(&format!("/f{at}.rs")));
    }
    set.insert(ids[3], path("/f1.rs"));
    assert_eq!(set.remove(ids[1]), Some(path("/f1.rs")));
    assert_eq!(set.file_for_path(&path("/f1.rs")), Some(&ids[3]));
    assert_eq!(set.iter().collect::<Vec<_>>(), [ids[0], ids[2], ids[3]]);
    assert_eq!(set.remove(ids[1]), None);
}
