//! The driver: does the IO the core ([`crate::decisions`]) decides, through a [`Net`] and a [`Git`]
//! the caller gives it (blaze-registry's HTTP client and git machinery, in
//! `blaze_registry::pinned`), and keeps what it gets in the store ([`crate::store`]).
//!
//! A download streams into a temporary file of the fetch's own under `scratch`, counted as it
//! comes against the fetcher's byte limit ([`Fetcher::with_most`]); once it is whole,
//! [`crate::store`] reads it back a chunk at a time, hashes it in the pin's algorithm and
//! BLAKE3, and only on a match puts it, chunk by chunk, as it reads it again. A mismatch, a
//! failure or a crash leaves at most the temporary file, which no reader of the store ever
//! sees, and which the guard removes on every return.

use std::{
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use blaze_cas::Handle;

use crate::{
    decisions::{Attempts, Decision, Failure, Policy},
    digest::{Blake3, Digest, Hashed},
    error::{FetchError, Producer, redact, scrub},
    flight::{Flights, Role, Waited},
    graph::{Tree, TreeFile},
    store::{Form, Made, Object, Objects, Origin, Produced, check_and_file, listing_of},
    tree::{self, Kind},
};

/// How many times a request of a pin waits for another's download and finds the store still
/// without it, before it gives up.
const ROUNDS: u32 = 3;

/// The most bytes a download may be unless its fetcher says otherwise ([`Fetcher::with_most`],
/// `blaze fetch-url`'s `most_bytes`): past it the transfer stops and nothing is kept.
pub const DEFAULT_MOST_BYTES: u64 = 4 << 30;

/// What asks servers for bytes.
pub trait Net: Send + Sync {
    /// `url`'s body from byte `from`, a piece at a time into `sink` with each piece's offset in
    /// the whole body, after its redirects; `Ok` once all of it went in. A server that ignores
    /// the range sends the whole body, whose pieces start at offset 0. Any other answer, or no
    /// answer, is a [`Failure`]: transient when another try may get the bytes. An error of
    /// `sink` stops the transfer and is a permanent failure.
    fn get(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        from: u64,
        // dyn: the driver's file and counter, once a try
        sink: &mut (dyn FnMut(u64, &[u8]) -> std::io::Result<()> + Send),
    ) -> Result<(), Failure>;
}

/// What checks out a git repository's commit.
pub trait Git: Send + Sync {
    /// `repo` checked out at the full commit id `rev`, under `scratch`, an empty directory of
    /// the fetch's own that is removed after: the checkout's root. Nothing is shared with
    /// another fetch or with the user's cargo home. Every `.git` (a submodule's is a file) is
    /// left out of the tree, as are the root's [`Git::markers`].
    fn checkout(&self, repo: &str, rev: &str, scratch: &Path) -> Result<PathBuf, String>;

    /// The files at a checkout's root that are the driver's, not the commit's (cargo's
    /// `.cargo-ok`).
    fn markers(&self) -> &[&str] {
        &[]
    }
}

/// What to get.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Source {
    /// The body of `url`, whose digest is `digest`.
    Url { url: String, digest: Digest },
    /// The tree of `repo` at `rev`, whose listing's digest ([`crate::tree`]) is `digest`.
    /// `rev` is a full commit id: it selects the commit and verifies nothing, as a SHA-1 can't.
    Git { repo: String, rev: String, digest: Digest },
}

impl Source {
    pub fn digest(&self) -> &Digest {
        match self {
            Source::Url { digest, .. } | Source::Git { digest, .. } => digest,
        }
    }

    /// What it gives: a file's bytes or a tree.
    pub fn form(&self) -> Form {
        match self {
            Source::Url { .. } => Form::Blob,
            Source::Git { .. } => Form::Tree,
        }
    }

    pub fn producer(&self) -> Producer {
        match self {
            Source::Url { url, .. } => Producer::Url(redact(url)),
            // clone: once a fetch, for its errors
            Source::Git { repo, rev, .. } => Producer::Git { repo: redact(repo), rev: rev.clone() },
        }
    }

    /// Its index record's origin.
    pub fn origin(&self) -> Origin<'_> {
        Origin::Pinned { pin: self.digest(), form: self.form() }
    }
}

