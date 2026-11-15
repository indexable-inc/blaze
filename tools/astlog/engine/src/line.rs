//! Where a form was written: the rules file and the line in it.

use std::fmt;
use std::sync::Arc;

/// A 1-based line of a rules file. The request's own text is `rules`; a file an
/// `(include ...)` read is named by the key its [`crate::Includes`] gave it, so a diagnostic
/// from inside a library names the library and not the file that included it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Line {
    file: Option<Arc<str>>,
    number: usize,
}

impl Line {
    /// Line `number` of the request's own rules text.
    #[must_use]
    pub const fn rules(number: usize) -> Self {
        Self { file: None, number }
    }

    /// Line `number` of the included file `file`.
    #[must_use]
    pub const fn of(file: Arc<str>, number: usize) -> Self {
        Self { file: Some(file), number }
    }

    #[must_use]
    pub const fn number(&self) -> usize {
        self.number
    }

    /// The included file this line is in; `None` for the request's own text.
    #[must_use]
    pub fn file(&self) -> Option<&str> {
        self.file.as_deref()
    }
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.file {
            Some(file) => write!(f, "{file}:{}", self.number),
            None => write!(f, "rules:{}", self.number),
        }
    }
}

/// Lets an error's context selector take the line of a form it borrows.
impl From<&Line> for Line {
    fn from(line: &Line) -> Self {
        line.clone() // clone: a file name's count
    }
}

/// Lets a test match an error's line by its number alone: `Error::TooDeep { line, .. } if line == 2`.
impl PartialEq<usize> for Line {
    fn eq(&self, other: &usize) -> bool {
        self.file.is_none() && self.number == *other
    }
}
