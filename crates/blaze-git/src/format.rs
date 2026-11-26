//! Zero-allocation Starship-compatible prompt formatter over [`core::fmt::Write`].

#![blaze::sans_io]

use core::fmt;

use crate::{Divergence, FileStatus, Head, PromptStatus, RepoState, StatusEntry};

/// Symbols and toggles controlling how [`format_status`] renders a [`PromptStatus`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PromptStyle {
    pub branch_prefix: &'static str,
    pub detached_prefix: &'static str,
    pub tag_prefix: &'static str,
    pub ahead: &'static str,
    pub behind: &'static str,
    pub diverged: &'static str,
    pub conflicted: &'static str,
    pub stashed: &'static str,
    pub deleted: &'static str,
    pub modified: &'static str,
    pub staged: &'static str,
    pub typechanged: &'static str,
    pub untracked: &'static str,
    pub show_counts: bool,
}

impl PromptStyle {
    /// Starship's default Unicode symbol set.
    pub const STARSHIP: PromptStyle = PromptStyle {
        branch_prefix: "",
        detached_prefix: "@",
        tag_prefix: "#",
        ahead: "⇡",
        behind: "⇣",
        diverged: "⇕",
        conflicted: "=",
        stashed: "$",
        deleted: "✘",
        modified: "!",
        staged: "+",
        typechanged: "»",
        untracked: "?",
        show_counts: true,
    };

    /// Pure ASCII symbol set for minimal terminals.
    pub const ASCII: PromptStyle = PromptStyle {
        branch_prefix: "",
        detached_prefix: "@",
        tag_prefix: "#",
        ahead: "^",
        behind: "v",
        diverged: "<>",
        conflicted: "=",
        stashed: "$",
        deleted: "x",
        modified: "!",
        staged: "+",
        typechanged: "~",
        untracked: "?",
        show_counts: true,
    };
}

/// Format `status` into `out` in Starship style (e.g. `main [⇡1!2+1?3$1]`) with zero allocations.
pub fn format_status(
    status: &PromptStatus<'_>,
    style: &PromptStyle,
    out: &mut impl fmt::Write,
) -> fmt::Result {
    match &status.head {
        Head::Branch { name, .. } => {
            out.write_str(style.branch_prefix)?;
            out.write_str(name.as_str())?;
        }
        Head::Detached { short, .. } => {
            out.write_str(style.detached_prefix)?;
            out.write_str(short.as_str())?;
            if let Some(tag) = &status.tag {
                out.write_str(" ")?;
                out.write_str(style.tag_prefix)?;
                out.write_str(tag.as_str())?;
            }
        }
    }

    if let Some(label) = status.state.label() {
        out.write_str(" (")?;
        out.write_str(label)?;
        match status.state {
            RepoState::Rebase { step: Some(s), total: Some(t), .. }
            | RepoState::Am { step: Some(s), total: Some(t) }
            | RepoState::AmOrRebase { step: Some(s), total: Some(t) } => {
                write!(out, " {s}/{t}")?;
            }
            _ => {}
        }
        out.write_str(")")?;
    }

    let c = status.counts;
    let div = status.upstream.as_ref().map(|u| u.divergence);
    let ahead = div.map_or(0, Divergence::ahead);
    let behind = div.map_or(0, Divergence::behind);
    let staged = c.staged();

    let has_status = c.conflicted > 0
        || ahead > 0
        || behind > 0
        || c.stashed > 0
        || c.deleted > 0
        || c.modified > 0
        || staged > 0
        || c.typechanged > 0
        || c.untracked > 0;

    if has_status {
        out.write_str(" [")?;
        write_token(out, style.conflicted, c.conflicted, style.show_counts)?;
        if ahead > 0 && behind > 0 {
            out.write_str(style.diverged)?;
            write_token(out, style.ahead, ahead, style.show_counts)?;
            write_token(out, style.behind, behind, style.show_counts)?;
        } else {
            write_token(out, style.ahead, ahead, style.show_counts)?;
            write_token(out, style.behind, behind, style.show_counts)?;
        }
        write_token(out, style.stashed, c.stashed, style.show_counts)?;
        write_token(out, style.deleted, c.deleted, style.show_counts)?;
        write_token(out, style.modified, c.modified, style.show_counts)?;
        write_token(out, style.staged, staged, style.show_counts)?;
        write_token(out, style.typechanged, c.typechanged, style.show_counts)?;
        write_token(out, style.untracked, c.untracked, style.show_counts)?;
        out.write_str("]")?;
    }

    Ok(())
}

