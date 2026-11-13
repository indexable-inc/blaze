//! A solo answer's rows, kept for the next daemon to take into its profile: the record, its
//! directory, and the daemon's take of them ([`Take`]).
//!
//! The design, in two lines: a command the client answered with no daemon ([`crate::solo`])
//! writes its spans as one file, `<state>/solo-rows/<pid>`, made whole under `<pid>.part` and
//! renamed into place (the thin `blaze`'s `kept::keep`, the one writer); the daemon takes every
//! whole file at its start and in its idle looks ([`Take`]), each a build of its own in the
//! history (`History::solo_build`), and removes it.
//!
//! Why a file a command and not one file appended to: a rename is the one commit a reader can
//! never see half of, so no lock is shared between the clients and the daemon and no reader
//! waits; a name a process (`pid`) bounds the directory by the system's process ids, and a
//! process whose id comes round again before any daemon took its predecessor's rows replaces
//! them: the loss is that one record, and only when no daemon started in a whole cycle of ids.
//!
//! The layout ([`Kept::write`]) is [`MAGIC`], then big-endian fields: the process (u32), its
//! start and end on the profile's clock (two i64), the build the slot's answer was certified
//! from (u64, 0 for none), the slot's key, the workspace's root and the command line (each a
//! u16 length and UTF-8), and the spans ([`put_timed`]). It is read whole or refused, naming
//! why ([`Kept::read`]); a file of the first layout (`blaze solo rows1`, never released) is
//! refused by its magic and removed as any file that is not a record.
//!
//! A sans-IO core (rule 14): bytes in, bytes out, and the take a machine over the daemon's
//! driver's effects, its clock and its look at whether a process lives handed in.

#![blaze::sans_io]

use std::collections::BTreeMap;

use blaze_api::Timed;
use blaze_core::{Completion, Done, Effect, FileName, Meta, Tag};

/// The directory in the daemon's state directory that holds the kept rows.
pub const DIR: &str = "solo-rows";

/// The first bytes of a kept file: its format's name and version.
pub const MAGIC: &[u8; 16] = b"blaze solo rows2";

/// The most spans a record carries: the client's fixed set is five.
pub const MOST_SPANS: usize = 64;

/// The most whole files one look takes; the rest are the next look's ([`Taken::more`]).
pub const MOST_TAKEN: usize = 256;

/// How long a `.part` whose process is gone is left before a look removes it, on the wall
/// clock: a writer makes its few hundred bytes whole in microseconds, so a `.part` this old
/// with no process is one whose writer died before its rename. The bound, not the process
/// alone, is what decides: a process of another pid namespace reads as gone to `kill(pid, 0)`
/// while it still writes.
pub const PART_GONE_AFTER_NS: u64 = 60_000_000_000;

/// The tags a [`Take`] uses, from its base: each look's steps count through them.
pub const TAGS: u64 = 1 << 32;

/// One solo answer's rows.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a test's record of what was taken
pub struct Kept {
    /// The command's process (a `run`'s program's too: `exec` keeps it).
    pub pid: u32,
    /// The kernel's start of the process and the command's end, on the profile's clock.
    pub started_ns: i64,
    pub ended_ns: i64,
    /// The build whose answer the slot holds (`Worked::build_id` in it), when it named one.
    pub certified: Option<u64>,
    /// The request slot's key, as its file is named (`{key:x}`).
    pub slot: String,
    /// The workspace's root, as the request named it (`builds.workspace`).
    pub workspace: String,
    /// The command and its words before the program's (`builds.command`: `run --release`).
    pub command: String,
    /// The command's spans, by name (the thin `blaze`'s `talk::Timings`).
    pub timed: Vec<Timed>,
}

