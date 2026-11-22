//! The wire between a compile-time process and blaze: one frame per [`Call`] or [`Answer`], a
//! 4-byte big-endian length then one deterministic CBOR array `[tag, fields..]` (blaze-cbor:
//! shortest heads, definite lengths), every length capped by [`WireLimits`] before anything is
//! sized by it. The tags are the variants' order in call.rs; a new variant is a new tag at the
//! end and a new [`crate::PROTOCOL`].
//!
//! The same length prefix as the launcher's run wire (blaze-launch wire.rs), so one reader of
//! frames serves both; the caps are this protocol's own, since a file answer or a macro's
//! output is far past the launcher's 16 MiB.

#![blaze::sans_io]

use blaze_cbor::{Reader, write};
use blaze_own::AVec;

use crate::call::{Answer, Call, ClockKind, FileKind, Stream, SysError, Want, Words};

/// The caps of the wire (D165: a field each, drawn down by the simulation).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WireLimits {
    /// The most bytes of one frame's body.
    pub most_frame: u32,
    /// The most bytes of one path or variable name.
    pub most_name: u32,
    /// The most bytes of one word list (arguments, the environment, a listing).
    pub most_words: u32,
    /// The most random bytes one call asks for.
    pub most_random: u32,
}

impl WireLimits {
    /// 256 MiB a frame (a macro's output may be 2^28 bytes: host.rs `Limits.output`), 4 KiB a
    /// name (`PATH_MAX`), 1 MiB a word list (`ARG_MAX` is of that order), 1 MiB of randomness.
    pub const SHIPPED: WireLimits = WireLimits { most_frame: 256 << 20, most_name: 4096, most_words: 1 << 20, most_random: 1 << 20 };
}

/// Why bytes are not a frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WireError {
    /// A frame's length is past [`WireLimits::most_frame`].
    Frame { len: u64, most: u32 },
    /// The body is not the CBOR this protocol writes, or a field is past its cap.
    Cbor(blaze_cbor::Error),
    /// A tag no variant has.
    Tag { tag: u64 },
    /// A tag with the wrong number of fields.
    Fields { tag: u64, len: usize },
    /// A word list not ended by a NUL.
    Words,
}

impl From<blaze_cbor::Error> for WireError {
    fn from(error: blaze_cbor::Error) -> WireError {
        WireError::Cbor(error)
    }
}

/// The first whole frame at the start of `buf`: its body and the bytes it took, or `None` when
/// `buf` holds only part of one.
pub fn split_frame(buf: &[u8], limits: WireLimits) -> Result<Option<(&[u8], usize)>, WireError> {
    let Some(head) = buf.first_chunk::<4>() else { return Ok(None) };
    let len = u32::from_be_bytes(*head);
    if len > limits.most_frame {
        return Err(WireError::Frame { len: u64::from(len), most: limits.most_frame });
    }
    let end = 4 + wide(len);
    match buf.get(4..end) {
        Some(body) => Ok(Some((body, end))),
        None => Ok(None),
    }
}

