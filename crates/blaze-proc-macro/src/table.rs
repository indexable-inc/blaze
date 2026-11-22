//! A crate's proc-macro results as ONE record: a sorted table of fixed-width rows over one
//! bytes section, read once a command and searched in place.
//!
//! - **The record.** `crate/<name>` in the records' folder, `name` the crate's
//!   ([`crate_name`]): a locator, never a key (every row carries its call's whole key, so two
//!   crates under one name share rows and nothing else). Its body is a head (the layout word,
//!   the row count, the list count), the rows ([`ROW`] bytes each: the call's base key, its
//!   read list's index, its answer's place), the read lists' places ([`LIST`] bytes each) and
//!   the bytes they point into. Rows are sorted by key, then list; lists by their bytes; so the
//!   body is a function of the set of rows (rule 25) and a call is a binary search.
//! - **Verified once (rule 74).** The file ends in the seal of its body. A command reads it
//!   whole once ([`open`]) and hashes it only when `whole/<name>` does not vouch for it: that
//!   file holds the boot it was written or checked in and the seal checked, so the body is
//!   hashed once a boot (a power cut tears unsynced bytes; nothing else does: the write is a
//!   publish, whole under its name or not there). [`Table`] is made only from bytes so proved
//!   and is the proof of their structure: a row read through it checks nothing again.
//! - **Read lists.** A row names one of the table's distinct read lists (subjects and the
//!   answer each had). A list is valued against the package at most once a command
//!   ([`CrateResults::current`]), whatever the number of calls that share it.
//! - **Written once.** A command that ran new calls writes the crate's table again, whole
//!   ([`CrateResults::keep`]): the rows it opened and its own. Two commands racing leave one
//!   of their two tables, never a mix; the other's new rows are run again by a later command.
//!   A row whose reads were found stale is left out.
//!
//! DEBT: the bytes are read whole through the effect layer (one copy a crate a command), not
//! mapped: bstd has no map effect, and the simulation could not tear one. The rows are laid
//! out for a mapping as they are.

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use blaze_expand::atoms::{ATOM, Atoms, AtomsBuilder, AtomsError, Checked};
use blaze_expand::{Ctx, Edition, Produced, ReadAnswer, ReadKind, Reads, TokenOrigin, TokenStream};
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc, SharedAlloc};
use bstd::sync::Lock;

use super::{
    Corrupt, Cursor, DirStore, FilesError, Held, RecordError, RecordName, RecordPlant, SEAL_LEN, answer_into, answer_len, answer_of, answer_pieces,
    origin_bytes, origin_of, tail_of, token_bytes, token_of,
};

/// A pending row's canonical bytes: one block of exactly their length in the allocator the
/// command handed the results ([`open`]).
type Bytes = blaze_own::Vec<u8, SharedAlloc<'static>>;
use crate::sandbox::{CALL_FILE, Package, answer_now, call_file_digest};

/// The layout word a body starts with. 03: a kept answer's tokens are rows over the table's
/// atoms ([`blaze_expand::atoms`], the token row form's one home) and its origins are runs
/// (02 held each token's kind and text again in every answer: 21 bytes a token). 02: places
/// are 64-bit (01's were 32-bit: a table past 4 GB could not be written).
const MAGIC: u64 = u64::from_le_bytes(*b"bzpmtb03");
/// The head: the layout word, the row count, the list count, the atom count, the atoms'
/// text's length. After it: the rows, the lists' places, the atoms, their text, then the
/// bytes the places point into.
const HEAD: usize = 24;
/// An origin run of a dense answer: how many tokens (32 bits), the origin's tag, the stride
/// (0 or 1), 16 zero bits, the first token's value (32 bits). Token `i` of a run came from
/// (tag, first + i * stride). Runs are greedy and maximal, so they are a function of the
/// origins.
const RUN: usize = 12;
/// A row: the base key (32), the list's index (32 bits), the answer's length (32 bits), the
/// answer's place (64 bits).
pub const ROW: usize = 48;
/// A list's place: where its bytes start (64 bits), their length, how many reads it names.
pub const LIST: usize = 16;
/// The seal's domain: the digest of a body under it ends the file.
const TABLE_SEAL: &str = "blaze proc-macro results table seal 1";

/// A list's verdict against this command's package: not valued yet, current, stale.
const UNKNOWN: u8 = 0;
const CURRENT: u8 = 1;
const STALE: u8 = 2;

/// The name a crate's results are filed under: the digest of its package's name (cargo's
/// `CARGO_PKG_NAME` among the variables the package gives its macros; of nothing when the
/// package has none). A locator, never a key: rows are valid by their own keys and read
/// lists, so the packages of one name (two versions, a library and its tests) share one
/// table soundly, and a variable or version that changes leaves the table where it was:
/// only the rows that read it miss. (Layout 1 named the table by every variable's value: any
/// change of one gave the crate an empty table.)
pub fn crate_name(package: &Package) -> ContentId {
    // A locator, never a key: a package with no known name (a toolchain crate) files under "".
    let name = match package.env.get("CARGO_PKG_NAME") {
        crate::package_env::EnvAnswer::Set(name) => name,
        crate::package_env::EnvAnswer::Unset | crate::package_env::EnvAnswer::Unknown => "",
    };
    ContentId::of("blaze proc-macro crate results 2", name.as_bytes())
}

fn table_file(krate: &ContentId) -> RecordName {
    DirStore::named(b"crate/", krate)
}

fn whole_file(krate: &ContentId) -> RecordName {
    DirStore::named(b"whole/", krate)
}

