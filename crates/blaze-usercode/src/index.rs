//! Finding a run's result in the store (docs/v2/plan/lanes/usercode/notes.md, "Keys").
//!
//! A result is kept under [`RunKey::traced`]: the run's code, what it was given, and every
//! input it read with its value then. To find one for a run about to happen, the store keeps
//! beside it the run's **shapes**: which paths, listings and variables a run of this
//! [`RunKey`] read, without their values. A shape is kept once, in the first free slot of a
//! chain under the run key ([`slot`]); a run key has as many shapes as its runs read
//! different things (usually one: a script reads the same files every run, whatever is in
//! them). Finding is then, for each shape, the values of its inputs now ([`Shape::values`])
//! and one get of the key they make: no list of past results is searched.
//!
//! The store never replaces a value, so the chain only grows, a slot at a time; two daemons
//! that keep different shapes in one slot see the [`Mismatch`] and move to the next.

use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
};

use blaze_cas::{Corrupt, Guard, Key, Kind, Mismatch, Reader, Store, Ticket, Unit};
use blaze_core::{Traced, Untraceable};
use blaze_format::cursor::{self, Put};
use blaze_ids::{ContentId, Hasher};
use blaze_mem::{Tag, frame};

use crate::{
    RunKey,
    trace::{self, ALL_ENV_NAME, Inputs, os_from},
};

/// What a run read, without the values.
#[derive(PartialEq, Eq, Default, Debug)]
pub struct Shape {
    pub files: Vec<PathBuf>,
    pub dirs: Vec<PathBuf>,
    /// The variables by name, with [`ALL_ENV_NAME`] for a run that read the whole
    /// environment.
    pub env: Vec<OsString>,
    pub untraceable: Vec<Untraceable>,
}

impl Shape {
    /// The shape of `traced`, in the order [`trace::normalize`] leaves it, which is sorted by
    /// reference and never copied: the shape owns only its names.
    pub fn of(traced: &Traced) -> Shape {
        frame::task(Tag::Usercode, |frame| {
            let env = trace::sorted_in(&traced.env, frame);
            let all = env.iter().any(|it| it.0 == ALL_ENV_NAME);
            // The variables a run that read them all saw are any the environment has then;
            // what it asked for by name and was not there is named still.
            let env = env
                .iter()
                .filter(|(name, value)| !all || name == ALL_ENV_NAME || value.is_none())
                .map(|it| it.0.clone()) // clone: the names the shape keeps
                .collect(); // alloc: the shape the caller keeps
            Shape {
                // clone: the paths the shape keeps
                files: trace::sorted_in(&traced.files, frame).iter().map(|it| it.0.clone()).collect(),
                // clone: the paths the shape keeps
                dirs: trace::sorted_in(&traced.dirs, frame).iter().map(|it| it.0.clone()).collect(),
                env,
                untraceable: trace::sorted_in(&traced.untraceable, frame).into_iter().copied().collect(), // alloc: kept
            }
        })
    }

    /// The inputs of this shape as `inputs` has them: what a run that read them would have
    /// recorded. A directory that is no longer there is a file that is not there, as
    /// [`trace::assemble`] has it. The [`Traced`] is the caller's to keep: a cache check
    /// hashes the inputs in place instead ([`find`]).
    pub fn values<I: Inputs>(&self, inputs: &mut I) -> Result<Traced, I::Error> {
        let mut traced = Traced::default();
        for path in &self.files {
            match trace::walked(path) {
                Some((dir, package)) => match inputs.tree(dir, package)? {
                    Some(hash) => traced.dirs.push((path.clone(), hash)), // clone: the Traced the caller keeps
                    None => traced.files.push((path.clone(), None)), // clone: the Traced the caller keeps
                },
                None => traced.files.push((path.clone(), inputs.file(path)?)), // clone: the Traced the caller keeps
            }
        }
        for path in &self.dirs {
            let hash = match trace::walked(path) {
                Some((dir, package)) => inputs.tree(dir, package)?,
                None => inputs.dir(path)?,
            };
            match hash {
                Some(hash) => traced.dirs.push((path.clone(), hash)), // clone: the Traced the caller keeps
                None => traced.files.push((path.clone(), None)), // clone: the Traced the caller keeps
            }
        }
        for name in &self.env {
            if name == ALL_ENV_NAME {
                let all = inputs.env_all();
                traced.env.push((ALL_ENV_NAME.into(), Some(trace::names_hash(all).into())));
                // clone: the Traced the caller keeps
                traced.env.extend(all.iter().map(|(name, value)| (name.clone(), Some(value.clone()))));
            } else {
                traced.env.push((name.clone(), inputs.env(name).map(OsStr::to_owned))); // clone: kept, as above
            }
        }
        traced.untraceable = self.untraceable.clone(); // clone: kept, as above
        trace::normalize(&mut traced);
        Ok(traced)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = SHAPE_MAGIC.to_vec(); // alloc: the bytes the caller keeps
        for list in [&self.files, &self.dirs] {
            out.put_u64(list.len() as u64);
            for path in list {
                out.put_bytes_u64(&trace::os_bytes(path.as_os_str()));
            }
        }
        out.put_u64(self.env.len() as u64);
        for name in &self.env {
            out.put_bytes_u64(&trace::os_bytes(name));
        }
        out.put_u64(self.untraceable.len() as u64);
        for it in &self.untraceable {
            out.put_bytes_u64(trace::untraceable_name(*it).as_bytes());
        }
        out
    }

