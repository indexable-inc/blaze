//! The `UnitGraph` task (docs/v2/plan/design/units.md, "The record"): which crates a request
//! compiles, and with which features and cfgs, as one record (blaze-unit-graph).
//!
//! Inputs, all known before the work: the lock's bytes, the workspace members' facts with the
//! digest of the `Units` record's out (what the workspace layout read, the toolchain's id with
//! it), the target and host descriptions and the ask. Their digest
//! ([`blaze_unit_graph::record::Source`]) is the task's source; the record is valid by its key.
//! Each dependency's manifests are read from its fetched sources' record through the
//! [`Records`] the driver passes (the records of the `Fetch` jobs it was handed, mapped); a
//! package whose record it was not handed is asked of `Fetch` ([`Entry::Need`]), so only what
//! the roots reach is ever fetched. One whose `Fetch` failed this command leaves no graph
//! ([`Entry::NotFetched`]); one whose sources are refused for good, when the graph reaches it,
//! leaves no graph either: [`UnitGraphTaskError::Missing`] names it with its finding.
//!
//! The record's bytes are a function of the graph alone: an edit to a member's manifest that
//! changes no resolution gives byte-equal bytes, so every task keyed by the record's digest is
//! cut off early.

use core::fmt;

use blaze_id::ContentId;
use blaze_manifest::lock::{Lock, LockId};
use blaze_own::{AVec, Alloc};
use blaze_unit_graph::record::{self, NotItsCfg, Source};
use blaze_unit_graph::{Ask, ExternalRecords, LeftOut, Open, Package, Pass, Records, Resolution, Resolver, SourceError, StoreSources, TargetDesc, Text, UnitError};

/// The most passes of the resolution: each pass asks for the packages one level deeper, so the
/// count is the graph's depth (tempo: 9). Past it, [`UnitGraphTaskError::Passes`] (rule 18).
pub const MOST_PASSES: u32 = 256;

/// What the task is handed.
pub struct UnitGraphInputs<'i, 'a> {
    pub lock: &'i [u8],
    /// The digest of what the workspace's layout read (its manifests and listings, in order).
    pub workspace_read: ContentId,
    /// Each member's lock id and facts.
    pub members: &'i [(LockId, Package<'a>)],
    /// Each non-member checkout path package's lock id and facts (from `[patch.*]` or path
    /// dependencies outside `workspace.members`).
    pub path_packages: &'i [(LockId, Package<'a>)],
    pub resolver: Resolver,
    pub target: &'i TargetDesc<'a>,
    pub host: &'i TargetDesc<'a>,
    pub ask: &'i Ask<'a>,
}

/// The task's source: the digest of its inputs, computed before any work (rule 65).
pub fn source(inputs: &UnitGraphInputs<'_, '_>) -> ContentId {
    let mut s = Source::new();
    s.lock(inputs.lock);
    s.workspace(&inputs.workspace_read.0);
    // The toolchain's id is not added here: the `Units` record's out already carries it, and
    // `workspace_read` is that (one home per input, rule 30).
    s.platform("target", inputs.target).platform("host", inputs.host).ask(inputs.ask);
    ContentId(s.finish().0)
}

/// Why the task failed: each names the thing.
#[derive(Debug)]
pub enum UnitGraphTaskError<'a> {
    /// The lock's plan (blaze-fetch's) refused it.
    Source(SourceError<'a>),
    Resolve(UnitError<'a>),
    Cfg(NotItsCfg),
    Passes,
    /// The graph reached packages whose sources are refused for good (a checksum that is not
    /// the lock's, an archive that does not unpack, no manifest): no graph is made from the
    /// rest (blaze-unit-graph's `Pass::Partial`; states.md 35). Each `name version`, sorted
    /// by lock id, with the findings that say why, in the order asked.
    Missing { packages: AVec<'a, Text<'a>>, findings: AVec<'a, SourceError<'a>> },
}

impl fmt::Display for UnitGraphTaskError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnitGraphTaskError::Source(e) => write!(f, "{e}"),
            UnitGraphTaskError::Resolve(e) => write!(f, "{e}"),
            UnitGraphTaskError::Cfg(e) => write!(f, "{e}"),
            UnitGraphTaskError::Passes => write!(f, "the unit graph did not settle in {MOST_PASSES} passes"),
            UnitGraphTaskError::Missing { packages, findings } => {
                write!(f, "the unit graph reached {} package(s) whose sources could not be read, so no graph is made (rule 3):", packages.len())?;
                for (i, package) in packages.iter().enumerate() {
                    write!(f, "{} {package}", if i == 0 { "" } else { "," })?;
                }
                for finding in findings.iter() {
                    write!(f, "; {finding}")?;
                }
                Ok(())
            }
        }
    }
}

