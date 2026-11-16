//! [`read_in`]: JSON text to a [`Value`], over the [`Reader`].

use core::alloc::Allocator;

use blaze_data::{Map, Value};
use blaze_own::Vec;

use crate::{Duplicates, Error, Item, Limits, Reader, What, read::Event};

/// `bytes` as one JSON value in `alloc`: maps keep the text's key order (and so its sorted view
/// too); a duplicate key is refused at the second one unless the limits say
/// [`Duplicates::LastWins`].
pub fn read_in<A: Allocator + Clone>(bytes: &[u8], limits: Limits, alloc: A) -> Result<Value<A>, Error> {
    let mut reader = Reader::new(bytes, limits)?;
    let first = next(&mut reader)?;
    let value = build(&mut reader, first, limits.duplicates, &alloc)?;
    match reader.next()? {
        None => Ok(value),
        Some(item) => unreachable!("the reader has a whole value, then {item:?}"),
    }
}

/// The next event inside a value, where the reader cannot end without an error.
fn next<'t>(reader: &mut Reader<'t>) -> Result<Item<'t>, Error> {
    match reader.next()? {
        Some(item) => Ok(item),
        None => unreachable!("the reader ends only after a whole value"),
    }
}

fn build<'t, A: Allocator + Clone>(
    reader: &mut Reader<'t>,
    item: Item<'t>,
    duplicates: Duplicates,
    alloc: &A,
) -> Result<Value<A>, Error> {
    Ok(match item.event {
        Event::Null => Value::Null,
        Event::Bool(b) => Value::Bool(b),
        Event::Int(n) => Value::Int(n),
        Event::Float(f) => Value::Float(f),
        Event::Text(text) => Value::Text(text.to_string_in(alloc.clone())),
        Event::StartList => {
            let mut list = Vec::new_in(alloc.clone());
            loop {
                let item = next(reader)?;
                if item.event == Event::EndList {
                    break;
                }
                list.push(build(reader, item, duplicates, alloc)?);
            }
            Value::List(list)
        }
        Event::StartMap => {
            let mut map = Map::new_in(alloc.clone());
            loop {
                let key_item = next(reader)?;
                let key = match key_item.event {
                    Event::EndMap => break,
                    Event::Key(key) => key.to_string_in(alloc.clone()),
                    other => unreachable!("a map holds keys, not {other:?}"),
                };
                let value_item = next(reader)?;
                let value = build(reader, value_item, duplicates, alloc)?;
                match duplicates {
                    Duplicates::Refuse => {
                        if map.insert_new(key, value).is_err() {
                            return Err(Error { at: key_item.at, what: What::DuplicateKey });
                        }
                    }
                    Duplicates::LastWins => {
                        map.insert(key, value);
                    }
                }
            }
            Value::Map(map)
        }
        Event::Key(_) | Event::EndList | Event::EndMap => {
            unreachable!("a value starts with a value's event, not {:?}", item.event)
        }
    })
}
