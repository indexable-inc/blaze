//! Pure `#![no_std]`, zero-unsafe, sans-IO remote package & repository autocomplete engine.
//!
//! Detects remote completion contexts from shell input (`git clone`, `gh repo`, `cargo add/install`,
//! `npm/bun/pnpm/yarn`, `brew`), builds percent-encoded API endpoints, and parses JSON responses
//! with a zero-dependency streaming recursive-descent scanner over explicit `blaze_own::Alloc<'a>`.

use core::fmt::Write as _;

use blaze_own::{AString, AVec, Alloc, String, Vec};

const MAX_JSON_DEPTH: usize = 64;

/// Remote registry or hosting provider powering live autocomplete.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RemoteProvider {
    /// GitHub repository search (`git clone` inserts full `.git` URL; `gh repo` inserts `owner/repo`).
    GitHubRepo { insert_git_url: bool },
    /// crates.io package search (`cargo add`, `cargo install`).
    CratesIo,
    /// npm registry search (`npm`, `bun`, `pnpm`, `yarn`).
    NpmRegistry,
    /// Homebrew formula lookup (`brew install`, `brew info`).
    Homebrew,
}

impl RemoteProvider {
    /// Placeholder hint shown when the user's query is empty or too short to fire a request.
    pub const fn hint(self) -> &'static str {
        match self {
            Self::GitHubRepo { .. } => {
                "Search GitHub repos (e.g. ripgrep, BurntSushi/ripgrep)..."
            }
            Self::CratesIo => "Search crates.io packages (e.g. serde, tokio)...",
            Self::NpmRegistry => "Search npm packages (e.g. zod, typescript)...",
            Self::Homebrew => "Search Homebrew formulae (e.g. ripgrep, jq)...",
        }
    }
}

/// Active remote completion context parsed from the command line before the cursor.
#[derive(Debug, PartialEq, Eq)]
pub struct RemoteContext<'a> {
    pub provider: RemoteProvider,
    pub query: AString<'a>,
    /// Byte offset in `line_before_cursor` where the active query token begins.
    pub replace_start: usize,
    /// Helpful placeholder shown when `query` is empty or too short.
    pub hint: &'static str,
}

impl<'a> RemoteContext<'a> {
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> RemoteContext<'b> {
        RemoteContext {
            provider: self.provider,
            query: String::from_str_in(self.query.as_str(), alloc),
            replace_start: self.replace_start,
            hint: self.hint,
        }
    }
}

/// Concrete remote API endpoint and cache key derived from `(provider, query)`.
#[derive(Debug, PartialEq, Eq)]
pub struct RemoteEndpoint<'a> {
    pub url: AString<'a>,
    /// Relative GitHub API path for `gh api <path>` when authenticated via `gh` CLI.
    pub gh_api_path: Option<AString<'a>>,
    pub cache_key: AString<'a>,
}

impl<'a> RemoteEndpoint<'a> {
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> RemoteEndpoint<'b> {
        RemoteEndpoint {
            url: String::from_str_in(self.url.as_str(), alloc),
            gh_api_path: self
                .gh_api_path
                .as_ref()
                .map(|s| String::from_str_in(s.as_str(), alloc)),
            cache_key: String::from_str_in(self.cache_key.as_str(), alloc),
        }
    }
}

/// One remote autocomplete candidate parsed from a provider response.
#[derive(Debug, PartialEq, Eq)]
pub struct RemoteCandidate<'a> {
    pub label: AString<'a>,
    pub insert: AString<'a>,
    pub detail: AString<'a>,
    pub doc: AString<'a>,
    pub owner_login: Option<AString<'a>>,
    pub avatar_url: Option<AString<'a>>,
    pub popularity: u64,
}

impl<'a> RemoteCandidate<'a> {
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> RemoteCandidate<'b> {
        RemoteCandidate {
            label: String::from_str_in(self.label.as_str(), alloc),
            insert: String::from_str_in(self.insert.as_str(), alloc),
            detail: String::from_str_in(self.detail.as_str(), alloc),
            doc: String::from_str_in(self.doc.as_str(), alloc),
            owner_login: self
                .owner_login
                .as_ref()
                .map(|s| String::from_str_in(s.as_str(), alloc)),
            avatar_url: self
                .avatar_url
                .as_ref()
                .map(|s| String::from_str_in(s.as_str(), alloc)),
            popularity: self.popularity,
        }
    }
}

#[derive(Clone, Copy)]
struct Token<'s> {
    raw: &'s str,
    start: usize,
}

/// Inspect `line_before_cursor` and return a [`RemoteContext`] if the cursor is positioned on a
/// remote package or repository argument.
pub fn detect_remote_context<'a>(
    line_before_cursor: &str,
    alloc: Alloc<'a>,
) -> Option<RemoteContext<'a>> {
    let (seg_start, seg) = current_command_segment(line_before_cursor);
    let (mut tokens, trailing_ws) = tokenize_segment(seg_start, seg, alloc);

    let (active_text, replace_start) = if trailing_ws {
        ("", line_before_cursor.len())
    } else {
        let last = tokens.pop()?;
        (last.raw, last.start)
    };

    if active_text.starts_with('-') || is_local_path_or_url(active_text) {
        return None;
    }

    let completed = tokens.as_slice();
    let cmd_idx = skip_env_and_wrappers(completed);
    let cmd_tok = completed.get(cmd_idx)?;
    let cmd_name = cmd_tok.raw.rsplit('/').next().unwrap_or(cmd_tok.raw);
    let rest = &completed[cmd_idx + 1..];

    let provider = match cmd_name {
        "git" => detect_git(rest)?,
        "gh" => detect_gh(rest)?,
        "cargo" => detect_cargo(rest)?,
        "npm" | "bun" | "pnpm" => detect_js_pkg(rest, active_text, false)?,
        "yarn" => detect_js_pkg(rest, active_text, true)?,
        "brew" => detect_brew(rest)?,
        _ => return None,
    };

    Some(RemoteContext {
        provider,
        query: String::from_str_in(active_text, alloc),
        replace_start,
        hint: provider.hint(),
    })
}

fn current_command_segment(line: &str) -> (usize, &str) {
    let bytes = line.as_bytes();
    let mut seg_start = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if in_single {
            if b == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'\\' => {
                escaped = true;
                i += 1;
            }
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b';' => {
                seg_start = i + 1;
                i += 1;
            }
            b'|' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'|' {
                    seg_start = i + 2;
                    i += 2;
                } else {
                    seg_start = i + 1;
                    i += 1;
                }
            }
            b'&' if i + 1 < bytes.len() && bytes[i + 1] == b'&' => {
                seg_start = i + 2;
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }

    (seg_start, &line[seg_start..])
}

fn tokenize_segment<'a, 's>(
    seg_start: usize,
    seg: &'s str,
    alloc: Alloc<'a>,
) -> (AVec<'a, Token<'s>>, bool) {
    let bytes = seg.as_bytes();
    let mut tokens = Vec::new_in(alloc);
    let mut i = 0usize;
    let mut ended_with_ws = true;

    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            ended_with_ws = true;
            break;
        }
        ended_with_ws = false;
        let tok_start = i;
        let mut in_single = false;
        let mut in_double = false;
        let mut escaped = false;

        while i < bytes.len() {
            let b = bytes[i];
            if escaped {
                escaped = false;
                i += 1;
                continue;
            }
            if in_single {
                if b == b'\'' {
                    in_single = false;
                }
                i += 1;
                continue;
            }
            if in_double {
                if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    in_double = false;
                }
                i += 1;
                continue;
            }
            match b {
                b'\\' => {
                    escaped = true;
                    i += 1;
                }
                b'\'' => {
                    in_single = true;
                    i += 1;
                }
                b'"' => {
                    in_double = true;
                    i += 1;
                }
                b if b.is_ascii_whitespace() => {
                    ended_with_ws = true;
                    break;
                }
                _ => {
                    i += 1;
                }
            }
        }

        let raw = &seg[tok_start..i];
        tokens.push(Token {
            raw,
            start: seg_start + tok_start,
        });
    }

    (tokens, ended_with_ws)
}

