//! One run of user code that ends (a build script, a probe): its result from the store when
//! one is there for its inputs as they are now ([`crate::index::find`]), else a spawn in a
//! sandbox that traces, whose result is checked and kept under exactly what it read
//! ([`crate::index::keep`]).
//!
//! What a run writes is its `OUT_DIR`, given empty: what is in it when it ends is read back
//! ([`collect`]), every path checked to be inside it, and kept with what it printed. A run
//! reused writes its `OUT_DIR` back as it was ([`restore`]). A run that failed is not kept:
//! cargo reruns a failed script every build, and so does blaze.
//!
//! With [`Mode::Verify`] a run found in the store runs anyway, and a result that differs
//! from the one kept is an error naming the run: its key misses an input it depends on, or it
//! is not a function of its inputs.

use std::{
    fmt, io,
    path::{Path, PathBuf},
};

use blaze_cas::{Key, Store, Ticket, Unit};
use blaze_core::{Command, Completion, Done, Effect, Exit, Sandbox, Tag, Traced};
use serde::{Deserialize, Serialize};

use crate::{
    RunKey,
    index::{self, FindError, Found, KeepError},
    roots::{NoRoot, RootMap, Rooted, verify},
    rule::{self, CargoRule},
    trace::Inputs,
};

/// What a run's result is, as kept.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Value {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// What its `OUT_DIR` held as it ended, by path in it, sorted.
    pub files: Vec<OutFile>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct OutFile {
    /// `/`-separated, relative, with no `.` or `..`.
    pub path: String,
    pub executable: bool,
    /// `None` for a directory, which is kept even when empty.
    pub bytes: Option<Vec<u8>>,
}

impl Value {
    pub fn succeeded(&self) -> bool {
        self.code == Some(0)
    }

    pub fn encode(&self) -> Vec<u8> {
        // alloc: the bytes the store keeps, encoded after the magic in one buffer
        postcard::to_extend(self, b"blaze v2 run value 1".to_vec())
            .expect("a value is always encodable")
    }

    pub fn decode(bytes: &[u8]) -> Result<Value, String> {
        let rest = bytes
            .strip_prefix(b"blaze v2 run value 1".as_slice())
            .ok_or_else(|| "not a run's value".to_owned())?; // alloc: error path
        postcard::from_bytes(rest).map_err(|error| format!("a run's value: {error}")) // alloc: error path
    }

    fn of(exit: Exit, files: Vec<OutFile>) -> Value {
        Value {
            code: exit.code,
            signal: exit.signal,
            stdout: exit.stdout,
            stderr: exit.stderr,
            files,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    Verify,
}

/// What a run is.
#[derive(Clone, Debug)]
pub struct Spec {
    pub key: RunKey,
    /// Run in a sandbox that traces; its `OUT_DIR`, when it has one, writable in it.
    pub command: Command,
    pub out_dir: Option<PathBuf>,
    pub mode: Mode,
    /// The roots its key is written against ([`crate::roots`]): what it read is kept and
    /// found through them, what it printed is kept ported, and a value whose files embed one
    /// is kept against where that root is.
    pub roots: RootMap,
}

/// Why a run gave no result.
#[derive(Debug)]
pub enum RunError<E> {
    Find(FindError<E>),
    Inputs(E),
    /// Keeping its result failed; `landed` are the tickets of the puts that landed before it
    /// did, readable at once, which the caller waits for as for any put of its own.
    /// `retracted` is the ticket of a record that the run depends on where its roots are,
    /// when one landed among them ([`verify::retraction`]): flushed at once, as any
    /// retraction is.
    Keep {
        error: KeepError,
        landed: Vec<Ticket>,
        retracted: Option<Ticket>,
    },
    /// The store's value is not a run's.
    BadValue(String),
    /// Its sandbox did not trace, or the spawn did not start, or ended oddly.
    Spawn(String),
    /// Reading or writing its `OUT_DIR`.
    OutDir(String),
    /// Verify mode: run again, it gave other than what was kept. `retracted` is the ticket of
    /// the record that the value, verified across checkouts, depends on where its roots are
    /// ([`verify::retraction`]): what was kept was another checkout's, and no checkout is
    /// given it again once the record is durable. `ran` is what this run gave, for a caller
    /// that found the run under another key too (a pure build script's pure key, found and
    /// verified with this one) and must retract or keep that key's value against it as well.
    /// `landed` are the tickets of every put made on the way (the record's, this checkout's
    /// own result's), `retracted` among them.
    Differs {
        key: RunKey,
        what: String,
        retracted: Option<Ticket>,
        ran: Box<Value>,
        landed: Vec<Ticket>,
    },
    /// It succeeded printing what cargo refuses, so what cargo would watch of it is not known:
    /// nothing is kept.
    Output {
        key: RunKey,
        error: blaze_build_script::Error,
    },
    /// It ran (its key missed, or nothing was found for it) and gave other than the result the
    /// store already keeps under the same key, `stored`, for the same inputs read: it read what
    /// its key misses (randomness, the clock, an input not traced), or it is not a function of
    /// its inputs. `what` is the first field that differs, with both values
    /// ([`differs_with_stderr`]); the kept result is never replaced. `landed` and `retracted`
    /// are as [`RunError::Keep`]'s.
    Rerun {
        key: RunKey,
        stored: Key,
        what: String,
        landed: Vec<Ticket>,
        retracted: Option<Ticket>,
    },
    /// What its sandbox traced it reading disagrees with what a check of it reads
    /// ([`crate::script::keyed`]): kept, its result would be under a key no check makes again
    /// (rule 21), so nothing is kept.
    Traced {
        key: RunKey,
        error: blaze_build_script::Error,
    },
}

impl<E: fmt::Debug> fmt::Display for RunError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Find(error) => write!(f, "finding the run's result: {error:?}"),
            RunError::Inputs(error) => write!(f, "hashing what the run read: {error:?}"),
            RunError::Keep { error, .. } => write!(f, "keeping the run's result: {error:?}"),
            RunError::BadValue(why) => write!(f, "the run's kept result: {why}"),
            RunError::Spawn(why) => write!(f, "the run: {why}"),
            RunError::OutDir(why) => write!(f, "its OUT_DIR: {why}"),
            RunError::Differs { key, what, retracted: None, .. } => write!(
                f,
                "verify: the run of {} given {} gave other than the result kept for the same \
                 inputs ({what}): its key misses an input, or it is not a function of its inputs",
                key.code, key.given
            ),
            RunError::Differs { key, what, retracted: Some(_), .. } => write!(
                f,
                "verify: the run of {} given {} gave other than the result verified across \
                 checkouts for the same inputs ({what}): it depends on where its roots are, so it \
                 is recorded as doing so and no checkout is given that result again",
                key.code, key.given
            ),
            RunError::Output { key, error } => {
                write!(
                    f,
                    "the run of {} given {} printed what cargo refuses: {error}",
                    key.code, key.given
                )
            }
            RunError::Rerun { key, stored, what, .. } => write!(
                f,
                "the run of {} given {} gave other than the result kept under the same key {stored:?} for \
                 the same inputs ({what}): it read what its key misses (randomness, the clock, an \
                 input not traced), or it is not a function of its inputs, or blaze's lookup and keep \
                 make different keys for one run, so it ran when its result was kept (rule 21, a bug \
                 in blaze); the kept result stays",
                key.code, key.given
            ),
            RunError::Traced { key, error } => {
                write!(
                    f,
                    "keeping the run of {} given {}: {error}; nothing is kept",
                    key.code, key.given
                )
            }
        }
    }
}

