//! The deterministic simulation of macro calls under edits: a token changed, an expander
//! rebuilt, lines added above a call, a variable's value changed. Each build asks every call
//! through the machine ([`crate::machine::lookup`], [`crate::machine::settle`]) against an
//! in-memory store, running the model expander on a miss.
//!
//! The oracle: a call runs exactly when what it truly depends on (its expander's build, its
//! tokens, and the answers of what it read: a variable, its span's line) is a combination no
//! earlier build ran it with; and every output, from a run or a record, is the output a fresh
//! run would give now.

use std::{alloc::Global, marker::PhantomData, sync::Arc};

use blaze_keyed::{
    kind::{Expander, Expansion, ExpansionInputs, Kind, ProcKind, StartState},
    proof::{self, Checks, Snapshot},
};
use blaze_rng::SplitMix64;
use hegel::TestCase;

use super::support::{Broke, Draw, Drawn, MemStore, control, filing, tool};
use crate::{
    Digest, NOTHING, Subject, What,
    machine::{Found, file, lookup, payload, settle},
    macro_call::{Ended, MacroValue, MacroValueRef, StateOut, call_context},
    reads::{Answers, Ask, Reads, SpanQuestion, SpanRef, Unanswerable},
    tokens::{AbsoluteOffset, AnchorId, CtxId, InSpan, OutputWriter, RelativeRange, Role, Token, TokenKind, stream_digest},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// The expander's module digest is left out of the key.
    ModuleOutOfKey,
    /// The variable the expander reads is not listed.
    AnswerNotRecorded,
    /// One instance serves every call of a build, and what it counts carries into the next call.
    StateCarried,
    /// Spans are hashed by absolute position.
    AbsolutePositions,
}

/// What a module's macro reads, the same for every call (an expander is a function of its
/// key and what it reads).
#[derive(Clone, Copy, Debug)]
struct Module {
    reads_env: bool,
    asks_line: bool,
}

#[derive(Clone, Debug)]
struct Call {
    module: usize,
    words: Vec<u64>,
    /// Where the call's item starts in its file; ten bytes a line.
    offset: u32,
}

#[derive(Clone, Debug)]
enum Edit {
    Token { call: usize, at: usize, to: u64 },
    Rebuild { module: usize },
    /// Lines added above `call`: it and every call after it move.
    Shift { call: usize, lines: u32 },
    Env { to: u64 },
}

#[derive(Clone, Debug)]
struct Plan {
    modules: Vec<Module>,
    calls: Vec<Call>,
    edits: Vec<Edit>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let modules: Vec<Module> =
        (0..d.int(1, 3)).map(|_| Module { reads_env: d.coin(), asks_line: d.coin() }).collect();
    let calls = (0..d.int(2, 6))
        .map(|i| Call {
            module: d.under(modules.len()),
            words: (0..d.int(1, 5)).map(|_| d.int(0, 3)).collect(),
            offset: 100 * i as u32,
        })
        .collect::<Vec<_>>();
    let edits = (0..d.int(1, 12))
        .map(|_| match d.int(0, 3) {
            0 => {
                let call = d.under(calls.len());
                Edit::Token { call, at: d.under(calls[call].words.len()), to: d.int(0, 3) }
            }
            1 => Edit::Rebuild { module: d.under(modules.len()) },
            2 => Edit::Shift { call: d.under(calls.len()), lines: d.int(1, 3) as u32 },
            _ => Edit::Env { to: d.int(0, 2) },
        })
        .collect();
    Plan { modules, calls, edits }
}

/// What a call truly depends on: its expander's build, its tokens, and what it read.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Truth {
    module: usize,
    build: u64,
    words: Vec<u64>,
    env: Option<u64>,
    line: Option<u32>,
}

struct World {
    modules: Vec<Module>,
    builds: Vec<u64>,
    env: u64,
    calls: Vec<Call>,
}

/// The answers the database gives one call: the variable `X`, and its span's line.
struct CallAnswers {
    env: u64,
    line: u32,
}

impl Answers for CallAnswers {
    fn answer(&mut self, ask: &Ask<'_>) -> Result<Digest, Unanswerable> {
        Ok(match ask {
            Ask::Env(b"X") => Digest::of("test env", &self.env.to_le_bytes()),
            Ask::Span(SpanQuestion::LineColumn, _) => Digest::of("test line", &self.line.to_le_bytes()),
            _ => NOTHING,
        })
    }
}

