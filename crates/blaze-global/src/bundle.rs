//! A cold build of a lock file's crates.io dependencies as one download.
//!
//! What a cold build of the same crates.io crate versions, with the same features, toolchain,
//! target and settings, computes is the same everywhere ([`BundleSpec`], named by its
//! [`BundleId`]). The builders build it once and publish every value it made under a signed
//! [`Manifest`]: the bundle's ID and the record of each value. A machine asks for the
//! manifest, checks its one signature, and fetches every value it lists that its store lacks
//! ([`Download`]), all at once over the client's connections. A value a verified manifest lists
//! is checked against the listed record and its content hash, with no signature of its own to
//! check.
//!
//! Only crates.io packages with a checksum are in a bundle ([`Locked::parse`]): a workspace
//! member, a path, git or other registry's package is built on the machine, as is anything of a
//! crates.io crate that reads one of them.

use blaze_cas::{Fetched, Key, Store, Ticket};
use blaze_core::{Effect, Tag};
use blaze_format::cursor::Reader;
use blaze_ids::ContentId;

use crate::{
    ByHash,
    client::{Client, GotBundle},
    public::Public,
    sign::{PublisherKey, RECORD_LEN, Record, Trust, hex},
    tls::Channels,
    wire::{Fields as _, truncated},
};

/// The most values one manifest lists.
pub const ENTRIES_MOST: usize = 1 << 20;

const SPEC_DOMAIN: &str = "blaze v2 global bundle 1";
const MANIFEST_DOMAIN: &str = "blaze v2 global manifest 1";
const MANIFEST_KEY_DOMAIN: &str = "blaze v2 global manifest key 1";
const SIGNED_AS: &[u8] = b"bundle";

const CRATES_IO: [&str; 2] = ["registry+https://github.com/rust-lang/crates.io-index", "sparse+https://index.crates.io/"];

/// A crates.io package of a lock file, as a build enables it.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Locked {
    pub name: String,
    pub version: String,
    /// The SHA-256 of its `.crate`, as the lock file records it.
    pub checksum: [u8; 32],
    /// The features the build enables, sorted.
    pub features: Vec<String>,
}

impl Locked {
    /// The crates.io packages of the lock file `lock` that the build builds: `built(name,
    /// version)` is the features the build enables of the package, or `None` when it does not
    /// build it (a dependency of another target). A crates.io package with no checksum is an
    /// error, since nothing then says which bytes it is.
    pub fn parse(lock: &str, built: impl Fn(&str, &str) -> Option<Vec<String>>) -> Result<Vec<Locked>, String> {
        // alloc: an error's message, made only when it fails.
        let table = lock.parse::<toml::Table>().map_err(|error| format!("Cargo.lock is not TOML: {error}"))?;
        // alloc: setup: once a bundle or its lock file, not a value.
        let Some(packages) = table.get("package") else { return Ok(Vec::new()) };
        let packages = packages.as_array().ok_or("Cargo.lock's `package` is not an array of tables")?;
        // alloc: setup: once a bundle or its lock file, not a value.
        let mut locked = Vec::new();
        for package in packages {
            let package = package.as_table().ok_or("a `package` of Cargo.lock that is not a table")?;
            let field = |name: &str| package.get(name).and_then(|it| it.as_str());
            let (Some(name), Some(version)) = (field("name"), field("version")) else {
                // alloc: an error's message, made only when it fails.
                return Err(format!("a package of Cargo.lock with no name or version: {package}"));
            };
            if !field("source").is_some_and(|source| CRATES_IO.contains(&source)) {
                continue;
            }
            let Some(features) = built(name, version) else { continue };
            let checksum = field("checksum").ok_or_else(|| {
                // alloc: an error's message, made only when it fails.
                format!("{name} {version} of crates.io has no checksum in Cargo.lock: nothing says which bytes it is")
            })?;
            let checksum = blaze_format::hex::decode::<32>(checksum)
                // alloc: an error's message, made only when it fails.
                .ok_or_else(|| format!("{name} {version}'s checksum {checksum:?} is not 64 hex digits"))?;
            let mut features = features;
            features.sort_unstable();
            features.dedup();
            // alloc: setup: once a bundle or its lock file, not a value.
            locked.push(Locked { name: name.to_owned(), version: version.to_owned(), checksum, features });
        }
        locked.sort_unstable();
        Ok(locked)
    }
}

/// What a cold build of crates.io dependencies is a function of.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BundleSpec {
    pub packages: Vec<Locked>,
    /// The pinned toolchain's content ID.
    pub toolchain: ContentId,
    pub target: String,
    /// The content ID of the build's settings (profile, backend, panic strategy), as the
    /// builders and the machine both make it.
    pub settings: ContentId,
}

/// A bundle's name: the hash of its [`BundleSpec`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct BundleId(pub ContentId);

