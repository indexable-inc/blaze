//! Keeping a store in step with a place other machines reach: `blaze-engine cache push REMOTE` and
//! `blaze-engine cache pull REMOTE`, and the same done by the daemon when `BLAZE_REMOTE` names one.
//! The commands on the local store alone, `blaze-engine cache stats|gc|verify`, are here with them.
//!
//! A remote is a directory, `s3://bucket/prefix`, an `https://` address that is read from
//! only (`https.rs`), or a tenant of blaze's cache gateway, `blaze+https://` (`gateway.rs`).
//! It holds sealed packs under the hash of
//! their bytes, the keys of each pack under the hash of those, and one manifest a project:
//! the packs its builds were served from. A push sends the packs the manifest lacks and
//! adds them to it; a pull fetches the ones it lists that are not here. With
//! `BLAZE_REMOTE_LAZY` the daemon fetches the keys only, and a pack when a build first asks
//! for a key it holds. A daemon that stays goes on in `keeper.rs`: it pulls again when a
//! build lacked something and the manifest has changed, and pushes on a schedule and when
//! it is told to end.
//!
//! What is fetched becomes a pack of the store like any other: it is written the way the
//! store writes its own, counts toward `BLAZE_CACHE_MAX`, and can be evicted and fetched
//! again. A pull fetches no more than fits under that limit.
//!
//! Nothing fetched is used before its bytes hash to the name it was listed under, so a
//! remote can withhold code and cannot change it. What it could change is the list, which
//! is what `signed.rs` is for. `BLAZE_REMOTE_READONLY` keeps a machine from writing at all.
//!
//! Nor does a remote say how much is taken from it. A list is read to [`LIST_MAX`] bytes,
//! a pack to the size its list gives it, anything else to `BLAZE_REMOTE_MAX`, and what is
//! longer is refused with nothing of it kept.

pub mod collect;
pub mod gateway;
pub mod https;
pub mod keeper;
pub mod s3;
pub mod signed;
pub mod transfer;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, bail};
use blaze_store::{
    FORMAT as STORE_FORMAT, StoreDir, limit_for, name_file_whole, pack_format, pack_keys,
    parse_size, write_file_whole,
};
use futures_util::{StreamExt, stream};
use rustc_hash::FxHashMap;
use tokio::{io::AsyncSeekExt, runtime::Runtime};
use url::Url;

use gateway::Gateway;
pub use https::{Https, next as https_next};
use s3::{S3, Written};
use signed::{NotRewritten, Trust, Turns, Unremembered};
use transfer::{Body, Object, TooLong, Version};
pub use transfer::{Digest, Fetched, Room, Traffic};

/// How often a list is read again and written again after another machine wrote first.
const LIST_WRITES: usize = 8;
/// An object that is there and that no list names yet is taken as sent for this long after
/// it was written. Past that it is sent again, so that a collection of the remote, which
/// goes by when an object was written, does not take it from under a push.
const REUSE_FOR: Duration = Duration::from_secs(60 * 60);

/// The most that is read of a list. A line of one is under 200 bytes, so this is a list
/// of some hundred thousand packs or units.
pub const LIST_MAX: u64 = 16 << 20;

/// The most that is taken of one object that is neither a list nor a pack, unless
/// `BLAZE_REMOTE_MAX` says: the archive of a unit, and the keys of the packs a daemon
/// fetches from when asked.
const OBJECT_MAX: u64 = 512 << 20;

pub struct Remote {
    place: Place,
    traffic: Arc<Traffic>,
    /// How long a request may go without an answer, or a body without a byte.
    patience: Duration,
    read_only: bool,
    trust: Trust,
    turns: Turns,
    /// `BLAZE_REMOTE_MAX`, or [`OBJECT_MAX`].
    object_max: u64,
}

enum Place {
    /// A directory, such as a mount shared by the machines.
    Dir(PathBuf),
    S3(Box<S3>),
    /// Files an HTTPS server hands out, such as a release's assets: read-only.
    Https(Box<Https>),
    /// blaze's cache gateway, one tenant of it (`gateway.rs`).
    Gateway(Box<Gateway>),
}

impl Remote {
    /// The remote `spec` names, as the environment has this machine use it:
    /// `BLAZE_REMOTE_JOBS` requests at a time (8), `BLAZE_REMOTE_TIMEOUT` seconds without
    /// an answer (30), `BLAZE_REMOTE_READONLY` to never write, `BLAZE_REMOTE_MAX` bytes of
    /// one object, and the keys of `signed.rs`. The turns of its lists are remembered in
    /// `kept_in`, the directory that what is fetched goes to.
    pub async fn open(spec: &str, kept_in: &Path) -> anyhow::Result<Remote> {
        let number = |name: &str, default: u64| match std::env::var(name) {
            Ok(value) => value.parse().with_context(|| format!("{name} is not a number")),
            Err(_) => Ok(default),
        };
        let traffic = Arc::new(Traffic::new(number("BLAZE_REMOTE_JOBS", 8)? as usize));
        let patience = Duration::from_secs(number("BLAZE_REMOTE_TIMEOUT", 30)?.max(1));
        let place = if spec.starts_with("blaze+") {
            Place::Gateway(Box::new(Gateway::open(spec, patience, traffic.clone())?))
        } else if spec.starts_with("s3://") {
            let spec = Url::parse(spec).context("the address of the remote is not a URL")?;
            Place::S3(Box::new(S3::open(&spec, patience, traffic.clone()).await?))
        } else if spec.starts_with("https://") || spec.starts_with("http://") {
            let spec = Url::parse(spec).context("the address of the remote is not a URL")?;
            Place::Https(Box::new(Https::open(&spec, patience, traffic.clone())?))
        } else {
            Place::Dir(PathBuf::from(spec))
        };
        let read_only = matches!(place, Place::Https(_))
            || std::env::var_os("BLAZE_REMOTE_READONLY").is_some_and(|it| it != "0");
        let object_max = match std::env::var("BLAZE_REMOTE_MAX") {
            Ok(text) => parse_size(&text)
                .with_context(|| format!("BLAZE_REMOTE_MAX=`{text}` is not a size"))?,
            Err(_) => OBJECT_MAX,
        };
        let turns = Turns::of(kept_in, &place.name());
        Ok(Remote {
            place,
            traffic,
            patience,
            read_only,
            trust: Trust::from_env()?,
            turns,
            object_max,
        })
    }

    /// The remote with its lists taken as they are, signed or not, and written unsigned:
    /// for the standard library a release ships (`shipped.rs`), which is as trusted as the
    /// release, or as the remote the user named for it.
    pub fn as_release(mut self) -> Remote {
        self.trust = Trust::of_release();
        self
    }

    /// The most that is taken of one object that says nothing of its size.
    pub fn object_max(&self) -> u64 {
        self.object_max
    }

    /// An error when this machine is not to write to the remote, saying why.
    pub fn may_write(&self) -> anyhow::Result<()> {
        if self.read_only {
            bail!("BLAZE_REMOTE_READONLY is set: nothing is sent");
        }
        if let Place::Gateway(gateway) = &self.place
            && let Some(why) = gateway.cannot_write()
        {
            bail!(why);
        }
        match self.trust.cannot_sign() {
            Some(why) => bail!("{why}: what this machine sent would not be trusted"),
            None => Ok(()),
        }
    }

    /// The object under `name` from `offset` on, or `None` when there is nothing under it.
    async fn read_from(&self, name: &str, offset: u64) -> anyhow::Result<Option<Body>> {
        match &self.place {
            Place::Dir(dir) => {
                self.traffic.operations.fetch_add(1, Ordering::Relaxed);
                match tokio::fs::File::open(dir.join(name)).await {
                    Ok(mut file) => {
                        file.seek(std::io::SeekFrom::Start(offset)).await?;
                        Ok(Some(Body::File(file)))
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e).with_context(|| format!("fetching {name}")),
                }
            }
            Place::S3(s3) => Ok(s3.read_from(name, offset).await?.map(Body::S3)),
            Place::Https(https) => Ok(https.read_from(name, offset).await?.map(Body::Http)),
            Place::Gateway(gateway) => gateway.read_from(name, offset).await,
        }
    }

    /// The whole of a small object and the version it was read at, asked for again for as
    /// long as a pack is. One of more than `most` bytes is an error, and no more than a
    /// read past that many of them is held.
    async fn read_small(
        &self,
        name: &str,
        most: u64,
    ) -> anyhow::Result<Option<(Vec<u8>, Version)>> {
        let _permit = self.traffic.permit().await?;
        let read = self.patiently(|| self.read_small_once(name, most)).await?;
        if read.is_some() {
            self.traffic.moved.fetch_add(1, Ordering::Relaxed);
        }
        Ok(read)
    }

