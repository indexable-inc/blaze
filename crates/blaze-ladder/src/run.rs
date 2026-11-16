//! The straight line: source files to a status, each stage the shape of the task the request
//! machine will ask for (docs/v2/plan/design/bottom.md section 9): lower the files, check each
//! body, build it to LIR, emit it, place every body behind its slot, call the entry.
//!
//! Every stage's refusal is a [`RunError`] naming the function it is about (rule 19); nothing
//! outside a stage's subset is skipped (rule 3).

use std::alloc::Global;

use blaze_build::{Adts, BuildError, Instance, Subst};
use blaze_emit::{Code, EmitError, Via};
use blaze_own::Alloc;
use blaze_typeck::toy::ToyCtx;
use blaze_typeck::{Adt, CheckError, DiagCode, ExprId, ItemKey, List, Needs, Outcome, PNode, PatId, PortableTys};

use crate::lower::{self, Abi, LowerError, Program, SourceFile};
use crate::place::{self, Body, Entry, PlaceError, Target};

/// Why a program did not run.
#[derive(Debug)]
pub enum RunError {
    /// The lowering refused the program; `located` is the refusal with its file's path.
    Lower { error: LowerError, located: Box<str, Global> },
    /// More instances than a slot index holds.
    TooManyInstances,
    /// The checker stopped on a malformed body or a bound.
    Check { function: Box<str, Global>, error: CheckError },
    /// The checker asked for a question the lowered program's world holds back: the world's bug.
    Need { function: Box<str, Global> },
    /// The body has type errors: the first one's code, and how many.
    Types { function: Box<str, Global>, first: DiagCode, count: u32 },
    Build { function: Box<str, Global>, error: BuildError },
    Emit { function: Box<str, Global>, error: EmitError },
    /// A function the program calls that is neither a body nor an outside symbol.
    Callee { function: Box<str, Global>, callee: ItemKey },
    Place(PlaceError),
    /// The program has no `main` at its crate root.
    NoMain,
    /// `main`'s signature is not one the skeleton calls.
    MainShape { what: &'static str },
}

impl core::fmt::Display for RunError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RunError::Lower { located, .. } => write!(f, "{located}"),
            RunError::TooManyInstances => write!(f, "more than 2^32 instances"),
            RunError::Check { function, error } => write!(f, "`{function}`: the checker stopped: {error:?}"),
            RunError::Need { function } => {
                write!(f, "`{function}`: the checker needed a fact the program's world holds back")
            }
            RunError::Types { function, first, count } => {
                write!(f, "`{function}`: {count} type errors, the first {first:?}")
            }
            RunError::Build { function, error } => write!(f, "`{function}`: {error}"),
            RunError::Emit { function, error } => write!(f, "`{function}`: {error}"),
            RunError::Callee { function, callee } => {
                write!(f, "`{function}` calls item {:#x}, which is neither a body nor an outside symbol", callee.0)
            }
            RunError::Place(error) => write!(f, "{error}"),
            RunError::NoMain => write!(f, "the crate root has no `main`"),
            RunError::MainShape { what } => write!(f, "`main`: {what}"),
        }
    }
}

impl core::error::Error for RunError {}

struct ProgramAdts<'p>(&'p Program);

impl Adts for ProgramAdts<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.0.adt(key)
    }

    fn int_lit(&self, owner: ItemKey, expr: ExprId) -> Option<u128> {
        self.body(owner)?.int_value(expr)
    }

    fn int_pat(&self, owner: ItemKey, pat: PatId) -> Option<u128> {
        self.body(owner)?.pat_int_value(pat)
    }

    fn byte_str(&self, owner: ItemKey, expr: ExprId) -> Option<&[u8]> {
        self.body(owner)?.byte_str(expr)
    }
}

impl ProgramAdts<'_> {
    fn body(&self, owner: ItemKey) -> Option<&lower::Body> {
        self.0.fns.get(self.0.index_of(owner)?)?.body.as_ref()
    }
}

/// How `main` is called, from its signature: `fn main()` or `extern "C" fn main() -> i32`.
fn entry_shape(program: &Program, main: usize) -> Result<Entry, RunError> {
    let sig = program.sig(main);
    if !sig.inputs.is_empty() {
        return Err(RunError::MainShape { what: "it takes parameters (argc and argv are not passed yet)" });
    }
    match sig.types.nodes.get(sig.output.0 as usize) {
        Some(PNode::Tuple(list)) if list.len == 0 => Ok(Entry::Unit),
        Some(PNode::Int(blaze_typeck::IntTy::I32)) if program.fns[main].abi == Abi::C => Ok(Entry::I32),
        Some(PNode::Int(blaze_typeck::IntTy::I32)) => {
            Err(RunError::MainShape { what: "it returns `i32` with the Rust ABI; write `extern \"C\"` and `#![no_main]`" })
        }
        _ => Err(RunError::MainShape { what: "it returns neither `()` nor `i32`" }),
    }
}

