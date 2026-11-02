use std::sync::Arc;

use base_db::{Crate, Db, Env};
use blaze_format::Aligned;
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use hir_def::{
    ConstId, ModuleDefId,
    deps::{ImageBuf, ImageStore, Mode},
    nameres::{crate_def_map, frozen},
};
use hir_expand::fixture;
use parking_lot::Mutex;
use rustc_abi::Size;
use rustc_type_ir::{IntTy, Mutability, UintTy};

use super::{codec, *};
use crate::{
    ComplexMemoryMap, MemoryMap, PointerAt,
    next_solver::{
        AllocationData, Const, ConstExt as _, ConstKind, ScalarInt, Ty, TyKind, Tys, ValTree,
        ValTreeKind, ValueConst, default_types,
    },
};

const KEY: ContentId = ContentId([3; 32]);
const BASE: ContentId = ContentId([5; 32]);

fn ty(kind: TyKind<'static>) -> Ty<'static> {
    Ty::new_without_interner(kind)
}

fn usize_value(n: u64) -> Const<'static> {
    let scalar = ScalarInt::try_from_uint(n, Size::from_bytes(8u64)).unwrap();
    let value = ValueConst::new(default_types().types.usize, ValTree::new(ValTreeKind::Leaf(scalar)));
    Const::new_without_interner(ConstKind::Value(value))
}

fn alloc(ty: Ty<'static>, memory: &[u8], memory_map: MemoryMap<'static>) -> Allocation<'static> {
    Allocation::new(AllocationData { ty, memory: memory.into(), memory_map })
}

fn scalar(ty: Ty<'static>, memory: &[u8]) -> Allocation<'static> {
    alloc(ty, memory, MemoryMap::Empty)
}

/// Two pieces, inserted out of address order, with their alignments and the reference's
/// pointer word with provenance.
fn complex() -> MemoryMap<'static> {
    let mut map = ComplexMemoryMap::default();
    map.insert(0x2000, Box::new(*b"hello"));
    map.insert(0x1000, Box::new([1, 2, 3, 4]));
    map.note_align(0x1000, 4);
    map.note_align(0x2000, 1);
    map.note_pointer(PointerAt::Value(0));
    MemoryMap::Complex(Box::new(map))
}

/// One allocation of each kind the codec keeps.
fn kept_kinds() -> Vec<Allocation<'static>> {
    let types = &default_types().types;
    let regions = &default_types().regions;
    let u8_array = ty(TyKind::Array(types.u8, usize_value(3)));
    let pair = ty(TyKind::Tuple(Tys::new_from_slice(&[types.u16, types.bool])));
    let nested = ty(TyKind::Tuple(Tys::new_from_slice(&[pair, u8_array, types.unit])));
    let str_ref = [0, 0x20, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0];
    let slice_ref = [0, 0x10, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0];
    let mut kinds = vec![
        scalar(types.bool, &[1]),
        scalar(types.char, &u32::from('é').to_le_bytes()),
        scalar(types.unit, &[]),
        scalar(types.never, &[]),
        scalar(types.f16, &[0, 0x3c]),
        scalar(types.f32, &1.5f32.to_le_bytes()),
        scalar(types.f64, &(-2.25f64).to_le_bytes()),
        scalar(types.f128, &[7; 16]),
        scalar(u8_array, &[1, 2, 3]),
        scalar(pair, &[9, 0, 1, 0]),
        scalar(nested, &[9, 0, 1, 0, 1, 2, 3, 0]),
        scalar(ty(TyKind::Array(types.u8, usize_value(0))), &[]),
        scalar(ty(TyKind::RawPtr(types.u8, Mutability::Mut)), &0x1000u64.to_le_bytes()),
        alloc(types.static_str_ref, &str_ref, complex()),
        alloc(types.static_u8_slice, &slice_ref, complex()),
        alloc(ty(TyKind::Ref(regions.erased, types.u32, Mutability::Not)), &[0, 0x10, 0, 0, 0, 0, 0, 0], complex()),
        alloc(ty(TyKind::Ref(regions.statik, types.u8_slice, Mutability::Mut)), &slice_ref, complex()),
        alloc(ty(TyKind::Slice(types.u8)), &[1, 2], MemoryMap::simple(Box::new([4, 5]))),
        alloc(types.u32, &[1, 0, 0, 0], MemoryMap::simple(Box::default())),
        alloc(types.u32, &[1, 0, 0, 0], MemoryMap::Complex(Box::default())),
    ];
    let sizes = [8, 1, 2, 4, 8, 16];
    for (int, size) in [IntTy::Isize, IntTy::I8, IntTy::I16, IntTy::I32, IntTy::I64, IntTy::I128]
        .into_iter()
        .zip(sizes)
    {
        kinds.push(scalar(ty(TyKind::Int(int)), &(-(size as i128)).to_le_bytes()[..size]));
    }
    for (uint, size) in [UintTy::Usize, UintTy::U8, UintTy::U16, UintTy::U32, UintTy::U64, UintTy::U128]
        .into_iter()
        .zip(sizes)
    {
        kinds.push(scalar(ty(TyKind::Uint(uint)), &(size as u128 * 0x1234).to_le_bytes()[..size]));
    }
    kinds
}

