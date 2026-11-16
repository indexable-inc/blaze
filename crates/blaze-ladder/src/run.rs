//! The straight line: source files to a status, each stage the shape of the task the request
//! machine will ask for (docs/v2/plan/design/bottom.md section 9): lower the files, check each
//! body, build it to LIR, emit it, place every body behind its slot, call the entry.
//!
//! Every stage's refusal is a [`RunError`] naming the function it is about (rule 19); nothing
//! outside a stage's subset is skipped (rule 3).

use std::alloc::Global;

use blaze_build::{Adts, BuildError};
use blaze_emit::{Code, EmitError, Via};
use blaze_own::Alloc;
use blaze_typeck::toy::ToyCtx;
use blaze_typeck::{Adt, CheckError, DiagCode, ItemKey, Needs, Outcome, PNode};

use crate::lower::{self, Abi, LowerError, Program, SourceFile};
use crate::place::{self, Body, Entry, PlaceError, Target};

/// Why a program did not run.
#[derive(Debug)]
pub enum RunError {
    Lower(LowerError),
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
            RunError::Lower(error) => write!(f, "{error}"),
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
pub fn run(files: &[SourceFile<'_>], root: &str) -> Result<u8, RunError> {
    let program = lower::lower(files, root).map_err(RunError::Lower)?;
    let main = program.entry().ok_or(RunError::NoMain)?;
    let entry = entry_shape(&program, main)?;
    // Slot `n` is the `n`th function with a body.
    let mut slot_of = Vec::with_capacity_in(program.fns.len(), Global);
    let mut next = 0u32;
    for function in program.fns.iter() {
        slot_of.push(function.body.as_ref().map(|_| {
            next += 1;
            next - 1
        }));
    }
    let adts = ProgramAdts(&program);
    let mut bodies = Vec::with_capacity_in(next as usize, Global);
    for (i, function) in program.fns.iter().enumerate() {
        let Some(body) = function.body.as_ref() else { continue };
        let name = || function.path.clone();
        let view = body.view(function.key);
        let world = program.world(i);
        let mut cx = ToyCtx::new();
        let checked = match blaze_typeck::check(&view, &world, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
            Ok(Outcome::Done(checked)) => checked,
            Ok(Outcome::Need(_)) => return Err(RunError::Need { function: name() }),
            Err(error) => return Err(RunError::Check { function: name(), error }),
        };
        if let Some(first) = checked.diags.first() {
            return Err(RunError::Types { function: name(), first: first.code, count: checked.diags.len() as u32 });
        }
        let built = blaze_build::build(&view, &checked, &program.sig(i), &adts, Global)
            .map_err(|error| RunError::Build { function: name(), error })?;
        let mut via = Vec::with_capacity_in(built.refs.len(), Global);
        let mut targets = Vec::with_capacity_in(built.refs.len(), Global);
        for &callee in built.refs.iter() {
            let Some(c) = program.index_of(callee) else {
                return Err(RunError::Callee { function: name(), callee });
            };
            match slot_of[c] {
                Some(slot) => {
                    via.push(Via::Slot);
                    targets.push(Target::Slot(slot));
                }
                None => {
                    via.push(Via::Address);
                    targets.push(Target::Symbol(program.fns[c].symbol()));
                }
            }
        }
        let mut code = Code::new_in(Global);
        blaze_emit::emit(built.func.view(), &via, &mut code).map_err(|error| RunError::Emit { function: name(), error })?;
        bodies.push(Body { code, targets });
    }
    let placed = place::place(&bodies).map_err(RunError::Place)?;
    let Some(slot) = slot_of[main] else { return Err(RunError::MainShape { what: "it has no body" }) };
    placed.call(slot, entry).map_err(RunError::Place)
}
