//! Who says what a key's value is.
//!
//! A [`Record`] binds a key to its value: the key, the value's kind, its content hash (the
//! store's [`blaze_cas::content_of`]) and its length. A publisher signs the record with
//! Ed25519 ([`Publisher`]); a machine takes a value only when a key it pins signed its record
//! ([`Trust`]) and the bytes hash to the record's content. The signed message is the record
//! behind a domain of its own, so no signature of another protocol is a record's.
//!
//! A key's value never changes (a key is a hash of everything the value is made from), so a
//! record that was true once is true always: an old record replayed is the same record, and
//! there is no turn or expiry to check.

use std::fmt;

use blaze_cas::{Key, Kind};
use blaze_ids::ContentId;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey};

use crate::Refused;

pub(crate) use blaze_format::hex::encode as hex;

/// Key, kind, content, length.
pub const RECORD_LEN: usize = 32 + 1 + 32 + 8;
/// A record, its publisher's key and the signature.
pub const SIGNED_LEN: usize = RECORD_LEN + 32 + 64;

const RECORD_DOMAIN: &[u8] = b"blaze v2 global record 1\0";
const DIGEST_DOMAIN: &[u8] = b"blaze v2 global digest 1\0";

/// The publisher keys of blaze's own builders, which this blaze takes records from. Rotated
/// by a release of blaze: a machine trusts the builders its blaze names.
pub const PINNED: &[PublisherKey] = &[];

/// What a key's value is, as a publisher says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Record {
    pub key: Key,
    pub kind: Kind,
    /// [`blaze_cas::content_of`] of the value's bytes.
    pub content: ContentId,
    pub len: u64,
}

impl Record {
    /// The record of `bytes` as the value of `key`.
    pub fn of(key: Key, kind: Kind, bytes: &[u8]) -> Record {
        Record { key, kind, content: blaze_cas::content_of(bytes), len: bytes.len() as u64 }
    }

    pub fn encode(&self) -> [u8; RECORD_LEN] {
        let mut out = [0; RECORD_LEN];
        out[..32].copy_from_slice(&self.key.0.0);
        out[32] = self.kind.code();
        out[33..65].copy_from_slice(&self.content.0);
        out[65..].copy_from_slice(&self.len.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8; RECORD_LEN]) -> Result<Record, String> {
        // alloc: an error's message, made only when it fails.
        let kind = Kind::from_code(bytes[32]).ok_or_else(|| format!("a record of an unknown kind code {}", bytes[32]))?;
        Ok(Record {
            key: Key(ContentId(bytes[..32].try_into().expect("32 bytes"))),
            kind,
            content: ContentId(bytes[33..65].try_into().expect("32 bytes")),
            len: u64::from_le_bytes(bytes[65..].try_into().expect("8 bytes")),
        })
    }

    fn message(&self) -> [u8; RECORD_DOMAIN.len() + RECORD_LEN] {
        let mut out = [0; RECORD_DOMAIN.len() + RECORD_LEN];
        out[..RECORD_DOMAIN.len()].copy_from_slice(RECORD_DOMAIN);
        out[RECORD_DOMAIN.len()..].copy_from_slice(&self.encode());
        out
    }
}

/// A publisher's Ed25519 public key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PublisherKey(pub [u8; 32]);

impl fmt::Debug for PublisherKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublisherKey({})", hex(&self.0))
    }
}

impl fmt::Display for PublisherKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex(&self.0))
    }
}

/// A record as a publisher signed it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Signed {
    pub record: Record,
    pub publisher: PublisherKey,
    pub signature: [u8; 64],
}

impl Signed {
    pub fn encode(&self) -> [u8; SIGNED_LEN] {
        let mut out = [0; SIGNED_LEN];
        out[..RECORD_LEN].copy_from_slice(&self.record.encode());
        out[RECORD_LEN..RECORD_LEN + 32].copy_from_slice(&self.publisher.0);
        out[RECORD_LEN + 32..].copy_from_slice(&self.signature);
        out
    }

