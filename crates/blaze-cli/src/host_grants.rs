//! `blaze run --grant <parameter>=<scope>` (docs/v2/plan/design/run.md, "What a program is
//! given": the grant's NAME is the parameter's, its VALUE the scope) read into what bstd's host
//! checks a hosted program's asks against (`bstd::hosting::Hosting`, its list `(kind, name)`:
//! the kind as the program's guest names it on the wire, `bstd::grant::Kind::name`, then the
//! parameter's name).
//!
//! The bug this home fixes (found by the D104 lane, 2026-11-16): program.rs handed each
//! `(name, value)` pair to `Hosting` as it came, so `--grant io=everything` reached the host as
//! kind `io`, name `everything`, and no ask matched it; only the inverted `--grant everything=io`
//! ran. The host's order is bstd's wire and the run spec's (`spec::grant_arg` reads the value by
//! the parameter's kind); the command line's is the design's: the command was the wrong side.
//!
//! What the hosted wire carries today (bstd grant.rs, `Grant::hosted`): besides the kinds that
//! fill themselves (which take no grant), only the whole capabilities (`Fs`, `Rng`, ..., value `whole`). A
//! scoped value (a folder, an endpoint, variable names) has no hosted form yet: it is refused
//! here by name before the run, never handed to the host as a grant the program then cannot be
//! given (rule 3).

use core::fmt;

use bstd::grant::Kind;

/// Why one `--grant` cannot reach a hosted program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostGrantError {
    /// The `nth` `--grant` (from 1) has no parameter name before its `=`.
    NoName { nth: usize },
    /// The `nth` `--grant` names a scope (a folder, an endpoint, variable names): bstd's hosted
    /// wire carries no scoped capability yet.
    Scoped { nth: usize },
}

impl fmt::Display for HostGrantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostGrantError::NoName { nth } => {
                write!(f, "`--grant` number {nth} names no parameter: a grant is `--grant <parameter>=<scope>` (run.md)")
            }
            HostGrantError::Scoped { nth } => write!(
                f,
                "`--grant` number {nth} gives a scope (a folder, an endpoint or variable names), which a hosted program cannot be \
                 given yet (bstd's hosted wire carries only `everything` and `whole`; run.md, \"What a program is given\")"
            ),
        }
    }
}

impl core::error::Error for HostGrantError {}

/// The `nth` (from 1) `--grant <name>=<value>` as the host's `(kind, name)`: `everything` for
/// the whole world, `whole` for a whole capability; any other value is a scope, refused.
pub fn host_grant<'a>(nth: usize, name: &'a str, value: &str) -> Result<(&'static str, &'a str), HostGrantError> {
    if name.is_empty() {
        return Err(HostGrantError::NoName { nth });
    }
    // The kind names come from bstd's own `Kind::name`, the one home of the wire's words: the
    // `Everything` arm stops compiling when D104 STEP C (d104-C-1) deletes that kind, and goes
    // with it.
    const WHOLE: &str = Kind::Whole("").name();
    match value {
        WHOLE => Ok((WHOLE, name)),
        _ => Err(HostGrantError::Scoped { nth }),
    }
}

#[cfg(test)]
mod tests {
    use super::{HostGrantError, host_grant};
    use bstd::grant::Kind;

    /// `--grant io=everything` reaches the host as kind `everything`, name `io`: the order of
    /// bstd's `Hosting` list and of the guest's ask (`guest::grant` asks `T::KIND.name()`, then
    /// the parameter's name). Before the fix program.rs passed `("io", "everything")`, which
    /// fails the first assertion. Prints nothing; passes as `a_grant_reaches_the_host_as_kind_then_name ... ok`.
    #[test]
    fn a_grant_reaches_the_host_as_kind_then_name() {
        assert_eq!(host_grant(1, "io", "everything"), Err(HostGrantError::Scoped { nth: 1 }));
        assert_eq!(host_grant(2, "files", "whole"), Ok((Kind::Whole("fs").name(), "files")));
        // Control: the inverted form, which the old code made the only one that ran, is a
        // scope now (`io` is no kind's value), refused by name.
        assert_eq!(host_grant(3, "everything", "io"), Err(HostGrantError::Scoped { nth: 3 }));
    }

    /// A scope the hosted wire cannot carry and a grant with no name are refused before the
    /// run, each naming its place.
    #[test]
    fn a_scope_or_a_nameless_grant_is_refused_by_place() {
        assert_eq!(host_grant(1, "data", "fixtures"), Err(HostGrantError::Scoped { nth: 1 }));
        assert_eq!(host_grant(2, "api", "api.example.com:443"), Err(HostGrantError::Scoped { nth: 2 }));
        assert_eq!(host_grant(4, "", "everything"), Err(HostGrantError::NoName { nth: 4 }));
    }
}
