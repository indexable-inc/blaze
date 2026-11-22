//! The pinned base tool set (D192, docs/v2/plan/design/base-tools.md): what a sandboxed build
//! step (a build script's spawned programs, a deferred C compile, a process tree's `sh
//! configure` and `make`, a native script module) may run, the same bytes on every machine.
//! Nothing it runs comes from the machine unless it has to, and what has to is a closed list
//! ([`FromMachine`]) whose identities are in the key of every step that used them.
//!
//! - [`pins`]: `pins/base-tools.json`, one versioned schema ([`pins::FORM`]), read with the
//!   tree's JSON reader into a [`PinsFile`] and written back; [`BaseSet::checked_in`] is the
//!   set for a platform, and a set whose digest is still `PENDING` is the typed refusal
//!   [`BaseSetError::NotBuilt`] ("the base tool set for aarch64-apple-darwin is not built yet:
//!   pins/base-tools.json"), never a reach to the OS.
//! - [`set`]: [`BaseSet`] (platform, the archive's 32-byte digest, the programs sorted) and the
//!   only way to a [`Program`]: [`StepTools::program`], whose miss is [`NotInSet`], the words
//!   a machine without the tool prints (`sh: cmake: command not found`) with the package and
//!   step. [`StepTools`] is linear: a step that took the set ends it by [`StepTools::finish`],
//!   whose [`ToolsPart`] the step's key takes, so a step that ran a program cannot leave the
//!   set's digest out of its key, and a step that ran none holds nothing.
//! - [`machine`]: [`FromMachine`], the closed list of what still comes from the machine (the
//!   Apple SDK, the system libraries a process must load), each with its identity bytes.
//! - [`sandbox`]: the guest folder ([`GUEST_ROOT`], [`GUEST_BIN`]: one spelling on every
//!   machine), the fixed environment ([`step_env`]: `PATH`, `TZ=UTC`, `LC_ALL=C`,
//!   `SOURCE_DATE_EPOCH` passed in), the Seatbelt lines granting the set's host folder
//!   ([`profile_lines`]) and the guard that refuses a profile granting an OS tool folder
//!   ([`os_tool_in_profile`]), and the `etc/answers` file the set's `uname` and `hostname`
//!   read ([`answers_file`]).
//!
//! Sans-IO: the caller reads the machine's facts (the OS build, the SDK's identity), resolves
//! the set's host folder and hands them in; nothing here reads a file, the clock or the
//! environment.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod machine;
pub mod pins;
pub mod sandbox;
pub mod set;

#[cfg(test)]
mod tests;

pub use machine::{FromMachine, MachineIdError, MachineOs, OsBuild, SdkId};
pub use pins::{Answers, Group, PinsError, PinsFile, PinsWhy, Pinned, Platform, Row, Uname};
pub use sandbox::{
    ANSWERS_MEMBER, BadAnswer, GUEST_BIN, GUEST_ROOT, HostRoot, HostRootError, MANIFEST_MEMBER, OS_TOOL_FOLDERS, OsToolGrant,
    ProfileAllows, SourceDateEpoch, answers_file, os_tool_in_profile, profile_lines, step_env,
};
pub use set::{BaseSet, BaseSetError, NotInSet, Program, SetDigest, StepAt, StepTools, ToolsPart, WrongMachine};
