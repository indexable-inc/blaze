//! What every build-script run of a command is answered with, made once from its world: the
//! probe table of each side and, with the `procs-run` feature, the whole setting
//! ([`ScriptSetup::open_recorded`]: both tables, the spawn setting, the native runner). The one
//! home of it (rule 30): blaze-cli's `Script` task (scripts.rs `Scripts::open_shared`, over the
//! command's store) and blaze-proc-macro's
//! `tempo_scripts` example (the census's exporter) both call it, so a probe the command answers
//! is answered by the example too, from the same world.
//!
//! A side's probe table, sorted by key ([`probe_table`]):
//! - the pinned rows (blaze-build-script `pinned::TEMPO`) whose `needs` the side's sysroot ships
//!   ([`shipped`]: the world's toolchain crates of that side), each keyed for the side's triple
//!   (`probe_ask::Compile::key` holds the target), so a try-compile is answered by a row made
//!   for its target, never by another triple's;
//! - one ships fact per shipped crate (`probe_ask::ships_key`), which lets blaze decide a probe
//!   of no items itself (`probe_ask::decide`: autocfg's empty crate);
//! - the library-feature listings ([`feature_table`]), which answer `#![feature(..)]` probes from
//!   the toolchain's own sources (`probe_ask::feature_rows`), kept as a record keyed from the
//!   world (lib_features_record.rs): a command whose store holds it lexes no toolchain file.
//!
//! A question none answers is the runtime's refusal naming the triple
//! (`SpawnRefusal::CannotAnswerYet`).

use core::fmt;

use blaze_build_script::probe::Row;
use blaze_build_script::probe_ask::{feature_rows, ships_rows};
use blaze_id::ContentId;
use blaze_own::{AVec, Alloc};
use blaze_scope::lib_features::{LibFeatures, LibFeaturesOpts, ScanError};
use blaze_scope::resolve::SourceTexts;

use crate::lib_features_record::{self, FeatureKeyError, FeatureRecordError, FeatureRecords, Phase, PhaseListing, ShelfTexts, features_key};
use crate::{Crate, Origin, Side, World};

/// The toolchain crates the world holds for `side` (its sysroot's), by library name.
pub fn shipped<'r, 'w: 'r>(world: &'r World<'w>, side: Side) -> impl Iterator<Item = &'w str> + Clone + 'r {
    world.crates.iter().filter(move |krate| toolchain_of(krate, side)).map(|krate| krate.input.name)
}

pub(crate) fn toolchain_of(krate: &Crate<'_>, side: Side) -> bool {
    matches!(krate.origin, Origin::Toolchain(_)) && krate.side == side
}

/// The pinned rows whose `needs` `shipped` holds, keyed for `triple`, and one ships fact per
/// shipped crate, unsorted.
pub fn pinned_table<'a, 's>(triple: &str, shipped: impl Iterator<Item = &'s str> + Clone, alloc: Alloc<'a>) -> AVec<'a, Row> {
    let mut known = AVec::new_in(alloc);
    for row in blaze_build_script::pinned::TEMPO {
        if row.needs.iter().all(|needed| shipped.clone().any(|name| name == *needed)) {
            known.push(*row);
        }
    }
    let mut rows = blaze_build_script::probe::pinned_rows(triple, &known, alloc);
    rows.extend_from_slice(&ships_rows(triple, shipped, alloc));
    rows
}

/// A toolchain crate's library features could not be listed: the crate, the triple whose cfg
/// it was read under, and the scan's refusal (naming the file and the byte).
#[derive(Debug)]
pub struct ProbeTableError<'w, 'a> {
    pub triple: &'w str,
    pub krate: &'w str,
    pub why: ScanError<'a>,
}

impl fmt::Display for ProbeTableError<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the toolchain crate `{}` for {}: {}", self.krate, self.triple, self.why)
    }
}

impl core::error::Error for ProbeTableError<'_, '_> {}

/// Why a side's library-feature listings are not made: no key covers the walk, the walk's
/// refusal, the store's, or a record under the key that does not read (each names it).
#[derive(Debug)]
pub enum FeatureTableError<'w, 'a, E> {
    Key(FeatureKeyError<'w>),
    Scan(ProbeTableError<'w, 'a>),
    Store { key: ContentId, why: E },
    Record(FeatureRecordError),
}

impl<E: fmt::Display> fmt::Display for FeatureTableError<'_, '_, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeatureTableError::Key(error) => write!(f, "{error}"),
            FeatureTableError::Scan(error) => write!(f, "{error}"),
            FeatureTableError::Store { key, why } => write!(f, "the library-features record {key:x}: the store: {why}"),
            FeatureTableError::Record(error) => write!(f, "{error}"),
        }
    }
}

impl<E: fmt::Display + fmt::Debug> core::error::Error for FeatureTableError<'_, '_, E> {}

