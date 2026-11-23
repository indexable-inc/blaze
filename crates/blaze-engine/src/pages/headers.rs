//! Fixed-width producer headers used by discovery and planning. Only the four
//! producers whose planner consumes an owned row carry its observation lists.
//! Offsets are relative to the payload after the sorted table.

use super::*;
use crate::RowFacts;

const MAGIC: &[u8; 4] = b"BEH1";
const ENTRY: usize = ROW_FIXED + 1 + ID + 8;
const SHARDS: usize = 1 << 7;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    pub facts: RowFacts,
    pub reads: u32,
    pub defines: u32,
    pub record: Option<Key>,
}

impl Header {
    pub fn from_page(page: &VerifiedPage<'_>, name: Name) -> Option<Self> {
        let row = page.row(name)?;
        Some(Self { facts: row.facts, reads: row.reads().len() as u32, defines: row.defines().len() as u32, record: page.record(name) })
    }
}

fn full(name: Name) -> bool {
    matches!(name.kind, Kind::Units | Kind::UnitGraph | Kind::Fetch | Kind::Script)
}

fn shard_full(index: usize) -> bool {
    index < 8 || (16..24).contains(&index)
}

enum HeaderChange<'k> {
    Set { name: Name, row: &'k Row<'k>, record: Option<Key> },
    Remove(Name),
    RecordOnly { name: Name, record: Option<Key> },
}

impl HeaderChange<'_> {
    fn name(&self) -> Name {
        match *self {
            HeaderChange::Set { name, .. } | HeaderChange::Remove(name) | HeaderChange::RecordOnly { name, .. } => name,
        }
    }
}

/// One changed producer header shard ready to be merged and content-hashed independently of
/// every other shard ([`HeaderPages::encode_with`], [`Headers::after_with`]).
pub struct HeaderShardJob<'k, 'b, 'a> {
    index: usize,
    old_key: Key,
    base: Option<&'b IndexedHeaders<'b>>,
    changes: SVec<'a, HeaderChange<'k>>,
}

/// The merged and hashed result of one [`HeaderShardJob`].
pub struct HeaderShardBuilt<'a> {
    index: usize,
    key: Key,
    bytes: SVec<'a, u8>,
    changed: bool,
}

