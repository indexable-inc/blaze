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


fn file_shard_of(file: Name) -> usize {
    usize::from(file.subject.0[0] >> 4)
}

fn empty_directory(alloc: SharedAlloc<'_>) -> Root<'_> {
    Root {
        serial: Gen(0), bits: 0, bytes: 0, pages: SVec::new_in(alloc), page_shards: SVec::new_in(alloc),
        live_pages: 0, files: SMap::new_in(alloc), places: SMap::new_in(alloc),
        units: SVec::new_in(alloc), findings: SMap::new_in(alloc), owed_tasks: SMap::new_in(alloc),
        owed_cells: SMap::new_in(alloc), outlines: 0,
        file_shards: [Key::default(); FILE_SHARDS], places_key: Key::default(), summary_key: Key::default(),
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

pub(super) fn encode_radix_shard<'a>(bits: u8, shard: u32, keys: &[Key], alloc: SharedAlloc<'a>) -> Option<(Key, SVec<'a, u8>)> {
    if keys.iter().all(|k| *k == Key::default()) {
        return None;
    }
    let mut bytes = SVec::with_capacity_in(RADIX_SHARD_BYTES, alloc);
    bytes.extend_from_slice(RADIX_SHARD_MAGIC);
    bytes.push(bits);
    bytes.extend_from_slice(&shard.to_le_bytes());
    for key in keys {
        put_id(&mut bytes, &key.0);
    }
    debug_assert_eq!(bytes.len(), RADIX_SHARD_BYTES, "a radix leaf shard has fixed size");
    let key = content_key(&bytes);
    Some((key, bytes))
}

fn encode_file_shard<'a>(shard: usize, files: &SMap<'_, Name, Option<ContentId>>, alloc: SharedAlloc<'a>) -> Option<(Key, SVec<'a, u8>)> {
    let count = files.keys().filter(|file| file_shard_of(**file) == shard).count();
    if count == 0 {
        return None;
    }
    let mut bytes = SVec::with_capacity_in(4 + 1 + 4 + count * (NAME + 1 + ID), alloc);
    bytes.extend_from_slice(FILE_SHARD_MAGIC);
    bytes.push(u8::try_from(shard).unwrap_or(0));
    put_u32(&mut bytes, count);
    for (&name, digest) in files.iter() {
        if file_shard_of(name) != shard {
            continue;
        }
        put_name(&mut bytes, name);
        bytes.push(u8::from(digest.is_some()));
        if let Some(digest) = digest {
            put_id(&mut bytes, digest);
        }
    }
    let key = content_key(&bytes);
    Some((key, bytes))
}

fn encode_places<'a>(places: &SMap<'_, Name, FilePlace>, alloc: SharedAlloc<'a>) -> Option<(Key, SVec<'a, u8>)> {
    if places.is_empty() {
        return None;
    }
    let mut bytes = SVec::with_capacity_in(4 + 4 + places.len() * FILE_PLACE, alloc);
    bytes.extend_from_slice(PLACES_MAGIC);
    put_u32(&mut bytes, places.len());
    for (&file, place) in places.iter() {
        put_name(&mut bytes, file);
        put_name(&mut bytes, place.unit);
        bytes.push(place.edition);
        bytes.push(place.flags);
    }
    let key = content_key(&bytes);
    Some((key, bytes))
}

fn encode_summary<'a>(units: &[Name], findings: &SMap<'_, Name, u32>, alloc: SharedAlloc<'a>) -> Option<(Key, SVec<'a, u8>)> {
    if units.is_empty() && findings.is_empty() {
        return None;
    }
    let mut bytes = SVec::with_capacity_in(4 + 4 + units.len() * NAME + 4 + findings.len() * (NAME + 4), alloc);
    bytes.extend_from_slice(SUMMARY_MAGIC);
    put_u32(&mut bytes, units.len());
    for &name in units.iter() {
        put_name(&mut bytes, name);
    }
    put_u32(&mut bytes, findings.len());
    for (&name, &count) in findings.iter() {
        put_name(&mut bytes, name);
        bytes.extend_from_slice(&count.to_le_bytes());
    }
    let key = content_key(&bytes);
    Some((key, bytes))
}

