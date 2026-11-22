//! A published set (published.md): the publishable records of one toolchain for one target,
//! laid out by [`super::lay_out_bound`] under `public/<toolchain>/<target>/` of an origin. Three
//! things make a set one fact a reader can hold a publisher to:
//!
//! - ITS NAME ([`SetName`]): two parts of `[0-9a-z._-]`, each 1 to 64 bytes and starting with a
//!   letter or digit, so neither holds a `/` or is `.` or `..`: the set's folder never leaves the
//!   origin's. Its id ([`SetName::id`]) is the digest of the two parts.
//! - ITS HEAD IS BOUND TO THE NAME: the head's signature is over `id || head[..96]`
//!   (`read::publish_bound`), so a head another set's publisher signed with the same key is not
//!   this set's: [`SetCheck`] refuses it as a signature that does not stand.
//! - ITS GENERATION ONLY RISES (the rollback rule): a publisher lays a set out at the generation
//!   after the one its folder's head holds (equal records lay out equal bytes at the same one, so
//!   a publish of nothing new writes nothing new); a reader keeps the highest generation it has
//!   taken for the set (blaze-cli store/published.rs keeps it as a record of the home's store,
//!   under a root named by the set's id) and [`SetCheck`] refuses a head below it, after its
//!   signature stood, before any other field of it is believed ([`SignatureWhy::Stale`]).
//!
//! The order an origin's folder is written in ([`write_rank`]) is the last part: every object
//! named by its digest first, then the `prefix`, then the `head`, each replaced whole by a rename.
//! A reader reads the prefix (or the head) and, through it, only objects named by digests it
//! checked; a digest-named object is never replaced by other bytes. So a reader sees the old set
//! whole or the new one whole, whatever point a publish stopped at.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use core::fmt;

use blaze_cas::plain::Namespace;
use blaze_ids::ContentId;

use crate::read::form::{HEAD_DOMAIN, HeadCheck, Object, SignatureWhy};
use crate::trust::PublicKey;

/// The domain a set's id is hashed under.
pub const SET_DOMAIN: &str = "blaze-remote published set 1";
/// One part of a set's name, at most.
pub const PART_MOST: usize = 64;
/// The folder of an origin the public namespace's sets are under.
pub const PUBLIC_DIR: &str = "public";

/// A set's name: the toolchain's identity and the target, each checked ([`SetName::new`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SetName<'s> {
    toolchain: &'s str,
    target: &'s str,
}

/// Why a part of a set's name was refused: the part and what is wrong (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetNameError {
    Empty { what: &'static str },
    TooLong { what: &'static str, len: usize },
    /// A byte outside `[0-9a-z._-]`, or a first byte that is not a letter or digit.
    Byte { what: &'static str, at: usize, byte: u8 },
}

impl fmt::Display for SetNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SetNameError::Empty { what } => write!(f, "a published set's {what} is empty"),
            SetNameError::TooLong { what, len } => write!(f, "a published set's {what} is {len} bytes, past the {PART_MOST} a name part holds"),
            SetNameError::Byte { what, at, byte } => {
                write!(f, "a published set's {what} has byte {byte:#04x} at {at}: a part is `[0-9a-z._-]` and starts with a letter or digit, so it never leaves its folder")
            }
        }
    }
}

impl core::error::Error for SetNameError {}

fn part(what: &'static str, text: &str) -> Result<(), SetNameError> {
    if text.is_empty() {
        return Err(SetNameError::Empty { what });
    }
    if text.len() > PART_MOST {
        return Err(SetNameError::TooLong { what, len: text.len() });
    }
    for (at, &byte) in text.as_bytes().iter().enumerate() {
        let alnum = byte.is_ascii_lowercase() || byte.is_ascii_digit();
        let ok = alnum || (at > 0 && matches!(byte, b'.' | b'_' | b'-'));
        if !ok {
            return Err(SetNameError::Byte { what, at, byte });
        }
    }
    Ok(())
}

impl<'s> SetName<'s> {
    /// The set of `toolchain` (its id's 64 hex digits, as blaze-toolchain `ToolchainId::hex`
    /// spells it) for `target` (a triple).
    pub fn new(toolchain: &'s str, target: &'s str) -> Result<SetName<'s>, SetNameError> {
        part("toolchain", toolchain)?;
        part("target", target)?;
        Ok(SetName { toolchain, target })
    }

    pub fn toolchain(&self) -> &'s str {
        self.toolchain
    }

    pub fn target(&self) -> &'s str {
        self.target
    }

    /// The set's id: the digest of `toolchain || 0 || target`, which its head's signature is
    /// bound to.
    pub fn id(&self) -> ContentId {
        let mut bytes = [0u8; 2 * PART_MOST + 1];
        let (toolchain, target) = (self.toolchain.as_bytes(), self.target.as_bytes());
        bytes[..toolchain.len()].copy_from_slice(toolchain);
        let at = toolchain.len() + 1;
        bytes[at..at + target.len()].copy_from_slice(target);
        ContentId::of(SET_DOMAIN, &bytes[..at + target.len()])
    }

    /// The set's folder beneath an origin, part by part: `public`, the toolchain, the target.
    pub fn dir(&self) -> [&'s str; 3] {
        [PUBLIC_DIR, self.toolchain, self.target]
    }
}

