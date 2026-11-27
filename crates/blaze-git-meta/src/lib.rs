//! Pure `#![no_std]`, zero-unsafe, sans-IO git metadata parsers & GitHub avatar identity resolver
//! built on `blaze-own` explicit allocators (`Alloc<'a>`, `AVec`, `AString`).
//!
//! Covers:
//! - Parsing batched `git log --format=... --name-only` streams into per-entry last-commit
//!   metadata for `ll` / `ls -l` tables (matching GitHub's repository tree view).
//! - Parsing `git blame --porcelain` streams into typed line-by-line blame records.
//! - Extracting GitHub `owner/repo` from remote URLs and resolving GitHub avatar URLs from
//!   author emails (`id+login@users.noreply.github.com`, `login@users.noreply.github.com`, or
//!   commit-SHA lookup via the GitHub API).
//! - Deterministic FNV-1a 64-bit avatar cache keys, 1–2 letter author monograms, muted-pastel
//!   monogram ring hues, and a zero-overhead TSV cache index codec.

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

use core::fmt::Write as _;

use blaze_own::{AString, AVec, Alloc, String, Vec};

/// Parsed GitHub repository coordinates (`owner/repo`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHubRepo<'a> {
    pub owner: AString<'a>,
    pub repo: AString<'a>,
}

/// How an author's GitHub avatar can be resolved without guessing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AvatarSource<'a> {
    /// GitHub numeric noreply email (`123456+login@users.noreply.github.com`):
    /// direct avatar URL `https://avatars.githubusercontent.com/u/{id}?s=64&v=4`.
    GitHubUserId { id: u64, login: AString<'a> },
    /// GitHub login noreply email (`login@users.noreply.github.com`):
    /// direct avatar URL `https://github.com/{login}.png?size=64`.
    GitHubLogin(AString<'a>),
    /// Regular email: driver may query `repos/{owner}/{repo}/commits/{sha}` if on GitHub,
    /// otherwise render the deterministic monogram circle.
    NeedsCommitLookup,
}

/// Last-commit metadata for a single immediate entry (file or folder) in a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirCommit<'a> {
    pub entry_name: AString<'a>,
    pub full_sha: AString<'a>,
    pub short_sha: AString<'a>,
    pub author_name: AString<'a>,
    pub author_email: AString<'a>,
    pub author_time_secs: i64,
    pub subject: AString<'a>,
}

/// Single parsed line from `git blame --porcelain`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameLine<'a> {
    pub line_no: u32,
    pub full_sha: AString<'a>,
    pub short_sha: AString<'a>,
    pub author_name: AString<'a>,
    pub author_email: AString<'a>,
    pub author_time_secs: i64,
    pub summary: AString<'a>,
    pub code: AString<'a>,
}

/// Persisted avatar disk cache index status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AvatarCacheStatus {
    /// `<key>.img` was downloaded and verified on disk.
    Ok,
    /// Lookup returned no GitHub avatar (negatively cached so we don't re-query).
    Miss,
}

/// Single row in the avatar disk cache `index.tsv`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvatarIndexRow<'a> {
    pub email_key: AString<'a>,
    pub fetched_secs: i64,
    pub github_login: AString<'a>,
    pub status: AvatarCacheStatus,
}

/// Parsed summary counts from `git diff --shortstat` or `git log --shortstat`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShortStat {
    pub files_changed: u32,
    pub added_lines: u32,
    pub deleted_lines: u32,
}

/// Compact commit entry with diff churn (`+added` / `-deleted`) parsed from
/// `git log --shortstat --format=%x1e%h%x1f%ct%x1f%s`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchCommit<'a> {
    pub short_sha: AString<'a>,
    pub time_secs: i64,
    pub subject: AString<'a>,
    pub stat: ShortStat,
}