impl HeaderShardJob<'_, '_, '_> {
    /// Linearly merge `base` with `changes` into one header shard and compute its content key.
    pub fn run<'a>(&self, alloc: SharedAlloc<'a>) -> Result<HeaderShardBuilt<'a>, PagesError> {
        let index = self.index;
        let page = u32::try_from(index).unwrap_or(ROOT_PAGE);
        let has_payload = shard_full(index);
        let base_table = self.base.map_or(&[][..], |b| b.table);
        let base_payload = self.base.map_or(&[][..], |b| b.payload);
        let base_count = base_table.len() / ENTRY;
        let max_entries = base_count + self.changes.len();
        let mut table = SVec::with_capacity_in(9 + max_entries * ENTRY, alloc);
        table.extend_from_slice(MAGIC);
        table.push(u8::try_from(index).unwrap_or(0));
        table.extend_from_slice(&0u32.to_le_bytes());
        let mut payload = if has_payload {
            SVec::with_capacity_in(base_payload.len(), alloc)
        } else {
            SVec::new_in(alloc)
        };
        let mut bi = 0usize;
        let emit_base_run = |table: &mut SVec<'a, u8>, payload: &mut SVec<'a, u8>, from: usize, to: usize| -> Result<(), PagesError> {
            if from >= to {
                return Ok(());
            }
            let run = base_table.get(from * ENTRY..to * ENTRY).unwrap_or_default();
            if !has_payload {
                table.extend_from_slice(run);
                return Ok(());
            }
            for raw in run.chunks_exact(ENTRY) {
                let old_start = usize::try_from(view::word(raw.get(ENTRY - 8..).unwrap_or_default())).unwrap_or(usize::MAX);
                let old_len = usize::try_from(view::word(raw.get(ENTRY - 4..).unwrap_or_default())).unwrap_or(usize::MAX);
                let new_start = payload.len();
                payload.extend_from_slice(base_payload.get(old_start..old_start.saturating_add(old_len)).unwrap_or_default());
                table.extend_from_slice(raw.get(..ENTRY - 8).unwrap_or_default());
                for field in [new_start, old_len] {
                    let offset = u32::try_from(field).map_err(|_| PagesError::Count { page, count: u32::MAX })?;
                    table.extend_from_slice(&offset.to_le_bytes());
                }
            }
            Ok(())
        };
        for change in self.changes.iter() {
            let target = change.name();
            let run_start = bi;
            while let Some(raw) = base_table.get(bi * ENTRY..(bi + 1) * ENTRY) && view::name(raw) < target {
                bi += 1;
            }
            emit_base_run(&mut table, &mut payload, run_start, bi)?;
            let matched = base_table.get(bi * ENTRY..(bi + 1) * ENTRY).filter(|raw| view::name(raw) == target);
            match *change {
                HeaderChange::Set { name, row, record } => {
                    if matched.is_some() { bi += 1; }
                    let start = payload.len();
                    debug_assert_eq!(full(name), has_payload);
                    if has_payload { put_row(&mut payload, name, row); }
                    let count = |n| u32::try_from(n).map_err(|_| PagesError::Count { page, count: u32::MAX });
                    let header = Header {
                        facts: RowFacts::of(row),
                        reads: count(row.reads.len())?,
                        defines: count(row.defines.len())?,
                        record,
                    };
                    put_header(&mut table, name, header, start, payload.len() - start, page)?;
                }
                HeaderChange::Remove(_) => {
                    if matched.is_some() { bi += 1; }
                }
                HeaderChange::RecordOnly { record, .. } => {
                    let Some(raw) = matched else {
                        // An entered task can leave only debt and no row.
                        continue;
                    };
                    bi += 1;
                    let old_start = usize::try_from(view::word(raw.get(ENTRY - 8..).unwrap_or_default())).unwrap_or(usize::MAX);
                    let old_len = usize::try_from(view::word(raw.get(ENTRY - 4..).unwrap_or_default())).unwrap_or(usize::MAX);
                    let new_start = payload.len();
                    if old_len > 0 {
                        payload.extend_from_slice(base_payload.get(old_start..old_start.saturating_add(old_len)).unwrap_or_default());
                    }
                    table.extend_from_slice(raw.get(..ROW_FIXED).unwrap_or_default());
                    table.push(u8::from(record.is_some()));
                    put_id(&mut table, &record.unwrap_or_default().0);
                    for field in [new_start, old_len] {
                        let offset = u32::try_from(field).map_err(|_| PagesError::Count { page, count: u32::MAX })?;
                        table.extend_from_slice(&offset.to_le_bytes());
                    }
                }
            }
        }
        emit_base_run(&mut table, &mut payload, bi, base_count)?;
        let count = u32::try_from((table.len() - 9) / ENTRY).map_err(|_| PagesError::Count { page, count: u32::MAX })?;
        if count == 0 {
            return Ok(HeaderShardBuilt { index, key: Key::default(), bytes: SVec::new_in(alloc), changed: false });
        }
        if let Some(count_slot) = table.get_mut(5..9) {
            count_slot.copy_from_slice(&count.to_le_bytes());
        }
        if !payload.is_empty() {
            table.extend_from_slice(&payload);
        }
        let key = content_key(&table);
        let changed = key != self.old_key;
        let bytes = if changed { table } else { SVec::new_in(alloc) };
        Ok(HeaderShardBuilt { index, key, bytes, changed })
    }
}

struct EntryAt {
    name: Name,
    header: Header,
    start: usize,
    len: usize,
}

fn entry(bytes: &[u8], page: u32) -> Result<EntryAt, PagesError> {
    let mut r = Rd { bytes, at: 0, page };
    let name = r.name()?;
    let key = Key(r.id()?);
    let phase = r.phase()?;
    let file = r.name()?;
    let source = r.id()?;
    let out = r.id()?;
    let serial = Gen(r.u64()?);
    let findings = r.u32()?;
    let flags = r.u8()?;
    if flags & !(INLINE | FAILED) != 0 { return Err(r.tag(flags)); }
    let defines = r.u32()?;
    let reads = r.u32()?;
    if reads as usize > crate::READS_CAP { return Err(PagesError::Reads { page, at: r.at as u32, count: reads }); }
    let has_record = r.u8()?;
    if has_record > 1 { return Err(r.tag(has_record)); }
    let record = Key(r.id()?);
    if has_record == 0 && record != Key::default() { return Err(PagesError::Magic { page }); }
    let facts = RowFacts { key, phase, file, source, out, serial, findings, inline: flags & INLINE != 0, failed: flags & FAILED != 0 };
    let header = Header { facts, reads, defines, record: (has_record == 1).then_some(record) };
    Ok(EntryAt { name, header, start: r.u32()? as usize, len: r.u32()? as usize })
}

