//! The directory and the page writer for a request over a stored answer.

use super::*;
use crate::{Ask, RowFacts, StoredPages, Work};

fn put_plan(out: &mut SVec<'_, u8>, plan: &Planned) {
    put_name(out, plan.name);
    put_phase(out, plan.phase);
    put_id(out, &plan.source);
    put_name(out, plan.file);
}

/// The standing key and plan fields certified by a member's actual last row.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Claim {
    pub(super) key: Key,
    pub(super) phase: Phase,
    pub(super) source: ContentId,
}

pub(super) fn claim(row: &Row<'_>, file: Name, owed: bool) -> Option<Claim> {
    (!owed && row.file == file).then_some(Claim { key: row.key, phase: row.phase, source: row.source })
}

pub(super) fn put_member(bytes: &mut SVec<'_, u8>, name: Name, claim: Option<Claim>) {
    put_name(bytes, name);
    bytes.push(u8::from(claim.is_some()));
    match claim {
        Some(claim) => {
            put_id(bytes, &claim.key.0);
            put_phase(bytes, claim.phase);
            put_id(bytes, &claim.source);
        }
        None => bytes.extend_from_slice(&[0; ID + 3 + ID]),
    }
}

pub(super) fn file_digest(bytes: &[u8]) -> ContentId {
    ContentId::of("blaze-engine stored file plan 2", bytes)
}

/// Fingerprint of today's sorted plans, compared to the digest made from the stored
/// members' actual keys. A hand-built row under a different key cannot certify a plan.
pub fn plan_digest(file: Name, plans: &[Planned], tools: &crate::Tools, alloc: SharedAlloc<'_>) -> ContentId {
    let mut bytes = Vec::with_capacity_in(NAME + 4 + FILE_MEMBER * plans.len(), alloc);
    put_name(&mut bytes, file);
    put_u32(&mut bytes, plans.len());
    for plan in plans {
        put_member(&mut bytes, plan.name, Some(Claim { key: tools.key(plan.name, &plan.source), phase: plan.phase, source: plan.source }));
    }
    file_digest(&bytes)
}


fn empty_directory(alloc: SharedAlloc<'_>) -> Root<'_> {
    Root {
        serial: Gen(0), bits: 0, bytes: 0, pages: Vec::new_in(alloc), files: BTreeMap::new_in(alloc),
        places: BTreeMap::new_in(alloc), units: Vec::new_in(alloc), findings: BTreeMap::new_in(alloc), owed_tasks: BTreeMap::new_in(alloc),
        owed_cells: BTreeMap::new_in(alloc), outlines: 0,
    }
}

pub(super) fn directory<'a>(kept: &Kept<'_>, alloc: SharedAlloc<'a>) -> Root<'a> {
    let answer = &kept.answer;
    let mut root = empty_directory(alloc);
    root.owed_cells.extend(answer.owed_cells.iter().map(|(&id, &at)| (id, at)));
    root.owed_tasks.extend(answer.owed_tasks.iter().map(|(&name, &plan)| (name, plan)));
    for (&name, row) in answer.rows.iter() {
        if row.phase.stage == Stage::Units { root.units.push(name); }
        root.outlines += u32::from(row.phase.stage == Stage::Outlines);
        if row.failed || row.findings > 0 { root.findings.insert(name, row.findings); }
    }

    root
}

impl<'a> Root<'a> {
    pub(super) fn finish(self, mut puts: SVec<'a, (Key, SVec<'a, u8>)>, records: SVec<'a, (Key, SVec<'a, (Name, Key)>)>, plant: PagesPlant, alloc: SharedAlloc<'a>) -> Encoded<'a> {
        let mut bytes = Vec::with_capacity_in(ROOT_HEAD + ID * self.pages.len(), alloc);
        bytes.extend_from_slice(ROOT_MAGIC);
        bytes.extend_from_slice(&self.serial.0.to_le_bytes());
        bytes.push(self.bits);
        bytes.extend_from_slice(&self.bytes.to_le_bytes());
        put_u32(&mut bytes, self.pages.len());
        for key in self.pages.iter() { put_id(&mut bytes, &key.0); }
        bytes.extend_from_slice(&self.outlines.to_le_bytes());
        put_u32(&mut bytes, self.files.len());
        for (&name, digest) in self.files.iter() {
            put_name(&mut bytes, name);
            bytes.push(u8::from(digest.is_some()));
            if let Some(digest) = digest { put_id(&mut bytes, digest); }
        }
        put_u32(&mut bytes, self.places.len());
        for (&file, place) in self.places.iter() {
            put_name(&mut bytes, file);
            put_name(&mut bytes, place.unit);
            bytes.push(place.edition);
            bytes.push(place.flags);
        }
        put_u32(&mut bytes, self.units.len());
        for &name in self.units.iter() { put_name(&mut bytes, name); }
        put_u32(&mut bytes, self.findings.len());
        for (&name, &count) in self.findings.iter() { put_name(&mut bytes, name); bytes.extend_from_slice(&count.to_le_bytes()); }
        put_u32(&mut bytes, self.owed_tasks.len());
        for plan in self.owed_tasks.values() { put_plan(&mut bytes, plan); }
        put_u32(&mut bytes, self.owed_cells.len());
        for (id, &at) in self.owed_cells.iter() { put_cell_id(&mut bytes, id); bytes.extend_from_slice(&at.0.to_le_bytes()); }
        let root = content_key(&bytes);
        puts.push((root, bytes));
        Encoded {
            serial: self.serial, bits: self.bits, bytes: self.bytes, root, pages: self.pages, puts, plant, records,
            files: self.outlines, findings: self.findings.len() as u32, owed: u32::try_from(self.owed_tasks.len() + self.owed_cells.len()).unwrap_or(u32::MAX),
            changed_entries: 0, changed_pages: 0,
        }
    }
}

fn ordered<T: Copy + Ord>(prev: &mut Option<T>, value: T, at: usize, section: &'static str) -> Result<(), PagesError> {
    if prev.is_some_and(|last| last >= value) {
        return Err(PagesError::Unsorted { page: ROOT_PAGE, section, at: at as u32 });
    }
    *prev = Some(value);
    Ok(())
}