impl<E> RunError<E> {
    /// The tickets of the puts it made before it failed: each is in the store and readable,
    /// so the caller waits for them to be durable as for a run that succeeded.
    pub fn landed(&self) -> &[Ticket] {
        match self {
            RunError::Keep { landed, .. }
            | RunError::Differs { landed, .. }
            | RunError::Rerun { landed, .. } => landed,
            _ => &[],
        }
    }

    /// The ticket of a retraction record it put before it failed, if one landed: the caller
    /// flushes it at once and lets go of what it retracts, as for a run that succeeded.
    pub fn retracted(&self) -> Option<Ticket> {
        match self {
            RunError::Keep { retracted, .. }
            | RunError::Differs { retracted, .. }
            | RunError::Rerun { retracted, .. } => *retracted,
            _ => None,
        }
    }

    fn keep(error: KeepError) -> RunError<E> {
        // alloc: empty, allocates nothing
        RunError::Keep { error, landed: Vec::new(), retracted: None }
    }
}

/// A run in flight, or done.
#[derive(Debug)]
pub struct Run {
    spec: Spec,
    tag: Tag,
    /// In verify mode, what the store had.
    kept: Option<Value>,
    /// In verify mode, the key of the unmarked result when what the store had was verified
    /// across checkouts ([`Found::Hit`]'s `verified`): a rerun that differs retracts it.
    verified: Option<Key>,
}

/// What [`Run::complete`] gave.
#[derive(Debug)]
pub struct Completed {
    pub value: Value,
    /// What it read, by real paths.
    pub traced: Traced,
    /// The tickets of what was kept.
    pub tickets: Vec<Ticket>,
    /// The ticket of a record that the run depends on where its roots are
    /// ([`verify::Kept::retracted`]): durable before the run is reported done.
    pub retracted: Option<Ticket>,
}

/// How a run began.
#[derive(Debug)]
pub enum Begun {
    /// From the store, its `OUT_DIR` written back: done.
    Reused(Value),
    /// Its spawn asked for: [`Run::complete`] takes the spawn's completion.
    Running(Run),
}

impl Run {
    /// Begins `spec`: its result as `inputs` are now, or its spawn as `tag`.
    pub fn begin<I: Inputs>(
        mut spec: Spec,
        store: &Store,
        inputs: &mut I,
        tag: Tag,
        effects: &mut Vec<Effect>,
    ) -> Result<Begun, RunError<I::Error>>
    where
        I::Error: From<NoRoot>,
    {
        let roots = spec.roots.view();
        let found = index::find_held(store, &spec.key, &mut Rooted::new(roots, inputs))
            .map_err(RunError::Find)?;
        let (kept, verified) = match found {
            Found::Hit { value, verified, .. } => {
                let value = Value::decode(value).map_err(RunError::BadValue)?;
                // What it printed, with this run's roots.
                (
                    Some(
                        roots
                            .restored(value)
                            .map_err(|error| RunError::BadValue(error.to_string()))?,
                    ),
                    verified,
                )
            }
            Found::Miss { .. } => (None, None),
        };
        let kept = match (kept, spec.mode) {
            (Some(value), Mode::Normal) => {
                if let Some(out_dir) = &spec.out_dir {
                    restore(out_dir, &value.files).map_err(RunError::OutDir)?;
                }
                return Ok(Begun::Reused(value));
            }
            (kept, _) => kept,
        };
        match &mut spec.command.sandbox {
            Sandbox::On { trace, write, .. } => {
                *trace = true;
                if let Some(out_dir) = &spec.out_dir
                    && !write.contains(out_dir)
                {
                    write.push(out_dir.clone()); // clone: setup, the sandbox's command owns what it may write
                }
            }
            Sandbox::Off => {
                return Err(RunError::Spawn(format!(
                    // alloc: error path
                    "{}: user code runs in a sandbox that traces; this one is off, so what it \
                     reads can't be known and its result can't be kept",
                    spec.command.program.display()
                )));
            }
        }
        if let Some(out_dir) = &spec.out_dir {
            empty(out_dir).map_err(RunError::OutDir)?;
        }
        // alloc: a spawn, at the edge: the effect owns its command, and the run keeps its spec
        effects.push(Effect::Spawn { command: Box::new(spec.command.clone()), tag });
        Ok(Begun::Running(Run { spec, tag, kept, verified }))
    }

    pub fn tag(&self) -> Tag {
        self.tag
    }

