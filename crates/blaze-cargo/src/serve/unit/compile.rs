//! One unit's compile ([`Front::compile`](crate::serve::work::Front::compile)): its
//! diagnostics as rustc gives them and cargo shows them, then, unless it is a check, its code
//! compiled and linked by blaze, and every file cargo would write for it. Each of its
//! [`Phase`]s is a node of the plane of its own, timed by what it does: what one made waits
//! for the next in the unit's [`Phases`].
//!
//! A unit's result is kept in the store as its record, under what it was made from: the
//! front end's identity of the unit's inputs (its sources, cfgs, env, its dependencies' and
//! their build scripts' results, deeply), its plan, its profile and target, the code identities
//! of the front end, the code generator and what makes the record
//! ([`crate::serve::RecordMakers`]), the bytes of a `-C profile-use` profile, the goal and the
//! colour of its diagnostics ([`RecordInputs`]),
//! and filed by what its code read that no key names, the process variables and what its
//! proc-macro expansions read outside the tree, with their values ([`reads`]). A unit
//! whose record and outputs are all in the store is fresh: its diagnostics are replayed as
//! cargo replays a fresh unit's, and nothing runs.

#![forbid(unsafe_code)]

pub(super) mod doctest;

use std::{fmt, sync::Arc};

use blaze_api::{ApiError, Artifact, Diagnostic, Level};
use blaze_cas::{Key, Kind};
use blaze_codegen::{driver::ProfileCounts, settings::Pgo};
use blaze_core::lock::{Lock, LockGuard};
use blaze_diagnostics::{Diag, Options, lints::CommandLine};
use blaze_ids::{ContentId, Hasher};
use blaze_items::CodeHash;
use blaze_lower::{ProcMacroLoadResult, ProcMacroLoadingError};
use blaze_profile::span::{self, Name};
use blaze_units::schema::{Mode, Unit};
use hir_ty_codegen::v2::BodyChecks;
use serde::{Deserialize, Serialize};

use super::{
    Job, Real, UnitReader, artifact,
    checks::Checking,
    checks_only,
    codegen::{self, Makes, Recorded},
    files::Mounted,
    graph, reads, say, script,
};
use crate::serve::work::{Compiled, Failed, Goal, Linking, Output, Phase, Put};

type Image = crate::serve::work::Image<Real>;

/// What a compiled unit made that later work of the job reads.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct Made {
    /// Its main file and content: a build script's program, a test binary, a library.
    pub(crate) main: Option<(String, ContentId)>,
    /// A test binary's tests, in the harness's order.
    pub(crate) tests: Vec<TestOf>,
    /// What the binary does depends on beside the code it reaches: its target, toolchain,
    /// codegen settings and panic strategy ([`blaze_usercode::test::Suite::context`]).
    pub(crate) context: Option<ContentId>,
    /// Every instance its codegen compiled or found, with the code hash it was made from, as
    /// [`blaze_api::Response::CodegenInputs`] lists them: sorted by key.
    pub(crate) inputs: Vec<(ContentId, ContentId)>,
    /// A test binary's execution table, made beside its link
    /// ([`super::test::image::table`]), which its tests are selected by: kept with its record,
    /// so a replayed binary selects by the table it was linked with.
    pub(crate) table: Option<blaze_codegen::ExecTable>,
    /// A test binary's span tables, a hash of each, sorted: every test's output depends on
    /// them (a panic's `file:line:col`), so a change to one reaches every test
    /// ([`super::reach`]). What else a test reaches is walked from [`Made::table`] when asked.
    pub(crate) spans: Vec<CodeHash>,
}

/// A test of a test binary, as selection sees it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct TestOf {
    pub(crate) name: String,
    pub(crate) ignore: bool,
    /// `#[should_panic]`, with its `expected` message if it gives one.
    pub(crate) should_panic: Option<Option<String>>,
    pub(crate) bench: bool,
    /// It reaches the outside world, as codegen marks it: it always runs. What else it
    /// reaches is walked from its binary's table when asked ([`Made::table`],
    /// `ExecTable::reaches`), never per test at build.
    pub(crate) outside: bool,
}

/// A unit's result as the store keeps it, after the [`header`]: what a replay gives back.
#[derive(Deserialize)]
struct Record {
    ok: bool,
    open: bool,
    why: Option<String>,
    diagnostics: Vec<Diagnostic>,
    outputs: Vec<(String, Key, bool)>,
    artifact: Option<Artifact>,
    made: Made,
}

/// [`Record`] as it is written, borrowing what the compile made rather than copying it.
#[derive(Serialize)]
struct RecordRef<'a> {
    ok: bool,
    open: bool,
    why: Option<&'a str>,
    diagnostics: &'a [Diagnostic],
    outputs: Vec<(&'a str, Key, bool)>,
    artifact: Option<&'a Artifact>,
    made: &'a Made,
}

/// The domain of a record's key, and the record's format version: bumped by hand on any change
/// to what a record holds or how it is laid out ([`Record`], [`RecordRef`], [`header`]) or to
/// how its key is made ([`RecordInputs::key`]), so that no record of another format is read as
/// this one's. blaze-cargo's own code is of [`crate::serve::RecordCode`] too, so such a change
/// moves every key even unbumped; the bump says so by name. 9: keyed by
/// [`crate::serve::RecordMakers`] in place of the executable's bytes.
const RECORD: &str = "blaze v2 cargo unit record 9";
const OUTPUT: &str = "blaze v2 cargo output 1";

/// The store's key of an output file's `bytes`: what a record names it by, and what a dev
/// executable's program is known by in the daemon's code memory ([`super::run::inproc`]).
pub(crate) fn output_key(bytes: &[u8]) -> Key {
    Key(ContentId::of(OUTPUT, bytes))
}
/// A span table's anchor and content, as a test's selection holds it.
const SPANS: &str = "blaze v2 cargo test spans 1";
/// A test binary's context with the native inputs its link read, in link order.
const NATIVES: &str = "blaze v2 cargo test natives 1";

/// The record's fixed header, which `fresh` reads where the store has it, allocating nothing:
/// `MAGIC`, the count of outputs, of native inputs and of profiles (`u32` LE each), each
/// output's key (32 bytes), then each native input's BLAKE3 hash (32 bytes), its path's length
/// (`u32` LE) and its path, then each profile its codegen used ([`super::codegen::counted`]):
/// its content ID (32 bytes) and its used, mismatched and applied counts (`u64` LE each),
/// sorted by ID. The CBOR of [`Record`] follows.
pub(crate) mod header {
    use blaze_codegen::driver::ProfileCounts;
    use blaze_format::cursor::{Put as _, Reader};
    use blaze_ids::ContentId;

    pub(crate) const MAGIC: &[u8; 4] = b"BZU2";

    /// A profile's entry: its ID and three counts.
    const PROFILE: usize = 32 + 3 * 8;

    pub(crate) fn write(
        out: &mut Vec<u8>,
        outputs: &[blaze_cas::Key],
        natives: &[(String, ContentId)],
        profiles: &[(ContentId, ProfileCounts)],
    ) {
        let paths: usize = natives.iter().map(|it| it.0.len()).sum();
        out.reserve(
            16 + 32 * outputs.len() + 36 * natives.len() + paths + PROFILE * profiles.len(),
        );
        out.put_bytes(MAGIC);
        out.put_u32(outputs.len() as u32);
        out.put_u32(natives.len() as u32);
        out.put_u32(profiles.len() as u32);
        for key in outputs {
            out.put_bytes(&key.0.0);
        }
        for (path, hash) in natives {
            out.put_bytes(&hash.0);
            out.put_bytes_u32(path.as_bytes());
        }
        for (id, counts) in profiles {
            out.put_bytes(&id.0);
            out.put_u64(counts.used);
            out.put_u64(counts.mismatched);
            out.put_u64(counts.applied);
        }
    }

    /// The profiles of the header `bytes` begin with, as [`write`] wrote them; `None` for a
    /// header that is not, or is cut short.
    pub(crate) fn profiles(bytes: &[u8]) -> Option<Vec<(ContentId, ProfileCounts)>> {
        let mut read = Reader::new(bytes);
        if read.array::<4>().ok()? != MAGIC {
            return None;
        }
        let outputs = read.u32().ok()?;
        let natives = read.u32().ok()?;
        let profiles = read.u32().ok()?;
        read.skip(32 * outputs as usize).ok()?;
        for _ in 0..natives {
            read.skip(32).ok()?;
            read.str_u32().ok()?;
        }
        // alloc: none for a unit that used no profile; one per record of one that did
        let mut out = Vec::new();
        for _ in 0..profiles {
            let id = ContentId(*read.array().ok()?);
            let counts = ProfileCounts {
                used: read.u64().ok()?,
                mismatched: read.u64().ok()?,
                applied: read.u64().ok()?,
            };
            out.push((id, counts));
        }
        Some(out)
    }

    /// The outputs and the native inputs of the header `bytes` begin with, as [`write`] wrote
    /// them; `None` for a header that is not, or is cut short.
    pub(crate) fn outputs_natives(
        bytes: &[u8],
    ) -> Option<(Vec<blaze_cas::Key>, Vec<(String, ContentId)>)> {
        let mut read = Reader::new(bytes);
        if read.array::<4>().ok()? != MAGIC {
            return None;
        }
        let outputs = read.u32().ok()?;
        let natives = read.u32().ok()?;
        read.u32().ok()?;
        // alloc: once per record read this way, its few outputs and natives
        let mut keys = Vec::with_capacity(outputs as usize);
        for _ in 0..outputs {
            keys.push(blaze_cas::Key(ContentId(*read.array().ok()?)));
        }
        let mut read_natives = Vec::with_capacity(natives as usize); // alloc: as above
        for _ in 0..natives {
            let hash = ContentId(*read.array().ok()?);
            read_natives.push((read.str_u32().ok()?.to_owned(), hash)); // alloc: as above
        }
        Some((keys, read_natives))
    }

    /// Whether every output is in the store and every native input is as it was, and where the
    /// CBOR body starts; `None` for a record that is not, or a header that is cut short.
    pub(crate) fn check(
        bytes: &[u8],
        has: impl Fn(&blaze_cas::Key) -> bool,
        file: &mut impl FnMut(&std::path::Path) -> Option<[u8; 32]>,
    ) -> Option<usize> {
        let mut read = Reader::new(bytes);
        if read.array::<4>().ok()? != MAGIC {
            return None;
        }
        let outputs = read.u32().ok()?;
        let natives = read.u32().ok()?;
        let profiles = read.u32().ok()?;
        for _ in 0..outputs {
            let key = blaze_cas::Key(blaze_ids::ContentId(*read.array().ok()?));
            if !has(&key) {
                return None;
            }
        }
        for _ in 0..natives {
            let hash: [u8; 32] = *read.array().ok()?;
            let path = read.str_u32().ok()?;
            if file(std::path::Path::new(path))? != hash {
                return None;
            }
        }
        read.skip(PROFILE.checked_mul(profiles as usize)?).ok()?;
        Some(read.at())
    }
}

/// The base key of `unit`'s record ([`RecordInputs::key`]), which [`reads`] files it under
/// with what its code read: none while the front end can't say what the unit's inputs are.
/// Worked out once per job and unit, once the writer has set the unit's identity; before
/// that, none and nothing memoized.
fn record_key(job: &Job, unit: usize) -> Result<Option<Key>, ApiError> {
    if let Some(key) = job.keys.get(unit).and_then(|it| it.get()) {
        return Ok(*key);
    }
    let Some(identity) = job.identities.get(unit).and_then(|it| it.get()) else { return Ok(None) };
    let key = match identity {
        Some(identity) => Some(work_out_key(job, unit, *identity)?),
        None => None,
    };
    if let Some(slot) = job.keys.get(unit) {
        let _ = slot.set(key);
    }
    Ok(key)
}

/// `unit`'s identity in the front end, once the writer set it.
fn identity(job: &Job, unit: usize) -> Option<ContentId> {
    job.identities.get(unit).and_then(|it| it.get()).copied().flatten()
}

fn work_out_key(job: &Job, unit: usize, identity: ContentId) -> Result<Key, ApiError> {
    with_record_inputs(job, unit, identity, |inputs| inputs.key(unit))
}

