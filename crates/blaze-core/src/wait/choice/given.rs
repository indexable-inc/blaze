//! The choice of this process's wait driver from the launcher's mark as a value its caller
//! read: the environment-free form of [`super::choose_from_env`]. An entry with a world reads
//! [`SANDBOXED`] from its environment capability (whose values are text) and passes the value
//! here.
//!
//! The decision ([`waits_of`]) is apart from the choosing ([`choose_from`]): the choosing sets
//! the process's one driver, once, so only the decision can be asked many times, by a test or
//! by an entry that refuses a bad mark before anything is chosen.

#[cfg(doc)]
use super::SANDBOXED;
use super::{MarkRefused, OsString, Waits, choose};

/// The driver the launcher's mark asks for: [`Waits::Direct`] where [`SANDBOXED`] is `1`,
/// [`Waits::Ring`] where it is not set. Reads nothing but `value` and chooses nothing.
///
/// # Errors
///
/// [`MarkRefused`]: the mark set to anything but `1` (the launcher sets nothing else).
pub fn waits_of(value: Option<&str>) -> Result<Waits, MarkRefused> {
    match value {
        None => Ok(Waits::Ring),
        Some("1") => Ok(Waits::Direct),
        Some(mark) => Err(MarkRefused(OsString::from(mark))), // alloc: an error's value
    }
}

/// Chooses this process's driver from the value of [`SANDBOXED`] its caller read: what `main`
/// calls first, before any thread exists and before any wait.
///
/// # Errors
///
/// As [`waits_of`]: a refused mark chooses nothing.
///
/// # Panics
///
/// As [`choose`].
pub fn choose_from(value: Option<&str>) -> Result<Waits, MarkRefused> {
    let waits = waits_of(value)?;
    choose(waits);
    Ok(waits)
}

#[cfg(test)]
mod tests {
    use super::{MarkRefused, OsString, Waits, choose_from, waits_of};

    /// Each value `choose_from_env`'s doc names goes to its driver, and nothing but the value
    /// decides: not set is the ring, `1` is direct calls, any other value is refused naming
    /// it. A refused mark through `choose_from` chooses nothing (it returns before `choose`,
    /// so this test leaves the process's choice as it was).
    ///
    /// Expected line: `test wait::choice::given::tests::choose_from_reads_only_its_value ... ok`.
    #[test]
    fn choose_from_reads_only_its_value() {
        assert_eq!(waits_of(None), Ok(Waits::Ring));
        assert_eq!(waits_of(Some("1")), Ok(Waits::Direct));
        for other in ["", "0", "yes", "1 ", "true"] {
            assert_eq!(waits_of(Some(other)), Err(MarkRefused(OsString::from(other))), "{other:?}");
        }
        assert_eq!(choose_from(Some("2")), Err(MarkRefused(OsString::from("2"))));
    }
}