    /// Takes the spawn's completion: the result, kept when it succeeded (what `rule` says
    /// cargo would watch is added when it read what can't be traced). `inputs` hash what it
    /// read as it is now, when it has just ended. `keyed` takes out of what was traced what
    /// the run's key has already ([`crate::script::keyed`]), and refuses a trace that disagrees
    /// with what a check of the run reads ([`RunError::Traced`]); what it read in its own
    /// `OUT_DIR`, given empty, it wrote itself, and is no input.
    ///
    /// It is kept written against its roots, marked with every root
    /// ([`crate::roots::Roots::kept`]), and given to other checkouts only once one ran it and
    /// got the same ([`crate::roots::verify`]): a miss that finds another checkout's copy
    /// under the same unmarked trace compares the two here, as [`Mode::Verify`] does. A rerun
    /// in [`Mode::Verify`] of a value verified across checkouts that gives otherwise records
    /// that the run depends on where its roots are ([`verify::retraction`]), keeps its own
    /// value here, and is [`RunError::Differs`] with the record's ticket.
    pub fn complete<I: Inputs>(
        self,
        completion: Completion,
        store: &mut Store,
        inputs: &mut I,
        rule: impl FnOnce(&Exit) -> Result<CargoRule, blaze_build_script::Error>,
        keyed: impl FnOnce(&mut Traced) -> Result<(), blaze_build_script::Error>,
    ) -> Result<Completed, RunError<I::Error>>
    where
        I::Error: From<NoRoot>,
    {
        assert_eq!(completion.tag, self.tag, "a run takes its own completion");
        let mut exit = match completion.done {
            Done::Exited(exit) => *exit,
            Done::Failed(error) => {
                // alloc: error path
                return Err(RunError::Spawn(format!(
                    "{}: {error}",
                    self.spec.command.program.display()
                )));
            }
            other => return Err(RunError::Spawn(format!("a spawn completed as {other:?}"))), // alloc: error path
        };
        let Some(mut traced) = exit.traced.take() else {
            return Err(RunError::Spawn(format!(
                // alloc: error path
                "{}: its sandbox traces, and its exit has no record of what it read",
                self.spec.command.program.display()
            )));
        };
        keyed(&mut traced).map_err(|error| RunError::Traced { key: self.spec.key, error })?;
        if let Some(out_dir) = &self.spec.out_dir {
            traced.files.retain(|it| !it.0.starts_with(out_dir));
            traced.dirs.retain(|it| !it.0.starts_with(out_dir));
        }
        let files = match &self.spec.out_dir {
            Some(out_dir) => collect(out_dir).map_err(RunError::OutDir)?,
            None => Vec::new(), // alloc: none, an empty list
        };
        let key = self.spec.key;
        let rule = (exit.code == Some(0))
            .then(|| rule(&exit))
            .transpose()
            .map_err(|error| RunError::Output { key, error })?;
        let value = Value::of(exit, files);
        // One comparison for verify mode, stderr included: the one sharing makes
        // ([`ported_differs`]), so a rerun that differs only in stderr is a difference said
        // here, never a silent retraction in `verify::keep` or a mismatch of its own key.
        let differed = self.kept.as_ref().and_then(|kept| differs_with_stderr(kept, &value));
        if let Some(what) = differed {
            let Some(unmarked) = self.verified else {
                // alloc: error path, what the rerun gave
                // alloc: none, no puts
                return Err(RunError::Differs {
                    key,
                    what,
                    retracted: None,
                    ran: Box::new(value),
                    landed: Vec::new(),
                });
            };
            // What it read, widened by cargo's rule before the record is put: that can fail
            // (an input it can't read), and nothing is put yet then.
            if let Some(rule) = &rule {
                rule::widen(&mut traced, rule, inputs).map_err(RunError::Inputs)?;
            }
            // Verified across checkouts, and this one gets otherwise: what was verified is
            // another checkout's, and no checkout is given it again.
            let record = verify::retraction(&key, &unmarked);
            let mut landed = Vec::new(); // alloc: error path, the tickets of what it puts
            if let Err(error) = index::keep_into(
                store,
                Unit::SHARED,
                &record.run,
                &record.traced,
                &record.value,
                &mut landed,
            ) {
                // The record's value may have landed before its put failed: its ticket is
                // the first, and it is a retraction all the same.
                let retracted = landed.first().copied();
                return Err(RunError::Keep { error, landed, retracted });
            }
            // Bound as soon as it is put: the record is in the store, so its ticket goes out
            // with every error from here on, and the job waits for it to be durable.
            let retracted = landed.first().copied();
            // This checkout's own, here alone, so the next run here finds it. Failing to keep
            // it is said in the error, which still carries the record's ticket.
            let what = match rule {
                None => what,
                Some(_) => {
                    let roots = self.spec.roots.view();
                    let kept = roots
                        .kept(&traced, &value)
                        .map_err(|error| KeepError::Scan { run: key, why: error.to_string() });
                    let put = kept.and_then(|(marked, stored)| {
                        index::keep_into(
                            store,
                            Unit::SHARED,
                            &key,
                            &marked.local,
                            &stored.encode(),
                            &mut landed,
                        )
                    });
                    match put {
                        Ok(()) => what,
                        Err(error) => format!("{what}; and keeping its own result here: {error:?}"), // alloc: error path
                    }
                }
            };
            // alloc: error path, what the rerun gave
            return Err(RunError::Differs { key, what, retracted, ran: Box::new(value), landed });
        }
        let Some(rule) = rule else {
            // alloc: none, no tickets
            return Ok(Completed { value, traced, tickets: Vec::new(), retracted: None });
        };
        rule::widen(&mut traced, &rule, inputs).map_err(RunError::Inputs)?;
        let roots = self.spec.roots.view();
        // alloc: error path, why the scan could not be made
        let (marked, stored) = roots.kept(&traced, &value).map_err(|error| {
            RunError::keep(KeepError::Scan { run: key, why: error.to_string() })
        })?;
        let kept = verify::keep(store, &roots, &key, marked, stored.encode(), ported_differs)
            .map_err(|it| match it.error {
                // The store keeps another value under this run's key and trace: said by what
                // differs, both values named, never by the two hashes alone (rule 19).
                KeepError::Mismatch(mismatch) => {
                    let what = rerun_differs(store, &mismatch, &stored).unwrap_or_else(|why| {
                        // alloc: error path
                        format!(
                            "its value hashes {}, not {} as kept; {why}",
                            mismatch.given, mismatch.kept
                        )
                    });
                    RunError::Rerun {
                        key,
                        stored: mismatch.key,
                        what,
                        landed: it.tickets,
                        retracted: it.retracted,
                    }
                }
                error => RunError::Keep { error, landed: it.tickets, retracted: it.retracted },
            })?;
        // Verify mode: a copy comparison that retracted is a difference too, said as one.
        if self.spec.mode == Mode::Verify
            && let Some(ticket) = kept.retracted
        {
            let what = match kept.verdict {
                verify::Verdict::Depends { what } => what,
                other => format!("{other:?}"), // alloc: error path
            };
            // alloc: error path, what the rerun gave
            return Err(RunError::Differs {
                key,
                what,
                retracted: Some(ticket),
                ran: Box::new(value),
                landed: kept.tickets,
            });
        }
        Ok(Completed { value, traced, tickets: kept.tickets, retracted: kept.retracted })
    }
}

