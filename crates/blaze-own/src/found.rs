//! The answer of a lookup that crosses a crate (end-state.md D193 point 5): [`Found::Known`],
//! [`Found::Absent`] or [`Found::Unknown`] with its cause. Only `Absent` may become a "there is
//! no such thing" error; `Unknown` fails the task that asked, naming what it asked for and why
//! no answer came (rule 3, rule 19). `Option` at such a seam reads an unknown as an absence:
//! three false errors of 2026-11-21 were that (an impl not filed, a glob of a crate whose names
//! failed, a constant's proc macro with no runner), and a file read that failed for a reason
//! other than its absence (a permission, a read that moved, the driver) was another.
//!
//! The type is here, below every crate that has such a seam, so there is one of it (rule 30).
//! There is no `From<Option<T>>`: a table (a test's, a fixed list) that knows what it lacks is
//! absent says so by name, [`Found::of_table`]; nothing converts an `Option` quietly.

/// What a lookup found: the value, a sure absence, or no answer and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[must_use = "an `Unknown` is a failure of the task that asked"]
pub enum Found<T, C> {
    /// The value.
    Known(T),
    /// There is none: the asker may say so.
    Absent,
    /// No answer: the asker fails, naming what it asked for and this cause.
    Unknown(C),
}

impl<T, C> Found<T, C> {
    /// A table's lookup: what a table holds is all there is, so what it lacks is absent. For
    /// a source that cannot fail (a test's table, a fixed list), never for a read of a file,
    /// a store or another task's answer.
    pub fn of_table(found: Option<T>) -> Found<T, C> {
        match found {
            Some(value) => Found::Known(value),
            None => Found::Absent,
        }
    }

    /// The value mapped; `Absent` and `Unknown` as they were.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Found<U, C> {
        match self {
            Found::Known(value) => Found::Known(f(value)),
            Found::Absent => Found::Absent,
            Found::Unknown(cause) => Found::Unknown(cause),
        }
    }

    /// The value, or `None` when there is surely none; `Err` with the cause when no answer
    /// came: the one way out to an `Option`, with the unknown taken first (`?` on it).
    pub fn known(self) -> Result<Option<T>, C> {
        match self {
            Found::Known(value) => Ok(Some(value)),
            Found::Absent => Ok(None),
            Found::Unknown(cause) => Err(cause),
        }
    }

    /// Whether this is [`Found::Known`].
    pub fn is_known(&self) -> bool {
        matches!(self, Found::Known(_))
    }
}

/// The cause of a [`Found::Unknown`] read of a file or a record: which one (`path`, as the
/// reader names it: a checkout file, a source key's hex) and the OS's or the store's error as
/// its reader typed it (bstd's `FsError`, held in the reader's frame: the crates that take a
/// read cannot name bstd, so it crosses as its words). The asker's error carries it whole.
#[derive(Clone, Copy)]
pub struct Unread<'a> {
    pub path: &'a str,
    pub why: &'a (dyn core::fmt::Display + Send + Sync),
}

impl core::fmt::Display for Unread<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "`{}` could not be read ({}): neither its bytes nor its absence is known", self.path, self.why)
    }
}

impl core::fmt::Debug for Unread<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Unread({:?}: {})", self.path, self.why)
    }
}

#[cfg(test)]
mod tests {
    use super::Found;

    /// The one way to an `Option` keeps the unknown apart: TWIN `Known` and `Absent` come out
    /// as `Some` and `None`; CONTROL `Unknown` is the `Err` an `Option` would have hidden as
    /// `None`, and a table's lookup makes no `Unknown`.
    #[test]
    fn an_unknown_never_reads_as_an_absence() {
        assert_eq!(Found::<u8, &str>::Known(1).known(), Ok(Some(1)));
        assert_eq!(Found::<u8, &str>::Absent.known(), Ok(None));
        assert_eq!(Found::<u8, &str>::Unknown("denied").known(), Err("denied"));
        assert_eq!(Found::<u8, &str>::of_table(None), Found::Absent);
        assert_eq!(Found::<u8, &str>::of_table(Some(2)).map(|v| v * 2), Found::Known(4));
        assert_eq!(Found::<u8, &str>::Unknown("moved").map(|v| v * 2), Found::Unknown("moved"));
    }
}
