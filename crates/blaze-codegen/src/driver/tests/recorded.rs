//! The walk over describe records ([`Build::recording`], docs/v2/plan/design/store.md,
//! "Describe records") against the walk that reads none: another database of the same sources
//! builds the same program and describes only what has no record, and a key that leaves an
//! input out is caught by the same oracle.
//!
//! The front end here is [`Program`] with a [`Shelf`]: a function's record key is made of
//! everything its describe reads, and its record stands for the describe under that key. A
//! database of its own is [`Program::copy`], where nothing was ever described.

use std::sync::{Arc, atomic::Ordering};

use blaze_cas::Key;

use super::{kept::difference, *};
use crate::driver::records;

pub(in crate::driver) const KEY_DOMAIN: &str = "blaze v2 test describe record key 1";

/// The records of a test's describes, and the store that keeps them.
pub(in crate::driver) struct Shelf {
    /// Each record's describe, by the record's bytes: what stands in for a record's layout,
    /// which is a front end's own.
    described: Lock<HashMap<Vec<u8>, Described<u32, u32>>>,
    stored: Lock<BTreeMap<Key, Vec<u8>>>,
    /// The functions with no content key: the program's own code, which an edit reaches.
    own: Vec<u32>,
    /// The planted bug of the control: a function's key leaves its body out, so an edit of
    /// the body finds the record of the body before it.
    leaves_body_out: bool,
}

impl Shelf {
    fn new(own: &[u32], leaves_body_out: bool) -> Arc<Shelf> {
        Arc::new(Shelf {
            described: Lock::new("test.codegen.driver.shelf_described", HashMap::default()),
            stored: Lock::new("test.codegen.driver.shelf_stored", BTreeMap::new()),
            own: own.to_vec(),
            leaves_body_out,
        })
    }

    /// The record key of function `id` of `program`: everything [`Program::describe`] reads
    /// of it, by content. `None` for the program's own code.
    pub(in crate::driver) fn key(&self, program: &Program, id: u32, _: &Origin) -> Option<Key> {
        if self.own.contains(&id) {
            return None;
        }
        let function = program.functions.get(&id)?;
        let mut key = Hasher::new(KEY_DOMAIN);
        key.update(&id.to_le_bytes());
        if !self.leaves_body_out {
            key.update(&[function.body]);
        }
        let numbers = |key: &mut Hasher, numbers: &[u32]| {
            key.update(&(numbers.len() as u64).to_le_bytes());
            for it in numbers {
                key.update(&it.to_le_bytes());
            }
        };
        numbers(&mut key, &function.calls);
        numbers(&mut key, &function.statics);
        numbers(&mut key, &function.built_in);
        function.origin.hash_into(&mut key);
        let rest = format!(
            "{:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?}",
            function.symbol,
            function.linkage,
            function.outside,
            function.code_of,
            function.owner,
            function.identity_of,
            function.anchors,
            (function.foreign, function.over_editable),
            program.devirtualized(id),
        );
        key.part(rest.as_bytes());
        Some(Key(key.finish()))
    }

    /// The record of `described`, kept under `key`: the key's own bytes, which name the
    /// describe here.
    pub(in crate::driver) fn write(&self, key: Key, described: &Described<u32, u32>) -> Vec<u8> {
        let bytes = key.0.0.to_vec();
        self.described.lock().insert(bytes.clone(), described.clone());
        bytes
    }

    pub(in crate::driver) fn read(&self, bytes: &[u8]) -> Result<Described<u32, u32>, String> {
        (self.described.lock().get(bytes).cloned())
            .ok_or_else(|| format!("{} bytes that are no record of this shelf", bytes.len()))
    }

    /// Puts the records among `puts` in the store, as a build's caller does; how many.
    fn keep(&self, puts: &[Put]) -> usize {
        let described = self.described.lock();
        let mut stored = self.stored.lock();
        let mut kept = 0;
        for put in puts {
            if put.kind == records::KIND && described.contains_key(&put.bytes) {
                stored.insert(put.key, put.bytes.clone());
                kept += 1;
            }
        }
        kept
    }
}

