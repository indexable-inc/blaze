//! Sans-IO OpenSSH `ssh_config(5)` parser, `Include` loader state machine, wildcard matcher,
//! and effective host resolver (`Host`, `HostName`, `User`, `Port`, `IdentityFile`, `ProxyJump`,
//! `ProxyCommand`, `ForwardAgent`, `Include`).

use blaze_own::{AString, AVec, Alloc, String, Vec};

/// Explicit bounds for parsing untrusted OpenSSH config files (rule `bounded-progress`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigLimits {
    /// Maximum number of bytes accepted from a single config file.
    pub max_file_bytes: usize,
    /// Maximum `Include` nesting depth (prevents cycles).
    pub max_include_depth: u8,
    /// Maximum total files loaded including root + all `Include` targets.
    pub max_files: u16,
    /// Maximum host/match blocks retained.
    pub max_blocks: usize,
    /// Maximum concrete hosts returned by [`SshConfig::list_hosts`].
    pub max_hosts: usize,
}

impl ConfigLimits {
    pub const SHIPPED: Self = Self {
        max_file_bytes: 1 << 20, // 1 MiB
        max_include_depth: 8,
        max_files: 64,
        max_blocks: 4096,
        max_hosts: 512,
    };
}

/// One pattern inside a `Host` directive (possibly negated with `!`).
#[derive(Debug, PartialEq, Eq)]
pub struct HostPattern<'a> {
    pub negated: bool,
    pub pattern: AString<'a>,
}

/// One `Host ...` stanza from an OpenSSH config file.
#[derive(Debug, PartialEq, Eq)]
pub struct HostBlock<'a> {
    pub patterns: AVec<'a, HostPattern<'a>>,
    pub hostname: Option<AString<'a>>,
    pub user: Option<AString<'a>>,
    pub port: Option<u16>,
    pub identity_files: AVec<'a, AString<'a>>,
    pub proxy_jump: Option<AString<'a>>,
    pub proxy_command: Option<AString<'a>>,
    pub forward_agent: Option<bool>,
}

impl<'a> HostBlock<'a> {
    fn new(alloc: Alloc<'a>) -> Self {
        Self {
            patterns: Vec::new_in(alloc),
            hostname: None,
            user: None,
            port: None,
            identity_files: Vec::new_in(alloc),
            proxy_jump: None,
            proxy_command: None,
            forward_agent: None,
        }
    }

    fn matches(&self, host: &str) -> bool {
        let mut positive = false;
        for pat in self.patterns.iter() {
            if glob_match(pat.pattern.as_str(), host) {
                if pat.negated {
                    return false;
                }
                positive = true;
            }
        }
        positive
    }
}

/// Effective SSH connection parameters resolved for a specific host alias (OpenSSH first-match-wins).
#[derive(Debug, PartialEq, Eq)]
pub struct ResolvedHost<'a> {
    pub alias: AString<'a>,
    pub hostname: Option<AString<'a>>,
    pub user: Option<AString<'a>>,
    pub port: Option<u16>,
    pub identity_files: AVec<'a, AString<'a>>,
    pub proxy_jump: Option<AString<'a>>,
    pub proxy_command: Option<AString<'a>>,
    pub forward_agent: Option<bool>,
}

/// Concise host entry for shell completion & connection pickers.
#[derive(Debug, PartialEq, Eq)]
pub struct HostSummary<'a> {
    pub alias: AString<'a>,
    pub hostname: Option<AString<'a>>,
    pub user: Option<AString<'a>>,
    pub port: Option<u16>,
    pub proxy_jump: Option<AString<'a>>,
}

/// Parsed OpenSSH configuration (root file + any resolved `Include` fragments).
#[derive(Debug)]
pub struct SshConfig<'a> {
    pub blocks: AVec<'a, HostBlock<'a>>,
    pub includes: AVec<'a, AString<'a>>,
}