pub(super) fn read_directory<'a>(mut r: Rd<'_>, serial: Gen, bits: u8, total: u64, pages: SVec<'a, Key>, alloc: SharedAlloc<'a>) -> Result<Root<'a>, PagesError> {
    let mut root = empty_directory(alloc);
    root.serial = serial;
    root.bits = bits;
    root.bytes = total;
    root.pages = pages;
    root.outlines = r.u32()?;
    let mut prev = None;
    for _ in 0..r.count(NAME + 1)? {
        let at = r.at;
        let name = r.name()?;
        ordered(&mut prev, name, at, "file directory")?;
        let value = match r.u8()? { 0 => None, 1 => Some(r.id()?), tag => return Err(r.tag(tag)) };
        root.files.insert(name, value);
    }
    prev = None;
    for _ in 0..r.count(FILE_PLACE)? {
        let at = r.at;
        let file = r.name()?;
        ordered(&mut prev, file, at, "file places directory")?;
        let place = FilePlace { unit: r.name()?, edition: r.u8()?, flags: r.u8()? };
        // A place is of a file the root plans (the writer keeps no other): any other is a root
        // of another form.
        if !root.files.contains_key(&file) {
            return Err(PagesError::Unsorted { page: ROOT_PAGE, section: "file places directory (a place of no planned file)", at: at as u32 });
        }
        root.places.insert(file, place);
    }
    prev = None;
    for _ in 0..r.count(NAME)? {
        let n = r.name()?;
        ordered(&mut prev, n, r.at - NAME, "units directory")?;
        root.units.push(n);
    }
    prev = None;
    for _ in 0..r.count(NAME + 4)? {
        let n = r.name()?;
        ordered(&mut prev, n, r.at - NAME, "findings directory")?;
        root.findings.insert(n, r.u32()?);
    }
    prev = None;
    for _ in 0..r.count(OWED_TASK)? {
        let name = r.name()?;
        ordered(&mut prev, name, r.at - NAME, "owed tasks directory")?;
        root.owed_tasks.insert(name, Planned { name, phase: r.phase()?, source: r.id()?, file: r.name()? });
    }
    let mut prev = None;
    for _ in 0..r.count(OWED_CELL)? {
        let id = r.cell_id()?;
        ordered(&mut prev, id, r.at - CELL_ID, "owed cells directory")?;
        root.owed_cells.insert(id, Gen(r.u64()?));
    }
    if r.at != r.bytes.len() { return Err(PagesError::Trailing { page: ROOT_PAGE, at: r.at as u32 }); }
    Ok(root)
}

// A section is keyed by its name or cell id. For a name the unused cell bytes are zero.
// CellId's ordering is exactly the ordering of the wire keys.
fn named(name: Name) -> CellId { CellId { of: name, cell: Cell([0; CELL]) } }

type Writes<'a> = SMap<'a, CellId, Option<SVec<'a, u8>>>;
struct ChangedPage<'a> {
    witness: Name,
    sections: [Writes<'a>; SECTIONS],
}
struct Changed<'a> {
    bits: u8,
    pages: SMap<'a, u32, ChangedPage<'a>>,
    alloc: SharedAlloc<'a>,
}
impl<'a> Changed<'a> {
    fn set(&mut self, section: usize, key: CellId, value: Option<SVec<'a, u8>>) {
        let alloc = self.alloc;
        // A readers entry is on its cell's own page, every other on its task's (the form).
        let placed = if section == READERS { readers_slot(&key) } else { key.of };
        let page = self.pages.entry(page_of(placed, self.bits)).or_insert_with(|| ChangedPage {
            witness: placed, sections: core::array::from_fn(|_| BTreeMap::new_in(alloc)),
        });
        page.sections[section].insert(key, value);
    }
}