/// A verified table and its immutable, length-delimited full rows.
pub struct IndexedHeaders<'b> {
    key: Key,
    index: usize,
    table: &'b [u8],
    payload: &'b [u8],
}

impl<'b> IndexedHeaders<'b> {
    /// Shard `index`, its bytes proved by `sealed`: its head checked (form, index, that the
    /// fixed-width table fits and its last row ends the payload), nothing hashed or walked (the
    /// pages' module docs, "Checked once"). A lookup is a binary search of the table.
    pub fn open(sealed: Sealed<'b>, index: usize) -> Result<Self, PagesError> {
        let (key, bytes) = (sealed.key(), sealed.bytes());
        let page = u32::try_from(index).unwrap_or(ROOT_PAGE);
        let mut r = Rd { bytes, page, at: 0 };
        if index >= HeaderPages::COUNT || r.take(4)? != MAGIC || r.u8()? as usize != index { return Err(PagesError::Magic { page }); }
        let count = r.count(ENTRY)?;
        let table = r.take(count * ENTRY)?;
        let payload = &bytes[r.at..];
        let end = match table.len().checked_sub(ENTRY) {
            Some(last) => view::word(&table[last + ENTRY - 8..]) as usize + view::word(&table[last + ENTRY - 4..]) as usize,
            None => 0,
        };
        if end != payload.len() { return Err(PagesError::Trailing { page, at: (r.at + end.min(payload.len())) as u32 }); }
        Ok(Self { key, index, table, payload })
    }

    /// [`IndexedHeaders::open`], then every row walked: each entry's form, order, shard and
    /// extent, and each full row against its header. For an explicit verify; the edit path
    /// never walks.
    pub fn open_whole(sealed: Sealed<'b>, index: usize) -> Result<Self, PagesError> {
        let shard = Self::open(sealed, index)?;
        let (table, payload) = (shard.table, shard.payload);
        let page = u32::try_from(index).unwrap_or(ROOT_PAGE);
        let mut previous = None;
        let mut end = 0usize;
        for (at, raw) in table.chunks_exact(ENTRY).enumerate() {
            let e = entry(raw, page)?;
            if HeaderPages::slot(e.name) != Some(index) {
                return Err(PagesError::Misplaced { page, section: "producer headers", name: e.name });
            }
            if previous.is_some_and(|name| name >= e.name) {
                return Err(PagesError::Unsorted { page, section: "producer headers", at: (9 + at * ENTRY) as u32 });
            }
            previous = Some(e.name);
            if e.start != end || e.len > payload.len() - end { return Err(PagesError::Short { page, at: end as u32 }); }
            end += e.len;
            if full(e.name) {
                let mut row = Rd { bytes: &payload[e.start..end], page, at: 0 };
                if row.name()? != e.name { return Err(PagesError::Magic { page }); }
                let stored = view::read_row(&mut row)?;
                view::check_row(&stored, page, e.start)?;
                if row.at != e.len || stored.facts != e.header.facts || stored.defines().len() != e.header.defines as usize || stored.reads().len() != e.header.reads as usize {
                    return Err(PagesError::Magic { page });
                }
            } else if e.len != 0 {
                return Err(PagesError::Magic { page });
            }
        }
        if end != payload.len() { return Err(PagesError::Trailing { page, at: (9 + table.len() + end) as u32 }); }
        Ok(shard)
    }

    fn lookup(&self, name: Name) -> Option<EntryAt> {
        let mut needle = [0; NAME];
        needle[0] = name.kind as u8;
        needle[1..].copy_from_slice(&name.subject.0);
        let (at, _) = crate::search(self.table.len() / ENTRY, |i| self.table[i * ENTRY..][..NAME].cmp(&needle[..]));
        let b = &self.table[at? * ENTRY..];
        let header = Header {
            facts: view::row_facts(b),
            defines: view::word(&b[ROW_FIXED - 8..]),
            reads: view::word(&b[ROW_FIXED - 4..]),
            record: (b[ROW_FIXED] == 1).then(|| Key(view::id(&b[ROW_FIXED + 1..]))),
        };
        Some(EntryAt { name, header, start: view::word(&b[ENTRY - 8..]) as usize, len: view::word(&b[ENTRY - 4..]) as usize })
    }