fn is_env_assignment(tok: &str) -> bool {
    let Some(eq) = tok.find('=') else {
        return false;
    };
    if eq == 0 {
        return false;
    }
    let name = tok[..eq].as_bytes();
    let first_ok = name[0].is_ascii_alphabetic() || name[0] == b'_';
    first_ok && name[1..].iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn skip_env_and_wrappers(tokens: &[Token<'_>]) -> usize {
    let mut idx = 0usize;
    while idx < tokens.len() {
        let raw = tokens[idx].raw;
        if is_env_assignment(raw) {
            idx += 1;
            continue;
        }
        match raw {
            "sudo" => {
                idx += 1;
                while idx < tokens.len() && tokens[idx].raw.starts_with('-') {
                    let flag = tokens[idx].raw;
                    idx += 1;
                    if matches!(flag, "-u" | "-g" | "-C" | "-h" | "-p" | "-r" | "-t" | "-T" | "-D")
                        && idx < tokens.len()
                    {
                        idx += 1;
                    }
                }
            }
            "env" => {
                idx += 1;
                while idx < tokens.len() && tokens[idx].raw.starts_with('-') {
                    let flag = tokens[idx].raw;
                    idx += 1;
                    if matches!(flag, "-u" | "-C" | "-S" | "--unset" | "--chdir")
                        && idx < tokens.len()
                    {
                        idx += 1;
                    }
                }
            }
            "time" => {
                idx += 1;
                while idx < tokens.len() && tokens[idx].raw.starts_with('-') {
                    idx += 1;
                }
            }
            _ => break,
        }
    }
    idx
}

fn is_local_path_or_url(tok: &str) -> bool {
    tok.starts_with("./")
        || tok.starts_with("../")
        || tok.starts_with('/')
        || tok.starts_with("~/")
        || tok.starts_with("file://")
        || tok.starts_with("http://")
        || tok.starts_with("https://")
        || tok.starts_with("ssh://")
        || tok.starts_with("git://")
        || tok.starts_with("git@")
}

/// Walk completed arguments after a subcommand, returning the number of positional arguments seen
/// so far, or `None` if the active token is being consumed as the value of a preceding flag.
fn count_positionals(args: &[Token<'_>], takes_value: fn(&str) -> bool) -> Option<usize> {
    let mut pos = 0usize;
    let mut after_double_dash = false;
    let mut expecting_value = false;

    for tok in args {
        if expecting_value {
            expecting_value = false;
            continue;
        }
        if after_double_dash {
            pos += 1;
            continue;
        }
        if tok.raw == "--" {
            after_double_dash = true;
            continue;
        }
        if tok.raw.starts_with('-') && tok.raw.len() > 1 {
            if !tok.raw.contains('=') && takes_value(tok.raw) {
                expecting_value = true;
            }
            continue;
        }
        pos += 1;
    }

    if expecting_value {
        return None;
    }
    Some(pos)
}

fn is_git_global_value_flag(flag: &str) -> bool {
    matches!(
        flag,
        "-C" | "-c"
            | "--git-dir"
            | "--work-tree"
            | "--namespace"
            | "--super-prefix"
            | "--config-env"
    )
}

fn is_git_clone_value_flag(flag: &str) -> bool {
    matches!(
        flag,
        "-b" | "--branch"
            | "--depth"
            | "-o"
            | "--origin"
            | "-c"
            | "--config"
            | "--reference"
            | "--reference-if-able"
            | "--filter"
            | "-j"
            | "--jobs"
            | "--separate-git-dir"
            | "-u"
            | "--upload-pack"
            | "--template"
            | "--shallow-since"
            | "--shallow-exclude"
            | "--server-option"
            | "--bundle-uri"
            | "--revision"
    )
}

fn detect_git(rest: &[Token<'_>]) -> Option<RemoteProvider> {
    let mut i = 0usize;
    while i < rest.len() && rest[i].raw.starts_with('-') {
        let flag = rest[i].raw;
        i += 1;
        if !flag.contains('=') && is_git_global_value_flag(flag) {
            if i >= rest.len() {
                return None;
            }
            i += 1;
        }
    }
    let sub = rest.get(i)?.raw;
    if sub != "clone" {
        return None;
    }
    let pos = count_positionals(&rest[i + 1..], is_git_clone_value_flag)?;
    if pos != 0 {
        return None;
    }
    Some(RemoteProvider::GitHubRepo {
        insert_git_url: true,
    })
}

fn is_gh_repo_value_flag(flag: &str) -> bool {
    matches!(
        flag,
        "-u" | "--upstream-remote-name"
            | "--org"
            | "--fork-name"
            | "--remote-name"
            | "-b"
            | "--branch"
            | "-q"
            | "--jq"
            | "-t"
            | "--template"
            | "--json"
    )
}

fn detect_gh(rest: &[Token<'_>]) -> Option<RemoteProvider> {
    if rest.len() < 2 || rest[0].raw != "repo" {
        return None;
    }
    if !matches!(rest[1].raw, "clone" | "view" | "fork") {
        return None;
    }
    let pos = count_positionals(&rest[2..], is_gh_repo_value_flag)?;
    if pos != 0 {
        return None;
    }
    Some(RemoteProvider::GitHubRepo {
        insert_git_url: false,
    })
}

fn is_cargo_value_flag(flag: &str) -> bool {
    matches!(
        flag,
        "--features"
            | "-F"
            | "--git"
            | "--path"
            | "--rev"
            | "--branch"
            | "--tag"
            | "--version"
            | "--target"
            | "--bin"
            | "--example"
            | "--root"
            | "--registry"
            | "--index"
            | "--manifest-path"
            | "-p"
            | "--package"
            | "--profile"
            | "--target-dir"
            | "--rename"
            | "--config"
            | "-j"
            | "--jobs"
            | "-Z"
            | "--color"
    )
}

fn detect_cargo(rest: &[Token<'_>]) -> Option<RemoteProvider> {
    let mut i = 0usize;
    if let Some(first) = rest.first()
        && first.raw.starts_with('+')
    {
        i = 1;
    }
    let sub = rest.get(i)?.raw;
    if !matches!(sub, "add" | "install") {
        return None;
    }
    let _pos = count_positionals(&rest[i + 1..], is_cargo_value_flag)?;
    Some(RemoteProvider::CratesIo)
}

fn is_js_pkg_value_flag(flag: &str) -> bool {
    matches!(
        flag,
        "--registry"
            | "--prefix"
            | "--cwd"
            | "--filter"
            | "-F"
            | "--workspace"
            | "-w"
            | "--tag"
            | "--config"
            | "--cache"
    )
}

fn detect_js_pkg(
    rest: &[Token<'_>],
    active_text: &str,
    yarn_only_add: bool,
) -> Option<RemoteProvider> {
    if active_text.starts_with("file:") || active_text.starts_with("git+") {
        return None;
    }
    let sub = rest.first()?.raw;
    let valid_sub = if yarn_only_add {
        sub == "add"
    } else {
        matches!(sub, "install" | "i" | "add")
    };
    if !valid_sub {
        return None;
    }
    let _pos = count_positionals(&rest[1..], is_js_pkg_value_flag)?;
    Some(RemoteProvider::NpmRegistry)
}

fn is_brew_value_flag(flag: &str) -> bool {
    matches!(flag, "--cc" | "--env" | "--os" | "--arch")
}

fn detect_brew(rest: &[Token<'_>]) -> Option<RemoteProvider> {
    let sub = rest.first()?.raw;
    if !matches!(sub, "install" | "info") {
        return None;
    }
    let _pos = count_positionals(&rest[1..], is_brew_value_flag)?;
    Some(RemoteProvider::Homebrew)
}

/// Build the remote API URL, optional `gh api` path, and cache key for `(provider, query)`.
///
/// Returns `None` if `query` (after trimming) is empty or shorter than 2 bytes (except GitHub
/// `owner/` queries where `owner` is non-empty).
pub fn build_endpoint<'a>(
    provider: RemoteProvider,
    query: &str,
    alloc: Alloc<'a>,
) -> Option<RemoteEndpoint<'a>> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }

    match provider {
        RemoteProvider::GitHubRepo { insert_git_url } => {
            let mut gh_path = String::new_in(alloc);
            gh_path.push_str("search/repositories?q=");
            if let Some((owner, rest)) = q.split_once('/') {
                let owner = owner.trim();
                let rest = rest.trim();
                if owner.is_empty() {
                    return None;
                }
                if rest.is_empty() {
                    gh_path.push_str("user:");
                    push_percent_encoded(&mut gh_path, owner);
                } else {
                    push_percent_encoded(&mut gh_path, rest);
                    gh_path.push_str("+user:");
                    push_percent_encoded(&mut gh_path, owner);
                }
            } else {
                if q.len() < 2 {
                    return None;
                }
                push_percent_encoded(&mut gh_path, q);
            }
            gh_path.push_str("&sort=stars&per_page=10");

            let mut url = String::new_in(alloc);
            url.push_str("https://api.github.com/");
            url.push_str(gh_path.as_str());

            let mut cache_key = String::new_in(alloc);
            cache_key.push_str(if insert_git_url {
                "github-git:"
            } else {
                "github:"
            });
            cache_key.push_str(q);

            Some(RemoteEndpoint {
                url,
                gh_api_path: Some(gh_path),
                cache_key,
            })
        }
        RemoteProvider::CratesIo => {
            if q.len() < 2 {
                return None;
            }
            let mut url = String::new_in(alloc);
            url.push_str("https://crates.io/api/v1/crates?q=");
            push_percent_encoded(&mut url, q);
            url.push_str("&per_page=10");

            let mut cache_key = String::new_in(alloc);
            cache_key.push_str("crates:");
            cache_key.push_str(q);

            Some(RemoteEndpoint {
                url,
                gh_api_path: None,
                cache_key,
            })
        }
        RemoteProvider::NpmRegistry => {
            if q.len() < 2 {
                return None;
            }
            let mut url = String::new_in(alloc);
            url.push_str("https://registry.npmjs.org/-/v1/search?text=");
            push_percent_encoded(&mut url, q);
            url.push_str("&size=10");

            let mut cache_key = String::new_in(alloc);
            cache_key.push_str("npm:");
            cache_key.push_str(q);

            Some(RemoteEndpoint {
                url,
                gh_api_path: None,
                cache_key,
            })
        }
        RemoteProvider::Homebrew => {
            if q.len() < 2 {
                return None;
            }
            let mut url = String::new_in(alloc);
            url.push_str("https://formulae.brew.sh/api/formula/");
            push_percent_encoded(&mut url, q);
            url.push_str(".json");

            let mut cache_key = String::new_in(alloc);
            cache_key.push_str("brew:");
            cache_key.push_str(q);

            Some(RemoteEndpoint {
                url,
                gh_api_path: None,
                cache_key,
            })
        }
    }
}

fn push_percent_encoded(out: &mut AString<'_>, s: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0F) as usize] as char);
        }
    }
}

/// Format a count compactly for detail badges (`950` -> `"950"`, `12_400` -> `"12.4k"`,
/// `340_000_000` -> `"340M"`).
pub fn format_compact_count_in<'a>(n: u64, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    if n < 1_000 {
        let _ = write!(out, "{n}");
        return out;
    }
    let (unit, suffix) = if n < 1_000_000 {
        (1_000u64, 'k')
    } else if n < 1_000_000_000 {
        (1_000_000u64, 'M')
    } else {
        (1_000_000_000u64, 'B')
    };
    let whole = n / unit;
    let tenth = (n % unit) / (unit / 10);
    if whole >= 100 || tenth == 0 {
        let _ = write!(out, "{whole}{suffix}");
    } else {
        let _ = write!(out, "{whole}.{tenth}{suffix}");
    }
    out
}

/// Parse a raw JSON HTTP response body for `provider` into completion candidates allocated in `alloc`.
pub fn parse_response<'a>(
    provider: RemoteProvider,
    body: &[u8],
    alloc: Alloc<'a>,
) -> AVec<'a, RemoteCandidate<'a>> {
    let mut out = Vec::new_in(alloc);
    let mut scanner = JsonScanner::new(body);
    match provider {
        RemoteProvider::GitHubRepo { insert_git_url } => {
            parse_github_response(&mut scanner, insert_git_url, alloc, &mut out);
        }
        RemoteProvider::CratesIo => {
            parse_crates_response(&mut scanner, alloc, &mut out);
        }
        RemoteProvider::NpmRegistry => {
            parse_npm_response(&mut scanner, alloc, &mut out);
        }
        RemoteProvider::Homebrew => {
            parse_homebrew_response(&mut scanner, alloc, &mut out);
        }
    }
    out
}

fn parse_github_response<'a>(
    scanner: &mut JsonScanner<'_>,
    insert_git_url: bool,
    alloc: Alloc<'a>,
    out: &mut AVec<'a, RemoteCandidate<'a>>,
) {
    let _ = scanner.visit_object(alloc, |key, sc| {
        if key == "items" {
            if sc.peek_non_ws() == Some(b'[') {
                return sc.visit_array(|sc| {
                    if sc.peek_non_ws() != Some(b'{') {
                        return sc.skip_value();
                    }
                    let mut full_name: Option<AString<'a>> = None;
                    let mut clone_url: Option<AString<'a>> = None;
                    let mut description: Option<AString<'a>> = None;
                    let mut stargazers_count: u64 = 0;
                    let mut language: Option<AString<'a>> = None;
                    let mut owner_login: Option<AString<'a>> = None;
                    let mut avatar_url: Option<AString<'a>> = None;

                    let ok = sc.visit_object(alloc, |k, sc| match k {
                        "full_name" => match sc.parse_opt_string_in(alloc) {
                            Some(v) => {
                                full_name = v;
                                true
                            }
                            None => false,
                        },
                        "clone_url" => match sc.parse_opt_string_in(alloc) {
                            Some(v) => {
                                clone_url = v;
                                true
                            }
                            None => false,
                        },
                        "description" => match sc.parse_opt_string_in(alloc) {
                            Some(v) => {
                                description = v;
                                true
                            }
                            None => false,
                        },
                        "stargazers_count" => match sc.parse_opt_u64() {
                            Some(v) => {
                                stargazers_count = v.unwrap_or(0);
                                true
                            }
                            None => false,
                        },
                        "language" => match sc.parse_opt_string_in(alloc) {
                            Some(v) => {
                                language = v;
                                true
                            }
                            None => false,
                        },
                        "owner" => {
                            if sc.peek_non_ws() == Some(b'{') {
                                sc.visit_object(alloc, |ok_key, sc| match ok_key {
                                    "login" => match sc.parse_opt_string_in(alloc) {
                                        Some(v) => {
                                            owner_login = v;
                                            true
                                        }
                                        None => false,
                                    },
                                    "avatar_url" => match sc.parse_opt_string_in(alloc) {
                                        Some(v) => {
                                            avatar_url = v;
                                            true
                                        }
                                        None => false,
                                    },
                                    _ => sc.skip_value(),
                                })
                            } else {
                                sc.skip_value()
                            }
                        }
                        _ => sc.skip_value(),
                    });

                    if ok
                        && let Some(full) = full_name
                        && !full.as_str().is_empty()
                    {
                        let insert = if insert_git_url {
                            match clone_url {
                                Some(c) if !c.as_str().is_empty() => c,
                                _ => {
                                    let mut s = String::new_in(alloc);
                                    s.push_str("https://github.com/");
                                    s.push_str(full.as_str());
                                    s.push_str(".git");
                                    s
                                }
                            }
                        } else {
                            String::from_str_in(full.as_str(), alloc)
                        };

                        let stars = format_compact_count_in(stargazers_count, alloc);
                        let mut detail = String::new_in(alloc);
                        detail.push_str("★ ");
                        detail.push_str(stars.as_str());
                        if let Some(lang) = language.as_ref()
                            && !lang.as_str().is_empty()
                        {
                            detail.push_str(" · ");
                            detail.push_str(lang.as_str());
                        }

                        let avatar = avatar_url.and_then(|mut url| {
                            if url.as_str().is_empty() {
                                return None;
                            }
                            if url.as_str().contains('?') {
                                url.push_str("&s=64");
                            } else {
                                url.push_str("?s=64");
                            }
                            Some(url)
                        });

                        let owner = owner_login.filter(|s| !s.as_str().is_empty());

                        out.push(RemoteCandidate {
                            label: full,
                            insert,
                            detail,
                            doc: description.unwrap_or_else(|| String::new_in(alloc)),
                            owner_login: owner,
                            avatar_url: avatar,
                            popularity: stargazers_count,
                        });
                    }
                    ok
                });
            }
            sc.skip_value()
        } else {
            sc.skip_value()
        }
    });
}