/// Runs the program of `files`, whose crate root is `root`, in this process: its status.
///
/// Every body is checked once, generically. Instances are then built from `main` forward:
/// slot `n` is the `n`th instance reached, and only reached instances are built (a generic fn
/// once per list of ground arguments, an unreached fn never).
pub fn run(files: &[SourceFile<'_>], root: &str) -> Result<u8, RunError> {
    let program = lower::lower(files, root).map_err(|error| {
        // alloc: the refusal's text, with the file's path, once per failed run.
        let located = format!("{}", error.located(files)).into_boxed_str();
        RunError::Lower { error, located }
    })?;
    let main = program.entry().ok_or(RunError::NoMain)?;
    let entry = entry_shape(&program, main)?;
    if program.fns[main].body.is_none() {
        return Err(RunError::MainShape { what: "it has no body" });
    }
    let mut checked = Vec::with_capacity_in(program.fns.len(), Global);
    for (i, function) in program.fns.iter().enumerate() {
        let Some(body) = function.body.as_ref() else {
            checked.push(None);
            continue;
        };
        let name = || function.path.clone();
        let view = body.view(function.key);
        let world = program.world(i);
        let mut cx = ToyCtx::new();
        let done = match blaze_typeck::check(&view, &world, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
            Ok(Outcome::Done(done)) => done,
            Ok(Outcome::Need(_)) => return Err(RunError::Need { function: name() }),
            Err(error) => return Err(RunError::Check { function: name(), error }),
        };
        if let Some(first) = done.diags.first() {
            return Err(RunError::Types { function: name(), first: first.code, count: done.diags.len() as u32 });
        }
        checked.push(Some(done));
    }
    let adts = ProgramAdts(&program);
    let mut ground = PortableTys::new_in(Alloc::global());
    let mut instances = Vec::with_capacity_in(program.fns.len(), Global);
    instances.push(Instance { item: program.fns[main].key, args: List::EMPTY });
    let mut bodies = Vec::with_capacity_in(program.fns.len(), Global);
    let mut args = Vec::new_in(Global);
    let mut at = 0;
    while let Some(&instance) = instances.get(at) {
        at += 1;
        let i = program.index_of(instance.item).ok_or(RunError::Callee { function: "main".into(), callee: instance.item })?;
        let function = &program.fns[i];
        let name = || function.path.clone();
        let (Some(body), Some(done)) = (function.body.as_ref(), checked[i].as_ref()) else {
            return Err(RunError::Callee { function: name(), callee: instance.item });
        };
        args.clear();
        let malformed = |_| RunError::Build { function: name(), error: BuildError::Malformed { what: "an instance's arguments past the ground table" } };
        args.extend_from_slice(ground.view().list(instance.args).map_err(malformed)?);
        let view = body.view(function.key);
        let mut subst = Subst { args: &args, ground: &mut ground, scratch: Alloc::global() };
        let built = blaze_build::build(&view, done, &program.sig(i), &mut subst, &adts, Global)
            .map_err(|error| RunError::Build { function: name(), error })?;
        let mut via = Vec::with_capacity_in(built.refs.len(), Global);
        let mut targets = Vec::with_capacity_in(built.refs.len(), Global);
        for &callee in built.refs.iter() {
            let Some(c) = program.index_of(callee.item) else {
                return Err(RunError::Callee { function: name(), callee: callee.item });
            };
            if program.fns[c].body.is_some() {
                let slot = match instances.iter().position(|&known| known == callee) {
                    Some(slot) => slot,
                    None => {
                        instances.push(callee);
                        instances.len() - 1
                    }
                };
                via.push(Via::Slot);
                targets.push(Target::Slot(u32::try_from(slot).map_err(|_| RunError::TooManyInstances)?));
            } else {
                via.push(Via::Address);
                targets.push(Target::Symbol(program.fns[c].symbol()));
            }
        }
        let mut code = Code::new_in(Global);
        blaze_emit::emit(built.func.view(), &via, &mut code).map_err(|error| RunError::Emit { function: name(), error })?;
        bodies.push(Body { code, targets });
    }
    let placed = place::place(&bodies).map_err(RunError::Place)?;
    // `main`'s instance was the first reached.
    placed.call(0, entry).map_err(RunError::Place)
}