fn u32_at(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
}

/// A 64-bit place at `at`: `usize::MAX` (inside no table) when this machine's addresses do
/// not hold it.
fn place_at(bytes: &[u8], at: usize) -> usize {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    usize::try_from(u64::from_le_bytes(word)).unwrap_or(usize::MAX)
}

/// A body whose seal was checked, or vouched for in this boot: made only by [`read_whole`].
struct Whole {
    body: Vec<u8>,
}

/// A crate's results table, checked: every place in it lies inside it and its rows are in
/// order. Made only by [`Table::of`]; reads through it check nothing again.
pub struct Table {
    body: Vec<u8>,
    rows: usize,
    lists: usize,
    /// Where the atoms start in `body`, and how many there are; their text follows them.
    atoms_at: usize,
    atoms: usize,
    text_len: usize,
    /// The atoms and their text, proved once by [`Table::of`].
    proved: Checked,
}

/// One row of a [`Table`].
#[derive(Clone, Copy)]
struct Row<'t> {
    key: &'t [u8],
    list: usize,
    answer: &'t [u8],
}

impl Table {
    /// No rows: what a crate with no record has.
    const fn empty() -> Table {
        Table { body: Vec::new(), rows: 0, lists: 0, atoms_at: 0, atoms: 0, text_len: 0, proved: Checked::EMPTY }
    }

    /// The table's atoms: what a dense answer's token rows name.
    fn atoms(&self) -> Atoms<'_> {
        let text_at = self.atoms_at + self.atoms * ATOM;
        let (atoms, text) = (&self.body[self.atoms_at..text_at], &self.body[text_at..text_at + self.text_len]);
        // SAFETY: `proved` is set only where a `Table` is made: in `Table::of`, from
        // `Atoms::of(&body[atoms_at..text_at], &body[text_at..data_at])`, with `text_len` =
        // `data_at - text_at` and these same `atoms_at`, `atoms` and `body` stored beside it; or
        // `Checked::EMPTY` in `Table::empty`, whose sections are both empty. `body` is private to
        // `Table` and no method writes it after it is made (every use takes `&self`), so these are
        // the very bytes the proof was taken of, unchanged.
        unsafe { self.proved.atoms(atoms, text) }
            .expect("`Table::of` proved these two sections of the body, which nothing writes after")
    }

    /// Row `row`'s answer in the loose form ([`answer_into`]'s bytes): what a run's answer is
    /// compared with.
    fn loose_answer(&self, row: usize) -> Result<Vec<u8>, Corrupt> {
        loosen(&self.atoms(), self.row(row).answer)
    }

    /// `whole`'s body as a table, or what in it is not one.
    fn of(whole: Whole) -> Result<Table, &'static str> {
        let body = whole.body;
        if body.len() < HEAD {
            return Err("shorter than its head");
        }
        if u64::from_le_bytes([body[0], body[1], body[2], body[3], body[4], body[5], body[6], body[7]]) != MAGIC {
            return Err("its layout word");
        }
        let (rows, lists, atoms, text_len) = (u32_at(&body, 8), u32_at(&body, 12), u32_at(&body, 16), u32_at(&body, 20));
        let lists_at = rows.checked_mul(ROW).and_then(|it| it.checked_add(HEAD)).ok_or("its row count")?;
        let atoms_at = lists.checked_mul(LIST).and_then(|it| it.checked_add(lists_at)).ok_or("its list count")?;
        let text_at = atoms.checked_mul(ATOM).and_then(|it| it.checked_add(atoms_at)).ok_or("its atom count")?;
        let data_at = text_at.checked_add(text_len).ok_or("its atoms' text length")?;
        if data_at > body.len() {
            return Err("its tables run past its end");
        }
        let proved = Atoms::of(&body[atoms_at..text_at], &body[text_at..data_at]).map_err(|_| "its atoms")?.checked();
        let inside = |at: usize, len: usize| at >= data_at && at.checked_add(len).is_some_and(|end| end <= body.len());
        for list in 0..lists {
            let entry = lists_at + list * LIST;
            if !inside(place_at(&body, entry), u32_at(&body, entry + 8)) {
                return Err("a read list's place");
            }
        }
        for row in 0..rows {
            let entry = HEAD + row * ROW;
            if u32_at(&body, entry + 32) >= lists || !inside(place_at(&body, entry + 40), u32_at(&body, entry + 36)) {
                return Err("a row's list or answer place");
            }
            // Key then list index, strictly rising: 36 bytes compared as they lie would order
            // the index by its little-endian bytes, so the two are compared apart.
            if row > 0 {
                let before = entry - ROW;
                let order = body[before..before + 32].cmp(&body[entry..entry + 32]).then(u32_at(&body, before + 32).cmp(&u32_at(&body, entry + 32)));
                if order != core::cmp::Ordering::Less {
                    return Err("its rows' order");
                }
            }
        }
        Ok(Table { body, rows, lists, atoms_at, atoms, text_len, proved })
    }

    /// How many rows it holds.
    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// The body's bytes: what [`body_of`] wrote.
    pub fn bytes(&self) -> &[u8] {
        &self.body
    }

    fn row(&self, row: usize) -> Row<'_> {
        let entry = HEAD + row * ROW;
        let (at, len) = (place_at(&self.body, entry + 40), u32_at(&self.body, entry + 36));
        Row { key: &self.body[entry..entry + 32], list: u32_at(&self.body, entry + 32), answer: &self.body[at..at + len] }
    }

    fn list(&self, list: usize) -> &[u8] {
        let entry = HEAD + self.rows * ROW + list * LIST;
        let (at, len) = (place_at(&self.body, entry), u32_at(&self.body, entry + 8));
        &self.body[at..at + len]
    }

    /// The rows of `key`, by a binary search of the row table.
    fn rows_of(&self, key: &ContentId) -> core::ops::Range<usize> {
        let (mut low, mut high) = (0, self.rows);
        while low < high {
            let mid = low + (high - low) / 2;
            let entry = HEAD + mid * ROW;
            if self.body[entry..entry + 32] < key.0[..] {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        let mut end = low;
        while end < self.rows && self.body[HEAD + end * ROW..HEAD + end * ROW + 32] == key.0[..] {
            end += 1;
        }
        low..end
    }
}

/// A read list in canonical bytes: the count, then each read's subject and the answer it had.
fn list_into(out: &mut Vec<u8>, reads: &Reads<'_>) -> Result<(), usize> {
    list_pieces(reads, |piece| out.extend_from_slice(piece))
}

/// [`list_into`]'s bytes, each piece handed to `out` in order (measured, then written:
/// [`CrateResults::insert`]). `Err`: the index of a read whose answer a list does not hold,
/// before any piece of that read is handed out.
fn list_pieces(reads: &Reads<'_>, mut out: impl FnMut(&[u8])) -> Result<(), usize> {
    out(&(reads.list().len() as u32).to_le_bytes());
    for (at, read) in reads.list().iter().enumerate() {
        let subject = reads.subject(read);
        let answer = match read.answer {
            ReadAnswer::Digest(id) => Some(id),
            ReadAnswer::Absent => None,
            ReadAnswer::Bool(_) | ReadAnswer::Number(_) => return Err(at),
        };
        out(&(subject.len() as u32).to_le_bytes());
        out(subject.as_bytes());
        match answer {
            Some(id) => {
                out(&[1]);
                out(&id.0);
            }
            None => out(&[0]),
        }
    }
    Ok(())
}

/// What a call's row holds beside its key, in canonical bytes: `produced`'s read list, then
/// its answer. Two runs gave one row exactly when these are equal. `Err`: the index of a
/// read whose answer a list does not hold.
pub fn row_bytes(produced: &Produced<'_>) -> Result<Vec<u8>, usize> {
    let mut out = Vec::new();
    list_into(&mut out, &produced.reads)?;
    answer_into(&mut out, produced);
    Ok(out)
}

/// The reads of a read list's bytes, in `alloc`.
fn reads_of<'a>(list: &[u8], alloc: Alloc<'a>) -> Result<Reads<'a>, Corrupt> {
    let mut c = Cursor { bytes: list };
    let mut reads = Reads::new_in(alloc);
    for _ in 0..c.u32("the read count")? {
        let subject = c.str("a read's subject")?;
        reads.push(ReadKind::ProcMacro, subject, c.answer()?).map_err(|_| Corrupt("a read that does not fit"))?;
    }
    if !c.bytes.is_empty() {
        return Err(Corrupt("bytes after the read list's end"));
    }
    Ok(reads)
}