fn wide(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

fn len64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn int_bits(n: i32) -> u64 {
    u64::from(u32::from_be_bytes(n.to_be_bytes()))
}

fn bits_int(n: u64) -> Result<i32, WireError> {
    let n = u32::try_from(n).map_err(|_| WireError::Cbor(blaze_cbor::Error { at: 0, why: "an i32's bits past u32" }))?;
    Ok(i32::from_be_bytes(n.to_be_bytes()))
}

/// Opens a frame: its length is patched by [`close`].
fn open(out: &mut AVec<'_, u8>) -> usize {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    start
}

fn close(out: &mut AVec<'_, u8>, start: usize, limits: WireLimits) -> Result<(), WireError> {
    let len = out.len() - start - 4;
    let Some(len32) = u32::try_from(len).ok().filter(|len| *len <= limits.most_frame) else {
        out.truncate(start);
        return Err(WireError::Frame { len: len64(len), most: limits.most_frame });
    };
    out[start..start + 4].copy_from_slice(&len32.to_be_bytes());
    Ok(())
}

fn head(out: &mut AVec<'_, u8>, tag: u64, fields: u64) {
    write::array(out, fields + 1);
    write::uint(out, tag);
}

fn maybe(out: &mut AVec<'_, u8>, bytes: Option<&[u8]>) {
    match bytes {
        None => write::array(out, 0),
        Some(bytes) => {
            write::array(out, 1);
            write::bytes(out, bytes);
        }
    }
}

fn cbor_uint_len(n: u64) -> usize {
    if n < 24 {
        1
    } else if n <= u64::from(u8::MAX) {
        2
    } else if n <= u64::from(u16::MAX) {
        3
    } else if n <= u64::from(u32::MAX) {
        5
    } else {
        9
    }
}

fn cbor_bytes_len(bytes: &[u8]) -> usize {
    cbor_uint_len(len64(bytes.len())).saturating_add(bytes.len())
}

fn cbor_maybe_len(bytes: Option<&[u8]>) -> usize {
    match bytes {
        None => 1,
        Some(bytes) => 1usize.saturating_add(cbor_bytes_len(bytes)),
    }
}

fn call_body_len(call: &Call<'_>) -> usize {
    // Array header (1 byte for 1..=5 elements) + tag (1 byte for 0..=23).
    const HEAD: usize = 2;
    match *call {
        Call::Hello { protocol } => HEAD + cbor_uint_len(u64::from(protocol)),
        Call::Look { path, .. } => HEAD.saturating_add(cbor_bytes_len(path)).saturating_add(1),
        Call::Env { name } | Call::CreateDir { path: name } | Call::Remove { path: name } | Call::TrackPath { path: name } | Call::Denied { name } => {
            HEAD.saturating_add(cbor_bytes_len(name))
        }
        Call::EnvAll | Call::Args | Call::Cwd | Call::Thread | Call::Socket => HEAD,
        Call::Clock { .. } => HEAD + 1,
        Call::Random { len } => HEAD + cbor_uint_len(u64::from(len)),
        Call::Write { bytes, .. } => HEAD.saturating_add(1).saturating_add(cbor_bytes_len(bytes)),
        Call::WriteFile { path, at, bytes } => HEAD.saturating_add(cbor_bytes_len(path)).saturating_add(cbor_uint_len(at)).saturating_add(cbor_bytes_len(bytes)),
        Call::SetLen { path, len } => HEAD.saturating_add(cbor_bytes_len(path)).saturating_add(cbor_uint_len(len)),
        Call::Rename { from, to } => HEAD.saturating_add(cbor_bytes_len(from)).saturating_add(cbor_bytes_len(to)),
        Call::Sleep { ns } => HEAD + cbor_uint_len(ns),
        Call::Spawn { program, args, env, cwd } => HEAD
            .saturating_add(cbor_bytes_len(program))
            .saturating_add(cbor_bytes_len(args.as_bytes()))
            .saturating_add(cbor_bytes_len(env.as_bytes()))
            .saturating_add(cbor_bytes_len(cwd)),
        Call::Bridge { method, args } => HEAD.saturating_add(cbor_uint_len(u64::from(method))).saturating_add(cbor_bytes_len(args)),
        Call::TrackEnv { name, value } => HEAD.saturating_add(cbor_bytes_len(name)).saturating_add(cbor_maybe_len(value)),
        Call::Return { output } => HEAD.saturating_add(cbor_bytes_len(output)),
        Call::Exit { code } => HEAD + cbor_uint_len(int_bits(code)),
    }
}

fn answer_body_len(answer: &Answer<'_>) -> usize {
    // Array header (1 byte for 1..=4 elements) + tag (1 byte for 0..=11).
    const HEAD: usize = 2;
    match *answer {
        Answer::Hello { protocol } => HEAD + cbor_uint_len(u64::from(protocol)),
        Answer::File { bytes } | Answer::Random { bytes } | Answer::Bridge { reply: bytes } => HEAD.saturating_add(cbor_bytes_len(bytes)),
        Answer::Stat { len, .. } => HEAD + 1 + cbor_uint_len(len),
        Answer::Listing { names } | Answer::Words { words: names } => HEAD.saturating_add(cbor_bytes_len(names.as_bytes())),
        Answer::Value { value } => HEAD.saturating_add(cbor_maybe_len(value)),
        Answer::Clock { ns } => HEAD + cbor_uint_len(ns),
        Answer::Done => HEAD,
        Answer::Spawned { status, stdout, stderr } => {
            HEAD.saturating_add(cbor_uint_len(int_bits(status))).saturating_add(cbor_bytes_len(stdout)).saturating_add(cbor_bytes_len(stderr))
        }
        Answer::Error(_) => HEAD + 1,
    }
}

/// `call` as one frame at the end of `out`; on an error `out` is as it was.
pub fn write_call(call: &Call<'_>, out: &mut AVec<'_, u8>, limits: WireLimits) -> Result<(), WireError> {
    let body_len = call_body_len(call);
    if len64(body_len) > u64::from(limits.most_frame) {
        return Err(WireError::Frame { len: len64(body_len), most: limits.most_frame });
    }
    out.reserve(body_len.saturating_add(4));
    let start = open(out);
    match *call {
        Call::Hello { protocol } => {
            head(out, 0, 1);
            write::uint(out, u64::from(protocol));
        }
        Call::Look { path, want } => {
            head(out, 1, 2);
            write::bytes(out, path);
            write::uint(out, match want {
                Want::Read => 0,
                Want::Stat => 1,
                Want::List => 2,
            });
        }
        Call::Env { name } => {
            head(out, 2, 1);
            write::bytes(out, name);
        }
        Call::EnvAll => head(out, 3, 0),
        Call::Args => head(out, 4, 0),
        Call::Cwd => head(out, 5, 0),
        Call::Clock { which } => {
            head(out, 6, 1);
            write::uint(out, match which {
                ClockKind::Wall => 0,
                ClockKind::Monotonic => 1,
            });
        }
        Call::Random { len } => {
            head(out, 7, 1);
            write::uint(out, u64::from(len));
        }
        Call::Write { stream, bytes } => {
            head(out, 8, 2);
            write::uint(out, match stream {
                Stream::Out => 1,
                Stream::Err => 2,
            });
            write::bytes(out, bytes);
        }
        Call::WriteFile { path, at, bytes } => {
            head(out, 9, 3);
            write::bytes(out, path);
            write::uint(out, at);
            write::bytes(out, bytes);
        }
        Call::SetLen { path, len } => {
            head(out, 10, 2);
            write::bytes(out, path);
            write::uint(out, len);
        }
        Call::CreateDir { path } => {
            head(out, 11, 1);
            write::bytes(out, path);
        }
        Call::Remove { path } => {
            head(out, 12, 1);
            write::bytes(out, path);
        }
        Call::Rename { from, to } => {
            head(out, 13, 2);
            write::bytes(out, from);
            write::bytes(out, to);
        }
        Call::Sleep { ns } => {
            head(out, 14, 1);
            write::uint(out, ns);
        }
        Call::Thread => head(out, 15, 0),
        Call::Socket => head(out, 16, 0),
        Call::Spawn { program, args, env, cwd } => {
            head(out, 17, 4);
            write::bytes(out, program);
            write::bytes(out, args.as_bytes());
            write::bytes(out, env.as_bytes());
            write::bytes(out, cwd);
        }
        Call::Bridge { method, args } => {
            head(out, 18, 2);
            write::uint(out, u64::from(method));
            write::bytes(out, args);
        }
        Call::TrackEnv { name, value } => {
            head(out, 19, 2);
            write::bytes(out, name);
            maybe(out, value);
        }
        Call::TrackPath { path } => {
            head(out, 20, 1);
            write::bytes(out, path);
        }
        Call::Return { output } => {
            head(out, 21, 1);
            write::bytes(out, output);
        }
        Call::Exit { code } => {
            head(out, 22, 1);
            write::uint(out, int_bits(code));
        }
        Call::Denied { name } => {
            head(out, 23, 1);
            write::bytes(out, name);
        }
    }
    close(out, start, limits)
}

struct Fields<'f> {
    r: Reader<'f>,
    tag: u64,
    len: usize,
    limits: WireLimits,
}

