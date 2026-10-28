//! The remote's key index: which pack of a project holds which key, without the keys of
//! every pack. Pure logic over bytes, with no files, sockets or clocks: the remote's
//! adapters (`blaze-cargo`'s `remote/index.rs`) fetch and send what is here.
//!
//! A remote holds a project's sealed packs and, beside each, the list of its keys (32 bytes
//! a key, a tenth of the pack). Finding the pack of one key from those lists is reading all
//! of them: some 650 MB for blaze's own CI. The index is what a miss reads instead, a few
//! hundred KB.
//!
//! # Objects
//!
//! - The **root**, `manifests/<project>.index`: a list as the manifest is one, signed and
//!   turned the same way, one line a run, newest last: `run1 <run> <entries> <packs>`.
//!   Lines of another kind are kept as they are by those that do not know them.
//! - A **run**, `index/<sha256>`: the packs it covers, sorted, and for each of its `2^bits`
//!   shards the hash and number of entries of that shard (all zeros and 0 for one with
//!   none, which is no object). A push indexes the packs the manifest lists and no run
//!   covers in a run of its own; runs of about one size are merged ([`plan_merge`]), so a
//!   project has a few runs, the largest holding most of it.
//! - A **shard**, `index/<sha256>`: the entries of one run whose keys start with the
//!   shard's `bits` bits, in groups by pack. An entry is the first `prefix` bytes of a key.
//!
//! Everything below the root is named by the hash of its bytes, so the root's signature
//! covers it all: a reader checks the root, then each run and shard against the hash it was
//! named by, then the shape [`Run::parse`] and [`Shard::parse`] check. What it is told is a
//! pack that holds a key's first bytes. That pack still has to be in the manifest, and the
//! key in the pack once it is fetched: an index names candidates, and never a value. A run
//! does not say the format of its packs; the manifest does, and a reader fetches only those
//! it lists of the format it reads.
//!
//! An entry keeps 6 bytes of a key. With 20 million entries, the chance that a key no pack
//! holds matches one is 20e6 / 2^48, about 7e-8 a lookup: a fetch of a pack that does not
//! have it, which the pack's own index then says. Every pack that holds a key has its entry,
//! so a key is found as long as one pack the manifest lists holds it.

use std::fmt::Write as _;

use rustc_hash::FxHashMap;
use sha2::{Digest as _, Sha256};

use crate::KEY;

/// A SHA-256: of a pack, of a run or of a shard.
pub type Hash = [u8; 32];

/// What a run starts with: the last byte is the format of runs.
pub const RUN_MAGIC: &[u8; 8] = b"blazeir\x01";
/// What a shard starts with: the last byte is the format of shards.
pub const SHARD_MAGIC: &[u8; 8] = b"blazeis\x01";
/// The word a root's line of a run starts with, of this format of runs.
pub const RUN_LINE: &str = "run1";
/// How many bytes of a key an entry written now keeps.
pub const PREFIX: u8 = 6;
/// The fewest and most bytes of a key an entry may keep.
const PREFIX_RANGE: std::ops::RangeInclusive<u8> = 4..=8;
/// The most shards a run is cut into is `2^BITS_MAX`.
pub const BITS_MAX: u8 = 16;
/// A run is cut into shards of at most about this many bytes, so that a miss reads little.
pub const SHARD_TARGET: u64 = 256 << 10;
/// Runs whose entries are within a factor of this of each other are merged once there are
/// this many of them.
pub const FANOUT: usize = 4;
/// The most runs a root names after a push: past this, the smallest are merged whatever
/// their sizes.
pub const RUNS_MAX: usize = 24;

/// The magic, the bits, the prefix, two bytes that are zero and the count of packs.
const RUN_HEAD: usize = 8 + 1 + 1 + 2 + 4;
/// A shard's hash and its count of entries.
const RUN_SHARD: usize = 32 + 4;
/// The magic, the bits, the prefix, two bytes that are zero, the slot and the count of
/// groups.
const SHARD_HEAD: usize = 8 + 1 + 1 + 2 + 4 + 4;
/// A group's pack, as its place in the run's packs, and its count of entries.
const GROUP: usize = 4 + 4;

/// Why bytes are not a run or a shard.
#[derive(Debug, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Malformed {}

