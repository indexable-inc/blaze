//! The patch protocol: what the launcher and the daemon say to each other over the launcher's
//! Unix socket (docs/v2/plan/design/launcher.md, section 2; rule `no-mcp`).
//!
//! One versioned schema ([`SCHEMA`]) in CBOR's core deterministic encoding, framed as
//! blaze-protocol frames its own: a four-byte big-endian length, then one item, the message
//! `{id, body, schema}`. A body is a map of the variant's name to its fields, a struct a map
//! keyed by its field names (blaze-protocol's shapes for serde), so the daemon may read these
//! with a derive and a person with any CBOR tool. One value has one encoding: the keys of a
//! map come in the order of their encodings, which this module writes by hand and reads by
//! expecting exactly that order. Anything else is refused, naming the first offence and its
//! offset: a schema that is not this one, an unknown variant or key, an integer not in its
//! shortest head, bytes left over.
//!
//! [`Ask`] is what the launcher says, [`Tell`] what the daemon says. Neither side waits for a
//! reply by position: `id` ties an answer to what it answers (an [`Ask::Applied`] carries the
//! id of its [`Tell::Edit`]).
//!
//! Nothing here allocates on its own or does IO: a message is written onto the end of the
//! caller's buffer and read in place, borrowing its text, byte strings and lists
//! ([`List`]) from the frame.

pub mod cbor;

use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;

use cbor::{Reader, Writer};

use crate::{CodeId, Slot, Thread, Tier, Txn};

/// The version of the schema of every body. A reader of another refuses the message by name
/// rather than guess at its fields; a launcher and a daemon of two schemas restart the
/// program (launcher.md, section 4).
pub const SCHEMA: u32 = 3;

/// The longest item a frame carries, as blaze-protocol's.
pub const LONGEST_FRAME: u32 = 16 << 20;

/// How deep a body's items nest at most: a list of structs inside a variant inside a message.
const DEPTH: u32 = 8;

/// Why bytes are not a message of this schema: what was wrong, and at which byte of the item.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WireError {
    /// The item ends before what its heads promise.
    Short { at: usize },
    /// An integer or a length not in the shortest head that holds it.
    NotShortest { at: usize },
    /// Another kind of item than the schema has here.
    Type { at: usize, want: &'static str },
    /// Another key than the schema has next in this map.
    Key { at: usize, want: &'static str },
    /// A variant the schema does not have.
    Variant { at: usize },
    /// Text that is not UTF-8.
    Utf8 { at: usize },
    /// A value the field cannot hold.
    Range { at: usize, what: &'static str },
    /// Bytes after the item.
    Left { at: usize },
    /// A message of another schema.
    Schema { said: u64 },
    /// An item longer than a frame carries.
    Long { len: u64 },
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            WireError::Short { at } => write!(f, "the message ends early (byte {at})"),
            WireError::NotShortest { at } => {
                write!(f, "byte {at}: a number not in its shortest encoding")
            }
            WireError::Type { at, want } => write!(f, "byte {at}: not {want}"),
            WireError::Key { at, want } => write!(f, "byte {at}: the key here is `{want}`"),
            WireError::Variant { at } => write!(f, "byte {at}: a variant this schema has not"),
            WireError::Utf8 { at } => write!(f, "byte {at}: text that is not UTF-8"),
            WireError::Range { at, what } => write!(f, "byte {at}: {what}"),
            WireError::Left { at } => write!(f, "byte {at}: bytes after the message"),
            WireError::Schema { said } => write!(
                f,
                "a message of schema {said}; this launcher protocol is schema {SCHEMA}"
            ),
            WireError::Long { len } => {
                write!(f, "a message of {len} bytes, longer than {LONGEST_FRAME}")
            }
        }
    }
}

impl core::error::Error for WireError {}

/// The content id of a code record in the store: what the launcher reads, and what the bytes
/// it placed must hash to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RecordId(pub [u8; 32]);

/// The program a launcher was started for: the id in the sidecar beside its executable.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ProgramId(pub [u8; 32]);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Arch {
    X86_64,
    Aarch64,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Os {
    Linux,
    Macos,
}