fn expected_text(word: u64, build: u64, env: Option<u64>, line: Option<u32>, count: u64) -> String {
    let env = env.map_or("-".into(), |it| it.to_string());
    let line = line.map_or("-".into(), |it| it.to_string());
    format!("w{word}.b{build}.e{env}.l{line}.c{count}")
}

fn harness(why: impl std::fmt::Display) -> Broke {
    Broke::Harness(why.to_string())
}

/// One build, one request's snapshot: every call asked once, in order.
fn build(
    snap: Snapshot<'_>,
    world: &World,
    store: &MemStore,
    seen: &mut Vec<Truth>,
    plant: Plant,
    step: usize,
) -> Result<(), Broke> {
    let subject = Subject { what: What::ProcMacro, name: Arc::from("model!") };
    let host = Digest::of("test host", b"1");
    let mut carried = 0;
    let mut checks = Checks::default();
    for (i, call) in world.calls.iter().enumerate() {
        let anchor = AnchorId(i as u64 + 1);
        let anchor_at = AbsoluteOffset(call.offset);
        let texts: Vec<String> = call.words.iter().map(|w| format!("w{w}")).collect();
        let spans: Vec<InSpan> = (0..call.words.len() as u32)
            .map(|j| {
                let start = AbsoluteOffset(call.offset + 5 + 3 * j);
                let end = AbsoluteOffset(start.0 + 2);
                let range = match plant {
                    Plant::AbsolutePositions => RelativeRange { start: start.0, end: end.0 },
                    _ => RelativeRange::from_absolute(start, end, anchor_at).expect("after the anchor"),
                };
                InSpan { anchor, range, ctx: CtxId(0) }
            })
            .collect();
        let tokens = || {
            texts.iter().zip(&spans).map(|(text, span)| Token { kind: TokenKind::Ident, text, span: *span })
        };
        let site = InSpan { anchor, range: RelativeRange { start: 0, end: 0 }, ctx: CtxId(0) };
        let (input, numbering) = stream_digest(Role::Input, Some([site; 3]), tokens());
        let build_no = world.builds[call.module];
        let mut module_bytes = (call.module as u64).to_le_bytes().to_vec();
        module_bytes.extend_from_slice(&build_no.to_le_bytes());
        let module = match plant {
            Plant::ModuleOutOfKey => NOTHING,
            _ => Digest::of("test module", &module_bytes),
        };
        let key = Expansion::key(&ExpansionInputs {
            tool: tool(),
            expander: Expander::Proc { module, export: 0, kind: ProcKind::Bang, host },
            input,
            attr: None,
            context: call_context(2021, false, StartState::Fresh),
            of: PhantomData,
        });
        let line = call.offset / 10;
        let mut answers = CallAnswers { env: world.env, line };
        let behaviour = world.modules[call.module];
        let env = behaviour.reads_env.then_some(world.env);
        let asked_line = behaviour.asks_line.then_some(line);
        let truth = Truth { module: call.module, build: build_no, words: call.words.clone(), env, line: asked_line };
        let expected: Vec<String> = call.words.iter().map(|&w| expected_text(w, build_no, env, asked_line, 0)).collect();

        let ran = match lookup(snap, store, &mut answers, &subject, key, &mut checks, Global).map_err(harness)? {
            Found::Hit { .. } => false,
            Found::Miss(missed) => {
                // The model expander, as the host serves it: each read a list entry.
                let mut reads = Reads::default();
                if behaviour.reads_env && plant != Plant::AnswerNotRecorded {
                    let ask = Ask::Env(b"X");
                    reads.push(&ask, answers.answer(&ask).map_err(|it| harness(it.0))?);
                }
                if behaviour.asks_line {
                    let ask = Ask::Span(SpanQuestion::LineColumn, SpanRef { anchor: 0, range: spans[0].range });
                    reads.push(&ask, answers.answer(&ask).map_err(|it| harness(it.0))?);
                }
                let count = match plant {
                    Plant::StateCarried => {
                        carried += 1;
                        carried - 1
                    }
                    _ => 0,
                };
                let outs: Vec<String> =
                    call.words.iter().map(|&w| expected_text(w, build_no, env, asked_line, count)).collect();
                let mut writer = OutputWriter::new(&numbering);
                for (text, span) in outs.iter().zip(&spans) {
                    writer
                        .token(&Token { kind: TokenKind::Ident, text, span: *span })
                        .map_err(harness)?;
                }
                let value = MacroValue {
                    ended: Ended::Output(writer),
                    diagnostics: &[],
                    printed: b"",
                    state_out: StateOut::Unchanged,
                    state_bytes: b"",
                }
                .encode();
                let put = settle(&subject, key, reads, value, &mut answers, &mut checks).map_err(harness)?;
                let filer = missed.filer(put.settled, put.list.shape()).map_err(harness)?;
                file(store, &subject, filer, &put, &mut checks, Global).map_err(filing)?;
                true
            }
        };
        // Read the call's value back through a lookup, as every later asker would.
        let Found::Hit { holds, record } =
            lookup(snap, store, &mut answers, &subject, key, &mut checks, Global).map_err(harness)?
        else {
            return Err(harness(format!("step {step}, call {i}: no record right after its put")));
        };
        let value = MacroValueRef::read(payload(&subject, &holds, &record).map_err(harness)?).map_err(harness)?;
        let output = value.output.map_err(|panic| harness(format!("panicked: {panic}")))?;
        let mut got = Vec::new_in(Global);
        for token in output.tokens() {
            got.push(token.map_err(harness)?.text.to_string());
        }
        if got != expected {
            return Err(Broke::Oracle(format!(
                "step {step}, call {i} ({truth:?}): gave {got:?}, a fresh run gives {expected:?}"
            )));
        }
        let should_run = !seen.contains(&truth);
        if ran != should_run {
            return Err(Broke::Oracle(format!(
                "step {step}, call {i} ({truth:?}): {} though it {}",
                if ran { "ran" } else { "was answered from a record" },
                if should_run { "was never run with these inputs" } else { "ran with these inputs before" },
            )));
        }
        if should_run {
            seen.push(truth);
        }
    }
    Ok(())
}

