//! A names lookup hashes nothing (lane RP4, floor A): [`LoggedScope::get`] logs its read as the
//! module, namespace and name ([`CellSink::put_binding`]), and [`Log`] derives each binding
//! cell's key once per DISTINCT read ([`BindingKeys`]). The oracle: the cells logged are
//! byte-equal to the cells logging per lookup made (each read keyed by [`cell_key`] on the
//! spot), over every walk of the names_cells family from every module of both crates; the
//! floor: keys derived == distinct reads, however many lookups.
//!
//! [`LoggedScope::get`]: crate::names_cells::LoggedScope

use core::cell::RefCell;

use blaze_id::{ContentId, ItemKey};
use blaze_mem::{Frame, Tag};
use blaze_own::{AVec, Erased};
use blaze_surface::Namespace;

use super::names_cells::{BEFORE, Side, family};
use crate::cell_key::{BindingKeys, cell_key};
use crate::names::{Written, path};
use crate::names_cells::{CellSink, Log, Logged, NameCell};
use crate::names_record::{LoadNames, RecordNames};

/// The side's records, borrowed.
struct Borrowed<'s>(&'s [&'static [u8]]);

impl<'a> LoadNames<'a> for Borrowed<'_> {
    fn load(&self, krate: usize) -> Option<&'a [u8]> {
        self.0.get(krate).copied()
    }
}

/// A sink that logs into `log` AND keeps every read as logging per lookup made it: a binding
/// read keyed by [`cell_key`] at once (the old `LoggedScope::get`).
struct Twin<'l, 'a> {
    log: &'l Log<'a>,
    direct: RefCell<AVec<'a, NameCell>>,
}

impl CellSink for Twin<'_, '_> {
    fn put(&self, cell: NameCell) {
        self.direct.borrow_mut().push(cell);
        self.log.put(cell);
    }

    fn put_binding(&self, module: ItemKey, ns: Namespace, name: &str) {
        self.direct.borrow_mut().push(NameCell::Binding { module, name: cell_key(ns, name) });
        self.log.put_binding(module, ns, name);
    }
}

/// Expected line: `test tests::names_cells_keys::a_lookup_derives_no_key_and_a_distinct_read_one ... ok`,
/// printing `NAMES KEYS: lookups N, distinct M, keys derived M (floor M, ratio 1.00)`.
#[test]
fn a_lookup_derives_no_key_and_a_distinct_read_one() {
    let frame = Frame::new(Tag::Nameres).unwrap_or_else(|e| panic!("the test's frame: {e:?}"));
    let erased = Erased::new(&frame);
    let g = erased.handle();
    let before = Side::solve(&BEFORE);
    let world = RecordNames::new(&before.keys, Borrowed(&before.bytes), g);
    // ONE log over every walk: the repeats a task's many paths make through few scopes.
    let log = Log::new(g);
    let twin = Twin { log: &log, direct: RefCell::new(AVec::new_in(g)) };
    let logged = Logged { world: &world, log: &twin };
    let mut walks = 0u64;
    for krate in 0..before.keys.len() {
        let mut modules: AVec<'_, ItemKey> = AVec::new_in(g);
        modules.extend(before.record(krate).modules());
        for module in modules.iter().copied() {
            for (kind, segments) in family() {
                for ns in [Namespace::Types, Namespace::Values] {
                    walks += 1;
                    let _ = path(&logged, module, ns, Written { kind, segments: &segments }, g);
                }
            }
        }
    }
    let mut direct = twin.direct.into_inner();
    let lookups = direct.iter().filter(|c| matches!(c, NameCell::Binding { .. })).count() as u64;
    direct.sort_unstable();
    direct.dedup();
    let distinct = direct.iter().filter(|c| matches!(c, NameCell::Binding { .. })).count() as u64;
    let derived = log.binding_keys_derived();
    std::println!(
        "NAMES KEYS: {walks} walks, lookups {lookups}, distinct {distinct}, keys derived {derived} (floor {distinct}, ratio {:.2})",
        derived as f64 / distinct.max(1) as f64
    );
    assert_eq!(log.binding_reads(), lookups, "every lookup reached the log");
    assert!(lookups > distinct, "the fixture repeats no read: lookups {lookups}, distinct {distinct}");
    assert_eq!(derived, distinct, "a key derived per lookup, not per distinct read");

    // Byte-equal: the log's cells are the per-lookup cells, each once, under the same keys.
    let mut want: AVec<'_, ([u8; 16], NameCell)> = AVec::new_in(g);
    want.extend(direct.iter().map(|c| (c.key(), *c)));
    want.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let got = log.cells(g);
    assert_eq!(&got[..], &want[..], "the cells logged once per distinct read differ from the cells logged per lookup");
}

/// Expected line: `test tests::names_cells_keys::the_memo_keys_each_read_once_and_equal ... ok`.
/// N lookups of M distinct (module, namespace, name) reads: M keys derived, each equal to
/// [`cell_key`]'s; the module and the namespace keep reads of one name apart.
#[test]
fn the_memo_keys_each_read_once_and_equal() {
    let frame = Frame::new(Tag::Nameres).unwrap_or_else(|e| panic!("the test's frame: {e:?}"));
    let erased = Erased::new(&frame);
    let g = erased.handle();
    let mut memo = BindingKeys::new_in(g);
    let modules = [ItemKey(ContentId([1; 32])), ItemKey(ContentId([2; 32]))];
    let names = ["Vec", "Ve", "", "a\u{e9}b", "String", "x"];
    let (mut n, mut fresh) = (0u64, 0u64);
    for round in 0..10 {
        for module in modules {
            for ns in [Namespace::Types, Namespace::Values, Namespace::Macros] {
                for name in names {
                    let k = memo.key(module, ns, name);
                    n += 1;
                    fresh += u64::from(k.fresh);
                    assert_eq!(k.key, cell_key(ns, name), "{name:?} in {ns:?}: another key than cell_key's");
                    assert_eq!(k.fresh, round == 0, "{name:?} in {ns:?}, round {round}: fresh is wrong");
                }
            }
        }
    }
    let m = (modules.len() * 3 * names.len()) as u64;
    std::println!("NAMES KEYS memo: lookups {n}, distinct {m}, keys derived {} (floor {m})", memo.derived());
    assert_eq!(memo.derived(), m);
    assert_eq!(fresh, m);
    assert_eq!(memo.asked(), n);
    assert_eq!(memo.len() as u64, m);
}
