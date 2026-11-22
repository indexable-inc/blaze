//! Decode: bytes to the schema's values, every length capped before anything is sized by it,
//! every key held to the schema's one order, nothing skipped (rule 3).

use blaze_cbor::{Reader, why};
use blaze_id::{ContentId, ItemKey};
use blaze_own::{AString, AVec, Alloc};

use crate::{
    Answer, Call, Change, Checkout, Edit, EditWhy, Entered, Entry, EntryKind, Finding, FindingId, Limits, MOST_INT, Note, Path,
    Place, Refusal, Request, Response, Spot, Subject, Target, VERSION, What, WireCode, WireError, WorldId,
    enc::wide,
    types::{absolute, relative},
};

/// The reason this crate hands blaze-cbor for a text past its field's cap, told apart from
/// blaze-cbor's own reasons by content.
const LONG: &str = "a text past its field's limit (blaze-world-api)";
/// The same for a list.
const MANY: &str = "a list past its field's limit (blaze-world-api)";
/// The same for a map: no shape of the schema has more entries than this.
const FIELDS: &str = "a map of more entries than any shape of the world schema has";
const MOST_FIELDS: usize = 16;
/// The same for an integer past [`MOST_INT`].
const BIG: &str = "an integer past 2^53 - 1 (blaze-world-api)";

fn refused(at: usize, code: WireCode, field: &'static str, said: u64) -> WireError {
    WireError::new(at, code, field, said)
}

/// blaze-cbor's refusal as this schema's.
fn cbor(error: blaze_cbor::Error, field: &'static str) -> WireError {
    let code = match error.why {
        why::NOT_SHORTEST => WireCode::NotShortest,
        why::INDEFINITE => WireCode::Indefinite,
        why::BYTES_AFTER => WireCode::Trailing,
        why::FIXED_LONGER | why::FIXED_LENGTH => WireCode::Range,
        _ => WireCode::Cbor,
    };
    WireError { at: error.at, code, field, said: 0, cbor: error.why }
}

/// The entries of one map being read: how many are left and the last key read.
struct Fields {
    left: usize,
    last: Option<u64>,
}

pub(crate) struct Dec<'b, 'a> {
    r: Reader<'b>,
    limits: Limits,
    alloc: Alloc<'a>,
}

impl<'b, 'a> Dec<'b, 'a> {
    fn new(bytes: &'b [u8], limits: Limits, alloc: Alloc<'a>) -> Result<Dec<'b, 'a>, WireError> {
        if bytes.len() > limits.most_bytes {
            return Err(refused(0, WireCode::TooLarge, "message", wide(limits.most_bytes)));
        }
        Ok(Dec { r: Reader::new(bytes), limits, alloc })
    }

    fn open(&mut self, field: &'static str) -> Result<Fields, WireError> {
        let left = self.r.map(MOST_FIELDS, FIELDS).map_err(|e| cbor(e, field))?;
        Ok(Fields { left, last: None })
    }

    fn order(fields: &Fields, key: u64, at: usize, field: &'static str) -> Result<(), WireError> {
        match fields.last {
            Some(last) if key == last => Err(refused(at, WireCode::DuplicateKey, field, key)),
            Some(last) if key < last => Err(refused(at, WireCode::KeyOrder, field, key)),
            _ => Ok(()),
        }
    }

    /// The next key, which must be `want`: the schema's keys of a shape are 0, 1, 2, ... and
    /// all present, so a larger key means `want` is missing.
    fn key(&mut self, fields: &mut Fields, want: u64, field: &'static str) -> Result<(), WireError> {
        let at = self.r.at();
        if fields.left == 0 {
            return Err(refused(at, WireCode::MissingKey, field, want));
        }
        let key = self.r.uint().map_err(|e| cbor(e, field))?;
        Self::order(fields, key, at, field)?;
        if key > want {
            return Err(refused(at, WireCode::MissingKey, field, want));
        }
        if key < want {
            return Err(refused(at, WireCode::UnknownKey, field, key));
        }
        fields.left -= 1;
        fields.last = Some(key);
        Ok(())
    }

    /// The map ends: an entry left is a key the schema does not have.
    fn close(&mut self, fields: Fields, field: &'static str) -> Result<(), WireError> {
        if fields.left == 0 {
            return Ok(());
        }
        let at = self.r.at();
        let key = self.r.uint().map_err(|e| cbor(e, field))?;
        Self::order(&fields, key, at, field)?;
        Err(refused(at, WireCode::UnknownKey, field, key))
    }