/// Which store a fetcher keeps things in.
#[derive(Clone, Copy)]
pub enum StoreOf {
    /// The process's ([`Handle::installed`]), as of each fetch: the daemon makes its fetchers
    /// before it installs its store.
    Installed,
    Given(&'static Handle),
    /// None: [`Fetcher::download_verified`] only, for a process with no store (a client, a
    /// tool before the daemon); [`Fetcher::get`] is [`FetchError::NoStore`].
    Unkept,
}

/// A download whose bytes have the pin's digest: the only bytes [`Fetcher::download_verified`]
/// gives.
pub struct Verified {
    pub bytes: Vec<u8>,
    pub hashed: Hashed,
}

pub struct Fetcher {
    store: StoreOf,
    // dyn: the IO driver, chosen once a fetcher; each call is a network transfer.
    net: Arc<dyn Net>,
    // dyn: as `net`; each call is a checkout.
    git: Option<Arc<dyn Git>>,
    scratch: PathBuf,
    policy: Policy,
    /// How long a request waits for another's download of the same pin.
    wait_most: Duration,
    /// The most bytes a download may be.
    most: u64,
    flights: &'static Flights,
}

impl Fetcher {
    /// Keeps what it gets in `store`, downloads through `net`, and writes its temporary files
    /// under `scratch`.
    // dyn: as `Fetcher::net`
    pub fn new(store: StoreOf, net: Arc<dyn Net>, scratch: PathBuf) -> Fetcher {
        Fetcher {
            store,
            net,
            git: None,
            scratch,
            policy: Policy::default(),
            wait_most: Duration::from_secs(30 * 60),
            most: DEFAULT_MOST_BYTES,
            flights: Flights::process(),
        }
    }

    // dyn: as `Fetcher::git`
    pub fn with_git(self, git: Arc<dyn Git>) -> Fetcher {
        Fetcher { git: Some(git), ..self }
    }

    pub fn with_policy(self, policy: Policy) -> Fetcher {
        Fetcher { policy, ..self }
    }

    /// With a single-flight table of its own, for a test.
    pub fn with_flights(self, flights: &'static Flights) -> Fetcher {
        Fetcher { flights, ..self }
    }

    pub fn with_wait_most(self, wait_most: Duration) -> Fetcher {
        Fetcher { wait_most, ..self }
    }

    /// With downloads of at most `most` bytes in place of [`DEFAULT_MOST_BYTES`].
    pub fn with_most(self, most: u64) -> Fetcher {
        Fetcher { most, ..self }
    }

    fn handle(&self, from: impl FnOnce() -> Producer) -> Result<&'static Handle, FetchError> {
        match self.store {
            StoreOf::Given(store) => Ok(store),
            StoreOf::Installed => {
                Handle::installed().ok_or_else(|| FetchError::NoStore { from: from() })
            }
            StoreOf::Unkept => Err(FetchError::NoStore { from: from() }),
        }
    }

