//! The real front end behind the keyed driver's seam: [`Command`] implements
//! `blaze_keyed::front::Front` with blaze-unitkey's keys and blaze-front's two tasks, in the
//! snapshots of [`crate::snapshot`].
//!
//! # The text, gathered before the request
//!
//! A key task reads no disk: the command gathers each unit's text first ([`Command::gather`],
//! through a [`Source`], the one place a file is read), so the core here is sans-IO. The gather
//! is blaze-unitkey's (`blaze_unitkey::gather`, the one copy, which the std test and hir-def's
//! census take too): the root, its modules breadth first, the modules macros spell, the
//! fixpoint, the late reads; this module only gives it the unit's cfg, env and reading, the
//! image map and the answers' hash. Every file is read once per process
//! and kept by content: an image is filed under its key ([`Command::image`]), so the library and
//! test units of one crate share each image. The host's seam gives no way to hand over a stored
//! image (`Keyed` carries keys, not image bytes, and the `Front` methods get no store), so a
//! fresh process parses every file of every unit it keys once; that is the gap to the key
//! floor below, and closing it needs an image lookup on the seam.
//!
//! # Late reads
//!
//! `env!` and `option_env!` read the unit's own `Graph` env, then `CARGO_PRIMARY_PACKAGE` (`1`)
//! for a member; nothing else (the process's environment is not read, and the snapshot's
//! process env is empty, so the database answers exactly as the key did). `include!`,
//! `include_str!` and `include_bytes!` read the path relative to the including file's
//! directory, and the file is set in the unit's source root in every snapshot of it. Each
//! answer is one fact of [`Command::late`], the unit's late table, which the request hands back
//! as the task's `Universe::outside`; a surface task that sees another value fails by
//! `blaze_front::surface::LATE_MOVED`. A late read whose argument is not a literal, an include
//! of a missing or non-UTF-8 file, are refused by name at gathering, except in a file reached
//! only through a module a macro spells (`mods`, "Modules a macro call writes"), which the
//! program may not use: there they are [`Deferred`] to the surface task, which refuses them
//! ([`DEFERRED`]) only if the def map has a module in that file.
//!
//! The order: a late table needs the unit's text, and the request needs every unit's late
//! table up front, so `run::check` first looks the certificate up itself, from the sweep's
//! answers alone, and gathers nothing when it holds (every late table is then empty, which a
//! certified request never reads). On a miss every unit is gathered, std's included: the driver
//! keys every unit on a miss, and a surface or body task needs the text of its whole closure.
//! What gathers only the units whose files moved is the driver's, not this module's: key a
//! unit whose files did not move from its kept manifest (`UnitInputs::late` is in it) rather
//! than run its key task. A key task of a unit not gathered is refused by
//! `snapshot::NOT_GATHERED`, so a certificate that held at the command's look and missed at
//! the request's fails loudly rather than keying no text.
//!
//! # What is refused, by name
//!
//! - Before any task, a unit the graph blocked (`graph::Unit::blocked`: a build script, a
//!   crate reading its script's output, a proc macro, a crate expanding one, a device crate),
//!   and any proc-macro or build-script unit: every task of it fails with
//!   `TaskFailed::Refused { what }` ([`BUILD_SCRIPT`], [`SCRIPT_OUTPUT`], [`PROC_MACRO`],
//!   [`USES_PROC_MACRO`], [`DEVICE`]). Expansion and build-script output have no home here yet.
//! - At gathering, a late read whose argument is not a literal (std's
//!   `include!(concat!(env!("OUT_DIR"), ..))`: no script ran, so nothing is in `OUT_DIR`) or an
//!   include of a missing file: `CommandError::Graph` naming the unit, the file and the
//!   macro's argument text.
//! - blaze-front's blockers (`blaze_front::body::front`'s list), as blaze-front refuses them:
//!   1. a body's syntax is its whole file's (the snapshot sets and parses whole files);
//!   3. a crate opened from its pages costs O(its modules) (paid per snapshot, see
//!      `crate::snapshot`);
//!   8. `IMAGE_UNASKED`: a proc macro that traced a read, or an `include!` of a file outside
//!      the unit's tree (`OUT_DIR`), refused by blaze-front's image check;
//!   9. a late read inside a body, refused by the input kind's name
//!      (`blaze_item_hash::reads::input`).
//!
//!   And, from blaze-front's facts, a body that reads a block's def map fails by
//!   `blaze_front::facts::BLOCK_SCOPE`.
//!
//! # Cost against the floors (docs/v2/plan/design/target.md, "floors")
//!
//! - key: floor, one hash of each file and one image lookup when the file is unchanged. Here:
//!   one hash of each file plus, the first time a process sees a file, its parse and image
//!   (O(bytes)); then `unit::key`, O(rows + late reads). A body key is made for every item
//!   of every unit, asked or not (`unit::key` makes them in its one pass over the rows and has
//!   no switch to skip them; the driver asks a body only of an item the unit's surface names,
//!   and nothing here assumes every keyed item is asked: the find index is a lookup built
//!   over the rows, and an item asked with no body row is `NO_BODY`).
//! - surface: floor, the changed file reparsed and its line table. Here: a snapshot opened
//!   (see `crate::snapshot`) and the unit's whole crate lowered from its text to its surface:
//!   O(the unit's items), every time.
//! - body: floor, one body check (a private body edit: 10-30 ms, ratio ~1.2). Here: one body
//!   check in a kept snapshot, plus, once per (worker, unit, universe), the snapshot's opening
//!   and its find index. Within a request the opening is amortized over the unit's bodies.

#![blaze::sans_io]

use core::alloc::Allocator;

