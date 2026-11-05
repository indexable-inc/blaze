use std::alloc::Global;
use std::{collections::HashMap, panic::AssertUnwindSafe};

use blaze_format::SchemaHash;
use blaze_ids::{ContentId, rng::SplitMix64};
use blaze_image::{Builder, Image, SectionId};

use super::*;
use crate::order_key;

/// Compiled in: "a" twice, so the first is the one interning gives.
const STATICS: [&str; 3] = ["a", "b", "a"];
const SCHEMA: SchemaHash = SchemaHash(ContentId([7; 32]));
const ID: SectionId = SectionId::SYMBOLS;

fn shape() -> Shape {
    Shape {
        statics: STATICS.len() as u32,
        static_bytes: |id| STATICS[id as usize - 1].as_bytes(),
        key: order_key,
        align: 1,
        size: 1,
        text: true,
        overlay: Overlay::Secret,
    }
}

/// A short name from few letters, so many share an order key and a hash tag.
fn name(rng: &mut SplitMix64) -> String {
    let len = rng.index(12);
    (0..len).map(|_| ['a', 'b', 'é', '_', '0'][rng.index(5)]).collect()
}

fn section_bytes(table: &Table, live: &Live) -> Vec<u8> {
    let mut builder = Builder::new();
    table.write(live, &mut builder.section(ID, SCHEMA));
    let image = Image::open(builder.finish().unwrap()).unwrap();
    let section = image.section(ID, SCHEMA).unwrap();
    image.whole(&section).unwrap().to_vec()
}

fn image_of(section: &[u8]) -> &'static Image<Vec<u8>> {
    let mut builder = Builder::new();
    builder.add(ID, SCHEMA, section);
    Box::leak(Box::new(Image::open(builder.finish().unwrap()).unwrap()))
}

fn reopen(table: &Table, live: &Live) -> Result<Table, RestoreError> {
    let image = image_of(&section_bytes(table, live));
    Table::restore(shape(), image, &image.section(ID, SCHEMA).unwrap(), &serial)
}

fn everything(table: &Table) -> Live {
    let mut live = Live::new();
    (1..table.end()).for_each(|id| live.mark_id(id));
    live
}

/// Every value of `model` reads back at its ID from `table`, and every other ID below `end`
/// names nothing.
fn check(table: &Table, model: &HashMap<u32, String>, end: u32) {
    for id in table.first..end {
        match model.get(&id) {
            Some(text) => {
                assert_eq!(table.get(id), Some(text.as_bytes()), "ID {id}");
                assert_eq!(table.find(text.as_bytes()), Some(id), "{text:?}");
                assert_eq!(table.intern(text.as_bytes()), id, "{text:?}");
                assert_eq!(table.key(id), order_key(text.as_bytes()));
            }
            None => assert_eq!(table.get(id), None, "ID {id} was freed"),
        }
    }
    assert_eq!(table.find(b"a"), Some(1));
    assert_eq!(table.find(b"b"), Some(2));
    assert_eq!(table.get(3), Some(&b"a"[..]));
    assert_eq!(table.get(0), None);
}

#[test]
fn compiled_in_values_are_their_ids() {
    let table = Table::new(shape());
    assert_eq!(table.intern(b"a"), 1, "a value compiled in twice is the first");
    assert_eq!(table.intern(b"b"), 2);
    assert_eq!(table.intern(b"c"), 4);
    assert_eq!(table.end(), 5);
    let restored = reopen(&table, &Live::new()).unwrap();
    assert_eq!(restored.intern(b"a"), 1);
    assert_eq!(restored.intern(b"b"), 2);
    assert_eq!(restored.get(4), None, "unmarked, so freed");
    assert_eq!(restored.intern(b"d"), 4, "a freed ID goes to the next new value");
}

#[test]
fn ids_are_equal_after_write_and_reopen() {
    let mut rng = SplitMix64::new(0x1d5);
    let table = Table::new(shape());
    let mut model = HashMap::new();
    for _ in 0..3000 {
        let text = name(&mut rng) + &rng.below(400).to_string();
        let id = table.intern(text.as_bytes());
        if id >= table.first {
            let previous = model.insert(id, text.clone());
            assert!(previous.is_none_or(|previous| previous == text), "ID {id} given twice");
        }
    }
    let mut current = table;
    for generation in 0..3 {
        let end = current.end();
        let next = reopen(&current, &everything(&current)).unwrap();
        check(&next, &model, end);
        let text = format!("made in generation {generation}");
        let id = next.intern(text.as_bytes());
        assert_eq!(id, end, "nothing was freed, so a new value takes the next ID");
        model.insert(id, text);
        current = next;
    }
}

