//! Git's per-checkout ignore rules, sans-IO.
//!
//! The matcher deliberately stops at the checkout's own `.gitignore` files and the served
//! repository's `info/exclude`. A bare `git status` may also consult the machine's
//! `core.excludesFile` and `~/.config/git/ignore`, but those are machine facts and would make a
//! script's answer depend on who served the same checkout. The served answer therefore follows
//! the checkout-only subset by design.

use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::git::GitFiles;

/// One rule of an ignore file, parsed once.
struct Rule<'a> {
    negated: bool,
    only_directory: bool,
    pattern: AString<'a>,
}

/// The ignore rules of one status computation (E2, 2026-11-20): `info/exclude` and each folder's
/// `.gitignore`, every file read through [`GitFiles::read`] and parsed on its first ask, then
/// asked of every path below it. Before, each path asked read `info/exclude` and every
/// `.gitignore` from the root down again (tempo's edit: 11% of the command's opens), so n
/// untracked paths in one folder read its `.gitignore` n times; now once.
///
/// git's precedence is unchanged: `info/exclude` first, then each `.gitignore` from the root
/// down to the path's folder, a later file overriding an earlier one and, within a file, the
/// last matching line winning. A missing file is still asked (the reader records the absence)
/// and kept as no rule. Held for one computation only: it is never a cache between commands.
pub(crate) struct Ignores<'a> {
    alloc: Alloc<'a>,
    /// `info/exclude`'s rules; `None` until first asked.
    exclude: Option<AVec<'a, Rule<'a>>>,
    /// Each folder's `.gitignore` rules (empty: absent, or no rule), by the folder's path from
    /// the worktree's root (`""` the root), sorted by bytes.
    folders: AVec<'a, (AString<'a>, AVec<'a, Rule<'a>>)>,
    /// The ignore files read (a file absent counts): what the tests count.
    #[cfg_attr(not(test), allow(dead_code, reason = "read by the tests' read count (E2)"))]
    pub(crate) files_read: u32,
}

impl<'a> Ignores<'a> {
    pub(crate) fn new(alloc: Alloc<'a>) -> Self {
        Ignores { alloc, exclude: None, folders: Vec::new_in(alloc), files_read: 0 }
    }

    /// The rules of the file `path`, read and parsed.
    fn parse(&mut self, path: &str, files: &mut impl GitFiles) -> AVec<'a, Rule<'a>> {
        let alloc = self.alloc;
        let mut bytes = Vec::new_in(alloc);
        let mut rules = Vec::new_in(alloc);
        self.files_read = self.files_read.saturating_add(1);
        if files.read(path, &mut bytes) {
            for line in bytes.split(|byte| *byte == b'\n') {
                if let Some((negated, pattern, only_directory)) = rule(line) {
                    rules.push(Rule { negated, only_directory, pattern: String::from_str_in(pattern, alloc) });
                }
            }
        }
        rules
    }

    /// Where the rules of the folder `base`'s `.gitignore` are in [`Self::folders`], read on
    /// the first ask.
    fn folder(&mut self, work: &str, base: &str, files: &mut impl GitFiles) -> usize {
        match self.folders.binary_search_by(|(known, _)| known.as_bytes().cmp(base.as_bytes())) {
            Ok(at) => at,
            Err(at) => {
                let mut ignore = join(work, base, self.alloc);
                if !ignore.is_empty() {
                    ignore.push('/');
                }
                ignore.push_str(".gitignore");
                let rules = self.parse(&ignore, files);
                self.folders.insert(at, (String::from_str_in(base, self.alloc), rules));
                at
            }
        }
    }

    /// Whether `path` (from the worktree's root) is ignored by the repository's served ignore
    /// files, in git's precedence (the type's docs).
    pub(crate) fn ignored(&mut self, work: &str, gitdir: &str, path: &str, directory: bool, files: &mut impl GitFiles) -> bool {
        let mut answer = false;
        if self.exclude.is_none() {
            let info = join(gitdir, "info/exclude", self.alloc);
            let rules = self.parse(&info, files);
            self.exclude = Some(rules);
        }
        if let Some(value) = self.exclude.as_deref().and_then(|rules| last_of(rules, "", path, directory)) {
            answer = value;
        }
        let parent = path.rsplit_once('/').map_or("", |(base, _)| base);
        // The root's file, then each folder down to the path's own.
        let ends = core::iter::once(0).chain(parent.match_indices('/').map(|(at, _)| at)).chain((!parent.is_empty()).then_some(parent.len()));
        for end in ends {
            let base = &parent[..end];
            let at = self.folder(work, base, files);
            if let Some(value) = last_of(&self.folders[at].1, base, path, directory) {
                answer = value;
            }
        }
        answer
    }
}

