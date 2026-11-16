//! The filler of [`ScriptOutputs`]: each build script's outputs record, read in place, by the
//! unit it serves (the library or proc macro whose `build_script` it is: blaze-proc-macro's
//! `script_unit::served`). Its `cargo::rustc-cfg` atoms are what [`world`](crate::world) joins
//! to that unit's cfg; the record itself goes into the crate's input (`CrateInput::script`, for
//! the expansion host `blaze_build_script::cx::ScriptCx`: `env!("OUT_DIR")`, `env!` pairs,
//! `include!` of a generated file), and its crate digest into the crate's key.
//!
//! Records are checked once ([`OutputsRef::open`]); a unit given two is a refusal, never a
//! choice between them (rule 21).

use core::fmt;

use blaze_build_script::outputs::{OutputsRef, RecordError};
use blaze_own::{AVec, Alloc};
use blaze_unit_graph::UnitId;

use crate::ScriptOutputs;

/// One served unit's record and its cfg atoms, sorted and each once.
struct Served<'w> {
    unit: UnitId,
    outputs: OutputsRef<'w>,
    cfg: AVec<'w, (&'w str, Option<&'w str>)>,
    /// [`OutputsRef::crate_digest`], once.
    digest: blaze_id::ContentId,
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
    /// Each `(served unit, record bytes)` opened and its cfg atoms gathered.
    pub fn new(records: impl IntoIterator<Item = (UnitId, &'w [u8])>, alloc: Alloc<'w>) -> Result<ScriptRecords<'w>, ScriptRecordsError> {
        let mut served: AVec<'w, Served<'w>> = AVec::new_in(alloc.clone());
        for (unit, bytes) in records {
            let outputs = OutputsRef::open(bytes).map_err(|why| ScriptRecordsError::NotARecord { unit, why })?;
            let mut cfg = AVec::new_in(alloc.clone());
            cfg.extend(outputs.cfgs());
            cfg.sort_unstable();
            cfg.dedup();
            served.push(Served { unit, outputs, cfg, digest: outputs.crate_digest() });
        }
        served.sort_unstable_by_key(|s| s.unit);
        if let Some(pair) = served.windows(2).find(|pair| pair[0].unit == pair[1].unit) {
            return Err(ScriptRecordsError::Twice { unit: pair[0].unit });
        }
        Ok(ScriptRecords { served })
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

/// Given to [`world`](crate::world) as `Some(&&records)`: the atoms live as long as the records.
impl<'w> ScriptOutputs<'w> for &'w ScriptRecords<'w> {
    fn served(&self, lib: UnitId) -> Option<crate::Served<'w>> {
        let records: &'w ScriptRecords<'w> = self;
        records.find(lib).map(|s| crate::Served { outputs: s.outputs, cfg: s.cfg.as_slice(), digest: s.digest })
    }
}
