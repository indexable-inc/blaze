// Writing a large record once, in place (`Dir::reserve`): backed by `blaze_core::reserve::Reserve`
// over `Effect::MapWrite` so no whole record is materialized on the heap.

use blaze_core::{
    Done, Effect, FileName, Tag,
    reserve::{Next, Reserve, ReserveError, Why},
};
use blaze_own::Share as _;

use super::{Dir, FsError, FsKind, FsOp, ReadWrite, answered_error, call, driver_error};
use crate::{cx::Cx, path::Path};

fn retag(effect: Effect, tag: Tag) -> Effect {
    match effect {
        Effect::MapWrite { file, len, .. } => Effect::MapWrite { file, len, tag },
        Effect::Truncate { file, len, .. } => Effect::Truncate { file, len, tag },
        Effect::Sync { file, .. } => Effect::Sync { file, tag },
        Effect::Rename { from, to, .. } => Effect::Rename { from, to, tag },
        Effect::Remove { file, .. } => Effect::Remove { file, tag },
        other => other,
    }
}

fn reserve_error(key: &FileName, err: ReserveError) -> FsError {
    match err.why {
        Why::Failed(io_err) => FsError::new(FsOp::Publish, key, FsKind::Driver(io_err.0)),
        Why::Answer(got) => answered_error(FsOp::Publish, key, &got),
        Why::TooLong { .. } | Why::NotNow => driver_error(FsOp::Publish, key, &err),
    }
}