fn bytes_of_cap<'a>(alloc: SharedAlloc<'a>, cap: usize, write: impl FnOnce(&mut SVec<'a, u8>)) -> SVec<'a, u8> {
    let mut bytes = SVec::with_capacity_in(cap, alloc);
    write(&mut bytes);
    bytes
}

fn row_byte_len(row: &Row<'_>) -> usize {
    let indexed = row.by_cell.iter().enumerate().any(|(i, &at)| i != usize::try_from(at).unwrap_or(usize::MAX));
    ROW_FIXED + DEFINE * row.defines.len() + READ * row.reads.len() + if indexed { 4 * row.by_cell.len() } else { 0 }
}

/// Fast upper bound on bytes added by `work` and `kept` (used to decide whether the merged
/// answer might outgrow `last.bits`).
fn estimate_added_bytes(kept: &Kept<'_>, work: &Work<'_>) -> u64 {
    let mut added: u64 = 0;
    for row in work.rows.values().flatten() {
        added = added.saturating_add(u64::try_from(row_byte_len(row)).unwrap_or(u64::MAX));
    }
    let cells = work.cells.iter().filter(|(_, v)| v.is_some()).count();
    added = added.saturating_add(u64::try_from(CELL_ENTRY.saturating_mul(cells)).unwrap_or(u64::MAX));
    let owed = OWED_CELL.saturating_mul(work.owed_cells.len()).saturating_add(OWED_TASK.saturating_mul(work.owed_tasks.len()));
    added = added.saturating_add(u64::try_from(owed).unwrap_or(u64::MAX));
    for names in work.by_file.values().flatten() {
        let len = NAME.saturating_add(4).saturating_add(FILE_MEMBER.saturating_mul(names.len()));
        added = added.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
    }
    for (_, changes) in work.readers.iter() {
        let adds = changes.iter().filter(|&(_, &add)| add).count();
        if adds > 0 {
            let len = CELL_ID.saturating_add(4).saturating_add(NAME.saturating_mul(adds));
            added = added.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
        }
    }
    let extra = CAUSE.saturating_mul(kept.causes.len()).saturating_add((NAME + ID).saturating_mul(kept.records.len()));
    added.saturating_add(u64::try_from(extra).unwrap_or(u64::MAX))
}

/// A synthetic [`Name`] whose [`page_of`] at `bits` is `index`.
fn page_witness(index: usize, bits: u8) -> Name {
    let mut subject = [0u8; 32];
    let shift = 16u32.saturating_sub(u32::from(bits.min(blaze_drive::answers::MOST_BITS)));
    let lead = u16::try_from(index << shift).unwrap_or(0);
    if let Some(head) = subject.get_mut(0..2) {
        head.copy_from_slice(&lead.to_be_bytes());
    }
    Name::new(Kind::Units, ContentId(subject))
}

fn collect_old_entries<'a, 'p>(old_pages: &[&'p VerifiedPage<'_>], section: usize, alloc: SharedAlloc<'a>) -> SVec<'a, (CellId, &'p [u8])> {
    let mut out = SVec::new_in(alloc);
    for page in old_pages {
        for bytes in page.entries(section) {
            out.push((key_of_entry(bytes, section), bytes));
        }
    }
    out.sort_unstable_by_key(|(k, _)| *k);
    out
}

fn write_at<'a>(bufs: &mut [PageBuf<'a>], bits: u8, name: Name, section: usize, write: impl FnOnce(&mut SVec<'a, u8>)) {
    let idx = usize::try_from(page_of(name, bits)).unwrap_or(0);
    if let Some(buf) = bufs.get_mut(idx) {
        write(buf.entry(section));
    }
}

/// Re-pages `(last, work)` into `new_bits > last.bits` when the answer has outgrown `last.bits`
/// (for example, a cold workspace starting from a tiny `tool-base` with `bits = 0..2`). Writes
/// directly into `1 << new_bits` [`PageBuf`]s via sorted 2-way merges without allocating
/// per-entry intermediate `SVec`s. Returns `Ok(None)` if exact merged sizing still fits `last.bits`.
fn try_repage<'a>(kept: &Kept<'_>, last: &Root<'_>, source: &dyn StoredPages, work: &Work<'_>, alloc: SharedAlloc<'a>) -> Result<Option<Encoded<'a>>, PagesError> {
    let mut old_pages: SVec<'a, &VerifiedPage<'_>> = SVec::with_capacity_in(last.pages.len(), alloc);
    for (idx, &last_key) in last.pages.iter().enumerate() {
        if last_key == Key::default() {
            continue;
        }
        let witness = page_witness(idx, last.bits);
        let Some(page) = source.page_for(witness, Ask::Witness)? else {
            return Err(PagesError::Missing { page: u32::try_from(idx).unwrap_or(u32::MAX), key: last_key });
        };
        old_pages.push(page);
    }

    let mut root = empty_directory(alloc);
    root.serial = work.serial();
    root.outlines = last.outlines;
    root.files.extend(last.files.iter().map(|(&name, &digest)| (name, digest)));
    root.places.extend(last.places.iter().map(|(&name, &place)| (name, place)));
    root.findings.extend(last.findings.iter().map(|(&name, &count)| (name, count)));
    let mut units = crate::SSet::new_in(alloc);
    units.extend(last.units.iter().copied());
    let mut files = crate::SSet::new_in(alloc);
    files.extend(work.by_file.keys().copied());
    let mut record_updates: SMap<'a, CellId, Option<Key>> = SMap::new_in(alloc);
    let mut cause_updates: SMap<'a, CellId, Option<(Gen, Cause)>> = SMap::new_in(alloc);

    for (&name, value) in work.rows.iter() {
        let old = previous(source, name)?;
        let next = value.as_ref();
        let plan = next.map(|r| Planned { name, phase: r.phase, file: r.file, source: r.source });
        if old.map(|facts| facts.planned(name)) != plan || old.map(|r| r.key) != next.map(|r| r.key) {
            if let Some(old) = old { files.insert(old.file); }
            if let Some(plan) = plan { files.insert(plan.file); }
        }
        if old.is_some_and(|r| r.phase.stage == Stage::Outlines) {
            root.outlines = root.outlines.checked_sub(1).ok_or(PagesError::Total { page: page_of(name, last.bits), field: "outline count" })?;
        }
        root.outlines = root.outlines.checked_add(u32::from(next.is_some_and(|r| r.phase.stage == Stage::Outlines)))
            .ok_or(PagesError::Total { page: page_of(name, last.bits), field: "outline count" })?;
        if next.is_some_and(|r| r.phase.stage == Stage::Units) { units.insert(name); } else { units.remove(&name); }
        match next.filter(|r| r.failed || r.findings > 0) {
            Some(r) => { root.findings.insert(name, r.findings); }
            None => { root.findings.remove(&name); }
        }
        let k = named(name);
        record_updates.insert(k, next.and_then(|_| kept.records.get(&name).copied()));
        if next.is_none() {
            cause_updates.insert(k, None);
        } else if let Some(&c) = kept.causes.get(&name) {
            cause_updates.insert(k, Some(c));
        }
    }
    for (&name, &cause) in kept.causes.iter() {
        if !work.rows.contains_key(&name) && previous(source, name)?.is_some() {
            let k = named(name);
            cause_updates.insert(k, Some(cause));
            record_updates.insert(k, kept.records.get(&name).copied());
        }
    }
    root.units.extend(units.iter().copied());
    for (&name, &old) in last.owed_tasks.iter() {
        if !work.owed_tasks.contains_key(&name) {
            files.insert(old.file);
            if let Some(row) = previous(source, name)? { files.insert(row.file); }
        }
    }
    for (&name, &plan) in work.owed_tasks.iter() {
        if last.owed_tasks.get(&name) != Some(&plan) {
            files.insert(plan.file);
            if let Some(old) = previous(source, name)? { files.insert(old.file); }
        }
        root.owed_tasks.insert(name, plan);
    }
    for (&id, &moved) in work.owed_cells.iter() {
        root.owed_cells.insert(id, moved);
    }

    let mut file_updates: SMap<'a, CellId, Option<SVec<'a, u8>>> = SMap::new_in(alloc);
    for &file in files.iter() {
        let replacement = work.by_file.get(&file);
        if matches!(replacement, Some(None)) {
            if root.files.remove(&file).is_some() { file_updates.insert(named(file), None); }
            continue;
        }
        if replacement.is_none() && !last.files.contains_key(&file) { continue; }
        let base = source.page_for(file, Ask::File)?;
        let old_members = base.map_or(&[][..], |page| page.file_members(file));
        let count = replacement.and_then(|s| s.as_ref()).map_or(old_members.len() / FILE_MEMBER, |names| names.len());
        let mut record = SVec::with_capacity_in(NAME + 4 + count * FILE_MEMBER, alloc);
        put_name(&mut record, file);
        put_u32(&mut record, count);
        let mut all_stand = true;
        let mut member = |name: Name| -> Result<(), PagesError> {
            if !work.rows.contains_key(&name) && last.owed_tasks.get(&name) == work.owed_tasks.get(&name)
                && let Some(saved) = VerifiedPage::member(old_members, name)
            {
                all_stand &= saved.get(NAME).copied() == Some(1);
                record.extend_from_slice(saved);
                return Ok(());
            }
            let claim = if work.owed_tasks.contains_key(&name) {
                None
            } else {
                match work.rows.get(&name) {
                    Some(row) => row.as_ref().and_then(|row| claim(row, file, false)),
                    None => previous(source, name)?.filter(|r| r.file == file).map(|r| Claim { key: r.key, phase: r.phase, source: r.source }),
                }
            };
            all_stand &= claim.is_some();
            put_member(&mut record, name, claim);
            Ok(())
        };
        if let Some(Some(names)) = replacement {
            for &name in names.iter() { member(name)?; }
        } else if let Some(base) = base {
            for name in base.file_tasks(file) { member(name)?; }
        }
        root.files.insert(file, all_stand.then(|| file_digest(&record)));
        file_updates.insert(named(file), Some(record));
    }
    root.places.extend(kept.places.iter().map(|(&name, &place)| (name, place)));
    let planned = &root.files;
    root.places.retain(|file, _| planned.contains_key(file));

    // Collect old entries per section and compute exact merged byte total (matching `pages::encode`).
    let old_rows = collect_old_entries(&old_pages, ROWS, alloc);
    let old_cells = collect_old_entries(&old_pages, CELLS, alloc);
    let old_files = collect_old_entries(&old_pages, FILES, alloc);
    let old_causes = collect_old_entries(&old_pages, CAUSES, alloc);
    let old_records = collect_old_entries(&old_pages, RECORDS, alloc);
    let old_readers = collect_old_entries(&old_pages, READERS, alloc);

    let mut total: u64 = 0;
    for &(k, b) in old_rows.iter() {
        if !work.rows.contains_key(&k.of) {
            total = total.saturating_add(u64::try_from(b.len()).unwrap_or(u64::MAX));
        }
    }
    for row in work.rows.values().flatten() {
        let len = ROW_FIXED + DEFINE * row.defines.len() + READ * row.reads.len();
        total = total.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
    }
    let kept_old_cells = old_cells.iter().filter(|(k, _)| !work.cells.contains_key(k)).count();
    let new_cells = work.cells.iter().filter(|(_, v)| v.is_some()).count();
    total = total.saturating_add(u64::try_from(CELL_ENTRY * (kept_old_cells + new_cells) + OWED_CELL * root.owed_cells.len() + OWED_TASK * root.owed_tasks.len()).unwrap_or(u64::MAX));
    for &(k, b) in old_files.iter() {
        if !file_updates.contains_key(&k) {
            total = total.saturating_add(u64::try_from(b.len()).unwrap_or(u64::MAX));
        }
    }
    for rec in file_updates.values().flatten() {
        total = total.saturating_add(u64::try_from(rec.len()).unwrap_or(u64::MAX));
    }
    let kept_old_causes = old_causes.iter().filter(|(k, _)| !cause_updates.contains_key(k)).count();
    let new_causes = cause_updates.values().filter(|v| v.is_some()).count();
    total = total.saturating_add(u64::try_from(CAUSE * (kept_old_causes + new_causes)).unwrap_or(u64::MAX));
    let kept_old_records = old_records.iter().filter(|(k, _)| !record_updates.contains_key(k)).count();
    let new_records = record_updates.values().filter(|v| v.is_some()).count();
    total = total.saturating_add(u64::try_from((NAME + ID) * (kept_old_records + new_records)).unwrap_or(u64::MAX));
    let mut old_reader_iter = old_readers.iter().peekable();
    for (&id, changes) in work.readers.iter() {
        while let Some(&&(ok, ob)) = old_reader_iter.peek() && ok < id {
            total = total.saturating_add(u64::try_from(ob.len()).unwrap_or(u64::MAX));
            old_reader_iter.next();
        }
        let mut old_count = 0usize;
        if let Some(&&(ok, ob)) = old_reader_iter.peek() && ok == id {
            old_count = (ob.len().saturating_sub(CELL_ID + 4)) / NAME;
            old_reader_iter.next();
        }
        let adds = changes.iter().filter(|&(_, &add)| add).count();
        let removes = changes.iter().filter(|&(_, &add)| !add).count();
        let est = old_count.saturating_add(adds).saturating_sub(removes);
        if est > 0 {
            total = total.saturating_add(u64::try_from(CELL_ID + 4 + NAME * est).unwrap_or(u64::MAX));
        }
    }
    for &(_, ob) in old_reader_iter {
        total = total.saturating_add(u64::try_from(ob.len()).unwrap_or(u64::MAX));
    }

    let new_bits = bits_kept(total, Some(last.bits));
    if new_bits == last.bits {
        return Ok(None);
    }

    let count = 1usize << new_bits;
    let mut bufs: SVec<'a, PageBuf<'a>> = SVec::with_capacity_in(count, alloc);
    for _ in 0..count {
        bufs.push(PageBuf::new_in(alloc));
    }

    // 1. ROWS
    {
        let mut updates = work.rows.iter().peekable();
        for &(k, old_bytes) in old_rows.iter() {
            while let Some(&(&name, val)) = updates.peek() && named(name) < k {
                updates.next();
                if let Some(row) = val {
                    write_at(&mut bufs, new_bits, name, ROWS, |out| put_row(out, name, row));
                }
            }
            if let Some(&(&name, val)) = updates.peek() && named(name) == k {
                updates.next();
                if let Some(row) = val {
                    write_at(&mut bufs, new_bits, name, ROWS, |out| put_row(out, name, row));
                }
            } else {
                write_at(&mut bufs, new_bits, k.of, ROWS, |out| out.extend_from_slice(old_bytes));
            }
        }
        for (&name, val) in updates {
            if let Some(row) = val {
                write_at(&mut bufs, new_bits, name, ROWS, |out| put_row(out, name, row));
            }
        }
    }

    // 2. CELLS
    {
        let mut updates = work.cells.iter().peekable();
        for &(k, old_bytes) in old_cells.iter() {
            while let Some(&(&id, val)) = updates.peek() && id < k {
                updates.next();
                if let Some((digest, moved)) = val {
                    write_at(&mut bufs, new_bits, id.of, CELLS, |out| {
                        put_cell_id(out, &id);
                        put_id(out, digest);
                        out.extend_from_slice(&moved.0.to_le_bytes());
                    });
                }
            }
            if let Some(&(&id, val)) = updates.peek() && id == k {
                updates.next();
                if let Some((digest, moved)) = val {
                    write_at(&mut bufs, new_bits, id.of, CELLS, |out| {
                        put_cell_id(out, &id);
                        put_id(out, digest);
                        out.extend_from_slice(&moved.0.to_le_bytes());
                    });
                }
            } else {
                write_at(&mut bufs, new_bits, k.of, CELLS, |out| out.extend_from_slice(old_bytes));
            }
        }
        for (&id, val) in updates {
            if let Some((digest, moved)) = val {
                write_at(&mut bufs, new_bits, id.of, CELLS, |out| {
                    put_cell_id(out, &id);
                    put_id(out, digest);
                    out.extend_from_slice(&moved.0.to_le_bytes());
                });
            }
        }
    }

    // 3. OWED_CELLS & 4. OWED_TASKS (complete sets live in `root`)
    for (id, moved) in root.owed_cells.iter() {
        write_at(&mut bufs, new_bits, id.of, OWED_CELLS, |out| {
            put_cell_id(out, id);
            out.extend_from_slice(&moved.0.to_le_bytes());
        });
    }
    for (&name, plan) in root.owed_tasks.iter() {
        write_at(&mut bufs, new_bits, name, OWED_TASKS, |out| put_plan(out, plan));
    }

    // 5. FILES
    {
        let mut updates = file_updates.iter().peekable();
        for &(k, old_bytes) in old_files.iter() {
            while let Some(&(&uk, val)) = updates.peek() && uk < k {
                updates.next();
                if let Some(rec) = val {
                    write_at(&mut bufs, new_bits, uk.of, FILES, |out| out.extend_from_slice(rec));
                }
            }
            if let Some(&(&uk, val)) = updates.peek() && uk == k {
                updates.next();
                if let Some(rec) = val {
                    write_at(&mut bufs, new_bits, uk.of, FILES, |out| out.extend_from_slice(rec));
                }
            } else {
                write_at(&mut bufs, new_bits, k.of, FILES, |out| out.extend_from_slice(old_bytes));
            }
        }
        for (&uk, val) in updates {
            if let Some(rec) = val {
                write_at(&mut bufs, new_bits, uk.of, FILES, |out| out.extend_from_slice(rec));
            }
        }
    }

    // 6. CAUSES
    {
        let mut updates = cause_updates.iter().peekable();
        for &(k, old_bytes) in old_causes.iter() {
            while let Some(&(&uk, val)) = updates.peek() && uk < k {
                updates.next();
                if let Some((serial, cause)) = val {
                    write_at(&mut bufs, new_bits, uk.of, CAUSES, |out| put_cause(out, uk.of, *serial, *cause));
                }
            }
            if let Some(&(&uk, val)) = updates.peek() && uk == k {
                updates.next();
                if let Some((serial, cause)) = val {
                    write_at(&mut bufs, new_bits, uk.of, CAUSES, |out| put_cause(out, uk.of, *serial, *cause));
                }
            } else {
                write_at(&mut bufs, new_bits, k.of, CAUSES, |out| out.extend_from_slice(old_bytes));
            }
        }
        for (&uk, val) in updates {
            if let Some((serial, cause)) = val {
                write_at(&mut bufs, new_bits, uk.of, CAUSES, |out| put_cause(out, uk.of, *serial, *cause));
            }
        }
    }

    // 7. RECORDS
    {
        let mut updates = record_updates.iter().peekable();
        for &(k, old_bytes) in old_records.iter() {
            while let Some(&(&uk, val)) = updates.peek() && uk < k {
                updates.next();
                if let Some(key) = val {
                    write_at(&mut bufs, new_bits, uk.of, RECORDS, |out| {
                        put_name(out, uk.of);
                        put_id(out, &key.0);
                    });
                }
            }
            if let Some(&(&uk, val)) = updates.peek() && uk == k {
                updates.next();
                if let Some(key) = val {
                    write_at(&mut bufs, new_bits, uk.of, RECORDS, |out| {
                        put_name(out, uk.of);
                        put_id(out, &key.0);
                    });
                }
            } else {
                write_at(&mut bufs, new_bits, k.of, RECORDS, |out| out.extend_from_slice(old_bytes));
            }
        }
        for (&uk, val) in updates {
            if let Some(key) = val {
                write_at(&mut bufs, new_bits, uk.of, RECORDS, |out| {
                    put_name(out, uk.of);
                    put_id(out, &key.0);
                });
            }
        }
    }

    // 8. READERS (using one reusable scratch buffer across all changed cells)
    {
        let mut scratch: SVec<'a, u8> = SVec::new_in(alloc);
        let write_merged = |bufs: &mut [PageBuf<'a>], scratch: &mut SVec<'a, u8>, id: CellId, old_bytes: Option<&[u8]>, changes: crate::readers::Edits<'_>| {
            scratch.clear();
            put_cell_id(scratch, &id);
            put_u32(scratch, 0);
            let mut ch = changes.iter().peekable();
            if let Some(tail) = old_bytes.and_then(|bytes| bytes.get(CELL_ID + 4..)) {
                for chunk in tail.chunks_exact(NAME) {
                    let mut r = Rd { bytes: chunk, at: 0, page: 0 };
                    let Ok(reader) = r.name() else { continue };
                    while ch.peek().is_some_and(|item| *item.0 < reader) {
                        if let Some((&name, &true)) = ch.next() { put_name(scratch, name); }
                    }
                    if ch.peek().is_some_and(|item| *item.0 == reader) {
                        if let Some((&name, &true)) = ch.next() { put_name(scratch, name); }
                    } else {
                        put_name(scratch, reader);
                    }
                }
            }
            for (&name, &add) in ch {
                if add { put_name(scratch, name); }
            }
            let count = (scratch.len() - CELL_ID - 4) / NAME;
            if count > 0 {
                let count_u32 = u32::try_from(count).unwrap_or(u32::MAX);
                if let Some(slot) = scratch.get_mut(CELL_ID..CELL_ID + 4) {
                    slot.copy_from_slice(&count_u32.to_le_bytes());
                }
                write_at(bufs, new_bits, readers_slot(&id), READERS, |out| out.extend_from_slice(scratch));
            }
        };
        let mut updates = work.readers.iter().peekable();
        for &(k, old_bytes) in old_readers.iter() {
            while let Some(&(&id, changes)) = updates.peek() && id < k {
                updates.next();
                write_merged(&mut bufs, &mut scratch, id, None, changes);
            }
            if let Some(&(&id, changes)) = updates.peek() && id == k {
                updates.next();
                write_merged(&mut bufs, &mut scratch, id, Some(old_bytes), changes);
            } else {
                write_at(&mut bufs, new_bits, readers_slot(&k), READERS, |out| out.extend_from_slice(old_bytes));
            }
        }
        for (&id, changes) in updates {
            write_merged(&mut bufs, &mut scratch, id, None, changes);
        }
    }

    root.bits = new_bits;
    root.bytes = 0;
    root.pages = SVec::with_capacity_in(count, alloc);
    let mut puts = SVec::new_in(alloc);
    let mut records = SVec::new_in(alloc);
    for (index, buf) in bufs.iter().enumerate() {
        let idx_u32 = u32::try_from(index).unwrap_or(u32::MAX);
        match buf.finish(new_bits, idx_u32, alloc) {
            Some(bytes) => {
                let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                root.bytes = root.bytes.checked_add(len).ok_or(PagesError::Total { page: idx_u32, field: "page byte count" })?;
                let key = content_key(&bytes);
                root.pages.push(key);
                let rec_bytes = buf.parts.get(RECORDS).map_or(&[][..], |part| part.as_slice());
                let page_records = view::page_records(rec_bytes, alloc);
                puts.push((key, bytes));
                records.push((key, page_records));
            }
            None => root.pages.push(Key::default()),
        }
    }
    Ok(Some(root.finish(puts, records, PagesPlant::None, alloc)))
}

fn key_of_entry(bytes: &[u8], section: usize) -> CellId {
    let mut r = Rd { bytes, at: 0, page: 0 };
    if view::key_width(section) == NAME {
        named(r.name().expect("the writer merges authenticated entries"))
    } else {
        r.cell_id().expect("the writer merges authenticated entries")
    }
}

fn merge_section<'b, 'c>(
    base: Option<&'b VerifiedPage<'b>>,
    changes: &'c Writes<'c>,
    section: usize,
    mut emit: impl FnMut(&[u8]),
) {
    let mut edits = changes.iter().peekable();
    for bytes in base.into_iter().flat_map(|p| p.entries(section)) {
        let key = key_of_entry(bytes, section);
        while edits.peek().is_some_and(|item| *item.0 < key) {
            if let Some((_, Some(replacement))) = edits.next() {
                emit(replacement);
            }
        }
        if edits.peek().is_some_and(|item| *item.0 == key) {
            if let Some((_, Some(replacement))) = edits.next() {
                emit(replacement);
            }
        } else {
            emit(bytes);
        }
    }
    for (_, value) in edits {
        if let Some(replacement) = value {
            emit(replacement);
        }
    }
}

/// Merge the old page's entries with sorted replacements and deletions in each column.
/// Unchanged sections are copied in one contiguous slice; changed sections are merged directly
/// into one exact-sized output buffer without intermediate `PageBuf` reallocations.
fn merge_page<'a>(base: Option<&VerifiedPage<'_>>, change: &ChangedPage<'_>, bits: u8, index: u32, alloc: SharedAlloc<'a>) -> (SVec<'a, u8>, SVec<'a, (Name, Key)>) {
    let mut counts = [0usize; SECTIONS];
    let mut body_len = 0usize;
    for (section, (section_changes, count_slot)) in change.sections.iter().zip(counts.iter_mut()).enumerate() {
        if section_changes.is_empty() {
            if let Some(base) = base {
                *count_slot = base.section_count(section);
                body_len += base.section_bytes(section).len();
            }
        } else {
            merge_section(base, section_changes, section, |bytes| {
                *count_slot += 1;
                body_len += bytes.len();
            });
        }
    }
    let entries: usize = counts.iter().sum();
    if entries == 0 {
        return (SVec::new_in(alloc), SVec::new_in(alloc));
    }
    let body_at = starts_end(entries);
    let total_len = body_at + body_len;
    let mut page: SVec<'a, u8> = SVec::with_capacity_in(total_len, alloc);
    page.extend_from_slice(PAGE_MAGIC);
    page.push(bits);
    page.extend_from_slice(&index.to_le_bytes());
    for count in counts {
        put_u32(&mut page, count);
    }
    let mut at = body_at;
    for (section, section_changes) in change.sections.iter().enumerate() {
        if section_changes.is_empty() {
            if let Some(base) = base {
                let count = base.section_count(section);
                if count > 0 {
                    let base_start = base.section_start(section);
                    for j in 0..count {
                        put_u32(&mut page, at + (base.section_entry_start(section, j) - base_start));
                    }
                    at += base.section_bytes(section).len();
                }
            }
        } else {
            merge_section(base, section_changes, section, |bytes| {
                put_u32(&mut page, at);
                at += bytes.len();
            });
        }
    }
    put_u32(&mut page, total_len);
    let mut records: SVec<'a, (Name, Key)> = SVec::with_capacity_in(counts.get(RECORDS).copied().unwrap_or(0), alloc);
    for (section, section_changes) in change.sections.iter().enumerate() {
        if section_changes.is_empty() {
            if let Some(base) = base {
                page.extend_from_slice(base.section_bytes(section));
                if section == RECORDS {
                    records.extend(base.records());
                }
            }
        } else {
            merge_section(base, section_changes, section, |bytes| {
                page.extend_from_slice(bytes);
                if section == RECORDS && let Some(rest) = bytes.get(NAME..) {
                    records.push((view::name(bytes), Key(view::id(rest))));
                }
            });
        }
    }
    debug_assert_eq!(page.len(), total_len, "a merged page's start table ends at its length");
    (page, records)
}