fn write_token(out: &mut impl fmt::Write, sym: &str, count: u32, show_counts: bool) -> fmt::Result {
    if count == 0 {
        return Ok(());
    }
    out.write_str(sym)?;
    if show_counts && count > 1 {
        write!(out, "{count}")?;
    }
    Ok(())
}

/// Format `status` in Git's `--porcelain=v1` format (with optional `-b` branch header).
pub fn format_porcelain_v1(
    status: &PromptStatus<'_>,
    show_branch: bool,
    out: &mut impl fmt::Write,
) -> fmt::Result {
    format_short_status(status, show_branch, false, out)
}

/// Format `status` in Git's `--short` (`-s`) format, with optional `-b` branch header and ANSI colors.
pub fn format_short_status(
    status: &PromptStatus<'_>,
    show_branch: bool,
    color: bool,
    out: &mut impl fmt::Write,
) -> fmt::Result {
    const GREEN: &str = "\x1b[32m";
    const RED: &str = "\x1b[31m";
    const RESET: &str = "\x1b[0m";

    if show_branch {
        out.write_str("## ")?;
        match &status.head {
            Head::Branch { name, oid, .. } => {
                if oid.is_none() {
                    out.write_str("No commits yet on ")?;
                }
                if color {
                    out.write_str(GREEN)?;
                }
                out.write_str(name.as_str())?;
                if color {
                    out.write_str(RESET)?;
                }
                if let Some(up) = &status.upstream {
                    out.write_str("...")?;
                    if color {
                        out.write_str(RED)?;
                    }
                    if !up.remote.is_empty() && up.remote.as_str() != "." {
                        out.write_str(up.remote.as_str())?;
                        out.write_str("/")?;
                    }
                    out.write_str(up.branch.as_str())?;
                    if color {
                        out.write_str(RESET)?;
                    }
                    write_short_divergence(up.divergence, out)?;
                }
            }
            Head::Detached { .. } => {
                out.write_str("HEAD (no branch)")?;
            }
        }
        out.write_str("\n")?;
    }

    // Git `--short` / `--porcelain=v1` emits tracked/unmerged entries (sorted by path) first,
    // followed by untracked (`??`) entries (sorted by path).
    for pass_untracked in [false, true] {
        for entry in status.entries.iter() {
            if entry.is_untracked() != pass_untracked {
                continue;
            }
            let x = entry.index.code();
            let y = entry.worktree.code();
            if color {
                if entry.is_untracked() || entry.unmerged {
                    write!(out, "{RED}{x}{y}{RESET} ")?;
                } else {
                    if x != ' ' {
                        write!(out, "{GREEN}{x}{RESET}")?;
                    } else {
                        out.write_str(" ")?;
                    }
                    if y != ' ' {
                        write!(out, "{RED}{y}{RESET}")?;
                    } else {
                        out.write_str(" ")?;
                    }
                    out.write_str(" ")?;
                }
            } else {
                write!(out, "{x}{y} ")?;
            }
            write_git_path(entry.path.as_str(), out)?;
            out.write_str("\n")?;
        }
    }
    Ok(())
}

