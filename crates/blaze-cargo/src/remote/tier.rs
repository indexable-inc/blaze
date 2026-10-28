//! The remote as a read-through tier of the store, with `BLAZE_REMOTE_LAZY`: a key the store
//! lacks is looked up in the project's key index, and the pack that holds it is fetched,
//! checked and kept before the store answers. The thread that asked waits for it, however
//! long that takes; when the remote cannot answer, the build fails naming what failed, and
//! nothing is compiled in place of what the remote would have brought. A key that no pack
//! the manifest lists holds is a miss, and compiles.
//!
//! What to fetch is decided by `blaze_store::remote_tier`, which is pure; this reads the
//! objects it asks for and fetches the packs it names. The root of the index and the
//! manifest are read once, when the daemon starts and when the manifest has changed since
//! (`keeper.rs`), never per key. Runs and shards are named by the hash of their bytes, so
//! they are kept in the store's directory (`remote-index/<project>/`), checked against their
//! names when read back, and never asked of the remote again; those the root no longer names
//! are removed when it is read. Lookups
//! from many threads share what each fetches: an object is asked for once however many
//! lookups need it, and the objects one lookup needs are asked for together.

use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};

use blaze_store::{
    FORMAT as STORE_FORMAT, limit_for,
    remote_index::{self as format, Hash, Root},
    remote_tier::{self, Lookup, Tier, Want},
    write_file_whole,
};
use futures_util::{StreamExt, TryStreamExt, stream};
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::{
    runtime::Runtime,
    sync::{OnceCell, OwnedRwLockWriteGuard, RwLock},
};

use super::{
    Digest, Fetched, Listed, Manifest, Remote, claim, fetch_pack, index, local_packs, note_down,
    pack_file, transfer,
};

/// The directory of the store that keeps the runs and shards read.
const KEPT: &str = "remote-index";

/// How many times a pack another process was fetching is waited for before this one gives
/// up on it: each time, that process let it go without it.
const ELSEWHERE: usize = 3;

pub(super) enum State {
    /// The root and the manifest are being read; lookups wait for them.
    Loading,
    Ready {
        tier: Tier,
        listed: FxHashMap<Digest, Listed>,
        /// The most bytes the store may hold, which a pack it is to keep cannot be over.
        limit: u64,
    },
    /// They could not be read: every lookup fails with this.
    Failed(String),
}

/// See the module.
pub struct Tiered {
    store: PathBuf,
    remote: Arc<Remote>,
    runtime: Arc<Runtime>,
    project: String,
    state: Arc<RwLock<State>>,
    /// What is being fetched, by its hash: a run, a shard, a key list or a pack.
    fetching: Mutex<FxHashMap<Hash, Arc<OnceCell<Result<(), String>>>>>,
    /// Whether a lookup failed since one was last asked for: the store asks for none until
    /// the build that failed takes the failure, so the next one is of a build after it.
    failed: AtomicBool,
}

impl Tiered {
    /// The tier of `project` on `remote` for the store in `store`, and the hold on it that
    /// [`Self::load`] takes: lookups wait until it is loaded.
    pub(super) fn new(
        store: &Path,
        remote: Arc<Remote>,
        runtime: Arc<Runtime>,
        project: &str,
    ) -> (Arc<Tiered>, OwnedRwLockWriteGuard<State>) {
        let state = Arc::new(RwLock::new(State::Loading));
        let held = state.clone().try_write_owned().expect("nothing else has it yet");
        let tiered = Tiered {
            store: store.to_owned(),
            remote,
            runtime,
            project: project.to_owned(),
            state,
            fetching: Mutex::default(),
            failed: AtomicBool::new(false),
        };
        (Arc::new(tiered), held)
    }

    /// Reads the root of the index, every run it names and the manifest, keeping what was
    /// read before that they still name, and says what they are.
    pub(super) async fn load(
        &self,
        mut held: OwnedRwLockWriteGuard<State>,
    ) -> anyhow::Result<String> {
        let before = std::mem::replace(&mut *held, State::Loading);
        match self.look(before).await {
            Ok((state, said)) => {
                *held = state;
                Ok(said)
            }
            Err(e) => {
                *held = State::Failed(format!("{e:#}"));
                Err(e)
            }
        }
    }

