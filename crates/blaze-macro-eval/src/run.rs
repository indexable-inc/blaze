//! One expansion on the MIR evaluator: the macro's client made by the bridge's own
//! `Client::expand1`/`expand2` for the macro's function, its input in a buffer made by the
//! bridge's own `Buffer::new` and `reserve`, and its `run` called with a `BridgeConfig` whose
//! dispatch is `Closure::call`, which blaze claims: every request the client makes comes to
//! [`Server::dispatch`], and is answered in the client's buffer. The shims of [`os`] are
//! claimed beside it. Whatever else the macro calls the evaluator runs or refuses, checking
//! every access.
//!
//! The layouts written here are the bridge's `#[repr(C)]` ones, which [`protocol`] checks are
//! the pinned sources': `Buffer` five words (`data`, `len`, `capacity`, `reserve`, `drop`),
//! `Closure` two (`call`, `env`), `BridgeConfig` a `Buffer`, a `Closure` and a `bool`, and
//! `Client` its `run` pointer.
//!
//! [`os`]: crate::os
//! [`protocol`]: crate::protocol

use std::{
    cell::RefCell,
    time::{Duration, Instant},
};

use base_db::{ConstEvalBudget, Crate, CrateOrigin, Db, Env};
use blaze_hash::HashMap;
use blaze_profile::span::{Ending, MacroExpand, MacroExpanded, Name as SpanName, Outcome};
use hir_def::{AssocItemId, FunctionId, HasModule, ModuleDefId, StaticId, nameres::crate_def_map};
use hir_expand::name::Name;
use hir_ty::{
    InferBodyId, ParamEnvAndCrate,
    db::HirDatabase,
    mir::Budget,
    next_solver::{DbInterner, GenericArg, GenericArgs, Region, RegionExt, SolverDefId, Ty},
};
use hir_ty_mir::mir::{
    Evaluator, MirEvalError,
    foreign::{Claimed, Foreign},
};
use intern::Symbol;
use span::{Edition, Span};

use crate::{
    guest::{Guest, Stop},
    os::{Answer, Os, Shim},
    server::{Emitted, Server},
    stream::Stream,
    wire::Width,
};

/// What a macro's expansion may use of the evaluator's memory, stack and heap together. A
/// derive of serde's size uses tens of megabytes natively; the evaluator's buffers may hold
/// twice what they use, so one expansion holds at most 2 GiB of the daemon's.
pub(crate) const MEMORY: u64 = 1 << 30;

/// The steps (basic blocks run) one expansion may take on the evaluator before it stops,
/// naming the macro and where it was: a macro that loops ends in an error, not a stalled
/// build. Each expansion of a run, the session's earlier calls run again included, has its own.
/// A derive of serde's size runs a few million basic blocks natively: this is ten to a hundred
/// times that, and minutes of the evaluator at most.
pub(crate) const STEPS: u64 = 1 << 27;

/// The `usize` of the evaluated client: the shims model only 64-bit targets (`os/target.rs`),
/// and [`State::word`] is the same eight bytes.
const WIDTH: Width = Width::W64;

/// The functions of the `proc_macro` crate a run calls or claims.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bridge {
    pub(crate) expand1: FunctionId,
    pub(crate) expand2: FunctionId,
    pub(crate) buffer_new: FunctionId,
    pub(crate) closure_call: FunctionId,
}

/// The method `name` of an inherent impl in the module at `path` of `krate`.
fn method(db: &Db, krate: Crate, path: &[&str], name: &str) -> Result<FunctionId, String> {
    let def_map = crate_def_map(db, krate);
    let mut module = def_map.crate_root(db);
    for part in path {
        let child = def_map.module(db, module).children(db).get(&Name::new_symbol_root(Symbol::intern(part)));
        let Some(&child) = child else {
            // alloc: error path
            return Err(format!("its `proc_macro` has no module `{}`", path.join("::")));
        };
        module = child;
    }
    let name = Name::new_symbol_root(Symbol::intern(name));
    let scope = def_map.module(db, module).scope(db);
    scope
        .impls()
        .find_map(|it| {
            it.impl_items(db).items.iter().find_map(|(item_name, item)| match item {
                AssocItemId::FunctionId(function) if *item_name == name => Some(*function),
                _ => None,
            })
        })
        // alloc: error path
        .ok_or_else(|| format!("its `proc_macro` has no `{}::{}`", path.join("::"), name.as_str()))
}

