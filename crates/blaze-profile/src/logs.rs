//! Logs: which `tracing` events the flight recorder keeps, which of them the history writes,
//! and an event's field values as the model and a profile's parts carry them. An event is a
//! row of `events` (its message alone) and a row of `event_fields` for each other field, typed
//! as the event recorded it ([`crate::flight::evented`]).
//!
//! What is kept, by level:
//! - ERROR and WARN: every event, of any target.
//! - INFO: an event of a `blaze*` target ([`crate::keeps`]).
//! - DEBUG: an event of the target `blaze` or a `blaze::` path only ([`blaze_debug`]): kept in
//!   the ring, so a stall dump and the live feed show it, and written to the history by
//!   [`Policy`] and [`Hold`] alone.
//! - TRACE: never.
//!
//! What the history writes of DEBUG rows ([`Hold`]), under [`Policy::Info`], the default: a
//! build's DEBUG rows are held until the build ends; written when it failed or stalled, or an
//! ERROR event of it was read, or the history closed before it ended (a crash or a shutdown
//! under way); dropped when it ended well (ok or cancelled). Rows of no build are dropped.
//! Under [`Policy::Debug`] ([`POLICY_VAR`]`=debug`) every row is written. The rule depends
//! only on the rows read and the builds' ends, in the order the history was told them: never
//! on time or thread timing.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde_derive::{Deserialize, Serialize};

use crate::flight::{EventRow, FieldRow, FieldValue, Read};

/// Whether an event of `target` named `name` at `level` is kept by the flight recorder (the
/// module doc's rule).
pub fn keeps_event(target: &str, name: &str, level: tracing::Level) -> bool {
    match level {
        tracing::Level::ERROR | tracing::Level::WARN => true,
        tracing::Level::INFO => crate::keeps(target, name),
        tracing::Level::DEBUG => blaze_debug(target),
        _ => false,
    }
}

/// [`keeps_event`] of the level `tracing` names `level` (`"info"`, `"DEBUG"`, `"3"`): what a
/// lint reads from a macro's text. False for a name that is no level's.
pub fn keeps_event_named(target: &str, name: &str, level: &str) -> bool {
    level.parse::<tracing::Level>().is_ok_and(|level| keeps_event(target, name, level))
}

/// Whether a DEBUG event of `target` is kept: its target is `blaze` or a path under it
/// (`target: "blaze::store"`), written to opt in. A module path's own target (`blaze_fmt::...`)
/// is not: a `blaze*` crate's `debug!` calls in hot paths cost no ring row unless they ask.
pub fn blaze_debug(target: &str) -> bool {
    target == crate::TARGET
        || target.strip_prefix(crate::TARGET).is_some_and(|rest| rest.starts_with("::"))
}

/// The variable that says what the history writes of DEBUG events ([`Policy`]).
pub const POLICY_VAR: &str = "BLAZE_HISTORY_LEVEL";

/// What the history writes of DEBUG events: the module doc's rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Policy {
    /// DEBUG rows of a build in trouble only ([`Hold`]).
    #[default]
    Info,
    /// Every DEBUG row the ring kept.
    Debug,
}

impl Policy {
    /// The policy `value` names: `info` or `debug`, any case; the default when unset.
    pub fn parse(value: Option<&str>) -> Result<Policy, PolicyError> {
        match value {
            None => Ok(Policy::Info),
            Some(it) if it.eq_ignore_ascii_case("info") => Ok(Policy::Info),
            Some(it) if it.eq_ignore_ascii_case("debug") => Ok(Policy::Debug),
            // alloc: an error
            Some(it) => Err(PolicyError { value: it.to_owned() }),
        }
    }

    /// The policy [`POLICY_VAR`] names in this process's environment.
    pub fn from_env() -> Result<Policy, PolicyError> {
        match std::env::var(POLICY_VAR) {
            Ok(value) => Policy::parse(Some(&value)),
            Err(std::env::VarError::NotPresent) => Ok(Policy::Info),
            // alloc: an error
            Err(std::env::VarError::NotUnicode(value)) => {
                Err(PolicyError { value: value.to_string_lossy().into_owned() })
            }
        }
    }
}

/// [`POLICY_VAR`] names no policy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{var}={value:?} names no history level: it takes `info` (the default) or `debug`", var = POLICY_VAR)]
pub struct PolicyError {
    pub value: String,
}