fn parse_crates_response<'a>(
    scanner: &mut JsonScanner<'_>,
    alloc: Alloc<'a>,
    out: &mut AVec<'a, RemoteCandidate<'a>>,
) {
    let _ = scanner.visit_object(alloc, |key, sc| {
        if key == "crates" {
            if sc.peek_non_ws() == Some(b'[') {
                return sc.visit_array(|sc| {
                    if sc.peek_non_ws() != Some(b'{') {
                        return sc.skip_value();
                    }
                    let mut name: Option<AString<'a>> = None;
                    let mut max_stable: Option<AString<'a>> = None;
                    let mut max_ver: Option<AString<'a>> = None;
                    let mut newest_ver: Option<AString<'a>> = None;
                    let mut downloads: u64 = 0;
                    let mut description: Option<AString<'a>> = None;

                    let ok = sc.visit_object(alloc, |k, sc| match k {
                        "name" => match sc.parse_opt_string_in(alloc) {
                            Some(v) => {
                                name = v;
                                true
                            }
                            None => false,
                        },
                        "max_stable_version" => match sc.parse_opt_string_in(alloc) {
                            Some(v) => {
                                max_stable = v;
                                true
                            }
                            None => false,
                        },
                        "max_version" => match sc.parse_opt_string_in(alloc) {
                            Some(v) => {
                                max_ver = v;
                                true
                            }
                            None => false,
                        },
                        "newest_version" => match sc.parse_opt_string_in(alloc) {
                            Some(v) => {
                                newest_ver = v;
                                true
                            }
                            None => false,
                        },
                        "downloads" => match sc.parse_opt_u64() {
                            Some(v) => {
                                downloads = v.unwrap_or(0);
                                true
                            }
                            None => false,
                        },
                        "description" => match sc.parse_opt_string_in(alloc) {
                            Some(v) => {
                                description = v;
                                true
                            }
                            None => false,
                        },
                        _ => sc.skip_value(),
                    });

                    if ok
                        && let Some(crate_name) = name
                        && !crate_name.as_str().is_empty()
                    {
                        let version = max_stable
                            .filter(|s| !s.as_str().is_empty())
                            .or_else(|| max_ver.filter(|s| !s.as_str().is_empty()))
                            .or_else(|| newest_ver.filter(|s| !s.as_str().is_empty()));

                        let dl = format_compact_count_in(downloads, alloc);
                        let mut detail = String::new_in(alloc);
                        if let Some(ver) = version.as_ref() {
                            detail.push('v');
                            detail.push_str(ver.as_str());
                            detail.push_str(" · ");
                        }
                        detail.push_str(dl.as_str());

                        let insert = String::from_str_in(crate_name.as_str(), alloc);
                        out.push(RemoteCandidate {
                            label: crate_name,
                            insert,
                            detail,
                            doc: description.unwrap_or_else(|| String::new_in(alloc)),
                            owner_login: None,
                            avatar_url: None,
                            popularity: downloads,
                        });
                    }
                    ok
                });
            }
            sc.skip_value()
        } else {
            sc.skip_value()
        }
    });
}