impl Bridge {
    /// The bridge's functions in `proc_macro`, the crate the macro crate is built against.
    pub(crate) fn find(db: &Db, proc_macro: Crate) -> Result<Bridge, String> {
        Ok(Bridge {
            expand1: method(db, proc_macro, &["bridge", "client"], "expand1")?,
            expand2: method(db, proc_macro, &["bridge", "client"], "expand2")?,
            buffer_new: method(db, proc_macro, &["bridge", "buffer"], "new")?,
            closure_call: method(db, proc_macro, &["bridge", "closure"], "call")?,
        })
    }
}

/// One call to expand: which function, what it is given, and the state it starts in.
pub(crate) struct Expansion<'a> {
    /// The macro's `#[proc_macro*]` function.
    pub(crate) function: FunctionId,
    /// An attribute's: its client is `expand2`, given the attribute's arguments then the item.
    pub(crate) attr: bool,
    pub(crate) inputs: &'a [Stream],
    /// The def, call and mixed sites.
    pub(crate) sites: [Span; 3],
    pub(crate) edition: Edition,
    /// Whether the calling crate enables `forced_keywords`.
    pub(crate) forced_keywords: bool,
}

/// What an expansion gave and what it did besides.
pub(crate) struct Ran {
    /// The output, or the macro's panic message.
    pub(crate) output: Result<Stream, String>,
    /// Whether it learned of a span what its key does not say.
    pub(crate) asked: bool,
    pub(crate) tracked: Vec<String>,
    pub(crate) emitted: Vec<Emitted>,
    /// What it read, each line as `assemble_seen` takes it.
    pub(crate) seen: Vec<(blaze_usercode::trace::Seen, String)>,
    /// Why what it read can't be keyed, when it can't.
    pub(crate) unkeyable: Option<String>,
    /// The statics outside the standard library that the runs changed: each one's bytes as
    /// they ended, and whether they hold an address of the evaluator's memory.
    pub(crate) statics: Vec<(StaticId, Vec<u8>, bool)>,
}

/// What a run spent on the evaluator, read from it whether the run ended or stopped: what the
/// run's `macro expand` span says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Spent {
    /// Whether it ran on the evaluator: a compiled run counts no steps.
    pub(crate) evaluated: bool,
    /// Basic blocks run, over every expansion of the run (the session's earlier calls too).
    pub(crate) steps: u64,
    /// The most bytes of stack and heap the evaluator held at once.
    pub(crate) peak_bytes: u64,
    /// The budget that stopped it, when one did.
    pub(crate) over: Option<Over>,
}

/// A budget of [`STEPS`] or [`MEMORY`] that stopped a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Over {
    Steps,
    Memory,
}

/// How a run ended, as its span's `outcome` says it.
pub(crate) fn outcome_of(ran: &Result<Ran, Stop>, spent: &Spent) -> Outcome {
    match (ran, spent.over) {
        (Ok(ran), _) if ran.output.is_err() => Outcome::Panic,
        (Ok(_), _) => Outcome::Ok,
        (Err(_), Some(Over::Steps)) => Outcome::StepLimit,
        (Err(_), Some(Over::Memory)) => Outcome::MemoryLimit,
        (Err(_), None) => Outcome::Error,
    }
}

/// Opens and enters the span of one run of a macro, `macro expand`: its `function` the
/// macro's path (`serde_derive::Serialize`), its `crate` the crate the call is in. [`ran_on`]
/// says what the run spent and how it ended, and closes it.
pub(crate) fn expand_span(path: &str, calling: &str) -> Ending<MacroExpanded> {
    MacroExpand { path: SpanName::of(path), krate: SpanName::of(calling) }.open()
}

/// Closes `span`, an [`expand_span`], with what its run spent and how it ended: steps and peak
/// bytes only for a run on the evaluator.
pub(crate) fn ran_on(span: Ending<MacroExpanded>, spent: &Spent, ran: &Result<Ran, Stop>) {
    let evaluated = |it: u64| spent.evaluated.then_some(it);
    span.ended(MacroExpanded {
        steps: evaluated(spent.steps),
        peak_bytes: evaluated(spent.peak_bytes),
        outcome: outcome_of(ran, spent),
    });
}

