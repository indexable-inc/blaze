//! A build script's outputs joined to its crate's input at TASK time ([`joined`], decision D115
//! (B)): its `cargo::rustc-cfg` atoms added to the unit's cfg, and the record as the crate's
//! `CrateInput::script` (for the expansion host `blaze_build_script::cx::ScriptCx`:
//! `env!("OUT_DIR")`, `env!` pairs, `include!` of a generated file). Never into the crate's
//! key: a script's output is not known before it runs, so it is a read of the task.
//!
//! [`ScriptRecords`]: a folder's records by the unit each serves, for the examples that take
//! records from a folder (blaze-real's names_load, this crate's names example). Records are
//! checked once ([`OutputsRef::open`]); a unit given two is a refusal, never a choice between
//! them (rule 21).

use core::fmt;

use blaze_build_script::outputs::{OutputsRef, RecordError};
use blaze_own::{AVec, Alloc, BVec, Bump};
use blaze_scope::resolve::CrateInput;
use blaze_unit_graph::UnitId;

/// `input` with its build script's outputs `outputs` joined, at the crate's task time
/// (decision D115 (B)): the script's `cargo::rustc-cfg` atoms added to the unit's cfg, sorted and
/// each once, and the record as [`CrateInput::script`]. The key stays the input's own: a
/// script's output is a read of the task (its `Script` record and out in the read list),
/// never part of a crate's key. The cfg lives in `alloc`.
pub fn joined<'w>(input: &CrateInput<'w>, outputs: OutputsRef<'w>, alloc: Bump<'w>) -> CrateInput<'w> {
    let mut cfg: BVec<'w, (&'w str, Option<&'w str>)> = BVec::with_capacity_in(input.cfg.len(), alloc);
    cfg.extend_from_slice(input.cfg);
    cfg.extend(outputs.cfgs());
    cfg.sort_unstable();
    cfg.dedup();
    CrateInput { cfg: cfg.into_bump_slice(), script: Some(outputs), ..*input }
}

/// The subject of the `Script` run a crate of the list reads: its package, version, SIDE (the
/// world's, never the graph's `for_host`) and features, the parts of its list key (ruling D115
/// (2)). The one function: the world's [`Script::Read`], the `Script` task's lookup and the
/// `CrateScope` job that reads its run all name a run by it, so two units with one crate key
/// cannot name two runs. `features` as the list holds them (sorted, each once). The bytes
/// hashed live in `alloc` for the call.
pub fn script_subject(package: &str, version: &str, side: crate::Side, features: &[&str], alloc: Alloc<'_>) -> blaze_id::ContentId {
    let mut bytes: AVec<'_, u8> = AVec::new_in(alloc);
    let mut put = |part: &[u8]| {
        bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
        bytes.extend_from_slice(part);
    };
    put(package.as_bytes());
    put(version.as_bytes());
    put(match side {
        crate::Side::Target => b"target",
        crate::Side::Macro => b"macro",
    });
    for feature in features {
        put(feature.as_bytes());
    }
    blaze_id::ContentId::of("blaze-world script subject 1", &bytes)
}

/// One served unit's record.
struct Served<'w> {
    unit: UnitId,
    outputs: OutputsRef<'w>,
}

/// Every build script's outputs of a graph, by the unit each serves, sorted by unit.
pub struct ScriptRecords<'w> {
    served: AVec<'w, Served<'w>>,
}

/// Why records are not a [`ScriptRecords`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScriptRecordsError {
    /// The bytes given for `unit` are not an outputs record.
    NotARecord { unit: UnitId, why: RecordError },
    /// `unit` was given two records.
    Twice { unit: UnitId },
}

impl fmt::Display for ScriptRecordsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScriptRecordsError::NotARecord { unit, why } => {
                write!(f, "the build-script outputs given for unit {} are not an outputs record: {why:?}", unit.0)
            }
            ScriptRecordsError::Twice { unit } => write!(f, "unit {} was given two build-script outputs records", unit.0),
        }
    }
}

impl<'w> ScriptRecords<'w> {
    /// Each `(served unit, record bytes)` opened.
    pub fn new(records: impl IntoIterator<Item = (UnitId, &'w [u8])>, alloc: Alloc<'w>) -> Result<ScriptRecords<'w>, ScriptRecordsError> {
        let mut served: AVec<'w, Served<'w>> = AVec::new_in(alloc);
        for (unit, bytes) in records {
            let outputs = OutputsRef::open(bytes).map_err(|why| ScriptRecordsError::NotARecord { unit, why })?;
            served.push(Served { unit, outputs });
        }
        served.sort_unstable_by_key(|s| s.unit);
        if let Some(pair) = served.windows(2).find(|pair| pair[0].unit == pair[1].unit) {
            return Err(ScriptRecordsError::Twice { unit: pair[0].unit });
        }
        Ok(ScriptRecords { served })
    }

    /// The digest of every stored file the records name (a native script's `OUT_DIR` files,
    /// kept beside the records, not in them), with repeats: what a file area for them holds.
    pub fn stored_digests(&self) -> impl Iterator<Item = [u8; 32]> + '_ {
        self.served.iter().flat_map(|s| s.outputs.stored().map(|file| *file.sha256))
    }

    /// These records with their stored files read from `area` (`OutputsRef::with_area`): an
    /// `include!` of one reads it there, and a missing area or file is the reader's typed
    /// `StoredError`, never an empty file.
    pub fn with_area(mut self, area: &'w dyn blaze_build_script::outputs::StoredBytes) -> ScriptRecords<'w> {
        for served in self.served.iter_mut() {
            served.outputs = served.outputs.with_area(area);
        }
        self
    }

    fn find(&self, lib: UnitId) -> Option<&Served<'w>> {
        self.served.binary_search_by_key(&lib, |s| s.unit).ok().map(|at| &self.served[at])
    }

    /// The record of the script serving `lib`, read in place.
    pub fn outputs(&self, lib: UnitId) -> Option<OutputsRef<'w>> {
        self.find(lib).map(|s| s.outputs)
    }

    /// How many units have a record.
    pub fn len(&self) -> usize {
        self.served.len()
    }

    pub fn is_empty(&self) -> bool {
        self.served.is_empty()
    }
}