/// Parse a GitHub remote URL (`git@github.com:owner/repo.git`, `https://github.com/owner/repo`,
/// `ssh://git@github.com/owner/repo.git`, `git://github.com/owner/repo.git`) into `owner/repo`.
pub fn parse_github_remote<'a>(url: &str, alloc: Alloc<'a>) -> Option<GitHubRepo<'a>> {
    let u = url.trim();
    let rest = if let Some(r) = u.strip_prefix("git@github.com:") {
        r
    } else if let Some(r) = u.strip_prefix("https://github.com/") {
        r
    } else if let Some(r) = u.strip_prefix("http://github.com/") {
        r
    } else if let Some(r) = u.strip_prefix("ssh://git@github.com/") {
        r
    } else if let Some(r) = u.strip_prefix("git://github.com/") {
        r
    } else {
        return None;
    };
    let rest = rest.trim_matches('/');
    let mut parts = rest.split('/');
    let owner = parts.next()?.trim();
    let raw_repo = parts.next()?.trim();
    let repo = raw_repo.strip_suffix(".git").unwrap_or(raw_repo);
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    if !is_valid_github_slug(owner) || !is_valid_github_slug(repo) {
        return None;
    }
    Some(GitHubRepo {
        owner: String::from_str_in(owner, alloc),
        repo: String::from_str_in(repo, alloc),
    })
}

fn is_valid_github_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Classify a git author email into a direct GitHub avatar source when possible.
pub fn resolve_avatar_source<'a>(author_email: &str, alloc: Alloc<'a>) -> AvatarSource<'a> {
    let trimmed = author_email.trim().trim_matches(|c| c == '<' || c == '>');
    let Some((local, domain)) = trimmed.split_once('@') else {
        return AvatarSource::NeedsCommitLookup;
    };
    if !domain.eq_ignore_ascii_case("users.noreply.github.com") {
        return AvatarSource::NeedsCommitLookup;
    }
    if let Some((id_str, login)) = local.split_once('+')
        && let Ok(id) = id_str.parse::<u64>()
        && !login.is_empty()
        && is_valid_github_slug(login)
    {
        return AvatarSource::GitHubUserId {
            id,
            login: String::from_str_in(login, alloc),
        };
    }
    if !local.is_empty() && is_valid_github_slug(local) {
        return AvatarSource::GitHubLogin(String::from_str_in(local, alloc));
    }
    AvatarSource::NeedsCommitLookup
}

/// Deterministic 64-bit FNV-1a hex key (16 lowercase ASCII hex chars) over the normalized
/// author email. Independent of machine hash seeds so the disk cache is stable across restarts.
pub fn avatar_cache_key_hex(email: &str) -> [u8; 16] {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in email.trim().trim_matches(|c| c == '<' || c == '>').bytes() {
        h ^= u64::from(b.to_ascii_lowercase());
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 16];
    for (i, slot) in out.iter_mut().enumerate() {
        let shift = (15 - i) * 4;
        *slot = HEX[((h >> shift) & 0xf) as usize];
    }
    out
}

/// Format [`avatar_cache_key_hex`] as an explicit-allocator [`AString`].
pub fn avatar_cache_key_in<'a>(email: &str, alloc: Alloc<'a>) -> AString<'a> {
    let bytes = avatar_cache_key_hex(email);
    let s = core::str::from_utf8(&bytes).unwrap_or("0000000000000000");
    String::from_str_in(s, alloc)
}

/// Extract 1–2 uppercase ASCII monogram letters from an author name (`"Andrew Gazelka"` -> `"AG"`,
/// `"alice"` -> `"A"`, `""` -> `"?"`).
pub fn author_initials(name: &str) -> ([u8; 2], usize) {
    let mut out = [b'?'; 2];
    let mut n;
    let mut words = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty());
    if let Some(first) = words.next() {
        out[0] = first.as_bytes()[0].to_ascii_uppercase();
        n = 1;
        let mut last = None;
        for w in words {
            last = Some(w);
        }
        if let Some(l) = last {
            out[1] = l.as_bytes()[0].to_ascii_uppercase();
            n = 2;
        }
    } else {
        n = 1;
    }
    (out, n)
}