impl<'f> Fields<'f> {
    fn open(body: &'f [u8], most_tag: u64, limits: WireLimits) -> Result<Fields<'f>, WireError> {
        let mut r = Reader::new(body);
        let len = r.array(8, "frame fields")?;
        if len == 0 {
            return Err(WireError::Fields { tag: 0, len });
        }
        let tag = r.uint()?;
        if tag > most_tag {
            return Err(WireError::Tag { tag });
        }
        Ok(Fields { r, tag, len: len - 1, limits })
    }

    fn expect(&self, n: usize) -> Result<(), WireError> {
        if self.len == n { Ok(()) } else { Err(WireError::Fields { tag: self.tag, len: self.len }) }
    }

    fn name(&mut self) -> Result<&'f [u8], WireError> {
        Ok(self.r.bytes(wide(self.limits.most_name), "a path or name")?)
    }

    fn data(&mut self) -> Result<&'f [u8], WireError> {
        Ok(self.r.bytes(wide(self.limits.most_frame), "a frame's bytes")?)
    }

    fn random(&mut self) -> Result<&'f [u8], WireError> {
        Ok(self.r.bytes(wide(self.limits.most_random), "random bytes")?)
    }

    fn words(&mut self) -> Result<Words<'f>, WireError> {
        Words::new(self.r.bytes(wide(self.limits.most_words), "a word list")?).ok_or(WireError::Words)
    }

    fn uint(&mut self, most: u64, what: &'static str) -> Result<u64, WireError> {
        Ok(self.r.uint_most(most, what)?)
    }

    fn maybe(&mut self) -> Result<Option<&'f [u8]>, WireError> {
        match self.r.array(1, "an optional value")? {
            0 => Ok(None),
            _ => Ok(Some(self.r.bytes(wide(self.limits.most_words), "a variable's value")?)),
        }
    }

    fn end<T>(self, value: T) -> Result<T, WireError> {
        self.r.end()?;
        Ok(value)
    }
}

