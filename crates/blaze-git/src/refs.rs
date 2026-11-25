//! Parsing of Git references, `HEAD`, `packed-refs`, `.git/config`, `logs/refs/stash`,
//! worktree pointers (`gitdir:` / `commondir`), and in-progress operation state (`git_state`).

#![blaze::sans_io]

use blaze_git_fetch::Oid;
use blaze_memchr::{memchr, memchr_iter};
use blaze_own::{AString, Alloc, String};

use crate::{ErrorKind, GitError, GitFile, RepoState};

/// Parsed content of `.git/HEAD`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeadFile<'a> {
    /// Symbolic ref (`ref: refs/heads/main`), path trimmed of whitespace.
    Symbolic(&'a str),
    /// Detached commit OID.
    Detached(Oid),
}

/// Parse `.git/HEAD` bytes.
pub fn parse_head(bytes: &[u8]) -> Result<HeadFile<'_>, GitError> {
    let text = core::str::from_utf8(bytes)
        .map_err(|_| GitError { file: GitFile::Head, at: 0, kind: ErrorKind::Malformed })?;
    let trimmed = text.trim();
    if let Some(target) = trimmed.strip_prefix("ref:") {
        let ref_name = target.trim();
        if ref_name.is_empty() || !ref_name_safe(ref_name) {
            return Err(GitError { file: GitFile::Head, at: 4, kind: ErrorKind::Malformed });
        }
        return Ok(HeadFile::Symbolic(ref_name));
    }
    if let Some(oid) = Oid::parse_hex(trimmed.as_bytes()) {
        return Ok(HeadFile::Detached(oid));
    }
    Err(GitError { file: GitFile::Head, at: 0, kind: ErrorKind::Malformed })
}

/// Parse a loose ref file (either 40-hex OID or symbolic `ref: ...`).
pub fn parse_loose_ref(bytes: &[u8]) -> Result<HeadFile<'_>, GitError> {
    let text = core::str::from_utf8(bytes)
        .map_err(|_| GitError { file: GitFile::Ref, at: 0, kind: ErrorKind::Malformed })?;
    let trimmed = text.trim();
    if let Some(target) = trimmed.strip_prefix("ref:") {
        let ref_name = target.trim();
        if ref_name.is_empty() || !ref_name_safe(ref_name) {
            return Err(GitError { file: GitFile::Ref, at: 4, kind: ErrorKind::Malformed });
        }
        return Ok(HeadFile::Symbolic(ref_name));
    }
    if let Some(oid) = Oid::parse_hex(trimmed.as_bytes()) {
        return Ok(HeadFile::Detached(oid));
    }
    Err(GitError { file: GitFile::Ref, at: 0, kind: ErrorKind::Malformed })
}

