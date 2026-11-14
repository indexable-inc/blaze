//! Trust: Ed25519 signatures by `ring` 0.17 (`ring::signature::ED25519`), the same
//! implementation blaze-global and blaze-pack use.
//!
//! - An anchor key is trusted per namespace in settings. It signs key lists only, and is kept
//!   offline by the owner.
//! - A key list (`<ns>/keys/<serial>`) names builder keys with roles ([`Roles::BUILD`] signs
//!   statements and bundles, [`Roles::ROOT`] signs roots), a validity window and a revoked flag.
//!   Rotation is a new list with a higher serial. Revocation is a list with the key marked
//!   revoked, which refuses every object it signed, old ones included. A client keeps the
//!   highest serial it has seen per namespace and refuses a lower one ([`Why::StaleKeyList`]),
//!   so a revocation cannot be rolled back by serving an old list.
//! - A statement binds (namespace, toolchain, tier, segment digest, length, record count,
//!   issued, builder) and is signed by the builder. The segment digest covers every byte of
//!   the segment, so it binds every key in the segment to its value.
//!
//! A signed message is `domain || 0x00 || body`, with a domain per object kind, so a signature
//! over one kind of object is never valid for another.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{collections::BTreeMap, fmt};

use blaze_cas::plain::{Namespace, Verifier};
use blaze_ids::ContentId;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};

use crate::{
    name::Toolchain,
    wire::{hex, id_at, key_at, put_u32, put_u64, sig_at, u32_at, u64_at},
};

/// An Ed25519 public key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublicKey(pub [u8; 32]);

impl PublicKey {
    pub fn from_hex(text: &str) -> Option<PublicKey> {
        blaze_format::hex::decode::<32>(text.trim()).map(PublicKey)
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&blaze_format::hex::encode(&self.0))
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({self})")
    }
}

/// A signing key: an Ed25519 seed held only in memory. Its Debug shows the public key alone.
pub struct Signer {
    pair: Ed25519KeyPair,
    public: PublicKey,
}

impl fmt::Debug for Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signer {{ public: {}, seed: <redacted> }}", self.public)
    }
}

impl Signer {
    /// The signer of a 32-byte seed.
    pub fn from_seed(seed: &[u8; 32]) -> Signer {
        let pair = Ed25519KeyPair::from_seed_unchecked(seed).expect("every 32-byte seed is an Ed25519 key");
        let public = PublicKey(pair.public_key().as_ref().try_into().expect("32 bytes"));
        Signer { pair, public }
    }

    /// The signer of a seed in hex (64 digits). The error never holds the text.
    pub fn from_hex(what: &str, text: &str) -> Result<Signer, String> {
        let seed = blaze_format::hex::decode::<32>(text.trim()).ok_or_else(|| format!("{what} is not 64 hex digits (an Ed25519 seed); its value is not shown"))?;
        Ok(Signer::from_seed(&seed))
    }

    pub fn public(&self) -> PublicKey {
        self.public
    }

    pub(crate) fn sign(&self, domain: &str, body: &[u8]) -> [u8; 64] {
        let mut message = Vec::with_capacity(domain.len() + 1 + body.len());
        message.extend_from_slice(domain.as_bytes());
        message.push(0);
        message.extend_from_slice(body);
        self.pair.sign(&message).as_ref().try_into().expect("64 bytes")
    }
}

pub(crate) fn verify_sig(key: &PublicKey, domain: &str, body: &[u8], signature: &[u8; 64]) -> bool {
    note(Check::Signature, signature[..32].try_into().expect("32 bytes"));
    if crate::planted(crate::Plant::SkipSignature) {
        return true;
    }
    let mut message = Vec::with_capacity(domain.len() + 1 + body.len());
    message.extend_from_slice(domain.as_bytes());
    message.push(0);
    message.extend_from_slice(body);
    UnparsedPublicKey::new(&ED25519, &key.0).verify(&message, signature).is_ok()
}

/// What a check was of.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Check {
    Signature,
    Digest,
}

/// The checks made on this thread since [`reset_checks`] (decision 18: verify once, by
/// construction). `twice` counts a check of one object (a signature, by its first 32 bytes; a
/// digest, by its value) made again: the simulation's oracle requires zero.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Checks {
    pub signatures: u64,
    pub digests: u64,
    pub twice: u64,
}