/// Deterministic dark-theme-friendly pastel accent RGB for an author's monogram circle.
pub fn author_badge_rgb(email_or_name: &str) -> [u8; 3] {
    const PALETTE: [[u8; 3]; 8] = [
        [0x58, 0x9d, 0xf6], // sky blue
        [0x57, 0xab, 0x5a], // emerald
        [0xe5, 0xb5, 0x67], // amber
        [0xe0, 0x7a, 0x5f], // terracotta
        [0xa3, 0x71, 0xf7], // lavender
        [0x39, 0xc5, 0xcf], // teal
        [0xec, 0x77, 0xab], // rose
        [0x98, 0xc3, 0x79], // sage
    ];
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in email_or_name.trim().bytes() {
        h ^= u64::from(b.to_ascii_lowercase());
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    PALETTE[(h as usize) % PALETTE.len()]
}

/// Parse the output of:
/// `git log -n 512 --format=%x1e%H%x1f%h%x1f%an%x1f%ae%x1f%at%x1f%s%x1f --name-only -- .`
///
/// `rel_dir` is the repo-relative prefix of the listed directory (from `git rev-parse --show-prefix`,
/// e.g. `""` at repo root or `"crates/blaze-shell/"`).
///
/// Returns at most one [`DirCommit`] (the most recent commit) per immediate child entry name.
pub fn parse_dir_commits_in<'a>(
    raw: &str,
    rel_dir: &str,
    max_entries: usize,
    alloc: Alloc<'a>,
) -> AVec<'a, DirCommit<'a>> {
    let prefix = rel_dir.trim_matches('/');
    let mut out: AVec<'a, DirCommit<'a>> = Vec::new_in(alloc);

    for record in raw.split('\x1e') {
        let rec = record.trim_start_matches(['\n', '\r']);
        if rec.is_empty() {
            continue;
        }
        let mut fields = rec.split('\x1f');
        let Some(full_sha) = fields.next().map(str::trim) else {
            continue;
        };
        let Some(short_sha) = fields.next().map(str::trim) else {
            continue;
        };
        let Some(author_name) = fields.next().map(str::trim) else {
            continue;
        };
        let Some(author_email) = fields.next().map(str::trim) else {
            continue;
        };
        let Some(time_str) = fields.next().map(str::trim) else {
            continue;
        };
        let Some(subject) = fields.next().map(str::trim) else {
            continue;
        };
        let files_block = fields.next().unwrap_or("");
        if full_sha.len() < 7 || short_sha.is_empty() {
            continue;
        }
        let author_time_secs = time_str.parse::<i64>().unwrap_or(0);

        for line in files_block.lines() {
            let clean = unquote_git_path(line.trim());
            if clean.is_empty() {
                continue;
            }
            let Some(child) = immediate_child(clean, prefix) else {
                continue;
            };
            if out.iter().any(|e| e.entry_name.as_str() == child) {
                continue;
            }
            out.push(DirCommit {
                entry_name: String::from_str_in(child, alloc),
                full_sha: String::from_str_in(full_sha, alloc),
                short_sha: String::from_str_in(short_sha, alloc),
                author_name: String::from_str_in(author_name, alloc),
                author_email: String::from_str_in(author_email, alloc),
                author_time_secs,
                subject: String::from_str_in(subject, alloc),
            });
            if max_entries > 0 && out.len() >= max_entries {
                return out;
            }
        }
    }
    out
}

fn unquote_git_path(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn immediate_child<'p>(repo_path: &'p str, prefix: &str) -> Option<&'p str> {
    let sub = if prefix.is_empty() {
        repo_path
    } else {
        let rest = repo_path.strip_prefix(prefix)?;
        rest.strip_prefix('/')?
    };
    let child = sub.split('/').next().unwrap_or("");
    if child.is_empty() { None } else { Some(child) }
}

/// Cached commit header metadata inside [`parse_blame_porcelain_in`].
struct BlameCommitCache<'a> {
    full_sha: AString<'a>,
    author_name: AString<'a>,
    author_email: AString<'a>,
    author_time_secs: i64,
    summary: AString<'a>,
}

