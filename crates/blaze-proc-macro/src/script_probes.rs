//! A wasm build script's `rustc` probe answered by blaze's own front end (end-state.md D193
//! point 2; blaze-build-script probe_check.rs, the sans-IO core; handoff p1-probe-checker.txt).
//!
//! The command's setting hands a run [`ScriptProbes`]: the checker (the front end as a probe
//! asks it) and the store of answers by question key. With it, [`ScriptSandbox::checked_probe`]
//! is the `Route::Rustc` arm: the line read and held to what blaze models
//! (`probe_check::read_line`), the snippet's bytes taken from stdin or the script's view, the
//! child's environment by std's `Command` semantics, and `probe_check::answer`, the one place a
//! probe is answered. A refusal of the line names its argument ([`SpawnRefusal::ProbeLine`]);
//! a checker that cannot say stops the script naming the key and the cause
//! ([`SpawnRefusal::ProbeUnknown`]: never an exit the script could read as "not supported").
//!
//! Without it (`Script::probes` `None`, the default until the real checker has answered the
//! ladder of real probes) the pinned table answers, as before.
//!
//! The run's key holds the checker's identity in place of the table's (script.rs `base`): a
//! new front end, a new list of implemented features or new target definitions run the
//! scripts that probe again (rule 21).

use std::sync::{Arc, Mutex};

use blaze_build_script::probe_check::{self, CheckerId, Child, Line, ProbeChecker, ProbeKey, ProbeRecords, Stop as ProbeStop};
use blaze_build_script::probe_ask::Snippet;
use blaze_own::{AVec, Alloc};
use blaze_wasi::Stop;

use crate::record::{DirStore, FilesError};
use crate::script_sandbox::{Refusal, ScriptSandbox, SpawnRefusal};
use crate::spawn_route::{Decoded, child_env};

/// What answers a run's `rustc` probes when the command's setting turns the checker on.
#[derive(Clone)]
pub struct ScriptProbes {
    /// The front end as a probe asks it (blaze-cli's checker over the engine's check; a test's
    /// fake).
    pub checker: Arc<dyn ProbeChecker + Send + Sync>,
    /// The answers by question key, written once: the build scripts' records in a command.
    pub records: Arc<dyn ProbeStore + Send + Sync>,
}

impl blaze_own::Share for ScriptProbes {}

impl ScriptProbes {
    /// The checker's identity: what the run's key holds in place of the pinned table's.
    pub fn id(&self) -> CheckerId {
        self.checker.id()
    }
}

/// The store of probe answers, shared by the runs of a command (`&self`: each record is
/// written once, so two runs putting one key put equal bytes or one is refused).
pub trait ProbeStore {
    /// The record under `key`, copied into `alloc`, if one was written.
    fn get<'a>(&self, key: &ProbeKey, alloc: Alloc<'a>) -> Result<Option<AVec<'a, u8>>, ProbeStoreError>;
    /// Writes `bytes` under `key`: equal bytes again are no write; other bytes are refused.
    fn put(&self, key: &ProbeKey, bytes: &[u8]) -> Result<(), ProbeStoreError>;
}

/// Why the probe store refused.
#[derive(Debug)]
pub enum ProbeStoreError {
    /// The records' folder refused (record.rs; other bytes under a key among them).
    Files(FilesError),
    /// Another answer is already held under the key (rule 21).
    Twice(ProbeKey),
}

impl std::fmt::Display for ProbeStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeStoreError::Files(error) => write!(f, "the probe records: {error}"),
            ProbeStoreError::Twice(key) => write!(f, "another answer is already held under probe {:02x?} (rule 21)", &key.0.0[..8]),
        }
    }
}

/// The build scripts' records (`DirStore::build_scripts`), `probe/<64 hex of the key>`.
impl ProbeStore for DirStore {
    fn get<'a>(&self, key: &ProbeKey, alloc: Alloc<'a>) -> Result<Option<AVec<'a, u8>>, ProbeStoreError> {
        let held = self.get_probe(&key.0).map_err(ProbeStoreError::Files)?;
        Ok(held.map(|bytes| {
            let mut out = AVec::with_capacity_in(bytes.len(), alloc);
            out.extend_from_slice(&bytes);
            out
        }))
    }

    fn put(&self, key: &ProbeKey, bytes: &[u8]) -> Result<(), ProbeStoreError> {
        self.put_probe(&key.0, bytes).map_err(ProbeStoreError::Files)
    }
}

/// Answers held in memory, for a run without a store folder and for tests; [`Self::len`] is
/// how many records were written.
#[derive(Default)]
pub struct MemProbeStore {
    rows: Mutex<Vec<(ProbeKey, Vec<u8>)>>,
}

