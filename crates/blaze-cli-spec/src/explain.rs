//! Pure `#![no_std]`, zero-unsafe, sans-IO command-line tokenizer and hover/tooltip explainer.
//!
//! Splits a shell command line into exact UTF-8 byte spans with semantic [`TokenRole`]
//! classification, and explains any token using parsed [`CliSpec`] flags/subcommands, `man`
//! page summaries, shell builtin summaries, and curated common-tool fallbacks.

use core::fmt::Write as _;

use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::{CliSpec, FlagSpec, SpecSource};

/// Semantic role of one token in a shell command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenRole {
    Command,
    Subcommand,
    Flag,
    FlagValue,
    Arg,
    Operator,
    Variable,
    Redir,
}

/// One tokenized span of a shell command line with exact UTF-8 byte offsets into `source`.
#[derive(Debug, PartialEq, Eq)]
pub struct CommandToken<'a> {
    /// Start byte offset in `source`.
    pub start: usize,
    /// Exclusive end byte offset in `source`.
    pub end: usize,
    /// Exact token text (`&source[start..end]`).
    pub text: AString<'a>,
    /// Classified semantic role of the token.
    pub role: TokenRole,
    /// Normalized command name for this pipeline stage (e.g. `"git"`, `"rg"`, `"cargo"`).
    pub cmd_base: AString<'a>,
    /// Active subcommand for this pipeline stage (e.g. `Some("log")` for `git log -n 4 --stat`).
    pub subcmd: Option<AString<'a>>,
    /// Preceding flag without `=` whose value this token may be (e.g. `Some("-n")`).
    pub for_flag: Option<AString<'a>>,
}

/// One structured detail row inside a [`TokenExplanation`].
#[derive(Debug, PartialEq, Eq)]
pub struct ExplainLine<'a> {
    pub tag: AString<'a>,
    pub text: AString<'a>,
}

/// Structured explanation for a single [`CommandToken`].
#[derive(Debug, PartialEq, Eq)]
pub struct TokenExplanation<'a> {
    pub badge: &'static str,
    pub title: AString<'a>,
    pub summary: AString<'a>,
    pub details: AVec<'a, ExplainLine<'a>>,
    pub source_note: AString<'a>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RawKind {
    Word,
    Operator,
    Redir,
}

struct RawSpan {
    start: usize,
    end: usize,
    kind: RawKind,
}

/// Scan `source` into [`CommandToken`]s preserving exact UTF-8 byte spans `(start, end)`.
///
/// Respects single quotes (`'...'`), double quotes (`"..."` with `\"` escapes), and backslash
/// escapes; splits pipeline/control operators (`|`, `&&`, `||`, `;`) and I/O redirections;
/// tracks `KEY=VAL` prefixes, wrapper commands (`sudo`, `env`, `time`), subcommand CLIs,
/// flags, and flag values.
pub fn tokenize_command_spans<'a>(source: &str, alloc: Alloc<'a>) -> AVec<'a, CommandToken<'a>> {
    let raw_spans = scan_raw_spans(source, alloc);
    let mut out = Vec::new_in(alloc);

    let mut stage_has_cmd = false;
    let mut in_wrapper = false;
    let mut wrapper_cmd: Option<&str> = None;
    let mut wrapper_prev_flag: Option<&str> = None;
    let mut cmd_base: &str = "";
    let mut subcmd: Option<&str> = None;
    let mut expect_subcmd = false;
    let mut after_double_dash = false;
    let mut prev_flag: Option<&str> = None;
    let mut prev_was_redir = false;

    for span in raw_spans.iter() {
        let raw = &source[span.start..span.end];
        match span.kind {
            RawKind::Operator => {
                out.push(CommandToken {
                    start: span.start,
                    end: span.end,
                    text: String::from_str_in(raw, alloc),
                    role: TokenRole::Operator,
                    cmd_base: String::new_in(alloc),
                    subcmd: None,
                    for_flag: None,
                });
                stage_has_cmd = false;
                in_wrapper = false;
                wrapper_cmd = None;
                wrapper_prev_flag = None;
                cmd_base = "";
                subcmd = None;
                expect_subcmd = false;
                after_double_dash = false;
                prev_flag = None;
                prev_was_redir = false;
            }
            RawKind::Redir => {
                out.push(CommandToken {
                    start: span.start,
                    end: span.end,
                    text: String::from_str_in(raw, alloc),
                    role: TokenRole::Redir,
                    cmd_base: String::from_str_in(cmd_base, alloc),
                    subcmd: subcmd.map(|s| String::from_str_in(s, alloc)),
                    for_flag: None,
                });
                prev_flag = None;
                prev_was_redir = raw != "2>&1";
            }
            RawKind::Word => {
                if prev_was_redir {
                    prev_was_redir = false;
                    let role = if raw.starts_with('$') {
                        TokenRole::Variable
                    } else {
                        TokenRole::Arg
                    };
                    out.push(CommandToken {
                        start: span.start,
                        end: span.end,
                        text: String::from_str_in(raw, alloc),
                        role,
                        cmd_base: String::from_str_in(cmd_base, alloc),
                        subcmd: subcmd.map(|s| String::from_str_in(s, alloc)),
                        for_flag: None,
                    });
                    continue;
                }

                if !stage_has_cmd {
                    if in_wrapper {
                        if let Some(wf) = wrapper_prev_flag.take()
                            && !raw.starts_with('-')
                        {
                            let base = wrapper_cmd.unwrap_or("");
                            out.push(CommandToken {
                                start: span.start,
                                end: span.end,
                                text: String::from_str_in(raw, alloc),
                                role: TokenRole::FlagValue,
                                cmd_base: String::from_str_in(base, alloc),
                                subcmd: None,
                                for_flag: Some(String::from_str_in(wf, alloc)),
                            });
                            continue;
                        }
                        if raw.starts_with('-') && raw.len() >= 2 && raw != "--" {
                            let base = wrapper_cmd.unwrap_or("");
                            if !raw.contains('=') && is_known_value_flag(raw) {
                                wrapper_prev_flag = Some(raw);
                            }
                            out.push(CommandToken {
                                start: span.start,
                                end: span.end,
                                text: String::from_str_in(raw, alloc),
                                role: TokenRole::Flag,
                                cmd_base: String::from_str_in(base, alloc),
                                subcmd: None,
                                for_flag: None,
                            });
                            continue;
                        }
                    }

                    if is_env_assignment(raw) {
                        out.push(CommandToken {
                            start: span.start,
                            end: span.end,
                            text: String::from_str_in(raw, alloc),
                            role: TokenRole::Arg,
                            cmd_base: String::new_in(alloc),
                            subcmd: None,
                            for_flag: None,
                        });
                        continue;
                    }

                    if raw.starts_with('$') {
                        stage_has_cmd = true;
                        in_wrapper = false;
                        cmd_base = raw;
                        out.push(CommandToken {
                            start: span.start,
                            end: span.end,
                            text: String::from_str_in(raw, alloc),
                            role: TokenRole::Variable,
                            cmd_base: String::from_str_in(raw, alloc),
                            subcmd: None,
                            for_flag: None,
                        });
                        continue;
                    }

                    let base = extract_cmd_base(raw);
                    if is_wrapper_cmd(base) {
                        in_wrapper = true;
                        wrapper_cmd = Some(base);
                        wrapper_prev_flag = None;
                        out.push(CommandToken {
                            start: span.start,
                            end: span.end,
                            text: String::from_str_in(raw, alloc),
                            role: TokenRole::Command,
                            cmd_base: String::from_str_in(base, alloc),
                            subcmd: None,
                            for_flag: None,
                        });
                        continue;
                    }

                    stage_has_cmd = true;
                    in_wrapper = false;
                    wrapper_cmd = None;
                    wrapper_prev_flag = None;
                    cmd_base = base;
                    expect_subcmd = is_subcommand_cli(base);
                    out.push(CommandToken {
                        start: span.start,
                        end: span.end,
                        text: String::from_str_in(raw, alloc),
                        role: TokenRole::Command,
                        cmd_base: String::from_str_in(base, alloc),
                        subcmd: None,
                        for_flag: None,
                    });
                    continue;
                }

                if !after_double_dash && raw == "--" {
                    after_double_dash = true;
                    prev_flag = None;
                    out.push(CommandToken {
                        start: span.start,
                        end: span.end,
                        text: String::from_str_in(raw, alloc),
                        role: TokenRole::Arg,
                        cmd_base: String::from_str_in(cmd_base, alloc),
                        subcmd: subcmd.map(|s| String::from_str_in(s, alloc)),
                        for_flag: None,
                    });
                    continue;
                }

                if !after_double_dash && raw.starts_with('-') && raw.len() >= 2 && raw != "--" {
                    prev_flag = if raw.contains('=') { None } else { Some(raw) };
                    out.push(CommandToken {
                        start: span.start,
                        end: span.end,
                        text: String::from_str_in(raw, alloc),
                        role: TokenRole::Flag,
                        cmd_base: String::from_str_in(cmd_base, alloc),
                        subcmd: subcmd.map(|s| String::from_str_in(s, alloc)),
                        for_flag: None,
                    });
                    continue;
                }

                if raw.starts_with('$') {
                    let attached = prev_flag.take();
                    out.push(CommandToken {
                        start: span.start,
                        end: span.end,
                        text: String::from_str_in(raw, alloc),
                        role: TokenRole::Variable,
                        cmd_base: String::from_str_in(cmd_base, alloc),
                        subcmd: subcmd.map(|s| String::from_str_in(s, alloc)),
                        for_flag: attached.map(|f| String::from_str_in(f, alloc)),
                    });
                    continue;
                }

                if expect_subcmd && !after_double_dash {
                    if let Some(pf) = prev_flag
                        && is_known_value_flag(pf)
                    {
                        prev_flag = None;
                        out.push(CommandToken {
                            start: span.start,
                            end: span.end,
                            text: String::from_str_in(raw, alloc),
                            role: TokenRole::FlagValue,
                            cmd_base: String::from_str_in(cmd_base, alloc),
                            subcmd: None,
                            for_flag: Some(String::from_str_in(pf, alloc)),
                        });
                        continue;
                    }
                    prev_flag = None;
                    expect_subcmd = false;
                    subcmd = Some(raw);
                    out.push(CommandToken {
                        start: span.start,
                        end: span.end,
                        text: String::from_str_in(raw, alloc),
                        role: TokenRole::Subcommand,
                        cmd_base: String::from_str_in(cmd_base, alloc),
                        subcmd: Some(String::from_str_in(raw, alloc)),
                        for_flag: None,
                    });
                    continue;
                }

                let attached = prev_flag.take();
                let role = if attached.is_some_and(is_known_value_flag) {
                    TokenRole::FlagValue
                } else {
                    TokenRole::Arg
                };
                out.push(CommandToken {
                    start: span.start,
                    end: span.end,
                    text: String::from_str_in(raw, alloc),
                    role,
                    cmd_base: String::from_str_in(cmd_base, alloc),
                    subcmd: subcmd.map(|s| String::from_str_in(s, alloc)),
                    for_flag: attached.map(|f| String::from_str_in(f, alloc)),
                });
            }
        }
    }

    out
}