/// One changed page ready to be merged and content-hashed independently of every other page
/// ([`encode_changes_with`]).
pub struct PageJob<'b, 'c> {
    index: u32,
    base: Option<&'b VerifiedPage<'b>>,
    change: &'c ChangedPage<'c>,
    bits: u8,
    last_key: Key,
}

/// The merged and hashed result of one [`PageJob`].
pub struct PageBuilt<'a> {
    index: u32,
    base_len: u64,
    new_len: u64,
    key: Key,
    bytes: SVec<'a, u8>,
    records: SVec<'a, (Name, Key)>,
    changed: bool,
}

impl PageJob<'_, '_> {
    /// Merge this page's base with its delta and compute its content key.
    pub fn run<'a>(&self, alloc: SharedAlloc<'a>) -> PageBuilt<'a> {
        let base_len = self.base.map_or(0, |p| u64::try_from(p.len()).unwrap_or(u64::MAX));
        let (bytes, records) = merge_page(self.base, self.change, self.bits, self.index, alloc);
        let new_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let key = if bytes.is_empty() { Key::default() } else { content_key(&bytes) };
        let changed = key != Key::default() && key != self.last_key;
        let (bytes, records) = if changed {
            (bytes, records)
        } else {
            (SVec::new_in(alloc), SVec::new_in(alloc))
        };
        PageBuilt { index: self.index, base_len, new_len, key, bytes, records, changed }
    }
}

