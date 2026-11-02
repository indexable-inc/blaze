//! Release builds: what each function tells the others before any is compiled (#899, #176),
//! as ThinLTO's summaries do, and what a function's compile is given from them.
//!
//! A [`Summary`] is a function of the function alone, kept in the store under its instance
//! key, so a warm build decides every function's [`Imports`] from summaries without building
//! any IR. What a compile is given is in its blob's key ([`Imports::blob_key`]): every body it
//! may build in, by instance key (which holds the callee's code hash), and every fact of the
//! whole program it was compiled with. A callee's edit changes the key of every function that
//! imported it, and of no other.

#![forbid(unsafe_code)]

use std::cell::RefCell;

use blaze_format::cursor::Put;
use blaze_ids::{ContentId, Hasher};
use serde::{Deserialize, Serialize};

use crate::InstanceKey;

/// 2: the fields are hashed as fixed-width little-endian bytes, not as JSON.
pub const IMPORTS_DOMAIN: &str = "blaze v2 imports 2";

/// What a function's `#[inline]` asks, as rustc tells LLVM.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Hint {
    None,
    Inline,
    Always,
    Never,
}

/// An argument at a call site, as far as the caller's IR knows it. 16 bytes.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Arg {
    Unknown,
    /// An integer or pointer-free constant of `bits` bits, at most 64.
    Int { bits: u8, value: u64 },
}

const _: () = assert!(size_of::<Arg>() == 16);

impl Arg {
    /// Its bytes in [`Imports::blob_key`]: a tag, the width and the value, 10 in all.
    fn encode(self, out: &mut Vec<u8>) {
        match self {
            Arg::Unknown => out.put_bytes(&[0; 10]),
            Arg::Int { bits, value } => {
                out.put_bytes(&[1, bits]);
                out.put_u64(value);
            }
        }
    }
}

/// What LLVM's function attribute inference says of a function, which its callers may be
/// compiled knowing. 6 bytes; [`Attrs::bits`] packs it in one.
#[derive(Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq, Hash, Debug)]
pub struct Attrs {
    pub nounwind: bool,
    pub readonly: bool,
    pub readnone: bool,
    pub norecurse: bool,
    pub willreturn: bool,
    pub noreturn: bool,
}

const _: () = assert!(size_of::<Attrs>() == 6);

impl Attrs {
    pub const NOUNWIND: u8 = 1 << 0;
    pub const READONLY: u8 = 1 << 1;
    pub const READNONE: u8 = 1 << 2;
    pub const NORECURSE: u8 = 1 << 3;
    pub const WILLRETURN: u8 = 1 << 4;
    pub const NORETURN: u8 = 1 << 5;

    pub fn bits(self) -> u8 {
        u8::from(self.nounwind) * Attrs::NOUNWIND
            | u8::from(self.readonly) * Attrs::READONLY
            | u8::from(self.readnone) * Attrs::READNONE
            | u8::from(self.norecurse) * Attrs::NORECURSE
            | u8::from(self.willreturn) * Attrs::WILLRETURN
            | u8::from(self.noreturn) * Attrs::NORETURN
    }

    pub fn from_bits(bits: u8) -> Attrs {
        Attrs {
            nounwind: bits & Attrs::NOUNWIND != 0,
            readonly: bits & Attrs::READONLY != 0,
            readnone: bits & Attrs::READNONE != 0,
            norecurse: bits & Attrs::NORECURSE != 0,
            willreturn: bits & Attrs::WILLRETURN != 0,
            noreturn: bits & Attrs::NORETURN != 0,
        }
    }
}

/// A call site of a function. 64 bytes, its `args` on the heap.
#[derive(Serialize, Deserialize, PartialEq, Eq, Hash, Debug, Clone)] // clone: inside `Summary`
pub struct Call {
    pub callee: InstanceKey,
    /// How many call sites of it the function has.
    pub count: u32,
    /// Per parameter of the callee: what every one of these call sites passes, if they agree.
    pub args: Vec<Arg>,
}

const _: () = assert!(size_of::<Call>() == 64);

// clone: the memo keeps one copy of each summary a build made for the builds after it (driver.rs, flight.rs)
#[derive(Serialize, Deserialize, PartialEq, Eq, Hash, Debug, Clone)]
pub struct Summary {
    pub key: InstanceKey,
    /// Its instructions, as LLVM counts them after its pre-link pipeline.
    pub size: u32,
    pub hint: Hint,
    pub calls: Vec<Call>,
    /// Functions whose address it takes: they can be called from where no summary says.
    pub addressed: Vec<InstanceKey>,
    pub params: u32,
    /// The parameters its body never reads, by place.
    pub unused: Vec<u32>,
    /// What LLVM infers of it alone, knowing nothing of what it calls.
    pub attrs: Attrs,
    /// What LLVM infers of it when every function of the build it calls (every
    /// [`Call::callee`]) is nounwind, readnone, willreturn and norecurse: what holds of its
    /// own instructions, for [`crate::imports::decide`] to meet with its callees'. Its
    /// `noreturn` is [`Summary::attrs`]'s, as a noreturn callee on one path says nothing of
    /// the others.
    pub body: Attrs,
    /// Whether code outside the build can call it: an exported symbol, a patchable function
    /// of a live build. Nothing is assumed of its callers.
    pub external: bool,
}

