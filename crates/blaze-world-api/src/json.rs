//! The JSON rendering (rule 42: `blaze world '<one JSON value>'`): the same schema with the
//! keys named, as a [`Value`] that blaze-json writes. Key `v` is the version, `call` a
//! request's call and `reply` a response's kind; a variant's tag is `kind`; ids are 64
//! lowercase hex digits; every code is its name ([`crate::Call::name`] and the like); a
//! `checked` reply holds its answer's fields beside `v` and `reply`. Every field is present.

use blaze_data::{Int, Map, Value};
use blaze_own::{AString, AVec, Alloc};

use crate::{
    Answer, Change, Edit, Entered, Entry, Finding, Note, Place, Refusal, Request, Response, Spot, Subject, Target, VERSION, What,
};

type V<'a> = Value<Alloc<'a>>;

/// One JSON object being made.
struct Obj<'a> {
    map: Map<Alloc<'a>>,
    alloc: Alloc<'a>,
}

impl<'a> Obj<'a> {
    fn new(alloc: Alloc<'a>) -> Obj<'a> {
        Obj { map: Map::new_in(alloc), alloc }
    }

    fn put(mut self, key: &str, value: V<'a>) -> Obj<'a> {
        if self.map.insert(AString::from_str_in(key, self.alloc), value).is_some() {
            unreachable!("the schema names each key of a shape once (`{key}`)");
        }
        self
    }

    fn text(self, key: &str, text: &str) -> Obj<'a> {
        let value = Value::Text(AString::from_str_in(text, self.alloc));
        self.put(key, value)
    }

    fn uint(self, key: &str, n: u64) -> Obj<'a> {
        self.put(key, Value::Int(Int::from_u64(n)))
    }

    fn flag(self, key: &str, value: bool) -> Obj<'a> {
        self.put(key, Value::Bool(value))
    }

    fn id(self, key: &str, bytes: &[u8; 32]) -> Obj<'a> {
        let value = hex(bytes, self.alloc);
        self.put(key, value)
    }

    fn list<T>(self, key: &str, items: &[T], mut each: impl FnMut(&T, Alloc<'a>) -> V<'a>) -> Obj<'a> {
        let mut list = AVec::with_capacity_in(items.len(), self.alloc);
        for item in items {
            list.push(each(item, self.alloc));
        }
        self.put(key, Value::List(list))
    }

    fn done(self) -> V<'a> {
        Value::Map(self.map)
    }
}

/// 64 lowercase hex digits.
fn hex<'a>(bytes: &[u8; 32], alloc: Alloc<'a>) -> V<'a> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 64];
    for (i, byte) in bytes.iter().enumerate() {
        out[2 * i] = DIGITS[usize::from(byte >> 4)];
        out[2 * i + 1] = DIGITS[usize::from(byte & 0xf)];
    }
    match core::str::from_utf8(&out) {
        Ok(text) => Value::Text(AString::from_str_in(text, alloc)),
        Err(_) => unreachable!("hex digits are ASCII"),
    }
}

impl Request<'_> {
    pub fn to_value<'a>(&self, alloc: Alloc<'a>) -> V<'a> {
        let o = Obj::new(alloc).uint("v", VERSION).text("call", self.call().name());
        let o = match self {
            Request::Open { checkout } => o.text("checkout", checkout.as_str()),
            Request::Patch { base, edits } => o.id("base", &base.0).list("edits", edits, Edit::to_value),
            Request::Read { world, what } => o.id("world", &world.0).put("what", what.to_value(alloc)),
            Request::Check { world, select, lints } => {
                o.id("world", &world.0).list("select", select, Target::to_value).flag("lints", *lints)
            }
            Request::Diff { from, to } => o.id("from", &from.0).id("to", &to.0),
            Request::Commit { world, onto, expect } => o.id("world", &world.0).text("onto", onto.as_str()).id("expect", &expect.0),
            Request::Explain { world, subject } => o.id("world", &world.0).put("subject", subject.to_value(alloc)),
        };
        o.done()
    }
}

