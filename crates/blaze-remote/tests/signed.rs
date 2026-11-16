//! Signed objects as checked-in bytes, verified in the shape the `blaze` binary links (no
//! default features, so no `ring` and no `Signer`): a key list, a statement and a head, each
//! accepted as it was signed and refused, with blaze-ed25519's typed reason, when one byte of
//! its message, its signature or its key is flipped, when its signature is taken under another
//! object's domain, or when its signer is not in the key list. A verification gated by mistake
//! under `sign` would let the reader take what it must refuse and only the signing build would
//! be tested; these run in both shapes.
//!
//! `data/signed.txt` is written by `the_checked_in_bytes_are_what_signing_makes` (under `sign`):
//! Ed25519 signatures are deterministic, so the bytes it makes from fixed seeds must equal the
//! file's, and on a difference it prints the file as it should be.

use blaze_cas::plain::{ENTRY, FOOTER, HEADER, Namespace};
use blaze_ed25519::Refused as Ed;
use blaze_ids::ContentId;
use blaze_keyed::store::{Refused, Statement};
use blaze_remote::name::Toolchain;
use blaze_remote::read::{Ed25519Keys, Head, HeadCheck, Refused as HeadRefused, SignatureWhy};
use blaze_remote::segment::{SegmentId, SegmentRef};
use blaze_remote::trust::{KeyList, Trusted};
use blaze_remote::{PublicKey, Why};

const SIGNED: &str = include_str!("data/signed.txt");
const NS: Namespace = Namespace::PUBLIC;
const TOOLCHAIN: &str = "nightly-2026-11-13";
const HEAD_DOMAIN: &str = "blaze-remote origin head 1";
const KEYS_DOMAIN: &str = "blaze-remote key list 1";

fn bytes(hex: &str) -> Vec<u8> {
    assert!(hex.len() % 2 == 0, "hex of odd length: {hex}");
    (0..hex.len()).step_by(2).map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex")).collect()
}

/// The fixture named `name`.
fn fixture(name: &str) -> Vec<u8> {
    let line = SIGNED.lines().find(|line| line.split(' ').next() == Some(name)).unwrap_or_else(|| panic!("data/signed.txt has no {name}"));
    bytes(line.split(' ').nth(1).unwrap_or_else(|| panic!("{name}: no bytes")))
}

fn key(name: &str) -> PublicKey {
    PublicKey(fixture(name).try_into().unwrap_or_else(|_| panic!("{name}: 32 bytes")))
}

/// The one segment every statement here is about.
fn segment() -> SegmentRef {
    SegmentRef::new(SegmentId(ContentId([2; 32])), (HEADER + 16 + ENTRY + FOOTER) as u64, (HEADER + 16) as u64, 1).expect("a segment's shape")
}

/// What a reader trusts: the anchor, and the key list it signed taken.
fn trusted() -> Trusted {
    let mut trusted = Trusted::new(Toolchain::new(TOOLCHAIN).expect("a toolchain name"));
    trusted.anchor(NS, vec![key("anchor")]);
    trusted.take_list("keys", NS, &fixture("key_list")).expect("the genuine key list");
    trusted
}

fn statement(name: &str) -> Statement {
    Statement::decode(&fixture(name)).expect("a statement's form")
}

/// The statement refused, with what the verifier says.
fn statement_refused(statement: &Statement) -> Refused {
    let refusal = trusted().statement("statement", NS, &segment(), &statement.encode()).expect_err("refused");
    match refusal.why {
        Why::Statement(why) => why,
        other => panic!("a statement refused for {other:?}"),
    }
}

fn head_check(keys: &[PublicKey]) -> Ed25519Keys<'_> {
    Ed25519Keys { namespace: NS, keys }
}

fn head_refused(bytes: &[u8]) -> SignatureWhy {
    let builder = [key("builder")];
    match Head::decode(bytes, NS, &head_check(&builder)) {
        Err(HeadRefused::Signature { why, .. }) => why,
        other => panic!("a head taken or refused otherwise: {other:?}"),
    }
}

fn flipped(mut bytes: Vec<u8>, at: usize) -> Vec<u8> {
    bytes[at] ^= 1;
    bytes
}

#[test]
fn the_genuine_ones_are_accepted() {
    let trusted = trusted();
    assert_eq!(trusted.list(NS).expect("taken").serial, 1);
    let taken = trusted.statement("statement", NS, &segment(), &fixture("statement")).expect("the genuine statement");
    assert_eq!(taken.signer, key("builder").0);
    let builder = [key("builder")];
    let head = Head::decode(&fixture("head"), NS, &head_check(&builder)).expect("the genuine head");
    assert_eq!((head.generation, head.signer), (7, key("builder")));
}

#[test]
fn a_byte_flipped_in_the_message_is_refused() {
    // A key list's serial (byte 16), a statement's record count, a head's generation (byte 16):
    // each covered by the signature and checked by nothing before it.
    let list = KeyList::decode("keys", &flipped(fixture("key_list"), 16), NS, &[key("anchor")]).expect_err("refused");
    assert_eq!(list.why, Why::Signature(Ed::Mismatch));
    let mut changed = statement("statement");
    changed.records ^= 1;
    assert_eq!(statement_refused(&changed), Refused::Signature { signer: key("builder").0 });
    assert_eq!(head_refused(&flipped(fixture("head"), 16)), SignatureWhy::Invalid(Ed::Mismatch));
}