    /// The store's objects and index, as this fetcher keeps them.
    pub fn objects(&self) -> Result<Objects<'static>, FetchError> {
        self.handle(|| Producer::Given("a lookup".to_owned())).map(Objects::new) // alloc: error path
    }

    /// `source`'s object: the store's when its index has the pin, else fetched, checked and kept.
    pub fn get(&self, source: &Source) -> Result<Object, FetchError> {
        self.get_with(source, &[])
    }

    /// [`Fetcher::get`], asking with `headers` (a registry's token). They are no part of what is
    /// kept or looked up: the pin is.
    pub fn get_with(
        &self,
        source: &Source,
        headers: &[(&str, &str)],
    ) -> Result<Object, FetchError> {
        let objects = Objects::new(self.handle(|| source.producer())?);
        let pin = source.digest();
        let origin = source.origin();
        if let Some(object) = objects.lookup(origin)? {
            return Ok(object);
        }
        for _ in 0..ROUNDS {
            match self.flights.begin((objects.store_id(), origin.key()), *pin) {
                Role::Lead(_leader) => {
                    // One that led before this began may have kept it since the lookup above.
                    if let Some(object) = objects.lookup(origin)? {
                        return Ok(object);
                    }
                    return match source {
                        Source::Url { url, digest } => self.download(objects, url, digest, headers),
                        Source::Git { repo, rev, digest } => {
                            self.git_tree(objects, repo, rev, digest)
                        }
                    };
                }
                Role::Follow(follower) => match follower.wait(self.wait_most) {
                    Waited::TimedOut { waited } => {
                        return Err(FetchError::WaitedTooLong { pin: *pin, waited });
                    }
                    Waited::LeaderEnded => {
                        if let Some(object) = objects.lookup(origin)? {
                            return Ok(object);
                        }
                        // The leader failed: this request leads the next round itself.
                    }
                },
            }
        }
        Err(FetchError::NoLeader { pin: *pin, rounds: ROUNDS })
    }

    /// The bytes of `object`, checked against its BLAKE3 as they are read.
    pub fn bytes(&self, object: &Object) -> Result<Vec<u8>, FetchError> {
        self.objects()?.read(object)
    }

    fn download(
        &self,
        objects: Objects<'_>,
        url: &str,
        pin: &Digest,
        headers: &[(&str, &str)],
    ) -> Result<Object, FetchError> {
        let mut part = Part::new(&self.scratch, pin)?;
        self.download_to(&mut part, url, pin, headers)?;
        let from = Producer::Url(redact(url));
        // Checked, whether it matched or not, it is not resumed again.
        part.checked();
        check_and_file(objects, pin, Produced { made: Made::File(&part.path), from })
    }

    /// `url`'s body, downloaded, checked against `pin` and not kept: for a process with no store
    /// ([`StoreOf::Unkept`]). It is never single-flight (with no store, a follower would have
    /// nothing to read), and a mismatch gives no bytes.
    pub fn download_verified(
        &self,
        url: &str,
        pin: &Digest,
        headers: &[(&str, &str)],
    ) -> Result<Verified, FetchError> {
        let mut part = Part::new(&self.scratch, pin)?;
        self.download_to(&mut part, url, pin, headers)?;
        part.checked();
        // alloc: the download, whole, as the caller is given it; once a download
        let bytes = std::fs::read(&part.path).map_err(|error| FetchError::Io {
            what: format!("reading back {}", part.path.display()), // alloc: error path
            error,
        })?;
        let hashed = verify(Producer::Url(redact(url)), pin, &bytes)?;
        Ok(Verified { bytes, hashed })
    }

    /// `url`'s body into `part`, tried as the policy says, each try resuming from what `part`
    /// has (an earlier try's, or an earlier attempt's, even of another process: it is named by
    /// the pin); a body past the byte limit stops its transfer and is [`FetchError::TooBig`],
    /// not tried again. What is resumed is checked with the rest, against the pin, by the
    /// caller.
    fn download_to(
        &self,
        part: &mut Part,
        url: &str,
        pin: &Digest,
        headers: &[(&str, &str)],
    ) -> Result<(), FetchError> {
        let from = Producer::Url(redact(url));
        let mut attempts = Attempts::new(self.policy);
        let start = Instant::now();
        loop {
            attempts.advance_clock(start.elapsed());
            match attempts.next_decision() {
                Decision::Ask { attempt } => {
                    let mut written = part.resume()?;
                    let _span = tracing::info_span!(target: "blaze", "blaze-fetch download", url = %from, attempt, resumed_from = written).entered();
                    let mut too_big = false;
                    let most = self.most;
                    let asked = self.net.get(url, headers, written, &mut |offset: u64, bytes: &[u8]| {
                        if offset != written {
                            // The server sent the whole body, not the range from `written`:
                            // what was there is dropped and the body kept from its start.
                            if offset != 0 {
                                return Err(std::io::Error::other(format!(
                                    "a piece at byte {offset} came after byte {written}"
                                ))); // alloc: error path
                            }
                            part.restart()?;
                            written = 0;
                        }
                        written += bytes.len() as u64;
                        if written > most {
                            too_big = true;
                            return Err(std::io::Error::other(format!(
                                "the body is past the limit of {most} bytes"
                            ))); // alloc: error path
                        }
                        part.file.write_all(bytes)
                    });
                    if too_big {
                        part.checked();
                        return Err(FetchError::TooBig { from, most });
                    }
                    let asked = asked.and_then(|()| {
                        part.file.flush().map_err(|e| {
                            Failure::Permanent(format!("writing {}: {e}", part.path.display()))
                        }) // alloc: error path
                    });
                    match asked {
                        Ok(()) => return Ok(()),
                        Err(failure) => attempts.failed(scrubbed(failure)),
                    }
                }
                Decision::Wait { until } => {
                    let _span = tracing::info_span!(target: "blaze", "blaze-fetch waits to try again", url = %from).entered();
                    wait_out(until.saturating_sub(start.elapsed()));
                }
                Decision::GiveUp { tries, last } => {
                    return Err(FetchError::GaveUp { from, pin: *pin, tries, last });
                }
            }
        }
    }

    fn git_tree(
        &self,
        objects: Objects<'_>,
        repo: &str,
        rev: &str,
        pin: &Digest,
    ) -> Result<Object, FetchError> {
        let from = Producer::Git { repo: redact(repo), rev: rev.to_owned() }; // alloc: once a fetch
        let refused = |why: String| FetchError::GaveUp {
            from: from.clone(), // clone: error path
            pin: *pin,
            tries: 1,
            last: Failure::Permanent(scrub(&why)),
        };
        if !is_commit(rev) {
            return Err(refused(format!("`{rev}` is not a full commit id (40 or 64 hex digits)"))); // alloc: error path
        }
        let git = self.git.as_ref().ok_or_else(|| FetchError::NoGit { from: from.clone() })?; // clone: error path
        let scratch = Scratch::new(&self.scratch)?;
        let root = {
            let _span =
                tracing::info_span!(target: "blaze", "blaze-fetch git checkout", repo = %from)
                    .entered();
            git.checkout(repo, rev, &scratch.0).map_err(refused)?
        };
        // The byte limit covers a checkout as it covers a download: the scratch directory whole
        // (the clone's database with it), then the tree as it is read, before any file is.
        let held = dir_len(&scratch.0)?;
        if held > self.most {
            return Err(FetchError::TooBig { from, most: self.most });
        }
        let tree = Tree { files: read_tree(&root, git.markers(), self.most, &from)? };
        check_and_file(objects, pin, Produced { made: Made::Tree(&tree), from })
    }

    /// `object` as a file (a blob) or a directory (a tree) named by its BLAKE3 under `dir`,
    /// written once, read-only, renamed into place whole; one already there is checked against
    /// the BLAKE3 before it is given, and written again if it is not it. Each file is written
    /// from the store a chunk at a time, checked against its BLAKE3 as it goes.
    pub fn materialize(&self, object: &Object, dir: &Path) -> Result<PathBuf, FetchError> {
        let objects = self.objects()?;
        let io = |what: String| move |error: std::io::Error| FetchError::Io { what, error };
        std::fs::create_dir_all(dir).map_err(io(format!("creating {}", dir.display())))?; // alloc: error path
        let at = dir.join(object.blake3.hex());
        if at.exists() {
            if materialized(object, &at)? {
                return Ok(at);
            }
            remove_any(&at).map_err(io(format!(
                "removing {}, which is not {}",
                at.display(),
                object.blake3
            )))?; // alloc: error path
        }
        let part =
            dir.join(format!(".{}{PART}{}-{}", object.blake3.hex(), std::process::id(), next())); // alloc: once a materialize
        let written = match object.form {
            Form::Blob => write_object(objects, object.blake3, &part, Kind::File).and_then(|len| {
                if len == object.len {
                    Ok(())
                } else {
                    Err(FetchError::Length {
                        object: object.blake3,
                        expected: object.len,
                        actual: len,
                    })
                }
            }),
            Form::Tree => {
                let listing = objects.read(object)?;
                let entries = tree::parse(&listing).map_err(|refused| FetchError::Tree {
                    from: Producer::Given(object.blake3.to_string()), // alloc: error path
                    refused,
                })?;
                std::fs::create_dir(&part).map_err(io(format!("creating {}", part.display())))?; // alloc: error path
                entries.iter().try_for_each(|entry| {
                    let at = directories_under(&part, &entry.path)?;
                    let len = write_object(objects, entry.blake3, &at, entry.kind)?;
                    if len == entry.len {
                        Ok(())
                    } else {
                        Err(FetchError::Length {
                            object: entry.blake3,
                            expected: entry.len,
                            actual: len,
                        })
                    }
                })
            }
        };
        let placed = written.and_then(|()| match std::fs::rename(&part, &at) {
            Ok(()) => Ok(()),
            // Another materialize of it got there first: its copy is checked as any other.
            Err(_) if at.exists() && materialized(object, &at)? => Ok(()),
            Err(error) => Err(FetchError::Io {
                what: format!("renaming {} to {}", part.display(), at.display()),
                error,
            }), // alloc: error path
        });
        if placed.is_err() {
            _ = remove_any(&part);
        }
        placed.map(|()| at)
    }
}

