//! The CBOR data model without floats, tags or undefined, and its one encoding.

/// A CBOR value of the kinds the schema has. A [`Value::Map`] keeps its entries sorted
/// bytewise by the encodings of their keys, with no key twice: [`Value::map`] makes one so,
/// and [`Value::decode`] refuses one that is not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Uint(u64),
    /// The integer `-1 - n`.
    Nint(u64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Bool(bool),
    Null,
}

/// The first thing that makes bytes other than deterministic CBOR of one value, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offence {
    pub at: usize,
    pub what: &'static str,
}

impl std::fmt::Display for Offence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.what, self.at)
    }
}

impl std::error::Error for Offence {}

/// How deep arrays and maps may nest: a message is shallow, and a deep one would take the
/// decoder's stack.
const DEEPEST: usize = 64;

impl Value {
    /// A map of `entries`, sorted as the encoding has them, or the key that is there twice.
    pub fn map(entries: Vec<(Value, Value)>) -> Result<Value, Value> {
        let mut keyed: Vec<(Vec<u8>, (Value, Value))> =
            entries.into_iter().map(|it| (it.0.encode(), it)).collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some(twice) = keyed.windows(2).find(|it| it[0].0 == it[1].0) {
            return Err(twice[0].1.0.clone());
        }
        Ok(Value::Map(keyed.into_iter().map(|it| it.1).collect()))
    }

    /// The value's one encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut Vec<u8>) {
        match self {
            Value::Uint(n) => head(out, 0, *n),
            Value::Nint(n) => head(out, 1, *n),
            Value::Bytes(bytes) => {
                head(out, 2, bytes.len() as u64);
                out.extend_from_slice(bytes);
            }
            Value::Text(text) => {
                head(out, 3, text.len() as u64);
                out.extend_from_slice(text.as_bytes());
            }
            Value::Array(items) => {
                head(out, 4, items.len() as u64);
                items.iter().for_each(|it| it.write(out));
            }
            Value::Map(entries) => {
                head(out, 5, entries.len() as u64);
                for (key, value) in entries {
                    key.write(out);
                    value.write(out);
                }
            }
            Value::Bool(false) => out.push(0xf4),
            Value::Bool(true) => out.push(0xf5),
            Value::Null => out.push(0xf6),
        }
    }

    /// The one value `bytes` are the deterministic encoding of.
    pub fn decode(bytes: &[u8]) -> Result<Value, Offence> {
        let mut reader = Reader { bytes, at: 0 };
        let value = reader.value(0)?;
        if reader.at != bytes.len() {
            return Err(Offence { at: reader.at, what: "bytes after the value" });
        }
        Ok(value)
    }
}

/// The head of an item of major type `major` and argument `n`, in its shortest form.
fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    let major = major << 5;
    match n {
        0..24 => out.push(major | n as u8),
        24..0x100 => out.extend([major | 24, n as u8]),
        0x100..0x1_0000 => {
            out.push(major | 25);
            out.extend((n as u16).to_be_bytes());
        }
        0x1_0000..0x1_0000_0000 => {
            out.push(major | 26);
            out.extend((n as u32).to_be_bytes());
        }
        _ => {
            out.push(major | 27);
            out.extend(n.to_be_bytes());
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn offence(&self, at: usize, what: &'static str) -> Offence {
        Offence { at, what }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], Offence> {
        let end = self.at.checked_add(len).filter(|&it| it <= self.bytes.len());
        let end = end.ok_or_else(|| self.offence(self.at, "the value ends early"))?;
        let bytes: &'a [u8] = self.bytes;
        let taken = &bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    /// The major type and argument of the next head, refused unless in its shortest form.
    fn head(&mut self) -> Result<(u8, u8, u64), Offence> {
        let at = self.at;
        let first = self.take(1)?[0];
        let (major, info) = (first >> 5, first & 0x1f);
        let n = match info {
            0..24 => u64::from(info),
            24..28 if major == 7 => return Ok((major, info, 0)),
            24 => u64::from(self.take(1)?[0]),
            25 => u64::from(u16::from_be_bytes(self.take(2)?.try_into().unwrap_or_default())),
            26 => u64::from(u32::from_be_bytes(self.take(4)?.try_into().unwrap_or_default())),
            27 => u64::from_be_bytes(self.take(8)?.try_into().unwrap_or_default()),
            31 => return Err(self.offence(at, "an indefinite length")),
            _ => return Err(self.offence(at, "a reserved head")),
        };
        let shortest = match info {
            24 => n >= 24,
            25 => n > 0xff,
            26 => n > 0xffff,
            27 => n > 0xffff_ffff,
            _ => true,
        };
        if !shortest {
            return Err(self.offence(at, "a number not in its shortest form"));
        }
        Ok((major, info, n))
    }

    /// A length, which can't be more than what is left: one that says more can't be read.
    fn length(&self, n: u64, at: usize) -> Result<usize, Offence> {
        usize::try_from(n)
            .ok()
            .filter(|&it| it <= self.bytes.len() - self.at)
            .ok_or_else(|| self.offence(at, "a length longer than the message"))
    }

    fn value(&mut self, depth: usize) -> Result<Value, Offence> {
        let at = self.at;
        if depth > DEEPEST {
            return Err(self.offence(at, "arrays or maps nested too deep"));
        }
        let (major, info, n) = self.head()?;
        Ok(match major {
            0 => Value::Uint(n),
            1 => Value::Nint(n),
            2 => {
                let len = self.length(n, at)?;
                Value::Bytes(self.take(len)?.to_vec())
            }
            3 => {
                let len = self.length(n, at)?;
                let text = self.take(len)?;
                let text = std::str::from_utf8(text)
                    .map_err(|_| self.offence(at, "text that is not UTF-8"))?;
                Value::Text(text.to_owned())
            }
            4 => {
                // Each item is a byte at least.
                let len = self.length(n, at)?;
                let mut items = Vec::with_capacity(len);
                for _ in 0..len {
                    items.push(self.value(depth + 1)?);
                }
                Value::Array(items)
            }
            5 => {
                let len = self.length(n, at)?;
                let mut entries = Vec::with_capacity(len);
                let bytes = self.bytes;
                let mut last: Option<&[u8]> = None;
                for _ in 0..len {
                    let key_at = self.at;
                    let key = self.value(depth + 1)?;
                    let encoded = &bytes[key_at..self.at];
                    match last {
                        Some(last) if last == encoded => {
                            return Err(self.offence(key_at, "a map key that is there twice"));
                        }
                        Some(last) if last > encoded => {
                            return Err(self.offence(key_at, "map keys out of order"));
                        }
                        _ => {}
                    }
                    last = Some(encoded);
                    let value = self.value(depth + 1)?;
                    entries.push((key, value));
                }
                Value::Map(entries)
            }
            6 => return Err(self.offence(at, "a tag")),
            _ => match info {
                20 => Value::Bool(false),
                21 => Value::Bool(true),
                22 => Value::Null,
                25..28 => return Err(self.offence(at, "a float")),
                23 => return Err(self.offence(at, "undefined")),
                _ => return Err(self.offence(at, "a simple value")),
            },
        })
    }
}
