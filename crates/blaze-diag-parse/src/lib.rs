//! Pure `#![no_std]`, zero-unsafe, sans-IO inline-Markdown parser and universal compiler/CLI
//! diagnostic line classifier built on `blaze-own` explicit allocators (`Alloc<'a>`, `AVec`,
//! `AString`).
//!
//! Covers:
//! - Parsing inline Markdown spans (`` `code` ``, ```` ``code`` ````, `**bold**`, `*italic*`,
//!   `[label](url)`) without panicking; unmatched delimiters stay literal text.
//! - Detecting source file paths and `file:line[:col]` references.
//! - Classifying uncolored compiler/linter/CLI diagnostic lines across `blaze check`, `rustc`,
//!   `clippy`, `rg`, `git`, panics, code frames, sub-notes (`= note:`, `= help:`, `fix:`), and
//!   summary lines.
//! - Detecting whether a batch of stdout lines contains structured diagnostics vs. plain output.
//! - Splitting optional trailing parenthetical source tags (e.g. `(engine_body_plan.rs)`).

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

use blaze_own::{AString, AVec, Alloc, String, Vec};

/// One inline Markdown token inside a diagnostic message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MdInline<'a> {
    Text(AString<'a>),
    Code(AString<'a>),
    Bold(AString<'a>),
    Italic(AString<'a>),
    Link {
        label: AString<'a>,
        url: AString<'a>,
    },
}

/// Parse inline Markdown (`` `code` ``, `**bold**`, `*italic*`, `[label](url)`) without ever
/// panicking; unmatched delimiters stay literal text.
pub fn parse_inline_markdown_in<'a>(input: &str, alloc: Alloc<'a>) -> AVec<'a, MdInline<'a>> {
    let mut out: AVec<'a, MdInline<'a>> = Vec::new_in(alloc);
    let bytes = input.as_bytes();
    let n = bytes.len();
    let mut i = 0usize;
    let mut plain: AString<'a> = String::new_in(alloc);

    let flush_plain = |plain: &mut AString<'a>, out: &mut AVec<'a, MdInline<'a>>| {
        if !plain.is_empty() {
            let taken = core::mem::replace(plain, String::new_in(alloc));
            out.push(MdInline::Text(taken));
        }
    };

    while i < n {
        // 1. Inline code: `` `...` `` or ```` ``...`` ````
        if bytes[i] == b'`' {
            let mut ticks = 0usize;
            while i + ticks < n && bytes[i + ticks] == b'`' {
                ticks += 1;
            }
            let content_start = i + ticks;
            let mut j = content_start;
            let mut found = None;
            while j + ticks <= n {
                if bytes[j] == b'`' {
                    let mut run = 0usize;
                    while j + run < n && bytes[j + run] == b'`' {
                        run += 1;
                    }
                    if run == ticks {
                        found = Some(j);
                        break;
                    }
                    j += run;
                    continue;
                }
                j += 1;
            }
            if let Some(end) = found
                && end > content_start
            {
                let code = input[content_start..end].trim();
                if !code.is_empty() {
                    flush_plain(&mut plain, &mut out);
                    out.push(MdInline::Code(String::from_str_in(code, alloc)));
                    i = end + ticks;
                    continue;
                }
            }
            for _ in 0..ticks {
                plain.push('`');
            }
            i += ticks;
            continue;
        }

        // 2. Bold: `**...**`
        if i + 1 < n && bytes[i] == b'*' && bytes[i + 1] == b'*' {
            if let Some(rel_end) = input[i + 2..].find("**")
                && rel_end > 0
            {
                let inner = &input[i + 2..i + 2 + rel_end];
                if !inner.starts_with(' ') && !inner.ends_with(' ') {
                    flush_plain(&mut plain, &mut out);
                    out.push(MdInline::Bold(String::from_str_in(inner, alloc)));
                    i = i + 2 + rel_end + 2;
                    continue;
                }
            }
            plain.push_str("**");
            i += 2;
            continue;
        }

        // 3. Italic: `*...*` (ignore spaced multiplication like `2 * 3`)
        if bytes[i] == b'*' && (i + 1 < n && bytes[i + 1] != b' ' && bytes[i + 1] != b'*') {
            if let Some(rel_end) = input[i + 1..].find('*')
                && rel_end > 0
            {
                let inner = &input[i + 1..i + 1 + rel_end];
                if !inner.ends_with(' ') {
                    flush_plain(&mut plain, &mut out);
                    out.push(MdInline::Italic(String::from_str_in(inner, alloc)));
                    i = i + 1 + rel_end + 1;
                    continue;
                }
            }
            plain.push('*');
            i += 1;
            continue;
        }

        // 4. Markdown link: `[label](url)`
        if bytes[i] == b'[' {
            if let Some(close_br) = input[i + 1..].find("](") {
                let label_start = i + 1;
                let label_end = label_start + close_br;
                let url_start = label_end + 2;
                if let Some(close_par) = input[url_start..].find(')')
                    && close_br > 0
                    && close_par > 0
                {
                    let label = &input[label_start..label_end];
                    let url = &input[url_start..url_start + close_par];
                    if !label.contains('\n') && !url.contains(char::is_whitespace) {
                        flush_plain(&mut plain, &mut out);
                        out.push(MdInline::Link {
                            label: String::from_str_in(label, alloc),
                            url: String::from_str_in(url, alloc),
                        });
                        i = url_start + close_par + 1;
                        continue;
                    }
                }
            }
            plain.push('[');
            i += 1;
            continue;
        }

        let Some(ch) = input[i..].chars().next() else {
            break;
        };
        plain.push(ch);
        i += ch.len_utf8();
    }

    flush_plain(&mut plain, &mut out);
    out
}

/// True when an inline code token looks like a source file path or `file:line:col` reference.
pub fn looks_like_file_path(s: &str) -> bool {
    if s.contains('/') && !s.starts_with("http") && s.len() >= 3 {
        return true;
    }
    let base = s.split(':').next().unwrap_or(s);
    matches!(
        base.rsplit('.').next(),
        Some(
            "rs" | "toml"
                | "json"
                | "md"
                | "ts"
                | "tsx"
                | "js"
                | "py"
                | "c"
                | "h"
                | "sh"
                | "nix"
                | "lock"
        )
    ) && base.contains('.')
}

/// Visual severity category for a diagnostic callout card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagSeverity {
    Error,
    Warning,
    Note,
    Help,
    /// Unstructured informational stderr on a zero-exit command (e.g. `git pull` ref updates).
    Info,
}

