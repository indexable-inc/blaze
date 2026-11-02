//! BLAKE3 as `const fn`s, so a record's [`SchemaHash`] is a constant made from its
//! description: the same bytes as [`ContentId::of`] of that description under
//! [`SCHEMA_DOMAIN`], which the tests check against the `blake3` crate.

use blaze_ids::ContentId;

use crate::SchemaHash;

/// The domain every schema hash is derived under.
pub const SCHEMA_DOMAIN: &str = "blaze v2 schema 1";

/// The schema hash of a record described by `description`: its name, version and fields, as
/// the writer of the record spells them. Any change to the layout must change the text.
pub const fn schema(description: &str) -> SchemaHash {
    SchemaHash(ContentId(derive_key(SCHEMA_DOMAIN, Input::Bytes(description.as_bytes()))))
}

/// One part of a schema made by [`schema_of`]: each is framed by its length, so two lists of
/// parts hash equal only when they are equal.
#[derive(Clone, Copy, Debug)]
pub enum Part<'a> {
    Str(&'a str),
    U64(u64),
    Schema(SchemaHash),
}

/// The schema hash of a record made of `parts`: what [`crate::record!`] makes from a struct's
/// name, version, and each field's name, offset and schema.
pub const fn schema_of(parts: &[Part<'_>]) -> SchemaHash {
    SchemaHash(ContentId(derive_key(SCHEMA_DOMAIN, Input::Parts(parts))))
}

const IV: [u32; 8] = [
    0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19,
];
const PERMUTATION: [usize; 16] = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];
const CHUNK_START: u32 = 1;
const CHUNK_END: u32 = 2;
const PARENT: u32 = 4;
const ROOT: u32 = 8;
const DERIVE_KEY_CONTEXT: u32 = 32;
const DERIVE_KEY_MATERIAL: u32 = 64;
const BLOCK: usize = 64;
const CHUNK: usize = 1024;

#[derive(Clone, Copy)]
enum Input<'a> {
    Bytes(&'a [u8]),
    /// Each part as its length (8 bytes, little-endian) and then its bytes.
    Parts(&'a [Part<'a>]),
}

const fn part_len(part: &Part<'_>) -> usize {
    8 + match part {
        Part::Str(it) => it.len(),
        Part::U64(_) => 8,
        Part::Schema(_) => 32,
    }
}

const fn part_byte(part: &Part<'_>, at: usize) -> u8 {
    if at < 8 {
        return ((part_len(part) - 8) as u64).to_le_bytes()[at];
    }
    let at = at - 8;
    match part {
        Part::Str(it) => it.as_bytes()[at],
        Part::U64(it) => it.to_le_bytes()[at],
        Part::Schema(it) => it.0.0[at],
    }
}

impl Input<'_> {
    const fn len(&self) -> usize {
        match self {
            Input::Bytes(it) => it.len(),
            Input::Parts(parts) => {
                let (mut len, mut i) = (0, 0);
                while i < parts.len() {
                    len += part_len(&parts[i]);
                    i += 1;
                }
                len
            }
        }
    }
}

/// Reads an [`Input`] front to back, which is all BLAKE3 needs.
struct Reader<'a> {
    input: Input<'a>,
    part: usize,
    at: usize,
}

impl Reader<'_> {
    const fn next(&mut self) -> u8 {
        match self.input {
            Input::Bytes(it) => {
                self.at += 1;
                it[self.at - 1]
            }
            Input::Parts(parts) => {
                while self.at == part_len(&parts[self.part]) {
                    self.part += 1;
                    self.at = 0;
                }
                self.at += 1;
                part_byte(&parts[self.part], self.at - 1)
            }
        }
    }
}