impl BundleId {
    pub fn of(spec: &BundleSpec) -> BundleId {
        // alloc: setup: once a bundle or its lock file, not a value.
        let mut packages: Vec<&Locked> = spec.packages.iter().collect();
        packages.sort_unstable();
        packages.dedup();
        // alloc: setup: once a bundle or its lock file, not a value.
        let mut bytes = Vec::new();
        let text = |text: &str, bytes: &mut Vec<u8>| {
            bytes.extend_from_slice(&(text.len() as u64).to_le_bytes());
            bytes.extend_from_slice(text.as_bytes());
        };
        bytes.extend_from_slice(&spec.toolchain.0);
        bytes.extend_from_slice(&spec.settings.0);
        text(&spec.target, &mut bytes);
        bytes.extend_from_slice(&(packages.len() as u64).to_le_bytes());
        for package in packages {
            text(&package.name, &mut bytes);
            text(&package.version, &mut bytes);
            bytes.extend_from_slice(&package.checksum);
            // alloc: setup: once a bundle or its lock file, not a value.
            let mut features: Vec<&String> = package.features.iter().collect();
            features.sort_unstable();
            features.dedup();
            bytes.extend_from_slice(&(features.len() as u64).to_le_bytes());
            for feature in features {
                text(feature, &mut bytes);
            }
        }
        BundleId(ContentId::of(SPEC_DOMAIN, &bytes))
    }

    /// The key a cache keeps the bundle's manifest under.
    pub fn manifest_key(&self) -> Key {
        Key(ContentId::of(MANIFEST_KEY_DOMAIN, &self.0.0))
    }
}

/// The values of a bundle, by their records, signed once as a whole.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Manifest {
    pub id: BundleId,
    /// Sorted by key, each key once.
    pub entries: Vec<Record>,
    pub publisher: PublisherKey,
    pub signature: [u8; 64],
}

impl Manifest {
    /// The manifest of `entries` as bundle `id`, signed by `publisher`. Each entry's key must
    /// have been shown public ([`Public`]).
    pub fn sign(id: BundleId, entries: impl IntoIterator<Item = (Public, Record)>, publisher: &crate::Publisher) -> Manifest {
        let mut entries: Vec<Record> = entries
            .into_iter()
            .map(|(public, record)| {
                assert_eq!((public.key(), public.kind()), (record.key, record.kind), "a record of another key than the one shown public");
                record
            })
            // alloc: setup: once a bundle or its lock file, not a value.
            .collect();
        entries.sort_unstable_by_key(|it| it.key);
        entries.dedup();
        if let Some(pair) = entries.windows(2).find(|pair| pair[0].key == pair[1].key) {
            panic!("two records of {:x} in one bundle: {:?} and {:?}", pair[0].key.0, pair[0], pair[1]);
        }
        assert!(entries.len() <= ENTRIES_MOST, "a bundle of {} values, more than {ENTRIES_MOST}", entries.len());
        let signature = publisher.sign_digest(SIGNED_AS, &digest(&id, &entries));
        Manifest { id, entries, publisher: publisher.key(), signature }
    }

    /// The bytes of every value it lists.
    pub fn bytes(&self) -> u64 {
        self.entries.iter().map(|it| it.len).sum()
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.reserve(32 + 32 + 64 + 4 + RECORD_LEN * self.entries.len());
        out.extend_from_slice(&self.id.0.0);
        out.extend_from_slice(&self.publisher.0);
        out.extend_from_slice(&self.signature);
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for entry in &self.entries {
            out.extend_from_slice(&entry.encode());
        }
    }

    /// Reads one, every count checked against what the bytes hold before anything is
    /// allocated. Its signature is not checked: [`Manifest::verify`] does that.
    pub fn decode(reader: &mut Reader<'_>) -> Result<Manifest, String> {
        let id = BundleId(ContentId(*reader.array().map_err(truncated)?));
        let publisher = PublisherKey(*reader.array().map_err(truncated)?);
        let signature = *reader.array().map_err(truncated)?;
        let count = reader.count(RECORD_LEN, ENTRIES_MOST, "records")?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            entries.push(Record::decode(reader.array().map_err(truncated)?)?);
        }
        Ok(Manifest { id, entries, publisher, signature })
    }

    /// Whether a publisher `trust` pins signed it, and its entries are sorted and each key's
    /// once.
    pub fn verify(&self, trust: &Trust) -> Result<(), String> {
        trust.verify_digest(SIGNED_AS, &digest(&self.id, &self.entries), &self.publisher, &self.signature)?;
        match self.entries.windows(2).find(|pair| pair[0].key >= pair[1].key) {
            // alloc: an error's message, made only when it fails.
            Some(pair) => Err(format!("its records are not sorted by key, each once, at {:x}", pair[1].key.0)),
            None => Ok(()),
        }
    }
}

fn digest(id: &BundleId, entries: &[Record]) -> ContentId {
    let mut bytes = Vec::with_capacity(32 + 8 + RECORD_LEN * entries.len());
    bytes.extend_from_slice(&id.0.0);
    bytes.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    for entry in entries {
        bytes.extend_from_slice(&entry.encode());
    }
    ContentId::of(MANIFEST_DOMAIN, &bytes)
}

/// How a [`Download`] ended.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Downloaded {
    /// The cache has no bundle of the ID: the build is cold.
    NoBundle,
    /// Every value it lists is in the store: `fetched` of the `listed` were fetched, the rest
    /// were there already or fetched by another. Durable once each ticket is.
    Kept { listed: usize, fetched: usize, bytes: u64, tickets: Vec<Ticket> },
}

