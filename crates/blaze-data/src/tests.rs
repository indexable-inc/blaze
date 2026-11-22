//! `value!` in the forms today's `json!` call sites use, typed reading's paths, and the map
//! against a model with its planted-bug control.

use core::fmt;
use std::{
    collections::BTreeMap,
    format,
    string::{String as StdString, ToString},
    vec::Vec as StdVec,
};

use blaze_own::{Alloc, String, planted::Planted};
use blaze_sim::{Broken, Config, NoPlant, Tape, controls};

use crate::{Cursor, DataPlant, FromValue, Int, Kind, Map, Shape, ShapeError, ToValue, Value};

/// The tests' allocator: blaze-own's test heap.
type Heap = Alloc<'static>;

fn heap() -> Heap {
    blaze_own::testing::heap().alloc()
}

#[cfg(feature = "std")]
#[test]
fn std_types_are_read_and_written_strictly() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::PathBuf};

    let read = |value: &Value<Heap>| Cursor::root(value, heap()).get::<StdVec<StdString>>();
    let names: StdVec<StdString> = StdVec::from(["a".to_string(), "b".to_string()]);
    let value = names.to_value(heap());
    assert_eq!(value, value!(in heap(); ["a", "b"]));
    assert_eq!(read(&value).unwrap(), names);
    let error = read(&value!(in heap(); ["a", 1])).unwrap_err();
    assert_eq!(error.to_string(), "[1]: expected text, found integer");

    let path = PathBuf::from("/tmp/x y");
    let value = path.to_value(heap());
    assert_eq!(value.as_str(), Some("/tmp/x y"));
    assert_eq!(Cursor::root(&value, heap()).get::<PathBuf>().unwrap(), path);
    let raw = PathBuf::from(OsStr::from_bytes(b"/tmp/\xff"));
    let value = raw.to_value(heap());
    assert_eq!(value.as_bytes(), Some(&b"/tmp/\xff"[..]));
    assert_eq!(Cursor::root(&value, heap()).get::<PathBuf>().unwrap(), raw);
    assert!(matches!(
        Cursor::root(&value!(in heap(); 1), heap()).get::<PathBuf>().unwrap_err().shape,
        Shape::Expected { expected: Kind::Text, found: Kind::Int }
    ));
}

#[test]
fn invalid_names_the_type_and_the_reason() {
    let value = value!(in heap(); {"v": "x"});
    let root = Cursor::root(&value, heap());
    let error = root.field("v").unwrap().invalid("version", &"no digits at byte 0");
    assert_eq!(error.shape, Shape::Invalid { ty: "version" });
    assert_eq!(error.to_string(), "v: not a version: no digits at byte 0");
}

fn text(s: &str) -> String<Heap> {
    String::from_str_in(s, heap())
}

#[test]
fn the_macro_takes_the_forms_the_tree_uses() {
    let pid = 7u32;
    let name = text("link");
    let none: Option<u64> = None;
    let key = "dynamic";
    let on = true;
    let v = value!(in heap(); {
        "ph": "X",
        "pid": pid,
        "name": name,
        "args": [1, 2.5, null, true, "x", -3, [], {}],
        "none": none,
        (key): if on { "yes" } else { "no" },
        "nested": {"a": {"b": [false, {"c": null}]}},
        "op": if pid == 0 { Value::Null } else { value!(in heap(); "text") },
    });
    assert_eq!(v.get("ph").unwrap(), &"X");
    assert_eq!(*v.get("pid").unwrap(), 7u64);
    assert_eq!(v.get("name").unwrap(), "link", "a String is read by reference, not moved");
    assert_eq!(name.as_str(), "link");
    let args = v.get("args").unwrap().as_list().unwrap();
    assert_eq!(args.len(), 8);
    assert_eq!(args[1].as_f64(), Some(2.5));
    assert!(args[2].is_null());
    assert_eq!(args[5].as_i64(), Some(-3));
    assert_eq!(args[6].as_list().map(<[_]>::len), Some(0));
    assert_eq!(args[7].as_map().map(Map::len), Some(0));
    assert!(v.get("none").unwrap().is_null());
    assert_eq!(v.get("dynamic").unwrap(), &"yes");
    assert_eq!(v.get("op").unwrap(), &"text");
    let keys: StdVec<&str> = v.as_map().unwrap().iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["ph", "pid", "name", "args", "none", "dynamic", "nested", "op"], "insertion order");
    let sorted: StdVec<&str> = v.as_map().unwrap().sorted().map(|(k, _)| k).collect();
    assert_eq!(sorted, ["args", "dynamic", "name", "nested", "none", "op", "ph", "pid"]);
    assert!(value!(in heap(); null).is_null());
    assert_eq!(value!(in heap(); [1, 2,]).as_list().map(<[_]>::len), Some(2), "a trailing comma");
    assert_eq!(value!(in heap(); {"a": 1, "a": 2}).get("a").unwrap(), &2u64, "the later key wins");
    let inner = value!(in heap(); {"k": [1]});
    let outer = value!(in heap(); {"inner": inner, "again": inner});
    assert_eq!(outer.get("again").unwrap(), &inner);
}

