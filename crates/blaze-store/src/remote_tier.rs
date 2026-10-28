//! The remote as a tier under the store: what a store that lacks a key fetches to find the
//! pack that holds it. Pure logic over bytes, with no files, sockets or clocks: the adapter
//! (`blaze-cargo`'s `remote/tier.rs`) fetches the objects this asks for, from its own disk
//! or from the remote, and hands their bytes back.
//!
//! A [`Tier`] is one look at a project on the remote: the runs of its key index
//! ([`crate::remote_index`]) the root named, and the packs of this build's format the
//! manifest listed, both read once per session. [`Tier::lookup`] of a key says either what
//! to fetch first (every run, then the key's shard of each run, and the key lists of listed
//! packs no run covers) or, once those are in, the packs that may hold the key, newest run
//! first. Every object is checked against the hash it is named by before it is used
//! ([`Tier::take`]), and is never asked for again: runs and shards are immutable.
//!
//! The holders are candidates. A run's entry is the first bytes of a key, so a pack it names
//! may not hold the key; the store checks the key in the fetched pack and asks for the next
//! candidate. A pack is a candidate only if the manifest lists it with this build's format:
//! runs do not record the format of their packs, and a pack taken out of the manifest stays
//! in its run until a merge. A key no candidate holds is a true miss, and is compiled.
//!
//! A run or shard that cannot be had, or is not what its name says, is an error naming it:
//! leaving that run out would make a key it alone covers look like a miss, which compiles,
//! and that is the fallback blaze does not take. A push takes such a run out and indexes
//! its packs again (`remote/index.rs`).
//!
//! A listed pack that no run covers (pushed by a blaze from before the index, or between a
//! push's manifest and its index) is found through its key list, which names every key of
//! the pack in full: that is complete, only larger. A pack the store has needs no list.

use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};
use sha2::{Digest as _, Sha256};

use crate::{
    KEY,
    remote_index::{self, Hash, Named, Root, Run, Shard},
};

/// A pack the manifest lists with this build's format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listed {
    pub pack: Hash,
    /// The hash of the list of its keys.
    pub keys: Hash,
    /// How many bytes the pack is: its list of keys is no longer.
    pub size: u64,
}

/// An object a lookup needs that the tier does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Want {
    /// A run the root names, of at most `most` bytes.
    Run { run: Hash, most: u64 },
    /// The shard in `slot` of `run`, of at most `most` bytes.
    Shard { run: Hash, slot: usize, shard: Hash, most: u64 },
    /// The list of the keys of `pack`, named by its hash `keys`, of at most `most` bytes.
    Keys { pack: Hash, keys: Hash, most: u64 },
}

impl Want {
    /// What the object's bytes hash to, which is also its name.
    pub fn hash(&self) -> &Hash {
        match self {
            Want::Run { run, .. } => run,
            Want::Shard { shard, .. } => shard,
            Want::Keys { keys, .. } => keys,
        }
    }

    /// The most bytes the object can be: ask for no more.
    pub fn most(&self) -> u64 {
        match self {
            Want::Run { most, .. } | Want::Shard { most, .. } | Want::Keys { most, .. } => *most,
        }
    }

    /// Whether it is an object of the index, `index/<hash>`, rather than a key list.
    pub fn is_index(&self) -> bool {
        !matches!(self, Want::Keys { .. })
    }

    /// The remote's name of the object.
    pub fn name(&self) -> String {
        match self {
            Want::Keys { keys, .. } => format!("keys/{}", remote_index::hex(keys)),
            _ => format!("index/{}", remote_index::hex(self.hash())),
        }
    }
}

/// What a lookup comes to.
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    /// These objects are needed first; ask again once they are taken.
    Fetch(Vec<Want>),
    /// The listed packs that may hold the key, the likeliest first. None: no pack the
    /// manifest lists holds it.
    Holders(Vec<Hash>),
}

/// Why an object cannot be used: it names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unusable(pub String);

impl std::fmt::Display for Unusable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unusable {}

/// Whether `bytes` can be the object `want` asks for: no longer than it can be, and hashing
/// to its name. [`Tier::take`] checks this and what the bytes say.
pub fn check(want: &Want, bytes: &[u8]) -> Result<(), Unusable> {
    let name = want.name();
    if bytes.len() as u64 > want.most() {
        return Err(Unusable(format!(
            "{name} is {} bytes, more than the {} it can be",
            bytes.len(),
            want.most()
        )));
    }
    let got: Hash = Sha256::digest(bytes).into();
    if got != *want.hash() {
        return Err(Unusable(format!(
            "{name}: its {} bytes hash to {}",
            bytes.len(),
            remote_index::hex(&got)
        )));
    }
    Ok(())
}