/// The budget that stopped the evaluation `error` ended it in, under the frames it unwound.
pub(crate) fn over_budget(error: &MirEvalError<'_>) -> Option<Over> {
    let mut leaf = error;
    while let MirEvalError::InFunction(inner, _) = leaf {
        leaf = inner;
    }
    match leaf {
        MirEvalError::OverBudget { spent: Budget::Steps(_), .. } => Some(Over::Steps),
        MirEvalError::OverBudget { spent: Budget::Memory(_), .. } => Some(Over::Memory),
        _ => None,
    }
}

/// The state the claimed calls share with the run that set them: outside the evaluator, so
/// the run reads it back once the evaluator is done.
struct State<'e> {
    server: Server,
    os: Os<'e>,
    shims: HashMap<FunctionId, Shim>,
    /// Why a claimed call stopped the evaluation.
    stop: Option<Stop>,
    reply: Vec<u8>,
    /// The size of a pointer of the evaluated program.
    word: usize,
}

/// How often a run on the evaluator says it is still running.
const PROGRESS: Duration = Duration::from_secs(10);

/// The evaluator's embedder for one run.
struct Host<'r, 'e> {
    db: &'r Db,
    closure_call: FunctionId,
    state: &'r RefCell<State<'e>>,
    /// The macro and the crate calling it, as the run's progress lines name them.
    what: &'r str,
    started: Instant,
    /// When the run next says it is still running.
    next: Instant,
}

/// The evaluator's memory as a shim reaches it.
struct Memory<'a, 'db> {
    db: &'a Db,
    eval: &'a mut Evaluator<'db>,
}

/// `error` as the expansion's error. Its types print through the attached database, so it is
/// rendered with `db` attached: the caller may run outside any attach (the daemon's loop).
pub(crate) fn stop_of(db: &Db, error: MirEvalError<'_>) -> Stop {
    hir_ty::attach_db(db, || Stop(describe::describe(db, &error)))
}

impl Guest for Memory<'_, '_> {
    fn read(&self, addr: u64, len: usize) -> Result<&[u8], Stop> {
        self.eval.read_bytes(addr, len).map_err(|e| stop_of(self.db, e))
    }

    fn write(&mut self, addr: u64, bytes: &[u8]) -> Result<(), Stop> {
        self.eval.write_bytes(addr, bytes).map_err(|e| stop_of(self.db, e))
    }

    fn set_errno(&mut self, code: i32) -> Result<(), Stop> {
        self.eval.set_errno(code).map_err(|e| stop_of(self.db, e))
    }

    fn alloc(&mut self, bytes: &[u8], align: usize) -> Result<u64, Stop> {
        self.eval.alloc_bytes(bytes, align).map_err(|e| stop_of(self.db, e))
    }
}

/// The `n`th word of `bytes`, of `word` bytes.
fn word_at(bytes: &[u8], word: usize, n: usize) -> Result<u64, Stop> {
    let Some(it) = bytes.get(n * word..(n + 1) * word) else {
        // alloc: error path
        return Err(Stop(format!("gave {} bytes where the bridge has {} words", bytes.len(), n + 1)));
    };
    let mut out = [0; 8];
    out[..word].copy_from_slice(it);
    Ok(u64::from_le_bytes(out))
}

fn set_word(bytes: &mut [u8], word: usize, n: usize, value: u64) {
    bytes[n * word..(n + 1) * word].copy_from_slice(&value.to_le_bytes()[..word]);
}

/// The buffer `buffer` (its five words' bytes) made to hold `bytes` and nothing else: grown
/// by its own `reserve` when it is too small, as the client's `extend_from_slice` grows it.
fn fill(db: &Db, eval: &mut Evaluator<'_>, buffer: &mut Vec<u8>, word: usize, bytes: &[u8]) -> Result<(), Stop> {
    set_word(buffer, word, 1, 0);
    if (word_at(buffer, word, 2)? as usize) < bytes.len() {
        let reserve = buffer[3 * word..4 * word].to_vec(); // alloc: a growth, once per buffer too small
        let additional = (bytes.len() as u64).to_le_bytes();
        *buffer = eval.call_pointer(&reserve, &[buffer, &additional[..word]]).map_err(|e| stop_of(db, e))?;
    }
    let data = word_at(buffer, word, 0)?;
    if !bytes.is_empty() {
        eval.write_bytes(data, bytes).map_err(|e| stop_of(db, e))?;
    }
    set_word(buffer, word, 1, bytes.len() as u64);
    Ok(())
}