fn parse_npm_response<'a>(
    scanner: &mut JsonScanner<'_>,
    alloc: Alloc<'a>,
    out: &mut AVec<'a, RemoteCandidate<'a>>,
) {
    let _ = scanner.visit_object(alloc, |key, sc| {
        if key == "objects" {
            if sc.peek_non_ws() == Some(b'[') {
                return sc.visit_array(|sc| {
                    if sc.peek_non_ws() != Some(b'{') {
                        return sc.skip_value();
                    }
                    let mut name: Option<AString<'a>> = None;
                    let mut version: Option<AString<'a>> = None;
                    let mut description: Option<AString<'a>> = None;
                    let mut downloads: u64 = 0;

                    let ok = sc.visit_object(alloc, |k, sc| match k {
                        "package" => {
                            if sc.peek_non_ws() == Some(b'{') {
                                sc.visit_object(alloc, |pk, sc| match pk {
                                    "name" => match sc.parse_opt_string_in(alloc) {
                                        Some(v) => {
                                            name = v;
                                            true
                                        }
                                        None => false,
                                    },
                                    "version" => match sc.parse_opt_string_in(alloc) {
                                        Some(v) => {
                                            version = v;
                                            true
                                        }
                                        None => false,
                                    },
                                    "description" => match sc.parse_opt_string_in(alloc) {
                                        Some(v) => {
                                            description = v;
                                            true
                                        }
                                        None => false,
                                    },
                                    _ => sc.skip_value(),
                                })
                            } else {
                                sc.skip_value()
                            }
                        }
                        "downloads" => {
                            if sc.peek_non_ws() == Some(b'{') {
                                sc.visit_object(alloc, |dk, sc| match dk {
                                    "monthly" => match sc.parse_opt_u64() {
                                        Some(v) => {
                                            downloads = v.unwrap_or(0);
                                            true
                                        }
                                        None => false,
                                    },
                                    _ => sc.skip_value(),
                                })
                            } else {
                                sc.skip_value()
                            }
                        }
                        _ => sc.skip_value(),
                    });

                    if ok
                        && let Some(pkg_name) = name
                        && !pkg_name.as_str().is_empty()
                    {
                        let mut detail = String::new_in(alloc);
                        if let Some(ver) = version.as_ref()
                            && !ver.as_str().is_empty()
                        {
                            detail.push('v');
                            detail.push_str(ver.as_str());
                        }
                        let insert = String::from_str_in(pkg_name.as_str(), alloc);
                        out.push(RemoteCandidate {
                            label: pkg_name,
                            insert,
                            detail,
                            doc: description.unwrap_or_else(|| String::new_in(alloc)),
                            owner_login: None,
                            avatar_url: None,
                            popularity: downloads,
                        });
                    }
                    ok
                });
            }
            sc.skip_value()
        } else {
            sc.skip_value()
        }
    });
}

