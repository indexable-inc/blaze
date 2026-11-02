use super::*;

fn path(path: &str) -> VfsPath {
    VfsPath::new_virtual_path(path.to_owned())
}

#[test]
fn virtual_path_starts_with_is_component_based() {
    assert!(!path("/foobar").starts_with(&path("/foo")));
    assert!(path("/foo/bar").starts_with(&path("/foo")));
}

#[test]
fn virtual_path_extensions() {
    assert_eq!(VirtualPath("/".to_owned()).name_and_extension(), None);
    assert_eq!(
        VirtualPath("/directory".to_owned()).name_and_extension(),
        Some(("directory", None))
    );
    assert_eq!(
        VirtualPath("/directory/".to_owned()).name_and_extension(),
        Some(("directory", None))
    );
    assert_eq!(
        VirtualPath("/directory/file".to_owned()).name_and_extension(),
        Some(("file", None))
    );
    assert_eq!(
        VirtualPath("/directory/.file".to_owned()).name_and_extension(),
        Some((".file", None))
    );
    assert_eq!(
        VirtualPath("/directory/.file.rs".to_owned()).name_and_extension(),
        Some((".file", Some("rs")))
    );
    assert_eq!(
        VirtualPath("/directory/file.rs".to_owned()).name_and_extension(),
        Some(("file", Some("rs")))
    );
}

#[test]
fn a_virtual_join_takes_a_component_per_leading_parent() {
    let mut dir = path("/a/b/lib.rs");
    assert!(dir.pop());
    assert_eq!(dir.join("m.rs"), Some(path("/a/b/m.rs")));
    assert_eq!(dir.join("./m.rs"), Some(path("/a/b/m.rs")));
    assert_eq!(dir.join("../../shared/m.rs"), Some(path("/shared/m.rs")));
    assert_eq!(dir.join("../../../m.rs"), None);
}

#[test]
fn pop_takes_the_last_component_until_none_is_left() {
    let mut it = path("/foo/bar");
    assert!(it.pop());
    assert_eq!(it, path("/foo"));
    assert!(it.pop());
    assert_eq!(it.to_string(), "");
    assert!(!it.pop());
    assert_eq!(path("/foo/bar").parent(), Some(path("/foo")));
}

#[cfg(unix)]
#[test]
fn disk_and_virtual_paths_never_meet() {
    let disk = VfsPath::from(AbsPathBuf::assert_utf8(std::env::temp_dir().join("a")));
    let text = disk.to_string();
    let virt = path(&text);
    assert_ne!(disk, virt);
    assert!(!disk.starts_with(&virt) && !virt.starts_with(&disk));
    assert_eq!(disk.strip_prefix(&virt), None);
    assert!(disk.as_path().is_some() && virt.as_path().is_none());

    let (mut a, mut b) = (Vec::new(), Vec::new());
    disk.encode(&mut a);
    virt.encode(&mut b);
    assert_eq!((a[0], b[0]), (0, 1));
    assert_eq!(a[1..], b[1..]);
}

#[cfg(windows)]
fn disk(path: &str) -> VfsPath {
    VfsPath::from(AbsPathBuf::try_from(path).unwrap())
}

#[cfg(windows)]
#[test]
fn a_windows_key_ignores_drive_case_and_separators() {
    let key = |path: &str| {
        let mut key = Vec::new();
        disk(path).encode(&mut key);
        key
    };
    assert_eq!(key("C:/x.rs"), key("c:/x.rs"));
    assert_eq!(key("C:/x/y.rs"), key("C:\\x\\y.rs"));
    assert_eq!(key("C:/x/y"), [0, 67, 58, 92, 120, 92, 121]);
}

#[test]
fn a_disk_path_is_normalized_when_made_and_joined() {
    let base = AbsPathBuf::assert_utf8(std::env::temp_dir());
    let made = VfsPath::from(base.join("a/../b.rs"));
    assert_eq!(made, VfsPath::from(base.join("b.rs")));
    let dir = VfsPath::from(base.join("d"));
    assert_eq!(dir.join("../c.rs"), Some(VfsPath::from(base.join("c.rs"))));
}
