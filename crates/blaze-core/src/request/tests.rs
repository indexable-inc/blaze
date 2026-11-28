use core::time::Duration;

use blaze_machine::{DriveOpts, Handler, Machine, Perform, Turn, drive};
use blaze_own::testing::heap;
use blaze_own::{Alloc, with_brand};
use blaze_sim::{Broke, Config};

use super::{
    ByteLen, ByteOffset, Connect, ConnectOpts, CopyDataOpts, DmaBuf, DriverHandler,
    DriverHandlerError, FillRandom, FillRandomOpts, FixedBufIndex, FlushedProof, FsAnswer,
    FsError, FsRequest, IoAnswer, IoRequest, LENT_READ_MAX, ListOpts, Listen, ListenOpts,
    LockOpts, LockOutcome, MaxBytes, NetError, OpenOpts, ReadAllOpts, ReadIntoOpts, ReadOpts,
    RecvIntoOpts, RenameFlushedOpts, RenameUnsyncedOpts, SendFromOpts, SimHandler,
    SimHandlerError, Sleep, SleepOpts, TruncateOpts, WriteDurableOpts, WriteFromOpts,
};
use crate::handle::{
    Conn as ConnHandle, Dir, ExitKind, File, FixedFileSlot, Listener, ReadWrite, TaskId, Unflushed,
};
use crate::sim::{Faults, Sim};
use crate::{Address, DirName, Driver, FileName, LockMode, RANDOM_MOST};

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum DurabilityPlant {
    #[default]
    None,
    /// Planted bug: skip `Flush` and `SyncDir`, using `RenameUnsynced` instead.
    SkipFlushAndSync,
}

/// Real durable atomic-write machine driven by `SimHandler`:
/// `OpenWrite(tmp) -> WriteFrom(tmp, buf) -> Flush(tmp) -> Close(tmp) -> RenameFlushed(tmp -> target) -> SyncDir(root)`
/// Closes `tmp` linearly on EVERY error path!
enum AtomicWrite<'io, 'a> {
    Start {
        dir: Dir<'io, ReadWrite>,
        tmp: FileName,
        target: FileName,
        buf: DmaBuf<'a>,
        plant: DurabilityPlant,
    },
    Opening {
        dir: Dir<'io, ReadWrite>,
        tmp: FileName,
        target: FileName,
        buf: DmaBuf<'a>,
        plant: DurabilityPlant,
    },
    Writing {
        dir: Dir<'io, ReadWrite>,
        tmp: FileName,
        target: FileName,
        file: File<'io, Unflushed>,
        plant: DurabilityPlant,
    },
    Flushing {
        dir: Dir<'io, ReadWrite>,
        target: FileName,
    },
    ClosingBeforeRename {
        dir: Dir<'io, ReadWrite>,
        target: FileName,
        proof: Option<FlushedProof>,
        unsynced_from: Option<FileName>,
    },
    ClosingOnError {
        err: FsError,
    },
    Renaming {
        dir: Dir<'io, ReadWrite>,
        sync_dir: bool,
    },
    SyncingDir,
}

impl<'io, 'a> Machine for AtomicWrite<'io, 'a> {
    type Req = FsRequest<'io, 'a>;
    type Out = ();
    type Err = FsError;

    fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match self {
            AtomicWrite::Start { dir, tmp, target, buf, plant } => {
                let req = dir.open_write(OpenOpts { path: tmp.clone(), create: true, truncate: true });
                Turn::Ask {
                    request: FsRequest::OpenWrite(req),
                    next: AtomicWrite::Opening { dir, tmp, target, buf, plant },
                }
            }
            _ => unreachable!(),
        }
    }

    fn resume(
        self,
        answer: FsAnswer<'io, 'a, '_>,
        _cx: &mut (),
    ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match (self, answer) {
            (
                AtomicWrite::Opening { dir, tmp, target, buf, plant },
                FsAnswer::OpenedWrite(Ok(file)),
            ) => {
                let req = file.write_from(WriteFromOpts { at: ByteOffset(0), buf });
                Turn::Ask {
                    request: FsRequest::WriteFrom(req),
                    next: AtomicWrite::Writing { dir, tmp, target, file, plant },
                }
            }
            (AtomicWrite::Opening { buf, .. }, FsAnswer::OpenedWrite(Err(e))) => {
                buf.release();
                Turn::Fail(e)
            }
            (
                AtomicWrite::Writing { dir, tmp, target, file, plant },
                FsAnswer::Written(Ok(buf)),
            ) => {
                buf.release();
                if plant == DurabilityPlant::SkipFlushAndSync {
                    Turn::Ask {
                        request: FsRequest::Close(file.close()),
                        next: AtomicWrite::ClosingBeforeRename {
                            dir,
                            target,
                            proof: None,
                            unsynced_from: Some(tmp),
                        },
                    }
                } else {
                    Turn::Ask {
                        request: FsRequest::Flush(file.flush()),
                        next: AtomicWrite::Flushing { dir, target },
                    }
                }
            }
            (AtomicWrite::Writing { file, .. }, FsAnswer::Written(Err(be))) => {
                be.buf.release();
                Turn::Ask {
                    request: FsRequest::Close(file.close()),
                    next: AtomicWrite::ClosingOnError { err: be.error },
                }
            }
            (AtomicWrite::Flushing { dir, target }, FsAnswer::Flushed(Ok((file, proof)))) => {
                Turn::Ask {
                    request: FsRequest::Close(file.close()),
                    next: AtomicWrite::ClosingBeforeRename {
                        dir,
                        target,
                        proof: Some(proof),
                        unsynced_from: None,
                    },
                }
            }
            (AtomicWrite::Flushing { .. }, FsAnswer::Flushed(Err((file, err)))) => Turn::Ask {
                request: FsRequest::Close(file.close()),
                next: AtomicWrite::ClosingOnError { err },
            },
            (
                AtomicWrite::ClosingBeforeRename { dir, target, proof, unsynced_from },
                FsAnswer::Closed(Ok(())),
            ) => {
                if let Some(flushed) = proof {
                    let req = dir.rename_flushed(RenameFlushedOpts { flushed, to: target });
                    Turn::Ask {
                        request: FsRequest::RenameFlushed(req),
                        next: AtomicWrite::Renaming { dir, sync_dir: true },
                    }
                } else {
                    let from = unsynced_from.expect("planted path");
                    let req = dir.rename_unsynced(RenameUnsyncedOpts { from, to: target });
                    Turn::Ask {
                        request: FsRequest::RenameUnsynced(req),
                        next: AtomicWrite::Renaming { dir, sync_dir: false },
                    }
                }
            }
            (AtomicWrite::ClosingBeforeRename { .. }, FsAnswer::Closed(Err(e))) => Turn::Fail(e),
            (AtomicWrite::ClosingOnError { err }, FsAnswer::Closed(_)) => Turn::Fail(err),
            (AtomicWrite::Renaming { dir, sync_dir }, FsAnswer::Renamed(Ok(()))) => {
                if sync_dir {
                    Turn::Ask {
                        request: FsRequest::SyncDir(dir.sync_dir()),
                        next: AtomicWrite::SyncingDir,
                    }
                } else {
                    Turn::Done(())
                }
            }
            (AtomicWrite::Renaming { .. }, FsAnswer::Renamed(Err(e))) => Turn::Fail(e),
            (AtomicWrite::SyncingDir, FsAnswer::SyncedDir(Ok(()))) => Turn::Done(()),
            (AtomicWrite::SyncingDir, FsAnswer::SyncedDir(Err(e))) => Turn::Fail(e),
            _ => unreachable!("typed answer matches active waiting state"),
        }
    }
}

fn read_file_direct(sim: &Sim, path: &FileName) -> Option<std::vec::Vec<u8>> {
    sim.exists(path).then(|| sim.contents(path))
}