/// Parse `git blame --porcelain <file>` output into typed [`BlameLine`] records (bounded to
/// `max_lines` rows so giant generated files never exhaust memory).
pub fn parse_blame_porcelain_in<'a>(
    raw: &str,
    max_lines: usize,
    alloc: Alloc<'a>,
) -> AVec<'a, BlameLine<'a>> {
    let mut out: AVec<'a, BlameLine<'a>> = Vec::new_in(alloc);
    let mut commits: AVec<'a, BlameCommitCache<'a>> = Vec::new_in(alloc);
    let mut lines = raw.lines();

    while let Some(header) = lines.next() {
        if max_lines > 0 && out.len() >= max_lines {
            break;
        }
        let mut hparts = header.split_ascii_whitespace();
        let Some(full_sha) = hparts.next() else {
            continue;
        };
        let _orig_line = hparts.next();
        let Some(final_line_s) = hparts.next() else {
            continue;
        };
        if full_sha.len() < 7 || !full_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let line_no = final_line_s.parse::<u32>().unwrap_or(0);

        let mut author_name: Option<&str> = None;
        let mut author_email: Option<&str> = None;
        let mut author_time_secs: Option<i64> = None;
        let mut summary: Option<&str> = None;
        let mut code_line: &str = "";

        for l in lines.by_ref() {
            if let Some(code) = l.strip_prefix('\t') {
                code_line = code;
                break;
            }
            if let Some(v) = l.strip_prefix("author ") {
                author_name = Some(v.trim());
            } else if let Some(v) = l.strip_prefix("author-mail ") {
                author_email = Some(v.trim().trim_matches(|c| c == '<' || c == '>'));
            } else if let Some(v) = l.strip_prefix("author-time ") {
                author_time_secs = v.trim().parse::<i64>().ok();
            } else if let Some(v) = l.strip_prefix("summary ") {
                summary = Some(v.trim());
            }
        }

        // Update or look up cached commit metadata (git omits repeated commit headers!).
        let cached_idx = commits.iter().position(|c| c.full_sha.as_str() == full_sha);
        let idx = if let Some(i) = cached_idx {
            if let Some(an) = author_name {
                commits[i].author_name = String::from_str_in(an, alloc);
            }
            if let Some(ae) = author_email {
                commits[i].author_email = String::from_str_in(ae, alloc);
            }
            if let Some(at) = author_time_secs {
                commits[i].author_time_secs = at;
            }
            if let Some(sm) = summary {
                commits[i].summary = String::from_str_in(sm, alloc);
            }
            i
        } else {
            let i = commits.len();
            commits.push(BlameCommitCache {
                full_sha: String::from_str_in(full_sha, alloc),
                author_name: String::from_str_in(author_name.unwrap_or(""), alloc),
                author_email: String::from_str_in(author_email.unwrap_or(""), alloc),
                author_time_secs: author_time_secs.unwrap_or(0),
                summary: String::from_str_in(summary.unwrap_or(""), alloc),
            });
            i
        };

        let meta = &commits[idx];
        let short_end = full_sha.len().min(7);
        out.push(BlameLine {
            line_no,
            full_sha: String::from_str_in(full_sha, alloc),
            short_sha: String::from_str_in(&full_sha[..short_end], alloc),
            author_name: String::from_str_in(meta.author_name.as_str(), alloc),
            author_email: String::from_str_in(meta.author_email.as_str(), alloc),
            author_time_secs: meta.author_time_secs,
            summary: String::from_str_in(meta.summary.as_str(), alloc),
            code: String::from_str_in(code_line, alloc),
        });
    }

    out
}

/// Parse the persistent avatar disk cache `index.tsv`.
pub fn parse_avatar_index_in<'a>(raw: &str, alloc: Alloc<'a>) -> AVec<'a, AvatarIndexRow<'a>> {
    let mut out = Vec::new_in(alloc);
    for line in raw.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let mut parts = l.split('\t');
        let Some(key) = parts.next() else {
            continue;
        };
        let Some(ts_s) = parts.next() else {
            continue;
        };
        let login = parts.next().unwrap_or("");
        let status_s = parts.next().unwrap_or("miss");
        let Ok(fetched_secs) = ts_s.parse::<i64>() else {
            continue;
        };
        let status = if status_s == "ok" {
            AvatarCacheStatus::Ok
        } else {
            AvatarCacheStatus::Miss
        };
        out.push(AvatarIndexRow {
            email_key: String::from_str_in(key, alloc),
            fetched_secs,
            github_login: String::from_str_in(login, alloc),
            status,
        });
    }
    out
}