/// A key's first bytes as an entry keeps them: big-endian, the bytes past `prefix` zero, so
/// that entries sort as keys do.
fn entry_of(key: &[u8], prefix: u8) -> u64 {
    let mut bytes = [0; 8];
    bytes[..prefix as usize].copy_from_slice(&key[..prefix as usize]);
    u64::from_be_bytes(bytes)
}

/// What is left of an entry of more bytes once it keeps only `prefix` of them.
fn truncated(entry: u64, prefix: u8) -> u64 {
    match prefix {
        8 => entry,
        _ => entry & !(u64::MAX >> (8 * u32::from(prefix))),
    }
}

/// The shard of a run cut into `2^bits` that an entry is in: its first `bits` bits.
fn slot_of(entry: u64, bits: u8) -> usize {
    match bits {
        0 => 0,
        _ => (entry >> (64 - u32::from(bits))) as usize,
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

/// One run: see the module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    bits: u8,
    prefix: u8,
    packs: Vec<Hash>,
    /// For each slot, the shard's hash and count of entries, `None` when it has none.
    shards: Vec<Option<(Hash, u32)>>,
}

impl Run {
    /// The run in `bytes`, which have been checked against the hash it was named by.
    pub fn parse(bytes: &[u8]) -> Result<Run, Malformed> {
        if bytes.len() < RUN_HEAD || &bytes[..8] != RUN_MAGIC {
            return Err(Malformed("is not a run of this format"));
        }
        let (bits, prefix) = (bytes[8], bytes[9]);
        if bits > BITS_MAX || !PREFIX_RANGE.contains(&prefix) || bytes[10..12] != [0, 0] {
            return Err(Malformed("has a head this format does not have"));
        }
        let count = u32_at(bytes, 12) as usize;
        let slots = 1usize << bits;
        let want = count
            .checked_mul(32)
            .and_then(|it| it.checked_add(RUN_HEAD + slots * RUN_SHARD))
            .ok_or(Malformed("is shorter than it says"))?;
        if bytes.len() != want {
            return Err(Malformed("is not as long as its counts say"));
        }
        let at = RUN_HEAD + count * 32;
        let packs: Vec<Hash> =
            bytes[RUN_HEAD..at].as_chunks::<32>().0.iter().copied().collect::<Vec<_>>();
        if !packs.is_sorted_by(|a, b| a < b) {
            return Err(Malformed("lists its packs out of order, or one twice"));
        }
        let mut shards = Vec::with_capacity(slots);
        for shard in bytes[at..].chunks_exact(RUN_SHARD) {
            let hash: Hash = shard[..32].try_into().unwrap();
            let entries = u32_at(shard, 32);
            shards.push(match (hash == [0; 32], entries) {
                (true, 0) => None,
                (false, 1..) => Some((hash, entries)),
                _ => return Err(Malformed("names a shard with no entries, or none with some")),
            });
        }
        Ok(Run { bits, prefix, packs, shards })
    }

    /// The packs the run covers, sorted.
    pub fn packs(&self) -> &[Hash] {
        &self.packs
    }

    pub fn entries(&self) -> u64 {
        self.shards.iter().flatten().map(|it| u64::from(it.1)).sum()
    }

    /// The shards of the run that have entries: their slots and hashes.
    pub fn shards(&self) -> impl Iterator<Item = (usize, Hash)> + '_ {
        self.shards.iter().enumerate().filter_map(|(slot, it)| Some((slot, it.as_ref()?.0)))
    }

    /// The slot of the shard that would hold `key`.
    pub fn slot(&self, key: &[u8; KEY]) -> usize {
        slot_of(entry_of(key, self.prefix), self.bits)
    }

    /// The hash of the shard in `slot`, `None` when the run has no entry there.
    pub fn shard(&self, slot: usize) -> Option<Hash> {
        Some(self.shards.get(slot)?.as_ref()?.0)
    }

    /// The packs of this run whose entries in `shard` match `key`: those that may hold it.
    /// A shard of another run names none.
    pub fn holders<'a>(
        &'a self,
        shard: &'a Shard,
        key: &[u8; KEY],
    ) -> impl Iterator<Item = &'a Hash> {
        let entry = entry_of(key, self.prefix);
        shard.holding(entry).filter_map(|pack| self.packs.get(pack as usize))
    }

    /// The most bytes the shard in `slot` can be, so a reader asks for no more.
    pub fn shard_max(&self, slot: usize) -> u64 {
        let entries = self.shards.get(slot).copied().flatten().map_or(0, |it| it.1);
        (SHARD_HEAD + self.packs.len() * GROUP) as u64 + u64::from(entries) * u64::from(self.prefix)
    }
}