/// One function's new code in a [`Tell::Edit`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Bind<'a> {
    pub slot: Slot,
    /// The function's code record ([`crate::record`]), which a [`Tell::Record`] gave before.
    pub record: RecordId,
    /// The function's code identity: what a test's result is keyed by.
    pub code: CodeId,
    pub tier: Tier,
    /// The code's length, placed.
    pub len: u64,
    /// Its alignment: a power of two.
    pub align: u32,
    /// What each function and static the record names is in this process, in the order of
    /// the record's places ([`crate::record::Place`]): a function's slot, a static's number
    /// (a [`StaticDef::number`] of this or an earlier message). A place that is an outside
    /// symbol takes none: the record names it.
    pub refs: List<'a, u32>,
}

/// One static's definition in a [`Tell::Statics`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct StaticDef<'a> {
    /// The static's number in this process, which a [`Bind::refs`] or another static's
    /// `refs` names it by: given out by the daemon, each once.
    pub number: u32,
    /// The static's record ([`crate::record`]), which a [`Tell::Record`] gave before.
    pub record: RecordId,
    /// As [`Bind::refs`], for the places of the static's record.
    pub refs: List<'a, u32>,
}

/// What a list of a message holds: an item written and read as one CBOR item, which may
/// borrow from the frame it is read out of.
pub trait Listed<'a>: Copy + PartialEq + fmt::Debug {
    fn write<A: Allocator>(&self, to: &mut Writer<'_, A>);
    fn read(from: &mut Reader<'a>) -> Result<Self, WireError>;
}

/// A list in a message: a slice, for the side that writes it, or the items where they lie
/// in a frame, for the side that read it. The two compare equal when their items do.
#[derive(Clone, Copy)]
pub struct List<'a, T> {
    repr: Repr<'a, T>,
}

#[derive(Clone, Copy)]
enum Repr<'a, T> {
    Slice(&'a [T]),
    /// `count` items, each read once without error when the list was ([`List::read`]).
    Wire { items: &'a [u8], count: usize },
}

impl<'a, T: Listed<'a>> List<'a, T> {
    pub const fn of(items: &'a [T]) -> List<'a, T> {
        List { repr: Repr::Slice(items) }
    }

    pub fn len(&self) -> usize {
        match self.repr {
            Repr::Slice(items) => items.len(),
            Repr::Wire { count, .. } => count,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn iter(&self) -> ListIter<'a, T> {
        match self.repr {
            Repr::Slice(items) => ListIter::Slice(items.iter()),
            Repr::Wire { items, count } => {
                ListIter::Wire { from: Reader::new(items), left: count }
            }
        }
    }

    pub(crate) fn write<A: Allocator>(&self, to: &mut Writer<'_, A>) {
        to.array(self.len());
        for item in self.iter() {
            item.write(to);
        }
    }

    pub(crate) fn read(from: &mut Reader<'a>) -> Result<List<'a, T>, WireError> {
        let count = from.array()?;
        let start = from.at();
        for _ in 0..count {
            T::read(from)?;
        }
        Ok(List { repr: Repr::Wire { items: from.since(start), count } })
    }
}

impl<'a, T: Listed<'a>> PartialEq for List<'a, T> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().zip(other.iter()).all(|(a, b)| a == b)
    }
}

impl<'a, T: Listed<'a>> fmt::Debug for List<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// The items of a [`List`].
pub enum ListIter<'a, T> {
    Slice(core::slice::Iter<'a, T>),
    Wire { from: Reader<'a>, left: usize },
}

impl<'a, T: Listed<'a>> Iterator for ListIter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        match self {
            ListIter::Slice(items) => items.next().copied(),
            ListIter::Wire { left: 0, .. } => None,
            ListIter::Wire { from, left } => {
                *left -= 1;
                // The invariant: `List::read` read every one of these items without error
                // from these same bytes, so this read gives the item.
                T::read(from).ok()
            }
        }
    }
}

impl Listed<'_> for Slot {
    fn write<A: Allocator>(&self, to: &mut Writer<'_, A>) {
        to.uint(u64::from(self.0));
    }

    fn read(from: &mut Reader<'_>) -> Result<Slot, WireError> {
        from.uint32("a slot past the 32 bits a table has").map(Slot)
    }
}

