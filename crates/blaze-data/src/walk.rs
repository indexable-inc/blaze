//! A value walked and built without recursion, so no depth a reader accepts can overflow the
//! stack: [`Walk`] visits a [`Value`] as a stream of [`Visit`]s with its open containers on a
//! stack in a scratch allocator, and [`Build`] makes a value from the same shape of events
//! with its open containers on such a stack. Every format's writer is a walk and every reader
//! a build, so there is one iterative traversal, here.

#![blaze::sans_io]

use core::{alloc::Allocator, fmt};

use blaze_own::{String, Vec};

use crate::{Map, Step, Value, path::write_step};

/// The order a walk visits a map's entries in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Order {
    /// As inserted.
    Insertion,
    /// By key bytes (code-point order).
    Sorted,
    /// The shorter key first, then by bytes: deterministic CBOR's order of text keys, whose
    /// encodings sort this way (RFC 8949, section 4.2.1).
    LengthFirst,
}

/// One step of a walk.
pub enum Visit<'v, A: Allocator> {
    /// A value that is no list or map.
    Leaf(&'v Value<A>),
    /// A list opens; its elements follow, each after an [`Visit::Item`].
    StartList(&'v [Value<A>]),
    /// The next element of the open list, by its index; its value follows.
    Item(usize),
    /// The open list closes; its length.
    EndList(usize),
    /// A map opens; its entries follow, each a [`Visit::Key`] then the value.
    StartMap(&'v Map<A>),
    /// The next entry of the open map: its place in the walk's order and its key.
    Key(usize, &'v str),
    /// The open map closes; its length.
    EndMap(usize),
}

enum Frame<'v, A: Allocator, S: Allocator> {
    List { items: &'v [Value<A>], next: usize },
    /// `perm` is the entries' insertion indices in [`Order::LengthFirst`]'s order, else empty.
    Map { map: &'v Map<A>, next: usize, perm: Vec<usize, S> },
}

/// A value visited depth first; see the module documentation.
pub struct Walk<'v, A: Allocator, S: Allocator + Clone> {
    stack: Vec<Frame<'v, A, S>, S>,
    pending: Option<&'v Value<A>>,
    order: Order,
    scratch: S,
}

impl<'v, A: Allocator, S: Allocator + Clone> Walk<'v, A, S> {
    pub fn new_in(value: &'v Value<A>, order: Order, scratch: S) -> Walk<'v, A, S> {
        Walk { stack: Vec::new_in(scratch.clone()), pending: Some(value), order, scratch }
    }

    /// How many lists and maps are open.
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// The path to the value visited last (`a.b[3]`, empty at the root), as
    /// [`At::write_into`](crate::At::write_into) writes one.
    pub fn path_into<W: fmt::Write + ?Sized>(&self, out: &mut W) -> fmt::Result {
        self.path_after(out, false)
    }

    /// [`path_into`](Self::path_into) after a prefix: `before` says whether one was written,
    /// so the first key gets its `.`.
    pub fn path_after<W: fmt::Write + ?Sized>(&self, out: &mut W, mut before: bool) -> fmt::Result {
        for frame in self.stack.iter() {
            let step = match frame {
                Frame::List { next: 0, .. } | Frame::Map { next: 0, .. } => continue,
                Frame::List { next, .. } => Step::Index(*next - 1),
                Frame::Map { map, next, perm } => match self.entry(*map, perm, *next - 1) {
                    Some((key, _)) => Step::Key(key),
                    None => unreachable!("a map frame's entries before `next` are there"),
                },
            };
            before = write_step(out, step, before)?;
        }
        Ok(())
    }

    fn entry(&self, map: &'v Map<A>, perm: &Vec<usize, S>, i: usize) -> Option<(&'v str, &'v Value<A>)> {
        match self.order {
            Order::Insertion => map.entry_at(i),
            Order::Sorted => map.sorted_at(i),
            Order::LengthFirst => map.entry_at(*perm.get(i)?),
        }
    }

    fn open(&mut self, value: &'v Value<A>) -> Visit<'v, A> {
        match value {
            Value::List(list) => {
                self.stack.push(Frame::List { items: list.as_slice(), next: 0 });
                Visit::StartList(list.as_slice())
            }
            Value::Map(map) => {
                let mut perm = Vec::new_in(self.scratch.clone());
                if self.order == Order::LengthFirst {
                    perm = Vec::with_capacity_in(map.len(), self.scratch.clone());
                    for i in 0..map.len() {
                        perm.push(i);
                    }
                    perm.sort_unstable_by(|&a, &b| {
                        let (Some((a, _)), Some((b, _))) = (map.entry_at(a), map.entry_at(b)) else {
                            unreachable!("the permutation holds the map's indices")
                        };
                        a.len().cmp(&b.len()).then_with(|| a.as_bytes().cmp(b.as_bytes()))
                    });
                }
                self.stack.push(Frame::Map { map, next: 0, perm });
                Visit::StartMap(map)
            }
            leaf => Visit::Leaf(leaf),
        }
    }
}

impl<'v, A: Allocator, S: Allocator + Clone> Iterator for Walk<'v, A, S> {
    type Item = Visit<'v, A>;

    fn next(&mut self) -> Option<Visit<'v, A>> {
        if let Some(value) = self.pending.take() {
            return Some(self.open(value));
        }
        let order = self.order;
        let (visit, pending) = match self.stack.last_mut()? {
            Frame::List { items, next } => {
                let items: &'v [Value<A>] = *items;
                match items.get(*next) {
                    Some(item) => {
                        *next += 1;
                        (Visit::Item(*next - 1), Some(item))
                    }
                    None => (Visit::EndList(items.len()), None),
                }
            }
            Frame::Map { map, next, perm } => {
                let map: &'v Map<A> = *map;
                let entry = match order {
                    Order::Insertion => map.entry_at(*next),
                    Order::Sorted => map.sorted_at(*next),
                    Order::LengthFirst => perm.get(*next).and_then(|&i| map.entry_at(i)),
                };
                match entry {
                    Some((key, value)) => {
                        *next += 1;
                        (Visit::Key(*next - 1, key), Some(value))
                    }
                    None => (Visit::EndMap(map.len()), None),
                }
            }
        };
        match pending {
            Some(value) => self.pending = Some(value),
            None => {
                self.stack.pop();
            }
        }
        Some(visit)
    }
}

enum Open<A: Allocator> {
    List(Vec<Value<A>, A>),
    /// A map and the key whose value is due, if one is.
    Map(Map<A>, Option<String<A>>),
}

/// A value made from events, its open containers on a stack in `S`; see the module
/// documentation. The caller (a format's reader) has checked the events' shape: a misuse is a
/// bug in it and stops with a message naming the call.
pub struct Build<A: Allocator + Clone, S: Allocator> {
    open: Vec<Open<A>, S>,
    done: Option<Value<A>>,
    alloc: A,
}

impl<A: Allocator + Clone, S: Allocator> Build<A, S> {
    /// A build of a value in `alloc`, its stack in `scratch`.
    pub fn new_in(alloc: A, scratch: S) -> Build<A, S> {
        Build { open: Vec::new_in(scratch), done: None, alloc }
    }

    /// The allocator the value is made in.
    pub fn alloc(&self) -> &A {
        &self.alloc
    }

    /// How many lists and maps are open.
    pub fn depth(&self) -> usize {
        self.open.len()
    }

    /// A whole value (a scalar, or a container made elsewhere) where one is due.
    pub fn value(&mut self, value: Value<A>) {
        match self.open.last_mut() {
            None if self.done.is_none() => self.done = Some(value),
            None => unreachable!("`Build::value` after the top value was whole"),
            Some(Open::List(list)) => list.push(value),
            Some(Open::Map(map, key)) => match key.take() {
                // `key` checked the key is new, or the caller asked the later value to win.
                Some(key) => {
                    map.insert(key, value);
                }
                None => unreachable!("`Build::value` in a map before its key"),
            },
        }
    }

    pub fn start_list(&mut self, capacity: usize) {
        self.due_check("start_list");
        self.open.push(Open::List(Vec::with_capacity_in(capacity, self.alloc.clone())));
    }

    pub fn start_map(&mut self, capacity: usize) {
        self.due_check("start_map");
        self.open.push(Open::Map(Map::with_capacity_in(capacity, self.alloc.clone()), None));
    }

    fn due_check(&self, call: &str) {
        match (self.open.last(), &self.done) {
            (None, Some(_)) => unreachable!("`Build::{call}` after the top value was whole"),
            (Some(Open::Map(_, None)), _) => unreachable!("`Build::{call}` in a map before its key"),
            _ => {}
        }
    }

    /// The next key of the open map; refused, with the insertion index of the first, when the
    /// map has it.
    pub fn key(&mut self, key: String<A>) -> Result<(), usize> {
        match self.open.last_mut() {
            Some(Open::Map(map, due @ None)) => {
                if let Some(first) = map.position(key.as_str()) {
                    return Err(first);
                }
                *due = Some(key);
                Ok(())
            }
            _ => unreachable!("`Build::key` outside a map, or with a key due"),
        }
    }

    /// The next key of the open map, its value to replace an earlier one's under that key.
    pub fn key_last_wins(&mut self, key: String<A>) {
        match self.open.last_mut() {
            Some(Open::Map(_, due @ None)) => *due = Some(key),
            _ => unreachable!("`Build::key_last_wins` outside a map, or with a key due"),
        }
    }

    /// The open list or map closes and takes its place in its parent.
    pub fn end(&mut self) {
        let value = match self.open.pop() {
            Some(Open::List(list)) => Value::List(list),
            Some(Open::Map(map, None)) => Value::Map(map),
            Some(Open::Map(_, Some(_))) => unreachable!("`Build::end` of a map with a key's value due"),
            None => unreachable!("`Build::end` with nothing open"),
        };
        self.value(value);
    }

    /// Whether the top value is whole.
    pub fn is_whole(&self) -> bool {
        self.open.is_empty() && self.done.is_some()
    }

    /// The value, once whole.
    pub fn finish(self) -> Option<Value<A>> {
        match self.open.is_empty() {
            true => self.done,
            false => None,
        }
    }
}