/// Format `status` in human-readable `git status` long format with optional ANSI colors.
pub fn format_long_status(
    status: &PromptStatus<'_>,
    color: bool,
    out: &mut impl fmt::Write,
) -> fmt::Result {
    const GREEN: &str = "\x1b[32m";
    const RED: &str = "\x1b[31m";
    const RESET: &str = "\x1b[0m";

    match &status.head {
        Head::Branch { name, oid, .. } => {
            writeln!(out, "On branch {}", name.as_str())?;
            if oid.is_none() {
                writeln!(out, "\nNo commits yet")?;
            }
        }
        Head::Detached { short, .. } => {
            writeln!(out, "HEAD detached at {}", short.as_str())?;
        }
    }

    if let Some(up) = &status.upstream {
        let mut up_name_has_remote = !up.remote.is_empty() && up.remote.as_str() != ".";
        if up.branch.is_empty() {
            up_name_has_remote = false;
        }
        match up.divergence {
            Divergence::Equal => {
                out.write_str("Your branch is up to date with '")?;
                write_upstream_name(up.remote.as_str(), up.branch.as_str(), up_name_has_remote, out)?;
                out.write_str("'.\n")?;
            }
            Divergence::Counted { ahead, behind: 0 } => {
                out.write_str("Your branch is ahead of '")?;
                write_upstream_name(up.remote.as_str(), up.branch.as_str(), up_name_has_remote, out)?;
                writeln!(
                    out,
                    "' by {ahead} {}.",
                    if ahead == 1 { "commit" } else { "commits" }
                )?;
            }
            Divergence::Counted { ahead: 0, behind } => {
                out.write_str("Your branch is behind '")?;
                write_upstream_name(up.remote.as_str(), up.branch.as_str(), up_name_has_remote, out)?;
                writeln!(
                    out,
                    "' by {behind} {}, and can be fast-forwarded.",
                    if behind == 1 { "commit" } else { "commits" }
                )?;
            }
            Divergence::Counted { ahead, behind } => {
                out.write_str("Your branch and '")?;
                write_upstream_name(up.remote.as_str(), up.branch.as_str(), up_name_has_remote, out)?;
                writeln!(
                    out,
                    "' have diverged,\nand have {ahead} and {behind} different commits each, respectively."
                )?;
            }
            Divergence::Capped { ahead_at_least, behind_at_least } => {
                out.write_str("Your branch and '")?;
                write_upstream_name(up.remote.as_str(), up.branch.as_str(), up_name_has_remote, out)?;
                writeln!(
                    out,
                    "' differ by at least {ahead_at_least} ahead and {behind_at_least} behind."
                )?;
            }
            Divergence::UpstreamMissing => {
                out.write_str("Your branch is based on '")?;
                write_upstream_name(up.remote.as_str(), up.branch.as_str(), up_name_has_remote, out)?;
                out.write_str("', but the upstream is gone.\n")?;
            }
            Divergence::Unborn | Divergence::Skipped => {}
        }
    }

    if let Some(label) = status.state.label() {
        writeln!(out, "You are currently in the middle of a {label}.")?;
    }

    let mut has_staged = false;
    let mut has_unmerged = false;
    let mut has_unstaged = false;
    let mut has_untracked = false;
    for e in status.entries.iter() {
        if e.unmerged {
            has_unmerged = true;
        } else if e.is_untracked() {
            has_untracked = true;
        } else {
            if e.is_staged() {
                has_staged = true;
            }
            if e.is_unstaged() {
                has_unstaged = true;
            }
        }
    }

    if has_staged {
        writeln!(out, "\nChanges to be committed:")?;
        for e in status.entries.iter() {
            if !e.is_staged() {
                continue;
            }
            let label = match e.index {
                FileStatus::Added => "new file:   ",
                FileStatus::Modified => "modified:   ",
                FileStatus::Deleted => "deleted:    ",
                FileStatus::TypeChanged => "typechange: ",
                _ => "modified:   ",
            };
            out.write_str("\t")?;
            if color {
                out.write_str(GREEN)?;
            }
            out.write_str(label)?;
            out.write_str(e.path.as_str())?;
            if color {
                out.write_str(RESET)?;
            }
            out.write_str("\n")?;
        }
    }

    if has_unmerged {
        writeln!(out, "\nUnmerged paths:")?;
        for e in status.entries.iter() {
            if !e.unmerged {
                continue;
            }
            let label = unmerged_label(e);
            out.write_str("\t")?;
            if color {
                out.write_str(RED)?;
            }
            out.write_str(label)?;
            out.write_str(e.path.as_str())?;
            if color {
                out.write_str(RESET)?;
            }
            out.write_str("\n")?;
        }
    }

    if has_unstaged {
        writeln!(out, "\nChanges not staged for commit:")?;
        for e in status.entries.iter() {
            if !e.is_unstaged() {
                continue;
            }
            let label = match e.worktree {
                FileStatus::Added => "new file:   ",
                FileStatus::Modified => "modified:   ",
                FileStatus::Deleted => "deleted:    ",
                FileStatus::TypeChanged => "typechange: ",
                _ => "modified:   ",
            };
            out.write_str("\t")?;
            if color {
                out.write_str(RED)?;
            }
            out.write_str(label)?;
            out.write_str(e.path.as_str())?;
            if color {
                out.write_str(RESET)?;
            }
            out.write_str("\n")?;
        }
    }

    if has_untracked {
        writeln!(out, "\nUntracked files:")?;
        for e in status.entries.iter() {
            if !e.is_untracked() {
                continue;
            }
            out.write_str("\t")?;
            if color {
                out.write_str(RED)?;
            }
            out.write_str(e.path.as_str())?;
            if color {
                out.write_str(RESET)?;
            }
            out.write_str("\n")?;
        }
    }

    if !has_staged && !has_unmerged && !has_unstaged && !has_untracked {
        if status.counts.is_dirty() {
            // Counts-only status without `Detail::Full` entries: show summary counts.
            writeln!(
                out,
                "\nDirty working tree (staged={}, modified={}, deleted={}, untracked={}, conflicted={})",
                status.counts.staged(),
                status.counts.modified,
                status.counts.deleted,
                status.counts.untracked,
                status.counts.conflicted,
            )?;
        } else {
            writeln!(out, "nothing to commit, working tree clean")?;
        }
    } else if !has_staged && !has_unmerged {
        if has_unstaged {
            writeln!(out, "\nno changes added to commit")?;
        } else {
            writeln!(out, "\nnothing added to commit but untracked files present")?;
        }
    }

    Ok(())
}

