//! The descriptor table's invariant (descriptors.rs, "The invariant") on drawn interleavings of
//! reads, releases and forgets by three readers, a control for each of its lines, and the same
//! table under eight real threads.
//!
//! The interleaving's grain: a [`Table`] operation is one step at each point where another
//! thread can come between. A hit ([`Table::get`]) is one lock-free probe of the shared map, a
//! linearizable read (its own model checks: blaze-hash shared/tests.rs); a miss is two steps,
//! the probe with [`Table::opening`] and the open, then [`Table::keep`] under the lock; a
//! [`Table::forget`] is one step under the lock. So a drawn order of those steps is an order of
//! every interleaving the table's protocol has, run on the table's own code.

#[cfg(not(blaze_check))]
mod drawn {
    use std::{
        alloc::Global,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    use blaze_sim::{Broke, Config, Tape, check, control};

    use super::super::{Access, Key, Opening, Table, TablePlant};
    use crate::{Cache, FileName};

    /// The steps a case runs at most, and so the opens it can make.
    const STEPS: usize = 48;

    /// The names read: three under `pack`, one beside it.
    const NAMES: [&str; 4] = ["pack/a", "pack/b", "pack/c", "loose/d"];

    /// What a forget names: each file, and the directory of three.
    const FORGETS: [&str; 5] = ["pack/a", "pack/b", "pack/c", "loose/d", "pack"];

    /// The readers.
    const READERS: usize = 3;

    /// Which test descriptors are open, by the open that made them.
    struct Ledger {
        open: std::vec::Vec<AtomicBool, Global>,
    }

    /// A test descriptor: open until its last hold drops.
    struct Handle {
        id: usize,
        ledger: Arc<Ledger>,
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            self.ledger.open[self.id].store(false, Ordering::Relaxed);
        }
    }

    /// What the test knows of an open: its key, and how many forgets of its name had ended when
    /// its open began.
    #[derive(Clone, Debug)]
    struct Made {
        key: Key,
        forgotten_at_open: u64,
    }

    /// A reader between the two steps of a miss.
    struct Opened {
        key: Key,
        opening: Opening,
        handle: Arc<Handle>,
    }

    #[derive(Default)]
    struct Reader {
        opening: Option<Opened>,
        holds: std::vec::Vec<Arc<Handle>>,
    }

    fn key(at: usize, cache: Cache, access: Access) -> Key {
        (FileName::new(NAMES[at]).expect("a test name is valid"), cache, access)
    }

    /// The forgets of each name ended so far, by [`NAMES`] index.
    fn forgotten_of(forgotten: &[u64; NAMES.len()], key: &Key) -> u64 {
        forgotten[NAMES.iter().position(|it| *it == key.0.as_str()).expect("a test name")]
    }

    /// The invariant, read after every step.
    fn holds(table: &Table<Handle>, bound: usize, ledger: &Ledger, made: &[Made], readers: &[Reader], forgotten: &[u64; NAMES.len()]) -> Result<(), Broke> {
        let mut kept = std::vec::Vec::new_in(Global);
        let found = table.each_kept(|key, file| kept.push((key.clone(), file.id))); // clone: a refcount
        if kept.len() > bound {
            return Err(Broke::Oracle(format!("the table is past its bound: {} kept, bound {bound}", kept.len())));
        }
        if found != kept.len() {
            return Err(Broke::Oracle(format!("the halves differ: {found} found, {} kept", kept.len())));
        }
        for (key, id) in &kept {
            if made[*id].key != *key {
                return Err(Broke::Oracle(format!("{} is kept under {:?}, opened as {:?}", id, key, made[*id].key)));
            }
            if made[*id].forgotten_at_open != forgotten_of(forgotten, key) {
                return Err(Broke::Oracle(format!("open {id} of {} was kept after a forget of its name that ended after its open began", key.0)));
            }
        }
        for reader in readers {
            for hold in &reader.holds {
                if !ledger.open[hold.id].load(Ordering::Relaxed) {
                    return Err(Broke::Oracle(format!("a read holds open {} and it is closed", hold.id)));
                }
            }
        }
        for (id, open) in ledger.open.iter().enumerate().take(made.len()) {
            if !open.load(Ordering::Relaxed) {
                continue;
            }
            let is_kept = kept.iter().any(|(_, it)| *it == id);
            let is_held = readers.iter().any(|reader| reader.holds.iter().any(|it| it.id == id) || reader.opening.as_ref().is_some_and(|it| it.handle.id == id));
            if !is_kept && !is_held {
                return Err(Broke::Oracle(format!("open {id} is open but kept by nothing: not the table's, no read's")));
            }
        }
        Ok(())
    }