impl Listed<'_> for u32 {
    fn write<A: Allocator>(&self, to: &mut Writer<'_, A>) {
        to.uint(u64::from(*self));
    }

    fn read(from: &mut Reader<'_>) -> Result<u32, WireError> {
        from.uint32("a slot or a static's number past 32 bits")
    }
}

impl<'a> Listed<'a> for StaticDef<'a> {
    fn write<A: Allocator>(&self, to: &mut Writer<'_, A>) {
        to.map(3);
        to.text("refs");
        self.refs.write(to);
        to.text("number");
        to.uint(u64::from(self.number));
        to.text("record");
        to.bytes(&self.record.0);
    }

    fn read(from: &mut Reader<'a>) -> Result<StaticDef<'a>, WireError> {
        from.map(3, "a static's definition, a map of 3")?;
        from.key("refs")?;
        let refs = List::read(from)?;
        from.key("number")?;
        let number = from.uint32("a static's number past 32 bits")?;
        from.key("record")?;
        let record = RecordId(id(from, "a record id is 32 bytes")?);
        Ok(StaticDef { number, record, refs })
    }
}

impl<'a> Listed<'a> for Bind<'a> {
    fn write<A: Allocator>(&self, to: &mut Writer<'_, A>) {
        to.map(7);
        to.text("len");
        to.uint(self.len);
        to.text("code");
        to.bytes(&self.code.0);
        to.text("refs");
        self.refs.write(to);
        to.text("slot");
        self.slot.write(to);
        to.text("tier");
        to.text(tier_name(self.tier));
        to.text("align");
        to.uint(u64::from(self.align));
        to.text("record");
        to.bytes(&self.record.0);
    }

    fn read(from: &mut Reader<'a>) -> Result<Bind<'a>, WireError> {
        from.map(7, "a bind, a map of 7")?;
        from.key("len")?;
        let len = from.uint()?;
        from.key("code")?;
        let code = CodeId(id(from, "a code id is 32 bytes")?);
        from.key("refs")?;
        let refs = List::read(from)?;
        from.key("slot")?;
        let slot = Slot::read(from)?;
        from.key("tier")?;
        let tier = tier(from)?;
        from.key("align")?;
        let align = from.uint32("an alignment past 32 bits")?;
        from.key("record")?;
        let record = RecordId(id(from, "a record id is 32 bytes")?);
        Ok(Bind { slot, record, code, tier, len, align, refs })
    }
}

