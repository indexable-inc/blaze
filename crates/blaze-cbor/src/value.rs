//! [`Value`] in and out, by the table in the crate's docs: written by a [`Walk`] and read into a
//! [`Build`], each with its open containers on a stack in an allocator, so no depth recurses.

use core::alloc::Allocator;

use blaze_data::{Build, Order, Value, Visit, Walk};
use blaze_own::{
    String, Vec,
    planted::{NoPlant, Planted},
};

use crate::{CborPlant, Decoder, Error, Event, Limits, wide, why, write};

/// The most room reserved for a list or map ahead of its elements: a head's length is checked
/// against the bytes left, but each byte may become a whole `Value`, so the reservation is
/// capped and the rest grows as elements arrive.
const MOST_RESERVED: usize = 4096;

/// `value`'s deterministic CBOR in `alloc`.
pub fn to_vec_in<A: Allocator, B: Allocator + Copy>(value: &Value<A>, alloc: B) -> Vec<u8, B> {
    let mut out = Vec::new_in(alloc);
    write_into(value, &mut out, alloc);
    out
}

/// `value`'s deterministic CBOR appended to `out`; the walk's stack and each map's key order
/// are kept in `scratch`. Every `Value` has a CBOR form, so this cannot fail.
pub fn write_into<A: Allocator, B: Allocator, S: Allocator + Clone>(value: &Value<A>, out: &mut Vec<u8, B>, scratch: S) {
    write_into_planted(value, out, scratch, NoPlant);
}

/// [`write_into`] with its plant seam: `plant` is [`NoPlant`] where it ships and a
/// [`CborPlant`] under the round trip's control (D165).
pub fn write_into_planted<A: Allocator, B: Allocator, S: Allocator + Clone>(
    value: &Value<A>,
    out: &mut Vec<u8, B>,
    scratch: S,
    plant: impl Planted<CborPlant>,
) {
    // A text key's encoding is its length's head then its bytes, and heads grow with the
    // length: the encodings' bytewise order is the shorter key first, then bytes.
    let order = match plant.has(CborPlant::KeysBytewise) {
        false => Order::LengthFirst,
        true => Order::Sorted,
    };
    for visit in Walk::new_in(value, order, scratch) {
        match visit {
            Visit::Leaf(Value::Null) => write::null(out),
            Visit::Leaf(Value::Bool(b)) => write::boolean(out, *b),
            Visit::Leaf(Value::Int(n)) => write::int(out, *n),
            Visit::Leaf(Value::Float(x)) => write::float(out, *x),
            Visit::Leaf(Value::Text(text)) => write::text(out, text.as_str()),
            Visit::Leaf(Value::Bytes(bytes)) => write::bytes(out, bytes.as_slice()),
            Visit::Leaf(Value::List(_) | Value::Map(_)) => unreachable!("a walk visits containers as starts"),
            Visit::StartList(list) => write::array(out, wide(list.len())),
            Visit::StartMap(map) => write::map(out, wide(map.len())),
            Visit::Key(_, key) => write::text(out, key),
            Visit::Item(_) | Visit::EndList(_) | Visit::EndMap(_) => {}
        }
    }
}

/// One value read from `bytes` in `alloc`, within `limits`; every rule in the crate's docs
/// held (a key twice among them), and nothing after the value. The decoder's and the build's
/// stacks are in `alloc` too.
pub fn read_in<A: Allocator + Copy>(bytes: &[u8], limits: Limits, alloc: A) -> Result<Value<A>, Error> {
    let mut decoder = Decoder::new_in(bytes, limits, alloc)?;
    let mut build = Build::new_in(alloc, alloc);
    while !build.is_whole() {
        let Some(item) = decoder.next()? else { unreachable!("the decoder ends only after a whole value") };
        match item.event {
            Event::Null => build.value(Value::Null),
            Event::Bool(b) => build.value(Value::Bool(b)),
            Event::Int(n) => build.value(Value::Int(n)),
            Event::Float(x) => build.value(Value::Float(x)),
            Event::Text(text) => build.value(Value::Text(String::from_str_in(text, alloc))),
            Event::Bytes(bytes) => {
                let mut out = Vec::with_capacity_in(bytes.len(), alloc);
                out.extend_from_slice(bytes);
                build.value(Value::Bytes(out));
            }
            Event::StartList { len } => build.start_list(len.min(MOST_RESERVED)),
            Event::StartMap { len } => build.start_map(len.min(MOST_RESERVED)),
            Event::Key(key) => {
                if build.key(String::from_str_in(key, alloc)).is_err() {
                    unreachable!("the decoder refuses a key twice by the key order");
                }
            }
            Event::EndList | Event::EndMap => build.end(),
        }
    }
    if let Some(item) = decoder.next()? {
        return Err(Error { at: item.at, why: why::BYTES_AFTER });
    }
    match build.finish() {
        Some(value) => Ok(value),
        None => unreachable!("the build is whole"),
    }
}