/// DEBUG rows held a build, at most: past it, its oldest go, with their fields.
pub const HOLD_EVENTS: usize = 1 << 14;
/// Builds whose DEBUG rows are held at once, at most: past it, the lowest build's go.
pub const HOLD_BUILDS: usize = 8;
/// Builds remembered as in trouble, or as ended well, at most: the lowest are forgotten.
const REMEMBERED: usize = 64;

/// What the history writes of DEBUG rows, as a state machine on the rows read and the builds'
/// ends ([`Policy`]): owns no IO and reads no clock. [`Hold::sort`] runs on each read before it
/// is written.
#[derive(Debug)]
pub struct Hold {
    policy: Policy,
    held: BTreeMap<u32, Held>,
    /// Builds in trouble: their DEBUG rows are written.
    troubled: BTreeSet<u32>,
    /// Builds that ended well: their DEBUG rows are dropped.
    well: BTreeSet<u32>,
    /// Held rows of builds that came into trouble since the last sort: the next one writes them.
    released: Held,
    /// DEBUG rows dropped since the hold was made.
    dropped: u64,
}

/// A build's held DEBUG rows: its events, and their fields in the same order.
#[derive(Debug, Default)]
struct Held {
    events: VecDeque<EventRow>,
    fields: VecDeque<FieldRow>,
}

impl Held {
    fn rows(&self) -> usize {
        self.events.len() + self.fields.len()
    }

    /// Drops its oldest event and that event's fields (the front ones: fields follow their
    /// event, in order); the rows dropped.
    fn pop_oldest(&mut self) -> u64 {
        let Some(event) = self.events.pop_front() else { return 0 };
        let mut dropped = 1;
        while self.fields.front().is_some_and(|it| it.event_id == event.event_id) {
            self.fields.pop_front();
            dropped += 1;
        }
        dropped
    }
}

/// What [`Hold::sort`] does with a DEBUG row of a build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    Write,
    Hold,
    Drop,
}

impl Hold {
    pub fn new(policy: Policy) -> Hold {
        Hold {
            policy,
            // alloc: once a history, the builds held stay under HOLD_BUILDS
            held: BTreeMap::new(),
            // alloc: once a history, under REMEMBERED
            troubled: BTreeSet::new(),
            // alloc: once a history, under REMEMBERED
            well: BTreeSet::new(),
            released: Held::default(),
            dropped: 0,
        }
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// DEBUG rows held now, events and fields.
    pub fn held(&self) -> usize {
        self.held.values().map(Held::rows).sum::<usize>() + self.released.rows()
    }

    /// The event ids of the DEBUG rows held now, events and fields: rows of the rings no flush
    /// has written, which the flight recorder's mapping keeps for a postmortem
    /// ([`crate::flight::flush_pending`]).
    pub fn held_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.held.values().chain([&self.released]).flat_map(|held| {
            held.events.iter().map(|it| it.event_id).chain(held.fields.iter().map(|it| it.event_id))
        })
    }

