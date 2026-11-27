//! Pure `#![no_std]`, zero-unsafe, sans-IO structured CLI Help & Man-Page parser.
//!
//! Turns raw `--help` / `-h` / `help` / `man` output (`git --help`, `rg --help`,
//! `blaze --help`, `cargo --help`, GNU/clap/cobra/man pages) into a structured [`HelpDoc`]
//! with usage synopses, categorized subcommand/option sections, status/default badges,
//! runnable examples (parsed from `EXAMPLES:` or curated TLDR defaults), and footer notes.

use blaze_own::{AString, AVec, Alloc, String, Vec};

/// Structured representation of a parsed CLI `--help` or `man` page.
#[derive(Debug, PartialEq, Eq)]
pub struct HelpDoc<'a> {
    /// Root command / subcommand title (e.g. `"git"`, `"git switch"`, `"rg"`, `"blaze"`).
    pub command: AString<'a>,
    /// Optional introductory summary paragraph above the sections.
    pub summary: Option<AString<'a>>,
    /// Usage / synopsis lines (e.g. `["git [-v | --version] ... <command> [<args>]"]`).
    pub usage: AVec<'a, AString<'a>>,
    /// Categorized command / option / flag sections.
    pub sections: AVec<'a, HelpSection<'a>>,
    /// Runnable practical examples (from tool output or curated TLDR examples).
    pub examples: AVec<'a, HelpExample<'a>>,
    /// Trailing footer notes / references.
    pub notes: AVec<'a, AString<'a>>,
}

/// One categorized section of subcommands, options, or flags in a [`HelpDoc`].
#[derive(Debug, PartialEq, Eq)]
pub struct HelpSection<'a> {
    pub title: AString<'a>,
    /// Optional `(see also: <cmd>)` reference extracted from the section heading.
    pub see_also: Option<AString<'a>>,
    pub items: AVec<'a, HelpItem<'a>>,
}

/// One subcommand or flag row inside a [`HelpSection`].
#[derive(Debug, PartialEq, Eq)]
pub struct HelpItem<'a> {
    /// Subcommand or flag syntax (e.g. `"switch"`, `"-C <path>"`, `"-p, --paginate"`).
    pub name: AString<'a>,
    pub is_flag: bool,
    pub desc: AString<'a>,
    /// Optional status/default pill badge (e.g. `"EXPERIMENTAL"`, `"default: auto"`).
    pub badge: Option<AString<'a>>,
}

/// One runnable example command and its description.
#[derive(Debug, PartialEq, Eq)]
pub struct HelpExample<'a> {
    pub cmd: AString<'a>,
    pub desc: AString<'a>,
}