impl<'a> Root<'a> {
    fn write_header(&self, flags: u8, bytes: &mut SVec<'a, u8>) {
        bytes.extend_from_slice(ROOT_MAGIC);
        bytes.extend_from_slice(&self.serial.0.to_le_bytes());
        bytes.push(self.bits);
        bytes.extend_from_slice(&self.bytes.to_le_bytes());
        bytes.push(flags);
        bytes.extend_from_slice(&self.live_pages.to_le_bytes());
        bytes.extend_from_slice(&self.outlines.to_le_bytes());
    }

    fn write_owed(&self, bytes: &mut SVec<'a, u8>) {
        put_u32(bytes, self.owed_tasks.len());
        for plan in self.owed_tasks.values() { put_plan(bytes, plan); }
        put_u32(bytes, self.owed_cells.len());
        for (id, &at) in self.owed_cells.iter() { put_cell_id(bytes, id); bytes.extend_from_slice(&at.0.to_le_bytes()); }
    }

    fn finish_flat(
        self,
        mut puts: SVec<'a, (Key, SVec<'a, u8>)>,
        records: SVec<'a, (Key, SVec<'a, (Name, Key)>)>,
        page_deltas: Option<SVec<'a, PageDelta>>,
        plant: PagesPlant,
        alloc: SharedAlloc<'a>,
    ) -> Encoded<'a> {
        let task_puts = u32::try_from(puts.len()).unwrap_or(u32::MAX);
        let mut bytes = SVec::with_capacity_in(ROOT_HEAD + 4 + ID * self.pages.len(), alloc);
        self.write_header(ROOT_FLAG_FLAT, &mut bytes);
        put_u32(&mut bytes, self.pages.len());
        for key in self.pages.iter() { put_id(&mut bytes, &key.0); }
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
        self.write_owed(&mut bytes);
        let root = content_key(&bytes);
        puts.push((root, bytes));
        Encoded {
            serial: self.serial, bits: self.bits, bytes: self.bytes, root, pages: self.pages,
            page_shards: SVec::new_in(alloc), live_pages: self.live_pages, puts, task_puts,
            tree_keys: SVec::new_in(alloc), page_deltas, plant, records,
            files: self.outlines, findings: u32::try_from(self.findings.len()).unwrap_or(u32::MAX),
            owed: u32::try_from(self.owed_tasks.len() + self.owed_cells.len()).unwrap_or(u32::MAX),
            changed_entries: 0, changed_pages: 0,
        }
    }

    fn finish_radix_top(
        self,
        mut puts: SVec<'a, (Key, SVec<'a, u8>)>,
        task_puts: u32,
        records: SVec<'a, (Key, SVec<'a, (Name, Key)>)>,
        page_deltas: Option<SVec<'a, PageDelta>>,
        plant: PagesPlant,
        alloc: SharedAlloc<'a>,
    ) -> Encoded<'a> {
        let mut tree_keys: SVec<'a, Key> = SVec::with_capacity_in(self.page_shards.len() + FILE_SHARDS + 2, alloc);
        for &key in self.page_shards.iter() {
            if key != Key::default() { tree_keys.push(key); }
        }
        for &key in self.file_shards.iter() {
            if key != Key::default() { tree_keys.push(key); }
        }
        if self.places_key != Key::default() { tree_keys.push(self.places_key); }
        if self.summary_key != Key::default() { tree_keys.push(self.summary_key); }

        let cap = ROOT_HEAD + 4 + ID * (self.page_shards.len() + FILE_SHARDS + 2) + 8;
        let mut bytes = SVec::with_capacity_in(cap, alloc);
        self.write_header(ROOT_FLAG_RADIX, &mut bytes);
        put_u32(&mut bytes, self.page_shards.len());
        for key in self.page_shards.iter() { put_id(&mut bytes, &key.0); }
        for key in self.file_shards.iter() { put_id(&mut bytes, &key.0); }
        put_id(&mut bytes, &self.places_key.0);
        put_id(&mut bytes, &self.summary_key.0);
        self.write_owed(&mut bytes);
        let root = content_key(&bytes);
        puts.push((root, bytes));
        Encoded {
            serial: self.serial, bits: self.bits, bytes: self.bytes, root, pages: SVec::new_in(alloc),
            page_shards: self.page_shards, live_pages: self.live_pages, puts, task_puts,
            tree_keys, page_deltas, plant, records,
            files: self.outlines, findings: u32::try_from(self.findings.len()).unwrap_or(u32::MAX),
            owed: u32::try_from(self.owed_tasks.len() + self.owed_cells.len()).unwrap_or(u32::MAX),
            changed_entries: 0, changed_pages: 0,
        }
    }

    pub(super) fn finish_whole(
        mut self,
        flat_pages: SVec<'a, Key>,
        same_paging: Option<&Root<'_>>,
        mut puts: SVec<'a, (Key, SVec<'a, u8>)>,
        records: SVec<'a, (Key, SVec<'a, (Name, Key)>)>,
        plant: PagesPlant,
        alloc: SharedAlloc<'a>,
    ) -> Encoded<'a> {
        if self.bits <= RADIX_FLAT_MAX_BITS {
            self.pages = flat_pages;
            return self.finish_flat(puts, records, None, plant, alloc);
        }
        let task_puts = u32::try_from(puts.len()).unwrap_or(u32::MAX);
        let shard_count = 1usize << (self.bits - RADIX_LEAF_BITS);
        let mut page_shards = SVec::with_capacity_in(shard_count, alloc);
        for (shard_idx, chunk) in flat_pages.chunks_exact(RADIX_LEAF_PAGES).enumerate() {
            let shard_u32 = u32::try_from(shard_idx).unwrap_or(u32::MAX);
            match encode_radix_shard(self.bits, shard_u32, chunk, alloc) {
                Some((key, bytes)) => {
                    page_shards.push(key);
                    if same_paging.is_none_or(|r| r.page_shards.get(shard_idx) != Some(&key)) {
                        puts.push((key, bytes));
                    }
                }
                None => page_shards.push(Key::default()),
            }
        }
        self.page_shards = page_shards;
        for (s, slot) in self.file_shards.iter_mut().enumerate() {
            match encode_file_shard(s, &self.files, alloc) {
                Some((key, bytes)) => {
                    *slot = key;
                    if same_paging.is_none_or(|r| r.file_shards.get(s) != Some(&key)) {
                        puts.push((key, bytes));
                    }
                }
                None => *slot = Key::default(),
            }
        }
        match encode_places(&self.places, alloc) {
            Some((key, bytes)) => {
                self.places_key = key;
                if same_paging.is_none_or(|r| r.places_key != key) {
                    puts.push((key, bytes));
                }
            }
            None => self.places_key = Key::default(),
        }
        match encode_summary(&self.units, &self.findings, alloc) {
            Some((key, bytes)) => {
                self.summary_key = key;
                if same_paging.is_none_or(|r| r.summary_key != key) {
                    puts.push((key, bytes));
                }
            }
            None => self.summary_key = Key::default(),
        }
        self.finish_radix_top(puts, task_puts, records, None, plant, alloc)
    }

    #[expect(clippy::too_many_arguments, reason = "incremental radix finish: each argument one dirty mask or output list")]
    fn finish_incremental_radix(
        mut self,
        mut puts: SVec<'a, (Key, SVec<'a, u8>)>,
        task_puts: u32,
        records: SVec<'a, (Key, SVec<'a, (Name, Key)>)>,
        page_deltas: SVec<'a, PageDelta>,
        changed_file_shards: u16,
        places_changed: bool,
        summary_changed: bool,
        alloc: SharedAlloc<'a>,
    ) -> Encoded<'a> {
        for (s, slot) in self.file_shards.iter_mut().enumerate() {
            if (changed_file_shards & (1u16 << s)) == 0 {
                continue;
            }
            let old_key = *slot;
            match encode_file_shard(s, &self.files, alloc) {
                Some((key, bytes)) => {
                    *slot = key;
                    if key != old_key {
                        puts.push((key, bytes));
                    }
                }
                None => *slot = Key::default(),
            }
        }
        if places_changed {
            let old_key = self.places_key;
            match encode_places(&self.places, alloc) {
                Some((key, bytes)) => {
                    self.places_key = key;
                    if key != old_key {
                        puts.push((key, bytes));
                    }
                }
                None => self.places_key = Key::default(),
            }
        }
        if summary_changed {
            let old_key = self.summary_key;
            match encode_summary(&self.units, &self.findings, alloc) {
                Some((key, bytes)) => {
                    self.summary_key = key;
                    if key != old_key {
                        puts.push((key, bytes));
                    }
                }
                None => self.summary_key = Key::default(),
            }
        }
        self.finish_radix_top(puts, task_puts, records, Some(page_deltas), PagesPlant::None, alloc)
    }
}

