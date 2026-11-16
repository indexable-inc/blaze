//! Path helpers that behave as cargo-util's do, since paths cargo prints are made with them.

use std::path::{Component, Path, PathBuf};

/// Removes `.` and resolves `..` lexically, without touching the file system: cargo's
/// `cargo_util::paths::normalize_path`.
pub fn normalize_path(path: &Path) -> PathBuf {
    // On Windows the daemon's `/C:/a` has no prefix: pushed part by part, its `C:` would take the
    // root's place (`C:a`). It stays `/`-separated, as the daemon names it.
    if cfg!(windows) && path.has_root() && !matches!(path.components().next(), Some(Component::Prefix(..))) {
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::ParentDir => {
                    parts.pop();
                }
                Component::Normal(c) => parts.push(c.to_string_lossy()),
                _ => {}
            }
        }
        return PathBuf::from(format!("/{}", parts.join("/")));
    }
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

/// `path` relative to `base`, both taken lexically (cargo prints these with pathdiff 0.2's
/// `diff_paths`, whose rules this keeps): an absolute `path` under a relative `base` is
/// itself; a relative `path` under an absolute `base` has no answer, nor has a `base` with a
/// `..` past the common part.
pub(crate) fn relative_to(path: &Path, base: &Path) -> Option<PathBuf> {
    if path.is_absolute() != base.is_absolute() {
        return path.is_absolute().then(|| path.to_path_buf());
    }
    let mut ita = path.components();
    let mut itb = base.components();
    let mut out = PathBuf::new();
    // Whether a component has been written: the common prefix is skipped only before that.
    let mut started = false;
    loop {
        match (ita.next(), itb.next()) {
            (None, None) => break,
            (Some(a), None) => {
                out.push(a);
                out.extend(ita.by_ref());
                break;
            }
            (None, Some(_)) => out.push(Component::ParentDir),
            (Some(a), Some(b)) if !started && a == b => continue,
            (Some(a), Some(Component::CurDir)) => out.push(a),
            (Some(_), Some(Component::ParentDir)) => return None,
            (Some(a), Some(_)) => {
                out.push(Component::ParentDir);
                for _ in itb.by_ref() {
                    out.push(Component::ParentDir);
                }
                out.push(a);
                out.extend(ita.by_ref());
                break;
            }
        }
        started = true;
    }
    Some(out)
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