/// What the launcher says to the daemon.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Ask<'a> {
    /// The first message: which program this process is, and the target its code is for.
    /// `holds`: the check word of the start image this launcher holds already (read from the
    /// program's start file, `<program>.blaze-start`), or `None`. `running`: whether the
    /// program's `main` was already started from that image, so the launcher joins a daemon
    /// at a first call no image held, rather than waiting for its answer before it runs.
    /// Read from a map of 5 (a launcher before these two) as `None, false`.
    Hello {
        program: ProgramId,
        pid: u64,
        arch: Arch,
        os: Os,
        page: u32,
        holds: Option<[u8; 32]>,
        running: bool,
    },
    /// A stub fired: `thread` called `slot`, which has no code here, and waits for it. The
    /// answer is a [`Tell::Edit`] that binds the slot, or a [`Tell::Refused`].
    Need { slot: Slot, thread: Thread },
    /// The edit `txn` is committed: every call from the flag's store on runs it. `epoch` is
    /// the one the commit published.
    Applied { txn: Txn, epoch: u64 },
    /// The edit `txn` could not be staged and nothing of it is reachable: `why` names the
    /// record, the slot or the room that failed it.
    Failed { txn: Txn, why: &'a str },
    /// The answer to [`Tell::Probe`]: the probed slots a thread has a frame in.
    Frames { txn: Txn, held: List<'a, Slot> },
    /// A domain ended: one byte a slot, set where the domain entered the slot's body (its
    /// exact code dependency set), and the effect, when there was one, that a domain cannot
    /// hold and that ended it for a process of its own.
    Ended { domain: u64, marks: &'a [u8], needs: Option<&'a str> },
    /// The launcher starts the program again in place for [`Tell::Restart`] `txn`: the next
    /// message on a new connection is a [`Ask::Hello`] from the same pid.
    Restarting { txn: Txn },
}

/// What the daemon says to the launcher.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tell<'a> {
    /// The answer to [`Ask::Hello`]: how many slots the program has. The launcher declares
    /// them; nothing runs yet.
    Welcome { slots: u32 },
    /// Run the program: its statics are defined. `inits` are the statics of its initializer
    /// sections (`.init_array`, `__mod_init_func`), by number, each holding the address of a
    /// function the launcher calls first, in this order, as the system's loader would; then
    /// it calls `entry`, the slot of the C `main`.
    Start { entry: Slot, inits: List<'a, u32> },
    /// The bytes of a record ([`crate::record`]) that a later [`Bind`] or [`StaticDef`] names
    /// by `id`, their content id: the launcher keeps them until a message uses them. Records
    /// come over the socket, not from a directory of the store: a record is a few hundred
    /// bytes the daemon has in memory when it answers, and the launcher needs no reader of
    /// the store's packs.
    Record { id: RecordId, bytes: &'a [u8] },
    /// Define these statics: each gets its memory (all of them first, so they may point at
    /// one another), then its initial value. A static is defined once a process, before the
    /// first record that names it is bound.
    Statics { defs: List<'a, StaticDef<'a>> },
    /// Bind these bodies in one transaction, after declaring `declare` more slots. `together`
    /// is false when each body means what the one it replaces did (a first call, a promotion).
    Edit { txn: Txn, binds: List<'a, Bind<'a>>, declare: u32, together: bool },
    /// The function of `slot` does not compile: the program stops with `diagnostic`.
    Refused { slot: Slot, diagnostic: &'a str },
    /// Which of these slots' bodies does a thread have a frame in? Asked before an edit that
    /// is live only when none has (a signature's change).
    Probe { txn: Txn, slots: List<'a, Slot> },
    /// The edit `txn` cannot be applied to the running program, for `why`: start again in
    /// place. Its code is ready.
    Restart { txn: Txn, why: &'a str },
    /// The answer to an [`Ask::Hello`] for a program this daemon holds no code of, and why:
    /// the launcher stops, naming it.
    Unheld { why: &'a str },
    /// The answer to an [`Ask::Hello`] in one message, for a launcher that has declared and
    /// placed nothing: an image of placed code ([`crate::image`]), which is what a
    /// [`Tell::Welcome`], the [`Tell::Statics`], the first [`Tell::Edit`] and the
    /// [`Tell::Start`] of this build leave in a launcher's memory. The launcher copies it in,
    /// moves its addresses to its own and runs the program.
    Image { bytes: &'a [u8] },
}

impl<'a> Ask<'a> {
    /// Writes the message as one frame onto the end of `out`. A message too long for a frame
    /// leaves `out` as it was.
    pub fn encode<A: Allocator>(&self, id: u64, out: &mut Vec<u8, A>) -> Result<(), WireError> {
        frame(id, out, |to| match *self {
            Ask::Hello { program, pid, arch, os, page, holds, running } => {
                // The keys in the order of their encodings: by length, then by bytes.
                variant(to, "Hello", 7);
                to.text("os");
                to.text(match os {
                    Os::Linux => "Linux",
                    Os::Macos => "Macos",
                });
                to.text("pid");
                to.uint(pid);
                to.text("arch");
                to.text(match arch {
                    Arch::X86_64 => "X86_64",
                    Arch::Aarch64 => "Aarch64",
                });
                to.text("page");
                to.uint(u64::from(page));
                to.text("holds");
                match holds {
                    Some(check) => to.bytes(&check),
                    None => to.null(),
                }
                to.text("program");
                to.bytes(&program.0);
                to.text("running");
                to.bool(running);
            }
            Ask::Need { slot, thread } => {
                variant(to, "Need", 2);
                to.text("slot");
                slot.write(to);
                to.text("thread");
                to.uint(thread.0);
            }
            Ask::Applied { txn, epoch } => {
                variant(to, "Applied", 2);
                to.text("txn");
                to.uint(txn.0);
                to.text("epoch");
                to.uint(epoch);
            }
            Ask::Failed { txn, why } => {
                variant(to, "Failed", 2);
                to.text("txn");
                to.uint(txn.0);
                to.text("why");
                to.text(why);
            }
            Ask::Frames { txn, held } => {
                variant(to, "Frames", 2);
                to.text("txn");
                to.uint(txn.0);
                to.text("held");
                held.write(to);
            }
            Ask::Ended { domain, marks, needs } => {
                variant(to, "Ended", 3);
                to.text("marks");
                to.bytes(marks);
                to.text("needs");
                match needs {
                    Some(effect) => to.text(effect),
                    None => to.null(),
                }
                to.text("domain");
                to.uint(domain);
            }
            Ask::Restarting { txn } => {
                variant(to, "Restarting", 1);
                to.text("txn");
                to.uint(txn.0);
            }
        })
    }

    /// Reads the message of one frame's item: the id and the body.
    pub fn decode(item: &'a [u8]) -> Result<(u64, Ask<'a>), WireError> {
        let (id, mut from) = envelope(item)?;
        from.map(1, "a body, a map of one variant")?;
        let named = from.at();
        let ask = match from.text()? {
            "Hello" => {
                // A map of 7, or of 5 from a launcher that says neither `holds` nor `running`.
                let fields = match from.clone().map(7, "Hello, a map of 7") {
                    Ok(()) => 7,
                    Err(_) => 5,
                };
                from.map(fields, "Hello, a map of 5 or 7")?;
                from.key("os")?;
                let at = from.at();
                let os = match from.text()? {
                    "Linux" => Os::Linux,
                    "Macos" => Os::Macos,
                    _ => return Err(WireError::Variant { at }),
                };
                from.key("pid")?;
                let pid = from.uint()?;
                from.key("arch")?;
                let at = from.at();
                let arch = match from.text()? {
                    "X86_64" => Arch::X86_64,
                    "Aarch64" => Arch::Aarch64,
                    _ => return Err(WireError::Variant { at }),
                };
                from.key("page")?;
                let page = from.uint32("a page size past 32 bits")?;
                let holds = match fields {
                    7 => {
                        from.key("holds")?;
                        match from.null() {
                            true => None,
                            false => Some(id32(&mut from, "a start image's check word is 32 bytes")?),
                        }
                    }
                    _ => None,
                };
                from.key("program")?;
                let program = ProgramId(id32(&mut from, "a program id is 32 bytes")?);
                let running = match fields {
                    7 => {
                        from.key("running")?;
                        from.bool()?
                    }
                    _ => false,
                };
                Ask::Hello { program, pid, arch, os, page, holds, running }
            }
            "Need" => {
                from.map(2, "Need, a map of 2")?;
                from.key("slot")?;
                let slot = Slot::read(&mut from)?;
                from.key("thread")?;
                let thread = Thread(from.uint()?);
                Ask::Need { slot, thread }
            }
            "Applied" => {
                from.map(2, "Applied, a map of 2")?;
                from.key("txn")?;
                let txn = Txn(from.uint()?);
                from.key("epoch")?;
                let epoch = from.uint()?;
                Ask::Applied { txn, epoch }
            }
            "Failed" => {
                from.map(2, "Failed, a map of 2")?;
                from.key("txn")?;
                let txn = Txn(from.uint()?);
                from.key("why")?;
                let why = from.text()?;
                Ask::Failed { txn, why }
            }
            "Frames" => {
                from.map(2, "Frames, a map of 2")?;
                from.key("txn")?;
                let txn = Txn(from.uint()?);
                from.key("held")?;
                let held = List::read(&mut from)?;
                Ask::Frames { txn, held }
            }
            "Ended" => {
                from.map(3, "Ended, a map of 3")?;
                from.key("marks")?;
                let marks = from.bytes()?;
                from.key("needs")?;
                let needs = match from.null() {
                    true => None,
                    false => Some(from.text()?),
                };
                from.key("domain")?;
                let domain = from.uint()?;
                Ask::Ended { domain, marks, needs }
            }
            "Restarting" => {
                from.map(1, "Restarting, a map of 1")?;
                from.key("txn")?;
                Ask::Restarting { txn: Txn(from.uint()?) }
            }
            _ => return Err(WireError::Variant { at: named }),
        };
        from.end()?;
        Ok((id, ask))
    }
}

impl<'a> Tell<'a> {
    /// Writes the message as one frame onto the end of `out`. A message too long for a frame
    /// leaves `out` as it was.
    pub fn encode<A: Allocator>(&self, id: u64, out: &mut Vec<u8, A>) -> Result<(), WireError> {
        frame(id, out, |to| match *self {
            Tell::Welcome { slots } => {
                variant(to, "Welcome", 1);
                to.text("slots");
                to.uint(u64::from(slots));
            }
            Tell::Start { entry, inits } => {
                variant(to, "Start", 2);
                to.text("entry");
                entry.write(to);
                to.text("inits");
                inits.write(to);
            }
            Tell::Record { id, bytes } => {
                variant(to, "Record", 2);
                to.text("id");
                to.bytes(&id.0);
                to.text("bytes");
                to.bytes(bytes);
            }
            Tell::Statics { defs } => {
                variant(to, "Statics", 1);
                to.text("defs");
                defs.write(to);
            }
            Tell::Edit { txn, binds, declare, together } => {
                variant(to, "Edit", 4);
                to.text("txn");
                to.uint(txn.0);
                to.text("binds");
                binds.write(to);
                to.text("declare");
                to.uint(u64::from(declare));
                to.text("together");
                to.bool(together);
            }
            Tell::Refused { slot, diagnostic } => {
                variant(to, "Refused", 2);
                to.text("slot");
                slot.write(to);
                to.text("diagnostic");
                to.text(diagnostic);
            }
            Tell::Probe { txn, slots } => {
                variant(to, "Probe", 2);
                to.text("txn");
                to.uint(txn.0);
                to.text("slots");
                slots.write(to);
            }
            Tell::Restart { txn, why } => {
                variant(to, "Restart", 2);
                to.text("txn");
                to.uint(txn.0);
                to.text("why");
                to.text(why);
            }
            Tell::Unheld { why } => {
                variant(to, "Unheld", 1);
                to.text("why");
                to.text(why);
            }
            Tell::Image { bytes } => {
                variant(to, "Image", 1);
                to.text("bytes");
                to.bytes(bytes);
            }
        })
    }

    /// Reads the message of one frame's item: the id and the body.
    pub fn decode(item: &'a [u8]) -> Result<(u64, Tell<'a>), WireError> {
        let (id, mut from) = envelope(item)?;
        from.map(1, "a body, a map of one variant")?;
        let named = from.at();
        let tell = match from.text()? {
            "Welcome" => {
                from.map(1, "Welcome, a map of 1")?;
                from.key("slots")?;
                let slots = from.uint32("a slot count past 32 bits")?;
                Tell::Welcome { slots }
            }
            "Start" => {
                from.map(2, "Start, a map of 2")?;
                from.key("entry")?;
                let entry = Slot::read(&mut from)?;
                from.key("inits")?;
                let inits = List::read(&mut from)?;
                Tell::Start { entry, inits }
            }
            "Record" => {
                from.map(2, "Record, a map of 2")?;
                from.key("id")?;
                let id = RecordId(id32(&mut from, "a record id is 32 bytes")?);
                from.key("bytes")?;
                let bytes = from.bytes()?;
                Tell::Record { id, bytes }
            }
            "Statics" => {
                from.map(1, "Statics, a map of 1")?;
                from.key("defs")?;
                Tell::Statics { defs: List::read(&mut from)? }
            }
            "Edit" => {
                from.map(4, "Edit, a map of 4")?;
                from.key("txn")?;
                let txn = Txn(from.uint()?);
                from.key("binds")?;
                let binds = List::read(&mut from)?;
                from.key("declare")?;
                let declare = from.uint32("a slot count past 32 bits")?;
                from.key("together")?;
                let together = from.bool()?;
                Tell::Edit { txn, binds, declare, together }
            }
            "Refused" => {
                from.map(2, "Refused, a map of 2")?;
                from.key("slot")?;
                let slot = Slot::read(&mut from)?;
                from.key("diagnostic")?;
                let diagnostic = from.text()?;
                Tell::Refused { slot, diagnostic }
            }
            "Probe" => {
                from.map(2, "Probe, a map of 2")?;
                from.key("txn")?;
                let txn = Txn(from.uint()?);
                from.key("slots")?;
                let slots = List::read(&mut from)?;
                Tell::Probe { txn, slots }
            }
            "Restart" => {
                from.map(2, "Restart, a map of 2")?;
                from.key("txn")?;
                let txn = Txn(from.uint()?);
                from.key("why")?;
                let why = from.text()?;
                Tell::Restart { txn, why }
            }
            "Unheld" => {
                from.map(1, "Unheld, a map of 1")?;
                from.key("why")?;
                Tell::Unheld { why: from.text()? }
            }
            "Image" => {
                from.map(1, "Image, a map of 1")?;
                from.key("bytes")?;
                Tell::Image { bytes: from.bytes()? }
            }
            _ => return Err(WireError::Variant { at: named }),
        };
        from.end()?;
        Ok((id, tell))
    }
}

/// The first frame of `bytes`, a stream's bytes so far: its item and how many bytes the
/// frame took, or `None` while the frame is not all here. A length past [`LONGEST_FRAME`] is
/// refused before a byte of the item is waited for.
pub fn split(bytes: &[u8]) -> Result<Option<(&[u8], usize)>, WireError> {
    let Some((len, rest)) = bytes.split_first_chunk::<4>() else {
        return Ok(None);
    };
    let len = u32::from_be_bytes(*len);
    if len > LONGEST_FRAME {
        return Err(WireError::Long { len: u64::from(len) });
    }
    Ok(rest.get(..len as usize).map(|item| (item, 4 + len as usize)))
}

/// One frame onto the end of `out`: the length, then `{id, body, schema}` with `body` writing
/// the body.
fn frame<A: Allocator>(
    id: u64,
    out: &mut Vec<u8, A>,
    body: impl FnOnce(&mut Writer<'_, A>),
) -> Result<(), WireError> {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    let mut to = Writer::new(out);
    to.map(3);
    to.text("id");
    to.uint(id);
    to.text("body");
    body(&mut to);
    to.text("schema");
    to.uint(u64::from(SCHEMA));
    let len = (out.len() - start - 4) as u64;
    if len > u64::from(LONGEST_FRAME) {
        out.truncate(start);
        return Err(WireError::Long { len });
    }
    out[start..start + 4].copy_from_slice(&(len as u32).to_be_bytes());
    Ok(())
}

/// A body's head: the map of one variant, its name, and the map of its `fields` fields.
fn variant<A: Allocator>(to: &mut Writer<'_, A>, name: &str, fields: usize) {
    to.map(1);
    to.text(name);
    to.map(fields);
}

/// Reads a message's envelope: the id, and a reader of the body alone. The schema is the
/// map's last entry and is checked before a byte of the body is taken for a field, so a
/// message of another schema is refused as that and not as a malformed body.
fn envelope(item: &[u8]) -> Result<(u64, Reader<'_>), WireError> {
    let mut from = Reader::new(item);
    from.map(3, "a message, a map of 3")?;
    from.key("id")?;
    let id = from.uint()?;
    from.key("body")?;
    let body = from.at();
    from.skip(DEPTH)?;
    let body_end = from.at();
    from.key("schema")?;
    let schema = from.uint()?;
    from.end()?;
    if schema != u64::from(SCHEMA) {
        return Err(WireError::Schema { said: schema });
    }
    Ok((id, Reader::within(item, body, body_end)))
}

fn tier_name(tier: Tier) -> &'static str {
    match tier {
        Tier::Zero => "Zero",
        Tier::Optimized => "Optimized",
    }
}

fn tier(from: &mut Reader<'_>) -> Result<Tier, WireError> {
    let at = from.at();
    match from.text()? {
        "Zero" => Ok(Tier::Zero),
        "Optimized" => Ok(Tier::Optimized),
        _ => Err(WireError::Variant { at }),
    }
}

/// A byte string of exactly 32 bytes.
fn id(from: &mut Reader<'_>, what: &'static str) -> Result<[u8; 32], WireError> {
    let at = from.at();
    <[u8; 32]>::try_from(from.bytes()?).map_err(|_| WireError::Range { at, what })
}

/// [`id`], under the name the decoders use where `id` is the message's.
fn id32(from: &mut Reader<'_>, what: &'static str) -> Result<[u8; 32], WireError> {
    id(from, what)
}