    /// The shape [`Shape::encode`] wrote, which the caller keeps (an explanation of a rerun).
    /// Each list's count is checked against the bytes left at one byte an entry.
    pub fn decode(bytes: &[u8]) -> Result<Shape, String> {
        let rest = bytes
            .strip_prefix(SHAPE_MAGIC.as_slice())
            .ok_or_else(|| "not a shape".to_owned())?; // alloc: error path
        let mut at = cursor::Reader::new(rest);
        let mut shape = Shape::default();
        for _ in 0..at.count_u64(1)? {
            shape.files.push(PathBuf::from(os_from(at.bytes_u64()?)?));
        }
        for _ in 0..at.count_u64(1)? {
            shape.dirs.push(PathBuf::from(os_from(at.bytes_u64()?)?));
        }
        for _ in 0..at.count_u64(1)? {
            shape.env.push(os_from(at.bytes_u64()?)?);
        }
        for _ in 0..at.count_u64(1)? {
            let name = at.bytes_u64()?;
            let untraceable = std::str::from_utf8(name).ok().and_then(trace::untraceable_of);
            // alloc: error path
            let why = || format!("no untraceable is {:?}", String::from_utf8_lossy(name));
            shape.untraceable.push(untraceable.ok_or_else(why)?);
        }
        match at.is_done() {
            true => Ok(shape),
            false => Err("the shape has bytes after its end".to_owned()), // alloc: error path
        }
    }
}

/// Another on Windows, whose names are [`trace::os_bytes`]' UTF-16 units: a shape of an
/// older blaze's bytes there is not one.
const SHAPE_MAGIC: &[u8; 16] = if cfg!(windows) { b"blaze v2 shape 2" } else { b"blaze v2 shape 1" };

/// The key of slot `n` of `run`'s chain of shapes.
pub fn slot(run: &RunKey, n: u32) -> Key {
    Key(Hasher::new("blaze v2 run shape slot 1")
        .id(&run.code)
        .id(&run.given)
        .update(&n.to_le_bytes())
        .finish())
}

/// The store's key of a result.
pub fn result_key(run: &RunKey, traced: &Traced) -> Key {
    Key(run.traced(traced))
}

/// The run key [`values`] hashes under: no library and no call, which the image of the
/// calling crate keys itself (`hir_def::deps`).
pub const VALUES: RunKey = RunKey { code: ContentId([0; 32]), given: ContentId([0; 32]) };

/// The hash of the values the names of the shape `bytes` have as `inputs` has them now, as
/// [`VALUES`]`.traced` hashes a run's: equal exactly when every name has the value it had.
/// `None` when a directory of the shape is no longer one.
pub fn values<I: Inputs>(bytes: &[u8], inputs: &mut I) -> Result<Option<ContentId>, FindError<I::Error>> {
    match keyed(bytes, &VALUES, false, inputs) {
        Ok(keyed) => Ok(keyed.map(|(key, _)| key)),
        Err(Keyed::NotAShape(why)) => Err(FindError::NotAShape { key: Key(ContentId([0; 32])), why }),
        Err(Keyed::Inputs(error)) => Err(FindError::Inputs(error)),
    }
}

