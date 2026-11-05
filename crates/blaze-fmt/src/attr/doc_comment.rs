// Follows `src/tools/rustfmt/src/attr/doc_comment.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0).

use crate::comment::CommentStyle;
use std::fmt::{self, Display};

/// Formats a string as a doc comment using the given [`CommentStyle`].
pub(super) struct DocCommentFormatter<'a> {
    literal: &'a str,
    style: CommentStyle<'a>,
}

impl<'a> DocCommentFormatter<'a> {
    pub(super) const fn new(literal: &'a str, style: CommentStyle<'a>) -> Self {
        Self { literal, style }
    }
}

impl Display for DocCommentFormatter<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let opener = self.style.opener().trim_end();
        let mut lines = self.literal.lines().peekable();

        // Handle `#[doc = ""]`.
        if lines.peek().is_none() {
            return write!(formatter, "{opener}");
        }

        while let Some(line) = lines.next() {
            let is_last_line = lines.peek().is_none();
            if is_last_line {
                write!(formatter, "{opener}{line}")?;
            } else {
                writeln!(formatter, "{opener}{line}")?;
            }
        }
        Ok(())
    }
}
