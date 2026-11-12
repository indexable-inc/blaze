//! A pack's head: which pack the pin of a scope is, and who says so.
//!
//! **The trust rule.** A hash proves bytes are the named ones. It does not prove a record is
//! an honest build of what its key was hashed from: a wrong record under a real key is code
//! in the user's program. So a record is taken for its key only when someone the user trusts
//! said so, and saying so is a head: it names one manifest (by hash, so every key, length and
//! content hash in it), and it is either
//!
//! - signed (Ed25519) by a key in the user's [`Trust`]: the release key for what a release
//!   publishes, a team's or a machine's key for what a peer serves; or
//! - the user's own word: a file they named to `blaze store import --unsigned`.
//!
//! Nothing else maps a key to bytes: not a source's address, not a peer's presence on the
//! network, not a manifest that merely hashes to its name. An untrusted source can therefore
//! do two things only: withhold (a miss, built locally) and send wrong bytes (caught by the
//! record's hash, the source named). What a *trusted* signer can do is everything; that is
//! what trusting means, and why the keys are the user's to list.
//!
//! A head is the one mutable name of the layout (`<scope>/<pin>.head`). An old head played
//! again names an old pack, whose records are true forever (a key covers every input of its
//! value), so there is no turn or expiry to check: the worst an old head does is offer
//! records no build asks for.
//!
//! Its bytes: one CBOR array,
//!
//! ```text
//! [ "blaze pack head", 1, pin, scope, pack, manifest_len, body_len, records, signer, signature ]
//! ```
//!
//! with `signer` 32 bytes and `signature` 64, or both empty for an unsigned head. The signed
//! message is a domain of its own, then the same array with the signature empty.

use alloc::{
    format,
    string::{String, ToString as _},
    vec::Vec,
};

use crate::{Id, Manifest, Refused, Signatures, cbor, layout, manifest};

const MAGIC: &str = "blaze pack head";
pub const LAYOUT: u64 = 1;
/// The most bytes of a head: what a reader asks a source for at most.
pub const HEAD_MOST: u64 = 1024;
const DOMAIN: &[u8] = b"blaze pack head signature 1\0";
const WHAT: &str = "the pack's head";

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Head {
    pub pin: String,
    pub scope: String,
    /// The pack: the hash of its manifest's bytes.
    pub pack: Id,
    pub manifest_len: u64,
    pub body_len: u64,
    pub records: u64,
    /// The signer's public key and its signature, or none.
    pub signed: Option<([u8; 32], [u8; 64])>,
}

/// Who vouched for a head.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vouched {
    /// A key of the user's [`Trust`] signed it.
    Signer([u8; 32]),
    /// It is unsigned and the user said to take it.
    User,
}

/// The public keys whose heads the user takes.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Trust {
    keys: Vec<[u8; 32]>,
}

impl Trust {
    pub fn new(mut keys: Vec<[u8; 32]>) -> Trust {
        keys.sort_unstable();
        keys.dedup();
        Trust { keys }
    }