    /// [`Self::load`] once more: the manifest has changed, or the last read failed.
    pub async fn reload(&self) -> anyhow::Result<String> {
        let held = self.state.clone().write_owned().await;
        self.load(held).await
    }

    async fn look(&self, before: State) -> anyhow::Result<(State, String)> {
        let project = &self.project;
        // The root before the manifest: every pack a run covers was listed before the run
        // was written, so the manifest read after lists it, unless a collection took it out.
        let lines = self.remote.read_list(&index::root_at(project)).await?;
        let root = Root::parse(&lines.unwrap_or_default());
        let manifest = Manifest::fetch(&self.remote, project).await?.unwrap_or_default();
        let here = local_packs(&claim(&self.store, false).await?)?;
        let limit = limit_for(&self.store).map_err(anyhow::Error::msg)?.unwrap_or(u64::MAX);
        let listed: FxHashMap<Digest, Listed> =
            manifest.0.into_iter().filter(|(_, it)| it.format == STORE_FORMAT).collect();
        let object_max = self.remote.object_max;
        let packs = listed.iter().map(|(pack, it)| remote_tier::Listed {
            pack: *pack.bytes(),
            keys: *it.keys.bytes(),
            size: it.size.min(object_max),
        });
        let here = here.iter().map(|it| *it.bytes());
        let mut tier = match before {
            State::Ready { mut tier, .. } => {
                tier.renew(&root, packs, here);
                tier
            }
            _ => Tier::new(&root, packs, here, object_max),
        };
        // Every lookup reads every run first: read them now, so that a run that cannot be
        // had fails here, and so that what is kept of the old ones can be cleared.
        let runs: Vec<Want> = match tier.lookup(&[0; 32]) {
            Lookup::Fetch(wants) => {
                wants.into_iter().filter(|it| matches!(it, Want::Run { .. })).collect()
            }
            Lookup::Holders(_) => Vec::new(),
        };
        if !runs.is_empty() {
            let read = stream::iter(runs.into_iter().map(|want| async move {
                let bytes = self.bytes_of(&want, |bytes| remote_tier::check(&want, bytes)).await;
                Ok::<_, String>((want, bytes?))
            }));
            let read: Result<Vec<_>, String> =
                read.buffer_unordered(self.remote.jobs()).try_collect().await;
            for (want, bytes) in read.map_err(|why| self.failure(why))? {
                tier.take(&want, &bytes).map_err(|why| self.failure(why.0))?;
            }
        }
        self.clear_kept(tier.named()).await;
        let (runs, uncovered) = tier.counts();
        let mut said =
            format!("the index of `{project}`: {runs} runs over {} packs listed", listed.len());
        if let Some(uncovered) = uncovered.filter(|it| *it != 0) {
            said += &format!(", {uncovered} of them not indexed, found by their key lists");
        }
        Ok((State::Ready { tier, listed, limit }, said))
    }

    /// An error of a lookup, naming the remote and the project.
    fn failure(&self, why: String) -> anyhow::Error {
        anyhow::anyhow!("{}: `{}`: {why}", self.remote.place.name(), self.project)
    }

    /// The store's copy of a run or shard, if it has one.
    fn kept(&self, hash: &Hash) -> PathBuf {
        self.kept_dir().join(format::hex(hash))
    }

    /// Where the store keeps what it read of this project's index: a directory of each
    /// project, as daemons of several share a store and each clears what its root does not
    /// name.
    fn kept_dir(&self) -> PathBuf {
        self.store.join(KEPT).join(&self.project)
    }

