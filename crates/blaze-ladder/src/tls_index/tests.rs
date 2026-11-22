//! The run's thread-local numbering and descriptor (THREADS.md D2), and the key it makes.
//!
//! The rung `x0-tls-threads` passes on the old whole-program block too, so these pin the index
//! form itself: without it there is no `index_of`, no row per index, no `create_key` with an
//! ender: the tests cannot pass.

use std::alloc::Global;

use blaze_build::data::{DataAlloc, DataError, DataRecord, DataReloc, DataTarget};
use blaze_build::tls::index;
use blaze_build::tls::{SelfPointer, TlsError};
use blaze_typeck::ItemKey;

use super::{TlsVars, create_key, template_key};
use crate::run::RunError;

const A: ItemKey = ItemKey(0xa);
const B: ItemKey = ItemKey(0xb);

fn word(bytes: &[u8], at: usize) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(w)
}

fn record(bytes: &[u8], align: u8, relocs: &[DataReloc]) -> DataRecord<Global> {
    let mut allocs = Vec::new_in(Global);
    allocs.push(DataAlloc { bytes: bytes.to_vec_in(Global), align, mutable: true, relocs: relocs.to_vec_in(Global) });
    DataRecord { allocs }
}

/// D2: "Each thread-local static gets an INDEX, numbered by the run in the order records naming
/// it are placed". Asking A, B, A numbers A 0 and B 1, and A again finds 0.
/// Prints: "index_of: A 0, B 1, A 0".
#[test]
fn index_of_numbers_on_first_ask() {
    let mut vars = TlsVars::new();
    let got = [vars.index_of(A).unwrap(), vars.index_of(B).unwrap(), vars.index_of(A).unwrap()];
    assert_eq!(got, [0, 1, 0]);
    assert_eq!(vars.len(), 2);
    println!("index_of: A {}, B {}, A {}", got[0], got[1], got[2]);
}

/// D2's descriptor: `key @0, root @8, count @16, cap @24, vars @32` (40 bytes), `vars` the rows,
/// one `VarRow` of 32 bytes per index (`size, align, init, init_len`), its `init` the address of
/// the thread-local's template static. A (8 bytes, holding 40) is index 0, B (1 byte, holding 7)
/// is index 1; B's alignment is raised to 8 (macOS `aligned_alloc` refuses less).
/// Prints: "descriptor: count 2, cap 2, rows 64 bytes (32 each)".
#[test]
fn the_descriptor_has_a_32_byte_row_per_index() {
    let mut vars = TlsVars::new();
    vars.index_of(A).unwrap();
    vars.index_of(B).unwrap();
    let a = record(&40u64.to_le_bytes(), 3, &[]);
    let b = record(&[7], 0, &[]);
    let (desc, templates) = vars.parts_of(&[(A, &a), (B, &b)]).unwrap();
    let head = &desc.allocs[0];
    assert_eq!(head.bytes.len(), 40);
    assert!(head.mutable, "the key and root words are written at load");
    let (count, cap) = (word(&head.bytes, 16), word(&head.bytes, 24));
    assert_eq!((count, cap), (2, 2));
    assert_eq!(head.relocs.as_slice(), [DataReloc { offset: 32, target: DataTarget::Local(1) }]);
    let rows = &desc.allocs[1];
    assert_eq!(rows.bytes.len() as u64, cap * 32, "cap rows of 32 bytes each");
    assert_eq!([word(&rows.bytes, 0), word(&rows.bytes, 8), word(&rows.bytes, 24)], [8, 8, 8], "row 0: size, align, init_len");
    assert_eq!([word(&rows.bytes, 32), word(&rows.bytes, 40), word(&rows.bytes, 56)], [1, 8, 1], "row 1: size, align, init_len");
    assert_eq!(
        rows.relocs.as_slice(),
        [
            DataReloc { offset: 16, target: DataTarget::Static(template_key(A)) },
            DataReloc { offset: 48, target: DataTarget::Static(template_key(B)) },
        ]
    );
    assert_eq!(templates.len(), 2);
    assert_eq!(templates[0].0, template_key(A));
    assert_eq!(templates[0].1.allocs[0].bytes.as_slice(), 40u64.to_le_bytes());
    assert!(!templates[0].1.allocs[0].mutable, "a template is only copied from");
    assert_eq!(templates[1].1.allocs[0].bytes.as_slice(), [7]);
    assert_ne!(template_key(A), A, "a template's key is not its thread-local's");
    println!("descriptor: count {count}, cap {cap}, rows {} bytes (32 each)", rows.bytes.len());
}

/// A numbered thread-local with no record is refused naming it (`DataError::NoStatic`), and a
/// template pointing into its own value is refused (`TlsError::SelfPointer`): every thread's
/// copy would point at the template.
/// Prints: "refused: no record for 0xb; A points into itself".
#[test]
fn a_missing_record_and_a_self_pointer_are_refused_by_name() {
    let mut vars = TlsVars::new();
    vars.index_of(A).unwrap();
    vars.index_of(B).unwrap();
    let a = record(&40u64.to_le_bytes(), 3, &[]);
    let missing = vars.parts_of(&[(A, &a)]);
    assert!(matches!(missing, Err(RunError::Tls { error: TlsError::Data(DataError::NoStatic { static_: B }) })));
    let mut only_a = TlsVars::new();
    only_a.index_of(A).unwrap();
    let selfish = record(&0u64.to_le_bytes(), 3, &[DataReloc { offset: 0, target: DataTarget::Local(0) }]);
    let refused = only_a.parts_of(&[(A, &selfish)]);
    assert!(matches!(refused, Err(RunError::Tls { error: TlsError::SelfPointer(SelfPointer { static_: A }) })));
    println!("refused: no record for {:#x}; A points into itself", B.0);
}

unsafe extern "C" fn no_op_ender(_table: *mut core::ffi::c_void) {}

/// `create_key` makes the key in the descriptor's first word and writes `root`: macOS the key's
/// TSD slot offset `key * 8` with `ROOT_OWN` clear (D2: "macOS `key * 8` from
/// `tpidrro_el0 & !7`"); Linux an offset from `tpidr_el0` with `ROOT_OWN` set. Without an
/// ender's address it refuses (`PlaceError::NoEnder`).
/// Prints: "create_key: key <k>, root <hex>".
#[test]
fn create_key_writes_the_key_and_the_root() {
    let desc = std::boxed::Box::new([0u64; 5]);
    let at = desc.as_ptr() as u64;
    assert!(matches!(create_key(at, 0), Err(crate::place::PlaceError::NoEnder)));
    create_key(at, no_op_ender as usize as u64).expect("the key is made");
    // SAFETY: `desc` is alive and was written only by `create_key` on this thread.
    let (key, root) = unsafe { ((at as *const u64).read(), ((at + 8) as *const u64).read()) };
    if cfg!(target_os = "macos") {
        assert_eq!(root, key * 8, "macOS: the key's TSD slot");
        assert_eq!(root & index::ROOT_OWN, 0);
    } else {
        assert_ne!(root & index::ROOT_OWN, 0, "Linux: the getter stores the root itself");
    }
    println!("create_key: key {key}, root {root:#x}");
    // SAFETY: the key was made above; no thread holds a value under it.
    #[cfg(target_os = "macos")]
    { blaze_sys::posix::thread::key_delete(key as blaze_sys::posix::thread::pthread_key_t).unwrap(); }
    #[cfg(not(target_os = "macos"))]
    { unsafe { libc::pthread_key_delete(key as libc::pthread_key_t) }; }
}