/// Serialize avatar cache index rows deterministically as TSV.
pub fn format_avatar_index_in<'a>(rows: &[AvatarIndexRow<'_>], alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    for r in rows {
        let st = match r.status {
            AvatarCacheStatus::Ok => "ok",
            AvatarCacheStatus::Miss => "miss",
        };
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}",
            r.email_key.as_str(),
            r.fetched_secs,
            r.github_login.as_str(),
            st
        );
    }
    out
}

/// Parse one `git diff --shortstat` / `git log --shortstat` summary line into [`ShortStat`].
pub fn parse_git_shortstat(line: &str) -> ShortStat {
    let mut stat = ShortStat::default();
    for part in line.split(',') {
        let mut toks = part.split_whitespace();
        let Some(num_s) = toks.next() else {
            continue;
        };
        let Ok(num) = num_s.parse::<u32>() else {
            continue;
        };
        let rest = toks.next().unwrap_or("");
        if rest.starts_with("file") {
            stat.files_changed = num;
        } else if rest.starts_with("insertion") {
            stat.added_lines = num;
        } else if rest.starts_with("deletion") {
            stat.deleted_lines = num;
        }
    }
    stat
}

/// Parse `git log --shortstat --format=%x1e%h%x1f%ct%x1f%s` output into at most `limit`
/// [`BranchCommit`] entries (unbounded when `limit == 0`).
pub fn parse_git_log_commits_in<'a>(
    stdout: &str,
    limit: usize,
    alloc: Alloc<'a>,
) -> AVec<'a, BranchCommit<'a>> {
    let mut commits: AVec<'a, BranchCommit<'a>> = Vec::with_capacity_in(limit.min(64), alloc);
    for rec in stdout.split('\x1e') {
        if limit > 0 && commits.len() >= limit {
            break;
        }
        let rec = rec.trim();
        if rec.is_empty() {
            continue;
        }
        let mut lines = rec.lines();
        let Some(header) = lines.next() else {
            continue;
        };
        let mut fields = header.splitn(3, '\x1f');
        let Some(sha) = fields.next().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        let time_secs = fields
            .next()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0);
        let subject = fields.next().unwrap_or("").trim();
        let mut stat = ShortStat::default();
        for stat_line in lines {
            let trimmed = stat_line.trim();
            if trimmed.contains("file")
                || trimmed.contains("insertion")
                || trimmed.contains("deletion")
            {
                stat = parse_git_shortstat(trimmed);
                break;
            }
        }
        commits.push(BranchCommit {
            short_sha: String::from_str_in(sha, alloc),
            time_secs,
            subject: String::from_str_in(subject, alloc),
            stat,
        });
    }
    commits
}

/// Split `"WIP on main: c54fc93e6 Merge..."` or `"On main: pre-pull-backup"` into
/// `(branch, summary)` with zero allocations.
pub fn parse_stash_header(raw: &str) -> (&str, &str) {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed
        .strip_prefix("WIP on ")
        .or_else(|| trimmed.strip_prefix("On "))
        && let Some((br, msg)) = rest.split_once(':')
    {
        return (br.trim(), msg.trim());
    }
    ("", trimmed)
}

