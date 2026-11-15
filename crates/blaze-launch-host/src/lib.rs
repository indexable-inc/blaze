//! The daemon's side of blaze's launcher (docs/v2/plan/design/launcher.md, steps 3 to 5).
//!
//! | module | what |
//! |---|---|
//! | [`record`] | a build's function or static made the record a launcher takes (`blaze_launch::record`): its relocations by place, its positions as data |
//! | [`bind`] | the bind record: a launch record with what fills each of its places, kept in the store under a key of every input of its bytes, read back with no database |
//! | [`start`] | the start image: a launched program's whole state and its next launcher's answer as one store record, kept by [`Program::kept`] and read back by [`Program::restored`] with no database |
//! | [`program`] | [`Program`]: one program's slots, the records it was given piece by piece, what each of its launchers has bound, and each build classified as an edit to apply to the running program or a restart |
//!
//! Nothing here does IO: blaze-cargo's driver owns the sockets, the threads and the store,
//! gives [`Program`] each build's start, the launchers' messages and the pieces their first
//! calls want, and writes the frames it hands back.

// `#[expect(blaze::..)]` names blaze's own lints; inert under rustc.
#![feature(register_tool)]
#![register_tool(blaze)]

pub mod bind;
pub mod program;
pub mod record;
pub mod start;

#[cfg(test)]
mod tests;

pub use bind::{Bound, Names, Under};
pub use program::{Conn, Piece, Program, Said, Told};
pub use start::{Of, start_key};
