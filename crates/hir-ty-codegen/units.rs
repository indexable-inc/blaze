//! Codegen units: the functions of a build of a whole program compiled by LLVM many to a
//! module, as rustc compiles the codegen units of a crate (issue #176). A build compiles in
//! units with `BLAZE_UNITS=1`; otherwise each function is a module of its own.
//!
//! A compile of one function is a module of its own, and each imports what it calls
//! ([`super::imports`]): two that call one function each link its body in and optimize it.
//! A unit is a module of many: what they import is linked in and optimized once for all of
//! them, and one calls another in the module, which LLVM may build in.
//!
//! The functions are found before any is compiled. A function's IR is built with LLVM's part
//! left for later ([`super::Detach::Unit`]), and what it imports is chosen, as its compile
//! would choose it: what it calls that it does not import, what those imports call that is
//! not imported in turn, and every function it takes the address of, is found next
//! ([`calls_after_imports`]). A unit links its imports in as `linkonce_odr`, and an import
//! LLVM left a call of is kept: its code is the code of the function it is of, unless that
//! function is compiled as one of its own ([`llvm::emit_unit`]). So what the code of a unit
//! calls is known before LLVM runs, and the functions are found in one round. The functions
//! found are cut into units in the order they were found in, each of about [`UNIT`]
//! instructions, and the units are compiled on every core as they are cut, the largest
//! first. The units of a build are the same in every build of the same program: they are
//! cut from what was found, in the order it was found in, and not from the order the
//! threads finished in.
//!
//! It is not the default, as it builds no faster and helix runs slower (16 cores, 2026-10-24,
//! against compiles of one function each): helix in 71.1 s against 69.7 s on x86-64, running
//! at 1.28x of cargo's release build against 1.21x, and ripgrep, nushell and bun within 3% of
//! the build time either way but nushell on AArch64, 6% faster. Units link in half as many
//! imported instructions as the compiles of one function do (8.8 M against 17.8 M for
//! helix), and LLVM's time summed over the cores is 28% less. But a function some unit
//! imports and keeps is compiled again in every other unit that keeps it (49 k kept for
//! 50.6 k functions), finding the functions from IR compiles a quarter more than finding them
//! from code does (50.6 k against 40.6 k), and the cores wait on the waves the functions are
//! found in: helix's units keep them busy three quarters of the time. Putting a unit's
//! functions through ThinLTO's pre-link passes before their imports are linked in, as rustc
//! does, took 3 s off ripgrep's optimization and ran helix at 1.35x. Either way LLVM
//! takes about four times rustc's time for code of the same size: ripgrep's 8.7 k functions
//! and 2.9 MB of text take 100 s summed over the cores, where rustc's 9.3 k and 2.8 MB take
//! about 25 s, which `docs/research/codegen-units.md` goes into.
//!
//! The code of each function of a unit is read out of the unit's object on its own
//! ([`super::llvm::emit_unit`]), with what it refers to, and is put in the object as the code
//! of a compile of that function alone is. A function a unit has no code for is compiled
//! alone, as every function of a build that takes edits is: the database's code of it
//! (`compiled_root`, `compiled_instance`), whose module is the same one-function unit.

use std::{
    collections::BinaryHeap,
    sync::{Condvar, LazyLock, Mutex, atomic::Ordering},
};

use cranelift_codegen::ir;
use hir_def::FunctionId;
use hir_ty::{db::HirDatabase, traits::StoredParamEnvAndCrate};
use rustc_hash::{FxHashMap, FxHashSet};

use super::{
    Backend, CompiledFunction, Detach, Instance, InstanceDef, ObjectCodegen, ObjectFormat,
    ParallelWork, Pending, Produced, Rejected, Result, SymbolResolver,
    backend::{self, Tally},
    cached, compiled_instance, compiled_root,
    fragment::{self, DataRequest, FuncRequest, FuncTarget, Recorder},
    home, imports, instance_rows, llvm, root_instance, root_rows, stable,
    tier::{self, Kind},
};

