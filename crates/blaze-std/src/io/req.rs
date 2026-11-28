//! Zero-alloc typed linear standard I/O requests (`ReadStdinReq`, `WriteStdoutReq`,
//! `WriteStderrReq`) and deterministic [`SimStdioHandler`].

#![blaze::sans_io]

use core::fmt;

use blaze_machine::{Handler, Perform, Request};
use blaze_own::{Alloc, Vec};

/// Typed refusal returned by standard I/O request handlers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StdioError {
    /// Target stream was closed (broken pipe or closed input).
    Closed,
    /// Write exceeded the handler's bounded output buffer cap (rule 18).
    BufferFull { max_bytes: usize },
    /// Explicit handler allocator refused growth.
    OutOfMemory,
}

impl fmt::Display for StdioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            StdioError::Closed => f.write_str("stdio stream closed"),
            StdioError::BufferFull { max_bytes } => {
                write!(f, "stdio output exceeded bounded cap of {max_bytes} bytes")
            }
            StdioError::OutOfMemory => f.write_str("stdio handler allocator out of memory"),
        }
    }
}

impl core::error::Error for StdioError {}

/// Typed linear request to read up to `max_bytes` from standard input.
///
/// The answer borrows handler-owned scratch bytes (`&'s [u8]`) with zero copy; an empty slice
/// signals EOF.
#[blaze::linear]
#[derive(PartialEq, Eq, Debug)]
pub struct ReadStdinReq {
    pub max_bytes: usize,
}

impl Request for ReadStdinReq {
    type Answer<'s> = Result<&'s [u8], StdioError>;
}

/// Typed linear request to write `bytes` to standard output.
#[blaze::linear]
#[derive(PartialEq, Eq, Debug)]
pub struct WriteStdoutReq<'a> {
    pub bytes: &'a [u8],
}

impl Request for WriteStdoutReq<'_> {
    type Answer<'s> = Result<usize, StdioError>;
}

/// Typed linear request to write `bytes` to standard error.
#[blaze::linear]
#[derive(PartialEq, Eq, Debug)]
pub struct WriteStderrReq<'a> {
    pub bytes: &'a [u8],
}

impl Request for WriteStderrReq<'_> {
    type Answer<'s> = Result<usize, StdioError>;
}

/// Unified standard I/O request union for driving sans-IO [`blaze_machine::Machine`]s.
#[blaze::linear]
#[derive(PartialEq, Eq, Debug)]
pub enum StdioReq<'a> {
    ReadStdin(ReadStdinReq),
    WriteStdout(WriteStdoutReq<'a>),
    WriteStderr(WriteStderrReq<'a>),
}

/// Unified answer matching [`StdioReq`].
#[derive(PartialEq, Eq, Debug)]
pub enum StdioAns<'s> {
    Stdin(Result<&'s [u8], StdioError>),
    Stdout(Result<usize, StdioError>),
    Stderr(Result<usize, StdioError>),
}

impl Request for StdioReq<'_> {
    type Answer<'s> = StdioAns<'s>;
}

/// Options for [`SimStdioHandler::new`] (`#[blaze::args]`, rule 83).
#[blaze::args]
pub struct SimStdioOpts<'a> {
    pub stdin: &'a [u8],
    pub max_output_bytes: usize,
    pub alloc: Alloc<'a>,
}

/// Deterministic `#![no_std]` handler for [`ReadStdinReq`], [`WriteStdoutReq`],
/// [`WriteStderrReq`], and [`StdioReq`] backed by an explicit caller [`Alloc`].
pub struct SimStdioHandler<'a> {
    stdin: &'a [u8],
    stdin_pos: usize,
    max_output_bytes: usize,
    stdout: Vec<u8, Alloc<'a>>,
    stderr: Vec<u8, Alloc<'a>>,
    stdin_closed: bool,
    stdout_closed: bool,
    stderr_closed: bool,
}

impl<'a> SimStdioHandler<'a> {
    /// Creates a deterministic stdio handler serving `opts.stdin` and capturing stdout/stderr in
    /// `opts.alloc` up to `opts.max_output_bytes` each.
    pub fn new(opts: SimStdioOpts<'a>) -> Self {
        Self {
            stdin: opts.stdin,
            stdin_pos: 0,
            max_output_bytes: opts.max_output_bytes,
            stdout: Vec::new_in(opts.alloc),
            stderr: Vec::new_in(opts.alloc),
            stdin_closed: false,
            stdout_closed: false,
            stderr_closed: false,
        }
    }

    #[inline]
    pub fn stdout(&self) -> &[u8] {
        self.stdout.as_slice()
    }

    #[inline]
    pub fn stderr(&self) -> &[u8] {
        self.stderr.as_slice()
    }

    #[inline]
    pub fn stdin_remaining(&self) -> usize {
        self.stdin.len().saturating_sub(self.stdin_pos)
    }

