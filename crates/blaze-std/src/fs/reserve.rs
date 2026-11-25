// Writing a large record once, in place (`Dir::reserve`): backed by `blaze_core::reserve::Reserve`
// over `Effect::MapWrite` so no whole record is materialized on the heap.

use core::time::Duration;

use blaze_core::{
    Done, Effect, FileName, MappingWrite, Tag,
    reserve::{Next, Reserve, ReserveError, Why, part_tag},
    wait::Deadline,
};
use blaze_own::Share as _;

use super::{Dir, DirName, Durable, EntryKind, FsError, FsKind, FsOp, Meta, ReadWrite, answered_error, driver_error, stat};
use crate::{Stopped, cx::Cx, io::Io, path::Path};

/// Bounded teardown window for settling an in-flight reservation effect or removing an abandoned
/// scratch file even after the caller's `Cx` has stopped or panicked.
const TEARDOWN_WAIT: Duration = Duration::from_secs(5);

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

#[derive(Debug)]
enum SubmitStop {
    /// `cx` stopped before submitting, or the in-flight effect settled conclusively before
    /// returning, so it is safe to run cleanup (`Effect::Remove`).
    Settled(FsError),
    /// The in-flight effect did not settle within [`TEARDOWN_WAIT`] / `poll_fuel` and was
    /// forgotten; it may still complete in the driver, so callers MUST NOT race it with
    /// `Effect::Remove`.
    Unsettled(FsError),
}

/// Submits one reservation effect and waits under `cx`; if `cx` stops (cancel, deadline, stall)
/// while the effect is in flight, settles that effect on `io` before returning. Distinguishes a
/// settled stop (safe to remove the scratch file) from an unsettled stop (must not race cleanup
/// against an in-flight effect).
fn submit_and_settle(io: &Io, cx: &Cx, key: &FileName, effect: Effect) -> Result<Done, SubmitStop> {
    if matches!(effect, Effect::Sync { .. }) && io.plant() == super::Plant::SyncSkipped {
        return Ok(Done::Synced);
    }
    let began = io.now();
    cx.stopped_by(began)
        .map_err(|s| SubmitStop::Settled(FsError::new(FsOp::Publish, key, FsKind::Stopped(s))))?;
    let tag = io.submit(|tag| retag(effect, tag));
    let deadline = cx.deadline();
    let mut idle = 0;
    loop {
        if let Some(done) = io.take(tag) {
            return Ok(done);
        }
        let stopped = if cx.cancelled() {
            Some(Stopped::Cancelled { span: cx.name() })
        } else if deadline.passed(io.now()) {
            Some(Stopped::Deadline { span: cx.name(), at: deadline.get() })
        } else if idle == io.limits().poll_fuel {
            Some(Stopped::Stalled { span: cx.name(), polls: idle })
        } else {
            None
        };
        if let Some(stopped) = stopped {
            let err = FsError::new(FsOp::Publish, key, FsKind::Stopped(stopped));
            let until = Deadline::after(io.now(), TEARDOWN_WAIT);
            return match io.settle(tag, until, "bstd::fs::Dir::reserve settle") {
                Ok(done @ Done::Renamed) => Ok(done),
                Ok(_done) => Err(SubmitStop::Settled(err)),
                Err(_) => Err(SubmitStop::Unsettled(err)),
            };
        }
        match io.take_or_poll(tag, deadline) {
            Ok(done) => return Ok(done),
            Err(0) => idle += 1,
            Err(_) => idle = 0,
        }
    }
}

/// Submits and settles an abandonment `Effect::Remove` through a teardown deadline independent of
/// the caller's (possibly stopped or unwinding) `Cx`.
fn teardown_remove(io: &Io, key: &FileName, cleanup: Effect) -> Result<Done, FsError> {
    let tag = io.submit(|tag| retag(cleanup, tag));
    let until = Deadline::after(io.now(), TEARDOWN_WAIT);
    io.settle(tag, until, "bstd::fs::Dir::reserve cleanup")
        .map_err(|s| FsError::new(FsOp::Publish, key, FsKind::Stopped(s)))
}