/// `then` of the inputs of `unit`'s record key: the one place they are gathered, so the key
/// and its parts as `blaze why` names them ([`RecordInputs::parts`]) cannot differ.
fn with_record_inputs<T>(
    job: &Job,
    unit: usize,
    identity: ContentId,
    then: impl FnOnce(&RecordInputs<'_>) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let u = job.unit(unit)?;
    // The code of what makes it ([`super::deps::record_makers`]): a record is made by
    // blaze-cargo itself, from the front end's and the code generator's outputs.
    let makers = super::deps::record_makers()?;
    let runs = runs(job, unit);
    // An inert script's compile makes nothing, unless `BLAZE_ELIDE=verify` builds it to run it.
    let verified = u.target.is_custom_build()
        && !script::ported(job, unit)
        && script::verifies(job)?
        && script::inert(job, unit)?.is_some();
    // The flags name a profile by its path; its bytes are the input.
    let pgo = match codegen::unit_pgo(job, unit)
        .map_err(|what| ApiError::new(what, Some(u.target.name.clone())))?
    {
        Pgo::Use(profile) => Some(profile),
        Pgo::None | Pgo::Generate => None,
    };
    // The lints name their profile by its path too; its bytes are the input.
    let lint_profile = lint_profile(job, unit)?.map(|it| it.profile.id);
    // The profiles the images it links are laid out by: their heat is the input
    // (docs/v2/plan/lanes/layout/layout.md).
    let links =
        matches!(codegen::makes(u), Ok(Makes::Executable | Makes::Shared { proc_macro: false }));
    let layout = match links && !checks_only(job.input.goal, u) {
        true => super::layout::key(job, job.plan(unit)?)
            .map_err(|what| ApiError::new(what, Some(u.target.name.clone())))?,
        false => None,
    };
    let inputs = RecordInputs {
        identity,
        unit: KeyedUnit::of(job, u)?,
        plan: job.plan(unit)?,
        run_env: &runs.env,
        script_check_cfgs: script_check_cfgs(job, unit)?,
        script_link_args: script_link_args(job, unit)?,
        linked_rustflag_libs: linked_rustflag_libs(job, unit)?,
        makers,
        goal: keyed_goal(job.input.goal, u),
        color: job.input.color,
        short: job.input.short,
        verified,
        reached: reached_only(job, u)?,
        pgo,
        lint_profile,
        layout,
        width: job.input.width,
        refused: job.input.refused,
    };
    then(&inputs)
}

/// What a unit whose record the store does not keep is filed under past its record key: what
/// a miss whose key is as it was is down to ([`blaze_kernel::Miss::beyond`]).
const FILED_BEYOND: &str =
    "the values of the process variables and proc-macro reads its record is filed under";

/// What `unit` is across edits, telling apart units side by side (one package's lib built
/// for a build script and for the target, or as a test and as a lib, or for the target and
/// for the wasm macros of one triple): its package, target, mode, platform and whether it is
/// a wasm macro's, profile and whether it is std's or checked as a test. What `blaze why`
/// compares it with its last miss by ([`blaze_kernel::Miss::subject`]).
pub(super) fn unit_subject(job: &Job, u: &Unit) -> Result<ContentId, String> {
    // The workspace's root too: one `BLAZE_HOME`'s ledger holds every workspace's units, and a
    // registry crate's package id is the same in each (rule 18).
    let workspace = &job.input.config.invocation.root;
    let named = (
        workspace,
        &u.pkg_id,
        &u.target.name,
        &u.target.kind,
        u.mode,
        &u.platform,
        u.macro_wasm,
        &u.profile.name,
        u.is_std,
        u.checked_as_test,
    );
    let bytes = blaze_protocol::cbor::to_vec(&named)
        .map_err(|why| format!("encoding what unit `{}` is: {why}", span_unit(u)))?;
    Ok(Hasher::new(SUBJECT).part(&bytes).finish())
}

/// The subject of a function or a run, `subject` within its build, as one of the unit whose
/// subject is `unit`: two units' builds of one function are two subjects.
pub(super) fn within(unit: ContentId, subject: ContentId) -> ContentId {
    Hasher::new(WITHIN).id(&unit).id(&subject).finish()
}

/// The domains of [`unit_subject`] and [`within`].
const SUBJECT: &str = "blaze v2 why unit subject 3";
const WITHIN: &str = "blaze v2 why subject within a unit 1";

/// Why `unit`'s record was not in the store, for `blaze why`: its key's parts by name, which
/// the daemon tells apart against the unit's last compute. Made on a miss only. The parts are
/// made from inputs gathered again, and refused unless they make the very key the record was
/// looked up under, so they never describe other inputs than the key's.
fn missed(job: &Job, unit: usize) -> blaze_kernel::Miss {
    use blaze_kernel::{Miss, Missed};
    let (item, subject) = match job.unit(unit) {
        Ok(u) => (span_unit(u), unit_subject(job, u)),
        Err(error) => (format!("unit {unit}"), Err(error.what)), // alloc: once a miss
    };
    let refused = |subject: ContentId, missed: Missed| Miss {
        step: "unit",
        subject,
        item: item.clone(), // clone: once a miss
        missed,
        key: ContentId::default(),
        kept: None,
        // alloc: none
        parts: Vec::new(),
        beyond: None,
    };
    let subject = match subject {
        Ok(it) => it,
        Err(why) => return refused(ContentId::default(), Missed::Failed(why)),
    };
    let made = (|| -> Result<Miss, String> {
        let identity = match job.identities.get(unit).and_then(|it| it.get()) {
            // alloc: once a miss
            None => return Err(
                "its crate's identity in the front end was not set when its record was looked up"
                    .to_owned(),
            ),
            Some(None) => {
                return Ok(refused(
                    subject,
                    Missed::Always(
                        // alloc: once a miss
                        "the front end can't say what its inputs are, so it has no record key and is compiled every build"
                            .to_owned(),
                    ),
                ));
            }
            Some(Some(identity)) => *identity,
        };
        let Some(looked) = record_key(job, unit).map_err(|error| error.what)? else {
            // alloc: once a miss
            return Err("its record has no key, though its crate's identity is set".to_owned());
        };
        let (key, parts) = with_record_inputs(job, unit, identity, |inputs| {
            Ok((inputs.key(unit)?, inputs.parts(unit)?))
        })
        .map_err(|error| error.what)?;
        if key != looked {
            // alloc: once a miss
            return Err("its inputs changed between its record's lookup and now (a profile it reads was rewritten during the build), so its parts would not describe the key it missed under".to_owned());
        }
        let mut miss = Miss::of_parts("unit", subject, item.clone(), key.0, parts); // clone: once a miss
        miss.beyond = Some(FILED_BEYOND);
        Ok(miss)
    })();
    made.unwrap_or_else(|why| refused(subject, Missed::Failed(why)))
}

/// What a unit's record key is made of. The process's environment is not among it: a record
/// is filed by the variables its code read and their values ([`reads`]), so a variable no
/// crate of the unit read (a per-run `BLAZE_PROFILE`) leaves the key as it was. The variables
/// of blaze's own that change what a compile makes are here by name: `BLAZE_ELIDE`
/// (`verified`) and `BLAZE_DEPS` (`reached`).
pub(crate) struct RecordInputs<'a> {
    /// The front end's identity of the unit's crate ([`blaze_lower::Lowered::identity`]).
    pub(crate) identity: ContentId,
    pub(crate) unit: KeyedUnit<'a>,
    pub(crate) plan: &'a blaze_units::query::UnitPlan,
    /// How its program is run ([`artifact::run_env`]).
    pub(crate) run_env: &'a [(String, String)],
    /// The `--check-cfg`s its package's build script runs printed ([`script_check_cfgs`]):
    /// its check's lints read them, and neither its plan nor its crate's identity holds them.
    pub(crate) script_check_cfgs: Vec<String>,
    /// The `rustc-link-arg*`s of the build scripts it is linked with, each with its script's
    /// package ([`script_link_args`]): its link reads them, and neither its plan (which has
    /// the rustflags' `-C link-arg`s) nor its crate's identity holds them.
    pub(crate) script_link_args: Vec<(String, String)>,
    /// The `-l`s and `-L`s in the rustflags of the crates it links in beside its own, each with
    /// its crate's package ([`linked_rustflag_libs`]): its link reads them, and its plan holds
    /// only its own rustflags (whose `-l`, `-L` and link options it covers).
    pub(crate) linked_rustflag_libs: Vec<(String, String)>,
    /// The code that makes the record ([`super::deps::record_makers`]): the front end's (its
    /// diagnostics, and the bodies the code generator compiles), the code generator's (the
    /// code it makes; the unit's target and flags, the rest of `Toolchain::id`, are in its
    /// plan) and that of blaze-cargo's closure (the lints, the linker, the files blaze-cargo
    /// writes and the record's encoding). Each moves on any edit of a package it is of, so a
    /// record made by other code is never found.
    pub(crate) makers: crate::serve::RecordMakers,
    /// [`keyed_goal`].
    pub(crate) goal: u8,
    pub(crate) color: bool,
    pub(crate) short: bool,
    pub(crate) verified: bool,
    pub(crate) reached: bool,
    /// The bytes of a `-C profile-use` profile.
    pub(crate) pgo: Option<ContentId>,
    /// The bytes of the profile its profile lints read ([`lint_profile`]).
    pub(crate) lint_profile: Option<ContentId>,
    /// The heat of the profiles its images are laid out by, and that their links write
    /// address maps ([`super::layout::key`]): none with no `layout` in the target directory.
    pub(crate) layout: Option<ContentId>,
    pub(crate) width: Option<u32>,
    pub(crate) refused: Option<u32>,
}

impl RecordInputs<'_> {
    /// The record's base key, of unit `unit` (for an error to name).
    pub(crate) fn key(&self, unit: usize) -> Result<Key, ApiError> {
        let mut hasher = Hasher::new(RECORD);
        hasher.id(&self.identity);
        // The unit, its plan and its run environment, each encoded into one buffer kept per
        // thread and hashed from it: no buffer allocated per unit once the first has grown it.
        SCRATCH.with_borrow_mut(|scratch| -> Result<(), ApiError> {
            hash_cbor(&mut hasher, scratch, &self.unit, "unit", unit)?;
            hash_cbor(&mut hasher, scratch, self.plan, "plan", unit)?;
            hash_cbor(&mut hasher, scratch, self.run_env, "run environment", unit)?;
            hash_cbor(
                &mut hasher,
                scratch,
                &self.script_check_cfgs,
                "build scripts' check-cfgs",
                unit,
            )?;
            hash_cbor(
                &mut hasher,
                scratch,
                &self.script_link_args,
                "build scripts' link arguments",
                unit,
            )?;
            hash_cbor(
                &mut hasher,
                scratch,
                &self.linked_rustflag_libs,
                "the rustflags' libraries and their directories of the crates it links",
                unit,
            )
        })?;
        hasher.id(&self.makers.front_end).id(&self.makers.codegen).id(&self.makers.record).update(
            &[
                self.goal,
                u8::from(self.color),
                u8::from(self.short),
                u8::from(self.verified),
                u8::from(self.reached),
            ],
        );
        if let Some(profile) = &self.pgo {
            hasher.id(profile);
        }
        // Tagged, so a lint profile is never read as a `-C profile-use` one.
        if let Some(profile) = &self.lint_profile {
            hasher.update(b"lint profile").id(profile);
        }
        if let Some(layout) = &self.layout {
            hasher.update(b"layout").id(layout);
        }
        // The rendered diagnostics' columns: none is rustc's default, apart from every width.
        hasher.update(&self.width.map_or(0, |it| u64::from(it) + 1).to_le_bytes());
        // How many refused functions its error names: every one apart from each count.
        hasher.update(&self.refused.map_or(0, |it| u64::from(it) + 1).to_le_bytes());
        Ok(Key(hasher.finish()))
    }

    /// The parts of [`Self::key`] by name, each hashed alone, for `blaze why`: the unit's own
    /// fields and each dependency by the name it is used as, so a changed feature or profile
    /// is named. Made on a miss only.
    pub(crate) fn parts(&self, unit: usize) -> Result<Vec<blaze_kernel::Part>, ApiError> {
        use blaze_kernel::Part;
        let bytes = |name: &str, bytes: &[u8]| {
            let mut hasher = Hasher::new(PART);
            hasher.part(bytes);
            Part::new(name, Some(hasher.finish()))
        };
        let cbor = |name: &str, value: &dyn erased::Encode| -> Result<Part, ApiError> {
            // alloc: once a miss, a part
            let mut scratch = Vec::new();
            value.encode(&mut scratch).map_err(|why| {
                ApiError::new(format!("encoding the {name} of unit {unit}: {why}"), None)
            })?;
            Ok(bytes(name, &scratch))
        };
        let id = |name: &str, id: Option<&ContentId>| Part::new(name, id.copied());
        let flag = |name: &str, on: bool| bytes(name, &[u8::from(on)]);
        let u = &self.unit;
        // alloc: once a miss, a part each part of the key
        let mut parts = vec![
            id(
                "the crate's identity in the front end (its sources and its dependencies')",
                Some(&self.identity),
            ),
            cbor("package", &u.pkg_id)?,
            cbor("target", u.target)?,
            cbor("profile", u.profile)?,
            cbor("platform", u.platform)?,
            flag("whether it is built to run as a wasm proc macro", u.macro_wasm),
            cbor("mode", &u.mode)?,
            cbor("features", &u.features)?,
            flag("whether it is std's", u.is_std),
            cbor("plan", self.plan)?,
            cbor("run environment", &self.run_env)?,
            cbor("the check-cfgs its build scripts printed", &self.script_check_cfgs)?,
            cbor("the link arguments its build scripts printed", &self.script_link_args)?,
            cbor(
                "the `-l`s and `-L`s in the rustflags of the crates it links",
                &self.linked_rustflag_libs,
            )?,
            id("the front end's code identity", Some(&self.makers.front_end)),
            id("the code generator's code identity", Some(&self.makers.codegen)),
            id(
                "the code identity of what makes a record (blaze-cargo's closure)",
                Some(&self.makers.record),
            ),
            bytes("goal", &[self.goal]),
            flag("color", self.color),
            flag("short diagnostics", self.short),
            flag("BLAZE_ELIDE=verify (built to verify an inert build script)", self.verified),
            flag("BLAZE_DEPS (only what programs reach)", self.reached),
            id("the -C profile-use profile", self.pgo.as_ref()),
            id("the lint profile", self.lint_profile.as_ref()),
            id("the layout profiles' heat", self.layout.as_ref()),
            bytes(
                "the diagnostics' width",
                &self.width.map_or(0, |it| u64::from(it) + 1).to_le_bytes(),
            ),
            bytes(
                "how many refused functions its error names",
                &self.refused.map_or(0, |it| u64::from(it) + 1).to_le_bytes(),
            ),
        ];
        for dep in &u.dependencies {
            parts.push(cbor(&format!("dependency `{}`", dep.extern_crate_name), dep)?);
        }
        Ok(parts)
    }
}

/// The domain of a part of a record key hashed alone ([`RecordInputs::parts`]).
const PART: &str = "blaze v2 why record part 1";

/// A value of any type the record key encodes, for [`RecordInputs::parts`] to take one at a
/// time.
mod erased {
    pub(super) trait Encode {
        fn encode(&self, out: &mut Vec<u8>) -> Result<(), blaze_protocol::cbor::Error>;
    }

    impl<T: serde::Serialize + ?Sized> Encode for T {
        fn encode(&self, out: &mut Vec<u8>) -> Result<(), blaze_protocol::cbor::Error> {
            blaze_protocol::cbor::append(out, self)
        }
    }
}

/// The goal as `unit`'s record key holds it. A build script's compile is the same program
/// whatever the job's goal ([`super::checks_only`] is false for it under every goal), so a
/// `blaze check` and a `blaze build` share its record; any other unit keys the goal.
pub(crate) fn keyed_goal(goal: Goal, unit: &Unit) -> u8 {
    if unit.mode == Mode::Build && unit.target.is_custom_build() {
        return 1;
    }
    match goal {
        Goal::Check => 0,
        Goal::Build => 1,
        Goal::Test => 2,
        Goal::Run => 3,
    }
}