impl Edit<'_> {
    pub fn to_value<'a>(&self, alloc: Alloc<'a>) -> V<'a> {
        let o = Obj::new(alloc);
        let o = match self {
            Edit::WriteFile { path, text } => o.text("kind", "write_file").text("path", path.as_str()).text("text", text.as_str()),
            Edit::DeleteFile { path } => o.text("kind", "delete_file").text("path", path.as_str()),
            Edit::RenameFile { from, to } => o.text("kind", "rename_file").text("from", from.as_str()).text("to", to.as_str()),
            Edit::ReplaceRange { path, start, end, text, expect } => o
                .text("kind", "replace_range")
                .text("path", path.as_str())
                .uint("start", *start)
                .uint("end", *end)
                .text("text", text.as_str())
                .text("expect", expect.as_str()),
            Edit::ReplaceItem { item, text } => o.text("kind", "replace_item").id("item", &item.0.0).text("text", text.as_str()),
        };
        o.done()
    }
}

impl What<'_> {
    pub fn to_value<'a>(&self, alloc: Alloc<'a>) -> V<'a> {
        let o = Obj::new(alloc);
        let o = match self {
            What::File { path } => o.text("kind", "file").text("path", path.as_str()),
            What::Item { key } => o.text("kind", "item").id("key", &key.0.0),
            What::Dir { path } => o.text("kind", "dir").text("path", path.as_ref().map_or("", |path| path.as_str())),
        };
        o.done()
    }
}

impl Target<'_> {
    pub fn to_value<'a>(&self, alloc: Alloc<'a>) -> V<'a> {
        let o = Obj::new(alloc);
        let o = match self {
            Target::Path { path } => o.text("kind", "path").text("path", path.as_str()),
            Target::Item { key } => o.text("kind", "item").id("key", &key.0.0),
        };
        o.done()
    }
}

impl Subject {
    pub fn to_value<'a>(&self, alloc: Alloc<'a>) -> V<'a> {
        let o = Obj::new(alloc);
        let o = match self {
            Subject::Finding { id } => o.text("kind", "finding").id("id", &id.0),
            Subject::Item { key } => o.text("kind", "item").id("key", &key.0.0),
        };
        o.done()
    }
}

/// The name of a reply: key `reply` of a response.
fn reply_name(response: &Response<'_>) -> &'static str {
    match response {
        Response::World { .. } => "world",
        Response::Refused { .. } => "refused",
        Response::Text { .. } => "text",
        Response::Listing { .. } => "listing",
        Response::Checked { .. } => "checked",
        Response::Changes { .. } => "changes",
        Response::Committed { .. } => "committed",
        Response::Moved { .. } => "moved",
        Response::Explained { .. } => "explained",
    }
}

impl Response<'_> {
    pub fn to_value<'a>(&self, alloc: Alloc<'a>) -> V<'a> {
        let o = Obj::new(alloc).uint("v", VERSION).text("reply", reply_name(self));
        let o = match self {
            Response::World { world } => o.id("world", &world.0),
            Response::Refused { refusal } => o.put("refusal", refusal.to_value(alloc)),
            Response::Text { path, start, text } => o.text("path", path.as_str()).uint("start", *start).text("text", text.as_str()),
            Response::Listing { entries } => o.list("entries", entries, Entry::to_value),
            Response::Checked { answer } => answer.fields(o),
            Response::Changes { changes } => o.list("changes", changes, Change::to_value),
            Response::Committed { files_written } => o.uint("files_written", *files_written),
            Response::Moved { now } => o.id("now", &now.0),
            Response::Explained { why, reads } => {
                o.text("why", why.as_str()).list("reads", reads, |read, alloc| Value::Text(AString::from_str_in(read.as_str(), alloc)))
            }
        };
        o.done()
    }
}

impl Answer<'_> {
    fn fields<'a>(&self, o: Obj<'a>) -> Obj<'a> {
        let alloc = o.alloc;
        o.id("world", &self.world.0)
            .list("findings", &self.findings, Finding::to_value)
            .flag("certified", self.certified)
            .put("entered", self.entered.to_value(alloc))
            .uint("ms", self.ms)
    }
}

