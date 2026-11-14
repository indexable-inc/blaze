//! The deterministic simulation of a build script's runs under edits: a traced file changed,
//! a file it only looked for appearing, a file below a declared directory changed, a file it
//! never touched changed, a variable changed, its code changed. Each build asks the script
//! through the machine; a miss runs the model script, whose reads the "sandbox" traces.
//!
//! The oracle: the script runs exactly when its code or something it read or declared answers
//! a way no earlier run saw; its `OUT_DIR`, from a run or a record, is restored byte for byte
//! (paths, modes, bytes) as a fresh run leaves it, its directives are the ones a fresh run
//! prints, and the reads its declarations miss are reported, each as itself.

use std::{alloc::Global, borrow::Cow, marker::PhantomData, sync::Arc};

use blaze_build_script::output::{Directive, LinkArgTarget, Severity};
use blaze_hash::HashMap;
use blaze_keyed::{
    kind::{CodeKey, Key, Kind, ScriptInputs, ScriptRun, Settings, UnitManifest},
    proof::{self, Checks, Snapshot},
};
use blaze_rng::SplitMix64;
use hegel::TestCase;

use super::support::{Broke, Draw, Drawn, MemStore, control, filing, tool};
use crate::{
    Digest, NOTHING, Subject, What,
    machine::{Found, file, lookup, payload, settle},
    reads::{Answers, Ask, AskBuf, Reads, Unanswerable},
    script_run::{
        Blobs, Exit, Mode, OutTree, ScriptValue, ScriptValueRef, Undeclared, add_declarations, blob_digest,
        script_args, script_env, undeclared,
    },
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// The driver drops the trace's read outside the package (a C header), as cargo would.
    TraceDropped,
    /// The read list is the declarations alone, as cargo has it.
    DeclarationsOnly,
    /// An executable of `OUT_DIR` is kept as a plain file.
    ModeLost,
}

const A: &[u8] = b"$pkg/src/a.h";
const B: &[u8] = b"$pkg/src/b.h";
const C: &[u8] = b"/usr/include/c.h";
const OPT: &[u8] = b"$pkg/opt.toml";
const DATA_X: &[u8] = b"$pkg/data/x";
const DATA_Y: &[u8] = b"$pkg/data/y";
const UNRELATED: &[u8] = b"$pkg/unrelated.txt";
const PATHS: [&[u8]; 7] = [A, B, C, OPT, DATA_X, DATA_Y, UNRELATED];

#[derive(Clone, Debug)]
enum Edit {
    /// The file at `PATHS[path]` given these bytes, or removed.
    File { path: usize, to: Option<u64> },
    Cc(u64),
    Profile(u64),
    Code,
}

#[derive(Clone, Debug)]
struct Plan {
    edits: Vec<Edit>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let edits = (0..d.int(1, 14))
        .map(|_| match d.int(0, 5) {
            0..=2 => {
                let path = d.under(PATHS.len());
                // Only the optional file is ever absent: the script needs the others.
                let to = match PATHS[path] == OPT && d.coin() {
                    true => None,
                    false => Some(d.int(0, 2)),
                };
                Edit::File { path, to }
            }
            3 => Edit::Cc(d.int(0, 2)),
            4 => Edit::Profile(d.int(0, 2)),
            _ => Edit::Code,
        })
        .collect();
    Plan { edits }
}

