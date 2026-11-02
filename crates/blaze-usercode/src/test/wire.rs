//! The fork server's protocol: what blaze ([`crate::test::queue`]) and a test binary's fork
//! server (the harness side, [`crate::test::fork_server`]) say on the Unix socket blaze listens
//! on. This file is compiled twice: as this module of blaze, and as text inside the harness of
//! every test binary blaze writes, in whatever edition the tested crate has. So it names
//! everything from `::std` and `::core`, uses nothing of blaze and nothing an edition changes,
//! has no `unsafe` (blaze-usercode forbids it), and holds no tests (they are in
//! `crate::test`'s).
//!
//! Every message is a frame: its body's length as a little-endian `u32`, then the body, at
//! most [`LONGEST`] bytes. A body is a kind byte and its fields: integers little-endian, a
//! string or bytes a `u32` length and then them, a list a `u32` count and then its items.
//! Frames are encoded into a buffer the caller keeps and read in place ([`View`], from
//! [`Frames::pop_view`]); [`Frame`] is the owned form.
//!
//! - The server, once connected and before any test runs: [`Frame::Hello`] (kind 1) with
//!   [`VERSION`], its process id, how long one fork of it took (nanoseconds, measured before the
//!   hello) and the name of every test of the binary in order; a test is named by its index in
//!   this list from then on.
//! - blaze, only as its first frame to the server: [`Frame::Replicate`] (kind 132), make `count`
//!   copies of yourself. Each copy is a server of its own: it connects anew on the same socket,
//!   says its own hello (the same tests) and runs what blaze sends it there, so a binary's forks
//!   run `count + 1` at a time. The server that was asked goes on as it was.
//! - blaze: [`Frame::Run`] (kind 129), the test at an index under an id of blaze's choosing.
//!   The server forks a child that marks its scope (`access(MARK + name)`), runs the one test
//!   as libtest would, with its output (standard output and error, together) going to the
//!   server, and exits; the server answers [`Frame::Ended`] (kind 2) with the id, how it ended,
//!   what it wrote and what it executed ([`Executed`]). Many tests run at once.
//! - blaze: [`Frame::Kill`] (kind 130): the test of the id is killed, with every process it
//!   started; its [`Frame::Ended`] still comes. A kill of an id that ended is nothing.
//! - blaze: [`Frame::Quit`] (kind 131): the server exits once every test it runs has ended
//!   and been answered. A server whose socket ends kills what it runs and exits.
//!
//! What a test executed (docs/v2/plan/lanes/fastcompile/select.md, "The record"): a server
//! built with jump-table slots maps, before each fork, a region of [`HEADER`] plus a byte per
//! slot, shared with the child, starting with [`header`]. The child's runtime arms it
//! (`__blaze_exec_arm`), then each slot's first call sets its byte; once the child has ended
//! the server reads it ([`read_region`], the rules of `blaze_testsel::record::Record::of_region`)
//! and sends the slots set.

/// The variable of a test binary's environment that makes its `main` a fork server: the
/// absolute path of the socket to connect to. The server takes it out of its environment
/// before any test runs.
pub const SOCKET: &str = "__BLAZE_FORK_SERVER";

/// What a child marks its scope with in the sandbox's record, the test's name after it
/// (`blaze-cargo`'s `sandbox::MARK`).
pub const MARK: &str = "/.blaze-test/";

/// The protocol's version, said by [`Frame::Hello`].
pub const VERSION: u32 = 3;

/// The most bytes a frame's body may have.
pub const LONGEST: u32 = 64 << 20;

/// The most bytes of output a test's [`Frame::Ended`] carries; past it the output is cut and
/// says so.
pub const LONGEST_OUTPUT: usize = 16 << 20;

/// The first bytes of an execution record's region.
pub const MAGIC: [u8; 8] = *b"blzexec1";

/// An execution record's header: [`MAGIC`], the slot count and the slots armed, each a
/// little-endian `u32`; slot `n`'s byte is at `HEADER + n`.
pub const HEADER: usize = 16;

/// What a server says a test executed when its binary has no slots.
pub const UNRECORDED: &str = "built without jump-table slots";

