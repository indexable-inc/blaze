//! The derived facts' pins at the command's edge, and a fact read answered again now (brief P,
//! port27/tempo-deps NOTES).
//!
//! - [`read_apple_userland`]: the one place the machine is read for them: on macOS the guest's
//!   userland is the host's (`SpawnSetting::userland`), so its `SystemVersion.plist` and kernel
//!   release are read once by the command and handed down (rule 75).
//! - [`command_spawn_setting`]: the command's spawn setting with the pins
//!   ([`SpawnSetting::facts_only`]), made by blaze-build-script's one producer
//!   ([`blaze_build_script::fact_pins::fact_pins`]) from the clang pin table and that text.
//! - [`read_now`]: what a script record's read is now, a derived fact's subject answered again
//!   with the command's pins. The pins are part of what the fact's answer is derived from, so a
//!   record that asked a fact is reused only when today's pins give the same reply (rule 21);
//!   with no pins a fact read cannot be worked out and the list is passed over, so the script
//!   runs again and its spawn is refused by name.

use core::fmt::Display;

use blaze_build_script::{
    fact_pins::{self, CLANG_GUEST_DIR, FactPinsError, OwnedPins, SYSTEM_VERSION_PLIST, UserlandError},
    facts,
};
use blaze_pins::CLANG_JSON;
use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_record::Digest;

use crate::{
    script_sandbox::{Tree, answer_now},
    spawn_route::{FactPins, SpawnSetting, reply_digest},
};

impl FactPins {
    /// The owned pins of the producer, as the spawn setting holds them.
    pub fn from_owned(pins: &OwnedPins<'_>) -> FactPins {
        FactPins {
            clang_version_text: pins.clang_version_text.as_str().to_owned(),
            macos_product_version: pins.macos_product_version.as_str().to_owned(),
            macos_build_version: pins.macos_build_version.as_str().to_owned(),
        }
    }

    /// Borrowed, as `blaze_build_script::facts::answer` takes them.
    pub fn as_pins(&self) -> facts::Pins<'_> {
        facts::Pins {
            clang_version_text: &self.clang_version_text,
            macos_product_version: &self.macos_product_version,
            macos_build_version: &self.macos_build_version,
        }
    }
}

/// The Apple userland's text as the command read it: the plist and the kernel's release.
#[derive(Debug)]
pub struct UserlandText<'a> {
    pub plist: AString<'a>,
    pub darwin_release: AString<'a>,
}

/// Why the command has no pins for the derived facts: what was read or asked, and why.
#[derive(Debug)]
pub enum CommandPinsError<'a> {
    /// The userland's version file could not be read.
    ReadUserland { path: &'static str, error: std::io::Error },
    /// `uname` failed, or its release is not UTF-8 (`error` is then `InvalidData`).
    KernelRelease { error: std::io::Error },
    /// The userland's text declares no userland ([`UserlandError`], owned).
    NoUserlandKey { key: &'static str },
    MalformedUserland { what: &'static str, text: AString<'a> },
    /// The clang pin table cannot give `clang --version` ([`FactPinsError`], owned).
    ClangPin { field: &'static str },
    VersionMismatch { pinned: AString<'a> },
    NoTriple { target: AString<'a> },
}

impl core::fmt::Display for CommandPinsError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CommandPinsError::ReadUserland { path, error } => {
                write!(f, "{path}: {error}: the Apple userland is not declared, so the derived facts have no pins (brief P, rule 75)")
            }
            CommandPinsError::KernelRelease { error } => {
                write!(f, "the kernel's release (uname): {error}: the derived facts have no pins (brief P, rule 75)")
            }
            CommandPinsError::NoUserlandKey { key } => Display::fmt(&UserlandError::NoKey { key: *key }, f),
            CommandPinsError::MalformedUserland { what, text } => Display::fmt(&UserlandError::Malformed { what: *what, text: text.as_str() }, f),
            CommandPinsError::ClangPin { field } => Display::fmt(&FactPinsError::Pin { field: *field }, f),
            CommandPinsError::VersionMismatch { pinned } => Display::fmt(&FactPinsError::VersionMismatch { pinned: pinned.as_str() }, f),
            CommandPinsError::NoTriple { target } => Display::fmt(&FactPinsError::NoTriple { target: target.as_str() }, f),
        }
    }
}

fn owned_userland<'a>(error: UserlandError<'_>, alloc: Alloc<'a>) -> CommandPinsError<'a> {
    match error {
        UserlandError::NoKey { key } => CommandPinsError::NoUserlandKey { key },
        UserlandError::Malformed { what, text } => CommandPinsError::MalformedUserland { what, text: String::from_str_in(text, alloc) },
    }
}

fn owned_pin<'a>(error: FactPinsError<'_>, alloc: Alloc<'a>) -> CommandPinsError<'a> {
    match error {
        FactPinsError::Pin { field } => CommandPinsError::ClangPin { field },
        FactPinsError::VersionMismatch { pinned } => CommandPinsError::VersionMismatch { pinned: String::from_str_in(pinned, alloc) },
        FactPinsError::NoTriple { target } => CommandPinsError::NoTriple { target: String::from_str_in(target, alloc) },
    }
}

/// The kernel's release, as `uname -r` prints it.
#[allow(unsafe_code)]
fn kernel_release<'a>(alloc: Alloc<'a>) -> Result<AString<'a>, CommandPinsError<'a>> {
    // SAFETY: `utsname` is arrays of C chars only, so all zero bytes is a valid value; `uname`
    // writes NUL-terminated strings into the struct it is given and keeps no pointer to it.
    let mut name: libc::utsname = unsafe { core::mem::zeroed() };
    // SAFETY: `name` is a live, writable `utsname` for the whole call.
    if unsafe { libc::uname(&mut name) } != 0 {
        return Err(CommandPinsError::KernelRelease { error: std::io::Error::last_os_error() });
    }
    let mut bytes: AVec<'a, u8> = Vec::new_in(alloc);
    for c in name.release.iter().take_while(|c| **c != 0) {
        bytes.push(*c as u8);
    }
    let text = core::str::from_utf8(&bytes)
        .map_err(|_| CommandPinsError::KernelRelease { error: std::io::Error::from(std::io::ErrorKind::InvalidData) })?;
    Ok(String::from_str_in(text, alloc))
}

