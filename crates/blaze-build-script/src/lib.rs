//! Build scripts run as cargo 1.99 runs them, with nothing of cargo's: the environment a
//! script is given ([`env`]), the run itself ([`run`]), what it prints read as cargo reads it
//! ([`output`]) and who receives what ([`apply`]), for blaze to run the scripts it compiles without cargo (no-rustc
//! step I, [#89]) and in its sandbox ([#147]).
//!
//! What a run is made from is a plain description ([`unit::Unit`]) that blaze's manifest
//! reader and resolver fill, every value cargo always gives a script filled
//! ([`unit::Unit::check`]); this crate reads no manifest, asks no cargo and runs no script
//! (blaze's sandbox does). A script is any executable the caller places (`Unit::script`);
//! nothing here assumes where or by what it was compiled.
//!
//! For blaze v2 (docs/v2/plan/lanes/usercode.md): scripts that only print constant directives are read
//! rather than run ([`elide`](mod@elide), #1296); the environment says what cargo sets from the unit and
//! what it passes through ([`Env::origin`], #805); and what cargo's rule watches of a run
//! is data ([`watch`]), for a run keyed as cargo keys it (#1450).
//!
//! Each reads its input in one pass over borrowed bytes: what a script prints is read line by
//! line into a [`output::Sink`] ([`output::read`]), the root file elision reads is lexed a
//! token at a time ([`elide::lex::Tokens`]), and a run is judged from its bytes alone
//! ([`run::judge`]). docs/v2/plan/lanes/usercode/scripts.md says how each compares with the
//! fastest known way.
//!
//! [#89]: https://github.com/indexable-inc/blaze/issues/89
//! [#147]: https://github.com/indexable-inc/blaze/issues/147

#![feature(allocator_api, allocator_ext)]
#![forbid(unsafe_code)]

pub mod apply;
pub mod elide;
pub mod env;
pub mod output;
pub mod run;
pub mod unit;
pub mod watch;

pub use elide::{elide, verify};
pub use env::{Env, Origin, env, envify};
pub use output::{Output, parse, read};
pub use unit::Unit;
pub use watch::{Watched, watched, watched_in};

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

/// What cargo would fail with, in cargo's words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}
