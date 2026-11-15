use blaze_ids::{ContentId, Hasher};

use super::{Double, Len, Num, key_id, runs, setup, view};
use crate::{Cancelled, Cx, Inputs, Query, QueryKind, inputs::probe::SKIPS_VERIFIED};

/// Reads two files and one listing, one file through two queries and one twice: `a` through
/// [`Double`] (which asks [`Len`]), `b` through [`Len`] and itself, and the listing of `d`.
struct Top;

impl Query for Top {
    const KIND: QueryKind = QueryKind(0x2f40);
    const NAME: &'static str = "top";
    const PRODUCER: ContentId = ContentId([0x40; 32]);
    type Key = ();
    type Value = Num;
    fn input(_: &()) -> ContentId {
        key_id("top")
    }
    fn execute(cx: &mut Cx<'_>, _: &()) -> Result<Num, Cancelled> {
        super::ran("top");
        let a = cx.get::<Double>(&"a".to_owned())?;
        let b = cx.get::<Len>(&"b".to_owned())?;
        let again = cx.file("b")?.map_or(0, |file| file.bytes.len() as u64);
        let names = cx.dir("d")?.map_or(0, |names| names.len() as u64);
        Ok(Num(a + b + again + names))
    }
}

fn file_id(bytes: &str) -> ContentId {
    ContentId::of(blaze_vfs::FILE_DOMAIN, bytes.as_bytes())
}

/// The ID of a listing of files (no directories) named `names`, in order, as the database
/// hashes what the tree lends.
fn listing_id(names: &[&str]) -> ContentId {
    let mut hasher = Hasher::new(crate::NAMES_DOMAIN);
    for name in names {
        hasher.part(name.as_bytes()).update(&[0]);
    }
    hasher.finish()
}

/// `f` with the planted bug of [`SKIPS_VERIFIED`] on this thread.
fn planted<R>(f: impl FnOnce() -> R) -> R {
    SKIPS_VERIFIED.with(|it| it.set(true));
    let out = f();
    SKIPS_VERIFIED.with(|it| it.set(false));
    out
}

/// The invariant: `inputs_of` a key in a view is exactly the files and listings of that view
/// read under the memo the key holds there, transitively (through dependencies that ran and
/// through ones proved unchanged without running, by early cutoff), each once, with the ID the
/// view has for it; and `None` where the key holds no memo. Control: the walk that leaves out a
/// dependency proved unchanged without running ([`SKIPS_VERIFIED`]) loses `a`, which only
/// [`Double`] reaches, and is caught.
#[test]
fn the_inputs_of_a_result_are_the_files_and_listings_under_it_with_their_ids() {
    let (mut db, tree) = setup();
    db.register::<Top>();
    let files = |a: &'static str| [("a", a), ("b", "1"), ("d/x", ""), ("d/y", "")];
    let expect = |a: &str| Inputs {
        files: vec![(Box::from("a"), file_id(a)), (Box::from("b"), file_id("1"))],
        listings: vec![(Box::from("d"), listing_id(&["x", "y"]))],
    };

    let one = view(&db, &tree, &files("abc"));
    assert_eq!(db.pin().inputs_of::<Top>(one, &()), None, "the view never resolved it");
    assert_eq!(db.pin().get::<Top>(one, &()), Ok(6 + 1 + 1 + 2));
    assert_eq!(db.pin().inputs_of::<Top>(one, &()), Some(expect("abc")));
    assert_eq!(
        planted(|| db.pin().inputs_of::<Top>(one, &())),
        Some(expect("abc")),
        "where everything ran, the plant changes nothing: the control is about cutoff alone"
    );

    // `a` changes, its length does not: `len(a)` runs again, `double(a)` and `top` are proved
    // unchanged by cutoff, and `top`'s memo holds in the new view.
    let two = view(&db, &tree, &files("xyz"));
    assert_eq!(db.pin().get::<Top>(two, &()), Ok(6 + 1 + 1 + 2));
    assert_eq!(
        (runs("len"), runs("double"), runs("top")),
        (3, 1, 1),
        "the premise: only len(a) ran again"
    );
    assert_eq!(db.pin().inputs_of::<Top>(two, &()), Some(expect("xyz")), "a with its new ID");

    let blind = planted(|| db.pin().inputs_of::<Top>(two, &()));
    assert_ne!(blind, Some(expect("xyz")), "the planted bug is caught");
    assert_eq!(
        blind.map(|it| it.files),
        Some(vec![(Box::from("b"), file_id("1"))]),
        "it lost exactly a, read only under double(a), proved unchanged without running"
    );
}

/// The invariant: the plan database's `take_counts` reports, a registered query each, the
/// memos it made (first runs and runs after a change apart) and the ones it proved unchanged
/// without running, since the last take, and nothing for a hit. Control: the same runs taken
/// twice report nothing the second time.
#[test]
fn the_database_counts_what_each_query_ran_and_proved() {
    let (mut db, tree) = setup();
    db.register::<Top>();
    let files = |a: &'static str| [("a", a), ("b", "1"), ("d/x", ""), ("d/y", "")];
    let one = view(&db, &tree, &files("abc"));
    assert_eq!(db.pin().get::<Top>(one, &()), Ok(10));
    let two = view(&db, &tree, &files("xyz"));
    assert_eq!(db.pin().get::<Top>(two, &()), Ok(10));
    assert_eq!(db.pin().get::<Top>(two, &()), Ok(10), "a hit, counted nowhere");

    let taken = || {
        let mut rows = Vec::new();
        db.take_counts(|name, counts| {
            rows.push((name, counts.executed, counts.verified, counts.ran_new, counts.ran_changed))
        });
        rows
    };
    let mut rows = taken();
    rows.sort_unstable();
    // len: a and b first, a again after its edit; b proved unchanged in `two`.
    assert_eq!(
        rows,
        vec![("double", 1, 1, 1, 0), ("len", 3, 1, 2, 1), ("top", 1, 1, 1, 0)],
        "each query's runs and proofs"
    );
    assert_eq!(taken(), Vec::new(), "a take reports what was gained since the last one");
}
