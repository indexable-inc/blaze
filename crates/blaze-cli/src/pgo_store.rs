//! Persisted cross-run `BPG1` PGO profile storage in the CAS store (Rule 86, Rule 30).
//!
//! Unifies JIT-on-call execution (`blaze run` and default `blaze build` executables) with
//! standalone AOT release builds (`blaze build --release` / `--standalone`):
//! - Every JIT run merges its executed function symbols into a content-addressed `BPG1` record
//!   under `pgo_ptr_key(program_shape)` using exponential decay (`7/8`), keeping both the pointer
//!   and record rooted under the program's CAS root.
//! - `blaze build --release` / `--standalone` loads the persisted `BPG1` record under that same
//!   checkout-path-free key and hands it to `export_lto::optimize_export_workspace_par`.

use core::fmt;

use blaze_cas::Key as CasKey;
use blaze_cas::plain::{Lent, LentError, Namespace};
use blaze_core::Driver;
use blaze_ids::ContentId;
use blaze_keyed::kind::ReservedTag;
use blaze_lir::body::BodyId;
use blaze_lir::pgo::{BodyWeight, PgoRecordError, PgoSiteTable};
use blaze_own::{Alloc, Vec};

use crate::store::plain_place::StoreAlloc;

const PGO_PTR_DOMAIN: &str = "blaze-cli pgo profile ptr 1";
const PGO_REC_DOMAIN: &str = "blaze-cli pgo profile rec 1";

/// Weight awarded to each function observed executing during one JIT run (strictly above
/// `export_lto::STATIC_STRAIGHT_LINE_WEIGHT` = 10 so executed functions rank above unexecuted
/// cold code while preserving loop-nest dominance).
const JIT_EXECUTED_BODY_WEIGHT: u64 = 100;

/// Cross-run exponential decay (`7/8`, matching `blaze_ladder::run::make_one::export_lto`).
const PGO_DECAY_NUM: u64 = 7;
const PGO_DECAY_DEN: u64 = 8;

/// Checkout-path-free CAS pointer key for a program's persisted `BPG1` PGO profile.
pub(crate) fn pgo_ptr_key(program_shape: ContentId) -> CasKey {
    let mut h = blaze_ids::Hasher::new(PGO_PTR_DOMAIN);
    h.id(&program_shape);
    CasKey(h.finish())
}

/// Why reading or persisting a `BPG1` PGO profile failed (Rule 3: never silently swallow).
#[derive(Debug)]
pub(crate) enum PgoStoreError {
    Store(LentError),
    Decode(PgoRecordError),
}

impl fmt::Display for PgoStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PgoStoreError::Store(why) => write!(f, "persisted PGO profile store error: {why}"),
            PgoStoreError::Decode(why) => write!(f, "persisted BPG1 PGO record refused: {why}"),
        }
    }
}

/// Loads the persisted `BPG1` PGO record bytes (and its record key, for GC rooting) pointed to by
/// `ptr_key`, returning `Ok(None)` when no profile has been recorded yet or its target expired.
pub(crate) fn load_pgo<'m, D: Driver>(
    lent: &'m Lent<D, StoreAlloc>,
    ptr_key: &CasKey,
) -> Result<Option<(CasKey, &'m [u8])>, PgoStoreError> {
    let tag = ReservedTag::Drive.tag();
    let rec_key = match lent.pointed(tag, ptr_key) {
        Ok(Some(key)) => key,
        Ok(None) | Err(LentError::TargetGone { .. }) => return Ok(None),
        Err(why) => return Err(PgoStoreError::Store(why)),
    };
    let Some(held) = lent.get(&rec_key).map_err(PgoStoreError::Store)? else {
        return Ok(None);
    };
    Ok(Some((rec_key, held.bytes)))
}

/// Sans-IO merge of newly executed JIT function symbols onto an optional prior `BPG1` record,
/// returning the canonical sealed `BPG1` bytes.
pub(crate) fn merge_jit_symbols<'a>(
    existing_bpg1: Option<&[u8]>,
    executed_symbols: &[[u8; 32]],
    alloc: Alloc<'a>,
) -> Result<Vec<u8, Alloc<'a>>, PgoRecordError> {
    let mut bodies = Vec::with_capacity_in(executed_symbols.len(), alloc);
    for &sym in executed_symbols {
        bodies.push(BodyWeight { body: BodyId(sym), weight: JIT_EXECUTED_BODY_WEIGHT });
    }
    let fresh = PgoSiteTable::new_in(&[], bodies.as_slice(), 1, alloc);
    let merged = match existing_bpg1 {
        Some(bytes) => {
            let prev = PgoSiteTable::decode(bytes, alloc)?;
            prev.merge_decayed(&fresh, PGO_DECAY_NUM, PGO_DECAY_DEN, alloc)
        }
        None => fresh,
    };
    Ok(merged.encode(alloc))
}