    /// The builds whose DEBUG rows are held now, at most [`HOLD_BUILDS`] after a sort.
    pub fn held_builds(&self) -> impl Iterator<Item = u32> + '_ {
        self.held.keys().copied()
    }

    /// DEBUG rows dropped since the hold was made, events and fields.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Leaves in `read` what the history writes of it now: every row but DEBUG ones, DEBUG
    /// ones of builds in trouble (with the rows held of builds that came into trouble since
    /// the last sort, appended), and holds or drops the other DEBUG rows by the module doc's
    /// rule. An ERROR event in `read` puts its build in trouble first.
    pub fn sort(&mut self, read: &mut Read) {
        if self.policy == Policy::Debug {
            return;
        }
        // alloc: once a sort that reads an error, at most a build an error
        let errors: Vec<u32> = read
            .events
            .iter()
            .filter(|it| it.build != 0 && level(it.site) == Some(tracing::Level::ERROR))
            .map(|it| it.build)
            .collect();
        for build in errors {
            self.trouble(build);
        }
        let mut dropped = 0;
        let held = &mut self.held;
        let (troubled, well) = (&self.troubled, &self.well);
        let fate = |build: u32, site: u16| match level(site) {
            Some(tracing::Level::DEBUG) => match build {
                0 => Fate::Drop,
                _ if troubled.contains(&build) => Fate::Write,
                _ if well.contains(&build) => Fate::Drop,
                _ => Fate::Hold,
            },
            _ => Fate::Write,
        };
        read.events.retain(|it| match fate(it.build, it.site) {
            Fate::Write => true,
            Fate::Hold => {
                held.entry(it.build).or_default().events.push_back(*it);
                false
            }
            Fate::Drop => {
                dropped += 1;
                false
            }
        });
        read.fields.retain(|it| match fate(it.build, it.site) {
            Fate::Write => true,
            Fate::Hold => {
                held.entry(it.build).or_default().fields.push_back(*it);
                false
            }
            Fate::Drop => {
                dropped += 1;
                false
            }
        });
        for build in held.values_mut() {
            while build.events.len() > HOLD_EVENTS {
                dropped += build.pop_oldest();
            }
        }
        while held.len() > HOLD_BUILDS {
            if let Some((_, gone)) = held.pop_first() {
                dropped += gone.rows() as u64;
            }
        }
        self.dropped += dropped;
        read.events.extend(self.released.events.drain(..));
        read.fields.extend(self.released.fields.drain(..));
    }

    /// Notes that `build` ended: in trouble (it failed or stalled) or well (ok or cancelled).
    /// Its held rows go to the next sort, or are dropped.
    pub fn ended(&mut self, build: u32, trouble: bool) {
        if build == 0 {
            return;
        }
        if trouble {
            self.trouble(build);
        } else if !self.troubled.contains(&build) {
            if let Some(gone) = self.held.remove(&build) {
                self.dropped += gone.rows() as u64;
            }
            remember(&mut self.well, build);
        }
    }

    /// Puts every build still held in trouble: the history is closing before they ended, so
    /// the next sort writes their rows.
    pub fn release_all(&mut self) {
        // alloc: once a history, as it closes
        let builds: Vec<u32> = self.held.keys().copied().collect();
        for build in builds {
            self.trouble(build);
        }
    }

    fn trouble(&mut self, build: u32) {
        self.well.remove(&build);
        remember(&mut self.troubled, build);
        if let Some(mut held) = self.held.remove(&build) {
            self.released.events.append(&mut held.events);
            self.released.fields.append(&mut held.fields);
        }
    }
}

/// Adds `build` to `set`, forgetting the lowest past [`REMEMBERED`].
fn remember(set: &mut BTreeSet<u32>, build: u32) {
    set.insert(build);
    while set.len() > REMEMBERED {
        set.pop_first();
    }
}

/// The level of the callsite `site`.
fn level(site: u16) -> Option<tracing::Level> {
    crate::flight::meta(site).map(|it| *it.level())
}

/// A field's value as the model and a profile's part carry it: [`FieldValue`] with its text
/// resolved.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Value {
    I64(i64),
    U64(u64),
    F64(f64),
    Bool(bool),
    /// Its text; `None` when the text was let go before it was read.
    Str(Option<String>),
}

impl Value {
    /// `value` with its text looked up ([`crate::flight::name`]).
    pub fn of(value: FieldValue) -> Value {
        match value {
            FieldValue::I64(it) => Value::I64(it),
            FieldValue::U64(it) => Value::U64(it),
            FieldValue::F64(it) => Value::F64(it),
            FieldValue::Bool(it) => Value::Bool(it),
            // alloc: a field's text, once a field a reader takes
            FieldValue::Str(sym) => Value::Str(crate::flight::name(sym).map(|it| it.to_string())),
        }
    }

    /// Its kind, as `event_fields.kind` names it.
    pub const fn kind(&self) -> &'static str {
        match self {
            Value::I64(_) => "i64",
            Value::U64(_) => "u64",
            Value::F64(_) => "f64",
            Value::Bool(_) => "bool",
            Value::Str(_) => "str",
        }
    }

    /// It as JSON: a number, a boolean, a string or null.
    pub fn json(&self) -> serde_json::Value {
        match self {
            Value::I64(it) => (*it).into(),
            Value::U64(it) => (*it).into(),
            Value::F64(it) => serde_json::Number::from_f64(*it)
                .map_or(serde_json::Value::Null, serde_json::Value::Number),
            Value::Bool(it) => (*it).into(),
            // clone: a field's text, once a field a trace
            Value::Str(it) => it.clone().map_or(serde_json::Value::Null, serde_json::Value::String),
        }
    }
}