/// What [`find`] found.
#[derive(Debug)]
pub enum Found<'s> {
    /// A result whose every input is as it is now: its key, the value kept and the shape it
    /// was found by, in place; with [`find_reads`], also the [`values`] of that shape, hashed
    /// in the same pass as the key.
    Hit { key: Key, value: &'s [u8], shape: &'s [u8], values: Option<ContentId> },
    /// No result of a shape the run has had is for the inputs as they are now; `shapes` is
    /// how many it has had.
    Miss { shapes: u32 },
}

#[derive(Debug)]
pub enum FindError<E> {
    Corrupt(Corrupt),
    /// A slot that holds what is not a shape: the store's bytes were checked, so a key of
    /// another schema is in this slot.
    NotAShape { key: Key, why: String },
    Inputs(E),
}

/// The result of `run` for `inputs` as they are now, if the store has one: read in place on
/// any thread, the hit's bytes valid while `guard`'s task runs.
pub fn find<'s, I: Inputs>(
    store: &'s Reader,
    guard: &'s Guard,
    run: &RunKey,
    inputs: &mut I,
) -> Result<Found<'s>, FindError<I::Error>> {
    search_in(|key| Ok(store.get(guard, key)?.map(|entry| entry.bytes)), run, false, inputs)
}

/// [`find`], with a hit's [`values`] hashed in the same pass as its key: what a caller that
/// reports what the run read (`hir_def::deps`, through `proc_macro_api::kept`) asks, at no second read
/// of any input.
pub fn find_reads<'s, I: Inputs>(
    store: &'s Reader,
    guard: &'s Guard,
    run: &RunKey,
    inputs: &mut I,
) -> Result<Found<'s>, FindError<I::Error>> {
    search_in(|key| Ok(store.get(guard, key)?.map(|entry| entry.bytes)), run, true, inputs)
}

/// [`find`] for the store's own thread, which holds it.
pub(crate) fn find_held<'s, I: Inputs>(
    store: &'s Store,
    run: &RunKey,
    inputs: &mut I,
) -> Result<Found<'s>, FindError<I::Error>> {
    search_in(|key| Ok(store.get(key)?.map(|entry| entry.bytes)), run, false, inputs)
}

pub(crate) fn search<'s, I: Inputs>(
    get: impl Fn(&Key) -> Result<Option<&'s [u8]>, Corrupt>,
    run: &RunKey,
    inputs: &mut I,
) -> Result<Found<'s>, FindError<I::Error>> {
    search_in(get, run, false, inputs)
}

fn search_in<'s, I: Inputs>(
    get: impl Fn(&Key) -> Result<Option<&'s [u8]>, Corrupt>,
    run: &RunKey,
    with_values: bool,
    inputs: &mut I,
) -> Result<Found<'s>, FindError<I::Error>> {
    let mut n = 0;
    loop {
        let at = slot(run, n);
        let Some(bytes) = get(&at).map_err(FindError::Corrupt)? else {
            return Ok(Found::Miss { shapes: n });
        };
        let keyed = keyed(bytes, run, with_values, inputs).map_err(|error| match error {
            Keyed::NotAShape(why) => FindError::NotAShape { key: at, why },
            Keyed::Inputs(error) => FindError::Inputs(error),
        })?;
        if let Some((key, values)) = keyed
            && let Some(value) = get(&Key(key)).map_err(FindError::Corrupt)?
        {
            return Ok(Found::Hit { key: Key(key), value, shape: bytes, values });
        }
        n += 1;
    }
}

/// What a check hashes a shape's inputs into: the result key and, when asked, the
/// [`VALUES`] hash of the same entries, each input read once for both.
struct Keys {
    run: trace::Keyer,
    values: Option<trace::Keyer>,
}

impl Keys {
    fn file(&mut self, path: &[u8], hash: Option<&[u8; 32]>) {
        self.run.file(path, hash);
        if let Some(values) = &mut self.values {
            values.file(path, hash);
        }
    }

    fn dir(&mut self, path: &[u8], hash: &[u8; 32]) {
        self.run.dir(path, hash);
        if let Some(values) = &mut self.values {
            values.dir(path, hash);
        }
    }

    fn env(&mut self, name: &[u8], value: Option<&[u8]>) {
        self.run.env(name, value);
        if let Some(values) = &mut self.values {
            values.env(name, value);
        }
    }