    #[inline]
    pub fn close_stdin(&mut self) {
        self.stdin_closed = true;
    }

    #[inline]
    pub fn close_stdout(&mut self) {
        self.stdout_closed = true;
    }

    #[inline]
    pub fn close_stderr(&mut self) {
        self.stderr_closed = true;
    }

    fn read_stdin(&mut self, req: ReadStdinReq) -> Result<&[u8], StdioError> {
        let ReadStdinReq { max_bytes } = req;
        if self.stdin_closed {
            return Err(StdioError::Closed);
        }
        let rem = self.stdin.len().saturating_sub(self.stdin_pos);
        let n = max_bytes.min(rem);
        let start = self.stdin_pos;
        self.stdin_pos = start.saturating_add(n);
        Ok(&self.stdin[start..start + n])
    }

    fn write_stdout(&mut self, req: WriteStdoutReq<'_>) -> Result<usize, StdioError> {
        let WriteStdoutReq { bytes } = req;
        if self.stdout_closed {
            return Err(StdioError::Closed);
        }
        if self.stdout.len().saturating_add(bytes.len()) > self.max_output_bytes {
            return Err(StdioError::BufferFull { max_bytes: self.max_output_bytes });
        }
        self.stdout.try_extend_from_slice(bytes).map_err(|_| StdioError::OutOfMemory)?;
        Ok(bytes.len())
    }

    fn write_stderr(&mut self, req: WriteStderrReq<'_>) -> Result<usize, StdioError> {
        let WriteStderrReq { bytes } = req;
        if self.stderr_closed {
            return Err(StdioError::Closed);
        }
        if self.stderr.len().saturating_add(bytes.len()) > self.max_output_bytes {
            return Err(StdioError::BufferFull { max_bytes: self.max_output_bytes });
        }
        self.stderr.try_extend_from_slice(bytes).map_err(|_| StdioError::OutOfMemory)?;
        Ok(bytes.len())
    }
}

impl Handler<ReadStdinReq> for SimStdioHandler<'_> {
    type Error = core::convert::Infallible;

    fn perform<Out>(
        &mut self,
        request: ReadStdinReq,
        deliver: impl FnOnce(Perform<Result<&'_ [u8], StdioError>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let ans = self.read_stdin(request);
        Ok(deliver(Perform::Answered(ans)))
    }
}

impl Handler<WriteStdoutReq<'_>> for SimStdioHandler<'_> {
    type Error = core::convert::Infallible;

    fn perform<Out>(
        &mut self,
        request: WriteStdoutReq<'_>,
        deliver: impl FnOnce(Perform<Result<usize, StdioError>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let ans = self.write_stdout(request);
        Ok(deliver(Perform::Answered(ans)))
    }
}

impl Handler<WriteStderrReq<'_>> for SimStdioHandler<'_> {
    type Error = core::convert::Infallible;

    fn perform<Out>(
        &mut self,
        request: WriteStderrReq<'_>,
        deliver: impl FnOnce(Perform<Result<usize, StdioError>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let ans = self.write_stderr(request);
        Ok(deliver(Perform::Answered(ans)))
    }
}

impl Handler<StdioReq<'_>> for SimStdioHandler<'_> {
    type Error = core::convert::Infallible;

    fn perform<Out>(
        &mut self,
        request: StdioReq<'_>,
        deliver: impl FnOnce(Perform<StdioAns<'_>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let ans = match request {
            StdioReq::ReadStdin(req) => StdioAns::Stdin(self.read_stdin(req)),
            StdioReq::WriteStdout(req) => StdioAns::Stdout(self.write_stdout(req)),
            StdioReq::WriteStderr(req) => StdioAns::Stderr(self.write_stderr(req)),
        };
        Ok(deliver(Perform::Answered(ans)))
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use blaze_machine::{DriveOpts, Machine, Turn};
    use blaze_own::{Alloc, Vec, testing::heap};

    use super::{
        ReadStdinReq, SimStdioHandler, SimStdioOpts, StdioAns, StdioError, StdioReq,
        WriteStderrReq, WriteStdoutReq,
    };
    use crate::{
        io::{Io, Limits, SimWorld},
        mem::TASK_FRAME,
    };

    enum DrainAndReportMachine<'a> {
        Init { chunk: usize, banner: &'a [u8], collected: Vec<u8, Alloc<'a>> },
        Reading { chunk: usize, banner: &'a [u8], collected: Vec<u8, Alloc<'a>> },
        WritingBanner { collected: Vec<u8, Alloc<'a>> },
    }

    impl<'a> Machine for DrainAndReportMachine<'a> {
        type Req = StdioReq<'a>;
        type Out = Vec<u8, Alloc<'a>>;
        type Err = StdioError;

        fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
            match self {
                DrainAndReportMachine::Init { chunk, banner, collected } => Turn::Ask {
                    request: StdioReq::ReadStdin(ReadStdinReq { max_bytes: chunk }),
                    next: DrainAndReportMachine::Reading { chunk, banner, collected },
                },
                _ => unreachable!("DrainAndReportMachine::start called on non-Init state"),
            }
        }

        fn resume(
            self,
            answer: StdioAns<'_>,
            _cx: &mut (),
        ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
            match (self, answer) {
                (
                    DrainAndReportMachine::Reading { chunk, banner, mut collected },
                    StdioAns::Stdin(Ok(bytes)),
                ) => {
                    if bytes.is_empty() {
                        Turn::Ask {
                            request: StdioReq::WriteStderr(WriteStderrReq { bytes: banner }),
                            next: DrainAndReportMachine::WritingBanner { collected },
                        }
                    } else {
                        if collected.try_extend_from_slice(bytes).is_err() {
                            return Turn::Fail(StdioError::OutOfMemory);
                        }
                        Turn::Ask {
                            request: StdioReq::ReadStdin(ReadStdinReq { max_bytes: chunk }),
                            next: DrainAndReportMachine::Reading { chunk, banner, collected },
                        }
                    }
                }
                (DrainAndReportMachine::Reading { .. }, StdioAns::Stdin(Err(err))) => {
                    Turn::Fail(err)
                }
                (
                    DrainAndReportMachine::WritingBanner { collected },
                    StdioAns::Stderr(Ok(_)),
                ) => Turn::Done(collected),
                (DrainAndReportMachine::WritingBanner { .. }, StdioAns::Stderr(Err(err))) => {
                    Turn::Fail(err)
                }
                _ => unreachable!("typed StdioAns always matches active state"),
            }
        }
    }

