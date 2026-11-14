//! The plain store's thin driver: [`Plain`]'s effects carried out on a [`Driver`] (the native
//! one, or the simulation in tests), reads into a caller's buffer by `pread`, the fetch-on-miss
//! seam ([`Source`], [`Verifier`]), and the store a root's settings choose ([`Opened`]).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_core::{Completion, Done, Driver, Effect, FileName, Native, Tag, wait::Deadline};
use blaze_ids::ContentId;

use super::{
    core::{Batch, Committed, Got, Plain, PlainConfig, PutError, Report, Sliced, TAGS},
    format::{self, Namespace, SEGMENT_MOST, Segment},
    settings::{Form, Settings},
};
use crate::{Corrupt, Key, Kind};

/// The tags the store asks with; the driver's own (a fetched segment's write) follow them.
const STORE_TAGS: u64 = 1 << 50;
const OWN_TAGS: u64 = STORE_TAGS + TAGS;

/// A driver with effects in flight and no completion for this long has stalled: an error
/// naming what the store waited for.
const STALL: Duration = Duration::from_secs(120);

/// Where a key was found by a [`Source`]: the segment holding it, as its header and footer
/// describe it (digest, length, index, entries, namespace).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Found {
    pub key: Key,
    pub segment: Segment,
}

/// Somewhere else that holds segments: a shared bucket (written by another lane), or a
/// local directory ([`LocalDir`], the one built here). Asked on a miss, in batches. What it
/// returns is untrusted until the store has checked it ([`Verifier`], the digest, the
/// layout); nothing it returns is used before.
pub trait Source {
    /// Which of `keys` (sorted, each once) it holds in `namespace`, each with the segment that
    /// holds it: one round trip a call (a bucket answers from its index shards).
    fn find(&mut self, namespace: Namespace, keys: &[Key]) -> Result<Vec<Found>, String>;

    /// `len` bytes at `at` of the segment whose digest is `digest`: one ranged read. Fewer
    /// bytes than asked is the source's error, not a short answer.
    fn fetch(&mut self, digest: &ContentId, at: u64, len: u64) -> Result<Vec<u8>, String>;

    /// The signed statement it holds for the segment `digest`: what a [`Verifier`] checks.
    fn statement(&mut self, digest: &ContentId) -> Result<Vec<u8>, String>;
}

/// What decides whether a fetched segment is trusted. A segment's digest covers every byte of
/// it (header, payloads, index), so a statement binding (namespace, digest, length) binds every
/// key in it to its value. The store asks this, then checks the bytes against the digest and
/// the layout, before it writes or maps anything.
pub trait Verifier {
    /// Ok when `statement` is one this machine trusts: that a builder trusted for `namespace`
    /// made the segment `digest` of `len` bytes. Why not, otherwise.
    fn verify(&self, namespace: Namespace, digest: &ContentId, len: u64, statement: &[u8]) -> Result<(), String>;
}

/// A directory of segments, `<digest hex>.seg`, each beside its `<digest hex>.statement`: the
/// one [`Source`] built now (a bundle unpacked on disk, a test's bucket). Its `find` reads each
/// segment's header and probes its index by ranged reads, as a bucket's index shard is probed.
pub struct LocalDir {
    dir: PathBuf,
}

impl LocalDir {
    pub fn new(dir: impl Into<PathBuf>) -> LocalDir {
        LocalDir { dir: dir.into() }
    }

    fn path(&self, digest: &ContentId, ext: &str) -> PathBuf {
        // alloc: a path, once a read
        self.dir.join(format!("{}.{ext}", hex(digest)))
    }

    /// Puts a segment's bytes and its statement in the directory, unchanged.
    pub fn put(&self, bytes: &[u8], statement: &[u8]) -> Result<ContentId, String> {
        let described = format::check_segment(bytes).map_err(|why| format!("a segment to put: {why}"))?;
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        let seg = self.path(&described.digest, "seg");
        std::fs::write(&seg, bytes).map_err(|e| format!("{}: {e}", seg.display()))?;
        let st = self.path(&described.digest, "statement");
        std::fs::write(&st, statement).map_err(|e| format!("{}: {e}", st.display()))?;
        Ok(described.digest)
    }
}

