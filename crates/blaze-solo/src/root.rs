//! The session root: one file a (checkout, `Kernel::registry()` digest), of two slots. Each
//! slot is `magic | sequence u64 | root key 32 B | pin 32 B | crc32c u32`, 80 bytes; the slot
//! of sequence `s` sits at offset `(s % 2) * 80`, so a save writes the slot the newest is not
//! in and a torn write leaves the newest whole.
//!
//! Open takes the valid slot with the highest sequence. A save writes the other slot with the
//! sequence one past, under an exclusive `flock` of the file, and only when the newest valid
//! slot is still the one the command opened; otherwise another command saved in between and
//! this one lost the race: it drops its delta and says so ([`crate::run`]). No fsync on this
//! path: a slot lost to a crash is the older one read again, whose session is whole (its
//! records were made durable before it was written), and whose pin then differs from the
//! table's, or agrees with it ([`Opened::Behind`] below).

#![blaze::sans_io]

use blaze_cas::Key;
use blaze_ids::{ContentId, crc32c::crc32c};
use blaze_kernel::Pin;

pub const SLOT_MAGIC: [u8; 4] = *b"BZS1";
pub const SLOT_BYTES: usize = 4 + 8 + 32 + 32 + 4;

/// One saved session: its root record in the store and the pin it was verified at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slot {
    /// One past the slot it replaced; the first save is 1.
    pub sequence: u64,
    pub root: Key,
    pub pin: Pin,
}

impl Slot {
    pub fn encode(&self) -> [u8; SLOT_BYTES] {
        let mut out = [0; SLOT_BYTES];
        out[0..4].copy_from_slice(&SLOT_MAGIC);
        out[4..12].copy_from_slice(&self.sequence.to_le_bytes());
        out[12..44].copy_from_slice(&self.root.0.0);
        out[44..76].copy_from_slice(&self.pin.0.0);
        let check = crc32c(&out[..76]);
        out[76..80].copy_from_slice(&check.to_le_bytes());
        out
    }

    /// The slot `bytes` hold, or `None`: empty, torn, rotten or of another layout. A slot that
    /// is not whole is as if never written, which the other slot covers.
    pub fn decode(bytes: &[u8]) -> Option<Slot> {
        let bytes: &[u8; SLOT_BYTES] = bytes.get(..SLOT_BYTES)?.try_into().ok()?;
        if bytes[0..4] != SLOT_MAGIC {
            return None;
        }
        let check = u32::from_le_bytes(bytes[76..80].try_into().expect("4 bytes"));
        if check != crc32c(&bytes[..76]) {
            return None;
        }
        let sequence = u64::from_le_bytes(bytes[4..12].try_into().expect("8 bytes"));
        let root = Key(ContentId(bytes[12..44].try_into().expect("32 bytes")));
        let pin = Pin(ContentId(bytes[44..76].try_into().expect("32 bytes")));
        (sequence > 0).then_some(Slot { sequence, root, pin })
    }

    /// Where the slot of `sequence` sits in the file.
    pub fn offset(sequence: u64) -> u64 {
        (sequence % 2) * SLOT_BYTES as u64
    }
}

/// The valid slot of the highest sequence in the file's bytes, if any. A slot is valid only at
/// its own offset: a whole slot found at the other one was not written by this protocol.
pub fn newest(file: &[u8]) -> Option<Slot> {
    let at = |n: usize| {
        let slot = Slot::decode(file.get(n * SLOT_BYTES..)?)?;
        (Slot::offset(slot.sequence) == (n * SLOT_BYTES) as u64).then_some(slot)
    };
    match (at(0), at(1)) {
        (Some(a), Some(b)) => Some(if a.sequence > b.sequence { a } else { b }),
        (one, other) => one.or(other),
    }
}

/// What a command opened, before it carries anything.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Opened {
    /// No slot was ever saved for this checkout and registry: a new session.
    New,
    /// The newest slot's session, at the table's pin: carried from there.
    Open,
    /// The newest slot names a root record the store does not have: a store whose last
    /// commit a crash took, or one removed or collected. The session starts new and the
    /// report says `lost`, with the root it names.
    ///
    /// This is not "silently empty": the slot proves a session was saved, the store proves
    /// its root is gone, and the command says both. Starting new costs only work: every
    /// answer is computed again or found in the store by its reads, each checked against the
    /// world as it is; nothing is taken on the word of a session that is not there.
    Lost { missing: Key },
    /// The newest slot's pin is not the table's: the two were written by different commands
    /// (a crash kept one of a save's two writes, or a command of another registry wrote the
    /// table). The sweep's changes are against the table, not the session, so the session
    /// cannot be carried by them: a new session.
    Behind { saved: Pin, table: Pin },
}

impl Opened {
    pub fn name(self) -> &'static str {
        match self {
            Opened::New => "new",
            Opened::Open => "open",
            Opened::Lost { .. } => "lost",
            Opened::Behind { .. } => "behind",
        }
    }
}

/// What to open, from the newest slot, the pin of the table the sweep starts from, and
/// whether the store holds a root.
pub fn decide(
    newest: Option<&Slot>,
    table: Pin,
    has: impl FnOnce(&Key) -> Result<bool, String>,
) -> Result<Opened, String> {
    let Some(slot) = newest else { return Ok(Opened::New) };
    if slot.pin != table {
        return Ok(Opened::Behind { saved: slot.pin, table });
    }
    match has(&slot.root)? {
        true => Ok(Opened::Open),
        false => Ok(Opened::Lost { missing: slot.root }),
    }
}