    fn raw_uint(&mut self, field: &'static str) -> Result<u64, WireError> {
        let at = self.r.at();
        self.r.uint_most(MOST_INT, BIG).map_err(|e| match e.why {
            BIG => refused(at, WireCode::Range, field, MOST_INT),
            _ => cbor(e, field),
        })
    }

    fn uint(&mut self, fields: &mut Fields, key: u64, field: &'static str) -> Result<u64, WireError> {
        self.key(fields, key, field)?;
        self.raw_uint(field)
    }

    fn boolean(&mut self, fields: &mut Fields, key: u64, field: &'static str) -> Result<bool, WireError> {
        self.key(fields, key, field)?;
        self.r.boolean().map_err(|e| cbor(e, field))
    }

    fn id(&mut self, fields: &mut Fields, key: u64, field: &'static str) -> Result<[u8; 32], WireError> {
        self.key(fields, key, field)?;
        self.r.fixed::<32>().map_err(|e| cbor(e, field))
    }

    fn item(&mut self, fields: &mut Fields, key: u64, field: &'static str) -> Result<ItemKey, WireError> {
        Ok(ItemKey(ContentId(self.id(fields, key, field)?)))
    }

    /// A code of `from_code`'s set; another number is [`WireCode::UnknownVariant`].
    fn code<T>(
        &mut self,
        fields: &mut Fields,
        key: u64,
        field: &'static str,
        from_code: fn(u64) -> Option<T>,
    ) -> Result<T, WireError> {
        self.key(fields, key, field)?;
        let at = self.r.at();
        let code = self.raw_uint(field)?;
        from_code(code).ok_or_else(|| refused(at, WireCode::UnknownVariant, field, code))
    }