    /// One case: up to [`STEPS`] drawn steps of [`READERS`] readers on a table of a drawn bound,
    /// the invariant read after each.
    fn run(tape: &Tape, plant: TablePlant) -> Result<(), Broke> {
        let bound = tape.int_in(1..=3usize, "bound");
        let table: Table<Handle> = Table::new(bound);
        let mut open = std::vec::Vec::new_in(Global);
        open.resize_with(STEPS, || AtomicBool::new(false));
        let ledger = Arc::new(Ledger { open });
        let mut made: std::vec::Vec<Made> = std::vec::Vec::new();
        let mut readers: std::vec::Vec<Reader> = (0..READERS).map(|_| Reader::default()).collect();
        let mut forgotten = [0u64; NAMES.len()];
        for _ in 0..tape.int_in(0..=STEPS, "steps") {
            let reader = &mut readers[tape.index(READERS, "which reader")];
            if let Some(Opened { key, opening, handle }) = reader.opening.take() {
                let at_bound = table.each_kept(|_, _| {}) == bound;
                let id = handle.id;
                let got = table.keep(key.clone(), opening, handle, plant); // clone: a refcount
                let mut kept_now = false;
                table.each_kept(|it, file| kept_now |= *it == key && file.id == id);
                match (got.id == id, kept_now) {
                    (false, _) => tape.reached("another read's descriptor was kept meanwhile"),
                    (true, false) => tape.reached("a read that raced a forget kept nothing"),
                    (true, true) if at_bound => tape.reached("a keep at the bound evicted the oldest"),
                    (true, true) => {}
                }
                if made[got.id].key != key {
                    return Err(Broke::Oracle(format!("a keep of {} answered open {}", key.0, got.id)));
                }
                reader.holds.push(got);
            } else {
                match tape.int_in(0..=3u8, "what the reader does") {
                    0 | 1 => {
                        let cache = if tape.coin("around the cache") { Cache::Bypassed } else { Cache::Kept };
                        let access = if tape.coin("for writing") { Access::Write } else { Access::Read };
                        let key = key(tape.index(NAMES.len(), "which name"), cache, access);
                        let other = (key.0.clone(), cache, if access == Access::Read { Access::Write } else { Access::Read }); // clone: a refcount
                        let mut both = false;
                        table.each_kept(|it, _| both |= *it == other);
                        match table.get(&key) {
                            Some(file) => {
                                tape.reached("a read found its descriptor kept");
                                if both {
                                    tape.reached("one file kept for reading and for writing at once");
                                }
                                if made[file.id].key != key {
                                    return Err(Broke::Oracle(format!("a read of {} found open {}, of {}", key.0, file.id, made[file.id].key.0)));
                                }
                                reader.holds.push(file);
                            }
                            None if made.len() < STEPS => {
                                let opening = table.opening();
                                let id = made.len();
                                made.push(Made { key: key.clone(), forgotten_at_open: forgotten_of(&forgotten, &key) }); // clone: a refcount
                                ledger.open[id].store(true, Ordering::Relaxed);
                                // alloc: the test's descriptor.
                                let handle = Arc::new(Handle { id, ledger: Arc::clone(&ledger) });
                                reader.opening = Some(Opened { key, opening, handle });
                            }
                            None => {}
                        }
                    }
                    2 => {
                        if !reader.holds.is_empty() {
                            let at = tape.index(reader.holds.len(), "which hold");
                            drop(reader.holds.swap_remove(at));
                        }
                    }
                    _ => {
                        let name = FileName::new(FORGETS[tape.index(FORGETS.len(), "what a forget names")]).expect("a test name is valid");
                        table.forget(&name);
                        for (at, it) in NAMES.iter().enumerate() {
                            if FileName::new(it).expect("a test name is valid").is_under(&name) {
                                forgotten[at] += 1;
                            }
                        }
                    }
                }
            }
            holds(&table, bound, &ledger, &made, &readers, &forgotten)?;
        }
        Ok(())
    }