#[test]
fn a_byte_flipped_in_the_signature_is_refused() {
    // The lowest byte of S (byte 32 of the signature): S stays below L, so the equation fails.
    let list = fixture("key_list");
    let at = list.len() - 32;
    let refused = KeyList::decode("keys", &flipped(list, at), NS, &[key("anchor")]).expect_err("refused");
    assert_eq!(refused.why, Why::Signature(Ed::Mismatch));
    let mut changed = statement("statement");
    changed.signature[32] ^= 1;
    assert_eq!(statement_refused(&changed), Refused::Signature { signer: key("builder").0 });
    assert_eq!(head_refused(&flipped(fixture("head"), 96 + 32)), SignatureWhy::Invalid(Ed::Mismatch));
    // And R's first byte: this R then has no x (step 5), the reason named.
    assert_eq!(head_refused(&flipped(fixture("head"), 96)), SignatureWhy::Invalid(Ed::REncoding(blaze_ed25519::Encoding::NotOnCurve)));
}

#[test]
fn a_byte_flipped_in_the_key_is_refused() {
    // The key given and named both flipped, so the key is trusted and only the signature can
    // refuse it.
    let head = fixture("head");
    let signature: [u8; 64] = head[96..160].try_into().expect("64 bytes");
    for at in [0, 1, 31] {
        let mut flipped_key = key("builder");
        flipped_key.0[at] ^= 1;
        let keys = [flipped_key];
        let why = head_check(&keys).check(NS, &flipped_key, HEAD_DOMAIN, &head[..96], &signature).expect_err("refused");
        assert!(matches!(why, SignatureWhy::Invalid(Ed::Mismatch | Ed::KeyEncoding(_))), "key byte {at} flipped: {why:?}");
    }
    // A statement naming another key: not in the list, so refused before its signature.
    let mut changed = statement("statement");
    changed.signer[0] ^= 1;
    assert_eq!(statement_refused(&changed), Refused::Untrusted { signer: changed.signer });
}

#[test]
fn the_right_signature_under_another_domain_is_refused() {
    let head = fixture("head");
    let signature: [u8; 64] = head[96..160].try_into().expect("64 bytes");
    let builder = [key("builder")];
    head_check(&builder).check(NS, &key("builder"), HEAD_DOMAIN, &head[..96], &signature).expect("under its own domain");
    let why = head_check(&builder).check(NS, &key("builder"), KEYS_DOMAIN, &head[..96], &signature).expect_err("refused");
    assert_eq!(why, SignatureWhy::Invalid(Ed::Mismatch));
}

#[test]
fn a_statement_by_a_key_not_in_the_list_is_refused() {
    let stranger = statement("stranger_statement");
    assert_eq!(stranger.signer, key("stranger").0);
    assert_eq!(statement_refused(&stranger), Refused::Untrusted { signer: key("stranger").0 });
}

/// The checked-in bytes are what signing makes from fixed seeds (Ed25519 is deterministic).
#[cfg(feature = "sign")]
#[test]
fn the_checked_in_bytes_are_what_signing_makes() {
    use blaze_keyed::kind::Tier;
    use blaze_remote::Bounded;
    use blaze_remote::Signer;
    use blaze_remote::trust::{KeyEntry, Roles};

    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let anchor = Signer::from_seed(&[1; 32]);
    let builder = Signer::from_seed(&[2; 32]);
    let stranger = Signer::from_seed(&[3; 32]);
    let entry = KeyEntry { key: builder.public(), roles: Roles::BUILD, revoked: false, from: 0, until: u64::MAX };
    let list = KeyList { namespace: NS, serial: 1, issued: 1_000, keys: Bounded::new(vec![entry]).expect("one key") };
    let segment = segment();
    let signed_by = |signer: &Signer| {
        let mut statement = Statement {
            namespace: NS,
            segment: segment.digest(),
            len: segment.segment_len(),
            toolchain: Toolchain::new(TOOLCHAIN).expect("a toolchain name").digest(),
            records: segment.count(),
            tier: Tier::Top,
            issued: 1_000,
            signer: [0; 32],
            signature: [0; 64],
        };
        signer.sign_statement(&mut statement);
        statement.encode()
    };
    let head = Head { namespace: NS, generation: 7, manifest: ContentId([5; 32]), manifest_len: 4096, signer: builder.public() };
    let head = head.encode(|domain, body| builder.sign(domain, body));
    let made = [
        ("anchor", hex(&anchor.public().0)),
        ("builder", hex(&builder.public().0)),
        ("stranger", hex(&stranger.public().0)),
        ("key_list", hex(&list.encode(&anchor))),
        ("statement", hex(&signed_by(&builder))),
        ("stranger_statement", hex(&signed_by(&stranger))),
        ("head", hex(&head)),
    ]
    .iter()
    .map(|(name, hex)| format!("{name} {hex}\n"))
    .collect::<String>();
    assert!(SIGNED == made, "data/signed.txt is not what signing makes; it should be:\n{made}");
}