fn write_upstream_name(
    remote: &str,
    branch: &str,
    has_remote: bool,
    out: &mut impl fmt::Write,
) -> fmt::Result {
    if has_remote {
        out.write_str(remote)?;
        out.write_str("/")?;
    }
    out.write_str(branch)
}

fn write_short_divergence(div: Divergence, out: &mut impl fmt::Write) -> fmt::Result {
    match div {
        Divergence::Counted { ahead, behind } if ahead > 0 && behind > 0 => {
            write!(out, " [ahead {ahead}, behind {behind}]")
        }
        Divergence::Counted { ahead, behind: 0 } if ahead > 0 => {
            write!(out, " [ahead {ahead}]")
        }
        Divergence::Counted { ahead: 0, behind } if behind > 0 => {
            write!(out, " [behind {behind}]")
        }
        Divergence::Capped { ahead_at_least, behind_at_least }
            if ahead_at_least > 0 || behind_at_least > 0 =>
        {
            write!(out, " [ahead {ahead_at_least}+, behind {behind_at_least}+]")
        }
        Divergence::UpstreamMissing => out.write_str(" [gone]"),
        _ => Ok(()),
    }
}

fn unmerged_label(e: &StatusEntry<'_>) -> &'static str {
    match (e.index, e.worktree) {
        (FileStatus::Deleted, FileStatus::Deleted) => "both deleted:    ",
        (FileStatus::Added, FileStatus::Unmerged) => "added by us:     ",
        (FileStatus::Unmerged, FileStatus::Deleted) => "deleted by them: ",
        (FileStatus::Unmerged, FileStatus::Added) => "added by them:   ",
        (FileStatus::Deleted, FileStatus::Unmerged) => "deleted by us:   ",
        (FileStatus::Added, FileStatus::Added) => "both added:      ",
        _ => "both modified:   ",
    }
}

fn write_git_path(path: &str, out: &mut impl fmt::Write) -> fmt::Result {
    let needs_quote = path
        .bytes()
        .any(|b| b < 0x20 || b == 0x7f || b == b'"' || b == b'\\' || b >= 0x80);
    if !needs_quote {
        return out.write_str(path);
    }
    out.write_str("\"")?;
    for b in path.bytes() {
        match b {
            b'"' => out.write_str("\\\"")?,
            b'\\' => out.write_str("\\\\")?,
            b'\n' => out.write_str("\\n")?,
            b'\r' => out.write_str("\\r")?,
            b'\t' => out.write_str("\\t")?,
            0x20..=0x7e => out.write_char(b as char)?,
            other => write!(out, "\\{:03o}", other)?,
        }
    }
    out.write_str("\"")
}
