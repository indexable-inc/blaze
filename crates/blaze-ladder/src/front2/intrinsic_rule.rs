//! How a `#[rustc_intrinsic]` fn is built: the ONE home of the rule (rule 30), which the run's
//! table rows (program/tabled.rs `row`, program/lazy/fill.rs `row`) both take.
//!
//! The rule, for every intrinsic and not for any one name:
//! 1. An intrinsic blaze-build's table names ([`Which::named`]) is built as the table's row
//!    says: the table is the compiler's own implementation, which the language lets a backend
//!    give in place of any fallback body (and must give for one with none).
//! 2. One the table does not name that is declared WITH a body, and not marked
//!    `#[rustc_intrinsic_must_be_overridden]`, is built from that body like any fn. That is what
//!    the attribute means: the pinned library/core/src/intrinsics/mod.rs, lines 13-17:
//!    "Intrinsics don't need a body. However, they optionally can have a body, which we call the
//!    "fallback body". This will be used by codegen backends that do not have a dedicated
//!    implementation of the intrinsic [..] The fallback body obviously has to be a valid
//!    implementation of the documented specification of the intrinsic." (Where the body is
//!    exactly the meaning core marks it `#[miri::intrinsic_fallback_is_spec]`, as
//!    `integer_min`'s `if a < b { a } else { b }`.) This is how each nightly's new intrinsics
//!    keep building with no row: the body is valid Rust.
//! 3. One the table does not name that is declared with NO body (`fn name(..);`, the compiler
//!    must supply it), or is marked must-be-overridden (its body is not its meaning), is the
//!    typed refusal [`Unbuilt`], with where the declaration stands ([`declared_at`], rule 19).
//!
//! Rule 2 is the language's rule stated here, not a retry of rule 1 (rule 3): a name the table
//! holds is never built from its body, and a body is never asked for a name the table lacks
//! unless the declaration carries one.
//!
//! The constant evaluator takes the same rule from the frozen fn head (front2/heads.rs
//! `FnHead::const_call`).

use core::fmt;
use std::boxed::Box;
use std::format;

use blaze_build::intrinsics::Which;
use blaze_id::ItemKey as LibKey;

use super::source::NamesSource;

/// The attribute that makes an intrinsic's body no fallback: a backend must override it.
pub const MUST_BE_OVERRIDDEN: &str = "rustc_intrinsic_must_be_overridden";

/// What a `#[rustc_intrinsic]` fn's declaration says, from its signature's lowering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Declared {
    /// The declaration has a body (`{ .. }`), not `;`.
    pub has_body: bool,
    /// The declaration carries `#[rustc_intrinsic_must_be_overridden]`.
    pub must_be_overridden: bool,
}

/// How a `#[rustc_intrinsic]` fn is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Built {
    /// As the table's row says (rule 1).
    Table(Which),
    /// From the declaration's own body, like any fn (rule 2).
    FallbackBody,
}

/// Why a `#[rustc_intrinsic]` fn the table does not name cannot be built (rule 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unbuilt {
    /// Declared with no body: only the compiler can supply it.
    NoBody,
    /// Declared with a body, but `#[rustc_intrinsic_must_be_overridden]`: the body is a
    /// placeholder, not the intrinsic's meaning.
    MustBeOverridden,
}

impl fmt::Display for Unbuilt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unbuilt::NoBody => f.write_str("it is declared with no body (`;`), so only the compiler can supply it"),
            Unbuilt::MustBeOverridden => write!(f, "it is marked `#[{MUST_BE_OVERRIDDEN}]`, so its body is not its meaning"),
        }
    }
}

/// The rule (module docs) for the intrinsic `name` declared as `declared`.
pub fn built(name: &str, declared: Declared) -> Result<Built, Unbuilt> {
    if let Ok(which) = Which::named(name) {
        return Ok(Built::Table(which));
    }
    match declared {
        Declared { must_be_overridden: true, .. } => Err(Unbuilt::MustBeOverridden),
        Declared { has_body: false, .. } => Err(Unbuilt::NoBody),
        Declared { has_body: true, must_be_overridden: false } => Ok(Built::FallbackBody),
    }
}

/// Where library item `lib`'s declaration stands, as a refusal prints it (rule 71: computed
/// only here, when printed): `file:line` of its text's first byte (its outline entry, which
/// starts at its first attribute or doc line). An item written by a macro is in that
/// expansion's text, which has no file of its own: said so, with its file when the source
/// names one.
pub fn declared_at<'n, S: NamesSource<'n>>(names: S, lib: LibKey) -> Box<str> {
    let Some(place) = names.place_of(lib) else {
        return Box::from("(the names records give it no place)");
    };
    let blaze_scope::resolve::At::File(file) = place.at else {
        return Box::from("(in a macro's expansion, which has no file of its own)");
    };
    let Some(shown) = names.file_shown(place.krate, file) else {
        return format!("(file {file} of crate {}, which the names records do not name)", place.krate).into_boxed_str();
    };
    let line = names
        .item_start(lib)
        .and_then(|(_, start)| names.file_text(place.krate, file)?.get(..start as usize))
        .map(|before| before.bytes().filter(|&b| b == b'\n').count() + 1);
    match line {
        Some(line) => format!("{shown}:{line}").into_boxed_str(),
        None => format!("{shown} (its line: the file's text was not read)").into_boxed_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Built, Declared, Unbuilt, built};

    /// A name no table holds: the rule then follows the declaration alone.
    const UNNAMED: &str = "not_an_intrinsic_any_table_names";

    #[test]
    fn a_named_intrinsic_is_the_table_s_whatever_its_declaration() {
        for (has_body, must_be_overridden) in [(false, false), (true, false), (true, true), (false, true)] {
            let got = built("ctpop", Declared { has_body, must_be_overridden });
            assert!(matches!(got, Ok(Built::Table(_))), "ctpop declared {has_body}/{must_be_overridden}: {got:?}");
        }
    }

    #[test]
    fn an_unnamed_intrinsic_with_a_body_is_built_from_it() {
        assert_eq!(built(UNNAMED, Declared { has_body: true, must_be_overridden: false }), Ok(Built::FallbackBody));
    }

    #[test]
    fn an_unnamed_intrinsic_with_no_body_or_must_be_overridden_is_refused_by_which() {
        assert_eq!(built(UNNAMED, Declared { has_body: false, must_be_overridden: false }), Err(Unbuilt::NoBody));
        assert_eq!(built(UNNAMED, Declared { has_body: true, must_be_overridden: true }), Err(Unbuilt::MustBeOverridden));
        assert_eq!(built(UNNAMED, Declared { has_body: false, must_be_overridden: true }), Err(Unbuilt::MustBeOverridden));
    }
}