thread_local! {
    static CHECKS: std::cell::Cell<Checks> = const { std::cell::Cell::new(Checks { signatures: 0, digests: 0, twice: 0 }) };
    static CHECKED: std::cell::RefCell<std::collections::BTreeSet<(Check, [u8; 32])>> = const { std::cell::RefCell::new(std::collections::BTreeSet::new()) };
}

/// Counts one check of `what`. The counters are observation only: nothing decides on them.
pub(crate) fn note(check: Check, what: [u8; 32]) {
    let again = CHECKED.with_borrow_mut(|it| !it.insert((check, what)));
    let mut checks = CHECKS.get();
    match check {
        Check::Signature => checks.signatures += 1,
        Check::Digest => checks.digests += 1,
    }
    checks.twice += u64::from(again);
    CHECKS.set(checks);
}

/// The checks made on this thread since the last reset: one client's, as a test or a command
/// runs one client a thread.
pub fn checks() -> Checks {
    CHECKS.get()
}

/// Starts the count again: a new client.
pub fn reset_checks() {
    CHECKS.set(Checks::default());
    CHECKED.with_borrow_mut(std::collections::BTreeSet::clear);
}

/// Why an object was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Why {
    /// Its signature does not verify under the key it names.
    Signature,
    /// Its signer is not in the namespace's key list (or, for a key list, not an anchor).
    UnknownSigner,
    /// Its signer is revoked.
    Revoked,
    /// Its signer was not valid when it was issued.
    NotValid { from: u64, until: u64, at: u64 },
    /// Its signer lacks the role this object needs.
    Role { wanted: &'static str },
    /// It is of another namespace.
    Namespace { found: u64, wanted: u64 },
    /// It is of another toolchain.
    Toolchain,
    /// Its bytes are not the ones its digest, or the digest naming it, say.
    Digest,
    /// It is larger than anything of its kind may be.
    TooLarge { len: u64, most: u64 },
    /// Its length is not the one named for it.
    Length { len: u64, wanted: u64 },
    /// It is not well formed: what is wrong.
    Malformed(&'static str),
    /// An index entry places a record outside its segment.
    OutsideSegment,
    /// A key list older than one already seen.
    StaleKeyList { serial: u64, seen: u64 },
    /// Two builders made different values under one key.
    NotDeterministic { ours: ContentId, theirs: ContentId },
    /// A segment of an organisation's namespace was to go to another namespace.
    Private { segment: u64, wanted: u64 },
    /// The fast tier for a public namespace: what is shared is the top tier only.
    Tier,
    /// A name part is refused.
    Name(String),
}

/// A refusal: the object, the key (a signer or a record key) it is about, and why.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Refusal {
    pub object: String,
    pub key: Option<String>,
    pub why: Why,
}

impl Refusal {
    pub(crate) fn new(object: &str, key: Option<String>, why: Why) -> Refusal {
        Refusal { object: object.to_owned(), key, why }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} refused", self.object)?;
        if let Some(key) = &self.key {
            write!(f, " (key {key})")?;
        }
        match &self.why {
            Why::Signature => write!(f, ": its signature does not verify"),
            Why::UnknownSigner => write!(f, ": its signer is not trusted for this namespace"),
            Why::Revoked => write!(f, ": its signer is revoked"),
            Why::NotValid { from, until, at } => write!(f, ": its signer is valid {from}..{until} (unix s), not at {at}"),
            Why::Role { wanted } => write!(f, ": its signer may not sign a {wanted}"),
            Why::Namespace { found, wanted } => write!(f, ": it is of namespace {found:016x}, not {wanted:016x}"),
            Why::Toolchain => write!(f, ": it is of another toolchain"),
            Why::Digest => write!(f, ": its bytes are not the ones its digest names (forged, torn or cut short)"),
            Why::TooLarge { len, most } => write!(f, ": it is {len} bytes, at most {most} may be read"),
            Why::Length { len, wanted } => write!(f, ": it is {len} bytes, {wanted} named"),
            Why::Malformed(what) => write!(f, ": {what}"),
            Why::OutsideSegment => write!(f, ": an index entry places a record outside its segment"),
            Why::StaleKeyList { serial, seen } => write!(f, ": key list serial {serial} is older than serial {seen} already seen"),
            Why::NotDeterministic { ours, theirs } => write!(f, ": two builders made different values under one key (digest {} here, {} in the bucket); rule deterministic-output", hex(ours), hex(theirs)),
            Why::Private { segment, wanted } => write!(f, ": a segment of namespace {segment:016x} may not go to namespace {wanted:016x}"),
            Why::Tier => write!(f, ": only the top tier is shared in a public namespace"),
            Why::Name(why) => write!(f, ": {why}"),
        }
    }
}