impl<'a> SshConfig<'a> {
    /// Resolve effective OpenSSH parameters for `alias` (first matching directive wins per OpenSSH).
    pub fn resolve(&self, alias: &str, alloc: Alloc<'a>) -> ResolvedHost<'a> {
        let mut out = ResolvedHost {
            alias: String::from_str_in(alias, alloc),
            hostname: None,
            user: None,
            port: None,
            identity_files: Vec::new_in(alloc),
            proxy_jump: None,
            proxy_command: None,
            forward_agent: None,
        };
        for blk in self.blocks.iter() {
            if !blk.matches(alias) {
                continue;
            }
            if out.hostname.is_none()
                && let Some(h) = &blk.hostname
            {
                out.hostname = Some(expand_host_tokens(h.as_str(), alias, alloc));
            }
            if out.user.is_none()
                && let Some(u) = &blk.user
            {
                out.user = Some(String::from_str_in(u.as_str(), alloc));
            }
            if out.port.is_none() {
                out.port = blk.port;
            }
            for id in blk.identity_files.iter() {
                out.identity_files.push(String::from_str_in(id.as_str(), alloc));
            }
            if out.proxy_jump.is_none()
                && let Some(pj) = &blk.proxy_jump
            {
                out.proxy_jump = Some(String::from_str_in(pj.as_str(), alloc));
            }
            if out.proxy_command.is_none()
                && let Some(pc) = &blk.proxy_command
            {
                out.proxy_command = Some(String::from_str_in(pc.as_str(), alloc));
            }
            if out.forward_agent.is_none() {
                out.forward_agent = blk.forward_agent;
            }
        }
        out
    }

    /// Enumerate all concrete (non-wildcard, non-negated) host aliases defined across stanzas,
    /// each paired with its resolved `HostName`/`User`/`Port`/`ProxyJump`.
    pub fn list_hosts(&self, limit: usize, alloc: Alloc<'a>) -> AVec<'a, HostSummary<'a>> {
        let mut out = Vec::new_in(alloc);
        for blk in self.blocks.iter() {
            for pat in blk.patterns.iter() {
                if out.len() >= limit {
                    return out;
                }
                let raw = pat.pattern.as_str();
                if pat.negated || raw.is_empty() || raw.contains('*') || raw.contains('?') {
                    continue;
                }
                if out.iter().any(|h: &HostSummary<'a>| h.alias.as_str() == raw) {
                    continue;
                }
                let resolved = self.resolve(raw, alloc);
                out.push(HostSummary {
                    alias: resolved.alias,
                    hostname: resolved.hostname,
                    user: resolved.user,
                    port: resolved.port,
                    proxy_jump: resolved.proxy_jump,
                });
            }
        }
        out
    }
}

/// Parse a single OpenSSH config text buffer in-place (sans-IO, zero panics).
pub fn parse_config<'a>(text: &str, limits: ConfigLimits, alloc: Alloc<'a>) -> SshConfig<'a> {
    let mut blocks = Vec::new_in(alloc);
    let mut includes = Vec::new_in(alloc);
    let bounded = if text.len() > limits.max_file_bytes {
        let mut end = limits.max_file_bytes;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    } else {
        text
    };

    // Directives before any `Host` line apply globally (equivalent to `Host *`).
    let mut current: Option<HostBlock<'a>> = None;
    let mut in_match_block = false;

    for raw_line in bounded.lines() {
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        let Some((key_raw, val_raw)) = split_directive(line) else {
            continue;
        };
        if key_raw.eq_ignore_ascii_case("include") {
            for tok in split_ssh_tokens(val_raw, alloc).iter() {
                if includes.len() < usize::from(limits.max_files) {
                    includes.push(String::from_str_in(tok.as_str(), alloc));
                }
            }
            continue;
        }
        if key_raw.eq_ignore_ascii_case("host") {
            if let Some(prev) = current.take()
                && blocks.len() < limits.max_blocks
            {
                blocks.push(prev);
            }
            in_match_block = false;
            let mut blk = HostBlock::new(alloc);
            for tok in split_ssh_tokens(val_raw, alloc).iter() {
                let s = tok.as_str();
                if let Some(rest) = s.strip_prefix('!') {
                    if !rest.is_empty() {
                        blk.patterns.push(HostPattern {
                            negated: true,
                            pattern: String::from_str_in(rest, alloc),
                        });
                    }
                } else if !s.is_empty() {
                    blk.patterns.push(HostPattern {
                        negated: false,
                        pattern: String::from_str_in(s, alloc),
                    });
                }
            }
            current = Some(blk);
            continue;
        }
        if key_raw.eq_ignore_ascii_case("match") {
            if let Some(prev) = current.take()
                && blocks.len() < limits.max_blocks
            {
                blocks.push(prev);
            }
            in_match_block = true;
            continue;
        }
        if in_match_block {
            continue;
        }
        let blk = current.get_or_insert_with(|| {
            let mut global = HostBlock::new(alloc);
            global.patterns.push(HostPattern {
                negated: false,
                pattern: String::from_str_in("*", alloc),
            });
            global
        });
        let val = unquote(val_raw.trim());
        if val.is_empty() {
            continue;
        }
        if key_raw.eq_ignore_ascii_case("hostname") {
            if blk.hostname.is_none() {
                blk.hostname = Some(String::from_str_in(val, alloc));
            }
        } else if key_raw.eq_ignore_ascii_case("user") {
            if blk.user.is_none() {
                blk.user = Some(String::from_str_in(val, alloc));
            }
        } else if key_raw.eq_ignore_ascii_case("port") {
            if blk.port.is_none()
                && let Ok(p) = val.parse::<u16>()
            {
                blk.port = Some(p);
            }
        } else if key_raw.eq_ignore_ascii_case("identityfile") {
            blk.identity_files.push(String::from_str_in(val, alloc));
        } else if key_raw.eq_ignore_ascii_case("proxyjump") {
            if blk.proxy_jump.is_none() {
                blk.proxy_jump = Some(String::from_str_in(val, alloc));
            }
        } else if key_raw.eq_ignore_ascii_case("proxycommand") {
            if blk.proxy_command.is_none() {
                blk.proxy_command = Some(String::from_str_in(val, alloc));
            }
        } else if key_raw.eq_ignore_ascii_case("forwardagent") && blk.forward_agent.is_none() {
            if val.eq_ignore_ascii_case("yes") || val.eq_ignore_ascii_case("true") {
                blk.forward_agent = Some(true);
            } else if val.eq_ignore_ascii_case("no") || val.eq_ignore_ascii_case("false") {
                blk.forward_agent = Some(false);
            }
        }
    }

    if let Some(last) = current
        && blocks.len() < limits.max_blocks
    {
        blocks.push(last);
    }

    SshConfig { blocks, includes }
}

