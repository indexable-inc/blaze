//! Pure `#![no_std]`, zero-unsafe, sans-IO CLI schema, argv parser, help/usage renderer, and
//! flag/subcommand completion spec parser built on `blaze-own` explicit allocators
//! (`Alloc<'a>`, `AVec`, `AString`).
//!
//! Combines six pure parsing/data layers with zero OS IO:
//! 1. **Declarative CLI app schema & parser** ([`app`]) — zero proc macros, explicit allocators
//! 2. **Blaze CLI single source of truth** ([`blaze_app`]) — commands, flags, help, usage, completions
//! 3. **Curated manual specs** ([`manual`]) for popular CLIs & tools with non-standard help
//! 4. **`--help` / `-h` parser** ([`help_parser`]) across clap, commander (`claude`), cobra, GNU, argparse
//! 5. **Fish completion & `man` page parsers** ([`man_parser`])
//! 6. **Cache line codec** ([`codec`]) for thin host drivers to persist parsed specs by binary mtime.

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod app;
pub mod blaze_app;
pub mod codec;
pub mod explain;
pub mod help_doc;
pub mod help_parser;
pub mod man_parser;
pub mod manual;
pub mod remote;

use blaze_own::{AString, AVec, Alloc, String, Vec};

/// Origin of a parsed [`CliSpec`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecSource {
    Manual,
    Help,
    Fish,
    Man,
    Merged,
}

/// One CLI option/flag parsed from `--help`, fish completions, `man`, or manual specs.
#[derive(Debug, PartialEq, Eq)]
pub struct FlagSpec<'a> {
    /// Long flag including leading `--` (e.g. `"--dangerously-skip-permissions"`).
    pub long: Option<AString<'a>>,
    /// Short flag including leading `-` (e.g. `"-p"`).
    pub short: Option<AString<'a>>,
    /// Optional argument placeholder (e.g. `"<model>"`, `"<directories...>"`).
    pub value_hint: Option<AString<'a>>,
    /// Single-line human description shown in quick-doc / detail column.
    pub description: AString<'a>,
}

impl<'a> FlagSpec<'a> {
    /// Primary completion token (prefers `--long`, falls back to `-s`).
    pub fn primary(&self) -> &str {
        self.long
            .as_ref()
            .map(AString::as_str)
            .or_else(|| self.short.as_ref().map(AString::as_str))
            .unwrap_or("")
    }

    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> FlagSpec<'b> {
        FlagSpec {
            long: self.long.as_ref().map(|s| String::from_str_in(s.as_str(), alloc)),
            short: self.short.as_ref().map(|s| String::from_str_in(s.as_str(), alloc)),
            value_hint: self.value_hint.as_ref().map(|s| String::from_str_in(s.as_str(), alloc)),
            description: String::from_str_in(self.description.as_str(), alloc),
        }
    }
}

/// One CLI subcommand parsed from `--help`, fish completions, or manual specs.
#[derive(Debug, PartialEq, Eq)]
pub struct SubcommandSpec<'a> {
    pub name: AString<'a>,
    pub description: AString<'a>,
}

impl<'a> SubcommandSpec<'a> {
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> SubcommandSpec<'b> {
        SubcommandSpec {
            name: String::from_str_in(self.name.as_str(), alloc),
            description: String::from_str_in(self.description.as_str(), alloc),
        }
    }
}

/// Complete flag and subcommand specification for a command (or `cmd subcmd`).
#[derive(Debug, PartialEq, Eq)]
pub struct CliSpec<'a> {
    pub command: AString<'a>,
    pub flags: AVec<'a, FlagSpec<'a>>,
    pub subcommands: AVec<'a, SubcommandSpec<'a>>,
    pub source: SpecSource,
}

impl<'a> CliSpec<'a> {
    pub fn empty_in(command: &str, source: SpecSource, alloc: Alloc<'a>) -> Self {
        Self {
            command: String::from_str_in(command, alloc),
            flags: Vec::new_in(alloc),
            subcommands: Vec::new_in(alloc),
            source,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.flags.is_empty() && self.subcommands.is_empty()
    }

    /// Merge `other` into `self`, deduplicating flags by `(long, short)` and subcommands by name,
    /// preferring non-empty descriptions and value hints.
    pub fn merge_from(&mut self, other: &CliSpec<'_>, alloc: Alloc<'a>) {
        for f in other.flags.iter() {
            if let Some(existing) = self.flags.iter_mut().find(|e| {
                (f.long.is_some()
                    && e.long.as_ref().map(AString::as_str) == f.long.as_ref().map(AString::as_str))
                    || (f.long.is_none()
                        && f.short.is_some()
                        && e.short.as_ref().map(AString::as_str)
                            == f.short.as_ref().map(AString::as_str))
            }) {
                if existing.description.as_str().is_empty() && !f.description.as_str().is_empty() {
                    existing.description = String::from_str_in(f.description.as_str(), alloc);
                }
                if existing.value_hint.is_none()
                    && let Some(h) = f.value_hint.as_ref()
                {
                    existing.value_hint = Some(String::from_str_in(h.as_str(), alloc));
                }
                if existing.short.is_none()
                    && let Some(s) = f.short.as_ref()
                {
                    existing.short = Some(String::from_str_in(s.as_str(), alloc));
                }
                continue;
            }
            self.flags.push(f.clone_in(alloc));
        }

        for s in other.subcommands.iter() {
            if let Some(existing) = self
                .subcommands
                .iter_mut()
                .find(|e| e.name.as_str() == s.name.as_str())
            {
                if existing.description.as_str().is_empty() && !s.description.as_str().is_empty() {
                    existing.description = String::from_str_in(s.description.as_str(), alloc);
                }
                continue;
            }
            self.subcommands.push(s.clone_in(alloc));
        }
        if !other.is_empty() && self.source != other.source {
            self.source = SpecSource::Merged;
        }
    }
}

/// Binaries a thin host driver must NEVER execute with `--help` (destructive, privileged, or TUI);
/// they still get completion from fish completions, `man`, and curated manual specs.
pub fn is_unsafe_to_exec_help(cmd: &str) -> bool {
    matches!(
        cmd,
        "rm" | "rmdir"
            | "dd"
            | "mkfs"
            | "fdisk"
            | "diskutil"
            | "sudo"
            | "su"
            | "doas"
            | "kill"
            | "killall"
            | "pkill"
            | "reboot"
            | "shutdown"
            | "halt"
            | "poweroff"
            | "yes"
            | "vi"
            | "vim"
            | "nvim"
            | "nano"
            | "emacs"
            | "ed"
            | "top"
            | "htop"
            | "btop"
            | "less"
            | "more"
            | "man"
            | "ssh"
            | "scp"
            | "sftp"
            | "login"
            | "passwd"
            | "open"
            | "pbcopy"
            | "pbpaste"
    )
}