#[test]
fn debug_is_cbor_diagnostic_notation() {
    let mut bytes = blaze_own::Vec::new_in(heap());
    bytes.extend_from_slice(&[0, 255]);
    let mut v = value!(in heap(); {"a": [1, 2.5, null, "q\""], "z": {}});
    v.as_map_mut().unwrap().insert(text("b"), Value::Bytes(bytes));
    assert_eq!(format!("{v:?}"), r#"{"a": [1, 2.5, null, "q\""], "z": {}, "b": h'00ff'}"#);
    assert_eq!(format!("{:?}", Value::<Heap>::Float(f64::NAN)), "NaN");
}

#[test]
fn integers_are_exact_and_floats_compare_by_bits() {
    assert_eq!(Int::from_i64(5), Int::from_u64(5), "one representation per number");
    assert_eq!(Int::from_u64(u64::MAX).as_i64(), None);
    assert_eq!(Int::from_i64(i64::MIN).as_u64(), None);
    assert!(Int::from_i64(-1) < Int::from_u64(u64::MAX));
    assert_eq!(Int::from_u64(1 << 53).as_f64_exact(), Some(9007199254740992.0));
    assert_eq!(Int::from_u64((1 << 53) + 1).as_f64_exact(), None);
    assert_eq!(Value::<Heap>::Float(f64::NAN), Value::<Heap>::Float(f64::NAN));
    assert_ne!(Value::<Heap>::Float(0.0), Value::<Heap>::Float(-0.0));
    assert_ne!(Value::<Heap>::Float(1.0), value!(in heap(); 1), "a float is not an integer");
}

/// A test type read and written by hand, as reflection will.
#[derive(Debug, PartialEq)]
struct Run {
    suite: StdString,
    timeout_minutes: u32,
    ids: StdVec<StdString>,
}

impl FromValue<Heap> for Run {
    fn from_value(c: &Cursor<'_, '_, Heap>) -> Result<Run, ShapeError<Heap>> {
        c.fields(|f| {
            let suite = f.cursor("suite")?.text()?.to_string();
            let timeout_minutes = f.take_opt("timeout_minutes")?.unwrap_or(300);
            let ids: Option<blaze_own::Vec<String<Heap>, Heap>> = f.take_opt("ids")?;
            let ids = ids.map(|ids| ids.iter().map(|s| s.as_str().to_string()).collect()).unwrap_or_default();
            Ok(Run { suite, timeout_minutes, ids })
        })
    }
}

fn read(v: &Value<Heap>) -> Result<Run, StdString> {
    Cursor::root(v, heap()).get::<Run>().map_err(|e| e.to_string())
}

#[test]
fn typed_reading_names_the_path() {
    let run = read(&value!(in heap(); {"suite": "uv", "ids": ["a"]})).unwrap();
    assert_eq!(run, Run { suite: "uv".into(), timeout_minutes: 300, ids: ["a".into()].into() });
    assert_eq!(read(&value!(in heap(); [1])).unwrap_err(), "the value: expected map, found list");
    assert_eq!(read(&value!(in heap(); {})).unwrap_err(), "suite: missing");
    assert_eq!(
        read(&value!(in heap(); {"suite": "uv", "zeta": 1})).unwrap_err(),
        r#"the value: unknown field "zeta"; known: "suite", "timeout_minutes", "ids""#
    );
    assert_eq!(
        read(&value!(in heap(); {"suite": "uv", "ids": ["a", 2]})).unwrap_err(),
        "ids[1]: expected text, found integer"
    );
    assert_eq!(
        read(&value!(in heap(); {"suite": "uv", "timeout_minutes": 5000000000u64})).unwrap_err(),
        "timeout_minutes: out of range for u32"
    );
    let deep = value!(in heap(); {"a": {"b": [0, 1, 2, {"x.y": true}]}});
    let root = Cursor::root(&deep, heap());
    let a = root.field("a").unwrap();
    let b = a.field("b").unwrap();
    let three = b.index(3).unwrap();
    let error = three.field("x.y").unwrap().text().unwrap_err();
    assert_eq!(error.to_string(), r#"a.b[3]."x.y": expected text, found bool"#);
    assert_eq!(b.index(9).unwrap_err().shape, Shape::NoIndex { len: 4 });
    assert_eq!(root.field("a").unwrap().expected(Kind::Text).shape, Shape::Expected { expected: Kind::Text, found: Kind::Map });
}

/// A plain struct through `fields!`: both fields optional, so only the unknown-field check
/// speaks.
struct Paint<A: core::alloc::Allocator> {
    color: Option<String<A>>,
    size: Option<u32>,
}
fields!(for<A> Paint<A> { ; color, size });

/// One required field, for the missing-but-near case.
struct Pen<A: core::alloc::Allocator> {
    color: String<A>,
}
fields!(for<A> Pen<A> { color });

fn at_a_b<T: FromValue<Heap>>(v: &Value<Heap>) -> Result<T, StdString> {
    let root = Cursor::root(v, heap());
    let a = root.field("a").map_err(|e| e.to_string())?;
    let b = a.field("b").map_err(|e| e.to_string())?;
    b.get::<T>().map_err(|e| e.to_string())
}

#[test]
fn reading_is_strict_by_default() {
    let paint = at_a_b::<Paint<Heap>>(&value!(in heap(); {"a": {"b": {"color": "red", "size": 3}}})).unwrap();
    assert_eq!((paint.color.as_deref(), paint.size), (Some("red"), Some(3)));
    assert_eq!(
        at_a_b::<Paint<Heap>>(&value!(in heap(); {"a": {"b": {"colour": "red", "size": 3}}})).err().unwrap(),
        r#"a.b: unknown field "colour" (did you mean "color"?); known: "color", "size""#,
        "the exact text: every extra key, the nearest known name, the known names"
    );
    assert_eq!(
        at_a_b::<Paint<Heap>>(&value!(in heap(); {"a": {"b": {"colour": 1, "zzz": 2}}})).err().unwrap(),
        r#"a.b: unknown fields "colour" (did you mean "color"?), "zzz"; known: "color", "size""#
    );
    assert_eq!(
        at_a_b::<Pen<Heap>>(&value!(in heap(); {"a": {"b": {"colour": "red"}}})).err().unwrap(),
        r#"a.b.color: missing (found unknown field "colour")"#
    );
    let written = Paint::<Heap> { color: None, size: Some(2) }.to_value(heap());
    assert_eq!(written, value!(in heap(); {"size": 2}), "`None` is left out");

    // The opt-out is named at the call and hands the extra keys back.
    let v = value!(in heap(); {"color": "red", "extra": 1});
    let (color, extra) = Cursor::root(&v, heap())
        .fields_allowing_extra(|f| f.take::<String<Heap>>("color"))
        .unwrap();
    assert_eq!((color.as_str(), extra.keys.as_slice()), ("red", ["extra"].as_slice()));

    // No coercion between kinds; an integer outside its type is refused.
    let one = value!(in heap(); 1);
    let get = |v: &Value<Heap>| Cursor::root(v, heap()).get::<bool>().map_err(|e| e.to_string());
    assert_eq!(get(&one).unwrap_err(), "the value: expected bool, found integer", "`1` is no `true`");
    let text_one = value!(in heap(); "1");
    assert_eq!(
        Cursor::root(&text_one, heap()).get::<u32>().unwrap_err().to_string(),
        "the value: expected integer, found text",
        "a number in a string is text"
    );
    let float_one = value!(in heap(); 1.0);
    assert_eq!(
        Cursor::root(&float_one, heap()).get::<i64>().unwrap_err().to_string(),
        "the value: expected integer, found float"
    );
    assert_eq!(
        Cursor::root(&one, heap()).get::<f64>().unwrap_err().to_string(),
        "the value: expected float, found integer"
    );
    assert_eq!(Cursor::root(&one, heap()).f64_or_int().unwrap(), 1.0, "only when asked by name");
    let big = value!(in heap(); 300);
    assert_eq!(Cursor::root(&big, heap()).get::<u8>().unwrap_err().to_string(), "the value: out of range for u8");

    // An enum's unknown name lists the known ones.
    let blue = value!(in heap(); "Blue");
    assert_eq!(
        Cursor::root(&blue, heap()).one_of(&["red", "blue"]).unwrap_err().to_string(),
        r#"the value: unknown variant "Blue" (did you mean "blue"?); known: "red", "blue""#
    );
    let red = value!(in heap(); "red");
    assert_eq!(Cursor::root(&red, heap()).one_of(&["red", "blue"]).unwrap(), 0);
}

/// `body` on a thread with a 64 KiB stack, where a recursion on depth overflows long before a
/// million levels.
fn small_stack(body: impl FnOnce() + Send + 'static) {
    let thread = std::thread::Builder::new().stack_size(64 << 10).spawn(body).unwrap();
    thread.join().unwrap();
}

/// A value `n` deep, lists and maps by turns, built without recursion.
fn deep(n: usize) -> Value<Heap> {
    let mut v = Value::Null;
    for i in 0..n {
        v = match i % 2 {
            0 => Value::List(blaze_own::Vec::from_array_in([v], heap())),
            _ => {
                let mut map = Map::new_in(heap());
                map.insert(text("k"), v);
                Value::Map(map)
            }
        };
    }
    v
}

/// The most containers open at once, walked.
fn depth_of(v: &Value<Heap>) -> usize {
    let mut walk = crate::Walk::new_in(v, crate::Order::Sorted, heap());
    let mut most = 0;
    while walk.next().is_some() {
        most = most.max(walk.depth());
    }
    most
}

#[test]
fn deep_values_copy_and_drop_in_a_small_stack() {
    for n in [128, 1_000_000] {
        small_stack(move || {
            let v = deep(n);
            let copy = v.copy_in(heap());
            assert_eq!(depth_of(&copy), n);
            drop(v);
            drop(copy);
        });
    }
}

#[test]
fn to_value_covers_the_standard_shapes() {
    let list = [1u8, 2, 3];
    assert_eq!(list.to_value(heap()), value!(in heap(); [1, 2, 3]));
    assert_eq!('é'.to_value(heap()), value!(in heap(); "é"));
    assert_eq!(Some(1i64).to_value(heap()), value!(in heap(); 1));
    assert_eq!(None::<i64>.to_value(heap()), Value::<Heap>::Null);
    assert_eq!(i64::MIN.to_value(heap()).as_i64(), Some(i64::MIN));
}

/// How the map parted from its model.
#[derive(Clone, PartialEq, Eq, Debug)]
enum MapBroken {
    /// A remove handed back another value than the model's.
    RemoveDiffers { key: &'static str, got: Option<u64>, want: Option<u64> },
    /// The sorted view is not the model's entries.
    SortedDiffers { sorted: StdVec<(StdString, u64)>, want: StdVec<(StdString, u64)> },
    /// Insertion order is not the order the model first saw the keys in.
    OrderDiffers { order: StdVec<StdString>, want: StdVec<StdString> },
    /// A lookup is not the model's.
    GetDiffers { key: &'static str },
}

impl fmt::Display for MapBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MapBroken::RemoveDiffers { key, got, want } => {
                write!(f, "remove {key:?} gave {got:?}, unlike the model's {want:?}")
            }
            MapBroken::SortedDiffers { sorted, want } => {
                write!(f, "the sorted view {sorted:?} differs from the model {want:?}")
            }
            MapBroken::OrderDiffers { order, want } => {
                write!(f, "insertion order {order:?} differs from the model {want:?}")
            }
            MapBroken::GetDiffers { key } => write!(f, "get {key:?} differs from the model"),
        }
    }
}

/// The map against a `BTreeMap` model under inserts and removes, in both orders; every insert
/// takes `plant` ([`NoPlant`] for the clean run).
fn map_agrees(tape: &Tape, plant: impl Planted<DataPlant>) -> Result<(), Broken<MapBroken>> {
    let keys = ["a", "b", "c", "d", "aa", ""];
    let mut map = Map::new_in(heap());
    let mut model: BTreeMap<&str, u64> = BTreeMap::new();
    let mut first_seen: StdVec<&str> = StdVec::new();
    let ops = tape.vec(0..=24, "ops", |t| (t.coin("remove"), *t.one_of(&keys, "key"), t.range(0..100, "n")));
    for (remove, key, n) in ops {
        if remove {
            let got = map.remove(key).and_then(|v| v.as_u64());
            let want = model.remove(key);
            if got != want {
                return Err(Broken::Oracle(MapBroken::RemoveDiffers { key, got, want }));
            }
            first_seen.retain(|k| *k != key);
        } else {
            map.insert_planted(text(key), Value::Int(Int::from_u64(n)), plant);
            if model.insert(key, n).is_none() {
                first_seen.push(key);
            }
        }
    }
    let sorted: StdVec<(StdString, u64)> =
        map.sorted().map(|(k, v)| (k.to_string(), v.as_u64().unwrap_or(u64::MAX))).collect();
    let want: StdVec<(StdString, u64)> = model.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    if sorted != want {
        return Err(Broken::Oracle(MapBroken::SortedDiffers { sorted, want }));
    }
    let order: StdVec<StdString> = map.iter().map(|(k, _)| k.to_string()).collect();
    let want: StdVec<StdString> = first_seen.iter().map(|k| k.to_string()).collect();
    if order != want {
        return Err(Broken::Oracle(MapBroken::OrderDiffers { order, want }));
    }
    for key in keys {
        if map.get(key).and_then(Value::as_u64) != model.get(key).copied() {
            return Err(Broken::Oracle(MapBroken::GetDiffers { key }));
        }
    }
    Ok(())
}

blaze_sim::plants! {
    /// The bugs planted in the map, each with the invariant of the model oracle that must catch
    /// it.
    enum MapControl breaks MapBroken {
        /// The map as it is.
        None,
        /// A present key inserted a second time: the sorted view has the key twice, or a remove
        /// finds the stale copy first.
        DuplicateOnHit => MapBroken::SortedDiffers { .. } | MapBroken::RemoveDiffers { .. },
    }
}

impl MapControl {
    /// The map's plant this control plants.
    fn planted(self) -> DataPlant {
        match self {
            MapControl::None => DataPlant::None,
            MapControl::DuplicateOnHit => DataPlant::DuplicateOnHit,
        }
    }
}

#[test]
fn the_map_agrees_with_a_model() {
    blaze_sim::check(Config::cases(500), |tape| map_agrees(tape, NoPlant));
}

#[test]
fn every_map_plant_is_caught_by_its_invariant() {
    controls(Config::cases(500), |tape, plant: MapControl| map_agrees(tape, plant.planted()));
}