/// Sans-IO multi-file OpenSSH config loader supporting recursive `Include` directives with
/// strict depth & file count caps (rule `sans-io-cores`, `bounded-progress`).
pub struct ConfigLoader<'a> {
    alloc: Alloc<'a>,
    limits: ConfigLimits,
    queue: AVec<'a, (AString<'a>, u8)>,
    files_loaded: u16,
    blocks: AVec<'a, HostBlock<'a>>,
    includes: AVec<'a, AString<'a>>,
}

/// Next action requested by [`ConfigLoader::poll`].
pub enum LoadStep<'a> {
    /// Driver should read the requested include glob/path (relative to `~/.ssh/` if not absolute)
    /// and pass all matching file texts via [`ConfigLoader::feed_files`].
    NeedInclude(AString<'a>),
    /// All config files have been parsed into a single merged [`SshConfig`].
    Done(SshConfig<'a>),
}

impl<'a> ConfigLoader<'a> {
    pub fn new(root_text: &str, limits: ConfigLimits, alloc: Alloc<'a>) -> Self {
        let parsed = parse_config(root_text, limits, alloc);
        let mut queue = Vec::new_in(alloc);
        for inc in parsed.includes.iter() {
            queue.push((String::from_str_in(inc.as_str(), alloc), 1));
        }
        Self {
            alloc,
            limits,
            queue,
            files_loaded: 1,
            blocks: parsed.blocks,
            includes: parsed.includes,
        }
    }

    pub fn poll(&mut self) -> LoadStep<'a> {
        while let Some((pattern, depth)) = self.queue.pop() {
            if depth <= self.limits.max_include_depth && self.files_loaded < self.limits.max_files {
                return LoadStep::NeedInclude(pattern);
            }
        }
        LoadStep::Done(SshConfig {
            blocks: core::mem::replace(&mut self.blocks, Vec::new_in(self.alloc)),
            includes: core::mem::replace(&mut self.includes, Vec::new_in(self.alloc)),
        })
    }

    /// Supply the contents of files matching the last [`LoadStep::NeedInclude`] pattern.
    pub fn feed_files(&mut self, contents: &[&str], next_depth: u8) {
        for &text in contents {
            if self.files_loaded >= self.limits.max_files {
                break;
            }
            self.files_loaded = self.files_loaded.saturating_add(1);
            let parsed = parse_config(text, self.limits, self.alloc);
            for blk in parsed.blocks {
                if self.blocks.len() < self.limits.max_blocks {
                    self.blocks.push(blk);
                }
            }
            if next_depth <= self.limits.max_include_depth {
                for inc in parsed.includes.iter() {
                    self.queue.push((String::from_str_in(inc.as_str(), self.alloc), next_depth));
                }
            }
        }
    }
}

/// OpenSSH-style wildcard glob matcher (`*` matches 0+ chars, `?` matches any single char).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_pi: Option<usize> = None;
    let mut star_ti: usize = 0;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi].eq_ignore_ascii_case(&t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

fn expand_host_tokens<'a>(raw: &str, alias: &str, alloc: Alloc<'a>) -> AString<'a> {
    if !raw.contains('%') {
        return String::from_str_in(raw, alloc);
    }
    let mut out = String::new_in(alloc);
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next() {
                Some('h') => out.push_str(alias),
                Some('%') => out.push('%'),
                Some(other) => {
                    out.push('%');
                    out.push(other);
                }
                None => out.push('%'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (idx, b) in line.bytes().enumerate() {
        if b == b'"' {
            in_quotes = !in_quotes;
        } else if b == b'#' && !in_quotes {
            return &line[..idx];
        }
    }
    line
}

fn split_directive(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let key = &line[..i];
    let mut rest = line[i..].trim_start();
    if let Some(after_eq) = rest.strip_prefix('=') {
        rest = after_eq.trim_start();
    }
    Some((key, rest))
}

fn unquote(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn split_ssh_tokens<'a>(raw: &str, alloc: Alloc<'a>) -> AVec<'a, AString<'a>> {
    let mut out = Vec::new_in(alloc);
    let mut cur = String::new_in(alloc);
    let mut in_quotes = false;
    for ch in raw.chars() {
        if ch == '"' {
            in_quotes = !in_quotes;
        } else if ch.is_whitespace() && !in_quotes {
            if !cur.is_empty() {
                out.push(core::mem::replace(&mut cur, String::new_in(alloc)));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