/// A bundle's download into a store: its manifest asked for, then every value it lists that
/// the store lacks, together.
pub struct Download {
    id: BundleId,
    asked: Tag,
    state: State,
}

enum State {
    Asking,
    /// `waiting`: each key fetched and not yet answered, with its length.
    Fetching { listed: usize, waiting: ByHash<Key, u64>, fetched: usize, bytes: u64, tickets: Vec<Ticket>, failed: Vec<String> },
    Done(Result<Downloaded, String>),
}

/// The most failures an error names; the rest are counted.
const NAMED_MOST: usize = 8;

impl Download {
    pub fn start<C: Channels>(id: BundleId, client: &mut Client<C>, effects: &mut Vec<Effect>) -> Download {
        let asked = client.bundle(id, effects);
        Download { id, asked, state: State::Asking }
    }

    pub fn id(&self) -> BundleId {
        self.id
    }

    /// Takes the client's manifests ([`Client::bundles`]), returning those not its own.
    pub fn bundles<C: Channels>(
        &mut self,
        got: Vec<GotBundle>,
        store: &mut Store,
        client: &mut Client<C>,
        effects: &mut Vec<Effect>,
    ) -> Vec<GotBundle> {
        // alloc: setup: once a bundle or its lock file, not a value.
        let mut others = Vec::new();
        for got in got {
            if got.tag != self.asked || !matches!(self.state, State::Asking) {
                others.push(got);
                continue;
            }
            self.state = match got.result {
                // alloc: an error's message, made only when it fails.
                Err(why) => State::Done(Err(format!("the manifest of bundle {}: {why}", hex(&self.id.0.0)))),
                Ok(None) => State::Done(Ok(Downloaded::NoBundle)),
                Ok(Some(manifest)) => {
                    let publics: Vec<Public> =
                        // alloc: setup: once a bundle or its lock file, not a value.
                        manifest.entries.iter().map(|entry| Public::listed(entry.key, entry.kind)).collect();
                    // alloc: setup: once a bundle or its lock file, not a value.
                    let lens: ByHash<Key, u64> = manifest.entries.iter().map(|it| (it.key, it.len)).collect();
                    let waiting = client
                        .fetch_many(store, &publics, effects)
                        .into_iter()
                        .map(|(key, _)| (key, lens[&key]))
                        // alloc: setup: once a bundle or its lock file, not a value.
                        .collect();
                    State::Fetching {
                        listed: manifest.entries.len(),
                        waiting,
                        fetched: 0,
                        bytes: 0,
                        // alloc: setup: once a bundle or its lock file, not a value.
                        tickets: Vec::new(),
                        // alloc: an error's message, made only when it fails.
                        failed: Vec::new(),
                    }
                }
            };
            self.settle();
        }
        others
    }

    /// Takes how the store's fetches went ([`Store::fetched`]), returning those not its own.
    pub fn fetched(&mut self, fetched: Vec<(Key, Fetched)>) -> Vec<(Key, Fetched)> {
        let State::Fetching { waiting, fetched: count, bytes, tickets, failed, .. } = &mut self.state else {
            return fetched;
        };
        // alloc: setup: once a bundle or its lock file, not a value.
        let mut others = Vec::new();
        for (key, how) in fetched {
            let Some(len) = waiting.remove(&key) else {
                others.push((key, how));
                continue;
            };
            match how {
                Fetched::Kept(ticket) => {
                    *count += 1;
                    *bytes += len;
                    tickets.push(ticket);
                }
                // alloc: an error's message, made only when it fails.
                Fetched::Missing => failed.push(format!("{:x}: the cache lists it and has no value of it", key.0)),
                // alloc: an error's message, made only when it fails.
                Fetched::Refused(why) => failed.push(format!("{:x}: {why}", key.0)),
            }
        }
        self.settle();
        others
    }

    /// How it ended, once it has.
    pub fn done(&self) -> Option<&Result<Downloaded, String>> {
        match &self.state {
            State::Done(result) => Some(result),
            _ => None,
        }
    }

    fn settle(&mut self) {
        let State::Fetching { listed, waiting, fetched, bytes, tickets, failed } = &mut self.state else { return };
        if !waiting.is_empty() {
            return;
        }
        let result = if failed.is_empty() {
            Ok(Downloaded::Kept { listed: *listed, fetched: *fetched, bytes: *bytes, tickets: std::mem::take(tickets) })
        } else {
            // alloc: an error's message, made only when it fails.
            let named = failed.iter().take(NAMED_MOST).cloned().collect::<Vec<_>>().join("; ");
            let more = failed.len().saturating_sub(NAMED_MOST);
            // alloc: an error's message, made only when it fails.
            let more = if more == 0 { String::new() } else { format!("; and {more} more") };
            // alloc: an error's message, made only when it fails.
            Err(format!(
                "bundle {}: {} of its {listed} values were not kept: {named}{more}",
                hex(&self.id.0.0),
                failed.len()
            ))
        };
        self.state = State::Done(result);
    }
}
