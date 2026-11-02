//! What a unit's record is filed by beside its key ([`super::compile`]): what its crate and
//! every crate it depends on read that no key can name before they are resolved, with the
//! values read. That is the process variables their `env!` and `option_env!` calls read, and
//! what their proc-macro expansions read besides the call (a file outside the source tree, a
//! listing, a variable of the shell), as the expansion server traced it.
//!
//! Those reads are known only once the crates are resolved, so they are kept as a dependency's
//! image keeps its own (`hir_def::deps::filed`): the names in the first free of [`SLOTS`]
//! slots under the record's base key, and the record under the base key, those names and the
//! values they had. A record whose closure read nothing of the kind is under the base key
//! alone. Looking one up asks the base key, then each slot with the values its names have now:
//! a variable's in the job's environment, an expansion's worked out again from its names
//! (the daemon's `proc_macro_api::kept::Keeping::reads_now`) with no macro run. So a record is used only while
//! everything it read is as it was, and a variable nothing read (a per-run `BLAZE_PROFILE`,
//! `OLDPWD`) never makes it miss.
//!
//! The names come from the database's logs: the `env!` log notes each variable an expansion
//! reads, the expansion log each proc-macro expansion's names and values, and each notes the
//! reads a value served from the store in place of resolving was filed by
//! (`hir_def::deps::filed::search`), so once a unit is compiled the logs name everything its
//! code was made from.

#![forbid(unsafe_code)]

#[cfg(test)]
pub(super) mod tests;

use base_db::{Crate, Db};
use blaze_cas::Key;
use blaze_format::cursor::{Put as _, Reader};
use blaze_ids::{ContentId, Hasher};

/// The domain of a record's slots and of a record filed by its reads.
const DOMAIN: &str = "blaze v2 cargo unit record reads 2";

/// The first byte of a slot's bytes: the version of [`Reads::encode`].
const VERSION: u8 = 2;

/// How many sets of names one base key keeps. A crate's code reads other names only where a
/// value it read picks them (`env!(concat!(env!("A"), ..))`), so one in practice.
pub(crate) const SLOTS: u32 = hir_def::deps::SLOTS;

/// What a unit's crate closure read, each part sorted, each read once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Reads {
    /// The process variables `env!` and `option_env!` read.
    names: Vec<String>,
    /// What the proc-macro expansions read, by the variables of the crate the calls are in.
    expansions: Vec<Expansions>,
}

/// What the proc-macro expansions of the crates of one set of variables read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Expansions {
    /// The crate's variables, sorted, as its expansions were given them. They are in the
    /// record's base key already (the crate's identity); the slot keeps them because a lookup
    /// has no database, and a shape's names are worked out with them (a file's path from
    /// `CARGO_MANIFEST_DIR`).
    env: Vec<(String, String)>,
    /// Each expansion's names as its server encodes them, sorted, with the hash of their
    /// values when it ran: known for reads made in this process, none for reads decoded from
    /// a slot.
    shapes: Vec<(Vec<u8>, Option<ContentId>)>,
}

