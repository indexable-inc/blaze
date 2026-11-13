//! Running out of memory is an outcome, through every growth path (decision D29,
//! docs/v2/plan/design/fallible-bstd.md): a task whose frame hands out at most a small limit
//! grows bstd's collections past it, and every growth that finds no room returns an error
//! naming that task's frame and leaves the collection as it was. The task goes on, its scope
//! joins, and another task spawned afterwards grows its own collections.
//!
//! The paths: [`Bytes::extend`](crate::mem::Bytes::extend) and `grow_zeroed`, [`Vec`] `push`,
//! `extend_from_slice` and `reserve`, [`String`] `push_str`, [`format_in!`], [`Map::insert`],
//! and a directory listing ([`Dir::list`]) into the task's frame.
//!
//! Plant: [`Plant::PushThroughAlloc`], a push routed back to `alloc`'s own `Vec::push`. Its
//! effect is the process's end, so its control runs it in a child process.

use hegel::TestCase;

use super::{Draw, Drawn, check, path, root, world, write_synced};
use crate::{
    Plant, Stopped,
    cx::Cx,
    fs::{Dir, FsError, FsKind, ReadWrite},
    map::Map,
    mem::{ByteLen, FormatError, OutOfMemory, String, TASK_FRAME, Vec, format_in},
};

/// The growing task's name: what every refusal must say.
const GROWER: &str = "grower";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    BytesExtend(usize),
    BytesGrowZeroed(usize),
    VecPush(u8),
    VecExtend(usize),
    VecReserve(usize),
    StringPushStr(usize),
    FormatIn(usize),
    MapInsert(u64),
    List,
}

/// The growth paths, for a tally of which were refused.
const KINDS: usize = 9;

impl Op {
    fn kind(self) -> usize {
        match self {
            Op::BytesExtend(_) => 0,
            Op::BytesGrowZeroed(_) => 1,
            Op::VecPush(_) => 2,
            Op::VecExtend(_) => 3,
            Op::VecReserve(_) => 4,
            Op::StringPushStr(_) => 5,
            Op::FormatIn(_) => 6,
            Op::MapInsert(_) => 7,
            Op::List => 8,
        }
    }
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    /// The most the growing task's frame hands out.
    limit: usize,
    /// Files in the directory the task lists.
    files: usize,
    ops: std::vec::Vec<Op>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let limit = d.int(64, 16 << 10) as usize;
    let files = d.int(0, 24) as usize;
    let mut ops = std::vec::Vec::new();
    for _ in 0..d.int(1, 48) {
        let size = |d: &mut dyn FnMut() -> u64| d() as usize;
        let mut drawn = || d.int(0, 1500);
        ops.push(match drawn() % 9 {
            0 => Op::BytesExtend(size(&mut drawn)),
            1 => Op::BytesGrowZeroed(size(&mut drawn)),
            2 => Op::VecPush(drawn() as u8),
            3 => Op::VecExtend(size(&mut drawn)),
            4 => Op::VecReserve(size(&mut drawn)),
            5 => Op::StringPushStr(size(&mut drawn)),
            6 => Op::FormatIn(size(&mut drawn)),
            7 => Op::MapInsert(drawn()),
            _ => Op::List,
        });
    }
    Plan { seed, limit, files, ops }
}

/// What the growing task saw: growths taken and refused, by path.
#[derive(Clone, Copy, Default, Debug)]
struct Seen {
    grown: [usize; KINDS],
    refused: [usize; KINDS],
}

/// One push the way `plant` says: bstd's, or `alloc`'s own.
fn push<'f>(bytes: &mut Vec<u8, &'f crate::mem::Frame>, cx: &'f Cx, byte: u8, plant: Plant) -> Result<(), OutOfMemory> {
    match plant {
        Plant::PushThroughAlloc => {
            let mut raw = std::mem::replace(bytes, Vec::new_in(cx.frame())).into_std();
            raw.push(byte);
            *bytes = Vec::from_std(raw);
            Ok(())
        }
        _ => bytes.push(byte),
    }
}