/// Whether `unit` is a build script elision found inert, or one of std's own that blaze has a
/// port of, so neither built nor run: under `BLAZE_ELIDE=verify` an inert one is built and run
/// as well; std's never is ([`script::ported`]).
fn elided_unbuilt(job: &Job, unit: usize) -> Result<bool, ApiError> {
    let custom_build = job.unit(unit)?.target.is_custom_build();
    let unverified = script::ported(job, unit) || !script::verifies(job)?;
    Ok(custom_build && unverified && script::inert(job, unit)?.is_some())
}

/// The variable that builds every dependency whole: `full`, for the differential run against a
/// reachable-only build (`harness/v2/reachable.py`); unset, only what the build's programs reach.
pub(crate) const DEPS: &str = "BLAZE_DEPS";

/// Why a crate [`reached_only`] has no check outcome or bodies of its own in the store.
pub(crate) const REACHED_ONLY: &str = "only what the build's programs reach of it is type-checked and compiled (`BLAZE_DEPS=full` checks it whole)";

/// Whether only what the build's programs reach of `unit`'s crate is type-checked and compiled
/// (docs/v2/plan/lanes/cold-v2/notes.md): a crate of the standard library, or of a package
/// cargo caps the lints of (a registry or git dependency), unless the job's [`DEPS`] is `full`.
/// Such a library runs no check and compiles no roots of its own: a program's walk, from roots
/// as complete as rustc's collector's, compiles what it reaches of it and type-checks those
/// bodies on the way, and a body it reaches that does not type-check stops that walk with an
/// error naming it. A workspace member and a path package are checked and compiled whole, as
/// cargo shows their diagnostics. Any other value of [`DEPS`] is an error naming it.
pub(crate) fn reached_only(job: &Job, unit: &Unit) -> Result<bool, ApiError> {
    match job
        .input
        .config
        .invocation
        .env
        .iter()
        .find(|(name, _)| name == DEPS)
        .map(|(_, value)| value.as_str())
    {
        None => {}
        Some("full") => return Ok(false),
        Some(value) => {
            return Err(ApiError::new(
                // alloc: error path
                format!(
                    "{DEPS}={value} is not a value blaze takes: `full` checks and compiles every dependency whole, \
                     unset only what the build's programs reach of each"
                ),
                None,
            ));
        }
    }
    let package = job.package(unit)?;
    Ok(capped(unit.is_std, package.is_member, package.source.as_deref()))
}

/// The diagnostics of a crate [`reached_only`] with no check outcome in the store: its one check
/// of its own (`blaze_diagnostics::check_reached_only`), run whatever [`DEPS`] says. It has two
/// parts. Expanding every macro call and lowering every body: rustc does that for every crate it
/// compiles, so a call that fails to expand fails this crate's unit, naming the call, the body
/// and where it is, never a later crate's as "MIR that failed to lower" with this one built.
/// And the `#[blaze::sealed]` checks, when a crate of its graph seals a trait
/// (`hir_ty::sealed_in_graph`): sealing is the sealing crate's opt-in and binds every crate
/// that can name the trait, a registry crate rustc accepted included, and a registry crate that
/// seals is checked by nothing else (docs/v2/plan/lanes/frontend/sealed-orphan.md). Nothing is
/// type-checked; only what the build's programs reach is. std is checked like every other
/// crate.
pub(crate) fn reached_only_diags(
    db: &base_db::Db,
    krate: base_db::Crate,
    files: blaze_diagnostics::Files<'_>,
    options: impl FnOnce() -> Result<Options, ApiError>,
) -> Result<Vec<Diag>, ApiError> {
    let _span = tracing::info_span!(target: "blaze", "unit reached-only check", ?krate).entered();
    Ok(blaze_diagnostics::check_reached_only(db, hir::Crate::from(krate), files, &options()?))
}

/// Whether a unit's diagnostics are ones cargo never shows (lints capped, so only an error
/// could be seen): the standard library's, and any package that is neither a workspace member
/// nor at a path.
pub(crate) fn capped(is_std: bool, is_member: bool, source: Option<&str>) -> bool {
    is_std || !(is_member || local(source))
}

/// A unit as its record's key hashes it: each dependency by what it is, never by its index
/// in the graph, which another workspace's graph (or this one's after a dependency is added)
/// gives another number. What a dependency is made from is in the identity.
#[derive(Serialize)]
pub(crate) struct KeyedUnit<'a> {
    pkg_id: &'a str,
    target: &'a blaze_units::schema::Target,
    profile: &'a blaze_units::schema::Profile,
    platform: &'a Option<String>,
    /// [`Unit::macro_wasm`]: a `--target wasm32-wasip1` unit and a wasm macro's of one
    /// package differ by it alone. Left out when off, so every other unit's key is as it was.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    macro_wasm: bool,
    mode: blaze_units::schema::Mode,
    features: &'a [String],
    is_std: bool,
    dependencies: Vec<KeyedDep<'a>>,
}

#[derive(Serialize)]
struct KeyedDep<'a> {
    pkg_id: &'a str,
    target: &'a str,
    mode: blaze_units::schema::Mode,
    platform: &'a Option<String>,
    /// As [`KeyedUnit::macro_wasm`].
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    macro_wasm: bool,
    extern_crate_name: &'a str,
    public: Option<bool>,
    noprelude: Option<bool>,
    nounused: Option<bool>,
}

impl<'a> KeyedUnit<'a> {
    fn of(job: &'a Job, u: &'a Unit) -> Result<KeyedUnit<'a>, ApiError> {
        KeyedUnit::among(u, |at| job.unit(at))
    }

    /// `u` of a graph whose unit `at` is `unit_at(at)`.
    pub(crate) fn among(
        u: &'a Unit,
        unit_at: impl Fn(usize) -> Result<&'a Unit, ApiError>,
    ) -> Result<KeyedUnit<'a>, ApiError> {
        // alloc: once per unit's key, a dependency each
        let mut dependencies = Vec::with_capacity(u.dependencies.len());
        for dep in &u.dependencies {
            let on = unit_at(dep.index)?;
            dependencies.push(KeyedDep {
                pkg_id: &on.pkg_id,
                target: &on.target.name,
                mode: on.mode,
                platform: &on.platform,
                macro_wasm: on.macro_wasm,
                extern_crate_name: &dep.extern_crate_name,
                public: dep.public,
                noprelude: dep.noprelude,
                nounused: dep.nounused,
            });
        }
        Ok(KeyedUnit {
            pkg_id: &u.pkg_id,
            target: &u.target,
            profile: &u.profile,
            platform: &u.platform,
            macro_wasm: u.macro_wasm,
            mode: u.mode,
            features: &u.features,
            is_std: u.is_std,
            dependencies,
        })
    }
}

fn hash_cbor<T: Serialize + ?Sized>(
    hasher: &mut Hasher,
    scratch: &mut Vec<u8>,
    value: &T,
    what: &str,
    unit: usize,
) -> Result<(), ApiError> {
    scratch.clear();
    blaze_protocol::cbor::append(scratch, value)
        .map_err(|why| ApiError::new(format!("encoding the {what} of unit {unit}: {why}"), None))?;
    hasher.part(scratch);
    Ok(())
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The machine's files as the process's table of them by `stat` hashes them: one `stat` for a
/// file hashed before, read only when its `stat` changed.
fn machine_file(
    disk: &mut blaze_usercode::inputs::Disk,
) -> impl FnMut(&std::path::Path) -> Option<[u8; 32]> + '_ {
    move |path| blaze_usercode::trace::Inputs::file(disk, path).ok().flatten()
}

/// Whether nothing of `unit` will be made again: its record and all it wrote are in the store,
/// and every native input it linked is as it was. Read in place: no record decoded, and no
/// allocation for a record whose code read no process variable. A store it can't read says
/// not fresh, and the unit's compile, looking again, fails naming why ([`kept`]).
pub(crate) fn fresh(job: &Job, unit: usize) -> bool {
    let Ok(Some(base)) = record_key(job, unit) else { return false };
    let guard = blaze_cas::enter();
    let found_in = |store: &blaze_cas::Reader, std: bool| {
        matches!(filed_in(job, store, std, &guard, base), Ok(Some(_)))
    };
    found_in(&job.input.store, false) || (shared(job, unit) && found_in(&job.input.std, true))
}

/// The store a record is read from, named for an error: the daemon's, or with `std` the shared
/// std store.
fn store_named(job: &Job, std: bool) -> String {
    match std {
        // alloc: error path
        true => format!("the shared std store ({})", job.input.config.std),
        false => "the daemon's store".to_owned(), // alloc: error path
    }
}

/// The record of base key `base` in `store`, filed under `base` or under what its code read
/// with the values it has now ([`reads::find`]): a variable's in the job's environment, a
/// file's outside every source root as it is on disk, a proc-macro expansion's worked out
/// again from its names. With where it was found and where
/// its CBOR body starts. An error names the slot that does not decode or whose reads can't be
/// worked out, or the key and the store (`std`: the shared std store) a read of failed: a store
/// that can't be read is never a record not kept (rule 3).
fn filed_in<'g>(
    job: &Job,
    store: &'g blaze_cas::Reader,
    std: bool,
    guard: &'g blaze_cas::Guard,
    base: Key,
) -> Result<Option<(reads::Found, blaze_cas::Entry<'g>, usize)>, ApiError> {
    let mut hit = None;
    // The first read that failed, with its key; `find` goes on with it as missing, and its
    // answer is not used.
    let failed: std::cell::Cell<Option<(Key, blaze_cas::Corrupt)>> = std::cell::Cell::new(None);
    let fail = |key: &Key, error: blaze_cas::Corrupt| {
        let first = failed.take();
        failed.set(first.or(Some((*key, error))));
    };
    let at = reads::find(
        base,
        &job.input.config.invocation.env,
        &|manifest| job.units().is_primary_manifest(manifest),
        |env, shape| job.input.macros.keeps().reads_now(env, shape),
        |path| base_db::IncludedFile::content_now(std::path::Path::new(path)),
        |slot| match store.get(guard, slot) {
            Ok(it) => it.map(|it| it.bytes),
            Err(error) => {
                fail(slot, error);
                None
            }
        },
        |key| match found(store, guard, key) {
            Ok(it) => {
                hit = it;
                hit.is_some()
            }
            Err(error) => {
                fail(key, error);
                false
            }
        },
    );
    if let Some((key, error)) = failed.take() {
        // alloc: error path
        let what = format!(
            "reading {:x} (of record key {:x}) from {}: {error}",
            key.0,
            base.0,
            store_named(job, std)
        );
        return Err(ApiError::new(what, None));
    }
    let at = at.map_err(|why| ApiError::new(why, None))?;
    Ok(at.zip(hit).map(|(at, (entry, body))| (at, entry, body)))
}

/// Whether `unit` is the standard library's, whose records are also in the shared std store
/// ([`crate::serve::work::JobInput::std`]).
fn shared(job: &Job, unit: usize) -> bool {
    job.unit(unit).is_ok_and(|it| it.is_std)
}

/// The record of `key` in `store`, every output it names in `store` and every native input
/// as it was, and where its CBOR body starts. A read of the store that fails is the error.
fn found<'g>(
    store: &'g blaze_cas::Reader,
    guard: &'g blaze_cas::Guard,
    key: &Key,
) -> Result<Option<(blaze_cas::Entry<'g>, usize)>, blaze_cas::Corrupt> {
    let Some(entry) = store.get(guard, key)? else { return Ok(None) };
    let mut disk = blaze_usercode::inputs::Disk::new(Vec::new());
    let body =
        header::check(entry.bytes, |key| store.has(guard, key), &mut machine_file(&mut disk));
    Ok(body.map(|body| (entry, body)))
}

/// What `unit`'s last build made, as its record keeps it: none when the view's units were never
/// built, or were built from other code.
pub(crate) fn last_made(job: &Job, unit: usize) -> Result<Option<Made>, ApiError> {
    Ok(kept(job, unit)?.map(|it| it.record.made))
}

/// Every instance the view's last build compiled or found, with the code hash it was made
/// from, sorted by key: each unit's from its record. A unit with no record, never built or
/// built from other code, is an error naming it.
pub(crate) fn codegen_inputs(job: &Job) -> Result<Vec<(ContentId, ContentId)>, Failed> {
    let mut inputs = Vec::new();
    for unit in 0..job.graph.len() {
        if job.graph.has(unit as u32, graph::flag::RUN) {
            continue;
        }
        if job.cancelled() {
            return Err(Failed::Again);
        }
        let Some(made) = last_made(job, unit)? else {
            let name = &job.unit(unit)?.target.name;
            return Err(ApiError::new(
                format!("`{name}` has no build of this view to list the codegen inputs of"),
                None,
            )
            .into());
        };
        inputs.extend(made.inputs);
    }
    inputs.sort_unstable();
    inputs.dedup();
    Ok(inputs)
}

/// What the build `unit`'s record is of did with each profile it used ([`header::profiles`]),
/// read from its header in place: [`Recorded::Missing`], naming the unit and its record key,
/// when the store has no record of it or the key is not known yet. The standard library's
/// units are looked for in the shared std store too.
pub(crate) fn recorded_profiles(job: &Job, unit: usize) -> Result<Recorded, ApiError> {
    let name = || job.unit(unit).map_or("", |it| it.target.name.as_str());
    let Some(base) = record_key(job, unit)? else {
        // alloc: an error
        return Ok(Recorded::Missing(format!(
            "unit {unit} (`{}`): its record key is not known",
            name()
        )));
    };
    let guard = blaze_cas::enter();
    let read = |store: &blaze_cas::Reader, std: bool| -> Result<Option<Vec<_>>, ApiError> {
        let Some((_, entry, _)) = filed_in(job, store, std, &guard, base)? else { return Ok(None) };
        recorded_in(job, unit, base, entry.bytes).map(Some)
    };
    let found = match read(&job.input.store, false)? {
        Some(profiles) => Some(profiles),
        None if shared(job, unit) => read(&job.input.std, true)?,
        None => None,
    };
    Ok(match found {
        Some(profiles) => Recorded::Read(profiles),
        // alloc: an error
        None => Recorded::Missing(format!(
            "unit {unit} (`{}`): the store has no record under its key {:x}",
            name(),
            base.0
        )),
    })
}