impl Entered {
    pub fn to_value<'a>(&self, alloc: Alloc<'a>) -> V<'a> {
        Obj::new(alloc)
            .uint("names", self.names)
            .uint("heads", self.heads)
            .uint("bodies", self.bodies)
            .uint("lints", self.lints)
            .done()
    }
}

impl Entry<'_> {
    pub fn to_value<'a>(&self, alloc: Alloc<'a>) -> V<'a> {
        Obj::new(alloc).text("name", self.name.as_str()).text("kind", self.kind.name()).done()
    }
}

impl Finding<'_> {
    pub fn to_value<'a>(&self, alloc: Alloc<'a>) -> V<'a> {
        Obj::new(alloc)
            .id("id", &self.id.0)
            .text("code", self.code.as_str())
            .text("message", self.message.as_str())
            .put("at", self.at.to_value(alloc))
            .list("notes", &self.notes, Note::to_value)
            .list("fix", &self.fix, Edit::to_value)
            .done()
    }
}

impl Spot<'_> {
    pub fn to_value<'a>(&self, alloc: Alloc<'a>) -> V<'a> {
        Obj::new(alloc).id("item", &self.item.0.0).text("path", self.path.as_str()).put("place", self.place.to_value(alloc)).done()
    }
}

impl Place {
    pub fn to_value<'a>(&self, alloc: Alloc<'a>) -> V<'a> {
        Obj::new(alloc)
            .uint("start", self.start)
            .uint("end", self.end)
            .uint("line", self.line)
            .uint("column", self.column)
            .done()
    }
}

impl Note<'_> {
    pub fn to_value<'a>(&self, alloc: Alloc<'a>) -> V<'a> {
        Obj::new(alloc).text("message", self.message.as_str()).list("at", &self.at, Spot::to_value).done()
    }
}

impl Change<'_> {
    pub fn to_value<'a>(&self, alloc: Alloc<'a>) -> V<'a> {
        let o = Obj::new(alloc);
        let o = match self {
            Change::FileAdded { path } => o.text("kind", "file_added").text("path", path.as_str()),
            Change::FileDeleted { path } => o.text("kind", "file_deleted").text("path", path.as_str()),
            Change::FileChanged { path } => o.text("kind", "file_changed").text("path", path.as_str()),
            Change::ItemAdded { item, path } => o.text("kind", "item_added").id("item", &item.0.0).text("path", path.as_str()),
            Change::ItemRemoved { item, path } => o.text("kind", "item_removed").id("item", &item.0.0).text("path", path.as_str()),
            Change::ItemChanged { item, path } => o.text("kind", "item_changed").id("item", &item.0.0).text("path", path.as_str()),
        };
        o.done()
    }
}

impl Refusal<'_> {
    pub fn to_value<'a>(&self, alloc: Alloc<'a>) -> V<'a> {
        let o = Obj::new(alloc);
        let o = match self {
            Refusal::Edit { index, why } => o.text("kind", "edit").uint("index", *index).text("why", why.name()),
            Refusal::NoSuchWorld { world } => o.text("kind", "no_such_world").id("world", &world.0),
            Refusal::NotACheckout { checkout } => o.text("kind", "not_a_checkout").text("checkout", checkout.as_str()),
            Refusal::NoSuchPath { path } => o.text("kind", "no_such_path").text("path", path.as_str()),
            Refusal::NoSuchItem { item } => o.text("kind", "no_such_item").id("item", &item.0.0),
            Refusal::NoSuchFinding { id } => o.text("kind", "no_such_finding").id("id", &id.0),
            Refusal::Wire { at, what, field, said } => {
                o.text("kind", "wire").uint("at", *at).text("what", what.name()).text("field", field.as_str()).uint("said", *said)
            }
            Refusal::Io { path, message } => o.text("kind", "io").text("path", path.as_str()).text("message", message.as_str()),
            Refusal::Unsupported { call, why } => o.text("kind", "unsupported").text("call", call.name()).text("why", why.as_str()),
        };
        o.done()
    }
}