fn hex(id: &ContentId) -> String {
    use std::fmt::Write;
    // alloc: a name
    let mut out = String::with_capacity(64);
    for byte in id.0 {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn unhex(text: &str) -> Option<ContentId> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(ContentId(out))
}

impl Source for LocalDir {
    fn find(&mut self, namespace: Namespace, keys: &[Key]) -> Result<Vec<Found>, String> {
        let entries = match std::fs::read_dir(&self.dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("{}: {e}", self.dir.display())),
            Ok(entries) => entries,
        };
        // alloc: a batch's answer
        let mut digests = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", self.dir.display()))?;
            let name = entry.file_name();
            if let Some(digest) = name.to_str().and_then(|it| it.strip_suffix(".seg")).and_then(unhex) {
                digests.push((digest, entry.metadata().map_err(|e| format!("{}: {e}", entry.path().display()))?.len()));
            }
        }
        // By digest, so the answer does not depend on the directory's order.
        digests.sort_unstable();
        // alloc: as above
        let mut out = Vec::new();
        for (digest, len) in digests {
            if len < (format::HEADER + format::FOOTER) as u64 || len > SEGMENT_MOST {
                continue;
            }
            let head = self.fetch(&digest, 0, format::HEADER as u64)?;
            let foot = self.fetch(&digest, len - format::FOOTER as u64, format::FOOTER as u64)?;
            let Some(described) = format::describe(&head, &foot, len) else { continue };
            if described.namespace != namespace {
                continue;
            }
            let segment = Segment::of(&described, len);
            for key in keys {
                if key.0.0[..] < head[32..64] || key.0.0[..] > head[64..96] {
                    continue;
                }
                let found = format::find_ranged(described.count, key, |at, n| self.fetch(&digest, described.index_at + at, n as u64))?;
                if found.is_some() {
                    out.push(Found { key: *key, segment });
                }
            }
        }
        Ok(out)
    }

    fn fetch(&mut self, digest: &ContentId, at: u64, len: u64) -> Result<Vec<u8>, String> {
        use std::os::unix::fs::FileExt;
        let path = self.path(digest, "seg");
        let file = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        // alloc: the bytes asked for
        let mut out = vec![0u8; len as usize];
        file.read_exact_at(&mut out, at).map_err(|e| format!("{}: {len} bytes at {at}: {e}", path.display()))?;
        Ok(out)
    }

    fn statement(&mut self, digest: &ContentId) -> Result<Vec<u8>, String> {
        let path = self.path(digest, "statement");
        std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// The plain store driven on the calling thread.
pub struct Driven<D: Driver = Native> {
    driver: D,
    root: PathBuf,
    core: Plain,
    effects: Vec<Effect>,
    done: Vec<Completion>,
    /// Completions of the driver's own effects, by tag.
    own: Vec<Completion>,
    /// Each mapped segment opened once more for `pread` ([`Driven::get_into`]), native only.
    files: BTreeMap<FileName, std::fs::File>,
}

impl Driven<Native> {
    /// The plain store in `root/plain`, made if missing, on the native driver over `root`.
    pub fn open(root: &Path, settings: &Settings) -> Result<Driven<Native>, String> {
        let dir = FileName::new("plain").expect("a relative name");
        let at = root.join(dir.as_str());
        std::fs::create_dir_all(&at).map_err(|e| format!("{}: {e}", at.display()))?;
        exclude_from_backup(&at)?;
        let driver = blaze_core::native(root, 2).map_err(|e| format!("the native driver over {}: {e}", root.display()))?;
        let nonce = nonce();
        let cfg = PlainConfig {
            dir,
            tags: STORE_TAGS,
            cap_bytes: settings.cap_bytes(),
            // No boot id known: every open checks every segment, never none.
            boot: boot().unwrap_or(nonce),
            nonce,
            durable: settings.durable,
            page: blaze_mem::page_size() as u32,
            namespace: Namespace::LOCAL,
            // alloc: once a store
            reads: vec![Namespace::LOCAL, Namespace::PUBLIC],
        };
        let mut it = Driven::with(driver, root, cfg)?;
        it.open_files()?;
        Ok(it)
    }

    /// The record under `key` read into `out` by one `pread` of its proved segment (no mapping
    /// is touched, nothing is checked); its kind, or `None` when no segment holds it.
    pub fn get_into(&self, key: &Key, out: &mut Vec<u8>) -> Result<Option<Kind>, String> {
        use std::os::unix::fs::FileExt;
        let Some(located) = self.core.locate(key).map_err(|it| it.to_string())? else {
            // Put and not yet committed: in memory.
            return Ok(self.core.get(key).map_err(|it| it.to_string())?.map(|got| {
                out.clear();
                out.extend_from_slice(got.bytes);
                got.kind
            }));
        };
        let file = self.files.get(&located.file).ok_or_else(|| format!("{}: mapped but not opened", located.file))?;
        out.resize(located.len as usize, 0);
        file.read_exact_at(out, located.at).map_err(|e| format!("{}: {} bytes at {}: {e}", located.file, located.len, located.at))?;
        Ok(Some(located.kind))
    }

    fn open_files(&mut self) -> Result<(), String> {
        for file in self.core.mapped_files() {
            if self.files.contains_key(file) {
                continue;
            }
            let path = self.root.join(file.as_str());
            let opened = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            self.files.insert(file.clone(), opened);
        }
        Ok(())
    }

    /// [`Driven::commit_with`], then the new segment opened for `pread`.
    pub fn commit(&mut self, batches: Vec<Batch>) -> Result<Committed, String> {
        let committed = self.commit_with(batches)?;
        self.open_files()?;
        Ok(committed)
    }
}

impl<D: Driver> Driven<D> {
    /// The store `cfg` names, opened on `driver` (whose root is `root`).
    pub fn with(driver: D, root: &Path, cfg: PlainConfig) -> Result<Driven<D>, String> {
        let mut it = Driven {
            driver,
            root: root.to_owned(),
            core: Plain::new(cfg),
            effects: Vec::new(),
            done: Vec::new(),
            own: Vec::new(),
            files: BTreeMap::new(),
        };
        it.core.open(&mut it.effects);
        it.run("opening the plain store")?;
        if !it.core.ready() {
            return Err(format!("the plain store in {} did not open: its driver went idle first", it.core.dir()));
        }
        for lost in &it.core.report().lost {
            tracing::warn!(target: "blaze_cas::plain", file = %lost.file, records = ?lost.records, "lost in a power cut: {}", lost.why);
        }
        Ok(it)
    }

    /// Runs until nothing is in flight, the store's completions to it, the driver's own kept.
    fn run(&mut self, what: &'static str) -> Result<(), String> {
        let _span = tracing::info_span!("plain.wait", what).entered();
        let mut since = self.driver.now();
        loop {
            self.driver.submit(&mut self.effects);
            if self.driver.in_flight() == 0 {
                break;
            }
            let stall = Deadline::after(since, STALL);
            if self.driver.poll(&mut self.done, stall) > 0 {
                since = self.driver.now();
            } else if stall.passed(self.driver.now()) {
                return Err(format!("the plain store in {} stalled {what}: {} effects in flight for {} s", self.core.dir(), self.driver.in_flight(), STALL.as_secs()));
            }
            let wall = self.driver.wall_ns();
            for completion in self.done.drain(..) {
                if self.core.owns(completion.tag) {
                    self.core.complete(completion, wall, &mut self.effects);
                } else {
                    self.own.push(completion);
                }
            }
        }
        match self.core.broken() {
            Some(why) => Err(why.to_owned()),
            None => Ok(()),
        }
    }

    pub fn core(&self) -> &Plain {
        &self.core
    }

    pub fn driver(&mut self) -> &mut D {
        &mut self.driver
    }

    pub fn report(&self) -> &Report {
        self.core.report()
    }

    pub fn get(&self, key: &Key) -> Result<Option<Got<'_>>, Corrupt> {
        self.core.get(key)
    }

    pub fn has(&self, key: &Key) -> Result<bool, Corrupt> {
        self.core.has(key)
    }

    /// The explicit scrub ([`Plain::scrub`]): every record's CRC-32C against its bytes now.
    pub fn scrub(&self) -> Vec<Corrupt> {
        self.core.scrub()
    }

    pub fn put(&mut self, key: Key, kind: Kind, value: &[u8]) -> Result<(), PutError> {
        self.core.put(key, kind, value)
    }

    pub fn root(&mut self, name: &str, keys: &[Key]) -> Result<Key, PutError> {
        self.core.root(name, keys)
    }

    pub fn unroot(&mut self, name: &str) {
        self.core.unroot(name)
    }

    /// Writes the store's puts and `batches` as one segment and flips to a generation naming
    /// it, driven until done.
    pub fn commit_with(&mut self, batches: Vec<Batch>) -> Result<Committed, String> {
        let now = self.driver.wall_ns();
        self.core.commit(batches, now, &mut self.effects).map_err(|it| it.to_string())?;
        self.run("committing")?;
        match self.core.take_committed() {
            Some(outcome) => outcome,
            None => Err(format!("the plain store in {} ended a commit without an outcome", self.core.dir())),
        }
    }

    /// Up to `slices` compaction slices, each bounded by [`super::SLICE_BYTES`], until one says
    /// there is no more. Lowers the calling thread first ([`lower_this_thread`]): call it after
    /// the command has answered, or from its own invocation, never before a build's first read.
    pub fn compact(&mut self, slices: u32) -> Result<Vec<Sliced>, String> {
        lower_this_thread()?;
        // alloc: a report, once a compaction
        let mut out = Vec::new();
        for _ in 0..slices {
            if !self.core.compact(&mut self.effects) {
                return Err(format!("the plain store in {} is not ready to compact", self.core.dir()));
            }
            self.run("compacting")?;
            let sliced = self.core.take_sliced().ok_or_else(|| format!("the plain store in {} ended a slice without an outcome", self.core.dir()))?;
            out.push(sliced);
            if !sliced.more || sliced.aborted || sliced.segments == 0 {
                break;
            }
        }
        Ok(out)
    }

    /// Fetches what `source` holds of `keys` in `namespace` (one of the namespaces the store
    /// reads): each segment it names is verified ([`Verifier`]: before a byte is fetched),
    /// fetched whole, checked (its digest, its namespace, its layout), written here unchanged,
    /// and named in a new generation. The segments adopted, by id.
    pub fn fetch_missing(&mut self, source: &mut dyn Source, verifier: &dyn Verifier, namespace: Namespace, keys: &[Key]) -> Result<Vec<u64>, String> {
        if !self.core.reads().contains(&namespace) {
            return Err(format!("namespace {:016x} is not one this store reads: a key is never answered from a namespace the caller did not ask for", namespace.0));
        }
        // alloc: once a fetch
        let mut missing: Vec<Key> = Vec::new();
        for key in keys {
            if !self.core.has(key).map_err(|it| it.to_string())? {
                missing.push(*key);
            }
        }
        missing.sort_unstable();
        missing.dedup();
        if missing.is_empty() {
            return Ok(Vec::new());
        }
        let found = source.find(namespace, &missing)?;
        // alloc: as above
        let mut segments: BTreeMap<ContentId, Segment> = BTreeMap::new();
        for it in found {
            if it.segment.namespace != namespace {
                return Err(format!("the source answered {:x} from namespace {:016x}, asked for {:016x}: refused", it.key.0, it.segment.namespace.0, namespace.0));
            }
            segments.insert(it.segment.digest, it.segment);
        }
        self.pull(source, verifier, namespace, segments.into_values().collect())
    }

    /// Fetches whole segments a source names (a bundle: std for a toolchain, target and cfg
    /// set), each verified and checked as [`Driven::fetch_missing`] says, and names them here.
    pub fn pull(&mut self, source: &mut dyn Source, verifier: &dyn Verifier, namespace: Namespace, wanted: Vec<Segment>) -> Result<Vec<u64>, String> {
        // alloc: once a pull
        let mut adopted = Vec::new();
        let mut ids = Vec::new();
        for (i, want) in wanted.into_iter().enumerate() {
            let name = hex(&want.digest);
            if want.len > SEGMENT_MOST || want.len < (format::HEADER + format::FOOTER) as u64 {
                return Err(format!("segment {name}: the source says it is {} bytes; a segment is at most {SEGMENT_MOST}: refused before a byte is fetched", want.len));
            }
            let statement = source.statement(&want.digest)?;
            verifier.verify(namespace, &want.digest, want.len, &statement).map_err(|why| format!("segment {name}: its statement is not trusted: {why}"))?;
            let bytes = source.fetch(&want.digest, 0, want.len)?;
            if bytes.len() as u64 != want.len {
                return Err(format!("segment {name}: {} bytes fetched, {} named", bytes.len(), want.len));
            }
            let described = format::check_segment(&bytes).map_err(|why| format!("segment {name}: {why}"))?;
            if described.digest != want.digest || described.namespace != namespace {
                return Err(format!("segment {name}: its bytes are segment {} of namespace {:016x}: not the one named and verified; refused", hex(&described.digest), described.namespace.0));
            }
            let seg = Segment::of(&described, want.len);
            let file = FileName::new(&format!("{}/seg-{:016x}", self.core.dir().as_str(), seg.id)).expect("a store's file");
            self.effects.push(Effect::WriteUnsynced { file: file.clone(), bytes, tag: Tag(OWN_TAGS + i as u64) });
            self.run("writing a fetched segment")?;
            let written = self.own.drain(..).collect::<Vec<_>>();
            for completion in written {
                if let Done::Failed(error) = completion.done {
                    return Err(format!("{file}: writing a fetched segment: {}", error.0));
                }
            }
            ids.push(seg.id);
            adopted.push(seg);
        }
        if adopted.is_empty() {
            return Ok(ids);
        }
        self.core.adopt(adopted, &mut self.effects).map_err(|it| it.to_string())?;
        self.run("naming fetched segments")?;
        Ok(ids)
    }

    /// Marks segments as pushed to a shared bucket: compaction never rewrites them.
    pub fn publish(&mut self, ids: Vec<u64>) -> Result<(), String> {
        self.core.publish(ids, &mut self.effects).map_err(|it| it.to_string())?;
        self.run("marking segments published")
    }
}