use base_db::{ConstEvalBudget, CrateWorkspaceData, Version};
use blaze_core::lock::Lock;
use blaze_format::Aligned;
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use blaze_keyed::{
    driver::{ItemPlace, TaskFailed},
    front::{Front, GivenBody, Keyed, Linked, SpanTables, Universe},
    kind::{self, Body, FileImage, Key, Settings, Surface, ToolId},
    program::RowChange,
    record::{Fact, Owner, StableId},
};
use blaze_unitkey::{image::Reading, read::Image, unit};
use cfg::CfgOptions;
use intern::Symbol;
use syntax::Extensions;

use crate::{
    CommandError,
    graph::{Blocked, CfgAtom, Graph, UnitKind},
    print::Lines,
    snapshot::{self, Holders, Pool, Prepared, Task, refused},
};

/// A unit that is a proc macro.
pub const PROC_MACRO: &str =
    "the no-daemon command: a proc-macro unit (proc macros are not expanded by this command yet)";
/// A unit that is a build script.
pub const BUILD_SCRIPT: &str =
    "the no-daemon command: a build-script unit (build scripts are not run by this command yet)";
/// A unit with a direct proc-macro dependency.
pub const USES_PROC_MACRO: &str =
    "the no-daemon command: a unit that depends on a proc macro (proc-macro expansion is not run by this command yet)";
/// A unit that reads its build script's output.
pub const SCRIPT_OUTPUT: &str =
    "the no-daemon command: a unit that reads its build script's output (`OUT_DIR`, `rustc-cfg`, `rustc-env`; no script runs yet)";
/// A unit built for the GPU device.
pub const DEVICE: &str = "the no-daemon command: a unit built for the GPU device (not lowered by this command)";
/// A unit's `Graph` key is not the front end's crate key.
pub const KEY: &str =
    "the no-daemon command: a unit's `Graph` key is not the front end's crate key of it (`snapshot::crate_keys`)";
/// A unit index past the graph.
pub const NO_UNIT: &str = "the no-daemon command: a unit index past `Graph::units`";
/// A unit's owner is not its crate key's.
pub const OWNER: &str =
    "the no-daemon command: a unit's `Graph` owner is not `Owner::of_subject` of its crate key (`snapshot::crate_keys`)";
/// The host's target has no entry in blaze's target table.
pub const NO_TARGET: &str =
    "the no-daemon command: the host is no target blaze knows (aarch64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu)";
/// blaze's toolchain version does not parse.
pub const NO_TOOLCHAIN: &str = "the no-daemon command: blaze's rustc version is not a version (`blaze_units::stdlib::target::rustc_version`)";
// The gather is blaze-unitkey's, the one copy (rule 30): its rules, its seam and what it
// returns, named here as the command's.
pub use blaze_unitkey::gather::{
    ASKS, Answer, CIRCULAR, Deferred, FileText, Included, MISSING_FILE, MISSING_INCLUDE, MODS, NOT_IMAGED, NOT_LITERAL,
    NOT_UTF8_PATH, Source, SourceError, TWO_MODULE_FILES, UnitText,
};

/// What the gather deferred in a candidate file ([`Deferred`]), which the def map uses.
pub const DEFERRED: &str = "a surface: a file reached only through a module a macro spells holds what the gather could not answer, and the def map uses that file (`front::Deferred`; the file, what and the gather's rule are printed under the unit)";

/// A plant for the deferral's test (`front_tests`): the gather refusing in a candidate file as
/// in any other, or the surface never refusing what was deferred.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DeferPlant {
    None,
    RefuseAtGather,
    NeverRefuse,
}

#[cfg(test)]
std::thread_local! {
    pub(crate) static DEFER_PLANT: core::cell::Cell<DeferPlant> = const { core::cell::Cell::new(DeferPlant::None) };
}

/// The plant in force: [`DeferPlant::None`] outside tests.
fn defer_plant() -> DeferPlant {
    #[cfg(test)]
    let plant = DEFER_PLANT.with(|it| it.get());
    #[cfg(not(test))]
    let plant = {
        let _ = (DeferPlant::RefuseAtGather, DeferPlant::NeverRefuse);
        DeferPlant::None
    };
    plant
}

/// A late read's answer hashed as the request hashes it (blaze-host's `outside`): the gather's
/// [`blaze_unitkey::gather::Digests`].
struct HostDigests;

impl blaze_unitkey::gather::Digests for HostDigests {
    fn var(value: Option<&[u8]>) -> ContentId {
        blaze_host::outside::var_answer(value)
    }

    fn file(bytes: &[u8]) -> ContentId {
        blaze_host::outside::file_answer(bytes)
    }
}

/// The command's images, by content, as the gather files and finds them: one map shared by
/// every unit of the process, so a file two units read is imaged once.
struct Kept<'m, A: Allocator + Clone>(&'m mut HashMap<ContentId, Aligned, A>);

impl<A: Allocator + Clone> blaze_unitkey::gather::Images for Kept<'_, A> {
    fn get(&self, key: &Key<FileImage>) -> Option<&[u8]> {
        self.0.get(&key.id()).map(|image| &image[..])
    }

    fn put(&mut self, key: Key<FileImage>, record: &[u8]) {
        self.0.insert(key.id(), Aligned::copy_from(record));
    }
}

/// A row of a unit's env as the gather reads it.
fn env_pair<A: Allocator>(row: &(Box<str, A>, Box<str, A>)) -> (&[u8], &[u8]) {
    (row.0.as_bytes(), row.1.as_bytes())
}

