//! `SpawnAnswer`: what the sandbox gave back for one request, as a canonical record (READINESS.md
//! 1a, S1; 1d's "outputs" row: files sorted by path, the read list sorted and once).

use core::cmp::Ordering;
use core::fmt;

use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc};

use crate::{Malformed, Put, Take, guest_path_ok};

/// The answer record's layout word.
const LAYOUT: u64 = u64::from_le_bytes(*b"bzspawn1");

/// The digest's domain.
const DOMAIN: &str = "blaze v2 spawn answer 1";

/// How the program ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    Code(i32),
    Signal(u8),
}

/// One row of the read list: a file read (by content), a path looked for and absent, a folder
/// listed (by the digest of its sorted names).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seen<'a> {
    Read { path: AString<'a>, digest: ContentId },
    Miss { path: AString<'a> },
    List { path: AString<'a>, digest: ContentId },
}

impl Seen<'_> {
    /// The guest path the row is about.
    pub fn path(&self) -> &str {
        match self {
            Seen::Read { path, .. } | Seen::Miss { path } | Seen::List { path, .. } => path.as_str(),
        }
    }

    /// The row's kind, its tag in the encoding and its second sort key.
    fn kind(&self) -> u8 {
        match self {
            Seen::Read { .. } => 1,
            Seen::Miss { .. } => 2,
            Seen::List { .. } => 3,
        }
    }

    fn order(&self, other: &Seen<'_>) -> Ordering {
        self.path().cmp(other.path()).then(self.kind().cmp(&other.kind()))
    }
}

/// A file the program left in a writable folder: its bytes, or `None` when it removed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Written<'a> {
    pub path: AString<'a>,
    pub bytes: Option<AVec<'a, u8>>,
}

/// The sandbox's answer to one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnAnswer<'a> {
    pub status: Status,
    pub stdout: AVec<'a, u8>,
    pub stderr: AVec<'a, u8>,
    /// Sorted by path, each path once.
    pub written: AVec<'a, Written<'a>>,
    /// Sorted by (path, kind), each row once.
    pub seen: AVec<'a, Seen<'a>>,
}

/// Why an answer was not made. Each names the path it refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnswerError<'a> {
    /// Two written files have one path.
    WrittenTwice { path: AString<'a> },
    /// Two read-list rows of one path and kind disagree (a file read as two contents).
    SeenTwice { path: AString<'a> },
    /// A written file's path is not absolute, or holds `..` or a NUL.
    Path { path: AString<'a> },
}

impl fmt::Display for AnswerError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnswerError::WrittenTwice { path } => {
                write!(f, "`{path}`: a spawn answer writes it twice (one result per path)")
            }
            AnswerError::SeenTwice { path } => {
                write!(f, "`{path}`: a spawn answer's read list gives two different results for it")
            }
            AnswerError::Path { path } => {
                write!(f, "`{path}`: a spawn answer's written file must be an absolute guest path without `..` or NUL")
            }
        }
    }
}

impl core::error::Error for AnswerError<'_> {}