/// The profiles `unit`'s record under `base`, `bytes`, says its build used ([`header::profiles`]);
/// a header that does not read is an error naming the unit and the key.
fn recorded_in(
    job: &Job,
    unit: usize,
    base: Key,
    bytes: &[u8],
) -> Result<Vec<(ContentId, ProfileCounts)>, ApiError> {
    header::profiles(bytes).ok_or_else(|| {
        let name = job.unit(unit).map_or("", |it| it.target.name.as_str());
        // alloc: an error
        ApiError::new(
            format!(
                "unit {unit} (`{name}`): its record under {:x} has a header that does not read",
                base.0
            ),
            None,
        )
    })
}

/// A unit's record, found ([`kept`]).
struct Kept {
    record: Record,
    /// What copies it from the shared std store into the daemon's: none for the daemon's own.
    puts: Vec<Put>,
    /// What its build did with each profile, from its header ([`header::profiles`]).
    profiles: Vec<(ContentId, ProfileCounts)>,
}

/// The record of `unit` the store has, every output and native input of it as it was. A
/// standard library unit's the store has not is the shared std store's, another daemon's,
/// with the puts that copy it, the slot of what it read and every output it names into the
/// store. A record key that can't be worked out, or a slot that does not decode, is an error.
fn kept(job: &Job, unit: usize) -> Result<Option<Kept>, ApiError> {
    let Some(base) = record_key(job, unit)? else { return Ok(None) };
    let guard = blaze_cas::enter();
    // A record that is there and does not decode is an error naming the unit, the key and the
    // store, never read as one not kept (rule 3): a record format change is the key's to cover.
    // The unit's name and package, or, when it can't be looked up, its index and why: the
    // key, the store and the error are said either way.
    let named = || match job.unit(unit) {
        Ok(u) => (
            format!("unit {unit} (`{}` of {})", u.target.name, u.pkg_id),
            Some(u.pkg_id.to_string()),
        ), // alloc: error path
        Err(it) => (format!("unit {unit} (its name unread: {})", it.what), None), // alloc: error path
    };
    let undecoded = |store: &str, error: String| {
        let (what, pkg) = named();
        // alloc: error path
        ApiError::new(
            format!("{what}: its record under {:x} in {store} does not decode: {error}", base.0),
            pkg,
        )
    };
    if let Some((_, entry, body)) = filed_in(job, &job.input.store, false, &guard, base)? {
        let record = blaze_protocol::cbor::from_slice(&entry.bytes[body..])
            .map_err(|error| undecoded("the daemon's store", error.to_string()))?;
        let profiles = recorded_in(job, unit, base, entry.bytes)?;
        // alloc: none
        return Ok(Some(Kept { record, puts: Vec::new(), profiles }));
    }
    if !shared(job, unit) {
        return Ok(None);
    }
    let std = &job.input.std;
    let in_std = || store_named(job, true);
    let Some((at, entry, body)) = filed_in(job, std, true, &guard, base)? else { return Ok(None) };
    let record: Record = blaze_protocol::cbor::from_slice(&entry.bytes[body..])
        .map_err(|error| undecoded(&in_std(), error.to_string()))?;
    let profiles = recorded_in(job, unit, base, entry.bytes)?;
    let mut puts = Vec::with_capacity(record.outputs.len() + 2); // alloc: once per std unit found shared
    let copied = at.slot.iter().chain(record.outputs.iter().map(|(_, output, _)| output));
    for key in copied {
        let read = std.get(&guard, key).map_err(|error| {
            let (what, pkg) = named();
            // alloc: error path
            ApiError::new(
                format!("{what}: reading its output {:x} from {}: {error}", key.0, in_std()),
                pkg,
            )
        })?;
        let Some(it) = read else { return Ok(None) };
        // alloc: a copy of what another daemon made, once per output, for the store to own
        puts.push(Put { key: *key, kind: it.kind, bytes: it.bytes.to_vec() });
    }
    // alloc: the record's copy, once per std unit found shared
    puts.push(Put { key: at.key, kind: entry.kind, bytes: entry.bytes.to_vec() });
    Ok(Some(Kept { record, puts, profiles }))
}

/// What std unit `unit` made, for the shared std store: its record `record` and every output
/// it names, from `compiled`'s puts or the store.
fn share(job: &Job, unit: usize, compiled: &Compiled, record: &Put) -> Result<Vec<Put>, ApiError> {
    let guard = blaze_cas::enter();
    let mut share = Vec::with_capacity(compiled.outputs.len() + 1); // alloc: once per std unit made
    for output in &compiled.outputs {
        let put = match compiled.puts.iter().find(|it| it.key == output.key) {
            // alloc: a copy for the shared store, once per output of a std unit made
            Some(put) => Put { key: put.key, kind: put.kind, bytes: put.bytes.clone() },
            None => match job.input.store.get(&guard, &output.key) {
                // alloc: a copy for the shared store, once per output of a std unit made
                Ok(Some(it)) => Put { key: output.key, kind: it.kind, bytes: it.bytes.to_vec() },
                Ok(None) => {
                    // alloc: an error, once
                    let what = format!(
                        "unit {unit}'s output {} is neither made now nor in the store",
                        output.path
                    );
                    return Err(ApiError::new(what, None));
                }
                Err(corrupt) => {
                    return Err(ApiError::new(
                        format!("unit {unit}'s output {}: {corrupt}", output.path),
                        None,
                    ));
                }
            },
        };
        share.push(put);
    }
    // alloc: the record's copy, once per std unit made
    share.push(Put { key: record.key, kind: record.kind, bytes: record.bytes.clone() });
    Ok(share)
}

/// What a unit's phases made so far, for the phases after them ([`Phase`]), under the unit's
/// lock in [`Job::phases`]. A phase takes what it read only once nothing of it can give way,
/// so a phase posted again finds the unit as it was.
#[derive(Default)]
pub(crate) struct Phases {
    /// Its compile replayed from its record: the phases after [`Phase::Names`] make nothing.
    kept: Option<Compiled>,
    /// What [`Phase::Names`] found the unit does.
    does: Option<Does>,
    /// The check of every body of its crate, when one runs: [`Phase::Bodies`] runs it, and
    /// [`Phase::Codegen`] beside it is told what it reaches first and whether it failed
    /// ([`super::checks`]).
    checking: Option<Arc<Checking>>,
    /// The check's diagnostics, or those of the check served for it.
    diags: Option<Vec<Diag>>,
    /// The check's errors break the crate's items
    /// ([`blaze_diagnostics::CrateCheck::items_broken`]): a unit that fails with them fails
    /// shut, its dependents unchecked; one that fails without them fails open
    /// ([`Compiled::open`]).
    items_broken: bool,
    /// The text of each file `diags` point into that no mount of the build holds, by its name
    /// in them ([`unmounted`]).
    unmounted: Vec<(String, String)>,
    /// What [`Phase::Codegen`] gave; none when the unit compiles no code.
    generated: Option<Generated>,
    /// The unit as [`Phase::Link`] left it, for [`Phase::Store`].
    linked: Option<Linked>,
    /// Why its record and the templates its codegen missed were not in the store, for
    /// `blaze why` ([`Compiled::misses`]).
    misses: Vec<blaze_kernel::Miss>,
    /// What [`Phase::Codegen`] made of a `Doctest` unit's examples and runner ([`doctest`]).
    doctests: Option<doctest::Generated>,
}

/// What a unit does, as [`Phase::Names`] finds it.
#[derive(Clone, Copy)]
struct Does {
    /// A build script found to do nothing: neither built nor run.
    inert: bool,
    /// It compiles code of its own: it is no check, and not a library compiled only as a
    /// program reaches it.
    builds_code: bool,
}

/// What codegen gave: what the unit makes and the code generator's result, or why the unit
/// makes nothing codegen can say.
type Generated = Result<(Makes, Result<Result<codegen::Parts, codegen::Refused>, String>), String>;

/// A unit as [`Phase::Link`] left it.
enum Linked {
    /// Replayed from its record, a proc macro's expanders loaded.
    Kept(Compiled),
    // alloc: once per unit made
    Made(Box<Making>),
}

/// What a unit made, before its outputs are hashed for the store.
struct Making {
    diagnostics: Vec<Diagnostic>,
    ok: bool,
    /// [`Compiled::open`].
    open: bool,
    /// [`Compiled::why`], where the unit's own errors failed it.
    why: Option<&'static str>,
    made: Made,
    /// No file at all: it failed, or it is a build script that does nothing.
    nothing: bool,
    /// Each file rustc would write: its path, what it holds, and whether it is executable.
    files: Vec<(String, Written, bool)>,
    /// What codegen put, the outputs to follow.
    puts: Vec<Put>,
    natives: Vec<(String, ContentId)>,
    links: Vec<(String, Linking)>,
    macros: Option<ProcMacroLoadResult>,
    /// The records of images keyed by all they are made from before they are built (a
    /// doctest's examples), each filed once its output is keyed ([`doctest`]).
    records: Vec<ImageRecord>,
}

/// What a unit writes at one of its paths ([`Making::files`]).
pub(crate) enum Written {
    /// These bytes, which the store is given.
    Bytes(Vec<u8>),
    /// The bytes the store keeps under this key, as the path's last build of the same inputs
    /// left them: an image nothing it is made from changed since ([`Linking::Unchanged`]).
    Kept(Key),
}

/// The record of an image keyed before it is built: under `filing`'s key, its output at
/// `path` and the native inputs `natives` its link read, as a unit record's header has them
/// ([`header`]), so a build of the same inputs finds the image made ([`doctest`]).
pub(crate) struct ImageRecord {
    pub(crate) path: String,
    pub(crate) filing: reads::Filing,
    pub(crate) natives: Vec<(String, ContentId)>,
}

/// Phase `phase` of `unit`'s compile, reading the front end through `reader`; the compile
/// itself once its last, [`Phase::Store`], is done. `images` are the kept images
/// [`Phase::Link`] relinks. A proc macro's compile gives its expanders ([`Compiled::macros`]).
pub(crate) fn compile(
    job: &Job,
    reader: &UnitReader,
    unit: usize,
    phase: Phase,
    images: &mut Vec<Image>,
) -> Result<Option<Compiled>, Failed> {
    let Some(slot) = job.phases.get(unit) else {
        // alloc: error path
        return Err(ApiError::new(format!("unit {unit} has no phases in the job"), None).into());
    };
    let of = job.units().graph.units.get(unit);
    let krate = Name::of(of.map_or("", |it| it.target.name.as_str()));
    // alloc: once per phase of a unit, the name its span carries
    let unit_name = Name::of(&of.map(span_unit).unwrap_or_default());
    let _span = match phase {
        Phase::Names => span::UnitNames { krate, unit: unit_name }.open(),
        Phase::Bodies => span::UnitBodies { krate, unit: unit_name }.open(),
        Phase::Codegen => span::UnitCodegen { krate, unit: unit_name }.open(),
        Phase::Link => span::UnitLink { krate, unit: unit_name }.open(),
        Phase::Store => span::UnitStore { krate, unit: unit_name }.open(),
    };
    match phase {
        Phase::Names => names(job, reader, unit, slot)?,
        Phase::Bodies => bodies(job, reader, unit, slot)?,
        Phase::Codegen => generate(job, reader, unit, slot)?,
        Phase::Link => link(job, reader, unit, &mut lock(slot), images)?,
        Phase::Store => return store(job, reader, unit, &mut lock(slot)).map(Some),
    }
    Ok(None)
}

/// A unit as its phase spans name it in `unit`: its package, the package's version and its
/// target, `serde v1.0.219 (build script)`, `blaze-api v0.1.0 (lib test)`. Every build script's
/// target is `build-script-build`, so the target's name alone (the spans' `crate`) can't tell
/// two apart.
pub(crate) fn span_unit(unit: &Unit) -> String {
    let package = crate::cli::say::package_name(&unit.pkg_id);
    let version = crate::cli::say::package_version(&unit.pkg_id);
    let target = &unit.target;
    let kind = target.kind.first().map_or("", String::as_str);
    let named: std::borrow::Cow<'_, str> = match kind {
        _ if target.is_custom_build() && unit.mode == Mode::RunCustomBuild => {
            "build script run".into()
        }
        _ if target.is_custom_build() => "build script".into(),
        _ if target.is_lib() => "lib".into(),
        kind => format!("{kind} \"{}\"", target.name).into(), // alloc: once per phase of a unit
    };
    // As cargo names a unit: a test or bench target's test is just the target.
    let test =
        (unit.mode == Mode::Test || unit.checked_as_test) && !matches!(kind, "test" | "bench");
    let suffix = match () {
        _ if unit.mode == Mode::Doctest => " doctest",
        _ if test => " test",
        _ => "",
    };
    format!("{package} v{version} ({named}{suffix})") // alloc: once per phase of a unit
}

/// A unit's phases, each holding the lock while it changes them. Only [`Phase::Bodies`] and
/// [`Phase::Codegen`], side by side, ever wait on it, each only to read or leave a field.
fn lock(slot: &Lock<Phases>) -> LockGuard<'_, Phases> {
    // A phase that panicked took nothing it read: it changes the unit's only once it is done.
    slot.lock()
}

