//! Sans-IO OpenSSH config / `known_hosts` parser and Blaze Remote Shell (`BSH1`) framing protocol.
//!
//! Zero `std`, zero `alloc` defaults, zero external dependencies, `#![forbid(unsafe_code)]`, and
//! `#![blaze::sans_io]` so any host driver (`bstd`, `blaze-shell`, CLI tools, or tests) can feed
//! `~/.ssh/config`, `Include`d fragments, and `~/.ssh/known_hosts` bytes with explicit bounds
//! (rule `bounded-progress`), and speak the multiplexed low-latency `BSH1` remote-shell wire
//! protocol over any byte stream (SSH exec channel, Unix socket, or deterministic simulation).

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

pub mod config;
pub mod known_hosts;
pub mod proto;

#[cfg(test)]
mod tests;

pub use config::{
    ConfigLimits, ConfigLoader, HostPattern, HostSummary, LoadStep, ResolvedHost, SshConfig,
    glob_match, parse_config,
};
pub use known_hosts::{KnownHost, parse_known_hosts};
pub use proto::{FrameCodec, FrameError, FrameLimits, Message, RemoteCompletion, RemoteGitSummary};