/// One frame's body read as a call.
pub fn read_call(body: &[u8], limits: WireLimits) -> Result<Call<'_>, WireError> {
    let mut f = Fields::open(body, 23, limits)?;
    let call = match f.tag {
        0 => {
            f.expect(1)?;
            Call::Hello { protocol: u32::try_from(f.uint(u64::from(u32::MAX), "protocol")?).unwrap_or(u32::MAX) }
        }
        1 => {
            f.expect(2)?;
            let path = f.name()?;
            let want = match f.uint(2, "want")? {
                0 => Want::Read,
                1 => Want::Stat,
                _ => Want::List,
            };
            Call::Look { path, want }
        }
        2 => {
            f.expect(1)?;
            Call::Env { name: f.name()? }
        }
        3 => {
            f.expect(0)?;
            Call::EnvAll
        }
        4 => {
            f.expect(0)?;
            Call::Args
        }
        5 => {
            f.expect(0)?;
            Call::Cwd
        }
        6 => {
            f.expect(1)?;
            Call::Clock { which: if f.uint(1, "clock")? == 0 { ClockKind::Wall } else { ClockKind::Monotonic } }
        }
        7 => {
            f.expect(1)?;
            let most = u64::from(f.limits.most_random);
            Call::Random { len: u32::try_from(f.uint(most, "random length")?).unwrap_or(u32::MAX) }
        }
        8 => {
            f.expect(2)?;
            let stream = match f.uint(2, "stream")? {
                1 => Stream::Out,
                2 => Stream::Err,
                tag => return Err(WireError::Tag { tag }),
            };
            Call::Write { stream, bytes: f.data()? }
        }
        9 => {
            f.expect(3)?;
            let path = f.name()?;
            let at = f.uint(u64::MAX, "offset")?;
            Call::WriteFile { path, at, bytes: f.data()? }
        }
        10 => {
            f.expect(2)?;
            let path = f.name()?;
            Call::SetLen { path, len: f.uint(u64::MAX, "length")? }
        }
        11 => {
            f.expect(1)?;
            Call::CreateDir { path: f.name()? }
        }
        12 => {
            f.expect(1)?;
            Call::Remove { path: f.name()? }
        }
        13 => {
            f.expect(2)?;
            let from = f.name()?;
            Call::Rename { from, to: f.name()? }
        }
        14 => {
            f.expect(1)?;
            Call::Sleep { ns: f.uint(u64::MAX, "sleep")? }
        }
        15 => {
            f.expect(0)?;
            Call::Thread
        }
        16 => {
            f.expect(0)?;
            Call::Socket
        }
        17 => {
            f.expect(4)?;
            let program = f.name()?;
            let args = f.words()?;
            let env = f.words()?;
            Call::Spawn { program, args, env, cwd: f.name()? }
        }
        18 => {
            f.expect(2)?;
            let method = u8::try_from(f.uint(255, "bridge method")?).unwrap_or(u8::MAX);
            Call::Bridge { method, args: f.data()? }
        }
        19 => {
            f.expect(2)?;
            let name = f.name()?;
            Call::TrackEnv { name, value: f.maybe()? }
        }
        20 => {
            f.expect(1)?;
            Call::TrackPath { path: f.name()? }
        }
        21 => {
            f.expect(1)?;
            Call::Return { output: f.data()? }
        }
        22 => {
            f.expect(1)?;
            Call::Exit { code: bits_int(f.uint(u64::from(u32::MAX), "exit code")?)? }
        }
        _ => {
            f.expect(1)?;
            Call::Denied { name: f.name()? }
        }
    };
    f.end(call)
}

fn sys_tag(error: SysError) -> u64 {
    match error {
        SysError::NotFound => 0,
        SysError::NotADir => 1,
        SysError::IsADir => 2,
        SysError::ReadOnly => 3,
        SysError::Unsupported => 4,
        SysError::Invalid => 5,
        SysError::NoSpace => 6,
        SysError::TooLarge => 7,
        SysError::Exists => 8,
        SysError::NotEmpty => 9,
    }
}