fn ordered<T: Copy + Ord>(prev: &mut Option<T>, value: T, at: usize, section: &'static str) -> Result<(), PagesError> {
    if prev.is_some_and(|last| last >= value) {
        return Err(PagesError::Unsorted { page: ROOT_PAGE, section, at: at as u32 });
    }
    *prev = Some(value);
    Ok(())
}

fn read_owed(r: &mut Rd<'_>, root: &mut Root<'_>) -> Result<(), PagesError> {
    let mut prev = None;
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
    if r.at != r.bytes.len() { return Err(PagesError::Trailing { page: ROOT_PAGE, at: u32::try_from(r.at).unwrap_or(u32::MAX) }); }
    Ok(())
}

pub(super) fn read_flat_directory<'a>(
    mut r: Rd<'_>,
    serial: Gen,
    bits: u8,
    total: u64,
    live_pages: u32,
    outlines: u32,
    pages: SVec<'a, Key>,
    alloc: SharedAlloc<'a>,
) -> Result<Root<'a>, PagesError> {
    let mut root = empty_directory(alloc);
    root.serial = serial;
    root.bits = bits;
    root.bytes = total;
    root.live_pages = live_pages;
    root.pages = pages;
    root.outlines = outlines;
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
    read_owed(&mut r, &mut root)?;
    Ok(root)
}

