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

fn bytes_of<'a>(alloc: SharedAlloc<'a>, write: impl FnOnce(&mut SVec<'a, u8>)) -> SVec<'a, u8> {
    let mut bytes = Vec::new_in(alloc);
    write(&mut bytes);
    bytes
}

fn key_of_entry(bytes: &[u8], section: usize) -> CellId {
    let mut r = Rd { bytes, at: 0, page: 0 };
    if view::key_width(section) == NAME {
        named(r.name().expect("the writer merges authenticated entries"))
    } else {
        r.cell_id().expect("the writer merges authenticated entries")
    }
}

/// Merge the old page's entries with sorted replacements and deletions in each column.
/// Unchanged variable lists are copied as byte ranges and never decoded into rows.
fn merge_page<'a>(base: Option<&VerifiedPage<'_>>, change: &ChangedPage<'_>, bits: u8, index: u32, alloc: SharedAlloc<'a>) -> (SVec<'a, u8>, SVec<'a, (Name, Key)>) {
    let mut buf = PageBuf::new_in(alloc);
    for section in 0..SECTIONS {
        let mut changes = change.sections[section].iter().peekable();
        let mut append = |bytes: &[u8]| buf.entry(section).extend_from_slice(bytes);
        for bytes in base.into_iter().flat_map(|p| p.entries(section)) {
            let key = key_of_entry(bytes, section);
            while changes.peek().is_some_and(|item| *item.0 < key) {
                if let Some((_, Some(replacement))) = changes.next() { append(replacement); }
            }
            if changes.peek().is_some_and(|item| *item.0 == key) {
                if let Some((_, Some(replacement))) = changes.next() { append(replacement); }
            } else {
                append(bytes);
            }
        }
        for (_, value) in changes { if let Some(replacement) = value { append(replacement); } }
    }
    let records = view::page_records(&buf.parts[RECORDS], alloc);
    // A page left with nothing on it is named by no key (`bytes.is_empty()` below).
    let page = buf.finish(bits, index, alloc).unwrap_or_else(|| Vec::new_in(alloc));
    (page, records)
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
            delta.set(OWED_TASKS, named(name), Some(bytes_of(alloc, |b| put_plan(b, &plan))));
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
            delta.set(OWED_CELLS, id, Some(bytes_of(alloc, |b| { put_cell_id(b, &id); b.extend_from_slice(&moved.0.to_le_bytes()); })));
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
        let mut changes = changes.iter().peekable();
        let mut readers = Vec::new_in(delta.alloc);
        put_cell_id(&mut readers, &id);
        put_u32(&mut readers, 0);
        for reader in source.page_for(readers_slot(&id), Ask::Readers)?.into_iter().flat_map(|p| p.readers(&id)) {
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
        delta.set(READERS, id, (count > 0).then_some(readers));
    }
    Ok(())
}

/// Fold the request into the existing page directory. Only pages containing a changed
/// row, reader, cell, obligation or file plan can acquire a new key.
pub fn encode_changes<'a>(kept: &Kept<'_>, last: &Root<'_>, alloc: SharedAlloc<'a>) -> Result<Encoded<'a>, PagesError> {
    let answer = &kept.answer;
    let source = answer.stored().ok_or(PagesError::Base { serial: answer.serial })?;
    let work = answer.work().ok_or(PagesError::Base { serial: answer.serial })?;
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
        delta.set(ROWS, k, next.map(|row| bytes_of(alloc, |b| put_row(b, name, row))));
        let record = next.and_then(|_| kept.records.get(&name));
        delta.set(RECORDS, k, record.map(|key| bytes_of(alloc, |b| { put_name(b, name); put_id(b, &key.0); })));
        if next.is_none() {
            delta.set(CAUSES, k, None);
        } else if let Some(&(serial, cause)) = kept.causes.get(&name) {
            delta.set(CAUSES, k, Some(bytes_of(alloc, |b| put_cause(b, name, serial, cause))));
        }
    }
    // An entered task whose completion kept its row still has a cause and may publish a record.
    for (&name, &(serial, cause)) in kept.causes.iter() {
        if !work.rows.contains_key(&name) && previous(source, name)?.is_some() {
            delta.set(CAUSES, named(name), Some(bytes_of(alloc, |b| put_cause(b, name, serial, cause))));
            delta.set(RECORDS, named(name), kept.records.get(&name).map(|key| bytes_of(alloc, |b| { put_name(b, name); put_id(b, &key.0); })));
        }
    }
    root.units.extend(units.iter().copied());
    for (&id, value) in work.cells.iter() {
        delta.set(CELLS, id, value.map(|(digest, moved)| bytes_of(alloc, |b| {
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
    let mut puts = Vec::new_in(alloc);
    let mut records = Vec::new_in(alloc);
    let changed_pages = u32::try_from(delta.pages.len()).unwrap_or(u32::MAX);
    let changed_entries = delta.pages.values().map(|page| page.sections.iter().map(BTreeMap::len).sum::<usize>()).sum::<usize>();
    let changed_entries = u32::try_from(changed_entries).unwrap_or(u32::MAX);
    for (&index, change) in delta.pages.iter() {
        let base = source.page_for(change.witness, Ask::Witness)?;
        let last_key = last.pages[index as usize];
        if base.is_none() && last_key != Key::default() { return Err(PagesError::Missing { page: index, key: last_key }); }
        let (bytes, keys) = merge_page(base, change, last.bits, index, alloc);
        root.bytes = root.bytes.checked_sub(base.map_or(0, |p| p.len() as u64))
            .and_then(|n| n.checked_add(bytes.len() as u64)).ok_or(PagesError::Total { page: index, field: "page byte count" })?;
        let key = if bytes.is_empty() { Key::default() } else { content_key(&bytes) };
        root.pages[index as usize] = key;
        if key != Key::default() && key != last_key {
            puts.push((key, bytes));
            records.push((key, keys));
        }
    }
    let mut encoded = root.finish(puts, records, PagesPlant::None, alloc);
    encoded.changed_entries = changed_entries;
    encoded.changed_pages = changed_pages;
    Ok(encoded)
}
