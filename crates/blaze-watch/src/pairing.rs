//! [`Pairing`]: how a browser comes to be let in, once.
//!
//! 1. The page (on ix.dev, its origin checked) asks the viewer to pair: the viewer makes a
//!    [`Ticket`] (the page's handle, 128 random bits) and a [`Code`] (8 characters, 40 bits),
//!    and the page shows the code.
//! 2. The next blaze command the person runs asks the viewer whether a pairing waits, prints
//!    the code and asks the person whether the browser shows the same; a yes confirms it. Only
//!    a command of the viewer's own user reaches the viewer's socket (watch.md, "Security").
//! 3. The page polls with its ticket and, once confirmed, is given its [`Key`] (256 random
//!    bits), once. The viewer keeps every key it gave (at most [`MOST_BROWSERS`]) in a file of
//!    its user's, mode 0600, written whole; the page keeps its own in its origin's storage and
//!    sends it as `Authorization: Bearer <base64url>` on every request.
//!
//! One pairing waits at a time; a newer one replaces it; one waits at most [`PENDING_FOR`]. A
//! person comparing codes, not typing them, is what keeps a page from pairing by guessing: a
//! code is never accepted from the page's side at all. Keys are compared in constant time, over
//! every key kept.

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Vec;

use crate::{Nanos, Random};

pub const CODE_LEN: usize = 8;
pub const MOST_BROWSERS: usize = 16;
pub const PENDING_FOR: Nanos = Nanos::secs(120);
/// Crockford's base32: no I, L, O or U, so a code read aloud is not misread.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// What the page and the command both show.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Code(pub [u8; CODE_LEN]);

impl Code {
    pub fn as_str(&self) -> &str {
        // Every byte is from the ASCII alphabet.
        core::str::from_utf8(&self.0).unwrap_or("")
    }
}

/// The page's handle on its pending pairing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ticket(pub [u8; 16]);

/// A paired browser's key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Key(pub [u8; 32]);

impl fmt::Debug for Key {
    // A key is a secret: its bytes never reach a log (Code Review Rules, "Secrets").
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key(..)")
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PairError {
    /// No pairing waits with that code (none, another, or it expired).
    NoSuch,
    /// Every browser slot is taken; one must be removed first.
    Full { most: usize },
}

impl fmt::Display for PairError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PairError::NoSuch => write!(f, "no browser waits to pair with that code"),
            PairError::Full { most } => write!(f, "{most} browsers are paired already, the most the viewer keeps"),
        }
    }
}

impl core::error::Error for PairError {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Waiting,
    Confirmed(Key),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pending {
    ticket: Ticket,
    code: Code,
    began: Nanos,
    phase: Phase,
}

/// What a page's poll learns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Poll {
    Waiting,
    /// Confirmed: the key, given this once.
    Key(Key),
    /// No such pairing (expired, replaced, or never).
    Unknown,
}

fn same<const N: usize>(a: &[u8; N], b: &[u8; N]) -> bool {
    a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

pub struct Pairing<A: Allocator> {
    keys: Vec<Key, A>,
    pending: Option<Pending>,
}

impl<A: Allocator> Pairing<A> {
    /// With the keys the viewer's file holds.
    pub fn new_in(keys: &[Key], alloc: A) -> Result<Pairing<A>, PairError> {
        if keys.len() > MOST_BROWSERS {
            return Err(PairError::Full { most: MOST_BROWSERS });
        }
        let mut kept = Vec::with_capacity_in(MOST_BROWSERS, alloc);
        kept.extend_from_slice(keys);
        Ok(Pairing { keys: kept, pending: None })
    }

    pub fn keys(&self) -> &[Key] {
        &self.keys
    }

    fn live(&mut self, now: Nanos) -> Option<&mut Pending> {
        if let Some(pending) = self.pending
            && now.since(pending.began) > PENDING_FOR
        {
            self.pending = None;
        }
        self.pending.as_mut()
    }

    /// The page asks to pair: its ticket and the code to show. Replaces a waiting one.
    pub fn start<R: Random>(&mut self, now: Nanos, random: &mut R) -> (Ticket, Code) {
        let mut ticket = [0u8; 16];
        random.fill(&mut ticket);
        let mut bits = [0u8; 5];
        random.fill(&mut bits);
        let bits = bits.iter().fold(0u64, |n, b| (n << 8) | u64::from(*b));
        let mut code = [0u8; CODE_LEN];
        for (i, c) in code.iter_mut().enumerate() {
            *c = ALPHABET[((bits >> (5 * i)) & 31) as usize];
        }
        let pending = Pending { ticket: Ticket(ticket), code: Code(code), began: now, phase: Phase::Waiting };
        self.pending = Some(pending);
        (pending.ticket, pending.code)
    }

    /// The code a blaze command should show the person, if a pairing waits.
    pub fn shown(&mut self, now: Nanos) -> Option<Code> {
        match self.live(now) {
            Some(Pending { code, phase: Phase::Waiting, .. }) => Some(*code),
            _ => None,
        }
    }

    /// The person said the browser shows `code`: a key is made and kept.
    pub fn confirm<R: Random>(&mut self, code: Code, now: Nanos, random: &mut R) -> Result<(), PairError> {
        let full = self.keys.len() >= MOST_BROWSERS;
        let Some(pending) = self.live(now) else {
            return Err(PairError::NoSuch);
        };
        if pending.phase != Phase::Waiting || !same(&pending.code.0, &code.0) {
            return Err(PairError::NoSuch);
        }
        if full {
            return Err(PairError::Full { most: MOST_BROWSERS });
        }
        let mut key = [0u8; 32];
        random.fill(&mut key);
        pending.phase = Phase::Confirmed(Key(key));
        self.keys.push(Key(key));
        Ok(())
    }

    /// The person said no: the pairing is dropped.
    pub fn decline(&mut self) {
        self.pending = None;
    }

    pub fn poll(&mut self, ticket: Ticket, now: Nanos) -> Poll {
        let Some(pending) = self.live(now) else {
            return Poll::Unknown;
        };
        if !same(&pending.ticket.0, &ticket.0) {
            return Poll::Unknown;
        }
        match pending.phase {
            Phase::Waiting => Poll::Waiting,
            Phase::Confirmed(key) => {
                self.pending = None;
                Poll::Key(key)
            }
        }
    }

    /// Whether `key` is a paired browser's: every kept key compared, in constant time.
    pub fn check(&self, key: &[u8; 32]) -> bool {
        self.keys.iter().fold(false, |found, kept| found | same(&kept.0, key))
    }

    /// The keys file: the kept keys, 32 bytes each, in the order given.
    pub fn write_keys<B: Allocator>(&self, out: &mut Vec<u8, B>) {
        for key in self.keys.iter() {
            out.extend_from_slice(&key.0);
        }
    }
}