/// The invariant: every committed `AtomicWrite` survives any subsequent `Sim::crash()` intact
/// (byte-for-byte equal to the committed payload), and every failed or crashed uncommitted write
/// leaves either the previous durable payload or absent—never torn bytes, and zero leaked
/// handles on normal exit.
fn run_durability_dst(plant: DurabilityPlant) {
    let h = heap();
    blaze_sim::check(Config::cases(120), |tape| {
        let seed = tape.next_u64("sim seed");
        let fail_rate = if tape.coin("faults") { 5 } else { 0 };
        let mut sim = Sim::new(seed, Faults { fail_one_in: fail_rate, ..Faults::default() });
        let tmp = FileName::new("state.tmp").unwrap();
        let target = FileName::new("state.bin").unwrap();

        let mut last_committed: Option<std::vec::Vec<u8>> = None;
        let rounds = 1 + (tape.below(4, "rounds") as usize);
        let mut verdict: Result<(), Broke> = Ok(());

        for r in 0..rounds {
            // Use > 2 sectors (1536 bytes) so an unsynced crash tears sectors visibly.
            let byte = (0xA0 + r as u8) as u8;
            let payload = std::vec![byte; 1536];
            with_brand(|brand| {
                let mut handler = SimHandler::new(&mut sim, TaskId(1), brand, h.alloc());
                let root = handler.root();
                let buf = DmaBuf::from_slice_in(&payload, h.alloc());
                let machine = AtomicWrite::Start {
                    dir: root,
                    tmp: tmp.clone(),
                    target: target.clone(),
                    buf,
                    plant,
                };
                let res = drive(&mut handler, &mut (), machine, DriveOpts { max_steps: 16 });
                // Normal exit must have ZERO leaked handles.
                if let Err(e) = handler.finish_task(ExitKind::Normal) {
                    verdict = Err(Broke::Oracle(format!("handle leak on normal exit: {e}")));
                    return;
                }
                if res.is_ok() {
                    last_committed = Some(payload.clone());
                }
            });
            if verdict.is_err() {
                return verdict;
            }

            // Cut power after each round!
            sim.crash();
            let on_disk = read_file_direct(&sim, &target);
            if last_committed.as_ref() == Some(&payload) {
                // Round committed: disk MUST hold the exact new payload, never torn or rolled back.
                if on_disk.as_ref() != Some(&payload) {
                    return Err(Broke::Oracle(format!(
                        "durability lost after crash in round {r}: expected {} bytes of {:#x}, got {:?}",
                        payload.len(),
                        payload[0],
                        on_disk.as_ref().map(|v| (v.len(), v.first().copied()))
                    )));
                }
            } else {
                // Round failed (e.g. injected fault during Write, Flush, Rename, or trailing SyncDir):
                // disk must hold EITHER the previous durable payload OR the full untorn new payload
                // (if Rename already landed before a trailing SyncDir fault)—NEVER torn bytes.
                let matches_prev = on_disk == last_committed;
                let matches_full_new = on_disk.as_ref() == Some(&payload);
                if !matches_prev && !matches_full_new {
                    return Err(Broke::Oracle(format!(
                        "torn or corrupted state after failed round {r} crash: got {:?}",
                        on_disk.as_ref().map(|v| (v.len(), v.first().copied()))
                    )));
                }
                if matches_full_new {
                    last_committed = Some(payload);
                }
            }
        }
        Ok(())
    });
}

#[test]
fn atomic_write_typestate_durability_dst() {
    run_durability_dst(DurabilityPlant::None);
}

#[test]
#[should_panic(expected = "durability lost after crash")]
fn the_oracle_catches_unflushed_rename_on_crash() {
    run_durability_dst(DurabilityPlant::SkipFlushAndSync);
}

#[test]
fn absent_file_is_not_found_and_lent_read_enforces_cap_and_leak_fails_loudly() {
    let h = heap();
    let mut sim = Sim::new(7, Faults::default());
    let missing = FileName::new("does-not-exist.txt").unwrap();
    let existing = FileName::new("hello.txt").unwrap();

    with_brand(|brand| {
        let mut handler = SimHandler::new(&mut sim, TaskId(42), brand, h.alloc());
        let ro = handler.root().narrow_read_only();

        // 1. ReadAll on missing file returns FsError::NotFound, never empty!
        let req = FsRequest::ReadAll(ro.read_all(ReadAllOpts {
            path: missing.clone(),
            max: MaxBytes(1024),
            buf: DmaBuf::with_capacity_in(ByteLen(64), h.alloc()),
        }));
        let ans = handler
            .perform(req, |p| match p {
                Perform::Answered(FsAnswer::ReadAll(res)) => res,
                _ => unreachable!(),
            })
            .unwrap();
        match ans {
            Err(be) => {
                assert_eq!(be.error, FsError::NotFound { path: missing.clone() });
                be.buf.release();
            }
            Ok(_) => panic!("missing file must return NotFound, not empty"),
        }

        // 2. Create a file, leave it open, verify capped Read rejects > LENT_READ_MAX,
        //    and verify normal exit reports LeakedHandles loudly!
        let open_req = FsRequest::OpenWrite(handler.root().open_write(OpenOpts {
            path: existing.clone(),
            create: true,
            truncate: true,
        }));
        let file = handler
            .perform(open_req, |p| match p {
                Perform::Answered(FsAnswer::OpenedWrite(Ok(f))) => f,
                _ => unreachable!(),
            })
            .unwrap();

        let too_big = FsRequest::Read(file.read(ReadOpts {
            at: ByteOffset(0),
            len: ByteLen(LENT_READ_MAX.0 + 1),
        }));
        let err = handler
            .perform(too_big, |p| match p {
                Perform::Answered(FsAnswer::Read(Err(e))) => e,
                _ => unreachable!(),
            })
            .unwrap();
        assert_eq!(
            err,
            FsError::LimitExceeded {
                requested: u64::from(LENT_READ_MAX.0) + 1,
                max: u64::from(LENT_READ_MAX.0),
            }
        );

        // Deliberately forget to close `file` (raw id leak test) -> normal exit MUST fail loudly!
        let _leaked_id = file.into_id();
        assert!(matches!(
            handler.finish_task(ExitKind::Normal),
            Err(SimHandlerError::LeakedHandles(l)) if l.task == TaskId(42) && l.count == 1
        ));
    });
}