    fn untraceable(&mut self, name: &[u8]) {
        self.run.untraceable(name);
        if let Some(values) = &mut self.values {
            values.untraceable(name);
        }
    }

    fn finish(self) -> (ContentId, Option<ContentId>) {
        (self.run.finish(), self.values.map(trace::Keyer::finish))
    }
}

enum Keyed<E> {
    NotAShape(String),
    Inputs(E),
}

/// The key of the result of `run` that read what the shape `bytes` is, with its inputs as
/// `inputs` has them now: the shape read in place and each input hashed into the key as it is
/// asked, so a check builds no [`Traced`] and copies no name. `None` when a directory of the
/// shape is no longer one: what read it then has no result that read this.
///
/// What it hashes is what [`RunKey::traced`] of [`Shape::values`] hashes: the shape is the
/// normalized order, and the whole environment, sorted as [`Inputs::env_all`] lends it, is
/// merged into the names the run asked for by the same order, the names' own (on Windows
/// not their bytes'). With `with_values`, the [`VALUES`] hash of the same entries comes
/// with the key.
fn keyed<I: Inputs>(
    bytes: &[u8],
    run: &RunKey,
    with_values: bool,
    inputs: &mut I,
) -> Result<Option<(ContentId, Option<ContentId>)>, Keyed<I::Error>> {
    let bad = Keyed::NotAShape;
    let short = |error: cursor::Error| Keyed::NotAShape(error.into());
    let all_name = trace::os_bytes(OsStr::new(ALL_ENV_NAME));
    // alloc: error path
    let rest = bytes.strip_prefix(SHAPE_MAGIC.as_slice()).ok_or_else(|| bad("not a shape".to_owned()))?;
    let mut at = cursor::Reader::new(rest);
    let mut keyer =
        Keys { run: trace::Keyer::new(run), values: with_values.then(|| trace::Keyer::new(&VALUES)) };
    for _ in 0..at.count_u64(1).map_err(short)? {
        let bytes = at.bytes_u64().map_err(short)?;
        let path = trace::path_in(bytes).map_err(bad)?;
        let hash = match trace::walked(&path) {
            // A walked directory that was not there: one there now is what the run did not
            // read.
            Some((dir, package)) => match inputs.tree(dir, package).map_err(Keyed::Inputs)? {
                Some(_) => return Ok(None),
                None => None,
            },
            None => inputs.file(&path).map_err(Keyed::Inputs)?,
        };
        keyer.file(bytes, hash.as_ref());
    }
    for _ in 0..at.count_u64(1).map_err(short)? {
        let bytes = at.bytes_u64().map_err(short)?;
        let path = trace::path_in(bytes).map_err(bad)?;
        let hash = match trace::walked(&path) {
            Some((dir, package)) => inputs.tree(dir, package),
            None => inputs.dir(&path),
        };
        match hash.map_err(Keyed::Inputs)? {
            Some(hash) => keyer.dir(bytes, &hash),
            None => return Ok(None),
        }
    }
    let names = at.count_u64(1).map_err(short)?;
    let mut asked = at;
    let mut whole = false;
    for _ in 0..names {
        whole |= asked.bytes_u64().map_err(short)? == &*all_name;
    }
    if whole {
        let all = inputs.env_all();
        let hex = trace::names_hex(all);
        let hex = trace::os_bytes(OsStr::new(std::str::from_utf8(&hex).expect("a hash in hex is ASCII")));
        let mut all = all.iter().peekable();
        for _ in 0..names {
            let name = at.bytes_u64().map_err(short)?;
            let named = trace::os_in(name).map_err(bad)?;
            while let Some((set, value)) = all.next_if(|it| it.0.as_os_str() < &*named) {
                keyer.env(&trace::os_bytes(set), Some(&*trace::os_bytes(value)));
            }
            if name == &*all_name {
                keyer.env(name, Some(&*hex));
            } else if let Some((_, value)) = all.next_if(|it| it.0.as_os_str() == &*named) {
                keyer.env(name, Some(&*trace::os_bytes(value)));
            } else {
                // What the whole environment has not is not set.
                keyer.env(name, None);
            }
        }
        for (set, value) in all {
            keyer.env(&trace::os_bytes(set), Some(&*trace::os_bytes(value)));
        }
    } else {
        for _ in 0..names {
            let name = at.bytes_u64().map_err(short)?;
            let value = inputs.env(&trace::os_in(name).map_err(bad)?).map(trace::os_bytes);
            keyer.env(name, value.as_deref());
        }
    }
    for _ in 0..at.count_u64(1).map_err(short)? {
        let name = at.bytes_u64().map_err(short)?;
        std::str::from_utf8(name)
            .ok()
            .and_then(trace::untraceable_of)
            .ok_or_else(|| bad(format!("no untraceable is {:?}", String::from_utf8_lossy(name))))?; // alloc: error path
        keyer.untraceable(name);
    }
    match at.is_done() {
        true => Ok(Some(keyer.finish())),
        false => Err(bad("the shape has bytes after its end".to_owned())), // alloc: error path
    }
}

