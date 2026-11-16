//! The ledger's and a build dir's record's JSON forms, as blaze-data values: written by hand to
//! the shapes serde's derive gave them, so a ledger or record written before is read the same.
//!
//! - A struct is a map of its fields in declaration order; a field that was `#[serde(default)]`
//!   may be missing, an `Option` may be missing or null, and fields no one asked for are let
//!   be (serde's derive refused none here).
//! - An enum is externally tagged: a unit variant is its name as text (`"Pinned"`), a struct
//!   variant a map of its name to a map of its fields (`{"Head": {"checkout": ".."}}`), a newtype
//!   variant a map of its name to the value (`{"Go": {"Unused": {"for_ns": 9}}}`).
//! - A tuple is a list.

use std::alloc::Global;

use blaze_data::{Cursor, FromValue, Kind, Map, Shape, ShapeError, ToValue, Value};

use crate::{
    ledger::Ledger,
    plan::{Class, Fate, Keep, NoSpace, Outcome, Phase, Reason, RunReport, Stop, Trigger, Verdict, VolumeReport},
    record::Record,
};

type C<'v, 'p> = Cursor<'v, 'p, Global>;
type E = ShapeError<Global>;

fn key(key: &str) -> blaze_own::String<Global> {
    blaze_own::String::from_str_in(key, Global)
}

/// A map of `fields`, in their order.
fn object<const N: usize>(fields: [(&str, Value<Global>); N]) -> Value<Global> {
    let mut map = Map::with_capacity_in(N, Global);
    for (name, value) in fields {
        map.insert(key(name), value);
    }
    Value::Map(map)
}

fn text(text: &str) -> Value<Global> {
    text.to_value(Global)
}

fn int(n: u64) -> Value<Global> {
    n.to_value(Global)
}

fn list<T: ToValue<Global>>(items: &[T]) -> Value<Global> {
    items.to_value(Global)
}

fn texts(items: &[String]) -> Value<Global> {
    Value::List(blaze_own::Vec::from_iter_in(items.iter().map(|it| text(it)), Global))
}

/// A variant with fields or a payload: `{name: inner}`.
fn tagged(name: &str, inner: Value<Global>) -> Value<Global> {
    object([(name, inner)])
}

/// An externally tagged enum's variant name, and its payload's cursor when it has one.
fn variant<'v, 'q>(c: &'q C<'v, '_>) -> Result<(&'v str, Option<C<'v, 'q>>), E> {
    match c.value() {
        Value::Text(name) => Ok((name.as_str(), None)),
        Value::Map(map) if map.len() == 1 => match map.entry_at(0) {
            Some((name, _)) => Ok((name, Some(c.field(name)?))),
            None => unreachable!("a map of one entry has an entry 0"),
        },
        _ => Err(c.expected(Kind::Map)),
    }
}

/// A variant `ty` has no such name, or has it with the wrong payload.
fn unknown<T>(c: &C<'_, '_>, ty: &'static str) -> Result<T, E> {
    Err(c.error(Shape::OutOfRange { ty }))
}

/// The payload of a variant that has one.
fn payload<'v, 'q>(c: &C<'_, '_>, inner: Option<C<'v, 'q>>, ty: &'static str) -> Result<C<'v, 'q>, E> {
    match inner {
        Some(inner) => Ok(inner),
        None => unknown(c, ty),
    }
}

fn string(c: &C<'_, '_>, key: &str) -> Result<String, E> {
    Ok(c.field(key)?.text()?.to_owned())
}

fn strings(c: &C<'_, '_>, key: &str) -> Result<Vec<String>, E> {
    let field = c.field(key)?;
    let mut out = Vec::with_capacity(field.list()?.len());
    field.each(|it| {
        out.push(it.text()?.to_owned());
        Ok::<(), E>(())
    })?;
    Ok(out)
}