impl Dir<ReadWrite> {
    /// Reserves `room` bytes beside `path` via [`blaze_core::reserve::Reserve`] (`Effect::MapWrite`),
    /// lets `write` populate the writable mapping in place, and publishes it atomically at `path`
    /// (truncated to the written byte count, synced, and renamed) when `write` returns `Ok(len)`.
    /// When `write` returns `Err(why)`, the mapping is unmapped, the scratch file is removed, and
    /// `Ok(Err(why))` is returned with `path` untouched.
    pub fn reserve<E>(
        &self,
        cx: &Cx,
        path: &Path,
        room: u64,
        write: impl FnOnce(&mut [u8]) -> Result<u64, E>,
    ) -> Result<Result<(), E>, FsError> {
        let key = self.beneath(path);
        if let Some(quota) = self.quota.as_deref()
            && let Err(over) = quota.take(room)
        {
            return Err(FsError::new(FsOp::Publish, &key, FsKind::OverQuota(over)));
        }
        let fresh = self.io.fresh();
        let unique = if self.io.is_sim() {
            fresh
        } else {
            (u64::from(crate::rt::process_id()) << 32) | (fresh & 0xffff_ffff)
        };
        let (mut reserve, first) = Reserve::new(key.share(), room, Tag(unique));
        let submit = |effect: Effect| -> Result<Done, FsError> {
            if matches!(effect, Effect::Sync { .. }) && self.io.plant() == super::Plant::SyncSkipped {
                return Ok(Done::Synced);
            }
            call(&self.io, cx, FsOp::Publish, &key, |tag| retag(effect, tag))
        };
        let mut write_fn = Some(write);
        let mut done = match submit(first) {
            Ok(done) => done,
            Err(error) => {
                if let Some(cleanup) = reserve.abandon(None) {
                    let _ = submit(cleanup);
                }
                return Err(error);
            }
        };
        loop {
            match reserve.answer(done) {
                Ok(Next::Write(mut mapping)) => {
                    let outcome = match write_fn.take() {
                        Some(f) => f(mapping.bytes_mut()),
                        None => {
                            if let Some(cleanup) = reserve.abandon(Some(mapping)) {
                                let _ = submit(cleanup);
                            }
                            return Err(driver_error(FsOp::Publish, &key, &"reserve write invoked twice"));
                        }
                    };
                    match outcome {
                        Ok(len) => {
                            let effect = match reserve.finish(mapping, len) {
                                Ok(effect) => effect,
                                Err(err) => {
                                    if let Some(cleanup) = reserve.abandon(None) {
                                        let _ = submit(cleanup);
                                    }
                                    return Err(reserve_error(&key, err));
                                }
                            };
                            done = match submit(effect) {
                                Ok(done) => done,
                                Err(error) => {
                                    if let Some(cleanup) = reserve.abandon(None) {
                                        let _ = submit(cleanup);
                                    }
                                    return Err(error);
                                }
                            };
                        }
                        Err(user_err) => {
                            if let Some(cleanup) = reserve.abandon(Some(mapping)) {
                                let _ = submit(cleanup);
                            }
                            return Ok(Err(user_err));
                        }
                    }
                }
                Ok(Next::Ask(effect)) => {
                    done = match submit(effect) {
                        Ok(done) => done,
                        Err(error) => {
                            if let Some(cleanup) = reserve.abandon(None) {
                                let _ = submit(cleanup);
                            }
                            return Err(error);
                        }
                    };
                }
                Ok(Next::Stored) => {
                    self.io.waited_for_device();
                    self.io.names_synced();
                    return Ok(Ok(()));
                }
                Ok(Next::Abandoned) => {
                    return Err(driver_error(FsOp::Publish, &key, &"reservation abandoned unexpectedly"));
                }
                Err(err) => {
                    if let Some(cleanup) = reserve.abandon(None) {
                        let _ = submit(cleanup);
                    }
                    return Err(reserve_error(&key, err));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::time::Duration;

    use blaze_core::sim::Faults;
    use blaze_sim::{Broke, Config, Tape};

    use crate::Plant;
    use crate::fs::{Dir, FsKind, ReadWrite};
    use crate::io::{Io, Limits, SimWorld};
    use crate::mem::{Bytes, TASK_FRAME};
    use crate::path::Path;
    use crate::test::BUDGET;

    /// `Dir::reserve` writes in place, truncates to the written prefix, and cleans up scratch
    /// files on both publish and user-error abandon.
    /// Expected: `test fs::reserve::tests::reserve_publishes_and_abandons_cleanly ... ok`.
    #[test]
    fn reserve_publishes_and_abandons_cleanly() {
        let io = Io::simulated(SimWorld::new(7, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
        let root = io.need_fs().expect("fs").root();
        let target = Path::new("records/k1").expect("path");

        let payload = b"hello reserved record";
        let outcome: Result<(), &str> = root
            .reserve(&cx, &target, 4096, |buf| {
                buf[..payload.len()].copy_from_slice(payload);
                Ok(u64::try_from(payload.len()).unwrap_or(0))
            })
            .expect("reserve fs ok");
        assert_eq!(outcome, Ok(()));
        let mut read = Bytes::new_in(cx.frame(), "read reserved");
        root.read_all(&cx, &target, &mut read).expect("read");
        assert_eq!(read.as_slice(), payload);

        // User error abandons the reservation without modifying `target` or leaving `.part-`.
        let aborted: Result<(), &str> = root
            .reserve(&cx, &target, 4096, |buf| {
                buf[0] = b'X';
                Err("refused")
            })
            .expect("abandon fs ok");
        assert_eq!(aborted, Err("refused"));
        let mut still = Bytes::new_in(cx.frame(), "read still");
        root.read_all(&cx, &target, &mut still).expect("read still");
        assert_eq!(still.as_slice(), payload);
        let listed = root.entries(&cx, &Path::new("records").expect("dir")).expect("entries").expect("some");
        assert_eq!(listed.len(), 1, "no .part- scratch file left behind");

        // Quota exceeded fails before MapWrite and reports `OverQuota`.
        let tight = root.quota(4);
        let over_target = Path::new("records/k2").expect("path");
        let err = tight
            .reserve::<()>(&cx, &over_target, 4096, |buf| {
                buf[..payload.len()].copy_from_slice(payload);
                Ok(u64::try_from(payload.len()).unwrap_or(0))
            })
            .expect_err("quota exceeded");
        assert!(matches!(err.kind, FsKind::OverQuota(_)));
        assert!(root.stat(&cx, &over_target).expect("stat").is_none());
    }

    #[derive(Clone, Copy, Debug)]
    enum ActorChoice {
        Step(usize),
        Start(usize, usize, bool),
        Crash,
    }

    struct InterleavePlan {
        seed: u64,
        fail_one_in: u32,
        fail_syncs: bool,
        steps: Vec<ActorChoice>,
    }

    fn draw_interleave_plan(tape: &Tape) -> InterleavePlan {
        let seed = tape.next_u64("interleave sim seed");
        let fail_one_in = *tape.one_of(&[0, 0, 6, 10], "fail one in");
        let fail_syncs = tape.one_in(4, "fail syncs");
        let count = tape.int_in(6..=28u64, "steps");
        let mut steps = Vec::new();
        for _ in 0..count {
            let choice = match tape.int_in(0..=9u64, "choice") {
                0..=2 => ActorChoice::Step(0),
                3..=5 => ActorChoice::Step(1),
                6 => ActorChoice::Start(0, tape.int_in(1..=8192u64, "len 0") as usize, tape.one_in(4, "refuse 0")),
                7 => ActorChoice::Start(1, tape.int_in(1..=8192u64, "len 1") as usize, tape.one_in(4, "refuse 1")),
                _ => ActorChoice::Crash,
            };
            steps.push(choice);
        }
        steps.push(ActorChoice::Crash);
        InterleavePlan { seed, fail_one_in, fail_syncs, steps }
    }

    fn payload_of(seq: u8, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let fill = seq.wrapping_mul(37).wrapping_add(1);
        for i in 0..len {
            out.push(fill.wrapping_add((i % 251) as u8));
        }
        out
    }

    fn ask_sim(sim: &mut blaze_core::sim::Sim, effect: Effect, plant: Plant) -> Done {
        if matches!(effect, Effect::Sync { .. }) && plant == Plant::SyncSkipped {
            return Done::Synced;
        }
        let mut effects = alloc::vec![effect];
        let mut got = None;
        let _ = blaze_core::run_until_idle(sim, &mut effects, |completion, _| got = Some(completion.done));
        got.expect("sim completes one effect")
    }

    fn read_sim_target(sim: &mut blaze_core::sim::Sim, key: &FileName) -> Option<Vec<u8>> {
        // Read target directly without injected faults for oracle verification.
        for _ in 0..64 {
            let done = ask_sim(sim, Effect::ReadAll { file: key.share(), tag: Tag(0xffff_ffff) }, Plant::None);
            match done {
                Done::Read(bytes) if bytes.is_empty() => {
                    let st = ask_sim(sim, Effect::Stat { file: key.share(), tag: Tag(0xffff_fffe) }, Plant::None);
                    if matches!(st, Done::Stat(None)) {
                        return None;
                    }
                    if matches!(st, Done::Stat(Some(_))) {
                        return Some(bytes);
                    }
                }
                Done::Read(bytes) => return Some(bytes),
                Done::Failed(_) => continue,
                _ => continue,
            }
        }
        None
    }

    struct SimActor {
        reserve: Reserve,
        pending: Option<Effect>,
        payload: Vec<u8>,
        refuse: bool,
        renamed_submitted: bool,
    }

    fn run_interleave_dst(plan: &InterleavePlan, plant: Plant) -> Result<(), String> {
        let mut sim = blaze_core::sim::Sim::new(
            plan.seed,
            Faults { fail_one_in: plan.fail_one_in, ..Faults::default() },
        );
        let dir_name = FileName::new("records").expect("dir");
        let key = FileName::new("records/k").expect("key");
        if plan.fail_syncs {
            sim.fail_syncs(dir_name.share());
        }

        let mut actors: [Option<SimActor>; 2] = [None, None];
        let mut next_tag: u64 = 1;
        let mut seq: u8 = 0;
        // Exact set of whole states allowed at `key` after a crash right now.
        // Initially `None` (absent). Once any actor commits (`Next::Stored`), the pre-race value
        // is evicted: only that committed payload (and payloads of actors whose rename raced
        // concurrently with or after it) remain valid — NEVER the stale pre-race value.
        let mut allowed: Vec<Option<Vec<u8>>> = alloc::vec![None];

        for (step_idx, &choice) in plan.steps.iter().enumerate() {
            match choice {
                ActorChoice::Start(slot, len, refuse) => {
                    if actors[slot].is_some() {
                        continue;
                    }
                    seq = seq.wrapping_add(1);
                    let payload = payload_of(seq, len);
                    let room = u64::try_from(len.saturating_add(4096)).unwrap_or(u64::MAX);
                    let tag = Tag(next_tag);
                    next_tag += 1;
                    let (reserve, first) = Reserve::new(key.share(), room, tag);
                    actors[slot] = Some(SimActor {
                        reserve,
                        pending: Some(first),
                        payload,
                        refuse,
                        renamed_submitted: false,
                    });
                }
                ActorChoice::Step(slot) => {
                    let Some(mut actor) = actors[slot].take() else {
                        continue;
                    };
                    let Some(effect) = actor.pending.take() else {
                        continue;
                    };
                    let is_rename = matches!(effect, Effect::Rename { .. });
                    let done = ask_sim(&mut sim, effect, plant);
                    if is_rename && matches!(done, Done::Renamed) {
                        actor.renamed_submitted = true;
                        let cand = Some(actor.payload.clone());
                        if !allowed.contains(&cand) {
                            allowed.push(cand);
                        }
                    }
                    match actor.reserve.answer(done) {
                        Ok(Next::Write(mut mapping)) => {
                            mapping.bytes_mut()[..actor.payload.len()].copy_from_slice(&actor.payload);
                            if actor.refuse {
                                actor.pending = actor.reserve.abandon(Some(mapping));
                                if actor.pending.is_some() {
                                    actors[slot] = Some(actor);
                                }
                            } else {
                                let len = u64::try_from(actor.payload.len()).unwrap_or(0);
                                match actor.reserve.finish(mapping, len) {
                                    Ok(next_eff) => {
                                        actor.pending = Some(next_eff);
                                        actors[slot] = Some(actor);
                                    }
                                    Err(err) => return Err(format!("step {step_idx}: finish: {err}")),
                                }
                            }
                        }
                        Ok(Next::Ask(next_eff)) => {
                            actor.pending = Some(next_eff);
                            actors[slot] = Some(actor);
                        }
                        Ok(Next::Stored) => {
                            // Durable commit! Evict any pre-race value strictly older than this
                            // commit. Only this actor's payload and any OTHER in-flight actor that
                            // has already submitted a concurrent `Rename` may appear after a crash.
                            let mut next_allowed = alloc::vec![Some(actor.payload.clone())];
                            let other_slot = 1 - slot;
                            if let Some(other) = actors[other_slot].as_ref()
                                && other.renamed_submitted
                            {
                                let cand = Some(other.payload.clone());
                                if !next_allowed.contains(&cand) {
                                    next_allowed.push(cand);
                                }
                            }
                            allowed = next_allowed;
                        }
                        Ok(Next::Abandoned) => {}
                        Err(_) => {
                            if let Some(cleanup) = actor.reserve.abandon(None) {
                                actor.pending = Some(cleanup);
                                actors[slot] = Some(actor);
                            }
                        }
                    }
                }
                ActorChoice::Crash => {
                    // Drop any in-flight actor mappings before crashing (the process died).
                    actors = [None, None];
                    sim.crash();
                    if plan.fail_syncs {
                        sim.fail_syncs(dir_name.share());
                    }
                    let got = read_sim_target(&mut sim, &key);
                    if !allowed.contains(&got) {
                        return Err(format!(
                            "step {step_idx}: crash left torn or stale record ({:?} bytes, {} allowed states)",
                            got.as_ref().map(Vec::len),
                            allowed.len()
                        ));
                    }
                    // Whatever survived the crash is now the sole durable ground truth.
                    allowed = alloc::vec![got];
                }
            }
        }
        Ok(())
    }

    fn read_bstd_target(io: &Io, dir: &Dir<ReadWrite>, target: &Path) -> Result<Option<Vec<u8>>, String> {
        for _ in 0..64 {
            let cx = io.cx(BUDGET, TASK_FRAME).map_err(|e| format!("cx: {e}"))?;
            let mut bytes = Bytes::new_in(cx.frame(), "reserve dst read");
            match dir.read_all(&cx, target, &mut bytes) {
                Ok(_) => return Ok(Some(bytes.as_slice().to_vec())),
                Err(err) if err.kind == FsKind::NotFound => return Ok(None),
                Err(err) if matches!(err.kind, FsKind::Driver(_)) => continue,
                Err(err) => return Err(format!("read_all: {err}")),
            }
        }
        Err("read_all: exhausted retries on injected fault".into())
    }

    fn run_bstd_crash_check(seed: u64, plant: Plant) -> Result<(), String> {
        let mut world = SimWorld::new(seed, Limits::USUAL);
        world.faults = Faults { max_latency: Duration::from_micros(50), ..Faults::default() };
        let io = Io::planted(world, plant);
        let cx = io.cx(BUDGET, TASK_FRAME).map_err(|e| format!("cx: {e}"))?;
        let dir = io.need_fs().map_err(|e| format!("fs: {e}"))?.root().subdir(&Path::new("records").map_err(|e| format!("{e}"))?);
        let target = Path::new("k").map_err(|e| format!("{e}"))?;
        let payload = payload_of(1, 8192);
        let res: Result<Result<(), &str>, _> = dir.reserve(&cx, &target, 16_384, |buf| {
            buf[..payload.len()].copy_from_slice(&payload);
            Ok(u64::try_from(payload.len()).unwrap_or(0))
        });
        if !matches!(res, Ok(Ok(()))) {
            return Err(format!("reserve failed: {res:?}"));
        }
        if !io.crash() {
            return Err("not simulated".into());
        }
        let got = read_bstd_target(&io, &dir, &target)?;
        if got.as_ref() != Some(&payload) {
            return Err(format!("crash after Dir::reserve lost/tore durable record (got {:?})", got.as_ref().map(Vec::len)));
        }
        Ok(())
    }

    /// Deterministic crash, fault-injection, and tape-driven concurrent-interleaving DST for
    /// `Dir::reserve` / `Reserve`: interleaves two concurrent reservations effect-by-effect on the
    /// tape with mid-flight crashes, rejects any stale pre-race value after a commit, and verifies
    /// that a planted skipped-sync bug (`Plant::SyncSkipped`) is caught by the crash oracle.
    #[test]
    fn reserve_crash_faults_and_interleaving_dst() {
        blaze_sim::check(Config::cases(120), |tape| {
            let plan = draw_interleave_plan(tape);
            run_interleave_dst(&plan, Plant::None).map_err(Broke::Oracle)
        });

        for seed in 1u64..=16 {
            run_bstd_crash_check(seed, Plant::None).expect("bstd Dir::reserve survives crash whole");
        }

        // Planted-bug control: skipping `Effect::Sync` before `Effect::Rename` must be caught on
        // both the tape-driven interleaving DST and `Dir::reserve`.
        let mut caught_interleave = false;
        let mut caught_bstd = false;
        for seed in 1u64..=64 {
            let plan = InterleavePlan {
                seed,
                fail_one_in: 0,
                fail_syncs: false,
                steps: alloc::vec![
                    ActorChoice::Start(0, 8192, false),
                    ActorChoice::Step(0),
                    ActorChoice::Step(0),
                    ActorChoice::Step(0),
                    ActorChoice::Step(0),
                    ActorChoice::Crash,
                ],
            };
            if run_interleave_dst(&plan, Plant::SyncSkipped).is_err() {
                caught_interleave = true;
            }
            if run_bstd_crash_check(seed, Plant::SyncSkipped).is_err() {
                caught_bstd = true;
            }
            if caught_interleave && caught_bstd {
                break;
            }
        }
        assert!(caught_interleave, "planted control Plant::SyncSkipped must be caught by interleave DST");
        assert!(caught_bstd, "planted control Plant::SyncSkipped must be caught by Dir::reserve DST");
    }
}