/// The front end of the no-daemon command over one graph.
pub struct Command<'g, A: Allocator + Clone> {
    pub(crate) graph: &'g Graph<A>,
    pub(crate) alloc: A,
    pub(crate) cfgs: Vec<CfgOptions, A>,
    pub(crate) prepared: Vec<Prepared<A>, A>,
    pub(crate) workspace: triomphe::Arc<CrateWorkspaceData>,
    pub(crate) holders: Holders<A>,
    pools: Vec<Pool<A>, A>,
    texts: Vec<Option<UnitText<A>>, A>,
    images: HashMap<ContentId, Aligned, A>,
    /// What refused tasks said beyond their rules, one line each with the unit it is about, for
    /// the summary to print under that unit ([`Command::told`]): the file, the item or
    /// declaration at `file:line`, the rule, which `TaskFailed` (a `&'static str`) cannot carry.
    /// Taken once per refusal ([`Command::tell`]); never on a task that succeeds.
    told: Lock<Vec<(u32, Box<str, A>), A>>,
    /// The programs a `build` or `run` links, by the driver's program number
    /// ([`Command::add_program`]), each with its pool of plan snapshots
    /// ([`snapshot::lease_plan`]). Empty for `check`.
    programs: Vec<(Linking<A>, Pool<A>), A>,
}

/// A program the command links, as the driver numbers it: what `program.rs`'s plan, code and
/// link functions are given beside the task's own parts.
pub struct Linking<A: Allocator + Clone> {
    /// The program's unit in the graph: what `program.rs` opens snapshots of and tells refusals
    /// under, which it names `program`.
    pub unit: u32,
    /// What its code is made under: the toolchain the command found for the graph's target,
    /// and the program's profile (`graph::Program::profile`).
    pub policy: blaze_codegen::plan::Policy,
    pub settings: crate::code::FrontSettings,
    /// The image's name for the linker (its kept state is verified under it), its file name
    /// and how it is linked ([`blaze_link::step`]).
    pub name: Box<str, A>,
    pub to: blaze_core::FileName,
    pub output: blaze_link::Output,
}

/// A program task's number past [`Command::add_program`]'s.
/// A key task's unit has two files (two images) a program prints under one name: a span read
/// of one would answer for the other (rule 21). One file under two module paths is one name and
/// one image, and is no clash.
pub const TWO_FILES_ONE_NAME: &str = "a key task: two files of the unit print one name (`program::printed_name_id`; rule 21)";
pub const NO_PROGRAM: &str ="a program task: its number is no program the command links (`Command::add_program`)";

/// The rule a blocked unit's tasks are refused by.
fn blocked_rule(blocked: Blocked) -> &'static str {
    match blocked {
        Blocked::BuildScript => BUILD_SCRIPT,
        Blocked::ScriptOutput => SCRIPT_OUTPUT,
        Blocked::ProcMacro => PROC_MACRO,
        Blocked::Expansion => USES_PROC_MACRO,
        Blocked::Device => DEVICE,
    }
}

/// The host's target tuple.
pub(crate) fn host() -> Option<&'static str> {
    match (cfg!(target_arch = "aarch64"), cfg!(target_arch = "x86_64"), cfg!(target_os = "macos"), cfg!(target_os = "linux")) {
        (true, _, true, _) => Some("aarch64-apple-darwin"),
        (_, true, _, true) => Some("x86_64-unknown-linux-gnu"),
        (true, _, _, true) => Some("aarch64-unknown-linux-gnu"),
        _ => None,
    }
}

/// The host's target data: [`workspace_of`] the host's triple. Kept only until `graph::own`
/// calls [`workspace_of`] with `Graph::triple`; then it goes.
pub(crate) fn workspace() -> Result<CrateWorkspaceData, CommandError> {
    workspace_of(host().ok_or(CommandError::Host { rule: NO_TARGET })?)
}

/// What every crate of a graph built for `triple` shares: the triple's target data (blaze's
/// table, `blaze_units::stdlib::target`), blaze's toolchain version, the default const-eval
/// budget, no trusted crates. A triple the table has no entry for is refused ([`NO_TARGET`]).
pub fn workspace_of(triple: &str) -> Result<CrateWorkspaceData, CommandError> {
    use blaze_units::stdlib::target;
    let host_error = |rule| CommandError::Host { rule };
    let data_layout = target::data_layout(triple).map_err(|_| host_error(NO_TARGET))?;
    let has_thumb_interworking = target::has_thumb_interworking(triple).map_err(|_| host_error(NO_TARGET))?;
    let version = target::rustc_version();
    let number = version.split(' ').next().unwrap_or(version);
    let toolchain = Version::parse(number).map_err(|_| host_error(NO_TOOLCHAIN))?;
    let arch = match triple.split('-').next() {
        Some("wasm32") => base_db::target::Arch::Wasm32,
        Some("wasm64") => base_db::target::Arch::Wasm64,
        _ => base_db::target::Arch::Other,
    };
    Ok(CrateWorkspaceData {
        target: Ok(base_db::target::TargetData {
            tuple: Symbol::intern(triple),
            data_layout: data_layout.into_boxed_str(),
            arch,
            has_thumb_interworking,
        }),
        toolchain: Some(toolchain),
        const_eval: ConstEvalBudget::default(),
        trusted: Box::default(),
    })
}

pub(crate) fn cfg_of<A: Allocator + Clone>(atoms: &[CfgAtom<A>]) -> CfgOptions {
    let mut cfg = CfgOptions::default();
    for atom in atoms {
        match atom {
            CfgAtom::Flag(name) => cfg.insert_atom(Symbol::intern(name)),
            CfgAtom::KeyValue { key, value } => cfg.insert_key_value(Symbol::intern(key), Symbol::intern(value)),
        }
    }
    cfg
}

