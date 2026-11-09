//! Verified before shared (docs/v2/plan/design/store.md, "Portable keys"): a run's value is
//! never given to another checkout on the word of the checkout that ran it. Each value is kept
//! three ways in the store's key space, all through [`index::keep`], so all of it is the
//! store's and is restored with it:
//!
//! - **Here**: under the run's key, its trace ported with a mark for every root
//!   ([`Marked::local`]): found again only where every root is where it was.
//! - **A copy** for another checkout to check, when nothing ties the run to its roots' strings
//!   ([`Marked::shared`]): under [`copies`] of the run and its unmarked trace, one entry per
//!   checkout that kept it (its [`Origin`], root by root, is the one file it read). A copy is a
//!   small record naming the key the checkout kept the value under here, never the value
//!   again. A lookup never finds a copy: the run key's chain does not hold them.
//! - **Verified**: once a second checkout ran the same run, read the same things (its unmarked
//!   trace makes the same key) and gave the same ported value as every other checkout's copy,
//!   the value goes under the run's key with the unmarked trace and a mark for each root that
//!   was the same in that pair ([`Origin::differing`]), one put per pair's set of roots: only
//!   a root seen to differ with the value the same is left unmarked, and only together with
//!   the others that differed in the same pair, so a value that holds a root two checkouts
//!   share (the tools, a shared target directory) is never given to a third where it is
//!   elsewhere. It is kept with the key of its unmarked result before it
//!   ([`index::verified_value`]). When one differs, the run is recorded as depending on where
//!   its roots are ([`index::depends`]): nothing of it is verified from then on, a value
//!   verified before is never found again ([`index::find`] asks), and each checkout keeps its
//!   own. A checkout that reruns a verified value and gets another (verify or check mode)
//!   records the same ([`retraction`]).
//!
//! The comparison is made as a checkout keeps what it ran ([`keeps`]): a run that missed its
//! key runs anyway, so checking a copy costs no run, only a few reads of the store.

use std::path::PathBuf;

use blaze_cas::{Corrupt, Key, Store, Ticket, Unit};
use blaze_core::Traced;
use blaze_ids::{ContentId, Hasher};

use crate::{
    RunKey,
    index::{self, KeepError, Shape},
    roots::{Marked, Role, RoleSet, Roots},
};

/// What a [`index::depends`] record holds.
pub const DEPENDS: &[u8] = b"blaze v2 the run depends on where its roots are 1";

/// What a copy's value starts with, then the key its checkout kept the value under here.
const COPY: &[u8] = b"blaze v2 a checkout's copy 2\0";

/// The most checkouts' copies of one run and trace that are read: two that agree verify it
/// and one that differs ends it, so a chain is this long only when that many checkouts kept
/// it before any saw another's. Past it, the run is kept here alone.
pub const MOST_COPIES: u32 = 64;

/// The first name of a copy's one path, then its [`Origin`]: each role's tag and the hex of
/// its root's hash.
const ORIGIN_PATH: &str = "\0origin/";

/// The run the copies of the value of `run` that read `shared` (an unmarked, ported trace) are
/// kept under, one per checkout ([`origin_trace`]).
pub fn copies(run: &RunKey, shared: &Traced) -> RunKey {
    let result = index::result_key(run, shared);
    RunKey {
        code: run.code,
        given: Hasher::new("blaze v2 unverified copies 2").id(&run.given).id(&result.0).finish(),
    }
}

/// Which checkout a set of roots is, root by root: each role's root hashed as its mark is
/// ([`Roots::mark`]), in role order, so two checkouts are compared one root at a time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Origin([Option<[u8; 32]>; Role::ALL.len()]);

impl Origin {
    pub fn of(roots: &Roots<'_>) -> Origin {
        Origin(Role::ALL.map(|role| roots.get(role).map(super::root_hash)))
    }