/// The most bytes a run of `packs` can be, as its root's line says: so a reader asks for no
/// more.
pub fn run_max(packs: u32) -> u64 {
    (RUN_HEAD + (RUN_SHARD << BITS_MAX)) as u64 + u64::from(packs) * 32
}

/// One shard of a run, parsed.
#[derive(Debug)]
pub struct Shard {
    /// Each group's pack, as its place in the run's packs, and its entries.
    groups: Vec<(u32, std::ops::Range<usize>)>,
    entries: Vec<u64>,
}

impl Shard {
    /// The shard in `bytes` as the shard in `slot` of `run`, the bytes checked against the
    /// hash the run names it by.
    pub fn parse(bytes: &[u8], run: &Run, slot: usize) -> Result<Shard, Malformed> {
        if bytes.len() < SHARD_HEAD || &bytes[..8] != SHARD_MAGIC {
            return Err(Malformed("is not a shard of this format"));
        }
        let (bits, prefix) = (bytes[8], bytes[9]);
        if bits != run.bits || prefix != run.prefix || bytes[10..12] != [0, 0] {
            return Err(Malformed("is not cut as its run is"));
        }
        if u32_at(bytes, 12) as usize != slot {
            return Err(Malformed("is the shard of another slot"));
        }
        let count = u32_at(bytes, 16) as usize;
        let entries_at = count
            .checked_mul(GROUP)
            .and_then(|it| it.checked_add(SHARD_HEAD))
            .filter(|it| *it <= bytes.len())
            .ok_or(Malformed("is shorter than its groups"))?;
        let width = prefix as usize;
        let (mut groups, mut entries, mut last_pack) =
            (Vec::with_capacity(count), Vec::new(), None);
        let mut at = entries_at;
        for group in bytes[SHARD_HEAD..entries_at].chunks_exact(GROUP) {
            let (pack, len) = (u32_at(group, 0), u32_at(group, 4) as usize);
            if pack as usize >= run.packs.len()
                || last_pack.is_some_and(|it| it >= pack)
                || len == 0
            {
                return Err(Malformed("has a group of no pack of its run, out of order or empty"));
            }
            last_pack = Some(pack);
            let end = len
                .checked_mul(width)
                .and_then(|it| it.checked_add(at))
                .filter(|it| *it <= bytes.len())
                .ok_or(Malformed("is shorter than its entries"))?;
            let start = entries.len();
            for key in bytes[at..end].chunks_exact(width) {
                let entry = entry_of(key, prefix);
                if slot_of(entry, bits) != slot
                    || entries[start..].last().is_some_and(|it| *it >= entry)
                {
                    return Err(Malformed("has an entry of another slot, or out of order"));
                }
                entries.push(entry);
            }
            groups.push((pack, start..entries.len()));
            at = end;
        }
        if at != bytes.len() {
            return Err(Malformed("has bytes past its entries"));
        }
        if run.shards.get(slot).and_then(|it| it.as_ref()).map(|it| it.1 as usize)
            != Some(entries.len())
        {
            return Err(Malformed("does not have the entries its run says"));
        }
        Ok(Shard { groups, entries })
    }

    /// The places in the run's packs of the groups that have `entry`.
    fn holding(&self, entry: u64) -> impl Iterator<Item = u32> + '_ {
        let entries = &self.entries;
        (self.groups.iter())
            .filter(move |(_, range)| entries[range.clone()].binary_search(&entry).is_ok())
            .map(|(pack, _)| *pack)
    }

    /// Every entry and the place in the run's packs of the pack it is of.
    fn all(&self) -> impl Iterator<Item = (u64, u32)> + '_ {
        (self.groups.iter())
            .flat_map(|(pack, range)| self.entries[range.clone()].iter().map(|it| (*it, *pack)))
    }
}