/// The [`Produced`] of a row this command ran: its read list's reads and its loose answer.
fn produced_of<'a>(list: &[u8], answer: &[u8], edition: Edition, alloc: Alloc<'a>) -> Result<Produced<'a>, Corrupt> {
    answer_of(Cursor { bytes: answer }, reads_of(list, alloc)?, edition, alloc)
}

/// One origin run at `c` ([`RUN`]): its length, its tag, its first value and its stride.
fn run_of(c: &mut Cursor<'_>) -> Result<(u32, u8, u32, u32), Corrupt> {
    let len = c.u32("an origin run")?;
    let tag = c.u8("an origin run's tag")?;
    let stride = c.u8("an origin run's stride")?;
    if stride > 1 || c.take(2, "an origin run")? != [0, 0] {
        return Err(Corrupt("an origin run's stride"));
    }
    Ok((len, tag, c.u32("an origin run's first value")?, u32::from(stride)))
}

/// The origin of token `at` of the run (`tag`, `first`, `stride`).
fn run_origin(tag: u8, first: u32, stride: u32, at: u32) -> Result<TokenOrigin, Corrupt> {
    at.checked_mul(stride).and_then(|step| first.checked_add(step)).and_then(|value| origin_of(tag, value)).ok_or(Corrupt("an origin run's values"))
}

/// A dense answer's head at `c`: how many tokens, how many origin runs, and the tokens' rows.
fn dense_head<'b>(c: &mut Cursor<'b>) -> Result<(u32, u32, &'b [u8]), Corrupt> {
    let (count, runs) = (c.u32("the token count")?, c.u32("the origin run count")?);
    let rows = (count as usize).checked_mul(blaze_expand::atoms::ROW).ok_or(Corrupt("the token count"))?;
    Ok((count, runs, c.take(rows, "the token rows")?))
}