/// The host's Apple userland, read once by the command (the edge; rule 75): its
/// `SystemVersion.plist` and kernel release. Nothing below the command reads the machine.
pub fn read_apple_userland<'a>(alloc: Alloc<'a>) -> Result<UserlandText<'a>, CommandPinsError<'a>> {
    let read = std::fs::read_to_string(SYSTEM_VERSION_PLIST).map_err(|error| CommandPinsError::ReadUserland { path: SYSTEM_VERSION_PLIST, error })?;
    Ok(UserlandText { plist: String::from_str_in(&read, alloc), darwin_release: kernel_release(alloc)? })
}

/// The spawn setting with the derived facts' pins for scripts whose guest is `host`, from the
/// userland's `text` and the clang pin table ([`CLANG_JSON`], at `pins/clang.json`).
pub fn pinned_setting<'a>(host: &str, text: &UserlandText<'_>, alloc: Alloc<'a>) -> Result<SpawnSetting, CommandPinsError<'a>> {
    let userland = fact_pins::apple_userland(text.plist.as_str(), text.darwin_release.as_str()).map_err(|e| owned_userland(e, alloc))?;
    let pins = fact_pins::fact_pins(CLANG_JSON, host, &userland, CLANG_GUEST_DIR, alloc).map_err(|e| owned_pin(e, alloc))?;
    Ok(SpawnSetting::facts_only(FactPins::from_owned(&pins)))
}

/// The command's spawn setting (chosen once per command beside its executor): for a macOS host,
/// the derived facts' pins from the host's userland, read here; a read or a pin that fails stops
/// the command naming it. Any other host declares no Apple userland (READINESS Q1 is open), so it
/// gets [`SpawnSetting::off`] and each fact is refused by name at its spawn.
pub fn command_spawn_setting<'a>(host: &str, alloc: Alloc<'a>) -> Result<SpawnSetting, CommandPinsError<'a>> {
    if !host.ends_with("-apple-darwin") {
        return Ok(SpawnSetting::off());
    }
    let text = read_apple_userland(alloc)?;
    pinned_setting(host, &text, alloc)
}

/// What a read subject says now, when it is a derived fact's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FactNow {
    /// Not a fact's subject (`file:`, `dir:`, `stat:`, `git:` and the rest).
    NotAFact,
    /// A fact's, and the command has no pins: it cannot be worked out now.
    NoPins,
    /// A fact's, answered now: its reply's digest, `None` when it is refused now (which differs
    /// from any recorded answer: a fact is recorded only when answered).
    Answer(Option<ContentId>),
}

/// `subject` ([`facts::subject`]'s form: the program's word, then one argument per line, or a
/// missing program's `absent-program\n<host>\n<name>`) answered now with `pins`.
pub fn fact_now(subject: &str, pins: Option<&FactPins>, alloc: Alloc<'_>) -> FactNow {
    // A program found missing (spawn_route.rs `ABSENT_HEAD`, D192): a fact of the pinned base
    // tool set's names and blaze's answered list, not of these pins, so answered with none.
    if let Some(now) = crate::spawn_route::absent_now(subject, alloc) {
        return FactNow::Answer(now);
    }
    let mut lines = subject.split('\n');
    let head = lines.next().unwrap_or("");
    if !matches!(head, "pkg-config" | "sw_vers" | "cc-version") {
        return FactNow::NotAFact;
    }
    let Some(pins) = pins else { return FactNow::NoPins };
    let mut args: AVec<'_, &str> = Vec::new_in(alloc);
    for line in lines {
        args.push(line);
    }
    let compiler_arg: [&str; 1];
    let question = match (head, &*args) {
        ("cc-version", [program, arg]) => {
            compiler_arg = [*arg];
            facts::question(program, &compiler_arg)
        }
        ("cc-version", _) => None,
        (program, args) => facts::question(program, args),
    };
    let Some(fact) = question else { return FactNow::Answer(None) };
    match facts::answer(&fact, &pins.as_pins(), alloc) {
        Ok(answer) => FactNow::Answer(Some(reply_digest(answer.code, answer.stdout.as_bytes(), answer.stderr.as_bytes()))),
        Err(_) => FactNow::Answer(None),
    }
}

/// What the read `subject` is now for a record's lookup: a fact answered with `pins`
/// ([`fact_now`]), anything else by [`answer_now`] in `tree`; `None` where it cannot be worked
/// out now, which passes over that read list.
pub fn read_now(tree: &Tree, subject: &str, pins: Option<&FactPins>, alloc: Alloc<'_>) -> Option<Digest> {
    match fact_now(subject, pins, alloc) {
        FactNow::NotAFact => answer_now(tree, subject).ok().map(|now| now.map_or(Digest::Nothing, Digest::Is)),
        FactNow::NoPins => None,
        FactNow::Answer(now) => Some(now.map_or(Digest::Nothing, Digest::Is)),
    }
}