/// A run being made: of the keys of packs, and of the entries of runs it replaces.
pub struct Builder {
    prefix: u8,
    packs: Vec<Hash>,
    ids: FxHashMap<Hash, u32>,
    entries: Vec<(u64, u32)>,
}

/// A run made: its bytes and the bytes of its shards, which go up before the run does, and
/// the run before the root names it.
pub struct Built {
    pub run: Vec<u8>,
    pub shards: Vec<Vec<u8>>,
    pub entries: u64,
    pub packs: u32,
}

impl Default for Builder {
    fn default() -> Builder {
        Builder {
            prefix: PREFIX,
            packs: Vec::new(),
            ids: FxHashMap::default(),
            entries: Vec::new(),
        }
    }
}

impl Builder {
    fn id(&mut self, pack: &Hash) -> u32 {
        *self.ids.entry(*pack).or_insert_with(|| {
            self.packs.push(*pack);
            self.packs.len() as u32 - 1
        })
    }

    /// Covers `pack`, which holds `keys` (all of them, as its list of keys has them).
    pub fn add_pack<'a>(&mut self, pack: &Hash, keys: impl IntoIterator<Item = &'a [u8; KEY]>) {
        let id = self.id(pack);
        self.entries.extend(keys.into_iter().map(|key| (entry_of(key, PREFIX), id)));
    }

    /// Takes in what `run` covers whose pack `keep` says is still listed, with the entries of
    /// its shards in `shards` (each by its slot, as [`Shard::parse`] read it).
    pub fn add_run<'a>(
        &mut self,
        run: &Run,
        shards: impl IntoIterator<Item = &'a Shard>,
        keep: impl Fn(&Hash) -> bool,
    ) {
        self.prefix = self.prefix.min(run.prefix);
        let ids: Vec<Option<u32>> =
            run.packs.iter().map(|pack| keep(pack).then(|| self.id(pack))).collect();
        for shard in shards {
            for (entry, pack) in shard.all() {
                if let Some(Some(id)) = ids.get(pack as usize) {
                    self.entries.push((entry, *id));
                }
            }
        }
    }

    /// Whether `pack` is covered: added, or of a run added and kept.
    pub fn covers(&self, pack: &Hash) -> bool {
        self.ids.contains_key(pack)
    }

    /// How many entries were added, before an entry added twice for one pack is kept once.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The run, cut into shards of about [`SHARD_TARGET`] bytes. Every pack that holds a key
    /// keeps its entry: a collection of the remote may take out any one of them.
    pub fn finish(self) -> Built {
        let Builder { prefix, packs, entries: mut placed, .. } = self;
        let mut order: Vec<u32> = (0..packs.len() as u32).collect();
        order.sort_by_key(|it| packs[*it as usize]);
        let mut place = vec![0u32; packs.len()];
        for (at, id) in order.iter().enumerate() {
            place[*id as usize] = at as u32;
        }
        for entry in &mut placed {
            *entry = (truncated(entry.0, prefix), place[entry.1 as usize]);
        }
        placed.sort_unstable();
        placed.dedup();
        let bits = (0..=BITS_MAX)
            .find(|bits| (placed.len() as u64 * u64::from(prefix)) >> bits <= SHARD_TARGET)
            .unwrap_or(BITS_MAX);
        // By slot, then by pack, then by entry: each slot's groups in the order written.
        placed.sort_unstable_by_key(|(entry, pack)| (slot_of(*entry, bits), *pack, *entry));
        let mut run = Vec::with_capacity(RUN_HEAD + packs.len() * 32 + (RUN_SHARD << bits));
        run.extend_from_slice(RUN_MAGIC);
        run.extend_from_slice(&[bits, prefix, 0, 0]);
        run.extend_from_slice(&(packs.len() as u32).to_le_bytes());
        for id in &order {
            run.extend_from_slice(&packs[*id as usize]);
        }
        let mut shards = Vec::new();
        let mut named = vec![([0u8; 32], 0u32); 1 << bits];
        for slot in placed.chunk_by(|a, b| slot_of(a.0, bits) == slot_of(b.0, bits)) {
            let at = slot_of(slot[0].0, bits);
            let groups: Vec<&[(u64, u32)]> = slot.chunk_by(|a, b| a.1 == b.1).collect();
            let mut shard = Vec::with_capacity(
                SHARD_HEAD + groups.len() * GROUP + slot.len() * prefix as usize,
            );
            shard.extend_from_slice(SHARD_MAGIC);
            shard.extend_from_slice(&[bits, prefix, 0, 0]);
            shard.extend_from_slice(&(at as u32).to_le_bytes());
            shard.extend_from_slice(&(groups.len() as u32).to_le_bytes());
            for group in &groups {
                shard.extend_from_slice(&group[0].1.to_le_bytes());
                shard.extend_from_slice(&(group.len() as u32).to_le_bytes());
            }
            for (entry, _) in slot {
                shard.extend_from_slice(&entry.to_be_bytes()[..prefix as usize]);
            }
            named[at] = (Sha256::digest(&shard).into(), slot.len() as u32);
            shards.push(shard);
        }
        for (hash, count) in named {
            run.extend_from_slice(&hash);
            run.extend_from_slice(&count.to_le_bytes());
        }
        Built { run, shards, entries: placed.len() as u64, packs: packs.len() as u32 }
    }
}

