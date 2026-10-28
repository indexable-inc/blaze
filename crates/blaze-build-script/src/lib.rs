//! Build scripts run as cargo 1.99 runs them, with nothing of cargo's: the environment a
//! script is given ([`env`]), the run itself ([`run`]), what it prints read as cargo reads it
//! ([`output`]) and who receives what ([`apply`]), for blaze to run the scripts it compiles without cargo (no-rustc
//! step I, [#89]) and in its sandbox ([#147]).
//!
//! What a run is made from is a plain description ([`unit::Unit`]) that blaze's manifest
//! reader and resolver fill; this crate reads no manifest and asks no cargo. cargo is the
//! oracle of `harness/scripts.py`, which runs a corpus of scripts under both and compares.
//!
//! [#89]: https://github.com/indexable-inc/blaze/issues/89
//! [#147]: https://github.com/indexable-inc/blaze/issues/147

pub mod apply;
pub mod env;
pub mod inert;
pub mod output;
pub mod run;
pub mod unit;

pub use env::{Env, env, envify};
pub use output::{Output, parse};
pub use unit::Unit;

/// What cargo would fail with, in cargo's words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}
