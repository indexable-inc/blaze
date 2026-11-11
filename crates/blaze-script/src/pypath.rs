//! Paths as Python's `os.path` and `glob` treat them on a POSIX system, so a ported script
//! finds the same files and names them the same way: [`normpath`], [`join`], [`abspath`],
//! [`relpath`], [`glob`] and the [`fnmatch`] it matches names with, and [`pure_path`], a path
//! as `pathlib.Path` writes it.

use std::path::Path;

/// `os.path.normpath`: `.` and empty parts dropped, `a/..` folded, a leading `/` kept (two when
/// there were exactly two), `.` for nothing.
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let slashes = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else {
        usize::from(path.starts_with('/'))
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part != ".." || (slashes == 0 && parts.is_empty()) || parts.last() == Some(&"..") {
            parts.push(part);
        } else if !parts.is_empty() {
            parts.pop();
        }
    }
    let joined = format!("{}{}", "/".repeat(slashes), parts.join("/"));
    if joined.is_empty() { ".".to_owned() } else { joined }
}

/// `str(pathlib.PurePosixPath(path))`: empty and `.` parts dropped (`..` kept), no trailing `/`,
/// a leading `/` kept (two when there were exactly two), `.` for nothing.
pub fn pure_path(path: &str) -> String {
    let root = if path.starts_with("//") && !path.starts_with("///") {
        "//"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts: Vec<&str> = path.split('/').filter(|it| !it.is_empty() && *it != ".").collect();
    match (root, parts.is_empty()) {
        ("", true) => ".".to_owned(),
        (root, _) => format!("{root}{}", parts.join("/")),
    }
}

/// `os.path.join(a, b)`: `b` alone when it is absolute.
pub fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') || a.is_empty() {
        b.to_owned()
    } else if a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `os.path.abspath`: joined to the working directory, then normalized.
pub fn abspath(path: &str) -> String {
    if path.starts_with('/') {
        return normpath(path);
    }
    let here = std::env::current_dir().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
    normpath(&join(&here, path))
}

/// `os.path.relpath(path, start)`.
pub fn relpath(path: &str, start: &str) -> String {
    let path = abspath(path);
    let start = abspath(start);
    let path: Vec<&str> = path.split('/').filter(|it| !it.is_empty()).collect();
    let start: Vec<&str> = start.split('/').filter(|it| !it.is_empty()).collect();
    let common = path.iter().zip(&start).take_while(|(a, b)| a == b).count();
    let mut out: Vec<&str> = vec![".."; start.len() - common];
    out.extend(&path[common..]);
    if out.is_empty() { ".".to_owned() } else { out.join("/") }
}

/// `fnmatch.fnmatchcase(name, pattern)`: `*` any run, `?` any one character, `[abc]`, `[a-z]`
/// and `[!a]` one of a set (a `[` with no `]` after it is itself).
pub fn fnmatch(name: &str, pattern: &str) -> bool {
    let name: Vec<char> = name.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    matches(&name, &pattern)
}

fn matches(name: &[char], pattern: &[char]) -> bool {
    let Some((&first, rest)) = pattern.split_first() else { return name.is_empty() };
    match first {
        '*' => (0..=name.len()).any(|skip| matches(&name[skip..], rest)),
        '?' => !name.is_empty() && matches(&name[1..], rest),
        '[' => match set(rest) {
            Some(set) => name.first().is_some_and(|c| set.holds(*c)) && matches(&name[1..], set.after),
            None => name.first() == Some(&'[') && matches(&name[1..], rest),
        },
        c => name.first() == Some(&c) && matches(&name[1..], rest),
    }
}

/// A `[...]` of a pattern: its characters and ranges, whether a `!` negates it, and the
/// pattern after its `]`.
struct Set<'p> {
    negated: bool,
    body: &'p [char],
    after: &'p [char],
}

impl Set<'_> {
    fn holds(&self, c: char) -> bool {
        let body = self.body;
        let mut i = 0;
        let mut found = false;
        while i < body.len() {
            if i + 2 < body.len() && body[i + 1] == '-' {
                found |= body[i] <= c && c <= body[i + 2];
                i += 3;
            } else {
                found |= body[i] == c;
                i += 1;
            }
        }
        found != self.negated
    }
}

/// The set after a `[`; `None` when no `]` ends it.
fn set(pattern: &[char]) -> Option<Set<'_>> {
    let negated = pattern.first() == Some(&'!');
    let body_start = usize::from(negated);
    // A `]` first in the set is one of its characters.
    let search_from = body_start + usize::from(pattern.get(body_start) == Some(&']'));
    let end = search_from + pattern[search_from.min(pattern.len())..].iter().position(|c| *c == ']')?;
    Some(Set { negated, body: &pattern[body_start..end], after: &pattern[end + 1..] })
}