/// A run as a root names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub run: Hash,
    pub entries: u64,
    pub packs: u32,
}

/// A root: the runs it names, oldest first, and the lines it has that this format does
/// not know, which are written back as they are.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Root {
    pub runs: Vec<Named>,
    pub other: Vec<String>,
}

impl Root {
    /// The root in the lines of its list, as `signed.rs` opened them. A line of a run that
    /// does not parse is kept with the lines of other kinds, and names no run.
    pub fn parse(lines: &str) -> Root {
        let mut root = Root::default();
        for line in lines.lines() {
            let run = || {
                let mut fields = line.split(' ');
                (fields.next()? == RUN_LINE).then_some(())?;
                let run = unhex(fields.next()?)?;
                let entries = number(fields.next()?)?;
                let packs = u32::try_from(number(fields.next()?)?).ok()?;
                fields.next().is_none().then_some(Named { run, entries, packs })
            };
            match run() {
                Some(named) if !root.runs.iter().any(|it| it.run == named.run) => {
                    root.runs.push(named)
                }
                Some(_) => {}
                None => root.other.push(line.to_owned()),
            }
        }
        root
    }

    pub fn lines(&self) -> String {
        let mut text = String::new();
        for line in &self.other {
            text.push_str(line);
            text.push('\n');
        }
        for run in &self.runs {
            _ = writeln!(text, "{RUN_LINE} {} {} {}", hex(&run.run), run.entries, run.packs);
        }
        text
    }
}

/// A number written in the one way there is to write it.
fn number(text: &str) -> Option<u64> {
    let number: u64 = text.parse().ok()?;
    (number.to_string() == text).then_some(number)
}

pub fn hex(hash: &Hash) -> String {
    hash.iter().fold(String::with_capacity(64), |mut text, byte| {
        _ = write!(text, "{byte:02x}");
        text
    })
}