/// Parsed structure of one uncolored diagnostic output line (`stderr` or diagnostic `stdout`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassifiedLine<'a> {
    Header {
        severity: DiagSeverity,
        badge: &'a str,
        /// Optional short fix/diagnostic id (e.g. `u7eys` in `blaze check` output).
        id: Option<&'a str>,
        /// Optional inline source location (e.g. `Cargo.toml:69:1`).
        location: Option<&'a str>,
        body: &'a str,
    },
    /// Indented `= note:`, `= help:`, `fix:`, `note:`, `help:` sub-annotation inside an existing card.
    SubNote {
        severity: DiagSeverity,
        badge: &'a str,
        body: &'a str,
    },
    /// Compiler source arrow `--> path/to/file.rs:42:10`.
    Location(&'a str),
    /// Compiler code frame line (`42 | let x = ...` or `   | ^^^`).
    CodeFrame(&'a str),
    /// Build/check summary line (e.g. `0 errors, 1 warning, 1 fix: blaze check --fix u7eys`).
    Summary {
        has_errors: bool,
        summary: &'a str,
        command: Option<&'a str>,
    },
    /// Plain line (body continuation or unstructured stderr).
    Plain(&'a str),
}

fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len()
        && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn is_short_tool_prefix(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 24
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && s.bytes().any(|b| b.is_ascii_alphabetic())
}

fn is_blaze_diag_id(s: &str) -> bool {
    (3..=12).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && s.bytes().any(|b| b.is_ascii_alphabetic())
        && !s.contains('.')
        && !s.contains(':')
}

/// True when a token looks like `path/file.rs:42:1` or `Cargo.toml:69:1`.
fn looks_like_inline_location(tok: &str) -> bool {
    let mut parts = tok.split(':');
    let Some(file) = parts.next() else {
        return false;
    };
    let Some(line) = parts.next() else {
        return false;
    };
    if line.is_empty() || !line.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if let Some(col) = parts.next()
        && (col.is_empty() || !col.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    if parts.next().is_some() {
        return false;
    }
    looks_like_file_path(file)
}

fn parse_severity_token(tok: &str) -> Option<DiagSeverity> {
    if tok.eq_ignore_ascii_case("error")
        || (starts_with_ignore_ascii_case(tok, "error[") && tok.ends_with(']'))
    {
        Some(DiagSeverity::Error)
    } else if tok.eq_ignore_ascii_case("warning")
        || (starts_with_ignore_ascii_case(tok, "warning[") && tok.ends_with(']'))
    {
        Some(DiagSeverity::Warning)
    } else {
        None
    }
}

/// Parse blaze-style diagnostic header lines such as:
/// `u7eys  warning[unused-dependency]  Cargo.toml:69:1  dependency \`serde\` ...`
/// or `warning[unused-dependency]  Cargo.toml:69:1  dependency \`serde\` ...`
fn parse_blaze_diag_header(trimmed: &str) -> Option<ClassifiedLine<'_>> {
    let mut rest = trimmed;
    let first_end = rest.find(char::is_whitespace)?;
    let first = &rest[..first_end];
    rest = rest[first_end..].trim_start();
    if rest.is_empty() {
        return None;
    }

    let (id, severity, badge, after_sev) = if let Some(sev) = parse_severity_token(first) {
        // Must have bracketed code (e.g. `warning[foo]`) or inline location so we don't match prose!
        if !first.contains('[') {
            return None;
        }
        (None, sev, first, rest)
    } else if is_blaze_diag_id(first) {
        let sev_end = rest.find(char::is_whitespace)?;
        let sev_tok = &rest[..sev_end];
        let sev = parse_severity_token(sev_tok)?;
        let after = rest[sev_end..].trim_start();
        if after.is_empty() {
            return None;
        }
        (Some(first), sev, sev_tok, after)
    } else {
        return None;
    };

    // Optional inline `file:line:col` location token right after the severity badge.
    let (location, body) = if let Some(loc_end) = after_sev.find(char::is_whitespace) {
        let cand = &after_sev[..loc_end];
        if looks_like_inline_location(cand) {
            (Some(cand), after_sev[loc_end..].trim_start())
        } else {
            (None, after_sev)
        }
    } else if looks_like_inline_location(after_sev) {
        (Some(after_sev), "")
    } else {
        (None, after_sev)
    };

    Some(ClassifiedLine::Header {
        severity,
        badge,
        id,
        location,
        body,
    })
}

/// Parse a compact compiler/check summary line like:
/// `0 errors, 1 warning, 1 fix: blaze check --fix u7eys`
fn parse_summary_line(trimmed: &str) -> Option<ClassifiedLine<'_>> {
    let num_end = trimmed.find(|c: char| !c.is_ascii_digit())?;
    if num_end == 0 {
        return None;
    }
    let err_count = trimmed[..num_end].parse::<u32>().ok()?;
    let after_num = trimmed[num_end..].trim_start();
    if !(after_num.starts_with("error") && after_num.contains("warning")) {
        return None;
    }
    if let Some((left, right)) = trimmed.split_once(": ")
        && !right.trim().is_empty()
    {
        return Some(ClassifiedLine::Summary {
            has_errors: err_count > 0,
            summary: left.trim(),
            command: Some(right.trim()),
        });
    }
    Some(ClassifiedLine::Summary {
        has_errors: err_count > 0,
        summary: trimmed,
        command: None,
    })
}

fn looks_like_code_frame(raw: &str) -> bool {
    let trimmed = raw.trim_start();
    if trimmed.starts_with('|') || trimmed.starts_with("...") {
        return true;
    }
    if let Some((num, _)) = trimmed.split_once('|') {
        let n = num.trim();
        if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
            return true;
        }
    }
    false
}

