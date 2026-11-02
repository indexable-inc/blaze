//! The bytes of a kept constant value: its type with no database id, its memory and its memory
//! map, little-endian, read where the store keeps them.
//!
//! ```text
//! value  := VERSION:u8 key:[u8; 32] body
//! body   := ty memory:(len:u32 bytes) map
//! ty     := 0 bool | 1 char | 2 str | 3 never | 4..=9 int | 10..=15 uint | 16..=19 float
//!         | 20 array ty size:u8 data:u128 | 21 slice ty | 22 tuple n:u32 ty^n
//!         | 23 ref region:u8 mutability:u8 ty | 24 raw-ptr mutability:u8 ty
//! map    := 0 | 1 len:u32 bytes | 2 n:u32 (address:u64 len:u32 bytes)^n m:u32 (address:u64 align:u64)^m
//!           v:u32 (offset:u64)^v p:u32 (address:u64)^p
//! ```
//!
//! The last two columns of a complex map are where its pointers with provenance are, sorted.
//!
//! A value with a part this cannot write (a type naming a definition, another region, a map
//! holding a static, a vtable or a `TypeId`) is not written at all: [`encode`] answers `None`
//! and the constant is evaluated every time, never kept in part.

use blaze_format::cursor::{Put, Reader};
use blaze_ids::ContentId;
use rustc_abi::Size;
use rustc_type_ir::{FloatTy, IntTy, Mutability, UintTy, inherent::IntoKind as _};
use smallvec::SmallVec;

use crate::{
    ComplexMemoryMap, Extent, MemoryMap, MemoryPieces,
    next_solver::{
        Allocation, AllocationData, Const, ConstExt as _, ConstKind, Region, RegionKind, ScalarInt,
        Ty, TyKind, Tys, ValTree, ValTreeKind, ValueConst, default_types,
    },
};

/// The version of what this writes: the first byte of a kept value and part of its key.
pub const VERSION: u8 = 2;

/// The header of a kept value: [`VERSION`] and the key it is filed under.
pub(super) const HEADER: usize = 1 + 32;

/// How deep a type may nest: a kept value's type is the declared type of a constant, a few
/// levels deep; the bound keeps a damaged value from recursing without end.
const MAX_DEPTH: u32 = 64;

type R<T> = Result<T, &'static str>;

fn err(error: blaze_format::cursor::Error) -> &'static str {
    error.as_str()
}

/// The body of `value`, or `None` when a part of it cannot be written portably.
pub(crate) fn encode(value: Allocation<'_>) -> Option<Vec<u8>> {
    let AllocationData { ty, memory, memory_map } = &*value;
    // alloc: one kept value's bytes, written on a miss only
    let mut out = Vec::with_capacity(16 + memory.len() + map_len(memory_map));
    put_ty(&mut out, *ty, 0)?;
    put_bytes(&mut out, memory)?;
    put_map(&mut out, memory_map)?;
    Some(out)
}

/// The value kept under `key`: [`VERSION`], the key, then `body` ([`encode`]'s).
pub(crate) fn file(key: ContentId, body: &[u8]) -> Vec<u8> {
    // alloc: one value put to the store, at drain
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.put_u8(VERSION);
    out.put_bytes(&key.0);
    out.put_bytes(body);
    out
}

/// The body of `bytes`, a value kept under `key`: its version and key checked.
pub(crate) fn body(bytes: &[u8], key: ContentId) -> R<&[u8]> {
    let mut at = Reader::new(bytes);
    if at.u8().map_err(err)? != VERSION {
        return Err("a value of another version");
    }
    if *at.array::<32>().map_err(err)? != key.0 {
        return Err("a value filed under another key");
    }
    Ok(at.take_rest())
}

/// The allocation `bytes`, a value kept under `key`, holds.
pub(crate) fn decode<'db>(bytes: &[u8], key: ContentId) -> R<Allocation<'db>> {
    decode_body(body(bytes, key)?)
}

/// The allocation `body` ([`encode`]'s) holds, every length checked against the bytes.
pub(crate) fn decode_body<'db>(body: &[u8]) -> R<Allocation<'db>> {
    let mut at = Reader::new(body);
    let ty = get_ty(&mut at, 0)?;
    let memory = Box::from(at.bytes_u32().map_err(err)?);
    let memory_map = get_map(&mut at)?;
    at.finish().map_err(|_| "bytes after the value")?;
    Ok(Allocation::new(AllocationData { ty, memory, memory_map }))
}

fn map_len(map: &MemoryMap<'_>) -> usize {
    match map {
        MemoryMap::Empty => 1,
        MemoryMap::Simple(bytes) => 5 + bytes.len(),
        MemoryMap::Complex(map) => {
            let pieces = map.memory.iter().map(|(_, it)| 12 + it.len()).sum::<usize>();
            let pointers = map.value_pointers.len() + map.memory_pointers.len();
            17 + pieces + 16 * map.aligns.len() + 8 * pointers
        }
    }
}