#[expect(clippy::too_many_arguments, reason = "radix root decode: each argument one parsed header field")]
pub(super) fn read_radix_directory<'a, 'g>(
    mut r: Rd<'_>,
    serial: Gen,
    bits: u8,
    total: u64,
    live_pages: u32,
    outlines: u32,
    page_shards: SVec<'a, Key>,
    mut get: impl FnMut(Key) -> Got<'g>,
    alloc: SharedAlloc<'a>,
) -> Result<Root<'a>, PagesError> {
    let mut root = empty_directory(alloc);
    root.serial = serial;
    root.bits = bits;
    root.bytes = total;
    root.live_pages = live_pages;
    root.page_shards = page_shards;
    root.outlines = outlines;
    for slot in root.file_shards.iter_mut() {
        *slot = Key(r.id()?);
    }
    root.places_key = Key(r.id()?);
    root.summary_key = Key(r.id()?);
    read_owed(&mut r, &mut root)?;

    for (shard, &key) in root.file_shards.iter().enumerate() {
        if key == Key::default() {
            continue;
        }
        let sealed = get(key).ok_or(PagesError::Missing { page: ROOT_PAGE, key })?.map_err(|torn| torn.at(ROOT_PAGE))?;
        let mut sr = Rd { bytes: sealed.bytes(), at: 0, page: ROOT_PAGE };
        if sr.take(4)? != FILE_SHARD_MAGIC || usize::from(sr.u8()?) != shard {
            return Err(PagesError::Magic { page: ROOT_PAGE });
        }
        let mut prev = None;
        for _ in 0..sr.count(NAME + 1)? {
            let at = sr.at;
            let name = sr.name()?;
            if file_shard_of(name) != shard {
                return Err(PagesError::Misplaced { page: ROOT_PAGE, section: "file directory shard", name });
            }
            ordered(&mut prev, name, at, "file directory")?;
            let value = match sr.u8()? { 0 => None, 1 => Some(sr.id()?), tag => return Err(sr.tag(tag)) };
            root.files.insert(name, value);
        }
        if sr.at != sr.bytes.len() {
            return Err(PagesError::Trailing { page: ROOT_PAGE, at: u32::try_from(sr.at).unwrap_or(u32::MAX) });
        }
    }

    if root.places_key != Key::default() {
        let key = root.places_key;
        let sealed = get(key).ok_or(PagesError::Missing { page: ROOT_PAGE, key })?.map_err(|torn| torn.at(ROOT_PAGE))?;
        let mut pr = Rd { bytes: sealed.bytes(), at: 0, page: ROOT_PAGE };
        if pr.take(4)? != PLACES_MAGIC {
            return Err(PagesError::Magic { page: ROOT_PAGE });
        }
        let mut prev = None;
        for _ in 0..pr.count(FILE_PLACE)? {
            let at = pr.at;
            let file = pr.name()?;
            ordered(&mut prev, file, at, "file places directory")?;
            let place = FilePlace { unit: pr.name()?, edition: pr.u8()?, flags: pr.u8()? };
            if !root.files.contains_key(&file) {
                return Err(PagesError::Unsorted { page: ROOT_PAGE, section: "file places directory (a place of no planned file)", at: u32::try_from(at).unwrap_or(u32::MAX) });
            }
            root.places.insert(file, place);
        }
        if pr.at != pr.bytes.len() {
            return Err(PagesError::Trailing { page: ROOT_PAGE, at: u32::try_from(pr.at).unwrap_or(u32::MAX) });
        }
    }

    if root.summary_key != Key::default() {
        let key = root.summary_key;
        let sealed = get(key).ok_or(PagesError::Missing { page: ROOT_PAGE, key })?.map_err(|torn| torn.at(ROOT_PAGE))?;
        let mut sr = Rd { bytes: sealed.bytes(), at: 0, page: ROOT_PAGE };
        if sr.take(4)? != SUMMARY_MAGIC {
            return Err(PagesError::Magic { page: ROOT_PAGE });
        }
        let mut prev = None;
        for _ in 0..sr.count(NAME)? {
            let n = sr.name()?;
            ordered(&mut prev, n, sr.at - NAME, "units directory")?;
            root.units.push(n);
        }
        prev = None;
        for _ in 0..sr.count(NAME + 4)? {
            let n = sr.name()?;
            ordered(&mut prev, n, sr.at - NAME, "findings directory")?;
            root.findings.insert(n, sr.u32()?);
        }
        if sr.at != sr.bytes.len() {
            return Err(PagesError::Trailing { page: ROOT_PAGE, at: u32::try_from(sr.at).unwrap_or(u32::MAX) });
        }
    }

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
fn try_repage<'a, 'g>(
    kept: &Kept<'_>,
    last: &Root<'_>,
    source: &dyn StoredPages,
    work: &Work<'_>,
    get_record: &mut impl FnMut(Key) -> Got<'g>,
    alloc: SharedAlloc<'a>,
) -> Result<Option<Encoded<'a>>, PagesError> {
    let mut old_pages: SVec<'a, &VerifiedPage<'_>> = SVec::with_capacity_in(usize::try_from(last.live_pages).unwrap_or(0), alloc);
    if !last.is_radix() {
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
    } else {
        for (shard_idx, &shard_key) in last.page_shards.iter().enumerate() {
            if shard_key == Key::default() {
                continue;
            }
            let shard_u32 = u32::try_from(shard_idx).unwrap_or(u32::MAX);
            let sealed = get_record(shard_key)
                .ok_or(PagesError::Missing { page: ROOT_PAGE, key: shard_key })?
                .map_err(|torn| torn.at(ROOT_PAGE))?;
            let shard = decode_radix_shard(sealed, shard_u32, last.bits, alloc)?;
            let base_idx = shard_idx << RADIX_LEAF_BITS;
            for (slot, &last_key) in shard.keys.iter().enumerate() {
                if last_key == Key::default() {
                    continue;
                }
                let idx = base_idx + slot;
                let witness = page_witness(idx, last.bits);
                let Some(page) = source.page_for(witness, Ask::Witness)? else {
                    return Err(PagesError::Missing { page: u32::try_from(idx).unwrap_or(u32::MAX), key: last_key });
                };
                old_pages.push(page);
            }
        }
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
    let mut flat_pages = SVec::with_capacity_in(count, alloc);
    let mut puts = SVec::new_in(alloc);
    let mut records = SVec::new_in(alloc);
    let mut live_pages = 0u32;
    for (index, buf) in bufs.iter().enumerate() {
        let idx_u32 = u32::try_from(index).unwrap_or(u32::MAX);
        match buf.finish(new_bits, idx_u32, alloc) {
            Some(bytes) => {
                let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                root.bytes = root.bytes.checked_add(len).ok_or(PagesError::Total { page: idx_u32, field: "page byte count" })?;
                live_pages = live_pages.saturating_add(1);
                let key = content_key(&bytes);
                flat_pages.push(key);
                let rec_bytes = buf.parts.get(RECORDS).map_or(&[][..], |part| part.as_slice());
                let page_records = view::page_records(rec_bytes, alloc);
                puts.push((key, bytes));
                records.push((key, page_records));
            }
            None => flat_pages.push(Key::default()),
        }
    }
    root.live_pages = live_pages;
    Ok(Some(root.finish_whole(flat_pages, None, puts, records, PagesPlant::None, alloc)))
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
/// Returns the bitmask of `FILE_SHARDS` (0..16) whose entries changed in `root.files`.
fn file_changes<'a>(root: &mut Root<'a>, last: &Root<'_>, work: &Work<'_>, source: &dyn StoredPages, delta: &mut Changed<'a>, files: &crate::SSet<'a, Name>) -> Result<u16, PagesError> {
    let mut changed_shards = 0u16;
    for &file in files.iter() {
        let replacement = work.by_file.get(&file);
        if matches!(replacement, Some(None)) {
            if root.files.remove(&file).is_some() {
                changed_shards |= 1u16 << file_shard_of(file);
                delta.set(FILES, named(file), None);
            }
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
        let next_digest = all_stand.then(|| file_digest(&record));
        if root.files.insert(file, next_digest) != Some(next_digest) {
            changed_shards |= 1u16 << file_shard_of(file);
        }
        if !last.files.contains_key(&file) || old_members != &record[NAME + 4..] {
            delta.set(FILES, named(file), Some(record));
        }
    }
    Ok(changed_shards)
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

/// Fold the request into the existing page directory (both flat and 2-level radix roots). Only
/// pages containing a changed row, reader, cell, obligation or file plan can acquire a new key.
/// When the merged answer outgrows `last.bits` (e.g. a cold workspace starting from a small
/// `tool-base`), re-pages directly into `bits_kept(total, Some(last.bits))`.
pub fn encode_changes<'a, 'g, G>(kept: &Kept<'_>, last: &Root<'_>, alloc: SharedAlloc<'a>, get_record: G) -> Result<Encoded<'a>, PagesError>
where
    G: FnMut(Key) -> Got<'g>,
{
    encode_changes_radix(kept, last, alloc, get_record, |jobs| {
        let mut out = SVec::with_capacity_in(jobs.len(), alloc);
        for job in jobs {
            out.push(job.run(alloc));
        }
        out
    })
}

/// [`encode_changes`] with a caller-supplied executor for independent [`PageJob`]s, supporting
/// both flat (`bits <= RADIX_FLAT_MAX_BITS`) and 2-level 256-way radix (`bits >
/// RADIX_FLAT_MAX_BITS`) roots. Only the `<= changed_pages` touched leaf shards are fetched
/// through `get_record`, updated, and hashed; untouched shards and unchanged directory
/// sub-records carry their keys forward with zero reads and zero hashing.
pub fn encode_changes_radix<'a, 'g, G, F>(
    kept: &Kept<'_>,
    last: &Root<'_>,
    alloc: SharedAlloc<'a>,
    mut get_record: G,
    run_pages: F,
) -> Result<Encoded<'a>, PagesError>
where
    G: FnMut(Key) -> Got<'g>,
    F: FnOnce(&[PageJob<'_, '_>]) -> SVec<'a, PageBuilt<'a>>,
{
    let answer = &kept.answer;
    let source = answer.stored().ok_or(PagesError::Base { serial: answer.serial })?;
    let work = answer.work().ok_or(PagesError::Base { serial: answer.serial })?;
    let added = estimate_added_bytes(kept, work);
    let page_budget = blaze_drive::answers::PAGE_BYTES << last.bits.min(blaze_drive::answers::MOST_BITS);
    if added > page_budget / 2
        && blaze_drive::answers::bits_for(last.bytes.saturating_add(added)) > last.bits
        && let Some(encoded) = try_repage(kept, last, source, work, &mut get_record, alloc)?
    {
        return Ok(encoded);
    }
    let mut delta = Changed { bits: last.bits, pages: BTreeMap::new_in(alloc), alloc };
    let mut root = empty_directory(alloc);
    root.serial = work.serial();
    root.bits = last.bits;
    root.bytes = last.bytes;
    root.live_pages = last.live_pages;
    if !last.is_radix() {
        root.pages.extend_from_slice(&last.pages);
    } else {
        root.page_shards.extend_from_slice(&last.page_shards);
        root.file_shards = last.file_shards;
        root.places_key = last.places_key;
        root.summary_key = last.summary_key;
    }
    root.outlines = last.outlines;
    root.files.extend(last.files.iter().map(|(&name, &digest)| (name, digest)));
    root.places.extend(last.places.iter().map(|(&name, &place)| (name, place)));
    root.findings.extend(last.findings.iter().map(|(&name, &count)| (name, count)));
    let mut units = BTreeSet::new_in(alloc);
    units.extend(last.units.iter().copied());
    let mut files = BTreeSet::new_in(alloc);
    files.extend(work.by_file.keys().copied());
    let mut summary_changed = false;
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
        if next.is_some_and(|r| r.phase.stage == Stage::Units) {
            if units.insert(name) { summary_changed = true; }
        } else if units.remove(&name) {
            summary_changed = true;
        }
        match next.filter(|r| r.failed || r.findings > 0) {
            Some(r) => {
                if root.findings.insert(name, r.findings) != Some(r.findings) {
                    summary_changed = true;
                }
            }
            None => {
                if root.findings.remove(&name).is_some() {
                    summary_changed = true;
                }
            }
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
    let changed_file_shards = file_changes(&mut root, last, work, source, &mut delta, &files)?;
    // This request's places over the last root's, and none of a file the root no longer plans:
    // the root [`encode`] would write for the same plan.
    let mut places_changed = false;
    for (&name, &place) in kept.places.iter() {
        if root.files.contains_key(&name) && root.places.insert(name, place) != Some(place) {
            places_changed = true;
        }
    }
    let places_before_retain = root.places.len();
    let planned = &root.files;
    root.places.retain(|file, _| planned.contains_key(file));
    if root.places.len() != places_before_retain {
        places_changed = true;
    }

    let changed_pages = u32::try_from(delta.pages.len()).unwrap_or(u32::MAX);
    let changed_entries = delta.pages.values().map(|page| page.sections.iter().map(BTreeMap::len).sum::<usize>()).sum::<usize>();
    let changed_entries = u32::try_from(changed_entries).unwrap_or(u32::MAX);

    let mut encoded = if !last.is_radix() {
        let mut jobs: SVec<'_, PageJob<'_, '_>> = SVec::with_capacity_in(delta.pages.len(), alloc);
        for (&index, change) in delta.pages.iter() {
            let base = source.page_for(change.witness, Ask::Witness)?;
            let last_key = last.pages.get(usize::try_from(index).unwrap_or(usize::MAX)).copied().unwrap_or_default();
            if base.is_none() && last_key != Key::default() { return Err(PagesError::Missing { page: index, key: last_key }); }
            jobs.push(PageJob { index, base, change, bits: last.bits, last_key });
        }
        let built = run_pages(&jobs);
        let mut puts = SVec::with_capacity_in(built.len() + 1, alloc);
        let mut records = SVec::with_capacity_in(built.len(), alloc);
        let mut page_deltas = SVec::with_capacity_in(built.len(), alloc);
        for one in built {
            root.bytes = root.bytes.checked_sub(one.base_len)
                .and_then(|n| n.checked_add(one.new_len)).ok_or(PagesError::Total { page: one.index, field: "page byte count" })?;
            if let Some(slot) = root.pages.get_mut(usize::try_from(one.index).unwrap_or(usize::MAX)) {
                let old = *slot;
                if old != one.key {
                    if old == Key::default() && one.key != Key::default() {
                        root.live_pages = root.live_pages.saturating_add(1);
                    } else if old != Key::default() && one.key == Key::default() {
                        root.live_pages = root.live_pages.saturating_sub(1);
                    }
                    page_deltas.push(PageDelta { index: one.index, old, new: one.key });
                    *slot = one.key;
                }
            }
            if one.changed {
                puts.push((one.key, one.bytes));
                records.push((one.key, one.records));
            }
        }
        root.finish_flat(puts, records, Some(page_deltas), PagesPlant::None, alloc)
    } else {
        // Fetch ONLY the `<= changed_pages` touched radix leaf shards.
        let mut touched_shards: SMap<'a, usize, RadixShard<'a>> = SMap::new_in(alloc);
        for &index in delta.pages.keys() {
            let (shard_idx, _) = Root::shard_of(usize::try_from(index).unwrap_or(usize::MAX));
            if touched_shards.contains_key(&shard_idx) {
                continue;
            }
            let Some(&shard_key) = last.page_shards.get(shard_idx) else {
                return Err(PagesError::Count { page: ROOT_PAGE, count: index });
            };
            let shard = if shard_key == Key::default() {
                RadixShard { keys: blaze_own::Box::new_in([Key::default(); RADIX_LEAF_PAGES], alloc) }
            } else {
                let shard_u32 = u32::try_from(shard_idx).unwrap_or(u32::MAX);
                let sealed = get_record(shard_key)
                    .ok_or(PagesError::Missing { page: ROOT_PAGE, key: shard_key })?
                    .map_err(|torn| torn.at(ROOT_PAGE))?;
                decode_radix_shard(sealed, shard_u32, last.bits, alloc)?
            };
            touched_shards.insert(shard_idx, shard);
        }
        let mut jobs: SVec<'_, PageJob<'_, '_>> = SVec::with_capacity_in(delta.pages.len(), alloc);
        for (&index, change) in delta.pages.iter() {
            let base = source.page_for(change.witness, Ask::Witness)?;
            let (shard_idx, slot_idx) = Root::shard_of(usize::try_from(index).unwrap_or(usize::MAX));
            let last_key = touched_shards.get(&shard_idx).and_then(|s| s.keys.get(slot_idx)).copied().unwrap_or_default();
            if base.is_none() && last_key != Key::default() { return Err(PagesError::Missing { page: index, key: last_key }); }
            jobs.push(PageJob { index, base, change, bits: last.bits, last_key });
        }
        let built = run_pages(&jobs);
        let mut puts = SVec::with_capacity_in(built.len() + touched_shards.len() + 4, alloc);
        let mut records = SVec::with_capacity_in(built.len(), alloc);
        let mut page_deltas = SVec::with_capacity_in(built.len(), alloc);
        for one in built {
            root.bytes = root.bytes.checked_sub(one.base_len)
                .and_then(|n| n.checked_add(one.new_len)).ok_or(PagesError::Total { page: one.index, field: "page byte count" })?;
            let (shard_idx, slot_idx) = Root::shard_of(usize::try_from(one.index).unwrap_or(usize::MAX));
            if let Some(shard) = touched_shards.get_mut(&shard_idx)
                && let Some(slot) = shard.keys.get_mut(slot_idx)
            {
                let old = *slot;
                if old != one.key {
                    if old == Key::default() && one.key != Key::default() {
                        root.live_pages = root.live_pages.saturating_add(1);
                    } else if old != Key::default() && one.key == Key::default() {
                        root.live_pages = root.live_pages.saturating_sub(1);
                    }
                    page_deltas.push(PageDelta { index: one.index, old, new: one.key });
                    *slot = one.key;
                }
            }
            if one.changed {
                puts.push((one.key, one.bytes));
                records.push((one.key, one.records));
            }
        }
        let task_puts = u32::try_from(puts.len()).unwrap_or(u32::MAX);
        for (shard_idx, shard) in touched_shards.into_iter() {
            let shard_u32 = u32::try_from(shard_idx).unwrap_or(u32::MAX);
            let old_shard_key = root.page_shards.get(shard_idx).copied().unwrap_or_default();
            match encode_radix_shard(last.bits, shard_u32, shard.keys.as_slice(), alloc) {
                Some((new_key, bytes)) => {
                    if let Some(slot) = root.page_shards.get_mut(shard_idx) {
                        *slot = new_key;
                    }
                    if new_key != old_shard_key {
                        puts.push((new_key, bytes));
                    }
                }
                None => {
                    if let Some(slot) = root.page_shards.get_mut(shard_idx) {
                        *slot = Key::default();
                    }
                }
            }
        }
        root.finish_incremental_radix(puts, task_puts, records, page_deltas, changed_file_shards, places_changed, summary_changed, alloc)
    };
    encoded.changed_entries = changed_entries;
    encoded.changed_pages = changed_pages;
    Ok(encoded)
}