/// The list at `key`, each read as a `T`; `missing` when there is none (a `#[serde(default)]`).
fn items<T: FromValue<Global>>(c: &C<'_, '_>, key: &str, missing: Option<Vec<T>>) -> Result<Vec<T>, E> {
    let field = match (c.field_opt(key)?, missing) {
        (Some(field), _) => field,
        (None, Some(missing)) => return Ok(missing),
        (None, None) => return Err(c.field(key).err().unwrap_or_else(|| c.error(Shape::Missing))),
    };
    let mut out = Vec::with_capacity(field.list()?.len());
    field.each(|it| {
        out.push(it.get::<T>()?);
        Ok::<(), E>(())
    })?;
    Ok(out)
}

impl ToValue<Global> for Class {
    fn to_value(&self, _: Global) -> Value<Global> {
        text(match self {
            Class::Workspace => "Workspace",
            Class::Profile => "Profile",
            Class::Std => "Std",
            Class::Temp => "Temp",
            Class::Trash => "Trash",
        })
    }
}

impl FromValue<Global> for Class {
    fn from_value(c: &C<'_, '_>) -> Result<Class, E> {
        match c.text()? {
            "Workspace" => Ok(Class::Workspace),
            "Profile" => Ok(Class::Profile),
            "Std" => Ok(Class::Std),
            "Temp" => Ok(Class::Temp),
            "Trash" => Ok(Class::Trash),
            _ => unknown(c, "Class"),
        }
    }
}

impl ToValue<Global> for Trigger {
    fn to_value(&self, _: Global) -> Value<Global> {
        text(match self {
            Trigger::Pressure => "Pressure",
            Trigger::Urgent => "Urgent",
            Trigger::Idle => "Idle",
            Trigger::Period => "Period",
            Trigger::Asked => "Asked",
        })
    }
}

impl FromValue<Global> for Trigger {
    fn from_value(c: &C<'_, '_>) -> Result<Trigger, E> {
        match c.text()? {
            "Pressure" => Ok(Trigger::Pressure),
            "Urgent" => Ok(Trigger::Urgent),
            "Idle" => Ok(Trigger::Idle),
            "Period" => Ok(Trigger::Period),
            "Asked" => Ok(Trigger::Asked),
            _ => unknown(c, "Trigger"),
        }
    }
}

impl ToValue<Global> for Phase {
    fn to_value(&self, _: Global) -> Value<Global> {
        text(match self {
            Phase::Scan => "Scan",
            Phase::Mark => "Mark",
            Phase::Purge => "Purge",
            Phase::Sweep => "Sweep",
            Phase::Unlink => "Unlink",
        })
    }
}

impl FromValue<Global> for Phase {
    fn from_value(c: &C<'_, '_>) -> Result<Phase, E> {
        match c.text()? {
            "Scan" => Ok(Phase::Scan),
            "Mark" => Ok(Phase::Mark),
            "Purge" => Ok(Phase::Purge),
            "Sweep" => Ok(Phase::Sweep),
            "Unlink" => Ok(Phase::Unlink),
            _ => unknown(c, "Phase"),
        }
    }
}

impl ToValue<Global> for Reason {
    fn to_value(&self, _: Global) -> Value<Global> {
        match self {
            Reason::CheckoutGone { checkout } => tagged("CheckoutGone", object([("checkout", text(checkout))])),
            Reason::Unused { for_ns } => tagged("Unused", object([("for_ns", int(*for_ns))])),
            Reason::NoDaemon { for_ns } => tagged("NoDaemon", object([("for_ns", int(*for_ns))])),
            Reason::Temporary { for_ns } => tagged("Temporary", object([("for_ns", int(*for_ns))])),
            Reason::OverCap { workspace, cap_bytes } => {
                tagged("OverCap", object([("workspace", text(workspace)), ("cap_bytes", int(*cap_bytes))]))
            }
            Reason::OverBudget { to_free_bytes } => {
                tagged("OverBudget", object([("to_free_bytes", int(*to_free_bytes))]))
            }
            Reason::Within { dir } => tagged("Within", object([("dir", text(dir))])),
            Reason::Trash { for_ns } => tagged("Trash", object([("for_ns", int(*for_ns))])),
        }
    }
}