fn parse_homebrew_response<'a>(
    scanner: &mut JsonScanner<'_>,
    alloc: Alloc<'a>,
    out: &mut AVec<'a, RemoteCandidate<'a>>,
) {
    let mut name: Option<AString<'a>> = None;
    let mut stable: Option<AString<'a>> = None;
    let mut desc: Option<AString<'a>> = None;

    let ok = scanner.visit_object(alloc, |key, sc| match key {
        "name" => match sc.parse_opt_string_in(alloc) {
            Some(v) => {
                name = v;
                true
            }
            None => false,
        },
        "desc" => match sc.parse_opt_string_in(alloc) {
            Some(v) => {
                desc = v;
                true
            }
            None => false,
        },
        "versions" => {
            if sc.peek_non_ws() == Some(b'{') {
                sc.visit_object(alloc, |vk, sc| match vk {
                    "stable" => match sc.parse_opt_string_in(alloc) {
                        Some(v) => {
                            stable = v;
                            true
                        }
                        None => false,
                    },
                    _ => sc.skip_value(),
                })
            } else {
                sc.skip_value()
            }
        }
        _ => sc.skip_value(),
    });

    if ok
        && let Some(formula_name) = name
        && !formula_name.as_str().is_empty()
    {
        let mut detail = String::new_in(alloc);
        if let Some(ver) = stable.as_ref()
            && !ver.as_str().is_empty()
        {
            detail.push('v');
            detail.push_str(ver.as_str());
            detail.push_str(" · brew");
        } else {
            detail.push_str("brew");
        }
        let insert = String::from_str_in(formula_name.as_str(), alloc);
        out.push(RemoteCandidate {
            label: formula_name,
            insert,
            detail,
            doc: desc.unwrap_or_else(|| String::new_in(alloc)),
            owner_login: None,
            avatar_url: None,
            popularity: 0,
        });
    }
}

/// Minimal, zero-unsafe, `#![no_std]` streaming recursive-descent JSON scanner.
///
/// Unescaped object keys are borrowed directly from the input slice with zero allocation;
/// skipped values allocate nothing and enforce a nesting depth ceiling (`MAX_JSON_DEPTH`).
struct JsonScanner<'b> {
    input: &'b [u8],
    pos: usize,
    depth: usize,
}

enum JsonKey<'a, 'b> {
    Borrowed(&'b str),
    Owned(AString<'a>),
}

impl<'a, 'b> JsonKey<'a, 'b> {
    fn as_str(&self) -> &str {
        match self {
            Self::Borrowed(s) => s,
            Self::Owned(s) => s.as_str(),
        }
    }
}

impl<'b> JsonScanner<'b> {
    fn new(input: &'b [u8]) -> Self {
        let pos = if input.starts_with(&[0xEF, 0xBB, 0xBF]) {
            3
        } else {
            0
        };
        Self {
            input,
            pos,
            depth: 0,
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.input.len() {
            match self.input[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn peek_non_ws(&mut self) -> Option<u8> {
        self.skip_ws();
        self.input.get(self.pos).copied()
    }

    fn expect_byte(&mut self, expected: u8) -> bool {
        self.skip_ws();
        if self.input.get(self.pos) == Some(&expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn consume_literal(&mut self, lit: &[u8]) -> bool {
        self.skip_ws();
        if self.input[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            true
        } else {
            false
        }
    }

    fn visit_object<'a>(
        &mut self,
        alloc: Alloc<'a>,
        mut on_entry: impl FnMut(&str, &mut Self) -> bool,
    ) -> bool {
        if self.depth >= MAX_JSON_DEPTH || !self.expect_byte(b'{') {
            return false;
        }
        self.depth += 1;

        if self.peek_non_ws() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return true;
        }

        loop {
            let Some(key) = self.parse_key(alloc) else {
                self.depth -= 1;
                return false;
            };
            if !self.expect_byte(b':') {
                self.depth -= 1;
                return false;
            }
            if !on_entry(key.as_str(), self) {
                self.depth -= 1;
                return false;
            }
            match self.peek_non_ws() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return true;
                }
                _ => {
                    self.depth -= 1;
                    return false;
                }
            }
        }
    }

    fn visit_array(&mut self, mut on_elem: impl FnMut(&mut Self) -> bool) -> bool {
        if self.depth >= MAX_JSON_DEPTH || !self.expect_byte(b'[') {
            return false;
        }
        self.depth += 1;

        if self.peek_non_ws() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return true;
        }

        loop {
            if !on_elem(self) {
                self.depth -= 1;
                return false;
            }
            match self.peek_non_ws() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return true;
                }
                _ => {
                    self.depth -= 1;
                    return false;
                }
            }
        }
    }

    /// Read a JSON object key, borrowing directly when no escape sequences appear.
    fn parse_key<'a>(&mut self, alloc: Alloc<'a>) -> Option<JsonKey<'a, 'b>> {
        self.skip_ws();
        if self.input.get(self.pos) != Some(&b'"') {
            return None;
        }
        let start = self.pos + 1;
        let mut i = start;
        while i < self.input.len() {
            let b = self.input[i];
            if b == b'"' {
                let s = core::str::from_utf8(&self.input[start..i]).ok()?;
                self.pos = i + 1;
                return Some(JsonKey::Borrowed(s));
            }
            if b == b'\\' {
                let owned = self.parse_string_in(alloc)?;
                return Some(JsonKey::Owned(owned));
            }
            if b < 0x20 {
                return None;
            }
            i += 1;
        }
        None
    }