/// Validate a ref path stays inside the git directory (`refs/...` with no `..` or `\`).
pub fn ref_name_safe(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains('\\')
        && name.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Parse a `.git` file pointing to a worktree/submodule gitdir (`gitdir: <path>`).
pub fn parse_gitdir_file(bytes: &[u8]) -> Result<&str, GitError> {
    let text = core::str::from_utf8(bytes)
        .map_err(|_| GitError { file: GitFile::GitPointer, at: 0, kind: ErrorKind::Malformed })?;
    let first = text.lines().next().unwrap_or("");
    let target = first
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(GitError { file: GitFile::GitPointer, at: 0, kind: ErrorKind::Malformed })?;
    Ok(target)
}

/// Look up `ref_name` in `.git/packed-refs` bytes using SIMD newline scanning.
pub fn lookup_packed_ref(packed_refs: &[u8], ref_name: &str) -> Option<Oid> {
    let target = ref_name.as_bytes();
    let mut pos = 0;
    while pos < packed_refs.len() {
        let nl = memchr(b'\n', &packed_refs[pos..]).map_or(packed_refs.len(), |i| pos + i);
        let mut line = &packed_refs[pos..nl];
        if let Some(stripped) = line.strip_suffix(b"\r") {
            line = stripped;
        }
        pos = nl + 1;
        if line.len() > 41 && line[0] != b'#' && line[0] != b'^' && line[40] == b' ' && &line[41..] == target {
            return Oid::parse_hex(&line[..40]);
        }
    }
    None
}

/// Find a tag name in `packed-refs` whose direct OID or peeled `^<oid>` matches `target_oid`.
///
/// Returns the tag short name (stripped of `refs/tags/`).
pub fn find_packed_tag_for_oid<'p>(packed_refs: &'p [u8], target_oid: Oid) -> Option<&'p str> {
    let target_hex = target_oid.hex();
    let mut last_tag: Option<&'p [u8]> = None;
    let mut pos = 0;
    while pos < packed_refs.len() {
        let nl = memchr(b'\n', &packed_refs[pos..]).map_or(packed_refs.len(), |i| pos + i);
        let mut line = &packed_refs[pos..nl];
        if let Some(stripped) = line.strip_suffix(b"\r") {
            line = stripped;
        }
        pos = nl + 1;
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        if line[0] == b'^' {
            if let Some(tag) = last_tag
                && line.len() >= 41
                && line[1..41].eq_ignore_ascii_case(&target_hex)
            {
                return core::str::from_utf8(tag).ok();
            }
            last_tag = None;
            continue;
        }
        if line.len() > 41 && line[40] == b' ' {
            if let Some(tag) = line[41..].strip_prefix(b"refs/tags/") {
                if line[..40].eq_ignore_ascii_case(&target_hex) {
                    return core::str::from_utf8(tag).ok();
                }
                last_tag = Some(tag);
            } else {
                last_tag = None;
            }
        } else {
            last_tag = None;
        }
    }
    None
}

/// Count stash entries in `logs/refs/stash` using SIMD newline scanning.
pub fn count_stashes(stash_log: &[u8]) -> u32 {
    if stash_log.is_empty() {
        return 0;
    }
    let mut count = 0u32;
    let mut start = 0usize;
    for nl in memchr_iter(b'\n', stash_log) {
        if nl > start {
            count = count.saturating_add(1);
        }
        start = nl + 1;
    }
    if start < stash_log.len() {
        count = count.saturating_add(1);
    }
    count
}

/// Relevant subset of `.git/config` for Starship prompt and status computation.
#[derive(PartialEq, Eq, Debug)]
pub struct RepoConfig<'a> {
    pub filemode: bool,
    pub bare: bool,
    pub autocrlf: AutoCrlf,
    /// Configured upstream remote for the queried branch (`[branch "<name>"] remote = ...`).
    pub branch_remote: Option<AString<'a>>,
    /// Configured upstream merge ref for the queried branch (`[branch "<name>"] merge = ...`).
    pub branch_merge: Option<AString<'a>>,
    /// Optional `core.excludesfile` path if set in repo config.
    pub excludes_file: Option<AString<'a>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AutoCrlf {
    False,
    True,
    Input,
}

impl<'a> RepoConfig<'a> {
    pub fn default_config() -> RepoConfig<'a> {
        RepoConfig {
            filemode: true,
            bare: false,
            autocrlf: AutoCrlf::False,
            branch_remote: None,
            branch_merge: None,
            excludes_file: None,
        }
    }

    /// Resolve the local tracking ref path (e.g. `refs/remotes/origin/main`) and display
    /// `(remote, short_branch)` from `[branch "<current>"]` settings.
    pub fn upstream_ref(&self, alloc: Alloc<'a>) -> Option<UpstreamRef<'a>> {
        let remote = self.branch_remote.as_ref()?;
        let merge = self.branch_merge.as_ref()?;
        let short_branch = merge.as_str().strip_prefix("refs/heads/").unwrap_or(merge.as_str());
        let ref_path = if remote.as_str() == "." {
            String::from_str_in(merge.as_str(), alloc)
        } else if merge.as_str().starts_with("refs/heads/") {
            let mut s = String::from_str_in("refs/remotes/", alloc);
            s.push_str(remote.as_str());
            s.push('/');
            s.push_str(short_branch);
            s
        } else {
            String::from_str_in(merge.as_str(), alloc)
        };
        if !ref_name_safe(ref_path.as_str()) {
            return None;
        }
        Some(UpstreamRef {
            remote: String::from_str_in(remote.as_str(), alloc),
            branch: String::from_str_in(short_branch, alloc),
            ref_path,
        })
    }
}

