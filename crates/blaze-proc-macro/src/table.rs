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

use blaze_expand::{Edition, Produced, ReadAnswer, ReadKind, Reads};
use blaze_ids::{ContentId, Hasher};
use blaze_own::Alloc;
use bstd::sync::Lock;

use super::{Corrupt, Cursor, DirStore, FilesError, RecordError, RecordName, RecordPlant, SEAL_LEN, answer_into, answer_of};
use crate::sandbox::{Package, answer_now};

/// The layout word a body starts with.
const MAGIC: u64 = u64::from_le_bytes(*b"bzpmtb01");
/// The head: the layout word, the row count, the list count.
const HEAD: usize = 16;
/// A row: the base key (32), the list's index, the answer's place and length, a zero word.
pub const ROW: usize = 48;
/// A list's place: where its bytes start, their length, how many reads it names.
pub const LIST: usize = 12;
/// The seal's domain: the digest of a body under it ends the file.
const TABLE_SEAL: &str = "blaze proc-macro results table seal 1";

/// A list's verdict against this command's package: not valued yet, current, stale.
const UNKNOWN: u8 = 0;
const CURRENT: u8 = 1;
const STALE: u8 = 2;

/// The name a crate's results are filed under: the digest of the variables its package gives
/// its macros (cargo's `CARGO_PKG_NAME`, `CARGO_PKG_VERSION`, `CARGO_CRATE_NAME` and the
/// rest), sorted. A locator: rows are valid by their own keys and read lists.
pub fn crate_name(package: &Package) -> ContentId {
    let mut pairs: Vec<&(String, String)> = package.env.iter().collect();
    pairs.sort();
    let mut hasher = Hasher::new("blaze proc-macro crate results 1");
    for (name, value) in pairs {
        hasher.update(&(name.len() as u32).to_le_bytes()).update(name.as_bytes());
        hasher.update(&(value.len() as u32).to_le_bytes()).update(value.as_bytes());
    }
    hasher.finish()
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
        Table { body: Vec::new(), rows: 0, lists: 0 }
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
        let (rows, lists) = (u32_at(&body, 8), u32_at(&body, 12));
        let lists_at = rows.checked_mul(ROW).and_then(|it| it.checked_add(HEAD)).ok_or("its row count")?;
        let data_at = lists.checked_mul(LIST).and_then(|it| it.checked_add(lists_at)).ok_or("its list count")?;
        if data_at > body.len() {
            return Err("its tables run past its end");
        }
        let inside = |at: usize, len: usize| at >= data_at && at.checked_add(len).is_some_and(|end| end <= body.len());
        for list in 0..lists {
            let entry = lists_at + list * LIST;
            if !inside(u32_at(&body, entry), u32_at(&body, entry + 4)) {
                return Err("a read list's place");
            }
        }
        for row in 0..rows {
            let entry = HEAD + row * ROW;
            if u32_at(&body, entry + 32) >= lists || !inside(u32_at(&body, entry + 36), u32_at(&body, entry + 40)) {
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
        Ok(Table { body, rows, lists })
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
        let (at, len) = (u32_at(&self.body, entry + 36), u32_at(&self.body, entry + 40));
        Row { key: &self.body[entry..entry + 32], list: u32_at(&self.body, entry + 32), answer: &self.body[at..at + len] }
    }

    fn list(&self, list: usize) -> &[u8] {
        let entry = HEAD + self.rows * ROW + list * LIST;
        let (at, len) = (u32_at(&self.body, entry), u32_at(&self.body, entry + 4));
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
    out.extend_from_slice(&(reads.list().len() as u32).to_le_bytes());
    for (at, read) in reads.list().iter().enumerate() {
        let subject = reads.subject(read);
        out.extend_from_slice(&(subject.len() as u32).to_le_bytes());
        out.extend_from_slice(subject.as_bytes());
        match read.answer {
            ReadAnswer::Digest(id) => {
                out.push(1);
                out.extend_from_slice(&id.0);
            }
            ReadAnswer::Absent => out.push(0),
            ReadAnswer::Bool(_) | ReadAnswer::Number(_) => return Err(at),
        }
    }
    Ok(())
}

/// The [`Produced`] of a row: its read list's reads and its answer, in `alloc`.
fn produced_of<'a>(list: &[u8], answer: &[u8], edition: Edition, alloc: Alloc<'a>) -> Result<Produced<'a>, Corrupt> {
    let mut c = Cursor { bytes: list };
    let mut reads = Reads::new_in(alloc);
    for _ in 0..c.u32("the read count")? {
        let subject = c.str("a read's subject")?;
        reads.push(ReadKind::ProcMacro, subject, c.answer()?).map_err(|_| Corrupt("a read that does not fit"))?;
    }
    if !c.bytes.is_empty() {
        return Err(Corrupt("bytes after the read list's end"));
    }
    answer_of(Cursor { bytes: answer }, reads, edition, alloc)
}