/// Lowers the calling thread for background work: utility QoS on macOS
/// (`pthread_set_qos_class_self_np`); nice 19 and the idle IO class on Linux, both per thread
/// there. One way: an unprivileged thread cannot raise itself again.
pub fn lower_this_thread() -> Result<(), String> {
    #[cfg(target_vendor = "apple")]
    {
        // SAFETY: one call about the calling thread, no memory passed.
        let rc = unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0) };
        if rc != 0 {
            return Err(format!("pthread_set_qos_class_self_np(QOS_CLASS_UTILITY): errno {rc}"));
        }
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: one system call about the calling thread (who 0 is the caller), no memory passed.
        if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19) } != 0 {
            return Err(format!("setpriority(PRIO_PROCESS, 0, 19): {}", std::io::Error::last_os_error()));
        }
        // IOPRIO_WHO_PROCESS = 1, IOPRIO_CLASS_IDLE = 3 in bits 13 and up.
        // SAFETY: as above.
        if unsafe { libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13) } != 0 {
            return Err(format!("ioprio_set(IOPRIO_CLASS_IDLE): {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// Marks `dir` excluded from Time Machine on macOS: the sticky attribute `tmutil addexclusion`
/// sets (`com.apple.metadata:com_apple_backup_excludeItem`, a binary plist of the string
/// `com.apple.backupd`), which follows the directory if it moves. `CACHEDIR.TAG` (written by
/// the first commit) covers the tools that honour it elsewhere.
fn exclude_from_backup(dir: &Path) -> Result<(), String> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::unix::ffi::OsStrExt;
        const NAME: &std::ffi::CStr = c"com.apple.metadata:com_apple_backup_excludeItem";
        const VALUE: &[u8] = b"bplist00_\x10\x11com.apple.backupd\x08\x00\x00\x00\x00\x00\x00\x01\x01\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x1c";
        let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).map_err(|_| format!("{}: a path with a NUL", dir.display()))?;
        // SAFETY: both strings are NUL-terminated and live across the call; `VALUE` is
        // `VALUE.len()` bytes; position 0, no options.
        let rc = unsafe { libc::setxattr(path.as_ptr(), NAME.as_ptr(), VALUE.as_ptr().cast(), VALUE.len(), 0, 0) };
        if rc != 0 {
            return Err(format!("{}: setxattr({}): {}", dir.display(), NAME.to_string_lossy(), std::io::Error::last_os_error()));
        }
    }
    let _ = dir;
    Ok(())
}