/// Classify an uncolored `stderr` / diagnostic `stdout` line into a structured diagnostic element.
pub fn classify_line(raw: &str, exit_code: i32) -> ClassifiedLine<'_> {
    let trimmed = raw.trim_start();
    let indent = raw.len() - trimmed.len();

    if let Some(loc) = trimmed
        .strip_prefix("-->")
        .or_else(|| trimmed.strip_prefix(":::"))
    {
        let loc = loc.trim();
        if !loc.is_empty() {
            return ClassifiedLine::Location(loc);
        }
    }
    if let Some(rest) = trimmed.strip_prefix("= ") {
        if let Some(body) = rest.strip_prefix("note:") {
            return ClassifiedLine::SubNote {
                severity: DiagSeverity::Note,
                badge: "note",
                body: body.trim_start(),
            };
        }
        if let Some(body) = rest.strip_prefix("help:") {
            return ClassifiedLine::SubNote {
                severity: DiagSeverity::Help,
                badge: "help",
                body: body.trim_start(),
            };
        }
    }
    if indent > 0 {
        if let Some(body) = trimmed.strip_prefix("fix:") {
            return ClassifiedLine::SubNote {
                severity: DiagSeverity::Help,
                badge: "fix",
                body: body.trim_start(),
            };
        }
        if let Some(body) = trimmed
            .strip_prefix("help:")
            .or_else(|| trimmed.strip_prefix("hint:"))
        {
            return ClassifiedLine::SubNote {
                severity: DiagSeverity::Help,
                badge: "help",
                body: body.trim_start(),
            };
        }
        if let Some(body) = trimmed.strip_prefix("note:") {
            return ClassifiedLine::SubNote {
                severity: DiagSeverity::Note,
                badge: "note",
                body: body.trim_start(),
            };
        }
    }
    if looks_like_code_frame(raw) {
        return ClassifiedLine::CodeFrame(raw);
    }

    if indent == 0 {
        if let Some(summary) = parse_summary_line(trimmed) {
            return summary;
        }
        if let Some(blaze_hdr) = parse_blaze_diag_header(trimmed) {
            return blaze_hdr;
        }
        // Explicit `error[E0308]: ...`, `error: ...`, `fatal: ...`
        if let Some(colon) = trimmed.find(':') {
            let head = &trimmed[..colon];
            let body = trimmed[colon + 1..].trim_start();
            if !body.is_empty() {
                if head.eq_ignore_ascii_case("error")
                    || starts_with_ignore_ascii_case(head, "error[")
                    || head.eq_ignore_ascii_case("fatal")
                    || head.eq_ignore_ascii_case("panic")
                {
                    return ClassifiedLine::Header {
                        severity: DiagSeverity::Error,
                        badge: head,
                        id: None,
                        location: None,
                        body,
                    };
                }
                if head.eq_ignore_ascii_case("warning")
                    || starts_with_ignore_ascii_case(head, "warning[")
                    || head.eq_ignore_ascii_case("warn")
                {
                    return ClassifiedLine::Header {
                        severity: DiagSeverity::Warning,
                        badge: head,
                        id: None,
                        location: None,
                        body,
                    };
                }
                if head.eq_ignore_ascii_case("note") {
                    return ClassifiedLine::Header {
                        severity: DiagSeverity::Note,
                        badge: head,
                        id: None,
                        location: None,
                        body,
                    };
                }
                if head.eq_ignore_ascii_case("help")
                    || head.eq_ignore_ascii_case("hint")
                    || head.eq_ignore_ascii_case("suggestion")
                {
                    return ClassifiedLine::Header {
                        severity: DiagSeverity::Help,
                        badge: head,
                        id: None,
                        location: None,
                        body,
                    };
                }
                // CLI tool prefix like `rg: ripgrep requires ...` or `git: ...` or `blaze: ...`
                if is_short_tool_prefix(head) && exit_code != 0 {
                    return ClassifiedLine::Header {
                        severity: DiagSeverity::Error,
                        badge: head,
                        id: None,
                        location: None,
                        body,
                    };
                }
            }
        }
        if trimmed.starts_with("thread '") && trimmed.contains("panicked") {
            return ClassifiedLine::Header {
                severity: DiagSeverity::Error,
                badge: "panic",
                id: None,
                location: None,
                body: trimmed,
            };
        }
    }

    ClassifiedLine::Plain(raw)
}