fn has_magic(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// `os.path.split`: the part before the last `/` (without trailing slashes unless it is all
/// slashes) and the part after.
fn split(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        None => ("", path),
        Some(at) => {
            let head = &path[..=at];
            let trimmed = head.trim_end_matches('/');
            (if trimmed.is_empty() { head } else { trimmed }, &path[at + 1..])
        }
    }
}

/// `glob.glob(pattern)`, not recursive and without hidden names: the paths that exist and
/// match, each part of the pattern matched against one directory's names, in no set order
/// (Python's callers sort them, and so must a port's).
pub fn glob(pattern: &str) -> Vec<String> {
    glob_in(pattern, false)
}

fn glob_in(pattern: &str, dirs_only: bool) -> Vec<String> {
    let (dir, base) = split(pattern);
    if !has_magic(pattern) {
        let there = if base.is_empty() { Path::new(dir).is_dir() } else { Path::new(pattern).symlink_metadata().is_ok() };
        return if there && (!dirs_only || Path::new(pattern).is_dir()) { vec![pattern.to_owned()] } else { vec![] };
    }
    let dirs: Vec<String> = if dir.is_empty() {
        vec![String::new()]
    } else if dir != pattern && has_magic(dir) {
        glob_in(dir, true)
    } else {
        vec![dir.to_owned()]
    };
    let mut out = Vec::new();
    for dir in dirs {
        if has_magic(base) {
            let listed = std::fs::read_dir(if dir.is_empty() { "." } else { dir.as_str() });
            let Ok(listed) = listed else { continue };
            for entry in listed.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') && !base.starts_with('.') {
                    continue;
                }
                if dirs_only && !entry.path().is_dir() {
                    continue;
                }
                if fnmatch(&name, base) {
                    out.push(if dir.is_empty() { name } else { join(&dir, &name) });
                }
            }
        } else {
            let path = join(&dir, base);
            let there = if base.is_empty() {
                Path::new(&dir).is_dir()
            } else {
                Path::new(&path).symlink_metadata().is_ok()
            };
            if there && (!dirs_only || Path::new(&path).is_dir()) {
                out.push(path);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each expectation is what Python 3.13's os.path and fnmatch give.
    #[test]
    fn paths_are_pythons() {
        assert_eq!(normpath(""), ".");
        assert_eq!(normpath("a/./b/../c/"), "a/c");
        assert_eq!(normpath("../a/../../b"), "../../b");
        assert_eq!(normpath("/../a"), "/a");
        assert_eq!(normpath("//a//b"), "//a/b");
        assert_eq!(normpath("///a"), "/a");
        assert_eq!(relpath("/r/crates/x", "/r"), "crates/x");
        assert_eq!(relpath("/r", "/r"), ".");
        assert_eq!(relpath("/a/b", "/a/c/d"), "../../b");
        assert_eq!(join("a", "/b"), "/b");
        assert_eq!(join("a/", "b"), "a/b");
        for (path, said) in [("", "."), (".", "."), ("./a//b/", "a/b"), ("a/./../b", "a/../b"), ("//x", "//x"), ("///x", "/x"), ("/", "/")] {
            assert_eq!(pure_path(path), said, "{path:?}");
        }
    }

    #[test]
    fn names_match_as_fnmatch_matches_them() {
        assert!(fnmatch("blaze-x", "*"));
        assert!(fnmatch("a.rs", "*.rs") && !fnmatch("a.rsx", "*.rs"));
        assert!(fnmatch("ab", "a?") && !fnmatch("a", "a?"));
        assert!(fnmatch("b", "[abc]") && !fnmatch("d", "[abc]"));
        assert!(fnmatch("d", "[!abc]") && !fnmatch("a", "[!abc]"));
        assert!(fnmatch("m", "[a-z]") && !fnmatch("M", "[a-z]"));
        assert!(fnmatch("]", "[]]") && fnmatch("[x", "[x"));
    }

    #[test]
    fn a_glob_finds_what_python_finds() {
        let root = std::env::temp_dir().join(format!("blaze-script-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["crates/a", "crates/b", "crates/.hidden", "tools/x"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("crates/README.md"), "").unwrap();
        let at = root.to_string_lossy().into_owned();
        let mut found = glob(&format!("{at}/crates/*"));
        found.sort();
        let named: Vec<String> = found.iter().map(|it| relpath(it, &at)).collect();
        assert_eq!(named, ["crates/README.md", "crates/a", "crates/b"]);
        assert_eq!(glob(&format!("{at}/tools/x")), [format!("{at}/tools/x")]);
        assert!(glob(&format!("{at}/tools/none")).is_empty());
        let mut deep = glob(&format!("{at}/*/a"));
        deep.sort();
        assert_eq!(deep, [format!("{at}/crates/a")]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