/// What a builder key may sign.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Roles(pub u32);

impl Roles {
    pub const BUILD: Roles = Roles(1);
    pub const ROOT: Roles = Roles(2);

    pub fn has(self, role: Roles) -> bool {
        self.0 & role.0 == role.0
    }
}

/// One key of a key list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyEntry {
    pub key: PublicKey,
    pub roles: Roles,
    pub revoked: bool,
    /// Valid from, until (unix seconds, inclusive).
    pub from: u64,
    pub until: u64,
}

/// A namespace's builder keys, signed by an anchor.
///
/// Layout: a 48-byte header (`BLRK`, format u32, namespace u64, serial u64, issued u64,
/// count u32, zero u32, zero u64), then `count` entries of 56 bytes (key 32, roles u32,
/// revoked u32, from u64, until u64), then the anchor's key (32) and signature (64).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KeyList {
    pub namespace: Namespace,
    pub serial: u64,
    pub issued: u64,
    pub keys: Vec<KeyEntry>,
}

const KEYS_DOMAIN: &str = "blaze-remote key list 1";
const KEYS_HEADER: usize = 48;
const KEY_ENTRY: usize = 56;
/// The most keys a list may name.
pub const KEYS_MOST: usize = 1024;
/// The most bytes a key list may be.
pub const KEY_LIST_MOST: u64 = (KEYS_HEADER + KEYS_MOST * KEY_ENTRY + 96) as u64;

impl KeyList {
    pub fn encode(&self, anchor: &Signer) -> Vec<u8> {
        let mut out = Vec::with_capacity(KEYS_HEADER + self.keys.len() * KEY_ENTRY + 96);
        out.extend_from_slice(b"BLRK");
        put_u32(&mut out, 1);
        put_u64(&mut out, self.namespace.0);
        put_u64(&mut out, self.serial);
        put_u64(&mut out, self.issued);
        put_u32(&mut out, self.keys.len() as u32);
        put_u32(&mut out, 0);
        put_u64(&mut out, 0);
        for key in &self.keys {
            out.extend_from_slice(&key.key.0);
            put_u32(&mut out, key.roles.0);
            put_u32(&mut out, u32::from(key.revoked));
            put_u64(&mut out, key.from);
            put_u64(&mut out, key.until);
        }
        out.extend_from_slice(&anchor.public().0);
        let signature = anchor.sign(KEYS_DOMAIN, &out);
        out.extend_from_slice(&signature);
        out
    }

    /// The list in `bytes`, checked: its form, its namespace, and its signature by one of
    /// `anchors`.
    pub fn decode(object: &str, bytes: &[u8], namespace: Namespace, anchors: &[PublicKey]) -> Result<KeyList, Refusal> {
        let refuse = |why| Refusal::new(object, None, why);
        if bytes.len() as u64 > KEY_LIST_MOST {
            return Err(refuse(Why::TooLarge { len: bytes.len() as u64, most: KEY_LIST_MOST }));
        }
        if bytes.len() < KEYS_HEADER + 96 || bytes[0..4] != *b"BLRK" || u32_at(bytes, 4) != 1 {
            return Err(refuse(Why::Malformed("not a key list of format 1")));
        }
        let count = u32_at(bytes, 32) as usize;
        if count > KEYS_MOST || bytes.len() != KEYS_HEADER + count * KEY_ENTRY + 96 {
            return Err(refuse(Why::Length { len: bytes.len() as u64, wanted: (KEYS_HEADER + count.min(KEYS_MOST) * KEY_ENTRY + 96) as u64 }));
        }
        let body_len = bytes.len() - 64;
        let anchor = PublicKey(key_at(bytes, body_len - 32));
        if !anchors.contains(&anchor) {
            return Err(Refusal::new(object, Some(anchor.to_string()), Why::UnknownSigner));
        }
        if !verify_sig(&anchor, KEYS_DOMAIN, &bytes[..body_len], &sig_at(bytes, body_len)) {
            return Err(Refusal::new(object, Some(anchor.to_string()), Why::Signature));
        }
        let found = u64_at(bytes, 8);
        if found != namespace.0 && !crate::planted(crate::Plant::SkipNamespace) {
            return Err(refuse(Why::Namespace { found, wanted: namespace.0 }));
        }
        let mut keys = Vec::with_capacity(count);
        for i in 0..count {
            let at = KEYS_HEADER + i * KEY_ENTRY;
            let revoked = match u32_at(bytes, at + 36) {
                0 => false,
                1 => true,
                _ => return Err(refuse(Why::Malformed("a key's revoked flag is neither 0 nor 1"))),
            };
            keys.push(KeyEntry { key: PublicKey(key_at(bytes, at)), roles: Roles(u32_at(bytes, at + 32)), revoked, from: u64_at(bytes, at + 40), until: u64_at(bytes, at + 48) });
        }
        Ok(KeyList { namespace, serial: u64_at(bytes, 16), issued: u64_at(bytes, 24), keys })
    }

