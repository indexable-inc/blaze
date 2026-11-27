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
use blaze_unit_graph::{
    Ask, ExternalRecords, HostUnitAsk, HostUnitKind, LeftOut, Open, Package, Pass, Records, Resolution, Resolver, SourceError, StoreSources, TargetDesc, Text, UnitError, UnitGraph, UnitKind,
};

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
    /// Optional `wasm64-blaze-wasip1` execution platform for self-compiled build scripts and proc
    /// macros. When present, discovery also resolves and fetches any wasm64-only locked packages
    /// reachable from the graph's `BuildScript` and `ProcMacro` units (e.g. `errno` under `rustix`
    /// on Linux) so `<store>/sources/` holds them before `wasm64_compile` runs.
    pub wasm_host: Option<&'i TargetDesc<'a>>,
    pub ask: &'i Ask<'a>,
}

/// The task's source: the digest of its inputs, computed before any work (rule 65).
pub fn source(inputs: &UnitGraphInputs<'_, '_>) -> ContentId {
    let mut s = Source::new();
    s.lock(inputs.lock);
    s.workspace(&inputs.workspace_read.0);
    // The toolchain's id is not added here: the `Units` record's out already carries it, and
    // `workspace_read` is that (one home per input, rule 30).
    s.platform("target", inputs.target).platform("host", inputs.host);
    if let Some(wasm_host) = inputs.wasm_host {
        s.platform("wasm_host", wasm_host);
    }
    s.ask(inputs.ask);
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
    /// Optional encoded `wasm64-blaze-wasip1` host-unit graph record (present when `wasm_host`
    /// was supplied and the host graph had at least one build script or proc macro).
    pub wasm_record: Option<AVec<'a, u8>>,
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
    pub wasm_record: Option<AVec<'r, u8>>,
    pub findings: AVec<'a, SourceError<'a>>,
    pub left_out: AVec<'a, LeftOut<'a>>,
}