impl<'r, 'e> Host<'r, 'e> {
    /// `Closure::call(&mut self, buffer)`: the request in the buffer answered in it.
    fn dispatch(&self, eval: &mut Evaluator<'_>, args: &[&[u8]], ret: &mut [u8]) -> Result<(), Stop> {
        let Some(buffer) = args.get(1) else {
            return Err(Stop("called the bridge's dispatch without its buffer".to_owned())); // alloc: error path
        };
        // The state is not borrowed across `fill`: the buffer's `reserve` is the program's
        // code, which may make claimed calls of its own.
        let (word, mut reply, answered) = {
            let mut state = self.state.borrow_mut();
            let state = &mut *state;
            let word = state.word;
            let (data, len) = (word_at(buffer, word, 0)?, word_at(buffer, word, 1)?);
            let len = usize::try_from(len).map_err(|_| Stop("sent a request longer than memory".to_owned()))?; // alloc: error path
            let request = eval.read_bytes(data, len).map_err(|e| stop_of(self.db, e))?;
            let mut reply = std::mem::take(&mut state.reply);
            let answered = state.server.dispatch(self.db, request, &mut reply);
            (word, reply, answered)
        };
        let mut buffer = buffer.to_vec(); // alloc: the five words of the reply's buffer
        let filled = answered.and_then(|()| fill(self.db, eval, &mut buffer, word, &reply));
        reply.clear();
        self.state.borrow_mut().reply = reply;
        filled?;
        if ret.len() != buffer.len() {
            // alloc: error path
            return Err(Stop(format!("returns a buffer of {} bytes, not the bridge's {}", ret.len(), buffer.len())));
        }
        ret.copy_from_slice(&buffer);
        Ok(())
    }
}

impl<'db, 'r: 'db, 'e: 'db> Foreign<'db> for Host<'r, 'e> {
    fn claims(&mut self, db: &'db Db, func: FunctionId) -> bool {
        if func == self.closure_call {
            return true;
        }
        match Os::claims(db, func) {
            Some(shim) => {
                self.state.borrow_mut().shims.insert(func, shim);
                true
            }
            None => false,
        }
    }

    fn call(
        &mut self,
        eval: &mut Evaluator<'db>,
        func: FunctionId,
        _generic_args: GenericArgs<'db>,
        args: &[&[u8]],
        ret: &mut [u8],
    ) -> Result<Claimed, MirEvalError<'db>> {
        // The evaluator gives the run back only at its claimed calls (the bridge's dispatch,
        // the shims), which a macro makes for each token it reads or writes.
        let now = Instant::now();
        if now >= self.next {
            self.next = now + PROGRESS;
            let secs = now.duration_since(self.started).as_secs();
            let steps = eval.steps_spent();
            eprintln!("blaze serve: {} has run {secs} s on the evaluator, {steps} steps into its expansion", self.what);
        }
        let result = if func == self.closure_call {
            self.dispatch(eval, args, ret).map(|()| Claimed::Answered)
        } else {
            let mut state = self.state.borrow_mut();
            let state = &mut *state;
            match state.shims.get(&func) {
                Some(&shim) => state.os.call(shim, &mut Memory { db: self.db, eval }, args, ret).map(|answer| match answer {
                    Answer::Answered => Claimed::Answered,
                    Answer::Evaluator => Claimed::Declined,
                }),
                None => Err(Stop("called a claimed function that has no shim".to_owned())), // alloc: error path
            }
        };
        result.map_err(|stop| {
            self.state.borrow_mut().stop.get_or_insert(stop);
            MirEvalError::MachineStop
        })
    }
}

/// Whether `st` is the standard library's (`std`, `core`, `alloc`, `proc_macro` and the
/// rest of the sysroot): its state is the runtime's, made again each run.
pub(crate) fn of_sysroot(db: &Db, st: StaticId) -> bool {
    matches!(st.module(db).krate(db).data(db).origin, CrateOrigin::Lang(_))
}