impl FromValue<Global> for Reason {
    fn from_value(c: &C<'_, '_>) -> Result<Reason, E> {
        let (name, inner) = variant(c)?;
        let f = payload(c, inner, "Reason")?;
        match name {
            "CheckoutGone" => Ok(Reason::CheckoutGone { checkout: string(&f, "checkout")? }),
            "Unused" => Ok(Reason::Unused { for_ns: f.req("for_ns")? }),
            "NoDaemon" => Ok(Reason::NoDaemon { for_ns: f.req("for_ns")? }),
            "Temporary" => Ok(Reason::Temporary { for_ns: f.req("for_ns")? }),
            "OverCap" => Ok(Reason::OverCap { workspace: string(&f, "workspace")?, cap_bytes: f.req("cap_bytes")? }),
            "OverBudget" => Ok(Reason::OverBudget { to_free_bytes: f.req("to_free_bytes")? }),
            "Within" => Ok(Reason::Within { dir: string(&f, "dir")? }),
            "Trash" => Ok(Reason::Trash { for_ns: f.req("for_ns")? }),
            _ => unknown(c, "Reason"),
        }
    }
}

impl ToValue<Global> for Keep {
    fn to_value(&self, _: Global) -> Value<Global> {
        match self {
            Keep::Head { checkout } => tagged("Head", object([("checkout", text(checkout))])),
            Keep::Pinned => text("Pinned"),
            Keep::Held { agent } => tagged("Held", object([("agent", int(*agent))])),
            Keep::Leased => text("Leased"),
            Keep::Transaction => text("Transaction"),
            Keep::Grace { left_ns } => tagged("Grace", object([("left_ns", int(*left_ns))])),
            Keep::Fresh => text("Fresh"),
            Keep::Refused { why } => tagged("Refused", object([("why", text(why))])),
            Keep::UsedAfterMark => text("UsedAfterMark"),
            Keep::LeasedAtSweep => text("LeasedAtSweep"),
            Keep::NestedLeasedAtSweep { dir } => tagged("NestedLeasedAtSweep", object([("dir", text(dir))])),
            Keep::LeftInTrash { why } => tagged("LeftInTrash", object([("why", text(why))])),
            Keep::Gone => text("Gone"),
            Keep::Failed { why } => tagged("Failed", object([("why", text(why))])),
            Keep::NotReached => text("NotReached"),
        }
    }
}

impl FromValue<Global> for Keep {
    fn from_value(c: &C<'_, '_>) -> Result<Keep, E> {
        let (name, inner) = variant(c)?;
        let unit = |keep: Keep| match inner.is_none() {
            true => Ok(keep),
            false => unknown(c, "Keep"),
        };
        match name {
            "Pinned" => return unit(Keep::Pinned),
            "Leased" => return unit(Keep::Leased),
            "Transaction" => return unit(Keep::Transaction),
            "Fresh" => return unit(Keep::Fresh),
            "UsedAfterMark" => return unit(Keep::UsedAfterMark),
            "LeasedAtSweep" => return unit(Keep::LeasedAtSweep),
            "Gone" => return unit(Keep::Gone),
            "NotReached" => return unit(Keep::NotReached),
            _ => {}
        }
        let f = payload(c, inner, "Keep")?;
        match name {
            "Head" => Ok(Keep::Head { checkout: string(&f, "checkout")? }),
            "Held" => Ok(Keep::Held { agent: f.req("agent")? }),
            "Grace" => Ok(Keep::Grace { left_ns: f.req("left_ns")? }),
            "Refused" => Ok(Keep::Refused { why: string(&f, "why")? }),
            "NestedLeasedAtSweep" => Ok(Keep::NestedLeasedAtSweep { dir: string(&f, "dir")? }),
            "LeftInTrash" => Ok(Keep::LeftInTrash { why: string(&f, "why")? }),
            "Failed" => Ok(Keep::Failed { why: string(&f, "why")? }),
            _ => unknown(c, "Keep"),
        }
    }
}

