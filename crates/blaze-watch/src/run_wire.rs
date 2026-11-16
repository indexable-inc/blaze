//! The wire forms of blaze-run this crate reads and writes, by the bytes blaze-run defines
//! (docs/v2/plan/design/run.md): a registry entry (`registry::Entry::encode`, 88 bytes) and a
//! viewer's `Toggle` request (`viewer::toggle`: `[1, 0, hooks, on, [slot, ...]]`). They are
//! restated here, not imported, because blaze-run links blaze-format and blaze-hash and this
//! crate links nothing third-party; watch.md ("What other crates must provide") asks blaze-run to
//! move both into one zero-dependency home that both crates take (rule 30), and a test here holds
//! these bytes to the layout run.md states until then.

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Vec;

use crate::cbor;

/// The bytes of an entry: a version word, pid, process start, checkout, program, began.
pub const ENTRY_LEN: usize = 4 + 4 + 8 + 32 + 32 + 8;
const ENTRY_VERSION: u32 = 1;
/// The schema of blaze-run's viewer requests.
pub const REQUEST_VERSION: u64 = 1;
const TOGGLE: u64 = 0;

/// A run's name: its pid and its process's start time (a reused pid is another name).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RunName {
    pub pid: u32,
    pub start: u64,
}

/// One live run as its registry entry holds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    pub name: RunName,
    pub checkout: [u8; 32],
    pub program: [u8; 32],
    /// When it began, on the run's clock.
    pub began: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bad {
    Length { len: usize },
    Version { version: u32 },
}

impl fmt::Display for Bad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Bad::Length { len } => write!(f, "a registry entry of {len} bytes, not {ENTRY_LEN}"),
            Bad::Version { version } => {
                write!(f, "a registry entry of layout {version}, where this blaze reads {ENTRY_VERSION}")
            }
        }
    }
}

impl core::error::Error for Bad {}

fn take<const N: usize>(bytes: &[u8], at: &mut usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes[*at..*at + N]);
    *at += N;
    out
}

impl Entry {
    pub fn decode(bytes: &[u8]) -> Result<Entry, Bad> {
        if bytes.len() != ENTRY_LEN {
            return Err(Bad::Length { len: bytes.len() });
        }
        let mut at = 0;
        let version = u32::from_le_bytes(take(bytes, &mut at));
        if version != ENTRY_VERSION {
            return Err(Bad::Version { version });
        }
        let pid = u32::from_le_bytes(take(bytes, &mut at));
        let start = u64::from_le_bytes(take(bytes, &mut at));
        let checkout = take(bytes, &mut at);
        let program = take(bytes, &mut at);
        let began = u64::from_le_bytes(take(bytes, &mut at));
        Ok(Entry { name: RunName { pid, start }, checkout, program, began })
    }

    /// The same bytes blaze-run writes; the simulations make entries with it.
    pub fn encode(&self) -> [u8; ENTRY_LEN] {
        let mut out = [0u8; ENTRY_LEN];
        let mut at = 0;
        for part in [
            &ENTRY_VERSION.to_le_bytes()[..],
            &self.name.pid.to_le_bytes(),
            &self.name.start.to_le_bytes(),
            &self.checkout,
            &self.program,
            &self.began.to_le_bytes(),
        ] {
            out[at..at + part.len()].copy_from_slice(part);
            at += part.len();
        }
        out
    }
}

/// Appends blaze-run's toggle request: turn `hooks` on or off for `slots`.
pub fn toggle<A: Allocator>(slots: &[u32], hooks: u16, on: bool, out: &mut Vec<u8, A>) {
    cbor::array(out, 5);
    cbor::uint(out, REQUEST_VERSION);
    cbor::uint(out, TOGGLE);
    cbor::uint(out, u64::from(hooks));
    cbor::boolean(out, on);
    cbor::array(out, slots.len() as u64);
    for slot in slots {
        cbor::uint(out, u64::from(*slot));
    }
}
