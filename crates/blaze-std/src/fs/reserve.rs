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
        let (mut reserve, first) = Reserve::new(key.share(), room, Tag(fresh));
        let submit = |effect: Effect| -> Result<Done, FsError> {
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
    use crate::fs::FsKind;
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

        // Quota exceeded abandons cleanly and reports `OverQuota`.
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
}
