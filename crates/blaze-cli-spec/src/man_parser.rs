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
    use super::*;

    #[test]
    fn parses_fish_and_man_no_std() {
        let alloc = Alloc::global();
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
}
