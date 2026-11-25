//! Zero-allocation Starship-compatible prompt formatter over [`core::fmt::Write`].

#![blaze::sans_io]

use core::fmt;

use crate::{Divergence, Head, PromptStatus, RepoState};

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