fn scan_raw_spans<'a>(source: &str, alloc: Alloc<'a>) -> AVec<'a, RawSpan> {
    let mut spans = Vec::new_in(alloc);
    let len = source.len();
    let mut i = 0usize;

    while i < len {
        let rest = &source[i..];
        let Some(ch) = rest.chars().next() else {
            break;
        };
        if ch.is_whitespace() {
            i += ch.len_utf8();
            continue;
        }

        if let Some((op_len, kind)) = match_boundary_operator_or_redir(rest) {
            spans.push(RawSpan {
                start: i,
                end: i + op_len,
                kind,
            });
            i += op_len;
            continue;
        }

        let start = i;
        let mut quote: Option<char> = None;
        let mut escape = false;

        while i < len {
            let tail = &source[i..];
            let Some(c) = tail.chars().next() else {
                break;
            };
            if escape {
                escape = false;
                i += c.len_utf8();
                continue;
            }
            if let Some(q) = quote {
                if q == '"' && c == '\\' {
                    escape = true;
                } else if c == q {
                    quote = None;
                }
                i += c.len_utf8();
                continue;
            }
            if c == '\\' {
                escape = true;
                i += 1;
                continue;
            }
            if c == '\'' || c == '"' {
                quote = Some(c);
                i += 1;
                continue;
            }
            if c.is_whitespace() || is_midword_delimiter(tail) {
                break;
            }
            i += c.len_utf8();
        }

        if i > start {
            spans.push(RawSpan {
                start,
                end: i,
                kind: RawKind::Word,
            });
        }
    }

    spans
}

fn match_boundary_operator_or_redir(rest: &str) -> Option<(usize, RawKind)> {
    if rest.starts_with("&&") || rest.starts_with("||") {
        return Some((2, RawKind::Operator));
    }
    if rest.starts_with('|') || rest.starts_with(';') {
        return Some((1, RawKind::Operator));
    }
    if rest.starts_with("2>&1") {
        return Some((4, RawKind::Redir));
    }
    if rest.starts_with("2>>") || rest.starts_with("&>>") {
        return Some((3, RawKind::Redir));
    }
    if rest.starts_with("2>")
        || rest.starts_with("&>")
        || rest.starts_with(">>")
        || rest.starts_with("<<")
    {
        return Some((2, RawKind::Redir));
    }
    if rest.starts_with('>') || rest.starts_with('<') {
        return Some((1, RawKind::Redir));
    }
    None
}

fn is_midword_delimiter(tail: &str) -> bool {
    tail.starts_with('|')
        || tail.starts_with(';')
        || tail.starts_with('>')
        || tail.starts_with('<')
        || tail.starts_with("&&")
        || tail.starts_with("&>")
}

fn extract_cmd_base(raw: &str) -> &str {
    let unquoted = strip_outer_quotes(raw);
    let no_caret = unquoted.strip_prefix('^').unwrap_or(unquoted);
    let base = match no_caret.rfind('/') {
        Some(idx) if idx + 1 < no_caret.len() => &no_caret[idx + 1..],
        _ => no_caret,
    };
    base.strip_prefix('^').unwrap_or(base)
}