    /// The roles whose roots differ between the two; an error when one has a role the other
    /// has not, which one key never is (the layout of the roots is in the key).
    pub fn differing(&self, other: &Origin) -> Result<RoleSet, String> {
        let mut out = RoleSet::default();
        for (at, role) in Role::ALL.into_iter().enumerate() {
            match (self.0[at], other.0[at]) {
                (None, None) => {}
                (Some(ours), Some(theirs)) => {
                    if ours != theirs {
                        out.insert(role);
                    }
                }
                // alloc: error path
                _ => {
                    return Err(format!(
                        "a copy whose roots are not this run's: one has a {} root and one not",
                        role.name()
                    ));
                }
            }
        }
        Ok(out)
    }

    fn path(&self) -> PathBuf {
        use std::fmt::Write;
        let mut out = String::from(ORIGIN_PATH); // alloc: once per copy kept or read, its one path
        for (role, hash) in Role::ALL.into_iter().zip(self.0) {
            if let Some(hash) = hash {
                out.push(char::from(role.tag()));
                let _ = write!(out, "{:x}", ContentId(hash));
            }
        }
        PathBuf::from(out)
    }

    /// The origin a copy's shape, `bytes`, names.
    fn of_shape(bytes: &[u8]) -> Result<Origin, String> {
        let shape = Shape::decode(bytes)?;
        let [path] = shape.files.as_slice() else {
            return Err(format!(
                "a copy's shape reads {} files, not its origin alone",
                shape.files.len()
            )); // alloc: error path
        };
        let rest = path.to_str().and_then(|it| it.strip_prefix(ORIGIN_PATH));
        let mut rest = rest
            .ok_or_else(|| format!("a copy's shape reads {}, not an origin", path.display()))?
            .as_bytes(); // alloc: error path
        let mut out = [None; Role::ALL.len()];
        let mut last = None;
        while let [tag, tail @ ..] = rest {
            let role = Role::of(*tag)
                .ok_or_else(|| format!("a copy's origin names no role {:?}", char::from(*tag)))?; // alloc: error path
            let at = Role::ALL.iter().position(|it| *it == role).unwrap_or(0);
            if last.is_some_and(|last| at <= last) {
                return Err(format!("a copy's origin names the {} root out of order", role.name())); // alloc: error path
            }
            last = Some(at);
            let (hex, tail) = tail
                .split_at_checked(64)
                .ok_or_else(|| format!("a copy's origin is short in its {} root", role.name()))?; // alloc: error path
            let mut hash = [0u8; 32];
            for (i, byte) in hash.iter_mut().enumerate() {
                let pair = std::str::from_utf8(&hex[2 * i..2 * i + 2])
                    .map_err(|error| format!("a copy's origin: {error}"))?; // alloc: error path
                *byte = u8::from_str_radix(pair, 16)
                    .map_err(|error| format!("a copy's origin {pair:?}: {error}"))?; // alloc: error path
            }
            out[at] = Some(hash);
            rest = tail;
        }
        Ok(Origin(out))
    }
}

/// What a copy of `origin` is kept as having read: its origin, by name and hash, so each
/// checkout's copy has a shape and a key of its own.
fn origin_trace(origin: &Origin) -> Traced {
    let path = origin.path();
    let hash = ContentId::of("blaze v2 copy origin 2", path.as_os_str().as_encoded_bytes());
    // alloc: once per copy kept or read, its one path
    Traced { files: vec![(path, Some(hash.0))], ..Traced::default() }
}

/// A copy's value: the key its checkout kept the value under here.
fn copy_of(here: &Key) -> Vec<u8> {
    [COPY, &here.0.0[..]].concat() // alloc: once per copy kept, its record
}

/// The key a copy's value, `bytes`, names.
fn copied(bytes: &[u8]) -> Result<Key, String> {
    let key = bytes.strip_prefix(COPY).and_then(|it| <[u8; 32]>::try_from(it).ok());
    key.map(|it| Key(ContentId(it))).ok_or_else(|| "not a copy's record".to_owned()) // alloc: error path
}

/// The puts that record that `run`'s value under `unmarked` (the key of its unmarked result,
/// [`crate::index::Found::Hit`]'s `verified`) depends on where its roots are: a checkout that
/// reran a value verified across checkouts and got another. From then on no checkout is given
/// it ([`index::find`] asks), and none is verified again.
pub fn retraction(run: &RunKey, unmarked: &Key) -> Put {
    // alloc: once, a record
    Put {
        run: index::depends(run, unmarked),
        traced: Traced::default(),
        value: DEPENDS.to_vec(),
        durable: true,
    }
}