enum CleanLine<'s, 'a> {
    Borrowed(&'s str),
    Owned(AString<'a>),
}

impl<'s, 'a> CleanLine<'s, 'a> {
    fn as_str(&self) -> &str {
        match self {
            Self::Borrowed(s) => s,
            Self::Owned(s) => s.as_str(),
        }
    }
}

/// Strip ANSI CSI/OSC escape sequences so colored `--help` output parses cleanly.
fn strip_ansi_in<'s, 'a>(s: &'s str, alloc: Alloc<'a>) -> CleanLine<'s, 'a> {
    if !s.contains('\x1b') {
        return CleanLine::Borrowed(s);
    }
    let mut out = String::with_capacity_in(s.len(), alloc);
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
        let Some(ch) = s[i..].chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }
    CleanLine::Owned(out)
}

fn starts_with_ascii_ci(haystack: &str, prefix: &str) -> bool {
    let h = haystack.as_bytes();
    let p = prefix.as_bytes();
    h.len() >= p.len() && h[..p.len()].eq_ignore_ascii_case(p)
}

fn contains_ascii_ci(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || n.len() > h.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// True when `cmd_source` is a direct help/man invocation (not piped into another command).
pub fn is_help_invocation(cmd_source: &str) -> bool {
    let trimmed = cmd_source.trim();
    if trimmed.is_empty() || trimmed.contains('|') || trimmed.contains('>') {
        return false;
    }
    let mut toks = trimmed.split_whitespace();
    let Some(first) = toks.next() else {
        return false;
    };
    if first == "man" || first == "tldr" {
        return toks.next().is_some();
    }
    let Some(second) = toks.next() else {
        return false;
    };
    if matches!(second, "help" | "--help" | "-h" | "-help") {
        return true;
    }
    toks.any(|t| matches!(t, "--help" | "-h" | "-help"))
}

/// Extract clean base command name from the user's invocation
/// (e.g. `"git switch --help"` -> `"git switch"`).
fn extract_command_name_in<'a>(
    cmd_source: &str,
    usage: &[AString<'a>],
    alloc: Alloc<'a>,
) -> AString<'a> {
    let mut toks = cmd_source.split_whitespace();
    if let Some(first) = toks.next() {
        let mut out = String::new_in(alloc);
        let mut count = 0usize;
        let skip_first = matches!(first, "man" | "tldr");
        let mut push_part = |t: &str| -> bool {
            if matches!(t, "--help" | "-h" | "-help" | "help") {
                return true;
            }
            if t.starts_with('-') {
                return false;
            }
            if count > 0 {
                out.push(' ');
            }
            out.push_str(t);
            count += 1;
            count < 2
        };
        let mut keep_going = true;
        if !skip_first {
            keep_going = push_part(first);
        }
        if keep_going {
            for t in toks {
                if !push_part(t) {
                    break;
                }
            }
        }
        if count > 0 {
            return out;
        }
    }
    if let Some(first_u) = usage.first()
        && let Some(word) = first_u.as_str().split_whitespace().next()
    {
        return String::from_str_in(word, alloc);
    }
    String::from_str_in("command", alloc)
}

/// Split an indented two-column help row (`   clone      Clone a repo`, `  -v, --verbose  Desc`)
/// into `(name, description)`.
fn split_help_item_line(raw: &str) -> Option<(&str, &str)> {
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
    Some((name, desc))
}

/// Extract optional `(see also: <cmd>)` suffix from a section heading (e.g. git's headings).
fn parse_section_heading(raw: &str) -> Option<(&str, Option<&str>)> {
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
        let title = trimmed[..open].trim().trim_end_matches(':');
        let see = trimmed[open + "(see also:".len()..trimmed.len() - 1].trim();
        if !title.is_empty() {
            return Some((title, if see.is_empty() { None } else { Some(see) }));
        }
    }
    // Standard colon heading (`Commands:`, `Options:`, `Flags:`, `Arguments:`, `Examples:`).
    if let Some(stem) = trimmed.strip_suffix(':') {
        let s = stem.trim();
        let words = s.split_whitespace().count();
        if !s.is_empty() && words <= 6 {
            return Some((s, None));
        }
    }
    // ALL-CAPS man-page heading (`NAME`, `SYNOPSIS`, `OPTIONS`, `COMMANDS`, `EXAMPLES`).
    if trimmed.len() <= 32
        && trimmed.bytes().any(|b| b.is_ascii_uppercase())
        && trimmed
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b == b' ' || b == b'-' || b == b'/')
    {
        return Some((trimmed, None));
    }
    // Short unindented category line directly preceding indented items
    // (e.g. `grow, mark and tweak your common history`).
    if trimmed.split_whitespace().count() <= 9 && !trimmed.contains("  ") {
        return Some((trimmed, None));
    }
    None
}

/// Extract optional leading `EXPERIMENTAL:` / `DEPRECATED:` or trailing `[default: ...]` badge.
fn extract_item_badge(raw_desc: &str) -> (&str, Option<&str>) {
    let d = raw_desc.trim();
    for prefix in ["EXPERIMENTAL:", "DEPRECATED:", "BETA:", "UNSTABLE:"] {
        if let Some(rest) = d.strip_prefix(prefix) {
            return (rest.trim_start(), Some(prefix.trim_end_matches(':')));
        }
    }
    if let Some(open) = d.rfind("[default:")
        && d.ends_with(']')
    {
        let badge = d[open + 1..d.len() - 1].trim();
        let clean = d[..open].trim_end();
        if !clean.is_empty() && badge.len() <= 28 {
            return (clean, Some(badge));
        }
    }
    (d, None)
}