    /// The bytes of `want` that `check` accepts: the store's copy when it has such a one,
    /// otherwise the remote's, which are then kept.
    async fn bytes_of(
        &self,
        want: &Want,
        check: impl Fn(&[u8]) -> Result<(), remote_tier::Unusable>,
    ) -> Result<Vec<u8>, String> {
        let kept = self.kept(want.hash());
        if want.is_index()
            && let Ok(bytes) = tokio::fs::read(&kept).await
        {
            if check(&bytes).is_ok() {
                return Ok(bytes);
            }
            _ = tokio::fs::remove_file(&kept).await;
        }
        let name = want.name();
        let most = want.most().min(self.remote.object_max);
        let read = self.remote.read_small(&name, most).await.map_err(|e| format!("{e:#}"))?;
        let Some((bytes, _)) = read else {
            return Err(match want {
                Want::Keys { pack, .. } => {
                    format!("{name}, the key list of {}.pack, is not there", Digest::from(*pack))
                }
                _ => format!(
                    "{name} of the index is not there: the next push takes out the run that \
                     names it"
                ),
            });
        };
        if let Err(why) = check(&bytes) {
            self.remote.traffic.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(why.0);
        }
        if want.is_index() {
            let (kept, bytes) = (kept, bytes.clone());
            let wrote = tokio::task::spawn_blocking(move || {
                std::fs::create_dir_all(kept.parent().expect("in a directory"))?;
                write_file_whole(&kept, &bytes)
            });
            match wrote.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("remote: {name} is not kept: {e}"),
                Err(e) => eprintln!("remote: {name} is not kept: {e}"),
            }
        }
        Ok(bytes)
    }

    /// Removes the runs and shards the store keeps that are not in `named`.
    async fn clear_kept(&self, named: FxHashSet<Hash>) {
        let dir = self.kept_dir();
        let cleared = tokio::task::spawn_blocking(move || {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            };
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name();
                let wanted = (name.to_str().and_then(Digest::parse))
                    .is_some_and(|it| named.contains(it.bytes()));
                if !wanted {
                    std::fs::remove_file(entry.path())?;
                }
            }
            Ok(())
        });
        match cleared.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("remote: what the store keeps of the index is not cleared: {e}")
            }
            Err(e) => eprintln!("remote: what the store keeps of the index is not cleared: {e}"),
        }
    }

    /// Does `work` once for `hash`, however many lookups ask for it at once. A failure is
    /// not remembered: the next lookup after it does the work again.
    async fn once(
        &self,
        hash: Hash,
        work: impl Future<Output = Result<(), String>>,
    ) -> Result<(), String> {
        let lock = || self.fetching.lock().unwrap_or_else(PoisonError::into_inner);
        let cell = lock().entry(hash).or_default().clone();
        let done = cell.get_or_init(|| work).await.clone();
        let mut fetching = lock();
        if fetching.get(&hash).is_some_and(|it| Arc::ptr_eq(it, &cell)) {
            fetching.remove(&hash);
        }
        done
    }

    /// Reads a run, a shard or a key list into the tier.
    async fn read(&self, want: Want) -> Result<(), String> {
        self.once(*want.hash(), async {
            let bytes = self.bytes_of(&want, |bytes| remote_tier::check(&want, bytes)).await?;
            match &mut *self.state.write().await {
                State::Ready { tier, .. } => tier.take(&want, &bytes).map_err(|why| why.0),
                _ => Ok(()),
            }
        })
        .await
    }

    /// Brings the listed pack `pack` into the store's directory, unless it is there.
    async fn bring(&self, pack: Hash) -> Result<(), String> {
        let digest = Digest::from(pack);
        let (listed, limit) = match &*self.state.read().await {
            State::Ready { listed, limit, .. } => (listed.get(&digest).copied(), *limit),
            _ => (None, 0),
        };
        let Some(listed) = listed else {
            return Err(format!("{digest}.pack is not listed by the manifest read"));
        };
        if listed.size > limit {
            return Err(format!(
                "{digest}.pack is listed as {} bytes, over BLAZE_CACHE_MAX ({limit}): the store \
                 cannot keep it to read it",
                listed.size
            ));
        }
        let to = self.store.join(pack_file(&digest));
        self.once(pack, async {
            for _ in 0..ELSEWHERE {
                if tokio::fs::try_exists(&to).await.unwrap_or(false) {
                    return Ok(());
                }
                match fetch_pack(&self.remote, &self.store, &digest, &listed).await {
                    Ok(Fetched::Whole(bytes)) => {
                        eprintln!("remote: {digest}.pack fetched for a build, {bytes} bytes");
                        return Ok(());
                    }
                    Ok(Fetched::Elsewhere) => {
                        transfer::fetched_elsewhere(&to, listed.packed.is_some())
                            .await
                            .map_err(|e| format!("waiting for {digest}.pack: {e}"))?;
                    }
                    Err(e) => return Err(format!("{e:#}")),
                }
            }
            match tokio::fs::try_exists(&to).await.unwrap_or(false) {
                true => Ok(()),
                false => Err(format!(
                    "{digest}.pack: another process took it up and let it go {ELSEWHERE} times"
                )),
            }
        })
        .await
    }

    /// The next pack that may hold `key` and is not in `tried`, brought into the store's
    /// directory: see the module.
    async fn lookup(&self, key: [u8; 32], tried: Vec<Hash>) -> Result<Option<Hash>, String> {
        if self.failed.swap(false, Ordering::Relaxed) {
            // A build after the one that failed: the remote may be back.
            self.remote.traffic.try_again();
            let failed = matches!(&*self.state.read().await, State::Failed(_));
            if failed && let Err(e) = self.reload().await {
                return Err(format!("{e:#}"));
            }
        }
        loop {
            let step = match &*self.state.read().await {
                State::Ready { tier, .. } => tier.lookup(&key),
                State::Failed(why) => return Err(why.clone()),
                State::Loading => return Err("the index was never read".to_owned()),
            };
            match step {
                Lookup::Fetch(wants) => {
                    let reads = stream::iter(wants.into_iter().map(|want| self.read(want)));
                    let read = reads.buffer_unordered(self.remote.jobs()).try_collect::<Vec<()>>();
                    read.await.map_err(|why| format!("{:#}", self.failure(why)))?;
                }
                Lookup::Holders(holders) => {
                    let Some(pack) = holders.into_iter().find(|it| !tried.contains(it)) else {
                        return Ok(None);
                    };
                    self.bring(pack).await.map_err(|why| format!("{:#}", self.failure(why)))?;
                    return Ok(Some(pack));
                }
            }
        }
    }

    /// The store's question, from a thread that compiles: [`Self::lookup`], waited for.
    pub fn fetch(self: &Arc<Self>, key: &[u8; 32], tried: &[Hash]) -> Result<Option<Hash>, String> {
        let (send, came) = std::sync::mpsc::sync_channel(1);
        let (tiered, key, tried) = (self.clone(), *key, tried.to_vec());
        self.runtime.spawn(async move { _ = send.send(tiered.lookup(key, tried).await) });
        let found = came.recv().unwrap_or_else(|_| Err("the lookup was dropped".to_owned()));
        if let Err(why) = &found {
            self.failed.store(true, Ordering::Relaxed);
            note_down(&self.store, &self.remote.place.name(), Some(&anyhow::anyhow!("{why}")));
        }
        found
    }
}

/// Reads `key` as a lookup through `tiered` would, for the tests.
#[cfg(test)]
pub async fn find(tiered: &Tiered, key: &[u8; 32]) -> anyhow::Result<Vec<Digest>> {
    let mut tried = Vec::new();
    while let Some(pack) = tiered.lookup(*key, tried.clone()).await.map_err(anyhow::Error::msg)? {
        tried.push(pack);
        let name = tiered.store.join(pack_file(&Digest::from(pack)));
        let bytes = std::fs::read(&name).map_err(|e| anyhow::anyhow!("{}: {e}", name.display()))?;
        let keys = blaze_store::pack_keys(&pack_file(&Digest::from(pack)), &bytes)
            .map_err(anyhow::Error::msg)?;
        if keys.contains(key) {
            break;
        }
    }
    Ok(tried.into_iter().map(Digest::from).collect())
}
