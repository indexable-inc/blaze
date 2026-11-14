//! blaze-memo's `Resident` against blaze-keyed's reference `Resident`, through
//! `blaze_keyed::memo::Memo`, for any sequence of keeps, gets, arena drops and request ends.
//!
//! The refinement: after every operation the two give the same answer to every get (the same
//! bytes, or none) and hold the same `resident_bytes`. blaze-keyed's own test holds its
//! reference to `RecordsOnly` under the real driver (`resident_refines_records_only`), so this
//! one closes the chain: blaze-memo's table gives the driver the outcomes and the store of a
//! records-only run.

use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{Body, Certificate, Key, Kind, Surface},
    memo::{Memo, Resident as Reference},
    proof::{self, Snapshot},
    record::{self, Traced},
};
use blaze_memo::Resident;
use blaze_sim::Tape;

/// Small enough that keeps cross it and drop every arena.
const CAP: u64 = 512;
const ARENAS: u32 = 3;
const KEYS: u8 = 6;

#[derive(Clone, Debug)]
enum Op {
    Keep { kind: u8, arena: u32, key: u8, len: usize, fill: u8 },
    Drop(u32),
    End,
}

fn op(tape: &Tape) -> Op {
    let arena = u32::from(tape.int_in(0u8..=ARENAS as u8 - 1, "arena"));
    match tape.int_in(0u8..=7, "op") {
        0 => Op::Drop(arena),
        1 => Op::End,
        _ => Op::Keep {
            kind: tape.int_in(0u8..=2, "kind"),
            arena,
            key: tape.int_in(0u8..=KEYS - 1, "key"),
            len: usize::from(tape.int_in(0u8..=200, "len")),
            fill: tape.int::<u8>("fill"),
        },
    }
}

fn key<K: Kind>(key: u8) -> Key<K> {
    Key::from_id(ContentId::of("blaze-memo resident test key", &[key]))
}

/// Keeps a real record of kind `K` under its key, with the proof a driver holds for it: the
/// one key check (`proof::hit`) in the asking request's snapshot.
fn keep_as<K: Kind<Root = Traced>>(memo: &mut impl Memo, snap: Snapshot<'_>, arena: u32, k: u8, payload: &[u8]) {
    let key = key::<K>(k);
    let bytes = record::write_traced(&key, &[], &[], payload);
    let hit = proof::hit(snap, key, &bytes).expect("a record written under its key opens under it");
    memo.keep::<K>(arena, &hit.holds(), &bytes);
}

fn apply(memo: &mut impl Memo, snap: Snapshot<'_>, op: &Op) {
    match *op {
        Op::Keep { kind, arena, key: k, len, fill } => {
            // A record's bytes vary with its key, so a mixed-up key shows.
            let payload: Vec<u8> = (0..len).map(|at| fill ^ k ^ at as u8).collect();
            match kind {
                0 => keep_as::<Body>(memo, snap, arena, k, &payload),
                1 => keep_as::<Surface>(memo, snap, arena, k, &payload),
                _ => keep_as::<Certificate>(memo, snap, arena, k, &payload),
            }
        }
        Op::Drop(arena) => memo.drop_arena(arena),
        Op::End => memo.end_request(),
    }
}

/// Every answer `memo` gives, in a fixed order.
fn answers(memo: &impl Memo) -> Vec<Option<Vec<u8>>> {
    let mut out = Vec::new();
    for arena in 0..ARENAS {
        for k in 0..KEYS {
            out.push(memo.get::<Body>(arena, &key::<Body>(k)).map(<[u8]>::to_vec));
            out.push(memo.get::<Surface>(arena, &key::<Surface>(k)).map(<[u8]>::to_vec));
            out.push(memo.get::<Certificate>(arena, &key::<Certificate>(k)).map(<[u8]>::to_vec));
        }
    }
    out
}

fn aligned(memo: &impl Memo) -> bool {
    (0..ARENAS).all(|arena| {
        (0..KEYS).all(|k| {
            memo.get::<Body>(arena, &key::<Body>(k)).is_none_or(|it| it.as_ptr().addr() % 16 == 0)
        })
    })
}

fn refines<M: Memo>(ops: &[Op], mut mine: M) -> Result<(), String> {
    let mut reference = Reference::new(CAP);
    for (step, op) in ops.iter().enumerate() {
        // Each op is asked in its own request: a proof never outlives the snapshot it was
        // made in.
        proof::scope(|snap| {
            apply(&mut mine, snap, op);
            apply(&mut reference, snap, op);
        });
        if answers(&mine) != answers(&reference) {
            return Err(format!("step {step} ({op:?}): the answers differ"));
        }
        if mine.resident_bytes() != reference.resident_bytes() {
            return Err(format!(
                "step {step} ({op:?}): {} resident bytes, the reference {}",
                mine.resident_bytes(),
                reference.resident_bytes()
            ));
        }
        if !aligned(&mine) {
            return Err(format!("step {step}: a record is not 16-aligned"));
        }
    }
    Ok(())
}

#[test]
fn resident_refines_the_reference() {
    blaze_sim::check(blaze_sim::Config::cases(300), |tape| {
        let ops: Vec<Op> = tape.vec(0..=60, "ops", op);
        refines(&ops, Resident::new(CAP))
    });
}

/// A memo that forgets to drop an arena: the planted bug the oracle must refuse.
struct KeepsDropped(Resident);

impl Memo for KeepsDropped {
    const NAME: &'static str = "planted: keeps dropped arenas";

    fn get<K: Kind>(&self, arena: u32, key: &Key<K>) -> Option<&[u8]> {
        self.0.get(arena, key)
    }

    fn keep<K: Kind>(&mut self, arena: u32, holds: &proof::Holds<'_, K>, record: &[u8]) {
        self.0.keep(arena, holds, record);
    }

    fn drop_arena(&mut self, _arena: u32) {}

    fn end_request(&mut self) {
        self.0.end_request();
    }

    fn resident_bytes(&self) -> u64 {
        self.0.resident_bytes()
    }
}

#[test]
fn the_oracle_catches_a_stale_entry() {
    let ops = [
        Op::Keep { kind: 0, arena: 1, key: 2, len: 8, fill: 7 },
        Op::Drop(1),
    ];
    refines(&ops, Resident::new(CAP)).unwrap();
    assert!(refines(&ops, KeepsDropped(Resident::new(CAP))).is_err(), "the oracle missed a stale entry");
}