fn strip_outer_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn is_env_assignment(raw: &str) -> bool {
    let Some(eq) = raw.find('=') else {
        return false;
    };
    if eq == 0 {
        return false;
    }
    let name = &raw[..eq];
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_wrapper_cmd(cmd: &str) -> bool {
    matches!(cmd, "sudo" | "env" | "time")
}

fn is_subcommand_cli(cmd: &str) -> bool {
    matches!(
        cmd,
        "git"
            | "gh"
            | "cargo"
            | "blaze"
            | "docker"
            | "kubectl"
            | "npm"
            | "pnpm"
            | "bun"
            | "yarn"
            | "brew"
            | "rustup"
            | "jj"
            | "systemctl"
            | "tmux"
            | "ip"
            | "aws"
            | "gcloud"
            | "claude"
    )
}

fn is_known_value_flag(flag: &str) -> bool {
    known_value_flag_hint(flag).is_some()
}

fn known_value_flag_hint(flag: &str) -> Option<&'static str> {
    match flag {
        "-n" | "--max-count" | "--depth" => Some("<number>"),
        "-L" => Some("<range>"),
        "-C" => Some("<path>"),
        "-A" | "-B" => Some("<lines>"),
        "-m" => Some("<message>"),
        "-f" | "--file" => Some("<file>"),
        "-o" | "--output" => Some("<path>"),
        "-p" | "--package" => Some("<spec>"),
        "-t" | "--type" => Some("<type>"),
        "-g" | "--glob" => Some("<glob>"),
        "-e" => Some("<pattern>"),
        "-d" | "--data" => Some("<data>"),
        "-b" | "--branch" => Some("<branch>"),
        "-u" => Some("<user>"),
        "-H" | "--header" => Some("<header>"),
        "-X" | "--request" => Some("<method>"),
        "--format" => Some("<format>"),
        "--features" => Some("<features>"),
        "--bin" => Some("<name>"),
        "--cwd" => Some("<dir>"),
        "--cmd" => Some("<command>"),
        "--model" => Some("<model>"),
        "--effort" => Some("<level>"),
        "--target" => Some("<triple>"),
        "--profile" => Some("<profile>"),
        "--config" => Some("<path>"),
        _ => None,
    }
}

/// Explain a single [`CommandToken`] using optional [`CliSpec`], `man` page summary, and
/// builtin summary, allocating all output in `alloc`.
pub fn explain_token_in<'a>(
    tok: &CommandToken<'_>,
    spec: Option<&CliSpec<'_>>,
    man_summary: Option<&str>,
    builtin_summary: Option<&str>,
    alloc: Alloc<'a>,
) -> Option<TokenExplanation<'a>> {
    match tok.role {
        TokenRole::Operator => Some(explain_operator_in(tok, alloc)),
        TokenRole::Redir => Some(explain_redir_in(tok, alloc)),
        TokenRole::Variable => Some(explain_variable_in(tok, alloc)),
        TokenRole::Command => explain_command_in(tok, spec, man_summary, builtin_summary, alloc),
        TokenRole::Subcommand => explain_subcommand_in(tok, spec, man_summary, alloc),
        TokenRole::Flag => explain_flag_in(tok, spec, alloc),
        TokenRole::FlagValue => {
            let flag = tok.for_flag.as_ref()?.as_str();
            explain_flag_value_in(tok, flag, spec, true, alloc)
        }
        TokenRole::Arg => {
            let flag = tok.for_flag.as_ref()?.as_str();
            explain_flag_value_in(tok, flag, spec, false, alloc)
        }
    }
}

fn explain_operator_in<'a>(tok: &CommandToken<'_>, alloc: Alloc<'a>) -> TokenExplanation<'a> {
    let summary = match tok.text.as_str() {
        "|" => "Pipe structured value or stdout into the next command stage",
        "&&" => "Run the next command only if the previous command exits with status 0",
        "||" => "Run the next command only if the previous command exits with a non-zero status",
        ";" => "Run the next command sequentially regardless of the previous exit status",
        _ => "Shell command separator",
    };
    TokenExplanation {
        badge: "PIPE",
        title: String::from_str_in(tok.text.as_str(), alloc),
        summary: String::from_str_in(summary, alloc),
        details: Vec::new_in(alloc),
        source_note: String::from_str_in("shell syntax", alloc),
    }
}

fn explain_redir_in<'a>(tok: &CommandToken<'_>, alloc: Alloc<'a>) -> TokenExplanation<'a> {
    let summary = match tok.text.as_str() {
        ">" => "Redirect stdout to a file (overwrite)",
        ">>" => "Redirect stdout to a file (append)",
        "2>" => "Redirect stderr to a file (overwrite)",
        "2>>" => "Redirect stderr to a file (append)",
        "2>&1" => "Merge stderr into stdout",
        "&>" => "Redirect both stdout and stderr to a file (overwrite)",
        "&>>" => "Redirect both stdout and stderr to a file (append)",
        "<" => "Read stdin from a file",
        "<<" => "Read stdin from a here-document",
        _ => "Shell I/O redirection",
    };
    TokenExplanation {
        badge: "REDIR",
        title: String::from_str_in(tok.text.as_str(), alloc),
        summary: String::from_str_in(summary, alloc),
        details: Vec::new_in(alloc),
        source_note: String::from_str_in("shell syntax", alloc),
    }
}

fn explain_variable_in<'a>(tok: &CommandToken<'_>, alloc: Alloc<'a>) -> TokenExplanation<'a> {
    let raw = tok.text.as_str();
    let norm = raw
        .strip_prefix("${")
        .and_then(|s| s.strip_suffix('}'))
        .or_else(|| raw.strip_prefix('$'))
        .unwrap_or(raw);

    let summary = if norm == "env" || norm.starts_with("env.") {
        "Environment variables record (e.g. $env.PATH, $env.HOME)"
    } else {
        match norm {
            "in" => "Structured pipeline input value from the previous stage",
            "last" => "Result value from the previous command",
            "_" => "Last argument of the previous command",
            "HOME" => "Current user's home directory path",
            "PATH" => "Colon-separated executable search path",
            "PWD" => "Current working directory path",
            "USER" => "Current logged-in username",
            "SHELL" => "Path to the user's login shell",
            "?" => "Exit status code of the most recently executed command",
            "$" => "Process ID (PID) of the current shell",
            _ => "Shell variable expansion",
        }
    };

    TokenExplanation {
        badge: "VAR",
        title: String::from_str_in(raw, alloc),
        summary: String::from_str_in(summary, alloc),
        details: Vec::new_in(alloc),
        source_note: String::from_str_in("shell syntax", alloc),
    }
}

fn explain_command_in<'a>(
    tok: &CommandToken<'_>,
    spec: Option<&CliSpec<'_>>,
    man_summary: Option<&str>,
    builtin_summary: Option<&str>,
    alloc: Alloc<'a>,
) -> Option<TokenExplanation<'a>> {
    let cmd = if !tok.cmd_base.as_str().is_empty() {
        tok.cmd_base.as_str()
    } else {
        tok.text.as_str()
    };

    let mut details = Vec::new_in(alloc);

    if let Some(bs) = builtin_summary.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(sp) = spec.filter(|sp| !sp.is_empty()) {
            push_spec_count_detail(&mut details, sp, alloc);
        }
        return Some(TokenExplanation {
            badge: "BUILTIN",
            title: String::from_str_in(cmd, alloc),
            summary: String::from_str_in(bs, alloc),
            details,
            source_note: String::from_str_in("blaze-sh builtin", alloc),
        });
    }

    if let Some(ms) = man_summary.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(sp) = spec.filter(|sp| !sp.is_empty()) {
            push_spec_count_detail(&mut details, sp, alloc);
        }
        let mut note = String::from_str_in("man ", alloc);
        note.push_str(cmd);
        return Some(TokenExplanation {
            badge: "COMMAND",
            title: String::from_str_in(cmd, alloc),
            summary: String::from_str_in(ms, alloc),
            details,
            source_note: note,
        });
    }

    if let Some(curated) = curated_command_summary(cmd) {
        if let Some(sp) = spec.filter(|sp| !sp.is_empty()) {
            push_spec_count_detail(&mut details, sp, alloc);
        }
        let source_note = match spec {
            Some(sp) if !sp.is_empty() => spec_source_note(sp, tok, alloc),
            _ => String::from_str_in("curated spec", alloc),
        };
        return Some(TokenExplanation {
            badge: "COMMAND",
            title: String::from_str_in(cmd, alloc),
            summary: String::from_str_in(curated, alloc),
            details,
            source_note,
        });
    }

    if let Some(sp) = spec.filter(|sp| !sp.is_empty()) {
        let mut summary = String::new_in(alloc);
        match (sp.flags.len(), sp.subcommands.len()) {
            (f, 0) => {
                let _ = write!(summary, "Command with {f} flags");
            }
            (0, s) => {
                let _ = write!(summary, "Command with {s} subcommands");
            }
            (f, s) => {
                let _ = write!(summary, "Command with {f} flags and {s} subcommands");
            }
        }
        return Some(TokenExplanation {
            badge: "COMMAND",
            title: String::from_str_in(cmd, alloc),
            summary,
            details,
            source_note: spec_source_note(sp, tok, alloc),
        });
    }

    None
}