/// Parse a CLI `--help` / `-h` / `man` output into a structured [`HelpDoc`] allocated in `alloc`.
/// Returns `None` if the text does not look like structured help output.
pub fn parse_help_doc_in<'a>(
    cmd_source: &str,
    raw_lines: &[&str],
    alloc: Alloc<'a>,
) -> Option<HelpDoc<'a>> {
    if raw_lines.len() < 4 {
        return None;
    }
    let explicit_help = is_help_invocation(cmd_source);
    let mut clean_lines: AVec<'a, CleanLine<'_, 'a>> = Vec::with_capacity_in(raw_lines.len(), alloc);
    for &l in raw_lines {
        clean_lines.push(strip_ansi_in(l, alloc));
    }

    let mut usage: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    let mut summary = String::new_in(alloc);
    let mut sections: AVec<'a, HelpSection<'a>> = Vec::new_in(alloc);
    let mut examples: AVec<'a, HelpExample<'a>> = Vec::new_in(alloc);
    let mut notes: AVec<'a, AString<'a>> = Vec::new_in(alloc);

    let mut in_usage = false;
    let mut in_examples = false;
    let mut pending_example_desc: Option<AString<'a>> = None;

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
        if !raw.starts_with(' ')
            && (starts_with_ascii_ci(trimmed, "usage:")
                || trimmed.eq_ignore_ascii_case("synopsis")
                || trimmed.eq_ignore_ascii_case("synopsis:"))
        {
            in_usage = true;
            in_examples = false;
            if let Some((_, rest)) = trimmed.split_once(':') {
                let r = rest.trim();
                if !r.is_empty() {
                    usage.push(String::from_str_in(r, alloc));
                }
            }
            i += 1;
            continue;
        }
        if in_usage && (raw.starts_with(' ') || raw.starts_with('\t')) {
            usage.push(String::from_str_in(trimmed, alloc));
            i += 1;
            continue;
        }
        in_usage = false;

        // 2. Indented `EXAMPLES` section parsing.
        if in_examples && (raw.starts_with(' ') || raw.starts_with('\t')) {
            if let Some(comment) = trimmed.strip_prefix('#') {
                pending_example_desc = Some(String::from_str_in(comment.trim(), alloc));
            } else if let Some((cmd_part, desc_part)) = split_help_item_line(raw) {
                examples.push(HelpExample {
                    cmd: String::from_str_in(cmd_part.trim_start_matches("$ "), alloc),
                    desc: String::from_str_in(desc_part, alloc),
                });
            } else if let Some(cmd_s) = trimmed
                .strip_prefix("$ ")
                .or_else(|| trimmed.strip_prefix("> "))
            {
                examples.push(HelpExample {
                    cmd: String::from_str_in(cmd_s.trim(), alloc),
                    desc: pending_example_desc
                        .take()
                        .unwrap_or_else(|| String::new_in(alloc)),
                });
            } else if let Some(desc) = pending_example_desc.take() {
                examples.push(HelpExample {
                    cmd: String::from_str_in(trimmed, alloc),
                    desc,
                });
            } else if let Some(last) = examples.last_mut()
                && last.desc.is_empty()
            {
                last.desc = String::from_str_in(trimmed, alloc);
            } else {
                examples.push(HelpExample {
                    cmd: String::from_str_in(trimmed, alloc),
                    desc: String::new_in(alloc),
                });
            }
            i += 1;
            continue;
        }

        // 3. Section headings (only treat unindented heading as a section if followed by indented
        //    rows or explicitly a known section header like `Options:` / `Commands:` / `Examples:`).
        if !raw.starts_with(' ')
            && !raw.starts_with('\t')
            && let Some((title, see_also)) = parse_section_heading(raw)
        {
            let next_nonempty = clean_lines[i + 1..]
                .iter()
                .find(|l| !l.as_str().trim().is_empty());
            let next_is_indented = next_nonempty
                .is_some_and(|l| l.as_str().starts_with(' ') || l.as_str().starts_with('\t'));
            if title.eq_ignore_ascii_case("examples") || title.eq_ignore_ascii_case("example") {
                in_examples = true;
                pending_example_desc = None;
                i += 1;
                continue;
            }
            if next_is_indented
                && let Some(next_line) = next_nonempty
            {
                // Make sure the next indented line actually looks like a help item
                // (not an arbitrary indented quote).
                let next_str = next_line.as_str();
                if split_help_item_line(next_str).is_some()
                    || contains_ascii_ci(title, "command")
                    || contains_ascii_ci(title, "option")
                    || contains_ascii_ci(title, "flag")
                    || contains_ascii_ci(title, "argument")
                    || see_also.is_some()
                {
                    in_examples = false;
                    sections.push(HelpSection {
                        title: String::from_str_in(title, alloc),
                        see_also: see_also.map(|s| String::from_str_in(s, alloc)),
                        items: Vec::new_in(alloc),
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
                let (desc, badge) = extract_item_badge(raw_desc);
                if sections.is_empty() {
                    sections.push(HelpSection {
                        title: String::from_str_in(
                            if is_flag { "Options" } else { "Commands" },
                            alloc,
                        ),
                        see_also: None,
                        items: Vec::new_in(alloc),
                    });
                }
                if let Some(sec) = sections.last_mut() {
                    sec.items.push(HelpItem {
                        name: String::from_str_in(name, alloc),
                        is_flag,
                        desc: String::from_str_in(desc, alloc),
                        badge: badge.map(|b| String::from_str_in(b, alloc)),
                    });
                }
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
                if item.badge.is_none() {
                    let (clean_d, new_badge) = extract_item_badge(item.desc.as_str());
                    if let Some(b) = new_badge {
                        let badge_owned = String::from_str_in(b, alloc);
                        let clean_owned = String::from_str_in(clean_d, alloc);
                        item.desc = clean_owned;
                        item.badge = Some(badge_owned);
                    }
                }
                i += 1;
                continue;
            }
        }

        // 5. Unindented intro prose or trailing footer notes.
        in_examples = false;
        if sections.is_empty() {
            if !summary.is_empty() {
                summary.push(' ');
            }
            summary.push_str(trimmed);
        } else {
            // Join consecutive non-blank footer lines into clean paragraphs.
            if let Some(last_note) = notes.last_mut()
                && i > 0
                && !clean_lines[i - 1].as_str().trim().is_empty()
                && !trimmed.starts_with('\'')
            {
                last_note.push(' ');
                last_note.push_str(trimmed);
            } else {
                notes.push(String::from_str_in(trimmed, alloc));
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

    let command = extract_command_name_in(cmd_source, usage.as_slice(), alloc);
    if examples.is_empty() {
        examples = curated_examples_for_in(command.as_str(), alloc);
    }

    let summary = if summary.is_empty() {
        None
    } else {
        Some(summary)
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
pub fn curated_examples_for_in<'a>(command: &str, alloc: Alloc<'a>) -> AVec<'a, HelpExample<'a>> {
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
            (
                "curl -X POST -H \"Content-Type: application/json\" -d '{}' <url>",
                "Send JSON POST request",
            ),
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
    let mut out = Vec::with_capacity_in(pairs.len(), alloc);
    for &(cmd, desc) in pairs {
        out.push(HelpExample {
            cmd: String::from_str_in(cmd, alloc),
            desc: String::from_str_in(desc, alloc),
        });
    }
    out
}

/// Promote single-quoted CLI invocations like `'git help -a'` in footer notes into Markdown
/// inline `` `code` `` spans so they render as clickable chips.
pub fn promote_single_quoted_commands_in<'a>(
    text: &str,
    root_cmd: &str,
    alloc: Alloc<'a>,
) -> AString<'a> {
    let root = root_cmd.split_whitespace().next().unwrap_or(root_cmd);
    let mut out = String::with_capacity_in(text.len() + 16, alloc);
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
                let matches_root = !root.is_empty()
                    && (inner == root
                        || inner
                            .strip_prefix(root)
                            .is_some_and(|rest| rest.starts_with(' ')));
                if matches_root || inner.starts_with('-') {
                    out.push('`');
                    out.push_str(inner);
                    out.push('`');
                    i = i + 1 + rel_end + 1;
                    continue;
                }
            }
        }
        let Some(ch) = text[i..].chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Primary flag token to insert into composer when clicking a flag spec
/// (`"-p, --paginate"` -> `"--paginate"`, `"--color=<when>"` -> `"--color"`).
pub fn primary_flag_token(spec: &str) -> &str {
    for part in spec.split([',', '|']) {
        let tok = part.split_whitespace().next().unwrap_or("").trim();
        if tok.starts_with("--") {
            return tok.split('=').next().unwrap_or(tok);
        }
    }
    spec.split_whitespace()
        .next()
        .unwrap_or(spec)
        .trim_end_matches(',')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_help_and_man_invocations() {
        assert!(is_help_invocation("git --help"));
        assert!(is_help_invocation("git -h"));
        assert!(is_help_invocation("git help switch"));
        assert!(is_help_invocation("git switch --help"));
        assert!(is_help_invocation("go -help"));
        assert!(is_help_invocation("man git"));
        assert!(is_help_invocation("tldr tar"));

        assert!(!is_help_invocation(""));
        assert!(!is_help_invocation("man"));
        assert!(!is_help_invocation("tldr"));
        assert!(!is_help_invocation("git status"));
        assert!(!is_help_invocation("git --help | head -n 5"));
        assert!(!is_help_invocation("git --help > out.txt"));
    }

    #[test]
    fn parses_git_help_into_sections_badges_examples_and_notes() {
        let alloc = blaze_own::testing::heap().alloc();
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
        let doc = parse_help_doc_in("git --help", &sample, alloc).expect("git --help parsed");
        assert_eq!(doc.command.as_str(), "git");
        assert_eq!(
            doc.summary.as_ref().map(AString::as_str),
            Some("These are common Git commands used in various situations:")
        );
        assert_eq!(doc.usage.len(), 2);
        assert_eq!(doc.sections.len(), 2);
        assert_eq!(doc.sections[0].title.as_str(), "start a working area");
        assert_eq!(
            doc.sections[0].see_also.as_ref().map(AString::as_str),
            Some("git help tutorial")
        );
        assert_eq!(doc.sections[0].items.len(), 2);
        assert_eq!(doc.sections[0].items[0].name.as_str(), "clone");
        assert!(!doc.sections[0].items[0].is_flag);
        assert_eq!(
            doc.sections[1].items[0].badge.as_ref().map(AString::as_str),
            Some("EXPERIMENTAL")
        );
        assert_eq!(doc.sections[1].items[0].desc.as_str(), "Rewrite history");
        assert!(!doc.examples.is_empty(), "curated git examples attached");
        assert_eq!(doc.examples[0].cmd.as_str(), "git status");
        assert_eq!(doc.notes.len(), 1);
    }

    #[test]
    fn parses_clap_gnu_help_with_ansi_and_default_badges() {
        let alloc = blaze_own::testing::heap().alloc();
        let sample = [
            "\x1b[1mripgrep\x1b[0m — recursively search directories for a regex pattern",
            "",
            "\x1b[1;32mUsage:\x1b[0m rg [OPTIONS] PATTERN [PATH ...]",
            "",
            "\x1b[1;32mOptions:\x1b[0m",
            "  -i, --ignore-case       Case insensitive search",
            "      --color <WHEN>      Controls when to use color",
            "                          across output lines [default: auto]",
            "  -L, --follow            DEPRECATED: Follow symbolic links",
            "",
            "Commands:",
            "  search                  Run a search query",
            "  files                   List candidate files",
        ];
        let doc = parse_help_doc_in("rg --help", &sample, alloc).expect("clap/GNU help parsed");
        assert_eq!(doc.command.as_str(), "rg");
        assert_eq!(
            doc.summary.as_ref().map(AString::as_str),
            Some("ripgrep — recursively search directories for a regex pattern")
        );
        assert_eq!(doc.usage.len(), 1);
        assert_eq!(doc.usage[0].as_str(), "rg [OPTIONS] PATTERN [PATH ...]");
        assert_eq!(doc.sections.len(), 2);

        let opts = &doc.sections[0];
        assert_eq!(opts.title.as_str(), "Options");
        assert_eq!(opts.items.len(), 3);
        assert!(opts.items[0].is_flag);
        assert_eq!(opts.items[0].name.as_str(), "-i, --ignore-case");

        // Badge extracted from wrapped continuation line.
        assert_eq!(
            opts.items[1].desc.as_str(),
            "Controls when to use color across output lines"
        );
        assert_eq!(
            opts.items[1].badge.as_ref().map(AString::as_str),
            Some("default: auto")
        );

        // Leading DEPRECATED: badge stripped cleanly.
        assert_eq!(
            opts.items[2].badge.as_ref().map(AString::as_str),
            Some("DEPRECATED")
        );
        assert_eq!(opts.items[2].desc.as_str(), "Follow symbolic links");

        let cmds = &doc.sections[1];
        assert_eq!(cmds.title.as_str(), "Commands");
        assert_eq!(cmds.items.len(), 2);
        assert!(!cmds.items[0].is_flag);
        assert_eq!(cmds.items[0].name.as_str(), "search");
    }

    #[test]
    fn parses_explicit_examples_section_overriding_curated() {
        let alloc = blaze_own::testing::heap().alloc();
        let sample = [
            "Usage: mytool [OPTIONS] <INPUT>",
            "",
            "Options:",
            "  -v, --verbose      Enable verbose logging [default: false]",
            "  -o, --output FILE  Write output to FILE",
            "",
            "EXAMPLES:",
            "  # Run with verbose logging",
            "  $ mytool -v input.txt",
            "  mytool -o out.txt    Write result to out.txt",
            "  > mytool --check",
            "  Check syntax only",
        ];
        let doc = parse_help_doc_in("mytool --help", &sample, alloc).expect("parsed examples");
        assert_eq!(doc.examples.len(), 3);
        assert_eq!(doc.examples[0].cmd.as_str(), "mytool -v input.txt");
        assert_eq!(doc.examples[0].desc.as_str(), "Run with verbose logging");
        assert_eq!(doc.examples[1].cmd.as_str(), "mytool -o out.txt");
        assert_eq!(doc.examples[1].desc.as_str(), "Write result to out.txt");
        assert_eq!(doc.examples[2].cmd.as_str(), "mytool --check");
        assert_eq!(doc.examples[2].desc.as_str(), "Check syntax only");
    }

    #[test]
    fn curated_examples_returns_entries_for_known_and_empty_for_unknown() {
        let alloc = blaze_own::testing::heap().alloc();
        let git_ex = curated_examples_for_in("git", alloc);
        assert!(git_ex.len() >= 4);
        assert_eq!(git_ex[0].cmd.as_str(), "git status");

        let sub_ex = curated_examples_for_in("git switch", alloc);
        assert_eq!(sub_ex.len(), 3);

        let unknown = curated_examples_for_in("nonexistent-cli-xyz", alloc);
        assert!(unknown.is_empty());
    }

    #[test]
    fn promotes_single_quoted_commands_without_touching_contractions() {
        let alloc = blaze_own::testing::heap().alloc();
        let note = "Run 'git help -a' or 'git' with '-v'; don't touch 'other text' or it's fine.";
        let promoted = promote_single_quoted_commands_in(note, "git switch", alloc);
        assert_eq!(
            promoted.as_str(),
            "Run `git help -a` or `git` with `-v`; don't touch 'other text' or it's fine."
        );
    }

    #[test]
    fn extracts_primary_flag_token_zero_alloc() {
        assert_eq!(primary_flag_token("-p, --paginate"), "--paginate");
        assert_eq!(primary_flag_token("-f | --force"), "--force");
        assert_eq!(primary_flag_token("--color=<when>"), "--color");
        assert_eq!(primary_flag_token("-c, --config=<path>"), "--config");
        assert_eq!(primary_flag_token("-C <path>"), "-C");
        assert_eq!(primary_flag_token("-v,"), "-v");
        assert_eq!(primary_flag_token(""), "");
    }

    #[test]
    fn rejects_normal_non_help_output() {
        let alloc = blaze_own::testing::heap().alloc();
        let lines = ["hello world", "foo bar", "baz qux", "done"];
        assert!(parse_help_doc_in("echo hello", &lines, alloc).is_none());

        // Too few lines even with explicit --help.
        let short = ["usage: foo", "  -h  help"];
        assert!(parse_help_doc_in("foo --help", &short, alloc).is_none());

        // Non-help invocation with usage but fewer than 3 items is rejected.
        let sparse = [
            "usage: foo [OPTIONS]",
            "",
            "Options:",
            "  -a  Alpha",
            "  -b  Beta",
        ];
        assert!(parse_help_doc_in("foo", &sparse, alloc).is_none());
        // Explicit help invocation with the same text succeeds.
        assert!(parse_help_doc_in("foo --help", &sparse, alloc).is_some());
    }
}