/// The [`Produced`] of a table's row: its read list's reads and its dense answer over `atoms`
/// (the token count, the run count, a row a token, the runs, then the diagnostics and their
/// text as the loose form has them). The tokens are read with room made once.
fn dense_produced<'a>(atoms: &Atoms<'_>, list: &[u8], answer: &[u8], edition: Edition, alloc: Alloc<'a>) -> Result<Produced<'a>, Corrupt> {
    let reads = reads_of(list, alloc)?;
    let mut c = Cursor { bytes: answer };
    let (count, runs, rows) = dense_head(&mut c)?;
    let tokens = TokenStream::from_rows(atoms, rows, Ctx::root(edition), alloc).map_err(|error| match error {
        AtomsError::NoSuchAtom { .. } => Corrupt("a token row past the table's atoms"),
        _ => Corrupt("tokens that do not fit a stream"),
    })?;
    let mut origins: AVec<'a, TokenOrigin> = AVec::new_in(alloc);
    origins.reserve(count as usize);
    for _ in 0..runs {
        let (len, tag, first, stride) = run_of(&mut c)?;
        if len as usize > count as usize - origins.len() {
            return Err(Corrupt("origin runs longer than the tokens"));
        }
        for at in 0..len {
            origins.push(run_origin(tag, first, stride, at)?);
        }
    }
    if origins.len() != count as usize {
        return Err(Corrupt("origin runs shorter than the tokens"));
    }
    let (diagnostics, text) = tail_of(c, alloc)?;
    Ok(Produced { tokens, origins, reads, diagnostics, text })
}

/// A dense answer over `atoms` in the loose form: [`answer_into`]'s bytes for the same answer.
fn loosen(atoms: &Atoms<'_>, answer: &[u8]) -> Result<Vec<u8>, Corrupt> {
    let mut c = Cursor { bytes: answer };
    let (count, runs, rows) = dense_head(&mut c)?;
    let mut out = Vec::with_capacity(answer.len() * 4);
    out.extend_from_slice(&count.to_le_bytes());
    for id in rows.chunks_exact(blaze_expand::atoms::ROW) {
        let id = u32::from_le_bytes([id[0], id[1], id[2], id[3]]);
        let (kind, text) = atoms.atom(id).ok_or(Corrupt("a token row past the table's atoms"))?;
        out.extend_from_slice(&token_bytes(kind));
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
    }
    let mut tokens = 0u32;
    for _ in 0..runs {
        let (len, tag, first, stride) = run_of(&mut c)?;
        if len > count - tokens {
            return Err(Corrupt("origin runs longer than the tokens"));
        }
        for at in 0..len {
            origin_bytes(&mut out, run_origin(tag, first, stride, at)?);
        }
        tokens += len;
    }
    if tokens != count {
        return Err(Corrupt("origin runs shorter than the tokens"));
    }
    out.extend_from_slice(c.bytes);
    Ok(out)
}

/// A row's answer as [`file_of`] is given it.
#[derive(Clone, Copy)]
enum Answer<'r> {
    /// The loose form ([`answer_into`]'s bytes): a row this command ran.
    Loose(&'r [u8]),
    /// The dense form over the atoms of the table the command found: a row it found.
    Kept(&'r [u8]),
}

/// Why an answer is not written dense.
enum Undense {
    Corrupt(Corrupt),
    /// More atoms, or more bytes of atom text, than a table's 32-bit words count.
    Atoms,
}

impl From<Corrupt> for Undense {
    fn from(corrupt: Corrupt) -> Undense {
        Undense::Corrupt(corrupt)
    }
}

/// What follows an answer's rows in the dense form: its runs, diagnostics and text.
enum Tail<'r> {
    /// A range of the buffer the runs of loose answers are made in.
    Made(core::ops::Range<usize>),
    /// A found row's own bytes.
    Kept(&'r [u8]),
}

/// One answer as [`file_of`] writes it: its counts, where its tokens' interned ids start, and
/// its tail.
struct Part<'r> {
    tokens: u32,
    runs: u32,
    ids: usize,
    tail: Tail<'r>,
}

/// `part`'s tail: of `made`, the buffer loose answers' runs were made in, or its own bytes.
fn tail_bytes<'m>(part: &'m Part<'_>, made: &'m [u8]) -> &'m [u8] {
    match &part.tail {
        // clone: a range, two words.
        Tail::Made(range) => &made[range.clone()],
        Tail::Kept(bytes) => bytes,
    }
}

fn run_into(made: &mut Vec<u8>,(tag, first, len, stride): (u8, u32, u32, u32)) {
    let mut run = [0u8; RUN];
    run[..4].copy_from_slice(&len.to_le_bytes());
    run[4] = tag;
    run[5] = u8::from(stride == 1);
    run[8..].copy_from_slice(&first.to_le_bytes());
    made.extend_from_slice(&run);
}

/// A loose answer's part: its tokens interned in `builder` (their ids appended to `ids`), its
/// origins as greedy maximal runs and the rest of it appended to `made`.
fn loose_part<'r>(bytes: &[u8], builder: &mut AtomsBuilder<'_>, ids: &mut Vec<u32>, made: &mut Vec<u8>) -> Result<Part<'r>, Undense> {
    let mut c = Cursor { bytes };
    let count = c.u32("the token count")?;
    let first_id = ids.len();
    for _ in 0..count {
        let kind: [u8; 8] = c.take(8, "a token")?.try_into().map_err(|_| Corrupt("a token"))?;
        let kind = token_of(kind).ok_or(Corrupt("a token's kind"))?;
        let text = c.str("a token's text")?;
        ids.push(builder.intern(kind, text).map_err(|_| Undense::Atoms)?);
    }
    let start = made.len();
    let mut runs = 0u32;
    // The open run: its tag, its first value, its length, its stride.
    let mut open: Option<(u8, u32, u32, u32)> = None;
    for _ in 0..count {
        let (tag, value) = (c.u8("an origin")?, c.u32("an origin")?);
        origin_of(tag, value).ok_or(Corrupt("an origin"))?;
        open = Some(match open {
            // The second token of a run sets its stride: the same value, or the next one.
            Some((t, first, 1, _)) if t == tag && (value == first || first.checked_add(1) == Some(value)) => (t, first, 2, value - first),
            Some((t, first, len, stride)) if t == tag && len > 1 && len.checked_mul(stride).and_then(|step| first.checked_add(step)) == Some(value) => (t, first, len + 1, stride),
            Some(run) => {
                run_into(made, run);
                runs += 1;
                (tag, value, 1, 0)
            }
            None => (tag, value, 1, 0),
        });
    }
    if let Some(run) = open {
        run_into(made, run);
        runs += 1;
    }
    made.extend_from_slice(c.bytes);
    Ok(Part { tokens: count, runs, ids: first_id, tail: Tail::Made(start..made.len()) })
}