impl Kept {
    /// The file's bytes. An error names the field too long for its length.
    pub fn write(&self) -> Result<Vec<u8>, String> {
        let texts = self.slot.len() + self.workspace.len() + self.command.len();
        // alloc: once a solo answer, the record's bytes
        let mut out = Vec::with_capacity(MAGIC.len() + 4 + 8 * 3 + 6 + texts + 2 + 40 * self.timed.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.pid.to_be_bytes());
        out.extend_from_slice(&self.started_ns.to_be_bytes());
        out.extend_from_slice(&self.ended_ns.to_be_bytes());
        out.extend_from_slice(&self.certified.unwrap_or(0).to_be_bytes());
        put_text(&mut out, "slot key", &self.slot)?;
        put_text(&mut out, "workspace", &self.workspace)?;
        put_text(&mut out, "command line", &self.command)?;
        put_timed(&mut out, &self.timed)?;
        Ok(out)
    }

    /// [`Kept::write`]'s bytes, read whole: an error names what is short, extra or not the layout.
    pub fn read(bytes: &[u8]) -> Result<Kept, String> {
        let rest = bytes
            .strip_prefix(MAGIC)
            .ok_or("it does not begin as a kept solo answer's rows begin (`blaze solo rows2`)")?;
        let (pid, rest) = take::<4>(rest, "the process")?;
        let (started_ns, rest) = take::<8>(rest, "the start")?;
        let (ended_ns, rest) = take::<8>(rest, "the end")?;
        let (certified, rest) = take::<8>(rest, "the certified build")?;
        let (slot, rest) = take_text(rest, "slot key")?;
        let (workspace, rest) = take_text(rest, "workspace")?;
        let (command, rest) = take_text(rest, "command line")?;
        let (timed, rest) = take_timed(rest)?;
        if !rest.is_empty() {
            return Err(format!("{} bytes follow its last span", rest.len()));
        }
        let certified = u64::from_be_bytes(certified);
        Ok(Kept {
            pid: u32::from_be_bytes(pid),
            started_ns: i64::from_be_bytes(started_ns),
            ended_ns: i64::from_be_bytes(ended_ns),
            certified: (certified != 0).then_some(certified),
            slot: slot.to_owned(),           // alloc: once a record read
            workspace: workspace.to_owned(), // alloc: once a record read
            command: command.to_owned(),     // alloc: once a record read
            timed,
        })
    }
}

/// The next `N` bytes of `rest`, `what` naming them where `rest` is shorter.
fn take<'a, const N: usize>(rest: &'a [u8], what: &str) -> Result<([u8; N], &'a [u8]), String> {
    let (head, rest) = rest.split_first_chunk::<N>().ok_or_else(|| format!("it ends before {what}"))?;
    Ok((*head, rest))
}

/// `text` appended as a u16 length and its UTF-8; an error names `what` where it is too long.
fn put_text(out: &mut Vec<u8>, what: &str, text: &str) -> Result<(), String> {
    let len = u16::try_from(text.len())
        .map_err(|_| format!("a {what} of {} bytes is longer than a kept record holds", text.len()))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(text.as_bytes());
    Ok(())
}

/// [`put_text`]'s text at the head of `rest`, and what follows it.
fn take_text<'a>(rest: &'a [u8], what: &str) -> Result<(&'a str, &'a [u8]), String> {
    let (len, rest) = take::<2>(rest, what)?;
    let len = u16::from_be_bytes(len) as usize;
    if rest.len() < len {
        return Err(format!("it holds {} bytes of a {what} of {len}", rest.len()));
    }
    let (text, rest) = rest.split_at(len);
    let text = std::str::from_utf8(text).map_err(|_| format!("its {what} is not UTF-8"))?;
    Ok((text, rest))
}

/// `timed` appended to `out`: a u16 count, then each span as a u16 name length, its UTF-8
/// name, and its start and end (two big-endian i64). An error names a list or a name too long.
/// The one encoding of a client's spans by name: the thin `blaze`'s handover writes them so too.
pub fn put_timed(out: &mut Vec<u8>, timed: &[Timed]) -> Result<(), String> {
    if timed.len() > MOST_SPANS {
        return Err(format!("{} spans, more than the {MOST_SPANS} a client's rows hold", timed.len()));
    }
    out.extend_from_slice(&(timed.len() as u16).to_be_bytes());
    for span in timed {
        let len = u16::try_from(span.name.len())
            .map_err(|_| format!("the span name `{}` is longer than a client's rows hold", span.name))?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(span.name.as_bytes());
        out.extend_from_slice(&span.start_ns.to_be_bytes());
        out.extend_from_slice(&span.end_ns.to_be_bytes());
    }
    Ok(())
}