/// Unwind-safe owner of an in-flight [`Reserve`] and its active [`MappingWrite`]: if the write
/// callback panics, `Drop` unmaps the mapping and settles `Effect::Remove` so no `.part-<tag>`
/// scratch file leaks inside a live process.
struct ReserveGuard<'a> {
    io: &'a Io,
    key: &'a FileName,
    reserve: Reserve,
    mapping: Option<MappingWrite>,
    active: bool,
}

impl ReserveGuard<'_> {
    fn abandon_now(&mut self) -> Result<(), FsError> {
        self.active = false;
        let mapping = self.mapping.take();
        if let Some(cleanup) = self.reserve.abandon(mapping) {
            let done = teardown_remove(self.io, self.key, cleanup)?;
            self.reserve.answer(done).map_err(|err| reserve_error(self.key, err))?;
        }
        Ok(())
    }
}

impl Drop for ReserveGuard<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let mapping = self.mapping.take();
        if let Some(cleanup) = self.reserve.abandon(mapping) {
            let _ = teardown_remove(self.io, self.key, cleanup);
        }
    }
}

impl Dir<ReadWrite> {
    /// Reserves `room` bytes beside `path` via [`blaze_core::reserve::Reserve`] (`Effect::MapWrite`),
    /// lets `write` populate the writable mapping in place, and publishes it atomically at `path`
    /// (truncated to the written byte count, synced, and renamed) when `write` returns `Ok(len)`.
    /// When `write` returns `Err(why)`, the mapping is unmapped, the scratch file is removed
    /// (propagating any cleanup I/O error), and `Ok(Err(why))` is returned with `path` untouched.
    /// If `write` panics, an unwind guard unmaps and removes the scratch file before unwinding.
    pub fn reserve<E>(
        &self,
        cx: &Cx,
        path: &Path,
        room: u64,
        write: impl FnOnce(&mut [u8]) -> Result<u64, E>,
    ) -> Result<Result<(), E>, FsError> {
        let key = self.beneath(path);
        cx.stopped_by(self.io.now())
            .map_err(|s| FsError::new(FsOp::Publish, &key, FsKind::Stopped(s)))?;
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
        let (reserve, first) = Reserve::new(key.share(), room, Tag(unique));
        let mut guard = ReserveGuard {
            io: &self.io,
            key: &key,
            reserve,
            mapping: None,
            active: true,
        };
        let mut write_fn = Some(write);
        let mut done = match submit_and_settle(&self.io, cx, &key, first) {
            Ok(done) => done,
            Err(SubmitStop::Settled(error)) => {
                let _ = guard.abandon_now();
                return Err(error);
            }
            Err(SubmitStop::Unsettled(error)) => {
                guard.active = false;
                return Err(error);
            }
        };
        loop {
            match guard.reserve.answer(done) {
                Ok(Next::Write(mapping)) => {
                    guard.mapping = Some(mapping);
                    let outcome = match write_fn.take() {
                        Some(f) => {
                            let Some(buf) = guard.mapping.as_mut().map(MappingWrite::bytes_mut) else {
                                let _ = guard.abandon_now();
                                return Err(driver_error(FsOp::Publish, &key, &"reserve mapping missing"));
                            };
                            f(buf)
                        }
                        None => {
                            let _ = guard.abandon_now();
                            return Err(driver_error(FsOp::Publish, &key, &"reserve write invoked twice"));
                        }
                    };
                    match outcome {
                        Ok(len) => {
                            let Some(mapping) = guard.mapping.take() else {
                                let _ = guard.abandon_now();
                                return Err(driver_error(FsOp::Publish, &key, &"reserve mapping missing"));
                            };
                            let effect = match guard.reserve.finish(mapping, len) {
                                Ok(effect) => effect,
                                Err(err) => {
                                    let _ = guard.abandon_now();
                                    return Err(reserve_error(&key, err));
                                }
                            };
                            done = match submit_and_settle(&self.io, cx, &key, effect) {
                                Ok(done) => done,
                                Err(SubmitStop::Settled(error)) => {
                                    let _ = guard.abandon_now();
                                    return Err(error);
                                }
                                Err(SubmitStop::Unsettled(error)) => {
                                    guard.active = false;
                                    return Err(error);
                                }
                            };
                        }
                        Err(user_err) => {
                            guard.abandon_now()?;
                            return Ok(Err(user_err));
                        }
                    }
                }
                Ok(Next::Ask(effect)) => {
                    done = match submit_and_settle(&self.io, cx, &key, effect) {
                        Ok(done) => done,
                        Err(SubmitStop::Settled(error)) => {
                            let _ = guard.abandon_now();
                            return Err(error);
                        }
                        Err(SubmitStop::Unsettled(error)) => {
                            guard.active = false;
                            return Err(error);
                        }
                    };
                }
                Ok(Next::Stored) => {
                    guard.active = false;
                    self.io.waited_for_device();
                    self.io.names_synced();
                    return Ok(Ok(()));
                }
                Ok(Next::Abandoned) => {
                    guard.active = false;
                    return Err(driver_error(FsOp::Publish, &key, &"reservation abandoned unexpectedly"));
                }
                Err(err) => {
                    let _ = guard.abandon_now();
                    return Err(reserve_error(&key, err));
                }
            }
        }
    }