fn push_spec_count_detail<'a>(
    details: &mut AVec<'a, ExplainLine<'a>>,
    sp: &CliSpec<'_>,
    alloc: Alloc<'a>,
) {
    let mut text = String::new_in(alloc);
    match (sp.flags.len(), sp.subcommands.len()) {
        (f, 0) => {
            let _ = write!(text, "{f} flags available");
        }
        (0, s) => {
            let _ = write!(text, "{s} subcommands available");
        }
        (f, s) => {
            let _ = write!(text, "{f} flags, {s} subcommands available");
        }
    }
    details.push(ExplainLine {
        tag: String::from_str_in("spec", alloc),
        text,
    });
}

fn explain_subcommand_in<'a>(
    tok: &CommandToken<'_>,
    spec: Option<&CliSpec<'_>>,
    man_summary: Option<&str>,
    alloc: Alloc<'a>,
) -> Option<TokenExplanation<'a>> {
    let cmd = tok.cmd_base.as_str();
    let sub = tok.text.as_str();
    let mut title = String::new_in(alloc);
    if !cmd.is_empty() {
        title.push_str(cmd);
        title.push(' ');
    }
    title.push_str(sub);

    let spec_sub = spec.and_then(|sp| {
        sp.subcommands
            .iter()
            .find(|s| s.name.as_str() == sub)
    });

    if let Some(s) = spec_sub
        && !s.description.as_str().is_empty()
    {
        let mut details = Vec::new_in(alloc);
        if let Some(ms) = man_summary.map(str::trim).filter(|m| !m.is_empty())
            && ms != s.description.as_str()
        {
            details.push(ExplainLine {
                tag: String::from_str_in("man", alloc),
                text: String::from_str_in(ms, alloc),
            });
        }
        let source_note = spec
            .map(|sp| spec_source_note(sp, tok, alloc))
            .unwrap_or_else(|| String::from_str_in("curated spec", alloc));
        return Some(TokenExplanation {
            badge: "SUBCOMMAND",
            title,
            summary: String::from_str_in(s.description.as_str(), alloc),
            details,
            source_note,
        });
    }

    if let Some(ms) = man_summary.map(str::trim).filter(|m| !m.is_empty()) {
        let mut note = String::from_str_in("man ", alloc);
        if !cmd.is_empty() {
            note.push_str(cmd);
            note.push('-');
        }
        note.push_str(sub);
        return Some(TokenExplanation {
            badge: "SUBCOMMAND",
            title,
            summary: String::from_str_in(ms, alloc),
            details: Vec::new_in(alloc),
            source_note: note,
        });
    }

    if let Some(curated) = curated_subcommand_summary(cmd, sub) {
        return Some(TokenExplanation {
            badge: "SUBCOMMAND",
            title,
            summary: String::from_str_in(curated, alloc),
            details: Vec::new_in(alloc),
            source_note: String::from_str_in("curated spec", alloc),
        });
    }

    if let Some(sp) = spec
        && spec_sub.is_some()
    {
        let mut summary = String::from_str_in("Subcommand of ", alloc);
        summary.push_str(cmd);
        return Some(TokenExplanation {
            badge: "SUBCOMMAND",
            title,
            summary,
            details: Vec::new_in(alloc),
            source_note: spec_source_note(sp, tok, alloc),
        });
    }

    None
}

fn explain_flag_in<'a>(
    tok: &CommandToken<'_>,
    spec: Option<&CliSpec<'_>>,
    alloc: Alloc<'a>,
) -> Option<TokenExplanation<'a>> {
    let raw = tok.text.as_str();
    let (flag_part, inline_val) = split_flag_eq(raw);

    // 1. Exact flag lookup in `spec.flags` (handles --long, --long=val, -s, and single-dash long flags).
    if let Some(sp) = spec
        && let Some(f) = find_flag(sp, flag_part)
    {
        return Some(build_matched_flag_explanation(
            raw,
            flag_part,
            inline_val,
            f,
            sp,
            tok,
            alloc,
        ));
    }

    // 2. Combined short flags such as `-rf`, `-la`, `-xvf`.
    if inline_val.is_none()
        && flag_part.starts_with('-')
        && !flag_part.starts_with("--")
        && flag_part.len() > 2
        && let Some(sp) = spec
    {
        let mut details = Vec::new_in(alloc);
        let mut matched_count = 0usize;
        let mut total_count = 0usize;
        let mut short_buf = [0u8; 5];
        short_buf[0] = b'-';

        for ch in flag_part[1..].chars() {
            total_count += 1;
            let ch_len = ch.encode_utf8(&mut short_buf[1..]).len();
            let short_str = core::str::from_utf8(&short_buf[..1 + ch_len]).unwrap_or("");
            if let Some(f) = find_flag(sp, short_str) {
                matched_count += 1;
                let desc = if !f.description.as_str().is_empty() {
                    f.description.as_str()
                } else if let Some(l) = f.long.as_ref() {
                    l.as_str()
                } else if let Some(h) = f.value_hint.as_ref() {
                    h.as_str()
                } else {
                    "Option flag"
                };
                details.push(ExplainLine {
                    tag: String::from_str_in(short_str, alloc),
                    text: String::from_str_in(desc, alloc),
                });
            } else {
                details.push(ExplainLine {
                    tag: String::from_str_in(short_str, alloc),
                    text: String::from_str_in("unrecognized flag", alloc),
                });
            }
        }

        if matched_count > 0 {
            let mut summary = String::new_in(alloc);
            let _ = write!(summary, "Combined short flags ({total_count} options)");
            return Some(TokenExplanation {
                badge: "FLAG",
                title: String::from_str_in(raw, alloc),
                summary,
                details,
                source_note: spec_source_note(sp, tok, alloc),
            });
        }
    }

    // 3. Inline `--flag=value` fallback when no spec entry matched.
    if let Some(val) = inline_val {
        let mut summary = String::from_str_in("Option ", alloc);
        summary.push_str(flag_part);
        if let Some(hint) = known_value_flag_hint(flag_part) {
            summary.push(' ');
            summary.push_str(hint);
        }
        let mut details = Vec::new_in(alloc);
        details.push(ExplainLine {
            tag: String::from_str_in("value", alloc),
            text: String::from_str_in(val, alloc),
        });
        return Some(TokenExplanation {
            badge: "FLAG",
            title: String::from_str_in(raw, alloc),
            summary,
            details,
            source_note: String::from_str_in("command syntax", alloc),
        });
    }

    // 4. Universal convention fallback for --help / -h / --version.
    let universal = match flag_part {
        "--help" | "-h" => Some("Display help information"),
        "--version" => Some("Print version information"),
        _ => None,
    };
    if let Some(desc) = universal {
        return Some(TokenExplanation {
            badge: "FLAG",
            title: String::from_str_in(raw, alloc),
            summary: String::from_str_in(desc, alloc),
            details: Vec::new_in(alloc),
            source_note: String::from_str_in("command syntax", alloc),
        });
    }

    None
}