const HELLO: u8 = 1;
const ENDED: u8 = 2;
const RUN: u8 = 129;
const KILL: u8 = 130;
const QUIT: u8 = 131;
const REPLICATE: u8 = 132;

const LOW: u64 = 0x7f7f_7f7f_7f7f_7f7f;
const HIGH: u64 = 0x8080_8080_8080_8080;

/// How a test's child ended.
#[derive(PartialEq, Eq, Debug)]
pub enum End {
    /// Its exit code: 0 is a pass, as libtest would judge it; anything else a failure, whose
    /// output says why.
    Exited(i32),
    /// The signal that ended it: a kill, an abort, a crash.
    Signaled(i32),
    /// No child was made, and why: the fork failed, or there is no test at the index.
    NotStarted(String),
}

/// [`End`] read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EndView<'a> {
    Exited(i32),
    Signaled(i32),
    NotStarted(&'a str),
}

impl End {
    pub fn view<'a>(&'a self) -> EndView<'a> {
        match self {
            End::Exited(code) => EndView::Exited(*code),
            End::Signaled(signal) => EndView::Signaled(*signal),
            End::NotStarted(why) => EndView::NotStarted(why),
        }
    }
}

impl<'a> EndView<'a> {
    pub fn to_end(self) -> End {
        match self {
            EndView::Exited(code) => End::Exited(code),
            EndView::Signaled(signal) => End::Signaled(signal),
            EndView::NotStarted(why) => End::NotStarted(why.to_owned()), // alloc: the owned frame: what a result keeps
        }
    }
}

/// What a test's child executed.
#[derive(PartialEq, Eq, Debug)]
pub enum Executed {
    /// Its whole record: the slots it called, strictly increasing, each below `slots`.
    Hits { slots: u32, hits: Vec<u32> },
    /// Not whole, and why: the binary has no slots, the child was killed or did not exit, the
    /// runtime did not arm, the test did not start.
    Not(String),
}

