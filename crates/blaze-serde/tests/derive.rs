//! Each attribute and shape blaze-serde-derive takes, written and read back through serde_json
//! and through the daemon's CBOR (`blaze_protocol::cbor`), which says it is not human-readable
//! and sorts map keys, so a derive that leans on either shows here. The JSON text is pinned
//! too: it is serde's own derive's text for the same type, so a value written by a crate before
//! the port reads after it.

use std::{borrow::Cow, collections::BTreeMap, fmt::Debug, marker::PhantomData};

use blaze_serde::{
    Deserialize, Serialize,
    serde::{Deserializer, Serializer},
};

/// `value` written as `json`, and `json` and `value`'s CBOR both read back as `value`.
fn round_trip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + Debug>(value: &T, json: &str) {
    assert_eq!(serde_json::to_string(value).unwrap(), json, "serde_json wrote");
    assert_eq!(&serde_json::from_str::<T>(json).unwrap(), value, "serde_json read back");
    let cbor = blaze_protocol::cbor::to_vec(value).unwrap();
    assert_eq!(&blaze_protocol::cbor::from_slice::<T>(&cbor).unwrap(), value, "CBOR read back");
}

fn json_error<T: for<'de> Deserialize<'de> + Debug>(json: &str) -> String {
    serde_json::from_str::<T>(json).unwrap_err().to_string()
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Plain {
    a: u32,
    b: String,
    c: Vec<i64>,
    d: Option<bool>,
}

/// A missing `Option` field reads as `None` without `default`, as serde's derive reads it.
#[test]
fn a_named_struct_round_trips_and_a_missing_option_is_none() {
    let value = Plain { a: 1, b: "two".into(), c: vec![-3, 4], d: Some(true) };
    round_trip(&value, r#"{"a":1,"b":"two","c":[-3,4],"d":true}"#);
    let read: Plain = serde_json::from_str(r#"{"a":1,"b":"","c":[]}"#).unwrap();
    assert_eq!(read.d, None);
}

#[test]
fn a_missing_field_is_an_error_naming_it() {
    let said = json_error::<Plain>(r#"{"a":1,"c":[]}"#);
    assert!(said.starts_with("missing field `b`"), "{said}");
}

#[test]
fn a_field_twice_is_an_error_naming_it() {
    let said = json_error::<Plain>(r#"{"a":1,"a":2,"b":"","c":[]}"#);
    assert!(said.starts_with("duplicate field `a`"), "{said}");
}

#[test]
fn an_unknown_field_is_ignored_by_default() {
    let read: Plain = serde_json::from_str(r#"{"a":1,"x":{"y":[1]},"b":"","c":[]}"#).unwrap();
    assert_eq!(read.a, 1);
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
struct Strict {
    a: u32,
    b: u32,
}

/// serde's text, which serde_json and the daemon's CBOR both pass through.
#[test]
fn deny_unknown_fields_names_the_field_and_the_fields_known() {
    round_trip(&Strict { a: 1, b: 2 }, r#"{"a":1,"b":2}"#);
    let said = json_error::<Strict>(r#"{"a":1,"extra":3,"b":2}"#);
    assert!(said.starts_with("unknown field `extra`, expected `a` or `b`"), "{said}");
    let mut map = BTreeMap::new();
    map.insert("a", 1);
    map.insert("b", 2);
    map.insert("c", 3);
    let cbor = blaze_protocol::cbor::to_vec(&map).unwrap();
    let said = blaze_protocol::cbor::from_slice::<Strict>(&cbor).unwrap_err().to_string();
    assert!(said.contains("unknown field `c`, expected `a` or `b`"), "{said}");
}

fn seven() -> u32 {
    7
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Fields {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    count: u32,
    #[serde(default = "seven")]
    seven: u32,
    #[serde(skip)]
    cache: Vec<u8>,
    #[serde(skip_serializing_if = "is_zero", default)]
    sparse: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    maybe: Option<String>,
}

#[test]
fn field_attributes_rename_default_skip_and_skip_serializing_if() {
    let full = Fields {
        kind: "k".into(),
        count: 2,
        seven: 8,
        cache: Vec::new(),
        sparse: 3,
        maybe: Some("m".into()),
    };
    round_trip(&full, r#"{"type":"k","count":2,"seven":8,"sparse":3,"maybe":"m"}"#);
    let sparse =
        Fields { kind: "k".into(), count: 0, seven: 7, cache: Vec::new(), sparse: 0, maybe: None };
    round_trip(&sparse, r#"{"type":"k","count":0,"seven":7}"#);
    let read: Fields = serde_json::from_str(r#"{"type":"k","cache":[1,2]}"#).unwrap();
    assert_eq!(read, sparse, "a missing field takes its default; a skipped one is never read");
    let written = Fields { cache: vec![1], ..sparse };
    assert!(!serde_json::to_string(&written).unwrap().contains("cache"));
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(default)]
struct Defaulted {
    a: u32,
    b: String,
}

impl Default for Defaulted {
    fn default() -> Self {
        Defaulted { a: 5, b: "five".into() }
    }
}

fn page() -> Paged {
    Paged { size: 4096, count: 1 }
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(default = "page")]
struct Paged {
    size: u32,
    count: u32,
}

/// A missing field takes the container default's value for it, not its type's.
#[test]
fn a_container_default_fills_each_missing_field() {
    round_trip(&Defaulted { a: 1, b: "b".into() }, r#"{"a":1,"b":"b"}"#);
    assert_eq!(
        serde_json::from_str::<Defaulted>(r#"{"a":1}"#).unwrap(),
        Defaulted { a: 1, b: "five".into() }
    );
    assert_eq!(serde_json::from_str::<Defaulted>("{}").unwrap(), Defaulted::default());
    assert_eq!(
        serde_json::from_str::<Paged>(r#"{"count":3}"#).unwrap(),
        Paged { size: 4096, count: 3 }
    );
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "kebab-case")]
struct Kebab {
    first_field: u8,
    r#second_one: u8,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
struct Camel {
    first_field: u8,
    #[serde(rename = "kept")]
    renamed_field: u8,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum Lower {
    FirstVariant,
    Second,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
enum Snake {
    FirstVariant,
    HTTPServer,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "kebab-case")]
enum KebabEnum {
    FirstVariant { inner_field: u8 },
}

/// serde's case rules: on a struct they rename fields, on an enum variants (not their fields);
/// a field's own `rename` wins; a raw identifier loses its `r#`.
#[test]
fn rename_all_renames_as_serde_does() {
    round_trip(&Kebab { first_field: 1, second_one: 2 }, r#"{"first-field":1,"second-one":2}"#);
    round_trip(&Camel { first_field: 1, renamed_field: 2 }, r#"{"firstField":1,"kept":2}"#);
    round_trip(&Lower::FirstVariant, r#""firstvariant""#);
    round_trip(&Lower::Second, r#""second""#);
    round_trip(&Snake::FirstVariant, r#""first_variant""#);
    round_trip(&Snake::HTTPServer, r#""h_t_t_p_server""#);
    round_trip(
        &KebabEnum::FirstVariant { inner_field: 1 },
        r#"{"first-variant":{"inner_field":1}}"#,
    );
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Unit;

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Newtype(u64);

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Tuple(u8, String, #[serde(skip)] u8);

#[test]
fn unit_newtype_and_tuple_structs_round_trip() {
    round_trip(&Unit, "null");
    round_trip(&Newtype(9), "9");
    round_trip(&Tuple(1, "a".into(), 0), r#"[1,"a"]"#);
    let said = json_error::<Tuple>("[1]");
    assert!(
        said.starts_with("invalid length 1, expected tuple struct Tuple with 2 elements"),
        "{said}"
    );
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(transparent)]
struct Transparent {
    inner: Vec<u8>,
}

#[test]
fn a_transparent_struct_is_its_one_field() {
    round_trip(&Transparent { inner: vec![1, 2] }, "[1,2]");
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Generic<T, const N: usize> {
    items: Vec<T>,
    #[serde(default)]
    extra: T,
    #[serde(skip)]
    width: PhantomData<[u8; N]>,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Bounded<T: Clone + Default = u8>
where
    T: PartialEq,
{
    value: T,
}

#[test]
fn generic_structs_bound_their_parameters() {
    round_trip(
        &Generic::<u8, 3> { items: vec![1, 2, 3], extra: 4, width: PhantomData },
        r#"{"items":[1,2,3],"extra":4}"#,
    );
    round_trip(&Bounded { value: String::from("v") }, r#"{"value":"v"}"#);
    round_trip(&Bounded::<u8> { value: 3 }, r#"{"value":3}"#);
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Borrowed<'a, 'b> {
    text: &'a str,
    #[serde(serialize_with = "as_bytes")]
    bytes: &'b [u8],
    #[serde(borrow)]
    cow: Cow<'a, str>,
    #[serde(borrow, serialize_with = "as_bytes")]
    cow_bytes: Cow<'b, [u8]>,
    owned: String,
}

/// Bytes as one byte string: serde writes a `[u8]` as a sequence of numbers.
fn as_bytes<S: Serializer, B: AsRef<[u8]>>(value: &B, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_bytes(value.as_ref())
}

fn lies_in(part: &[u8], whole: &[u8]) -> bool {
    whole.as_ptr_range().contains(&part.as_ptr())
}

/// `&str` and `&[u8]` borrow without being asked, and a `Cow` marked `borrow` borrows when the
/// input lends: each points into the input's bytes.
#[test]
fn borrowed_fields_point_into_the_input() {
    let value = Borrowed {
        text: "t",
        bytes: b"by",
        cow: Cow::Borrowed("c"),
        cow_bytes: Cow::Borrowed(b"cb"),
        owned: "o".into(),
    };
    let cbor = blaze_protocol::cbor::to_vec(&value).unwrap();
    let read: Borrowed<'_, '_> = blaze_protocol::cbor::from_slice(&cbor).unwrap();
    assert_eq!(read, value);
    assert!(lies_in(read.text.as_bytes(), &cbor) && lies_in(read.bytes, &cbor));
    assert!(matches!(read.cow, Cow::Borrowed(it) if lies_in(it.as_bytes(), &cbor)));
    assert!(matches!(read.cow_bytes, Cow::Borrowed(it) if lies_in(it, &cbor)));
    #[derive(Deserialize, Debug)]
    struct Text<'a> {
        #[serde(borrow)]
        cow: Cow<'a, str>,
    }
    let read: Text<'_> = serde_json::from_str(r#"{"cow":"c\n"}"#).unwrap();
    assert!(
        matches!(read.cow, Cow::Owned(ref it) if it == "c\n"),
        "an escaped string is owned: {read:?}"
    );
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
enum External {
    Unit,
    Newtype(u8),
    Tuple(u8, String),
    Struct {
        a: u8,
        #[serde(default)]
        b: u8,
    },
    Empty {},
}

#[test]
fn an_externally_tagged_enum_round_trips_every_variant_kind() {
    round_trip(&External::Unit, r#""Unit""#);
    round_trip(&External::Newtype(1), r#"{"Newtype":1}"#);
    round_trip(&External::Tuple(1, "t".into()), r#"{"Tuple":[1,"t"]}"#);
    round_trip(&External::Struct { a: 1, b: 2 }, r#"{"Struct":{"a":1,"b":2}}"#);
    round_trip(&External::Empty {}, r#"{"Empty":{}}"#);
    assert_eq!(
        serde_json::from_str::<External>(r#"{"Struct":{"a":1}}"#).unwrap(),
        External::Struct { a: 1, b: 0 }
    );
    let said = json_error::<External>(r#""Nope""#);
    assert!(
        said.starts_with(
            "unknown variant `Nope`, expected one of `Unit`, `Newtype`, `Tuple`, `Struct`, `Empty`"
        ),
        "{said}"
    );
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Inner {
    x: u8,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum Internal {
    UnitOne,
    Wraps(Inner),
    Holds {
        y: u8,
        #[serde(default)]
        z: Option<u8>,
    },
    #[serde(other)]
    Unknown,
}

#[test]
fn an_internally_tagged_enum_puts_its_tag_among_the_fields() {
    round_trip(&Internal::UnitOne, r#"{"kind":"unit-one"}"#);
    round_trip(&Internal::Wraps(Inner { x: 1 }), r#"{"kind":"wraps","x":1}"#);
    round_trip(&Internal::Holds { y: 2, z: Some(3) }, r#"{"kind":"holds","y":2,"z":3}"#);
    let read: Internal = serde_json::from_str(r#"{"y":2,"kind":"holds"}"#).unwrap();
    assert_eq!(read, Internal::Holds { y: 2, z: None }, "the tag need not come first");
    assert_eq!(
        serde_json::from_str::<Internal>(r#"{"kind":"later","q":1}"#).unwrap(),
        Internal::Unknown
    );
    let said = json_error::<Internal>(r#"{"y":2}"#);
    assert!(said.starts_with("missing field `kind`"), "{said}");
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(tag = "op")]
enum Strictly {
    Only,
}

#[test]
fn an_internally_tagged_enum_without_other_refuses_an_unknown_tag() {
    let said = json_error::<Strictly>(r#"{"op":"Else"}"#);
    assert!(said.starts_with("unknown variant `Else`, expected `Only`"), "{said}");
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(untagged)]
enum Untagged {
    Nothing,
    Number(u64),
    Pair(u8, u8),
    Named { name: String },
}

#[test]
fn an_untagged_enum_reads_the_first_variant_that_fits() {
    round_trip(&Untagged::Nothing, "null");
    round_trip(&Untagged::Number(5), "5");
    round_trip(&Untagged::Pair(1, 2), "[1,2]");
    round_trip(&Untagged::Named { name: "n".into() }, r#"{"name":"n"}"#);
    let said = json_error::<Untagged>(r#""text""#);
    assert!(said.starts_with("data did not match any variant of untagged enum Untagged"), "{said}");
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Flattened {
    id: u8,
    #[serde(flatten)]
    inner: Inner,
    #[serde(flatten)]
    rest: BTreeMap<String, u32>,
}

/// A flattened struct takes the keys it names; a flattened map after it sees the rest.
#[test]
fn flattened_fields_share_their_parents_map() {
    let mut rest = BTreeMap::new();
    rest.insert("other".to_owned(), 9);
    round_trip(&Flattened { id: 1, inner: Inner { x: 2 }, rest }, r#"{"id":1,"x":2,"other":9}"#);
}

mod as_seq {
    use blaze_serde::{
        Deserialize,
        serde::{Deserializer, Serializer, ser::SerializeSeq},
    };

    pub fn serialize<S: Serializer>(value: &(u8, u8), serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(2))?;
        seq.serialize_element(&value.0)?;
        seq.serialize_element(&value.1)?;
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<(u8, u8), D::Error> {
        let [a, b] = <[u8; 2]>::deserialize(deserializer)?;
        Ok((a, b))
    }
}

fn doubled<S: Serializer>(value: &u8, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_u16(u16::from(*value) * 2)
}

fn halved<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u8, D::Error> {
    let value = u16::deserialize(deserializer)?;
    u8::try_from(value / 2).map_err(blaze_serde::serde::de::Error::custom)
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct With<T> {
    #[serde(with = "as_seq")]
    pair: (u8, u8),
    #[serde(serialize_with = "doubled", deserialize_with = "halved")]
    half: u8,
    #[serde(deserialize_with = "halved", default)]
    missing: u8,
    plain: T,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
enum WithInVariant {
    V(#[serde(with = "as_seq")] (u8, u8)),
}

#[test]
fn with_serialize_with_and_deserialize_with_route_through_the_functions() {
    round_trip(
        &With { pair: (1, 2), half: 3, missing: 0, plain: 'c' },
        r#"{"pair":[1,2],"half":6,"missing":0,"plain":"c"}"#,
    );
    let read: With<char> = serde_json::from_str(r#"{"pair":[1,2],"half":6,"plain":"c"}"#).unwrap();
    assert_eq!(read.missing, 0);
    round_trip(&WithInVariant::V((4, 5)), r#"{"V":[4,5]}"#);
}

#[derive(Deserialize, PartialEq, Debug)]
struct Columns(Vec<u8>);

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(try_from = "Columns")]
struct Width {
    width: usize,
}

impl TryFrom<Columns> for Width {
    type Error = String;

    fn try_from(columns: Columns) -> Result<Self, String> {
        match columns.0.len() {
            0 => Err("no columns".to_owned()),
            width => Ok(Width { width }),
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(from = "u8")]
struct FromByte {
    value: u16,
}

impl From<u8> for FromByte {
    fn from(value: u8) -> Self {
        FromByte { value: u16::from(value) * 10 }
    }
}

/// `try_from` and `from` read the other type and convert; writing stays field by field, as
/// serde's derive writes without `into`.
#[test]
fn from_and_try_from_read_through_the_other_type() {
    assert_eq!(serde_json::from_str::<Width>("[1,2,3]").unwrap(), Width { width: 3 });
    let said = json_error::<Width>("[]");
    assert!(said.starts_with("no columns"), "{said}");
    assert_eq!(serde_json::to_string(&Width { width: 2 }).unwrap(), r#"{"width":2}"#);
    assert_eq!(serde_json::from_str::<FromByte>("4").unwrap(), FromByte { value: 40 });
}

#[derive(Deserialize)]
struct Parts<T> {
    items: Vec<T>,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(
    try_from = "Parts<T>",
    bound(serialize = "T: Serialize", deserialize = "T: Deserialize<'de> + Ord")
)]
struct Sorted<T> {
    items: Vec<T>,
}

impl<T: Ord> TryFrom<Parts<T>> for Sorted<T> {
    type Error = &'static str;

    fn try_from(parts: Parts<T>) -> Result<Self, &'static str> {
        match parts.items.is_sorted() {
            true => Ok(Sorted { items: parts.items }),
            false => Err("items out of order"),
        }
    }
}

/// `bound` replaces the bounds the derive would infer, here as blaze-codegen's `Relocs` uses it.
#[test]
fn bound_replaces_the_inferred_bounds() {
    round_trip(&Sorted { items: vec![1, 2] }, r#"{"items":[1,2]}"#);
    let said = json_error::<Sorted<u8>>(r#"{"items":[2,1]}"#);
    assert!(said.starts_with("items out of order"), "{said}");
}