/// Why a result could not be kept.
#[derive(Debug)]
pub enum KeepError {
    Corrupt(Corrupt),
    /// The store has a value of the result's key with other bytes: the key misses an input
    /// the value depends on, or the run is not deterministic. Never kept over.
    Mismatch(Mismatch),
}

/// Keeps `value`, the result of `run`, which read `traced`: the result under its key and its
/// shape in the chain if the chain has it not. The tickets of what was put. [`keep_in`] of
/// the store's shared unit.
pub fn keep(
    store: &mut Store,
    run: &RunKey,
    traced: &Traced,
    value: Vec<u8>,
) -> Result<Vec<Ticket>, KeepError> {
    keep_in(store, Unit::SHARED, run, traced, value)
}

/// [`keep`] into the packs of `unit`, the crate version the run is of ([`Store::unit`]): a
/// build script's package, an expansion's calling crate, a test's crate. Its shapes go there
/// too, since only runs of that crate version read them.
pub fn keep_in(
    store: &mut Store,
    unit: Unit,
    run: &RunKey,
    traced: &Traced,
    value: Vec<u8>,
) -> Result<Vec<Ticket>, KeepError> {
    let mut tickets = Vec::with_capacity(2);
    let key = result_key(run, traced);
    tickets.push(store.put_in(unit, key, Kind::Run, value).map_err(KeepError::Mismatch)?);
    let shape = Shape::of(traced).encode();
    let mut n = 0;
    loop {
        let at = slot(run, n);
        let same = store.get(&at).map_err(KeepError::Corrupt)?.map(|it| it.bytes == shape.as_slice());
        match same {
            Some(true) => return Ok(tickets),
            Some(false) => n += 1,
            // clone: the store owns what it keeps, and a slot another took first needs the shape again
            None => match store.put_in(unit, at, Kind::Run, shape.clone()) {
                Ok(ticket) => {
                    tickets.push(ticket);
                    return Ok(tickets);
                }
                // Another kept another shape here first.
                Err(_) => n += 1,
            },
        }
    }
}

