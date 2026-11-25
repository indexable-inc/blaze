//! Sans-IO OpenSSH `known_hosts` parser extracting unhashed hostnames & `[host]:port` entries.

use blaze_own::{AString, AVec, Alloc, String, Vec};

/// One concrete host entry parsed from `~/.ssh/known_hosts`.
#[derive(Debug, PartialEq, Eq)]
pub struct KnownHost<'a> {
    pub host: AString<'a>,
    pub port: Option<u16>,
}

/// Parse `~/.ssh/known_hosts` text, skipping comments, `@revoked`, wildcards, and hashed `|1|...`
/// hostnames, deduplicating up to `limit` hosts.
pub fn parse_known_hosts<'a>(text: &str, limit: usize, alloc: Alloc<'a>) -> AVec<'a, KnownHost<'a>> {
    let mut out = Vec::new_in(alloc);
    for raw_line in text.lines() {
        if out.len() >= limit {
            break;
        }
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('@') {
            continue;
        }
        let Some(host_field) = line.split_whitespace().next() else {
            continue;
        };
        if host_field.starts_with("|1|") {
            continue;
        }
        for part in host_field.split(',') {
            if out.len() >= limit {
                break;
            }
            let part = part.trim();
            if part.is_empty()
                || part.starts_with('!')
                || part.contains('*')
                || part.contains('?')
            {
                continue;
            }
            let (host, port) = if let Some(inner) = part.strip_prefix('[') {
                if let Some((h, p_rest)) = inner.split_once("]:") {
                    (h, p_rest.parse::<u16>().ok())
                } else {
                    continue;
                }
            } else {
                (part, None)
            };
            if host.is_empty() {
                continue;
            }
            if out.iter().any(|k: &KnownHost<'a>| k.host.as_str() == host && k.port == port) {
                continue;
            }
            out.push(KnownHost { host: String::from_str_in(host, alloc), port });
        }
    }
    out
}
