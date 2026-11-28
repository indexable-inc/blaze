//! Explicit `#![blaze::sans_io]` machine for fetching a single stored file into a caller-owned
//! [`DmaBuf`] over typed linear direct filesystem requests (`OpenRead` -> `ReadInto` -> `Close`).
//!
//! Guarantees:
//! - Zero global heap allocation: reads directly into the caller-supplied `DmaBuf<'a>` and
//!   returns it in BOTH `Ok(FilledBuf<'a>)` and `Err(StoredFileFetchError<'a>)`.
//! - Explicit `NotFound` vs empty file distinction: an absent file returns
//!   [`StoredFileFetchError::NotFound`], while an existing 0-byte file succeeds with
//!   `FilledBuf { filled: ByteLen(0), .. }`.
//! - Mandatory linear handle closure: [`Close`] is issued on BOTH the success path AND the
//!   read-error / cancellation paths so no file handle ever leaks.

#![blaze::sans_io]
#![forbid(unsafe_code)]

use core::fmt;

use blaze_core::FileName;
use blaze_core::handle::{Dir, File, ReadOnly};
use blaze_core::request::{
    BufError, ByteOffset, Close, DirectFsBackend, DirectFsHandler, DirectFsHandlerError, DmaBuf,
    FilledBuf, FsError, OpenRead, ReadInto, ReadIntoOpts,
};
use blaze_machine::{
    CancelAction, CancelReason, Handler, Machine, Perform, Request, Stopped, Turn,
};

