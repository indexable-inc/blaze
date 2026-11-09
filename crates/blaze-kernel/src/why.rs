//! Why a lookup missed (lane why-miss, docs/v2/plan/design/why.md). Nothing here runs on a
//! hit's path, and nothing here writes: a reason is worked out only once a lookup has found
//! nothing and someone wants to know why, and what it needs from before is handed in.
//!
//! - **Of a step the kernel computes** ([`Missed`], [`crate::Kernel::why`]): the caller hands in
//!   the key of the entry it saw kept last under the identity (the daemon's ledger remembers
//!   it from the [`Miss::kept`] of the miss before). Nothing kept is [`Missed::New`]; otherwise
//!   the first read of that entry whose hash changed, with the hash it saw then and the one it
//!   sees now. The store holds no record of what was kept for this, so explaining can never
//!   fail a keep or grow the store.
//! - **Of a step keyed by its whole identity, or a unit outside the kernel** ([`Miss`]): the
//!   parts of its key by name, each with its hash. The daemon keeps each subject's parts from
//!   the last time it was computed and names the parts that changed (blaze-cargo's
//!   `serve::why`). A key that does not expose its parts says so ([`Missed::NotExplained`]),
//!   never a guess.
//! - **Per build**: a producer gives its misses back with what it built (codegen's
//!   `Built::misses`, a unit's `Compiled::misses`), never into a queue of the process, so two
//!   builds that overlap never see each other's.

use blaze_cas::Key;
use blaze_ids::ContentId;

use crate::{Entry, Get, Identity, Seen, World, shape_slot};

/// Why a lookup of a step the kernel computes found no entry.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Missed {
    /// Nothing is kept under its identity: no entry kept last was handed in, and no shape of
    /// reads is kept under it.
    New,
    /// The entry kept last under its identity saw `read` as `was`, and it is `now` now: the
    /// first of the entry's reads, in its order (kind, then key), whose hash changed. `changed`
    /// of the entry's `reads` changed in all.
    Read {
        /// The read as its world names it ([`World::describe`]): a layout, a constant or a
        /// callee by path.
        read: String,
        /// The item the read is of, when its world says ([`World::names`]): what a chain of
        /// reasons follows.
        names: Option<String>,
        was: Seen,
        now: Seen,
        changed: u32,
        reads: u32,
    },
    /// Shapes of reads are kept under its identity, but no entry kept last was handed in: it
    /// was kept before the daemon's ledger began, or by another daemon.
    Unrecorded,
    /// The entry handed in as kept last is not in the store: the build that computed it did
    /// not keep it (it failed or was cancelled first), or the store is another one.
    Collected,
    /// Every read of the entry kept last hashes as it did, yet lookup did not find it: no
    /// shape of reads under its identity names it (a keep that did not finish).
    Unshaped,
    /// It is computed every time, for the reason given (a build script run outside the
    /// sandbox).
    Always(String),
    /// Its key does not expose its parts yet: the kind of key, as "not explained yet: <kind>".
    NotExplained(String),
    /// The reason could not be worked out: what failed. The miss itself is unaffected.
    Failed(String),
}

/// One part of a key, by name: what a miss of a key with parts is told apart by.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Part {
    /// What it is, as a person reads it: `toolchain`, `settings`, `env CC`. One name a part:
    /// a key with two parts of one name is refused ([`Miss::of_parts`]).
    pub name: String,
    /// Its hash: none when the key has no such part this time (no `-C profile-use` profile).
    pub hash: Option<ContentId>,
    /// The item it is of, when it is another's (a dependency's unit): what a chain follows.
    pub names: Option<String>,
}

impl Part {
    pub fn new(name: impl Into<String>, hash: Option<ContentId>) -> Part {
        Part { name: name.into(), hash, names: None }
    }
}