/// What the task made: the graph's record bytes (blaze-unit-graph's `record` form), and one
/// finding a dependency whose sources could not be read (not fetched, refused, no manifest),
/// in the order asked: a package the graph never reached (a graph that reaches one is
/// [`UnitGraphTaskError::Missing`]); the task maps each finding to a finding record naming it.
pub struct UnitGraphMade<'a> {
    pub record: AVec<'a, u8>,
    pub findings: AVec<'a, SourceError<'a>>,
    /// The roots' targets left out by `required-features` (also in the record): the task
    /// says each as a note, never quietly (rule 3).
    pub left_out: AVec<'a, LeftOut<'a>>,
}

/// As [`UnitGraphMade`], with the encoded graph record allocated in a separate allocator `'r`
/// (e.g. the global heap so it can be handed without copying) while transient findings and
/// left-out entries live in the session's scratch allocator `'a`.
pub struct UnitGraphMadeIn<'a, 'r> {
    pub record: AVec<'r, u8>,
    pub findings: AVec<'a, SourceError<'a>>,
    pub left_out: AVec<'a, LeftOut<'a>>,
}

/// What one entry of the task answered (drive.md, "For a successor", step 1). The task holds
/// nothing between entries: each replays the resolution from the roots over the records it was
/// handed, so the reach is fetched level by level (tempo: 9 deep).
pub enum Entry<'a> {
    /// The graph, whole: every package it reached was read from its record.
    Made(UnitGraphMade<'a>),
    /// The `Fetch` subjects (record keys) of the packages this level reached whose records it
    /// was not handed, sorted, each once: the task is entered again once they settle.
    Need(AVec<'a, ContentId>),
    /// `count` packages it reached had a `Fetch` that failed this command (`Task::failed`).
    /// No record is kept: an incomplete graph must never stand by its key, so the next command
    /// asks again.
    NotFetched { count: u32 },
}

/// As [`Entry`], with [`UnitGraphMadeIn`] holding the encoded record in `'r`.
pub enum SessionEntry<'a, 'r> {
    Made(UnitGraphMadeIn<'a, 'r>),
    Need(AVec<'a, ContentId>),
    NotFetched { count: u32 },
}

/// The `Fetch` jobs a task was handed that hold no sources, by subject, each sorted.
#[derive(Clone, Copy)]
pub struct Unheld<'s> {
    /// Failed this command, with no record (the network's or the store's fault): no graph.
    pub failed: &'s [ContentId],
    /// Refused for good, a record saying so (a checksum that is not the lock's, an archive
    /// that does not unpack): a graph that reaches it is refused naming it.
    pub refused: &'s [ContentId],
}

/// Stateful multi-wave runner for the `UnitGraph` task.
///
/// Keeps the lock's fetch plan, opened source pack views, laid-out git workspaces, and parsed
/// dependency manifests across `Entry::Need` waves so each crate manifest and git commit
/// workspace is parsed once rather than once per dependency level.
pub struct UnitGraphSession<'l, 'b, 'a> {
    lock: &'l Lock<'a>,
    target: &'l TargetDesc<'a>,
    host: &'l TargetDesc<'a>,
    sources: StoreSources<'l, 'b, 'a, ExternalRecords>,
    res: Resolution<'l, 'a>,
    findings: AVec<'a, SourceError<'a>>,
    not_fetched: u32,
    passes: u32,
    alloc: Alloc<'a>,
}