    pub fn get(&self, name: Name) -> Option<Header> { self.lookup(name).map(|e| e.header) }

    pub fn planner_row<'a>(&self, name: Name, alloc: SharedAlloc<'a>) -> Option<Row<'a>> {
        if !full(name) { return None; }
        let e = self.lookup(name)?;
        Some(StoredRow::from_checked(&self.payload[e.start..e.start + e.len]).to_owned(alloc))
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HeaderPages {
    pub keys: [Key; SHARDS],
}

impl Default for HeaderPages {
    fn default() -> Self { Self { keys: [Key::default(); Self::COUNT] } }
}

impl HeaderPages {
    pub const COUNT: usize = SHARDS;

    pub fn slot(name: Name) -> Option<usize> {
        // A sweep of the file and unit headers does not page in the signatures.
        let (base, bits) = match name.kind {
            Kind::BodyCheck | Kind::Lint => return None,
            Kind::Units | Kind::UnitGraph | Kind::Fetch => (0, 3),
            Kind::FileItems => (8, 3),
            Kind::Script => (16, 3),
            Kind::Signature => (32, 6),
            Kind::ImplBucket => (96, 5),
            _ => (24, 3),
        };
        Some(base + page_of(name, bits) as usize)
    }

    /// Share every untouched shard by key. A record decision without a rewritten row
    /// replaces only the record field of the previous header.
    pub fn encode<'b, 'a>(
        kept: &Kept<'_>, last: Option<&HeaderPages>,
        get: impl FnMut(usize) -> Result<Option<&'b IndexedHeaders<'b>>, PagesError>,
        alloc: SharedAlloc<'a>,
    ) -> Result<(Self, SVec<'a, (Key, SVec<'a, u8>)>), PagesError> {
        Self::encode_with(kept, last, get, alloc, |jobs| {
            let mut out = SVec::with_capacity_in(jobs.len(), alloc);
            for job in jobs {
                out.push(job.run(alloc));
            }
            out
        })
    }

    /// [`HeaderPages::encode`] with a caller-supplied executor for independent
    /// [`HeaderShardJob`]s so a multi-threaded driver can merge and BLAKE3-hash changed shards in
    /// parallel while keeping this crate sans-IO and deterministic.
    pub fn encode_with<'b, 'a, F>(
        kept: &Kept<'_>, last: Option<&HeaderPages>,
        mut get: impl FnMut(usize) -> Result<Option<&'b IndexedHeaders<'b>>, PagesError>,
        alloc: SharedAlloc<'a>,
        run_shards: F,
    ) -> Result<(Self, SVec<'a, (Key, SVec<'a, u8>)>), PagesError>
    where
        F: FnOnce(&[HeaderShardJob<'_, '_, '_>]) -> SVec<'a, Result<HeaderShardBuilt<'a>, PagesError>>,
    {
        let work = kept.answer.work();
        if work.is_some() && last.is_none() { return Err(PagesError::Base { serial: kept.answer.serial }); }
        let mut names: [SVec<'a, Name>; Self::COUNT] = core::array::from_fn(|_| Vec::new_in(alloc));
        let mut select = |name: &Name| {
            if let Some(index) = Self::slot(*name) { names[index].push(*name); }
        };
        if let Some(work) = work {
            work.rows.keys().for_each(&mut select);
            kept.causes.keys().for_each(&mut select);
        } else {
            kept.answer.rows.keys().for_each(select);
        }
        let mut next = match (work, last) {
            (Some(_), Some(last)) => last.clone(),
            _ => Self::default(),
        };
        let mut jobs: SVec<'_, HeaderShardJob<'_, '_, '_>> = SVec::with_capacity_in(Self::COUNT, alloc);
        for (index, mut shard_names) in names.into_iter().enumerate() {
            if shard_names.is_empty() { continue; }
            let old_key = last.map_or(Key::default(), |last| last.keys.get(index).copied().unwrap_or_default());
            let base = if work.is_some() && old_key != Key::default() {
                let page = u32::try_from(index).unwrap_or(ROOT_PAGE);
                let base = get(index)?.ok_or(PagesError::Missing { page, key: old_key })?;
                if base.key != old_key { return Err(PagesError::Digest { page, want: old_key, got: base.key }); }
                if base.index != index { return Err(PagesError::Magic { page }); }
                Some(base)
            } else {
                None
            };
            shard_names.sort_unstable();
            shard_names.dedup();
            let mut changes = SVec::with_capacity_in(shard_names.len(), alloc);
            for name in shard_names {
                match work {
                    Some(work) => match work.rows.get(&name) {
                        Some(Some(row)) => changes.push(HeaderChange::Set { name, row, record: kept.records.get(&name).copied() }),
                        Some(None) => changes.push(HeaderChange::Remove(name)),
                        None => changes.push(HeaderChange::RecordOnly { name, record: kept.records.get(&name).copied() }),
                    },
                    None => if let Some(row) = kept.answer.rows.get(&name) {
                        changes.push(HeaderChange::Set { name, row, record: kept.records.get(&name).copied() });
                    },
                }
            }
            jobs.push(HeaderShardJob { index, old_key, base, changes });
        }
        let built = run_shards(&jobs);
        let mut puts = SVec::with_capacity_in(built.len(), alloc);
        for one in built {
            let one = one?;
            if let Some(slot) = next.keys.get_mut(one.index) {
                *slot = one.key;
            }
            if one.changed {
                puts.push((one.key, one.bytes));
            }
        }
        Ok((next, puts))
    }
}

/// The producer header index as a stored answer names it (D187): the shards current with the
/// answer's pages, or none. An edit committed by the `Ready` driver writes pages alone: it
/// neither reads nor rewrites a shard (the edit's floor, D168), so the index it leaves is
/// [`Headers::Absent`], and every later lookup is answered from the row's own page, which holds
/// the same row (the shards are a second index of the pages' rows: D187). Absent names no key,
/// so a shard older than the pages cannot be read: the bug is not representable (rule 76).
/// Only a cold write (no last answer) makes the index whole again.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Headers {
    /// Each shard's key, current with the pages (a zero key: an empty shard).
    Current(HeaderPages),
    /// No shard is current since the answer of request `since`: the first commit that wrote
    /// pages without them.
    Absent { since: Gen },
}

/// Which driver wrote an answer: what [`Headers::after`] keeps of the index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Writer {
    /// The phase path: rewrites the shards its entered rows sit on, when they are current.
    Phases,
    /// The `Ready` driver's edit commit: pages alone, no shard read or written.
    Ready,
}