/// A miss, as a build gives it back for the daemon to explain and keep with its history.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Miss {
    /// What was computed again: a step's name (`codegen dev template`), `unit`, `build script
    /// run`.
    pub step: &'static str,
    /// What it is across edits, telling apart instances that exist side by side (a function
    /// by its identity, origin and settings within its unit; a unit by package, target, mode,
    /// platform and profile): what the daemon compares it with the last time by. Two
    /// different misses of one step under one subject in one build are refused, never one
    /// kept over the other (rule 21).
    pub subject: ContentId,
    /// What of, by path, for a person only: a function, a unit (`app v0.1.0 (lib)`), a
    /// package's build script.
    pub item: String,
    pub missed: Missed,
    /// Its key, as the store files it.
    pub key: ContentId,
    /// The key of the entry the kernel kept for it now, for a step the kernel computes: what
    /// the daemon remembers and hands to [`crate::Kernel::why`] at its next miss.
    pub kept: Option<ContentId>,
    /// The parts of its key, sorted by name, each once: empty where the key exposes none.
    pub parts: Vec<Part>,
    /// What the value is filed under past `key` that is not explained yet (a unit's process
    /// variables, a build script's traced reads): what a miss whose key is as it was is down to.
    pub beyond: Option<&'static str>,
}

impl Miss {
    /// A miss of a key made of `parts`, which are sorted here. A part given twice alike is
    /// kept once; two different parts of one name make the miss [`Missed::Failed`], naming it,
    /// as no change of either could be told from the other's.
    pub fn of_parts(
        step: &'static str,
        subject: ContentId,
        item: String,
        key: ContentId,
        mut parts: Vec<Part>,
    ) -> Miss {
        parts.sort();
        parts.dedup();
        let twice = parts.windows(2).find(|pair| pair[0].name == pair[1].name).map(|pair| {
            // alloc: an error
            format!("its key has two different parts named `{}`, so a change of one cannot be told from the other", pair[0].name)
        });
        let missed = match twice {
            Some(why) => Missed::Failed(why),
            None => Missed::New,
        };
        Miss { step, subject, item, missed, key, kept: None, parts, beyond: None }
    }
}

/// Why `identity`'s lookup found nothing in `store`, now that it has: the first read of the
/// entry kept last (`last`, as the caller remembers it) that hashes differently in `world` now.
/// Called on a miss only, by someone who wants the reason. A store or world that fails here is
/// [`Missed::Failed`], never the miss's error.
pub(crate) fn explain<W: World + ?Sized, G: Get + ?Sized>(
    store: &G,
    world: &W,
    identity: &Identity,
    last: Option<Key>,
) -> Missed {
    match explained(store, world, identity, last) {
        Ok(missed) => missed,
        Err(why) => Missed::Failed(why),
    }
}

fn explained<W: World + ?Sized, G: Get + ?Sized>(
    store: &G,
    world: &W,
    identity: &Identity,
    last: Option<Key>,
) -> Result<Missed, String> {
    // alloc: an error
    let at = |key: Key, why: String| format!("the store at {:x}: {why}", key.0);
    let Some(key) = last else {
        let first = shape_slot(&identity.id(), 0);
        return Ok(match store.has(&first).map_err(|why| at(first, why))? {
            true => Missed::Unrecorded,
            false => Missed::New,
        });
    };
    let Some(bytes) = store.get(&key).map_err(|why| at(key, why))? else {
        return Ok(Missed::Collected);
    };
    let entry = Entry::read(bytes, key, identity).map_err(|why| at(key, why))?;
    let (mut first, mut changed) = (None, 0u32);
    for (read, was) in entry.reads() {
        // alloc: an error
        let now =
            world.hash(read).map_err(|error| format!("{}: {}", world.describe(read), error.why))?;
        if now == was {
            continue;
        }
        changed += 1;
        if first.is_none() {
            first = Some((read, was, now));
        }
    }
    let reads = u32::try_from(entry.reads().len()).unwrap_or(u32::MAX);
    Ok(match first {
        Some((read, was, now)) => Missed::Read {
            read: world.describe(read),
            names: world.names(read),
            was,
            now,
            changed,
            reads,
        },
        None => Missed::Unshaped,
    })
}