impl<'g, A: Allocator + Clone> Command<'g, A> {
    /// Prepares every unit of `graph`: its cfg, crate key, tool, settings, closure and what is
    /// refused of it. Refuses a graph whose owners are not its crate keys' ([`OWNER`]) or whose
    /// dependencies are not below their dependents.
    pub fn new(graph: &'g Graph<A>, alloc: A) -> Result<Command<'g, A>, CommandError> {
        let workspace = triomphe::Arc::new(workspace_of(&graph.triple)?);
        let count = graph.units.len();
        let mut cfgs = Vec::with_capacity_in(count, alloc.clone());
        cfgs.extend(graph.units.iter().map(|unit| cfg_of(&unit.cfg)));
        // The graph's keys are checked, not trusted: one hash per unit.
        let keys = snapshot::crate_keys(graph, &workspace, &cfgs, alloc.clone())?;
        let mut prepared = Vec::with_capacity_in(count, alloc.clone());
        let mut seen = Vec::with_capacity_in(count, alloc.clone());
        for (at, unit) in graph.units.iter().enumerate() {
            let crate_key = unit.key;
            if keys[at] != crate_key {
                return Err(CommandError::Unit { unit: at as u32, rule: KEY });
            }
            if unit.owner != Owner::of_subject(&crate_key) {
                return Err(CommandError::Unit { unit: at as u32, rule: OWNER });
            }
            // The closure: a walk down the dependencies, read out ascending.
            seen.clear();
            seen.resize(count, false);
            seen[at] = true;
            for down in (0..=at).rev() {
                if seen[down] {
                    for dep in graph.units[down].deps.iter() {
                        if dep.unit as usize >= down {
                            return Err(CommandError::Unit { unit: down as u32, rule: snapshot::DEP_ORDER });
                        }
                        seen[dep.unit as usize] = true;
                    }
                }
            }
            let mut closure = Vec::new_in(alloc.clone());
            closure.extend((0..=at as u32).filter(|&it| seen[it as usize]));
            // What the graph blocked (`Graph::units[u].blocked`, the one home of it); a kind the
            // graph should have blocked and did not is refused all the same.
            let refused = match (unit.blocked, unit.kind) {
                (Some(blocked), _) => Some(blocked_rule(blocked)),
                (None, UnitKind::ProcMacro) => Some(PROC_MACRO),
                (None, UnitKind::BuildScript) => Some(BUILD_SCRIPT),
                (None, _) => None,
            };
            // The unit's own profile settings and its cfg (sorted, each atom once: the graph's
            // invariant), so a key covers the cfg its text was read under by `Settings` and
            // needs nothing of the crate key for it (rule 21). The crate key still holds the
            // cfg today (`base_db::crate_item_key`); once it does not, a script's printed cfg
            // moves these settings and no crate key.
            //
            // Nothing of the graph's: `Graph::settings` holds the command (`check` or `build`)
            // and every other unit's profile, and a unit's manifest, surface and bodies are
            // functions of none of them (rule 21: the key holds the inputs and nothing else;
            // rule 77). A body's steps are in its key by `Steps`; what a link makes of the
            // goal and the profile is in the plan and code keys (`code::Policy`, the program's
            // profile) and the certificate (`Graph::identity`), never here.
            let mut hasher = Hasher::new("blaze-command unit settings 4");
            hasher.id(&unit.settings);
            for atom in unit.cfg.iter() {
                hasher.part(atom.key().as_bytes());
                match atom.value() {
                    None => hasher.part(&[0]),
                    Some(value) => hasher.part(&[1]).part(value.as_bytes()),
                };
            }
            let settings = Settings(hasher.finish());
            prepared.push(Prepared {
                crate_key,
                tool: ToolId { tool: graph.tool, target: graph.target, namespace: unit.namespace },
                settings,
                reading: Reading { edition: unit.edition, extensions: Extensions::NONE },
                closure,
                refused,
            });
        }
        let mut pools = Vec::with_capacity_in(count, alloc.clone());
        pools.extend((0..count).map(|_| Pool::new(alloc.clone())));
        let mut texts = Vec::with_capacity_in(count, alloc.clone());
        texts.extend((0..count).map(|_| None));
        Ok(Command {
            graph,
            cfgs,
            prepared,
            workspace,
            holders: Holders::new(alloc.clone()),
            pools,
            texts,
            images: HashMap::new_in(alloc.clone()),
            told: Lock::new("command.front.told", Vec::new_in(alloc.clone())),
            programs: Vec::new_in(alloc.clone()),
            alloc,
        })
    }

    /// Adds a program to link, with a plan pool keeping at most `kept` snapshots (the workers
    /// that run its tasks at once): its number, the driver's (`ProgramPlan`'s index and
    /// `ProgramWalker::add_program`'s, both in this order).
    pub fn add_program(&mut self, linking: Linking<A>, kept: usize) -> u32 {
        self.programs.push((linking, Pool::with_cap(kept, self.alloc.clone())));
        (self.programs.len() - 1) as u32
    }

    /// Program `program`'s linking, or [`NO_PROGRAM`].
    pub fn linking(&self, program: u32) -> Result<&Linking<A>, TaskFailed> {
        self.programs.get(program as usize).map(|(linking, _)| linking).ok_or(refused(NO_PROGRAM))
    }

    /// The plan pool of the program whose unit is `unit`.
    pub(crate) fn plan_pool(&self, unit: u32) -> Option<&Pool<A>> {
        self.programs.iter().find(|(linking, _)| linking.unit == unit).map(|(_, pool)| pool)
    }

    /// What refused tasks said beyond their rules, in the order they were refused, copied into
    /// `alloc`: one line each with its unit, every item at `file:line`.
    pub fn told<B: Allocator + Clone>(&self, alloc: B) -> Vec<(u32, Box<str, B>), B> {
        let told = self.told.lock();
        let mut out = Vec::with_capacity_in(told.len(), alloc.clone());
        out.extend(told.iter().map(|(unit, line)| (*unit, crate::boxed(line, alloc.clone()))));
        out
    }

    /// Any task's refusal of unit `unit`, said in one line (rule 19: the file, the item or
    /// declaration at `file:line`, the rule), kept for the summary under that unit. A task
    /// calls it once, where it returns its `TaskFailed`.
    pub fn tell(&self, unit: u32, line: impl core::fmt::Display) {
        let mut text = blaze_mem::String::new_in(self.alloc.clone());
        blaze_mem::text::format_into(&mut text, format_args!("{line}"));
        self.told.lock().push((unit, text.into_boxed_str()));
    }

    /// Item place `at` of unit `unit` (a [`TaskFailed::TwoItems`] place), as a person reads it:
    /// `path:line` through the file's image (its item row's first token, the image's line
    /// table), in `alloc`. A place the gathered text cannot resolve says what it has: the
    /// file's path and the item row, or the file's index in the unit.
    pub fn place<B: Allocator + Clone>(&self, unit: u32, at: ItemPlace, alloc: B) -> Box<str, B> {
        let mut out = blaze_mem::String::new_in(alloc);
        let Some(file) = self.text(unit).and_then(|text| text.files.get(at.file as usize)) else {
            blaze_mem::text::format_into(&mut out, format_args!("file {} of the unit, item row {}", at.file, at.row));
            return out.into_boxed_str();
        };
        let line = self
            .image(&file.image)
            .and_then(|bytes| Image::open(bytes, &file.image).ok())
            .and_then(|image| {
                let row = image.items().get(at.row as usize)?;
                let token = image.tokens().get(row.tokens.at as usize)?;
                Some(Lines { starts: image.lines() }.at(token.start).0)
            });
        let shown = String::from_utf8_lossy(&file.path);
        match line {
            Some(line) => blaze_mem::text::format_into(&mut out, format_args!("{shown}:{line}")),
            None => blaze_mem::text::format_into(&mut out, format_args!("{shown}, item row {}", at.row)),
        }
        out.into_boxed_str()
    }

    /// The unit's gathered text, `None` before [`Command::gather`].
    pub fn text(&self, unit: u32) -> Option<&UnitText<A>> {
        self.texts.get(unit as usize)?.as_ref()
    }

    /// A gathered file image, 16-aligned, by its key.
    pub fn image(&self, key: &Key<FileImage>) -> Option<&[u8]> {
        self.images.get(&key.id()).map(|image| &image[..])
    }

    /// The unit's late table: what the request hands back as its tasks' `Universe::outside`
    /// (and its `Request::late` row). Empty before [`Command::gather`].
    pub fn late(&self, unit: u32) -> &[Fact] {
        self.text(unit).map_or(&[], |text| &text.late[..])
    }

    /// The process ends: nothing the command holds is torn down piece by piece.
    pub fn exit(self) {
        core::mem::forget(self);
    }

    fn check(&self, unit: u32) -> Result<&Prepared<A>, TaskFailed> {
        let prepared = self.prepared.get(unit as usize).ok_or(refused(NO_UNIT))?;
        match prepared.refused {
            Some(what) => Err(refused(what)),
            None => Ok(prepared),
        }
    }

    /// Gathers unit `unit`'s text through `source` by the one gather
    /// ([`blaze_unitkey::gather::gather`]), its images filed in the command's map. Once per
    /// unit; a second call does nothing. A refusal names the unit, the file and what in it.
    pub fn gather<S: Source>(&mut self, unit: u32, source: &mut S) -> Result<(), CommandError> {
        let at = unit as usize;
        if self.texts.get(at).ok_or(CommandError::Unit { unit, rule: NO_UNIT })?.is_some() {
            return Ok(());
        }
        let graph = self.graph;
        let named = &graph.units[at];
        let prepared = &self.prepared[at];
        let input = blaze_unitkey::gather::Gather {
            tool: prepared.tool,
            reading: prepared.reading,
            cfg: &self.cfgs[at],
            root: named.root.as_bytes(),
            env: &named.env,
            pair: env_pair::<A>,
            member: named.member,
            defer: defer_plant() != DeferPlant::RefuseAtGather,
        };
        let gathered = blaze_unitkey::gather::gather::<_, _, HostDigests, _, _>(&input, source, &mut Kept(&mut self.images), self.alloc.clone());
        let text = gathered.map_err(|error| {
            let what = match error.what.is_empty() {
                true => String::from_utf8_lossy(&error.path).into_owned(),
                false => format!("{} in {}", String::from_utf8_lossy(&error.what), String::from_utf8_lossy(&error.path)),
            };
            CommandError::Graph {
                unit: format!("`{}` ({})", named.name, named.package).into_boxed_str(),
                rule: error.rule,
                what: what.into_boxed_str(),
            }
        })?;
        self.texts[at] = Some(text);
        Ok(())
    }

}

/// The rule a key error breaks.
fn key_rule(error: &blaze_unitkey::KeyError) -> &'static str {
    use blaze_unitkey::KeyError;
    match error {
        KeyError::Image { .. } => "a key task: a file image does not open under its key (`KeyError::Image`)",
        KeyError::Unanswered { .. } => "a key task: a late read has no answer (`KeyError::Unanswered`)",
        KeyError::Answers => "a key task: the late answers are not sorted or one is given twice (`KeyError::Answers`)",
        KeyError::Pool { .. } => "a key task: a file image is not one blaze-unitkey wrote (`KeyError::Pool`)",
        // Returned as `TaskFailed::TwoItems` with both places by the key task, never by name.
        KeyError::TwoItems { .. } => "a key task: two items of the unit have one stable id (`KeyError::TwoItems`)",
        #[allow(unreachable_patterns)]
        _ => "a key task: the unit's keys were refused (`blaze_unitkey::KeyError`)",
    }
}