/// `bytes` checked against `pin`: their hashes, or the mismatch naming `from` and both
/// digests. For bytes a caller already has and does not keep (a `file://` registry, a
/// download with no store); [`Objects::keep_pinned`] keeps them too.
pub fn verify(from: Producer, pin: &Digest, bytes: &[u8]) -> Result<Hashed, FetchError> {
    let hashed = Hashed::of(pin.algo(), bytes);
    match crate::decisions::verdict(pin, &hashed) {
        crate::decisions::Verdict::Matches => Ok(hashed),
        crate::decisions::Verdict::Mismatch { expected, actual } => {
            Err(FetchError::Mismatch { from, expected, actual })
        }
    }
}

/// Whether `rev` is a full git commit id: SHA-1's 40 hex digits or SHA-256's 64.
pub fn is_commit(rev: &str) -> bool {
    matches!(rev.len(), 40 | 64) && rev.bytes().all(|it| it.is_ascii_hexdigit())
}

/// Whether what is at `at` is `object`, by its BLAKE3.
fn materialized(object: &Object, at: &Path) -> Result<bool, FetchError> {
    match object.form {
        Form::Blob => Ok(hash_file(at)? == object.blake3),
        Form::Tree => {
            let from = Producer::Given(at.display().to_string()); // alloc: once a check of a materialized tree
            let tree = Tree { files: read_tree(at, &[], u64::MAX, &from)? };
            Ok(listing_of(&tree, &from).is_ok_and(|listing| Blake3::of(&listing) == object.blake3))
        }
    }
}