/// [`Executed`] borrowed: what a server encodes from its buffers and a frame read in place
/// holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExecView<'a> {
    Hits { slots: u32, hits: Hits<'a> },
    Not(&'a str),
}

/// Slots as the wire has them: each a little-endian `u32`, strictly increasing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hits<'a>(&'a [u8]);

impl<'a> Hits<'a> {
    /// The hits `bytes` are, four bytes each; `None` for a length that is not a multiple of 4.
    pub fn of_bytes(bytes: &'a [u8]) -> Option<Hits<'a>> {
        match bytes.len() % 4 {
            0 => Some(Hits(bytes)),
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        self.0.len() / 4
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.0
    }

    pub fn iter(&self) -> impl Iterator<Item = u32> + 'a {
        self.0.chunks_exact(4).map(|it| u32::from_le_bytes([it[0], it[1], it[2], it[3]]))
    }

    pub fn to_vec(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.len());
        out.extend(self.iter());
        out
    }
}

impl<'a> ExecView<'a> {
    pub fn to_executed(self) -> Executed {
        match self {
            ExecView::Hits { slots, hits } => Executed::Hits { slots, hits: hits.to_vec() }, // alloc: the owned frame: what a result keeps
            ExecView::Not(why) => Executed::Not(why.to_owned()), // alloc: the owned frame: what a result keeps
        }
    }
}

/// The header a server writes into a test's region before it forks: `armed` is zero.
pub fn header(slots: u32) -> [u8; HEADER] {
    let mut out = [0u8; HEADER];
    out[..8].copy_from_slice(&MAGIC);
    out[8..12].copy_from_slice(&slots.to_le_bytes());
    out
}

/// What a test's child executed, from its region once it has ended, by the rules of
/// `blaze_testsel::record::Record::of_region`: whole only when the child exited (`ended` is
/// why not), the header is blaze's and the runtime armed every slot. `head` is the region's
/// header, `mapped` the slots the region has room for, and `word(n)` the little-endian `u64`
/// of its bytes `HEADER + 8n .. HEADER + 8n + 8`, for each `n` below `(slots + 7) / 8`. The
/// slots set go into `hits` as the wire has them ([`Hits`]), eight bytes a step: a word of
/// zeros is skipped in one compare, as most of a binary never runs in one test. The slot
/// count, or why the record is not whole.
pub fn read_region(
    head: [u8; HEADER],
    mapped: u32,
    ended: Result<(), &str>,
    mut word: impl FnMut(usize) -> u64,
    hits: &mut Vec<u8>,
) -> Result<u32, String> {
    hits.clear();
    if let Err(why) = ended {
        return Err(format!("its execution record is not whole: {}", why)); // alloc: error path
    }
    if head[..8] != MAGIC {
        return Err("its execution record does not start with blaze's magic".to_owned()); // alloc: error path
    }
    let slots = u32::from_le_bytes([head[8], head[9], head[10], head[11]]);
    let armed = u32::from_le_bytes([head[12], head[13], head[14], head[15]]);
    if armed != slots {
        return Err(format!("the runtime armed {} of its {} slots", armed, slots)); // alloc: error path
    }
    if slots > mapped {
        return Err(format!("its execution record holds fewer than its {} slots", slots)); // alloc: error path
    }
    let words = (slots as usize).div_ceil(8);
    let tail = slots % 8;
    // Four words a step, a group of zeros skipped in one compare: the loads of a group are
    // independent, so the step costs about one cycle a word and the scan runs near the
    // bandwidth of the cache the child left the region in. The last word, which may hold
    // bytes past the slots, is read alone and masked.
    let unmasked = if tail == 0 { words } else { words - 1 };
    let mut n = 0;
    while n + 4 <= unmasked {
        let group = [word(n), word(n + 1), word(n + 2), word(n + 3)];
        if group[0] | group[1] | group[2] | group[3] != 0 {
            for (k, value) in group.iter().enumerate() {
                push_set(hits, n + k, *value);
            }
        }
        n += 4;
    }
    while n < words {
        let mut value = word(n);
        if n + 1 == words && tail != 0 {
            value &= (1u64 << (8 * tail)) - 1;
        }
        push_set(hits, n, value);
        n += 1;
    }
    Ok(slots)
}

/// The slots of word `n` whose bytes are not zero, as the wire has them.
fn push_set(hits: &mut Vec<u8>, n: usize, value: u64) {
    if value == 0 {
        return;
    }
    // The high bit of each byte that is not zero: the low seven bits plus 0x7f carry into it,
    // and no byte carries into the next.
    let mut set = (((value & LOW) + LOW) | value) & HIGH;
    let base = (n * 8) as u32;
    while set != 0 {
        hits.extend_from_slice(&(base + set.trailing_zeros() / 8).to_le_bytes());
        set &= set - 1;
    }
}

/// A frame. [`View`] is the same read in place.
#[derive(PartialEq, Eq, Debug)]
pub enum Frame {
    /// `fork` is how long one fork of the server took, in nanoseconds.
    Hello { version: u32, pid: u32, fork: u64, tests: Vec<String> },
    Ended { id: u64, end: End, output: Vec<u8>, executed: Executed },
    Run { id: u64, index: u32 },
    Kill { id: u64 },
    Quit,
    Replicate { count: u32 },
}

/// A frame read in place from the bytes it came in: every length checked against them, every
/// string checked UTF-8, nothing copied.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View<'a> {
    Hello { version: u32, pid: u32, fork: u64, tests: Names<'a> },
    Ended { id: u64, end: EndView<'a>, output: &'a [u8], executed: ExecView<'a> },
    Run { id: u64, index: u32 },
    Kill { id: u64 },
    Quit,
    Replicate { count: u32 },
}

/// The names of a hello, read in place: `count` strings, each checked when the frame was read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Names<'a> {
    count: u32,
    bytes: &'a [u8],
}

impl<'a> Names<'a> {
    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = &'a str> + 'a {
        let mut at = Cursor { bytes: self.bytes, at: 0 };
        (0..self.count).map(move |_| at.string("test name").expect("a name checked when its hello was read"))
    }
}

// These writers and `Cursor` are not `blaze_format::cursor`'s: this file is also the text of
// every test binary's harness, which links nothing of blaze.
fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

/// [`Frame::Hello`] of these parts, with its length, added to `out`: the bytes
/// [`Frame::encode`] adds for it, from borrowed names, so a server says hello straight from its
/// tests' names, with no list of its own.
pub fn encode_hello<'a>(out: &mut Vec<u8>, version: u32, pid: u32, fork: u64, tests: impl Iterator<Item = &'a str>) {
    let start = out.len();
    put_u32(out, 0);
    put_hello(out, version, pid, fork, tests);
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

fn put_hello<'a>(out: &mut Vec<u8>, version: u32, pid: u32, fork: u64, tests: impl Iterator<Item = &'a str>) {
    out.push(HELLO);
    put_u32(out, version);
    put_u32(out, pid);
    put_u64(out, fork);
    let count = out.len();
    put_u32(out, 0);
    let mut names = 0u32;
    for name in tests {
        put_bytes(out, name.as_bytes());
        names += 1;
    }
    out[count..count + 4].copy_from_slice(&names.to_le_bytes());
}

