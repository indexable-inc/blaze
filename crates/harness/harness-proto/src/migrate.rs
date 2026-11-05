//! The one-time move of a chat log from the NDJSON of before schema 1 to frames ([`crate::log`]), as a sans-IO core:
//! [`plan`] looks at what is on disk and returns the [`Step`]s, and the daemon performs them in order at start, before
//! it listens (crates/harness/harness/src/chat/mod.rs `migrate_logs`):
//!
//! 1. [`Step::WriteTmp`]: the converted log ([`convert`]) to `objects/<id>/chat.migrating`;
//! 2. [`Step::SyncTmp`]: fsync it, so the name never points at data the disk does not have;
//! 3. [`Step::Rename`]: rename it over `chat`, the one commit point;
//! 4. [`Step::SyncDir`]: fsync the directory, so the rename is durable;
//! 5. [`Step::Mark`]: record in the index that the chat's log is this schema's.
//!
//! A crash anywhere leaves `chat` the old log or the new one, whole, never neither: until the rename the old file is
//! untouched, and the new one is synced before it takes the name. Recovery is [`plan`] again: it decides by what the
//! file is, not by the mark (a crash between the rename and the mark leaves a converted log unmarked, and converting it
//! again would read frames as NDJSON); the mark only lets a later start skip reading logs already converted, and it is
//! written last, so a marked log is always converted. The proof is the test below: every crash point, every outcome a
//! crash can leave (the rename durable or not, each file's unsynced bytes kept whole, torn or lost), a crash during
//! recovery too, against the oracle that the log afterwards holds every event of the old one exactly once, in order;
//! and planted bugs it must catch.
use crate::event::{Body, Event};
use crate::log::{self, MAGIC, NotALog};

/// The converted log's name while it is written, beside `chat`.
pub const TMP: &str = "chat.migrating";

/// One effect of a migration, in the order [`plan`] lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Create (or truncate) [`TMP`] and write these bytes.
    WriteTmp(Vec<u8>),
    /// fsync [`TMP`].
    SyncTmp,
    /// Rename [`TMP`] over `chat`.
    Rename,
    /// fsync the chat's directory.
    SyncDir,
    /// Mark the chat's log converted in the index.
    Mark,
}

/// A safeguard removed, for the test's planted-bug controls.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plant {
    None,
    /// No fsync of the new log before the rename.
    NoSync,
    /// The mark before the rename.
    MarkFirst,
    /// Recovery trusts a missing mark: an unmarked log is converted whatever it holds.
    TrustMark,
}

/// The steps for a chat whose index mark is `marked` and whose `chat` file holds `chat` (None: there is none).
///
/// # Errors
/// A `chat` file that is neither log format (named; the daemon reports it and leaves it alone).
pub fn plan(marked: bool, chat: Option<&[u8]>) -> Result<Vec<Step>, NotALog> {
    plan_planted(marked, chat, Plant::None)
}

#[doc(hidden)]
pub fn plan_planted(marked: bool, chat: Option<&[u8]>, plant: Plant) -> Result<Vec<Step>, NotALog> {
    if marked {
        return Ok(Vec::new());
    }
    let Some(bytes) = chat else { return Ok(vec![Step::Mark]) };
    let ndjson = match log::read(bytes) {
        Ok(_) => plant == Plant::TrustMark && !bytes.is_empty(),
        Err(NotALog::Ndjson) => true,
        Err(e) => return Err(e),
    };
    if !ndjson {
        return Ok(vec![Step::Mark]);
    }
    let write = Step::WriteTmp(convert(bytes));
    Ok(match plant {
        Plant::NoSync => vec![write, Step::Rename, Step::SyncDir, Step::Mark],
        Plant::MarkFirst => vec![write, Step::SyncTmp, Step::Mark, Step::Rename, Step::SyncDir],
        Plant::None | Plant::TrustMark => vec![write, Step::SyncTmp, Step::Rename, Step::SyncDir, Step::Mark],
    })
}

/// An NDJSON log as frames: each line that is an event (as [`Event::from_json`] reads it) its frame, in order; a line
/// that is not (the torn last line a crash mid-append left, or a line naming no event this schema has) an `error`
/// event in its place naming it, stamped with the time of the event before it.
#[must_use]
pub fn convert(ndjson: &[u8]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    let mut ts = 0;
    for (i, line) in ndjson.split(|&b| b == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let ev = serde_json::from_slice(line).ok().and_then(|v| Event::from_json(&v)).unwrap_or_else(|| Event {
            ts,
            body: Body::Error { text: dropped(i + 1, line.len()) },
        });
        ts = ev.ts;
        out.extend_from_slice(&ev.frame());
    }
    out
}

