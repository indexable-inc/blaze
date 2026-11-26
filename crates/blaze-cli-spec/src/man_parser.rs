//! Sans-IO parsers for Fish completion files (`complete -c <cmd> ...`) and BSD/GNU `man` pages.

use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::help_parser::strip_ansi_and_overstrike_in;
use crate::{CliSpec, FlagSpec, SpecSource, SubcommandSpec};

/// Parse Fish `complete -c <cmd> -s <short> -l <long> -d '<desc>'` text in `alloc`.
pub fn parse_fish_completions_in<'a>(cmd: &str, src: &str, alloc: Alloc<'a>) -> CliSpec<'a> {
    let mut spec = CliSpec::empty_in(cmd, SpecSource::Fish, alloc);
    for raw_line in src.lines() {
        let line = raw_line.trim();
        if !line.starts_with("complete ") || line.contains("__fish_seen_subcommand_from") {
            continue;
        }
        let tokens = split_shell_words_in(line, alloc);
        let mut short: Option<AString<'a>> = None;
        let mut long: Option<AString<'a>> = None;
        let mut desc = String::new_in(alloc);
        let mut sub_arg: Option<&str> = None;
        let mut is_top_subcmd_cond = false;

        let mut i = 1usize;
        while i < tokens.len() {
            match tokens[i].as_str() {
                "-s" if i + 1 < tokens.len() => {
                    let s = tokens[i + 1].as_str();
                    if s.len() == 1 {
                        let mut out = String::from_str_in("-", alloc);
                        out.push_str(s);
                        short = Some(out);
                    }
                    i += 2;
                }
                "-l" | "-o" if i + 1 < tokens.len() => {
                    let l = tokens[i + 1].as_str();
                    if !l.is_empty() {
                        let mut out = String::from_str_in("--", alloc);
                        out.push_str(l);
                        long = Some(out);
                    }
                    i += 2;
                }
                "-d" if i + 1 < tokens.len() => {
                    desc = String::from_str_in(tokens[i + 1].as_str(), alloc);
                    i += 2;
                }
                "-a" if i + 1 < tokens.len() => {
                    sub_arg = Some(tokens[i + 1].as_str());
                    i += 2;
                }
                "-n" if i + 1 < tokens.len() => {
                    if tokens[i + 1].as_str().contains("__fish_use_subcommand") {
                        is_top_subcmd_cond = true;
                    }
                    i += 2;
                }
                _ => i += 1,
            }
        }

        if short.is_some() || long.is_some() {
            if !spec.flags.iter().any(|e| {
                (long.is_some()
                    && e.long.as_ref().map(AString::as_str) == long.as_ref().map(AString::as_str))
                    || (long.is_none()
                        && short.is_some()
                        && e.short.as_ref().map(AString::as_str)
                            == short.as_ref().map(AString::as_str))
            }) {
                spec.flags.push(FlagSpec {
                    long,
                    short,
                    value_hint: None,
                    description: desc,
                });
            }
        } else if is_top_subcmd_cond
            && let Some(arg) = sub_arg
            && !arg.contains(' ')
            && !arg.starts_with('-')
            && !arg.starts_with('(')
            && !spec.subcommands.iter().any(|s| s.name.as_str() == arg)
        {
            spec.subcommands.push(SubcommandSpec {
                name: String::from_str_in(arg, alloc),
                description: desc,
            });
        }
    }
    spec
}