/// Runs `expansions` in order on one evaluator, each with its own server as rustc gives each
/// expansion its own handles, starting from the statics `preset`: what the last gave, and
/// what all of them did. The ones before the last are the session's earlier calls, run again
/// for the state they leave. What the evaluator spent goes in `spent`, however the run ends.
pub(crate) fn run(
    db: &Db,
    what: &str,
    bridge: &Bridge,
    env: &Env,
    preset: &[(StaticId, Vec<u8>)],
    expansions: &[Expansion<'_>],
    spent: &mut Spent,
) -> Result<Ran, Stop> {
    evaluate(db, what, bridge, env, preset, expansions, spent)
}

fn evaluate(
    db: &Db,
    what: &str,
    bridge: &Bridge,
    env: &Env,
    preset: &[(StaticId, Vec<u8>)],
    expansions: &[Expansion<'_>],
    spent: &mut Spent,
) -> Result<Ran, Stop> {
    spent.evaluated = true;
    let Some(last) = expansions.last() else {
        return Err(Stop("was given nothing to expand".to_owned())); // alloc: error path
    };
    let state = RefCell::new(State {
        server: Server::new(last.sites, last.edition, last.forced_keywords, WIDTH),
        os: Os::new(env),
        shims: HashMap::default(),
        stop: None,
        // alloc: once per run, the replies' buffer
        reply: Vec::new(),
        word: 8,
    });
    // The evaluator's error borrows the state; made a `Stop` before the state is taken back.
    let output = on_evaluator(db, what, bridge, &state, preset, expansions, spent).map_err(|e| {
        spent.over = over_budget(&e);
        stop_of(db, e)
    });
    let state = state.into_inner();
    let (output, statics) = match (output, state.stop) {
        (Ok(it), _) => it,
        (Err(_), Some(stop)) => return Err(stop),
        (Err(error), None) => return Err(error),
    };
    let server = state.server;
    // alloc: once per run, what it read, given to the key
    let seen = state.os.seen().map(|(seen, what)| (seen, what.to_owned())).collect();
    Ok(Ran {
        output,
        asked: server.asked,
        tracked: server.tracked,
        emitted: server.emitted,
        seen,
        unkeyable: state.os.unkeyable().map(str::to_owned),
        statics,
    })
}

/// What a run gave on the evaluator: the last expansion's output, and the statics it changed.
type Evaluated = (Result<Stream, String>, Vec<(StaticId, Vec<u8>, bool)>);

/// The evaluator's part of [`evaluate`], borrowing the state for as long as the evaluator
/// lives. What the evaluator spent goes in `spent` once its expansions end or stop.
#[allow(clippy::too_many_arguments)]
fn on_evaluator<'db>(
    db: &'db Db,
    what: &'db str,
    bridge: &Bridge,
    state: &'db RefCell<State<'_>>,
    preset: &[(StaticId, Vec<u8>)],
    expansions: &[Expansion<'_>],
    spent: &mut Spent,
) -> Result<Evaluated, MirEvalError<'db>> {
    let first = expansions[0].function;
    let krate = first.module(db).krate(db);
    let env = ParamEnvAndCrate { param_env: db.trait_environment(first.into()), krate };
    let owner: InferBodyId<'db> = first.into();
    let mut eval = Evaluator::new(db, owner, Some(env))?;
    let budget = ConstEvalBudget { steps: STEPS, memory: MEMORY };
    eval.set_budget(budget);
    if let Some(checks) = hir_ty_mir::mir::overflow::crate_checks_overflow(db, krate) {
        eval.set_inherited_overflow_checks(checks);
    }
    eval.track_statics();
    for (id, bytes) in preset {
        eval.preset_static(*id, bytes);
    }
    let started = Instant::now();
    let host = Host { db, closure_call: bridge.closure_call, state, what, started, next: started + PROGRESS };
    eval.set_foreign(Some(Box::new(host))); // alloc: once per run, the evaluator's embedder
    let interner = DbInterner::new_with(db, krate);
    let call_args = GenericArgs::for_item(interner, SolverDefId::FunctionId(bridge.closure_call), |_, _, _, _| {
        GenericArg::from(Region::new_erased(interner))
    });
    let dispatch = eval.fn_pointer(bridge.closure_call, call_args)?;
    let ran = expand_all(db, bridge, state, &mut eval, interner, &dispatch, budget, expansions, spent);
    spent.steps += eval.steps_spent();
    spent.peak_bytes = eval.memory_peak();
    ran
}

/// [`on_evaluator`]'s expansions, in order, on `eval`; the steps of each but the last go in
/// `spent` as the next one's budget replaces them.
#[allow(clippy::too_many_arguments)]
fn expand_all<'db>(
    db: &'db Db,
    bridge: &Bridge,
    state: &'db RefCell<State<'_>>,
    eval: &mut Evaluator<'db>,
    interner: DbInterner<'db>,
    dispatch: &[u8],
    budget: ConstEvalBudget,
    expansions: &[Expansion<'_>],
    spent: &mut Spent,
) -> Result<Evaluated, MirEvalError<'db>> {
    let mut output = Ok(Stream::default());
    // alloc: once per run, the input's and the config's bytes
    let mut input = Vec::new();
    let mut config = Vec::new(); // alloc: once per run, as above
    for (at, expansion) in expansions.iter().enumerate() {
        if at > 0 {
            state.borrow_mut().server = Server::new(expansion.sites, expansion.edition, expansion.forced_keywords, WIDTH);
            spent.steps += eval.steps_spent();
            eval.set_budget(budget);
        }
        let expand = if expansion.attr { bridge.expand2 } else { bridge.expand1 };
        let macro_fn = Ty::new_fn_def(
            interner,
            hir_def::CallableDefId::FunctionId(expansion.function).into(),
            GenericArgs::default(),
        );
        let expand_args =
            GenericArgs::for_item(interner, SolverDefId::FunctionId(expand), |_, _, _, _| GenericArg::from(macro_fn));
        // `f`, the macro's function item, has no bytes.
        let client = eval.call(expand, expand_args, &[&[]])?;
        let mut buffer = eval.call(bridge.buffer_new, GenericArgs::default(), &[])?;
        let word = buffer.len() / 5;
        if !matches!(word, 4 | 8) || buffer.len() != 5 * word || client.len() != word {
            let (buffer, client) = (buffer.len(), client.len());
            // alloc: error path
            return Err(MirEvalError::NotSupported(format!(
                "a bridge `Buffer` of {buffer} bytes and a `Client` of {client}, not the pinned bridge's"
            )));
        }
        {
            let mut state = state.borrow_mut();
            state.word = word;
            state.server.input(expansion.inputs, &mut input);
        }
        if let Err(stop) = fill(db, eval, &mut buffer, word, &input) {
            state.borrow_mut().stop.get_or_insert(stop);
            return Err(MirEvalError::MachineStop);
        }
        config.clear();
        config.extend_from_slice(&buffer);
        config.extend_from_slice(dispatch);
        config.extend_from_slice(&[0; 8][..word]);
        // `force_show_panics: false`, then the padding to the config's alignment.
        config.resize(config.len() + word, 0);
        let returned = eval.call_pointer(&client, &[&config])?;
        let data = word_at(&returned, word, 0);
        let len = word_at(&returned, word, 1);
        let (data, len) = match (data, len) {
            (Ok(data), Ok(len)) => (data, len as usize),
            (Err(stop), _) | (_, Err(stop)) => {
                state.borrow_mut().stop.get_or_insert(stop);
                return Err(MirEvalError::MachineStop);
            }
        };
        let bytes = eval.read_bytes(data, len)?;
        let answer = state.borrow_mut().server.output(bytes);
        output = match answer {
            Ok(it) => it,
            Err(stop) => {
                state.borrow_mut().stop.get_or_insert(stop);
                return Err(MirEvalError::MachineStop);
            }
        };
    }
    // alloc: once per run, the statics it changed
    let mut statics = Vec::new();
    eval.changed_statics(&mut statics)?;
    statics.retain(|(st, _, _)| !of_sysroot(db, *st));
    Ok((output, statics))
}