fn previous(source: &dyn StoredPages, name: Name) -> Result<Option<RowFacts>, PagesError> {
    Ok(source.page_for(name, Ask::Row)?.and_then(|page| page.row(name)).map(|row| row.facts))
}

/// Writes for the sparse owed columns. Their whole sets are in the directory for
/// enumerating them and are compared here to avoid dirtying a page for standing debt.
fn obligations<'a>(root: &mut Root<'a>, last: &Root<'_>, work: &Work<'_>, source: &dyn StoredPages, delta: &mut Changed<'a>, files: &mut crate::SSet<'a, Name>) -> Result<(), PagesError> {
    let alloc = delta.alloc;
    for (&name, &old) in last.owed_tasks.iter() {
        if !work.owed_tasks.contains_key(&name) {
            delta.set(OWED_TASKS, named(name), None);
            files.insert(old.file);
            if let Some(row) = previous(source, name)? { files.insert(row.file); }
        }
    }
    for (&name, &plan) in work.owed_tasks.iter() {
        if last.owed_tasks.get(&name) != Some(&plan) {
            delta.set(OWED_TASKS, named(name), Some(bytes_of_cap(alloc, OWED_TASK, |b| put_plan(b, &plan))));
            files.insert(plan.file);
            if let Some(old) = previous(source, name)? { files.insert(old.file); }
        }
        root.owed_tasks.insert(name, plan);
    }
    for &id in last.owed_cells.keys() {
        if !work.owed_cells.contains_key(&id) { delta.set(OWED_CELLS, id, None); }
    }
    for (&id, &moved) in work.owed_cells.iter() {
        if last.owed_cells.get(&id) != Some(&moved) {
            delta.set(OWED_CELLS, id, Some(bytes_of_cap(alloc, OWED_CELL, |b| { put_cell_id(b, &id); b.extend_from_slice(&moved.0.to_le_bytes()); })));
        }
        root.owed_cells.insert(id, moved);
    }
    Ok(())
}