/// A found row's part: its rows' atoms (of `old`, the found table's) interned in `builder`,
/// each once (`remap`: an old atom's interned id, `u32::MAX` while it has none: an interned
/// id is below that), its tail as it lies.
fn kept_part<'r>(bytes: &'r [u8], old: Option<&Atoms<'_>>, remap: &mut [u32], builder: &mut AtomsBuilder<'_>, ids: &mut Vec<u32>) -> Result<Part<'r>, Undense> {
    let past = Corrupt("a token row past the table's atoms");
    let mut c = Cursor { bytes };
    let (count, runs, rows) = dense_head(&mut c)?;
    let first_id = ids.len();
    for id in rows.chunks_exact(blaze_expand::atoms::ROW) {
        let id = u32::from_le_bytes([id[0], id[1], id[2], id[3]]);
        let slot = remap.get_mut(id as usize).ok_or(past.clone())?;
        if *slot == u32::MAX {
            let (kind, text) = old.and_then(|old| old.atom(id)).ok_or(past.clone())?;
            *slot = builder.intern(kind, text).map_err(|_| Undense::Atoms)?;
        }
        ids.push(*slot);
    }
    Ok(Part { tokens: count, runs, ids: first_id, tail: Tail::Kept(c.bytes) })
}

/// What valuing a read list found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Valued {
    /// Each read is answered now as the list says.
    holds: bool,
    /// A read valued was the call's file ([`CALL_FILE`]): the verdict is the call's, not the
    /// package's, and is not kept for the list.
    of_call: bool,
}

/// Whether each read of `list` is answered now, in `package` and for a call in `file` (the
/// call's [`blaze_expand::ProcCall::file`], `None` where the host does not answer it), as the
/// list says; `valued` counts the reads asked.
fn list_holds(list: &[u8], package: &Package, file: Option<&str>, valued: &AtomicU64) -> Result<Valued, Corrupt> {
    let mut c = Cursor { bytes: list };
    let mut of_call = false;
    for _ in 0..c.u32("the read count")? {
        let subject = c.str("a read's subject")?;
        let held = c.answer()?;
        valued.fetch_add(1, Ordering::Relaxed);
        let now = if subject == CALL_FILE {
            of_call = true;
            Ok(file.map(call_file_digest))
        } else {
            answer_now(package, subject)
        };
        let Ok(now) = now else { return Ok(Valued { holds: false, of_call }) };
        if now.map_or(ReadAnswer::Absent, ReadAnswer::Digest) != held {
            return Ok(Valued { holds: false, of_call });
        }
    }
    Ok(Valued { holds: true, of_call })
}

/// A row a command ran: its key, its read list and its answer, each in canonical bytes.
struct New {
    key: ContentId,
    list: Bytes,
    answer: Bytes,
}

/// The rows a command ran for one crate, sorted by key then list bytes.
struct Pending {
    rows: Vec<New>,
    /// How many of them the last [`CrateResults::keep`] wrote.
    written: usize,
}

/// What a [`CrateResults::keep`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kept {
    /// The command ran no call the record did not hold: nothing written.
    Nothing,
    /// The crate's table written again, whole.
    Written { rows: usize, bytes: usize },
}

/// One crate name's results for one command: the table as the command found it and the rows
/// the command ran. Shared by every package of that name the command expands in; each has
/// its own [`Verdicts`].
pub struct CrateResults {
    krate: ContentId,
    table: Table,
    pending: Lock<Pending>,
    /// How many rows are pending: a lookup takes the lock only when there are some.
    pending_rows: AtomicU64,
    /// How many reads were valued against a package.
    valued: AtomicU64,
    /// Where a pending row's bytes are made: the allocator the command handed ([`open`]).
    alloc: SharedAlloc<'static>,
}

/// One package's verdict on each read list of a table: whether the list's reads are answered
/// in that package now as the list says. A list is valued at most once a package a command,
/// whatever the number of calls that share it.
pub struct Verdicts {
    lists: Vec<AtomicU8>,
}

impl Verdicts {
    /// No list of `results`' table valued yet.
    pub fn of(results: &CrateResults) -> Verdicts {
        Verdicts { lists: (0..results.table.lists).map(|_| AtomicU8::new(UNKNOWN)).collect() }
    }
}