    pub fn decode(bytes: &[u8; SIGNED_LEN]) -> Result<Signed, String> {
        Ok(Signed {
            record: Record::decode(bytes[..RECORD_LEN].try_into().expect("a record's bytes"))?,
            publisher: PublisherKey(bytes[RECORD_LEN..RECORD_LEN + 32].try_into().expect("32 bytes")),
            signature: bytes[RECORD_LEN + 32..].try_into().expect("64 bytes"),
        })
    }
}

/// One of blaze's builders, signing what it publishes. Its seed is a secret: it is never
/// printed, and [`crate::Secrets::publisher`] refuses any value holding it.
pub struct Publisher {
    pair: Ed25519KeyPair,
    seed: [u8; 32],
    key: PublisherKey,
}

impl fmt::Debug for Publisher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Publisher({})", self.key)
    }
}

impl Publisher {
    /// The publisher whose Ed25519 private key is `seed` (RFC 8032's 32-byte secret).
    pub fn from_seed(seed: &[u8; 32]) -> Result<Publisher, String> {
        let pair = Ed25519KeyPair::from_seed_unchecked(seed)
            // alloc: an error's message, made only when it fails.
            .map_err(|error| format!("an Ed25519 seed refused: {error}"))?;
        let key = PublisherKey(pair.public_key().as_ref().try_into().expect("an Ed25519 public key is 32 bytes"));
        Ok(Publisher { pair, seed: *seed, key })
    }

    pub fn key(&self) -> PublisherKey {
        self.key
    }

    pub fn sign(&self, record: Record) -> Signed {
        let signature = self.pair.sign(&record.message());
        Signed {
            record,
            publisher: self.key,
            signature: signature.as_ref().try_into().expect("an Ed25519 signature is 64 bytes"),
        }
    }

    /// The signature of `digest` as a `what` of the global cache (a bundle's manifest).
    pub(crate) fn sign_digest(&self, what: &'static [u8], digest: &ContentId) -> [u8; 64] {
        let signature = self.pair.sign(&digest_message(what, digest));
        signature.as_ref().try_into().expect("an Ed25519 signature is 64 bytes")
    }

    pub(crate) fn seed(&self) -> &[u8; 32] {
        &self.seed
    }
}

fn digest_message(what: &'static [u8], digest: &ContentId) -> Vec<u8> {
    let mut out = Vec::with_capacity(DIGEST_DOMAIN.len() + what.len() + 33);
    out.extend_from_slice(DIGEST_DOMAIN);
    out.extend_from_slice(what);
    out.push(0);
    out.extend_from_slice(&digest.0);
    out
}

/// The publisher keys a machine takes records from.
#[derive(Clone, Debug)]
pub struct Trust {
    pinned: Vec<PublisherKey>,
}

impl Trust {
    /// The keys this blaze pins ([`PINNED`]).
    pub fn pinned() -> Result<Trust, String> {
        // alloc: setup: once a trust.
        Trust::new(PINNED.to_vec())
    }

    /// Refuses no keys: a cache no one is trusted to publish to is not one to ask.
    pub fn new(mut pinned: Vec<PublisherKey>) -> Result<Trust, String> {
        if pinned.is_empty() {
            return Err("no publisher of the global cache is pinned: this blaze takes nothing from it".into());
        }
        pinned.sort_unstable();
        pinned.dedup();
        Ok(Trust { pinned })
    }

    pub fn trusts(&self, publisher: &PublisherKey) -> bool {
        self.pinned.binary_search(publisher).is_ok()
    }

    /// Whether a pinned publisher signed `signed`: the publisher first, then the signature.
    pub fn verify(&self, signed: &Signed) -> Result<(), Refused> {
        let refused = |what: String| Refused { key: signed.record.key, what };
        if !self.trusts(&signed.publisher) {
            // alloc: an error's message, made only when it fails.
            return Err(refused(format!("its record is signed by {}, which this blaze does not pin", signed.publisher)));
        }
        UnparsedPublicKey::new(&ED25519, &signed.publisher.0)
            .verify(&signed.record.message(), &signed.signature)
            // alloc: an error's message, made only when it fails.
            .map_err(|_| refused(format!("its record's signature is not {}'s", signed.publisher)))
    }