    /// The invariant: outside an operation, at most `bound` descriptors are kept, both halves
    /// hold the same count, every open descriptor is kept, held by a read or opened by a read not
    /// yet kept, a read's descriptor is open while it holds it, a read finds the descriptor of
    /// the key it asked (a read's and a write's of one file are two), and no descriptor of either
    /// access whose open began before a forget of its name is kept after that forget.
    #[test]
    fn the_table_keeps_its_invariant_on_drawn_interleavings() {
        let config = Config {
            must_reach: &[
                "a read found its descriptor kept",
                "one file kept for reading and for writing at once",
                "another read's descriptor was kept meanwhile",
                "a read that raced a forget kept nothing",
                "a keep at the bound evicted the oldest",
            ],
            ..Config::cases(4_000)
        };
        check(config, |tape| run(tape, TablePlant::None));
    }

    /// A keep that compares no forget keeps a descriptor its open began before a forget of its
    /// name, and the oracle sees it.
    #[test]
    fn the_oracle_catches_a_descriptor_kept_after_a_forget() {
        control(Config::cases(4_000), TablePlant::None, TablePlant::KeptAfterForget, "was kept after a forget", run);
    }

    /// A keep at the bound that evicts nothing grows the table past it.
    #[test]
    fn the_oracle_catches_a_table_past_its_bound() {
        control(Config::cases(4_000), TablePlant::None, TablePlant::PastBound, "past its bound", run);
    }

    /// An evicted descriptor the table keeps open (as one held by a retired entry of the found
    /// half would be) is open with no one keeping it.
    #[test]
    fn the_oracle_catches_an_evicted_descriptor_left_open() {
        control(Config::cases(4_000), TablePlant::None, TablePlant::Lingers, "kept by nothing", run);
    }
}

/// The table under eight real threads: reads, releases and forgets of six names on a table of
/// three, then every hold let go. The kept count never passes the bound, and once no read holds
/// anything every descriptor still open is a kept one.
#[cfg(not(blaze_check))]
mod threads {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::super::{Access, Table, TablePlant};
    use crate::{Cache, FileName};

    /// Open test descriptors, counted down as each closes.
    struct Handle {
        open: Arc<AtomicUsize>,
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            self.open.fetch_sub(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn eight_threads_keep_the_bound_and_close_what_they_let_go() {
        const NAMES: [&str; 6] = ["p/a", "p/b", "p/c", "p/d", "q/e", "q/f"];
        let table: Arc<Table<Handle>> = Arc::new(Table::new(3));
        let open = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8u64)
            .map(|seed| {
                let (table, open) = (Arc::clone(&table), Arc::clone(&open));
                std::thread::spawn(move || {
                    let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
                    let mut holds = Vec::new();
                    for _ in 0..2_000 {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        let name = FileName::new(NAMES[(rng % NAMES.len() as u64) as usize]).unwrap();
                        match (rng >> 8) % 8 {
                            0 => table.forget(&name),
                            1 | 2 => drop(holds.pop()),
                            _ => {
                                let access = if rng & (1 << 20) == 0 { Access::Read } else { Access::Write };
                                let key = (name, Cache::Kept, access);
                                let file = match table.get(&key) {
                                    Some(file) => file,
                                    None => {
                                        let opening = table.opening();
                                        open.fetch_add(1, Ordering::Relaxed);
                                        let opened = Arc::new(Handle { open: Arc::clone(&open) });
                                        table.keep(key, opening, opened, TablePlant::None)
                                    }
                                };
                                assert!(table.len() <= 3, "the table passed its bound");
                                holds.push(file);
                                if holds.len() > 4 {
                                    holds.remove(0);
                                }
                            }
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(table.len() <= 3);
        assert_eq!(open.load(Ordering::Relaxed), table.len(), "every descriptor still open is a kept one");
    }
}