fn put_len(out: &mut Vec<u8>, len: usize) -> Option<()> {
    out.put_u32(u32::try_from(len).ok()?);
    Some(())
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Option<()> {
    put_len(out, bytes.len())?;
    out.put_bytes(bytes);
    Some(())
}

const INTS: [IntTy; 6] = [IntTy::Isize, IntTy::I8, IntTy::I16, IntTy::I32, IntTy::I64, IntTy::I128];
const UINTS: [UintTy; 6] =
    [UintTy::Usize, UintTy::U8, UintTy::U16, UintTy::U32, UintTy::U64, UintTy::U128];
const FLOATS: [FloatTy; 4] = [FloatTy::F16, FloatTy::F32, FloatTy::F64, FloatTy::F128];

fn mutability(it: Mutability) -> u8 {
    match it {
        Mutability::Not => 0,
        Mutability::Mut => 1,
    }
}

fn put_ty(out: &mut Vec<u8>, ty: Ty<'_>, depth: u32) -> Option<()> {
    if depth > MAX_DEPTH {
        return None;
    }
    match ty.kind() {
        TyKind::Bool => out.put_u8(0),
        TyKind::Char => out.put_u8(1),
        TyKind::Str => out.put_u8(2),
        TyKind::Never => out.put_u8(3),
        TyKind::Int(it) => out.put_u8(4 + INTS.iter().position(|&int| int == it)? as u8),
        TyKind::Uint(it) => out.put_u8(10 + UINTS.iter().position(|&uint| uint == it)? as u8),
        TyKind::Float(it) => out.put_u8(16 + FLOATS.iter().position(|&float| float == it)? as u8),
        TyKind::Array(element, len) => {
            out.put_u8(20);
            put_ty(out, element, depth + 1)?;
            put_array_len(out, len)?;
        }
        TyKind::Slice(element) => {
            out.put_u8(21);
            put_ty(out, element, depth + 1)?;
        }
        TyKind::Tuple(tys) => {
            out.put_u8(22);
            put_len(out, tys.len())?;
            for ty in tys {
                put_ty(out, ty, depth + 1)?;
            }
        }
        TyKind::Ref(region, pointee, mutbl) => {
            out.put_u8(23);
            out.put_u8(match region.kind() {
                RegionKind::ReStatic => 0,
                RegionKind::ReErased => 1,
                _ => return None,
            });
            out.put_u8(mutability(mutbl));
            put_ty(out, pointee, depth + 1)?;
        }
        TyKind::RawPtr(pointee, mutbl) => {
            out.put_u8(24);
            out.put_u8(mutability(mutbl));
            put_ty(out, pointee, depth + 1)?;
        }
        _ => return None,
    }
    Some(())
}

/// An array's length: a `usize` value, as its scalar's size and bits.
fn put_array_len(out: &mut Vec<u8>, len: Const<'_>) -> Option<()> {
    let ConstKind::Value(value) = len.kind() else { return None };
    if value.ty != default_types().types.usize {
        return None;
    }
    let &ValTreeKind::Leaf(scalar) = value.value.inner() else { return None };
    out.put_u8(u8::try_from(scalar.size().bytes()).ok()?);
    out.put_u128(scalar.to_bits_unchecked());
    Some(())
}

fn put_map(out: &mut Vec<u8>, map: &MemoryMap<'_>) -> Option<()> {
    match map {
        MemoryMap::Empty => out.put_u8(0),
        MemoryMap::Simple(bytes) => {
            out.put_u8(1);
            put_bytes(out, bytes)?;
        }
        MemoryMap::Complex(map) => {
            let ComplexMemoryMap {
                memory,
                vtable,
                statics,
                aligns,
                type_ids,
                value_pointers,
                memory_pointers,
            } = &**map;
            if !statics.is_empty() || vtable.tys().len() != 0 || !type_ids.is_empty() {
                return None;
            }
            out.put_u8(2);
            put_len(out, memory.len())?;
            for (&address, bytes) in memory.iter() {
                out.put_u64(u64::try_from(address).ok()?);
                put_bytes(out, bytes)?;
            }
            put_len(out, aligns.len())?;
            for &(address, align) in aligns {
                out.put_u64(u64::try_from(address).ok()?);
                out.put_u64(u64::try_from(align).ok()?);
            }
            for column in [value_pointers, memory_pointers] {
                put_len(out, column.len())?;
                for &at in column {
                    out.put_u64(u64::try_from(at).ok()?);
                }
            }
        }
    }
    Some(())
}

fn get_mutability(at: &mut Reader<'_>) -> R<Mutability> {
    match at.u8().map_err(err)? {
        0 => Ok(Mutability::Not),
        1 => Ok(Mutability::Mut),
        _ => Err("a mutability of no kind"),
    }
}

fn get_ty<'db>(at: &mut Reader<'_>, depth: u32) -> R<Ty<'db>> {
    if depth > MAX_DEPTH {
        return Err("a type nested deeper than any kept");
    }
    let types = &default_types().types;
    // The kept types name no definition: they are the same in every crate, and need none.
    let ty = |kind| Ty::new_without_interner(kind);
    let tag = at.u8().map_err(err)?;
    Ok(match tag {
        0 => types.bool,
        1 => types.char,
        2 => types.str,
        3 => types.never,
        4..=9 => ty(TyKind::Int(INTS[usize::from(tag - 4)])),
        10..=15 => ty(TyKind::Uint(UINTS[usize::from(tag - 10)])),
        16..=19 => ty(TyKind::Float(FLOATS[usize::from(tag - 16)])),
        20 => {
            let element = get_ty(at, depth + 1)?;
            ty(TyKind::Array(element, get_array_len(at)?))
        }
        21 => ty(TyKind::Slice(get_ty(at, depth + 1)?)),
        22 => {
            let count = at.count_u32(1).map_err(err)?;
            let mut tys: SmallVec<[Ty<'db>; 8]> = SmallVec::with_capacity(count);
            for _ in 0..count {
                tys.push(get_ty(at, depth + 1)?);
            }
            ty(TyKind::Tuple(Tys::new_from_slice(&tys)))
        }
        23 => {
            let regions = &default_types().regions;
            let region: Region<'db> = match at.u8().map_err(err)? {
                0 => regions.statik,
                1 => regions.erased,
                _ => return Err("a region of no kind"),
            };
            let mutbl = get_mutability(at)?;
            ty(TyKind::Ref(region, get_ty(at, depth + 1)?, mutbl))
        }
        24 => {
            let mutbl = get_mutability(at)?;
            ty(TyKind::RawPtr(get_ty(at, depth + 1)?, mutbl))
        }
        _ => return Err("a type of no kind"),
    })
}