/// Whether each read of `list` is answered now, in `package`, as the list says; `valued`
/// counts the reads asked.
fn list_holds(list: &[u8], package: &Package, valued: &AtomicU64) -> Result<bool, Corrupt> {
    let mut c = Cursor { bytes: list };
    for _ in 0..c.u32("the read count")? {
        let subject = c.str("a read's subject")?;
        let held = c.answer()?;
        valued.fetch_add(1, Ordering::Relaxed);
        let Ok(now) = answer_now(package, subject) else { return Ok(false) };
        if now.map_or(ReadAnswer::Absent, ReadAnswer::Digest) != held {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A row a command ran: its key, its read list and its answer, each in canonical bytes.
struct New {
    key: ContentId,
    list: Vec<u8>,
    answer: Vec<u8>,
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

/// One crate's results for one command: the table as the command found it, each read list's
/// verdict against the crate's package, and the rows the command ran.
pub struct CrateResults {
    krate: ContentId,
    table: Table,
    /// One verdict a list of the table: valued at most once a command.
    verdicts: Vec<AtomicU8>,
    pending: Lock<Pending>,
    /// How many rows are pending: a lookup takes the lock only when there are some.
    pending_rows: AtomicU64,
    /// How many reads were valued against the package.
    valued: AtomicU64,
}

/// The body of the table holding `rows` (key, list bytes, answer bytes), in any order and
/// without two of one key and list: sorted here, so the bytes are the set's.
fn body_of(krate: &ContentId, rows: &mut Vec<(&[u8], &[u8], &[u8])>) -> Result<Vec<u8>, RecordError> {
    let mut lists: Vec<&[u8]> = rows.iter().map(|row| row.1).collect();
    lists.sort_unstable();
    lists.dedup();
    rows.sort_unstable_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(b.1)));
    let lists_at = HEAD + rows.len() * ROW;
    let data_at = lists_at + lists.len() * LIST;
    let bytes = data_at + lists.iter().map(|list| list.len()).sum::<usize>() + rows.iter().map(|row| row.2.len()).sum::<usize>();
    if u32::try_from(bytes).is_err() {
        return Err(RecordError::TooLarge { krate: *krate, bytes });
    }
    let mut out = Vec::with_capacity(bytes);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    out.extend_from_slice(&(lists.len() as u32).to_le_bytes());
    // The lists' places first: a row's answer lies after every list.
    let mut at = data_at;
    let mut answer_at = data_at + lists.iter().map(|list| list.len()).sum::<usize>();
    for (key, list, answer) in rows.iter() {
        // Sorted and without a duplicate above: the list is found.
        let index = lists.partition_point(|it| *it < *list);
        out.extend_from_slice(key);
        out.extend_from_slice(&(index as u32).to_le_bytes());
        out.extend_from_slice(&(answer_at as u32).to_le_bytes());
        out.extend_from_slice(&(answer.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        answer_at += answer.len();
    }
    for list in &lists {
        out.extend_from_slice(&(at as u32).to_le_bytes());
        out.extend_from_slice(&(list.len() as u32).to_le_bytes());
        out.extend_from_slice(&list[..4]);
        at += list.len();
    }
    for list in &lists {
        out.extend_from_slice(list);
    }
    for (_, _, answer) in rows.iter() {
        out.extend_from_slice(answer);
    }
    Ok(out)
}

/// The whole body filed for `krate`, proved: none when no file is there or the file is torn
/// (a power cut's: counted in [`DirStore::torn`], and the next keep replaces it).
fn read_whole(store: &DirStore, krate: &ContentId) -> Result<Option<Whole>, FilesError> {
    let Some(mut bytes) = store.files.read(&table_file(krate))? else { return Ok(None) };
    let Some(body_len) = bytes.len().checked_sub(SEAL_LEN) else {
        store.torn.fetch_add(1, Ordering::Relaxed);
        return Ok(None);
    };
    // A host with no boot fact vouches for nothing: the body is hashed at every open.
    let boot = store.files.io.boot().ok();
    let mut vouched = false;
    if let Some(boot) = &boot
        && let Some(said) = store.files.read(&whole_file(krate))?
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
            let seal: [u8; SEAL_LEN] = ContentId::of(TABLE_SEAL, &bytes[..body_len]).0;
            store.files.publish(&whole_file(krate), &boot.0, &seal)?;
        }
    }
    bytes.truncate(body_len);
    Ok(Some(Whole { body: bytes }))
}

/// `body` filed as `krate`'s table: whole under its name or not there (a publish), then
/// vouched for in this boot, as bytes this process wrote.
fn write_whole(store: &DirStore, krate: &ContentId, body: &[u8]) -> Result<(), FilesError> {
    let seal: [u8; SEAL_LEN] = ContentId::of(TABLE_SEAL, body).0;
    if store.plant == RecordPlant::RenameFirst {
        store.files.write_in_place(&table_file(krate), body, &seal)?;
    } else {
        store.files.publish(&table_file(krate), body, &seal)?;
    }
    if let Ok(boot) = store.files.io.boot() {
        store.files.publish(&whole_file(krate), &boot.0, &seal)?;
    }
    Ok(())
}

/// The results filed for the crate named `krate`, read and proved once for this command.
pub fn open(store: &DirStore, krate: ContentId) -> Result<CrateResults, RecordError> {
    let table = match read_whole(store, &krate).map_err(RecordError::Files)? {
        Some(whole) => Table::of(whole).map_err(|what| RecordError::Table { krate, what })?,
        None => Table::empty(),
    };
    let verdicts = (0..table.lists).map(|_| AtomicU8::new(UNKNOWN)).collect();
    Ok(CrateResults {
        krate,
        table,
        verdicts,
        pending: Lock::new("a crate's proc-macro results of this command", Pending { rows: Vec::new(), written: 0 }),
        pending_rows: AtomicU64::new(0),
        valued: AtomicU64::new(0),
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

    /// Whether the table's read list `list` holds in `package`: valued the first time it is
    /// asked in this command, a load of its verdict after.
    fn current(&self, list: usize, package: &Package) -> Result<bool, Corrupt> {
        match self.verdicts[list].load(Ordering::Relaxed) {
            CURRENT => Ok(true),
            STALE => Ok(false),
            _ => {
                let holds = list_holds(self.table.list(list), package, &self.valued)?;
                // Two workers valuing one list store the same verdict: the package is the
                // command's snapshot.
                self.verdicts[list].store(if holds { CURRENT } else { STALE }, Ordering::Relaxed);
                Ok(holds)
            }
        }
    }

    fn pending(&self, store: &DirStore) -> Result<bstd::sync::LockGuard<'_, Pending>, RecordError> {
        let name = table_file(&self.krate);
        let cx = store.files.cx(&name).map_err(RecordError::Files)?;
        self.pending.lock(&cx).map_err(|_| RecordError::Files(FilesError::Stopped { name }))
    }

    /// The answer of `base` under what its reads are in `package` now, decoded in `alloc`:
    /// a row of the table whose read list holds, or a row this command ran.
    pub fn find<'a>(&self, store: &DirStore, base: &ContentId, package: &Package, edition: Edition, alloc: Alloc<'a>) -> Result<Option<Produced<'a>>, RecordError> {
        let corrupt = |Corrupt(what)| RecordError::Corrupt { base: *base, what };
        for at in self.table.rows_of(base) {
            let row = self.table.row(at);
            if self.current(row.list, package).map_err(corrupt)? {
                return produced_of(self.table.list(row.list), row.answer, edition, alloc).map(Some).map_err(corrupt);
            }
        }
        if self.pending_rows.load(Ordering::Acquire) == 0 {
            return Ok(None);
        }
        let pending = self.pending(store)?;
        let first = pending.rows.partition_point(|row| row.key.0 < base.0);
        // A row this command ran was run in this package: its reads hold.
        match pending.rows.get(first) {
            Some(row) if row.key == *base => produced_of(&row.list, &row.answer, edition, alloc).map(Some).map_err(corrupt),
            _ => Ok(None),
        }
    }

    /// `produced` taken as `base`'s answer, to be written by the next [`CrateResults::keep`].
    /// Another answer under the same key, reads and digests is refused
    /// ([`RecordError::TwoAnswers`], rule 21); the same one again is taken once.
    pub fn insert(&self, store: &DirStore, base: ContentId, produced: &Produced<'_>) -> Result<(), RecordError> {
        let mut list = Vec::new();
        list_into(&mut list, &produced.reads).map_err(|read| RecordError::Unkeepable { base, read })?;
        let mut answer = Vec::new();
        answer_into(&mut answer, produced);
        self.insert_bytes(store, base, list, answer)
    }

    /// [`CrateResults::insert`] of a read list and an answer already in canonical bytes.
    fn insert_bytes(&self, store: &DirStore, base: ContentId, list: Vec<u8>, answer: Vec<u8>) -> Result<(), RecordError> {
        for at in self.table.rows_of(&base) {
            let row = self.table.row(at);
            if self.table.list(row.list) == list {
                return if row.answer == answer { Ok(()) } else { Err(RecordError::TwoAnswers { base }) };
            }
        }
        let mut pending = self.pending(store)?;
        let at = pending.rows.partition_point(|row| (row.key.0, &row.list[..]) < (base.0, &list[..]));
        if let Some(row) = pending.rows.get(at)
            && row.key == base
            && row.list == list
        {
            return if row.answer == answer { Ok(()) } else { Err(RecordError::TwoAnswers { base }) };
        }
        pending.rows.insert(at, New { key: base, list, answer });
        self.pending_rows.store(pending.rows.len() as u64, Ordering::Release);
        Ok(())
    }

    /// The table this command leaves: the rows it found (but those whose reads it found
    /// stale) and the rows it ran, as a body. None when it ran nothing since the last keep.
    fn body(&self, pending: &Pending) -> Result<Option<Vec<u8>>, RecordError> {
        if pending.rows.len() == pending.written {
            return Ok(None);
        }
        let mut rows: Vec<(&[u8], &[u8], &[u8])> = Vec::with_capacity(self.table.rows + pending.rows.len());
        for at in 0..self.table.rows {
            let row = self.table.row(at);
            if self.verdicts[row.list].load(Ordering::Relaxed) != STALE {
                rows.push((row.key, self.table.list(row.list), row.answer));
            }
        }
        for row in &pending.rows {
            rows.push((&row.key.0, &row.list, &row.answer));
        }
        body_of(&self.krate, &mut rows).map(Some)
    }

    /// Writes the crate's table when this command ran calls it did not hold: whole, under a
    /// temporary name, renamed onto the crate's.
    pub fn keep(&self, store: &DirStore) -> Result<Kept, RecordError> {
        let mut pending = self.pending(store)?;
        let Some(body) = self.body(&pending)? else { return Ok(Kept::Nothing) };
        write_whole(store, &self.krate, &body).map_err(RecordError::Files)?;
        pending.written = pending.rows.len();
        Ok(Kept::Written { rows: u32_at(&body, 8), bytes: body.len() + SEAL_LEN })
    }
}

#[cfg(test)]
#[path = "table_tests.rs"]
mod tests;