/// What [`convert`] writes in place of line `n` of `len` bytes that was not an event.
#[must_use]
pub fn dropped(n: usize, len: usize) -> String {
    format!(
        "line {n} of this chat's NDJSON log ({len} bytes) was not a whole event (likely torn by a crash mid-append); \
         it was dropped when the log moved to harness schema {}",
        crate::SCHEMA
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A file: what it holds now, and what of that the disk has (an fsync makes them equal).
    #[derive(Clone, Debug)]
    struct File {
        data: Vec<u8>,
        synced: Vec<u8>,
    }

    /// The chat's directory on a disk that can lose power: its names now and as last synced, the files they name,
    /// and the index's mark (SQLite: durable when written).
    #[derive(Clone, Debug)]
    struct Disk {
        files: Vec<File>,
        names: BTreeMap<&'static str, usize>,
        synced_names: BTreeMap<&'static str, usize>,
        marked: bool,
    }

    impl Disk {
        fn new(chat: &[u8]) -> Self {
            let names = BTreeMap::from([("chat", 0)]);
            Self { files: vec![File { data: chat.to_vec(), synced: chat.to_vec() }], synced_names: names.clone(), names, marked: false }
        }

        fn chat(&self) -> Option<&[u8]> {
            self.names.get("chat").map(|&i| self.files[i].data.as_slice())
        }

        fn step(&mut self, s: &Step) {
            match s {
                Step::WriteTmp(b) => {
                    self.files.push(File { data: b.clone(), synced: Vec::new() });
                    self.names.insert(TMP, self.files.len() - 1);
                }
                Step::SyncTmp => {
                    let f = &mut self.files[self.names[TMP]];
                    f.synced = f.data.clone();
                }
                Step::Rename => {
                    let i = self.names.remove(TMP).expect("rename of a tmp that is there");
                    self.names.insert("chat", i);
                }
                Step::SyncDir => self.synced_names = self.names.clone(),
                Step::Mark => self.marked = true,
            }
        }

        /// Every state a power loss now can leave: the directory as synced or as it is (a rename may reach the disk
        /// before its fsync, and before the data it names), and each named file's unsynced bytes lost, torn or kept.
        fn crashes(&self) -> Vec<Disk> {
            let mut out = Vec::new();
            for names in [&self.synced_names, &self.names] {
                let ids: Vec<usize> = names.values().copied().collect();
                let choices: Vec<Vec<Vec<u8>>> = ids
                    .iter()
                    .map(|&i| {
                        let f = &self.files[i];
                        if f.data == f.synced {
                            vec![f.data.clone()]
                        } else {
                            let half = f.data[..f.data.len() / 2].to_vec();
                            vec![f.synced.clone(), half, f.data.clone()]
                        }
                    })
                    .collect();
                let mut pick = vec![0; ids.len()];
                loop {
                    let mut d = self.clone();
                    for (k, &i) in ids.iter().enumerate() {
                        let kept = choices[k][pick[k]].clone();
                        d.files[i] = File { data: kept.clone(), synced: kept };
                    }
                    d.names = names.clone();
                    d.synced_names = names.clone();
                    out.push(d);
                    let Some(k) = (0..ids.len()).find(|&k| pick[k] + 1 < choices[k].len()) else { break };
                    pick[k] += 1;
                    pick[..k].iter_mut().for_each(|p| *p = 0);
                }
            }
            out
        }
    }

    /// Two turns' events, a REPL cell's detail with steps, and a torn last line.
    fn old_log() -> Vec<u8> {
        let lines = [
            r#"{"ev":"session","agent":"claude","model":"opus","session_id":"9f2c","ts":1}"#,
            r#"{"ev":"user","text":"count the files","ts":2}"#,
            r#"{"ev":"busy","busy":true,"ts":3}"#,
            r#"{"ev":"block_start","block":"m:0","kind":"thinking","ts":4}"#,
            r#"{"ev":"block","block":"m:0","kind":"thinking","text":"list them","ts":5}"#,
            r#"{"ev":"tool_call","block":"t1","name":"ts","input":{"code":"await glob('src/*')"},"ts":6}"#,
            r#"{"ev":"tool_detail","block":"t1","cell":"r.t1","status":"done","ms":4,"effects":[{"kind":"search","done":"Found 3","status":"ok"}],"value":{"t":"scalar","type":"number","v":3},"ts":7}"#,
            r#"{"ev":"tool_result","block":"t1","text":"3","error":false,"ts":8}"#,
            r#"{"ev":"turn_end","ok":true,"stop":"end_turn","duration_ms":900,"cost_usd":0.02,"text":"3 files.","ts":9}"#,
            r#"{"ev":"busy","busy":false,"ts":10}"#,
            r#"{"ev":"block","block":"m2:0","kind":"te"#,
        ];
        lines.join("\n").into_bytes()
    }

    /// The oracle: what the old log's events are, each once and in order, and its one torn line reported.
    fn expected() -> Vec<Event> {
        let log = old_log();
        let mut evs: Vec<Event> = log
            .split(|&b| b == b'\n')
            .filter_map(|l| serde_json::from_slice(l).ok().and_then(|v| Event::from_json(&v)))
            .collect();
        assert_eq!(evs.len(), 10);
        evs.push(Event { ts: 10, body: Body::Error { text: dropped(11, log.split(|&b| b == b'\n').last().unwrap().len()) } });
        evs
    }

    /// The invariant at a crash: `chat` is the old log, whole, or a whole new one holding every event once.
    fn intact(d: &Disk, old: &[u8], want: &[Event]) -> Result<(), String> {
        match d.chat() {
            None => Err("no chat log at all".into()),
            Some(b) if b == old => Ok(()),
            Some(b) => holds(b, want),
        }
    }

    fn holds(b: &[u8], want: &[Event]) -> Result<(), String> {
        let log = log::read(b).map_err(|e| format!("not a log: {e}"))?;
        if log.torn.is_some() {
            return Err(format!("a torn frame at {:?}", log.torn));
        }
        let got: Vec<Event> = log.records.into_iter().map(|r| r.event).collect::<Result<_, _>>()?;
        if got != want {
            return Err(format!("{} events, want {}: {got:?}", got.len(), want.len()));
        }
        Ok(())
    }

    /// Run `plan` on `d` to the end, crashing after `crash` of its steps when asked: the states a crash leaves, or the
    /// finished disk.
    fn run(d: &Disk, plant: Plant, crash: Option<usize>) -> Result<Vec<Disk>, String> {
        let steps = plan_planted(d.marked, d.chat(), plant).map_err(|e| e.to_string())?;
        let mut d = d.clone();
        for (i, s) in steps.iter().enumerate() {
            if crash == Some(i) {
                return Ok(d.crashes());
            }
            d.step(s);
        }
        if crash == Some(steps.len()) {
            return Ok(d.crashes());
        }
        Ok(vec![d])
    }

    /// Every crash point of a migration, every state each leaves, then recovery (crashing again at each of its points,
    /// then recovering for good): the first violation of the oracle, if any.
    fn search(plant: Plant) -> Option<String> {
        let old = old_log();
        let want = expected();
        let start = Disk::new(&old);
        for crash in 0..=5 {
            for c1 in run(&start, plant, Some(crash)).unwrap() {
                if let Err(e) = intact(&c1, &old, &want) {
                    return Some(format!("crash after {crash} steps: {e}"));
                }
                for crash2 in 0..=5 {
                    for c2 in run(&c1, plant, Some(crash2)).unwrap() {
                        if let Err(e) = intact(&c2, &old, &want) {
                            return Some(format!("crash after {crash} steps, then {crash2} of recovery: {e}"));
                        }
                        let done = match run(&c2, plant, None) {
                            Ok(mut v) => v.remove(0),
                            Err(e) => return Some(format!("recovery after crashes at {crash}, {crash2}: {e}")),
                        };
                        if let Err(e) = done.chat().map_or(Err("no chat log".to_owned()), |b| holds(b, &want)) {
                            return Some(format!("after crashes at {crash}, {crash2} and recovery: {e}"));
                        }
                        if !done.marked {
                            return Some(format!("after crashes at {crash}, {crash2} and recovery: not marked"));
                        }
                    }
                }
            }
        }
        None
    }

    /// The migration's invariant holds at every crash point and after every recovery: the log is the old one or the new
    /// one, whole; and once recovered it holds every event of the old log exactly once, in order, and is marked.
    #[test]
    fn a_crash_anywhere_loses_and_doubles_nothing() {
        assert_eq!(search(Plant::None), None);
        // A log converted before (a crash between the rename and the mark) is marked, not converted again.
        let mut d = Disk::new(&convert(&old_log()));
        assert_eq!(plan(false, d.chat()).unwrap(), vec![Step::Mark]);
        d.step(&Step::Mark);
        assert_eq!(plan(d.marked, d.chat()).unwrap(), vec![]);
        assert_eq!(plan(false, None).unwrap(), vec![Step::Mark]);
        assert!(plan(false, Some(b"\x00garbage")).is_err());
    }

    /// The controls: with each safeguard removed, the search finds a crash that breaks the invariant.
    #[test]
    fn each_planted_bug_is_caught() {
        for plant in [Plant::NoSync, Plant::MarkFirst, Plant::TrustMark] {
            let found = search(plant);
            assert!(found.is_some(), "{plant:?} went uncaught");
            eprintln!("{plant:?}: {}", found.unwrap());
        }
    }
}