/// One look at a project on the remote: see the module.
pub struct Tier {
    /// The runs the root names, newest first.
    runs: Vec<Named>,
    /// The packs the manifest lists with this build's format.
    listed: FxHashMap<Hash, Listed>,
    /// Packs the store had when the look was taken: their keys are found without the tier.
    here: FxHashSet<Hash>,
    /// The runs read, which outlive a renewal while the root still names them.
    parsed: FxHashMap<Hash, Arc<Run>>,
    /// The shards read, by their hash.
    shards: FxHashMap<Hash, Arc<Shard>>,
    /// Once every run is read: the listed packs no run covers and the store lacks, whose
    /// key lists are read instead.
    uncovered: Option<Vec<Hash>>,
    /// The keys of those whose lists were read, and the pack of each.
    loose: FxHashMap<[u8; KEY], u32>,
    loose_packs: Vec<Hash>,
    loose_read: FxHashSet<Hash>,
    /// How many bytes of key lists have been read, and the most that may be.
    loose_bytes: u64,
    loose_max: u64,
}

impl Tier {
    /// The tier for a root, the packs its manifest lists with this build's format, and the
    /// packs the store has. Key lists of uncovered packs are read to `loose_max` bytes in
    /// all; past that a lookup is an error.
    pub fn new(
        root: &Root,
        listed: impl IntoIterator<Item = Listed>,
        here: impl IntoIterator<Item = Hash>,
        loose_max: u64,
    ) -> Tier {
        let mut tier = Tier {
            runs: Vec::new(),
            listed: FxHashMap::default(),
            here: FxHashSet::default(),
            parsed: FxHashMap::default(),
            shards: FxHashMap::default(),
            uncovered: None,
            loose: FxHashMap::default(),
            loose_packs: Vec::new(),
            loose_read: FxHashSet::default(),
            loose_bytes: 0,
            loose_max,
        };
        tier.renew(root, listed, here);
        tier
    }

    /// A new look at the project, keeping what was read that it still names.
    pub fn renew(
        &mut self,
        root: &Root,
        listed: impl IntoIterator<Item = Listed>,
        here: impl IntoIterator<Item = Hash>,
    ) {
        self.runs = root.runs.iter().rev().cloned().collect();
        self.listed = listed.into_iter().map(|it| (it.pack, it)).collect();
        self.here = here.into_iter().collect();
        let named: FxHashSet<Hash> = self.runs.iter().map(|it| it.run).collect();
        self.parsed.retain(|run, _| named.contains(run));
        let shards: FxHashSet<Hash> =
            self.parsed.values().flat_map(|run| run.shards().map(|(_, it)| it)).collect();
        self.shards.retain(|shard, _| shards.contains(shard));
        self.uncovered = None;
        self.settle();
        // What was read of lists of packs that are still uncovered is kept.
        if let Some(uncovered) = &self.uncovered {
            let still: FxHashSet<&Hash> = uncovered.iter().collect();
            let packs = &self.loose_packs;
            self.loose.retain(|_, at| still.contains(&packs[*at as usize]));
            self.loose_read.retain(|it| still.contains(it));
        } else {
            self.loose.clear();
            self.loose_read.clear();
        }
        if self.loose.is_empty() {
            self.loose_packs.clear();
            self.loose_bytes = 0;
        }
    }

    /// Works out the uncovered packs once every run is read.
    fn settle(&mut self) {
        if self.uncovered.is_some() || self.runs.iter().any(|it| !self.parsed.contains_key(&it.run))
        {
            return;
        }
        let covered: FxHashSet<&Hash> =
            self.parsed.values().flat_map(|run| run.packs().iter()).collect();
        let mut uncovered: Vec<Hash> = (self.listed.keys())
            .filter(|pack| !covered.contains(pack) && !self.here.contains(*pack))
            .copied()
            .collect();
        uncovered.sort_unstable();
        self.uncovered = Some(uncovered);
    }