/// The last rule of `rules` (one file's, relative to `base`) that matches `path`.
fn last_of(rules: &[Rule<'_>], base: &str, path: &str, directory: bool) -> Option<bool> {
    let mut answer = None;
    for rule in rules {
        if rule.only_directory && !directory {
            continue;
        }
        if matches(&rule.pattern, base, path) {
            answer = Some(!rule.negated);
        }
    }
    answer
}

fn join<'a>(base: &str, name: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::from_str_in(base, alloc);
    if !base.is_empty() && !name.is_empty() {
        out.push('/');
    }
    out.push_str(name);
    out
}

/// The last rule in `text` that matches `path`, relative to `base` (the rule table's tests).
#[cfg(test)]
pub(crate) fn last(text: &[u8], base: &str, path: &str, directory: bool) -> Option<bool> {
    let mut answer = None;
    for line in text.split(|byte| *byte == b'\n') {
        let Some((negated, pattern, only_directory)) = rule(line) else { continue };
        if only_directory && !directory {
            continue;
        }
        if matches(pattern, base, path) {
            answer = Some(!negated);
        }
    }
    answer
}

fn rule(line: &[u8]) -> Option<(bool, &str, bool)> {
    let mut line = line.strip_suffix(b"\r").unwrap_or(line);
    while line.last() == Some(&b' ') {
        let mut slashes = 0;
        let mut at = line.len();
        while at > 0 && line[at - 1] == b'\\' {
            slashes += 1;
            at -= 1;
        }
        if slashes % 2 == 1 {
            break;
        }
        line = &line[..line.len() - 1];
    }
    if line.is_empty() || line[0] == b'#' {
        return None;
    }
    let mut negated = false;
    if line[0] == b'!' {
        negated = true;
        line = &line[1..];
    } else if line.starts_with(b"\\!") {
        line = &line[1..];
    }
    if line.is_empty() {
        return None;
    }
    let only_directory = line.ends_with(b"/") && !line.ends_with(b"\\/");
    if only_directory {
        line = &line[..line.len() - 1];
    }
    let pattern = core::str::from_utf8(line).ok()?;
    Some((negated, pattern, only_directory))
}

fn matches(pattern: &str, base: &str, path: &str) -> bool {
    let anchored = pattern.starts_with('/') || pattern.contains('/');
    let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
    if anchored {
        let Some(relative) = relative(base, path) else { return false };
        path_match(pattern, relative)
    } else {
        path.split('/').any(|part| segment_match(pattern, part))
    }
}

fn relative<'a>(base: &str, path: &'a str) -> Option<&'a str> {
    if base.is_empty() {
        return Some(path);
    }
    let rest = path.strip_prefix(base)?;
    rest.strip_prefix('/').or_else(|| (rest.is_empty()).then_some(rest))
}

fn path_match(pattern: &str, path: &str) -> bool {
    let (patterns, pattern_count) = split_segments(pattern);
    let (paths, path_count) = split_segments(path);
    path_segments(&patterns, pattern_count, &paths, path_count, 0, 0)
}

fn split_segments(text: &str) -> ([&str; 64], usize) {
    let mut out = [""; 64];
    let mut count = 0;
    let mut start = 0;
    for (at, byte) in text.bytes().enumerate() {
        if byte == b'/' {
            if count < out.len() {
                out[count] = &text[start..at];
                count += 1;
            }
            start = at + 1;
        }
    }
    if count < out.len() {
        out[count] = &text[start..];
        count += 1;
    }
    (out, count)
}