// ---------------------------------------------------------------------------
// Wave 2: Network echo machine & Lock/FS/Host tests over `DriverHandler<Sim>`
// ---------------------------------------------------------------------------

/// Linear network echo machine over `IoRequest`:
/// `Listen -> Connect -> Accept -> SendFrom(client) -> RecvInto(server) -> SendFrom(server) -> RecvInto(client) -> CloseConn(client) -> CloseConn(server) -> Unlisten`
enum NetEchoMachine<'io, 'a> {
    Start {
        addr: Address,
        payload: DmaBuf<'a>,
        alloc: Alloc<'a>,
    },
    Listening {
        addr: Address,
        payload: DmaBuf<'a>,
        alloc: Alloc<'a>,
    },
    Connecting {
        listener: Listener<'io>,
        payload: DmaBuf<'a>,
        alloc: Alloc<'a>,
    },
    Accepting {
        listener: Listener<'io>,
        client: ConnHandle<'io>,
        payload: DmaBuf<'a>,
        alloc: Alloc<'a>,
    },
    ClientSending {
        listener: Listener<'io>,
        client: ConnHandle<'io>,
        server: ConnHandle<'io>,
        expected_len: usize,
        alloc: Alloc<'a>,
    },
    ServerRecving {
        listener: Listener<'io>,
        client: ConnHandle<'io>,
        server: ConnHandle<'io>,
        expected_len: usize,
    },
    ServerSending {
        listener: Listener<'io>,
        client: ConnHandle<'io>,
        server: ConnHandle<'io>,
        expected_len: usize,
    },
    ClientRecving {
        listener: Listener<'io>,
        client: ConnHandle<'io>,
        server: ConnHandle<'io>,
    },
    ClosingClient {
        listener: Listener<'io>,
        server: ConnHandle<'io>,
        received: DmaBuf<'a>,
    },
    ClosingServer {
        listener: Listener<'io>,
        received: DmaBuf<'a>,
    },
    Unlistening {
        received: DmaBuf<'a>,
    },
}

impl<'io, 'a> Machine for NetEchoMachine<'io, 'a> {
    type Req = IoRequest<'io, 'a>;
    type Out = DmaBuf<'a>;
    type Err = NetError;

    fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match self {
            NetEchoMachine::Start { addr, payload, alloc } => Turn::Ask {
                request: IoRequest::Listen(Listen::new(ListenOpts { addr: addr.clone() })),
                next: NetEchoMachine::Listening { addr, payload, alloc },
            },
            _ => unreachable!(),
        }
    }

    fn resume(
        self,
        answer: IoAnswer<'io, 'a, '_>,
        _cx: &mut (),
    ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match (self, answer) {
            (
                NetEchoMachine::Listening { addr, payload, alloc },
                IoAnswer::Listened(Ok(listener)),
            ) => Turn::Ask {
                request: IoRequest::Connect(Connect::new(ConnectOpts { addr })),
                next: NetEchoMachine::Connecting { listener, payload, alloc },
            },
            (NetEchoMachine::Listening { payload, .. }, IoAnswer::Listened(Err(e))) => {
                payload.release();
                Turn::Fail(e)
            }
            (
                NetEchoMachine::Connecting { listener, payload, alloc },
                IoAnswer::Connected(Ok(client)),
            ) => {
                let req = listener.accept();
                Turn::Ask {
                    request: IoRequest::Accept(req),
                    next: NetEchoMachine::Accepting { listener, client, payload, alloc },
                }
            }
            (
                NetEchoMachine::Accepting { listener, client, payload, alloc },
                IoAnswer::Accepted(Ok(server)),
            ) => {
                let expected_len = payload.len();
                let req = client.send_from(SendFromOpts { buf: payload });
                Turn::Ask {
                    request: IoRequest::SendFrom(req),
                    next: NetEchoMachine::ClientSending {
                        listener,
                        client,
                        server,
                        expected_len,
                        alloc,
                    },
                }
            }
            (
                NetEchoMachine::ClientSending { listener, client, server, expected_len, alloc },
                IoAnswer::Sent(Ok(buf)),
            ) => {
                buf.release();
                let mut rx = DmaBuf::with_capacity_in(ByteLen(expected_len as u32), alloc);
                rx.resize_zero(expected_len);
                let req = server.recv_into(RecvIntoOpts { buf: rx });
                Turn::Ask {
                    request: IoRequest::RecvInto(req),
                    next: NetEchoMachine::ServerRecving { listener, client, server, expected_len },
                }
            }
            (
                NetEchoMachine::ServerRecving { listener, client, server, expected_len },
                IoAnswer::Recved(Ok(filled)),
            ) => {
                assert_eq!(filled.filled.0 as usize, expected_len);
                let req = server.send_from(SendFromOpts { buf: filled.buf });
                Turn::Ask {
                    request: IoRequest::SendFrom(req),
                    next: NetEchoMachine::ServerSending { listener, client, server, expected_len },
                }
            }
            (
                NetEchoMachine::ServerSending { listener, client, server, expected_len },
                IoAnswer::Sent(Ok(mut buf)),
            ) => {
                buf.resize_zero(expected_len);
                let req = client.recv_into(RecvIntoOpts { buf });
                Turn::Ask {
                    request: IoRequest::RecvInto(req),
                    next: NetEchoMachine::ClientRecving { listener, client, server },
                }
            }
            (
                NetEchoMachine::ClientRecving { listener, client, server },
                IoAnswer::Recved(Ok(filled)),
            ) => Turn::Ask {
                request: IoRequest::CloseConn(client.close()),
                next: NetEchoMachine::ClosingClient { listener, server, received: filled.buf },
            },
            (
                NetEchoMachine::ClosingClient { listener, server, received },
                IoAnswer::ClosedConn(Ok(())),
            ) => Turn::Ask {
                request: IoRequest::CloseConn(server.close()),
                next: NetEchoMachine::ClosingServer { listener, received },
            },
            (
                NetEchoMachine::ClosingServer { listener, received },
                IoAnswer::ClosedConn(Ok(())),
            ) => Turn::Ask {
                request: IoRequest::Unlisten(listener.unlisten()),
                next: NetEchoMachine::Unlistening { received },
            },
            (NetEchoMachine::Unlistening { received }, IoAnswer::Unlistened(Ok(()))) => {
                Turn::Done(received)
            }
            (_, ans) => panic!("unexpected NetEchoMachine answer: {ans:?}"),
        }
    }
}