/// The reader's check of a set's head: signed by a key trusted for the origin (trust.rs
/// `trusted_for`: the compiled-in `PUBLISHED_KEYS`, or one of `keys`, those the command named
/// for it), over the set's id and the head, at a generation no lower than `seen` (the highest
/// this home has taken for the set; 0 when none).
pub struct SetCheck<'k> {
    pub set: ContentId,
    pub keys: &'k [PublicKey],
    pub seen: u64,
}

impl HeadCheck for SetCheck<'_> {
    fn check(&self, namespace: Namespace, signer: &PublicKey, domain: &'static str, message: &[u8], signature: &[u8; 64]) -> Result<(), SignatureWhy> {
        if namespace != Namespace::PUBLIC || !crate::trust::trusted_for(signer, self.keys) {
            return Err(SignatureWhy::UnknownSigner);
        }
        crate::trust::verify_bound(signer, domain, &self.set.0, message, signature).map_err(SignatureWhy::Invalid)?;
        // Believed only now that the signature stands: the head's generation (bytes 16..24,
        // read/form.rs).
        if domain == HEAD_DOMAIN && message.len() >= 24 {
            let mut word = [0u8; 8];
            word.copy_from_slice(&message[16..24]);
            let generation = u64::from_le_bytes(word);
            if generation < self.seen {
                return Err(SignatureWhy::Stale { generation, seen: self.seen });
            }
        }
        Ok(())
    }
}

/// Where an object goes in a publish of a set: 0 for an object named by its digest (written
/// first, in any order: immutable, and equal bytes whoever writes it), 1 for the `prefix`, 2 for
/// the `head`, the last. A writer writes each whole and renames it into place, in rising rank.
pub fn write_rank(object: &Object) -> u8 {
    match object {
        Object::Head => 2,
        Object::Prefix => 1,
        Object::Manifest(_) | Object::Segment(_) | Object::Summary(_) | Object::Index(_) => 0,
    }
}

/// Whether `name` is one an object of a set may have: `head`, `prefix`, or `manifest-`, `seg-`,
/// `sum-` or `idx-` and 64 lowercase hex digits. Every [`Object::name`] is; nothing else is, so
/// no name a reader or writer uses can leave the set's folder.
pub fn is_object_name(name: &str) -> bool {
    if name == "head" || name == "prefix" {
        return true;
    }
    let digest = ["manifest-", "seg-", "sum-", "idx-"].iter().find_map(|prefix| name.strip_prefix(prefix));
    match digest {
        Some(hex) => hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_name_never_leaves_its_folder() {
        let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let set = SetName::new(id, "aarch64-apple-darwin").expect("a toolchain id and a triple");
        assert_eq!(set.dir(), ["public", id, "aarch64-apple-darwin"]);
        for bad in ["", ".", "..", "../x", "a/b", "A", ".hidden", "-x", "a b"] {
            assert!(SetName::new(bad, "t").is_err(), "{bad:?} taken as a toolchain");
            assert!(SetName::new("t", bad).is_err(), "{bad:?} taken as a target");
        }
        let long = [b'a'; PART_MOST + 1];
        let long = core::str::from_utf8(&long).expect("ASCII");
        assert_eq!(SetName::new(long, "t"), Err(SetNameError::TooLong { what: "toolchain", len: PART_MOST + 1 }));
        // The id keeps the parts apart: `ab`/`c` is not `a`/`bc`.
        assert_ne!(SetName::new("ab", "c").expect("a name").id(), SetName::new("a", "bc").expect("a name").id());
    }

    #[test]
    fn every_object_name_is_a_digest_name_and_nothing_else_is() {
        let id = ContentId([0xab; 32]);
        for object in [Object::Head, Object::Prefix, Object::Manifest(id), Object::Segment(id), Object::Summary(id), Object::Index(id)] {
            let name = object.name();
            assert!(is_object_name(name.as_str()), "{} is not a set's object name", name.as_str());
        }
        for bad in ["", "heads", "seg-", "seg-../../etc", "../head", "seg-ABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABAB"] {
            assert!(!is_object_name(bad), "{bad:?} taken as an object's name");
        }
        assert_eq!((write_rank(&Object::Segment(id)), write_rank(&Object::Prefix), write_rank(&Object::Head)), (0, 1, 2));
    }
}