    #[test]
    fn cx_run_stdio_roundtrip() {
        let h = heap();
        let alloc = h.alloc();
        let io = Io::simulated(SimWorld::new(89, Limits::USUAL));
        let cx = io.cx(Duration::from_secs(10), TASK_FRAME).expect("root cx");

        let mut handler = SimStdioHandler::new(SimStdioOpts {
            stdin: b"hello blaze stdio",
            max_output_bytes: 64,
            alloc,
        });

        // 1. Chunked ReadStdinReq through Cx::run_with until EOF, copying into caller Vec.
        let mut read_all = Vec::new_in(alloc);
        loop {
            let n = cx
                .run_with("bstd.io.read_stdin", &mut handler, ReadStdinReq { max_bytes: 6 }, |res| {
                    let chunk = res.expect("stdin read ok");
                    read_all.extend_from_slice(chunk);
                    chunk.len()
                })
                .expect("handler perform read_stdin");
            if n == 0 {
                break;
            }
        }
        assert_eq!(read_all.as_slice(), b"hello blaze stdio");
        assert_eq!(handler.stdin_remaining(), 0);

        // 2. WriteStdoutReq and WriteStderrReq through Cx::run.
        let out_n = cx
            .run("bstd.io.write_stdout", &mut handler, WriteStdoutReq { bytes: read_all.as_slice() })
            .expect("handler perform write_stdout")
            .expect("write_stdout ok");
        assert_eq!(out_n, b"hello blaze stdio".len());
        assert_eq!(handler.stdout(), b"hello blaze stdio");

        let err_n = cx
            .run("bstd.io.write_stderr", &mut handler, WriteStderrReq { bytes: b"diag: ok\n" })
            .expect("handler perform write_stderr")
            .expect("write_stderr ok");
        assert_eq!(err_n, b"diag: ok\n".len());
        assert_eq!(handler.stderr(), b"diag: ok\n");

        // 3. Bounded cap refusal and closed-stream refusal.
        let overflow = cx
            .run("bstd.io.write_overflow", &mut handler, WriteStdoutReq { bytes: &[b'x'; 64] })
            .expect("handler perform overflow");
        assert_eq!(overflow, Err(StdioError::BufferFull { max_bytes: 64 }));

        handler.close_stderr();
        let closed = cx
            .run("bstd.io.write_closed", &mut handler, WriteStderrReq { bytes: b"late" })
            .expect("handler perform closed");
        assert_eq!(closed, Err(StdioError::Closed));

        // 4. Driving a sans-IO machine over StdioReq through Cx::drive_machine.
        let mut machine_handler = SimStdioHandler::new(SimStdioOpts {
            stdin: b"stream-bytes",
            max_output_bytes: 64,
            alloc,
        });
        let machine = DrainAndReportMachine::Init {
            chunk: 5,
            banner: b"done",
            collected: Vec::new_in(alloc),
        };
        let got = cx
            .drive_machine(
                "bstd.io.stdio_machine",
                &mut machine_handler,
                &mut (),
                machine,
                DriveOpts { max_steps: 16 },
            )
            .expect("stdio machine succeeds");
        assert_eq!(got.as_slice(), b"stream-bytes");
        assert_eq!(machine_handler.stderr(), b"done");
    }
}
