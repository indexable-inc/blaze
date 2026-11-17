//! The plain store's thin driver: [`Plain`]'s effects carried out on a [`Driver`] (the native
//! one, or the simulation in tests), reads into a caller's buffer by `pread`, the fetch-on-miss
//! seam (blaze-keyed's [`Source`] and [`Verifier`]: one home, rule 30), and the store a root's
//! settings choose ([`Opened`]).

use std::{
    alloc::Global,
    collections::BTreeMap,
    ops::Range,
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_core::{Completion, Driver, Done, Effect, FileName, Native, Tag as EffectTag, wait::Deadline};
use blaze_ids::ContentId;
use blaze_keyed::{
    exec::RecordRef,
    kind::{Namespace, Tag},
    store::{FOOTER, HEADER, Located, Put, Refused, SegmentId, SegmentLen, SegmentRef, Source, Statement, StoreError, StoreFile, Verifier},
};

use super::{
    core::{Batch, Committed, Got, Plain, PlainConfig, PutError, Report, Sliced, TAGS},
    format::{self, Segment},
    settings::{Form, Settings},
};
use crate::{Corrupt, Key};

/// The tags the store asks with; the driver's own (a fetched segment's write) follow them.
const STORE_TAGS: u64 = 1 << 50;
const OWN_TAGS: u64 = STORE_TAGS + TAGS;

/// A driver with effects in flight and no completion for this long has stalled: an error
/// naming what the store waited for.
const STALL: Duration = Duration::from_secs(120);

/// An OS call's failure, by name: the call, what it was on, its errno (0 when the error is not
/// the system's).
fn os(call: &'static str, what: &'static str, error: &std::io::Error) -> StoreError {
    StoreError::Os { call, what, errno: error.raw_os_error().unwrap_or(0) }
}

/// A directory of segments, `<digest hex>.seg`, each beside its `<digest hex>.statement` (a
/// [`Statement`], [`Statement::LEN`] bytes): the one [`Source`] built here (a bundle unpacked on
/// disk, a test's bucket). Its `find` reads each segment's header and footer and probes its
/// index by ranged reads, as a bucket's index shard is probed.
pub struct LocalDir {
    dir: PathBuf,
}

impl LocalDir {
    pub fn new(dir: impl Into<PathBuf>) -> LocalDir {
        LocalDir { dir: dir.into() }
    }

    fn path(&self, digest: &ContentId, ext: &str) -> PathBuf {
        self.dir.join(format!("{}.{ext}", hex(digest)))
    }

    /// Puts a segment's bytes and its statement in the directory, unchanged; the segment it is.
    pub fn put(&self, bytes: &[u8], statement: &Statement) -> Result<SegmentId, StoreError> {
        let described = format::check_segment(bytes)?;
        std::fs::create_dir_all(&self.dir).map_err(|e| os("mkdir", "a local source's directory", &e))?;
        std::fs::write(self.path(&described.digest, "seg"), bytes).map_err(|e| os("write", "a local source's segment", &e))?;
        std::fs::write(self.path(&described.digest, "statement"), statement.encode()).map_err(|e| os("write", "a local source's statement", &e))?;
        Ok(SegmentId(described.digest))
    }

    /// `out.len()` bytes at `at` of the segment `digest`: one `pread`.
    fn read_at(&self, digest: &ContentId, at: u64, out: &mut [u8]) -> Result<(), StoreError> {
        use std::os::unix::fs::FileExt;
        let file = std::fs::File::open(self.path(digest, "seg")).map_err(|e| os("open", "a local source's segment", &e))?;
        file.read_exact_at(out, at).map_err(|e| os("pread", "a local source's segment", &e))
    }
}

fn hex(id: &ContentId) -> String {
    use std::fmt::Write;
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
    fn find(&mut self, namespace: Namespace, keys: &[RecordRef], found: &mut [Option<Located>]) -> Result<(), StoreError> {
        if found.len() != keys.len() {
            return Err(StoreError::Other { what: "a find's answer is one slot for each key asked" });
        }
        found.fill(None);
        let entries = match std::fs::read_dir(&self.dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(os("readdir", "a local source's directory", &e)),
            Ok(entries) => entries,
        };
        let mut digests: Vec<(ContentId, u64)> = Vec::new_in(Global);
        for entry in entries {
            let entry = entry.map_err(|e| os("readdir", "a local source's directory", &e))?;
            let name = entry.file_name();
            if let Some(digest) = name.to_str().and_then(|it| it.strip_suffix(".seg")).and_then(unhex) {
                digests.push((digest, entry.metadata().map_err(|e| os("stat", "a local source's segment", &e))?.len()));
            }
        }
        // By digest, so the answer does not depend on the directory's order.
        digests.sort_unstable();
        for (digest, len) in digests {
            let segment = SegmentId(digest);
            SegmentLen::new(len).map_err(|why| StoreError::Wire { what: "a local source's segment length", why })?;
            let mut head = [0u8; HEADER];
            let mut foot = [0u8; FOOTER];
            self.read_at(&digest, 0, &mut head)?;
            self.read_at(&digest, len - FOOTER as u64, &mut foot)?;
            let Some(described) = format::describe(&head, &foot, len) else {
                return Err(StoreError::Layout { segment, what: "a local source's file is not a plain segment of this format", at: 0 });
            };
            if described.namespace != namespace {
                continue;
            }
            let reference = SegmentRef::new(segment, len, described.index_at, described.count).map_err(|why| StoreError::Wire { what: "a local source's segment", why })?;
            for (key, slot) in keys.iter().zip(found.iter_mut()) {
                if slot.is_some() || key.key.0[..] < head[32..64] || key.key.0[..] > head[64..96] {
                    continue;
                }
                let index_at = described.index_at;
                let hit = format::find_ranged(segment, described.count, &key.key, |at, out| self.read_at(&digest, index_at + at, out))?;
                if let Some((_, entry)) = hit {
                    if entry.tag != key.tag {
                        return Err(StoreError::TwoValues { tag: key.tag, key: key.key });
                    }
                    *slot = Some(Located { segment: reference, at: entry.at, len: entry.len });
                }
            }
        }
        Ok(())
    }

    fn fetch_into(&mut self, segment: &SegmentRef, range: Range<u64>, out: &mut [u8]) -> Result<(), StoreError> {
        let wanted = range.end.saturating_sub(range.start);
        if range.end > segment.size() || out.len() as u64 != wanted {
            return Err(StoreError::Fetched { segment: segment.digest(), len: out.len() as u64, wanted });
        }
        self.read_at(&segment.digest().0, range.start, out)
    }

    fn statement(&mut self, segment: &SegmentRef) -> Result<Statement, StoreError> {
        let bytes = std::fs::read(self.path(&segment.digest().0, "statement")).map_err(|e| os("read", "a local source's statement", &e))?;
        Statement::decode(&bytes).map_err(|why| StoreError::Wire { what: "a local source's statement", why })
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
    /// The plain store in `root/plain`, made if missing, on the native driver over `root`. It
    /// reads and writes the namespaces `settings` name ([`Settings::reads`],
    /// [`Settings::writes`]): everything [`Namespace::LOCAL`] by default.
    pub fn open(root: &Path, settings: &Settings) -> Result<Driven<Native>, StoreError> {
        let dir = FileName::new("plain").expect("a relative name");
        let at = root.join(dir.as_str());
        std::fs::create_dir_all(&at).map_err(|e| os("mkdir", "the plain store's directory", &e))?;
        exclude_from_backup(&at)?;
        let driver = blaze_core::native(root, 2).map_err(|e| os("making the native driver", "the store's root", &e))?;
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
            writes: settings.writes(),
            reads: settings.reads(),
        };
        let mut it = Driven::with(driver, root, cfg)?;
        it.open_files()?;
        Ok(it)
    }

    /// The record under `key` read into `out` by one `pread` of its proved segment (no mapping
    /// is touched, nothing is checked); its tag, or `None` when no namespace read holds it.
    pub fn get_into(&self, key: &Key, out: &mut Vec<u8>) -> Result<Option<Tag>, StoreError> {
        use std::os::unix::fs::FileExt;
        let Some(placed) = self.core.locate(key) else {
            // Put and not yet committed: in memory.
            return Ok(self.core.get(key).map(|got| {
                out.clear();
                out.extend_from_slice(got.bytes);
                got.tag
            }));
        };
        let file = self.files.get(&placed.file).ok_or(StoreError::Other { what: "a segment mapped and not opened for pread" })?;
        out.resize(placed.len as usize, 0);
        file.read_exact_at(out, placed.at).map_err(|e| os("pread", "a plain store's segment", &e))?;
        Ok(Some(placed.tag))
    }

    fn open_files(&mut self) -> Result<(), StoreError> {
        for file in self.core.mapped_files() {
            if self.files.contains_key(file) {
                continue;
            }
            let path = self.root.join(file.as_str());
            let opened = std::fs::File::open(&path).map_err(|e| os("open", "a mapped segment, for pread", &e))?;
            self.files.insert(file.clone(), opened);
        }
        Ok(())
    }

    /// [`Driven::commit_with`], then the new segments opened for `pread`.
    pub fn commit(&mut self, batches: Vec<Batch>) -> Result<Committed, StoreError> {
        let committed = self.commit_with(batches)?;
        self.open_files()?;
        Ok(committed)
    }
}

impl<D: Driver> Driven<D> {
    /// The store `cfg` names, opened on `driver` (whose root is `root`).
    pub fn with(driver: D, root: &Path, cfg: PlainConfig) -> Result<Driven<D>, StoreError> {
        let mut it = Driven {
            driver,
            root: root.to_owned(),
            core: Plain::new(cfg),
            effects: Vec::new_in(Global),
            done: Vec::new_in(Global),
            own: Vec::new_in(Global),
            files: BTreeMap::new(),
        };
        it.core.open(&mut it.effects);
        it.run("opening the plain store")?;
        if !it.core.ready() {
            // Its driver went idle first.
            return Err(StoreError::Unfinished { what: "an open" });
        }
        for lost in &it.core.report().lost {
            tracing::warn!(target: "blaze_cas::plain", file = %lost.file, records = ?lost.records, "lost in a power cut: {}", lost.why);
        }
        // Handed: this open returns with its report ([`Driven::report`]), so the marks of the
        // losses it names may be cleared by this process's next flip (or
        // [`Driven::acknowledge`]). An open that fails hands nothing, and the marks stay for
        // the next.
        it.core.handed();
        Ok(it)
    }

    /// One flip that clears the "lost, not yet reported" marks of the losses this open
    /// reported, for a process that makes no commit or compaction of its own (whose flip
    /// would clear them at no cost): `false` when there were none.
    pub fn acknowledge(&mut self) -> Result<bool, StoreError> {
        if !self.core.acknowledge(&mut self.effects) {
            return Ok(false);
        }
        self.run("clearing the marks of reported losses")?;
        Ok(true)
    }

    /// Runs until nothing is in flight, the store's completions to it, the driver's own kept.
    fn run(&mut self, what: &'static str) -> Result<(), StoreError> {
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
                // Effects in flight and no completion for [`STALL`].
                return Err(StoreError::Stalled { waited_for: what });
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
            Some(why) => Err(why.clone()),
            None => Ok(()),
        }
    }

    pub fn core(&self) -> &Plain {
        &self.core
    }

    pub(crate) fn core_mut(&mut self) -> &mut Plain {
        &mut self.core
    }

    pub fn driver(&mut self) -> &mut D {
        &mut self.driver
    }

    pub fn report(&self) -> &Report {
        self.core.report()
    }

    pub fn get(&self, key: &Key) -> Option<Got<'_>> {
        self.core.get(key)
    }

    pub fn has(&self, key: &Key) -> bool {
        self.core.has(key)
    }

    /// The explicit scrub ([`Plain::scrub`]): every record's CRC-32C against its bytes now.
    pub fn scrub(&self) -> Vec<Corrupt> {
        self.core.scrub()
    }

    pub fn put(&mut self, key: Key, namespace: Namespace, tag: Tag, value: &[u8]) -> Result<Put, PutError> {
        self.core.put(key, namespace, tag, value)
    }

    pub fn root(&mut self, name: &str, keys: &[Key]) -> Result<Key, StoreError> {
        self.core.root(name, keys)
    }

    pub fn unroot(&mut self, name: &str) {
        self.core.unroot(name)
    }

    /// Writes the store's puts and `batches` as one segment for each namespace that holds
    /// records, and flips to a generation naming them, driven until done.
    pub fn commit_with(&mut self, batches: Vec<Batch>) -> Result<Committed, StoreError> {
        let now = self.driver.wall_ns();
        self.core.commit(batches, now, &mut self.effects)?;
        self.run("committing")?;
        match self.core.take_committed() {
            Some(outcome) => outcome,
            None => Err(StoreError::Unfinished { what: "a commit" }),
        }
    }

    /// Up to `slices` compaction slices, each bounded by [`super::SLICE_BYTES`], until one says
    /// there is no more. Lowers the calling thread first ([`lower_this_thread`]): call it after
    /// the command has answered, or from its own invocation, never before a build's first read.
    pub fn compact(&mut self, slices: u32) -> Result<Vec<Sliced>, StoreError> {
        lower_this_thread()?;
        self.slices(slices)
    }

    /// [`Driven::compact`] on the calling thread as it is: for a caller that runs it beside
    /// builds on a thread it lowered itself.
    pub(crate) fn slices(&mut self, slices: u32) -> Result<Vec<Sliced>, StoreError> {
        let mut out = Vec::new_in(Global);
        for _ in 0..slices {
            if !self.core.compact(&mut self.effects) {
                return Err(StoreError::Busy { phase: self.core.what() });
            }
            self.run("compacting")?;
            let sliced = self.core.take_sliced().ok_or(StoreError::Unfinished { what: "a compaction slice" })?;
            out.push(sliced);
            if !sliced.more || sliced.aborted || sliced.segments == 0 {
                break;
            }
        }
        Ok(out)
    }

    /// Fetches what `source` holds of `keys` in `namespace` (one of the namespaces the store
    /// reads, and the only one asked): each segment it names is verified ([`Verifier`]: before
    /// a byte of it is fetched), fetched whole, checked once (its digest is its id, its
    /// namespace the one asked, its layout), written here unchanged, and named in a new
    /// generation. The segments adopted, by id.
    ///
    /// The fetch buffer is the global heap, not the caller's frame: the bytes are handed to the
    /// segment's write ([`Effect::WriteUnsynced`] owns a `Vec<u8>` of the global heap), so a
    /// frame's buffer would be copied once more.
    pub fn fetch_missing(&mut self, source: &mut dyn Source, verifier: &dyn Verifier, namespace: Namespace, keys: &[RecordRef]) -> Result<Vec<u64>, StoreError> {
        if !self.core.reads().contains(&namespace) {
            return Err(StoreError::Other { what: "a fetch from a namespace this store does not read: a key is never answered from a namespace the caller did not ask for" });
        }
        let mut missing: Vec<RecordRef> = Vec::with_capacity_in(keys.len(), Global);
        missing.extend(keys.iter().filter(|it| self.core.get_in(&[namespace], &Key(it.key)).is_none()).copied());
        missing.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        missing.dedup_by(|a, b| a.key == b.key);
        if missing.is_empty() {
            return Ok(Vec::new_in(Global));
        }
        let mut found: Vec<Option<Located>> = Vec::with_capacity_in(missing.len(), Global);
        found.resize(missing.len(), None);
        source.find(namespace, &missing, &mut found)?;
        let mut segments: BTreeMap<SegmentId, SegmentRef> = BTreeMap::new();
        for it in found.into_iter().flatten() {
            segments.insert(it.segment.digest(), it.segment);
        }
        let mut wanted = Vec::with_capacity_in(segments.len(), Global);
        wanted.extend(segments.into_values());
        self.pull(source, verifier, namespace, &wanted)
    }

    /// Fetches whole segments a source names (a bundle: std for a toolchain, target and cfg
    /// set), each verified and checked as [`Driven::fetch_missing`] says, and names them here.
    pub fn pull(&mut self, source: &mut dyn Source, verifier: &dyn Verifier, namespace: Namespace, wanted: &[SegmentRef]) -> Result<Vec<u64>, StoreError> {
        let mut adopted = Vec::with_capacity_in(wanted.len(), Global);
        let mut ids = Vec::with_capacity_in(wanted.len(), Global);
        for (i, want) in wanted.iter().enumerate() {
            let segment = want.digest();
            let statement = source.statement(want)?;
            if statement.namespace != namespace {
                return Err(StoreError::Refused { segment, why: Refused::Namespace { found: statement.namespace, wanted: namespace } });
            }
            verifier.verify(&statement, want).map_err(|why| StoreError::Refused { segment, why })?;
            // The length is the reference's, checked by its constructor against the most a
            // segment is: nothing past it is ever allocated.
            let mut bytes: Vec<u8> = Vec::with_capacity_in(want.size() as usize, Global);
            bytes.resize(want.size() as usize, 0);
            source.fetch_into(want, 0..want.size(), &mut bytes)?;
            let described = format::check_segment(&bytes)?;
            if described.digest != segment.0 {
                return Err(StoreError::Refused { segment, why: Refused::Digest { found: described.digest, wanted: segment } });
            }
            if described.namespace != namespace {
                return Err(StoreError::Namespace { found: described.namespace, wanted: namespace });
            }
            let seg = Segment::of(&described, want.size());
            let file = FileName::new(&format!("{}/seg-{:016x}", self.core.dir().as_str(), seg.id)).expect("a store's file");
            self.effects.push(Effect::WriteUnsynced { file: file.clone(), bytes, tag: EffectTag(OWN_TAGS + i as u64) });
            self.run("writing a fetched segment")?;
            for completion in self.own.drain(..) {
                if let Done::Failed(error) = completion.done {
                    return Err(StoreError::Io { doing: "writing a fetched segment", file: StoreFile::Segment { id: seg.id }, error: error.0 });
                }
            }
            ids.push(seg.id);
            adopted.push(seg);
        }
        if adopted.is_empty() {
            return Ok(ids);
        }
        self.core.adopt(adopted, &mut self.effects)?;
        self.run("naming fetched segments")?;
        Ok(ids)
    }

    /// Marks segments as pushed to a shared bucket: compaction never rewrites them.
    pub fn publish(&mut self, ids: Vec<u64>) -> Result<(), StoreError> {
        self.core.publish(ids, &mut self.effects)?;
        self.run("marking segments published")
    }
}