    /// Whether `key` may sign an object needing `role` issued at `at`.
    pub fn allows(&self, object: &str, key: &PublicKey, role: Roles, at: u64) -> Result<(), Refusal> {
        let refuse = |why| Err(Refusal::new(object, Some(key.to_string()), why));
        let Some(entry) = self.keys.iter().find(|it| it.key == *key) else { return refuse(Why::UnknownSigner) };
        if entry.revoked {
            return refuse(Why::Revoked);
        }
        if !entry.roles.has(role) {
            return refuse(Why::Role { wanted: if role == Roles::ROOT { "root" } else { "statement or bundle" } });
        }
        if at < entry.from || at > entry.until {
            return refuse(Why::NotValid { from: entry.from, until: entry.until, at });
        }
        Ok(())
    }
}

/// The tier of the code a segment holds. What is shared publicly is the top tier only (the
/// owner, 2026-11-13: everything pulled is maximally optimized; the fast tier is never pushed
/// for dependencies).
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// No code, or code at the top tier.
    Top,
    /// Code by the fast backend: a workspace's own, never public.
    Fast,
}

/// A builder's signed statement about one segment, 200 bytes.
///
/// Layout: `BLRT`, format u32, namespace u64, segment digest 32, length u64, toolchain digest
/// 32, records u32, tier u8 (0 top, 1 fast), zero 3, issued u64, builder key 32 (136 bytes
/// signed), signature 64.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Statement {
    pub namespace: Namespace,
    pub segment: ContentId,
    pub len: u64,
    pub toolchain: ContentId,
    pub records: u32,
    pub tier: Tier,
    pub issued: u64,
    pub builder: PublicKey,
}

const STATEMENT_DOMAIN: &str = "blaze-remote statement 1";
/// A statement's length.
pub const STATEMENT_LEN: usize = 200;
const STATEMENT_BODY: usize = 136;

impl Statement {
    pub fn encode(&self, signer: &Signer) -> Vec<u8> {
        let mut out = Vec::with_capacity(STATEMENT_LEN);
        out.extend_from_slice(b"BLRT");
        put_u32(&mut out, 1);
        put_u64(&mut out, self.namespace.0);
        out.extend_from_slice(&self.segment.0);
        put_u64(&mut out, self.len);
        out.extend_from_slice(&self.toolchain.0);
        put_u32(&mut out, self.records);
        out.extend_from_slice(&[if self.tier == Tier::Fast { 1 } else { 0 }, 0, 0, 0]);
        put_u64(&mut out, self.issued);
        out.extend_from_slice(&signer.public().0);
        let signature = signer.sign(STATEMENT_DOMAIN, &out);
        out.extend_from_slice(&signature);
        out
    }

    /// The statement in `bytes`, its signature checked under the key it names (whether that
    /// key is trusted is [`Trusted::statement`]'s).
    pub fn decode(object: &str, bytes: &[u8]) -> Result<Statement, Refusal> {
        let refuse = |why| Refusal::new(object, None, why);
        if bytes.len() != STATEMENT_LEN {
            return Err(refuse(Why::Length { len: bytes.len() as u64, wanted: STATEMENT_LEN as u64 }));
        }
        if bytes[0..4] != *b"BLRT" || u32_at(bytes, 4) != 1 || bytes[93..96] != [0, 0, 0] {
            return Err(refuse(Why::Malformed("not a statement of format 1")));
        }
        let builder = PublicKey(key_at(bytes, 104));
        if !verify_sig(&builder, STATEMENT_DOMAIN, &bytes[..STATEMENT_BODY], &sig_at(bytes, STATEMENT_BODY)) {
            return Err(Refusal::new(object, Some(builder.to_string()), Why::Signature));
        }
        let tier = match bytes[92] {
            0 => Tier::Top,
            1 => Tier::Fast,
            _ => return Err(refuse(Why::Malformed("a statement's tier is neither 0 nor 1"))),
        };
        Ok(Statement {
            namespace: Namespace(u64_at(bytes, 8)),
            segment: id_at(bytes, 16),
            len: u64_at(bytes, 48),
            toolchain: id_at(bytes, 56),
            records: u32_at(bytes, 88),
            tier,
            issued: u64_at(bytes, 96),
            builder,
        })
    }
}