impl MemProbeStore {
    /// How many records are held.
    pub fn len(&self) -> usize {
        crate::host::lock(&self.rows).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ProbeStore for MemProbeStore {
    fn get<'a>(&self, key: &ProbeKey, alloc: Alloc<'a>) -> Result<Option<AVec<'a, u8>>, ProbeStoreError> {
        let rows = crate::host::lock(&self.rows);
        Ok(rows.iter().find(|(have, _)| have == key).map(|(_, bytes)| {
            let mut out = AVec::with_capacity_in(bytes.len(), alloc);
            out.extend_from_slice(bytes);
            out
        }))
    }

    fn put(&self, key: &ProbeKey, bytes: &[u8]) -> Result<(), ProbeStoreError> {
        let mut rows = crate::host::lock(&self.rows);
        match rows.iter().find(|(have, _)| have == key) {
            Some((_, held)) if held[..] == *bytes => Ok(()),
            Some(_) => Err(ProbeStoreError::Twice(*key)),
            None => {
                rows.push((*key, bytes.to_vec()));
                Ok(())
            }
        }
    }
}

/// A [`ProbeStore`] as the core's [`ProbeRecords`].
struct Records<'s>(&'s (dyn ProbeStore + Send + Sync));

impl ProbeRecords for Records<'_> {
    type Error = ProbeStoreError;

    fn get<'a>(&self, key: &ProbeKey, alloc: Alloc<'a>) -> Result<Option<AVec<'a, u8>>, ProbeStoreError> {
        self.0.get(key, alloc)
    }

    fn put(&mut self, key: &ProbeKey, bytes: &[u8]) -> Result<(), ProbeStoreError> {
        self.0.put(key, bytes)
    }
}

impl ScriptSandbox {
    /// The spawn of `rustc` with `args` (`args_text` the same words) answered through
    /// `probes`: the child's exit code and its two outputs, or the script stopped by name.
    pub(crate) fn checked_probe(&mut self, probes: &ScriptProbes, args_text: &[&str], spawned: &Decoded, args: &[String]) -> Result<(i32, Vec<u8>, Vec<u8>), Stop> {
        let frame = match blaze_mem::Frame::new(blaze_mem::Tag::Other) {
            Ok(frame) => frame,
            Err(error) => return Err(self.refuse(Refusal::Spawn(SpawnRefusal::Fact { program: "rustc".to_owned(), args: args.to_vec(), why: format!("no frame for the probe's answer: {error}") }))),
        };
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let line = match probe_check::read_line(args_text, alloc) {
            Ok(line) => line,
            Err(refused) => return Err(self.refuse(Refusal::Spawn(SpawnRefusal::ProbeLine { args: args.to_vec(), at: refused.at, why: refused.why }))),
        };
        let owned_source;
        let source: &[u8] = match &line {
            Line::Compile(compile) => match compile.input {
                Snippet::Stdin => spawned.stdin.as_slice(),
                Snippet::File(path) => {
                    let script_cwd;
                    let cwd = match spawned.cwd.as_deref() {
                        Some(cwd) => cwd,
                        None => {
                            script_cwd = self.script.cwd();
                            &script_cwd
                        }
                    };
                    match self.snippet_file(cwd, path)? {
                        Some(bytes) => {
                            owned_source = bytes;
                            &owned_source
                        }
                        // Not in the script's view: as the table's arm refuses it.
                        None => return Err(self.refuse(Refusal::Spawn(SpawnRefusal::RustcArgs { args: args.to_vec() }))),
                    }
                }
            },
            Line::Version { .. } | Line::Print { .. } => &[],
        };
        let env = child_env(&self.script.env, spawned);
        let env_refs: Vec<(&str, &str)> = env.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
        // Mode 2: the child's standard output is piped to the script, which can read it.
        let child = Child { host: &self.script.host, env: &env_refs, stdout_read: spawned.modes[0] == 2 };
        let probe = match probe_check::probe(line, args_text, source, &child, alloc) {
            Ok(probe) => probe,
            Err(refused) => return Err(self.refuse(Refusal::Spawn(SpawnRefusal::ProbeLine { args: args.to_vec(), at: refused.at, why: refused.why }))),
        };
        let mut records = Records(&*probes.records);
        match probe_check::answer(&probe, &*probes.checker, &mut records, alloc) {
            Ok(exit) => Ok((exit.code, exit.stdout.to_vec(), exit.stderr.to_vec())),
            Err(stop) => {
                let (key, unknown) = match &stop {
                    ProbeStop::Unknown { key, .. } => (*key, true),
                    ProbeStop::Read { key, .. } | ProbeStop::Write { key, .. } | ProbeStop::Malformed { key, .. } => (*key, false),
                };
                let why = stop.to_string();
                let refusal = if unknown {
                    SpawnRefusal::ProbeUnknown { key: key.0, args: args.to_vec(), why }
                } else {
                    SpawnRefusal::ProbeRecords { key: key.0, args: args.to_vec(), why }
                };
                Err(self.refuse(Refusal::Spawn(refusal)))
            }
        }
    }
}
