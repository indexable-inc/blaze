//! The walk behind a directory argument: every regular file below it, in sorted order, minus
//! what git would not show.
//!
//! What is left out, and nothing else:
//! - hidden entries (a name starting with `.`) below the root, `.git` among them;
//! - symbolic links (never followed, never listed);
//! - what a `.gitignore` excludes, when the root is inside a git checkout (an ancestor, or the
//!   root itself, holds `.git`): the `.gitignore` of every directory from the checkout's top down
//!   to the root, and of every directory walked. Deeper files win over shallower ones and later
//!   lines over earlier ones; an excluded directory is not entered, so nothing under it is
//!   re-included. Patterns are git's: `#` comments, `!` negation, `\` escapes, a trailing `/`
//!   for directories only, a `/` at the start or in the middle anchoring the pattern to its
//!   file's directory (else it matches a name at any depth), `*`, `?`, `[...]` (with `!` or `^`
//!   negation and ranges, over bytes), and `**` as a whole segment (`**/x`, `x/**`, `x/**/y`).
//!
//! Not read: `.git/info/exclude`, `core.excludesFile` (the global excludes) and `.ignore` files.
//! The first two are per machine, so the files a directory argument names would differ between
//! two people's checkouts of one tree; the third is ripgrep's, not git's. No caller relies on
//! them: blaze's gate (`scan.py`) passes git's own file list, one file at a time, and the
//! patterns blaze's `.gitignore` files use are all in the subset above.

use std::alloc::Global;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::SourceError;

/// Push every file below `root` that the walk keeps onto `out`, each as `root` joined with its
/// path below it, sorted within each directory.
///
/// # Errors
///
/// [`SourceError::Walk`] naming the directory that could not be listed or the `.gitignore` that
/// could not be read.
pub(crate) fn files(root: &Path, out: &mut Vec<PathBuf>) -> Result<(), SourceError> {
    let absolute = std::path::absolute(root).map_err(|source| walk_error(root, source))?;
    let mut layers: Vec<Layer> = Vec::new_in(Global);
    let checkout = checkout_top(&absolute)?;
    if let Some(top) = &checkout {
        // The `.gitignore` files above the root, from the checkout's top down; the root's own is
        // read when it is walked.
        let mut above: Vec<&Path> = Vec::new_in(Global);
        above.extend(absolute.ancestors().skip(1).take_while(|dir| dir.starts_with(top)));
        for dir in above.into_iter().rev() {
            layers.extend(Layer::read(dir)?);
        }
    }
    // Each directory waiting to be listed, with how many layers its parent had.
    let mut pending: Vec<(PathBuf, PathBuf, usize)> = Vec::new_in(Global);
    pending.push((root.to_path_buf(), absolute, layers.len()));
    while let Some((dir, absolute, depth)) = pending.pop() {
        layers.truncate(depth);
        if checkout.is_some() {
            layers.extend(Layer::read(&absolute)?);
        }
        let mut entries: Vec<(std::ffi::OsString, fs::FileType)> = Vec::new_in(Global);
        for entry in fs::read_dir(&absolute).map_err(|source| walk_error(&dir, source))? {
            let entry = entry.map_err(|source| walk_error(&dir, source))?;
            let kind = entry.file_type().map_err(|source| walk_error(&entry.path(), source))?;
            entries.push((entry.file_name(), kind));
        }
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        let mut subdirs: Vec<(PathBuf, PathBuf, usize)> = Vec::new_in(Global);
        for (name, kind) in entries {
            if name.as_encoded_bytes().starts_with(b".") || kind.is_symlink() {
                continue;
            }
            let is_dir = kind.is_dir();
            let entry_absolute = absolute.join(&name);
            if excluded(&layers, &entry_absolute, is_dir) {
                continue;
            }
            if is_dir {
                subdirs.push((dir.join(&name), entry_absolute, layers.len()));
            } else if kind.is_file() {
                out.push(dir.join(&name));
            }
        }
        // Popped first to last, so the walk goes depth first in sorted order.
        pending.extend(subdirs.into_iter().rev());
    }
    Ok(())
}