fn path_segments(patterns: &[&str; 64], pattern_count: usize, paths: &[&str; 64], path_count: usize, pattern_at: usize, path_at: usize) -> bool {
    if pattern_at == pattern_count {
        return path_at == path_count;
    }
    let pattern = patterns[pattern_at];
    if pattern == "**" {
        let next = pattern_at + 1;
        let last = next == pattern_count;
        if last {
            return path_at < path_count;
        }
        if path_at == path_count {
            return false;
        }
        return path_segments(patterns, pattern_count, paths, path_count, next, path_at)
            || path_segments(patterns, pattern_count, paths, path_count, pattern_at, path_at + 1);
    }
    if path_at == path_count || !segment_match(pattern, paths[path_at]) {
        return false;
    }
    path_segments(patterns, pattern_count, paths, path_count, pattern_at + 1, path_at + 1)
}

fn segment_match(pattern: &str, path: &str) -> bool {
    let p = pattern.as_bytes();
    let s = path.as_bytes();
    segment_bytes(p, s, 0, 0)
}

fn segment_bytes(pattern: &[u8], path: &[u8], pattern_at: usize, path_at: usize) -> bool {
    if pattern_at == pattern.len() {
        return path_at == path.len();
    }
    match pattern[pattern_at] {
        b'*' => {
            segment_bytes(pattern, path, pattern_at + 1, path_at)
                || path_at < path.len() && segment_bytes(pattern, path, pattern_at, path_at + 1)
        }
        b'?' => path_at < path.len() && segment_bytes(pattern, path, pattern_at + 1, path_at + 1),
        b'\\' => pattern_at + 1 < pattern.len() && path_at < path.len() && pattern[pattern_at + 1] == path[path_at] && segment_bytes(pattern, path, pattern_at + 2, path_at + 1),
        b'[' => {
            let Some(close) = pattern[pattern_at + 1..].iter().position(|byte| *byte == b']') else { return path_at < path.len() && pattern[pattern_at] == path[path_at] && segment_bytes(pattern, path, pattern_at + 1, path_at + 1) };
            if path_at >= path.len() {
                return false;
            }
            let body = &pattern[pattern_at + 1..pattern_at + 1 + close];
            let (negated, body) = match body.first() {
                Some(b'!') | Some(b'^') => (true, &body[1..]),
                _ => (false, body),
            };
            let mut found = false;
            let mut at = 0;
            while at < body.len() {
                if at + 2 < body.len() && body[at + 1] == b'-' {
                    found |= body[at] <= path[path_at] && path[path_at] <= body[at + 2];
                    at += 3;
                } else {
                    found |= body[at] == path[path_at];
                    at += 1;
                }
            }
            (found != negated) && segment_bytes(pattern, path, pattern_at + close + 2, path_at + 1)
        }
        byte => path_at < path.len() && byte == path[path_at] && segment_bytes(pattern, path, pattern_at + 1, path_at + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::last;

    #[test]
    fn gitignore_rules_match_the_table() {
        let rows = [
            ("# comment\n\n*.tmp", "a.tmp", false, true),
            ("*.tmp", "dir/a.tmp", false, true),
            ("/a.tmp", "dir/a.tmp", false, false),
            ("/a.tmp", "a.tmp", false, true),
            ("build/", "build", true, true),
            ("build/", "build/file", false, false),
            ("a/**/b", "a/b", false, true),
            ("a/**/b", "a/x/y/b", false, true),
            ("abc/**", "abc", true, false),
            ("abc/**", "abc/x", false, true),
            ("**/cache", "cache", true, true),
            ("**/cache", "a/cache", true, true),
            ("file[0-9]", "file7", false, true),
            ("file?", "file7", false, true),
            (r"foo\ bar", "foo bar", false, true),
            (r"\#literal", "#literal", false, true),
            ("*.log\n!important.log", "important.log", false, false),
        ];
        for (patterns, path, directory, expected) in rows {
            assert_eq!(last(patterns.as_bytes(), "", path, directory).unwrap_or(false), expected, "{patterns:?} {path:?}");
        }
    }

    #[test]
    fn later_lines_override_earlier_lines() {
        assert_eq!(last(b"*.tmp\n!important.tmp", "", "important.tmp", false), Some(false));
        assert_eq!(last(b"!important.tmp\n*.tmp", "", "important.tmp", false), Some(true));
    }
}