#[test]
fn net_echo_machine_over_sim_driver_handler() {
    let h = heap();
    blaze_sim::check(Config::cases(32), |tape| {
        let seed = tape.next_u64("sim seed");
        let len = 1 + (tape.below(256, "payload_len") as usize);
        let fill_byte = (tape.below(255, "fill_byte") as u8).wrapping_add(1);
        let mut expected = std::vec::Vec::with_capacity(len);
        for i in 0..len {
            expected.push(fill_byte.wrapping_add(i as u8));
        }

        let mut sim = Sim::new(seed, Faults::default());
        let addr = Address::Unix(FileName::new("echo.sock").unwrap());
        let mut verdict: Result<(), Broke> = Ok(());

        with_brand(|brand| {
            let mut handler: DriverHandler<'_, '_, Sim> =
                DriverHandler::new(&mut sim, TaskId(7), brand, h.alloc());
            let payload = DmaBuf::from_slice_in(&expected, h.alloc());
            let machine = NetEchoMachine::Start { addr: addr.clone(), payload, alloc: h.alloc() };
            match drive(&mut handler, &mut (), machine, DriveOpts { max_steps: 32 }) {
                Ok(echoed) => {
                    if echoed.as_slice() != expected.as_slice() {
                        verdict = Err(Broke::Oracle(format!(
                            "echo mismatch: expected {} bytes, got {} bytes",
                            expected.len(),
                            echoed.len()
                        )));
                    }
                    echoed.release();
                }
                Err(e) => {
                    verdict = Err(Broke::Oracle(format!("echo machine failed: {e}")));
                }
            }

            if verdict.is_ok() {
                if let Err(e) = handler.finish_task(ExitKind::Normal) {
                    verdict = Err(Broke::Oracle(format!("leaked handles after clean echo: {e}")));
                }
            }
        });
        verdict
    });

    // Also verify Cancelled exit reclaims an open Listener + Conn cleanly in the driver.
    let mut sim = Sim::new(99, Faults::default());
    let addr = Address::Unix(FileName::new("cancel.sock").unwrap());
    with_brand(|brand| {
        let mut handler: DriverHandler<'_, '_, Sim> =
            DriverHandler::new(&mut sim, TaskId(8), brand, h.alloc());
        let listener = handler
            .perform(IoRequest::Listen(Listen::new(ListenOpts { addr: addr.clone() })), |p| match p
            {
                Perform::Answered(IoAnswer::Listened(Ok(l))) => l,
                _ => unreachable!(),
            })
            .unwrap();
        let client = handler
            .perform(IoRequest::Connect(Connect::new(ConnectOpts { addr: addr.clone() })), |p| {
                match p {
                    Perform::Answered(IoAnswer::Connected(Ok(c))) => c,
                    _ => unreachable!(),
                }
            })
            .unwrap();
        let _leaked_listener = listener.into_id();
        let _leaked_conn = client.into_id();
        let reclaimed = handler.finish_task(ExitKind::Cancelled).unwrap();
        assert_eq!(reclaimed, 2);
    });
}