/// The BLAKE3 of the file at `at`, read a piece at a time.
fn hash_file(at: &Path) -> Result<Blake3, FetchError> {
    let io =
        |error: std::io::Error| FetchError::Io { what: format!("reading {}", at.display()), error }; // alloc: error path
    let mut file = std::fs::File::open(at).map_err(io)?;
    let mut hashing = blaze_ids::b3::Hasher::new();
    let mut buffer = vec![0; 64 << 10]; // alloc: one buffer, once a check of a materialized file
    loop {
        match file.read(&mut buffer) {
            Ok(0) => return Ok(Blake3(hashing.finalize())),
            Ok(n) => {
                hashing.update(&buffer[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io(e)),
        }
    }
}

/// What a temporary name has after the object's hex and before the process and a count.
const PART: &str = ".part-";

fn next() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A download's partial file, named by its pin (`.part-pin-<algo>-<hex>`) so that any later
/// try, attempt or process resumes it: kept when the download fails, removed once it was
/// checked against the pin ([`Part::checked`]), matched or not, so a partial that was wrong is
/// downloaded again from nothing. It is held locked from [`Part::new`] until it drops. A
/// second download of the same pin at once (another process: one process downloads a pin
/// once, [`crate::flight`]) gets a name of its own, as every download had before, removed
/// whatever happens.
struct Part {
    path: PathBuf,
    file: stdx::file_lock::Locked,
    /// Whether the name is this download's alone, never resumed.
    own: bool,
    checked: std::cell::Cell<bool>,
}

impl Part {
    fn new(scratch: &Path, pin: &Digest) -> Result<Part, FetchError> {
        std::fs::create_dir_all(scratch).map_err(|error| FetchError::Io {
            what: format!("creating {}", scratch.display()),
            error,
        })?; // alloc: error path
        let path = scratch.join(format!("{PART}pin-{}-{}", pin.algo().name(), pin.hex())); // alloc: once a download
        let io = |path: &Path, error| FetchError::Io { what: format!("opening {}", path.display()), error }; // alloc: error path
        // A symlink or directory at the name is refused, never written through (the scratch
        // directory is blaze's own; this is the check `create_new` made before).
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if !meta.is_file() => {
                return Err(io(&path, std::io::Error::other("it is not a plain file")));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(&path, error)),
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| io(&path, e))?;
        match stdx::file_lock::Locked::try_lock(file).map_err(|e| io(&path, e))? {
            Ok(file) => Ok(Part { path, file, own: false, checked: std::cell::Cell::new(false) }),
            Err(_held) => {
                let own = scratch.join(format!("{PART}{}-{}", std::process::id(), next())); // alloc: once such a download
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&own)
                    .map_err(|e| io(&own, e))?;
                let file = stdx::file_lock::Locked::lock(file).map_err(|e| io(&own, e))?;
                Ok(Part { path: own, file, own: true, checked: std::cell::Cell::new(false) })
            }
        }
    }

    /// Where a try resumes: the file's length, its end sought.
    fn resume(&mut self) -> Result<u64, FetchError> {
        let io = |error| FetchError::Io { what: format!("reading {}", self.path.display()), error }; // alloc: error path
        std::io::Seek::seek(&mut *self.file, std::io::SeekFrom::End(0)).map_err(io)
    }

    /// Empty again, written from its start: the server sent the whole body.
    fn restart(&mut self) -> std::io::Result<()> {
        self.file.set_len(0)?;
        std::io::Seek::seek(&mut *self.file, std::io::SeekFrom::Start(0)).map(drop)
    }

    /// It was checked against its pin: it goes when this drops.
    fn checked(&self) {
        self.checked.set(true);
    }
}