/// What this machine trusts: per namespace, the anchors from settings, the highest key list
/// serial seen, and the key list verified for this session; and the one toolchain it reads.
#[derive(Clone, Debug)]
pub struct Trusted {
    pub toolchain: Toolchain,
    anchors: BTreeMap<u64, Vec<PublicKey>>,
    seen: BTreeMap<u64, u64>,
    lists: BTreeMap<u64, KeyList>,
}

impl Trusted {
    pub fn new(toolchain: Toolchain) -> Trusted {
        Trusted { toolchain, anchors: BTreeMap::new(), seen: BTreeMap::new(), lists: BTreeMap::new() }
    }

    pub fn anchor(&mut self, namespace: Namespace, keys: Vec<PublicKey>) {
        self.anchors.insert(namespace.0, keys);
    }

    pub fn anchors(&self, namespace: Namespace) -> &[PublicKey] {
        self.anchors.get(&namespace.0).map_or(&[], |it| it.as_slice())
    }

    /// Records a serial already seen (persisted by the caller between processes).
    pub fn saw(&mut self, namespace: Namespace, serial: u64) {
        let seen = self.seen.entry(namespace.0).or_insert(0);
        *seen = (*seen).max(serial);
    }

    pub fn seen(&self, namespace: Namespace) -> u64 {
        self.seen.get(&namespace.0).copied().unwrap_or(0)
    }

    pub fn list(&self, namespace: Namespace) -> Option<&KeyList> {
        self.lists.get(&namespace.0)
    }

    /// Decodes and takes the key list in `bytes` for `namespace`, refusing one older than any
    /// seen.
    pub fn take_list(&mut self, object: &str, namespace: Namespace, bytes: &[u8]) -> Result<&KeyList, Refusal> {
        let list = KeyList::decode(object, bytes, namespace, self.anchors(namespace))?;
        let seen = self.seen(namespace);
        if list.serial < seen {
            return Err(Refusal::new(object, None, Why::StaleKeyList { serial: list.serial, seen }));
        }
        self.saw(namespace, list.serial);
        self.lists.insert(namespace.0, list);
        Ok(self.lists.get(&namespace.0).expect("just put"))
    }

    /// Checks a statement for the segment `digest` of `len` bytes in `namespace`: its
    /// signature, its namespace, toolchain, digest and length, and its builder in the key list.
    pub fn statement(&self, object: &str, namespace: Namespace, digest: &ContentId, len: u64, bytes: &[u8]) -> Result<Statement, Refusal> {
        let statement = Statement::decode(object, bytes)?;
        let refuse = |why| Refusal::new(object, Some(statement.builder.to_string()), why);
        if statement.namespace != namespace && !crate::planted(crate::Plant::SkipNamespace) {
            return Err(refuse(Why::Namespace { found: statement.namespace.0, wanted: namespace.0 }));
        }
        if statement.toolchain != self.toolchain.digest() {
            return Err(refuse(Why::Toolchain));
        }
        if statement.segment != *digest {
            return Err(refuse(Why::Digest));
        }
        if statement.len != len {
            return Err(refuse(Why::Length { len, wanted: statement.len }));
        }
        let Some(list) = self.list(namespace) else { return Err(refuse(Why::UnknownSigner)) };
        list.allows(object, &statement.builder, Roles::BUILD, statement.issued)?;
        Ok(statement)
    }
}

impl Verifier for Trusted {
    fn verify(&self, namespace: Namespace, digest: &ContentId, len: u64, statement: &[u8]) -> Result<(), String> {
        let object = crate::name::statement(namespace, &self.toolchain, digest).map_err(|it| it.to_string())?;
        self.statement(&object, namespace, digest, len, statement).map(|_| ()).map_err(|it| it.to_string())
    }
}
