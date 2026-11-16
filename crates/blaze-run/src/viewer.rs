//! What a viewer sends a run's parent (rule 33): one versioned schema in deterministic CBOR,
//! over the parent's Unix socket (`<name>.sock` beside its registry entry, mode 0600, so only
//! the run's user reaches it). Today one request: [`Request::Toggle`], which the parent's
//! driver hands to [`crate::parent::Parent::toggle`].
//!
//! A request is the array `[version, kind, ...]`; a toggle is `[1, 0, hooks, on, [slot, ...]]`.
//! Deterministic: every head in its shortest form, every length definite, nothing after the
//! value; a decoder refuses any other form by name, and bounds the slots by its caller's cap
//! before it takes one (network CBOR decoded with limits).

use core::fmt;

use blaze_own::AVec;

use crate::{Hooks, SlotId};

/// The schema this blaze speaks.
pub const VERSION: u64 = 1;
const TOGGLE: u64 = 0;

const UINT: u8 = 0;
const ARRAY: u8 = 4;
const FALSE: u8 = 0xf4;
const TRUE: u8 = 0xf5;

/// A request, without its slots (pushed to the caller's buffer).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Request {
    /// Turn `hooks` on or off for each slot pushed: their hook stubs go in or out; nothing is
    /// compiled.
    Toggle { hooks: Hooks, on: bool },
}

/// Why bytes are not a request: each names the byte it is at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bad {
    Truncated { at: usize },
    /// A head in a longer form than its value needs, or an indefinite length.
    NotDeterministic { at: usize },
    Unexpected { at: usize, wanted: &'static str },
    Version { found: u64 },
    Kind { found: u64 },
    TooLarge { at: usize, what: &'static str, found: u64 },
    /// Hooks with a bit no hook has ([`Hooks::ALL`]).
    NoSuchHook { at: usize, found: u64 },
    TooMany { slots: u64, most: usize },
    Trailing { at: usize },
}

impl fmt::Display for Bad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Bad::Truncated { at } => write!(f, "a viewer request ends at byte {at}, mid-value"),
            Bad::NotDeterministic { at } => {
                write!(f, "a viewer request's head at byte {at} is not in its shortest definite form")
            }
            Bad::Unexpected { at, wanted } => write!(f, "a viewer request has no {wanted} at byte {at}"),
            Bad::Version { found } => {
                write!(f, "a viewer request of schema {found}, where this blaze speaks {VERSION}")
            }
            Bad::Kind { found } => write!(f, "a viewer request of unknown kind {found}"),
            Bad::TooLarge { at, what, found } => {
                write!(f, "a viewer request's {what} at byte {at} is {found}, past its type")
            }
            Bad::NoSuchHook { at, found } => write!(
                f,
                "a viewer request's hooks at byte {at} are {found:#x}, past every hook there is ({:#x})",
                Hooks::ALL.0
            ),
            Bad::TooMany { slots, most } => {
                write!(f, "a viewer request names {slots} slots, past the cap of {most}")
            }
            Bad::Trailing { at } => write!(f, "a viewer request goes on past its value, at byte {at}"),
        }
    }
}

impl core::error::Error for Bad {}

fn head(major: u8, value: u64, out: &mut AVec<'_, u8>) {
    let major = major << 5;
    match value {
        0..=23 => out.push(major | value as u8),
        24..=0xff => out.extend_from_slice(&[major | 24, value as u8]),
        0x100..=0xffff => {
            out.push(major | 25);
            out.extend_from_slice(&(value as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(major | 26);
            out.extend_from_slice(&(value as u32).to_be_bytes());
        }
        _ => {
            out.push(major | 27);
            out.extend_from_slice(&value.to_be_bytes());
        }
    }
}

/// Pushes the toggle of `hooks` to `on` for `slots` onto `out`.
pub fn toggle(slots: &[SlotId], hooks: Hooks, on: bool, out: &mut AVec<'_, u8>) {
    head(ARRAY, 5, out);
    head(UINT, VERSION, out);
    head(UINT, TOGGLE, out);
    head(UINT, u64::from(hooks.0), out);
    out.push(if on { TRUE } else { FALSE });
    head(ARRAY, slots.len() as u64, out);
    for slot in slots {
        head(UINT, u64::from(slot.0), out);
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, Bad> {
        let byte = *self.bytes.get(self.at).ok_or(Bad::Truncated { at: self.at })?;
        self.at += 1;
        Ok(byte)
    }

    /// One head of `major`: its value, refused unless in its shortest form.
    fn head(&mut self, major: u8, wanted: &'static str) -> Result<u64, Bad> {
        let at = self.at;
        let first = self.byte()?;
        if first >> 5 != major {
            return Err(Bad::Unexpected { at, wanted });
        }
        let (len, least) = match first & 0x1f {
            small @ 0..=23 => return Ok(u64::from(small)),
            24 => (1, 24),
            25 => (2, 0x100),
            26 => (4, 0x1_0000),
            27 => (8, 0x1_0000_0000),
            _ => return Err(Bad::NotDeterministic { at }),
        };
        let mut value = 0u64;
        for _ in 0..len {
            value = (value << 8) | u64::from(self.byte()?);
        }
        if value < least {
            return Err(Bad::NotDeterministic { at });
        }
        Ok(value)
    }

    fn array(&mut self, len: u64, wanted: &'static str) -> Result<(), Bad> {
        let at = self.at;
        match self.head(ARRAY, wanted)? == len {
            true => Ok(()),
            false => Err(Bad::Unexpected { at, wanted }),
        }
    }
}

/// The request in `bytes`, its slots pushed to `slots`: at most `most_slots` of them, refused
/// before any is taken.
pub fn read(bytes: &[u8], most_slots: usize, slots: &mut AVec<'_, SlotId>) -> Result<Request, Bad> {
    let mut reader = Reader { bytes, at: 0 };
    reader.array(5, "request of five fields")?;
    let found = reader.head(UINT, "schema version")?;
    if found != VERSION {
        return Err(Bad::Version { found });
    }
    let found = reader.head(UINT, "kind")?;
    if found != TOGGLE {
        return Err(Bad::Kind { found });
    }
    let at = reader.at;
    let found = reader.head(UINT, "hooks")?;
    let hooks = Hooks(u16::try_from(found).map_err(|_| Bad::TooLarge { at, what: "hooks", found })?);
    if hooks.0 & !Hooks::ALL.0 != 0 {
        return Err(Bad::NoSuchHook { at, found });
    }
    let at = reader.at;
    let on = match reader.byte()? {
        TRUE => true,
        FALSE => false,
        _ => return Err(Bad::Unexpected { at, wanted: "bool" }),
    };
    let len = reader.head(ARRAY, "array of slots")?;
    if len > most_slots as u64 {
        return Err(Bad::TooMany { slots: len, most: most_slots });
    }
    slots.reserve(len as usize);
    for _ in 0..len {
        let at = reader.at;
        let found = reader.head(UINT, "slot")?;
        slots.push(SlotId(u32::try_from(found).map_err(|_| Bad::TooLarge { at, what: "slot", found })?));
    }
    if reader.at != bytes.len() {
        return Err(Bad::Trailing { at: reader.at });
    }
    Ok(Request::Toggle { hooks, on })
}