#[derive(Clone)]
struct World {
    files: Vec<(&'static [u8], Option<u64>)>,
    cc: u64,
    profile: u64,
    code: u64,
}

impl World {
    fn file(&self, path: &[u8]) -> Option<u64> {
        self.files.iter().find(|it| it.0 == path).and_then(|it| it.1)
    }
}

fn below(path: &[u8], dir: &[u8]) -> bool {
    path.starts_with(dir) && (path.len() == dir.len() || path[dir.len()] == b'/')
}

/// The answers of the world now: what the view's tree and the request's environment say.
impl Answers for World {
    fn answer(&mut self, ask: &Ask<'_>) -> Result<Digest, Unanswerable> {
        let content = |it: Option<u64>| it.map_or(NOTHING, |v| Digest::of("test file", &v.to_le_bytes()));
        let var = |v: u64| Digest::of("test var", &v.to_le_bytes());
        Ok(match *ask {
            Ask::File(path) => content(self.file(path)),
            Ask::Look(path) => self.file(path).map_or(NOTHING, |_| Digest::of("test look", b"file")),
            Ask::Tree(dir) => {
                let mut hasher = blaze_ids::Hasher::new("test tree");
                for (path, it) in &self.files {
                    if below(path, dir) {
                        hasher.part(path).id(&content(*it));
                    }
                }
                hasher.finish()
            }
            Ask::Env(b"CC") => var(self.cc),
            Ask::Env(b"PROFILE_X") => var(self.profile),
            _ => NOTHING,
        })
    }
}

/// What a fresh run of the script on `world` leaves: its `OUT_DIR` sorted by path, its
/// directives, and the reads its sandbox traced.
struct Ran {
    out: Vec<(Vec<u8>, Mode, Vec<u8>)>,
    directives: Vec<Directive<'static>>,
    traced: Vec<Ask<'static>>,
}

fn script(world: &World) -> Ran {
    let opt = world.file(OPT).is_some();
    let gen_rs = format!(
        "a={:?} b={:?} c={:?} opt={opt} cc={} profile={} code={}",
        world.file(A),
        world.file(B),
        world.file(C),
        world.cc,
        world.profile,
        world.code,
    );
    let mut directives = vec![
        Directive::RerunIfChanged(Cow::Borrowed("src/a.h")),
        Directive::RerunIfChanged(Cow::Borrowed("data")),
        Directive::RerunIfEnvChanged(Cow::Borrowed("CC")),
        Directive::Env(Cow::Borrowed("GEN"), Cow::Borrowed("1")),
        Directive::LinkLib(Cow::Borrowed("z")),
        Directive::LinkArg(LinkArgTarget::SingleBin("tool".to_string()), Cow::Borrowed("-x")),
        Directive::Metadata(Cow::Borrowed("include"), Cow::Borrowed("$out")),
        Directive::Log(Severity::Warning, Cow::Borrowed("generated")),
    ];
    if opt {
        directives.push(Directive::Cfg(Cow::Borrowed("has_opt")));
    }
    Ran {
        out: vec![
            (b"bin/tool".to_vec(), Mode::Executable, format!("tool{}", world.code).into_bytes()),
            (b"empty".to_vec(), Mode::Dir, Vec::new()),
            (b"gen.rs".to_vec(), Mode::File, gen_rs.into_bytes()),
        ],
        directives,
        traced: vec![Ask::File(A), Ask::File(B), Ask::File(C), Ask::Look(OPT), Ask::Env(b"CC"), Ask::Env(b"PROFILE_X")],
    }
}

/// The reads the script's declarations miss, as [`undeclared`] must report them.
fn expected_undeclared() -> Vec<Undeclared> {
    vec![
        Undeclared { ask: AskBuf::of(&Ask::File(B)), in_package: true },
        Undeclared { ask: AskBuf::of(&Ask::File(C)), in_package: false },
        Undeclared { ask: AskBuf::of(&Ask::Look(OPT)), in_package: true },
        Undeclared { ask: AskBuf::of(&Ask::Env(b"PROFILE_X")), in_package: false },
    ]
}

/// What a run truly depends on: its code, and the answer of everything it read or declared.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Truth {
    code: u64,
    a: Option<u64>,
    b: Option<u64>,
    c: Option<u64>,
    opt: bool,
    data: (Option<u64>, Option<u64>),
    cc: u64,
    profile: u64,
}

fn truth(world: &World) -> Truth {
    Truth {
        code: world.code,
        a: world.file(A),
        b: world.file(B),
        c: world.file(C),
        opt: world.file(OPT).is_some(),
        data: (world.file(DATA_X), world.file(DATA_Y)),
        cc: world.cc,
        profile: world.profile,
    }
}

struct MemBlobs(HashMap<Digest, Vec<u8>, Global>);

impl Blobs for MemBlobs {
    fn blob(&self, digest: &Digest) -> Option<&[u8]> {
        self.0.get(digest).map(|it| it.as_slice())
    }
}

fn harness(why: impl std::fmt::Display) -> Broke {
    Broke::Harness(why.to_string())
}

