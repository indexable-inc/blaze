//! A [`Document`] lowered to the format-neutral [`Value`]: tables to maps in the order the text
//! names their keys, arrays to lists, strings to text, integers and floats as they are, booleans,
//! and a datetime to its RFC 3339 text (TOML's datetimes have no `Value` variant of their own;
//! [`Item::as_datetime`] keeps the typed form for a caller that needs it).
//!
//! The lowering walks the node columns (first child, next sibling, parent) into a [`Build`],
//! so no depth recurses.

use core::alloc::Allocator;

use blaze_data::{Build, Int, Map, Value};
use blaze_own::String;

use crate::doc::{Document, Item, Kind, NONE, widen};

impl<A: Allocator> Document<'_, A> {
    /// The whole document as a map in `alloc`.
    pub fn to_value_in<B: Allocator + Clone>(&self, alloc: B) -> Value<B> {
        self.root().to_value_in(alloc)
    }

    /// The whole document as a map in `alloc`, unwrapped: the root is always a table.
    pub fn to_map_in<B: Allocator + Clone>(&self, alloc: B) -> Map<B> {
        match self.to_value_in(alloc).into_map() {
            Some(map) => map,
            None => unreachable!("a document's root is a table"),
        }
    }
}

impl<A: Allocator> Item<'_, '_, A> {
    /// This node and all under it as a value in `alloc`; the walk's stack is in `alloc` too.
    pub fn to_value_in<B: Allocator + Clone>(&self, alloc: B) -> Value<B> {
        let nodes = &self.doc.nodes;
        let mut build = Build::new_in(alloc.clone(), alloc.clone());
        let mut at = self.id;
        loop {
            let item = Item { doc: self.doc, id: at };
            if at != self.id && nodes.kinds()[widen(nodes.parents()[widen(at)])] == Kind::Table {
                // The reader refused every duplicate, so no key here is twice.
                if build.key(String::from_str_in(item.key(), alloc.clone())).is_err() {
                    unreachable!("the reader refused a key twice in one table");
                }
            }
            let opened = match item.kind() {
                Kind::Table => {
                    build.start_map(0);
                    true
                }
                Kind::Array => {
                    build.start_list(0);
                    true
                }
                _ => {
                    build.value(leaf(item, alloc.clone()));
                    false
                }
            };
            if opened {
                let first = nodes.first_children()[widen(at)];
                if first != NONE {
                    at = first;
                    continue;
                }
                build.end();
            }
            // Climb to the next sibling, closing each container whose last child this was.
            loop {
                if at == self.id {
                    return match build.finish() {
                        Some(value) => value,
                        None => unreachable!("the walk closed every container it opened"),
                    };
                }
                let next = nodes.next_siblings()[widen(at)];
                if next != NONE {
                    at = next;
                    break;
                }
                at = nodes.parents()[widen(at)];
                build.end();
            }
        }
    }
}

/// A node that is no table or array as a value in `alloc`.
fn leaf<A: Allocator, B: Allocator + Clone>(item: Item<'_, '_, A>, alloc: B) -> Value<B> {
    match item.kind() {
        Kind::String => match item.as_str() {
            Some(text) => Value::Text(String::from_str_in(text, alloc)),
            None => unreachable!("a string node has text"),
        },
        Kind::Integer => match item.as_integer() {
            Some(n) => Value::Int(Int::from_i64(n)),
            None => unreachable!("an integer node has a value"),
        },
        Kind::Float => match item.as_float() {
            Some(x) => Value::Float(x),
            None => unreachable!("a float node has a value"),
        },
        Kind::Bool => Value::Bool(item.as_bool() == Some(true)),
        Kind::Datetime => match item.as_datetime() {
            Some(dt) => {
                let mut text = String::new_in(alloc);
                blaze_own::format_into!(&mut text, "{dt}");
                Value::Text(text)
            }
            None => unreachable!("a datetime node has a datetime"),
        },
        Kind::Table | Kind::Array => unreachable!("`leaf` of a container"),
    }
}
