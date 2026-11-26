//! The plain store's thin driver: [`Plain`]'s effects carried out on a [`Driver`] (the native
//! one, or the simulation in tests), reads into a caller's buffer by `pread`, the fetch-on-miss
//! seam (blaze-keyed's [`Source`] and [`Verifier`]: one home, rule 30).

use std::alloc::Allocator;
use std::{
    alloc::Global,
    collections::BTreeMap,
    io::Error as IoError,
    ops::Range,
    path::{Path, PathBuf},
    process,
    time::Duration,
};

use blaze_core::{Completion, Driver, Done, Effect, FileName, LockMode, Native, Tag as EffectTag, wait::Deadline};
use blaze_ids::ContentId;
use blaze_own::Share;
use blaze_keyed::{
    exec::RecordRef,
    kind::{Namespace, Tag},
    store::{FOOTER, HEADER, Holder, Located, Put, Refused, SegmentId, SegmentLen, SegmentRef, Source, Statement, StoreError, StoreFile, TwoValues, Verifier},
};

use super::{
    core::{Batch, CommitDurability, CommitRootMap, Committed, Found, GcPlan, Got, LentRecord, Liveness, PidCheck, PidLiveness, Plain, PlainConfig, Probe, Probes, PutError, Renewed, Report, Scrubbed, Sliced, TAGS},
    format::{self, Segment},
    par::{Par, Serial},
    settings::Settings,
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
                        // A read of kind `key.tag` met another kind's record (rule 21).
                        return Err(StoreError::TwoValues(TwoValues { key: key.key, tag: key.tag, len: None, held_tag: entry.tag, held_len: u64::from(entry.len), held: Holder::Committed, differs_at: None }));
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

/// Reset-safe identity of the committed `head` file on disk: combines the head slot's full
/// manifest coordinates (`generation`, `log`, `manifest_at`, `manifest_len`, `manifest_crc`) with
/// the path-resolved `(dev, ino)` from `Stat`, so a resident daemon detects external commits,
/// `blaze clean --store`, and wipe+recreate to the same generation number (even when an `io_uring`
/// driver retains a cached descriptor for the unlinked old `head` inode).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HeadIdentity {
    pub generation: u64,
    pub log: u64,
    pub manifest_at: u64,
    pub manifest_len: u32,
    pub manifest_crc: u32,
    pub dev: u64,
    pub ino: u64,
}

/// The plain store driven on the calling thread, what it keeps in the caller's `A`. The
/// effect queues stay blaze-core's process-heap vectors (`Driver::submit`, `Driver::poll`).
pub struct Driven<D: Driver, A: Allocator + Clone> {
    driver: D,
    root: PathBuf,
    core: Plain<A>,
    effects: Vec<Effect>,
    done: Vec<Completion>,
    /// Completions of the driver's own effects, by tag.
    own: Vec<Completion>,
    /// Each mapped segment opened once more for `pread` ([`Driven::get_into`]), native only.
    files: BTreeMap<FileName, std::fs::File>,
    /// The test's power cut ([`Driven::cut`]).
    #[cfg(test)]
    cut: Option<Cut>,
    /// Test-only knob to suppress automatic tiered merging on commit when a test intentionally
    /// constructs many unmerged segments (e.g. `bloom_tests` exercising 120-segment skipping).
    #[cfg(test)]
    auto_merge: bool,
}

/// Where a test's power cut falls ([`Driven::cut`]): every run from then on stops, as if the
/// process and the machine went down there.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Cut {
    /// After this many more completions.
    After(u32),
    /// At the first completion after a run's flip landed: between the flip and what its
    /// commit asks after it (a late sync), a window a drawn count almost never hits.
    AtFlip,
}

impl<A: Allocator + Clone> Driven<Native, A> {
    /// The plain store in `root/plain`, made if missing, on the native driver over `root`. It
    /// reads and writes the namespaces `settings` name ([`Settings::reads`],
    /// [`Settings::writes`]): everything [`Namespace::LOCAL`] by default. What it keeps is in
    /// `alloc`, the caller's.
    pub fn open(root: &Path, settings: &Settings, alloc: A) -> Result<Driven<Native, A>, StoreError> {
        Driven::open_with_par(root, settings, alloc, &Serial)
    }

    /// Parallel twin of [`Driven::open`]: verifies large earlier-boot segments through `par`.
    pub fn open_with_par<P: Par>(root: &Path, settings: &Settings, alloc: A, par: &P) -> Result<Driven<Native, A>, StoreError> {
        let dir = FileName::new("plain").expect("a relative name");
        let at = root.join(dir.as_str());
        std::fs::create_dir_all(&at).map_err(|e| os("mkdir", "the plain store's directory", &e))?;
        exclude_from_backup(&at)?;
        let driver = blaze_core::native(root, 2).map_err(|e| os("making the native driver", "the store's root", &e))?;
        let nonce = nonce();
        let spelled = root.as_os_str().as_encoded_bytes();
        let mut own_root = Vec::with_capacity_in(spelled.len(), alloc.clone());
        own_root.extend_from_slice(spelled);
        let mut machine = Vec::with_capacity_in(1, alloc.clone());
        machine.push(own_root);
        let cfg = PlainConfig {
            dir,
            tags: STORE_TAGS,
            cap_bytes: settings.cap_bytes(),
            // No boot id known: every open checks every segment, never none.
            boot: boot().unwrap_or(nonce),
            nonce,
            durable: settings.durable,
            // The store's own root, in the spelling it was opened by; the caller adds the
            // checkout's and the home's ([`Driven::machine`]).
            machine,
            writes: settings.writes(alloc.clone()),
            reads: settings.reads(alloc.clone()),
            // The keyed handle's callers root nothing of their own.
            liveness: Liveness::Everything,
            lock_step: crate::plain::LockStep::Backoff,
        };
        let mut it = Driven::with_par(driver, root, cfg, alloc, par)?;
        it.set_pid(process::id());
        it.open_files()?;
        Ok(it)
    }

    /// The record under `key` read into `out` by one `pread` of its proved segment (no mapping
    /// is touched, nothing is checked); its tag, or `None` when no namespace read holds it.
    pub fn get_into(&mut self, key: &Key, out: &mut Vec<u8>) -> Result<Option<Tag>, StoreError> {
        use std::os::unix::fs::FileExt;
        let Some(found) = self.get(key)? else { return Ok(None) };
        let Some(placed) = self.core.placed(found) else {
            // Put and not yet committed: in memory.
            let got = self.core.got(found);
            out.clear();
            out.extend_from_slice(got.bytes);
            return Ok(Some(got.tag));
        };
        // A segment mapped at this probe is opened for pread now, once.
        self.open_files()?;
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
            self.files.insert(file.share(), opened);
        }
        Ok(())
    }

    /// [`Driven::commit_with`], then the new segments opened for `pread`.
    pub fn commit(&mut self, batches: &[&Batch]) -> Result<Committed<A>, StoreError> {
        self.commit_par(batches, &Serial)
    }

    /// Parallel twin of [`Driven::commit`]: runs `TwoValues` verification, BLAKE3 segment sealing,
    /// and Bloom construction through `par`, then opens the new segments for `pread`.
    pub fn commit_par<P: Par>(&mut self, batches: &[&Batch], par: &P) -> Result<Committed<A>, StoreError> {
        let committed = self.commit_with_par(batches, par)?;
        self.open_files()?;
        Ok(committed)
    }
}

impl<D: Driver, A: Allocator + Clone> Driven<D, A> {
    /// The store `cfg` names, opened on `driver` (whose root is `root`), what it keeps in
    /// `alloc`.
    pub fn with(driver: D, root: &Path, cfg: PlainConfig<A>, alloc: A) -> Result<Driven<D, A>, StoreError> {
        Driven::with_par(driver, root, cfg, alloc, &Serial)
    }

    /// Parallel twin of [`Driven::with`]: verifies large earlier-boot segments through `par`.
    pub fn with_par<P: Par>(driver: D, root: &Path, cfg: PlainConfig<A>, alloc: A, par: &P) -> Result<Driven<D, A>, StoreError> {
        Driven::with_core_par(driver, root, Plain::new(cfg, alloc), par)
    }

    /// [`Driven::with`], its store opened with `plant`.
    #[cfg(test)]
    pub(crate) fn planted(driver: D, root: &Path, cfg: PlainConfig<A>, alloc: A, plant: super::core::Plant) -> Result<Driven<D, A>, StoreError> {
        let mut it = Driven::bare(driver, root, Plain::planted(cfg, alloc, plant));
        it.core.open(&mut it.effects);
        it.opened_par(&Serial)
    }

    /// A power cut where `at` says (`None`: none).
    #[cfg(test)]
    pub(crate) fn cut(&mut self, at: Option<Cut>) {
        self.cut = at;
    }

    /// Suppresses automatic tiered merging on commit for tests that deliberately construct many
    /// unmerged segments to measure multi-segment probing / Bloom skipping.
    #[cfg(test)]
    pub(crate) fn disable_auto_merge(&mut self) {
        self.auto_merge = false;
    }

    fn bare(driver: D, root: &Path, mut core: Plain<A>) -> Driven<D, A> {
        core.set_stream_commits(true);
        Driven {
            driver,
            root: root.to_owned(),
            core,
            effects: Vec::new_in(Global),
            done: Vec::new_in(Global),
            own: Vec::new_in(Global),
            files: BTreeMap::new(),
            #[cfg(test)]
            cut: None,
            #[cfg(test)]
            auto_merge: true,
        }
    }

    fn with_core_par<P: Par>(driver: D, root: &Path, core: Plain<A>, par: &P) -> Result<Driven<D, A>, StoreError> {
        let mut it = Driven::bare(driver, root, core);
        it.core.open(&mut it.effects);
        it.opened_par(par)
    }

    /// The open (asked already) run to its end and its report handed.
    fn opened_par<P: Par>(mut self, par: &P) -> Result<Driven<D, A>, StoreError> {
        let it = &mut self;
        it.run_par("opening the plain store", par)?;
        if !it.core.ready() {
            // Its driver went idle first.
            return Err(StoreError::Unfinished { what: "an open" });
        }
        for lost in &it.core.report().lost {
            blaze_trace::warn!(target: "blaze_cas::plain", file = %lost.file, records = ?lost.records, "lost in a power cut: {}", lost.why);
        }
        // Handed: this open returns with its report ([`Driven::report`]), so the marks of the
        // losses it names may be cleared by this process's next flip (or
        // [`Driven::acknowledge`]). An open that fails hands nothing, and the marks stay for
        // the next.
        it.core.handed();
        it.unroot_holed_par(par)?;
        Ok(self)
    }

