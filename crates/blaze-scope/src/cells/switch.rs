//! The switch (feature `cells`): the cell core solves a crate, driving its expansions through
//! the same hook the old fill uses, and writes every module's scope ([`Cells::scope`]) in place
//! of the old fill's `done`. What follows (the root's published exports, the records) reads the
//! scopes as before.
//!
//! A module's in-memory read list (`Resolved::reads`) is left empty: no record holds it (the
//! records are written from the scopes; the crate's read list is the questions asked of its
//! dependencies, `crate::asked`), and the old core's incremental re-solve (`again.rs`) does not
//! run on these scopes.

use blaze_hash::HashMap;
use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc};

use super::{BuiltDeps, CellError, Cells, EnumNames, FragId};
use crate::Def;
use crate::facts::ScopeFacts;
use crate::resolve::Built;
use crate::scope::{ReadList, Resolved};

/// The most rounds of made modules (blocks and expansions' modules found after a run) one
/// crate's solve takes: a crate past it is refused by name, never cut short.
pub const MOST_ROUNDS: u32 = 64;

/// Why the cell core wrote no scopes for a crate, naming the thing.
#[derive(Clone, Debug)]
pub enum Unwritten {
    Load(CellError),
    Run(CellError),
    /// A module with `count` imports or calls the core holds as outside what it follows, or
    /// left undecided: its scope would not be the answer (rule 3: refused, never guessed).
    Unknown { module: ItemKey, count: u32 },
    /// Made modules kept appearing past [`MOST_ROUNDS`] rounds.
    Rounds,
}

impl core::fmt::Display for Unwritten {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Unwritten::Load(e) => write!(f, "the cell core's load: {e}"),
            Unwritten::Run(e) => write!(f, "the cell core's run: {e}"),
            Unwritten::Unknown { module, count } => {
                write!(f, "module {module:?}: {count} imports or macro calls the cell core does not follow or left undecided")
            }
            Unwritten::Rounds => write!(f, "made modules kept appearing past {MOST_ROUNDS} rounds of the cell core"),
        }
    }
}

/// Solves the crate under `root` with the cell core and writes each of its modules' scopes.
/// `expand(call, def)` makes a call's expansion, which `facts.expansion` then hands back;
/// `made` lists every module made so far (expansions' modules, blocks); `places` says where
/// a block's body is written.
#[expect(clippy::too_many_arguments, reason = "the solve's inputs, each named; one caller")]
pub(crate) fn solve_and_write<'a, F: ScopeFacts<Name = &'a str>>(
    facts: &F,
    built: &Built<'_, 'a>,
    enums: &dyn EnumNames<&'a str>,
    places: &dyn Fn(ItemKey) -> Option<(ItemKey, u32)>,
    root: ItemKey,
    expand: &dyn Fn(ItemKey, Def),
    made: &dyn Fn(&mut AVec<'a, ItemKey>),
    alloc: Alloc<'a>,
) -> Result<HashMap<ItemKey, Resolved<'a, &'a str>, Alloc<'a>>, Unwritten> {
    let deps = BuiltDeps { built, alloc, asks: true, through: core::cell::Cell::new(None) };
    let mut c = Cells::load_with(facts, root, Some(&deps), alloc).map_err(Unwritten::Load)?;
    c.enums = Some(enums);
    c.run(facts, expand, &[]).map_err(Unwritten::Run)?;
    // The modules no `mod` line names (a block's module) and those made after the walk: solved
    // after the rest, round by round, until a run makes none.
    let mut rounds = 0u32;
    loop {
        let mut extra: AVec<'a, ItemKey> = AVec::new_in(alloc);
        made(&mut extra);
        extra.retain(|k| !c.by_key.contains_key(k));
        if extra.is_empty() {
            break;
        }
        rounds += 1;
        if rounds > MOST_ROUNDS {
            return Err(Unwritten::Rounds);
        }
        extra.sort_unstable();
        extra.dedup();
        c.load_more_at(facts, &extra, places).map_err(Unwritten::Load)?;
        c.run(facts, expand, &[]).map_err(Unwritten::Run)?;
    }
    let ix = c.write_index(alloc);
    let mut keys: AVec<'a, ItemKey> = AVec::with_capacity_in(c.modules.len(), alloc);
    keys.extend(c.modules.iter().filter(|m| !m.foreign && !m.unread && m.text != FragId::MAX).map(|m| m.key));
    keys.sort_unstable();
    let mut done: HashMap<ItemKey, Resolved<'a, &'a str>, Alloc<'a>> = HashMap::new_in(alloc);
    for &k in keys.iter() {
        let Some((scope, unknown)) = c.scope(&ix, k, alloc) else { continue };
        if unknown > 0 {
            return Err(Unwritten::Unknown { module: k, count: unknown });
        }
        done.insert(k, Resolved { scope, reads: ReadList { entries: AVec::new_in(alloc) } });
    }
    Ok(done)
}
