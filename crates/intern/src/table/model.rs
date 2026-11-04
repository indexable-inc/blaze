//! Under the model checker (`RUSTFLAGS="--cfg blaze_check" blaze test -p intern --release
//! model_`): a table's lock-free paths against a sequential set of values. Interning races on
//! the overlay's index and on taking an ID (the image's freed IDs first, then the next); a find
//! on another thread sees nothing or the whole value, and the ID it finds reads back its bytes
//! through the ID column, whose slot is set before the index publishes the value. Each value
//! gets one ID, distinct values distinct IDs, and a restored table hands out each freed ID once.
//!
//! Each value has its own history, checked against a sequential set of that one value: the
//! table is linearizable value by value, not across values. Two values may sit in different
//! shards, each published with a release store and read with acquire loads
//! (docs/v2/plan/design/concurrency.md), so a thread that interns 1 and then misses 2 while
//! another interns 2 and then misses 1 is a run the memory model allows (store buffering) and
//! no one order of the four explains. No caller can tell: a thread that has not synchronized
//! with an intern has no "before" to miss it in, and one that has finds it. One history over
//! both values asked for that order and failed with "not linearizable" on every tree that had
//! these models.

use std::sync::Arc;

use blaze_check::{
    linear::{History, Spec},
    thread,
};
use blaze_format::SchemaHash;
use blaze_ids::ContentId;
use blaze_image::{Builder, Image, SectionId};

use super::*;

const SCHEMA: SchemaHash = SchemaHash(ContentId([9; 32]));
const ID: SectionId = SectionId::SYMBOLS;

/// One compiled-in value, so made IDs start at 2.
fn shape() -> Shape {
    Shape {
        statics: 1,
        static_bytes: |_| b"s",
        key: crate::order_key,
        align: 1,
        size: 1,
        text: true,
        overlay: Overlay::Secret,
    }
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Intern(u8),
    Find(u8),
}

fn bytes(value: u8) -> [u8; 1] {
    [b'a' + value]
}

/// The sequential table, seen from one value: whether it is in it.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
struct Has(bool);

impl Spec for Has {
    type Op = Op;
    /// `Intern`: always `true`. `Find`: whether it found.
    type Ret = bool;

    fn apply(&mut self, op: &Op) -> bool {
        match *op {
            Op::Intern(_) => {
                self.0 = true;
                true
            }
            Op::Find(_) => self.0,
        }
    }
}

/// A history for each value the models use.
type Histories = [History<Has>; 3];

/// Runs `op` as one operation of its value's history: the ID it interned or found, whose bytes
/// read back from this thread.
fn run(histories: &Histories, table: &Table, op: Op) -> Option<u32> {
    let mut id = None;
    let (Op::Intern(value) | Op::Find(value)) = op;
    histories[value as usize].record(op, |&op| match op {
        Op::Intern(value) => {
            id = Some(table.intern(&bytes(value)));
            true
        }
        Op::Find(value) => {
            id = table.find(&bytes(value));
            id.is_some()
        }
    });
    if let Some(id) = id {
        assert_eq!(table.get(id), Some(&bytes(value)[..]), "an ID found reads its whole value");
    }
    id
}

/// Two threads intern 1 and 2 in opposite orders on a fresh table while finding the other's:
/// each value one ID, two IDs for two values, and every ID read back from either thread.
#[test]
fn model_racing_interns_get_one_id_each() {
    blaze_check::model(|| {
        let table = Arc::new(Table::new(shape()));
        let histories = Arc::new(Histories::default());
        let other = {
            let (table, histories) = (table.clone(), histories.clone());
            thread::spawn(move || {
                let one = run(&histories, &table, Op::Intern(1));
                let two = run(&histories, &table, Op::Find(2));
                let again = run(&histories, &table, Op::Intern(2));
                (one, two, again)
            })
        };
        let two = run(&histories, &table, Op::Intern(2));
        let one = run(&histories, &table, Op::Find(1));
        let (one_there, two_found, two_again) = other.join().unwrap();
        histories.iter().for_each(|history| history.check(Has::default()));
        assert_eq!(two, two_again, "2 interned once");
        if one.is_some() {
            assert_eq!(one, one_there, "1 found under the ID it was given");
        }
        if two_found.is_some() {
            assert_eq!(two_found, two, "2 found under the ID it was given");
        }
        assert_ne!(one_there, two, "two values, two IDs");
        let mut ids = [one_there.unwrap(), two.unwrap()];
        ids.sort_unstable();
        assert_eq!(ids, [2, 3], "made IDs are the next ones, each taken once");
        assert_eq!(table.end(), 4);
    });
}

/// An image whose table freed one ID: two values made from two threads take that ID and the
/// next, one each, while each finds the other's and reads it through the column.
#[test]
fn model_freed_ids_are_taken_once() {
    let image: &'static Image<Vec<u8>> = {
        let table = Table::new(shape());
        let kept = table.intern(b"k");
        assert_eq!(table.intern(b"f"), kept + 1);
        let mut live = Live::new();
        live.mark_id(kept);
        let mut builder = Builder::new();
        table.write(&live, &mut builder.section(ID, SCHEMA));
        // alloc: the test's image, once, read by every execution
        Box::leak(Box::new(Image::open(builder.finish().unwrap()).unwrap()))
    };
    blaze_check::model(|| {
        let section = image.section(ID, SCHEMA).unwrap();
        let table = Arc::new(Table::restore(shape(), image, &section, &serial).unwrap());
        assert_eq!((table.get(2), table.get(3), table.end()), (Some(&b"k"[..]), None, 4));
        let histories = Arc::new(Histories::default());
        let other = {
            let (table, histories) = (table.clone(), histories.clone());
            thread::spawn(move || {
                [Op::Intern(0), Op::Find(1)].map(|op| run(&histories, &table, op))
            })
        };
        let one = run(&histories, &table, Op::Intern(1));
        let zero = run(&histories, &table, Op::Find(0));
        let [zero_there, one_found] = other.join().unwrap();
        histories.iter().for_each(|history| history.check(Has::default()));
        if zero.is_some() {
            assert_eq!(zero, zero_there);
        }
        if one_found.is_some() {
            assert_eq!(one_found, one);
        }
        let mut ids = [zero_there.unwrap(), one.unwrap()];
        ids.sort_unstable();
        assert_eq!(ids, [3, 4], "the freed ID once, then the next");
        assert_eq!(table.find(b"k"), Some(2), "the kept value keeps its ID");
        assert_eq!(table.end(), 5);
    });
}