/// The growing task: `plan`'s operations on collections in its own frame, each checked
/// against a twin on the global heap.
///
/// The oracle: every growth returns; an `Ok` leaves the collection equal to the model after
/// the same change; an `Err` names the task's frame, asked for something, and leaves the
/// collection equal to the model before it; the frame never hands out more than its limit.
fn grow(cx: &Cx, plan: &Plan, dir: &Dir<ReadWrite>, plant: Plant) -> Result<Seen, std::string::String> {
    let frame = cx.frame();
    let mut bytes = cx.bytes();
    let mut vec = Vec::new_in(frame);
    let mut text = String::new_in(frame);
    let mut map: Map<u64, u64, _> = Map::new_in(frame);
    let (mut bytes_model, mut vec_model, mut text_model) =
        (std::vec::Vec::new(), std::vec::Vec::new(), std::string::String::new());
    let mut map_model = std::collections::BTreeMap::new();
    let mut seen = Seen::default();
    for (step, &op) in plan.ops.iter().enumerate() {
        let said = format!("step {step}, {op:?}, limit {}", plan.limit);
        // A refusal by one of the collections, or by `Bytes`, which names the span.
        let named = |arena: &'static str, asked: usize| match (arena, asked) {
            (GROWER, 1..) => Ok(()),
            _ => Err(format!("{said}: refused naming {arena:?} and asking {asked} bytes, not {GROWER:?}")),
        };
        let stopped = |error: Stopped| match error {
            Stopped::OutOfMemory { arena, asked } => named(arena, asked),
            other => Err(format!("{said}: stopped with {other}, not out of memory")),
        };
        let outcome: Result<(), Result<(), std::string::String>> = match op {
            Op::BytesExtend(len) => {
                let more = vec![0xb1; len];
                bytes.extend(&more).map(|()| bytes_model.extend_from_slice(&more)).map_err(stopped)
            }
            Op::BytesGrowZeroed(len) => bytes
                .grow_zeroed(len)
                .map(|lent| {
                    lent.fill(0xb2);
                    bytes_model.resize(bytes_model.len() + len, 0xb2);
                })
                .map_err(stopped),
            Op::VecPush(byte) => push(&mut vec, cx, byte, plant)
                .map(|()| vec_model.push(byte))
                .map_err(|error| named(error.arena, error.asked)),
            Op::VecExtend(len) => {
                let more = vec![0xc3; len];
                vec.extend_from_slice(&more)
                    .map(|()| vec_model.extend_from_slice(&more))
                    .map_err(|error| named(error.arena, error.asked))
            }
            Op::VecReserve(more) => match vec.reserve(more) {
                Ok(()) if vec.capacity() - vec.len() >= more => Ok(()),
                Ok(()) => return Err(format!("{said}: said it reserved, with room for {}", vec.capacity() - vec.len())),
                Err(error) => Err(named(error.arena, error.asked)),
            },
            Op::StringPushStr(len) => {
                let more = "é".repeat(len / 2);
                text.push_str(&more)
                    .map(|()| text_model.push_str(&more))
                    .map_err(|error| named(error.arena, error.asked))
            }
            Op::FormatIn(width) => match format_in!(frame, "{:width$}|{step}", "") {
                Ok(made) if made.len() == width + 1 + step.to_string().len() => Ok(()),
                Ok(made) => return Err(format!("{said}: formatted {} bytes", made.len())),
                Err(FormatError::Full(error)) => Err(named(error.arena, error.asked)),
                Err(FormatError::Trait) => return Err(format!("{said}: no formatting trait here fails")),
            },
            Op::MapInsert(key) => match map.insert(key, step as u64) {
                Ok(was) if was == map_model.insert(key, step as u64) => Ok(()),
                Ok(was) => return Err(format!("{said}: the map had {was:?} under {key}")),
                Err(error) => Err(named(error.arena, error.asked)),
            },
            Op::List => match dir.list(cx, frame) {
                Ok(found) if found.len() == plan.files => Ok(()),
                Ok(found) => return Err(format!("{said}: listed {} of {} files", found.len(), plan.files)),
                Err(FsError { kind: FsKind::Stopped(error), .. }) => Err(stopped(error)),
                Err(other) => return Err(format!("{said}: the listing failed with {other}")),
            },
        };
        match outcome {
            Ok(()) => seen.grown[op.kind()] += 1,
            Err(named) => {
                named?;
                seen.refused[op.kind()] += 1;
            }
        }
        if bytes.as_slice() != bytes_model.as_slice() {
            return Err(format!("{said}: the bytes are not the model's"));
        }
        if vec.as_slice() != vec_model.as_slice() {
            return Err(format!("{said}: the vector is not the model's"));
        }
        if text.as_str() != text_model.as_str() {
            return Err(format!("{said}: the text is not the model's"));
        }
        if map.len() != map_model.len() || map_model.iter().any(|(key, value)| map.get(key) != Some(value)) {
            return Err(format!("{said}: the map is not the model's"));
        }
        if frame.used() > plan.limit {
            return Err(format!("{said}: the frame hands out {} bytes, past its limit {}", frame.used(), plan.limit));
        }
    }
    Ok(seen)
}

/// Another task, after the grower: its own frame grows its own collections.
fn bystander(cx: &Cx) -> Result<usize, std::string::String> {
    let mut vec = Vec::new_in(cx.frame());
    let mut text = String::new_in(cx.frame());
    for at in 0..2048u32 {
        vec.push(at).map_err(|error| format!("another task's push was refused: {error}"))?;
        text.push_str("ok").map_err(|error| format!("another task's text was refused: {error}"))?;
    }
    Ok(vec.len() + text.len())
}