/// Records executed function symbols from a finished JIT run into the program's persisted `BPG1`
/// CAS profile, returning `Some((ptr_key, rec_key))` so the caller roots both under `keep_program`.
pub(crate) fn record_jit_pgo<D: Driver>(
    lent: &Lent<D, StoreAlloc>,
    ptr_key: &CasKey,
    executed_symbols: &[[u8; 32]],
    alloc: Alloc<'_>,
) -> Result<Option<(CasKey, CasKey)>, PgoStoreError> {
    if executed_symbols.is_empty() {
        return Ok(None);
    }
    let existing = load_pgo(lent, ptr_key)?.map(|(_, bytes)| bytes);
    let encoded = merge_jit_symbols(existing, executed_symbols, alloc).map_err(PgoStoreError::Decode)?;
    let rec_key = CasKey(ContentId::of(PGO_REC_DOMAIN, encoded.as_slice()));
    let tag = ReservedTag::Drive.tag();
    if !matches!(lent.pointed(tag, ptr_key), Ok(Some(cur)) if cur == rec_key) {
        lent.put(&rec_key, Namespace::LOCAL, tag, encoded.as_slice()).map_err(PgoStoreError::Store)?;
        lent.point(tag, ptr_key, &rec_key).map_err(PgoStoreError::Store)?;
    }
    Ok(Some((*ptr_key, rec_key)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_cas::plain::{Liveness, Settings as StoreSettings};
    use blaze_core::FileName;
    use blaze_core::sim::{Faults, Sim};
    use crate::store::plain_place::{Facts, config, open};

    #[test]
    fn pgo_store_persists_decays_and_refuses_corrupt_bpg1() {
        let alloc = blaze_own::testing::heap().alloc();
        let sim = Sim::new(42, Faults::default());
        let mut cfg = config(FileName::new("store").expect("dir"), &StoreSettings::default(), Facts { boot: Some(1), nonce: 1 }, alloc);
        cfg.liveness = Liveness::Reached;
        let lent = open(sim, cfg, alloc).expect("open store");

        let shape = ContentId::of("pgo test shape", b"hello");
        let ptr = pgo_ptr_key(shape);
        assert!(load_pgo(&lent, &ptr).expect("empty").is_none());

        let sym_a = [0xAA; 32];
        let sym_b = [0xBB; 32];
        let (p1, r1) = record_jit_pgo(&lent, &ptr, &[sym_a], alloc).expect("write 1").expect("some");
        assert_eq!(p1, ptr);
        let (loaded_r1, bytes1) = load_pgo(&lent, &ptr).expect("load 1").expect("present");
        assert_eq!(loaded_r1, r1);
        let t1 = PgoSiteTable::decode(bytes1, alloc).expect("decode 1");
        assert_eq!(t1.body_weight(BodyId(sym_a)), JIT_EXECUTED_BODY_WEIGHT);
        assert_eq!(t1.body_weight(BodyId(sym_b)), 0);

        // Second run executes sym_a and sym_b: sym_a decays (100 * 7 / 8 = 87) + 100 = 187, sym_b = 100.
        let (_, r2) = record_jit_pgo(&lent, &ptr, &[sym_a, sym_b], alloc).expect("write 2").expect("some");
        assert_ne!(r2, r1);
        let (_, bytes2) = load_pgo(&lent, &ptr).expect("load 2").expect("present");
        let t2 = PgoSiteTable::decode(bytes2, alloc).expect("decode 2");
        assert!(t2.body_weight(BodyId(sym_a)) > t2.body_weight(BodyId(sym_b)));
        assert_eq!(t2.body_weight(BodyId(sym_b)), JIT_EXECUTED_BODY_WEIGHT);

        // Corrupt BPG1 bytes must be refused cleanly (Rule 3).
        let mut corrupt = Vec::new_in(alloc);
        corrupt.extend_from_slice(bytes2);
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xFF;
        assert!(merge_jit_symbols(Some(corrupt.as_slice()), &[sym_a], alloc).is_err());
    }
}