/// The file of the table holding `rows` (key, list bytes, answer bytes), in any order and
/// without two of one key and list: sorted here, so the bytes are the set's. The body, then
/// its seal: what [`write_whole`] publishes as it lies. Its length is the rows' own (places
/// are 64-bit); what is refused by name is a count, or one list or answer, past 32 bits.
///
/// Every answer is written dense: a loose one's tokens are interned, a found one's rows
/// (over `old`, the atoms of the table they were found in) are mapped onto the new table's
/// atoms, which are sorted by content: the bytes do not depend on which rows were found and
/// which were run, or in what order.
fn file_of(krate: &ContentId, old: Option<&Atoms<'_>>, rows: &mut Vec<(&[u8], &[u8], Answer<'_>)>) -> Result<Vec<u8>, RecordError> {
    let mut lists: Vec<&[u8]> = rows.iter().map(|row| row.1).collect();
    lists.sort_unstable();
    lists.dedup();
    rows.sort_unstable_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(b.1)));
    let too_large = |what: &'static str, count: usize| RecordError::TooLarge { krate: *krate, what, count };
    let word = |what: &'static str, count: usize| u32::try_from(count).map_err(|_| too_large(what, count));
    let (row_count, list_count) = (word("rows", rows.len())?, word("read lists", lists.len())?);
    // DEBT: `Global` and a fixed seed named here: `keep`'s callers (other lanes' drivers) pass
    // no allocator and no seed. No byte written depends on the seed: the atoms are sorted.
    let mut builder = AtomsBuilder::new_in(Alloc::global(), 0);
    let mut remap: Vec<u32> = vec![u32::MAX; old.map_or(0, Atoms::len)];
    let (mut ids, mut made): (Vec<u32>, Vec<u8>) = (Vec::new(), Vec::new());
    let mut parts: Vec<Part<'_>> = Vec::with_capacity(rows.len());
    for (key, _, answer) in rows.iter() {
        let part = match *answer {
            Answer::Loose(bytes) => loose_part(bytes, &mut builder, &mut ids, &mut made),
            Answer::Kept(bytes) => kept_part(bytes, old, &mut remap, &mut builder, &mut ids),
        };
        parts.push(part.map_err(|error| match error {
            Undense::Corrupt(Corrupt(what)) => {
                let mut base = [0u8; 32];
                base.copy_from_slice(key);
                RecordError::Corrupt { base: ContentId(base), what }
            }
            Undense::Atoms => too_large("atoms or bytes of their text", builder.len()),
        })?);
    }
    let built = builder.finish();
    let (atom_count, text_len) = (word("atoms", built.atoms.len() / ATOM)?, word("bytes of atom text", built.text.len())?);
    let answer_len = |part: &Part<'_>| 8 + part.tokens as usize * blaze_expand::atoms::ROW + tail_bytes(part, &made).len();
    let lists_at = HEAD + rows.len() * ROW;
    let atoms_at = lists_at + lists.len() * LIST;
    let data_at = atoms_at + built.atoms.len() + built.text.len();
    let lists_len = lists.iter().map(|list| list.len()).sum::<usize>();
    let bytes = data_at + lists_len + parts.iter().map(answer_len).sum::<usize>();
    let mut out = Vec::with_capacity(bytes + SEAL_LEN);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&row_count.to_le_bytes());
    out.extend_from_slice(&list_count.to_le_bytes());
    out.extend_from_slice(&atom_count.to_le_bytes());
    out.extend_from_slice(&text_len.to_le_bytes());
    // The lists' bytes first: a row's answer lies after every list.
    let mut answer_at = data_at + lists_len;
    for ((key, list, _), part) in rows.iter().zip(&parts) {
        // Sorted and without a duplicate above: the list is found.
        let index = lists.partition_point(|it| *it < *list);
        out.extend_from_slice(key);
        out.extend_from_slice(&(index as u32).to_le_bytes());
        out.extend_from_slice(&word("bytes of one answer", answer_len(part))?.to_le_bytes());
        out.extend_from_slice(&(answer_at as u64).to_le_bytes());
        answer_at += answer_len(part);
    }
    let mut at = data_at;
    for list in &lists {
        out.extend_from_slice(&(at as u64).to_le_bytes());
        out.extend_from_slice(&word("bytes of one read list", list.len())?.to_le_bytes());
        out.extend_from_slice(&list[..4]);
        at += list.len();
    }
    out.extend_from_slice(&built.atoms);
    out.extend_from_slice(&built.text);
    for list in &lists {
        out.extend_from_slice(list);
    }
    for part in &parts {
        out.extend_from_slice(&part.tokens.to_le_bytes());
        out.extend_from_slice(&part.runs.to_le_bytes());
        for id in &ids[part.ids..part.ids + part.tokens as usize] {
            out.extend_from_slice(&built.ids[*id as usize].to_le_bytes());
        }
        out.extend_from_slice(tail_bytes(part, &made));
    }
    let seal: [u8; SEAL_LEN] = ContentId::of(TABLE_SEAL, &out).0;
    out.extend_from_slice(&seal);
    Ok(out)
}

