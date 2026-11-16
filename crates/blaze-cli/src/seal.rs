//! A checkout's pointer files, `.blaze/v2/at/<name>`: one per (tool, request shape), its name 16
//! hex digits of H(tool, shape), at most `pointers::KEPT` kept (blaze-drive pointers.rs says
//! why, names them and says which go; read-addressed.md, "End state: what an edit costs",
//! piece 1). Each is the drive's last-answer pointer and the leaf table in one file, written in
//! place (no temporary file, no rename) and sealed by a header that holds the body's length
//! and a check over the length and the body. (Before 2026-11-16: one file, `.blaze/v2/last`,
//! so a `test` between two `check`s, or a second build of blaze, took the pointer the next
//! `check` would have walked forward from. A file left there by an older build is not read.)
//!
//! Why this form: a write-then-rename of each of the two files cost about 260 µs a file, a
//! quarter of an edit's time in main. One file written in place is one truncate and one write.
//! A crash during the write leaves a file whose check fails: it reads as ABSENT (one line on
//! stderr, then the request answers in full and writes it anew), never as an error that stops
//! the command and never as valid. Absent is always safe: the pointer is derived data, and a
//! request with none walks in full against the store's records.
//!
//! ```text
//! magic "BCS1" | len: u32 | check: [u8; 16] = H(len, body)[..16] | body: [u8; len]
//! body = last's length: u32 | last (the drive's BDL form) | the leaf table (BCL form)
//! ```

use core::fmt;

use blaze_drive::key::Tool;
use blaze_drive::pointers::{self, PointerName};
use blaze_ids::ContentId;
use bstd::cx::Cx;
use bstd::fs::{Dir, Durable, FsKind, ReadWrite};
use bstd::mem::Frame;
use bstd::path::Path;
use blaze_own::Vec;

use crate::CliError;

/// The pointer file's form.
pub(crate) const SEAL_MAGIC: &[u8; 4] = b"BCS1";
/// What a note or an error calls a pointer file.
pub(crate) const PATH: &str = "a pointer in .blaze/v2/at";
const HEADER: usize = 4 + 4 + 16;
const AT_LEN: usize = pointers::FOLDER.len() + 1 + 16;

/// One request's pointer file: `tool`'s, for requests of `shape`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct At {
    name: PointerName,
    text: [u8; AT_LEN],
}

impl At {
    pub(crate) fn of(tool: Tool, shape: &ContentId) -> At {
        At::named(PointerName::of(tool, shape))
    }

    fn named(name: PointerName) -> At {
        let mut text = [b'/'; AT_LEN];
        text[..pointers::FOLDER.len()].copy_from_slice(pointers::FOLDER.as_bytes());
        text[pointers::FOLDER.len() + 1..].copy_from_slice(name.bytes());
        At { name, text }
    }

    /// The file's path, checkout-relative.
    pub(crate) fn path(&self) -> Result<Path, CliError> {
        let text = core::str::from_utf8(&self.text).map_err(|_| CliError::Text { what: "a pointer file's name is not UTF-8" })?;
        Ok(Path::new(text)?)
    }
}
const CHECK_DOMAIN: &str = "blaze-cli seal check 1";

/// A bug the seal's test plants, to show the test catches it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The reader takes the header's length without comparing the check.
    CheckIgnored,
}

/// Why a pointer file of this form is not whole: a write that a crash cut short.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Torn {
    /// The file's length, and the length its header claims (0 when no header).
    pub len: u32,
    pub claims: u32,
}

impl fmt::Display for Torn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{PATH} is torn ({} bytes, its header claims a {}-byte body, its check fails): a write a crash cut short; answering in full and writing it anew",
            self.len, self.claims
        )
    }
}

/// The length is in the check for a reader's sake, not for safety: the body is sliced by it, so a
/// check of the body alone already fails on a wrong length (a planted check without the length
/// was tried and no crash told it apart; it is not kept as a control).
fn check(len: u32, body: &[u8]) -> [u8; 16] {
    let mut h = blaze_ids::Hasher::new(CHECK_DOMAIN);
    h.update(&len.to_le_bytes());
    h.update(body);
    let id: ContentId = h.finish();
    let mut out = [0; 16];
    out.copy_from_slice(&id.0[..16]);
    out
}

