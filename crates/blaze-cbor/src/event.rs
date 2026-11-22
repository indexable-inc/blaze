//! The streaming form, as blaze-json's: bytes to [`Event`]s ([`Decoder`]) and events to
//! bytes ([`Encoder`]). A list's and a map's event carries its length, since CBOR's heads say
//! it first. Both hold the encoding rules: the decoder refuses what breaks them, the encoder
//! refuses an event that would (a key out of order is refused, not moved: a stream cannot be
//! reordered, and choosing quietly is not this crate's).

use core::{alloc::Allocator, cmp::Ordering, fmt};

use blaze_data::Int;
use blaze_own::Vec;

use crate::{Error, Limits, Reader, major, wide, why, write};

/// One step of a value.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Event<'a> {
    Null,
    Bool(bool),
    Int(Int),
    Float(f64),
    Bytes(&'a [u8]),
    Text(&'a str),
    /// A map's key; its value's events follow.
    Key(&'a str),
    StartList { len: usize },
    EndList,
    StartMap { len: usize },
    EndMap,
}

/// An event and the offset it starts at.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Item<'a> {
    pub at: usize,
    pub event: Event<'a>,
}

/// What an open container takes next: one state, in place of a map flag and a key flag.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Due {
    /// A list's next item.
    Item,
    /// A map's next key.
    Key,
    /// The value of the map's key just read.
    Value,
}

impl Due {
    /// The state of a container opened as a map (`map`) or a list.
    fn opened(map: bool) -> Due {
        if map { Due::Key } else { Due::Item }
    }

    fn is_map(self) -> bool {
        self != Due::Item
    }

    /// The state after an item (a list's) or a value (a map's) was taken.
    fn after_item(self) -> Due {
        match self {
            Due::Item => Due::Item,
            Due::Key | Due::Value => Due::Key,
        }
    }
}

#[derive(Clone, Copy)]
struct Frame {
    /// Items (a map's: entries) still to come.
    left: usize,
    due: Due,
    /// The previous key's encoded bytes, as a range; empty before the first.
    prev: (usize, usize),
}

/// Where a stream is in its one value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    /// Nothing read or written yet.
    Before,
    /// The value has begun (it may also have ended: the stack says).
    Begun,
}

/// Bytes to events, one at a time, within [`Limits`]. A pull reader over the whole input (the
/// input is one record already in memory), not a `feed`/`finish` codec: see the crate's docs.
pub struct Decoder<'a, A: Allocator> {
    reader: Reader<'a>,
    limits: Limits,
    stack: Vec<Frame, A>,
    place: Place,
}

/// `(new, prev)` keys' encodings compared: the rule's refusal if `new` is not after `prev`.
fn key_order(new: &[u8], prev: &[u8]) -> Result<(), &'static str> {
    if prev.is_empty() {
        return Ok(());
    }
    match prev.cmp(new) {
        Ordering::Less => Ok(()),
        Ordering::Equal => Err(why::KEY_TWICE),
        Ordering::Greater => Err(why::KEY_ORDER),
    }
}

impl<'a, A: Allocator> Decoder<'a, A> {
    /// A decoder of `input`, its stack in `alloc`; an input past `limits.most_bytes` refused.
    pub fn new_in(input: &'a [u8], limits: Limits, alloc: A) -> Result<Decoder<'a, A>, Error> {
        if input.len() > limits.most_bytes {
            return Err(Error { at: 0, why: why::TOO_LARGE });
        }
        Ok(Decoder { reader: Reader::new(input), limits, stack: Vec::new_in(alloc), place: Place::Before })
    }

    /// Where the next event starts.
    pub fn at(&self) -> usize {
        self.reader.at
    }

    fn fail<T>(&self, at: usize, why: &'static str) -> Result<T, Error> {
        Err(Error { at, why })
    }

    /// The next event; `None` once the one value has ended where the input does.
    pub fn next(&mut self) -> Result<Option<Item<'a>>, Error> {
        let at = self.reader.at;
        match self.stack.last() {
            Some(top) if top.left == 0 && top.due != Due::Value => {
                let map = top.due.is_map();
                self.stack.pop();
                let event = if map { Event::EndMap } else { Event::EndList };
                return Ok(Some(Item { at, event }));
            }
            Some(top) if top.due == Due::Key => return self.key().map(Some),
            Some(_) => {}
            None if self.place == Place::Begun => {
                self.reader.end()?;
                return Ok(None);
            }
            None => self.place = Place::Begun,
        }
        if let Some(top) = self.stack.last_mut() {
            top.left -= 1;
            top.due = top.due.after_item();
        }
        self.value().map(Some)
    }