/// Typed linear request union emitted by [`StoredFileFetchMachine`].
#[blaze::linear]
#[derive(Debug)]
pub enum StoredFileFetchReq<'io, 'a> {
    OpenRead(OpenRead<'io>),
    ReadInto(ReadInto<'io, 'a>),
    Close(Close<'io>),
}

/// Typed answer union paired with [`StoredFileFetchReq`].
#[derive(Debug)]
pub enum StoredFileFetchAns<'io, 'a> {
    Opened(Result<File<'io, ReadOnly>, FsError>),
    Read(Result<FilledBuf<'a>, BufError<'a, FsError>>),
    Closed(Result<(), FsError>),
}

impl<'io, 'a> Request for StoredFileFetchReq<'io, 'a> {
    type Answer<'s> = StoredFileFetchAns<'io, 'a>;
}

/// Typed failure of [`StoredFileFetchMachine`], returning the caller's [`DmaBuf<'a>`] whenever
/// the machine still holds it so no caller allocation is ever lost on error.
#[derive(Debug)]
pub enum StoredFileFetchError<'a> {
    /// Target stored file does not exist (distinct from an existing 0-byte file).
    NotFound { path: FileName, buf: DmaBuf<'a> },
    /// Opening or reading the file failed (any opened handle was closed before failing).
    Io { error: FsError, buf: DmaBuf<'a> },
    /// Closing the file handle failed after reading completed.
    CloseFailed { error: FsError, buf: DmaBuf<'a> },
    /// Machine was cancelled while a read was in flight and closed the handle cleanly.
    Cancelled { reason: CancelReason },
}

impl fmt::Display for StoredFileFetchError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoredFileFetchError::NotFound { path, .. } => {
                write!(f, "stored file not found: {path}")
            }
            StoredFileFetchError::Io { error, .. } => write!(f, "stored file IO error: {error}"),
            StoredFileFetchError::CloseFailed { error, .. } => {
                write!(f, "stored file close failed: {error}")
            }
            StoredFileFetchError::Cancelled { reason } => {
                write!(f, "stored file fetch cancelled ({reason:?})")
            }
        }
    }
}

impl core::error::Error for StoredFileFetchError<'_> {}

#[derive(Debug)]
enum State<'io, 'a> {
    Init { dir: Dir<'io, ReadOnly>, path: FileName, buf: DmaBuf<'a> },
    Opening { buf: DmaBuf<'a> },
    Reading { file: File<'io, ReadOnly> },
    ClosingOnSuccess { filled: FilledBuf<'a> },
    ClosingOnError { buf: DmaBuf<'a>, error: FsError },
    ClosingOnCancel { reason: CancelReason },
}

/// Explicit `#![blaze::sans_io]` machine fetching one stored file via
/// `OpenRead -> ReadInto -> Close` into a caller-owned [`DmaBuf<'a>`].
#[derive(Debug)]
pub struct StoredFileFetchMachine<'io, 'a> {
    state: State<'io, 'a>,
}

impl<'io, 'a> StoredFileFetchMachine<'io, 'a> {
    /// Creates a [`StoredFileFetchMachine`] reading `path` beneath read-only directory `dir` into
    /// caller-owned `buf`.
    #[inline]
    pub fn new(dir: Dir<'io, ReadOnly>, path: FileName, buf: DmaBuf<'a>) -> Self {
        Self { state: State::Init { dir, path, buf } }
    }
}

impl<'io, 'a> Machine<()> for StoredFileFetchMachine<'io, 'a> {
    type Req = StoredFileFetchReq<'io, 'a>;
    type Out = FilledBuf<'a>;
    type Err = StoredFileFetchError<'a>;

    fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match self.state {
            State::Init { dir, path, buf } => Turn::Ask {
                request: StoredFileFetchReq::OpenRead(dir.open_read(path)),
                next: Self { state: State::Opening { buf } },
            },
            State::Opening { .. }
            | State::Reading { .. }
            | State::ClosingOnSuccess { .. }
            | State::ClosingOnError { .. }
            | State::ClosingOnCancel { .. } => {
                unreachable!("StoredFileFetchMachine::start called on non-Init state")
            }
        }
    }

    fn resume(
        self,
        answer: StoredFileFetchAns<'io, 'a>,
        _cx: &mut (),
    ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match (self.state, answer) {
            (State::Opening { buf }, StoredFileFetchAns::Opened(Ok(file))) => {
                let req = file.read_into(ReadIntoOpts { at: ByteOffset(0), buf });
                Turn::Ask {
                    request: StoredFileFetchReq::ReadInto(req),
                    next: Self { state: State::Reading { file } },
                }
            }
            (
                State::Opening { buf },
                StoredFileFetchAns::Opened(Err(FsError::NotFound { path })),
            ) => Turn::Fail(StoredFileFetchError::NotFound { path, buf }),
            (State::Opening { buf }, StoredFileFetchAns::Opened(Err(error))) => {
                Turn::Fail(StoredFileFetchError::Io { error, buf })
            }
            (State::Reading { file }, StoredFileFetchAns::Read(Ok(filled))) => Turn::Ask {
                request: StoredFileFetchReq::Close(file.close()),
                next: Self { state: State::ClosingOnSuccess { filled } },
            },
            (State::Reading { file }, StoredFileFetchAns::Read(Err(be))) => Turn::Ask {
                request: StoredFileFetchReq::Close(file.close()),
                next: Self { state: State::ClosingOnError { buf: be.buf, error: be.error } },
            },
            (State::ClosingOnSuccess { filled }, StoredFileFetchAns::Closed(Ok(()))) => {
                Turn::Done(filled)
            }
            (State::ClosingOnSuccess { filled }, StoredFileFetchAns::Closed(Err(error))) => {
                Turn::Fail(StoredFileFetchError::CloseFailed { error, buf: filled.buf })
            }
            (State::ClosingOnError { buf, error }, StoredFileFetchAns::Closed(_)) => {
                Turn::Fail(StoredFileFetchError::Io { error, buf })
            }
            (State::ClosingOnCancel { reason }, StoredFileFetchAns::Closed(_)) => {
                Turn::Fail(StoredFileFetchError::Cancelled { reason })
            }
            _ => unreachable!("StoredFileFetchAns always matches active machine state"),
        }
    }

    fn on_cancel(
        self,
        stopped: Stopped,
        _cx: &mut (),
    ) -> CancelAction<Turn<Self, Self::Req, Self::Out, Self::Err>> {
        match self.state {
            State::Init { buf, .. }
            | State::Opening { buf }
            | State::ClosingOnError { buf, .. } => {
                buf.release();
                CancelAction::Abandon
            }
            State::ClosingOnSuccess { filled } => {
                filled.buf.release();
                CancelAction::Abandon
            }
            State::Reading { file } => CancelAction::Cleanup(Turn::Ask {
                request: StoredFileFetchReq::Close(file.close()),
                next: Self { state: State::ClosingOnCancel { reason: stopped.reason } },
            }),
            State::ClosingOnCancel { .. } => CancelAction::Abandon,
        }
    }
}

impl<'io, 'a, 'b, B: DirectFsBackend> Handler<StoredFileFetchReq<'io, 'a>>
    for DirectFsHandler<'io, 'b, B>
{
    type Error = DirectFsHandlerError;

    fn perform<Out>(
        &mut self,
        request: StoredFileFetchReq<'io, 'a>,
        deliver: impl FnOnce(Perform<StoredFileFetchAns<'io, 'a>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let perf = match request {
            StoredFileFetchReq::OpenRead(req) => {
                match Handler::perform(self, req, core::convert::identity)? {
                    Perform::Answered(res) => Perform::Answered(StoredFileFetchAns::Opened(res)),
                    Perform::Stopped(stopped) => Perform::Stopped(stopped),
                }
            }
            StoredFileFetchReq::ReadInto(req) => {
                match Handler::perform(self, req, core::convert::identity)? {
                    Perform::Answered(res) => Perform::Answered(StoredFileFetchAns::Read(res)),
                    Perform::Stopped(stopped) => Perform::Stopped(stopped),
                }
            }
            StoredFileFetchReq::Close(req) => {
                match Handler::perform(self, req, core::convert::identity)? {
                    Perform::Answered(res) => Perform::Answered(StoredFileFetchAns::Closed(res)),
                    Perform::Stopped(stopped) => Perform::Stopped(stopped),
                }
            }
        };
        Ok(deliver(perf))
    }
}

#[cfg(test)]
mod tests {
    use blaze_core::FileName;
    use blaze_core::handle::TaskId;
    use blaze_core::request::{
        ByteLen, DirectFsBackend, DirectFsHandler, DirectFsHandlerError, DmaBuf, FsError,
        InMemoryDirectFs,
    };
    use blaze_core::sim::{Faults, Sim};
    use blaze_machine::{
        CancelReason, DriveError, DriveOpts, GraceBudget, Handler, Perform, Stopped, drive,
    };
    use blaze_own::{Alloc, testing::heap, with_brand};

    use super::{
        StoredFileFetchAns, StoredFileFetchError, StoredFileFetchMachine, StoredFileFetchReq,
    };

    fn run_readback_over_backend<B: DirectFsBackend>(
        backend: &mut B,
        present: &FileName,
        expected: &[u8],
        empty: &FileName,
        missing: &FileName,
        alloc: Alloc<'static>,
    ) {
        with_brand(|brand| {
            let mut handler = DirectFsHandler::new(backend, TaskId(701), brand, alloc);
            let ro = handler.root().narrow_read_only();

            // 1. Existing non-empty file reads back exact bytes into caller-owned DmaBuf.
            let buf = DmaBuf::with_capacity_in(ByteLen((expected.len() + 32) as u32), alloc);
            let filled = drive(
                &mut handler,
                &mut (),
                StoredFileFetchMachine::new(ro, present.clone(), buf),
                DriveOpts { max_steps: 8 },
            )
            .expect("present file fetch succeeds");
            assert_eq!(filled.filled, ByteLen(expected.len() as u32));
            assert_eq!(filled.buf.as_slice(), expected);
            filled.buf.release();

            // 2. Existing 0-byte file succeeds with ByteLen(0) (distinct from NotFound).
            let empty_buf = DmaBuf::with_capacity_in(ByteLen(64), alloc);
            let empty_filled = drive(
                &mut handler,
                &mut (),
                StoredFileFetchMachine::new(ro, empty.clone(), empty_buf),
                DriveOpts { max_steps: 8 },
            )
            .expect("empty file fetch succeeds");
            assert_eq!(empty_filled.filled, ByteLen(0));
            assert!(empty_filled.buf.is_empty());
            empty_filled.buf.release();

            // 3. Missing file fails explicitly with NotFound and returns the caller's DmaBuf.
            let missing_buf = DmaBuf::with_capacity_in(ByteLen(64), alloc);
            let missing_res = drive(
                &mut handler,
                &mut (),
                StoredFileFetchMachine::new(ro, missing.clone(), missing_buf),
                DriveOpts { max_steps: 8 },
            );
            match missing_res {
                Err(DriveError::Machine(StoredFileFetchError::NotFound { path, buf })) => {
                    assert_eq!(path, *missing);
                    buf.release();
                }
                other => panic!("expected NotFound for missing stored file, got {other:?}"),
            }

            assert_eq!(handler.finish_checked(), Ok(()));
        });
    }

    #[test]
    fn stored_file_fetch_readback_empty_and_not_found_over_direct_sim() {
        let h = heap();
        let mut sim = Sim::new(42, Faults::default());
        let present = FileName::new("files/deadbeef").expect("valid path");
        let empty = FileName::new("files/empty").expect("valid path");
        let missing = FileName::new("files/missing").expect("valid path");
        let payload = b"pub const GENERATED_CONST: u32 = 42;\n";

        sim.change(&present, payload);
        sim.change(&empty, b"");

        run_readback_over_backend(&mut sim, &present, payload, &empty, &missing, h.alloc());
    }

    #[test]
    fn stored_file_fetch_readback_and_not_found_over_in_memory_direct_fs() {
        let h = heap();
        let alloc = h.alloc();
        let mut fs = InMemoryDirectFs::new(alloc);
        let present = FileName::new("files/cafebabe").expect("valid path");
        let empty = FileName::new("files/zero").expect("valid path");
        let missing = FileName::new("files/absent").expect("valid path");
        let payload = b"stored-in-memory-bytes";

        fs.open_write(&present, true, true).expect("create present");
        fs.write_at(&present, 0, payload).expect("write present");
        fs.open_write(&empty, true, true).expect("create empty");

        run_readback_over_backend(&mut fs, &present, payload, &empty, &missing, alloc);
    }

    #[test]
    fn stored_file_fetch_read_fault_closes_handle_cleanly_over_direct_sim() {
        let h = heap();
        let alloc = h.alloc();
        let mut sim = Sim::new(99, Faults::default());
        let dir_name = FileName::new("files").expect("valid dir");
        let faulty = FileName::new("files/faulty").expect("valid path");

        sim.change(&faulty, b"unreadable-sector-data");
        // `OpenRead` succeeds, then `ReadInto` fails with `FsError::Io`: the machine MUST emit
        // `Close` before failing so `finish_checked()` observes zero leaked handles.
        sim.fail_reads(dir_name, 1);

        with_brand(|brand| {
            let mut handler = DirectFsHandler::new(&mut sim, TaskId(702), brand, alloc);
            let ro = handler.root().narrow_read_only();
            let buf = DmaBuf::with_capacity_in(ByteLen(128), alloc);

            let res = drive(
                &mut handler,
                &mut (),
                StoredFileFetchMachine::new(ro, faulty.clone(), buf),
                DriveOpts { max_steps: 8 },
            );
            match res {
                Err(DriveError::Machine(StoredFileFetchError::Io {
                    error: FsError::Io { .. },
                    buf,
                })) => {
                    buf.release();
                }
                other => panic!("expected StoredFileFetchError::Io on read fault, got {other:?}"),
            }

            assert_eq!(handler.finish_checked(), Ok(()));
        });
    }

    struct CancelOnReadHandler<'h, 'io, 'b, B: DirectFsBackend> {
        inner: &'h mut DirectFsHandler<'io, 'b, B>,
    }

    impl<'io, 'a, 'b, B: DirectFsBackend> Handler<StoredFileFetchReq<'io, 'a>>
        for CancelOnReadHandler<'_, 'io, 'b, B>
    {
        type Error = DirectFsHandlerError;

        fn perform<Out>(
            &mut self,
            request: StoredFileFetchReq<'io, 'a>,
            deliver: impl FnOnce(Perform<StoredFileFetchAns<'io, 'a>>) -> Out,
        ) -> Result<Out, Self::Error> {
            let perf = match request {
                StoredFileFetchReq::OpenRead(req) => {
                    match Handler::perform(self.inner, req, core::convert::identity)? {
                        Perform::Answered(res) => Perform::Answered(StoredFileFetchAns::Opened(res)),
                        Perform::Stopped(s) => Perform::Stopped(s),
                    }
                }
                StoredFileFetchReq::ReadInto(req) => {
                    if let Perform::Answered(res) =
                        Handler::perform(self.inner, req, core::convert::identity)?
                    {
                        match res {
                            Ok(filled) => filled.buf.release(),
                            Err(be) => be.buf.release(),
                        }
                    }
                    Perform::Stopped(Stopped::cancelled(GraceBudget::DEFAULT))
                }
                StoredFileFetchReq::Close(req) => {
                    match Handler::perform(self.inner, req, core::convert::identity)? {
                        Perform::Answered(res) => Perform::Answered(StoredFileFetchAns::Closed(res)),
                        Perform::Stopped(s) => Perform::Stopped(s),
                    }
                }
            };
            Ok(deliver(perf))
        }
    }

    #[test]
    fn stored_file_fetch_cancel_during_read_closes_handle_cleanly() {
        let h = heap();
        let alloc = h.alloc();
        let mut sim = Sim::new(107, Faults::default());
        let file = FileName::new("files/cancelled").expect("valid path");
        sim.change(&file, b"cancelled-mid-read");

        with_brand(|brand| {
            let mut direct = DirectFsHandler::new(&mut sim, TaskId(703), brand, alloc);
            let ro = direct.root().narrow_read_only();
            let buf = DmaBuf::with_capacity_in(ByteLen(64), alloc);
            let mut cancel_handler = CancelOnReadHandler { inner: &mut direct };

            let res = drive(
                &mut cancel_handler,
                &mut (),
                StoredFileFetchMachine::new(ro, file.clone(), buf),
                DriveOpts { max_steps: 8 },
            );
            assert!(matches!(
                res,
                Err(DriveError::Machine(StoredFileFetchError::Cancelled {
                    reason: CancelReason::Cancelled,
                }))
            ));
            assert_eq!(direct.finish_checked(), Ok(()));
        });
    }
}