/// Lowers the calling thread for background work: utility QoS on macOS
/// (`pthread_set_qos_class_self_np`); nice 19 and the idle IO class on Linux, both per thread
/// there. One way: an unprivileged thread cannot raise itself again.
pub fn lower_this_thread() -> Result<(), StoreError> {
    #[cfg(target_vendor = "apple")]
    {
        // SAFETY: one call about the calling thread, no memory passed.
        let rc = unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0) };
        if rc != 0 {
            return Err(StoreError::Os { call: "pthread_set_qos_class_self_np(QOS_CLASS_UTILITY)", what: "the calling thread", errno: rc });
        }
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: one system call about the calling thread (who 0 is the caller), no memory passed.
        if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19) } != 0 {
            return Err(os("setpriority(PRIO_PROCESS, 0, 19)", "the calling thread", &std::io::Error::last_os_error()));
        }
        // IOPRIO_WHO_PROCESS = 1, IOPRIO_CLASS_IDLE = 3 in bits 13 and up.
        // SAFETY: as above.
        if unsafe { libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13) } != 0 {
            return Err(os("ioprio_set(IOPRIO_CLASS_IDLE)", "the calling thread", &std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// Marks `dir` excluded from Time Machine on macOS: the sticky attribute `tmutil addexclusion`
/// sets (`com.apple.metadata:com_apple_backup_excludeItem`, a binary plist of the string
/// `com.apple.backupd`), which follows the directory if it moves. `CACHEDIR.TAG` (written by
/// the first commit) covers the tools that honour it elsewhere.
fn exclude_from_backup(dir: &Path) -> Result<(), StoreError> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::unix::ffi::OsStrExt;
        const NAME: &std::ffi::CStr = c"com.apple.metadata:com_apple_backup_excludeItem";
        const VALUE: &[u8] = b"bplist00_\x10\x11com.apple.backupd\x08\x00\x00\x00\x00\x00\x00\x01\x01\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x1c";
        // A path the system gave holds no NUL; one that does is refused as the kernel would.
        let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).map_err(|_| StoreError::Os { call: "setxattr", what: "the plain store's directory (a path with a NUL)", errno: libc::EINVAL })?;
        // SAFETY: both strings are NUL-terminated and live across the call; `VALUE` is
        // `VALUE.len()` bytes; position 0, no options.
        let rc = unsafe { libc::setxattr(path.as_ptr(), NAME.as_ptr(), VALUE.as_ptr().cast(), VALUE.len(), 0, 0) };
        if rc != 0 {
            return Err(os("setxattr(com.apple.metadata:com_apple_backup_excludeItem)", "the plain store's directory", &std::io::Error::last_os_error()));
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
        (rc == 0).then(|| {
            let mut out = Vec::with_capacity_in(len, Global);
            out.extend_from_slice(&buf[..len]);
            out
        })
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
            // The packs' open says its failure as text; the plain store's typed error is
            // printed beside it.
            Form::Plain => Driven::open(root, &settings).map(Opened::Plain).map_err(|it| it.to_string()),
        }
    }
}
