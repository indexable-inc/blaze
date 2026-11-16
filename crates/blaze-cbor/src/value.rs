//! [`Value`] in and out, by the table in the crate's docs.

use core::alloc::Allocator;

use blaze_data::{Map, Value};
use blaze_own::{String, Vec};

use crate::{Decoder, Error, Event, Limits, wide, why, write};

/// `value`'s deterministic CBOR in `alloc`.
pub fn to_vec_in<A: Allocator, B: Allocator + Clone>(value: &Value<A>, alloc: B) -> Vec<u8, B> {
    let mut out = Vec::new_in(alloc.clone());
    write_into(value, &mut out, alloc);
    out
}

/// `value`'s deterministic CBOR appended to `out`; each map's key order is found in `scratch`.
/// Every `Value` has a CBOR form, so this cannot fail.
pub fn write_into<A: Allocator, B: Allocator, S: Allocator + Clone>(value: &Value<A>, out: &mut Vec<u8, B>, scratch: S) {
    match value {
        Value::Null => write::null(out),
        Value::Bool(b) => write::boolean(out, *b),
        Value::Int(n) => write::int(out, *n),
        Value::Float(x) => write::float(out, *x),
        Value::Text(text) => write::text(out, text.as_str()),
        Value::Bytes(bytes) => write::bytes(out, bytes.as_slice()),
        Value::List(list) => {
            write::array(out, wide(list.len()));
            for item in list.iter() {
                write_into(item, out, scratch.clone());
            }
        }
        Value::Map(map) => {
            write::map(out, wide(map.len()));
            let mut order = Vec::with_capacity_in(map.len(), scratch.clone());
            for entry in map.iter() {
                order.push(entry);
            }
            // A text key's encoding is its length's head then its bytes, and heads grow with
            // the length: the encodings' bytewise order is the shorter key first, then bytes.
            let length_first = crate::plant::length_first();
            order.sort_unstable_by(|(a, _), (b, _)| {
                let by_len = if length_first { a.len().cmp(&b.len()) } else { core::cmp::Ordering::Equal };
                by_len.then_with(|| a.as_bytes().cmp(b.as_bytes()))
            });
            for (key, item) in order.iter() {
                write::text(out, key);
                write_into(item, out, scratch.clone());
            }
        }
    }
}

/// One value read from `bytes` in `alloc`, within `limits`; every rule in the crate's docs
/// held, and nothing after the value.
pub fn read_in<A: Allocator + Clone>(bytes: &[u8], limits: Limits, alloc: A) -> Result<Value<A>, Error> {
    let mut decoder = Decoder::new_in(bytes, limits, alloc.clone())?;
    let value = build(&mut decoder, &alloc)?;
    match decoder.next()? {
        None => Ok(value),
        Some(item) => Err(Error { at: item.at, why: why::BYTES_AFTER }),
    }
}

fn next<'a, D: Allocator>(decoder: &mut Decoder<'a, D>) -> Result<crate::Item<'a>, Error> {
    match decoder.next()? {
        Some(item) => Ok(item),
        None => unreachable!("the decoder ends only after a whole value"),
    }
}

fn build<D: Allocator, A: Allocator + Clone>(decoder: &mut Decoder<'_, D>, alloc: &A) -> Result<Value<A>, Error> {
    let item = next(decoder)?;
    Ok(match item.event {
        Event::Null => Value::Null,
        Event::Bool(b) => Value::Bool(b),
        Event::Int(n) => Value::Int(n),
        Event::Float(x) => Value::Float(x),
        Event::Text(text) => Value::Text(String::from_str_in(text, alloc.clone())),
        Event::Bytes(bytes) => {
            let mut out = Vec::with_capacity_in(bytes.len(), alloc.clone());
            out.extend_from_slice(bytes);
            Value::Bytes(out)
        }
        Event::StartList { len } => {
            let mut list = Vec::with_capacity_in(len, alloc.clone());
            for _ in 0..len {
                list.push(build(decoder, alloc)?);
            }
            let _end = next(decoder)?;
            Value::List(list)
        }
        Event::StartMap { len } => {
            let mut map = Map::with_capacity_in(len, alloc.clone());
            for _ in 0..len {
                let Event::Key(key) = next(decoder)?.event else { unreachable!("a map's entry starts with its key") };
                let key = String::from_str_in(key, alloc.clone());
                let value = build(decoder, alloc)?;
                // The decoder refused a key twice, so nothing is replaced.
                map.insert(key, value);
            }
            let _end = next(decoder)?;
            Value::Map(map)
        }
        Event::Key(_) | Event::EndList | Event::EndMap => unreachable!("the decoder balances its events"),
    })
}