/// Parse formatted `man` page text (stripping overstrikes) for flags into `alloc`.
pub fn parse_man_output_in<'a>(cmd: &str, raw: &str, alloc: Alloc<'a>) -> CliSpec<'a> {
    let clean = strip_ansi_and_overstrike_in(raw, alloc);
    let mut spec = CliSpec::empty_in(cmd, SpecSource::Man, alloc);
    let mut lines: AVec<'a, &str> = Vec::new_in(alloc);
    for l in clean.as_str().lines() {
        lines.push(l);
    }
    let mut i = 0usize;
    let mut in_options = false;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start_matches(' ').len();

        if indent == 0 && !trimmed.is_empty() && trimmed.chars().all(|c| !c.is_ascii_lowercase()) {
            in_options = matches!(
                trimmed,
                "OPTIONS" | "DESCRIPTION" | "FLAGS" | "COMMANDS" | "COMMAND-LINE OPTIONS"
            );
            i += 1;
            continue;
        }

        if in_options && (3..=10).contains(&indent) && trimmed.starts_with('-') {
            let (flag_chunk, inline_desc) = if let Some(pos) = trimmed.find("  ") {
                (&trimmed[..pos], trimmed[pos..].trim())
            } else {
                (trimmed, "")
            };
            let desc_str = if !inline_desc.is_empty() {
                inline_desc
            } else if i + 1 < lines.len() {
                let next = lines[i + 1];
                let next_indent = next.len() - next.trim_start_matches(' ').len();
                let next_trim = next.trim();
                if next_indent > indent && !next_trim.is_empty() && !next_trim.starts_with('-') {
                    next_trim
                } else {
                    ""
                }
            } else {
                ""
            };

            let mut short: Option<&str> = None;
            let mut long: Option<&str> = None;
            let mut value_hint: Option<&str> = None;

            for raw_tok in flag_chunk.split(|c: char| c == ',' || c.is_whitespace()) {
                let tok = raw_tok.trim();
                if tok.is_empty() {
                    continue;
                }
                if let Some(rest) = tok.strip_prefix("--") {
                    let name = rest.split('=').next().unwrap_or(rest);
                    if !name.is_empty()
                        && name.len() <= 48
                        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                        && long.is_none()
                    {
                        long = Some(&tok[..2 + name.len()]);
                    }
                } else if let Some(rest) = tok.strip_prefix('-') {
                    if rest.len() == 1
                        && rest.chars().all(|c| c.is_ascii_alphanumeric())
                        && short.is_none()
                    {
                        short = Some(tok);
                    }
                } else if value_hint.is_none()
                    && (tok.starts_with('<')
                        || tok.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
                {
                    value_hint = Some(tok);
                }
            }

            if (short.is_some() || long.is_some())
                && !spec.flags.iter().any(|e| {
                    (long.is_some() && e.long.as_ref().map(AString::as_str) == long)
                        || (long.is_none()
                            && short.is_some()
                            && e.short.as_ref().map(AString::as_str) == short)
                })
            {
                spec.flags.push(FlagSpec {
                    long: long.map(|l| String::from_str_in(l, alloc)),
                    short: short.map(|s| String::from_str_in(s, alloc)),
                    value_hint: value_hint.map(|h| String::from_str_in(h, alloc)),
                    description: String::from_str_in(desc_str, alloc),
                });
            }
        }
        i += 1;
    }
    spec
}

/// Extract a clean one-line summary from the `NAME` section of a formatted `man` page.
///
/// Strips ANSI escapes and BSD overstrikes, locates the `NAME` / `Name` section header, joins
/// wrapped lines until the next blank line or ALL-CAPS section header, takes the text after a
/// dash separator (` - `, ` -- `, ` – `, ` — `, ` ‐ `, ` − `) if present, and truncates cleanly
/// to at most 220 UTF-8 bytes on a char boundary.
pub fn parse_man_summary_in<'a>(src: &str, alloc: Alloc<'a>) -> Option<AString<'a>> {
    const MAX_SUMMARY_BYTES: usize = 220;

    let clean = strip_ansi_and_overstrike_in(src, alloc);
    let mut lines = clean.as_str().lines();

    let mut name_indent = None;
    for raw_line in lines.by_ref() {
        let trimmed = raw_line.trim();
        if trimmed == "NAME" || trimmed == "Name" {
            name_indent = Some(raw_line.len() - raw_line.trim_start_matches([' ', '\t']).len());
            break;
        }
    }
    let name_indent = name_indent?;

    let mut joined = String::new_in(alloc);
    for raw_line in lines {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            if !joined.as_str().is_empty() {
                break;
            }
            continue;
        }
        let indent = raw_line.len() - raw_line.trim_start_matches([' ', '\t']).len();
        if is_all_caps_section_header(trimmed, indent, name_indent) {
            break;
        }
        if !joined.as_str().is_empty() {
            joined.push(' ');
        }
        push_collapsed_ws(&mut joined, trimmed);
    }

    let full = joined.as_str().trim();
    if full.is_empty() {
        return None;
    }

    let after_sep = extract_after_dash_sep(full).trim();
    let chosen = if after_sep.is_empty() { full } else { after_sep };
    let truncated = truncate_utf8_bytes(chosen, MAX_SUMMARY_BYTES).trim_end();
    if truncated.is_empty() {
        return None;
    }
    Some(String::from_str_in(truncated, alloc))
}

fn is_all_caps_section_header(trimmed: &str, indent: usize, header_indent: usize) -> bool {
    if trimmed.is_empty() || trimmed.len() > 64 {
        return false;
    }
    if trimmed.contains(" - ")
        || trimmed.contains(" -- ")
        || trimmed.contains(" \u{2013} ")
        || trimmed.contains(" \u{2014} ")
    {
        return false;
    }
    let all_caps = trimmed.chars().any(|c| c.is_ascii_uppercase())
        && trimmed
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, ' ' | '-' | '_'));
    all_caps && indent <= header_indent
}