/// The whole body filed for `krate`, proved: none when no file is there, when the file is
/// torn (a power cut's: counted in [`DirStore::torn`], and the next keep replaces it), or
/// when another command put a longer table there under this read (counted in
/// [`DirStore::replaced`]: this command runs the crate's calls again, as the loser of two
/// commands' keeps does).
fn read_whole(store: &DirStore, krate: &ContentId) -> Result<Option<Whole>, FilesError> {
    let mut bytes = match store.files.read(&table_file(krate))? {
        Held::Absent => return Ok(None),
        Held::Whole(bytes) => bytes,
        Held::Replaced => {
            store.replaced.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }
    };
    let Some(body_len) = bytes.len().checked_sub(SEAL_LEN) else {
        store.torn.fetch_add(1, Ordering::Relaxed);
        return Ok(None);
    };
    // A host with no boot fact vouches for nothing: the body is hashed at every open.
    let boot = store.files.boot;
    let mut vouched = false;
    if let Some(boot) = &boot
        && let Held::Whole(said) = store.files.read(&whole_file(krate))?
    {
        vouched = said.len() == boot.0.len() + SEAL_LEN && said[..boot.0.len()] == boot.0 && said[boot.0.len()..] == bytes[body_len..];
    }
    if !vouched && store.plant != RecordPlant::SealSkipped {
        if ContentId::of(TABLE_SEAL, &bytes[..body_len]).0 != bytes[body_len..] {
            store.torn.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }
        store.hashed.fetch_add(1, Ordering::Relaxed);
        if let Some(boot) = &boot {
            store.files.publish(&whole_file(krate), &voucher(&boot.0, &bytes[body_len..]))?;
        }
    }
    bytes.truncate(body_len);
    Ok(Some(Whole { body: bytes }))
}

/// What `whole/<name>` holds: the boot a table was written or checked in, then its seal.
fn voucher(boot: &[u8], seal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(boot.len() + seal.len());
    out.extend_from_slice(boot);
    out.extend_from_slice(seal);
    out
}

/// `file` ([`file_of`]'s: a body, then its seal) filed as `krate`'s table: whole under its
/// name or not there (a publish of the slice as it lies), then vouched for in this boot, as
/// bytes this process wrote.
fn write_whole(store: &DirStore, krate: &ContentId, file: &[u8]) -> Result<(), FilesError> {
    match store.plant {
        RecordPlant::RenameFirst => store.files.write_in_place(&table_file(krate), file)?,
        RecordPlant::FramedPublish => store.files.publish_framed(&table_file(krate), file)?,
        RecordPlant::None | RecordPlant::SealSkipped => store.files.publish(&table_file(krate), file)?,
    }
    if let Some(boot) = store.files.boot {
        store.files.publish(&whole_file(krate), &voucher(&boot.0, &file[file.len() - SEAL_LEN..]))?;
    }
    Ok(())
}

/// The results filed for the crate named `krate`, read and proved once for this command; the
/// rows the command runs are made in `alloc` (the hook's: its runtime's heap, handed from the
/// program's entry).
pub fn open(store: &DirStore, krate: ContentId, alloc: SharedAlloc<'static>) -> Result<CrateResults, RecordError> {
    let table = match read_whole(store, &krate).map_err(RecordError::Files)? {
        Some(whole) => Table::of(whole).map_err(|what| RecordError::Table { krate, what })?,
        None => Table::empty(),
    };
    Ok(CrateResults {
        krate,
        table,
        pending: Lock::new("a crate's proc-macro results of this command", Pending { rows: Vec::new(), written: 0 }),
        pending_rows: AtomicU64::new(0),
        valued: AtomicU64::new(0),
        alloc,
    })
}

impl CrateResults {
    /// The crate's name ([`crate_name`]).
    pub fn krate(&self) -> ContentId {
        self.krate
    }

    /// The table as this command found it.
    pub fn table(&self) -> &Table {
        &self.table
    }

    /// How many reads were valued against the package so far.
    pub fn valued(&self) -> u64 {
        self.valued.load(Ordering::Relaxed)
    }

    /// Whether the table's read list `list` holds in `package`, whose verdicts on this table
    /// `verdicts` are, for a call in `file`: valued the first time it is asked in this
    /// command, a load of its verdict after. A verdict that valued the call's file is that
    /// call's alone and is not kept: the list is valued again for the next call (two calls of
    /// one key in two files are two rows).
    fn current(&self, verdicts: &Verdicts, list: usize, package: &Package, file: Option<&str>) -> Result<bool, Corrupt> {
        match verdicts.lists[list].load(Ordering::Relaxed) {
            CURRENT => Ok(true),
            STALE => Ok(false),
            _ => {
                let valued = list_holds(self.table.list(list), package, file, &self.valued)?;
                // Two workers valuing one list store the same verdict: the package is the
                // command's snapshot.
                if !valued.of_call {
                    verdicts.lists[list].store(if valued.holds { CURRENT } else { STALE }, Ordering::Relaxed);
                }
                Ok(valued.holds)
            }
        }
    }