/// [`Phase::Names`]: the unit's record, replayed when the store keeps it; otherwise its
/// lint flags, its crate's def map, what it does, and its check: served from the store, none
/// (what a program reaches of the crate is type-checked by its walk), or of every body.
fn names(job: &Job, reader: &UnitReader, unit: usize, slot: &Lock<Phases>) -> Result<(), Failed> {
    let kept = {
        let _span = tracing::info_span!("unit record lookup").entered();
        kept(job, unit)?
    };
    if kept.is_none() {
        let miss = missed(job, unit);
        lock(slot).misses.push(miss);
    }
    if let Some(Kept { record, puts, profiles }) = kept {
        // What its build did with its profiles, as the record says: counted for this job's
        // check that a profile applied to something ([`codegen::unapplied`]).
        if let Some(slot) = job.counted.get(unit) {
            let _ = slot.set(profiles.into_boxed_slice());
        }
        let compiled = Compiled {
            diagnostics: record.diagnostics,
            ok: record.ok,
            open: record.open,
            why: record.why,
            puts,
            share: Vec::new(),
            outputs: record
                .outputs
                .into_iter()
                .map(|(path, key, executable)| Output { path, key, executable })
                .collect(),
            artifact: record.artifact.map(|it| Artifact { fresh: true, ..it }),
            macros: None,
            links: Vec::new(),
            // alloc: none
            misses: Vec::new(),
        };
        if let Some(made) = job.made.get(unit) {
            let _ = made.set(record.made);
        }
        lock(slot).kept = Some(compiled);
        return Ok(());
    }
    let u = job.unit(unit)?;
    if u.mode == Mode::Doctest {
        return doctest::names(job, unit, slot);
    }
    let krate = crate_of(job, unit, u)?;
    let db = reader.db();
    macros_built(db, krate, u)?;
    options(job, db, unit, u)?;
    {
        let _span = tracing::info_span!("unit def map").entered();
        let _ = hir_def::nameres::crate_def_map(db, krate);
    }
    let elided = {
        let _span = tracing::info_span!("build script inert").entered();
        elided_unbuilt(job, unit)?
    };
    // Its span, named for the script, is opened where a script is proved (`unbuilt`): a unit
    // that is no build script's compile does nothing here.
    let inert = elided || script::pure::unbuilt(job, reader, unit)?;
    let reached = reached_only(job, u)?;
    // A library compiled only as a program reaches it has no code of its own to build. A proc
    // macro built for wasm is a program, its module ([`macros`] loads it); any other proc
    // macro's lib has none that is loaded: its expanders run its MIR.
    let makes = codegen::makes(u);
    let library_reached = reached && makes.as_ref().is_ok_and(|it| *it == Makes::Library);
    let proc_macro = makes.is_ok_and(|it| it == Makes::Shared { proc_macro: true })
        && !codegen::wasm_macro(job, unit);
    let builds_code = !inert && !checks_only(job.input.goal, u) && !library_reached && !proc_macro;
    let served = {
        let _span = tracing::info_span!("unit check served").entered();
        blaze_diagnostics::cache::read(db, reader.checks(), krate, job.mounted.root())
    };
    let _span = tracing::info_span!("unit check setup").entered();
    let (checking, diags, items_broken) = match served {
        Some(Err(why)) => return Err(ApiError::new(why, Some(u.target.name.clone())).into()),
        // A dependency's check outcome from the store: no check runs, and codegen reads the
        // bodies' records served with it.
        Some(Ok(outcome)) => (None, Some(outcome.diags), outcome.items_broken),
        // A build script neither built nor run (an elision found inert, std's own that blaze
        // has a port of, one proved pure) compiles nothing, so nothing of it is checked.
        None if reached && inert => (None, Some(Vec::new()), false), // alloc: empty, allocates nothing
        // Of a crate checked only where its programs reach it, the one check of its own. Its
        // errors are not told apart: any of them breaks its items.
        None if reached => {
            let files = blaze_diagnostics::Files::new(reader.paths(), job.mounted.root());
            (
                None,
                Some(reached_only_diags(db, krate, files, || Ok(options(job, db, unit, u)?.1))?),
                true,
            )
        }
        // alloc: once per unit checked, shared by its check and its codegen
        None => (Some(Arc::new(Checking::new(db, hir::Crate::from(krate)))), None, false),
    };
    let unmounted =
        diags.as_deref().map_or_else(Vec::new, |diags| unmounted(reader, &job.mounted, diags));
    let mut phases = lock(slot);
    phases.does = Some(Does { inert, builds_code });
    (phases.checking, phases.diags, phases.unmounted, phases.items_broken) =
        (checking, diags, unmounted, items_broken);
    Ok(())
}

/// The text of each file `diags` point into that no mount of the build holds (a build script's
/// output under `OUT_DIR`, read by an `include!`, a file outside every source root an
/// `include!` read), or that is not the file at its path (a crate root with a test harness or
/// a wasm macro's glue appended, named with its text's identity,
/// [`blaze_diagnostics::emit::names_text`]), as the database has it: the text their spans were
/// made against, which [`assemble`], with no database, can't read through `mounted`. Held is a
/// mount's file there, not a path under a mount's directory: `target/` is under the
/// workspace's, and the view has none of its outputs. A file the database has no text of is
/// left out, and [`assemble`] names it as unread.
pub(super) fn unmounted(reader: &UnitReader, mounted: &Mounted, diags: &[Diag]) -> Vec<(String, String)> {
    let db = reader.db();
    let names: Vec<&str> = blaze_diagnostics::emit::Sources::default()
        .unread(diags)
        .into_iter()
        // A mount's bytes that fail their check are left to `add_sources`, which adds them unread.
        .filter(|name| {
            blaze_diagnostics::emit::names_text(name)
                || matches!(mounted.read(&mounted.absolute(name)), Ok(None))
        })
        .collect(); // alloc: the few names of a unit's diagnostics no mount holds or that are no file of one
    if names.is_empty() {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    reader
        .files_named(mounted.root(), &names)
        .into_iter()
        // alloc: a file's text, once per file of a unit's diagnostics no mount holds
        .map(|(name, file)| (name, db.file_text(file).text(db).to_string()))
        .collect()
}

/// [`Phase::Bodies`]: the check of every body of the unit's crate, when one runs, its outcome
/// kept to be stored as a dependency crate's.
fn bodies(job: &Job, reader: &UnitReader, unit: usize, slot: &Lock<Phases>) -> Result<(), Failed> {
    // clone: a refcount, once per unit checked
    let Some(checking) = lock(slot).checking.clone() else { return Ok(()) };
    let u = job.unit(unit)?;
    let krate = crate_of(job, unit, u)?;
    let db = reader.db();
    let (flags, options) = options(job, db, unit, u)?;
    let files = blaze_diagnostics::Files::new(reader.paths(), job.mounted.root());
    let blaze_diagnostics::CrateCheck { diags, items_broken } = checking.check(db, files, &options);
    if hir_def::deps::eligible(db, krate) {
        let paths = blaze_diagnostics::cache::Paths::of(db, krate);
        let root = job.mounted.root();
        // alloc: the outcome encoded once more per dependency crate checked, for its report line
        let refused = blaze_diagnostics::cache::encode(&diags, items_broken, &paths, root)
            .err()
            .map(|it| it.to_string());
        // clone: once per dependency crate checked this build
        job.check_later(krate, flags, diags.clone(), items_broken, refused);
    }
    let plan = job.plan(unit)?;
    let diags = match &plan.clippy {
        Some(clippy) => {
            let profile = lint_profile(job, unit)?;
            linted(
                db,
                krate,
                reader,
                job.mounted.root(),
                u,
                plan,
                clippy,
                options.lints,
                profile,
                diags,
            )?
        }
        None => diags,
    };
    let unmounted = unmounted(reader, &job.mounted, &diags);
    let mut phases = lock(slot);
    (phases.diags, phases.unmounted, phases.items_broken) = (Some(diags), unmounted, items_broken);
    Ok(())
}

/// `diags`, the check's, with what clippy's lints and blaze's find in the unit's crate under
/// `cargo clippy`, at the levels of its lint flags and attributes, before rustc's closing lines,
/// which are made again to count them. `dead_code` is left out: `blaze::dead` says it once
/// the job's units end, over every configuration of the package ([`super::configs`]), so an
/// item another configuration uses is never called dead here. What drops it is the unit's own
/// plan (it is linted), which its record's key covers.
#[expect(
    clippy::too_many_arguments,
    reason = "the crate, where its files are named from, its plan and the check's"
)]
fn linted(
    db: &base_db::Db,
    krate: base_db::Crate,
    reader: &UnitReader,
    root: &str,
    u: &Unit,
    plan: &blaze_units::query::UnitPlan,
    clippy: &blaze_units::query::Clippy,
    command_line: CommandLine,
    profile: Option<blaze_lints::profile::Loaded>,
    mut diags: Vec<Diag>,
) -> Result<Vec<Diag>, ApiError> {
    let at = || Some(format!("clippy's lints of `{}`", u.target.name));
    let rust_version = plan
        .env
        .iter()
        .find(|(name, _)| name == "CARGO_PKG_RUST_VERSION")
        .map(|(_, it)| it.as_str());
    let file = clippy.conf.clone().map(|(path, text)| (std::path::PathBuf::from(path), text)); // clone: the file's text, once per unit linted
    let mut conf = blaze_lints::conf::Conf::of_text(file, rust_version.filter(|it| !it.is_empty()))
        .map_err(|why| ApiError::new(why, at()))?;
    conf.profile = profile;
    let paths = reader.paths();
    // alloc: a file's name, once per file of the crate and per span that leaves its file
    let name_of = |file: base_db::FileId| match paths.get(&file) {
        // As the check names it: a root with an appended module by its path and its text.
        Some(path) => Some(blaze_diagnostics::Files::of_text(
            db,
            file,
            blaze_diagnostics::Files::relative(path, root).to_owned(),
        )),
        // A file `include!` read from outside every source root, by its path there.
        None => db
            .files()
            .included_source_path(file)
            .map(|path| blaze_diagnostics::Files::relative(path.as_str(), root).to_owned()),
    };
    let setup = blaze_lints::Setup { command_line, conf };
    let found = blaze_lints::check(db, hir::Crate::from(krate), &setup, &name_of)
        .map_err(|why| ApiError::new(why.to_string(), at()))?;
    let end = diags.iter().rposition(|it| !closing(it)).map_or(0, |at| at + 1);
    diags.truncate(end);
    diags.retain(|it| it.lint_name() != Some("dead_code"));
    diags.extend(found.into_iter().map(|it| it.diagnostic));
    let closing = blaze_diagnostics::emit::summary(diags.iter());
    diags.extend(closing);
    Ok(diags)
}

/// The profiles a job's profile lints read, each by its settings, read the first time a unit
/// asks: every unit's settings, each profile read once.
pub(crate) struct LintProfiles(Box<[(LintSettings, Result<blaze_lints::profile::Loaded, String>)]>);

type LintSettings = blaze_lints::profile::Settings;

/// The settings of the profile `plan`'s lints read: `clippy.toml`'s keys over
/// `[workspace.metadata.blaze.lints]`'s; none when neither names a profile or the unit is not
/// linted.
fn lint_settings(plan: &blaze_units::query::UnitPlan) -> Result<Option<LintSettings>, String> {
    let Some(clippy) = &plan.clippy else { return Ok(None) };
    // clone: the file's text, once per unit linted
    let file = clippy.conf.clone().map(|(path, text)| (std::path::PathBuf::from(path), text));
    let conf = blaze_lints::conf::Conf::of_text(file, None)?;
    let workspace = clippy.profile.as_ref().map(|it| blaze_lints::profile::Workspace {
        path: it.path.as_deref(),
        hot_percent: it.hot_percent.as_deref(),
        cold_percent: it.cold_percent.as_deref(),
        manifest: &it.manifest,
    });
    conf.profile_settings(workspace.as_ref())
}

fn lint_profiles(job: &Job) -> &LintProfiles {
    job.lint_profiles.get_or_init(|| {
        // alloc: once per job, one entry per profile its units' lints name
        let mut read: Vec<(LintSettings, Result<blaze_lints::profile::Loaded, String>)> =
            Vec::new();
        for plan in &job.units().plans {
            if let Ok(Some(settings)) = lint_settings(plan)
                && !read.iter().any(|it| it.0 == settings)
            {
                // clone: once per job and profile
                read.push((settings.clone(), blaze_lints::profile::load(settings)));
            }
        }
        LintProfiles(read.into_boxed_slice())
    })
}

/// The profile `unit`'s profile lints read, if its configuration names one; an error naming
/// the unit and the profile's path when it does not read.
pub(crate) fn lint_profile(
    job: &Job,
    unit: usize,
) -> Result<Option<blaze_lints::profile::Loaded>, ApiError> {
    let u = job.unit(unit)?;
    let at = || Some(format!("the profile lints of `{}`", u.target.name));
    let Some(settings) = lint_settings(job.plan(unit)?).map_err(|why| ApiError::new(why, at()))?
    else {
        return Ok(None);
    };
    match lint_profiles(job).0.iter().find(|it| it.0 == settings) {
        // clone: the settings and a refcount, once per unit linted with a profile
        Some((_, Ok(loaded))) => Ok(Some(loaded.clone())),
        // clone: an error, once per unit it fails
        Some((_, Err(why))) => Err(ApiError::new(why.clone(), at())),
        None => Err(ApiError::new(
            format!(
                "the lint profile `{}` was not read for the job (`lint_profiles`)",
                settings.path.display()
            ),
            at(),
        )),
    }
}

/// Whether `diag` is one of rustc's closing lines after a crate's diagnostics
/// ([`blaze_diagnostics::emit::summary`]'s).
fn closing(diag: &Diag) -> bool {
    diag.code.is_none()
        && diag.span.primary.is_empty()
        && (diag.level == blaze_diagnostics::model::Level::FailureNote
            || cargo_hides(&diag.message))
}

/// [`Phase::Codegen`]: the unit's functions compiled from its roots, when it compiles code,
/// told by its check beside it what fails.
fn generate(
    job: &Job,
    reader: &UnitReader,
    unit: usize,
    slot: &Lock<Phases>,
) -> Result<(), Failed> {
    let (does, checking) = {
        let phases = lock(slot);
        // clone: a refcount, once per unit compiled
        (phases.does, phases.checking.clone())
    };
    let Some(Does { builds_code: true, .. }) = does else { return Ok(()) };
    let u = job.unit(unit)?;
    if u.mode == Mode::Doctest {
        return doctest::generate(job, reader, unit, slot);
    }
    let krate = crate_of(job, unit, u)?;
    // dyn: `HirFrontend::with_checks` takes it; called once a level of the walk
    let checks = checking.as_deref().map(|it| it as &dyn BodyChecks);
    let generated = codegen::makes(u).map(|makes| {
        let generated = codegen::generate(job, reader, unit, krate, &makes, &job.mounted, checks);
        (makes, generated)
    });
    // Its build may have stopped for the cancel, with an error that is not the unit's.
    if job.cancelled() {
        return Err(Failed::Again);
    }
    lock(slot).generated = Some(generated);
    Ok(())
}