/// Resolved upstream tracking reference.
#[derive(PartialEq, Eq, Debug)]
pub struct UpstreamRef<'a> {
    pub remote: AString<'a>,
    pub branch: AString<'a>,
    pub ref_path: AString<'a>,
}

/// Parse `.git/config` in a single pass, extracting `[core]` settings and `[branch "<branch>"]`.
pub fn parse_config<'a>(bytes: &[u8], branch: Option<&str>, alloc: Alloc<'a>) -> RepoConfig<'a> {
    let mut cfg = RepoConfig::default_config();

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Section {
        Other,
        Core,
        TargetBranch,
    }
    let mut section = Section::Other;

    let mut pos = 0;
    while pos < bytes.len() {
        let nl = memchr(b'\n', &bytes[pos..]).map_or(bytes.len(), |i| pos + i);
        let mut line = &bytes[pos..nl];
        if let Some(stripped) = line.strip_suffix(b"\r") {
            line = stripped;
        }
        pos = nl + 1;

        let line = trim_ws(line);
        if line.is_empty() || line[0] == b'#' || line[0] == b';' {
            continue;
        }

        if line[0] == b'[' {
            let Some(close) = memchr(b']', line) else {
                continue;
            };
            let header = trim_ws(&line[1..close]);
            if header.eq_ignore_ascii_case(b"core") {
                section = Section::Core;
            } else if let Some(b_name) = branch
                && let Some(sub) = parse_branch_section(header)
                && sub == b_name.as_bytes()
            {
                section = Section::TargetBranch;
            } else {
                section = Section::Other;
            }
            continue;
        }

        if section == Section::Other {
            continue;
        }

        let Some(eq) = memchr(b'=', line) else {
            continue;
        };
        let key = trim_ws(&line[..eq]);
        let val = strip_config_value(&line[eq + 1..]);

        match section {
            Section::Core => {
                if key.eq_ignore_ascii_case(b"filemode") {
                    if let Some(b) = parse_bool(val) {
                        cfg.filemode = b;
                    }
                } else if key.eq_ignore_ascii_case(b"bare") {
                    if let Some(b) = parse_bool(val) {
                        cfg.bare = b;
                    }
                } else if key.eq_ignore_ascii_case(b"autocrlf") {
                    if val.eq_ignore_ascii_case(b"input") {
                        cfg.autocrlf = AutoCrlf::Input;
                    } else if let Some(b) = parse_bool(val) {
                        cfg.autocrlf = if b { AutoCrlf::True } else { AutoCrlf::False };
                    }
                } else if key.eq_ignore_ascii_case(b"excludesfile")
                    && let Ok(s) = core::str::from_utf8(val)
                    && !s.is_empty()
                {
                    cfg.excludes_file = Some(String::from_str_in(s, alloc));
                }
            }
            Section::TargetBranch => {
                if key.eq_ignore_ascii_case(b"remote") {
                    if let Ok(s) = core::str::from_utf8(val) {
                        cfg.branch_remote = Some(String::from_str_in(s, alloc));
                    }
                } else if key.eq_ignore_ascii_case(b"merge")
                    && let Ok(s) = core::str::from_utf8(val)
                {
                    cfg.branch_merge = Some(String::from_str_in(s, alloc));
                }
            }
            Section::Other => {}
        }
    }

    cfg
}