#[test]
fn every_kept_kind_reads_back() {
    for value in kept_kinds() {
        let body = codec::encode(value).unwrap_or_else(|| panic!("{value:?} is not kept"));
        let filed = codec::file(KEY, &body);
        let read = codec::decode(&filed, KEY).unwrap();
        assert_eq!(read, value, "{value:?}");
        assert_eq!(codec::encode(read).as_deref(), Some(&body[..]), "{value:?} writes the same");
    }
}

/// A value with a part the codec cannot write is not written at all, never in part.
#[test]
fn what_is_not_kept_is_not_encoded() {
    let types = &default_types().types;
    let with_type_id = {
        let mut map = ComplexMemoryMap::default();
        map.type_ids.push(Some(types.u8));
        MemoryMap::Complex(Box::new(map))
    };
    let error_ref = ty(TyKind::Ref(default_types().regions.error, types.u8, Mutability::Not));
    for value in [
        scalar(types.error, &[]),
        scalar(types.dyn_trait_dummy_self, &[]),
        scalar(error_ref, &[0; 8]),
        scalar(ty(TyKind::Array(types.u8, default_types().consts.error)), &[]),
        scalar(ty(TyKind::Tuple(Tys::new_from_slice(&[types.u8, types.error]))), &[0]),
        alloc(types.u8, &[0], with_type_id),
    ] {
        assert_eq!(codec::encode(value), None, "{value:?} is kept");
    }
}

/// Every truncation of a kept value is refused, and a flip of any of its bytes reads back some
/// allocation or is refused, never a panic. The content hash, which catches every flip, is the
/// store's.
#[test]
fn damaged_values_are_refused_within_bounds() {
    let types = &default_types().types;
    let nested = ty(TyKind::Tuple(Tys::new_from_slice(&[
        ty(TyKind::Array(types.u8, usize_value(2))),
        types.static_str_ref,
    ])));
    let memory = [1, 2, 0, 0, 0, 0, 0, 0, 0, 0x20, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0];
    let filed = codec::file(KEY, &codec::encode(alloc(nested, &memory, complex())).unwrap());
    for len in 0..filed.len() {
        assert!(codec::decode(&filed[..len], KEY).is_err(), "a value cut to {len} bytes reads");
    }
    for at in 0..filed.len() {
        let mut bytes = filed.clone();
        bytes[at] ^= 0xff;
        let decoded = codec::decode(&bytes, KEY);
        assert!(at >= codec::HEADER || decoded.is_err(), "byte {at} of the header flipped reads");
    }
    assert_eq!(codec::decode(&filed, ContentId([4; 32])), Err("a value filed under another key"));
    let mut longer = filed.clone();
    longer.push(0);
    assert_eq!(codec::decode(&longer, KEY), Err("bytes after the value"));
    let deep = codec::file(KEY, &[21; 200]);
    assert_eq!(codec::decode(&deep, KEY), Err("a type nested deeper than any kept"));
}

#[test]
fn the_key_changes_with_each_input() {
    let item = ContentId([9; 32]);
    let base = key(BASE, item);
    assert_eq!(key(BASE, item), base, "a key is a function of its inputs");
    assert_ne!(key(ContentId([6; 32]), item), base, "the crate's check-result key");
    assert_ne!(key(BASE, ContentId([8; 32])), base, "the item's key");
    assert_ne!(key(item, BASE), base, "which is which");
    let mut other_version = Hasher::new(KEY_DOMAIN);
    other_version.id(&BASE).id(&item).update(&[VERSION + 1]);
    assert_ne!(other_version.finish(), base, "the version of the bytes");
    let mut same = Hasher::new(KEY_DOMAIN);
    same.id(&BASE).id(&item).update(&[VERSION]);
    assert_eq!(same.finish(), base, "the key is the domain, the base, the item and the version");
}