impl Reads {
    /// What `krate` and every crate it depends on read, as `db`'s logs have it. For a
    /// variable, only a crate that is not the standard library's (whose `env!` reads its own
    /// variables alone), and only a name the crate's own variables lack (`env!` reads the
    /// crate's, then the process's; the crate's are in its identity). The expansions' reads are
    /// [`hir_def::deps::closure_expansion_reads`]'s. An expansion of a call the crate still has
    /// whose reads are not known, or two of one crate's that found other values for the same
    /// names, are an error naming the call or crate: no key covers what the unit was made from.
    pub(crate) fn of(db: &Db, krate: Crate) -> Result<Reads, String> {
        // alloc: once per unit stored, a crate of its closure each
        let closure: blaze_hash::HashSet<Crate, std::alloc::Global> = krate.transitive_deps(db).iter().copied().collect();
        // alloc: once per unit stored, a variable read each
        let mut names = Vec::new();
        for (by, name) in db.files().env_read() {
            if !closure.contains(&by) || by.data(db).origin.is_lang() || by.env(db).get(&name).is_some() {
                continue;
            }
            names.push(name);
        }
        names.sort_unstable();
        names.dedup();
        let read = hir_def::deps::closure_expansion_reads(db, krate)
            .map_err(|refused| format!("no key covers what a proc-macro expansion read: {refused}"))?;
        // alloc: once per unit stored, a crate of its closure whose expansions read something each
        let mut expansions: Vec<(Crate, Expansions)> = Vec::with_capacity(read.len());
        for (by, shapes) in read {
            // alloc: a crate's variables, once per unit stored, a crate as above each
            let mut env: Vec<(String, String)> = by.env(db).iter().map(|(name, value)| (name.to_owned(), value.to_owned())).collect();
            env.sort_unstable();
            let shapes = shapes.into_iter().map(|(shape, values)| (shape.into_vec(), Some(values))).collect();
            expansions.push((by, Expansions { env, shapes }));
        }
        expansions.sort_unstable_by(|a, b| a.1.env.cmp(&b.1.env));
        // Crates of one set of variables work a shape's values out alike: one entry for them.
        expansions.dedup_by(|later, kept| {
            let same = later.1.env == kept.1.env;
            if same {
                kept.1.shapes.append(&mut later.1.shapes);
            }
            same
        });
        for (by, it) in &mut expansions {
            it.shapes.sort_unstable();
            it.shapes.dedup();
            if it.shapes.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                let name = hir_def::deps::crate_name(db, *by);
                return Err(format!(
                    "two proc-macro expansions in crate {name} read the same names and found other values: \
                     an input changed while it was resolved"
                ));
            }
        }
        Ok(Reads { names, expansions: expansions.into_iter().map(|(_, it)| it).collect() })
    }

    #[cfg(test)]
    pub(crate) fn named(names: &[&str]) -> Reads {
        let mut names: Vec<String> = names.iter().map(|it| (*it).to_owned()).collect();
        names.sort_unstable();
        names.dedup();
        Reads { names, expansions: Vec::new() }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.names.is_empty() && self.expansions.is_empty()
    }

    /// The names, as a slot keeps them: [`VERSION`], the variables' count and each, then the
    /// count of sets of crate variables and, for each, its variables' count, each name and
    /// value, its expansions' count and each one's names. Every string and shape framed by its
    /// length, every count and length a `u32` LE.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let names: usize = self.names.iter().map(|it| it.len() + 4).sum();
        let expansions: usize = (self.expansions.iter())
            .map(|it| 8 + it.env.iter().map(|(n, v)| n.len() + v.len() + 8).sum::<usize>() + it.shapes.iter().map(|(s, _)| s.len() + 4).sum::<usize>())
            .sum();
        // alloc: one slot's bytes, once per unit stored that read something
        let mut out = Vec::with_capacity(names + expansions + 9);
        out.put_u8(VERSION);
        out.put_u32(self.names.len() as u32);
        for name in &self.names {
            out.put_bytes_u32(name.as_bytes());
        }
        out.put_u32(self.expansions.len() as u32);
        for it in &self.expansions {
            out.put_u32(it.env.len() as u32);
            for (name, value) in &it.env {
                out.put_bytes_u32(name.as_bytes());
                out.put_bytes_u32(value.as_bytes());
            }
            out.put_u32(it.shapes.len() as u32);
            for (shape, _) in &it.shapes {
                out.put_bytes_u32(shape);
            }
        }
        out
    }

    /// What [`Reads::encode`] wrote, checked: its version, its lengths against the bytes, and
    /// each part sorted with no repeats. The bytes came from a store, which checked only their
    /// hash.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Reads, &'static str> {
        let short = |_| "a record's reads shorter than their lengths";
        let text = |_| "a record's read variable that is cut short or not UTF-8";
        let mut read = Reader::new(bytes);
        if read.u8().map_err(short)? != VERSION {
            return Err("a record's reads of another version");
        }
        let count = read.count_u32(4).map_err(short)?;
        // alloc: a slot read, once per slot asked
        let mut names = Vec::with_capacity(count);
        for _ in 0..count {
            names.push(read.str_u32().map_err(text)?.to_owned()); // alloc: as above
        }
        if !names.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err("a record's read variables out of order or repeated");
        }
        let count = read.count_u32(8).map_err(short)?;
        // alloc: as above
        let mut expansions = Vec::with_capacity(count);
        for _ in 0..count {
            let pairs = read.count_u32(8).map_err(short)?;
            let mut env = Vec::with_capacity(pairs); // alloc: as above
            for _ in 0..pairs {
                let name = read.str_u32().map_err(text)?.to_owned(); // alloc: as above
                env.push((name, read.str_u32().map_err(text)?.to_owned())); // alloc: as above
            }
            if !env.windows(2).all(|pair| pair[0].0 < pair[1].0) {
                return Err("a record's crate variables out of order or repeated");
            }
            let shapes = read.count_u32(4).map_err(short)?;
            let mut it = Expansions { env, shapes: Vec::with_capacity(shapes) }; // alloc: as above
            for _ in 0..shapes {
                it.shapes.push((read.bytes_u32().map_err(short)?.to_vec(), None)); // alloc: as above
            }
            if !it.shapes.windows(2).all(|pair| pair[0].0 < pair[1].0) {
                return Err("a record's expansions out of order or repeated");
            }
            expansions.push(it);
        }
        read.finish().map_err(|_| "bytes after a record's reads")?;
        if !expansions.windows(2).all(|pair| pair[0].env < pair[1].env) {
            return Err("a record's sets of crate variables out of order or repeated");
        }
        Ok(Reads { names, expansions })
    }

    /// The hash of the values the reads had as they were made: the variables' in `env` (the
    /// job's environment, `name=value` pairs), each expansion's as it reported them. An error
    /// for reads decoded from a slot, which know no values.
    fn values_then(&self, env: &[(String, String)]) -> Result<ContentId, String> {
        self.hash(env, |_, _, then| then.ok_or_else(|| "a record's reads read from the store, filed as if made here".to_owned()))
    }

    /// The hash of the values the reads have now: the variables' in `env`, each expansion's
    /// names worked out again by `now` with its crate's variables.
    fn values_now(
        &self,
        env: &[(String, String)],
        mut now: impl FnMut(&base_db::Env, &[u8]) -> Result<ContentId, String>,
    ) -> Result<ContentId, String> {
        // clone: a crate's variables, once per slot asked that holds expansions
        let given: Vec<base_db::Env> = self.expansions.iter().map(|it| it.env.iter().cloned().collect()).collect();
        self.hash(env, |at, shape, _| now(&given[at], shape))
    }

    /// The hash of the values `env` gives the variables, an unset one apart from every value,
    /// then of each expansion's that `expansion` gives, with the index of its set of variables.
    fn hash(
        &self,
        env: &[(String, String)],
        mut expansion: impl FnMut(usize, &[u8], Option<ContentId>) -> Result<ContentId, String>,
    ) -> Result<ContentId, String> {
        let mut hasher = Hasher::new(DOMAIN);
        hasher.part(b"values");
        for name in &self.names {
            hasher.opt_part(env.iter().find(|(it, _)| it == name).map(|(_, value)| value.as_bytes()));
        }
        for (at, it) in self.expansions.iter().enumerate() {
            for (shape, then) in &it.shapes {
                hasher.id(&expansion(at, shape, *then)?);
            }
        }
        Ok(hasher.finish())
    }
}