/// [`Frame::Ended`] of these parts, with its length, added to `out`: the bytes
/// [`Frame::encode`] adds for it, from borrowed parts, so a server encodes each test's output
/// and hits once, straight from the buffers they were read into.
pub fn encode_ended<'a>(out: &mut Vec<u8>, id: u64, end: EndView<'a>, output: &[u8], executed: ExecView<'a>) {
    let why = match end {
        EndView::NotStarted(why) => why.len() + 4,
        EndView::Exited(_) | EndView::Signaled(_) => 4,
    };
    let record = match executed {
        ExecView::Hits { hits, .. } => 8 + hits.bytes().len(),
        ExecView::Not(why) => 4 + why.len(),
    };
    out.reserve(4 + 1 + 8 + 1 + why + 4 + output.len() + 1 + record);
    let start = out.len();
    put_u32(out, 0);
    out.push(ENDED);
    put_u64(out, id);
    put_end(out, end);
    put_bytes(out, output);
    match executed {
        ExecView::Hits { slots, hits } => {
            out.push(0);
            put_u32(out, slots);
            put_u32(out, hits.len() as u32);
            out.extend_from_slice(hits.bytes());
        }
        ExecView::Not(why) => {
            out.push(1);
            put_bytes(out, why.as_bytes());
        }
    }
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

/// The ranges of descriptors at 3 or above that `open` does not name, each `(first, last)` and
/// ascending, the last ending at `u32::MAX`: what a test's child closes with `close_range`, so it
/// keeps exactly the descriptors the program had when its server started. Sorts `open`.
pub fn closed_ranges(open: &mut Vec<u32>, out: &mut Vec<(u32, u32)>) {
    open.sort_unstable();
    out.clear();
    let mut first = 3u32;
    for &fd in open.iter() {
        if fd < first {
            continue;
        }
        if fd > first {
            out.push((first, fd - 1));
        }
        first = fd.saturating_add(1);
    }
    out.push((first, u32::MAX));
}

fn put_end<'a>(out: &mut Vec<u8>, end: EndView<'a>) {
    match end {
        EndView::Exited(code) => {
            out.push(0);
            out.extend_from_slice(&code.to_le_bytes());
        }
        EndView::Signaled(signal) => {
            out.push(1);
            out.extend_from_slice(&signal.to_le_bytes());
        }
        EndView::NotStarted(why) => {
            out.push(2);
            put_bytes(out, why.as_bytes());
        }
    }
}