/// Copy unchanged member claims from the file's page. Only an added member or a
/// changed row/obligation needs to consult the row under its own name.
fn file_changes<'a>(root: &mut Root<'a>, last: &Root<'_>, work: &Work<'_>, source: &dyn StoredPages, delta: &mut Changed<'a>, files: &crate::SSet<'a, Name>) -> Result<(), PagesError> {
    for &file in files.iter() {
        let replacement = work.by_file.get(&file);
        if matches!(replacement, Some(None)) {
            if root.files.remove(&file).is_some() { delta.set(FILES, named(file), None); }
            continue;
        }
        if replacement.is_none() && !last.files.contains_key(&file) { continue; }
        let base = source.page_for(file, Ask::File)?;
        let old_members = base.map_or(&[][..], |page| page.file_members(file));
        let count = replacement.and_then(|s| s.as_ref()).map_or(old_members.len() / FILE_MEMBER, |names| names.len());
        let mut record = Vec::with_capacity_in(NAME + 4 + count * FILE_MEMBER, delta.alloc);
        put_name(&mut record, file);
        put_u32(&mut record, count);
        let mut all_stand = true;
        let mut member = |name: Name| -> Result<(), PagesError> {
            if !work.rows.contains_key(&name) && last.owed_tasks.get(&name) == work.owed_tasks.get(&name)
                && let Some(saved) = VerifiedPage::member(old_members, name)
            {
                all_stand &= saved[NAME] == 1;
                record.extend_from_slice(saved);
                return Ok(());
            }
            let claim = if work.owed_tasks.contains_key(&name) {
                None
            } else {
                match work.rows.get(&name) {
                    Some(row) => row.as_ref().and_then(|row| claim(row, file, false)),
                    None => previous(source, name)?.filter(|r| r.file == file).map(|r| Claim { key: r.key, phase: r.phase, source: r.source }),
                }
            };
            all_stand &= claim.is_some();
            put_member(&mut record, name, claim);
            Ok(())
        };
        if let Some(Some(names)) = replacement {
            for &name in names.iter() { member(name)?; }
        } else if let Some(base) = base {
            for name in base.file_tasks(file) { member(name)?; }
        }
        root.files.insert(file, all_stand.then(|| file_digest(&record)));
        if !last.files.contains_key(&file) || old_members != &record[NAME + 4..] {
            delta.set(FILES, named(file), Some(record));
        }
    }
    Ok(())
}

