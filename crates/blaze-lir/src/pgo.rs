//! PGO site and body weight tables for [`crate::inline::inline_guided`] and [`crate::lto`].
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, explicit allocator only.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Vec as OwnVec;

use crate::{
    Block, Op,
    body::BodyId,
    func::BlockKind,
    inline::SiteWeights,
    lto::ModuleBody,
    summary::FnSummary,
};

/// Magic bytes for the persistent cross-run PGO binary record (`BPG1`).
pub const PGO_MAGIC: [u8; 4] = *b"BPG1";
/// Binary schema version of `BPG1`.
pub const PGO_VERSION: u32 = 1;
/// Hash domain for sealing and verifying `BPG1` PGO binary records.
const PGO_RECORD_DOMAIN: &str = "blaze v2 lir pgo record 1";

/// Coarse PGO tier bucket joined into content-addressed JIT/export code keys so hot functions
/// compiled in run N are reused straight from disk in run N+1 without warm-up.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum PgoTier {
    Cold = 0,
    Warm = 1,
    Hot = 2,
}

/// Error returned when decoding a persisted `BPG1` PGO record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PgoRecordError {
    TooShort,
    BadMagic,
    BadVersion(u32),
    LengthMismatch,
    BadDigest,
}

impl fmt::Display for PgoRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PgoRecordError::TooShort => write!(f, "PGO record truncated"),
            PgoRecordError::BadMagic => write!(f, "PGO record missing BPG1 magic"),
            PgoRecordError::BadVersion(v) => write!(f, "unsupported PGO record version {v}"),
            PgoRecordError::LengthMismatch => write!(f, "PGO record length does not match header counts"),
            PgoRecordError::BadDigest => write!(f, "PGO record BLAKE3 integrity check failed"),
        }
    }
}

impl core::error::Error for PgoRecordError {}

/// One call-site execution weight observation: instruction `site_inst` inside `caller`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SiteEntry {
    pub caller: BodyId,
    pub site_inst: u32,
    pub weight: u64,
}

/// One body-level execution weight observation (for leaf bodies and JIT tier-up ranking).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct BodyWeight {
    pub body: BodyId,
    pub weight: u64,
}

/// Sorted, deterministic PGO table driving both [`SiteWeights`] (per-call-site inlining) and
/// body-level hotness (bottom-up LTO and JIT tier-up planning).
pub struct PgoSiteTable<A: Allocator> {
    sites: OwnVec<SiteEntry, A>,
    bodies: OwnVec<BodyWeight, A>,
    default_weight: u64,
    total_site_weight: u64,
    total_body_weight: u64,
}

impl<A: Allocator + Copy> PgoSiteTable<A> {
    /// Builds a sorted, deduplicated PGO table from raw call-site and body weight entries.
    ///
    /// Duplicate `(caller, site_inst)` or `body` entries are summed saturatingly.
    /// `default_weight` is returned by [`SiteWeights::weight`] for call sites not present in
    /// `raw_sites` (pass `1` for partial profiles or `0` when an unsampled site is known cold).
    pub fn new_in(
        raw_sites: &[SiteEntry],
        raw_bodies: &[BodyWeight],
        default_weight: u64,
        alloc: A,
    ) -> PgoSiteTable<A> {
        let mut sites = OwnVec::with_capacity_in(raw_sites.len(), alloc);
        sites.extend_from_slice(raw_sites);
        sites.sort_unstable_by(|a, b| a.caller.cmp(&b.caller).then_with(|| a.site_inst.cmp(&b.site_inst)));
        // Coalesce duplicate `(caller, site_inst)` keys in-place.
        let mut write = 0usize;
        for read in 0..sites.len() {
            let entry = sites[read];
            if write > 0
                && sites[write - 1].caller == entry.caller
                && sites[write - 1].site_inst == entry.site_inst
            {
                sites[write - 1].weight = sites[write - 1].weight.saturating_add(entry.weight);
            } else {
                sites[write] = entry;
                write += 1;
            }
        }
        sites.truncate(write);

        let mut total_site_weight = 0u64;
        for e in sites.iter() {
            total_site_weight = total_site_weight.saturating_add(e.weight);
        }

        let mut bodies = OwnVec::with_capacity_in(raw_bodies.len(), alloc);
        bodies.extend_from_slice(raw_bodies);
        bodies.sort_unstable_by(|a, b| a.body.cmp(&b.body));
        let mut bwrite = 0usize;
        for read in 0..bodies.len() {
            let entry = bodies[read];
            if bwrite > 0 && bodies[bwrite - 1].body == entry.body {
                bodies[bwrite - 1].weight = bodies[bwrite - 1].weight.saturating_add(entry.weight);
            } else {
                bodies[bwrite] = entry;
                bwrite += 1;
            }
        }
        bodies.truncate(bwrite);

        let mut total_body_weight = 0u64;
        for b in bodies.iter() {
            total_body_weight = total_body_weight.saturating_add(b.weight);
        }

        PgoSiteTable { sites, bodies, default_weight, total_site_weight, total_body_weight }
    }