fn strictly_sorted_written(written: &[Written<'_>]) -> bool {
    written.windows(2).all(|pair| pair[0].path < pair[1].path)
}

fn strictly_sorted_seen(seen: &[Seen<'_>]) -> bool {
    seen.windows(2).all(|pair| pair[0].order(&pair[1]) == Ordering::Less)
}

impl<'a> SpawnAnswer<'a> {
    /// A canonical answer: `written` sorted by path (a path twice refused), `seen` sorted by
    /// (path, kind) with exact repeats removed (two rows of one path and kind that disagree
    /// refused).
    pub fn new(
        status: Status,
        stdout: AVec<'a, u8>,
        stderr: AVec<'a, u8>,
        mut written: AVec<'a, Written<'a>>,
        mut seen: AVec<'a, Seen<'a>>,
    ) -> Result<SpawnAnswer<'a>, AnswerError<'a>> {
        if let Some(at) = written.iter().position(|file| !guest_path_ok(&file.path)) {
            return Err(AnswerError::Path { path: written.remove(at).path });
        }
        written.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        if let Some(twice) = written.windows(2).position(|pair| pair[0].path == pair[1].path) {
            return Err(AnswerError::WrittenTwice { path: written.remove(twice + 1).path });
        }
        seen.sort_unstable_by(|a, b| a.order(b));
        if let Some(twice) = seen.windows(2).position(|pair| pair[0].order(&pair[1]) == Ordering::Equal && pair[0] != pair[1])
        {
            let path = match seen.remove(twice + 1) {
                Seen::Read { path, .. } | Seen::Miss { path } | Seen::List { path, .. } => path,
            };
            return Err(AnswerError::SeenTwice { path });
        }
        seen.dedup();
        Ok(SpawnAnswer { status, stdout, stderr, written, seen })
    }

    /// The one encoding: the layout word, then every field in declared order.
    pub fn encode(&self, out: &mut AVec<'_, u8>) {
        let mut put = Put { out };
        put.u64(LAYOUT);
        match self.status {
            Status::Code(code) => {
                put.u8(1);
                put.i32(code);
            }
            Status::Signal(signal) => {
                put.u8(2);
                put.u8(signal);
            }
        }
        put.bytes(&self.stdout, "stdout");
        put.bytes(&self.stderr, "stderr");
        put.count(self.written.len(), "written");
        for file in self.written.iter() {
            put.str(&file.path, "written");
            match &file.bytes {
                None => put.u8(0),
                Some(bytes) => {
                    put.u8(1);
                    put.bytes(bytes, "written");
                }
            }
        }
        put.count(self.seen.len(), "seen");
        for row in self.seen.iter() {
            put.u8(row.kind());
            put.str(row.path(), "seen");
            match row {
                Seen::Read { digest, .. } | Seen::List { digest, .. } => put.id(digest),
                Seen::Miss { .. } => {}
            }
        }
    }

    /// The answer in `bytes`, refused unless canonical.
    pub fn decode<'b>(bytes: &'b [u8], alloc: Alloc<'b>) -> Result<SpawnAnswer<'b>, Malformed> {
        let mut take = Take::new(bytes);
        let layout = take.u64()?;
        if layout != LAYOUT {
            return Err(Malformed::Layout(layout));
        }
        let status = match take.u8()? {
            1 => Status::Code(take.i32()?),
            2 => Status::Signal(take.u8()?),
            _ => return Err(Malformed::NotCanonical { field: "status" }),
        };
        let stdout = take.byte_vec(alloc)?;
        let stderr = take.byte_vec(alloc)?;
        let mut written = AVec::new_in(alloc);
        for _ in 0..take.count()? {
            let path = take.string("written", alloc)?;
            let bytes = match take.u8()? {
                0 => None,
                1 => Some(take.byte_vec(alloc)?),
                _ => return Err(Malformed::NotCanonical { field: "written" }),
            };
            written.push(Written { path, bytes });
        }
        let mut seen = AVec::new_in(alloc);
        for _ in 0..take.count()? {
            let kind = take.u8()?;
            let path = take.string("seen", alloc)?;
            seen.push(match kind {
                1 => Seen::Read { path, digest: take.id()? },
                2 => Seen::Miss { path },
                3 => Seen::List { path, digest: take.id()? },
                _ => return Err(Malformed::NotCanonical { field: "seen" }),
            });
        }
        take.end()?;
        if !strictly_sorted_written(&written) {
            return Err(Malformed::Unsorted { field: "written" });
        }
        if !written.iter().all(|file| guest_path_ok(&file.path)) {
            return Err(Malformed::NotCanonical { field: "written" });
        }
        if !strictly_sorted_seen(&seen) {
            return Err(Malformed::Unsorted { field: "seen" });
        }
        Ok(SpawnAnswer { status, stdout, stderr, written, seen })
    }

    /// The answer's digest: its encoding under the domain `"blaze v2 spawn answer 1"`.
    pub fn digest(&self, alloc: Alloc<'_>) -> ContentId {
        let mut encoded = AVec::new_in(alloc);
        self.encode(&mut encoded);
        ContentId::of(DOMAIN, &encoded)
    }
}
