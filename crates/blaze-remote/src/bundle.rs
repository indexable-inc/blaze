//! A bundle: a signed manifest naming whole segments to fetch before a first build, for one
//! (namespace, toolchain, target, cfg set). Its records are the top tier only.
//!
//! Layout: `BLRB`, format u32, namespace u64, issued u64, key list serial u64, segments u32,
//! zero u32, then the toolchain, target and cfg names, each zero-padded to 64 bytes (232 bytes
//! so far). Then per segment 56 bytes: digest 32, length u64, index offset u64, records u32,
//! zero u32. Last come the signer's key (32) and signature (64).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_cas::plain::Namespace;
use blaze_ids::ContentId;

use crate::{
    name::{self, Toolchain},
    trust::{PublicKey, Refusal, Signer, Why, verify_sig},
    wire::{id_at, key_at, put_u32, put_u64, sig_at, u32_at, u64_at},
};

const DOMAIN: &str = "blaze-remote bundle 1";
const HEADER: usize = 232;
const ROW: usize = 56;
/// The most segments a bundle names.
pub const SEGMENTS_MOST: usize = 4096;
/// The most bytes a manifest may be.
pub const MANIFEST_MOST: u64 = (HEADER + SEGMENTS_MOST * ROW + 96) as u64;

/// What a bundle is for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BundleId {
    pub toolchain: Toolchain,
    pub target: String,
    pub cfg: String,
}

impl BundleId {
    pub fn new(toolchain: &str, target: &str, cfg: &str) -> Result<BundleId, name::BadName> {
        name::part("target", target)?;
        name::part("cfg", cfg)?;
        Ok(BundleId { toolchain: Toolchain::new(toolchain)?, target: target.to_owned(), cfg: cfg.to_owned() })
    }

    pub fn object(&self, namespace: Namespace) -> Result<String, name::BadName> {
        name::bundle(namespace, &self.toolchain, &self.target, &self.cfg)
    }
}

/// One segment of a bundle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SegmentRef {
    pub digest: ContentId,
    pub len: u64,
    pub index_at: u64,
    pub count: u32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Bundle {
    pub namespace: Namespace,
    pub id: BundleId,
    pub issued: u64,
    pub keys_serial: u64,
    pub segments: Vec<SegmentRef>,
}

fn padded(out: &mut Vec<u8>, text: &str) {
    let mut field = [0u8; 64];
    field[..text.len()].copy_from_slice(text.as_bytes());
    out.extend_from_slice(&field);
}

fn unpadded(field: &[u8]) -> Option<&str> {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    field[end..].iter().all(|b| *b == 0).then(|| std::str::from_utf8(&field[..end]).ok()).flatten()
}

impl Bundle {
    pub fn encode(&self, signer: &Signer) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + self.segments.len() * ROW + 96);
        out.extend_from_slice(b"BLRB");
        put_u32(&mut out, 1);
        put_u64(&mut out, self.namespace.0);
        put_u64(&mut out, self.issued);
        put_u64(&mut out, self.keys_serial);
        put_u32(&mut out, self.segments.len() as u32);
        put_u32(&mut out, 0);
        padded(&mut out, self.id.toolchain.as_str());
        padded(&mut out, &self.id.target);
        padded(&mut out, &self.id.cfg);
        for segment in &self.segments {
            out.extend_from_slice(&segment.digest.0);
            put_u64(&mut out, segment.len);
            put_u64(&mut out, segment.index_at);
            put_u32(&mut out, segment.count);
            put_u32(&mut out, 0);
        }
        out.extend_from_slice(&signer.public().0);
        let signature = signer.sign(DOMAIN, &out);
        out.extend_from_slice(&signature);
        out
    }

    /// The manifest in `bytes`, its signature checked under the key it names, and its
    /// coordinates checked to be the ones asked for (an object binds where it lies).
    pub fn decode(object: &str, bytes: &[u8], namespace: Namespace, id: &BundleId) -> Result<(Bundle, PublicKey), Refusal> {
        let refuse = |why| Refusal::new(object, None, why);
        if bytes.len() as u64 > MANIFEST_MOST {
            return Err(refuse(Why::TooLarge { len: bytes.len() as u64, most: MANIFEST_MOST }));
        }
        if bytes.len() < HEADER + 96 || bytes[0..4] != *b"BLRB" || u32_at(bytes, 4) != 1 {
            return Err(refuse(Why::Malformed("not a bundle manifest of format 1")));
        }
        let count = u32_at(bytes, 32) as usize;
        if count > SEGMENTS_MOST || bytes.len() != HEADER + count * ROW + 96 {
            return Err(refuse(Why::Length { len: bytes.len() as u64, wanted: (HEADER + count.min(SEGMENTS_MOST) * ROW + 96) as u64 }));
        }
        let body = bytes.len() - 64;
        let signer = PublicKey(key_at(bytes, body - 32));
        if !verify_sig(&signer, DOMAIN, &bytes[..body], &sig_at(bytes, body)) {
            return Err(Refusal::new(object, Some(signer.to_string()), Why::Signature));
        }
        if u64_at(bytes, 8) != namespace.0 && !crate::planted(crate::Plant::SkipNamespace) {
            return Err(refuse(Why::Namespace { found: u64_at(bytes, 8), wanted: namespace.0 }));
        }
        let names = (unpadded(&bytes[40..104]), unpadded(&bytes[104..168]), unpadded(&bytes[168..232]));
        if names != (Some(id.toolchain.as_str()), Some(id.target.as_str()), Some(id.cfg.as_str())) {
            return Err(refuse(Why::Malformed("a bundle manifest for other coordinates than where it lies")));
        }
        let mut segments = Vec::with_capacity(count);
        for i in 0..count {
            let at = HEADER + i * ROW;
            let segment = SegmentRef { digest: id_at(bytes, at), len: u64_at(bytes, at + 32), index_at: u64_at(bytes, at + 40), count: u32_at(bytes, at + 48) };
            if segment.len > blaze_cas::plain::SEGMENT_MOST {
                return Err(refuse(Why::TooLarge { len: segment.len, most: blaze_cas::plain::SEGMENT_MOST }));
            }
            segments.push(segment);
        }
        Ok((Bundle { namespace, id: id.clone(), issued: u64_at(bytes, 16), keys_serial: u64_at(bytes, 24), segments }, signer))
    }
}