fn walk_error(path: &Path, source: io::Error) -> SourceError {
    SourceError::Walk { path: path.to_path_buf(), source }
}

/// The nearest of `dir` and its ancestors holding a `.git` (a directory, or a worktree's file).
fn checkout_top(dir: &Path) -> Result<Option<PathBuf>, SourceError> {
    for candidate in dir.ancestors() {
        let git = candidate.join(".git");
        match fs::symlink_metadata(&git) {
            Ok(_) => return Ok(Some(candidate.to_path_buf())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(walk_error(&git, source)),
        }
    }
    Ok(None)
}

/// Whether the last pattern of `layers`, shallowest file first, that matches `path` excludes it.
fn excluded(layers: &[Layer], path: &Path, is_dir: bool) -> bool {
    let mut verdict = false;
    for layer in layers {
        let Ok(relative) = path.strip_prefix(&layer.base) else {
            continue;
        };
        let relative = relative.as_os_str().as_encoded_bytes();
        let name = relative.rsplit(|&c| c == b'/').next().unwrap_or(relative);
        for pattern in &layer.patterns {
            if pattern.dir_only && !is_dir {
                continue;
            }
            let subject = if pattern.anchored { relative } else { name };
            if glob(&pattern.glob, subject) {
                verdict = !pattern.negated;
            }
        }
    }
    verdict
}

/// One directory's `.gitignore`.
struct Layer {
    base: PathBuf,
    patterns: Vec<Pattern>,
}

struct Pattern {
    glob: Vec<u8>,
    negated: bool,
    dir_only: bool,
    anchored: bool,
}

impl Layer {
    /// `dir`'s `.gitignore`, if it has one.
    fn read(dir: &Path) -> Result<Option<Self>, SourceError> {
        let path = dir.join(".gitignore");
        let text = match fs::read(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(SourceError::Walk { path, source }),
        };
        let patterns = text.split(|&c| c == b'\n').filter_map(Pattern::parse).collect();
        Ok(Some(Self { base: dir.to_path_buf(), patterns }))
    }
}

impl Pattern {
    /// One line of a `.gitignore`, or `None` for a blank line or a comment.
    fn parse(line: &[u8]) -> Option<Self> {
        let mut line = line.strip_suffix(b"\r").unwrap_or(line);
        // Trailing spaces go unless the last is escaped.
        while let Some(rest) = line.strip_suffix(b" ") {
            if rest.ends_with(b"\\") {
                break;
            }
            line = rest;
        }
        if line.is_empty() || line.starts_with(b"#") {
            return None;
        }
        let negated = line.starts_with(b"!");
        if negated {
            line = &line[1..];
        }
        let dir_only = line.ends_with(b"/");
        if dir_only {
            line = &line[..line.len() - 1];
        }
        let anchored = line.contains(&b'/');
        let line = line.strip_prefix(b"/").unwrap_or(line);
        if line.is_empty() {
            return None;
        }
        Some(Self { glob: line.to_vec(), negated, dir_only, anchored })
    }
}

/// git's wildmatch with `**` segments, over bytes: `*` and `?` stop at `/`.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    while let Some(&c) = pattern.get(p) {
        match c {
            b'*' => {
                let segment_start = p == 0 || pattern[p - 1] == b'/';
                let double = pattern.get(p + 1) == Some(&b'*')
                    && segment_start
                    && matches!(pattern.get(p + 2), None | Some(b'/'));
                if double {
                    let Some(rest) = pattern.get(p + 3..) else {
                        // A final `**` matches everything below.
                        return true;
                    };
                    // Zero or more whole segments, then the rest.
                    let mut at = t;
                    loop {
                        if glob(rest, &text[at..]) {
                            return true;
                        }
                        match text[at..].iter().position(|&c| c == b'/') {
                            Some(slash) => at += slash + 1,
                            None => return false,
                        }
                    }
                }
                let mut end = p;
                while pattern.get(end) == Some(&b'*') {
                    end += 1;
                }
                let rest = &pattern[end..];
                let mut at = t;
                loop {
                    if glob(rest, &text[at..]) {
                        return true;
                    }
                    if at == text.len() || text[at] == b'/' {
                        return false;
                    }
                    at += 1;
                }
            }
            b'?' => {
                if text.get(t).is_none_or(|&c| c == b'/') {
                    return false;
                }
                p += 1;
                t += 1;
            }
            b'[' => {
                let Some(&byte) = text.get(t) else {
                    return false;
                };
                match class(&pattern[p + 1..], byte) {
                    Some((true, used)) => {
                        p += 1 + used;
                        t += 1;
                    }
                    // Not in the class, or a `[` never closed: git matches nothing then.
                    Some((false, _)) | None => return false,
                }
            }
            b'\\' => {
                // A trailing `\` is invalid and matches nothing, as in git.
                let Some(&literal) = pattern.get(p + 1) else {
                    return false;
                };
                if text.get(t) != Some(&literal) {
                    return false;
                }
                p += 2;
                t += 1;
            }
            literal => {
                if text.get(t) != Some(&literal) {
                    return false;
                }
                p += 1;
                t += 1;
            }
        }
    }
    t == text.len()
}