#[test]
fn gc_never_renumbers_a_live_value() {
    let mut rng = SplitMix64::new(0x6c);
    let mut current = Table::new(shape());
    let mut model: HashMap<u32, String> = HashMap::new();
    for generation in 0..8 {
        for _ in 0..rng.below(600) {
            let text = format!("{}{generation}", name(&mut rng));
            let id = current.intern(text.as_bytes());
            if id >= current.first {
                model.insert(id, text);
            }
        }
        let mut live = Live::new();
        model.retain(|&id, _| {
            let keep = rng.below(3) != 0;
            if keep {
                live.mark_id(id);
            }
            keep
        });
        let end = current.end();
        let next = reopen(&current, &live).unwrap();
        check(&next, &model, end);

        // A new value takes a freed ID first, lowest first, then the next: never a live one.
        let mut freed: Vec<u32> = (next.first..end).filter(|id| !model.contains_key(id)).collect();
        freed.sort_unstable();
        for (at, &expect) in freed.iter().chain([end, end + 1].iter()).enumerate() {
            let text = format!("new {generation} {at}");
            let id = next.intern(text.as_bytes());
            assert_eq!(id, expect);
            assert!(model.insert(id, text).is_none(), "ID {id} was live");
        }
        current = next;
    }
}

#[test]
fn a_freed_value_made_again_takes_a_new_id() {
    let table = Table::new(shape());
    let (kept, dropped) = (table.intern(b"kept"), table.intern(b"dropped"));
    let mut live = Live::new();
    live.mark_id(kept);
    let restored = reopen(&table, &live).unwrap();
    assert_eq!(restored.find(b"dropped"), None);
    assert_eq!(restored.find(b"kept"), Some(kept));
    let again = restored.intern(b"dropped");
    assert_eq!(again, dropped, "the only freed ID, so the first new value's");
    assert_eq!(restored.intern(b"third"), restored.end() - 1);
}

#[test]
fn a_lost_id_panics() {
    let table = Table::new(shape());
    let dropped = table.intern(b"dropped");
    let restored = reopen(&table, &Live::new()).unwrap();
    let lost = std::panic::catch_unwind(AssertUnwindSafe(|| restored.bytes(dropped)));
    assert!(lost.is_err(), "reading a freed ID is a bug, never a quiet empty value");
    let lost = std::panic::catch_unwind(AssertUnwindSafe(|| restored.key(dropped)));
    assert!(lost.is_err());
}