impl<'l, 'b, 'a> UnitGraphSession<'l, 'b, 'a> {
    /// Initializes a discovery session over `lock` and `inputs` once.
    pub fn new(
        inputs: &UnitGraphInputs<'l, 'a>,
        lock: &'l Lock<'a>,
        open: Open<'b>,
        alloc: Alloc<'a>,
    ) -> Result<UnitGraphSession<'l, 'b, 'a>, UnitGraphTaskError<'a>> {
        let sources = StoreSources::new(lock, ExternalRecords, open, alloc).map_err(UnitGraphTaskError::Source)?;
        let mut res = Resolution::new(lock, inputs.target, inputs.host, inputs.ask, inputs.resolver, alloc);
        for (is_member, (id, package)) in inputs
            .members
            .iter()
            .map(|entry| (true, entry))
            .chain(inputs.path_packages.iter().map(|entry| (false, entry)))
        {
            let package = package.clone();
            match is_member {
                true => res.member(*id, package),
                false => res.package(*id, Some(package)),
            }
            .map_err(UnitGraphTaskError::Resolve)?;
        }
        Ok(UnitGraphSession {
            lock,
            target: inputs.target,
            host: inputs.host,
            sources,
            res,
            findings: AVec::new_in(alloc),
            not_fetched: 0,
            passes: 0,
            alloc,
        })
    }

    /// Advances the session with the records available so far (including any newly fetched in the
    /// last wave) and `unheld`. Returns [`Entry::Need`] when another wave of fetches is required,
    /// [`Entry::Made`] once the graph is complete, or [`Entry::NotFetched`] if a reachable fetch
    /// failed this command.
    pub fn step<R: Records<'b>>(
        &mut self,
        records: &mut R,
        unheld: Unheld<'_>,
    ) -> Result<Entry<'a>, UnitGraphTaskError<'a>> {
        match self.step_encoded(records, unheld, self.alloc)? {
            SessionEntry::Made(made) => Ok(Entry::Made(UnitGraphMade { record: made.record, findings: made.findings, left_out: made.left_out })),
            SessionEntry::Need(needs) => Ok(Entry::Need(needs)),
            SessionEntry::NotFetched { count } => Ok(Entry::NotFetched { count }),
        }
    }

    /// As [`Self::step`], encoding the finished graph record directly into `record_alloc` (e.g.
    /// the global heap so it can be handed without copying) while all transient resolution and
    /// manifest-layout state stays in the session's scratch allocator.
    pub fn step_encoded<'r, R: Records<'b>>(
        &mut self,
        records: &mut R,
        unheld: Unheld<'_>,
        record_alloc: Alloc<'r>,
    ) -> Result<SessionEntry<'a, 'r>, UnitGraphTaskError<'a>> {
        let alloc = self.alloc;
        while self.passes < MOST_PASSES {
            self.passes += 1;
            match self.res.pass().map_err(UnitGraphTaskError::Resolve)? {
                Pass::Done(graph) => {
                    if self.not_fetched > 0 {
                        return Ok(SessionEntry::NotFetched { count: self.not_fetched });
                    }
                    let record = record::encode(&graph, self.target, self.host, record_alloc).map_err(UnitGraphTaskError::Cfg)?;
                    let findings = core::mem::replace(&mut self.findings, AVec::new_in(alloc));
                    return Ok(SessionEntry::Made(UnitGraphMadeIn { record, findings, left_out: graph.left_out }));
                }
                Pass::Partial(partial) => {
                    // A package not fetched this command is asked again by the next one.
                    if self.not_fetched > 0 {
                        return Ok(SessionEntry::NotFetched { count: self.not_fetched });
                    }
                    // Refused for good: no graph is made without them (states.md 35), each named.
                    let mut packages = AVec::with_capacity_in(partial.missing().len(), alloc);
                    for id in partial.missing() {
                        let locked = self.lock.package(*id);
                        let mut named = Text::new_in(alloc);
                        let _ = fmt::Write::write_fmt(&mut named, format_args!("{} {}", locked.name, locked.version));
                        packages.push(named);
                    }
                    let findings = core::mem::replace(&mut self.findings, AVec::new_in(alloc));
                    return Err(UnitGraphTaskError::Missing { packages, findings });
                }
                Pass::Need(needs) => {
                    let mut asks = AVec::new_in(alloc);
                    for id in needs.iter() {
                        let package = match self.sources.package_with(*id, records) {
                            Ok(package) => Some(package),
                            Err(SourceError::NotFetched { key, .. }) if unheld.failed.binary_search(&ContentId(key)).is_ok() => {
                                self.not_fetched += 1;
                                None
                            }
                            Err(SourceError::NotFetched { key, .. }) if unheld.refused.binary_search(&ContentId(key)).is_err() => {
                                asks.push(ContentId(key));
                                continue;
                            }
                            Err(finding) => {
                                self.findings.push(finding);
                                None
                            }
                        };
                        self.res.package(*id, package).map_err(UnitGraphTaskError::Resolve)?;
                    }
                    if !asks.is_empty() {
                        // A git commit's record serves all its packages: one ask a record.
                        asks.sort_unstable();
                        asks.dedup();
                        return Ok(SessionEntry::Need(asks));
                    }
                }
            }
        }
        Err(UnitGraphTaskError::Passes)
    }

    /// Non-empty package subdirectories discovered inside git commit packs during this session,
    /// so downstream world construction can skip re-running git workspace layout.
    pub fn located_git_dirs(&self) -> impl Iterator<Item = (LockId, &str)> {
        self.sources.located_git_dirs()
    }
}