/// How the value the store keeps under `mismatch`'s key differs from `ran` (as kept: ported),
/// the value a rerun gave: [`differs_with_stderr`], both values named. An error saying why
/// when the kept bytes are not a run's value, or are equal to it after all.
fn rerun_differs(
    store: &Store,
    mismatch: &blaze_cas::Mismatch,
    ran: &Value,
) -> Result<String, String> {
    let entry =
        store.get(&mismatch.key).map_err(|error| format!("reading the kept value: {error:?}"))?; // alloc: error path
    let Some(entry) = entry else {
        return Err("the kept value is no longer in the store".to_owned()); // alloc: error path
    };
    let bytes = index::split_verified(entry.bytes).map_or(entry.bytes, |(_, value)| value);
    let kept = Value::decode(bytes)?;
    differs_with_stderr(&kept, ran).ok_or_else(|| {
        "the kept value decodes equal to the rerun's, its bytes otherwise (an encoding that is not one to one)".to_owned() // alloc: error path
    })
}

/// [`differs`], and what it printed to stderr too: the comparison sharing makes, since a value
/// verified across checkouts is given as it is.
pub fn differs_with_stderr(kept: &Value, ran: &Value) -> Option<String> {
    differs(kept, ran).or_else(|| {
        (kept.stderr != ran.stderr).then(|| {
            let line = first_line_differing(&kept.stderr, &ran.stderr);
            let [kept, ran] = [&kept.stderr, &ran.stderr].map(|it| shown_line(it, line));
            format!("what it printed to stderr differs at line {line}: {ran}, not {kept}") // alloc: a difference said
        })
    })
}

/// How another checkout's copy of a run's value, `kept`, differs from this one's, `ran`, both
/// as kept (ported, encoded): [`differs`], and what it printed to stderr too, since a value
/// verified across checkouts is given as it is. An error for bytes that are not a value.
pub fn ported_differs(kept: &[u8], ran: &[u8]) -> Result<Option<String>, String> {
    let (kept, ran) = (Value::decode(kept)?, Value::decode(ran)?);
    Ok(differs_with_stderr(&kept, &ran))
}

/// The first way `ran` differs from `kept`, if it does: what verify mode says, so only a
/// difference is said, and an equal value allocates nothing.
pub fn differs(kept: &Value, ran: &Value) -> Option<String> {
    if (kept.code, kept.signal) != (ran.code, ran.signal) {
        // alloc: a difference said
        return Some(format!(
            "it ended {:?} {:?}, not {:?} {:?}",
            ran.code, ran.signal, kept.code, kept.signal
        ));
    }
    if kept.stdout != ran.stdout {
        let line = first_line_differing(&kept.stdout, &ran.stdout);
        let [kept, ran] = [&kept.stdout, &ran.stdout].map(|it| shown_line(it, line));
        return Some(format!("what it printed differs at line {line}: {ran}, not {kept}")); // alloc: a difference said
    }
    fn paths(value: &Value) -> impl Iterator<Item = &str> {
        value.files.iter().map(|it| it.path.as_str())
    }
    if !paths(kept).eq(paths(ran)) {
        let [kept, ran]: [Vec<&str>; 2] = [paths(kept).collect(), paths(ran).collect()]; // alloc: a difference said
        return Some(format!("its OUT_DIR holds {ran:?}, not {kept:?}")); // alloc: a difference said
    }
    kept.files.iter().zip(&ran.files).find(|(a, b)| a != b).map(|(a, b)| {
        let shown = |it: &OutFile| match &it.bytes {
            None => "a directory".to_owned(), // alloc: a difference said
            Some(bytes) => format!(
                "{} bytes{} hashing {}",
                bytes.len(),
                if it.executable { ", executable," } else { "" },
                blaze_ids::ContentId::of("blaze v2 run file shown 1", bytes)
            ), // alloc: a difference said
        };
        format!("{} in its OUT_DIR differs: {}, not {}", a.path, shown(b), shown(a)) // alloc: a difference said
    })
}

/// Line `line` (from 1) of `bytes` as a difference shows it: quoted, lossy, cut at 200
/// characters, or `no line` past the end.
fn shown_line(bytes: &[u8], line: usize) -> String {
    match bytes.split(|it| *it == b'\n').nth(line.saturating_sub(1)) {
        None => "no line".to_owned(), // alloc: a difference said
        Some(it) => {
            let text = String::from_utf8_lossy(it);
            let cut: String = text.chars().take(200).collect(); // alloc: a difference said
            match cut.len() < text.len() {
                true => format!("{cut:?}..."), // alloc: a difference said
                false => format!("{cut:?}"),   // alloc: a difference said
            }
        }
    }
}

fn first_line_differing(a: &[u8], b: &[u8]) -> usize {
    let mut a = a.split(|it| *it == b'\n');
    let mut b = b.split(|it| *it == b'\n');
    let mut line = 1;
    loop {
        match (a.next(), b.next()) {
            (None, None) => return 1,
            (a, b) if a != b => return line,
            _ => line += 1,
        }
    }
}

/// `dir` made, empty.
fn empty(dir: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: emptying it: {error}", dir.display())), // alloc: error path
    }
    // alloc: error path
    std::fs::create_dir_all(dir).map_err(|error| format!("{}: making it: {error}", dir.display()))
}