/// One value to keep: what [`keeps`] says to put, through [`index::keep`].
#[derive(Debug, PartialEq, Eq)]
pub struct Put {
    pub run: RunKey,
    pub traced: Traced,
    pub value: Vec<u8>,
    /// A [`retraction`]'s record: it must be durable before the run that put it is reported
    /// done, or a crash forgets that the value depends on where its roots are and it is given
    /// to every checkout again. Every caller that keeps puts waits for these ([`Kept::retracted`],
    /// the expander's `Keep::Durable`).
    pub durable: bool,
}

/// What [`keeps`] made of a run.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Something ties it to its roots' strings ([`Marked::shared`] is none), or it has no
    /// roots: kept here alone.
    Here,
    /// The first checkout to keep it: kept here, and a copy for another to check.
    Copied,
    /// Another checkout's copy gave the same: kept for every checkout.
    Verified,
    /// It depends on where its roots are, as another checkout's copy showed (`what` differs)
    /// or the store recorded before: kept here alone.
    Depends { what: String },
}

/// What to put of `value` (ported), the result of `run`, kept as `marked` says
/// ([`Roots::kept`]), in the checkout of `roots`, the store's entries as `get` has them now;
/// `differs` names how another checkout's copy of the value differs from this one, if it does
/// (the same comparison [`crate::run::Mode::Verify`] makes). A [`Verdict::Depends`] that
/// another's copy showed puts the [`index::depends`] record first.
pub fn keeps<'s>(
    get: impl Fn(&Key) -> Result<Option<&'s [u8]>, Corrupt>,
    roots: &Roots<'_>,
    run: &RunKey,
    marked: Marked,
    value: Vec<u8>,
    differs: impl Fn(&[u8], &[u8]) -> Result<Option<String>, String>,
) -> Result<(Vec<Put>, Verdict), KeepError> {
    let Marked { local, shared, .. } = marked;
    let here_key = index::result_key(run, &local);
    let here = |value: Vec<u8>| Put { run: *run, traced: local, value, durable: false };
    let Some(shared) = shared else {
        return Ok((vec![here(value)], Verdict::Here)); // alloc: once per run kept, its one put
    };
    let result = index::result_key(run, &shared);
    if get(&index::depends_key(run, &result)).map_err(KeepError::Corrupt)?.is_some() {
        // alloc: once per run kept, its one put
        return Ok((
            vec![here(value)],
            Verdict::Depends { what: "the store recorded it before".to_owned() },
        ));
    }
    let copies = copies(run, &shared);
    let ours = Origin::of(roots);
    // The roots seen to differ in each pair compared whose values were the same, each set
    // once: a pair is evidence only for the roots that differed in it, so each set is a
    // verified put of its own, never their union (A and B differing in the target directory,
    // A and C in the tools, say nothing of a checkout where both differ).
    let mut pairs: Vec<RoleSet> = Vec::new(); // alloc: once per run kept, at most MOST_COPIES sets
    let mut others = 0;
    let mut differed = None;
    let mut n = 0;
    while n < MOST_COPIES {
        let at = index::slot(&copies, n);
        n += 1;
        let Some(shape) = get(&at).map_err(KeepError::Corrupt)? else { break };
        let theirs = Origin::of_shape(shape).map_err(|why| KeepError::NotACopy { key: at, why })?;
        let roles = ours.differing(&theirs).map_err(|why| KeepError::NotACopy { key: at, why })?;
        if roles.is_empty() {
            continue;
        }
        let key = index::result_key(&copies, &origin_trace(&theirs));
        let Some(copy) = get(&key).map_err(KeepError::Corrupt)? else { continue };
        let pointed = copied(copy).map_err(|why| KeepError::NotACopy { key, why })?;
        // Their own entry, which a copy names: gone only with a store that lost it.
        let Some(kept) = get(&pointed).map_err(KeepError::Corrupt)? else { continue };
        others += 1;
        if let Some(what) =
            differs(kept, &value).map_err(|why| KeepError::NotACopy { key: pointed, why })?
        {
            differed = Some(what);
            break;
        }
        if !pairs.contains(&roles) {
            pairs.push(roles);
        }
    }
    if differed.is_none()
        && n == MOST_COPIES
        && get(&index::slot(&copies, n)).map_err(KeepError::Corrupt)?.is_some()
    {
        // alloc: error path, a verdict said
        differed = Some(format!("{MOST_COPIES} checkouts kept copies and none was verified"));
    }
    Ok(match differed {
        // alloc: once per run kept, its puts
        Some(what) => (vec![retraction(run, &result), here(value)], Verdict::Depends { what }),
        None if others > 0 => {
            let verified = index::verified_value(&result, &value);
            let puts = pairs
                .iter()
                .map(|differed| {
                    let mut traced = shared.clone(); // clone: once per pair verified, its put's trace
                    let mut same = RoleSet::default();
                    for role in roots.all().iter().filter(|it| !differed.contains(*it)) {
                        same.insert(role);
                    }
                    roots.mark(&mut traced, same);
                    // clone: once per pair verified, its put's value
                    Put { run: *run, traced, value: verified.clone(), durable: false }
                })
                .collect(); // alloc: once per run kept, a put per pair verified
            (puts, Verdict::Verified)
        }
        None => {
            let copy = Put {
                run: copies,
                traced: origin_trace(&ours),
                value: copy_of(&here_key),
                durable: false,
            };
            (vec![here(value), copy], Verdict::Copied) // alloc: as above
        }
    })
}