impl Records for Shelf {
    fn read(&self, key: &Key, read: &mut dyn FnMut(&[u8])) -> Result<bool, String> {
        let bytes = self.stored.lock().get(key).cloned();
        match bytes {
            Some(bytes) => {
                read(&bytes);
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

/// `program` (five functions, `main` its own code) over `shelf`.
fn over(shelf: &Arc<Shelf>, of: &Program) -> Program {
    Program { shelf: Some(shelf.clone()), ..of.copy() }
}

/// The build of `program` from `main`, its walk asking `shelf` for describes, or no store.
fn built(program: &Program, shelf: Option<&Shelf>) -> Result<Built, Vec<CodegenError>> {
    let policy = policy(OptLevel::O0, Mode::Output);
    let (machine, memo, store) = (Machine::default(), Memo::new(), Store::default());
    let build = Build::new(&machine, program, &policy, &memo);
    let mut build = match shelf {
        Some(shelf) => build.recording(shelf),
        None => build,
    };
    build.run(&[(1, member())], &store)
}

fn counts(built: &Result<Built, Vec<CodegenError>>) -> (usize, usize, usize) {
    let walk = built.as_ref().expect("the build is not refused").walk;
    (walk.described, walk.found, walk.reached)
}

/// The invariant: a build whose walk reads describe records is the build whose walk reads
/// none, and it describes only the functions that have no record: here `main`, the program's
/// own, whatever the number of the rest.
#[test]
fn another_database_finds_its_describes_in_the_store() {
    let shelf = Shelf::new(&[1], false);
    let first = over(&shelf, &program());
    let made = built(&first, Some(&shelf));
    assert_eq!(counts(&made), (5, 0, 5), "nothing is in the store yet");
    assert_eq!(shelf.keep(&made.as_ref().unwrap().puts), 4, "a record a function but `main`");

    // Another database of the same sources: nothing in it was ever described.
    let second = over(&shelf, &first);
    let found = built(&second, Some(&shelf));
    assert_eq!(counts(&found), (1, 4, 5), "only `main` is described");
    assert_eq!(second.engine.describes.load(Ordering::Relaxed), 1);
    assert_eq!(difference(&found, &built(&first.copy(), None)), None);
    assert_eq!(shelf.keep(&found.as_ref().unwrap().puts), 0, "a found describe is not put again");

    // The cause removed: the same database with no store to ask describes every function.
    let third = over(&shelf, &first);
    assert_eq!(counts(&built(&third, None)), (5, 0, 5));
    assert_eq!(third.engine.describes.load(Ordering::Relaxed), 5);
}

/// An edit of one function's body gives that function another key: it is described, the
/// rest are found, and the build is the fresh one.
#[test]
fn an_edited_function_misses_its_record_and_the_rest_are_found() {
    let shelf = Shelf::new(&[1], false);
    let first = over(&shelf, &program());
    shelf.keep(&built(&first, Some(&shelf)).unwrap().puts);

    let mut edited = over(&shelf, &first);
    edited.functions.get_mut(&4).unwrap().body = 0x45;
    let found = built(&edited, Some(&shelf));
    assert_eq!(counts(&found), (2, 3, 5), "`main` and the edited function are described");
    assert_eq!(difference(&found, &built(&edited.copy(), None)), None);
}

/// The control: with a key that leaves the body out, the same edit finds the record of the
/// body before it, and the oracle says the build is another than the fresh one.
#[test]
fn a_key_that_leaves_an_input_out_is_caught() {
    let shelf = Shelf::new(&[1], true);
    let first = over(&shelf, &program());
    shelf.keep(&built(&first, Some(&shelf)).unwrap().puts);

    let mut edited = over(&shelf, &first);
    edited.functions.get_mut(&4).unwrap().body = 0x45;
    let found = built(&edited, Some(&shelf));
    assert_eq!(counts(&found), (1, 4, 5), "the planted key finds the stale record");
    let difference = difference(&found, &built(&edited.copy(), None));
    assert!(difference.is_some(), "the oracle missed a describe of another body");
}

/// A record the store has that does not read is the build's error, naming the function and
/// the key: never described again in its place.
#[test]
fn a_record_that_does_not_read_refuses_the_build() {
    let shelf = Shelf::new(&[1], false);
    let program = over(&shelf, &program());
    let key = shelf.key(&program, 4, &member()).unwrap();
    shelf.stored.lock().insert(key, b"no record".to_vec());
    let errors = built(&program, Some(&shelf)).expect_err("a record that does not read");
    assert!(
        errors.iter().any(|it| it.what.contains("`f4`") && it.what.contains("does not read")),
        "{errors:?}"
    );
    assert_eq!(program.engine.describes.load(Ordering::Relaxed), 4, "`f4` is not described");
}

/// Two describes of one build under one key with other bytes are refused (rule 21); equal
/// ones are one put.
#[test]
fn two_records_of_one_key_are_one_put_or_a_refusal() {
    let put = |key: u8, bytes: &[u8]| Put {
        key: Key(ContentId([key; 32])),
        kind: records::KIND,
        bytes: bytes.to_vec(),
    };
    let sorted = records::sorted(vec![put(2, b"b"), put(1, b"a"), put(2, b"b")]).unwrap();
    assert_eq!(sorted, [put(1, b"a"), put(2, b"b")]);
    let refused = records::sorted(vec![put(1, b"a"), put(1, b"other")]).unwrap_err();
    assert!(refused.what.contains("one record key"), "{refused:?}");
}