    /// Whether a pinned publisher signed `digest` as a `what`.
    pub(crate) fn verify_digest(
        &self,
        what: &'static [u8],
        digest: &ContentId,
        publisher: &PublisherKey,
        signature: &[u8; 64],
    ) -> Result<(), String> {
        if !self.trusts(publisher) {
            // alloc: an error's message, made only when it fails.
            return Err(format!("signed by {publisher}, which this blaze does not pin"));
        }
        UnparsedPublicKey::new(&ED25519, &publisher.0)
            .verify(&digest_message(what, digest), signature)
            // alloc: an error's message, made only when it fails.
            .map_err(|_| format!("its signature is not {publisher}'s"))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn publisher(n: u8) -> Publisher {
        Publisher::from_seed(&[n; 32]).unwrap()
    }

    fn record() -> Record {
        Record::of(Key(ContentId([5; 32])), Kind::Code, b"the code")
    }

    #[test]
    fn records_and_signed_records_round_trip() {
        let signed = publisher(1).sign(record());
        assert_eq!(Record::decode(&signed.record.encode()), Ok(signed.record));
        assert_eq!(Signed::decode(&signed.encode()), Ok(signed));
        let mut bad = signed.encode();
        bad[32] = 77;
        assert!(Signed::decode(&bad).unwrap_err().contains("unknown kind code 77"));
    }

    #[test]
    fn a_pinned_signature_is_taken() {
        let trust = Trust::new(vec![publisher(1).key()]).unwrap();
        assert_eq!(trust.verify(&publisher(1).sign(record())), Ok(()));
    }

    #[test]
    fn a_signature_by_a_key_not_pinned_is_refused() {
        let trust = Trust::new(vec![publisher(1).key()]).unwrap();
        let error = trust.verify(&publisher(2).sign(record())).unwrap_err();
        assert_eq!(error.key, record().key);
        assert!(error.what.contains("does not pin"), "{error}");
    }

    #[test]
    fn every_field_of_a_record_is_signed() {
        let trust = Trust::new(vec![publisher(1).key()]).unwrap();
        let signed = publisher(1).sign(record());
        let changes: [fn(&mut Signed); 5] = [
            |it| it.record.key = Key(ContentId([6; 32])),
            |it| it.record.kind = Kind::Run,
            |it| it.record.content = ContentId([7; 32]),
            |it| it.record.len += 1,
            |it| it.signature[9] ^= 1,
        ];
        for change in changes {
            let mut changed = signed;
            change(&mut changed);
            let error = trust.verify(&changed).unwrap_err();
            assert!(error.what.contains("signature"), "{error}");
        }
    }

    #[test]
    fn a_publisher_claimed_by_another_signature_is_refused() {
        let trust = Trust::new(vec![publisher(1).key(), publisher(2).key()]).unwrap();
        let mut signed = publisher(2).sign(record());
        signed.publisher = publisher(1).key();
        assert!(trust.verify(&signed).is_err());
    }

    #[test]
    fn digests_are_signed_as_what_they_are() {
        let trust = Trust::new(vec![publisher(1).key()]).unwrap();
        let digest = ContentId([3; 32]);
        let signature = publisher(1).sign_digest(b"bundle", &digest);
        assert_eq!(trust.verify_digest(b"bundle", &digest, &publisher(1).key(), &signature), Ok(()));
        assert!(trust.verify_digest(b"other", &digest, &publisher(1).key(), &signature).is_err());
        assert!(trust.verify_digest(b"bundle", &ContentId([4; 32]), &publisher(1).key(), &signature).is_err());
    }

    #[test]
    fn no_pinned_key_is_refused_and_debug_hides_the_seed() {
        assert!(Trust::new(Vec::new()).unwrap_err().contains("no publisher"));
        let shown = format!("{:?}", publisher(171));
        assert!(!shown.contains(&hex(&[171; 32])), "{shown}");
    }
}
