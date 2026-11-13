//! Keys (docs/v2/plan/design/std.md, "The rest of the set", `Keys`): the OS keychain and
//! hardware keys, by [`KeyName`]: sign, verify, decrypt, and fetch a [`Secret`]. Narrowed to
//! named keys by [`Keys::only`], which never widens. A key in hardware never leaves it: it
//! signs and decrypts, and a fetch of it is refused.
//!
//! **Provider.** The simulation only ([`crate::hosted::SimHost::keys`]): blaze-core has no
//! effect for a keychain. The simulation's "signature" is the BLAKE3 of the key's material and
//! the message, and its "decryption" an XOR with the material: enough for a program's logic
//! to be tested against, and not cryptography.

use alloc::vec::Vec;
use core::fmt;

use crate::{
    Stopped,
    cx::Cx,
    hosted::{self, Refused},
    io::Io,
    mem::Bytes,
    name::KeyName,
    narrow::Allowed,
};

/// A key of the simulated keychain.
#[derive(Debug)]
pub struct SimKey {
    pub name: KeyName,
    pub material: Vec<u8>,
    /// Whether it lives in hardware: then it is used and never fetched.
    pub hardware: bool,
}

/// Secret bytes fetched from the keychain. No `Debug`, `Display` or `Clone`, and zeroed when
/// dropped; read only through [`Secret::expose`], which says so at its call.
pub struct Secret {
    bytes: Vec<u8>,
}

impl Secret {
    /// The secret's bytes: every use of them is this call, by name.
    pub fn expose(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        for byte in &mut self.bytes {
            *byte = 0;
        }
        // The zeroing is of memory about to be freed: kept from being optimized away.
        core::hint::black_box(&self.bytes);
    }
}

/// What a keychain call was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeysOp {
    Sign,
    Verify,
    Decrypt,
    Fetch,
}

/// A keychain call that failed: the call, the key and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct KeysError {
    pub op: KeysOp,
    pub key: KeyName,
    pub kind: Refused,
}

impl fmt::Display for KeysError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::keys {:?} with key {}: {}", self.op, self.key, self.kind)
    }
}

impl core::error::Error for KeysError {}

/// The length of a signature.
pub const SIGNATURE_LEN: usize = 32;

/// The simulation's signature of `message` under `material`.
fn simulated_signature(material: &[u8], message: &[u8]) -> [u8; SIGNATURE_LEN] {
    // alloc: the simulated digest's input, freed as this returns.
    let mut input = Vec::with_capacity(8 + material.len() + message.len());
    input.extend_from_slice(&(material.len() as u64).to_le_bytes());
    input.extend_from_slice(material);
    input.extend_from_slice(message);
    blaze_ids::b3::hash(&input)
}

/// The keychain: from [`Io::keys`].
#[derive(Clone)]
pub struct Keys {
    io: Io,
    allowed: Allowed<KeyName>,
}

impl Keys {
    pub(crate) fn new(io: Io) -> Keys {
        Keys { io, allowed: Allowed::Any }
    }

    /// Only the keys of `names` this reaches already: never one more.
    pub fn only(&self, names: &[KeyName]) -> Keys {
        Keys { io: self.io.clone(), allowed: self.allowed.only(names, self.io.plant()) }
    }

    /// `f` on the key `key`, if this reaches it and the keychain has it.
    fn with_key<R>(
        &self,
        cx: &Cx,
        op: KeysOp,
        key: &KeyName,
        f: impl FnOnce(&SimKey) -> Result<R, Refused>,
    ) -> Result<R, KeysError> {
        // clone: an error's name of its key, a refcount.
        let refuse = |kind| KeysError { op, key: key.clone(), kind };
        if !self.allowed.allows(key) {
            return Err(refuse(Refused::NotAllowed));
        }
        hosted::call(&self.io, cx, |host| match host.keys.iter().find(|it| &it.name == key) {
            Some(found) => f(found),
            None => Err(Refused::NotFound),
        })
        .map_err(refuse)
    }

    /// The signature of `message` under `key`, [`SIGNATURE_LEN`] bytes added to `into`.
    pub fn sign(&self, cx: &Cx, key: &KeyName, message: &[u8], into: &mut Bytes<'_>) -> Result<(), KeysError> {
        let signature = self.with_key(cx, KeysOp::Sign, key, |found| Ok(simulated_signature(&found.material, message)))?;
        // clone: an error's name of its key, a refcount.
        into.extend(&signature).map_err(|stopped| KeysError { op: KeysOp::Sign, key: key.clone(), kind: stopped.into() })
    }

    /// Whether `signature` is `key`'s of `message`.
    pub fn verify(&self, cx: &Cx, key: &KeyName, message: &[u8], signature: &[u8]) -> Result<bool, KeysError> {
        self.with_key(cx, KeysOp::Verify, key, |found| Ok(simulated_signature(&found.material, message)[..] == *signature))
    }

    /// `sealed` decrypted under `key`, added to `into`; how many bytes.
    pub fn decrypt(&self, cx: &Cx, key: &KeyName, sealed: &[u8], into: &mut Bytes<'_>) -> Result<usize, KeysError> {
        let arena = into.arena();
        self.with_key(cx, KeysOp::Decrypt, key, |found| {
            if found.material.is_empty() {
                return Err(Refused::Host("the key has no material"));
            }
            let start = into.len();
            let slot = into.grow_zeroed(sealed.len()).map_err(|_| Stopped::OutOfMemory { arena, asked: sealed.len() })?;
            for (at, (out, byte)) in slot.iter_mut().zip(sealed).enumerate() {
                *out = byte ^ found.material[at % found.material.len()];
            }
            Ok(into.len() - start)
        })
    }

    /// The secret `key` holds. A key in hardware is refused: it never leaves it.
    pub fn fetch(&self, cx: &Cx, key: &KeyName) -> Result<Secret, KeysError> {
        self.with_key(cx, KeysOp::Fetch, key, |found| match found.hardware {
            true => Err(Refused::Host("the key is in hardware and never leaves it")),
            // alloc: the secret's bytes, zeroed and freed when it is dropped.
            false => Ok(Secret { bytes: found.material.clone() }),
        })
    }
}

impl fmt::Debug for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Keys({:?})", self.allowed)
    }
}