/// What [`keep`] kept.
#[derive(Debug)]
pub struct Kept {
    /// The tickets of every put.
    pub tickets: Vec<Ticket>,
    pub verdict: Verdict,
    /// The ticket of the [`index::depends`] record, when one was put: what must be durable
    /// before the run is reported done, or a crash forgets that the value depends on its roots.
    pub retracted: Option<Ticket>,
}

/// Why [`keep`] kept not all it was to, with the tickets of the puts that landed before it
/// failed: each is readable at once, so the caller waits for them as for any put of its own.
#[derive(Debug)]
pub struct NotKept {
    pub error: KeepError,
    pub tickets: Vec<Ticket>,
    /// The ticket of an [`index::depends`] record among them, when its put landed: the
    /// caller flushes it at once, as a retraction kept whole is.
    pub retracted: Option<Ticket>,
}

impl From<KeepError> for NotKept {
    fn from(error: KeepError) -> NotKept {
        // alloc: empty, allocates nothing
        NotKept { error, tickets: Vec::new(), retracted: None }
    }
}

/// [`keeps`] on the store's own thread, and each put kept ([`index::keep_into`]): a put that
/// fails part way still hands back the tickets of what landed, in [`NotKept`].
pub fn keep(
    store: &mut Store,
    roots: &Roots<'_>,
    run: &RunKey,
    marked: Marked,
    value: Vec<u8>,
    differs: impl Fn(&[u8], &[u8]) -> Result<Option<String>, String>,
) -> Result<Kept, NotKept> {
    let held: &Store = store;
    let (puts, verdict) =
        keeps(|key| Ok(held.get(key)?.map(|it| it.bytes)), roots, run, marked, value, differs)?;
    let mut tickets = Vec::new(); // alloc: once per run kept, its tickets
    let mut retracted = None;
    for put in puts {
        let from = tickets.len();
        let kept =
            index::keep_into(store, Unit::SHARED, &put.run, &put.traced, &put.value, &mut tickets);
        // The record's own ticket, the first its put made. [`keeps`] gives at most one
        // durable put, so the `max` folds at most one ticket: no tickets of two segments are
        // compared (a record that dedupes gets the ticket of another segment's put). Every
        // ticket is also in `tickets`, which the caller waits on whole.
        if put.durable {
            retracted = retracted.max(tickets.get(from).copied());
        }
        if let Err(error) = kept {
            return Err(NotKept { error, tickets, retracted });
        }
    }
    Ok(Kept { tickets, verdict, retracted })
}