    /// D200: before any reader sees the store, every root that reaches a record a power cut
    /// took is dropped, by one flip, so a reader finds a root whole or no root, never a root
    /// with a hole (a lost record is rebuilt as on a first run).
    ///
    /// When: only when this open can have lost something a root reaches: the generation names
    /// a root ([`Plain::names_a_root`]) and an earlier boot's unsealed tail (from the rows, [`Plain::earlier_tail`]), or the open
    /// reported losses (a mark an earlier open left: its unrooting flip did not land). Then
    /// every row is mapped and the tail checked whole (the check its first probe would make
    /// this boot anyway), and only if something is lost are the roots walked
    /// ([`Plain::holed_roots`]: a find for each root, index and named key).
    ///
    /// Crash-safe and idempotent: the unrooting is one flip, which also clears the marks of
    /// the losses this open reported ([`Plain::handed`]), so a kill before it lands leaves the
    /// roots and the marks both, and the next open, seeing the marks, walks and drops again; a
    /// kill after leaves no root and no mark. Never half a root: a root is one manifest row.
    fn unroot_holed_par<P: Par>(&mut self, par: &P) -> Result<(), StoreError> {
        if !self.core.names_a_root() || (!self.core.earlier_tail() && self.core.report().lost.is_empty()) {
            return Ok(());
        }
        self.map_all_par(par)?;
        if self.core.report().lost.is_empty() || self.core.keeps_holed_root() {
            return Ok(());
        }
        let holed = self.core.holed_roots();
        if holed.is_empty() {
            return Ok(());
        }
        for name in holed.iter() {
            blaze_trace::warn!(target: "blaze_cas::plain", root = %name, "a root reached a record lost in a power cut: dropped before any reader saw it (D200); what it named is made again as on a first run");
            self.core.unroot(name);
        }
        self.commit_with_par(&[], par)?;
        Ok(())
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
        self.run_with_par(what, &[], &[], &Serial)
    }

    fn run_par<P: Par>(&mut self, what: &'static str, par: &P) -> Result<(), StoreError> {
        self.run_with_par(what, &[], &[], par)
    }