/// [`Phase::Link`]: the unit's diagnostics as cargo shows them and, unless its check failed,
/// its images linked from what codegen made (relinking `images`) or a library's record; a
/// proc macro's expanders loaded. A unit that has no link phase has this done by its store.
fn link(
    job: &Job,
    reader: &UnitReader,
    unit: usize,
    phases: &mut Phases,
    images: &mut Vec<Image>,
) -> Result<(), Failed> {
    if let Some(mut kept) = phases.kept.take() {
        kept.macros = macros(job, reader, unit, kept.ok, Module::Kept(&kept));
        held(job, unit, &mut kept)?;
        phases.linked = Some(Linked::Kept(kept));
        return Ok(());
    }
    if job.unit(unit)?.mode == Mode::Doctest {
        return doctest::link(job, reader, unit, phases, images);
    }
    let mut making = assemble(job, reader, unit, phases, images)?;
    making.macros = macros(job, reader, unit, making.ok, Module::Linked(&making.files));
    if !making.nothing
        && let Some(Ok((_, Ok(Ok(parts))))) = &mut phases.generated
    {
        // Moved, not copied: the store keeps them as they are.
        let built = std::mem::take(&mut parts.built.puts);
        making.puts.extend(built.into_iter().map(|it| Put {
            key: it.key,
            kind: it.kind,
            bytes: it.bytes,
        }));
    }
    if let Some(Ok((_, Ok(Ok(parts))))) = &mut phases.generated
        && !parts.built.misses.is_empty()
    {
        // Its functions are subjects within the unit: another unit's build of one is another.
        let subject = unit_subject(job, job.unit(unit)?);
        for mut miss in std::mem::take(&mut parts.built.misses) {
            match &subject {
                Ok(subject) => miss.subject = within(*subject, miss.subject),
                Err(why) => {
                    // No subject to compare it by: said, not dropped.
                    miss.subject = ContentId::default();
                    // alloc: error path, once a miss
                    miss.missed =
                        blaze_kernel::Missed::Failed(format!("its unit has no subject: {why}"));
                    miss.parts.clear();
                }
            }
            phases.misses.push(miss);
        }
    }
    if let Some(Ok((_, Ok(Ok(parts))))) = &mut phases.generated
        && !parts.built.memo_used.is_empty()
    {
        let used = std::mem::take(&mut parts.built.memo_used);
        if let Some(slot) = job.memo_used.get(unit) {
            // A unit links once a job: a second set has nothing the first lacks.
            let _ = slot.set(used);
        }
    }
    phases.linked = Some(Linked::Made(Box::new(making)));
    // What codegen made is linked (a unit links once a job, and [`store`] reads only
    // `linked`): every compiled function's code, relocations and line rows, its symbols and
    // test listing go now, not as the job ends. Held to the end, the code of each test binary,
    // with the std code each reaches copied into every one, was the 8 GiB codegen heap of a
    // `blaze test` of 21 small crates (B5, coord-105).
    phases.generated = None;
    Ok(())
}

/// A dev executable's record replayed ([`super::run::Launch::InProcess`]): each program it
/// names held by the daemon as that file ([`super::run::inproc::DevRuns::adopt`]), at its path
/// and at the copy cargo makes of it, so a run of the build starts the code the record is of.
/// The daemon holds only what it linked or adopted: a record another daemon made, or one of
/// an earlier value of the unit's inputs than the daemon's last link at the path (a variable
/// the build script reads set back), runs its own file, never the daemon's other link. A
/// running program replaced is told to start again, and the build says so.
fn held(job: &Job, unit: usize, kept: &mut Compiled) -> Result<(), Failed> {
    if !kept.ok {
        return Ok(());
    }
    let plan = job.plan(unit)?;
    let made = artifact::made(plan);
    let programs =
        || kept.outputs.iter().filter(|it| it.executable && made.contains(&it.path.as_str()));
    if programs().next().is_none() {
        return Ok(());
    }
    // The question its link asked ([`codegen::policy`]): a live build is a dev executable's.
    let policy =
        codegen::toolchain(job, unit).and_then(|toolchain| codegen::policy(job, unit, toolchain));
    if policy.map_err(|what| ApiError::new(what, None))?.mode != blaze_codegen::plan::Mode::Live {
        return Ok(());
    }
    let guard = blaze_cas::enter();
    let mut told = Vec::new();
    for output in programs() {
        let path = output.path.as_str();
        // Read only when the daemon does not hold the program by this key already.
        let bytes = || match kept.puts.iter().find(|it| it.key == output.key) {
            Some(put) => Ok(&put.bytes[..]),
            None => match job.input.store.get(&guard, &output.key) {
                Ok(Some(it)) => Ok(it.bytes),
                // alloc: error path
                Ok(None) => Err(format!(
                    "its record names the output {:x}, which the store does not hold",
                    output.key.0
                )),
                // alloc: error path
                Err(corrupt) => Err(format!(
                    "reading the output {:x} from the store: {corrupt}",
                    output.key.0
                )),
            },
        };
        let runs_from = artifact::uplifted(path, plan);
        let adopted = job.shared.dev_runs.adopt(path, runs_from, output.key, bytes);
        match adopted.map_err(|what| ApiError::new(format!("{path}: {what}"), None))? {
            // alloc: a path, once per running program replaced
            super::run::inproc::Told::Restart { why } => told.push((path.to_owned(), why)),
            super::run::inproc::Told::Nothing | super::run::inproc::Told::Patched { .. } => {}
        }
    }
    if told.is_empty() {
        return Ok(());
    }
    let unit_of = job.unit_of(unit)?;
    for (path, why) in told {
        // alloc: a warning's text, once per restart
        let what = format!(
            "`{path}` is running and can't take this build as a patch: {why}; start it again to run it"
        );
        let said = say::cargo(Level::Warning, what, Some(unit_of.clone()), job.input.color);
        kept.diagnostics.push(said);
    }
    Ok(())
}

/// [`Phase::Store`]: each output of the unit hashed into one put, its record encoded, and its
/// compile.
fn store(
    job: &Job,
    reader: &UnitReader,
    unit: usize,
    phases: &mut Phases,
) -> Result<Compiled, Failed> {
    if phases.linked.is_none() {
        link(job, reader, unit, phases, &mut Vec::new())?;
    }
    // A unit stored that built no code with a profile (none asked, it is checked only, or it
    // builds none) used none: counted as such, so the job's check that a profile applied to
    // something accounts for every unit it built ([`codegen::unapplied`]).
    if let Some(slot) = job.counted.get(unit) {
        slot.get_or_init(Box::default);
    }
    if matches!(phases.linked, Some(Linked::Kept(_)))
        && let Some(Linked::Kept(kept)) = phases.linked.take()
    {
        return Ok(kept);
    }
    let u = job.unit(unit)?;
    let plan = job.plan(unit)?;
    let package = job.package(u)?;
    // Read before the unit's is taken, so a give-way finds it as it was.
    let krate = crate_of(job, unit, u)?;
    let sources = crate_files(reader, krate);
    // Everything its closure's code was made from is in the database's logs now: it compiled.
    // An expansion whose reads were not traced read what no key can cover: no record is kept.
    let read = {
        let _span = tracing::info_span!("unit expansion reads").entered();
        reads::Reads::of(reader.db(), krate)
    };
    if let Err(why) = &read {
        tracing::info!(unit, crate_name = %u.target.name, "unit record not kept: {why}");
    }
    let key = record_key(job, unit)?.filter(|_| read.is_ok());
    let Some(Linked::Made(making)) = phases.linked.take() else {
        // alloc: error path
        return Err(ApiError::new(
            format!("unit {unit} (`{}`) is stored before it is linked", u.target.name),
            None,
        )
        .into());
    };
    let Making {
        diagnostics,
        ok,
        open,
        why,
        mut made,
        nothing,
        files,
        mut puts,
        natives,
        links,
        macros,
        records,
    } = *making;
    let mut outputs = Vec::new();
    let mut artifact = None;
    if !nothing {
        let mounted = &job.mounted;
        // Each file's bytes go into the store once, moved, under their key; a copy cargo makes
        // of one (`target/debug/foo` of `deps/foo-0123`) is the same key.
        let mut keyed: Vec<(String, Key, bool)> = Vec::with_capacity(files.len());
        let mut output_keys: Vec<Key> = Vec::with_capacity(plan.outputs.len());
        let mut put_output = |puts: &mut Vec<Put>, bytes: Vec<u8>| {
            let key = output_key(&bytes);
            if !output_keys.contains(&key) {
                output_keys.push(key);
                puts.push(Put { key, kind: Kind::Output, bytes });
            }
            key
        };
        let main_file = files.first().map(|it| it.0.clone());
        for (path, written, executable) in files {
            let key = match written {
                Written::Bytes(bytes) => put_output(&mut puts, bytes),
                // In the store already, under the record that names it.
                Written::Kept(key) => key,
            };
            keyed.push((path, key, executable));
        }
        // Each image record filed now its output has a key: the header of a unit record,
        // naming that output and the native inputs its link read, and nothing after it.
        for ImageRecord { path, filing, natives } in records {
            let Some((_, key, _)) = keyed.iter().find(|it| it.0 == path) else {
                // alloc: error path
                let what = format!(
                    "{} keeps a record of the image {path}, which it did not write",
                    span_unit(u)
                );
                return Err(ApiError::new(what, None).into());
            };
            let mut bytes = Vec::new(); // alloc: once per image recorded
            header::write(&mut bytes, &[*key], &natives, &[]);
            puts.push(Put { key: filing.key, kind: Kind::Query, bytes });
            puts.extend(filing.slot.map(|(key, bytes)| Put { key, kind: Kind::Query, bytes }));
        }
        outputs.reserve(plan.outputs.len());
        for path in &plan.outputs {
            let found = keyed.iter().find(|it| it.0 == *path).or_else(|| {
                keyed.iter().find(|it| artifact::uplifted(&it.0, plan) == Some(path.as_str()))
            });
            let (key, executable) = match found {
                Some((_, key, executable)) => (*key, *executable),
                None if path.ends_with(".d") => {
                    let text = dep_info(path, plan, main_file.as_deref(), &sources, mounted);
                    (put_output(&mut puts, text.into_bytes()), false)
                }
                // Debug info beside an image (`.dSYM`, `.pdb`, `.dwp`): blaze's images carry
                // their line tables, and write none beside them.
                None => continue,
            };
            outputs.push(Output { path: path.clone(), key, executable });
        }
        let main = artifact::main(u, plan);
        // A library's doctests: each example's program and their verdicts, beside the runner
        // the plan names ([`doctest::link`]).
        if u.mode == Mode::Doctest
            && let Some(main) = &main
        {
            for (path, key, executable) in &keyed {
                if doctest::beside(main, path) {
                    // clone: a path, once per example stored
                    outputs.push(Output { path: path.clone(), key: *key, executable: *executable });
                }
            }
        }
        let main_key = main
            .as_ref()
            .and_then(|main| outputs.iter().find(|it| it.path == *main))
            .map(|it| it.key);
        made.main = match (&main, main_key) {
            (Some(path), Some(key)) => Some((path.clone(), key.0)),
            _ => None,
        };
        artifact = match (&main, main_key) {
            (Some(path), Some(key)) => {
                Some(artifact::artifact(u, plan, package, path, key.0, false, runs(job, unit))?)
            }
            _ => None,
        };
        // The native code linked in is in no code hash a test reaches: a change of it runs them all.
        if let Some(context) = &mut made.context {
            *context = with_natives(*context, &natives);
        }
    }
    let misses = std::mem::take(&mut phases.misses);
    let why = why.map(str::to_owned); // alloc: a unit that failed, once
    let mut compiled = Compiled {
        diagnostics,
        ok,
        open,
        why,
        puts,
        share: Vec::new(),
        outputs,
        artifact,
        macros,
        links,
        misses,
    };
    // Every slot of the base key holding other names leaves the record unkept, and the unit
    // compiles again next time.
    let filing = match (key.filter(|_| !script::pure::built_nothing(job, unit)), &read) {
        (Some(base), Ok(read)) => {
            let guard = blaze_cas::enter();
            reads::file(
                base,
                read,
                &job.input.config.invocation.env,
                &|manifest| job.units().is_primary_manifest(manifest),
                |slot| job.input.store.get(&guard, slot).ok().flatten().map(|it| it.bytes),
            )
            .map_err(|why| {
                ApiError::new(
                    format!("filing the record of unit {unit} (`{}`): {why}", u.target.name),
                    None,
                )
            })?
        }
        _ => None,
    };
    if let Some(reads::Filing { key, slot }) = filing {
        let record = RecordRef {
            ok: compiled.ok,
            open: compiled.open,
            why: compiled.why.as_deref(),
            diagnostics: &compiled.diagnostics,
            outputs: compiled
                .outputs
                .iter()
                .map(|it| (it.path.as_str(), it.key, it.executable))
                .collect(),
            artifact: compiled.artifact.as_ref(),
            made: &made,
        };
        let keys: Vec<Key> = compiled.outputs.iter().map(|it| it.key).collect();
        let mut bytes = Vec::new();
        header::write(&mut bytes, &keys, &natives, codegen::counted(job, unit));
        blaze_protocol::cbor::append(&mut bytes, &record).map_err(|why| {
            ApiError::new(format!("encoding the record of unit {unit}: {why}"), None)
        })?;
        let record = Put { key, kind: Kind::Query, bytes };
        let slot = slot.map(|(key, bytes)| Put { key, kind: Kind::Query, bytes });
        if shared(job, unit) {
            compiled.share = share(job, unit, &compiled, &record)?;
            if let Some(slot) = &slot {
                // alloc: a slot's few names, once per std unit made that read a variable
                compiled.share.push(Put {
                    key: slot.key,
                    kind: slot.kind,
                    bytes: slot.bytes.clone(),
                });
            }
        }
        compiled.puts.push(record);
        compiled.puts.extend(slot);
    }
    if let Some(slot) = job.made.get(unit) {
        let _ = slot.set(made);
    }
    Ok(compiled)
}