    fn raw_text(&mut self, most: usize, field: &'static str) -> Result<&'b str, WireError> {
        let at = self.r.at();
        self.r.text(most, LONG).map_err(|e| match e.why {
            LONG => refused(at, WireCode::TooLong, field, wide(most)),
            _ => cbor(e, field),
        })
    }

    /// A text of at most `most_text` bytes.
    fn text(&mut self, fields: &mut Fields, key: u64, field: &'static str) -> Result<AString<'a>, WireError> {
        self.key(fields, key, field)?;
        let text = self.raw_text(self.limits.most_text, field)?;
        Ok(AString::from_str_in(text, self.alloc))
    }

    /// A name, a code or a field: at most `most_path` bytes.
    fn name(&mut self, fields: &mut Fields, key: u64, field: &'static str) -> Result<AString<'a>, WireError> {
        self.key(fields, key, field)?;
        let text = self.raw_text(self.limits.most_path, field)?;
        Ok(AString::from_str_in(text, self.alloc))
    }

    /// A [`Path`], or with `root` also the empty text, which is `None`.
    fn path_or_root(&mut self, fields: &mut Fields, key: u64, field: &'static str, root: bool) -> Result<Option<Path<'a>>, WireError> {
        self.key(fields, key, field)?;
        let at = self.r.at();
        let text = self.raw_text(self.limits.most_path, field)?;
        if root && text.is_empty() {
            return Ok(None);
        }
        match relative(text) {
            true => Path::new_in(text, self.alloc).map(Some).map_err(|_| refused(at, WireCode::BadPath, field, 0)),
            false => Err(refused(at, WireCode::BadPath, field, 0)),
        }
    }

    fn path(&mut self, fields: &mut Fields, key: u64, field: &'static str) -> Result<Path<'a>, WireError> {
        match self.path_or_root(fields, key, field, false)? {
            Some(path) => Ok(path),
            None => unreachable!("without `root`, the empty text is refused as not relative"),
        }
    }

    fn checkout(&mut self, fields: &mut Fields, key: u64, field: &'static str) -> Result<Checkout<'a>, WireError> {
        self.key(fields, key, field)?;
        let at = self.r.at();
        let text = self.raw_text(self.limits.most_path, field)?;
        match absolute(text) {
            true => Checkout::new_in(text, self.alloc).map_err(|_| refused(at, WireCode::BadPath, field, 0)),
            false => Err(refused(at, WireCode::BadPath, field, 0)),
        }
    }

    /// A list of at most `most` items, each read by `each`; sized only after the cap and the
    /// bytes left have both held.
    fn list<T>(
        &mut self,
        fields: &mut Fields,
        key: u64,
        field: &'static str,
        most: usize,
        mut each: impl FnMut(&mut Dec<'b, 'a>) -> Result<T, WireError>,
    ) -> Result<AVec<'a, T>, WireError> {
        self.key(fields, key, field)?;
        let at = self.r.at();
        let len = self.r.array(most, MANY).map_err(|e| match e.why {
            MANY => refused(at, WireCode::TooMany, field, wide(most)),
            _ => cbor(e, field),
        })?;
        let mut items = AVec::with_capacity_in(len, self.alloc);
        for _ in 0..len {
            items.push(each(self)?);
        }
        Ok(items)
    }

    /// A message's map, its version (key 0) and its call or reply (key 1).
    fn message(&mut self, field: &'static str) -> Result<(Fields, u64, usize), WireError> {
        let mut fields = self.open(field)?;
        self.key(&mut fields, 0, "v")?;
        let at = self.r.at();
        let version = self.raw_uint("v")?;
        if version != VERSION {
            return Err(refused(at, WireCode::Version, "v", version));
        }
        self.key(&mut fields, 1, field)?;
        let at = self.r.at();
        let kind = self.raw_uint(field)?;
        Ok((fields, kind, at))
    }

    /// A variant's map and its tag (key 0).
    fn variant(&mut self, field: &'static str) -> Result<(Fields, u64, usize), WireError> {
        let mut fields = self.open(field)?;
        self.key(&mut fields, 0, field)?;
        let at = self.r.at();
        let tag = self.raw_uint(field)?;
        Ok((fields, tag, at))
    }

    fn end(&self) -> Result<(), WireError> {
        self.r.end().map_err(|e| cbor(e, "message"))
    }

    fn edit(&mut self) -> Result<Edit<'a>, WireError> {
        let (mut f, tag, at) = self.variant("edit")?;
        let edit = match tag {
            0 => Edit::WriteFile { path: self.path(&mut f, 1, "path")?, text: self.text(&mut f, 2, "text")? },
            1 => Edit::DeleteFile { path: self.path(&mut f, 1, "path")? },
            2 => Edit::RenameFile { from: self.path(&mut f, 1, "from")?, to: self.path(&mut f, 2, "to")? },
            3 => {
                let path = self.path(&mut f, 1, "path")?;
                let start = self.uint(&mut f, 2, "start")?;
                let end_at = self.r.at();
                let end = self.uint(&mut f, 3, "end")?;
                if start > end {
                    return Err(refused(end_at, WireCode::Range, "end", end));
                }
                Edit::ReplaceRange { path, start, end, text: self.text(&mut f, 4, "text")?, expect: self.text(&mut f, 5, "expect")? }
            }
            4 => Edit::ReplaceItem { item: self.item(&mut f, 1, "item")?, text: self.text(&mut f, 2, "text")? },
            _ => return Err(refused(at, WireCode::UnknownVariant, "edit", tag)),
        };
        self.close(f, "edit")?;
        Ok(edit)
    }

    fn what(&mut self) -> Result<What<'a>, WireError> {
        let (mut f, tag, at) = self.variant("what")?;
        let what = match tag {
            0 => What::File { path: self.path(&mut f, 1, "path")? },
            1 => What::Item { key: self.item(&mut f, 1, "key")? },
            2 => What::Dir { path: self.path_or_root(&mut f, 1, "path", true)? },
            _ => return Err(refused(at, WireCode::UnknownVariant, "what", tag)),
        };
        self.close(f, "what")?;
        Ok(what)
    }

    fn target(&mut self) -> Result<Target<'a>, WireError> {
        let (mut f, tag, at) = self.variant("select")?;
        let target = match tag {
            0 => Target::Path { path: self.path(&mut f, 1, "path")? },
            1 => Target::Item { key: self.item(&mut f, 1, "key")? },
            _ => return Err(refused(at, WireCode::UnknownVariant, "select", tag)),
        };
        self.close(f, "select")?;
        Ok(target)
    }

    fn subject(&mut self) -> Result<Subject, WireError> {
        let (mut f, tag, at) = self.variant("subject")?;
        let subject = match tag {
            0 => Subject::Finding { id: FindingId(self.id(&mut f, 1, "id")?) },
            1 => Subject::Item { key: self.item(&mut f, 1, "key")? },
            _ => return Err(refused(at, WireCode::UnknownVariant, "subject", tag)),
        };
        self.close(f, "subject")?;
        Ok(subject)
    }

    fn request(&mut self) -> Result<Request<'a>, WireError> {
        let (mut f, call, at) = self.message("call")?;
        let Some(call) = Call::from_code(call) else {
            return Err(refused(at, WireCode::UnknownVariant, "call", call));
        };
        let most_edits = self.limits.most_edits;
        let most_list = self.limits.most_list;
        let request = match call {
            Call::Open => Request::Open { checkout: self.checkout(&mut f, 2, "checkout")? },
            Call::Patch => Request::Patch {
                base: WorldId(self.id(&mut f, 2, "base")?),
                edits: self.list(&mut f, 3, "edits", most_edits, Dec::edit)?,
            },
            Call::Read => {
                let world = WorldId(self.id(&mut f, 2, "world")?);
                self.key(&mut f, 3, "what")?;
                Request::Read { world, what: self.what()? }
            }
            Call::Check => Request::Check {
                world: WorldId(self.id(&mut f, 2, "world")?),
                select: self.list(&mut f, 3, "select", most_list, Dec::target)?,
                lints: self.boolean(&mut f, 4, "lints")?,
            },
            Call::Diff => Request::Diff { from: WorldId(self.id(&mut f, 2, "from")?), to: WorldId(self.id(&mut f, 3, "to")?) },
            Call::Commit => Request::Commit {
                world: WorldId(self.id(&mut f, 2, "world")?),
                onto: self.checkout(&mut f, 3, "onto")?,
                expect: WorldId(self.id(&mut f, 4, "expect")?),
            },
            Call::Explain => {
                let world = WorldId(self.id(&mut f, 2, "world")?);
                self.key(&mut f, 3, "subject")?;
                Request::Explain { world, subject: self.subject()? }
            }
        };
        self.close(f, "request")?;
        Ok(request)
    }

    fn place(&mut self) -> Result<Place, WireError> {
        let mut f = self.open("place")?;
        let start = self.uint(&mut f, 0, "start")?;
        let end_at = self.r.at();
        let end = self.uint(&mut f, 1, "end")?;
        if start > end {
            return Err(refused(end_at, WireCode::Range, "end", end));
        }
        let place = Place { start, end, line: self.uint(&mut f, 2, "line")?, column: self.uint(&mut f, 3, "column")? };
        self.close(f, "place")?;
        Ok(place)
    }

    fn spot(&mut self) -> Result<Spot<'a>, WireError> {
        let mut f = self.open("spot")?;
        let item = self.item(&mut f, 0, "item")?;
        let path = self.path(&mut f, 1, "path")?;
        self.key(&mut f, 2, "place")?;
        let spot = Spot { item, path, place: self.place()? };
        self.close(f, "spot")?;
        Ok(spot)
    }