/// What one entry of the task answered (drive.md). The task holds
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
    wasm_host: Option<&'l TargetDesc<'a>>,
    resolver: Resolver,
    sources: StoreSources<'l, 'b, 'a, ExternalRecords>,
    res: Resolution<'l, 'a>,
    done_graph: Option<UnitGraph<'a>>,
    wasm_res: Option<Resolution<'l, 'a>>,
    /// Packages requested via `SessionEntry::Need` that have not yet been fed into the active
    /// resolution (`wasm_res` if `Some`, else `res`). Preserving them across cold fetch handoffs
    /// lets re-entry load newly fetched packages without burning an extra resolution pass.
    pending: AVec<'a, LockId>,
    findings: AVec<'a, SourceError<'a>>,
    not_fetched: u32,
    passes: u32,
    wasm_passes: u32,
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
            wasm_host: inputs.wasm_host,
            resolver: inputs.resolver,
            sources,
            res,
            done_graph: None,
            wasm_res: None,
            pending: AVec::new_in(alloc),
            findings: AVec::new_in(alloc),
            not_fetched: 0,
            passes: 0,
            wasm_passes: 0,
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
            SessionEntry::Made(made) => Ok(Entry::Made(UnitGraphMade { record: made.record, wasm_record: made.wasm_record, findings: made.findings, left_out: made.left_out })),
            SessionEntry::Need(needs) => Ok(Entry::Need(needs)),
            SessionEntry::NotFetched { count } => Ok(Entry::NotFetched { count }),
        }
    }

    fn finish_missing(&mut self, missing: &[LockId]) -> Result<SessionEntry<'a, 'static>, UnitGraphTaskError<'a>> {
        let alloc = self.alloc;
        if self.not_fetched > 0 {
            return Ok(SessionEntry::NotFetched { count: self.not_fetched });
        }
        let mut packages = AVec::with_capacity_in(missing.len(), alloc);
        for id in missing {
            let locked = self.lock.package(*id);
            let mut named = Text::new_in(alloc);
            let _ = fmt::Write::write_fmt(&mut named, format_args!("{} {}", locked.name, locked.version));
            packages.push(named);
        }
        let findings = core::mem::replace(&mut self.findings, AVec::new_in(alloc));
        Err(UnitGraphTaskError::Missing { packages, findings })
    }

    /// Loads `needs` from `records` into the active resolution (`wasm_res` if `Some`, else `res`).
    /// Packages still unheld are retained in `self.pending` and their fetch keys returned in `asks`
    /// so cold fetch handoffs do not consume an extra resolution pass on re-entry.
    fn feed_needs<R: Records<'b>>(
        &mut self,
        needs: &[LockId],
        records: &mut R,
        unheld: Unheld<'_>,
    ) -> Result<AVec<'a, ContentId>, UnitGraphTaskError<'a>> {
        let alloc = self.alloc;
        let mut asks = AVec::new_in(alloc);
        let mut still_pending = AVec::new_in(alloc);
        for &id in needs {
            let package = match self.sources.package_with(id, records) {
                Ok(package) => Some(package),
                Err(SourceError::NotFetched { key, .. }) if unheld.failed.binary_search(&ContentId(key)).is_ok() => {
                    self.not_fetched += 1;
                    None
                }
                Err(SourceError::NotFetched { key, .. }) if unheld.refused.binary_search(&ContentId(key)).is_err() => {
                    asks.push(ContentId(key));
                    still_pending.push(id);
                    continue;
                }
                Err(finding) => {
                    self.findings.push(finding);
                    None
                }
            };
            let res = match &mut self.wasm_res {
                Some(wasm_res) => wasm_res,
                None => &mut self.res,
            };
            res.package(id, package).map_err(UnitGraphTaskError::Resolve)?;
        }
        self.pending = still_pending;
        asks.sort_unstable();
        asks.dedup();
        Ok(asks)
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
        if !self.pending.is_empty() {
            let pending = core::mem::replace(&mut self.pending, AVec::new_in(alloc));
            let asks = self.feed_needs(pending.as_slice(), records, unheld)?;
            if !asks.is_empty() {
                return Ok(SessionEntry::Need(asks));
            }
        }
        loop {
            if let Some(wasm_res) = &mut self.wasm_res {
                if self.wasm_passes >= MOST_PASSES {
                    return Err(UnitGraphTaskError::Passes);
                }
                self.wasm_passes += 1;
                match wasm_res.pass().map_err(UnitGraphTaskError::Resolve)? {
                    Pass::Done(wasm_graph) => {
                        if self.not_fetched > 0 {
                            return Ok(SessionEntry::NotFetched { count: self.not_fetched });
                        }
                        if let Some(graph) = self.done_graph.take() {
                            let record = record::encode(&graph, self.target, self.host, record_alloc).map_err(UnitGraphTaskError::Cfg)?;
                            let wasm_record = match self.wasm_host {
                                Some(wasm_desc) => Some(record::encode(&wasm_graph, wasm_desc, wasm_desc, record_alloc).map_err(UnitGraphTaskError::Cfg)?),
                                None => None,
                            };
                            let findings = core::mem::replace(&mut self.findings, AVec::new_in(alloc));
                            return Ok(SessionEntry::Made(UnitGraphMadeIn { record, wasm_record, findings, left_out: graph.left_out }));
                        }
                        return Err(UnitGraphTaskError::Passes);
                    }
                    Pass::Partial(partial) => {
                        if self.not_fetched > 0 {
                            return Ok(SessionEntry::NotFetched { count: self.not_fetched });
                        }
                        return match self.finish_missing(partial.missing()) {
                            Ok(SessionEntry::NotFetched { count }) => Ok(SessionEntry::NotFetched { count }),
                            Err(err) => Err(err),
                            Ok(_) => Err(UnitGraphTaskError::Passes),
                        };
                    }
                    Pass::Need(needs) => {
                        let asks = self.feed_needs(needs.as_slice(), records, unheld)?;
                        if !asks.is_empty() {
                            return Ok(SessionEntry::Need(asks));
                        }
                    }
                }
                continue;
            }
            if self.passes >= MOST_PASSES {
                return Err(UnitGraphTaskError::Passes);
            }
            self.passes += 1;
            match self.res.pass().map_err(UnitGraphTaskError::Resolve)? {
                Pass::Done(graph) => {
                    if self.not_fetched > 0 {
                        return Ok(SessionEntry::NotFetched { count: self.not_fetched });
                    }
                    if let Some(wasm_desc) = self.wasm_host {
                        let mut roots = AVec::new_in(alloc);
                        for (idx, unit) in graph.units.iter().enumerate() {
                            let kind = match unit.kind {
                                UnitKind::BuildScript => HostUnitKind::BuildScript,
                                UnitKind::ProcMacro => HostUnitKind::ProcMacro,
                                _ => continue,
                            };
                            let Ok(uid) = u32::try_from(idx) else { continue };
                            let side = graph.host_unit_side(blaze_unit_graph::UnitId(uid));
                            let mut features = AVec::with_capacity_in(unit.features.len(), alloc);
                            for feat in unit.features.iter() {
                                features.push(Text::from_str_in(feat.as_str(), alloc));
                            }
                            roots.push(HostUnitAsk { package: unit.package, kind, side, features });
                        }
                        if !roots.is_empty() {
                            let mut wasm_res = Resolution::for_host_units(self.lock, wasm_desc, roots, self.resolver, alloc);
                            self.res.move_packages_into(&mut wasm_res).map_err(UnitGraphTaskError::Resolve)?;
                            self.done_graph = Some(graph);
                            self.wasm_res = Some(wasm_res);
                            continue;
                        }
                    }
                    let record = record::encode(&graph, self.target, self.host, record_alloc).map_err(UnitGraphTaskError::Cfg)?;
                    let findings = core::mem::replace(&mut self.findings, AVec::new_in(alloc));
                    return Ok(SessionEntry::Made(UnitGraphMadeIn { record, wasm_record: None, findings, left_out: graph.left_out }));
                }
                Pass::Partial(partial) => {
                    // A package not fetched this command is asked again by the next one.
                    if self.not_fetched > 0 {
                        return Ok(SessionEntry::NotFetched { count: self.not_fetched });
                    }
                    // Refused for good: no graph is made without them (states.md 35), each named.
                    return match self.finish_missing(partial.missing()) {
                        Ok(SessionEntry::NotFetched { count }) => Ok(SessionEntry::NotFetched { count }),
                        Err(err) => Err(err),
                        Ok(_) => Err(UnitGraphTaskError::Passes),
                    };
                }
                Pass::Need(needs) => {
                    let asks = self.feed_needs(needs.as_slice(), records, unheld)?;
                    if !asks.is_empty() {
                        return Ok(SessionEntry::Need(asks));
                    }
                }
            }
        }
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
            wasm_host: None,
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

    /// When `wasm_host` is supplied (`wasm64-blaze-wasip1`), `UnitGraphSession` also discovers
    /// and requests `Fetch` for wasm64-only locked packages reached by build scripts (such as
    /// `errno` under `rustix` on Linux hosts) even though those packages are absent from the host
    /// Linux `UnitGraph`, and `unit_graph_task::source` hashes `wasm_host` (Rule 21).
    #[test]
    fn stateful_session_fetches_wasm64_only_build_script_deps_not_active_on_host() {
        let alloc = blaze_own::testing::heap().alloc();
        const LOCK_TEXT: &str = "version = 4\n\n\
             [[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"shim\"]\n\n\
             [[package]]\nname = \"shim\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"1111111111111111111111111111111111111111111111111111111111111111\"\ndependencies = [\"wasi_only\"]\n\n\
             [[package]]\nname = \"wasi_only\"\nversion = \"0.3.14\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"2222222222222222222222222222222222222222222222222222222222222222\"\n";
        let lock = Lock::read(LOCK_TEXT.as_bytes(), alloc).expect("lock");
        let app_id = lock.named("app").next().expect("app");

        let mut layout = Layout::new("", alloc).expect("layout");
        let app_toml = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n[build-dependencies]\nshim = \"1\"\n";
        let app_pkg = loop {
            match layout.pass().expect("layout pass") {
                LayoutPass::Done(units) => break Package::from_member(&units.members[0], alloc),
                LayoutPass::Need(needs) => {
                    for need in needs.iter() {
                        match need {
                            LayoutNeed::Manifest(path) if path == "Cargo.toml" => layout.manifest(path, Some(app_toml)).expect("manifest"),
                            LayoutNeed::Manifest(path) => layout.manifest(path, None).expect("manifest"),
                            LayoutNeed::Listing(dir) if dir == "src" => {
                                let entries = [LayoutEntry { name: "lib.rs", is_dir: false }];
                                layout.listing(dir, Some(&entries));
                            }
                            LayoutNeed::Listing(dir) if dir.is_empty() => {
                                let entries = [
                                    LayoutEntry { name: "Cargo.toml", is_dir: false },
                                    LayoutEntry { name: "build.rs", is_dir: false },
                                    LayoutEntry { name: "src", is_dir: true },
                                ];
                                layout.listing(dir, Some(&entries));
                            }
                            LayoutNeed::Listing(dir) => layout.listing(dir, None),
                        }
                    }
                }
            }
        };

        let shim_toml = b"[package]\nname = \"shim\"\nversion = \"1.0.0\"\nedition = \"2021\"\n\
            [target.'cfg(target_os = \"wasi\")'.dependencies]\n\
            wasi_only = \"0.3\"\n";
        let wasi_toml = b"[package]\nname = \"wasi_only\"\nversion = \"0.3.14\"\nedition = \"2021\"\n";
        let shim_sha = [0x11; 32];
        let wasi_sha = [0x22; 32];
        let shim_src = SourceId::Crate { name: "shim", version: "1.0.0", sha256: shim_sha };
        let wasi_src = SourceId::Crate { name: "wasi_only", version: "0.3.14", sha256: wasi_sha };
        let shim_pack = make_pack(shim_sha, &[("Cargo.toml", shim_toml), ("src/lib.rs", b"pub fn s() {}\n")], alloc);
        let wasi_pack = make_pack(wasi_sha, &[("Cargo.toml", wasi_toml), ("src/lib.rs", b"pub fn w() {}\n")], alloc);

        let mut linux_cfg = Vec::new_in(alloc);
        linux_cfg.push(CfgAtom::pair("target_os", "linux", alloc));
        let linux = TargetDesc::new("x86_64-unknown-linux-gnu", linux_cfg, alloc);
        let mut wasi_cfg = Vec::new_in(alloc);
        wasi_cfg.push(CfgAtom::pair("target_os", "wasi", alloc));
        let wasi = TargetDesc::new("wasm64-blaze-wasip1", wasi_cfg, alloc);

        let mut roots = Vec::new_in(alloc);
        roots.push(app_id);
        let ask = Ask { roots, features: Vec::new_in(alloc), all_features: false, no_default_features: false, tests: false };
        let members = [(app_id, app_pkg)];
        let inputs_without = UnitGraphInputs {
            lock: LOCK_TEXT.as_bytes(),
            workspace_read: ContentId([1; 32]),
            members: &members,
            path_packages: &[],
            resolver: Resolver::V2,
            target: &linux,
            host: &linux,
            wasm_host: None,
            ask: &ask,
        };
        let inputs_with = UnitGraphInputs { wasm_host: Some(&wasi), ..inputs_without };
        assert_ne!(source(&inputs_without), source(&inputs_with), "wasm_host must enter UnitGraph source key (Rule 21)");

        struct Handed<'p, 'a> {
            handed: AVec<'a, (ContentId, &'p [u8])>,
        }
        impl<'p> Records<'p> for Handed<'p, '_> {
            fn record(&mut self, source: &SourceId<'_>) -> Option<&'p [u8]> {
                let key = source.key();
                self.handed.iter().find(|(k, _)| *k == key).map(|(_, b)| *b)
            }
        }

        let unheld = Unheld { failed: &[], refused: &[] };
        let mut session = UnitGraphSession::new(&inputs_with, &lock, PackView::open_source_put, alloc).expect("session");
        let mut recs = Handed { handed: Vec::new_in(alloc) };
        // Wave 1: host graph needs `shim`.
        match session.step(&mut recs, unheld).expect("wave 1") {
            Entry::Need(needs) => assert_eq!(needs.as_slice(), &[shim_src.key()]),
            _ => panic!("expected wave 1 Need(shim)"),
        }
        // Wave 2: host graph finishes, then wasm64 host-unit pass discovers `wasi_only`!
        recs.handed.push((shim_src.key(), shim_pack.as_slice()));
        match session.step(&mut recs, unheld).expect("wave 2") {
            Entry::Need(needs) => assert_eq!(needs.as_slice(), &[wasi_src.key()]),
            _ => panic!("expected wave 2 Need(wasi_only)"),
        }
        // Wave 3: hand `wasi_only`, session completes with the host UnitGraph (which has no `wasi_only` unit).
        recs.handed.push((wasi_src.key(), wasi_pack.as_slice()));
        let made = match session.step(&mut recs, unheld).expect("wave 3") {
            Entry::Made(made) => made,
            _ => panic!("expected wave 3 Made"),
        };
        let decoded = record::decode(made.record.as_slice(), alloc).expect("decode graph");
        assert!(!decoded.graph.units.iter().any(|u| u.name.as_str() == "wasi_only"));
    }

    /// The wasm64 host-unit discovery phase gets its own `MOST_PASSES` budget independent of the
    /// host phase, so a graph whose host discovery plus WASI-only dependency chain exceeds 256
    /// total waves (each phase within `MOST_PASSES`) still completes cleanly.
    #[test]
    fn stateful_session_wasm_phase_has_independent_pass_budget() {
        let alloc = blaze_own::testing::heap().alloc();
        const LOCK_TEXT: &str = "version = 4\n\n\
             [[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"shim\"]\n\n\
             [[package]]\nname = \"shim\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"1111111111111111111111111111111111111111111111111111111111111111\"\ndependencies = [\"wasi_a\"]\n\n\
             [[package]]\nname = \"wasi_a\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"2222222222222222222222222222222222222222222222222222222222222222\"\ndependencies = [\"wasi_b\"]\n\n\
             [[package]]\nname = \"wasi_b\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"3333333333333333333333333333333333333333333333333333333333333333\"\n";
        let lock = Lock::read(LOCK_TEXT.as_bytes(), alloc).expect("lock");
        let app_id = lock.named("app").next().expect("app");

        let mut layout = Layout::new("", alloc).expect("layout");
        let app_toml = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n[build-dependencies]\nshim = \"1\"\n";
        let app_pkg = loop {
            match layout.pass().expect("layout pass") {
                LayoutPass::Done(units) => break Package::from_member(&units.members[0], alloc),
                LayoutPass::Need(needs) => {
                    for need in needs.iter() {
                        match need {
                            LayoutNeed::Manifest(path) if path == "Cargo.toml" => layout.manifest(path, Some(app_toml)).expect("manifest"),
                            LayoutNeed::Manifest(path) => layout.manifest(path, None).expect("manifest"),
                            LayoutNeed::Listing(dir) if dir == "src" => {
                                let entries = [LayoutEntry { name: "lib.rs", is_dir: false }];
                                layout.listing(dir, Some(&entries));
                            }
                            LayoutNeed::Listing(dir) if dir.is_empty() => {
                                let entries = [
                                    LayoutEntry { name: "Cargo.toml", is_dir: false },
                                    LayoutEntry { name: "build.rs", is_dir: false },
                                    LayoutEntry { name: "src", is_dir: true },
                                ];
                                layout.listing(dir, Some(&entries));
                            }
                            LayoutNeed::Listing(dir) => layout.listing(dir, None),
                        }
                    }
                }
            }
        };

        let shim_sha = [0x11; 32];
        let wasi_a_sha = [0x22; 32];
        let wasi_b_sha = [0x33; 32];
        let shim_src = SourceId::Crate { name: "shim", version: "1.0.0", sha256: shim_sha };
        let wasi_a_src = SourceId::Crate { name: "wasi_a", version: "0.1.0", sha256: wasi_a_sha };
        let wasi_b_src = SourceId::Crate { name: "wasi_b", version: "0.1.0", sha256: wasi_b_sha };
        let shim_pack = make_pack(
            shim_sha,
            &[
                (
                    "Cargo.toml",
                    b"[package]\nname = \"shim\"\nversion = \"1.0.0\"\nedition = \"2021\"\n[target.'cfg(target_os = \"wasi\")'.dependencies]\nwasi_a = \"0.1\"\n",
                ),
                ("src/lib.rs", b"pub fn s() {}\n"),
            ],
            alloc,
        );
        let wasi_a_pack = make_pack(
            wasi_a_sha,
            &[
                (
                    "Cargo.toml",
                    b"[package]\nname = \"wasi_a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nwasi_b = \"0.1\"\n",
                ),
                ("src/lib.rs", b"pub fn a() {}\n"),
            ],
            alloc,
        );
        let wasi_b_pack = make_pack(
            wasi_b_sha,
            &[
                ("Cargo.toml", b"[package]\nname = \"wasi_b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
                ("src/lib.rs", b"pub fn b() {}\n"),
            ],
            alloc,
        );

        let mut linux_cfg = Vec::new_in(alloc);
        linux_cfg.push(CfgAtom::pair("target_os", "linux", alloc));
        let linux = TargetDesc::new("x86_64-unknown-linux-gnu", linux_cfg, alloc);
        let mut wasi_cfg = Vec::new_in(alloc);
        wasi_cfg.push(CfgAtom::pair("target_os", "wasi", alloc));
        let wasi = TargetDesc::new("wasm64-blaze-wasip1", wasi_cfg, alloc);

        let mut roots = Vec::new_in(alloc);
        roots.push(app_id);
        let ask = Ask { roots, features: Vec::new_in(alloc), all_features: false, no_default_features: false, tests: false };
        let members = [(app_id, app_pkg)];
        let inputs = UnitGraphInputs {
            lock: LOCK_TEXT.as_bytes(),
            workspace_read: ContentId([2; 32]),
            members: &members,
            path_packages: &[],
            resolver: Resolver::V2,
            target: &linux,
            host: &linux,
            wasm_host: Some(&wasi),
            ask: &ask,
        };

        struct Handed<'p, 'a> {
            handed: AVec<'a, (ContentId, &'p [u8])>,
        }
        impl<'p> Records<'p> for Handed<'p, '_> {
            fn record(&mut self, source: &SourceId<'_>) -> Option<&'p [u8]> {
                let key = source.key();
                self.handed.iter().find(|(k, _)| *k == key).map(|(_, b)| *b)
            }
        }

        let unheld = Unheld { failed: &[], refused: &[] };
        let mut session = UnitGraphSession::new(&inputs, &lock, PackView::open_source_put, alloc).expect("session");
        // Simulate host discovery consuming `MOST_PASSES - 2` passes before the final host waves:
        // pass MOST_PASSES - 1 asks for `shim`, cold handoff feeds `shim` without consuming a pass,
        // and pass MOST_PASSES finishes the host graph and hands off to `wasm_res` with a fresh budget.
        session.passes = MOST_PASSES - 2;
        let mut recs = Handed { handed: Vec::new_in(alloc) };
        match session.step(&mut recs, unheld).expect("host wave 1") {
            Entry::Need(needs) => assert_eq!(needs.as_slice(), &[shim_src.key()]),
            _ => panic!("expected Need(shim)"),
        }
        recs.handed.push((shim_src.key(), shim_pack.as_slice()));
        // Host finishes on its `MOST_PASSES`-th pass; wasm phase now starts with its own fresh budget
        // and spans 3 passes (Need(wasi_a), Need(wasi_b), Done), exceeding MOST_PASSES in total!
        match session.step(&mut recs, unheld).expect("wasm wave 1") {
            Entry::Need(needs) => assert_eq!(needs.as_slice(), &[wasi_a_src.key()]),
            _ => panic!("expected Need(wasi_a)"),
        }
        recs.handed.push((wasi_a_src.key(), wasi_a_pack.as_slice()));
        match session.step(&mut recs, unheld).expect("wasm wave 2") {
            Entry::Need(needs) => assert_eq!(needs.as_slice(), &[wasi_b_src.key()]),
            _ => panic!("expected Need(wasi_b)"),
        }
        recs.handed.push((wasi_b_src.key(), wasi_b_pack.as_slice()));
        assert!(matches!(session.step(&mut recs, unheld).expect("wasm done"), Entry::Made(_)));
        assert_eq!(session.passes, MOST_PASSES);
        assert_eq!(session.wasm_passes, 3);
    }

    /// Cold fetch handoffs (`Entry::Need` -> re-entry with newly fetched sources) must consume the
    /// exact same resolution pass count as warm runs where every source is already present, so a
    /// 130-deep cold WASI-only dependency chain completes cleanly within `MOST_PASSES` (256).
    #[test]
    fn stateful_session_cold_fetch_handoffs_do_not_consume_resolution_passes() {
        use core::fmt::Write as _;
        const CHAIN_LEN: usize = 130;
        let alloc = blaze_own::testing::heap().alloc();

        let mut lock_text = blaze_own::AString::from_str_in("version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"shim\"]\n\n", alloc);
        let _ = writeln!(
            lock_text,
            "[[package]]\nname = \"shim\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{:064x}\"\ndependencies = [\"w0\"]\n",
            0xaausize
        );
        for i in 0..CHAIN_LEN {
            let _ = writeln!(
                lock_text,
                "[[package]]\nname = \"w{i}\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{:064x}\"",
                i + 1
            );
            if i + 1 < CHAIN_LEN {
                let _ = writeln!(lock_text, "dependencies = [\"w{}\"]", i + 1);
            }
            lock_text.push('\n');
        }
        let lock = Lock::read(lock_text.as_bytes(), alloc).expect("lock");
        let app_id = lock.named("app").next().expect("app");

        let mut layout = Layout::new("", alloc).expect("layout");
        let app_toml = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n[build-dependencies]\nshim = \"1\"\n";
        let app_pkg = loop {
            match layout.pass().expect("layout pass") {
                LayoutPass::Done(units) => break Package::from_member(&units.members[0], alloc),
                LayoutPass::Need(needs) => {
                    for need in needs.iter() {
                        match need {
                            LayoutNeed::Manifest(path) if path == "Cargo.toml" => layout.manifest(path, Some(app_toml)).expect("manifest"),
                            LayoutNeed::Manifest(path) => layout.manifest(path, None).expect("manifest"),
                            LayoutNeed::Listing(dir) if dir == "src" => {
                                let entries = [LayoutEntry { name: "lib.rs", is_dir: false }];
                                layout.listing(dir, Some(&entries));
                            }
                            LayoutNeed::Listing(dir) if dir.is_empty() => {
                                let entries = [
                                    LayoutEntry { name: "Cargo.toml", is_dir: false },
                                    LayoutEntry { name: "build.rs", is_dir: false },
                                    LayoutEntry { name: "src", is_dir: true },
                                ];
                                layout.listing(dir, Some(&entries));
                            }
                            LayoutNeed::Listing(dir) => layout.listing(dir, None),
                        }
                    }
                }
            }
        };

        let mut shim_sha = [0u8; 32];
        shim_sha[31] = 0xaa;
        let shim_key = SourceId::Crate { name: "shim", version: "1.0.0", sha256: shim_sha }.key();
        let shim_pack = make_pack(
            shim_sha,
            &[
                ("Cargo.toml", b"[package]\nname = \"shim\"\nversion = \"1.0.0\"\nedition = \"2021\"\n[target.'cfg(target_os = \"wasi\")'.dependencies]\nw0 = \"0.1\"\n"),
                ("src/lib.rs", b"pub fn s() {}\n"),
            ],
            alloc,
        );
        let mut all_packs: AVec<'_, (ContentId, AVec<'_, u8>)> = AVec::with_capacity_in(CHAIN_LEN + 1, alloc);
        all_packs.push((shim_key, shim_pack));
        for i in 0..CHAIN_LEN {
            let mut sha = [0u8; 32];
            sha[24..].copy_from_slice(&((i + 1) as u64).to_be_bytes());
            let mut name = blaze_own::AString::new_in(alloc);
            let _ = write!(name, "w{i}");
            let key = SourceId::Crate { name: name.as_str(), version: "0.1.0", sha256: sha }.key();
            let mut toml = blaze_own::AString::new_in(alloc);
            let _ = writeln!(toml, "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2021\"", name.as_str());
            if i + 1 < CHAIN_LEN {
                let _ = writeln!(toml, "[dependencies]\nw{} = \"0.1\"", i + 1);
            }
            let pack = make_pack(sha, &[("Cargo.toml", toml.as_bytes()), ("src/lib.rs", b"pub fn f() {}\n")], alloc);
            all_packs.push((key, pack));
        }

        let mut linux_cfg = Vec::new_in(alloc);
        linux_cfg.push(CfgAtom::pair("target_os", "linux", alloc));
        let linux = TargetDesc::new("x86_64-unknown-linux-gnu", linux_cfg, alloc);
        let mut wasi_cfg = Vec::new_in(alloc);
        wasi_cfg.push(CfgAtom::pair("target_os", "wasi", alloc));
        let wasi = TargetDesc::new("wasm64-blaze-wasip1", wasi_cfg, alloc);

        let mut roots = Vec::new_in(alloc);
        roots.push(app_id);
        let ask = Ask { roots, features: Vec::new_in(alloc), all_features: false, no_default_features: false, tests: false };
        let members = [(app_id, app_pkg)];
        let inputs = UnitGraphInputs {
            lock: lock_text.as_bytes(),
            workspace_read: ContentId([3; 32]),
            members: &members,
            path_packages: &[],
            resolver: Resolver::V2,
            target: &linux,
            host: &linux,
            wasm_host: Some(&wasi),
            ask: &ask,
        };

        struct Handed<'p, 'a> {
            handed: AVec<'a, (ContentId, &'p [u8])>,
        }
        impl<'p> Records<'p> for Handed<'p, '_> {
            fn record(&mut self, source: &SourceId<'_>) -> Option<&'p [u8]> {
                let key = source.key();
                self.handed.iter().find(|(k, _)| *k == key).map(|(_, b)| *b)
            }
        }

        let unheld = Unheld { failed: &[], refused: &[] };
        // Cold run: hand one newly requested pack per wave across all 131 waves.
        let mut cold = UnitGraphSession::new(&inputs, &lock, PackView::open_source_put, alloc).expect("cold session");
        let mut cold_recs = Handed { handed: Vec::new_in(alloc) };
        let cold_made = loop {
            match cold.step(&mut cold_recs, unheld).expect("cold step must stay within pass budget") {
                Entry::Made(made) => break made,
                Entry::Need(needs) => {
                    for want in needs.iter() {
                        let (_, pack) = all_packs.iter().find(|(k, _)| k == want).expect("pack exists");
                        cold_recs.handed.push((*want, pack.as_slice()));
                    }
                }
                Entry::NotFetched { .. } => panic!("unexpected NotFetched"),
            }
        };

        // Warm run: hand all 131 packs up front.
        let mut warm = UnitGraphSession::new(&inputs, &lock, PackView::open_source_put, alloc).expect("warm session");
        let mut warm_recs = Handed { handed: Vec::new_in(alloc) };
        for (k, p) in all_packs.iter() {
            warm_recs.handed.push((*k, p.as_slice()));
        }
        let warm_made = match warm.step(&mut warm_recs, unheld).expect("warm step") {
            Entry::Made(made) => made,
            _ => panic!("expected warm Made in one call"),
        };

        assert_eq!(cold.passes, warm.passes, "cold and warm host pass counts must match");
        assert_eq!(cold.wasm_passes, warm.wasm_passes, "cold and warm wasm pass counts must match");
        assert_eq!(cold.wasm_passes, (CHAIN_LEN + 1) as u32);
        assert_eq!(cold_made.record.as_slice(), warm_made.record.as_slice());
        assert_eq!(
            cold_made.wasm_record.as_deref(),
            warm_made.wasm_record.as_deref(),
            "cold and warm wasm_record bytes must match"
        );
    }
}