    async fn read_small_once(
        &self,
        name: &str,
        most: u64,
    ) -> anyhow::Result<Option<(Vec<u8>, Version)>> {
        match &self.place {
            Place::Dir(dir) => {
                self.traffic.operations.fetch_add(1, Ordering::Relaxed);
                let file = match tokio::fs::File::open(dir.join(name)).await {
                    Ok(file) => file,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e).with_context(|| format!("fetching {name}")),
                };
                let bytes = read_at_most(Body::File(file), most, name).await?;
                let version = Version(Some(Digest::of(&bytes).to_string()));
                Ok(Some((bytes, version)))
            }
            Place::S3(s3) => s3.read_small(name, most).await,
            Place::Https(https) => https.read_small(name, most).await,
            Place::Gateway(gateway) => gateway.read_small(name, most).await,
        }
    }

    /// The object under `name`, if there is one.
    async fn found(&self, name: &str) -> anyhow::Result<Option<Object>> {
        let _permit = self.traffic.permit().await?;
        self.patiently(|| self.found_once(name)).await
    }

    async fn found_once(&self, name: &str) -> anyhow::Result<Option<Object>> {
        match &self.place {
            Place::Dir(dir) => {
                self.traffic.operations.fetch_add(1, Ordering::Relaxed);
                match tokio::fs::metadata(dir.join(name)).await {
                    Ok(found) => Ok(Some(Object {
                        name: name.to_owned(),
                        len: found.len(),
                        written: found.modified()?,
                    })),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e).with_context(|| format!("looking for {name}")),
                }
            }
            Place::S3(s3) => s3.found(name).await,
            Place::Https(https) => https.found(name).await,
            Place::Gateway(gateway) => gateway.found(name).await,
        }
    }

    /// Whether the object under `name` is there with `len` bytes and need not be sent. One
    /// that has been there for longer than [`REUSE_FOR`] is sent again, which tells a
    /// collection of the remote that it is wanted.
    async fn holds(&self, name: &str, len: u64) -> anyhow::Result<bool> {
        let fresh = |it: &Object| it.written.elapsed().is_ok_and(|age| age < REUSE_FOR);
        Ok(self.found(name).await?.is_some_and(|it| it.len == len && fresh(&it)))
    }

    /// The objects whose names start with `prefix`, which ends a directory.
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<Object>> {
        let _permit = self.traffic.permit().await?;
        self.patiently(|| self.list_once(prefix)).await
    }

    async fn list_once(&self, prefix: &str) -> anyhow::Result<Vec<Object>> {
        match &self.place {
            Place::Dir(dir) => {
                self.traffic.operations.fetch_add(1, Ordering::Relaxed);
                let (dir, prefix) = (dir.join(prefix), prefix.to_owned());
                tokio::task::spawn_blocking(move || list_dir(&dir, &prefix)).await?
            }
            Place::S3(s3) => s3.list(prefix).await,
            Place::Https(_) => bail!("a remote over HTTPS cannot be listed"),
            Place::Gateway(_) => Err(transfer::Refused(format!(
                "{} cannot be listed: its tenant's objects are collected by the gateway",
                self.place.name()
            ))
            .into()),
        }
    }

    /// Removes the object under `name`.
    async fn delete(&self, name: &str) -> anyhow::Result<()> {
        self.may_write()?;
        let _permit = self.traffic.permit().await?;
        self.patiently(|| self.delete_once(name)).await
    }

    async fn delete_once(&self, name: &str) -> anyhow::Result<()> {
        match &self.place {
            Place::Dir(dir) => {
                self.traffic.operations.fetch_add(1, Ordering::Relaxed);
                match tokio::fs::remove_file(dir.join(name)).await {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        Err(e).with_context(|| format!("removing {name}"))
                    }
                    _ => Ok(()),
                }
            }
            Place::S3(s3) => s3.delete(name).await,
            Place::Https(_) => bail!("a remote over HTTPS is read-only"),
            Place::Gateway(_) => bail!("the gateway takes nothing out: {name} is left"),
        }
    }

    /// Writes a small object, unless what is there is no longer what was read.
    async fn write_small(
        &self,
        name: &str,
        bytes: Vec<u8>,
        read_at: Option<&Version>,
    ) -> anyhow::Result<Written> {
        self.may_write()?;
        let _permit = self.traffic.permit().await?;
        let bytes = Arc::new(bytes);
        let written =
            self.patiently(|| self.write_small_once(name, bytes.clone(), read_at)).await?;
        if let Written::Done = written {
            self.traffic.moved.fetch_add(1, Ordering::Relaxed);
        }
        Ok(written)
    }

    /// A write whose answer was lost and that is made again finds its own bytes there and
    /// is told it lost, which has the caller read them and see there is nothing to add.
    async fn write_small_once(
        &self,
        name: &str,
        bytes: Arc<Vec<u8>>,
        read_at: Option<&Version>,
    ) -> anyhow::Result<Written> {
        match &self.place {
            Place::Dir(dir) => {
                self.traffic.operations.fetch_add(1, Ordering::Relaxed);
                let (to, read_at) = (dir.join(name), read_at.and_then(|it| it.0.clone()));
                let write = move || write_if_unchanged(&to, &bytes, read_at.as_deref());
                tokio::task::spawn_blocking(write).await?
            }
            Place::S3(s3) => s3.write_small(name, &bytes, read_at).await,
            Place::Https(_) => bail!("a remote over HTTPS is read-only"),
            Place::Gateway(gateway) => gateway.write_small(name, &bytes, read_at).await,
        }
    }

    /// Puts the file under `name`, all of it or nothing.
    async fn send_file(&self, name: &str, file: &Path, len: u64) -> anyhow::Result<()> {
        self.may_write()?;
        self.send_file_anywhere(name, file, len).await?;
        self.traffic.moved.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn send_file_anywhere(&self, name: &str, file: &Path, len: u64) -> anyhow::Result<()> {
        match &self.place {
            Place::Dir(dir) => {
                let _permit = self.traffic.permit().await?;
                self.traffic.operations.fetch_add(1, Ordering::Relaxed);
                let (from, to) = (file.to_owned(), dir.join(name));
                tokio::task::spawn_blocking(move || copy_whole(&from, &to)).await??;
                self.traffic.sent.fetch_add(len, Ordering::Relaxed);
                Ok(())
            }
            Place::S3(s3) => s3.send_file(name, file, len).await,
            Place::Https(_) => bail!("a remote over HTTPS is read-only"),
            Place::Gateway(gateway) => {
                let _permit = self.traffic.permit().await?;
                self.patiently(|| gateway.send_file(name, file, len)).await
            }
        }
    }

    /// The lines of the list `name`, for a machine that takes what it names. No list is
    /// no lines. A signed list older than one this machine took before is an error.
    pub async fn read_list(&self, name: &str) -> anyhow::Result<Option<String>> {
        let Some((bytes, _)) = self.read_small(name, LIST_MAX).await? else { return Ok(None) };
        let text = String::from_utf8(bytes).with_context(|| format!("`{name}` is not text"))?;
        let taken = match self.trust.open(name, &text) {
            Ok(opened) => self.take_turn(name, opened.turn).await.map(|()| opened.lines),
            Err(e) => Err(e),
        };
        let lines = taken.inspect_err(|_| {
            self.traffic.rejected.fetch_add(1, Ordering::Relaxed);
        });
        Ok(Some(lines?.to_owned()))
    }

    /// The lines of the list `name`, for a machine that is about to add to it: what it
    /// would write the list back with. A list it would not sign under is an error.
    pub async fn read_list_to_extend(&self, name: &str) -> anyhow::Result<Option<String>> {
        let Some((bytes, _)) = self.read_small(name, LIST_MAX).await? else { return Ok(None) };
        let text = String::from_utf8(bytes).with_context(|| format!("`{name}` is not text"))?;
        Ok(Some(self.trust.open_to_extend(name, &text)?.lines.to_owned()))
    }

    /// Remembers the turn a signed list was taken at, unless a later one was taken before.
    async fn take_turn(&self, name: &str, turn: Option<u64>) -> anyhow::Result<()> {
        let Some(turn) = turn else { return Ok(()) };
        let (turns, name) = (self.turns.clone(), name.to_owned());
        tokio::task::spawn_blocking(move || turns.take(&name, turn)).await?
    }

    /// The highest turn this machine took of the list `name`, or 0.
    async fn turn_taken(&self, name: &str) -> anyhow::Result<u64> {
        let (turns, name) = (self.turns.clone(), name.to_owned());
        let taken = tokio::task::spawn_blocking(move || turns.remembered(&name)).await??;
        Ok(taken.unwrap_or(0))
    }

    /// Forgets the turns this machine took of the lists of this remote.
    pub fn forget_turns(&self) -> std::io::Result<bool> {
        self.turns.forget()
    }

    /// Replaces the lines of the list `name` by what `extend` makes of them, again if
    /// another machine wrote in between: no write undoes another.
    pub async fn extend_list(
        &self,
        name: &str,
        extend: impl Fn(&str) -> String,
    ) -> anyhow::Result<()> {
        self.rewrite_list(name, extend, true).await
    }

    /// As [`Self::extend_list`], for lines being taken out: a list this machine would not
    /// add to is left as it is, and that is an error.
    async fn shrink_list(&self, name: &str, shrink: impl Fn(&str) -> String) -> anyhow::Result<()> {
        self.rewrite_list(name, shrink, false).await
    }

    async fn rewrite_list(
        &self,
        name: &str,
        rewrite: impl Fn(&str) -> String,
        anew: bool,
    ) -> anyhow::Result<()> {
        for _ in 0..LIST_WRITES {
            let read = self.read_small(name, LIST_MAX).await?;
            let text = match &read {
                Some((bytes, _)) => std::str::from_utf8(bytes).unwrap_or_default(),
                None => "",
            };
            // Written back without a signature, a signed list is one nobody takes.
            self.trust.cannot_rewrite(name, text)?;
            let opened = match self.trust.open_to_extend(name, text) {
                Ok(opened) => self.take_turn(name, opened.turn).await.map(|()| opened.lines),
                Err(e) => Err(e),
            };
            let lines = match opened {
                Ok(lines) => lines,
                Err(e) if e.is::<Unremembered>() => return Err(e),
                Err(_) if read.is_none() => "",
                // Signing it would be vouching for it.
                Err(e) if anew => {
                    eprintln!("remote: {e:#}: it is written anew");
                    ""
                }
                Err(e) => return Err(e),
            };
            // The list it replaces was at the turn remembered, or was not taken at all.
            let after = self.turn_taken(name).await?;
            let (next, turn) = self.trust.seal(name, &rewrite(lines), after);
            let read_at = read.as_ref().map(|it| &it.1);
            if let Written::Done = self.write_small(name, next.into_bytes(), read_at).await? {
                return self.take_turn(name, turn).await;
            }
        }
        bail!("`{name}` was written by others {LIST_WRITES} times in a row")
    }

    /// The bytes of a small object whose name is their hash, when they are at most `most`.
    /// Bytes that hash to anything else are counted as rejected.
    async fn read_named_by_hash(
        &self,
        name: &str,
        want: &Digest,
        most: u64,
    ) -> anyhow::Result<Vec<u8>> {
        let Some((bytes, _)) = self.read_small(name, most).await? else {
            return Err(transfer::Missing(name.to_owned()).into());
        };
        let got = Digest::of(&bytes);
        if got != *want {
            self.traffic.rejected.fetch_add(1, Ordering::Relaxed);
            bail!("rejected {name}: its {} bytes hash to {got}, not to {want}", bytes.len());
        }
        Ok(bytes)
    }

    /// Sends an object whose name is the hash of its bytes, unless it is there.
    pub async fn send_named_by_hash(&self, name: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        if !self.holds(name, bytes.len() as u64).await? {
            // Whoever else writes it writes the same bytes.
            self.write_small(name, bytes, None).await?;
        }
        Ok(())
    }

    /// How many requests are made of the remote at a time.
    pub fn jobs(&self) -> usize {
        self.traffic.jobs()
    }

    pub fn traffic(&self) -> &Traffic {
        &self.traffic
    }
}

