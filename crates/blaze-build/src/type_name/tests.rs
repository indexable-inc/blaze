//! The printer of `type_name` (type_name.rs), against the forms the pinned core's any.rs
//! documents ("Returns the name of a type as a string slice") and the module doc lists.

use blaze_typeck::{FnPtrHeader, IntTy, ItemKey, List, Mutbl, Name, PNode, PortableTys, UintTy};

use super::{TypeNameError, TypeNames, type_name_text};

const OPTION: ItemKey = ItemKey(0x11);
const DEBUG: ItemKey = ItemKey(0x22);
const SEND: ItemKey = ItemKey(0x33);
const MAIN: ItemKey = ItemKey(0x44);
const UNNAMED: ItemKey = ItemKey(0x55);
const ITERATOR: ItemKey = ItemKey(0x66);
const ITEM: Name = Name(0x77);

struct Paths;

impl TypeNames for Paths {
    fn item_path(&self, item: ItemKey, out: &mut dyn FnMut(&str)) -> bool {
        let p = match item {
            OPTION => "core::option::Option",
            DEBUG => "core::fmt::Debug",
            SEND => "core::marker::Send",
            MAIN => "app::main",
            ITERATOR => "core::iter::traits::iterator::Iterator",
            _ => return false,
        };
        out(p);
        true
    }

    fn name_text(&self, name: Name, out: &mut dyn FnMut(&str)) -> bool {
        if name == ITEM {
            out("Item");
            return true;
        }
        false
    }
}

/// Each line: the ground type built, then the text it prints. Expected:
/// `(u8, [u16; 2])`, `core::option::Option<i32>`, `&str`, `&mut [u8]`, `*const ()`, `(u8,)`,
/// `fn(u8) -> u8`, `unsafe extern "C" fn()`, `dyn core::fmt::Debug + core::marker::Send`,
/// `dyn core::iter::traits::iterator::Iterator<Item = u8>`, `app::main::{{closure}}`, `!`,
/// `[i8; 3]`; an item with no path refused naming it.
#[test]
fn type_names_print_full_paths_and_arguments() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let mut g = PortableTys::new_in(alloc);
    let mut t = |n: PNode| g.intern(n).unwrap();
    let u8_ = t(PNode::Uint(UintTy::U8));
    let u16_ = t(PNode::Uint(UintTy::U16));
    let i8_ = t(PNode::Int(IntTy::I8));
    let i32_ = t(PNode::Int(IntTy::I32));
    let str_ = t(PNode::Str);
    let never = t(PNode::Never);
    let _ = &mut t;
    let two = g.usize_value(2).unwrap();
    let three = g.usize_value(3).unwrap();
    let arr = g.intern(PNode::Array(u16_, two)).unwrap();
    let pair_l = g.intern_list(&[u8_, arr]).unwrap();
    let pair = g.intern(PNode::Tuple(pair_l)).unwrap();
    let i32_l = g.intern_list(&[i32_]).unwrap();
    let option = g.intern(PNode::Adt(OPTION, i32_l)).unwrap();
    let ref_str = g.intern(PNode::Ref(Mutbl::Not, str_)).unwrap();
    let slice = g.intern(PNode::Slice(u8_)).unwrap();
    let mut_slice = g.intern(PNode::Ref(Mutbl::Mut, slice)).unwrap();
    let unit = g.intern(PNode::Tuple(List::EMPTY)).unwrap();
    let ptr_unit = g.intern(PNode::Ptr(Mutbl::Not, unit)).unwrap();
    let one_l = g.intern_list(&[u8_]).unwrap();
    let one = g.intern(PNode::Tuple(one_l)).unwrap();
    let f_l = g.intern_list(&[u8_, u8_]).unwrap();
    let fn_ptr = g.intern(PNode::FnPtr(f_l, FnPtrHeader::RUST)).unwrap();
    let unit_l = g.intern_list(&[unit]).unwrap();
    let c_unsafe = FnPtrHeader::from_bits(0x81).unwrap();
    let c_fn = g.intern(PNode::FnPtr(unit_l, c_unsafe)).unwrap();
    let debug = g.intern(PNode::ExTrait(DEBUG, List::EMPTY)).unwrap();
    let send = g.intern(PNode::ExAuto(SEND)).unwrap();
    let dyn_debug = g.dynamic(&[debug, send]).unwrap();
    let iter = g.intern(PNode::ExTrait(ITERATOR, List::EMPTY)).unwrap();
    let item = g.intern(PNode::ExProj { trait_: ITERATOR, name: ITEM, args: List::EMPTY, term: u8_ }).unwrap();
    let dyn_iter = g.dynamic(&[iter, item]).unwrap();
    let closure = g.intern(PNode::Closure { owner: MAIN, ordinal: 0, args: List::EMPTY }).unwrap();
    let arr3 = g.intern(PNode::Array(i8_, three)).unwrap();
    let unnamed = g.intern(PNode::Adt(UNNAMED, List::EMPTY)).unwrap();
    let cases = [
        (pair, "(u8, [u16; 2])"),
        (option, "core::option::Option<i32>"),
        (ref_str, "&str"),
        (mut_slice, "&mut [u8]"),
        (ptr_unit, "*const ()"),
        (one, "(u8,)"),
        (fn_ptr, "fn(u8) -> u8"),
        (c_fn, "unsafe extern \"C\" fn()"),
        (dyn_debug, "dyn core::fmt::Debug + core::marker::Send"),
        (dyn_iter, "dyn core::iter::traits::iterator::Iterator<Item = u8>"),
        (closure, "app::main::{{closure}}"),
        (never, "!"),
        (arr3, "[i8; 3]"),
    ];
    let view = g.view();
    for (ty, want) in cases {
        let got = type_name_text(view, ty, &Paths, alloc).unwrap();
        assert_eq!(got.as_str(), want);
    }
    assert_eq!(type_name_text(view, unnamed, &Paths, alloc).unwrap_err(), TypeNameError::NoPath { item: UNNAMED });
    println!("type names: 13 printed, 1 refused");
}
