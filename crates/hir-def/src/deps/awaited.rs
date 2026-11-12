//! Crates in the graph whose records were not asked for yet: *awaited* (restore.md, F2).
//!
//! A crate is in one of three states to the maintainers (name resolution, `100`; impls, `200`):
//! *served* (read from its records), *maintained* (resolved from source, with a state), or
//! *awaited*: added to the graph by a driver that will ask the store for its records before
//! anything reads it. An awaited crate has no job, no state and no published tables, so the
//! write that adds a thousand crates resolves none of them; the driver's serve
//! ([`super::serve_asked`]) then takes each crate it asked about out of the set in the same
//! write that gives the database the records found, and only the crates with none get their
//! first run from source.
//!
//! The invariant: a crate is *blocked* while it or any crate it depends on, directly or
//! through others, is awaited ([`blocked`]); no maintainer runs, prepares or publishes a
//! blocked crate, and a reader of one is stopped with `Unmaintained` naming it as awaited. A
//! crate leaves the set only by [`release`], which notes it as a crate added
//! ([`base_db::Written::crates_added`]): to the maintainers that is its first appearance, as
//! if the write had made it. It never enters the set again.
//!
//! Only a database whose driver promised to serve has the set ([`await_added`], made by
//! `blaze_lower::lower_awaiting`): every front end of the daemon. A database without it (a
//! test's, a tool's that holds no store) awaits nothing, and `blaze_lower`'s serve refuses it.

use std::sync::Arc;

use base_db::{Crate, Db};
use blaze_hash::{HashMap, HashSet};
use std::alloc::Global;

/// The crates awaited, set by [`await_crates`] and [`release`].
#[blaze_query::input(singleton, debug)]
pub struct Awaited {
    #[returns(ref)]
    pub crates: Arc<HashSet<Crate, Global>>,
}

/// Has `db` await the crates its driver adds from now on ([`await_crates`]): the driver's
/// promise that it asks the store for every crate it adds ([`super::serve_asked`]) before
/// anything reads one. Called once, on a database just made; again it changes nothing.
pub fn await_added(db: &mut Db) {
    if Awaited::try_get(&*db).is_none() {
        // alloc: the one input value, once a database
        let none = Arc::new(HashSet::new_in(Global));
        _ = Awaited::new(db, blaze_query::db::Durability::HIGH, none);
    }
}

/// Whether `db` awaits the crates added to it ([`await_added`]).
pub fn awaits_added(db: &Db) -> bool {
    Awaited::try_get(db).is_some()
}

/// Whether any crate of `db` is awaited: a load, for the passes that have nothing to skip.
pub fn any_awaited(db: &Db) -> bool {
    Awaited::try_get(db).is_some_and(|it| !it.crates(db).is_empty())
}

/// Whether `krate` itself is awaited (not whether it is [`blocked`]).
pub fn is_awaited(db: &Db, krate: Crate) -> bool {
    Awaited::try_get(db).is_some_and(|it| it.crates(db).contains(&krate))
}

fn set(db: &mut Db, crates: HashSet<Crate, Global>) {
    use blaze_query::db::{Durability, Setter};
    let Some(input) = Awaited::try_get(&*db) else {
        panic!("awaited crates are set in a database that awaits none (`deps::await_added`)")
    };
    // alloc: the one input value, once per write that adds or releases crates
    input.set_crates(db).with_durability(Durability::HIGH).to(Arc::new(crates));
}

/// Awaits `added`, crates the write this is called in just put into the graph: the write's
/// pass gives them no job. Nothing is written for an empty `added`, and nothing in a database
/// that awaits none ([`await_added`]). A crate given twice is awaited once.
pub fn await_crates(db: &mut Db, added: &[Crate]) {
    if added.is_empty() || !awaits_added(&*db) {
        return;
    }
    let Some(input) = Awaited::try_get(&*db) else { return };
    // clone: the set, once per write that adds crates
    let mut crates = (**input.crates(&*db)).clone();
    let before = crates.len();
    crates.extend(added.iter().copied());
    if crates.len() != before {
        set(db, crates);
    }
}

/// Takes `asked` and every crate they depend on, directly or through others, out of the
/// awaited set, and notes each one taken as a crate added, so the write this is called in
/// gives it its first run, or reads it from the records the same write set. Gives the crates
/// taken, sorted. Nothing is written when none of them was awaited.
///
/// Dependencies are released with their dependents: a crate is never left blocked under a
/// crate released.
pub fn release(db: &mut Db, asked: &[Crate]) -> Vec<Crate> {
    if !any_awaited(&*db) {
        // alloc: none, an empty list
        return Vec::new();
    }
    let Some(input) = Awaited::try_get(&*db) else { return Vec::new() }; // alloc: none
    // clone: the set, once per serve that releases crates
    let mut crates = (**input.crates(&*db)).clone();
    // alloc: once per serve while crates are awaited: the closure walked, and what it took
    let mut seen: HashSet<Crate, Global> = HashSet::new_in(Global);
    let mut stack: Vec<Crate> = asked.to_vec(); // alloc: as above
    let mut released = Vec::new(); // alloc: as above
    while let Some(krate) = stack.pop() {
        if !seen.insert(krate) {
            continue;
        }
        if crates.remove(&krate) {
            released.push(krate);
        }
        stack.extend(krate.data(&*db).dependencies.iter().map(|it| it.crate_id));
    }
    if released.is_empty() {
        return released;
    }
    released.sort_unstable();
    tracing::info!(
        target: "blaze",
        released = released.len() as u64,
        awaited = crates.len() as u64,
        "awaited crates released: their records were asked for"
    );
    set(db, crates);
    db.note_crates_added(&released);
    released
}