    /// Sweeps abandoned reservation scratch files (`<key>.part-<tag>`) directly inside this
    /// directory whenever `should_remove(name, tag)` returns `true`.
    ///
    /// On native worlds this streams `readdir` in O(1) memory without materializing a bounded
    /// directory listing (scaling past `Listing`'s entry/name caps); on simulated worlds it falls
    /// back to `stat` + `own_entries`. Ignores `NotFound` when the directory does not exist yet or
    /// a scratch file disappears concurrently, and propagates all other filesystem errors.
    pub fn sweep_parts(
        &self,
        cx: &Cx,
        mut should_remove: impl FnMut(&str, u64) -> bool,
    ) -> Result<u32, FsError> {
        let rel = match &self.base {
            DirName::Root => None,
            DirName::Named(name) => Some(name),
        };
        if let Some(swept) = self.io.sweep_parts_confined(cx, rel, &mut should_remove) {
            return match swept {
                Ok(Ok(count)) => Ok(count),
                Ok(Err(err)) => Err(FsError::new(FsOp::Entries, &self.base, FsKind::Driver(err.0))),
                Err(stopped) => Err(FsError::new(FsOp::Entries, &self.base, FsKind::Stopped(stopped))),
            };
        }
        if let DirName::Named(ref base) = self.base {
            match stat(&self.io, cx, FsOp::Entries, base)? {
                None => return Ok(0),
                Some(Meta { dir: false, .. }) => return Err(FsError::new(FsOp::Entries, base, FsKind::NotADirectory)),
                Some(_) => {}
            }
        }
        let entries = match self.own_entries(cx) {
            Ok(entries) => entries,
            Err(err) if err.kind == FsKind::NotFound => return Ok(0),
            Err(err) => return Err(err),
        };
        let mut removed = 0u32;
        for i in 0..entries.len() {
            if entries.kind(i) != EntryKind::File {
                continue;
            }
            let name = entries.name(i);
            let Some(tag) = part_tag(name) else {
                continue;
            };
            if !should_remove(name, tag) {
                continue;
            }
            let Ok(path) = entries.path(i) else {
                continue;
            };
            match self.remove(cx, &path, Durable::No) {
                Ok(()) => removed = removed.saturating_add(1),
                Err(err) if err.kind == FsKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
        Ok(removed)
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

    fn list_sim_dir(sim: &mut blaze_core::sim::Sim, dir: &FileName) -> Vec<FileName> {
        for _ in 0..64 {
            let done = ask_sim(sim, Effect::List { dir: dir.share().into(), tag: Tag(0xffff_fffd) }, Plant::None);
            match done {
                Done::Listed(entries) => return entries.into_iter().map(|(n, _)| n).collect(),
                Done::Failed(_) => continue,
                _ => continue,
            }
        }
        Vec::new()
    }

    fn sweep_sim_parts(sim: &mut blaze_core::sim::Sim, dir: &FileName) -> Result<(), String> {
        for entry in list_sim_dir(sim, dir) {
            if part_tag(entry.as_str()).is_some() {
                let mut removed = false;
                for _ in 0..64 {
                    match ask_sim(sim, Effect::Remove { file: entry.share(), tag: Tag(0xffff_fffc) }, Plant::None) {
                        Done::Removed => {
                            removed = true;
                            break;
                        }
                        Done::Failed(_) => continue,
                        _ => continue,
                    }
                }
                if !removed {
                    return Err(format!("could not sweep orphan {}", entry.as_str()));
                }
            }
        }
        for entry in list_sim_dir(sim, dir) {
            if part_tag(entry.as_str()).is_some() {
                return Err(format!("orphan scratch file remained after sweep: {}", entry.as_str()));
            }
        }
        Ok(())
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
                    // Verify recovery sweep reclaims every orphan `.part-` file without touching `key`.
                    sweep_sim_parts(&mut sim, &dir_name)?;
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
        // Start a second REAL reservation (`records/inflight`) and stop mid-protocol right BEFORE
        // `Effect::Rename` (after `MapWrite`, write, `Truncate`, `Sync`, and syncing the directory
        // entry) without renaming or removing, then crash `io` to verify `sweep_parts` reclaims
        // the real in-flight `<key>.part-<tag>` scratch file without publishing `inflight`.
        let inflight_target = Path::new("inflight").map_err(|e| format!("{e}"))?;
        let inflight_key = dir.beneath(&inflight_target);
        let (mut inflight, first) = Reserve::new(inflight_key.share(), 4096, Tag(777));
        let done = submit_and_settle(&io, &cx, &inflight_key, first).map_err(|e| format!("inflight map: {e:?}"))?;
        let mapping = match inflight.answer(done) {
            Ok(Next::Write(mut m)) => {
                m.bytes_mut()[..12].copy_from_slice(b"half-written");
                m
            }
            other => return Err(format!("expected Next::Write, got {other:?}")),
        };
        let trunc = inflight.finish(mapping, 12).map_err(|e| format!("inflight finish: {e}"))?;
        let done = submit_and_settle(&io, &cx, &inflight_key, trunc).map_err(|e| format!("inflight trunc: {e:?}"))?;
        let sync_eff = match inflight.answer(done) {
            Ok(Next::Ask(eff)) => eff,
            other => return Err(format!("expected Sync ask, got {other:?}")),
        };
        let done = submit_and_settle(&io, &cx, &inflight_key, sync_eff).map_err(|e| format!("inflight sync: {e:?}"))?;
        match inflight.answer(done) {
            Ok(Next::Ask(Effect::Rename { .. })) => {} // Drop before submitting Rename: simulate SIGKILL!
            other => return Err(format!("expected Rename ask, got {other:?}")),
        }
        // Sync directory so the in-flight scratch name is durable on the simulated disk before crash.
        let dir_sync = Effect::SyncDir {
            dir: DirName::Named(FileName::new("records").expect("records")),
            tag: Tag(0),
        };
        let _ = submit_and_settle(&io, &cx, &inflight_key, dir_sync);
        if !io.crash() {
            return Err("not simulated".into());
        }
        let sweep_cx = io.cx(BUDGET, TASK_FRAME).map_err(|e| format!("sweep cx: {e}"))?;
        if dir.stat(&sweep_cx, &inflight_target).map_err(|e| format!("stat inflight: {e}"))?.is_some() {
            return Err("crashed pre-rename reservation must not publish target".into());
        }
        let swept = dir.sweep_parts(&sweep_cx, |_, _| true).map_err(|e| format!("sweep_parts: {e}"))?;
        if swept == 0 {
            return Err("sweep_parts did not remove real in-flight .part- scratch file".into());
        }
        if dir.sweep_parts(&sweep_cx, |_, _| true).map_err(|e| format!("sweep_parts 2: {e}"))? != 0 {
            return Err("orphan .part- file survived sweep_parts".into());
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