/// Slot `n` of base key `base`: where the `n`th set of names its records read is kept.
pub(crate) fn slot(base: Key, n: u32) -> Key {
    let mut hasher = Hasher::new(DOMAIN);
    hasher.part(b"slot").id(&base.0).update(&n.to_le_bytes());
    Key(hasher.finish())
}

/// Where the record of base key `base` is for the reads `record` (a slot's bytes) with the
/// values `values`.
pub(crate) fn filed(base: Key, record: &[u8], values: ContentId) -> Key {
    let mut hasher = Hasher::new(DOMAIN);
    hasher.part(b"record").id(&base.0).part(record).id(&values);
    Key(hasher.finish())
}

/// The first key `found` says holds a record: `base`, then each slot's of `base` that `get`
/// gives, its names with the values they have now: a variable's that `env` gives it, an
/// expansion's that `now` works out with its crate's variables (the daemon's
/// `proc_macro_api::kept::Keeping::reads_now`).
/// None when none does; an error naming the slot when a slot does not decode or `now` fails.
pub(crate) fn find<'g>(
    base: Key,
    env: &[(String, String)],
    mut now: impl FnMut(&base_db::Env, &[u8]) -> Result<ContentId, String>,
    mut get: impl FnMut(&Key) -> Option<&'g [u8]>,
    mut found: impl FnMut(&Key) -> bool,
) -> Result<Option<Found>, String> {
    if found(&base) {
        return Ok(Some(Found { key: base, slot: None }));
    }
    for n in 0..SLOTS {
        let at = slot(base, n);
        let Some(record) = get(&at) else { break };
        let reads = Reads::decode(record).map_err(|why| format!("slot {n} of record key {base:?}: {why}"))?;
        let values = reads.values_now(env, &mut now).map_err(|why| format!("slot {n} of record key {base:?}: {why}"))?;
        let key = filed(base, record, values);
        if found(&key) {
            return Ok(Some(Found { key, slot: Some(at) }));
        }
    }
    Ok(None)
}

/// Where [`find`] found a record: its key, and the slot whose names it was filed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) key: Key,
    pub(crate) slot: Option<Key>,
}

/// Where a record made with the reads `reads` goes, the values being those they had as they
/// were made (a variable's that `env` gives it): the key, and the slot to keep the names in
/// when no slot of `base` holds them yet. None when every slot holds other names, so the
/// record is not kept. An error for reads read from the store, which know no values.
pub(crate) fn file<'g>(
    base: Key,
    reads: &Reads,
    env: &[(String, String)],
    mut get: impl FnMut(&Key) -> Option<&'g [u8]>,
) -> Result<Option<Filing>, String> {
    if reads.is_empty() {
        return Ok(Some(Filing { key: base, slot: None }));
    }
    let record = reads.encode();
    let key = filed(base, &record, reads.values_then(env)?);
    for n in 0..SLOTS {
        let at = slot(base, n);
        match get(&at) {
            Some(kept) if kept == record.as_slice() => return Ok(Some(Filing { key, slot: None })),
            Some(_) => continue,
            None => return Ok(Some(Filing { key, slot: Some((at, record)) })),
        }
    }
    Ok(None)
}

/// Where [`file`] puts a record: its key, and the slot and bytes of its names when they are
/// kept now.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Filing {
    pub(crate) key: Key,
    pub(crate) slot: Option<(Key, Vec<u8>)>,
}