/// True when a group of uncolored `stdout` lines contains structured compiler/linter diagnostics
/// (e.g. `blaze check`, `rustc`, `clippy`) and should be rendered with rich diagnostic cards.
pub fn looks_like_diagnostic_output(lines: &[(i64, &str)], exit_code: i32) -> bool {
    lines.iter().any(|&(_, raw)| {
        if raw.trim().is_empty() {
            return false;
        }
        matches!(
            classify_line(raw, exit_code),
            ClassifiedLine::Header { .. } | ClassifiedLine::Location(_)
        )
    })
}

/// Split an optional trailing parenthetical source hint like `(engine_body_plan.rs)` off the end
/// of a diagnostic message so it renders cleanly as a quiet muted tag.
pub fn split_trailing_source_tag(body: &str) -> (&str, Option<&str>) {
    let trimmed = body.trim_end();
    if let Some(open) = trimmed.rfind(" (")
        && trimmed.ends_with(')')
    {
        let inner = &trimmed[open + 2..trimmed.len() - 1];
        if !inner.is_empty() && !inner.contains(' ') && looks_like_file_path(inner) {
            return (trimmed[..open].trim_end(), Some(inner));
        }
    }
    (body, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text<'a>(s: &str, alloc: Alloc<'a>) -> MdInline<'a> {
        MdInline::Text(String::from_str_in(s, alloc))
    }

    fn code<'a>(s: &str, alloc: Alloc<'a>) -> MdInline<'a> {
        MdInline::Code(String::from_str_in(s, alloc))
    }

    fn bold<'a>(s: &str, alloc: Alloc<'a>) -> MdInline<'a> {
        MdInline::Bold(String::from_str_in(s, alloc))
    }

    fn italic<'a>(s: &str, alloc: Alloc<'a>) -> MdInline<'a> {
        MdInline::Italic(String::from_str_in(s, alloc))
    }

    fn link<'a>(label: &str, url: &str, alloc: Alloc<'a>) -> MdInline<'a> {
        MdInline::Link {
            label: String::from_str_in(label, alloc),
            url: String::from_str_in(url, alloc),
        }
    }

    #[test]
    fn parse_inline_markdown_handles_code_bold_italic_links_and_math() {
        let alloc = Alloc::global();
        let spans = parse_inline_markdown_in(
            "check `crates/globset/src/serde_impl.rs` and `#[cfg]` with **bold**, *italic*, [docs](https://ix.dev), and 2 * 3",
            alloc,
        );
        assert_eq!(
            spans.as_slice(),
            &[
                text("check ", alloc),
                code("crates/globset/src/serde_impl.rs", alloc),
                text(" and ", alloc),
                code("#[cfg]", alloc),
                text(" with ", alloc),
                bold("bold", alloc),
                text(", ", alloc),
                italic("italic", alloc),
                text(", ", alloc),
                link("docs", "https://ix.dev", alloc),
                text(", and 2 * 3", alloc),
            ]
        );
    }

    #[test]
    fn parse_inline_markdown_handles_double_backticks_utf8_and_unmatched_delimiters() {
        let alloc = Alloc::global();
        let spans = parse_inline_markdown_in(
            "use `` `foo` ``, ** not bold **, *unclosed, [bad](a b), and café `src/lib.rs`",
            alloc,
        );
        assert_eq!(
            spans.as_slice(),
            &[
                text("use ", alloc),
                code("`foo`", alloc),
                text(", ** not bold **, *unclosed, [bad](a b), and café ", alloc),
                code("src/lib.rs", alloc),
            ]
        );

        let empty = parse_inline_markdown_in("", alloc);
        assert!(empty.is_empty());

        let unclosed_ticks = parse_inline_markdown_in("raw `` ticks", alloc);
        assert_eq!(unclosed_ticks.as_slice(), &[text("raw `` ticks", alloc)]);
    }

    #[test]
    fn looks_like_file_path_recognizes_paths_and_extensions() {
        assert!(looks_like_file_path("crates/blaze-shell/src/main.rs"));
        assert!(looks_like_file_path("Cargo.toml"));
        assert!(looks_like_file_path("Cargo.toml:69:1"));
        assert!(looks_like_file_path("engine_body_plan.rs"));
        assert!(looks_like_file_path("flake.nix"));
        assert!(!looks_like_file_path("https://ix.dev/blaze"));
        assert!(!looks_like_file_path("serde"));
        assert!(!looks_like_file_path("rs"));
        assert!(!looks_like_file_path("a/"));
    }

    #[test]
    fn classify_blaze_check_diagnostics_with_id_location_fix_and_summary() {
        assert_eq!(
            classify_line(
                "u7eys  warning[unused-dependency]  Cargo.toml:69:1  dependency `serde` is declared",
                0,
            ),
            ClassifiedLine::Header {
                severity: DiagSeverity::Warning,
                badge: "warning[unused-dependency]",
                id: Some("u7eys"),
                location: Some("Cargo.toml:69:1"),
                body: "dependency `serde` is declared",
            }
        );
        assert_eq!(
            classify_line(
                "warning[unused-dependency]  crates/foo/Cargo.toml:12:1  unused dep",
                0,
            ),
            ClassifiedLine::Header {
                severity: DiagSeverity::Warning,
                badge: "warning[unused-dependency]",
                id: None,
                location: Some("crates/foo/Cargo.toml:12:1"),
                body: "unused dep",
            }
        );
        assert_eq!(
            classify_line(
                "  fix: delete `serde = \"1.0.77\"` (69:1-70:1): remove unused dependency `serde`",
                0,
            ),
            ClassifiedLine::SubNote {
                severity: DiagSeverity::Help,
                badge: "fix",
                body: "delete `serde = \"1.0.77\"` (69:1-70:1): remove unused dependency `serde`",
            }
        );
        assert_eq!(
            classify_line("0 errors, 1 warning, 1 fix: blaze check --fix u7eys", 0),
            ClassifiedLine::Summary {
                has_errors: false,
                summary: "0 errors, 1 warning, 1 fix",
                command: Some("blaze check --fix u7eys"),
            }
        );
        assert_eq!(
            classify_line("2 errors, 0 warnings", 1),
            ClassifiedLine::Summary {
                has_errors: true,
                summary: "2 errors, 0 warnings",
                command: None,
            }
        );
    }

    #[test]
    fn classify_rustc_and_clippy_diagnostics() {
        assert_eq!(
            classify_line("error[E0308]: mismatched types", 1),
            ClassifiedLine::Header {
                severity: DiagSeverity::Error,
                badge: "error[E0308]",
                id: None,
                location: None,
                body: "mismatched types",
            }
        );
        assert_eq!(
            classify_line("warning: unused variable: `x`", 0),
            ClassifiedLine::Header {
                severity: DiagSeverity::Warning,
                badge: "warning",
                id: None,
                location: None,
                body: "unused variable: `x`",
            }
        );
        assert_eq!(
            classify_line("  --> src/main.rs:42:10", 1),
            ClassifiedLine::Location("src/main.rs:42:10")
        );
        assert_eq!(
            classify_line("  ::: crates/foo/src/lib.rs:10:5", 1),
            ClassifiedLine::Location("crates/foo/src/lib.rs:10:5")
        );
        assert_eq!(
            classify_line("42 | let x: u32 = \"hi\";", 1),
            ClassifiedLine::CodeFrame("42 | let x: u32 = \"hi\";")
        );
        assert_eq!(
            classify_line("   |            ^^^^ expected `u32`, found `&str`", 1),
            ClassifiedLine::CodeFrame("   |            ^^^^ expected `u32`, found `&str`")
        );
        assert_eq!(
            classify_line("   = note: `#[warn(clippy::needless_return)]` on by default", 0),
            ClassifiedLine::SubNote {
                severity: DiagSeverity::Note,
                badge: "note",
                body: "`#[warn(clippy::needless_return)]` on by default",
            }
        );
        assert_eq!(
            classify_line("   = help: for further information visit https://rust-lang.github.io/rust-clippy/", 0),
            ClassifiedLine::SubNote {
                severity: DiagSeverity::Help,
                badge: "help",
                body: "for further information visit https://rust-lang.github.io/rust-clippy/",
            }
        );
        assert_eq!(
            classify_line("thread 'main' panicked at src/lib.rs:10:5:", 101),
            ClassifiedLine::Header {
                severity: DiagSeverity::Error,
                badge: "panic",
                id: None,
                location: None,
                body: "thread 'main' panicked at src/lib.rs:10:5:",
            }
        );
    }

    #[test]
    fn classify_rg_and_git_cli_diagnostics() {
        assert_eq!(
            classify_line("rg: ripgrep requires at least one pattern", 2),
            ClassifiedLine::Header {
                severity: DiagSeverity::Error,
                badge: "rg",
                id: None,
                location: None,
                body: "ripgrep requires at least one pattern",
            }
        );
        assert_eq!(
            classify_line("fatal: not a git repository (or any of the parent directories): .git", 128),
            ClassifiedLine::Header {
                severity: DiagSeverity::Error,
                badge: "fatal",
                id: None,
                location: None,
                body: "not a git repository (or any of the parent directories): .git",
            }
        );
        assert_eq!(
            classify_line("hint: Using 'master' as the name for the initial branch.", 0),
            ClassifiedLine::Header {
                severity: DiagSeverity::Help,
                badge: "hint",
                id: None,
                location: None,
                body: "Using 'master' as the name for the initial branch.",
            }
        );
        // Zero-exit tool-like prefix should remain Plain so informational output isn't misclassified as Error.
        assert_eq!(
            classify_line("From: https://github.com/indexable-inc/blaze", 0),
            ClassifiedLine::Plain("From: https://github.com/indexable-inc/blaze")
        );
        assert_eq!(
            classify_line("git: 'stauts' is not a git command. See 'git --help'.", 1),
            ClassifiedLine::Header {
                severity: DiagSeverity::Error,
                badge: "git",
                id: None,
                location: None,
                body: "'stauts' is not a git command. See 'git --help'.",
            }
        );
    }

    #[test]
    fn split_trailing_source_tag_splits_only_trailing_file_paths() {
        assert_eq!(
            split_trailing_source_tag(
                "no unit checks this file when a `#[cfg]` is false (engine_body_plan.rs)"
            ),
            (
                "no unit checks this file when a `#[cfg]` is false",
                Some("engine_body_plan.rs"),
            )
        );
        assert_eq!(
            split_trailing_source_tag("see details (crates/blaze-cli/src/main.rs)  "),
            ("see details", Some("crates/blaze-cli/src/main.rs"))
        );
        assert_eq!(
            split_trailing_source_tag("not a file tag (plain prose here)"),
            ("not a file tag (plain prose here)", None)
        );
        assert_eq!(
            split_trailing_source_tag("no parenthetical at all"),
            ("no parenthetical at all", None)
        );
    }

    #[test]
    fn looks_like_diagnostic_output_accepts_diagnostics_and_rejects_plain_stdout() {
        let blaze_stdout = [
            (
                1i64,
                "note: `crates/globset/src/serde_impl.rs`: no unit checks this file",
            ),
            (
                2i64,
                "u7eys  warning[unused-dependency]  Cargo.toml:69:1  dependency `serde` is declared",
            ),
            (3i64, "  fix: delete `serde = \"1.0.77\"`"),
            (4i64, "0 errors, 1 warning, 1 fix: blaze check --fix u7eys"),
        ];
        assert!(looks_like_diagnostic_output(&blaze_stdout, 0));

        let rustc_loc_only = [(1i64, "  --> src/lib.rs:12:5")];
        assert!(looks_like_diagnostic_output(&rustc_loc_only, 1));

        let plain_stdout = [
            (1i64, ""),
            (2i64, "hello world"),
            (3i64, "total 42"),
            (4i64, "drwxr-xr-x  12 user staff 384 Nov 26 12:00 crates"),
            (5i64, "From https://github.com/indexable-inc/blaze"),
            (6i64, " - [deleted]         (none)     -> origin/feat/demo"),
        ];
        assert!(!looks_like_diagnostic_output(&plain_stdout, 0));
    }
}
