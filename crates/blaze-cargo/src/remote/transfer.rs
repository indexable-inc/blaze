//! Moving one object between the remote and a file, so that the file is either all of what
//! its name says or not there.

use std::{
    fmt,
    fs::TryLockError,
    io::SeekFrom,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use anyhow::{Context, bail};
use aws_sdk_s3::primitives::ByteStream;
use blaze_store::name_file_whole;
use futures_util::{StreamExt, stream};
use sha2::{Digest as _, Sha256};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::{Semaphore, SemaphorePermit},
};

/// How often a transfer that broke off is taken up again where it stopped.
const RESUMES: u32 = 6;
/// The wait before the first of those; each one after waits twice as long.
const FIRST_WAIT: Duration = Duration::from_millis(250);
const CHUNK: usize = 1 << 16;

/// The SHA-256 of an object, which is its name on the remote.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Digest([u8; 32]);

impl Digest {
    pub fn of(bytes: &[u8]) -> Digest {
        Digest(Sha256::digest(bytes).into())
    }

    /// The digest written as 64 hex digits, and nothing else: a name that parses is safe
    /// as the name of a file.
    pub fn parse(hex: &str) -> Option<Digest> {
        let mut digest = [0; 32];
        if hex.len() != 64 || !hex.is_ascii() {
            return None;
        }
        for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks(2)) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        }
        Some(Digest(digest))
    }

    pub fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for Digest {
    fn from(bytes: [u8; 32]) -> Digest {
        Digest(bytes)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// What a small object was when it was read, for a write that must not undo another's.
pub struct Version(pub Option<String>);

/// The line to the remote that the transfers of a process share: how many requests are on
/// it at a time, and what went over it.
pub struct Traffic {
    gate: Semaphore,
    jobs: usize,
    /// Set when the remote stopped answering, so that what is still waiting does not each
    /// find that out for itself, with how it stopped.
    down: AtomicBool,
    why_down: Mutex<String>,
    /// Calls of the service, and the attempts they took.
    pub operations: AtomicU64,
    pub requests: AtomicU64,
    /// Transfers taken up again after they broke off.
    pub resumed: AtomicU64,
    pub received: AtomicU64,
    pub sent: AtomicU64,
    /// Objects that were not what their name says.
    pub rejected: AtomicU64,
    /// Objects that came or went whole.
    pub moved: AtomicU64,
}

impl Traffic {
    pub fn new(jobs: usize) -> Traffic {
        let jobs = jobs.max(1);
        Traffic {
            gate: Semaphore::new(jobs),
            jobs,
            down: AtomicBool::new(false),
            why_down: Mutex::default(),
            operations: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            resumed: AtomicU64::new(0),
            received: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            moved: AtomicU64::new(0),
        }
    }

    /// One line of what a push or pull moved and how fast: the objects, the bytes that went
    /// the way of `verb`, the seconds and MiB/s.
    pub fn summary(&self, verb: &str, took: Duration) -> String {
        let count = |it: &AtomicU64| it.load(Ordering::Relaxed);
        let bytes = match verb {
            "push" => count(&self.sent),
            _ => count(&self.received),
        };
        let seconds = took.as_secs_f64();
        let rate = bytes as f64 / (1 << 20) as f64 / seconds.max(1e-3);
        format!(
            "{verb}: {} objects, {bytes} bytes, {seconds:.2} s, {rate:.1} MiB/s",
            count(&self.moved)
        )
    }

    pub fn jobs(&self) -> usize {
        self.jobs
    }

    /// A place on the line, held for as long as a request and its body take.
    pub async fn permit(&self) -> anyhow::Result<SemaphorePermit<'_>> {
        let permit = self.gate.acquire().await?;
        if self.down.load(Ordering::Relaxed) {
            let why = self.why_down.lock().map(|it| it.clone()).unwrap_or_default();
            bail!("the remote stopped answering: {why}");
        }
        Ok(permit)
    }

    /// Takes the remote to be down, for the reason `error` gives.
    fn went_down(&self, error: &anyhow::Error) {
        if let Ok(mut why) = self.why_down.lock() {
            *why = format!("{error:#}").lines().next().unwrap_or_default().to_owned();
        }
        self.down.store(true, Ordering::Relaxed);
    }

    /// Has the remote tried again after it was taken to be down.
    pub fn try_again(&self) {
        self.down.store(false, Ordering::Relaxed);
    }

    pub fn rejected(&self) -> u64 {
        self.rejected.load(Ordering::Relaxed)
    }
}

impl fmt::Display for Traffic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = |it: &AtomicU64| it.load(Ordering::Relaxed);
        let (operations, requests) = (count(&self.operations), count(&self.requests));
        write!(
            f,
            "{} requests ({} tried again, {} transfers resumed), {} bytes received, {} sent, {} objects rejected",
            requests.max(operations),
            requests.saturating_sub(operations),
            count(&self.resumed),
            count(&self.received),
            count(&self.sent),
            count(&self.rejected),
        )
    }
}