/// Runs one entry of the task over `lock` (the lock `inputs.lock` is the text of). `records`
/// holds the records of the `Fetch` jobs the task was handed; a package whose record it lacks
/// is asked for unless its subject is in `unheld`.
pub fn run<'b, 'a, R: Records<'b>>(
    inputs: &UnitGraphInputs<'_, 'a>,
    lock: &Lock<'a>,
    mut records: R,
    open: Open<'b>,
    unheld: Unheld<'_>,
    alloc: Alloc<'a>,
) -> Result<Entry<'a>, UnitGraphTaskError<'a>> {
    let mut session = UnitGraphSession::new(inputs, lock, open, alloc)?;
    session.step(&mut records, unheld)
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use blaze_fetch::SourceId;
    use blaze_fetch::pack::{Builder, Kind as PackKind, Limits, Mode, PackView};
    use blaze_manifest::layout::{Entry as LayoutEntry, Layout, Need as LayoutNeed, Pass as LayoutPass};
    use blaze_manifest::lock::Lock;
    use blaze_own::Vec;
    use blaze_unit_graph::{Ask, CfgAtom, Package, Resolver, TargetDesc};

    fn make_pack<'a>(sha256: [u8; 32], files: &[(&str, &[u8])], alloc: Alloc<'a>) -> AVec<'a, u8> {
        let total: u64 = files.iter().map(|(_, b)| b.len() as u64).sum();
        let limits = Limits { most_files: 64, most_bytes: 1 << 20, most_path: 256 };
        let mut b = Builder::new(PackKind::Crate, sha256, limits, total, alloc).expect("pack builder");
        for &(path, bytes) in files {
            b.file(path, Mode::File, bytes).expect("pack file");
        }
        b.finish().expect("pack finish")
    }

    fn member_from_toml<'a>(toml_text: &str, alloc: Alloc<'a>) -> Package<'a> {
        let mut layout = Layout::new("", alloc).expect("layout");
        loop {
            match layout.pass().expect("layout pass") {
                LayoutPass::Done(units) => return Package::from_member(&units.members[0], alloc),
                LayoutPass::Need(needs) => {
                    for need in needs.iter() {
                        match need {
                            LayoutNeed::Manifest(path) if path == "Cargo.toml" => layout.manifest(path, Some(toml_text)).expect("manifest"),
                            LayoutNeed::Manifest(path) => layout.manifest(path, None).expect("manifest"),
                            LayoutNeed::Listing(dir) if dir == "src" => {
                                let entries = [LayoutEntry { name: "lib.rs", is_dir: false }];
                                layout.listing(dir, Some(&entries));
                            }
                            LayoutNeed::Listing(dir) if dir.is_empty() => {
                                let entries = [
                                    LayoutEntry { name: "Cargo.toml", is_dir: false },
                                    LayoutEntry { name: "src", is_dir: true },
                                ];
                                layout.listing(dir, Some(&entries));
                            }
                            LayoutNeed::Listing(dir) => layout.listing(dir, None),
                        }
                    }
                }
            }
        }
    }

    /// A 3-level chain (`app -> mid -> leaf`) where `UnitGraphSession` fetches one wave at a
    /// time, verifies each pack record is requested from `Records` at most once across waves,
    /// and produces byte-identical graph bytes to stateless `run`.
    #[test]
    fn stateful_session_parses_each_level_once_and_matches_stateless_run() {
        let alloc = blaze_own::testing::heap().alloc();
        const LOCK_TEXT: &str = "version = 4\n\n\
             [[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"mid\"]\n\n\
             [[package]]\nname = \"mid\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\ndependencies = [\"leaf\"]\n\n\
             [[package]]\nname = \"leaf\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"\n";
        let lock = Lock::read(LOCK_TEXT.as_bytes(), alloc).expect("lock");
        let app_id = lock.named("app").next().expect("app");
        let app_pkg = member_from_toml("[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nmid = \"1\"\n", alloc);

        let mid_toml = b"[package]\nname = \"mid\"\nversion = \"1.0.0\"\nedition = \"2021\"\n[dependencies]\nleaf = \"1\"\n";
        let leaf_toml = b"[package]\nname = \"leaf\"\nversion = \"1.0.0\"\nedition = \"2021\"\n";
        let mid_sha = [0xAA; 32];
        let leaf_sha = [0xBB; 32];
        let mid_src = SourceId::Crate { name: "mid", version: "1.0.0", sha256: mid_sha };
        let leaf_src = SourceId::Crate { name: "leaf", version: "1.0.0", sha256: leaf_sha };
        let mid_pack = make_pack(mid_sha, &[("Cargo.toml", mid_toml), ("src/lib.rs", b"pub fn m() {}\n")], alloc);
        let leaf_pack = make_pack(leaf_sha, &[("Cargo.toml", leaf_toml), ("src/lib.rs", b"pub fn l() {}\n")], alloc);

        let mut cfg = Vec::new_in(alloc);
        cfg.push(CfgAtom::flag("unix", alloc));
        let target = TargetDesc::new("aarch64-apple-darwin", cfg, alloc);
        let mut roots = Vec::new_in(alloc);
        roots.push(app_id);
        let ask = Ask { roots, features: Vec::new_in(alloc), all_features: false, no_default_features: false, tests: false };
        let members = [(app_id, app_pkg)];
        let inputs = UnitGraphInputs {
            lock: LOCK_TEXT.as_bytes(),
            workspace_read: ContentId([1; 32]),
            members: &members,
            path_packages: &[],
            resolver: Resolver::V2,
            target: &target,
            host: &target,
            ask: &ask,
        };

        struct Counting<'p, 'a> {
            handed: AVec<'a, (ContentId, &'p [u8])>,
            reads: AVec<'a, ContentId>,
        }
        impl<'p> Records<'p> for Counting<'p, '_> {
            fn record(&mut self, source: &SourceId<'_>) -> Option<&'p [u8]> {
                let key = source.key();
                let bytes = self.handed.iter().find(|(k, _)| *k == key).map(|(_, b)| *b)?;
                self.reads.push(key);
                Some(bytes)
            }
        }

        let unheld = Unheld { failed: &[], refused: &[] };
        let mut session = UnitGraphSession::new(&inputs, &lock, PackView::open_source_put, alloc).expect("session");
        let mut recs = Counting { handed: Vec::new_in(alloc), reads: Vec::new_in(alloc) };
        // Wave 1: needs `mid`.
        match session.step(&mut recs, unheld).expect("wave 1") {
            Entry::Need(needs) => assert_eq!(needs.as_slice(), &[mid_src.key()]),
            _ => panic!("expected wave 1 Need(mid)"),
        }
        // Wave 2: hand `mid`, needs `leaf`.
        recs.handed.push((mid_src.key(), mid_pack.as_slice()));
        match session.step(&mut recs, unheld).expect("wave 2") {
            Entry::Need(needs) => assert_eq!(needs.as_slice(), &[leaf_src.key()]),
            _ => panic!("expected wave 2 Need(leaf)"),
        }
        // Wave 3: hand `leaf`, finishes.
        recs.handed.push((leaf_src.key(), leaf_pack.as_slice()));
        let session_bytes = match session.step(&mut recs, unheld).expect("wave 3") {
            Entry::Made(made) => made.record,
            _ => panic!("expected wave 3 Made"),
        };
        // Each record was opened/parsed exactly once across all 3 waves.
        assert_eq!(recs.reads.len(), 2, "each pack read once across 3 waves: {:?}", recs.reads);

        // Byte-identical to stateless `run` handed all records up front.
        let mut handed_all = Vec::new_in(alloc);
        handed_all.push((mid_src.key(), mid_pack.as_slice()));
        handed_all.push((leaf_src.key(), leaf_pack.as_slice()));
        let all = Counting { handed: handed_all, reads: Vec::new_in(alloc) };
        let stateless_bytes = match run(&inputs, &lock, all, PackView::open_source_put, unheld, alloc).expect("stateless") {
            Entry::Made(made) => made.record,
            _ => panic!("expected stateless Made"),
        };
        assert_eq!(session_bytes.as_slice(), stateless_bytes.as_slice());
    }
}