const SYS_ERRORS: [SysError; 10] = [
    SysError::NotFound,
    SysError::NotADir,
    SysError::IsADir,
    SysError::ReadOnly,
    SysError::Unsupported,
    SysError::Invalid,
    SysError::NoSpace,
    SysError::TooLarge,
    SysError::Exists,
    SysError::NotEmpty,
];

/// `answer` as one frame at the end of `out`; on an error `out` is as it was.
pub fn write_answer(answer: &Answer<'_>, out: &mut AVec<'_, u8>, limits: WireLimits) -> Result<(), WireError> {
    let body_len = answer_body_len(answer);
    if len64(body_len) > u64::from(limits.most_frame) {
        return Err(WireError::Frame { len: len64(body_len), most: limits.most_frame });
    }
    out.reserve(body_len.saturating_add(4));
    let start = open(out);
    match *answer {
        Answer::Hello { protocol } => {
            head(out, 0, 1);
            write::uint(out, u64::from(protocol));
        }
        Answer::File { bytes } => {
            head(out, 1, 1);
            write::bytes(out, bytes);
        }
        Answer::Stat { kind, len } => {
            head(out, 2, 2);
            write::uint(out, match kind {
                FileKind::File => 0,
                FileKind::Dir => 1,
            });
            write::uint(out, len);
        }
        Answer::Listing { names } => {
            head(out, 3, 1);
            write::bytes(out, names.as_bytes());
        }
        Answer::Value { value } => {
            head(out, 4, 1);
            maybe(out, value);
        }
        Answer::Words { words } => {
            head(out, 5, 1);
            write::bytes(out, words.as_bytes());
        }
        Answer::Clock { ns } => {
            head(out, 6, 1);
            write::uint(out, ns);
        }
        Answer::Random { bytes } => {
            head(out, 7, 1);
            write::bytes(out, bytes);
        }
        Answer::Done => head(out, 8, 0),
        Answer::Spawned { status, stdout, stderr } => {
            head(out, 9, 3);
            write::uint(out, int_bits(status));
            write::bytes(out, stdout);
            write::bytes(out, stderr);
        }
        Answer::Bridge { reply } => {
            head(out, 10, 1);
            write::bytes(out, reply);
        }
        Answer::Error(error) => {
            head(out, 11, 1);
            write::uint(out, sys_tag(error));
        }
    }
    close(out, start, limits)
}

/// One frame's body read as an answer.
pub fn read_answer(body: &[u8], limits: WireLimits) -> Result<Answer<'_>, WireError> {
    let mut f = Fields::open(body, 11, limits)?;
    let answer = match f.tag {
        0 => {
            f.expect(1)?;
            Answer::Hello { protocol: u32::try_from(f.uint(u64::from(u32::MAX), "protocol")?).unwrap_or(u32::MAX) }
        }
        1 => {
            f.expect(1)?;
            Answer::File { bytes: f.data()? }
        }
        2 => {
            f.expect(2)?;
            let kind = if f.uint(1, "file kind")? == 0 { FileKind::File } else { FileKind::Dir };
            Answer::Stat { kind, len: f.uint(u64::MAX, "length")? }
        }
        3 => {
            f.expect(1)?;
            Answer::Listing { names: f.words()? }
        }
        4 => {
            f.expect(1)?;
            Answer::Value { value: f.maybe()? }
        }
        5 => {
            f.expect(1)?;
            Answer::Words { words: f.words()? }
        }
        6 => {
            f.expect(1)?;
            Answer::Clock { ns: f.uint(u64::MAX, "clock")? }
        }
        7 => {
            f.expect(1)?;
            Answer::Random { bytes: f.random()? }
        }
        8 => {
            f.expect(0)?;
            Answer::Done
        }
        9 => {
            f.expect(3)?;
            let status = bits_int(f.uint(u64::from(u32::MAX), "status")?)?;
            let stdout = f.data()?;
            Answer::Spawned { status, stdout, stderr: f.data()? }
        }
        10 => {
            f.expect(1)?;
            Answer::Bridge { reply: f.data()? }
        }
        _ => {
            f.expect(1)?;
            let at = f.uint(len64(SYS_ERRORS.len() - 1), "error")?;
            Answer::Error(SYS_ERRORS[usize::try_from(at).unwrap_or(0)])
        }
    };
    f.end(answer)
}