    fn key(&mut self) -> Result<Item<'a>, Error> {
        let start = self.reader.at;
        match self.reader.input.get(start) {
            None => return self.fail(start, why::END_AT_VALUE),
            Some(first) if first >> 5 != major::TEXT => return self.fail(start, why::KEY_NOT_TEXT),
            Some(_) => {}
        }
        let key = self.reader.text(usize::MAX, why::TEXT_PAST_INPUT)?;
        let end = self.reader.at;
        let Some(top) = self.stack.last_mut() else { unreachable!("a key is read in a map") };
        let input = self.reader.input;
        if let Err(why) = key_order(&input[start..end], &input[top.prev.0..top.prev.1]) {
            self.reader.at = start;
            return self.fail(start, why);
        }
        top.prev = (start, end);
        top.due = Due::Value;
        Ok(Item { at: start, event: Event::Key(key) })
    }

    fn open(&mut self, start: usize, len: usize, map: bool) -> Result<(), Error> {
        if self.stack.len() >= self.limits.most_depth {
            self.reader.at = start;
            return self.fail(start, why::TOO_DEEP);
        }
        self.stack.push(Frame { left: len, due: Due::opened(map), prev: (0, 0) });
        Ok(())
    }

    fn value(&mut self) -> Result<Item<'a>, Error> {
        let start = self.reader.at;
        let Some(&first) = self.reader.input.get(start) else {
            return self.fail(start, why::END_AT_VALUE);
        };
        let event = match first >> 5 {
            major::UINT => Event::Int(Int::from_u64(self.reader.uint()?)),
            major::NEGATIVE => {
                let n = self.reader.number(major::NEGATIVE, why::NOT_UINT)?;
                match i64::try_from(n) {
                    // -1 - n, which is !n for n in i64.
                    Ok(n) => Event::Int(Int::from_i64(!n)),
                    Err(_) => {
                        self.reader.at = start;
                        return self.fail(start, why::NEGATIVE_PAST_I64);
                    }
                }
            }
            major::BYTES => Event::Bytes(self.reader.bytes(usize::MAX, why::BYTES_PAST_INPUT)?),
            major::TEXT => Event::Text(self.reader.text(usize::MAX, why::TEXT_PAST_INPUT)?),
            major::ARRAY => {
                let len = self.reader.array(usize::MAX, why::ARRAY_PAST_INPUT)?;
                self.open(start, len, false)?;
                Event::StartList { len }
            }
            major::MAP => {
                let n = self.reader.number(major::MAP, why::NOT_ARRAY)?;
                let left = wide(self.reader.input.len() - self.reader.at);
                if n.checked_mul(2).is_none_or(|items| items > left) {
                    self.reader.at = start;
                    return self.fail(start, why::MAP_PAST_INPUT);
                }
                let len = usize::try_from(n).unwrap_or_else(|_| unreachable!("checked against the input"));
                self.open(start, len, true)?;
                Event::StartMap { len }
            }
            major::TAG => return self.fail(start, why::TAG),
            _ => self.simple(start, first)?,
        };
        Ok(Item { at: start, event })
    }

    fn simple(&mut self, start: usize, first: u8) -> Result<Event<'a>, Error> {
        let size = match first {
            write::FALSE => return self.step(Event::Bool(false)),
            write::TRUE => return self.step(Event::Bool(true)),
            write::NULL => return self.step(Event::Null),
            write::HALF => 2,
            write::SINGLE => 4,
            write::DOUBLE => 8,
            0xff => return self.fail(start, why::INDEFINITE),
            _ => return self.fail(start, why::SIMPLE),
        };
        self.reader.at += 1;
        let raw = match self.reader.take(size) {
            Ok(raw) => raw,
            Err(_) => {
                self.reader.at = start;
                return self.fail(start, why::END_IN_VALUE);
            }
        };
        let bits = raw.iter().fold(0u64, |n, byte| (n << 8) | u64::from(*byte));
        let low = |n: u64| u32::try_from(n).unwrap_or_else(|_| unreachable!("as many bytes as asked"));
        let (x, refused) = match size {
            2 => {
                let half = u16::try_from(bits).unwrap_or_else(|_| unreachable!("two bytes"));
                let x = write::from_half(half);
                (x, x.is_nan() && half != write::NAN_HALF)
            }
            4 => {
                let x = f64::from(f32::from_bits(low(bits)));
                (x, x.is_nan() || write::to_half(x).is_some())
            }
            _ => {
                let x = f64::from_bits(bits);
                (x, x.is_nan() || write::to_half(x).is_some() || write::to_single(x).is_some())
            }
        };
        if refused {
            self.reader.at = start;
            let why = if x.is_nan() { why::NAN_NOT_CANONICAL } else { why::FLOAT_NOT_SHORTEST };
            return self.fail(start, why);
        }
        Ok(Event::Float(x))
    }

    fn step(&mut self, event: Event<'a>) -> Result<Event<'a>, Error> {
        self.reader.at += 1;
        Ok(event)
    }
}

