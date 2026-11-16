//! A [`Document`] lowered to the format-neutral [`Value`]: tables to maps in the order the text
//! names their keys, arrays to lists, strings to text, integers and floats as they are, booleans,
//! and a datetime to its RFC 3339 text (TOML's datetimes have no `Value` variant of their own;
//! [`Item::as_datetime`] keeps the typed form for a caller that needs it).

use core::alloc::Allocator;

use blaze_data::{Int, Map, Value};
use blaze_own::{String, Vec};

use crate::doc::{Document, Item, Kind};

impl<A: Allocator> Document<'_, A> {
    /// The whole document as a map in `alloc`.
    pub fn to_value_in<B: Allocator + Clone>(&self, alloc: B) -> Value<B> {
        self.root().to_value_in(alloc)
    }
}

impl<A: Allocator> Item<'_, '_, A> {
    /// This node and all under it as a value in `alloc`.
    pub fn to_value_in<B: Allocator + Clone>(&self, alloc: B) -> Value<B> {
        match self.kind() {
            Kind::Table => {
                let mut map = Map::new_in(alloc.clone());
                for child in self.children() {
                    let key = String::from_str_in(child.key(), alloc.clone());
                    // The reader refused every duplicate, so no insert here replaces.
                    map.insert(key, child.to_value_in(alloc.clone()));
                }
                Value::Map(map)
            }
            Kind::Array => {
                let mut list = Vec::with_capacity_in(self.len(), alloc.clone());
                for child in self.children() {
                    list.push(child.to_value_in(alloc.clone()));
                }
                Value::List(list)
            }
            Kind::String => match self.as_str() {
                Some(text) => Value::Text(String::from_str_in(text, alloc)),
                None => unreachable!("a string node has text"),
            },
            Kind::Integer => match self.as_integer() {
                Some(n) => Value::Int(Int::from_i64(n)),
                None => unreachable!("an integer node has a value"),
            },
            Kind::Float => match self.as_float() {
                Some(x) => Value::Float(x),
                None => unreachable!("a float node has a value"),
            },
            Kind::Bool => Value::Bool(self.as_bool() == Some(true)),
            Kind::Datetime => match self.as_datetime() {
                Some(dt) => {
                    let mut text = String::new_in(alloc);
                    blaze_own::format_into!(&mut text, "{dt}");
                    Value::Text(text)
                }
                None => unreachable!("a datetime node has a datetime"),
            },
        }
    }
}
