//! Path helpers that behave as cargo-util's do, since paths cargo prints are made with them.

use std::path::{Component, Path, PathBuf};

/// Removes `.` and resolves `..` lexically, without touching the file system: cargo's
/// `cargo_util::paths::normalize_path`.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut components = path.components().peekable();
    let mut ret = if let Some(c @ Component::Prefix(..)) = components.peek().cloned() {
        components.next();
        PathBuf::from(c.as_os_str())
    } else {
        PathBuf::new()
    };

    for component in components {
        match component {
            Component::Prefix(..) => unreachable!(),
            Component::RootDir => ret.push(Component::RootDir),
            Component::CurDir => {}
            Component::ParentDir => {
                if ret.ends_with(Component::ParentDir) {
                    ret.push(Component::ParentDir);
                } else {
                    let popped = ret.pop();
                    if !popped && !ret.has_root() {
                        ret.push(Component::ParentDir);
                    }
                }
            }
            Component::Normal(c) => ret.push(c),
        }
    }
    ret
}

/// The path, then each of its parents, stopping after `stop_at` when given (cargo's
/// `paths::ancestors`, which also stops at `__CARGO_TEST_ROOT`).
pub(crate) fn ancestors<'a>(
    path: &'a Path,
    stop_at: Option<&'a Path>,
) -> impl Iterator<Item = &'a Path> + 'a {
    let mut current = Some(path);
    std::iter::from_fn(move || {
        let path = current?;
        current = path.parent();
        if stop_at == Some(path) {
            current = None;
        }
        Some(path)
    })
}
