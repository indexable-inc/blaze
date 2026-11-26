//! Sans-IO parser for `--help` / `-h` output across clap, commander (`claude`), cobra (`gh`,
//! `docker`, `kubectl`), GNU (`git`, `tar`, `curl`), argparse (`python3`), and POSIX CLI tools.

use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::{CliSpec, FlagSpec, SpecSource, SubcommandSpec};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Unknown,
    Options,
    Commands,
}

/// Strip ANSI escape sequences (`\x1b[...m`) and backspace overstrikes (`c\bc`) into `alloc`.
pub fn strip_ansi_and_overstrike_in<'a>(raw: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.peek() == Some(&'[') {
                let _ = chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() || c == '~' {
                        break;
                    }
                }
            }
            continue;
        }
        if ch == '\x08' {
            out.pop();
            continue;
        }
        out.push(ch);
    }
    out
}

/// Parse `--help` / `-h` output for `command` into flags and subcommands allocated in `alloc`.
pub fn parse_help_output_in<'a>(command: &str, raw: &str, alloc: Alloc<'a>) -> CliSpec<'a> {
    let clean = strip_ansi_and_overstrike_in(raw, alloc);
    let mut spec = CliSpec::empty_in(command, SpecSource::Help, alloc);
    let mut section = Section::Unknown;
    let mut lines: AVec<'a, &str> = Vec::new_in(alloc);
    for l in clean.as_str().lines() {
        lines.push(l);
    }
    let mut i = 0usize;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
        if indent <= 2 && is_section_header(trimmed) {
            let body = trimmed.trim_end_matches(':');
            if contains_ascii_ci(body, "command") {
                section = Section::Commands;
            } else if contains_ascii_ci(body, "option")
                || contains_ascii_ci(body, "flag")
                || contains_ascii_ci(body, "argument")
            {
                section = Section::Options;
            } else {
                section = Section::Unknown;
            }
            i += 1;
            continue;
        }

        if indent <= 8 && trimmed.starts_with('-') && !trimmed.starts_with("---") {
            if let Some(parsed_flags) = parse_flag_line(line, lines.as_slice(), &mut i, alloc) {
                for f in parsed_flags.into_iter() {
                    push_unique_flag(&mut spec.flags, f);
                }
                continue;
            }
        }

        if section == Section::Commands
            && (2..=8).contains(&indent)
            && !trimmed.starts_with('-')
            && let Some(sub) = parse_subcommand_line(trimmed, alloc)
        {
            if !spec.subcommands.iter().any(|s| s.name.as_str() == sub.name.as_str()) {
                spec.subcommands.push(sub);
            }
        }

        i += 1;
    }

    spec
}

fn contains_ascii_ci(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || n.len() > h.len() {
        return false;
    }
    for win in h.windows(n.len()) {
        if win
            .iter()
            .zip(n.iter())
            .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
        {
            return true;
        }
    }
    false
}

fn is_section_header(trimmed: &str) -> bool {
    if trimmed.len() > 48 || trimmed.starts_with('-') {
        return false;
    }
    if trimmed.ends_with(':') {
        let body = trimmed.trim_end_matches(':');
        return !body.is_empty()
            && !body.contains("  ")
            && body
                .chars()
                .all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '-' || c == '/');
    }
    trimmed.eq_ignore_ascii_case("options")
        || trimmed.eq_ignore_ascii_case("flags")
        || trimmed.eq_ignore_ascii_case("commands")
        || trimmed.eq_ignore_ascii_case("subcommands")
        || trimmed.eq_ignore_ascii_case("available commands")
        || trimmed.eq_ignore_ascii_case("core commands")
}