/// How much may still be written of what a remote sends, when what it sends does not say.
pub struct Room {
    left: Option<AtomicU64>,
    of: u64,
}

impl Room {
    /// Room for `most` bytes, or for any number of them.
    pub fn of(most: Option<u64>) -> Room {
        Room { left: most.map(AtomicU64::new), of: most.unwrap_or(u64::MAX) }
    }

    /// Takes room for `bytes` that are about to be written: an error when there is not
    /// that much left, before any of them is.
    pub fn take(&self, bytes: u64) -> anyhow::Result<()> {
        let Some(left) = &self.left else { return Ok(()) };
        let took =
            left.try_update(Ordering::Relaxed, Ordering::Relaxed, |left| left.checked_sub(bytes));
        match took {
            Ok(_) => Ok(()),
            Err(_) => {
                bail!("the remote holds more than the {} bytes that are taken from it", self.of)
            }
        }
    }
}

/// An object that is longer than what is taken of one.
#[derive(Debug)]
pub struct TooLong {
    pub name: String,
    pub most: u64,
}

impl fmt::Display for TooLong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "rejected {}: it is longer than the {} bytes that are taken of it",
            self.name, self.most
        )
    }
}

impl std::error::Error for TooLong {}

/// The bytes of an object from some offset on, as they arrive.
pub enum Body {
    File(File),
    S3(ByteStream),
    Http(hyper::body::Incoming),
    /// An object with nothing left from where it was read.
    Empty,
}

impl Body {
    pub async fn next(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        match self {
            Body::File(file) => {
                let mut chunk = vec![0; CHUNK];
                let read = file.read(&mut chunk).await?;
                chunk.truncate(read);
                Ok((read != 0).then_some(chunk))
            }
            Body::S3(stream) => Ok(stream.try_next().await?.map(|it| it.to_vec())),
            Body::Http(body) => super::https::next(body).await,
            Body::Empty => Ok(None),
        }
    }
}

/// A failure that waiting does not cure: the service said no, a list names what is not
/// there, an object is longer than is taken, or a file of this machine could not be written.
pub fn is_final(error: &anyhow::Error) -> bool {
    error.is::<Refused>()
        || error.is::<Missing>()
        || error.is::<TooLong>()
        || error.is::<std::io::Error>()
}