impl ToValue<Global> for Fate {
    fn to_value(&self, _: Global) -> Value<Global> {
        match self {
            Fate::Go(reason) => tagged("Go", reason.to_value(Global)),
            Fate::Keep(keep) => tagged("Keep", keep.to_value(Global)),
        }
    }
}

impl FromValue<Global> for Fate {
    fn from_value(c: &C<'_, '_>) -> Result<Fate, E> {
        let (name, inner) = variant(c)?;
        let inner = payload(c, inner, "Fate")?;
        match name {
            "Go" => Ok(Fate::Go(inner.get()?)),
            "Keep" => Ok(Fate::Keep(inner.get()?)),
            _ => unknown(c, "Fate"),
        }
    }
}

impl ToValue<Global> for Verdict {
    fn to_value(&self, _: Global) -> Value<Global> {
        object([
            ("path", text(&self.path)),
            ("class", self.class.to_value(Global)),
            ("bytes", int(self.bytes)),
            ("used_ns", int(self.used_ns)),
            ("fate", self.fate.to_value(Global)),
        ])
    }
}

impl FromValue<Global> for Verdict {
    fn from_value(c: &C<'_, '_>) -> Result<Verdict, E> {
        Ok(Verdict {
            path: string(c, "path")?,
            class: c.req("class")?,
            bytes: c.req("bytes")?,
            used_ns: c.req("used_ns")?,
            fate: c.req("fate")?,
        })
    }
}

impl ToValue<Global> for VolumeReport {
    fn to_value(&self, _: Global) -> Value<Global> {
        object([
            ("area", text(&self.area)),
            ("total_bytes", int(self.total_bytes)),
            ("free_bytes", int(self.free_bytes)),
            ("comfortable_bytes", int(self.comfortable_bytes)),
            ("floor_bytes", int(self.floor_bytes)),
            ("blaze_bytes", int(self.blaze_bytes)),
            ("share_bytes", int(self.share_bytes)),
            ("pressure", self.pressure.to_value(Global)),
            ("to_free_bytes", int(self.to_free_bytes)),
        ])
    }
}

impl FromValue<Global> for VolumeReport {
    fn from_value(c: &C<'_, '_>) -> Result<VolumeReport, E> {
        Ok(VolumeReport {
            area: string(c, "area")?,
            total_bytes: c.req("total_bytes")?,
            free_bytes: c.req("free_bytes")?,
            comfortable_bytes: c.req("comfortable_bytes")?,
            floor_bytes: c.req("floor_bytes")?,
            blaze_bytes: c.req("blaze_bytes")?,
            share_bytes: c.req("share_bytes")?,
            pressure: c.req("pressure")?,
            to_free_bytes: c.req("to_free_bytes")?,
        })
    }
}

/// `kept` is a list of `[path, bytes, keep]`.
impl ToValue<Global> for NoSpace {
    fn to_value(&self, _: Global) -> Value<Global> {
        let kept = blaze_own::Vec::from_iter_in(
            self.kept.iter().map(|(path, bytes, keep)| {
                Value::List(blaze_own::Vec::from_array_in([text(path), int(*bytes), keep.to_value(Global)], Global))
            }),
            Global,
        );
        object([
            ("area", text(&self.area)),
            ("free_bytes", int(self.free_bytes)),
            ("floor_bytes", int(self.floor_bytes)),
            ("kept", Value::List(kept)),
            ("skipped", texts(&self.skipped)),
        ])
    }
}