/// A store in memory, which refuses a second put of a key with other bytes.
#[derive(Default)]
struct Memory {
    values: Mutex<HashMap<ContentId, Arc<Aligned>>>,
}

impl ImageStore for Memory {
    fn get(&self, key: &ContentId) -> Result<Option<ImageBuf>, String> {
        Ok(self.values.lock().get(key).map(|it| Arc::clone(it) as ImageBuf))
    }

    fn put(&self, key: ContentId, value: Vec<u8>) -> Result<(), String> {
        let mut values = self.values.lock();
        match values.get(&key) {
            Some(kept) if kept[..] != value[..] => Err(format!("{key:?} is kept with other bytes")),
            Some(_) => Ok(()),
            None => {
                values.insert(key, Arc::new(Aligned::copy_from(&value)));
                Ok(())
            }
        }
    }

    fn reads_now(&self, _: &Env, _: &[u8]) -> Result<ContentId, String> {
        Err("these fixtures expand no proc macro".to_owned())
    }
}

impl Memory {
    fn keep(&self, key: ContentId, value: Allocation<'_>) {
        self.put(key, codec::file(key, &codec::encode(value).unwrap())).unwrap();
    }

    fn read(&self, key: ContentId) -> Option<Allocation<'static>> {
        let value = self.values.lock().get(&key).map(Arc::clone)?;
        Some(codec::decode(&value, key).unwrap())
    }
}

const FIXTURE: &str = r#"
//- /main.rs crate:main deps:lib new_source_root:local
const MINE: u32 = lib::SEVEN;
//- /lib.rs crate:lib new_source_root:library
pub const SEVEN: u32 = 7;
"#;

fn crate_named(db: &Db, name: &str) -> Crate {
    *base_db::all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

fn const_named(db: &Db, krate: Crate, name: &str) -> ConstId {
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for def in module.scope(db).declarations() {
            if let ModuleDefId::ConstId(it) = def
                && GeneralConstId::ConstId(it).name(db) == name
            {
                return it;
            }
        }
    }
    panic!("no constant {name}")
}

/// A database of [`FIXTURE`] keeping `lib`'s constants in `store`, filed under `base`, and
/// `lib::SEVEN`.
fn kept(store: &Arc<Memory>, base: Option<ContentId>, mode: Mode) -> (Db, Crate, ConstId) {
    let mut db = fixture::db(FIXTURE).into_db();
    let lib = crate_named(&db, "lib");
    let seven = const_named(&db, lib, "SEVEN");
    let crates = HashMap::from_iter([(lib, base)]);
    set(&mut db, Arc::clone(store) as Arc<dyn ImageStore + Send + Sync>, crates, mode);
    (db, lib, seven)
}

fn u32_value(n: u32) -> Allocation<'static> {
    scalar(default_types().types.u32, &n.to_le_bytes())
}

/// The differences of kept constant values `differences` holds.
fn of_constants(differences: Vec<String>) -> Vec<String> {
    differences.into_iter().filter(|it| it.contains(KIND)).collect()
}

#[test]
fn a_hit_allocates_only_its_result() {
    let store = Arc::new(Memory::default());
    let (db, _, seven) = kept(&store, Some(BASE), Mode::Serve);
    crate::attach_db(&db, || {
        let item = hir_def::deps::const_key(&db, seven).unwrap();
        store.keep(key(BASE, item), u32_value(8));
        let empty = GenericArgs::empty();
        assert_eq!(cached(&db, seven, empty), Ok(Some(u32_value(8))), "the store's value");
        let (served, used) = blaze_alloc_count::measure(|| cached(&db, seven, empty));
        assert_eq!(served, Ok(Some(u32_value(8))));
        assert!(used.allocations <= 1, "a hit allocates more than its memory: {used:?}");
    });
}

#[test]
fn what_is_not_kept_is_not_asked() {
    let store = Arc::new(Memory::default());
    let (db, lib, seven) = kept(&store, Some(BASE), Mode::Serve);
    crate::attach_db(&db, || {
        let mine = const_named(&db, crate_named(&db, "main"), "MINE");
        assert!(site(&db, mine, GenericArgs::empty()).is_none(), "a workspace member's");
        let args = GenericArgs::new_from_slice(&[default_types().types.u8.into()]);
        assert!(site(&db, seven, args).is_none(), "with generic arguments");
        assert_eq!(site(&db, seven, GenericArgs::empty()).map(|it| it.krate), Some(lib));
        assert_eq!(cached(&db, seven, GenericArgs::empty()), Ok(None), "not in the store");
    });
}