impl<A: Allocator + Clone + Send + Sync> Front for Command<'_, A> {
    /// The unit's keys from its gathered text: one `unit::key`, its parts moved into `Keyed`.
    fn key<F: Allocator + Clone>(&self, unit: u32, frame: F) -> Result<Keyed<F>, TaskFailed> {
        let prepared = self.check(unit)?;
        let text = self.text(unit).ok_or(refused(snapshot::NOT_GATHERED))?;
        let mut files = Vec::with_capacity_in(text.files.len(), frame.clone());
        // One printed name per file, by the one function the span task's facts use
        // (`program::printed_name_id`), zero for a file the program prints no name for: in the
        // manifest's key, so a moved checkout or a new std commit rekeys the unit's span reads.
        let mut names = Vec::with_capacity_in(text.files.len(), frame.clone());
        for file in text.files.iter() {
            let image = self.image(&file.image).ok_or(refused(snapshot::NOT_GATHERED))?;
            files.push(unit::UnitFile { key: file.image, image, module: &file.module });
            names.push(crate::program::printed_name_id(self, unit, &file.path).unwrap_or(ContentId([0; 32])));
        }
        // One file may be two modules (two `#[path]`s naming it: std's `sys::pal::{hermit, wasi,
        // teeos}::time` are each `sys/pal/unix/time.rs`), two entries of one name and one image:
        // the driver keeps it as one span file. Two images under one name would be two files a
        // program prints alike: refused here, both told by their gathered paths and image keys.
        let mut by_name = Vec::with_capacity_in(names.len(), frame.clone());
        by_name.extend((0..names.len()).filter(|&at| names[at] != ContentId([0; 32])));
        by_name.sort_unstable_by_key(|&at| (names[at], text.files[at].image));
        let clash = by_name.windows(2).find(|pair| names[pair[0]] == names[pair[1]] && text.files[pair[0]].image != text.files[pair[1]].image);
        if let Some(&[at, other]) = clash {
            let (first, second) = (&text.files[at], &text.files[other]);
            self.tell(
                unit,
                format_args!(
                    "files {at} `{}` (image {:?}) and {other} `{}` (image {:?}) print one name (id {:?}): {TWO_FILES_ONE_NAME}",
                    String::from_utf8_lossy(&first.path),
                    first.image,
                    String::from_utf8_lossy(&second.path),
                    second.image,
                    names[at],
                ),
            );
            return Err(refused(TWO_FILES_ONE_NAME));
        }
        let mut answers = Vec::with_capacity_in(text.answers.len(), frame.clone());
        answers.extend(text.answers.iter().map(|answer| unit::LateAnswer {
            file: answer.file,
            late: answer.late,
            name: &answer.name,
            value: answer.value,
        }));
        let input = unit::Unit {
            tool: prepared.tool,
            settings: prepared.settings,
            unit: prepared.crate_key,
            cfg: &self.cfgs[unit as usize],
            files: &files,
            answers: &answers,
            names: &names,
        };
        // Two items under one key are the variant's to say (both places, which the summary
        // prints at file:line with `Command::place`); no told line repeats them (rule 30).
        // Keying runs no query of hir-def or hir-ty, so no typed refusal unwinds out of it: it
        // is not run under `Command::named`.
        let keys = unit::key(&input, frame).map_err(|error| match error {
            blaze_unitkey::KeyError::TwoItems { first, second, .. } => TaskFailed::TwoItems {
                first: ItemPlace { file: first.file, row: first.row },
                second: ItemPlace { file: second.file, row: second.row },
            },
            other => refused(key_rule(&other)),
        })?;
        Ok(Keyed { manifest: keys.manifest, surface: keys.surface, files: keys.files, items: keys.items, names })
    }

    /// The unit's surface, in a snapshot whose own crate is its text and every dependency its
    /// surface.
    fn surface(&self, unit: u32, key: Key<Surface>, universe: Universe<'_>) -> Result<Aligned, TaskFailed> {
        self.check(unit)?;
        let text = self.text(unit).ok_or(refused(snapshot::NOT_GATHERED))?;
        self.named(unit, || {
            let opened = snapshot::open(self, unit, &universe, Task::Surface)?;
            // What the gather deferred refuses the unit only now, and only if the def map uses
            // its file: first, since such a file's expansions read what the late table has no
            // fact of, which the surface would refuse by a rule that does not name the cause.
            // The def map is a query of the snapshot: the surface reuses it.
            if self.deferred_used(unit, text, &opened) {
                return Err(refused(DEFERRED));
            }
            // A refusal's own line (the file, the declaration at file:line, the rule) is kept
            // under the unit.
            let out = blaze_front::surface::surface_telling(&opened.db, opened.own, key, universe, &opened.find, |line| {
                self.tell(unit, line)
            });
            #[cfg(test)]
            snapshot::count_resolved(&opened.db);
            out
        })
    }

    /// One body, in a kept snapshot of the task's universe, or a new one. A snapshot a task
    /// unwound out of is dropped, never put back.
    fn body(&self, unit: u32, item: StableId, key: Key<Body>, universe: Universe<'_>) -> Result<Aligned, TaskFailed> {
        self.body_of(unit, item, key, kind::Steps::Checked, universe)
    }

    /// [`Front::body`] running `steps`: a MIR-alone body for a unit no request checks, under
    /// the key the driver made with [`kind::Steps::key`].
    fn body_of(&self, unit: u32, item: StableId, key: Key<Body>, steps: kind::Steps, universe: Universe<'_>) -> Result<Aligned, TaskFailed> {
        self.check(unit)?;
        self.named(unit, || self.body_in(unit, item, key, steps, universe))
    }

    /// Program `program`'s plan record named `key`: `program::plan` under its policy and
    /// settings, given the program's unit where `program.rs` names a program (it opens
    /// snapshots of that unit and tells refusals under it).
    fn plan(
        &self,
        program: u32,
        ask: &[u8],
        key: Key<kind::PlanRecord>,
        bodies: &[GivenBody<'_>],
        universe: Universe<'_>,
    ) -> Result<Aligned, TaskFailed> {
        let at = self.linking(program)?;
        crate::program::plan(self, at.unit, ask, key, bodies, universe, &at.policy, at.settings)
    }

    /// Program `program`'s code record for `key`: `program::code`, as [`Front::plan`] calls
    /// `program::plan`.
    fn code(
        &self,
        program: u32,
        key: Key<kind::CodeRecord>,
        plan: &[u8],
        callees: &[(Key<kind::PlanRecord>, &[u8])],
        bodies: &[GivenBody<'_>],
        universe: Universe<'_>,
    ) -> Result<Aligned, TaskFailed> {
        let at = self.linking(program)?;
        crate::program::code(self, at.unit, key, plan, callees, bodies, universe, &at.policy, at.settings)
    }

    /// One link step of program `program`: `program::link` with the program's name, file and
    /// output, returned as it is (the linker's new state and the patch from the old image to
    /// the new). It writes nothing: the host makes the image a store file from the patch
    /// ([`crate::images::HomeImages`]) before the kept manifest naming it is filed, and places
    /// the path last (blaze-keyed `driver/program.rs`, "Durability").
    fn link<F: Allocator + Clone>(
        &self,
        program: u32,
        changes: &[RowChange],
        code: &[&[u8]],
        plans: &[(Key<kind::PlanRecord>, &[u8])],
        spans: &[(ContentId, &[u8])],
        payload: &[u8],
        frame: F,
    ) -> Result<Linked<F>, TaskFailed> {
        let at = self.linking(program)?;
        crate::program::link(self, at.unit, &at.policy, &at.name, at.to.clone(), at.output.clone(), changes, code, plans, spans, payload, frame)
    }

    /// The span tables of `owner` of unit `unit`: `program::spans`, under `named` so a typed
    /// refusal from a query is told by the unit.
    fn spans<F: Allocator + Clone>(&self, unit: u32, owner: StableId, universe: Universe<'_>, frame: F) -> Result<SpanTables<F>, TaskFailed> {
        self.check(unit)?;
        self.named(unit, || crate::program::spans(self, unit, owner, &universe, frame))
    }
}

/// What a given crate's frozen part that does not read in a snapshot is refused by, by the
/// part (`hir_def::nameres::frozen::Unreadable::kind`).
pub const UNREADABLE_DEF_MAP: &str = "a snapshot: a given crate's frozen def map (its surface's pages) does not read in this snapshot (`hir_def::nameres::frozen::Unreadable`; the crate and why are in the error log)";
pub const UNREADABLE_LANG_ITEMS: &str = "a snapshot: a given crate's frozen lang items do not read in this snapshot (`hir_def::nameres::frozen::Unreadable`; the crate and why are in the error log)";
pub const UNREADABLE_DIAGNOSTIC_ITEMS: &str = "a snapshot: a given crate's frozen diagnostic items do not read in this snapshot (`hir_def::nameres::frozen::Unreadable`; the crate and why are in the error log)";
pub const UNREADABLE_OTHER: &str = "a snapshot: a given crate's frozen part does not read in this snapshot (`hir_def::nameres::frozen::Unreadable`; the crate, the part and why are in the error log)";

#[cfg(test)]
std::thread_local! {
    /// The last `Unreadable` a task of this thread was refused by, whole (crate, part, why):
    /// what a test prints when a task is refused.
    pub(crate) static LAST_UNREADABLE: core::cell::RefCell<Option<String>> = const { core::cell::RefCell::new(None) };
}

/// Whether a panic's payload is one of the typed refusals a task of the command turns into its
/// failure ([`Command::named`]): hir-def's `Unreadable` (a given crate's frozen part, a keyed
/// file's text) and hir-ty's `Unserved` (a body the item store does not serve). The command's
/// panic hook ([`crate::run::quiet_refusals`]) prints nothing for these: the reason is told
/// under the unit, and a refusal is no crash.
// dyn: a panic's payload, read once
pub(crate) fn is_refusal(payload: &(dyn std::any::Any + Send)) -> bool {
    hir_def::nameres::frozen::Unreadable::of_payload(payload).is_some()
        || hir_ty::body_store::Unserved::of_payload(payload).is_some()
        || base_db::NoText::of_payload(payload).is_some()
}

/// The rule a task fails by when it asked the parse of a file that has no text in its
/// snapshot (`base_db::NoText`): a file of a crate given by its address is never parsed, so
/// whatever asked must read the crate's kept entries instead. The file is told under the unit.
pub const NO_TEXT: &str =
    "a snapshot: the parse of a file with no text here was asked (`base_db::NoText`: a file of a crate given by its address is not parsed); the file is told under the unit";

impl<A: Allocator + Clone + Send + Sync> Command<'_, A> {
    /// Runs a task of unit `unit`, turning a typed refusal it unwinds with into its failure,
    /// the whole reason told under the unit ([`Command::tell`]):
    ///
    /// - hir-def's `Unreadable` (frozen bytes, or a keyed file's text, that do not read): the
    ///   reason as `hir_def::nameres::stopped` renders it (the crate, the part, why, the file),
    ///   the failure the rule of the part;
    /// - hir-ty's `Unserved`: its own words, the failure `blaze_front::body::mir::failed`'s (a
    ///   body with no record given is the driver's `NoBody`, which it makes and reruns).
    ///
    /// Every other panic goes on unwinding to the host, which fails the one task (rule 20).
    fn named<T>(&self, unit: u32, task: impl FnOnce() -> Result<T, TaskFailed>) -> Result<T, TaskFailed> {
        let payload = match std::panic::catch_unwind(core::panic::AssertUnwindSafe(task)) {
            Ok(out) => return out,
            Err(payload) => payload,
        };
        match self.stop_of(unit, &*payload) {
            Some(failed) => Err(failed),
            None => std::panic::resume_unwind(payload),
        }
    }

    /// The typed stop a task unwound with, told under `unit` as [`Command::named`]'s doc says,
    /// and the failure its rule names; `None` for any other panic, which the caller passes on
    /// to the host (rule 20). The one home of the payload types a task's unwind may be a
    /// refusal of (rule 30): [`Command::named`] for unit tasks, `program::caught` for a
    /// program's plan, code and link tasks.
    pub(crate) fn stop_of(&self, unit: u32, payload: &(dyn std::any::Any + Send)) -> Option<TaskFailed> {
        use hir_def::nameres::frozen::Unreadable;
        use hir_ty::body_store::Unserved;
        if let Some(unreadable) = Unreadable::of_payload(payload) {
            let rule = match unreadable.kind {
                "def map" | "def map pages" => UNREADABLE_DEF_MAP,
                "lang items" => UNREADABLE_LANG_ITEMS,
                "diagnostic items" => UNREADABLE_DIAGNOSTIC_ITEMS,
                _ => UNREADABLE_OTHER,
            };
            // `stopped` renders every typed stop of name resolution, an `Unreadable` included.
            if let Some(said) = hir_def::nameres::stopped(payload) {
                #[cfg(test)]
                LAST_UNREADABLE.with(|it| *it.borrow_mut() = Some(said.clone()));
                self.tell(unit, &said);
            }
            return Some(refused(rule));
        }
        if let Some(no_text) = base_db::NoText::of_payload(payload) {
            self.tell(unit, no_text);
            return Some(refused(NO_TEXT));
        }
        if let Some(unserved) = Unserved::of_payload(payload) {
            let failed = blaze_front::body::mir::failed(unserved);
            // A missing record is the driver's to make: no refusal to tell.
            if !matches!(failed, TaskFailed::NoBody { .. }) {
                self.tell(unit, unserved);
            }
            return Some(failed);
        }
        None
    }
}