    /// Returns the recorded body-level weight of `body`, falling back to the sum of its call-site
    /// weights if no explicit body weight was supplied.
    pub fn body_weight(&self, body: BodyId) -> u64 {
        if let Ok(idx) = self.bodies.binary_search_by_key(&body, |b| b.body) {
            return self.bodies[idx].weight;
        }
        // Sum recorded call-site weights belonging to `body`.
        let start = self.sites.partition_point(|e| e.caller < body);
        let mut sum = 0u64;
        let mut i = start;
        while i < self.sites.len() && self.sites[i].caller == body {
            sum = sum.saturating_add(self.sites[i].weight);
            i += 1;
        }
        sum
    }

    #[inline]
    pub fn total_site_weight(&self) -> u64 {
        self.total_site_weight
    }

    #[inline]
    pub fn total_body_weight(&self) -> u64 {
        self.total_body_weight
    }

    #[inline]
    pub fn site_entries(&self) -> &[SiteEntry] {
        &self.sites
    }

    #[inline]
    pub fn body_entries(&self) -> &[BodyWeight] {
        &self.bodies
    }

    /// Coarse PGO tier bucket of `body` (for joining into content-addressed JIT/export code keys).
    pub fn tier_of(&self, body: BodyId) -> PgoTier {
        let w = self.body_weight(body);
        if w == 0 {
            return PgoTier::Cold;
        }
        let denom = self.total_body_weight.max(self.total_site_weight).max(1);
        let ppm = w.saturating_mul(1_000_000) / denom;
        if ppm >= 10_000 || w >= 100 {
            PgoTier::Hot
        } else if ppm >= 1_000 || w >= 10 {
            PgoTier::Warm
        } else {
            PgoTier::Cold
        }
    }

    /// Cold-start static PGO estimator for run #1 (before any runtime profile exists):
    /// assigns higher weights to call sites and bodies with loops, baseline weights to normal
    /// code, and `0` to `#[cold]` bodies and unwind/landing blocks.
    pub fn estimate_static(module: &[ModuleBody<A>], alloc: A) -> PgoSiteTable<A> {
        let mut raw_sites = OwnVec::new_in(alloc);
        let mut raw_bodies = OwnVec::with_capacity_in(module.len(), alloc);
        for mb in module {
            let view = mb.func.view();
            let summary = FnSummary::compute(view, &alloc);
            let is_cold = summary.is_cold;
            let body_w = if is_cold {
                0
            } else if summary.has_loops {
                100
            } else {
                10
            };
            raw_bodies.push(BodyWeight { body: mb.id, weight: body_w });

            let blocks = view.blocks();
            for b in 0..blocks {
                let block = Block(b as u32);
                let kind = view.block_kind(block);
                let site_w = if is_cold || kind != BlockKind::Normal {
                    0
                } else if summary.has_loops {
                    100
                } else {
                    10
                };
                for at in view.block(block) {
                    if view.op(at) == Op::Call {
                        raw_sites.push(SiteEntry {
                            caller: mb.id,
                            site_inst: at as u32,
                            weight: site_w,
                        });
                    }
                }
            }
        }
        PgoSiteTable::new_in(&raw_sites, &raw_bodies, 1, alloc)
    }

    /// Cross-run exponential moving decay merge: `new_w = old_w * decay_num / decay_den + fresh_w`.
    /// Entries whose combined weight decays to `0` are dropped, keeping the persisted table compact.
    pub fn merge_decayed(
        &self,
        fresh: &PgoSiteTable<A>,
        decay_num: u64,
        decay_den: u64,
        alloc: A,
    ) -> PgoSiteTable<A> {
        let den = decay_den.max(1);
        let mut merged_sites = OwnVec::with_capacity_in(self.sites.len() + fresh.sites.len(), alloc);
        for s in self.sites.iter() {
            let decayed = s.weight.saturating_mul(decay_num) / den;
            if decayed > 0 {
                merged_sites.push(SiteEntry { caller: s.caller, site_inst: s.site_inst, weight: decayed });
            }
        }
        merged_sites.extend_from_slice(&fresh.sites);

        let mut merged_bodies = OwnVec::with_capacity_in(self.bodies.len() + fresh.bodies.len(), alloc);
        for b in self.bodies.iter() {
            let decayed = b.weight.saturating_mul(decay_num) / den;
            if decayed > 0 {
                merged_bodies.push(BodyWeight { body: b.body, weight: decayed });
            }
        }
        merged_bodies.extend_from_slice(&fresh.bodies);

        PgoSiteTable::new_in(&merged_sites, &merged_bodies, fresh.default_weight, alloc)
    }