/// A misuse of the [`Encoder`]: the event's 0-based index and what.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StreamError {
    pub event: usize,
    pub what: StreamWhat,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StreamWhat {
    /// A key where a value is due, or outside a map.
    KeyNotDue,
    /// A value where a map's key is due.
    KeyDue,
    /// More items than the list's or map's length.
    TooMany,
    /// An end before the length's items, or after a key with no value.
    TooFew,
    /// An end of a list in a map, or of a map in a list, or of nothing.
    Unbalanced,
    /// A key not after the one before it in the rule's order.
    KeyOrder,
    KeyTwice,
    /// An event after the one value ended.
    AfterEnd,
    /// `finish` before the value ended.
    Unfinished,
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CBOR event {}: {:?}", self.event, self.what)
    }
}

impl core::error::Error for StreamError {}

/// Events to deterministic bytes appended to `out`; its stack in its own allocator.
pub struct Encoder<'o, A: Allocator, S: Allocator> {
    out: &'o mut Vec<u8, A>,
    stack: Vec<Frame, S>,
    count: usize,
    written: Written,
}

/// Whether the encoder's one value is whole.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Written {
    /// Not yet begun, or begun and not ended.
    Open,
    /// Ended: any further event is refused.
    Whole,
}

impl Written {
    /// `Whole` once the stack of open containers is empty after an event.
    fn after(stack_empty: bool) -> Written {
        if stack_empty { Written::Whole } else { Written::Open }
    }
}

impl<'o, A: Allocator, S: Allocator> Encoder<'o, A, S> {
    pub fn new_in(out: &'o mut Vec<u8, A>, alloc: S) -> Encoder<'o, A, S> {
        Encoder { out, stack: Vec::new_in(alloc), count: 0, written: Written::Open }
    }

    fn fail(&self, what: StreamWhat) -> Result<(), StreamError> {
        Err(StreamError { event: self.count, what })
    }

    pub fn event(&mut self, event: &Event<'_>) -> Result<(), StreamError> {
        let result = self.step(event);
        self.count += 1;
        result
    }

    fn step(&mut self, event: &Event<'_>) -> Result<(), StreamError> {
        if self.written == Written::Whole {
            return self.fail(StreamWhat::AfterEnd);
        }
        match *event {
            Event::EndList | Event::EndMap => {
                let map = matches!(event, Event::EndMap);
                let Some(top) = self.stack.last() else { return self.fail(StreamWhat::Unbalanced) };
                if top.due.is_map() != map {
                    return self.fail(StreamWhat::Unbalanced);
                }
                if top.left != 0 || top.due == Due::Value {
                    return self.fail(StreamWhat::TooFew);
                }
                self.stack.pop();
                self.written = Written::after(self.stack.is_empty());
                return Ok(());
            }
            Event::Key(key) => {
                let start = self.out.len();
                match self.stack.last() {
                    Some(top) if top.due == Due::Key => {}
                    _ => return self.fail(StreamWhat::KeyNotDue),
                }
                write::text(self.out, key);
                let Some(top) = self.stack.last_mut() else { unreachable!("seen above") };
                let end = self.out.len();
                let bytes = self.out.as_slice();
                if let Err(why) = key_order(&bytes[start..end], &bytes[top.prev.0..top.prev.1]) {
                    self.out.truncate(start);
                    let what = if why == why::KEY_TWICE { StreamWhat::KeyTwice } else { StreamWhat::KeyOrder };
                    return self.fail(what);
                }
                top.prev = (start, end);
                top.due = Due::Value;
                return Ok(());
            }
            _ => {}
        }
        match self.stack.last_mut() {
            Some(top) if top.due == Due::Key => return self.fail(StreamWhat::KeyDue),
            Some(top) if top.left == 0 => return self.fail(StreamWhat::TooMany),
            Some(top) => {
                top.left -= 1;
                top.due = top.due.after_item();
            }
            None => {}
        }
        match *event {
            Event::Null => write::null(self.out),
            Event::Bool(b) => write::boolean(self.out, b),
            Event::Int(n) => write::int(self.out, n),
            Event::Float(x) => write::float(self.out, x),
            Event::Bytes(b) => write::bytes(self.out, b),
            Event::Text(t) => write::text(self.out, t),
            Event::StartList { len } => {
                write::array(self.out, wide(len));
                self.stack.push(Frame { left: len, due: Due::opened(false), prev: (0, 0) });
            }
            Event::StartMap { len } => {
                write::map(self.out, wide(len));
                self.stack.push(Frame { left: len, due: Due::opened(true), prev: (0, 0) });
            }
            Event::Key(_) | Event::EndList | Event::EndMap => unreachable!("handled above"),
        }
        self.written = Written::after(self.stack.is_empty());
        Ok(())
    }

    /// The value is whole.
    pub fn finish(self) -> Result<(), StreamError> {
        match self.written {
            Written::Whole => Ok(()),
            Written::Open => Err(StreamError { event: self.count, what: StreamWhat::Unfinished }),
        }
    }
}