/// The macro functions of `krate`, a proc-macro crate, by the name each is invoked by: what
/// `__blaze_proc_macros` lists, made from the crate's def map as that list is.
pub(crate) fn functions(db: &Db, krate: Crate) -> Vec<(Symbol, hir_expand::proc_macro::ProcMacroKind, FunctionId)> {
    let def_map = crate_def_map(db, krate);
    // alloc: once a proc-macro crate, its list
    let mut out = Vec::new();
    for def in def_map.module(db, def_map.crate_root(db)).scope(db).declarations() {
        let ModuleDefId::FunctionId(function) = def else { continue };
        let Some(id) = def_map.fn_as_proc_macro(function) else { continue };
        let kind = hir_def::Lookup::lookup(&id, db).kind;
        let name = match kind {
            hir_expand::proc_macro::ProcMacroKind::CustomDerive => {
                match hir_def::attrs::AttrFlags::derive_info(db, hir_def::MacroId::ProcMacroId(id)) {
                    Some(info) => info.trait_name.clone(), // clone: a symbol, once a macro
                    None => continue,
                }
            }
            _ => hir_def::signatures::FunctionSignature::of(db, function).name.symbol().clone(), // clone: as above
        };
        out.push((name, kind, function));
    }
    out
}

mod describe;

#[cfg(test)]
mod tests;