/// Whether `byte` is in the class `body` starts (just after its `[`), and how many bytes of
/// `body` the class takes, its `]` included; `None` when the class is never closed.
fn class(body: &[u8], byte: u8) -> Option<(bool, usize)> {
    let mut at = 0;
    let negated = matches!(body.first(), Some(b'!' | b'^'));
    if negated {
        at += 1;
    }
    let mut found = false;
    let mut first = true;
    loop {
        let mut low = *body.get(at)?;
        if low == b']' && !first {
            return Some((found != negated && byte != b'/', at + 1));
        }
        first = false;
        if low == b'\\' {
            at += 1;
            low = *body.get(at)?;
        }
        at += 1;
        let mut high = low;
        if body.get(at) == Some(&b'-') && body.get(at + 1).is_some_and(|&c| c != b']') {
            high = body[at + 1];
            at += 2;
            if high == b'\\' {
                high = *body.get(at)?;
                at += 1;
            }
        }
        found |= (low..=high).contains(&byte);
    }
}

#[cfg(test)]
mod tests {
    use super::{Pattern, glob};

    fn matches(line: &str, path: &str) -> bool {
        let pattern = Pattern::parse(line.as_bytes()).expect("a pattern");
        let name = path.rsplit('/').next().unwrap_or(path);
        let subject = if pattern.anchored { path } else { name };
        glob(&pattern.glob, subject.as_bytes())
    }

    #[test]
    fn globs_follow_git() {
        assert!(matches("target", "a/b/target"));
        assert!(matches("/target*/", "target-x"));
        assert!(!matches("/target*/", "a/target"));
        assert!(matches("*.rs", "src/lib.rs"));
        assert!(!matches("a/*.rs", "a/b/c.rs"));
        assert!(matches("a/**/c.rs", "a/c.rs"));
        assert!(matches("a/**/c.rs", "a/b/x/c.rs"));
        assert!(matches("**/c.rs", "a/b/c.rs"));
        assert!(matches("/library/**", "library/x/y"));
        assert!(!matches("/library/**", "library"));
        assert!(matches("[a-c]?", "b9"));
        assert!(!matches("[!a-c]?", "b9"));
        assert!(matches(".dev.vars*", ".dev.vars.prod"));
        assert!(matches("\\#x", "#x"));
        assert!(Pattern::parse(b"# comment").is_none());
        let negated = Pattern::parse(b"!analysis/lake-manifest.json").expect("a pattern");
        assert!(negated.negated && negated.anchored);
        assert!(Pattern::parse(b"vendor/").is_some_and(|it| it.dir_only && !it.anchored));
    }
}