impl super::Remote {
    /// Makes a request of the remote until it is answered, with the waits a broken transfer
    /// is taken up after, for as long as waiting may cure what went wrong.
    pub async fn patiently<T, F>(&self, mut request: impl FnMut() -> F) -> anyhow::Result<T>
    where
        F: Future<Output = anyhow::Result<T>>,
    {
        let mut again = 0;
        loop {
            match request().await {
                Ok(answer) => return Ok(answer),
                Err(e) if is_final(&e) => return Err(e),
                Err(e) if again == RESUMES => {
                    self.traffic.went_down(&e);
                    return Err(e.context(format!("after asking {again} times more")));
                }
                Err(_) => {
                    tokio::time::sleep(FIRST_WAIT * 2u32.pow(again)).await;
                    again += 1;
                    self.traffic.resumed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

/// How a fetch ended, when it did not fail.
pub enum Fetched {
    /// The file is there, and this many bytes came over the line for it.
    Whole(u64),
    /// Another process is fetching the same file.
    Elsewhere,
}

/// How many of the first bytes of an object are kept to tell what it is.
const START: usize = 8;

/// The file an object is fetched into before it has its name: `NAME.fetch.tmp`, which in a
/// store is a temporary file of the store's, locked by the process that writes it. One left
/// by a process that was stopped is taken up where it ends.
struct Partial {
    path: PathBuf,
    file: File,
    hash: Sha256,
    len: u64,
    start: Vec<u8>,
}

impl Partial {
    /// `None` when another process holds it. A file that arrives compressed is kept aside
    /// under another name than one that arrives as it is, so neither is taken for the other.
    async fn open(to: &Path, packed: bool) -> anyhow::Result<Option<Partial>> {
        let mut path = to.as_os_str().to_owned();
        path.push(if packed { ".zst.tmp" } else { ".fetch.tmp" });
        let path = PathBuf::from(path);
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        // What is fetched into a store is the store's, which no other user reads.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options.open(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        let file = File::from_std(file);
        let mut partial = Partial { path, file, hash: Sha256::new(), len: 0, start: Vec::new() };
        let mut chunk = vec![0; CHUNK];
        loop {
            let read = partial.file.read(&mut chunk).await?;
            if read == 0 {
                break;
            }
            partial.count(&chunk[..read]);
        }
        Ok(Some(partial))
    }

    fn count(&mut self, chunk: &[u8]) {
        let wanted = START.saturating_sub(self.start.len()).min(chunk.len());
        self.start.extend_from_slice(&chunk[..wanted]);
        self.hash.update(chunk);
        self.len += chunk.len() as u64;
    }

    async fn append(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        self.file.write_all(chunk).await?;
        self.count(chunk);
        Ok(())
    }

    async fn empty(&mut self) -> anyhow::Result<()> {
        self.file.set_len(0).await?;
        self.file.seek(SeekFrom::Start(0)).await?;
        (self.hash, self.len) = (Sha256::new(), 0);
        self.start.clear();
        Ok(())
    }

    fn digest(&self) -> Digest {
        Digest(self.hash.clone().finalize().into())
    }

    /// Gives the file its name, which is when anything else starts to read it. A file that
    /// cannot be given it is removed: the last of its bytes may not have been written.
    async fn seal(mut self, to: &Path) -> anyhow::Result<()> {
        let flushed = self.file.flush().await;
        let (file, from, to) = (self.file.into_std().await, self.path, to.to_owned());
        let named = tokio::task::spawn_blocking(move || {
            let named = flushed.and_then(|()| name_file_whole(&file, &from, &to));
            if named.is_err() {
                _ = std::fs::remove_file(&from);
            }
            named
        });
        Ok(named.await??)
    }

    async fn discard(self) {
        _ = tokio::fs::remove_file(&self.path).await;
    }

    /// Unpacks the file, which is all there, into `to` if what comes out hashes to `want`
    /// and starts as `is_wanted` says, and removes it. `false` is what came out being
    /// something else.
    async fn unpack(
        mut self,
        to: &Path,
        (want, most): (Digest, u64),
        is_wanted: fn(&[u8]) -> bool,
    ) -> anyhow::Result<bool> {
        // The last write is still on its way to the file until this: tokio writes behind.
        let flushed = self.file.flush().await;
        let (from, to) = (self.path, to.to_owned());
        drop(self.file);
        if let Err(e) = flushed {
            _ = tokio::fs::remove_file(&from).await;
            return Err(e.into());
        }
        let unpacked = tokio::task::spawn_blocking(move || {
            let unpacked = unpack_whole(&from, &to, (want, most), is_wanted);
            _ = std::fs::remove_file(&from);
            unpacked
        });
        Ok(unpacked.await??)
    }
}

/// Writes what the zstd file `from` holds to `to`, whole or not at all, when it is at most
/// `most` bytes: a file is small for what it unpacks to, and no more than that many bytes
/// of what comes out are ever on the disk.
fn unpack_whole(
    from: &Path,
    to: &Path,
    (want, most): (Digest, u64),
    is_wanted: fn(&[u8]) -> bool,
) -> std::io::Result<bool> {
    use std::io::{Read, Write};
    let mut aside = to.as_os_str().to_owned();
    aside.push(format!(".{}.tmp", std::process::id()));
    let aside = PathBuf::from(aside);
    let unpack = || {
        let mut packed = zstd::Decoder::new(std::fs::File::open(from)?)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        // What is fetched into a store is the store's, which no other user reads.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&aside)?;
        let (mut hash, mut start, mut chunk) = (Sha256::new(), Vec::new(), vec![0; CHUNK]);
        let mut left = most;
        loop {
            let read = packed.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            let Some(rest) = left.checked_sub(read as u64) else { return Ok(false) };
            left = rest;
            let wanted = START.saturating_sub(start.len()).min(read);
            start.extend_from_slice(&chunk[..wanted]);
            hash.update(&chunk[..read]);
            file.write_all(&chunk[..read])?;
        }
        if Digest(hash.finalize().into()) != want || !is_wanted(&start) {
            return Ok(false);
        }
        name_file_whole(&file, &aside, to).map(|()| true)
    };
    let unpacked = unpack();
    if !matches!(unpacked, Ok(true)) {
        _ = std::fs::remove_file(&aside);
    }
    // Bytes that are not zstd are a remote that lies, as bytes of another hash are.
    match unpacked {
        Err(e)
            if e.kind() == std::io::ErrorKind::Other
                || e.kind() == std::io::ErrorKind::InvalidData =>
        {
            Ok(false)
        }
        other => other,
    }
}

impl super::Remote {
    /// Fetches the object `name` into the file `to` if its bytes hash to `want`, start as
    /// `is_wanted` says the file has to and are at most `most`. An object that is longer is
    /// counted as rejected, and no more of it than `most` bytes is ever on the disk.
    ///
    /// A transfer that breaks off is taken up from the byte it stopped at, a few times with
    /// a growing wait in between, and by the next process after this one is stopped. Bytes
    /// that are not what was asked for are thrown away and the object is counted as
    /// rejected. A failure that waiting does not cure leaves nothing behind.
    pub async fn fetch_file(
        &self,
        name: &str,
        want: &Digest,
        to: &Path,
        most: u64,
        is_wanted: fn(&[u8]) -> bool,
    ) -> anyhow::Result<Fetched> {
        self.fetch_coded(name, want, None, to, most, is_wanted).await
    }

    /// As [`Self::fetch_file`], for an object that may be the file compressed with zstd:
    /// `unpacked` is then the hash of the file and the most it may unpack to, and `want`
    /// and `most` are those of the object. The object is checked before it is unpacked and
    /// the file after, so the name of a file vouches for it however it travelled, and
    /// neither is longer than it was listed as.
    pub async fn fetch_coded(
        &self,
        name: &str,
        want: &Digest,
        unpacked: Option<(Digest, u64)>,
        to: &Path,
        most: u64,
        is_wanted: fn(&[u8]) -> bool,
    ) -> anyhow::Result<Fetched> {
        let _permit = self.traffic.permit().await?;
        let Some(mut partial) = Partial::open(to, unpacked.is_some()).await? else {
            return Ok(Fetched::Elsewhere);
        };
        // What a stopped process left is not the start of this object.
        if partial.len > most {
            partial.empty().await?;
        }
        let mut left_by_another = partial.len != 0;
        let mut received = 0;
        let mut resumes = 0;
        loop {
            let had = partial.len;
            let appended = self.append_rest(name, &mut partial, most, &mut received).await;
            // A transfer that got further is not the same failure again.
            if partial.len > had {
                resumes = 0;
            }
            match appended {
                Ok(()) => {}
                Err(e) if is_final(&e) => {
                    partial.discard().await;
                    if e.is::<TooLong>() {
                        self.traffic.rejected.fetch_add(1, Ordering::Relaxed);
                    }
                    return Err(e.context(format!("fetching {name}")));
                }
                Err(e) if resumes == RESUMES => {
                    self.traffic.went_down(&e);
                    return Err(e.context(format!("fetching {name}, after {resumes} resumes")));
                }
                Err(_) => {
                    tokio::time::sleep(FIRST_WAIT * 2u32.pow(resumes)).await;
                    resumes += 1;
                    self.traffic.resumed.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
            let got = partial.digest();
            if got == *want {
                let whole = match unpacked {
                    Some(file) => partial.unpack(to, file, is_wanted).await,
                    None if is_wanted(&partial.start) => partial.seal(to).await.map(|()| true),
                    None => {
                        partial.discard().await;
                        Ok(false)
                    }
                };
                if whole.with_context(|| format!("fetching {name}"))? {
                    self.traffic.moved.fetch_add(1, Ordering::Relaxed);
                    return Ok(Fetched::Whole(received));
                }
                self.traffic.rejected.fetch_add(1, Ordering::Relaxed);
                bail!("rejected {name}: it is not what it is listed as");
            }
            // What a stopped process left may be the start of something else.
            if std::mem::take(&mut left_by_another) {
                partial.empty().await?;
                continue;
            }
            let len = partial.len;
            partial.discard().await;
            self.traffic.rejected.fetch_add(1, Ordering::Relaxed);
            bail!("rejected {name}: its {len} bytes hash to {got}, not to {want}");
        }
    }

    /// Appends what the object has after the bytes of `partial`, to `most` bytes in all.
    async fn append_rest(
        &self,
        name: &str,
        partial: &mut Partial,
        most: u64,
        received: &mut u64,
    ) -> anyhow::Result<()> {
        if let super::Place::Gateway(gateway) = &self.place
            && most.saturating_sub(partial.len) >= RANGED_AT
        {
            return self.append_ranges(gateway, name, partial, most, received).await;
        }
        let Some(mut body) = self.read_from(name, partial.len).await? else {
            return Err(Missing(name.to_owned()).into());
        };
        loop {
            let chunk = tokio::time::timeout(self.patience, body.next()).await;
            let chunk = chunk.with_context(|| format!("no bytes for {:?}", self.patience))??;
            let Some(chunk) = chunk else { return Ok(()) };
            self.traffic.received.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            *received += chunk.len() as u64;
            if partial.len + chunk.len() as u64 > most {
                return Err(TooLong { name: name.to_owned(), most }.into());
            }
            partial.append(&chunk).await?;
        }
    }

    /// As [`Self::append_rest`], in ranges of [`RANGE`] bytes asked for `BLAZE_REMOTE_JOBS`
    /// at a time and appended in order: one stream through the gateway is slower than the
    /// line. The first range says how long the object is.
    async fn append_ranges(
        &self,
        gateway: &super::gateway::Gateway,
        name: &str,
        partial: &mut Partial,
        most: u64,
        received: &mut u64,
    ) -> anyhow::Result<()> {
        let from = partial.len;
        let first = gateway.range(name, from, from + RANGE - 1).await?;
        let Some((bytes, total)) = first else { return Err(Missing(name.to_owned()).into()) };
        if total > most {
            return Err(TooLong { name: name.to_owned(), most }.into());
        }
        *received += bytes.len() as u64;
        partial.append(&bytes).await?;
        let starts = (partial.len..total).step_by(RANGE as usize);
        let mut ranges = stream::iter(starts.map(|at| async move {
            let range = gateway.range(name, at, (at + RANGE).min(total) - 1).await?;
            range.ok_or_else(|| anyhow::Error::from(Missing(name.to_owned())))
        }))
        .buffered(self.traffic.jobs());
        while let Some(range) = ranges.next().await {
            let (bytes, now) = range?;
            if now != total {
                bail!("{name} changed length from {total} to {now} bytes while it was fetched");
            }
            *received += bytes.len() as u64;
            partial.append(&bytes).await?;
        }
        Ok(())
    }
}

/// An object at least this many bytes from its end is fetched in ranges from a gateway.
const RANGED_AT: u64 = 16 << 20;
/// The size of one of those ranges.
const RANGE: u64 = 8 << 20;

/// An object that a list names and the remote does not have.
#[derive(Debug)]
pub struct Missing(pub String);

/// An object of a remote, as a listing has it.
pub struct Object {
    pub name: String,
    pub len: u64,
    /// When it was last written, by the clock of whatever holds it.
    pub written: SystemTime,
}

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is listed and is not there", self.0)
    }
}

impl std::error::Error for Missing {}

/// A request the service answered with a no that asking again does not change.
#[derive(Debug)]
pub struct Refused(pub String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}