/// The awaited crate that blocks `krate`: itself, or the first found below it; `None` when
/// `krate` is not blocked. For the error a reader of a blocked crate is stopped with.
pub fn blocker(db: &Db, krate: Crate) -> Option<Crate> {
    let input = Awaited::try_get(db)?;
    let awaited = input.crates(db);
    if awaited.is_empty() {
        return None;
    }
    // alloc: an error's path, once
    let mut seen: HashSet<Crate, Global> = HashSet::new_in(Global);
    let mut stack = vec![krate]; // alloc: as above
    while let Some(at) = stack.pop() {
        if !seen.insert(at) {
            continue;
        }
        if awaited.contains(&at) {
            return Some(at);
        }
        stack.extend(at.data(db).dependencies.iter().map(|it| it.crate_id));
    }
    None
}

/// The crates of the graph that are blocked: awaited, or depending on an awaited crate
/// through any path. Reads inputs alone (the set, the graph's list, each crate's
/// dependencies), so a maintainer's apply may call it. Empty, with no walk and no allocation,
/// when nothing is awaited.
pub fn blocked(db: &Db) -> HashSet<Crate, Global> {
    fn of(
        db: &Db,
        krate: Crate,
        awaited: &HashSet<Crate, Global>,
        memo: &mut HashMap<Crate, bool, Global>,
    ) -> bool {
        if let Some(&known) = memo.get(&krate) {
            return known;
        }
        let mut blocked = awaited.contains(&krate);
        for dep in &krate.data(db).dependencies {
            // Every dependency is asked, so each is in the memo once.
            blocked |= of(db, dep.crate_id, awaited, memo);
        }
        memo.insert(krate, blocked);
        blocked
    }
    let mut out = HashSet::new_in(Global);
    let Some(input) = Awaited::try_get(db) else { return out };
    let awaited = input.crates(db);
    if awaited.is_empty() {
        return out;
    }
    // alloc: once per pass while crates are awaited, a flag a crate
    let mut memo = HashMap::new_in(Global);
    for &krate in base_db::all_crates(db).iter() {
        if of(db, krate, awaited, &mut memo) {
            out.insert(krate);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use base_db::{Crate, Db};
    use hir_expand::fixture;

    use super::*;

    const CHAIN: &str = r#"
//- /main.rs crate:main deps:lib0 new_source_root:local
fn main() {}
//- /lib0.rs crate:lib0 deps:lib1 new_source_root:library
pub fn f0() {}
//- /lib1.rs crate:lib1 new_source_root:library
pub fn f1() {}
//- /other.rs crate:other new_source_root:library
pub fn o() {}
"#;

    fn named(db: &Db, name: &str) -> Crate {
        *base_db::all_crates(db)
            .iter()
            .find(|it| it.named(db).to_string() == name)
            .unwrap_or_else(|| panic!("no crate `{name}`"))
    }

    fn names(db: &Db, crates: impl IntoIterator<Item = Crate>) -> Vec<String> {
        let mut names: Vec<String> = crates.into_iter().map(|it| it.named(db).to_string()).collect();
        names.sort();
        names
    }

    /// The invariant of the set: a crate is blocked exactly while it or a crate below it is
    /// awaited; a release of a crate takes it and everything below it, so nothing released is
    /// left blocked; and a release of crates not awaited writes nothing (the view stands).
    /// The set's logic alone, on crates a fixture resolved already: that an awaited crate is
    /// not resolved is counted where crates are added awaiting (blaze-lower's
    /// `a_crate_added_to_an_awaiting_front_end_is_not_resolved_until_it_is_asked_about`).
    #[test]
    fn a_crate_is_blocked_while_it_or_one_below_it_is_awaited() {
        let mut db = fixture::db(CHAIN).into_db();
        let (main, lib0, lib1, other) =
            (named(&db, "main"), named(&db, "lib0"), named(&db, "lib1"), named(&db, "other"));
        // A database that awaits nothing: nothing is awaited by asking.
        await_crates(&mut db, &[lib1]);
        assert!(!awaits_added(&db) && !any_awaited(&db) && blocked(&db).is_empty());

        await_added(&mut db);
        assert!(awaits_added(&db) && !any_awaited(&db));
        await_crates(&mut db, &[lib1, lib1]);
        assert!(is_awaited(&db, lib1) && !is_awaited(&db, lib0));
        assert_eq!(names(&db, blocked(&db)), ["lib0", "lib1", "main"], "all above `lib1`");
        assert_eq!(blocker(&db, main), Some(lib1));
        assert_eq!(blocker(&db, other), None);

        // A release of a crate beside them takes nothing and writes nothing.
        let view = db.nonce_and_revision().1;
        assert_eq!(release(&mut db, &[other]), Vec::new());
        assert_eq!(db.nonce_and_revision().1, view, "nothing released: no write");

        // A release of the top takes everything below it, in one write.
        await_crates(&mut db, &[main]);
        assert_eq!(names(&db, release(&mut db, &[main])), ["lib1", "main"]);
        assert!(!any_awaited(&db) && blocked(&db).is_empty());
        assert_ne!(db.nonce_and_revision().1, view, "the release is a write");
        let view = db.nonce_and_revision().1;
        assert_eq!(release(&mut db, &[main, other]), Vec::new());
        assert_eq!(db.nonce_and_revision().1, view, "nothing awaited: no write");
    }
}