impl Frame {
    /// The frame, with its length, added to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        put_u32(out, 0);
        match self {
            Frame::Hello { version, pid, fork, tests } => put_hello(out, *version, *pid, *fork, tests.iter().map(String::as_str)),
            Frame::Ended { id, end, output, executed } => {
                out.push(ENDED);
                put_u64(out, *id);
                put_end(out, end.view());
                put_bytes(out, output);
                match executed {
                    Executed::Hits { slots, hits } => {
                        out.push(0);
                        put_u32(out, *slots);
                        put_u32(out, hits.len() as u32);
                        for hit in hits {
                            put_u32(out, *hit);
                        }
                    }
                    Executed::Not(why) => {
                        out.push(1);
                        put_bytes(out, why.as_bytes());
                    }
                }
            }
            Frame::Run { id, index } => {
                out.push(RUN);
                put_u64(out, *id);
                put_u32(out, *index);
            }
            Frame::Kill { id } => {
                out.push(KILL);
                put_u64(out, *id);
            }
            Frame::Quit => out.push(QUIT),
            Frame::Replicate { count } => {
                out.push(REPLICATE);
                put_u32(out, *count);
            }
        }
        let len = (out.len() - start - 4) as u32;
        out[start..start + 4].copy_from_slice(&len.to_le_bytes());
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new(); // alloc: empty, allocates nothing
        self.encode(&mut out);
        out
    }

    /// The frame a body is, or why it is not one.
    pub fn decode(body: &[u8]) -> Result<Frame, String> {
        View::decode(body).map(View::to_frame)
    }
}

impl<'a> View<'a> {
    /// The frame a body is, read in place, or why it is not one.
    pub fn decode(body: &'a [u8]) -> Result<View<'a>, String> {
        let mut at = Cursor { bytes: body, at: 0 };
        let frame = match at.u8("kind")? {
            HELLO => {
                let version = at.u32("version")?;
                let pid = at.u32("pid")?;
                let fork = at.u64("fork time")?;
                let count = at.u32("test count")?;
                let from = at.at;
                for _ in 0..count {
                    at.string("test name")?;
                }
                View::Hello { version, pid, fork, tests: Names { count, bytes: &body[from..at.at] } }
            }
            ENDED => {
                let id = at.u64("run id")?;
                let end = match at.u8("end kind")? {
                    0 => EndView::Exited(at.u32("exit code")? as i32),
                    1 => EndView::Signaled(at.u32("signal")? as i32),
                    2 => EndView::NotStarted(at.string("reason it did not start")?),
                    other => return Err(format!("no end is of kind {}", other)), // alloc: error path
                };
                let output = at.bytes("output")?;
                let executed = match at.u8("execution record kind")? {
                    0 => {
                        let slots = at.u32("slot count")?;
                        let count = at.u32("hit count")? as usize;
                        let hits = "slots hit";
                        let len = count.checked_mul(4).ok_or_else(|| format!("a frame ends early, in its {}", hits))?; // alloc: error path
                        let bytes = at.take(len, hits)?;
                        let hits = Hits(bytes);
                        let mut last: Option<u32> = None;
                        for hit in hits.iter() {
                            if hit >= slots || last.is_some_and(|it| it >= hit) {
                                return Err(format!( // alloc: error path
                                    "an execution record names slot {} out of order or past its {} slots",
                                    hit, slots
                                ));
                            }
                            last = Some(hit);
                        }
                        ExecView::Hits { slots, hits }
                    }
                    1 => ExecView::Not(at.string("reason it has no execution record")?),
                    other => return Err(format!("no execution record is of kind {}", other)), // alloc: error path
                };
                View::Ended { id, end, output, executed }
            }
            RUN => {
                let id = at.u64("run id")?;
                View::Run { id, index: at.u32("test index")? }
            }
            KILL => View::Kill { id: at.u64("run id")? },
            QUIT => View::Quit,
            REPLICATE => View::Replicate { count: at.u32("replica count")? },
            other => return Err(format!("no frame is of kind {}", other)), // alloc: error path
        };
        match at.at == body.len() {
            true => Ok(frame),
            false => Err(format!("a frame has {} bytes after its end", body.len() - at.at)), // alloc: error path
        }
    }

    pub fn to_frame(self) -> Frame {
        match self {
            View::Hello { version, pid, fork, tests } => {
                let mut names = Vec::with_capacity(tests.len());
                names.extend(tests.iter().map(str::to_owned));
                Frame::Hello { version, pid, fork, tests: names }
            }
            View::Ended { id, end, output, executed } => {
                Frame::Ended { id, end: end.to_end(), output: output.to_vec(), executed: executed.to_executed() } // alloc: the owned frame: what a result keeps
            }
            View::Run { id, index } => Frame::Run { id, index },
            View::Kill { id } => Frame::Kill { id },
            View::Quit => Frame::Quit,
            View::Replicate { count } => Frame::Replicate { count },
        }
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

/// Each read takes the name of the field it reads (`what`), which a frame too short for it is
/// an error naming.
impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize, what: &str) -> Result<&'a [u8], String> {
        if self.bytes.len() - self.at < len {
            return Err(format!("a frame ends early, in its {}", what)); // alloc: error path
        }
        let taken = &self.bytes[self.at..self.at + len];
        self.at += len;
        Ok(taken)
    }