fn build_matched_flag_explanation<'a>(
    raw: &str,
    flag_part: &str,
    inline_val: Option<&str>,
    f: &FlagSpec<'_>,
    sp: &CliSpec<'_>,
    tok: &CommandToken<'_>,
    alloc: Alloc<'a>,
) -> TokenExplanation<'a> {
    let summary = if !f.description.as_str().is_empty() {
        String::from_str_in(f.description.as_str(), alloc)
    } else if let Some(h) = f.value_hint.as_ref() {
        let mut s = String::from_str_in("Takes ", alloc);
        s.push_str(h.as_str());
        s
    } else {
        let mut s = String::from_str_in("Option for ", alloc);
        s.push_str(tok.cmd_base.as_str());
        s
    };

    let mut details = Vec::new_in(alloc);
    if let Some(val) = inline_val {
        let mut text = String::from_str_in(val, alloc);
        if let Some(h) = f.value_hint.as_ref() {
            text.push_str(" (");
            text.push_str(h.as_str());
            text.push(')');
        }
        details.push(ExplainLine {
            tag: String::from_str_in("value", alloc),
            text,
        });
    } else if let Some(h) = f.value_hint.as_ref() {
        details.push(ExplainLine {
            tag: String::from_str_in("takes", alloc),
            text: String::from_str_in(h.as_str(), alloc),
        });
    }

    if let (Some(l), Some(s)) = (f.long.as_ref(), f.short.as_ref()) {
        let alias = if flag_part == l.as_str() {
            s.as_str()
        } else {
            l.as_str()
        };
        details.push(ExplainLine {
            tag: String::from_str_in("alias", alloc),
            text: String::from_str_in(alias, alloc),
        });
    }

    TokenExplanation {
        badge: "FLAG",
        title: String::from_str_in(raw, alloc),
        summary,
        details,
        source_note: spec_source_note(sp, tok, alloc),
    }
}

fn explain_flag_value_in<'a>(
    tok: &CommandToken<'_>,
    flag: &str,
    spec: Option<&CliSpec<'_>>,
    allow_known_fallback: bool,
    alloc: Alloc<'a>,
) -> Option<TokenExplanation<'a>> {
    let matched = spec.and_then(|sp| find_flag(sp, flag));
    let spec_takes_value = matched.is_some_and(|f| f.value_hint.is_some());
    if !spec_takes_value && !(allow_known_fallback && is_known_value_flag(flag)) {
        return None;
    }

    let hint = matched
        .and_then(|f| f.value_hint.as_ref().map(AString::as_str))
        .or_else(|| known_value_flag_hint(flag));

    let mut summary = String::from_str_in("Argument for ", alloc);
    summary.push_str(flag);
    if let Some(h) = hint {
        summary.push(' ');
        summary.push_str(h);
    }

    let mut details = Vec::new_in(alloc);
    if let Some(f) = matched
        && !f.description.as_str().is_empty()
    {
        details.push(ExplainLine {
            tag: String::from_str_in(flag, alloc),
            text: String::from_str_in(f.description.as_str(), alloc),
        });
    }

    let source_note = match (spec, matched) {
        (Some(sp), Some(_)) => spec_source_note(sp, tok, alloc),
        _ => String::from_str_in("command syntax", alloc),
    };

    Some(TokenExplanation {
        badge: "VALUE",
        title: String::from_str_in(tok.text.as_str(), alloc),
        summary,
        details,
        source_note,
    })
}

fn split_flag_eq(raw: &str) -> (&str, Option<&str>) {
    if let Some(eq) = raw.find('=')
        && eq >= 2
    {
        return (&raw[..eq], Some(&raw[eq + 1..]));
    }
    (raw, None)
}

fn find_flag<'a, 'b>(spec: &'b CliSpec<'a>, flag: &str) -> Option<&'b FlagSpec<'a>> {
    spec.flags.iter().find(|f| {
        f.long.as_ref().map(AString::as_str) == Some(flag)
            || f.short.as_ref().map(AString::as_str) == Some(flag)
    })
}

fn spec_source_note<'a>(spec: &CliSpec<'_>, tok: &CommandToken<'_>, alloc: Alloc<'a>) -> AString<'a> {
    let cmd = if !spec.command.as_str().is_empty() {
        spec.command.as_str()
    } else {
        tok.cmd_base.as_str()
    };
    let mut out = String::new_in(alloc);
    match spec.source {
        SpecSource::Man => {
            out.push_str("man ");
            for ch in cmd.chars() {
                out.push(if ch == ' ' { '-' } else { ch });
            }
        }
        SpecSource::Help => {
            out.push_str(cmd);
            out.push_str(" --help");
        }
        SpecSource::Fish => out.push_str("fish completions"),
        SpecSource::Manual => out.push_str("curated spec"),
        SpecSource::Merged => out.push_str("man / --help"),
    }
    out
}

fn curated_command_summary(cmd: &str) -> Option<&'static str> {
    match cmd {
        "git" => Some("Distributed version control system"),
        "gh" => Some("GitHub command-line tool"),
        "cargo" => Some("Rust package manager and build tool"),
        "blaze" => Some("Incremental Rust compiler, runner, tester, and linter"),
        "rg" => Some("Recursively search directories for a regex pattern (ripgrep)"),
        "fd" => Some("Find files and directories quickly"),
        "ls" => Some("List directory contents"),
        "cd" => Some("Change the current working directory"),
        "cp" => Some("Copy files and directories"),
        "mv" => Some("Move or rename files and directories"),
        "rm" => Some("Remove files or directories"),
        "mkdir" => Some("Create directories"),
        "cat" => Some("Concatenate and print files"),
        "head" => Some("Output the first part of files"),
        "tail" => Some("Output the last part of files"),
        "grep" => Some("Search for patterns in files or stdin"),
        "find" => Some("Search for files in a directory hierarchy"),
        "sed" => Some("Stream editor for filtering and transforming text"),
        "awk" => Some("Pattern scanning and text processing language"),
        "sort" => Some("Sort lines of text files"),
        "uniq" => Some("Report or omit repeated adjacent lines"),
        "wc" => Some("Count lines, words, and bytes"),
        "xargs" => Some("Build and execute command lines from stdin"),
        "curl" => Some("Transfer data from or to a server"),
        "wget" => Some("Non-interactive network downloader"),
        "tar" => Some("Archive and extract files"),
        "ssh" => Some("OpenSSH remote login client"),
        "scp" => Some("Secure remote file copy over SSH"),
        "rsync" => Some("Fast incremental file transfer and sync"),
        "docker" => Some("Build, run, and manage containers"),
        "kubectl" => Some("Control Kubernetes clusters"),
        "npm" => Some("Node.js package manager"),
        "pnpm" => Some("Fast, disk-space-efficient Node.js package manager"),
        "bun" => Some("JavaScript/TypeScript runtime, bundler, and package manager"),
        "yarn" => Some("JavaScript package manager"),
        "brew" => Some("Homebrew package manager for macOS and Linux"),
        "rustup" => Some("Rust toolchain installer and version manager"),
        "jj" => Some("Jujutsu Git-compatible version control system"),
        "systemctl" => Some("Control the systemd system and service manager"),
        "tmux" => Some("Terminal multiplexer"),
        "ip" => Some("Show and manipulate routing, network devices, and interfaces"),
        "aws" => Some("Amazon Web Services command-line interface"),
        "gcloud" => Some("Google Cloud command-line interface"),
        "claude" => Some("Anthropic Claude Code CLI agent"),
        "jq" => Some("Command-line JSON processor"),
        "sudo" => Some("Execute a command as another user (typically root)"),
        "env" => Some("Run a command in a modified environment"),
        "time" => Some("Measure execution time of a command"),
        "ps" => Some("Report a snapshot of running processes"),
        "kill" => Some("Send a signal to a process"),
        "chmod" => Some("Change file mode bits (permissions)"),
        "chown" => Some("Change file owner and group"),
        "ln" => Some("Create hard or symbolic links between files"),
        "touch" => Some("Create empty files or update timestamps"),
        "df" => Some("Report filesystem disk space usage"),
        "du" => Some("Estimate file and directory disk usage"),
        "echo" => Some("Write arguments to standard output"),
        "printf" => Some("Format and print data"),
        "man" => Some("Display system reference manuals"),
        "which" => Some("Locate an executable in PATH"),
        "sort-by" => Some("Sort a structured table or list by column"),
        "where" => Some("Filter rows matching a predicate"),
        "select" => Some("Project specific columns from a table"),
        _ => None,
    }
}