    /// What finding `key` takes next: see the module.
    pub fn lookup(&self, key: &[u8; KEY]) -> Lookup {
        let unread: Vec<Want> = (self.runs.iter())
            .filter(|named| !self.parsed.contains_key(&named.run))
            .map(|named| Want::Run { run: named.run, most: remote_index::run_max(named.packs) })
            .collect();
        if !unread.is_empty() {
            return Lookup::Fetch(unread);
        }
        let mut wants = Vec::new();
        for pack in self.uncovered.iter().flatten() {
            if !self.loose_read.contains(pack) {
                let listed = &self.listed[pack];
                wants.push(Want::Keys { pack: *pack, keys: listed.keys, most: listed.size });
            }
        }
        let mut shards = Vec::with_capacity(self.runs.len());
        for named in &self.runs {
            let run = &self.parsed[&named.run];
            let slot = run.slot(key);
            let Some(hash) = run.shard(slot) else { continue };
            match self.shards.get(&hash) {
                Some(shard) => shards.push((run, shard)),
                None => wants.push(Want::Shard {
                    run: named.run,
                    slot,
                    shard: hash,
                    most: run.shard_max(slot),
                }),
            }
        }
        if !wants.is_empty() {
            return Lookup::Fetch(wants);
        }
        let mut holders: Vec<Hash> = Vec::new();
        let mut hold = |pack: &Hash| {
            if self.listed.contains_key(pack) && !holders.contains(pack) {
                holders.push(*pack);
            }
        };
        for (run, shard) in shards {
            run.holders(shard, key).for_each(&mut hold);
        }
        if let Some(at) = self.loose.get(key) {
            hold(&self.loose_packs[*at as usize]);
        }
        Lookup::Holders(holders)
    }

    /// Takes the bytes of an object a lookup asked for, once they hash to its name and are
    /// what it is. Bytes of an object no longer needed (the tier was renewed since it was
    /// asked for) are checked and dropped.
    pub fn take(&mut self, want: &Want, bytes: &[u8]) -> Result<(), Unusable> {
        check(want, bytes)?;
        let name = want.name();
        match *want {
            Want::Run { run, .. } => {
                let Some(named) = self.runs.iter().find(|it| it.run == run) else { return Ok(()) };
                let parsed = Run::parse(bytes).map_err(|why| Unusable(format!("{name} {why}")))?;
                if parsed.packs().len() != named.packs as usize {
                    return Err(Unusable(format!("{name} is not of the packs its root says")));
                }
                self.parsed.insert(run, Arc::new(parsed));
                self.settle();
            }
            Want::Shard { run, slot, shard, .. } => {
                let Some(parsed) = self.parsed.get(&run) else { return Ok(()) };
                if parsed.shard(slot) != Some(shard) {
                    return Ok(());
                }
                let read = Shard::parse(bytes, parsed, slot)
                    .map_err(|why| Unusable(format!("{name} {why}")))?;
                self.shards.insert(shard, Arc::new(read));
            }
            Want::Keys { pack, .. } => {
                let wanted =
                    self.uncovered.as_ref().is_some_and(|it| it.binary_search(&pack).is_ok());
                if !wanted || self.loose_read.contains(&pack) {
                    return Ok(());
                }
                if bytes.len() % KEY != 0 {
                    return Err(Unusable(format!("{name} is {} bytes, not keys", bytes.len())));
                }
                self.loose_bytes += bytes.len() as u64;
                if self.loose_bytes > self.loose_max {
                    return Err(Unusable(format!(
                        "the key lists of the packs no run of the index covers are over {} \
                         bytes: a push of a blaze that writes the index covers them",
                        self.loose_max
                    )));
                }
                let at = self.loose_packs.len() as u32;
                self.loose_packs.push(pack);
                for key in bytes.as_chunks::<KEY>().0 {
                    self.loose.entry(*key).or_insert(at);
                }
                self.loose_read.insert(pack);
            }
        }
        Ok(())
    }

    /// The runs and shards this look names that were read: what a cache of them keeps.
    pub fn named(&self) -> FxHashSet<Hash> {
        let runs = self.parsed.keys().copied();
        runs.chain(self.parsed.values().flat_map(|run| run.shards().map(|(_, it)| it))).collect()
    }

    /// The listed pack `pack`, if the manifest lists it with this build's format.
    pub fn listed(&self, pack: &Hash) -> Option<&Listed> {
        self.listed.get(pack)
    }