#[test]
fn lock_contention_and_unlock_and_fs_ops_over_sim() {
    let h = heap();
    let mut sim = Sim::new(12345, Faults::default());
    let sub_dir = FileName::new("work").unwrap();
    let data_file = FileName::new("work/data.bin").unwrap();
    let copy_file = FileName::new("work/copy.bin").unwrap();
    let lock_file = FileName::new("work/state.lock").unwrap();

    // Another process holds the lock initially.
    sim.hold_lock(&lock_file, LockMode::Exclusive);

    with_brand(|brand| {
        let mut handler: DriverHandler<'_, '_, Sim> =
            DriverHandler::new(&mut sim, TaskId(11), brand, h.alloc());
        let root = handler.root();

        // 1. MakeDir
        let made = handler
            .perform(IoRequest::MakeDir(root.make_dir(sub_dir.clone())), |p| match p {
                Perform::Answered(IoAnswer::MadeDir(r)) => r,
                _ => unreachable!(),
            })
            .unwrap();
        assert_eq!(made, Ok(()));

        // 2. WriteDurable
        let payload = b"hello-blaze-wave2-payload";
        let written = handler
            .perform(
                IoRequest::WriteDurable(root.write_durable(WriteDurableOpts {
                    path: data_file.clone(),
                    buf: DmaBuf::from_slice_in(payload, h.alloc()),
                })),
                |p| match p {
                    Perform::Answered(IoAnswer::WrittenDurable(r)) => r,
                    _ => unreachable!(),
                },
            )
            .unwrap()
            .unwrap();
        written.release();

        // 3. Stat on existing & missing file
        let meta = handler
            .perform(IoRequest::Stat(root.stat(data_file.clone())), |p| match p {
                Perform::Answered(IoAnswer::Stat(r)) => r,
                _ => unreachable!(),
            })
            .unwrap()
            .unwrap()
            .expect("data_file must exist");
        assert_eq!(meta.len, payload.len() as u64);
        assert!(!meta.dir);

        // 4. CopyData + Truncate
        let copied = handler
            .perform(
                IoRequest::CopyData(root.copy_data(CopyDataOpts {
                    from: data_file.clone(),
                    to: copy_file.clone(),
                })),
                |p| match p {
                    Perform::Answered(IoAnswer::CopiedData(r)) => r,
                    _ => unreachable!(),
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(copied, ByteOffset(payload.len() as u64));

        let opened_copy = handler
            .perform(
                IoRequest::OpenWrite(root.open_write(OpenOpts {
                    path: copy_file.clone(),
                    create: false,
                    truncate: false,
                })),
                |p| match p {
                    Perform::Answered(IoAnswer::OpenedWrite(r)) => r,
                    _ => unreachable!(),
                },
            )
            .unwrap()
            .unwrap();
        let trunc_res = handler
            .perform(
                IoRequest::Truncate(opened_copy.truncate(TruncateOpts { len: ByteOffset(5) })),
                |p| match p {
                    Perform::Answered(IoAnswer::Truncated(r)) => r,
                    _ => unreachable!(),
                },
            )
            .unwrap();
        assert_eq!(trunc_res, Ok(()));
        let closed = handler
            .perform(IoRequest::Close(opened_copy.close()), |p| match p {
                Perform::Answered(IoAnswer::Closed(r)) => r,
                _ => unreachable!(),
            })
            .unwrap();
        assert_eq!(closed, Ok(()));

        let copy_meta = handler
            .perform(IoRequest::Stat(root.stat(copy_file.clone())), |p| match p {
                Perform::Answered(IoAnswer::Stat(r)) => r,
                _ => unreachable!(),
            })
            .unwrap()
            .unwrap()
            .expect("copy_file must exist");
        assert_eq!(copy_meta.len, 5);

        // 5. List via Effect::Entries
        let listing = handler
            .perform(
                IoRequest::List(root.list(ListOpts { dir: DirName::Named(sub_dir.clone()) })),
                |p| match p {
                    Perform::Answered(IoAnswer::Listed(r)) => r,
                    _ => unreachable!(),
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(listing.len(), 2);
        assert!(listing.find("copy.bin").is_some());
        assert!(listing.find("data.bin").is_some());

        // 6. Lock contended while `sim.hold_lock` holds it!
        let lock_res = handler
            .perform(
                IoRequest::Lock(root.lock(LockOpts {
                    path: lock_file.clone(),
                    mode: LockMode::Exclusive,
                })),
                |p| match p {
                    Perform::Answered(IoAnswer::Locked(r)) => r,
                    _ => unreachable!(),
                },
            )
            .unwrap()
            .unwrap();
        assert!(matches!(lock_res, LockOutcome::Contended));

        // Release outside lock -> now Lock succeeds!
        handler.sim_mut().release_lock(&lock_file);
        let acquired = handler
            .perform(
                IoRequest::Lock(root.lock(LockOpts {
                    path: lock_file.clone(),
                    mode: LockMode::Exclusive,
                })),
                |p| match p {
                    Perform::Answered(IoAnswer::Locked(r)) => r,
                    _ => unreachable!(),
                },
            )
            .unwrap()
            .unwrap();
        let lock_handle = match acquired {
            LockOutcome::Acquired(h) => h,
            LockOutcome::Contended => panic!("expected lock to be acquired after release"),
        };

        // Second exclusive lock on same file while we hold it must also contend.
        let second = handler
            .perform(
                IoRequest::Lock(root.lock(LockOpts {
                    path: lock_file.clone(),
                    mode: LockMode::Exclusive,
                })),
                |p| match p {
                    Perform::Answered(IoAnswer::Locked(r)) => r,
                    _ => unreachable!(),
                },
            )
            .unwrap()
            .unwrap();
        assert!(matches!(second, LockOutcome::Contended));

        // Unlock cleanly.
        let unlocked = handler
            .perform(IoRequest::Unlock(lock_handle.unlock()), |p| match p {
                Perform::Answered(IoAnswer::Unlocked(r)) => r,
                _ => unreachable!(),
            })
            .unwrap();
        assert_eq!(unlocked, Ok(()));

        // 7. Remove files and RemoveDir
        for path in [&data_file, &copy_file, &lock_file] {
            let rm = handler
                .perform(IoRequest::Remove(root.remove(path.clone())), |p| match p {
                    Perform::Answered(IoAnswer::Removed(r)) => r,
                    _ => unreachable!(),
                })
                .unwrap();
            assert_eq!(rm, Ok(()));
        }
        let rmdir = handler
            .perform(IoRequest::RemoveDir(root.remove_dir(sub_dir.clone())), |p| match p {
                Perform::Answered(IoAnswer::RemovedDir(r)) => r,
                _ => unreachable!(),
            })
            .unwrap();
        assert_eq!(rmdir, Ok(()));
        let gone = handler
            .perform(IoRequest::Stat(root.stat(data_file.clone())), |p| match p {
                Perform::Answered(IoAnswer::Stat(r)) => r,
                _ => unreachable!(),
            })
            .unwrap()
            .unwrap();
        assert!(gone.is_none());

        // 8. Sleep advances virtual clock
        let before = handler.driver().now();
        handler
            .perform(
                IoRequest::Sleep(Sleep::new(SleepOpts { after: Duration::from_millis(25) })),
                |p| match p {
                    Perform::Answered(IoAnswer::Slept) => (),
                    _ => unreachable!(),
                },
            )
            .unwrap();
        let after = handler.driver().now();
        assert!(after.saturating_sub(before) >= Duration::from_millis(25));

        // 9. FillRandom fills caller DMA buffer and enforces RANDOM_MOST cap
        let mut rbuf = DmaBuf::with_capacity_in(ByteLen(32), h.alloc());
        rbuf.resize_zero(32);
        let filled_rng = handler
            .perform(IoRequest::FillRandom(FillRandom::new(FillRandomOpts { buf: rbuf })), |p| {
                match p {
                    Perform::Answered(IoAnswer::FilledRandom(r)) => r,
                    _ => unreachable!(),
                }
            })
            .unwrap()
            .unwrap();
        assert_eq!(filled_rng.len(), 32);
        assert!(filled_rng.as_slice().iter().any(|&b| b != 0));
        filled_rng.release();

        let over_cap = DmaBuf::with_capacity_in(ByteLen((RANDOM_MOST + 1) as u32), h.alloc());
        let cap_err = handler
            .perform(
                IoRequest::FillRandom(FillRandom::new(FillRandomOpts { buf: over_cap })),
                |p| match p {
                    Perform::Answered(IoAnswer::FilledRandom(Err(be))) => be,
                    _ => unreachable!(),
                },
            )
            .unwrap();
        assert_eq!(
            cap_err.error,
            FsError::LimitExceeded {
                requested: (RANDOM_MOST + 1) as u64,
                max: RANDOM_MOST as u64,
            }
        );
        cap_err.buf.release();

        // Clean normal exit with zero leaked handles.
        assert_eq!(handler.finish_task(ExitKind::Normal), Ok(0));
    });

    // 10. Loud LeakedHandles if a Lock<'io> AND a Conn<'io> are leaked on normal exit!
    let leak_addr = Address::Unix(FileName::new("leak.sock").unwrap());
    let leak_lock_path = FileName::new("leaked.lock").unwrap();
    with_brand(|brand| {
        let mut handler: DriverHandler<'_, '_, Sim> =
            DriverHandler::new(&mut sim, TaskId(77), brand, h.alloc());
        let root = handler.root();

        let lock_handle = match handler
            .perform(
                IoRequest::Lock(root.lock(LockOpts {
                    path: leak_lock_path.clone(),
                    mode: LockMode::Exclusive,
                })),
                |p| match p {
                    Perform::Answered(IoAnswer::Locked(Ok(o))) => o,
                    _ => unreachable!(),
                },
            )
            .unwrap()
        {
            LockOutcome::Acquired(h) => h,
            LockOutcome::Contended => panic!("lock should be free"),
        };

        let listener = handler
            .perform(
                IoRequest::Listen(Listen::new(ListenOpts { addr: leak_addr.clone() })),
                |p| match p {
                    Perform::Answered(IoAnswer::Listened(Ok(l))) => l,
                    _ => unreachable!(),
                },
            )
            .unwrap();
        let conn = handler
            .perform(
                IoRequest::Connect(Connect::new(ConnectOpts { addr: leak_addr.clone() })),
                |p| match p {
                    Perform::Answered(IoAnswer::Connected(Ok(c))) => c,
                    _ => unreachable!(),
                },
            )
            .unwrap();

        // Unlisten properly, but deliberately leak BOTH `lock_handle` and `conn`!
        let _ = handler
            .perform(IoRequest::Unlisten(listener.unlisten()), |p| match p {
                Perform::Answered(IoAnswer::Unlistened(Ok(()))) => (),
                _ => unreachable!(),
            })
            .unwrap();

        let _leaked_lock = lock_handle.into_id();
        let _leaked_conn = conn.into_id();

        assert!(matches!(
            handler.finish_task(ExitKind::Normal),
            Err(DriverHandlerError::LeakedHandles(l)) if l.task == TaskId(77) && l.count == 2
        ));
    });
}

#[test]
fn fixed_buf_index_and_fixed_file_slot_survive_io_round_trip() {
    let h = heap();
    let mut sim = Sim::new(42, Faults::default());
    let path = FileName::new("fixed-ring.bin").unwrap();
    let payload = b"zero-copy-io-uring-fixed-buffer";

    with_brand(|brand| {
        let mut handler = SimHandler::new(&mut sim, TaskId(99), brand, h.alloc());
        let root = handler.root();

        let file = handler
            .perform(
                root.open_write(OpenOpts { path: path.clone(), create: true, truncate: true }),
                |p| match p {
                    Perform::Answered(res) => res,
                    _ => unreachable!(),
                },
            )
            .unwrap()
            .unwrap();

        assert_eq!(file.fixed_file_slot(), FixedFileSlot(file.id().slot()));

        let unregistered = DmaBuf::from_slice_in(payload, h.alloc());
        assert_eq!(unregistered.registered_index(), None);
        assert!(!unregistered.is_registered());

        let buf = unregistered.with_registered_index(FixedBufIndex(7));
        assert_eq!(buf.registered_index(), Some(FixedBufIndex(7)));
        assert!(buf.is_registered());

        let mut buf = handler
            .perform(file.write_from(WriteFromOpts { at: ByteOffset(0), buf }), |p| match p {
                Perform::Answered(res) => res,
                _ => unreachable!(),
            })
            .unwrap()
            .unwrap();
        assert_eq!(buf.registered_index(), Some(FixedBufIndex(7)));
        assert!(buf.is_registered());

        let (flushed, _proof) = handler
            .perform(file.flush(), |p| match p {
                Perform::Answered(res) => res,
                _ => unreachable!(),
            })
            .unwrap()
            .unwrap();
        assert_eq!(flushed.fixed_file_slot(), FixedFileSlot(flushed.id().slot()));

        buf.resize_zero(payload.len());
        assert_eq!(buf.registered_index(), Some(FixedBufIndex(7)));

        let filled = handler
            .perform(flushed.read_into(ReadIntoOpts { at: ByteOffset(0), buf }), |p| match p {
                Perform::Answered(res) => res,
                _ => unreachable!(),
            })
            .unwrap()
            .unwrap();
        assert_eq!(filled.filled, ByteLen(payload.len() as u32));
        assert_eq!(filled.buf.as_slice(), payload);
        assert_eq!(filled.buf.registered_index(), Some(FixedBufIndex(7)));
        assert!(filled.buf.is_registered());
        filled.buf.release();

        let closed = handler
            .perform(flushed.close(), |p| match p {
                Perform::Answered(res) => res,
                _ => unreachable!(),
            })
            .unwrap();
        assert_eq!(closed, Ok(()));
        assert_eq!(handler.finish_task(ExitKind::Normal), Ok(0));
    });
}