fn curated_subcommand_summary(cmd: &str, sub: &str) -> Option<&'static str> {
    match (cmd, sub) {
        ("git", "status") => Some("Show the working tree status"),
        ("git", "diff") => Some("Show changes between commits, index, and working tree"),
        ("git", "log") => Some("Show commit logs"),
        ("git", "add") => Some("Add file contents to the staging index"),
        ("git", "commit") => Some("Record changes to the repository"),
        ("git", "push") => Some("Update remote refs along with associated objects"),
        ("git", "pull") => Some("Fetch from and integrate with another repository"),
        ("git", "fetch") => Some("Download objects and refs from another repository"),
        ("git", "clone") => Some("Clone a repository into a new directory"),
        ("git", "checkout") => Some("Switch branches or restore working tree files"),
        ("git", "switch") => Some("Switch branches"),
        ("git", "branch") => Some("List, create, or delete branches"),
        ("git", "rebase") => Some("Reapply commits on top of another base tip"),
        ("git", "merge") => Some("Join two or more development histories together"),
        ("git", "stash") => Some("Stash changes in a dirty working directory"),
        ("git", "reset") => Some("Reset current HEAD to the specified state"),
        ("cargo", "build") => Some("Compile the current package"),
        ("cargo", "check") => Some("Analyze the current package without building binaries"),
        ("cargo", "test") => Some("Run unit and integration tests"),
        ("cargo", "run") => Some("Run a binary or example of the local package"),
        ("cargo", "clippy") => Some("Run Clippy lints on the current package"),
        ("blaze", "run") => Some("Compile and run a workspace binary"),
        ("blaze", "test") => Some("Run workspace tests"),
        ("blaze", "check") => Some("Type-check and lint workspace crates"),
        ("blaze", "export") => Some("Export an optimized native binary"),
        ("docker", "ps") => Some("List containers"),
        ("docker", "run") => Some("Create and run a new container from an image"),
        ("docker", "build") => Some("Build an image from a Dockerfile"),
        ("docker", "exec") => Some("Execute a command in a running container"),
        ("kubectl", "get") => Some("Display one or many Kubernetes resources"),
        ("kubectl", "apply") => Some("Apply a configuration to a resource"),
        ("kubectl", "logs") => Some("Print the logs for a container in a pod"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::{Erased, Inline};

    use super::*;
    use crate::SubcommandSpec;

    fn make_flag<'a>(
        long: Option<&str>,
        short: Option<&str>,
        value_hint: Option<&str>,
        desc: &str,
        alloc: Alloc<'a>,
    ) -> FlagSpec<'a> {
        FlagSpec {
            long: long.map(|s| String::from_str_in(s, alloc)),
            short: short.map(|s| String::from_str_in(s, alloc)),
            value_hint: value_hint.map(|s| String::from_str_in(s, alloc)),
            description: String::from_str_in(desc, alloc),
        }
    }

    #[test]
    fn tokenizes_and_explains_git_log_pipeline_on_stack_alloc() {
        let buf = Inline::<65536>::new();
        let erased = Erased::new(&buf);
        let alloc = erased.handle();

        let src = "git log -n 4 --stat";
        let toks = tokenize_command_spans(src, alloc);
        assert_eq!(toks.len(), 5);

        // Exact UTF-8 byte spans match source slices
        for t in toks.iter() {
            assert_eq!(&src[t.start..t.end], t.text.as_str());
        }

        assert_eq!(toks[0].role, TokenRole::Command);
        assert_eq!(toks[0].cmd_base.as_str(), "git");
        assert_eq!(toks[0].subcmd, None);

        assert_eq!(toks[1].role, TokenRole::Subcommand);
        assert_eq!(toks[1].cmd_base.as_str(), "git");
        assert_eq!(toks[1].subcmd.as_ref().map(AString::as_str), Some("log"));

        assert_eq!(toks[2].role, TokenRole::Flag);
        assert_eq!(toks[2].subcmd.as_ref().map(AString::as_str), Some("log"));

        assert_eq!(toks[3].role, TokenRole::FlagValue);
        assert_eq!(toks[3].text.as_str(), "4");
        assert_eq!(toks[3].subcmd.as_ref().map(AString::as_str), Some("log"));
        assert_eq!(toks[3].for_flag.as_ref().map(AString::as_str), Some("-n"));

        assert_eq!(toks[4].role, TokenRole::Flag);
        assert_eq!(toks[4].text.as_str(), "--stat");
        assert_eq!(toks[4].subcmd.as_ref().map(AString::as_str), Some("log"));

        // Build git-log man spec
        let mut spec = CliSpec::empty_in("git log", SpecSource::Man, alloc);
        spec.flags.push(make_flag(
            Some("--max-count"),
            Some("-n"),
            Some("<number>"),
            "Limit the number of commits to output",
            alloc,
        ));
        spec.flags.push(make_flag(
            Some("--stat"),
            None,
            None,
            "Generate a diffstat",
            alloc,
        ));

        // Subcommand explanation from man_summary
        let sub_exp = explain_token_in(&toks[1], None, Some("Show commit logs"), None, alloc)
            .expect("subcommand explanation");
        assert_eq!(sub_exp.badge, "SUBCOMMAND");
        assert_eq!(sub_exp.title.as_str(), "git log");
        assert_eq!(sub_exp.summary.as_str(), "Show commit logs");
        assert_eq!(sub_exp.source_note.as_str(), "man git-log");

        // FlagValue explanation for `4` after `-n`
        let val_exp = explain_token_in(&toks[3], Some(&spec), None, None, alloc)
            .expect("flag value explanation");
        assert_eq!(val_exp.badge, "VALUE");
        assert_eq!(val_exp.title.as_str(), "4");
        assert_eq!(val_exp.summary.as_str(), "Argument for -n <number>");
        assert_eq!(val_exp.details.len(), 1);
        assert_eq!(val_exp.details[0].tag.as_str(), "-n");
        assert_eq!(
            val_exp.details[0].text.as_str(),
            "Limit the number of commits to output"
        );
        assert_eq!(val_exp.source_note.as_str(), "man git-log");

        // Flag explanation for `--stat`
        let flag_exp = explain_token_in(&toks[4], Some(&spec), None, None, alloc)
            .expect("stat flag explanation");
        assert_eq!(flag_exp.badge, "FLAG");
        assert_eq!(flag_exp.title.as_str(), "--stat");
        assert_eq!(flag_exp.summary.as_str(), "Generate a diffstat");
        assert_eq!(flag_exp.source_note.as_str(), "man git-log");
    }

    #[test]
    fn explains_combined_short_flags_and_skips_plain_args() {
        let buf = Inline::<65536>::new();
        let erased = Erased::new(&buf);
        let alloc = erased.handle();

        let src = "rm -rf foo";
        let toks = tokenize_command_spans(src, alloc);
        assert_eq!(toks.len(), 3);
        assert_eq!(toks[0].role, TokenRole::Command);
        assert_eq!(toks[1].role, TokenRole::Flag);
        assert_eq!(toks[1].text.as_str(), "-rf");
        assert_eq!(toks[2].role, TokenRole::Arg);
        assert_eq!(toks[2].for_flag.as_ref().map(AString::as_str), Some("-rf"));

        let mut spec = CliSpec::empty_in("rm", SpecSource::Man, alloc);
        spec.flags.push(make_flag(
            Some("--recursive"),
            Some("-r"),
            None,
            "Remove directories and their contents recursively",
            alloc,
        ));
        spec.flags.push(make_flag(
            Some("--force"),
            Some("-f"),
            None,
            "Ignore nonexistent files and never prompt",
            alloc,
        ));

        let exp = explain_token_in(&toks[1], Some(&spec), None, None, alloc)
            .expect("combined flag explanation");
        assert_eq!(exp.badge, "FLAG");
        assert_eq!(exp.title.as_str(), "-rf");
        assert_eq!(exp.details.len(), 2);
        assert_eq!(exp.details[0].tag.as_str(), "-r");
        assert!(exp.details[0].text.as_str().contains("recursively"));
        assert_eq!(exp.details[1].tag.as_str(), "-f");
        assert!(exp.details[1].text.as_str().contains("never prompt"));
        assert_eq!(exp.source_note.as_str(), "man rm");

        // Plain filename `foo` must return None (no noisy tooltip)
        assert!(explain_token_in(&toks[2], Some(&spec), None, None, alloc).is_none());
    }

    #[test]
    fn explains_inline_eq_flag_and_spec_driven_arg_value() {
        let buf = Inline::<65536>::new();
        let erased = Erased::new(&buf);
        let alloc = erased.handle();

        let src = "git clone --depth=1 --origin upstream https://example.com/repo";
        let toks = tokenize_command_spans(src, alloc);
        assert_eq!(toks.len(), 6);
        assert_eq!(toks[2].role, TokenRole::Flag);
        assert_eq!(toks[2].text.as_str(), "--depth=1");
        // `--depth=1` contains `=`, so next token is `--origin` (Flag), not FlagValue.
        assert_eq!(toks[3].role, TokenRole::Flag);
        assert_eq!(toks[3].text.as_str(), "--origin");
        // `--origin` is not in the hardcoded list, so `upstream` is Arg with for_flag = Some("--origin")
        assert_eq!(toks[4].role, TokenRole::Arg);
        assert_eq!(toks[4].for_flag.as_ref().map(AString::as_str), Some("--origin"));
        // URL has no preceding flag
        assert_eq!(toks[5].role, TokenRole::Arg);
        assert_eq!(toks[5].for_flag, None);

        let mut spec = CliSpec::empty_in("git clone", SpecSource::Help, alloc);
        spec.flags.push(make_flag(
            Some("--depth"),
            None,
            Some("<depth>"),
            "Create a shallow clone with history truncated to <depth> commits",
            alloc,
        ));
        spec.flags.push(make_flag(
            Some("--origin"),
            Some("-o"),
            Some("<name>"),
            "Use <name> instead of origin to track upstream",
            alloc,
        ));

        let depth_exp = explain_token_in(&toks[2], Some(&spec), None, None, alloc)
            .expect("--depth=1 explanation");
        assert_eq!(depth_exp.badge, "FLAG");
        assert_eq!(depth_exp.title.as_str(), "--depth=1");
        assert!(depth_exp.summary.as_str().contains("shallow clone"));
        assert!(depth_exp.details.iter().any(|d| d.tag.as_str() == "value" && d.text.as_str() == "1 (<depth>)"));
        assert_eq!(depth_exp.source_note.as_str(), "git clone --help");

        // `upstream` has role Arg, but `spec` says `--origin` takes `<name>` -> explained as VALUE!
        let origin_val_exp = explain_token_in(&toks[4], Some(&spec), None, None, alloc)
            .expect("spec-driven arg value");
        assert_eq!(origin_val_exp.badge, "VALUE");
        assert_eq!(origin_val_exp.summary.as_str(), "Argument for --origin <name>");
        assert_eq!(origin_val_exp.details[0].tag.as_str(), "--origin");

        // Plain positional URL returns None
        assert!(explain_token_in(&toks[5], Some(&spec), None, None, alloc).is_none());
    }

    #[test]
    fn tokenizes_pipes_builtins_redirs_vars_wrappers_and_quotes() {
        let buf = Inline::<65536>::new();
        let erased = Erased::new(&buf);
        let alloc = erased.handle();

        let src = "ls -l | sort-by size -r";
        let toks = tokenize_command_spans(src, alloc);
        assert_eq!(toks.len(), 6);
        assert_eq!(toks[0].role, TokenRole::Command);
        assert_eq!(toks[0].cmd_base.as_str(), "ls");
        assert_eq!(toks[1].role, TokenRole::Flag);
        assert_eq!(toks[2].role, TokenRole::Operator);
        assert_eq!(toks[2].text.as_str(), "|");
        assert_eq!(toks[3].role, TokenRole::Command);
        assert_eq!(toks[3].cmd_base.as_str(), "sort-by");
        assert_eq!(toks[4].role, TokenRole::Arg);
        assert_eq!(toks[5].role, TokenRole::Flag);
        assert_eq!(toks[5].cmd_base.as_str(), "sort-by");

        let pipe_exp = explain_token_in(&toks[2], None, None, None, alloc).expect("pipe");
        assert_eq!(pipe_exp.badge, "PIPE");
        assert_eq!(pipe_exp.title.as_str(), "|");
        assert_eq!(pipe_exp.source_note.as_str(), "shell syntax");

        let builtin_exp = explain_token_in(
            &toks[3],
            None,
            None,
            Some("Sort structured table rows by column"),
            alloc,
        )
        .expect("builtin");
        assert_eq!(builtin_exp.badge, "BUILTIN");
        assert_eq!(builtin_exp.title.as_str(), "sort-by");
        assert_eq!(builtin_exp.source_note.as_str(), "blaze-sh builtin");

        // Env assignment, wrapper, path prefix, caret prefix, quotes, redir, variable
        let complex = r#"RUST_LOG=debug sudo /usr/bin/^git -C "/tmp/my \"repo\"" commit -m 'fix it' 2>&1 && echo $HOME"#;
        let ctoks = tokenize_command_spans(complex, alloc);
        for t in ctoks.iter() {
            assert_eq!(&complex[t.start..t.end], t.text.as_str());
        }
        assert_eq!(ctoks[0].role, TokenRole::Arg); // RUST_LOG=debug
        assert_eq!(ctoks[1].role, TokenRole::Command); // sudo
        assert_eq!(ctoks[1].cmd_base.as_str(), "sudo");
        assert_eq!(ctoks[2].role, TokenRole::Command); // /usr/bin/^git
        assert_eq!(ctoks[2].cmd_base.as_str(), "git");
        assert_eq!(ctoks[3].role, TokenRole::Flag); // -C
        assert_eq!(ctoks[4].role, TokenRole::FlagValue); // "/tmp/my \"repo\""
        assert_eq!(ctoks[4].for_flag.as_ref().map(AString::as_str), Some("-C"));
        assert_eq!(ctoks[5].role, TokenRole::Subcommand); // commit
        assert_eq!(ctoks[5].subcmd.as_ref().map(AString::as_str), Some("commit"));
        assert_eq!(ctoks[6].role, TokenRole::Flag); // -m
        assert_eq!(ctoks[7].role, TokenRole::FlagValue); // 'fix it'
        assert_eq!(ctoks[7].for_flag.as_ref().map(AString::as_str), Some("-m"));
        assert_eq!(ctoks[8].role, TokenRole::Redir); // 2>&1
        assert_eq!(ctoks[9].role, TokenRole::Operator); // &&
        assert_eq!(ctoks[10].role, TokenRole::Command); // echo
        assert_eq!(ctoks[11].role, TokenRole::Variable); // $HOME

        let redir_exp = explain_token_in(&ctoks[8], None, None, None, alloc).expect("redir");
        assert_eq!(redir_exp.badge, "REDIR");
        assert!(redir_exp.summary.as_str().contains("stderr"));

        let var_exp = explain_token_in(&ctoks[11], None, None, None, alloc).expect("var");
        assert_eq!(var_exp.badge, "VAR");
        assert!(var_exp.summary.as_str().contains("home directory"));
    }

    #[test]
    fn explains_subcommand_from_spec_and_command_from_spec_only() {
        let buf = Inline::<65536>::new();
        let erased = Erased::new(&buf);
        let alloc = erased.handle();

        let toks = tokenize_command_spans("cargo test", alloc);
        let mut spec = CliSpec::empty_in("cargo", SpecSource::Help, alloc);
        spec.subcommands.push(SubcommandSpec {
            name: String::from_str_in("test", alloc),
            description: String::from_str_in("Execute all unit and integration tests", alloc),
        });
        let sub_exp = explain_token_in(&toks[1], Some(&spec), None, None, alloc).expect("sub");
        assert_eq!(sub_exp.badge, "SUBCOMMAND");
        assert_eq!(sub_exp.title.as_str(), "cargo test");
        assert_eq!(
            sub_exp.summary.as_str(),
            "Execute all unit and integration tests"
        );
        assert_eq!(sub_exp.source_note.as_str(), "cargo --help");

        // Unknown command with a parsed spec falls back to flag/subcommand count summary
        let custom_toks = tokenize_command_spans("mytool --fast", alloc);
        let mut custom_spec = CliSpec::empty_in("mytool", SpecSource::Fish, alloc);
        custom_spec.flags.push(make_flag(
            Some("--fast"),
            None,
            None,
            "Enable fast mode",
            alloc,
        ));
        let cmd_exp =
            explain_token_in(&custom_toks[0], Some(&custom_spec), None, None, alloc).expect("cmd");
        assert_eq!(cmd_exp.badge, "COMMAND");
        assert_eq!(cmd_exp.summary.as_str(), "Command with 1 flags");
        assert_eq!(cmd_exp.source_note.as_str(), "fish completions");
    }

    #[test]
    fn handles_utf8_escapes_double_dash_wrapper_flags_and_priority() {
        let buf = Inline::<65536>::new();
        let erased = Erased::new(&buf);
        let alloc = erased.handle();

        // Multibyte UTF-8 + escaped space + operators + redirections + variables
        let src = r#"cat my\ café.txt > out.log || echo $in ; rm -- -file"#;
        let toks = tokenize_command_spans(src, alloc);
        for t in toks.iter() {
            assert_eq!(&src[t.start..t.end], t.text.as_str());
        }
        assert_eq!(toks[0].role, TokenRole::Command); // cat
        assert_eq!(toks[1].role, TokenRole::Arg); // my\ café.txt
        assert_eq!(toks[1].text.as_str(), r"my\ café.txt");
        assert_eq!(toks[2].role, TokenRole::Redir); // >
        assert_eq!(toks[3].role, TokenRole::Arg); // out.log
        assert_eq!(toks[4].role, TokenRole::Operator); // ||
        assert_eq!(toks[5].role, TokenRole::Command); // echo
        assert_eq!(toks[6].role, TokenRole::Variable); // $in
        assert_eq!(toks[7].role, TokenRole::Operator); // ;
        assert_eq!(toks[8].role, TokenRole::Command); // rm
        assert_eq!(toks[9].role, TokenRole::Arg); // --
        assert_eq!(toks[10].role, TokenRole::Arg); // -file (after --, not a flag!)

        // Wrapper flags before wrapped command
        let wtoks = tokenize_command_spans("sudo -u root ls -la", alloc);
        assert_eq!(wtoks[0].role, TokenRole::Command);
        assert_eq!(wtoks[0].cmd_base.as_str(), "sudo");
        assert_eq!(wtoks[1].role, TokenRole::Flag);
        assert_eq!(wtoks[1].cmd_base.as_str(), "sudo");
        assert_eq!(wtoks[2].role, TokenRole::FlagValue);
        assert_eq!(wtoks[2].cmd_base.as_str(), "sudo");
        assert_eq!(wtoks[2].for_flag.as_ref().map(AString::as_str), Some("-u"));
        assert_eq!(wtoks[3].role, TokenRole::Command);
        assert_eq!(wtoks[3].cmd_base.as_str(), "ls");
        assert_eq!(wtoks[4].role, TokenRole::Flag);
        assert_eq!(wtoks[4].cmd_base.as_str(), "ls");

        // Command precedence: builtin > man_summary > curated
        let e_builtin = explain_token_in(
            &wtoks[3],
            None,
            Some("man ls summary"),
            Some("builtin ls summary"),
            alloc,
        )
        .expect("builtin priority");
        assert_eq!(e_builtin.badge, "BUILTIN");
        assert_eq!(e_builtin.summary.as_str(), "builtin ls summary");
        assert_eq!(e_builtin.source_note.as_str(), "blaze-sh builtin");

        let e_man =
            explain_token_in(&wtoks[3], None, Some("man ls summary"), None, alloc).expect("man");
        assert_eq!(e_man.badge, "COMMAND");
        assert_eq!(e_man.summary.as_str(), "man ls summary");
        assert_eq!(e_man.source_note.as_str(), "man ls");

        let e_curated = explain_token_in(&wtoks[3], None, None, None, alloc).expect("curated");
        assert_eq!(e_curated.badge, "COMMAND");
        assert_eq!(e_curated.summary.as_str(), "List directory contents");

        // `||`, `;`, `>`, `$in` explanations
        assert!(explain_token_in(&toks[4], None, None, None, alloc)
            .expect("or")
            .summary
            .as_str()
            .contains("non-zero"));
        assert!(explain_token_in(&toks[7], None, None, None, alloc)
            .expect("semi")
            .summary
            .as_str()
            .contains("sequentially"));
        assert!(explain_token_in(&toks[2], None, None, None, alloc)
            .expect("gt")
            .summary
            .as_str()
            .contains("stdout"));
        assert!(explain_token_in(&toks[6], None, None, None, alloc)
            .expect("in")
            .summary
            .as_str()
            .contains("pipeline"));
    }
}