    fn run_with_par<P: Par>(&mut self, what: &'static str, batches: &[&Batch], records: &[LentRecord<'_>], par: &P) -> Result<(), StoreError> {
        let _span = blaze_trace::info_span!("plain.wait", what).entered();
        let mut since = self.driver.now();
        #[cfg(test)]
        let flipped_from = self.core.generation();
        loop {
            self.driver.submit(&mut self.effects);
            if self.driver.in_flight() == 0 {
                break;
            }
            let stall = Deadline::after(since, STALL);
            let got = self.driver.poll(&mut self.done, stall);
            if got == 0 && stall.passed(self.driver.now()) {
                // Effects in flight and no completion for [`STALL`].
                return Err(StoreError::Stalled { waited_for: what });
            }
            if got > 0 {
                let wall = self.driver.wall_ns();
                for completion in self.done.drain(..) {
                    #[cfg(test)]
                    {
                        let now = match &mut self.cut {
                            Some(Cut::After(0)) => true,
                            Some(Cut::After(left)) => {
                                *left -= 1;
                                false
                            }
                            Some(Cut::AtFlip) => self.core.generation() != flipped_from,
                            None => false,
                        };
                        if now {
                            // The power cut: this completion and the rest, of every run, reach nothing.
                            self.cut = Some(Cut::After(0));
                            return Err(StoreError::Unfinished { what: "a run the test's power cut stopped" });
                        }
                    }
                    if self.core.owns(completion.tag) {
                        self.core.complete_with_par(completion, wall, batches, records, par, &mut self.effects);
                    } else {
                        self.own.push(completion);
                    }
                }
                // Refresh AFTER processing completions so CPU work inside `Plain::complete`
                // (e.g. compaction planning/copying on huge stores) never causes the next newly
                // submitted I/O to start with an already-expired stall deadline.
                since = self.driver.now();
            }
        }
        match self.core.broken() {
            Some(why) => Err(why.clone()),
            None => Ok(()),
        }
    }

    pub fn core(&self) -> &Plain<A> {
        &self.core
    }

    pub(crate) fn core_mut(&mut self) -> &mut Plain<A> {
        &mut self.core
    }

    pub fn driver(&mut self) -> &mut D {
        &mut self.driver
    }

    /// The driver back, the store let go of (a test's simulation, crashed and opened again).
    pub fn into_driver(self) -> D {
        self.driver
    }

    pub fn report(&self) -> &Report {
        self.core.report()
    }

    /// The record under `key` in the namespaces read, found by [`Plain::find`]: a segment
    /// mapped at the first probe that reaches it (at most one a step, each step run to its
    /// completion), an earlier boot's unsealed segment checked whole at its first hit. Read the
    /// bytes with [`Driven::got`]. `None`: absent. The command's main thread reads this way;
    /// pool tasks read through the handle after [`Driven::map_all`].
    pub fn get(&mut self, key: &Key) -> Result<Option<Found>, StoreError> {
        self.get_hinted(key, None)
    }

    /// [`Driven::get`] with an optional segment-generation locality hint (e.g. a manifest root's
    /// generation): probes already-mapped segments first, then the hinted generation's segment,
    /// then remaining segments in probe order (Bloom-filtered).
    pub fn get_hinted(&mut self, key: &Key, hint_gen: Option<u64>) -> Result<Option<Found>, StoreError> {
        let before = self.core.probes().probes;
        let lost = self.core.report().lost.len();
        self.core.set_probe_hint(hint_gen);
        // Bounded: each map is of a row not mapped before, and the generation names few.
        let result = loop {
            let now = self.driver.wall_ns();
            match self.core.find_read(key, now, &mut self.effects) {
                Ok(Probe::Hit(found)) => break Ok(Some(found)),
                Ok(Probe::Absent) => break Ok(None),
                Ok(Probe::Map) => {
                    if let Err(err) = self.run("mapping a segment at its first probe") {
                        break Err(err);
                    }
                }
                Err(err) => break Err(err),
            }
        };
        self.core.set_probe_hint(None);
        let found = result?;
        self.core.count_get(before);
        self.hand_losses(lost);
        Ok(found)
    }

    /// Zero-allocation lookup of root `name` in the current manifest followed by a hinted read of
    /// its root record (probing the root's creation generation before older/larger segments).
    pub fn get_root(&mut self, name: &str) -> Result<Option<Found>, StoreError> {
        let Some((key, generation)) = self.core.root_target(name) else {
            return Ok(None);
        };
        self.get_hinted(&key, Some(generation))
    }

    /// The NEWEST record under `key` in [`Namespace::LOCAL`] ([`Plain::find_newest`]): what a
    /// pointer is read by. Mapped as [`Driven::get`] maps; read the bytes with [`Driven::got`].
    pub fn newest(&mut self, key: &Key) -> Result<Option<Found>, StoreError> {
        let before = self.core.probes().probes;
        let lost = self.core.report().lost.len();
        // Bounded: each map is of a row not mapped before, and the generation names few.
        let found = loop {
            let now = self.driver.wall_ns();
            match self.core.find_newest(Namespace::LOCAL, key, now, &mut self.effects)? {
                Probe::Hit(found) => break Some(found),
                Probe::Absent => break None,
                Probe::Map => self.run("mapping a segment at a pointer's first probe")?,
            }
        };
        self.core.count_get(before);
        self.hand_losses(lost);
        Ok(found)
    }

    /// The bytes of what [`Driven::get`] found, in place, borrowed from the store.
    pub fn got(&self, found: Found) -> Got<'_> {
        self.core.got(found)
    }

    pub fn has(&mut self, key: &Key) -> Result<bool, StoreError> {
        Ok(self.get(key)?.is_some())
    }

    /// Every row mapped and every parked row checked: whole from now on ([`Plain::map_all`]).
    /// The command asks it once, when the request's first pool task is about to start (the
    /// handle cannot map); a commit, a compaction slice, a scrub and a fetch ask it first.
    pub fn map_all(&mut self) -> Result<(), StoreError> {
        self.map_all_par(&Serial)
    }

    /// Parallel twin of [`Driven::map_all`]: verifies large earlier-boot segments through `par`.
    pub fn map_all_par<P: Par>(&mut self, par: &P) -> Result<(), StoreError> {
        if self.core.whole() && self.core.broken().is_none() && self.core.ready() {
            return Ok(());
        }
        let lost = self.core.report().lost.len();
        let now = self.driver.wall_ns();
        if !self.core.map_all_par(now, par, &mut self.effects)? {
            return Ok(());
        }
        if !self.effects.is_empty() {
            self.run_par("mapping every segment named", par)?;
        }
        if !self.core.whole() {
            return Err(StoreError::Unfinished { what: "mapping every segment named" });
        }
        self.hand_losses(lost);
        Ok(())
    }

    /// Losses found after the open (a first probe's check, a map of a file gone): logged and
    /// handed, as the open's are ([`Driven::opened`]).
    fn hand_losses(&mut self, from: usize) {
        let lost = &self.core.report().lost;
        if lost.len() <= from {
            return;
        }
        for lost in &lost[from..] {
            blaze_trace::warn!(target: "blaze_cas::plain", file = %lost.file, records = ?lost.records, "lost in a power cut: {}", lost.why);
        }
        self.core.handed();
    }

    /// `(segments mapped, segments named)`: `--stats` "segments mapped N of M named".
    pub fn mapped_of_named(&self) -> (u32, u32) {
        self.core.mapped_of_named()
    }

    /// What the gets probed: `--stats` "probes per get (mean, most)".
    pub fn probes(&self) -> Probes {
        self.core.probes()
    }

    /// The explicit scrub ([`Plain::scrub`]): every record's CRC-32C against its bytes now,
    /// over a whole store.
    pub fn scrub(&mut self) -> Result<Vec<Corrupt, A>, StoreError> {
        self.map_all()?;
        Ok(self.core.scrub())
    }

    pub fn put(&mut self, key: Key, namespace: Namespace, tag: Tag, value: &[u8]) -> Result<Put, PutError> {
        self.core.put(key, namespace, tag, value)
    }

    pub fn root(&mut self, name: &str, keys: &[Key]) -> Result<Key, StoreError> {
        self.root_names_held(name, keys, &|_| false)?;
        self.core.root(name, keys)
    }

    /// D200's set-time guard, the one home of it: every key a root record names directly is
    /// `own` (put by the caller and not yet handed to this store: a `Lent`'s arena) or found
    /// as [`Driven::get`] finds (put uncommitted here, or committed in a namespace read; a row
    /// mapped at its first probe), else [`StoreError::RootNamesUnheld`] naming the root and
    /// the first such key. A name no one put is a writer's bug, refused here on every boot;
    /// what a power cut lost is the open's ([`Driven::unroot_holed`]).
    ///
    /// Cost, per key: `own` (a `Lent`'s hash lookup), then this store's held batches (a
    /// `BTreeMap` lookup per namespace), then the rows in probe order (a binary search of each
    /// row's sorted entries, until a hit). An indexed root names its indexes alone here; what an
    /// index holds is not looked for. Counted in [`super::PutPath::root_probes`].
    pub(crate) fn root_names_held(&mut self, name: &str, keys: &[Key], own: &dyn Fn(&Key) -> bool) -> Result<(), StoreError> {
        // Bounded before any probe: the bound the root's own setting refuses past
        // (`ROOT_KEYS_MOST` direct records, `ROOT_INDEXES_MOST` indexes: equal).
        if keys.len() > super::core::ROOT_KEYS_MOST {
            return Err(StoreError::IndexOver { keys: keys.len() as u64, most: super::core::ROOT_KEYS_MOST as u64 });
        }
        self.core.count_root_probes(keys.len() as u64);
        for key in keys {
            if own(key) || self.get(key)?.is_some() {
                continue;
            }
            return Err(StoreError::RootNamesUnheld { root: name.to_owned(), key: key.0 });
        }
        Ok(())
    }

    /// [`Driven::root_for`], with `own` the caller's puts not yet handed here
    /// ([`Driven::root_names_held`]).
    pub(crate) fn root_for_owned(&mut self, name: &str, keys: &[Key], life_ns: u64, own: &dyn Fn(&Key) -> bool) -> Result<Key, StoreError> {
        self.root_names_held(name, keys, own)?;
        self.core.root_for(name, keys, life_ns)
    }

    /// [`Driven::root_through`], with `own` the caller's puts not yet handed here.
    pub(crate) fn root_through_owned(&mut self, name: &str, indexes: &[Key], life_ns: u64, own: &dyn Fn(&Key) -> bool) -> Result<Key, StoreError> {
        self.root_names_held(name, indexes, own)?;
        self.core.root_through(name, indexes, life_ns)
    }

    pub fn unroot(&mut self, name: &str) {
        self.core.unroot(name)
    }

    /// Writes the store's puts and `batches` as one segment for each namespace that holds
    /// records, and flips to a generation naming them, driven until done.
    pub fn commit_with(&mut self, batches: &[&Batch]) -> Result<Committed<A>, StoreError> {
        self.commit_records_par(batches, &[], &Serial)
    }

    /// Parallel twin of [`Driven::commit_with`]: runs `TwoValues` verification, BLAKE3 segment
    /// sealing, Bloom construction, and targeted prior-boot segment checks through `par`.
    pub fn commit_with_par<P: Par>(&mut self, batches: &[&Batch], par: &P) -> Result<Committed<A>, StoreError> {
        self.commit_records_par(batches, &[], par)
    }

    /// Maps and verifies ONLY the unmapped/parked local segments whose Bloom filter matches at
    /// least one key being committed in `batches`, `records`, or `self.core.held` (plus any
    /// segment lacking a valid Bloom filter), leaving non-matching prior-boot segments unmapped
    /// and unhashed during foreground commits ([`Plain::map_for_commit_par`]).
    fn map_for_commit_par<P: Par>(&mut self, batches: &[&Batch], records: &[LentRecord<'_>], par: &P) -> Result<(), StoreError> {
        if self.core.whole() && self.core.broken().is_none() && self.core.ready() {
            return Ok(());
        }
        let lost = self.core.report().lost.len();
        let now = self.driver.wall_ns();
        if self.core.map_for_commit_par(batches, records, now, par, &mut self.effects)? && !self.effects.is_empty() {
            self.run_par("mapping commit-matching segments", par)?;
        }
        self.hand_losses(lost);
        Ok(())
    }

    /// Maps ONLY the Bloom-matching segments needed to probe pending `Liveness::Reached` root
    /// edits in [`Plain::reused_par`], falling back to [`Driven::map_all_par`] only when committed
    /// keys still require probing unbloomed `other_roots` (dead-key resurrection / cross-checkout).
    fn map_for_commit_roots_par<P: Par>(&mut self, par: &P) -> Result<(), StoreError> {
        if self.core.whole() && self.core.broken().is_none() && self.core.ready() {
            return Ok(());
        }
        // Bounded to 4 targeted rounds (edited+prior roots -> changed+replaced indexes -> leaf
        // keys -> final check); falls back to `map_all_par` if still not settled.
        for _ in 0..4 {
            let lost = self.core.report().lost.len();
            let now = self.driver.wall_ns();
            match self.core.map_for_commit_roots_par(now, par, &mut self.effects)? {
                CommitRootMap::Ready => {
                    self.hand_losses(lost);
                    return Ok(());
                }
                CommitRootMap::Mapped => {
                    if !self.effects.is_empty() {
                        self.run_par("mapping commit-root segments", par)?;
                    }
                    self.hand_losses(lost);
                }
                CommitRootMap::NeedAll => {
                    self.hand_losses(lost);
                    self.map_all_par(par)?;
                    return Ok(());
                }
            }
        }
        self.map_all_par(par)
    }

    /// [`Driven::commit_with`] with `records` lent where they lie (core.rs
    /// [`Plain::commit_records`]): a `Lent`'s shared arena, copied once, by the layout.
    pub fn commit_records(&mut self, batches: &[&Batch], records: &[LentRecord<'_>]) -> Result<Committed<A>, StoreError> {
        self.commit_records_par(batches, records, &Serial)
    }

    /// Parallel twin of [`Driven::commit_records`]: runs `TwoValues` verification, BLAKE3 segment
    /// sealing, Bloom construction, and targeted prior-boot segment verification through `par`.
    pub fn commit_records_par<P: Par>(&mut self, batches: &[&Batch], records: &[LentRecord<'_>], par: &P) -> Result<Committed<A>, StoreError> {
        // After the answer: only map/verify segments that can conflict with the batch or pending
        // roots being committed (Bloom-filtered), unless `Plant::CommitMapsAll` or emergency
        // auto-tier-merge requires a whole store.
        let writing = self.core.holds_uncommitted() || batches.iter().any(|it| !it.is_empty()) || !records.is_empty();
        let mut auto_folded = false;
        if self.core.needs_whole_for_commit() {
            self.map_all_par(par)?;
        }
        if writing {
            #[cfg(test)]
            let allow_auto = self.auto_merge;
            #[cfg(not(test))]
            let allow_auto = true;
            // Routine tiered merging (`Plain::AUTO_TIER_MERGE_BG` = 24) runs asynchronously in
            // detached `blaze gc --background`. Foreground commits only fold a synchronous slice
            // at the emergency safety ceiling (`Plain::AUTO_TIER_MERGE_FG` = 128) so interactive
            // `blaze check`/`test` commits never stall on multi-hundred-MB tier merges.
            if allow_auto && !self.core.slice_parked() && self.core.needs_auto_tier_merge() {
                self.map_all_par(par)?;
                let now = self.driver.wall_ns();
                if self.core.fold_tiered_slice(now, &mut self.effects)? {
                    self.run_par("auto-merging tiers on commit", par)?;
                }
                auto_folded = self.core.slice_parked();
            } else {
                self.map_for_commit_par(batches, records, par)?;
            }
        }
        if !self.core.whole() {
            self.map_for_commit_roots_par(par)?;
        }
        let now = self.driver.wall_ns();
        self.core.commit_records_par(batches, records, now, par, &mut self.effects)?;
        self.run_with_par("committing", batches, records, par)?;
        if auto_folded {
            let _ = self.core.take_sliced();
        }
        match self.core.take_committed() {
            Some(outcome) => outcome,
            None => Err(StoreError::Unfinished { what: "a commit" }),
        }
    }

    /// Up to `slices` compaction slices until one says there is no more: a pruning or sweeping
    /// slice bounded by [`super::SLICE_BYTES`], a tiered merge by the amortisation (module doc). Lowers the calling thread first ([`lower_this_thread`]): call it after
    /// the command has answered, or from a background helper, never before a build's first read.
    pub fn compact(&mut self, slices: u32) -> Result<Vec<Sliced, A>, StoreError> {
        lower_this_thread()?;
        self.compact_foreground(slices)
    }

    /// [`Driven::compact`] at the calling thread's current priority (no [`lower_this_thread`]):
    /// for foreground `blaze gc` or a caller that already manages thread QoS itself.
    pub fn compact_foreground(&mut self, slices: u32) -> Result<Vec<Sliced, A>, StoreError> {
        self.map_all()?;
        let mut out = Vec::new_in(self.core.alloc().clone());
        for _ in 0..slices {
            if !self.core.compact(&mut self.effects) {
                return Err(StoreError::Busy { phase: self.core.what() });
            }
            self.run("compacting")?;
            let sliced = self.core.take_sliced().ok_or(StoreError::Unfinished { what: "a compaction slice" })?;
            out.push(sliced);
            if !sliced.more || sliced.aborted || (sliced.segments == 0 && sliced.evicted_roots == 0 && sliced.removed == 0 && sliced.orphans == 0) {
                break;
            }
        }
        Ok(out)
    }

    /// Read-only inspection of what a GC pass would reclaim now ([`Plain::plan_gc`]).
    pub fn plan_gc(&mut self) -> Result<GcPlan, StoreError> {
        self.map_all()?;
        let now = self.driver.wall_ns();
        self.core.plan_gc(now)
    }

    /// Whether total committed segment bytes exceed the high watermark of a finite budget.
    pub fn over_budget(&self) -> bool {
        self.core.over_budget()
    }

    /// Total committed segment bytes.
    pub fn total_bytes(&self) -> u64 {
        self.core.bytes()
    }

    /// Configured store byte budget (`u64::MAX` = unlimited).
    pub fn cap_bytes(&self) -> u64 {
        self.core.cap_bytes()
    }

    /// Overrides the store byte budget for this open handle (e.g. `blaze gc --max-gb`).
    pub fn set_cap(&mut self, cap_bytes: u64) {
        self.core.set_cap(cap_bytes);
    }

    /// After the answer: one cleanup slice written with no lock, for the next commit's flip to
    /// name ([`Plain::fold_slice`]), on a lowered thread. Whether one is parked; a slice that
    /// found nothing to do is [`Driven::take_sliced`]'s at once.
    pub fn fold_slice(&mut self) -> Result<bool, StoreError> {
        lower_this_thread()?;
        self.map_all()?;
        if self.core.fold_slice(&mut self.effects)? {
            self.run("writing a cleanup slice")?;
        }
        Ok(self.core.slice_parked())
    }

    /// The last slice's outcome, once: a folded one's after the commit that flipped it.
    pub fn take_sliced(&mut self) -> Option<Sliced> {
        self.core.take_sliced()
    }

    /// [`Plain::root_for`].
    pub fn root_for(&mut self, name: &str, keys: &[Key], life_ns: u64) -> Result<Key, StoreError> {
        self.root_for_owned(name, keys, life_ns, &|_| false)
    }

    /// [`Plain::index`].
    pub fn index(&mut self, keys: &[Key]) -> Result<Key, StoreError> {
        self.core.index(keys)
    }

    /// [`Plain::root_through`].
    pub fn root_through(&mut self, name: &str, indexes: &[Key], life_ns: u64) -> Result<Key, StoreError> {
        self.root_through_owned(name, indexes, life_ns, &|_| false)
    }

    /// [`Plain::hint_replaced_indexes`].
    pub fn hint_replaced_indexes(&mut self, indexes: &[Key]) {
        self.core.hint_replaced_indexes(indexes);
    }

    /// O(1) lease renewal for an already-live root `name` ([`Plain::renew_root`]): checks the
    /// manifest's root entry against the driver's wall clock without probing or hashing held keys.
    pub fn renew_root(&mut self, name: &str, life_ns: u64) -> Renewed {
        let now = self.driver.wall_ns();
        self.core.renew_root(name, life_ns, now)
    }

    /// O(segments + roots + retired) check of whether compaction or GC housekeeping is due at
    /// `now_ns` ([`Plain::needs_housekeeping`]).
    pub fn needs_housekeeping(&self, now_ns: u64) -> bool {
        self.core.needs_housekeeping(now_ns)
    }

    /// Whether detached `blaze gc --background` should be spawned at `now_ns`
    /// ([`Plain::needs_bg_gc`]): fires immediately on urgent triggers and batches routine
    /// unsealed-segment sealing past [`Plain::BG_GC_UNSEALED_SEGMENTS`] / [`Plain::BG_GC_UNSEALED_BYTES`].
    pub fn needs_bg_gc(&self, now_ns: u64) -> bool {
        self.core.needs_bg_gc(now_ns)
    }

    /// [`Plain::index_beside`].
    pub fn index_beside(&mut self, owner: Key, keys: &[Key]) -> Result<Key, StoreError> {
        self.core.index_beside(owner, keys)
    }

    /// [`Plain::release_lease`].
    pub fn release_lease(&mut self) {
        self.core.release_lease()
    }

    /// [`Plain::rearm_lease`], refreshing `opened_ns` to the driver's current wall clock.
    pub fn rearm_lease(&mut self) {
        let now_ns = self.driver.wall_ns();
        self.core.rearm_lease(now_ns)
    }

    /// [`Plain::set_commit_durability`].
    pub fn set_commit_durability(&mut self, durability: CommitDurability) {
        self.core.set_commit_durability(durability)
    }

    /// [`Plain::set_pid`].
    pub fn set_pid(&mut self, pid: u32) {
        self.core.set_pid(pid)
    }

    /// [`Plain::reclaim_dead_leases`].
    pub fn reclaim_dead_leases(&mut self, check: &dyn PidCheck) -> u32 {
        self.core.reclaim_dead_leases(check)
    }

    /// Reads at most [`format::HEAD_LEN`] + 1 bytes of `head` through the driver (zero segment
    /// opens or mmaps, bounded allocation) alongside a path-based `Stat` and returns the
    /// reset-safe [`HeadIdentity`] on disk (`None` when fresh or missing).
    pub fn head_identity_on_disk(&mut self) -> Result<Option<HeadIdentity>, StoreError> {
        let stat_tag = EffectTag(OWN_TAGS);
        let read_tag = EffectTag(OWN_TAGS + 1);
        let head = self.core.head_file().share();
        // `Effect::Stat` always resolves `head` by path (`OP_STATX` on Linux `io_uring`), whereas
        // `Effect::Read` may read from a kept descriptor in `Uring::names` if `head` was
        // unlinked externally (e.g. `blaze clean --store`). Carrying `(dev, ino)` from `Stat`
        // guarantees any replaced `head` inode produces a distinct `HeadIdentity`.
        self.effects.push(Effect::Stat { file: head.share(), tag: stat_tag });
        self.effects.push(Effect::Read { file: head, at: 0, len: format::HEAD_LEN + 1, tag: read_tag });
        self.run("reading the head generation")?;
        let mut head_stat: Option<Option<(u64, u64)>> = None;
        let mut read_bytes = None;
        let mut stat_error = None;
        let mut read_error = None;
        for completion in self.own.drain(..) {
            if completion.tag == stat_tag {
                match completion.done {
                    Done::Stat(meta) => head_stat = Some(meta.filter(|m| !m.dir).map(|m| (m.dev, m.ino))),
                    Done::Failed(error) | Done::Broken { error, .. } => {
                        stat_error.get_or_insert(StoreError::Io { doing: "reading the head generation", file: StoreFile::Head, error: error.0 });
                    }
                    _ => {
                        stat_error.get_or_insert(StoreError::Other { what: "unexpected completion while reading the head generation" });
                    }
                }
            } else if completion.tag == read_tag {
                match completion.done {
                    Done::Read(bytes) => read_bytes = Some(bytes),
                    Done::Failed(error) | Done::Broken { error, .. } => {
                        read_error.get_or_insert(StoreError::Io { doing: "reading the head generation", file: StoreFile::Head, error: error.0 });
                    }
                    _ => {
                        read_error.get_or_insert(StoreError::Other { what: "unexpected completion while reading the head generation" });
                    }
                }
            }
        }
        if let Some(error) = stat_error {
            return Err(error);
        }
        let Some(Some((dev, ino))) = head_stat else {
            return Ok(None);
        };
        if let Some(error) = read_error {
            return Err(error);
        }
        let Some(bytes) = read_bytes else {
            return Err(StoreError::Unfinished { what: "reading the head generation" });
        };
        match format::read_head(&bytes) {
            format::HeadRead::Committed(newest, _, _) => Ok(Some(HeadIdentity {
                generation: newest.generation,
                log: newest.log,
                manifest_at: newest.manifest_at,
                manifest_len: newest.manifest_len,
                manifest_crc: newest.manifest_crc,
                dev,
                ino,
            })),
            format::HeadRead::Fresh { .. } => Ok(None),
            format::HeadRead::Version { found, .. } => Err(StoreError::Format { what: format::HEAD_SLOT, found, wanted: format::FORMAT }),
            format::HeadRead::Refused(why) => Err(why),
        }
    }

    /// Reads only the fixed [`format::HEAD_LEN`] bytes of `head` through the driver (zero segment
    /// opens or mmaps) and returns the current committed generation on disk (`0` when fresh or
    /// missing).
    pub fn head_generation_on_disk(&mut self) -> Result<u64, StoreError> {
        Ok(self.head_identity_on_disk()?.map_or(0, |id| id.generation))
    }

    /// The exit tail sealed ([`Plain::seal`]): a command calls it after it answered, before or
    /// beside [`Driven::compact`], on a lowered thread. One barrier per segment this boot wrote
    /// and none sealed, then one flip; the next boot's open proves them with no byte read.
    /// Whether anything was sealed.
    pub fn seal(&mut self) -> Result<bool, StoreError> {
        lower_this_thread()?;
        self.seal_foreground()
    }

    /// [`Driven::seal`] at the calling thread's current priority (no [`lower_this_thread`]):
    /// for foreground `blaze gc`.
    pub fn seal_foreground(&mut self) -> Result<bool, StoreError> {
        if !self.core.seal(&mut self.effects) {
            return Ok(false);
        }
        self.run("sealing the exit tail")?;
        Ok(true)
    }

    /// One slice of the background pass ([`Plain::scrub_next`]), on a lowered thread: the
    /// helper a command leaves after its answer calls it until it returns `None` (every sealed
    /// segment walked this boot) or the helper expires. A rot it finds is in
    /// [`Driven::report`]'s losses, handed to the caller with the slice.
    pub fn scrub_next(&mut self, budget_bytes: u64) -> Result<Option<Scrubbed>, StoreError> {
        lower_this_thread()?;
        self.map_all()?;
        let Some(slice) = self.core.scrub_next(budget_bytes, &mut self.effects) else { return Ok(None) };
        if !self.core.ready() {
            self.run("recording the background pass")?;
        }
        if slice.rot.is_some() {
            // Handed to the caller with the slice: the next flip clears its mark.
            self.core.handed();
        }
        Ok(Some(slice))
    }

    /// Adds `paths` (the checkout's, the home's, in each spelling the caller knows) to what a
    /// put into a shared namespace is scanned for ([`super::PlainConfig::machine`]).
    pub fn machine(&mut self, paths: &[&Path]) {
        for path in paths {
            self.core.machine(path.as_os_str().as_encoded_bytes());
        }
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
    pub fn fetch_missing(&mut self, source: &mut dyn Source, verifier: &dyn Verifier, namespace: Namespace, keys: &[RecordRef]) -> Result<Vec<u64, A>, StoreError> {
        if !self.core.reads().contains(&namespace) {
            return Err(StoreError::Other { what: "a fetch from a namespace this store does not read: a key is never answered from a namespace the caller did not ask for" });
        }
        // What is held is asked of every row, not of the mapped ones alone.
        self.map_all()?;
        let alloc = self.core.alloc().clone();
        let mut missing: Vec<RecordRef, &A> = Vec::with_capacity_in(keys.len(), &alloc);
        missing.extend(keys.iter().filter(|it| self.core.get_in(&[namespace], &Key(it.key)).is_none()).copied());
        missing.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        missing.dedup_by(|a, b| a.key == b.key);
        if missing.is_empty() {
            drop(missing);
            return Ok(Vec::new_in(alloc));
        }
        let mut found: Vec<Option<Located>, &A> = Vec::with_capacity_in(missing.len(), &alloc);
        found.resize(missing.len(), None);
        source.find(namespace, &missing, &mut found)?;
        let mut segments: BTreeMap<SegmentId, SegmentRef> = BTreeMap::new();
        for it in found.into_iter().flatten() {
            segments.insert(it.segment.digest(), it.segment);
        }
        drop(missing);
        let mut wanted = Vec::with_capacity_in(segments.len(), alloc);
        wanted.extend(segments.into_values());
        self.pull(source, verifier, namespace, &wanted)
    }

    /// Fetches whole segments a source names (a bundle: std for a toolchain, target and cfg
    /// set), each verified and checked as [`Driven::fetch_missing`] says, and names them here.
    pub fn pull(&mut self, source: &mut dyn Source, verifier: &dyn Verifier, namespace: Namespace, wanted: &[SegmentRef]) -> Result<Vec<u64, A>, StoreError> {
        // The adopted segments are mapped as their flip lands: a whole store.
        self.map_all()?;
        let mut adopted = Vec::with_capacity_in(wanted.len(), self.core.alloc().clone());
        let mut ids = Vec::with_capacity_in(wanted.len(), self.core.alloc().clone());
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
            self.effects.push(Effect::WriteUnsynced { file: file.share(), bytes, tag: EffectTag(OWN_TAGS + i as u64) });
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

    /// Marks segments as pushed to a shared bucket: compaction never rewrites them. `ids` in
    /// the store's allocator ([`Plain::alloc`]).
    pub fn publish(&mut self, ids: Vec<u64, A>) -> Result<(), StoreError> {
        self.core.publish(ids, &mut self.effects)?;
        self.run("marking segments published")
    }
}

/// Non-blocking exclusive advisory file lock (`Effect::Lock` / `Effect::Unlock`) over any
/// [`Driver`] (native or simulated), automatically released on drop. Used by `blaze gc` so
/// at most one background GC process runs against a store root at a time without blocking.
pub struct ExclusiveLock<D: Driver> {
    driver: D,
    lock_tag: EffectTag,
    unlock_tag: EffectTag,
    held: bool,
}

impl<D: Driver> ExclusiveLock<D> {
    /// Attempts to acquire an exclusive lock on `file` without waiting. Returns `Ok(Some(lock))`
    /// when acquired, `Ok(None)` when another process already holds it (`Done::Contended`), or
    /// `Err(StoreError)` on I/O failure or stall.
    pub fn try_acquire(mut driver: D, file: FileName) -> Result<Option<ExclusiveLock<D>>, StoreError> {
        let lock_tag = EffectTag(1u64 << 58);
        let unlock_tag = EffectTag((1u64 << 58) + 1);
        let mut effects = core::iter::once(Effect::Lock { file, mode: LockMode::Exclusive, tag: lock_tag }).collect();
        let mut done = core::default::Default::default();
        let mut acquired = None;
        driver.submit(&mut effects);
        let start = driver.now();
        let deadline = Deadline::after(start, Duration::from_secs(5));
        while driver.in_flight() > 0 {
            driver.poll(&mut done, deadline);
            for completion in done.drain(..) {
                if completion.tag != lock_tag {
                    continue;
                }
                match completion.done {
                    Done::Locked => acquired = Some(true),
                    Done::Contended => acquired = Some(false),
                    Done::Failed(error) => {
                        return Err(StoreError::Io { doing: "acquiring exclusive gc lock", file: StoreFile::FlipLock, error: error.0 });
                    }
                    Done::Broken { error, .. } => {
                        return Err(StoreError::Io { doing: "acquiring exclusive gc lock", file: StoreFile::FlipLock, error: error.0 });
                    }
                    _ => return Err(StoreError::Other { what: "unexpected completion while acquiring exclusive gc lock" }),
                }
            }
            if acquired.is_some() || deadline.passed(driver.now()) {
                break;
            }
        }
        match acquired {
            Some(true) => Ok(Some(ExclusiveLock { driver, lock_tag, unlock_tag, held: true })),
            Some(false) => Ok(None),
            None => Err(StoreError::Stalled { waited_for: "acquiring exclusive gc lock" }),
        }
    }
}

impl<D: Driver> Drop for ExclusiveLock<D> {
    fn drop(&mut self) {
        if !self.held {
            return;
        }
        self.held = false;
        let mut effects = core::iter::once(Effect::Unlock { lock: self.lock_tag, tag: self.unlock_tag }).collect();
        let mut done = core::default::Default::default();
        self.driver.submit(&mut effects);
        let deadline = Deadline::after(self.driver.now(), Duration::from_secs(1));
        while self.driver.in_flight() > 0 && !deadline.passed(self.driver.now()) {
            self.driver.poll(&mut done, deadline);
            done.clear();
        }
    }
}

/// Native Unix [`PidCheck`] via `kill(pid, 0)` (outside `#![blaze::sans_io]`, rules 14, 75).
#[derive(Clone, Copy, Default, Debug)]
pub struct OsPidCheck;

impl PidCheck for OsPidCheck {
    fn check_pid(&self, pid: u32) -> PidLiveness {
        #[cfg(unix)]
        {
            let Ok(pid_i32) = libc::pid_t::try_from(pid) else {
                return PidLiveness::Unknown;
            };
            if pid_i32 <= 0 {
                return PidLiveness::Dead;
            }
            // SAFETY: signal 0 performs only a kernel existence/permission check on positive
            // `pid_i32` without delivering a signal or touching user memory.
            let rc = unsafe { libc::kill(pid_i32, 0) };
            if rc == 0 {
                return PidLiveness::Alive;
            }
            match IoError::last_os_error().raw_os_error() {
                Some(libc::EPERM) => PidLiveness::Alive,
                Some(libc::ESRCH) => PidLiveness::Dead,
                _ => PidLiveness::Unknown,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            PidLiveness::Unknown
        }
    }
}

/// Lowers the calling thread for background work: at most utility QoS on macOS
/// (`pthread_set_qos_class_self_np`, preserving `QOS_CLASS_BACKGROUND` if already lower);
/// nice 19 and the idle IO class on Linux, both per thread there. One way: an unprivileged
/// thread cannot raise itself again.
pub fn lower_this_thread() -> Result<(), StoreError> {
    #[cfg(target_vendor = "apple")]
    {
        if let Ok((current, _)) = blaze_core::threads::qos::this_thread()
            && !current.above(blaze_core::threads::qos::Qos::Utility)
        {
            return Ok(());
        }
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

#[cfg(test)]
mod sota10_commit_floor_tests {
    use super::*;
    use super::super::{
        core::{LockStep, Plant, ROOT_LIFE_NS, TIER_SEGMENTS_MOST},
        test_root,
    };
    use blaze_core::sim::{Faults, Sim};
    use blaze_mem::Frame;

    const TAG: Tag = Tag(1);

    fn make_cfg<'a>(n: u64, boot: u64, durable: bool, liveness: Liveness, cap_bytes: u64, root: &'a Frame) -> PlainConfig<&'a Frame> {
        let mut writes = Vec::new_in(root);
        writes.push(Namespace::LOCAL);
        let mut reads = Vec::new_in(root);
        reads.push(Namespace::LOCAL);
        PlainConfig {
            dir: FileName::new("plain").expect("dir"),
            tags: n * TAGS,
            cap_bytes,
            boot,
            nonce: 7_000 + n,
            durable,
            machine: PlainConfig::empty_machine(root),
            writes,
            reads,
            liveness,
            lock_step: LockStep::Backoff,
        }
    }

    fn test_key(n: u32) -> Key {
        Key(ContentId::of("sota10 commit floor test key", &n.to_le_bytes()))
    }

    fn fill_payload(n: u32, dst: &mut [u8]) {
        for (i, slot) in dst.iter_mut().enumerate() {
            *slot = u8::try_from((usize::try_from(n).unwrap_or(0).wrapping_mul(31).wrapping_add(i)) & 0xff).unwrap_or(0);
        }
    }

    /// Coalesced small segments (`<= SMALL_SEGMENT_COALESCE_BYTES`) emit 1 single
    /// `Effect::WriteUnsynced` (0 `Truncate`, 0 `WriteAt` on `seg-*`) matching `seal_segment`
    /// byte-for-byte, and survive `sim.crash()` at every completion step across both durable and
    /// unsynced commits without ever serving partial or corrupt bytes (Rules 26, 43).
    #[test]
    fn coalesced_small_segment_single_write_unsynced_and_crash_recovery_dst() {
        let root = test_root();

        // 1. Effect-shape proof: small commit emits 1 WriteUnsynced, 0 Truncate, 0 WriteAt on seg-*.
        {
            let mut sim = Sim::new(9101, Faults::default());
            let mut plain = Plain::new(make_cfg(1, 1, true, Liveness::Everything, u64::MAX, &root), &root);
            plain.set_stream_commits(true);
            let mut effects = Default::default();
            let mut done = Default::default();
            plain.open_whole(&mut effects);
            loop {
                sim.submit(&mut effects);
                if sim.in_flight() == 0 {
                    break;
                }
                sim.poll(&mut done, Deadline::after(sim.now(), Duration::from_secs(60)));
                let wall = sim.wall_ns();
                for c in done.drain(..) {
                    plain.complete(c, wall, &mut effects);
                }
            }
            let mut buf0 = [0u8; 512];
            let mut buf1 = [0u8; 768];
            fill_payload(1, &mut buf0);
            fill_payload(2, &mut buf1);
            let k0 = test_key(1);
            let k1 = test_key(2);
            plain.put(k0, Namespace::LOCAL, TAG, &buf0).expect("put 0");
            plain.put(k1, Namespace::LOCAL, TAG, &buf1).expect("put 1");

            let wall = sim.wall_ns();
            plain.commit_records(&[], &[], wall, &mut effects).expect("commit start");
            let mut seg_write_unsynced = 0u32;
            let mut seg_truncate = 0u32;
            let mut seg_write_at = 0u32;
            let mut written_seg_bytes: std::vec::Vec<u8> = std::vec::Vec::new();
            loop {
                for eff in &effects {
                    match eff {
                        Effect::WriteUnsynced { file, bytes, .. } if file.as_str().contains("/seg-") => {
                            seg_write_unsynced += 1;
                            written_seg_bytes.extend_from_slice(bytes);
                        }
                        Effect::Truncate { file, .. } if file.as_str().contains("/seg-") => seg_truncate += 1,
                        Effect::WriteAt { file, .. } if file.as_str().contains("/seg-") => seg_write_at += 1,
                        _ => {}
                    }
                }
                sim.submit(&mut effects);
                if sim.in_flight() == 0 {
                    break;
                }
                sim.poll(&mut done, Deadline::after(sim.now(), Duration::from_secs(60)));
                let w = sim.wall_ns();
                for c in done.drain(..) {
                    plain.complete(c, w, &mut effects);
                }
            }
            assert_eq!(seg_write_unsynced, 1, "small segment must coalesce into 1 Effect::WriteUnsynced");
            assert_eq!(seg_truncate, 0, "coalesced small segment must not emit Effect::Truncate");
            assert_eq!(seg_write_at, 0, "coalesced small segment must not emit Effect::WriteAt");
            let committed = plain.take_committed().expect("settled").expect("commit ok");
            assert_eq!(committed.segments.len(), 1);

            // Verify byte-for-byte equality against `format::seal_segment`.
            let refs = [
                (RecordRef { key: k0.0, tag: TAG }, &buf0[..]),
                (RecordRef { key: k1.0, tag: TAG }, &buf1[..]),
            ];
            let mut expected = Default::default();
            let described = format::seal_segment(Namespace::LOCAL, &refs, &mut expected).expect("seal_segment");
            assert_eq!(described.count, 2);
            assert_eq!(written_seg_bytes.as_slice(), expected.as_slice(), "coalesced small segment must match seal_segment byte-for-byte");
            let mut check = Driven::with(sim, Path::new("/sim"), make_cfg(2, 2, true, Liveness::Everything, u64::MAX, &root), &root).expect("reopen");
            assert!(check.scrub().expect("scrub").is_empty());
            let g0 = check.get(&k0).expect("get 0").expect("present 0");
            assert_eq!(check.got(g0).bytes, &buf0[..]);
            let g1 = check.get(&k1).expect("get 1").expect("present 1");
            assert_eq!(check.got(g1).bytes, &buf1[..]);
        }

        // 2. Crash-recovery DST: replay deterministically and crash (`sim.crash()`) at EVERY
        //    completion step of a coalesced small-segment commit across seeds 0..4 and both
        //    durable=true and durable=false.
        for durable in [true, false] {
            for seed in 0..4u64 {
                let mut seed_buf = [0u8; 256];
                fill_payload(100, &mut seed_buf);
                let seed_k = test_key(100);
                let seed_n = u32::try_from(seed).unwrap_or(0);
                let mut payload_a = [0u8; 640];
                let mut payload_b = [0u8; 1024];
                fill_payload(200 + seed_n, &mut payload_a);
                fill_payload(300 + seed_n, &mut payload_b);
                let ka = test_key(200 + seed_n);
                let kb = test_key(300 + seed_n);

                let run_until = |stop_at: Option<u32>| -> (Sim, u32) {
                    let mut w0 = Driven::with(
                        Sim::new(9200 + seed, Faults::default()),
                        Path::new("/sim"),
                        make_cfg(1, 1, true, Liveness::Everything, u64::MAX, &root),
                        &root,
                    )
                    .expect("seed open");
                    w0.put(seed_k, Namespace::LOCAL, TAG, &seed_buf).expect("seed put");
                    w0.commit_with(&[]).expect("seed commit");
                    let mut sim = w0.into_driver();

                    let mut plain = Plain::new(make_cfg(2, 1, durable, Liveness::Everything, u64::MAX, &root), &root);
                    plain.set_stream_commits(true);
                    let mut effects = Default::default();
                    let mut done = Default::default();
                    plain.open_whole(&mut effects);
                    loop {
                        sim.submit(&mut effects);
                        if sim.in_flight() == 0 {
                            break;
                        }
                        sim.poll(&mut done, Deadline::after(sim.now(), Duration::from_secs(60)));
                        let wall = sim.wall_ns();
                        for c in done.drain(..) {
                            plain.complete(c, wall, &mut effects);
                        }
                    }
                    plain.put(ka, Namespace::LOCAL, TAG, &payload_a).expect("put a");
                    plain.put(kb, Namespace::LOCAL, TAG, &payload_b).expect("put b");
                    plain.commit_records(&[], &[], sim.wall_ns(), &mut effects).expect("commit");
                    let mut step = 0u32;
                    'drive: loop {
                        sim.submit(&mut effects);
                        if sim.in_flight() == 0 {
                            break;
                        }
                        sim.poll(&mut done, Deadline::after(sim.now(), Duration::from_secs(60)));
                        let wall = sim.wall_ns();
                        for c in done.drain(..) {
                            plain.complete(c, wall, &mut effects);
                            step += 1;
                            if stop_at == Some(step) {
                                break 'drive;
                            }
                        }
                    }
                    if stop_at.is_none() {
                        assert!(plain.take_committed().expect("settled").is_ok());
                    }
                    (sim, step)
                };

                let (_, total_steps) = run_until(None);
                assert!(total_steps >= 2, "commit must execute multiple completion steps");
                for cut_step in 1..=total_steps {
                    let (mut crashed, _) = run_until(Some(cut_step));
                    crashed.crash();
                    let mut reopened = Driven::with(
                        crashed,
                        Path::new("/sim"),
                        make_cfg(10 + u64::from(cut_step), 2, durable, Liveness::Everything, u64::MAX, &root),
                        &root,
                    )
                    .expect("reopen after crash");
                    assert!(
                        reopened.scrub().expect("scrub after crash").is_empty(),
                        "seed {seed} step {cut_step}: corrupt record after crash"
                    );
                    let base_got = reopened.get(&seed_k).expect("get seed").expect("durable seed must survive crash");
                    assert_eq!(reopened.got(base_got).bytes, &seed_buf[..]);
                    let got_a = reopened.get(&ka).expect("get a").map(|f| reopened.got(f).bytes == &payload_a[..]);
                    let got_b = reopened.get(&kb).expect("get b").map(|f| reopened.got(f).bytes == &payload_b[..]);
                    assert_eq!(got_a, got_b, "seed {seed} step {cut_step}: coalesced segment commit must be atomic (all or none)");
                    if let Some(eq) = got_a {
                        assert!(eq, "seed {seed} step {cut_step}: recovered payload must match written bytes");
                    }
                }
            }
        }
    }

    /// `Liveness::Reached` root-edit commits map ONLY Bloom-matching segments (leaving unrelated
    /// prior-boot segments unmapped and unhashed) while skipping unchanged page indexes and
    /// subtracting replaced prior-index keys before segment probes. `Plant::CommitMapsAll` control
    /// eagerly maps every segment, and resurrecting a dead key still marks its exact segment touched.
    #[test]
    fn reached_root_edit_commit_maps_targeted_without_map_all_and_tracks_resurrection() {
        let root = test_root();
        let mut payload = [0u8; 128];

        let build_store = || -> (Sim, [Key; 20], Key, Key, u64) {
            let mut buf = [0u8; 128];
            let mut w = Driven::with(
                Sim::new(9301, Faults::default()),
                Path::new("/sim"),
                make_cfg(1, 1, false, Liveness::Reached, u64::MAX, &root),
                &root,
            )
            .expect("boot1 open");
            w.disable_auto_merge();

            // 18 unrelated committed segments (must stay unmapped on edit commit).
            for s in 0..18u32 {
                fill_payload(1_000 + s, &mut buf);
                w.put(test_key(1_000 + s), Namespace::LOCAL, TAG, &buf).expect("put unrelated");
                w.commit_with(&[]).expect("commit unrelated");
            }

            // 1 unrooted dead record in its own segment (for resurrection soundness check).
            let dead_k = test_key(50_001);
            fill_payload(50_001, &mut buf);
            w.put(dead_k, Namespace::LOCAL, TAG, &buf).expect("put dead");
            let dead_commit = w.commit_with(&[]).expect("commit dead");
            let dead_seg_id = dead_commit.segments[0];

            // 1 indexed reach root with 20 page indexes (>= ROOT_BLOOM_MIN_INDEXES) + answer root.
            let mut indexes = [test_key(0); 20];
            let mut kept_in_page0 = test_key(0);
            for i in 0..20usize {
                let n = u32::try_from(i).unwrap_or(0);
                let ka = test_key(20_000 + n * 2);
                let kb = test_key(20_000 + n * 2 + 1);
                fill_payload(20_000 + n * 2, &mut buf);
                w.put(ka, Namespace::LOCAL, TAG, &buf).expect("put ka");
                fill_payload(20_000 + n * 2 + 1, &mut buf);
                w.put(kb, Namespace::LOCAL, TAG, &buf).expect("put kb");
                if i == 0 {
                    kept_in_page0 = ka;
                }
                indexes[i] = w.index(&[ka, kb]).expect("index");
            }
            w.root_through("ws.reach", &indexes, ROOT_LIFE_NS).expect("root reach");
            w.root_for("ws.answer", &[kept_in_page0], ROOT_LIFE_NS).expect("root answer");
            w.commit_with(&[]).expect("commit roots");
            let _ = w.seal().expect("seal boot1");
            (w.into_driver(), indexes, kept_in_page0, dead_k, dead_seg_id)
        };

        // 1. Targeted edit commit in boot 2 (Plant::None): replaces 1 of 20 page indexes.
        let (sim_boot1, mut indexes, kept_in_page0, dead_k, dead_seg_id) = build_store();
        let mut w2 = Driven::planted(
            sim_boot1,
            Path::new("/sim"),
            make_cfg(2, 2, false, Liveness::Reached, u64::MAX, &root),
            &root,
            Plant::None,
        )
        .expect("boot2 open");
        w2.disable_auto_merge();
        assert_eq!(w2.mapped_of_named().0, 0, "lazy open maps 0 segments");

        let fresh_k = test_key(60_001);
        fill_payload(60_001, &mut payload);
        w2.put(fresh_k, Namespace::LOCAL, TAG, &payload).expect("put fresh");
        indexes[0] = w2.index(&[kept_in_page0, fresh_k]).expect("new page 0 index");
        w2.root_through("ws.reach", &indexes, ROOT_LIFE_NS).expect("update reach");
        w2.root_for("ws.answer", &[kept_in_page0, fresh_k], ROOT_LIFE_NS).expect("update answer");
        w2.commit_with(&[]).expect("edit commit");

        let (mapped, named) = w2.mapped_of_named();
        assert!(!w2.core().whole(), "normal Liveness::Reached edit commit must not call map_all()");
        assert!(mapped <= 6 && mapped * 2 < named, "targeted root edit must leave unrelated segments unmapped (mapped={mapped}/{named})");
        assert!(w2.core().checks().full <= 6, "targeted root edit must not hash unrelated prior-boot segments (checks={})", w2.core().checks().full);

        // 2. Planted control (`Plant::CommitMapsAll`): forces eager `map_all()` over all segments.
        let (sim_planted, mut indexes_planted, kept_planted, _, _) = build_store();
        let mut w_planted = Driven::planted(
            sim_planted,
            Path::new("/sim"),
            make_cfg(3, 2, false, Liveness::Reached, u64::MAX, &root),
            &root,
            Plant::CommitMapsAll,
        )
        .expect("planted open");
        w_planted.disable_auto_merge();
        w_planted.put(fresh_k, Namespace::LOCAL, TAG, &payload).expect("put fresh planted");
        indexes_planted[0] = w_planted.index(&[kept_planted, fresh_k]).expect("new idx planted");
        w_planted.root_through("ws.reach", &indexes_planted, ROOT_LIFE_NS).expect("update reach planted");
        w_planted.commit_with(&[]).expect("planted commit");
        assert!(w_planted.core().whole(), "Plant::CommitMapsAll control must call map_all()");
        assert_eq!(w_planted.mapped_of_named().0, w_planted.mapped_of_named().1, "CommitMapsAll maps every segment");

        // 3. Dead-key resurrection on a lazy reopen still marks the exact dead segment touched.
        let sim_after_edit = w2.into_driver();
        let mut w3 = Driven::with(
            sim_after_edit,
            Path::new("/sim"),
            make_cfg(4, 2, false, Liveness::Reached, u64::MAX, &root),
            &root,
        )
        .expect("reopen for resurrection");
        w3.disable_auto_merge();
        let res_idx = w3.index(&[kept_in_page0, dead_k]).expect("resurrect index");
        indexes[0] = res_idx;
        w3.root_through("ws.reach", &indexes, ROOT_LIFE_NS).expect("resurrect reach");
        let res = w3.commit_with(&[]).expect("resurrect commit");
        assert_eq!(
            w3.core().segment_touched(dead_seg_id),
            Some(res.generation),
            "resurrecting a dead key from a lazy commit must still mark its exact segment touched"
        );
    }

    /// `needs_bg_gc` gates unsealed-segment spawning behind `BG_GC_UNSEALED_SEGMENTS` /
    /// `BG_GC_UNSEALED_BYTES` while preserving immediate triggers for over-budget, expired roots,
    /// tier overflow (`> K`), and unlinkable retired files (and preserving `needs_housekeeping`
    /// returning true after 1 unsealed segment).
    #[test]
    fn needs_bg_gc_batches_unsealed_segments_and_keeps_urgent_triggers_immediate() {
        let root = test_root();
        let mut payload = [0u8; 128];
        let mut store = Driven::with(
            Sim::new(9401, Faults::default()),
            Path::new("/sim"),
            make_cfg(1, 1, false, Liveness::Reached, 32 << 20, &root),
            &root,
        )
        .expect("open");
        store.disable_auto_merge();

        // Commits 1..=TIER_SEGMENTS_MOST (4) small unsealed segments: `needs_housekeeping` is true,
        // `needs_bg_gc` stays false (no background GC spawn on every edit).
        let mut keys = [test_key(0); TIER_SEGMENTS_MOST + 1];
        for i in 0..TIER_SEGMENTS_MOST {
            let n = u32::try_from(i).unwrap_or(0);
            let k = test_key(70_000 + n);
            keys[i] = k;
            fill_payload(70_000 + n, &mut payload);
            store.put(k, Namespace::LOCAL, TAG, &payload).expect("put");
            store.root_for("keep", &keys[..=i], ROOT_LIFE_NS).expect("root");
            store.release_lease();
            store.commit_with(&[]).expect("commit");
            let now = store.driver().wall_ns();
            assert!(store.needs_housekeeping(now), "needs_housekeeping keeps single-unsealed semantics");
            assert!(!store.needs_bg_gc(now), "needs_bg_gc must not spawn bg GC for {} small unsealed segments", i + 1);
        }

        // Urgent trigger 1: over budget fires immediately even below unsealed count threshold.
        let now = store.driver().wall_ns();
        store.core_mut().set_cap(1);
        assert!(store.needs_bg_gc(now), "over-budget must trigger bg GC immediately");
        store.core_mut().set_cap(32 << 20);
        assert!(!store.needs_bg_gc(now), "back under budget clears bg GC trigger");

        // Urgent trigger 2: expired root fires immediately.
        let expires = store.core().roots_expiring()[0].2;
        assert!(!store.needs_bg_gc(expires - 1), "unexpired root does not trigger bg GC");
        assert!(store.needs_bg_gc(expires), "expired root must trigger bg GC immediately");

        // Urgent trigger 3: tier overflow (> K = 4 in tier 0) fires immediately.
        let k_over = test_key(70_999);
        keys[TIER_SEGMENTS_MOST] = k_over;
        fill_payload(70_999, &mut payload);
        store.put(k_over, Namespace::LOCAL, TAG, &payload).expect("put over K");
        store.root_for("keep", &keys, ROOT_LIFE_NS).expect("root over K");
        store.release_lease();
        store.commit_with(&[]).expect("commit over K");
        let now = store.driver().wall_ns();
        assert!(store.needs_bg_gc(now), "tier overflow (> K) must trigger bg GC immediately");
    }
}

#[cfg(test)]
mod par_commit_and_open_dst_tests {
    use super::*;
    use super::super::{
        core::{LockStep, ROOT_LIFE_NS},
        par::ShuffledPar,
        test_root,
    };
    use blaze_core::sim::{Faults, Sim};
    use blaze_mem::Frame;

    const TAG: Tag = Tag(1);

    fn par_cfg<'a>(n: u64, boot: u64, liveness: Liveness, root: &'a Frame) -> PlainConfig<&'a Frame> {
        let mut writes = Vec::new_in(root);
        writes.push(Namespace::LOCAL);
        let mut reads = Vec::new_in(root);
        reads.push(Namespace::LOCAL);
        PlainConfig {
            dir: FileName::new("plain").expect("dir"),
            tags: n * TAGS,
            cap_bytes: u64::MAX,
            boot,
            nonce: 17_000 + n,
            durable: true,
            machine: PlainConfig::empty_machine(root),
            writes,
            reads,
            liveness,
            lock_step: LockStep::Backoff,
        }
    }

    fn k(n: u32) -> Key {
        Key(ContentId::of("sota12 par dst key", &n.to_le_bytes()))
    }

    fn payload_bytes(seed: u32, len: usize, root: &Frame) -> Vec<u8, &Frame> {
        let mut out = Vec::with_capacity_in(len, root);
        for i in 0..len {
            let b = u8::try_from((usize::try_from(seed).unwrap_or(0).wrapping_mul(37).wrapping_add(i.wrapping_mul(13))) & 0xff).unwrap_or(0);
            out.push(b);
        }
        out
    }

    /// Runs a multi-segment workload exercising EVERY parallel CAS seam:
    /// 1. Commit 1: 4200 x 64B (~268 KiB) via `Batch` -> triggers `seal_entries_par` (>= 128 KiB)
    ///    and `BloomBuilder::build_par_from` (>= 4096 keys, both Stage 1 and Stage 2).
    /// 2. Commit 2: 40 new x 8 KiB (~320 KiB) + 24 already-committed duplicates + 8 intra-batch
    ///    duplicates via `Batch` + indexed reach root + direct root -> triggers `check_committed_par`
    ///    (>= 32 keys), `seal_entries_par`, and `reused_par`.
    /// 3. Crash/reboot (`boot: 1 -> 2`) without sealing + `Driven::with_par` + `map_all_par` ->
    ///    triggers `Verified::check_par` -> `format::check_segment_par` on both >= 128 KiB segments.
    ///
    /// Returns the reopened `Driven` (all segments mapped & verified) and segment IDs committed.
    fn run_workload<'a, P: Par>(
        sim_seed: u64,
        par: &P,
        root: &'a Frame,
    ) -> (Driven<Sim, &'a Frame>, Vec<u64, &'a Frame>) {
        let mut store = Driven::with_par(
            Sim::new(sim_seed, Faults::default()),
            Path::new("/sim"),
            par_cfg(1, 1, Liveness::Reached, root),
            root,
            par,
        )
        .expect("boot 1 open");
        store.disable_auto_merge();

        // Commit 1: 4200 records x 64 bytes = 268,800 bytes (> 128 KiB and >= 4096 keys).
        let mut batch1 = Batch::new(Namespace::LOCAL);
        let mut small = [0u8; 64];
        for i in 0..4200u32 {
            for (j, slot) in small.iter_mut().enumerate() {
                *slot = u8::try_from((usize::try_from(i).unwrap_or(0).wrapping_mul(29).wrapping_add(j)) & 0xff).unwrap_or(0);
            }
            batch1.put(k(i), TAG, &small).expect("batch1 put");
        }
        let c1 = store.commit_with_par(&[&batch1], par).expect("commit 1");
        assert_eq!(c1.segments.len(), 1);

        // Commit 2: 40 new records x 8 KiB (320 KiB) + 24 already-committed duplicates + 8
        // intra-batch duplicates (exercising parallel TwoValues + SkipSame + seal_entries_par).
        let mut batch2a = Batch::new(Namespace::LOCAL);
        let mut batch2b = Batch::new(Namespace::LOCAL);
        let mut root_keys: Vec<Key, &Frame> = Vec::with_capacity_in(64, root);
        for i in 0..40u32 {
            let key = k(10_000 + i);
            let buf = payload_bytes(10_000 + i, 8192, root);
            batch2a.put(key, TAG, buf.as_slice()).expect("batch2a new");
            if i < 8 {
                batch2b.put(key, TAG, buf.as_slice()).expect("batch2b intra-batch dup");
            }
            root_keys.push(key);
        }
        for i in 0..24u32 {
            for (j, slot) in small.iter_mut().enumerate() {
                *slot = u8::try_from((usize::try_from(i).unwrap_or(0).wrapping_mul(29).wrapping_add(j)) & 0xff).unwrap_or(0);
            }
            batch2b.put(k(i), TAG, &small).expect("batch2b committed dup");
            root_keys.push(k(i));
        }
        let c2 = store.commit_with_par(&[&batch2a, &batch2b], par).expect("commit 2");
        assert_eq!(c2.segments.len(), 1);

        // Commit 3: pin an indexed root and a direct root referencing both segments.
        let idx_key = store.index(root_keys.as_slice()).expect("index");
        store.root_through("par.reach", &[idx_key], ROOT_LIFE_NS).expect("root_through");
        store.root_for("par.answer", &[k(0), k(10_000)], ROOT_LIFE_NS).expect("root_for");
        let c3 = store.commit_with_par(&[], par).expect("commit 3 roots");

        let mut seg_ids = Vec::with_capacity_in(3, root);
        seg_ids.extend_from_slice(&c1.segments);
        seg_ids.extend_from_slice(&c2.segments);
        seg_ids.extend_from_slice(&c3.segments);

        // Simulate crash / reboot (boot 1 -> boot 2, unsealed segments) and verify via check_segment_par.
        let mut sim = store.into_driver();
        sim.crash();
        let mut reopened = Driven::with_par(
            sim,
            Path::new("/sim"),
            par_cfg(2, 2, Liveness::Reached, root),
            root,
            par,
        )
        .expect("boot 2 reopen");
        reopened.map_all_par(par).expect("boot 2 map_all_par");
        assert!(reopened.core().checks().full >= 2, "cold boot must verify unsealed segments");
        assert!(reopened.scrub().expect("scrub").is_empty(), "scrub must find zero corrupt records");

        // Spot-check payloads from both large segments.
        let g1 = reopened.get(&k(4199)).expect("get k(4199)").expect("present");
        assert_eq!(reopened.got(g1).bytes.len(), 64);
        let g2 = reopened.get(&k(10_039)).expect("get k(10_039)").expect("present");
        let want2 = payload_bytes(10_039, 8192, root);
        assert_eq!(reopened.got(g2).bytes, want2.as_slice());

        (reopened, seg_ids)
    }

    #[test]
    fn shuffled_par_matches_serial_bit_for_bit_across_large_commits_blooms_roots_and_cold_reopen() {
        let root = test_root();
        let (serial_store, serial_segs) = run_workload(4242, &Serial, &root);
        let serial_gen = serial_store.core().generation();
        let serial_roots = serial_store.core().roots();
        let serial_mapped_count = serial_store.core().mapped_count();
        let mut serial_indexes: Vec<Vec<u8, &Frame>, &Frame> = Vec::with_capacity_in(serial_mapped_count, &root);
        let mut serial_seg_bytes: Vec<Vec<u8, &Frame>, &Frame> = Vec::with_capacity_in(serial_mapped_count, &root);
        for i in 0..serial_mapped_count {
            let mapped = serial_store.core().mapped_at(i);
            let mut run_copy = Vec::with_capacity_in(mapped.run().len(), &root);
            run_copy.extend_from_slice(mapped.run());
            serial_indexes.push(run_copy);
            let mut bytes_copy = Vec::with_capacity_in(mapped.bytes().len(), &root);
            bytes_copy.extend_from_slice(mapped.bytes());
            serial_seg_bytes.push(bytes_copy);
        }
        let serial_sim = serial_store.into_driver();
        let bloom_file = FileName::new("plain/bloom").expect("bloom name");
        let head_file = FileName::new("plain/head").expect("head name");
        let serial_bloom = serial_sim.contents(&bloom_file);
        let serial_head = serial_sim.contents(&head_file);
        assert!(!serial_bloom.is_empty(), "bloom sidecar must be written");

        for shuffle_seed in 0..4u64 {
            let par = ShuffledPar::from_seed(8, 0x507a_1200 + shuffle_seed);
            let (par_store, par_segs) = run_workload(4242, &par, &root);
            assert_eq!(par_segs.as_slice(), serial_segs.as_slice(), "seed {shuffle_seed}: segment IDs must match Serial");
            assert_eq!(par_store.core().generation(), serial_gen, "seed {shuffle_seed}: generation must match Serial");
            assert_eq!(par_store.core().roots(), serial_roots, "seed {shuffle_seed}: roots must match Serial");
            assert_eq!(par_store.core().mapped_count(), serial_mapped_count, "seed {shuffle_seed}: mapped count must match Serial");

            for i in 0..serial_mapped_count {
                let mapped = par_store.core().mapped_at(i);
                assert_eq!(mapped.run(), serial_indexes[i].as_slice(), "seed {shuffle_seed} seg {i}: index bytes must match Serial bit-for-bit");
                assert_eq!(mapped.bytes(), serial_seg_bytes[i].as_slice(), "seed {shuffle_seed} seg {i}: segment bytes must match Serial bit-for-bit");
            }

            let par_sim = par_store.into_driver();
            for &id in serial_segs.iter() {
                let seg_name = FileName::new(&format!("plain/seg-{id:016x}")).expect("seg name");
                assert_eq!(
                    par_sim.contents(&seg_name),
                    serial_sim.contents(&seg_name),
                    "seed {shuffle_seed}: on-disk segment {id:016x} must match Serial bit-for-bit",
                );
            }
            assert_eq!(
                par_sim.contents(&bloom_file),
                serial_bloom,
                "seed {shuffle_seed}: bloom sidecar must match Serial bit-for-bit",
            );
            assert_eq!(
                par_sim.contents(&head_file),
                serial_head,
                "seed {shuffle_seed}: head file must match Serial bit-for-bit",
            );
        }
    }

    #[test]
    fn shuffled_par_two_values_collision_order_matches_serial_deterministically() {
        let root = test_root();

        let seed_base = |sim_seed: u64| -> Driven<Sim, &Frame> {
            let mut store = Driven::with(
                Sim::new(sim_seed, Faults::default()),
                Path::new("/sim"),
                par_cfg(1, 1, Liveness::Everything, &root),
                &root,
            )
            .expect("open");
            let mut base = Batch::new(Namespace::LOCAL);
            for i in 0..64u32 {
                let buf = payload_bytes(i, 128, &root);
                base.put(k(i), TAG, buf.as_slice()).expect("base put");
            }
            store.commit_with(&[&base]).expect("base commit");
            store
        };

        // Case A: multiple Committed collisions + a later ThisProcess collision -> first sorted key wins.
        let build_case_a = || -> (Batch, Batch) {
            let mut sorted_keys: Vec<(Key, u32), &Frame> = Vec::with_capacity_in(48, &root);
            for i in 0..48u32 {
                sorted_keys.push((k(i), i));
            }
            sorted_keys.sort_unstable_by(|a, b| a.0.0.cmp(&b.0.0));
            let mut b1 = Batch::new(Namespace::LOCAL);
            let mut b2 = Batch::new(Namespace::LOCAL);
            for (pos, &(key, id)) in sorted_keys.iter().enumerate() {
                let mut buf = payload_bytes(id, 128, &root);
                if pos == 5 || pos == 22 || pos == 41 {
                    buf[pos % 128] ^= 0xff;
                }
                b1.put(key, TAG, buf.as_slice()).expect("b1 put");
                if pos == 30 {
                    buf[7] ^= 0x5a;
                    b2.put(key, TAG, buf.as_slice()).expect("b2 this-proc clash");
                }
            }
            (b1, b2)
        };

        let (ba1, ba2) = build_case_a();
        let serial_err_a = match seed_base(5101).commit_with_par(&[&ba1, &ba2], &Serial) {
            Err(StoreError::TwoValues(tv)) => tv,
            other => panic!("expected serial TwoValues in Case A, got {other:?}"),
        };
        assert_eq!(serial_err_a.held, Holder::Committed);

        for shuffle_seed in 0..8u64 {
            let par = ShuffledPar::from_seed(8, 0x7000 + shuffle_seed);
            let par_err_a = match seed_base(5101).commit_with_par(&[&ba1, &ba2], &par) {
                Err(StoreError::TwoValues(tv)) => tv,
                other => panic!("seed {shuffle_seed}: expected par TwoValues in Case A, got {other:?}"),
            };
            assert_eq!(par_err_a, serial_err_a, "seed {shuffle_seed}: Case A TwoValues must match Serial");
        }

        // Case B: ThisProcess collision at sorted index 3 BEFORE Committed collisions at 12 and 35.
        let build_case_b = || -> (Batch, Batch) {
            let mut sorted_keys: Vec<(Key, u32), &Frame> = Vec::with_capacity_in(48, &root);
            for i in 0..48u32 {
                sorted_keys.push((k(i), i));
            }
            sorted_keys.sort_unstable_by(|a, b| a.0.0.cmp(&b.0.0));
            let mut b1 = Batch::new(Namespace::LOCAL);
            let mut b2 = Batch::new(Namespace::LOCAL);
            for (pos, &(key, id)) in sorted_keys.iter().enumerate() {
                let mut buf = payload_bytes(id, 128, &root);
                if pos == 12 || pos == 35 {
                    buf[pos % 128] ^= 0xff;
                }
                b1.put(key, TAG, buf.as_slice()).expect("b1 put");
                if pos == 3 {
                    buf[11] ^= 0xa5;
                    b2.put(key, TAG, buf.as_slice()).expect("b2 early this-proc clash");
                }
            }
            (b1, b2)
        };

        let (bb1, bb2) = build_case_b();
        let serial_err_b = match seed_base(5102).commit_with_par(&[&bb1, &bb2], &Serial) {
            Err(StoreError::TwoValues(tv)) => tv,
            other => panic!("expected serial TwoValues in Case B, got {other:?}"),
        };
        assert_eq!(serial_err_b.held, Holder::ThisProcess);

        for shuffle_seed in 0..8u64 {
            let par = ShuffledPar::from_seed(8, 0x8000 + shuffle_seed);
            let par_err_b = match seed_base(5102).commit_with_par(&[&bb1, &bb2], &par) {
                Err(StoreError::TwoValues(tv)) => tv,
                other => panic!("seed {shuffle_seed}: expected par TwoValues in Case B, got {other:?}"),
            };
            assert_eq!(par_err_b, serial_err_b, "seed {shuffle_seed}: Case B TwoValues must match Serial");
        }
    }
}
