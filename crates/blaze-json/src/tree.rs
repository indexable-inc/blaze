//! [`read_in`]: JSON text to a [`Value`], over the [`Reader`], into a [`Build`]: the reader's
//! events go straight into the build's stack of open containers, so no depth recurses.

use core::alloc::Allocator;

use blaze_data::{Build, Value};

use crate::{Duplicates, Error, Limits, Reader, What, read::Event};

/// `bytes` as one JSON value in `alloc`: maps keep the text's key order (and so its sorted view
/// too); a duplicate key is refused at the second one unless the limits say
/// [`Duplicates::LastWins`]; anything but blanks after the value is refused. The reader's and
/// the build's stacks are in `alloc` too.
pub fn read_in<A: Allocator + Clone>(bytes: &[u8], limits: Limits, alloc: A) -> Result<Value<A>, Error> {
    let mut reader = Reader::new_in(bytes, limits, alloc.clone())?;
    let mut build = Build::new_in(alloc.clone(), alloc.clone());
    // The reader ends only after a whole value and the blanks after it, so `None` is the end.
    while let Some(item) = reader.next()? {
        match item.event {
            Event::Null => build.value(Value::Null),
            Event::Bool(b) => build.value(Value::Bool(b)),
            Event::Int(n) => build.value(Value::Int(n)),
            Event::Float(f) => build.value(Value::Float(f)),
            Event::Text(text) => build.value(Value::Text(text.to_string_in(alloc.clone()))),
            Event::StartList => build.start_list(0),
            Event::StartMap => build.start_map(0),
            Event::EndList | Event::EndMap => build.end(),
            Event::Key(key) => {
                let key = key.to_string_in(alloc.clone());
                match limits.duplicates {
                    Duplicates::Refuse => {
                        if build.key(key).is_err() {
                            return Err(Error { at: item.at, what: What::DuplicateKey });
                        }
                    }
                    Duplicates::LastWins => build.key_last_wins(key),
                }
            }
        }
    }
    match build.finish() {
        Some(value) => Ok(value),
        None => unreachable!("the reader ends only after a whole value"),
    }
}
