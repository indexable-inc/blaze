//! Rich structured diagnostic & inline-Markdown renderer for external command `stderr` and
//! `blaze-sh` [`ShellError`] panels.
//!
//! Turns walls of raw stderr text into readable, scannable diagnostic cards:
//! - Classifies `error:` / `error[E0xxx]:` / `fatal:`, `warning:`, `note:`, `help:`, `<tool>: ...`,
//!   source locations (`--> path:line:col`), and compiler code frames (`42 | ...`, `   | ^^^`).
//! - Parses inline Markdown spans (`` `code` ``, `**bold**`, `*italic*`, `[label](url)`) and flows
//!   them with proper word-boundary wrapping (zero mid-word breaks, zero leading spaces on wrapped
//!   lines), rendering backticked identifiers & file paths as crisp clickable monospace chips.

use crate::theme;
use crate::view::{CellMetrics, HitAction, HitRect, HoverKind, Scene, TextFace};

/// One inline Markdown token inside a diagnostic message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MdInline {
    Text(String),
    Code(String),
    Bold(String),
    Italic(String),
    Link { label: String, url: String },
}

/// Parse inline Markdown (`` `code` ``, `**bold**`, `*italic*`, `[label](url)`) without ever
/// panicking; unmatched delimiters stay literal text.
pub fn parse_inline_markdown(input: &str) -> Vec<MdInline> {
    let mut out: Vec<MdInline> = Vec::new();
    let bytes = input.as_bytes();
    let n = bytes.len();
    let mut i = 0usize;
    let mut plain = String::new();

    let flush_plain = |plain: &mut String, out: &mut Vec<MdInline>| {
        if !plain.is_empty() {
            out.push(MdInline::Text(std::mem::take(plain)));
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
                    out.push(MdInline::Code(code.to_owned()));
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
                    out.push(MdInline::Bold(inner.to_owned()));
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
                    out.push(MdInline::Italic(inner.to_owned()));
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
                            label: label.to_owned(),
                            url: url.to_owned(),
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

        let ch = input[i..].chars().next().unwrap();
        plain.push(ch);
        i += ch.len_utf8();
    }

    flush_plain(&mut plain, &mut out);
    out
}

/// True when an inline code token looks like a source file path or `file:line:col` reference.
fn looks_like_file_path(s: &str) -> bool {
    if s.contains('/') && !s.starts_with("http") && s.len() >= 3 {
        return true;
    }
    let base = s.split(':').next().unwrap_or(s);
    matches!(
        base.rsplit('.').next(),
        Some("rs" | "toml" | "json" | "md" | "ts" | "tsx" | "js" | "py" | "c" | "h" | "sh" | "nix" | "lock")
    ) && base.contains('.')
}

/// Flow and render inline-Markdown prose + monospace `` `code` `` chips with clean word-boundary
/// wrapping. Returns the vertical height consumed in logical pixels.
#[allow(clippy::too_many_arguments)]
pub fn layout_inline_markdown(
    scene: &mut Scene,
    text: &str,
    x0: f32,
    y0: f32,
    max_w: f32,
    first_line_indent: f32,
    first_line_right_reserve: f32,
    base_color: [f32; 4],
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    let spans = parse_inline_markdown(text);
    if spans.is_empty() {
        return m.ch;
    }

    enum Atom {
        Word { text: String, color: [f32; 4], bold: bool, italic: bool, underline: bool, copy: Option<String> },
        CodeChip { text: String, is_path: bool },
        Space,
    }

    let mut atoms: Vec<Atom> = Vec::new();
    for sp in spans {
        match sp {
            MdInline::Text(t) => push_prose_words(&t, base_color, false, false, false, None, &mut atoms),
            MdInline::Bold(t) => push_prose_words(&t, theme::INK_1, true, false, false, None, &mut atoms),
            MdInline::Italic(t) => push_prose_words(&t, base_color, false, true, false, None, &mut atoms),
            MdInline::Link { label, url } => {
                push_prose_words(&label, theme::SERIES_BLUE, false, false, true, Some(url), &mut atoms);
            }
            MdInline::Code(code) => {
                let is_path = looks_like_file_path(&code);
                atoms.push(Atom::CodeChip { text: code, is_path });
            }
        }
    }

    fn push_prose_words(
        s: &str,
        color: [f32; 4],
        bold: bool,
        italic: bool,
        underline: bool,
        copy: Option<String>,
        out: &mut Vec<Atom>,
    ) {
        let mut cur = String::new();
        for ch in s.chars() {
            if ch.is_whitespace() {
                if !cur.is_empty() {
                    out.push(Atom::Word {
                        text: std::mem::take(&mut cur),
                        color,
                        bold,
                        italic,
                        underline,
                        copy: copy.clone(),
                    });
                }
                if !matches!(out.last(), Some(Atom::Space)) {
                    out.push(Atom::Space);
                }
            } else {
                cur.push(ch);
            }
        }
        if !cur.is_empty() {
            out.push(Atom::Word {
                text: cur,
                color,
                bold,
                italic,
                underline,
                copy,
            });
        }
    }

    let line_h = m.ch + 4.0;
    let space_w = m.cw;
    let chip_pad_x = 4.5f32;

    let mut line_idx = 0usize;
    let mut cur_x = x0 + first_line_indent;
    let mut cur_y = y0;
    let line_limit = |idx: usize| -> f32 {
        let reserve = if idx == 0 { first_line_right_reserve } else { 0.0 };
        (x0 + (max_w - reserve).max(80.0)).max(x0 + first_line_indent + 40.0)
    };
    let mut at_line_start = first_line_indent <= 0.5;

    for atom in atoms {
        match atom {
            Atom::Space => {
                if !at_line_start {
                    cur_x += space_w;
                }
            }
            Atom::CodeChip { text, is_path } => {
                let avail_full = (max_w - 8.0).max(60.0);
                let max_chars = ((avail_full - chip_pad_x * 2.0) / m.cw).floor().max(6.0) as usize;
                let shown = if text.chars().count() > max_chars {
                    crate::view::truncate_middle(&text, max_chars)
                } else {
                    text.clone()
                };
                let text_w = shown.chars().count() as f32 * m.cw;
                let chip_w = text_w + chip_pad_x * 2.0;

                if !at_line_start && cur_x + chip_w > line_limit(line_idx) {
                    line_idx += 1;
                    cur_y += line_h;
                    cur_x = x0;
                }
                let chip_bg = if is_path {
                    theme::rgba(0x58, 0x9d, 0xf6, 0.12)
                } else {
                    theme::BG_CHIP
                };
                let chip_border = if is_path {
                    theme::rgba(0x58, 0x9d, 0xf6, 0.34)
                } else {
                    theme::LINE
                };
                let chip_fg = if is_path { theme::SERIES_BLUE } else { theme::INK_1 };
                scene.round_rect(
                    cur_x,
                    cur_y - 1.0,
                    cur_x + chip_w,
                    cur_y + m.ch + 1.0,
                    4.0,
                    chip_bg,
                    1.0,
                    chip_border,
                    clip_id,
                );
                scene.text(cur_x + chip_pad_x, cur_y, &shown, chip_fg, false, false, clip_id);
                let hit = HitRect::new(cur_x, cur_y - 1.0, cur_x + chip_w, cur_y + m.ch + 1.0);
                scene.push_hit_clipped(hit, clip_id, HitAction::CopyText(text.clone()));
                scene.push_hover_clipped(
                    hit,
                    clip_id,
                    HoverKind::Tooltip {
                        title: text,
                        subtitle: Some("click to copy".to_owned()),
                    },
                );
                cur_x += chip_w;
                at_line_start = false;
            }
            Atom::Word { text, color, bold, italic, underline, copy } => {
                // Keep short trailing punctuation (`:`, `,`, `;`, `)`, `.`) attached to the
                // preceding code chip without awkwardly stranding it at the start of a new line!
                let is_attached_punct = text.len() <= 2
                    && text
                        .chars()
                        .all(|c| matches!(c, ':' | ',' | ';' | '.' | ')' | ']' | '!' | '?'));
                let w = text.chars().count() as f32 * m.cw;
                if !at_line_start && !is_attached_punct && cur_x + w > line_limit(line_idx) {
                    line_idx += 1;
                    cur_y += line_h;
                    cur_x = x0;
                }
                // If a single long token still exceeds the full line width, hard-wrap cleanly.
                let max_line_w = (line_limit(line_idx) - cur_x).max(40.0);
                if w > max_line_w && w > (max_w - 8.0).max(40.0) {
                    let mut rest = text.as_str();
                    while !rest.is_empty() {
                        let avail_cols = ((line_limit(line_idx) - cur_x).max(40.0) / m.cw)
                            .floor()
                            .max(1.0) as usize;
                        let take_b = rest
                            .char_indices()
                            .nth(avail_cols)
                            .map_or(rest.len(), |(bi, _)| bi);
                        let chunk = &rest[..take_b];
                        let cw = chunk.chars().count() as f32 * m.cw;
                        scene.text(cur_x, cur_y, chunk, color, bold, italic, clip_id);
                        cur_x += cw;
                        rest = &rest[take_b..];
                        if !rest.is_empty() {
                            line_idx += 1;
                            cur_y += line_h;
                            cur_x = x0;
                        }
                    }
                } else {
                    scene.text(cur_x, cur_y, &text, color, bold, italic, clip_id);
                    if underline {
                        scene.over_round_rect(
                            cur_x,
                            cur_y + m.ch - 1.5,
                            cur_x + w,
                            cur_y + m.ch - 0.5,
                            0.0,
                            color,
                            clip_id,
                        );
                    }
                    if let Some(target) = copy {
                        let hit = HitRect::new(cur_x, cur_y, cur_x + w, cur_y + m.ch);
                        scene.push_hit_clipped(hit, clip_id, HitAction::CopyText(target.clone()));
                        scene.push_hover_clipped(
                            hit,
                            clip_id,
                            HoverKind::Tooltip {
                                title: target,
                                subtitle: Some("click to copy".to_owned()),
                            },
                        );
                    }
                    cur_x += w;
                }
                at_line_start = false;
            }
        }
    }

    (cur_y - y0) + m.ch
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

impl DiagSeverity {
    fn palette(self) -> ([f32; 4], [f32; 4], [f32; 4], [f32; 4]) {
        // (card_bg, rail_col, badge_bg, badge_fg)
        match self {
            DiagSeverity::Error => (
                theme::rgba(0xe0, 0x6c, 0x75, 0.08),
                theme::DANGER,
                theme::rgba(0xe0, 0x6c, 0x75, 0.18),
                theme::DANGER,
            ),
            DiagSeverity::Warning => (
                theme::rgba(0xe5, 0xb5, 0x67, 0.075),
                theme::rgb(0xe5, 0xb5, 0x67),
                theme::rgba(0xe5, 0xb5, 0x67, 0.18),
                theme::rgb(0xe5, 0xb5, 0x67),
            ),
            DiagSeverity::Note => (
                theme::rgba(0x58, 0x9d, 0xf6, 0.065),
                theme::SERIES_BLUE,
                theme::rgba(0x58, 0x9d, 0xf6, 0.16),
                theme::SERIES_BLUE,
            ),
            DiagSeverity::Help => (
                theme::rgba(0x4c, 0xc3, 0x8a, 0.065),
                theme::OK,
                theme::rgba(0x4c, 0xc3, 0x8a, 0.16),
                theme::OK,
            ),
            DiagSeverity::Info => (
                theme::rgba(0x9a, 0xa0, 0xaa, 0.05),
                theme::INK_4,
                theme::rgba(0x9a, 0xa0, 0xaa, 0.14),
                theme::INK_2,
            ),
        }
    }
}

/// Parsed structure of one uncolored diagnostic output line (`stderr` or diagnostic `stdout`).
#[derive(Clone, Debug, PartialEq, Eq)]
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

fn is_short_tool_prefix(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 24
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && s.bytes().any(|b| b.is_ascii_alphabetic())
}

fn is_blaze_diag_id(s: &str) -> bool {
    (3..=12).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && s.bytes().any(|b| b.is_ascii_alphabetic())
        && !s.contains('.')
        && !s.contains(':')
}

/// True when a token looks like `path/file.rs:42:1` or `Cargo.toml:69:1`.
fn looks_like_inline_location(tok: &str) -> bool {
    let mut parts = tok.split(':');
    let Some(file) = parts.next() else { return false };
    let Some(line) = parts.next() else { return false };
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
    let lower = tok.to_ascii_lowercase();
    if lower == "error" || (lower.starts_with("error[") && lower.ends_with(']')) {
        Some(DiagSeverity::Error)
    } else if lower == "warning" || (lower.starts_with("warning[") && lower.ends_with(']')) {
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
pub fn classify_stderr_line(raw: &str, exit_code: i32) -> ClassifiedLine<'_> {
    let trimmed = raw.trim_start();
    let indent = raw.len() - trimmed.len();

    if let Some(loc) = trimmed.strip_prefix("-->").or_else(|| trimmed.strip_prefix(":::")) {
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
        if let Some(body) = trimmed.strip_prefix("help:").or_else(|| trimmed.strip_prefix("hint:")) {
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
            let head_lower = head.to_ascii_lowercase();
            if !body.is_empty() {
                if head_lower == "error"
                    || head_lower.starts_with("error[")
                    || head_lower == "fatal"
                    || head_lower == "panic"
                {
                    return ClassifiedLine::Header {
                        severity: DiagSeverity::Error,
                        badge: head,
                        id: None,
                        location: None,
                        body,
                    };
                }
                if head_lower == "warning"
                    || head_lower.starts_with("warning[")
                    || head_lower == "warn"
                {
                    return ClassifiedLine::Header {
                        severity: DiagSeverity::Warning,
                        badge: head,
                        id: None,
                        location: None,
                        body,
                    };
                }
                if head_lower == "note" {
                    return ClassifiedLine::Header {
                        severity: DiagSeverity::Note,
                        badge: head,
                        id: None,
                        location: None,
                        body,
                    };
                }
                if head_lower == "help" || head_lower == "hint" || head_lower == "suggestion" {
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
            classify_stderr_line(raw, exit_code),
            ClassifiedLine::Header { .. } | ClassifiedLine::Location(_)
        )
    })
}

/// Split an optional trailing parenthetical source hint like `(engine_body_plan.rs)` off the end
/// of a diagnostic message so it renders cleanly as a quiet muted tag.
fn split_trailing_source_tag(body: &str) -> (&str, Option<&str>) {
    let trimmed = body.trim_end();
    if let Some(open) = trimmed.rfind(" (")
        && trimmed.ends_with(')')
    {
        let inner = &trimmed[open + 2..trimmed.len() - 1];
        if !inner.is_empty()
            && !inner.contains(' ')
            && looks_like_file_path(inner)
        {
            return (trimmed[..open].trim_end(), Some(inner));
        }
    }
    (body, None)
}

/// Render a contiguous group of uncolored `stderr` lines as crisp, word-wrapped diagnostic cards
/// with severity rails, pill badges, clickable inline Markdown `` `code` `` chips, and monospace
/// compiler code frames. Returns vertical height consumed.
#[allow(clippy::too_many_arguments)]
pub fn layout_stderr_diagnostic_group(
    scene: &mut Scene,
    lines: &[(i64, &str)],
    exit_code: i32,
    x0: f32,
    y0: f32,
    width: f32,
    first_card_right_reserve: f32,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    struct Card<'a> {
        severity: DiagSeverity,
        badge: Option<&'a str>,
        id: Option<&'a str>,
        header_loc: Option<&'a str>,
        entries: Vec<CardEntry<'a>>,
    }
    enum CardEntry<'a> {
        Prose(&'a str),
        /// Verbatim monospace line preserving indentation and column alignment (e.g. `git pull` ref table).
        Verbatim(&'a str),
        SubNote { severity: DiagSeverity, badge: &'a str, body: &'a str },
        Location(&'a str),
        CodeFrame(&'a str),
        Summary { summary: &'a str, command: Option<&'a str> },
    }

    let has_inline_md = |s: &str| {
        parse_inline_markdown(s)
            .iter()
            .any(|sp| !matches!(sp, MdInline::Text(_)))
    };

    let mut cards: Vec<Card<'_>> = Vec::new();
    for &(_, raw) in lines {
        if raw.trim().is_empty() {
            continue;
        }
        match classify_stderr_line(raw, exit_code) {
            ClassifiedLine::Header { severity, badge, id, location, body } => {
                let mut entries = Vec::new();
                if !body.is_empty() {
                    entries.push(CardEntry::Prose(body));
                }
                cards.push(Card {
                    severity,
                    badge: Some(badge),
                    id,
                    header_loc: location,
                    entries,
                });
            }
            ClassifiedLine::SubNote { severity, badge, body } => {
                if let Some(last) = cards.last_mut() {
                    last.entries.push(CardEntry::SubNote { severity, badge, body });
                } else {
                    cards.push(Card {
                        severity,
                        badge: Some(badge),
                        id: None,
                        header_loc: None,
                        entries: vec![CardEntry::Prose(body)],
                    });
                }
            }
            ClassifiedLine::Location(loc) => {
                if let Some(last) = cards.last_mut() {
                    last.entries.push(CardEntry::Location(loc));
                } else {
                    cards.push(Card {
                        severity: if exit_code != 0 { DiagSeverity::Error } else { DiagSeverity::Note },
                        badge: None,
                        id: None,
                        header_loc: None,
                        entries: vec![CardEntry::Location(loc)],
                    });
                }
            }
            ClassifiedLine::CodeFrame(frame) => {
                if let Some(last) = cards.last_mut() {
                    last.entries.push(CardEntry::CodeFrame(frame));
                } else {
                    cards.push(Card {
                        severity: if exit_code != 0 { DiagSeverity::Error } else { DiagSeverity::Note },
                        badge: None,
                        id: None,
                        header_loc: None,
                        entries: vec![CardEntry::CodeFrame(frame)],
                    });
                }
            }
            ClassifiedLine::Summary { has_errors, summary, command } => {
                cards.push(Card {
                    severity: if has_errors { DiagSeverity::Error } else { DiagSeverity::Help },
                    badge: None,
                    id: None,
                    header_loc: None,
                    entries: vec![CardEntry::Summary { summary, command }],
                });
            }
            ClassifiedLine::Plain(txt) => {
                let is_structured_card = cards.last().is_some_and(|c| c.badge.is_some());
                let entry = if is_structured_card || has_inline_md(txt) {
                    CardEntry::Prose(txt)
                } else {
                    CardEntry::Verbatim(txt)
                };
                if let Some(last) = cards.last_mut() {
                    last.entries.push(entry);
                } else {
                    cards.push(Card {
                        severity: if exit_code != 0 { DiagSeverity::Error } else { DiagSeverity::Info },
                        badge: None,
                        id: None,
                        header_loc: None,
                        entries: vec![entry],
                    });
                }
            }
        }
    }

    if cards.is_empty() {
        return 0.0;
    }

    let mut cur_y = y0;
    let x1 = x0 + width;
    for (ci, card) in cards.iter().enumerate() {
        if ci > 0 {
            cur_y += 6.0;
        }
        let card_y0 = cur_y;
        let (card_bg, rail_col, badge_bg, badge_fg) = card.severity.palette();
        let bg_idx = scene.rects.len();
        scene.round_rect(x0, card_y0, x1, card_y0 + 24.0, 6.0, card_bg, 0.0, [0.0; 4], clip_id);
        let rail_idx = scene.rects.len();
        scene.round_rect(x0, card_y0, x0 + 2.5, card_y0 + 24.0, 1.25, rail_col, 0.0, [0.0; 4], clip_id);

        let inner_x = x0 + 12.0;
        let inner_w = (x1 - 12.0 - inner_x).max(80.0);
        let right_reserve = if ci == 0 { first_card_right_reserve } else { 0.0 };
        cur_y += 7.0;

        let mut badge_indent = 0.0f32;
        if let Some(badge) = card.badge {
            let bw = m.measure(badge, TextFace::UiSmall) + 12.0;
            let bh = m.ch - 1.0;
            scene.round_rect(
                inner_x,
                cur_y + 0.5,
                inner_x + bw,
                cur_y + 0.5 + bh,
                bh * 0.5,
                badge_bg,
                1.0,
                [rail_col[0], rail_col[1], rail_col[2], 0.40],
                clip_id,
            );
            scene.ui_small(inner_x + 6.0, cur_y + 0.5, badge, badge_fg, true, false, clip_id);
            let mut hx = inner_x + bw + 8.0;

            if let Some(id) = card.id {
                let id_w = id.chars().count() as f32 * m.cw + 10.0;
                scene.round_rect(
                    hx,
                    cur_y + 0.5,
                    hx + id_w,
                    cur_y + 0.5 + bh,
                    4.0,
                    theme::BG_CHIP,
                    1.0,
                    theme::LINE,
                    clip_id,
                );
                scene.text(hx + 5.0, cur_y, id, theme::INK_2, false, false, clip_id);
                let hit = HitRect::new(hx, cur_y, hx + id_w, cur_y + m.ch);
                scene.push_hit_clipped(hit, clip_id, HitAction::CopyText(id.to_owned()));
                scene.push_hover_clipped(
                    hit,
                    clip_id,
                    HoverKind::Tooltip {
                        title: id.to_owned(),
                        subtitle: Some("click to copy fix id".to_owned()),
                    },
                );
                hx += id_w + 8.0;
            }

            if let Some(loc) = card.header_loc {
                let avail = (x1 - 12.0 - right_reserve - hx).max(40.0);
                let max_ch = (avail / m.cw).floor().max(8.0) as usize;
                let shown = crate::view::truncate_middle(loc, max_ch);
                let lw = shown.chars().count() as f32 * m.cw;
                scene.text(hx, cur_y, &shown, theme::SERIES_BLUE, false, false, clip_id);
                let hit = HitRect::new(hx, cur_y, hx + lw, cur_y + m.ch);
                scene.push_hit_clipped(hit, clip_id, HitAction::CopyText(loc.to_owned()));
                scene.push_hover_clipped(
                    hit,
                    clip_id,
                    HoverKind::Tooltip {
                        title: loc.to_owned(),
                        subtitle: Some("click to copy location".to_owned()),
                    },
                );
            }

            if card.id.is_some() || card.header_loc.is_some() {
                // Dedicated header row when diagnostic carries a fix id or inline source location.
                cur_y += m.ch + 5.0;
            } else {
                badge_indent = bw + 8.0;
            }
        }

        let body_color = match card.severity {
            DiagSeverity::Error => theme::INK_1,
            DiagSeverity::Warning => theme::rgb(0xef, 0xd8, 0xb0),
            DiagSeverity::Note => theme::INK_2,
            DiagSeverity::Help => theme::rgb(0xc6, 0xe6, 0xd4),
            DiagSeverity::Info => theme::INK_2,
        };

        for (ei, entry) in card.entries.iter().enumerate() {
            let indent = if ei == 0 { badge_indent } else { 0.0 };
            let reserve = if ei == 0 && card.id.is_none() && card.header_loc.is_none() {
                right_reserve
            } else {
                0.0
            };
            match *entry {
                CardEntry::Summary { summary, command } => {
                    let mut sx = inner_x;
                    scene.text(sx, cur_y, summary, body_color, true, false, clip_id);
                    sx += summary.chars().count() as f32 * m.cw;
                    if let Some(cmd) = command {
                        sx += 10.0;
                        let avail = (x1 - 12.0 - reserve - sx - 10.0).max(40.0);
                        let max_ch = (avail / m.cw).floor().max(8.0) as usize;
                        let shown = crate::view::truncate_middle(cmd, max_ch);
                        let cw = shown.chars().count() as f32 * m.cw + 10.0;
                        scene.round_rect(
                            sx,
                            cur_y - 1.0,
                            sx + cw,
                            cur_y + m.ch + 1.0,
                            4.0,
                            theme::BG_CHIP,
                            1.0,
                            theme::LINE,
                            clip_id,
                        );
                        scene.text(sx + 5.0, cur_y, &shown, theme::INK_1, false, false, clip_id);
                        let hit = HitRect::new(sx, cur_y - 1.0, sx + cw, cur_y + m.ch + 1.0);
                        scene.push_hit_clipped(hit, clip_id, HitAction::CopyText(cmd.to_owned()));
                        scene.push_hover_clipped(
                            hit,
                            clip_id,
                            HoverKind::Tooltip {
                                title: cmd.to_owned(),
                                subtitle: Some("click to copy command".to_owned()),
                            },
                        );
                    }
                    cur_y += m.ch + 3.0;
                }
                CardEntry::Verbatim(raw) => {
                    let avail_w = (inner_w - reserve).max(80.0);
                    let max_cols = (avail_w / m.cw).floor().max(16.0) as usize;
                    for seg in crate::view::wrap_lines(raw, max_cols) {
                        scene.text(inner_x, cur_y, seg, body_color, false, false, clip_id);
                        cur_y += m.ch + 2.0;
                    }
                }
                CardEntry::Prose(txt) => {
                    let (main_txt, src_tag) = split_trailing_source_tag(txt);
                    let h = layout_inline_markdown(
                        scene,
                        main_txt,
                        inner_x,
                        cur_y,
                        inner_w,
                        indent,
                        reserve,
                        body_color,
                        m,
                        clip_id,
                    );
                    cur_y += h + 3.0;
                    if let Some(tag) = src_tag {
                        let tw = tag.chars().count() as f32 * m.cw + 10.0;
                        let tx = (x1 - 12.0 - tw).max(inner_x);
                        let tag_y = cur_y - 1.0;
                        scene.round_rect(
                            tx,
                            tag_y,
                            tx + tw,
                            tag_y + m.ch - 1.0,
                            4.0,
                            theme::rgba(0x88, 0x90, 0x9e, 0.10),
                            1.0,
                            theme::LINE_FAINT,
                            clip_id,
                        );
                        scene.text(tx + 5.0, tag_y, tag, theme::INK_4, false, false, clip_id);
                        let hit = HitRect::new(tx, tag_y, tx + tw, tag_y + m.ch);
                        scene.push_hit_clipped(hit, clip_id, HitAction::CopyText(tag.to_owned()));
                        cur_y += m.ch + 2.0;
                    }
                }
                CardEntry::SubNote { severity, badge, body } => {
                    let (_, sr, sbg, sfg) = severity.palette();
                    let bw = m.measure(badge, TextFace::UiSmall) + 10.0;
                    let bh = m.ch - 2.0;
                    scene.round_rect(
                        inner_x,
                        cur_y + 1.0,
                        inner_x + bw,
                        cur_y + 1.0 + bh,
                        bh * 0.5,
                        sbg,
                        1.0,
                        [sr[0], sr[1], sr[2], 0.35],
                        clip_id,
                    );
                    scene.ui_small(inner_x + 5.0, cur_y + 1.0, badge, sfg, true, false, clip_id);
                    let h = layout_inline_markdown(
                        scene,
                        body,
                        inner_x,
                        cur_y,
                        inner_w,
                        bw + 7.0,
                        0.0,
                        theme::INK_2,
                        m,
                        clip_id,
                    );
                    cur_y += h + 3.0;
                }
                CardEntry::Location(loc) => {
                    scene.text(inner_x, cur_y, "→ ", theme::INK_4, false, false, clip_id);
                    let lx = inner_x + 2.0 * m.cw;
                    let max_ch = ((inner_w - 2.0 * m.cw) / m.cw).floor().max(10.0) as usize;
                    let shown = crate::view::truncate_middle(loc, max_ch);
                    let lw = shown.chars().count() as f32 * m.cw;
                    scene.text(lx, cur_y, &shown, theme::SERIES_BLUE, false, false, clip_id);
                    let hit = HitRect::new(lx, cur_y, lx + lw, cur_y + m.ch);
                    scene.push_hit_clipped(hit, clip_id, HitAction::CopyText(loc.to_owned()));
                    scene.push_hover_clipped(
                        hit,
                        clip_id,
                        HoverKind::Tooltip {
                            title: loc.to_owned(),
                            subtitle: Some("click to copy location".to_owned()),
                        },
                    );
                    cur_y += m.ch + 3.0;
                }
                CardEntry::CodeFrame(frame) => {
                    let max_ch = (inner_w / m.cw).floor().max(16.0) as usize;
                    let shown = crate::view::truncate_chars(frame, max_ch);
                    if let Some((gutter, code)) = shown.split_once('|') {
                        let gw = (gutter.chars().count() + 1) as f32 * m.cw;
                        scene.text(inner_x, cur_y, gutter, theme::INK_4, false, false, clip_id);
                        scene.text(
                            inner_x + gutter.chars().count() as f32 * m.cw,
                            cur_y,
                            "│",
                            theme::LINE_STRONG,
                            false,
                            false,
                            clip_id,
                        );
                        let is_caret = code.trim_start().starts_with('^') || code.trim_start().starts_with('-');
                        let code_col = if is_caret { rail_col } else { theme::INK_2 };
                        scene.text(inner_x + gw, cur_y, code, code_col, is_caret, false, clip_id);
                    } else {
                        scene.text(inner_x, cur_y, &shown, theme::INK_3, false, false, clip_id);
                    }
                    cur_y += m.ch + 2.0;
                }
            }
        }

        cur_y += 4.0;
        scene.rects[bg_idx].rect[3] = cur_y;
        scene.rects[rail_idx].rect[3] = cur_y;
    }

    cur_y - y0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_inline_markdown_handles_code_bold_italic_and_links() {
        let spans = parse_inline_markdown(
            "check `crates/globset/src/serde_impl.rs` and `#[cfg]` with **bold**, *italic*, [docs](https://ix.dev), and 2 * 3",
        );
        assert_eq!(
            spans,
            vec![
                MdInline::Text("check ".into()),
                MdInline::Code("crates/globset/src/serde_impl.rs".into()),
                MdInline::Text(" and ".into()),
                MdInline::Code("#[cfg]".into()),
                MdInline::Text(" with ".into()),
                MdInline::Bold("bold".into()),
                MdInline::Text(", ".into()),
                MdInline::Italic("italic".into()),
                MdInline::Text(", ".into()),
                MdInline::Link { label: "docs".into(), url: "https://ix.dev".into() },
                MdInline::Text(", and 2 * 3".into()),
            ]
        );
    }

    #[test]
    fn classify_stderr_separates_tool_error_note_warning_location_and_frame() {
        assert_eq!(
            classify_stderr_line("rg: ripgrep requires at least one pattern", 2),
            ClassifiedLine::Header {
                severity: DiagSeverity::Error,
                badge: "rg",
                id: None,
                location: None,
                body: "ripgrep requires at least one pattern",
            }
        );
        assert_eq!(
            classify_stderr_line("note: `crates/globset/src/serde_impl.rs`: no unit checks this file", 2),
            ClassifiedLine::Header {
                severity: DiagSeverity::Note,
                badge: "note",
                id: None,
                location: None,
                body: "`crates/globset/src/serde_impl.rs`: no unit checks this file",
            }
        );
        assert_eq!(
            classify_stderr_line("error[E0308]: mismatched types", 1),
            ClassifiedLine::Header {
                severity: DiagSeverity::Error,
                badge: "error[E0308]",
                id: None,
                location: None,
                body: "mismatched types",
            }
        );
        assert_eq!(
            classify_stderr_line(
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
            classify_stderr_line(
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
            classify_stderr_line("0 errors, 1 warning, 1 fix: blaze check --fix u7eys", 0),
            ClassifiedLine::Summary {
                has_errors: false,
                summary: "0 errors, 1 warning, 1 fix",
                command: Some("blaze check --fix u7eys"),
            }
        );
        assert_eq!(
            classify_stderr_line("  --> src/main.rs:42:10", 1),
            ClassifiedLine::Location("src/main.rs:42:10")
        );
        assert_eq!(
            classify_stderr_line("42 | let x: u32 = \"hi\";", 1),
            ClassifiedLine::CodeFrame("42 | let x: u32 = \"hi\";")
        );
    }

    #[test]
    fn looks_like_diagnostic_output_detects_blaze_check_stdout_and_ignores_plain_output() {
        let blaze_stdout = [
            (1i64, "note: `crates/globset/src/serde_impl.rs`: no unit checks this file"),
            (2i64, "u7eys  warning[unused-dependency]  Cargo.toml:69:1  dependency `serde` is declared"),
            (3i64, "  fix: delete `serde = \"1.0.77\"`"),
            (4i64, "0 errors, 1 warning, 1 fix: blaze check --fix u7eys"),
        ];
        assert!(looks_like_diagnostic_output(&blaze_stdout, 0));

        let normal_stdout = [
            (1i64, "hello world"),
            (2i64, "total 42"),
        ];
        assert!(!looks_like_diagnostic_output(&normal_stdout, 0));
    }

    #[test]
    fn layout_stderr_diagnostic_group_renders_separate_error_and_note_cards_with_code_chips() {
        let mut scene = Scene::new();
        let lines = [
            (10i64, "rg: ripgrep requires at least one pattern to execute a search"),
            (
                20i64,
                "note: `crates/globset/src/serde_impl.rs`: no unit checks this file (by the `mod` in `crates/globset/src/lib.rs`) when a `#[cfg]` is false (engine_body_plan.rs)",
            ),
        ];
        let h = layout_stderr_diagnostic_group(
            &mut scene,
            &lines,
            2,
            16.0,
            20.0,
            720.0,
            90.0,
            CellMetrics::default(),
            0,
        );
        assert!(h > 60.0);
        // Should emit clickable copy regions for all 4 backticked code spans + trailing source tag!
        let copy_hits: Vec<&str> = scene
            .hit_regions
            .iter()
            .filter_map(|r| match &r.action {
                HitAction::CopyText(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert!(copy_hits.contains(&"crates/globset/src/serde_impl.rs"));
        assert!(copy_hits.contains(&"mod"));
        assert!(copy_hits.contains(&"#[cfg]"));
        assert!(copy_hits.contains(&"engine_body_plan.rs"));
        // All diagnostic body text should render in monospace.
        assert!(
            scene
                .text_runs
                .iter()
                .filter(|r| r.text == "ripgrep" || r.text == "unit")
                .all(|r| r.face == TextFace::Mono)
        );
    }

    #[test]
    fn plain_cli_stderr_preserves_monospace_column_alignment() {
        let mut scene = Scene::new();
        let line = " - [deleted]         (none)     -> origin/feat/branch-hover-commit-graph";
        let lines = [(1i64, "From https://github.com/indexable-inc/blaze"), (2i64, line)];
        let h = layout_stderr_diagnostic_group(
            &mut scene,
            &lines,
            0,
            16.0,
            20.0,
            900.0,
            0.0,
            CellMetrics::default(),
            0,
        );
        assert!(h > 20.0);
        assert!(
            scene
                .text_runs
                .iter()
                .any(|r| r.face == TextFace::Mono && r.text == line)
        );
    }
}