/// Where a run read something it can't see again: what [`crate::rule`] keys such a run by.
pub fn untraceable(traced: &Traced) -> bool {
    !traced.untraceable.is_empty()
}
/// The content ID of a shape, for an explanation of a rerun.
pub fn shape_id(shape: &Shape) -> ContentId {
    ContentId::of(trace::os_domain!("blaze v2 shape id 1"), &shape.encode())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn a_shape_round_trips_and_forgets_values() {
        let traced = Traced {
            files: vec![("/b".into(), Some([1; 32])), ("/a".into(), None)],
            dirs: vec![("/d".into(), [2; 32])],
            env: vec![("CC".into(), Some("cc".into())), ("X".into(), None)],
            untraceable: vec![Untraceable::Clock],
        };
        let shape = Shape::of(&traced);
        assert_eq!(shape.files, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(Shape::decode(&shape.encode()).as_ref(), Ok(&shape));
        let mut other = traced.clone();
        other.files[0].1 = Some([9; 32]);
        assert_eq!(Shape::of(&other), shape);
        let mut bytes = shape.encode();
        bytes.push(0);
        assert!(Shape::decode(&bytes).is_err());
        assert!(Shape::decode(&bytes[..10]).is_err());
    }

    #[test]
    fn a_run_that_read_the_whole_environment_has_one_name_for_it() {
        let traced = Traced {
            env: vec![
                (ALL_ENV_NAME.into(), Some("h".into())),
                ("A".into(), Some("1".into())),
            ],
            ..Traced::default()
        };
        assert_eq!(Shape::of(&traced).env, vec![OsString::from(ALL_ENV_NAME)]);
    }

    #[test]
    fn a_whole_environment_and_a_name_not_there_find_their_result() {
        use std::ffi::OsStr;

        use crate::trace::{Line, Seen, assemble};

        struct Env(Vec<(OsString, OsString)>);
        impl Inputs for Env {
            type Error = ();
            fn entry(&mut self, _: &Path) -> Result<crate::trace::Entry, ()> {
                Ok(crate::trace::Entry::Missing)
            }
            fn file(&mut self, _: &Path) -> Result<Option<[u8; 32]>, ()> {
                Ok(None)
            }
            fn dir(&mut self, _: &Path) -> Result<Option<[u8; 32]>, ()> {
                Ok(None)
            }
            fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
                self.0.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
            }
            fn env_all(&mut self) -> &[(OsString, OsString)] {
                self.0.sort();
                &self.0
            }
            fn names(&mut self, _: &Path, _: &mut trace::Names<'_>) -> Result<bool, ()> {
                Ok(false)
            }
        }
        let lines = [Line::new(Seen::Env, "", "*"), Line::new(Seen::Env, "", "UNSET"), Line::new(Seen::Env, "", "A")];
        let mut env = Env(vec![("A".into(), "1".into())]);
        let traced = assemble(&lines, None, &mut env).unwrap();
        let shape = Shape::of(&traced);
        assert_eq!(shape.values(&mut env).unwrap(), traced);
        env.0.push(("UNSET".into(), "now".into()));
        assert_ne!(shape.values(&mut env).unwrap(), traced);
        let mut added = Env(vec![("A".into(), "1".into()), ("B".into(), "2".into())]);
        assert_ne!(shape.values(&mut added).unwrap(), traced);
    }

    /// The key a check streams from a shape's bytes is the key of the [`Traced`] the shape's
    /// values are, for each way the inputs can have changed; a directory gone is no key.
    #[test]
    fn a_check_keys_a_shape_as_its_values_are_keyed() {
        use std::{collections::BTreeMap, ffi::OsStr};

        #[derive(Clone)]
        struct World {
            files: BTreeMap<PathBuf, [u8; 32]>,
            dirs: BTreeMap<PathBuf, [u8; 32]>,
            env: Vec<(OsString, OsString)>,
        }
        impl Inputs for World {
            type Error = ();
            fn entry(&mut self, _: &Path) -> Result<crate::trace::Entry, ()> {
                Ok(crate::trace::Entry::Missing)
            }
            fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, ()> {
                Ok(self.files.get(path).copied())
            }
            fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, ()> {
                Ok(self.dirs.get(path).copied())
            }
            fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
                self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
            }
            fn env_all(&mut self) -> &[(OsString, OsString)] {
                self.env.sort();
                &self.env
            }
            fn names(&mut self, _: &Path, _: &mut trace::Names<'_>) -> Result<bool, ()> {
                Ok(false)
            }
        }
        let run = RunKey { code: ContentId::of("blaze v2 test 1", b"run"), given: ContentId::default() };
        let world = World {
            files: [("/a".into(), [1; 32]), ("/b".into(), [2; 32])].into(),
            dirs: [("/d".into(), [3; 32])].into(),
            env: vec![("B".into(), "2".into()), ("A".into(), "1".into())],
        };
        for whole in [false, true] {
            let mut traced = Traced {
                files: vec![("/a".into(), Some([1; 32])), ("/b".into(), Some([2; 32])), ("/gone".into(), None)],
                dirs: vec![("/d".into(), [3; 32])],
                env: vec![("A".into(), Some("1".into())), ("UNSET".into(), None)],
                untraceable: vec![Untraceable::Clock],
            };
            if whole {
                let hash = trace::names_hash(&world.clone().env_all().to_vec());
                traced.env.push((ALL_ENV_NAME.into(), Some(hash.into())));
                traced.env.push(("B".into(), Some("2".into())));
            }
            let shape = Shape::of(&traced).encode();
            let changes: [&dyn Fn(&mut World); 5] = [
                &|_: &mut World| {},
                &|it: &mut World| _ = it.files.insert("/b".into(), [9; 32]),
                &|it: &mut World| _ = it.files.insert("/gone".into(), [9; 32]),
                &|it: &mut World| it.env.push(("UNSET".into(), "now".into())),
                &|it: &mut World| it.env.push(("C".into(), "3".into())),
            ];
            let unchanged = VALUES.traced(&traced);
            for (n, change) in changes.into_iter().enumerate() {
                let mut now = world.clone();
                change(&mut now);
                let values = Shape::decode(&shape).unwrap().values(&mut now.clone()).unwrap();
                let streamed = keyed(&shape, &run, false, &mut now.clone()).ok().unwrap();
                assert_eq!(streamed, Some((run.traced(&values), None)), "whole {whole}");
                let both = keyed(&shape, &run, true, &mut now.clone()).ok().unwrap();
                assert_eq!(both, Some((run.traced(&values), Some(VALUES.traced(&values)))), "whole {whole}");
                let alone = super::values(&shape, &mut now).ok().unwrap();
                assert_eq!(alone, Some(VALUES.traced(&values)), "whole {whole}");
                // A variable the run did not ask for is read only with the whole environment.
                let same = n == 0 || (n == 4 && !whole);
                assert_eq!(alone == Some(unchanged), same, "whole {whole}, change {n}");
            }
            assert_eq!(keyed(&shape, &run, false, &mut world.clone()).ok().unwrap(), Some((run.traced(&traced), None)));
            let mut gone = world.clone();
            gone.dirs.clear();
            assert_eq!(keyed(&shape, &run, true, &mut gone.clone()).ok().unwrap(), None);
            assert_eq!(super::values(&shape, &mut gone).ok().unwrap(), None);
        }
    }

    /// A check of a kept run's shape, whose files, directories and variables are as they were,
    /// allocates nothing: the shape is read in place, each value lent and hashed into the key.
    #[cfg(unix)]
    #[test]
    fn a_warm_check_of_a_shape_allocates_nothing() {
        use std::{collections::BTreeMap, ffi::OsStr};

        struct World {
            files: BTreeMap<PathBuf, [u8; 32]>,
            dirs: BTreeMap<PathBuf, [u8; 32]>,
            env: Vec<(OsString, OsString)>,
        }
        impl Inputs for World {
            type Error = ();
            fn entry(&mut self, _: &Path) -> Result<crate::trace::Entry, ()> {
                Ok(crate::trace::Entry::Missing)
            }
            fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, ()> {
                Ok(self.files.get(path).copied())
            }
            fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, ()> {
                Ok(self.dirs.get(path).copied())
            }
            fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
                self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
            }
            fn env_all(&mut self) -> &[(OsString, OsString)] {
                &self.env
            }
            fn names(&mut self, _: &Path, _: &mut trace::Names<'_>) -> Result<bool, ()> {
                Ok(false)
            }
        }
        let run = RunKey { code: ContentId::of("blaze v2 test 1", b"run"), given: ContentId::default() };
        let traced = Traced {
            files: vec![("/a".into(), Some([1; 32])), ("/gone".into(), None)],
            dirs: vec![("/d".into(), [3; 32])],
            env: vec![("A".into(), Some("1".into())), ("UNSET".into(), None)],
            untraceable: vec![],
        };
        let shape = Shape::of(&traced).encode();
        let mut world = World {
            files: [("/a".into(), [1; 32])].into(),
            dirs: [("/d".into(), [3; 32])].into(),
            env: vec![("A".into(), "1".into())],
        };
        let key = blaze_alloc_count::assert_at_most(0, "a kept run's shape checked", || {
            keyed(&shape, &run, false, &mut world).ok().unwrap()
        });
        assert_eq!(key, Some((run.traced(&traced), None)));
        let both = blaze_alloc_count::assert_at_most(0, "a kept run's shape checked with its values", || {
            keyed(&shape, &run, true, &mut world).ok().unwrap()
        });
        assert_eq!(both, Some((run.traced(&traced), Some(VALUES.traced(&traced)))));
    }

    #[test]
    fn slots_are_per_run_and_per_number() {
        let run = RunKey { code: ContentId::of("blaze v2 test 1", b"a"), given: ContentId::default() };
        let other = RunKey { code: ContentId::of("blaze v2 test 1", b"b"), given: ContentId::default() };
        assert_ne!(slot(&run, 0), slot(&run, 1));
        assert_ne!(slot(&run, 0), slot(&other, 0));
    }
}
