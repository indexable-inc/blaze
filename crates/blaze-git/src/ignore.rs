//! Sans-IO `.gitignore` and `info/exclude` rule parser and matcher (`gitignore(5)`).
//!
//! Pre-parses each ignore file once into compact rules in the caller's allocator and matches
//! paths with zero allocations. Supports negation (`!`), directory-only rules (trailing `/`),
//! anchored rules (leading or interior `/`), `**` double-star globs, `*`, `?`, `[a-z]` / `[!a-z]`
//! character classes, backslash escaping, and trailing-space trimming.

#![blaze::sans_io]

use blaze_memchr::memchr;
use blaze_own::{AString, AVec, Alloc, String, Vec, planted::Planted};

use crate::{GitStatusPlant, Limits};

/// One compiled `.gitignore` rule.
pub struct IgnoreRule<'a> {
    pub negated: bool,
    pub only_dir: bool,
    pub anchored: bool,
    /// True if the pattern contains no glob metacharacters (`*`, `?`, `[`, `\`), enabling O(1)
    /// byte-equality fast path.
    pub literal: bool,
    pub pattern: AString<'a>,
}

/// Rules parsed from one `.gitignore` or `info/exclude` file, scoped to `base_dir` (`""` = root).
pub struct IgnoreFile<'a> {
    pub base_dir: AString<'a>,
    pub rules: AVec<'a, IgnoreRule<'a>>,
}

/// Hierarchical ignore rule table for one status computation.
pub struct IgnoreStack<'a> {
    alloc: Alloc<'a>,
    exclude: Option<AVec<'a, IgnoreRule<'a>>>,
    /// Global/config excludesfile rules if supplied by driver.
    global_exclude: Option<AVec<'a, IgnoreRule<'a>>>,
    /// Per-folder `.gitignore` files, sorted by `base_dir` bytes.
    folders: AVec<'a, IgnoreFile<'a>>,
    total_rules: u32,
    max_rules: u32,
    pub rules_capped: bool,
}