#[test]
fn a_restored_table_interns_from_many_threads() {
    let table = Table::new(shape());
    let ids: Vec<u32> = (0..2000).map(|n| table.intern(format!("old {n}").as_bytes())).collect();
    let mut live = Live::new();
    ids.iter().step_by(2).for_each(|&id| live.mark_id(id));
    let restored = reopen(&table, &live).unwrap();
    let seen: Vec<Vec<u32>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8u32)
            .map(|worker| {
                let restored = &restored;
                scope.spawn(move || {
                    (0..3000u32)
                        .map(|n| (n + worker * 331) % 3000)
                        .map(|n| restored.intern(format!("new {n}").as_bytes()))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers.into_iter().map(|it| it.join().unwrap()).collect()
    });
    let mut owner = HashMap::new();
    for (worker, ids) in seen.iter().enumerate() {
        for (i, &id) in ids.iter().enumerate() {
            let n = (i as u32 + worker as u32 * 331) % 3000;
            assert_eq!(*owner.entry(id).or_insert(n), n, "ID {id} given to two values");
            assert!(!live.contains(id), "ID {id} is live");
            assert_eq!(restored.get(id), Some(format!("new {n}").as_bytes()));
        }
    }
    for &id in ids.iter().step_by(2) {
        assert!(restored.get(id).is_some());
    }
}

#[test]
fn a_corrupt_table_is_dropped_whole() {
    let table = Table::new(shape());
    (0..500).for_each(|n| _ = table.intern(format!("value {n}").as_bytes()));
    let mut builder = Builder::new();
    table.write(&everything(&table), &mut builder.section(ID, SCHEMA));
    let mut file = builder.finish().unwrap();
    let image = Image::open(file.clone()).unwrap();
    let section = image.section(ID, SCHEMA).unwrap();
    // The last byte of the section's values, far from its root.
    file[(section.at + section.len - 1) as usize] ^= 0x20;
    let image: &'static Image<Vec<u8>> = Box::leak(Box::new(Image::open(file).unwrap()));
    match Table::restore(shape(), image, &section, &serial) {
        Err(RestoreError::Corrupt(corrupt)) => assert_eq!(corrupt.section, ID),
        other => panic!("expected a corrupt section, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn another_shape_is_refused() {
    let section = section_bytes(&Table::new(shape()), &Live::new());
    let image = image_of(&section);
    let other = Shape { statics: 2, ..shape() };
    let refused = Table::restore(other, image, &image.section(ID, SCHEMA).unwrap(), &serial);
    assert!(matches!(refused, Err(RestoreError::Invalid { section: ID, .. })));
}

/// Whatever bytes a section holds, restoring it is an error or a table whose every read is in
/// bounds and whose interning stays sound: the checks of `validate` leave nothing for a read to
/// trip on. (That each value is reachable in the index is the CRC's to guard, not these.)
#[test]
fn a_restore_never_accepts_a_table_it_cannot_read() {
    let mut rng = SplitMix64::new(0xbad);
    let table = Table::new(shape());
    (0..40).for_each(|_| _ = table.intern(name(&mut rng).as_bytes()));
    let mut live = Live::new();
    (table.first..table.end()).filter(|id| id % 3 != 0).for_each(|id| live.mark_id(id));
    let section = section_bytes(&table, &live);
    let (mut accepted, mut refused) = (0, 0);
    for _ in 0..4000 {
        let mut bytes = section.clone();
        for _ in 0..1 + rng.below(3) {
            // Mostly the root, where one bit moves a whole column.
            let at = match rng.below(2) {
                0 => rng.below(size_of::<Root>() as u64),
                _ => rng.below(bytes.len() as u64),
            } as usize;
            bytes[at] ^= 1 << rng.below(8);
        }
        let image = image_of(&bytes);
        match Table::restore(shape(), image, &image.section(ID, SCHEMA).unwrap(), &serial) {
            Ok(restored) => {
                accepted += 1;
                for id in 0..restored.end() + 2 {
                    if let Some(value) = restored.get(id) {
                        assert!(std::str::from_utf8(value).is_ok());
                        let _ = restored.key(id);
                        let again = restored.intern(value);
                        assert_eq!(restored.get(again), Some(value));
                    }
                }
                let id = restored.intern(b"made after a strange restore");
                assert!(restored.get(id).is_some());
            }
            Err(RestoreError::Invalid { .. } | RestoreError::Corrupt(_)) => refused += 1,
            Err(other) => panic!("{other}"),
        }
    }
    assert!(accepted > 0 && refused > 0, "{accepted} accepted, {refused} refused");
}

/// `section` with `patch` applied at byte `at`, as a restore sees it, and `section`'s root.
fn patched(
    section: &[u8],
    at: impl FnOnce(&Root) -> u64,
    patch: &[u8],
) -> (Result<Table, RestoreError>, Root) {
    let image = image_of(section);
    let root = *image.value::<Root>(&image.section(ID, SCHEMA).unwrap(), 0).unwrap();
    let at = at(&root) as usize;
    let mut bytes = section.to_vec();
    bytes[at..at + patch.len()].copy_from_slice(patch);
    let image = image_of(&bytes);
    (Table::restore(shape(), image, &image.section(ID, SCHEMA).unwrap(), &serial), root)
}

/// A start past the values is refused, naming its byte, before the UTF-8 check could slice past
/// them: a value's end is checked against the values' end, not only against the next start,
/// which a later chunk checks.
#[test]
fn a_start_past_the_values_is_refused_where_it_is() {
    let table = Table::new(shape());
    let mut live = Live::new();
    [&b"first value"[..], b"second", b"third"]
        .iter()
        .for_each(|value| live.mark_id(table.intern(value)));
    let section = section_bytes(&table, &live);
    let (restored, root) = patched(&section, |root| root.starts + 4, &u32::MAX.to_le_bytes());
    let what = "a value's bytes out of order or past the values";
    assert_eq!(
        restored.err(),
        Some(RestoreError::Invalid { section: ID, what, at: root.starts + 4 })
    );
}

/// A value that is not UTF-8 in a text table is refused at its first byte.
#[test]
fn a_value_that_is_not_text_is_refused_where_it_is() {
    let table = Table::new(shape());
    let mut live = Live::new();
    live.mark_id(table.intern(b"text"));
    let section = section_bytes(&table, &live);
    let (restored, root) = patched(&section, |root| root.bytes, &[0xff]);
    let what = "a value that is not UTF-8";
    assert_eq!(restored.err(), Some(RestoreError::Invalid { section: ID, what, at: root.bytes }));
    let error = RestoreError::Invalid { section: ID, what, at: root.bytes };
    assert!(error.to_string().contains(&format!("at byte {}", root.bytes)), "{error}");
}

/// `section` with its index emptied but for one run of `len` full groups from group `from`
/// (through the last group into the first), filled with the index's own slots in order: an
/// image the writer never made, its CRC whole. Also `section`'s root.
fn with_run(section: &[u8], from: usize, len: usize) -> (Result<Table, RestoreError>, Root) {
    let image = image_of(section);
    let root = *image.value::<Root>(&image.section(ID, SCHEMA).unwrap(), 0).unwrap();
    let (ctrl, ids, slots) = (root.ctrl as usize, root.ids as usize, root.groups as usize * GROUP);
    let tag = |bytes: &[u8], slot: usize| bytes[ctrl + slot];
    let id = |bytes: &[u8], slot: usize| {
        u32::from_le_bytes(bytes[ids + 4 * slot..][..4].try_into().unwrap())
    };
    let full: Vec<(u8, u32)> = (0..slots)
        .filter(|&slot| tag(section, slot) != 0)
        .map(|slot| (tag(section, slot), id(section, slot)))
        .collect();
    assert!(full.len() >= len * GROUP, "{} slots to fill {len} groups", full.len());
    let mut bytes = section.to_vec();
    bytes[ctrl..ctrl + slots].fill(0);
    bytes[ids..ids + 4 * slots].fill(0);
    for (n, &(tag, id)) in full[..len * GROUP].iter().enumerate() {
        // A group's control word is little-endian, so slot `i`'s tag is byte `i` of the column.
        let slot = (from * GROUP + n) % slots;
        bytes[ctrl + slot] = tag;
        bytes[ids + 4 * slot..][..4].copy_from_slice(&id.to_le_bytes());
    }
    let image = image_of(&bytes);
    (Table::restore(shape(), image, &image.section(ID, SCHEMA).unwrap(), &serial), root)
}

/// Review finding L-1: an image at most half full whose full slots make one long run, which
/// every lookup that misses there walks, is refused where the run starts: inside one chunk of
/// the check, across two, and through the last group into the first. A run one group shorter
/// is not refused for its length.
#[test]
fn a_long_run_of_full_groups_is_refused_where_it_starts() {
    let table = Table::new(shape());
    (0..10_000u32).for_each(|n| _ = table.intern(format!("value {n}").as_bytes()));
    let section = section_bytes(&table, &everything(&table));
    let (restored, root) = with_run(&section, 0, 0);
    assert!(restored.is_err(), "an empty index misses the compiled-in values");
    let groups = root.groups as usize;
    assert_eq!(groups, 2 * (CHUNK / GROUP), "two chunks of the check");
    let refused = refused_run(groups);
    assert_eq!(refused, 35, "for 2^12 groups: ln 2 (12 + 65) / (8 (ln 2 - 1/2)) is 34.5");
    let what = "a run of full index groups longer than half full allows";
    for from in [0, CHUNK / GROUP - refused / 2, groups - refused / 2] {
        let (restored, _) = with_run(&section, from, refused);
        let at = root.ctrl + 8 * from as u64;
        assert_eq!(
            restored.err(),
            Some(RestoreError::Invalid { section: ID, what, at }),
            "a run from group {from}"
        );
        let (restored, _) = with_run(&section, from, refused - 1);
        assert!(
            !matches!(restored, Err(RestoreError::Invalid { what: refused_for, .. }) if refused_for == what),
            "a run of {} from group {from} is short enough",
            refused - 1
        );
    }
}

/// Review finding L-2: an image whose index key is not the one its layout was placed by is
/// refused, naming the key's byte, where every lookup in it would miss and give each value a
/// second ID: in a table with values compiled in (they are looked up) and in one without
/// (every kept value is).
#[test]
fn an_index_under_another_key_is_refused() {
    let what = "an index whose key does not find its values";
    let at = std::mem::offset_of!(Root, key0) as u64;
    for statics in [STATICS.len() as u32, 0] {
        let shape = || Shape { statics, ..shape() };
        let table = Table::new(shape());
        (0..1_000u32).for_each(|n| _ = table.intern(format!("value {n}").as_bytes()));
        let section = section_bytes(&table, &everything(&table));
        let restore = |bytes: &[u8]| {
            let image = image_of(bytes);
            Table::restore(shape(), image, &image.section(ID, SCHEMA).unwrap(), &serial)
        };
        assert!(restore(&section).is_ok(), "{statics} compiled in: the image as written");
        let mut bytes = section.clone();
        bytes[at as usize..][..8].iter_mut().for_each(|byte| *byte ^= 0x5a);
        assert_eq!(
            restore(&bytes).err(),
            Some(RestoreError::Invalid { section: ID, what, at }),
            "{statics} compiled in"
        );
    }
}

/// Review finding M-1: a fixed shape's tables (the fuzz hooks') place by one key, so an input
/// takes the same probes in every run, while every other table draws its own. (Under the model
/// checker a drawn key fails `model.rs`'s models: "the model is not deterministic".)
#[test]
fn only_a_fixed_shape_places_alike_every_time() {
    let fixed = || Table::new(Shape { overlay: Overlay::Fixed, ..shape() });
    assert_eq!(fixed().overlay, fixed().overlay);
    assert_ne!(Table::new(shape()).overlay, Table::new(shape()).overlay);
}

/// The table against an `HashMap<Vec<u8>, u32>` over seeded values, many sharing a hash tag,
/// a home group or an order key: a value new to the map takes the next ID, a known one its own,
/// and every ID reads back its bytes and key, in the overlay and again after a write and a
/// restore (the mapped base), where new values continue from the same next ID.
#[test]
fn interning_agrees_with_a_map_model() {
    for seed in 0..3 {
        let mut rng = SplitMix64::new(0x5eed + seed);
        let table = Table::new(shape());
        let mut model: blaze_hash::HashMap<Vec<u8>, u32, Global> =
            blaze_hash::HashMap::new_in(Global); // alloc: a test's model
        let mut next = table.end();
        // Text, as the shape's restore checks: short names, numbered ones, long runs, digits.
        let value = |rng: &mut SplitMix64| -> Vec<u8> {
            match rng.below(4) {
                0 => name(rng).into_bytes(),
                1 => format!("{}{}", name(rng), rng.below(50)).into_bytes(),
                2 => "x".repeat(rng.index(300)).into_bytes(),
                _ => {
                    let digits = rng.next_u64().to_string();
                    digits.as_bytes()[..rng.index(digits.len() + 1)].to_vec()
                }
            }
        };
        let check = |table: &Table,
                     model: &mut blaze_hash::HashMap<Vec<u8>, u32, Global>,
                     next: &mut u32,
                     bytes: Vec<u8>| {
            // alloc: a test's model
            let id = table.intern(&bytes);
            let expect = match model.get(&bytes) {
                Some(&id) => id,
                None if STATICS.iter().any(|s| s.as_bytes() == bytes) => id,
                None => {
                    *next += 1;
                    *next - 1
                }
            };
            assert_eq!(id, expect, "seed {seed}: {bytes:?}");
            model.insert(bytes.clone(), id);
            assert_eq!(table.get(id), Some(&bytes[..]));
            assert_eq!(table.find(&bytes), Some(id));
            assert_eq!(table.key(id), order_key(&bytes));
        };
        for _ in 0..4_000 {
            let bytes = value(&mut rng);
            check(&table, &mut model, &mut next, bytes);
        }
        assert_eq!(table.end(), next);
        let mut live = Live::new();
        model.values().for_each(|&id| live.mark_id(id));
        let restored = reopen(&table, &live).unwrap();
        for (bytes, &id) in &model {
            assert_eq!(restored.get(id), Some(&bytes[..]), "seed {seed}: restored {bytes:?}");
            assert_eq!(restored.find(bytes), Some(id));
        }
        for _ in 0..1_000 {
            let bytes = value(&mut rng);
            check(&restored, &mut model, &mut next, bytes);
        }
    }
}

#[test]
fn live_sets() {
    let mut live = Live::new();
    assert!(!live.contains(0) && !live.contains(1_000_000));
    for id in [1, 63, 64, 65, 4095, 1_000_000] {
        live.mark_id(id);
        live.mark_id(id);
    }
    assert_eq!(live.count(), 6);
    assert!(live.contains(64) && live.contains(1_000_000) && !live.contains(66));
}

/// The mapped index's hash spreads names over tags and home groups under any key.
#[test]
fn the_index_hash_spreads() {
    let names: Vec<Vec<u8>> = (0..20_000u32).map(|n| format!("name {n}").into_bytes()).collect();
    let key = index_key(names.iter().map(Vec::as_slice));
    let hashes: Vec<u64> = names.iter().map(|name| key.hash(name)).collect();
    let mut distinct = hashes.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), hashes.len());
    // Every tag value is used, and the low bits (the probe start) spread over 1024 groups.
    let mut tags = [0u32; 128];
    let mut starts = vec![0u32; 1024];
    for &hash in &hashes {
        tags[(swiss::tag(hash) & 0x7f) as usize] += 1;
        starts[hash as usize & 1023] += 1;
    }
    assert!(tags.iter().all(|&n| n > 0));
    assert!(starts.iter().all(|&n| n < 60), "{:?}", starts.iter().max());
    assert_ne!(key.hash(b""), key.hash(b"\0"));
    assert_ne!(key.hash(b"ab"), key.hash(b"ba"));
}

/// The hash the image's index is laid out by is SipHash-1-3, on fixed names under a fixed key
/// (an independent port's values): it may not depend on the process, the build or the
/// platform. The key is a function of the kept values alone.
#[test]
fn the_index_hash_is_the_images() {
    let key = SipKey::new(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
    let pinned: [(&[u8], u64); 5] = [
        (b"", 0xabac_0158_050f_c4dc),
        (b"self", 0x22f0_2dea_c666_6ba0),
        (b"iter_mut", 0x777b_0425_4dc8_bc6e),
        (b"core::fmt", 0x7177_ac06_7f9d_9af4),
        ("ünïcödé".as_bytes(), 0x0179_9c15_29bb_30bd),
    ];
    for (bytes, hash) in pinned {
        assert_eq!(key.hash(bytes), hash, "{bytes:?}");
    }
    let values: [&[u8]; 3] = [b"x", b"yz", b""];
    assert_eq!(index_key(values), index_key(values.map(|it| it)));
    assert_ne!(index_key(values), index_key([&b"xy"[..], b"z", b""]), "lengths are in the key");
    assert_ne!(index_key(values), index_key([&b"yz"[..], b"x", b""]), "the order is in the key");
}

/// Two tables that interned the same values in the same order write the same bytes, though
/// each drew its own random key for the overlay: no image depends on that key.
#[test]
fn an_image_is_the_same_whatever_the_overlay_key() {
    let tables = [Table::new(shape()), Table::new(shape())];
    assert_ne!(tables[0].overlay, tables[1].overlay);
    for table in &tables {
        let mut rng = SplitMix64::new(9);
        (0..3_000).for_each(|_| _ = table.intern(name(&mut rng).as_bytes()));
        (0..3_000u32).for_each(|n| _ = table.intern(format!("value {n}").as_bytes()));
    }
    let live = everything(&tables[0]);
    assert_eq!(section_bytes(&tables[0], &live), section_bytes(&tables[1], &live));
    let reopened = [reopen(&tables[0], &live).unwrap(), reopen(&tables[1], &live).unwrap()];
    assert_eq!(reopened[0].base.key, reopened[1].base.key);
    assert_ne!(reopened[0].overlay, reopened[1].overlay);
}

/// `n` distinct 24-byte ASCII names with one [`blaze_phf::name_hash`], as review finding S5
/// made them: every step of that hash inverts, so after two free words the third is solved for
/// a fixed state before the finish; about one try in 256 solves to ASCII.
fn names_with_one_name_hash(n: usize) -> Vec<String> {
    use blaze_phf::{NAME_K, name_hash_start, name_hash_word};
    const STATE: u64 = 0x0123_4567_89ab_cdef;
    // NAME_K's inverse mod 2^64, by Newton's iteration: each step doubles the bits that hold.
    let mut inverse = NAME_K;
    (0..6).for_each(|_| {
        inverse = inverse.wrapping_mul(2u64.wrapping_sub(NAME_K.wrapping_mul(inverse)))
    });
    assert_eq!(NAME_K.wrapping_mul(inverse), 1);
    let before = STATE.rotate_right(26).wrapping_mul(inverse);
    let hex = |word: u64| -> [u8; 8] {
        std::array::from_fn(|at| b"0123456789abcdef"[(word >> (4 * at)) as usize & 15])
    };
    let mut names = Vec::with_capacity(n);
    for tries in 0u64.. {
        if names.len() == n {
            break;
        }
        let (first, second) = (hex(tries), hex(tries >> 32 | 0x5a5a_0000));
        let mut h = name_hash_start(24);
        h = name_hash_word(h, u64::from_le_bytes(first));
        h = name_hash_word(h, u64::from_le_bytes(second));
        let third = h ^ before;
        if third & 0x8080_8080_8080_8080 != 0 {
            continue;
        }
        let bytes = [first, second, third.to_le_bytes()].concat();
        names.push(String::from_utf8(bytes).expect("ASCII"));
    }
    names
}

/// Review finding S5: names with one `name_hash` cost the table a bounded number of probes
/// each, interned, found again, written into an image and found in it. With the table placed
/// by `name_hash` again, each probe walks past every name before it: interning 10k such names
/// loaded 26 million control words (16k: 74 million and 607 ms, against 75 thousand and 1.7 ms
/// placed by SipHash), where the bound is 16 a name.
#[test]
fn names_with_one_name_hash_take_bounded_probes() {
    const N: usize = 10_000;
    let names = names_with_one_name_hash(N);
    let hash = blaze_phf::name_hash(names[0].as_bytes());
    assert!(names.iter().all(|name| blaze_phf::name_hash(name.as_bytes()) == hash));
    assert_eq!(names.iter().collect::<std::collections::BTreeSet<_>>().len(), N, "distinct names");

    let table = Table::new(shape());
    probes::take();
    let ids: Vec<u32> = names.iter().map(|name| table.intern(name.as_bytes())).collect();
    let interned = probes::take();
    for (name, &id) in names.iter().zip(&ids) {
        assert_eq!(table.find(name.as_bytes()), Some(id));
    }
    let found = probes::take();
    let restored = reopen(&table, &everything(&table)).unwrap();
    let written = probes::take();
    for (name, &id) in names.iter().zip(&ids) {
        assert_eq!(restored.intern(name.as_bytes()), id);
    }
    let found_in_image = probes::take();
    let bound = 16 * N as u64;
    for (what, probes) in [
        ("interning", interned),
        ("finding", found),
        ("writing the index", written),
        ("finding in the image", found_in_image),
    ] {
        assert!(
            probes <= bound,
            "{what} {N} names with one name_hash loaded {probes} control words, over {bound}"
        );
    }
}

/// Four workers taking every fourth job, in no fixed order.
// dyn: `blaze_image::Jobs`'s signature, one call per restore
fn four_threads(n: usize, job: &(dyn Fn(usize) + Sync)) {
    std::thread::scope(|scope| {
        for worker in 0..4 {
            scope.spawn(move || (worker..n).rev().step_by(4).for_each(job));
        }
    });
}

/// A restore split across workers gives the table and the error one core gives: the first
/// corrupt group, wherever the workers were.
#[test]
fn a_parallel_restore_is_the_serial_one() {
    let table = Table::new(shape());
    (0..200_000u32).for_each(|n| _ = table.intern(format!("symbol number {n}").as_bytes()));
    let mut builder = Builder::new();
    table.write(&everything(&table), &mut builder.section(ID, SCHEMA));
    let file = builder.finish().unwrap();
    let image: &'static Image<Vec<u8>> = Box::leak(Box::new(Image::open(file.clone()).unwrap()));
    let section = image.section(ID, SCHEMA).unwrap();
    assert!(section.groups as usize > 4 * CHUNK_GROUPS, "{} groups", section.groups);
    let parallel = Table::restore(shape(), image, &section, &four_threads).unwrap();
    for id in (parallel.first..parallel.end()).step_by(997) {
        assert_eq!(parallel.get(id), table.get(id));
    }

    let mut corrupt = file;
    let group_len = image.group_len() as usize;
    for group in [3, section.groups as usize - 2] {
        corrupt[section.at as usize + group * group_len + 17] ^= 0x40;
    }
    let serial_error = {
        let image: &'static Image<Vec<u8>> =
            Box::leak(Box::new(Image::open(corrupt.clone()).unwrap()));
        Table::restore(shape(), image, &section, &serial).err().unwrap()
    };
    for _ in 0..8 {
        let image: &'static Image<Vec<u8>> =
            Box::leak(Box::new(Image::open(corrupt.clone()).unwrap()));
        let error = Table::restore(shape(), image, &section, &four_threads).err().unwrap();
        assert_eq!(error, serial_error);
    }
    assert!(
        matches!(serial_error, RestoreError::Corrupt(Corrupt { group: 3, .. })),
        "{serial_error}"
    );
}

/// Interning a value the table holds, finding it and reading it allocate nothing: from the
/// compiled-in values, the mapped base of a restored table, and the overlay over it.
#[test]
fn hits_allocate_nothing() {
    let table = Table::new(shape());
    let mut model = HashMap::new();
    let mut rng = SplitMix64::new(41);
    for _ in 0..500 {
        let text = name(&mut rng);
        model.insert(table.intern(text.as_bytes()), text);
    }
    let restored = reopen(&table, &everything(&table)).unwrap();
    let mut overlay = HashMap::new();
    for _ in 0..500 {
        let text = format!("new {}", rng.next_u64());
        overlay.insert(restored.intern(text.as_bytes()), text);
    }
    for (what, values) in [("the mapped base", &model), ("the overlay", &overlay)] {
        for (&id, text) in values {
            let text = text.as_bytes();
            blaze_alloc_count::assert_at_most(0, what, || {
                assert_eq!(restored.intern(text), id);
                assert_eq!(restored.find(text), Some(id));
                assert_eq!(restored.get(id), Some(text));
            });
        }
    }
    blaze_alloc_count::assert_at_most(0, "a compiled-in value", || {
        assert_eq!(restored.intern(b"a"), 1);
        assert_eq!(restored.find(b"b"), Some(2));
        assert_eq!(restored.get(3), Some(&b"a"[..]));
    });
}

/// Interning new values into a table allocates nothing: the overlay's bytes come from the
/// store's region and its ID column is a reserved region of its own.
#[test]
fn misses_allocate_nothing() {
    let table = Table::new(shape());
    table.intern(b"warm");
    let ids = blaze_alloc_count::assert_at_most(0, "interning new values", || {
        (0..20_000u32)
            .map(|n| table.intern(&n.to_le_bytes()))
            .fold(0u64, |sum, id| sum + u64::from(id))
    });
    assert!(ids > 0);
    for n in 0..20_000u32 {
        assert_eq!(
            table.find(&n.to_le_bytes()).and_then(|id| table.get(id)),
            Some(&n.to_le_bytes()[..])
        );
    }
}

/// IDs set from many threads at once, in no order and far apart, read back from every thread.
#[test]
fn the_id_column_publishes_each_slot_once() {
    let table = Table::new(shape());
    std::thread::scope(|scope| {
        for worker in 0..8u32 {
            let table = &table;
            scope.spawn(move || {
                for n in 0..5_000u32 {
                    let bytes = (worker << 24 | n).to_le_bytes();
                    let id = table.intern(&bytes);
                    assert_eq!(table.get(id), Some(&bytes[..]));
                }
            });
        }
    });
    assert_eq!(table.end() - table.first, 40_000);
}