impl FromValue<Global> for NoSpace {
    fn from_value(c: &C<'_, '_>) -> Result<NoSpace, E> {
        let field = c.field("kept")?;
        let mut kept = Vec::with_capacity(field.list()?.len());
        field.each(|it| {
            if it.list()?.len() != 3 {
                return unknown(&it, "(String, u64, Keep)");
            }
            kept.push((it.index(0)?.text()?.to_owned(), it.index(1)?.get()?, it.index(2)?.get()?));
            Ok::<(), E>(())
        })?;
        Ok(NoSpace {
            area: string(c, "area")?,
            free_bytes: c.req("free_bytes")?,
            floor_bytes: c.req("floor_bytes")?,
            kept,
            skipped: match c.field_opt("skipped")? {
                Some(_) => strings(c, "skipped")?,
                None => Vec::new(),
            },
        })
    }
}

impl ToValue<Global> for Stop {
    fn to_value(&self, _: Global) -> Value<Global> {
        match self {
            Stop::Fuel { fuel } => tagged("Fuel", object([("fuel", fuel.to_value(Global))])),
            Stop::Deadline { run_ns } => tagged("Deadline", object([("run_ns", int(*run_ns))])),
            Stop::ScanFailed { why } => tagged("ScanFailed", object([("why", text(why))])),
            Stop::Slice { bytes } => tagged("Slice", object([("bytes", int(*bytes))])),
            Stop::Clock { now_ns, newest_ns } => {
                tagged("Clock", object([("now_ns", int(*now_ns)), ("newest_ns", int(*newest_ns))]))
            }
            Stop::Preempted { by } => tagged("Preempted", object([("by", by.to_value(Global))])),
        }
    }
}

impl FromValue<Global> for Stop {
    fn from_value(c: &C<'_, '_>) -> Result<Stop, E> {
        let (name, inner) = variant(c)?;
        let f = payload(c, inner, "Stop")?;
        match name {
            "Fuel" => Ok(Stop::Fuel { fuel: f.req("fuel")? }),
            "Deadline" => Ok(Stop::Deadline { run_ns: f.req("run_ns")? }),
            "ScanFailed" => Ok(Stop::ScanFailed { why: string(&f, "why")? }),
            "Slice" => Ok(Stop::Slice { bytes: f.req("bytes")? }),
            "Clock" => Ok(Stop::Clock { now_ns: f.req("now_ns")?, newest_ns: f.req("newest_ns")? }),
            "Preempted" => Ok(Stop::Preempted { by: f.req("by")? }),
            _ => unknown(c, "Stop"),
        }
    }
}

impl ToValue<Global> for Outcome {
    fn to_value(&self, _: Global) -> Value<Global> {
        match self {
            Outcome::Done => text("Done"),
            Outcome::Stopped { phase, why, done, left } => tagged(
                "Stopped",
                object([
                    ("phase", phase.to_value(Global)),
                    ("why", why.to_value(Global)),
                    ("done", done.to_value(Global)),
                    ("left", left.to_value(Global)),
                ]),
            ),
        }
    }
}

impl FromValue<Global> for Outcome {
    fn from_value(c: &C<'_, '_>) -> Result<Outcome, E> {
        match variant(c)? {
            ("Done", None) => Ok(Outcome::Done),
            ("Stopped", Some(f)) => Ok(Outcome::Stopped {
                phase: f.req("phase")?,
                why: f.req("why")?,
                done: f.req("done")?,
                left: f.req("left")?,
            }),
            _ => unknown(c, "Outcome"),
        }
    }
}

impl ToValue<Global> for RunReport {
    fn to_value(&self, _: Global) -> Value<Global> {
        object([
            ("run", int(self.run)),
            ("why", self.why.to_value(Global)),
            ("dry_run", Value::Bool(self.dry_run)),
            ("started_ns", int(self.started_ns)),
            ("ended_ns", int(self.ended_ns)),
            ("outcome", self.outcome.to_value(Global)),
            ("objects", int(self.objects)),
            ("total_bytes", int(self.total_bytes)),
            ("marked_objects", int(self.marked_objects)),
            ("marked_bytes", int(self.marked_bytes)),
            ("swept_objects", int(self.swept_objects)),
            ("swept_bytes", int(self.swept_bytes)),
            ("kept_objects", int(self.kept_objects)),
            ("kept_bytes", int(self.kept_bytes)),
            ("unlinked_objects", int(self.unlinked_objects)),
            ("unlinked_bytes", int(self.unlinked_bytes)),
            ("over_budget_bytes", int(self.over_budget_bytes)),
            ("pressure", self.pressure.to_value(Global)),
            ("volumes", list(&self.volumes)),
            ("short", self.short.to_value(Global)),
            ("clock_checked", Value::Bool(self.clock_checked)),
            ("problems", texts(&self.problems)),
            ("verdicts", list(&self.verdicts)),
        ])
    }
}