impl<'a> IgnoreStack<'a> {
    pub fn new(limits: Limits, alloc: Alloc<'a>) -> IgnoreStack<'a> {
        IgnoreStack {
            alloc,
            exclude: None,
            global_exclude: None,
            folders: Vec::new_in(alloc),
            total_rules: 0,
            max_rules: limits.most_ignore_rules,
            rules_capped: false,
        }
    }

    pub fn set_info_exclude(&mut self, bytes: &[u8], plant: impl Planted<GitStatusPlant>) {
        let rules = self.parse_rules(bytes, plant);
        self.exclude = Some(rules);
    }

    pub fn set_global_exclude(&mut self, bytes: &[u8], plant: impl Planted<GitStatusPlant>) {
        let rules = self.parse_rules(bytes, plant);
        self.global_exclude = Some(rules);
    }

    pub fn add_folder_gitignore(
        &mut self,
        base_dir: &str,
        bytes: &[u8],
        plant: impl Planted<GitStatusPlant>,
    ) {
        let rules = self.parse_rules(bytes, plant);
        match self.folders.binary_search_by(|f| f.base_dir.as_bytes().cmp(base_dir.as_bytes())) {
            Ok(at) => self.folders[at].rules = rules,
            Err(at) => {
                self.folders.insert_shift(
                    at,
                    IgnoreFile { base_dir: String::from_str_in(base_dir, self.alloc), rules },
                );
            }
        }
    }

    fn parse_rules(
        &mut self,
        bytes: &[u8],
        plant: impl Planted<GitStatusPlant>,
    ) -> AVec<'a, IgnoreRule<'a>> {
        let mut out = Vec::new_in(self.alloc);
        let mut pos = 0;
        while pos < bytes.len() {
            let nl = memchr(b'\n', &bytes[pos..]).map_or(bytes.len(), |i| pos + i);
            let mut line = &bytes[pos..nl];
            if let Some(stripped) = line.strip_suffix(b"\r") {
                line = stripped;
            }
            pos = nl + 1;

            if let Some(rule) = parse_rule_line(line, self.alloc, plant) {
                if self.total_rules >= self.max_rules {
                    self.rules_capped = true;
                    break;
                }
                self.total_rules += 1;
                out.push(rule);
            }
        }
        out
    }

    /// Whether `path` (worktree-relative, `/`-separated) is ignored according to Git's precedence:
    /// global excludes -> `info/exclude` -> root `.gitignore` -> ... -> parent `.gitignore`.
    /// Within each file, the last matching rule wins.
    pub fn is_ignored(&self, path: &str, is_dir: bool) -> bool {
        let mut ignored = false;

        if let Some(rules) = &self.global_exclude
            && let Some(val) = eval_rules(rules, "", path, is_dir)
        {
            ignored = val;
        }
        if let Some(rules) = &self.exclude
            && let Some(val) = eval_rules(rules, "", path, is_dir)
        {
            ignored = val;
        }

        // Check root `.gitignore` (`base_dir == ""`), then each ancestor directory down to parent.
        let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
        if let Ok(at) = self.folders.binary_search_by(|f| f.base_dir.as_bytes().cmp(b""))
            && let Some(val) = eval_rules(&self.folders[at].rules, "", path, is_dir)
        {
            ignored = val;
        }
        if !parent.is_empty() {
            let pb = parent.as_bytes();
            let mut at_slash = 0;
            loop {
                let end = memchr(b'/', &pb[at_slash..]).map_or(pb.len(), |i| at_slash + i);
                let base = &parent[..end];
                if let Ok(idx) =
                    self.folders.binary_search_by(|f| f.base_dir.as_bytes().cmp(base.as_bytes()))
                    && let Some(val) = eval_rules(&self.folders[idx].rules, base, path, is_dir)
                {
                    ignored = val;
                }
                if end == pb.len() {
                    break;
                }
                at_slash = end + 1;
            }
        }

        ignored
    }
}

fn eval_rules(rules: &[IgnoreRule<'_>], base: &str, path: &str, is_dir: bool) -> Option<bool> {
    let rel = if base.is_empty() {
        path
    } else {
        path.strip_prefix(base)?.strip_prefix('/')?
    };
    let mut matched = None;
    for rule in rules {
        if rule.only_dir && !is_dir {
            continue;
        }
        if rule_matches(rule, rel) {
            matched = Some(!rule.negated);
        }
    }
    matched
}

fn parse_rule_line<'a>(
    raw: &[u8],
    alloc: Alloc<'a>,
    plant: impl Planted<GitStatusPlant>,
) -> Option<IgnoreRule<'a>> {
    // Strip UTF-8 BOM on line 1 if present.
    let raw = raw.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(raw);
    if raw.is_empty() || raw[0] == b'#' {
        return None;
    }
    // Trim unescaped trailing spaces.
    let mut end = raw.len();
    while end > 0 && raw[end - 1] == b' ' {
        let mut backslashes = 0usize;
        let mut k = end - 1;
        while k > 0 && raw[k - 1] == b'\\' {
            backslashes += 1;
            k -= 1;
        }
        if backslashes % 2 == 1 {
            break;
        }
        end -= 1;
    }
    let line = &raw[..end];
    if line.is_empty() {
        return None;
    }

    let (negated, rest) = if line[0] == b'!' {
        if plant.has(GitStatusPlant::IgnoreNegation) {
            return None;
        }
        (true, &line[1..])
    } else {
        (false, line)
    };
    if rest.is_empty() {
        return None;
    }

    let text = core::str::from_utf8(rest).ok()?;
    let (only_dir, without_trailing) = match text.strip_suffix('/') {
        Some(s) => (true, s),
        None => (false, text),
    };
    if without_trailing.is_empty() {
        return None;
    }

    let anchored = without_trailing.contains('/');
    let stripped = without_trailing.strip_prefix('/').unwrap_or(without_trailing);
    if stripped.is_empty() {
        return None;
    }

    let literal = !stripped.bytes().any(|b| matches!(b, b'*' | b'?' | b'[' | b'\\'));
    Some(IgnoreRule {
        negated,
        only_dir,
        anchored,
        literal,
        pattern: String::from_str_in(stripped, alloc),
    })
}