fn reader_changes<'a>(work: &Work<'_>, source: &dyn StoredPages, delta: &mut Changed<'a>) -> Result<(), PagesError> {
    for (&id, changes) in work.readers.iter() {
        let old_page = source.page_for(readers_slot(&id), Ask::Readers)?;
        let old_raw = old_page.and_then(|p| p.readers_raw(&id));
        let adds = changes.iter().filter(|&(_, &add)| add).count();
        let mut changes = changes.iter().peekable();
        let cap = old_raw.map_or(CELL_ID + 4, |raw| raw.len()).saturating_add(NAME * adds);
        let mut readers = SVec::with_capacity_in(cap, delta.alloc);
        put_cell_id(&mut readers, &id);
        put_u32(&mut readers, 0);
        for reader in old_page.into_iter().flat_map(|p| p.readers(&id)) {
            while changes.peek().is_some_and(|item| *item.0 < reader) {
                if let Some((&name, &true)) = changes.next() { put_name(&mut readers, name); }
            }
            if changes.peek().is_some_and(|item| *item.0 == reader) {
                if let Some((&name, &true)) = changes.next() { put_name(&mut readers, name); }
            } else {
                put_name(&mut readers, reader);
            }
        }
        for (&name, &add) in changes { if add { put_name(&mut readers, name); } }
        let count = (readers.len() - CELL_ID - 4) / NAME;
        let count = u32::try_from(count).expect("a cell's readers fit the section count");
        readers[CELL_ID..CELL_ID + 4].copy_from_slice(&count.to_le_bytes());
        // Skip dirtying the page if the merged readers entry is byte-identical to the old one.
        let unchanged = match (count == 0, old_raw) {
            (true, None) => true,
            (false, Some(raw)) => raw == readers.as_slice(),
            _ => false,
        };
        if !unchanged {
            delta.set(READERS, id, (count > 0).then_some(readers));
        }
    }
    Ok(())
}

/// Fold the request into the existing page directory. Only pages containing a changed
/// row, reader, cell, obligation or file plan can acquire a new key. When the merged answer
/// outgrows `last.bits` (e.g. a cold workspace starting from a small `tool-base`), re-pages
/// directly into `bits_kept(total, Some(last.bits))`.
pub fn encode_changes<'a>(kept: &Kept<'_>, last: &Root<'_>, alloc: SharedAlloc<'a>) -> Result<Encoded<'a>, PagesError> {
    encode_changes_with(kept, last, alloc, |jobs| {
        let mut out = SVec::with_capacity_in(jobs.len(), alloc);
        for job in jobs {
            out.push(job.run(alloc));
        }
        out
    })
}