/// [`put_timed`]'s spans at the head of `bytes`, and what follows them.
pub fn take_timed(bytes: &[u8]) -> Result<(Vec<Timed>, &[u8]), String> {
    let (count, mut rest) = take::<2>(bytes, "its spans' count")?;
    let count = u16::from_be_bytes(count) as usize;
    if count > MOST_SPANS {
        return Err(format!("it says {count} spans, more than the {MOST_SPANS} a client's rows hold"));
    }
    let mut timed = Vec::with_capacity(count); // alloc: a client's few spans, once a read
    for at in 0..count {
        let (len, after) = take::<2>(rest, "a span's name length")?;
        let len = u16::from_be_bytes(len) as usize;
        if after.len() < len {
            return Err(format!("span {at}'s name is cut: {} of {len} bytes", after.len()));
        }
        let (name, after) = after.split_at(len);
        let name = std::str::from_utf8(name).map_err(|_| format!("span {at}'s name is not UTF-8"))?;
        let (start_ns, after) = take::<8>(after, "a span's start")?;
        let (end_ns, after) = take::<8>(after, "a span's end")?;
        timed.push(Timed {
            name: name.to_owned(), // alloc: a span's name, a few a read
            start_ns: i64::from_be_bytes(start_ns),
            end_ns: i64::from_be_bytes(end_ns),
        });
        rest = after;
    }
    Ok((timed, rest))
}

/// The name of process `pid`'s kept file, and of the file it is made whole in first.
pub fn names(pid: u32) -> (String, String) {
    // alloc: two short names, once a solo answer
    (pid.to_string(), format!("{pid}.part"))
}

/// What a name in the directory is to a look.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Named {
    /// `<pid>`: a whole record, to take.
    Whole(u32),
    /// `<pid>.part`: one its writer is still making whole, or died making.
    Part(u32),
    /// Anything else: not a kept record's name.
    Foreign,
}

impl Named {
    /// What the last part of a listed name (`leaf`) is. A pid is decimal digits alone, so
    /// `+7` or `07.part` is foreign: the writer never names a file so.
    pub fn of(leaf: &str) -> Named {
        let pid = |digits: &str| {
            let plain = !digits.is_empty() && digits.bytes().all(|it| it.is_ascii_digit()) && !digits.starts_with('0');
            plain.then(|| digits.parse::<u32>().ok()).flatten()
        };
        match leaf.strip_suffix(".part") {
            Some(digits) => pid(digits).map_or(Named::Foreign, Named::Part),
            None => pid(leaf).map_or(Named::Foreign, Named::Whole),
        }
    }
}

/// What one look found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Taken {
    /// Every whole file's record, in the order of its process id, each removed before it is
    /// here: a file whose removal failed is in [`Taken::refused`] instead, so no look takes a
    /// record a later look takes again.
    pub kept: Vec<Kept>,
    /// Each file that is not taken, named, with why: a `.part` of a process that is gone, past
    /// [`PART_GONE_AFTER_NS`] (removed); a file that does not read as a record or is not a kept
    /// record's name (removed); one that could not be read or removed (left, named).
    pub refused: Vec<String>,
    /// More whole files than [`MOST_TAKEN`] were there: the rest are the next look's.
    pub more: bool,
    /// Why the look itself ended short: the directory could not be stated or listed.
    pub failed: Option<String>,
}