/// What one release compile is given, decided from the summaries of the whole build.
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Eq, Hash, Debug)] // clone: the build takes each release function's copy to drop what it may not build in
pub struct Imports {
    /// The functions whose exports it may build in, at any depth, sorted.
    pub imported: Vec<InstanceKey>,
    /// Its parameters every caller passes the same constant for: it is compiled with them
    /// known.
    pub constants: Vec<(u32, Arg)>,
    /// What it may assume of the functions it calls, sorted by callee.
    pub callees: Vec<(InstanceKey, Attrs)>,
    /// Parameters of callees it need not compute, as no callee reads them: by callee, sorted.
    pub dead: Vec<(InstanceKey, Vec<u32>)>,
}

impl Imports {
    /// The store's key of the code compiled for `key` with these imports: one BLAKE3 over
    /// every field laid out in a buffer the thread reuses.
    pub fn blob_key(&self, key: InstanceKey) -> ContentId {
        thread_local! {
            static BYTES: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) }; // alloc: empty, the buffer the thread reuses
        }
        BYTES.with_borrow_mut(|bytes| {
            bytes.clear();
            self.encode(bytes);
            let mut hasher = Hasher::new(IMPORTS_DOMAIN);
            hasher.id(&key.0).update(bytes);
            hasher.finish()
        })
    }

    /// Every field, each list as its length then its elements at fixed widths, little-endian:
    /// two different `Imports` never give the same bytes.
    fn encode(&self, out: &mut Vec<u8>) {
        let dead: usize = self.dead.iter().map(|it| 36 + 4 * it.1.len()).sum();
        out.reserve(16 + 32 * self.imported.len() + 14 * self.constants.len() + 33 * self.callees.len() + dead);
        out.put_u32(self.imported.len() as u32);
        for key in &self.imported {
            out.put_bytes(&key.0.0);
        }
        out.put_u32(self.constants.len() as u32);
        for &(param, arg) in &self.constants {
            out.put_u32(param);
            arg.encode(out);
        }
        out.put_u32(self.callees.len() as u32);
        for (key, attrs) in &self.callees {
            out.put_bytes(&key.0.0);
            out.put_u8(attrs.bits());
        }
        out.put_u32(self.dead.len() as u32);
        for (key, params) in &self.dead {
            out.put_bytes(&key.0.0);
            out.put_u32(params.len() as u32);
            for param in params {
                out.put_u32(*param);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    #[test]
    fn an_import_is_in_the_key() {
        let none = Imports::default();
        let one = Imports { imported: vec![key(2)], ..Imports::default() };
        let other = Imports { imported: vec![key(3)], ..Imports::default() };
        let keys = [none.blob_key(key(1)), one.blob_key(key(1)), other.blob_key(key(1))];
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[1], keys[2]);
    }

    #[test]
    fn every_whole_program_fact_is_in_the_key() {
        let base = Imports::default().blob_key(key(1));
        let constant = Imports { constants: vec![(0, Arg::Int { bits: 32, value: 7 })], ..Imports::default() };
        let attrs = Imports {
            callees: vec![(key(2), Attrs { nounwind: true, ..Attrs::default() })],
            ..Imports::default()
        };
        let dead = Imports { dead: vec![(key(2), vec![1])], ..Imports::default() };
        for it in [constant, attrs, dead] {
            assert_ne!(it.blob_key(key(1)), base);
        }
    }

    #[test]
    fn the_key_tells_apart_what_json_did() {
        let int = |bits, value| Arg::Int { bits, value };
        let all = [
            Imports::default(),
            Imports { constants: vec![(0, int(32, 7))], ..Imports::default() },
            Imports { constants: vec![(0, int(64, 7))], ..Imports::default() },
            Imports { constants: vec![(1, int(32, 7))], ..Imports::default() },
            Imports { constants: vec![(0, Arg::Unknown)], ..Imports::default() },
            Imports { constants: vec![(0, int(0, 0))], ..Imports::default() },
            Imports { dead: vec![(key(2), vec![])], ..Imports::default() },
            Imports { dead: vec![(key(2), vec![0])], ..Imports::default() },
            Imports { dead: vec![(key(2), vec![]), (key(3), vec![])], ..Imports::default() },
            Imports { callees: vec![(key(2), Attrs::default())], ..Imports::default() },
            Imports { callees: vec![(key(2), Attrs { noreturn: true, ..Attrs::default() })], ..Imports::default() },
            Imports { imported: vec![key(2), key(3)], ..Imports::default() },
            Imports { imported: vec![key(3), key(2)], ..Imports::default() },
        ];
        let keys: Vec<ContentId> = all.iter().map(|it| it.blob_key(key(1))).collect();
        for (at, one) in keys.iter().enumerate() {
            assert!(keys[at + 1..].iter().all(|other| other != one), "{at}");
        }
        assert_ne!(all[1].blob_key(key(1)), all[1].blob_key(key(2)));
        // The buffer the thread reuses holds nothing over.
        assert_eq!(all[11].blob_key(key(1)), all[11].clone().blob_key(key(1)));
        assert_eq!(keys[0], Imports::default().blob_key(key(1)));
    }

    #[test]
    fn attrs_round_trip_through_their_bits() {
        for bits in 0..64u8 {
            assert_eq!(Attrs::from_bits(bits).bits(), bits);
        }
        let one = Attrs { readonly: true, willreturn: true, ..Attrs::default() };
        assert_eq!(one.bits(), Attrs::READONLY | Attrs::WILLRETURN);
    }

    #[test]
    fn sizes() {
        assert_eq!(size_of::<Arg>(), 16);
        assert_eq!(size_of::<Attrs>(), 6);
        assert_eq!(size_of::<Call>(), 64);
    }
}