/// What `dir` holds: every file and directory below it, by path, sorted. A link is what it
/// leads to when that is inside `dir`, and an error otherwise: what a later step reads
/// through it would be read outside every trace.
///
/// An entry's kind is the listing's (`d_type`), so a directory costs no call of its own, and a
/// file is opened once, its mode and length read on the descriptor: open, fstat, two reads and
/// close, the path resolved once (docs/v2/plan.md, "A run's
/// `OUT_DIR`").
pub fn collect(dir: &Path) -> Result<Vec<OutFile>, String> {
    use std::io::Read as _;
    let top = std::fs::canonicalize(dir).map_err(|error| format!("{}: {error}", dir.display()))?; // alloc: error path
    let mut out = Vec::new(); // alloc: the files the value keeps, read once as a run ends
    let mut stack = vec![PathBuf::new()]; // alloc: once per OUT_DIR collected
    while let Some(rel) = stack.pop() {
        let at = dir.join(&rel);
        let entries =
            std::fs::read_dir(&at).map_err(|error| format!("{}: {error}", at.display()))?; // alloc: error path
        for entry in entries {
            let entry = entry.map_err(|error| format!("{}: {error}", at.display()))?; // alloc: error path
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(format!("{}: a name that is not UTF-8 ({name:?})", at.display())); // alloc: error path
            };
            let rel = rel.join(name);
            let path = entry.path();
            // alloc: error path
            let kind = entry.file_type().map_err(|error| format!("{}: {error}", path.display()))?;
            let linked = kind.is_symlink();
            if linked {
                let target = std::fs::canonicalize(&path)
                    .map_err(|error| format!("{}: a link to nothing: {error}", path.display()))?; // alloc: error path
                if !target.starts_with(&top) {
                    return Err(format!(
                        // alloc: error path
                        "{} is a link to {}, outside the OUT_DIR: what is read through it \
                         would be read outside every trace",
                        path.display(),
                        target.display()
                    ));
                }
            }
            let is_dir = match linked {
                // alloc: error path
                true => std::fs::metadata(&path)
                    .map_err(|error| format!("{}: {error}", path.display()))?
                    .is_dir(),
                false => kind.is_dir(),
            };
            let name = rel_name(&rel);
            match is_dir {
                true => {
                    out.push(OutFile { path: name, executable: false, bytes: None });
                    // A link to a directory inside is kept as what it leads to, but not
                    // walked twice.
                    if !linked {
                        stack.push(rel);
                    }
                }
                false => {
                    let read = |path: &Path| -> std::io::Result<(bool, Vec<u8>)> {
                        let mut file = std::fs::File::open(path)?;
                        let meta = file.metadata()?;
                        // alloc: the file's bytes the value keeps, at their length
                        let mut bytes =
                            Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
                        file.read_to_end(&mut bytes)?;
                        Ok((executable(&meta), bytes))
                    };
                    // alloc: error path
                    let (executable, bytes) =
                        read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
                    out.push(OutFile { path: name, executable, bytes: Some(bytes) });
                }
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// `rel`, `/`-separated: the path an [`OutFile`] keeps.
fn rel_name(rel: &Path) -> String {
    let mut name = String::with_capacity(rel.as_os_str().len());
    for (at, part) in rel.components().enumerate() {
        if at > 0 {
            name.push('/');
        }
        name.push_str(&part.as_os_str().to_string_lossy());
    }
    name
}

#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &std::fs::Metadata) -> bool {
    false
}

/// Writes `files` into `dir`, emptied first: the `OUT_DIR` a run reused left. Each path is
/// checked to stay inside it before anything is touched; one that would not is the store's
/// bytes being wrong, and leaves `dir` as it was.
///
/// A directory is made once: one the value lists (it does each, before what is in it) or one
/// made for a file is not made again for the next file in it. A file is open, write, close,
/// and `fchmod` on its descriptor when it is executable.
pub fn restore(dir: &Path, files: &[OutFile]) -> Result<(), String> {
    use std::io::Write as _;
    for file in files {
        let ok = !file.path.is_empty()
            && !file.path.starts_with('/')
            && file
                .path
                .split('/')
                .all(|it| !it.is_empty() && it != "." && it != ".." && !it.contains('\\'));
        if !ok {
            return Err(format!("a kept OUT_DIR names {:?}, which would leave it", file.path)); // alloc: error path
        }
    }
    empty(dir)?;
    // alloc: once per OUT_DIR restored, the directories made in it, lent from `files`
    let mut made = blaze_hash::HashSet::<&str, std::alloc::Global>::default();
    for file in files {
        let path = dir.join(&file.path);
        match &file.bytes {
            None => {
                if made.insert(file.path.as_str()) {
                    // alloc: error path
                    std::fs::create_dir_all(&path)
                        .map_err(|error| format!("{}: {error}", path.display()))?;
                }
            }
            Some(bytes) => {
                if let Some((parent, _)) = file.path.rsplit_once('/')
                    && made.insert(parent)
                {
                    let parent = dir.join(parent);
                    // alloc: error path
                    std::fs::create_dir_all(&parent)
                        .map_err(|error| format!("{}: {error}", parent.display()))?;
                }
                let write = || -> std::io::Result<()> {
                    let mut out = std::fs::File::create(&path)?;
                    out.write_all(bytes)?;
                    #[cfg(unix)]
                    if file.executable {
                        use std::os::unix::fs::PermissionsExt as _;
                        out.set_permissions(std::fs::Permissions::from_mode(0o755))?;
                    }
                    Ok(())
                };
                write().map_err(|error| format!("{}: {error}", path.display()))?; // alloc: error path
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::BTreeMap,
        ffi::{OsStr, OsString},
        rc::Rc,
        time::Duration,
    };

    use blaze_cas::Config;
    use blaze_core::{
        FileName, run_until_idle,
        sim::{Faults, Ran, Sim},
    };
    use blaze_ids::ContentId;

    use super::*;
    use crate::trace::{DIRECTORY, listing};

    struct Fixed {
        files: BTreeMap<PathBuf, [u8; 32]>,
        env: Vec<(OsString, OsString)>,
    }

    /// `/ws`, and each checkout's package under `/ckout`, are directories holding `data`.
    fn is_dir(path: &Path) -> bool {
        path == Path::new("/ws") || path.parent() == Some(Path::new("/ckout"))
    }

    impl Inputs for Fixed {
        type Error = String;
        fn entry(&mut self, path: &Path) -> Result<crate::trace::Entry, String> {
            Ok(match (is_dir(path), self.files.get(path)) {
                (true, _) => crate::trace::Entry::Dir,
                (false, Some(hash)) => {
                    crate::trace::Entry::File(ContentId(*hash), blaze_vfs::Mode::Regular)
                }
                (false, None) => crate::trace::Entry::Missing,
            })
        }
        fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
            Ok(match is_dir(path) {
                true => Some(DIRECTORY),
                false => self.files.get(path).copied(),
            })
        }
        fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
            Ok(is_dir(path).then(|| listing([OsStr::new("data")])))
        }
        fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
            self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
        }
        fn env_all(&mut self) -> &[(OsString, OsString)] {
            self.env.sort();
            &self.env
        }
        fn names(
            &mut self,
            path: &Path,
            out: &mut crate::trace::Names<'_>,
        ) -> Result<bool, String> {
            if !is_dir(path) {
                return Ok(false);
            }
            out.push(OsStr::new("data"), false);
            Ok(true)
        }
    }

    fn opened(sim: &mut Sim) -> Store {
        let mut effects = Vec::new();
        let config =
            Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 1 << 48 };
        let mut store = Store::open(config, &mut effects);
        run_until_idle(sim, &mut effects, |completion, effects| {
            store.complete(completion, effects);
        }).expect("the effects end");
        assert!(store.ready());
        store
    }

    fn spec(mode: Mode) -> Spec {
        Spec {
            key: RunKey {
                code: ContentId::of("blaze v2 test 1", b"script"),
                given: ContentId::default(),
            },
            command: Command {
                program: "/script".into(),
                args: Vec::new(),
                env: vec![("CC".into(), "cc".into())],
                dir: "/ws".into(),
                stdin: Vec::new(),
                sandbox: Sandbox::On {
                    read: vec!["/ws".into()],
                    write: Vec::new(),
                    network: false,
                    trace: false,
                    services: Vec::new(),
                },
                memory: None,
            },
            out_dir: None,
            mode,
            roots: crate::roots::RootMap::default(),
        }
    }

    /// A script that reads `/ws/data` and `CC` and prints what `printed` holds then.
    fn script(
        sim: &mut Sim,
        data: Rc<RefCell<[u8; 32]>>,
        printed: Rc<RefCell<Vec<u8>>>,
        runs: Rc<RefCell<u32>>,
    ) {
        sim.program("/script", move |command, _| {
            *runs.borrow_mut() += 1;
            let Sandbox::On { trace: true, .. } = command.sandbox else { panic!("a run traces") };
            let traced = Traced {
                files: vec![("/ws/data".into(), Some(*data.borrow()))],
                dirs: vec![("/ws".into(), listing([OsStr::new("data")]))],
                env: vec![("CC".into(), Some("cc".into()))],
                untraceable: Vec::new(),
            };
            let exit = Exit {
                code: Some(0),
                signal: None,
                stdout: printed.borrow().clone(),
                stderr: Vec::new(),
                traced: Some(traced),
                peak_memory: 0,
            };
            Ran { exit, takes: Duration::from_millis(1) }
        });
    }

    /// [`spec`] in the checkout whose package is `dir`, a root ([`crate::roots`]).
    fn spec_in(mode: Mode, dir: &str) -> Spec {
        let mut spec = spec(mode);
        spec.command.dir = dir.into();
        spec.command.sandbox = Sandbox::On {
            read: vec![dir.into()],
            write: Vec::new(),
            network: false,
            trace: false,
            services: Vec::new(),
        };
        spec.roots =
            crate::roots::RootMap::default().with(crate::roots::Role::Package, Path::new(dir));
        spec
    }

    /// A script that reads `data` in the directory it runs in and `CC`, and prints what
    /// `printed` holds then.
    fn script_in_dir(sim: &mut Sim, printed: Rc<RefCell<Vec<u8>>>, runs: Rc<RefCell<u32>>) {
        sim.program("/script", move |command, _| {
            *runs.borrow_mut() += 1;
            let Sandbox::On { trace: true, .. } = command.sandbox else { panic!("a run traces") };
            let traced = Traced {
                files: vec![(command.dir.join("data"), Some([1; 32]))],
                dirs: vec![(command.dir.clone(), listing([OsStr::new("data")]))],
                env: vec![("CC".into(), Some("cc".into()))],
                untraceable: Vec::new(),
            };
            let exit = Exit {
                code: Some(0),
                signal: None,
                stdout: printed.borrow().clone(),
                stderr: Vec::new(),
                traced: Some(traced),
                peak_memory: 0,
            };
            Ran { exit, takes: Duration::from_millis(1) }
        });
    }

    fn go(
        sim: &mut Sim,
        store: &mut Store,
        inputs: &mut Fixed,
        mode: Mode,
    ) -> Result<(Value, bool), RunError<String>> {
        go_with(sim, store, inputs, spec(mode))
    }

    fn go_with(
        sim: &mut Sim,
        store: &mut Store,
        inputs: &mut Fixed,
        spec: Spec,
    ) -> Result<(Value, bool), RunError<String>> {
        let mut effects = Vec::new();
        let run = match Run::begin(spec, store, inputs, Tag(1), &mut effects)? {
            Begun::Reused(value) => return Ok((value, false)),
            Begun::Running(run) => run,
        };
        let mut done = None;
        run_until_idle(sim, &mut effects, |completion, _| done = Some(completion))
        .expect("the effects end");
        let completed =
            run.complete(done.unwrap(), store, inputs, |_| Ok(CargoRule::default()), |_| Ok(()))?;
        Ok((completed.value, true))
    }

    #[test]
    fn a_run_is_reused_until_what_it_read_changes_and_verified_on_request() {
        let mut sim = Sim::new(3, Faults::default());
        let mut store = opened(&mut sim);
        let data = Rc::new(RefCell::new([1; 32]));
        let printed = Rc::new(RefCell::new(b"cargo:rustc-cfg=one\n".to_vec()));
        let runs = Rc::new(RefCell::new(0));
        script(&mut sim, data.clone(), printed.clone(), runs.clone());
        let mut inputs = Fixed {
            files: [("/ws/data".into(), [1; 32])].into(),
            env: vec![("CC".into(), "cc".into())],
        };

        let (first, ran) = go(&mut sim, &mut store, &mut inputs, Mode::Normal).unwrap();
        assert!(ran && first.stdout == b"cargo:rustc-cfg=one\n");
        let (again, ran) = go(&mut sim, &mut store, &mut inputs, Mode::Normal).unwrap();
        assert!(!ran && again == first);
        // A worker's read, through a reader under its task's guard, finds what the store's
        // thread finds.
        let key = spec(Mode::Normal).key;
        let reader = store.reader();
        let guard = blaze_cas::enter();
        let Ok(Found::Hit { value: read, .. }) = index::find(&reader, &guard, &key, &mut inputs)
        else {
            panic!("a reader misses a kept run")
        };
        let Ok(Found::Hit { value: held, .. }) = index::find_held(&store, &key, &mut inputs) else {
            panic!("the store misses a kept run")
        };
        assert_eq!(read, held);
        // Warm, either finds the kept run with no allocation: the hit is lent in place.
        #[cfg(unix)]
        {
            let read = blaze_alloc_count::assert_at_most(0, "a warm find by a reader", || {
                matches!(index::find(&reader, &guard, &key, &mut inputs), Ok(Found::Hit { .. }))
            });
            let held =
                blaze_alloc_count::assert_at_most(0, "a warm find on the store's thread", || {
                    matches!(index::find_held(&store, &key, &mut inputs), Ok(Found::Hit { .. }))
                });
            assert!(read && held);
        }
        drop(guard);
        // A variable the run never read changes nothing.
        inputs.env.push(("PATH".into(), "/elsewhere".into()));
        assert!(!go(&mut sim, &mut store, &mut inputs, Mode::Normal).unwrap().1);

        inputs.files.insert("/ws/data".into(), [2; 32]);
        *data.borrow_mut() = [2; 32];
        *printed.borrow_mut() = b"cargo:rustc-cfg=two\n".to_vec();
        let (second, ran) = go(&mut sim, &mut store, &mut inputs, Mode::Normal).unwrap();
        assert!(ran && second.stdout == b"cargo:rustc-cfg=two\n");
        // Both kept: going back reuses the first.
        inputs.files.insert("/ws/data".into(), [1; 32]);
        assert_eq!(
            go(&mut sim, &mut store, &mut inputs, Mode::Normal).unwrap(),
            (first.clone(), false)
        );
        assert_eq!(*runs.borrow(), 2);

        *data.borrow_mut() = [1; 32];
        *printed.borrow_mut() = b"cargo:rustc-cfg=one\n".to_vec();
        assert_eq!(go(&mut sim, &mut store, &mut inputs, Mode::Verify).unwrap(), (first, true));
        *printed.borrow_mut() = b"cargo:rustc-cfg=other\n".to_vec();
        let differs = go(&mut sim, &mut store, &mut inputs, Mode::Verify).unwrap_err();
        assert!(matches!(differs, RunError::Differs { .. }), "{differs:?}");
    }

    /// C2 (round 4): a value verified across two checkouts is given to every checkout until
    /// one reruns it (verify mode) and gets otherwise; from then on the record that it depends
    /// on where its roots are keeps every checkout from being given it, and each keeps its own.
    #[test]
    fn a_verified_run_that_reruns_otherwise_is_retracted_for_every_checkout() {
        let mut sim = Sim::new(5, Faults::default());
        let mut store = opened(&mut sim);
        let printed = Rc::new(RefCell::new(b"cargo:rustc-cfg=one\n".to_vec()));
        let runs = Rc::new(RefCell::new(0));
        script_in_dir(&mut sim, printed.clone(), runs.clone());
        let files =
            ["a", "b", "c", "d"].map(|it| (PathBuf::from(format!("/ckout/{it}/data")), [1; 32]));
        let mut inputs = Fixed { files: files.into(), env: vec![("CC".into(), "cc".into())] };
        let mut go_in = |sim: &mut Sim, store: &mut Store, mode: Mode, dir: &str| {
            go_with(sim, store, &mut inputs, spec_in(mode, dir))
        };

        // Two checkouts run it and agree: verified.
        assert!(go_in(&mut sim, &mut store, Mode::Normal, "/ckout/a").unwrap().1);
        assert!(go_in(&mut sim, &mut store, Mode::Normal, "/ckout/b").unwrap().1);
        // A third is given it without a run.
        let (given, ran) = go_in(&mut sim, &mut store, Mode::Normal, "/ckout/c").unwrap();
        assert!(!ran && given.stdout == b"cargo:rustc-cfg=one\n");
        assert_eq!(*runs.borrow(), 2);

        // The third reruns it on request and gets otherwise: retracted, the record's ticket
        // given for the caller to make durable.
        *printed.borrow_mut() = b"cargo:rustc-cfg=other\n".to_vec();
        match go_in(&mut sim, &mut store, Mode::Verify, "/ckout/c") {
            Err(RunError::Differs { retracted: Some(_), .. }) => {}
            other => panic!("a verified value that reruns otherwise is retracted, not {other:?}"),
        }
        assert_eq!(*runs.borrow(), 3);

        // A fourth checkout is no longer given it: it runs.
        let (fourth, ran) = go_in(&mut sim, &mut store, Mode::Normal, "/ckout/d").unwrap();
        assert!(ran && fourth.stdout == b"cargo:rustc-cfg=other\n");
        // The third finds its own, kept here as it was retracted.
        let (own, ran) = go_in(&mut sim, &mut store, Mode::Normal, "/ckout/c").unwrap();
        assert!(!ran && own.stdout == b"cargo:rustc-cfg=other\n");
        // The first still finds its own, kept here before it was shared.
        let (first, ran) = go_in(&mut sim, &mut store, Mode::Normal, "/ckout/a").unwrap();
        assert!(!ran && first.stdout == b"cargo:rustc-cfg=one\n");
        assert_eq!(*runs.borrow(), 4);
    }

    /// C2 (round 7): a pure build script's run is kept under its sandboxed key and its pure
    /// key at once, so two checkouts verify both together. One check run that differs from
    /// the sandboxed key's verified value fails with what it ran ([`RunError::Differs`]'s
    /// `ran`), and that is what retracts the pure key's verified value too (as
    /// `serve::unit::script` does on that error): a fourth checkout is given neither.
    #[test]
    fn one_check_run_that_differs_retracts_both_its_keys() {
        let mut sim = Sim::new(6, Faults::default());
        let mut store = opened(&mut sim);
        let printed = Rc::new(RefCell::new(b"cargo:rustc-cfg=one\n".to_vec()));
        let runs = Rc::new(RefCell::new(0));
        script_in_dir(&mut sim, printed.clone(), runs.clone());
        let files =
            ["a", "b", "c", "d"].map(|it| (PathBuf::from(format!("/ckout/{it}/data")), [1; 32]));
        let mut inputs = Fixed { files: files.into(), env: vec![("CC".into(), "cc".into())] };
        let proof = crate::pure::Proof {
            identity: ContentId::of("blaze v2 test 1", b"pure script"),
            env: vec!["CC".into()],
            reads: true,
            writes: true,
            prints: true,
        };
        let read = |dir: &str| vec![(PathBuf::from(format!("{dir}/data")), Some([1; 32]))];
        let pure_hit = |store: &Store, inputs: &mut Fixed, dir: &str| {
            let reader = store.reader();
            let guard = blaze_cas::enter();
            crate::pure::find_hit(
                &reader,
                &guard,
                &proof,
                spec_in(Mode::Normal, dir).roots.view(),
                inputs,
            )
            .unwrap()
        };

        // Two checkouts run it, each keeping it under both keys, and agree: both verified.
        for dir in ["/ckout/a", "/ckout/b"] {
            let (value, ran) =
                go_with(&mut sim, &mut store, &mut inputs, spec_in(Mode::Normal, dir)).unwrap();
            assert!(ran);
            crate::pure::keep(
                &mut store,
                &proof,
                spec_in(Mode::Normal, dir).roots.view(),
                read(dir),
                &mut inputs,
                &value,
            )
            .unwrap();
        }
        let hit = pure_hit(&store, &mut inputs, "/ckout/c")
            .expect("a third checkout is given the pure key's value");
        assert!(hit.verified.is_some(), "verified across the two checkouts");

        // The third reruns it on request and gets otherwise: the sandboxed key is retracted,
        // and the error carries what it ran, which differs from the pure key's hit too.
        *printed.borrow_mut() = b"cargo:rustc-cfg=other\n".to_vec();
        let ran =
            match go_with(&mut sim, &mut store, &mut inputs, spec_in(Mode::Verify, "/ckout/c")) {
                Err(RunError::Differs { retracted: Some(_), ran, .. }) => ran,
                other => {
                    panic!("a verified value that reruns otherwise is retracted, not {other:?}")
                }
            };
        assert_eq!(ran.stdout, b"cargo:rustc-cfg=other\n");
        let c = spec_in(Mode::Normal, "/ckout/c").roots;
        let retracted = crate::pure::retract(&mut store, &proof, c.view(), &hit, &ran).unwrap();
        assert!(
            retracted.is_some_and(|(_, tickets)| !tickets.is_empty()),
            "the pure key's verified value is retracted too"
        );

        // A fourth checkout is given neither: its pure key misses, and its sandboxed key runs.
        assert_eq!(
            pure_hit(&store, &mut inputs, "/ckout/d"),
            None,
            "the pure key's value is not given again"
        );
        let (fourth, ran) =
            go_with(&mut sim, &mut store, &mut inputs, spec_in(Mode::Normal, "/ckout/d")).unwrap();
        assert!(ran && fourth.stdout == b"cargo:rustc-cfg=other\n");
    }

    #[test]
    fn a_run_outside_a_sandbox_is_refused() {
        let mut sim = Sim::new(4, Faults::default());
        let store = opened(&mut sim);
        let mut off = spec(Mode::Normal);
        off.command.sandbox = Sandbox::Off;
        let mut inputs = Fixed { files: BTreeMap::new(), env: Vec::new() };
        let begun = Run::begin(off, &store, &mut inputs, Tag(1), &mut Vec::new());
        assert!(matches!(begun, Err(RunError::Spawn(_))));
    }

    fn value() -> Value {
        Value {
            code: Some(0),
            signal: None,
            stdout: b"cargo:rustc-cfg=a\ncargo:rustc-cfg=b\n".to_vec(),
            stderr: Vec::new(),
            files: vec![
                OutFile { path: "gen".into(), executable: false, bytes: None },
                OutFile { path: "gen/a.rs".into(), executable: false, bytes: Some(b"x".to_vec()) },
            ],
        }
    }

    #[test]
    fn a_value_round_trips() {
        assert_eq!(Value::decode(&value().encode()), Ok(value()));
        assert!(Value::decode(b"other").is_err());
    }

    #[test]
    fn differences_are_named() {
        assert_eq!(differs(&value(), &value()), None);
        let mut printed = value();
        printed.stdout = b"cargo:rustc-cfg=a\ncargo:rustc-cfg=c\n".to_vec();
        assert_eq!(
            differs(&value(), &printed).as_deref(),
            Some(
                r#"what it printed differs at line 2: "cargo:rustc-cfg=c", not "cargo:rustc-cfg=b""#
            )
        );
        let mut file = value();
        file.files[1].bytes = Some(b"y".to_vec());
        let file_differs = differs(&value(), &file).unwrap();
        assert!(
            file_differs.starts_with("gen/a.rs in its OUT_DIR differs: 1 bytes hashing "),
            "{file_differs}"
        );
        assert!(file_differs.contains(", not 1 bytes hashing "), "{file_differs}");
        let mut failed = value();
        failed.code = Some(1);
        assert!(differs(&value(), &failed).unwrap().contains("ended"));
    }

    #[test]
    fn an_out_dir_is_collected_and_restored_and_kept_inside() {
        let dir = std::env::temp_dir().join(format!("blaze-out-dir-test-{}", std::process::id()));
        empty(&dir).unwrap();
        restore(&dir, &value().files).unwrap();
        assert_eq!(collect(&dir).unwrap(), value().files);
        let bad = [OutFile { path: "../x".into(), executable: false, bytes: Some(Vec::new()) }];
        assert!(restore(&dir, &bad).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/hosts", dir.join("out")).unwrap();
            assert!(collect(&dir).unwrap_err().contains("outside the OUT_DIR"));
            std::fs::remove_file(dir.join("out")).unwrap();
            std::os::unix::fs::symlink(dir.join("gen/a.rs"), dir.join("in")).unwrap();
            let files = collect(&dir).unwrap();
            assert!(
                files
                    .iter()
                    .any(|it| it.path == "in" && it.bytes.as_deref() == Some(b"x".as_slice()))
            );
        }
        _ = std::fs::remove_dir_all(&dir);
    }

    /// Directories made once, whichever way the value lists them: a parent it does not list, a
    /// directory listed after a file in it, and a file's mode set on its descriptor.
    #[test]
    fn a_restore_makes_each_directory_once_and_collect_reads_it_back() {
        let dir = std::env::temp_dir().join(format!("blaze-out-dir-once-{}", std::process::id()));
        let file = |path: &str, executable: bool, bytes: &[u8]| OutFile {
            path: path.into(),
            executable,
            bytes: Some(bytes.to_vec()),
        };
        let listed = |path: &str| OutFile { path: path.into(), executable: false, bytes: None };
        let given = [
            file("a/b/deep.rs", false, b"deep"),
            listed("a"),
            file("a/one.rs", false, b"1"),
            file("a/two.rs", false, b"2"),
            listed("a/b"),
            file("run.sh", true, b"#!/bin/sh\n"),
            listed("empty"),
            file("top.rs", false, b""),
        ];
        restore(&dir, &given).unwrap();
        let mut expected = given.to_vec();
        expected.sort_by(|a, b| a.path.cmp(&b.path));
        #[cfg(not(unix))]
        expected.iter_mut().for_each(|it| it.executable = false);
        assert_eq!(collect(&dir).unwrap(), expected);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.join("run.sh")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
            std::os::unix::fs::symlink(dir.join("a/b"), dir.join("to-b")).unwrap();
            let files = collect(&dir).unwrap();
            let to_b = files.iter().find(|it| it.path == "to-b").unwrap();
            assert_eq!(to_b.bytes, None, "a link to a directory inside is kept as the directory");
            assert!(!files.iter().any(|it| it.path.starts_with("to-b/")), "and not walked twice");
        }
        // Restored again over what is there: emptied first, the same files.
        restore(&dir, &expected).unwrap();
        assert_eq!(collect(&dir).unwrap(), expected);
        _ = std::fs::remove_dir_all(&dir);
    }
}