    /// Parse a JSON string, `null` -> `Some(None)`, or skip any other JSON value -> `Some(None)`.
    /// Returns `None` only on malformed JSON.
    fn parse_opt_string_in<'a>(&mut self, alloc: Alloc<'a>) -> Option<Option<AString<'a>>> {
        match self.peek_non_ws()? {
            b'"' => self.parse_string_in(alloc).map(Some),
            b'n' => {
                if self.consume_literal(b"null") {
                    Some(None)
                } else {
                    None
                }
            }
            _ => {
                if self.skip_value() {
                    Some(None)
                } else {
                    None
                }
            }
        }
    }

    fn parse_string_in<'a>(&mut self, alloc: Alloc<'a>) -> Option<AString<'a>> {
        if !self.expect_byte(b'"') {
            return None;
        }
        let mut out = String::new_in(alloc);
        let mut chunk_start = self.pos;

        while self.pos < self.input.len() {
            let b = self.input[self.pos];
            if b == b'"' {
                if self.pos > chunk_start {
                    let s = core::str::from_utf8(&self.input[chunk_start..self.pos]).ok()?;
                    out.push_str(s);
                }
                self.pos += 1;
                return Some(out);
            }
            if b == b'\\' {
                if self.pos > chunk_start {
                    let s = core::str::from_utf8(&self.input[chunk_start..self.pos]).ok()?;
                    out.push_str(s);
                }
                self.pos += 1;
                let esc = *self.input.get(self.pos)?;
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000C}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let code = self.parse_hex4()?;
                        if (0xD800..=0xDBFF).contains(&code) {
                            // High surrogate: consume paired low surrogate if present.
                            if self.input[self.pos..].starts_with(b"\\u") {
                                let saved = self.pos;
                                self.pos += 2;
                                if let Some(low) = self.parse_hex4()
                                    && (0xDC00..=0xDFFF).contains(&low)
                                {
                                    let combined = 0x10000
                                        + (((code as u32) - 0xD800) << 10)
                                        + ((low as u32) - 0xDC00);
                                    out.push(char::from_u32(combined).unwrap_or('\u{FFFD}'));
                                } else {
                                    self.pos = saved;
                                    out.push('\u{FFFD}');
                                }
                            } else {
                                out.push('\u{FFFD}');
                            }
                        } else if (0xDC00..=0xDFFF).contains(&code) {
                            out.push('\u{FFFD}');
                        } else {
                            out.push(char::from_u32(code as u32).unwrap_or('\u{FFFD}'));
                        }
                    }
                    _ => return None,
                }
                chunk_start = self.pos;
                continue;
            }
            if b < 0x20 {
                return None;
            }
            self.pos += 1;
        }
        None
    }

    fn parse_hex4(&mut self) -> Option<u16> {
        if self.pos + 4 > self.input.len() {
            return None;
        }
        let mut val = 0u16;
        for &b in &self.input[self.pos..self.pos + 4] {
            let digit = match b {
                b'0'..=b'9' => (b - b'0') as u16,
                b'a'..=b'f' => (b - b'a' + 10) as u16,
                b'A'..=b'F' => (b - b'A' + 10) as u16,
                _ => return None,
            };
            val = (val << 4) | digit;
        }
        self.pos += 4;
        Some(val)
    }

    /// Parse a non-negative integer into `u64`, `null` -> `Some(None)`, or skip other values.
    fn parse_opt_u64(&mut self) -> Option<Option<u64>> {
        match self.peek_non_ws()? {
            b'0'..=b'9' => {
                let mut val = 0u64;
                if self.input[self.pos] == b'0' {
                    self.pos += 1;
                } else {
                    while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                        let d = (self.input[self.pos] - b'0') as u64;
                        val = val.saturating_mul(10).saturating_add(d);
                        self.pos += 1;
                    }
                }
                if !self.skip_number_fraction_and_exp() {
                    return None;
                }
                Some(Some(val))
            }
            b'n' => {
                if self.consume_literal(b"null") {
                    Some(None)
                } else {
                    None
                }
            }
            _ => {
                if self.skip_value() {
                    Some(None)
                } else {
                    None
                }
            }
        }
    }

    fn skip_number_fraction_and_exp(&mut self) -> bool {
        if self.pos < self.input.len() && self.input[self.pos] == b'.' {
            self.pos += 1;
            if self.pos >= self.input.len() || !self.input[self.pos].is_ascii_digit() {
                return false;
            }
            while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }
        if self.pos < self.input.len() && matches!(self.input[self.pos], b'e' | b'E') {
            self.pos += 1;
            if self.pos < self.input.len() && matches!(self.input[self.pos], b'+' | b'-') {
                self.pos += 1;
            }
            if self.pos >= self.input.len() || !self.input[self.pos].is_ascii_digit() {
                return false;
            }
            while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }
        true
    }

    fn skip_string(&mut self) -> bool {
        if !self.expect_byte(b'"') {
            return false;
        }
        while self.pos < self.input.len() {
            let b = self.input[self.pos];
            if b == b'"' {
                self.pos += 1;
                return true;
            }
            if b == b'\\' {
                self.pos += 1;
                if self.pos >= self.input.len() {
                    return false;
                }
                let esc = self.input[self.pos];
                self.pos += 1;
                if esc == b'u' && self.parse_hex4().is_none() {
                    return false;
                }
                continue;
            }
            if b < 0x20 {
                return false;
            }
            self.pos += 1;
        }
        false
    }

    fn skip_number(&mut self) -> bool {
        self.skip_ws();
        if self.pos < self.input.len() && self.input[self.pos] == b'-' {
            self.pos += 1;
        }
        if self.pos >= self.input.len() {
            return false;
        }
        if self.input[self.pos] == b'0' {
            self.pos += 1;
        } else if self.input[self.pos].is_ascii_digit() {
            while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        } else {
            return false;
        }
        self.skip_number_fraction_and_exp()
    }

    fn skip_value(&mut self) -> bool {
        let Some(b) = self.peek_non_ws() else {
            return false;
        };
        match b {
            b'"' => self.skip_string(),
            b'{' => {
                if self.depth >= MAX_JSON_DEPTH {
                    return false;
                }
                self.pos += 1;
                self.depth += 1;
                if self.peek_non_ws() == Some(b'}') {
                    self.pos += 1;
                    self.depth -= 1;
                    return true;
                }
                loop {
                    if !self.skip_string() || !self.expect_byte(b':') || !self.skip_value() {
                        self.depth -= 1;
                        return false;
                    }
                    match self.peek_non_ws() {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            self.depth -= 1;
                            return true;
                        }
                        _ => {
                            self.depth -= 1;
                            return false;
                        }
                    }
                }
            }
            b'[' => {
                if self.depth >= MAX_JSON_DEPTH {
                    return false;
                }
                self.pos += 1;
                self.depth += 1;
                if self.peek_non_ws() == Some(b']') {
                    self.pos += 1;
                    self.depth -= 1;
                    return true;
                }
                loop {
                    if !self.skip_value() {
                        self.depth -= 1;
                        return false;
                    }
                    match self.peek_non_ws() {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            self.depth -= 1;
                            return true;
                        }
                        _ => {
                            self.depth -= 1;
                            return false;
                        }
                    }
                }
            }
            b't' => self.consume_literal(b"true"),
            b'f' => self.consume_literal(b"false"),
            b'n' => self.consume_literal(b"null"),
            b'-' | b'0'..=b'9' => self.skip_number(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::{Erased, Inline};

    use super::*;

    fn with_test_alloc(f: impl FnOnce(Alloc<'_>)) {
        let buf = Inline::<65536>::new();
        let arena = Erased::new(&buf);
        f(arena.handle());
    }

    #[test]
    fn detect_git_clone_contexts() {
        with_test_alloc(|alloc| {
            let ctx = detect_remote_context("git clone ", alloc).expect("empty query");
            assert_eq!(
                ctx.provider,
                RemoteProvider::GitHubRepo {
                    insert_git_url: true
                }
            );
            assert_eq!(ctx.query.as_str(), "");
            assert_eq!(ctx.replace_start, 10);
            assert!(!ctx.hint.is_empty());

            let ctx = detect_remote_context("git clone rip", alloc).expect("query rip");
            assert_eq!(
                ctx.provider,
                RemoteProvider::GitHubRepo {
                    insert_git_url: true
                }
            );
            assert_eq!(ctx.query.as_str(), "rip");
            assert_eq!(ctx.replace_start, 10);

            let ctx = detect_remote_context("git clone --depth 1 rip", alloc).expect("value flag");
            assert_eq!(ctx.query.as_str(), "rip");
            assert_eq!(ctx.replace_start, 20);

            let ctx = detect_remote_context(
                "git clone --recursive -b main --single-branch tokio-rs/",
                alloc,
            )
            .expect("mixed flags");
            assert_eq!(ctx.query.as_str(), "tokio-rs/");
            assert_eq!(ctx.replace_start, 46);

            let ctx =
                detect_remote_context("git clone --depth=1 rip", alloc).expect("inline flag value");
            assert_eq!(ctx.query.as_str(), "rip");
            assert_eq!(ctx.replace_start, 20);

            // Typing a value for a value-taking flag must NOT trigger repo search.
            assert!(detect_remote_context("git clone --depth ", alloc).is_none());
            assert!(detect_remote_context("git clone --depth 1", alloc).is_none());
            assert!(detect_remote_context("git clone -b ma", alloc).is_none());

            // Second positional arg (destination directory) must NOT trigger.
            assert!(
                detect_remote_context("git clone https://github.com/a/b.git dest_dir", alloc)
                    .is_none()
            );
            assert!(detect_remote_context("git clone rip dest_dir", alloc).is_none());

            // Local paths, explicit URLs, and flags must NOT trigger.
            assert!(detect_remote_context("git clone ./local", alloc).is_none());
            assert!(detect_remote_context("git clone ../local", alloc).is_none());
            assert!(detect_remote_context("git clone /tmp/repo", alloc).is_none());
            assert!(detect_remote_context("git clone ~/repo", alloc).is_none());
            assert!(detect_remote_context("git clone file:///tmp/repo", alloc).is_none());
            assert!(detect_remote_context("git clone http://example.com/a", alloc).is_none());
            assert!(detect_remote_context("git clone https://github.com/a/b", alloc).is_none());
            assert!(detect_remote_context("git clone git@github.com:a/b.git", alloc).is_none());
            assert!(detect_remote_context("git clone --dep", alloc).is_none());
            assert!(detect_remote_context("git clone", alloc).is_none());
        });
    }

    #[test]
    fn detect_segments_env_wrappers_and_other_providers() {
        with_test_alloc(|alloc| {
            let line = "echo 'a | b' && FOO=bar sudo env BAR=1 time git clone rip";
            let ctx = detect_remote_context(line, alloc).expect("chained wrapped git clone");
            assert_eq!(
                ctx.provider,
                RemoteProvider::GitHubRepo {
                    insert_git_url: true
                }
            );
            assert_eq!(ctx.query.as_str(), "rip");
            assert_eq!(&line[ctx.replace_start..], "rip");

            let ctx = detect_remote_context("gh repo clone ", alloc).expect("gh repo clone");
            assert_eq!(
                ctx.provider,
                RemoteProvider::GitHubRepo {
                    insert_git_url: false
                }
            );
            assert_eq!(ctx.query.as_str(), "");
            assert_eq!(ctx.replace_start, 14);

            let ctx =
                detect_remote_context("gh repo view BurntSushi/rip", alloc).expect("gh repo view");
            assert_eq!(
                ctx.provider,
                RemoteProvider::GitHubRepo {
                    insert_git_url: false
                }
            );
            assert_eq!(ctx.query.as_str(), "BurntSushi/rip");

            let ctx = detect_remote_context("gh repo fork tok", alloc).expect("gh repo fork");
            assert_eq!(
                ctx.provider,
                RemoteProvider::GitHubRepo {
                    insert_git_url: false
                }
            );
            assert_eq!(ctx.query.as_str(), "tok");
            assert!(detect_remote_context("gh repo clone a/b local_dir", alloc).is_none());

            let ctx = detect_remote_context("cargo add ser", alloc).expect("cargo add");
            assert_eq!(ctx.provider, RemoteProvider::CratesIo);
            assert_eq!(ctx.query.as_str(), "ser");
            assert_eq!(ctx.replace_start, 10);

            let ctx = detect_remote_context("cargo add --features derive serde tok", alloc)
                .expect("cargo add multi");
            assert_eq!(ctx.provider, RemoteProvider::CratesIo);
            assert_eq!(ctx.query.as_str(), "tok");
            assert!(detect_remote_context("cargo add -F ser", alloc).is_none());
            assert!(detect_remote_context("cargo add --path ./foo", alloc).is_none());

            let ctx =
                detect_remote_context("cargo install --locked rip", alloc).expect("cargo install");
            assert_eq!(ctx.provider, RemoteProvider::CratesIo);
            assert_eq!(ctx.query.as_str(), "rip");

            let ctx = detect_remote_context("bun add zod", alloc).expect("bun add");
            assert_eq!(ctx.provider, RemoteProvider::NpmRegistry);
            assert_eq!(ctx.query.as_str(), "zod");
            assert_eq!(ctx.replace_start, 8);

            for cmd in [
                "npm install react",
                "npm i -D typ",
                "npm add vit",
                "bun install @types/no",
                "bun i hono",
                "pnpm add esbuild",
                "pnpm install astro",
                "pnpm i solid",
                "yarn add drizzle",
            ] {
                let ctx = detect_remote_context(cmd, alloc).expect(cmd);
                assert_eq!(ctx.provider, RemoteProvider::NpmRegistry, "{cmd}");
            }
            assert!(detect_remote_context("yarn install zod", alloc).is_none());

            let ctx = detect_remote_context("brew install rip", alloc).expect("brew install");
            assert_eq!(ctx.provider, RemoteProvider::Homebrew);
            assert_eq!(ctx.query.as_str(), "rip");

            let ctx = detect_remote_context("brew info jq", alloc).expect("brew info");
            assert_eq!(ctx.provider, RemoteProvider::Homebrew);
            assert_eq!(ctx.query.as_str(), "jq");
        });
    }

    #[test]
    fn build_endpoints_and_percent_encoding() {
        with_test_alloc(|alloc| {
            assert!(
                build_endpoint(
                    RemoteProvider::GitHubRepo {
                        insert_git_url: true
                    },
                    "",
                    alloc
                )
                .is_none()
            );
            assert!(
                build_endpoint(
                    RemoteProvider::GitHubRepo {
                        insert_git_url: true
                    },
                    "a",
                    alloc
                )
                .is_none()
            );
            assert!(
                build_endpoint(
                    RemoteProvider::GitHubRepo {
                        insert_git_url: true
                    },
                    "/foo",
                    alloc
                )
                .is_none()
            );
            assert!(build_endpoint(RemoteProvider::CratesIo, "s", alloc).is_none());

            let ep = build_endpoint(
                RemoteProvider::GitHubRepo {
                    insert_git_url: true,
                },
                "ripgrep",
                alloc,
            )
            .expect("ripgrep");
            assert_eq!(
                ep.gh_api_path.as_ref().map(AString::as_str),
                Some("search/repositories?q=ripgrep&sort=stars&per_page=10")
            );
            assert_eq!(
                ep.url.as_str(),
                "https://api.github.com/search/repositories?q=ripgrep&sort=stars&per_page=10"
            );

            let ep = build_endpoint(
                RemoteProvider::GitHubRepo {
                    insert_git_url: false,
                },
                "tokio-rs/",
                alloc,
            )
            .expect("tokio-rs/");
            assert_eq!(
                ep.gh_api_path.as_ref().map(AString::as_str),
                Some("search/repositories?q=user:tokio-rs&sort=stars&per_page=10")
            );
            assert_eq!(
                ep.url.as_str(),
                "https://api.github.com/search/repositories?q=user:tokio-rs&sort=stars&per_page=10"
            );

            let ep = build_endpoint(
                RemoteProvider::GitHubRepo {
                    insert_git_url: true,
                },
                "BurntSushi/rip",
                alloc,
            )
            .expect("BurntSushi/rip");
            assert_eq!(
                ep.gh_api_path.as_ref().map(AString::as_str),
                Some("search/repositories?q=rip+user:BurntSushi&sort=stars&per_page=10")
            );

            let ep = build_endpoint(RemoteProvider::CratesIo, "serde json", alloc).expect("crates");
            assert_eq!(
                ep.url.as_str(),
                "https://crates.io/api/v1/crates?q=serde%20json&per_page=10"
            );
            assert!(ep.gh_api_path.is_none());

            let ep = build_endpoint(RemoteProvider::NpmRegistry, "@types/node", alloc).expect("npm");
            assert_eq!(
                ep.url.as_str(),
                "https://registry.npmjs.org/-/v1/search?text=%40types%2Fnode&size=10"
            );
            assert!(ep.gh_api_path.is_none());

            let ep = build_endpoint(RemoteProvider::Homebrew, "café", alloc).expect("utf8 brew");
            assert_eq!(
                ep.url.as_str(),
                "https://formulae.brew.sh/api/formula/caf%C3%A9.json"
            );
            assert!(ep.gh_api_path.is_none());
        });
    }

    #[test]
    fn format_compact_counts() {
        with_test_alloc(|alloc| {
            assert_eq!(format_compact_count_in(0, alloc).as_str(), "0");
            assert_eq!(format_compact_count_in(950, alloc).as_str(), "950");
            assert_eq!(format_compact_count_in(1_000, alloc).as_str(), "1k");
            assert_eq!(format_compact_count_in(12_400, alloc).as_str(), "12.4k");
            assert_eq!(format_compact_count_in(52_431, alloc).as_str(), "52.4k");
            assert_eq!(format_compact_count_in(150_200, alloc).as_str(), "150k");
            assert_eq!(format_compact_count_in(1_500_000, alloc).as_str(), "1.5M");
            assert_eq!(format_compact_count_in(340_000_000, alloc).as_str(), "340M");
            assert_eq!(
                format_compact_count_in(2_300_000_000, alloc).as_str(),
                "2.3B"
            );
        });
    }

    #[test]
    fn parse_github_response_with_escapes_and_unicode() {
        with_test_alloc(|alloc| {
            let body = br#"{
                "total_count": 2,
                "incomplete_results": false,
                "items": [
                    {
                        "id": 123,
                        "full\u005Fname": "BurntSushi/ripgrep",
                        "clone_url": "https://github.com/BurntSushi/ripgrep.git",
                        "description": "Fast \"grep\" tool \u2728 \uD83E\uDD80\\n",
                        "stargazers_count": 52431,
                        "language": "Rust",
                        "topics": ["rust", "search"],
                        "owner": {
                            "login": "BurntSushi",
                            "avatar_url": "https://avatars.githubusercontent.com/u/456674?v=4",
                            "site_admin": false
                        }
                    },
                    {
                        "full_name": "foo/bar",
                        "description": null,
                        "stargazers_count": 850,
                        "language": null,
                        "owner": {
                            "login": "foo",
                            "avatar_url": "https://avatars.githubusercontent.com/u/1"
                        }
                    }
                ]
            }"#;

            let git_items = parse_response(
                RemoteProvider::GitHubRepo {
                    insert_git_url: true,
                },
                body,
                alloc,
            );
            assert_eq!(git_items.len(), 2);
            assert_eq!(git_items[0].label.as_str(), "BurntSushi/ripgrep");
            assert_eq!(
                git_items[0].insert.as_str(),
                "https://github.com/BurntSushi/ripgrep.git"
            );
            assert_eq!(git_items[0].detail.as_str(), "★ 52.4k · Rust");
            assert_eq!(git_items[0].doc.as_str(), "Fast \"grep\" tool ✨ 🦀\\n");
            assert_eq!(
                git_items[0].owner_login.as_ref().map(AString::as_str),
                Some("BurntSushi")
            );
            assert_eq!(
                git_items[0].avatar_url.as_ref().map(AString::as_str),
                Some("https://avatars.githubusercontent.com/u/456674?v=4&s=64")
            );
            assert_eq!(git_items[0].popularity, 52431);

            // Fallback clone_url synthesized from full_name, no language, avatar without '?'.
            assert_eq!(
                git_items[1].insert.as_str(),
                "https://github.com/foo/bar.git"
            );
            assert_eq!(git_items[1].detail.as_str(), "★ 850");
            assert_eq!(git_items[1].doc.as_str(), "");
            assert_eq!(
                git_items[1].avatar_url.as_ref().map(AString::as_str),
                Some("https://avatars.githubusercontent.com/u/1?s=64")
            );

            // gh repo mode inserts owner/repo slug.
            let gh_items = parse_response(
                RemoteProvider::GitHubRepo {
                    insert_git_url: false,
                },
                body,
                alloc,
            );
            assert_eq!(gh_items[0].insert.as_str(), "BurntSushi/ripgrep");
            assert_eq!(gh_items[1].insert.as_str(), "foo/bar");
        });
    }

    #[test]
    fn parse_crates_npm_and_homebrew_responses() {
        with_test_alloc(|alloc| {
            let crates_body = br#"{
                "crates": [
                    {
                        "name": "serde",
                        "max_stable_version": "1.0.219",
                        "max_version": "1.0.219",
                        "newest_version": "1.0.219",
                        "downloads": 340000000,
                        "description": "A generic serialization\/deserialization framework"
                    },
                    {
                        "name": "prerelease-crate",
                        "max_stable_version": null,
                        "max_version": "0.1.0-alpha.1",
                        "downloads": 12400,
                        "description": "Alpha crate"
                    }
                ]
            }"#;
            let crates = parse_response(RemoteProvider::CratesIo, crates_body, alloc);
            assert_eq!(crates.len(), 2);
            assert_eq!(crates[0].label.as_str(), "serde");
            assert_eq!(crates[0].insert.as_str(), "serde");
            assert_eq!(crates[0].detail.as_str(), "v1.0.219 · 340M");
            assert_eq!(
                crates[0].doc.as_str(),
                "A generic serialization/deserialization framework"
            );
            assert_eq!(crates[0].popularity, 340_000_000);
            assert_eq!(crates[1].detail.as_str(), "v0.1.0-alpha.1 · 12.4k");

            let npm_body = br#"{
                "objects": [
                    {
                        "package": {
                            "name": "zod",
                            "version": "3.24.2",
                            "description": "TypeScript-first schema validation"
                        },
                        "score": { "final": 0.91 }
                    }
                ]
            }"#;
            let npm = parse_response(RemoteProvider::NpmRegistry, npm_body, alloc);
            assert_eq!(npm.len(), 1);
            assert_eq!(npm[0].label.as_str(), "zod");
            assert_eq!(npm[0].insert.as_str(), "zod");
            assert_eq!(npm[0].detail.as_str(), "v3.24.2");
            assert_eq!(npm[0].doc.as_str(), "TypeScript-first schema validation");

            let brew_body = br#"{
                "name": "ripgrep",
                "full_name": "ripgrep",
                "desc": "Search tool like grep and The Silver Searcher",
                "versions": { "stable": "14.1.1", "head": "HEAD", "bottle": true }
            }"#;
            let brew = parse_response(RemoteProvider::Homebrew, brew_body, alloc);
            assert_eq!(brew.len(), 1);
            assert_eq!(brew[0].label.as_str(), "ripgrep");
            assert_eq!(brew[0].insert.as_str(), "ripgrep");
            assert_eq!(brew[0].detail.as_str(), "v14.1.1 · brew");
            assert_eq!(
                brew[0].doc.as_str(),
                "Search tool like grep and The Silver Searcher"
            );

            // Malformed / 404 / deeply nested responses return empty gracefully without panicking.
            assert!(parse_response(RemoteProvider::Homebrew, b"<html>404</html>", alloc).is_empty());
            assert!(
                parse_response(
                    RemoteProvider::GitHubRepo {
                        insert_git_url: true
                    },
                    br#"{"message":"API rate limit exceeded"}"#,
                    alloc
                )
                .is_empty()
            );
            let deep = b"{\"a\":[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]}";
            assert!(parse_response(RemoteProvider::Homebrew, deep, alloc).is_empty());
        });
    }
}
