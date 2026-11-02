use std::path::PathBuf;

use super::private;
use crate::FileName;

/// A root of its own, and the store under it, not made yet.
fn store(what: &str) -> (PathBuf, FileName) {
    let root = std::env::temp_dir().join(format!("blaze-core-private-{what}-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    (root.join("store"), FileName::new("store").unwrap())
}

#[test]
fn a_missing_directory_is_made_and_is_the_users_alone() {
    let (dir, _) = store("missing");
    assert_eq!(private(&dir.join("deep"), &FileName::new("store/deep").unwrap()).unwrap(), None);
    assert!(dir.join("deep").is_dir());
}

#[cfg(unix)]
mod unix {
    use std::{os::unix::fs::PermissionsExt as _, path::Path};

    use super::{private, store};
    use crate::{Access, FileName};

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn a_made_directory_is_the_users_to_read_too() {
        let (dir, name) = store("made");
        assert_eq!(private(&dir, &name).unwrap(), None);
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn a_directory_others_read_is_closed_to_them() {
        let (dir, name) = store("closed");
        std::fs::create_dir(&dir).unwrap();
        chmod(&dir, 0o755);
        assert_eq!(private(&dir, &name).unwrap(), None);
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn a_directory_others_write_is_refused_and_left_as_it_is() {
        for refused in [0o770, 0o707, 0o777] {
            let (dir, name) = store(&format!("open-{refused:o}"));
            std::fs::create_dir(&dir).unwrap();
            chmod(&dir, refused);
            assert_eq!(private(&dir, &name).unwrap(), Some((name, Access::OthersWrite)), "{refused:o}");
            assert_eq!(mode(&dir), refused);
        }
    }

    #[test]
    fn a_file_anyone_may_write_is_refused_by_its_name() {
        let (dir, name) = store("file");
        std::fs::create_dir_all(dir.join("packs")).unwrap();
        std::fs::write(dir.join("packs/a"), b"a").unwrap();
        chmod(&dir.join("packs/a"), 0o646);
        let file = FileName::new("store/packs/a").unwrap();
        assert_eq!(private(&dir, &name).unwrap(), Some((file, Access::OthersWrite)));
    }

    /// A umask of 002 gives every file its group's write bit: harmless while the group cannot
    /// reach the files, refused while it can.
    #[test]
    fn a_file_its_group_may_write_is_refused_only_while_the_group_gets_in() {
        let (dir, name) = store("group");
        std::fs::create_dir(&dir).unwrap();
        chmod(&dir, 0o700);
        std::fs::write(dir.join("index"), b"i").unwrap();
        chmod(&dir.join("index"), 0o664);
        assert_eq!(private(&dir, &name).unwrap(), None);
        chmod(&dir, 0o710);
        let file = FileName::new("store/index").unwrap();
        assert_eq!(private(&dir, &name).unwrap(), Some((file, Access::OthersWrite)));
    }

    #[test]
    fn a_link_is_checked_by_what_it_points_at() {
        let (dir, name) = store("link");
        std::fs::create_dir(&dir).unwrap();
        let outside = dir.parent().unwrap().join("outside");
        std::fs::write(&outside, b"o").unwrap();
        chmod(&outside, 0o666);
        std::os::unix::fs::symlink(&outside, dir.join("value")).unwrap();
        let file = FileName::new("store/value").unwrap();
        assert_eq!(private(&dir, &name).unwrap(), Some((file, Access::OthersWrite)));
    }
}
