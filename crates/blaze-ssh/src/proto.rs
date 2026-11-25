//! Sans-IO Blaze Remote Shell Protocol (`BSH1`): low-latency multiplexed framing for a remote
//! `blaze-shell` agent uploaded/bootstrapped over SSH so interactive typing stays local-speed
//! while structured pipelines, completions, and git prompt snapshots stream back over SSH stdio.

use blaze_own::{AString, AVec, Alloc, String, Vec};

/// 4-byte protocol magic for every `BSH1` frame.
pub const MAGIC: [u8; 4] = *b"BSH1";
pub const PROTOCOL_VERSION: u16 = 1;

/// Explicit frame limits (rule `bounded-progress`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameLimits {
    pub max_payload_bytes: usize,
}

impl FrameLimits {
    pub const SHIPPED: Self = Self { max_payload_bytes: 16 << 20 };
}

/// Typed framing refusal (rule `errors-name-the-thing`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    BadMagic,
    PayloadTooLarge { len: usize, max: usize },
    Truncated,
    UnknownTag(u8),
    InvalidUtf8,
}

/// Remote completion suggestion streamed from the remote `blaze-shell` agent.
#[derive(Debug, PartialEq, Eq)]
pub struct RemoteCompletion<'a> {
    pub label: AString<'a>,
    pub detail: AString<'a>,
    pub is_dir: bool,
}

/// Compact remote Git summary streamed from the remote `blaze-shell` agent.
#[derive(Debug, PartialEq, Eq)]
pub struct RemoteGitSummary<'a> {
    pub cwd: AString<'a>,
    pub branch: AString<'a>,
    pub dirty_count: u32,
    pub stashed: u32,
}