impl FromValue<Global> for RunReport {
    fn from_value(c: &C<'_, '_>) -> Result<RunReport, E> {
        Ok(RunReport {
            run: c.req("run")?,
            why: c.req("why")?,
            dry_run: c.req("dry_run")?,
            started_ns: c.req("started_ns")?,
            ended_ns: c.req("ended_ns")?,
            outcome: c.req("outcome")?,
            objects: c.req("objects")?,
            total_bytes: c.req("total_bytes")?,
            marked_objects: c.req("marked_objects")?,
            marked_bytes: c.req("marked_bytes")?,
            swept_objects: c.req("swept_objects")?,
            swept_bytes: c.req("swept_bytes")?,
            kept_objects: c.req("kept_objects")?,
            kept_bytes: c.req("kept_bytes")?,
            unlinked_objects: c.req("unlinked_objects")?,
            unlinked_bytes: c.req("unlinked_bytes")?,
            over_budget_bytes: c.req("over_budget_bytes")?,
            pressure: c.opt("pressure")?.unwrap_or(0),
            volumes: items(c, "volumes", Some(Vec::new()))?,
            short: c.opt("short")?,
            clock_checked: c.opt("clock_checked")?.unwrap_or(false),
            problems: strings(c, "problems")?,
            verdicts: items(c, "verdicts", None)?,
        })
    }
}

impl ToValue<Global> for Ledger {
    fn to_value(&self, _: Global) -> Value<Global> {
        object([
            ("format", self.format.to_value(Global)),
            ("last_end_ns", self.last_end_ns.to_value(Global)),
            ("runs", list(&self.runs)),
        ])
    }
}

impl FromValue<Global> for Ledger {
    fn from_value(c: &C<'_, '_>) -> Result<Ledger, E> {
        Ok(Ledger { format: c.req("format")?, last_end_ns: c.opt("last_end_ns")?, runs: items(c, "runs", None)? })
    }
}

/// `profiles` is a list of `[name, used_ns]`.
impl ToValue<Global> for Record {
    fn to_value(&self, _: Global) -> Value<Global> {
        let profiles = blaze_own::Vec::from_iter_in(
            self.profiles
                .iter()
                .map(|(name, used)| Value::List(blaze_own::Vec::from_array_in([text(name), int(*used)], Global))),
            Global,
        );
        object([
            ("format", self.format.to_value(Global)),
            ("checkout", self.checkout.as_deref().to_value(Global)),
            ("used_ns", int(self.used_ns)),
            ("profiles", Value::List(profiles)),
        ])
    }
}

impl FromValue<Global> for Record {
    fn from_value(c: &C<'_, '_>) -> Result<Record, E> {
        let mut profiles = Vec::new();
        if let Some(field) = c.field_opt("profiles")? {
            field.each(|it| {
                if it.list()?.len() != 2 {
                    return unknown(&it, "(String, u64)");
                }
                profiles.push((it.index(0)?.text()?.to_owned(), it.index(1)?.get()?));
                Ok::<(), E>(())
            })?;
        }
        let checkout = match c.field_opt("checkout")? {
            Some(it) if !it.is_null() => Some(it.text()?.to_owned()),
            _ => None,
        };
        Ok(Record { format: c.req("format")?, checkout, used_ns: c.req("used_ns")?, profiles })
    }
}