fn parse_branch_section(header: &[u8]) -> Option<&[u8]> {
    if header.len() < 8 || !header[..6].eq_ignore_ascii_case(b"branch") {
        return None;
    }
    let rest = trim_ws(&header[6..]);
    if rest.len() >= 2 && rest[0] == b'"' && rest[rest.len() - 1] == b'"' {
        Some(&rest[1..rest.len() - 1])
    } else {
        None
    }
}

fn trim_ws(mut b: &[u8]) -> &[u8] {
    while let Some(&first) = b.first() {
        if first == b' ' || first == b'\t' {
            b = &b[1..];
        } else {
            break;
        }
    }
    while let Some(&last) = b.last() {
        if last == b' ' || last == b'\t' {
            b = &b[..b.len() - 1];
        } else {
            break;
        }
    }
    b
}

/// Strip inline comments (`#` or `;` outside quotes) and surrounding quotes from a config value.
fn strip_config_value(raw: &[u8]) -> &[u8] {
    let mut in_quotes = false;
    let mut escaped = false;
    let mut end = raw.len();
    for (i, &b) in raw.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' if in_quotes => escaped = true,
            b'"' => in_quotes = !in_quotes,
            b'#' | b';' if !in_quotes => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    let trimmed = trim_ws(&raw[..end]);
    if trimmed.len() >= 2 && trimmed[0] == b'"' && trimmed[trimmed.len() - 1] == b'"' {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

fn parse_bool(val: &[u8]) -> Option<bool> {
    if val.eq_ignore_ascii_case(b"true")
        || val.eq_ignore_ascii_case(b"yes")
        || val.eq_ignore_ascii_case(b"on")
        || val == b"1"
    {
        Some(true)
    } else if val.eq_ignore_ascii_case(b"false")
        || val.eq_ignore_ascii_case(b"no")
        || val.eq_ignore_ascii_case(b"off")
        || val == b"0"
    {
        Some(false)
    } else {
        None
    }
}

/// Marker files inspected to determine [`RepoState`].
#[derive(Clone, Copy, Default)]
pub struct StateFiles<'a> {
    pub rebase_merge_msgnum: Option<&'a [u8]>,
    pub rebase_merge_end: Option<&'a [u8]>,
    pub rebase_merge_interactive: bool,
    pub rebase_apply_rebasing: bool,
    pub rebase_apply_applying: bool,
    pub rebase_apply_next: Option<&'a [u8]>,
    pub rebase_apply_last: Option<&'a [u8]>,
    pub merge_head: bool,
    pub cherry_pick_head: bool,
    pub revert_head: bool,
    pub bisect_log: bool,
}

fn parse_u32_line(bytes: Option<&[u8]>) -> Option<u32> {
    let text = core::str::from_utf8(bytes?).ok()?;
    text.trim().parse::<u32>().ok()
}

/// Determine the repository's active operation state in Git's exact precedence order.
pub fn detect_repo_state(files: &StateFiles<'_>) -> RepoState {
    if files.rebase_merge_msgnum.is_some()
        || files.rebase_merge_end.is_some()
        || files.rebase_merge_interactive
    {
        return RepoState::Rebase {
            interactive: files.rebase_merge_interactive,
            step: parse_u32_line(files.rebase_merge_msgnum),
            total: parse_u32_line(files.rebase_merge_end),
        };
    }
    if files.rebase_apply_rebasing
        || files.rebase_apply_applying
        || files.rebase_apply_next.is_some()
        || files.rebase_apply_last.is_some()
    {
        let step = parse_u32_line(files.rebase_apply_next);
        let total = parse_u32_line(files.rebase_apply_last);
        return if files.rebase_apply_rebasing {
            RepoState::Rebase { interactive: false, step, total }
        } else if files.rebase_apply_applying {
            RepoState::Am { step, total }
        } else {
            RepoState::AmOrRebase { step, total }
        };
    }
    if files.merge_head {
        return RepoState::Merge;
    }
    if files.cherry_pick_head {
        return RepoState::CherryPick;
    }
    if files.revert_head {
        return RepoState::Revert;
    }
    if files.bisect_log {
        return RepoState::Bisect;
    }
    RepoState::Clean
}