fn run_seen(plan: &Plan, plant: Plant) -> Result<Seen, std::string::String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let fs = io.need_fs().map_err(|e| format!("harness: {e}"))?;
    let dir = fs.open_dir(&cx, &path("listed")).map_err(|e| format!("harness: {e}"))?;
    for at in 0..plan.files {
        write_synced(&cx, &dir, &path(&format!("file-{at}")), b"x").map_err(|e| format!("harness: {e}"))?;
    }
    // A scope is linear, so nothing returns while it is live: outcomes are kept and judged
    // after its join.
    let scope = cx.scope().map_err(|e| format!("harness: {e}"))?;
    let grower = scope.spawn(GROWER, ByteLen::of_memory(plan.limit), |cx| grow(cx, plan, &dir, plant));
    let grown = grower.join(&cx);
    let after = scope.spawn("bystander", TASK_FRAME, bystander);
    let other = after.join(&cx);
    let joined = scope.join(&cx);
    let seen = grown.map_err(|error| format!("the growing task did not go on to its end: {error}"))?;
    match other {
        Ok(4096..) => {}
        other => return Err(format!("the task after the grower did not grow its own: {other:?}")),
    }
    joined.map_err(|error| format!("the scope did not join: {error}"))?;
    Ok(seen)
}

fn run(plan: &Plan, plant: Plant) -> Result<(), std::string::String> {
    run_seen(plan, plant).map(|_| ())
}

/// The invariant: a growth past its arena's limit is an error naming the arena, whatever the
/// growth path, and the collection is as it was; the task goes on, its scope joins, and the
/// next task grows its own.
#[hegel::test(test_cases = 200)]
fn a_growth_past_a_tasks_frame_is_an_error_naming_it_and_the_tasks_go_on(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

/// A plan that takes every growth path past the limit: each of them many times over.
fn every_path() -> Plan {
    let ops = (0..40u64)
        .flat_map(|round| {
            [
                Op::BytesExtend(300),
                Op::BytesGrowZeroed(300),
                Op::VecPush(round as u8),
                Op::VecExtend(300),
                Op::VecReserve(600),
                Op::StringPushStr(300),
                Op::FormatIn(300),
                Op::MapInsert(round),
                Op::List,
            ]
        })
        // Pushes one at a time, until they alone pass the limit.
        .chain((0..8192).map(|at| Op::VecPush(at as u8)))
        .collect();
    Plan { seed: 7, limit: 4096, files: 24, ops }
}

/// Set for the child process the control below starts: its run has the plant.
const PLANT_ENV: &str = "BSTD_GROWTH_PLANT";
const THIS_TEST: &str = "tests::growth::every_growth_path_is_refused_past_the_limit_and_the_task_goes_on";

/// Each of the nine growth paths is refused at least once by a 4096-byte frame, naming it,
/// and the task, its scope and the next task go on. Run as itself this is the sound case;
/// the control below runs it again in a child process with the plant.
#[test]
fn every_growth_path_is_refused_past_the_limit_and_the_task_goes_on() {
    let plant = match std::env::var(PLANT_ENV).as_deref() {
        Ok("push-through-alloc") => Plant::PushThroughAlloc,
        Ok(other) => panic!("{PLANT_ENV}={other}: not a plant of this test"),
        Err(_) => Plant::None,
    };
    let seen = run_seen(&every_path(), plant).unwrap_or_else(|why| panic!("{why}"));
    assert!(
        seen.refused.iter().all(|&refused| refused > 0) && seen.grown.iter().sum::<usize>() > 0,
        "every path is refused at least once, and some growth is taken: {seen:?}"
    );
    println!("survived: {seen:?}");
}

/// The cause (rule test-the-cause) and the control: with a push routed back to `alloc`'s
/// infallible `Vec::push`, the same plan ends the process in `handle_alloc_error` (SIGABRT,
/// "memory allocation of .. bytes failed"), which the oracle "the task goes on" cannot
/// survive; without the plant the same child ends well. A real process, because only the
/// kernel shows an abort: nothing in the process that aborted can report it.
#[cfg(unix)]
#[test]
fn the_oracle_catches_a_push_routed_to_allocs_infallible_one() {
    use std::os::unix::process::ExitStatusExt;

    /// SIGABRT, on every unix blaze targets.
    const SIGABRT: i32 = 6;
    let child = |plant: Option<&str>| {
        #[expect(clippy::disallowed_methods, reason = "the test binary run again as its own child")]
        let mut command = std::process::Command::new(std::env::current_exe().expect("this binary"));
        command.args([THIS_TEST, "--exact", "--nocapture", "--test-threads=1"]);
        match plant {
            Some(plant) => command.env(PLANT_ENV, plant),
            None => command.env_remove(PLANT_ENV),
        };
        command.output().expect("the child ran")
    };

    let sound = child(None);
    let said = std::string::String::from_utf8_lossy(&sound.stdout);
    assert!(sound.status.success(), "without the plant the child ends well: {:?}", sound.status);
    assert!(said.contains("survived: Seen"), "the child ran the plan: {said}");

    let planted = child(Some("push-through-alloc"));
    let said = std::string::String::from_utf8_lossy(&planted.stderr);
    assert_eq!(
        planted.status.signal(),
        Some(SIGABRT),
        "with the plant the process is aborted: {:?}\n{said}",
        planted.status
    );
    assert!(
        said.contains("memory allocation of") && said.contains("failed"),
        "by alloc's handle_alloc_error: {said}"
    );
}