/// JSON as serde_json's `to_vec_pretty` wrote it: two spaces, fields in declaration order.
pub(crate) fn pretty<T: ToValue<Global>>(value: &T) -> Vec<u8> {
    let options = blaze_json::WriteOptions {
        keys: blaze_json::KeyOrder::Insertion,
        ..blaze_json::WriteOptions::PRETTY
    };
    match blaze_json::to_string_in(&value.to_value(Global), options, Global) {
        Ok(text) => text.as_str().as_bytes().to_vec(),
        Err(why) => unreachable!("numbers, text, lists and maps have a JSON form: {why}"),
    }
}

/// Compact JSON, fields in declaration order: a fate on one line of the run's text.
pub(crate) fn compact<T: ToValue<Global>>(value: &T) -> String {
    let options = blaze_json::WriteOptions { keys: blaze_json::KeyOrder::Insertion, ..blaze_json::WriteOptions::COMPACT };
    match blaze_json::to_string_in(&value.to_value(Global), options, Global) {
        Ok(text) => text.as_str().to_owned(),
        Err(why) => unreachable!("numbers, text, lists and maps have a JSON form: {why}"),
    }
}

/// `bytes` read as JSON, then as a `T`; the error names the byte or the field.
pub(crate) fn read<T: FromValue<Global>>(bytes: &[u8]) -> Result<T, String> {
    let value = blaze_json::read_in(bytes, blaze_json::Limits::DEFAULT, Global).map_err(|e| e.to_string())?;
    Cursor::root(&value, Global).get().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ledger as serde_json's `to_vec_pretty` of the derive wrote it reads, and writes back to
    /// the same bytes.
    #[test]
    fn a_ledger_of_serdes_form_reads_and_writes_back_the_same() {
        let old = r#"{
  "format": 1,
  "last_end_ns": 7,
  "runs": [
    {
      "run": 3,
      "why": "Asked",
      "dry_run": false,
      "started_ns": 1,
      "ended_ns": 7,
      "outcome": {
        "Stopped": {
          "phase": "Sweep",
          "why": {
            "Clock": {
              "now_ns": 9,
              "newest_ns": 2
            }
          },
          "done": 1,
          "left": 2
        }
      },
      "objects": 2,
      "total_bytes": 10,
      "marked_objects": 0,
      "marked_bytes": 0,
      "swept_objects": 1,
      "swept_bytes": 4,
      "kept_objects": 1,
      "kept_bytes": 6,
      "unlinked_objects": 0,
      "unlinked_bytes": 0,
      "over_budget_bytes": 0,
      "pressure": 5,
      "volumes": [],
      "short": {
        "area": "/a",
        "free_bytes": 1,
        "floor_bytes": 2,
        "kept": [
          [
            "/a/b",
            6,
            {
              "Head": {
                "checkout": "/c"
              }
            }
          ],
          [
            "/a/d",
            1,
            "Pinned"
          ]
        ],
        "skipped": []
      },
      "clock_checked": true,
      "problems": [
        "p"
      ],
      "verdicts": [
        {
          "path": "/a/b",
          "class": "Workspace",
          "bytes": 4,
          "used_ns": 1,
          "fate": {
            "Go": {
              "Unused": {
                "for_ns": 9
              }
            }
          }
        }
      ]
    }
  ]
}"#;
        let ledger: Ledger = read(old.as_bytes()).unwrap();
        assert_eq!(ledger.runs[0].verdicts[0].fate, Fate::Go(Reason::Unused { for_ns: 9 }));
        assert_eq!(String::from_utf8(pretty(&ledger)).unwrap(), old);
        // A ledger from before `pressure`, `volumes`, `short` and `clock_checked` reads them as
        // their defaults.
        let older = old
            .replace("      \"pressure\": 5,\n", "")
            .replace("      \"volumes\": [],\n", "")
            .replace("      \"clock_checked\": true,\n", "");
        let ledger: Ledger = read(older.as_bytes()).unwrap();
        assert_eq!((ledger.runs[0].pressure, ledger.runs[0].clock_checked), (0, false));
    }
}