    /// How many runs the root names, and how many listed packs no run covers.
    pub fn counts(&self) -> (usize, Option<usize>) {
        (self.runs.len(), self.uncovered.as_ref().map(Vec::len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_index::Builder;

    fn key(seed: u64) -> [u8; KEY] {
        Sha256::digest(seed.to_le_bytes()).into()
    }

    fn pack(n: u8) -> Hash {
        [n; 32]
    }

    fn sha(bytes: &[u8]) -> Hash {
        Sha256::digest(bytes).into()
    }

    /// The objects of a remote: by hash, as the adapter fetches them.
    #[derive(Default)]
    struct Remote(FxHashMap<Hash, Vec<u8>>);

    impl Remote {
        fn put(&mut self, bytes: Vec<u8>) -> Hash {
            let hash = sha(&bytes);
            self.0.insert(hash, bytes);
            hash
        }

        /// A run of `packs`, each holding its keys, put with its shards: its root line.
        fn run(&mut self, packs: &[(Hash, &[[u8; KEY]])]) -> Named {
            let mut builder = Builder::default();
            for (pack, keys) in packs {
                builder.add_pack(pack, keys.iter());
            }
            let built = builder.finish();
            for shard in built.shards {
                self.put(shard);
            }
            Named { run: self.put(built.run), entries: built.entries, packs: built.packs }
        }

        /// Looks `key` up as the adapter does, fetching what is asked for: the holders and
        /// how many objects were fetched.
        fn find(&self, tier: &mut Tier, key: &[u8; KEY]) -> Result<(Vec<Hash>, usize), Unusable> {
            let mut fetched = 0;
            loop {
                match tier.lookup(key) {
                    Lookup::Holders(holders) => return Ok((holders, fetched)),
                    Lookup::Fetch(wants) => {
                        assert!(!wants.is_empty());
                        for want in wants {
                            let bytes = self
                                .0
                                .get(want.hash())
                                .ok_or_else(|| Unusable(format!("{} is not there", want.name())))?;
                            tier.take(&want, bytes)?;
                            fetched += 1;
                        }
                    }
                }
            }
        }
    }

    fn listed(remote: &mut Remote, n: u8, keys: &[[u8; KEY]]) -> Listed {
        let keys = remote.put(keys.concat());
        Listed { pack: pack(n), keys, size: 1 << 20 }
    }

    #[test]
    fn a_key_is_found_through_the_runs_and_the_key_lists() {
        let keys: Vec<_> = (0..4000).map(key).collect();
        let mut remote = Remote::default();
        let old = remote.run(&[(pack(1), &keys[..1000]), (pack(2), &keys[1000..2000])]);
        let new = remote.run(&[(pack(3), &keys[2000..3000])]);
        let root = Root { runs: vec![old, new], other: Vec::new() };
        let listed: Vec<Listed> = (1..=4)
            .map(|n| listed(&mut remote, n, &keys[(n as usize - 1) * 1000..][..1000]))
            .collect();
        let mut tier = Tier::new(&root, listed, [], u64::MAX);
        let (holders, fetched) = remote.find(&mut tier, &keys[0]).unwrap();
        assert_eq!(holders, vec![pack(1)]);
        // Both runs, the key list of the pack no run covers, and the key's shard of each.
        assert_eq!(fetched, 2 + 1 + 2);
        assert_eq!(tier.counts(), (2, Some(1)));
        for (at, key) in keys.iter().enumerate() {
            let (holders, _) = remote.find(&mut tier, key).unwrap();
            assert_eq!(holders, vec![pack((at / 1000) as u8 + 1)], "key {at}");
        }
        // Everything is read once: a lookup of a key read before fetches nothing.
        assert_eq!(remote.find(&mut tier, &keys[5]).unwrap().1, 0);
        let (holders, _) = remote.find(&mut tier, &key(99_999)).unwrap();
        assert!(holders.is_empty());
    }

    #[test]
    fn only_listed_packs_are_holders_and_the_newest_run_comes_first() {
        let keys: Vec<_> = (0..100).map(key).collect();
        let mut remote = Remote::default();
        let old = remote.run(&[(pack(1), &keys), (pack(2), &keys)]);
        let new = remote.run(&[(pack(3), &keys)]);
        let root = Root { runs: vec![old, new], other: Vec::new() };
        // Pack 2 was taken out of the manifest, or is of another format.
        let listed = [1, 3].map(|n| listed(&mut remote, n, &keys));
        let mut tier = Tier::new(&root, listed, [], u64::MAX);
        assert_eq!(remote.find(&mut tier, &keys[7]).unwrap().0, vec![pack(3), pack(1)]);
    }

    #[test]
    fn a_pack_the_store_has_needs_no_key_list() {
        let keys: Vec<_> = (0..100).map(key).collect();
        let mut remote = Remote::default();
        let listed = [listed(&mut remote, 1, &keys[..50]), listed(&mut remote, 2, &keys[50..])];
        let mut tier = Tier::new(&Root::default(), listed, [pack(1)], u64::MAX);
        assert_eq!(remote.find(&mut tier, &keys[0]).unwrap(), (vec![], 1));
        assert_eq!(remote.find(&mut tier, &keys[60]).unwrap(), (vec![pack(2)], 0));
    }

    #[test]
    fn an_object_that_is_not_what_its_name_says_is_an_error_naming_it() {
        let keys: Vec<_> = (0..100).map(key).collect();
        let mut remote = Remote::default();
        let named = remote.run(&[(pack(1), &keys)]);
        let root = Root { runs: vec![named.clone()], other: Vec::new() };
        let listed = [listed(&mut remote, 1, &keys)];
        // A shard gone: the lookup fails, naming it, and does not call the key a miss.
        let run = Run::parse(&remote.0[&named.run]).unwrap();
        let (_, shard) = run.shards().next().unwrap();
        let mut broken = Remote(remote.0.clone());
        broken.0.remove(&shard);
        let mut tier = Tier::new(&root, listed, [], u64::MAX);
        let missing = (0..100).map(key).find(|it| run.shard(run.slot(it)) == Some(shard)).unwrap();
        let e = broken.find(&mut tier, &missing).unwrap_err();
        assert!(e.0.contains(&remote_index::hex(&shard)), "{e}");
        // Bytes of another object under a name are refused.
        let want = match tier.lookup(&missing) {
            Lookup::Fetch(wants) => wants[0].clone(),
            other => panic!("{other:?}"),
        };
        let e = tier.take(&want, &remote.0[&named.run]).unwrap_err();
        assert!(e.0.contains("hash to"), "{e}");
        // A run that is not of the packs its root line says is refused.
        let lying = Named { packs: 2, ..named };
        let mut tier =
            Tier::new(&Root { runs: vec![lying], other: Vec::new() }, listed, [], u64::MAX);
        let e = remote.find(&mut tier, &keys[0]).unwrap_err();
        assert!(e.0.contains("not of the packs"), "{e}");
    }

    #[test]
    fn a_renewed_tier_keeps_what_its_root_still_names() {
        let keys: Vec<_> = (0..300).map(key).collect();
        let mut remote = Remote::default();
        let first = remote.run(&[(pack(1), &keys[..100])]);
        let second = remote.run(&[(pack(2), &keys[100..200])]);
        let listed3 = listed(&mut remote, 3, &keys[200..]);
        let all = [listed(&mut remote, 1, &keys[..100]), listed(&mut remote, 2, &keys[100..200])];
        let root = Root { runs: vec![first.clone()], other: Vec::new() };
        let mut tier = Tier::new(&root, [all[0]], [], u64::MAX);
        assert_eq!(remote.find(&mut tier, &keys[0]).unwrap().0, vec![pack(1)]);
        let root = Root { runs: vec![first, second], other: Vec::new() };
        tier.renew(&root, [all[0], all[1], listed3], []);
        // The first run and its shard are kept; the second run, its shard and the key
        // list of pack 3 are read.
        assert_eq!(remote.find(&mut tier, &keys[0]).unwrap(), (vec![pack(1)], 3));
        assert_eq!(remote.find(&mut tier, &keys[150]).unwrap().0, vec![pack(2)]);
        assert_eq!(remote.find(&mut tier, &keys[250]).unwrap().0, vec![pack(3)]);
    }

    #[test]
    fn key_lists_past_the_most_are_an_error() {
        let keys: Vec<_> = (0..100).map(key).collect();
        let mut remote = Remote::default();
        let listed = [listed(&mut remote, 1, &keys)];
        let mut tier = Tier::new(&Root::default(), listed, [], 32 * 99);
        assert!(remote.find(&mut tier, &keys[0]).unwrap_err().0.contains("key lists"));
    }
}