/// [`encode_changes`] with a caller-supplied executor for independent [`PageJob`]s, letting a
/// multi-threaded driver merge and BLAKE3-hash changed pages in parallel while keeping this crate
/// sans-IO and deterministic.
pub fn encode_changes_with<'a, F>(kept: &Kept<'_>, last: &Root<'_>, alloc: SharedAlloc<'a>, run_pages: F) -> Result<Encoded<'a>, PagesError>
where
    F: FnOnce(&[PageJob<'_, '_>]) -> SVec<'a, PageBuilt<'a>>,
{
    let answer = &kept.answer;
    let source = answer.stored().ok_or(PagesError::Base { serial: answer.serial })?;
    let work = answer.work().ok_or(PagesError::Base { serial: answer.serial })?;
    let added = estimate_added_bytes(kept, work);
    let page_budget = blaze_drive::answers::PAGE_BYTES << last.bits.min(blaze_drive::answers::MOST_BITS);
    if added > page_budget / 2
        && blaze_drive::answers::bits_for(last.bytes.saturating_add(added)) > last.bits
        && let Some(encoded) = try_repage(kept, last, source, work, alloc)?
    {
        return Ok(encoded);
    }
    let mut delta = Changed { bits: last.bits, pages: BTreeMap::new_in(alloc), alloc };
    let mut root = empty_directory(alloc);
    root.serial = work.serial();
    root.bits = last.bits;
    root.bytes = last.bytes;
    root.pages.extend_from_slice(&last.pages);
    root.outlines = last.outlines;
    root.files.extend(last.files.iter().map(|(&name, &digest)| (name, digest)));
    root.places.extend(last.places.iter().map(|(&name, &place)| (name, place)));
    root.findings.extend(last.findings.iter().map(|(&name, &count)| (name, count)));
    let mut units = BTreeSet::new_in(alloc);
    units.extend(last.units.iter().copied());
    let mut files = BTreeSet::new_in(alloc);
    files.extend(work.by_file.keys().copied());
    for (&name, value) in work.rows.iter() {
        let old = previous(source, name)?;
        let next = value.as_ref();
        let plan = next.map(|r| Planned { name, phase: r.phase, file: r.file, source: r.source });
        if old.map(|facts| facts.planned(name)) != plan || old.map(|r| r.key) != next.map(|r| r.key) {
            if let Some(old) = old { files.insert(old.file); }
            if let Some(plan) = plan { files.insert(plan.file); }
        }
        if old.is_some_and(|r| r.phase.stage == Stage::Outlines) {
            root.outlines = root.outlines.checked_sub(1).ok_or(PagesError::Total { page: page_of(name, last.bits), field: "outline count" })?;
        }
        root.outlines = root.outlines.checked_add(u32::from(next.is_some_and(|r| r.phase.stage == Stage::Outlines)))
            .ok_or(PagesError::Total { page: page_of(name, last.bits), field: "outline count" })?;
        if next.is_some_and(|r| r.phase.stage == Stage::Units) { units.insert(name); } else { units.remove(&name); }
        match next.filter(|r| r.failed || r.findings > 0) {
            Some(r) => { root.findings.insert(name, r.findings); }
            None => { root.findings.remove(&name); }
        }
        let k = named(name);
        delta.set(ROWS, k, next.map(|row| bytes_of_cap(alloc, row_byte_len(row), |b| put_row(b, name, row))));
        let record = next.and_then(|_| kept.records.get(&name));
        delta.set(RECORDS, k, record.map(|key| bytes_of_cap(alloc, NAME + ID, |b| { put_name(b, name); put_id(b, &key.0); })));
        if next.is_none() {
            delta.set(CAUSES, k, None);
        } else if let Some(&(serial, cause)) = kept.causes.get(&name) {
            delta.set(CAUSES, k, Some(bytes_of_cap(alloc, CAUSE, |b| put_cause(b, name, serial, cause))));
        }
    }
    // An entered task whose completion kept its row still has a cause and may publish a record.
    for (&name, &(serial, cause)) in kept.causes.iter() {
        if !work.rows.contains_key(&name) && previous(source, name)?.is_some() {
            delta.set(CAUSES, named(name), Some(bytes_of_cap(alloc, CAUSE, |b| put_cause(b, name, serial, cause))));
            delta.set(RECORDS, named(name), kept.records.get(&name).map(|key| bytes_of_cap(alloc, NAME + ID, |b| { put_name(b, name); put_id(b, &key.0); })));
        }
    }
    root.units.extend(units.iter().copied());
    for (&id, value) in work.cells.iter() {
        delta.set(CELLS, id, value.map(|(digest, moved)| bytes_of_cap(alloc, CELL_ENTRY, |b| {
            put_cell_id(b, &id); put_id(b, &digest); b.extend_from_slice(&moved.0.to_le_bytes());
        })));
    }
    obligations(&mut root, last, work, source, &mut delta, &mut files)?;
    reader_changes(work, source, &mut delta)?;
    file_changes(&mut root, last, work, source, &mut delta, &files)?;
    // This request's places over the last root's, and none of a file the root no longer plans:
    // the root [`encode`] would write for the same plan.
    root.places.extend(kept.places.iter().map(|(&name, &place)| (name, place)));
    let planned = &root.files;
    root.places.retain(|file, _| planned.contains_key(file));
    let changed_pages = u32::try_from(delta.pages.len()).unwrap_or(u32::MAX);
    let changed_entries = delta.pages.values().map(|page| page.sections.iter().map(BTreeMap::len).sum::<usize>()).sum::<usize>();
    let changed_entries = u32::try_from(changed_entries).unwrap_or(u32::MAX);
    let mut jobs: SVec<'_, PageJob<'_, '_>> = SVec::with_capacity_in(delta.pages.len(), alloc);
    for (&index, change) in delta.pages.iter() {
        let base = source.page_for(change.witness, Ask::Witness)?;
        let last_key = last.pages.get(usize::try_from(index).unwrap_or(usize::MAX)).copied().unwrap_or_default();
        if base.is_none() && last_key != Key::default() { return Err(PagesError::Missing { page: index, key: last_key }); }
        jobs.push(PageJob { index, base, change, bits: last.bits, last_key });
    }
    let built = run_pages(&jobs);
    let mut puts = SVec::with_capacity_in(built.len(), alloc);
    let mut records = SVec::with_capacity_in(built.len(), alloc);
    for one in built {
        root.bytes = root.bytes.checked_sub(one.base_len)
            .and_then(|n| n.checked_add(one.new_len)).ok_or(PagesError::Total { page: one.index, field: "page byte count" })?;
        if let Some(slot) = root.pages.get_mut(usize::try_from(one.index).unwrap_or(usize::MAX)) {
            *slot = one.key;
        }
        if one.changed {
            puts.push((one.key, one.bytes));
            records.push((one.key, one.records));
        }
    }
    let mut encoded = root.finish(puts, records, PagesPlant::None, alloc);
    encoded.changed_entries = changed_entries;
    encoded.changed_pages = changed_pages;
    Ok(encoded)
}