fn parse_flag_line<'a>(
    line: &str,
    lines: &[&str],
    idx: &mut usize,
    alloc: Alloc<'a>,
) -> Option<AVec<'a, FlagSpec<'a>>> {
    let trimmed = line.trim_start();
    let base_indent = line.len() - trimmed.len();
    let (spec_part, mut desc) = split_spec_and_desc(trimmed);

    let mut consumed_next = false;
    if desc.is_empty() && *idx + 1 < lines.len() {
        let next = lines[*idx + 1];
        let next_trim = next.trim();
        let next_indent = next.len() - next.trim_start_matches([' ', '\t']).len();
        if !next_trim.is_empty()
            && next_indent > base_indent + 1
            && !next_trim.starts_with('-')
        {
            desc = next_trim;
            consumed_next = true;
        }
    }

    let parsed = parse_flag_spec_chunk(spec_part, desc, alloc)?;
    *idx += if consumed_next { 2 } else { 1 };
    Some(parsed)
}

fn split_spec_and_desc(trimmed: &str) -> (&str, &str) {
    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    let mut angle_depth = 0u32;
    let mut bracket_depth = 0u32;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => angle_depth += 1,
            b'>' => angle_depth = angle_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'\t' if angle_depth == 0 && bracket_depth == 0 && i > 0 => {
                return (trimmed[..i].trim_end(), trimmed[i..].trim());
            }
            b' ' if angle_depth == 0
                && bracket_depth == 0
                && i > 0
                && i + 1 < bytes.len()
                && bytes[i + 1] == b' ' =>
            {
                return (trimmed[..i].trim_end(), trimmed[i..].trim());
            }
            _ => {}
        }
        i += 1;
    }
    (trimmed, "")
}

fn parse_flag_spec_chunk<'a>(
    spec_part: &str,
    desc: &str,
    alloc: Alloc<'a>,
) -> Option<AVec<'a, FlagSpec<'a>>> {
    if spec_part.starts_with("- ") || spec_part.starts_with("-- ") {
        return None;
    }

    let mut shorts: AVec<'a, &str> = Vec::new_in(alloc);
    let mut longs: AVec<'a, &str> = Vec::new_in(alloc);
    let mut value_hint: Option<&str> = None;

    let mut raw_tokens: AVec<'a, &str> = Vec::new_in(alloc);
    let bytes = spec_part.as_bytes();
    let mut start = None;
    let mut depth = 0u32;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'<' | b'[' => {
                if start.is_none() {
                    start = Some(i);
                }
                depth += 1;
            }
            b'>' | b']' => {
                depth = depth.saturating_sub(1);
            }
            b' ' | b',' | b'/' | b'|' if depth == 0 => {
                if let Some(s) = start.take() {
                    raw_tokens.push(&spec_part[s..i]);
                }
            }
            _ => {
                if start.is_none() {
                    start = Some(i);
                }
            }
        }
    }
    if let Some(s) = start {
        raw_tokens.push(&spec_part[s..]);
    }

    for &tok in raw_tokens.iter() {
        let tok = tok.trim_matches(|c| c == ',' || c == ';');
        if tok.is_empty() {
            continue;
        }
        if tok.starts_with("--") {
            let (flag_name, inline_val) = split_inline_value(tok);
            if is_valid_long_flag(flag_name) {
                longs.push(flag_name);
                if value_hint.is_none() && inline_val.is_some() {
                    value_hint = inline_val;
                }
            }
        } else if tok.starts_with('-') && tok.len() >= 2 {
            let (flag_name, inline_val) = split_inline_value(tok);
            if is_valid_short_flag(flag_name) {
                shorts.push(flag_name);
                if value_hint.is_none() && inline_val.is_some() {
                    value_hint = inline_val;
                }
            }
        } else if value_hint.is_none()
            && (tok.starts_with('<')
                || tok.starts_with('[')
                || tok.chars().all(|c| c.is_ascii_uppercase() || c == '_' || c == '-'))
        {
            value_hint = Some(tok);
        }
    }

    if shorts.is_empty() && longs.is_empty() {
        return None;
    }

    let mut out = Vec::new_in(alloc);
    if longs.is_empty() {
        for &s in shorts.iter() {
            out.push(FlagSpec {
                long: None,
                short: Some(String::from_str_in(s, alloc)),
                value_hint: value_hint.map(|h| String::from_str_in(h, alloc)),
                description: collapse_spaces_in(desc, alloc),
            });
        }
    } else {
        for (idx, &l) in longs.iter().enumerate() {
            let short = if idx == 0 {
                shorts.first().map(|s| String::from_str_in(s, alloc))
            } else {
                None
            };
            out.push(FlagSpec {
                long: Some(String::from_str_in(l, alloc)),
                short,
                value_hint: value_hint.map(|h| String::from_str_in(h, alloc)),
                description: collapse_spaces_in(desc, alloc),
            });
        }
    }
    Some(out)
}