/// The sealed bytes of `last` and `leaves`.
pub(crate) fn seal<'f>(last: &[u8], leaves: &[u8], frame: &'f Frame) -> Result<Vec<u8, &'f Frame>, CliError> {
    let too_big = |_| CliError::Text { what: "the checkout's pointer file past 4 GiB" };
    let last_len = u32::try_from(last.len()).map_err(too_big)?;
    let len = u32::try_from(4 + last.len() + leaves.len()).map_err(too_big)?;
    let mut bytes = Vec::with_capacity_in(HEADER + len as usize, frame);
    bytes.extend_from_slice(SEAL_MAGIC);
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(&[0; 16]);
    bytes.extend_from_slice(&last_len.to_le_bytes());
    bytes.extend_from_slice(last);
    bytes.extend_from_slice(leaves);
    let sum = check(len, &bytes[HEADER..]);
    bytes[8..HEADER].copy_from_slice(&sum);
    Ok(bytes)
}

/// The body of sealed `bytes` (of this form: the caller checked the magic), split into the last
/// pointer and the leaf table; [`Torn`] when the header or the check says the write was cut.
pub(crate) fn open(bytes: &[u8], plant: Plant) -> Result<(&[u8], &[u8]), Torn> {
    let file_len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    let Some(header) = bytes.get(..HEADER) else { return Err(Torn { len: file_len, claims: 0 }) };
    let claims = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let torn = Torn { len: file_len, claims };
    let body = bytes.get(HEADER..HEADER + claims as usize).ok_or(torn)?;
    if plant != Plant::CheckIgnored && check(claims, body) != header[8..HEADER] {
        return Err(torn);
    }
    let last_len = body.first_chunk::<4>().map(|word| u32::from_le_bytes(*word) as usize).ok_or(torn)?;
    let last = body.get(4..4 + last_len).ok_or(torn)?;
    Ok((last, &body[4 + last_len..]))
}

/// Write the sealed `bytes` at the pointer `at` in place: one truncate, one write, nothing
/// synced. `fresh`: no pointer was at `at` when the request read it, so the folder grows by
/// one: first the pointers past `pointers::KEPT` go ([`evict`]). A request that found its
/// pointer writes it alone: no listing, no stat.
pub(crate) fn write(cx: &Cx, root: &Dir<ReadWrite>, at: &At, fresh: bool, bytes: &[u8], frame: &Frame) -> Result<(), CliError> {
    if fresh {
        evict(cx, root, at, frame)?;
    }
    let file = root.create(cx, &at.path()?)?;
    file.append(cx, bytes, Durable::No)?;
    file.close(cx, Durable::No)?;
    Ok(())
}