fn run(plan: &Plan, plant: Plant) -> Result<(), Broke> {
    let mut world =
        World { modules: plan.modules.clone(), builds: vec![0; plan.modules.len()], env: 0, calls: plan.calls.clone() };
    let store = MemStore::new();
    let mut seen = Vec::new();
    proof::scope(|snap| build(snap, &world, &store, &mut seen, plant, 0))?;
    for (step, edit) in plan.edits.iter().enumerate() {
        match *edit {
            Edit::Token { call, at, to } => world.calls[call].words[at] = to,
            Edit::Rebuild { module } => world.builds[module] += 1,
            Edit::Shift { call, lines } => {
                for it in &mut world.calls[call..] {
                    it.offset += 10 * lines;
                }
            }
            Edit::Env { to } => world.env = to,
        }
        proof::scope(|snap| build(snap, &world, &store, &mut seen, plant, step + 1))?;
    }
    Ok(())
}

/// The invariant: a call is redone exactly when its key or a listed answer changed, and what
/// a record gives is what a run gives.
#[hegel::test(test_cases = 200)]
fn a_call_is_redone_exactly_when_its_key_or_a_listed_answer_changed(tc: TestCase) {
    let plan = plan(&mut Drawn(&tc));
    if let Err(broke) = run(&plan, Plant::None) {
        panic!("{broke}\nplan {plan:?}");
    }
}

fn seeded(rng: &mut SplitMix64, plant: Plant) -> Result<(), Broke> {
    run(&plan(rng), plant)
}

#[test]
fn the_oracle_catches_the_module_left_out_of_the_key() {
    control(Plant::ModuleOutOfKey, seeded);
}

#[test]
fn the_oracle_catches_a_host_answer_not_recorded() {
    control(Plant::AnswerNotRecorded, seeded);
}

#[test]
fn the_oracle_catches_expander_state_carried_across_calls() {
    control(Plant::StateCarried, seeded);
}

#[test]
fn the_oracle_catches_spans_hashed_by_absolute_position() {
    control(Plant::AbsolutePositions, seeded);
}