/// Where a proc macro built for wasm has its module, as its link phase finds it.
#[derive(Clone, Copy)]
enum Module<'a> {
    /// The files its link just wrote: (path, what it holds, executable).
    Linked(&'a [(String, Written, bool)]),
    /// Its record, kept: its outputs are among its puts or in the store.
    Kept(&'a Compiled),
}

impl Module<'_> {
    /// The module's bytes: the one `.wasm` file the unit writes ([`codegen::wasm_macro`]'s
    /// output is named as a `cdylib` of wasm32-wasip1 is), checked to be a module before
    /// Wasmtime is given it. An error says why there are none, or what the file is instead.
    fn bytes(&self, job: &Job) -> Result<Vec<u8>, ModuleError> {
        let wasm = |path: &str| path.ends_with(".wasm");
        let (path, bytes) = match self {
            Module::Linked(files) => match files.iter().find(|(path, _, _)| wasm(path)) {
                // alloc: once per proc macro linked for wasm, the module's bytes for its load
                Some((path, Written::Bytes(bytes), _)) => (path.as_str(), bytes.clone()),
                // Only a doctest's images are kept as they were; a proc macro's is linked.
                Some((_, Written::Kept(_), _)) | None => return Err(ModuleError::NotLinked),
            },
            Module::Kept(kept) => {
                let Some(output) = kept.outputs.iter().find(|it| wasm(&it.path)) else {
                    return Err(ModuleError::NotKept);
                };
                let path = output.path.as_str();
                match kept.puts.iter().find(|it| it.key == output.key) {
                    // alloc: once per proc macro kept for wasm, the module's bytes for its load
                    Some(put) => (path, put.bytes.clone()),
                    None => {
                        let guard = blaze_cas::enter();
                        match job.input.store.get(&guard, &output.key) {
                            // alloc: as above
                            Ok(Some(it)) => (path, it.bytes.to_vec()),
                            // alloc: error path
                            Ok(None) => {
                                return Err(ModuleError::NotInStore { path: path.to_owned() });
                            }
                            // alloc: error path
                            Err(corrupt) => {
                                return Err(ModuleError::Corrupt {
                                    path: path.to_owned(),
                                    why: corrupt.to_string(),
                                });
                            }
                        }
                    }
                }
            }
        };
        module_at(path, bytes)
    }
}

/// `bytes`, a proc macro's `.wasm` output at `path`, if they are a wasm module; else what they
/// are instead, so Wasmtime is never given a crate record or other file to parse.
pub(crate) fn module_at(path: &str, bytes: Vec<u8>) -> Result<Vec<u8>, ModuleError> {
    match FileKind::of(&bytes) {
        FileKind::WasmModule => Ok(bytes),
        // alloc: error path
        found => Err(ModuleError::NotAModule { path: path.to_owned(), found }),
    }
}

/// What a file a unit writes is, by its first bytes: what a proc macro's `.wasm` output is
/// checked to be before Wasmtime is given it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FileKind {
    /// A WebAssembly module (`\0asm`): what a proc macro's link writes.
    WasmModule,
    /// An `ar` archive (`!<arch>\n`): a library's crate record ([`crate_record`]), what a unit
    /// that is only checked writes for each of its outputs.
    Archive,
    /// Neither.
    Other,
}

impl FileKind {
    pub(crate) fn of(bytes: &[u8]) -> FileKind {
        match bytes {
            [0, b'a', b's', b'm', ..] => FileKind::WasmModule,
            _ if bytes.starts_with(b"!<arch>\n") => FileKind::Archive,
            _ => FileKind::Other,
        }
    }

    fn name(self) -> &'static str {
        match self {
            FileKind::WasmModule => "a linked wasm32-wasip1 module",
            FileKind::Archive => {
                "an `ar` archive (a library's crate record, which a unit only checked writes)"
            }
            FileKind::Other => "neither a wasm module nor an archive",
        }
    }
}

/// Why a proc macro built for wasm has no module to load ([`Module::bytes`]), each said after
/// "the wasm module of the proc-macro crate `name`:".
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum ModuleError {
    /// Its link wrote no `.wasm` file.
    NotLinked,
    /// Its kept record names no `.wasm` output.
    NotKept,
    /// Its kept record's `.wasm` output is not in the store.
    NotInStore { path: String },
    /// The store could not read its kept `.wasm` output.
    Corrupt { path: String, why: String },
    /// Its `.wasm` output holds `found`, not a module: the unit was not linked as a wasm proc
    /// macro (it was only checked, or a record of another kind is kept under its key).
    NotAModule { path: String, found: FileKind },
}

impl fmt::Display for ModuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModuleError::NotLinked => f.write_str("its link wrote no `.wasm` file"),
            ModuleError::NotKept => f.write_str("its kept record names no `.wasm` output"),
            ModuleError::NotInStore { path } => {
                write!(f, "its kept module {path} is not in the store")
            }
            ModuleError::Corrupt { path, why } => write!(f, "its kept module {path}: {why}"),
            ModuleError::NotAModule { path, found } => write!(
                f,
                "its output {path} is {}, where {} is expected: the unit was not linked as a wasm proc macro",
                found.name(),
                FileKind::WasmModule.name()
            ),
        }
    }
}

/// The expanders of proc-macro unit `unit`, for the crates that use it
/// ([`crate::serve::work::Writer::macros`]); none for a unit that is no proc macro or did not
/// build. Every build plans its proc macros for wasm32-wasip1 ([`codegen::wasm_macro`]), and
/// each runs its `module` in this process, every expansion in a fresh instance under Wasmtime
/// ([`blaze_macro_eval::load_wasm`]); a module that can't be read or loaded is an error naming
/// the macro crate, never run another way. A proc macro built for the host (a plan with
/// `Spec::macro_wasm` off, which no build makes) is refused naming it: blaze's MIR evaluator
/// is const eval's and the reference the wasm host is tested against, never a build's macro
/// host (AGENTS.md rule 8).
///
/// Asked again each time the unit compiles or its record is reused, as the front end it reads
/// may have changed.
fn macros(
    job: &Job,
    reader: &UnitReader,
    unit: usize,
    ok: bool,
    module: Module<'_>,
) -> Option<ProcMacroLoadResult> {
    if !ok || !job.crates.is_proc_macro(unit) {
        return None;
    }
    let name = job.unit(unit).map_or("", |it| it.target.name.as_str());
    let failed = |what: String| Some(Err(ProcMacroLoadingError::Refused(what.into())));
    if !codegen::wasm_macro(job, unit) {
        // alloc: error path
        let (package, built_for) = job
            .unit(unit)
            .map(|it| (it.pkg_id.as_str(), it.built_for().to_string()))
            .unwrap_or_default();
        return failed(format!(
            "proc macro `{name}` of {package} is built for {built_for}, not as a wasm module: a build runs proc \
             macros only as wasm32-wasip1 modules, so its unit plan is wrong"
        ));
    }
    let Some(krate) = job.crates.krate(unit) else {
        return failed(format!("proc macro `{name}` has no crate in the front end"));
    };
    // What its expansions are kept under: the front end's code, whose closure holds the macro
    // server and bridge that run them ([`super::deps::front_end`]), as the daemon's own
    // expansions are.
    let toolchain = match super::deps::front_end() {
        Ok(it) => it,
        Err(why) => return failed(format!("proc macro `{name}`: {}", why.what)),
    };
    let bytes = match module.bytes(job) {
        Ok(it) => it,
        Err(why) => {
            return failed(format!("the wasm module of the proc-macro crate `{name}`: {why}"));
        }
    };
    let modules = match job.input.macros.modules() {
        Ok(it) => it,
        Err(why) => {
            return failed(format!(
                "the wasm module of the proc-macro crate `{name}`: no Wasmtime engine: {why}"
            ));
        }
    };
    Some(blaze_macro_eval::load_wasm(
        reader.db(),
        krate,
        toolchain,
        &job.input.macros,
        modules,
        &bytes,
    ))
}

/// The lint flags of unit `unit`'s check, as its check is given them: its plan's
/// ([`lint_flags`]), then a `--check-cfg` for each `cargo::rustc-check-cfg` its package's build
/// script runs printed ([`script_check_cfgs`]), as cargo's `add_custom_flags` gives rustc
/// them after the plan's. The one list the check, its options and the served outcomes' keys
/// read.
pub(crate) fn unit_lint_flags(job: &Job, unit: usize) -> Result<Vec<String>, ApiError> {
    let u = job.unit(unit)?;
    let package = job.package(u)?;
    let mut flags =
        lint_flags(u, job.plan(unit)?, local(package.source.as_deref()), job.input.verbose > 1);
    let scripts = script_check_cfgs(job, unit)?;
    flags.reserve_exact(2 * scripts.len());
    for spec in scripts {
        flags.push("--check-cfg".to_owned()); // alloc: `CommandLine::parse` takes `&[String]`; once per spec per unit per build
        flags.push(spec);
    }
    Ok(flags)
}

/// What follows each `--check-cfg` unit `unit`'s rustc is given by its package's build script
/// runs (`cargo::rustc-check-cfg`, [`codegen::given`]): read once those runs are applied, which
/// every unit's check and record key come after; an error naming a run it is linked with that
/// has not run, never a list without it.
fn script_check_cfgs(job: &Job, unit: usize) -> Result<Vec<String>, ApiError> {
    let u = job.unit(unit)?;
    // clone: the unit's name, on the error path
    codegen::given(job, unit as u32)
        .map(|it| it.check_cfgs)
        .map_err(|what| ApiError::new(what, Some(u.target.name.clone())))
}

/// The `rustc-link-arg*`s `unit`'s link is given by the build script runs it is linked with
/// ([`codegen::script_args`]), each with its script's package: read once those runs are
/// applied, as [`script_check_cfgs`] is.
fn script_link_args(job: &Job, unit: usize) -> Result<Vec<(String, String)>, ApiError> {
    let u = job.unit(unit)?;
    // clone: the unit's name, on the error path
    let named = |what: String| ApiError::new(what, Some(u.target.name.clone()));
    let mine = codegen::given(job, unit as u32).map_err(named)?;
    let args = codegen::script_args(job, unit as u32, &mine).map_err(named)?;
    Ok(args
        .into_iter()
        .filter_map(|it| match it.from {
            blaze_link::args::ArgFrom::BuildScript { package } => Some((package, it.arg)),
            blaze_link::args::ArgFrom::Rustflags | blaze_link::args::ArgFrom::RustflagOption => {
                None
            }
        })
        // alloc: once per unit per build, what its record key hashes
        .collect())
}

/// The `-l`s and `-L`s of the rustflags of the crates `unit` links in beside its own
/// ([`codegen::linked_rustflag_libs`]): none for a unit that links nothing.
fn linked_rustflag_libs(job: &Job, unit: usize) -> Result<Vec<(String, String)>, ApiError> {
    let u = job.unit(unit)?;
    if !matches!(codegen::makes(u), Ok(Makes::Executable | Makes::Shared { .. })) {
        // alloc: none, empty
        return Ok(Vec::new());
    }
    codegen::linked_rustflag_libs(job, unit as u32)
        // clone: the unit's name, on the error path
        .map_err(|what| ApiError::new(what, Some(u.target.name.clone())))
}

pub(crate) use blaze_lower::{lint_flags, local};

/// Whether a diagnostic is one of rustc's closing lines cargo does not show: "aborting due to
/// ...", "N warnings emitted" (`cargo/src/cargo/core/compiler/mod.rs`, `on_stderr_line_inner`).
pub(crate) fn cargo_hides(message: &str) -> bool {
    message.starts_with("aborting due to")
        || message.ends_with("warning emitted")
        || message.ends_with("warnings emitted")
}

/// `Ok` once every proc macro `krate` can expand is built ([`blaze_items::macros_built`]): what
/// a unit's front-end job asks as it starts, before its def map, check, inference or codegen
/// reads anything. A unit is posted after each unit it depends on is done, a proc macro's load
/// applied, so this holds in every build; a unit ordered ahead of a proc macro's build stops
/// here with an error naming it and the macro (rule 3), never checks a half-expanded crate.
fn macros_built(db: &base_db::Db, krate: base_db::Crate, u: &Unit) -> Result<(), ApiError> {
    blaze_items::macros_built(db, krate).map_err(|pending| {
        // alloc: error path
        let what = format!(
            "{} was to start before the proc macros it loads were built: {}",
            span_unit(u),
            pending.describe(db)
        );
        ApiError::new(what, Some(u.target.name.clone()))
    })
}

/// `unit`'s crate in the front end.
pub(super) fn crate_of(job: &Job, unit: usize, u: &Unit) -> Result<base_db::Crate, ApiError> {
    job.crates.krate(unit).ok_or_else(|| {
        // alloc: error path
        ApiError::new(
            format!("unit {unit} (`{}`) has no crate in the front end", u.target.name),
            None,
        )
    })
}

/// The lint flags of `unit`'s check, and the options they give it in `db`
/// ([`blaze_diagnostics::cache::Inputs::options`], the same a kept outcome of it is keyed by):
/// an error for flags that name no lint blaze has, whether or not a check runs.
pub(super) fn options(
    job: &Job,
    db: &base_db::Db,
    unit: usize,
    u: &Unit,
) -> Result<(Vec<String>, Options), ApiError> {
    let flags = unit_lint_flags(job, unit)?;
    let options = blaze_diagnostics::cache::Inputs::of(db, &flags).options().map_err(|why| {
        ApiError::new(why, Some(format!("the lint flags of `{}`", u.target.name)))
    })?;
    Ok((flags, options))
}