/// The machine's boot as a number: Linux's `boot_id`, macOS's `kern.bootsessionuuid`, hashed.
fn boot() -> Option<u64> {
    #[cfg(target_os = "linux")]
    let id: Option<Vec<u8>> = std::fs::read("/proc/sys/kernel/random/boot_id").ok();
    #[cfg(target_vendor = "apple")]
    let id: Option<Vec<u8>> = {
        let mut buf = [0u8; 64];
        let mut len = buf.len();
        // SAFETY: the name is NUL-terminated, `buf` is `len` bytes to write, no new value is set.
        let rc = unsafe { libc::sysctlbyname(c"kern.bootsessionuuid".as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
        (rc == 0).then(|| buf[..len].to_vec())
    };
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    let id: Option<Vec<u8>> = None;
    let id = id?;
    (!id.is_empty()).then(|| format::id_of("blaze plain store boot 1", &[&id]))
}

/// This process's: its pid, the wall clock and an address, hashed.
fn nonce() -> u64 {
    let here = 0u8;
    let address = (&here as *const u8 as usize as u64).to_le_bytes();
    format::id_of("blaze plain store nonce 1", &[&std::process::id().to_le_bytes(), &blaze_core::system_wall_ns().to_le_bytes(), &address])
}

/// The store a root's settings (`<root>/store.json`) choose: the packs, as before, or the plain
/// store.
pub enum Opened {
    Packs(crate::Driven),
    Plain(Driven<Native>),
}

impl Opened {
    pub fn open(root: &Path) -> Result<Opened, String> {
        let settings = Settings::read(root)?;
        match settings.form {
            Form::Packs => crate::Driven::open(root).map(Opened::Packs),
            Form::Plain => Driven::open(root, &settings).map(Opened::Plain),
        }
    }
}