/// Parse `git diff --numstat` lines (`<added>\t<deleted>\t<path>`) into `(path, added, deleted)`
/// entries (bounded to 256 lines). Binary entries (`-\t-\t<path>`) are omitted.
pub fn parse_git_diff_numstat_in<'a>(
    stdout: &str,
    alloc: Alloc<'a>,
) -> AVec<'a, (AString<'a>, u32, u32)> {
    let mut out: AVec<'a, (AString<'a>, u32, u32)> = Vec::new_in(alloc);
    for line in stdout.lines().take(256) {
        let mut parts = line.splitn(3, '\t');
        let (Some(a_s), Some(d_s), Some(raw_path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let path = raw_path.trim_end_matches('\r');
        if path.is_empty() || a_s == "-" || d_s == "-" {
            continue;
        }
        if let (Ok(added), Ok(deleted)) = (a_s.parse::<u32>(), d_s.parse::<u32>()) {
            out.push((String::from_str_in(path, alloc), added, deleted));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_remotes() {
        let alloc = Alloc::global();
        let ssh = parse_github_remote("git@github.com:indexable-inc/blaze.git", alloc).unwrap();
        assert_eq!(ssh.owner.as_str(), "indexable-inc");
        assert_eq!(ssh.repo.as_str(), "blaze");

        let https = parse_github_remote("https://github.com/rust-lang/rust", alloc).unwrap();
        assert_eq!(https.owner.as_str(), "rust-lang");
        assert_eq!(https.repo.as_str(), "rust");

        assert!(parse_github_remote("https://gitlab.com/foo/bar.git", alloc).is_none());
    }

    #[test]
    fn resolves_github_noreply_avatars() {
        let alloc = Alloc::global();
        match resolve_avatar_source("7644264+andrewgazelka@users.noreply.github.com", alloc) {
            AvatarSource::GitHubUserId { id, login } => {
                assert_eq!(id, 7_644_264);
                assert_eq!(login.as_str(), "andrewgazelka");
            }
            other => panic!("expected GitHubUserId, got {other:?}"),
        }
        match resolve_avatar_source("octocat@users.noreply.github.com", alloc) {
            AvatarSource::GitHubLogin(login) => assert_eq!(login.as_str(), "octocat"),
            other => panic!("expected GitHubLogin, got {other:?}"),
        }
        assert_eq!(
            resolve_avatar_source("andrew@indexable.com", alloc),
            AvatarSource::NeedsCommitLookup
        );
    }

    #[test]
    fn parses_batched_dir_log_first_commit_per_child() {
        let alloc = Alloc::global();
        let raw = "\x1eaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\x1faaaaaaa\x1fAndrew\x1fa@b.c\x1f1700000002\x1fnewer touch\x1f\nsrc/main.rs\nCargo.toml\n\x1ebbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\x1fbbbbbbb\x1fBob\x1fb@b.c\x1f1700000001\x1folder touch\x1f\nsrc/lib.rs\nREADME.md\n";
        let got = parse_dir_commits_in(raw, "", 64, alloc);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].entry_name.as_str(), "src");
        assert_eq!(got[0].short_sha.as_str(), "aaaaaaa");
        assert_eq!(got[1].entry_name.as_str(), "Cargo.toml");
        assert_eq!(got[1].short_sha.as_str(), "aaaaaaa");
        assert_eq!(got[2].entry_name.as_str(), "README.md");
        assert_eq!(got[2].short_sha.as_str(), "bbbbbbb");
    }

    #[test]
    fn parses_git_blame_porcelain_with_repeated_commits() {
        let alloc = Alloc::global();
        let raw = "\
1111111111111111111111111111111111111111 1 1 2
author Andrew Gazelka
author-mail <7644264+andrewgazelka@users.noreply.github.com>
author-time 1700000000
summary initial commit
filename src/main.rs
\tfn main() {
1111111111111111111111111111111111111111 2 2
\t    println!(\"hi\");
";
        let lines = parse_blame_porcelain_in(raw, 100, alloc);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].line_no, 1);
        assert_eq!(lines[0].short_sha.as_str(), "1111111");
        assert_eq!(lines[0].author_name.as_str(), "Andrew Gazelka");
        assert_eq!(lines[0].code.as_str(), "fn main() {");
        // Second line inherits cached commit metadata cleanly!
        assert_eq!(lines[1].line_no, 2);
        assert_eq!(lines[1].author_name.as_str(), "Andrew Gazelka");
        assert_eq!(lines[1].summary.as_str(), "initial commit");
        assert_eq!(lines[1].code.as_str(), "    println!(\"hi\");");
    }

    #[test]
    fn avatar_index_round_trips() {
        let alloc = Alloc::global();
        let mut rows = Vec::new_in(alloc);
        rows.push(AvatarIndexRow {
            email_key: avatar_cache_key_in("Andrew@Indexable.com", alloc),
            fetched_secs: 1_795_000_000,
            github_login: String::from_str_in("andrewgazelka", alloc),
            status: AvatarCacheStatus::Ok,
        });
        let encoded = format_avatar_index_in(rows.as_slice(), alloc);
        let decoded = parse_avatar_index_in(encoded.as_str(), alloc);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].github_login.as_str(), "andrewgazelka");
        assert_eq!(decoded[0].status, AvatarCacheStatus::Ok);
    }

    #[test]
    fn deterministic_initials_and_keys() {
        let (init, len) = author_initials("Andrew Gazelka");
        assert_eq!(&init[..len], b"AG");
        assert_eq!(avatar_cache_key_hex("A@B.COM"), avatar_cache_key_hex("  <a@b.com> "));
    }

    #[test]
    fn parses_git_shortstat_variants() {
        assert_eq!(
            parse_git_shortstat(" 3 files changed, 42 insertions(+), 7 deletions(-)"),
            ShortStat { files_changed: 3, added_lines: 42, deleted_lines: 7 }
        );
        assert_eq!(
            parse_git_shortstat(" 1 file changed, 1 insertion(+)"),
            ShortStat { files_changed: 1, added_lines: 1, deleted_lines: 0 }
        );
        assert_eq!(
            parse_git_shortstat(" 2 files changed, 9 deletions(-)"),
            ShortStat { files_changed: 2, added_lines: 0, deleted_lines: 9 }
        );
        assert_eq!(parse_git_shortstat(""), ShortStat::default());
    }

    #[test]
    fn parses_git_log_commits_with_shortstat_and_limit() {
        let alloc = Alloc::global();
        let raw = "\
\x1ea1b2c3d\x1f1700000100\x1ffeat commit two
 1 file changed, 2 insertions(+), 1 deletion(-)

\x1ee4f5a6b\x1f1700000050\x1ffeat commit one
 2 files changed, 10 insertions(+)

\x1e9999999\x1f1700000000\x1fempty commit
";
        let all = parse_git_log_commits_in(raw, 5, alloc);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].short_sha.as_str(), "a1b2c3d");
        assert_eq!(all[0].time_secs, 1_700_000_100);
        assert_eq!(all[0].subject.as_str(), "feat commit two");
        assert_eq!(
            all[0].stat,
            ShortStat { files_changed: 1, added_lines: 2, deleted_lines: 1 }
        );
        assert_eq!(all[1].short_sha.as_str(), "e4f5a6b");
        assert_eq!(
            all[1].stat,
            ShortStat { files_changed: 2, added_lines: 10, deleted_lines: 0 }
        );
        assert_eq!(all[2].short_sha.as_str(), "9999999");
        assert_eq!(all[2].stat, ShortStat::default());

        let capped = parse_git_log_commits_in(raw, 2, alloc);
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[1].short_sha.as_str(), "e4f5a6b");
    }

    #[test]
    fn parses_stash_headers_zero_alloc() {
        assert_eq!(
            parse_stash_header("WIP on main: c54fc93e6 Merge PR #12"),
            ("main", "c54fc93e6 Merge PR #12")
        );
        assert_eq!(
            parse_stash_header("  On feat/tiling: pre-pull-backup  "),
            ("feat/tiling", "pre-pull-backup")
        );
        assert_eq!(
            parse_stash_header("custom raw message"),
            ("", "custom raw message")
        );
    }

    #[test]
    fn parses_git_diff_numstat_text_and_skips_binary() {
        let alloc = Alloc::global();
        let raw = "12\t3\tsrc/prompt.rs\n-\t-\tassets/logo.png\n0\t5\told.md\r\n\n";
        let got = parse_git_diff_numstat_in(raw, alloc);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0.as_str(), "src/prompt.rs");
        assert_eq!((got[0].1, got[0].2), (12, 3));
        assert_eq!(got[1].0.as_str(), "old.md");
        assert_eq!((got[1].1, got[1].2), (0, 5));
    }
}
