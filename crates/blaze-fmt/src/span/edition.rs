//! Follows `compiler/rustc_span/src/edition.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0).

use std::cell::Cell;

#[derive(Clone, Copy, Hash, PartialEq, PartialOrd, Debug, Eq, Ord)]
pub enum Edition {
    Edition2015,
    Edition2018,
    Edition2021,
    Edition2024,
    EditionFuture,
}

impl Edition {
    pub fn at_least_rust_2018(self) -> bool {
        self >= Edition::Edition2018
    }

    pub fn at_least_rust_2021(self) -> bool {
        self >= Edition::Edition2021
    }

    pub fn at_least_rust_2024(self) -> bool {
        self >= Edition::Edition2024
    }

    /// rust-analyzer's name for the same edition, for its lexer and parser.
    pub fn to_ra(self) -> parser::Edition {
        match self {
            Edition::Edition2015 => parser::Edition::Edition2015,
            Edition::Edition2018 => parser::Edition::Edition2018,
            Edition::Edition2021 => parser::Edition::Edition2021,
            Edition::Edition2024 => parser::Edition::Edition2024,
            Edition::EditionFuture => parser::Edition::Edition2024,
        }
    }
}

thread_local! {
    static SESSION_EDITION: Cell<Edition> = const { Cell::new(Edition::Edition2015) };
}

pub(super) fn set_session_edition(edition: Edition) -> Edition {
    SESSION_EDITION.with(|e| e.replace(edition))
}

/// The edition of the session that is formatting, the one rustc reads from its globals.
pub fn session_edition() -> Edition {
    SESSION_EDITION.with(Cell::get)
}