    pub fn trusts(&self, key: &[u8; 32]) -> bool {
        self.keys.binary_search(key).is_ok()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

impl Head {
    /// The unsigned head of `manifest`, whose bytes are `bytes` and hash to `pack`.
    pub fn of(manifest: &Manifest, pack: Id, bytes: &[u8]) -> Head {
        Head {
            // alloc: the head's own names, once a head
            pin: manifest.pin().to_string(),
            scope: manifest.scope().to_string(),
            pack,
            manifest_len: bytes.len() as u64,
            body_len: manifest.body_len(),
            records: manifest.records().len() as u64,
            signed: None,
        }
    }

    fn write(&self, signature: Option<&[u8; 64]>, out: &mut Vec<u8>) {
        cbor::array(out, 10);
        cbor::text(out, MAGIC);
        cbor::uint(out, LAYOUT);
        cbor::text(out, &self.pin);
        cbor::text(out, &self.scope);
        cbor::bytes(out, &self.pack.0);
        cbor::uint(out, self.manifest_len);
        cbor::uint(out, self.body_len);
        cbor::uint(out, self.records);
        match &self.signed {
            Some((signer, _)) => cbor::bytes(out, signer),
            None => cbor::bytes(out, &[]),
        }
        match signature {
            Some(signature) => cbor::bytes(out, signature),
            None => cbor::bytes(out, &[]),
        }
    }

    /// What `signer` signs to vouch for this head: sign it, then [`Head::signed_by`].
    pub fn message(&self, signer: &[u8; 32]) -> Vec<u8> {
        let as_signed = Head { signed: Some((*signer, [0; 64])), ..self.clone() };
        // alloc: the message, once a signature made or checked
        let mut out = Vec::with_capacity(DOMAIN.len() + 256);
        out.extend_from_slice(DOMAIN);
        as_signed.write(None, &mut out);
        out
    }

    /// This head with `signer`'s `signature` of [`Head::message`].
    pub fn signed_by(mut self, signer: [u8; 32], signature: [u8; 64]) -> Head {
        self.signed = Some((signer, signature));
        self
    }

    pub fn encode(&self) -> Vec<u8> {
        // alloc: the head's bytes, which are the output
        let mut out = Vec::with_capacity(256);
        self.write(self.signed.as_ref().map(|(_, signature)| signature), &mut out);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Head, Refused> {
        if bytes.len() as u64 > HEAD_MOST {
            return Err(Refused::TooLarge { what: WHAT, len: bytes.len() as u64, most: HEAD_MOST });
        }
        let mut reader = cbor::Reader::new(bytes);
        let malformed = |error: cbor::Error| Refused::Malformed { what: WHAT, at: error.at, why: error.why };
        let foreign = || Refused::Layout { what: WHAT, found: "not a blaze pack head".to_string() };
        if reader.array().map_err(|_| foreign())? != 10 {
            return Err(foreign());
        }
        if reader.text().map_err(|_| foreign())? != MAGIC {
            return Err(foreign());
        }
        let version = reader.uint().map_err(malformed)?;
        if version != LAYOUT {
            // alloc: an error's text
            return Err(Refused::Layout { what: WHAT, found: format!("of layout {version}, not {LAYOUT}") });
        }
        let pin = layout::name("the pin", reader.text().map_err(malformed)?)?.to_string();
        let scope = layout::name("the scope", reader.text().map_err(malformed)?)?.to_string();
        let pack = Id(reader.fixed::<32>().map_err(malformed)?);
        let manifest_len = reader.uint().map_err(malformed)?;
        let body_len = reader.uint().map_err(malformed)?;
        let records = reader.uint().map_err(malformed)?;
        let at = reader.at();
        let signer = reader.bytes().map_err(malformed)?;
        let signature = reader.bytes().map_err(malformed)?;
        reader.end().map_err(malformed)?;
        for (what, len, most) in [
            ("the pack's manifest", manifest_len, manifest::MANIFEST_MOST),
            ("the pack's body", body_len, manifest::BODY_MOST),
            ("the pack's record count", records, manifest::RECORDS_MOST),
        ] {
            if len > most {
                return Err(Refused::TooLarge { what, len, most });
            }
        }
        let signed = match (<[u8; 32]>::try_from(signer), <[u8; 64]>::try_from(signature)) {
            (Ok(signer), Ok(signature)) => Some((signer, signature)),
            _ if signer.is_empty() && signature.is_empty() => None,
            _ => {
                return Err(Refused::Malformed {
                    what: WHAT,
                    at,
                    why: "a signer and a signature that are neither 32 and 64 bytes nor both empty",
                });
            }
        };
        Ok(Head { pin, scope, pack, manifest_len, body_len, records, signed })
    }

    /// Who vouches for this head, or why no one does: a signer of `trust` whose signature
    /// verifies; or, for an unsigned head, the user, when `unsigned` says they named it so.
    /// A head signed by a key outside `trust`, or with a signature that is not its signer's,
    /// is refused whatever `unsigned` says: a signature that fails is never an unsigned head.
    pub fn vouched(
        &self,
        trust: &Trust,
        signatures: &impl Signatures,
        unsigned: bool,
    ) -> Result<Vouched, Refused> {
        match &self.signed {
            None if unsigned => Ok(Vouched::User),
            None => Err(Refused::Untrusted {
                // alloc: an error's text
                what: format!(
                    "the head of pin `{}` (pack {}) is unsigned; a pack is taken when a trusted key signed its head, or when you import its file yourself with `--unsigned`",
                    self.pin, self.pack
                ),
            }),
            Some((signer, signature)) => {
                if !trust.trusts(signer) {
                    return Err(Refused::Untrusted {
                        // alloc: an error's text
                        what: format!(
                            "the head of pin `{}` (pack {}) is signed by {}, which is not among the keys you trust",
                            self.pin,
                            self.pack,
                            Id(*signer)
                        ),
                    });
                }
                if !signatures.verify(signer, &self.message(signer), signature) {
                    return Err(Refused::Untrusted {
                        // alloc: an error's text
                        what: format!(
                            "the signature on the head of pin `{}` (pack {}) is not {}'s",
                            self.pin,
                            self.pack,
                            Id(*signer)
                        ),
                    });
                }
                Ok(Vouched::Signer(*signer))
            }
        }
    }

    /// Whether `manifest`, already checked to hash to [`Head::pack`], is what this head says
    /// of it: a head and a manifest that disagree are refused naming the field.
    pub fn agrees(&self, manifest: &Manifest) -> Result<(), Refused> {
        let differs = |field: &'static str| Refused::Malformed {
            what: WHAT,
            at: 0,
            why: field,
        };
        if self.pin != manifest.pin() {
            return Err(differs("its pin is not its manifest's"));
        }
        if self.scope != manifest.scope() {
            return Err(differs("its scope is not its manifest's"));
        }
        if self.body_len != manifest.body_len() {
            return Err(differs("its body length is not the sum of its manifest's records"));
        }
        if self.records != manifest.records().len() as u64 {
            return Err(differs("its record count is not its manifest's"));
        }
        Ok(())
    }
}
