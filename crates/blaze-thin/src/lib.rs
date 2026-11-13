//! The thin `blaze` (docs/client.md, "The thin blaze"): the program installed as `blaze`,
//! beside the engine (`blaze-engine`, which is the whole CLI and the daemon).
//!
//! The engine links the compiler, LLVM and its own allocator, so it costs milliseconds to
//! start before it asks the daemon anything. This program links none of them. It answers the
//! commands whose cost is otherwise all start-up:
//!
//! - `--version`, from what it is built with and the launcher's kept pin lookup;
//! - a `build` or a `run` that goes well with nothing to say but cargo's status lines
//!   (nothing to do, or units compiled with no diagnostic: an edit's build): one request on
//!   the daemon's socket, the CLI's own (the same bytes), whose answer it prints as the CLI
//!   prints it, and for `run` the program, which it becomes ([`talk`]: one connection a
//!   command, one `Status` on it).
//!
//! Everything else is the engine's, and is never done here a second, lesser way:
//!
//! - a command line with any word this does not read exactly as the CLI does ([`route`]), an
//!   environment that sets one of blaze's variables, a workspace this blaze is not known to
//!   be the one for ([`pin`]), no daemon listening, or a daemon that is not the engine beside
//!   this program: the engine is run with the command line as typed, before anything is sent;
//! - an answer that is more than cargo's status lines ([`answer`]), or that has not
//!   ended in [`handover::AFTER`]: the engine is handed the connection and what was read of
//!   the answer ([`handover`]) and prints it, so nothing is asked of the daemon twice.
//!
//! This library is what the engine shares with the program so the two can't differ: cargo's
//! lines ([`say`]), the color rule and the request ([`request`]), the reader of the kept pin
//! lookup and `--version`'s sentences ([`pin`]), and the handover's layout ([`handover`]).
//! It reads files in [`pin`] and, on Unix, [`solo`]: the client's own check of a build or a
//! run with nothing changed over the request slot the daemon wrote, which answers it with no
//! daemon asked (/tmp/plan/10-no-daemon-run.md), and [`kept`]: such an answer's spans,
//! written as a file the next daemon takes into its profile (the record and the take are
//! blaze-certify's `kept`); the program (`main.rs`) does the
//! rest of the IO.

#![deny(unsafe_code)]

pub mod answer;
pub mod handover;
#[cfg(unix)]
pub mod kept;
pub mod pin;
pub mod request;
pub mod route;
pub mod say;
#[cfg(unix)]
pub mod solo;
pub mod talk;

/// This blaze's version, as a pin names it: a release's `0.YYYYMMDD.N`, or `0.0.0-dev` for a
/// build of the tree (build.rs, which works it out as blaze-engine's does its own).
pub const VERSION: &str = env!("BLAZE_VERSION");

/// What `--version` prints first: `blaze 0.20261028.9 (fb82b09 2026-10-28)`.
pub const VERSION_LINE: &str = env!("BLAZE_VERSION_LINE");

/// The engine's file name beside the thin `blaze`: the release archives' and the build
/// directory's name of the whole program.
pub const ENGINE: &str = "blaze-engine";