/// The library-feature listings of `side` for `triple` (`probe_ask::feature_rows`), unsorted:
/// for each crate a probe of no items loads (`core` under `#![no_std]`, else `std`), every
/// toolchain crate of the side it reaches by its dependencies (the crates rustc's `lib_features`
/// reads), each read under its own cfg and features by blaze-scope's `lib_features`. `core`'s
/// closure is read first and `std`'s listing goes on from it (std's closure holds core's).
///
/// A record (lib_features_record.rs, rule 77): keyed from the world before any file is read
/// (`features_key`: `tool`, the triple, the shelf's packs, each visited crate's root, cfg and
/// features), read from `records` when it holds it (no file is lexed), else walked, written
/// there and answered. A record that does not read is the typed error naming it, never a walk
/// in its place (rule 3).
pub fn feature_table<'w, 'a, S: FeatureRecords>(
    world: &World<'w>,
    side: Side,
    triple: &'w str,
    tool: ContentId,
    records: &mut S,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, Row>, FeatureTableError<'w, 'a, S::Error>> {
    feature_table_by(world, side, triple, tool, records, alloc, |phases, alloc| scan_phases(world, phases, triple, alloc))
}

/// [`feature_table`] walking by `scan`: the seam by which a test shows a hit never walks.
pub(crate) fn feature_table_by<'w, 'a, S: FeatureRecords>(
    world: &World<'w>,
    side: Side,
    triple: &'w str,
    tool: ContentId,
    records: &mut S,
    alloc: Alloc<'a>,
    scan: impl FnOnce(&[Phase<'a>], Alloc<'a>) -> Result<AVec<'a, PhaseListing<'w, 'a>>, ProbeTableError<'w, 'a>>,
) -> Result<AVec<'a, Row>, FeatureTableError<'w, 'a, S::Error>> {
    let phases = lib_features_record::phases(world, side, alloc);
    let key = features_key(world, &phases, triple, tool, alloc).map_err(FeatureTableError::Key)?;
    if let Some(bytes) = records.get(key).map_err(|why| FeatureTableError::Store { key, why })? {
        let listed = lib_features_record::decode(key, bytes, alloc).map_err(FeatureTableError::Record)?;
        return Ok(rows_of(triple, &listed, alloc));
    }
    let listed = scan(&phases, alloc).map_err(FeatureTableError::Scan)?;
    let bytes = lib_features_record::encode(key, &listed, alloc).map_err(FeatureTableError::Record)?;
    records.put(key, &bytes).map_err(|why| FeatureTableError::Store { key, why })?;
    Ok(rows_of(triple, &listed, alloc))
}

/// The walk of `phases` (`lib_features_record::phases` of `world`): each phase's listing,
/// read from the shelf's trees alone (`ShelfTexts`, the trees the key covers).
pub(crate) fn scan_phases<'w, 'a>(world: &World<'w>, phases: &[Phase<'_>], triple: &'w str, alloc: Alloc<'a>) -> Result<AVec<'a, PhaseListing<'w, 'a>>, ProbeTableError<'w, 'a>> {
    let texts = ShelfTexts::new(&world.texts);
    let mut features = LibFeatures::new(LibFeaturesOpts { alloc });
    let mut out = AVec::with_capacity_in(phases.len(), alloc);
    for phase in phases {
        for &at in phase.crates.iter() {
            let krate = &world.crates[at];
            // Only a crate of `#![feature(staged_api)]` may carry a stability attribute (rustc
            // refuses one elsewhere, E0734): the registry crates beneath std (libc, cfg-if,
            // hashbrown, ...) declare no feature and are not read.
            let staged = texts.read(at, krate.input.root).is_some_and(|text| text.contains("staged_api"));
            if !staged {
                continue;
            }
            let mut atoms = AVec::new_in(alloc);
            atoms.extend_from_slice(krate.input.cfg);
            atoms.extend(krate.input.features.iter().map(|feature| ("feature", Some(*feature))));
            let cfg = blaze_scope::task::Cfg::from_atoms(&atoms, alloc);
            if let Err(why) = features.scan(&texts, at, krate.input.root, &cfg) {
                return Err(ProbeTableError { triple, krate: krate.input.name, why });
            }
        }
        let listed = features.listed(alloc);
        out.push(PhaseListing { needs: phase.needs, declared: listed.declared, mentioned: listed.mentioned });
    }
    Ok(out)
}

/// Each phase's rows (`probe_ask::feature_rows`), in phase order.
fn rows_of<'a>(triple: &str, listed: &[PhaseListing<'_, '_>], alloc: Alloc<'a>) -> AVec<'a, Row> {
    let mut rows = AVec::new_in(alloc);
    for phase in listed {
        rows.extend_from_slice(&feature_rows(triple, phase.needs, &phase.declared, &phase.mentioned, alloc));
    }
    rows
}