impl Drop for Part {
    fn drop(&mut self) {
        if self.own || self.checked.get() {
            _ = std::fs::remove_file(&self.path);
        }
    }
}

/// A checkout's directory of its own, removed when it drops.
struct Scratch(PathBuf);

impl Scratch {
    fn new(under: &Path) -> Result<Scratch, FetchError> {
        let dir = under.join(format!("{PART}git-{}-{}", std::process::id(), next())); // alloc: once a checkout
        std::fs::create_dir_all(&dir).map_err(|error| FetchError::Io {
            what: format!("creating {}", dir.display()),
            error,
        })?; // alloc: error path
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = remove_any(&self.0) {
            tracing::warn!(dir = %self.0.display(), %error, "a git fetch's scratch directory was not removed");
        }
    }
}

fn remove_any(at: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(at) {
        Ok(it) if it.is_dir() => std::fs::remove_dir_all(at),
        Ok(_) => std::fs::remove_file(at),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Every file and symlink under `root`, by its `/`-separated path, none in a `.git` and none
/// of `markers` at the root; past `most` bytes of files, [`FetchError::TooBig`] naming `from`,
/// before the file that passes it is read.
fn read_tree(
    root: &Path,
    markers: &[&str],
    most: u64,
    from: &Producer,
) -> Result<Vec<TreeFile>, FetchError> {
    let mut total = 0u64;
    let io = |at: &Path| {
        let what = format!("reading {}", at.display()); // alloc: error path
        move |error: std::io::Error| FetchError::Io { what, error }
    };
    let mut files = Vec::new(); // alloc: once a tree read
    let mut dirs = vec![(root.to_owned(), String::new())]; // alloc: once a tree read
    while let Some((dir, under)) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).map_err(io(&dir))? {
            let entry = entry.map_err(io(&dir))?;
            let at = entry.path();
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(FetchError::Io {
                    what: at.display().to_string(),
                    error: std::io::Error::other("its name is not UTF-8"),
                }); // alloc: error path
            };
            if name.eq_ignore_ascii_case(".git") || (under.is_empty() && markers.contains(&name)) {
                continue;
            }
            let path = if under.is_empty() { name.to_owned() } else { format!("{under}/{name}") }; // alloc: once a file
            let kind = entry.file_type().map_err(io(&at))?;
            if kind.is_dir() {
                dirs.push((at, path));
            } else if kind.is_symlink() {
                let target = std::fs::read_link(&at).map_err(io(&at))?;
                let Some(target) = target.to_str() else {
                    return Err(FetchError::Io {
                        what: at.display().to_string(),
                        error: std::io::Error::other("it points to a path that is not UTF-8"),
                    }); // alloc: error path
                };
                files.push(TreeFile {
                    path,
                    kind: Kind::Link,
                    bytes: target.replace('\\', "/").into_bytes(),
                }); // alloc: a link's target, once a link
            } else if kind.is_file() {
                let metadata = entry.metadata().map_err(io(&at))?;
                total = total.saturating_add(metadata.len());
                if total > most {
                    return Err(FetchError::TooBig { from: from.clone(), most }); // clone: error path
                }
                let bytes = std::fs::read(&at).map_err(io(&at))?; // alloc: once a file
                let kind = if is_executable(&metadata) { Kind::Exec } else { Kind::File };
                files.push(TreeFile { path, kind, bytes });
            } else {
                return Err(FetchError::Io {
                    what: at.display().to_string(),
                    error: std::io::Error::other("it is neither a file, a directory nor a symlink"),
                }); // alloc: error path
            }
        }
    }
    Ok(files)
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::Metadata) -> bool {
    false
}