impl<'g, A: Allocator + Clone + Send + Sync> Command<'g, A> {
    /// Whether the def map of `opened`, a surface snapshot of `unit`, has a module in a file
    /// the gather deferred something in ([`Deferred`]); each such is told under the unit (the
    /// file, what, the gather's rule). A deferred file with no id in the snapshot counts as
    /// used: nothing shows it is not.
    fn deferred_used<B: Allocator>(&self, unit: u32, text: &UnitText<A>, opened: &snapshot::Snapshot<B>) -> bool {
        if text.deferred.is_empty() || defer_plant() == DeferPlant::NeverRefuse {
            return false;
        }
        let map = hir_def::nameres::crate_def_map(&opened.db, opened.own);
        let mut used = false;
        for deferred in text.deferred.iter() {
            let uses = match opened.own_files.get(deferred.file as usize).copied().flatten() {
                Some(file) => map.modules_for_file(&opened.db, file).next().is_some(),
                None => true,
            };
            if !uses {
                continue;
            }
            used = true;
            let path = text
                .files
                .get(deferred.file as usize)
                .map_or(std::borrow::Cow::Borrowed("a file of the unit"), |file| String::from_utf8_lossy(&file.path));
            self.tell(
                unit,
                format_args!("{path}: {}: {} (rule: {DEFERRED})", String::from_utf8_lossy(&deferred.what), deferred.rule),
            );
        }
        used
    }