const fn g(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, x: u32, y: u32) {
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(x);
    state[d] = (state[d] ^ state[a]).rotate_right(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(12);
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(y);
    state[d] = (state[d] ^ state[a]).rotate_right(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(7);
}

const fn compress(cv: &[u32; 8], block: &[u32; 16], counter: u64, len: u32, flags: u32) -> [u32; 16] {
    let mut state = [
        cv[0],
        cv[1],
        cv[2],
        cv[3],
        cv[4],
        cv[5],
        cv[6],
        cv[7],
        IV[0],
        IV[1],
        IV[2],
        IV[3],
        counter as u32,
        (counter >> 32) as u32,
        len,
        flags,
    ];
    let mut m = *block;
    let mut round = 0;
    while round < 7 {
        g(&mut state, 0, 4, 8, 12, m[0], m[1]);
        g(&mut state, 1, 5, 9, 13, m[2], m[3]);
        g(&mut state, 2, 6, 10, 14, m[4], m[5]);
        g(&mut state, 3, 7, 11, 15, m[6], m[7]);
        g(&mut state, 0, 5, 10, 15, m[8], m[9]);
        g(&mut state, 1, 6, 11, 12, m[10], m[11]);
        g(&mut state, 2, 7, 8, 13, m[12], m[13]);
        g(&mut state, 3, 4, 9, 14, m[14], m[15]);
        let mut permuted = [0; 16];
        let mut i = 0;
        while i < 16 {
            permuted[i] = m[PERMUTATION[i]];
            i += 1;
        }
        m = permuted;
        round += 1;
    }
    let mut i = 0;
    while i < 8 {
        state[i] ^= state[i + 8];
        state[i + 8] ^= cv[i];
        i += 1;
    }
    state
}

const fn first_eight(words: &[u32; 16]) -> [u32; 8] {
    [words[0], words[1], words[2], words[3], words[4], words[5], words[6], words[7]]
}

/// A compression not yet run: the last one of a chunk or a parent, which is the root's when
/// nothing follows.
struct Output {
    cv: [u32; 8],
    block: [u32; 16],
    counter: u64,
    len: u32,
    flags: u32,
}

impl Output {
    const fn chaining_value(&self) -> [u32; 8] {
        first_eight(&compress(&self.cv, &self.block, self.counter, self.len, self.flags))
    }

    const fn root(&self) -> [u8; 32] {
        let words = compress(&self.cv, &self.block, 0, self.len, self.flags | ROOT);
        let mut out = [0; 32];
        let mut i = 0;
        while i < 32 {
            out[i] = words[i / 4].to_le_bytes()[i % 4];
            i += 1;
        }
        out
    }
}

/// The next `len` bytes, at most a block, as words, zero-padded.
const fn block_words(reader: &mut Reader<'_>, len: usize) -> [u32; 16] {
    let mut bytes = [0u8; BLOCK];
    let mut i = 0;
    while i < len {
        bytes[i] = reader.next();
        i += 1;
    }
    let mut words = [0; 16];
    let mut w = 0;
    while w < 16 {
        words[w] =
            u32::from_le_bytes([bytes[4 * w], bytes[4 * w + 1], bytes[4 * w + 2], bytes[4 * w + 3]]);
        w += 1;
    }
    words
}

/// Chunk `index`, `len` bytes of it, from `reader`, up to its last compression.
const fn chunk(reader: &mut Reader<'_>, index: u64, len: usize, key: &[u32; 8], flags: u32) -> Output {
    let blocks = if len == 0 { 1 } else { len.div_ceil(BLOCK) };
    let mut cv = *key;
    let mut b = 0;
    loop {
        let start = if b == 0 { CHUNK_START } else { 0 };
        let this = if b + 1 == blocks { len - b * BLOCK } else { BLOCK };
        let words = block_words(reader, this);
        if b + 1 == blocks {
            return Output { cv, block: words, counter: index, len: this as u32, flags: flags | start | CHUNK_END };
        }
        cv = first_eight(&compress(&cv, &words, index, BLOCK as u32, flags | start));
        b += 1;
    }
}

const fn parent(left: &[u32; 8], right: &[u32; 8], key: &[u32; 8], flags: u32) -> Output {
    let mut block = [0; 16];
    let mut i = 0;
    while i < 8 {
        block[i] = left[i];
        block[i + 8] = right[i];
        i += 1;
    }
    Output { cv: *key, block, counter: 0, len: BLOCK as u32, flags: flags | PARENT }
}

const fn hash(input: Input<'_>, key: &[u32; 8], flags: u32) -> [u8; 32] {
    let len = input.len();
    let chunks = if len == 0 { 1 } else { len.div_ceil(CHUNK) };
    let mut reader = Reader { input, part: 0, at: 0 };
    let mut stack = [[0u32; 8]; 54];
    let mut depth = 0;
    let mut index = 0;
    while index + 1 < chunks {
        let mut cv = chunk(&mut reader, index as u64, CHUNK, key, flags).chaining_value();
        let mut total = index as u64 + 1;
        while total & 1 == 0 {
            depth -= 1;
            cv = parent(&stack[depth], &cv, key, flags).chaining_value();
            total >>= 1;
        }
        stack[depth] = cv;
        depth += 1;
        index += 1;
    }
    let mut output = chunk(&mut reader, index as u64, len - index * CHUNK, key, flags);
    while depth > 0 {
        depth -= 1;
        output = parent(&stack[depth], &output.chaining_value(), key, flags);
    }
    output.root()
}

const fn words(bytes: &[u8; 32]) -> [u32; 8] {
    let mut words = [0; 8];
    let mut i = 0;
    while i < 8 {
        words[i] =
            u32::from_le_bytes([bytes[4 * i], bytes[4 * i + 1], bytes[4 * i + 2], bytes[4 * i + 3]]);
        i += 1;
    }
    words
}

/// `blake3::Hasher::new_derive_key(context)` fed `material`.
const fn derive_key(context: &str, material: Input<'_>) -> [u8; 32] {
    let key = hash(Input::Bytes(context.as_bytes()), &IV, DERIVE_KEY_CONTEXT);
    hash(material, &words(&key), DERIVE_KEY_MATERIAL)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(material: &[u8]) -> [u8; 32] {
        ContentId::of(SCHEMA_DOMAIN, material).0
    }

    fn bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn const_blake3_is_blake3() {
        let lens = [
            0, 1, 2, 3, 63, 64, 65, 127, 128, 129, 1023, 1024, 1025, 2047, 2048, 2049, 3072, 3073,
            4096, 4097, 5000, 8192, 8193, 16_385, 31_744, 65_537,
        ];
        for len in lens {
            let input = bytes(len);
            assert_eq!(derive_key(SCHEMA_DOMAIN, Input::Bytes(&input)), runtime(&input), "{len} bytes");
        }
        let long = "a context longer than one block of sixty-four bytes, so the key takes two";
        let input = bytes(3000);
        let expected = blaze_ids::b3::official::Hasher::new_derive_key(long).update(&input).finalize();
        assert_eq!(derive_key(long, Input::Bytes(&input)), expected);
    }

    #[test]
    fn a_schema_is_its_content_id() {
        const NODE: SchemaHash = schema("node 1: kind u16, size u32");
        assert_eq!(NODE.0, ContentId::of(SCHEMA_DOMAIN, b"node 1: kind u16, size u32"));
        assert_ne!(NODE, schema("node 2: kind u16, size u32"));
    }

    #[test]
    fn parts_are_framed() {
        let inner = schema("u32");
        let parts = [Part::Str("node"), Part::U64(1), Part::Str("kind"), Part::Schema(inner)];
        let mut framed = Vec::new();
        for part in parts {
            let bytes: Vec<u8> = match part {
                Part::Str(it) => it.as_bytes().to_vec(),
                Part::U64(it) => it.to_le_bytes().to_vec(),
                Part::Schema(it) => it.0.0.to_vec(),
            };
            framed.extend((bytes.len() as u64).to_le_bytes());
            framed.extend(bytes);
        }
        assert_eq!(schema_of(&parts).0.0, runtime(&framed));
        assert_ne!(
            schema_of(&[Part::Str("ab"), Part::Str("c")]),
            schema_of(&[Part::Str("a"), Part::Str("bc")])
        );
        assert_eq!(schema_of(&[]).0.0, runtime(&[]));
        let many: Vec<Part<'_>> = (0..200).map(Part::U64).collect();
        let mut framed = Vec::new();
        for n in 0..200u64 {
            framed.extend(8u64.to_le_bytes());
            framed.extend(n.to_le_bytes());
        }
        assert_eq!(schema_of(&many).0.0, runtime(&framed));
    }
}