/// The planted bug of the index's simulation (headers_sim.rs), tests only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeadersPlant {
    None,
    /// A ready commit names the last shards as current, unrewritten: a later lookup reads a
    /// shard older than the pages.
    ReadyKeepsShards,
}

impl Headers {
    /// The shards' keys when current.
    pub fn current(&self) -> Option<&HeaderPages> {
        match self {
            Headers::Current(keys) => Some(keys),
            Headers::Absent { .. } => None,
        }
    }

    /// The index a commit of `kept` names after `last` (`None`: no last answer, a cold write),
    /// written by `writer`, and the shard records to put. A cold write encodes every shard
    /// whole; the phase path rewrites the shards its rows sit on over current ones (reading
    /// each base by `get`) and keeps an absent index absent (it has no base to rewrite: its
    /// lookups were answered from pages); a ready commit reads and writes no shard. `serial` is
    /// the request's, named as `since` by the first commit that leaves the index absent.
    pub fn after<'b, 'a>(
        last: Option<&Headers>,
        kept: &Kept<'_>,
        writer: Writer,
        serial: Gen,
        plant: HeadersPlant,
        get: impl FnMut(usize) -> Result<Option<&'b IndexedHeaders<'b>>, PagesError>,
        alloc: SharedAlloc<'a>,
    ) -> Result<(Headers, SVec<'a, (Key, SVec<'a, u8>)>), PagesError> {
        Self::after_with(last, kept, writer, serial, plant, get, alloc, |jobs| {
            let mut out = SVec::with_capacity_in(jobs.len(), alloc);
            for job in jobs {
                out.push(job.run(alloc));
            }
            out
        })
    }

    /// [`Headers::after`] with a caller-supplied executor for independent [`HeaderShardJob`]s.
    pub fn after_with<'b, 'a, F>(
        last: Option<&Headers>,
        kept: &Kept<'_>,
        writer: Writer,
        serial: Gen,
        plant: HeadersPlant,
        get: impl FnMut(usize) -> Result<Option<&'b IndexedHeaders<'b>>, PagesError>,
        alloc: SharedAlloc<'a>,
        run_shards: F,
    ) -> Result<(Headers, SVec<'a, (Key, SVec<'a, u8>)>), PagesError>
    where
        F: FnOnce(&[HeaderShardJob<'_, '_, '_>]) -> SVec<'a, Result<HeaderShardBuilt<'a>, PagesError>>,
    {
        let none = || Vec::new_in(alloc);
        match (last, writer) {
            (None, _) => {
                let (keys, puts) = HeaderPages::encode_with(kept, None, get, alloc, run_shards)?;
                Ok((Headers::Current(keys), puts))
            }
            (Some(Headers::Current(keys)), Writer::Phases) => {
                let (keys, puts) = HeaderPages::encode_with(kept, Some(keys), get, alloc, run_shards)?;
                Ok((Headers::Current(keys), puts))
            }
            (Some(Headers::Current(keys)), Writer::Ready) => match plant {
                HeadersPlant::ReadyKeepsShards => Ok((Headers::Current(keys.clone()), none())),
                HeadersPlant::None => Ok((Headers::Absent { since: serial }, none())),
            },
            (Some(Headers::Absent { since }), _) => Ok((Headers::Absent { since: *since }, none())),
        }
    }

    /// `name`'s header: from its shard when the index is current and its kind has one, else
    /// from its own page. `shard` is asked only for a current index; `page` only otherwise.
    pub fn header<'s, 'p>(
        &self,
        name: Name,
        shard: impl FnOnce(usize) -> Result<Option<&'s IndexedHeaders<'s>>, PagesError>,
        page: impl FnOnce() -> Result<Option<&'p VerifiedPage<'p>>, PagesError>,
    ) -> Result<Option<Header>, PagesError> {
        match (self, HeaderPages::slot(name)) {
            (Headers::Current(_), Some(index)) => Ok(shard(index)?.and_then(|headers| headers.get(name))),
            (Headers::Current(_), None) | (Headers::Absent { .. }, _) => Ok(page()?.and_then(|page| Header::from_page(page, name))),
        }
    }

    /// The full row the units planner and script ordering read (a kind whose shard carries
    /// rows: units, the unit graph, fetches, scripts; `None` for any other): from its shard
    /// when the index is current, else from its own page, the same row in the same form.
    pub fn planner_row<'s, 'p, 'a>(
        &self,
        name: Name,
        shard: impl FnOnce(usize) -> Result<Option<&'s IndexedHeaders<'s>>, PagesError>,
        page: impl FnOnce() -> Result<Option<&'p VerifiedPage<'p>>, PagesError>,
        alloc: SharedAlloc<'a>,
    ) -> Result<Option<Row<'a>>, PagesError> {
        if !full(name) {
            return Ok(None);
        }
        match (self, HeaderPages::slot(name)) {
            (Headers::Current(_), Some(index)) => Ok(shard(index)?.and_then(|headers| headers.planner_row(name, alloc))),
            (Headers::Current(_), None) | (Headers::Absent { .. }, _) => Ok(page()?.and_then(|page| page.row(name)).map(|row| row.to_owned(alloc))),
        }
    }
}

fn put_header(table: &mut SVec<'_, u8>, name: Name, header: Header, start: usize, len: usize, page: u32) -> Result<(), PagesError> {
    let f = header.facts;
    put_name(table, name);
    put_id(table, &f.key.0);
    put_phase(table, f.phase);
    put_name(table, f.file);
    put_id(table, &f.source);
    put_id(table, &f.out);
    table.extend_from_slice(&f.serial.0.to_le_bytes());
    table.extend_from_slice(&f.findings.to_le_bytes());
    table.push(u8::from(f.inline) * INLINE | u8::from(f.failed) * FAILED);
    table.extend_from_slice(&header.defines.to_le_bytes());
    table.extend_from_slice(&header.reads.to_le_bytes());
    table.push(u8::from(header.record.is_some()));
    put_id(table, &header.record.unwrap_or_default().0);
    for field in [start, len] {
        let offset = u32::try_from(field).map_err(|_| PagesError::Count { page, count: u32::MAX })?;
        table.extend_from_slice(&offset.to_le_bytes());
    }
    Ok(())
}
