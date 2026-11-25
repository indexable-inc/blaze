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
    enum ReserveAct {
        Commit(usize),
        Refuse(usize),
        Interleave(usize, usize),
        Crash,
    }

    struct ReservePlan {
        seed: u64,
        latency_us: u64,
        fail_one_in: u32,
        fail_syncs: bool,
        acts: Vec<ReserveAct>,
    }

    fn draw_plan(tape: &Tape) -> ReservePlan {
        let seed = tape.next_u64("reserve sim seed");
        let latency_us = *tape.one_of(&[0, 50, 500], "latency us");
        let fail_one_in = *tape.one_of(&[0, 0, 5, 9], "fail one in");
        let fail_syncs = tape.one_in(4, "fail syncs");
        let count = tape.int_in(3..=14u64, "acts");
        let mut acts = Vec::new();
        for _ in 0..count {
            let act = match tape.int_in(0..=9u64, "act") {
                0..=3 => ReserveAct::Commit(tape.int_in(1..=12_288u64, "commit len") as usize),
                4..=5 => ReserveAct::Refuse(tape.int_in(1..=8_192u64, "refuse len") as usize),
                6..=7 => ReserveAct::Interleave(
                    tape.int_in(1..=8_192u64, "left len") as usize,
                    tape.int_in(1..=8_192u64, "right len") as usize,
                ),
                _ => ReserveAct::Crash,
            };
            acts.push(act);
        }
        acts.push(ReserveAct::Crash);
        ReservePlan { seed, latency_us, fail_one_in, fail_syncs, acts }
    }

    fn payload_of(seq: u8, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let fill = seq.wrapping_mul(37).wrapping_add(1);
        for i in 0..len {
            out.push(fill.wrapping_add((i % 251) as u8));
        }
        out
    }

    fn read_target(io: &Io, dir: &Dir<ReadWrite>, target: &Path) -> Result<Option<Vec<u8>>, String> {
        // Retry if the simulator injects a transient fault on the oracle's verification read.
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

    fn run_reserve_dst(plan: &ReservePlan, plant: Plant) -> Result<(), String> {
        let mut world = SimWorld::new(plan.seed, Limits::USUAL);
        world.faults = Faults {
            fail_one_in: plan.fail_one_in,
            max_latency: Duration::from_micros(plan.latency_us),
            ..Faults::default()
        };
        let io = Io::planted(world, plant);
        let cx = io.cx(BUDGET, TASK_FRAME).map_err(|e| format!("root cx: {e}"))?;
        let dir = io
            .need_fs()
            .map_err(|e| format!("fs: {e}"))?
            .root()
            .subdir(&Path::new("records").map_err(|e| format!("dir path: {e}"))?);
        let target = Path::new("k").map_err(|e| format!("target path: {e}"))?;
        let apply_sync_fault = |io: &Io| {
            if plan.fail_syncs {
                if let Some(name) = dir.name().name() {
                    io.with_sim(|sim| sim.fail_syncs(name.clone()));
                }
            }
        };
        apply_sync_fault(&io);

        let mut durable: Option<Vec<u8>> = None;
        let mut allowed: Vec<Option<Vec<u8>>> = alloc::vec![None];
        let mut seq: u8 = 0;

        for (step_idx, &act) in plan.acts.iter().enumerate() {
            match act {
                ReserveAct::Commit(len) => {
                    seq = seq.wrapping_add(1);
                    let bytes = payload_of(seq, len);
                    let room = u64::try_from(len.saturating_add(4096)).unwrap_or(u64::MAX);
                    let res: Result<Result<(), &str>, _> = dir.reserve(&cx, &target, room, |buf| {
                        buf[..bytes.len()].copy_from_slice(&bytes);
                        Ok(u64::try_from(bytes.len()).unwrap_or(0))
                    });
                    match res {
                        Ok(Ok(())) => {
                            durable = Some(bytes.clone());
                            allowed.clear();
                            allowed.push(durable.clone());
                        }
                        Ok(Err(_)) => return Err(format!("step {step_idx}: unexpected user err")),
                        Err(err) if matches!(err.kind, FsKind::Driver(_)) => {
                            // A faulted rename/sync may or may not have flipped the target name.
                            allowed.push(Some(bytes));
                        }
                        Err(err) => return Err(format!("step {step_idx}: unexpected fs err: {err}")),
                    }
                }
                ReserveAct::Refuse(len) => {
                    seq = seq.wrapping_add(1);
                    let bytes = payload_of(seq, len);
                    let room = u64::try_from(len.saturating_add(4096)).unwrap_or(u64::MAX);
                    let res: Result<Result<(), &str>, _> = dir.reserve(&cx, &target, room, |buf| {
                        buf[..bytes.len()].copy_from_slice(&bytes);
                        Err("refused")
                    });
                    match res {
                        Ok(Err("refused")) => {}
                        Err(err) if matches!(err.kind, FsKind::Driver(_)) => {}
                        other => return Err(format!("step {step_idx}: refuse outcome {other:?}")),
                    }
                }
                ReserveAct::Interleave(len_a, len_b) => {
                    seq = seq.wrapping_add(1);
                    let bytes_a = payload_of(seq, len_a);
                    seq = seq.wrapping_add(1);
                    let bytes_b = payload_of(seq, len_b);
                    let room_a = u64::try_from(len_a.saturating_add(4096)).unwrap_or(u64::MAX);
                    let room_b = u64::try_from(len_b.saturating_add(4096)).unwrap_or(u64::MAX);
                    let dir_a = dir.clone();
                    let dir_b = dir.clone();
                    let target_a = target.clone();
                    let target_b = target.clone();
                    let io_a = io.clone();
                    let io_b = io.clone();
                    let (res_a, res_b) = std::thread::scope(|s| {
                        let t1 = s.spawn(|| {
                            let c1 = io_a.cx(BUDGET, TASK_FRAME).expect("cx1");
                            dir_a.reserve::<&str>(&c1, &target_a, room_a, |buf| {
                                buf[..bytes_a.len()].copy_from_slice(&bytes_a);
                                Ok(u64::try_from(bytes_a.len()).unwrap_or(0))
                            })
                        });
                        let t2 = s.spawn(|| {
                            let c2 = io_b.cx(BUDGET, TASK_FRAME).expect("cx2");
                            dir_b.reserve::<&str>(&c2, &target_b, room_b, |buf| {
                                buf[..bytes_b.len()].copy_from_slice(&bytes_b);
                                Ok(u64::try_from(bytes_b.len()).unwrap_or(0))
                            })
                        });
                        (t1.join().expect("t1"), t2.join().expect("t2"))
                    });
                    let mut committed_any = false;
                    for (res, payload) in [(res_a, &bytes_a), (res_b, &bytes_b)] {
                        match res {
                            Ok(Ok(())) => {
                                committed_any = true;
                                allowed.push(Some(payload.clone()));
                            }
                            Err(err) if matches!(err.kind, FsKind::Driver(_)) => {
                                allowed.push(Some(payload.clone()));
                            }
                            other => return Err(format!("step {step_idx}: interleaved outcome {other:?}")),
                        }
                    }
                    if committed_any {
                        // Once either concurrent reservation commits durably, target must hold
                        // one of the newly committed (or racing) full payloads, never None or torn.
                        allowed.retain(|cand| cand.as_ref().is_some_and(|b| b == &bytes_a || b == &bytes_b) || cand == &durable);
                        durable = None;
                    }
                }
                ReserveAct::Crash => {
                    if !io.crash() {
                        return Err("world is not simulated".into());
                    }
                    let got = read_target(&io, &dir, &target)?;
                    if let Some(ref want) = durable {
                        if got.as_ref() != Some(want) {
                            return Err(format!(
                                "step {step_idx}: crash lost or tore durable record (want {} bytes, got {:?})",
                                want.len(),
                                got.as_ref().map(Vec::len)
                            ));
                        }
                    } else if !allowed.contains(&got) {
                        return Err(format!(
                            "step {step_idx}: crash left torn/unexpected record ({:?} bytes, {} valid states)",
                            got.as_ref().map(Vec::len),
                            allowed.len()
                        ));
                    }
                    durable = got.clone();
                    allowed.clear();
                    allowed.push(got);
                    apply_sync_fault(&io);
                }
            }
        }
        Ok(())
    }

    /// Crash, fault-injection, and concurrent-interleaving DST for `Dir::reserve`: every
    /// committed reservation survives a crash whole, failed/refused/interleaved reservations
    /// never leave a torn record, and a planted skipped-sync bug is caught by the oracle.
    #[test]
    fn reserve_crash_faults_and_interleaving_dst() {
        blaze_sim::check(Config::cases(80), |tape| {
            let plan = draw_plan(tape);
            run_reserve_dst(&plan, Plant::None).map_err(Broke::Oracle)
        });

        // Planted-bug control: skipping `Effect::Sync` before `Effect::Rename` must be caught.
        let mut caught = false;
        for seed in 1u64..=64 {
            let plan = ReservePlan {
                seed,
                latency_us: 0,
                fail_one_in: 0,
                fail_syncs: false,
                acts: alloc::vec![ReserveAct::Commit(8192), ReserveAct::Crash],
            };
            if run_reserve_dst(&plan, Plant::SyncSkipped).is_err() {
                caught = true;
                break;
            }
        }
        assert!(caught, "planted control Plant::SyncSkipped must be caught by the crash oracle");
    }
}
