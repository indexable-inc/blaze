//! The daemon's side of blaze's launcher (docs/v2/plan/design/launcher.md, steps 3 to 5).
//!
//! | module | what |
//! |---|---|
//! | [`record`] | a build's function or static made the record a launcher takes (`blaze_launch::record`): its relocations by place, its positions as data |
//! | [`program`] | [`Program`]: one program's slots, what each of its launchers has bound, and each build classified as an edit to apply to the running program or a restart |
//!
//! Nothing here does IO: blaze-cargo's driver owns the sockets and the threads, gives
//! [`Program`] the builds and the launchers' messages, and writes the frames it hands back.

pub mod program;
pub mod record;

#[cfg(test)]
mod tests;

pub use program::{Conn, Program, Tables, Told};