fn split_inline_value(tok: &str) -> (&str, Option<&str>) {
    if let Some(pos) = tok.find(['=', '[']) {
        let name = &tok[..pos];
        let val = tok[pos..].trim_start_matches('=');
        let hint = if val.is_empty() { None } else { Some(val) };
        (name, hint)
    } else {
        (tok, None)
    }
}

fn is_valid_long_flag(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("--") else {
        return false;
    };
    !rest.is_empty()
        && rest.len() <= 64
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && rest.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
}

fn is_valid_short_flag(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('-') else {
        return false;
    };
    rest.len() == 1 && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '?')
}

fn parse_subcommand_line<'a>(trimmed: &str, alloc: Alloc<'a>) -> Option<SubcommandSpec<'a>> {
    let (left, desc) = split_spec_and_desc(trimmed);
    if desc.is_empty() {
        return None;
    }
    let name = left.split([' ', ',', '|', '[']).next()?.trim();
    if name.is_empty()
        || name.len() > 32
        || name.starts_with('-')
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        || !name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
    {
        return None;
    }
    Some(SubcommandSpec {
        name: String::from_str_in(name, alloc),
        description: collapse_spaces_in(desc, alloc),
    })
}

fn push_unique_flag<'a>(flags: &mut AVec<'a, FlagSpec<'a>>, f: FlagSpec<'a>) {
    if flags.iter().any(|e| {
        (f.long.is_some()
            && e.long.as_ref().map(AString::as_str) == f.long.as_ref().map(AString::as_str))
            || (f.long.is_none()
                && f.short.is_some()
                && e.short.as_ref().map(AString::as_str) == f.short.as_ref().map(AString::as_str))
    }) {
        return;
    }
    flags.push(f);
}

fn collapse_spaces_in<'a>(s: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    let mut prev_ws = false;
    for ch in s.trim().chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_commander_help_no_std() {
        let alloc = blaze_own::testing::heap().alloc();
        let sample = r#"
Usage: claude [options] [command] [prompt]

Options:
  --dangerously-skip-permissions        Bypass all permission checks.
  --allowedTools, --allowed-tools <tools...>
      Comma or space-separated list of tool names to allow
  -p, --print                           Print response and exit
  --bg, --background                    Start the session in the background

Commands:
  mcp                                   Configure and manage MCP servers
  doctor                                Check health of auto-updater
"#;
        let spec = parse_help_output_in("claude", sample, alloc);
        assert!(spec.flags.iter().any(|f| {
            f.long.as_ref().map(AString::as_str) == Some("--dangerously-skip-permissions")
                && f.description.as_str().contains("Bypass")
        }));
        assert!(spec.flags.iter().any(|f| {
            f.long.as_ref().map(AString::as_str) == Some("--allowed-tools")
                && f.description.as_str().contains("Comma or space-separated")
        }));
        assert!(spec.flags.iter().any(|f| {
            f.long.as_ref().map(AString::as_str) == Some("--print")
                && f.short.as_ref().map(AString::as_str) == Some("-p")
        }));
        assert!(spec.subcommands.iter().any(|s| s.name.as_str() == "mcp"));
        assert!(spec.subcommands.iter().any(|s| s.name.as_str() == "doctor"));
    }
}