/// About how many instructions of IR a unit has: a function larger than [`super::LARGE`] is
/// a unit of its own, and a build too small to give every core a few units of this size is
/// cut finer, down to [`SMALLEST`]. `BLAZE_UNIT_SIZE` sets it.
const UNIT: usize = 20_000;
const SMALLEST: usize = 2_000;
/// The most functions a unit has.
const MEMBERS: usize = 256;

/// Whether the build under way compiles in units: with `BLAZE_UNITS=1`, one of a whole
/// program whose compiles import what they call, to ELF. The code of a function of a unit
/// is of the unit, so a build that takes edits, whose functions are compiled again one at a
/// time, has none; the other formats are compiled a function to a module.
pub(super) fn enabled() -> bool {
    static ASKED: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("BLAZE_UNITS").is_some_and(|it| it == "1"));
    *ASKED
        && imports::enabled()
        && super::Target::selected().is_ok_and(|it| it.object_format == ObjectFormat::Elf)
}

fn unit_size() -> usize {
    static SIZE: LazyLock<usize> = LazyLock::new(|| {
        std::env::var("BLAZE_UNIT_SIZE").ok().and_then(|it| it.parse().ok()).unwrap_or(UNIT)
    });
    *SIZE
}

/// A function found with its IR built.
struct Built {
    kind: Kind,
    pending: Pending,
}

/// What became of a function of a unit.
enum Outcome {
    /// The unit's code of it.
    Code(CompiledFunction),
    /// The code of an import the unit kept: the function's unless one it was compiled as has
    /// code of its own.
    Kept(CompiledFunction),
    /// Compiled alone, as the database has it.
    Alone,
}

/// What a function calls that is to be compiled: instances, and roots compiled only once
/// called.
#[derive(Default)]
struct Found {
    instances: Vec<(Instance, StoredParamEnvAndCrate)>,
    roots: Vec<FunctionId>,
}

/// Which of the functions a function calls are to be compiled, as
/// [`ObjectCodegen::compile_ahead`] has it: not a root, which is called by its symbol, unless
/// it is compiled only once called; not an instance an earlier round rejected; and not one
/// the store has.
struct Finder<'x> {
    rejected: &'x FxHashMap<Instance, Rejected>,
    on_demand: &'x FxHashSet<FunctionId>,
    entries: Option<&'x cached::Entries>,
}

impl Finder<'_> {
    fn find<'r>(
        &self,
        db: &dyn HirDatabase,
        symbols: &dyn SymbolResolver,
        names: &stable::StableNames<'_, '_>,
        homes: &mut home::Homes,
        requests: impl IntoIterator<Item = &'r FuncRequest>,
        found: &mut Found,
    ) {
        let in_store = |callee: &Instance| {
            let Some(entries) = self.entries else { return false };
            names.instance(callee).is_some_and(|name| {
                entries.usable(&name, &|symbol| symbols.provides(symbol) && symbols.linked(symbol))
            })
        };
        for request in requests {
            let FuncTarget::Instance(callee, asked) = &request.target else { continue };
            // A function without generic arguments is a root, called by its symbol when
            // rustc compiled it.
            let root = match callee.def {
                InstanceDef::Fn(func) if root_like(callee) => {
                    if self.on_demand.contains(&func) {
                        found.roots.push(func);
                    }
                    symbols.function(func).is_some()
                }
                _ => false,
            };
            if !root && !self.rejected.contains_key(callee) && !in_store(callee) {
                found.instances.push((callee.clone(), homes.home(db, callee, asked)));
            }
        }
    }
}

/// Whether an instance is of a function without generic arguments, which is a root, called
/// by its symbol when rustc compiled it.
fn root_like(instance: &Instance) -> bool {
    matches!(instance.def, InstanceDef::Fn(_))
        && instance.args.as_ref().iter().all(|it| it.region().is_some())
}