/// Remove the pointers `pointers::evicted` names, `at` about to be written: one listing of the
/// folder, and one stat effect for the pointers' modified times only when `KEPT` or more are
/// there. An entry whose name is no pointer's is left alone. A pointer another command removed
/// first (two commands in one checkout) is gone, which is what was wanted.
fn evict(cx: &Cx, root: &Dir<ReadWrite>, at: &At, frame: &Frame) -> Result<(), CliError> {
    let Some(entries) = root.entries(cx, &Path::new(pointers::FOLDER)?)? else { return Ok(()) };
    let mut held: Vec<(PointerName, u64), &Frame> = Vec::new_in(frame);
    let mut paths: Vec<Path, &Frame> = Vec::new_in(frame);
    for i in 0..entries.len() {
        if let Some(name) = PointerName::parse(entries.name(i)) {
            held.push((name, 0));
            paths.push(At::named(name).path()?);
        }
    }
    if held.len() < pointers::KEPT {
        return Ok(());
    }
    let looks = root.looks(cx, paths.iter())?;
    for (slot, look) in held.iter_mut().zip(looks.iter()) {
        slot.1 = look.modified_ns;
    }
    let erased = blaze_own::Erased::new(frame);
    let mut gone = blaze_own::AVec::new_in(erased.handle());
    pointers::evicted(held.as_mut_slice(), at.name, pointers::Plant::None, &mut gone);
    for name in gone.iter() {
        match root.remove(cx, &At::named(*name).path()?, Durable::No) {
            Ok(()) => {}
            Err(error) if error.kind == FsKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use blaze_sim::{Broke, Config, Tape, check as sim_check, control};
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::test::BUDGET;

    use super::*;

    /// The folder's glue (which pointers go is blaze-drive's `pointers::evicted`, proven in its
    /// sim): with `KEPT` pointers there, a fresh one's write removes exactly one of them,
    /// leaves a file that is no pointer alone and leaves itself; a write that is not fresh
    /// removes nothing. Without the `evict` call in `write` the folder holds `KEPT + 2`.
    #[test]
    fn a_fresh_pointer_past_the_cap_removes_one() {
        let io = Io::simulated(SimWorld::new(2, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
        let at = |n: u8| At::of(Tool(ContentId([n; 32])), &ContentId([7; 32]));
        let count = || root.entries(&cx, &Path::new(pointers::FOLDER).expect("a path")).expect("listed").expect("a folder");
        for n in 0..pointers::KEPT as u8 {
            write(&cx, &root, &at(n), true, b"pointer", frame).expect("written");
        }
        let stray = Path::new(".blaze/v2/at/notes.txt").expect("a path");
        root.create(&cx, &stray).expect("made").close(&cx, Durable::No).expect("closed");
        assert_eq!(count().len(), pointers::KEPT + 1);
        write(&cx, &root, &at(0), false, b"pointer again", frame).expect("written");
        assert_eq!(count().len(), pointers::KEPT + 1, "a pointer written over itself removed another");
        write(&cx, &root, &at(100), true, b"pointer", frame).expect("written");
        let after = count();
        assert_eq!(after.len(), pointers::KEPT + 1, "KEPT pointers and the stray file");
        let name = at(100).name;
        assert!(after.find(core::str::from_utf8(name.bytes()).expect("hex")).is_some(), "the pointer written is not there");
        assert!(after.find("notes.txt").is_some(), "a file that is no pointer was removed");
    }

    fn draw(tape: &Tape, what: &'static str) -> (std::vec::Vec<u8>, std::vec::Vec<u8>) {
        let last = tape.bytes(0..=40, what);
        let leaves = tape.bytes(0..=80, what);
        (last, leaves)
    }

    /// Every file a crash can leave while new sealed bytes are written over old ones: a write
    /// cut after any byte, (a) after the truncate, as `write` does, and (b) in place over the old
    /// bytes, and (c) with any subset of its 4-byte words reaching the disk, the file as long as
    /// either version. Each reads as whole-old, whole-new or absent.
    fn crashes(tape: &Tape, plant: Plant) -> Result<(), Broke> {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).map_err(|e| Broke::Harness(std::format!("{e}")))?;
        let frame = cx.frame();
        let (old_last, old_leaves) = draw(tape, "old");
        let (new_last, new_leaves) = draw(tape, "new");
        let harness = |e: CliError| Broke::Harness(std::format!("{e}"));
        let old = seal(&old_last, &old_leaves, frame).map_err(harness)?.as_slice().to_vec();
        let new = seal(&new_last, &new_leaves, frame).map_err(harness)?.as_slice().to_vec();
        let whole = |bytes: &[u8], what: &str| -> Result<(), Broke> {
            match open(bytes, plant) {
                Err(_) => Ok(()),
                Ok((last, leaves)) if (last, leaves) == (&old_last[..], &old_leaves[..]) => Ok(()),
                Ok((last, leaves)) if (last, leaves) == (&new_last[..], &new_leaves[..]) => Ok(()),
                Ok(_) => Err(Broke::Oracle(std::format!("a crash {what} left a pointer read as neither old nor new"))),
            }
        };
        if open(&new, plant).map(|(a, b)| (a.to_vec(), b.to_vec())) != Ok((new_last.clone(), new_leaves.clone())) {
            return Err(Broke::Oracle(std::string::String::from("a whole write does not read back")));
        }
        for cut in 0..=new.len() {
            whole(&new[..cut], "after the truncate")?;
            let mut over = old.clone();
            over.resize(old.len().max(cut), 0);
            over[..cut].copy_from_slice(&new[..cut]);
            whole(&over, "in place")?;
        }
        let len = if tape.one_in(2, "torn length") { old.len() } else { new.len() };
        let mut torn = old.clone();
        torn.resize(len, 0);
        for (word, chunk) in new.chunks(4).enumerate() {
            if tape.one_in(2, "word reached the disk") {
                for (k, byte) in chunk.iter().enumerate() {
                    if let Some(slot) = torn.get_mut(word * 4 + k) {
                        *slot = *byte;
                    }
                }
            }
        }
        whole(&torn, "with words torn")
    }

    #[test]
    fn a_crash_at_any_point_reads_whole_old_whole_new_or_absent() {
        sim_check(Config::cases(500), |tape| crashes(tape, Plant::None));
    }

    #[test]
    fn the_oracle_catches_an_ignored_check() {
        control(Config::cases(2000), Plant::None, Plant::CheckIgnored, "neither old nor new", crashes);
    }
}