/// Two values are equal when they are of one kind and the same bits: a float equals itself,
/// NaN too, so the model's rows compare.
impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::I64(a), Value::I64(b)) => a == b,
            (Value::U64(a), Value::U64(b)) => a == b,
            (Value::F64(a), Value::F64(b)) => a.to_bits() == b.to_bits(),
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Value {}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::I64(it) => write!(f, "{it}"),
            Value::U64(it) => write!(f, "{it}"),
            Value::F64(it) => write!(f, "{it}"),
            Value::Bool(it) => write!(f, "{it}"),
            Value::Str(Some(it)) => f.write_str(it),
            Value::Str(None) => f.write_str("?"),
        }
    }
}

/// An event's fields as one text, `key=value` each, for a label: what the message said
/// before fields were rows of their own.
pub fn fields_text(fields: &[(String, Value)]) -> String {
    use std::fmt::Write;
    // alloc: a label, once an event a report
    let mut text = String::new();
    for (key, value) in fields {
        if !text.is_empty() {
            text.push(' ');
        }
        // Writing to a `String` does not fail.
        _ = write!(text, "{key}={value}");
    }
    text
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use arrow_array::{
        Array, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray, UInt64Array,
    };

    use super::*;
    use crate::{batches, flight};

    /// Reads the rows of the rings a test's thread `tid` wrote in the last minute.
    fn mine(tid: u32) -> Read {
        let mut read = Read::default();
        flight::window(Duration::from_secs(60), &mut read);
        let ids: BTreeSet<u64> =
            read.events.iter().filter(|it| it.thread == tid).map(|it| it.event_id).collect();
        read.events.retain(|it| ids.contains(&it.event_id));
        read.fields.retain(|it| ids.contains(&it.event_id));
        read
    }

    fn column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> &'a T {
        batch
            .column_by_name(name)
            .unwrap_or_else(|| panic!("event_fields has {name}"))
            .as_any()
            .downcast_ref::<T>()
            .unwrap_or_else(|| panic!("{name} is of its schema's type"))
    }

    #[test]
    fn an_events_typed_fields_are_rows_of_their_own_type_and_its_message_is_alone() {
        crate::install();
        let read = std::thread::spawn(|| {
            let tid = crate::sampler::os_tid() as u32;
            let _build = flight::building(0x7a11);
            tracing::info!(target: "blaze", n = -3i64, count = 7u64, ratio = 0.5, ok = true, label = "x", shown = ?[1, 2], "typed fields");
            mine(tid)
        })
        .join()
        .expect("the test's thread");
        let [event] = read.events.as_slice() else { panic!("one event: {:?}", read.events) };
        assert_eq!(
            flight::name(event.message).as_deref(),
            Some("typed fields"),
            "the message alone"
        );
        assert_eq!(read.fields.len(), 6, "a row a field other than the message: {:?}", read.fields);
        assert!(read.fields.iter().all(|it| it.event_id == event.event_id && it.build == 0x7a11));
        let batch = batches::event_fields(&read.fields, batches::Process::this())
            .expect("a batch of event_fields");
        let keys = strings(batch.column_by_name("key").expect("key").as_ref());
        assert_eq!(keys, ["n", "count", "ratio", "ok", "label", "shown"]);
        let kinds = strings(batch.column_by_name("kind").expect("kind").as_ref());
        assert_eq!(kinds, ["i64", "u64", "f64", "bool", "str", "str"]);
        let ints = column::<Int64Array>(&batch, "value_i64");
        assert_eq!((ints.value(0), ints.null_count()), (-3, 5));
        let unsigned = column::<UInt64Array>(&batch, "value_u64");
        assert_eq!((unsigned.value(1), unsigned.null_count()), (7, 5));
        let floats = column::<Float64Array>(&batch, "value_f64");
        assert_eq!((floats.value(2), floats.null_count()), (0.5, 5));
        let bools = column::<BooleanArray>(&batch, "value_bool");
        assert_eq!((bools.value(3), bools.null_count()), (true, 5));
        let texts = column::<StringArray>(&batch, "value_str");
        assert_eq!((texts.value(4), texts.value(5), texts.null_count()), ("x", "[1, 2]", 4));
        let ordinals: Vec<u16> = read.fields.iter().map(|it| it.ordinal).collect();
        assert_eq!(ordinals, [0, 1, 2, 3, 4, 5]);
        let events =
            batches::events(&read.events, batches::Process::this()).expect("a batch of events");
        let ids = column::<UInt64Array>(&events, "event_id");
        assert_eq!(ids.value(0), event.event_id, "events and event_fields name the event alike");
    }

    /// A dictionary column's values, row by row.
    fn strings(array: &dyn Array) -> Vec<String> {
        let dict = array
            .as_any()
            .downcast_ref::<arrow_array::DictionaryArray<arrow_array::types::UInt32Type>>()
            .expect("a dictionary column");
        let values = dict.values().as_any().downcast_ref::<StringArray>().expect("of strings");
        dict.keys()
            .iter()
            .map(|key| values.value(key.expect("a key") as usize).to_owned())
            .collect()
    }

    #[test]
    fn debug_rows_are_written_for_the_build_that_failed_and_dropped_for_the_one_that_ended_well() {
        crate::install();
        let (ok, failed) = (0x7a21, 0x7a22);
        let mut read = std::thread::spawn(move || {
            let tid = crate::sampler::os_tid() as u32;
            for build in [ok, failed] {
                let _build = flight::building(build);
                tracing::debug!(target: "blaze", step = 1u64, "debug of a build");
                tracing::info!(target: "blaze", step = 2u64, "info of a build");
            }
            tracing::debug!(target: "blaze_fmt::hot", "a module path's debug is not kept");
            mine(tid)
        })
        .join()
        .expect("the test's thread");
        assert_eq!(
            read.events.len(),
            4,
            "the opted-in debug and the info events of each build: {:?}",
            read.events
        );
        let mut hold = Hold::new(Policy::Info);
        hold.sort(&mut read);
        let builds = |read: &Read| {
            read.events.iter().map(|it| (it.build, level(it.site))).collect::<Vec<_>>()
        };
        assert_eq!(
            builds(&read),
            [(ok, Some(tracing::Level::INFO)), (failed, Some(tracing::Level::INFO))],
            "debug rows held"
        );
        assert_eq!(read.fields.len(), 2, "the info events' fields");
        assert_eq!(hold.held(), 4, "two debug events and their fields");
        hold.ended(ok, false);
        hold.ended(failed, true);
        let mut next = Read::default();
        hold.sort(&mut next);
        assert_eq!(
            builds(&next),
            [(failed, Some(tracing::Level::DEBUG))],
            "the failed build's debug row alone"
        );
        let [field] = next.fields.as_slice() else { panic!("its field: {:?}", next.fields) };
        assert_eq!((field.event_id, field.value), (next.events[0].event_id, FieldValue::U64(1)));
        assert_eq!(
            (hold.held(), hold.dropped()),
            (0, 2),
            "the ok build's debug row and its field dropped"
        );
        let mut again = Read::default();
        hold.sort(&mut again);
        assert_eq!(again.rows(), 0, "a sort writes what it released once");
    }

    #[test]
    fn the_history_level_is_info_or_debug_and_anything_else_is_named() {
        assert_eq!(Policy::parse(None), Ok(Policy::Info));
        assert_eq!(Policy::parse(Some("DEBUG")), Ok(Policy::Debug));
        let error = Policy::parse(Some("trace")).expect_err("trace is no history level");
        assert!(error.to_string().contains("BLAZE_HISTORY_LEVEL=\"trace\""), "{error}");
    }

    #[test]
    fn warn_of_any_target_is_kept_and_debug_only_when_it_opts_in() {
        assert!(keeps_event("hyper::proto", "event x.rs:1", tracing::Level::WARN));
        assert!(!keeps_event("hyper::proto", "event x.rs:1", tracing::Level::INFO));
        assert!(keeps_event("blaze::store", "event x.rs:1", tracing::Level::DEBUG));
        assert!(!keeps_event("blaze_fmt", "event x.rs:1", tracing::Level::DEBUG));
        assert!(!keeps_event("blaze", "event x.rs:1", tracing::Level::TRACE));
        assert!(keeps_event_named("anything", "event x.rs:1", "error"));
    }
}