    /// Encodes this table into a deterministic, reboot-persistent `BPG1` binary record sealed with
    /// a 32-byte BLAKE3 integrity digest (shared across runs, worktrees, and `blaze export`).
    pub fn encode(&self, alloc: A) -> OwnVec<u8, A> {
        let header_bytes = 24usize; // magic(4) + version(4) + default_weight(8) + nsites(4) + nbodies(4)
        let site_bytes = self.sites.len() * 44; // BodyId(32) + site_inst(4) + weight(8)
        let body_bytes = self.bodies.len() * 40; // BodyId(32) + weight(8)
        let total = header_bytes + site_bytes + body_bytes + 32;
        let mut out = OwnVec::with_capacity_in(total, alloc);
        out.extend_from_slice(&PGO_MAGIC);
        out.extend_from_slice(&PGO_VERSION.to_le_bytes());
        out.extend_from_slice(&self.default_weight.to_le_bytes());
        out.extend_from_slice(&(self.sites.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.bodies.len() as u32).to_le_bytes());
        for s in self.sites.iter() {
            out.extend_from_slice(&s.caller.0);
            out.extend_from_slice(&s.site_inst.to_le_bytes());
            out.extend_from_slice(&s.weight.to_le_bytes());
        }
        for b in self.bodies.iter() {
            out.extend_from_slice(&b.body.0);
            out.extend_from_slice(&b.weight.to_le_bytes());
        }
        let mut hasher = blaze_ids::Hasher::new(PGO_RECORD_DOMAIN);
        hasher.update(&out);
        let digest = hasher.finish().0;
        out.extend_from_slice(&digest);
        out
    }

    /// Decodes and verifies a persisted `BPG1` PGO record, checking magic, version, length, and
    /// BLAKE3 integrity digest before trusting any bytes.
    pub fn decode(bytes: &[u8], alloc: A) -> Result<PgoSiteTable<A>, PgoRecordError> {
        const HEADER: usize = 24;
        const TRAILER: usize = 32;
        if bytes.len() < HEADER + TRAILER {
            return Err(PgoRecordError::TooShort);
        }
        if bytes[0..4] != PGO_MAGIC {
            return Err(PgoRecordError::BadMagic);
        }
        let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if version != PGO_VERSION {
            return Err(PgoRecordError::BadVersion(version));
        }
        let default_weight = u64::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]);
        let nsites = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) as usize;
        let nbodies = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]) as usize;
        let site_bytes = nsites.checked_mul(44).ok_or(PgoRecordError::LengthMismatch)?;
        let body_bytes = nbodies.checked_mul(40).ok_or(PgoRecordError::LengthMismatch)?;
        let expected = HEADER
            .checked_add(site_bytes)
            .and_then(|n| n.checked_add(body_bytes))
            .and_then(|n| n.checked_add(TRAILER))
            .ok_or(PgoRecordError::LengthMismatch)?;
        if bytes.len() != expected {
            return Err(PgoRecordError::LengthMismatch);
        }
        let payload = &bytes[..bytes.len() - TRAILER];
        let mut hasher = blaze_ids::Hasher::new(PGO_RECORD_DOMAIN);
        hasher.update(payload);
        if hasher.finish().0 != bytes[bytes.len() - TRAILER..] {
            return Err(PgoRecordError::BadDigest);
        }

        let mut cur = HEADER;
        let mut raw_sites = OwnVec::with_capacity_in(nsites, alloc);
        for _ in 0..nsites {
            let mut caller = [0u8; 32];
            caller.copy_from_slice(&bytes[cur..cur + 32]);
            cur += 32;
            let site_inst = u32::from_le_bytes([bytes[cur], bytes[cur + 1], bytes[cur + 2], bytes[cur + 3]]);
            cur += 4;
            let weight = u64::from_le_bytes([
                bytes[cur], bytes[cur + 1], bytes[cur + 2], bytes[cur + 3],
                bytes[cur + 4], bytes[cur + 5], bytes[cur + 6], bytes[cur + 7],
            ]);
            cur += 8;
            raw_sites.push(SiteEntry { caller: BodyId(caller), site_inst, weight });
        }
        let mut raw_bodies = OwnVec::with_capacity_in(nbodies, alloc);
        for _ in 0..nbodies {
            let mut body = [0u8; 32];
            body.copy_from_slice(&bytes[cur..cur + 32]);
            cur += 32;
            let weight = u64::from_le_bytes([
                bytes[cur], bytes[cur + 1], bytes[cur + 2], bytes[cur + 3],
                bytes[cur + 4], bytes[cur + 5], bytes[cur + 6], bytes[cur + 7],
            ]);
            cur += 8;
            raw_bodies.push(BodyWeight { body: BodyId(body), weight });
        }
        Ok(PgoSiteTable::new_in(&raw_sites, &raw_bodies, default_weight, alloc))
    }
}

impl<A: Allocator> SiteWeights for PgoSiteTable<A> {
    #[inline]
    fn weight(&self, caller: BodyId, site_inst: u32) -> u64 {
        match self.sites.binary_search_by(|e| e.caller.cmp(&caller).then_with(|| e.site_inst.cmp(&site_inst))) {
            Ok(idx) => self.sites[idx].weight,
            Err(_) => self.default_weight,
        }
    }
}