/// Multiplexed `BSH1` protocol message exchanged between local `blaze-shell` and remote agent.
#[derive(Debug, PartialEq, Eq)]
pub enum Message<'a> {
    Hello { version: u16, client_id: AString<'a> },
    Welcome { version: u16, remote_host: AString<'a>, cwd: AString<'a> },
    Exec { job_id: u64, command: AString<'a> },
    ExecOutput { job_id: u64, is_stderr: bool, chunk: AVec<'a, u8> },
    ExecDone { job_id: u64, exit_code: i32, duration_ms: u64 },
    Cancel { job_id: u64 },
    CompleteReq { req_id: u64, line: AString<'a>, cursor: u32 },
    CompleteResp { req_id: u64, items: AVec<'a, RemoteCompletion<'a>> },
    PromptQuery { req_id: u64 },
    PromptResp { req_id: u64, summary: RemoteGitSummary<'a> },
    Ping { seq: u64 },
    Pong { seq: u64 },
}

const TAG_HELLO: u8 = 1;
const TAG_WELCOME: u8 = 2;
const TAG_EXEC: u8 = 3;
const TAG_EXEC_OUTPUT: u8 = 4;
const TAG_EXEC_DONE: u8 = 5;
const TAG_CANCEL: u8 = 6;
const TAG_COMPLETE_REQ: u8 = 7;
const TAG_COMPLETE_RESP: u8 = 8;
const TAG_PROMPT_QUERY: u8 = 9;
const TAG_PROMPT_RESP: u8 = 10;
const TAG_PING: u8 = 11;
const TAG_PONG: u8 = 12;

/// Sans-IO streaming frame encoder/decoder over any byte transport.
pub struct FrameCodec<'a> {
    alloc: Alloc<'a>,
    limits: FrameLimits,
    rx: AVec<'a, u8>,
}

impl<'a> FrameCodec<'a> {
    pub fn new(limits: FrameLimits, alloc: Alloc<'a>) -> Self {
        Self { alloc, limits, rx: Vec::new_in(alloc) }
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) {
        self.rx.extend_from_slice(bytes);
    }

    /// Try to decode the next complete `BSH1` message from the internal receive buffer.
    pub fn next_message(&mut self) -> Result<Option<Message<'a>>, FrameError> {
        if self.rx.len() < 9 {
            return Ok(None);
        }
        if self.rx[..4] != MAGIC {
            return Err(FrameError::BadMagic);
        }
        let len = u32::from_le_bytes([self.rx[4], self.rx[5], self.rx[6], self.rx[7]]) as usize;
        if len > self.limits.max_payload_bytes {
            return Err(FrameError::PayloadTooLarge { len, max: self.limits.max_payload_bytes });
        }
        let total = 8 + 1 + len;
        if self.rx.len() < total {
            return Ok(None);
        }
        let tag = self.rx[8];
        let payload = &self.rx[9..total];
        let msg = decode_payload(tag, payload, self.alloc)?;
        // Shift remaining buffered bytes forward.
        let rem = self.rx.len() - total;
        if rem > 0 {
            self.rx.copy_within(total.., 0);
        }
        self.rx.truncate(rem);
        Ok(Some(msg))
    }

    /// Encode `msg` into `out` as a `BSH1` wire frame.
    pub fn encode(msg: &Message<'_>, out: &mut AVec<'a, u8>, alloc: Alloc<'a>) {
        let mut body = Vec::new_in(alloc);
        let tag = encode_payload(msg, &mut body);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.push(tag);
        out.extend_from_slice(&body);
    }
}

fn push_str(buf: &mut AVec<'_, u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

fn encode_payload(msg: &Message<'_>, buf: &mut AVec<'_, u8>) -> u8 {
    match msg {
        Message::Hello { version, client_id } => {
            buf.extend_from_slice(&version.to_le_bytes());
            push_str(buf, client_id.as_str());
            TAG_HELLO
        }
        Message::Welcome { version, remote_host, cwd } => {
            buf.extend_from_slice(&version.to_le_bytes());
            push_str(buf, remote_host.as_str());
            push_str(buf, cwd.as_str());
            TAG_WELCOME
        }
        Message::Exec { job_id, command } => {
            buf.extend_from_slice(&job_id.to_le_bytes());
            push_str(buf, command.as_str());
            TAG_EXEC
        }
        Message::ExecOutput { job_id, is_stderr, chunk } => {
            buf.extend_from_slice(&job_id.to_le_bytes());
            buf.push(u8::from(*is_stderr));
            buf.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
            buf.extend_from_slice(chunk);
            TAG_EXEC_OUTPUT
        }
        Message::ExecDone { job_id, exit_code, duration_ms } => {
            buf.extend_from_slice(&job_id.to_le_bytes());
            buf.extend_from_slice(&exit_code.to_le_bytes());
            buf.extend_from_slice(&duration_ms.to_le_bytes());
            TAG_EXEC_DONE
        }
        Message::Cancel { job_id } => {
            buf.extend_from_slice(&job_id.to_le_bytes());
            TAG_CANCEL
        }
        Message::CompleteReq { req_id, line, cursor } => {
            buf.extend_from_slice(&req_id.to_le_bytes());
            push_str(buf, line.as_str());
            buf.extend_from_slice(&cursor.to_le_bytes());
            TAG_COMPLETE_REQ
        }
        Message::CompleteResp { req_id, items } => {
            buf.extend_from_slice(&req_id.to_le_bytes());
            buf.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for item in items.iter() {
                push_str(buf, item.label.as_str());
                push_str(buf, item.detail.as_str());
                buf.push(u8::from(item.is_dir));
            }
            TAG_COMPLETE_RESP
        }
        Message::PromptQuery { req_id } => {
            buf.extend_from_slice(&req_id.to_le_bytes());
            TAG_PROMPT_QUERY
        }
        Message::PromptResp { req_id, summary } => {
            buf.extend_from_slice(&req_id.to_le_bytes());
            push_str(buf, summary.cwd.as_str());
            push_str(buf, summary.branch.as_str());
            buf.extend_from_slice(&summary.dirty_count.to_le_bytes());
            buf.extend_from_slice(&summary.stashed.to_le_bytes());
            TAG_PROMPT_RESP
        }
        Message::Ping { seq } => {
            buf.extend_from_slice(&seq.to_le_bytes());
            TAG_PING
        }
        Message::Pong { seq } => {
            buf.extend_from_slice(&seq.to_le_bytes());
            TAG_PONG
        }
    }
}

struct Cursor<'b> {
    data: &'b [u8],
    pos: usize,
}

impl<'b> Cursor<'b> {
    fn take(&mut self, n: usize) -> Result<&'b [u8], FrameError> {
        if self.pos + n > self.data.len() {
            return Err(FrameError::Truncated);
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, FrameError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, FrameError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, FrameError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i32(&mut self) -> Result<i32, FrameError> {
        let b = self.take(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, FrameError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }

    fn string<'a>(&mut self, alloc: Alloc<'a>) -> Result<AString<'a>, FrameError> {
        let n = self.u32()? as usize;
        let b = self.take(n)?;
        let s = core::str::from_utf8(b).map_err(|_| FrameError::InvalidUtf8)?;
        Ok(String::from_str_in(s, alloc))
    }
}

fn decode_payload<'a>(
    tag: u8,
    payload: &[u8],
    alloc: Alloc<'a>,
) -> Result<Message<'a>, FrameError> {
    let mut c = Cursor { data: payload, pos: 0 };
    match tag {
        TAG_HELLO => Ok(Message::Hello { version: c.u16()?, client_id: c.string(alloc)? }),
        TAG_WELCOME => Ok(Message::Welcome {
            version: c.u16()?,
            remote_host: c.string(alloc)?,
            cwd: c.string(alloc)?,
        }),
        TAG_EXEC => Ok(Message::Exec { job_id: c.u64()?, command: c.string(alloc)? }),
        TAG_EXEC_OUTPUT => {
            let job_id = c.u64()?;
            let is_stderr = c.u8()? != 0;
            let len = c.u32()? as usize;
            let bytes = c.take(len)?;
            let mut chunk = Vec::with_capacity_in(len, alloc);
            chunk.extend_from_slice(bytes);
            Ok(Message::ExecOutput { job_id, is_stderr, chunk })
        }
        TAG_EXEC_DONE => Ok(Message::ExecDone {
            job_id: c.u64()?,
            exit_code: c.i32()?,
            duration_ms: c.u64()?,
        }),
        TAG_CANCEL => Ok(Message::Cancel { job_id: c.u64()? }),
        TAG_COMPLETE_REQ => Ok(Message::CompleteReq {
            req_id: c.u64()?,
            line: c.string(alloc)?,
            cursor: c.u32()?,
        }),
        TAG_COMPLETE_RESP => {
            let req_id = c.u64()?;
            let count = (c.u32()? as usize).min(4096);
            let mut items = Vec::with_capacity_in(count, alloc);
            for _ in 0..count {
                items.push(RemoteCompletion {
                    label: c.string(alloc)?,
                    detail: c.string(alloc)?,
                    is_dir: c.u8()? != 0,
                });
            }
            Ok(Message::CompleteResp { req_id, items })
        }
        TAG_PROMPT_QUERY => Ok(Message::PromptQuery { req_id: c.u64()? }),
        TAG_PROMPT_RESP => Ok(Message::PromptResp {
            req_id: c.u64()?,
            summary: RemoteGitSummary {
                cwd: c.string(alloc)?,
                branch: c.string(alloc)?,
                dirty_count: c.u32()?,
                stashed: c.u32()?,
            },
        }),
        TAG_PING => Ok(Message::Ping { seq: c.u64()? }),
        TAG_PONG => Ok(Message::Pong { seq: c.u64()? }),
        other => Err(FrameError::UnknownTag(other)),
    }
}