    fn pending(&self, store: &DirStore) -> Result<bstd::sync::LockGuard<'_, Pending>, RecordError> {
        let name = table_file(&self.krate);
        let cx = store.files.cx(&name).map_err(RecordError::Files)?;
        self.pending.lock(&cx).map_err(|_| RecordError::Files(FilesError::Stopped { name }))
    }

    /// The answer of `base` under what its reads are in `package` now, for a call in `file`
    /// (its [`blaze_expand::ProcCall::file`], `None` where the host does not answer it),
    /// decoded in `alloc`: a row of the table whose read list holds by `verdicts` (this
    /// package's, made by [`Verdicts::of`] this value), or a row this command ran whose reads
    /// hold in `package` and `file`.
    pub fn find<'a>(
        &self,
        store: &DirStore,
        base: &ContentId,
        package: &Package,
        verdicts: &Verdicts,
        file: Option<&str>,
        edition: Edition,
        alloc: Alloc<'a>,
    ) -> Result<Option<Produced<'a>>, RecordError> {
        let corrupt = |Corrupt(what)| RecordError::Corrupt { base: *base, what };
        for at in self.table.rows_of(base) {
            let row = self.table.row(at);
            if self.current(verdicts, row.list, package, file).map_err(corrupt)? {
                return dense_produced(&self.table.atoms(), self.table.list(row.list), row.answer, edition, alloc).map(Some).map_err(corrupt);
            }
        }
        if self.pending_rows.load(Ordering::Acquire) == 0 {
            return Ok(None);
        }
        let pending = self.pending(store)?;
        let first = pending.rows.partition_point(|row| row.key.0 < base.0);
        // Another package of this name may have run the row: its reads are valued here, as a
        // table row's are (a call's own package's hold; a repeat inside one command is rare).
        for row in pending.rows[first..].iter().take_while(|row| row.key == *base) {
            if list_holds(row.list.as_slice(), package, file, &self.valued).map_err(corrupt)?.holds {
                return produced_of(row.list.as_slice(), row.answer.as_slice(), edition, alloc).map(Some).map_err(corrupt);
            }
        }
        Ok(None)
    }

    /// `produced` taken as `base`'s answer, to be written by the next [`CrateResults::keep`].
    /// Another answer under the same key, reads and digests is refused
    /// ([`RecordError::TwoAnswers`], rule 21); the same one again is taken once.
    pub fn insert(&self, store: &DirStore, base: ContentId, produced: &Produced<'_>) -> Result<(), RecordError> {
        // Each in ONE block of its own length: measured, then written (the 2026-11-18 heap log
        // held 380 MB of answers grown a push at a time, up to twice their length each).
        let mut list_len = 0usize;
        list_pieces(&produced.reads, |piece| list_len += piece.len()).map_err(|read| RecordError::Unkeepable { base, read })?;
        let mut list = self.sized(base, list_len)?;
        list_pieces(&produced.reads, |piece| list.extend_from_slice(piece)).map_err(|read| RecordError::Unkeepable { base, read })?;
        let mut answer = self.sized(base, answer_len(produced))?;
        answer_pieces(produced, |piece| answer.extend_from_slice(piece));
        self.insert_bytes(store, base, list, answer)
    }

    /// An empty buffer with room for exactly `len` bytes of `base`'s row, in the results'
    /// allocator; refused by name when it refuses.
    fn sized(&self, base: ContentId, len: usize) -> Result<Bytes, RecordError> {
        Bytes::try_with_capacity_in(len, self.alloc).map_err(|_| RecordError::NoRoom { base, bytes: len })
    }

    /// [`CrateResults::insert`] of a read list and an answer already in canonical bytes.
    fn insert_bytes(&self, store: &DirStore, base: ContentId, list: Bytes, answer: Bytes) -> Result<(), RecordError> {
        for at in self.table.rows_of(&base) {
            let row = self.table.row(at);
            if self.table.list(row.list) == list.as_slice() {
                // The found row is dense: compared in the loose form the run's answer is in.
                let held = self.table.loose_answer(at).map_err(|Corrupt(what)| RecordError::Corrupt { base, what })?;
                return if held.as_slice() == answer.as_slice() { Ok(()) } else { Err(RecordError::TwoAnswers { base }) };
            }
        }
        let mut pending = self.pending(store)?;
        let at = pending.rows.partition_point(|row| (row.key.0, row.list.as_slice()) < (base.0, list.as_slice()));
        if let Some(row) = pending.rows.get(at)
            && row.key == base
            && row.list.as_slice() == list.as_slice()
        {
            return if row.answer.as_slice() == answer.as_slice() { Ok(()) } else { Err(RecordError::TwoAnswers { base }) };
        }
        pending.rows.insert(at, New { key: base, list, answer });
        self.pending_rows.store(pending.rows.len() as u64, Ordering::Release);
        Ok(())
    }

    /// The table this command leaves: the rows it found (but those whose read list some
    /// package of `verdicts` found stale and none found current) and the rows it ran, as a
    /// file. None when it ran nothing since the last keep.
    fn body(&self, pending: &Pending, verdicts: &[&Verdicts]) -> Result<Option<Vec<u8>>, RecordError> {
        if pending.rows.len() == pending.written {
            return Ok(None);
        }
        let verdict = |list: usize, wanted: u8| verdicts.iter().any(|it| it.lists[list].load(Ordering::Relaxed) == wanted);
        let mut rows: Vec<(&[u8], &[u8], Answer<'_>)> = Vec::with_capacity(self.table.rows + pending.rows.len());
        for at in 0..self.table.rows {
            let row = self.table.row(at);
            if verdict(row.list, CURRENT) || !verdict(row.list, STALE) {
                rows.push((row.key, self.table.list(row.list), Answer::Kept(row.answer)));
            }
        }
        for row in &pending.rows {
            rows.push((&row.key.0, row.list.as_slice(), Answer::Loose(row.answer.as_slice())));
        }
        file_of(&self.krate, Some(&self.table.atoms()), &mut rows).map(Some)
    }

    /// Writes the crate's table when this command ran calls it did not hold: whole, under a
    /// temporary name, renamed onto the crate's. `verdicts`: those of every package of this
    /// name the command expanded in (what says which rows it found stale).
    pub fn keep(&self, store: &DirStore, verdicts: &[&Verdicts]) -> Result<Kept, RecordError> {
        let mut pending = self.pending(store)?;
        let Some(file) = self.body(&pending, verdicts)? else { return Ok(Kept::Nothing) };
        write_whole(store, &self.krate, &file).map_err(RecordError::Files)?;
        pending.written = pending.rows.len();
        Ok(Kept::Written { rows: u32_at(&file, 8), bytes: file.len() })
    }
}

#[cfg(test)]
#[path = "table_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "table_once_tests.rs"]
mod once_tests;