    fn note(&mut self) -> Result<Note<'a>, WireError> {
        let mut f = self.open("note")?;
        let most_list = self.limits.most_list;
        let note = Note { message: self.text(&mut f, 0, "message")?, at: self.list(&mut f, 1, "at", most_list, Dec::spot)? };
        self.close(f, "note")?;
        Ok(note)
    }

    fn finding(&mut self) -> Result<Finding<'a>, WireError> {
        let mut f = self.open("finding")?;
        let id = FindingId(self.id(&mut f, 0, "id")?);
        let code = self.name(&mut f, 1, "code")?;
        let message = self.text(&mut f, 2, "message")?;
        self.key(&mut f, 3, "at")?;
        let at = self.spot()?;
        let most_list = self.limits.most_list;
        let most_edits = self.limits.most_edits;
        let notes = self.list(&mut f, 4, "notes", most_list, Dec::note)?;
        let fix = self.list(&mut f, 5, "fix", most_edits, Dec::edit)?;
        self.close(f, "finding")?;
        Ok(Finding { id, code, message, at, notes, fix })
    }

    fn entered(&mut self) -> Result<Entered, WireError> {
        let mut f = self.open("entered")?;
        let entered = Entered {
            names: self.uint(&mut f, 0, "names")?,
            heads: self.uint(&mut f, 1, "heads")?,
            bodies: self.uint(&mut f, 2, "bodies")?,
            lints: self.uint(&mut f, 3, "lints")?,
        };
        self.close(f, "entered")?;
        Ok(entered)
    }

    fn entry(&mut self) -> Result<Entry<'a>, WireError> {
        let mut f = self.open("entry")?;
        let entry = Entry { name: self.name(&mut f, 0, "name")?, kind: self.code(&mut f, 1, "kind", EntryKind::from_code)? };
        self.close(f, "entry")?;
        Ok(entry)
    }

    fn change(&mut self) -> Result<Change<'a>, WireError> {
        let (mut f, tag, at) = self.variant("change")?;
        let change = match tag {
            0 => Change::FileAdded { path: self.path(&mut f, 1, "path")? },
            1 => Change::FileDeleted { path: self.path(&mut f, 1, "path")? },
            2 => Change::FileChanged { path: self.path(&mut f, 1, "path")? },
            3 => Change::ItemAdded { item: self.item(&mut f, 1, "item")?, path: self.path(&mut f, 2, "path")? },
            4 => Change::ItemRemoved { item: self.item(&mut f, 1, "item")?, path: self.path(&mut f, 2, "path")? },
            5 => Change::ItemChanged { item: self.item(&mut f, 1, "item")?, path: self.path(&mut f, 2, "path")? },
            _ => return Err(refused(at, WireCode::UnknownVariant, "change", tag)),
        };
        self.close(f, "change")?;
        Ok(change)
    }

    fn refusal(&mut self) -> Result<Refusal<'a>, WireError> {
        let (mut f, tag, at) = self.variant("refusal")?;
        let refusal = match tag {
            0 => Refusal::Edit { index: self.uint(&mut f, 1, "index")?, why: self.code(&mut f, 2, "why", EditWhy::from_code)? },
            1 => Refusal::NoSuchWorld { world: WorldId(self.id(&mut f, 1, "world")?) },
            2 => Refusal::NotACheckout { checkout: self.checkout(&mut f, 1, "checkout")? },
            3 => Refusal::NoSuchPath { path: self.path(&mut f, 1, "path")? },
            4 => Refusal::NoSuchItem { item: self.item(&mut f, 1, "item")? },
            5 => Refusal::NoSuchFinding { id: FindingId(self.id(&mut f, 1, "id")?) },
            6 => Refusal::Wire {
                at: self.uint(&mut f, 1, "at")?,
                what: self.code(&mut f, 2, "what", WireCode::from_code)?,
                field: self.name(&mut f, 3, "field")?,
                said: self.uint(&mut f, 4, "said")?,
            },
            7 => Refusal::Io { path: self.path(&mut f, 1, "path")?, message: self.text(&mut f, 2, "message")? },
            8 => Refusal::Unsupported { call: self.code(&mut f, 1, "call", Call::from_code)?, why: self.text(&mut f, 2, "why")? },
            _ => return Err(refused(at, WireCode::UnknownVariant, "refusal", tag)),
        };
        self.close(f, "refusal")?;
        Ok(refusal)
    }

    fn response(&mut self) -> Result<Response<'a>, WireError> {
        let (mut f, reply, at) = self.message("reply")?;
        let most_list = self.limits.most_list;
        let response = match reply {
            0 => Response::World { world: WorldId(self.id(&mut f, 2, "world")?) },
            1 => {
                self.key(&mut f, 2, "refusal")?;
                Response::Refused { refusal: self.refusal()? }
            }
            2 => Response::Text {
                path: self.path(&mut f, 2, "path")?,
                start: self.uint(&mut f, 3, "start")?,
                text: self.text(&mut f, 4, "text")?,
            },
            3 => Response::Listing { entries: self.list(&mut f, 2, "entries", most_list, Dec::entry)? },
            4 => {
                let world = WorldId(self.id(&mut f, 2, "world")?);
                let findings = self.list(&mut f, 3, "findings", most_list, Dec::finding)?;
                let certified = self.boolean(&mut f, 4, "certified")?;
                self.key(&mut f, 5, "entered")?;
                let entered = self.entered()?;
                let ms = self.uint(&mut f, 6, "ms")?;
                Response::Checked { answer: Answer { world, findings, certified, entered, ms } }
            }
            5 => Response::Changes { changes: self.list(&mut f, 2, "changes", most_list, Dec::change)? },
            6 => Response::Committed { files_written: self.uint(&mut f, 2, "files_written")? },
            7 => Response::Moved { now: WorldId(self.id(&mut f, 2, "now")?) },
            8 => Response::Explained {
                why: self.text(&mut f, 2, "why")?,
                reads: self.list(&mut f, 3, "reads", most_list, |d| {
                    let text = d.raw_text(d.limits.most_path, "reads")?;
                    Ok(AString::from_str_in(text, d.alloc))
                })?,
            },
            _ => return Err(refused(at, WireCode::UnknownVariant, "reply", reply)),
        };
        self.close(f, "response")?;
        Ok(response)
    }
}

impl<'a> Request<'a> {
    /// The request `bytes` hold, and nothing after it.
    pub fn decode(bytes: &[u8], limits: Limits, alloc: Alloc<'a>) -> Result<Request<'a>, WireError> {
        let mut dec = Dec::new(bytes, limits, alloc)?;
        let request = dec.request()?;
        dec.end()?;
        Ok(request)
    }
}

impl<'a> Response<'a> {
    /// The response `bytes` hold, and nothing after it.
    pub fn decode(bytes: &[u8], limits: Limits, alloc: Alloc<'a>) -> Result<Response<'a>, WireError> {
        let mut dec = Dec::new(bytes, limits, alloc)?;
        let response = dec.response()?;
        dec.end()?;
        Ok(response)
    }
}