    fn u8(&mut self, what: &str) -> Result<u8, String> {
        Ok(self.take(1, what)?[0])
    }

    fn u32(&mut self, what: &str) -> Result<u32, String> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self, what: &str) -> Result<u64, String> {
        let b = self.take(8, what)?;
        Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }

    fn bytes(&mut self, what: &str) -> Result<&'a [u8], String> {
        let len = self.u32(what)? as usize;
        self.take(len, what)
    }

    fn string(&mut self, what: &str) -> Result<&'a str, String> {
        let bytes = self.bytes(what)?;
        match ::std::str::from_utf8(bytes) {
            Ok(text) => Ok(text),
            Err(_) => Err(format!("the {} of a frame is not UTF-8", what)), // alloc: error path
        }
    }
}

/// Frames out of a byte stream that comes in pieces. The frames read are skipped over, not
/// moved out: what still waits moves to the front only where a push would otherwise grow
/// the buffer, so many frames in one piece cost one pass, not one move of the rest per frame.
/// The buffer is kept across frames, so a steady stream allocates nothing.
#[derive(Default, Debug)]
pub struct Frames {
    buffer: Vec<u8>,
    /// Where the bytes not yet read as a frame start.
    start: usize,
}

impl Frames {
    pub fn new() -> Frames {
        Frames { buffer: Vec::new(), start: 0 } // alloc: empty, allocates nothing
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.start == self.buffer.len() {
            self.buffer.clear();
            self.start = 0;
        } else if self.start > 0 && self.buffer.len() + bytes.len() > self.buffer.capacity() {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
        self.buffer.extend_from_slice(bytes);
    }

    /// [`Frames::push`] of bytes the caller owns: taken as the buffer, with no copy, when
    /// nothing is waiting.
    pub fn push_owned(&mut self, bytes: Vec<u8>) {
        match self.waiting() {
            0 => {
                self.buffer = bytes;
                self.start = 0;
            }
            _ => self.push(&bytes),
        }
    }

    /// The next whole frame, read in place, `None` until one has come. An error is for good:
    /// the stream is not this protocol's.
    pub fn pop_view<'a>(&'a mut self) -> Result<Option<View<'a>>, String> {
        if self.start == self.buffer.len() {
            self.buffer.clear();
            self.start = 0;
        }
        let rest = &self.buffer[self.start..];
        if rest.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
        if len > LONGEST {
            return Err(format!("a frame of {} bytes, past the most of {}", len, LONGEST)); // alloc: error path
        }
        let len = len as usize;
        if rest.len() < 4 + len {
            return Ok(None);
        }
        let at = self.start + 4;
        self.start = at + len;
        View::decode(&self.buffer[at..at + len]).map(Some)
    }

    /// [`Frames::pop_view`], owned.
    pub fn pop(&mut self) -> Result<Option<Frame>, String> {
        Ok(self.pop_view()?.map(View::to_frame))
    }

    /// Bytes of a frame not yet whole.
    pub fn waiting(&self) -> usize {
        self.buffer.len() - self.start
    }

    /// How many more bytes the frame being received needs before it is whole (its length's
    /// bytes first): what one read can ask for, so a long frame comes in one read rather than
    /// many small ones. Never more than a frame can be.
    pub fn needed(&self) -> usize {
        let rest = &self.buffer[self.start..];
        if rest.len() < 4 {
            return 4 - rest.len();
        }
        let len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]).min(LONGEST) as usize;
        (4 + len).saturating_sub(rest.len())
    }

    /// The bytes the buffer has room for.
    pub fn capacity(&self) -> usize {
        self.buffer.capacity()
    }
}