/// The daemon's take of the kept rows: a machine over its driver's effects, one look at a time
/// ([`Take::look`]): the directory stated (none: nothing kept), listed, each whole file read
/// and then removed, and its record given only once it is removed ([`Taken::kept`]); a `.part`
/// is never read.
#[derive(Debug)]
pub struct Take {
    base: u64,
    next: u64,
    look: Option<Look>,
}

#[derive(Debug)]
struct Look {
    dir: FileName,
    step: Step,
    taken: Taken,
}

#[derive(Debug)]
enum Step {
    Stat(Tag),
    List(Tag),
    Files(BTreeMap<Tag, File>),
}

#[derive(Debug)]
struct File {
    name: FileName,
    is: Is,
}

#[derive(Debug)]
enum Is {
    /// A whole file of `pid`, being read.
    Reading(u32),
    /// Read, and being removed: its record once removed, or why it is not one.
    Removing(Result<Kept, String>),
}

impl Take {
    /// A take whose effects are tagged from `base` on, through [`TAGS`] of them.
    pub fn new(base: u64) -> Take {
        Take { base, next: 0, look: None }
    }

    /// Whether `tag` is one of this take's.
    pub fn owns(&self, tag: Tag) -> bool {
        (self.base..self.base + TAGS).contains(&tag.0)
    }

    /// Whether a look is under way.
    pub fn looking(&self) -> bool {
        self.look.is_some()
    }

    fn tag(&mut self) -> Tag {
        let tag = Tag(self.base + self.next);
        self.next = (self.next + 1) % TAGS;
        tag
    }

    /// A look at `dir` begun, its first effect pushed to `effects`; none while one is under way
    /// (false then).
    pub fn look(&mut self, dir: FileName, effects: &mut Vec<Effect>) -> bool {
        if self.look.is_some() {
            return false;
        }
        let tag = self.tag();
        // clone: a refcount, the name the look keeps
        effects.push(Effect::Stat { file: dir.clone(), tag });
        self.look = Some(Look { dir, step: Step::Stat(tag), taken: Taken::default() });
        true
    }

    /// One of this take's effects completed: the look's next effects pushed to `effects`, and
    /// what it found once it ended. `now_ns` is the driver's wall clock, which a `.part`'s age
    /// is told by; `alive` says whether a process runs. A completion no step waits for is a
    /// refusal naming its tag.
    pub fn complete(
        &mut self,
        completion: Completion,
        now_ns: u64,
        alive: impl Fn(u32) -> bool,
        effects: &mut Vec<Effect>,
    ) -> Option<Taken> {
        let Completion { tag, done } = completion;
        let Some(mut look) = self.look.take() else {
            // alloc: a fault's message
            let refused = vec![format!("a completion of tag {} that no look of the solo rows waits for", tag.0)];
            return Some(Taken { refused, ..Taken::default() });
        };
        let ended = match std::mem::replace(&mut look.step, Step::Files(BTreeMap::new())) {
            Step::Stat(at) if at == tag => match done {
                Done::Stat(None) => true,
                Done::Stat(Some(_)) => {
                    let tag = self.tag();
                    // clone: a refcount
                    effects.push(Effect::List { dir: look.dir.clone(), tag });
                    look.step = Step::List(tag);
                    false
                }
                other => {
                    look.taken.failed = Some(format!("stating {}: {}", look.dir, failure(&other)));
                    true
                }
            },
            Step::List(at) if at == tag => match done {
                Done::Listed(listed) => {
                    let files = self.listed(&mut look, listed, now_ns, alive, effects);
                    let ended = files.is_empty();
                    look.step = Step::Files(files);
                    ended
                }
                other => {
                    look.taken.failed = Some(format!("listing {}: {}", look.dir, failure(&other)));
                    true
                }
            },
            Step::Files(mut files) => {
                match files.remove(&tag) {
                    Some(file) => self.file(&mut look.taken, &mut files, file, done, effects),
                    None => look.taken.refused.push(format!(
                        "a completion of tag {} that no step of the look at {} waits for",
                        tag.0, look.dir
                    )),
                }
                let ended = files.is_empty();
                look.step = Step::Files(files);
                ended
            }
            step => {
                look.taken.refused.push(format!(
                    "a completion of tag {} while the look at {} waits for another ({step:?})",
                    tag.0, look.dir
                ));
                look.step = step;
                false
            }
        };
        if !ended {
            self.look = Some(look);
            return None;
        }
        let mut taken = look.taken;
        taken.kept.sort_unstable_by_key(|it| it.pid);
        Some(taken)
    }