fn get_array_len<'db>(at: &mut Reader<'_>) -> R<Const<'db>> {
    let size = at.u8().map_err(err)?;
    let data = at.u128().map_err(err)?;
    if !(1..=16).contains(&size) {
        return Err("an array length of no size");
    }
    let scalar = ScalarInt::try_from_uint(data, Size::from_bytes(size))
        .ok_or("an array length wider than its size")?;
    let value = ValueConst::new(default_types().types.usize, ValTree::new(ValTreeKind::Leaf(scalar)));
    Ok(Const::new_without_interner(ConstKind::Value(value)))
}

fn get_map<'db>(at: &mut Reader<'_>) -> R<MemoryMap<'db>> {
    match at.u8().map_err(err)? {
        0 => Ok(MemoryMap::Empty),
        1 => Ok(MemoryMap::Simple(Box::from(at.bytes_u32().map_err(err)?))),
        2 => {
            let count = at.count_u32(12).map_err(err)?;
            // alloc: the map of the value served, part of the result
            let mut bytes = Vec::new();
            // alloc: as above
            let mut addresses = Vec::with_capacity(count);
            // alloc: as above
            let mut extents = Vec::with_capacity(count);
            for _ in 0..count {
                let address = word(at.u64().map_err(err)?)?;
                if addresses.last().is_some_and(|&last| last >= address) {
                    return Err("pieces of memory out of the order of their addresses");
                }
                let piece = at.bytes_u32().map_err(err)?;
                extents.push(Extent { start: bytes.len(), len: piece.len() });
                bytes.extend_from_slice(piece);
                addresses.push(address);
            }
            let count = at.count_u32(16).map_err(err)?;
            // alloc: as above
            let mut aligns: Vec<(usize, usize)> = Vec::with_capacity(count);
            for _ in 0..count {
                let address = word(at.u64().map_err(err)?)?;
                if aligns.last().is_some_and(|&(last, _)| last >= address) {
                    return Err("alignments out of the order of their addresses");
                }
                aligns.push((address, word(at.u64().map_err(err)?)?));
            }
            let mut map = ComplexMemoryMap {
                memory: MemoryPieces::from_columns(bytes, addresses, extents),
                aligns,
                ..ComplexMemoryMap::default()
            };
            for column in [&mut map.value_pointers, &mut map.memory_pointers] {
                let count = at.count_u32(8).map_err(err)?;
                // alloc: as above
                column.reserve_exact(count);
                for _ in 0..count {
                    let place = word(at.u64().map_err(err)?)?;
                    if column.last().is_some_and(|&last| last >= place) {
                        return Err("pointers with provenance out of the order of their places");
                    }
                    column.push(place);
                }
            }
            // alloc: as above
            Ok(MemoryMap::Complex(Box::new(map)))
        }
        _ => Err("a memory map of no kind"),
    }
}

fn word(value: u64) -> R<usize> {
    usize::try_from(value).map_err(|_| "an address past usize")
}