/// The probe table of `side` for `triple`, sorted by key: [`pinned_table`] over [`shipped`] and
/// [`feature_table`] (its record in `records`, keyed with `tool`).
pub fn probe_table<'w, 'a, S: FeatureRecords>(
    world: &World<'w>,
    side: Side,
    triple: &'w str,
    tool: ContentId,
    records: &mut S,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, Row>, FeatureTableError<'w, 'a, S::Error>> {
    let mut rows = pinned_table(triple, shipped(world, side), alloc);
    rows.extend_from_slice(&feature_table(world, side, triple, tool, records, alloc)?);
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(rows)
}

#[cfg(feature = "procs-run")]
pub use setup::{NativeRefused, ScriptSetup, ScriptSetupError, ScriptSetupOpts};

#[cfg(feature = "procs-run")]
mod setup {
    use std::sync::Arc;
    use std::vec::Vec;

    use blaze_build_script::probe::Row;
    use blaze_own::Alloc;
    use blaze_proc_macro::fact_pins::{CommandPinsError, command_spawn_setting};
    use blaze_proc_macro::native_setting::{NativeRun, NativeRunError, NativeRunOpts};
    use blaze_proc_macro::spawn_route::SpawnSetting;

    use blaze_id::ContentId;

    use super::{FeatureTableError, probe_table};
    use crate::lib_features_record::FeatureRecords;
    use crate::{Side, World};

    /// What [`ScriptSetup::open_recorded`] is made from: what the caller's edge read (rule 75).
    pub struct ScriptSetupOpts<'w, 'a, 's, R> {
        pub world: &'w World<'w>,
        /// The triple the target side's scripts run for (the host's: `HOST` and `TARGET`).
        pub target: &'w str,
        /// The triple the macro side's scripts run for (`wasm32-wasip1`, D89 (a)).
        pub macro_target: &'w str,
        /// What the native runner is opened from, or the caller's refusal `R` saying why its
        /// edge has no setting (no SDK, no shim beside it); a script placed native then fails
        /// with it, never run as wasm (D112).
        pub native: Result<NativeRunOpts<'a, 's>, R>,
        pub alloc: Alloc<'a>,
    }

    /// Why the command's native runner is not open: the caller's own refusal, or the opening's.
    #[derive(Debug)]
    pub enum NativeRefused<R> {
        Caller(R),
        Open(NativeRunError),
    }

    impl<R: core::fmt::Display> core::fmt::Display for NativeRefused<R> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                NativeRefused::Caller(why) => write!(f, "{why}"),
                NativeRefused::Open(error) => write!(f, "{error}"),
            }
        }
    }

    /// Everything a command's build-script runs share, made once: each side's probe table
    /// ([`super::probe_table`]), the spawn setting (the derived facts' pins,
    /// `fact_pins::command_spawn_setting`) and the native runner, or why it is not open. A
    /// `script_unit::Setting` is made from these for each run.
    pub struct ScriptSetup<'a, R> {
        pub table: Arc<Vec<Row>>,
        pub macro_table: Arc<Vec<Row>>,
        pub spawn: Arc<SpawnSetting>,
        pub native: Result<NativeRun<'a>, NativeRefused<R>>,
    }

    /// Why no build script of the command can be answered: every one fails with these words.
    /// `E` is the library-features store's refusal ([`FeatureRecords::Error`]).
    #[derive(Debug)]
    pub enum ScriptSetupError<'w, 'a, E> {
        Table(FeatureTableError<'w, 'a, E>),
        Spawn(CommandPinsError<'a>),
    }

    impl<E: core::fmt::Display> core::fmt::Display for ScriptSetupError<'_, '_, E> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                ScriptSetupError::Table(error) => write!(f, "the probe table: {error}"),
                ScriptSetupError::Spawn(error) => write!(f, "the command's spawn setting (the derived facts' pins): {error}"),
            }
        }
    }

    impl<E: core::fmt::Display + core::fmt::Debug> core::error::Error for ScriptSetupError<'_, '_, E> {}

    impl<'a, R> ScriptSetup<'a, R> {
        /// The setting of the scripts of `opts.world`, each side's library-features record read
        /// from `records` or made and written there (keyed with `tool`, the caller's identity of
        /// blaze's build: script_setting.rs `feature_table`). A table or the spawn setting that
        /// cannot be made stops every script ([`ScriptSetupError`]); a native runner that
        /// cannot be opened stops only the scripts placed native ([`ScriptSetup::native`]). The
        /// runner is opened last, so a failure before it leaves no jobserver fifo behind.
        pub fn open_recorded<'w, S: FeatureRecords>(
            opts: ScriptSetupOpts<'w, 'a, '_, R>,
            tool: ContentId,
            records: &mut S,
        ) -> Result<ScriptSetup<'a, R>, ScriptSetupError<'w, 'a, S::Error>> {
            let ScriptSetupOpts { world, target, macro_target, native, alloc } = opts;
            let table = probe_table(world, Side::Target, target, tool, records, alloc).map_err(ScriptSetupError::Table)?;
            let macro_table = probe_table(world, Side::Macro, macro_target, tool, records, alloc).map_err(ScriptSetupError::Table)?;
            let spawn = command_spawn_setting(target, alloc).map_err(ScriptSetupError::Spawn)?;
            let native = match native {
                Ok(opts) => NativeRun::new(opts).map_err(NativeRefused::Open),
                Err(why) => Err(NativeRefused::Caller(why)),
            };
            Ok(ScriptSetup { table: Arc::new(table.to_vec()), macro_table: Arc::new(macro_table.to_vec()), spawn: Arc::new(spawn), native })
        }
    }
}