impl Place {
    /// What tells this remote from another on this machine: where it is, and not how it
    /// is reached.
    fn name(&self) -> String {
        match self {
            Place::Dir(dir) => dir.display().to_string(),
            Place::S3(s3) => s3.place(),
            Place::Https(https) => https.place(),
            Place::Gateway(gateway) => gateway.place(),
        }
    }
}

/// All the bytes of `body` when they are at most `most`: an error when there are more,
/// with at most one read past that many of them held.
async fn read_at_most(mut body: Body, most: u64, name: &str) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await? {
        if (bytes.len() + chunk.len()) as u64 > most {
            return Err(TooLong { name: name.to_owned(), most }.into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// The files of a directory of a directory remote, named as objects are. No directory is
/// no files.
fn list_dir(dir: &Path, prefix: &str) -> anyhow::Result<Vec<Object>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("listing {prefix}")),
    };
    let mut objects = Vec::new();
    for entry in entries {
        let entry = entry?;
        let found = entry.metadata()?;
        let Some(name) = entry.file_name().to_str().map(|it| format!("{prefix}{it}")) else {
            continue;
        };
        if found.is_file() {
            objects.push(Object { name, len: found.len(), written: found.modified()? });
        }
    }
    Ok(objects)
}

/// Copies a file into a directory remote so that it is there whole or not at all.
fn copy_whole(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut partial = to.as_os_str().to_owned();
    partial.push(format!(".{}.tmp", std::process::id()));
    let partial = PathBuf::from(partial);
    let copied = std::fs::copy(from, &partial)
        .and_then(|_| File::options().write(true).open(&partial))
        .and_then(|file| name_file_whole(&file, &partial, to));
    if copied.is_err() {
        _ = std::fs::remove_file(&partial);
    }
    copied
}