/// [`Phase::Link`]'s making of the unit, from what its phases before left in `phases`, which
/// it only reads: its diagnostics as cargo shows them, then, unless they fail it, the files
/// rustc would write, each image linked (relinking `images`).
fn assemble(
    job: &Job,
    reader: &UnitReader,
    unit: usize,
    phases: &Phases,
    images: &mut Vec<Image>,
) -> Result<Making, Failed> {
    let u = job.unit(unit)?;
    let plan = job.plan(unit)?;
    let unit_of = job.unit_of(unit)?;
    let color = job.input.color;
    let mounted = &job.mounted;
    let (Some(Does { inert, .. }), Some(diags)) = (phases.does, &phases.diags) else {
        // alloc: error path
        let what = format!(
            "unit {unit} (`{}`) is linked before its names are read and its check is done",
            u.target.name
        );
        return Err(ApiError::new(what, None).into());
    };
    let mut sources = blaze_diagnostics::emit::Sources::default();
    for (name, text) in &phases.unmounted {
        // clone: a file no mount holds, once per unit whose diagnostics point into one
        sources.add(name, Some(text.clone()));
    }
    mounted.add_sources(&mut sources, diags);
    let mut style = blaze_diagnostics::emit::Style {
        ansi: color,
        short: job.input.short,
        ..Default::default()
    };
    if let Some(width) = job.input.width {
        style.width = usize::try_from(width).unwrap_or(usize::MAX);
    }
    let mut diagnostics: Vec<Diagnostic> = diags
        .iter()
        .filter(|it| !cargo_hides(&it.message))
        .map(|it| {
            blaze_diagnostics::api::diagnostic(it, &sources, style, Some(unit_of.clone()))
                // alloc: error path, naming the diagnostic whose span can't be placed
                .map_err(|why| {
                    format!(
                        "{why}, in the diagnostic `{}`",
                        it.message.lines().next().unwrap_or_default()
                    )
                })
        })
        .collect::<Result<_, _>>()
        // clone: the unit's name, on the error path
        .map_err(|why| ApiError::new(why, Some(u.target.name.clone())))?;
    let failed = diagnostics.iter().any(|it| it.level == Level::Error);
    let mut made = Made::default();
    let nothing = |diagnostics: Vec<Diagnostic>, ok: bool, made: Made| Making {
        diagnostics,
        ok,
        open: false,
        why: None,
        made,
        nothing: true,
        files: Vec::new(),
        puts: Vec::new(),
        natives: Vec::new(),
        links: Vec::new(),
        macros: None,
        records: Vec::new(),
    };
    if failed {
        // Failed in its bodies or lints only: its items are sound, and what is checked against
        // them may be under keep-going ([`Compiled::open`]). A unit whose code is built or
        // run (a build script, a proc macro, a library a program links) fails shut: what
        // depends on it needs that code. Both inputs are in the record's key (its check's
        // outcome through its sources, the goal through [`keyed_goal`]).
        let only_checked = checks_only(job.input.goal, u);
        let open = !phases.items_broken && only_checked;
        let why = match (open, only_checked) {
            (true, _) => "errors only in its bodies or lints",
            (false, true) => "errors in its items",
            (false, false) => "errors in its code",
        };
        return Ok(Making { open, why: Some(why), ..nothing(diagnostics, false, made) });
    }
    if job.cancelled() {
        return Err(Failed::Again);
    }
    let mut natives = Vec::new();
    let mut links = Vec::new(); // alloc: none until a link
    // What each file rustc would write holds: (path, bytes, executable).
    let mut files: Vec<(String, Written, bool)> = Vec::new();
    let identity = identity(job, unit);
    if inert {
        // A build script that does nothing is not run, so not built: what it prints is known.
        return Ok(nothing(diagnostics, true, made));
    }
    match &phases.generated {
        // [`generate`] leaves none only for a unit that builds no code; one that does and has
        // none had it taken by its link ([`link`] lets it go once linked), so a second link
        // would write records in place of its code: refused, named, never written.
        None if matches!(phases.does, Some(Does { builds_code: true, .. })) => {
            // alloc: error path
            return Err(ApiError::new(format!("unit {unit} ({}) is linked twice in one job: its generated code went with its first link", span_unit(u)), None).into());
        }
        None => {
            for path in artifact::made(plan) {
                files.push((
                    path.to_owned(),
                    Written::Bytes(crate_record(u, identity, &[])),
                    false,
                ));
            }
        }
        Some(Err(what)) => {
            // clone: error path
            diagnostics.push(say::refused(what.clone(), Some(unit_of), color));
            return Ok(nothing(diagnostics, false, made));
        }
        Some(Ok((makes, generated))) => {
            let parts = match generated
                .as_ref()
                .map_err(|what| ApiError::new(what.clone(), Some(u.target.name.clone())))?
            {
                Ok(it) => it,
                Err(refused) => {
                    // A toolchain function the front end failed on: the job's report of it
                    // first, the same from every unit that reaches it, which the job says once
                    // (`Jobs::compiled`); the unit's own line points at it.
                    for failed in &refused.toolchain {
                        let report = failed
                            .report(&mut sources, mounted, style, color)
                            // clone: the unit's name, on the error path
                            .map_err(|why| ApiError::new(why, Some(u.target.name.clone())))?;
                        diagnostics.push(report);
                    }
                    let what = crate::refused_error(
                        &span_unit(u),
                        refused.errors.len(),
                        refused.lines(),
                        job.input.refused,
                    );
                    diagnostics.push(say::refused(what, Some(unit_of), color));
                    return Ok(nothing(diagnostics, false, made));
                }
            };
            if job.cancelled() {
                return Err(Failed::Again);
            }
            let generated = parts.over(reader, mounted);
            // Lint `blaze::vecreport`: what the optimized code still does in its loops, at
            // the source line, in the order of the positions.
            let reported = generated
                .report(&job.input.store)
                .map_err(|what| ApiError::new(what, Some(u.target.name.clone())))?;
            for it in reported {
                // alloc: a finding's text, once each
                let what = match it.file.is_empty() {
                    true => format!(
                        "{} [blaze::vecreport, in `{}`, at no statement]",
                        it.message, it.function
                    ),
                    false => format!(
                        "{}:{}:{}: {} [blaze::vecreport, in `{}`]",
                        it.file, it.line, it.column, it.message, it.function
                    ),
                };
                diagnostics.push(say::cargo(Level::Warning, what, Some(unit_of.clone()), color));
            }
            made.context = Some(context(generated.policy));
            made.inputs =
                generated.built.inputs.iter().map(|(key, code)| (key.0, code.0)).collect();
            // A test's output prints positions (a panic's `file:line:col`), which no code hash
            // has: a reformat that moves a `panic!` changes its message. Every span table of the
            // binary is in each test's selection, so such an edit runs its tests again.
            // alloc: once per test unit, one hash per span table
            let mut spans: Vec<CodeHash> = match generated.roots.tests.is_empty() {
                true => Vec::new(),
                false => generated
                    .side()
                    .map_err(|what| ApiError::new(what, Some(u.target.name.clone())))?
                    .spans
                    .iter()
                    .map(|(anchor, table)| {
                        let mut hasher = Hasher::new(SPANS);
                        hasher.id(anchor).part(table);
                        CodeHash(hasher.finish())
                    })
                    .collect(),
            };
            spans.sort_unstable();
            spans.dedup();
            made.spans = spans;
            // Each test by name only: what it reaches is walked from the binary's table when a
            // reach request or a kept run asks, not here, where a binary of thousands of tests
            // walked the program once per test (2026-11-01: minutes per binary).
            for test in &generated.roots.tests {
                if generated.built.roots.get(test.root).is_none() {
                    return Err(Failed::Error(ApiError::new(
                        format!("test `{}` has no root among the build's", test.name),
                        Some(u.target.name.clone()),
                    )));
                }
                made.tests.push(TestOf {
                    name: test.name.clone(),
                    ignore: test.ignore,
                    should_panic: test.should_panic.clone(),
                    bench: test.bench,
                    outside: test.outside,
                });
            }
            let roots: Vec<ContentId> = generated.built.roots.iter().map(|it| it.0).collect();
            for path in artifact::made(plan) {
                let library = path.ends_with(".rlib") || path.ends_with(".rmeta");
                if library || *makes == Makes::Library {
                    files.push((
                        path.to_owned(),
                        Written::Bytes(crate_record(u, identity, &roots)),
                        false,
                    ));
                    continue;
                }
                match codegen::link(
                    job,
                    unit,
                    crate_of(job, unit, u)?,
                    path,
                    makes,
                    &generated,
                    images,
                ) {
                    Ok(linked) => {
                        // The running program was told to start again: the build says so too,
                        // naming the executable and why, as no patch reached it.
                        if let Some(why) = &linked.restart {
                            // alloc: a warning's text, once per restart
                            let what = format!(
                                "`{path}` is running and can't take this build as a patch: {why}; start it again to run it"
                            );
                            diagnostics.push(say::cargo(
                                Level::Warning,
                                what,
                                Some(unit_of.clone()),
                                color,
                            ));
                        }
                        natives.extend(linked.read);
                        if linked.table.is_some() {
                            made.table = linked.table;
                        }
                        // alloc: once per image linked, a unit links one or two
                        links.push((path.to_owned(), linked.how));
                        files.push((path.to_owned(), Written::Bytes(linked.bytes), true));
                    }
                    Err(failed) => {
                        // alloc: error path
                        diagnostics.push(say::refused(failed.to_string(), Some(unit_of), color));
                        return Ok(nothing(diagnostics, false, made));
                    }
                }
            }
        }
    }
    // alloc: none: the built puts are moved in after, and the outputs by the store
    Ok(Making {
        diagnostics,
        ok: true,
        open: false,
        why: None,
        made,
        nothing: false,
        files,
        puts: Vec::new(),
        natives,
        links,
        macros: None,
        records: Vec::new(),
    })
}

/// A test binary's `context` with the content of each native input its link read, in link
/// order; `context` itself when it read none.
pub(crate) fn with_natives(context: ContentId, natives: &[(String, ContentId)]) -> ContentId {
    if natives.is_empty() {
        return context;
    }
    let mut hasher = Hasher::new(NATIVES);
    hasher.id(&context);
    for (_, native) in natives {
        hasher.id(native);
    }
    hasher.finish()
}

/// How `unit`'s program is run: its plan's runner, and cargo's variables of it, from the build
/// scripts' runs it depends on (its package's script's `rustc-env`, every run's library search
/// directories). Worked out once per job and unit: the record's key and the artifact read it.
fn runs(job: &Job, unit: usize) -> artifact::Runs {
    if let Some(slot) = job.runs.get(unit) {
        return slot.get_or_init(|| work_out_runs(job, unit)).clone();
    }
    artifact::Runs::default()
}

fn work_out_runs(job: &Job, unit: usize) -> artifact::Runs {
    let units = job.units();
    let Some(plan) = units.plans.get(unit) else { return artifact::Runs::default() };
    let package = job.graph.package(unit as u32);
    let mut own: &[(String, String)] = &[];
    let mut native: Vec<String> = Vec::new();
    let mut seen = blaze_hash::HashSet::new_in(std::alloc::Global);
    for at in job.graph.closure(unit as u32) {
        if !job.graph.has(at, graph::flag::RUN) {
            continue;
        }
        let Some(applied) = job.scripts[at as usize].get() else { continue };
        if job.graph.package(at) == package {
            own = &applied.output.env;
        }
        for path in &applied.output.library_paths {
            let path = blaze_build_script::output::search_path_of(path.path());
            if seen.insert(path) {
                native.push(path.to_string_lossy().into_owned());
            }
        }
    }
    let cargo = job.shared.launcher.to_string_lossy();
    artifact::Runs {
        runner: plan.runner.clone(),
        env: artifact::run_env(plan, &cargo, own, &native, std::env::consts::OS),
    }
}

/// What a test binary's behaviour depends on beside the code it reaches.
fn context(policy: &blaze_codegen::plan::Policy) -> ContentId {
    let bytes = blaze_protocol::cbor::to_vec(policy).unwrap_or_default();
    ContentId::of("blaze v2 test context 2", &bytes)
}

/// What blaze writes for a library (`.rlib`, `.rmeta`): no object code (its functions are in
/// the store, by key, for whatever links it), but an `ar` archive, as rustc's are, with one
/// member `lib.rmeta` saying which unit it is, what it was made from, and its roots' keys.
pub(crate) fn crate_record(
    unit: &Unit,
    identity: Option<ContentId>,
    roots: &[ContentId],
) -> Vec<u8> {
    let mut member = b"blaze v2 crate 1\n".to_vec();
    member.extend_from_slice(format!("{} {}\n", unit.pkg_id, unit.target.name).as_bytes());
    if let Some(identity) = identity {
        member.extend_from_slice(&identity.0);
    }
    member.extend_from_slice(&(roots.len() as u64).to_le_bytes());
    for root in roots {
        member.extend_from_slice(&root.0);
    }
    ar(&[("lib.rmeta", &member)])
}

/// A System V `ar` archive of `members`, deterministic (no time, no owner, mode 644).
fn ar(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, bytes) in members {
        let header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            format!("{name}/"),
            0,
            0,
            0,
            644,
            bytes.len()
        );
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(bytes);
        if bytes.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

/// The files of `krate`'s modules, by absolute path, sorted.
fn crate_files(reader: &UnitReader, krate: base_db::Crate) -> Vec<String> {
    let db = reader.db();
    let paths = reader.paths();
    let def_map = hir_def::nameres::crate_def_map(db, krate);
    let mut out = Vec::new();
    for (_, module) in def_map.modules(db) {
        if let Some(file) = module.origin.file_id()
            && let Some(path) = paths.get(&file.file_id(db))
        {
            out.push(path);
        }
    }
    out.sort_unstable();
    out.dedup();
    out.into_iter().cloned().collect()
}

/// A dep-info file as rustc (`deps/foo-0123.d`: its outputs, then each source by the name
/// rustc gives it) or cargo (`target/debug/foo.d`: the uplifted file and absolute sources)
/// writes it.
pub(crate) fn dep_info(
    path: &str,
    plan: &blaze_units::query::UnitPlan,
    main: Option<&str>,
    sources: &[String],
    mounted: &Mounted,
) -> String {
    let escape = |it: &str| it.replace(' ', "\\ ");
    let rustc = plan.extra_filename.is_empty() || path.contains(plan.extra_filename.as_str());
    match rustc {
        true => {
            let names: Vec<String> = sources.iter().map(|it| escape(&mounted.name(it))).collect();
            let deps = names.join(" ");
            let mut out = String::new();
            for target in main.into_iter().chain(std::iter::once(path)) {
                out += &format!("{}: {deps}\n\n", escape(target));
            }
            for name in &names {
                out += &format!("{name}:\n");
            }
            out
        }
        false => {
            let target = main.and_then(|it| artifact::uplifted(it, plan)).or(main).unwrap_or(path);
            let deps: Vec<String> = sources.iter().map(|it| escape(it)).collect();
            format!("{}: {}\n", escape(target), deps.join(" "))
        }
    }
}