fn push_collapsed_ws(out: &mut AString<'_>, s: &str) {
    let mut prev_ws = false;
    for ch in s.chars() {
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
}

fn extract_after_dash_sep(s: &str) -> &str {
    const SEPS: &[&str] = &[
        " -- ",
        " - ",
        " \u{2013} ",
        " \u{2014} ",
        " \u{2010} ",
        " \u{2212} ",
    ];
    let mut best: Option<(usize, usize)> = None;
    for sep in SEPS {
        if let Some(pos) = s.find(sep) {
            let take = match best {
                None => true,
                Some((best_pos, best_len)) => pos < best_pos || (pos == best_pos && sep.len() > best_len),
            };
            if take {
                best = Some((pos, sep.len()));
            }
        }
    }
    if let Some((pos, len)) = best {
        &s[pos + len..]
    } else {
        s
    }
}

fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn split_shell_words_in<'a>(line: &str, alloc: Alloc<'a>) -> AVec<'a, AString<'a>> {
    let mut out = Vec::new_in(alloc);
    let mut cur = String::new_in(alloc);
    let mut quote: Option<char> = None;
    let mut escape = false;
    for ch in line.chars() {
        if escape {
            cur.push(ch);
            escape = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escape = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                cur.push(ch);
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if ch.is_whitespace() {
            if !cur.as_str().is_empty() {
                out.push(core::mem::replace(&mut cur, String::new_in(alloc)));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.as_str().is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use blaze_own::{Erased, Inline};

    use super::*;

    #[test]
    fn parses_fish_and_man_no_std() {
        let buf = Inline::<32768>::new();
        let erased = Erased::new(&buf);
        let alloc = erased.handle();
        let fish = r#"
complete -c fd -s H -l hidden -d 'Search hidden files and directories'
complete -c fd -s I -l no-ignore -d 'Do not respect .(git|fd)ignore files'
"#;
        let spec = parse_fish_completions_in("fd", fish, alloc);
        assert_eq!(spec.flags.len(), 2);
        assert_eq!(spec.flags[0].long.as_ref().map(AString::as_str), Some("--hidden"));
        assert_eq!(spec.flags[0].short.as_ref().map(AString::as_str), Some("-H"));

        let man = "OPTIONS\n     -a      Include directory entries whose names begin with a dot.\n     -l      List files in the long format.\n";
        let mspec = parse_man_output_in("ls", man, alloc);
        assert!(mspec.flags.iter().any(|f| f.short.as_ref().map(AString::as_str) == Some("-a")));
        assert!(mspec.flags.iter().any(|f| f.short.as_ref().map(AString::as_str) == Some("-l")));
    }

    #[test]
    fn parse_man_summary_bsd_overstrike_and_wrapped() {
        let buf = Inline::<32768>::new();
        let erased = Erased::new(&buf);
        let alloc = erased.handle();

        // BSD mandoc overstrike: bold NAME and bold command name + wrapped description.
        let bsd = concat!(
            "LS(1)                     General Commands Manual                    LS(1)\n\n",
            "N\x08NA\x08AM\x08ME\x08E\n",
            "     l\x08ls\x08s -- list directory\n",
            "     contents with metadata\n\n",
            "S\x08SY\x08YN\x08NO\x08OP\x08PS\x08SI\x08IS\x08S\n",
            "     ls [-ABCFGHILPRSTUWZadfgklmnpqrstux1] [file ...]\n",
        );
        let summary = parse_man_summary_in(bsd, alloc).expect("summary");
        assert_eq!(summary.as_str(), "list directory contents with metadata");
    }

    #[test]
    fn parse_man_summary_separators_sections_and_truncation() {
        let buf = Inline::<32768>::new();
        let erased = Erased::new(&buf);
        let alloc = erased.handle();

        // ANSI colored header + em-dash + immediate SYNOPSIS without blank line
        let ansi = "\x1b[1mName\x1b[0m\n     git-log \u{2014} Show commit logs\nSYNOPSIS\n     git log\n";
        let summary = parse_man_summary_in(ansi, alloc).expect("summary");
        assert_eq!(summary.as_str(), "Show commit logs");

        // En-dash separator
        let en = "NAME\n     rg \u{2013} recursively search the current directory for lines matching a pattern\n\nDESCRIPTION\n";
        let s2 = parse_man_summary_in(en, alloc).expect("en-dash summary");
        assert_eq!(
            s2.as_str(),
            "recursively search the current directory for lines matching a pattern"
        );

        // No dash separator returns full trimmed line
        let plain = "NAME\n     standalone summary line without dashes\n\nDESCRIPTION\n";
        let s3 = parse_man_summary_in(plain, alloc).expect("plain summary");
        assert_eq!(s3.as_str(), "standalone summary line without dashes");

        // Missing NAME section returns None
        assert!(parse_man_summary_in("SYNOPSIS\n     foo [bar]\n", alloc).is_none());

        // Long UTF-8 summary truncates cleanly to <= 220 bytes on a char boundary
        let mut long_page = String::from_str_in("NAME\n     tool - ", alloc);
        for _ in 0..80 {
            long_page.push_str("\u{00e9}abc"); // 5 bytes each -> 400 bytes
        }
        long_page.push_str("\n\nDESCRIPTION\n");
        let s4 = parse_man_summary_in(long_page.as_str(), alloc).expect("truncated");
        assert!(s4.as_str().len() <= 220);
        assert!(s4.as_str().len() >= 215);
    }
}