    /// The listing's names sorted: each whole file read (at most [`MOST_TAKEN`]), each foreign
    /// name and each `.part` past its bound with no process removed, refused by name.
    fn listed(
        &mut self,
        look: &mut Look,
        listed: Vec<(FileName, Meta)>,
        now_ns: u64,
        alive: impl Fn(u32) -> bool,
        effects: &mut Vec<Effect>,
    ) -> BTreeMap<Tag, File> {
        let mut files = BTreeMap::new();
        let mut whole = 0;
        for (name, meta) in listed {
            let leaf = name.as_str().rsplit('/').next().unwrap_or(name.as_str());
            if meta.dir {
                look.taken.refused.push(format!("{name}: a directory, which no writer of kept rows makes"));
                continue;
            }
            let why = match Named::of(leaf) {
                Named::Whole(pid) if whole < MOST_TAKEN => {
                    whole += 1;
                    let tag = self.tag();
                    // clone: a refcount
                    effects.push(Effect::ReadAll { file: name.clone(), tag });
                    files.insert(tag, File { name, is: Is::Reading(pid) });
                    continue;
                }
                Named::Whole(_) => {
                    look.taken.more = true;
                    continue;
                }
                Named::Part(pid) => {
                    let age = now_ns.saturating_sub(meta.modified_ns);
                    if age <= PART_GONE_AFTER_NS || alive(pid) {
                        continue;
                    }
                    format!("made by process {pid}, gone, and not made whole in {} s", age / 1_000_000_000)
                }
                Named::Foreign => "not a kept record's name".to_owned(), // alloc: a refusal
            };
            let tag = self.tag();
            // clone: a refcount
            effects.push(Effect::Remove { file: name.clone(), tag });
            files.insert(tag, File { name, is: Is::Removing(Err(why)) });
        }
        files
    }

    /// A file's step done: a read becomes its removal, a removal its record or its refusal.
    fn file(
        &mut self,
        taken: &mut Taken,
        files: &mut BTreeMap<Tag, File>,
        file: File,
        done: Done,
        effects: &mut Vec<Effect>,
    ) {
        let File { name, is } = file;
        match (is, done) {
            (Is::Reading(pid), Done::Read(bytes)) => {
                let record = match Kept::read(&bytes) {
                    Ok(kept) if kept.pid == pid => Ok(kept),
                    Ok(kept) => Err(format!("the record of process {}", kept.pid)),
                    Err(why) => Err(why),
                };
                let tag = self.tag();
                // clone: a refcount
                effects.push(Effect::Remove { file: name.clone(), tag });
                files.insert(tag, File { name, is: Is::Removing(record) });
            }
            (Is::Reading(_), other) => {
                taken.refused.push(format!("{name}: reading it: {}; left for the next look", failure(&other)));
            }
            (Is::Removing(Ok(kept)), Done::Removed) => taken.kept.push(kept),
            (Is::Removing(Err(why)), Done::Removed) => taken.refused.push(format!("{name}: {why}; removed")),
            (Is::Removing(record), other) => {
                let what = match record {
                    Ok(_) => "a whole record, not taken".to_owned(), // alloc: a refusal
                    Err(why) => why,
                };
                taken.refused.push(format!("{name}: {what}; removing it: {}", failure(&other)));
            }
        }
    }
}

/// What a completion that is not the one its step asked for says.
fn failure(done: &Done) -> String {
    match done {
        Done::Failed(error) => error.to_string(),
        other => format!("the driver answered {other:?}"),
    }
}

#[cfg(test)]
mod tests;