fn unhex(text: &str) -> Option<Hash> {
    if text.len() != 64 || !text.bytes().all(|it| matches!(it, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut hash = [0; 32];
    for (byte, pair) in hash.iter_mut().zip(text.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(hash)
}

/// Which of `runs` (their entries, as the root says) to merge into one, if any. A run's
/// tier is the power of [`FANOUT`] its entries are at: once [`FANOUT`] runs are of one tier,
/// they and every smaller run are merged, and past [`RUNS_MAX`] runs the smallest are,
/// whatever their tiers. So a push of a few packs merges small runs often and cheaply, and
/// the run that holds most of the project is written again about once for each time the
/// project grows by a factor of [`FANOUT`].
pub fn plan_merge(runs: &[u64]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..runs.len()).collect();
    order.sort_by_key(|it| (runs[*it], *it));
    let tier = |at: &usize| runs[*at].max(1).ilog(FANOUT as u64);
    let (mut seen, mut take) = (0, 0);
    for same in order.chunk_by(|a, b| tier(a) == tier(b)) {
        seen += same.len();
        if same.len() >= FANOUT {
            take = seen;
            break;
        }
    }
    if runs.len() - take.saturating_sub(1) > RUNS_MAX {
        take = runs.len() - RUNS_MAX + 1;
    }
    if take < 2 {
        return Vec::new();
    }
    let mut chosen = order[..take].to_vec();
    chosen.sort_unstable();
    chosen
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn key(seed: u64) -> [u8; KEY] {
        Sha256::digest(seed.to_le_bytes()).into()
    }

    fn pack(n: u8) -> Hash {
        [n; 32]
    }

    /// Every shard of a built run, parsed as a reader would after checking its hash.
    fn opened(built: &Built) -> (Run, BTreeMap<usize, Shard>) {
        let run = Run::parse(&built.run).unwrap();
        let by_hash: FxHashMap<Hash, &Vec<u8>> =
            built.shards.iter().map(|it| (Sha256::digest(it).into(), it)).collect();
        let shards = run
            .shards()
            .map(|(slot, hash)| (slot, Shard::parse(by_hash[&hash], &run, slot).unwrap()))
            .collect();
        (run, shards)
    }

    fn holders(run: &Run, shards: &BTreeMap<usize, Shard>, key: &[u8; KEY]) -> Vec<Hash> {
        match shards.get(&run.slot(key)) {
            Some(shard) => run.holders(shard, key).copied().collect(),
            None => Vec::new(),
        }
    }

    #[test]
    fn a_key_is_found_in_the_pack_that_holds_it() {
        for count in [0u64, 1, 1000, 200_000] {
            let mut builder = Builder::default();
            let keys: Vec<_> = (0..count).map(key).collect();
            for (n, chunk) in keys.chunks(keys.len().div_ceil(5).max(1)).enumerate() {
                builder.add_pack(&pack(n as u8 + 1), chunk);
            }
            builder.add_pack(&pack(99), []);
            let built = builder.finish();
            assert!(built.shards.iter().all(|it| it.len() as u64 <= SHARD_TARGET + 4096));
            let (run, shards) = opened(&built);
            assert_eq!(run.entries(), count);
            assert!(run.packs().contains(&pack(99)), "a pack with no keys is covered");
            for (at, key) in keys.iter().enumerate() {
                let n = at / keys.len().div_ceil(5).max(1);
                assert_eq!(holders(&run, &shards, key), vec![pack(n as u8 + 1)]);
            }
            let absent =
                (count..count + 1000).filter(|it| !holders(&run, &shards, &key(*it)).is_empty());
            assert_eq!(absent.count(), 0);
        }
    }

    #[test]
    fn a_key_two_packs_hold_is_found_in_both() {
        let keys: Vec<_> = (0..100).map(key).collect();
        let mut builder = Builder::default();
        builder.add_pack(&pack(1), &keys[..10]);
        builder.add_pack(&pack(2), &keys);
        // A pack indexed twice, as two machines pushing at once do, is in the run once.
        builder.add_pack(&pack(1), &keys[..10]);
        let (run, shards) = opened(&builder.finish());
        assert_eq!(run.entries(), 110);
        for (at, key) in keys.iter().enumerate() {
            let want = if at < 10 { vec![pack(1), pack(2)] } else { vec![pack(2)] };
            assert_eq!(holders(&run, &shards, key), want);
        }
        // Once the larger is taken out, the smaller still has its keys.
        let mut merged = Builder::default();
        merged.add_run(&run, shards.values(), |it| *it != pack(2));
        let (run, shards) = opened(&merged.finish());
        assert!(keys[..10].iter().all(|key| holders(&run, &shards, key) == vec![pack(1)]));
    }

    #[test]
    fn a_merged_run_keeps_what_is_still_listed() {
        let keys: Vec<_> = (0..5000).map(key).collect();
        let mut first = Builder::default();
        first.add_pack(&pack(1), &keys[..2000]);
        first.add_pack(&pack(2), &keys[2000..3000]);
        let (first, first_shards) = opened(&first.finish());
        let mut merged = Builder::default();
        merged.add_pack(&pack(3), &keys[3000..]);
        merged.add_run(&first, first_shards.values(), |it| *it != pack(2));
        let (run, shards) = opened(&merged.finish());
        assert_eq!(run.packs(), &[pack(1), pack(3)]);
        for (at, key) in keys.iter().enumerate() {
            let want = match at {
                ..2000 => vec![pack(1)],
                2000..3000 => vec![],
                _ => vec![pack(3)],
            };
            assert_eq!(holders(&run, &shards, key), want, "key {at}");
        }
    }

    #[test]
    fn the_same_packs_make_the_same_run() {
        let keys: Vec<_> = (0..3000).map(key).collect();
        let build = |order: &[u8]| {
            let mut builder = Builder::default();
            for n in order {
                builder.add_pack(&pack(*n), &keys[*n as usize * 1000..][..1000]);
            }
            builder.finish().run
        };
        assert_eq!(build(&[0, 1, 2]), build(&[2, 0, 1]));
    }

    /// `seed` and `runs` changes of it, the same in every run: bytes flipped, cut, put in
    /// and taken out.
    fn changed(seed: &[u8], runs: usize) -> impl Iterator<Item = Vec<u8>> {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound.max(1) as u64) as usize
        };
        let seed = seed.to_vec();
        std::iter::once(seed.clone()).chain((0..runs).map(move |_| {
            let mut bytes = seed.clone();
            for _ in 0..=next(4) {
                let at = next(bytes.len());
                match next(4) {
                    0 => bytes.truncate(at),
                    1 if !bytes.is_empty() => bytes[at] ^= 1 << next(8),
                    2 => bytes.insert(at, [0, 1, 0x7f, 0x80, 0xff][next(5)]),
                    _ if !bytes.is_empty() => _ = bytes.remove(at),
                    _ => {}
                }
            }
            bytes
        }))
    }

    #[test]
    fn no_bytes_break_the_reading_of_a_run_or_a_shard() {
        let keys: Vec<_> = (0..300).map(key).collect();
        let mut builder = Builder::default();
        builder.add_pack(&pack(1), &keys[..200]);
        builder.add_pack(&pack(2), &keys[200..]);
        let built = builder.finish();
        let run = Run::parse(&built.run).unwrap();
        let (slot, _) = run.shards().next().unwrap();
        assert!(built.run.len() as u64 <= run_max(2));
        for (slot, hash) in run.shards() {
            let shard = built.shards.iter().find(|it| Sha256::digest(it)[..] == hash[..]).unwrap();
            assert!(shard.len() as u64 <= run.shard_max(slot));
        }
        for bytes in changed(&built.run, 5000) {
            if let Ok(run) = Run::parse(&bytes) {
                _ = run.entries();
            }
        }
        for bytes in changed(&built.shards[0], 5000) {
            if let Ok(shard) = Shard::parse(&bytes, &run, slot) {
                for key in &keys {
                    _ = run.holders(&shard, key).count();
                }
            }
        }
    }

    #[test]
    fn a_root_keeps_the_lines_it_does_not_know() {
        let text = format!(
            "run2 something new\n{RUN_LINE} {} 10 2\n{RUN_LINE} {} 10 2\n{RUN_LINE} {} 3 1 x\n",
            "ab".repeat(32),
            "ab".repeat(32),
            "cd".repeat(32),
        );
        let root = Root::parse(&text);
        assert_eq!(root.runs, vec![Named { run: [0xab; 32], entries: 10, packs: 2 }]);
        assert_eq!(root.other.len(), 2);
        assert_eq!(Root::parse(&root.lines()), root);
    }

    #[test]
    fn runs_of_about_one_size_are_merged() {
        assert_eq!(plan_merge(&[]), Vec::<usize>::new());
        assert_eq!(plan_merge(&[100, 100, 100]), Vec::<usize>::new());
        assert_eq!(plan_merge(&[100, 120, 90, 110]), vec![0, 1, 2, 3]);
        // The large run stays until the small ones come near it.
        assert_eq!(plan_merge(&[1_000_000, 100, 120, 90, 110]), vec![1, 2, 3, 4]);
        assert_eq!(plan_merge(&[1_000_000, 100, 120, 90]), Vec::<usize>::new());
        // Too many runs merge the smallest, whatever their sizes.
        let spread: Vec<u64> = (0..30).map(|it| 1 << (2 * it)).collect();
        let chosen = plan_merge(&spread);
        assert_eq!(spread.len() - chosen.len() + 1, RUNS_MAX);
        assert_eq!(chosen, (0..chosen.len()).collect::<Vec<_>>());
    }
}