#[test]
fn an_unreadable_kept_value_is_an_error_naming_it() {
    let store = Arc::new(Memory::default());
    let (db, _, seven) = kept(&store, Some(BASE), Mode::Serve);
    crate::attach_db(&db, || {
        let key = key(BASE, hir_def::deps::const_key(&db, seven).unwrap());
        let body = codec::encode(u32_value(7)).unwrap();
        store.put(key, codec::file(ContentId([1; 32]), &body)).unwrap();
        let Err(ConstEvalError::MirEvalError(MirEvalError::InternalError(said))) =
            cached(&db, seven, GenericArgs::empty())
        else {
            panic!("a value filed under another key is served")
        };
        assert!(said.contains("`SEVEN`") && said.contains("another key"), "{said}");
    });
}

#[test]
fn a_recorded_value_is_drained_under_its_key() {
    let store = Arc::new(Memory::default());
    let (db, lib, seven) = kept(&store, None, Mode::Serve);
    crate::attach_db(&db, || {
        let empty = GenericArgs::empty();
        assert_eq!(cached(&db, seven, empty), Ok(None), "a crate checked now has no base yet");
        for _ in 0..3 {
            record(&db, seven, empty, &Ok(u32_value(7).store()));
        }
        let failed = Err(ConstEvalError::MirEvalError(MirEvalError::Panic("no".to_owned())));
        record(&db, seven, empty, &failed);
        let bases = HashMap::from_iter([(lib, BASE)]);
        assert_eq!(drain(&db, &bases), Ok(Drained { written: 1, unkeyed: 0 }));
        let item = hir_def::deps::const_key(&db, seven).unwrap();
        assert_eq!(store.read(key(BASE, item)), Some(u32_value(7)));
        assert_eq!(drain(&db, &bases), Ok(Drained::default()), "drained once");

        record(&db, seven, empty, &Ok(u32_value(7).store()));
        assert_eq!(drain(&db, &HashMap::default()), Ok(Drained { written: 0, unkeyed: 1 }));

        record(&db, seven, empty, &Ok(u32_value(7).store()));
        record(&db, seven, empty, &Ok(u32_value(9).store()));
        let two = drain(&db, &bases).unwrap_err();
        assert!(two.contains("`SEVEN`") && two.contains("two values"), "{two}");
    });
}

#[test]
fn check_mode_records_a_difference_and_verify_serves() {
    let store = Arc::new(Memory::default());
    let (db, _, seven) = kept(&store, Some(BASE), Mode::Check);
    crate::attach_db(&db, || {
        _ = frozen::differences();
        let key = key(BASE, hir_def::deps::const_key(&db, seven).unwrap());
        store.keep(key, u32_value(8));
        let empty = GenericArgs::empty();
        assert_eq!(cached(&db, seven, empty), Ok(None), "check mode evaluates");
        record(&db, seven, empty, &Ok(u32_value(8).store()));
        assert_eq!(of_constants(frozen::differences()), Vec::<String>::new(), "the same value");
        record(&db, seven, empty, &Ok(u32_value(7).store()));
        let found = of_constants(frozen::differences());
        assert!(found.len() == 1 && found[0].contains("`SEVEN`"), "{found:?}");
        assert_eq!(drain(&db, &HashMap::default()), Ok(Drained::default()), "nothing to keep");
    });
    let (db, _, seven) = kept(&store, Some(BASE), Mode::Verify);
    crate::attach_db(&db, || {
        assert_eq!(cached(&db, seven, GenericArgs::empty()), Ok(Some(u32_value(8))));
        assert_eq!(of_constants(frozen::differences()), Vec::<String>::new(), "it writes back");
    });
}

#[test]
fn setting_the_same_store_changes_nothing() {
    let store = Arc::new(Memory::default());
    let (mut db, lib, _) = kept(&store, Some(BASE), Mode::Serve);
    // clone: a test reading the input before and after
    let before = KeptConsts::get(&db).kept(&db).clone();
    let same = Arc::clone(&store) as Arc<dyn ImageStore + Send + Sync>;
    set(&mut db, same, HashMap::from_iter([(lib, Some(BASE))]), Mode::Serve);
    assert_eq!(*KeptConsts::get(&db).kept(&db), before, "the input is not set again");
    let other = Arc::clone(&store) as Arc<dyn ImageStore + Send + Sync>;
    set(&mut db, other, HashMap::from_iter([(lib, Some(KEY))]), Mode::Serve);
    assert_ne!(*KeptConsts::get(&db).kept(&db), before, "another base is another input");
}