/// `root` joined with `path`, each directory between them made, or checked to be one with
/// [`std::fs::symlink_metadata`]: a symlink or a file where a directory goes (a listing's
/// `A` link and `a/b` on a case-insensitive disk) is refused, never written through.
pub(crate) fn directories_under(root: &Path, path: &str) -> Result<PathBuf, FetchError> {
    let mut at = root.to_owned(); // alloc: once a file written out
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        at.push(part);
        if parts.peek().is_none() {
            break;
        }
        let io = |error: std::io::Error| FetchError::Io {
            what: format!("making the directory {}", at.display()),
            error,
        }; // alloc: error path
        match std::fs::symlink_metadata(&at) {
            Ok(it) if it.is_dir() => {}
            Ok(_) => {
                return Err(io(std::io::Error::other(
                    "something not a directory is in its place, and nothing is written through it",
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&at).map_err(io)?
            }
            Err(e) => return Err(io(e)),
        }
    }
    Ok(at)
}

/// The total length of every file under `dir`, symlinks not followed.
fn dir_len(dir: &Path) -> Result<u64, FetchError> {
    let mut total = 0u64;
    let mut dirs = vec![dir.to_owned()]; // alloc: once a checkout measured
    while let Some(dir) = dirs.pop() {
        let io = |error: std::io::Error| FetchError::Io {
            what: format!("measuring {}", dir.display()),
            error,
        }; // alloc: error path
        for entry in std::fs::read_dir(&dir).map_err(io)? {
            let entry = entry.map_err(io)?;
            let kind = entry.file_type().map_err(io)?;
            if kind.is_dir() {
                dirs.push(entry.path());
            } else {
                total = total.saturating_add(entry.metadata().map_err(io)?.len());
            }
        }
    }
    Ok(total)
}

/// The object filed under `blake3` written as a new file of `kind` at `at`, whose directory is
/// there, read-only, a chunk at a time from the store: its length. Nothing is there before it
/// (`create_new`, which follows no symlink), so nothing is written through one.
fn write_object(
    objects: Objects<'_>,
    blake3: Blake3,
    at: &Path,
    kind: Kind,
) -> Result<u64, FetchError> {
    let io =
        |error: std::io::Error| FetchError::Io { what: format!("writing {}", at.display()), error }; // alloc: error path
    match kind {
        Kind::Link => {
            let mut target = Vec::new(); // alloc: a link's target, once a link
            let len = objects.read_into(blake3, &mut |piece| {
                target.extend_from_slice(piece);
                Ok(())
            })?;
            let to = std::str::from_utf8(&target)
                .map_err(|_| io(std::io::Error::other("the link's target is not UTF-8")))?;
            #[cfg(unix)]
            return std::os::unix::fs::symlink(to, at).map(|()| len).map_err(io);
            #[cfg(not(unix))]
            return Err(io(std::io::Error::other(format!(
                "it is a symlink to {to} ({len} bytes), and this platform has none to write"
            )))); // alloc: error path
        }
        Kind::File | Kind::Exec => {
            let mut file =
                std::fs::OpenOptions::new().write(true).create_new(true).open(at).map_err(io)?;
            let len = objects.read_into(blake3, &mut |piece| file.write_all(piece).map_err(io))?;
            file.flush().map_err(io)?;
            let mut permissions = file.metadata().map_err(io)?.permissions();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                permissions.set_mode(if kind == Kind::Exec { 0o555 } else { 0o444 });
            }
            #[cfg(not(unix))]
            permissions.set_readonly(true);
            file.set_permissions(permissions).map_err(io)?;
            Ok(len)
        }
    }
}

/// `failure` with every URL in its message redacted ([`scrub`]): a driver's text is never
/// trusted to have done it.
fn scrubbed(failure: Failure) -> Failure {
    match failure {
        Failure::Transient(said) => Failure::Transient(scrub(&said)),
        Failure::Permanent(said) => Failure::Permanent(scrub(&said)),
    }
}

/// Holds this thread for a wait the core decided: the driver's one timer.
#[allow(
    clippy::disallowed_methods,
    reason = "the driver's timer: `Attempts` decides the wait, from the clock it is given"
)]
fn wait_out(wait: Duration) {
    std::thread::sleep(wait);
}