fn rule_matches(rule: &IgnoreRule<'_>, rel: &str) -> bool {
    let pat = rule.pattern.as_bytes();
    let target = rel.as_bytes();
    if rule.literal {
        return if rule.anchored {
            target == pat
        } else {
            let basename = match target.iter().rposition(|&b| b == b'/') {
                Some(i) => &target[i + 1..],
                None => target,
            };
            basename == pat
        };
    }

    if rule.anchored {
        glob_match(pat, target)
    } else {
        // Non-anchored pattern without `/` matches the basename (or any component if it has `**`).
        let basename = match target.iter().rposition(|&b| b == b'/') {
            Some(i) => &target[i + 1..],
            None => target,
        };
        glob_match(pat, basename)
    }
}

/// Iterative glob matcher supporting `*`, `?`, `[...]`, `\x`, and `**` path-segment wildcards.
///
/// Bounded linear-like backtracking; never recurses on arbitrary untrusted patterns.
pub fn glob_match(pat: &[u8], text: &[u8]) -> bool {
    glob_match_bounded(pat, text, 0)
}

fn glob_match_bounded(pat: &[u8], text: &[u8], depth: u8) -> bool {
    // Hard cap on `**` nesting depth to guarantee bounded progress on pathological patterns.
    if depth > 16 {
        return false;
    }
    let mut pi = 0usize;
    let mut ti = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_ti: usize = 0;

    while ti < text.len() {
        // Handle `**` segment wildcard.
        if pi + 1 < pat.len() && pat[pi] == b'*' && pat[pi + 1] == b'*' {
            let at_start = pi == 0 || pat[pi - 1] == b'/';
            let after = pi + 2;
            let at_end = after == pat.len() || pat[after] == b'/';
            if at_start && at_end {
                if after == pat.len() {
                    // Trailing `**` matches everything remaining.
                    return true;
                }
                // `**/` matches zero or more path segments.
                let next_pat = &pat[after + 1..];
                if glob_match_bounded(next_pat, &text[ti..], depth + 1) {
                    return true;
                }
                let mut scan = ti;
                while let Some(slash) = memchr(b'/', &text[scan..]) {
                    let after_slash = scan + slash + 1;
                    if glob_match_bounded(next_pat, &text[after_slash..], depth + 1) {
                        return true;
                    }
                    scan = after_slash;
                }
                return false;
            }
        }

        if pi < pat.len() {
            match pat[pi] {
                b'\\' if pi + 1 < pat.len() => {
                    if text[ti] == pat[pi + 1] {
                        pi += 2;
                        ti += 1;
                        continue;
                    }
                }
                b'*' => {
                    star_pi = Some(pi);
                    star_ti = ti;
                    pi += 1;
                    continue;
                }
                b'?' if text[ti] != b'/' => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                b'[' if text[ti] != b'/' => {
                    if let Some((matched, next_pi)) = match_char_class(pat, pi + 1, text[ti])
                        && matched
                    {
                        pi = next_pi;
                        ti += 1;
                        continue;
                    }
                }
                b if b == text[ti] => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                _ => {}
            }
        }

        if let Some(sp) = star_pi {
            // Single `*` never crosses `/`.
            if text[star_ti] != b'/' {
                star_ti += 1;
                ti = star_ti;
                pi = sp + 1;
                continue;
            }
        }
        return false;
    }

    // Consume trailing `*` or `/**` or `**`.
    while pi < pat.len() {
        if pat[pi] == b'*' {
            pi += 1;
        } else if pat[pi..].starts_with(b"/**") && pi + 3 == pat.len() {
            pi += 3;
        } else {
            break;
        }
    }
    pi == pat.len()
}

fn match_char_class(pat: &[u8], mut i: usize, ch: u8) -> Option<(bool, usize)> {
    if i >= pat.len() {
        return None;
    }
    let negated = pat[i] == b'!' || pat[i] == b'^';
    if negated {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < pat.len() {
        if pat[i] == b']' && !first {
            return Some((if negated { !matched } else { matched }, i + 1));
        }
        first = false;
        let lo = if pat[i] == b'\\' && i + 1 < pat.len() {
            i += 1;
            pat[i]
        } else {
            pat[i]
        };
        if i + 2 < pat.len() && pat[i + 1] == b'-' && pat[i + 2] != b']' {
            let hi = pat[i + 2];
            if ch >= lo && ch <= hi {
                matched = true;
            }
            i += 3;
        } else {
            if ch == lo {
                matched = true;
            }
            i += 1;
        }
    }
    None
}
