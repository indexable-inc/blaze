//! Rich structured CLI Help & Man-Page parser and GPU renderer for `blaze-shell`.
//!
//! Turns raw `--help` / `-h` / `help` / `man` walls of text (`git --help`, `rg --help`,
//! `blaze --help`, `cargo --help`, GNU/clap/cobra/man pages) into a scannable, interactive
//! native manual card:
//! - Syntax-highlighted `USAGE` synopsis with clickable flags and placeholders
//! - Runnable `EXAMPLES` card (parsed from the tool's own `EXAMPLES:` section or augmented
//!   with curated TLDR-style examples) where clicking inserts directly into the composer
//! - Categorized subcommand & option tables with aligned columns, inline Markdown `` `code` ``
//!   chips, `EXPERIMENTAL` badges, clickable `see also` links, and one-click `?` subcommand
//!   help drill-down.

use blaze_sh::lex::highlight;

use crate::diag_view::layout_inline_markdown;
use crate::icons::{Icon, draw_icon};
use crate::theme;
use crate::view::{CellMetrics, HitAction, HitRect, HoverKind, Scene, TextFace, truncate_chars};

/// Structured representation of a parsed CLI `--help` or `man` page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelpDoc {
    /// Root command / subcommand title (e.g. `"git"`, `"git switch"`, `"rg"`, `"blaze"`).
    pub command: String,
    /// Optional introductory summary paragraph above the sections.
    pub summary: Option<String>,
    /// Usage / synopsis lines (e.g. `["git [-v | --version] ... <command> [<args>]"]`).
    pub usage: Vec<String>,
    /// Categorized command / option / flag sections.
    pub sections: Vec<HelpSection>,
    /// Runnable practical examples (from tool output or curated TLDR examples).
    pub examples: Vec<HelpExample>,
    /// Trailing footer notes / references.
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelpSection {
    pub title: String,
    /// Optional `(see also: <cmd>)` reference extracted from the section heading.
    pub see_also: Option<String>,
    pub items: Vec<HelpItem>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelpItem {
    /// Subcommand or flag syntax (e.g. `"switch"`, `"-C <path>"`, `"-p, --paginate"`).
    pub name: String,
    pub is_flag: bool,
    pub desc: String,
    /// Optional status/default pill badge (e.g. `"EXPERIMENTAL"`, `"default: auto"`).
    pub badge: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelpExample {
    pub cmd: String,
    pub desc: String,
}

/// Strip ANSI CSI/OSC escape sequences so colored `--help` output parses cleanly.
fn strip_ansi(s: &str) -> String {
    if !s.contains('\x1b') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
                continue;
            } else if i + 1 < bytes.len() && bytes[i + 1] == b']' {
                i += 2;
                while i < bytes.len() && bytes[i] != 0x07 {
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == 0x07 {
                    i += 1;
                }
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// True when `cmd_source` is a direct help/man invocation (not piped into another command).
pub fn is_help_invocation(cmd_source: &str) -> bool {
    let trimmed = cmd_source.trim();
    if trimmed.is_empty() || trimmed.contains('|') || trimmed.contains('>') {
        return false;
    }
    let toks: Vec<&str> = trimmed.split_whitespace().collect();
    if toks.is_empty() {
        return false;
    }
    if toks[0] == "man" || toks[0] == "tldr" {
        return toks.len() >= 2;
    }
    if toks.len() >= 2 && matches!(toks[1], "help" | "--help" | "-h" | "-help") {
        return true;
    }
    toks.iter().skip(1).any(|&t| matches!(t, "--help" | "-h" | "-help"))
}

/// Extract clean base command name from the user's invocation (e.g. `"git switch --help"` -> `"git switch"`).
fn extract_command_name(cmd_source: &str, usage: &[String]) -> String {
    let toks: Vec<&str> = cmd_source.split_whitespace().collect();
    if !toks.is_empty() {
        let mut parts: Vec<&str> = Vec::new();
        let start = if matches!(toks[0], "man" | "tldr") { 1 } else { 0 };
        for &t in toks.iter().skip(start) {
            if matches!(t, "--help" | "-h" | "-help" | "help") {
                continue;
            }
            if t.starts_with('-') {
                break;
            }
            parts.push(t);
            if parts.len() >= 2 {
                break;
            }
        }
        if !parts.is_empty() {
            return parts.join(" ");
        }
    }
    if let Some(first_u) = usage.first()
        && let Some(word) = first_u.split_whitespace().next()
    {
        return word.to_owned();
    }
    "command".to_owned()
}

/// Split an indented two-column help row (`   clone      Clone a repo`, `  -v, --verbose  Desc`)
/// into `(name, description)`.
fn split_help_item_line(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim_start();
    let indent = raw.len() - trimmed.len();
    if indent == 0 || trimmed.is_empty() {
        return None;
    }
    // Look for a tab or a run of >= 2 spaces separating the left item token from its description.
    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    let mut split_at: Option<usize> = None;
    while i < bytes.len() {
        if bytes[i] == b'\t' {
            split_at = Some(i);
            break;
        }
        if bytes[i] == b' ' && i + 1 < bytes.len() && bytes[i + 1] == b' ' {
            split_at = Some(i);
            break;
        }
        i += 1;
    }
    let idx = split_at?;
    let name = trimmed[..idx].trim();
    let desc = trimmed[idx..].trim();
    if name.is_empty() || desc.is_empty() || name.len() > 42 {
        return None;
    }
    // Reject ordinary indented prose sentences (e.g. wrapped continuation lines with double spaces).
    let is_flag = name.starts_with('-');
    if !is_flag {
        // Subcommands are 1-2 short tokens (e.g. `clone`, `add`, `worktree add`, `help [cmd]`).
        let word_count = name.split_whitespace().count();
        if word_count > 3 || name.ends_with('.') || name.ends_with(':') {
            return None;
        }
        let first_ch = name.chars().next()?;
        if !(first_ch.is_ascii_alphanumeric() || first_ch == '<' || first_ch == '[') {
            return None;
        }
    }
    Some((name.to_owned(), desc.to_owned()))
}

/// Extract optional `(see also: <cmd>)` suffix from a section heading (e.g. git's headings).
fn parse_section_heading(raw: &str) -> Option<(String, Option<String>)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || raw.starts_with(' ') || raw.starts_with('\t') {
        return None;
    }
    if trimmed.len() > 86 {
        return None;
    }
    // Skip obvious footer/prose sentences (ends with period or starts with quote).
    if trimmed.ends_with('.') || trimmed.starts_with('\'') || trimmed.starts_with('"') {
        return None;
    }
    // Git-style `(see also: git help tutorial)` heading.
    if let Some(open) = trimmed.find("(see also:")
        && trimmed.ends_with(')')
    {
        let title = trimmed[..open].trim().trim_end_matches(':').to_owned();
        let see = trimmed[open + "(see also:".len()..trimmed.len() - 1].trim().to_owned();
        if !title.is_empty() {
            return Some((title, if see.is_empty() { None } else { Some(see) }));
        }
    }
    // Standard colon heading (`Commands:`, `Options:`, `Flags:`, `Arguments:`, `Examples:`).
    if let Some(stem) = trimmed.strip_suffix(':') {
        let s = stem.trim();
        let words = s.split_whitespace().count();
        if !s.is_empty() && words <= 6 {
            return Some((s.to_owned(), None));
        }
    }
    // ALL-CAPS man-page heading (`NAME`, `SYNOPSIS`, `OPTIONS`, `COMMANDS`, `EXAMPLES`).
    if trimmed.len() <= 32
        && trimmed.bytes().any(|b| b.is_ascii_uppercase())
        && trimmed
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b == b' ' || b == b'-' || b == b'/')
    {
        return Some((trimmed.to_owned(), None));
    }
    // Short unindented category line directly preceding indented items (e.g. `grow, mark and tweak your common history`).
    if trimmed.split_whitespace().count() <= 9 && !trimmed.contains("  ") {
        return Some((trimmed.to_owned(), None));
    }
    None
}

/// Extract optional leading `EXPERIMENTAL:` / `DEPRECATED:` or trailing `[default: ...]` badge.
fn extract_item_badge(raw_desc: &str) -> (String, Option<String>) {
    let d = raw_desc.trim();
    for prefix in ["EXPERIMENTAL:", "DEPRECATED:", "BETA:", "UNSTABLE:"] {
        if let Some(rest) = d.strip_prefix(prefix) {
            return (
                rest.trim_start().to_owned(),
                Some(prefix.trim_end_matches(':').to_owned()),
            );
        }
    }
    if let Some(open) = d.rfind("[default:")
        && d.ends_with(']')
    {
        let badge = d[open + 1..d.len() - 1].trim().to_owned();
        let clean = d[..open].trim_end().to_owned();
        if !clean.is_empty() && badge.len() <= 28 {
            return (clean, Some(badge));
        }
    }
    (d.to_owned(), None)
}

/// Parse a CLI `--help` / `-h` / `man` output into a structured [`HelpDoc`].
/// Returns `None` if the text does not look like structured help output.
pub fn parse_help_doc(cmd_source: &str, raw_lines: &[&str]) -> Option<HelpDoc> {
    let explicit_help = is_help_invocation(cmd_source);
    let clean_lines: Vec<String> = raw_lines.iter().map(|l| strip_ansi(l)).collect();
    if clean_lines.len() < 4 {
        return None;
    }

    let mut usage: Vec<String> = Vec::new();
    let mut summary_lines: Vec<String> = Vec::new();
    let mut sections: Vec<HelpSection> = Vec::new();
    let mut examples: Vec<HelpExample> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    let mut in_usage = false;
    let mut in_examples = false;
    let mut pending_example_desc: Option<String> = None;

    let n = clean_lines.len();
    let mut i = 0usize;
    while i < n {
        let raw = clean_lines[i].as_str();
        let trimmed = raw.trim();

        if trimmed.is_empty() {
            in_usage = false;
            i += 1;
            continue;
        }

        // 1. Usage / Synopsis block (`usage: git ...` / `Usage: rg ...` / `USAGE:`).
        let lower = trimmed.to_ascii_lowercase();
        if !raw.starts_with(' ')
            && (lower.starts_with("usage:") || lower == "synopsis" || lower == "synopsis:")
        {
            in_usage = true;
            in_examples = false;
            if let Some((_, rest)) = trimmed.split_once(':') {
                let r = rest.trim();
                if !r.is_empty() {
                    usage.push(r.to_owned());
                }
            }
            i += 1;
            continue;
        }
        if in_usage && (raw.starts_with(' ') || raw.starts_with('\t')) {
            usage.push(trimmed.to_owned());
            i += 1;
            continue;
        }
        in_usage = false;

        // 2. Indented `EXAMPLES` section parsing.
        if in_examples && (raw.starts_with(' ') || raw.starts_with('\t')) {
            if let Some(comment) = trimmed.strip_prefix('#') {
                pending_example_desc = Some(comment.trim().to_owned());
            } else if let Some((cmd_part, desc_part)) = split_help_item_line(raw) {
                examples.push(HelpExample {
                    cmd: cmd_part.trim_start_matches("$ ").to_owned(),
                    desc: desc_part,
                });
            } else if let Some(cmd_s) = trimmed.strip_prefix("$ ").or_else(|| trimmed.strip_prefix("> ")) {
                examples.push(HelpExample {
                    cmd: cmd_s.trim().to_owned(),
                    desc: pending_example_desc.take().unwrap_or_default(),
                });
            } else if let Some(desc) = pending_example_desc.take() {
                examples.push(HelpExample {
                    cmd: trimmed.to_owned(),
                    desc,
                });
            } else if let Some(last) = examples.last_mut()
                && last.desc.is_empty()
            {
                last.desc = trimmed.to_owned();
            } else {
                examples.push(HelpExample {
                    cmd: trimmed.to_owned(),
                    desc: String::new(),
                });
            }
            i += 1;
            continue;
        }

        // 3. Section headings (only treat unindented heading as a section if followed by indented rows
        //    or explicitly a known section header like `Options:` / `Commands:` / `Examples:`).
        if !raw.starts_with(' ')
            && !raw.starts_with('\t')
            && let Some((title, see_also)) = parse_section_heading(raw)
        {
            let next_nonempty = clean_lines[i + 1..]
                .iter()
                .find(|l| !l.trim().is_empty());
            let next_is_indented = next_nonempty
                .is_some_and(|l| l.starts_with(' ') || l.starts_with('\t'));
            let t_lower = title.to_ascii_lowercase();
            if t_lower == "examples" || t_lower == "example" {
                in_examples = true;
                pending_example_desc = None;
                i += 1;
                continue;
            }
            if next_is_indented {
                // Make sure the next indented line actually looks like a help item (not an arbitrary indented quote).
                let next_line = next_nonempty.unwrap().as_str();
                if split_help_item_line(next_line).is_some()
                    || t_lower.contains("command")
                    || t_lower.contains("option")
                    || t_lower.contains("flag")
                    || t_lower.contains("argument")
                    || see_also.is_some()
                {
                    in_examples = false;
                    sections.push(HelpSection {
                        title,
                        see_also,
                        items: Vec::new(),
                    });
                    i += 1;
                    continue;
                }
            }
        }

        // 4. Indented item row (`   clone      Clone a repository...`) or wrapped continuation.
        if raw.starts_with(' ') || raw.starts_with('\t') {
            if let Some((name, raw_desc)) = split_help_item_line(raw) {
                let is_flag = name.starts_with('-');
                let (desc, badge) = extract_item_badge(&raw_desc);
                if sections.is_empty() {
                    sections.push(HelpSection {
                        title: if is_flag { "Options".to_owned() } else { "Commands".to_owned() },
                        see_also: None,
                        items: Vec::new(),
                    });
                }
                sections.last_mut().unwrap().items.push(HelpItem {
                    name,
                    is_flag,
                    desc,
                    badge,
                });
                i += 1;
                continue;
            }
            // Wrapped continuation of the previous item's description.
            if let Some(sec) = sections.last_mut()
                && let Some(item) = sec.items.last_mut()
            {
                if !item.desc.is_empty() {
                    item.desc.push(' ');
                }
                item.desc.push_str(trimmed);
                let (clean_d, new_badge) = extract_item_badge(&item.desc);
                if new_badge.is_some() && item.badge.is_none() {
                    item.desc = clean_d;
                    item.badge = new_badge;
                }
                i += 1;
                continue;
            }
        }

        // 5. Unindented intro prose or trailing footer notes.
        in_examples = false;
        if sections.is_empty() {
            summary_lines.push(trimmed.to_owned());
        } else {
            // Join consecutive non-blank footer lines into clean paragraphs.
            if let Some(last_note) = notes.last_mut()
                && i > 0
                && !clean_lines[i - 1].trim().is_empty()
                && !trimmed.starts_with('\'')
            {
                last_note.push(' ');
                last_note.push_str(trimmed);
            } else {
                notes.push(trimmed.to_owned());
            }
        }
        i += 1;
    }

    // Drop empty sections that captured no items.
    sections.retain(|s| !s.items.is_empty());
    let total_items: usize = sections.iter().map(|s| s.items.len()).sum();

    // Guard against false positives on ordinary stdout:
    // require either an explicit help invocation with structure, or a `usage:` header + >=3 items.
    if explicit_help {
        if total_items < 2 && usage.is_empty() {
            return None;
        }
    } else if usage.is_empty() || total_items < 3 {
        return None;
    }

    let command = extract_command_name(cmd_source, &usage);
    if examples.is_empty() {
        examples = curated_examples_for(&command);
    }

    let summary = if summary_lines.is_empty() {
        None
    } else {
        Some(summary_lines.join(" "))
    };

    Some(HelpDoc {
        command,
        summary,
        usage,
        sections,
        examples,
        notes,
    })
}

/// Curated practical TLDR-style examples for common CLIs when their `--help` omits an `EXAMPLES` block.
fn curated_examples_for(command: &str) -> Vec<HelpExample> {
    let pairs: &[(&str, &str)] = match command {
        "git" => &[
            ("git status", "Inspect modified, staged, and untracked files"),
            ("git switch -c feat/topic", "Create and switch to a new branch"),
            ("git add -p", "Interactively stage specific hunks"),
            ("git commit -m \"message\"", "Record staged changes with a message"),
            ("git diff main...HEAD", "Compare current branch against main"),
            ("git log --oneline -n 15", "Compact recent commit history"),
        ],
        "git switch" => &[
            ("git switch main", "Switch to an existing branch"),
            ("git switch -c feat/new", "Create and switch to a new branch"),
            ("git switch -", "Jump back to the previous branch"),
        ],
        "git diff" => &[
            ("git diff", "Unstaged working tree changes"),
            ("git diff --staged", "Changes staged for the next commit"),
            ("git diff --stat main...HEAD", "Per-file churn summary vs main"),
        ],
        "git log" => &[
            ("git log --oneline -n 20", "Compact one-line commit list"),
            ("git log --graph --oneline --decorate -n 20", "Visual branch & merge graph"),
            ("git log -p -n 3", "Recent commits with full inline diffs"),
        ],
        "git rebase" => &[
            ("git rebase main", "Replay current branch commits onto main"),
            ("git rebase -i HEAD~4", "Interactively squash or reword last 4 commits"),
            ("git rebase --abort", "Cancel an in-progress rebase safely"),
        ],
        "git reset" => &[
            ("git reset --soft HEAD~1", "Undo last commit, keeping changes staged"),
            ("git reset HEAD <file>", "Unstage a file without losing edits"),
        ],
        "git stash" => &[
            ("git stash push -m \"wip\"", "Save uncommitted edits with a label"),
            ("git stash list", "List saved stashes"),
            ("git stash pop", "Re-apply and drop the latest stash"),
        ],
        "rg" | "ripgrep" => &[
            ("rg \"pattern\" src/", "Recursive regex search in src/"),
            ("rg -F \"literal\" -t rust", "Fixed-string search in Rust files only"),
            ("rg -l \"TODO\" -g \"!target\"", "List matching file paths, skipping target/"),
            ("rg -C 3 \"panic\"", "Show 3 lines of surrounding context"),
        ],
        "blaze" => &[
            ("blaze check", "Fast incremental typecheck & lint"),
            ("blaze test", "Build and run unit/integration tests"),
            ("blaze run -- --help", "Run default binary with arguments"),
            ("blaze check --fix", "Apply safe automatic lint fixes"),
        ],
        "cargo" => &[
            ("cargo check -p <crate>", "Fast compile check for one workspace crate"),
            ("cargo test -p <crate> -- <filter>", "Run matching tests in one crate"),
            ("cargo build --release", "Build optimized release artifacts"),
        ],
        "tar" => &[
            ("tar -czf archive.tar.gz dir/", "Create a gzip-compressed archive"),
            ("tar -xzf archive.tar.gz", "Extract a .tar.gz archive here"),
            ("tar -tf archive.tar.gz", "List contents without extracting"),
        ],
        "curl" => &[
            ("curl -fsSL https://example.com", "Fetch URL silently, following redirects"),
            ("curl -s https://api.example.com | from json", "Fetch JSON into structured table"),
            ("curl -X POST -H \"Content-Type: application/json\" -d '{}' <url>", "Send JSON POST request"),
        ],
        "gh" => &[
            ("gh pr status", "Check status of your open pull requests"),
            ("gh pr create --fill", "Open a PR from current branch commits"),
            ("gh pr view --web", "Open current branch PR in browser"),
        ],
        "docker" => &[
            ("docker ps -a", "List running and stopped containers"),
            ("docker logs -f <container>", "Stream live logs from a container"),
            ("docker exec -it <container> sh", "Open an interactive shell inside container"),
        ],
        "fd" => &[
            ("fd -e rs", "Find all .rs files recursively"),
            ("fd \"test\" crates/", "Find files matching \"test\" under crates/"),
        ],
        "jq" => &[
            ("jq '.items[] | {name, id}' data.json", "Project selected fields from JSON array"),
            ("jq -r '.version' package.json", "Extract raw unquoted string field"),
        ],
        _ => &[],
    };
    pairs
        .iter()
        .map(|&(cmd, desc)| HelpExample {
            cmd: cmd.to_owned(),
            desc: desc.to_owned(),
        })
        .collect()
}

/// Promote single-quoted CLI invocations like `'git help -a'` in footer notes into Markdown
/// inline `` `code` `` spans so they render as clickable chips.
fn promote_single_quoted_commands(text: &str, root_cmd: &str) -> String {
    let root = root_cmd.split_whitespace().next().unwrap_or(root_cmd);
    let mut out = String::with_capacity(text.len() + 16);
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            let prev_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            if prev_ok
                && let Some(rel_end) = text[i + 1..].find('\'')
                && rel_end > 0
                && rel_end <= 48
            {
                let inner = &text[i + 1..i + 1 + rel_end];
                if inner == root
                    || inner.starts_with(&format!("{root} "))
                    || inner.starts_with('-')
                {
                    out.push('`');
                    out.push_str(inner);
                    out.push('`');
                    i = i + 1 + rel_end + 1;
                    continue;
                }
            }
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Primary flag token to insert into composer when clicking a flag spec (`"-p, --paginate"` -> `"--paginate"`).
fn primary_flag_token(spec: &str) -> String {
    for part in spec.split([',', '|']) {
        let tok = part.split_whitespace().next().unwrap_or("").trim();
        if tok.starts_with("--") {
            return tok.split('=').next().unwrap_or(tok).to_owned();
        }
    }
    spec.split_whitespace()
        .next()
        .unwrap_or(spec)
        .trim_end_matches(',')
        .to_owned()
}

/// Render a syntax-highlighted CLI usage line with clickable flags (`--foo`, `-C`), amber
/// placeholders (`<path>`), and muted brackets (`[`, `]`, `|`). Returns vertical height consumed.
#[allow(clippy::too_many_arguments)]
fn layout_usage_line(
    scene: &mut Scene,
    line: &str,
    root_cmd: &str,
    x0: f32,
    y0: f32,
    max_w: f32,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    let line_h = m.ch + 3.0;
    let mut cx = x0;
    let mut cy = y0;
    let x1 = x0 + max_w;
    let flag_col = theme::rgb(0x74, 0xb9, 0xff);
    let arg_col = theme::rgb(0xe8, 0xa8, 0x48);

    // Split on whitespace while keeping `[...]` groups readable and wrapping cleanly at token edges.
    for (ti, tok) in line.split_whitespace().enumerate() {
        let tok_w = tok.chars().count() as f32 * m.cw;
        if ti > 0 {
            if cx + m.cw + tok_w > x1 && cx > x0 {
                cy += line_h;
                cx = x0 + 4.0 * m.cw;
            } else {
                cx += m.cw;
            }
        }

        // Render sub-tokens inside brackets cleanly (e.g. `[--git-dir=<path>]`).
        let mut rest = tok;
        while !rest.is_empty() {
            if let Some(ch) = rest.chars().next()
                && matches!(ch, '[' | ']' | '|' | '(' | ')')
            {
                let s = &rest[..ch.len_utf8()];
                scene.text(cx, cy, s, theme::INK_4, false, false, clip_id);
                cx += m.cw;
                rest = &rest[ch.len_utf8()..];
                continue;
            }
            if rest.starts_with('<')
                && let Some(end) = rest.find('>')
            {
                let seg = &rest[..=end];
                scene.text(cx, cy, seg, arg_col, false, false, clip_id);
                cx += seg.chars().count() as f32 * m.cw;
                rest = &rest[end + 1..];
                continue;
            }
            // Scan until next bracket, pipe, or `<`.
            let seg_end = rest
                .find(|c: char| matches!(c, '[' | ']' | '|' | '(' | ')' | '<'))
                .unwrap_or(rest.len());
            let seg = &rest[..seg_end];
            let seg_w = seg.chars().count() as f32 * m.cw;
            if ti == 0 && seg == root_cmd.split_whitespace().next().unwrap_or("") {
                scene.text(cx, cy, seg, theme::INK_1, true, false, clip_id);
            } else if seg.starts_with('-') {
                scene.text(cx, cy, seg, flag_col, false, false, clip_id);
                let flag_tok = seg.trim_end_matches('=').to_owned();
                if flag_tok.len() >= 2 {
                    let hit = HitRect::new(cx, cy, cx + seg_w, cy + m.ch);
                    scene.push_hit_clipped(hit, clip_id, HitAction::InsertToken(flag_tok.clone()));
                    scene.push_hover_clipped(
                        hit,
                        clip_id,
                        HoverKind::Tooltip {
                            title: flag_tok,
                            subtitle: Some("click to insert flag".to_owned()),
                        },
                    );
                }
            } else {
                scene.text(cx, cy, seg, theme::INK_2, false, false, clip_id);
            }
            cx += seg_w;
            rest = &rest[seg_end..];
        }
    }
    (cy - y0) + m.ch
}

/// Render a structured [`HelpDoc`] as a native GPU manual card with Usage, runnable Examples,
/// grouped subcommands/flags with aligned columns, inline Markdown chips, and clickable footer links.
#[allow(clippy::too_many_arguments)]
pub fn layout_help_doc(
    scene: &mut Scene,
    doc: &HelpDoc,
    x0: f32,
    y0: f32,
    width: f32,
    top_right_reserve: f32,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    let x1 = x0 + width;
    let bg_idx = scene.rects.len();
    scene.round_rect(
        x0,
        y0,
        x1,
        y0 + 40.0,
        8.0,
        theme::rgb(0x13, 0x13, 0x17),
        1.0,
        theme::LINE,
        clip_id,
    );

    let pad_x = 14.0;
    let inner_x0 = x0 + pad_x;
    let inner_x1 = x1 - pad_x;
    let inner_w = (inner_x1 - inner_x0).max(160.0);
    let mut cur_y = y0;

    // 1. Header bar (`<icon> git   manual & quick reference`).
    let hdr_h = m.ch + 14.0;
    scene.round_rect(
        x0 + 1.0,
        y0 + 1.0,
        x1 - 1.0,
        y0 + hdr_h,
        7.0,
        theme::rgb(0x18, 0x18, 0x1e),
        0.0,
        [0.0; 4],
        clip_id,
    );
    scene.line(x0 + 1.0, y0 + hdr_h, x1 - 1.0, y0 + hdr_h, 1.0, theme::LINE_FAINT, clip_id);
    let hdr_ty = y0 + (hdr_h - m.ch) * 0.5;
    let ic_s = m.icon_sm();
    draw_icon(scene, Icon::Doc, inner_x0, y0 + (hdr_h - ic_s) * 0.5, ic_s, theme::SERIES_BLUE, clip_id);
    let cmd_x = inner_x0 + ic_s + 8.0;
    scene.text(cmd_x, hdr_ty, &doc.command, theme::INK_1, true, false, clip_id);
    let cmd_w = doc.command.chars().count() as f32 * m.cw;
    let sub_x = cmd_x + cmd_w + 10.0;
    let sub_avail = (inner_x1 - top_right_reserve - sub_x - 8.0).max(0.0);
    if sub_avail >= 60.0 {
        let sub_txt = doc.summary.as_deref().unwrap_or("manual & quick reference");
        let max_c = (sub_avail / m.measure("n", TextFace::UiSmall).max(5.5)).floor().max(8.0) as usize;
        scene.ui_small(
            sub_x,
            hdr_ty + 1.0,
            truncate_chars(sub_txt, max_c),
            theme::INK_3,
            false,
            false,
            clip_id,
        );
    }
    cur_y += hdr_h + 10.0;

    let small_ch = m.line_h(TextFace::UiSmall);
    let flag_col = theme::rgb(0x74, 0xb9, 0xff);
    let subcmd_col = theme::rgb(0x7c, 0xd4, 0xa8);
    let warn_col = theme::rgb(0xe8, 0xa8, 0x48);

    // 2. USAGE box.
    if !doc.usage.is_empty() {
        scene.ui_small(inner_x0, cur_y, "USAGE", theme::INK_4, true, false, clip_id);
        cur_y += small_ch + 4.0;

        let u_box_idx = scene.rects.len();
        let u_y0 = cur_y;
        scene.round_rect(
            inner_x0,
            u_y0,
            inner_x1,
            u_y0 + 24.0,
            6.0,
            theme::rgb(0x10, 0x10, 0x13),
            1.0,
            theme::LINE_FAINT,
            clip_id,
        );
        cur_y += 7.0;
        let u_inner_x = inner_x0 + 10.0;
        let u_inner_w = (inner_w - 20.0).max(120.0);
        for uline in &doc.usage {
            let h = layout_usage_line(scene, uline, &doc.command, u_inner_x, cur_y, u_inner_w, m, clip_id);
            cur_y += h + 3.0;
        }
        cur_y += 4.0;
        scene.rects[u_box_idx].rect[3] = cur_y;
        cur_y += 12.0;
    }

    // 3. EXAMPLES card (emerald accent rail, syntax-highlighted commands, click to insert!).
    if !doc.examples.is_empty() {
        scene.ui_small(inner_x0, cur_y, "EXAMPLES", theme::OK, true, false, clip_id);
        let hint = "click to insert";
        let hw = m.measure(hint, TextFace::UiSmall);
        scene.ui_small(inner_x1 - hw, cur_y, hint, theme::INK_4, false, false, clip_id);
        cur_y += small_ch + 4.0;

        let ex_bg_idx = scene.rects.len();
        let ex_y0 = cur_y;
        scene.round_rect(
            inner_x0,
            ex_y0,
            inner_x1,
            ex_y0 + 24.0,
            6.0,
            theme::rgba(0x4c, 0xc2, 0x7a, 0.045),
            1.0,
            theme::rgba(0x4c, 0xc2, 0x7a, 0.20),
            clip_id,
        );
        let ex_rail_idx = scene.rects.len();
        scene.round_rect(
            inner_x0,
            ex_y0,
            inner_x0 + 2.5,
            ex_y0 + 24.0,
            1.25,
            theme::OK,
            0.0,
            [0.0; 4],
            clip_id,
        );
        cur_y += 6.0;

        let ex_in_x = inner_x0 + 11.0;
        let ex_in_w = (inner_x1 - 10.0 - ex_in_x).max(120.0);
        let max_cmd_c = doc
            .examples
            .iter()
            .map(|e| e.cmd.chars().count())
            .max()
            .unwrap_or(16)
            .clamp(16, 36);
        let cmd_col_w = ((max_cmd_c + 3) as f32 * m.cw).min(ex_in_w * 0.56);
        let row_h = m.ch + 5.0;

        for ex in doc.examples.iter().take(6) {
            let row_rect = HitRect::new(inner_x0 + 4.0, cur_y, inner_x1 - 4.0, cur_y + row_h);
            let ty = cur_y + (row_h - m.ch) * 0.5;
            scene.text(ex_in_x, ty, "$ ", theme::INK_4, false, false, clip_id);
            let mut sx = ex_in_x + 2.0 * m.cw;
            let cmd_shown = truncate_chars(
                &ex.cmd,
                ((cmd_col_w - 2.0 * m.cw) / m.cw).floor().max(8.0) as usize,
            );
            let spans = highlight(&cmd_shown, blaze_own::Alloc::global());
            let mut prev_b = 0usize;
            for sp in spans.iter() {
                let s = (sp.span.start as usize).min(cmd_shown.len());
                let e = (sp.span.end as usize).min(cmd_shown.len());
                if s > prev_b {
                    let gap = &cmd_shown[prev_b..s];
                    scene.text(sx, ty, gap, theme::INK_2, false, false, clip_id);
                    sx += gap.chars().count() as f32 * m.cw;
                }
                if e > s {
                    let s_txt = &cmd_shown[s..e];
                    scene.text(sx, ty, s_txt, theme::highlight_color(sp.kind), false, false, clip_id);
                    sx += s_txt.chars().count() as f32 * m.cw;
                }
                prev_b = e;
            }
            if prev_b < cmd_shown.len() {
                let tail = &cmd_shown[prev_b..];
                scene.text(sx, ty, tail, theme::INK_2, false, false, clip_id);
            }
            if !ex.desc.is_empty() {
                let desc_x = ex_in_x + cmd_col_w + 10.0;
                let rem_w = (inner_x1 - 10.0 - desc_x).max(40.0);
                let max_dc = (rem_w / m.cw).floor().max(8.0) as usize;
                scene.text(
                    desc_x,
                    ty,
                    truncate_chars(&ex.desc, max_dc),
                    theme::INK_3,
                    false,
                    false,
                    clip_id,
                );
            }
            scene.push_hit_clipped(row_rect, clip_id, HitAction::InsertToken(ex.cmd.clone()));
            scene.push_hover_clipped(
                row_rect,
                clip_id,
                HoverKind::Tooltip {
                    title: ex.cmd.clone(),
                    subtitle: Some("click to insert example into prompt".to_owned()),
                },
            );
            cur_y += row_h;
        }
        cur_y += 5.0;
        scene.rects[ex_bg_idx].rect[3] = cur_y;
        scene.rects[ex_rail_idx].rect[3] = cur_y;
        cur_y += 12.0;
    }

    // 4. Categorized Sections (commands / options / flags).
    let root_cmd = doc.command.split_whitespace().next().unwrap_or(&doc.command);
    let global_max_name_c = doc
        .sections
        .iter()
        .flat_map(|s| s.items.iter())
        .map(|it| it.name.chars().count())
        .max()
        .unwrap_or(10)
        .clamp(10, 26);
    for (si, sec) in doc.sections.iter().enumerate() {
        if si > 0 {
            cur_y += 10.0;
        }
        // Section heading + optional clickable `see also` pill on the right.
        let heading = sec.title.to_ascii_uppercase();
        scene.ui_small(inner_x0, cur_y, &heading, theme::INK_3, true, false, clip_id);
        let head_w = m.measure_styled(&heading, TextFace::UiSmall, true);
        let mut line_x1 = inner_x1;
        if let Some(see) = &sec.see_also {
            let lbl = format!("{see} ↗");
            let lw = m.measure(&lbl, TextFace::UiSmall) + 12.0;
            let sx0 = (inner_x1 - lw).max(inner_x0 + head_w + 14.0);
            let s_rect = HitRect::new(sx0, cur_y - 1.5, inner_x1, cur_y + small_ch + 1.5);
            scene.round_rect(
                s_rect.x0,
                s_rect.y0,
                s_rect.x1,
                s_rect.y1,
                4.0,
                theme::rgba(0x58, 0x9d, 0xf6, 0.10),
                1.0,
                theme::rgba(0x58, 0x9d, 0xf6, 0.28),
                clip_id,
            );
            scene.ui_small(sx0 + 6.0, cur_y, &lbl, theme::SERIES_BLUE, false, false, clip_id);
            scene.push_hit_clipped(s_rect, clip_id, HitAction::RunCommand(see.clone()));
            scene.push_hover_clipped(
                s_rect,
                clip_id,
                HoverKind::Tooltip {
                    title: see.clone(),
                    subtitle: Some("click to run".to_owned()),
                },
            );
            line_x1 = sx0 - 10.0;
        }
        if inner_x0 + head_w + 12.0 < line_x1 {
            let ly = cur_y + small_ch * 0.5;
            scene.line(inner_x0 + head_w + 10.0, ly, line_x1, ly, 1.0, theme::LINE_FAINT, clip_id);
        }
        cur_y += small_ch + 6.0;

        // Use a uniform left column width across sections so descriptions align on one clean spine.
        let has_subcmd_drill = sec.items.iter().any(|it| !it.is_flag);
        let drill_extra = if has_subcmd_drill { 20.0 } else { 0.0 };
        let left_col_w = ((global_max_name_c as f32 * m.cw) + 18.0 + drill_extra).min(inner_w * 0.42);
        let desc_x = inner_x0 + left_col_w + 10.0;
        let desc_w = (inner_x1 - desc_x).max(100.0);

        for item in &sec.items {
            let name_max_c = ((left_col_w - 14.0 - drill_extra) / m.cw).floor().max(6.0) as usize;
            let shown_name = truncate_chars(&item.name, name_max_c);
            let nw = shown_name.chars().count() as f32 * m.cw;

            // Left clickable command / flag chip.
            let chip_x0 = inner_x0;
            let chip_w = nw + 10.0;
            let chip_rect = HitRect::new(chip_x0, cur_y - 1.0, chip_x0 + chip_w, cur_y + m.ch + 1.0);
            let (chip_bg, chip_border, chip_fg) = if item.is_flag {
                (
                    theme::rgba(0x74, 0xb9, 0xff, 0.09),
                    theme::rgba(0x74, 0xb9, 0xff, 0.24),
                    flag_col,
                )
            } else {
                (
                    theme::rgba(0x7c, 0xd4, 0xa8, 0.08),
                    theme::rgba(0x7c, 0xd4, 0xa8, 0.24),
                    subcmd_col,
                )
            };
            scene.round_rect(
                chip_rect.x0,
                chip_rect.y0,
                chip_rect.x1,
                chip_rect.y1,
                4.0,
                chip_bg,
                1.0,
                chip_border,
                clip_id,
            );
            scene.text(chip_x0 + 5.0, cur_y, &shown_name, chip_fg, !item.is_flag, false, clip_id);

            let insert_str = if item.is_flag {
                primary_flag_token(&item.name)
            } else {
                format!("{root_cmd} {}", item.name.split_whitespace().next().unwrap_or(&item.name))
            };
            scene.push_hit_clipped(chip_rect, clip_id, HitAction::InsertToken(insert_str.clone()));
            scene.push_hover_clipped(
                chip_rect,
                clip_id,
                HoverKind::Tooltip {
                    title: insert_str,
                    subtitle: Some(if item.is_flag {
                        "click to insert flag".to_owned()
                    } else {
                        "click to insert command".to_owned()
                    }),
                },
            );

            // For subcommands, add a crisp `?` micro-button right next to the chip to open its own help!
            if !item.is_flag {
                let sub = item.name.split_whitespace().next().unwrap_or(&item.name);
                let qx0 = chip_rect.x1 + 4.0;
                let qw = m.cw + 6.0;
                if qx0 + qw < desc_x - 4.0 {
                    let q_rect = HitRect::new(qx0, cur_y - 0.5, qx0 + qw, cur_y + m.ch + 0.5);
                    scene.round_rect(
                        q_rect.x0,
                        q_rect.y0,
                        q_rect.x1,
                        q_rect.y1,
                        4.0,
                        theme::BG_CHIP,
                        1.0,
                        theme::LINE_FAINT,
                        clip_id,
                    );
                    scene.text(qx0 + 3.0, cur_y, "?", theme::INK_4, false, false, clip_id);
                    let help_cmd = format!("{root_cmd} {sub} --help");
                    scene.push_hit_clipped(q_rect, clip_id, HitAction::RunCommand(help_cmd.clone()));
                    scene.push_hover_clipped(
                        q_rect,
                        clip_id,
                        HoverKind::Tooltip {
                            title: help_cmd,
                            subtitle: Some("click to inspect subcommand manual".to_owned()),
                        },
                    );
                }
            }

            // Right column: optional badge + inline-Markdown monospace description (renders `` `HEAD` `` chips!).
            let mut desc_indent = 0.0f32;
            if let Some(badge) = &item.badge {
                let bw = m.measure(badge, TextFace::UiSmall) + 10.0;
                let is_exp = badge.eq_ignore_ascii_case("EXPERIMENTAL")
                    || badge.eq_ignore_ascii_case("DEPRECATED");
                let (b_bg, b_bd, b_fg) = if is_exp {
                    (
                        theme::rgba(0xe8, 0xa8, 0x48, 0.14),
                        theme::rgba(0xe8, 0xa8, 0x48, 0.36),
                        warn_col,
                    )
                } else {
                    (theme::BG_CHIP, theme::LINE, theme::INK_3)
                };
                scene.round_rect(
                    desc_x,
                    cur_y,
                    desc_x + bw,
                    cur_y + m.ch - 1.0,
                    4.0,
                    b_bg,
                    1.0,
                    b_bd,
                    clip_id,
                );
                scene.ui_small(desc_x + 5.0, cur_y + 0.5, badge, b_fg, is_exp, false, clip_id);
                desc_indent = bw + 7.0;
            }
            let dh = layout_inline_markdown(
                scene,
                &item.desc,
                desc_x,
                cur_y,
                desc_w,
                desc_indent,
                0.0,
                theme::INK_2,
                m,
                clip_id,
            );
            cur_y += dh.max(m.ch) + 6.0;
        }
    }

    // 5. Footer Notes (promoting single-quoted commands like `'git help -a'` into clickable chips).
    if !doc.notes.is_empty() {
        cur_y += 4.0;
        scene.line(inner_x0, cur_y, inner_x1, cur_y, 1.0, theme::LINE_FAINT, clip_id);
        cur_y += 8.0;
        for note in &doc.notes {
            let promoted = promote_single_quoted_commands(note, &doc.command);
            let nh = layout_inline_markdown(
                scene,
                &promoted,
                inner_x0,
                cur_y,
                inner_w,
                0.0,
                0.0,
                theme::INK_3,
                m,
                clip_id,
            );
            cur_y += nh + 4.0;
        }
    }

    cur_y += 8.0;
    scene.rects[bg_idx].rect[3] = cur_y;
    cur_y - y0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_help_into_sections_badges_examples_and_notes() {
        let sample = [
            "usage: git [-v | --version] [-h | --help] [-C <path>]",
            "           <command> [<args>]",
            "",
            "These are common Git commands used in various situations:",
            "",
            "start a working area (see also: git help tutorial)",
            "   clone      Clone a repository into a new directory",
            "   init       Create an empty Git repository",
            "",
            "grow, mark and tweak your common history",
            "   history    EXPERIMENTAL: Rewrite history",
            "   reset      Set `HEAD` or the index to a known state",
            "",
            "'git help -a' and 'git help -g' list available subcommands.",
        ];
        let doc = parse_help_doc("git --help", &sample).expect("git --help parsed");
        assert_eq!(doc.command, "git");
        assert_eq!(doc.usage.len(), 2);
        assert_eq!(doc.sections.len(), 2);
        assert_eq!(doc.sections[0].title, "start a working area");
        assert_eq!(doc.sections[0].see_also.as_deref(), Some("git help tutorial"));
        assert_eq!(doc.sections[0].items.len(), 2);
        assert_eq!(doc.sections[0].items[0].name, "clone");
        assert_eq!(doc.sections[1].items[0].badge.as_deref(), Some("EXPERIMENTAL"));
        assert_eq!(doc.sections[1].items[0].desc, "Rewrite history");
        assert!(!doc.examples.is_empty(), "curated git examples attached");
        assert_eq!(doc.notes.len(), 1);

        let mut scene = Scene::new();
        let h = layout_help_doc(&mut scene, &doc, 16.0, 20.0, 820.0, 100.0, CellMetrics::default(), 0);
        assert!(h > 200.0);
        // Should emit RunCommand for `git help tutorial` and `git clone --help`, plus CopyText for `HEAD`.
        assert!(scene.hit_regions.iter().any(|r| r.action == HitAction::RunCommand("git help tutorial".into())));
        assert!(scene.hit_regions.iter().any(|r| r.action == HitAction::RunCommand("git clone --help".into())));
        assert!(scene.hit_regions.iter().any(|r| r.action == HitAction::CopyText("HEAD".into())));
        assert!(scene.hit_regions.iter().any(|r| r.action == HitAction::InsertToken("git status".into())));
    }

    #[test]
    fn ignores_normal_non_help_stdout() {
        let lines = ["hello world", "foo bar", "baz qux", "done"];
        assert!(parse_help_doc("echo hello", &lines).is_none());
    }
}