/// Whether a function whose IR is built can be in a unit with others.
fn shares(pending: &Pending) -> std::result::Result<(), &'static str> {
    if !pending.wants.seen.is_empty() {
        Err("shows LLVM the bodies it calls")
    } else if !pending.wants.requests.ordered() {
        Err("asks for what it calls out of order")
    } else {
        Ok(())
    }
}

/// What the code of a function in a unit can call: what it calls that it does not import,
/// the same of what it imports, and every function it takes the address of. What it imports
/// and LLVM leaves a call of is kept by the unit ([`llvm::emit_unit`]); a root, which is
/// called by its symbol, is called.
fn calls_after_imports(db: &dyn HirDatabase, pending: &Pending) -> Vec<FuncRequest> {
    let (Ok(isa), Ok(target)) = (super::target(), super::selected_target()) else {
        return pending.wants.requests.funcs.clone();
    };
    let mut recorder = Recorder::new(db, &**isa, target, false);
    let (funcs, datas) = recorder.moved_requests(&pending.wants.requests);
    let chosen = imports::chosen_for(&mut recorder, &|_| false, imports::BUDGET);
    let imported: FxHashMap<u32, &imports::Import> = chosen.iter().map(|it| (it.id, it)).collect();
    // What the IR refers to of its requests: some it asked for may be gone from it.
    let func = &pending.func;
    let named = |name: &ir::ExternalName| match name {
        ir::ExternalName::User(name) => {
            let name = &func.params.user_named_funcs()[*name];
            match name.namespace {
                0 => Some(Ok(funcs[name.index as usize].as_u32())),
                1 => Some(Err(datas[name.index as usize].as_u32())),
                _ => None,
            }
        }
        _ => None,
    };
    let (mut next, mut addressed, mut data) = (Vec::new(), FxHashSet::default(), Vec::new());
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            match func.dfg.insts[inst] {
                ir::InstructionData::Call { func_ref, .. }
                | ir::InstructionData::TryCall { func_ref, .. } => {
                    next.extend(named(&func.dfg.ext_funcs[func_ref].name).and_then(|it| it.ok()));
                }
                ir::InstructionData::FuncAddr { func_ref, .. } => {
                    let name = named(&func.dfg.ext_funcs[func_ref].name);
                    addressed.extend(name.and_then(|it| it.ok()));
                }
                ir::InstructionData::UnaryGlobalValue { global_value, .. } => {
                    let mut global = global_value;
                    loop {
                        match &func.global_values[global] {
                            ir::GlobalValueData::Symbol { name, .. } => {
                                match named(name) {
                                    Some(Ok(id)) => _ = addressed.insert(id),
                                    Some(Err(id)) => data.push(id),
                                    None => {}
                                }
                                break;
                            }
                            ir::GlobalValueData::IAddImm { base, .. }
                            | ir::GlobalValueData::Load { base, .. } => global = *base,
                            _ => break,
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let requests = recorder.wants.into_requests();
    // What is called, and what is taken the address of, with what that is built in calls
    // and refers to in turn.
    let (mut called, mut seen, mut reached) =
        (FxHashSet::default(), FxHashSet::default(), FxHashSet::default());
    next.extend(addressed.iter().copied());
    called.extend(addressed);
    loop {
        if let Some(at) = data.pop() {
            if !reached.insert(at) {
                continue;
            }
            if let DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } =
                &requests.datas[at as usize]
            {
                for &(_, func) in &blob.fn_relocs {
                    called.insert(func.as_u32());
                    next.push(func.as_u32());
                }
                data.extend(blob.relocs.iter().map(|it| it.1.as_u32()));
            }
        } else if let Some(id) = next.pop() {
            if !seen.insert(id) {
                continue;
            }
            match imported.get(&id) {
                Some(import) => {
                    next.extend(import.funcs.iter().copied());
                    data.extend(import.datas.iter().copied());
                    if let FuncTarget::Instance(instance, _) = &requests.funcs[id as usize].target
                        && root_like(instance)
                    {
                        called.insert(id);
                    }
                }
                None => _ = called.insert(id),
            }
        } else {
            break;
        }
    }
    let mut called: Vec<u32> = called.into_iter().collect();
    called.sort_unstable();
    called.into_iter().map(|at| requests.funcs[at as usize].clone()).collect()
}

impl<'a, 'db> ObjectCodegen<'a, 'db> {
    /// Compiles `roots` and `instances` in units, and everything their code calls in turn,
    /// for [`Self::define_function`] and [`Self::define_instance`] to find.
    pub(super) fn compile_units(
        &mut self,
        roots: &[FunctionId],
        instances: Vec<(Instance, StoredParamEnvAndCrate)>,
    ) {
        let start = stdx::time::Instant::now();
        let mut tried = Tried {
            instances: std::mem::take(&mut self.ahead),
            roots: std::mem::take(&mut self.unit_tried),
        };
        let mut wave = Vec::new();
        tried.add(self.symbols, Found { instances, roots: roots.to_vec() }, &mut wave);
        let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
        let told = self.tells_variables().then(|| self.told.clone());
        let entries = self.entries.as_deref();
        let ctx = Context {
            stored: &self.stored_roots,
            roots_from_store: self.roots_from_store(),
            entries,
            finder: Finder { rejected: self.rejected, on_demand: self.on_demand, entries },
            owned: &self.own_statics.crates,
            hydrate: self.hydrate,
            told: told.as_ref(),
        };
        let stats = std::env::var_os("BLAZE_STATS").is_some();
        let mut codes = Vec::new();
        while !wave.is_empty() {
            let began = stdx::time::Instant::now();
            let round = Mutex::new(Round::new(wave, tried, threads));
            let idle = Condvar::new();
            self.everywhere(threads, &|db, symbols, _| ctx.work(&round, &idle, db, symbols));
            let mut round = round.into_inner().unwrap_or_else(|e| e.into_inner());
            if stats && round.functions != 0 {
                eprintln!(
                    "units: {} functions found in {} waves and compiled in {} units of {} \
                     instructions, {:.1?}",
                    round.functions,
                    round.waves,
                    round.units,
                    round.size,
                    began.elapsed()
                );
            }
            tried = round.tried;
            wave = Vec::new();
            round.done.sort_by_key(|it| it.0);
            for (_, outcomes, found) in round.done {
                codes.extend(outcomes);
                tried.add(self.symbols, found, &mut wave);
            }
        }
        (self.ahead, self.unit_tried) = (tried.instances, tried.roots);
        // A function's own code first, then what was kept of it as an import, in the order
        // the units were cut in, the first kept.
        let mut kept = Vec::new();
        for (kind, outcome) in codes {
            match (kind, outcome) {
                (Kind::Root(root), Outcome::Code(code)) => _ = self.unit_roots.insert(root, code),
                (Kind::Instance(instance, _), Outcome::Code(code)) => {
                    _ = self.unit_instances.insert(instance, code)
                }
                (Kind::Instance(instance, _), Outcome::Kept(code)) => kept.push((instance, code)),
                _ => {}
            }
        }
        for (instance, code) in kept {
            self.unit_instances.entry(instance).or_insert(code);
        }
        self.phases.compile += start.elapsed();
    }

    /// Runs `work`, which takes jobs until there are none, on every core, and on this thread
    /// for what is left: [`SymbolResolver::in_parallel`] leaves a single job to this thread.
    fn everywhere(&self, threads: usize, work: &ParallelWork<'_>) {
        self.symbols.in_parallel(threads, work);
        work(self.db, self.symbols, 0);
    }
}

/// What was compiled or tried: [`ObjectCodegen::ahead`] and [`ObjectCodegen::unit_tried`].
struct Tried {
    instances: FxHashSet<(Instance, StoredParamEnvAndCrate)>,
    roots: FxHashSet<FunctionId>,
}

impl Tried {
    /// Adds to `wave` what of `found` was not compiled or tried yet.
    fn add(&mut self, symbols: &dyn SymbolResolver, found: Found, wave: &mut Vec<Kind>) {
        for root in found.roots {
            if symbols.function(root).is_some() && self.roots.insert(root) {
                wave.push(Kind::Root(root));
            }
        }
        for it in found.instances {
            if self.instances.insert(it.clone()) {
                wave.push(Kind::Instance(it.0, it.1));
            }
        }
    }
}

/// A round of [`ObjectCodegen::compile_units`]: the functions found a wave at a time, cut
/// into units as they are found, and the units compiled meanwhile.
struct Round {
    tried: Tried,
    threads: usize,
    /// The wave being built, the IR of each with what it calls, how many of it were taken
    /// and how many of those are being built.
    wave: Vec<Kind>,
    built: Vec<Option<(Option<Result<Produced>>, Found)>>,
    taken: usize,
    building: usize,
    /// The unit being filled, and its instructions.
    unit: Vec<Built>,
    filled: usize,
    /// The units and the functions compiled alone yet to be compiled, by their place in the
    /// order they were cut in, and what became of those compiled, in any order.
    jobs: BinaryHeap<Queued>,
    queued: usize,
    done: Vec<(usize, Vec<(Kind, Outcome)>, Found)>,
    functions: usize,
    waves: usize,
    units: usize,
    size: usize,
}

impl Round {
    fn new(wave: Vec<Kind>, tried: Tried, threads: usize) -> Round {
        Round {
            tried,
            threads,
            built: wave.iter().map(|_| None).collect(),
            wave,
            taken: 0,
            building: 0,
            unit: Vec::new(),
            filled: 0,
            jobs: BinaryHeap::new(),
            queued: 0,
            done: Vec::new(),
            functions: 0,
            waves: 0,
            units: 0,
            size: 0,
        }
    }

    /// Cuts the wave just built into units, in the order it has, and makes what it calls
    /// the next wave.
    fn found(&mut self, symbols: &dyn SymbolResolver) {
        let wave = std::mem::take(&mut self.wave);
        let built = std::mem::take(&mut self.built);
        let mut next = Vec::new();
        self.waves += 1;
        for (kind, built) in std::iter::zip(wave, built) {
            let (produced, found) = built.unwrap_or_default();
            self.cut(kind, produced);
            self.tried.add(symbols, found, &mut next);
        }
        if next.is_empty() && !self.unit.is_empty() {
            let unit = std::mem::take(&mut self.unit);
            self.queue(Job::Unit(unit));
        }
        self.built = next.iter().map(|_| None).collect();
        (self.wave, self.taken) = (next, 0);
    }

    /// Puts a function found in the unit being filled, or in a unit of its own, or with what
    /// is compiled alone: one whose IR could not be built for a unit, whose code is not
    /// LLVM's, or that [`shares`] turns down. A unit is cut once it has about a share of all
    /// found so far, so that the units are as many as the cores can take from early on, and
    /// so that they are the same in every build of the program, whatever order the threads
    /// finish in: of [`SMALLEST`] instructions at least and of [`UNIT`] at most.
    fn cut(&mut self, kind: Kind, produced: Option<Result<Produced>>) {
        let why: std::result::Result<Pending, &str> = match produced {
            None => return,
            Some(Ok(Produced::Pending(pending))) => shares(&pending).map(|()| pending),
            Some(Ok(Produced::Code(_))) => Err("its code built without LLVM"),
            Some(Err(_)) => Err("IR not built for a unit"),
        };
        self.functions += 1;
        let pending = match why {
            Ok(pending) => pending,
            Err(why) => {
                Tally::global().alone(why, 1);
                return self.queue(Job::Alone(kind));
            }
        };
        let size = pending.func.dfg.num_insts();
        self.size += size;
        // Too large to optimize, it would leave the rest of its unit unoptimized.
        if size > super::LARGE {
            return self.queue(Job::Unit(vec![Built { kind, pending }]));
        }
        self.filled += size;
        self.unit.push(Built { kind, pending });
        let share = (self.size / (self.threads * 4).max(1)).clamp(SMALLEST, unit_size());
        if self.filled >= share || self.unit.len() >= MEMBERS {
            let unit = std::mem::take(&mut self.unit);
            self.queue(Job::Unit(unit));
        }
    }

    fn queue(&mut self, job: Job) {
        if let Job::Unit(_) = &job {
            self.units += 1;
            self.filled = 0;
        }
        self.jobs.push(Queued { size: job.size(), place: self.queued, job });
        self.queued += 1;
    }
}

/// A job of a [`Round`], the largest first, and of two alike the one cut first.
struct Queued {
    size: usize,
    place: usize,
    job: Job,
}

impl Ord for Queued {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.size.cmp(&other.size).then(other.place.cmp(&self.place))
    }
}

impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Queued {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Queued {}

/// What the threads of a [`Round`] share of the build.
struct Context<'x> {
    stored: &'x FxHashMap<FunctionId, Option<super::StableName>>,
    roots_from_store: bool,
    entries: Option<&'x cached::Entries>,
    finder: Finder<'x>,
    owned: &'x FxHashSet<base_db::Crate>,
    hydrate: bool,
    told: Option<&'x super::ToldTypes>,
}

impl Context<'_> {
    /// Takes the jobs of `round` until there are none: a function of the wave to build the IR
    /// of first, as what the next wave is waits for it, and else a unit to compile.
    fn work(
        &self,
        round: &Mutex<Round>,
        idle: &Condvar,
        db: &dyn HirDatabase,
        symbols: &dyn SymbolResolver,
    ) {
        let names = stable::StableNames::new(db, symbols, self.owned.clone(), self.hydrate);
        let mut homes = home::Homes::default();
        let lock = || round.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = lock();
        loop {
            if state.taken < state.wave.len() {
                let at = state.taken;
                let kind = state.wave[at].clone();
                (state.taken, state.building) = (at + 1, state.building + 1);
                drop(state);
                let built = self.built(db, symbols, &names, &mut homes, &kind);
                state = lock();
                (state.built[at], state.building) = (Some(built), state.building - 1);
                if state.taken == state.wave.len() && state.building == 0 {
                    state.found(symbols);
                    idle.notify_all();
                }
                continue;
            }
            if let Some(Queued { place, job, .. }) = state.jobs.pop() {
                drop(state);
                let (outcomes, found) = self.compiled(db, symbols, &names, &mut homes, job);
                state = lock();
                state.done.push((place, outcomes, found));
                continue;
            }
            // Every function is found and every job taken.
            if state.wave.is_empty() {
                return;
            }
            state = idle.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// The IR of a function, with LLVM's part left for later, and what its code calls once
    /// LLVM has built in what it imports. `None` for one that is not compiled, without a
    /// symbol or in the store, and an error for one whose IR could not be built for a unit:
    /// that one is compiled alone here, to find what it calls.
    fn built(
        &self,
        db: &dyn HirDatabase,
        symbols: &dyn SymbolResolver,
        names: &stable::StableNames<'_, '_>,
        homes: &mut home::Homes,
        kind: &Kind,
    ) -> (Option<Result<Produced>>, Found) {
        let produced = match kind {
            Kind::Root(root) if symbols.function(*root).is_none() => {
                // It is not compiled, and is still lowered to be told why.
                _ = db.mir_body((*root).into());
                None
            }
            Kind::Root(root) => {
                let in_store = match self.stored.get(root) {
                    Some(name) => name.is_some(),
                    None => {
                        self.roots_from_store
                            && names.instance(&root_instance(db, *root)).is_some_and(|name| {
                                stable::StableNames::of_checkout(&name)
                                    && self.entries.is_some_and(|entries| {
                                        entries.usable(&name, &|symbol| {
                                            symbols.provides(symbol) && symbols.linked(symbol)
                                        })
                                    })
                            })
                    }
                };
                (!in_store).then(|| tier::for_build(|| super::root_code(db, *root, true)))
            }
            Kind::Instance(instance, env) => Some(tier::for_build(|| {
                super::instance_code(db, instance.clone(), env.clone(), Detach::Unit)
            })),
        };
        let mut found = Found::default();
        match &produced {
            None => {}
            Some(Ok(Produced::Pending(pending))) if shares(pending).is_ok() => {
                let calls = calls_after_imports(db, pending);
                self.finder.find(db, symbols, names, homes, &calls, &mut found);
            }
            Some(_) => {
                let compiled = match kind {
                    Kind::Root(root) => compiled_root(db, *root),
                    Kind::Instance(instance, env) => {
                        compiled_instance(db, instance.clone(), env.clone())
                    }
                };
                if let Ok(compiled) = compiled {
                    let calls = &compiled.requests.funcs;
                    self.finder.find(db, symbols, names, homes, calls, &mut found);
                }
            }
        }
        (produced, found)
    }

    /// Compiles a unit, or a function alone, and what the code of each function calls.
    fn compiled(
        &self,
        db: &dyn HirDatabase,
        symbols: &dyn SymbolResolver,
        names: &stable::StableNames<'_, '_>,
        homes: &mut home::Homes,
        job: Job,
    ) -> (Vec<(Kind, Outcome)>, Found) {
        let outcomes = match job {
            Job::Unit(members) => unit(db, members),
            Job::Alone(kind) => vec![(kind, Outcome::Alone)],
        };
        let mut found = Found::default();
        for (kind, outcome) in &outcomes {
            let compiled = match (outcome, kind) {
                (Outcome::Code(code) | Outcome::Kept(code), _) => Some(code),
                (Outcome::Alone, Kind::Root(root)) => {
                    if self.told.is_some() {
                        root_rows(db, *root);
                    }
                    compiled_root(db, *root).as_ref().ok()
                }
                (Outcome::Alone, Kind::Instance(instance, env)) => {
                    if self.told.is_some() {
                        instance_rows(db, instance.clone(), env.clone());
                    }
                    compiled_instance(db, instance.clone(), env.clone()).as_ref().ok()
                }
            };
            let Some(compiled) = compiled else { continue };
            if let Some(told) = self.told {
                told.ahead(db, compiled);
            }
            // What the unit kept has code.
            let calls = compiled.requests.funcs.iter().filter(|it| match &it.target {
                FuncTarget::Instance(callee, _) => !outcomes.iter().any(|(kind, outcome)| {
                    matches!((kind, outcome), (Kind::Instance(it, _), Outcome::Kept(_)) if it == callee)
                }),
                _ => true,
            });
            self.finder.find(db, symbols, names, homes, calls, &mut found);
        }
        (outcomes, found)
    }
}

enum Job {
    Unit(Vec<Built>),
    Alone(Kind),
}

impl Job {
    fn size(&self) -> usize {
        match self {
            Job::Unit(members) => members.iter().map(|it| it.pending.func.dfg.num_insts()).sum(),
            Job::Alone(_) => super::LARGE,
        }
    }
}

/// Compiles the functions of a unit in one module: the code of each, or [`Outcome::Alone`]
/// for one the unit has none for, which is then compiled alone.
fn unit(db: &dyn HirDatabase, members: Vec<Built>) -> Vec<(Kind, Outcome)> {
    let tally = Tally::global();
    let alone = |members: Vec<Built>, why: &str| {
        tally.alone(why, members.len());
        members.into_iter().map(|it| (it.kind, Outcome::Alone)).collect::<Vec<_>>()
    };
    let (Ok(isa), Ok(target)) = (super::target(), super::selected_target()) else {
        return alone(members, "no target");
    };
    let Ok(Backend::Llvm(optimization)) = Backend::from_env() else {
        return alone(members, "no LLVM");
    };
    let mut recorder = Recorder::new(db, &**isa, target, false);
    // Each function in the unit's names, and the request the others call it by.
    let mut funcs = Vec::with_capacity(members.len());
    let mut defined: FxHashSet<Instance> = FxHashSet::default();
    for member in &members {
        let (moved_funcs, moved_datas) = recorder.moved_requests(&member.pending.wants.requests);
        let func = fragment::renamed(&member.pending.func, &moved_funcs, &moved_datas);
        let (instance, env) = match &member.kind {
            Kind::Root(root) => {
                let env = super::root_body(db, *root).map(|it| it.1.store());
                let Ok(env) = env else { return alone(members, "a root without a body") };
                (root_instance(db, *root), env)
            }
            Kind::Instance(instance, env) => (instance.clone(), env.clone()),
        };
        let known = recorder.requests.funcs.iter().position(|it| {
            matches!(&it.target, FuncTarget::Instance(called, _) if *called == instance)
                && it.sig == func.signature
        });
        let request = match known {
            Some(at) => at as u32,
            None => {
                let target = FuncTarget::Instance(instance.clone(), env);
                recorder.func(target, &func.signature).as_u32()
            }
        };
        defined.insert(instance);
        funcs.push((func, request));
    }
    let budget = imports::BUDGET.saturating_mul(members.len());
    let chosen = imports::chosen_for(&mut recorder, &|it| defined.contains(it), budget);
    let chosen = imports::tallied(chosen);
    let compiled: Vec<llvm::Member<'_>> = std::iter::zip(&members, &funcs)
        .map(|(member, (func, request))| llvm::Member {
            func,
            named: &member.pending.named,
            pointees: &member.pending.wants.pointees,
            request: *request,
        })
        .collect();
    let start = stdx::time::Instant::now();
    let emitted = llvm::emit_unit(&mut recorder.wants, &compiled, optimization, &chosen);
    let took = start.elapsed();
    drop(compiled);
    let llvm::Unit { members: codes, kept } = match emitted {
        Ok(unit) => unit,
        Err(why) => return alone(members, &why.to_string()),
    };
    tally.unit(members.len());
    let each = took / members.len() as u32;
    // An import kept but not read is called by the code, and found in it.
    let kept: Vec<(Kind, Outcome)> = kept
        .into_iter()
        .filter_map(|(id, code)| {
            let FuncTarget::Instance(instance, env) =
                recorder.requests.funcs[id as usize].target.clone()
            else {
                return None;
            };
            // A root is a function of its own, called by its symbol.
            if root_like(&instance) {
                return None;
            }
            let mut code = backend::pad(code.ok()?).ok()?;
            code.lines.clear();
            code.slots = Default::default();
            let requests = recorder.wants.referenced(&mut code);
            tally.unit_kept.fetch_add(1, Ordering::Relaxed);
            let compiled = CompiledFunction {
                requests,
                code,
                source: None,
                closure: None,
                variables: None,
                layouts: Vec::new(),
            };
            Some((Kind::Instance(instance, env), Outcome::Kept(compiled)))
        })
        .collect();
    let mut outcomes: Vec<(Kind, Outcome)> = std::iter::zip(members, codes)
        .map(|(member, code)| {
            let code = match code.map(backend::pad) {
                Ok(Ok(code)) => code,
                Ok(Err(why)) => {
                    tally.alone(&why.to_string(), 1);
                    return (member.kind, Outcome::Alone);
                }
                Err(why) => {
                    tally.alone(&why.to_string(), 1);
                    return (member.kind, Outcome::Alone);
                }
            };
            let mut code = code;
            let requests = recorder.wants.referenced(&mut code);
            tally.llvm.add(each, code.bytes.len());
            crate::counted::compiled();
            let Pending { source, variables, layouts, closure, .. } = member.pending;
            let compiled = CompiledFunction { requests, code, source, closure, variables, layouts };
            (member.kind, Outcome::Code(compiled))
        })
        .collect();
    outcomes.extend(kept);
    outcomes
}