fn build(
    snap: Snapshot<'_>,
    world: &mut World,
    store: &MemStore,
    blobs: &mut MemBlobs,
    seen: &mut Vec<Truth>,
    plant: Plant,
    step: usize,
) -> Result<(), Broke> {
    let subject = Subject { what: What::BuildScript, name: Arc::from("model v0.1.0") };
    let mut checks = Checks::default();
    let unit = Key::<UnitManifest>::from_id(Digest::of("test script unit", &world.code.to_le_bytes()));
    let given: [(&[u8], &[u8]); 2] = [(b"TARGET", b"x86_64-unknown-linux-gnu"), (b"OUT_DIR", b"$out")];
    let key = ScriptRun::key(&ScriptInputs {
        tool: tool(),
        script: CodeKey::of(&tool(), unit, &[]),
        package: Digest::of("test package", b"model 0.1.0"),
        host: Digest::of("test host", b"1"),
        env: script_env(&given),
        args: script_args(&[]),
        settings: Settings(Digest::of("test settings", b"dev")),
        of: PhantomData,
    });
    let expected = script(world);
    let ran = match lookup(snap, store, world, &subject, key, &mut checks, Global).map_err(harness)? {
        Found::Hit { .. } => false,
        Found::Miss(missed_lookup) => {
            let fresh = script(world);
            let mut reads = Reads::default();
            if plant != Plant::DeclarationsOnly {
                for ask in &fresh.traced {
                    if plant == Plant::TraceDropped && *ask == Ask::File(C) {
                        continue;
                    }
                    reads.push(ask, world.answer(ask).map_err(|it| harness(it.0))?);
                }
            }
            add_declarations(&mut reads, b"$pkg", &fresh.directives, world)
                .map_err(|(ask, why)| harness(format!("{ask}: {}", why.0)))?;
            let mut tree = OutTree::default();
            for (path, mode, bytes) in &fresh.out {
                let mode = match (plant, *mode) {
                    (Plant::ModeLost, Mode::Executable) => Mode::File,
                    (_, mode) => mode,
                };
                let digest = blob_digest(bytes);
                if mode != Mode::Dir {
                    blobs.0.insert(digest, bytes.clone());
                }
                tree.push(path, mode, digest, bytes.len() as u64);
            }
            tree.seal().map_err(|it| harness(format!("{it:?}")))?;
            let value =
                ScriptValue { exit: Exit::Code(0), directives: &fresh.directives, out_dir: &tree, stderr: b"warning" }
                    .encode();
            let put = settle(&subject, key, reads, value, world, &mut checks).map_err(harness)?;
            // The trace is the truth, and the declarations are checked against it.
            let missed = undeclared(&put.list, b"$pkg", &fresh.directives);
            if plant == Plant::None && missed.as_slice() != expected_undeclared().as_slice() {
                return Err(Broke::Oracle(format!(
                    "step {step}: reported {missed:?} undeclared, the script's are {:?}",
                    expected_undeclared()
                )));
            }
            let filer = missed_lookup.filer(put.settled, put.list.shape()).map_err(harness)?;
            file(store, &subject, filer, &put, &mut checks, Global).map_err(filing)?;
            true
        }
    };
    let Found::Hit { holds, record } =
        lookup(snap, store, world, &subject, key, &mut checks, Global).map_err(harness)?
    else {
        return Err(harness(format!("step {step}: no record right after its put")));
    };
    let value = ScriptValueRef::read(payload(&subject, &holds, &record).map_err(harness)?).map_err(harness)?;
    let mut restored = Vec::new();
    value
        .out_dir
        .restore(blobs, |entry, bytes| restored.push((entry.path.to_vec(), entry.mode, bytes.to_vec())))
        .map_err(|it| harness(format!("{it:?}")))?;
    if restored != expected.out {
        return Err(Broke::Oracle(format!(
            "step {step}: OUT_DIR restored as {restored:?}, a fresh run leaves {:?}",
            expected.out
        )));
    }
    let directives: Vec<Directive<'_>> = value.directives().collect();
    if directives != expected.directives {
        return Err(Broke::Oracle(format!(
            "step {step}: directives {directives:?}, a fresh run prints {:?}",
            expected.directives
        )));
    }
    let now = truth(world);
    let should_run = !seen.contains(&now);
    if ran != should_run {
        return Err(Broke::Oracle(format!(
            "step {step} ({now:?}): {} though it {}",
            if ran { "ran" } else { "was answered from a record" },
            if should_run { "never ran with these inputs" } else { "ran with these inputs before" },
        )));
    }
    if should_run {
        seen.push(now);
    }
    Ok(())
}

fn run(plan: &Plan, plant: Plant) -> Result<(), Broke> {
    let mut world = World { files: PATHS.iter().map(|&it| (it, Some(0))).collect(), cc: 0, profile: 0, code: 0 };
    world.files.iter_mut().find(|it| it.0 == OPT).expect("listed").1 = None;
    let store = MemStore::new();
    let mut blobs = MemBlobs(HashMap::new_in(Global));
    let mut seen = Vec::new();
    proof::scope(|snap| build(snap, &mut world, &store, &mut blobs, &mut seen, plant, 0))?;
    for (step, edit) in plan.edits.iter().enumerate() {
        match *edit {
            Edit::File { path, to } => world.files[path].1 = to,
            Edit::Cc(to) => world.cc = to,
            Edit::Profile(to) => world.profile = to,
            Edit::Code => world.code += 1,
        }
        proof::scope(|snap| build(snap, &mut world, &store, &mut blobs, &mut seen, plant, step + 1))?;
    }
    Ok(())
}

/// The invariant: a script reruns exactly when its code or a read or declared answer
/// changed, its `OUT_DIR` comes back byte for byte, and its undeclared reads are reported.
#[hegel::test(test_cases = 200)]
fn a_script_reruns_exactly_when_what_it_read_changed(tc: TestCase) {
    let plan = plan(&mut Drawn(&tc));
    if let Err(broke) = run(&plan, Plant::None) {
        panic!("{broke}\nplan {plan:?}");
    }
}

fn seeded(rng: &mut SplitMix64, plant: Plant) -> Result<(), Broke> {
    run(&plan(rng), plant)
}

#[test]
fn the_oracle_catches_a_traced_read_dropped() {
    control(Plant::TraceDropped, seeded);
}

#[test]
fn the_oracle_catches_reads_taken_from_the_declarations() {
    control(Plant::DeclarationsOnly, seeded);
}

#[test]
fn the_oracle_catches_out_dir_not_restored_byte_for_byte() {
    control(Plant::ModeLost, seeded);
}
