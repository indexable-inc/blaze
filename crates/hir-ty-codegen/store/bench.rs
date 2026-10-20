//! What the microbenchmarks of `crates/ra-jit-bench` measure: the keys a function is kept
//! under, the records of a pack and the store's reads. Not for anything else; the names here
//! may change with the benchmarks.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use super::{Identity, KEY, Pack, StoreDir, Tuning, record};

/// The key of the entry for `name`, as a build asks for it.
pub fn entry_key(name: &str, debug: bool, program: &[u8; KEY]) -> Option<[u8; KEY]> {
    super::super::cached::Entry::key(name, debug, Some(program))
}

/// A key as the store has it on disk: the entry's key under the identity of the build.
pub struct Salted(Identity);

impl Salted {
    pub fn new(build: &[u8], toolchain: &str) -> Salted {
        Salted(Identity::new(build, toolchain))
    }

    pub fn key(&self, key: &[u8; KEY]) -> [u8; KEY] {
        // PROBE, not to be merged: a deliberate slowdown, to see the comparison fail.
        self.0.key(&self.0.key(key))
    }
}

/// The bytes of a record of a pack: header, checksum and value.
pub fn encode_record(key: &[u8; KEY], value: &[u8]) -> Vec<u8> {
    record(key, value, value.len() as u32)
}

/// How many records a sealed pack has, once its bytes are checked against its name and each
/// record against its checksum.
pub fn check_pack(name: &str, bytes: &[u8]) -> Result<usize, &'static str> {
    Pack::check(name, bytes).map(|it| it.len())
}

/// A store in a directory of its own with one sealed pack, which goes with it.
pub struct SealedStore {
    store: Option<super::Store>,
    dir: PathBuf,
    keys: Vec<[u8; KEY]>,
    pack: (String, Vec<u8>),
}

impl SealedStore {
    /// `count` values of `len` bytes each, under keys made of their index.
    pub fn new(tag: &str, count: usize, len: usize) -> std::io::Result<SealedStore> {
        static MADE: AtomicUsize = AtomicUsize::new(0);
        let made = MADE.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ra-jit-bench-{tag}-{}-{made}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        let identity = Identity::new(b"a build", "a toolchain");
        // Nothing is sealed until all of it is in, and then all of it in one pack.
        let tuning = Tuning { seal_at: 1 << 30, limit: None };
        let store = super::Store::open(&dir, identity, tuning)?;
        let keys: Vec<[u8; KEY]> = (0..count).map(|it| key_of(it as u64)).collect();
        for (at, key) in keys.iter().enumerate() {
            let value: Vec<u8> = (0..len).map(|it| (it ^ at) as u8).collect();
            store.insert(key, &value);
        }
        store.settle(1);
        let names = StoreDir::at(&dir).packs()?;
        let [name] = &names[..] else {
            return Err(std::io::Error::other(format!("{} packs, not one", names.len())));
        };
        let bytes = std::fs::read(dir.join(name))?;
        Ok(SealedStore { pack: (name.clone(), bytes), store: Some(store), dir, keys })
    }

    pub fn keys(&self) -> &[[u8; KEY]] {
        &self.keys
    }

    /// The name and the bytes of the sealed pack.
    pub fn pack(&self) -> (&str, &[u8]) {
        (&self.pack.0, &self.pack.1)
    }

    pub fn get(&self, key: &[u8; KEY]) -> Option<Vec<u8>> {
        self.store.as_ref()?.get(key)
    }

    pub fn has(&self, key: &[u8; KEY]) -> bool {
        self.store.as_ref().is_some_and(|it| it.has(key))
    }
}

impl Drop for SealedStore {
    fn drop(&mut self) {
        drop(self.store.take());
        _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A key that is the same for the same number, and spread like a hash.
pub fn key_of(number: u64) -> [u8; KEY] {
    use sha2::Digest;
    sha2::Sha256::digest(number.to_le_bytes()).into()
}