/// Writes a file of a directory remote if it still hashes to `read_at`, under a lock that
/// the other machines writing it take too.
fn write_if_unchanged(to: &Path, bytes: &[u8], read_at: Option<&str>) -> anyhow::Result<Written> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut lock = to.as_os_str().to_owned();
    lock.push(".lock");
    let lock = File::create(lock)?;
    lock.lock()?;
    let now = match std::fs::read(to) {
        Ok(bytes) => Some(Digest::of(&bytes).to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if now.as_deref() != read_at {
        return Ok(Written::Lost);
    }
    write_file_whole(to, bytes)?;
    Ok(Written::Done)
}

/// How the transfers of one push or pull ended.
#[derive(Default)]
pub struct Tally {
    pub done: usize,
    pub bytes: u64,
    /// Left to another process that was fetching the same thing.
    pub elsewhere: usize,
    pub failed: Vec<anyhow::Error>,
}

impl Tally {
    /// Runs `work` on every item, a few at a time. One that fails does not stop the others.
    pub async fn of<T, F>(
        items: impl IntoIterator<Item = T>,
        jobs: usize,
        work: impl FnMut(T) -> F,
    ) -> Tally
    where
        F: Future<Output = anyhow::Result<Fetched>>,
    {
        let mut tally = Tally::default();
        let mut results = stream::iter(items).map(work).buffer_unordered(jobs);
        while let Some(result) = results.next().await {
            match result {
                Ok(Fetched::Whole(bytes)) => {
                    tally.done += 1;
                    tally.bytes += bytes;
                }
                Ok(Fetched::Elsewhere) => tally.elsewhere += 1,
                Err(e) => tally.failed.push(e),
            }
        }
        tally
    }

    /// `said` when everything went through, and an error that says how much did not
    /// otherwise, after each failure on a line of its own.
    pub fn settle(self, said: String, traffic: &Traffic) -> anyhow::Result<String> {
        if self.failed.is_empty() {
            return Ok(said);
        }
        for failure in &self.failed {
            eprintln!("remote: {failure:#}");
        }
        let rejected = traffic.rejected();
        bail!("{said}; {} failed, {rejected} of them rejected", self.failed.len())
    }
}

/// A pack as a manifest lists it: `PACK SIZE KEYS FORMAT PACKED PACKED_SIZE`, the hash of
/// the pack, how many bytes it is, the hash of the list of its keys, the format of the
/// store it is of, and the hash and size of the object that holds it compressed. A line
/// without the last two is a pack that was sent as it is.
#[derive(Clone, Copy)]
struct Listed {
    size: u64,
    keys: Digest,
    format: u32,
    packed: Option<(Digest, u64)>,
}

impl Listed {
    /// The object the pack is in, and the hash of that object.
    fn object(&self, pack: &Digest) -> (String, Digest) {
        match self.packed {
            Some((packed, _)) => (format!("packs/{pack}.pack.zst"), packed),
            None => (pack_object(pack), *pack),
        }
    }
}

impl Listed {
    /// The most the list of the pack's keys can be: the keys are in the pack too.
    fn keys_max(&self, remote: &Remote) -> u64 {
        self.size.min(remote.object_max)
    }
}

/// What a project's builds were served from.
#[derive(Default)]
struct Manifest(BTreeMap<Digest, Listed>);

impl Manifest {
    fn at(project: &str) -> String {
        format!("manifests/{project}")
    }

    /// The packs of the lines that are whole. A line that is not is one this version does
    /// not know how to read, and names nothing to fetch.
    fn parse(lines: &str) -> Manifest {
        let packs = lines.lines().filter_map(|line| {
            let mut fields = line.split(' ');
            let pack = Digest::parse(fields.next()?)?;
            let size = fields.next()?.parse().ok()?;
            let keys = Digest::parse(fields.next()?)?;
            let format = fields.next()?.parse().ok()?;
            let packed = match (fields.next(), fields.next()) {
                (Some(packed), Some(size)) => Some((Digest::parse(packed)?, size.parse().ok()?)),
                _ => None,
            };
            Some((pack, Listed { size, keys, format, packed }))
        });
        Manifest(packs.collect())
    }

    fn lines(&self) -> String {
        let line = |(pack, it): (&Digest, &Listed)| match it.packed {
            Some((packed, size)) => {
                format!("{pack} {} {} {} {packed} {size}\n", it.size, it.keys, it.format)
            }
            None => format!("{pack} {} {} {}\n", it.size, it.keys, it.format),
        };
        self.0.iter().map(line).collect()
    }

    async fn fetch(remote: &Remote, project: &str) -> anyhow::Result<Option<Manifest>> {
        let lines = remote.read_list(&Manifest::at(project)).await?;
        Ok(lines.map(|it| Manifest::parse(&it)))
    }

    /// The manifest as the machine that adds to it reads it.
    async fn fetch_to_extend(remote: &Remote, project: &str) -> anyhow::Result<Option<Manifest>> {
        let lines = remote.read_list_to_extend(&Manifest::at(project)).await?;
        Ok(lines.map(|it| Manifest::parse(&it)))
    }

    /// The packs of the format this build reads that are not in `local`, and how many of
    /// another format were passed over.
    fn missing(&self, local: &BTreeSet<Digest>) -> (Vec<(Digest, Listed)>, usize) {
        let absent = self.0.iter().filter(|(pack, _)| !local.contains(pack));
        let (ours, others): (Vec<_>, Vec<_>) = absent
            .map(|(pack, listed)| (*pack, *listed))
            .partition(|it| it.1.format == STORE_FORMAT);
        (ours, others.len())
    }
}

/// Whether what a fetched file starts with is a pack of the format this build reads. A
/// manifest that lists a pack as of this format and is wrong about it is not believed.
fn is_pack_of_this_format(start: &[u8]) -> bool {
    pack_format(start) == Some(STORE_FORMAT)
}

fn pack_file(pack: &Digest) -> String {
    format!("{pack}.pack")
}

fn pack_object(pack: &Digest) -> String {
    format!("packs/{pack}.pack")
}

fn keys_object(keys: &Digest) -> String {
    format!("keys/{keys}")
}

/// The sealed packs of a store, by the hash their names say.
fn local_packs(store: &StoreDir) -> anyhow::Result<BTreeSet<Digest>> {
    let names = store.packs()?;
    Ok(names.iter().filter_map(|it| Digest::parse(it.strip_suffix(".pack")?)).collect())
}

/// The keys of a sealed pack as one object, if the pack is what its name says. A pack
/// that is not is one a disk or a stopped process spoiled, and is never sent.
fn keys_of(store: &Path, pack: Digest) -> anyhow::Result<(u64, Vec<u8>)> {
    let name = pack_file(&pack);
    let file = File::open(store.join(&name))?;
    // SAFETY: a sealed pack is never written to again.
    let bytes = unsafe { memmap2::Mmap::map(&file)? };
    match pack_keys(&name, &bytes) {
        Ok(keys) => Ok((bytes.len() as u64, keys.concat())),
        Err(why) => bail!("{name} of the store {why}: it is not sent"),
    }
}

/// Sends a pack and its keys, unless the remote has them, and says how to list it.
async fn send_pack(store: &Path, remote: &Remote, pack: Digest) -> anyhow::Result<(Listed, u64)> {
    let from = store.to_owned();
    let (size, keys) = tokio::task::spawn_blocking(move || keys_of(&from, pack)).await??;
    let keys_digest = Digest::of(&keys);
    remote.send_named_by_hash(&keys_object(&keys_digest), keys).await?;
    let from = store.join(pack_file(&pack));
    let packed = tokio::task::spawn_blocking(move || packed(&from, size)).await??;
    let mut listed = Listed { size, keys: keys_digest, format: STORE_FORMAT, packed: None };
    if let Some(packed) = packed {
        let len = packed.len() as u64;
        listed.packed = Some((Digest::of(&packed), len));
        let (object, _) = listed.object(&pack);
        remote.send_named_by_hash(&object, packed).await?;
        return Ok((listed, len));
    }
    let (object, _) = listed.object(&pack);
    if remote.holds(&object, size).await? {
        return Ok((listed, 0));
    }
    remote.send_file(&object, &store.join(pack_file(&pack)), size).await?;
    Ok((listed, size))
}

/// The pack at `path`, of `size` bytes, compressed, when that makes it a quarter smaller or
/// more. A pack's small values are compressed already and its large ones are not, which
/// keeps them as they are to be read in place: compiled code that compresses to a fifth.
fn packed(path: &Path, size: u64) -> anyhow::Result<Option<Vec<u8>>> {
    let file = File::open(path)?;
    // SAFETY: a sealed pack is never written to again.
    let bytes = unsafe { memmap2::Mmap::map(&file)? };
    let packed = zstd::bulk::compress(&bytes, 3)?;
    Ok((packed.len() as u64 * 4 <= size * 3).then_some(packed))
}

/// Makes the directory a store of this build's format, and seals what ended daemons left
/// open in it when `adopt` says to.
async fn claim(store: &Path, adopt: bool) -> anyhow::Result<StoreDir> {
    let dir = StoreDir::at(store);
    let claimed = tokio::task::spawn_blocking(move || {
        dir.claim()?;
        if adopt {
            dir.adopt();
        }
        std::io::Result::Ok(dir)
    });
    claimed.await?.with_context(|| store.display().to_string())
}

/// Sends the sealed packs the project's manifest lacks, and lists the ones that arrived.
pub async fn push(store: &Path, remote: &Remote, project: &str) -> anyhow::Result<String> {
    remote.may_write()?;
    let local = local_packs(&claim(store, true).await?)?;
    let listed = match Manifest::fetch_to_extend(remote, project).await {
        Ok(manifest) => manifest.unwrap_or_default(),
        Err(e) if required() => return Err(e.context("reading the manifest to add to")),
        // A list this machine does not trust is one it does not add to: see `extend_list`.
        Err(e) => {
            eprintln!("remote: {e:#}: every pack is listed as if it had none");
            Manifest::default()
        }
    };
    let missing: Vec<Digest> = local.into_iter().filter(|it| !listed.0.contains_key(it)).collect();
    let (mut sent, mut bytes, mut failed) = (Manifest::default(), 0, Vec::new());
    let mut sending = stream::iter(
        missing.iter().map(|&pack| async move { (pack, send_pack(store, remote, pack).await) }),
    )
    .buffer_unordered(remote.traffic.jobs());
    while let Some((pack, result)) = sending.next().await {
        match result {
            Ok((listed, size)) => {
                sent.0.insert(pack, listed);
                bytes += size;
            }
            Err(e) => failed.push(e),
        }
    }
    drop(sending);
    if !sent.0.is_empty() {
        let extend = |lines: &str| {
            let mut manifest = Manifest::parse(lines);
            manifest.0.extend(sent.0.iter().map(|(pack, listed)| (*pack, *listed)));
            manifest.lines()
        };
        remote.extend_list(&Manifest::at(project), extend).await?;
    }
    let said = format!("{} packs added to `{project}`, {bytes} bytes sent", sent.0.len());
    Tally { failed, ..Tally::default() }.settle(said, &remote.traffic)
}

/// Fetches a pack as its line of the manifest says it is held.
async fn fetch_pack(
    remote: &Remote,
    store: &Path,
    pack: &Digest,
    listed: &Listed,
) -> anyhow::Result<Fetched> {
    let (object, want) = listed.object(pack);
    // Both are as long as the list says: the object that is sent, and the pack in it.
    let unpacked = listed.packed.map(|_| (*pack, listed.size));
    let most = listed.packed.map_or(listed.size, |(_, size)| size);
    let to = store.join(pack_file(pack));
    remote.fetch_coded(&object, &want, unpacked, &to, most, is_pack_of_this_format).await
}

/// The packs of `missing` that fit in a store that may hold `limit` bytes, the first listed
/// first, and how many do not.
fn fitting(missing: Vec<(Digest, Listed)>, limit: Option<u64>) -> (Vec<(Digest, Listed)>, usize) {
    let mut room = limit.unwrap_or(u64::MAX);
    let mut fits = |listed: &Listed| {
        let fits = listed.size <= room;
        room -= if fits { listed.size } else { 0 };
        fits
    };
    let (fit, over): (Vec<_>, Vec<_>) = missing.into_iter().partition(|(_, listed)| fits(listed));
    (fit, over.len())
}

/// The packs of `missing` that have a key the store lacks, and how many have none. A store
/// makes one pack of many small ones, so a pack that is not there by name may be there by
/// what is in it. A list of keys that cannot be had leaves its pack to be fetched.
async fn lacking(
    remote: &Remote,
    dir: StoreDir,
    missing: Vec<(Digest, Listed)>,
) -> anyhow::Result<(Vec<(Digest, Listed)>, usize)> {
    let here = tokio::task::spawn_blocking(move || dir.keys()).await??;
    // A store with nothing in it lacks them all, and no list has to be fetched to know.
    if here.is_empty() || missing.is_empty() {
        return Ok((missing, 0));
    }
    let here = &here;
    let asked = stream::iter(missing.into_iter().map(|(pack, listed)| async move {
        let most = listed.keys_max(remote);
        let keys = remote.read_named_by_hash(&keys_object(&listed.keys), &listed.keys, most).await;
        if let Err(e) = &keys {
            eprintln!("remote: the keys of {pack}.pack are not known, so it is fetched: {e:#}");
        }
        let all_here = keys.is_ok_and(|keys| {
            !keys.is_empty() && keys.as_chunks::<32>().0.iter().all(|key| here.contains(key))
        });
        ((pack, listed), all_here)
    }));
    let asked: Vec<_> = asked.buffered(remote.traffic.jobs()).collect().await;
    let (here_already, lacking): (Vec<_>, Vec<_>) = asked.into_iter().partition(|it| it.1);
    Ok((lacking.into_iter().map(|it| it.0).collect(), here_already.len()))
}

/// Removes the packs that have gone unused the longest until the store is under its limit,
/// which what was just fetched may have put it over.
async fn keep_under(dir: StoreDir, limit: Option<u64>) -> anyhow::Result<String> {
    let collected = tokio::task::spawn_blocking(move || dir.collect(limit)).await??;
    Ok(match collected.evicted {
        0 => String::new(),
        evicted => format!(", {evicted} packs unused the longest evicted to stay under the limit"),
    })
}

/// Whether the remote lists any pack of `project`.
pub async fn lists(remote: &Remote, project: &str) -> anyhow::Result<bool> {
    Ok(Manifest::fetch(remote, project).await?.is_some_and(|it| !it.0.is_empty()))
}

/// Fetches the packs of the project's manifest that are not in the local store, as far as
/// they fit under `BLAZE_CACHE_MAX`. A pack of another format than this build reads is left
/// where it is, and one that is longer than the manifest says is rejected.
pub async fn pull(store: &Path, remote: &Remote, project: &str) -> anyhow::Result<String> {
    let limit = limit_for(store).map_err(anyhow::Error::msg)?;
    let dir = claim(store, false).await?;
    let local = local_packs(&dir)?;
    let Some(manifest) = Manifest::fetch(remote, project).await? else {
        return Ok(format!("`{project}` has no manifest on the remote: nothing fetched"));
    };
    let (missing, other_format) = manifest.missing(&local);
    let (missing, here_already) = lacking(remote, StoreDir::at(store), missing).await?;
    let (fit, over) = fitting(missing, limit);
    let fetch = |(pack, listed): (Digest, Listed)| async move {
        fetch_pack(remote, store, &pack, &listed).await
    };
    let tally = Tally::of(fit, remote.traffic.jobs(), fetch).await;
    let mut said = format!(
        "{} packs of `{project}` fetched, {} bytes",
        tally.done + tally.elsewhere,
        tally.bytes
    );
    if other_format != 0 {
        said += &format!(", {other_format} of another format left");
    }
    if here_already != 0 {
        said += &format!(", {here_already} that are in other packs of the store passed over");
    }
    if over != 0 {
        said += &format!(", {over} left that do not fit under BLAZE_CACHE_MAX");
    }
    if tally.done != 0 {
        said += &keep_under(dir, limit).await?;
    }
    tally.settle(said, &remote.traffic)
}

const USAGE: &str = "usage: blaze cache push|pull REMOTE [PROJECT]\n       \
                     blaze cache stats\n       \
                     blaze cache gc [--max SIZE]\n       \
                     blaze cache gc --remote REMOTE [--unlisted-after AGE] [--older-than AGE] \
                     [--dry-run]\n       \
                     blaze cache verify\n       \
                     blaze cache keygen FILE\n       \
                     blaze cache forget REMOTE";

/// `blaze-engine cache VERB`, on the store of the user's builds.
pub fn command(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let Some(verb) = args.next() else { bail!(USAGE) };
    if verb == "keygen" {
        return signed::keygen(Path::new(&args.next().context(USAGE)?));
    }
    let mut args = args.peekable();
    if verb == "gc" && args.next_if(|it| it == "--remote").is_some() {
        return collect_remote(args);
    }
    let store = crate::cargo::daemon::store().context("BLAZE_CACHE says there is no store")?;
    // Not for push and pull, which write to sockets: a reset from the remote would end them
    // by SIGPIPE, with no word of why (exit status 141).
    if matches!(verb.as_str(), "stats" | "gc" | "verify") {
        crate::end_with_reader();
    }
    match verb.as_str() {
        "stats" => stats(&store),
        "gc" => collect(&store, args),
        "verify" => verify(&store),
        "push" | "pull" => transfer_command(&store, &verb, args),
        "forget" => forget(&store, args),
        _ => bail!(USAGE),
    }
}

fn transfer_command(
    store: &Path,
    verb: &str,
    mut args: impl Iterator<Item = String>,
) -> anyhow::Result<()> {
    let Some(remote) = args.next() else { bail!(USAGE) };
    let project = project_name(args.next())?;
    let start = Instant::now();
    let (done, summary) = runtime()?.block_on(async {
        let remote = Remote::open(&remote, store).await?;
        let done = match verb {
            "push" => push(store, &remote, &project).await,
            _ => pull(store, &remote, &project).await,
        };
        eprintln!("remote: {}", remote.traffic);
        anyhow::Ok((done, remote.traffic.summary(verb, start.elapsed())))
    })?;
    // Said whether it went through or not: CI puts it in the job's summary.
    println!("{summary}");
    println!("{} in {:.1?}", done?, start.elapsed());
    Ok(())
}

/// Whether a remote that fails fails what it was used for: `BLAZE_REMOTE_REQUIRED=1`, which
/// CI sets so that a cache that stopped working is not a slowdown nobody sees. Without it the
/// failure is said on every build's `Finished` line and the build goes on.
pub fn required() -> bool {
    std::env::var_os("BLAZE_REMOTE_REQUIRED").is_some_and(|it| it != "0" && !it.is_empty())
}

/// The file in a store's directory that says how the daemon's last exchange with its remote
/// failed, for the builds it serves to say: their `Finished` line is not the daemon's.
const DOWN: &str = "remote-down";

/// Remembers how the last exchange with the remote `place` went: `failed` or not.
fn note_down(store: &Path, place: &str, failed: Option<&anyhow::Error>) {
    let file = store.join(DOWN);
    let noted = match failed {
        Some(e) => {
            let why = format!("{e:#}");
            let why = why.lines().next().unwrap_or_default();
            write_file_whole(&file, format!("{place}: {why}").as_bytes())
        }
        None => match std::fs::remove_file(&file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    };
    if let Err(e) = noted {
        eprintln!("remote: {}: {e}", file.display());
    }
}

/// What the build's `Finished` line says of the remote: how it failed last, if it did.
pub fn noted_down() -> Option<String> {
    let store = crate::cargo::daemon::store()?;
    let said = std::fs::read_to_string(store.join(DOWN)).ok()?;
    Some(said.trim().to_owned()).filter(|it| !it.is_empty())
}

/// `blaze-engine cache forget REMOTE`: forgets the turns taken of the remote's lists, so that
/// whatever list it has next is taken. For a remote that was started again from nothing
/// by a machine whose clock is behind, or a memory that was damaged.
fn forget(store: &Path, mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let Some(remote) = args.next() else { bail!(USAGE) };
    let forgot = runtime()?
        .block_on(async { anyhow::Ok(Remote::open(&remote, store).await?.forget_turns()?) })?;
    if forgot {
        println!("the turns taken of the lists of {remote} are forgotten");
    } else {
        println!("no turn was taken of a list of {remote}");
    }
    Ok(())
}

/// `blaze-engine cache gc --remote REMOTE`: takes out of the remote what its lists no longer
/// name, and of its lists what is old or held twice. An age is a count and `s`, `m`, `h`
/// or `d`.
fn collect_remote(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let Some(remote) = args.next() else { bail!(USAGE) };
    let mut ages = collect::Ages { unlisted: collect::Ages::UNLISTED, listed: None, dry: false };
    while let Some(flag) = args.next() {
        let mut age = || {
            let age = args.next().context(USAGE)?;
            collect::parse_age(&age).with_context(|| format!("`{age}` is not an age"))
        };
        match flag.as_str() {
            "--unlisted-after" => ages.unlisted = age()?,
            "--older-than" => ages.listed = Some(age()?),
            "--dry-run" => ages.dry = true,
            _ => bail!(USAGE),
        }
    }
    let start = Instant::now();
    // A machine that signs remembers the turns of the lists it writes where it keeps its
    // store, and without one where the daemon keeps what is its own.
    let kept_in = match std::env::var_os("BLAZE_CACHE") {
        Some(store) => PathBuf::from(store),
        None => crate::cargo::daemon::home()?,
    };
    let done = runtime()?.block_on(async {
        let remote = Remote::open(&remote, &kept_in).await?;
        let done = collect::collect(&remote, &ages).await;
        eprintln!("remote: {}", remote.traffic);
        done
    })?;
    println!("{done}\nin {:.1?}", start.elapsed());
    Ok(())
}

fn format_line(format: Option<u32>) -> String {
    match format {
        None => "format: none (the directory has no store in it)".to_owned(),
        Some(STORE_FORMAT) => format!("format: {STORE_FORMAT}"),
        Some(other) => format!("format: {other} (this blaze reads {STORE_FORMAT})"),
    }
}

fn seconds_since(at: SystemTime) -> u64 {
    at.elapsed().map_or(0, |it| it.as_secs())
}

/// `blaze-engine cache stats`: what the directory holds, without reading any pack.
fn stats(store: &Path) -> anyhow::Result<()> {
    let stats = StoreDir::at(store).stats().with_context(|| store.display().to_string())?;
    println!("{}", format_line(stats.format));
    println!("packs: {}", stats.packs);
    println!("pack bytes: {}", stats.pack_bytes);
    println!("open packs: {}", stats.open);
    println!("open bytes: {}", stats.open_bytes);
    println!("temporary files: {}", stats.temporary);
    println!("temporary bytes: {}", stats.temporary_bytes);
    match limit_for(store).map_err(anyhow::Error::msg)? {
        Some(limit) => println!("limit bytes: {limit}"),
        None => println!("limit bytes: off"),
    }
    if let Some((first, last)) = stats.used {
        println!("least recently used: {} seconds ago", seconds_since(first));
        println!("most recently used: {} seconds ago", seconds_since(last));
    }
    Ok(())
}

/// `blaze-engine cache gc [--max SIZE]`: collects garbage now, down to the size given or the one
/// `BLAZE_CACHE_MAX` names.
fn collect(store: &Path, mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let limit = match (args.next().as_deref(), args.next()) {
        (None, _) => limit_for(store).map_err(anyhow::Error::msg)?,
        (Some("--max"), Some(size)) => {
            Some(parse_size(&size).with_context(|| format!("`{size}` is not a size"))?)
        }
        _ => bail!(USAGE),
    };
    let dir = StoreDir::at(store);
    dir.claim()?;
    let collected = dir.collect(limit)?;
    if collected.busy {
        bail!("another process is collecting the store");
    }
    println!("evicted packs: {}", collected.evicted);
    println!("evicted bytes: {}", collected.evicted_bytes);
    println!("merged packs: {}", collected.merged);
    println!("sealed open packs: {}", collected.adopted);
    println!("removed temporary files: {}", collected.abandoned);
    println!("kept packs: {}", collected.kept);
    println!("kept bytes: {}", collected.kept_bytes);
    Ok(())
}

/// `blaze-engine cache verify`: reads everything and fails if any of it is not what was written.
fn verify(store: &Path) -> anyhow::Result<()> {
    let verified = StoreDir::at(store).verify().with_context(|| store.display().to_string())?;
    println!("{}", format_line(verified.format));
    println!("packs: {}", verified.packs);
    println!("records: {}", verified.records);
    println!("bytes: {}", verified.bytes);
    println!("open packs: {}", verified.open);
    println!("torn bytes: {}", verified.torn_bytes);
    println!("corrupt packs: {}", verified.corrupt.len());
    for (name, why) in &verified.corrupt {
        println!("corrupt: {name} {why}");
    }
    if verified.format.is_some_and(|it| it != STORE_FORMAT) {
        bail!("the store is not of the format this blaze reads");
    }
    if !verified.corrupt.is_empty() {
        bail!("{} packs are not what was written", verified.corrupt.len());
    }
    Ok(())
}

/// The name of a project, `default` when none is given, as the name of an object.
pub fn project_name(given: Option<String>) -> anyhow::Result<String> {
    let project = given.unwrap_or_else(|| "default".to_owned());
    if project.is_empty() || project.contains(['/', '\\']) || project.starts_with('.') {
        bail!("`{project}` is not the name of a project");
    }
    Ok(project)
}

/// The runtime the transfers of this process run on. The compiler's threads are not its
/// workers: they wait on it from outside.
pub fn runtime() -> anyhow::Result<Runtime> {
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    Ok(runtime.worker_threads(2).thread_name("blaze-remote").enable_all().build()?)
}

/// The remote the daemon keeps its store in step with: `BLAZE_REMOTE`, and the project
/// `BLAZE_PROJECT` names in it.
pub struct Shared {
    store: PathBuf,
    remote: Arc<Remote>,
    project: String,
    runtime: Arc<Runtime>,
    /// Whether packs are left on the remote until a build asks for a key of theirs.
    lazy: bool,
    /// How often what the daemon compiled is sent while it runs, if on a schedule at all.
    push_every: Option<Duration>,
    asked: Arc<keeper::Asked>,
}

impl Shared {
    /// `None` without a store or a remote, or with `BLAZE_REMOTE=off`. A remote that
    /// cannot be reached is not an error to the build, unless [`required`]: it compiles
    /// what it would have fetched, and its `Finished` line says so.
    pub fn from_env() -> anyhow::Result<Option<Shared>> {
        let Some(store) = std::env::var_os("BLAZE_CACHE").map(PathBuf::from) else {
            return Ok(None);
        };
        let remote = match std::env::var("BLAZE_REMOTE") {
            Ok(remote) if !remote.is_empty() && remote != "off" => remote,
            _ => return Ok(None),
        };
        let opened = runtime().and_then(|runtime| {
            let project = project_name(std::env::var("BLAZE_PROJECT").ok())?;
            let push_every = keeper::push_every_from_env()?;
            let remote = Arc::new(runtime.block_on(Remote::open(&remote, &store))?);
            let lazy = std::env::var_os("BLAZE_REMOTE_LAZY").is_some_and(|it| it != "0");
            let (runtime, asked) = (Arc::new(runtime), Arc::default());
            Ok(Shared { store: store.clone(), remote, project, runtime, lazy, push_every, asked })
        });
        match opened {
            Ok(shared) => Ok(Some(shared)),
            Err(e) => {
                eprintln!("remote: {e:#}");
                note_down(&store, &remote, Some(&e));
                match required() {
                    true => Err(e.context("BLAZE_REMOTE_REQUIRED is set")),
                    false => Ok(None),
                }
            }
        }
    }

    /// The remote `spec` names, for the project `project` of the store in `store`, with its
    /// packs left there until a key of theirs is asked for: what takes a part of a project
    /// shared by all, as `blaze-engine closure materialize` takes the store paths it is asked for.
    /// A remote that cannot be opened is an error; one that fails later is said, and what
    /// it would have brought is missing from the store.
    pub fn lazily(store: &Path, spec: &str, project: &str) -> anyhow::Result<Shared> {
        let runtime = runtime()?;
        let remote = Arc::new(runtime.block_on(Remote::open(spec, store))?);
        Ok(Shared {
            store: store.to_owned(),
            remote,
            project: project_name(Some(project.to_owned()))?,
            runtime: Arc::new(runtime),
            lazy: true,
            push_every: None,
            asked: Arc::default(),
        })
    }

    /// Fetches what the project's manifest lists, or with `BLAZE_REMOTE_LAZY` only what
    /// tells which pack holds which key, leaving each pack to the first build that asks.
    /// From here on the store asks this remote for the keys it lacks. An error only when
    /// [`required`].
    pub fn pull(&self) -> anyhow::Result<()> {
        // An outage during one build is over by the next, or is found again.
        self.remote.traffic.try_again();
        let start = Instant::now();
        let done = self.runtime.block_on(async {
            self.asked.see(self.manifest_now().await);
            self.pull_as_set().await
        });
        let asked = self.asked.clone();
        blaze_store::fetch_missing_with(Box::new(move |key| asked.fetch(key)));
        self.report(done, start)
    }

    /// A pull the way the daemon is set to pull: of every pack, or of what says where the
    /// keys are.
    async fn pull_as_set(&self) -> anyhow::Result<String> {
        if !self.lazy {
            return pull(&self.store, &self.remote, &self.project).await;
        }
        let lazy = Lazy::load(self).await?;
        let said =
            format!("{} keys in {} packs to fetch from", lazy.holder.len(), lazy.packs.len());
        self.asked.fetch_from(lazy);
        Ok(said)
    }

    /// What the project's manifest hashes to now: `None` when there is none or it cannot
    /// be read.
    async fn manifest_now(&self) -> Option<Digest> {
        let read = self.remote.read_small(&Manifest::at(&self.project), LIST_MAX).await;
        Some(Digest::of(&read.ok()??.0))
    }

    /// An error only when [`required`].
    pub fn push(&self) -> anyhow::Result<()> {
        if self.remote.may_write().is_err() {
            return Ok(());
        }
        self.remote.traffic.try_again();
        let start = Instant::now();
        let done = self.runtime.block_on(push(&self.store, &self.remote, &self.project));
        self.report(done, start)
    }

    fn report(&self, done: anyhow::Result<String>, start: Instant) -> anyhow::Result<()> {
        let done = match done {
            Ok(done) => {
                eprintln!("remote: {done} in {:.1?}", start.elapsed());
                note_down(&self.store, &self.remote.place.name(), None);
                Ok(())
            }
            Err(e) => self.failed(e),
        };
        eprintln!("remote: {}", self.remote.traffic);
        done
    }

    /// Says how an exchange with the remote failed, here and on the builds' `Finished`
    /// line, and is that failure when [`required`].
    fn failed(&self, e: anyhow::Error) -> anyhow::Result<()> {
        eprintln!("remote: {e:#}");
        note_down(&self.store, &self.remote.place.name(), Some(&e));
        match required() {
            true => Err(e.context("BLAZE_REMOTE_REQUIRED is set")),
            false => Ok(()),
        }
    }
}

/// How long a thread that compiles waits for the pack of a key it lacks, while the remote
/// brings packs within it: a key that comes in that time is not compiled. Once one takes
/// longer, no thread of this process waits on the remote again.
const WAIT_AT_MOST: Duration = Duration::from_millis(250);

/// A pack being fetched: whether it came, once that is known.
#[derive(Default)]
struct Arriving {
    came: std::sync::Mutex<Option<bool>>,
    known: std::sync::Condvar,
}

/// The packs of a manifest that are not here yet, and which of them holds each key.
pub struct Lazy {
    store: PathBuf,
    remote: Arc<Remote>,
    runtime: Arc<Runtime>,
    /// Each pack and, once a key of it was asked for, its fetch.
    packs: Vec<(Digest, Listed, OnceLock<Arc<Arriving>>)>,
    holder: FxHashMap<[u8; 32], u32>,
    /// Whether every pack waited for came within [`WAIT_AT_MOST`].
    in_time: AtomicBool,
}

impl Lazy {
    /// Fetches the keys of every pack the manifest lists that the store lacks. They are
    /// kept in memory: the store's directory holds nothing that is not the store's.
    async fn load(shared: &Shared) -> anyhow::Result<Lazy> {
        let (store, remote) = (&shared.store, &shared.remote);
        let local = local_packs(&claim(store, false).await?)?;
        let manifest = Manifest::fetch(remote, &shared.project).await?.unwrap_or_default();
        let (missing, _) = manifest.missing(&local);
        // A pack the store may not hold is not one to fetch, and the keys of them all are
        // kept in memory: no more of them than one object may be.
        let limit = limit_for(store).map_err(anyhow::Error::msg)?.unwrap_or(u64::MAX);
        let held = &Room::of(Some(remote.object_max));
        let fetched = stream::iter(missing.into_iter().map(|(pack, listed)| async move {
            if listed.size > limit {
                bail!("{pack}.pack is listed as {} bytes, over BLAZE_CACHE_MAX", listed.size);
            }
            let most = listed.keys_max(remote);
            let keys =
                remote.read_named_by_hash(&keys_object(&listed.keys), &listed.keys, most).await?;
            held.take(keys.len() as u64)?;
            Ok((pack, listed, keys))
        }))
        .buffer_unordered(remote.traffic.jobs())
        .collect::<Vec<_>>()
        .await;
        let mut lazy = Lazy {
            store: store.clone(),
            remote: remote.clone(),
            runtime: shared.runtime.clone(),
            packs: Vec::new(),
            holder: FxHashMap::default(),
            in_time: AtomicBool::new(true),
        };
        for fetched in fetched {
            match fetched {
                Ok((pack, listed, keys)) => lazy.add(pack, listed, &keys),
                Err(e) if required() => return Err(e),
                Err(e) => eprintln!("remote: {e:#}"),
            }
        }
        Ok(lazy)
    }

    fn add(&mut self, pack: Digest, listed: Listed, keys: &[u8]) {
        let at = self.packs.len() as u32;
        self.packs.push((pack, listed, OnceLock::new()));
        for key in keys.as_chunks::<32>().0 {
            self.holder.entry(*key).or_insert(at);
        }
    }

    /// Whether the pack that holds `key` is in the store now. The first key of a pack that is
    /// asked for starts fetching it; the thread that asked waits for it [`WAIT_AT_MOST`], while
    /// the remote keeps within that, and otherwise compiles the key rather than wait on the
    /// network. A pack that comes later is read by the store then, and the keys asked for
    /// after that are found.
    fn fetch(&self, key: &[u8; 32]) -> bool {
        let Some(&at) = self.holder.get(key) else { return false };
        let (pack, listed, asked) = &self.packs[at as usize];
        let mut first = false;
        let arriving = asked.get_or_init(|| {
            first = true;
            Arc::default()
        });
        if first {
            let (pack, listed, arriving) = (*pack, *listed, arriving.clone());
            let (remote, store) = (self.remote.clone(), self.store.clone());
            self.runtime.spawn(async move {
                let came = match fetch_pack(&remote, &store, &pack, &listed).await {
                    Ok(Fetched::Whole(bytes)) => {
                        eprintln!("remote: {pack}.pack fetched for a build, {bytes} bytes");
                        true
                    }
                    Ok(Fetched::Elsewhere) => false,
                    // The build compiles what the pack held; with BLAZE_REMOTE_REQUIRED the
                    // client fails it once it is over.
                    Err(e) => {
                        let why = format!("{e:#}");
                        eprintln!("remote: {}", why.lines().next().unwrap_or_default());
                        note_down(&store, &remote.place.name(), Some(&e));
                        false
                    }
                };
                *arriving.came.lock().unwrap_or_else(|it| it.into_inner()) = Some(came);
                arriving.known.notify_all();
                if came && let Some(store) = blaze_store::Store::global() {
                    store.refresh();
                }
            });
        }
        if !self.in_time.load(Ordering::Relaxed) {
            return false;
        }
        let came = arriving.came.lock().unwrap_or_else(|it| it.into_inner());
        let waited = arriving.known.wait_timeout_while(came, WAIT_AT_MOST, |it| it.is_none());
        let (came, _) = waited.unwrap_or_else(|it| it.into_inner());
        match *came {
            Some(came) => came,
            None => {
                if self.in_time.swap(false, Ordering::Relaxed) {
                    eprintln!(
                        "remote: a pack took over {WAIT_AT_MOST:?}: what is not here is compiled \
                         while the packs come"
                    );
                }
                false
            }
        }
    }
}

#[cfg(test)]
impl Remote {
    /// The directory `dir` as a remote, with what is taken of its lists remembered in
    /// `kept_in`.
    pub fn in_dir(dir: &Path, kept_in: &Path, trust: Trust, object_max: u64) -> Remote {
        let traffic = Arc::new(Traffic::new(8));
        Remote::in_place(Place::Dir(dir.to_owned()), traffic, kept_in, trust, object_max)
    }

    /// The gateway tenant `spec` names as a remote, reached with `token`.
    pub fn at_gateway(spec: &str, token: Option<&str>, kept_in: &Path, trust: Trust) -> Remote {
        let (traffic, patience) = (Arc::new(Traffic::new(8)), Duration::from_secs(5));
        let gateway =
            Gateway::with_token(spec, token.map(str::to_owned), patience, traffic.clone());
        let place = Place::Gateway(Box::new(gateway.unwrap()));
        Remote::in_place(place, traffic, kept_in, trust, OBJECT_MAX)
    }

    fn in_place(
        place: Place,
        traffic: Arc<Traffic>,
        kept_in: &Path,
        trust: Trust,
        object_max: u64,
    ) -> Remote {
        let turns = Turns::of(kept_in, &place.name());
        Remote {
            place,
            traffic,
            patience: Duration::from_secs(5),
            read_only: false,
            trust,
            turns,
            object_max,
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-remote-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Writes an object of a directory remote.
    pub fn put(remote: &Path, name: &str, bytes: &[u8]) {
        let to = remote.join(name);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::write(to, bytes).unwrap();
    }

    /// The keys of a machine that names none and was told to take what is there.
    pub fn taking_unsigned() -> Trust {
        Trust::of(None, &[], true)
    }

    /// A remote in a new directory whose lists nobody signs and this machine takes.
    pub fn unsigned(name: &str, object_max: u64) -> (Remote, PathBuf) {
        let dir = scratch(name);
        (Remote::in_dir(&dir, &dir.join("kept"), taking_unsigned(), object_max), dir)
    }

    pub fn wait<T>(work: impl Future<Output = T>) -> T {
        runtime().unwrap().block_on(work)
    }

    /// `seed` and `runs` changes of it, the same ones in every run: bytes cut, flipped,
    /// put in and taken out.
    pub fn changed(seed: &[u8], runs: usize) -> impl Iterator<Item = Vec<u8>> {
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
                match next(5) {
                    0 => bytes.truncate(at),
                    1 if !bytes.is_empty() => bytes[at] ^= 1 << next(8),
                    2 => bytes.insert(at, b"\n /.\\\0\xff"[next(7)]),
                    3 if !bytes.is_empty() => _ = bytes.remove(at),
                    _ => {
                        let from = next(bytes.len());
                        let again = bytes[from..].to_vec();
                        bytes.splice(at..at, again);
                    }
                }
            }
            bytes
        }))
    }

    /// How many bytes the files below `dir` hold.
    pub fn held(dir: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
        entries
            .flatten()
            .map(|it| match it.metadata() {
                Ok(meta) if meta.is_dir() => held(&it.path()),
                Ok(meta) => meta.len(),
                Err(_) => 0,
            })
            .sum()
    }

    #[test]
    fn an_object_is_read_to_the_bound_and_not_past_it() {
        let (remote, dir) = unsigned("bounded", 1 << 20);
        put(&dir, "keys/long", &[7; 100]);
        wait(async {
            for most in [100, 1 << 40] {
                assert_eq!(
                    remote.read_small("keys/long", most).await.unwrap().unwrap().0.len(),
                    100
                );
            }
            for most in [0, 1, 99] {
                let refused = remote.read_small("keys/long", most).await.err().unwrap();
                assert!(refused.is::<TooLong>(), "{refused}");
            }
            assert!(remote.read_small("keys/none", 0).await.unwrap().is_none());
        });
        _ = std::fs::remove_dir_all(&dir);
    }

    /// An object with no end is given up on at the bound.
    #[cfg(unix)]
    #[test]
    fn an_object_without_an_end_is_given_up_on() {
        wait(async {
            let endless = tokio::fs::File::open("/dev/zero").await.unwrap();
            let refused = read_at_most(Body::File(endless), 1 << 20, "zero").await.unwrap_err();
            assert!(refused.is::<TooLong>(), "{refused}");
        });
    }

    /// A remote with one thing that starts as a pack does, listed as `listed_size` bytes.
    fn remote_with_pack(name: &str, len: usize, listed_size: u64) -> (Remote, PathBuf, Digest) {
        let (remote, dir) = unsigned(name, 1 << 20);
        let mut pack = [&b"blazepk"[..], &[STORE_FORMAT as u8]].concat();
        pack.resize(len, 7);
        let (named, keys) = (Digest::of(&pack), Digest::of(&[1; 32]));
        put(&dir, &pack_object(&named), &pack);
        put(&dir, &keys_object(&keys), &[1; 32]);
        let listed = Listed { size: listed_size, keys, format: STORE_FORMAT, packed: None };
        put(&dir, "manifests/default", Manifest([(named, listed)].into()).lines().as_bytes());
        (remote, dir, named)
    }

    #[test]
    fn a_pack_longer_than_its_list_says_is_not_written() {
        let len = 3 * (1 << 16) + 100;
        for listed_size in [0, 8, 1 << 16, len as u64 - 1] {
            let (remote, dir, _) = remote_with_pack("long-pack", len, listed_size);
            let store = dir.join("store");
            let refused = wait(pull(&store, &remote, "default")).unwrap_err().to_string();
            assert!(refused.contains("1 failed, 1 of them rejected"), "{refused}");
            let left: Vec<_> = std::fs::read_dir(&store).unwrap().flatten().collect();
            let left: Vec<_> = left.iter().map(|it| it.file_name()).collect();
            assert!(left.iter().all(|it| it == "version" || it == "lock"), "{left:?}");
            _ = std::fs::remove_dir_all(&dir);
        }
        let (remote, dir, named) = remote_with_pack("exact-pack", len, len as u64);
        let store = dir.join("store");
        std::fs::create_dir_all(&store).unwrap();
        assert!(matches!(wait(fetched(&remote, &store, named, len as u64)), Fetched::Whole(_)));
        assert_eq!(held(&store), len as u64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let pack = std::fs::metadata(store.join(pack_file(&named))).unwrap();
            assert_eq!(pack.permissions().mode() & 0o777, 0o600, "a fetched pack is the user's");
        }
        _ = std::fs::remove_dir_all(&dir);
    }

    async fn fetched(remote: &Remote, store: &Path, pack: Digest, most: u64) -> Fetched {
        let (from, to) = (pack_object(&pack), store.join(pack_file(&pack)));
        remote.fetch_file(&from, &pack, &to, most, is_pack_of_this_format).await.unwrap()
    }

    /// A remote with one pack of `len` bytes that all are the same, sent compressed and
    /// listed as `listed_size` bytes unpacked.
    fn remote_with_packed(name: &str, len: usize, listed_size: u64) -> (Remote, PathBuf, u64) {
        let (remote, dir) = unsigned(name, 1 << 20);
        let mut pack = [&b"blazepk"[..], &[STORE_FORMAT as u8]].concat();
        pack.resize(len, 7);
        let sent = zstd::encode_all(&pack[..], 3).unwrap();
        let (named, keys, packed) = (Digest::of(&pack), Digest::of(&[1; 32]), Digest::of(&sent));
        put(&dir, &format!("packs/{named}.pack.zst"), &sent);
        put(&dir, &keys_object(&keys), &[1; 32]);
        let packed = Some((packed, sent.len() as u64));
        let listed = Listed { size: listed_size, keys, format: STORE_FORMAT, packed };
        put(&dir, "manifests/default", Manifest([(named, listed)].into()).lines().as_bytes());
        (remote, dir, sent.len() as u64)
    }

    #[test]
    fn a_pack_that_unpacks_to_more_than_its_list_says_is_not_written() {
        let len = 64 << 20;
        for listed_size in [0, 1 << 16, len as u64 - 1] {
            let (remote, dir, sent) = remote_with_packed("bomb-pack", len, listed_size);
            assert!(sent < 64 << 10, "{sent} bytes are sent for {len}");
            let store = dir.join("store");
            let refused = wait(pull(&store, &remote, "default")).unwrap_err().to_string();
            assert!(refused.contains("1 failed, 1 of them rejected"), "{refused}");
            let left: Vec<_> = std::fs::read_dir(&store).unwrap().flatten().collect();
            let left: Vec<_> = left.iter().map(|it| it.file_name()).collect();
            assert!(left.iter().all(|it| it == "version" || it == "lock"), "{left:?}");
            _ = std::fs::remove_dir_all(&dir);
        }
        let (remote, dir, _) = remote_with_packed("packed-pack", 1 << 20, 1 << 20);
        let store = dir.join("store");
        wait(pull(&store, &remote, "default")).unwrap();
        let more = held(&store) - (1 << 20);
        assert!(more < 100, "the pack and the store's two files, and {more} bytes more");
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_packed_pack_is_all_written_before_it_is_unpacked() {
        // Bytes zstd makes nothing smaller of, so what is sent ends in a chunk of its own.
        let mut pack = [&b"blazepk"[..], &[STORE_FORMAT as u8]].concat();
        let mut seed = 0x9e3779b97f4a7c15u64;
        while pack.len() < (64 << 10) + 9 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            pack.extend_from_slice(&seed.to_le_bytes());
        }
        let sent = zstd::encode_all(&pack[..], 3).unwrap();
        let (named, keys) = (Digest::of(&pack), Digest::of(&[1; 32]));
        let packed = Some((Digest::of(&sent), sent.len() as u64));
        let listed = Listed { size: pack.len() as u64, keys, format: STORE_FORMAT, packed };
        for round in 0..500 {
            let (remote, dir) = unsigned("flushed-pack", 1 << 20);
            put(&dir, &format!("packs/{named}.pack.zst"), &sent);
            put(&dir, &keys_object(&keys), &[1; 32]);
            put(&dir, "manifests/default", Manifest([(named, listed)].into()).lines().as_bytes());
            let pulled = wait(pull(&dir.join("store"), &remote, "default"));
            _ = std::fs::remove_dir_all(&dir);
            pulled.unwrap_or_else(|e| panic!("round {round}: {e:#}"));
        }
    }

    #[test]
    fn a_packed_pack_longer_than_its_list_says_is_not_unpacked() {
        let (remote, dir, sent) = remote_with_packed("long-packed", 1 << 20, 1 << 20);
        let manifest = std::fs::read_to_string(dir.join("manifests/default")).unwrap();
        let short = manifest.trim_end().rsplit_once(' ').unwrap().0;
        put(&dir, "manifests/default", format!("{short} {}\n", sent - 1).as_bytes());
        let store = dir.join("store");
        let refused = wait(pull(&store, &remote, "default")).unwrap_err().to_string();
        assert!(refused.contains("1 failed, 1 of them rejected"), "{refused}");
        assert!(held(&store) < 100, "{} bytes are in the store", held(&store));
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_machine_without_a_key_does_not_write_a_signed_list_back() {
        let signed = Signed::new("kept-signed");
        let keyless = |told| {
            let trust = Trust::of(None, &[], told);
            Remote::in_dir(&signed.dir.join("remote"), &signed.dir.join("c"), trust, 1 << 20)
        };
        wait(async {
            signed.add("one").await;
            let list = signed.list();
            for told in [false, true] {
                let remote = keyless(told);
                let add = |lines: &str| format!("{lines}two\n");
                let refused = remote.extend_list("manifests/default", add).await.unwrap_err();
                assert!(refused.is::<NotRewritten>(), "{refused}");
                let none = |_: &str| String::new();
                let refused = remote.shrink_list("manifests/default", none).await.unwrap_err();
                assert!(refused.is::<NotRewritten>(), "{refused}");
                assert_eq!(signed.list(), list, "the list is as it was signed");
            }
            // The signer takes lines out and signs what is left, at a later turn.
            let none = |_: &str| String::new();
            signed.signer.shrink_list("manifests/default", none).await.unwrap();
            assert_eq!(signed.read().await.unwrap(), "");
            // And does not take them out of a list that was put back under it.
            signed.put_back(&list);
            assert!(signed.signer.shrink_list("manifests/default", none).await.is_err());
            assert_eq!(signed.list(), list);
        });
        _ = std::fs::remove_dir_all(&signed.dir);
    }

    #[test]
    fn what_a_stopped_fetch_left_does_not_count_against_the_bound_twice() {
        let (remote, dir, named) = remote_with_pack("resumed-pack", 1000, 1000);
        let store = dir.join("store");
        std::fs::create_dir_all(&store).unwrap();
        // Longer than the pack may be, and not its start.
        std::fs::write(store.join(format!("{}.fetch.tmp", pack_file(&named))), [9; 5000]).unwrap();
        assert!(matches!(wait(fetched(&remote, &store, named, 1000)), Fetched::Whole(1000)));
        assert_eq!(held(&store), 1000);
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_manifest_longer_than_is_read_is_refused() {
        let (remote, dir) = unsigned("long-manifest", 1 << 20);
        let line = format!("{0} 4096 {0} {STORE_FORMAT}\n", "ab".repeat(32));
        let mut list = line.repeat(LIST_MAX as usize / line.len());
        list.push_str(&"\n".repeat(LIST_MAX as usize - list.len()));
        put(&dir, "manifests/default", list.as_bytes());
        wait(async {
            assert_eq!(Manifest::fetch(&remote, "default").await.unwrap().unwrap().0.len(), 1);
            list.push('\n');
            put(&dir, "manifests/default", list.as_bytes());
            let refused = Manifest::fetch(&remote, "default").await.err().unwrap();
            assert!(refused.is::<TooLong>(), "{refused}");
            assert!(remote.read_list_to_extend("manifests/default").await.is_err());
            let written = remote.extend_list("manifests/default", |lines| lines.to_owned()).await;
            assert!(written.is_err());
        });
        assert_eq!(std::fs::read(dir.join("manifests/default")).unwrap().len(), list.len());
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_keys_of_a_pack_are_no_longer_than_the_pack() {
        let (remote, dir) = unsigned("long-keys", 1 << 10);
        let keys = Digest::of(b"");
        let listed = |size| Listed { size, keys, format: STORE_FORMAT, packed: None };
        assert_eq!(listed(100).keys_max(&remote), 100);
        assert_eq!(listed(u64::MAX).keys_max(&remote), 1 << 10);
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_bytes_break_the_reading_of_a_manifest() {
        let (first, second) = ("ab".repeat(32), "0f".repeat(32));
        let seed = format!(
            "{first} 4096 {second} 2\n{second} 18446744073709551615 {first} 2 {first} 512\n"
        );
        let mut listed = 0;
        for bytes in changed(seed.as_bytes(), 20_000) {
            let Ok(text) = String::from_utf8(bytes) else { continue };
            let manifest = Manifest::parse(&text);
            for (pack, it) in &manifest.0 {
                // What a pack is fetched under and into is a name of digits and nothing else.
                let name = pack_file(pack);
                assert!(name.len() == 69 && !name[..64].contains(['/', '\\', '\0', '.']), "{name}");
                assert_eq!(keys_object(&it.keys).len(), 69);
                listed += 1;
            }
            let (fit, over) = fitting(manifest.0.into_iter().collect(), Some(1 << 20));
            assert!(
                fit.iter().map(|it| it.1.size).sum::<u64>() <= 1 << 20 && fit.len() + over <= 4
            );
        }
        assert!(listed > 1000);
    }

    /// Two machines and a remote between them: one signs, the other trusts its key.
    struct Signed {
        dir: PathBuf,
        signer: Remote,
        reader: Remote,
    }

    impl Signed {
        fn new(name: &str) -> Signed {
            let dir = scratch(name);
            let at = dir.join("remote");
            let signer =
                Remote::in_dir(&at, &dir.join("a"), Trust::of(Some(1), &[], false), 1 << 20);
            let reader = Remote::in_dir(&at, &dir.join("b"), Trust::of(None, &[1], false), 1 << 20);
            Signed { dir, signer, reader }
        }

        fn list(&self) -> Vec<u8> {
            std::fs::read(self.dir.join("remote/manifests/default")).unwrap()
        }

        fn put_back(&self, list: &[u8]) {
            put(&self.dir.join("remote"), "manifests/default", list);
        }

        async fn add(&self, line: &'static str) {
            let add = move |lines: &str| format!("{lines}{line}\n");
            self.signer.extend_list("manifests/default", add).await.unwrap();
        }

        async fn read(&self) -> anyhow::Result<String> {
            Ok(self.reader.read_list("manifests/default").await?.unwrap_or_default())
        }
    }

    #[test]
    fn a_list_that_was_put_back_is_not_taken() {
        let signed = Signed::new("put-back");
        wait(async {
            signed.add("one").await;
            let first = signed.list();
            assert_eq!(signed.read().await.unwrap(), "one\n");
            signed.add("two").await;
            let second = signed.list();
            assert_eq!(signed.read().await.unwrap(), "one\ntwo\n");
            // The remote, or whoever writes to it, puts the first list back.
            signed.put_back(&first);
            let refused = signed.read().await.unwrap_err().to_string();
            assert!(refused.contains("an older list is not taken"), "{refused}");
            assert_eq!(signed.reader.traffic.rejected(), 1);
            // And is taken at its word again once it has the list it had.
            signed.put_back(&second);
            assert_eq!(signed.read().await.unwrap(), "one\ntwo\n");
            // A machine that has taken nothing takes the old list: it has no way to know.
            let new = dir_reader(&signed, "c");
            signed.put_back(&first);
            assert_eq!(new.read_list("manifests/default").await.unwrap().unwrap(), "one\n");
            // Forgetting is how a machine is told to take whatever is there.
            assert!(signed.reader.forget_turns().unwrap());
            assert_eq!(signed.read().await.unwrap(), "one\n");
        });
        _ = std::fs::remove_dir_all(&signed.dir);
    }

    fn dir_reader(signed: &Signed, name: &str) -> Remote {
        let trust = Trust::of(None, &[1], false);
        Remote::in_dir(&signed.dir.join("remote"), &signed.dir.join(name), trust, 1 << 20)
    }

    #[test]
    fn a_signer_does_not_add_to_a_list_that_was_put_back() {
        let signed = Signed::new("put-back-under-a-signer");
        let turn = |list: &[u8]| -> u64 {
            let text = std::str::from_utf8(list).unwrap();
            text.lines().last().unwrap().split(' ').nth(2).unwrap().parse().unwrap()
        };
        wait(async {
            signed.add("one").await;
            let first = signed.list();
            signed.add("two").await;
            let second = signed.list();
            assert!(turn(&second) > turn(&first));
            signed.put_back(&first);
            signed.add("three").await;
            // What it signs now names what it adds and comes after all it signed before.
            let third = signed.list();
            assert!(turn(&third) > turn(&second), "{} after {}", turn(&third), turn(&second));
            assert_eq!(signed.read().await.unwrap(), "three\n");
            // A reader that had the second list takes the third and not the second again.
            signed.put_back(&second);
            assert!(signed.read().await.is_err());
        });
        _ = std::fs::remove_dir_all(&signed.dir);
    }

    #[test]
    fn a_memory_that_is_damaged_stops_a_push_and_a_pull() {
        let signed = Signed::new("damaged-memory");
        wait(async {
            signed.add("one").await;
            assert_eq!(signed.read().await.unwrap(), "one\n");
            let list = signed.list();
            for kept in ["a", "b"] {
                let turns = signed.dir.join(kept).join("turns");
                let of_remote = std::fs::read_dir(turns).unwrap().next().unwrap().unwrap().path();
                for file in std::fs::read_dir(of_remote).unwrap().flatten() {
                    if file.file_name() != "lock" {
                        std::fs::write(file.path(), "not a turn").unwrap();
                    }
                }
            }
            let add = |lines: &str| format!("{lines}two\n");
            let refused = signed.signer.extend_list("manifests/default", add).await.unwrap_err();
            assert!(refused.is::<Unremembered>(), "{refused}");
            assert!(signed.read().await.unwrap_err().is::<Unremembered>());
            assert_eq!(signed.list(), list, "the list is as it was");
        });
        _ = std::fs::remove_dir_all(&signed.dir);
    }
}