    fn body_in(
        &self,
        unit: u32,
        item: StableId,
        key: Key<Body>,
        steps: kind::Steps,
        universe: Universe<'_>,
    ) -> Result<Aligned, TaskFailed> {
        let digest = {
            let tables = snapshot::given(self, unit, &universe, Task::Body, self.alloc.clone())?;
            let mut addresses = Vec::with_capacity_in(tables.len(), self.alloc.clone());
            addresses.extend(tables.iter().map(|&(member, at, _)| (member, at)));
            snapshot::universe_digest(&addresses, &universe)
        };
        let pool = &self.pools[unit as usize];
        let opened = match pool.take(digest) {
            Some(opened) => opened,
            None => snapshot::open(self, unit, &universe, Task::Body)?,
        };
        let out = {
            let traces = opened.installed.as_ref().ok_or(refused(snapshot::NO_STORE))?;
            let find = |item: StableId| opened.find.get(item);
            // A refused row's readers, told under the unit as a surface's refusals are.
            let told = |line: &str| self.tell(unit, line);
            let front = blaze_front::body::front::Front { db: &opened.db, traces, find: &find, told: &told };
            blaze_front::body::body_with(&front, item, key, universe, steps)
        };
        #[cfg(test)]
        snapshot::count_resolved(&opened.db);
        pool.put(opened, self.alloc.clone());
        out
    }
}
